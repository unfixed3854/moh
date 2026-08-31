# Fullscreen Ratatui Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace Moh's public Pi-like custom renderer with a fullscreen Ratatui terminal client that owns the alternate screen and provides stable keyboard and mouse transcript scrolling.

**Architecture:** Keep the backend SessionSnapshot authoritative in client::app and move only transient presentation state into a private client::ui module. Render complete frames with Ratatui widgets and let Ratatui own layout, buffers, cursor placement, resize handling, and cell diffing; a small private client::terminal module adds input polling, bracketed paste, mouse capture, and reliable cleanup.

**Tech Stack:** Rust 2024, Ratatui 0.30.2 with the Crossterm backend, Crossterm 0.29, pulldown-cmark 0.13, unicode-segmentation 1.13, unicode-width 0.2, Tokio, Ratatui TestBackend.

**Spec:** docs/superpowers/specs/2026-08-27-ratatui-fullscreen-tui-design.md

## Global Constraints

- Use conventional commits.
- Use Ratatui 0.30.2 with Crossterm support; do not add third-party Ratatui widget crates.
- Enter the alternate screen and use the fullscreen viewport; restore the original screen on every normal or recoverable error exit.
- Remove the public moh::tui component and renderer API without a compatibility adapter.
- Keep the backend SessionSnapshot authoritative; UiState may store only prompt, menu, help, scroll, redraw, sanitized local-notice, and presentation-only local-error state.
- Preserve detach semantics: Ctrl+C and /quit detach without cancelling backend work; Escape and /cancel remain explicit cancellation paths.
- Keep prompt editing single-line and grapheme-safe.
- PageUp and PageDown scroll by max(viewport_height - 1, 1); each mouse-wheel step scrolls exactly three rows; only End resumes auto-follow.
- When the prompt cursor is not at its end, End moves that cursor first. When it is already at the end and no popup is open, End resumes transcript auto-follow.
- Mouse support is wheel-only; ignore click, drag, hover, and movement events.
- A normal layout requires at least 20 columns and 3 rows; smaller nonzero frames show terminal too small.
- Preserve terminal-control sanitization at every user, backend, path, menu, notice, and Markdown boundary.
- Keep current command, model, effort, process, help, status, streaming, snapshot-replacement, and client-error behavior unless the approved spec explicitly simplifies it.
- Each implementation task follows red-green-refactor and ends with a focused conventional commit.

---

## File Structure Map

### New private client UI

- src/client/ui/mod.rs — UiState, TranscriptScroll, MenuState, sanitization helpers, and private UI exports.
- src/client/ui/editor.rs — single-line grapheme-safe PromptEditor and EditorOutcome.
- src/client/ui/markdown.rs — pulldown-cmark to owned Ratatui Text conversion.
- src/client/ui/view.rs — transcript projection, layout, widgets, cursor, popups, status, scrollbar, and frame tests.
- src/client/terminal.rs — Crossterm EventSource, Ratatui setup, extra terminal modes, panic cleanup, and lifecycle tests.

### Existing client changes

- src/client/mod.rs — declare the private terminal and ui modules.
- src/client/app.rs — replace AppIds and custom Tui operations with UiState, SessionSnapshot reduction, Ratatui Terminal draws, and Crossterm Event routing.
- src/client/app_tests.rs — replace custom InputEvent and RecordingTerminal fixtures with Crossterm Event and Ratatui TestBackend assertions while preserving product-level behavior tests.

### Package changes

- Cargo.toml and Cargo.lock — add Ratatui 0.30.2; remove vte and vt100 after the old framework is deleted.
- src/lib.rs — remove the public tui module and update the crate-level description.

### Deleted custom framework

- src/tui/app.rs
- src/tui/component.rs
- src/tui/components/container.rs
- src/tui/components/input.rs
- src/tui/components/markdown.rs
- src/tui/components/message.rs
- src/tui/components/mod.rs
- src/tui/components/spacer.rs
- src/tui/components/suggestions.rs
- src/tui/components/surface.rs
- src/tui/components/text.rs
- src/tui/error.rs
- src/tui/input.rs
- src/tui/mod.rs
- src/tui/overlay.rs
- src/tui/renderer.rs
- src/tui/terminal.rs
- src/tui/text.rs

### Deleted or migrated old-framework tests

- tests/components.rs
- tests/overlay.rs
- tests/renderer.rs
- tests/terminal.rs
- tests/text_layout.rs
- tests/tui.rs

Product behavior from those files is ported into src/client/ui module tests and src/client/app_tests.rs before deletion. Tests of custom differential plans, ANSI reopening, cursor-marker invariants, opaque component IDs, main-screen scrollback preservation, and byte-level output are intentionally removed.

---

### Task 1: Add Transient Menu and Transcript Scroll State

**Files:**
- Create: src/client/ui/mod.rs
- Modify: src/client/mod.rs

**Interfaces:**
- Produces: TranscriptScroll::update_metrics(&mut self, content_height: usize, viewport_height: u16).
- Produces: TranscriptScroll::{page_up,page_down,wheel_up,wheel_down,follow_latest,top,auto_follow}.
- Produces: MenuKind::{Commands,Models,Efforts,Processes}.
- Produces: MenuItem::new(value: impl Into<String>, description: impl Into<String>).
- Produces: MenuState::{set,clear,select_next,select_previous,selected_value,is_open}.
- Later tasks embed these values in UiState and render them; no public library API is created.

- [ ] **Step 1: Declare the private UI module and write failing scroll tests**

Add this declaration to src/client/mod.rs:

~~~rust
mod ui;
~~~

Create src/client/ui/mod.rs with tests that refer to the not-yet-defined state:

