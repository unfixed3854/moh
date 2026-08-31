# Pi-Like Rendering Engine Design

## Goal

Build a reusable Rust terminal UI library for `moh` with Pi-style retained components, main-screen differential rendering, focused keyboard input, and composited overlays. Include a mini-chat binary that exercises the library in the interaction pattern the coding harness will eventually use.

The milestone is a usable TUI foundation rather than a broad port of Pi. It includes the custom renderer and essential components while deliberately excluding Markdown, images, alternate-screen layouts, mouse handling, themes, and autocomplete.

## Approach

Implement a custom line-differential engine behind a terminal abstraction. Components return width-bounded, ANSI-styled lines. The renderer retains the previous frame, composites overlays, compares logical lines, and writes only the smallest safe changed region using synchronized terminal output.

Use focused crates for terminal and text mechanics:

- `crossterm` for terminal detection, raw mode, keyboard events, terminal sizing, cursor commands, and resize events;
- `unicode-width` for terminal cell width;
- `unicode-segmentation` for grapheme-safe cursor movement and editing;
- `vte` as the low-level ANSI parser used by `moh`'s own style-aware tokenizer for measurement, wrapping, slicing, and overlay composition;
- `vt100` as a development dependency for end-to-end screen-state assertions.

The differential algorithm remains owned by `moh`; the project will not use Ratatui or another full TUI framework.

## Library boundary

Convert the package into a library plus its existing default binary. Expose TUI functionality beneath `moh::tui`, with focused modules for components, rendering, terminal I/O, display-width utilities, overlays, input events, and errors. The binary imports the public library API and has no privileged access to renderer internals.

The central component contract is:

```rust
pub trait Component {
    fn render(&mut self, width: u16) -> Result<Vec<String>, RenderError>;
    fn handle_input(&mut self, event: &InputEvent) -> InputOutcome;
    fn set_focused(&mut self, focused: bool);
    fn invalidate(&mut self);
}
```

Components that do not accept input use the default no-op implementations for input and focus methods. Rendering accepts `&mut self` so components may maintain caches keyed by available width.

Top-level components receive stable opaque `ComponentId` values from `Tui::add_component`. Application code uses these IDs to focus components and access them through checked update methods. This avoids leaking indices or requiring self-referential ownership.

## Architecture

The runtime has four layers:

1. The application owns semantic state and reacts to submitted prompts or dismissed overlays.
2. `Tui` owns the ordered root components, current focus, overlay stack, dirty state, and render scheduling.
3. `Renderer` owns the previous logical frame, last terminal dimensions, current managed cursor row, and the differential update algorithm.
4. A `Terminal` interface provides geometry, writes, flushing, lifecycle setup/restore, and event polling. `CrosstermTerminal` is the production implementation; recording and virtual-terminal-backed implementations support tests.

Root components are arranged vertically in insertion order. Containers are presentational during this milestone; nested focus traversal is outside scope. One top-level component is focused at a time unless a capturing overlay temporarily owns input.

The main-screen renderer preserves the user's normal terminal scrollback. It never enters the alternate screen. The engine treats the rendered document as an append-oriented conversation whose tail normally occupies the visible viewport.

## Components

The first release includes:

- `Text`: stores plain or ANSI-styled text and wraps it to the available display width without splitting grapheme clusters or corrupting active ANSI styles;
- `Container`: owns child components, renders them vertically, supports dynamic additions, propagates invalidation, and remains non-focusable;
- `Input`: a single-line editor with grapheme-safe insertion, Left/Right, Home/End, Backspace/Delete, Enter submission, and a cursor marker in its rendered output;
- `Spacer`: renders a configured number of empty rows;
- an internal or public simple overlay content component sufficient for the demo's help dialog.

`InputOutcome` communicates UI-relevant behavior without embedding application logic in components:

```rust
pub enum InputOutcome {
    Ignored,
    Consumed,
    Changed,
    Submitted(String),
    Dismissed,
}
```

`Submitted` clears the input after transferring its previous content to the application. `Changed` marks the TUI dirty. `Consumed` handles a recognized event that does not alter rendered state. `Dismissed` asks the application or TUI overlay controller to close the active transient UI.

## Input and focus flow

`crossterm` events are normalized into a small public `InputEvent` model so components do not depend directly on the backend. It covers character input, navigation/editing keys, Enter, Escape, modifiers, paste content, resize, and unsupported events.

