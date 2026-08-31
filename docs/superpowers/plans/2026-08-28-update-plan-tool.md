# Update Plan Tool Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add one durable, session-scoped `update_plan` tool with authoritative RPC/TUI progress rendering.

**Architecture:** Typed plan values live in the session domain, while a bounded request channel lets the Rig tool ask the session actor to atomically replace the plan. The actor owns projection, checkpointing, warnings, and broadcasts; snapshots and `PlanChanged` events carry the state to clients, and each new run receives the current plan through `RunContext`.

**Tech Stack:** Rust 2024, Tokio channels, Rig portable tools, Serde, Schemars, Garde, rusqlite, Cap'n Proto 0.27, Crossterm, Ratatui 0.30.

**Spec:** `docs/superpowers/specs/2026-08-28-update-plan-tool-design.md`

## Global Constraints

- Expose exactly one model-facing tool named `update_plan`; add no aliases or read tool.
- Statuses are exactly `pending`, `in_progress`, `completed`, `blocked`, and `cancelled`.
- A plan contains at most 32 ordered items and at most one `in_progress` item.
- Step text is already trimmed, contains no controls, and contains 1-256 Unicode scalar values.
- Plan state is session-owned durable application state, not conversation history or a workspace file.
- `SessionSnapshot` remains the sole authoritative client projection.
- Preserve all existing Cap'n Proto ordinals and check generated Rust bindings into Git.
- Use TDD and finish each task with the focused tests and conventional commit shown below.
- Preserve unrelated worktree changes and stage only the paths named by the current task.

## File Structure

- `src/session/types.rs` — canonical `PlanStatus` and `PlanItem`, plus plan fields on session records, snapshots, and events.
- `src/tools/plan.rs` — strict `UpdatePlanArgs`, plan-request channel, canonical output, and model-visible errors.
- `src/runtime/rig/plan_tool.rs` — thin `PortableTool` adapter named `update_plan`.
- `src/session/store.rs` — schema-v2 migration and transactional plan persistence.
- `src/session/actor.rs` — authoritative plan request handling, checkpointing, sequencing, and broadcasts.
- `src/session/projection.rs` — server-side `PlanChanged` reduction.
- `src/harness/types.rs` — current-plan snapshot in `RunContext`.
- `src/runtime/rig/codex.rs` — tool registration and generated per-run plan context.
- `schema/moh.capnp` and `src/rpc/moh_capnp.rs` — source protocol and checked-in generated bindings.
- `src/rpc/convert.rs` — plan snapshot/event conversions.
- `src/client/app.rs` — client projection reduction and Ctrl+T behavior.
- `src/client/ui/mod.rs` and `src/client/ui/view.rs` — mutually exclusive Help/Plan popup state and rendering.
- `README.md` — user-facing tool, durability, status, and shortcut documentation.
- `tests/plan_tool.rs` — domain/channel behavior.
- Existing focused integration tests — storage, actor, Rig, schema, RPC, and client behavior.

---

### Task 1: Define the plan domain and tool request port

**Files:**
- Create: `src/tools/plan.rs`
- Create: `tests/plan_tool.rs`
- Modify: `src/tools/mod.rs`
- Modify: `src/session/types.rs`
- Modify: `src/session/mod.rs`
- Modify: `tests/tool_schema_semantics.rs`

**Interfaces:**
- Produces: `PlanStatus::{Pending, InProgress, Completed, Blocked, Cancelled}`.
- Produces: `PlanStatus::as_str(self) -> &'static str` and `impl FromStr<Err = PlanStatusParseError>`.
- Produces: `PlanItem::parse(step: impl Into<String>, status: PlanStatus) -> Result<PlanItem, PlanItemError>`.
- Produces: `UpdatePlanArgs { explanation: Option<String>, plan: Vec<PlanItem> }`.
- Produces: `PlanUpdateClient::replace(args: UpdatePlanArgs) -> Result<PlanUpdateOutcome, PlanToolError>`.
- Produces: `plan_update_channel() -> (PlanUpdateClient, PlanUpdateReceiver)` and `PlanUpdateReceiver::recv()`.
- Produces: `PlanUpdateRequest::succeed(PlanUpdateOutcome)` and `PlanUpdateRequest::fail(PlanToolError)` as the only response settlement methods.

