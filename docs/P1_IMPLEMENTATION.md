# P1 — Overlay + Recovery: Implementation Brief

Researched 2026-09-13. **Do not re-explore the repo from scratch.** This file
is the handoff for the next agent. Product intent lives in
`docs/WISPR_FLOW_GAP_ANALYSIS.md` §6 P1 (items 6–12). Architecture constraints
live in `docs/ARCHITECTURE.md`. This brief is the *how*.

Status: **shipped 2026-09-13**. P0 (dashboard / `stats.json`) shipped earlier the same day.

---

## 1. What to ship (acceptance)

A Wispr user should feel the daily loop without opening the main window.

| # | Feature | Done when |
|---|---|---|
| 6 | Floating always-on-top pill | Second Tauri window: idle / recording / transcribing; drag-snap to a screen edge; click-through when idle; “Copy” chip for ~10 s after a successful take |
| 7 | Dedicated cancel | Esc (while recording) and a Cancel control drop the take: no STT, no history, no paste. WAV deleted. |
| 8 | Paste-last / copy-last | Global shortcuts, Wispr defaults `Shift+Alt+Z` / `Shift+Alt+X`, rebindable in Settings |
| 9 | Mic picker + meter + fallback | Capture from a named device, not only OS default; live peak/RMS while recording; if the chosen device vanishes, next start falls back to default and we emit a notice |
| 10 | Cancel model download | In-flight download stops, `.part` is deleted, card returns to Download |
| 11 | First-run onboarding | Fresh install: Welcome → talk key → mic test → download recommended model. Existing users must **not** see the wizard. |
| 12 | 19-minute session warning | Always toast/event at 19:00. Optional hard stop at 20:00 (`session_cap`, default **off** — unlimited + warn only) |

Out of scope for this slice: P2 text rules, P3 LLM/Audio API, mouse Flow,
RAM-based model hero (P4), mid-take device hot-swap.

---

## 2. Repo map (already verified — do not re-walk)

Single-binary Tauri v2 app. No `lib.rs`. Frontend is one `src/main.ts` view
router (no React, no test runner). Rust unit tests are the project pattern
(`stats.rs`, `text.rs`, `main.rs`).

```
src-tauri/src/
  main.rs      ~1994 lines — state, commands, capture, hotkeys, tray, download
  models.rs    catalog + default_model_id() = "parakeet-v3"
  text.rs      dictionary / fillers / punctuation / WAV I/O  (has tests)
  stats.rs     lifetime ledger                             (has tests)

src/main.ts    ~1419 lines — all UI
src/style.css  SpeakType-ish light chrome
index.html     single page (#app)
```

Key symbols in `main.rs` (line numbers as of 2026-09-13; search the name if
they drift):

| Symbol | ~line | Notes |
|---|---|---|
| `hotkey_vk` | 46 | `RightCtrl`/`LeftCtrl`/`ScrollLock`/`F9` → VK; else combo |
| `TalkEdge::{Pressed,Released,Cancel}` | 64 | Cancel today = *other key while talk-key held* and only if `hotkey_owned` |
| `spawn_hotkey_thread` | 85 | 10 ms `GetAsyncKeyState` poll; add Esc here |
| `Settings` | 163 | serde; unknown fields ignored. Add new fields with `#[serde(default…)]` |
| `HistoryItem` | 201 | already has `raw_text`, `words_out`, `dict_hits` (P0) |
| `ActiveRecording` | 260 | `samples`, `sample_rate`, `stop`, `done`, `started_at` |
| `AppState` | 268 | `recording`, `hotkey_owned`, `active`, `settings`, `history`, `stats`, `downloading: HashSet<String>` |
| `run_capture` | 570 | **always** `host.default_input_device()` — this is the picker hole |
| `start_recording` / `stop_and_save_wav` | 647 / 687 | stream must stay alive until `stop` (known silence bug) |
| `get_audio_devices` / `test_microphone` | 740 / 779 | list + 1.5 s peak test, still default device only |
| `download_model` | 1065 | streams to `.part`, rename on success; **no cancel flag** |
| `set_settings` | 1420 | validates model / talk key / mode; then `apply_hotkey_registration` |
| `cancel_recording` | 1452 | **exists, unused from UI**. Stops, writes WAV, deletes WAV. Perfect cancel path. |
| `toggle_recording` / `stop_transcribe` | 1460 / 1477 | UI + tray + combo |
| `TalkEdge` consumer in `setup` | 1751 | Pressed/Released/Cancel routing |
| `invoke_handler` | 1824 | add new commands here |
| `mod tests` / `test_state()` | 1861 | **must grow** when `AppState` grows |

