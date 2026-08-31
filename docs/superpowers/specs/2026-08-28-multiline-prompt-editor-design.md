# Multiline Prompt Editor Design

## Goal

Let a Moh user compose and edit a multiline prompt with Shift+Enter while
retaining Enter as submission and preserving the existing command-menu and
terminal interaction semantics.

## Scope

This change is confined to the fullscreen Ratatui client.  It replaces the
single-line presentation assumptions in `PromptEditor` and the prompt renderer;
it does not change session submission, transcript rendering, model transport,
clipboard support, or command syntax.

## Input and editor semantics

- Plain Enter submits the entire prompt, including any embedded newlines.
- Shift+Enter inserts exactly one newline at the grapheme-safe cursor position.
- Bracketed paste preserves LF line breaks, converts CRLF and CR to LF, and
  drops unsafe control sequences as it does today.  Tab remains a printable
  editor character; all other disallowed C0/C1 controls remain excluded.
- Left and Right move by grapheme.  Backspace and Delete remove one complete
  grapheme.  Ctrl+Left, Ctrl+Right, Ctrl+Backspace, and Ctrl+Delete retain their
  existing whitespace-aware behavior across newline boundaries.
- Home and End operate on the current logical line.  The existing transcript
  "follow latest" shortcut remains reachable only when End is pressed at the
  end of the final prompt line and no popup is open.
- Up and Down move the cursor to the nearest column on the previous or next
  visible line, including lines produced by soft wrapping.  If there is no
  adjacent line, they are consumed without changing editor state.  An open
  command or selector menu continues to consume Up and Down before the editor
  sees them.

## Layout and rendering

The prompt remains anchored immediately above the one-row status line.  Its
height grows from one row to at most four rows, based on the text's wrapped
visual height at the available terminal width.  The transcript receives all
remaining rows.  On a short terminal, prompt height is clamped so the
transcript and status rows remain renderable.

`PromptEditor` exposes a display window for the available width and visible
height.  The window contains only the cursor-visible logical/soft-wrapped rows,
plus the cursor's row and column.  The view renders a cyan `❯ ` prefix on the
first visible prompt row and aligns continuation rows beneath the text start.
The hardware cursor is positioned on the returned row and column and is always
inside the prompt area.

The editor owns only text, grapheme cursor position, preferred vertical column,
and display scrolling.  `view.rs` remains responsible for reserving prompt
height, rendering cells, and placing popups.  This keeps input mutation and
Ratatui layout separate.

## Error handling and compatibility

Unsafe pasted controls must not become terminal control sequences.  Existing
release-event filtering, Ctrl+C, help, selector, slash-command, transcript
scroll, and busy-submission behavior are unchanged.  A failed submission keeps
the complete multiline text in the editor through the existing restore path.

## Tests and acceptance

Unit tests for `PromptEditor` cover Shift+Enter insertion, plain Enter
submission, multiline paste normalization, grapheme-safe deletion over a line
boundary, Home/End, and Up/Down cursor movement on both explicit and wrapped
lines.  Render tests use Ratatui's `TestBackend` to assert the visible prompt
cells, prefix/continuation alignment, capped height, cursor position, and the
status row.  App tests confirm an open menu still owns Up/Down and that a
multiline prompt is submitted as one string.

Manual PTY acceptance checks verify Shift+Enter composition, scrolling after
four prompt rows, menu navigation, terminal resize/reflow, and alternate-screen
restoration.  Live model submission is not required for the PTY check.