- [ ] **Step 1: Write failing domain and channel tests**

Create `tests/plan_tool.rs` with table-driven tests that parse all five statuses, reject empty/whitespace/control/257-scalar steps, reject 33 items and two active items, accept duplicate text, accept an empty clear, and prove a closed request channel returns `[E_RUNTIME]`.

Use this channel round-trip shape:

```rust
#[tokio::test]
async fn update_waits_for_the_authoritative_receiver() {
    let (client, mut receiver) = plan_update_channel();
    let call = tokio::spawn(async move {
        client
            .replace(UpdatePlanArgs {
                explanation: Some("Start verification".into()),
                plan: vec![PlanItem::parse("Run tests", PlanStatus::InProgress).unwrap()],
            })
            .await
    });
    let request = receiver.recv().await.unwrap();
    request.succeed(PlanUpdateOutcome::durable(
        request.plan().to_vec(),
        request.explanation().map(str::to_owned),
    ));
    assert_eq!(call.await.unwrap().unwrap().plan()[0].step(), "Run tests");
}
```

Extend `tests/tool_schema_semantics.rs` to assert `plan` is required, `explanation` is optional, `plan.maxItems == 32`, statuses use canonical snake-case names, nested objects are strict, and `explanation`/`step` descriptions match their Rust field documentation.

- [ ] **Step 2: Run the focused tests and confirm the missing API failure**

Run:

```bash
cargo test --test plan_tool --test tool_schema_semantics
```

Expected: compilation fails because the plan types and channel do not exist.

- [ ] **Step 3: Implement the typed contract and bounded channel**

Add domain types with explicit accessors and parsers:

```rust
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus { Pending, InProgress, Completed, Blocked, Cancelled }

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanItem {
    step: String,
    status: PlanStatus,
}
```

Implement `PlanItem::parse` using `chars().count()`, `trim() == step`, and `char::is_control`. Implement `UpdatePlanArgs::validate()` so every caller enforces the same full-list constraints before sending. Use a Tokio `mpsc` channel with capacity 8 and a per-request `oneshot::Sender<Result<PlanUpdateOutcome, PlanToolError>>`. Keep request fields private; expose `plan()`, `explanation()`, and consuming `succeed(outcome)`/`fail(error)` methods so only the actor can settle a request.

Define stable model-visible failures explicitly:

```rust
#[derive(Debug, Error)]
pub enum PlanToolError {
    #[error("[E_INVALID_ARGUMENT] {0}")]
    InvalidArgument(&'static str),
    #[error("[E_RUNTIME] plan tool state is unavailable")]
    Runtime,
}
```

Store the optional explanation in `PlanUpdateOutcome` so the accepted actor response returns it to the model without adding it to durable session state. Return canonical text from `PlanUpdateOutcome::render()` in this form:

```text
Plan updated: 1 completed, 1 in progress, 2 pending, 0 blocked, 0 cancelled.
Explanation: Start verification
1. [completed] Inspect the code
2. [in_progress] Run tests
```

When durability is pending, append `Plan persistence is pending; the live session retains this update.`

- [ ] **Step 4: Run focused tests**

Run:

```bash
cargo test --test plan_tool --test tool_schema_semantics
cargo fmt --all -- --check
git diff --check
```

Expected: all focused tests pass and formatting/diff checks are clean.

- [ ] **Step 5: Commit the contract**

```bash
git add src/session/types.rs src/session/mod.rs src/tools/mod.rs src/tools/plan.rs tests/plan_tool.rs tests/tool_schema_semantics.rs
git diff --cached --check
git commit -m "feat(plan): define update plan contract"
```

---

### Task 2: Persist ordered plans with session schema version 2

**Files:**
- Modify: `src/session/types.rs`
- Modify: `src/session/store.rs`
- Modify: `tests/session_store.rs`
- Modify: `src/session/projection.rs`
- Modify: `tests/session_actor.rs`
- Modify: `tests/session_projection.rs`
- Modify: `tests/support/mod.rs`

