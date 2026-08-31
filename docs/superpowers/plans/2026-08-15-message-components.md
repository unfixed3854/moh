# Bordered prompt and message components Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a dim-gray bordered prompt and bordered user-message components while keeping assistant messages open, unboxed, and label-free.

**Architecture:** Add a private bordered-surface helper shared by `Input` and a new `UserMessage` component. Add semantic `UserMessage` and `AiMessage` components with width-keyed caches and `set_text`/`text` accessors. Update the demo to use those components while preserving the existing retained renderer, streaming lifecycle, sanitization, cursor ownership, and transactional conversation behavior.

**Tech Stack:** Rust 2024, the existing `Component` trait, ANSI-aware `wrap_ansi`/`visible_width`, Crossterm-compatible SGR controls, Tokio test harness, and the repository's existing integration/unit tests.

## Global Constraints

- The prompt input and user messages share one dim-gray bordered surface.
- Assistant messages remain open and unboxed, with no role captions, labels, or artificial decoration.
- No message component emits a cursor marker.
- Preserve the existing plain-text sanitization boundary for submitted prompts, streamed assistant deltas, final responses, provider errors, and paths.
- Preserve normal main-screen scrollback, responsive streaming updates, status semantics, help overlays, input focus, request cancellation, and transactional conversation behavior.
- Zero-width rendering returns `RenderError::InvalidLayoutWidth`.
- Every component line must stay within the requested terminal width, including narrow non-zero widths.
- ANSI styling must terminate before the next line's content and at line end.
- Do not add Markdown, syntax highlighting, alternate-screen layout, right-aligned chat bubbles, theme configuration, animations, mouse behavior, or provider/conversation behavior.
- Validate with `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-targets`, `cargo build --locked`, and `git diff --check`.

---

## File map

- Create `src/tui/components/surface.rs`: private width calculation and dim-gray bordered-line rendering shared by the input and user message.
- Create `src/tui/components/message.rs`: public `UserMessage` and `AiMessage` components with text state, cache invalidation, and rendering.
- Modify `src/tui/components/mod.rs`: register the private surface/message modules and export the two public message components.
- Modify `src/tui/components/input.rs`: render the existing editor inside the shared three-row bordered surface while retaining all editing behavior.
- Modify `tests/components.rs`: cover bordered surfaces, user/assistant message rendering, input cursor placement, narrow widths, and changed input geometry.
- Modify `src/demo.rs`: replace transcript/live-response `Text` components with `UserMessage`/`AiMessage` and remove role captions from rendered output.
- Modify the existing test module in `src/demo.rs`: update output assertions and add message-style integration assertions.

## Task 1: Add the shared bordered surface and semantic message components

**Files:**

- Create: `src/tui/components/surface.rs`
- Create: `src/tui/components/message.rs`
- Modify: `src/tui/components/mod.rs`
- Test: `tests/components.rs`

**Interfaces:**

- Produces public `UserMessage` and `AiMessage`, both implementing `Component`.
- Both message types expose:

```rust
pub fn new(text: impl Into<String>) -> Self;
pub fn set_text(&mut self, text: impl Into<String>);
pub fn text(&self) -> &str;
```

- Produces private surface helpers callable by sibling component modules:

```rust
pub(super) fn bordered_content_width(width: usize) -> Result<usize>;
pub(super) fn render_bordered(width: usize, content: &[String]) -> Result<Vec<String>>;
```

### Step 1: Write failing component tests for the public message API

Extend the import at the top of `tests/components.rs`:

```rust
use moh::tui::components::{AiMessage, Container, Input, Spacer, Text, UserMessage};
```

Add tests with these exact behaviors:

