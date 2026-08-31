# Background Jobs and Bash Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a non-interactive Bash tool with foreground and background execution, backed by reusable job status, waiting, cancellation, retention, and shutdown infrastructure.

**Architecture:** A process-local `JobRegistry` owns generic execution identity and lifecycle state, while producer-owned `JobDetails` render type-specific results. `BashService` registers every command as a job, supervises the process and output asynchronously, and either awaits it or returns immediately; thin Rig adapters expose Bash and three generic lifecycle tools.

**Tech Stack:** Rust 2024, Tokio 1.53 (`fs`, `io-util`, `macros`, `process`, `rt`, `sync`, `time`), Futures 0.3, tempfile 3.27, Rig 0.41, Nix 0.31.3 (`signal`, Unix only).

**Spec:** `docs/superpowers/specs/2026-08-25-background-jobs-and-bash-design.md`

## Global Constraints

- A job is one execution. A future persistent `agent_id` must remain distinct from each subagent-turn `job_id`.
- `JobRegistry` is process-local, shared for the engine lifetime, visible across prompt turns, and not persisted across application restarts.
- Use exactly four states: `running`, `completed`, `failed`, and `cancelled`. A nonzero Bash exit is `completed`, while timeout is `failed`.
- Permit at most 16 running jobs and retain at most 64 terminal jobs. Never evict a running job; evict the oldest terminal entry first.
- Run `bash -lc <command>` from `RunContext.cwd`, inherit the environment, set stdin to null, and pipe stdout and stderr. Do not add a PTY or interactive input.
- `bash.background` defaults to `false`. Omitted `timeout_ms` means no timeout; accepted explicit values are `1..=3_600_000` milliseconds.
- Cap model-facing Bash output at 50 KiB or 2,000 lines and preserve full, source-labelled output in a user-private temporary log while the terminal job is retained.
- Foreground cancellation terminates the job. A successfully started background job survives cancellation or completion of the originating agent run.
- On Unix, terminate the complete process group. Elsewhere, provide best-effort direct-child termination.
- `job_wait` defaults to 30 seconds and rejects values greater than 300,000 milliseconds. Its wait must be notification-driven, not a timer-based polling loop.
- `job_cancel` is idempotent and uses a two-second terminate-then-force-kill path for Bash.
- Application shutdown must await job cancellation before the Tokio runtime is dropped.
- Do not inject unsolicited completion messages or wake idle conversations in this implementation.
- Preserve current read, edit, write, auth, harness-history, model, and Codex transport behavior.
- Every new public item needs rustdoc because the crate enables `#![warn(missing_docs)]`.
- Use conventional commits and stage only files named by the current task.
- Validate the completed feature with `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-targets`, and `cargo build --locked`.

---

## File Structure

- `src/tools/job.rs` — generic job IDs, states, snapshots, producer detail interface, registry, lifecycle service, strict lifecycle schemas, retention, waiting, cancellation, and shutdown.
- `src/tools/bash.rs` — cwd-bound Bash service, process supervisor, output accumulator and temporary log, timeout/cancellation behavior, and Bash schema.
- `src/tools/mod.rs` — exports the job and Bash service APIs.
- `src/runtime/rig/job_tool.rs` — the `job_status`, `job_wait`, and `job_cancel` PortableTool adapters with one shared runtime error code.
- `src/runtime/rig/bash_tool.rs` — the `bash` PortableTool adapter and Bash runtime error projection.
- `src/runtime/rig/mod.rs` — exports the new adapters.
- `src/runtime/rig/codex.rs` — owns one registry, creates cwd-bound Bash services, registers seven tools, exposes a shutdown handle, and treats job/Bash infrastructure failures as fatal.
- `src/main.rs` and `src/app.rs` — retain the registry handle and await shutdown after terminal cleanup, with error projection through `AppError`.
- `tests/job_tool.rs` — generic registry and lifecycle-tool behavior.
- `tests/bash_tool.rs` — process execution, output, timeout, cancellation, process-tree, retention-log, and shutdown behavior.
- `tests/rig_runtime.rs` — model-visible schemas, Bash/lifecycle tool loops, cancellation, and fatal runtime projection.
- `Cargo.toml` and `Cargo.lock` — Tokio process/I/O features and safe Unix process-group signalling.
- `README.md` — user-facing Bash and background-job behavior and limitations.

## Task 1: Build the generic job registry and lifecycle services

**Files:**
- Create: `src/tools/job.rs`
- Modify: `src/tools/mod.rs:3-13`
- Create: `tests/job_tool.rs`

