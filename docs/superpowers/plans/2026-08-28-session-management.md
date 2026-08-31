# Session Management Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add durable, resumable, switchable, browsable, renameable, and removable Moh sessions while keeping empty chats ephemeral and detached work running in the backend.

**Architecture:** Extend the existing SQLite store, session actors, manager, and Cap'n Proto backend as the authority for durable lifecycle state. Replace the fixed terminal session adapter with a workspace controller that owns either one explicit backend attachment or a client-local draft, then layer a Ratatui session browser over that controller.

**Tech Stack:** Rust stable, Tokio current-thread runtime, rusqlite, Cap'n Proto/capnp-rpc, Rig/Codex Responses transport, Crossterm, Ratatui, and the existing fake-engine/test harnesses.

**Spec:** `docs/superpowers/specs/2026-08-28-session-management-design.md`

## Global Constraints

- Use conventional commits and stage only the files named by each task.
- Follow TDD: add a focused failing test, observe the intended failure, implement the smallest complete behavior, then rerun the focused and affected suites.
- The terminal client owns at most one durable session attachment.
- `/new` accepts no arguments and always creates a client-local empty draft.
- An empty draft has no ID, actor, job registry, database row, or browser entry.
- Persist the first user message before the model stream is polled.
- Persist visible failed, cancelled, and interrupted turns, but pass only successful exchanges to the model.
- AI title generation is independent of conversation history and automated tests use fakes rather than live network calls.
- Titles contain 1-64 Unicode scalar values, may duplicate, and manual rename wins over pending generation.
- Confirmed deletion is permanent and must cancel the active run, reap jobs, and disconnect all attachments.
- Background runs and jobs survive client switching/detachment but not backend death.
- Keep `SessionSnapshot` authoritative for every durable session; draft presentation state must remain visibly distinct.
- Cap'n Proto generated bindings remain checked in.
- Final verification is `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-targets`, `cargo build --locked`, and `git diff --check`.

---

## File Structure

### New focused units

- `src/session/title.rs`: validated titles, deterministic fallback, generated-title sanitation, and the title-generation port.
- `src/runtime/rig/title.rs`: production Codex implementation of the title-generation port.
- `src/client/workspace.rs`: backend connection plus the one active draft/session attachment and all switch/fallback operations.
- `src/client/ui/session_browser.rs`: browser modes, fuzzy filtering, grouping, nested rename/delete state, navigation, and modal rendering.
- `tests/session_title.rs`: public title-domain behavior.

### Existing units with expanded responsibility

- `src/session/types.rs`: durable turn, record, summary, selector, and event values.
- `src/session/store.rs`: schema v2, migration, durable transcript/history, title lookup/mutation, scoped listing, and deletion.
- `src/session/projection.rs`: reconstruction from durable transcript and reduction of title/deletion events.
- `src/session/actor.rs`: durable event ordering, exact attachments, rename/title application, and deletion quiescence.
- `src/session/manager.rs`: startup choice, materialization, title-task ownership, switching operations, and deletion coordination.
- `src/backend/activity.rs`: title-task activity in idle eligibility.
- `schema/moh.capnp`, `src/rpc/moh_capnp.rs`, `src/rpc/convert.rs`: protocol v2 values and generated bindings.
- `src/rpc/server.rs`, `src/rpc/client.rs`: typed lifecycle RPC and exact detach.
- `src/client/session.rs`, `src/client/app.rs`, `src/client/ui/mod.rs`, `src/client/ui/view.rs`: workspace-facing application loop and draft/session presentation.
- `src/cli.rs`, `src/main.rs`: new CLI forms and startup dispatch.
- Existing `tests/session_*.rs`, `tests/rpc_*.rs`, `tests/client_server.rs`, `tests/cli.rs`, and `src/client/app_tests.rs`: cross-layer regression coverage.

---

### Task 1: Session title domain and generation port

**Files:**
- Create: `src/session/title.rs`
- Create: `tests/session_title.rs`
- Modify: `src/session/mod.rs`

**Interfaces:**
- Produces: `SessionTitle`, `SessionTitleParseError`, `TitleSource`, `fallback_title(&str) -> SessionTitle`, `sanitize_generated_title(&str) -> Option<SessionTitle>`, `TitleRequest`, `TitleGenerationError`, and object-safe `SessionTitleGenerator::generate(TitleRequest) -> BoxFuture<'static, Result<String, TitleGenerationError>>`.
- Consumes: `SessionId`, `ReasoningLevel`, and `futures::future::BoxFuture` already available in the crate.

- [ ] **Step 1: Add failing title validation and sanitation tests**

```rust
#[test]
fn fallback_collapses_whitespace_and_truncates_on_a_scalar_boundary() {
    let title = fallback_title("  investigate\n\tthis   session persistence failure in detail  ");
    assert_eq!(
        title.as_str(),
        "investigate this session persistence failure in detail"
    );
}

#[test]
fn generated_title_uses_first_plain_nonempty_line() {
    assert_eq!(
        sanitize_generated_title("\n**\"Fix session switching\"**\nignored")
            .unwrap()
            .as_str(),
        "Fix session switching"
    );
    assert!(sanitize_generated_title("\u{1b}[2J\n").is_none());
}
```

- [ ] **Step 2: Run the title tests and observe the missing API**

Run: `cargo test --test session_title`

Expected: FAIL because `moh::session::{fallback_title, sanitize_generated_title, SessionTitle}` do not exist.

- [ ] **Step 3: Implement validated titles and deterministic helpers**

```rust
pub const MAX_SESSION_TITLE_SCALARS: usize = 64;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionTitle(String);

impl SessionTitle {
    pub fn parse(value: impl Into<String>) -> Result<Self, SessionTitleParseError> {
        let value = value.into();
        let count = value.chars().count();
        if count == 0 || count > MAX_SESSION_TITLE_SCALARS
            || value.trim() != value || value.chars().any(char::is_control)
        {
            return Err(SessionTitleParseError);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str { &self.0 }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TitleSource { Fallback, Generated, Manual }
```

Implement `fallback_title` by collapsing Unicode whitespace, preferring the final word boundary at or before 63 scalars when truncation is required, and appending `…`. Implement `sanitize_generated_title` by selecting the first nonblank line, trimming paired surrounding `"`, `'`, `` ` ``, `*`, and `_`, removing controls, collapsing whitespace, and calling `SessionTitle::parse` after the same bounded truncation.

Also implement `Display` for CLI/UI use and private stored-string conversions for `TitleSource` with exact values `fallback`, `generated`, and `manual`.

- [ ] **Step 4: Add the async title-generation boundary**

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TitleRequest {
    pub session_id: SessionId,
    pub model: String,
    pub reasoning: ReasoningLevel,
    pub first_message: String,
    pub expected_revision: u64,
}

pub trait SessionTitleGenerator: Send + Sync {
    fn generate(
        &self,
        request: TitleRequest,
    ) -> BoxFuture<'static, Result<String, TitleGenerationError>>;
}
```

