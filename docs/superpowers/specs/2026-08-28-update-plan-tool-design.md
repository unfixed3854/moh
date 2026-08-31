# Update Plan Tool Design

## Summary

Add one model-facing `update_plan` tool that maintains a small, ordered,
session-scoped execution plan. The plan is durable application state, is
included in authoritative session snapshots, and is rendered by the terminal
client. It is not conversation history and is never written into the project
working tree.

This milestone deliberately does not add task CRUD, dependencies, ownership,
subagent coordination, or plan-mode permission controls.

## Goals

- Give the model an atomic whole-list plan update suitable for multi-step work.
- Keep plan state visible to attached clients while a run is active.
- Preserve plan state through context compaction, client detachment, backend
  restart, failed runs, and cancelled runs.
- Make the current plan available to every new model run without requiring a
  separate read tool.
- Preserve `SessionSnapshot` as the sole authoritative client projection.
- Provide compact progress in the status row and a complete read-only popup.

## Non-goals

- Stable task identifiers or granular task mutation.
- Nested tasks, priorities, dependencies, owners, scheduling, or subagents.
- A user-editable task list.
- `enter_plan_mode`, `exit_plan_mode`, or any other permission mode.
- Repository files such as `TODO.md` or plan export.
- Compatibility aliases such as `todowrite`, `plan_update`, or `task_update`.

## Domain model

`PlanStatus` has five values:

- `pending`;
- `in_progress`;
- `completed`;
- `blocked`; and
- `cancelled`.

`PlanItem` contains `step: String` and `status: PlanStatus`. A plan is an
ordered `Vec<PlanItem>` owned by one session. Order is meaningful and is
preserved exactly.

The complete durable `SessionRecord` and transport-facing `SessionSnapshot`
contain the current plan. `SessionProjection` reduces `PlanChanged` events by
replacing its complete plan. The client performs the same reduction; it never
infers plan state from transcript tool arguments.

Plans remain present after successful completion, run failure, or explicit
cancellation. Only another successful `update_plan` call can replace or clear
the plan. Passing an empty list clears it.

## Tool contract

The runtime registers one strict Rig tool named `update_plan`:

```json
{
  "explanation": "Optional reason for changing the plan",
  "plan": [
    {
      "step": "Run the focused tests",
      "status": "in_progress"
    }
  ]
}
```

`plan` is required. `explanation` is optional, model-visible context for the
current update; it is not stored as plan state.

Validation applies to the complete request before mutation:

- the plan contains at most 32 items;
- every step contains 1 through 256 Unicode scalar values;
- every step is already trimmed and contains no control characters;
- every status is one of the five declared values;
- at most one item is `in_progress`; and
- unknown object properties are rejected by the generated schema.

Exact duplicate step text is allowed because two ordered phases can
legitimately share a short label. The tool does not impose status-transition
rules: a whole-list replacement may revise the plan as work changes.

Invalid input rejects the complete update without mutation. A successful tool
result returns the canonical complete plan and compact counts so the model can
confirm the accepted state during the current run.

## Runtime and actor ownership

The session actor remains the sole owner of plan mutation, sequencing,
persistence, and client broadcasts.

Each per-session engine receives a cloneable plan-tool client backed by an
internal channel. The actor owns the receiver. When the Rig adapter invokes
`update_plan`, it validates the typed arguments, sends a replacement request,
and waits on a one-shot response.

The actor loop selects plan requests alongside client commands, harness events,
and job changes. This remains live while the model run is waiting for the tool,
so the request cannot deadlock behind `Harness::next_event`. For an accepted
request the actor:

1. replaces the plan in its durable record;
2. reduces a sequenced `SessionEvent::PlanChanged`;
3. attempts a durable checkpoint;
4. broadcasts the plan event and any persistence-warning transition; and
5. replies to the waiting tool with the canonical state and persistence
   outcome.

The tool infrastructure reports a closed request channel or dropped response
as a stable runtime error. Domain validation errors remain model-recoverable
tool errors and do not terminate the run.

## Persistence

The SQLite session schema advances from version 1 to version 2. Version 2 adds
an ordered plan-item table keyed by session ID and item position. Status is
stored as its canonical lowercase name. The session foreign key cascades on
deletion.

Opening a version 1 database migrates it transactionally to version 2 without
changing existing session or message data. New and migrated sessions begin
with an empty plan. Loading validates stored positions, text, status, and the
single-`in_progress` invariant; malformed durable data produces the existing
invalid-stored-data error family rather than silently repairing it.