Frontend hooks:

| Thing | Where |
|---|---|
| Settings type + `saveSettings` rebuilds a **full** object | `src/main.ts` ~71, ~1322 — **spread `…settings`** when adding fields or you will wipe them |
| Dictate timer | `startTimer` / `paintRecTimer` (~394) already ticks 250 ms |
| Models download button | `viewModels` ~722 — disabled “Downloading…”, no Cancel |
| Setup device list | `viewSetup` ~763 — display only, not selectable |
| Events already listened | `dictflow://recording`, `transcribing`, `download`, `download-progress`, `history-updated` |
| `call()` | wraps `invoke`, toasts errors |

Config:

- `src-tauri/tauri.conf.json` — one window `main` (1280×800). Overlay is a
  **second window**, not a DOM widget.
- `src-tauri/capabilities/default.json` — `"windows": ["main"]`. Must add
  `"overlay"` plus window perms below.
- `vite.config.ts` — single input. Overlay needs a second HTML entry.
- `package.json` — no vitest. Do **not** add a JS test runner for P1. Logic
  tests go in Rust.

---

## 3. SWE rules for this slice

1. **Pure logic in new modules, I/O stays in `main.rs`.** Same pattern as
   `stats.rs` / `text.rs`. If a function needs `cpal`, `reqwest`, or a window
   handle, it is not the unit-tested core.
2. **`#[cfg(test)]` on every new module.** Table-driven where it helps.
3. **Do not refactor `main.rs` for sport.** Extract only what P1 needs.
   `main.rs` is already ~2k lines; new code belongs in new files.
4. **Forward-compatible settings.** Missing JSON fields must default so
   existing `%APPDATA%/dictflow/settings.json` still loads.
5. **Onboarding vs existing users:**
   - `Default::default()` (no file) → `onboarded: false` → wizard.
   - `#[serde(default = "default_true")]` on `onboarded` → old files without
     the field skip the wizard.
6. **Fail-open, don’t steal keys.** Esc is polled **only while recording**.
   Never register a permanent global Esc. Utility shortcuts always need at
   least one modifier.
7. **Cancel is discard.** `cancel_recording` already does the right thing.
   Do not invent a second path. After cancel: emit `dictflow://recording`
   false; do **not** emit `history-updated`.
8. **Stream lifetime.** `run_capture` must keep the `cpal::Stream` alive
   until `stop`. Returning it from the builder closure is load-bearing.
9. **Update `test_state()`** whenever `AppState` gains a field or the
   `downloading` type changes.
10. **Frontend `saveSettings` must spread.** See §2. Same for any new
    Settings write.

---

## 4. New Rust modules (create these)

### `src-tauri/src/session.rs` — 19 / 20 minute policy

```
pub const WARN_SECS: f64 = 19.0 * 60.0;  // 1140
pub const CAP_SECS: f64  = 20.0 * 60.0;  // 1200

pub enum SessionTick { None, Warn, Cap }

pub fn session_tick(elapsed_secs: f64, already_warned: bool, cap_enabled: bool) -> SessionTick
```

- `elapsed < 1140` → `None`
- `elapsed >= 1140 && !already_warned` → `Warn` (even if cap is off)
- `elapsed >= 1200 && cap_enabled` → `Cap` (caller then `stop_transcribe`,
  not cancel — user spoke for 20 min, keep the take)
- otherwise `None`

Tests: below warn; warn once; no second warn; cap off at 21 min stays `None`
after warn; cap on at 20 min → `Cap`.

### `src-tauri/src/shortcuts.rs` — parse `Shift+Alt+Z` (no Tauri types)

```
pub struct ShortcutSpec { pub ctrl, alt, shift, meta: bool; pub key: String }

pub fn parse_shortcut(s: &str) -> Result<ShortcutSpec, String>
pub fn format_shortcut(s: &ShortcutSpec) -> String
pub fn is_safe_utility(s: &ShortcutSpec) -> bool  // letter/digit requires ≥1 modifier
```

