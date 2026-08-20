# AGENTS.md

## Product constraints

- Snapbar targets Windows 11.
- Keep the app manually launched and non-resident. Closing the final window must terminate the process.
- A capture is copied to the Windows clipboard only. Do not add automatic file persistence, uploads, telemetry, or network calls.
- Do not attempt to bypass protected-content or screen-capture restrictions.

## Architecture

- Use Rust and GPUI for the application UI.
- Keep Windows capture and clipboard logic isolated from GPUI in `src/capture.rs`.
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
