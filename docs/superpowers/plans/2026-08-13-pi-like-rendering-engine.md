# Pi-Like Rendering Engine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a reusable Rust TUI library with Pi-style main-screen differential rendering, essential interactive components, overlays, and a mini-chat demo.

**Architecture:** Components render ANSI-styled, display-width-bounded lines into a retained document. `Tui` owns components, focus, overlays, and dirty state; `Renderer` compares complete logical frames and emits the smallest safe synchronized terminal update through a backend-neutral `Terminal` trait. The production adapter uses Crossterm, while recording and VT100-backed tests verify byte-level operations and final screen state.

**Tech Stack:** Rust 2024, `crossterm` 0.29.0, `unicode-width` 0.2.2, `unicode-segmentation` 1.13.3, `vte` 0.15.0, `thiserror` 2.0.20, and development-only `vt100` 0.16.2.

## Global Constraints

- Render in the terminal's main screen; do not enter the alternate screen.
- Preserve normal scrollback on ordinary appends and updates.
- The differential algorithm belongs to `moh`; do not add Ratatui or another TUI framework.
- Measure terminal cells, not bytes or Unicode scalar values.
- Never split a grapheme cluster or count ANSI control sequences as visible columns.
- Bracket every content mutation with synchronized-output sequences `\x1b[?2026h` and `\x1b[?2026l`.
- Keep root focus flat; nested focus traversal is outside this milestone.
- Keep Markdown, images, mouse support, themes, autocomplete, alternate-screen layouts, and model integration out of scope.
- Preserve unrelated working-tree changes and stage only files belonging to the current task.
- Before completion, run `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-targets`, and `cargo build --locked`.

## File structure

- `Cargo.toml`: runtime and test dependencies.
- `Cargo.lock`: resolved dependency graph.
- `src/lib.rs`: public library root.
- `src/tui/mod.rs`: public TUI exports and shared constants.
- `src/tui/error.rs`: typed renderer and component errors.
- `src/tui/input.rs`: backend-neutral input event model.
- `src/tui/text.rs`: ANSI tokenization, display width, wrapping, slicing, padding, and style restoration.
- `src/tui/component.rs`: component contract, opaque IDs, downcasting support, and input outcomes.
- `src/tui/components/mod.rs`: primitive component exports.
- `src/tui/components/text.rs`: cached wrapped text component.
- `src/tui/components/spacer.rs`: fixed-height blank component.
- `src/tui/components/container.rs`: vertical child composition.
- `src/tui/components/input.rs`: grapheme-aware single-line editor.
- `src/tui/terminal.rs`: terminal traits, geometry, Crossterm adapter, event conversion, and RAII lifecycle.
- `src/tui/renderer.rs`: retained frame state and main-screen differential writes.
- `src/tui/overlay.rs`: overlay options, geometry resolution, ANSI-safe composition, and stack entries.
- `src/tui/app.rs`: `Tui` component ownership, focus, dispatch, overlay lifecycle, frame construction, and dirty rendering.
- `src/demo.rs`: binary-only mini-chat application state and event reducer.
- `src/main.rs`: interactive demo entry point.
- `tests/text_layout.rs`: public text-utility behavior.
- `tests/components.rs`: primitive component behavior.
- `tests/renderer.rs`: recording-terminal differential behavior and VT100 screen assertions.
- `tests/overlay.rs`: overlay geometry and ANSI-safe composition.
- `tests/tui.rs`: focus, input routing, validation, dirty-state, and overlay integration.
- `tests/terminal.rs`: lifecycle order and backend event conversion.
- `README.md`: library usage, demo controls, and verification commands.

---

### Task 1: Establish the library contracts and ANSI/Unicode text engine

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Create: `src/lib.rs`
- Create: `src/tui/mod.rs`
- Create: `src/tui/error.rs`
- Create: `src/tui/input.rs`
- Create: `src/tui/component.rs`
- Create: `src/tui/text.rs`
- Create: `tests/text_layout.rs`

**Interfaces:**
- Produces `Component`, `ComponentId`, `InputEvent`, `InputOutcome`, `Key`, `Modifiers`, `RenderError`, `Result<T>`, `CURSOR_MARKER`, and the text-layout functions consumed by every later task.
- `visible_width`, `wrap_ansi`, `slice_columns`, and `pad_to_width` are the only approved display-width primitives; later tasks must not reimplement width math.

- [ ] **Step 1: Add the dependency set and empty library root**

Run:

```bash
cargo add crossterm@0.29.0 unicode-width@0.2.2 unicode-segmentation@1.13.3 vte@0.15.0 thiserror@2.0.20
cargo add --dev vt100@0.16.2
```

Create `src/lib.rs`:

```rust
pub mod tui;
```

Create `src/tui/mod.rs` with these module declarations and exports:

```rust
mod component;
mod error;
mod input;
pub mod text;

pub use component::{Component, ComponentId, InputOutcome};
pub use error::{RenderError, Result};
pub use input::{InputEvent, Key, Modifiers};

pub const CURSOR_MARKER: &str = "\x1b_moh:c\x07";
pub const LINE_RESET: &str = "\x1b[0m\x1b]8;;\x07";
```

