# SpeakType — Functional Analysis

Source: `karansinghgit/speaktype` @ main (Swift, ~100 files, macOS 13+, Apple
Silicon recommended, MIT + Polar commercial licensing). Cloned
2026-09-13 to `%TEMP%\opencode\speaktype-research` for this teardown.

This doc is the functional spec DictFlow ports from. Each section ends with the
DictFlow status: **done** / **partial** / **todo**.

## 1. Core loop

Global hotkey → mic capture → on-device STT → text post-processing → paste
into the focused app → history entry. State machine (`TranscriptionState`):
`idle → listening → transcribing → ready | error`.

- **Hotkeys** (`HotkeyOption`, default `Fn`): single left/right ⌘/⌃/⌥/Fn keys
  via `NSEvent` flags monitors + a suppressing `CGEventTap` for Fn. Two modes
  (`recordingMode`): `0` hold-to-record (release stops; pressing another combo
  cancels), `1` toggle (press starts, press stops). `fn` also needs an
  emoji-picker suppression hack (synthetic F19) skipped for terminals.
- **DictFlow: partial.** Combo hotkey `Ctrl+Alt+Space` toggle only
  (`tauri-plugin-global-shortcut`). Todo: hold-to-talk, single-key polling
  (`GetAsyncKeyState`, Quill pattern), per-user hotkey choice.

## 2. Commit pipeline (the important detail)

`MiniRecorderWindowController.handleCommit`, in order:

1. **Copy text to clipboard FIRST** (always — transcription is never lost).
   Snapshot previous clipboard first if `restoreClipboardAfterAutoPaste`
   (default ON).
2. Settle pill to idle. 3. If no Accessibility permission: stop silently, user
   pastes manually (no nag popup).
3. Re-activate the previously focused app, wait 500 ms.
4. Paste via synthetic `Cmd+V` (`CGEvent`; AppleScript fallback exists).
5. Wait 350 ms, restore previous clipboard **only if it still holds our text**.

- **DictFlow: partial.** Paste works but never restores the clipboard, and the
  fallback path (no permission → leave on clipboard) is implicit. Todo: text
  snapshot/restore (arboard = text only; non-text clipboard content can't be
  preserved — accepted limitation, noted in UI).

## 3. Audio capture

`AVCaptureSession` → 16 kHz mono 16-bit WAV (`AudioFormat.default`), live
level meter (`audioLevels`), background chunk writers, device change handling,
idle session stop, mic picker (`AudioDevice` list, `selectedDeviceId`),
permission request flow. Error taxonomy: permissionDenied,
audioEngineFailure, fileWriteError, maxDurationExceeded (a cap exists),
audioInputUnavailable.

- **DictFlow: partial.** `cpal` (WASAPI) capture on a dedicated thread,
  device default only, no meter, no duration cap, no picker. Todo in v0.3.

## 4. Engines + model catalog

`SpeechToTextEngine` protocol; `TranscriptionManager` routes by
`TranscriptionEngineKind` (whisper/parakeet); display state mirrors the active
engine; **background warm-up** after download/select (first load otherwise
stalls 30–60 s).

| # | Model | Engine | Notes |
|---|-------|--------|-------|
| 1 | Whisper Large v3 Turbo 1.6 GB | whisper | multilingual, most accurate |
| 2 | Whisper Medium 1.5 GB | whisper | balanced |
| 3 | Whisper Small.en 244 MB | whisper | recommended English balance |
| 4 | Whisper Base.en 74 MB | whisper | fast & light |
| 5 | Whisper Tiny 39 MB | whisper | fastest |
| 6 | Parakeet TDT v3 ~2 GB | parakeet | 25 langs, real-time |
| 7 | Parakeet TDT v2 ~2 GB | parakeet | English, fastest recall |
| 8 | Parakeet TDT-CTC 110M ~450 MB | parakeet | tiny & fast |

