# Client-Server Architecture Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split Moh into a terminal client and one automatically managed global backend that hosts multiple durable sessions and keeps agent work running after clients detach.

**Architecture:** A Unix-domain-socket Cap'n Proto service owns a lazy session manager. Each session actor owns one `Harness`, isolated job and file-observation state, persistence, an authoritative projection, and bounded observer queues; terminal clients attach from snapshots and send commands through RPC. A global activity tracker shuts the backend down only after the configured idle interval with no connections, runs, jobs, or dirty committed state.

**Tech Stack:** Rust 2024, Tokio current-thread runtime and `LocalSet`, Cap'n Proto/`capnp-rpc` 0.27, Unix-domain sockets, SQLite/`rusqlite`, TOML, Crossterm, existing Rig/Codex runtime.

**Spec:** `docs/superpowers/specs/2026-08-26-client-server-architecture-design.md`

## Global Constraints

- Use conventional commits.
- One global backend serves the current OS user.
- Unix-domain sockets are the only production transport in this milestone; non-Unix builds must fail with a clear unsupported-platform error rather than silently opening TCP.
- One default session exists per canonical CWD, and additional sessions may share that CWD.
- One run is allowed per session; different sessions may run concurrently.
- Multiple clients have equal control over a shared session.
- The idle timeout defaults to exactly 15 minutes and is loaded from `[server].idle_timeout` in Moh's TOML config.
- Disconnect, Ctrl+C, and `/quit` detach without cancellation; Escape and `/cancel` explicitly cancel.
- Persist only successful user/assistant turns, model, reasoning, context usage, identity, CWD, and last activity.
- Do not persist deltas, tool activity, failed/cancelled prompts, jobs, observers, or active runs.
- Keep job visibility and read-before-write/edit authority isolated per session.
- Check generated Cap'n Proto Rust bindings into the repository; normal builds must not require the `capnp` compiler.
- Preserve credential redaction, terminal sanitation/restoration, and all existing harness/tool/runtime behavior.
- Every implementation task follows red-green-refactor and ends with a focused conventional commit.

---

## File Structure Map

### Shared library additions

- `src/local/mod.rs` — exports local config, path, endpoint, and launch primitives.
- `src/local/config.rs` — strict TOML config parsing and the 15-minute default.
- `src/local/paths.rs` — platform config/state/runtime paths and Unix permission checks.
- `src/local/launch.rs` — connect-or-spawn lock, stale-socket validation, and detached process creation.
- `src/session/mod.rs` — public session-domain exports.
- `src/session/types.rs` — stable IDs, selectors, settings, summaries, snapshots, events, and command errors.
- `src/session/store.rs` — versioned SQLite repository and corruption quarantine.
- `src/session/projection.rs` — authoritative presentation-neutral snapshot reduction.
- `src/session/actor.rs` — one-run session actor, observer fan-out, commands, and dirty checkpointing.
- `src/session/manager.rs` — lazy multi-session registry and per-connection cleanup.
- `src/session/runtime.rs` — production/fake runtime factory interface and Codex factory.
- `src/backend/mod.rs` — backend composition entry points.
- `src/backend/activity.rs` — connection/run/job accounting and idle deadline.
- `src/backend/server.rs` — Unix accept loop, model-runtime initialization, and clean shutdown.
- `src/rpc/mod.rs` — protocol exports and version constants.
- `src/rpc/moh_capnp.rs` — checked-in generated Rust bindings; never hand-edit.
- `src/rpc/convert.rs` — lossless domain/schema conversion.
- `src/rpc/server.rs` — Cap'n Proto backend/session implementations and observer pumps.
- `src/rpc/client.rs` — bootstrap, attachment, sequence handling, and typed session commands.
- `schema/moh.capnp` — source protocol schema.
- `scripts/generate-rpc.sh` — reproducible binding regeneration.

### Binary client changes

- `src/cli.rs` — dependency-free parsing for default, `--new`, `--session`, `sessions`, and `server` modes.
- `src/client/mod.rs` — terminal-client composition.
- `src/client/app.rs` — existing TUI application moved from `src/app.rs` and driven by a session-client boundary.
- `src/client/session.rs` — TUI-facing async `SessionClient` trait plus RPC adapter.
- `src/server.rs` — foreground/detached backend mode composition.
- `src/main.rs` — mode dispatch and runtime setup only.

### Test additions

- `tests/support/mod.rs` — controlled run engine, repository failpoint, event helpers, and temporary local paths.
- `tests/local_config.rs` — TOML defaults, strict parsing, and path injection.
- `tests/local_paths.rs` — Unix endpoint ownership/type/mode checks.
- `tests/session_store.rs` — schema, identity, persistence, migration, and quarantine.
- `tests/session_actor.rs` — run lifecycle, projections, observers, and dirty checkpoints.
- `tests/session_manager.rs` — default/additional sessions, same-CWD concurrency, and isolation.
- `tests/backend_activity.rs` — paused-clock idle behavior.
- `tests/rpc_schema.rs` — domain/schema round trips and version fields.
- `tests/rpc_transport.rs` — real Unix-stream Cap'n Proto calls and callbacks.
- `tests/local_launch.rs` — startup locking and stale endpoint recovery.
- `tests/client_server.rs` — end-to-end detach, reattach, concurrency, persistence, and idle shutdown.

---

### Task 1: Isolate Conversation-Lifetime Tool State

**Files:**
- Modify: `src/tools/read.rs:110-141`
- Modify: `src/tools/job.rs:247-585`
- Modify: `tests/write_tool.rs`
- Modify: `tests/job_tool.rs`

**Interfaces:**
- Produces: `ReadServiceFactory::isolated_session(&self) -> ReadServiceFactory`.
- Produces: `JobRegistry::subscribe_changes(&self) -> watch::Receiver<u64>`.
- Produces: `JobRegistry::running_count(&self) -> Result<usize, JobRegistryError>`.
- Preserves: `ReadServiceFactory::clone` continues to share both anchors and observations inside one session.

- [ ] **Step 1: Write the failing cross-session observation test**

Add to `tests/write_tool.rs`:

~~~rust
#[tokio::test]
async fn isolated_reader_factory_does_not_share_write_authority() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "original\n").unwrap();
    let root = ReadServiceFactory::new(ReadConfig::at(
        directory.path().join("anchors.sqlite"),
    ));
    let first = root.isolated_session();
    let second = root.isolated_session();

    first
        .for_cwd(directory.path().to_owned())
        .read(ReadArgs::path("note.txt"))
        .await
        .unwrap();

    let error = WriteServiceFactory::sharing_reads(&second)
        .for_cwd(directory.path().to_owned())
        .write(WriteArgs {
            path: "note.txt".into(),
            content: "replacement\n".into(),
        })
        .await
        .unwrap_err();

    assert!(matches!(error, WriteToolError::NotRead));
    assert_eq!(
        std::fs::read_to_string(directory.path().join("note.txt")).unwrap(),
        "original\n"
    );
}
~~~

- [ ] **Step 2: Write the failing job-change subscription test**

Add to `tests/job_tool.rs`:

~~~rust
#[tokio::test]
async fn registry_change_subscription_reports_running_count_transitions() {
    let registry = JobRegistry::new();
    let mut changes = registry.subscribe_changes();
    let lease = running(&registry);

    changes.changed().await.unwrap();
    assert_eq!(registry.running_count().unwrap(), 1);

    drop(lease);
    changes.changed().await.unwrap();
    assert_eq!(registry.running_count().unwrap(), 0);
}
~~~

- [ ] **Step 3: Run the focused tests and verify red**

Run:

~~~bash
cargo test --test write_tool isolated_reader_factory_does_not_share_write_authority
cargo test --test job_tool registry_change_subscription_reports_running_count_transitions
~~~

Expected: both fail to compile because the three new methods do not exist.

- [ ] **Step 4: Implement the minimal isolation and notification APIs**

Add this method to `ReadServiceFactory`:

~~~rust
pub fn isolated_session(&self) -> Self {
    Self {
        config: self.config.clone(),
        store: Arc::clone(&self.store),
        observations: FileObservations::default(),
    }
}
~~~

Add these methods to `JobRegistry`:

~~~rust
pub fn subscribe_changes(&self) -> watch::Receiver<u64> {
    self.inner.version.subscribe()
}

pub fn running_count(&self) -> Result<usize, JobRegistryError> {
    Ok(self
        .lock()?
        .jobs
        .values()
        .filter(|entry| entry.snapshot.state() == JobState::Running)
        .count())
}
~~~

Document that `isolated_session` shares the durable anchor store but starts empty file observations.

- [ ] **Step 5: Run focused and full tool tests**

Run:

~~~bash
cargo test --test write_tool
cargo test --test edit_tool
cargo test --test read_tool
cargo test --test job_tool
cargo test --test bash_tool
~~~

Expected: all tests pass.

