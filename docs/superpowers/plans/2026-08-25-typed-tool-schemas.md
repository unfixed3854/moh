# Typed Tool Schemas Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Generate every model-visible tool schema from its typed Rust argument struct while validating structural arguments with Garde.

**Architecture:** Argument structs become the schema and structural-validation source of truth by deriving Serde, Schemars, and Garde traits. Services retain validation that requires filesystem, anchor, or job-registry state; Rig adapters generate their `PortableTool::parameters()` values from the argument type.

**Tech Stack:** Rust 2024, Serde, Schemars 1.2.2, Garde 0.23.0, Rig 0.41.0, Tokio, Cargo.

**Spec:** `docs/superpowers/specs/2026-08-25-typed-tool-schemas-design.md`

## Global Constraints

- Keep `#[serde(deny_unknown_fields)]` on every tool argument struct.
- Remove `read.file_path`; `read.path` is required and non-empty.
- Preserve names, descriptions, runtime error codes, filesystem safety checks, and job lifecycle behavior.
- Preserve `job_wait.timeout_ms = 0` as a valid immediate-timeout value; its maximum is 300,000.
- Keep Bash `timeout_ms` in the inclusive 1..=3,600,000 range.
- Do not reintroduce hand-written `serde_json::json!` schemas.
- Validate with `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-targets`, and `cargo build --locked`.

---

## File structure

- `Cargo.toml`: declare direct schema-generation and validation dependencies.
- `Cargo.lock`: lock the new direct validation dependency and its transitive crates.
- `src/tools/read.rs`: make `ReadArgs` a derived required-path contract and enforce its structural validation before filesystem work.
- `src/tools/write.rs`, `src/tools/edit.rs`, `src/tools/bash.rs`, `src/tools/job.rs`: derive typed schemas and field validators, while retaining stateful checks in their services.
- `src/runtime/rig/{read_tool,write_tool,edit_tool,bash_tool,job_tool}.rs`: obtain each `PortableTool` parameter value through `schemars::schema_for!` rather than a service schema method.
- `tests/{read_tool,write_tool,edit_tool,bash_tool,job_tool,rig_runtime}.rs`: assert validation and the important generated-schema and emitted-payload contracts.
- `README.md`: remove the retired `file_path` wording only if it appears outside generated code or tests.