**Interfaces:**
- Consumes: `PlanItem` and `PlanStatus` from Task 1.
- Produces: `SessionRecord.plan: Vec<PlanItem>`.
- Produces: schema version 2 with `plan_items(session_id, position, step, status)`.
- Preserves: `SessionRepository::checkpoint(record)` as the one atomic full-record write.

- [ ] **Step 1: Add failing fresh-store, migration, and round-trip tests**

In `tests/session_store.rs`, add tests that:

```rust
let record = repository.load(session_id).await.unwrap();
assert!(record.plan.is_empty());

let mut changed = record.clone();
changed.plan = vec![
    PlanItem::parse("Inspect", PlanStatus::Completed).unwrap(),
    PlanItem::parse("Verify", PlanStatus::InProgress).unwrap(),
];
repository.checkpoint(changed.clone()).await.unwrap();
assert_eq!(repository.load(session_id).await.unwrap().plan, changed.plan);
```

Create a literal version-1 SQLite database using the existing v1 schema, open it through `SessionStore::open_at`, and assert its messages survive while its plan starts empty and `PRAGMA user_version` becomes 2. Add replacement, empty-clear, cascade-delete, non-contiguous-position, unknown-status, invalid-step, and two-active-row cases.

- [ ] **Step 2: Run storage tests and confirm schema/field failures**

Run:

```bash
cargo test --test session_store
```

Expected: compilation or assertions fail because `SessionRecord.plan` and schema v2 do not exist.

- [ ] **Step 3: Implement schema-v2 migration and record persistence**

Add `plan: Vec<PlanItem>` to `SessionRecord` and initialize it to `Vec::new()` in every existing fixture. Define schema v2 with:

```sql
CREATE TABLE plan_items (
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    position INTEGER NOT NULL CHECK (position >= 0),
    step TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending', 'in_progress', 'completed', 'blocked', 'cancelled')),
    PRIMARY KEY (session_id, position)
);
PRAGMA user_version = 2;
```

Migrate v1 inside one immediate transaction by creating `plan_items` and setting `user_version = 2`. Reject versions above 2. Load rows ordered by `position`, require positions `0..N` without gaps, parse every item through the Task 1 domain constructor, and validate the complete plan.

In `checkpoint_sync`, delete and reinsert the session's plan rows inside the existing metadata/history transaction. Do not alter `update_metadata`; metadata-only writes must leave plan rows untouched.

- [ ] **Step 4: Run focused storage and compile-surface tests**

Run:

```bash
cargo test --test session_store
cargo test --test session_projection --test session_manager --test session_actor
cargo fmt --all -- --check
git diff --check
```

Expected: storage and affected session fixtures pass.

- [ ] **Step 5: Commit durable storage**

```bash
git add src/session/types.rs src/session/store.rs src/session/projection.rs tests/session_store.rs tests/session_actor.rs tests/session_projection.rs tests/support/mod.rs
git diff --cached --check
git commit -m "feat(session): persist execution plans"
```

---

### Task 3: Make the session actor authoritative for plan updates

**Files:**
- Modify: `src/session/types.rs`
- Modify: `src/session/projection.rs`
- Modify: `src/session/runtime.rs`
- Modify: `src/session/actor.rs`
- Modify: `src/session/manager.rs`
- Modify: `tests/session_projection.rs`
- Modify: `tests/session_actor.rs`
- Modify: `tests/session_manager.rs`
- Modify: `src/client/app_tests.rs`
- Modify: `src/client/session.rs`
- Modify: `src/client/ui/view.rs`
- Modify: `src/rpc/client.rs`
- Modify: `src/rpc/convert.rs`
- Modify: `tests/rpc_schema.rs`
- Modify: `tests/rpc_transport.rs`
- Modify: `tests/support/mod.rs`

**Interfaces:**
- Consumes: `PlanUpdateReceiver` and `PlanUpdateOutcome` from Task 1.
- Produces: `SessionEvent::PlanChanged(Vec<PlanItem>)`.
- Produces: `SessionSnapshot.plan: Vec<PlanItem>`.
- Produces: `SessionEngineBundle.plans: PlanUpdateReceiver` while its engine owns the paired `PlanUpdateClient`.

- [ ] **Step 1: Write failing projection and active-run actor tests**