~~~rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_follow_tracks_bottom_and_manual_scroll_stays_stable() {
        let mut scroll = TranscriptScroll::default();
        scroll.update_metrics(30, 8);
        assert_eq!(scroll.top(), 22);
        assert!(scroll.auto_follow());

        scroll.page_up();
        assert_eq!(scroll.top(), 15);
        assert!(!scroll.auto_follow());

        scroll.update_metrics(40, 8);
        assert_eq!(scroll.top(), 15);

        scroll.follow_latest();
        assert_eq!(scroll.top(), 32);
        assert!(scroll.auto_follow());
    }

    #[test]
    fn page_and_wheel_steps_are_exact_and_clamped() {
        let mut scroll = TranscriptScroll::default();
        scroll.update_metrics(20, 5);
        scroll.page_up();
        assert_eq!(scroll.top(), 11);
        scroll.wheel_up();
        assert_eq!(scroll.top(), 8);
        scroll.wheel_down();
        assert_eq!(scroll.top(), 11);
        scroll.page_down();
        assert_eq!(scroll.top(), 15);
        scroll.page_down();
        assert_eq!(scroll.top(), 15);
    }

    #[test]
    fn menu_selection_wraps_and_replacement_selects_first_item() {
        let mut menu = MenuState::default();
        menu.set(
            MenuKind::Commands,
            [
                MenuItem::new("/quit", "Exit moh"),
                MenuItem::new("/model", "Change model"),
            ],
        );
        assert_eq!(menu.selected_value(), Some("/quit"));
        menu.select_previous();
        assert_eq!(menu.selected_value(), Some("/model"));
        menu.select_next();
        assert_eq!(menu.selected_value(), Some("/quit"));

        menu.set(
            MenuKind::Models,
            [MenuItem::new("gpt-5.6-terra", "Balanced model")],
        );
        assert_eq!(menu.selected_value(), Some("gpt-5.6-terra"));
    }
}
~~~

- [ ] **Step 2: Run the focused tests and verify red**

Run:

~~~bash
cargo test --bin moh client::ui::tests
~~~

Expected: compilation fails because TranscriptScroll, MenuState, MenuKind, and MenuItem do not exist.

- [ ] **Step 3: Implement the minimal state types**

Add these state shapes and the exact constants to src/client/ui/mod.rs:

~~~rust
const MOUSE_SCROLL_ROWS: usize = 3;

#[derive(Debug)]
pub(super) struct TranscriptScroll {
    top: usize,
    content_height: usize,
    viewport_height: u16,
    auto_follow: bool,
}

impl Default for TranscriptScroll {
    fn default() -> Self {
        Self {
            top: 0,
            content_height: 0,
            viewport_height: 0,
            auto_follow: true,
        }
    }
}

impl TranscriptScroll {
    fn max_top(&self) -> usize {
        self.content_height
            .saturating_sub(usize::from(self.viewport_height))
    }

    pub(super) fn update_metrics(&mut self, content_height: usize, viewport_height: u16) {
        self.content_height = content_height;
        self.viewport_height = viewport_height;
        self.top = if self.auto_follow {
            self.max_top()
        } else {
            self.top.min(self.max_top())
        };
    }

    pub(super) fn page_up(&mut self) {
        self.auto_follow = false;
        let step = usize::from(self.viewport_height.saturating_sub(1).max(1));
        self.top = self.top.saturating_sub(step);
    }

    pub(super) fn page_down(&mut self) {
        self.auto_follow = false;
        let step = usize::from(self.viewport_height.saturating_sub(1).max(1));
        self.top = self.top.saturating_add(step).min(self.max_top());
    }

    pub(super) fn wheel_up(&mut self) {
        self.auto_follow = false;
        self.top = self.top.saturating_sub(MOUSE_SCROLL_ROWS);
    }

    pub(super) fn wheel_down(&mut self) {
        self.auto_follow = false;
        self.top = self.top.saturating_add(MOUSE_SCROLL_ROWS).min(self.max_top());
    }

    pub(super) fn follow_latest(&mut self) {
        self.auto_follow = true;
        self.top = self.max_top();
    }

    pub(super) const fn top(&self) -> usize {
        self.top
    }

    pub(super) const fn auto_follow(&self) -> bool {
        self.auto_follow
    }
}
~~~

Define MenuKind, MenuItem, and MenuState as private-client types. MenuState::set must sanitize neither field yet; Task 2 centralizes line sanitization before values enter state. set replaces all items and selects index zero, clear removes the kind and items, empty navigation is a no-op, and next/previous wrap exactly like the old Suggestions component.

Use these shapes:

~~~rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MenuKind {
    Commands,
    Models,
    Efforts,
    Processes,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct MenuItem {
    value: String,
    description: String,
}

#[derive(Default)]
pub(super) struct MenuState {
    kind: Option<MenuKind>,
    items: Vec<MenuItem>,
    selected: usize,
}
~~~

MenuItem::new owns its two inputs. MenuState::set collects the supplied iterator, records the kind, and resets selected to zero. selected_value returns self.items.get(self.selected).map(|item| item.value.as_str()). is_open returns kind.is_some() && !items.is_empty().

- [ ] **Step 4: Run focused tests**

Run:

~~~bash
cargo test --bin moh client::ui::tests
~~~

Expected: all three tests pass.

- [ ] **Step 5: Commit**

~~~bash
git add src/client/mod.rs src/client/ui/mod.rs
git commit -m "feat(tui): add fullscreen ui state"
~~~

---

### Task 2: Port the Grapheme-Safe Prompt Editor

**Files:**
- Create: src/client/ui/editor.rs
- Modify: src/client/ui/mod.rs
- Source behavior to port: src/tui/components/input.rs
- Source tests to port: tests/components.rs:386-954

**Interfaces:**
- Produces: PromptEditor::{new,value,set_value,clear,at_end,handle_event,display_window}.
- Produces: EditorOutcome::{Ignored,Consumed,Changed,Submitted(String)}.
- Produces: EditorWindow { text: String, cursor_column: u16 }.
- Produces: sanitize_line(&str) -> String in client::ui.
- Produces: UiState with editor, scroll, menu, help, local notices, local-error override, and redraw scheduling.

- [ ] **Step 1: Write failing editor and UiState tests**

Create src/client/ui/editor.rs with the test module first. Use real Crossterm events:

~~~rust
#[cfg(test)]
mod tests {
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers,
    };
    use super::*;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn editor_edits_and_submits_whole_graphemes() {
        let mut editor = PromptEditor::new();
        assert_eq!(
            editor.handle_event(&Event::Paste("a👩‍💻b".into())),
            EditorOutcome::Changed
        );
        assert_eq!(editor.handle_event(&key(KeyCode::Left)), EditorOutcome::Changed);
        assert_eq!(
            editor.handle_event(&key(KeyCode::Backspace)),
            EditorOutcome::Changed
        );
        assert_eq!(editor.value(), "ab");
        assert_eq!(
            editor.handle_event(&key(KeyCode::Enter)),
            EditorOutcome::Submitted("ab".into())
        );
        assert_eq!(editor.value(), "");
    }

    #[test]
    fn editor_preserves_word_shortcuts_and_normalizes_paste() {
        let mut editor = PromptEditor::new();
        editor.handle_event(&Event::Paste("one  two\nthree\x1b[2J".into()));
        let ctrl_left = Event::Key(KeyEvent::new(
            KeyCode::Left,
            KeyModifiers::CONTROL,
        ));
        let ctrl_backspace = Event::Key(KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::CONTROL,
        ));
        assert_eq!(editor.handle_event(&ctrl_left), EditorOutcome::Changed);
        assert_eq!(editor.handle_event(&ctrl_backspace), EditorOutcome::Changed);
        assert_eq!(editor.value(), "one  three[2J");
    }

    #[test]
    fn display_window_keeps_the_hardware_cursor_inside_the_available_width() {
        let mut editor = PromptEditor::new();
        editor.set_value("0123456789");
        let window = editor.display_window(4);
        assert!(unicode_width::UnicodeWidthStr::width(window.text.as_str()) <= 4);
        assert!(window.cursor_column < 4);
        assert!(window.text.ends_with("789"));
    }
}
~~~

