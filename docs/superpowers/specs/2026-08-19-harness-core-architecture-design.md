# Harness Core Architecture Design

## Goal

Establish a model-neutral harness core before Moh gains more tools, providers,
or frontends. Preserve the current application behavior while reversing the
dependency direction: the session and run lifecycle become the stable center,
and Rig, Codex, tools, and the terminal UI become adapters around it.

This is an evolutionary extraction inside the existing crate. The current
public Rust APIs are allowed to break so the new boundaries do not need
compatibility wrappers.

## Motivation

Moh's individual subsystems are already robust:

- the retained TUI has explicit component, renderer, and terminal boundaries;
- conversation updates commit the user prompt and assistant response together;
- Codex credential refresh is bounded, redacted, cancellation-safe, and atomic;
- the Rig integration runs a complete tool loop with a shared model-call budget;
- the read tool has bounded input, durable anchors, and corruption recovery.

The current composition does not yet provide an equally strong harness
boundary. `conversation` imports provider-owned events, errors, traits, and Rig
messages. `CodexProvider` owns authentication, transport compatibility, the
Rig agent, tool registration, retry behavior, budgets, and UI-shaped activity
events. The binary event loop then decides when to commit or abandon a turn.

This design extracts the orchestration semantics without rewriting the proven
subsystems.

## Scope

The milestone includes:

- a public, model-neutral `moh::harness` module;
- explicit session, run, event, context, and error types;
- one active run per harness;
- successful-only transactional model history;
- generic tool activity events;
- separation of the Rig runtime from the Codex transport and authentication;
- explicit Codex, agent, and read-tool configuration;
- blocking execution for synchronous read and SQLite work;
- migration of the application loop to harness commands and events;
- replacement of the current public conversation and provider APIs;
- boundary-focused tests while preserving current behavior coverage.

The milestone explicitly excludes:

- filesystem authority or path-confinement policy;
- changes to the read tool's current path semantics;
- conversation or event persistence;
- crate or workspace splitting;
- additional providers or tools;
- concurrent runs, multiple sessions, or multi-agent execution;
- a generic provider factory or plugin framework;
- a custom replacement for Rig's agent loop;
- TUI redesign.

## Chosen Approach

Use an evolutionary extraction inside the current crate. Introduce the core
contracts first, move existing behavior behind them, and then migrate the
application. Keep Rig's agent loop and the current Codex compatibility code,
but place them in modules whose dependencies point toward the harness core.

Two alternatives were rejected:

1. Splitting into workspace crates now would enforce compile-time boundaries,
   but the contracts are still being discovered and the split would add
   packaging work without improving current behavior.
2. Continuing with feature-oriented vertical slices would be quicker for the
   next tool, but would deepen the coupling between provider, agent runtime,
   tool activity, history, and presentation.

## Architecture and Dependency Direction

The crate will have these conceptual layers:

1. `moh::harness` is the application core. It owns session history, the active
   run, run identifiers, lifecycle validation, cancellation, terminal outcome
   handling, and public events.
2. `moh::runtime::rig` adapts Rig's agent runtime to the core `RunEngine`
   interface. It owns `AgentBuilder`, tool registration, model-call budgets,
   tool-loop event translation, and the current pre-terminal 401 recovery.
3. `moh::providers::codex` owns Codex credential handling, HTTP request
   construction, SSE compatibility, and completion-model construction.
4. `moh::tools` owns tool behavior and durable tool state. Its Rig-facing
   adapter is separate from the synchronous read service.
5. `moh::tui` remains the reusable rendering library.
6. The binary application is the composition and presentation adapter. It
   translates terminal input into harness commands and projects harness events
   into TUI components.

The dependency direction is:

```text
binary app -> harness <- runtime::rig -> providers::codex
                         |
                         +-> tools

binary app -> tui
```

`harness` must not import Rig, Codex, Reqwest, Crossterm, or concrete tool
types. The TUI library must not import harness, provider, or runtime types.

## Harness Core

### Messages and session history