**Interfaces:**
- Produces `JobId`, `JobKind`, `JobState`, `JobDetails`, `JobSnapshot`, `JobRegistry`, `JobLease`, `JobUpdater`, and `JobRegistryError`.
- Produces `JobStatusArgs`, `JobWaitArgs`, `JobCancelArgs`, `JobService`, and `JobToolError`.
- `JobRegistry::start(kind, title, initial_details) -> Result<JobLease, JobRegistryError>` reserves capacity and returns producer ownership of one running job.
- `JobLease::{id,snapshot,updater,finish,cancelled}` lets a producer capture its initial snapshot, create a cloneable `JobUpdater`, settle exactly once, and receive cancellation. `JobUpdater::update` publishes partial producer details without transferring lease ownership.
- `JobRegistry::{status,wait,cancel,shutdown}` supplies the generic lifecycle contract consumed by later tasks.

- [ ] **Step 1: Write failing registry transition and notification tests**

Create `tests/job_tool.rs` with a small producer detail type and tests that require the public lifecycle API. The wait test must finish from another Tokio task so it proves an event wakes the waiter.

```rust
use std::{sync::Arc, time::Duration};

use moh::tools::{
    JobDetails, JobKind, JobRegistry, JobState, JobStatusArgs, JobToolError,
    JobWaitArgs, JobService,
};

#[derive(Debug)]
struct TestDetails(&'static str);

impl JobDetails for TestDetails {
    fn render(&self) -> String {
        self.0.to_owned()
    }
}

#[tokio::test]
async fn wait_wakes_when_a_running_job_finishes() {
    let registry = JobRegistry::new();
    let lease = registry
        .start(JobKind::Bash, "fixture", Arc::new(TestDetails("running")))
        .unwrap();
    let id = lease.id();
    tokio::spawn(async move {
        tokio::task::yield_now().await;
        lease
            .finish(JobState::Completed, Arc::new(TestDetails("done")))
            .unwrap();
    });

    let result = registry
        .wait(&[id], Some(Duration::from_secs(1)))
        .await
        .unwrap();

    assert!(!result.timed_out);
    assert_eq!(result.snapshots[0].state(), JobState::Completed);
    assert_eq!(result.snapshots[0].details().render(), "done");
}

#[tokio::test]
async fn cancel_is_idempotent_and_waits_for_the_terminal_snapshot() {
    let registry = JobRegistry::new();
    let mut lease = registry
        .start(JobKind::Bash, "fixture", Arc::new(TestDetails("running")))
        .unwrap();
    let id = lease.id();
    tokio::spawn(async move {
        lease.cancelled().await;
        lease
            .finish(JobState::Cancelled, Arc::new(TestDetails("stopped")))
            .unwrap();
    });

    let first = registry.cancel(id).await.unwrap();
    let second = registry.cancel(id).await.unwrap();

    assert_eq!(first.state(), JobState::Cancelled);
    assert_eq!(second.state(), JobState::Cancelled);
}
```

Add tests in the same file for monotonic `job-0`, `job-1` display/parse behavior, rejection of malformed IDs, 16-running-job capacity, eviction after the 65th terminal result, immediate wait on a terminal job, wait timeout, unknown IDs, concurrent waiters, and shutdown cancellation.

- [ ] **Step 2: Run the job target and verify the public API is absent**

Run: `cargo test --test job_tool`

Expected: FAIL with unresolved imports from `moh::tools` for the job types.

- [ ] **Step 3: Implement the typed registry and producer lease**

Create `src/tools/job.rs`. Use a short synchronous `std::sync::Mutex` critical section for in-memory state and a Tokio `watch` version channel for event-driven waits. `JobLease::drop` must settle an otherwise-running job as `failed` with a producer-disappeared detail so a panicked or aborted supervisor cannot leave an immortal running entry.

```rust
const MAX_RUNNING_JOBS: usize = 16;
const MAX_TERMINAL_JOBS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct JobId(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobKind {
    Bash,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobState {
    Running,
    Completed,
    Failed,
    Cancelled,
}

pub trait JobDetails: std::fmt::Debug + Send + Sync {
    fn render(&self) -> String;

    fn cleanup(&self) {}
}

#[derive(Clone)]
pub struct JobSnapshot {
    id: JobId,
    kind: JobKind,
    state: JobState,
    title: String,
    started_at: chrono::DateTime<chrono::Utc>,
    completed_at: Option<chrono::DateTime<chrono::Utc>>,
    details: Arc<dyn JobDetails>,
}

#[derive(Clone)]
pub struct JobRegistry {
    inner: Arc<RegistryInner>,
}

pub struct JobLease {
    registry: JobRegistry,
    id: JobId,
    cancellation: tokio::sync::watch::Receiver<bool>,
    settled: bool,
}

#[derive(Clone)]
pub struct JobUpdater {
    registry: JobRegistry,
    id: JobId,
    token: u64,
}

pub struct JobWaitResult {
    pub snapshots: Vec<JobSnapshot>,
    pub timed_out: bool,
}
```

