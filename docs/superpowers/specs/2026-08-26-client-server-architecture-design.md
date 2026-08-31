# Client-Server Architecture and Basic Session Persistence Design

## Goal

Separate Moh's terminal frontend from a long-lived local backend using Cap'n
Proto RPC. The backend owns agent execution and session state so closing a
client does not interrupt an active agent. A single backend hosts multiple
independent sessions, including multiple sessions in the same working
directory, and exits automatically after a configurable period with no
clients or live work.

Add basic session persistence as part of the boundary. Successful conversation
turns and session settings survive a normal backend shutdown and restart;
provisional run state and background processes do not.

## Context

Moh currently composes `CodexRunEngine`, `Harness`, `JobRegistry`, and the TUI
inside one process. `Harness` already provides a model-neutral boundary and
owns successful-only history plus one active run. The binary application owns
the terminal loop and projects harness events into presentation. Background
jobs deliberately outlive the tool call that starts them, but application exit
currently cancels the active run and shuts every job down.

This ownership prevents a run from surviving terminal-client exit and makes a
second frontend reuse presentation code rather than a stable application
protocol. Issue #26 reverses that boundary: the backend becomes authoritative,
while terminal, web, native, and editor clients become projections and command
sources.

The Rust Cap'n Proto RPC implementation provides a two-party client/server vat
network over a bidirectional byte stream. Its `RpcSystem` is not `Send`, so the
backend drives connection RPC systems and session actors on a Tokio `LocalSet`.
Existing blocking filesystem and SQLite operations remain outside the async
runtime thread.

## Decisions

- One global backend process serves the current operating-system user.
- The first transport is a permission-restricted Unix-domain socket. Windows
  transport is deferred, but the application protocol does not depend on Unix
  socket types.
- A session has a stable generated ID. Canonical working directory is session
  metadata and the default lookup key, not the session's identity.
- One default session exists per canonical working directory. Additional
  independent sessions may use the same directory.
- Multiple clients may attach to the same session with equal control.
- Each session allows one active agent run. Different sessions may run agents
  concurrently.
- The backend broadcasts numbered live events through client-supplied Cap'n
  Proto observer capabilities. Reattachment uses a fresh authoritative
  snapshot, not historical callback replay.
- A 15-minute idle timeout is the default and is configurable in Moh's TOML
  configuration file.
- Automatic shutdown is allowed only with no connected clients, no active
  agent runs, and no running background jobs.
- Basic persistence stores successful conversation turns and session settings.
  Failed, cancelled, partial, and tool-activity records remain process-local.
- Closing or losing a client connection detaches it and never cancels a run.
  Cancellation is a separate explicit session command.

## Scope

This milestone includes:

- Cap'n Proto schema and checked-in Rust bindings;
- Unix socket discovery, safe automatic backend spawning, and stale endpoint
  recovery;
- protocol version negotiation;
- a global backend and lazily loaded multi-session registry;
- automatic default sessions plus explicit creation and attachment;
- simultaneous clients and observer event delivery;
- backend-owned harness, run, model-setting, job, and tool-observation state;
- a client-owned TUI projection reconstructed from session snapshots;
- configurable inactivity shutdown;
- SQLite persistence for committed history and session settings;
- CLI surfaces for creating, selecting, and listing sessions;
- detach and explicit-cancel controls;
- boundary, integration, subprocess, persistence, and lifecycle tests.

## Non-goals

The milestone does not include:

- TCP, remote access, TLS, or cross-user access;
- Windows named-pipe support;
- restoring an active model run or background process after backend death;
- durable provisional text, tool activity, failed prompts, or cancelled prompts;
- session deletion, rename, pruning, export, or a session-management TUI;
- web, native GUI, or editor-extension implementations;
- multiple concurrent runs inside one session;
- multi-agent coordination inside a session;
- a generic provider plugin protocol;
- configuration reload while the backend is running;
- live backend handoff during executable updates.

## User and CLI Model

### Default session

Running `moh` canonicalizes the current working directory, connects to the
global backend, and attaches to that directory's default session. If the
default session does not exist, the backend creates it. The mapping is durable,
so a later backend instance selects the same session.

