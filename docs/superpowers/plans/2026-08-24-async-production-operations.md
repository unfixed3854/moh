# Async Production Operations Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose Moh's production filesystem, durable-anchor, and credential-persistence operations as direct async APIs while keeping unavoidable synchronous work on Tokio's blocking pool.

**Architecture:** Tool services become async at their public boundary and use private blocking sections for filesystem and SQLite work. Anchor-store operations become awaitable and remain serialized through their connection mutex. Codex credential refresh keeps its network request on the async executor, with separate blocking tasks for lock acquisition, auth-file reads, and atomic persistence; no nested Tokio runtime remains.

**Tech Stack:** Rust 2024, Tokio 1.53 (`rt`, `sync`, `time`, `macros`), Rusqlite 0.40, Reqwest 0.13, tempfile 3.27, Rig 0.41.

**Spec:** `docs/superpowers/specs/2026-08-24-async-production-operations-design.md`

## Global Constraints

- Keep one Cargo package and do not add dependencies; Tokio's existing blocking pool is the only offload mechanism.
- `ReadService::read`, `WriteService::write`, `EditService::edit`, durable anchor operations, `AuthFile::load`, `AuthFile::load_from_env`, and `CodexModelFactory::from_env` must be awaitable by production callers.
- Keep terminal I/O and test-server/test-thread infrastructure synchronous.
- Do not add awaits inside a write or edit checksum-validation-and-replacement critical sequence.
- Preserve current read-before-write/edit requirements, stale-read and stale-anchor rejection, newline/BOM/permission preservation, and atomic installation behavior.
- Preserve SQLite schema, corruption recovery, serialized connection access, and the three-attempt busy retry policy.
- Preserve credential lock ownership across the OAuth request and revalidation, the five-second lock timeout, request timeouts, concurrent-rotation rejection, private permissions, and atomic persistence.
- Preserve current model-visible tool schemas, descriptions, domain errors, and `MOH_READ_RUNTIME`, `MOH_WRITE_RUNTIME`, and `MOH_EDIT_RUNTIME` error codes.
- Do not change harness lifecycle, concurrent-run policy, conversation persistence, model defaults, or Codex protocol behavior.
- Every new public item needs rustdoc; keep the live Codex test ignored.
- Validate with `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-targets`, and `cargo build --locked`.

---

## File Structure

- `src/tools/blocking.rs` — private typed wrapper around `tokio::task::spawn_blocking`, shared by the tool and durable-storage implementation.
- `src/tools/anchor_store.rs` — async open/load/save façade over serialized synchronous SQLite work.
- `src/tools/read.rs` — async read service, lazily initialized async anchor store, and synchronous read payload helpers used only on blocking workers.
- `src/tools/write.rs` and `src/tools/edit.rs` — async public services whose complete mutation critical sections run on the blocking pool.
- `src/runtime/rig/{read_tool,write_tool,edit_tool}.rs` — thin await-only Rig adapters that retain current error-code projection.
- `src/providers/codex/auth.rs` — async credential loading and refresh orchestration without a nested runtime.
- `src/providers/codex/model.rs` and `src/main.rs` — await startup credential loading.
- `tests/{anchor_store,read_tool,write_tool,edit_tool,codex_auth,rig_runtime}.rs`, `tests/codex_live.rs`, and `src/main.rs` tests — await the new production API surface while retaining existing behavioral assertions.

## Task 1: Add the tool blocking boundary and make anchor storage async

**Files:**
- Create: `src/tools/blocking.rs`
- Modify: `src/tools/mod.rs`
- Modify: `src/tools/anchor_store.rs`
- Modify: `tests/anchor_store.rs`

