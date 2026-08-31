# Prompt Input Shortcuts and Status Line Design

## Goal

Improve moh's interactive prompt with word-wise editing shortcuts and a compact status line that identifies the active model, request state, and current working directory.

The prompt remains a single-line, grapheme-aware editor. The change is limited to the existing TUI input component and the demo's presentation/lifecycle code; it does not introduce a theme system, nested focus traversal, or provider changes.

## Current context

The TUI already normalizes crossterm events into InputEvent, routes events to the focused root component, and renders root components vertically. Input already supports Unicode-grapheme insertion, plain Left/Right, Home/End, Delete, Backspace, paste, Enter submission, horizontal scrolling, and an internal cursor marker.

The demo currently creates a transcript, an empty status Text, and a focused Input. The status is updated to thinking..., ready, or error during the asynchronous conversation loop. The existing renderer supports ANSI-aware Text styling and cursor placement within any managed line.

## Chosen approach

Extend the existing Input component with whitespace-delimited word movement and deletion. Keep the status as the existing Text root component, but place it after the input and give it a small ANSI-styled formatter in the demo. Use the existing MODEL constant for the model label and capture/sanitize the cwd once during demo construction.

This is preferred over a new composite prompt component because root containers are intentionally presentational and nested focus traversal is outside the current TUI boundary. It is also preferred over a general styling API because the project has explicitly deferred a theme system and this feature needs one focused visual treatment.

## Interaction contract

### Existing editing behavior

- Plain Left and Right move one Unicode grapheme at a time.
- Plain Backspace removes the grapheme immediately before the cursor.
- Plain Delete removes the grapheme immediately at the cursor.
- Home and End move to the beginning and end of the input.
- Enter submits and clears the current value.
- Alt-modified editing remains ignored.
- Existing paste normalization and terminal-control sanitization remain unchanged.

### New word shortcuts

Word boundaries are whitespace-delimited. A grapheme is whitespace when all of its scalar values are Unicode whitespace; every other grapheme belongs to a word run. Punctuation remains attached to the neighboring non-whitespace run.

- Ctrl+Left skips whitespace to the left, then skips the preceding non-whitespace run, stopping at that run's beginning.
- Ctrl+Right skips whitespace at the cursor when present, then the next non-whitespace run and its following whitespace, stopping at the next word's beginning or the input end.
- Ctrl+Backspace removes the whitespace immediately before the cursor and the preceding non-whitespace run. At the beginning it is consumed without changing the value.
  When the cursor is inside a non-whitespace run, it removes only that run's prefix and leaves any preceding separator intact.
- Ctrl+Delete removes the non-whitespace run at the cursor and following whitespace. If the cursor is in whitespace, it removes that whitespace, the next non-whitespace run, and its following whitespace. At the end it is consumed without changing the value.

Word operations use the same grapheme boundaries as ordinary editing, so they never split combining sequences, emoji sequences, or wide graphemes. A recognized operation at a boundary returns InputOutcome::Consumed; a movement or deletion that changes the cursor/value returns InputOutcome::Changed.

The normalized InputEvent and crossterm mapping already carry the required Control modifier and editing key variants, so no new event type is needed.

## Visual design

The demo's managed root components will render in this order:

```text
transcript
❯ prompt text and cursor
╰─ gpt-5.6-luna · ready · /current/working/directory
```

While a request is active, the bottom line becomes:

```text
╰─ gpt-5.6-luna · thinking... · /current/working/directory
```

On a failed request, the state becomes error. The status formatter uses existing ANSI-aware Text rendering: the prefix and cwd are dim, the model is cyan, ready is green, thinking... is yellow, and error is red. The prompt itself uses the plain Unicode ❯ marker so it remains safe under Input's plain-text prompt contract; the existing reverse-video cursor cell supplies the active editing affordance.

The cwd is read during build, converted with to_string_lossy, and passed through Input::sanitize_plain_text before being placed in the status line. The status line continues to use the existing width-bounded Text wrapping behavior on narrow terminals rather than adding a second truncation/layout system.

The introductory transcript copy will no longer duplicate the model label. The help overlay will document the new word shortcuts alongside Enter, Ctrl+O, Ctrl+C, and Escape.

## Status lifecycle and data flow

1. build obtains the current working directory, creates the transcript, focused Input::new("❯ "), and status Text, and inserts them in transcript/input/status order.
2. The initial status is gpt-5.6-luna · ready · <cwd>.
3. Submission appends the user message, starts the conversation turn, and changes the status state to thinking....
4. A successful completion appends the sanitized assistant response and changes the state to ready.
5. A provider/conversation failure appends the sanitized error and changes the state to error.
6. Every status mutation requests a render. The renderer continues to locate the cursor marker on the input line even though the status line follows it.

The model label comes from codex_provider::MODEL, so the status cannot drift from the provider's configured model. No request payload or conversation state changes are part of this feature.

## Robustness and safety

- Cwd text is sanitized before entering ANSI-aware Text rendering.
- Model/state labels are fixed application strings; assistant/error text continues through the existing plain-text sanitizer before transcript rendering.
- A failure to read the cwd is mapped to the existing RenderError::Io result path during demo construction rather than silently showing a misleading path.
- Ctrl+C remains an application-global exit shortcut and must continue to break the event loop before another pending request poll.
- Pending request cleanup, terminal restoration, and provider authentication behavior are unchanged.

## Testing

Add focused component tests for:

- Ctrl+Left/Right from the beginning, middle, end, and across repeated whitespace;
- Ctrl+Backspace/Delete with leading/trailing whitespace, punctuation, empty input, and Unicode grapheme runs;
- boundary outcomes returning Consumed and successful edits returning Changed;
- plain editing continuing to operate by grapheme rather than word.

Extend demo tests to verify:

- the rendered root order places the input before the status line;
- the initial status contains the model, ready, and the test process cwd;
- submission changes the status to thinking... before completion;
- success returns to ready and failure returns to error while preserving the status layout;
- help copy includes the new shortcuts.

Run the existing project validation sequence after implementation:

```text
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --locked
git diff --check
```

## Scope exclusions

This feature does not add command history, multiline editing, autocomplete, mouse support, clipboard behavior beyond the existing bracketed paste path, a reusable theme API, status truncation rules beyond current Text wrapping, streaming model output, or any change to Codex authentication/provider transport.