Add UiState tests to src/client/ui/mod.rs:

~~~rust
#[test]
fn authoritative_reset_clears_only_local_projection_state() {
    let mut ui = UiState::new();
    ui.editor_mut().set_value("keep me");
    ui.push_notice("safe\x1b[2J error");
    ui.set_help_open(true);
    ui.authoritative_reset();

    assert_eq!(ui.editor().value(), "keep me");
    assert!(ui.notices().is_empty());
    assert!(!ui.local_error());
    assert!(!ui.help_open());
    assert!(ui.take_redraw());
}
~~~

- [ ] **Step 2: Run focused tests and verify red**

Run:

~~~bash
cargo test --bin moh client::ui::editor::tests
cargo test --bin moh client::ui::tests::authoritative_reset_clears_only_local_projection_state
~~~

Expected: compilation fails because PromptEditor, EditorOutcome, EditorWindow, and UiState do not exist.

- [ ] **Step 3: Port the editor as plain state**

Move the grapheme-span, byte-index, cursor-column, insertion, deletion, word-movement, and word-deletion algorithms from src/tui/components/input.rs into PromptEditor. Remove Component, InputEvent, focus, ANSI, cursor marker, and render_open_surface concerns.

Use this public-to-client interface:

~~~rust
#[derive(Debug, Eq, PartialEq)]
pub(super) enum EditorOutcome {
    Ignored,
    Consumed,
    Changed,
    Submitted(String),
}

pub(super) struct EditorWindow {
    pub(super) text: String,
    pub(super) cursor_column: u16,
}

#[derive(Default)]
pub(super) struct PromptEditor {
    value: String,
    cursor_grapheme: usize,
    scroll_column: usize,
}
~~~

PromptEditor::handle_event accepts Event::Paste and key press/repeat events. It ignores key releases, resize, focus, and mouse events. Preserve the old key mapping, including Ctrl+H as Ctrl+Backspace on Unix terminals, and return Consumed at editing boundaries. display_window(width) uses unicode-width cell counts, keeps the cursor column strictly below width when width is nonzero, and returns an empty zero-column window when width is zero.

Add sanitize_line to src/client/ui/mod.rs. It must remove the old cursor marker string, turn each run of newline, carriage return, or tab into one space, drop every other C0/C1 control including Escape, and preserve printable Unicode. PromptEditor::set_value, paste, and inserted characters all pass through this function.

Change MenuItem::new to pass both value and description through sanitize_line before storing them. This makes every command, model, effort, process, and backend-supplied description safe before MenuState owns it.

- [ ] **Step 4: Add UiState**

Define UiState in src/client/ui/mod.rs:

~~~rust
pub(super) struct UiState {
    editor: PromptEditor,
    scroll: TranscriptScroll,
    menu: MenuState,
    help_open: bool,
    notices: Vec<String>,
    local_error: bool,
    needs_redraw: bool,
}

impl UiState {
    pub(super) fn new() -> Self {
        Self {
            editor: PromptEditor::new(),
            scroll: TranscriptScroll::default(),
            menu: MenuState::default(),
            help_open: false,
            notices: Vec::new(),
            local_error: false,
            needs_redraw: true,
        }
    }

    pub(super) fn push_notice(&mut self, notice: impl AsRef<str>) {
        self.notices.push(sanitize_line(notice.as_ref()));
        self.local_error = true;
        self.request_redraw();
    }

    pub(super) fn clear_local_error(&mut self) {
        self.local_error = false;
        self.request_redraw();
    }

    pub(super) fn authoritative_reset(&mut self) {
        self.notices.clear();
        self.local_error = false;
        self.menu.clear();
        self.help_open = false;
        self.request_redraw();
    }

    pub(super) fn request_redraw(&mut self) {
        self.needs_redraw = true;
    }

    pub(super) fn take_redraw(&mut self) -> bool {
        std::mem::take(&mut self.needs_redraw)
    }
}
~~~

Add narrow accessors for editor, scroll, menu, help, notices, and local_error; do not make fields public. set_help_open, editor_mut, scroll_mut, and menu_mut request redraw after mutation at their call sites rather than hiding mutation in Deref wrappers.

- [ ] **Step 5: Port and run the full editor behavior suite**

