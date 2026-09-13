// DictFlow — offline voice dictation for Windows (Wispr Flow-style, 100% local).
// Stack: Tauri v2 (WebView2) + Rust backend + whisper.cpp sidecar (Whisper)
//        + in-process sherpa-onnx (Parakeet) + cpal (WASAPI) + SendInput paste.
//
// Pipeline (mirrors SpeakType): hotkey → mic capture → STT engine →
// dictionary snippets → auto-edit → smart punctuation → paste anywhere.

mod models;
mod stats;
mod text;

use std::collections::HashSet;
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
    /// modifier-combo cancel.
    Cancel,
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
            let mut last_change = std::time::Instant::now();
            let mut prev_keys = snapshot_keys();
            loop {
                std::thread::sleep(Duration::from_millis(10));
                let vk = app
                    .state::<Mutex<AppState>>()
                    .lock()
                    .map(|s| hotkey_vk(&s.settings.hotkey_key))
                    .unwrap_or(None);
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
    /// Talk key: "RightCtrl" (default) | "LeftCtrl" | "ScrollLock" | "F9" | "CtrlAltSpace".
    hotkey_key: String,
    /// "hold" (default, macOS-like: down starts, up stops) | "toggle".
    recording_mode: String,
}

fn default_cleanup() -> String {
    "full".to_owned()
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
    /// Name of the OS default input device we capture from (None = none).
    audio_device: Option<String>,
    /// App version (Cargo package version, single source of truth).
    version: String,
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
}

