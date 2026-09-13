# DictFlow — SpeakType for Windows

Offline, privacy-first voice dictation for Windows 10/11 — Wispr Flow-style UX,
100% local like SpeakType.
Stack: **Tauri v2 (WebView2) + Rust + in-process Parakeet (sherpa-onnx)**,
whisper.cpp sidecar for Whisper — a Windows port of
[SpeakType](https://github.com/karansinghgit/speaktype) (macOS, Swift + WhisperKit).

> Status: v0.2. Two engines (Parakeet in-process default, Whisper via sidecar),
> dashboard UI (Dictate / AI Models / History / Dictionary / Statistics / Settings),
> model catalog with background warm-up, full offline post-processing pipeline.

## What SpeakType delivers (researched from source)

SpeakType (`speaktype/`, ~100 Swift files, macOS 13+, Apple Silicon recommended, MIT):

| # | Feature | Where in SpeakType source |
|---|---------|---------------------------|
| 1 | **Push-to-talk global hotkey** — default `Fn` hold-to-talk (mode 0) or toggle mode (mode 1); also single left/right ⌘/⌃/⌥ keys | `App/AppDelegate.swift` (CGEventTap + `NSEvent` flags monitors), `Models/HotkeyOption.swift`, `Models/HotkeyConfiguration.swift` |
| 2 | **Mic capture** — 16 kHz mono 16-bit WAV, live level meter, chunked background recording, device picker, idle session stop | `Services/AudioRecordingService.swift` (AVFoundation `AVCaptureSession` + `AVAssetWriter`) |
| 3 | **Local STT, two engines** behind a `SpeechToTextEngine` protocol + `TranscriptionManager` router | `Services/WhisperService.swift` (WhisperKit/CoreML), `Services/Transcription/ParakeetEngine.swift` (FluidAudio/CoreML), `Services/Transcription/TranscriptionManager.swift` |
| 4 | **Model catalog** — 5 Whisper (tiny 39 MB … large-v3-turbo 1.6 GB) + 3 Parakeet TDT; RAM/chip-aware recommender; background warm-up after download (first load otherwise stalls 30–60 s) | `Models/AIModel.swift`, `Models/DeviceCapability.swift`, `Utilities/ModelSelection.swift`, `Services/ModelDownloadService.swift`, `Utilities/ModelStorage.swift` |
| 5 | **Post-processing pipeline** — noise/placeholder strip → filler-word removal ("Auto Edit" toggle) → dictionary snippets/vocab fixes → smart trailing punctuation (strips `.` after emails/URLs/numbers) | `WhisperService.normalizedTranscription`, `Services/DictionaryService.swift`, `Utilities/SmartTrailingPunctuation.swift` |
| 6 | **Paste anywhere** — clipboard snapshot → `Cmd+V` via `CGEvent` (AppleScript fallback) → clipboard restore; terminal/emoji-picker edge-case handling | `Services/ClipboardService.swift` |
| 7 | **History + statistics** — transcripts with audio files, word counts, durations, per-day stats | `Services/HistoryService.swift`, `Views/Screens/History/*`, `Views/Screens/Statistics/*` |
| 8 | **App shell** — menu-bar app (dock icon only with dashboard open), floating recorder pill, dashboard sidebar, onboarding + permissions flow | `App/speaktypeApp.swift`, `Controllers/MiniRecorderWindowController.swift`, `Views/*` |
| 9 | **Monetization** — 14-day trial + license keys validated via Polar, Pro feature gate | `Services/TrialManager.swift`, `Services/LicenseManager.swift` |
| 10 | **Auto-update** — GitHub Releases check, DMG download + self-replace | `Services/UpdateService.swift` |

Dependencies (all Apple-only): `WhisperKit` (argmaxinc, CoreML/Neural Engine),
`FluidAudio` (Parakeet/CoreML), `KeyboardShortcuts` (sindresorhus), `whisperkit-cli`.

## macOS → Windows mapping

| SpeakType (macOS-only) | DictFlow (Windows) | Notes |
|---|---|---|
| WhisperKit (CoreML/ANE) | **whisper.cpp** (`ggml`, CPU + Vulkan/CUDA) via `whisper-cli.exe` sidecar (`%APPDATA%/dictflow/bin/` or `PATH`) | Same models, re-downloaded as ggml from Hugging Face |
| FluidAudio Parakeet (CoreML) | **sherpa-onnx in-process** (official `sherpa-onnx` Rust crate, static CPU build, auto-downloaded libs) — Parakeet TDT 0.6B v2 (English) + v3 (25 langs) int8 from `csukuangfj` HF repos | Default engine; background warm-up after download/select, like SpeakType |
| AVFoundation capture | **WASAPI** via `cpal` crate at 16 kHz mono | Proven pattern (localflow uses exactly this) |
| `Fn`/modifier hold via CGEventTap | Single-key hold/toggle (default hold **Right Ctrl**) via `GetAsyncKeyState` polling (Quill pattern, zero-dep raw FFI) + `Ctrl+Alt+Space` combo fallback via `tauri-plugin-global-shortcut` — `RegisterHotKey` cannot do bare modifiers | Configurable in Settings → Talk key |
| `Cmd+V` via CGEvent | **`Ctrl+V` via `SendInput`** (`enigo` crate) + clipboard backup/restore (`arboard`) | |
| MenuBarExtra / floating pill | Tauri **tray icon** + transparent always-on-top pill window | |
| SwiftUI dashboard | Web frontend (TS + Vite in Tauri) | |
| Keychain (license keys) | Credential Manager / DPAPI — only if monetization returns | Dropped in Phase 1 (MIT-only, no trial/Polar) |
| GitHub → DMG updater | `tauri-plugin-updater` + **NSIS/MSI** via `tauri-action` CI | |
| F19 emoji-picker suppression hack | N/A on Windows — dropped | |
| SwiftData history | JSON file now → SQLite later | |

Pure-logic ports (done, in `src-tauri/src/text.rs` with unit tests): Auto-Edit
filler removal, `DictionaryService` replacements, `SmartTrailingPunctuation`,
16 kHz resampling, history stats.

## Existing Windows alternatives surveyed

Same idea already ships on Windows — we borrow proven patterns, not code, from:

| Project | Stack | What to steal |
|---|---|---|
| **Vakh** (MIT) | Tauri 2 + `whisper-rs` + Win32 `SendInput`, MSI, orb UI | Closest to our stack; in-process whisper reference |
| **localflow** | Tauri 2 + whisper.cpp + llama.cpp cleanup, `cpal` 16 kHz, `SendInput`/Ctrl+V, SQLite history, pill | Full pipeline reference incl. VAD + LLM polish |
| **Quill** (AGPL — patterns only) | Tauri 2 + React, whisper.cpp CPU bundled + optional CUDA, `GetAsyncKeyState` polling, clipboard-paste commit | Hotkey polling + packaging model |
| **SpeakoFlow** (MIT, 211★, Handy fork) | Tauri, whisper.cpp + Parakeet + Silero VAD | Parakeet-on-Windows + VAD reference for Phase 3 |
| **openwispr** (MIT) | Pill, push-to-talk/hands-free, i18n | UX reference |
| **mosh888/whisper-dictation** | Python `faster-whisper` tray app | Fastest prototype path (rejected — you chose Tauri) |

## Roadmap

- **v0.2 (done):** Parakeet TDT v2/v3 in-process via sherpa-onnx (default, warm-up
  included), Whisper ggml catalog via sidecar, multi-file downloads with progress,
  dictionary + auto-edit + smart-punctuation pipeline, dashboard UI (Dictate,
  Models, History, Dictionary, Statistics, Settings), JSON history/settings,
  Ctrl+V paste, tray + `Ctrl+Alt+Space`. No licensing (MIT-only).
- **v0.3:** hold-to-talk + single-key polling (`GetAsyncKeyState`, Quill pattern),
  mic picker, floating waveform pill, Silero VAD trim, SQLite history,
  NSIS installer + updater, DirectML/CUDA provider option.
- **v0.4:** local LLM cleanup (`llama.cpp` sidecar, localflow pattern),
  multi-language UI, onboarding/permissions flow.

## Develop (Windows)

Prereqs: Rust stable MSVC, Node 20+, VS 2022 Build Tools (MSVC), WebView2
(preinstalled on Win 10/11), CMake.

```powershell
npm install
npm run tauri dev      # desktop app with HMR
npm run tauri build    # NSIS/MSI installer
```

Models live under `%APPDATA%\dictflow\models\<id>\`:
- Parakeet: `encoder.int8.onnx`, `decoder.int8.onnx`, `joiner.int8.onnx`,
  `tokens.txt` from `csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-{v2,v3}-int8`
  (downloaded in-app, ~670 MB each).
- Whisper: `ggml-<name>.bin` from `https://huggingface.co/ggerganov/whisper.cpp`
  (downloaded in-app) **plus** a `whisper.cpp` build as
  `%APPDATA%\dictflow\bin\whisper-cli.exe` (or anywhere on `PATH`).