```rust
#[test]
fn user_message_renders_with_a_dim_bordered_surface_and_no_role_label() {
    let mut message = UserMessage::new("previous prompt");

    let lines = message.render(24).unwrap();

    assert_eq!(lines.len(), 3);
    assert!(lines[0].contains("╭"));
    assert!(lines[1].contains("previous prompt"));
    assert!(lines[2].contains("╰"));
    assert!(!lines.iter().any(|line| line.contains("user")));
    for line in lines {
        assert_eq!(moh::tui::text::visible_width(&line), 24);
    }
}

#[test]
fn assistant_message_is_open_and_empty_messages_render_no_rows() {
    let mut message = AiMessage::new("answer");
    assert_eq!(message.render(24).unwrap(), vec!["answer"]);

    message.set_text("");
    assert!(message.render(24).unwrap().is_empty());
}

#[test]
fn message_set_text_invalidates_the_width_keyed_render_cache() {
    let mut message = AiMessage::new("old");
    assert_eq!(message.render(8).unwrap(), vec!["old"]);

    message.set_text("new");
    assert_eq!(message.text(), "new");
    assert_eq!(message.render(8).unwrap(), vec!["new"]);
}

#[test]
fn user_message_wraps_without_exceeding_the_requested_width() {
    let mut message = UserMessage::new("alpha beta gamma");

    let lines = message.render(12).unwrap();

    assert!(lines.len() > 3);
    assert!(lines
        .iter()
        .all(|line| moh::tui::text::visible_width(line) <= 12));
}

#[test]
fn messages_reject_zero_width_and_reset_ansi_at_frame_boundaries() {
    let mut user = UserMessage::new("\x1b[31mred\x1b[0m");
    assert_eq!(
        user.render(0).unwrap_err().to_string(),
        "layout width must be greater than zero"
    );

    let lines = user.render(12).unwrap();
    assert!(lines.iter().all(|line| line.ends_with(moh::tui::LINE_RESET)));
}
```

The open assistant assertion establishes that `AiMessage` does not add a role
prefix or border. The user-message assertions verify the visual contract
without depending on exact ANSI escape-sequence bytes.

### Step 2: Run the focused tests and verify they fail

Run:

```bash
cargo test --test components user_message_renders_with_a_dim_bordered_surface_and_no_role_label -- --exact
cargo test --test components assistant_message_is_open_and_empty_messages_render_no_rows -- --exact
cargo test --test components message_set_text_invalidates_the_width_keyed_render_cache -- --exact
```

Expected: compilation fails because `AiMessage`, `UserMessage`, and the new
component module do not exist yet.

### Step 3: Register the new private and public modules

Update `src/tui/components/mod.rs` to keep the existing module list and add:

```rust
mod message;
mod surface;

pub use message::{AiMessage, UserMessage};
```

Keep `surface` private. Keep the existing `Container`, `Input`, `Spacer`, and
`Text` exports unchanged.

### Step 4: Implement the shared bordered-surface helper

In `src/tui/components/surface.rs`, use the existing `LINE_RESET`,
`RenderError`, `Result`, and `visible_width` APIs. Define:

```rust
const DIM: &str = "\x1b[2m";
const LEFT_RAIL: &str = "│ ";
const RIGHT_RAIL: &str = " │";
```

Implement `bordered_content_width` as follows:

```rust
pub(super) fn bordered_content_width(width: usize) -> Result<usize> {
    if width == 0 {
        return Err(RenderError::InvalidLayoutWidth);
    }
    Ok(width.saturating_sub(
        visible_width(LEFT_RAIL) + visible_width(RIGHT_RAIL),
    ))
}
```

Use display-cell widths, not byte lengths, for all layout arithmetic. For
ordinary widths, the frame has two cells on each horizontal side. For widths
1-3, emit a compact width-safe border and use the available interior cells;
the helper must never construct a string wider than `width`.

Implement `render_bordered(width, content)` with this behavior:

1. Reject `width == 0` with `InvalidLayoutWidth`.
2. Render a dim `╭──╮` top border and dim `╰──╯` bottom border, repeating `─`
   until the exact requested display width is reached.
3. For each content line, measure visible width, append spaces to fill the
   interior, and draw dim side rails.
4. Append `LINE_RESET` after content before drawing a dim right rail, so
   content SGR state cannot color the border.
5. Add `LINE_RESET` at the end of every emitted line.
6. For a zero-cell interior, emit only compact rails and do not panic.