The harness owns a minimal model-neutral message representation:

```rust
pub enum Role {
    User,
    Assistant,
}

pub struct Message {
    pub role: Role,
    pub text: String,
}
```

The committed `Session` remains in memory and contains successful user and
assistant exchanges in chronological order. Tool calls and failed prompts do
not become model-facing history. The representation is deliberately text-only;
new content-part variants will be added only when a feature requires them.

### Run requests and context

The core request passed to an engine is:

```rust
pub struct RunRequest {
    pub history: Vec<Message>,
    pub prompt: String,
    pub context: RunContext,
}

pub struct RunContext {
    pub cwd: PathBuf,
}
```

The harness snapshots committed history when a prompt is submitted. The
request owns its data so the engine stream has no borrow into mutable session
state.

`RunContext::cwd` replaces process-global current-directory lookup for relative
tool paths. Absolute reads remain allowed. There is no capability or
confinement field in this milestone.

### Run engine boundary

The runtime boundary is intentionally small:

```rust
pub trait RunEngine {
    fn start(&self, request: RunRequest) -> RunStream;
}
```

`RunStream` yields `Result<EngineEvent, RunFailure>`. An engine event has no
run ID because the harness, not an adapter, owns run identity:

```rust
pub enum EngineEvent {
    AssistantDelta(String),
    ToolStarted {
        call_id: String,
        name: String,
        arguments: serde_json::Value,
    },
    ToolFinished {
        call_id: String,
        name: String,
    },
    Completed(String),
}
```

Raw tool results stay inside the Rig/model interaction and are not copied into
harness events. This avoids retaining large read results while still exposing
observable agent activity.

### Harness ownership

`Harness<E: RunEngine>` replaces `Conversation` and owns:

- the engine;
- committed session history;
- an optional active run;
- the next monotonic `RunId`;
- the pending user prompt for the active run.

The public command surface has these semantics:

- `submit(prompt, context) -> Result<RunEvent, HarnessError>` starts a run and
  returns its `Started` event;
- `is_running()` to derive busy state;
- `next_event() -> Option<RunEvent>` awaits the next event for the active run,
  returning `None` only when no run is active;
- `cancel() -> Result<RunEvent, HarnessError>` drops the active engine stream
  and returns its `Cancelled` event;
- `history()` returns committed model-facing messages.

Submitting while busy returns `HarnessError::Busy`. Cancelling without an
active run returns `HarnessError::NotRunning`. `RunId` allocation uses checked
increment and returns `HarnessError::RunIdExhausted` rather than wrapping.

### Public run events

The harness attaches the active ID and exposes ordered events:

```rust
pub enum RunEvent {
    Started { run_id: RunId },
    AssistantDelta { run_id: RunId, text: String },
    ToolStarted {
        run_id: RunId,
        call_id: String,
        name: String,
        arguments: serde_json::Value,
    },
    ToolFinished {
        run_id: RunId,
        call_id: String,
        name: String,
    },
    Completed { run_id: RunId, response: String },
    Failed { run_id: RunId, failure: RunFailure },
    Cancelled { run_id: RunId },
}
```

The event stream is the observable run record for this milestone, but it is not
persisted. The TUI transcript is a projection of these events and is never the
source of model history.

### Lifecycle invariants

The harness enforces:

1. At most one run is active.
2. `submit` allocates one unique ID, snapshots committed history, stores the
   pending prompt, and emits `Started`.
3. Assistant deltas are forwarded but never committed alone.
4. Tool events are forwarded only for the active run.
5. `Completed` must contain a non-whitespace final response. It atomically
   commits the pending user prompt and final assistant response, clears the
   active run, and emits the public terminal event.
6. An engine error clears the active run and emits `Failed` without changing
   committed history.
7. Engine EOF before `Completed` is a protocol failure and does not commit
   partial output.
8. `cancel` drops the engine stream before releasing the busy state, emits
   `Cancelled`, and does not change committed history.
9. After every terminal event, another prompt may start.

## Error Model