Store terminal insertion order separately from the job map so eviction is deterministic. Give every lease/updater an internal token so late output from a settled or replaced producer cannot mutate the retained snapshot. Use `checked_add` for IDs and return a runtime error on exhaustion. Increment the global watch version with `send_replace` after every update or transition. `wait` accepts `Option<Duration>` (`None` means no deadline), subscribes once, checks the requested snapshots, and then awaits version changes inside an optional `tokio::time::timeout`; it may loop after unrelated job events, but it must never sleep or poll on an interval. Call the latest details object's default-no-op `cleanup()` hook when evicting a terminal entry and for every retained entry during registry shutdown, allowing producer-owned temporary resources to be released even while an external snapshot clone still exists.

- [ ] **Step 4: Implement strict lifecycle arguments and `JobService`**

Use `#[serde(deny_unknown_fields)]` for all three argument types. Parse string IDs at the service boundary and map registry errors into stable model-visible categories.

```rust
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobStatusArgs {
    pub job_id: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobWaitArgs {
    pub job_ids: Vec<String>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobCancelArgs {
    pub job_id: String,
}

#[derive(Clone)]
pub struct JobService {
    registry: JobRegistry,
}

impl JobService {
    pub async fn status(&self, args: JobStatusArgs) -> Result<rig::tool::ToolOutput, JobToolError>;
    pub async fn wait(&self, args: JobWaitArgs) -> Result<rig::tool::ToolOutput, JobToolError>;
    pub async fn cancel(&self, args: JobCancelArgs) -> Result<rig::tool::ToolOutput, JobToolError>;
}
```

`job_status` with no ID renders compact one-line snapshots in creation order. With an ID, and for every snapshot returned by wait/cancel, render ID, kind, state, timestamps, title, and `details.render()`. Reject an empty `job_ids` array and `timeout_ms > 300_000` with `[E_INVALID_ARGUMENT]`; default to 30,000 ms. Map malformed IDs to `[E_INVALID_ARGUMENT]`, missing IDs to `[E_NOT_FOUND]`, capacity to `[E_BUSY]`, and poisoned or invariant failures to `[E_RUNTIME]`. Lifecycle inspection remains available after shutdown, while a producer `start` returns `JobRegistryError::ShuttingDown`, which Bash maps to `[E_RUNTIME]`.

Export the public types from `src/tools/mod.rs` and add `pub mod job;`.

- [ ] **Step 5: Complete schema, retention, and shutdown tests**

Assert exact required fields and strictness:

```rust
assert_eq!(JobService::wait_parameters()["required"], serde_json::json!(["job_ids"]));
assert_eq!(JobService::wait_parameters()["additionalProperties"], false);
assert_eq!(JobService::wait_parameters()["properties"]["job_ids"]["minItems"], 1);
assert_eq!(JobService::wait_parameters()["properties"]["timeout_ms"]["maximum"], 300_000);
assert_eq!(JobService::cancel_parameters()["required"], serde_json::json!(["job_id"]));
assert_eq!(JobService::status_parameters()["required"], serde_json::json!([]));
```

For retention, finish 65 jobs and assert `job-0` is not found while `job-1` through `job-64` remain. Use an atomic test detail to assert eviction calls `cleanup()` exactly once. For capacity, keep 16 leases alive and assert the 17th start returns `JobRegistryError::Capacity`; finish one and assert another start succeeds.

- [ ] **Step 6: Run focused tests and commit the generic substrate**

Run:

```bash
cargo test --test job_tool
cargo test --lib tools::job
cargo fmt --all
```

Expected: PASS, including notification-driven wait, idempotent cancellation, capacity, retention, and schema assertions.

```bash
git add src/tools/job.rs src/tools/mod.rs tests/job_tool.rs
git commit -m "feat(tools): add background job registry"
```

## Task 2: Add foreground and background Bash execution

**Files:**
- Modify: `Cargo.toml:21`
- Modify: `Cargo.lock`
- Create: `src/tools/bash.rs`
- Modify: `src/tools/mod.rs:3-15`
- Create: `tests/bash_tool.rs`

**Interfaces:**
- Consumes `JobRegistry::start`, `JobLease`, and `JobDetails` from Task 1.
- Produces `BashArgs`, `BashServiceFactory`, `BashService`, `BashJobDetails`, and `BashToolError`.
- Produces `pub async fn BashService::bash(&self, args: BashArgs) -> Result<ToolOutput, BashToolError>`.
- Produces one supervisor task per job and a startup oneshot that distinguishes a running child from synchronous spawn failure.

- [ ] **Step 1: Write failing Bash behavior and schema tests**

Create `tests/bash_tool.rs` with a helper that shares one registry across cwd-bound service instances.

