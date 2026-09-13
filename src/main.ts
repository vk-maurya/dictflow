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
type View = "home" | "dictate" | "models" | "setup" | "history" | "dictionary" | "stats" | "settings" | "onboard";
type OnboardStep = "welcome" | "talkkey" | "mictest" | "download";
type StatsPeriod = "today" | "7d" | "30d" | "all";

interface DayBucket {
  dictations: number;
  words: number;
  audio_secs: number;
  raw_words: number;
  dict_hits: number;
}
interface UsageStats {
  lifetime_dictations: number;
  lifetime_words: number;
  lifetime_audio_secs: number;
  lifetime_raw_words: number;
  lifetime_dict_hits: number;
  by_day: Record<string, DayBucket>;
}

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
  raw_text?: string;
  words_out?: number;
  dict_hits?: number;
  cleanup?: string;
}
interface DictionaryEntry {
  id: string;
  trigger: string;
  replacement: string;
  enabled: boolean;
  whole_word: boolean;
  kind?: string;
  starred?: boolean;
  usage?: number;
}
interface Settings {
  model_id: string;
  language: string;
  auto_paste: boolean;
  cleanup: string;
  translate: boolean;
  hotkey_key: string;
  recording_mode: string;
  audio_device: string | null;
  overlay_enabled: boolean;
  overlay_edge: string;
  overlay_offset: number;
  paste_last_key: string;
  copy_last_key: string;
  onboarded: boolean;
  session_cap: boolean;
  audio_backend: string;
  audio_api_base: string;
  audio_api_model: string;
  llm_backend: string;
  llm_api_base: string;
  llm_api_model: string;
  llm_temperature: number;
  llm_timeout_ms: number;
  llm_preset: string;
  llm_custom_prompt: string;
  llm_enabled: boolean;
}
interface ProviderStatus {
  audio_key_masked: string;
  llm_key_masked: string;
  has_audio_key: boolean;
  has_llm_key: boolean;
  default_prompt: string;
  prompt_presets: Record<string, string>;
}

const DEFAULT_PROCESS_PROMPT =
  "You are a transcript editor. Clean and lightly rewrite speech-to-text: fix spelling, ASR errors, grammar, and punctuation. Return only the cleaned text.";

const AUDIO_HOSTS: { id: string; label: string; base: string; model: string }[] = [
  { id: "openai", label: "OpenAI", base: "https://api.openai.com/v1", model: "whisper-1" },
  { id: "groq", label: "Groq", base: "https://api.groq.com/openai/v1", model: "whisper-large-v3-turbo" },
  { id: "custom", label: "Custom", base: "", model: "" },
];

function audioHostId(base: string): string {
  const b = (base || "").trim().replace(/\/+$/, "");
  if (b.includes("api.groq.com")) return "groq";
  if (!b || b.includes("api.openai.com")) return "openai";
  return "custom";
}

function audioHostLabel(base: string): string {
  const id = audioHostId(base);
  if (id === "groq") return "Groq";
  if (id === "openai") return "OpenAI";
  return "Online";
}

/** Live speech source: local catalog name, or the online API model name. */
function activeSpeech(): {
  id: string;
  name: string;
  engineLabel: string;
  languages: string;
  ready: boolean;
  online: boolean;
} {
  const online = settings?.audio_backend === "openai_compat";
  if (online) {
    const name = (settings?.audio_api_model ?? "").trim() || "online model";
    return {
      id: name,
      name,
      engineLabel: audioHostLabel(settings?.audio_api_base ?? ""),
      languages: "via API",
      ready: Boolean((settings?.audio_api_model ?? "").trim()),
      online: true,
    };
  }
  const m = models.find((x) => x.id === status?.model_id);
  return {
    id: status?.model_id ?? "",
    name: m?.name ?? status?.speech_model_name ?? status?.model_id ?? "No model",
    engineLabel: status?.engine_label ?? "Local",
    languages: m?.languages ?? "",
    ready: Boolean(status?.model_loaded),
    online: false,
  };
}

function historyModelLabel(id: string): { name: string; engine: string } {
  if (!id) return { name: "", engine: "" };
  const m = models.find((x) => x.id === id);
  if (m) return { name: m.name, engine: m.engine };
  return { name: id, engine: "api" };
}

function processPromptFor(preset: string, custom: string): string {
  if (preset === "custom" && custom.trim()) return custom;
  return provider?.prompt_presets?.[preset] || provider?.default_prompt || DEFAULT_PROCESS_PROMPT;
}

function resolveSavedPrompt(preset: string, text: string): { preset: string; custom: string } {
  const trimmed = text.trim();
  const defaults = provider?.prompt_presets ?? {};
  if (preset !== "custom" && (!trimmed || trimmed === (defaults[preset] ?? "").trim())) {
    return { preset, custom: "" };
  }
  if (trimmed === (defaults.clean ?? DEFAULT_PROCESS_PROMPT).trim()) {
    return { preset: "clean", custom: "" };
  }
  for (const [id, body] of Object.entries(defaults)) {
    if (trimmed === body.trim()) return { preset: id, custom: "" };
  }
  return { preset: "custom", custom: trimmed };
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
  recommended_model: string;
  transcribing: boolean;
  speech_source?: string;
  speech_model?: string;
  speech_model_name?: string;
  speech_ready?: boolean;
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
  is_selected: boolean;
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

let view: View = "home";
let status: Status | null = null;
let models: ModelStatus[] = [];
let history: HistoryItem[] = [];
let usage: UsageStats | null = null;
let dict: DictionaryEntry[] = [];
let settings: Settings | null = null;
let statsPeriod: StatsPeriod = "today";
let recordStartedAt: number | null = null;
let recordTick: number | null = null;
let transcribing = false;
let lastResult: string | null = null;
let modelFilter: "all" | Engine = "all";
let historyQuery = "";
let homeQuery = "";
let dlBars: Record<string, { file: string; pct: number | null }> = {};
let audioDevices: AudioDeviceInfo[] | null = null;
let micTest: MicTest | null = null;
let micTesting = false;
let onboardStep: OnboardStep = "welcome";
let livePeak = 0;
let dictTab: "vocab" | "snippet" = "vocab";
let correctingIdx: number | null = null;
let modelsPane: "audio" | "llm" = "audio";
let provider: ProviderStatus | null = null;

type NavItem = { id: View; label: string; ico: string };

const NAV_MAIN: NavItem[] = [
  { id: "home", label: "Dashboard", ico: "home" },
  { id: "dictate", label: "Dictate", ico: "mic" },
  { id: "history", label: "History", ico: "clock" },
  { id: "dictionary", label: "Dictionary", ico: "book" },
  { id: "stats", label: "Insights", ico: "chart" },
  { id: "models", label: "AI Models", ico: "cpu" },
];
const NAV_FOOT: NavItem[] = [
  { id: "setup", label: "Setup", ico: "check" },
  { id: "settings", label: "Settings", ico: "gear" },
];

const ICO: Record<string, string> = {
  home: `<path d="M4 10.5 12 3l8 7.5V21H4z"/><path d="M9 21v-7h6v7"/>`,
  mic: `<path d="M12 3a3 3 0 0 1 3 3v6a3 3 0 0 1-6 0V6a3 3 0 0 1 3-3z"/><path d="M5 11a7 7 0 0 0 14 0"/><path d="M12 18v3"/>`,
  cpu: `<rect x="5" y="5" width="14" height="14" rx="2"/><path d="M9 2v3M15 2v3M9 19v3M15 19v3M2 9h3M2 15h3M19 9h3M19 15h3"/><rect x="9" y="9" width="6" height="6" rx="1"/>`,
  check: `<circle cx="12" cy="12" r="9"/><path d="m8 12 2.5 2.5L16 9"/>`,
  clock: `<circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/>`,
  book: `<path d="M4 5a2 2 0 0 1 2-2h13v16H6a2 2 0 0 0-2 2z"/><path d="M6 3v16"/>`,
  chart: `<path d="M4 19V5M4 19h16M8 16v-5M12 16V8M16 16v-3"/>`,
  gear: `<circle cx="12" cy="12" r="3"/><path d="M12 2v2M12 20v2M2 12h2M20 12h2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M19.1 4.9l-1.4 1.4M6.3 17.7l-1.4 1.4"/>`,
  search: `<circle cx="11" cy="11" r="6.5"/><path d="m16 16 4 4"/>`,
  copy: `<rect x="8" y="8" width="12" height="12" rx="2"/><path d="M4 16V6a2 2 0 0 1 2-2h10"/>`,
  trash: `<path d="M4 7h16M9 7V5h6v2M6 7l1 13h10l1-13"/>`,
  play: `<path d="m8 5 11 7-11 7V5z"/>`,
  pause: `<path d="M8 5h3v14H8zM13 5h3v14h-3z"/>`,
  file: `<path d="M7 3h8l5 5v13H7z"/><path d="M15 3v5h5"/>`,
  warn: `<path d="M12 3 22 20H2z"/><path d="M12 9v5M12 17h.01"/>`,
  download: `<path d="M12 4v12m-5-5 5 5 5-5M5 20h14"/>`,
  x: `<path d="M6 6l12 12M18 6 6 18"/>`,
};

function ico(name: string): string {
  return `<svg class="ico-svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${ICO[name] ?? ""}</svg>`;
}

function navBtn(n: NavItem): string {
  return `<button data-nav="${n.id}" class="${view === n.id ? "active" : ""}">${ico(n.ico)}<span>${n.label}</span></button>`;
}

const EMPTY_DAY: DayBucket = { dictations: 0, words: 0, audio_secs: 0, raw_words: 0, dict_hits: 0 };

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
  ["off", "None — raw transcript"],
  ["light", "Light — fillers only"],
  ["full", "Full — fillers + tidy"],
];