Do not use `String::len()` to decide whether a content line fits; use
`visible_width`. The caller is responsible for wrapping content to the value
returned by `bordered_content_width`.

### Step 5: Implement `UserMessage` and `AiMessage`

In `src/tui/components/message.rs`, define each component with the same state
shape as `Text`:

```rust
pub struct UserMessage {
    text: String,
    source_revision: u64,
    cache: Option<(u16, u64, Vec<String>)>,
}

pub struct AiMessage {
    text: String,
    source_revision: u64,
    cache: Option<(u16, u64, Vec<String>)>,
}
```

For both types, `new` stores the supplied string and starts revision/caches at
zero. `set_text` replaces the string, increments the revision with
`wrapping_add(1)`, and leaves the old cache unavailable through the revision
key. `text` returns `&str`.

Implement `UserMessage::render` as:

```rust
if self.text.is_empty() {
    return Ok(Vec::new());
}
let inner_width = bordered_content_width(usize::from(width))?;
let body = wrap_ansi(&self.text, inner_width.max(1))?;
let lines = render_bordered(usize::from(width), &body)?;
```

Cache the final `lines` by `(width, source_revision)`. For a zero-width
request, let `bordered_content_width` return `InvalidLayoutWidth` before any
wrapping occurs. For tiny widths, retain the helper's compact border behavior.

Implement `AiMessage::render` by returning empty output for empty text and
otherwise caching `wrap_ansi(&self.text, usize::from(width))?`. This preserves
the existing open `Text` behavior and ANSI-aware wrapping.

### Step 6: Run the focused component tests and commit

Run:

```bash
cargo test --test components user_message_renders_with_a_dim_bordered_surface_and_no_role_label -- --exact
cargo test --test components assistant_message_is_open_and_empty_messages_render_no_rows -- --exact
cargo test --test components message_set_text_invalidates_the_width_keyed_render_cache -- --exact
cargo test --test components user_message_wraps_without_exceeding_the_requested_width -- --exact
cargo test --test components messages_reject_zero_width_and_reset_ansi_at_frame_boundaries -- --exact
cargo fmt --all -- --check
```

Expected: all new tests pass and formatting is clean. Commit the isolated
component change:

```bash
git add src/tui/components/mod.rs src/tui/components/surface.rs \
  src/tui/components/message.rs tests/components.rs
git commit -m "feat: add bordered message components"
```

## Task 2: Render the prompt input inside the shared border

**Files:**

- Modify: `src/tui/components/input.rs:220-270`
- Test: `tests/components.rs` existing input rendering tests near
  `focused_input_keeps_one_cursor_marker_within_every_narrow_width`

**Interfaces:**

- Consumes `surface::bordered_content_width` and `surface::render_bordered`.
- Preserves `Input::new`, `value`, `set_value`, `clear`, `handle_input`, and
  `set_focused` signatures.
- Produces a three-row bordered render for normal widths, with the cursor
  marker located on the middle content row.

### Step 1: Add failing input geometry tests

Add these tests to `tests/components.rs`:

```rust
#[test]
fn focused_input_places_the_cursor_marker_inside_the_bordered_content_row() {
    let mut input = Input::new("❯ ");
    input.set_focused(true);

    let lines = input.render(20).unwrap();

    assert_eq!(lines.len(), 3);
    assert!(!lines[0].contains(CURSOR_MARKER));
    assert!(lines[1].contains(CURSOR_MARKER));
    assert!(!lines[2].contains(CURSOR_MARKER));
    assert!(lines
        .iter()
        .all(|line| moh::tui::text::visible_width(line) <= 20));
}

#[test]
fn input_scrolls_inside_the_bordered_content_width() {
    let mut input = Input::new("❯ ");
    input.set_focused(true);
    input.set_value("0123456789abcdefghij");

    let lines = input.render(12).unwrap();

    assert_eq!(lines.len(), 3);
    assert!(lines[1].contains("j"));
    assert!(lines
        .iter()
        .all(|line| moh::tui::text::visible_width(line) <= 12));
}

#[test]
fn bordered_input_handles_the_smallest_non_zero_widths() {
    let mut input = Input::new("❯ ");

    for width in 1..=4 {
        let lines = input.render(width).unwrap();
        assert_eq!(lines.len(), 3);
        assert!(lines
            .iter()
            .all(|line| moh::tui::text::visible_width(line) <= usize::from(width)));
    }
}

#[test]
fn input_rejects_zero_width_before_building_a_frame() {
    let mut input = Input::new("❯ ");

    assert_eq!(
        input.render(0).unwrap_err().to_string(),
        "layout width must be greater than zero"
    );
}
```

