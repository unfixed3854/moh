# Fullscreen Ratatui Migration Design

## Goal

Replace Moh's retained, main-screen, Pi-like terminal renderer with a fullscreen
Ratatui client for [issue #27](https://github.com/unfixed3854/moh/issues/27).
Moh will enter the alternate screen, own the full terminal viewport while it is
running, keep transcript scrolling inside the application, and restore the
user's original terminal screen on exit.

The migration should preserve Moh's current product behavior and visual
language where those choices remain useful. When Ratatui provides the relevant
layout or widget primitive, prefer it over a compatibility implementation even
when that produces small visual or interaction changes. The result should have
one UI model, not Ratatui hidden behind the old component API.

## Context

The terminal client currently projects backend session state into a public
`moh::tui` library. That library owns retained components, opaque component
identifiers, ANSI-aware text layout, cursor markers, overlay composition,
terminal lifecycle, and a custom differential renderer designed to preserve
normal terminal scrollback.

The custom public API has no in-repository consumers other than Moh's terminal
client and its tests. Ratatui already owns fullscreen layout, cell buffers,
frame diffing, resize handling, cursor placement, and standard widgets. Keeping
the current API as an adapter would preserve the main maintenance burden this
migration is intended to remove.

The long-lived backend and session protocol are outside this rendering change.
The backend remains authoritative for conversation history, the active run,
jobs, model settings, and persistence. Detaching the terminal client must not
cancel backend work.

## Decisions

- Use Ratatui's fullscreen viewport and the terminal alternate screen.
- Remove the custom public `moh::tui` API rather than provide compatibility
  adapters.
- Keep the existing client-server session boundary and command behavior.
- Keep current visual styling where convenient, but use Ratatui widgets instead
  of reproducing custom component implementations.
- Support internal transcript scrolling with keyboard and mouse wheel.
- Mouse support is wheel-only for this issue. Click, hover, drag, and selection
  behavior are deferred.
- Keep Moh's single-line prompt editor because Ratatui does not provide one.
- Keep `pulldown-cmark`, but convert Markdown into Ratatui styled text instead
  of ANSI strings.
- Use one complete Ratatui frame per draw. Ratatui, not application code, owns
  cell diffing and terminal writes.

## Non-goals

This migration does not add:

- a visual redesign or theme system;
- a multiline prompt editor;
- mouse click, drag, hover, or text-selection behavior;
- new slash commands or shortcuts;
- a session-management UI;
- backend, RPC, persistence, provider, or tool behavior changes;
- public replacement APIs for downstream users of the removed custom TUI
  library;
- third-party Ratatui widget crates unless implementation reveals a concrete
  need that is separately reviewed.

## Architecture

`src/client/app.rs` remains responsible for the attached session client,
authoritative `SessionSnapshot`, command resolution, backend requests, and
projection of session updates.

A new private `src/client/ui/` module owns presentation:

- `mod.rs` defines transient `UiState` and coordinates event handling;
- `editor.rs` contains the single-line prompt editor;
- `markdown.rs` converts supported Markdown into Ratatui `Text`, `Line`, and
  `Span` values;
- `view.rs` lays out and renders a complete frame;

`src/client/terminal.rs` owns production setup, input polling, and cleanup
around Ratatui's default Crossterm terminal. Session semantics stay in
`client::app`; presentation state and rendering stay private to `client::ui`;
terminal mode management contains no application behavior.

`UiState` contains only state that cannot be derived from the session snapshot:

- prompt contents and cursor position;
- transcript scroll offset and auto-follow mode;
- active command, model, effort, or process selection;
- help and selector visibility;
- sanitized local notices produced by client-side command or RPC failures, plus
  their presentation-only error-status override;
- the last measured transcript viewport needed to calculate page steps and
  clamp scrolling;
- whether another frame is required.

Model, effort, context usage, jobs, transcript entries, active response, busy
state, and backend-reported errors remain derived from the current
`SessionSnapshot`. The UI must not create a second semantic session model.
Local notices are cleared by an authoritative snapshot replacement and their
status override is cleared by the next run or authoritative status-changing
event.

The event flow is:

1. Receive a Crossterm input event or a typed backend session update.
2. Reduce it into a transient UI change or existing application action.
3. Apply backend events to the authoritative snapshot through the existing
   projection path.
4. Draw a full Ratatui frame when visible state changed.
5. Let Ratatui diff its current and previous cell buffers and flush only the
   changed cells.

The custom component IDs, type downcasts, retained logical-line frame,
application-owned differential plans, ANSI overlay compositor, and cursor
marker disappear.

## Fullscreen Layout

Every normal frame is divided vertically relative to `frame.area()`:

1. A borderless transcript viewport receives all remaining rows.
2. A single-line prompt is pinned above the bottom row.
3. The compact model, effort, context, status, process count, and working
   directory line is pinned to the bottom row.

The normal layout requires at least 20 columns and 3 rows. Smaller nonzero
frames render a clipped, plain `terminal too small` message over the available
area and do not attempt the normal layout or popup placement. A later resize
back above the threshold returns to the full UI without losing editor or scroll
state.

The transcript uses a wrapped Ratatui `Paragraph` over styled `Text`. A vertical
Ratatui `Scrollbar` is rendered at the right edge only when the wrapped content
exceeds the transcript viewport. The scrollbar consumes its own column so it
does not overwrite message text.

The prompt remains one line. It uses styled Ratatui spans for the cyan prompt
marker and the sanitized editor value, horizontally windows long values around
the editor cursor, and requests the real hardware cursor with
`Frame::set_cursor_position`.

The status remains one compact dim line with the existing semantic segments.
It is truncated safely by Ratatui when the terminal is narrow enough to fit the
normal layout but not the entire status string.

## Transcript Scrolling

The transcript starts in auto-follow mode. During every draw in this mode, its
top offset is set to the greatest valid offset so the newest wrapped content is
visible.

The controls are:

- `PageUp`: subtract `max(viewport_height - 1, 1)` rows from the top offset;
- `PageDown`: add `max(viewport_height - 1, 1)` rows to the top offset;
- mouse wheel up or down: move three rows per wheel step in the corresponding
  direction;
- `End`, when routed to transcript scrolling as described under Prompt Editor:
  jump to the greatest valid top offset and enable auto-follow.

`PageUp`, `PageDown`, and a mouse-wheel event disable auto-follow. While
auto-follow is disabled, appended or streamed content leaves the current top
offset unchanged, then clamps it only if the content becomes shorter. This
prevents new output from pulling a reader away from older content. Reaching the
bottom through `PageDown` or the wheel does not implicitly resume auto-follow;
only `End` does.

Resizing rewraps the transcript at the actual frame width. In auto-follow mode
the new bottom remains visible. In manual mode the previous top offset is
retained and clamped to the new valid range. This is a row-level anchor rather
than a semantic message anchor and is deliberately sufficient for this
migration.

Wheel capture is enabled for the terminal session. Other mouse events are
ignored and do not change focus, selection, or application state.

## Widgets and Presentation

Use Ratatui primitives directly:

- `Layout` and `Constraint` for the root frame and popup geometry;
- `Text`, `Line`, `Span`, and `Style` for semantic content;
- `Paragraph` for the transcript, prompt, status, help, and simple messages;
- `Block` for the user-message rail and popup borders;
- `List` with `ListState` for command, model, effort, and process selectors;
- `Clear` before popup widgets;
- `Scrollbar` with `ScrollbarState` for transcript position.

The old `Container`, `Text`, `Spacer`, `Suggestions`, `UserMessage`,
`AiMessage`, surface, overlay, renderer, and terminal output abstractions are
not reimplemented.

User messages keep a subtle left accent rail using a left-bordered Ratatui
`Block`. Assistant messages remain open text. Spacing between transcript items
is produced while constructing the transcript's semantic lines instead of
through spacer components.

Help renders as a centered bordered popup. Selector lists render above the
prompt and are clamped to the available frame. Popup width is limited to the
frame width and popup height to the available rows above the prompt. Existing
keyboard precedence remains: global exit/help shortcuts first, an open popup
next, command-menu handling next, and prompt editing last. Escape closes the
top transient UI or cancels an active request according to the current
behavior.

## Prompt Editor

Ratatui has no built-in text input widget, so Moh retains a small presentation
state object for the existing single-line editor behavior. It supports:

- grapheme-safe insertion, deletion, and cursor movement;
- Home and End within the editor when no transcript-scroll handling consumes
  the event;
- Ctrl+Left and Ctrl+Right word movement;
- Ctrl+Backspace and Ctrl+Delete word deletion;
- bracketed paste sanitized into one line;
- submission on Enter;
- current Tab, Up, and Down interaction with suggestion lists.

There is one `End` conflict. When no popup is open and the prompt cursor is not
at the end, plain `End` moves the editor cursor to the end. When no popup is
open and the prompt cursor is already at the end, plain `End` jumps the
transcript to the bottom and enables auto-follow. An open popup gets input
precedence and may consume or ignore `End` without changing transcript scroll.
This preserves editor behavior while keeping the approved scroll shortcut
reachable. Tests must cover all three outcomes.

The editor produces display-safe text and a cursor column; it does not implement
a Ratatui widget trait or write terminal bytes. The view renders it with a
standard `Paragraph` and sets the frame cursor.

## Markdown and Untrusted Text

Keep `pulldown-cmark` and the currently supported Markdown constructs, but map
them to structured Ratatui spans and modifiers:

- headings and strong text use bold;
- emphasis uses italic;
- strikethrough uses crossed-out text;
- inline and fenced code retain the current code color and dim language label;
- lists, task markers, block quotes, rules, and line breaks retain their
  current textual structure;
- links render as underlined labels, followed by a dim visible destination when
  the label alone does not expose it.

Ratatui cells replace raw ANSI output. OSC-8 clickable hyperlink escapes are
therefore not preserved in this migration. The visible destination keeps links
usable without reintroducing an escape-aware rendering path.

All backend- and user-supplied text remains sanitized before display. Control
characters, terminal escape characters, and the old internal cursor-marker
sequence are removed or normalized. Newlines and tabs are retained only in
contexts that deliberately support them, such as Markdown transcript content;
the prompt, status fields, menu labels, paths, and error summaries remain
single-line plain text.

## Terminal Lifecycle

Production uses Ratatui's fallible fullscreen initialization and default
Crossterm backend. Ratatui owns raw mode, alternate-screen entry, its terminal
buffers, cursor restoration, and panic restoration.

A small private Moh guard adds the modes Ratatui does not own for this client:
bracketed paste and Crossterm mouse capture. Setup is tracked step by step. If a
later setup operation fails, all completed earlier operations are rolled back
before returning the error.

Normal teardown occurs in this order:

1. Disable mouse capture if it was enabled.
2. Disable bracketed paste if it was enabled.
3. Ask Ratatui to restore the terminal.

The guard provides explicit fallible restoration for normal returns and a
best-effort `Drop` fallback. A chained panic hook attempts to disable Moh's two
extra modes before delegating to Ratatui's installed restoration hook. The
private mode-operation seam is injectable in tests but is not a general
terminal abstraction.

Normal exit, `/quit`, Ctrl+C, backend disconnect, session projection failure,
input failure, and draw failure all use this teardown path. Application or
session errors are displayed only after restoration so diagnostics appear on
the user's original screen. If application work and restoration both fail,
the application error stays primary and includes cleanup context. If only
restoration fails, restoration is the returned error.

Detaching or losing the terminal client does not cancel an active backend run.
Explicit cancellation continues to use the existing command and RPC path.

## Error Handling

The custom `RenderError` variants tied to component identifiers, logical line
width, cursor markers, overlays, or custom frame invariants are removed.

The client error model retains session and projection failures and adds or
uses focused terminal I/O/setup/cleanup context. Pure view construction should
not return errors. Ratatui draw errors propagate through the terminal error
variant and trigger restoration.

A zero-sized or unavailable terminal is an initialization/draw error. A small
but nonzero terminal is a supported state and renders the minimal frame
described above. Invalid popup geometry is clamped rather than treated as an
error.

## Dependencies and Deletions

Add `ratatui = "0.30.2"` with Crossterm support. Keep the direct `crossterm`
dependency for event input, bracketed paste, and mouse capture.

Remove `vte` after ANSI tokenization is deleted. Remove the `vt100` development
dependency after raw-terminal renderer tests are replaced by Ratatui
`TestBackend` assertions. Retain `unicode-segmentation`, `unicode-width`, and
`pulldown-cmark` for prompt editing, cursor measurement, sanitization, and
Markdown conversion unless implementation proves one is no longer used.

Delete the public `src/tui/` module and remove `pub mod tui` from `src/lib.rs`.
Delete obsolete integration tests whose subject is the removed renderer,
component framework, ANSI layout, overlay compositor, or terminal-output
abstraction. Port product-level assertions to the client UI tests before
deleting their old form.

## Testing

Implementation follows test-driven development. Each migrated behavior first
gets a failing test at the new boundary.

### Pure state and editor tests

Cover:

- grapheme-safe prompt editing and horizontal cursor windowing;
- word navigation and deletion shortcuts;
- paste and control-character sanitization;
- command/menu input precedence and selection;
- page-size calculation at zero-, one-, and multi-row viewport heights;
- PageUp, PageDown, three-row wheel steps, clamping, and End restoration;
- auto-follow at the bottom;
- stable manual offset while streamed content arrives;
- resize behavior in auto-follow and manual modes;
- ignored click, move, drag, and mouse-button events.

### Markdown tests

Assert semantic Ratatui text, spans, styles, and visible link destinations for
the supported Markdown syntax. Assert that untrusted escape/control input
cannot become terminal control output.

### Frame tests

Render through `ratatui::backend::TestBackend` and assert visible cells and
styles for:

- initial ready, thinking, and error states;
- the pinned transcript, prompt, and status regions;
- user rails and assistant Markdown;
- live streamed response updates;
- long transcript wrapping and overflow scrollbar;
- keyboard and mouse scroll positions;
- hardware cursor placement and long prompt windowing;
- help, command, model, effort, and process popups;
- popup clamping;
- narrow and short normal layouts;
- the under-minimum terminal frame;
- terminal resize and reflow.

Assertions should target observable cells, styles, state, and cursor position,
not Ratatui's emitted Crossterm bytes or internal diff algorithm.

### Lifecycle and integration tests

Use injected mode operations to prove partial-startup rollback and teardown
ordering for mouse, paste, and Ratatui restoration failures. Preserve existing
session-client tests for submission, cancellation, settings, job commands,
backend events, detach behavior, and projection validation while replacing
their custom-TUI fixtures.

Manual acceptance in a real pseudo-terminal verifies:

- alternate-screen entry and restoration of the original screen;
- prompt editing and submission;
- PageUp, PageDown, End, and mouse-wheel transcript scrolling;
- streaming at the bottom and while manually scrolled;
- help and selector interaction;
- resizing and the minimum-size view;
- normal exit and representative error restoration.

## Acceptance Criteria

The migration is complete when:

- running Moh uses a Ratatui fullscreen alternate-screen client;
- the custom public renderer/component API and its obsolete dependencies are
  gone;
- the transcript is internally scrollable by page keys and mouse wheel;
- manual scrolling is stable during streaming and End resumes auto-follow;
- current prompt, command, help, selector, status, session, and detach behavior
  remains available subject to the approved widget and link simplifications;
- Ratatui owns layout, cells, frame diffing, resizing, and standard widgets;
- terminal state is restored after normal exit, recoverable failures, and
  panics on the supported unwind path;
- frame, state, lifecycle, and existing application tests pass;
- real-terminal acceptance shows correct fullscreen behavior and restoration;
- `cargo fmt --all -- --check` passes;
- `cargo clippy --all-targets --all-features -- -D warnings` passes;
- `cargo test --all-targets` passes;
- `cargo build --locked` passes;
- `git diff --check` reports no whitespace errors.