Add a projection test proving this event is legal both idle and busy and replaces rather than appends:

```rust
let plan = vec![PlanItem::parse("Verify", PlanStatus::InProgress).unwrap()];
projection.apply(SessionEvent::PlanChanged(plan.clone())).unwrap();
assert_eq!(projection.snapshot(vec![]).plan, plan);
```

Add an actor test with a controlled engine that invokes its `PlanUpdateClient` while the actor is polling an active run. Assert the request resolves without timeout, the observer receives `PlanChanged` before the later `ToolFinished`/completion events, reattachment contains the plan, and completion/failure/cancellation do not clear it.

Extend the existing failing repository double so a plan checkpoint failure leaves live plan state present, returns an outcome with `durable == false`, broadcasts `PersistenceWarning`, and succeeds after `flush()` retries the dirty record.

- [ ] **Step 2: Run focused tests and verify failure**

Run:

```bash
cargo test --test session_projection --test session_actor --test session_manager
```

Expected: compilation fails because snapshots, events, and engine bundles lack plan state.

- [ ] **Step 3: Add plan state to projection and engine bundles**

Initialize `SessionProjection.plan` from `SessionRecord.plan`; copy it into every snapshot; reduce `PlanChanged` by full replacement; and exempt `PlanChanged` from run-ID validation.

Extend `SessionEngineBundle` with `plans: PlanUpdateReceiver`. In every fake and production factory, create `plan_update_channel()`, retain the client in the engine, and return the receiver in the bundle.

- [ ] **Step 4: Handle plan requests in the actor loop**

Add `ActorInput::PlanUpdate(PlanUpdateRequest)` and select `bundle.plans.recv()` in both the running and idle loop branches. Implement one handler with this order:

```rust
let previous = std::mem::replace(&mut self.record.plan, request.plan().to_vec());
let explanation = request.explanation().map(str::to_owned);
let event = match self.project(SessionEvent::PlanChanged(self.record.plan.clone())) {
    Ok(event) => event,
    Err(_) => {
        self.record.plan = previous;
        request.fail(PlanToolError::Runtime);
        return;
    }
};
let warning = self.persist_checkpoint().await;
let durable = !matches!(warning, Some(Some(_)));
if let Some(event) = event { self.broadcast(event); }
self.broadcast_persistence_transition(warning.clone());
request.succeed(PlanUpdateOutcome::new(
    self.record.plan.clone(),
    explanation,
    durable,
));
```

Preserve the existing sequence-exhaustion behavior. If projection fails, restore the previous plan and call `request.fail(PlanToolError::Runtime)` before returning to the actor loop. A failed checkpoint does not roll back live state; the existing dirty record owns the retry.

- [ ] **Step 5: Run focused actor tests**

Run:

```bash
cargo test --test session_projection --test session_actor --test session_manager
cargo fmt --all -- --check
git diff --check
```

Expected: all focused session tests pass without a current-thread runtime stall.

- [ ] **Step 6: Commit actor ownership**

```bash
git add src/session src/tools/plan.rs src/client/app_tests.rs src/client/session.rs src/client/ui/view.rs src/rpc/client.rs src/rpc/convert.rs tests/rpc_schema.rs tests/rpc_transport.rs tests/support/mod.rs tests/session_projection.rs tests/session_actor.rs tests/session_manager.rs
git diff --cached --check
git commit -m "feat(session): own plan updates in actor"
```

---

### Task 4: Register `update_plan` and inject current plan context

**Files:**
- Create: `src/runtime/rig/plan_tool.rs`
- Modify: `src/runtime/rig/mod.rs`
- Modify: `src/runtime/rig/codex.rs`
- Modify: `src/harness/types.rs`
- Modify: `tests/rig_runtime.rs`
- Modify: `tests/codex_live.rs`
- Modify: `src/session/actor.rs`
- Modify: `tests/codex_live.rs`
- Modify: `tests/harness.rs`
- Modify: `tests/rig_runtime.rs`