Move the product-level cases from tests/components.rs covering graphemes, combining sequences, word boundaries, shifted modifier rejection, paste, control sanitization, Home/End, narrow horizontal windows, and empty submission into src/client/ui/editor.rs.

Run:

~~~bash
cargo test --bin moh client::ui
~~~

Expected: every UI state and editor test passes.

- [ ] **Step 6: Commit**

~~~bash
git add src/client/ui/mod.rs src/client/ui/editor.rs
git commit -m "feat(tui): port prompt editor state"
~~~

---

### Task 3: Convert Markdown to Ratatui Styled Text

**Files:**
- Modify: Cargo.toml
- Modify: Cargo.lock
- Create: src/client/ui/markdown.rs
- Modify: src/client/ui/mod.rs
- Source behavior to port: src/tui/components/markdown.rs
- Source tests to port: tests/components.rs:118-249 and tests/text_layout.rs:6-138

**Interfaces:**
- Produces: markdown_text(source: &str) -> ratatui::text::Text<'static>.
- Produces: sanitize_markdown_source(source: &str) -> String.
- Produces: supported semantic styles without raw ANSI or OSC-8 output.

- [ ] **Step 1: Add Ratatui and write failing semantic tests**

Add the dependency without changing unrelated versions:

~~~toml
ratatui = "0.30.2"
~~~

Create src/client/ui/markdown.rs with tests:

~~~rust
#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier};
    use super::*;

    #[test]
    fn markdown_maps_inline_and_block_styles_to_spans() {
        let source = format!(
            "# Heading\n\n**bold** and {}code{}",
            char::from(96),
            char::from(96),
        );
        let text = markdown_text(&source);
        assert_eq!(text.lines.len(), 3);
        assert!(text.lines[0].spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert!(text.lines[2]
            .spans
            .iter()
            .any(|span| span.content == "bold"
                && span.style.add_modifier.contains(Modifier::BOLD)));
        assert!(text.lines[2]
            .spans
            .iter()
            .any(|span| span.content == "code" && span.style.fg == Some(Color::Yellow)));
    }

    #[test]
    fn links_keep_a_visible_destination_without_terminal_escapes() {
        let text = markdown_text("[Ratatui](https://ratatui.rs)");
        let rendered = text
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(rendered, "Ratatui (https://ratatui.rs)");
        assert!(!rendered.contains('\u{1b}'));
    }

    #[test]
    fn markdown_sanitization_preserves_lines_and_removes_terminal_controls() {
        assert_eq!(
            sanitize_markdown_source("# title\r\n\nbody\t\x1b[31mred\x1b[0m"),
            "# title\n\nbody    red"
        );
    }
}
~~~

- [ ] **Step 2: Run tests and verify red**

Run:

~~~bash
cargo test --bin moh client::ui::markdown::tests
~~~

Expected: compilation fails because markdown_text and sanitize_markdown_source do not exist.

- [ ] **Step 3: Implement the styled Markdown builder**

Reuse the pulldown-cmark option set and block rules from the old renderer, but emit owned Ratatui values:

~~~rust
pub(super) fn markdown_text(source: &str) -> Text<'static> {
    let source = sanitize_markdown_source(source);
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    MarkdownBuilder::new().collect(Parser::new_ext(&source, options))
}
~~~

Define MarkdownBuilder with:

~~~rust
struct MarkdownBuilder {
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    style: Style,
    style_stack: Vec<Style>,
    lists: Vec<Option<u64>>,
    quote_depth: usize,
    in_code_block: bool,
    links: Vec<LinkContext>,
}

struct LinkContext {
    destination: String,
    label: String,
}
~~~

For every pulldown-cmark event, append raw printable content as Span::styled with the current Style. Push the prior style before adding bold, italic, crossed-out, underline, dim, or yellow-code attributes and restore it on the matching end event. Preserve blank paragraph separation, list indentation and numbering, task markers, quote rails, fenced-language labels, code-block newlines, hard breaks, soft spaces, headings, and rules. At a link end, append a dim space-and-parenthesized destination only when the collected visible label differs from the destination.

sanitize_markdown_source first removes the exact old cursor-marker sequence, then strips complete ECMA-48 CSI, OSC, and APC escape sequences with a small sanitation-only state machine. CSI ends at its 0x40-0x7e final byte; OSC and APC end at BEL or String Terminator. It then normalizes CRLF and CR to LF, expands tabs to four spaces, and removes every other control except LF. This scanner never measures width, wraps text, tracks style, or emits terminal bytes, so it does not recreate the old ANSI rendering layer.

- [ ] **Step 4: Port Markdown product tests and run**

Move the common inline, block, list, quote, code, incomplete-stream, Unicode, and sanitization cases from the two old test files into markdown.rs. Replace ANSI-string assertions with Text line content and Style assertions.

Run:

~~~bash
cargo test --bin moh client::ui::markdown
~~~

Expected: all Markdown tests pass.

- [ ] **Step 5: Commit**

~~~bash
git add Cargo.toml Cargo.lock src/client/ui/mod.rs src/client/ui/markdown.rs
git commit -m "feat(tui): render markdown as ratatui text"
~~~

---

### Task 4: Render the Fullscreen Frame with Ratatui Widgets

**Files:**
- Create: src/client/ui/view.rs
- Modify: src/client/ui/mod.rs

