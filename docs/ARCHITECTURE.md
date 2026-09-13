# Architecture

## Pipeline

```
talk key / mic button / tray
  → capture thread (owns cpal::Stream, WASAPI 16 kHz-ish mono)
  → recordings/rec-<ts>.wav
  → spawn_blocking: load + resample to 16 kHz mono
  → STT engine
      ├─ Parakeet: transcriber thread (owns sherpa-onnx OfflineRecognizer)
      └─ Whisper: whisper-cli.exe sidecar (temp 16 kHz wav)
  → dictionary snippets → cleanup (off/light/full) → smart punctuation
  → history (JSON) → clipboard + Ctrl+V (SendInput) → clipboard restore
```

Heavy work never blocks the async runtime: capture, transcription, and the
hotkey poller each own a thread; only `Send + Sync` handles cross into Tauri
state (`Arc<Mutex<…>>`, `Arc<AtomicBool>`, `mpsc::Sender`).

## Modules (`src-tauri/src/`)

| File | Owns |
|------|------|
| `main.rs` | App state, all Tauri commands, hotkeys, tray, downloads, audio capture, paste |
| `models.rs` | Model catalog (both engines): ids, URLs, sizes, scores |
| `text.rs` | Pure offline text passes: dictionary, fillers, tidy, smart punctuation, WAV I/O + resampling |

## Frontend (`src/`)

Framework-free TypeScript + Vite: a tiny view router (`renderView`) over
`Dictate / Models / Setup / History / Dictionary / Statistics / Settings`,
Tauri `invoke` + event listeners, toasts. No state library — module-level
caches refreshed from commands.

## On-disk layout (`%APPDATA%/dictflow/`)

```
models/<id>/…      # ggml-*.bin (Whisper) or encoder/decoder/joiner/tokens (Parakeet)
recordings/…       # per-dictation WAVs (deleted with their history item)
bin/               # optional whisper-cli.exe sidecar
settings.json      # Settings (unknown fields ignored → forward compatible)
history.json       # HistoryItem[] (migrates v0.1 plain-text format)
dictionary.json    # DictionaryEntry[]
logs/              # tauri-plugin-log file target
```

## Events (backend → UI)

`dictflow://recording(bool)` · `dictflow://transcribing(bool)` ·
`dictflow://download(string)` · `dictflow://download-progress{model_id,file,received,total}` ·
`dictflow://history-updated`

## Key design constraints

- `cpal::Stream` and `sherpa_onnx::OfflineRecognizer` are `!Send` → each lives
  on exactly one thread for its whole life (see the stream-drop silence bug,
  fixed by returning the stream out of the builder closure).
- Static CRT (`+crt-static`, `.cargo/config.toml`) matches sherpa-onnx's MT
  libs — never mix CRTs (LNK4098 = future heap corruption).
- Clipboard restore is text-only (arboard limitation); non-text clipboard
  content is not preserved — stated in UI-adjacent code comments.
- `RegisterHotKey` can't do bare modifiers → single-key talk uses
  `GetAsyncKeyState` polling (raw FFI, no dependency).