`RunFailure` preserves operational meaning without exposing credentials,
provider bodies, authorization headers, or raw tool outputs.

It contains:

- a `RunStage`: startup, model request, tool execution, or finalization;
- a `RunFailureKind`: authentication, transport, HTTP rejection, protocol
  incompatibility, empty response, budget exhaustion, or runtime/tool
  infrastructure;
- an explicit `retryable` value determined at the adapter boundary;
- concise redacted display text;
- a typed source chain where it can remain safe.

Budget exhaustion must no longer map to generic protocol incompatibility.
Permanent credential refresh failures remain distinguishable from transient
transport failures. Model-visible read errors continue through Rig as tool
results and do not fail the run.

## Rig Runtime Adapter

`runtime::rig::CodexRunEngine` is the concrete first `RunEngine`
implementation. It is allowed to depend on both Rig and the Codex adapter. A
generic model-provider factory is deferred until a second implementation
demonstrates the required abstraction.

For each request, the engine:

1. converts model-neutral committed history into Rig messages;
2. obtains the current Codex completion model;
3. creates a request-scoped Rig agent;
4. registers the read tool;
5. applies the configured model, reasoning effort, and model-call limit;
6. runs the Rig tool loop;
7. converts assistant and tool activity into `EngineEvent` values;
8. buffers provisional tool-turn text and emits only terminal assistant output;
9. validates Codex completed-response evidence;
10. applies the current one-refresh behavior before terminal assistant text has
    been emitted;
11. shares one model-call budget across the initial attempt, tool
    continuations, and an authentication rebuild.

Dropping the engine stream cancels the Rig run. OAuth refresh persistence keeps
its existing cancellation-safe transaction semantics.

## Codex Adapter

The current authentication and provider file will be decomposed conceptually
as:

- `providers::codex::auth`: credential discovery, parsing, redaction, refresh,
  lock acquisition, concurrent-change detection, and atomic persistence;
- `providers::codex::model`: completion-model and request construction;
- `providers::codex::sse`: streaming HTTP compatibility, missing content-type
  normalization, and completed-response observation.

The exact file count may stay small where splitting would only create trivial
modules, but these responsibilities must not move back into the harness or
application.

## Configuration

Hardcoded production behavior becomes explicit injectable configuration with
defaults matching today's application:

```rust
pub struct CodexConfig {
    pub api_base: String,
    pub refresh_url: String,
}

pub struct AgentConfig {
    pub model: String,
    pub reasoning: ReasoningLevel,
    pub max_model_calls: usize,
}

pub struct ReadConfig {
    pub anchor_store_path: PathBuf,
}
```

`CodexConfig` belongs to the provider layer. `AgentConfig` belongs to the Rig
runtime. `ReadConfig` belongs to the tool layer. Production constructors supply
the current endpoints, `gpt-5.6-luna`, medium reasoning, the 512-call limit,
and the platform state location.

Tests construct ordinary explicit configuration. The hidden provider
constructors used only to inject test paths and budgets are removed. The
composition root passes model display metadata to the application instead of
the TUI importing a provider constant.

## Read Tool and Blocking Execution

The current reader mixes its synchronous domain behavior with Rig's async
`PortableTool` adapter. Split it into:

- `ReadService`, which synchronously validates arguments, resolves paths from
  the supplied run `cwd`, reads files or directories, updates anchors, and
  returns the existing model-visible output;
- a thin Rig read-tool adapter, which invokes `ReadService` through
  `tokio::task::spawn_blocking`.

The service remains synchronous so filesystem, normalization, anchor, and
directory behavior can be tested directly. The adapter owns join-error mapping
and ensures filesystem and SQLite work do not run on Moh's single-thread async
executor.

All existing read semantics remain unchanged, including absolute paths,
canonicalization, byte and line limits, directory listings, model-visible error
codes, durable anchors, and store corruption recovery.

## Application and TUI Integration

The binary remains responsible for terminal lifecycle and presentation, but no
longer owns agent lifecycle semantics.

