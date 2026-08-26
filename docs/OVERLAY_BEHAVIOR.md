# Overlay behavior invariants

The Snapbar control surface is a fixed-size transparent GPUI window whose native Win32 region changes with the visible title-bar UI.

- The overlay window follows the center-safe area of the active Teams title bar. It must not sit over the meeting content or the Teams meeting control row.
- `DWMWA_EXTENDED_FRAME_BOUNDS` provides the Teams frame in screen coordinates. `DWMWA_CAPTION_BUTTON_BOUNDS` limits the usable right side of the title bar.
- The idle surface is a centered 92 × 30 logical-pixel pill. It expands horizontally to a 272 × 36 logical-pixel control strip after the configured hover delay.
- The visible surface, rather than the larger transparent GPUI window, is vertically centered inside the measured caption band so DPI scaling cannot push controls into the meeting UI.
- The window never expands downward. Capture, save, rescan, and exit actions remain on one horizontal row.
- When the title-bar safe span is too narrow for the full strip, only the centered camera control is exposed; the compact camera remains a stateful, directly clickable control.
- The native region must be expressed in window-relative coordinates, including the client-area origin offset.
- `ClientToScreen` is bound directly from `user32` so the client origin can be converted without depending on a generated windows-rs module placement.
- The native region must match the current collapsed, expanded, or compact silhouette. Transparent pixels outside that region must remain click-through so Teams title-bar dragging still works.
- Expansion begins only after a stable hover. Collapse occurs after the pointer remains outside the visible surface for the configured delay.
- Native cursor-position checks are the source of truth when GPUI misses a mouse-leave event. There is no click-to-pin state that can leave the strip permanently open.
- The overlay keeps `WS_EX_TOOLWINDOW` and `WS_EX_NOACTIVATE`, remains topmost relative to Teams, and is excluded from screen capture.
