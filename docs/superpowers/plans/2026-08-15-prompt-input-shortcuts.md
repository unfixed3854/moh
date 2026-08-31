# Prompt Input Shortcuts Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (- [ ]) syntax for tracking.

**Goal:** Add whitespace-delimited Ctrl word movement/deletion to moh's grapheme-aware prompt and render a styled model/request/cwd status line below the input.

**Architecture:** Keep editing behavior in the existing Input component, using grapheme indices for every cursor and deletion range. Keep status presentation in the existing demo Text root, with a small status formatter and a captured sanitized cwd; order demo roots as transcript, input, status.

**Tech Stack:** Rust 2024, unicode-segmentation, crossterm-normalized InputEvent, moh's ANSI-aware Text, Tokio demo loop, Cargo tests/Clippy.

## Global Constraints

- Preserve plain Left/Right/Home/End/Delete/Backspace behavior as grapheme-aware editing.
- Define words as maximal runs of non-whitespace Unicode graphemes; punctuation stays attached to the run.
- Support Ctrl+Left/Right and Ctrl+Backspace/Delete while continuing to ignore unsupported Ctrl and Alt combinations.
- Render the status below the focused prompt as: ╰─ gpt-5.6-luna · <state> · <cwd>.
- Use codex_provider::MODEL for the model label and sanitize cwd text before ANSI-aware rendering.
- Do not add a theme system, nested focus traversal, new dependencies, model/provider changes, or conversation-state changes.
- Keep assistant and error transcript text behind the existing plain-text sanitizer.
- Finish with cargo fmt --all -- --check, cargo clippy --all-targets --all-features -- -D warnings, cargo test --all-targets, cargo build --locked, and git diff --check.

---

### Task 1: Add word-wise prompt editing

**Files:**
- Modify: src/tui/components/input.rs:52-117,170-269 — add private grapheme classification, word-boundary movement, range deletion, and Ctrl dispatch.
- Modify: tests/components.rs:7-22,89-340 — add focused Ctrl movement/deletion tests using the existing control_key helper.

**Interfaces:**
- Consumes: the existing Input value/cursor representation, InputEvent::Key, Key::{Left,Right,Backspace,Delete}, Modifiers.control, and UnicodeSegmentation.
- Produces: private Input helpers that return a changed/not-changed boolean and preserve InputOutcome::{Changed,Consumed} semantics; no public API or normalized event changes.

- [ ] **Step 1: Write failing word-navigation and word-deletion tests**

Add tests beside the existing grapheme editing tests. Use these cases so cursor positions are observed through subsequent edits rather than private state:

```rust
#[test]
fn input_control_navigation_moves_by_whitespace_delimited_words() {
    let mut input = Input::new("");
    input.set_value("one  👩‍💻 two");
    input.set_focused(true);

    input.handle_input(&key(Key::Home));
    assert_eq!(
        input.handle_input(&control_key(Key::Right)),
        InputOutcome::Changed
    );
    assert_eq!(
        input.handle_input(&control_key(Key::Delete)),
        InputOutcome::Changed
    );
    assert_eq!(input.value(), "one  two");

    input.set_value("one  👩‍💻 two");
    assert_eq!(
        input.handle_input(&control_key(Key::Left)),
        InputOutcome::Changed
    );
    assert_eq!(
        input.handle_input(&control_key(Key::Backspace)),
        InputOutcome::Changed
    );
    assert_eq!(input.value(), "one  two");

    input.set_value("one  👩‍💻 two");
    input.handle_input(&key(Key::Home));
    input.handle_input(&control_key(Key::Right));
    input.handle_input(&control_key(Key::Right));
    input.handle_input(&control_key(Key::Right));
    assert_eq!(
        input.handle_input(&control_key(Key::Right)),
        InputOutcome::Consumed
    );
}

#[test]
fn input_control_deletion_removes_adjacent_word_and_whitespace() {
    let mut input = Input::new("");
    input.set_focused(true);

    input.set_value("one  two  ");
    assert_eq!(
        input.handle_input(&control_key(Key::Backspace)),
        InputOutcome::Changed
    );
    assert_eq!(input.value(), "one  ");

    input.set_value("one  two  three");
    input.handle_input(&key(Key::Home));
    assert_eq!(
        input.handle_input(&control_key(Key::Delete)),
        InputOutcome::Changed
    );
    assert_eq!(input.value(), "two  three");

    input.set_value("one  two");
    input.handle_input(&key(Key::Home));
    for _ in 0..3 {
        input.handle_input(&key(Key::Right));
    }
    assert_eq!(
        input.handle_input(&control_key(Key::Delete)),
        InputOutcome::Changed
    );
    assert_eq!(input.value(), "one");

    input.set_value("界 👩‍💻");
    input.handle_input(&key(Key::Home));
    assert_eq!(
        input.handle_input(&control_key(Key::Delete)),
        InputOutcome::Changed
    );
    assert_eq!(input.value(), "👩‍💻");
    input.set_value("界 👩‍💻");
    assert_eq!(
        input.handle_input(&control_key(Key::Backspace)),
        InputOutcome::Changed
    );
    assert_eq!(input.value(), "界 ");
    input.set_value("界");
    assert_eq!(
        input.handle_input(&control_key(Key::Delete)),
        InputOutcome::Consumed
    );
}
```