**Interfaces:**
- Produces `crate::tools::blocking::run(operation) -> Result<T, BlockingError<E>>`, where `operation: FnOnce() -> Result<T, E> + Send + 'static` and `T, E: Send + 'static`.
- Produces `async fn AnchorStore::open_at(path: PathBuf) -> Result<AnchorStore, AnchorStoreError>`.
- Produces `async fn AnchorStore::load(&self, canonical_path: PathBuf) -> Result<Option<AnchorSnapshot>, AnchorStoreError>` and `async fn AnchorStore::save(&self, canonical_path: PathBuf, snapshot: AnchorSnapshot) -> Result<(), AnchorStoreError>`.

- [ ] **Step 1: Convert the durable-store tests to the intended async API**

Change every `#[test]` in `tests/anchor_store.rs` that opens, loads, or saves a store to `#[tokio::test]`, then await each operation. Keep the existing multi-threaded test body synchronous inside its spawned threads, but use a local Tokio runtime in each thread for the new public async calls.

```rust
#[tokio::test]
async fn saves_and_reopens_a_snapshot() {
    let path = tempdir().unwrap().path().join("anchors.sqlite");
    let store = AnchorStore::open_at(path.clone()).await.unwrap();
    store.save(canonical.clone(), snapshot.clone()).await.unwrap();

    let reopened = AnchorStore::open_at(path).await.unwrap();
    assert_eq!(reopened.load(canonical).await.unwrap(), Some(snapshot));
}
```

- [ ] **Step 2: Run the durable-store tests to verify the async API is missing**

Run: `cargo test --test anchor_store`

Expected: FAIL because `AnchorStore::{open_at,load,save}` return synchronous `Result` values that cannot be awaited.

- [ ] **Step 3: Add the private typed blocking helper**

Create `src/tools/blocking.rs`. Keep worker failure distinct from the operation's domain error so callers can preserve error classification.

```rust
pub(crate) enum BlockingError<E> {
    Operation(E),
    Worker(tokio::task::JoinError),
}

pub(crate) async fn run<T, E>(
    operation: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<T, BlockingError<E>>
where
    T: Send + 'static,
    E: Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(BlockingError::Worker)?
        .map_err(BlockingError::Operation)
}
```

Declare it with `mod blocking;` in `src/tools/mod.rs`; do not re-export it.

- [ ] **Step 4: Make `AnchorStore` awaitable without moving a SQLite connection across tasks unsafely**

Keep the existing synchronous database implementation as private `open_sync`, `load_sync`, and `save_sync` functions. Store the connection as `Arc<Mutex<Connection>>` so each async method clones only the handle and submits its own synchronous operation through `blocking::run`. Add an `AnchorStoreError::Worker` variant for a failed blocking job; `ReadService` will continue to map any anchor-store error to `ReadToolError::Store`.

```rust
pub async fn load(
    &self,
    canonical_path: PathBuf,
) -> Result<Option<AnchorSnapshot>, AnchorStoreError> {
    let connection = Arc::clone(&self.connection);
    blocking::run(move || load_sync(&connection, &canonical_path))
        .await
        .map_err(AnchorStoreError::from_blocking)
}
```

Keep `retry_sqlite` and its `thread::sleep` unchanged: they execute only inside a blocking worker. Preserve corruption quarantine and schema migration inside `open_sync`.

- [ ] **Step 5: Run the durable-store test target**

Run: `cargo test --test anchor_store`

Expected: PASS, including reopening, corruption rebuild, invalid-snapshot, and contention assertions.

- [ ] **Step 6: Commit the independently working storage boundary**

```bash
git add src/tools/blocking.rs src/tools/mod.rs src/tools/anchor_store.rs tests/anchor_store.rs
git commit -m "refactor(tools): make anchor storage async"
```

## Task 2: Make the read service and its Rig adapter awaitable

**Files:**
- Modify: `src/tools/read.rs`
- Modify: `src/runtime/rig/read_tool.rs`
- Modify: `tests/read_tool.rs`