Recommender (`AIModel.recommendedModel`): RAM + chip tier + Neural Engine +
use-case weights (dictation/balanced/transcription), with human-readable
reason. Models live under Application Support (migrated out of Documents —
needs no permission). Per-model: download progress + cancel + error-with-retry,
delete, RAM warnings, Selected/Installed badges, load-time spinner with
elapsed timer.

- **DictFlow: done (adapted).** 6-model catalog (Parakeet v2/v3 int8 via
  sherpa-onnx in-process + 4 Whisper ggml via sidecar), warm-up preload,
  progress, auto-select. Gaps (todo): model delete, cancel download,
  recommended hero, RAM warnings, load-stage display, Parakeet CTC-110M,
  Large/Medium Whisper.

## 5. Post-processing (exact order)

Engine text → `DictionaryService.apply` → Auto-Edit filler removal (toggle,
default OFF in SpeakType — DictFlow defaults ON) → `SmartTrailingPunctuation`
(toggle, default ON; strips lone trailing `.` on emails/URLs/numbers/tokens).

- **DictFlow: done** (`text.rs`, unit-tested). Note the default flip on
  auto-edit.

## 6. Screens

Sidebar (7): Dashboard, Transcribe Audio, History, Dictionary, Statistics,
AI Models, Settings. Plus: 3-page onboarding (Welcome → hotkey nudge →
Permissions), Permissions screen (mic + accessibility rows, 1 s polling),
menu-bar popover (stats grid + last 5 transcripts + Open/Quit), floating pill
(idle/record/transcribing, movable, always-on-top, click-through when idle),
update sheet, license view. First launch with no model redirects to AI Models.

- **DictFlow: partial.** 6 views (no Dashboard home, no Transcribe Audio yet,
  no onboarding, no pill, tray menu instead of popover).
- Dashboard metrics: greeting + words + **time saved @ 40 WPM** + today/all
  counts + 7-day activity chart. Menu-bar shows the same stats + recents.
- History: cards with expand → **audio playback** (`AudioPlayerService`:
  play/pause/seek), copy, delete (also deletes audio file), Clear All (keeps
  stats). Trial-expired users see last 5 only.
- Dictionary: rule cards (trigger → replacement), enable toggle, editor sheet
  (trigger/replacement/whole-word), delete confirm. **Pro-gated after trial.**
- Statistics: period selector (week/month/…), words-per-day bar chart, summary
  cards, live-tick while recording.
- Settings tabs: General (theme, hotkey, **recording mode**, language +
  recents, auto-edit, smart-punctuation, clipboard restore, pill visibility +
  position, auto-update, license) / Audio (device list + refresh) /
  Permissions (status rows).
- Transcribe Audio: drag-drop **or** file picker for audio/**video**, record
  inline alternatively, result with copy/paste, saves to history with duration
  + audio URL.

## 7. Monetization (documented, NOT ported)

14-day trial from first launch. Post-expiry: transcribe 10/day, history last 5,
export/advanced-models/cloud-sync/dictionary blocked. License keys via Polar,
keychain-stored, Pro gate component + upgrade prompts. DictFlow stays MIT-only.

## 8. Updates & misc

GitHub Releases check on launch (daily), skip-version + reminder logic, DMG
download/verify/mount/self-replace. Custom fonts (Satoshi/ClashDisplay/Source
Sans), theme system, SwiftData + UserDefaults persistence, `AppLogger`,
Xcode `Makefile` workflow, SwiftLint CI, unit + UI tests.

## 9. DictFlow parity checklist

Done: dual-engine transcribe, warm-up, catalog+progress, pipeline, history,
stats, dictionary, settings, tray, global hotkey, JSON stores, CI+tests,
WAV file transcription (`transcribe_file`), clipboard restore-after-paste
(text only), model delete, single-key hold/toggle talk key (RightCtrl default,
ScrollLock/F9/combo options), per-dictation WAVs with history playback.
Next (ordered): cancel downloads · mic picker · pill window · onboarding ·
recommended-model hero · SQLite · NSIS/updater · DirectML/CUDA · LLM polish.
Explicitly dropped: Fn/emoji hacks, trial/Polar, SwiftData-isms.