const HOTKEYS: [string, string][] = [
  ["RightCtrl", "Right Ctrl (hold)"],
  ["LeftCtrl", "Left Ctrl (cancels on Ctrl+key)"],
  ["ScrollLock", "Scroll Lock"],
  ["F9", "F9"],
  ["CtrlAltSpace", "Ctrl + Alt + Space"],
];

const UTILITY_KEYS: [string, string][] = [
  ["Shift+Alt+Z", "Shift + Alt + Z"],
  ["Shift+Alt+X", "Shift + Alt + X"],
  ["Ctrl+Alt+Z", "Ctrl + Alt + Z"],
  ["Ctrl+Alt+X", "Ctrl + Alt + X"],
  ["Ctrl+Shift+Z", "Ctrl + Shift + Z"],
  ["Ctrl+Shift+X", "Ctrl + Shift + X"],
];

const OVERLAY_EDGES: [string, string][] = [
  ["top", "Top"],
  ["bottom", "Bottom"],
  ["left", "Left"],
  ["right", "Right"],
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

function ymd(d: Date): string {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

function noon(d: Date): Date {
  const x = new Date(d);
  x.setHours(12, 0, 0, 0);
  return x;
}

function shiftDays(d: Date, n: number): Date {
  const x = noon(d);
  x.setDate(x.getDate() + n);
  return x;
}

function dayKeys(count: number): string[] {
  const keys: string[] = [];
  let d = noon(new Date());
  for (let i = 0; i < count; i++) {
    keys.push(ymd(d));
    d = shiftDays(d, -1);
  }
  return keys;
}

function fmtClock(sec: number): string {
  const s = Math.max(0, Math.floor(sec));
  const m = Math.floor(s / 60);
  const r = s % 60;
  return `${m}:${r.toString().padStart(2, "0")}`;
}

function fmtCompact(n: number): string {
  if (n >= 1_000_000) {
    const v = n / 1_000_000;
    return `${v >= 10 ? Math.round(v) : v.toFixed(1).replace(/\.0$/, "")}M`;
  }
  if (n >= 1000) {
    const v = n / 1000;
    return `${v >= 10 ? Math.round(v) : v.toFixed(1).replace(/\.0$/, "")}K`;
  }
  return n.toLocaleString();
}

function fmtClockTime(ts: number): string {
  if (!ts) return "";
  return new Date(ts * 1000).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" }).toLowerCase();
}

function dayHeading(ts: number): string {
  if (!ts) return "IMPORTED";
  const d = noon(new Date(ts * 1000));
  const today = noon(new Date());
  if (ymd(d) === ymd(today)) return "TODAY";
  if (ymd(d) === ymd(shiftDays(today, -1))) return "YESTERDAY";
  return d.toLocaleDateString(undefined, { weekday: "long", month: "short", day: "numeric" }).toUpperCase();
}

function fmtConsumed(secs: number): string {
  if (!secs) return "0 min";
  if (secs < 60) return `${Math.round(secs)}s`;
  if (secs < 3600) {
    const m = secs / 60;
    return m >= 10 ? `${Math.round(m)} min` : `${m.toFixed(1)} min`;
  }
  const h = Math.floor(secs / 3600);
  const min = Math.round((secs % 3600) / 60);
  return min ? `${h}h ${min}m` : `${h}h`;
}

function fmtTimeSaved(words: number): string {
  const min = words / 40;
  if (!words) return "0 min";
  if (min < 1) return `${Math.round(min * 60)}s`;
  if (min < 60) return min < 10 ? `${min.toFixed(1)} min` : `${Math.round(min)} min`;
  const h = Math.floor(min / 60);
  const m = Math.round(min % 60);
  return m ? `${h}h ${m}m` : `${h}h`;
}

function calcWpm(words: number, secs: number): number {
  if (secs < 1 || words <= 0) return 0;
  return Math.round(words / (secs / 60));
}

function greeting(): string {
  return "Welcome back";
}

function periodTotals(period: StatsPeriod): DayBucket {
  if (!usage) return { ...EMPTY_DAY };
  if (period === "all") {
    return {
      dictations: usage.lifetime_dictations,
      words: usage.lifetime_words,
      audio_secs: usage.lifetime_audio_secs,
      raw_words: usage.lifetime_raw_words,
      dict_hits: usage.lifetime_dict_hits,
    };
  }
  const n = period === "today" ? 1 : period === "7d" ? 7 : 30;
  const tot: DayBucket = { ...EMPTY_DAY };
  for (const k of dayKeys(n)) {
    const b = usage.by_day[k];
    if (!b) continue;
    tot.dictations += b.dictations;
    tot.words += b.words;
    tot.audio_secs += b.audio_secs;
    tot.raw_words += b.raw_words;
    tot.dict_hits += b.dict_hits;
  }
  return tot;
}

function streakInfo(): { current: number; longest: number } {
  if (!usage) return { current: 0, longest: 0 };
  const keys = Object.keys(usage.by_day).sort();
  let longest = 0;
  let run = 0;
  let prev: string | null = null;
  for (const k of keys) {
    if ((usage.by_day[k]?.words ?? 0) <= 0) continue;
    if (prev) {
      const [y, m, d] = prev.split("-").map(Number);
      const next = shiftDays(new Date(y, m - 1, d), 1);
      run = ymd(next) === k ? run + 1 : 1;
    } else {
      run = 1;
    }
    prev = k;
    if (run > longest) longest = run;
  }
  let current = 0;
  let cursor = noon(new Date());
  while ((usage.by_day[ymd(cursor)]?.words ?? 0) > 0) {
    current += 1;
    cursor = shiftDays(cursor, -1);
  }
  return { current, longest };
}

function heatLevel(words: number): number {
  if (words <= 0) return 0;
  if (words < 250) return 1;
  if (words < 500) return 2;
  if (words < 750) return 3;
  return 4;
}

function startTimer(): void {
  if (recordTick != null) return;
  if (recordStartedAt == null) recordStartedAt = Date.now();
  recordTick = window.setInterval(paintRecTimer, 250);
  paintRecTimer();
}

function stopTimer(): void {
  if (recordTick != null) {
    window.clearInterval(recordTick);
    recordTick = null;
  }
  recordStartedAt = null;
  paintRecTimer();
}

function paintRecTimer(): void {
  const el = document.getElementById("rec-timer");
  if (!el) return;
  if (!recordStartedAt) {
    el.textContent = "";
    el.hidden = true;
    return;
  }
  el.hidden = false;
  el.textContent = fmtClock((Date.now() - recordStartedAt) / 1000);
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
    if (view === "models" || view === "onboard") renderView();
    paintStatusBits();
  }
}

async function refreshHistory(): Promise<void> {
  const h = await call<HistoryItem[]>("get_history");
  if (h) {
    history = h;
    if (view === "history") renderHistoryList();
    if (view === "stats" || view === "home" || view === "dictate") renderView();
  }
}

async function refreshStats(): Promise<void> {
  const s = await call<UsageStats>("get_stats");
  if (s) {
    usage = s;
    if (view === "stats" || view === "home") renderView();
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
    paintStatusBits();
  }
}

async function refreshProvider(): Promise<void> {
  const p = await call<ProviderStatus>("get_provider_status");
  if (p) provider = p;
}

async function refreshAudioDevices(): Promise<void> {
  const d = await call<AudioDeviceInfo[]>("get_audio_devices");
  if (d) {
    audioDevices = d;
    if (view === "setup" || view === "onboard" || view === "settings") renderView();
  }
}

async function finishOnboarding(): Promise<void> {
  if (!settings) return;
  const res = await call<Settings>("set_settings", { settings: { ...settings, onboarded: true } });
  if (res) {
    settings = res;
    view = "home";
    render();
  }
}

async function pickAudioDevice(name: string | null): Promise<void> {
  if (!settings) return;
  const res = await call<Settings>("set_settings", { settings: { ...settings, audio_device: name } });
  if (res) {
    settings = res;
    await refreshAudioDevices();
    await refreshStatus();
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
      (status.recording ? "dot-red" : transcribing ? "dot-amber" : activeSpeech().ready ? "dot-green" : "dot-gray");
  }
  const ft = document.getElementById("side-model");
  if (ft) {
    const speech = activeSpeech();
    ft.textContent = `${speech.engineLabel} • ${speech.name}`;
  }
  const pill = document.getElementById("side-pill");
  if (pill) pill.textContent = activeSpeech().online ? "Online" : "Offline";
  const ms = document.getElementById("mic-state");
  if (ms) {
    const key = talkKey();
    ms.textContent = status.recording
      ? `Recording — ${talkHold() ? `release ${key}` : `press ${key} again`} to finish`
      : transcribing
        ? "Transcribing"
        : `Idle — ${talkHold() ? `hold ${key}` : `press ${key}`} to talk`;
  }
  const mb = document.getElementById("mic-btn") as HTMLButtonElement | null;
  if (mb) mb.classList.toggle("recording", status.recording);
  if (status.recording) startTimer();
  else stopTimer();
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
  if (view !== "models" && view !== "onboard") renderView();
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
    (b as HTMLButtonElement).innerHTML = playingName === n ? `${ico("pause")} Playing` : `${ico("play")} Play`;
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
  const speech = activeSpeech();
  const last = lastResult
    ? `<div class="card"><h3>Last result</h3><div class="result-box">${esc(lastResult)}</div>
       <div class="row" style="margin-top:10px"><button class="ghost small" id="copy-last">Copy</button></div></div>`
    : "";
  const warn =
    !speech.ready
      ? `<div class="card warn-card"><h3>No model ready</h3>
         <p class="muted">${
           speech.online
             ? `Set an API model name on AI Models — speech is currently using the online source.`
             : `Download <b>${esc(speech.name)}</b> from AI Models first — everything runs offline after that.`
         }</p>
         <button class="small" id="goto-models">Open AI Models</button></div>`
      : "";
  const whisperWarn =
    !speech.online && status && status.engine === "whisper" && status.model_loaded && !status.whisper_binary
      ? `<div class="card warn-card"><h3>Whisper engine needs its binary</h3>
         <p class="muted">Place <code>whisper-cli.exe</code> (whisper.cpp build) in <code>${esc(status.data_dir)}\\bin</code> or on PATH. Parakeet models need no binary.</p></div>`
      : "";
  const using = speech.name
    ? speech.online
      ? ` • Using <b>${esc(speech.name)}</b> (${esc(speech.engineLabel)})`
      : ` • Using <b>${esc(speech.name)}</b>${speech.languages ? ` (${esc(speech.languages)})` : ""}`
    : "";
  return `
    <h1>Dictate</h1>
    <p class="page-sub">${
      speech.online
        ? `Press the mic, speak, press again — text lands in the focused app. Speech uses <b>${esc(speech.name)}</b> via ${esc(speech.engineLabel)}.`
        : "Press the mic, speak, press again — text lands in whatever app has focus. 100% offline."
    }</p>
    ${warn}${whisperWarn}
    <div class="card">
      <div class="mic-wrap">
        <button class="mic-btn${status?.recording ? " recording" : ""}" id="mic-btn">${ico("mic")}</button>
        <div class="mic-state" id="mic-state"></div>
        <div class="rec-timer" id="rec-timer" hidden></div>
      </div>
      <p class="hotkey-hint muted">Talk key <code>${talkHold() ? `hold ${talkKey()}` : talkKey()}</code> works from any app
      ${using}
      • Esc cancels this take
      • Paste last <code>${esc(settings?.paste_last_key ?? "Shift+Alt+Z")}</code>
      • Copy last <code>${esc(settings?.copy_last_key ?? "Shift+Alt+X")}</code></p>
      <div class="level-meter" id="level-meter" ${status?.recording ? "" : "hidden"}><div id="level-fill" style="width:${Math.round(Math.min(livePeak, 1) * 100)}%"></div></div>
      ${status?.recording ? `<div class="row" style="justify-content:center;margin-top:12px"><button class="danger-ghost small" id="cancel-rec">Cancel (Esc)</button></div>` : ""}
      <div class="row" style="justify-content:center;margin-top:12px">
        <button class="ghost small" id="file-btn">${ico("file")} Transcribe audio file (WAV)</button>
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
  const speech = activeSpeech();
  const cards = models
    .filter((m) => modelFilter === "all" || m.engine === modelFilter)
    .map((m) => {
      const inUse = !speech.online && m.id === (settings?.model_id ?? status?.model_id);
      const dl = dlBars[m.id] ?? (m.downloading ? { file: "queued…", pct: null } : null);
      const action =
        inUse && m.downloaded
          ? `<button class="ghost small" disabled>In use</button>`
          : dl
            ? `<button class="ghost small" data-cancel-dl="${m.id}">Cancel</button>`
            : !m.downloaded
              ? `<button class="small" data-dl="${m.id}">Download</button>`
              : `<button class="small" data-use="${m.id}">Use</button>`;
      const progress = dl
        ? `<div class="progress"><div id="dlbar-${m.id}" style="width:${dl.pct ?? 0}%"></div></div>
           <div class="dl-file" id="dlfile-${m.id}">${esc(dl.file)}${
             dl.pct != null ? ` — ${dl.pct}%` : ""
           }</div>`
        : "";
      return `<div class="model-card${inUse ? " active" : ""}">
        <div class="model-top">
          <div><div class="model-name">${esc(m.name)}</div>
          <div class="muted" style="font-size:12px">${esc(m.languages)} • ${esc(m.size_label)}</div></div>
          <span class="badge ${m.engine}">${m.engine === "parakeet" ? "Parakeet" : "Whisper"}</span>
        </div>
        <div class="essence">${esc(m.essence)}</div>
        ${meter("Speed", m.speed)}${meter("Accuracy", m.accuracy)}
        ${progress}
        <div class="row">${action}
          ${m.downloaded ? `<span class="muted" style="font-size:12px">on disk</span><button class="icon-btn" data-del-model="${m.id}" title="Delete model files">${ico("trash")}</button>` : ""}
        </div>
      </div>`;
    })
    .join("");
  const pane = `
    <div class="filter-tabs">
      <button class="${modelsPane === "audio" ? "active" : ""}" id="pane-audio">Audio</button>
      <button class="${modelsPane === "llm" ? "active" : ""}" id="pane-llm">LLM</button>
    </div>`;
  if (modelsPane === "llm") {
    const s = settings;
    const be = s?.llm_backend ?? "off";
    const bases = [
      ["off", "Off — rules only"],
      ["local", "Local (Ollama / LM Studio / llama.cpp)"],
      ["openai_compat", "OpenAI-compatible API"],
      ["anthropic", "Anthropic-compatible API"],
    ];
    const presets: [string, string][] = [
      ["clean", "Clean"],
      ["professional", "Professional"],
      ["casual", "Casual"],
      ["message", "Message"],
      ["bullets", "Bullets"],
      ["custom", "Custom"],
    ];
    const showConn = be !== "off";
    return `
      <h1>AI Models</h1>
      <p class="page-sub">Optional rewrite after local rules. Default is off — nothing leaves this PC. If the API errors, the rules text is pasted instead.</p>
      ${pane}
      <div class="card provider-card"><h3>LLM backend</h3>
        <p class="provider-note">Keys are stored in Windows Credential Manager (Generic Credentials: <code>DictFlow/…</code>), never in a settings export or a file on disk. A localhost server can use an empty key.</p>
        <div class="radio-list">
          ${bases.map(([v, l]) => `<label class="check"><input type="radio" name="llm-be" value="${v}" ${be === v ? "checked" : ""}> ${l}</label>`).join("")}
        </div>
        ${
          showConn
            ? `<div class="provider-grid">
          <label class="field">Base URL<input type="text" id="llm-base" value="${esc(s?.llm_api_base ?? "")}" placeholder="http://127.0.0.1:11434/v1"></label>
          <label class="field">Model<input type="text" id="llm-model" value="${esc(s?.llm_api_model ?? "")}" placeholder="gpt-4o-mini"></label>
          <label class="field">API key<input type="password" id="llm-key" autocomplete="off" placeholder="${esc(provider?.llm_key_masked || "optional on localhost")}"></label>
        </div>`
            : ""
        }
        <div class="provider-grid">
          <label class="field">Preset<select id="llm-preset">${presets.map(([v, l]) => `<option value="${v}" ${s?.llm_preset === v ? "selected" : ""}>${l}</option>`).join("")}</select></label>
          <label class="field span-all"><span class="field-head">Process prompt
            <button type="button" class="ghost small" id="llm-prompt-reset">Reset to default</button></span>
            <textarea id="llm-custom" rows="4" placeholder="${esc(DEFAULT_PROCESS_PROMPT)}">${esc(processPromptFor(s?.llm_preset ?? "clean", s?.llm_custom_prompt ?? ""))}</textarea>
            <span class="field-hint">The model cleans and rewrites the transcript (spelling, ASR slips, punctuation). Reset restores the Clean preset from the app.</span>
          </label>
        </div>
        ${
          showConn
            ? `<div class="set-row">
          <div><b>Polish after every dictation</b><div class="desc">Runs this prompt after local cleanup. Leave off to call the LLM only when cleanup is Medium or High.</div></div>
          <input type="checkbox" id="llm-on" ${s?.llm_enabled ? "checked" : ""}>
        </div>
        <div class="row">
          <button class="small" id="llm-save">Save LLM</button>
          <button class="ghost small" id="llm-test">Test connection</button>
        </div>`
            : `<div class="row">
          <button class="small" id="llm-save">Save LLM</button>
        </div>`
        }
      </div>`;
  }
  const s = settings;
  const audioBe = s?.audio_backend ?? "local";
  const audioApi = audioBe === "openai_compat";
  const micName = settings?.audio_device || status?.audio_device || "the Windows default microphone";
  return `
    <h1>AI Models</h1>
    <p class="page-sub">Local catalog stays the default. An OpenAI-compatible speech API is opt-in and sends the recording to that host.</p>
    ${pane}
    <div class="card provider-card"><h3>Speech source</h3>
      <div class="radio-list">
        <label class="check"><input type="radio" name="audio-be" value="local" ${audioBe === "local" ? "checked" : ""}> Local (Parakeet / Whisper on this PC)</label>
        <label class="check"><input type="radio" name="audio-be" value="openai_compat" ${audioBe === "openai_compat" ? "checked" : ""}> OpenAI-compatible API (<code>/v1/audio/transcriptions</code>)</label>
      </div>
      ${
        audioApi
          ? `<p class="provider-note">The API key is stored in Windows Credential Manager. Use the <code>/v1</code> root (Groq: <code>https://api.groq.com/openai/v1</code>), not the full <code>/audio/transcriptions</code> path. Test records 1.5 seconds from <b>${esc(micName)}</b> and POSTs only if the clip has speech.</p>
      <div class="provider-grid">
        <label class="field">Host<select id="audio-host">${AUDIO_HOSTS.map((h) => `<option value="${h.id}" ${audioHostId(s?.audio_api_base ?? "") === h.id ? "selected" : ""}>${h.label}</option>`).join("")}</select></label>
        <label class="field">Base URL<input type="text" id="audio-base" value="${esc(s?.audio_api_base ?? "")}" placeholder="https://api.groq.com/openai/v1"></label>
        <label class="field">Model<input type="text" id="audio-model" value="${esc(s?.audio_api_model ?? "")}" placeholder="whisper-large-v3-turbo"></label>
        <label class="field span-all">API key<input type="password" id="audio-key" autocomplete="off" placeholder="${esc(provider?.audio_key_masked || "required for cloud")}"></label>
      </div>
      <div class="row">
        <button class="small" id="audio-save">Save audio API</button>
        <button class="ghost small" id="audio-test">Test — speak 1.5s</button>
      </div>`
          : `<p class="provider-note">Dictation uses the downloaded model on this PC. Switch to the API option to send a spoken clip to a remote <code>/v1/audio/transcriptions</code> host.</p>
      <div class="row">
        <button class="small" id="audio-save">Save audio source</button>
      </div>`
      }
    </div>
    <div class="catalog-label">Local models</div>
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
              (d) => `<button class="dict-row device-pick${d.is_selected ? " selected" : ""}" data-pick-mic="${esc(d.name)}"><div class="dict-rule"><b>${esc(d.name)}</b>
                <span class="muted"> — ${d.sample_rate ? `${(d.sample_rate / 1000).toFixed(1)} kHz, ${d.channels}ch` : "unavailable"}</span></div>
                ${d.is_selected ? `<span class="badge">selected</span>` : d.is_default ? `<span class="badge">default</span>` : ""}</button>`
            )
            .join("");
  const verdict = !micTest
    ? ""
    : !micTest.callbacks
      ? `<p style="color:var(--amber)">Stream opened on <b>${esc(micTest.device)}</b> but zero audio frames arrived. Likely: another app holds the mic exclusively (quit voice apps, retest), or the default endpoint is dead — pick a working mic in Windows Sound settings.</p>`
      : micTest.peak > 0.02
        ? `<p style="color:var(--green)">Microphone working — <b>${esc(micTest.device)}</b> at ${(micTest.sample_rate / 1000).toFixed(1)} kHz, peak ${(micTest.peak * 100).toFixed(0)}% over ${micTest.duration_secs.toFixed(1)}s.</p>`
        : `<p style="color:var(--amber)">Frames arrive from <b>${esc(micTest.device)}</b> but only silence (peak ${(micTest.peak * 100).toFixed(1)}%) — unmute the mic and raise its input level in Windows Sound settings.</p>`;
  const speech = activeSpeech();
  const modelOk = speech.ready;
  return `
    <h1>Setup</h1>
    <p class="page-sub">Windows has no macOS-style permission popups — everything here is a check, not a grant.</p>
    <div class="card"><h3>1 · Microphone</h3>
      <p class="muted">Windows guards the mic with one global toggle:
      <b>Settings → Privacy &amp; security → Microphone → Let desktop apps access your microphone</b> must be ON.</p>
      <p class="muted">DictFlow captures from: <b>${esc(status?.audio_device ?? "none found")}</b>
      ${settings?.audio_device ? ` (preferred: ${esc(settings.audio_device)})` : " (Windows default)"}.</p>
      <div class="row" style="margin-bottom:12px">
        <button class="ghost small" id="mic-use-default">Use Windows default</button>
        <button class="small" id="mic-refresh">Refresh devices</button>
        <button class="small" id="mic-open-settings">Open microphone settings</button>
        <button class="small" id="mic-test" ${micTesting ? "disabled" : ""}>${micTesting ? "Testing… speak now" : `${ico("mic")} Test microphone (1.5s)`}</button>
      </div>
      ${verdict}
      ${devRows}
    </div>
    <div class="card"><h3>2 · Auto-paste</h3>
      <p class="muted">No accessibility permission needed on Windows — Ctrl+V injection always works.
      Toggle it in Settings → Auto-paste.</p>
    </div>
    <div class="card"><h3>3 · Speech model</h3>
      <p>${
        modelOk
          ? `<b>${esc(speech.name)}</b> ready${speech.online ? ` via ${esc(speech.engineLabel)}` : ""}.`
          : "No model ready yet."
      }</p>
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
      const used = historyModelLabel(h.model);
      return `<div class="hist-item">
        <div class="hist-text">${esc(h.text)}</div>
        <div class="hist-meta">
          <span>${fmtDate(h.date_unix)}</span>
          ${h.model ? `<span class="badge ${used.engine}">${esc(used.name)}</span>` : ""}
          ${h.duration_secs ? `<span>${fmtDur(h.duration_secs)}</span>` : ""}
          <span>${wordCount(h.text)} words</span>
          <span class="spacer"></span>
          ${h.audio_path ? `<button class="icon-btn" data-play="${esc(h.audio_path)}">${ico("play")} Play</button>` : ""}
          <button class="icon-btn" data-copy-hist="${idx}">Copy</button>
          <button class="icon-btn" data-correct-hist="${idx}">Correct</button>
          <button class="icon-btn" data-del-hist="${idx}">Delete</button>
        </div>
        ${correctingIdx === idx ? `<div class="correct-form">
          <label class="field">Heard<input type="text" id="corr-heard" value="${esc(h.raw_text || h.text)}"></label>
          <label class="field">Should be<input type="text" id="corr-written" placeholder="the correct spelling or phrase"></label>
          <button class="small" id="corr-add">Add to dictionary</button>
        </div>` : ""}
      </div>`;
    })
    .join("");
  refreshPlayLabels();
}

function viewDictionary(): string {
  const filtered = dict.filter((e) => (dictTab === "snippet") === (e.kind === "snippet"));
  const rows = filtered.length
    ? filtered
        .map(
          (e) => `<div class="dict-row${e.enabled ? "" : " dict-off"}">
        <button class="icon-btn star${e.starred ? " on" : ""}" data-dict-star="${e.id}" title="Star">${e.starred ? "Starred" : "Star"}</button>
        <input type="checkbox" data-dict-toggle="${e.id}" ${e.enabled ? "checked" : ""} title="Enable rule">
        <div class="dict-rule"><b>${esc(e.trigger)}</b><span class="arrow">→</span>${esc(e.replacement) || "<i class='muted'>(delete)</i>"}${e.usage ? ` <span class="muted">×${e.usage}</span>` : ""}</div>
        <button class="icon-btn" data-dict-del="${e.id}">Delete</button>
      </div>`
        )
        .join("")
    : `<div class="empty">${dictTab === "snippet" ? "No snippets yet. Longer expansions live here (email sign-offs, boilerplate)." : "No vocab yet. Example: say “my email” → get “you@example.com”."}</div>`;
  return `
    <h1>Dictionary</h1>
    <p class="page-sub">Vocab is short names and jargon. Snippets are longer expansions. Starred and most-used rise to the top. Runs fully offline.</p>
    <div class="filter-tabs">
      <button class="${dictTab === "vocab" ? "active" : ""}" id="dict-tab-vocab">Vocab</button>
      <button class="${dictTab === "snippet" ? "active" : ""}" id="dict-tab-snippet">Snippets</button>
    </div>
    <div class="card"><h3>Add ${dictTab === "snippet" ? "snippet" : "vocab"}</h3>
      <div class="add-form">
        <label class="field">You say<input type="text" id="dict-trigger" placeholder="${dictTab === "snippet" ? "sign off" : "my email"}" style="min-width:180px"></label>
        <label class="field">Inserted<input type="text" id="dict-repl" placeholder="${dictTab === "snippet" ? "Best regards, …" : "you@example.com"}" style="min-width:180px"></label>
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

function heroTiles(t: DayBucket): string {
  const cleaned = Math.max(0, t.raw_words - t.words);
  const extra =
    t.dict_hits > 0 || cleaned > 0
      ? `<div class="stat"><div class="num">${t.dict_hits.toLocaleString()}</div><div class="cap">Dictionary hits</div></div>
         ${cleaned > 0 ? `<div class="stat"><div class="num">${cleaned.toLocaleString()}</div><div class="cap">Words cleaned</div></div>` : ""}`
      : "";
  return `
    <div class="stat-grid">
      <div class="stat"><div class="num">${fmtConsumed(t.audio_secs)}</div><div class="cap">Minutes consumed</div></div>
      <div class="stat"><div class="num">${t.words.toLocaleString()}</div><div class="cap">Words dictated</div></div>
      <div class="stat"><div class="num">${fmtTimeSaved(t.words)}</div><div class="cap">Time saved @ 40 WPM</div></div>
      <div class="stat"><div class="num">${calcWpm(t.words, t.audio_secs) || "—"}</div><div class="cap">Your pace (WPM)</div></div>
      <div class="stat"><div class="num">${t.dictations.toLocaleString()}</div><div class="cap">Dictations</div></div>
      ${extra}
    </div>`;
}

function weekChart(days: number): string {
  const keys = dayKeys(days).reverse();
  const vals = keys.map((k) => usage?.by_day[k]?.audio_secs ?? 0);
  const words = keys.map((k) => usage?.by_day[k]?.words ?? 0);
  const max = Math.max(1, ...vals);
  const cols = keys
    .map((k, i) => {
      const label = new Date(k + "T12:00:00").toLocaleDateString(undefined, { weekday: "short" });
      const h = Math.round((vals[i] / max) * 100);
      const tip = `${label}: ${fmtConsumed(vals[i])}, ${words[i].toLocaleString()} words`;
      return `<div class="bar-col" title="${esc(tip)}">
        <div class="bar-count">${words[i] ? words[i].toLocaleString() : ""}</div>
        <div class="bar-track"><div class="bar-fill" style="height:${vals[i] > 0 ? Math.max(h, 8) : 0}%"></div></div>
        <div class="bar-day">${days > 7 ? k.slice(8) : label}</div>
      </div>`;
    })
    .join("");
  return `<div class="chart${days > 7 ? " chart-wide" : ""}">${cols}</div>`;
}

function heatmapHtml(): string {
  const weeks = 16;
  const today = noon(new Date());
  const start = shiftDays(today, -(today.getDay() + (weeks - 1) * 7));
  const cells: string[] = [];
  const { current } = streakInfo();
  const streakDays = new Set(dayKeys(current));
  for (let i = 0; i < weeks * 7; i++) {
    const d = shiftDays(start, i);
    const key = ymd(d);
    const future = d > today;
    const words = future ? 0 : usage?.by_day[key]?.words ?? 0;
    const lvl = future ? -1 : heatLevel(words);
    const glow = !future && current > 1 && streakDays.has(key);
    const title = future ? "" : `${key}: ${words} words`;
    cells.push(
      `<span class="heat-cell${lvl >= 0 ? ` b${lvl}` : " future"}${glow ? " streak" : ""}" title="${esc(title)}"></span>`
    );
  }
  return `<div class="heat" style="grid-template-rows:repeat(7,12px)">${cells.join("")}</div>
    <div class="heat-legend">
      <span>Less</span>
      <span class="heat-cell b0"></span><span class="heat-cell b1"></span>
      <span class="heat-cell b2"></span><span class="heat-cell b3"></span>
      <span class="heat-cell b4"></span>
      <span>More</span>
    </div>`;
}

function viewHome(): string {
  const all = periodTotals("all");
  const { current } = streakInfo();
  const wpm = calcWpm(all.words, all.audio_secs);
  const q = homeQuery.trim().toLowerCase();
  const items = (q ? history.filter((h) => h.text.toLowerCase().includes(q)) : history).slice(0, 60);
  const speech = activeSpeech();
  const ready = speech.ready;
  const groups: { head: string; items: { h: HistoryItem; idx: number }[] }[] = [];
  for (const h of items) {
    const head = dayHeading(h.date_unix);
    const last = groups[groups.length - 1];
    if (!last || last.head !== head) groups.push({ head, items: [] });
    groups[groups.length - 1].items.push({ h, idx: history.indexOf(h) });
  }
  const search = `<div class="timeline-search">${ico("search")}<input type="text" id="home-q" placeholder="Search" value="${esc(homeQuery)}"></div>`;
  const feed = groups.length
    ? groups
        .map(
          (g, gi) => `<div class="timeline-head"><span>${g.head}</span>${gi === 0 ? search : ""}</div>${g.items
            .map(
              ({ h, idx }) => `<div class="tl-row">
            <div class="tl-time">${fmtClockTime(h.date_unix)}</div>
            <div class="tl-text">${esc(h.text)}</div>
            <div class="tl-actions"><button class="icon-only" data-copy-hist="${idx}" title="Copy">${ico("copy")}</button></div>
          </div>`
            )
            .join("")}`
        )
        .join("")
    : `<div class="timeline-head"><span>TODAY</span>${search}</div>
       <div class="empty">${q ? "No matches." : `Nothing yet — hold ${esc(talkKey())} and speak.`}</div>`;
  return `
    <div class="home">
      <div class="home-feed">
        <div class="greeting">${greeting()}</div>
        <div class="hero-banner">
          <h2>${ready ? "Dictate anywhere on Windows" : "Set up DictFlow in a minute"}</h2>
          <p>${
            ready
              ? speech.online
                ? `Hold ${esc(talkKey())} and speak. Audio is transcribed by <b>${esc(speech.name)}</b> via ${esc(speech.engineLabel)}.`
                : `Hold ${esc(talkKey())} and speak. Text lands in the focused app. Everything stays on this PC.`
              : "Download a speech model first. After that, dictation is fully offline."
          }</p>
          <div><button class="light" id="${ready ? "hero-dictate" : "goto-models"}">${ready ? "Start now" : "Open AI Models"}</button></div>
        </div>
        ${feed}
      </div>
      <aside class="home-rail">
        <div class="rail-stat"><div class="n">${fmtCompact(all.words)}</div><div class="c">total words</div></div>
        <div class="rail-stat"><div class="n">${wpm || "—"}</div><div class="c">wpm</div></div>
        <div class="rail-stat"><div class="n">${current || 0}</div><div class="c">day streak</div></div>
        <div class="rail-card">
          <h4>${ready ? esc(speech.name) : "No model yet"}</h4>
          <p>${
            ready
              ? speech.online
                ? `Hold the talk key in any app. Speech uses ${esc(speech.engineLabel)} (${esc(speech.name)}).`
                : "Hold the talk key in any app. Text never leaves this PC."
              : "Download a model from AI Models to start dictating offline."
          }</p>
          <button class="accent small" id="goto-history">View history</button>
        </div>
      </aside>
    </div>`;
}

function viewStats(): string {
  const t = periodTotals(statsPeriod);
  const perModel = new Map<string, number>();
  history.forEach((h) => {
    const key = h.model || "unknown";
    perModel.set(key, (perModel.get(key) ?? 0) + 1);
  });
  const rows = [...perModel.entries()]
    .map(([id, n]) => {
      const used = historyModelLabel(id);
      return `<div class="dict-row"><div class="dict-rule">${esc(used.name || "unknown")}</div><b>${n}</b></div>`;
    })
    .join("");
  const { current, longest } = streakInfo();
  const chips: [StatsPeriod, string][] = [
    ["today", "Today"],
    ["7d", "7 days"],
    ["30d", "30 days"],
    ["all", "All time"],
  ];
  const chartDays = statsPeriod === "30d" || statsPeriod === "all" ? 30 : 7;
  return `
    <h1>Insights</h1>
    <p class="page-sub">Minutes, pace, and time saved. Lifetime numbers survive Clear all.</p>
    <div class="filter-tabs" id="stats-period">
      ${chips.map(([id, l]) => `<button class="${statsPeriod === id ? "active" : ""}" data-period="${id}">${l}</button>`).join("")}
    </div>
    ${heroTiles(t)}
    <div class="card">
      <h3>${chartDays === 30 ? "Last 30 days" : "Last 7 days"}</h3>
      <p class="week-sub">Bar height is minutes spoken; numbers on top are words.</p>
      ${weekChart(chartDays)}
    </div>
    <div class="card">
      <h3>Activity</h3>
      <p class="week-sub">Current streak ${current} day${current === 1 ? "" : "s"} · longest ${longest}.</p>
      ${heatmapHtml()}
    </div>
    <div class="card"><h3>Dictations per model</h3>
      <p class="week-sub">From History (last ${history.length} saved takes), not the lifetime ledger.</p>
      ${rows || `<div class="empty">No data yet.</div>`}
    </div>`;
}

function viewSettings(): string {
  if (!settings) return `<div class="empty">Loading…</div>`;
  const speech = activeSpeech();
  const modelOpts =
    (speech.online
      ? `<option value="__online__" selected>Online · ${esc(speech.name)} (${esc(speech.engineLabel)})</option>`
      : "") +
    models
      .map(
        (m) =>
          `<option value="${m.id}" ${!speech.online && m.id === settings!.model_id ? "selected" : ""}>${esc(m.name)} (${m.engine})${m.downloaded ? "" : " — not downloaded"}</option>`
      )
      .join("");
  const langOpts = LANGUAGES.map(
    ([v, l]) => `<option value="${v}" ${settings!.language === v ? "selected" : ""}>${l}</option>`
  ).join("");
  const llmReady = settings.llm_backend !== "off";
  const cleanupOpts = CLEANUPS.map(
    ([v, l]) => `<option value="${v}" ${settings!.cleanup === v ? "selected" : ""}>${l}</option>`
  ).join("") +
    `<option value="medium" ${settings.cleanup === "medium" ? "selected" : ""} ${llmReady ? "" : "disabled"}>Medium — LLM clean${llmReady ? "" : " (set Models → LLM)"}</option>
     <option value="high" ${settings.cleanup === "high" ? "selected" : ""} ${llmReady ? "" : "disabled"}>High — LLM professional${llmReady ? "" : " (set Models → LLM)"}</option>`;
  const hotkeyOpts = HOTKEYS.map(
    ([v, l]) => `<option value="${v}" ${settings!.hotkey_key === v ? "selected" : ""}>${l}</option>`
  ).join("");
  const modeOpts = [["hold", "Hold to talk"], ["toggle", "Toggle"]]
    .map(([v, l]) => `<option value="${v}" ${settings!.recording_mode === v ? "selected" : ""}>${l}</option>`)
    .join("");
  const micOpts = `<option value="" ${!settings.audio_device ? "selected" : ""}>Windows default</option>` +
    (audioDevices ?? [])
      .map((d) => `<option value="${esc(d.name)}" ${settings!.audio_device === d.name ? "selected" : ""}>${esc(d.name)}${d.is_default ? " (default)" : ""}</option>`)
      .join("");
  const edgeOpts = OVERLAY_EDGES.map(
    ([v, l]) => `<option value="${v}" ${settings!.overlay_edge === v ? "selected" : ""}>${l}</option>`
  ).join("");
  const pasteOpts = UTILITY_KEYS.map(
    ([v, l]) => `<option value="${v}" ${settings!.paste_last_key === v ? "selected" : ""}>${l}</option>`
  ).join("");
  const copyOpts = UTILITY_KEYS.map(
    ([v, l]) => `<option value="${v}" ${settings!.copy_last_key === v ? "selected" : ""}>${l}</option>`
  ).join("");
  return `
    <h1>Settings</h1>
    <p class="page-sub">Everything stays on this PC. No accounts, no telemetry.</p>
    <div class="card"><h3>Transcription</h3>
      <div class="set-row"><div><b>Model</b><div class="desc">${
        speech.online
          ? `Speech uses <b>${esc(speech.name)}</b> via ${esc(speech.engineLabel)}. Pick a local model below to switch back.`
          : `Currently <b>${esc(speech.name)}</b>. Parakeet v3 is the best all-rounder; Whisper needs its binary (below).`
      }</div></div>
        <select id="set-model">${modelOpts}</select></div>
      <div class="set-row"><div><b>Language</b><div class="desc">Whisper source language. Parakeet v3 auto-detects.</div></div>
        <select id="set-lang">${langOpts}</select></div>
      <div class="set-row"><div><b>Auto-paste</b><div class="desc">Type the result into the focused app via Ctrl+V right after transcribing.</div></div>
        <input type="checkbox" id="set-paste" ${settings.auto_paste ? "checked" : ""}></div>
      <div class="set-row"><div><b>Cleanup</b><div class="desc">None / Light / Full are local rules. Medium and High need an LLM (Models → LLM, coming next).</div></div>
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
    <div class="card"><h3>Microphone</h3>
      <div class="set-row"><div><b>Input device</b><div class="desc">Leave as Windows default, or pin a headset. If that device is unplugged, the next take falls back automatically.</div></div>
        <select id="set-mic">${micOpts}</select></div>
    </div>
    <div class="card"><h3>Overlay &amp; shortcuts</h3>
      <div class="set-row"><div><b>Floating pill</b><div class="desc">Small always-on-top meter. Click to start or stop talking. Drag to dock. Idle is a quiet line; talking draws a gold wave.</div></div>
        <input type="checkbox" id="set-overlay" ${settings.overlay_enabled ? "checked" : ""}></div>
      <div class="set-row"><div><b>Dock edge</b><div class="desc">Where the pill sits. Dragging also updates this.</div></div>
        <select id="set-edge">${edgeOpts}</select></div>
      <div class="set-row"><div><b>Paste last</b><div class="desc">Default is Shift+Alt+Z. Pastes the newest transcript into the focused app.</div></div>
        <select id="set-paste-last">${pasteOpts}</select></div>
      <div class="set-row"><div><b>Copy last</b><div class="desc">Default is Shift+Alt+X. Copies without pasting.</div></div>
        <select id="set-copy-last">${copyOpts}</select></div>
      <div class="set-row"><div><b>20-minute cap</b><div class="desc">Always warns at 19 minutes. Turn this on to auto-finish (keep) the take at 20 minutes. Off = unlimited.</div></div>
        <input type="checkbox" id="set-cap" ${settings.session_cap ? "checked" : ""}></div>
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

function viewOnboard(): string {
  const recId = status?.recommended_model || "parakeet-v3";
  const rec = models.find((m) => m.id === recId);
  const recReady = Boolean(rec?.downloaded);
  const recDl = dlBars[recId];
  const hotkeyOpts = HOTKEYS.map(
    ([v, l]) => `<option value="${v}" ${settings?.hotkey_key === v ? "selected" : ""}>${l}</option>`
  ).join("");
  const modeOpts = [["hold", "Hold to talk"], ["toggle", "Toggle"]]
    .map(([v, l]) => `<option value="${v}" ${settings?.recording_mode === v ? "selected" : ""}>${l}</option>`)
    .join("");
  const devices =
    audioDevices === null
      ? `<p class="muted">Click Refresh to list microphones.</p>`
      : audioDevices.length === 0
        ? `<p class="muted">No input devices — check the Windows microphone privacy toggle.</p>`
        : audioDevices
            .map(
              (d) => `<button class="dict-row device-pick${d.is_selected ? " selected" : ""}" data-pick-mic="${esc(d.name)}"><div class="dict-rule"><b>${esc(d.name)}</b></div>${d.is_selected ? `<span class="badge">selected</span>` : ""}</button>`
            )
            .join("");
  const verdict = !micTest
    ? ""
    : micTest.peak > 0.02
      ? `<p style="color:var(--green)">Heard you on <b>${esc(micTest.device)}</b>.</p>`
      : `<p style="color:var(--amber)">Mic opened but peak was low — unmute and try again.</p>`;
  let body = "";
  if (onboardStep === "welcome") {
    body = `<h1>Welcome to DictFlow</h1>
      <p class="page-sub">Hold a key, speak, and polished text lands in whichever app has focus. After you download a speech model, nothing leaves this PC.</p>
      <div class="card"><p>Four quick steps: pick a talk key, test the mic, download the recommended model. You can skip any of them.</p></div>`;
  } else if (onboardStep === "talkkey") {
    body = `<h1>Talk key</h1>
      <p class="page-sub">Right Ctrl is the safest default — it never types a character. Hold to talk, release to paste.</p>
      <div class="card">
        <div class="set-row"><div><b>Key</b></div><select id="on-hotkey">${hotkeyOpts}</select></div>
        <div class="set-row"><div><b>Mode</b></div><select id="on-mode">${modeOpts}</select></div>
      </div>`;
  } else if (onboardStep === "mictest") {
    body = `<h1>Microphone</h1>
      <p class="page-sub">Windows uses one global toggle: Settings → Privacy &amp; security → Microphone → Let desktop apps access your microphone.</p>
      <div class="card">
        <div class="row" style="margin-bottom:12px">
          <button class="small" id="mic-refresh">Refresh devices</button>
          <button class="small" id="mic-open-settings">Open microphone settings</button>
          <button class="small" id="mic-test" ${micTesting ? "disabled" : ""}>${micTesting ? "Testing… speak now" : "Test microphone (1.5s)"}</button>
        </div>
        ${verdict}${devices}
      </div>`;
  } else {
    const progress = recDl
      ? `<div class="progress"><div id="dlbar-${recId}" style="width:${recDl.pct ?? 0}%"></div></div>
         <div class="dl-file">${esc(recDl.file)}${recDl.pct != null ? ` — ${recDl.pct}%` : ""}</div>`
      : "";
    body = `<h1>Speech model</h1>
      <p class="page-sub">Parakeet v3 is the recommended all-rounder (~670 MB, 25 languages). After this, dictation is fully offline.</p>
      <div class="card">
        <div class="model-name">${esc(rec?.name ?? recId)}</div>
        <p class="muted">${esc(rec?.essence ?? "Best all-rounder")} • ${esc(rec?.size_label ?? "~670 MB")}</p>
        ${progress}
        <div class="row" style="margin-top:12px">
          ${recReady ? `<span class="muted">Ready on disk.</span>` : recDl
            ? `<button class="ghost small" data-cancel-dl="${recId}">Cancel</button>`
            : `<button class="small" data-dl="${recId}">Download recommended</button>`}
        </div>
      </div>`;
  }
  const back = onboardStep === "welcome" ? "" : `<button class="ghost" id="on-back">Back</button>`;
  const nextLabel =
    onboardStep === "download" ? (recReady ? "Finish" : "Skip and finish") : "Continue";
  return `<div class="onboard">
    ${body}
    <div class="row onboard-nav">${back}
      <button class="ghost" id="on-skip">Skip setup</button>
      <button id="on-next">${nextLabel}</button>
    </div>
  </div>`;
}

function render(): void {
  if (settings && !settings.onboarded) {
    view = "onboard";
    document.getElementById("app")!.innerHTML = `<main class="main onboard-shell" id="view"></main><div id="toasts"></div>`;
    renderView();
    return;
  }
  document.getElementById("app")!.innerHTML = `
    <aside class="sidebar">
      <div class="brand">
        <div class="brand-mark"><img src="${logoUrl}" alt="DictFlow logo"></div>
        <div class="brand-name">DictFlow</div>
        <span class="brand-pill" id="side-pill">${activeSpeech().online ? "Online" : "Offline"}</span>
      </div>
      <nav class="nav">${NAV_MAIN.map(navBtn).join("")}</nav>
      <nav class="nav nav-foot">${NAV_FOOT.map(navBtn).join("")}</nav>
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
  if (view === "onboard") box.innerHTML = viewOnboard();
  else if (view === "home") box.innerHTML = viewHome();
  else if (view === "dictate") box.innerHTML = viewDictate();
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
  (document.getElementById("cancel-rec") as HTMLButtonElement | null)?.addEventListener("click", async () => {
    const msg = await call<string>("cancel_dictation");
    if (msg !== null) {
      toast("Take discarded", "info");
      await refreshStatus();
      if (view === "dictate") renderView();
    }
  });
  (document.getElementById("copy-last") as HTMLButtonElement | null)?.addEventListener("click", () => {
    if (lastResult) copyText(lastResult);
  });
  (document.getElementById("goto-models") as HTMLButtonElement | null)?.addEventListener("click", () => {
    view = "models";
    render();
  });
  (document.getElementById("file-btn") as HTMLButtonElement | null)?.addEventListener("click", transcribeFile);
  (document.getElementById("goto-history") as HTMLElement | null)?.addEventListener("click", (e) => {
    e.preventDefault();
    view = "history";
    render();
  });
  (document.getElementById("hero-dictate") as HTMLButtonElement | null)?.addEventListener("click", () => {
    view = "dictate";
    render();
  });
  const hq = document.getElementById("home-q") as HTMLInputElement | null;
  hq?.addEventListener("input", () => {
    homeQuery = hq.value;
    const pos = hq.selectionStart;
    renderView();
    const next = document.getElementById("home-q") as HTMLInputElement | null;
    if (next) {
      next.focus();
      const caret = pos ?? homeQuery.length;
      next.setSelectionRange(caret, caret);
    }
  });
  document.querySelectorAll("[data-period]").forEach((b) =>
    (b as HTMLButtonElement).onclick = () => {
      statsPeriod = (b as HTMLButtonElement).dataset.period as StatsPeriod;
      renderView();
    }
  );
  // Models
  document.querySelectorAll("[data-dl]").forEach((b) =>
    (b as HTMLButtonElement).onclick = () => downloadModel((b as HTMLButtonElement).dataset.dl!)
  );
  document.querySelectorAll("[data-cancel-dl]").forEach((b) =>
    (b as HTMLButtonElement).onclick = async () => {
      const id = (b as HTMLButtonElement).dataset.cancelDl!;
      const msg = await call<string>("cancel_download", { id });
      if (msg !== null) toast("Download cancelled", "info");
    }
  );
  document.querySelectorAll("[data-pick-mic]").forEach((b) =>
    (b as HTMLButtonElement).onclick = () => pickAudioDevice((b as HTMLButtonElement).dataset.pickMic ?? null)
  );
  (document.getElementById("mic-use-default") as HTMLButtonElement | null)?.addEventListener("click", () => {
    pickAudioDevice(null);
  });
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
  document.getElementById("pane-audio")?.addEventListener("click", () => {
    modelsPane = "audio";
    renderView();
  });
  document.getElementById("pane-llm")?.addEventListener("click", () => {
    modelsPane = "llm";
    renderView();
  });
  const saveAudioApi = async () => {
    if (!settings) return;
    const be = (document.querySelector("input[name=audio-be]:checked") as HTMLInputElement | null)?.value ?? settings.audio_backend;
    const res = await call<Settings>("set_settings", {
      settings: {
        ...settings,
        audio_backend: be,
        audio_api_base: (document.getElementById("audio-base") as HTMLInputElement | null)?.value ?? settings.audio_api_base,
        audio_api_model: (document.getElementById("audio-model") as HTMLInputElement | null)?.value ?? settings.audio_api_model,
      },
    });
    if (res) settings = res;
    const key = (document.getElementById("audio-key") as HTMLInputElement | null)?.value ?? "";
    if (key.trim()) await call("set_provider_secrets", { audioApiKey: key, llmApiKey: null });
    await refreshProvider();
    await refreshStatus();
    await refreshModels();
    toast("Audio source saved", "success");
    renderView();
  };
  const saveLlm = async () => {
    if (!settings) return;
    const be = (document.querySelector("input[name=llm-be]:checked") as HTMLInputElement | null)?.value ?? settings.llm_backend;
    const resolved = resolveSavedPrompt(
      (document.getElementById("llm-preset") as HTMLSelectElement | null)?.value ?? settings.llm_preset,
      (document.getElementById("llm-custom") as HTMLTextAreaElement | null)?.value ?? settings.llm_custom_prompt,
    );
    const res = await call<Settings>("set_settings", {
      settings: {
        ...settings,
        llm_backend: be,
        llm_api_base: (document.getElementById("llm-base") as HTMLInputElement | null)?.value ?? settings.llm_api_base,
        llm_api_model: (document.getElementById("llm-model") as HTMLInputElement | null)?.value ?? settings.llm_api_model,
        llm_preset: resolved.preset,
        llm_custom_prompt: resolved.custom,
        llm_enabled: (document.getElementById("llm-on") as HTMLInputElement | null)?.checked ?? settings.llm_enabled,
      },
    });
    if (res) settings = res;
    const key = (document.getElementById("llm-key") as HTMLInputElement | null)?.value ?? "";
    if (key.trim()) await call("set_provider_secrets", { audioApiKey: null, llmApiKey: key });
    await refreshProvider();
    toast("LLM settings saved", "success");
    renderView();
  };
  document.getElementById("audio-save")?.addEventListener("click", () => saveAudioApi().catch(() => undefined));
  document.getElementById("audio-host")?.addEventListener("change", () => {
    const host = AUDIO_HOSTS.find((h) => h.id === (document.getElementById("audio-host") as HTMLSelectElement).value);
    const base = document.getElementById("audio-base") as HTMLInputElement | null;
    const model = document.getElementById("audio-model") as HTMLInputElement | null;
    if (!host || host.id === "custom") return;
    if (base) base.value = host.base;
    if (model) model.value = host.model;
    if (settings) {
      settings.audio_api_base = host.base;
      settings.audio_api_model = host.model;
    }
  });
  document.getElementById("llm-save")?.addEventListener("click", () => saveLlm().catch(() => undefined));
  document.querySelectorAll("input[name=audio-be], input[name=llm-be]").forEach((el) => {
    el.addEventListener("change", () => {
      if (!settings) return;
      const audioBe = (document.querySelector("input[name=audio-be]:checked") as HTMLInputElement | null)?.value;
      const llmBe = (document.querySelector("input[name=llm-be]:checked") as HTMLInputElement | null)?.value;
      if (audioBe) settings.audio_backend = audioBe;
      if (llmBe) settings.llm_backend = llmBe;
      const base = document.getElementById("audio-base") as HTMLInputElement | null;
      const model = document.getElementById("audio-model") as HTMLInputElement | null;
      if (base) settings.audio_api_base = base.value;
      if (model) settings.audio_api_model = model.value;
      const lb = document.getElementById("llm-base") as HTMLInputElement | null;
      const lm = document.getElementById("llm-model") as HTMLInputElement | null;
      const lp = document.getElementById("llm-preset") as HTMLSelectElement | null;
      const lc = document.getElementById("llm-custom") as HTMLTextAreaElement | null;
      const lo = document.getElementById("llm-on") as HTMLInputElement | null;
      if (lb) settings.llm_api_base = lb.value;
      if (lm) settings.llm_api_model = lm.value;
      if (lp && lc) {
        const resolved = resolveSavedPrompt(lp.value, lc.value);
        settings.llm_preset = resolved.preset;
        settings.llm_custom_prompt = resolved.custom;
      }
      if (lo) settings.llm_enabled = lo.checked;
      renderView();
    });
  });
  document.getElementById("llm-preset")?.addEventListener("change", () => {
    const sel = document.getElementById("llm-preset") as HTMLSelectElement;
    const ta = document.getElementById("llm-custom") as HTMLTextAreaElement | null;
    if (!ta) return;
    if (sel.value === "custom") {
      ta.value = settings?.llm_custom_prompt?.trim() || processPromptFor("clean", "");
    } else {
      ta.value = processPromptFor(sel.value, "");
    }
  });
  document.getElementById("llm-prompt-reset")?.addEventListener("click", async () => {
    const sel = document.getElementById("llm-preset") as HTMLSelectElement | null;
    const ta = document.getElementById("llm-custom") as HTMLTextAreaElement | null;
    const def = provider?.default_prompt || DEFAULT_PROCESS_PROMPT;
    if (sel) sel.value = "clean";
    if (ta) ta.value = def;
    if (settings) {
      settings.llm_preset = "clean";
      settings.llm_custom_prompt = "";
    }
    await saveLlm();
  });
  document.getElementById("audio-test")?.addEventListener("click", async () => {
    await saveAudioApi();
    toast("Speak now — recording 1.5 seconds", "info");
    const btn = document.getElementById("audio-test") as HTMLButtonElement | null;
    if (btn) {
      btn.disabled = true;
      btn.textContent = "Listening…";
    }
    const msg = await call<string>("test_audio_api");
    if (btn) {
      btn.disabled = false;
      btn.textContent = "Test — speak 1.5s";
    }
    if (msg !== null) toast(msg, "success");
    else if (btn) renderView();
  });
  document.getElementById("llm-test")?.addEventListener("click", async () => {
    await saveLlm();
    const msg = await call<string>("test_llm");
    if (msg !== null) toast(msg, "success");
  });
  // Setup
  (document.getElementById("mic-refresh") as HTMLButtonElement | null)?.addEventListener("click", refreshAudioDevices);
  (document.getElementById("mic-open-settings") as HTMLButtonElement | null)?.addEventListener("click", async () => {
    await call("open_mic_settings");
  });
  (document.getElementById("mic-test") as HTMLButtonElement | null)?.addEventListener("click", async () => {
    micTesting = true;
    micTest = null;
    if (view === "setup" || view === "onboard") renderView();
    const res = await call<MicTest>("test_microphone");
    micTesting = false;
    if (res) {
      micTest = res;
      if (res.peak <= 0.02) toast("Microphone test heard nothing — check mic + privacy toggle", "error");
      else toast("Microphone test OK", "success");
    }
    if (view === "setup" || view === "onboard") renderView();
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
    toast("History cleared — Insights totals kept", "success");
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
    const wholeWord = (document.getElementById("dict-ww") as HTMLInputElement).checked;
    const res = await call<DictionaryEntry[]>("add_dictionary_entry", { trigger, replacement, wholeWord, kind: dictTab });
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
  document.getElementById("dict-tab-vocab")?.addEventListener("click", () => {
    dictTab = "vocab";
    renderView();
  });
  document.getElementById("dict-tab-snippet")?.addEventListener("click", () => {
    dictTab = "snippet";
    renderView();
  });
  document.querySelectorAll("[data-dict-star]").forEach((b) =>
    (b as HTMLButtonElement).onclick = async () => {
      const id = (b as HTMLButtonElement).dataset.dictStar!;
      const cur = dict.find((e) => e.id === id);
      const res = await call<DictionaryEntry[]>("star_dictionary_entry", { id, starred: !cur?.starred });
      if (res) dict = res;
      renderView();
    }
  );
  document.querySelectorAll("[data-correct-hist]").forEach((b) =>
    (b as HTMLButtonElement).onclick = () => {
      const idx = Number((b as HTMLButtonElement).dataset.correctHist);
      correctingIdx = correctingIdx === idx ? null : idx;
      renderView();
    }
  );
  (document.getElementById("corr-add") as HTMLButtonElement | null)?.addEventListener("click", async () => {
    const heard = (document.getElementById("corr-heard") as HTMLInputElement | null)?.value ?? "";
    const written = (document.getElementById("corr-written") as HTMLInputElement | null)?.value ?? "";
    const res = await call<DictionaryEntry[]>("add_correction", { heard, written });
    if (res) {
      dict = res;
      correctingIdx = null;
      toast("Added to dictionary", "success");
      renderView();
    }
  });
  // Settings
  const sm = document.getElementById("set-model") as HTMLSelectElement | null;
  const sl = document.getElementById("set-lang") as HTMLSelectElement | null;
  const sp = document.getElementById("set-paste") as HTMLInputElement | null;
  const sc = document.getElementById("set-cleanup") as HTMLSelectElement | null;
  const st = document.getElementById("set-translate") as HTMLInputElement | null;
  const sk = document.getElementById("set-hotkey") as HTMLSelectElement | null;
  const smo = document.getElementById("set-mode") as HTMLSelectElement | null;
  const smic = document.getElementById("set-mic") as HTMLSelectElement | null;
  const sov = document.getElementById("set-overlay") as HTMLInputElement | null;
  const sedge = document.getElementById("set-edge") as HTMLSelectElement | null;
  const spl = document.getElementById("set-paste-last") as HTMLSelectElement | null;
  const scl = document.getElementById("set-copy-last") as HTMLSelectElement | null;
  const scap = document.getElementById("set-cap") as HTMLInputElement | null;
  const saveSettings = async () => {
    if (!settings || !sm || !sl || !sp || !sc || !st || !sk || !smo) return;
    const pickLocal = sm.value && sm.value !== "__online__";
    const next: Settings = {
      ...settings,
      model_id: pickLocal ? sm.value : settings.model_id,
      audio_backend: pickLocal ? "local" : settings.audio_backend,
      language: sl.value,
      auto_paste: sp.checked,
      cleanup: sc.value,
      translate: st.checked,
      hotkey_key: sk.value,
      recording_mode: smo.value,
      audio_device: smic?.value ? smic.value : null,
      overlay_enabled: sov?.checked ?? settings.overlay_enabled,
      overlay_edge: sedge?.value ?? settings.overlay_edge,
      paste_last_key: spl?.value ?? settings.paste_last_key,
      copy_last_key: scl?.value ?? settings.copy_last_key,
      session_cap: scap?.checked ?? settings.session_cap,
    };
    const res = await call<Settings>("set_settings", { settings: next });
    if (res) {
      settings = res;
      await refreshModels();
      await refreshStatus();
      renderView();
    }
  };
  sm?.addEventListener("change", saveSettings);
  sl?.addEventListener("change", saveSettings);
  sp?.addEventListener("change", saveSettings);
  sc?.addEventListener("change", saveSettings);
  st?.addEventListener("change", saveSettings);
  sk?.addEventListener("change", saveSettings);
  smo?.addEventListener("change", saveSettings);
  smic?.addEventListener("change", saveSettings);
  sov?.addEventListener("change", saveSettings);
  sedge?.addEventListener("change", saveSettings);
  spl?.addEventListener("change", saveSettings);
  scl?.addEventListener("change", saveSettings);
  scap?.addEventListener("change", saveSettings);

  const onHotkey = document.getElementById("on-hotkey") as HTMLSelectElement | null;
  const onMode = document.getElementById("on-mode") as HTMLSelectElement | null;
  const saveOnboardTalk = async () => {
    if (!settings || !onHotkey || !onMode) return;
    const res = await call<Settings>("set_settings", {
      settings: { ...settings, hotkey_key: onHotkey.value, recording_mode: onMode.value },
    });
    if (res) settings = res;
  };
  onHotkey?.addEventListener("change", saveOnboardTalk);
  onMode?.addEventListener("change", saveOnboardTalk);
  (document.getElementById("on-skip") as HTMLButtonElement | null)?.addEventListener("click", finishOnboarding);
  (document.getElementById("on-back") as HTMLButtonElement | null)?.addEventListener("click", async () => {
    const next = await call<string>("onboard_step", { current: onboardStep, backward: true });
    if (next && next !== "done") onboardStep = next as OnboardStep;
    renderView();
  });
  (document.getElementById("on-next") as HTMLButtonElement | null)?.addEventListener("click", async () => {
    if (onboardStep === "talkkey") await saveOnboardTalk();
    const next = await call<string>("onboard_step", { current: onboardStep, backward: false });
    if (!next || next === "done") {
      await finishOnboarding();
      return;
    }
    onboardStep = next as OnboardStep;
    if (onboardStep === "mictest" && !audioDevices) await refreshAudioDevices();
    renderView();
  });
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
  document.addEventListener("contextmenu", (e) => {
    const t = e.target as HTMLElement | null;
    if (t && t.closest("input, textarea, [contenteditable]")) return;
    e.preventDefault();
  });
  render();
  await Promise.all([refreshStatus(), refreshModels(), refreshHistory(), refreshStats(), refreshDict(), refreshSettings(), refreshAudioDevices(), refreshProvider()]);
  render();

  await listen<boolean>("dictflow://recording", (e) => {
    if (e.payload) startTimer();
    else stopTimer();
    refreshStatus();
  });
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
    if ((view === "models" || view === "onboard") && !bar) renderView();
  });
  await listen("dictflow://history-updated", () => {
    refreshHistory();
    refreshStats();
  });
  await listen<{ peak: number; rms: number }>("dictflow://level", (e) => {
    livePeak = e.payload.peak;
    const fill = document.getElementById("level-fill");
    if (fill) fill.style.width = `${Math.round(Math.min(livePeak, 1) * 100)}%`;
  });
  await listen<{ text: string; chip_ms?: number }>("dictflow://committed", (e) => {
    lastResult = e.payload.text;
    if (view === "dictate") renderView();
  });
  await listen("dictflow://session-warn", () => {
    toast("19 minutes — still recording. Esc cancels, talk key finishes.", "info");
  });
  await listen<{ from: string; to: string }>("dictflow://device-fallback", (e) => {
    const { from, to } = e.payload;
    toast(from ? `Mic “${from}” gone — using ${to}` : to, "info");
    refreshAudioDevices();
    refreshStatus();
  });

  setInterval(refreshStatus, 2000);
}

boot();
