# Changelog

All notable DictFlow changes, newest first. Format follows Keep a Changelog;
versions are SemVer (`package.json` / `src-tauri/Cargo.toml` /
`src-tauri/tauri.conf.json` stay in sync).

## [Unreleased]

### Added

- Chunked dictation: long takes are cut at natural pauses while you record and
  each piece is transcribed in the background, so only the last piece is left
  when you stop. The pieces are joined in order and the text pipeline (numbers,
  email, cleanup, dictionary, polish) runs once on the whole transcript.
  Pauses come from the bundled Silero VAD, with an adaptive energy detector as
  the fallback. Time is only a guardrail: a minimum piece length, and a hard
  cap that cuts at the quietest moment. Settings -> "Transcribe while you
  talk" (on by default) turns it off.
- Imported audio files use the same pause-based cutting.

### Changed

- Whisper sidecar temp files are unique per take and segment and are deleted
  after each call (previously a shared `last_transcription` / `last_16k.wav`).
- Cloud audio API uploads are built in memory instead of re-reading the WAV.

### Fixed

- A long dictation no longer waits on one large decode that the local model
  could truncate or stall on. In a 2-minute test recording the whole-file
  decode cut words short ("dictat", "dept") that the segmented decode
  completed.

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