**Interfaces:**
- Consumes: `PlanUpdateClient::replace(UpdatePlanArgs)`.
- Produces: `RigUpdatePlanTool` implementing `PortableTool` with `NAME = "update_plan"`.
- Produces: `RunContext.plan: Vec<PlanItem>`.
- Produces: `format_plan_context(plan: &[PlanItem]) -> Option<String>` for the per-run preamble.

- [ ] **Step 1: Write failing adapter, payload, and prompt-context tests**

In `tests/rig_runtime.rs`, extend the intercepted Responses request assertion from seven to eight tools and assert the strict generated schema for `update_plan`. Add a two-response mock: the first returns an `update_plan` function call, the test actor-side receiver accepts it, and the second returns final assistant text. Assert `ToolStarted`, `PlanChanged` through the actor integration test, `ToolFinished`, and completion order.

Add a request-capture test with a non-empty `RunContext.plan` and assert the model instructions contain exactly:

```text
# Current execution plan
1. [completed] Inspect code
2. [in_progress] Run tests
Use update_plan to replace this plan whenever its steps or statuses change.
```

Assert an empty plan emits no heading. Update `tests/codex_live.rs` only for the eighth declared tool and empty plan fixture.

- [ ] **Step 2: Run focused runtime tests and verify failure**

Run:

```bash
cargo test --test rig_runtime --test codex_live
```

Expected: assertions fail because `update_plan` is absent and `RunContext` has no plan.

- [ ] **Step 3: Implement the Rig adapter and registration**

Implement the adapter in the same shape as `RigWriteTool`:

```rust
impl PortableTool for RigUpdatePlanTool {
    const NAME: &'static str = "update_plan";
    type Error = RigUpdatePlanError;
    type Args = UpdatePlanArgs;
    type Output = ToolOutput;
    fn description(&self) -> String { UpdatePlanArgs::description().to_owned() }
    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schema_for!(UpdatePlanArgs)).expect("derived tool schema must serialize")
    }
}
```

Map domain errors to recoverable tool execution errors and closed-channel failures to the existing tool-infrastructure failure convention. Register the adapter alongside the current seven tools and add its runtime error code to the hook classification.

- [ ] **Step 4: Add current plan to run context and preamble**

Set `RunContext.plan` from the actor record at submission. Build the generated plan section from canonical statuses and sanitized domain text, append it after repository instructions and skills but before date/CWD context, and omit it when empty. Keep the static `system_prompt.md` unchanged except for one concise rule describing when to use `update_plan`.

- [ ] **Step 5: Run focused runtime tests**

Run:

```bash
cargo test --test rig_runtime --test codex_live --test tool_schema_semantics
cargo fmt --all -- --check
git diff --check
```

Expected: the eight-tool payload, tool loop, empty/non-empty context, and live-test compile surface pass.

- [ ] **Step 6: Commit runtime exposure**

```bash
git add src/harness/types.rs src/runtime/rig src/session/actor.rs tests/rig_runtime.rs tests/codex_live.rs tests/tool_schema_semantics.rs
git diff --cached --check
git commit -m "feat(runtime): expose update plan tool"
```

---

### Task 5: Transport plan snapshots and events over RPC

**Files:**
- Modify: `schema/moh.capnp`
- Modify: `src/rpc/moh_capnp.rs` using `scripts/generate-rpc.sh`
- Modify: `src/rpc/convert.rs`
- Modify: `tests/rpc_schema.rs`
- Modify: `tests/rpc_transport.rs`
- Modify: `tests/client_server.rs`

**Interfaces:**
- Consumes: `PlanStatus`, `PlanItem`, `SessionSnapshot.plan`, and `SessionEvent::PlanChanged`.
- Produces: Cap'n Proto `PlanStatus`, `PlanItem`, snapshot field `plan @9`, and event branch `planChanged @13`.

- [ ] **Step 1: Write failing schema and conversion tests**

Extend `tests/rpc_schema.rs` to assert the new declarations and exact ordinals while retaining every existing ordinal assertion. Add conversion round trips for all five statuses and ordered items, an empty plan, `planChanged`, and an invalid raw enum discriminant.

Extend the scripted snapshot/event fixtures in `tests/rpc_transport.rs` and `tests/client_server.rs` so reattachment and observer delivery preserve the plan.

- [ ] **Step 2: Run RPC tests and verify failure**