### Additional sessions

`moh --new` creates and attaches a new independent session in the current
directory. `moh --new NAME` also assigns an immutable optional name for this
milestone. Names are unique within a canonical working directory and cannot
use the generated-ID namespace.

`moh --session SELECTOR` attaches by globally unambiguous generated ID or by an
exact name scoped to the current directory. ID lookup takes precedence over
name lookup. Attaching by ID uses the session's persisted working directory,
even when the command is invoked elsewhere; the client displays that directory
before accepting input.

`moh sessions` prints the sessions for the canonical current directory without
starting the full-screen TUI. Each row includes the ID, optional name, default
marker, running/idle state, attached-client count, and last activity. It uses
the backend as the authority and therefore performs the same connect-or-spawn
flow as the interactive client.

### Multiple clients

Any number of clients may attach to one session. All receive the same ordered
events and may submit commands. The session actor serializes those commands.
Only the first submission while idle starts a run; later submissions receive a
typed `busy` result. Model and reasoning changes are last-writer-wins in actor
order and are broadcast to every client.

## Process Architecture

### Executable roles

The `moh` executable has two composition roots:

1. The default client role resolves the endpoint, connects or spawns, performs
   the protocol handshake, attaches to a session, and then starts terminal
   management.
2. The `moh server` role loads server configuration and persistence, binds the
   endpoint, accepts Cap'n Proto connections, hosts session actors, and owns
   shutdown.

A manually invoked `moh server` stays in the foreground for diagnosis.
Automatic spawning invokes an internal detached form with stdin disconnected,
a new Unix session, and stdout/stderr redirected to a diagnostic log in Moh's
state directory.

### Dependency direction

The conceptual dependency graph is:

~~~text
terminal client -> RPC client -> Cap'n Proto schema <- RPC server
                                                    |
                                              SessionManager
                                                    |
                                      Harness -> Rig -> Codex/tools
~~~

The terminal client does not construct or import `Harness`,
`CodexRunEngine`, `JobRegistry`, credential types, or persistence
implementations. The reusable `tui` module remains unaware of RPC and harness
types.

### Runtime model

The backend retains a current-thread Tokio runtime and adds a `LocalSet`.
Every accepted socket receives its own two-party `RpcSystem`, driven by a local
task. Session actors also run locally and communicate through bounded command
channels. Provider, tool, socket, and timer futures must never block that
thread. Existing synchronous filesystem and SQLite work continues through
`spawn_blocking` boundaries.

The terminal client may use its own current-thread runtime. It selects between
terminal input and RPC observer events instead of terminal input and
`Harness::next_event()`.

## Endpoint Discovery and Automatic Spawn

### Runtime paths and permissions

The endpoint resolver prefers a private directory under `XDG_RUNTIME_DIR`.
When no usable runtime directory exists, Unix platforms use a deterministic
owner-only directory below the system temporary directory that includes the
effective user ID. The resolver never derives the socket path from the current
working directory, avoiding Unix socket length limits for deep project paths.

The runtime directory is created with mode `0700`, and the socket is restricted
to its owner. Existing paths are rejected unless they have the expected owner
and file type. The local security model trusts processes running as the same
OS user; session IDs are identifiers, not authorization secrets.

### Connect-or-spawn sequence

The client uses this sequence:

1. Try to connect and complete the version handshake.
2. On endpoint absence or connection refusal, acquire an advisory startup lock
   at one exact resolved path.
3. Recheck the endpoint after acquiring the lock.
4. If it still cannot connect, validate and remove only an owner-matching stale
   socket at the resolved endpoint.
5. Spawn the detached backend and wait with a bounded deadline for a successful
   handshake.
6. Release the startup lock and continue as an ordinary client.

Racing clients block at the startup lock and recheck, so one backend wins.
Racing manually started servers are resolved by socket bind: the loser reports
that a backend is already running and exits. Spawn failures report the endpoint,
lock, and diagnostic-log paths without exposing environment secrets.