- [ ] **Step 6: Commit**

~~~bash
git add src/tools/read.rs src/tools/job.rs tests/write_tool.rs tests/job_tool.rs
git commit -m "refactor(tools): isolate session runtime state"
~~~

---

### Task 2: Add Strict Server Configuration and Local Paths

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/lib.rs`
- Create: `src/local/mod.rs`
- Create: `src/local/config.rs`
- Create: `src/local/paths.rs`
- Create: `tests/local_config.rs`
- Create: `tests/local_paths.rs`

**Interfaces:**
- Produces: `MohConfig::load(path: &Path) -> Result<MohConfig, ConfigError>`.
- Produces: `MohConfig::parse(text: &str, path: &Path) -> Result<MohConfig, ConfigError>`.
- Produces: `ServerConfig { idle_timeout: Duration }` with a 900-second default.
- Produces: `LocalPaths::platform_default() -> Result<LocalPaths, LocalPathError>`.
- Produces: injected `PathRoots` for tests without process-global environment mutation.
- Produces: `LocalPaths::prepare_runtime_dir()` and `validate_socket_candidate()`.

- [ ] **Step 1: Add dependencies and Tokio features**

Update `Cargo.toml`:

~~~toml
humantime-serde = "1.1.1"
toml = "1.1.4"

tokio = { version = "1.53.1", features = ["fs", "io-util", "macros", "net", "process", "rt", "signal", "sync", "test-util", "time"] }

[target.'cfg(unix)'.dependencies]
nix = { version = "0.31.3", features = ["process", "signal", "user"] }
~~~

Keep every existing dependency and feature. Run `cargo update` only as required by the edited manifest; do not upgrade unrelated direct dependencies.

- [ ] **Step 2: Write failing config tests**

Create `tests/local_config.rs`:

~~~rust
use std::{path::Path, time::Duration};

use moh::local::{MohConfig, ServerConfig};

#[test]
fn missing_config_uses_fifteen_minute_idle_timeout() {
    assert_eq!(
        ServerConfig::default().idle_timeout,
        Duration::from_secs(15 * 60)
    );
}

#[test]
fn config_parses_human_duration_and_rejects_unknown_keys() {
    let config = MohConfig::parse(
        "[server]\nidle_timeout = \"45s\"\n",
        Path::new("/tmp/config.toml"),
    )
    .unwrap();
    assert_eq!(config.server.idle_timeout, Duration::from_secs(45));

    let error = MohConfig::parse(
        "[server]\nidle_timeout = \"15m\"\nidle_timout = \"1s\"\n",
        Path::new("/tmp/config.toml"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("idle_timout"));
    assert!(error.to_string().contains("/tmp/config.toml"));
}
~~~

Also test zero duration, malformed TOML, and a missing file.

- [ ] **Step 3: Write failing path and permission tests**

Create `tests/local_paths.rs` under `#![cfg(unix)]`. Construct `PathRoots` from a temporary directory and assert:

~~~rust
let paths = LocalPaths::from_roots(PathRoots {
    runtime_dir: Some(root.join("runtime")),
    temp_dir: root.join("tmp"),
    config_dir: root.join("config"),
    state_dir: root.join("state"),
    effective_uid: nix::unistd::Uid::effective().as_raw(),
});
paths.prepare_runtime_dir().unwrap();
assert_eq!(
    std::fs::metadata(paths.runtime_dir())
        .unwrap()
        .permissions()
        .mode()
        & 0o777,
    0o700
);
assert_eq!(paths.socket_path(), paths.runtime_dir().join("backend.sock"));
assert_eq!(paths.spawn_lock_path(), paths.runtime_dir().join("backend.lock"));
assert_eq!(paths.server_log_path(), root.join("state/server.log"));
~~~

Add cases rejecting a symlink runtime directory, a non-socket endpoint, and a socket owned by a different injected UID.

- [ ] **Step 4: Run tests and verify red**

Run:

~~~bash
cargo test --test local_config
cargo test --test local_paths
~~~

Expected: fail because `moh::local` does not exist.

- [ ] **Step 5: Implement configuration types**

Use strict serde defaults:

~~~rust
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MohConfig {
    pub server: ServerConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(with = "humantime_serde")]
    pub idle_timeout: Duration,
}
~~~

`MohConfig::load` returns defaults on `ErrorKind::NotFound`, includes the path in parse/read errors, and rejects `Duration::ZERO` after deserialization.

- [ ] **Step 6: Implement injected and production path resolution**

`PathRoots` must contain exactly the five fields used by the tests. `LocalPaths` stores the resolved runtime directory, socket, spawn lock, config file, state directory, and server log. Production resolution uses `XDG_RUNTIME_DIR` when available and otherwise `std::env::temp_dir().join(format!("moh-{}", Uid::effective().as_raw()))`.

On Unix, use `symlink_metadata`, `MetadataExt::uid`, `FileTypeExt::is_socket`, and `PermissionsExt`. Never delete or chmod a path with unexpected owner or type.

- [ ] **Step 7: Run tests and validation**

Run:

~~~bash
cargo test --test local_config
cargo test --test local_paths
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
~~~

Expected: pass.

- [ ] **Step 8: Commit**

~~~bash
git add Cargo.toml Cargo.lock src/lib.rs src/local tests/local_config.rs tests/local_paths.rs
git commit -m "feat(config): add backend configuration and local paths"
~~~

---

### Task 3: Build the Versioned Session Store

**Files:**
- Modify: `src/lib.rs`
- Create: `src/session/mod.rs`
- Create: `src/session/types.rs`
- Create: `src/session/store.rs`
- Create: `tests/session_store.rs`
- Create: `tests/support/mod.rs`

**Interfaces:**
- Produces: `SessionId` displayed and parsed as `session-N`.
- Produces: validated `SessionName` and `SessionSelector::{Id, Name}`.
- Produces: `SessionSettings`, `SessionRecord`, and `SessionSummary`.
- Produces: object-safe `SessionRepository` returning `BoxFuture<'static, Result<_, SessionStoreError>>`.
- Produces: `SessionStore::open_at(path) -> Result<OpenedSessionStore, SessionStoreError>`.
- Produces: default/create/resolve/load/list/checkpoint/update-metadata operations.
- Produces: `StoreWarning::CorruptDatabaseQuarantined { path }`.

- [ ] **Step 1: Define failing identity and name tests**

In `tests/session_store.rs`, first assert:

~~~rust
#[test]
fn session_ids_and_names_have_unambiguous_namespaces() {
    let id: SessionId = "session-42".parse().unwrap();
    assert_eq!(id.to_string(), "session-42");
    assert!("session-01".parse::<SessionId>().is_err());
    assert!(SessionName::parse("review").is_ok());
    assert!(SessionName::parse("session-7").is_err());
    assert!(SessionName::parse("bad\nname").is_err());
}
~~~

`SessionName` accepts 1–64 Unicode scalar values, rejects leading/trailing whitespace, control characters, and the `session-` prefix.

- [ ] **Step 2: Define failing default/additional-session tests**

Use a temporary database and Unix `OsStrExt::as_bytes`:

~~~rust
#[tokio::test]
async fn store_reuses_default_and_allows_two_sessions_in_one_cwd() {
    let directory = tempfile::tempdir().unwrap();
    let opened = SessionStore::open_at(&directory.path().join("sessions.sqlite"))
        .await
        .unwrap();
    let cwd = directory.path().as_os_str().as_bytes().to_vec();
    let settings = test_settings();

    let default_a = opened
        .store
        .find_or_create_default(cwd.clone(), settings.clone())
        .await
        .unwrap();
    let default_b = opened
        .store
        .find_or_create_default(cwd.clone(), settings.clone())
        .await
        .unwrap();
    let extra = opened
        .store
        .create(cwd.clone(), Some(SessionName::parse("review").unwrap()), settings)
        .await
        .unwrap();

    assert_eq!(default_a.id, default_b.id);
    assert_ne!(default_a.id, extra.id);
    assert_eq!(opened.store.list(cwd).await.unwrap().len(), 2);
}
~~~

- [ ] **Step 3: Define failing checkpoint, restore, and quarantine tests**

Checkpoint a record containing one user/assistant pair, non-default model/reasoning, context usage, and last activity. Reopen and assert exact equality. Then write `b"not sqlite"` to a separate database path and assert `open_at` returns a fresh store plus one quarantine warning whose path exists.

Also inject a checkpoint error through `tests/support::FailingRepository` for Task 5. The fake implements the repository trait with an atomic `fail_checkpoints` switch and an in-memory record map.

- [ ] **Step 4: Run tests and verify red**

Run:

~~~bash
cargo test --test session_store
~~~

Expected: fail because the session module and store types do not exist.

- [ ] **Step 5: Implement domain types and repository interface**

Define the stable types:

