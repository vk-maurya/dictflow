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

Selecting text in another app and pressing the refine shortcut (default
Option+1 / Alt+1) is a separate path: synthetic copy, the text-cleanup model
already configured under Models, then paste over the selection. It does not
record audio and does not write history.

One saved model serves both paths. Each path has its own switch in
`settings.json`. Saving a model (`llm_backend` other than `off`) does not
turn either path on by itself.

| Key | Purpose | How it behaves |
|-----|---------|----------------|
| `llm_backend` | Which cleanup model is available | `off`, `s1_mini`, `local`, `openai_compat`, `openrouter`, or `anthropic`. `off` means neither path can call a model. |
| `llm_enabled` | Voice-to-text model cleanup | Default `false` ("Polish after every dictation" in Models → Text cleanup). `false` skips the model after dictation. `true` runs it after every dictation. For every backend except S1-mini, Settings → Cleanup values `medium` and `high` also run the model even when this is `false`. S1-mini runs on dictation only when this is `true`. |
| `cleanup` | Local voice-to-text rules, no model required | `off` skips them. `light` removes fillers. `full` (default) removes fillers and tidies punctuation. `medium` and `high` do that and also request the model, as above. |
| `refine_selection_key` | Grammar fix for selected text | Default `Alt+1` (Option+1 on macOS). Runs only when that shortcut is pressed. `Off` disables it. Independent of `llm_enabled`: a model can refine a selection while dictation stays rules-only, and dictation can use the model while this shortcut is `Off`. |

There is no per-take override. To skip model cleanup on voice-to-text, leave
`llm_enabled` false and set `cleanup` to `off`, `light`, or `full`. To skip
grammar refine, set `refine_selection_key` to `Off`, or do not press the
shortcut. A refine that already pasted is not undone from inside DictFlow.

Heavy work never blocks the async runtime: capture, transcription, and the
hotkey poller each own a thread; only `Send + Sync` handles cross into Tauri
state (`Arc<Mutex<…>>`, `Arc<AtomicBool>`, `mpsc::Sender`).

## Modules (`src-tauri/src/`)

| File | Owns |
|------|------|
| `main.rs` | App state, all Tauri commands, hotkeys, tray, downloads, audio capture, paste |
| `models.rs` | Model catalog (both engines): ids, URLs, sizes, scores |
| `text.rs` | Pure offline text passes: dictionary, fillers, tidy, smart punctuation, WAV I/O + resampling |
| `stats.rs` | Lifetime usage ledger (`stats.json`); survives History clear-all |
| `session.rs` | 19-minute warn / optional 20-minute cap policy |
| `shortcuts.rs` | Parse/validate paste-last, copy-last, and refine-selection shortcut strings |
| `refine.rs` | Selected-text refine: capture result, cleanup-model call, shortcut orchestration |
| `devices.rs` | Mic pick + fallback + peak/RMS (no cpal) |
| `overlay.rs` | Pill dock snap + copy-chip window |
| `onboarding.rs` | First-run wizard step machine |

## Frontend (`src/`)

Framework-free TypeScript + Vite: a tiny view router (`renderView`) over
`Dashboard / Dictate / Models / Setup / History / Dictionary / Insights / Settings`
plus a first-run onboarding shell. Second window `overlay.html` / `src/overlay.ts`
is the always-on-top pill. Tauri `invoke` + event listeners, toasts. No state
library — module-level caches refreshed from commands.

## On-disk layout (`%APPDATA%/com.dictflow.app/dictflow/`)

```
models/<id>/…      # ggml-*.bin (Whisper) or encoder/decoder/joiner/tokens (Parakeet)
recordings/…       # per-dictation WAVs (deleted with their history item)
bin/               # optional whisper-cli.exe sidecar
settings.json      # Settings (unknown fields ignored → forward compatible)
history.json       # HistoryItem[] (migrates v0.1 plain-text format)
stats.json         # Usage ledger (lifetime + by_day); not wiped by Clear all
dictionary.json    # DictionaryEntry[]
logs/              # tauri-plugin-log file target
```

## Events (backend → UI)

`dictflow://recording(bool)` · `dictflow://transcribing(bool)` ·
`dictflow://refining(bool)` · `dictflow://refine-notice{message,error}` ·
`dictflow://download(string)` · `dictflow://download-progress{model_id,file,received,total}` ·
`dictflow://history-updated` · `dictflow://level{peak,rms}` · `dictflow://committed{text}` ·
`dictflow://session-warn` · `dictflow://device-fallback{from,to}`

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
