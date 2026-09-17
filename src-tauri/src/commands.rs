//! Tauri IPC commands shared by Windows and macOS.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager, State};

use crate::audio::{default_input_name, resolve_capture_device, start_recording, stop_and_save_wav};
use crate::devices;
use crate::dictation::{last_text, spawn_record_ticker};
use crate::engine::{polish_cfg, resolve_binary};
use     crate::hotkey::{
    apply_hotkey_registration, apply_utility_shortcuts, hotkey_vks, set_fn_mode,
    validate_utility_key,
};
use crate::models::{self, EngineKind};
use crate::onboarding;
use crate::overlay;
use crate::paste::{paste_target_for, paste_text, remember_paste_target};
use crate::polish;
use crate::settings::{
    active_speech_engine_label, active_speech_id, active_speech_name, active_speech_ready,
    active_speech_source, is_online_audio, normalize_talk_key, truncate, HistoryItem, ModelStatus,
    Settings, Status,
};
use crate::shortcuts;
use crate::state::AppState;
use crate::stats::UsageStats;
use crate::text;
use crate::tray::apply_overlay_visibility;

#[tauri::command]
pub(crate) fn get_status(state: State<'_, Mutex<AppState>>) -> Status {
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
pub(crate) fn get_models(state: State<'_, Mutex<AppState>>) -> Vec<ModelStatus> {
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
pub(crate) fn select_model(state: State<'_, Mutex<AppState>>, id: String) -> Result<String, String> {
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
pub(crate) async fn download_model(
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
pub(crate) fn cancel_download(state: State<'_, Mutex<AppState>>, id: String) -> Result<String, String> {
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
pub(crate) fn remove_audio_file(audio_path: &Option<String>) {
    if let Some(p) = audio_path {
        let path = Path::new(p);
        if path.is_file() {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[tauri::command]
pub(crate) fn get_history(state: State<'_, Mutex<AppState>>) -> Vec<HistoryItem> {
    state.lock().unwrap().history.clone()
}

#[tauri::command]
pub(crate) fn get_stats(state: State<'_, Mutex<AppState>>) -> UsageStats {
    state.lock().unwrap().stats.clone()
}

#[tauri::command]
pub(crate) fn delete_history_item(state: State<'_, Mutex<AppState>>, index: usize) -> Result<(), String> {
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
pub(crate) fn clear_history(state: State<'_, Mutex<AppState>>) {
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
pub(crate) fn read_audio_file(state: State<'_, Mutex<AppState>>, name: String) -> Result<Vec<u8>, String> {
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
pub(crate) fn get_dictionary(state: State<'_, Mutex<AppState>>) -> Vec<text::DictionaryEntry> {
    let s = state.lock().unwrap();
    let mut entries = text::load_dictionary(&s.data_dir);
    text::sort_dictionary(&mut entries);
    entries
}

/// Monotonic-ish unique id for dictionary entries (nanos since epoch).
pub(crate) fn new_id(prefix: &str) -> String {
    format!(
        "{prefix}{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

#[tauri::command]
pub(crate) fn add_dictionary_entry(
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
pub(crate) fn set_dictionary_enabled(
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
pub(crate) fn delete_dictionary_entry(state: State<'_, Mutex<AppState>>, id: String) -> Result<(), String> {
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
pub(crate) fn star_dictionary_entry(
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
pub(crate) fn add_correction(
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
pub(crate) fn add_symbol_presets(
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
pub(crate) fn export_dictionary(state: State<'_, Mutex<AppState>>, path: String) -> Result<String, String> {
    let s = state.lock().unwrap();
    let entries = text::load_dictionary(&s.data_dir);
    let bytes = serde_json::to_vec_pretty(&entries).map_err(|e| e.to_string())?;
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    Ok(format!("exported {} rules → {path}", entries.len()))
}

#[tauri::command]
pub(crate) fn import_dictionary(
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
pub(crate) struct UpdateInfo {
    current: String,
    latest: Option<String>,
    url: Option<String>,
    available: bool,
    configured: bool,
}

#[tauri::command]
pub(crate) async fn check_for_updates() -> Result<UpdateInfo, String> {
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
pub(crate) struct ProviderStatus {
    audio_key_masked: String,
    llm_key_masked: String,
    has_audio_key: bool,
    has_llm_key: bool,
    default_prompt: String,
    prompt_presets: HashMap<String, String>,
}

#[tauri::command]
pub(crate) fn get_provider_status(state: State<'_, Mutex<AppState>>) -> ProviderStatus {
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
pub(crate) fn set_provider_secrets(
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
pub(crate) fn test_llm(state: State<'_, Mutex<AppState>>) -> Result<String, String> {
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
pub(crate) async fn test_audio_api(app: tauri::AppHandle) -> Result<String, String> {
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
pub(crate) fn get_settings(state: State<'_, Mutex<AppState>>) -> Settings {
    state.lock().unwrap().settings.clone()
}

#[tauri::command]
pub(crate) fn set_settings(
    app: tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
    settings: Settings,
) -> Result<Settings, String> {
    if models::find(&settings.model_id).is_none() {
        return Err(format!("unknown model: {}", settings.model_id));
    }
    let mut settings = settings;
    settings.hotkey_key = normalize_talk_key(&settings.hotkey_key);
    if hotkey_vks(&settings.hotkey_key).is_none() && settings.hotkey_key != "CtrlAltSpace" {
        return Err(format!("unknown talk key: {}", settings.hotkey_key));
    }
    if !matches!(settings.recording_mode.as_str(), "hold" | "toggle" | "double_tap") {
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
    // Keep the Fn-key tap in the matching mode (Fn, Fn+Ctrl, or idle).
    set_fn_mode(&settings.hotkey_key);
    apply_hotkey_registration(&app);
    apply_utility_shortcuts(&app);
    apply_overlay_visibility(&app);
    Ok(settings)
}

#[tauri::command]
pub(crate) fn copy_last(state: State<'_, Mutex<AppState>>) -> Result<String, String> {
    let text = last_text(&state.lock().unwrap())
        .ok_or_else(|| "nothing to copy — dictate first".to_owned())?;
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    cb.set_text(text.clone()).map_err(|e| e.to_string())?;
    Ok(format!("copied: {}", truncate(&text, 80)))
}

#[tauri::command]
pub(crate) fn paste_last(app: tauri::AppHandle, state: State<'_, Mutex<AppState>>) -> Result<String, String> {
    remember_paste_target(&app);
    let (text, saved) = {
        let s = state.lock().unwrap();
        (
            last_text(&s).ok_or_else(|| "nothing to paste — dictate first".to_owned())?,
            s.paste_target,
        )
    };
    paste_text(&app, &text, paste_target_for(&app, saved)).map_err(|e| format!("{e:#}"))?;
    Ok(format!("pasted: {}", truncate(&text, 80)))
}

#[tauri::command]
pub(crate) fn snap_overlay(x: i32, y: i32, screen_w: i32, screen_h: i32) -> overlay::OverlayPose {
    overlay::snap_to_edge(x, y, screen_w, screen_h, overlay::PILL_W, overlay::PILL_H)
}

#[tauri::command]
pub(crate) fn set_overlay_pose(
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

#[tauri::command]
pub(crate) fn onboard_step(current: String, backward: bool) -> Result<String, String> {
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
pub(crate) fn copy_chip_open(pasted_unix_ms: u64) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    overlay::copy_chip_visible(pasted_unix_ms, now, overlay::COPY_CHIP_MS)
}

#[tauri::command]
pub(crate) fn delete_model(state: State<'_, Mutex<AppState>>, id: String) -> Result<String, String> {
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