The backend binds and starts accepting before attempting model-catalog network
requests. A slow or unavailable catalog therefore does not make endpoint
startup look like a dead daemon.

## Backend and Session Ownership

### Global backend state

The backend owns:

- the listening endpoint and connected-RPC registry;
- protocol and backend-instance metadata;
- parsed server configuration;
- the session store;
- a lazy `SessionManager`;
- shared Codex authentication and model transport;
- shared durable read-anchor storage;
- model-catalog loading state;
- global activity and shutdown coordination.

Persisted sessions are not all instantiated at startup. `listSessions` combines
store summaries with live actor summaries. Opening or creating a session loads
or creates its actor on demand.

### Per-session state

Each session actor exclusively owns:

- stable ID, optional name, canonical CWD, and default marker;
- `Harness<CodexRunEngine>` and one optional active run;
- selected model and reasoning effort;
- latest context usage;
- committed conversation history;
- process-local transcript projection and active partial response;
- observer registrations and event sequence;
- an isolated `JobRegistry` and job services;
- isolated in-memory file observations used to authorize writes and edits;
- persistence dirty/error state.

Different sessions must not enumerate, wait for, or cancel each other's jobs.
A read in one session must not authorize a write or edit in another session,
even when both sessions use the same CWD. Durable line-anchor snapshots may be
shared because they describe files rather than conversation authority.

`CodexRunEngine` construction is refactored accordingly: provider/auth transport
and durable anchors can be shared, while active model, reasoning, jobs, and
file observations are created per session.

### In-memory projection

The actor maintains a presentation-neutral session projection. It is seeded
from persisted committed turns and then records live submitted prompts,
assistant deltas, tool starts, terminal events, settings, context usage, and
job summaries. Snapshots taken during the same backend lifetime can therefore
restore an active run and its tool activity.

Only the committed subset is durable. After a backend restart the projection
is reconstructed from successful user/assistant turns and stored settings;
process-local failed, cancelled, partial, and tool-activity records are absent.

## Cap'n Proto Protocol

### Schema and generated code

The source schema lives at `schema/moh.capnp`. Generated Rust bindings are
checked into the repository so normal `cargo build --locked` continues to
require only the Rust toolchain. A repository script regenerates the bindings
with a documented Cap'n Proto compiler version. Generated files are never
edited by hand.

The initial handshake returns a protocol major/minor version, backend instance
identifier, and startup warnings. Major-version mismatch is rejected before a
session is opened. Minor versions are reserved for additive schema evolution;
the first milestone requires all methods it calls to be advertised by the
server.

### Bootstrap interface

The bootstrap `Backend` capability provides these conceptual operations:

~~~text
getInfo() -> ProtocolInfo
openDefault(cwdBytes, observer) -> Session, SessionSnapshot
createSession(cwdBytes, optionalName, observer) -> Session, SessionSnapshot
openSession(selector, cwdBytesForNameLookup, observer) -> Session, SessionSnapshot
listSessions(cwdBytes) -> List(SessionSummary)
~~~

Unix paths cross the protocol as `Data`, not `Text`, so non-UTF-8 working
directories remain representable. Snapshots and summaries also contain a
lossy display string for non-Rust clients and presentation.

The connection's bootstrap implementation assigns a connection ID. Every
observer attachment is associated with that ID. When its `RpcSystem` ends,
the backend removes all corresponding observers immediately; it does not wait
for a future event callback to discover the disconnect.

### Session interface

The returned `Session` capability provides:

~~~text
submit(prompt) -> SubmitResult
cancel() -> CommandResult
selectModel(modelId) -> CommandResult
selectReasoning(level) -> CommandResult
listJobs() -> List(JobSnapshot)
cancelJob(jobId) -> JobResult
~~~

Domain outcomes use explicit result unions and stable error codes such as
`busy`, `notRunning`, `sessionNotFound`, `modelNotFound`,
`unsupportedReasoning`, and `jobNotFound`. Concise sanitized messages accompany
codes for display. Cap'n Proto exceptions are reserved for connection, framing,
and internal protocol failures.

### Snapshot and attachment