~~~rust
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionId(u64);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionSettings {
    pub model: String,
    pub reasoning: ReasoningLevel,
    pub context_tokens: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRecord {
    pub id: SessionId,
    pub name: Option<SessionName>,
    pub cwd: Vec<u8>,
    pub is_default: bool,
    pub settings: SessionSettings,
    pub history: Vec<Message>,
    pub created_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
}
~~~

The repository trait exposes:

~~~rust
pub trait SessionRepository: Send + Sync {
    fn find_or_create_default(
        &self,
        cwd: Vec<u8>,
        settings: SessionSettings,
    ) -> BoxFuture<'static, Result<SessionRecord, SessionStoreError>>;
    fn create(
        &self,
        cwd: Vec<u8>,
        name: Option<SessionName>,
        settings: SessionSettings,
    ) -> BoxFuture<'static, Result<SessionRecord, SessionStoreError>>;
    fn resolve(
        &self,
        selector: SessionSelector,
        cwd_for_name: Vec<u8>,
    ) -> BoxFuture<'static, Result<SessionRecord, SessionStoreError>>;
    fn load(
        &self,
        id: SessionId,
    ) -> BoxFuture<'static, Result<SessionRecord, SessionStoreError>>;
    fn list(
        &self,
        cwd: Vec<u8>,
    ) -> BoxFuture<'static, Result<Vec<SessionSummary>, SessionStoreError>>;
    fn checkpoint(
        &self,
        record: SessionRecord,
    ) -> BoxFuture<'static, Result<(), SessionStoreError>>;
    fn update_metadata(
        &self,
        record: SessionRecord,
    ) -> BoxFuture<'static, Result<(), SessionStoreError>>;
}
~~~

- [ ] **Step 6: Implement schema and serialized blocking access**

Create `sessions` and `messages` exactly as follows:

~~~sql
CREATE TABLE sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT,
    cwd BLOB NOT NULL,
    is_default INTEGER NOT NULL CHECK (is_default IN (0, 1)),
    model TEXT NOT NULL,
    reasoning TEXT NOT NULL,
    context_tokens INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    last_activity TEXT NOT NULL
);
CREATE UNIQUE INDEX one_default_per_cwd
    ON sessions(cwd) WHERE is_default = 1;
CREATE UNIQUE INDEX one_name_per_cwd
    ON sessions(cwd, name) WHERE name IS NOT NULL;
CREATE TABLE messages (
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('user', 'assistant')),
    text TEXT NOT NULL,
    PRIMARY KEY (session_id, position)
);
PRAGMA user_version = 1;
~~~

One `Arc<Mutex<Connection>>` backs the store. Each public operation clones the arc and calls `tools::blocking::run`. `checkpoint` uses an immediate transaction, updates metadata, deletes that session's old message rows, inserts the full ordered history, and commits. `update_metadata` updates only model, reasoning, context usage, and last activity in one statement; add a test proving its message rows remain untouched.

- [ ] **Step 7: Implement corruption quarantine**

Recognize SQLite `DatabaseCorrupt` and `NotADatabase` during initialization, rename the database to `sessions.sqlite.corrupt-<unix-millis>`, recreate schema once, and return `StoreWarning::CorruptDatabaseQuarantined`. Do not quarantine ordinary permission or disk errors.

- [ ] **Step 8: Run store and regression tests**

Run:

~~~bash
cargo test --test session_store
cargo test --test harness
cargo test --test anchor_store
~~~

Expected: pass.

- [ ] **Step 9: Commit**

~~~bash
git add src/lib.rs src/session tests/session_store.rs tests/support/mod.rs
git commit -m "feat(session): persist committed session history"
~~~

---

### Task 4: Define Session Projection and Event Reduction

**Files:**
- Modify: `src/session/mod.rs`
- Modify: `src/session/types.rs`
- Create: `src/session/projection.rs`
- Create: `tests/session_projection.rs`

**Interfaces:**
- Produces: cloneable `RunFailureSnapshot` and `JobSnapshotDto`.
- Produces: `TranscriptItem`, `ActiveRunSnapshot`, `ModelCatalogState`, `SessionSnapshot`.
- Produces: `SessionEventEnvelope { sequence, event }` and typed `SessionEvent`.
- Produces: `SessionProjection::from_record`, `snapshot`, and `apply`.
- Consumes: existing `RunEvent` without exposing provider/Rig types.

- [ ] **Step 1: Write failing projection reduction tests**

Cover this exact event sequence:

~~~rust
let mut projection = SessionProjection::from_record(record, ModelCatalogState::Loading);
projection.apply(SessionEvent::Started {
    run_id: 7,
    prompt: "inspect".into(),
});
projection.apply(SessionEvent::AssistantDelta {
    run_id: 7,
    text: "partial".into(),
});
projection.apply(SessionEvent::ToolStarted {
    run_id: 7,
    call_id: "call-1".into(),
    name: "read".into(),
    arguments: serde_json::json!({"path": "src/main.rs"}),
});
let snapshot = projection.snapshot(vec![]);

assert!(snapshot.busy);
assert_eq!(snapshot.active_run.as_ref().unwrap().assistant_text, "partial");
assert_eq!(snapshot.sequence, 3);
assert!(matches!(
    snapshot.transcript.last().unwrap(),
    TranscriptItem::ToolStarted { name, .. } if name == "read"
));
~~~

Add completion, failure, cancellation, settings, context usage, job, catalog, and persistence-warning cases. Verify completion clears the active projection and adds the final assistant item once.

- [ ] **Step 2: Run test and verify red**

Run:

~~~bash
cargo test --test session_projection
~~~

Expected: fail because projection types do not exist.

- [ ] **Step 3: Implement cloneable transport-neutral DTOs**

`RunFailureSnapshot::from(&RunFailure)` copies stage, kind, retryable, and sanitized message but never the source chain. `JobSnapshotDto::from(&JobSnapshot)` copies IDs, timestamps, title, state, kind, and rendered details.

Define `SessionEvent` with these exact variants:

~~~rust
pub enum SessionEvent {
    Started { run_id: u64, prompt: String },
    AssistantDelta { run_id: u64, text: String },
    ContextUsage { run_id: u64, input_tokens: u64 },
    ToolStarted {
        run_id: u64,
        call_id: String,
        name: String,
        arguments: serde_json::Value,
    },
    ToolFinished { run_id: u64, call_id: String, name: String },
    Completed { run_id: u64, response: String },
    Failed { run_id: u64, failure: RunFailureSnapshot },
    Cancelled { run_id: u64 },
    SettingsChanged(SessionSettings),
    JobsChanged(Vec<JobSnapshotDto>),
    CatalogChanged(ModelCatalogState),
    PersistenceWarning(Option<String>),
}
~~~

- [ ] **Step 4: Implement checked sequence reduction**

`SessionProjection::apply` increments sequence with `checked_add` before mutating. On exhaustion it returns `ProjectionError::SequenceExhausted`. It rejects a run event whose ID differs from the active run and never duplicates committed assistant text.

- [ ] **Step 5: Run tests**

Run:

~~~bash
cargo test --test session_projection
cargo test --test harness
~~~

Expected: pass.

- [ ] **Step 6: Commit**

~~~bash
git add src/session tests/session_projection.rs
git commit -m "feat(session): add authoritative session projection"
~~~

---

### Task 5: Implement the Session Actor

**Files:**
- Modify: `src/session/mod.rs`
- Create: `src/session/actor.rs`
- Create: `src/session/runtime.rs`
- Create: `tests/session_actor.rs`
- Modify: `tests/support/mod.rs`

**Interfaces:**
- Consumes: `Arc<dyn SessionRepository>`, `SessionRecord`, and `SessionProjection`.
- Produces: `SessionEngineFactory` and `SessionEngineBundle<E>`.
- Produces: cloneable `SessionHandle`.
- Produces: `SessionAttachment { snapshot, events }`.
- Produces: submit/cancel/settings/job/attach/detach/flush/shutdown commands.
- Produces: `ConnectionId(u64)`.
- Guarantees: actor never awaits observer delivery; it only uses `try_send`.

- [ ] **Step 1: Add a controlled engine test helper**

In `tests/support/mod.rs`, implement `ControlledEngine` with a request recorder and an `mpsc::UnboundedReceiver<Result<EngineEvent, RunFailure>>` per started stream. Provide:

~~~rust
pub fn controlled_engine() -> (ControlledEngine, ControlledEngineControl);
pub fn engine_bundle(
    engine: ControlledEngine,
    settings: &SessionSettings,
) -> SessionEngineBundle<ControlledEngine>;
~~~

`ControlledEngineControl::emit` sends engine events; `requests` returns captured `RunRequest` values.

- [ ] **Step 2: Write failing detach-without-cancel and observer tests**

In `tests/session_actor.rs`:

~~~rust
#[tokio::test]
async fn detached_actor_keeps_polling_and_reconnects_from_snapshot() {
    let fixture = actor_fixture().await;
    let mut first = fixture.handle.attach(ConnectionId(1)).await.unwrap();
    fixture.handle.submit("continue working".into()).await.unwrap();
    drop(first.events);

    fixture.control.emit(Ok(EngineEvent::AssistantDelta("half".into())));
    tokio::task::yield_now().await;

    let second = fixture.handle.attach(ConnectionId(2)).await.unwrap();
    assert!(second.snapshot.busy);
    assert_eq!(
        second.snapshot.active_run.unwrap().assistant_text,
        "half"
    );
    assert!(fixture.control.requests()[0].history.is_empty());
}
~~~

Add two-observer ordered delivery, busy rejection, explicit cancel, and a completed turn checkpoint test.

- [ ] **Step 3: Write the failing dirty-checkpoint test**

Toggle `FailingRepository::fail_checkpoints(true)` before emitting `Completed("done")`. Assert both `Completed` and `PersistenceWarning(Some(_))` arrive, the actor accepts a later attachment with the completed response, and `flush` succeeds after clearing the failpoint.

- [ ] **Step 4: Run tests and verify red**

Run:

~~~bash
cargo test --test session_actor
~~~

Expected: fail because actor/runtime types do not exist.

- [ ] **Step 5: Define runtime factory and actor handle**

Use these signatures:

~~~rust
pub struct SessionEngineBundle<E> {
    pub engine: E,
    pub active_model: ActiveModel,
    pub active_reasoning: ActiveReasoning,
    pub jobs: JobRegistry,
}

pub trait SessionEngineFactory: Clone + Send + Sync + 'static {
    type Engine: RunEngine;
    fn create(
        &self,
        settings: &SessionSettings,
    ) -> Result<SessionEngineBundle<Self::Engine>, RunFailure>;
}

pub struct SessionAttachment {
    pub snapshot: SessionSnapshot,
    pub events: mpsc::Receiver<SessionEventEnvelope>,
}
~~~

`SessionHandle::attach` creates a bounded channel of 128 events. The actor stores the sender with `ConnectionId`. `detach_connection` removes every sender for that connection.

- [ ] **Step 6: Implement the actor loop**

When running, select between `command_rx.recv()` and `harness.next_event()`. When idle, await commands only. Map every `RunEvent` into `SessionEvent`, reduce the projection, and fan out clones with `try_send`. Remove full or closed observer queues immediately.

After successful completion, copy `harness.history()` into the record, attempt `repository.checkpoint`, emit completion, and emit a warning on error. On settings or context-usage changes, call `repository.update_metadata` without rewriting history. Any store error retains the full record as a dirty idempotent checkpoint. `flush` retries that full record. `shutdown` rejects new submissions, flushes, and shuts the job registry down.

- [ ] **Step 7: Implement command semantics**

- `submit` preserves the current harness prompt validation and rejects running sessions.
- `cancel` maps `HarnessError::NotRunning` to `SessionCommandError::NotRunning`.
- `select_model` validates against a ready catalog when available, changes only future runs, updates metadata, and broadcasts.
- `select_reasoning` accepts only a level advertised for the active model.
- `list_jobs` returns only this actor's registry.
- `cancel_job` starts cancellation in a local task and answers its command response when that task finishes; the actor loop continues reducing harness and job events while cancellation is pending.

- [ ] **Step 8: Run focused tests**

Run:

~~~bash
cargo test --test session_actor
cargo test --test session_projection
cargo test --test harness
cargo test --test job_tool
~~~

Expected: pass.

- [ ] **Step 9: Commit**

~~~bash
git add src/session tests/session_actor.rs tests/support/mod.rs
git commit -m "feat(session): run agents in persistent session actors"
~~~

---

### Task 6: Add the Lazy Multi-Session Manager and Codex Runtime Factory

**Files:**
- Modify: `src/session/mod.rs`
- Modify: `src/session/runtime.rs`
- Create: `src/session/manager.rs`
- Modify: `src/runtime/rig/codex.rs:223-307`
- Create: `tests/session_manager.rs`
- Modify: `tests/rig_runtime.rs`

**Interfaces:**
- Consumes: `Arc<dyn SessionRepository>` and `SessionEngineFactory`.
- Produces: `SessionManagerHandle` with open-default/create/open/list/detach/shutdown methods.
- Produces: `CodexSessionEngineFactory` sharing model transport and durable anchors while isolating session state.
- Guarantees: generated IDs are globally resolved; names are resolved only in the supplied CWD.

- [ ] **Step 1: Write failing manager identity/concurrency tests**

Test:

~~~rust
#[tokio::test]
async fn manager_hosts_default_and_two_independent_same_cwd_sessions() {
    let fixture = manager_fixture().await;
    let default_a = fixture
        .manager
        .open_default(fixture.cwd.clone(), ConnectionId(1))
        .await
        .unwrap();
    let default_b = fixture
        .manager
        .open_default(fixture.cwd.clone(), ConnectionId(2))
        .await
        .unwrap();
    let extra = fixture
        .manager
        .create(
            fixture.cwd.clone(),
            Some(SessionName::parse("review").unwrap()),
            ConnectionId(3),
        )
        .await
        .unwrap();

    assert_eq!(default_a.snapshot.summary.id, default_b.snapshot.summary.id);
    assert_ne!(default_a.snapshot.summary.id, extra.snapshot.summary.id);
}
~~~

Start one controlled run in each distinct session and assert both are busy concurrently. Read a file in the first production-style runtime and assert the second runtime's writer returns `WriteToolError::NotRead`.

- [ ] **Step 2: Write failing connection cleanup and lazy-load tests**

Open two sessions under one `ConnectionId`, call `detach_connection`, then assert both attached counts become zero while their runs remain active. Recreate the manager over the same repository and assert opening by ID loads the actor lazily with stored history.

- [ ] **Step 3: Run tests and verify red**

Run:

~~~bash
cargo test --test session_manager
~~~

Expected: fail because the manager does not exist.

- [ ] **Step 4: Implement a serialized manager command loop**

Use one manager task with:

~~~rust
pub enum OpenRequest {
    Default { cwd: Vec<u8> },
    Create { cwd: Vec<u8>, name: Option<SessionName> },
    Select { selector: SessionSelector, cwd_for_name: Vec<u8> },
}
~~~

For every open, resolve/create through the repository, look up the actor by `SessionId`, spawn it only if absent, and forward `attach`. `list` overlays live actor summaries onto repository summaries. `detach_connection` broadcasts cleanup to every live actor.

- [ ] **Step 5: Implement the production Codex factory**

`CodexSessionEngineFactory` stores one cloneable `CodexModelFactory`, one base `AgentConfig`, and one root `ReadServiceFactory`. For each session:

1. call `root_reads.isolated_session()`;
2. copy persisted model/reasoning into the agent config;
3. create `CodexRunEngine`;
4. capture its `ActiveModel`, `ActiveReasoning`, and `JobRegistry`;
5. return `SessionEngineBundle`.

Add a Rig runtime test proving two bundles get different job registries and observations but share durable anchors.

- [ ] **Step 6: Run tests**

Run:

~~~bash
cargo test --test session_manager
cargo test --test rig_runtime
cargo test --test write_tool
cargo test --test job_tool
~~~

Expected: pass.

- [ ] **Step 7: Commit**

~~~bash
git add src/session src/runtime/rig/codex.rs tests/session_manager.rs tests/rig_runtime.rs
git commit -m "feat(session): manage concurrent backend sessions"
~~~

---

### Task 7: Track Global Activity and Idle Shutdown

**Files:**
- Modify: `src/lib.rs`
- Create: `src/backend/mod.rs`
- Create: `src/backend/activity.rs`
- Modify: `src/session/actor.rs`
- Modify: `src/session/manager.rs`
- Create: `tests/backend_activity.rs`

**Interfaces:**
- Produces: `ActivityTracker` with connection/run/job setters keyed by stable IDs.
- Produces: `ActivitySnapshot { connections, active_runs, running_jobs, generation }`.
- Produces: `wait_for_idle(snapshot_rx, timeout) -> IdleDeadline`.
- Consumes: actor run transitions and `JobRegistry::subscribe_changes`.
- Guarantees: dirty-session flush can veto shutdown.

- [ ] **Step 1: Write paused-clock lifecycle tests**

Create `tests/backend_activity.rs` with `#[tokio::test(start_paused = true)]`. Test the exact production default and reset behavior:

~~~rust
let tracker = ActivityTracker::new();
let waiter = tokio::spawn(wait_for_idle(
    tracker.subscribe(),
    Duration::from_secs(15 * 60),
));
tokio::time::advance(Duration::from_secs(14 * 60)).await;
assert!(!waiter.is_finished());