- Split on `+`, case-insensitive. Keys: `A`–`Z`, `0`–`9`, `Space`, `Escape`,
  `F1`–`F12`.
- Defaults: paste `Shift+Alt+Z`, copy `Shift+Alt+X`.
- Reject empty, unknown tokens, paste==copy, and `Ctrl+Alt+Space` (talk combo).

`main.rs` maps `ShortcutSpec` → `tauri_plugin_global_shortcut::Shortcut`
(`Modifiers` + `Code::KeyZ` / `KeyX` / …). Keep that mapping thin and
test the spec parser, not the plugin.

Tests: happy path, lowercase, reject bare `Z`, reject garbage, format
round-trip, defaults parse.

### `src-tauri/src/devices.rs` — picker + level (no cpal)

```
pub struct DeviceChoice { pub name: String, pub fell_back: bool }

pub fn choose_device(
    names: &[String],
    default_name: Option<&str>,
    preferred: Option<&str>,
) -> Result<DeviceChoice, String>

pub struct AudioLevel { pub peak: f32, pub rms: f32 }

pub fn level_from_samples(samples: &[f32]) -> AudioLevel
```

Rules:

- `preferred` present and in `names` → that name, `fell_back = false`.
- `preferred` missing/empty → `default_name`, `fell_back = false`.
- `preferred` set but not in `names` → `default_name`, `fell_back = true`.
- no devices / no default → `Err`.

`run_capture` uses this after listing cpal names. Mid-take unplug: do **not**
rebuild the stream. Set `stream_error` in the cpal err callback; the level
ticker can cancel. Next `start_recording` re-resolves (fallback).

Level: peak = max abs; rms = sqrt(mean square); empty → zeros. Live meter
reads the **last 1024** samples of `ActiveRecording.samples` every ~80 ms.

Tests: preferred hit; preferred gone → fallback; empty preferred → default;
no default → err; silence / known peak / empty slice.

### `src-tauri/src/overlay.rs` — dock snap + copy-chip window

Pure geometry. Pill constants: `PILL_W = 320`, `PILL_H = 56`, `MARGIN = 12`.
Copy-chip visible for `COPY_CHIP_SECS = 10`.

```
pub enum DockEdge { Top, Bottom, Left, Right }

pub struct OverlayPose { pub x: i32, pub y: i32, pub edge: DockEdge, pub offset: i32 }

pub fn snap_to_edge(x, y, screen_w, screen_h, pill_w, pill_h) -> OverlayPose
pub fn pose_to_xy(edge, offset, screen_w, screen_h, pill_w, pill_h) -> (i32, i32)
pub fn copy_chip_visible(pasted_unix_ms: u64, now_unix_ms: u64, window_ms: u64) -> bool
```

Snap: distance to each edge, pick nearest, clamp the free axis so the pill
stays on-screen. Offset = free-axis position (x on top/bottom, y on left/right).

Tests: center-top snap; near-left snap; clamp when dragged past the corner;
`pose_to_xy` inverse of a snapped pose; chip visible at 9.9 s, hidden at 10.1 s.

### `src-tauri/src/onboarding.rs` — wizard state machine

```
pub enum OnboardStep { Welcome, TalkKey, MicTest, Download, Done }

pub fn next(step) -> OnboardStep
pub fn prev(step) -> OnboardStep
pub fn recommended_model_id() -> &'static str  // models::default_model_id(), "parakeet-v3"
```

Skip is allowed at every step (sets `onboarded = true`). Do not trap the user.
P4 will replace “recommended” with RAM scoring — do not invent that here.

Tests: linear next/prev; `prev(Welcome) == Welcome`; `next(Done) == Done`.

---

## 5. Settings additions

Add to `Settings` (`main.rs` ~163). Every new field needs a serde default
**and** a `Default` impl value.

| Field | Serde default | `Default::default()` | Meaning |
|---|---|---|---|
| `audio_device: Option<String>` | `None` | `None` | Preferred input name; `None` = OS default |
| `overlay_enabled: bool` | `default_true` | `true` | Show the pill |
| `overlay_edge: String` | `"top"` | `"top"` | `top`/`bottom`/`left`/`right` |
| `overlay_offset: i32` | `0` | `0` | Along the docked edge |
| `paste_last_key: String` | `"Shift+Alt+Z"` | same | Utility shortcut |
| `copy_last_key: String` | `"Shift+Alt+X"` | same | Utility shortcut |
| `onboarded: bool` | **`default_true`** | **`false`** | See §3.5 |
| `session_cap: bool` | `false` | `false` | Hard stop at 20 min |

