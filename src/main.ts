import "./style.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import logoUrl from "../assets/logo-512.png";
import { open, save } from "@tauri-apps/plugin-dialog";
import { attachConsole, error as logError } from "@tauri-apps/plugin-log";
import { enable as autostartEnable, disable as autostartDisable, isEnabled as autostartIsEnabled } from "@tauri-apps/plugin-autostart";

// ---------------------------------------------------------------------------
// Types (mirror src-tauri)
// ---------------------------------------------------------------------------

type Engine = "whisper" | "parakeet";
type View = "dictate" | "models" | "setup" | "history" | "dictionary" | "stats" | "settings";

interface ModelFile {
  name: string;
  url: string;
  expected_bytes: number;
}
interface ModelStatus {
  id: string;
  name: string;
  engine: Engine;
  languages: string;
  size_label: string;
  speed: number;
  accuracy: number;
  essence: string;
  min_ram_gb: number;
  files: ModelFile[];
  downloaded: boolean;
  active: boolean;
  downloading: boolean;
}
interface HistoryItem {
  text: string;
  date_unix: number;
  duration_secs: number;
  model: string;
  audio_path: string | null;
}
interface DictionaryEntry {
  id: string;
  trigger: string;
  replacement: string;
  enabled: boolean;
  whole_word: boolean;
}
interface Settings {
  model_id: string;
  language: string;
  auto_paste: boolean;
  cleanup: string;
  translate: boolean;
  hotkey_key: string;
  recording_mode: string;
}
interface Status {
  recording: boolean;
  model_id: string;
  engine: Engine;
  engine_label: string;
  model_loaded: boolean;
  whisper_binary: string | null;
  data_dir: string;
  audio_device: string | null;
  version: string;
}
interface DlProgress {
  model_id: string;
  file: string;
  received: number;
  total: number | null;
}
interface UpdateInfo {
  current: string;
  latest: string | null;
  url: string | null;
  available: boolean;
  configured: boolean;
}
interface AudioDeviceInfo {
  name: string;
  sample_rate: number;
  channels: number;
  is_default: boolean;
}
interface MicTest {
  duration_secs: number;
  peak: number;
  samples: number;
  device: string;
  sample_rate: number;
  channels: number;
  callbacks: boolean;
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

let view: View = "dictate";
let status: Status | null = null;
let models: ModelStatus[] = [];
let history: HistoryItem[] = [];
let dict: DictionaryEntry[] = [];
let settings: Settings | null = null;
let transcribing = false;
let lastResult: string | null = null;
let modelFilter: "all" | Engine = "all";
let historyQuery = "";
let dlBars: Record<string, { file: string; pct: number | null }> = {};
let audioDevices: AudioDeviceInfo[] | null = null;
let micTest: MicTest | null = null;
let micTesting = false;

const NAV: { id: View; label: string; ico: string }[] = [
  { id: "dictate", label: "Dictate", ico: "🎙" },
  { id: "models", label: "AI Models", ico: "🧠" },
  { id: "setup", label: "Setup", ico: "✅" },
  { id: "history", label: "History", ico: "🕘" },
  { id: "dictionary", label: "Dictionary", ico: "📖" },
  { id: "stats", label: "Statistics", ico: "📊" },
  { id: "settings", label: "Settings", ico: "⚙" },
];

const LANGUAGES: [string, string][] = [
  ["auto", "Auto-detect"],
  ["en", "English"],
  ["de", "German"],
  ["fr", "French"],
  ["es", "Spanish"],
  ["it", "Italian"],
  ["pt", "Portuguese"],
  ["nl", "Dutch"],
  ["hi", "Hindi"],
];

const CLEANUPS: [string, string][] = [
  ["full", "Full — fillers + tidy"],
  ["light", "Light — fillers only"],
  ["off", "Off — raw transcript"],
];

const HOTKEYS: [string, string][] = [
  ["RightCtrl", "Right Ctrl (hold)"],
  ["LeftCtrl", "Left Ctrl ⚠ (cancels on Ctrl+key)"],
  ["ScrollLock", "Scroll Lock"],
  ["F9", "F9"],
  ["CtrlAltSpace", "Ctrl + Alt + Space"],
];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function esc(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function fmtDate(ts: number): string {
  if (!ts) return "imported";
  return new Date(ts * 1000).toLocaleString();
}

function fmtDur(sec: number): string {
  if (!sec) return "—";
  return sec < 60 ? `${sec.toFixed(1)}s` : `${(sec / 60).toFixed(1)}m`;
}

function wordCount(t: string): number {
  return t.split(/\s+/).filter(Boolean).length;
}

function toast(msg: string, kind: "info" | "success" | "error" = "info"): void {
  const box = document.getElementById("toasts")!;
  const el = document.createElement("div");
  el.className = `toast ${kind}`;
  el.textContent = msg;
  box.appendChild(el);
  setTimeout(() => el.remove(), kind === "error" ? 7000 : 4000);
}

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T | null> {
  try {
    return await invoke<T>(cmd, args);
  } catch (e) {
    logError(`invoke ${cmd} failed: ${e}`);
    toast(`${e}`, "error");
    return null;
  }
}

// ---------------------------------------------------------------------------
// Data loading
// ---------------------------------------------------------------------------

async function refreshStatus(): Promise<void> {
  const s = await call<Status>("get_status");
  if (s) {
    status = s;
    paintStatusBits();
  }
}

async function refreshModels(): Promise<void> {
  const m = await call<ModelStatus[]>("get_models");
  if (m) {
    models = m;
    if (view === "models") renderView();
    paintStatusBits();
  }
}

async function refreshHistory(): Promise<void> {
  const h = await call<HistoryItem[]>("get_history");
  if (h) {
    history = h;
    if (view === "history") renderHistoryList();
    if (view === "stats") renderView();
    if (view === "dictate") renderView();
  }
}

async function refreshDict(): Promise<void> {
  const d = await call<DictionaryEntry[]>("get_dictionary");
  if (d) {
    dict = d;
    if (view === "dictionary") renderView();
  }
}

async function refreshSettings(): Promise<void> {
  const s = await call<Settings>("get_settings");
  if (s) {
    settings = s;
    if (view === "settings") renderView();
  }
}

async function refreshAudioDevices(): Promise<void> {
  const d = await call<AudioDeviceInfo[]>("get_audio_devices");
  if (d) {
    audioDevices = d;
    if (view === "setup") renderView();
  }
}

/** Display name of the configured talk key. */
function talkKey(): string {
  const k = settings?.hotkey_key ?? "RightCtrl";
  return k === "RightCtrl"
    ? "Right Ctrl"
    : k === "LeftCtrl"
      ? "Left Ctrl"
      : k === "ScrollLock"
        ? "Scroll Lock"
        : k === "F9"
          ? "F9"
          : "Ctrl + Alt + Space";
}

/** True when the talk key is a hold-to-talk single key (macOS Fn feel). */
function talkHold(): boolean {
  return (
    (settings?.recording_mode ?? "hold") === "hold" &&
    (settings?.hotkey_key ?? "RightCtrl") !== "CtrlAltSpace"
  );
}

/** Lightweight tick: update footer + dictate bits without clobbering forms. */
function paintStatusBits(): void {
  if (!status) return;
  const dot = document.getElementById("side-dot");
  if (dot) {
    dot.className =
      "status-dot " +
      (status.recording ? "dot-red" : transcribing ? "dot-amber" : status.model_loaded ? "dot-green" : "dot-gray");
  }
  const ft = document.getElementById("side-model");
  if (ft) {
    const m = models.find((x) => x.id === status!.model_id);
    ft.textContent = `${status.engine_label} • ${m?.name ?? status.model_id}`;
  }
  const ms = document.getElementById("mic-state");
  if (ms) {
    const key = talkKey();
    ms.textContent = status.recording
      ? `● Recording… ${talkHold() ? `release ${key}` : `press ${key} again`} to finish`
      : transcribing
        ? "… Transcribing"
        : `○ Idle — ${talkHold() ? `hold ${key}` : `press ${key}`} to talk`;
  }
  const mb = document.getElementById("mic-btn") as HTMLButtonElement | null;
  if (mb) mb.classList.toggle("recording", status.recording);
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

async function toggleRecord(): Promise<void> {
  const msg = await call<string>("toggle_recording");
  if (msg !== null) {
    lastResult = msg;
    await refreshStatus();
    await refreshHistory();
    if (view === "dictate") renderView();
  }
}

async function downloadModel(id: string): Promise<void> {
  dlBars[id] = { file: "starting…", pct: null };
  if (view === "models") renderView();
  const msg = await call<string>("download_model", { id });
  delete dlBars[id];
  if (msg !== null) toast(msg, "success");
  await refreshModels();
  await refreshStatus();
  // If you have no working model yet (e.g. you just downloaded the selected
  // one), switch to it so Dictate lights up immediately.
  if (msg !== null && status && !status.model_loaded) {
    await selectModel(id);
  }
}

async function selectModel(id: string): Promise<void> {
  const msg = await call<string>("select_model", { id });
  if (msg !== null) toast(msg, "success");
  await refreshModels();
  await refreshStatus();
  await refreshSettings();
}

async function copyText(t: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(t);
    toast("Copied to clipboard", "success");
  } catch {
    toast("Copy failed", "error");
  }
}

let playingAudio: HTMLAudioElement | null = null;
let playingName: string | null = null;

function stopPlayback(): void {
  playingAudio?.pause();
  if (playingAudio) URL.revokeObjectURL(playingAudio.src);
  playingAudio = null;
  playingName = null;
}

function refreshPlayLabels(): void {
  document.querySelectorAll("[data-play]").forEach((b) => {
    const p = (b as HTMLButtonElement).dataset.play!;
    const n = p.split(/[\\/]/).pop() ?? p;
    (b as HTMLButtonElement).textContent = playingName === n ? "⏸ Playing" : "▶ Play";
  });
}

async function togglePlay(audioPath: string): Promise<void> {
  const name = audioPath.split(/[\\/]/).pop() ?? audioPath;
  if (playingAudio && playingName === name) {
    stopPlayback();
    refreshPlayLabels();
    return;
  }
  stopPlayback();
  const bytes = await call<number[]>("read_audio_file", { name });
  if (!bytes) return;
  const url = URL.createObjectURL(new Blob([new Uint8Array(bytes)], { type: "audio/wav" }));
  const el = new Audio(url);
  playingAudio = el;
  playingName = name;
  el.onended = () => {
    stopPlayback();
    refreshPlayLabels();
  };
  try {
    await el.play();
  } catch {
    toast("Playback failed", "error");
    stopPlayback();
  }
  refreshPlayLabels();
}

async function transcribeFile(): Promise<void> {
  const path = await open({
    multiple: false,
    filters: [{ name: "WAV audio", extensions: ["wav"] }],
  }).catch(() => null);
  if (typeof path !== "string" || !path) return;
  lastResult = "Transcribing file…";
  if (view === "dictate") renderView();
  const text = await call<string>("transcribe_file", { path });
  if (text !== null) {
    lastResult = text;
    toast("File transcribed — saved to History", "success");
    await refreshHistory();
  } else {
    lastResult = null;
  }
  if (view === "dictate") renderView();
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

function meter(label: string, value: number): string {
  return `<div class="meter"><span class="lbl">${label}</span><div class="bar"><div style="width:${Math.round(value * 10)}%"></div></div><span>${value.toFixed(1)}</span></div>`;
}

function viewDictate(): string {
  const m = models.find((x) => x.id === status?.model_id);
  const last = lastResult
    ? `<div class="card"><h3>Last result</h3><div class="result-box">${esc(lastResult)}</div>
       <div class="row" style="margin-top:10px"><button class="ghost small" id="copy-last">Copy</button></div></div>`
    : "";
  const warn =
    status && !status.model_loaded
      ? `<div class="card warn-card"><h3>⚠ No model ready</h3>
         <p class="muted">Download <b>${esc(m?.name ?? status.model_id)}</b> from AI Models first — everything runs offline after that.</p>
         <button class="small" id="goto-models">Open AI Models</button></div>`
      : "";
  const whisperWarn =
    status && status.engine === "whisper" && status.model_loaded && !status.whisper_binary
      ? `<div class="card warn-card"><h3>⚠ Whisper engine needs its binary</h3>
         <p class="muted">Place <code>whisper-cli.exe</code> (whisper.cpp build) in <code>${esc(status.data_dir)}\\bin</code> or on PATH. Parakeet models need no binary.</p></div>`
      : "";
  return `
    <h1>Dictate</h1>
    <p class="page-sub">Press the mic, speak, press again — text lands in whatever app has focus. 100% offline.</p>
    ${warn}${whisperWarn}
    <div class="card">
      <div class="mic-wrap">
        <button class="mic-btn${status?.recording ? " recording" : ""}" id="mic-btn">🎙</button>
        <div class="mic-state" id="mic-state"></div>
      </div>
      <p class="hotkey-hint muted">Talk key <code>${talkHold() ? `hold ${talkKey()}` : talkKey()}</code> works from any app
      ${m ? ` • Using <b>${esc(m.name)}</b> (${esc(m.languages)})` : ""}</p>
      <div class="row" style="justify-content:center;margin-top:12px">
        <button class="ghost small" id="file-btn">📄 Transcribe audio file (WAV)</button>
      </div>
    </div>
    ${last}`;
}

function viewModels(): string {
  const tabs = (["all", "whisper", "parakeet"] as const)
    .map(
      (f) =>
        `<button class="${modelFilter === f ? "active" : ""}" data-mfilter="${f}">${
          f === "all" ? "All" : f === "whisper" ? "Whisper" : "Parakeet"
        }</button>`
    )
    .join("");
  const cards = models
    .filter((m) => modelFilter === "all" || m.engine === modelFilter)
    .map((m) => {
      const dl = dlBars[m.id] ?? (m.downloading ? { file: "queued…", pct: null } : null);
      const action =
        m.active && m.downloaded
          ? `<button class="ghost small" disabled>✓ In use</button>`
          : dl
            ? `<button class="small" disabled>Downloading…</button>`
            : !m.downloaded
              ? `<button class="small" data-dl="${m.id}">Download</button>`
              : `<button class="small" data-use="${m.id}">Use</button>`;
      const progress = dl
        ? `<div class="progress"><div id="dlbar-${m.id}" style="width:${dl.pct ?? 0}%"></div></div>
           <div class="dl-file" id="dlfile-${m.id}">${esc(dl.file)}${
             dl.pct != null ? ` — ${dl.pct}%` : ""
           }</div>`
        : "";
      return `<div class="model-card${m.active ? " active" : ""}">
        <div class="model-top">
          <div><div class="model-name">${esc(m.name)}</div>
          <div class="muted" style="font-size:12px">${esc(m.languages)} • ${esc(m.size_label)}</div></div>
          <span class="badge ${m.engine}">${m.engine === "parakeet" ? "Parakeet" : "Whisper"}</span>
        </div>
        <div class="essence">✦ ${esc(m.essence)}</div>
        ${meter("Speed", m.speed)}${meter("Accuracy", m.accuracy)}
        ${progress}
        <div class="row">${action}
          ${m.downloaded ? `<span class="muted" style="font-size:12px">✓ on disk</span><button class="icon-btn" data-del-model="${m.id}" title="Delete model files">🗑</button>` : ""}
        </div>
      </div>`;
    })
    .join("");
  return `
    <h1>AI Models</h1>
    <p class="page-sub">Parakeet runs in-process (no extra binary). Whisper needs <code>whisper-cli.exe</code> — see Settings. Models live under <code>${esc(status?.data_dir ?? "")}\\models</code>.</p>
    <div class="filter-tabs">${tabs}</div>
    <div class="model-grid">${cards || `<div class="empty">No models for this filter.</div>`}</div>`;
}

function historyFiltered(): HistoryItem[] {
  const q = historyQuery.trim().toLowerCase();
  if (!q) return history;
  return history.filter((h) => h.text.toLowerCase().includes(q));
}

function viewSetup(): string {
  const devRows =
    audioDevices === null
      ? `<div class="empty">Click Refresh to list microphones.</div>`
      : audioDevices.length === 0
        ? `<div class="empty">No input devices found — check the Windows microphone privacy toggle below.</div>`
        : audioDevices
            .map(
              (d) => `<div class="dict-row"><div class="dict-rule"><b>${esc(d.name)}</b>
                <span class="muted"> — ${d.sample_rate ? `${(d.sample_rate / 1000).toFixed(1)} kHz, ${d.channels}ch` : "unavailable"}</span></div>
                ${d.is_default ? `<span class="badge">default</span>` : ""}</div>`
            )
            .join("");
  const verdict = !micTest
    ? ""
    : !micTest.callbacks
      ? `<p style="color:var(--amber)">⚠ Stream opened on <b>${esc(micTest.device)}</b> but zero audio frames arrived. Likely: another app holds the mic exclusively (quit voice apps, retest), or the default endpoint is dead — pick a working mic in Windows Sound settings.</p>`
      : micTest.peak > 0.02
        ? `<p style="color:var(--green)">✓ Microphone working — <b>${esc(micTest.device)}</b> at ${(micTest.sample_rate / 1000).toFixed(1)} kHz, peak ${(micTest.peak * 100).toFixed(0)}% over ${micTest.duration_secs.toFixed(1)}s.</p>`
        : `<p style="color:var(--amber)">⚠ Frames arrive from <b>${esc(micTest.device)}</b> but only silence (peak ${(micTest.peak * 100).toFixed(1)}%) — unmute the mic and raise its input level in Windows Sound settings.</p>`;
  const m = models.find((x) => x.id === status?.model_id);
  const modelOk = status?.model_loaded;
  return `
    <h1>Setup</h1>
    <p class="page-sub">Windows has no macOS-style permission popups — everything here is a check, not a grant.</p>
    <div class="card"><h3>1 · Microphone</h3>
      <p class="muted">Windows guards the mic with one global toggle:
      <b>Settings → Privacy &amp; security → Microphone → Let desktop apps access your microphone</b> must be ON.</p>
      <p class="muted">DictFlow captures from: <b>${esc(status?.audio_device ?? "none found")}</b></p>
      <div class="row" style="margin-bottom:12px">
        <button class="small" id="mic-refresh">Refresh devices</button>
        <button class="small" id="mic-open-settings">Open microphone settings</button>
        <button class="small" id="mic-test" ${micTesting ? "disabled" : ""}>${micTesting ? "Testing… speak now" : "🎙 Test microphone (1.5s)"}</button>
      </div>
      ${verdict}
      ${devRows}
    </div>
    <div class="card"><h3>2 · Auto-paste</h3>
      <p class="muted">No accessibility permission needed on Windows — Ctrl+V injection always works.
      Toggle it in Settings → Auto-paste.</p>
    </div>
    <div class="card"><h3>3 · Speech model</h3>
      <p>${modelOk ? `✓ <b>${esc(m?.name ?? "")}</b> ready.` : "⚠ No model ready yet."}</p>
      <button class="small ghost" id="setup-models">Open AI Models</button>
    </div>`;
}

function viewHistory(): string {
  return `
    <h1>History</h1>
    <p class="page-sub">${history.length} dictation${history.length === 1 ? "" : "s"} stored on this device only.</p>
    <div class="card"><div class="row">
      <input type="text" id="hist-q" placeholder="Search transcripts…" value="${esc(historyQuery)}" style="flex:1;min-width:200px">
      <button class="danger-ghost small" id="hist-clear">Clear all</button>
    </div></div>
    <div id="hist-list"></div>`;
}

function renderHistoryList(): void {
  const box = document.getElementById("hist-list");
  if (!box) return;
  const items = historyFiltered();
  if (!items.length) {
    box.innerHTML = `<div class="empty">Nothing here yet — dictate something first.</div>`;
    return;
  }
  box.innerHTML = items
    .map((h) => {
      const idx = history.indexOf(h);
      const m = models.find((x) => x.id === h.model);
      return `<div class="hist-item">
        <div class="hist-text">${esc(h.text)}</div>
        <div class="hist-meta">
          <span>${fmtDate(h.date_unix)}</span>
          ${h.model ? `<span class="badge ${m?.engine ?? ""}">${esc(m?.name ?? h.model)}</span>` : ""}
          ${h.duration_secs ? `<span>🎙 ${fmtDur(h.duration_secs)}</span>` : ""}
          <span>${wordCount(h.text)} words</span>
          <span class="spacer"></span>
          ${h.audio_path ? `<button class="icon-btn" data-play="${esc(h.audio_path)}">▶ Play</button>` : ""}
          <button class="icon-btn" data-copy-hist="${idx}">Copy</button>
          <button class="icon-btn" data-del-hist="${idx}">Delete</button>
        </div>
      </div>`;
    })
    .join("");
  refreshPlayLabels();
}

function viewDictionary(): string {
  const rows = dict.length
    ? dict
        .map(
          (e) => `<div class="dict-row${e.enabled ? "" : " dict-off"}">
        <input type="checkbox" data-dict-toggle="${e.id}" ${e.enabled ? "checked" : ""} title="Enable rule">
        <div class="dict-rule"><b>${esc(e.trigger)}</b><span class="arrow">→</span>${esc(e.replacement) || "<i class='muted'>(delete)</i>"}</div>
        <button class="icon-btn" data-dict-del="${e.id}">Delete</button>
      </div>`
        )
        .join("")
    : `<div class="empty">No rules yet. Example: say “my email” → get “you@example.com”.</div>`;
  return `
    <h1>Dictionary</h1>
    <p class="page-sub">Spoken triggers expand to exact text after every transcription — snippets, names, jargon. Runs fully offline.</p>
    <div class="card"><h3>Add rule</h3>
      <div class="add-form">
        <label class="field">You say<input type="text" id="dict-trigger" placeholder="my email" style="min-width:180px"></label>
        <label class="field">Inserted<input type="text" id="dict-repl" placeholder="you@example.com" style="min-width:180px"></label>
        <label class="check"><input type="checkbox" id="dict-ww" checked> Whole word</label>
        <button id="dict-add">Add</button>
      </div>
    </div>
    <div class="card"><h3>Shortcuts</h3>
      <div class="row">
        <button class="ghost small" id="dict-symbols">Add common symbols (@, .com, #…)</button>
        <button class="ghost small" id="dict-export">Export JSON</button>
        <button class="ghost small" id="dict-import">Import JSON</button>
      </div>
    </div>
    ${rows}`;
}

function viewStats(): string {
  const words = history.reduce((a, h) => a + wordCount(h.text), 0);
  const secs = history.reduce((a, h) => a + (h.duration_secs || 0), 0);
  const perModel = new Map<string, number>();
  history.forEach((h) => {
    const key = h.model || "unknown";
    perModel.set(key, (perModel.get(key) ?? 0) + 1);
  });
  const rows = [...perModel.entries()]
    .map(([id, n]) => {
      const m = models.find((x) => x.id === id);
      return `<div class="dict-row"><div class="dict-rule">${esc(m?.name ?? (id || "unknown"))}</div><b>${n}</b></div>`;
    })
    .join("");
  return `
    <h1>Statistics</h1>
    <p class="page-sub">Local-only counts. Clearing history resets these.</p>
    <div class="stat-grid">
      <div class="stat"><div class="num">${history.length}</div><div class="cap">Dictations</div></div>
      <div class="stat"><div class="num">${words.toLocaleString()}</div><div class="cap">Words dictated</div></div>
      <div class="stat"><div class="num">${fmtDur(secs)}</div><div class="cap">Audio recorded</div></div>
      <div class="stat"><div class="num">${dict.length}</div><div class="cap">Dictionary rules</div></div>
    </div>
    <div class="card"><h3>Dictations per model</h3>${rows || `<div class="empty">No data yet.</div>`}</div>`;
}

function viewSettings(): string {
  if (!settings) return `<div class="empty">Loading…</div>`;
  const modelOpts = models
    .map((m) => `<option value="${m.id}" ${m.id === settings!.model_id ? "selected" : ""}>${esc(m.name)} (${m.engine})${m.downloaded ? "" : " — not downloaded"}</option>`)
    .join("");
  const langOpts = LANGUAGES.map(
    ([v, l]) => `<option value="${v}" ${settings!.language === v ? "selected" : ""}>${l}</option>`
  ).join("");
  const cleanupOpts = CLEANUPS.map(
    ([v, l]) => `<option value="${v}" ${settings!.cleanup === v ? "selected" : ""}>${l}</option>`
  ).join("");
  const hotkeyOpts = HOTKEYS.map(
    ([v, l]) => `<option value="${v}" ${settings!.hotkey_key === v ? "selected" : ""}>${l}</option>`
  ).join("");
  const modeOpts = [["hold", "Hold to talk"], ["toggle", "Toggle"]]
    .map(([v, l]) => `<option value="${v}" ${settings!.recording_mode === v ? "selected" : ""}>${l}</option>`)
    .join("");
  return `
    <h1>Settings</h1>
    <p class="page-sub">Everything stays on this PC. No accounts, no telemetry.</p>
    <div class="card"><h3>Transcription</h3>
      <div class="set-row"><div><b>Model</b><div class="desc">Parakeet v3 is the best all-rounder; Whisper needs its binary (below).</div></div>
        <select id="set-model">${modelOpts}</select></div>
      <div class="set-row"><div><b>Language</b><div class="desc">Whisper source language. Parakeet v3 auto-detects.</div></div>
        <select id="set-lang">${langOpts}</select></div>
      <div class="set-row"><div><b>Auto-paste</b><div class="desc">Type the result into the focused app via Ctrl+V right after transcribing.</div></div>
        <input type="checkbox" id="set-paste" ${settings.auto_paste ? "checked" : ""}></div>
      <div class="set-row"><div><b>Cleanup</b><div class="desc">How much the transcript is polished. Wispr Flow calls this Auto Cleanup.</div></div>
        <select id="set-cleanup">${cleanupOpts}</select></div>
      <div class="set-row"><div><b>Translate to English</b><div class="desc">Whisper only, needs a non-Auto language. Parakeet v3 hears 25 languages as-is.</div></div>
        <input type="checkbox" id="set-translate" ${settings.translate ? "checked" : ""}></div>
    </div>
    <div class="card"><h3>Talk key</h3>
      <div class="set-row"><div><b>Key</b><div class="desc">Single-key hold feels like the Mac Fn key. Right Ctrl is the safest default; Scroll Lock / F9 never type anything. Left Ctrl works but every Ctrl+C/S/V briefly opens then discards the mic — the combo needs no polling.</div></div>
        <select id="set-hotkey">${hotkeyOpts}</select></div>
      <div class="set-row"><div><b>Mode</b><div class="desc">Hold: press-and-hold to record, release to transcribe. Toggle: press to start, press again to stop.</div></div>
        <select id="set-mode">${modeOpts}</select></div>
    </div>
    <div class="card"><h3>Storage & engine</h3>
      <div class="set-row"><div><b>Data folder</b><div class="desc"><code>${esc(status?.data_dir ?? "")}</code></div></div></div>
      <div class="set-row"><div><b>Whisper binary</b><div class="desc">${
        status?.whisper_binary ? `<code>${esc(status.whisper_binary)}</code>` : "Not found — Parakeet works without it."
      }</div></div></div>
    </div>
    <div class="card"><h3>About</h3>
      <div class="set-row"><div><b>Version</b><div class="desc">DictFlow ${esc(status?.version ?? "")} — MIT open source, 100% offline.</div></div>
        <button class="small ghost" id="check-updates">Check for updates</button></div>
      <div class="set-row"><div><b>Start with Windows</b><div class="desc">Launch minimized to the tray at login.</div></div>
        <input type="checkbox" id="set-autostart"></div>
    </div>`;
}

function render(): void {
  document.getElementById("app")!.innerHTML = `
    <aside class="sidebar">
      <div class="brand"><div class="brand-mark"><img src="${logoUrl}" alt="DictFlow logo"></div>
        <div><div class="brand-name">DictFlow</div><div class="brand-sub">offline dictation</div></div></div>
      <nav class="nav">${NAV.map((n) => `<button data-nav="${n.id}" class="${view === n.id ? "active" : ""}"><span class="ico">${n.ico}</span>${n.label}</button>`).join("")}</nav>
      <div class="side-footer"><span id="side-dot" class="status-dot dot-gray"></span><span id="side-model">…</span></div>
    </aside>
    <main class="main" id="view"></main>
    <div id="toasts"></div>`;
  document.querySelectorAll("[data-nav]").forEach((b) =>
    (b as HTMLButtonElement).onclick = () => {
      stopPlayback();
      view = (b as HTMLButtonElement).dataset.nav as View;
      render();
    }
  );
  renderView();
  paintStatusBits();
}

function renderView(): void {
  const box = document.getElementById("view");
  if (!box) return;
  if (view === "dictate") box.innerHTML = viewDictate();
  else if (view === "models") box.innerHTML = viewModels();
  else if (view === "setup") box.innerHTML = viewSetup();
  else if (view === "history") {
    box.innerHTML = viewHistory();
    renderHistoryList();
  } else if (view === "dictionary") box.innerHTML = viewDictionary();
  else if (view === "stats") box.innerHTML = viewStats();
  else box.innerHTML = viewSettings();
  bindView();
  paintStatusBits();
}

function bindView(): void {
  document.querySelectorAll("[data-nav]");
  // Dictate
  (document.getElementById("mic-btn") as HTMLButtonElement | null)?.addEventListener("click", toggleRecord);
  (document.getElementById("copy-last") as HTMLButtonElement | null)?.addEventListener("click", () => {
    if (lastResult) copyText(lastResult);
  });
  (document.getElementById("goto-models") as HTMLButtonElement | null)?.addEventListener("click", () => {
    view = "models";
    render();
  });
  (document.getElementById("file-btn") as HTMLButtonElement | null)?.addEventListener("click", transcribeFile);
  // Models
  document.querySelectorAll("[data-dl]").forEach((b) =>
    (b as HTMLButtonElement).onclick = () => downloadModel((b as HTMLButtonElement).dataset.dl!)
  );
  document.querySelectorAll("[data-use]").forEach((b) =>
    (b as HTMLButtonElement).onclick = () => selectModel((b as HTMLButtonElement).dataset.use!)
  );
  document.querySelectorAll("[data-del-model]").forEach((b) =>
    (b as HTMLButtonElement).onclick = async () => {
      const id = (b as HTMLButtonElement).dataset.delModel!;
      const m = models.find((x) => x.id === id);
      if (!window.confirm(`Delete ${m?.name ?? id} (${m?.size_label ?? ""}) from disk?`)) return;
      const msg = await call<string>("delete_model", { id });
      if (msg !== null) toast(msg, "success");
      await refreshModels();
      await refreshStatus();
      await refreshSettings();
    }
  );
  document.querySelectorAll("[data-mfilter]").forEach((b) =>
    (b as HTMLButtonElement).onclick = () => {
      modelFilter = (b as HTMLButtonElement).dataset.mfilter as typeof modelFilter;
      renderView();
    }
  );
  // Setup
  (document.getElementById("mic-refresh") as HTMLButtonElement | null)?.addEventListener("click", refreshAudioDevices);
  (document.getElementById("mic-open-settings") as HTMLButtonElement | null)?.addEventListener("click", async () => {
    await call("open_mic_settings");
  });
  (document.getElementById("mic-test") as HTMLButtonElement | null)?.addEventListener("click", async () => {
    micTesting = true;
    micTest = null;
    if (view === "setup") renderView();
    const res = await call<MicTest>("test_microphone");
    micTesting = false;
    if (res) {
      micTest = res;
      if (res.peak <= 0.02) toast("Microphone test heard nothing — check mic + privacy toggle", "error");
      else toast("Microphone test OK", "success");
    }
    if (view === "setup") renderView();
  });
  (document.getElementById("setup-models") as HTMLButtonElement | null)?.addEventListener("click", () => {
    view = "models";
    render();
  });
  // History
  const q = document.getElementById("hist-q") as HTMLInputElement | null;
  q?.addEventListener("input", () => {
    historyQuery = q.value;
    renderHistoryList();
  });
  (document.getElementById("hist-clear") as HTMLButtonElement | null)?.addEventListener("click", async () => {
    await call("clear_history");
    await refreshHistory();
    toast("History cleared", "success");
  });
  document.querySelectorAll("[data-copy-hist]").forEach((b) =>
    (b as HTMLButtonElement).onclick = () => {
      const h = history[Number((b as HTMLButtonElement).dataset.copyHist)];
      if (h) copyText(h.text);
    }
  );
  document.querySelectorAll("[data-del-hist]").forEach((b) =>
    (b as HTMLButtonElement).onclick = async () => {
      await call("delete_history_item", { index: Number((b as HTMLButtonElement).dataset.delHist) });
      await refreshHistory();
    }
  );
  document.querySelectorAll("[data-play]").forEach((b) =>
    (b as HTMLButtonElement).onclick = () => togglePlay((b as HTMLButtonElement).dataset.play!)
  );
  // Dictionary
  (document.getElementById("dict-add") as HTMLButtonElement | null)?.addEventListener("click", async () => {
    const trigger = (document.getElementById("dict-trigger") as HTMLInputElement).value;
    const replacement = (document.getElementById("dict-repl") as HTMLInputElement).value;
    const whole_word = (document.getElementById("dict-ww") as HTMLInputElement).checked;
    const res = await call<DictionaryEntry[]>("add_dictionary_entry", { trigger, replacement, whole_word });
    if (res) {
      dict = res;
      renderView();
      toast("Rule added", "success");
    }
  });
  (document.getElementById("dict-symbols") as HTMLButtonElement | null)?.addEventListener("click", async () => {
    const res = await call<DictionaryEntry[]>("add_symbol_presets");
    if (res) {
      dict = res;
      renderView();
      toast("Symbol presets added", "success");
    }
  });
  (document.getElementById("dict-export") as HTMLButtonElement | null)?.addEventListener("click", async () => {
    const path = await save({
      defaultPath: "dictflow-dictionary.json",
      filters: [{ name: "JSON", extensions: ["json"] }],
    }).catch(() => null);
    if (typeof path !== "string" || !path) return;
    const msg = await call<string>("export_dictionary", { path });
    if (msg !== null) toast(msg, "success");
  });
  (document.getElementById("dict-import") as HTMLButtonElement | null)?.addEventListener("click", async () => {
    const path = await open({
      multiple: false,
      filters: [{ name: "JSON", extensions: ["json"] }],
    }).catch(() => null);
    if (typeof path !== "string" || !path) return;
    const res = await call<DictionaryEntry[]>("import_dictionary", { path });
    if (res) {
      dict = res;
      renderView();
      toast(`Imported ${res.length} rules`, "success");
    }
  });
  document.querySelectorAll("[data-dict-toggle]").forEach((c) =>
    ((c as HTMLInputElement).onchange = async () => {
      await call("set_dictionary_enabled", { id: (c as HTMLInputElement).dataset.dictToggle, enabled: (c as HTMLInputElement).checked });
      await refreshDict();
    })
  );
  document.querySelectorAll("[data-dict-del]").forEach((b) =>
    (b as HTMLButtonElement).onclick = async () => {
      await call("delete_dictionary_entry", { id: (b as HTMLButtonElement).dataset.dictDel });
      await refreshDict();
    }
  );
  // Settings
  const sm = document.getElementById("set-model") as HTMLSelectElement | null;
  const sl = document.getElementById("set-lang") as HTMLSelectElement | null;
  const sp = document.getElementById("set-paste") as HTMLInputElement | null;
  const sc = document.getElementById("set-cleanup") as HTMLSelectElement | null;
  const st = document.getElementById("set-translate") as HTMLInputElement | null;
  const sk = document.getElementById("set-hotkey") as HTMLSelectElement | null;
  const smo = document.getElementById("set-mode") as HTMLSelectElement | null;
  const saveSettings = async () => {
    if (!settings || !sm || !sl || !sp || !sc || !st || !sk || !smo) return;
    const next: Settings = {
      model_id: sm.value,
      language: sl.value,
      auto_paste: sp.checked,
      cleanup: sc.value,
      translate: st.checked,
      hotkey_key: sk.value,
      recording_mode: smo.value,
    };
    const res = await call<Settings>("set_settings", { settings: next });
    if (res) {
      settings = res;
      await refreshModels();
      await refreshStatus();
      if (view === "dictate") renderView();
    }
  };
  sm?.addEventListener("change", saveSettings);
  sl?.addEventListener("change", saveSettings);
  sp?.addEventListener("change", saveSettings);
  sc?.addEventListener("change", saveSettings);
  st?.addEventListener("change", saveSettings);
  sk?.addEventListener("change", saveSettings);
  smo?.addEventListener("change", saveSettings);
  const sa = document.getElementById("set-autostart") as HTMLInputElement | null;
  if (sa) {
    autostartIsEnabled()
      .then((v) => {
        sa.checked = v;
      })
      .catch(() => undefined);
    sa.addEventListener("change", async () => {
      try {
        if (sa.checked) await autostartEnable();
        else await autostartDisable();
        toast(sa.checked ? "DictFlow will start with Windows" : "Auto-start off", "success");
      } catch (e) {
        toast(`${e}`, "error");
        sa.checked = !sa.checked;
      }
    });
  }
  (document.getElementById("check-updates") as HTMLButtonElement | null)?.addEventListener("click", async () => {
    const u = await call<UpdateInfo>("check_for_updates");
    if (!u) return;
    if (!u.configured) toast("Update check isn't wired to a repo yet (see docs/RELEASE.md)", "info");
    else if (u.available) toast(`Update available: ${u.latest} — ${u.url}`, "success");
    else toast(`You're on the latest version (${u.current})`, "success");
  });
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

async function boot(): Promise<void> {
  attachConsole().catch(() => undefined);
  render();
  await Promise.all([refreshStatus(), refreshModels(), refreshHistory(), refreshDict(), refreshSettings()]);
  renderView();

  await listen<boolean>("dictflow://recording", () => refreshStatus());
  await listen<boolean>("dictflow://transcribing", (e) => {
    transcribing = e.payload;
    paintStatusBits();
  });
  await listen<string>("dictflow://download", (e) => toast(e.payload, "info"));
  await listen<DlProgress>("dictflow://download-progress", (e) => {
    const p = e.payload;
    if (p.file === "done") {
      delete dlBars[p.model_id];
      refreshModels();
      return;
    }
    const pct = p.total ? Math.round((p.received / p.total) * 100) : null;
    dlBars[p.model_id] = { file: p.file, pct };
    const bar = document.getElementById(`dlbar-${p.model_id}`);
    if (bar && pct !== null) bar.style.width = `${pct}%`;
    const lbl = document.getElementById(`dlfile-${p.model_id}`);
    if (lbl) lbl.textContent = `${p.file}${pct !== null ? ` — ${pct}%` : ""}`;
    if (view === "models" && !bar) renderView();
  });
  await listen("dictflow://history-updated", () => refreshHistory());

  setInterval(refreshStatus, 2000);
}

boot();