- [ ] **Step 2: Write failing public text-layout tests**

Create `tests/text_layout.rs` with table-driven assertions covering:

```rust
use moh::tui::text::{pad_to_width, slice_columns, visible_width, wrap_ansi};

#[test]
fn visible_width_ignores_ansi_and_counts_terminal_cells() {
    assert_eq!(visible_width("abc"), 3);
    assert_eq!(visible_width("\x1b[31mred\x1b[0m"), 3);
    assert_eq!(visible_width("界"), 2);
    assert_eq!(visible_width("e\u{301}"), 1);
    assert_eq!(visible_width("👩‍💻"), 2);
}

#[test]
fn wrapping_preserves_graphemes_and_reopens_styles() {
    assert_eq!(wrap_ansi("ab界cd", 4).unwrap(), vec!["ab界", "cd"]);
    let lines = wrap_ansi("\x1b[31mabcdef\x1b[0m", 3).unwrap();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].starts_with("\x1b[31m"));
    assert!(lines[0].ends_with("\x1b[0m"));
    assert!(lines[1].starts_with("\x1b[31m"));
    assert_eq!(lines.iter().map(|line| visible_width(line)).collect::<Vec<_>>(), vec![3, 3]);
}

#[test]
fn slicing_and_padding_are_column_safe() {
    assert_eq!(slice_columns("a界bc", 1, 2).text, "界");
    assert_eq!(slice_columns("a界bc", 2, 2).text, " b");
    assert_eq!(visible_width(&pad_to_width("界", 4)), 4);
}
```

Also add explicit tests for empty input, width zero returning `RenderError::InvalidLayoutWidth`, embedded newlines, OSC 8 hyperlinks, a reset in the middle of a wrapped style, a family emoji, and a slice whose boundary lands inside a wide grapheme. The inside-grapheme rule is to substitute spaces for covered cells rather than emit half a grapheme.

- [ ] **Step 3: Run the text tests and verify the missing API failure**

Run:

```bash
cargo test --test text_layout
```

Expected: compilation fails because `moh::tui::text` functions and shared types are not implemented.

- [ ] **Step 4: Implement the public contracts**

Define `src/tui/component.rs` with these exact public shapes:

```rust
use std::any::Any;

use super::{InputEvent, RenderError};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ComponentId(pub(crate) u64);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputOutcome {
    Ignored,
    Consumed,
    Changed,
    Submitted(String),
    Dismissed,
}

pub trait AsAny {
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl<T: Any> AsAny for T {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

pub trait Component: AsAny {
    fn render(&mut self, width: u16) -> Result<Vec<String>, RenderError>;

    fn handle_input(&mut self, _event: &InputEvent) -> InputOutcome {
        InputOutcome::Ignored
    }

    fn set_focused(&mut self, _focused: bool) {}

    fn invalidate(&mut self) {}
}
```

Define `src/tui/input.rs`:

```rust
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Modifiers {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Key {
    Char(char),
    Enter,
    Escape,
    Left,
    Right,
    Home,
    End,
    Backspace,
    Delete,
    Tab,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputEvent {
    Key { key: Key, modifiers: Modifiers },
    Paste(String),
    Resize { width: u16, height: u16 },
    Unsupported,
}
```

Define `src/tui/error.rs` with `thiserror` and these variants:

```rust
use super::component::ComponentId;

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("interactive terminal required")]
    NotATerminal,
    #[error("invalid terminal size {width}x{height}")]
    InvalidTerminalSize { width: u16, height: u16 },
    #[error("layout width must be greater than zero")]
    InvalidLayoutWidth,
    #[error("component {component:?} line {line} is {actual} cells wide; maximum is {allowed}")]
    LineTooWide { component: ComponentId, line: usize, actual: usize, allowed: usize },
    #[error("expected at most one cursor marker, found {count}")]
    InvalidCursorMarkerCount { count: usize },
    #[error("component {0:?} does not exist")]
    UnknownComponent(ComponentId),
    #[error("component {component:?} is not a {expected}")]
    ComponentTypeMismatch { component: ComponentId, expected: &'static str },
}

pub type Result<T> = std::result::Result<T, RenderError>;
```

- [ ] **Step 5: Implement the ANSI tokenizer and layout functions**

In `src/tui/text.rs`, use `vte::Parser` to identify CSI/OSC control boundaries while retaining the original byte sequences in ordered tokens. Track SGR sequences since the most recent full reset and the current OSC 8 hyperlink opener in an `AnsiState`. Provide:

```rust
pub struct ColumnSlice {
    pub text: String,
    pub width: usize,
    pub state_at_start: AnsiState,
    pub state_at_end: AnsiState,
}

pub fn visible_width(input: &str) -> usize;
pub fn wrap_ansi(input: &str, width: usize) -> Result<Vec<String>>;
pub fn slice_columns(input: &str, start: usize, width: usize) -> ColumnSlice;
pub fn pad_to_width(input: &str, width: usize) -> String;
pub(crate) fn normalize_line(input: &str) -> String;
```

The implementation must follow these exact rules:

```rust
// Text tokens are split into Unicode grapheme clusters.
// Grapheme width comes from UnicodeWidthStr::width(grapheme).
// Control tokens are copied but add zero columns.
// CURSOR_MARKER is recognized as one indivisible zero-width control token even
// when the generic VTE parser classifies its APC payload as ignored bytes.
// A wrap closes active SGR and OSC 8 state with LINE_RESET, then reopens the
// saved state at the start of the continuation line.
// Newline always starts a new logical line, including consecutive newlines.
// A wide grapheme intersected by only part of a requested slice becomes the
// same number of ASCII spaces as the intersecting cells.
// pad_to_width truncates first, then appends spaces to the exact target width.
// normalize_line removes CURSOR_MARKER elsewhere in Task 5 and appends LINE_RESET.
```

- [ ] **Step 6: Run and harden the text tests**

Run:

```bash
cargo test --test text_layout
cargo clippy --lib --tests -- -D warnings
```

Expected: all text tests pass and Clippy reports no warnings. Add a regression assertion for every defect discovered while making the table pass.

- [ ] **Step 7: Commit Task 1**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/tui/mod.rs src/tui/error.rs src/tui/input.rs src/tui/component.rs src/tui/text.rs tests/text_layout.rs
git commit -m "feat: add TUI text layout foundation"
```

---

### Task 2: Implement reusable primitive components

**Files:**
- Modify: `src/tui/mod.rs`
- Create: `src/tui/components/mod.rs`
- Create: `src/tui/components/text.rs`
- Create: `src/tui/components/spacer.rs`
- Create: `src/tui/components/container.rs`
- Create: `src/tui/components/input.rs`
- Create: `tests/components.rs`

**Interfaces:**
- Consumes `Component`, `InputEvent`, `InputOutcome`, `RenderError`, `CURSOR_MARKER`, and `wrap_ansi` from Task 1.
- Produces `Text`, `Spacer`, `Container`, and `Input` for `Tui` and the demo.

- [ ] **Step 1: Write failing component tests**

Create `tests/components.rs` with these representative tests and a table for every key variant:

```rust
use moh::tui::{Component, InputEvent, InputOutcome, Key, Modifiers};
use moh::tui::components::{Container, Input, Spacer, Text};

fn key(key: Key) -> InputEvent {
    InputEvent::Key { key, modifiers: Modifiers::default() }
}

#[test]
fn text_wraps_and_spacer_has_fixed_height() {
    assert_eq!(Text::new("ab界cd").render(4).unwrap(), vec!["ab界", "cd"]);
    assert_eq!(Spacer::new(2).render(80).unwrap(), vec!["", ""]);
}

#[test]
fn container_preserves_child_order_and_supports_dynamic_addition() {
    let mut container = Container::new();
    container.push(Text::new("first"));
    container.push(Spacer::new(1));
    container.push(Text::new("second"));
    assert_eq!(container.render(20).unwrap(), vec!["first", "", "second"]);
}

#[test]
fn input_edits_by_grapheme_and_submits() {
    let mut input = Input::new("> ");
    input.set_focused(true);
    assert_eq!(input.handle_input(&key(Key::Char('a'))), InputOutcome::Changed);
    assert_eq!(input.handle_input(&InputEvent::Paste("👩‍💻界".into())), InputOutcome::Changed);
    assert_eq!(input.handle_input(&key(Key::Left)), InputOutcome::Changed);
    assert_eq!(input.handle_input(&key(Key::Backspace)), InputOutcome::Changed);
    assert_eq!(input.handle_input(&key(Key::Enter)), InputOutcome::Submitted("a界".into()));
    assert_eq!(input.value(), "");
}
```

Add assertions for Home, End, Right at end, Left at start, Delete, Backspace at start, empty Enter, control-modified characters being ignored, multiline paste normalized to spaces, horizontal scrolling at widths 1/2/8, exactly one cursor marker when focused, no marker when unfocused, cache reuse for unchanged `Text`, and `invalidate()` forcing a new render.

- [ ] **Step 2: Verify the component API is absent**

Run:

```bash
cargo test --test components
```

Expected: compilation fails because `moh::tui::components` does not exist.

- [ ] **Step 3: Implement `Text`, `Spacer`, and `Container`**

Export the module from `src/tui/mod.rs` with `pub mod components;`. Implement these public constructors and mutators:

```rust
impl Text {
    pub fn new(text: impl Into<String>) -> Self;
    pub fn set_text(&mut self, text: impl Into<String>);
    pub fn text(&self) -> &str;
}

impl Spacer {
    pub fn new(rows: usize) -> Self;
}