```rust
use std::time::{Duration, Instant};

use moh::tools::{BashArgs, BashServiceFactory, JobRegistry, JobState};

fn service(directory: &std::path::Path) -> (JobRegistry, moh::tools::BashService) {
    let registry = JobRegistry::new();
    let service = BashServiceFactory::new(registry.clone()).for_cwd(directory.to_owned());
    (registry, service)
}

#[tokio::test]
async fn foreground_bash_runs_from_the_bound_cwd_and_preserves_nonzero_exit() {
    let directory = tempfile::tempdir().unwrap();
    let (_registry, bash) = service(directory.path());

    let output = bash
        .bash(BashArgs {
            command: "printf '%s\\n' \"$PWD\"; printf 'warning\\n' >&2; exit 7".into(),
            background: false,
            timeout_ms: None,
        })
        .await
        .unwrap();
    let text = output.as_text().unwrap();

    assert!(text.contains(directory.path().to_str().unwrap()));
    assert!(text.contains("[stderr] warning"));
    assert!(text.contains("state: completed"));
    assert!(text.contains("exit code: 7"));
}

#[tokio::test]
async fn background_bash_returns_before_the_process_finishes() {
    let directory = tempfile::tempdir().unwrap();
    let (registry, bash) = service(directory.path());
    let started = Instant::now();

    let output = bash
        .bash(BashArgs {
            command: "sleep 0.5; printf 'done\\n'".into(),
            background: true,
            timeout_ms: None,
        })
        .await
        .unwrap();

    assert!(started.elapsed() < Duration::from_millis(250));
    assert!(output.as_text().unwrap().contains("state: running"));
    let terminal = registry
        .wait(&["job-0".parse().unwrap()], Some(Duration::from_secs(1)))
        .await
        .unwrap();
    assert_eq!(terminal.snapshots[0].state(), JobState::Completed);
}
```

Also test empty commands, timeout values `0` and `3_600_001`, optional `background`/`timeout_ms`, strict unknown-field rejection, inherited environment, and the exact schema (`command` required, `additionalProperties: false`, `background.default: false`, and `timeout_ms` minimum 1/maximum 3,600,000). Add a unit test inside `src/tools/bash.rs` for spawn failure using a private `with_program` constructor pointed at a missing executable; do not expose shell selection as public API.

- [ ] **Step 2: Run the Bash target and verify the service is absent**

Run: `cargo test --test bash_tool`

Expected: FAIL with unresolved imports for `BashArgs` and `BashServiceFactory`.

- [ ] **Step 3: Enable Tokio process and asynchronous file I/O**

Change the Tokio dependency to:

```toml
tokio = { version = "1.53.1", features = ["fs", "io-util", "macros", "process", "rt", "sync", "time"] }
```

Run `cargo check` once to refresh `Cargo.lock`. Do not add Nix until Task 3 introduces Unix process-group signalling.

- [ ] **Step 4: Implement strict Bash arguments, factory, and producer details**

Create `src/tools/bash.rs` with these public shapes and constants:

```rust
const MAX_TIMEOUT_MS: u64 = 3_600_000;
const MAX_OUTPUT_BYTES: usize = 50 * 1024;
const MAX_OUTPUT_LINES: usize = 2_000;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BashArgs {
    pub command: String,
    #[serde(default)]
    pub background: bool,
    pub timeout_ms: Option<u64>,
}

#[derive(Clone)]
pub struct BashServiceFactory {
    registry: JobRegistry,
    program: std::ffi::OsString,
}

pub struct BashService {
    cwd: std::path::PathBuf,
    registry: JobRegistry,
    program: std::ffi::OsString,
}

#[derive(Clone, Debug)]
pub struct BashJobDetails {
    command: String,
    output: String,
    full_output: Option<Arc<OutputLog>>,
    exit_code: Option<i32>,
    reason: Option<String>,
}
```

`OutputLog` is private and stores the absolute `PathBuf` plus `std::sync::Mutex<Option<tempfile::TempPath>>`. Its idempotent cleanup takes and closes the `TempPath`, unlinking the file even if older snapshots still hold cloned handles. `BashJobDetails::render` must show the command, bounded source-labelled output, exit code when present, reason when present, and `Full output: <absolute path>` only when truncated and the log has not been cleaned. Its `JobDetails::cleanup` implementation delegates to `OutputLog`.

Create the log with `tempfile::NamedTempFile` through `tools::blocking::run`, split it into `File` and `TempPath`, convert the file to `tokio::fs::File`, and share one `Arc<OutputLog>` across every details snapshot. `NamedTempFile` supplies user-only file permissions on Unix; cover that invariant in a target-gated test.

Expose `description()` and `parameters()` with only `command` required. Export the service API from `src/tools/mod.rs`.

- [ ] **Step 5: Implement the asynchronous process supervisor and both execution modes**

