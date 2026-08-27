# Overlay behavior invariants

The Snapbar control surface is a fixed-size transparent GPUI window whose native Win32 region changes with the visible title-bar UI.

- The overlay window follows the center-safe area of the active Teams title bar. Only the defined 8 logical-pixel island drop may cross the caption bottom edge; it must not cover Teams meeting controls or content.
- `DWMWA_EXTENDED_FRAME_BOUNDS` provides the Teams frame in screen coordinates. `DWMWA_CAPTION_BUTTON_BOUNDS` limits the usable right side of the title bar.
- The idle surface is a 92 × 30 logical-pixel transparent title-bar affordance: two adjacent 46-pixel caption-style cells containing only the status dot and a 16-pixel camera glyph. Compact mode is one 46 × 30 logical-pixel camera cell.
- Idle and compact cells stay completely inside the caption band. They have no backplate, shadow, label, rounded bottom, or drop; the black material appears only after native hover disclosure completes.
- After the configured disclosure delay, the surface becomes a 272 × 38 logical-pixel opaque-black island. Only this expanded island has a square top edge, a 16 logical-pixel bottom radius, and an 8 logical-pixel drop below the caption.
- Convert `DWMWA_CAPTION_BUTTON_BOUNDS` from window-relative coordinates using `GetWindowRect`, then clamp against screen-space `DWMWA_EXTENDED_FRAME_BOUNDS`. Position from the maximum expanded surface and reject any placement whose caption-contained height plus the defined drop cannot contain the surface.
- The top anchor does not change during disclosure. The idle surface occupies the 30-pixel caption band; expansion adds the reserved 8-pixel drop. Capture, save, rescan, and exit actions remain on one horizontal row; no separate downward menu opens.
- When the title-bar safe span is too narrow for the full strip, only the centered camera control is exposed; the compact camera remains a stateful, directly clickable control.
- The native region must be expressed in window-relative coordinates, including the client-area origin offset.
- `ClientToScreen` is bound directly from `user32` so the client origin can be converted without depending on a generated windows-rs module placement.
- The native region matches the current visible surface: a rectangular 92 × 30 idle region, rectangular 46 × 30 compact region, or square-top/rounded-bottom 272 × 38 expanded island. A one-alpha render surface keeps the transparent caption cells hit-testable; pixels outside the native region remain click-through so Teams title-bar dragging still works.
- Expansion begins only after a stable hover. Collapse occurs after the pointer remains outside the island silhouette for the configured delay.
- A window-level Win32 disclosure controller installed with `SetWindowSubclass` is the source of truth. It owns one explicit Collapsed / ExpandPending / Expanded / CollapsePending state machine, uses `TrackMouseEvent` / `WM_MOUSELEAVE`, and verifies the pointer with `WindowFromPoint` at timer boundaries. GPUI only renders the published mode.
- The expanded-only safety timer is a bounded fallback for missed leave messages; never restore permanent cursor polling.
- The subclass returns `MA_NOACTIVATE` for `WM_MOUSEACTIVATE`; operating the Snapbar controls must not activate the overlay or take focus from Teams.
- Collapsed, expanded, and compact dimensions come from one shared geometry source, and only the follower worker writes the native Win32 region.
- There is no click-to-pin state that can leave the strip permanently open.
- The overlay keeps `WS_EX_TOOLWINDOW` and `WS_EX_NOACTIVATE`, remains topmost relative to Teams, and is excluded from screen capture.