Input routing follows this precedence:

1. application-global shortcuts handle `Ctrl+C` and `Ctrl+O` in the demo;
2. the topmost visible capturing overlay receives keyboard input;
3. otherwise, the focused root component receives input;
4. unhandled input is returned to the application as `Ignored`.

Showing a capturing overlay records the previous focus, marks the overlay focused, and temporarily unfocuses the base component. Closing it restores the recorded base focus if that component still exists. Escape produces `Dismissed` for the demo help overlay.

## Frame construction

For each dirty frame, `Tui`:

1. reads the current terminal dimensions;
2. renders root components at the terminal width and concatenates their lines;
3. resolves each visible overlay's size and position, then composites overlays from back to front;
4. finds and strips the zero-width cursor marker, recording its document row and display column;
5. normalizes every line with a full SGR reset and OSC 8 hyperlink reset;
6. verifies that every line's visible width is no greater than the terminal width;
7. passes the complete logical frame and desired cursor position to `Renderer`;
8. clears the dirty flag only after a successful terminal write and flush.

Multiple requests before the next event-loop iteration set one dirty flag and produce one frame. Keyboard-driven changes may render immediately at the end of event dispatch so typing remains responsive.

The cursor marker is an internal ANSI control sequence ignored by terminal width calculations. Only a focused component should emit it. If multiple markers occur, rendering returns a typed invariant error rather than guessing which cursor is authoritative.

## Differential rendering

`Renderer` retains the last successfully written logical lines and geometry. It wraps every mutation in synchronized-output mode (`CSI ?2026h` before the update and `CSI ?2026l` afterward) so capable terminals display changes atomically.

Rendering paths are:

- **Initial frame:** write all lines without clearing the user's pre-existing terminal history.
- **No content changes:** write no frame content; reposition or show/hide the hardware cursor only if its state changed.
- **Pure append:** move to the end of the managed document and append the new rows, allowing normal terminal scrolling.
- **Changed range:** find the first and last unequal logical lines, move to the first changed visible row, clear each affected row, and rewrite through the last changed row.
- **Width change:** clear and fully reconstruct the managed document because wrapping changes downstream row boundaries.
- **Unsafe shrink or inconsistent geometry:** clear and fully reconstruct the managed document when stale rows cannot be erased without risking viewport corruption.

The renderer tracks the document row corresponding to the hardware cursor and the viewport's inferred top row. If a changed range begins above the accessible viewport or a relative cursor movement would be ambiguous, it selects the full-redraw recovery path.

Full redraw uses screen clearing only for content managed by `moh`. Because terminals cannot reliably reflow arbitrary scrollback, width-change recovery may replace the currently visible managed transcript. Ordinary appends and updates retain normal scrollback.

## ANSI and Unicode correctness

All layout is measured in terminal display cells, never bytes or scalar-value counts. ANSI control sequences have zero width. Grapheme clusters remain indivisible during editing, wrapping, truncation, and slicing. Width behavior must cover ASCII, wide CJK characters, combining marks, and emoji sequences.

The text utilities provide:

- visible display width;
- width-bounded wrapping that reapplies active styles on continuation lines;
- grapheme-safe truncation and column slicing;
- left/right padding to an exact visible width;
- ANSI state reset and reopening around overlay boundaries.

Each component must return lines within its assigned width. A violation identifies the component, its zero-based rendered line, the actual display width, and the allowed width. Overlay composition uses the same tokenizer and slicing primitives, ensuring the base line's style does not leak into the overlay and the overlay's style does not corrupt the preserved suffix.

## Overlays

Overlay options support:

- width as a fixed column count or terminal-width percentage;
- maximum height as fixed rows or terminal-height percentage;
- a uniform or per-edge margin;
- nine anchors: center, four corners, four edge centers;
- horizontal and vertical offsets;
- capturing or non-capturing focus behavior.

Resolution clamps dimensions and positions to the viewport after applying margins. Content exceeding maximum height is vertically clipped for this milestone; overlay scrolling is out of scope. Overlay lines are width-normalized before composition. Empty base rows are synthesized when an overlay extends below the root document but remains inside the visible viewport.

Overlays are stacked in creation order, and the topmost visible overlay is composited last. The demo uses one centered capturing help overlay, but stack behavior is deterministic and tested.

