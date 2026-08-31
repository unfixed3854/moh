# Agent Read Tool Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give Moh's Codex agent a durable, Pi-compatible hashline `read` tool that Rig executes during a submitted prompt.

**Architecture:** Add a small `tools` module with a text-only `ReadTool` and a SQLite-backed anchor store. Add `rig-agent` around the existing custom Responses completion model so Rig advertises, executes, correlates, and continues function calls while `ChatBackend` continues yielding only assistant text.

**Tech Stack:** Rust 2024, `rig-core` 0.41.0, `rig-agent` 0.41.0, SQLite via `rusqlite`, xxHash via `xxhash-rust`, `directories` 6.0.0, Tokio, Wiremock, Tempfile.

**Spec:** `docs/superpowers/specs/2026-08-18-agent-read-tool-design.md`

## Global Constraints

- Add only the text-file `read` tool; do not add write, replace, undo, tool-result image rendering, workspace confinement, conversation persistence, or a TUI tool-activity view.
- Accept cwd-relative and absolute paths, including paths through symlinks; canonicalize only for state identity and I/O.
- Use `ProjectDirs::state_dir()`; when it is `None`, use `ProjectDirs::data_local_dir()`. Never put the SQLite database in a configuration directory.
- Persist the database as `hash-store.sqlite` inside the Moh state directory.
- Allow exactly 512 model calls for each submitted prompt, including all tool continuations.
- Preserve `store: false`, medium reasoning, existing pre-text 401 refresh, stream cancellation, and redacted provider errors.
- Render text-only tool output. JPEG, PNG, GIF, WebP, and BMP must return `[E_NOT_TEXT]` pending GitHub issue #1.
- Do not print credentials, OAuth documents, authorization headers, or raw provider response bodies in tests or diagnostics.

---

### Task 1: Add dependencies and expose the tools module

**Files:**

- Modify: `Cargo.toml:6-24`
- Modify: `src/lib.rs:4-10`
- Create: `src/tools/mod.rs`
- Create: `src/tools/anchor_store.rs`
- Test: `src/tools/anchor_store.rs` (unit tests)

**Interfaces:**

- Produces `crate::tools::anchor_store::AnchorStore` and `AnchorSnapshot` for the read tool.
- Produces `crate::tools::moh_state_dir() -> Result<PathBuf, AnchorStoreError>`.
- Consumes `directories::ProjectDirs`, `rusqlite`, and xxHash algorithms from dependencies added here.

- [ ] **Step 1: Write the failing state-directory and snapshot round-trip tests**

Add a `#[cfg(test)]` module to `src/tools/anchor_store.rs` with a deterministic temporary-store constructor and tests for the exact public operations:

```rust
#[test]
fn saves_and_reopens_a_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("hash-store.sqlite");
    let store = AnchorStore::open_at(&path).unwrap();
    let snapshot = AnchorSnapshot {
        checksum: "checksum".into(),
        line_count: 2,
        hashes: vec!["Ab1".into(), "Cd2".into()],
    };
    store.save(Path::new("/canonical/file.txt"), &snapshot).unwrap();

    let reopened = AnchorStore::open_at(&path).unwrap();
    assert_eq!(reopened.load(Path::new("/canonical/file.txt")).unwrap(), Some(snapshot));
}

#[test]
fn moh_state_dir_uses_the_platform_state_or_local_data_directory() {
    let path = moh_state_dir().unwrap();
    assert!(path.ends_with("moh"));
}
```

- [ ] **Step 2: Run the focused test to verify it fails**

Run: `cargo test tools::anchor_store::tests`

Expected: FAIL because the `tools` module, `AnchorStore`, and `moh_state_dir` do not exist.

- [ ] **Step 3: Add the exact direct dependencies and public module surface**

Update the dependency list to include compatible versions and features:

```toml
directories = "6.0.0"
rig-agent = "0.41.0"
rusqlite = { version = "0.40.2", features = ["bundled"] }
xxhash-rust = { version = "0.8.18", features = ["xxh32", "xxh64"] }
```