**Interfaces:**
- Produces: render(frame: &mut ratatui::Frame<'_>, snapshot: &SessionSnapshot, ui: &mut UiState).
- Produces: status_line(snapshot: &SessionSnapshot, ui: &UiState) -> Line<'static>.
- Produces: transcript_entries(snapshot: &SessionSnapshot, ui: &UiState) -> Vec<TranscriptEntry>.
- Consumes: PromptEditor::display_window, TranscriptScroll, MenuState, markdown_text, sanitize_line.

- [ ] **Step 1: Write failing TestBackend layout tests**

Create view.rs with a test helper and the first frame tests:

~~~rust
#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use moh::{
        runtime::rig::ReasoningLevel,
        session::{
            ModelCatalogState, SessionSettings, SessionSnapshot, SessionSummary,
            TranscriptItem,
        },
    };
    use ratatui::{Terminal, backend::TestBackend};
    use super::*;

    fn snapshot_fixture() -> SessionSnapshot {
        let now = Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap();
        SessionSnapshot {
            summary: SessionSummary {
                id: "session-7".parse().unwrap(),
                name: None,
                cwd: b"/work/moh".to_vec(),
                cwd_display: "/work/moh".into(),
                is_default: true,
                busy: false,
                attached_clients: 1,
                last_activity: now,
            },
            transcript: vec![
                TranscriptItem::User("first prompt".into()),
                TranscriptItem::Assistant("first answer".into()),
            ],
            active_run: None,
            settings: SessionSettings {
                model: "test-model".into(),
                reasoning: ReasoningLevel::High,
                context_tokens: 128_000,
            },
            catalog: ModelCatalogState::Ready(Vec::new()),
            jobs: Vec::new(),
            persistence_warning: None,
            sequence: 14,
            busy: false,
        }
    }

    fn draw(
        snapshot: &SessionSnapshot,
        ui: &mut UiState,
        width: u16,
        height: u16,
    ) -> Terminal<TestBackend> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, snapshot, ui)).unwrap();
        terminal
    }

    #[test]
    fn frame_pins_prompt_above_status() {
        let snapshot = snapshot_fixture();
        let mut ui = UiState::new();
        let terminal = draw(&snapshot, &mut ui, 80, 10);
        let lines = terminal.backend().buffer().content
            .chunks(80)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>();
        assert!(lines[8].contains('❯'));
        assert!(lines[9].contains("test-model"));
        assert!(lines[9].contains("50%/256K"));
        assert!(lines[9].contains("/work/moh"));
    }

    #[test]
    fn undersized_frame_shows_only_the_minimum_size_message() {
        let snapshot = snapshot_fixture();
        let mut ui = UiState::new();
        let terminal = draw(&snapshot, &mut ui, 19, 2);
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("terminal too small"));
        assert!(!rendered.contains('❯'));
    }
}
~~~

- [ ] **Step 2: Run the layout tests and verify red**

Run:

~~~bash
cargo test --bin moh client::ui::view::tests::frame_pins_prompt_above_status
cargo test --bin moh client::ui::view::tests::undersized_frame_shows_only_the_minimum_size_message
~~~

Expected: compilation fails because render does not exist.

- [ ] **Step 3: Implement root layout, prompt, status, and minimum frame**

Use these exact layout constants:

~~~rust
const MIN_WIDTH: u16 = 20;
const MIN_HEIGHT: u16 = 3;
const PROMPT_HEIGHT: u16 = 1;
const STATUS_HEIGHT: u16 = 1;
const CONTEXT_WINDOW_TOKENS: u64 = 256_000;
~~~

Split normal frames with:

~~~rust
let [transcript_area, prompt_area, status_area] = Layout::vertical([
    Constraint::Min(1),
    Constraint::Length(PROMPT_HEIGHT),
    Constraint::Length(STATUS_HEIGHT),
])
.areas(frame.area());
~~~

Render the prompt as a cyan ❯ plus PromptEditor::display_window output. Set the hardware cursor only for the normal frame and place it within prompt_area. Render status as styled spans, with the same model, effort, context percentage, ready/thinking/error, optional process count, and sanitized CWD semantics as the old status_line. Error takes precedence when ui.local_error is true, the catalog failed, a persistence warning exists, or the last transcript item is Failed. Otherwise busy is thinking and idle is ready. A later Started item makes thinking current, while a later Assistant or Cancelled terminal item makes the idle status ready.

- [ ] **Step 4: Write failing transcript, scrollbar, cursor, and popup tests**

Add TestBackend cases that assert:

- user text has a cyan/dim left rail while assistant text remains open;
- active_run.assistant_text is visible exactly once;
- tool-started read activity is relative to snapshot.summary.cwd;
- a long transcript produces a visible right-side scrollbar;
- manual scroll changes visible top content and new active text does not change ui.scroll().top();
- long prompt text keeps TestBackend::cursor_position inside the prompt row;
- help uses Clear plus a centered bordered Paragraph;
- command/model/effort/process menus use a List above the prompt, show no more than five items, and highlight MenuState's selected row;
- popup rectangles remain within a 20x3 normal frame.

Run the new tests once. Expected: transcript and popup assertions fail because only the root layout exists.

- [ ] **Step 5: Implement transcript entries and viewport rendering**

Define:

~~~rust
enum TranscriptKind {
    Plain,
    User,
    Assistant,
    Activity,
}

struct TranscriptEntry {
    kind: TranscriptKind,
    text: Text<'static>,
    trailing_space: bool,
}
~~~

Build entries in this order:

1. introduction;
2. catalog and persistence messages derived from the snapshot;
3. snapshot.transcript items;
4. active_run.assistant_text when nonempty;
5. UiState local notices.

Cancelled transcript records render no message. Failed records render only failure.message. ToolStarted read calls render the existing relative path and line-range summary; unknown tools render dim Running NAME without arguments.

Measure each entry with Paragraph::line_count at its actual content width. User entries use a left-only Block border; all other entries use borderless Paragraphs. Treat the approved blank row after user, assistant, failure, and local-notice messages as part of that entry's measured height. Render only the intersection between each virtual entry range and the visible transcript range, using Paragraph::scroll for a clipped leading portion. Update TranscriptScroll metrics before choosing the visible range.

Reserve one right column and render Scrollbar with ScrollbarState only when content height exceeds viewport height. Do not render a scrollbar into a one-column transcript area.

- [ ] **Step 6: Implement popups with Ratatui widgets**

Render command/model/effort/process items with List and ListState. Use Clear before each popup. Menus occupy min(item_count, 5) content rows plus borders, fit above prompt_area, and clamp width to frame.area(). Help uses 60 percent of available width, at most 12 content rows plus borders, one-cell margins when available, and a centered Rect. On tiny normal frames, clamping may leave only the border or one clipped text row but must never underflow.

