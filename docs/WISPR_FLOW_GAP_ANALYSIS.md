# Wispr Flow vs DictFlow — Feature Research & Gap Analysis

Researched 2026-09-13 against Wispr Flow’s public site, changelog, and Help Center,
and against DictFlow as it exists in this repo today.

DictFlow is a **local-only, MIT, Windows** voice-dictation app. Wispr Flow is a
**cloud-backed, multi-platform commercial** product. This document maps every
user-facing Wispr Flow capability, marks what DictFlow already has, and lists
what is missing — with a concrete plan for the dashboard / minutes-consumed
gap the product currently does not show.

Sources:

- https://wisprflow.ai/features
- https://wisprflow.ai/pricing
- https://wisprflow.ai/whats-new
- https://docs.wisprflow.ai (Help Center, Apr–Aug 2026 articles)
- `docs/SPEAKTYPE_ANALYSIS.md` (macOS SpeakType port we already shipped)
- `src/main.ts`, `src-tauri/src/main.rs`, `src-tauri/src/text.rs`

---

## 1. What Wispr Flow actually is

Wispr Flow is a system-wide voice-to-text layer: hold a shortcut, speak, and
polished text lands in whichever app has focus. The product pitch is **4×
faster than typing**, with AI cleanup, personalization, and (as of August 2026)
a meeting Notetaker on top of dictation.

It ships on **Mac, Windows, iPhone, and Android**. Free includes dictation
with weekly caps (2,000 words/week desktop, 1,000/week iPhone, unlimited
Android). Pro is $15/user/mo monthly ($12 annual) for unlimited dictation,
teams, and longer Notetaker history.

Everything after model download in DictFlow is offline. Almost everything
that makes Wispr Flow *feel* magical — styles, transforms, backtrack,
context-aware names, command mode, Insights — is a **cloud LLM pass** on
the raw transcript plus account sync. That is the split this analysis
respects: we port **behavior**, not their servers.

---

## 2. Wispr Flow feature inventory

Grouped the way the product presents itself. Each item is tagged
**have** / **partial** / **missing** / **out of scope** for DictFlow.

### 2.1 Core dictation loop

| Feature | Wispr Flow | DictFlow | Status |
|---|---|---|---|
| Global push-to-talk into any text field | Fn / Ctrl+Win, hold or toggle | Right Ctrl (default), Scroll Lock, F9, Left Ctrl, Ctrl+Alt+Space; hold or toggle | **have** |
| Hands-free mode (separate start/stop shortcut) | Fn+Space / Ctrl+Win+Space | Same talk key in toggle mode only | **partial** |
| Cancel in-flight dictation | Esc, rebindable | Stop via talk key; no dedicated cancel that discards audio | **partial** |
| Auto-paste into focused app | Synthetic paste + clipboard restore | Ctrl+V via SendInput + text-only clipboard restore | **have** |
| Paste last transcript shortcut | Shift+Alt+Z (Windows) | Copy button on last result / history only | **missing** |
| Copy last transcript shortcut | Shift+Alt+X (Windows) | Manual copy in UI | **missing** |
| Mouse-button push-to-talk (Mouse Flow) | Bind Mouse 4–10 + middle click | Keyboard only | **missing** |
| 20-minute max session + 19-minute warning | Desktop | No duration cap, no live timer in UI | **partial** (unlimited; no warning) |
| Recover dictation after quit / crash | Audio saved, Recover in History | WAV is written on stop; crash mid-record loses the take | **partial** |
| Retry failed transcription from history | One-tap retry, orange failed row | Failed takes are not stored as retryable items | **missing** |
| Whisper / quiet-room dictation | Marketed as a model capability | Depends on the local model; no VAD / noise profile | **partial** |
| 100+ languages | Cloud models | Parakeet v3 = 25 langs; Whisper catalog is tiny/base/small + `.en` | **partial** |
| Cross-device sync | Account + Cloud Sync | Local JSON only, by design | **out of scope** |

### 2.2 The overlay (Flow Bar / bubble)

Wispr’s always-on-top **Flow Bar** is the product’s face: idle / listening /
transcribing, live timer, language picker, transform wand, copy-last chip,
right-click menu (hide 1 hour, mic picker, paste last), dockable to any
screen edge, opacity, auto-shrink.

DictFlow has a **tray icon + main window**. No floating pill, no live level
meter, no language chip, no copy-last overlay.

Status: **missing** (called out as next work in `SPEAKTYPE_ANALYSIS.md`).

### 2.3 Text intelligence (the “Flow edits while you speak” layer)