Run:

```bash
cargo test --test rpc_schema --test rpc_transport --test client_server
```

Expected: schema assertions and conversion compilation fail.

- [ ] **Step 3: Extend the source schema and regenerate bindings**

Add:

```capnp
enum PlanStatus {
  pending @0;
  inProgress @1;
  completed @2;
  blocked @3;
  cancelled @4;
}

struct PlanItem {
  step @0 :Text;
  status @1 :PlanStatus;
}
```

Add `plan @9 :List(PlanItem)` to `SessionSnapshot` and `planChanged @13 :List(PlanItem)` to `EventEnvelope`. Then run:

```bash
capnp --version
scripts/generate-rpc.sh
```

Expected: Cap'n Proto 1.5.0-compatible output and a regenerated checked-in `src/rpc/moh_capnp.rs`; never hand-edit that file.

- [ ] **Step 4: Implement conversion helpers**

Add paired `write_plan_status`/`read_plan_status`, `write_plan_items`/`read_plan_items`, and wire them into snapshot/event conversion. Parse decoded step text through `PlanItem::parse`; map malformed enum/status/text to `RpcConversionError` without panicking.

- [ ] **Step 5: Run focused RPC tests**

Run:

```bash
cargo test --test rpc_schema --test rpc_transport --test client_server
cargo fmt --all -- --check
git diff --check
```

Expected: schema, generated bindings, conversion, and transport tests pass.

- [ ] **Step 6: Commit RPC transport**

```bash
git add schema/moh.capnp src/rpc/moh_capnp.rs src/rpc/convert.rs tests/rpc_schema.rs tests/rpc_transport.rs tests/client_server.rs
git diff --cached --check
git commit -m "feat(rpc): transport session plans"
```

---

### Task 6: Render plan progress and the Ctrl+T popup

**Files:**
- Modify: `src/client/ui/mod.rs`
- Modify: `src/client/ui/view.rs`
- Modify: `src/client/app.rs`
- Modify: `src/client/app_tests.rs`

**Interfaces:**
- Consumes: authoritative `SessionSnapshot.plan` and `SessionEvent::PlanChanged`.
- Produces: private `PopupKind::{Help, Plan}` and `UiState::popup()`/`set_popup()`.
- Produces: Ctrl+T toggle, `plan C/T` status segment, and read-only plan popup.

- [ ] **Step 1: Write failing client projection and keyboard tests**

In `src/client/app_tests.rs`, add tests that apply a `PlanChanged` envelope and assert exact replacement plus sequence advancement. Add key tests proving:

```rust
Ctrl+T: None -> Plan -> None
Ctrl+O: Plan -> Help
Ctrl+T: Help -> Plan
Escape: Plan -> None without cancelling a busy run
Ctrl+T with a selector: selector closes and Plan opens
```

Assert a plan event received while the popup is open updates the next rendered frame.

- [ ] **Step 2: Write failing Ratatui cell/style tests**

In `src/client/ui/view.rs` tests, construct all five statuses and assert:

- the status row contains `plan 1/4` when one item is completed and one is cancelled;
- empty plans omit the status segment;
- the popup shows ordered markers and every step;
- `in_progress`, `blocked`, `completed`, and `cancelled` use distinct expected colors/modifiers;
- empty popup text is `No active plan`;
- narrow and minimum-size frames render without panic; and
- Ctrl+T appears in wide, narrow, and compact Help layouts.

- [ ] **Step 3: Run client tests and verify failure**

Run:

```bash
cargo test --lib client::app_tests
cargo test --lib client::ui::view::tests
```

Expected: compilation/assertions fail because plan projection and popup state are absent.

- [ ] **Step 4: Replace the Help boolean with popup state and wire events**