Session checkpoints replace the stored plan in the same transaction as the
session metadata and committed history. Clearing a plan therefore deletes all
of that session's plan rows atomically.

Persistence failure follows Moh's existing recoverable warning behavior. The
new plan remains authoritative in memory, the record becomes dirty, attached
clients receive the plan followed by a persistence warning, and the tool result
states that durability is pending. A later flush, successful checkpoint, or
shutdown retries the complete record. Validation or channel failure changes
neither live nor durable state.

## Model context

`RunContext` gains the current ordered plan. The actor snapshots it when a new
run is accepted. The Rig runtime appends a compact generated section to the
per-run preamble when the plan is non-empty. The section includes every step
and canonical status and tells the model to use `update_plan` to revise it.

This side-channel state is independent of committed user/assistant history, so
it survives compaction and does not require `get_plan`. An active run sees the
plan snapshot from submission plus the canonical results of its own later tool
updates. Concurrent runs do not exist within one Moh session.

## RPC and events

The Cap'n Proto schema adds `PlanStatus`, `PlanItem`, the plan list on
`SessionSnapshot`, and a `planChanged` branch on `EventEnvelope`. Existing field
ordinals remain unchanged; new fields and union branches use new ordinals.

Conversion code validates enum values and delegates stored text invariants to
the domain constructors. Snapshot and event round trips preserve ordering and
all five statuses.

`SessionEvent::PlanChanged(Vec<PlanItem>)` is independent of a run ID. The
actor may broadcast it while a run is active, and reconnecting clients receive
the same state in the attachment snapshot.

## Terminal client

The one-line status bar adds a compact plan segment only when the plan is
non-empty: `plan C/T`, where `C` counts completed items and `T` counts all
non-cancelled items. Cancelled items remain visible in the popup but do not
inflate progress. Ratatui may clip the right side normally on narrow terminals;
the plan segment must not displace the model, effort, context, or lifecycle
state segments.

Ctrl+T toggles a centered, bordered, read-only Plan popup while idle or busy.
The popup shows every item in order with distinct markers/styles, highlights
`in_progress`, and visibly distinguishes `blocked` and `cancelled`. Empty plans
show a neutral `No active plan` message.

Help and Plan become variants of one mutually exclusive popup state. Opening
either closes an open selector and replaces the other popup. Escape closes the
active popup before it can cancel a run. Ctrl+O continues to open Help. Plan
content updates immediately when a `PlanChanged` event arrives while the popup
is open.

The help popup documents Ctrl+T. The status bar and popup sanitize all
model-provided text before rendering.

## Errors and observability

The tool distinguishes:

- invalid arguments, reported with a stable argument error and no mutation;
- unavailable actor state or a closed internal channel, reported as a stable
  runtime error;
- a persistence warning after accepted live mutation, reported as successful
  live state with durability pending; and
- malformed stored data during session load, reported by the session-store
  boundary.

The existing `ToolStarted` and `ToolFinished` events remain sufficient for the
transcript. The Plan popup and status row are driven only by authoritative
snapshot and `PlanChanged` state.

## Testing

Domain and tool tests cover:

- all statuses and schema shape;
- empty clearing and the 32-item boundary;
- scalar-count, trimming, control-character, and active-item validation;
- atomic rejection without mutation;
- canonical output and closed-channel errors; and
- tool registration and model-visible descriptions.

Store tests cover:

- fresh version 2 creation;
- transactional version 1 migration;
- ordered round trips for all statuses;
- atomic replacement and clearing;
- cascade deletion;
- malformed stored rows; and
- dirty-checkpoint retry after persistence failure.

Actor and harness tests cover:

- handling a plan request while a run awaits its tool result;
- event sequence and observer broadcast ordering;
- snapshots before and after reattachment;
- completed, failed, and cancelled runs retaining the plan;
- backend restart restoration;
- persistence-warning transitions; and
- next-run plan injection without a read tool.

RPC tests cover snapshot and event round trips, invalid status values, and
ordered plans.

TUI tests assert visible cells and styles for status counts, each plan state,
the empty popup, live replacement, Ctrl+T toggling, Ctrl+O replacement, selector
closure, Escape precedence, narrow layouts, and minimum terminal dimensions.

Final automated verification is:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --locked
git diff --check
```

PTY acceptance verifies live plan updates during a run, Ctrl+T and Escape,
detach and reattach, backend restart persistence, resize behavior, and original
terminal restoration. Paid model behavior that cannot be exercised locally is
reported separately from deterministic PTY and automated evidence.