Add `pub mod tools;` to `src/lib.rs`, create `src/tools/mod.rs` with `pub mod anchor_store;`, and define `AnchorSnapshot` as `{ checksum: String, line_count: usize, hashes: Vec<String> }` with `Clone`, `Debug`, `Eq`, and `PartialEq`.

- [ ] **Step 4: Implement state directory resolution and SQLite persistence**

Implement these exact store operations:

```rust
pub fn moh_state_dir() -> Result<PathBuf, AnchorStoreError> {
    let directories = ProjectDirs::from("", "", "moh")
        .ok_or(AnchorStoreError::StateDirectoryUnavailable)?;
    Ok(directories
        .state_dir()
        .unwrap_or_else(|| directories.data_local_dir())
        .to_path_buf())
}

pub fn open_at(path: &Path) -> Result<Self, AnchorStoreError>;
pub fn load(&self, canonical_path: &Path) -> Result<Option<AnchorSnapshot>, AnchorStoreError>;
pub fn save(&self, canonical_path: &Path, snapshot: &AnchorSnapshot) -> Result<(), AnchorStoreError>;
```

`open_at` must create the parent directory, enable WAL, run `PRAGMA quick_check`, quarantine a corrupt database as `hash-store.sqlite.corrupt-<unix-millis>` before recreating it, and create this table:

```sql
CREATE TABLE IF NOT EXISTS snapshots (
    path TEXT PRIMARY KEY,
    checksum TEXT NOT NULL,
    line_count INTEGER NOT NULL,
    hashes TEXT NOT NULL
)
```

Use an immediate transaction for `save`, JSON-encode the whole hash vector, reject any snapshot containing duplicate or malformed anchors, and retry SQLite busy/locked errors three times after 100 ms. Map all errors to a typed `AnchorStoreError`; do not silently continue with in-memory anchors.

- [ ] **Step 5: Run the focused tests to verify they pass**

Run: `cargo test tools::anchor_store::tests`

Expected: PASS; the test opens a SQLite file only below its temporary directory, saves an anchor list, and recovers it from a second connection.

- [ ] **Step 6: Run formatting and commit the dependency/store foundation**

Run:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
git add Cargo.toml Cargo.lock src/lib.rs src/tools/mod.rs src/tools/anchor_store.rs
git commit -m "feat: add durable anchor store"
```

Expected: formatting and Clippy pass; the commit contains only the new dependency lock entries and tools foundation.

### Task 2: Implement Pi-compatible text reading and anchors

**Files:**

- Create: `src/tools/read.rs`
- Modify: `src/tools/mod.rs`
- Test: `tests/read_tool.rs`

**Interfaces:**

- Consumes `AnchorStore::{load,save}` and xxHash functions from Task 1.
- Produces `pub struct ReadTool`, `pub struct ReadArgs`, and `impl rig_core::tool::PortableTool for ReadTool`.
- Produces `ReadTool::with_store_path(path: PathBuf)` for deterministic tests and `ReadTool::default()` for the production state directory.
- Produces a text `ToolOutput` whose content is the exact model-visible result.

- [ ] **Step 1: Write failing happy-path, paging, and argument tests**

Create `tests/read_tool.rs` with tests that call `PortableTool::call` directly. Use a temporary fixture containing `one\ntwo\nthree\n` and a temporary SQLite path. Assert these exact result properties:

```rust
let output = tool.call(ReadArgs {
    path: Some(fixture.display().to_string()),
    file_path: None,
    offset: Some(2),
    limit: Some(1),
}).await.unwrap();
let text = output.as_text().unwrap();
let hash = text.split_once('│').unwrap().0;
assert_eq!(hash.len(), 3);
assert!(hash.chars().all(|character| character.is_ascii_alphanumeric()));
assert!(text.contains("│two"));
assert!(text.ends_with("[Showing lines 2-2 of 3. Use offset=3 to continue.]"));
```

Also add tests that reject zero, negative, fractional, and duplicate `path`/`file_path` input through the tool schema/argument deserialization, and that resolve a relative file below a scoped test cwd.

- [ ] **Step 2: Run the focused tests to verify they fail**

Run: `cargo test --test read_tool read_ -- --nocapture`

Expected: FAIL because `ReadTool` and `ReadArgs` do not exist.

- [ ] **Step 3: Define the strict Rig tool contract**

Implement `ReadArgs` with `#[serde(deny_unknown_fields)]` and the fields below; reject both path fields or neither in `ReadTool::call`:

```rust
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadArgs {
    pub path: Option<String>,
    pub file_path: Option<String>,
    pub offset: Option<u64>,
    pub limit: Option<u64>,
}

impl PortableTool for ReadTool {
    const NAME: &'static str = "read";
    type Args = ReadArgs;
    type Output = ToolOutput;
    type Error = ReadToolError;
}
```

The parameters schema must set `additionalProperties: false`, use `oneOf` to require exactly one of `path` and `file_path`, describe `offset` as a one-indexed positive integer, and instruct the model that output rows are `HASH│content` anchors. Keep the explicit runtime exactly-one validation as defense in depth.

- [ ] **Step 4: Write failing file-kind and display-boundary tests**

Extend `tests/read_tool.rs` to create each fixture and assert model-visible results:

```rust
assert!(read("missing.txt").await.unwrap_err().to_string().starts_with("[E_NOT_FOUND]"));
assert!(read(directory.path()).await.unwrap_err().to_string().starts_with("[E_NOT_TEXT]"));
assert!(read(binary_with_nul).await.unwrap_err().to_string().starts_with("[E_NOT_TEXT]"));
assert!(read(utf16_bom).await.unwrap_err().to_string().starts_with("[E_NOT_TEXT]"));
assert!(read(png_signature).await.unwrap_err().to_string().starts_with("[E_NOT_TEXT]"));
assert!(read(empty).await.unwrap().as_text().unwrap().contains("[File is empty. Use replace to insert content.]"));
```

Add fixtures for a UTF-8 BOM, malformed UTF-8 byte sequence, a text file beginning `BM` without NUL bytes, 238,329 newline-separated lines, a 100 MiB-plus file, and one 200 KiB-plus line.

- [ ] **Step 5: Run the boundary tests to verify they fail**

Run: `cargo test --test read_tool -- --nocapture`

Expected: FAIL because classification, size caps, BOM handling, and the long-line marker have not been implemented.

- [ ] **Step 6: Implement safe reading and Pi-compatible formatting**

Implement the following behavior in `ReadTool::call` before allocating anchors:

1. Join a relative path to `std::env::current_dir`, canonicalize it, inspect metadata, reject directories and non-regular files, and map `NotFound`/`PermissionDenied` to `[E_NOT_FOUND]`/`[E_ACCESS]`.
2. Reject byte length over `100 * 1024 * 1024`, NUL-bearing samples, UTF-16/32 BOMs, and image signatures for JPEG/PNG/GIF/WebP/BMP with the exact `[E_FILE_TOO_LARGE]` or `[E_NOT_TEXT]` prefix.
3. Decode UTF-8 lossily, strip one UTF-8 BOM, normalize CRLF/CR to LF, and append `[Non-UTF-8 bytes shown as U+FFFD; editing rewrites the file as UTF-8.]` only when replacement decoding occurred.
4. Represent a nonempty normalized file as `text.split('\n')`; represent an empty file as one empty logical line. Reject more than 238,328 lines.
5. Apply one-indexed paging. If `offset` is beyond the logical line count, return the extension-compatible explanatory result; otherwise append the exact continuation hint whenever rows remain.
6. For a selected row whose complete `HASH│content` byte length exceeds 204,800, emit the line-size marker and `sed -n 'Np' <path> | head -c 204800` guidance instead of a partial row.

- [ ] **Step 7: Write failing durable-anchor tests**

Add tests that read the same fixture through two newly constructed `ReadTool::with_store_path` instances and assert the same hashes. Then externally insert a leading line, remove it, change one interior line, and use repeated `}` lines. Parse anchors with:

```rust
fn anchors(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.split_once('│').map(|(hash, _)| hash.to_owned()))
        .collect()
}
```

Assert every returned hash is three ASCII alphanumeric characters, repeated lines have distinct anchors, unchanged occurrences retain their own prior anchors, and the modified line receives a different anchor.

- [ ] **Step 8: Run the durable-anchor tests to verify they fail**

Run: `cargo test --test read_tool anchor -- --nocapture`

Expected: FAIL because anchors are not yet persisted or matched across changed snapshots.

- [ ] **Step 9: Implement deterministic allocation and changed-snapshot matching**

Implement the anchor algorithm exactly as follows:

```rust
const ALPHABET: &[u8; 62] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
const HASH_SPACE: u32 = 62 * 62 * 62;
const PROBE_STRIDE: u32 = 62 * 62 + 62 + 1;

fn canonical_line(line: &str) -> &str { line.trim_end() }
fn base_slot(line: &str) -> u32 { (xxh32(canonical_line(line).as_bytes(), 0) >> 14) % HASH_SPACE }
```

Encode a slot in three base-62 digits. Track occupied slots in a `Vec<bool>` of `HASH_SPACE` entries and, on a collision, probe `(slot + PROBE_STRIDE) % HASH_SPACE` until free. On an unchanged checksum and line count, reuse the stored hash vector directly.

For a changed snapshot, first preserve anchors by matching canonical old/new lines with occurrence order and nearest original positions; reserve every preserved slot. For still-unmatched new lines, reuse anchors from removed identical lines in their old occurrence order, then allocate new hashes. Store the resulting full normalized-line hash vector in SQLite before returning output. A store failure must return `ReadToolError::Store`, never an ephemeral anchor list.

- [ ] **Step 10: Run the full read-tool suite and commit**

Run:

```bash
cargo fmt --check
cargo test --test read_tool
cargo clippy --all-targets -- -D warnings
git add src/tools/read.rs src/tools/mod.rs tests/read_tool.rs
git commit -m "feat: add anchored read tool"
```

Expected: every text, error, paging, and durable-anchor test passes; no image attachment behavior is introduced.

### Task 3: Execute `read` through Rig's agent runtime

**Files:**

- Modify: `src/codex_provider.rs:1-620`
- Modify: `tests/codex_provider.rs:1-620`
- Test: `tests/codex_provider.rs`

**Interfaces:**

- Consumes `ReadTool`, `rig_agent::AgentBuilder`, `StreamingChat`, and the existing `CodexCompletionModel`.
- Preserves `pub trait ChatBackend` and `pub type ChatStream` unchanged.
- Produces agent-driven `CodexProvider::{complete,stream}` with a 512-call max-turn limit.

- [ ] **Step 1: Write the failing two-request Responses integration test**

Add helpers that emit an initial SSE `response.output_item.done` function call and a final text SSE. Mount an ordered responder that returns them in sequence. The first function call must name `read`, use a fixture path, and include a provider `call_id`. Assert:

```rust
let chunks = provider.stream(vec![Message::user("read the fixture")]).collect::<Vec<_>>().await;
assert_eq!(chunks, vec![Ok("fixture answer".into())]);

let requests = server.received_requests().await.unwrap();
assert_eq!(requests.len(), 2);
let first: Value = serde_json::from_slice(&requests[0].body).unwrap();
assert_eq!(first["tools"][0]["name"], "read");
let second: Value = serde_json::from_slice(&requests[1].body).unwrap();
let output = second["input"].as_array().unwrap().iter()
    .find(|item| item["type"] == "function_call_output").unwrap();
assert_eq!(output["call_id"], "call_read_1");
assert!(output.to_string().contains("│fixture line"));
```

Keep test fixture files and the hash-store path under `tempdir`; do not use real user state.

- [ ] **Step 2: Run the integration test to verify it fails**

Run: `cargo test --test codex_provider rig_agent_executes_read -- --exact`