Create the child with null stdin and piped outputs. A startup oneshot must fire only after `spawn` succeeds and both output pipes are taken.

```rust
let mut command = tokio::process::Command::new(&program);
command
    .arg("-lc")
    .arg(&args.command)
    .current_dir(&cwd)
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .kill_on_drop(true);

let child = command.spawn().map_err(BashToolError::spawn)?;
```

Use two reader tasks with `tokio::io::AsyncReadExt::read` and a shared async output store. Prefix chunks with `[stdout] ` or `[stderr] ` in the combined log and bounded tail. Before moving the lease into the supervisor, create `let updater = lease.updater()` and clone it into the readers. After each complete chunk, publish a cloned running `BashJobDetails` through `JobUpdater::update` so `job_status` can see partial output. Await both readers before publishing the terminal details.

The service flow must be structurally equivalent to:

```rust
let lease = self.registry.start(JobKind::Bash, title, initial_details)?;
let id = lease.id();
let initial = lease.snapshot();
let (started_tx, started_rx) = tokio::sync::oneshot::channel();
tokio::spawn(run_supervisor(lease, process, output, started_tx));
started_rx.await.map_err(|_| BashToolError::Runtime)??;

if args.background {
    return Ok(render(initial));
}

let mut guard = CancelOnDrop::new(self.registry.clone(), id);
let snapshot = self.registry.wait(&[id], None).await?.snapshots.remove(0);
guard.disarm();
Ok(render(snapshot))
```

For foreground execution with no command timeout, wait without imposing an internal foreground deadline; run cancellation is handled by `CancelOnDrop` in Task 3. A spawn failure settles the lease as failed and returns `[E_SPAWN]` containing the job ID. A normal nonzero exit settles as completed.

- [ ] **Step 6: Run focused Bash and job tests**

Run:

```bash
cargo test --test bash_tool
cargo test --test job_tool
cargo check
cargo fmt --all
```

Expected: PASS. Background start must return before the fixture completes, and nonzero exit must remain a successful tool result.

- [ ] **Step 7: Commit the first Bash producer**

```bash
git add Cargo.toml Cargo.lock src/tools/bash.rs src/tools/mod.rs tests/bash_tool.rs
git commit -m "feat(tools): add asynchronous bash execution"
```

## Task 3: Harden timeout, output, cancellation, and process shutdown

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `src/tools/job.rs`
- Modify: `src/tools/bash.rs`
- Modify: `tests/job_tool.rs`
- Modify: `tests/bash_tool.rs`

**Interfaces:**
- Consumes the Task 1 registry and Task 2 Bash supervisor.
- Produces cancellation-on-drop for foreground jobs, two-second graceful Bash termination, Unix process-group force-kill, output tail truncation, partial status, and bounded registry shutdown.
- Produces `pub async fn JobRegistry::shutdown(&self) -> Result<(), JobRegistryError>` that rejects future starts and waits for every running producer to settle.

- [ ] **Step 1: Add failing timeout, cancellation, output, and shutdown tests**

Extend `tests/bash_tool.rs` with these cases:

```rust
#[tokio::test]
async fn timeout_is_failed_and_preserves_partial_output() {
    let directory = tempfile::tempdir().unwrap();
    let (_registry, bash) = service(directory.path());
    let output = bash
        .bash(BashArgs {
            command: "printf 'before timeout\\n'; sleep 30".into(),
            background: false,
            timeout_ms: Some(30),
        })
        .await
        .unwrap();
    let text = output.as_text().unwrap();
    assert!(text.contains("state: failed"));
    assert!(text.contains("timeout after 30 ms"));
    assert!(text.contains("before timeout"));
}

#[tokio::test]
async fn shutdown_cancels_a_started_background_job() {
    let directory = tempfile::tempdir().unwrap();
    let (registry, bash) = service(directory.path());
    bash.bash(BashArgs {
        command: "sleep 30".into(),
        background: true,
        timeout_ms: None,
    })
    .await
    .unwrap();

    registry.shutdown().await.unwrap();

    let snapshot = registry.status(Some("job-0".parse().unwrap())).unwrap()[0].clone();
    assert_eq!(snapshot.state(), JobState::Cancelled);
}
```

Add tests that abort a spawned foreground `bash` future and observe a cancelled job, prove a returned background job survives dropping the originating future, inspect partial output before completion, generate more than 2,000 lines and 50 KiB under `#[tokio::test(flavor = "current_thread")]` to verify non-stalling tail truncation plus a readable full-log path, and verify shutdown rejects a new start. Hold an old Bash snapshot across terminal eviction and assert the first log is still unlinked; separately assert shutdown unlinks a retained log. On Unix, assert the created log mode has no group or other permission bits.

Under `#[cfg(unix)]`, start `sleep 30` as a descendant, write `$!` to a fixture file, cancel the job, and assert `nix::sys::signal::kill(Pid::from_raw(pid), None)` returns `Err(Errno::ESRCH)` after cancellation.

