# Bordered prompt and message components

**Date:** 2026-08-15
**Status:** Approved and implemented

## Goal

Give the moh conversation a quiet, terminal-native visual hierarchy without
adding role captions. The prompt input and user messages share one dim-gray
bordered surface. Assistant messages remain open and unboxed, so the role is
communicated by treatment rather than labels such as `you`, `moh`, `user`, or
`assistant`.

## Current boundary

The TUI already composes width-bounded components vertically. `Input` currently
renders one unframed, horizontally scrolling line, while `Text` renders
ANSI-aware wrapped text. The demo stores the transcript in a `Container`, adds
plain `Text` entries for submitted user text and completed assistant text, and
uses a temporary `Text` component for streamed assistant output.

The existing renderer validates component line widths, preserves ANSI state at
wrap boundaries, and locates the input cursor through its internal zero-width
marker. The new visual treatment must preserve those contracts, including
normal terminal scrollback and responsive streaming updates.

## Chosen approach

Add semantic `UserMessage` and `AiMessage` components backed by a small shared
message renderer, and extend `Input` with the same bordered-surface layout.

The shared border primitive will own the geometry and dim-gray border styling.
`Input` will use it for a three-row single-line editor. `UserMessage` will use
it for a multi-line, width-aware message body. `AiMessage` will use the
existing ANSI-aware wrapping path without a border. The message components
will keep width-keyed render caches and expose `set_text`/`text` operations
where the demo needs to update streamed state; `Input` will preserve its
existing editing and scroll state.

This keeps the visual behavior reusable for callers while avoiding a broad
theme system or an unrelated rewrite of the renderer.

## Visual contract

At ordinary terminal widths, the prompt and user messages use this structure:

```text
╭────────────────────────────────────────╮
│ ❯  Write a message                     │
╰────────────────────────────────────────╯

╭────────────────────────────────────────╮
│ This is a previous user message.       │
╰────────────────────────────────────────╯
```

The top and bottom borders, corners, and side rails use the terminal's dim
attribute (`ESC[2m`) and are reset on every rendered line. The prompt/value
content remains normal text, with the existing reverse-video cursor cell when
focused. User message text has no role caption or label.

Assistant output stays unframed:

```text
The assistant response remains open and content-focused.
Longer responses wrap naturally at the terminal width.
```

The component must degrade gracefully at narrow widths. The interior width is
the available terminal width minus the frame's horizontal padding and rails.
When that space is small, the border remains width-safe and the content wraps
or scrolls within the remaining cells; no rendered line may exceed the
component width.

## Components and responsibilities

### Shared bordered surface

Add a private reusable layout helper in the components module (or a focused
message/layout module if that better fits the existing module boundaries). It
will:

- calculate the interior width from the requested terminal width;
- produce dim-gray top, content, and bottom lines with exact display width;
- preserve the caller's content ANSI state and close it before drawing the
  dim-gray rail;
- reset terminal styling at the end of every line; and
- handle the smallest valid widths without panicking or returning an
  over-wide line.

The helper is an implementation detail. Callers use `Input`, `UserMessage`, or
`AiMessage`, not border primitives directly.

### `Input`

Keep the existing plain-text prompt, value editing, sanitization, grapheme
navigation, cursor marker, and horizontal scrolling behavior. Change only its
rendered layout to a three-line bordered surface:

1. dim-gray top border;
2. bordered content row containing the prompt, horizontally scrolled value,
   and the existing cursor affordance;
3. dim-gray bottom border.

The component continues to emit at most one cursor marker. The cursor position
must refer to the content row, so the TUI's existing cursor discovery and
renderer positioning continue to work after the input grows vertically. When a
non-zero terminal width leaves no physical interior cell inside the frame, a
focused input preserves the zero-width cursor marker but omits the visible
reverse-video cursor cell; the visible cursor cell is retained whenever an
interior cell exists.

### `UserMessage`

Add a public non-focusable component for committed user text. It stores the
display text, wraps it to the bordered interior width without splitting
graphemes or leaking ANSI state, and renders the complete bordered surface.
It exposes:

- `new(text)`;
- `set_text(text)` with cache invalidation; and
- `text()` for inspection.

The demo passes text that has already crossed the existing plain-text
sanitization boundary. The component itself does not introduce a second
untrusted-input policy or accept role labels.

### `AiMessage`

Add a public non-focusable component for assistant text. It stores and wraps
assistant content using the existing open `Text` presentation, with no role
caption, border, or artificial decoration. It exposes the same `new`,
`set_text`, and `text` shape as `UserMessage`, allowing the temporary streamed
assistant component to be updated without changing its concrete type.

An empty `AiMessage` renders no rows, preserving the current live-response
clear behavior. Completed responses are appended to the transcript as
`AiMessage`; provider errors continue to use the existing error path but are
rendered through the assistant message treatment after sanitization.

## Demo integration

Update the demo's component construction and transcript mutations:

- build the live response as an `AiMessage` instead of `Text`;
- append submitted prompts as `UserMessage`;
- append completed assistant responses as `AiMessage`;
- update the live response through `AiMessage::set_text` during streaming;
- clear the live response by setting its text to empty; and
- keep status, help overlay, input focus, request cancellation, and
  transactional conversation behavior unchanged.

The root component order remains transcript, live assistant response, input,
and status. The additional input border rows are part of normal layout and do
not change the status semantics or cursor ownership.

## Error handling and safety

- Zero-width render requests continue to return `InvalidLayoutWidth`.
- Narrow but non-zero widths must produce width-safe output or the existing
  typed rendering error; no string concatenation may silently exceed the
  assigned width.
- All ANSI styling introduced by the components must terminate before the
  next line's content and at line end.
- Existing sanitization remains in place for submitted prompts, streamed
  assistant deltas, final assistant responses, provider errors, and paths.
- No message component emits a cursor marker.

## Testing

Add focused component tests for:

- exact bordered input geometry and dim-gray styling;
- cursor marker placement inside the bordered input content row;
- prompt horizontal scrolling and editing behavior with the reduced interior
  width;
- user message borders, wrapping, Unicode display width, empty state, and
  narrow terminal widths;
- assistant message open rendering, wrapping, empty state, and cache
  invalidation;
- absence of role captions in all message output; and
- ANSI reset behavior at frame and content boundaries.

Update demo tests to verify that submitted user text and streamed/completed
assistant text use their intended presentation while status ordering,
sanitization, help, resize, and cancellation behavior remain unchanged.

Run the repository validation sequence before claiming completion:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --locked
git diff --check
```

## Scope exclusions

This change does not add Markdown rendering, syntax highlighting, alternate
screen layout, right-aligned chat bubbles, a configurable theme system,
animations, mouse behavior, or new conversation/provider behavior.