**Interfaces:**
- Consumes `AnchorStore`'s async methods and `tools::blocking::run` from Task 1.
- Produces `pub async fn ReadService::read(&self, args: ReadArgs) -> Result<ToolOutput, ReadToolError>`.
- Produces `pub(crate) async fn ReadService::stored_snapshot(&self, canonical_path: PathBuf) -> Result<Option<AnchorSnapshot>, ReadToolError>`.
- `RigReadTool::call` returns the awaited service result without calling `spawn_blocking` itself.

- [ ] **Step 1: Convert read-service callers to await reads**

Change all read behavior tests in `tests/read_tool.rs` to `#[tokio::test]` and append `.await` before `unwrap`, `unwrap_err`, or assertions. Preserve every fixture and assertion verbatim, including paging, listings, text/image boundaries, access errors, anchor reuse, collision allocation, and large repeated snapshots.

```rust
#[tokio::test]
async fn read_pages_hashline_output() {
    let output = tool(directory.path())
        .read(ReadArgs::path("fixture.txt"))
        .await
        .unwrap();
    assert_eq!(output.as_text().unwrap(), expected);
}
```

- [ ] **Step 2: Run read-service tests to verify the API break**

Run: `cargo test --test read_tool`

Expected: FAIL because `ReadService::read` is synchronous and returns a `Result`, not a future.

- [ ] **Step 3: Split read work into blocking payload collection and async persistence**

Keep argument validation, schema, output formatting, anchor allocation, and text classification behavior unchanged. Add a private `ReadPayload` containing the requested path, canonical path, raw bytes, normalized lines, checksum, paging values, and either a directory listing or file data. Create it in one `blocking::run` call so all `std::fs` work remains off the executor.

Replace `Arc<OnceLock<Result<AnchorStore, AnchorStoreError>>>` with `Arc<tokio::sync::OnceCell<Result<AnchorStore, AnchorStoreError>>>`. Implement an async `store()` helper that initializes with `AnchorStore::open_at(self.config.anchor_store_path.clone()).await`, then maps initialization errors to `ReadToolError::Store`.

```rust
pub async fn read(&self, args: ReadArgs) -> Result<ToolOutput, ReadToolError> {
    let payload = blocking::run(move || collect_read_payload(cwd, args))
        .await
        .map_err(ReadToolError::from_blocking)?;
    match payload {
        ReadPayload::Directory { text } => Ok(ToolOutput::text(text)),
        ReadPayload::File(file) => {
            let hashes = self.hashes_for(&file.canonical_path, &file.checksum, &file.lines).await?;
            self.observations.record(&file.requested_path, file.canonical_path, &file.bytes)
                .map_err(|_| ReadToolError::Store)?;
            Ok(ToolOutput::text(format_output(&file.lines, &hashes, file.offset, file.limit, file.had_utf8_decode_errors)))
        }
    }
}
```

Implement `hashes_for` and `stored_snapshot` as async methods that await the store's `load` and `save` calls. Do not await while a `FileObservations` mutex guard exists.

- [ ] **Step 4: Reduce `RigReadTool` to error projection and awaiting**

Remove `run_blocking_read`, the generic local helper, and `panic_in_worker`. Keep `RigReadError` only if needed to map a `ReadToolError::Worker` into `MOH_READ_RUNTIME`; otherwise map the direct service error in `map_error`. The tool call must be structurally equivalent to:

```rust
async fn call(&self, args: ReadArgs) -> Result<ToolOutput, RigReadError> {
    self.service.read(args).await.map_err(RigReadError::from)
}
```

Move the current-thread non-stalling test from the adapter module to `src/tools/blocking.rs`, where it exercises `blocking::run` with the same oneshot/release-worker arrangement. Keep the assertion that a `spawn_local` task progresses before the blocking job is released.

- [ ] **Step 5: Run focused read and runtime tests**

Run:

```bash
cargo test --test read_tool
cargo test --test rig_runtime rig_agent_executes_read
cargo test --test rig_runtime ordinary_read_errors_remain_model_visible_and_continue_the_loop
```