- [ ] **Step 2: Run the focused tests and verify lifecycle gaps**

Run: `cargo test --test bash_tool`

Expected: FAIL because timeout, foreground drop cancellation, bounded output, process-group termination, and shutdown rejection are not complete.

- [ ] **Step 3: Add safe Unix process-group signalling**

Add the target-specific dependency:

```toml
[target.'cfg(unix)'.dependencies]
nix = { version = "0.31.3", features = ["signal"] }
```

Before spawn on Unix, configure a new process group through `std::os::unix::process::CommandExt` and Tokio's `as_std_mut()`:

```rust
#[cfg(unix)]
command.as_std_mut().process_group(0);
```

Use `nix::sys::signal::killpg` for `SIGTERM`, wait at most two seconds for `child.wait()`, then send `SIGKILL` and reap. On non-Unix targets, use `Child::start_kill` and `Child::wait`.

- [ ] **Step 4: Make foreground cancellation and timeout race the child safely**

Implement an armed guard whose synchronous drop only requests registry cancellation; the supervisor owns process termination and reaping.

```rust
struct CancelOnDrop {
    registry: JobRegistry,
    id: JobId,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.registry.request_cancel(self.id);
        }
    }
}
```

In the supervisor, `tokio::select!` over `child.wait()`, `lease.cancelled()`, and an optional pinned timeout future. Timeout settles `failed` with `timeout after N ms`; explicit cancellation settles `cancelled`. Both branches use the terminate-then-kill helper, then await both output readers before the final update.

- [ ] **Step 5: Bound model output while retaining the complete log**

Maintain the total observed line/byte counts separately from a tail deque. Remove complete oldest lines until both bounds are satisfied; if one line exceeds 50 KiB, retain its final valid UTF-8 suffix. Keep writing every lossy-decoded source-labelled chunk to the private Tokio file before updating the tail.

```rust
fn trim_tail(lines: &mut VecDeque<String>, bytes: &mut usize) {
    while lines.len() > MAX_OUTPUT_LINES || *bytes > MAX_OUTPUT_BYTES {
        if let Some(removed) = lines.pop_front() {
            *bytes -= removed.len() + 1;
        }
    }
}
```

Mark the details as truncated whenever observed totals exceed either limit. Flush the log before terminal publication. Every running and terminal `BashJobDetails` must share the same `Arc<OutputLog>` so registry eviction or shutdown unlinks the file through `JobDetails::cleanup`, even if a caller retains an older snapshot clone.

- [ ] **Step 6: Complete registry shutdown and producer cleanup**

`shutdown` must atomically reject new jobs, synchronously request cancellation for all running IDs, then event-wait until all are terminal. Wrap the aggregate wait in a three-second timeout, which exceeds the producer's two-second termination grace. If a producer fails to settle, return `JobRegistryError::ShutdownTimeout`; Bash's `kill_on_drop(true)` remains the final direct-child fallback during runtime teardown.

Add a `JobLease::drop` test that drops an unsettled lease and asserts a failed terminal snapshot rather than a permanently running entry.

- [ ] **Step 7: Run the lifecycle suite and commit hardening**

Run:

```bash
cargo test --test job_tool
cargo test --test bash_tool
cargo test --lib tools::job
cargo fmt --all
```

Expected: PASS, including the Unix descendant-process assertion, foreground cancellation, timeout, truncation, and shutdown.

```bash
git add Cargo.toml Cargo.lock src/tools/job.rs src/tools/bash.rs tests/job_tool.rs tests/bash_tool.rs
git commit -m "feat(tools): manage background job lifecycle"
```

## Task 4: Expose Bash and jobs through the Rig run engine

**Files:**
- Create: `src/runtime/rig/bash_tool.rs`
- Create: `src/runtime/rig/job_tool.rs`
- Modify: `src/runtime/rig/mod.rs:3-12`
- Modify: `src/runtime/rig/codex.rs:22-336`
- Modify: `tests/rig_runtime.rs:1-449`

**Interfaces:**
- Consumes `BashServiceFactory`, `JobService`, and `JobRegistry` from Tasks 1-3.
- Produces `RigBashTool`, `RigJobStatusTool`, `RigJobWaitTool`, and `RigJobCancelTool`.
- Produces runtime codes `MOH_BASH_RUNTIME` and `MOH_JOB_RUNTIME`.
- Extends `CodexRunEngine` with one registry, one Bash factory, one lifecycle service, and `pub fn job_registry(&self) -> JobRegistry` for host shutdown.

- [ ] **Step 1: Add failing multi-turn Rig tests and tool-call fixtures**

Add generic helpers to `tests/rig_runtime.rs` that can emit any function call and a response sequence longer than two turns:

```rust
fn function_call_sse(call_id: &str, name: &str, arguments: serde_json::Value) -> String {
    let arguments = serde_json::to_string(&arguments).unwrap();
    let function_call = json!({
        "type": "function_call",
        "id": format!("fc_{call_id}"),
        "arguments": arguments,
        "call_id": call_id,
        "name": name,
        "status": "completed"
    });
    let mut response = success_response("");
    response["output"] = json!([function_call.clone()]);
    [
        json!({"type":"response.output_item.done","sequence_number":0,"output_index":0,"item":function_call}),
        json!({"type":"response.completed","sequence_number":1,"response":response}),
    ]
    .into_iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect()
}
```

Create `SequenceResponses` backed by `Arc<Vec<String>>` and an atomic index. Add one test whose mocked model starts `bash` with `background: true`, then calls `job_wait` for `job-0`, then returns a final answer. Assert both tool event pairs, the final answer, and that the continuation contains the background ID and completed output.

Add `rig_agent_executes_foreground_bash_and_continues`, whose mocked model starts foreground Bash, receives its completed output, and then returns a final answer. Assert one Bash tool event pair, one continuation request containing the exit status and output, and the final completion. This covers the ordinary foreground continuation path separately from stream cancellation.

In the first request, assert all seven tool names are present and exact schemas include:

```rust
assert!(tools.iter().any(|tool| {
    tool["name"] == "bash"
        && tool["parameters"]["additionalProperties"] == false
        && tool["parameters"]["required"] == json!(["command"])
}));
assert!(tools.iter().any(|tool| tool["name"] == "job_status"));
assert!(tools.iter().any(|tool| tool["name"] == "job_wait"));
assert!(tools.iter().any(|tool| tool["name"] == "job_cancel"));
```

- [ ] **Step 2: Run the Rig test and verify tools are unregistered**

Run: `cargo test --test rig_runtime rig_agent_executes_background_bash_and_waits_for_it`

Expected: FAIL because the new tool adapters and engine registrations do not exist.

- [ ] **Step 3: Implement thin Bash and lifecycle PortableTool adapters**

Follow the existing edit/write adapter pattern. Bash maps registry/supervisor infrastructure failure to `MOH_BASH_RUNTIME`; ordinary invalid arguments, capacity, spawn, timeout snapshots, and nonzero exit remain model-visible.

```rust
impl rig::tool::PortableTool for RigBashTool {
    const NAME: &'static str = "bash";
    type Error = RigBashError;
    type Args = BashArgs;
    type Output = rig::tool::ToolOutput;

    fn description(&self) -> String {
        BashService::description().to_owned()
    }

    fn parameters(&self) -> serde_json::Value {
        BashService::parameters()
    }

    async fn call(&self, args: BashArgs) -> Result<Self::Output, Self::Error> {
        self.service.bash(args).await.map_err(RigBashError::from)
    }
}
```

In `job_tool.rs`, use three cloneable wrapper structs around one `Arc<JobService>`. Each declares its own name, description, parameters, and argument type, then awaits the matching service method. Map only `JobToolError::Runtime` to `MOH_JOB_RUNTIME`; keep invalid/not-found errors recoverable.

- [ ] **Step 4: Wire one shared registry through every run attempt**

Extend `CodexRunEngine`:

```rust
pub struct CodexRunEngine {
    models: CodexModelFactory,
    agent: AgentConfig,
    reads: ReadServiceFactory,
    edits: EditServiceFactory,
    writes: WriteServiceFactory,
    bash: BashServiceFactory,
    jobs: JobService,
    registry: JobRegistry,
}

pub fn job_registry(&self) -> JobRegistry {
    self.registry.clone()
}
```

Construct one registry in `CodexRunEngine::new`, share it with both factories/services, and add Bash plus all three job wrappers to `RunAttempt`. Register them in `AgentBuilder` after `write`. Extend `ToolRuntimeHook`'s fatal-code filter with both new runtime codes.

- [ ] **Step 5: Test foreground run cancellation and fatal registry projection**

Add a Rig test that starts foreground Bash with a long sleep, observes `ToolStarted`, drops the run stream, and uses `engine.job_registry().wait(...)` to assert `job-0` becomes cancelled without a continuation request. Add `unknown_job_id_remains_model_visible`, whose first model call supplies an unknown job ID, whose continuation receives `[E_NOT_FOUND]`, and whose second model response completes normally; this proves ordinary lifecycle errors remain model-visible instead of tripping the runtime hook.

Factor the hook's code predicate into a private `is_tool_runtime_code(&str) -> bool` helper and add a `src/runtime/rig/codex.rs` unit test asserting both new codes return true while an ordinary model-visible code returns false. In the two adapter modules, unit-test that a constructed registry/supervisor runtime error maps to `MOH_JOB_RUNTIME` or `MOH_BASH_RUNTIME`. Together these prove infrastructure errors stop through the existing hook while ordinary job errors remain recoverable, without corrupting production registry state solely for a test.

