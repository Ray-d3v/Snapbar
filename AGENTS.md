# AGENTS.md

## Product constraints

- Snapbar targets Windows 11.
- Keep the app manually launched and non-resident. Closing the final window must terminate the process.
- A capture is copied to the Windows clipboard only. Do not add automatic file persistence, uploads, telemetry, or network calls.
- Do not attempt to bypass protected-content or screen-capture restrictions.
- A successful capture must contain only the detected shared-content region. Never fall back to copying the entire Teams window.
- If shared-content detection confidence is insufficient, fail closed and leave the clipboard unchanged.
- Shared-content detection must derive coordinates from the current window/capture geometry. Do not add fixed-resolution, fixed-monitor, or fixed-pixel crop tables.

## Architecture

- Use Rust and GPUI for the application UI.
- Keep Windows capture, shared-content detection, and clipboard logic isolated from GPUI under `src/capture.rs` and `src/capture/`.
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