Expected: PASS. The Rig test must still render a normal read error to the model and continue to its final completion.

- [ ] **Step 6: Commit the async read boundary**

```bash
git add src/tools/read.rs src/tools/blocking.rs src/runtime/rig/read_tool.rs tests/read_tool.rs
git commit -m "refactor(tools): make read service async"
```

## Task 3: Make writes and edits awaitable while preserving atomic mutation sequences

**Files:**
- Modify: `src/tools/write.rs`
- Modify: `src/tools/edit.rs`
- Modify: `src/runtime/rig/write_tool.rs`
- Modify: `src/runtime/rig/edit_tool.rs`
- Modify: `tests/write_tool.rs`
- Modify: `tests/edit_tool.rs`

**Interfaces:**
- Consumes `ReadService::read(...).await`, `ReadService::stored_snapshot(...).await`, and `tools::blocking::run`.
- Produces `pub async fn WriteService::write(&self, args: WriteArgs) -> Result<ToolOutput, WriteToolError>`.
- Produces `pub async fn EditService::edit(&self, args: EditArgs) -> Result<ToolOutput, EditToolError>`.
- `RigWriteTool::call` and `RigEditTool::call` await services directly and preserve their runtime error codes.

- [ ] **Step 1: Convert write and edit behavior tests to async**

Change all `#[test]` functions in `tests/write_tool.rs` and `tests/edit_tool.rs` to `#[tokio::test]`; await every tool invocation and leave all filesystem assertions unchanged.

```rust
#[tokio::test]
async fn write_rejects_a_file_changed_after_it_was_read() {
    reader.read(ReadArgs::path("note.txt")).await.unwrap();
    std::fs::write(&path, "external change").unwrap();
    assert!(matches!(writer.write(args).await, Err(WriteToolError::StaleRead)));
}
```

- [ ] **Step 2: Run the mutation test targets to verify the async contract is not implemented**

Run: `cargo test --test write_tool && cargo test --test edit_tool`

Expected: FAIL because `WriteService::write` and `EditService::edit` return synchronous `Result` values.

- [ ] **Step 3: Offload each entire write critical section through the private helper**

Make `WriteService::write` clone the cwd and observations into one `blocking::run` closure that performs the existing path resolution, observation lookup, checksum validation, parent check, temporary-file creation, fsync, immediate checksum recheck, atomic persist, and observation refresh as one uninterrupted synchronous sequence. Add a typed worker-failure variant and map it to the existing Rig runtime code.

```rust
pub async fn write(&self, args: WriteArgs) -> Result<ToolOutput, WriteToolError> {
    let cwd = self.cwd.clone();
    let observations = self.observations.clone();
    blocking::run(move || write_sync(cwd, observations, args))
        .await
        .map_err(WriteToolError::from_blocking)?
}
```

Retain `persist_noclobber` behavior so a file that appears after the initial absent-path check still produces `NotRead`, and do not create a deleted parent directory for a previously observed file.

- [ ] **Step 4: Offload edit stages without weakening the final stale check**

Make argument validation synchronous and cheap. Offload canonicalization, observation lookup, byte read, and checksum validation; await `ReadService::stored_snapshot` to resolve anchors; build the replacement; then offload `replace_file` plus refreshed observation recording. `replace_file` must keep the existing read-immediately-before-`persist` comparison so an external modification between async stages returns `StaleRead`.

After a successful replacement, call `self.reader.read(ReadArgs::path(args.path)).await` to return refreshed model anchors.

```rust
let snapshot = self.reader.stored_snapshot(canonical.clone()).await
    .map_err(|_| EditToolError::Runtime)?
    .ok_or(EditToolError::NotRead)?;
// resolve the range, then run replace_file and observations.record together on a blocking worker
```

- [ ] **Step 5: Make Rig mutation adapters await-only**

