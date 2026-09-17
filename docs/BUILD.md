# Build & develop

DictFlow supports Windows and macOS. Pick the section for your platform.

---

## macOS

### Prerequisites

| Tool | Version | Install |
|------|---------|---------|
| Xcode Command Line Tools | any | `xcode-select --install` |
| Rust | stable | `curl --proto '=https' --tlsv1.2 https://sh.rustup.rs -sSf \| sh` |
| Rust target | aarch64 or x86_64 | `rustup target add aarch64-apple-darwin` (Apple Silicon) |
| Node.js | 20+ | `brew install node` |
| CMake | 3.x+ | `brew install cmake` (only needed to build whisper.cpp from source) |

Check: `rustc --version`, `cargo --version`, `node --version`.

### Optional — whisper.cpp sidecar

The Parakeet engine requires no sidecar. If you want the Whisper engine:

```bash
# Easiest: install via Homebrew
brew install whisper-cpp

# Copy to DictFlow's data directory
DATA="$HOME/Library/Application Support/com.dictflow.app/dictflow"
mkdir -p "$DATA/bin"
cp /opt/homebrew/bin/whisper-cli "$DATA/bin/"
```

Or build from source:
```bash
git clone https://github.com/ggerganov/whisper.cpp
cd whisper.cpp && cmake -B build && cmake --build build -j --config Release
cp build/bin/whisper-cli "$DATA/bin/"
```

### First setup

```bash
npm install
```

### Daily commands

```bash
npm run tauri dev                    # app with hot reload
npm run build                        # typecheck + production frontend bundle
cargo check --manifest-path src-tauri/Cargo.toml
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
npx tauri build --target aarch64-apple-darwin   # Apple Silicon .app + .dmg
```

Or use the convenience script: `bash scripts/build-macos.sh`

### Data directory

All user data (settings, models, history) lives in:
```
~/Library/Application Support/com.dictflow.app/dictflow/
```

### macOS permissions

DictFlow needs two macOS permissions (same pair SpeakType uses). The app prompts during onboarding:

| Permission | Why | How to grant |
|------------|-----|--------------|
| Microphone | Capture voice | Auto-prompted by CoreAudio on first recording |
| Accessibility | Fn talk key (session CGEventTap) + paste Cmd+V | Setup → Enable, then toggle **DictFlow** (not Terminal) in System Settings → Privacy & Security → Accessibility |

`npm run tauri dev` from a terminal wraps the debug binary as `src-tauri/target/debug/DictFlow.app` before launch. That is required: macOS only lists real `.app` bundles under Accessibility. A raw `dictflow` process is attributed to Terminal / Cursor and never shows up.

### Running a downloaded binary (Gatekeeper)

Pre-built binaries from GitHub Releases are ad-hoc signed but not notarized. To open them:

```bash
# Option A: remove quarantine attribute
xattr -dr com.apple.quarantine DictFlow.app

# Option B: right-click → Open in Finder on first launch
```

Self-built binaries (built from source) have no quarantine attribute and launch immediately.

### Troubleshooting

| Symptom | Fix |
|---------|-----|
| `Port 1420 is already in use` | Orphaned Vite from a killed terminal: `lsof -i :1420`, then `kill <pid>` |
| `captured no audio` | Setup → Test microphone; check Microphone permission in System Settings |
| `mac session tap failed` / Fn does nothing | Setup → Enable Accessibility, allow the prompt, then hold Fn. Restart only if the talk key stays dead. |
| `paste doesn't reach editor` | Grant Accessibility in System Settings; restart DictFlow |
| Slow first transcription | Model warm-up happens once after download/select; wait for it |
| `whisper-cli not found` | Copy the binary to `~/Library/Application Support/com.dictflow.app/dictflow/bin/` |

---

## Windows

### Prerequisites

| Tool | Version | Install |
|------|---------|---------|
| Rust (MSVC toolchain) | stable | `winget install Rustlang.Rustup`, then `rustup default stable-x86_64-pc-windows-msvc` |
| VS 2022 Build Tools | MSVC v143+ | Visual Studio Installer → *Desktop development with C++* (or existing VS 2022) |
| Node.js | 20+ | `winget install OpenJS.NodeJS` |
| WebView2 Runtime | any recent | preinstalled on Win 10/11 |
| CMake | 3.2x+ | `winget install Kitware.CMake` |
| Python + Pillow | 3.10+ | only for regenerating `assets/logo-512.png` |

Check: `rustc --version`, `cargo --version`, `node --version`, `cmake --version`.

### First setup

```powershell
npm install
```

This also fetches the Tauri CLI. The first Rust build downloads the
sherpa-onnx static libs (~100 MB) automatically — no manual step.

### Daily commands

```powershell
npm run tauri dev      # app with hot reload (Vite :1420 + debug binary)
npm run build          # typecheck + production frontend bundle (dist/)
cargo check --manifest-path src-tauri/Cargo.toml
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
npm run tauri build    # NSIS + MSI + portable exe (src-tauri/target/release/bundle/)
```

Or run everything via [`scripts/build-windows.ps1`](../scripts/build-windows.ps1).

### Data directory

All user data (settings, models, history) lives in:
```
%APPDATA%\com.dictflow.app\dictflow\
```

Logs: `%APPDATA%\com.dictflow.app\dictflow\logs\`

### Notes

- **Rust flags** live in [`.cargo/config.toml`](../.cargo/config.toml): static
  CRT (`+crt-static`) to match sherpa-onnx's prebuilt MT libs. If you see
  `LNK4098`, this file isn't being picked up (run cargo from inside the repo).
- **Frontend dev server** ignores `src-tauri/**` (`vite.config.ts`) — otherwise
  Windows file locks on `.dll`s during linking can crash Vite.
- **Icons**: edit `assets/logo-512.png`, then `npx tauri icon assets/logo-512.png`.
- **Portable exe**: `scripts/build-windows.ps1` (via
  `scripts/package-portable.ps1`) writes
  `src-tauri/target/release/bundle/portable/DictFlow.exe` and a versioned
  `DictFlow-<ver>-windows-x64-portable.exe`. See [RELEASE.md](RELEASE.md) to publish.

### Troubleshooting

| Symptom | Fix |
|---------|-----|
| `Port 1420 is already in use` | Orphaned Vite from a killed terminal: `netstat -ano \| findstr :1420`, then `taskkill /F /PID <id>` — or run `scripts/clean-dev.ps1` |
| `EBUSY … .dll` from Vite | Update `vite.config.ts` watch ignore (already configured); don't open `target/` in editors with eager watchers |
| `failed to remove dictflow.exe (os error 5)` | Old instance still running — quit it (tray → Quit) or `taskkill /F /IM dictflow.exe` |
| `captured no audio` | Setup view → Test microphone; check Sound settings default input + privacy toggle |
| `LNK4098` linker warning | Ensure `.cargo/config.toml` applies (see Notes) |
| Slow first transcription | Model warm-up happens in background after download/select; wait for it once |