- [ ] **Step 6: Run focused runtime and service suites**

Run:

```bash
cargo test --test rig_runtime rig_agent_executes_background_bash_and_waits_for_it
cargo test --test rig_runtime rig_agent_executes_foreground_bash_and_continues
cargo test --test rig_runtime dropping_foreground_bash_stream_cancels_the_job
cargo test --test rig_runtime unknown_job_id_remains_model_visible
cargo test --lib runtime::rig
cargo test --test bash_tool
cargo test --test job_tool
cargo fmt --all
```

Expected: PASS. The background path must produce two tool-call pairs and a final completion; dropped foreground execution must not send a continuation.

- [ ] **Step 7: Commit the runtime exposure**

```bash
git add src/runtime/rig/bash_tool.rs src/runtime/rig/job_tool.rs src/runtime/rig/mod.rs src/runtime/rig/codex.rs tests/rig_runtime.rs
git commit -m "feat(runtime): expose bash and job tools"
```

## Task 5: Drain jobs at application shutdown and document the feature

**Files:**
- Modify: `src/main.rs:5-34`
- Modify: `src/app.rs:1-162`
- Modify: `README.md:32-125`

**Interfaces:**
- Consumes `CodexRunEngine::job_registry()` and `JobRegistry::shutdown()` from Tasks 3-4.
- Produces an application shutdown order of active-run cancellation, terminal restoration, job drain, pending-auth-refresh drain, and runtime teardown.
- Documents model-visible Bash and lifecycle semantics without promising persistence, PTY support, or automatic completion delivery.

- [ ] **Step 1: Add an application-error projection for job shutdown**

Import `JobRegistryError` in `src/app.rs` and add a transparent variant:

```rust
/// Background jobs could not be drained safely during application shutdown.
#[error(transparent)]
Jobs(#[from] JobRegistryError),
```

Keep terminal restoration inside `app::run`; job shutdown belongs in `main` after `app::run` returns so the screen is restored before a slow process grace period.

- [ ] **Step 2: Retain and drain the engine's job handle**

Change the main async block to preserve the application result while always attempting shutdown:

```rust
let engine = CodexRunEngine::new(codex, AgentConfig::default(), reads)?;
let jobs = engine.job_registry();
let model_name = engine.model_name().to_owned();
let harness = Harness::new(engine);
let application_result = app::run(harness, model_name).await;
let shutdown_result = jobs.shutdown().await.map_err(app::AppError::from);
application_result.and(shutdown_result)
```

Do not move job draining into `run_with_current_thread_runtime`; that helper must continue draining pending credential refreshes after the application future returns, including when application or job shutdown reports an error.

- [ ] **Step 3: Document Bash and generic jobs**

Update the README architecture paragraph to describe `tools` as async capabilities and shared process-local state. Add an `## Agent command execution` section after file access covering:

```markdown
Moh's `bash` tool runs non-interactive `bash -lc` commands from the current
working directory. Foreground execution is the default; `background: true`
returns a job ID after the child starts. Optional timeouts are explicit and
limited to one hour.

`job_status`, `job_wait`, and `job_cancel` inspect and control process-local
jobs. Moh retains at most 16 running and 64 terminal jobs, bounds
model-visible output to 50 KiB or 2,000 lines, and provides a temporary path
for truncated full output. Jobs are not restored after restart, and this
milestone does not provide PTYs, stdin forwarding, or unsolicited completion
notifications.
```

Also state that application shutdown cancels and reaps remaining background processes before runtime teardown.

- [ ] **Step 4: Run formatting and focused shutdown regression tests**

Run:

```bash
cargo fmt --all -- --check
cargo test --test bash_tool shutdown_cancels_a_started_background_job
cargo test --test rig_runtime dropping_foreground_bash_stream_cancels_the_job
cargo test --bin moh application_error_waits_for_detached_refresh_persistence_before_returning
```

Expected: PASS. The existing auth-drain regression must remain green after job shutdown wiring.

- [ ] **Step 5: Run the complete repository gates**

Run each command separately and require exit code 0:

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --locked
```

Expected: formatting produces no diff, Clippy emits no warnings, every non-ignored test passes, and the locked build succeeds.

- [ ] **Step 6: Inspect the final diff and commit host integration**

Run:

```bash
git diff --check
git status --short
git log --stat --oneline -5
```

Confirm the worktree contains only the Task 5 files, the preceding four commits contain only their enumerated files, and no temporary Bash output log is tracked.

```bash
git add src/main.rs src/app.rs README.md
git commit -m "feat(app): drain background jobs on shutdown"
```