Introduce:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PopupKind { Help, Plan }
```

Store `Option<PopupKind>` in `UiState`. Opening either popup clears the menu; Escape clears the popup before selector or run cancellation logic. Handle Ctrl+T immediately after Ctrl+C/Ctrl+O global shortcuts and allow it while busy. Reduce `SessionEvent::PlanChanged(plan)` by replacing `projection.plan` and requesting redraw.

- [ ] **Step 5: Render compact progress and the popup**

Count `completed` items for `C`; count every non-cancelled item for `T`; add ` · plan C/T` before the working-directory segment only when the plan is non-empty. Render the popup with `Clear`, `Block`, `Paragraph`/`List`, clamped geometry, sanitized text, vertical clipping, and these semantics:

```text
○ pending
▶ in_progress
✓ completed
! blocked
– cancelled
```

Use cyan/bold for active, red/bold for blocked, green for completed, dim for cancelled, and default/muted for pending. The popup is read-only and scroll-free in this milestone; clip excess rows and show a final dim `… N more` row.

- [ ] **Step 6: Run focused client tests**

Run:

```bash
cargo test --lib client::app_tests
cargo test --lib client::ui::view::tests
cargo fmt --all -- --check
git diff --check
```

Expected: projection, keyboard precedence, visible cells/styles, and constrained layouts pass.

- [ ] **Step 7: Commit the TUI**

```bash
git add src/client/app.rs src/client/app_tests.rs src/client/ui/mod.rs src/client/ui/view.rs
git diff --cached --check
git commit -m "feat(tui): display execution plans"
```

---

### Task 7: Document, verify, and qualify the complete feature

**Files:**
- Modify: `README.md`
- Modify only if verification exposes a scoped defect: files already named in Tasks 1-6

**Interfaces:**
- Consumes: the complete approved feature.
- Produces: user-facing documentation and final automated/PTY evidence.

- [ ] **Step 1: Add README documentation**

Document the exact tool schema, five statuses, whole-list replacement, one-active invariant, empty clearing, session durability, persistence warnings, `plan C/T`, Ctrl+T, and the distinction from task graphs and Plan Mode. State that plans are application state under Moh's state directory and never modify the workspace.

- [ ] **Step 2: Run every focused integration surface once more**

Run:

```bash
cargo test --test plan_tool --test tool_schema_semantics --test session_store
cargo test --test session_projection --test session_actor --test session_manager
cargo test --test rig_runtime --test rpc_schema --test rpc_transport --test client_server
cargo test --lib client::app_tests
cargo test --lib client::ui::view::tests
```

Expected: all focused tests pass.

- [ ] **Step 3: Run the full automated gate from a fresh command**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --locked
git diff --check
```

Expected: every command exits 0 with no warnings promoted by Clippy and no whitespace errors.

- [ ] **Step 4: Exercise deterministic PTY behavior**

Build first, create isolated application directories, and launch the real binary in an explicit PTY:

```bash
cargo build --locked
plan_accept_dir=$(mktemp -d)
mkdir -p "$plan_accept_dir/state" "$plan_accept_dir/runtime" "$plan_accept_dir/config"
XDG_STATE_HOME="$plan_accept_dir/state" \
XDG_RUNTIME_DIR="$plan_accept_dir/runtime" \
XDG_CONFIG_HOME="$plan_accept_dir/config" \
target/debug/moh
```

In the prompt, request: `Use update_plan with one completed, one in-progress, one blocked, and one cancelled step, then wait before answering.` Verify and record:

1. an `update_plan` call changes the status count before the run completes;
2. Ctrl+T opens and closes the popup while busy;
3. Ctrl+O replaces Plan, Ctrl+T replaces Help, and Escape closes the popup without cancelling;
4. detaching and reattaching preserves the plan;
5. stopping and restarting the backend restores the plan from SQLite;
6. resize and minimum-size handling remain safe; and
7. normal exit restores the original terminal.

If the paid provider cannot be exercised, report model-selected tool behavior as unrun while still completing deterministic actor/RPC/TUI PTY checks. Do not represent mocked evidence as paid-network evidence.

- [ ] **Step 5: Commit documentation or scoped verification fixes**

```bash
git add README.md
git diff --cached --check
git commit -m "docs(plan): document execution plans"
```

If verification required code fixes, stage their exact paths and use a separate conventional `fix(...)` commit before the documentation commit.

- [ ] **Step 6: Inspect final branch state**

Run:

```bash
git status --short --branch
git log --oneline --decorate -10
```

Expected: the branch is clean and contains the design commit, plan commit, and focused conventional implementation commits from Tasks 1-7.