impl Container {
    pub fn new() -> Self;
    pub fn push(&mut self, component: impl Component + 'static);
    pub fn push_boxed(&mut self, component: Box<dyn Component>);
    pub fn clear(&mut self);
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}
```

`Text` caches `(width, source_revision, Vec<String>)`. `set_text` increments the revision. `Container::render` concatenates child lines and propagates the first error. `Container::invalidate` calls every child's `invalidate`.

- [ ] **Step 4: Implement grapheme-aware `Input`**

Use this state and public API in `src/tui/components/input.rs`:

```rust
pub struct Input {
    prompt: String,
    value: String,
    cursor_grapheme: usize,
    focused: bool,
    scroll_column: usize,
}

impl Input {
    pub fn new(prompt: impl Into<String>) -> Self;
    pub fn value(&self) -> &str;
    pub fn set_value(&mut self, value: impl Into<String>);
    pub fn clear(&mut self);
}
```

On every edit, derive grapheme byte boundaries with `UnicodeSegmentation::grapheme_indices`. Keep `cursor_grapheme` between zero and the grapheme count. Render the prompt plus a horizontally sliced value so the cursor cell remains visible. Insert `CURSOR_MARKER` immediately before the grapheme at the cursor, or before one reverse-video space at end-of-input. If width is smaller than the prompt, truncate the prompt and still produce a line no wider than the supplied width.

- [ ] **Step 5: Run component tests and library lint**

Run:

```bash
cargo test --test components
cargo clippy --lib --tests -- -D warnings
```

Expected: all component cases pass without warnings.

- [ ] **Step 6: Commit Task 2**

```bash
git add src/tui/mod.rs src/tui/components tests/components.rs
git commit -m "feat: add TUI primitive components"
```

---

### Task 3: Build the backend-neutral differential renderer

**Files:**
- Modify: `src/tui/mod.rs`
- Create: `src/tui/terminal.rs`
- Create: `src/tui/renderer.rs`
- Create: `tests/renderer.rs`

**Interfaces:**
- Consumes `RenderError`, `Result`, and `LINE_RESET` from Task 1.
- Produces `Terminal`, `TerminalSize`, `Frame`, `CursorPosition`, and `Renderer` for the controller and production backend.

- [ ] **Step 1: Write a recording terminal and failing renderer tests**

In `tests/renderer.rs`, define:

```rust
#[derive(Default)]
struct RecordingTerminal {
    size: TerminalSize,
    writes: Vec<u8>,
    fail_write: bool,
    fail_flush: bool,
}

impl Terminal for RecordingTerminal {
    fn size(&self) -> std::io::Result<TerminalSize> { Ok(self.size) }
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if self.fail_write { return Err(std::io::Error::other("write failed")); }
        self.writes.extend_from_slice(bytes);
        Ok(())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if self.fail_flush { return Err(std::io::Error::other("flush failed")); }
        Ok(())
    }
}
```

Test this exact state sequence:

```rust
let mut renderer = Renderer::new();
renderer.render(&mut terminal, Frame::new(vec!["one".into(), "two".into()], None, size)).unwrap();
assert!(output().starts_with("\x1b[?2026h"));
assert!(output().ends_with("\x1b[?2026l"));

terminal.clear_output();
renderer.render(&mut terminal, Frame::new(vec!["one".into(), "two".into()], None, size)).unwrap();
assert_eq!(output(), "");

