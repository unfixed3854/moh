# Multiline Prompt Editor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (- [ ]) syntax for tracking.

**Goal:** Add grapheme-safe multiline prompt composition with Shift+Enter while retaining Enter submission and existing TUI controls.

**Architecture:** PromptEditor owns multiline text, cursor, visual-row projection, and viewport. view.rs reserves one through four prompt rows, renders them, and pins status at the bottom. app.rs preserves popup precedence and asks the editor whether End is at the final line.

**Tech Stack:** Rust 2024, Crossterm 0.29, Ratatui 0.30 TestBackend, unicode-segmentation, unicode-width.

**Spec:** docs/superpowers/specs/2026-08-28-multiline-prompt-editor-design.md

## Global Constraints

- Plain Enter submits; Shift+Enter inserts one LF.
- Paste preserves LF, normalizes CRLF/CR to LF, maps Tab to one space, and removes other C0/C1 controls.
- Prompt height is between one and four rows while a transcript and status row remain available.
- Menus own Up/Down before the editor. Existing Ctrl+C, help, slash command, transcript-scroll, and busy-submission behavior remains intact.
- Render tests assert cells and cursor coordinates, not dimensions alone.
- Final gate: cargo fmt --all -- --check; cargo clippy --all-targets --all-features -- -D warnings; cargo test --all-targets; cargo build --locked; git diff --check.

---

### Task 1: Multiline Editor State

**Files:**

- Modify: src/client/ui/editor.rs:8-367
- Modify: src/client/ui/mod.rs:150-169

**Interfaces:**

- Produces: PromptEditor::{at_final_line_end, visual_height, display_window}.
- Produces: EditorWindow { lines: Vec<String>, cursor_row: u16, cursor_column: u16 }.

- [ ] **Step 1: Write failing tests**

Add these to the editor.rs test module, reusing key and modified.

~~~rust
#[test]
fn shift_enter_inserts_a_newline_and_enter_submits_it() {
    let mut editor = PromptEditor::new();
    editor.handle_event(&Event::Paste("first".into()));
    assert_eq!(editor.handle_event(&modified(KeyCode::Enter, KeyModifiers::SHIFT)), EditorOutcome::Changed);
    editor.handle_event(&Event::Paste("second".into()));
    assert_eq!(editor.value(), "first\nsecond");
    assert_eq!(editor.handle_event(&key(KeyCode::Enter)), EditorOutcome::Submitted("first\nsecond".into()));
}

#[test]
fn multiline_paste_normalizes_line_endings_and_controls() {
    let mut editor = PromptEditor::new();
    editor.handle_event(&Event::Paste("one\r\ntwo\rthree\t\x1b[2Jfour".into()));
    assert_eq!(editor.value(), "one\ntwo\nthree [2Jfour");
}

#[test]
fn vertical_navigation_and_viewport_keep_the_cursor_visible() {
    let mut editor = PromptEditor::new();
    editor.set_value("a\nb\nc\nd\ne");
    editor.display_window(2, 2);
    assert_eq!(editor.handle_event(&key(KeyCode::Up)), EditorOutcome::Changed);
    let window = editor.display_window(2, 2);
    assert_eq!(window.lines, ["c", "d"]);
    assert_eq!(window.cursor_row, 1);

    editor.set_value("abcd");
    editor.display_window(2, 2);
    assert_eq!(editor.handle_event(&key(KeyCode::Up)), EditorOutcome::Changed);
    assert_eq!(editor.display_window(2, 2).cursor_row, 0);
}
~~~

- [ ] **Step 2: Verify the intended behavior fails**

Run: cargo test --bin moh client::ui::editor::tests

Expected: FAIL because the editor flattens newlines and has no visual-row viewport.

- [ ] **Step 3: Implement sanitation, visual rows, and navigation**

Keep sanitize_line for one-line untrusted display. Add a private sanitize_editor_text in editor.rs, called by set_value, pasted text, and scalar insertion:

~~~rust
fn sanitize_editor_text(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\r' => {
                if characters.peek() == Some(&'\n') { characters.next(); }
                output.push('\n');
            }
            '\n' => output.push('\n'),
            '\t' => output.push(' '),
            '\u{0000}'..='\u{001f}' | '\u{007f}'..='\u{009f}' => {}
            printable => output.push(printable),
        }
    }
    output
}
~~~

Replace scroll_column with a private visual-row builder that splits at LF and before a grapheme exceeding nonzero width. It records row start/end grapheme indexes and cell widths; empty text and empty logical lines remain editable rows. Implement:

~~~rust
pub(crate) fn at_final_line_end(&self) -> bool;
pub(crate) fn visual_height(&self, width: u16) -> u16;
pub(crate) fn display_window(&mut self, width: u16, visible_height: u16) -> EditorWindow;
~~~

Shift+Enter inserts "\n"; modifier-free Enter submits. Home/End use the current logical line. Up/Down use the last nonzero display width, preserve preferred cell column, and clamp at target row boundaries. Reset preferred column and viewport state after horizontal movement, edits, set, clear, and submit.

- [ ] **Step 4: Run all editor tests**

Run: cargo test --bin moh client::ui::editor::tests

Expected: PASS with existing grapheme, word, sanitizer, and multiline tests green.

- [ ] **Step 5: Commit Task 1**

~~~bash
git add src/client/ui/editor.rs src/client/ui/mod.rs
git commit -m "feat(tui): support multiline prompt editing"
~~~

### Task 2: Capped Prompt Rendering

**Files:**

- Modify: src/client/ui/view.rs:18-70,397-411,487-890

**Interfaces:**

- Consumes: visual_height(width) and display_window(width, visible_height).
- Produces: prompt_height(area, editor) -> u16 clamped to one through four rows.

