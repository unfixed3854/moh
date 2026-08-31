# Harness Core Architecture Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox syntax for tracking.

**Goal:** Replace Moh's provider-shaped conversation path with a model-neutral harness core while preserving the current Codex, Rig, read-tool, and TUI behavior.

**Architecture:** Add moh::harness as the owner of sessions and run lifecycle, adapt Rig through runtime::rig::CodexRunEngine, and reduce providers::codex to authentication and completion transport. Keep one crate, move blocking read and SQLite work to Tokio's blocking pool, and make the binary a command/event projection layer.

**Tech Stack:** Rust 2024, Tokio current-thread runtime, futures streams, Rig 0.41, Reqwest 0.13, Rusqlite 0.40, Crossterm 0.29, thiserror 2.

**Spec:** docs/superpowers/specs/2026-08-19-harness-core-architecture-design.md

---

## Global constraints

- Keep one Cargo package and one library crate.
- Breaking the current public APIs is allowed. Do not add compatibility aliases or shims.
- Do not add filesystem authority, approval, sandboxing, allowlists, or path confinement.
- Absolute read paths remain valid. Resolve relative read paths from RunContext.cwd.
- Commit only successful, text-only user/assistant exchanges to session history.
- Support one active run per Harness. Do not add concurrent runs or persistence.
- Public run events expose tool lifecycle metadata, not raw tool results.
- Preserve the current defaults: gpt-5.6-luna, medium reasoning, and 512 model calls.
- Share one call budget across initial calls, tool continuations, and the one 401 retry. Token refresh neither consumes nor resets that budget.
- Preserve auth redaction, store: false, SSE completion validation, current read behavior, and current TUI rendering behavior.
- Do not introduce the skipped S/security category through adjacent work.
- Use cargo clippy --all-targets --all-features -- -D warnings as the primary static gate; do not add a mandatory cargo check gate.
- Keep the live Codex test ignored.
- Add rustdoc to every new public module, type, variant, field, method, constant, and re-exported API because the crate enables missing_docs warnings.

## Target source layout

    src/
      harness/
        mod.rs
        engine.rs
        error.rs
        state.rs
        types.rs
      providers/
        mod.rs
        codex/
          mod.rs
          auth.rs
          model.rs
          sse.rs
      runtime/
        mod.rs
        rig/
          mod.rs
          codex.rs
          read_tool.rs
      tools/
        mod.rs
        anchor_store.rs
        read.rs
      app.rs
      lib.rs
      main.rs
    tests/
      harness.rs
      rig_runtime.rs
      codex_auth.rs
      codex_live.rs
      read_tool.rs

The migration removes src/conversation.rs, src/codex_auth.rs, src/codex_provider.rs, and tests/conversation.rs after their responsibilities move to the target modules.

## Task 1: Add the model-neutral harness core

**Files:**

- Create: src/harness/mod.rs
- Create: src/harness/engine.rs
- Create: src/harness/error.rs
- Create: src/harness/state.rs
- Create: src/harness/types.rs
- Create: tests/harness.rs
- Modify: src/lib.rs

- [x] **Step 1: Write provider-free lifecycle tests**

Create tests/harness.rs with a fake engine that records requests and returns scripted streams:

    use std::{
        collections::VecDeque,
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    use futures::{stream, StreamExt};
    use moh::harness::{
        EngineEvent, Harness, HarnessError, Message, Role, RunContext, RunEngine,
        RunEvent, RunFailure, RunFailureKind, RunRequest, RunStage, RunStream,
    };

    #[derive(Clone, Default)]
    struct FakeEngine {
        requests: Arc<Mutex<Vec<RunRequest>>>,
        streams: Arc<Mutex<VecDeque<RunStream>>>,
    }

    impl FakeEngine {
        fn with_stream(events: Vec<Result<EngineEvent, RunFailure>>) -> Self {
            let streams = VecDeque::from([Box::pin(stream::iter(events)) as RunStream]);
            Self {
                requests: Arc::default(),
                streams: Arc::new(Mutex::new(streams)),
            }
        }
    }

    impl RunEngine for FakeEngine {
        fn start(&self, request: RunRequest) -> RunStream {
            self.requests.lock().unwrap().push(request);
            self.streams
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    Box::pin(stream::once(async {
                        Err(RunFailure::new(
                            RunStage::Startup,
                            RunFailureKind::RuntimeInfrastructure,
                            false,
                            "fake engine has no scripted stream",
                        ))
                    }))
                })
        }
    }

Add focused tests with these exact names:

- completed_run_commits_one_user_assistant_exchange
- failure_after_delta_does_not_commit_partial_history
- second_submit_while_running_returns_busy
- premature_engine_eof_becomes_protocol_failure
- blank_completion_becomes_empty_response_failure
- cancel_drops_the_stream_before_releasing_busy_state
- harness_assigns_monotonic_run_ids_and_preserves_tool_call_ids
- next_request_receives_a_snapshot_of_committed_history

Each test must assert complete event values, the final history, and the recorded RunRequest rather than checking only variants.

- [x] **Step 2: Run the new test target and confirm the missing API failure**

Run:

    cargo test --test harness

Expected: compilation fails because moh::harness and its types do not exist.

- [x] **Step 3: Define harness data and error types**

