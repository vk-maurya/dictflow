# Changelog

All notable DictFlow changes, newest first. Format follows Keep a Changelog;
versions are SemVer (`package.json` / `src-tauri/Cargo.toml` /
`src-tauri/tauri.conf.json` stay in sync).

## [0.3.1] - 2026-09-24

### Fixed

- macOS launch abort and dictation abort: sign the `.app` before the DMG is
  packed and hop overlay/tray calls back to the main thread.

### Added

- Parakeet Ultra 0.6B model option (`mldecode/parakeet-ultra-onnx-int8`):
  sherpa-onnx `nemo_transducer` bundle, 25 languages, ~630 MB, most accurate
  CPU model in the catalog. Default stays Parakeet TDT 0.6B v3.

## [0.3.0] - 2026-09-17

### Added

- macOS (Apple Silicon) support alongside Windows: session event tap talk key,
  Accessibility onboarding, Dock/tray behavior, signed `.dmg`.
- Windows WASAPI microphone name fixes and overlay chrome fixes.

## [0.2.1] - 2026-09-13

### Fixed

- Release workflow permissions fix.

## [0.2.0] - 2026-09-13

### Added

- Initial offline dictation release (Windows): Parakeet v2/v3, Whisper
  sidecar, tray + overlay, installer + portable builds.
