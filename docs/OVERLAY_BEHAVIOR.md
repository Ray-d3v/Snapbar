# Overlay behavior invariants

The Snapbar control surface is a fixed-size transparent GPUI window with a native Win32 region.

- The native region must be expressed in **window-relative** coordinates, including the client-area origin offset.
- `ClientToScreen` is bound directly from `user32` so the client origin can be converted without depending on a generated windows-rs module placement.
- The complete 68 px control-bar surface must remain inside the region at every DPI scale. Button bottoms, antialiasing, and shadows must not be clipped.
- When the menu is open, the bar and menu regions overlap slightly so there is no dead strip during pointer travel.
- An unpinned hover menu must close after the pointer remains outside the visible bar/menu surface for the configured close delay.
- Native cursor-position checks are the fallback source of truth when GPUI does not deliver a mouse-leave event because the pointer crossed a transparent or clipped portion of the window.
- A clicked menu remains pinned until the user clicks the menu button again.