This is the biggest quality gap. Wispr runs an LLM rewrite on every take.

| Feature | What Wispr does | What DictFlow does | Status |
|---|---|---|---|
| Auto Cleanup | Four levels: None / Light / Medium / High. Light = fillers + grammar. Medium = clarity. High = rewrite for polish. Original kept; “Undo AI edit” in history. | Three local levels: `off` / `light` (fillers) / `full` (fillers + punctuation tidy). No raw-vs-cleaned pair stored. | **partial** |
| Smart Formatting | Punctuation, capitalization, numbered lists from “one… two…”, spoken punctuation (“comma”, “new paragraph”), messaging-app trailing-period strip, context casing from surrounding text | `tidy_punctuation` + `smart_trailing_punctuation` only. No spoken punctuation commands, no lists, no context casing. | **partial** |
| Backtrack | “actually 3”, “scratch that”, “never mind”, or a restated phrase rewrites the earlier clause | Not implemented | **missing** |
| Context-aware names / jargon | Reads surrounding field text + personal/team dictionary so uncommon names land correctly | Dictionary replacements after STT; no field-context read | **partial** |
| App-aware Styles | Personal / Work / Email / Other × casual→formal. Detects Slack vs Gmail vs Cursor by process + URL. English + desktop only. | None | **missing** |
| Snippets (voice shortcuts) | Separate from Dictionary. Trigger ≤60 chars → expansion ≤4,000 chars, rich text on desktop. | Dictionary *is* the snippet system (trigger → replacement). No rich text, no length UX, no dedicated Snippets view. | **partial** |
| Dictionary auto-learn | When you correct a spelling, Flow adds it. Star / usage-rank / apostrophe-variant filter. | Manual add + symbol-preset + JSON import/export. No auto-learn, no star, no usage rank. | **partial** |
| Spoken punctuation + newline commands | Long built-in list (period, em dash, new line, degrees celsius, …) | Symbol presets in dictionary (`at` → `@`) if the user adds them | **partial** |
| Transforms (Beta) | Highlight text → Polish / Prompt Engineer / custom prompt. Optional auto-apply after every dictation. | None. Would need a local LLM. | **missing** (local-LLM polish is already on the SpeakType next-list) |
| Command Mode | Hold Ctrl+Win+Alt, speak an instruction, Flow executes it (paid). | None | **missing** (local-LLM) |
| File tagging / syntax / variable recognition | IDE-aware: file names, code syntax, vibe-coding variables | None | **missing** |
| Banking-app privacy pause | Auto-pauses in 50+ finance apps | None | **missing** (nice-to-have on Windows) |

### 2.4 History, notes, meetings

| Feature | Wispr Flow | DictFlow | Status |
|---|---|---|---|
| Dictation history with search, copy, delete | Yes | Yes | **have** |
| Per-row audio playback | Last 14 days | Per-item WAV, kept with the row | **have** (stricter: we keep the file until the row is deleted) |
| Undo AI edit / show raw transcript | Yes | Raw text is discarded after cleanup | **missing** |
| Scratchpad (floating notepad, Opt+S / Win+Alt+S) | Rich text, tabs, version history | None | **missing** |
| Notetaker (meetings, speaker labels, summaries, Q&A, MCP) | Mac now, Windows “coming soon” | None | **out of scope** for v1 local dictation (huge; needs diarization + LLM) |
| Calendar / Slack connect, pre-meeting briefs | Pro / team | None | **out of scope** |
| Transcript retention policy | Save / auto-delete 24h / never store | History capped at `HISTORY_LIMIT`; clear-all wipes stats | **partial** |

### 2.5 Insights / usage dashboard (the gap you called out)

Wispr Flow’s **Insights → Your Usage** (Mac/Windows desktop, April 2026) is
the product dashboard. It is *not* a billing meter. It is a personal
impact report.

**Tiles Wispr shows today**

1. **Words per minute** — `words / audio_minutes`, rounded, plus a
   percentile against published *typing* speeds (not against other Flow
   users). Updates after every take.
2. **Corrections by Flow** — words Auto Cleanup removed or rewrote, plus
   dictionary / snippet substitutions. Hidden when both are zero.
3. **Total words dictated** — lifetime count + “+% this month” vs the
   same number of days last month. Desktop vs mobile bar if they have
   both. Fun comparison tagline (“you’ve written N tweets”).
4. **Desktop app breakdown** — personal messages / work messages / email /
   AI prompts / documents / other. Intensity labels at 1k / 10k words.