Expected: FAIL because the provider still sends an empty tools list and does not issue a continuation request.

- [ ] **Step 3: Build a request-scoped Rig agent for each provider attempt**

Replace direct `completion_request(...).stream()` usage with an `AgentBuilder` around the existing configured `CodexCompletionModel`:

```rust
let agent = AgentBuilder::new(model)
    .tool(ReadTool::default())
    .additional_params(json!({
        "reasoning": Reasoning::new().with_effort(ReasoningEffort::Medium)
    }))
    .default_max_turns(512)
    .build();

let request = agent.stream_chat(prompt, messages).max_turns(512);
let stream = request.stream().await;
```

Import Rig agent traits from `rig_agent`; preserve `CodexCompletionModel::prepare_request` as the one place that adds `store: false` to every underlying call.

- [ ] **Step 4: Adapt the stream boundary without exposing tool internals**

Map `MultiTurnStreamItem` to Moh's `ChatStream` as follows:

```rust
match item? {
    MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text))
        if !text.text.is_empty() => terminal_text.push_str(&text.text),
    MultiTurnStreamItem::FinalResponse(response) => emit_terminal_text_once(response, terminal_text)?,
    _ => {}
}
```

Treat the emitted content from a model turn that ends in a tool call as provisional: buffer it and discard it before Rig's continuation. Yield only assistant text from the terminal no-tool model turn, preserving the spec's text-only TUI contract. Map Rig prompt/stream errors through the existing redacted `ProviderError` categories; never expose provider bodies.

- [ ] **Step 5: Preserve refresh and cancellation semantics with failing regression tests**

Extend the existing 401-before-first-delta test to make the recovered response use the Rig agent path and assert exactly one refresh and one successful tool schema request. Add a cancellation test that begins a tool-loop SSE response, drops the returned Moh stream, and proves its server handler is released without writing a second request.

- [ ] **Step 6: Run provider tests to verify the agent path is green**

Run:

```bash
cargo test --test codex_provider rig_agent_executes_read -- --exact
cargo test --test codex_provider stream_refreshes_and_retries_once_before_the_first_delta -- --exact
cargo test --test codex_provider
```

Expected: the first request advertises read, the second carries the exact function-call output and call ID, final text arrives once, and existing safe refresh/cancellation tests remain green.

- [ ] **Step 7: Commit the agent integration**

Run:

```bash
git add src/codex_provider.rs tests/codex_provider.rs Cargo.lock
git commit -m "feat: run read tool through Rig agent"
```

Expected: the commit contains only agent-run-loop adaptation and its HTTP-level tests.

### Task 4: Document and verify the finished feature

**Files:**

- Modify: `README.md:1-90`
- Test: all existing test targets

**Interfaces:**

- Consumes the user-visible behavior completed in Tasks 1-3.
- Produces accurate developer documentation without promising unimplemented editing or image support.

- [ ] **Step 1: Update README behavior and state-location documentation**

Add a short paragraph stating that Moh's Codex agent can read text files through a `read` tool; successful rows use durable `HASH│content` anchors; relative paths use the current working directory and absolute paths are allowed. State that anchor state is stored in Moh's platform state directory through `directories` (`$XDG_STATE_HOME/moh` or `~/.local/state/moh` on Linux), not in configuration. State that editing and image attachments are not yet supported.

- [ ] **Step 2: Verify the documentation diff**

Run: `git diff --check && git diff -- README.md`

Expected: no whitespace errors and no claim that write, replace, undo, image rendering, or workspace confinement exists.

- [ ] **Step 3: Run the complete verification suite**

Run:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

Expected: all commands exit successfully. If any existing test fails, diagnose and repair the regression before proceeding; do not weaken or delete it.

- [ ] **Step 4: Inspect final scope and commit documentation**

Run:

```bash
git status --short
git add README.md
git diff --cached --check
git commit -m "docs: describe agent read tool"
git status --short
```

Expected: only intended feature commits are present and the working tree is clean. Do not stage credential files, SQLite state, temporary fixtures, or build output.
