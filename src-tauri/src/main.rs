#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// DictFlow — offline voice dictation for Windows (Wispr Flow-style, 100% local).
// Stack: Tauri v2 (WebView2) + Rust backend + whisper.cpp sidecar (Whisper)
//        + in-process sherpa-onnx (Parakeet) + cpal (WASAPI) + SendInput paste.
//
// Pipeline (mirrors SpeakType): hotkey → mic capture → STT engine →
// spoken commands / backtrack / lists → dictionary → auto-edit → paste.

mod devices;
mod focus;
mod format;
mod models;
mod onboarding;
mod overlay;
mod polish;
mod prompts;
mod session;
mod shortcuts;
mod stats;
mod text;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;

use anyhow::Context;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager, State};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

use models::{EngineKind, ModelEntry};
use stats::UsageStats;

const HOTKEY: &str = "Ctrl+Alt+Space";
const HISTORY_LIMIT: usize = 100;

// ---------------------------------------------------------------------------
// Single-key push-to-talk (macOS Fn-key feel)
//
// `RegisterHotKey` (used by tauri-plugin-global-shortcut) cannot register bare
// modifier keys, so single-key talk buttons are polled with GetAsyncKeyState
// (the Quill pattern) on a dedicated thread — no extra dependencies, raw FFI:
//
// - mode "hold" (default): key down starts, key up stops + transcribes.
// - mode "toggle": key press toggles. Key release is ignored.
// - "CtrlAltSpace" keeps the old plugin-combo behavior instead of polling.
// ---------------------------------------------------------------------------

/// Virtual-key code for a configured talk key, or `None` for the combo.
fn hotkey_vk(key: &str) -> Option<i32> {
    match key {
        "LeftCtrl" => Some(0xA2),
        "RightCtrl" => Some(0xA3),
        "LeftAlt" => Some(0xA4),
        "RightAlt" => Some(0xA5),
        "ScrollLock" => Some(0x91),
        "F9" => Some(0x78),
        _ => None, // "CtrlAltSpace" and anything unknown → plugin shortcut
    }
}

#[link(name = "user32")]
extern "system" {
    fn GetAsyncKeyState(v_key: i32) -> i16;
    fn GetKeyboardState(lp_key_state: *mut u8) -> i32;
}

/// Talk-key edge events forwarded to the async consumer.
#[derive(Debug, Clone, Copy)]
enum TalkEdge {
    Pressed,
    Released,
    /// Another key went down while the talk key was held (e.g. Ctrl+C on a
    /// Left-Ctrl talk key) — discard, don't transcribe. Mirrors SpeakType's
    /// modifier-combo cancel. Only applies to hotkey-owned takes.
    Cancel,
    /// Esc while any recording is live — dedicated discard (P1).
    Escape,
}

/// Full 256-key snapshot in a single syscall (cheaper than 256 polls).
fn snapshot_keys() -> [u8; 256] {
    let mut buf = [0u8; 256];
    // SAFETY: buf is a valid 256-byte array; the API fills it synchronously.
    unsafe {
        GetKeyboardState(buf.as_mut_ptr());
    }
    buf
}

/// Poll the configured single key and forward press/release edges. Reads the
/// current setting every tick so changes apply without a restart.
fn spawn_hotkey_thread(
    app: tauri::AppHandle,
    tx: tokio::sync::mpsc::UnboundedSender<TalkEdge>,
) {
    std::thread::Builder::new()
        .name("dictflow-hotkey".to_owned())
        .spawn(move || {
            let mut was_down = false;
            let mut was_esc = false;
            let mut last_change = std::time::Instant::now();
            let mut last_esc = std::time::Instant::now();
            let mut prev_keys = snapshot_keys();
            loop {
                std::thread::sleep(Duration::from_millis(10));
                remember_paste_target(&app);
                let (vk, recording) = app
                    .state::<Mutex<AppState>>()
                    .lock()
                    .map(|s| (hotkey_vk(&s.settings.hotkey_key), s.recording))
                    .unwrap_or((None, false));
                // Esc is polled only while a take is live so we never steal it
                // from other apps when idle.
                if recording {
                    let esc = unsafe { GetAsyncKeyState(0x1B) } < 0;
                    if esc && !was_esc && last_esc.elapsed() > Duration::from_millis(30) {
                        last_esc = std::time::Instant::now();
                        if tx.send(TalkEdge::Escape).is_err() {
                            break;
                        }
                    }
                    was_esc = esc;
                } else {
                    was_esc = false;
                }
                let Some(vk) = vk else {
                    was_down = false;
                    prev_keys = snapshot_keys();
                    continue;
                };
                // SAFETY: GetAsyncKeyState is a pure read of async key state;
                // always safe to call, any thread, any VK code.
                let down = unsafe { GetAsyncKeyState(vk) } < 0; // high bit = down
                if down != was_down && last_change.elapsed() > Duration::from_millis(30) {
                    was_down = down;
                    last_change = std::time::Instant::now();
                    let edge = if down { TalkEdge::Pressed } else { TalkEdge::Released };
                    if tx.send(edge).is_err() {
                        break; // receiver gone (shutdown)
                    }
                }
                // While held, any *other* newly-down key cancels the take.
                // Mouse buttons (VK 0x01–0x06) are excluded — clicking mid-take
                // must not nuke it.
                if was_down {
                    let cur = snapshot_keys();
                    let intruded = cur.iter().enumerate().any(|(i, &b)| {
                        i != vk as usize
                            && !(1..=6).contains(&i)
                            && b & 0x80 != 0
                            && prev_keys[i] & 0x80 == 0
                    });
                    prev_keys = cur;
                    if intruded && tx.send(TalkEdge::Cancel).is_err() {
                        break;
                    }
                } else {
                    prev_keys = snapshot_keys();
                }
            }
        })
        .expect("spawn hotkey thread");
}

/// Idempotently route the combo shortcut: registered only while the combo is
/// the configured talk key (re-registering a live hotkey fails as duplicate).
fn apply_hotkey_registration(app: &tauri::AppHandle) {
    let combo = app
        .state::<Mutex<AppState>>()
        .lock()
        .map(|s| s.settings.hotkey_key == "CtrlAltSpace")
        .unwrap_or(true);
    let shortcut: Shortcut = HOTKEY.parse().expect("hotkey parses");
    let gs = app.global_shortcut();
    let _ = gs.unregister(shortcut);
    if combo {
        if let Err(e) = gs.register(shortcut) {
            log::warn!("global shortcut registration failed ({HOTKEY}): {e}");
        }
    }
}

fn spec_to_shortcut(spec: &shortcuts::ShortcutSpec) -> Result<Shortcut, String> {
    let mut mods = Modifiers::empty();
    if spec.ctrl {
        mods |= Modifiers::CONTROL;
    }
    if spec.alt {
        mods |= Modifiers::ALT;
    }
    if spec.shift {
        mods |= Modifiers::SHIFT;
    }
    if spec.meta {
        mods |= Modifiers::SUPER;
    }
    let code = match spec.key.as_str() {
        "A" => Code::KeyA,
        "B" => Code::KeyB,
        "C" => Code::KeyC,
        "D" => Code::KeyD,
        "E" => Code::KeyE,
        "F" => Code::KeyF,
        "G" => Code::KeyG,
        "H" => Code::KeyH,
        "I" => Code::KeyI,
        "J" => Code::KeyJ,
        "K" => Code::KeyK,
        "L" => Code::KeyL,
        "M" => Code::KeyM,
        "N" => Code::KeyN,
        "O" => Code::KeyO,
        "P" => Code::KeyP,
        "Q" => Code::KeyQ,
        "R" => Code::KeyR,
        "S" => Code::KeyS,
        "T" => Code::KeyT,
        "U" => Code::KeyU,
        "V" => Code::KeyV,
        "W" => Code::KeyW,
        "X" => Code::KeyX,
        "Y" => Code::KeyY,
        "Z" => Code::KeyZ,
        "0" => Code::Digit0,
        "1" => Code::Digit1,
        "2" => Code::Digit2,
        "3" => Code::Digit3,
        "4" => Code::Digit4,
        "5" => Code::Digit5,
        "6" => Code::Digit6,
        "7" => Code::Digit7,
        "8" => Code::Digit8,
        "9" => Code::Digit9,
        "Space" => Code::Space,
        "Escape" => Code::Escape,
        "F1" => Code::F1,
        "F2" => Code::F2,
        "F3" => Code::F3,
        "F4" => Code::F4,
        "F5" => Code::F5,
        "F6" => Code::F6,
        "F7" => Code::F7,
        "F8" => Code::F8,
        "F9" => Code::F9,
        "F10" => Code::F10,
        "F11" => Code::F11,
        "F12" => Code::F12,
        other => return Err(format!("unsupported shortcut key: {other}")),
    };
    Ok(Shortcut::new(Some(mods), code))
}

fn validate_utility_key(raw: &str, other: &str) -> Result<shortcuts::ShortcutSpec, String> {
    let spec = shortcuts::parse_shortcut(raw)?;
    if !shortcuts::is_safe_utility(&spec) {
        return Err(format!("{raw} needs a modifier so it does not steal typing"));
    }
    if shortcuts::is_talk_combo(&spec) {
        return Err("that combo is reserved for the talk key".to_owned());
    }
    if raw.eq_ignore_ascii_case(other) {
        return Err("paste-last and copy-last must be different".to_owned());
    }
    Ok(spec)
}

fn apply_utility_shortcuts(app: &tauri::AppHandle) {
    let (paste_raw, copy_raw, old) = {
        let st = app.state::<Mutex<AppState>>();
        let Ok(mut s) = st.lock() else {
            return;
        };
        let old = std::mem::take(&mut s.utility_shortcuts);
        (s.settings.paste_last_key.clone(), s.settings.copy_last_key.clone(), old)
    };
    let gs = app.global_shortcut();
    for sc in old {
        let _ = gs.unregister(sc);
    }
    let mut next = Vec::new();
    for raw in [paste_raw, copy_raw] {
        match shortcuts::parse_shortcut(&raw).and_then(|s| spec_to_shortcut(&s)) {
            Ok(sc) => {
                if let Err(e) = gs.register(sc) {
                    log::warn!("utility shortcut registration failed ({raw}): {e}");
                } else {
                    next.push(sc);
                }
            }
            Err(e) => log::warn!("utility shortcut skipped ({raw}): {e}"),
        }
    }
    if let Ok(mut s) = app.state::<Mutex<AppState>>().lock() {
        s.utility_shortcuts = next;
    }
}

// ---------------------------------------------------------------------------
// Settings + history
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Settings {
    model_id: String,
    /// BCP-47 code or "auto". Passed to whisper-cli `-l`; Parakeet v3 is
    /// multilingual without a language flag.
    language: String,
    auto_paste: bool,
    /// Cleanup level (Wispr Flow "Auto Cleanup" lite): "off" | "light"
    /// (filler words only) | "full" (fillers + punctuation tidy).
    #[serde(default = "default_cleanup")]
    cleanup: String,
    /// Whisper-only: translate to English (needs a non-"auto" language).
    #[serde(default)]
    translate: bool,
    /// Talk key: "RightCtrl" (default) | "LeftCtrl" | "LeftAlt" | "RightAlt" | "ScrollLock" | "F9" | "CtrlAltSpace".
    hotkey_key: String,
    /// "hold" (default, macOS-like: down starts, up stops) | "toggle".
    recording_mode: String,
    /// Preferred WASAPI input name. `None` = OS default.
    #[serde(default)]
    audio_device: Option<String>,
    #[serde(default = "default_true")]
    overlay_enabled: bool,
    #[serde(default = "default_overlay_edge")]
    overlay_edge: String,
    #[serde(default = "default_overlay_offset")]
    overlay_offset: i32,
    #[serde(default = "shortcuts::default_paste_last")]
    paste_last_key: String,
    #[serde(default = "shortcuts::default_copy_last")]
    copy_last_key: String,
    /// Missing field on old settings.json → already onboarded. Fresh Default → false.
    #[serde(default = "default_true")]
    onboarded: bool,
    /// Hard-stop the take at 20 minutes. Default off (warn only at 19).
    #[serde(default)]
    session_cap: bool,
    #[serde(default = "default_audio_backend")]
    audio_backend: String,
    #[serde(default)]
    audio_api_base: String,
    #[serde(default = "default_audio_model")]
    audio_api_model: String,
    #[serde(default = "default_llm_backend")]
    llm_backend: String,
    #[serde(default)]
    llm_api_base: String,
    #[serde(default = "default_llm_model")]
    llm_api_model: String,
    #[serde(default = "default_llm_temp")]
    llm_temperature: f32,
    #[serde(default = "default_llm_timeout")]
    llm_timeout_ms: u64,
    #[serde(default = "default_llm_preset")]
    llm_preset: String,
    #[serde(default)]
    llm_custom_prompt: String,
    #[serde(default)]
    llm_enabled: bool,
}