### Task 1: Add the schema and validator toolchain

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`

**Interfaces:**
- Produces: direct `schemars::{JsonSchema, schema_for}` and `garde::Validate` imports available to Moh's code and integration tests.
- Produces: Garde pattern support for the edit anchor and replacement-line field rules.

- [ ] **Step 1: Declare direct dependencies**

  Add these entries in `Cargo.toml` alongside the existing runtime dependencies:

  ```toml
  garde = { version = "0.23.0", features = ["derive", "pattern"] }
  schemars = "1.2.2"
  ```

  Run `cargo check` to resolve and lock Garde. Do not rely on Rig's transitive Schemars dependency.

- [ ] **Step 2: Commit the dependency setup**

  ```bash
  git add Cargo.toml Cargo.lock
  git commit -m "build: add tool schema dependencies"
  ```

### Task 2: Convert file-tool arguments and retire the read alias

**Files:**
- Modify: `src/tools/read.rs`
- Modify: `src/tools/write.rs`
- Modify: `src/tools/edit.rs`
- Modify: `tests/read_tool.rs`
- Modify: `tests/write_tool.rs`
- Modify: `tests/edit_tool.rs`

**Interfaces:**
- Consumes: Schemars and Garde direct dependencies from Task 1.
- Produces: `ReadArgs { path: String, offset: Option<u64>, limit: Option<u64> }` with no `file_path` field.
- Produces: `ReadArgs`, `WriteArgs`, and `EditArgs` implementations of `JsonSchema` and `Validate`.

- [ ] **Step 1: Write failing file-tool contract tests**

  Import `garde::Validate` and `schemars::schema_for` in the file-tool tests. Update all `ReadArgs` literals to use the required string field and remove the alias test. Add these assertions to the relevant existing argument-contract tests:

  ```rust
  assert!(serde_json::from_value::<ReadArgs>(json!({"file_path": "old.txt"})).is_err());
  assert!(ReadArgs { path: String::new(), offset: None, limit: None }
      .validate()
      .is_err());
  assert!(EditArgs {
      path: "note.txt".into(),
      remove_from: "ab!".into(),
      remove_to: "abc".into(),
      replacement_lines: vec![],
  }.validate().is_err());
  assert!(EditArgs {
      path: "note.txt".into(),
      remove_from: "abc".into(),
      remove_to: "def".into(),
      replacement_lines: vec!["line\nnext".into()],
  }.validate().is_err());
  ```

  Also add the derived-schema assertion:

  ```rust
  let schema = serde_json::to_value(schema_for!(ReadArgs)).unwrap();
  assert_eq!(schema["additionalProperties"], false);
  assert_eq!(schema["required"], json!(["path"]));
  assert_eq!(schema["properties"]["offset"]["minimum"], 1);
  assert!(schema["properties"].get("file_path").is_none());
  ```

- [ ] **Step 2: Run the file-tool tests to verify failure**

  Run: `cargo test --test read_tool --test write_tool --test edit_tool`

  Expected: FAIL because the old `ReadArgs` shape and manual schemas remain.

- [ ] **Step 3: Derive field constraints and preserve stateful service validation**

  Replace `ReadArgs`' custom deserializer and `RawReadArgs` with a derived struct:

  ```rust
  #[derive(Debug, Deserialize, JsonSchema, Validate)]
  #[serde(deny_unknown_fields)]
  pub struct ReadArgs {
      #[garde(length(min = 1))]
      pub path: String,
      #[garde(range(min = 1))]
      pub offset: Option<u64>,
      #[garde(range(min = 1))]
      pub limit: Option<u64>,
  }
  ```

  Derive the same traits on `WriteArgs` and `EditArgs`. Apply `length(min = 1)` to file paths; apply `pattern(r"^[A-Za-z0-9]{3}$")` to edit anchors; apply `inner(pattern(r"^[^\\r\\n]*$"))` to `replacement_lines`.

  At the beginning of each public service operation, call `args.validate()` and map it to that service's existing `InvalidArgument` error variant. Leave the old manual `parameters()` methods in place temporarily so the unchanged Rig adapters compile; Task 4 removes all of them. Keep edit's read observation, anchor lookup, ordering, stale-file, newline, BOM, permission, and atomic-replacement logic unchanged.

- [ ] **Step 4: Update direct callers and run file-tool tests**

  Change every `ReadArgs` construction across the repository to pass `path: ...` directly. Keep `ReadArgs::path()` as the concise constructor, updated for the new shape. Run:

  ```bash
  cargo test --test read_tool --test write_tool --test edit_tool
  ```

  Expected: PASS. The deleted `file_path` alias must deserialize as an unknown field, while valid direct service calls continue to preserve anchored reads, guarded writes, and edits.

- [ ] **Step 5: Commit the file-tool conversion**

  ```bash
  git add src/tools/read.rs src/tools/write.rs src/tools/edit.rs \
    tests/read_tool.rs tests/write_tool.rs tests/edit_tool.rs
  git commit -m "refactor(tools): derive file tool schemas"
  ```

### Task 3: Convert Bash and job argument contracts

**Files:**
- Modify: `src/tools/bash.rs`
- Modify: `src/tools/job.rs`
- Modify: `tests/bash_tool.rs`
- Modify: `tests/job_tool.rs`

**Interfaces:**
- Consumes: Schemars and Garde dependencies from Task 1.
- Produces: derived `JsonSchema` and `Validate` implementations for `BashArgs`, `JobStatusArgs`, `JobWaitArgs`, and `JobCancelArgs`.
- Produces: service-level Garde validation before Bash spawning or job-registry access.

- [ ] **Step 1: Write failing validation and schema tests**

  In `tests/bash_tool.rs`, replace `BashService::parameters()` with `schema_for!(BashArgs)` and add direct `.validate()` assertions for an empty command, zero timeout, and timeout `3_600_001`.

  In `tests/job_tool.rs`, use `schema_for!(JobWaitArgs)` and assert:

  ```rust
  assert!(JobWaitArgs { job_ids: vec![], timeout_ms: None }.validate().is_err());
  assert!(JobWaitArgs { job_ids: vec!["job-0".into()], timeout_ms: Some(0) }
      .validate()
      .is_ok());
  assert!(JobWaitArgs { job_ids: vec!["job-0".into()], timeout_ms: Some(300_001) }
      .validate()
      .is_err());
  ```

- [ ] **Step 2: Run the focused tests to verify failure**

  Run: `cargo test --test bash_tool --test job_tool`

  Expected: FAIL because these types do not yet derive their schema and validation traits.

- [ ] **Step 3: Move declarative Bash/job constraints into derives**

  Derive `JsonSchema` and `Validate` on all four argument structs. Use these Garde rules:

  ```rust
  #[garde(length(min = 1))]
  pub command: String,
  #[garde(range(min = 1, max = 3_600_000))]
  pub timeout_ms: Option<u64>,
  #[garde(length(min = 1))]
  pub job_ids: Vec<String>,
  #[garde(range(min = 0, max = 300_000))]
  pub timeout_ms: Option<u64>,
  ```

  Replace `BashService::validate` and the empty-list/max-wait portion of `JobService::wait` with calls to the generated validator. Preserve Bash spawn, output, timeout, cancellation, and registry error behavior. Preserve `JobService::parse_id`, registry lookup, waiting, and cancellation checks. Leave Bash and JobService's manual parameter-schema methods temporarily for the unchanged adapters; Task 4 removes them.

- [ ] **Step 4: Run focused tool tests**

  Run: `cargo test --test bash_tool --test job_tool`

  Expected: PASS. In particular, zero `job_wait.timeout_ms` remains valid and Bash zero timeout remains invalid.

- [ ] **Step 5: Commit the execution/job conversion**

  ```bash
  git add src/tools/bash.rs src/tools/job.rs tests/bash_tool.rs tests/job_tool.rs
  git commit -m "refactor(tools): derive execution tool schemas"
  ```

### Task 4: Generate Rig parameters from argument types

**Files:**
- Modify: `src/runtime/rig/read_tool.rs`
- Modify: `src/runtime/rig/write_tool.rs`
- Modify: `src/runtime/rig/edit_tool.rs`
- Modify: `src/runtime/rig/bash_tool.rs`
- Modify: `src/runtime/rig/job_tool.rs`
- Modify: `tests/rig_runtime.rs`

**Interfaces:**
- Consumes: every `PortableTool::Args` type implements `schemars::JsonSchema` from Tasks 2 and 3.
- Produces: every adapter's `parameters()` serializes `schemars::schema_for!(Args)` without service schema APIs.

- [ ] **Step 1: Strengthen the emitted-runtime-schema test**

  In `tests/rig_runtime.rs`, extend the initial request assertions to require:

  ```rust
  assert!(tools.iter().any(|tool| {
      tool["name"] == "read"
          && tool["parameters"]["required"] == json!(["path"])
          && tool["parameters"]["properties"].get("file_path").is_none()
          && tool["parameters"]["additionalProperties"] == false
  }));
  assert!(tools.iter().any(|tool| {
      tool["name"] == "job_wait"
          && tool["parameters"]["properties"]["timeout_ms"]["minimum"] == 0
  }));
  ```

- [ ] **Step 2: Run the runtime test to verify failure**

  Run: `cargo test --test rig_runtime runtime_registers_tools`

  Expected: FAIL because adapters still delegate to the manual schemas and advertise the old read alias.

- [ ] **Step 3: Replace service-schema delegation in all adapters**

  In each adapter, import `schemars::schema_for` and use its concrete argument type, for example:

  ```rust
  fn parameters(&self) -> serde_json::Value {
      serde_json::to_value(schema_for!(ReadArgs))
          .expect("derived tool schema must serialize")
  }
  ```

  Use `WriteArgs`, `EditArgs`, and `BashArgs` in their respective adapters. In the `job_tool!` macro, use `schema_for!($args)` rather than accepting a `$parameters` identifier, then remove that macro parameter and its three invocations' schema-method arguments. Delete every service-level manual parameter-schema method after no adapter calls one. Do not alter names, descriptions, `map_error`, ownership, or `call` routing.

- [ ] **Step 4: Run all Rig runtime tests**

  Run: `cargo test --test rig_runtime`

  Expected: PASS. The Responses payload has seven tools with generated strict schemas, required `read.path`, and no `file_path` property.

- [ ] **Step 5: Commit the runtime-boundary conversion**

  ```bash
  git add src/runtime/rig/read_tool.rs src/runtime/rig/write_tool.rs \
    src/runtime/rig/edit_tool.rs src/runtime/rig/bash_tool.rs \
    src/runtime/rig/job_tool.rs tests/rig_runtime.rs
  git commit -m "refactor(runtime): generate tool parameters"
  ```

### Task 5: Final regression sweep and documentation check

**Files:**
- Modify: `README.md` only if it documents the retired `file_path` alias.
- Verify: `Cargo.toml`, `Cargo.lock`, `src/tools/*.rs`, `src/runtime/rig/*.rs`, `tests/*.rs`

**Interfaces:**
- Consumes: complete generated-schema implementation from Tasks 1-4.
- Produces: a clean, formatted, fully validated repository state.

- [ ] **Step 1: Search for stale manual-schema and alias references**

  Run:

  ```bash
  rg -n 'file_path|fn (status_parameters|wait_parameters|cancel_parameters|parameters)\(|serde_json::json!' \
    README.md src/tools src/runtime/rig tests
  ```

  Expected: no production `file_path` field or manual parameter-schema builders; remaining `serde_json::json!` uses are test fixtures or unrelated transport payload construction.

- [ ] **Step 2: Update an affected README sentence if search finds one**

  Ensure the agent-file-access documentation names `path` as the only read-path argument. Do not change unrelated behavior or examples.

- [ ] **Step 3: Run formatting and static checks**

  Run:

  ```bash
  cargo fmt --all --check
  cargo clippy --all-targets --all-features -- -D warnings
  ```

  Expected: both commands exit 0. If formatting fails, run `cargo fmt --all`, inspect the diff, then re-run the check.

- [ ] **Step 4: Run the full test suite and locked build**

  Run:

  ```bash
  cargo test --all-targets
  cargo build --locked
  ```

  Expected: both commands exit 0.

- [ ] **Step 5: Commit final cleanup**

  ```bash
  git add README.md Cargo.toml Cargo.lock src tests
  git commit -m "test: verify typed tool schemas"
  ```
