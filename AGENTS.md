# AGENTS.md

## Product constraints

- Snapbar targets Windows 11.
- Snapbar is manually launched and remains resident in the notification area until the user explicitly exits it. Do not add Windows logon startup without a separate explicit request.
- While no Teams meeting is detected, keep the title-bar control hidden. Show it automatically only after local meeting evidence remains stable for the configured debounce interval.
- Minimized Teams windows are not treated as meeting exit; hide the title-bar control until the meeting window is restored.
- The notification-area menu and the title-bar controls must both provide a clear full-process exit action when the full control strip is available.
- Every successful capture must be copied to the Windows clipboard.
- Optional PNG persistence is allowed only behind an explicit user-controlled toggle, must default to off, and must use the Windows configured Screenshots known folder.
- Do not add uploads, telemetry, network calls, a Windows service, or Graph/Teams cloud dependencies for meeting detection.
- Do not attempt to bypass protected-content or screen-capture restrictions.
- A successful capture must contain only the detected shared-content region. Never fall back to copying the entire Teams window.
- If shared-content detection confidence is insufficient, fail closed and leave the clipboard unchanged.
- Shared-content detection must derive coordinates from the current window/capture geometry. Do not add fixed-resolution, fixed-monitor, or fixed-pixel crop tables.
- Participant strips and Teams chrome must be excluded, while content that belongs to the presented desktop, including its Windows taskbar, must remain in the capture.
- A stable UI Automation element whose accessible name identifies shared content is authoritative. Use its `BoundingRectangle` without image-based trimming.
- Image heuristics may produce diagnostics or regression tests, but must not be auto-applied when an authoritative UIA rectangle is unavailable.
- A previously confirmed UIA rectangle may be reused only when the capture size and detected layout are unchanged.
- Local meeting detection must combine multiple signals and debounce both entry and exit. Do not treat a single Teams window, a single `TeamsVideo` child, or one transient UIA read as sufficient proof by itself.
- Start Windows Graphics Capture only while shared-content evidence exists. Stop and release the capture session when sharing ends or the meeting exits.
- Do not copy the full Teams frame to CPU memory merely to compute UIA geometry. Read only the authoritative cropped region.
- Reuse the cropped CPU buffer and avoid per-frame heap allocation. Keep only one latest cropped frame and a low-frequency backup refresh.
- Place the visible Snapbar affordance in the center-safe area of the Teams title bar, not over the meeting content or meeting control row.
- The idle affordance must remain approximately 80–96 px wide and 28–30 px high. Expand controls horizontally only; never open a menu below the title bar.
- Use DWM frame and caption-button bounds to avoid the Windows caption controls. When the available title-bar span is too narrow, reduce to a single camera control.
- Hover expansion and collapse must be debounced. The non-pinned control strip must always collapse after the pointer leaves the actual visible surface.
- Invisible transparent window areas must not intercept clicks or title-bar dragging intended for Teams.

## Architecture

- Use Rust and GPUI for the application UI.
- Keep Windows capture, shared-content detection, clipboard, and optional file-save logic isolated from GPUI under `src/capture.rs` and `src/capture/`.
- Keep local meeting detection isolated in `src/meeting.rs` and notification-area lifetime control in `src/resident.rs`.
- Prefer `SetWinEventHook` for prompt local change notification, but retain a low-frequency watchdog scan for missed events.
- Keep the visible title-bar controls opaque black. Transparency is permitted only outside their rounded silhouette.
- Keep the native input region aligned to the currently visible collapsed, expanded, or compact silhouette.
- Keep the control out of Alt+Tab and the taskbar, do not activate it when shown or clicked, and exclude it from screen capture.
- Prefer small, direct changes. Do not introduce Electron, Tauri, another webview, a background service, or cloud authentication.

## Quality gates

Run these on Windows before merging:

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

Update `README.md` when user-visible behavior or known limitations change.
