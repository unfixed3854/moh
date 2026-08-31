# Async Production Operations Design

## Goal

Move Moh's production blocking-operation boundaries beneath direct async APIs.
The `read`, `write`, and `edit` services, durable anchor persistence, and Codex
credential persistence will be awaitable by their callers. Terminal I/O and
test-only synchronous infrastructure are out of scope.

## Context

Moh already drives agent runs and Codex HTTP/SSE transport asynchronously on a
Tokio current-thread runtime. The three Rig tool adapters currently offload
entire synchronous services with `tokio::task::spawn_blocking`; credential
refresh creates a nested Tokio runtime inside a blocking task so it can combine
synchronous file access and async HTTP. SQLite-backed anchors, advisory
credential locking, and atomic replacement also use synchronous APIs.

The migration must not weaken the read-before-write/edit rule, stale-read and
stale-anchor detection, atomic replacement, permission preservation, durable
anchor behavior, credential rotation revalidation, lock timeout, or existing
model-visible error categories.

## Architecture

### Async service boundaries

`ReadService::read`, `WriteService::write`, and `EditService::edit` become
async public methods. Their factories remain synchronous constructors because
they only compose in-memory state.

Each service submits one complete filesystem-critical operation to Tokio's
blocking pool and awaits its result. This keeps blocking work off the
current-thread executor without introducing an await between related security
and correctness checks. In particular, a write or edit continues to re-read
the observed checksum immediately before it atomically replaces the target.

`FileObservations` remains synchronous, in-memory shared state. Its mutex is
never held across an await.

### Durable anchor storage

`AnchorStore` exposes async internal operations for opening, loading, and
updating anchor rows. Their SQLite work runs on the blocking pool and retains
the existing serialized-connection, transaction, and busy-retry behavior.

Read-service initialization remains lazy. Initialization failure is memoized
as it is today so callers receive `E_STORE` rather than unstable anchors.

### Rig adapters

`RigReadTool`, `RigWriteTool`, and `RigEditTool` become thin adapters: each
awaits its respective service directly. They no longer own generic
`spawn_blocking` wrappers.

Blocking-worker failure remains distinguishable from tool-domain failure so
the adapters can retain their existing model-visible runtime error code and
not turn an infrastructure failure into an ordinary file-access error.

### Codex authentication

Credential refresh remains a single async operation. Reqwest request and
response handling run on the async executor. Blocking jobs are limited to:

- acquiring and retaining the companion advisory lock;
- reading and parsing the auth document before and after the OAuth exchange;
- atomically persisting a validated rotated document with its existing
  permission and durability guarantees.

The acquired lock is retained through the refresh request and revalidation,
then released after persistence or failure. No nested Tokio runtime is
created. The existing five-second lock timeout and thirty-second request bounds
are unchanged.

Auth-file loading used at startup also becomes async so production callers do
not synchronously read credentials on the runtime thread.

## Data flow

```text
Rig PortableTool::call
        |
        v
async Read/Write/Edit service
        |
        v
private blocking critical section --> filesystem / SQLite
        |
        v
typed domain or worker result --> existing Rig error projection

Codex refresh
  blocking lock/read --> async OAuth HTTP --> blocking re-read/persist
```

The blocking pool is an implementation boundary, not a public API. Callers use
the async service methods and do not need to know whether a particular
operation uses Tokio filesystem primitives or synchronous primitives in an
isolated critical section.

## Errors and cancellation

Existing tool-domain errors and error messages remain unchanged. A failed
blocking task remains a distinct runtime-infrastructure failure and maps to
the current Rig runtime error code. SQLite and filesystem failures retain their
current domain classification.

Dropping an awaiting tool future stops waiting for its result but cannot
unsafely interrupt an already-started atomic filesystem or SQLite operation.
This is the same practical cancellation model as today's adapter-owned
blocking jobs. Auth refresh continues to preserve a dispatched credential
rotation through the runtime's existing shutdown behavior.

## Testing and validation

Tests will await service methods directly and preserve coverage for:

- read formatting and durable anchors;
- stale-read, stale-anchor, and read-before-write/edit rejection;
- newline, BOM, permissions, and atomic replacement behavior;
- SQLite busy retry and durable-store failure mapping;
- non-stalling behavior of a blocking service operation on the current-thread
  executor;
- credential lock contention, refresh/retry behavior, concurrent rotation,
  cancellation, and atomic persistence.

Rig runtime tests will continue to verify model-visible error projection and
tool-loop behavior, but no longer test that Rig itself owns the blocking pool.

Run the repository's standard gates after implementation:

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --locked
```

## Non-goals

- Changing terminal I/O or input event handling.
- Replacing test mock servers or test synchronization primitives.
- Adding concurrent harness runs or persistence for conversation history.
- Replacing SQLite or changing its durable-anchor schema.
- Changing tool schemas, tool descriptions, model defaults, or Codex protocol
  behavior.