Attachment is one actor command. The actor registers the observer and returns
an authoritative `SessionSnapshot` containing:

- identity, name, CWD, and default status;
- the current presentation-neutral transcript;
- active prompt, accumulated assistant text, and active run ID when present;
- selected model and reasoning effort;
- context usage and model-catalog state;
- busy state and job snapshots;
- persistence warning state;
- the last included event sequence.

The observer is registered before the snapshot response is released. A client
may therefore receive a callback while the response is in flight; it buffers
callbacks, installs the snapshot, discards sequences already included, and
then applies later events in order.

### Observer delivery

Every session state change that clients need is represented by an event
envelope with a checked monotonically increasing sequence number and a typed
event union. Events cover run lifecycle, assistant deltas, tool activity,
context usage, model/reasoning changes, job changes, and persistence warnings.

Each observer has a bounded outbound queue and a dedicated delivery task.
The actor only enqueues; it never awaits a client callback. Queue overflow,
callback failure, or sequence exhaustion detaches that observer and leaves the
session and other clients running.

Callbacks are not a durable event log. A client that sees a sequence gap or
establishes a new connection attaches again and replaces its local projection
with a fresh snapshot. Event sequences are scoped to the live session actor
and may restart after backend restart because the snapshot is authoritative.

## Run and Command Data Flow

### Submission

1. A client sends `submit` through its `Session` capability.
2. The actor rejects the command with `busy` if its harness is running.
3. Otherwise it calls `Harness::submit` with the session CWD, updates its
   in-memory projection, and returns the started run ID.
4. The actor selects between mailbox commands and `Harness::next_event()`.
5. Run events update the authoritative projection and are broadcast to every
   observer.
6. The actor continues polling the harness when zero clients are attached.
7. A successful terminal event attempts to checkpoint committed history and
   metadata, then broadcasts completion in every case. A checkpoint failure
   marks the session dirty and immediately broadcasts a persistence warning.
   A failed or cancelled run is broadcast but not added to durable model
   history.

### Cancellation and detachment

Disconnect, Ctrl+C client exit, `/quit`, and terminal loss remove the client
without sending `Harness::cancel`. The actor continues its run and jobs.

Escape during an active run and `/cancel` send the explicit `cancel` RPC. The
actor calls `Harness::cancel`, updates every client, and remains available for
the next submission. Cancelling from any attached client affects the shared
session because clients have equal control.

### Settings and jobs

Model and reasoning selection move from client-held shared handles to session
actor commands. Successful changes are persisted and broadcast. Model catalog
loading is backend-global, but catalog availability or failure is projected to
clients without preventing use of the configured default model.

`/ps` and `/kill` use session RPC methods rather than direct `JobRegistry`
access. Per-session registry change notifications update the actor even when a
background job settles after its originating run and while no client is
attached.

## Basic Persistence

### Store and schema

A new `sessions.sqlite` database lives in Moh's platform state directory,
separate from the durable anchor database. `rusqlite` remains the storage
implementation. Schema setup and migrations use an explicit version and run
transactionally.

One shared `SessionStore` owns the database connection and serializes its
transactions. Its asynchronous boundary dispatches each operation to a
blocking worker, so concurrent session actors neither contend through separate
connections nor hold a SQLite mutex on the local async executor.

The logical schema contains:

- `sessions`: stable ID, optional name, canonical CWD blob, default marker,
  model, reasoning, context usage, creation time, and last activity;
- `messages`: session ID, ordered position, role, and text.

Constraints enforce one default session per canonical CWD, unique names within
a CWD, stable message order, and referential cleanup. Session names and CWDs
are immutable in this milestone.

### Commit semantics

On successful run completion, the harness has atomically added the user prompt
and assistant response to its in-memory history. The session actor attempts to
write both new messages plus current metadata in one SQLite transaction before
broadcasting `Completed`. When the transaction succeeds, the completion is
durable. When it fails, the actor still broadcasts `Completed`, immediately
follows it with a persistence warning, and retains an idempotent dirty
checkpoint for retry. Model/reasoning changes and context usage update the
session row without rewriting message history and use the same dirty-checkpoint
behavior on store failure.