Add one assertion that unsupported Ctrl navigation does not fall through to the plain behavior:

```rust
input.set_value("one two");
input.set_focused(true);
assert_eq!(
    input.handle_input(&control_key(Key::Home)),
    InputOutcome::Ignored
);
```

- [ ] **Step 2: Run the new tests and verify the current implementation fails**

Run:

```bash
cargo test --test components input_control_navigation_moves_by_whitespace_delimited_words -- --exact
cargo test --test components input_control_deletion_removes_adjacent_word_and_whitespace -- --exact
```

Expected: both tests fail because the current modifier guard ignores Ctrl+Left/Right/Backspace/Delete, and Ctrl+Home must remain ignored after the implementation is changed.

- [ ] **Step 3: Implement grapheme-safe word helpers**

Add private helpers in Input:

```rust
fn grapheme_at(&self, index: usize) -> Option<&str>;
fn is_whitespace(grapheme: &str) -> bool;
fn move_word_left(&mut self) -> bool;
fn move_word_right(&mut self) -> bool;
fn delete_word_before_cursor(&mut self) -> bool;
fn delete_word_at_cursor(&mut self) -> bool;
```

Implement is_whitespace with grapheme.chars().all(char::is_whitespace). Implement left movement by repeatedly skipping whitespace before the cursor and then non-whitespace graphemes before that. Implement right movement as follows: if the cursor is in whitespace, skip that whitespace and stop at the next non-whitespace grapheme; otherwise skip the remainder of the current word and then following whitespace. Return false when the cursor does not move.

For backward deletion, walk a temporary end index from the cursor left across whitespace and then the preceding non-whitespace run, replace the byte range with value.replace_range, and set cursor_grapheme to the range start. When the cursor is inside a word, this removes only the word prefix and preserves the preceding separator. For forward deletion, start at the cursor, skip whitespace when currently in whitespace, skip the next non-whitespace run, then skip its following whitespace, and delete that complete grapheme-index range. Return false at the input end or for an empty range.

Convert grapheme indices to byte offsets with UnicodeSegmentation::grapheme_indices; never slice the string at a scalar or byte index derived from a non-grapheme boundary.

- [ ] **Step 4: Route only the supported Ctrl shortcuts**

Preserve Alt as a disallowed modifier. Match Ctrl+Left/Right/Backspace/Delete before the plain editing arms and call the corresponding helpers. Add a later modifiers.control => InputOutcome::Ignored guard so Ctrl+Home, Ctrl+End, Ctrl+Escape, Ctrl+Enter, and Ctrl+character cannot fall through to plain behavior. Keep plain character input unchanged when Control is not pressed.

- [ ] **Step 5: Run the focused and existing component tests**

Run:

```bash
cargo test --test components
```

Expected: all component tests pass, including existing combining-mark, emoji, cursor, sanitization, and plain grapheme editing coverage.

- [ ] **Step 6: Commit the editor change**

```bash
git add src/tui/components/input.rs tests/components.rs
git commit -m "feat: add word-wise prompt editing shortcuts"
```

### Task 2: Add the styled status line below the prompt

**Files:**
- Modify: src/demo.rs:1-23,73-91,197-231,284-531 — capture cwd, reorder roots, format status states, update lifecycle text, and test rendering/status transitions.
- Read: src/codex_provider.rs:34-38 — use the existing public MODEL constant without modifying provider code.

**Interfaces:**
- Consumes: codex_provider::MODEL, Input::sanitize_plain_text, Text::set_text, Tui::component_mut, and the existing DemoIds/conversation lifecycle.
- Produces: private StatusState, status_line, and set_status helpers; DemoIds.cwd stores the sanitized cwd used by every status update.

- [ ] **Step 1: Add failing demo tests for layout, metadata, and help copy**

Import MODEL and Input in the demo test module and add this render assertion using the existing RecordingTerminal:

```rust
#[test]
fn build_places_status_below_prompt_and_includes_model_and_cwd() {
    let terminal = RecordingTerminal::new(None);
    let bytes = Rc::clone(&terminal.bytes);
    let (mut tui, _ids) = build(terminal).unwrap();
    tui.render_now().unwrap();

    let output = String::from_utf8(bytes.borrow().clone()).unwrap();
    let prompt = output.find('❯').expect("prompt marker");
    let status = output.find("ready").expect("ready status");
    let cwd = Input::sanitize_plain_text(
        &std::env::current_dir()
            .unwrap()
            .to_string_lossy(),
    );

    assert!(prompt < status);
    assert!(output.contains(MODEL));
    assert!(output.contains(&cwd));
}
```