tracker.set_connection(ConnectionId(1), true);
tracker.set_connection(ConnectionId(1), false);
tokio::time::advance(Duration::from_secs(15 * 60 - 1)).await;
assert!(!waiter.is_finished());
tokio::time::advance(Duration::from_secs(1)).await;
assert!(waiter.await.is_ok());
~~~

Add run and job blockers, a racing new connection, and generation invalidation.

- [ ] **Step 2: Run test and verify red**

Run:

~~~bash
cargo test --test backend_activity
~~~

Expected: fail because backend activity types do not exist.

- [ ] **Step 3: Implement keyed activity accounting**

Store connection IDs, running session IDs, and per-session running-job counts in one mutex. Publish a new `ActivitySnapshot` through `watch` only after a real state change. Use checked generation increments. Idle eligibility is exactly `connections == 0 && active_runs == 0 && running_jobs == 0`.

`wait_for_idle` creates a new sleep whenever eligibility begins or generation changes; it rechecks the snapshot after the sleep fires.

- [ ] **Step 4: Wire run and job transitions**

The session actor calls `set_run(session_id, true)` after accepted submit and clears it after every terminal event. A local job monitor subscribes to registry changes, calls `running_count`, updates `set_running_jobs`, and publishes `JobsChanged` to the actor. Actor shutdown clears both keys.

- [ ] **Step 5: Add shutdown-veto tests**

Use `FailingRepository` to keep a completed actor dirty. After idle deadline, call manager `flush_all` and assert automatic shutdown returns `ShutdownVeto::DirtySessions`. Clear the failpoint, retry, and assert manager/job shutdown succeeds.

- [ ] **Step 6: Run tests**

Run:

~~~bash
cargo test --test backend_activity
cargo test --test session_actor
cargo test --test session_manager
cargo test --test job_tool
~~~

Expected: pass.

- [ ] **Step 7: Commit**

~~~bash
git add src/lib.rs src/backend src/session tests/backend_activity.rs
git commit -m "feat(server): track backend idle lifecycle"
~~~

---

### Task 8: Define and Generate the Cap'n Proto Protocol

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/lib.rs`
- Create: `schema/moh.capnp`
- Create: `scripts/generate-rpc.sh`
- Create: `src/rpc/mod.rs`
- Create: `src/rpc/moh_capnp.rs`
- Create: `src/rpc/convert.rs`
- Create: `tests/rpc_schema.rs`

**Interfaces:**
- Produces: protocol major `1` and minor `0`.
- Produces: generated `backend::Client/Server`, `session::Client/Server`, and `observer::Client/Server`.
- Produces: total conversion functions for every session DTO and result union.
- Consumes: Task 4 session DTOs and Task 5 command errors.

- [ ] **Step 1: Add runtime-only RPC dependencies**

Update `Cargo.toml`:

~~~toml
capnp = "0.27.0"
capnp-rpc = "0.27.0"
tokio-util = { version = "0.7.19", features = ["compat"] }
~~~

Do not add `capnpc` or a build script to the package.

- [ ] **Step 2: Write the source schema**

Create `schema/moh.capnp` with this interface surface and stable ordinals:

~~~capnp
@0x9ea0e1de9de6bd37;

const protocolMajor :UInt16 = 1;
const protocolMinor :UInt16 = 0;

interface Backend {
  getInfo @0 () -> (info :ProtocolInfo);
  openDefault @1 (cwd :Data, observer :Observer) -> (result :OpenResult);
  createSession @2 (cwd :Data, name :Text, observer :Observer)
      -> (result :OpenResult);
  openSession @3 (selector :SessionSelector, cwdForName :Data, observer :Observer)
      -> (result :OpenResult);
  listSessions @4 (cwd :Data) -> (result :SessionListResult);
}

interface Session {
  submit @0 (prompt :Text) -> (result :SubmitResult);
  cancel @1 () -> (result :CommandResult);
  selectModel @2 (modelId :Text) -> (result :CommandResult);
  selectReasoning @3 (level :ReasoningLevel) -> (result :CommandResult);
  listJobs @4 () -> (result :JobListResult);
  cancelJob @5 (jobId :Text) -> (result :JobResult);
}

interface Observer {
  publish @0 (event :EventEnvelope) -> ();
}
~~~

Append these declarations so every wire field and ordinal is fixed before code generation:

~~~capnp
struct ProtocolInfo {
  major @0 :UInt16;
  minor @1 :UInt16;
  instanceId @2 :Text;
  startupWarnings @3 :List(Text);
  features @4 :List(Text);
}

struct SessionSelector {
  union {
    id @0 :Text;
    name @1 :Text;
  }
}

enum ErrorCode {
  busy @0;
  notRunning @1;
  sessionNotFound @2;
  sessionNameConflict @3;
  invalidArgument @4;
  modelNotFound @5;
  unsupportedReasoning @6;
  jobNotFound @7;
  backendStarting @8;
  backendUnavailable @9;
  persistence @10;
  internal @11;
}

struct CommandError {
  code @0 :ErrorCode;
  message @1 :Text;
}

struct OpenResult {
  union {
    success @0 :OpenSuccess;
    error @1 :CommandError;
  }
}

struct OpenSuccess {
  session @0 :Session;
  snapshot @1 :SessionSnapshot;
}

struct SessionListResult {
  union {
    sessions @0 :List(SessionSummary);
    error @1 :CommandError;
  }
}

struct SubmitResult {
  union {
    runId @0 :UInt64;
    error @1 :CommandError;
  }
}

struct CommandResult {
  union {
    ok @0 :Void;
    error @1 :CommandError;
  }
}

struct JobListResult {
  union {
    jobs @0 :List(JobSnapshot);
    error @1 :CommandError;
  }
}

struct JobResult {
  union {
    job @0 :JobSnapshot;
    error @1 :CommandError;
  }
}

enum ReasoningLevel {
  none @0;
  minimal @1;
  low @2;
  medium @3;
  high @4;
  xhigh @5;
  max @6;
}

struct SessionSettings {
  model @0 :Text;
  reasoning @1 :ReasoningLevel;
  contextTokens @2 :UInt64;
}

struct SessionSummary {
  id @0 :Text;
  name @1 :Text;
  cwd @2 :Data;
  cwdDisplay @3 :Text;
  isDefault @4 :Bool;
  busy @5 :Bool;
  attachedClients @6 :UInt32;
  lastActivity @7 :Text;
}

struct ActiveRun {
  runId @0 :UInt64;
  prompt @1 :Text;
  assistantText @2 :Text;
}

struct ToolStartedRecord {
  runId @0 :UInt64;
  callId @1 :Text;
  name @2 :Text;
  argumentsJson @3 :Text;
}

struct FailedRecord {
  runId @0 :UInt64;
  failure @1 :RunFailure;
}

struct TranscriptItem {
  union {
    user @0 :Text;
    assistant @1 :Text;
    toolStarted @2 :ToolStartedRecord;
    failed @3 :FailedRecord;
    cancelledRunId @4 :UInt64;
  }
}

struct ModelInfo {
  id @0 :Text;
  displayName @1 :Text;
  description @2 :Text;
  reasoningEfforts @3 :List(ReasoningLevel);
  hasDefaultReasoning @4 :Bool;
  defaultReasoning @5 :ReasoningLevel;
}

struct ModelCatalog {
  union {
    loading @0 :Void;
    ready @1 :List(ModelInfo);
    failed @2 :Text;
  }
}

enum JobKind {
  bash @0;
}

enum JobState {
  running @0;
  completed @1;
  failed @2;
  cancelled @3;
}

struct JobSnapshot {
  id @0 :Text;
  kind @1 :JobKind;
  state @2 :JobState;
  title @3 :Text;
  startedAt @4 :Text;
  completedAt @5 :Text;
  details @6 :Text;
}

enum RunStage {
  startup @0;
  modelRequest @1;
  toolExecution @2;
  finalization @3;
}

enum RunFailureKind {
  authentication @0;
  transport @1;
  httpRejected @2;
  protocol @3;
  emptyResponse @4;
  budgetExhausted @5;
  runtimeInfrastructure @6;
  toolInfrastructure @7;
}

struct RunFailure {
  stage @0 :RunStage;
  kind @1 :RunFailureKind;
  hasHttpStatus @2 :Bool;
  httpStatus @3 :UInt16;
  retryable @4 :Bool;
  message @5 :Text;
}

struct SessionSnapshot {
  summary @0 :SessionSummary;
  transcript @1 :List(TranscriptItem);
  activeRun @2 :ActiveRun;
  settings @3 :SessionSettings;
  catalog @4 :ModelCatalog;
  jobs @5 :List(JobSnapshot);
  persistenceWarning @6 :Text;
  sequence @7 :UInt64;
  busy @8 :Bool;
}

struct RunStarted {
  runId @0 :UInt64;
  prompt @1 :Text;
}

struct AssistantDelta {
  runId @0 :UInt64;
  text @1 :Text;
}

struct ContextUsage {
  runId @0 :UInt64;
  inputTokens @1 :UInt64;
}

struct ToolFinished {
  runId @0 :UInt64;
  callId @1 :Text;
  name @2 :Text;
}

struct RunCompleted {
  runId @0 :UInt64;
  response @1 :Text;
}

struct RunFailed {
  runId @0 :UInt64;
  failure @1 :RunFailure;
}

struct EventEnvelope {
  sequence @0 :UInt64;
  union {
    started @1 :RunStarted;
    assistantDelta @2 :AssistantDelta;
    contextUsage @3 :ContextUsage;
    toolStarted @4 :ToolStartedRecord;
    toolFinished @5 :ToolFinished;
    completed @6 :RunCompleted;
    failed @7 :RunFailed;
    cancelledRunId @8 :UInt64;
    settingsChanged @9 :SessionSettings;
    jobsChanged @10 :List(JobSnapshot);
    catalogChanged @11 :ModelCatalog;
    persistenceWarning @12 :Text;
  }
}
~~~

Timestamps are UTC RFC 3339 text. A null `activeRun` pointer means no active run. Empty optional text means absent only for schema fields explicitly documented as optional (`name`, completion timestamp, persistence warning). `hasDefaultReasoning` and `hasHttpStatus` guard their scalar fields. Conversion tests must enforce each convention.

- [ ] **Step 3: Add the generator script**

`scripts/generate-rpc.sh` must:

~~~bash
#!/usr/bin/env bash
set -euo pipefail

command -v capnp >/dev/null || {
  echo "capnp compiler is required to regenerate RPC bindings" >&2
  exit 1
}
command -v capnpc-rust >/dev/null || {
  echo "install capnpc-rust with: cargo install capnpc --version 0.27.0 --locked" >&2
  exit 1
}

capnp compile --src-prefix=schema -orust:src/rpc schema/moh.capnp
rustfmt --edition 2024 src/rpc/moh_capnp.rs
~~~

Use Cap'n Proto compiler 1.5.0 and `capnpc-rust` 0.27.0 for checked-in generation. Install those code-generation tools for this task, run the script, and check in the resulting `src/rpc/moh_capnp.rs`. Record `capnp --version`, then assert the generated header says `capnp binary version: 1.5.0` and `capnpc crate version: 0.27.0`; the plugin itself does not implement a version flag. Ordinary Cargo commands must not invoke the script.

- [ ] **Step 4: Write failing conversion round-trip tests**

In `tests/rpc_schema.rs`, build Cap'n Proto messages for a full snapshot and every event variant, decode them, and assert equality with the domain DTO. Include non-UTF-8 CWD bytes, every reasoning level, HTTP failure status, nullable name/timestamp/warning, and invalid enum handling.

- [ ] **Step 5: Run tests and verify red**

Run:

~~~bash
cargo test --test rpc_schema
~~~

Expected: fail because conversion functions do not exist.

- [ ] **Step 6: Implement conversions**

`src/rpc/convert.rs` contains named `write_*` and `read_*` functions for every schema/domain pair. Convert `serde_json::Value` tool arguments through UTF-8 JSON text and map invalid JSON to `RpcConversionError::InvalidToolArguments`. Use checked integer conversions and never panic on unknown future enum values.

- [ ] **Step 7: Prove checked-in generation**

Run:

~~~bash
cp src/rpc/moh_capnp.rs /tmp/moh_capnp.rs.before
scripts/generate-rpc.sh
diff -u /tmp/moh_capnp.rs.before src/rpc/moh_capnp.rs
cargo clean
cargo build --locked
cargo test --test rpc_schema
~~~

Expected: regeneration diff is empty; clean build and tests pass without invoking `capnp` from Cargo.

- [ ] **Step 8: Commit**

~~~bash
git add Cargo.toml Cargo.lock schema scripts/generate-rpc.sh src/lib.rs src/rpc tests/rpc_schema.rs
git commit -m "feat(rpc): define Cap'n Proto session protocol"
~~~

---

### Task 9: Serve Sessions over Cap'n Proto

**Files:**
- Modify: `src/rpc/mod.rs`
- Create: `src/rpc/server.rs`
- Create: `tests/rpc_transport.rs`
- Modify: `tests/support/mod.rs`

**Interfaces:**
- Consumes: `SessionManagerHandle`, `ActivityTracker`, generated server traits.
- Produces: `serve_connection(UnixStream, ConnectionId, BackendContext)`.
- Produces: per-connection `BackendImpl` and per-attachment `SessionImpl`.
- Produces: bounded observer pump from actor event receiver to remote `Observer::publish`.
- Guarantees: RPC-system completion detaches every observer for its connection.

- [ ] **Step 1: Write a failing real-Unix-stream handshake test**

Under `#![cfg(unix)]`, create `tokio::net::UnixStream::pair()`. Start `serve_connection` on one side and a raw Cap'n Proto client on the other. Call `getInfo` and assert major 1/minor 0 and the fixture instance ID.