`set_settings` extra validation:

- talk key unchanged rules
- `parse_shortcut` both utility keys; `is_safe_utility`; they must differ;
  neither may be `Ctrl+Alt+Space`
- `overlay_edge` ∈ {top,bottom,left,right}
- after save: `apply_hotkey_registration` **and** `apply_utility_shortcuts`

`get_status` should report the **resolved** capture device (preferred if
still present, else default), not blindly `default_input_name()`.

---

## 6. AppState / capture / download changes

```
struct ActiveRecording {
    // existing…
    stream_error: Arc<AtomicBool>,
}

struct AppState {
    // existing…
    downloading: HashMap<String, Arc<AtomicBool>>,  // was HashSet
}
```

`get_models` / `downloading.contains` → `contains_key`.

`run_capture(ready_tx, samples, stop, preferred: Option<String>, stream_error)`:

1. List input names + default name.
2. `devices::choose_device`.
3. Open that cpal device (match by name; if race-lost, default).
4. On stream err callback: `stream_error.store(true)`.
5. Keep returning the stream from the closure (silence bug).

After `start_recording` succeeds, spawn a ~80 ms ticker (tokio) that:

- reads last 1024 samples → `level_from_samples` → emit `dictflow://level {peak,rms}`
- `session_tick(elapsed, warned, settings.session_cap)`
  - `Warn` → emit `dictflow://session-warn` once (`already_warned` flag)
  - `Cap` → `stop_transcribe` (keep the take)
- if `stream_error` → `cancel_recording` + emit recording false + toast-worthy event

`test_microphone` must use the same device resolver as capture.

### Download cancel

```
#[tauri::command]
async fn cancel_download(state, id: String) -> Result<(), String>
```

Insert `Arc<AtomicBool>` when download starts. Stream loop: if flag, delete
the current `.part`, return `Err("download cancelled")`. Already-finished
files in the bundle stay (resume-friendly). Always remove the map entry in
the existing cleanup block. Frontend: Cancel button next to progress; on
`cancelled` drop `dlBars[id]` and refresh.

### Cancel dictation (UI + Esc)

```
#[tauri::command]
async fn cancel_dictation(app, state) -> Result<String, String>
```

If not recording → error. Else `cancel_recording`, emit `recording=false`.
Overlay + Dictate Cancel button call this.

Hotkey thread: poll `VK_ESCAPE` (`0x1B`) **only when `state.recording`**.
New edge `TalkEdge::Escape` → cancel **any** take (UI-started or hotkey).
Existing `TalkEdge::Cancel` (other key while held) stays `hotkey_owned`-only
so typing during a UI recording does not nuke it.

### Copy / paste last

```
fn last_text(state) -> Option<String>  // history.first().text
#[tauri::command] fn copy_last(state) -> Result<String, String>   // clipboard only
#[tauri::command] async fn paste_last(state) -> Result<String, String>  // reuse paste_text()
```

Empty history → `"nothing to paste — dictate first"`.

Register in setup after `load_settings`, re-register on `set_settings`.
Store the currently registered `Shortcut`s on `AppState` (or a small
`utility_shortcuts: Vec<Shortcut>`) so you can unregister the old pair
before the new pair. Extend the global-shortcut handler **or** use
`GlobalShortcutExt::on_shortcut` after settings load (cleaner — the builder
handler is wired before settings exist).

After a successful `stop_transcribe` (paste or not), emit:

```
dictflow://committed { text: String }
```

Overlay starts the 10 s copy chip from that event.

---

## 7. Overlay window (item 6)

This is the largest piece. It is a **second WebView**, not HTML inside `main`.

### Config

`tauri.conf.json` → `app.windows` add:

```
label: overlay
url: overlay.html
width: 320, height: 56
visible: false
decorations: false
transparent: true
alwaysOnTop: true
skipTaskbar: true
resizable: false
shadow: false
```

`vite.config.ts` rollup `input`: `main` → `index.html`, `overlay` → `overlay.html`.
Dev URL becomes `http://localhost:1420/overlay.html`.

### Capabilities (`default.json`)

```
"windows": ["main", "overlay"]
```

Add (on top of `core:default`):