5. **Usage streak heatmap** — Sun–Sat GitHub-style grid. Buckets:
   0 / 1–249 / 250–499 / 500–749 / 750+ words/day. Current + longest
   streak. Hover = words, apps, top app that day.

**Team / Enterprise Insights (admin web, not the personal app)**

- Time Saved: `words × (1/40 − 1/150)` minutes — typing at **40 WPM** vs
  dictating at **150 WPM**.
- Money Saved: trailing-29-day minutes × hourly rate, minus Flow subscription.
- Words, active users, top apps, dictionary items added, automatic fixes,
  per-member table, CSV export, leaderboard.

SpeakType (the macOS app we ported the rest of the UI from) used a
simpler personal formula on its Dashboard home:

- greeting
- total words
- **time saved @ 40 WPM** (`words / 40` minutes)
- today / all-time dictation counts
- 7-day activity chart

**What DictFlow Statistics shows today** (`viewStats` in `src/main.ts`)

| Tile | Computed from | Gap |
|---|---|---|
| Dictations | `history.length` | No today / week / month split |
| Words dictated | `sum(wordCount(text))` | Lifetime only; no WPM, no month-over-month |
| Audio recorded | `sum(duration_secs)` via `fmtDur` | Shows `12.3m` or `4.2s` — not “minutes consumed”, not hours+minutes, not today-vs-all |
| Dictionary rules | `dict.length` | Count of rules, not how often they fired |
| Dictations per model | group-by `model` | Useful; keep |

Also:

- Clearing history **zeros the stats**. Wispr keeps Insights even if you
  delete rows (they aggregate server-side).
- `duration_secs` is already on every `HistoryItem` and is written at
  commit time. The data for minutes consumed **already exists**. The
  dashboard just does not present it.
- We do not store `raw_text`, `cleaned_text`, `words_removed`,
  `dictionary_hits`, or `target_app`, so several Wispr tiles cannot be
  computed until the history schema grows.
- There is no Dashboard *home* (greeting + hero numbers). Statistics is
  a side nav item with four tiles and a model table. No chart.

### 2.6 Audio, models, setup

| Feature | Wispr Flow | DictFlow | Status |
|---|---|---|---|
| Mic list + pick a device | Ranked list; auto-fallback when a headset is plugged/unplugged; clamshell warning | Lists devices + 1.5s test on the **OS default** only | **partial** |
| Live input meter while recording | Flow Bar waveform | None | **missing** |
| Clear mic-error taxonomy | Unplugged / in use / blocked | Setup test covers silence vs no-callbacks; no in-dictation taxonomy | **partial** |
| Model catalog + download progress | Cloud models, user never picks weights | Parakeet v2/v3 int8 + Whisper tiny/base/small; progress, delete, warm-up | **have** (different product: we *are* the model picker) |
| Cancel in-flight download | Yes | Not wired | **missing** (SpeakType next-list) |
| Recommended-model hero (RAM / use-case) | N/A (cloud) | Scores exist on cards; no “recommended for this PC” banner | **partial** |
| First-run onboarding | Guided tours, permissions, style wizard | Setup view exists; no first-launch wizard | **partial** |

### 2.7 App chrome Wispr has that we do not

- Insights / Style / Transforms / Scratchpad / Snippets as first-class
  sidebar tabs (we merge snippets into Dictionary).
- Notification categories (tips, milestones, formatting reminders) with
  per-category mute.
- Hide / snooze overlay for 1 hour.
- Start-at-login (we have this).
- Privacy: Privacy Mode, Cloud Sync, HIPAA BAA, never-store-transcripts.
  Our equivalent is “nothing leaves the PC” — already stronger.
- Team shared dictionary / snippets, admin usage, SSO, SCIM, MDM.
- iOS / Android / keyboard IME / Flow Bubble.
- Accounts, plans, weekly word caps.

All of the team / compliance / mobile / account items are **out of scope**.

---

## 3. Minutes consumed — exact definitions we should ship

Three numbers people mean when they say “how much have I used”:

### A. Minutes consumed (audio)

Wall-clock microphone time.

```
audio_seconds  = Σ history.duration_secs
audio_minutes  = audio_seconds / 60
```

Format as `Xh Ym` once ≥ 60 minutes, else `Ym Zs`. This is the number
that is **missing as a first-class dashboard metric**. We already sum
`duration_secs` but bury it under “Audio recorded” with a one-decimal
`fmtDur`.

Also break out **today / last 7 days / this month / all time**.

### B. Dictation speed (WPM)

```
wpm = total_words / (audio_minutes)     # ignore items with duration_secs == 0
```