Update existing exact line assertions to inspect `lines[1]` for prompt/value
content, because the top and bottom borders now occupy `lines[0]` and
`lines[2]`. Update the existing narrow-width cursor assertions to inspect the
middle row and retain exactly one cursor marker at every non-zero width. For
widths with no physical frame interior, assert the marker-only fallback; for
widths with an interior cell, retain the reverse-video cursor cell. Keep all
input editing, sanitization, and grapheme behaviors intact.

### Step 2: Run the focused input tests and verify they fail

Run:

```bash
cargo test --test components focused_input_places_the_cursor_marker_inside_the_bordered_content_row -- --exact
cargo test --test components input_scrolls_inside_the_bordered_content_width -- --exact
cargo test --test components bordered_input_handles_the_smallest_non_zero_widths -- --exact
cargo test --test components input_rejects_zero_width_before_building_a_frame -- --exact
```

Expected: the new geometry tests fail because `Input::render` still returns one
unframed line.

### Step 3: Adapt `Input::render` to the shared interior width

Import the private surface helpers in `src/tui/components/input.rs`:

```rust
use super::surface::{bordered_content_width, render_bordered};
```

Replace the current width arithmetic with the shared interior width:

```rust
let terminal_width = usize::from(width);
let content_width = bordered_content_width(terminal_width)?;
let prompt = slice_columns(&self.prompt, 0, content_width);
let available_value_width = content_width.saturating_sub(prompt.width);
```

When `available_value_width > 0`, preserve the existing cursor-column and
horizontal-scroll calculations, but use that reduced width. When the bordered
surface has no physical interior cell, skip horizontal scrolling and construct
the smallest possible focused content row containing the zero-width cursor
marker but no visible cursor cell; this is the documented marker-only fallback.
When at least one interior cell exists, reserve one cell for the existing
reverse-video `CURSOR_CELL`, suppressing prompt/value text as necessary so the
focused input still shows the cursor. An unfocused input may use an empty
content row when no interior cell exists. Preserve the existing `display_value`
and cursor marker behavior everywhere else.

Build the content row from the prompt slice and value slice, then call:

```rust
render_bordered(terminal_width, &[line])
```

The returned three rows place the cursor marker on row one. Do not add a second
cursor marker or move cursor logic into the border helper.

### Step 4: Run all component tests and commit

Run:

```bash
cargo test --test components
cargo fmt --all -- --check
```

Expected: all component tests pass, including existing input navigation,
sanitization, and container tests. Commit the input-only change:

```bash
git add src/tui/components/input.rs tests/components.rs
git commit -m "feat: frame the prompt input"
```

## Task 3: Integrate the new message components into the demo

**Files:**

- Modify: `src/demo.rs:1-350`
- Modify: the existing test module in `src/demo.rs`, especially
  `successful_request_appends_model_answer_and_returns_to_ready`,
  `successful_request_streams_intermediate_text`, and
  `backend_response_is_sanitized_before_terminal_rendering`

**Interfaces:**

- Consumes `components::{AiMessage, Container, Input, UserMessage}`.
- `DemoIds::live_response` continues to identify one mutable root component,
  but its concrete type changes from `Text` to `AiMessage`.
- `transcript` remains a `Container` and accepts both message component types
  through `push`.

### Step 1: Update demo assertions to describe the new visual contract

Change the existing output assertions that contain `you: hello` or `moh:` so
they assert content without role captions and check the frame distinction.
In `successful_request_appends_model_answer_and_returns_to_ready`, add:

```rust
assert!(output.contains("hello"));
assert!(!output.contains("you: hello"));
assert!(!output.contains("moh: "));
assert!(output.contains("╭"));
assert!(output.contains("╰"));
```

In `successful_request_streams_intermediate_text`, assert that the assistant
delta appears without an assistant label while the input border and status
line remain present. Keep the existing checks for `MODEL`, `thinking...`,
`ready`, error text, help, resize, and cancellation behavior.

### Step 2: Run the focused demo tests and verify they fail

Run:

```bash
cargo test demo::tests::successful_request_appends_model_answer_and_returns_to_ready -- --exact
cargo test demo::tests::successful_request_streams_intermediate_text -- --exact
```

Expected: the updated assertions fail because the demo still formats `you:` and
`moh:` prefixes and the input remains unframed.

### Step 3: Construct semantic message components in `build`

Update the import list in `src/demo.rs` to include `AiMessage` and
`UserMessage`. Change the root construction to:

```rust
let transcript = tui.add_component(transcript);
let live_response = tui.add_component(AiMessage::new(""));
let input = tui.add_component(Input::new("❯ "));
```

Leave root ordering and the focus call unchanged. Keep the introduction as a
plain `Text` element because it is application chrome, not a conversation
message.

### Step 4: Replace transcript and live-response formatting

Change `begin_request` to append submitted text without a role prefix:

```rust
tui.component_mut::<Container>(ids.transcript)?
    .push(UserMessage::new(text));
```

Change `apply_response` to append the sanitized answer as:

```rust
tui.component_mut::<Container>(ids.transcript)?
    .push(AiMessage::new(Input::sanitize_plain_text(&answer)));
```

Change `update_live_response` to update the concrete `AiMessage` root:

```rust
let response = Input::sanitize_plain_text(response);
tui.component_mut::<AiMessage>(ids.live_response)?
    .set_text(response);
```

Change `clear_live_response` to call `set_text("")` on `AiMessage`.

Change `apply_provider_error` to clear the live assistant response and append
the sanitized error text as an `AiMessage` without a `moh: error:` caption:

```rust
tui.component_mut::<Container>(ids.transcript)?
    .push(AiMessage::new(Input::sanitize_plain_text(&error.to_string())));
```

Remove the now-unused `untrusted_transcript_text` helper and its `Text`
construction. Keep every existing `request_render`, status transition, and
conversation call in the same order.

### Step 5: Run the focused demo tests and commit

Run:

```bash
cargo test demo::tests
cargo test --test components
cargo fmt --all -- --check
```

Expected: the demo renders bordered user messages and input, open assistant
messages, no role captions, and all existing interaction tests pass. Commit
the integration change:

```bash
git add src/demo.rs
git commit -m "feat: use styled messages in the demo"
```

## Task 4: Run the complete validation sequence and inspect the final diff

**Files:**

- Verify: all changed files from Tasks 1-3

### Step 1: Run formatting and static validation

Run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: both commands exit successfully without warnings.

### Step 2: Run all tests and the locked build

Run:

```bash
cargo test --all-targets
cargo build --locked
```

Expected: every existing and new test passes, and the binary builds with the
locked dependency graph.

### Step 3: Verify the diff and working tree

Run:

```bash
git diff --check
git status -sb
git log --oneline -4
```

Confirm the only commits created for this change are the spec commit and the
three focused implementation commits, and that no generated or credential
files are present.

### Step 4: Commit any validation-only corrections

If the validation sequence requires a source correction, rerun the narrowest
failing test first, then the complete validation sequence. Stage only files
from this feature's known file set and commit the correction with a message
describing the actual fix:

```bash
git add src/tui/components/mod.rs src/tui/components/surface.rs \
  src/tui/components/message.rs src/tui/components/input.rs \
  src/demo.rs tests/components.rs
git commit -m "fix: keep message frame within terminal width"
```

Do not claim completion until the final complete validation sequence and
`git diff --check` both succeed.
