//! Persisted settings, history rows, and speech-source helpers.
//!
//! Talk-key *names* live here so a settings file copied between Windows and
//! macOS still loads. Platform-specific polling lives in [`crate::hotkey`].

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::models::{EngineKind, ModelEntry};
use crate::overlay;
use crate::shortcuts;

pub(crate) const HISTORY_LIMIT: usize = 100;

/// Map a stored talk key onto this OS so a settings file copied across
/// machines still picks a key that exists here. Windows Win-keys become Cmd
/// on macOS; Mac-only Fn combos fall back to Right Ctrl on Windows.
pub(crate) fn normalize_talk_key(key: &str) -> String {
    #[cfg(target_os = "macos")]
    {
        match key {
            "LeftWin" => "LeftCmd".to_owned(),
            "RightWin" => "RightCmd".to_owned(),
            "CtrlWin" => "CtrlCmd".to_owned(),
            "ScrollLock" => "Fn".to_owned(),
            other => other.to_owned(),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        match key {
            "Fn" | "FnCtrl" => "RightCtrl".to_owned(),
            "LeftCmd" => "LeftWin".to_owned(),
            "RightCmd" => "RightWin".to_owned(),
            "CtrlCmd" => "CtrlWin".to_owned(),
            other => other.to_owned(),
        }
    }
}

pub(crate) fn default_talk_key() -> String {
    #[cfg(target_os = "macos")]
    {
        "Fn".to_owned()
    }
    #[cfg(not(target_os = "macos"))]
    {
        "RightCtrl".to_owned()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Settings {
    pub model_id: String,
    /// BCP-47 code or "auto". Passed to whisper-cli `-l`; Parakeet v3 is
    /// multilingual without a language flag.
    pub language: String,
    pub auto_paste: bool,
    /// Cleanup level (Wispr Flow "Auto Cleanup" lite): "off" | "light"
    /// (filler words only) | "full" (fillers + punctuation tidy).
    #[serde(default = "default_cleanup")]
    pub cleanup: String,
    /// Whisper-only: translate to English (needs a non-"auto" language).
    #[serde(default)]
    pub translate: bool,
    /// Talk key. Windows default: RightCtrl. macOS default: Fn.
    /// Windows: LeftCtrl | RightCtrl | LeftAlt | RightAlt | LeftWin | RightWin | CtrlWin | ScrollLock | F9 | CtrlAltSpace.
    /// macOS: Fn | FnCtrl | LeftCtrl | RightCtrl | LeftAlt | RightAlt | LeftCmd | RightCmd | CtrlCmd | F9 | CtrlAltSpace.
    #[serde(default = "default_talk_key")]
    pub hotkey_key: String,
    /// "hold" (default, macOS-like: down starts, up stops) | "toggle".
    pub recording_mode: String,
    /// Preferred input device name. `None` = OS default.
    #[serde(default)]
    pub audio_device: Option<String>,
    #[serde(default = "default_true")]
    pub overlay_enabled: bool,
    #[serde(default = "default_overlay_edge")]
    pub overlay_edge: String,
    #[serde(default = "default_overlay_offset")]
    pub overlay_offset: i32,
    #[serde(default = "shortcuts::default_paste_last")]
    pub paste_last_key: String,
    #[serde(default = "shortcuts::default_copy_last")]
    pub copy_last_key: String,
    /// Missing field on old settings.json → already onboarded. Fresh Default → false.
    #[serde(default = "default_true")]
    pub onboarded: bool,
    /// Hard-stop the take at 20 minutes. Default off (warn only at 19).
    #[serde(default)]
    pub session_cap: bool,
    #[serde(default = "default_audio_backend")]
    pub audio_backend: String,
    #[serde(default)]
    pub audio_api_base: String,
    #[serde(default = "default_audio_model")]
    pub audio_api_model: String,
    #[serde(default = "default_llm_backend")]
    pub llm_backend: String,
    #[serde(default)]
    pub llm_api_base: String,
    #[serde(default = "default_llm_model")]
    pub llm_api_model: String,
    #[serde(default = "default_llm_temp")]
    pub llm_temperature: f32,
    #[serde(default = "default_llm_timeout")]
    pub llm_timeout_ms: u64,
    #[serde(default = "default_llm_preset")]
    pub llm_preset: String,
    #[serde(default)]
    pub llm_custom_prompt: String,
    #[serde(default)]
    pub llm_enabled: bool,
}

fn default_cleanup() -> String {
    "full".to_owned()
}

fn default_true() -> bool {
    true
}

pub(crate) fn default_overlay_edge() -> String {
    "bottom".to_owned()
}

pub(crate) fn default_overlay_offset() -> i32 {
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
            model_id: crate::models::default_model_id(),
            language: "auto".to_owned(),
            auto_paste: true,
            cleanup: default_cleanup(),
            translate: false,
            hotkey_key: default_talk_key(),
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
pub(crate) struct HistoryItem {
    pub text: String,
    pub date_unix: u64,
    pub duration_secs: f64,
    pub model: String,
    /// Per-dictation recording for playback. `#[serde(default)]` keeps
    /// pre-audio history files loadable.
    #[serde(default)]
    pub audio_path: Option<String>,
    /// Transcript before dictionary + cleanup. Empty on pre-P0 rows.
    #[serde(default)]
    pub raw_text: String,
    /// Cached polished word count. 0 means "compute from text" for old rows.
    #[serde(default)]
    pub words_out: u32,
    #[serde(default)]
    pub dict_hits: u32,
    #[serde(default)]
    pub cleanup: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Status {
    pub recording: bool,
    pub model_id: String,
    pub engine: EngineKind,
    pub engine_label: String,
    pub model_loaded: bool,
    pub whisper_binary: Option<String>,
    pub data_dir: String,
    /// Resolved capture device (preferred if still present, else OS default).
    pub audio_device: Option<String>,
    /// App version (Cargo package version, single source of truth).
    pub version: String,
    pub recommended_model: String,
    pub transcribing: bool,
    /// "local" or "online" — which STT source dictation actually uses.
    pub speech_source: String,
    /// Catalog id, or the API model name when the audio backend is online.
    pub speech_model: String,
    /// Human-readable name for the active speech source.
    pub speech_model_name: String,
    /// Ready to dictate with the selected source (downloaded, or API model set).
    pub speech_ready: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ModelStatus {
    #[serde(flatten)]
    pub entry: ModelEntry,
    pub downloaded: bool,
    pub active: bool,
    pub downloading: bool,
}

pub(crate) struct NewHistory {
    pub text: String,
    pub raw_text: String,
    pub duration_secs: f64,
    pub model: String,
    pub audio_path: Option<String>,
    pub dict_hits: u32,
    pub cleanup: String,
}

pub(crate) fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_owned();
    }
    format!("{}…", &s[..n])
}

pub(crate) fn is_online_audio(settings: &Settings) -> bool {
    settings.audio_backend == "openai_compat"
}

pub(crate) fn audio_host_label(base: &str) -> &'static str {
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
pub(crate) fn active_speech_id(settings: &Settings) -> String {
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

pub(crate) fn active_speech_name(settings: &Settings) -> String {
    if is_online_audio(settings) {
        active_speech_id(settings)
    } else {
        crate::models::find(&settings.model_id)
            .map(|m| m.name)
            .unwrap_or_else(|| settings.model_id.clone())
    }
}

pub(crate) fn active_speech_engine_label(settings: &Settings) -> String {
    if is_online_audio(settings) {
        audio_host_label(&settings.audio_api_base).to_owned()
    } else {
        crate::models::find(&settings.model_id)
            .map(|m| m.engine.label().to_owned())
            .unwrap_or_else(|| "Local".to_owned())
    }
}

pub(crate) fn active_speech_ready(settings: &Settings, data_dir: &Path) -> bool {
    if is_online_audio(settings) {
        !settings.audio_api_model.trim().is_empty()
    } else {
        crate::models::find(&settings.model_id).is_some_and(|m| m.is_downloaded(data_dir))
    }
}

pub(crate) fn active_speech_source(settings: &Settings) -> &'static str {
    if is_online_audio(settings) {
        "online"
    } else {
        "local"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn fresh_settings_are_not_onboarded() {
        assert!(!Settings::default().onboarded);
        assert_eq!(Settings::default().paste_last_key, "Shift+Alt+Z");
        assert_eq!(Settings::default().copy_last_key, "Shift+Alt+X");
        assert!(!Settings::default().session_cap);
        assert!(Settings::default().overlay_enabled);
        assert_eq!(Settings::default().hotkey_key, default_talk_key());
    }

    #[test]
    fn talk_key_normalizes_to_this_platform() {
        #[cfg(target_os = "macos")]
        {
            assert_eq!(normalize_talk_key("LeftWin"), "LeftCmd");
            assert_eq!(normalize_talk_key("RightWin"), "RightCmd");
            assert_eq!(normalize_talk_key("CtrlWin"), "CtrlCmd");
            assert_eq!(normalize_talk_key("ScrollLock"), "Fn");
            assert_eq!(normalize_talk_key("Fn"), "Fn");
            assert_eq!(normalize_talk_key("FnCtrl"), "FnCtrl");
            assert_eq!(normalize_talk_key("RightCtrl"), "RightCtrl");
            assert_eq!(default_talk_key(), "Fn");
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(normalize_talk_key("Fn"), "RightCtrl");
            assert_eq!(normalize_talk_key("FnCtrl"), "RightCtrl");
            assert_eq!(normalize_talk_key("LeftCmd"), "LeftWin");
            assert_eq!(normalize_talk_key("RightCmd"), "RightWin");
            assert_eq!(normalize_talk_key("CtrlCmd"), "CtrlWin");
            assert_eq!(normalize_talk_key("RightCtrl"), "RightCtrl");
            assert_eq!(default_talk_key(), "RightCtrl");
        }
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
    fn truncate_keeps_short_text() {
        assert_eq!(truncate("hi", 80), "hi");
    }

    #[test]
    fn truncate_clips_long_text() {
        let long = "x".repeat(200);
        assert_eq!(truncate(&long, 80), format!("{}…", &long[..80]));
    }

    #[test]
    fn local_speech_uses_catalog_name() {
        let s = Settings::default();
        assert_eq!(active_speech_source(&s), "local");
        assert_eq!(active_speech_id(&s), crate::models::default_model_id());
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
}
