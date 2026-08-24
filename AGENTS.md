# AGENTS.md

## Product constraints

- Snapbar targets Windows 11.
- Keep the app manually launched and non-resident. Closing the final window must terminate the process.
- Every successful capture must be copied to the Windows clipboard.
- Optional PNG persistence is allowed only behind an explicit user-controlled toggle, must default to off, and must use the Windows configured Screenshots known folder.
- Do not add uploads, telemetry, network calls, or a background service.
- Do not attempt to bypass protected-content or screen-capture restrictions.
- A successful capture must contain only the detected shared-content region. Never fall back to copying the entire Teams window.
- If shared-content detection confidence is insufficient, fail closed and leave the clipboard unchanged.
- Shared-content detection must derive coordinates from the current window/capture geometry. Do not add fixed-resolution, fixed-monitor, or fixed-pixel crop tables.
- Participant strips and Teams chrome must be excluded, while content that belongs to the presented desktop, including its Windows taskbar, must remain in the capture.
- A stable UI Automation element whose accessible name identifies shared content is authoritative. Use its `BoundingRectangle` without image-based trimming.
- Image heuristics may produce diagnostics or a future user-confirmation candidate, but must not be auto-applied when an authoritative UIA rectangle is unavailable.
- A previously confirmed UIA rectangle may be reused only when the capture size and detected layout are unchanged.

## Architecture

- Use Rust and GPUI for the application UI.
- Keep Windows capture, shared-content detection, clipboard, and optional file-save logic isolated from GPUI under `src/capture.rs` and `src/capture/`.
- Prefer small, direct changes. Do not introduce Electron, Tauri, a webview, or a background service.
- Keep the visible control bar opaque black. Transparency is permitted only outside its rounded silhouette.

## Quality gates

Run these on Windows before merging:

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

Update `README.md` when user-visible behavior or known limitations change.