struct AppState {
    recording: bool,
    /// True while the current recording was started by the talk key (a key
    /// release must not stop a UI/mic-button-started recording in hold mode).
    hotkey_owned: bool,
    active: Option<ActiveRecording>,
    settings: Settings,
    history: Vec<HistoryItem>,
    /// Lifetime usage; never cleared with history.
    stats: UsageStats,
    downloading: HashSet<String>,
    data_dir: PathBuf,
    transcriber: Transcriber,
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

fn run_capture(
    ready_tx: mpsc::Sender<Result<u32, String>>,
    samples: Arc<Mutex<Vec<f32>>>,
    stop: Arc<AtomicBool>,
) {
    // NOTE: the stream MUST be returned out of this closure — if it is dropped
    // here, capture stops instantly and every recording comes back silent.
    let result = (|| -> anyhow::Result<(cpal::Stream, u32)> {
        let host = cpal::default_host();
        let device = host.default_input_device().context(
            "no input device found — connect a microphone and check Windows \
             Settings → Privacy & security → Microphone (allow desktop apps)",
        )?;
        let supported = device
            .default_input_config()
            .context("no default input config")?;

        let writer = samples.clone();
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => {
                let config: cpal::StreamConfig = supported.clone().into();
                device.build_input_stream(
                    &config,
                    move |data: &[f32], _| {
                        writer.lock().unwrap().extend_from_slice(data);
                    },
                    |err| log::warn!("audio stream error: {err}"),
                    None,
                )?
            }
            cpal::SampleFormat::I16 => {
                let config: cpal::StreamConfig = supported.clone().into();
                device.build_input_stream(
                    &config,
                    move |data: &[i16], _| {
                        let mut lock = writer.lock().unwrap();
                        lock.extend(data.iter().map(|s| *s as f32 / i16::MAX as f32));
                    },
                    |err| log::warn!("audio stream error: {err}"),
                    None,
                )?
            }
            cpal::SampleFormat::U16 => {
                let config: cpal::StreamConfig = supported.clone().into();
                device.build_input_stream(
                    &config,
                    move |data: &[u16], _| {
                        let mut lock = writer.lock().unwrap();
                        lock.extend(
                            data.iter()
                                .map(|s| (*s as f32 / u16::MAX as f32) * 2.0 - 1.0),
                        );
                    },
                    |err| log::warn!("audio stream error: {err}"),
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

fn start_recording(state: &mut AppState) -> anyhow::Result<()> {
    if state.recording {
        anyhow::bail!("already recording");
    }
    state.active = None; // defensive: drop any stale session

    let samples: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = mpsc::channel();

    let t_samples = samples.clone();
    let t_stop = stop.clone();
    let t_done = done.clone();
    std::thread::Builder::new()
        .name("dictflow-capture".to_owned())
        .spawn(move || {
            run_capture(ready_tx, t_samples, t_stop);
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
    });
    Ok(())
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
}

/// List all input devices. Unlike macOS (per-app mic prompt), Windows guards
/// the mic with a global privacy toggle — if this comes back empty, the
/// toggle (or a missing mic) is the cause.
#[tauri::command]
fn get_audio_devices() -> Result<Vec<AudioDeviceInfo>, String> {
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
        out.push(AudioDeviceInfo {
            is_default: Some(&name) == default_name.as_ref(),
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
    let host = cpal::default_host();
    let (device, sample_rate, channels) = host
        .default_input_device()
        .and_then(|d| {
            let name = d.name().ok()?;
            let cfg = d.default_input_config().ok()?;
            Some((name, cfg.sample_rate().0, cfg.channels()))
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
    if samples.iter().all(|s| s.abs() < 0.002) {
        anyhow::bail!("captured only silence — speak closer to the microphone");
    }
    let entry = models::find(&input.model_id).context("unknown model")?;
    let raw = match entry.engine {
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
    };
    let raw_text = raw.trim().to_owned();

    // SpeakType pipeline order: dictionary → cleanup → smart punctuation.
    let (mut out, dict_hits) = text::apply_dictionary(&raw_text, &input.dictionary);
    match input.cleanup.as_str() {
        "light" => out = text::remove_fillers(&out),
        "full" => out = text::tidy_punctuation(&text::remove_fillers(&out)),
        _ => {}
    }
    out = text::smart_trailing_punctuation(&out);
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

fn paste_text(text: &str) -> anyhow::Result<()> {
    let mut cb = arboard::Clipboard::new().context("open clipboard")?;
    // Snapshot the current *text* clipboard so it can be restored after
    // auto-paste (SpeakType `restoreClipboardAfterAutoPaste`, default on).
    // Limitation: non-text clipboard content (images, files) cannot be
    // snapshotted through arboard and is not preserved.
    let previous = cb.get_text().ok();
    cb.set_text(text.to_owned()).context("set clipboard")?;
    std::thread::sleep(Duration::from_millis(120));

    // Focus is still the previously-focused app (our window is not focused
    // when triggered via global hotkey), so Ctrl+V pastes "anywhere".
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
        engine_label: engine.label().to_owned(),
        model_loaded: models::find(&s.settings.model_id)
            .is_some_and(|m| m.is_downloaded(&s.data_dir)),
        whisper_binary: resolve_binary(&s.data_dir).map(|p| p.display().to_string()),
        data_dir: s.data_dir.display().to_string(),
        audio_device: default_input_name(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
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
                active: id == s.settings.model_id,
                downloading: s.downloading.contains(&id),
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
    let (data_dir, transcriber) = {
        let mut s = state.lock().unwrap();
        if !s.downloading.insert(id.clone()) {
            return Err("download already in progress".to_owned());
        }
        (s.data_dir.clone(), s.transcriber.clone())
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
    text::load_dictionary(&s.data_dir)
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
    whole_word: bool,
) -> Result<Vec<text::DictionaryEntry>, String> {
    let trigger = trigger.trim().to_owned();
    if trigger.is_empty() {
        return Err("say-what-you-hear (trigger) must not be empty".to_owned());
    }
    let s = state.lock().unwrap();
    let mut entries = text::load_dictionary(&s.data_dir);
    let id = new_id("d");
    entries.insert(
        0,
        text::DictionaryEntry {
            id,
            trigger,
            replacement,
            enabled: true,
            whole_word,
        },
    );
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
        if entries
            .iter()
            .any(|e| e.trigger.eq_ignore_ascii_case(trigger))
        {
            continue;
        }
        entries.insert(
            0,
            text::DictionaryEntry {
                id: new_id("d"),
                trigger: trigger.to_string(),
                replacement: replacement.to_string(),
                enabled: true,
                whole_word: true,
            },
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
async fn toggle_recording(
    app: tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
) -> Result<String, String> {
    if !state.lock().unwrap().recording {
        let mut s = state.lock().unwrap();
        start_recording(&mut s).map_err(|e| e.to_string())?;
        s.hotkey_owned = false;
        drop(s);
        let _ = app.emit("dictflow://recording", true);
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
    let (input, parakeet, duration_secs, model_id, audio, cleanup) = {
        let mut s = state.lock().unwrap();
        s.hotkey_owned = false;
        let (wav, duration_secs) = stop_and_save_wav(&mut s).map_err(|e| e.to_string())?;
        let model_id = s.settings.model_id.clone();
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
        };
        (input, s.transcriber.clone(), duration_secs, model_id, audio, cleanup)
    };
    let _ = app.emit("dictflow://recording", false);
    let _ = app.emit("dictflow://transcribing", true);

    // STT + post-processing can take seconds (model load, inference) — keep it
    // off the async runtime.
    let result = tokio::task::spawn_blocking(move || run_transcription(&input, &parakeet))
        .await
        .map_err(|e| format!("transcription task failed: {e}"))?
        .map_err(|e| format!("{e:#}"))?;
    let text = result.text.clone();

    let auto_paste = {
        let mut s = state.lock().unwrap();
        s.push_history(NewHistory {
            text: result.text,
            raw_text: result.raw_text,
            duration_secs,
            model: model_id,
            audio_path: Some(audio),
            dict_hits: result.dict_hits,
            cleanup,
        });
        s.settings.auto_paste
    };
    let _ = app.emit("dictflow://transcribing", false);
    let _ = app.emit("dictflow://history-updated", ());

    if !auto_paste {
        return Ok(format!("transcribed (auto-paste off): {}", truncate(&text, 160)));
    }
    // Pasting must not kill the transcription if the foreground app rejects keys.
    match paste_text(&text) {
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
        };
        (input, s.transcriber.clone())
    };
    let _ = app.emit("dictflow://transcribing", true);
    let result = tokio::task::spawn_blocking(move || run_transcription(&input, &parakeet))
        .await
        .map_err(|e| format!("transcription task failed: {e}"))?
        .map_err(|e| format!("{e:#}"));
    let _ = app.emit("dictflow://transcribing", false);
    let result = result?;
    {
        let mut s = state.lock().unwrap();
        let model = s.settings.model_id.clone();
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
    Ok(result.text)
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

fn build_tray(app: &tauri::AppHandle) -> anyhow::Result<()> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::TrayIconBuilder;

    let toggle = MenuItem::with_id(app, "toggle", "Start/Stop dictation", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "Open DictFlow", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&toggle, &settings, &quit])?;

    let icon =
        tauri::image::Image::from_bytes(include_bytes!("../icons/32x32.png"))?;

    TrayIconBuilder::new()
        .icon(icon)
        .menu(&menu)
        .tooltip("DictFlow — offline voice dictation")
        .on_menu_event(|app, event| match event.id.as_ref() {
            "quit" => app.exit(0),
            "settings" => {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
            "toggle" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let state: State<'_, Mutex<AppState>> = app.state();
                    let _ = toggle_recording(app.clone(), state).await;
                });
            }
            _ => {}
        })
        .build(app)?;
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
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
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
                    let expected = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Space);
                    if *shortcut == expected && event.state == ShortcutState::Pressed {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let state: State<'_, Mutex<AppState>> = app.state();
                            match toggle_recording(app.clone(), state).await {
                                Ok(msg) => log::info!("dictation: {msg}"),
                                Err(e) => log::error!("dictation error: {e}"),
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
                hotkey_owned: false,
                active: None,
                settings: Settings::default(),
                history: Vec::new(),
                stats: UsageStats::default(),
                downloading: HashSet::new(),
                data_dir,
                transcriber,
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
                                match start_recording(&mut guard) {
                                    Ok(()) => {
                                        guard.hotkey_owned = true;
                                        drop(guard);
                                        let _ = hk_app.emit("dictflow://recording", true);
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
                    }
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_models,
            select_model,
            download_model,
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
            check_for_updates,
            get_settings,
            set_settings,
            toggle_recording,
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
            hotkey_owned: false,
            active: None,
            settings: Settings::default(),
            history: Vec::new(),
            stats: UsageStats::default(),
            downloading: HashSet::new(),
            data_dir: dir,
            transcriber: Transcriber::spawn(std::env::temp_dir()),
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
}