```
core:window:allow-start-dragging
core:window:allow-set-ignore-cursor-events
core:window:allow-set-position
core:window:allow-outer-position
core:window:allow-current-monitor
core:window:allow-primary-monitor
core:window:allow-available-monitors
core:window:allow-set-size
core:window:allow-show
core:window:allow-hide
core:window:allow-set-always-on-top
```

### Files to create

- `overlay.html` — empty shell, loads `/src/overlay.ts`
- `src/overlay.ts` — listen to events, invoke cancel/copy_last, drag + snap
- `src/overlay.css` — transparent page, pill chrome matching `--ink` / `--red`

`overlay.ts` phases: `idle` | `recording` | `transcribing` | `copychip`.

Click-through: `getCurrentWindow().setIgnoreCursorEvents(true)` only in
**idle** (no chip). Recording / transcribing / copy-chip → interactive
(cancel, copy, drag). Idle + click-through means **no drag while idle**;
reposition via Settings edge picker, or drag while the pill is interactive.
That is an accepted P1 tradeoff (documented in Settings copy).

On drag end:

```
outerPosition + currentMonitor.size
  → invoke snap_overlay
  → setPosition
  → persist overlay_edge + overlay_offset via set_settings (or a tiny
     set_overlay_pose command that does not revalidate the whole Settings)
```

On setup: if `overlay_enabled`, show window and `pose_to_xy` from settings.
Toggle in Settings shows/hides the window (`app.get_webview_window("overlay")`).

Pill UI (keep tiny):

- Idle: gray dot + “DictFlow”
- Recording: red pulse + `m:ss` + 4–5 level bars + Cancel
- Transcribing: amber “Transcribing…”
- Copy chip: last-text trunc + Copy (10 s)

Do not put the main-app sidebar or router in this window.

---

## 8. Frontend changes (`src/main.ts` + `style.css`)

### Onboarding (item 11)

If `settings && !settings.onboarded`, `render()` draws a full-page wizard
**without the sidebar**. Steps match `OnboardStep`. Skip and Finish both
`set_settings({ …settings, onboarded: true })` then `view = "home"`.

- Welcome — one paragraph, local-first promise, Continue / Skip
- Talk key — reuse `HOTKEYS` + hold/toggle selects (same as Settings)
- Mic — device list (selectable), Test 1.5 s, Open Windows mic settings
- Download — recommended card = `parakeet-v3` (or `status.model_id` if
  already downloaded). Reuse `download_model` + progress events. Continue
  enabled when that model is `downloaded` **or** user clicks Skip

### Dictate

- Cancel button visible while `status.recording`
- Live level bar under the mic (same `dictflow://level` event)
- At 19 min: toast from `dictflow://session-warn` (“19 minutes — still
  recording. Esc cancels, talk key finishes.”)
- Hint line: “Esc cancels this take”

### Setup / Settings

- Device rows become radios / click-to-select; write `audio_device`
  (`null` = “Windows default”)
- Show fallback banner if preferred is missing from the list
- Settings cards: Overlay on/off + edge; Paste-last / Copy-last dropdowns
  (safe combos only — `Shift+Alt+Z/X`, `Ctrl+Alt+Z/X`, `Ctrl+Shift+Z/X`);
  Session cap checkbox
- `saveSettings` **must** `{ ...settings, ...changed }`

### Models

Downloading card: progress + **Cancel** (`data-cancel-dl`). Keep Download
disabled only for the in-flight file, not forever.

### Events to subscribe (boot)

| Event | Payload | UI |
|---|---|---|
| `dictflow://level` | `{ peak, rms }` | Dictate meter + overlay bars |
| `dictflow://committed` | `{ text }` | `lastResult`, overlay chip |
| `dictflow://session-warn` | `{}` | toast |
| `dictflow://device-fallback` | `{ from, to }` | toast + Setup banner |

Existing five events stay as they are.

---

## 9. Command checklist (register all of these)

Existing stay. Add:

- `cancel_dictation`
- `cancel_download`
- `copy_last`
- `paste_last`
- `snap_overlay` (pure; also used by tests via the module)
- `set_overlay_pose` (optional, if you don’t want a full `set_settings` from the pill)

`get_audio_devices`: add `is_selected: bool` (name == preferred, or
`is_default` when preferred is `None`).

---

## 10. Implementation order (do this, in this order)