No provisional delta, tool call, failed prompt, cancelled prompt, job, or
observer record is written. On restart, `Harness::with_history` receives only
the stored successful messages.

### Persistence failure

A model run is not reclassified as failed after producing a valid answer just
because its checkpoint fails. The answer remains committed in the live
harness, the session becomes dirty, attached clients receive a persistence
warning, and later persistence operations retry an idempotent full checkpoint.

Automatic idle shutdown retries every dirty checkpoint. If any live committed
state still cannot be persisted, automatic shutdown is aborted and retried
later rather than deliberately discarding the state. Explicit process
termination performs a bounded best-effort flush and reports failure in the
diagnostic log.

Database corruption follows the established anchor-store recovery pattern:
the corrupt file is moved to a timestamped quarantine path, a fresh schema is
created, the diagnostic log records the path, and the next client receives a
startup warning. No quarantined data is silently presented as restored.

## Configuration

Moh reads an optional TOML file from its platform configuration directory. On
Linux this is normally `$XDG_CONFIG_HOME/moh/config.toml` or
`~/.config/moh/config.toml`.

The initial user-facing setting is:

~~~toml
[server]
idle_timeout = "15m"
~~~

The default is 15 minutes. The duration must be positive and representable by
`std::time::Duration`; human-readable units such as seconds, minutes, and hours
are accepted. Unknown keys, malformed TOML, and invalid durations are startup
errors with the config path and field, but not the file's unrelated contents.

Configuration is read once when the backend starts. Tests inject an ordinary
`ServerConfig` with short durations rather than changing global configuration
or sleeping in wall-clock time.

## Inactivity and Shutdown

The global lifecycle coordinator tracks:

- live RPC connections, including connections that have not attached yet;
- active harness runs across instantiated sessions;
- running jobs across per-session registries;
- an activity generation used to invalidate stale timers.

The idle timer exists only when all three counts are zero. Any new connection,
run, or job cancels the current timer and advances the generation. Disconnect,
run completion, and job settlement recompute eligibility. Persisted but
unloaded sessions do not keep the process alive.

When the configured deadline fires, the coordinator serially rechecks the
generation and all counts before closing the listener. Shutdown then:

1. checkpoints dirty sessions, aborting automatic shutdown on failure;
2. invokes every instantiated session's job-registry shutdown to clean retained
   details and reject late starts;
3. drains already-dispatched credential refresh persistence;
4. closes RPC systems and the store;
5. removes the exact owned socket path;
6. exits successfully.

A connection racing the deadline is counted before session attachment and
therefore vetoes shutdown. No active run or background job is cancelled by
automatic inactivity shutdown.

## Terminal Client Integration

The current application loop is retained as presentation logic but is driven
by a `SessionClient` boundary rather than generic over `RunEngine`. After
attachment it builds the transcript, status, live response, model state, and
job count from the snapshot. It then selects between terminal input and
observer events.

Help, input editing, fuzzy model selection, overlays, rendering, terminal
sanitation, and terminal restoration stay client-local. Model selection,
reasoning selection, submission, cancellation, `/ps`, and `/kill` become RPC
commands. All backend text is passed through the existing terminal-control and
Markdown sanitization boundaries before rendering.

The control changes are intentional:

- Ctrl+C exits and detaches without cancelling;
- `/quit` exits and detaches without cancelling;
- terminal closure or RPC client drop detaches without cancelling;
- Escape during an active run cancels it and keeps the client attached;
- `/cancel` does the same explicitly.

If the RPC connection fails, the client restores the terminal before reporting
the error. The first milestone does not silently respawn a crashed backend from
inside an active TUI; rerunning `moh` performs normal connect-or-spawn and
restores the durable session.

## Error Model and Isolation

- Connection and framing failures end only the affected client connection.
- Slow observers are detached without applying backpressure to the session.
- Ordinary `RunFailure` values terminate only their run and remain available
  for later submissions.
- A session actor invariant failure marks that session unavailable and reports
  a sanitized internal error; it must not stop other session actors.
