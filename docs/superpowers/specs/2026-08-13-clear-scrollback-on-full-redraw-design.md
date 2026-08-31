# Clear Scrollback on Full Redraw Design

## Goal

Prevent duplicated transcript blocks after a user fills the main-screen buffer, scrolls upward, and resizes the terminal.

## Root cause

Terminal width changes force `Renderer` to reconstruct the complete retained frame because wrapping may change every downstream row. The current full-redraw path emits `CSI 2J` followed by `CSI H`. `CSI 2J` clears the visible display but preserves saved scrollback, so the old rendered transcript remains in terminal history while the renderer writes a second complete copy. Repeating a resize can therefore leave multiple transcript copies in scrollback.

Existing VT100-backed tests validate only the visible screen and construct their parsers with zero scrollback. They consequently verify viewport reconstruction without detecting retained-history duplication.

## Chosen behavior

Every `FullRedraw` will purge saved scrollback before clearing the visible display, homing the cursor, and reconstructing the retained frame. Purging all saved lines is intentional and may remove terminal output that predates `moh`; the user accepted this tradeoff in favor of a single coherent transcript.

The initial render, pure appends, accessible changed-range rewrites, and resize events that do not select `FullRedraw` will retain their current behavior. They must not purge scrollback.

## Renderer change

Define the full-redraw clear sequence as:

```text
CSI 3J  CSI 2J  CSI H
```

`CSI 3J` erases saved lines, `CSI 2J` clears the visible display, and `CSI H` homes the hardware cursor. Use this sequence only in the existing `RenderPlan::FullRedraw` mutation path and in recovery code that reconstructs a retained frame during `finish` after an uncertain renderer I/O result.

Do not add a new render plan or change plan selection. Width changes, height shrink, inaccessible changed rows, explicit renderer reset, and renderer I/O recovery already converge on full reconstruction and should share the same purge behavior.

## Error handling and state

No new error type or renderer state is required. The purge, visible clear, home, frame reconstruction, and synchronized-update boundary remain part of one buffered terminal write. Existing write and flush failure behavior continues to mark the hardware screen unknown and retain the last successfully committed logical frame for recovery.

## Testing

Add focused renderer regressions that assert:

- a width-change full redraw emits `CSI 3J` before `CSI 2J` and `CSI H`;
- an unsafe full redraw caused by an inaccessible changed row also emits the purge sequence;
- recovery reconstruction during `finish` emits the same purge sequence;
- initial rendering and ordinary pure appends do not emit `CSI 3J`;
- existing VT100 visible-screen and cursor assertions still pass after the sequence changes.

The `vt100` development dependency does not implement `CSI 3J`, so byte-level assertions are the authoritative regression check for scrollback purging. Its existing screen model remains useful for validating the visible reconstruction performed by `CSI 2J`, cursor homing, and subsequent writes.

## Acceptance criteria

- Resizing in a state that requires full reconstruction leaves only the newly reconstructed managed transcript, with no prior copies retained in scrollback.
- Ordinary appends continue to build normal terminal scrollback without clearing it.
- Safe differential updates remain unchanged.
- All formatting, lint, test, and locked-build checks pass.
