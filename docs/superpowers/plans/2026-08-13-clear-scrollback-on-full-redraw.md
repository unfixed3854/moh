# Clear Scrollback on Full Redraw Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Purge saved terminal scrollback whenever the renderer must reconstruct the complete retained frame, preventing resize-driven transcript duplication.

**Architecture:** Keep the existing render-plan selection and differential behavior. Replace the full-reconstruction clear sequence with `CSI 3J`, `CSI 2J`, `CSI H` so both `RenderPlan::FullRedraw` and `finish` recovery purge saved lines before rebuilding the viewport; ordinary initial renders, appends, and safe rewrites remain unchanged.

**Tech Stack:** Rust 2024, the existing terminal escape-sequence renderer, `vt100` 0.16.2 for visible-screen assertions, and the existing recording terminal for byte-level assertions.

## Global Constraints

- Purging all saved lines during full reconstruction is intentional, including terminal output that predates `moh`.
- Initial rendering, ordinary appends, and safe differential rewrites must preserve scrollback.
- Do not add a dependency, render plan, error type, or renderer state field.
- Keep reconstruction and its purge inside the existing buffered terminal write and synchronized-update boundary.
- Preserve existing write and flush failure recovery semantics.
- The byte sequence must be exactly `\x1b[3J\x1b[2J\x1b[H`: purge saved lines, clear the visible display, then home the cursor.
- Before completion, run `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-targets`, and `cargo build --locked`.

---

### Task 1: Purge scrollback during full-frame reconstruction

**Files:**
- Modify: `tests/renderer.rs:68-258,631-722`
- Modify: `src/tui/renderer.rs:3-8,115-119,258-266`

**Interfaces:**
- Consumes: the existing `Renderer::render`, `Renderer::finish`, `RenderPlan::FullRedraw`, `RecordingTerminal::output`, and `Frame` test helpers.
- Produces: one private renderer constant, `PURGE_SCROLLBACK_CLEAR_SCREEN_AND_HOME: &str`, used by both full-redraw and finish-recovery reconstruction paths.

- [ ] **Step 1: Write byte-level regression assertions for purge and preservation paths**

Update the initial-frame and pure-append tests to prove normal scrollback-preserving paths do not purge saved lines:

```rust
assert!(!terminal.output().contains("\x1b[3J"));
```

Rename `width_change_forces_clear_and_home` to `width_change_purges_scrollback_before_clear_and_home`, and replace its final assertion with:

```rust
assert!(terminal.output().contains("\x1b[3J\x1b[2J\x1b[H"));
```

Strengthen `change_above_the_visible_viewport_forces_full_redraw` with the same exact-sequence assertion so a non-resize unsafe reconstruction is covered:

```rust
assert!(terminal.output().contains("\x1b[3J\x1b[2J\x1b[H"));
```

Update both finish-recovery prefix assertions to require the purge before clear-and-home:

```rust
assert!(
    terminal
        .output()
        .starts_with("\x1b[?2026h\x1b[3J\x1b[2J\x1b[H")
);
```

Keep the existing VT100 content and cursor assertions unchanged. `vt100` 0.16.2 ignores `CSI 3J`, so the recording terminal's exact bytes are the authoritative scrollback-purge check while VT100 continues to validate visible reconstruction.

- [ ] **Step 2: Run the renderer tests and verify the new regressions fail**

Run:

```bash
cargo test --test renderer
```

Expected: FAIL in the width-change, inaccessible-row, and finish-recovery assertions because renderer output still contains `\x1b[2J\x1b[H` without the preceding `\x1b[3J`. Existing preservation-path assertions should already pass.

- [ ] **Step 3: Implement the minimal shared purge sequence**

In `src/tui/renderer.rs`, replace the old constant:

```rust
const CLEAR_SCREEN_AND_HOME: &str = "\x1b[2J\x1b[H";
```

with:

```rust
const PURGE_SCROLLBACK_CLEAR_SCREEN_AND_HOME: &str = "\x1b[3J\x1b[2J\x1b[H";
```

Use `PURGE_SCROLLBACK_CLEAR_SCREEN_AND_HOME.as_bytes()` in exactly the two complete-frame reconstruction sites:

```rust
// Renderer::finish recovery
bytes.extend_from_slice(PURGE_SCROLLBACK_CLEAR_SCREEN_AND_HOME.as_bytes());

// RenderPlan::FullRedraw
bytes.extend_from_slice(PURGE_SCROLLBACK_CLEAR_SCREEN_AND_HOME.as_bytes());
```

Do not change `select_plan`, initial rendering, append rendering, changed-range rewriting, cursor movement, or renderer state transitions.

- [ ] **Step 4: Run focused tests and verify the fix passes**

Run:

```bash
cargo test --test renderer
```

Expected: all renderer tests pass. The full-reconstruction paths contain the exact purge-clear-home sequence, ordinary paths omit `CSI 3J`, and visible-screen/cursor assertions remain unchanged.

- [ ] **Step 5: Run the complete verification suite**

Run in order:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --locked
```

Expected: every command exits successfully with no formatting changes, Clippy warnings, test failures, or build errors.

- [ ] **Step 6: Commit the renderer fix and regressions**

```bash
git add src/tui/renderer.rs tests/renderer.rs
git commit -m "fix: purge scrollback on full redraw"
```