Add a help-copy assertion:

```rust
#[test]
fn help_lists_word_editing_shortcuts() {
    assert!(HELP.contains("Ctrl+Left/Right"));
    assert!(HELP.contains("Ctrl+Backspace/Delete"));
}
```

Extend the existing successful and failed request tests so their captured output asserts gpt-5.6-luna, thinking..., ready, error, and the sanitized cwd. Check these labels independently because ANSI color sequences are intentionally placed between status segments.

- [ ] **Step 2: Run the new demo tests and verify they fail**

Run:

```bash
cargo test --bin moh build_places_status_below_prompt_and_includes_model_and_cwd -- --exact
cargo test --bin moh help_lists_word_editing_shortcuts -- --exact
```

Expected: the layout test fails because the current status is empty and precedes the input; the help test fails because the current help copy has no word shortcuts. If Cargo reports the binary test target under a different name, run the exact test names with cargo test build_places_status_below_prompt_and_includes_model_and_cwd -- --exact and cargo test help_lists_word_editing_shortcuts -- --exact.

- [ ] **Step 3: Add the status formatter and captured cwd**

Define:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StatusState {
    Ready,
    Thinking,
    Error,
}

fn status_line(state: StatusState, cwd: &str) -> String;

fn set_status<T: Terminal>(
    tui: &mut Tui<T>,
    ids: &DemoIds,
    state: StatusState,
) -> std::result::Result<(), DemoError>;
```

Format the line as ╰─ gpt-5.6-luna · <state> · <cwd> using existing ANSI sequences: dim prefix/cwd, cyan model, green ready, yellow thinking..., and red error; terminate with the existing reset sequence expected by Text/Tui.

In build, read the cwd with std::env::current_dir().map_err(RenderError::from)?, convert it with to_string_lossy, sanitize it through Input::sanitize_plain_text, and store it in a new DemoIds.cwd field. Insert roots in this order:

```rust
let transcript = tui.add_component(transcript);
let input = tui.add_component(Input::new("❯ "));
let status = tui.add_component(Text::new(status_line(StatusState::Ready, &cwd)));
```

Keep focus on input. Remove the duplicated model name from INTRODUCTION and add these help lines:

```text
Ctrl+Left/Right move by word
Ctrl+Backspace/Delete delete by word
```

- [ ] **Step 4: Connect request lifecycle states**

Replace the literal status updates in begin_request and apply_response with set_status:

```rust
set_status(tui, ids, StatusState::Thinking)?;
```

On successful resolve_turn, append the answer and set Ready. On the error branch, append the sanitized error and set Error. Keep tui.request_render() after the lifecycle mutation so the input cursor and status line are redrawn together.

- [ ] **Step 5: Run focused demo tests, then the complete demo test module**

Run:

```bash
cargo test --bin moh build_places_status_below_prompt_and_includes_model_and_cwd -- --exact
cargo test --bin moh help_lists_word_editing_shortcuts -- --exact
cargo test --bin moh
```

Expected: the two focused tests pass, then all existing async request, sanitization, cancellation, exit, resize, and help tests pass with the input/status order changed.

- [ ] **Step 6: Commit the demo presentation change**

```bash
git add src/demo.rs
git commit -m "feat: add prompt status line"
```

### Task 3: Document controls and perform final verification

**Files:**
- Modify: README.md:7-12,70-82 — document prompt editing shortcuts and the status line.

**Interfaces:**
- Consumes: the implemented prompt behavior and status labels from Tasks 1 and 2.
- Produces: user-facing documentation and a verified branch with no formatting, lint, test, build, or whitespace failures.

- [ ] **Step 1: Update the README usage description**

Change the demo description to mention Left/Right, Home/End, Delete/Backspace, Ctrl+Left/Right, Ctrl+Backspace/Delete, Enter submission, help, and exit. Add that the bottom status line shows the active model, ready/thinking.../error state, and cwd. Keep the existing authentication and development instructions unchanged.

- [ ] **Step 2: Run the full validation sequence**

Run each command and require a successful exit:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --locked
git diff --check
```

The expected test result is the full existing suite plus the new component/demo coverage, with no live Codex test required for this UI-only change.

- [ ] **Step 3: Inspect the final diff and commit documentation**

```bash
git status -sb
git diff --stat
git diff -- README.md
git add README.md
git commit -m "docs: document prompt editing controls"
git status -sb
```

Expected: the three implementation commits plus the already committed spec are present on the feature branch, the final status is clean, and no generated files or credential material are staged.