fn default_cleanup() -> String {
    "full".to_owned()
}

fn default_true() -> bool {
    true
}

fn default_overlay_edge() -> String {
    "bottom".to_owned()
}

fn default_overlay_offset() -> i32 {
    overlay::OFFSET_CENTER
}

fn default_audio_backend() -> String {
    "local".to_owned()
}
fn default_audio_model() -> String {
    "whisper-1".to_owned()
}
fn default_llm_backend() -> String {
    "off".to_owned()
}
fn default_llm_model() -> String {
    "gpt-4o-mini".to_owned()
}
fn default_llm_temp() -> f32 {
    0.2
}
fn default_llm_timeout() -> u64 {
    8000
}
fn default_llm_preset() -> String {
    "clean".to_owned()
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            model_id: models::default_model_id(),
            language: "auto".to_owned(),
            auto_paste: true,
            cleanup: default_cleanup(),
            translate: false,
            hotkey_key: "RightCtrl".to_owned(),
            recording_mode: "hold".to_owned(),
            audio_device: None,
            overlay_enabled: true,
            overlay_edge: default_overlay_edge(),
            overlay_offset: default_overlay_offset(),
            paste_last_key: shortcuts::default_paste_last(),
            copy_last_key: shortcuts::default_copy_last(),
            onboarded: false,
            session_cap: false,
            audio_backend: default_audio_backend(),
            audio_api_base: String::new(),
            audio_api_model: default_audio_model(),
            llm_backend: default_llm_backend(),
            llm_api_base: String::new(),
            llm_api_model: default_llm_model(),
            llm_temperature: default_llm_temp(),
            llm_timeout_ms: default_llm_timeout(),
            llm_preset: default_llm_preset(),
            llm_custom_prompt: String::new(),
            llm_enabled: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HistoryItem {
    text: String,
    date_unix: u64,
    duration_secs: f64,
    model: String,
    /// Per-dictation recording for playback. `#[serde(default)]` keeps
    /// pre-audio history files loadable.
    #[serde(default)]
    audio_path: Option<String>,
    /// Transcript before dictionary + cleanup. Empty on pre-P0 rows.
    #[serde(default)]
    raw_text: String,
    /// Cached polished word count. 0 means "compute from text" for old rows.
    #[serde(default)]
    words_out: u32,
    #[serde(default)]
    dict_hits: u32,
    #[serde(default)]
    cleanup: String,
}

#[derive(Debug, Clone, Serialize)]
struct Status {
    recording: bool,
    model_id: String,
    engine: EngineKind,
    engine_label: String,
    model_loaded: bool,
    whisper_binary: Option<String>,
    data_dir: String,
    /// Resolved capture device (preferred if still present, else OS default).
    audio_device: Option<String>,
    /// App version (Cargo package version, single source of truth).
    version: String,
    recommended_model: String,
    transcribing: bool,
    /// "local" or "online" — which STT source dictation actually uses.
    speech_source: String,
    /// Catalog id, or the API model name when the audio backend is online.
    speech_model: String,
    /// Human-readable name for the active speech source.
    speech_model_name: String,
    /// Ready to dictate with the selected source (downloaded, or API model set).
    speech_ready: bool,
}

/// OS default capture endpoint name. Best-effort: never fails status.
fn default_input_name() -> Option<String> {
    cpal::default_host()
        .default_input_device()?
        .name()
        .ok()
}

#[derive(Debug, Clone, Serialize)]
struct ModelStatus {
    #[serde(flatten)]
    entry: ModelEntry,
    downloaded: bool,
    active: bool,
    downloading: bool,
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

/// Handles for the live capture thread (owns the `!Send` cpal stream).
struct ActiveRecording {
    samples: Arc<Mutex<Vec<f32>>>,
    sample_rate: u32,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    started_at: std::time::Instant,
    stream_error: Arc<AtomicBool>,
}

struct AppState {
    recording: bool,
    transcribing: bool,
    /// True while the current recording was started by the talk key (a key
    /// release must not stop a UI/mic-button-started recording in hold mode).
    hotkey_owned: bool,
    active: Option<ActiveRecording>,
    settings: Settings,
    history: Vec<HistoryItem>,
    /// Lifetime usage; never cleared with history.
    stats: UsageStats,
    downloading: HashMap<String, Arc<AtomicBool>>,
    /// Currently registered paste-last / copy-last shortcuts (so we can swap).
    utility_shortcuts: Vec<Shortcut>,
    data_dir: PathBuf,
    transcriber: Transcriber,
    /// Last non-DictFlow foreground HWND. Overlay clicks must not steal the
    /// caret; we restore this window before Ctrl+V.
    paste_target: Option<focus::Hwnd>,
}

impl AppState {
    fn settings_path(&self) -> PathBuf {
        self.data_dir.join("settings.json")
    }
    fn history_path(&self) -> PathBuf {
        self.data_dir.join("history.json")
    }

    fn load_settings(&mut self) {
        if let Ok(bytes) = std::fs::read(self.settings_path()) {
            if let Ok(s) = serde_json::from_slice::<Settings>(&bytes) {
                if models::find(&s.model_id).is_some() {
                    self.settings = s;
                    // P1 first cut defaulted to top-left. Move that unset pose
                    // to center-bottom so existing installs match the new default.
                    if self.settings.overlay_edge == "top" && self.settings.overlay_offset == 0 {
                        self.settings.overlay_edge = default_overlay_edge();
                        self.settings.overlay_offset = default_overlay_offset();
                    }
                    return;
                }
            }
        }
        self.settings = Settings::default();
    }

    fn save_settings(&self) {
        let _ = std::fs::create_dir_all(&self.data_dir);
        if let Ok(bytes) = serde_json::to_vec_pretty(&self.settings) {
            let _ = std::fs::write(self.settings_path(), bytes);
        }
    }

    fn load_history(&mut self) {
        let Ok(bytes) = std::fs::read(self.history_path()) else {
            return;
        };
        // Current format.
        if let Ok(items) = serde_json::from_slice::<Vec<HistoryItem>>(&bytes) {
            self.history = items;
            return;
        }
        // Migrate v0.1 plain-text format.
        if let Ok(texts) = serde_json::from_slice::<Vec<String>>(&bytes) {
            self.history = texts
                .into_iter()
                .map(|text| HistoryItem {
                    text,
                    date_unix: 0,
                    duration_secs: 0.0,
                    model: String::new(),
                    audio_path: None,
                    raw_text: String::new(),
                    words_out: 0,
                    dict_hits: 0,
                    cleanup: String::new(),
                })
                .collect();
            self.save_history();
        }
    }

    fn save_history(&self) {
        let _ = std::fs::create_dir_all(&self.data_dir);
        if let Ok(bytes) = serde_json::to_vec_pretty(&self.history) {
            let _ = std::fs::write(self.history_path(), bytes);
        }
    }

    fn push_history(&mut self, item: NewHistory) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let words_out = text::word_count(&item.text);
        let raw_words = if item.raw_text.is_empty() {
            words_out
        } else {
            text::word_count(&item.raw_text)
        };
        self.stats.record(
            now,
            words_out as u64,
            item.duration_secs,
            raw_words as u64,
            item.dict_hits as u64,
        );
        self.stats.save(&self.data_dir);
        self.history.insert(
            0,
            HistoryItem {
                text: item.text,
                date_unix: now,
                duration_secs: item.duration_secs,
                model: item.model,
                audio_path: item.audio_path,
                raw_text: item.raw_text,
                words_out,
                dict_hits: item.dict_hits,
                cleanup: item.cleanup,
            },
        );
        if self.history.len() > HISTORY_LIMIT {
            self.history.truncate(HISTORY_LIMIT);
        }
        self.save_history();
    }

    /// First launch after upgrade: fill the ledger from whatever history is
    /// still on disk. Later clear-all leaves stats.json alone.
    fn seed_stats_from_history(&mut self) {
        if !self.stats.is_empty() || self.history.is_empty() {
            return;
        }
        for h in &self.history {
            let words = if h.words_out > 0 {
                h.words_out as u64
            } else {
                text::word_count(&h.text) as u64
            };
            let raw = if h.raw_text.is_empty() {
                words
            } else {
                text::word_count(&h.raw_text) as u64
            };
            self.stats
                .record(h.date_unix, words, h.duration_secs, raw, h.dict_hits as u64);
        }
        self.stats.save(&self.data_dir);
    }
}

struct NewHistory {
    text: String,
    raw_text: String,
    duration_secs: f64,
    model: String,
    audio_path: Option<String>,
    dict_hits: u32,
    cleanup: String,
}

fn resolve_binary(data_dir: &Path) -> Option<PathBuf> {
    let local = data_dir.join("bin").join("whisper-cli.exe");
    if local.exists() {
        return Some(local);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let p = dir.join("whisper-cli.exe");
            p.exists().then_some(p)
        })
    })
}

// ---------------------------------------------------------------------------
// Parakeet engine: dedicated thread owning the sherpa-onnx recognizer
// ---------------------------------------------------------------------------

enum EngineJob {
    Preload {
        model_id: String,
    },
    Transcribe {
        model_id: String,
        samples_16k: Vec<f32>,
        reply: mpsc::Sender<Result<String, String>>,
    },
}

#[derive(Clone)]
struct Transcriber {
    tx: mpsc::Sender<EngineJob>,
}

impl Transcriber {
    fn spawn(data_dir: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("dictflow-transcriber".to_owned())
            .spawn(move || transcriber_loop(rx, data_dir))
            .expect("spawn transcriber thread");
        Transcriber { tx }
    }

    fn preload(&self, model_id: &str) {
        let _ = self.tx.send(EngineJob::Preload {
            model_id: model_id.to_owned(),
        });
    }

    /// Blocking decode on the transcriber thread. Call from `spawn_blocking`.
    fn transcribe(&self, model_id: &str, samples_16k: Vec<f32>) -> Result<String, String> {
        let (rep_tx, rep_rx) = mpsc::channel();
        self.tx
            .send(EngineJob::Transcribe {
                model_id: model_id.to_owned(),
                samples_16k,
                reply: rep_tx,
            })
            .map_err(|e| format!("transcriber gone: {e}"))?;
        rep_rx
            .recv()
            .map_err(|e| format!("transcriber dropped reply: {e}"))?
    }
}

fn num_threads() -> i32 {
    std::thread::available_parallelism()
        .map(|n| n.get().min(4) as i32)
        .unwrap_or(2)
}

fn ensure_parakeet<'a>(
    loaded: &'a mut Option<(String, sherpa_onnx::OfflineRecognizer)>,
    data_dir: &Path,
    model_id: &str,
) -> Result<&'a sherpa_onnx::OfflineRecognizer, String> {
    let needs_load = !matches!(loaded, Some((id, _)) if id == model_id);
    if needs_load {
        let entry = models::find(model_id)
            .ok_or_else(|| format!("unknown model: {model_id}"))?;
        if entry.engine != EngineKind::Parakeet {
            return Err(format!("{model_id} is not a Parakeet model"));
        }
        if !entry.is_downloaded(data_dir) {
            return Err(format!(
                "{} is not downloaded yet — open Models and download it first",
                entry.name
            ));
        }
        let dir = entry.dir(data_dir);
        let file = |name: &str| dir.join(name).display().to_string();
        let mut cfg = sherpa_onnx::OfflineRecognizerConfig::default();
        cfg.model_config.transducer = sherpa_onnx::OfflineTransducerModelConfig {
            encoder: Some(file("encoder.int8.onnx")),
            decoder: Some(file("decoder.int8.onnx")),
            joiner: Some(file("joiner.int8.onnx")),
        };
        cfg.model_config.tokens = Some(file("tokens.txt"));
        cfg.model_config.provider = Some("cpu".to_owned());
        cfg.model_config.num_threads = num_threads();
        let recognizer = sherpa_onnx::OfflineRecognizer::create(&cfg)
            .ok_or_else(|| "load Parakeet model failed (create returned null)".to_owned())?;
        *loaded = Some((model_id.to_owned(), recognizer));
    }
    Ok(&loaded.as_ref().expect("recognizer just loaded").1)
}

fn decode_parakeet(
    recognizer: &sherpa_onnx::OfflineRecognizer,
    samples_16k: &[f32],
) -> Result<String, String> {
    if samples_16k.is_empty() {
        return Err("no audio captured".to_owned());
    }
    let stream = recognizer.create_stream();
    stream.accept_waveform(16_000, samples_16k);
    recognizer.decode(&stream);
    stream
        .get_result()
        .map(|r| r.text.trim().to_owned())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| "transcription was empty".to_owned())
}