Add PageUp/PageDown or mouse wheel scroll and End follows latest output to the help copy so every new navigation path is discoverable.

- [ ] **Step 7: Run all frame and UI tests**

Run:

~~~bash
cargo test --bin moh client::ui
~~~

Expected: all state, editor, Markdown, layout, transcript, cursor, scrollbar, and popup tests pass.

- [ ] **Step 8: Commit**

~~~bash
git add src/client/ui/mod.rs src/client/ui/view.rs src/client/app_tests.rs
git commit -m "feat(tui): render fullscreen ratatui frames"
~~~

---

### Task 5: Add Ratatui Terminal Lifecycle and Crossterm Events

**Files:**
- Create: src/client/terminal.rs
- Modify: src/client/mod.rs
- Source lifecycle behavior to port: src/tui/terminal.rs
- Source tests to port: tests/terminal.rs

**Interfaces:**
- Produces: EventSource::poll_event(&mut self, timeout: Duration) -> io::Result<Option<crossterm::event::Event>>.
- Produces: CrosstermEvents.
- Produces: TerminalSession::start() -> io::Result<(ratatui::DefaultTerminal, TerminalSession<ProductionModes>)>.
- Produces: TerminalSession::start_with(ops) and TerminalSession::restore for lifecycle tests.

- [ ] **Step 1: Write failing event-source and lifecycle tests**

Declare mod terminal in src/client/mod.rs. In src/client/terminal.rs, define tests around a FakeModes operation log:

~~~rust
#[test]
fn successful_session_restores_mouse_paste_then_ratatui() {
    let effects = Rc::new(RefCell::new(Vec::new()));
    let modes = FakeModes::new(Rc::clone(&effects));
    let (_, mut session) = TerminalSession::start_with(modes).unwrap();
    session.restore().unwrap();
    assert_eq!(
        effects.borrow().as_slice(),
        ["init", "enable_paste", "enable_mouse",
         "disable_mouse", "disable_paste", "restore"]
    );
}

#[test]
fn mouse_setup_failure_unwinds_paste_and_ratatui() {
    let effects = Rc::new(RefCell::new(Vec::new()));
    let modes = FakeModes::failing(Rc::clone(&effects), "enable_mouse");
    let error = TerminalSession::start_with(modes).unwrap_err();
    assert_eq!(error.to_string(), "enable_mouse failed");
    assert_eq!(
        effects.borrow().as_slice(),
        ["init", "enable_paste", "enable_mouse", "disable_paste", "restore"]
    );
}

#[test]
fn restore_attempts_every_inverse_and_returns_the_first_error() {
    let effects = Rc::new(RefCell::new(Vec::new()));
    let modes = FakeModes::failing_many(
        Rc::clone(&effects),
        ["disable_mouse", "disable_paste"],
    );
    let (_, mut session) = TerminalSession::start_with(modes).unwrap();
    let error = session.restore().unwrap_err();
    assert_eq!(error.to_string(), "disable_mouse failed");
    assert!(effects.borrow().ends_with(
        &["disable_mouse", "disable_paste", "restore"]
    ));
}
~~~

- [ ] **Step 2: Run tests and verify red**

Run:

~~~bash
cargo test --bin moh client::terminal::tests
~~~

Expected: compilation fails because TerminalSession and the ModeOps seam do not exist.

- [ ] **Step 3: Implement EventSource and production polling**

Define:

~~~rust
pub(super) trait EventSource {
    fn poll_event(&mut self, timeout: Duration) -> io::Result<Option<Event>>;
}

pub(super) struct CrosstermEvents;

impl EventSource for CrosstermEvents {
    fn poll_event(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        if crossterm::event::poll(timeout)? {
            Ok(Some(crossterm::event::read()?))
        } else {
            Ok(None)
        }
    }
}
~~~

Do not normalize events into a second key model. client::app and PromptEditor consume Crossterm Event directly. Key release filtering belongs in the reducer/editor. Mouse events pass through unchanged for wheel handling.

- [ ] **Step 4: Implement tracked mode setup and teardown**

Define a private ModeOps trait with associated Terminal and these methods in order: init, enable_paste, enable_mouse, disable_mouse, disable_paste, restore. ProductionModes uses ratatui::try_init, Crossterm EnableBracketedPaste, EnableMouseCapture, DisableMouseCapture, DisableBracketedPaste, and ratatui::try_restore.

TerminalSession tracks ratatui_started, paste_enabled, mouse_enabled, and restored booleans. start_with rolls back every completed acquisition when a later one fails. restore attempts all still-pending inverses in the required order, retains the first error, remains idempotent after success, and leaves failed flags pending so Drop can retry them.

Install a chained panic hook after production setup:

~~~rust
fn install_extra_mode_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::DisableMouseCapture,
            crossterm::event::DisableBracketedPaste,
        );
        previous(info);
    }));
}
~~~

Ratatui's hook remains the delegated hook and restores raw mode and the alternate screen. Drop performs best-effort restore without panicking.

- [ ] **Step 5: Port the lifecycle failure matrix and run**

Port the relevant tests from tests/terminal.rs for every setup failure, cleanup failure, retry, idempotence, and Drop path. Remove old tests for custom key conversion, custom Terminal writes, NotATerminal, and zero-size validation; Ratatui/Crossterm now own those behaviors.

Run:

~~~bash
cargo test --bin moh client::terminal
~~~

Expected: all event-source and lifecycle tests pass.

- [ ] **Step 6: Commit**

~~~bash
git add src/client/mod.rs src/client/terminal.rs
git commit -m "feat(tui): manage ratatui terminal sessions"
~~~

---

### Task 6: Drive the Client Through Ratatui and UiState

**Files:**
- Modify: src/client/app.rs
- Modify: src/client/app_tests.rs
- Modify: src/client/ui/mod.rs
- Modify: src/client/ui/view.rs

