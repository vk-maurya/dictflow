//! Local usage ledger (`stats.json`). Survives History clear-all.
//!
//! Lifetime counters + one bucket per local calendar day. Small enough that
//! JSON is the right store (see docs/WISPR_FLOW_GAP_ANALYSIS.md). Writes go
//! through a temp file then rename so a crash mid-save cannot truncate the
//! ledger.

use chrono::{Local, TimeZone};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DayBucket {
    #[serde(default)]
    pub dictations: u64,
    #[serde(default)]
    pub words: u64,
    #[serde(default)]
    pub audio_secs: f64,
    #[serde(default)]
    pub raw_words: u64,
    #[serde(default)]
    pub dict_hits: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct UsageStats {
    #[serde(default)]
    pub lifetime_dictations: u64,
    #[serde(default)]
    pub lifetime_words: u64,
    #[serde(default)]
    pub lifetime_audio_secs: f64,
    #[serde(default)]
    pub lifetime_raw_words: u64,
    #[serde(default)]
    pub lifetime_dict_hits: u64,
    #[serde(default)]
    pub by_day: BTreeMap<String, DayBucket>,
}

impl UsageStats {
    pub fn is_empty(&self) -> bool {
        self.lifetime_dictations == 0 && self.by_day.is_empty()
    }

    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join("stats.json");
        let Ok(bytes) = std::fs::read(&path) else {
            return Self::default();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    pub fn save(&self, data_dir: &Path) {
        let _ = std::fs::create_dir_all(data_dir);
        let path = data_dir.join("stats.json");
        if let Ok(bytes) = serde_json::to_vec_pretty(self) {
            let _ = write_atomic(&path, &bytes);
        }
    }

    /// Increment lifetime + today's (or the take's) day bucket.
    pub fn record(
        &mut self,
        unix: u64,
        words: u64,
        audio_secs: f64,
        raw_words: u64,
        dict_hits: u64,
    ) {
        self.lifetime_dictations += 1;
        self.lifetime_words += words;
        self.lifetime_audio_secs += audio_secs;
        self.lifetime_raw_words += raw_words;
        self.lifetime_dict_hits += dict_hits;
        if let Some(key) = day_key_unix(unix) {
            let b = self.by_day.entry(key).or_default();
            b.dictations += 1;
            b.words += words;
            b.audio_secs += audio_secs;
            b.raw_words += raw_words;
            b.dict_hits += dict_hits;
        }
    }
}

/// Local calendar `YYYY-MM-DD` for a unix timestamp. `None` if the stamp is
/// missing (v0.1 migrated history rows).
pub fn day_key_unix(unix: u64) -> Option<String> {
    if unix == 0 {
        return None;
    }
    Local
        .timestamp_opt(unix as i64, 0)
        .single()
        .map(|t| t.format("%Y-%m-%d").to_string())
}

#[cfg(test)]
fn day_key_now() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

/// Write via `<name>.json.tmp` then atomically replace.
/// On Windows `rename` cannot overwrite an existing file, so the destination
/// is removed first. On POSIX (macOS, Linux) `rename` is an atomic swap with
/// no removal needed — removing first would create a brief window where the
/// file disappears, which is a correctness bug.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    #[cfg(target_os = "windows")]
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_increments_lifetime_and_day() {
        let mut s = UsageStats::default();
        s.record(1_700_000_000, 40, 12.0, 48, 2);
        assert_eq!(s.lifetime_dictations, 1);
        assert_eq!(s.lifetime_words, 40);
        assert!((s.lifetime_audio_secs - 12.0).abs() < f64::EPSILON);
        assert_eq!(s.lifetime_raw_words, 48);
        assert_eq!(s.lifetime_dict_hits, 2);
        let key = day_key_unix(1_700_000_000).unwrap();
        let day = s.by_day.get(&key).expect("day bucket");
        assert_eq!(day.dictations, 1);
        assert_eq!(day.words, 40);
    }

    #[test]
    fn zero_unix_skips_by_day_keeps_lifetime() {
        let mut s = UsageStats::default();
        s.record(0, 10, 1.0, 10, 0);
        assert_eq!(s.lifetime_dictations, 1);
        assert!(s.by_day.is_empty());
    }

    #[test]
    fn save_roundtrip_and_survives_reload() {
        let dir = std::env::temp_dir().join(format!(
            "dictflow-stats-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let mut s = UsageStats::default();
        s.record(1_700_000_000, 5, 2.5, 6, 1);
        s.save(&dir);
        let loaded = UsageStats::load(&dir);
        assert_eq!(loaded.lifetime_dictations, 1);
        assert_eq!(loaded.lifetime_words, 5);
        assert_eq!(loaded.lifetime_dict_hits, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn day_key_now_looks_like_ymd() {
        let k = day_key_now();
        assert_eq!(k.len(), 10);
        assert_eq!(&k[4..5], "-");
        assert_eq!(&k[7..8], "-");
    }
}