Wispr rounds to a whole number. SpeakType did not show this. We should.

### C. Time saved

Two honest formulas. Pick one and label it in the UI.

| Name | Formula | What it claims |
|---|---|---|
| **SpeakType / conservative** | `time_saved_min = words / 40` | “If you had typed this at 40 WPM.” Simple, matches the app we ported. |
| **Wispr Flow Impact** | `time_saved_min = words × (1/40 − 1/150)` | Incremental savings vs typing, assuming you dictate at 150 WPM. Smaller number, closer to their marketing. |

Recommendation: **ship both tiles**.

- *Time saved (vs typing @ 40 WPM)* — `words / 40`
- *Minutes spoken* — definition A
- *Your pace* — definition B

Optional later: Wispr’s incremental formula as a tooltip on Time saved
(“Wispr reports X using 40 vs 150 WPM”).

Worked example: 4,000 words over 28 minutes of audio.

| Metric | Value |
|---|---|
| Minutes consumed | 28.0 |
| WPM | 143 |
| Time saved @ 40 WPM | 100.0 min (1h 40m) |
| Wispr incremental save | 73.3 min |

### What we must stop doing

Clearing history currently resets Statistics. Minutes consumed only
means something if it **survives Clear all**. Persist a running
`UsageTotals` blob (`stats.json`) that increments on every commit and
never decrements on delete. History remains the detail log; stats are
the ledger.

---

## 4. Schema changes the dashboard needs

`HistoryItem` today:

```
text, date_unix, duration_secs, model, audio_path?
```

Add (all optional, defaulted, forward-compatible — same pattern as
`audio_path`):

```
raw_text: String          # pre-cleanup, for Undo + “words cleaned”
words_out: u32            # cached so we do not re-split in the UI
dict_hits: u32            # how many dictionary rules fired
cleanup: String           # off | light | full
target_app: Option<String> # process name that received the paste, if we can see it
```

New `stats.json` (never wiped by Clear history):

```
lifetime_dictations: u64
lifetime_words: u64
lifetime_audio_secs: f64
lifetime_raw_words: u64
lifetime_dict_hits: u64
by_day: { "YYYY-MM-DD": { dictations, words, audio_secs, dict_hits } }
```

`target_app` is the only field that needs new platform work
(`GetForegroundWindow` + `GetWindowThreadProcessId` +
`QueryFullProcessImageNameW` at paste time). Everything else is
bookkeeping we can do in the existing commit path.

---

## 5. Prioritized backlog

Scoped to a **local Windows** product. Cloud, teams, accounts, HIPAA,
Notetaker, mobile, and SSO are listed only so we do not accidentally
chase them.

### P0 — Dashboard that tells you what you used

This is the hole you noticed. Ship it first. No new engines, no LLM.

1. **Usage ledger** (`stats.json`) that increments on every successful
   take and is independent of History.
2. **Statistics redesign** (keep the nav name, or rename to **Insights**):
   - Hero row: Minutes consumed · Words · Time saved @ 40 WPM · WPM
   - Period chips: Today / 7 days / 30 days / All time
   - Today vs all-time dictation counts
   - 7-day (then 30-day) bar chart of minutes **and** words
   - Streak heatmap (Wispr buckets, local only)
   - Per-model table (already have — keep)
   - Dictionary hits (once `dict_hits` is stored; hide at zero)
3. **Dashboard home** on first launch / default view: greeting, the four
   hero numbers, last 5 transcripts, “hold Right Ctrl to talk”.
4. **Live tick** while recording: running seconds on the Dictate view
   (and later on the pill).
5. Store `raw_text` so a later “Undo cleanup” and a “words cleaned”
   tile are possible without a second schema bump.

Acceptance: after ten takes, Statistics shows minutes consumed for
today and all-time, time saved, and WPM, and **Clear all does not
zero the lifetime tiles**.

### P1 — Overlay + recovery (Wispr’s daily feel)

6. Floating always-on-top pill: idle / recording / transcribing, drag to
   dock, click-through when idle, copy-last chip for ~10 s after paste.
7. Dedicated cancel (Esc) that drops the take without pasting.
8. Paste-last and copy-last global shortcuts (Wispr defaults:
   Shift+Alt+Z / Shift+Alt+X), user-rebindable.
9. Mic picker (not just default) + live level meter + headset
   plug/unplug fallback.
10. Cancel model download.
11. First-run onboarding: Welcome → talk-key → mic test → download
    recommended model.
12. Session warning at 19 minutes (optional hard cap at 20, matching
    Wispr; we can also leave it unlimited and only warn).