- A model-catalog failure is visible but does not block configured-model runs.
- Persistence warnings are session-scoped, remain visible in snapshots, and
  clear only after a successful checkpoint.
- Startup and spawn errors include actionable endpoint, config, lock, and log
  paths without serializing credentials, authorization headers, provider
  bodies, or tool output into diagnostics.

## Testing Strategy

### Protocol and actor tests

Use fake engines, observers, and stores to cover:

- default-session creation and stable reattachment;
- two independent sessions sharing one CWD;
- one-run-per-session with concurrent runs across sessions;
- per-session model, reasoning, job, and file-observation isolation;
- simultaneous observers receiving identical ordered events;
- atomic snapshot/observer attachment and in-flight callback buffering;
- observer failure and bounded-queue overflow without run cancellation;
- disconnect without cancellation;
- explicit cancellation from either attached client;
- sequence gaps requiring snapshot replacement;
- actor reuse after completion, failure, and cancellation.

### Unix RPC and subprocess tests

Use temporary runtime/state/config roots and real Unix sockets to cover:

- Cap'n Proto handshake and incompatible protocol rejection;
- connect to an existing backend;
- concurrent connect-or-spawn producing one reachable backend;
- stale socket validation and recovery;
- refusal to remove a wrong-owner or wrong-type endpoint;
- detached client exit while a scripted run continues;
- reattachment snapshot during a run;
- two clients attached to one live session;
- two sessions in one CWD running independently;
- terminal-safe error reporting after connection loss.

Subprocess helpers use injected paths and bounded readiness signals rather than
the developer's real state directory or model credentials.

### Persistence tests

Use temporary SQLite databases to cover:

- schema creation and versioned migration;
- stable default and additional session identities;
- scoped unique names and generated-ID lookup;
- successful user/assistant pair transactionality;
- restored history, model, reasoning, context usage, and last activity;
- exclusion of deltas, tools, failures, cancellations, and jobs;
- dirty checkpoint retry and warning clearance;
- automatic-shutdown veto while dirty state cannot be saved;
- corruption quarantine and startup warning.

### Lifecycle tests

Use Tokio's paused clock or an injected timer to cover:

- the 15-minute production default and parsed overrides;
- no idle timer while a client, run, or job exists;
- timer reset on new activity;
- job completion starting eligibility while detached;
- a connection racing deadline expiry vetoing shutdown;
- clean job-detail, credential-refresh, store, and socket teardown;
- no cancellation of live work by the idle path.

### Client and regression tests

Adapt the current application tests to a fake `SessionClient` and cover:

- snapshot-to-TUI reconstruction;
- observer-event projection;
- Ctrl+C and `/quit` detachment;
- Escape and `/cancel` cancellation;
- RPC-backed model, reasoning, `/ps`, and `/kill` commands;
- visible typed command and persistence errors;
- existing help, resize, streaming, Markdown, status, sanitation, and terminal
  restoration behavior.

Existing harness, provider, Rig runtime, tool, job, renderer, component, and
terminal tests remain in force. The ordinary validation route is:

~~~text
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --locked
git diff --check
~~~

The live Codex test remains opt-in.

## Acceptance Criteria

The milestone is complete when:

- plain `moh` connects to or starts one global backend and opens the durable
  default session for its canonical CWD;
- `--new`, `--session`, and `sessions` support multiple independent sessions,
  including sessions sharing a CWD;
- closing every client does not cancel active runs or jobs;
- a detached session continues processing and can be reattached with a current
  snapshot;
- simultaneous clients observe one authoritative ordered session state;
- session jobs and write/edit observation authority cannot cross session
  boundaries;
- successful turns and settings survive a backend restart while provisional
  work does not;
- the backend exits after the configured idle interval only when it has no
  connections, runs, jobs, or unflushed committed state;
- ordinary builds do not require a system Cap'n Proto compiler;
- Unix endpoint permissions, startup races, and stale recovery are covered;
- the terminal client owns presentation but no harness, runtime, provider, job,
  or persistence state;
- all validation commands pass.