- [ ] **Step 1: Write failing TestBackend assertions**

Add these tests in view.rs using draw and status_row.

~~~rust
#[test]
fn multiline_prompt_aligns_continuations_and_pins_status() {
    let snapshot = snapshot_fixture();
    let mut ui = UiState::new();
    ui.editor_mut().set_value("first\nsecond");
    let terminal = draw(&snapshot, &mut ui, 20, 8);
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(0, 5)].symbol(), "❯");
    assert_eq!(buffer[(2, 5)].symbol(), "f");
    assert_eq!(buffer[(0, 6)].symbol(), " ");
    assert_eq!(buffer[(2, 6)].symbol(), "s");
    assert!(status_row(&terminal, 20, 8).contains("test-model"));
}

#[test]
fn prompt_height_caps_at_four_rows_and_keeps_cursor_visible() {
    let snapshot = snapshot_fixture();
    let mut ui = UiState::new();
    ui.editor_mut().set_value("a\nb\nc\nd\ne");
    let terminal = draw(&snapshot, &mut ui, 20, 10);
    let cursor = terminal.get_cursor_position().unwrap();
    assert_eq!(cursor.y, 8);
    assert!(cursor.x < 20);
    assert!(status_row(&terminal, 20, 10).contains("test-model"));
}
~~~

- [ ] **Step 2: Verify the rendering tests fail**

Run: cargo test --bin moh client::ui::view::tests

Expected: FAIL because the prompt has one fixed row.

- [ ] **Step 3: Implement layout and display**

Replace PROMPT_HEIGHT with MAX_PROMPT_HEIGHT: u16 = 4 and add:

~~~rust
fn prompt_height(area: Rect, editor: &PromptEditor) -> u16 {
    let maximum = area.height.saturating_sub(STATUS_HEIGHT).saturating_sub(1).max(1);
    editor.visual_height(area.width.saturating_sub(2))
        .clamp(1, MAX_PROMPT_HEIGHT.min(maximum))
}
~~~

Use it in vertical Layout. Render window rows as Text: first row begins with cyan "❯ "; continuations with unstyled "  ". Set cursor at area.x + 2 + cursor_column, area.y + cursor_row, clamped inside area. Pass expanded prompt area unchanged to popup rendering. Add "Shift+Enter adds a line" to HELP after Enter.

- [ ] **Step 4: Run view tests**

Run: cargo test --bin moh client::ui::view::tests

Expected: PASS with scroll, popup, multiline cells, cap, cursor, and status tests green.

- [ ] **Step 5: Commit Task 2**

~~~bash
git add src/client/ui/view.rs
git commit -m "feat(tui): render multiline prompt rows"
~~~

### Task 3: Event Routing, Documentation, and Verification

**Files:**

- Modify: src/client/app.rs:420-507
- Modify: src/client/app_tests.rs:510-545,1210-1300
- Modify: README.md:37-48

**Interfaces:**

- Consumes: PromptEditor::at_final_line_end().
- Produces: End follows transcript only at final-line end; menus keep Up/Down precedence.

- [ ] **Step 1: Write failing integration tests**

~~~rust
#[tokio::test]
async fn shift_enter_submits_one_multiline_prompt() {
    let client = ScriptedSessionClient::idle();
    let (_, _, _, client) = run_client_with_events(client, [
        Event::Paste("first".into()),
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
        Event::Paste("second".into()), key(KeyCode::Enter), control('c'),
    ]).await.unwrap();
    assert_eq!(client.state.borrow().submissions, ["first\nsecond"]);
}

#[tokio::test]
async fn open_command_menu_consumes_up_and_down_before_the_editor() {
    let (_, ui, _, _) = run_client_with_events(ScriptedSessionClient::idle(), [
        Event::Paste("/".into()), key(KeyCode::Down), key(KeyCode::Up), control('c'),
    ]).await.unwrap();
    assert_eq!(ui.editor().value(), "/");
    assert!(ui.menu().is_open());
}
~~~

Also test handle_event directly: End at end of first in "first\nsecond" moves within that line, while End at end of second follows latest output.

- [ ] **Step 2: Verify integration tests fail**

Run: cargo test --bin moh client::app::tests

Expected: FAIL before Task 1 supplies multiline input.

- [ ] **Step 3: Integrate and document**

Change follow-latest in handle_event from ui.editor().at_end() to ui.editor().at_final_line_end(). Keep handle_menu_event before ui.editor_mut().handle_event(&event), add no global Up/Down branch, and change README prompt copy to "Enter submits; Shift+Enter adds a line."

- [ ] **Step 4: Run app tests and formatting**

Run: cargo test --bin moh client::app::tests && cargo fmt --all -- --check

Expected: PASS with submission, End, selector, and lifecycle tests green.

- [ ] **Step 5: Commit Task 3**

~~~bash
git add src/client/app.rs src/client/app_tests.rs README.md
git commit -m "feat(tui): submit multiline prompts"
~~~

- [ ] **Step 6: Run the full quality gate**

~~~bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --locked
git diff --check
~~~

Expected: all commands exit successfully.

- [ ] **Step 7: PTY acceptance and scope inspection**

In an 80-column-or-wider PTY, compose with Shift+Enter; use Up, Down, Home, End, Backspace, and Delete; exceed four prompt rows; verify status stays at bottom; open "/" and verify Up/Down selects menu entries; resize; exit with Ctrl+C and verify alternate-screen restoration. Do not claim a live model response unless one was sent.

~~~bash
git status --short --branch
git log --oneline origin/main..HEAD
~~~

Expected: only feature commits are ahead of origin/main; unrelated work remains unstaged.
