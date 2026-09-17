//! Process-wide app state: settings, history, live capture handles.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, atomic::AtomicBool};

use tauri_plugin_global_shortcut::Shortcut;

use crate::engine::Transcriber;
use crate::focus;
use crate::models;
use crate::settings::{
    default_overlay_edge, default_overlay_offset, normalize_talk_key, HistoryItem, NewHistory,
    Settings, HISTORY_LIMIT,
};
use crate::stats::UsageStats;
use crate::text;

/// Handles for the live capture thread (owns the `!Send` cpal stream).
pub(crate) struct ActiveRecording {
    pub samples: Arc<Mutex<Vec<f32>>>,
    pub sample_rate: u32,
    pub stop: Arc<AtomicBool>,
    pub done: Arc<AtomicBool>,
    pub started_at: std::time::Instant,
    pub stream_error: Arc<AtomicBool>,
}

pub(crate) struct AppState {
    pub recording: bool,
    pub transcribing: bool,
    /// True while the current recording was started by the talk key (a key
    /// release must not stop a UI/mic-button-started recording in hold mode).
    pub hotkey_owned: bool,
    pub active: Option<ActiveRecording>,
    pub settings: Settings,
    pub history: Vec<HistoryItem>,
    /// Lifetime usage; never cleared with history.
    pub stats: UsageStats,
    pub downloading: HashMap<String, Arc<AtomicBool>>,
    /// Currently registered paste-last / copy-last shortcuts (so we can swap).
    pub utility_shortcuts: Vec<Shortcut>,
    pub data_dir: PathBuf,
    pub transcriber: Transcriber,
    /// Last non-DictFlow foreground focus handle (HWND on Windows, PID on macOS).
    /// Overlay clicks must not steal the caret; we restore this before paste.
    pub paste_target: Option<focus::FocusHandle>,
}

impl AppState {
    pub(crate) fn settings_path(&self) -> PathBuf {
        self.data_dir.join("settings.json")
    }
    fn history_path(&self) -> PathBuf {
        self.data_dir.join("history.json")
    }

    pub(crate) fn load_settings(&mut self) {
        if let Ok(bytes) = std::fs::read(self.settings_path()) {
            if let Ok(s) = serde_json::from_slice::<Settings>(&bytes) {
                if models::find(&s.model_id).is_some() {
                    self.settings = s;
                    let native = normalize_talk_key(&self.settings.hotkey_key);
                    if native != self.settings.hotkey_key {
                        self.settings.hotkey_key = native;
                        self.save_settings();
                    }
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

    pub(crate) fn save_settings(&self) {
        let _ = std::fs::create_dir_all(&self.data_dir);
        if let Ok(bytes) = serde_json::to_vec_pretty(&self.settings) {
            let _ = std::fs::write(self.settings_path(), bytes);
        }
    }

    pub(crate) fn load_history(&mut self) {
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

    pub(crate) fn save_history(&self) {
        let _ = std::fs::create_dir_all(&self.data_dir);
        if let Ok(bytes) = serde_json::to_vec_pretty(&self.history) {
            let _ = std::fs::write(self.history_path(), bytes);
        }
    }

    pub(crate) fn push_history(&mut self, item: NewHistory) {
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
    pub(crate) fn seed_stats_from_history(&mut self) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{HistoryItem, NewHistory, HISTORY_LIMIT};

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
    fn history_is_capped_at_limit() {
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
        assert_eq!(
            s.history.first().unwrap().text,
            format!("item {}", HISTORY_LIMIT + 9)
        );
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