Do not start with the overlay window. The first five slices are testable
without WebView.

```
1. session.rs + tests
2. shortcuts.rs + tests
3. devices.rs + tests
4. overlay.rs (geometry + chip) + tests
5. onboarding.rs + tests
6. Settings fields + Default/serde + set_settings validation + test_state()
7. run_capture device resolve + fallback event + test_microphone uses it
8. level ticker + session ticker + stream_error
9. cancel_dictation command + Esc TalkEdge + Dictate Cancel button
10. copy_last / paste_last + apply_utility_shortcuts
11. download cancel flag + Models Cancel button
12. Onboarding wizard in main.ts
13. Overlay window + overlay.html/ts/css + capabilities + vite input
14. Settings UI for overlay / shortcuts / mic / session_cap
15. cargo test -p dictflow
16. Mark P1 shipped in WISPR_FLOW_GAP_ANALYSIS.md §6 (same style as P0)
```

Each of 1–5 should compile and pass tests before touching I/O.

---

## 11. Test inventory (write these)

| Module | Cases |
|---|---|
| `session` | 1139 s → None; 1140 s → Warn; second tick still warned → None; 1200 s cap off → None; 1200 s cap on → Cap |
| `shortcuts` | `Shift+Alt+Z`; `shift+alt+z`; bare `Z` err; `Ctrl+Alt+Space` rejected as utility; paste≠copy; format round-trip |
| `devices` | preferred present; preferred missing → fallback; None preferred → default; empty list err; rms/peak of `[0,0]`, `[0.5,-1.0]`, `[]` |
| `overlay` | snap from top-center; snap from left; clamp; pose_to_xy; chip 9999 ms yes / 10001 ms no |
| `onboarding` | next/prev ends; recommended id is `parakeet-v3` |
| `main` (extend) | `test_state` still builds; settings missing new fields deserialize; `onboarded` missing → true; `Settings::default().onboarded` → false; cancel does not increment `stats` (call `cancel_recording` on a dummy active if you can; otherwise test `cancel_recording` only leaves history/stats untouched — you may need a small helper that skips real cpal) |

Do not try to unit-test cpal, SendInput, or the WebView. Those are manual.

Manual smoke (after `cargo test` is green):

1. Fresh data dir → wizard → skip → home. Delete `onboarded` from a real
   settings.json → restart → **no** wizard.
2. Hold talk key, hit Esc → no paste, no new history row.
3. Dictate once, `Shift+Alt+X` copies, `Shift+Alt+Z` pastes into Notepad.
4. Pick a non-default mic in Setup, test, dictate; unplug it, next dictate
   still works (default) + toast.
5. Start a model download, Cancel → card is Download again, no `.part` left
   under `%APPDATA%/dictflow/models/<id>/`.
6. Record past a few seconds, pill shows timer + bars; after paste, Copy
   chip ~10 s then idle click-through.
7. (Optional) set session_cap, we cannot sit 20 min in CI — trust unit tests.

---

## 12. Design constraints (do not fight these)

- `cpal::Stream` and `sherpa_onnx::OfflineRecognizer` are `!Send`. Capture
  and Parakeet each own a thread for life. See `ARCHITECTURE.md`.
- Static CRT (`+crt-static`) — do not add a crate that pulls a different CRT.
- `RegisterHotKey` cannot bind bare modifiers. Talk key stays polled.
  Utility shortcuts are real plugin shortcuts (they have modifiers).
- Overlay click-through vs drag: idle is click-through; drag only when the
  pill is interactive. Edge is also settable in Settings.
- Clipboard restore is text-only (arboard). Unchanged.
- No new dependencies unless a unit test truly needs one. Shortcut parsing
  is a 40-line matcher; do not pull a crate for it.

---

## 13. What a new agent should open first

1. This file.
2. `docs/WISPR_FLOW_GAP_ANALYSIS.md` §6 P1 + §2.2 (overlay feel) — product.
3. `docs/ARCHITECTURE.md` — constraints.
4. Then jump to the symbol table in §2 and start at §10 step 1.

Do **not** re-read SpeakType / Wispr websites. Do **not** port wookat
webview STT. Do **not** start P2/P3 in the same PR.

When P1 is merged, add a one-liner at the top of §6 P1 in the gap analysis
(`Shipped YYYY-MM-DD`, same as P0) and set the Status line in this file to
**shipped**.
