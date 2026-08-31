# Promote Demo to App Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rename the executable product layer and its current documentation from `demo` to `app` without changing behavior.

**Architecture:** Keep the binary-only application in one private module, renamed from `src/demo.rs` to `src/app.rs`. Rename only application-owned `Demo*` identifiers and current descriptive language; retain the reusable library boundary and preserve historical specs and plans unchanged.

**Tech Stack:** Rust 2024, Cargo, Tokio, Clippy

**Spec:** `docs/superpowers/specs/2026-08-18-promote-demo-to-app-design.md`

## Global Constraints

- This is a behavior-preserving naming change.
- Do not reorganize the application, alter public APIs, or change runtime behavior.
- Keep the application module private to the binary.
- Do not split `src/app.rs` into submodules.
- Preserve historical files under `docs/superpowers/specs/` and `docs/superpowers/plans/` unchanged.
- No live Codex request is required.

---

### Task 1: Promote the executable module from demo to app

**Files:**
- Rename: `src/demo.rs` to `src/app.rs`
- Modify: `src/app.rs`
- Modify: `src/main.rs`
- Modify: `src/lib.rs`
- Modify: `README.md`

**Interfaces:**
- Consumes: the existing private `demo::run() -> impl Future<Output = Result<(), DemoError>>` binary entry point and unchanged public `moh` library APIs.
- Produces: private `app::run() -> impl Future<Output = Result<(), AppError>>`, `AppIds`, `AppAction`, and `AppError`; runtime behavior and public library APIs remain unchanged.

- [ ] **Step 1: Rename the module file without updating its caller**

Use `apply_patch` to move `src/demo.rs` to `src/app.rs`, changing only the
declaration of `DemoIds` in the same patch so the move contains an explicit
hunk:

```diff
*** Begin Patch
*** Update File: src/demo.rs
*** Move to: src/app.rs
@@
-pub struct DemoIds {
+pub struct AppIds {
*** End Patch
```

This deliberately creates a compile-time structural failure before the module declaration is corrected.

- [ ] **Step 2: Verify the old module declaration fails**

Run:

```bash
cargo test --bin moh app::tests
```

Expected: FAIL because `src/main.rs` still declares `mod demo;` and `src/demo.rs` no longer exists.

- [ ] **Step 3: Rename the private module and application-owned identifiers**

In `src/main.rs`, make these exact replacements:

```rust
mod app;
```

```rust
match run_with_current_thread_runtime(app::run) {
```

In `src/app.rs`, replace every remaining whole identifier consistently:

```text
DemoIds    -> AppIds
DemoAction -> AppAction
DemoError  -> AppError
```

Update the test name `normal_exit_finishes_below_the_demo_content` to:

```rust
async fn normal_exit_finishes_below_the_application_content()
```

Do not change function signatures, control flow, strings visible in the terminal, or test expectations beyond application/demo terminology.

- [ ] **Step 4: Update current project documentation**

In `src/lib.rs`, replace the crate description with:

```rust
//! Retained, main-screen terminal UI primitives used by the `moh` application.
```

In `README.md`, change the run instruction from “Run the mini-chat demo” to “Run the application” and change “The demo requires an interactive terminal” to “The application requires an interactive terminal.” Preserve all commands, controls, authentication instructions, and development instructions.

Do not edit any pre-existing file under `docs/superpowers/specs/` or `docs/superpowers/plans/`.

- [ ] **Step 5: Prove active code and documentation no longer use demo terminology**

Run:

```bash
rg -n '\bdemo\b|\bDemo[A-Za-z]*\b' src README.md
```

Expected: no matches and exit status 1. Matches in historical files under `docs/superpowers/` are intentionally out of scope.

- [ ] **Step 6: Run focused application tests**

Run:

```bash
cargo test --bin moh app::tests
```

Expected: PASS for every test in the renamed `app::tests` module.

- [ ] **Step 7: Run the complete verification suite**

Run:

```bash
cargo fmt --all -- --check
cargo test --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: all commands exit successfully with no formatting differences, test failures, or Clippy warnings.

- [ ] **Step 8: Review the final diff for behavior preservation**

Run:

```bash
git status --short
git diff --stat
git diff -- src/app.rs src/main.rs src/lib.rs README.md
```

Expected: Git records `src/demo.rs` as renamed to `src/app.rs`; other changes are limited to the planned identifiers and current descriptive wording. There are no changes to historical specs or plans other than this new implementation plan.

- [ ] **Step 9: Commit the implementation**

```bash
git add src/app.rs src/demo.rs src/main.rs src/lib.rs README.md
git commit -m "refactor: promote demo module to app"
```