**Interfaces:**
- Consumes: client::terminal::{EventSource,CrosstermEvents,TerminalSession}.
- Consumes: client::ui::{UiState,EditorOutcome,MenuKind,MenuItem,render,sanitize_line}.
- Preserves: SessionClient and every RPC method in src/client/session.rs unchanged.
- Produces: run_event_loop<B, E, C> generic over Ratatui Backend, EventSource, and SessionClient.
- Produces: AppError conversions for io::Error and Infallible plus combined application/cleanup context.

- [ ] **Step 1: Convert the test fixtures to Crossterm and Ratatui**

In src/client/app_tests.rs:

- replace InputEvent, Key, and Modifiers helpers with Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind;
- replace ScriptedEvents storage with VecDeque<io::Result<Event>>;
- replace RecordingTerminal with Terminal<TestBackend>;
- make run_client_with_events return (Terminal<TestBackend>, UiState, SessionSnapshot, ScriptedSessionClient);
- inspect terminal.backend().buffer(), terminal.backend().cursor_position(), UiState, and SessionSnapshot instead of component downcasts or emitted ANSI bytes.

Use these helpers:

~~~rust
fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn control(character: char) -> Event {
    Event::Key(KeyEvent::new(
        KeyCode::Char(character),
        KeyModifiers::CONTROL,
    ))
}

fn wheel(kind: MouseEventKind) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    })
}
~~~

First port and run these tests as red tests: control_c_exits, control_o_opens_help, submitted_input_carries_its_text, snapshot_reconstructs_transcript_live_response_settings_context_and_jobs, matching_command_suggestions_render_above_the_prompt, backend_events_stream_markdown_and_sanitize_terminal_controls, process_commands_use_session_rpc, terminal_event_error_detaches_without_cancelling_and_preserves_error, and application_error_wins_while_restore_is_attempted.

Expected: compilation fails because app.rs still requires the removed custom event/terminal fixture shape.

- [ ] **Step 2: Replace AppIds with UiState and snapshot-derived helpers**

Delete AppIds, status component mutation, build_from_snapshot, populate_transcript, append_transcript_item, live-response component updates, overlay options, and component lookup calls.

Refactor these helpers to take authoritative data:

~~~rust
fn available_models(snapshot: &SessionSnapshot) -> &[ModelInfoDto];
fn available_efforts(snapshot: &SessionSnapshot) -> Vec<ReasoningLevel>;
fn running_jobs(snapshot: &SessionSnapshot) -> Vec<&JobSnapshotDto>;
fn resolve_submission(
    ui: &UiState,
    snapshot: &SessionSnapshot,
    text: String,
) -> AppAction;
fn refresh_menu(ui: &mut UiState, snapshot: &SessionSnapshot);
~~~

Local model-not-found, effort-not-found, job, and ClientSessionError messages call ui.push_notice. An authoritative SnapshotReplaced calls ui.authoritative_reset before refreshing the menu. Started, Completed, Failed, Cancelled, SettingsChanged, CatalogChanged, and PersistenceWarning clear the presentation-only error override at the same points where the old status component moved away from a local error.

- [ ] **Step 3: Reduce backend events into the snapshot only**

Keep validate_snapshot, sequence validation, and run-ID validation. apply_session_event mutates SessionSnapshot exactly as the current function already does, but removes every Tui/component side effect. After a valid event it updates projection.sequence, refreshes menus when catalog/jobs/settings affect them, updates local-error state, and requests a redraw.

Use this signature:

~~~rust
fn apply_session_update(
    ui: &mut UiState,
    projection: &mut SessionSnapshot,
    update: SessionUpdate,
) -> Result<(), AppError>;
~~~

JobsChanged updates projection.jobs. ToolStarted remains a TranscriptItem. AssistantDelta appends to active_run.assistant_text. Completed commits one assistant item and clears active_run. Failed commits only the sanitized failure snapshot and clears partial assistant text by clearing active_run.

- [ ] **Step 4: Route Crossterm events with the approved precedence**

Implement one reducer path with this precedence:

1. Key release events are ignored.
2. Ctrl+C exits; Ctrl+O opens help; Escape closes help.
3. PageUp, PageDown, wheel up, and wheel down mutate TranscriptScroll and request redraw.
4. With no popup, End at an editor already at end calls follow_latest; otherwise PromptEditor handles End.
5. Escape closes an open selector, else cancels an active request.
6. Ctrl+L, Ctrl+R, and Shift+Tab retain their busy/help/process guards.
7. An open MenuState handles Up, Down, Tab, and Enter.
8. PromptEditor handles paste and editing.
9. EditorOutcome::Submitted resolves slash commands or submits to the session.
10. Resize requests redraw; focus, click, drag, move, and other mouse events do nothing.

Add focused reducer tests for End's three states, three-row wheel steps, PageUp/PageDown, ignored mouse clicks, and release-event filtering.

- [ ] **Step 5: Replace the event loop and production run**

Use generic Ratatui drawing:

~~~rust
async fn run_event_loop<B, E, C>(
    terminal: &mut ratatui::Terminal<B>,
    ui: &mut UiState,
    events: &mut E,
    client: &mut C,
    projection: &mut SessionSnapshot,
) -> Result<(), AppError>
where
    B: ratatui::backend::Backend,
    B::Error: Into<AppError>,
    E: EventSource,
    C: SessionClient,
{
    let mut running = true;
    while running {
        draw_if_needed(terminal, ui, projection)?;
        let timeout = if projection.busy {
            Duration::ZERO
        } else {
            Duration::from_millis(16)
        };
        if let Some(event) = events.poll_event(timeout)? {
            running = handle_event(ui, client, projection, event).await?;
        }
        draw_if_needed(terminal, ui, projection)?;

        let update = tokio::select! {
            biased;
            update = client.next_update() => Some(update?),
            () = tokio::time::sleep(Duration::from_millis(16)) => None,
        };
        if let Some(update) = update {
            apply_session_update(ui, projection, update)?;
        }
    }
    draw_if_needed(terminal, ui, projection)?;
    Ok(())
}
~~~

Define handle_event with the precedence from Step 4 and this signature:

~~~rust
async fn handle_event<C: SessionClient>(
    ui: &mut UiState,
    client: &C,
    projection: &mut SessionSnapshot,
    event: Event,
) -> Result<bool, AppError>;
~~~

Define draw_if_needed with the same Backend bounds as run_event_loop; it returns without drawing when take_redraw is false and otherwise performs exactly one Terminal::draw call.

Implement From<io::Error> and From<Infallible> for AppError so production Crossterm and TestBackend share the loop. Each visible mutation calls ui.request_redraw. A draw occurs only when ui.take_redraw returns true:

~~~rust
terminal
    .draw(|frame| crate::client::ui::render(frame, projection, ui))
    .map_err(Into::into)?;
~~~

Production run starts TerminalSession, runs the application with DefaultTerminal and CrosstermEvents, then explicitly restores. Return application errors after restoration. When application and restoration both fail, return an AppError variant whose display starts with the application error and appends terminal cleanup also failed: CLEANUP. Drop remains only the retry fallback.

- [ ] **Step 6: Port the complete application behavior suite**

Port every product-level test in src/client/app_tests.rs. Replace these renderer-specific assertions:

- final newline/cursor-below-main-screen becomes alternate-screen cleanup coverage in terminal tests;
- absence of clear-screen or alternate-screen bytes becomes cell sanitization and original-screen restoration coverage;
- custom finish write counts become explicit TerminalSession restore operation counts;
- ANSI boundary assertions become Ratatui Style assertions in view tests.

Keep tests for command routing, model and effort selection, fuzzy matching, jobs, status contents, snapshot replacement, streaming, errors, cancellation, detach, observer failure, catalog/persistence status, terminal safety, and busy responsiveness.

Run:

~~~bash
cargo test --bin moh
cargo test --test client_server
~~~

Expected: all binary client and client-server tests pass through the Ratatui path.

- [ ] **Step 7: Commit**

~~~bash
git add src/client/app.rs src/client/app_tests.rs src/client/ui/mod.rs src/client/ui/view.rs
git commit -m "refactor(tui): drive client with ratatui"
~~~

---

### Task 7: Remove the Custom Renderer and Verify Fullscreen Acceptance

**Files:**
- Modify: Cargo.toml
- Modify: Cargo.lock
- Modify: src/lib.rs
- Delete: src/tui/
- Delete: tests/components.rs
- Delete: tests/overlay.rs
- Delete: tests/renderer.rs
- Delete: tests/terminal.rs
- Delete: tests/text_layout.rs
- Delete: tests/tui.rs
- Modify: README.md only if it describes main-screen or scrollback-preserving behavior

**Interfaces:**
- Removes: public moh::tui and every custom renderer/component/ANSI interface.
- Removes: vte and vt100 dependencies.
- Preserves: the private Ratatui client and all product-level tests migrated in Tasks 1-6.

- [ ] **Step 1: Prove no product code still uses the old framework**

Run:

~~~bash
rg -n "moh::tui|crate::tui|pub mod tui|CURSOR_MARKER|LINE_RESET|ComponentId|OverlayId|RenderPlan|vt100|vte::" src tests Cargo.toml
~~~

Expected before deletion: matches are limited to src/tui, the six obsolete test files, src/lib.rs, and dependency declarations. Any match in src/client or unrelated tests must be migrated before continuing.

- [ ] **Step 2: Delete the old framework and obsolete tests**

Delete every path listed in this task. Remove pub mod tui from src/lib.rs and replace the crate comment with:

~~~rust
//! Backend, session, runtime, RPC, and tool primitives used by the moh application.
~~~

Remove:

~~~toml
vte = "0.15.0"
~~~

and:

~~~toml
vt100 = "0.16.2"
~~~

Regenerate Cargo.lock through Cargo's normal dependency resolution. Do not update unrelated direct dependency requirements.

- [ ] **Step 3: Run focused post-deletion checks**

Run:

~~~bash
cargo test --bin moh client::ui
cargo test --bin moh client::app::tests
cargo test --test client_server
rg -n "moh::tui|crate::tui|pub mod tui|CURSOR_MARKER|ComponentId|OverlayId|vt100|vte::" src tests Cargo.toml
~~~

Expected: all focused tests pass and rg returns no matches.

- [ ] **Step 4: Exercise the real pseudo-terminal**

Build first:

~~~bash
cargo build --locked
~~~

Launch target/debug/moh in a real PTY attached to a local session. Verify and record each observation:

1. the original screen disappears after alternate-screen entry;
2. transcript, prompt, and status occupy the full viewport;
3. Ctrl+O help and the model/effort selectors render inside the viewport;
4. PageUp and PageDown move by one page minus one row;
5. wheel up and wheel down move the transcript by three rows;
6. streamed content follows at the bottom;
7. streamed content does not move a manually scrolled viewport;
8. End at the end of the prompt returns to the latest transcript content;
9. resizing reflows content and terminal sizes below 20x3 show terminal too small;
10. Ctrl+C returns to the exact original screen without cancelling an active backend run.

Use an explicit PTY/device target when more than one terminal is available. Do not claim wheel or restoration behavior from TestBackend alone.

- [ ] **Step 5: Run the complete merged-tree validation**

Run:

~~~bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --locked
git diff --check
git status --short
~~~

Expected: every command exits zero. git status shows only the reviewed Task 7 deletions and dependency/library/README changes before commit.

- [ ] **Step 6: Commit**

~~~bash
git add Cargo.toml Cargo.lock src/lib.rs src/tui tests/components.rs tests/overlay.rs tests/renderer.rs tests/terminal.rs tests/text_layout.rs tests/tui.rs README.md
git diff --cached --check
git commit -m "refactor(tui): remove custom renderer"
~~~

If README.md did not require a change, omit it from git add. After committing, run git status --short and require a clean worktree.

- [ ] **Step 7: Prepare the issue-linked handoff**

Summarize the final commits, automated verification, and real-terminal observations. Use Closes #27 in the eventual PR body, not in an intermediate commit message. Distinguish TestBackend proof from manual alternate-screen, mouse-wheel, and restoration proof.