- [ ] **Step 2: Write failing open/callback tests**

Call `openDefault` with a locally implemented generated `observer::Server`. Submit through the returned session capability, emit controlled engine delta/completion, and assert the observer receives strictly increasing converted events. Drop the client RPC system and assert the manager reports zero attached clients while the run remains active.

- [ ] **Step 3: Run tests and verify red**

Run:

~~~bash
cargo test --test rpc_transport
~~~

Expected: fail because `rpc::server` does not exist.

- [ ] **Step 4: Implement connection setup**

Use the official two-party pattern:

~~~rust
let (reader, writer) =
    tokio_util::compat::TokioAsyncReadCompatExt::compat(stream).split();
let network = capnp_rpc::twoparty::VatNetwork::new(
    futures::io::BufReader::new(reader),
    futures::io::BufWriter::new(writer),
    capnp_rpc::rpc_twoparty_capnp::Side::Server,
    Default::default(),
);
let bootstrap: moh_capnp::backend::Client =
    capnp_rpc::new_client(BackendImpl::new(context, connection_id));
let rpc = capnp_rpc::RpcSystem::new(Box::new(network), Some(bootstrap.client));
~~~

Drive `rpc` on `spawn_local`. In a completion guard, call `manager.detach_connection(connection_id)` and `activity.set_connection(connection_id, false)`.

- [ ] **Step 5: Implement generated server traits**

Each generated method decodes parameters, awaits exactly one manager/session command, and fills the matching result union. It converts domain errors rather than returning Cap'n Proto failure for ordinary outcomes.

The observer pump reads `SessionEventEnvelope` from the actor attachment and sequentially awaits remote `publish` calls. Callback failure ends only that pump.

- [ ] **Step 6: Test slow and failed observers**

Use one observer whose `publish` promise remains pending and a second observer that records. Emit more than 128 events; assert the actor detaches the slow observer while the recording observer and run continue.

- [ ] **Step 7: Run tests**

Run:

~~~bash
cargo test --test rpc_transport
cargo test --test session_actor
cargo test --test session_manager
~~~

Expected: pass.

- [ ] **Step 8: Commit**

~~~bash
git add src/rpc tests/rpc_transport.rs tests/support/mod.rs
git commit -m "feat(rpc): serve backend sessions"
~~~

---

### Task 10: Add the Typed RPC Client

**Files:**
- Modify: `src/rpc/mod.rs`
- Create: `src/rpc/client.rs`
- Modify: `tests/rpc_transport.rs`

**Interfaces:**
- Produces: `RpcBackendClient::connect(UnixStream)`.
- Produces: get-info, open-default, create, open-selector, and list methods.
- Produces: `RpcSessionClient` with snapshot, event, submit, cancel, settings, and job methods.
- Produces: `SessionUpdate::{Event, SnapshotReplaced}`.
- Guarantees: incompatible major is rejected before open; sequence gaps trigger a fresh attach snapshot.

- [ ] **Step 1: Write failing typed-client tests**

Replace raw client calls in one transport test with:

~~~rust
let backend = RpcBackendClient::connect(client_stream).await.unwrap();
assert_eq!(backend.info().protocol_major, 1);
let mut session = backend
    .open_default(cwd.clone())
    .await
    .unwrap();
session.submit("hello".into()).await.unwrap();
assert!(matches!(
    session.next_update().await.unwrap(),
    SessionUpdate::Event(SessionEventEnvelope {
        event: SessionEvent::Started { .. },
        ..
    })
));
~~~

Add create/open/list, typed busy error, and every settings/job command.

- [ ] **Step 2: Write incompatible-version and gap tests**

Use a fixture server reporting major 2 and assert `RpcClientError::IncompatibleProtocol { client: 1, server: 2 }`. Inject event sequences 4 then 6; assert the client reattaches by ID and returns `SnapshotReplaced` instead of exposing sequence 6.

- [ ] **Step 3: Run tests and verify red**

Run:

~~~bash
cargo test --test rpc_transport typed_client
~~~

Expected: fail because the typed client does not exist.

- [ ] **Step 4: Implement client connection and local observer**

Create the client-side two-party `RpcSystem` with `Side::Client`, bootstrap `backend::Client`, and drive it on `spawn_local`. A local generated `ObserverImpl` converts callbacks and sends them to an internal bounded channel.

`RpcBackendClient` stores the root capability, protocol info, connection task, and a monotonically allocated local attachment ID. `RpcSessionClient` stores the session capability, selector needed for reattachment, observer capability, current snapshot, expected sequence, and event receiver.

- [ ] **Step 5: Implement command/result conversion**

Every method returns domain DTOs or `RpcClientError::Command(SessionCommandError)`. Preserve backend messages only after terminal sanitization at the TUI boundary. Connection errors retain no request payloads.

- [ ] **Step 6: Implement sequence recovery**

Ignore events with `sequence <= snapshot.sequence`. Accept exactly `expected + 1`. On a larger sequence, call `openSession` by stable ID with a fresh observer, replace snapshot and receiver, and return `SessionUpdate::SnapshotReplaced`.

- [ ] **Step 7: Run tests**

Run:

~~~bash
cargo test --test rpc_transport
cargo test --test rpc_schema
~~~

Expected: pass.

- [ ] **Step 8: Commit**

~~~bash
git add src/rpc tests/rpc_transport.rs
git commit -m "feat(rpc): add typed session client"
~~~

---

### Task 11: Compose the Backend and Safe Connect-or-Spawn Flow

**Files:**
- Modify: `src/backend/mod.rs`
- Create: `src/backend/server.rs`
- Create: `src/local/launch.rs`
- Create: `src/server.rs`
- Create: `tests/local_launch.rs`
- Modify: `tests/support/mod.rs`

**Interfaces:**
- Produces: `BackendOptions<F> { paths, config, runtime_factory: F, repository }`.
- Produces: `run_backend(options) -> Result<ShutdownReason, BackendError>`.
- Produces: `connect_or_spawn(paths, BackendCommand) -> RpcBackendClient`.
- Produces: foreground and detached server launch commands.
- Consumes: RPC server, manager, activity tracker, session store, config.

- [ ] **Step 1: Write failing stale-socket and startup-lock tests**

In `tests/local_launch.rs`, use injected `LocalPaths`. Test:

- an existing reachable listener is reused without spawning;
- a stale owner-matching socket is removed;
- a regular file or symlink at the endpoint is rejected and remains untouched;
- two concurrent callers invoke the injectable spawn closure exactly once.

Use an atomic spawn count and bounded readiness channel; do not sleep.

- [ ] **Step 2: Write failing backend-idle integration test**

Start `run_backend` with a fake runtime factory, temporary store, one-second idle config, and temporary socket. Connect, open a session, disconnect, advance paused time, and assert the backend removes the socket and returns `ShutdownReason::Idle`. Keep a controlled run active and assert advancing time does not stop it.

- [ ] **Step 3: Run tests and verify red**

Run:

~~~bash
cargo test --test local_launch
~~~

Expected: fail because launch/backend composition does not exist.

- [ ] **Step 4: Implement connect-or-spawn**

Use `std::fs::File::try_lock` on the exact spawn-lock path. After lock acquisition, reconnect before stale cleanup. Validate ownership/type through `LocalPaths`. Spawn through an injected `BackendCommand` so tests select the current test executable and production selects `std::env::current_exe()` with `server --internal-detached`.

Retry `RpcBackendClient::connect` with a bounded five-second deadline and 25ms async intervals, and return only the client that completed a compatible protocol handshake. Report socket, lock, and log paths on failure.

- [ ] **Step 5: Implement detached Unix launch**

Production detached launch redirects stdin to null and stdout/stderr to an append-only private server log. Use `CommandExt::pre_exec` with a documented safety comment and `nix::unistd::setsid()`. Do not inherit terminal handles.

- [ ] **Step 6: Implement backend composition**

`run_backend` performs this order:

1. prepare paths and load/open the session store;
2. bind the Unix listener and set socket mode;
3. create activity tracking and the manager;
4. begin Codex runtime/catalog initialization without blocking `accept`;
5. accept connections, assigning checked `ConnectionId` values;
6. select between accepts, runtime state, signals, and idle deadline;
7. on idle, flush dirty actors and abort shutdown if flush fails;
8. shutdown actors/jobs, drain `AuthFile::drain_pending_refreshes`, close RPC tasks/store, and unlink the exact socket.

Before model runtime is ready, session opens return typed `backendStarting`; the client retries until ready or startup failure.

- [ ] **Step 7: Run tests**

Run:

~~~bash
cargo test --test local_launch
cargo test --test backend_activity
cargo test --test rpc_transport
~~~

Expected: pass.

- [ ] **Step 8: Commit**

~~~bash
git add src/backend src/local/launch.rs src/server.rs tests/local_launch.rs tests/support/mod.rs
git commit -m "feat(server): auto-start the local backend"
~~~

---

### Task 12: Add CLI Mode and Session Selection

**Files:**
- Create: `src/cli.rs`
- Modify: `src/lib.rs`
- Create: `tests/cli.rs`

**Interfaces:**
- Produces: `CliMode::{Default, New { name }, Session { selector }, Sessions, Server { detached }}`.
- Produces: deterministic parse errors and usage.
- Consumes: session name, ID, and selector types only.
- Defers: binary dispatch until Task 13, when every attach path is RPC-backed.

- [ ] **Step 1: Write failing table-driven parser tests**

Cover exactly:

~~~rust
assert_eq!(parse(["moh"]), Ok(CliMode::Default));
assert_eq!(
    parse(["moh", "--new"]),
    Ok(CliMode::New { name: None })
);
assert_eq!(
    parse(["moh", "--new", "review"]),
    Ok(CliMode::New {
        name: Some(SessionName::parse("review").unwrap())
    })
);
assert_eq!(
    parse(["moh", "--session", "session-7"]),
    Ok(CliMode::Session {
        selector: SessionSelector::Id("session-7".parse().unwrap())
    })
);
assert_eq!(parse(["moh", "sessions"]), Ok(CliMode::Sessions));
assert_eq!(
    parse(["moh", "server"]),
    Ok(CliMode::Server { detached: false })
);
~~~

Reject missing selector, extra arguments, conflicting modes, invalid names, and direct user spelling of `--internal-detached` unless it follows `server`.

- [ ] **Step 2: Run tests and verify red**

Run:

~~~bash
cargo test --test cli
~~~

Expected: fail because parser/modes do not exist.

- [ ] **Step 3: Implement dependency-free parsing**

Parse `OsString` values without Clap. Session names require Unicode and therefore report a clear error for non-Unicode name arguments; generated IDs remain ASCII. Usage lists every supported form.

- [ ] **Step 4: Run parser tests**

Run:

~~~bash
cargo test --test cli
~~~

Expected: pass without changing the existing binary entry point.

- [ ] **Step 5: Commit**

~~~bash
git add src/cli.rs src/lib.rs tests/cli.rs
git commit -m "feat(cli): define backend session commands"
~~~

---

### Task 13: Migrate the TUI to the Session Client

**Files:**
- Move: `src/app.rs` to `src/client/app.rs`
- Create: `src/client/mod.rs`
- Create: `src/client/session.rs`
- Modify: `src/main.rs`
- Modify: `src/server.rs`

**Interfaces:**
- Produces: binary-private async `SessionClient` trait implemented by an RPC adapter and test fake.
- Consumes: `SessionSnapshot` and `SessionUpdate`.
- Removes: direct client ownership of `Harness`, `CodexRunEngine`, `ActiveModel`, `ActiveReasoning`, and `JobRegistry`.
- Preserves: existing TUI rendering/input behavior except approved detach/cancel controls.

- [ ] **Step 1: Introduce a fakeable TUI session boundary**

Define in `src/client/session.rs`:

~~~rust
pub(super) trait SessionClient {
    fn snapshot(&self) -> &SessionSnapshot;
    async fn next_update(&mut self) -> Result<SessionUpdate, ClientSessionError>;
    async fn submit(&self, prompt: String) -> Result<u64, ClientSessionError>;
    async fn cancel(&self) -> Result<(), ClientSessionError>;
    async fn select_model(&self, model: String) -> Result<(), ClientSessionError>;
    async fn select_reasoning(
        &self,
        reasoning: ReasoningLevel,
    ) -> Result<(), ClientSessionError>;
    async fn list_jobs(&self) -> Result<Vec<JobSnapshotDto>, ClientSessionError>;
    async fn cancel_job(&self, id: String) -> Result<JobSnapshotDto, ClientSessionError>;
}
~~~

Implement it for a thin wrapper around `RpcSessionClient`. Add a scripted fake to the existing `app.rs` test module before changing the loop.