Remove their adapter-owned `tokio::task::spawn_blocking` calls. Map typed worker failures from the services to `MOH_WRITE_RUNTIME` and `MOH_EDIT_RUNTIME`; keep ordinary stale/read/access errors model-visible as before.

```rust
async fn call(&self, args: WriteArgs) -> Result<ToolOutput, RigWriteError> {
    self.service.write(args).await.map_err(RigWriteError::from)
}
```

- [ ] **Step 6: Run focused mutation and Rig integration tests**

Run: `cargo test --test write_tool && cargo test --test edit_tool && cargo test --test rig_runtime rig_agent_executes_write`

Expected: PASS, including permission, symlink, deletion, stale-read, BOM/newline, and fresh-anchor assertions.

- [ ] **Step 7: Commit the async mutation APIs**

```bash
git add src/tools/write.rs src/tools/edit.rs src/runtime/rig/write_tool.rs src/runtime/rig/edit_tool.rs tests/write_tool.rs tests/edit_tool.rs
git commit -m "refactor(tools): make write and edit services async"
```

## Task 4: Make credential loading and refresh persistence async

**Files:**
- Modify: `src/providers/codex/auth.rs`
- Modify: `src/providers/codex/model.rs`
- Modify: `src/main.rs`
- Modify: `tests/codex_auth.rs`
- Modify: `tests/rig_runtime.rs`
- Modify: `tests/codex_live.rs`

**Interfaces:**
- Produces `pub async fn AuthFile::load(path: impl Into<PathBuf>) -> Result<AuthFile, AuthError>` and `pub async fn AuthFile::load_from_env() -> Result<AuthFile, AuthError>`.
- Produces `pub async fn CodexModelFactory::from_env(config: CodexConfig) -> Result<CodexModelFactory, CodexModelError>`.
- `AuthFile::refresh` remains async and no longer creates a nested Tokio runtime.

- [ ] **Step 1: Convert auth and startup callers to await credential loading**

Change credential-loading tests in `tests/codex_auth.rs` from `#[test]` to `#[tokio::test]` where they call `AuthFile::load`, and await each load. Update helper functions used by async integration tests accordingly. Change `tests/rig_runtime.rs`'s synthetic-auth fixture and the ignored live test to await loading. In `src/main.rs`, await factory construction inside the existing runtime closure.

```rust
let codex = CodexModelFactory::from_env(CodexConfig::default()).await?;
let mut auth = AuthFile::load(path_for_run).await.unwrap();
```

For the existing test-only spawned threads that inspect or mutate credentials, retain synchronous thread infrastructure and create a local current-thread runtime only around `AuthFile::load(path).await`.

- [ ] **Step 2: Run auth tests to verify async startup and loading are missing**

Run: `cargo test --test codex_auth`

Expected: FAIL because `AuthFile::load`, `AuthFile::load_from_env`, and `CodexModelFactory::from_env` are synchronous values rather than futures.

- [ ] **Step 3: Separate synchronous file primitives from async orchestration**

Keep JSON parsing, credential validation, lock-file creation, and atomic temp-file replacement in private synchronous helpers. Wrap `load_sync`, `acquire_credential_lock`, and `persist_atomically` in narrow `tokio::task::spawn_blocking` calls. Convert a join failure during refresh to `AuthError::RefreshTransport`; for startup loading, preserve `FileRequired`, `Read`, and `Malformed` classifications rather than exposing raw worker details.

```rust
pub async fn load(path: impl Into<PathBuf>) -> Result<Self, AuthError> {
    let path = path.into();
    let load_path = path.clone();
    tokio::task::spawn_blocking(move || load_sync(load_path))
        .await
        .map_err(|error| AuthError::Read {
            path,
            source: std::io::Error::other(error),
        })?
}

async fn acquire_credential_lock_async(
    path: PathBuf,
    policy: RefreshPolicy,
) -> Result<std::fs::File, AuthError> {
    tokio::task::spawn_blocking(move || {
        acquire_credential_lock(&path, policy.lock_timeout, policy.lock_retry_interval)
    })
    .await
    .map_err(|_| AuthError::RefreshTransport)?
}
```