fn transcriber_loop(rx: mpsc::Receiver<EngineJob>, data_dir: PathBuf) {
    let mut loaded: Option<(String, sherpa_onnx::OfflineRecognizer)> = None;
    for job in rx {
        match job {
            EngineJob::Preload { model_id } => {
                match ensure_parakeet(&mut loaded, &data_dir, &model_id) {
                    Ok(_) => log::info!("parakeet warmed up: {model_id}"),
                    Err(e) => log::warn!("parakeet preload failed ({model_id}): {e}"),
                }
            }
            EngineJob::Transcribe {
                model_id,
                samples_16k,
                reply,
            } => {
                let out = ensure_parakeet(&mut loaded, &data_dir, &model_id)
                    .and_then(|rec| decode_parakeet(rec, &samples_16k));
                let _ = reply.send(out);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Audio capture (WASAPI via cpal, thread owns the !Send stream)
// ---------------------------------------------------------------------------

fn list_input_names() -> (Vec<String>, Option<String>) {
    let host = cpal::default_host();
    let default_name = host.default_input_device().and_then(|d| d.name().ok());
    let names = host
        .input_devices()
        .map(|devs| {
            devs.filter_map(|d| d.name().ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    (names, default_name)
}

fn resolve_capture_device(preferred: Option<&str>) -> anyhow::Result<devices::DeviceChoice> {
    let (names, default_name) = list_input_names();
    devices::choose_device(&names, default_name.as_deref(), preferred).map_err(anyhow::Error::msg)
}

fn open_input_named(name: &str) -> anyhow::Result<cpal::Device> {
    let host = cpal::default_host();
    if let Ok(devs) = host.input_devices() {
        for d in devs {
            if d.name().ok().as_deref() == Some(name) {
                return Ok(d);
            }
        }
    }
    host.default_input_device().context(
        "no input device found — connect a microphone and check Windows \
         Settings → Privacy & security → Microphone (allow desktop apps)",
    )
}

fn run_capture(
    ready_tx: mpsc::Sender<Result<u32, String>>,
    samples: Arc<Mutex<Vec<f32>>>,
    stop: Arc<AtomicBool>,
    device_name: String,
    stream_error: Arc<AtomicBool>,
) {
    // NOTE: the stream MUST be returned out of this closure — if it is dropped
    // here, capture stops instantly and every recording comes back silent.
    let result = (|| -> anyhow::Result<(cpal::Stream, u32)> {
        let device = open_input_named(&device_name)?;
        let supported = device
            .default_input_config()
            .context("no default input config")?;

        let writer = samples.clone();
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => {
                let config: cpal::StreamConfig = supported.clone().into();
                let flag = stream_error.clone();
                device.build_input_stream(
                    &config,
                    move |data: &[f32], _| {
                        writer.lock().unwrap().extend_from_slice(data);
                    },
                    {
                        let flag = flag.clone();
                        move |err| {
                            log::warn!("audio stream error: {err}");
                            flag.store(true, Ordering::SeqCst);
                        }
                    },
                    None,
                )?
            }
            cpal::SampleFormat::I16 => {
                let config: cpal::StreamConfig = supported.clone().into();
                let flag = stream_error.clone();
                device.build_input_stream(
                    &config,
                    move |data: &[i16], _| {
                        let mut lock = writer.lock().unwrap();
                        lock.extend(data.iter().map(|s| *s as f32 / i16::MAX as f32));
                    },
                    move |err| {
                        log::warn!("audio stream error: {err}");
                        flag.store(true, Ordering::SeqCst);
                    },
                    None,
                )?
            }
            cpal::SampleFormat::U16 => {
                let config: cpal::StreamConfig = supported.clone().into();
                let flag = stream_error.clone();
                device.build_input_stream(
                    &config,
                    move |data: &[u16], _| {
                        let mut lock = writer.lock().unwrap();
                        lock.extend(
                            data.iter()
                                .map(|s| (*s as f32 / u16::MAX as f32) * 2.0 - 1.0),
                        );
                    },
                    move |err| {
                        log::warn!("audio stream error: {err}");
                        flag.store(true, Ordering::SeqCst);
                    },
                    None,
                )?
            }
            other => anyhow::bail!("unsupported sample format: {other:?}"),
        };
        stream.play()?;
        Ok((stream, supported.sample_rate().0))
    })();

    match result {
        Ok((_stream, rate)) => {
            let _ = ready_tx.send(Ok(rate));
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(50));
            }
            // `_stream` is dropped here, releasing the microphone.
        }
        Err(e) => {
            let _ = ready_tx.send(Err(format!("{e:#}")));
        }
    }
}

fn start_recording(state: &mut AppState) -> anyhow::Result<devices::DeviceChoice> {
    if state.recording {
        anyhow::bail!("already recording");
    }
    state.active = None; // defensive: drop any stale session

    let preferred = state.settings.audio_device.clone();
    let choice = resolve_capture_device(preferred.as_deref())?;

    let samples: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let stream_error = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = mpsc::channel();

    let t_samples = samples.clone();
    let t_stop = stop.clone();
    let t_done = done.clone();
    let t_err = stream_error.clone();
    let t_name = choice.name.clone();
    std::thread::Builder::new()
        .name("dictflow-capture".to_owned())
        .spawn(move || {
            run_capture(ready_tx, t_samples, t_stop, t_name, t_err);
            t_done.store(true, Ordering::SeqCst);
        })
        .context("spawn capture thread")?;

    // Block until the stream is actually playing so the first syllable isn't
    // lost — and so a missing microphone surfaces as an error, not silence.
    let sample_rate = ready_rx
        .recv_timeout(Duration::from_secs(5))
        .context("capture thread did not respond")?
        .map_err(anyhow::Error::msg)?;

    state.recording = true;
    state.active = Some(ActiveRecording {
        samples,
        sample_rate,
        stop,
        done,
        started_at: std::time::Instant::now(),
        stream_error,
    });
    Ok(choice)
}

#[derive(Debug, Clone, Serialize)]
struct LevelEvent {
    peak: f32,
    rms: f32,
}

#[derive(Debug, Clone, Serialize)]
struct DeviceFallback {
    from: String,
    to: String,
}

#[derive(Debug, Clone, Serialize)]
struct CommittedEvent {
    text: String,
    chip_ms: u64,
}

fn committed(text: String) -> CommittedEvent {
    CommittedEvent {
        text,
        chip_ms: overlay::COPY_CHIP_MS,
    }
}

fn set_transcribing(app: &tauri::AppHandle, on: bool) {
    if let Ok(mut s) = app.state::<Mutex<AppState>>().lock() {
        s.transcribing = on;
    }
    let _ = app.emit("dictflow://transcribing", on);
}

fn emit_device_fallback(app: &tauri::AppHandle, preferred: Option<&str>, choice: &devices::DeviceChoice) {
    if choice.fell_back {
        let _ = app.emit(
            "dictflow://device-fallback",
            DeviceFallback {
                from: preferred.unwrap_or("").to_owned(),
                to: choice.name.clone(),
            },
        );
    }
}

/// Live peak/RMS + 19-minute warn / optional 20-minute cap. Not used for the
/// 1.5 s Setup mic test.
fn spawn_record_ticker(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut warned = false;
        loop {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let snap = {
                let st = app.state::<Mutex<AppState>>();
                let Ok(s) = st.lock() else {
                    break;
                };
                if !s.recording {
                    break;
                }
                let Some(rec) = s.active.as_ref() else {
                    break;
                };
                let samples = rec.samples.lock().unwrap();
                let start = samples.len().saturating_sub(1024);
                let level = devices::level_from_samples(&samples[start..]);
                drop(samples);
                let elapsed = rec.started_at.elapsed().as_secs_f64();
                let err = rec.stream_error.load(Ordering::SeqCst);
                let cap = s.settings.session_cap;
                (level, elapsed, err, cap)
            };
            let (level, elapsed, stream_err, cap) = snap;
            let _ = app.emit("dictflow://level", LevelEvent {
                peak: level.peak,
                rms: level.rms,
            });
            if stream_err {
                let st: State<'_, Mutex<AppState>> = app.state();
                if let Ok(mut guard) = st.lock() {
                    if guard.recording {
                        let _ = cancel_recording(&mut guard);
                    }
                }
                let _ = app.emit("dictflow://recording", false);
                let _ = app.emit(
                    "dictflow://device-fallback",
                    DeviceFallback {
                        from: String::new(),
                        to: "stream error — take discarded".to_owned(),
                    },
                );
                break;
            }
            match session::session_tick(elapsed, warned, cap) {
                session::SessionTick::Warn => {
                    warned = true;
                    let _ = app.emit("dictflow://session-warn", ());
                }
                session::SessionTick::Cap => {
                    let st: State<'_, Mutex<AppState>> = app.state();
                    match stop_transcribe(&app, st).await {
                        Ok(msg) => log::info!("session cap: {msg}"),
                        Err(e) => log::error!("session cap stop failed: {e}"),
                    }
                    break;
                }
                session::SessionTick::None => {}
            }
        }
    });
}

fn stop_and_save_wav(state: &mut AppState) -> anyhow::Result<(PathBuf, f64)> {
    let rec = state.active.take().context("not recording")?;
    state.recording = false;
    let duration_secs = rec.started_at.elapsed().as_secs_f64();
    rec.stop.store(true, Ordering::SeqCst);

    // Wait (bounded) for the capture thread to drop the stream and release
    // the microphone before reading the buffer.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !rec.done.load(Ordering::SeqCst) {
        if std::time::Instant::now() > deadline {
            anyhow::bail!("capture thread did not stop in time");
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let samples = rec.samples.lock().unwrap().clone();
    if samples.is_empty() {
        anyhow::bail!(
            "captured no audio from the default microphone — run the Setup \
             microphone test, and check Windows Settings → Privacy & security \
             → Microphone (allow desktop apps)"
        );
    }
    // Each recording gets its own file so History items can play back their
    // audio (SpeakType keeps audioFileURL per item; files die with the item).
    let dir = state.data_dir.join("recordings");
    std::fs::create_dir_all(&dir).context("create recordings dir")?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let wav_path = dir.join(format!("rec-{stamp}.wav"));
    text::write_wav_mono_16k(&wav_path, &samples, rec.sample_rate)?;
    Ok((wav_path, duration_secs))
}

// ---------------------------------------------------------------------------
// Microphone diagnostics + Windows settings deep-link
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct AudioDeviceInfo {
    name: String,
    sample_rate: u32,
    channels: u16,
    is_default: bool,
    is_selected: bool,
}

/// List all input devices. Unlike macOS (per-app mic prompt), Windows guards
/// the mic with a global privacy toggle — if this comes back empty, the
/// toggle (or a missing mic) is the cause.
#[tauri::command]
fn get_audio_devices(state: State<'_, Mutex<AppState>>) -> Result<Vec<AudioDeviceInfo>, String> {
    let preferred = state
        .lock()
        .ok()
        .and_then(|s| s.settings.audio_device.clone());
    let host = cpal::default_host();
    let default_name = host
        .default_input_device()
        .and_then(|d| d.name().ok());
    let mut out = Vec::new();
    let devices = host.input_devices().map_err(|e| e.to_string())?;
    for dev in devices {
        let name = dev.name().unwrap_or_else(|_| "(unnamed device)".to_owned());
        let (sample_rate, channels) = dev
            .default_input_config()
            .map(|c| (c.sample_rate().0, c.channels()))
            .unwrap_or((0, 0));
        let is_default = Some(&name) == default_name.as_ref();
        let is_selected = match preferred.as_deref() {
            Some(p) if !p.is_empty() => p == name,
            _ => is_default,
        };
        out.push(AudioDeviceInfo {
            is_default,
            is_selected,
            name,
            sample_rate,
            channels,
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, Serialize)]
struct MicTest {
    duration_secs: f64,
    peak: f32,
    samples: usize,
    device: String,
    sample_rate: u32,
    channels: u16,
    /// Whether any audio callback fired at all. `false` with a present device
    /// points at exclusive-mode holds or a dead endpoint, not permission.
    callbacks: bool,
}

/// Record ~1.5 s and report the peak level without transcribing or saving
/// history — the fastest way to tell a dead mic from a broken pipeline.
#[tauri::command]
async fn test_microphone(state: State<'_, Mutex<AppState>>) -> Result<MicTest, String> {
    {
        let mut s = state.lock().unwrap();
        if s.recording {
            return Err("already recording — stop first".to_owned());
        }
        start_recording(&mut s).map_err(|e| e.to_string())?;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let (wav, duration_secs, callbacks) = {
        let mut s = state.lock().unwrap();
        let buffered = s
            .active
            .as_ref()
            .map(|a| a.samples.lock().unwrap().len())
            .unwrap_or(0);
        let (wav, duration_secs) = stop_and_save_wav(&mut s).map_err(|e| e.to_string())?;
        (wav, duration_secs, buffered > 0)
    };
    let preferred = state
        .lock()
        .ok()
        .and_then(|s| s.settings.audio_device.clone());
    let choice = resolve_capture_device(preferred.as_deref()).ok();
    let host = cpal::default_host();
    let (device, sample_rate, channels) = choice
        .as_ref()
        .and_then(|c| {
            let d = open_input_named(&c.name).ok()?;
            let cfg = d.default_input_config().ok()?;
            Some((c.name.clone(), cfg.sample_rate().0, cfg.channels()))
        })
        .or_else(|| {
            host.default_input_device().and_then(|d| {
                let name = d.name().ok()?;
                let cfg = d.default_input_config().ok()?;
                Some((name, cfg.sample_rate().0, cfg.channels()))
            })
        })
        .unwrap_or(("(none)".to_owned(), 0, 0));
    let samples = text::load_wav_mono_16k(&wav).map_err(|e| format!("{e:#}"))?;
    let peak = samples.iter().fold(0.0f32, |a, s| a.max(s.abs()));
    Ok(MicTest {
        duration_secs,
        peak,
        samples: samples.len(),
        device,
        sample_rate,
        channels,
        callbacks,
    })
}

/// Open Windows Settings at the microphone privacy page (there is no macOS-
/// style per-app prompt on Windows — this toggle IS the permission).
#[tauri::command]
fn open_mic_settings() -> Result<(), String> {
    std::process::Command::new("cmd")
        .args(["/C", "start", "", "ms-settings:privacy-microphone"])
        .status()
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Transcription routing + post-processing
// ---------------------------------------------------------------------------

struct TranscribeInput {
    wav_path: PathBuf,
    model_id: String,
    language: String,
    cleanup: String,
    translate: bool,
    data_dir: PathBuf,
    dictionary: Vec<text::DictionaryEntry>,
    polish: PolishCfg,
}

#[derive(Clone)]
struct PolishCfg {
    audio_backend: String,
    audio_api_base: String,
    audio_api_model: String,
    audio_api_key: String,
    llm_backend: String,
    llm_api_base: String,
    llm_api_model: String,
    llm_api_key: String,
    llm_temperature: f32,
    llm_timeout_ms: u64,
    llm_preset: String,
    llm_custom_prompt: String,
    llm_enabled: bool,
}

fn polish_cfg(settings: &Settings, data_dir: &Path) -> PolishCfg {
    let secrets = polish::load_secrets(data_dir);
    let audio_base = if settings.audio_api_base.trim().is_empty() {
        polish::default_base("openai_compat").to_owned()
    } else {
        settings.audio_api_base.clone()
    };
    let llm_base = if settings.llm_api_base.trim().is_empty() {
        polish::default_base(&settings.llm_backend).to_owned()
    } else {
        settings.llm_api_base.clone()
    };
    PolishCfg {
        audio_backend: settings.audio_backend.clone(),
        audio_api_base: audio_base,
        audio_api_model: settings.audio_api_model.clone(),
        audio_api_key: secrets.audio_api_key,
        llm_backend: settings.llm_backend.clone(),
        llm_api_base: llm_base,
        llm_api_model: settings.llm_api_model.clone(),
        llm_api_key: secrets.llm_api_key,
        llm_temperature: settings.llm_temperature,
        llm_timeout_ms: settings.llm_timeout_ms,
        llm_preset: settings.llm_preset.clone(),
        llm_custom_prompt: settings.llm_custom_prompt.clone(),
        llm_enabled: settings.llm_enabled,
    }
}

fn transcribe_whisper(
    data_dir: &Path,
    model_id: &str,
    language: &str,
    translate: bool,
    wav_16k_path: &Path,
) -> anyhow::Result<String> {
    let entry = models::find(model_id).context("unknown model")?;
    let model_path = entry
        .single_path(data_dir)
        .context("not a single-file model")?;
    if !model_path.exists() {
        anyhow::bail!(
            "model not downloaded yet: {} (open Models and download it first)",
            model_path.display()
        );
    }
    let Some(binary) = resolve_binary(data_dir) else {
        anyhow::bail!(
            "whisper-cli.exe not found — drop a whisper.cpp build at {} or put it on PATH",
            data_dir.join("bin").display()
        );
    };
    // whisper.cpp CLI: whisper-cli -m <model> -f <wav> -otxt -of <out-prefix>
    let out_prefix = data_dir.join("last_transcription");
    let mut cmd = std::process::Command::new(&binary);
    cmd.arg("-m")
        .arg(&model_path)
        .arg("-f")
        .arg(wav_16k_path)
        .arg("-otxt")
        .arg("-of")
        .arg(&out_prefix)
        .arg("--no-prints");
    if language != "auto" && !language.is_empty() {
        cmd.arg("-l").arg(language);
    }
    // whisper.cpp `--translate` renders English from a foreign-language clip.
    // It needs an explicit source language, so it only applies off-"auto".
    if translate && language != "auto" && !language.is_empty() {
        cmd.arg("--translate");
    }
    let status = cmd
        .status()
        .with_context(|| format!("run {}", binary.display()))?;
    if !status.success() {
        anyhow::bail!("whisper-cli exited with {status}");
    }
    let txt_path = out_prefix.with_extension("txt");
    let text = std::fs::read_to_string(&txt_path)
        .with_context(|| format!("read {}", txt_path.display()))?;
    Ok(text)
}

struct TranscriptResult {
    text: String,
    raw_text: String,
    duration_secs: f64,
    dict_hits: u32,
}

/// Heavy work: runs inside `spawn_blocking` so the async runtime stays free.
fn run_transcription(
    input: &TranscribeInput,
    parakeet: &Transcriber,
) -> anyhow::Result<TranscriptResult> {
    let samples = text::load_wav_mono_16k(&input.wav_path)?;
    let duration_secs = samples.len() as f64 / 16_000.0;
    if let Some(why) = devices::blank_audio(&samples, 16_000) {
        anyhow::bail!("{}", why.message());
    }
    let raw = if input.polish.audio_backend == "openai_compat" {
        let wav = std::fs::read(&input.wav_path).context("read wav for audio API")?;
        polish::transcribe_openai(
            &input.polish.audio_api_base,
            &input.polish.audio_api_key,
            &input.polish.audio_api_model,
            input.polish.llm_timeout_ms.max(15_000),
            &wav,
        )?
    } else {
    let entry = models::find(&input.model_id).context("unknown model")?;
    match entry.engine {
        EngineKind::Whisper => {
            let tmp = input.data_dir.join("last_16k.wav");
            text::write_wav_mono_16k(&tmp, &samples, 16_000)?;
            transcribe_whisper(
                &input.data_dir,
                &input.model_id,
                &input.language,
                input.translate,
                &tmp,
            )?
        }
        EngineKind::Parakeet => parakeet
            .transcribe(&input.model_id, samples)
            .map_err(anyhow::Error::msg)?,
    }
    };
    let raw_text = raw.trim().to_owned();

    // P2: spoken commands → backtrack → lists, then dictionary + cleanup.
    let formatted = format::apply_offline_format(&raw_text);
    let mut dictionary = input.dictionary.clone();
    let (mut out, dict_hits) = text::apply_dictionary(&formatted, &mut dictionary);
    if dict_hits > 0 {
        text::sort_dictionary(&mut dictionary);
        let _ = text::save_dictionary(&input.data_dir, &dictionary);
    }
    match input.cleanup.as_str() {
        "light" | "medium" => out = text::remove_fillers(&out),
        "full" | "high" => out = text::tidy_punctuation(&text::remove_fillers(&out)),
        _ => {}
    }
    out = text::smart_trailing_punctuation(&out);
    let want_llm = input.polish.llm_enabled
        || input.cleanup == "medium"
        || input.cleanup == "high";
    if want_llm && input.polish.llm_backend != "off" {
        let preset = match input.cleanup.as_str() {
            "high" => "professional",
            "medium" => "clean",
            _ => input.polish.llm_preset.as_str(),
        };
        if let Some(polished) = polish::polish(&polish::PolishArgs {
            backend: &input.polish.llm_backend,
            base_url: &input.polish.llm_api_base,
            api_key: &input.polish.llm_api_key,
            model: &input.polish.llm_api_model,
            preset,
            custom_prompt: &input.polish.llm_custom_prompt,
            temperature: input.polish.llm_temperature,
            timeout_ms: input.polish.llm_timeout_ms,
            text: &out,
        }) {
            out = polished;
        }
    }
    let out = out.trim().to_owned();
    if out.is_empty() {
        anyhow::bail!("transcription was empty");
    }
    Ok(TranscriptResult {
        text: out,
        raw_text,
        duration_secs,
        dict_hits,
    })
}

// ---------------------------------------------------------------------------
// Paste anywhere (clipboard + Ctrl+V)
// ---------------------------------------------------------------------------

fn remember_paste_target(app: &tauri::AppHandle) {
    let ours = focus::our_hwnds(app);
    let Some(fg) = focus::foreground_hwnd() else {
        return;
    };
    if !focus::should_remember(fg, &ours) {
        return;
    }
    if let Ok(mut s) = app.state::<Mutex<AppState>>().lock() {
        s.paste_target = Some(fg);
    }
}

fn paste_target_for(app: &tauri::AppHandle, last_saved: Option<focus::Hwnd>) -> Option<focus::Hwnd> {
    focus::pick_paste_target(focus::foreground_hwnd(), last_saved, &focus::our_hwnds(app))
}

fn paste_text(text: &str, target: Option<focus::Hwnd>) -> anyhow::Result<()> {
    // SpeakType re-activates the previous app before Cmd+V so the caret
    // never follows the overlay click.
    if focus::restore_hwnd(target) {
        std::thread::sleep(Duration::from_millis(180));
    }

    let mut cb = arboard::Clipboard::new().context("open clipboard")?;
    // Snapshot the current *text* clipboard so it can be restored after
    // auto-paste (SpeakType `restoreClipboardAfterAutoPaste`, default on).
    // Limitation: non-text clipboard content (images, files) cannot be
    // snapshotted through arboard and is not preserved.
    let previous = cb.get_text().ok();
    cb.set_text(text.to_owned()).context("set clipboard")?;
    std::thread::sleep(Duration::from_millis(120));

    // Target window is in front again; Ctrl+V lands at its previous caret.
    use enigo::{Direction, Enigo, Key, Keyboard, Settings};
    let mut enigo = Enigo::new(&Settings::default()).context("init key injector")?;
    enigo
        .key(Key::Control, Direction::Press)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    enigo
        .key(Key::Unicode('v'), Direction::Click)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    enigo
        .key(Key::Control, Direction::Release)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    // Give the target app a beat to consume the paste, then restore the
    // previous clipboard — but only if it still holds our text (i.e. the
    // user hasn't copied something else meanwhile).
    std::thread::sleep(Duration::from_millis(350));
    if let Some(prev) = previous {
        if cb.get_text().map(|t| t == text).unwrap_or(false) {
            let _ = cb.set_text(prev);
        }
    }
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_owned();
    }
    format!("{}…", &s[..n])
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

fn is_online_audio(settings: &Settings) -> bool {
    settings.audio_backend == "openai_compat"
}

fn audio_host_label(base: &str) -> &'static str {
    let b = base.trim().trim_end_matches('/');
    if b.contains("api.groq.com") {
        "Groq"
    } else if b.is_empty() || b.contains("api.openai.com") {
        "OpenAI"
    } else {
        "Online"
    }
}

/// Catalog id, or the API model name when speech is coming from a host.
fn active_speech_id(settings: &Settings) -> String {
    if is_online_audio(settings) {
        let t = settings.audio_api_model.trim();
        if t.is_empty() {
            "online model".to_owned()
        } else {
            t.to_owned()
        }
    } else {
        settings.model_id.clone()
    }
}

fn active_speech_name(settings: &Settings) -> String {
    if is_online_audio(settings) {
        active_speech_id(settings)
    } else {
        models::find(&settings.model_id)
            .map(|m| m.name)
            .unwrap_or_else(|| settings.model_id.clone())
    }
}

fn active_speech_engine_label(settings: &Settings) -> String {
    if is_online_audio(settings) {
        audio_host_label(&settings.audio_api_base).to_owned()
    } else {
        models::find(&settings.model_id)
            .map(|m| m.engine.label().to_owned())
            .unwrap_or_else(|| "Local".to_owned())
    }
}

fn active_speech_ready(settings: &Settings, data_dir: &Path) -> bool {
    if is_online_audio(settings) {
        !settings.audio_api_model.trim().is_empty()
    } else {
        models::find(&settings.model_id)
            .is_some_and(|m| m.is_downloaded(data_dir))
    }
}

fn active_speech_source(settings: &Settings) -> &'static str {
    if is_online_audio(settings) {
        "online"
    } else {
        "local"
    }
}

#[tauri::command]
fn get_status(state: State<'_, Mutex<AppState>>) -> Status {
    let s = state.lock().unwrap();
    let engine = models::find(&s.settings.model_id)
        .map(|m| m.engine)
        .unwrap_or(EngineKind::Whisper);
    Status {
        recording: s.recording,
        model_id: s.settings.model_id.clone(),
        engine,
        engine_label: active_speech_engine_label(&s.settings),
        model_loaded: models::find(&s.settings.model_id)
            .is_some_and(|m| m.is_downloaded(&s.data_dir)),
        whisper_binary: resolve_binary(&s.data_dir).map(|p| p.display().to_string()),
        data_dir: s.data_dir.display().to_string(),
        audio_device: resolve_capture_device(s.settings.audio_device.as_deref())
            .ok()
            .map(|c| c.name)
            .or_else(default_input_name),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        recommended_model: onboarding::recommended_model_id().to_owned(),
        transcribing: s.transcribing,
        speech_source: active_speech_source(&s.settings).to_owned(),
        speech_model: active_speech_id(&s.settings),
        speech_model_name: active_speech_name(&s.settings),
        speech_ready: active_speech_ready(&s.settings, &s.data_dir),
    }
}

#[tauri::command]
fn get_models(state: State<'_, Mutex<AppState>>) -> Vec<ModelStatus> {
    let s = state.lock().unwrap();
    models::catalog()
        .into_iter()
        .map(|entry| {
            let id = entry.id.clone();
            ModelStatus {
                downloaded: entry.is_downloaded(&s.data_dir),
                active: !is_online_audio(&s.settings) && id == s.settings.model_id,
                downloading: s.downloading.contains_key(&id),
                entry,
            }
        })
        .collect()
}

#[tauri::command]
fn select_model(state: State<'_, Mutex<AppState>>, id: String) -> Result<String, String> {
    let entry = models::find(&id).ok_or_else(|| format!("unknown model: {id}"))?;
    let mut s = state.lock().unwrap();
    s.settings.model_id = id.clone();
    // Catalog "Use" means this local model — leave the online API.
    s.settings.audio_backend = "local".to_owned();
    s.save_settings();
    // Warm up Parakeet in the background so the first dictation doesn't pay
    // the model-load cost (mirrors SpeakType's warmUp).
    if entry.engine == EngineKind::Parakeet && entry.is_downloaded(&s.data_dir) {
        s.transcriber.preload(&id);
    }
    Ok(format!("selected {}", entry.name))
}

#[derive(Debug, Clone, Serialize)]
struct DownloadProgress {
    model_id: String,
    file: String,
    received: u64,
    total: Option<u64>,
}

#[tauri::command]
async fn download_model(
    app: tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
    id: String,
) -> Result<String, String> {
    let entry = models::find(&id).ok_or_else(|| format!("unknown model: {id}"))?;
    let (data_dir, transcriber, cancel) = {
        let mut s = state.lock().unwrap();
        if s.downloading.contains_key(&id) {
            return Err("download already in progress".to_owned());
        }
        let cancel = Arc::new(AtomicBool::new(false));
        s.downloading.insert(id.clone(), cancel.clone());
        (s.data_dir.clone(), s.transcriber.clone(), cancel)
    };

    let result = async {
        let dir = entry.dir(&data_dir);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let mut downloaded_mb = 0u64;
        for file in &entry.files {
            let dest = dir.join(&file.name);
            if dest.is_file() {
                downloaded_mb += dest.metadata().map(|m| m.len()).unwrap_or(0) / 1_000_000;
                continue;
            }
            let _ = app.emit(
                "dictflow://download",
                format!("downloading {}…", file.name),
            );
            let resp = reqwest::get(&file.url).await.map_err(|e| e.to_string())?;
            if !resp.status().is_success() {
                return Err(format!("download failed for {}: HTTP {}", file.name, resp.status()));
            }
            // Stream to a `.part` file so an interrupted download never looks
            // complete; rename only on success.
            let part = dest.with_extension("part");
            let mut out =
                tokio::fs::File::create(&part).await.map_err(|e| e.to_string())?;
            let total = resp.content_length();
            let mut received: u64 = 0;
            let mut last_emit: u64 = 0;
            use futures_util::StreamExt as _;
            use tokio::io::AsyncWriteExt as _;
            let mut stream = resp.bytes_stream();
            while let Some(chunk) = stream.next().await {
                if cancel.load(Ordering::Relaxed) {
                    drop(out);
                    let _ = std::fs::remove_file(&part);
                    return Err("download cancelled".to_owned());
                }
                let chunk = chunk.map_err(|e| e.to_string())?;
                out.write_all(&chunk).await.map_err(|e| e.to_string())?;
                received += chunk.len() as u64;
                if received - last_emit >= 256 * 1024 {
                    last_emit = received;
                    let _ = app.emit(
                        "dictflow://download-progress",
                        DownloadProgress {
                            model_id: id.clone(),
                            file: file.name.clone(),
                            received,
                            total,
                        },
                    );
                }
            }
            out.flush().await.map_err(|e| e.to_string())?;
            drop(out);
            std::fs::rename(&part, &dest).map_err(|e| e.to_string())?;
            downloaded_mb += received / 1_000_000;
        }
        Ok::<u64, String>(downloaded_mb)
    }
    .await;

    {
        let mut s = state.lock().unwrap();
        s.downloading.remove(&id);
    }

    match result {
        Ok(mb) => {
            if entry.engine == EngineKind::Parakeet {
                transcriber.preload(&id);
            }
            let _ = app.emit("dictflow://download-progress", DownloadProgress {
                model_id: id.clone(),
                file: "done".to_owned(),
                received: 1,
                total: Some(1),
            });
            Ok(format!("downloaded {} ({mb} MB)", entry.name))
        }
        Err(e) => Err(e),
    }
}

#[tauri::command]
fn cancel_download(state: State<'_, Mutex<AppState>>, id: String) -> Result<String, String> {
    let s = state.lock().unwrap();
    let flag = s
        .downloading
        .get(&id)
        .ok_or_else(|| "no download in progress for that model".to_owned())?;
    flag.store(true, Ordering::Relaxed);
    Ok(format!("cancelling {id}"))
}

/// Best-effort removal of a history item's audio file (SpeakType deletes the
/// backing audio on item delete / clear-all so recordings don't leak on disk).
fn remove_audio_file(audio_path: &Option<String>) {
    if let Some(p) = audio_path {
        let path = Path::new(p);
        if path.is_file() {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[tauri::command]
fn get_history(state: State<'_, Mutex<AppState>>) -> Vec<HistoryItem> {
    state.lock().unwrap().history.clone()
}

#[tauri::command]
fn get_stats(state: State<'_, Mutex<AppState>>) -> UsageStats {
    state.lock().unwrap().stats.clone()
}

#[tauri::command]
fn delete_history_item(state: State<'_, Mutex<AppState>>, index: usize) -> Result<(), String> {
    let mut s = state.lock().unwrap();
    if index >= s.history.len() {
        return Err("history item not found".to_owned());
    }
    let item = s.history.remove(index);
    remove_audio_file(&item.audio_path);
    s.save_history();
    Ok(())
}

#[tauri::command]
fn clear_history(state: State<'_, Mutex<AppState>>) {
    let mut s = state.lock().unwrap();
    for item in &s.history {
        remove_audio_file(&item.audio_path);
    }
    s.history.clear();
    s.save_history();
}

/// Read a recording back for in-app playback. Takes only a file *name* and
/// resolves it inside the recordings dir — never an arbitrary path.
#[tauri::command]
fn read_audio_file(state: State<'_, Mutex<AppState>>, name: String) -> Result<Vec<u8>, String> {
    if name.contains(['/', '\\', ':']) || name.contains("..") {
        return Err("invalid file name".to_owned());
    }
    let s = state.lock().unwrap();
    let path = s.data_dir.join("recordings").join(&name);
    if !path.is_file() {
        return Err("audio file not found".to_owned());
    }
    std::fs::read(&path).map_err(|e| e.to_string())
}

#[tauri::command]
fn get_dictionary(state: State<'_, Mutex<AppState>>) -> Vec<text::DictionaryEntry> {
    let s = state.lock().unwrap();
    let mut entries = text::load_dictionary(&s.data_dir);
    text::sort_dictionary(&mut entries);
    entries
}

/// Monotonic-ish unique id for dictionary entries (nanos since epoch).
fn new_id(prefix: &str) -> String {
    format!(
        "{prefix}{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

#[tauri::command]
fn add_dictionary_entry(
    state: State<'_, Mutex<AppState>>,
    trigger: String,
    replacement: String,
    whole_word: Option<bool>,
    kind: Option<String>,
) -> Result<Vec<text::DictionaryEntry>, String> {
    let trigger = trigger.trim().to_owned();
    if trigger.is_empty() {
        return Err("say-what-you-hear (trigger) must not be empty".to_owned());
    }
    let kind = match kind.as_deref() {
        Some("snippet") => "snippet",
        _ => "vocab",
    }
    .to_owned();
    let s = state.lock().unwrap();
    let mut entries = text::load_dictionary(&s.data_dir);
    let mut entry = text::DictionaryEntry::new(new_id("d"), trigger, replacement);
    entry.whole_word = whole_word.unwrap_or(true);
    entry.kind = kind;
    entries.insert(0, entry);
    text::sort_dictionary(&mut entries);
    text::save_dictionary(&s.data_dir, &entries).map_err(|e| e.to_string())?;
    Ok(entries)
}

#[tauri::command]
fn set_dictionary_enabled(
    state: State<'_, Mutex<AppState>>,
    id: String,
    enabled: bool,
) -> Result<(), String> {
    let s = state.lock().unwrap();
    let mut entries = text::load_dictionary(&s.data_dir);
    entries
        .iter_mut()
        .find(|e| e.id == id)
        .ok_or_else(|| "entry not found".to_owned())?
        .enabled = enabled;
    text::save_dictionary(&s.data_dir, &entries).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn delete_dictionary_entry(state: State<'_, Mutex<AppState>>, id: String) -> Result<(), String> {
    let s = state.lock().unwrap();
    let mut entries = text::load_dictionary(&s.data_dir);
    let before = entries.len();
    entries.retain(|e| e.id != id);
    if entries.len() == before {
        return Err("entry not found".to_owned());
    }
    text::save_dictionary(&s.data_dir, &entries).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn star_dictionary_entry(
    state: State<'_, Mutex<AppState>>,
    id: String,
    starred: bool,
) -> Result<Vec<text::DictionaryEntry>, String> {
    let s = state.lock().unwrap();
    let mut entries = text::load_dictionary(&s.data_dir);
    entries
        .iter_mut()
        .find(|e| e.id == id)
        .ok_or_else(|| "entry not found".to_owned())?
        .starred = starred;
    text::sort_dictionary(&mut entries);
    text::save_dictionary(&s.data_dir, &entries).map_err(|e| e.to_string())?;
    Ok(entries)
}

/// History “this should have been X” — add a vocab rule from a correction.
#[tauri::command]
fn add_correction(
    state: State<'_, Mutex<AppState>>,
    heard: String,
    written: String,
) -> Result<Vec<text::DictionaryEntry>, String> {
    let heard = heard.trim().to_owned();
    let written = written.trim().to_owned();
    if heard.is_empty() {
        return Err("heard text must not be empty".to_owned());
    }
    if written.is_empty() {
        return Err("correction must not be empty".to_owned());
    }
    if heard.eq_ignore_ascii_case(&written) {
        return Err("correction is the same as what was heard".to_owned());
    }
    add_dictionary_entry(state, heard, written, Some(true), Some("vocab".into()))
}

/// One-click symbol presets (Superwhisper-style): voice models mangle symbols,
/// so "at sign" said aloud becomes "@". Skips triggers the user already has.
#[tauri::command]
fn add_symbol_presets(
    state: State<'_, Mutex<AppState>>,
) -> Result<Vec<text::DictionaryEntry>, String> {
    const PRESETS: &[(&str, &str)] = &[
        ("at sign", "@"),
        ("dot com", ".com"),
        ("dot net", ".net"),
        ("dot org", ".org"),
        ("dot io", ".io"),
        ("hashtag", "#"),
        ("slash", "/"),
        ("underscore", "_"),
        ("dash", "-"),
    ];
    let s = state.lock().unwrap();
    let mut entries = text::load_dictionary(&s.data_dir);
    for (trigger, replacement) in PRESETS {
        if entries.iter().any(|e| {
            !e.is_snippet() && e.trigger.eq_ignore_ascii_case(trigger)
        }) {
            continue;
        }
        entries.insert(
            0,
            text::DictionaryEntry::new(new_id("d"), *trigger, *replacement),
        );
    }
    text::save_dictionary(&s.data_dir, &entries).map_err(|e| e.to_string())?;
    Ok(entries)
}

#[tauri::command]
fn export_dictionary(state: State<'_, Mutex<AppState>>, path: String) -> Result<String, String> {
    let s = state.lock().unwrap();
    let entries = text::load_dictionary(&s.data_dir);
    let bytes = serde_json::to_vec_pretty(&entries).map_err(|e| e.to_string())?;
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    Ok(format!("exported {} rules → {path}", entries.len()))
}

#[tauri::command]
fn import_dictionary(
    state: State<'_, Mutex<AppState>>,
    path: String,
) -> Result<Vec<text::DictionaryEntry>, String> {
    let bytes = std::fs::read(&path).map_err(|e| format!("read file: {e}"))?;
    let mut entries: Vec<text::DictionaryEntry> =
        serde_json::from_slice(&bytes).map_err(|_| "not a DictFlow dictionary file".to_owned())?;
    if entries.len() > 1000 {
        return Err("dictionary file has more than 1000 rules".to_owned());
    }
    for e in &mut entries {
        e.trigger = e.trigger.trim().to_owned();
        if e.trigger.is_empty() {
            return Err("dictionary file contains an empty trigger".to_owned());
        }
        e.id = new_id("d");
    }
    let s = state.lock().unwrap();
    text::save_dictionary(&s.data_dir, &entries).map_err(|e| e.to_string())?;
    Ok(entries)
}

/// Set to `"owner/repo"` once the project is pushed to GitHub to enable the
/// in-app update check (see docs/RELEASE.md).
const UPDATE_CHECK_REPO: Option<&str> = None;

#[derive(Debug, Clone, Serialize)]
struct UpdateInfo {
    current: String,
    latest: Option<String>,
    url: Option<String>,
    available: bool,
    configured: bool,
}

#[tauri::command]
async fn check_for_updates() -> Result<UpdateInfo, String> {
    let current = env!("CARGO_PKG_VERSION").to_owned();
    let Some(repo) = UPDATE_CHECK_REPO else {
        return Ok(UpdateInfo {
            current,
            latest: None,
            url: None,
            available: false,
            configured: false,
        });
    };
    #[derive(Deserialize)]
    struct GhRelease {
        tag_name: String,
        html_url: String,
    }
    let release: GhRelease = reqwest::Client::new()
        .get(format!("https://api.github.com/repos/{repo}/releases/latest"))
        .header("User-Agent", "dictflow")
        .header("Accept", "application/vnd.github.v3+json")
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let latest = release.tag_name.trim_start_matches('v').to_owned();
    Ok(UpdateInfo {
        available: latest != current,
        url: Some(release.html_url),
        latest: Some(release.tag_name),
        current,
        configured: true,
    })
}

#[derive(Debug, Clone, Serialize)]
struct ProviderStatus {
    audio_key_masked: String,
    llm_key_masked: String,
    has_audio_key: bool,
    has_llm_key: bool,
    default_prompt: String,
    prompt_presets: HashMap<String, String>,
}

#[tauri::command]
fn get_provider_status(state: State<'_, Mutex<AppState>>) -> ProviderStatus {
    let dir = state.lock().unwrap().data_dir.clone();
    let s = polish::load_secrets(&dir);
    ProviderStatus {
        audio_key_masked: polish::mask_key(&s.audio_api_key),
        llm_key_masked: polish::mask_key(&s.llm_api_key),
        has_audio_key: !s.audio_api_key.trim().is_empty(),
        has_llm_key: !s.llm_api_key.trim().is_empty(),
        default_prompt: polish::default_prompt().to_owned(),
        prompt_presets: polish::prompt_catalog(),
    }
}

#[tauri::command]
fn set_provider_secrets(
    state: State<'_, Mutex<AppState>>,
    audio_api_key: Option<String>,
    llm_api_key: Option<String>,
) -> Result<ProviderStatus, String> {
    let dir = state.lock().unwrap().data_dir.clone();
    let mut s = polish::load_secrets(&dir);
    if let Some(k) = audio_api_key {
        if !k.trim().is_empty() && !k.contains('…') {
            s.audio_api_key = k.trim().to_owned();
        }
    }
    if let Some(k) = llm_api_key {
        if !k.trim().is_empty() && !k.contains('…') {
            s.llm_api_key = k.trim().to_owned();
        }
    }
    polish::save_secrets(&dir, &s).map_err(|e| e.to_string())?;
    Ok(get_provider_status(state))
}

#[tauri::command]
fn test_llm(state: State<'_, Mutex<AppState>>) -> Result<String, String> {
    let (settings, dir) = {
        let s = state.lock().unwrap();
        (s.settings.clone(), s.data_dir.clone())
    };
    let cfg = polish_cfg(&settings, &dir);
    polish::ping_llm(
        &cfg.llm_backend,
        &cfg.llm_api_base,
        &cfg.llm_api_key,
        &cfg.llm_api_model,
        cfg.llm_timeout_ms,
    )
    .map(|_| "LLM responded".to_owned())
    .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn test_audio_api(app: tauri::AppHandle) -> Result<String, String> {
    let (settings, dir) = {
        let st = app.state::<Mutex<AppState>>();
        let mut s = st.lock().unwrap();
        if s.recording {
            return Err("already recording — stop first".to_owned());
        }
        start_recording(&mut s).map_err(|e| e.to_string())?;
        (s.settings.clone(), s.data_dir.clone())
    };
    let _ = app.emit("dictflow://recording", true);
    spawn_record_ticker(app.clone());
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let wav = {
        let st = app.state::<Mutex<AppState>>();
        let mut s = st.lock().unwrap();
        if !s.recording {
            return Err("recording stopped early — try again".to_owned());
        }
        let (wav, _) = stop_and_save_wav(&mut s).map_err(|e| e.to_string())?;
        wav
    };
    let _ = app.emit("dictflow://recording", false);
    let cfg = polish_cfg(&settings, &dir);
    let samples = match text::load_wav_mono_16k(&wav) {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_file(&wav);
            return Err(e.to_string());
        }
    };
    if let Some(why) = devices::blank_audio(&samples, 16_000) {
        let _ = std::fs::remove_file(&wav);
        return Err(why.message().to_owned());
    }
    let bytes = std::fs::read(&wav).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&wav);
    let base = cfg.audio_api_base.clone();
    let key = cfg.audio_api_key.clone();
    let model = cfg.audio_api_model.clone();
    let timeout = cfg.llm_timeout_ms.max(15_000);
    let result = tokio::task::spawn_blocking(move || {
        polish::transcribe_openai(&base, &key, &model, timeout, &bytes)
    })
    .await
    .map_err(|e| format!("audio test task failed: {e}"))?;
    match result {
        Ok(t) if t.trim().is_empty() => {
            Ok("Audio API accepted the clip (empty transcript — speak louder)".to_owned())
        }
        Ok(t) => Ok(format!("Audio API heard: {}", truncate(&t, 80))),
        Err(e) => {
            let msg = format!("{e:#}");
            if msg.contains("empty audio transcript") {
                Ok("Audio API accepted the clip (empty transcript — speak louder)".to_owned())
            } else {
                Err(msg)
            }
        }
    }
}

#[tauri::command]
fn get_settings(state: State<'_, Mutex<AppState>>) -> Settings {
    state.lock().unwrap().settings.clone()
}

#[tauri::command]
fn set_settings(
    app: tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
    settings: Settings,
) -> Result<Settings, String> {
    if models::find(&settings.model_id).is_none() {
        return Err(format!("unknown model: {}", settings.model_id));
    }
    if hotkey_vk(&settings.hotkey_key).is_none() && settings.hotkey_key != "CtrlAltSpace" {
        return Err(format!("unknown talk key: {}", settings.hotkey_key));
    }
    if settings.recording_mode != "hold" && settings.recording_mode != "toggle" {
        return Err(format!("unknown recording mode: {}", settings.recording_mode));
    }
    if settings.audio_backend != "local" && settings.audio_backend != "openai_compat" {
        return Err(format!("unknown audio backend: {}", settings.audio_backend));
    }
    if !matches!(
        settings.llm_backend.as_str(),
        "off" | "local" | "openai_compat" | "anthropic"
    ) {
        return Err(format!("unknown LLM backend: {}", settings.llm_backend));
    }
    let edge = overlay::DockEdge::parse(&settings.overlay_edge)
        .ok_or_else(|| format!("unknown overlay edge: {}", settings.overlay_edge))?;
    let paste = validate_utility_key(&settings.paste_last_key, &settings.copy_last_key)?;
    let copy = validate_utility_key(&settings.copy_last_key, &settings.paste_last_key)?;
    let mut settings = settings;
    if !polish::PRESETS.contains(&settings.llm_preset.as_str()) {
        settings.llm_preset = "clean".to_owned();
    }
    settings.overlay_edge = edge.as_str().to_owned();
    settings.paste_last_key = shortcuts::format_shortcut(&paste);
    settings.copy_last_key = shortcuts::format_shortcut(&copy);
    let mut s = state.lock().unwrap();
    let warm = settings.model_id != s.settings.model_id
        && models::find(&settings.model_id).is_some_and(|m| {
            m.engine == EngineKind::Parakeet && m.is_downloaded(&s.data_dir)
        });
    let warm_id = settings.model_id.clone();
    s.settings = settings.clone();
    s.save_settings();
    if warm {
        s.transcriber.preload(&warm_id);
    }
    drop(s);
    apply_hotkey_registration(&app);
    apply_utility_shortcuts(&app);
    apply_overlay_visibility(&app);
    Ok(settings)
}

/// Discard the live recording without transcribing, filing, or pasting
/// (SpeakType's cancel path: modifier-combo pressed mid-take).
fn cancel_recording(state: &mut AppState) -> anyhow::Result<()> {
    state.hotkey_owned = false;
    let (wav, _) = stop_and_save_wav(state)?;
    let _ = std::fs::remove_file(wav);
    Ok(())
}

#[tauri::command]
fn cancel_dictation(
    app: tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
) -> Result<String, String> {
    let mut s = state.lock().unwrap();
    if !s.recording {
        return Err("not recording".to_owned());
    }
    cancel_recording(&mut s).map_err(|e| e.to_string())?;
    s.transcribing = false;
    drop(s);
    let _ = app.emit("dictflow://recording", false);
    let _ = app.emit("dictflow://transcribing", false);
    Ok("cancelled".to_owned())
}

fn last_text(state: &AppState) -> Option<String> {
    state.history.first().map(|h| h.text.clone()).filter(|t| !t.is_empty())
}

#[tauri::command]
fn copy_last(state: State<'_, Mutex<AppState>>) -> Result<String, String> {
    let text = last_text(&state.lock().unwrap())
        .ok_or_else(|| "nothing to copy — dictate first".to_owned())?;
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    cb.set_text(text.clone()).map_err(|e| e.to_string())?;
    Ok(format!("copied: {}", truncate(&text, 80)))
}

#[tauri::command]
fn paste_last(app: tauri::AppHandle, state: State<'_, Mutex<AppState>>) -> Result<String, String> {
    remember_paste_target(&app);
    let (text, saved) = {
        let s = state.lock().unwrap();
        (
            last_text(&s).ok_or_else(|| "nothing to paste — dictate first".to_owned())?,
            s.paste_target,
        )
    };
    paste_text(&text, paste_target_for(&app, saved)).map_err(|e| format!("{e:#}"))?;
    Ok(format!("pasted: {}", truncate(&text, 80)))
}

#[tauri::command]
fn snap_overlay(x: i32, y: i32, screen_w: i32, screen_h: i32) -> overlay::OverlayPose {
    overlay::snap_to_edge(x, y, screen_w, screen_h, overlay::PILL_W, overlay::PILL_H)
}

#[tauri::command]
fn set_overlay_pose(
    app: tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
    edge: String,
    offset: i32,
) -> Result<Settings, String> {
    let parsed = overlay::DockEdge::parse(&edge)
        .ok_or_else(|| format!("unknown overlay edge: {edge}"))?;
    let mut s = state.lock().unwrap();
    s.settings.overlay_edge = parsed.as_str().to_owned();
    s.settings.overlay_offset = offset;
    s.save_settings();
    let out = s.settings.clone();
    drop(s);
    apply_overlay_visibility(&app);
    Ok(out)
}

fn apply_overlay_visibility(app: &tauri::AppHandle) {
    let Some(win) = app.get_webview_window("overlay") else {
        return;
    };
    let settings = app
        .state::<Mutex<AppState>>()
        .lock()
        .ok()
        .map(|s| s.settings.clone());
    let Some(settings) = settings else {
        return;
    };
    if !settings.overlay_enabled {
        let _ = win.hide();
        return;
    }
    focus::make_non_activating(&win);
    let _ = win.show();
    let _ = win.set_size(tauri::LogicalSize::new(
        overlay::PILL_W as f64,
        overlay::PILL_H as f64,
    ));
    if let Ok(Some(m)) = win.current_monitor() {
        let size = m.size();
        let origin = m.position();
        let edge = overlay::DockEdge::parse(&settings.overlay_edge).unwrap_or(overlay::DockEdge::Bottom);
        let (x, y) = overlay::pose_to_xy(
            edge,
            settings.overlay_offset,
            size.width as i32,
            size.height as i32,
            overlay::PILL_W,
            overlay::PILL_H,
        );
        let _ = win.set_position(tauri::PhysicalPosition::new(origin.x + x, origin.y + y));
    }
    focus::make_non_activating(&win);
}

#[tauri::command]
async fn toggle_recording(
    app: tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
) -> Result<String, String> {
    remember_paste_target(&app);
    if !state.lock().unwrap().recording {
        let mut s = state.lock().unwrap();
        let preferred = s.settings.audio_device.clone();
        let choice = start_recording(&mut s).map_err(|e| e.to_string())?;
        s.hotkey_owned = false;
        drop(s);
        emit_device_fallback(&app, preferred.as_deref(), &choice);
        let _ = app.emit("dictflow://recording", true);
        spawn_record_ticker(app.clone());
        return Ok("recording… press hotkey again to transcribe".to_owned());
    }
    stop_transcribe(&app, state).await
}

/// Stop the live recording, transcribe, file it in history, and paste.
/// Shared by the UI toggle, the tray item, and the talk-key handlers.
async fn stop_transcribe(
    app: &tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
) -> Result<String, String> {

    // Snapshot everything the heavy work needs, then release the lock.
    let (input, parakeet, duration_secs, history_model, audio, cleanup) = {
        let mut s = state.lock().unwrap();
        s.hotkey_owned = false;
        let (wav, duration_secs) = stop_and_save_wav(&mut s).map_err(|e| e.to_string())?;
        let model_id = s.settings.model_id.clone();
        let history_model = active_speech_id(&s.settings);
        let cleanup = s.settings.cleanup.clone();
        let audio = wav.display().to_string();
        let input = TranscribeInput {
            wav_path: wav,
            model_id: model_id.clone(),
            language: s.settings.language.clone(),
            cleanup: cleanup.clone(),
            translate: s.settings.translate,
            data_dir: s.data_dir.clone(),
            dictionary: text::load_dictionary(&s.data_dir),
            polish: polish_cfg(&s.settings, &s.data_dir),
        };
        (input, s.transcriber.clone(), duration_secs, history_model, audio, cleanup)
    };
    set_transcribing(app, true);
    let _ = app.emit("dictflow://recording", false);

    // STT + post-processing can take seconds (model load, inference) — keep it
    // off the async runtime.
    let wav_for_cleanup = input.wav_path.clone();
    let result = tokio::task::spawn_blocking(move || run_transcription(&input, &parakeet)).await;
    set_transcribing(app, false);
    let result = result
        .map_err(|e| format!("transcription task failed: {e}"))
        .and_then(|r| r.map_err(|e| format!("{e:#}")));
    if let Err(e) = &result {
        if devices::is_blank_audio_error(e) {
            let _ = std::fs::remove_file(&wav_for_cleanup);
        }
        return Err(e.clone());
    }
    let result = result.unwrap();
    let text = result.text.clone();

    let (auto_paste, saved_target) = {
        let mut s = state.lock().unwrap();
        s.push_history(NewHistory {
            text: result.text,
            raw_text: result.raw_text,
            duration_secs,
            model: history_model,
            audio_path: Some(audio),
            dict_hits: result.dict_hits,
            cleanup,
        });
        (s.settings.auto_paste, s.paste_target)
    };
    let _ = app.emit("dictflow://history-updated", ());
    let _ = app.emit("dictflow://committed", committed(text.clone()));

    if !auto_paste {
        return Ok(format!("transcribed (auto-paste off): {}", truncate(&text, 160)));
    }
    // Pasting must not kill the transcription if the foreground app rejects keys.
    match paste_text(&text, paste_target_for(app, saved_target)) {
        Ok(()) => Ok(format!("pasted: {}", truncate(&text, 80))),
        Err(e) => Ok(format!(
            "transcribed (paste failed: {e}): {}",
            truncate(&text, 160)
        )),
    }
}

/// Transcribe an audio file on disk (WAV for now — the loader reads PCM WAV;
/// mp3/video need a decoder and are a v0.3 item). Saves to history like a
/// dictation. Mirrors SpeakType's TranscribeAudioView minus drag-drop.
#[tauri::command]
async fn transcribe_file(
    app: tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
    path: String,
) -> Result<String, String> {
    if !Path::new(&path).is_file() {
        return Err("file not found".to_owned());
    }
    // Copy into our recordings folder: playback keeps working if the original
    // moves, and the webview can load it back by file name.
    let owned = {
        let s = state.lock().unwrap();
        let dir = s.data_dir.join("recordings");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        dir.join(format!("file-{stamp}.wav"))
    };
    std::fs::copy(&path, &owned).map_err(|e| format!("copy audio file: {e}"))?;
    let (input, parakeet) = {
        let s = state.lock().unwrap();
        let input = TranscribeInput {
            wav_path: owned.clone(),
            model_id: s.settings.model_id.clone(),
            language: s.settings.language.clone(),
            cleanup: s.settings.cleanup.clone(),
            translate: s.settings.translate,
            data_dir: s.data_dir.clone(),
            dictionary: text::load_dictionary(&s.data_dir),
            polish: polish_cfg(&s.settings, &s.data_dir),
        };
        (input, s.transcriber.clone())
    };
    set_transcribing(&app, true);
    let result = tokio::task::spawn_blocking(move || run_transcription(&input, &parakeet)).await;
    set_transcribing(&app, false);
    let result = result
        .map_err(|e| format!("transcription task failed: {e}"))
        .and_then(|r| r.map_err(|e| format!("{e:#}")));
    if let Err(e) = &result {
        if devices::is_blank_audio_error(e) {
            let _ = std::fs::remove_file(&owned);
        }
        return Err(e.clone());
    }
    let result = result.unwrap();
    {
        let mut s = state.lock().unwrap();
        let model = active_speech_id(&s.settings);
        let cleanup = s.settings.cleanup.clone();
        s.push_history(NewHistory {
            text: result.text.clone(),
            raw_text: result.raw_text,
            duration_secs: result.duration_secs,
            model,
            audio_path: Some(owned.display().to_string()),
            dict_hits: result.dict_hits,
            cleanup,
        });
    }
    let _ = app.emit("dictflow://history-updated", ());
    let _ = app.emit("dictflow://committed", committed(result.text.clone()));
    Ok(result.text)
}

#[tauri::command]
fn onboard_step(current: String, backward: bool) -> Result<String, String> {
    let step = onboarding::OnboardStep::parse(&current)
        .ok_or_else(|| format!("unknown onboard step: {current}"))?;
    let next = if backward {
        onboarding::prev(step)
    } else {
        onboarding::next(step)
    };
    Ok(next.as_str().to_owned())
}

#[tauri::command]
fn copy_chip_open(pasted_unix_ms: u64) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    overlay::copy_chip_visible(pasted_unix_ms, now, overlay::COPY_CHIP_MS)
}

#[tauri::command]
fn delete_model(state: State<'_, Mutex<AppState>>, id: String) -> Result<String, String> {
    let entry = models::find(&id).ok_or_else(|| format!("unknown model: {id}"))?;
    let mut s = state.lock().unwrap();
    let dir = entry.dir(&s.data_dir);
    if dir.exists() {
        // Note: if the model is currently loaded in the transcriber thread the
        // OS may hold the files open — then this fails with a sharing
        // violation and a restart clears it. Same class of issue as any
        // on-device model manager.
        std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    }
    if s.settings.model_id == id {
        s.settings.model_id = models::default_model_id();
        s.save_settings();
    }
    Ok(format!("deleted {}", entry.name))
}

// ---------------------------------------------------------------------------
// Tray + app setup
// ---------------------------------------------------------------------------

fn wants_tray_launch<I, S>(args: I, onboarded: bool) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    onboarded && args.into_iter().any(|a| a.as_ref() == "--minimized")
}

fn hide_main_to_tray(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
        let _ = w.set_skip_taskbar(true);
    }
}

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.set_skip_taskbar(false);
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn spawn_tray_toggle(app: &tauri::AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state: State<'_, Mutex<AppState>> = app.state();
        let _ = toggle_recording(app.clone(), state).await;
    });
}

fn build_tray(app: &tauri::AppHandle) -> anyhow::Result<()> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};

    let toggle = MenuItem::with_id(app, "toggle", "Start/Stop dictation", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "Open DictFlow", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&toggle, &settings, &quit])?;

    // tauri.conf.json already creates the "main" tray — attach to it so we
    // don't end up with a second icon.
    let tray = app
        .tray_by_id("main")
        .ok_or_else(|| anyhow::anyhow!("missing tray icon"))?;
    tray.set_menu(Some(menu))?;
    tray.set_tooltip(Some("DictFlow — offline voice dictation"))?;
    tray.set_show_menu_on_left_click(false)?;
    tray.on_menu_event(|app, event| match event.id.as_ref() {
        "quit" => app.exit(0),
        "settings" => show_main_window(app),
        "toggle" => spawn_tray_toggle(app),
        _ => {}
    });
    tray.on_tray_icon_event(|tray, event| match event {
        TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        }
        | TrayIconEvent::DoubleClick {
            button: MouseButton::Left,
            ..
        } => show_main_window(tray.app_handle()),
        _ => {}
    });
    Ok(())
}

pub fn run() {
    let builder = tauri::Builder::default()
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .targets([
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::LogDir {
                        file_name: Some("dictflow".into()),
                    }),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Webview),
                ])
                .build(),
        )
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_main_window(app);
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--minimized"]),
        ))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_shortcuts([HOTKEY])
                .expect("parse hotkey")
                .with_handler(|app, shortcut, event| {
                    if event.state != ShortcutState::Pressed {
                        return;
                    }
                    let expected = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Space);
                    if *shortcut == expected {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let state: State<'_, Mutex<AppState>> = app.state();
                            match toggle_recording(app.clone(), state).await {
                                Ok(msg) => log::info!("dictation: {msg}"),
                                Err(e) => log::error!("dictation error: {e}"),
                            }
                        });
                        return;
                    }
                    let (paste, copy) = {
                        let st = app.state::<Mutex<AppState>>();
                        let Ok(s) = st.lock() else {
                            return;
                        };
                        let paste = shortcuts::parse_shortcut(&s.settings.paste_last_key)
                            .ok()
                            .and_then(|sp| spec_to_shortcut(&sp).ok());
                        let copy = shortcuts::parse_shortcut(&s.settings.copy_last_key)
                            .ok()
                            .and_then(|sp| spec_to_shortcut(&sp).ok());
                        (paste, copy)
                    };
                    if paste.as_ref() == Some(shortcut) {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let state: State<'_, Mutex<AppState>> = app.state();
                            match paste_last(app.clone(), state) {
                                Ok(msg) => log::info!("{msg}"),
                                Err(e) => log::error!("paste-last: {e}"),
                            }
                        });
                    } else if copy.as_ref() == Some(shortcut) {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let state: State<'_, Mutex<AppState>> = app.state();
                            match copy_last(state) {
                                Ok(msg) => log::info!("{msg}"),
                                Err(e) => log::error!("copy-last: {e}"),
                            }
                        });
                    }
                })
                .build(),
        )
        .setup(|app| {
            let data_dir = app
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("dictflow");
            let _ = std::fs::create_dir_all(data_dir.join("models"));
            let _ = std::fs::create_dir_all(data_dir.join("bin"));

            let transcriber = Transcriber::spawn(data_dir.clone());
            let mut st = AppState {
                recording: false,
                transcribing: false,
                hotkey_owned: false,
                active: None,
                settings: Settings::default(),
                history: Vec::new(),
                stats: UsageStats::default(),
                downloading: HashMap::new(),
                utility_shortcuts: Vec::new(),
                data_dir,
                transcriber,
                paste_target: None,
            };
            st.load_settings();
            st.load_history();
            st.stats = UsageStats::load(&st.data_dir);
            st.seed_stats_from_history();
            // Warm up the selected Parakeet model in the background so the
            // first dictation doesn't pay the model-load cost.
            if let Some(entry) = models::find(&st.settings.model_id) {
                if entry.engine == EngineKind::Parakeet && entry.is_downloaded(&st.data_dir) {
                    st.transcriber.preload(&st.settings.model_id);
                }
            }
            app.manage(Mutex::new(st));
            build_tray(app.handle()).expect("build tray");
            apply_hotkey_registration(app.handle());
            apply_utility_shortcuts(app.handle());
            apply_overlay_visibility(app.handle());
            let onboarded = app
                .state::<Mutex<AppState>>()
                .lock()
                .map(|s| s.settings.onboarded)
                .unwrap_or(false);
            if wants_tray_launch(std::env::args(), onboarded) {
                hide_main_to_tray(app.handle());
            }

            // Single-key talk-button edges → start/stop. Lives for the app's
            // lifetime; the polling thread exits if the channel closes.
            let hk_app = app.handle().clone();
            let (hk_tx, mut hk_rx) =
                tokio::sync::mpsc::unbounded_channel::<TalkEdge>();
            spawn_hotkey_thread(hk_app.clone(), hk_tx);
            tauri::async_runtime::spawn(async move {
                while let Some(edge) = hk_rx.recv().await {
                    match edge {
                        TalkEdge::Pressed => {
                            let (mode, recording) = {
                                let s: State<'_, Mutex<AppState>> = hk_app.state();
                                let guard = s.lock().unwrap();
                                (guard.settings.recording_mode.clone(), guard.recording)
                            };
                            if recording {
                                // hold mode: press while a UI-owned recording runs → ignore.
                                if mode == "toggle" {
                                    let st: State<'_, Mutex<AppState>> = hk_app.state();
                                    match stop_transcribe(&hk_app, st).await {
                                        Ok(msg) => log::info!("dictation: {msg}"),
                                        Err(e) => log::error!("dictation error: {e}"),
                                    }
                                }
                            } else {
                                let st: State<'_, Mutex<AppState>> = hk_app.state();
                                let mut guard = st.lock().unwrap();
                                let preferred = guard.settings.audio_device.clone();
                                match start_recording(&mut guard) {
                                    Ok(choice) => {
                                        guard.hotkey_owned = true;
                                        drop(guard);
                                        emit_device_fallback(&hk_app, preferred.as_deref(), &choice);
                                        let _ = hk_app.emit("dictflow://recording", true);
                                        spawn_record_ticker(hk_app.clone());
                                    }
                                    Err(e) => log::error!("hotkey record failed: {e:#}"),
                                }
                            }
                        }
                        TalkEdge::Released => {
                            let (mode, recording, owned) = {
                                let s: State<'_, Mutex<AppState>> = hk_app.state();
                                let guard = s.lock().unwrap();
                                (
                                    guard.settings.recording_mode.clone(),
                                    guard.recording,
                                    guard.hotkey_owned,
                                )
                            };
                            if mode == "hold" && recording && owned {
                                let st: State<'_, Mutex<AppState>> = hk_app.state();
                                match stop_transcribe(&hk_app, st).await {
                                    Ok(msg) => log::info!("dictation: {msg}"),
                                    Err(e) => log::error!("dictation error: {e}"),
                                }
                            }
                        }
                        TalkEdge::Cancel => {
                            let owned = {
                                let s: State<'_, Mutex<AppState>> = hk_app.state();
                                let guard = s.lock().unwrap();
                                guard.recording && guard.hotkey_owned
                            };
                            if owned {
                                let st: State<'_, Mutex<AppState>> = hk_app.state();
                                let mut guard = st.lock().unwrap();
                                match cancel_recording(&mut guard) {
                                    Ok(()) => {
                                        drop(guard);
                                        let _ = hk_app.emit("dictflow://recording", false);
                                        log::info!("dictation cancelled (other key pressed)");
                                    }
                                    Err(e) => log::error!("dictation cancel failed: {e:#}"),
                                }
                            }
                        }
                        TalkEdge::Escape => {
                            let recording = {
                                let s: State<'_, Mutex<AppState>> = hk_app.state();
                                let rec = s.lock().unwrap().recording;
                                rec
                            };
                            if recording {
                                let st: State<'_, Mutex<AppState>> = hk_app.state();
                                let mut guard = st.lock().unwrap();
                                match cancel_recording(&mut guard) {
                                    Ok(()) => {
                                        drop(guard);
                                        let _ = hk_app.emit("dictflow://recording", false);
                                        log::info!("dictation cancelled (Esc)");
                                    }
                                    Err(e) => log::error!("dictation cancel failed: {e:#}"),
                                }
                            }
                        }
                    }
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() != "main" {
                return;
            }
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                hide_main_to_tray(window.app_handle());
                apply_overlay_visibility(window.app_handle());
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_models,
            select_model,
            download_model,
            cancel_download,
            get_history,
            get_stats,
            delete_history_item,
            clear_history,
            get_dictionary,
            add_dictionary_entry,
            set_dictionary_enabled,
            delete_dictionary_entry,
            add_symbol_presets,
            export_dictionary,
            import_dictionary,
            star_dictionary_entry,
            add_correction,
            check_for_updates,
            get_settings,
            set_settings,
            get_provider_status,
            set_provider_secrets,
            test_llm,
            test_audio_api,
            toggle_recording,
            cancel_dictation,
            copy_last,
            paste_last,
            snap_overlay,
            set_overlay_pose,
            onboard_step,
            copy_chip_open,
            transcribe_file,
            delete_model,
            get_audio_devices,
            test_microphone,
            open_mic_settings,
            read_audio_file
        ]);

    builder
        .run(tauri::generate_context!())
        .expect("error while running DictFlow");
}

fn main() {
    run();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> AppState {
        let dir = std::env::temp_dir().join(format!(
            "dictflow-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        AppState {
            recording: false,
            transcribing: false,
            hotkey_owned: false,
            active: None,
            settings: Settings::default(),
            history: Vec::new(),
            stats: UsageStats::default(),
            downloading: HashMap::new(),
            utility_shortcuts: Vec::new(),
            data_dir: dir,
            transcriber: Transcriber::spawn(std::env::temp_dir()),
            paste_target: None,
        }
    }

    #[test]
    fn catalog_has_both_engines_with_files() {
        let all = models::catalog();
        assert!(all.iter().any(|m| m.engine == EngineKind::Parakeet));
        assert!(all.iter().any(|m| m.engine == EngineKind::Whisper));
        for m in &all {
            assert!(!m.files.is_empty(), "model {} has no files", m.id);
            assert!(!m.files.iter().any(|f| f.url.is_empty()));
        }
        let whisper = models::find("base.en").expect("base.en in catalog");
        assert_eq!(
            whisper
                .single_path(Path::new("C:\\data"))
                .expect("single-file model")
                .file_name()
                .unwrap(),
            "ggml-base.en.bin"
        );
    }

    #[test]
    fn default_model_is_parakeet() {
        let id = models::default_model_id();
        let entry = models::find(&id).expect("default model in catalog");
        assert_eq!(entry.engine, EngineKind::Parakeet);
    }

    #[test]
    fn truncate_keeps_short_text() {
        assert_eq!(truncate("hi", 80), "hi");
    }

    #[test]
    fn truncate_clips_long_text() {
        let long = "x".repeat(200);
        assert_eq!(truncate(&long, 80), format!("{}…", &long[..80]));
    }

    #[test]
    fn history_is_capped_at_limit() {
        // Avoid disk writes: point history at a temp dir (save_history is
        // best-effort and harmless there).
        let mut s = test_state();
        for i in 0..(HISTORY_LIMIT + 10) {
            s.push_history(NewHistory {
                text: format!("item {i}"),
                raw_text: format!("raw {i}"),
                duration_secs: 1.0,
                model: "test".to_owned(),
                audio_path: None,
                dict_hits: 0,
                cleanup: "full".to_owned(),
            });
        }
        assert_eq!(s.history.len(), HISTORY_LIMIT);
        assert_eq!(s.history.first().unwrap().text, format!("item {}", HISTORY_LIMIT + 9));
        // Ledger is independent of the 100-row cap.
        assert_eq!(s.stats.lifetime_dictations, (HISTORY_LIMIT + 10) as u64);
        let _ = std::fs::remove_dir_all(&s.data_dir);
    }

    #[test]
    fn clear_history_keeps_stats() {
        let mut s = test_state();
        s.push_history(NewHistory {
            text: "hello world".into(),
            raw_text: "hello um world".into(),
            duration_secs: 2.0,
            model: "test".into(),
            audio_path: None,
            dict_hits: 1,
            cleanup: "full".into(),
        });
        assert_eq!(s.stats.lifetime_dictations, 1);
        assert_eq!(s.stats.lifetime_words, 2);
        s.history.clear();
        s.save_history();
        assert!(s.history.is_empty());
        assert_eq!(s.stats.lifetime_dictations, 1);
        assert_eq!(s.stats.lifetime_words, 2);
        let reloaded = UsageStats::load(&s.data_dir);
        assert_eq!(reloaded.lifetime_dictations, 1);
        let _ = std::fs::remove_dir_all(&s.data_dir);
    }

    #[test]
    fn seed_stats_from_existing_history_once() {
        let mut s = test_state();
        s.history.push(HistoryItem {
            text: "one two three".into(),
            date_unix: 1_700_000_000,
            duration_secs: 3.0,
            model: "test".into(),
            audio_path: None,
            raw_text: "one two three".into(),
            words_out: 3,
            dict_hits: 0,
            cleanup: "full".into(),
        });
        s.seed_stats_from_history();
        assert_eq!(s.stats.lifetime_dictations, 1);
        assert_eq!(s.stats.lifetime_words, 3);
        s.seed_stats_from_history();
        assert_eq!(s.stats.lifetime_dictations, 1, "must not double-count");
        let _ = std::fs::remove_dir_all(&s.data_dir);
    }

    #[test]
    fn fresh_settings_are_not_onboarded() {
        assert!(!Settings::default().onboarded);
        assert_eq!(Settings::default().paste_last_key, "Shift+Alt+Z");
        assert_eq!(Settings::default().copy_last_key, "Shift+Alt+X");
        assert!(!Settings::default().session_cap);
        assert!(Settings::default().overlay_enabled);
    }

    #[test]
    fn old_settings_json_skips_wizard() {
        let json = r#"{
            "model_id": "parakeet-v3",
            "language": "auto",
            "auto_paste": true,
            "hotkey_key": "RightCtrl",
            "recording_mode": "hold"
        }"#;
        let s: Settings = serde_json::from_str(json).expect("legacy settings");
        assert!(s.onboarded, "missing onboarded must default true");
        assert_eq!(s.paste_last_key, "Shift+Alt+Z");
        assert!(s.overlay_enabled);
        assert_eq!(s.overlay_edge, "bottom");
        assert!(s.audio_device.is_none());
        assert_eq!(s.audio_backend, "local");
        assert_eq!(s.llm_backend, "off");
        assert!(!s.llm_enabled);
    }

    #[test]
    fn autostart_minimized_requires_onboarded() {
        assert!(wants_tray_launch(["dictflow", "--minimized"], true));
        assert!(!wants_tray_launch(["dictflow", "--minimized"], false));
        assert!(!wants_tray_launch(["dictflow"], true));
    }

    #[test]
    fn recommended_matches_catalog_default() {
        assert_eq!(
            onboarding::recommended_model_id(),
            models::default_model_id()
        );
    }

    #[test]
    fn local_speech_uses_catalog_name() {
        let s = Settings::default();
        assert_eq!(active_speech_source(&s), "local");
        assert_eq!(active_speech_id(&s), models::default_model_id());
        assert_eq!(active_speech_name(&s), "Parakeet TDT 0.6B v3");
        assert_eq!(active_speech_engine_label(&s), "Parakeet");
    }

    #[test]
    fn online_speech_uses_api_model_name() {
        let s = Settings {
            audio_backend: "openai_compat".to_owned(),
            audio_api_base: "https://api.groq.com/openai/v1".to_owned(),
            audio_api_model: "whisper-large-v3-turbo".to_owned(),
            ..Settings::default()
        };
        assert_eq!(active_speech_source(&s), "online");
        assert_eq!(active_speech_id(&s), "whisper-large-v3-turbo");
        assert_eq!(active_speech_name(&s), "whisper-large-v3-turbo");
        assert_eq!(active_speech_engine_label(&s), "Groq");
        assert!(active_speech_ready(&s, Path::new(".")));
    }

    #[test]
    fn online_speech_empty_model_is_not_ready() {
        let s = Settings {
            audio_backend: "openai_compat".to_owned(),
            audio_api_model: "  ".to_owned(),
            ..Settings::default()
        };
        assert_eq!(active_speech_id(&s), "online model");
        assert!(!active_speech_ready(&s, Path::new(".")));
        assert_eq!(audio_host_label("https://api.openai.com/v1"), "OpenAI");
        assert_eq!(audio_host_label("http://127.0.0.1:8080/v1"), "Online");
    }

    #[test]
    fn cancel_does_not_touch_stats() {
        let mut s = test_state();
        s.push_history(NewHistory {
            text: "keep".into(),
            raw_text: "keep".into(),
            duration_secs: 1.0,
            model: "test".into(),
            audio_path: None,
            dict_hits: 0,
            cleanup: "full".into(),
        });
        assert_eq!(s.stats.lifetime_dictations, 1);
        assert_eq!(s.history.len(), 1);
        // No live recording — cancel_recording errors and must not wipe the ledger.
        assert!(cancel_recording(&mut s).is_err());
        assert_eq!(s.stats.lifetime_dictations, 1);
        assert_eq!(s.history.len(), 1);
        let _ = std::fs::remove_dir_all(&s.data_dir);
    }
}
