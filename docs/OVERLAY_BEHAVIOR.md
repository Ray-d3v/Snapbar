# Overlay behavior invariants

The Snapbar control surface is a fixed-size transparent GPUI window whose native Win32 region changes with the visible title-bar UI.

- The overlay window follows the center-safe area of the active Teams title bar. It must not sit over the meeting content or the Teams meeting control row.
- `DWMWA_EXTENDED_FRAME_BOUNDS` provides the Teams frame in screen coordinates. `DWMWA_CAPTION_BUTTON_BOUNDS` limits the usable right side of the title bar.
- The idle surface is a centered 92 × 28 logical-pixel pill. It expands horizontally to a 272 × 30 logical-pixel control strip after the configured hover delay.
- Convert `DWMWA_CAPTION_BUTTON_BOUNDS` from window-relative coordinates using `GetWindowRect`, then clamp against screen-space `DWMWA_EXTENDED_FRAME_BOUNDS`. Position from the maximum expanded surface and reject any placement whose visible surface is not fully contained in the resulting caption band.
- The window never expands downward. Capture, save, rescan, and exit actions remain on one horizontal row.
- When the title-bar safe span is too narrow for the full strip, only the centered camera control is exposed; the compact camera remains a stateful, directly clickable control.
- The native region must be expressed in window-relative coordinates, including the client-area origin offset.
- `ClientToScreen` is bound directly from `user32` so the client origin can be converted without depending on a generated windows-rs module placement.
- The native region keeps the current collapsed, expanded, or compact width, but its vertical hit band fills the 30 logical-pixel Teams title-bar height. The otherwise invisible pixels in that band use the minimum nonzero alpha required for layered-window hit testing; pixels outside the region remain click-through so Teams title-bar dragging still works.
- Expansion begins only after a stable hover. Collapse occurs after the pointer remains outside the title-bar-height hit band for the configured delay.
- A window-level Win32 disclosure controller installed with `SetWindowSubclass` is the source of truth. It owns one explicit Collapsed / ExpandPending / Expanded / CollapsePending state machine, uses `TrackMouseEvent` / `WM_MOUSELEAVE`, and verifies the pointer with `WindowFromPoint` at timer boundaries. GPUI only renders the published mode.
- The expanded-only safety timer is a bounded fallback for missed leave messages; never restore permanent cursor polling.
- The subclass returns `MA_NOACTIVATE` for `WM_MOUSEACTIVATE`; operating the Snapbar controls must not activate the overlay or take focus from Teams.
- Collapsed, expanded, and compact dimensions come from one shared geometry source, and only the follower worker writes the native Win32 region.
- There is no click-to-pin state that can leave the strip permanently open.
- The overlay keeps `WS_EX_TOOLWINDOW` and `WS_EX_NOACTIVATE`, remains topmost relative to Teams, and is excluded from screen capture.
