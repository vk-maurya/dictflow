# Build & develop (Windows)

## Prerequisites

| Tool | Version | Install |
|------|---------|---------|
| Rust (MSVC toolchain) | stable | `winget install Rustlang.Rustup`, then `rustup default stable-x86_64-pc-windows-msvc` |
| VS 2022 Build Tools | MSVC v143+ | Visual Studio Installer → *Desktop development with C++* (or existing VS 2022) |
| Node.js | 20+ | `winget install OpenJS.NodeJS` |
| WebView2 Runtime | any recent | preinstalled on Win 10/11 |
| CMake | 3.2x+ | `winget install Kitware.CMake` |
| Python + Pillow | 3.10+ | only for regenerating `assets/logo-512.png` |

Check: `rustc --version`, `cargo --version`, `node --version`, `cmake --version`.

## First setup

```powershell
npm install
```

This also fetches the Tauri CLI. The first Rust build downloads the
sherpa-onnx static libs (~100 MB) automatically — no manual step.

## Daily commands

```powershell
npm run tauri dev      # app with hot reload (Vite :1420 + debug binary)
npm run build          # typecheck + production frontend bundle (dist/)
cargo check --manifest-path src-tauri/Cargo.toml
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
npm run tauri build    # NSIS + MSI + portable exe (src-tauri/target/release/bundle/)
```

Or run everything via [`scripts/build-windows.ps1`](../scripts/build-windows.ps1).

## Notes

- **Rust flags** live in [`.cargo/config.toml`](../.cargo/config.toml): static
  CRT (`+crt-static`) to match sherpa-onnx's prebuilt MT libs. If you see
  `LNK4098`, this file isn't being picked up (run cargo from inside the repo).
- **Frontend dev server** ignores `src-tauri/**` (`vite.config.ts`) — otherwise
  Windows file locks on `.dll`s during linking crash Vite with `EBUSY`.
- **Icons**: edit `assets/logo-512.png` (or regenerate with
  `python <temp>/gen_logo.py`), then `npx tauri icon assets/logo-512.png`.
- **Logs**: backend logs go to stdout in `tauri dev` and to the app log dir in
  production (`tauri-plugin-log`); the webview forwards console via the same
  pipeline. Release builds are a GUI app (no extra CMD window). Open the log
  dir from `%APPDATA%\dictflow\logs`.
- **Portable exe**: `scripts/build-windows.ps1` (via
  `scripts/package-portable.ps1`) writes
  `src-tauri/target/release/bundle/portable/DictFlow.exe` and a versioned
  `DictFlow-<ver>-windows-x64-portable.exe`. Double-click — no installer.
  WebView2 (preinstalled on Windows 10/11) is still required. Data still
  lives in `%APPDATA%\dictflow`. See [RELEASE.md](RELEASE.md) to publish.

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| `Port 1420 is already in use` | Orphaned Vite from a killed terminal: `netstat -ano \| findstr :1420`, then `taskkill /F /PID <id>` — or run `scripts/clean-dev.ps1` |
| `EBUSY … .dll` from Vite | Update `vite.config.ts` watch ignore (already configured); don't open `target/` in editors with eager watchers |
| `failed to remove dictflow.exe (os error 5)` | Old instance still running — quit it (tray → Quit) or `taskkill /F /IM dictflow.exe` |
| `captured no audio` | Setup view → Test microphone; check Sound settings default input + privacy toggle |
| `LNK4098` linker warning | Ensure `.cargo/config.toml` applies (see Notes) |
| Slow first transcription | Model warm-up happens in background after download/select; wait for it once |