In src/harness/types.rs, add:

    use std::path::PathBuf;

    use serde_json::Value;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum Role {
        User,
        Assistant,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct Message {
        pub role: Role,
        pub text: String,
    }

    impl Message {
        pub fn new(role: Role, text: impl Into<String>) -> Self {
            Self {
                role,
                text: text.into(),
            }
        }
    }

    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    pub struct RunId(u64);

    impl RunId {
        pub(crate) const fn new(value: u64) -> Self {
            Self(value)
        }

        pub const fn get(self) -> u64 {
            self.0
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct RunContext {
        pub cwd: PathBuf,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct RunRequest {
        pub prompt: String,
        pub history: Vec<Message>,
        pub context: RunContext,
    }

    #[derive(Clone, Debug, PartialEq)]
    pub enum EngineEvent {
        AssistantDelta(String),
        ToolStarted {
            call_id: String,
            name: String,
            arguments: Value,
        },
        ToolFinished {
            call_id: String,
            name: String,
        },
        Completed(String),
    }

    #[derive(Debug)]
    pub enum RunEvent {
        Started { run_id: RunId },
        AssistantDelta { run_id: RunId, text: String },
        ToolStarted {
            run_id: RunId,
            call_id: String,
            name: String,
            arguments: Value,
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

In src/harness/error.rs, define:

    use std::error::Error;

    use thiserror::Error;

    type BoxError = Box<dyn Error + Send + Sync + 'static>;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum RunStage {
        Startup,
        ModelRequest,
        ToolExecution,
        Finalization,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub enum RunFailureKind {
        Authentication,
        Transport,
        HttpRejected { status: u16 },
        Protocol,
        EmptyResponse,
        BudgetExhausted,
        RuntimeInfrastructure,
        ToolInfrastructure,
    }

    #[derive(Debug, Error)]
    #[error("{message}")]
    pub struct RunFailure {
        stage: RunStage,
        kind: RunFailureKind,
        retryable: bool,
        message: String,
        #[source]
        source: Option<BoxError>,
    }

    impl RunFailure {
        pub fn new(
            stage: RunStage,
            kind: RunFailureKind,
            retryable: bool,
            message: impl Into<String>,
        ) -> Self {
            Self {
                stage,
                kind,
                retryable,
                message: message.into(),
                source: None,
            }
        }

        pub fn with_source<E>(mut self, source: E) -> Self
        where
            E: Error + Send + Sync + 'static,
        {
            self.source = Some(Box::new(source));
            self
        }

        pub const fn stage(&self) -> RunStage {
            self.stage
        }

        pub fn kind(&self) -> &RunFailureKind {
            &self.kind
        }

        pub const fn retryable(&self) -> bool {
            self.retryable
        }

        pub fn message(&self) -> &str {
            &self.message
        }
    }

    #[derive(Debug, Error, Eq, PartialEq)]
    pub enum HarnessError {
        #[error("a run is already active")]
        Busy,
        #[error("there is no active run")]
        NotRunning,
        #[error("run identifier space is exhausted")]
        RunIdExhausted,
    }

- [x] **Step 4: Define the engine port**

In src/harness/engine.rs, add:

    use std::pin::Pin;

    use futures::Stream;

    use super::{EngineEvent, RunFailure, RunRequest};

    pub type RunStream =
        Pin<Box<dyn Stream<Item = Result<EngineEvent, RunFailure>> + Send + 'static>>;

    pub trait RunEngine: Send + Sync + 'static {
        fn start(&self, request: RunRequest) -> RunStream;
    }

- [x] **Step 5: Implement the state machine**

In src/harness/state.rs, implement:

    pub struct Harness<E> {
        engine: E,
        history: Vec<Message>,
        active: Option<ActiveRun>,
        next_run_id: u64,
        ids_exhausted: bool,
    }

    struct ActiveRun {
        id: RunId,
        prompt: String,
        stream: RunStream,
    }

Provide these methods:

    impl<E: RunEngine> Harness<E> {
        pub fn new(engine: E) -> Self;
        pub fn with_history(engine: E, history: Vec<Message>) -> Self;
        pub fn submit(
            &mut self,
            prompt: impl Into<String>,
            context: RunContext,
        ) -> Result<RunEvent, HarnessError>;
        pub async fn next_event(&mut self) -> Option<RunEvent>;
        pub fn cancel(&mut self) -> Result<RunEvent, HarnessError>;
        pub fn history(&self) -> &[Message];
        pub const fn is_running(&self) -> bool;
    }

Required state rules:

1. submit rejects Busy before allocating an ID.
2. ID allocation uses checked_add and permanently reports RunIdExhausted after u64::MAX.
3. submit stores ActiveRun and immediately returns Started without polling the engine stream.
4. AssistantDelta and tool lifecycle events are projected with the active RunId while preserving engine-supplied call_id values.
5. Completed rejects blank text with RunFailureKind::EmptyResponse.
6. A valid Completed commits exactly the submitted user prompt and completed assistant response, then drops the active stream.
7. Engine failure and premature EOF produce Failed and do not mutate history.
8. cancel takes and drops ActiveRun before returning Cancelled, so another submit is immediately legal.

Re-export the public API from src/harness/mod.rs and expose pub mod harness from src/lib.rs.

- [x] **Step 6: Run the harness tests and primary static gate**

Run:

    cargo test --test harness
    cargo clippy --all-targets --all-features -- -D warnings

Expected: both commands pass.

- [x] **Step 7: Commit the harness core**

Run:

    git add src/harness src/lib.rs tests/harness.rs
    git diff --cached --check
    git commit -m "feat(harness): add model-neutral run core"

## Task 2: Separate ReadService from the Rig tool adapter

**Files:**

- Modify: src/tools/mod.rs
- Modify: src/tools/read.rs
- Modify: src/tools/anchor_store.rs
- Create: src/runtime/mod.rs
- Create: src/runtime/rig/mod.rs
- Create: src/runtime/rig/read_tool.rs
- Modify: src/codex_provider.rs
- Modify: tests/read_tool.rs

- [x] **Step 1: Convert existing read tests to the synchronous service API**

Keep the existing AnchorStore module boundary. In tests/read_tool.rs, replace PortableTool::call invocations with:

    let service = ReadServiceFactory::new(ReadConfig::at(store_path))
        .for_cwd(workspace.path().to_path_buf());
    let output = service.read(ReadArgs::path("notes.txt"))?;

Keep every current behavior assertion. Add:

    #[test]
    fn relative_paths_are_resolved_from_the_run_context_cwd() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("note.txt"), "context local\n").unwrap();
        let store = workspace.path().join("anchors.sqlite");
        let service = ReadServiceFactory::new(ReadConfig::at(store))
            .for_cwd(workspace.path().to_path_buf());

        let output = service.read(ReadArgs::path("note.txt")).unwrap();
        let text = output.as_text().unwrap();

        assert!(text.contains("context local"));
    }

- [x] **Step 2: Run the read integration test and confirm the API failure**

Run:

    cargo test --test read_tool

Expected: compilation fails because ReadServiceFactory, ReadConfig, and ReadArgs::path do not exist.

- [x] **Step 3: Implement explicit read configuration and the synchronous service**

In src/tools/read.rs, add:

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct ReadConfig {
        pub anchor_store_path: PathBuf,
    }

    impl ReadConfig {
        pub fn platform_default() -> Result<Self, ReadToolError> {
            Ok(Self::at(
                moh_state_dir()
                    .map_err(|_| ReadToolError::Store)?
                    .join("hash-store.sqlite"),
            ))
        }

        pub fn at(path: impl Into<PathBuf>) -> Self {
            Self {
                anchor_store_path: path.into(),
            }
        }
    }

    #[derive(Clone)]
    pub struct ReadServiceFactory {
        config: ReadConfig,
        store: Arc<OnceLock<Result<AnchorStore, AnchorStoreError>>>,
    }

    impl ReadServiceFactory {
        pub fn new(config: ReadConfig) -> Self;
        pub fn for_cwd(&self, cwd: PathBuf) -> ReadService;
    }

    #[derive(Clone)]
    pub struct ReadService {
        cwd: PathBuf,
        config: ReadConfig,
        store: Arc<OnceLock<Result<AnchorStore, AnchorStoreError>>>,
    }

    impl ReadService {
        pub fn read(&self, args: ReadArgs) -> Result<ToolOutput, ReadToolError>;
        pub fn description() -> &'static str;
        pub fn parameters() -> serde_json::Value;
    }

    impl ReadArgs {
        pub fn path(path: impl Into<String>) -> Self {
            Self {
                path: Some(path.into()),
                file_path: None,
                offset: None,
                limit: None,
            }
        }
    }

ReadService::read initializes AnchorStore through get_or_init, so platform lookup happens in ReadConfig::platform_default and SQLite open/recovery happens on the blocking worker during the first file read. Preserve the current behavior that a store initialization problem produces E_STORE when a file operation needs anchors; directory listings remain usable without the store.

Move the complete current read execution body from PortableTool::call into ReadService::read. Its first path operation must be:

    let raw_path = match (args.path, args.file_path) {
        (Some(path), None) | (None, Some(path)) if !path.is_empty() => path,
        (Some(_), None) | (None, Some(_)) => {
            return Err(ReadToolError::InvalidArgument("path must not be empty"));
        }
        _ => {
            return Err(ReadToolError::InvalidArgument(
                "supply exactly one of path or file_path",
            ));
        }
    };
    let requested = PathBuf::from(raw_path);
    let path = if requested.is_absolute() {
        requested
    } else {
        self.cwd.join(requested)
    };

Change canonicalize_path to accept &Path and call fs::canonicalize directly. No read operation may call std::env::current_dir.

- [x] **Step 4: Add the non-blocking Rig adapter**

In src/runtime/rig/read_tool.rs, define:

    use std::sync::Arc;

    use rig::tool::{PortableTool, ToolExecutionError, ToolOutput};
    use thiserror::Error;

    use crate::tools::{ReadArgs, ReadService, ReadToolError};

    const RUNTIME_ERROR_CODE: &str = "MOH_READ_RUNTIME";

    #[derive(Debug, Error)]
    pub enum RigReadError {
        #[error(transparent)]
        Domain(#[from] ReadToolError),
        #[error("[E_RUNTIME] read tool worker failed")]
        Runtime(#[source] tokio::task::JoinError),
    }

    #[derive(Clone)]
    pub struct RigReadTool {
        service: Arc<ReadService>,
    }

    impl RigReadTool {
        pub fn new(service: ReadService) -> Self {
            Self {
                service: Arc::new(service),
            }
        }
    }

    async fn run_blocking<T, F>(operation: F) -> Result<T, tokio::task::JoinError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        tokio::task::spawn_blocking(operation).await
    }

    pub async fn run_blocking_read(
        service: Arc<ReadService>,
        args: ReadArgs,
    ) -> Result<ToolOutput, RigReadError> {
        run_blocking(move || service.read(args))
            .await
            .map_err(RigReadError::Runtime)?
            .map_err(RigReadError::Domain)
    }

    impl PortableTool for RigReadTool {
        const NAME: &'static str = "read";

        type Error = RigReadError;
        type Args = ReadArgs;
        type Output = ToolOutput;

        fn description(&self) -> String {
            ReadService::description().to_owned()
        }

        fn parameters(&self) -> serde_json::Value {
            ReadService::parameters()
        }

        fn map_error(&self, error: Self::Error) -> ToolExecutionError {
            match error {
                RigReadError::Domain(error) => ToolExecutionError::from_error(error),
                RigReadError::Runtime(error) => {
                    ToolExecutionError::other("read tool worker failed")
                        .with_code(RUNTIME_ERROR_CODE)
                        .with_source(error)
                }
            }
        }

        async fn call(
            &self,
            args: Self::Args,
        ) -> Result<Self::Output, Self::Error> {
            run_blocking_read(Arc::clone(&self.service), args).await
        }
    }

Update the temporary src/codex_provider.rs integration to register RigReadTool instead of registering the service directly.

- [x] **Step 5: Prove a blocked read does not stall the current-thread executor**

In a cfg(test) module in src/runtime/rig/read_tool.rs, test the private run_blocking helper with #[tokio::test(flavor = "current_thread")]:

1. Create a Tokio one-shot entered channel and a std::sync::mpsc release channel.
2. Pin run_blocking around a closure that sends entered, blocks on release_rx.recv, and returns 42.
3. Spawn a second local task that yields once and sets an Arc<AtomicBool> progressed flag.
4. Use tokio::select to poll the pinned blocking job until entered_rx resolves; fail if the job finishes first.
5. Yield once and assert progressed is true before sending release.
6. Send release and assert the blocking job returns 42.

This test must fail if the closure is called directly on the Tokio thread.

- [x] **Step 6: Run focused and full read gates**

Run:

    cargo test --test read_tool
    cargo test tools::read
    cargo test runtime::rig::read_tool
    cargo clippy --all-targets --all-features -- -D warnings

Expected: all commands pass.

- [x] **Step 7: Commit the read boundary**

Run:

    git add src/tools src/runtime src/codex_provider.rs tests/read_tool.rs
    git diff --cached --check
    git commit -m "refactor(read): separate service from Rig adapter"

## Task 3: Extract the Codex auth and transport adapter

**Files:**

- Create: src/providers/mod.rs
- Create: src/providers/codex/mod.rs
- Create: src/providers/codex/auth.rs
- Create: src/providers/codex/model.rs
- Create: src/providers/codex/sse.rs
- Delete: src/codex_auth.rs
- Modify: src/codex_provider.rs
- Modify: src/lib.rs
- Modify: src/main.rs
- Modify: tests/codex_auth.rs
- Modify: tests/codex_provider.rs
- Modify: tests/codex_live.rs

- [x] **Step 1: Retarget auth tests to the final module path**

Change every auth import to:

    use moh::providers::codex::{
        resolve_codex_home, AuthError, AuthFile, CodexConfig, CodexCredentials,
        RefreshFailure,
    };

Add:

    #[test]
    fn codex_config_uses_current_production_endpoints_by_default() {
        let config = CodexConfig::default();
        assert_eq!(
            config.api_base,
            "https://chatgpt.com/backend-api/codex"
        );
        assert_eq!(
            config.refresh_url,
            "https://auth.openai.com/oauth/token"
        );
    }

- [x] **Step 2: Run auth tests and confirm the new path is missing**

Run:

    cargo test --test codex_auth

Expected: compilation fails because moh::providers::codex is not exported.

- [x] **Step 3: Move auth without changing behavior**

Move src/codex_auth.rs to src/providers/codex/auth.rs. Keep all AuthFile, token parsing, refresh, persistence, redaction, and unit-test behavior unchanged.

Create src/providers/mod.rs:

    pub mod codex;

Create src/providers/codex/mod.rs with:

    mod auth;
    mod model;
    mod sse;

    pub use auth::{
        resolve_codex_home, AuthError, AuthFile, CodexCredentials, RefreshFailure,
    };
    pub use model::{CodexModelError, CodexModelFactory};
    pub(crate) use model::{CodexCompletionModel, ModelCallBudget};

    #[derive(Clone, Debug)]
    pub struct CodexConfig {
        pub api_base: String,
        pub refresh_url: String,
    }

    impl Default for CodexConfig {
        fn default() -> Self {
            Self {
                api_base: "https://chatgpt.com/backend-api/codex".to_owned(),
                refresh_url: "https://auth.openai.com/oauth/token".to_owned(),
            }
        }
    }

Export pub mod providers from src/lib.rs and update src/main.rs and tests to the new auth path.

- [x] **Step 4: Move SSE parsing into its private transport module**

Move these existing units from src/codex_provider.rs into src/providers/codex/sse.rs without semantic changes:

- CompletionObserver, renamed CompletionEvidence
- CodexHttpClient and its HttpClientExt implementation
- observe_completed_sse
- next_sse_event
- sse_line_ending_length
- normalize_sse_lines
- the current line-ending and chunk-boundary tests

Expose CodexHttpClient as pub(super) and CompletionEvidence as pub(crate). Keep their fields private, provide CompletionEvidence::completed(), and provide CodexCompletionModel::completion_evidence() so RunAttempt can retain a clone before moving the model into AgentBuilder. The client must continue to add text/event-stream only when content-type is absent and observe a response.completed event without consuming or rewriting body bytes.

Keep the runtime responsible for rejecting stream termination when CompletionObserver::completed() is false.

- [x] **Step 5: Move the completion model and HTTP client**

Move CodexCompletionModel, the Rig CompletionModel implementation, request serialization, store: false enforcement, HTTP status mapping, and one-call transport behavior into src/providers/codex/model.rs.

Add:

    #[derive(Clone)]
    pub struct CodexModelFactory {
        inner: Arc<Inner>,
    }

    struct Inner {
        auth: tokio::sync::Mutex<AuthFile>,
        http: reqwest::Client,
        config: CodexConfig,
    }

    impl CodexModelFactory {
        pub fn from_env(config: CodexConfig) -> Result<Self, CodexModelError>;

        pub fn new(auth: AuthFile, config: CodexConfig) -> Self {
            Self {
                inner: Arc::new(Inner {
                    auth: tokio::sync::Mutex::new(auth),
                    http: reqwest::Client::new(),
                    config,
                }),
            }
        }

        pub(crate) async fn completion_model(
            &self,
            model: impl Into<String>,
            budget: ModelCallBudget,
        ) -> Result<CodexCompletionModel, CodexModelError>;

        pub async fn refresh(&self) -> Result<(), CodexModelError>;
    }

Keep ModelCallBudget and the response observer private to providers::codex where possible. Expose them as pub(crate) only where Task 4 needs construction or inspection.

At the end of this step, src/codex_provider.rs retains only Rig agent construction, agent-loop streaming, read-tool registration, the one-401 retry decision, and mapping into its temporary public provider events.

- [x] **Step 6: Run transport regression gates**

Run:

    cargo test --test codex_auth
    cargo test --test codex_provider
    cargo test --test codex_live --no-run
    cargo test providers::codex
    cargo clippy --all-targets --all-features -- -D warnings

Expected: all commands pass, and the live test remains ignored when tests are executed normally.

- [x] **Step 7: Commit the Codex adapter**

Run:

    git add src/providers src/codex_provider.rs src/lib.rs src/main.rs tests
    git add -u src/codex_auth.rs
    git diff --cached --check
    git commit -m "refactor(codex): isolate auth and transport adapter"

## Task 4: Implement CodexRunEngine and remove the provider-shaped API

**Files:**

- Create: src/runtime/rig/codex.rs
- Modify: src/runtime/rig/mod.rs
- Delete: src/codex_provider.rs
- Rename: tests/codex_provider.rs to tests/rig_runtime.rs
- Modify: tests/codex_live.rs
- Modify: src/lib.rs

- [x] **Step 1: Retarget runtime tests to RunEngine**

Rename tests/codex_provider.rs to tests/rig_runtime.rs. Replace ChatBackend, ChatEvent, CodexProvider, ProviderError, and ReadCall assertions with:

    use futures::StreamExt;
    use moh::{
        harness::{
            EngineEvent, Message, Role, RunContext, RunEngine, RunFailureKind,
            RunRequest,
        },
        providers::codex::{CodexConfig, CodexModelFactory},
        runtime::rig::{AgentConfig, CodexRunEngine, ReasoningLevel},
        tools::{ReadConfig, ReadServiceFactory},
    };

Construct a RunRequest with committed history, prompt, and cwd. Consume the RunStream returned by RunEngine::start.

Preserve all existing behavioral coverage:

- request JSON contains store: false
- auth headers and account selection
- SSE text streaming
- response.completed validation
- model and reasoning configuration
- read tool registration and continuation
- exactly one token refresh after 401
- retry request uses refreshed credentials
- call budget is shared across retry and continuation
- dropping the run stream before a tool result prevents the continuation request
- provider and authorization values remain redacted from errors

Change event expectations to EngineEvent. Assert budget exhaustion as:

    assert_eq!(failure.kind(), &RunFailureKind::BudgetExhausted);
    assert!(!failure.retryable());

- [x] **Step 2: Run the retargeted tests and confirm runtime types are missing**

Run:

    cargo test --test rig_runtime

Expected: compilation fails because CodexRunEngine, AgentConfig, and ReasoningLevel do not exist.

- [x] **Step 3: Define explicit agent configuration**

In src/runtime/rig/codex.rs, add:

    pub const DEFAULT_MODEL: &str = "gpt-5.6-luna";
    pub const DEFAULT_MAX_MODEL_CALLS: usize = 512;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum ReasoningLevel {
        Low,
        Medium,
        High,
    }

    impl ReasoningLevel {
        fn as_codex_effort(
            self,
        ) -> rig::providers::openai::responses_api::ReasoningEffort {
            use rig::providers::openai::responses_api::ReasoningEffort;

            match self {
                Self::Low => ReasoningEffort::Low,
                Self::Medium => ReasoningEffort::Medium,
                Self::High => ReasoningEffort::High,
            }
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct AgentConfig {
        pub model: String,
        pub reasoning: ReasoningLevel,
        pub max_model_calls: usize,
    }

    impl Default for AgentConfig {
        fn default() -> Self {
            Self {
                model: DEFAULT_MODEL.to_owned(),
                reasoning: ReasoningLevel::Medium,
                max_model_calls: DEFAULT_MAX_MODEL_CALLS,
            }
        }
    }

Reject max_model_calls == 0 during CodexRunEngine construction with a typed startup RunFailureKind::BudgetExhausted.

- [x] **Step 4: Implement the concrete engine**

Define:

    #[derive(Clone)]
    pub struct CodexRunEngine {
        models: CodexModelFactory,
        agent: AgentConfig,
        reads: ReadServiceFactory,
    }

    impl CodexRunEngine {
        pub fn new(
            models: CodexModelFactory,
            agent: AgentConfig,
            reads: ReadServiceFactory,
        ) -> Result<Self, RunFailure>;

        pub fn model_name(&self) -> &str;
    }

    impl RunEngine for CodexRunEngine {
        fn start(&self, request: RunRequest) -> RunStream {
            let budget = ModelCallBudget::new(self.agent.max_model_calls);
            let read = RigReadTool::new(self.reads.for_cwd(request.context.cwd.clone()));
            let attempt = RunAttempt::new(
                self.models.clone(),
                self.agent.clone(),
                budget,
                read,
                request,
            );
            Box::pin(attempt.into_stream())
        }
    }

Move the complete agent construction, Rig prompt/history conversion, MultiTurnStream polling, and retry state machine from src/codex_provider.rs into RunAttempt.

History conversion must be exhaustive:

    fn to_rig_messages(history: &[Message]) -> Vec<rig::message::Message> {
        history
            .iter()
            .map(|message| match message.role {
                Role::User => rig::message::Message::user(&message.text),
                Role::Assistant => rig::message::Message::assistant(&message.text),
            })
            .collect()
    }

Map Rig stream items as follows:

| Rig/runtime input | Engine output |
| --- | --- |
| assistant text delta | append to the private current-turn buffer |
| assistant tool call | clear the provisional buffer and emit EngineEvent::ToolStarted { call_id: internal_call_id, name, arguments } |
| ToolExecutionCommitted | EngineEvent::ToolFinished { call_id: internal_call_id, name } |
| ordinary StreamUserItem::ToolResult | keep internal to Rig and clear the provisional buffer |
| StreamUserItem::ToolResult with code MOH_READ_RUNTIME | Err(RuntimeInfrastructure RunFailure) |
| FinalResponse(text) | EngineEvent::AssistantDelta(text), then EngineEvent::Completed(text) |
| model/auth/stream error | Err(RunFailure) |

Do not emit text from a model turn that later calls a tool. Maintain a provisional current-turn buffer, clear it at tool-call and tool-result boundaries, and use FinalResponse.output as the single terminal assistant payload.

The engine must never expose tool results in EngineEvent. Domain ReadToolError values remain model-visible Rig tool results and continue the run; only the adapter's MOH_READ_RUNTIME infrastructure code terminates it.

- [x] **Step 5: Preserve the one-refresh retry with a shared budget**

The attempt stream owns one ModelCallBudget created before its first HTTP request. On the first 401 authentication rejection:

1. Mark refresh_attempted before awaiting refresh.
2. Call CodexModelFactory::refresh.
3. Rebuild the completion model and Rig agent with the same ModelCallBudget.
4. Restart the current attempt once.
5. Map a second 401 to a non-retryable Authentication failure.

Do not recreate ModelCallBudget anywhere in the retry branch. Add an exact test where max_model_calls is two, the first request returns 401, the retry invokes a tool, and the attempted continuation returns BudgetExhausted.

- [x] **Step 6: Map errors into stable harness failures**

Add exhaustive conversions with these rules:

| Source | RunStage | RunFailureKind | retryable |
| --- | --- | --- | --- |
| auth file or refresh rejection | Startup or ModelRequest | Authentication | false |
| Reqwest connect/timeout | ModelRequest | Transport | true |
| HTTP 401 after retry | ModelRequest | Authentication | false |
| other HTTP status | ModelRequest | HttpRejected { status } | status == 429 or status >= 500 |
| malformed/incomplete SSE | ModelRequest | Protocol | false |
| call limit | ModelRequest | BudgetExhausted | false |
| read worker join failure | ToolExecution | RuntimeInfrastructure | false |
| empty final answer | Finalization | EmptyResponse | false |

User-visible messages must not include access tokens, refresh tokens, authorization headers, account IDs, raw response bodies, or serialized provider errors.

- [x] **Step 7: Remove the old public provider layer**

Delete src/codex_provider.rs and remove these exports everywhere:

- ChatBackend
- ChatFuture
- ChatEvent
- ChatStream
- CodexProvider
- ProviderError
- ReadCall
- MODEL

Re-export CodexRunEngine, AgentConfig, ReasoningLevel, and DEFAULT_MODEL from src/runtime/rig/mod.rs. Export pub mod runtime from src/lib.rs.

Update tests/codex_live.rs to construct CodexModelFactory, ReadServiceFactory, and CodexRunEngine, call RunEngine::start, and wait for EngineEvent::Completed. Keep the existing credential and network ignore guard.

- [x] **Step 8: Run runtime regression gates**

Run:

    cargo test --test rig_runtime
    cargo test --test codex_live --no-run
    cargo test runtime::rig
    cargo clippy --all-targets --all-features -- -D warnings

Expected: all commands pass.

- [x] **Step 9: Commit the runtime migration**

Run:

    git add src/runtime src/providers src/lib.rs tests
    git add -u src/codex_provider.rs tests/codex_provider.rs
    git diff --cached --check
    git commit -m "refactor(runtime): run Codex through harness engine"

## Task 5: Migrate the terminal app to Harness

**Files:**

- Modify: src/app.rs
- Modify: src/main.rs
- Modify: src/lib.rs
- Delete: src/conversation.rs
- Delete: tests/conversation.rs

- [x] **Step 1: Replace app test doubles with a scripted engine**

In src/app.rs tests, replace fake ChatBackend implementations with a ScriptedEngine implementing RunEngine. Script EngineEvent values and errors, then wrap the engine in Harness.

Retain every existing app and TUI assertion. Add:

- tool_started_projects_read_arguments_with_the_current_cwd
- unknown_tool_arguments_fall_back_to_a_generic_activity_label
- failed_run_keeps_the_previous_committed_history
- cancelling_an_active_run_restores_idle_input_state
- application_exit_cancels_before_terminal_restoration

The unknown-tool test must use nested JSON arguments and verify that the UI does not display the raw JSON.

- [x] **Step 2: Run app tests and confirm they fail against the old coordinator**

Run:

    cargo test app::

Expected: compilation fails after the test imports are changed to Harness and RunEvent.

- [x] **Step 3: Make production composition explicit**

In src/main.rs, build dependencies in this order:

    let codex = CodexModelFactory::from_env(CodexConfig::default())?;
    let reads = ReadServiceFactory::new(ReadConfig::platform_default()?);
    let engine = CodexRunEngine::new(codex, AgentConfig::default(), reads)?;
    let model_name = engine.model_name().to_owned();
    let harness = Harness::new(engine);
    app::run(harness, model_name).await

Perform this construction inside the current run_with_current_thread_runtime closure so auth refresh and request work stay on the same cancellation-safe runtime. Extend AppError with transparent variants for CodexModelError, ReadToolError, RunFailure, and HarnessError. Do not collapse setup failures into strings.

- [x] **Step 4: Replace Conversation with direct harness ownership**

Make the app runtime generic over RunEngine:

    #[derive(Default)]
    struct RunProjection {
        assistant_text: String,
    }

    pub async fn run<R: RunEngine>(
        mut harness: Harness<R>,
        model_name: String,
    ) -> std::result::Result<(), AppError>;

    async fn run_event_loop<T, E, R>(
        tui: &mut Tui<T>,
        ids: &mut AppIds,
        events: &mut E,
        harness: &mut Harness<R>,
        projection: &mut RunProjection,
    ) -> std::result::Result<(), AppError>
    where
        T: Terminal,
        E: EventSource,
        R: RunEngine;

Pass model_name into build and store it in AppIds. Change status_line to accept that value instead of importing a provider constant.

Remove PendingTurn and every Conversation reference. On submit:

1. Construct RunContext with the app's captured cwd.
2. Call harness.submit and retain its Started event before mutating visible state.
3. Append the visible user message and thinking indicator only after submit succeeds.
4. Apply the Started event through the same projection function used for later events.

In the existing 16 ms event loop, select between terminal input and harness.next_event only while harness.is_running. Do not introduce a second event loop or a background channel.

- [x] **Step 5: Project generic RunEvent values into the current TUI**

Add:

    fn apply_run_event<T: Terminal>(
        tui: &mut Tui<T>,
        ids: &AppIds,
        projection: &mut RunProjection,
        event: RunEvent,
    ) -> std::result::Result<(), AppError> {
        match event {
            RunEvent::Started { .. } => {}
            RunEvent::AssistantDelta { text, .. } => {
                projection.assistant_text.push_str(&text);
                update_live_response(tui, ids, &projection.assistant_text)?;
            }
            RunEvent::ToolStarted {
                name, arguments, ..
            } => {
                let line = format_tool_started(&name, &arguments, &ids.cwd_path);
                tui.component_mut::<Container>(ids.transcript)?
                    .push(Text::new(line));
                tui.request_render();
            }
            RunEvent::ToolFinished { .. } => {}
            RunEvent::Completed { response, .. } => {
                projection.assistant_text.clear();
                clear_live_response(tui, ids)?;
                let transcript = tui.component_mut::<Container>(ids.transcript)?;
                transcript.push(AiMessage::new(moh::tui::text::sanitize_markdown(
                    &response,
                )));
                transcript.push(Spacer::new(1));
                set_status(tui, ids, StatusState::Ready)?;
                tui.request_render();
            }
            RunEvent::Failed { failure, .. } => {
                projection.assistant_text.clear();
                apply_run_failure(tui, ids, failure)?;
            }
            RunEvent::Cancelled { .. } => {
                projection.assistant_text.clear();
                clear_live_response(tui, ids)?;
                set_status(tui, ids, StatusState::Ready)?;
                tui.request_render();
            }
        }
        Ok(())
    }

Implement format_tool_started as a UI projection:

1. If name == "read", deserialize arguments into ReadArgs.
2. Display the same resolved read path and range wording the app displays today.
3. For any parse failure or unknown tool, sanitize name with Input::sanitize_plain_text and display Running {name}.
4. Never display raw JSON arguments.

Completed updates visible state only; Harness has already committed history. Failed and Cancelled clear provisional UI without committing history. apply_run_failure must display only RunFailure's redacted Display text.

- [x] **Step 6: Make shutdown order explicit**

On Ctrl-C, EOF, terminal error, or normal app exit:

1. If harness.is_running, call harness.cancel.
2. Drop the resulting Cancelled event after allowing app state cleanup where applicable.
3. Stop polling the run stream.
4. Restore terminal mode and screen state.

Retain the current terminal guard so restoration also runs on early errors.

- [x] **Step 7: Delete Conversation and scan for stale APIs**

Delete src/conversation.rs and tests/conversation.rs. Remove pub mod conversation from src/lib.rs.

Run:

    rg -n "Conversation|PendingTurn|ChatBackend|ChatFuture|ChatEvent|ChatStream|CodexProvider|ProviderError|ReadCall|codex_provider|codex_auth|\\bMODEL\\b" src tests

Expected: no matches.

- [x] **Step 8: Run app and full static gates**

Run:

    cargo test app::
    cargo test --test harness
    cargo test --test rig_runtime
    cargo clippy --all-targets --all-features -- -D warnings

Expected: all commands pass.

- [x] **Step 9: Commit the app migration**

Run:

    git add src/app.rs src/main.rs src/lib.rs
    git add -u src/conversation.rs tests/conversation.rs
    git diff --cached --check
    git commit -m "refactor(app): drive terminal UI from harness events"

## Task 6: Document and verify the completed architecture

**Files:**

- Modify: README.md
- Modify: docs/superpowers/plans/2026-08-19-harness-core-architecture.md

- [x] **Step 1: Document the architecture and extension points**

Add a concise Architecture section to README.md covering:

- harness owns run IDs, lifecycle, terminal outcomes, and successful history
- RunEngine is the model-neutral execution port
- runtime::rig adapts Rig and tools to that port
- providers::codex owns Codex authentication and HTTP/SSE completion transport
- tools owns synchronous capabilities such as ReadService
- app owns terminal command and event projection only

Include one extension example: another provider/runtime combination implements RunEngine and can reuse Harness and the TUI without changing harness state.

- [x] **Step 2: Verify crate documentation**

Run:

    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features

Expected: documentation builds with no warnings.

- [x] **Step 3: Run formatting and inspect its exact scope**

Run:

    cargo fmt --all -- --check

If it fails, run cargo fmt --all, then inspect:

    git status --short
    git diff --stat
    git diff --check

Expected: formatting changes touch only files involved in this plan, and git diff --check reports no whitespace errors.

- [x] **Step 4: Run the complete acceptance suite**

Run:

    cargo clippy --all-targets --all-features -- -D warnings
    cargo test --all-targets --all-features
    cargo build --locked

Expected: all commands pass. The credentialed live test reports ignored, not passed.

- [x] **Step 5: Audit architectural dependency direction**

Run:

    rg -n "rig::|reqwest::|crossterm::|providers::codex|runtime::rig|ReadService" src/harness
    rg -n "crossterm::|AppState|crate::app" src/providers src/runtime src/tools
    rg -n "std::env::current_dir" src/tools src/runtime
    rg -n "ChatBackend|ChatEvent|Conversation|PendingTurn|CodexProvider|ReadCall" src tests

Expected:

- src/harness has no matches.
- provider, runtime, and tool modules have no app or TUI matches.
- the read and runtime modules do not consult process cwd.
- removed APIs have no matches.

Crossterm remains intentionally present under src/tui and src/app.

- [x] **Step 6: Review the diff against the approved scope**

Run:

    git diff 3fbd019 --stat
    git diff 3fbd019 -- src/harness src/providers src/runtime src/tools src/app.rs src/main.rs

Confirm:

- no persistence or multi-run support was added
- no approval, sandbox, allowlist, or path-confinement policy was added
- no raw tool outputs cross the harness event boundary
- history commits only after a nonblank Completed event
- the one-refresh and shared-budget rules remain tested
- absolute read paths remain accepted
- existing TUI layout and rendering modules were not redesigned

- [x] **Step 7: Commit documentation**

Run:

    git add README.md docs/superpowers/plans/2026-08-19-harness-core-architecture.md
    git diff --cached --check
    git commit -m "docs: describe harness core architecture"

- [x] **Step 8: Verify final repository state**

Run:

    git status --short
    git log --oneline f73a207..HEAD

Expected: the worktree is clean and the stable merge-base range shows the complete feature history. Its ten-commit pre-final-review portion contains the design and planning commits, five primary implementation commits, the Task 5 app-visibility review fix, the architecture documentation commit, and the plan-bookkeeping commit; any later focused final-review fixes follow those commits without changing that topology.

## Completion criteria

The implementation is complete only when:

- all six task-level commits exist
- all non-live tests pass
- the live test compiles and remains ignored
- Clippy, rustdoc, formatting, and locked build gates pass
- no stale provider-shaped conversation API remains
- README matches the implemented module boundaries
- git status is clean