The event loop selects between terminal input and `Harness::next_event()`:

- prompt submission calls `Harness::submit`;
- busy presentation derives from `Harness::is_running`;
- assistant deltas update the live response;
- generic tool-start events update the transcript;
- completed, failed, and cancelled events update final transcript and status;
- help and resize remain responsive while a run is active;
- Ctrl+C calls harness cancellation, which drops the Rig stream, before
  terminal restoration.

Known tool presentation may interpret the generic read arguments to retain the
current compact `Read path · lines ...` display. It must not depend on a Codex
or Rig event type.

The TUI library remains unaware of the harness. Application-specific view
state and event formatting stay in the binary adapter.

## API Migration

The implementation may remove or replace these public APIs without deprecation
wrappers:

- `Conversation`;
- `PendingTurn` and `CompletedTurn`;
- `ChatBackend`, `ChatFuture`, and `ChatStream`;
- `ChatEvent` and provider-specific `ReadCall`;
- the current all-in-one `CodexProvider` constructors.

The migration must preserve observable product behavior:

- one request at a time;
- successful-only committed history;
- partial assistant display without partial history commits;
- compact read activity lines;
- the existing model, reasoning, endpoints, and call-budget defaults;
- one pre-terminal authentication refresh and retry;
- redacted errors;
- responsive help, resize, and exit;
- cancellation before another provider poll;
- terminal sanitation and restoration;
- unchanged read output and anchor behavior.

## Testing Strategy

### Harness contract tests

Use fake `RunEngine` implementations to cover:

- monotonically allocated run IDs;
- one-active-run enforcement;
- committed-history snapshots sent to the engine;
- ordered assistant and tool event forwarding;
- successful atomic history commit;
- empty completion rejection;
- premature EOF rejection;
- engine failure after partial output without history mutation;
- cancellation dropping the stream before releasing busy state;
- reuse after completion, failure, and cancellation.

These tests must not import Rig or provider types.

### Rig runtime tests

Retain and reorganize the current scripted Responses integration coverage for:

- request history, model, and reasoning configuration;
- read-tool advertisement and exact correlated function-call output;
- terminal-only assistant output;
- rejection of incomplete or malformed streams;
- shared model-call budget across tool continuations and 401 rebuilds;
- exactly one eligible authentication refresh;
- cancellation without an unwanted continuation request;
- generic tool-event translation.

### Codex adapter tests

Keep focused coverage for:

- request headers and `store: false`;
- SSE line endings, chunk boundaries, and completion evidence;
- transport and HTTP rejection classification;
- credential discovery and validation;
- redaction;
- refresh locking, rotation, bounded waits, and cancellation-safe persistence.

### Read tool tests

Keep existing reader and anchor-store behavior tests against `ReadService`.
Add an adapter test that holds a blocking read operation while proving another
task on the current-thread runtime continues to make progress.

### Application and TUI tests

Update application tests to use fake harness engines and generic events. Cover
successful display, partial display, read activity, failure recovery,
cancellation, help, resize, terminal cleanup, and repeat submission. Existing
TUI component, layout, renderer, overlay, and terminal tests remain unchanged
unless imports move.

## Acceptance Criteria

The milestone is complete when:

- `moh::harness` has no imports from Rig, Codex, Reqwest, Crossterm, or concrete
  tools;
- TUI event handling has no Rig or provider event types;
- `CodexRunEngine` implements the model-neutral `RunEngine` boundary;
- the harness, not the application, owns run IDs, terminal outcomes,
  successful-only history commits, premature EOF handling, and cancellation;
- configuration is explicit and testable without hidden constructors;
- read and SQLite work execute off the async runtime thread;
- all listed observable behavior remains intact;
- the opt-in live Codex test remains separate from ordinary validation;
- `cargo fmt --all -- --check` passes;
- `cargo clippy --all-targets --all-features -- -D warnings` passes;
- `cargo test --all-targets` passes;
- `cargo build --locked` passes;
- `git diff --check` passes.