## Error handling and lifecycle

Public rendering failures are typed. The error model includes at least:

```rust
pub enum RenderError {
    Io(std::io::Error),
    NotATerminal,
    InvalidTerminalSize { width: u16, height: u16 },
    LineTooWide {
        component: ComponentId,
        line: usize,
        actual: usize,
        allowed: usize,
    },
    InvalidCursorMarkerCount { count: usize },
}
```

Backend initialization rejects a zero-sized terminal or non-TTY interactive demo with a concise diagnostic. Library users can use an in-memory terminal regardless of TTY state.

An RAII session guard enables raw mode and bracketed paste, hides or configures the cursor, and ensures cleanup on every normal error-return path. Cleanup disables bracketed paste and raw mode, resets SGR and hyperlinks, restores cursor visibility, and leaves the cursor after the final managed content. A panic hook may perform best-effort restoration before delegating to the previous hook; tests must not rely on unwinding alone for correctness.

Invalid overlay values are clamped rather than treated as fatal. Terminal I/O failures preserve the prior renderer snapshot and leave the TUI dirty so a caller may report or retry the failure.

## Mini-chat demo

Running `cargo run` starts a main-screen mini chat with:

- a transcript container;
- a one-line status component;
- a focused input prompt;
- `Enter` to append the current prompt as a user transcript entry;
- a deterministic local assistant response or acknowledgement, requiring no model or network integration;
- `Ctrl+O` to open a centered help overlay;
- `Escape` to close the help overlay;
- `Ctrl+C` to exit and restore terminal state.

The demo responds to resize events by requesting a new frame. Enough transcript entries cause natural main-screen scrolling. Copy and wording stay functional and minimal because visual theming is not part of this milestone.

## Testing

Testing is organized around pure boundaries and observable terminal state.

### Text utilities

Unit tests cover ANSI-free and ANSI-styled visible width, reset handling, grapheme-safe slicing, wrapping, truncation, padding, CJK characters, combining marks, emoji sequences, and styles spanning wrapped lines.

### Components

Unit tests cover `Text` wrapping and caching, `Container` order and invalidation, `Spacer` height, and `Input` insertion, navigation, Home/End, Backspace/Delete, Unicode grapheme handling, cursor placement, submission, clearing, focus changes, and invalidation.

### Renderer

A recording terminal verifies emitted operations for initial render, unchanged render, cursor-only movement, one-line and multi-line changes, pure append, safe deletion, unsafe shrink fallback, width resize fallback, height changes, synchronized-output boundaries, batching, and injected write/flush failures.

A headless VT parser consumes actual emitted ANSI bytes for representative sequences. Tests assert final screen rows, preserved content, absence of style leakage, and hardware cursor location rather than relying only on escape-string snapshots.

### Overlays and focus

Tests cover nine anchors, percentage and fixed sizing, margins, offsets, viewport clamping, vertical clipping, ANSI-styled base preservation, multiple overlay ordering, capturing versus non-capturing input, Escape dismissal, and base-focus restoration.

### Lifecycle and demo

Lifecycle tests use a fake backend to prove that normal exit and injected event/render failures invoke restoration in the correct order. The demo's application-state reducer is tested without a real TTY for prompt submission, acknowledgement insertion, help open/close, resize dirtiness, and exit.

Manual acceptance in a real terminal verifies Unicode entry, transcript scrolling, overlay display, terminal resizing, flicker-free streaming-like status changes, clean exit, and retained scrollback.

## Acceptance criteria

The milestone is complete when:

- downstream Rust code can build a main-screen TUI from the public `moh::tui` API;
- root components render vertically and the focused input receives normalized events;
- overlays composite without corrupting ANSI styles or exceeding the viewport;
- ordinary updates write only their safe changed range and appends preserve scrollback;
- synchronized output brackets every content mutation;
- Unicode and ANSI width behavior passes the specified tests;
- terminal state is restored on normal exit and recoverable failures;
- the mini-chat workflow works in a real terminal;
- `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-targets`, and `cargo build --locked` pass.

## Explicitly deferred scope

This milestone does not implement alternate-screen rendering, constrained row/column layouts, scroll views, a multiline editor, Markdown or syntax highlighting, terminal image protocols, a theme system, mouse support, autocomplete, nested focus traversal, overlay scrolling, clipboard integration, or agent/model functionality.