terminal.clear_output();
renderer.render(&mut terminal, Frame::new(vec!["one".into(), "changed".into()], None, size)).unwrap();
assert!(output().contains("changed"));
assert!(!output().contains("one"));
```

Add tests for pure append using CRLF without screen clear, two separated changes rewriting the inclusive changed range, shorter content clearing stale rows, width change containing `\x1b[2J\x1b[H`, inaccessible changed rows choosing full redraw, height-only change retaining content when safe, cursor-only repositioning without sync markers, and write/flush failure preserving the previous snapshot.

- [ ] **Step 2: Verify renderer tests fail**

Run:

```bash
cargo test --test renderer
```

Expected: compilation fails because terminal and renderer types are absent.

- [ ] **Step 3: Implement terminal and frame contracts**

Create `src/tui/terminal.rs`:

```rust
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TerminalSize {
    pub width: u16,
    pub height: u16,
}

pub trait Terminal {
    fn size(&self) -> std::io::Result<TerminalSize>;
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()>;
    fn flush(&mut self) -> std::io::Result<()>;
}
```

Create `src/tui/renderer.rs`:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CursorPosition { pub row: usize, pub column: usize }

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
    pub lines: Vec<String>,
    pub cursor: Option<CursorPosition>,
    pub size: TerminalSize,
}

impl Frame {
    pub fn new(lines: Vec<String>, cursor: Option<CursorPosition>, size: TerminalSize) -> Self;
}

pub struct Renderer {
    previous: Option<Frame>,
    hardware_row: usize,
    viewport_top: usize,
    cursor_visible: bool,
}

impl Renderer {
    pub fn new() -> Self;
    pub fn render(&mut self, terminal: &mut dyn Terminal, frame: Frame) -> Result<()>;
    pub fn reset(&mut self);
}
```

- [ ] **Step 4: Implement differential update selection**

Implement `Renderer::render` with an internal `RenderPlan` enum:

```rust
enum RenderPlan {
    Initial,
    Unchanged,
    Append { from: usize },
    Rewrite { first: usize, last: usize },
    FullRedraw,
}
```

Select `FullRedraw` on width change, an update above `viewport_top`, or a shrink requiring more rows to clear than fit below the cursor. Select `Append` only when every previous line is byte-identical and the new frame has additional lines. For `Rewrite`, calculate the inclusive first/last unequal indices over the maximum frame length.

Build the entire ANSI mutation in memory before the single `Terminal::write` call. Commit `previous`, `hardware_row`, `viewport_top`, and cursor visibility only after both `write` and `flush` succeed. Use relative `CSI n A/B`, carriage return, `CSI 2K`, CRLF, `CSI 2J`, `CSI H`, `CSI ?25h`, and `CSI ?25l`; never use absolute terminal rows for main-screen document updates.

- [ ] **Step 5: Add VT100 final-screen assertions**

Feed initial/change/append/shrink byte streams into `vt100::Parser::new(height, width, 0)`. Assert `parser.screen().contents()` equals the expected rows after every mutation, and assert `parser.screen().cursor_position()` for a frame with `CursorPosition { row: 1, column: 3 }`.

- [ ] **Step 6: Run focused verification**

Run:

```bash
cargo test --test renderer
cargo clippy --lib --tests -- -D warnings
```

Expected: operation assertions and VT100 screen-state assertions pass.

- [ ] **Step 7: Commit Task 3**

```bash
git add src/tui/mod.rs src/tui/terminal.rs src/tui/renderer.rs tests/renderer.rs
git commit -m "feat: add main-screen differential renderer"
```

---

### Task 4: Add overlay geometry and ANSI-safe composition

**Files:**
- Modify: `src/tui/mod.rs`
- Create: `src/tui/overlay.rs`
- Create: `tests/overlay.rs`

**Interfaces:**
- Consumes `Component`, `ComponentId`, `pad_to_width`, `slice_columns`, `visible_width`, and `LINE_RESET`.
- Produces `OverlayId`, `OverlayOptions`, `OverlayAnchor`, `SizeValue`, `Margin`, `OverlayGeometry`, and `composite_line` for `Tui`.

- [ ] **Step 1: Write failing overlay geometry tests**

Create `tests/overlay.rs` with a 100x40 viewport table asserting `(row, column, width, height)` for all anchors:

```rust
let expected = [
    (OverlayAnchor::Center, (15, 30)),
    (OverlayAnchor::TopLeft, (0, 0)),
    (OverlayAnchor::TopCenter, (0, 30)),
    (OverlayAnchor::TopRight, (0, 60)),
    (OverlayAnchor::LeftCenter, (15, 0)),
    (OverlayAnchor::RightCenter, (15, 60)),
    (OverlayAnchor::BottomLeft, (30, 0)),
    (OverlayAnchor::BottomCenter, (30, 30)),
    (OverlayAnchor::BottomRight, (30, 60)),
];
```

Use overlay size 40x10. Add cases for 50-percent width, 80-percent max height, per-edge margins, offsets, width larger than viewport, zero-sized content, and negative offsets clamped inside margins.

Add composition assertions:

```rust
let line = composite_line("\x1b[31mabcdefgh\x1b[0m", "XY", 3, 2, 8);
assert_eq!(visible_width(&line), 8);
assert!(line.contains("XY"));
assert!(line.ends_with(LINE_RESET));
```

Feed the result to a VT100 parser and assert columns 0–2 and 5–7 remain red while the overlay cells do not inherit red unless the overlay supplies it.

- [ ] **Step 2: Verify overlay tests fail**

Run:

```bash
cargo test --test overlay
```

Expected: compilation fails because overlay types are absent.

- [ ] **Step 3: Implement overlay public types and geometry**

Use these public types:

```rust
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct OverlayId(pub(crate) u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SizeValue { Cells(u16), Percent(u8) }

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Margin { pub top: u16, pub right: u16, pub bottom: u16, pub left: u16 }

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OverlayAnchor {
    #[default] Center,
    TopLeft, TopCenter, TopRight,
    LeftCenter, RightCenter,
    BottomLeft, BottomCenter, BottomRight,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OverlayOptions {
    pub width: Option<SizeValue>,
    pub max_height: Option<SizeValue>,
    pub anchor: OverlayAnchor,
    pub offset_x: i16,
    pub offset_y: i16,
    pub margin: Margin,
    pub capturing: bool,
}
```

`Percent` clamps to 0–100. Resolve percentage dimensions with integer floor. Apply offsets after anchoring, then clamp within the margin-adjusted viewport.

- [ ] **Step 4: Implement ANSI-safe line and frame composition**

Provide:

```rust
pub fn composite_line(base: &str, overlay: &str, start: usize, width: usize, total: usize) -> String;
pub(crate) fn composite_overlay(base: &mut Vec<String>, lines: &[String], geometry: OverlayGeometry, viewport: TerminalSize);
```

Build a line as `base-prefix + LINE_RESET + padded-overlay + LINE_RESET + reopened-base-suffix`, using `ColumnSlice::state_at_start` to reopen the suffix's SGR and OSC 8 state. Synthesize blank base lines when the overlay occupies rows below current content. Clip at `max_height` and never grow beyond the terminal height.

- [ ] **Step 5: Run overlay tests and lint**

Run:

```bash
cargo test --test overlay
cargo clippy --lib --tests -- -D warnings
```

Expected: all geometry, style, width, and clipping assertions pass.

- [ ] **Step 6: Commit Task 4**

```bash
git add src/tui/mod.rs src/tui/overlay.rs tests/overlay.rs
git commit -m "feat: add composited TUI overlays"
```

---

### Task 5: Implement the `Tui` controller, focus, and frame construction

**Files:**
- Modify: `src/tui/mod.rs`
- Create: `src/tui/app.rs`
- Create: `tests/tui.rs`

**Interfaces:**
- Consumes all component, text, renderer, terminal, and overlay contracts from Tasks 1–4.
- Produces `Tui<T: Terminal>` and the stable application-facing methods used by the demo.

- [ ] **Step 1: Write failing controller tests**

Create `tests/tui.rs` around a `RecordingTerminal`. Cover this public workflow:

```rust
let mut tui = Tui::new(RecordingTerminal::new(40, 10));
let transcript = tui.add_component(Container::new());
let input = tui.add_component(Input::new("> "));
tui.focus(input).unwrap();

assert_eq!(tui.dispatch_input(&key(Key::Char('x'))).unwrap(), InputOutcome::Changed);
assert!(tui.is_dirty());
tui.render_if_dirty().unwrap();
assert!(!tui.is_dirty());

tui.component_mut::<Container>(transcript).unwrap().push(Text::new("hello"));
tui.request_render();
tui.render_if_dirty().unwrap();
```

Add tests for unknown IDs, type mismatch, replacing focus, removing focused components, unchanged `render_if_dirty`, resize dirtiness, line-too-wide errors identifying the root ID and line, multiple cursor markers, a capturing overlay receiving input before the input component, a non-capturing overlay leaving input focus unchanged, top overlay composition order, dismiss/hide restoring prior focus, and a failed terminal write leaving `is_dirty() == true`.

- [ ] **Step 2: Verify controller tests fail**

Run:

```bash
cargo test --test tui
```

Expected: compilation fails because `Tui` is absent.

- [ ] **Step 3: Implement component storage and checked access**

Create `src/tui/app.rs` with:

```rust
pub struct Tui<T: Terminal> {
    terminal: T,
    renderer: Renderer,
    components: Vec<ComponentEntry>,
    overlays: Vec<OverlayEntry>,
    focused: Option<ComponentId>,
    dirty: bool,
    next_component_id: u64,
    next_overlay_id: u64,
}

impl<T: Terminal> Tui<T> {
    pub fn new(terminal: T) -> Self;
    pub fn add_component(&mut self, component: impl Component + 'static) -> ComponentId;
    pub fn remove_component(&mut self, id: ComponentId) -> Result<()>;
    pub fn component_mut<C: Component + 'static>(&mut self, id: ComponentId) -> Result<&mut C>;
    pub fn focus(&mut self, id: ComponentId) -> Result<()>;
    pub fn request_render(&mut self);
    pub fn is_dirty(&self) -> bool;
    pub fn terminal(&self) -> &T;
    pub fn terminal_mut(&mut self) -> &mut T;
}
```

`component_mut` locates the opaque ID, calls `AsAny::as_any_mut().downcast_mut::<C>()`, and returns `ComponentTypeMismatch` with `std::any::type_name::<C>()` when necessary. Every mutating accessor marks the TUI dirty before returning.

- [ ] **Step 4: Implement focus and overlay lifecycle**

Add:

```rust
pub fn show_overlay(&mut self, component: impl Component + 'static, options: OverlayOptions) -> OverlayId;
pub fn hide_overlay(&mut self, id: OverlayId) -> bool;
pub fn dispatch_input(&mut self, event: &InputEvent) -> Result<InputOutcome>;
```

An `OverlayEntry` stores `id`, boxed component, options, and `previous_focus`. Showing a capturing overlay unfocuses the base target and focuses the overlay. Dispatch walks visible overlays from newest to oldest and selects the first capturing one; otherwise it targets the focused root. Hiding a capturing overlay restores the newest surviving capturing overlay or the recorded root focus.

- [ ] **Step 5: Implement validated frame construction and dirty rendering**

Add:

```rust
pub fn render_if_dirty(&mut self) -> Result<bool>;
pub fn render_now(&mut self) -> Result<()>;
```

`render_now` reads `TerminalSize`, rejects zero dimensions, renders every root at full width, validates each root line before concatenation, resolves/composites overlays, finds all `CURSOR_MARKER` occurrences, rejects counts above one, converts the marker byte position to display column, strips it, appends `LINE_RESET` to every line, and calls `Renderer::render`. Only a successful call clears dirty state. `render_if_dirty` returns `false` without reading terminal state when clean.

- [ ] **Step 6: Run controller tests and all library tests**

Run:

```bash
cargo test --test tui
cargo test --lib --tests
cargo clippy --lib --tests -- -D warnings
```

Expected: focus, overlay, validation, and retry behavior pass with no warnings.

- [ ] **Step 7: Commit Task 5**

```bash
git add src/tui/mod.rs src/tui/app.rs tests/tui.rs
git commit -m "feat: add TUI component controller"
```

---

### Task 6: Add Crossterm events and recoverable terminal lifecycle

**Files:**
- Modify: `src/tui/terminal.rs`
- Create: `tests/terminal.rs`

**Interfaces:**
- Consumes `InputEvent`, `Key`, `Modifiers`, `Terminal`, `TerminalSize`, and `RenderError`.
- Produces `CrosstermTerminal`, `EventSource`, `TerminalSession`, and `crossterm_event_to_input` for the executable.

- [ ] **Step 1: Write failing event-conversion and lifecycle tests**

Create `tests/terminal.rs` with direct Crossterm event values asserting:

```rust
assert_eq!(
    crossterm_event_to_input(Event::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL))),
    InputEvent::Key { key: Key::Char('o'), modifiers: Modifiers { control: true, alt: false, shift: false } }
);
assert_eq!(
    crossterm_event_to_input(Event::Resize(120, 35)),
    InputEvent::Resize { width: 120, height: 35 }
);
assert_eq!(crossterm_event_to_input(Event::Paste("界".into())), InputEvent::Paste("界".into()));
```

Test all editing keys, modifier combinations, key-release filtering, mouse/focus events becoming `Unsupported`, and resize.

Define a fake `SessionBackend` recording calls and assert exact successful order:

```text
enable_raw, enable_bracketed_paste, hide_cursor,
show_cursor, reset_styles, disable_bracketed_paste, disable_raw
```

Inject failure at each setup operation and assert only successfully enabled state is unwound in reverse-safe order. Inject cleanup failures and assert the first error is returned after every remaining cleanup action is attempted.

- [ ] **Step 2: Verify terminal tests fail**

Run:

```bash
cargo test --test terminal
```

Expected: compilation fails because production lifecycle APIs are absent.

- [ ] **Step 3: Implement Crossterm adapter and normalized events**

Add to `src/tui/terminal.rs`:

```rust
pub struct CrosstermTerminal<W: std::io::Write> { writer: W }

impl<W: std::io::Write> CrosstermTerminal<W> {
    pub fn new(writer: W) -> Self;
}

pub trait EventSource {
    fn poll_event(&mut self, timeout: std::time::Duration) -> std::io::Result<Option<InputEvent>>;
}

pub struct CrosstermEvents;

pub fn crossterm_event_to_input(event: crossterm::event::Event) -> InputEvent;
```

`CrosstermTerminal::size` calls `crossterm::terminal::size`. `write` and `flush` delegate to the writer. `CrosstermEvents::poll_event` calls `event::poll` then `event::read`, discarding `KeyEventKind::Release` as `Unsupported`.

- [ ] **Step 4: Implement RAII session setup and explicit restoration**

Define a `#[doc(hidden)]` backend trait so integration tests can inject lifecycle failures without making it part of the advertised high-level API, then expose:

```rust
#[doc(hidden)]
pub trait SessionBackend {
    fn is_interactive(&self) -> bool;
    fn size(&self) -> std::io::Result<TerminalSize>;
    fn enable_raw(&mut self) -> std::io::Result<()>;
    fn disable_raw(&mut self) -> std::io::Result<()>;
    fn enable_bracketed_paste(&mut self) -> std::io::Result<()>;
    fn disable_bracketed_paste(&mut self) -> std::io::Result<()>;
    fn hide_cursor(&mut self) -> std::io::Result<()>;
    fn show_cursor(&mut self) -> std::io::Result<()>;
    fn reset_styles(&mut self) -> std::io::Result<()>;
}

pub struct TerminalSession<B: SessionBackend> {
    backend: B,
    raw_enabled: bool,
    paste_enabled: bool,
    cursor_hidden: bool,
    restored: bool,
}

impl TerminalSession<CrosstermSessionBackend> {
    pub fn start() -> Result<Self>;
}

impl<B: SessionBackend> TerminalSession<B> {
    #[doc(hidden)]
    pub fn start_with(backend: B) -> Result<Self>;
    pub fn restore(&mut self) -> Result<()>;
}
```

`start` requires both stdin and stdout to satisfy `std::io::IsTerminal`, rejects zero dimensions, enables raw mode, enables bracketed paste, and hides the cursor. `restore` is idempotent and always attempts cursor show, `LINE_RESET`, bracketed-paste disable, and raw-mode disable according to enabled flags. `Drop` calls best-effort `restore` without panicking.

- [ ] **Step 5: Run lifecycle tests**

Run:

```bash
cargo test --test terminal
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: event mapping and every setup/cleanup failure point pass.

- [ ] **Step 6: Commit Task 6**

```bash
git add src/tui/terminal.rs tests/terminal.rs
git commit -m "feat: add Crossterm terminal lifecycle"
```

---

### Task 7: Build the mini-chat demo and document the library

**Files:**
- Create: `src/demo.rs`
- Modify: `src/main.rs`
- Modify: `README.md`

**Interfaces:**
- Consumes only public `moh::tui` APIs from Tasks 1–6.
- Produces the interactive default binary and user-facing library example.

- [ ] **Step 1: Write failing demo state tests**

In `src/demo.rs`, begin with unit tests for a pure reducer. Use:

```rust
#[derive(Debug, Eq, PartialEq)]
enum DemoAction {
    None,
    Submit(String),
    OpenHelp,
    CloseHelp,
    Resize,
    Exit,
}
```

Assert `Ctrl+C -> Exit`, `Ctrl+O -> OpenHelp`, Escape with help open -> `CloseHelp`, resize -> `Resize`, submitted input -> `Submit`, and ordinary changed input -> `None`. Assert the deterministic response for `"hello"` is exactly `"moh: received 5 characters"`, using Unicode character count rather than byte count.

- [ ] **Step 2: Run binary tests and verify failure**

Run:

```bash
cargo test --bin moh
```

Expected: compilation fails until the demo reducer and application loop exist.

- [ ] **Step 3: Implement mini-chat construction and reducer**

`src/demo.rs` must construct, through public library APIs:

```rust
pub struct DemoIds {
    pub transcript: ComponentId,
    pub status: ComponentId,
    pub input: ComponentId,
    pub help: Option<OverlayId>,
}

pub fn build<T: Terminal>(terminal: T) -> Result<(Tui<T>, DemoIds)>;
pub fn run() -> Result<()>;
```

Initial content is:

```text
moh — Pi-like renderer demo
Enter sends · Ctrl+O help · Ctrl+C exits

> 
```

On submission, append `you: {text}` and `moh: received {character_count} characters` to the transcript, set status to `ready`, and request a render. The help overlay is centered, 60-percent width, maximum 8 rows, margin 1, capturing, and contains the controls plus `Esc closes this help`. Resize only requests a new frame.

- [ ] **Step 4: Implement the production event loop in `src/main.rs`**

Use this cleanup structure:

```rust
mod demo;

fn main() {
    if let Err(error) = demo::run() {
        eprintln!("moh: {error}");
        std::process::exit(1);
    }
}
```

Inside `run`, create `TerminalSession::start()` before the TUI, render the initial frame, poll events at 16 ms, process globals before `Tui::dispatch_input`, and call `render_if_dirty` once per iteration. On exit, call `restore()` explicitly so cleanup errors can be returned; `Drop` remains the fallback.

- [ ] **Step 5: Update README usage and scope**

Replace the early hello-world wording with:

```markdown
## Rendering engine

`moh::tui` is a small main-screen terminal UI library inspired by Pi's rendering model. Components return width-bounded lines; the renderer retains the previous frame and updates only the safe changed range using synchronized output.

Run the mini-chat demo with `cargo run`. Enter submits, Ctrl+O opens help, Escape closes help, and Ctrl+C exits. The demo requires an interactive terminal.
```

Add a compilable library snippet constructing `Tui`, `Text`, and `Input`, and list the deferred features from the design without claiming support for them.

- [ ] **Step 6: Run demo tests and non-TTY diagnostic check**

Run:

```bash
cargo test --bin moh
cargo run </dev/null
```

Expected: binary tests pass; redirected execution exits nonzero and prints `moh: interactive terminal required` without a panic or raw escape stream.

- [ ] **Step 7: Commit Task 7**

```bash
git add src/demo.rs src/main.rs README.md
git commit -m "feat: add interactive renderer demo"
```

---

### Task 8: Run full verification and perform real-terminal acceptance

**Files:**
- Modify only files required by failures found during this task.

**Interfaces:**
- Consumes the complete library and demo.
- Produces the verified milestone with no known acceptance gaps.

- [ ] **Step 1: Run the exact repository validation suite**

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --locked
```

Expected: every command exits successfully. If a command fails, add the smallest regression test that reproduces the defect, fix it, rerun the focused test, then rerun all four commands.

- [ ] **Step 2: Audit spec coverage mechanically**

Run:

```bash
rg -n "alternate screen|ratatui|todo!|unimplemented!|panic!" src README.md
rg -n "CURSOR_MARKER|LINE_RESET|2026h|2026l|LineTooWide|InvalidCursorMarkerCount" src tests
git diff --check
```

Expected: no alternate-screen or Ratatui implementation, no placeholder macros, synchronized output and typed invariants are covered in source and tests, and no whitespace errors exist. A descriptive `panic!` is permitted only inside test code.

- [ ] **Step 3: Exercise the demo in a real PTY**

Run `cargo run` in an interactive terminal and verify this script manually:

1. type `Zażółć 👩‍💻 界` and press Enter;
2. confirm both transcript lines appear with intact Unicode;
3. submit at least fifteen prompts and confirm normal scrollback remains usable;
4. press `Ctrl+O`, confirm the centered help overlay, then press Escape;
5. resize narrower and wider, confirming wrapped content reconstructs without stale rows;
6. press `Ctrl+C`, then type a shell command to confirm echo, cursor, and canonical input behavior were restored.

- [ ] **Step 4: Review public API documentation**

Run:

```bash
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

Expected: documentation builds without warnings. Add concise rustdoc to any exported item reported missing context during review.

- [ ] **Step 5: Commit verification fixes if needed**

If Task 8 changed files:

```bash
git add Cargo.toml Cargo.lock README.md src tests
git commit -m "test: harden TUI rendering acceptance"
```

If no files changed, do not create an empty commit.

- [ ] **Step 6: Record final evidence**

Run:

```bash
git status --short
git log --oneline --decorate -10
```

Expected: the worktree contains no uncommitted task changes and history shows one focused commit per completed implementation task.