Make `TitleGenerationError` cloneable and safe to log without provider response bodies.

- [ ] **Step 5: Run focused and formatting checks**

Run: `cargo test --test session_title && cargo fmt --all -- --check`

Expected: PASS.

- [ ] **Step 6: Commit the title domain**

```bash
git add src/session/title.rs src/session/mod.rs tests/session_title.rs
git commit -m "feat(session): add session title domain"
```

---

### Task 2: Durable session schema v2 and migration

**Files:**
- Modify: `src/session/types.rs`
- Modify: `src/session/store.rs`
- Modify: `src/session/mod.rs`
- Modify: `tests/session_store.rs`
- Modify fixture initializers in: `tests/session_projection.rs`, `tests/session_actor.rs`, `tests/session_manager.rs`, `tests/rpc_schema.rs`, `tests/rpc_transport.rs`, `tests/client_server.rs`, `src/client/app_tests.rs`, `src/client/ui/view.rs`, `src/main.rs`

**Interfaces:**
- Consumes: `SessionTitle` and `TitleSource` from Task 1.
- Produces: `SessionRecord { title, title_source, title_revision, transcript, turns, ... }`, `DurableTurn`, `TurnStatus`, title-based `SessionSelector`, and schema version 2.

- [ ] **Step 1: Add a failing v1-to-v2 migration test**

Create a literal version-1 database in `tests/session_store.rs`, containing one named session with a successful pair, one unnamed session with a successful pair, and one empty default session. Reopen it through `SessionStore::open_at` and assert:

```rust
assert_eq!(sessions.len(), 2);
assert_eq!(sessions[0].title.as_str(), "review");
assert_eq!(sessions[1].title.as_str(), "Investigate the parser");
assert!(sessions.iter().all(|summary| !summary.title.as_str().is_empty()));
```

Also query `PRAGMA user_version` through the test connection and assert `2`.

- [ ] **Step 2: Run the migration test and verify the version mismatch**

Run: `cargo test --test session_store migrates_v1_sessions_to_titles_and_drops_empty_rows -- --exact`

Expected: FAIL because the store still opens schema version 1 without the v2 title/transcript tables.

- [ ] **Step 3: Replace the durable domain fields**

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnStatus { Running, Completed, Failed, Cancelled, Interrupted }

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableTurn {
    pub ordinal: u64,
    pub run_id: u64,
    pub prompt_position: u64,
    pub status: TurnStatus,
}