Implement `refresh_and_persist` as one async flow: await lock acquisition, await the first `AuthFile::load`, issue the existing async Reqwest request, await the re-read, validate the account and refresh token, update the document, then await atomic persistence. Bind the returned lock file to `_credential_lock` until the function returns. Delete the inner `tokio::runtime::Builder` and `runtime.block_on` block.

- [ ] **Step 4: Await factory construction at the provider and binary boundaries**

Make `CodexModelFactory::from_env` async and await `AuthFile::load_from_env`. In `main`, await `CodexModelFactory::from_env` before creating services and the engine. No other factory or engine constructor changes shape.

- [ ] **Step 5: Run focused auth, provider, and shutdown tests**

Run:

```bash
cargo test --test codex_auth
cargo test --test rig_runtime refreshes_and_retries_once_after_unauthorized
cargo test --test rig_runtime refresh_after_401_survives_provider_cancellation_and_runtime_teardown
cargo test --bin moh application_error_waits_for_detached_refresh_persistence_before_returning
```

Expected: PASS. Confirm a refresh still rotates and persists credentials after outer cancellation, lock contention leaves the current-thread executor responsive, and no nested runtime is constructed in production code.

- [ ] **Step 6: Commit the async credential boundary**

```bash
git add src/providers/codex/auth.rs src/providers/codex/model.rs src/main.rs tests/codex_auth.rs tests/rig_runtime.rs tests/codex_live.rs
git commit -m "refactor(auth): make credential persistence async"
```

## Task 5: Run the complete regression suite and inspect the migration boundary

**Files:**
- Modify only if formatting or a compiler-directed import/doc fix is required by the commands below.

**Interfaces:**
- Consumes the async tool and credential interfaces from Tasks 1-4.
- Produces a fully validated repository with no production adapter-owned whole-service `spawn_blocking` calls and no nested credential-refresh runtime.

- [ ] **Step 1: Check remaining production synchronous-boundary references**

Run:

```bash
rg -n "spawn_blocking|new_current_thread\(\)|runtime\.block_on" src
```

Expected: `spawn_blocking` appears only in `src/tools/blocking.rs` and narrow auth-file helpers; `new_current_thread` and `runtime.block_on` appear only in `src/main.rs`'s application bootstrap or tests, never in `src/providers/codex/auth.rs`; Rig adapters contain no `spawn_blocking`.

- [ ] **Step 2: Format the repository**

Run: `cargo fmt --all`

Expected: exits 0. Stage any formatter changes with the task that owns the touched files.

- [ ] **Step 3: Run the static gate**

Run: `cargo clippy --all-targets --all-features -- -D warnings`

Expected: PASS with no warnings, including missing documentation warnings for new public async APIs and error variants.

- [ ] **Step 4: Run all test targets**

Run: `cargo test --all-targets`

Expected: PASS; the ignored live Codex test remains ignored.

- [ ] **Step 5: Build the locked dependency graph**

Run: `cargo build --locked`

Expected: PASS.

- [ ] **Step 6: Commit any final validation-required fixes**

If the preceding commands changed tracked files, commit only those fixes:

```bash
git add src/tools/blocking.rs src/tools/mod.rs src/tools/anchor_store.rs src/tools/read.rs src/tools/write.rs src/tools/edit.rs src/runtime/rig/read_tool.rs src/runtime/rig/write_tool.rs src/runtime/rig/edit_tool.rs src/providers/codex/auth.rs src/providers/codex/model.rs src/main.rs tests/anchor_store.rs tests/read_tool.rs tests/write_tool.rs tests/edit_tool.rs tests/codex_auth.rs tests/rig_runtime.rs tests/codex_live.rs
git commit -m "chore: validate async production operations"
```

If no tracked file changed, do not create an empty commit.