- [ ] **Step 2: Write failing snapshot reconstruction tests**

Move `src/app.rs` to `src/client/app.rs` with history-preserving `git mv`. Add tests that build from a snapshot containing two committed turns, an active prompt, partial assistant text, one tool item, settings, context usage, and one running job. Assert transcript/status exactly reflect the snapshot before observer events.

- [ ] **Step 3: Write failing control tests**

Replace old cancellation-on-exit expectations with:

~~~rust
#[tokio::test]
async fn control_c_detaches_without_cancelling_remote_run() {
    let client = ScriptedSessionClient::busy();
    run_client_with_events(client.clone(), [control_c()]).await.unwrap();
    assert_eq!(client.cancel_count(), 0);
}

#[tokio::test]
async fn escape_cancels_and_keeps_the_client_open() {
    let client = ScriptedSessionClient::busy_then_idle();
    run_client_with_events(client.clone(), [escape(), control_c()])
        .await
        .unwrap();
    assert_eq!(client.cancel_count(), 1);
}
~~~

Add `/quit` detach and `/cancel` cancel tests.

- [ ] **Step 4: Run binary tests and verify red**

Run:

~~~bash
cargo test --bin moh
~~~

Expected: the new snapshot/control tests fail while old direct-harness paths still exist.

- [ ] **Step 5: Replace direct harness polling with client updates**

`run_event_loop` derives busy state from the local snapshot projection. It selects between terminal events and `SessionClient::next_update`. `SessionUpdate::Event` reduces locally; `SnapshotReplaced` replaces the projection and rebuilds application components without duplicating transcript entries.

Remove `cancel_active_run` from shutdown. Add `AppAction::Cancel` for Escape while busy and `/cancel`. Ctrl+C and `/quit` return from the loop without an RPC cancel.

- [ ] **Step 6: Migrate settings and job commands**

Replace `AppIds` fields holding `ActiveModel`, `ActiveReasoning`, and `JobRegistry` with local snapshot DTOs. Model/effort selection awaits RPC and updates only from authoritative events. `/ps` awaits `list_jobs`; `/kill` awaits `cancel_job`.

Keep fuzzy matching and overlay rendering local by consuming `ModelCatalogState::Ready`. Preserve visible catalog failure without blocking default model submissions.

- [ ] **Step 7: Complete binary dispatch and remove in-process composition**

Delete client-side construction of `CodexModelFactory`, `ReadServiceFactory`, `CodexRunEngine`, `Harness`, and job shutdown. These remain only in backend composition. Client runtime teardown only restores the terminal and closes RPC.

`main` resolves paths/config once and dispatches server modes to `server::run`, `sessions` to a non-interactive RPC list, and attach modes to `client::run` after `connect_or_spawn`. The table orders default first, then last activity descending, and prints ID, optional name, state, client count, and last activity. On non-Unix, client/server modes return `moh: local backend transport is not supported on this platform` and a nonzero status.

- [ ] **Step 8: Run focused application regression tests**

Run:

~~~bash
cargo test --bin moh
cargo test --test cli
cargo test --test components
cargo test --test renderer
cargo test --test terminal
cargo test --test text_layout
cargo test --test tui
~~~

Expected: all pass with the approved control changes.

- [ ] **Step 9: Commit**

~~~bash
git add -A src/app.rs src/client src/main.rs src/server.rs
git commit -m "refactor(app): drive the TUI through backend sessions"
~~~

---

### Task 14: Add End-to-End Client/Server Regression Coverage

**Files:**
- Create: `tests/client_server.rs`
- Modify: `tests/support/mod.rs`
- Modify: `src/local/launch.rs`
- Modify: `src/backend/server.rs`

**Interfaces:**
- Consumes: injectable `BackendCommand` and `BackendOptions<ControlledEngineFactory>`.
- Produces: a child-test entry using the current test executable, not a shipped fixture binary.
- Verifies: full socket/RPC/session/store/lifecycle behavior without real Codex credentials.

- [ ] **Step 1: Add the child-test server entry**

Use the standard current-test-executable pattern. An ignored test named `child_backend_entry` reads temporary paths and a script from environment variables, constructs a controlled runtime factory, runs `run_backend`, and exits. `BackendCommand` for tests launches:

~~~rust
Command::new(std::env::current_exe().unwrap())
    .args([
        "--exact",
        "child_backend_entry",
        "--ignored",
        "--nocapture",
    ])
~~~

The production launcher still launches `moh server --internal-detached`.

- [ ] **Step 2: Write the detach/reattach end-to-end test**

Connect through `connect_or_spawn`, open default, submit, drop the client, instruct the child engine to emit delta/completion through the fixture script, reconnect by session ID, and assert the snapshot contains the completed committed turn. Verify the child remained the same backend instance.

- [ ] **Step 3: Write same-CWD concurrency and multi-client tests**

Create two sessions in one CWD, submit controlled runs to both, and assert both busy with distinct run histories and job lists. Attach two clients to one session and assert they receive identical ordered completion events.

- [ ] **Step 4: Write restart persistence test**

Complete a turn, explicitly stop the child backend, start a new child with the same state path, and assert history/model/reasoning/context restore. Assert tool, failed, cancelled, and active-run projection records are absent.

- [ ] **Step 5: Write spawn-race and idle-shutdown tests**

Launch eight concurrent `connect_or_spawn` calls and assert one backend instance ID. Then disconnect all clients, use a 100ms injected idle timeout, and assert the socket disappears only after controlled runs/jobs finish.

- [ ] **Step 6: Run tests and diagnose any real race**

Run:

~~~bash
cargo test --test client_server -- --test-threads=1
cargo test --test local_launch
cargo test --test rpc_transport
~~~

Expected: pass without arbitrary sleeps; use readiness/watch notifications and Tokio deadlines only.

- [ ] **Step 7: Commit**

~~~bash
git add src/local/launch.rs src/backend/server.rs tests/client_server.rs tests/support/mod.rs
git commit -m "test: cover client-server session lifecycle"
~~~

---

### Task 15: Update Documentation and Run Full Verification

**Files:**
- Modify: `README.md`
- Modify: `docs/superpowers/specs/2026-08-26-client-server-architecture-design.md` only if implementation discovered an approved factual correction.
- Modify: `.github/workflows/ci.yml` only if RPC binding-regeneration verification is added as a separate opt-in job; ordinary validation must not install `capnp`.

**Interfaces:**
- Documents: global backend, sessions, persistence boundary, config, controls, paths, and diagnostics.
- Verifies: every acceptance criterion and ordinary validation route.

- [ ] **Step 1: Update README architecture and usage**

Document these exact commands and semantics:

~~~text
moh
moh --new
moh --new review
moh --session session-2
moh --session review
moh sessions
moh server
~~~

State that Ctrl+C and `/quit` detach, Escape and `/cancel` cancel, the default idle timeout is 15 minutes, and successful turns/settings persist while active work and jobs do not survive backend death.

Add the config example:

~~~toml
[server]
idle_timeout = "15m"
~~~

Document platform config/state/runtime paths, `server.log`, Unix-only transport, and `scripts/generate-rpc.sh` as a maintainer command.

- [ ] **Step 2: Run focused acceptance checks**

Run:

~~~bash
cargo test --test session_store
cargo test --test session_actor
cargo test --test session_manager
cargo test --test backend_activity
cargo test --test rpc_schema
cargo test --test rpc_transport
cargo test --test local_launch
cargo test --test client_server -- --test-threads=1
cargo test --bin moh
~~~

Expected: all pass.

- [ ] **Step 3: Run the complete validation route**

Run fresh and read every exit status:

~~~bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --locked
git diff --check
~~~

Expected: zero formatting diffs, zero Clippy warnings, all non-live tests pass, locked build succeeds, and diff check is clean.

- [ ] **Step 4: Manually smoke-test local detach behavior**

From a temporary repository with valid Codex credentials:

~~~bash
cargo run -- --new smoke
~~~

Submit a prompt that runs a bounded background command, exit with Ctrl+C, run `cargo run -- --session smoke`, and verify the session reports the continuing/completed work. Then run:

~~~bash
cargo run -- sessions
~~~

Verify the named session, state, and last activity are correct. Do not use the opt-in live model test as a substitute for automated verification.

- [ ] **Step 5: Review scope and status**

Run:

~~~bash
git status --short
git diff --stat origin/main...HEAD
git log --oneline origin/main..HEAD
~~~

Confirm every changed file belongs to issue #26, every task has one focused conventional commit, and no generated/tool output or credentials are untracked.

- [ ] **Step 6: Commit documentation**

~~~bash
git add README.md .github/workflows/ci.yml docs/superpowers/specs/2026-08-26-client-server-architecture-design.md
git diff --cached --check
git commit -m "docs: document persistent backend sessions"
~~~

Stage only files that actually changed; omit unchanged paths from `git add`.