pub struct SessionRecord {
    pub id: SessionId,
    pub title: SessionTitle,
    pub title_source: TitleSource,
    pub title_revision: u64,
    pub cwd: Vec<u8>,
    pub settings: SessionSettings,
    pub transcript: Vec<TranscriptItem>,
    pub turns: Vec<DurableTurn>,
    pub history: Vec<Message>,
    pub created_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
}
```

Replace `SessionSelector::Name` with `SessionSelector::Title(SessionTitle)`. Replace summary `name`/`is_default` with `title`, `title_revision: u64`, `running_jobs: u32`, and `running: bool`. Update every listed fixture explicitly so the crate compiles against the new domain.

- [ ] **Step 4: Define schema version 2**

Create v2 `sessions`, `messages`, `transcript_items`, and `turns` tables. `sessions.title` is non-null and non-unique; `transcript_items` stores ordered kind-specific nullable columns plus JSON arguments; `turns` stores ordinal, run ID, prompt position, and checked status. All child tables use `ON DELETE CASCADE` and `(session_id, position/ordinal)` primary keys.

- [ ] **Step 5: Implement the transactional v1 migration**

Implement `migrate_v1_to_v2(transaction: &rusqlite::Transaction<'_>)`. Read old rows in ID order, skip rows with no messages, use the old name when present, otherwise call `fallback_title` on the first user message, insert successful user/assistant transcript rows and completed turns, preserve IDs/settings/timestamps, replace old tables, and set `PRAGMA user_version = 2` before commit.

- [ ] **Step 6: Implement v2 row decoding and checkpoint encoding**

Decode all `TranscriptItem` variants without rendering logic. Reject unknown kinds/statuses and invalid JSON through `SessionStoreError::InvalidStoredData`. Extend full checkpoint to rewrite `messages`, `transcript_items`, and `turns` transactionally.

- [ ] **Step 7: Run the store suite and all compile-time fixture consumers**

Run: `cargo test --test session_store && cargo test --all-targets --no-fail-fast`

Expected: PASS; no fixture retains `name` or `is_default`.

- [ ] **Step 8: Commit schema v2**

```bash
git add src/session/types.rs src/session/store.rs src/session/mod.rs tests/session_store.rs tests/session_projection.rs tests/session_actor.rs tests/session_manager.rs tests/rpc_schema.rs tests/rpc_transport.rs tests/client_server.rs src/client/app_tests.rs src/client/ui/view.rs src/main.rs
git commit -m "feat(session): migrate durable sessions to schema v2"
```

---

### Task 3: Repository lifecycle operations

**Files:**
- Modify: `src/session/types.rs`
- Modify: `src/session/store.rs`
- Modify: `src/session/mod.rs`
- Modify: `tests/session_store.rs`

**Interfaces:**
- Consumes: schema v2 and title values from Tasks 1-2.
- Produces: `SessionListScope`, `MaterializeSession`, repository `materialize`, `list`, `rename`, `compare_and_set_generated_title`, and `delete` operations plus `AmbiguousTitle`.

- [ ] **Step 1: Add failing lifecycle repository tests**

Add focused tests named:

```text
materialize_persists_prompt_and_running_turn_atomically
list_supports_project_and_global_scopes_in_stable_order
duplicate_titles_require_id_after_ambiguous_lookup
manual_rename_increments_revision_and_allows_duplicates
generated_title_compare_and_set_cannot_overwrite_manual_rename
delete_cascades_every_session_child_row
loading_converts_running_turn_to_one_interruption_idempotently
```

The materialization assertion must reopen the database and verify a visible `User(prompt)`, empty successful history, and `TurnStatus::Running` before any actor exists.

- [ ] **Step 2: Run the lifecycle tests and observe missing repository methods**

Run: `cargo test --test session_store materialize_ -- --nocapture`

Expected: FAIL because `SessionRepository::materialize` is not defined.

- [ ] **Step 3: Add exact lifecycle request and scope types**

```rust
pub enum SessionListScope {
    Project(Vec<u8>),
    All,
}

pub struct MaterializeSession {
    pub cwd: Vec<u8>,
    pub title: SessionTitle,
    pub settings: SessionSettings,
    pub prompt: String,
    pub run_id: u64,
    pub created_at: DateTime<Utc>,
}
```

- [ ] **Step 4: Replace eager-create repository methods**

Remove `find_or_create_default` and `create`. Add object-safe boxed-future methods with these signatures:

```rust
fn materialize(&self, request: MaterializeSession)
    -> BoxFuture<'static, Result<SessionRecord, SessionStoreError>>;
fn list(&self, scope: SessionListScope)
    -> BoxFuture<'static, Result<Vec<SessionSummary>, SessionStoreError>>;
fn rename(&self, id: SessionId, title: SessionTitle)
    -> BoxFuture<'static, Result<SessionRecord, SessionStoreError>>;
fn compare_and_set_generated_title(
    &self, id: SessionId, expected_revision: u64, title: SessionTitle,
) -> BoxFuture<'static, Result<Option<SessionRecord>, SessionStoreError>>;
fn delete(&self, id: SessionId)
    -> BoxFuture<'static, Result<(), SessionStoreError>>;
```

Title resolution returns `SessionStoreError::AmbiguousTitle { title, ids }` when more than one exact CWD-scoped match exists.

- [ ] **Step 5: Implement interruption recovery**

During `load`/`resolve`, transactionally turn every `Running` turn into `Interrupted`, append exactly one `TranscriptItem::Failed` using `RunStage::Finalization`, `RunFailureKind::RuntimeInfrastructure`, `retryable = true`, and message `run interrupted by backend restart`, then return the updated record. A second load must not append another item.

- [ ] **Step 6: Run repository and whitespace checks**

Run: `cargo test --test session_store && git diff --check`

Expected: PASS.

- [ ] **Step 7: Commit repository lifecycle support**

```bash
git add src/session/types.rs src/session/store.rs src/session/mod.rs tests/session_store.rs
git commit -m "feat(session): add durable lifecycle operations"
```

---

### Task 4: Durable projection and actor event ordering

**Files:**
- Modify: `src/session/projection.rs`
- Modify: `src/session/actor.rs`
- Modify: `tests/session_projection.rs`
- Modify: `tests/session_actor.rs`

**Interfaces:**
- Consumes: durable transcript/turn state and full checkpoints from Tasks 2-3.
- Produces: projection reconstruction from `SessionRecord.transcript`, checkpoint-before-broadcast ordering for durable events, and `SessionHandle::spawn_materialized(..., first_prompt)`.

- [ ] **Step 1: Add failing projection restoration tests**

Build a record containing successful, failed, cancelled, tool, and interrupted items. Assert `SessionProjection::from_record(record, catalog).snapshot([])` preserves their exact order and derives no active run from terminal durable turns.

- [ ] **Step 2: Add a checkpoint-before-stream-poll actor test**

Use an engine stream that increments an atomic counter on first poll and a repository fake that records checkpoints. Materialize the actor with the first prompt and assert:

```rust
assert_eq!(stream_polls.load(Ordering::SeqCst), 0);
assert!(repository.record().transcript.contains(&TranscriptItem::User("first".into())));
```

Only after `next_event` is driven may `stream_polls` become `1`.

- [ ] **Step 3: Run the focused tests and verify current reconstruction/order fails**

Run: `cargo test --test session_projection restores_durable_visible_transcript -- --exact && cargo test --test session_actor materialization_checkpoints_before_stream_poll -- --exact`

Expected: FAIL because projection still derives transcript only from successful history and the actor has no materialized-start path.

- [ ] **Step 4: Rebuild projection from the durable transcript**

Change `SessionProjection::from_record` to clone `record.transcript`, initialize title-based summary fields, and keep `active_run = None`. Add internal methods used only by the actor to install the already-persisted first user prompt as the active run without appending it twice.

- [ ] **Step 5: Persist every stable visible transition before broadcast**

For Started, ToolStarted, Completed, Failed, and Cancelled: apply the event to projection and record/turn state, attempt `repository.checkpoint(record.clone())`, then broadcast the semantic event followed by any persistence warning. AssistantDelta and ToolFinished remain process-local. Completed also copies `harness.history()` into `record.history`; failed/cancelled never do.

- [ ] **Step 6: Add and implement `spawn_materialized`**

```rust
pub fn spawn_materialized<E>(
    repository: Arc<dyn SessionRepository>,
    record: SessionRecord,
    projection: SessionProjection,
    bundle: SessionEngineBundle<E>,
    first_prompt: String,
    activity: ActivityTracker,
) -> Result<Self, SessionCommandError>
```

Create the harness, call `submit` to obtain the expected first run ID without polling its returned stream, verify it matches the durable running turn, install the active projection, and then spawn the actor loop.

- [ ] **Step 7: Run actor, projection, and harness suites**

Run: `cargo test --test session_projection && cargo test --test session_actor && cargo test --test harness`

Expected: PASS.

- [ ] **Step 8: Commit durable event ordering**

```bash
git add src/session/projection.rs src/session/actor.rs tests/session_projection.rs tests/session_actor.rs
git commit -m "feat(session): persist visible session events"
```

---

### Task 5: Exact attachment identity and detach

**Files:**
- Modify: `src/session/types.rs`
- Modify: `src/session/actor.rs`
- Modify: `src/session/manager.rs`
- Modify: `src/session/mod.rs`
- Modify: `tests/session_actor.rs`
- Modify: `tests/session_manager.rs`

**Interfaces:**
- Produces: `AttachmentId(u64)`, `SessionHandle::attach(ConnectionId, AttachmentId)`, `SessionHandle::detach(ConnectionId, AttachmentId)`, and connection-wide cleanup retained as a fallback.
- Consumes: current actor observer queues and `ConnectionId`.

- [ ] **Step 1: Add failing repeated-switch attachment tests**

Attach IDs 1 and 2 from the same connection, detach only ID 1, and assert the snapshot reports one attached client and ID 2 still receives events. Then call connection-wide detach and assert zero attachments.

- [ ] **Step 2: Run the actor test and confirm detachment is too broad**

Run: `cargo test --test session_actor detach_removes_only_the_exact_attachment -- --exact`

Expected: FAIL because observers are keyed only by `ConnectionId`.

- [ ] **Step 3: Add checked attachment IDs**

```rust
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AttachmentId(pub u64);
```

Reject zero at the RPC conversion boundary; keep the domain tuple type cheap to copy.

- [ ] **Step 4: Key observers by connection and attachment**

Add `attachment_id` to the actor observer record. Change exact detach to retain every observer except the matching pair. Count attached clients by observers rather than distinct connection IDs; one terminal controller still contributes exactly one after a successful switch.

- [ ] **Step 5: Expose manager exact detach**

Add `SessionManagerHandle::detach(session_id, connection_id, attachment_id)` and keep `detach_connection(connection_id)` for socket teardown. A missing exact attachment is idempotent success.

- [ ] **Step 6: Run actor and manager suites**

Run: `cargo test --test session_actor && cargo test --test session_manager`

Expected: PASS.

- [ ] **Step 7: Commit exact attachment lifecycle**

```bash
git add src/session/types.rs src/session/actor.rs src/session/manager.rs src/session/mod.rs tests/session_actor.rs tests/session_manager.rs
git commit -m "feat(session): detach exact client attachments"
```

---

### Task 6: Production AI title generator and idle tracking

**Files:**
- Create: `src/runtime/rig/title.rs`
- Modify: `src/runtime/rig/mod.rs`
- Modify: `src/runtime/rig/codex.rs`
- Modify: `src/session/runtime.rs`
- Modify: `src/backend/activity.rs`
- Modify: `tests/backend_activity.rs`
- Modify: `tests/rig_runtime.rs`

**Interfaces:**
- Consumes: `SessionTitleGenerator`/`TitleRequest` from Task 1 and `CodexModelFactory`.
- Produces: `CodexTitleGenerator`, `SessionEngineFactory::title_generator() -> Arc<dyn SessionTitleGenerator>`, and title-task activity guards.

- [ ] **Step 1: Add failing idle and factory tests**

With Tokio time paused, assert `wait_for_idle` does not resolve while a title-task guard is held and resolves after the guard drops and the configured idle timeout elapses. Also assert `CodexSessionEngineFactory::title_generator()` returns a cloneable shared generator without creating a session runtime.

- [ ] **Step 2: Run focused tests and observe missing title activity**

Run: `cargo test --test backend_activity title_tasks_veto_idle_shutdown -- --exact && cargo test --test rig_runtime factory_exposes_independent_title_generator -- --exact`

Expected: FAIL because activity and factory expose no title-task concepts.

- [ ] **Step 3: Add RAII title-task activity**

Add `title_tasks: u32` to `ActivitySnapshot`, include it in equality/generation updates and every backend idle predicate, and expose:

```rust
pub fn begin_title_task(&self) -> TitleTaskGuard
```

The guard increments once and decrements exactly once on drop, including cancellation paths.

- [ ] **Step 4: Implement `CodexTitleGenerator`**

Make `ReasoningLevel::as_codex_effort` visible to sibling Rig modules with `pub(super)`. Build one-tool-free, one-turn Rig agent from `CodexModelFactory::completion_model(model, ModelCallBudget::new(1))`. Use a fixed preamble requesting one 3-8 word plain-text title, pass only `first_message`, set the request reasoning to `request.reasoning.as_codex_effort()`, and map authentication/transport/completion errors to sanitized `TitleGenerationError` values. Return raw text; Task 1 sanitation remains the only acceptance boundary.

- [ ] **Step 5: Wire the generator into the engine factory**

Store `Arc<CodexTitleGenerator>` in `CodexSessionEngineFactory`, initialize it from the shared model factory, and return it through the new trait method. Fake factories in session tests return scripted generators.

- [ ] **Step 6: Run runtime and backend suites**

Run: `cargo test --test rig_runtime && cargo test --test backend_activity && cargo test --test backend_activity -- --test-threads=1`

Expected: PASS without network access.

- [ ] **Step 7: Commit title generation infrastructure**

```bash
git add src/runtime/rig/title.rs src/runtime/rig/mod.rs src/runtime/rig/codex.rs src/session/runtime.rs src/backend/activity.rs tests/backend_activity.rs tests/rig_runtime.rs
git commit -m "feat(runtime): generate session titles with ai"
```

---

### Task 7: Actor rename, generated-title, and deletion lifecycle

**Files:**
- Modify: `src/session/types.rs`
- Modify: `src/session/projection.rs`
- Modify: `src/session/actor.rs`
- Modify: `tests/session_projection.rs`
- Modify: `tests/session_actor.rs`

**Interfaces:**
- Produces: `SessionEvent::TitleChanged`, `SessionEvent::Deleted`, `SessionHandle::rename`, `apply_generated_title`, `prepare_delete`, `finish_delete`, `abort_delete`, and a terminal actor outcome.
- Consumes: repository title CAS/delete prerequisites and exact attachments.

- [ ] **Step 1: Add failing title race and active-delete actor tests**

Use a scripted repository and jobs registry to assert manual rename broadcasts the new title, a generated result with the old revision is ignored, `prepare_delete` cancels the active run and shuts down running jobs, and `finish_delete` sends `Deleted` to every observer before closing their queues.

- [ ] **Step 2: Run focused actor tests**

Run: `cargo test --test session_actor rename_wins_over_pending_generated_title -- --exact && cargo test --test session_actor delete_quiesces_run_jobs_and_observers -- --exact`

Expected: FAIL because actor commands/events are missing.

- [ ] **Step 3: Add ordered metadata events**

```rust
SessionEvent::TitleChanged {
    title: SessionTitle,
    title_revision: u64,
},
SessionEvent::Deleted {
    session_id: SessionId,
},
```

Projection reduction updates summary title/revision for the first event. `Deleted` is terminal and does not mutate a reusable snapshot.

- [ ] **Step 4: Implement rename and generated-title commands**

Manual rename calls repository `rename`, updates actor record/projection, and broadcasts only after persistence succeeds. Generated title sanitizes the raw result, calls compare-and-set with the captured revision, broadcasts only on `Some(record)`, and treats invalid/failure/stale output as a no-op.

- [ ] **Step 5: Implement two-phase actor deletion**

`prepare_delete` sets `deleting`, rejects submit/attach/settings/job commands with a typed deleting error, cancels the harness if active, shuts down/reaps jobs, persists the terminal transcript, and returns. `finish_delete` broadcasts `Deleted`, clears activity, stops the job monitor, and exits the actor loop. `abort_delete` emits no deleted event but clears activity, closes observer queues, stops the job monitor, and exits so the manager can remove the quiesced actor and a client can rematerialize the retained row after repository deletion failure.

- [ ] **Step 6: Run projection and actor suites**

Run: `cargo test --test session_projection && cargo test --test session_actor`

Expected: PASS.

- [ ] **Step 7: Commit actor lifecycle mutations**

```bash
git add src/session/types.rs src/session/projection.rs src/session/actor.rs tests/session_projection.rs tests/session_actor.rs
git commit -m "feat(session): manage titles and deletion in actors"
```

---

### Task 8: Manager startup, materialization, listing, and deletion orchestration

**Files:**
- Modify: `src/session/types.rs`
- Modify: `src/session/manager.rs`
- Modify: `src/session/mod.rs`
- Modify: `tests/session_manager.rs`

**Interfaces:**
- Produces: `DraftDefaults`, `StartupResult`, manager `startup`, `materialize_and_submit`, scoped `list`, `rename`, `delete`, and title-task completion routing.
- Consumes: Tasks 3-7 repository, actor, factory, generator, and activity interfaces.

- [ ] **Step 1: Add failing manager behavior tests**

Add exact tests for:

```text
startup_selects_latest_running_run_or_job_in_project
startup_returns_draft_when_only_idle_sessions_exist
materialization_persists_before_actor_stream_is_polled
list_overlays_live_run_job_and_attachment_state_in_both_scopes
title_task_success_updates_title_and_manual_race_is_ignored
delete_removes_cold_session
delete_coordinates_live_actor_and_repository
delete_failure_drops_quiesced_actor_but_retains_record
```

- [ ] **Step 2: Run the startup tests and observe eager-default behavior**

Run: `cargo test --test session_manager startup_ -- --nocapture`

Expected: FAIL because `open_default` still eagerly creates a durable row.

- [ ] **Step 3: Add startup and draft result types**

```rust
pub struct DraftDefaults {
    pub cwd: Vec<u8>,
    pub settings: SessionSettings,
    pub catalog: ModelCatalogState,
}

pub enum StartupResult {
    Draft(DraftDefaults),
    Attached(ManagedSession),
}
```

- [ ] **Step 4: Implement atomic startup selection**

List project summaries, overlay every live actor snapshot, filter `running`, sort descending by `last_activity` then ID, and attach the first candidate within the same manager command. Return factory defaults/catalog as `Draft` when no candidate remains. Remove `OpenRequest::Default` and `OpenRequest::Create`.

- [ ] **Step 5: Implement materialize-and-submit**

Validate settings against the catalog, choose first run ID `0`, call repository `materialize` with fallback title and running turn, create the isolated runtime, spawn via `SessionHandle::spawn_materialized`, attach the requester, insert the actor, and return snapshot plus run ID. For title generation, select the lowest `ReasoningLevel` advertised by the initially selected model (falling back to that model's selected effort only when catalog metadata is unavailable), start the title future under an activity guard, and route completion back to that actor with session ID and expected revision.

- [ ] **Step 6: Implement scoped listing and mutation commands**

`list(SessionListScope)` overlays live title, busy, running-job count, running flag, attachments, and activity. Rename routes to a live actor or repository. Delete calls `prepare_delete` when live, repository `delete`, then `finish_delete`; on repository error call `abort_delete`, remove the actor from the registry, and return the persistence error. Cold deletion directly calls the repository.

- [ ] **Step 7: Make shutdown join title tasks**

Track title tasks in the manager loop, drain completed tasks continuously, and abort/join none prematurely. Explicit and idle shutdown wait for all dispatched title tasks, then flush/shut down actors. The RAII activity guard covers every completion and cancellation branch.

- [ ] **Step 8: Run manager and backend activity suites**

Run: `cargo test --test session_manager && cargo test --test backend_activity`

Expected: PASS.

- [ ] **Step 9: Commit manager orchestration**

```bash
git add src/session/types.rs src/session/manager.rs src/session/mod.rs tests/session_manager.rs
git commit -m "feat(session): orchestrate session lifecycle"
```

---

### Task 9: Cap'n Proto v2 schema and conversion layer

**Files:**
- Modify: `schema/moh.capnp`
- Modify generated: `src/rpc/moh_capnp.rs`
- Modify: `src/rpc/convert.rs`
- Modify: `src/rpc/mod.rs`
- Modify: `tests/rpc_schema.rs`

**Interfaces:**
- Produces wire values for startup/draft, materialization, scoped listing, title ambiguity, rename, delete, detach, title changes, and deletion.
- Consumes final domain types from Tasks 1-8.

- [ ] **Step 1: Add failing conversion tests for every new union arm**

Round-trip `DraftDefaults`, both `StartupResult` arms, materialize success, project/all scopes, duplicate-title ambiguity with two IDs, title-changed event, deleted event, summaries with running jobs, and exact attachment IDs. Test zero attachment ID and malformed title rejection.

- [ ] **Step 2: Run schema tests and observe missing generated values**

Run: `cargo test --test rpc_schema`

Expected: FAIL because protocol v1 has none of the new arms/fields.

- [ ] **Step 3: Define protocol v2**

Set major `2`, minor `0`. Replace eager backend methods with:

```capnp
startup @1 (cwd :Data, attachmentId :UInt64, observer :Observer)
    -> (result :StartupResult);
materialize @2 (cwd :Data, prompt :Text, settings :SessionSettings,
    attachmentId :UInt64, observer :Observer) -> (result :MaterializeResult);
openSession @3 (selector :SessionSelector, cwdForTitle :Data,
    attachmentId :UInt64, observer :Observer) -> (result :OpenResult);
listSessions @4 (scope :SessionListScope, cwd :Data)
    -> (result :SessionListResult);
renameSession @5 (id :Text, title :Text) -> (result :CommandResult);
deleteSession @6 (id :Text) -> (result :CommandResult);
```

Add `Session.detach @6 (attachmentId :UInt64)`. Replace name/default summary fields with title/titleRevision/running/runningJobs. Add ambiguity IDs to `CommandError` and typed deleting/deleted error codes.

- [ ] **Step 4: Regenerate checked-in bindings**

Run: `scripts/generate-rpc.sh`

Expected: `src/rpc/moh_capnp.rs` changes and contains protocol v2 accessors.

- [ ] **Step 5: Implement bounded conversion functions**

Add read/write functions for every new value. Continue using existing wire length limits, RFC 3339 parsing, JSON validation, and sanitized command errors. Reject `attachmentId == 0`. Preserve raw CWD bytes independently from lossy display text.

- [ ] **Step 6: Run schema and formatting checks**

Run: `cargo test --test rpc_schema && cargo fmt --all -- --check && git diff --check`

Expected: PASS.

- [ ] **Step 7: Commit protocol v2**

```bash
git add schema/moh.capnp src/rpc/moh_capnp.rs src/rpc/convert.rs src/rpc/mod.rs tests/rpc_schema.rs
git commit -m "feat(rpc): define session management protocol"
```

---

### Task 10: Typed RPC server and client lifecycle methods

**Files:**
- Modify: `src/rpc/server.rs`
- Modify: `src/rpc/client.rs`
- Modify: `src/backend/server.rs`
- Modify: `tests/rpc_transport.rs`
- Modify: `tests/client_server.rs`

**Interfaces:**
- Produces: `RpcBackendClient::{startup, materialize, list_sessions, rename_session, delete_session}`, `RpcSessionClient::detach`, and typed remote deletion recovery.
- Consumes: protocol and manager APIs from Tasks 8-9.

- [ ] **Step 1: Add failing typed transport tests**

Cover draft startup without a row, materialize-and-attach, project/all lists, duplicate-title ambiguity, exact detach while the connection remains open, switch attachment counts, rename propagation, deletion of another client's current session, and v1/v2 incompatibility reporting.

- [ ] **Step 2: Run transport tests and observe unimplemented RPC methods**

Run: `cargo test --test rpc_transport typed_client_supports_session_lifecycle -- --exact`

Expected: FAIL because server/client protocol v2 methods are not implemented.

- [ ] **Step 3: Implement server forwarding and observer termination**

Validate attachment IDs before manager calls. Build observer pumps only for attached results. Forward startup/materialize/list/rename/delete through `BackendContext`. Implement `Session.detach` against the exact session/connection/attachment tuple. Deliver `SessionEvent::Deleted` before closing the pump.

- [ ] **Step 4: Implement typed client methods**

Allocate a `LocalAttachment` before startup/materialize/open requests and send its ID. Return:

```rust
pub enum RpcStartup {
    Draft(DraftDefaults),
    Attached(RpcSessionClient),
}

pub struct MaterializedSession {
    pub session: RpcSessionClient,
    pub run_id: u64,
}
```

`RpcSessionClient::detach(self)` sends exact detach, then drops its observer and capability. Connection disconnect remains the broad cleanup fallback.

- [ ] **Step 5: Make deleted-session recovery typed**

When a callback contains `Deleted`, return `SessionUpdate::Deleted { session_id, cwd }`. When snapshot gap recovery receives not-found for the current stable ID, return the same update rather than `ObserverClosed` or a generic command error. When an observer closes without `Deleted`, attempt one ordinary stable-ID reattachment; this rematerializes a session whose failed delete quiesced its old actor, while a missing row still becomes the typed deleted transition.

- [ ] **Step 6: Update backend protocol metadata**

Advertise protocol v2 and feature names for startup, materialization, global listing, rename, delete, and exact detach. Keep startup warnings unchanged.

- [ ] **Step 7: Run RPC and subprocess suites**

Run: `cargo test --test rpc_transport && cargo test --test client_server`

Expected: PASS.

- [ ] **Step 8: Commit RPC lifecycle support**

```bash
git add src/rpc/server.rs src/rpc/client.rs src/backend/server.rs tests/rpc_transport.rs tests/client_server.rs
git commit -m "feat(rpc): serve session lifecycle operations"
```

---

### Task 11: CLI forms and workspace controller

**Files:**
- Create: `src/client/workspace.rs`
- Modify: `src/client/mod.rs`
- Modify: `src/client/session.rs`
- Modify: `src/cli.rs`
- Modify: `src/main.rs`
- Modify: `tests/cli.rs`
- Modify: `src/client/app_tests.rs`

**Interfaces:**
- Produces: `LaunchMode`, `DraftState`, `ChatProjection`, `WorkspaceUpdate`, private `WorkspaceClient` trait, and production `RpcWorkspaceController`.
- Consumes: Task 10 typed backend/session clients.

- [ ] **Step 1: Add failing CLI parsing tests**

Assert `moh --new` parses, `moh --new review` is rejected, canonical `session-7` becomes an ID selector, every other non-flag Unicode selector becomes a title, and usage contains no named-new form.

- [ ] **Step 2: Add failing workspace controller tests**

With a scripted backend, assert startup returns draft/attachment correctly, `/new`-equivalent `new_draft()` detaches without consulting startup, first submit materializes while preserving prompt on failure, switch opens target before detaching old, and global switch changes active CWD.

- [ ] **Step 3: Run CLI/controller tests and observe old composition**

Run: `cargo test --test cli && cargo test --bin moh workspace_ -- --nocapture`

Expected: FAIL because CLI accepts a name and the client has only a fixed `SessionClient` adapter.

- [ ] **Step 4: Define draft and chat projections**

```rust
pub(crate) struct DraftState {
    pub cwd: Vec<u8>,
    pub settings: SessionSettings,
    pub catalog: ModelCatalogState,
}

pub(crate) enum ChatProjection {
    Draft(DraftState),
    Session(SessionSnapshot),
}

pub(crate) enum WorkspaceUpdate {
    Session(SessionUpdate),
    Deleted { session_id: SessionId, cwd: Vec<u8> },
}
```

- [ ] **Step 5: Replace `SessionClient` with workspace operations**

The private trait exposes current projection, next update, submit, cancel, model/effort selection, jobs, `new_draft`, scoped list, switch by ID, rename, delete, and startup fallback. Production controller owns `RpcBackendClient` plus `Option<RpcSessionClient>` and never more than one settled attachment.

- [ ] **Step 6: Implement safe switch and fallback**

Open and validate the target first. Replace the current capability only after success, then await old exact detach. Deletion of current or remote `Deleted` calls backend startup for that session CWD. `/new` instead detaches and installs fresh defaults without startup selection.

- [ ] **Step 7: Move startup dispatch from `main` into the controller**

`main` canonicalizes CWD, connects once, handles noninteractive `moh sessions`, and passes backend/CWD/launch mode to `client::run`. Remove `AttachMode::New(name)` and eager `open_when_ready` session creation.

- [ ] **Step 8: Run CLI, main, and controller tests**

Run: `cargo test --test cli && cargo test --bin moh && cargo test --test client_server`

Expected: PASS.

- [ ] **Step 9: Commit workspace control**

```bash
git add src/client/workspace.rs src/client/mod.rs src/client/session.rs src/cli.rs src/main.rs tests/cli.rs src/client/app_tests.rs
git commit -m "feat(client): control active session workspace"
```

---

### Task 12: Draft-aware application commands and presentation

**Files:**
- Create: `src/client/ui/session_browser.rs`
- Modify: `src/client/app.rs`
- Modify: `src/client/ui/mod.rs`
- Modify: `src/client/ui/view.rs`
- Modify: `src/client/app_tests.rs`

**Interfaces:**
- Consumes: `ChatProjection` and `WorkspaceClient` from Task 11.
- Produces: `/new`, `/sessions`, minimal `SessionBrowserState::{open, close, is_open}`, draft model/effort changes, title/new-chat status, and session/draft rendering without a synthetic `SessionSnapshot`.

- [ ] **Step 1: Add failing command-resolution tests**

Assert exact `/new` returns `AppAction::NewDraft`, `/new anything` returns `NewUsage`, `/sessions` returns `OpenSessionBrowser`, and unmatched slash text remains a model submission. Verify `/new` while another session is busy never calls cancel.

- [ ] **Step 2: Add failing draft rendering tests**

Render a draft into `TestBackend` and assert visible `new chat`, selected model/effort, active CWD, no session ID, ready status, and no persistence/jobs fields. Render a durable session and assert its title appears in the status line.

- [ ] **Step 3: Run focused app/view tests**

Run: `cargo test --bin moh new_command_enters_ephemeral_draft -- --exact && cargo test --bin moh draft_status_has_no_session_identity -- --exact`

Expected: FAIL because actions and renderers accept only `SessionSnapshot`.

- [ ] **Step 4: Add `/new` and `/sessions` command specs**

Extend command autocomplete/help and resolve exact usage before the unmatched-slash fallback. `/new` calls `WorkspaceClient::new_draft`, clears local notices/menu/scroll/editor, and never cancels. Create `SessionBrowserState { open: bool }`, store it in `UiState`, and make `/sessions` call `open()` so the state transition is real and testable before navigation and rendering are added.

- [ ] **Step 5: Make model/effort selection draft-aware**

For drafts, update `DraftState.settings` locally after validating against its catalog. For durable sessions, retain RPC commands and authoritative events. Disable `/ps`, `/kill`, and `/cancel` in drafts with neutral `No running ...` feedback.

- [ ] **Step 6: Render `ChatProjection` explicitly**

Extract common catalog/settings/CWD accessors without fabricating a session summary. Transcript rendering uses the durable snapshot or only the introduction/notices for a draft. Status renders `new chat` for drafts and sanitized title for sessions.

- [ ] **Step 7: Run all binary client tests**

Run: `cargo test --bin moh`

Expected: PASS.

- [ ] **Step 8: Commit draft-aware commands**

```bash
git add src/client/ui/session_browser.rs src/client/app.rs src/client/ui/mod.rs src/client/ui/view.rs src/client/app_tests.rs
git commit -m "feat(tui): add ephemeral new chat commands"
```

---

### Task 13: Session browser state, grouping, and filtering

**Files:**
- Modify: `src/client/ui/session_browser.rs`
- Modify: `src/client/ui/mod.rs`
- Modify: `src/client/app_tests.rs`

**Interfaces:**
- Produces: `BrowserMode`, `BrowserLayer`, `SessionBrowserState`, `BrowserRow`, `open`, `set_sessions`, `toggle_mode`, navigation, fuzzy filtering, rename draft, and delete confirmation state.
- Consumes: `SessionSummary`, `SessionId`, and existing `PromptEditor`.

- [ ] **Step 1: Add failing pure browser-state tests**

Cover local default on every open, Tab toggle, current-project-first global grouping, descending group/row activity with descending-ID ties, fuzzy matches across title/ID/CWD, selection preservation by stable ID after refresh, page and wheel clamps, F2 rename state, Ctrl+D confirmation state, and nested Escape behavior.

- [ ] **Step 2: Run browser-state tests and observe missing module**

Run: `cargo test --bin moh session_browser_state_ -- --nocapture`

Expected: FAIL because `SessionBrowserState` does not exist.

- [ ] **Step 3: Implement explicit browser state**

```rust
pub(super) enum BrowserMode { Project, Global }

pub(super) enum BrowserLayer {
    List,
    Rename { session_id: SessionId, editor: PromptEditor, error: Option<String> },
    ConfirmDelete { session_id: SessionId },
}

pub(super) struct SessionBrowserState {
    open: bool,
    mode: BrowserMode,
    query: PromptEditor,
    sessions: Vec<SessionSummary>,
    visible: Vec<BrowserRow>,
    selected: usize,
    selected_id: Option<SessionId>,
    offset: usize,
    layer: BrowserLayer,
    warning: Option<String>,
}
```

- [ ] **Step 4: Implement deterministic filtering and grouping**

Move the existing subsequence scorer from `app.rs` into a private shared helper in this module or `ui/mod.rs`. Project mode filters exact raw CWD equality before fuzzy matching. Global mode emits group headings and nonselectable rows. Preserve selection by ID; otherwise select the first session row.

- [ ] **Step 5: Implement navigation and nested layers**

Navigation skips group headings. Page size comes from the latest rendered viewport. Wheel steps three selectable rows. Rename starts with the current title; confirmation captures title/ID from the latest summary. Escape returns Rename/Confirm to List, then closes the browser.

- [ ] **Step 6: Run browser and existing menu/editor tests**

Run: `cargo test --bin moh session_browser && cargo test --bin moh menu_ && cargo test --bin moh editor_`

Expected: PASS.

- [ ] **Step 7: Commit browser state**

```bash
git add src/client/ui/session_browser.rs src/client/ui/mod.rs src/client/app_tests.rs
git commit -m "feat(tui): model session browser state"
```

---

### Task 14: Ratatui browser modal and input routing

**Files:**
- Modify: `src/client/ui/session_browser.rs`
- Modify: `src/client/ui/view.rs`
- Modify: `src/client/app.rs`
- Modify: `src/client/app_tests.rs`

**Interfaces:**
- Consumes: browser state from Task 13 and workspace scoped listing from Task 11.
- Produces: `/sessions` modal, one-second refresh, keyboard/mouse routing, rename editor rendering, and destructive confirmation rendering.

- [ ] **Step 1: Add failing `TestBackend` cell/style assertions**

Render local and global modes at 100x30. Assert a bordered modal overlays but does not erase the background transcript, selected rows use DarkGray/Cyan, current/running/job markers are visible, global group headings are dim, the query cursor is placed, and rename/confirmation layers replace only the modal body.

- [ ] **Step 2: Add failing input/refresh tests**

Script `/sessions`, Tab, filtering text, Up/Down, PageUp/PageDown, wheel events, Enter, F2, Ctrl+D, and nested Escape. Pause Tokio time and assert list RPC runs at open and once per second only while open; refresh failure retains rows and sets a warning.

- [ ] **Step 3: Run focused modal tests**

Run: `cargo test --bin moh session_browser_renders_ -- --nocapture && cargo test --bin moh session_browser_refreshes_only_while_open -- --exact`

Expected: FAIL because browser rendering/input is not wired.

- [ ] **Step 4: Render the modal with Ratatui primitives**

Use `Clear`, bordered `Block`, `Tabs`, `List`, `ListState`, and `Paragraph`. Center within the frame with a one-cell outer margin when possible, preserve minimum-terminal behavior, cap height to the frame, and report the selectable viewport back to browser state. Do not enable click/drag handling.

- [ ] **Step 5: Give browser layers input precedence**

When open, route resize and Ctrl+C globally, then browser input before help/menu/transcript/editor shortcuts. Tab toggles mode and triggers immediate scoped refresh. F2 and Ctrl+D enter nested layers. Mouse wheel scrolls browser rows rather than transcript. Enter on List returns a switch action; Enter on Rename submits title; confirmation accepts `y`/Enter and rejects `n`/Escape.

- [ ] **Step 6: Add one-second refresh selection to the event loop**

Create a paused interval only while browser state is open. Select among terminal events, current-session observer updates, and refresh ticks. Apply session updates to the background projection while the modal stays open.

- [ ] **Step 7: Run binary TUI tests**

Run: `cargo test --bin moh`

Expected: PASS.

- [ ] **Step 8: Commit browser presentation**

```bash
git add src/client/ui/session_browser.rs src/client/ui/view.rs src/client/app.rs src/client/app_tests.rs
git commit -m "feat(tui): render session browser dialog"
```

---

### Task 15: Switch, rename, and delete browser actions

**Files:**
- Modify: `src/client/app.rs`
- Modify: `src/client/workspace.rs`
- Modify: `src/client/ui/session_browser.rs`
- Modify: `src/client/app_tests.rs`
- Modify: `tests/client_server.rs`

**Interfaces:**
- Consumes: complete workspace, browser, RPC, manager, and actor interfaces.
- Produces: end-to-end switch/rename/delete behavior and deterministic current-session fallback.

- [ ] **Step 1: Add failing application flow tests**

Script and assert:

```text
switch_opens_target_then_detaches_old_without_cancel
switch_failure_keeps_old_chat_and_browser_open
rename_updates_row_and_current_status_without_closing_browser
rename_error_preserves_inline_text
delete_other_session_keeps_browser_open
delete_current_closes_browser_and_selects_latest_running_local_session
delete_current_with_no_running_session_shows_draft
remote_delete_applies_the_same_fallback
delete_failure_keeps_row_and_reports_no_success
```

Verify confirmation text contains title, stable ID, cancellation, jobs, and disconnected clients.

- [ ] **Step 2: Run focused flow tests**

Run: `cargo test --bin moh switch_opens_target_then_detaches_old_without_cancel -- --exact && cargo test --bin moh delete_current_ -- --nocapture`

Expected: FAIL because modal actions are not connected to workspace mutations.

- [ ] **Step 3: Wire switch action**

Call `WorkspaceClient::switch_session(id)`. On success replace the background `ChatProjection`, update active project, call `UiState::authoritative_reset`, close browser, and follow the new transcript. On failure keep browser/query/selection and push a browser warning.

- [ ] **Step 4: Wire rename action**

Parse `SessionTitle` before RPC. On success refresh immediately, retain selected ID, close only the rename layer, and apply the authoritative title event/snapshot to current status. On domain/RPC error retain editor value and display its sanitized error inside the rename layer.

- [ ] **Step 5: Wire confirmed deletion**

Call backend delete by stable ID. For a noncurrent ID, remove stale row and refresh while keeping the modal open. For current ID, close modal and run workspace startup fallback using the deleted session CWD. Handle remote `WorkspaceUpdate::Deleted` identically and idempotently. If deletion fails for the current ID, have the workspace controller reopen that stable ID before reporting the error so the retained session remains immediately usable even though its cancelled run/jobs do not restart.

- [ ] **Step 6: Add cross-process acceptance tests**

In `tests/client_server.rs`, run two clients against one backend. Leave a fake model run active, detach/switch the first client, complete it, and assert reattachment sees the completed transcript. Delete a second client's current session and assert its next update is typed deletion followed by fallback, not generic connection loss.

- [ ] **Step 7: Run client and server integration suites**

Run: `cargo test --bin moh && cargo test --test client_server && cargo test --test rpc_transport`

Expected: PASS.

- [ ] **Step 8: Commit interactive lifecycle actions**

```bash
git add src/client/app.rs src/client/workspace.rs src/client/ui/session_browser.rs src/client/app_tests.rs tests/client_server.rs
git commit -m "feat(tui): manage sessions from the browser"
```

---

### Task 16: Documentation, regression gate, and PTY acceptance

**Files:**
- Modify: `README.md`
- Modify as failures require: only files already owned by Tasks 1-15

**Interfaces:**
- Consumes: the complete implementation.
- Produces: user-facing command/session documentation and final verification evidence.

- [ ] **Step 1: Update README session and TUI documentation**

Document exact CLI forms, ephemeral startup/new semantics, first-message persistence, AI/fallback/manual titles, `/sessions` local/global controls, switching/detachment, durable failed/cancelled/interrupted transcript, permanent delete confirmation, and the process-local limit for active runs/jobs. Remove default-session and named-creation wording.

- [ ] **Step 2: Run format and whitespace checks**

Run: `cargo fmt --all -- --check && git diff --check`

Expected: PASS with no output.

- [ ] **Step 3: Run Clippy with warnings denied**

Run: `cargo clippy --all-targets --all-features -- -D warnings`

Expected: PASS with no warnings.

- [ ] **Step 4: Run every target test**

Run: `cargo test --all-targets`

Expected: PASS across library, binary, integration, transport, migration, and Ratatui tests.

- [ ] **Step 5: Build from the lockfile**

Run: `cargo build --locked`

Expected: PASS without changing `Cargo.lock`.

- [ ] **Step 6: Run real PTY acceptance**

Using a private temporary XDG state/runtime/config directory and a fake or test backend, verify alternate-screen restoration, local/global browser modes, filtering, wheel scrolling, resize/reflow, `/new` invisibility in browser/database, switching away from active run/job, later transcript recovery, rename, active deletion confirmation, remote deletion fallback, and terminal restoration. Record which checks used fakes and leave live Codex title generation explicitly unqualified unless a configured account is intentionally used.

- [ ] **Step 7: Review final diff and commit documentation**

Run: `git status --short && git diff --stat && git diff --check`

Expected: only `README.md` plus any explicitly repaired Task 1-15 paths are modified and whitespace checks pass.

```bash
git add README.md
git commit -m "docs: document interactive session management"
```

- [ ] **Step 8: Verify the branch is clean and report evidence**

Run: `git status --short --branch && git log --oneline --decorate -17`

Expected: clean `feat/session-management` with the plan's conventional commits at the tip. Report automated checks separately from PTY and live-network qualification.
