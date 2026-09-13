# DictFlow (local-only voice dictation for Windows)

![DictFlow logo](assets/logo-512.png)

Fast, private, offline voice-to-text for Windows 10/11. Hold a key, speak,
release — text lands in whatever app has focus. Nothing ever leaves your PC.

- **100% offline** — Parakeet (in-process) or Whisper (sidecar), zero network
  after model download. No accounts, no telemetry.
- **Two engines** — Parakeet TDT 0.6B v2 (English) + v3 (25 languages) via
  sherpa-onnx, in-process; Whisper tiny/base/small (.en + multilingual) via a
  whisper.cpp sidecar.
- **Mac-style talk key** — hold Right Ctrl (default), Scroll Lock, F9, Left
  Ctrl, or the Ctrl+Alt+Space combo; hold-to-talk or toggle.
- **Post-processing pipeline** — dictionary snippets, symbol presets, 3-level
  cleanup, smart trailing punctuation, Whisper→English translate.
- **Full app** — dashboard views (Dictate, Models, Setup, History, Dictionary,
  Statistics, Settings), per-dictation audio playback, WAV file transcription,
  model manager with progress, mic diagnostics, auto-start, update check.

## Quick start

1. Install prerequisites (one-time): see [docs/BUILD.md](docs/BUILD.md).
2. `npm install`
3. `npm run tauri dev`
4. Open **AI Models**, download **Parakeet TDT 0.6B v3** (~670 MB, one time).
5. Hold **Right Ctrl**, speak, release. Text is typed into the focused app.

No model + no sidecar needed beyond that: Parakeet runs in-process.

## Documentation

- [docs/BUILD.md](docs/BUILD.md) — dev environment, builds, troubleshooting
- [docs/RELEASE.md](docs/RELEASE.md) — versioning, installers, GitHub releases
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — pipeline, threads, storage

## Project layout

```
src/                 # Web UI (TypeScript + Vite, no framework)
src-tauri/           # Rust backend (Tauri v2)
  src/main.rs        # state, commands, hotkeys, tray
  src/models.rs      # model catalog (both engines)
  src/text.rs        # offline text pipeline (dictionary, cleanup, punctuation)
assets/              # logo source art (logo-512.png)
scripts/             # build-windows.ps1, clean-dev.ps1
docs/                # build / release / architecture notes
```

## Permissions (Windows)

- **Microphone** — one global toggle: Settings → Privacy & security →
  Microphone → *Let desktop apps access your microphone*. The in-app
  **Setup** view tests the mic and lists devices.
- **Auto-paste** — needs no grant on Windows (unlike macOS Accessibility).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). PRs welcome — run `npm run build`,
`cargo clippy -- -D warnings`, and `cargo test` before pushing.

## License

MIT — see [LICENSE](LICENSE).