### P2 — Text quality we can do offline (no LLM)

These close the “Flow edits while you speak” gap with rules, not a
cloud model.

13. Spoken punctuation + newline command table (port Wispr’s list).
14. Numbered / bulleted list formatting from “one… two…” / “first…”.
15. Backtrack v1: regex / clause rewrite for “actually X”, “scratch
    that”, “never mind”, “I mean X”. Keep a conservative matcher so
    “I actually enjoyed it” survives.
16. Split **Snippets** from Dictionary (long expansions vs vocab
    boosts). Same JSON store, two tabs. Star + usage-count sort.
17. Auto-add to dictionary from an in-history “this should have been X”
    correction (no cloud learn).
18. Auto Cleanup fourth level stays blocked until a local LLM exists;
    rename our three levels in the UI to Wispr’s None / Light / Full
    wording so the setting is recognizable.

### P3 — Local LLM polish (the remaining magic)

19. Optional sidecar (llama.cpp / a small instruct model) for:
    - Auto Cleanup Medium / High
    - Transforms: Polish, Prompt Engineer, user prompts
    - Command Mode (open URL, insert date, “make this a bullet list”)
    - Undo AI edit already works if we stored `raw_text` in P0
20. App-aware Styles as **templates** (casual / formal / email /
    prompt) applied by detected foreground process. Detection is local;
    the rewrite is the local LLM. English-first, same constraint Wispr
    documents.
21. Context-aware casing: read a small clipboard / accessibility
    snapshot of the field *before* paste (Windows UI Automation). Hard;
    do after the pill.

### P4 — Platform completeness

22. Recommended-model hero from RAM + CPU.
23. Larger Whisper catalog (medium / large-v3-turbo) and Parakeet CTC-110M.
24. GPU path (DirectML / CUDA) for Whisper and sherpa-onnx.
25. NSIS installer + in-app updater (already sketched in `docs/RELEASE.md`).
26. SQLite instead of `history.json` once the ledger exists.
27. Video / mp3 file transcription (SpeakType Transcribe Audio).
28. Customizable talk-key capture (any combo, not a dropdown).
29. Mouse Flow.
30. Banking-app pause list (process-name denylist).

### Explicitly not porting

- Accounts, plans, weekly word caps, Polar / trials
- Cloud Sync, Privacy Mode-as-training-opt-out, HIPAA BAA
- Team shared dictionaries, admin Insights, leaderboards, SSO, SCIM
- Notetaker, MCP meeting tools, calendar / Slack
- iOS / Android / IME keyboard
- Share-to-social Insight cards, global WPM percentile vs a typing study
- Their servers, their models

---

## 6. Suggested implementation order (when we start)

Do P0 as one slice. It is entirely frontend + a small Rust persist
change. No new dependencies.

```
stats.json ledger
  → HistoryItem.raw_text + dict_hits
  → Statistics / Insights view (minutes, WPM, time saved, periods, 7-day chart)
  → Dashboard home as default view
  → live recording timer on Dictate
```

Then P1 pill + shortcuts (the “it feels like Wispr” layer), then P2
offline text rules (the “it writes like Wispr” layer). P3 waits on a
local LLM decision.

---

## 7. One-page scoreboard

| Area | Wispr Flow | DictFlow now | Next move |
|---|---|---|---|
| Hold key → paste anywhere, offline | Cloud STT | Local Parakeet / Whisper | Done |
| Dictionary / snippets | Two systems + auto-learn + teams | One system, manual | Split + auto-learn (P2) |
| Cleanup | 4 LLM levels + undo | 3 rule levels, no raw | Store raw; LLM later |
| Smart formatting / backtrack | Full | Tidy + trailing `.` | Rule port (P2) |
| Overlay | Flow Bar | Tray + window | Pill (P1) |
| **Usage dashboard** | Insights: WPM, words, minutes-as-heatmap, time saved (team), streaks | 4 lifetime tiles, no minutes hero, no periods, wiped on clear | **P0** |
| Recovery | Retry, crash recover, 20-min cap | WAV on success only | Cancel + retry (P1) |
| Mic | Ranked + fallback + meter | Default + test | Picker + meter (P1) |
| Styles / Transforms / Command | Cloud LLM | — | Local LLM (P3) |
| Scratchpad / Notetaker / teams | Yes | — | Out of scope |

The product works. The two things a Wispr user will notice first are:
**(1) no minutes / time-saved / WPM on the dashboard**, and **(2) the
transcript is cleaned by regex, not rewritten**. P0 fixes (1). P2 and
P3 fix (2).
