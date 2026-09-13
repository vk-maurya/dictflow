//! Offline text post-processing + audio helpers.
//!
//! - Dictionary (trigger → replacement snippet/vocab rules)
//! - Filler-word cleanup
//! - Smart trailing punctuation (don't punctuate emails/URLs/numbers)
//!
//! Plus WAV loading and linear resampling to the 16 kHz mono both engines need.

use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::Path;

// ---------------------------------------------------------------------------
// Dictionary
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DictionaryEntry {
    pub id: String,
    pub trigger: String,
    pub replacement: String,
    pub enabled: bool,
    pub whole_word: bool,
    /// `vocab` (default) or `snippet`. Old files omit this.
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub starred: bool,
    #[serde(default)]
    pub usage: u32,
}

fn default_kind() -> String {
    "vocab".to_owned()
}

impl DictionaryEntry {
    pub fn new(id: impl Into<String>, trigger: impl Into<String>, replacement: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            trigger: trigger.into(),
            replacement: replacement.into(),
            enabled: true,
            whole_word: true,
            kind: default_kind(),
            starred: false,
            usage: 0,
        }
    }

    pub fn is_snippet(&self) -> bool {
        self.kind == "snippet"
    }
}

pub fn sort_dictionary(entries: &mut [DictionaryEntry]) {
    entries.sort_by(|a, b| {
        b.starred
            .cmp(&a.starred)
            .then(b.usage.cmp(&a.usage))
            .then(a.trigger.to_ascii_lowercase().cmp(&b.trigger.to_ascii_lowercase()))
    });
}

fn dictionary_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("dictionary.json")
}

pub fn load_dictionary(data_dir: &Path) -> Vec<DictionaryEntry> {
    std::fs::read(dictionary_path(data_dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save_dictionary(data_dir: &Path, entries: &[DictionaryEntry]) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(entries)?;
    std::fs::write(dictionary_path(data_dir), bytes)?;
    Ok(())
}

/// Whitespace-separated word count (matches the frontend `wordCount`).
pub fn word_count(text: &str) -> u32 {
    text.split_whitespace().filter(|t| !t.is_empty()).count() as u32
}

/// Apply every enabled rule. Word-boundary-aware, case-insensitive; spaces in
/// the trigger match any whitespace run (mirrors `DictionaryService.replace`).
/// Returns the rewritten text and how many replacements fired. Increments
/// `usage` on each entry that matched.
pub fn apply_dictionary(text: &str, entries: &mut [DictionaryEntry]) -> (String, u32) {
    let mut result = text.to_owned();
    let mut hits: u32 = 0;
    for entry in entries.iter_mut().filter(|e| e.enabled) {
        let trigger = entry.trigger.trim();
        if trigger.is_empty() {
            continue;
        }
        let escaped = regex::escape(trigger).replace(' ', r"\s+");
        let lead_boundary = entry.whole_word
            && trigger.chars().next().is_some_and(|c| c.is_alphanumeric());
        let trail_boundary = entry.whole_word
            && trigger.chars().last().is_some_and(|c| c.is_alphanumeric());
        let pattern = format!(
            "{}{}{}",
            if lead_boundary { r"\b" } else { "" },
            escaped,
            if trail_boundary { r"\b" } else { "" }
        );
        if let Ok(re) = Regex::new(&format!("(?i){pattern}")) {
            // Replacement must stay literal ($ and \ have meaning in templates).
            let template = entry
                .replacement
                .replace('\\', r"\\")
                .replace('$', "$$");
            let n = re.find_iter(&result).count() as u32;
            if n > 0 {
                hits += n;
                entry.usage = entry.usage.saturating_add(n);
                result = re.replace_all(&result, template).into_owned();
            }
        }
    }
    (result, hits)
}

// ---------------------------------------------------------------------------
// Filler-word removal
// ---------------------------------------------------------------------------

/// Drop filler words ("um", "uh", …). Token-based so no look-around is needed
/// (the `regex` crate doesn't support it); lone punctuation tokens survive for
/// the tidy pass.
pub fn remove_fillers(text: &str) -> String {
    // The `regex` crate has no look-around, so we filter whitespace-separated
    // tokens: a token whose punctuation-stripped core is a filler word is
    // dropped ("Um," → dropped; "hum" and "uh-huh" survive).
    let filler = Regex::new(r"(?i)^(uh+|um+|umm+|uhm+|erm+|hmm+)$")
        .expect("filler pattern compiles");
    text.split_whitespace()
        .filter(|tok| {
            let core = tok.trim_matches(|c: char| c.is_ascii_punctuation());
            core.is_empty() || !filler.is_match(core)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Attach stray spaces before punctuation. Preserve newlines (P2 lists /
/// spoken “new line”); collapse other whitespace runs.
pub fn tidy_punctuation(text: &str) -> String {
    let tidy = Regex::new(r"[^\S\n]+([,.;:!?])").expect("tidy pattern compiles");
    let tidy = tidy.replace_all(text, "$1");
    let spaces = Regex::new(r"[^\S\n]+").expect("spaces pattern compiles");
    let tidy = spaces.replace_all(&tidy, " ");
    let nls = Regex::new(r"\n{3,}").expect("nl collapse");
    nls.replace_all(&tidy, "\n\n").trim().to_owned()
}

// ---------------------------------------------------------------------------
// Smart trailing punctuation
// ---------------------------------------------------------------------------

fn matches(text: &str, pattern: &str) -> bool {
    Regex::new(pattern)
        .map(|re| re.is_match(text))
        .unwrap_or(false)
}

/// Strip a lone sentence-final period when the whole transcript is an
/// email, URL, number, or single token — recognizers punctuate everything.
pub fn smart_trailing_punctuation(text: &str) -> String {
    let trimmed = text.trim();
    if !trimmed.ends_with('.') || trimmed.ends_with("..") {
        return text.to_owned();
    }
    let candidate = &trimmed[..trimmed.len() - 1];
    if candidate.is_empty() {
        return text.to_owned();
    }
    let is_number = matches(candidate, r"(?i)^\+?\(?\d(?:[\d\s.,\-()/:]*\d)?$");
    let is_email = matches(candidate, r"(?i)^[^\s@]+@[^\s@]+\.[^\s@]+$");
    let is_url = matches(
        candidate,
        r"(?i)^(?:[a-z][a-z0-9+.\-]*://\S+|www\.\S+\.\S+|[a-z0-9\-]+(?:\.[a-z0-9\-]+)*\.[a-z]{2,}(?:[/:?#]\S*)?)$",
    );
    let is_token = !candidate.chars().any(|c| c.is_whitespace()) && !candidate.contains('.');
    if is_number || is_email || is_url || is_token {
        candidate.to_owned()
    } else {
        text.to_owned()
    }
}

// ---------------------------------------------------------------------------
// Audio: load any PCM WAV as mono 16 kHz f32
// ---------------------------------------------------------------------------

/// Read a WAV file and return mono samples resampled to 16 kHz.
pub fn load_wav_mono_16k(path: &Path) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let spec = reader.spec();
    let from_rate = spec.sample_rate;
    let channels = spec.channels.max(1) as usize;

    let interleaved: Vec<f32> = match (spec.bits_per_sample, spec.sample_format) {
        (16, hound::SampleFormat::Int) => reader
            .samples::<i16>()
            .collect::<Result<Vec<_>, _>>()
            .context("read i16 samples")?
            .into_iter()
            .map(|s| s as f32 / i16::MAX as f32)
            .collect(),
        (32, hound::SampleFormat::Float) => reader
            .samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .context("read f32 samples")?,
        (bits, fmt) => anyhow::bail!("unsupported WAV format: {bits}-bit {fmt:?}"),
    };

    // Downmix to mono by averaging channels.
    let mono: Vec<f32> = if channels == 1 {
        interleaved
    } else {
        interleaved
            .chunks_exact(channels)
            .map(|c| c.iter().sum::<f32>() / channels as f32)
            .collect()
    };

    Ok(resample_linear(&mono, from_rate, 16_000))
}

/// Linear-interpolation resampler for f32 mono audio.
pub fn resample_linear(samples: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate || samples.is_empty() {
        return samples.to_vec();
    }
    let ratio = from_rate as f64 / to_rate as f64;
    let out_len = ((samples.len() as f64 / ratio).ceil() as usize).max(1);
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * ratio;
            let lo = pos.floor() as usize;
            let hi = (lo + 1).min(samples.len() - 1);
            let frac = (pos - lo as f64) as f32;
            samples[lo] * (1.0 - frac) + samples[hi] * frac
        })
        .collect()
}

/// Write mono f32 samples as 16-bit PCM WAV.
pub fn write_wav_mono_16k(path: &Path, samples: &[f32], sample_rate: u32) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer =
        hound::WavWriter::create(path, spec).with_context(|| format!("create {}", path.display()))?;
    for s in samples {
        writer.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
    }
    writer.finalize()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_count_splits_on_whitespace() {
        assert_eq!(word_count("one two  three"), 3);
        assert_eq!(word_count("  "), 0);
    }

    #[test]
    fn dictionary_replaces_whole_words_case_insensitively() {
        let mut entries = vec![DictionaryEntry::new("1", "my email", "a@b.com")];
        assert_eq!(
            apply_dictionary("contact MY EMAIL please", &mut entries).0,
            "contact a@b.com please"
        );
        assert_eq!(entries[0].usage, 1);
        // Whole-word: no match inside longer words.
        let mut entries = vec![DictionaryEntry::new("1", "fig", "FigJam")];
        assert_eq!(apply_dictionary("a figment", &mut entries).0, "a figment");
        let mut entries = vec![DictionaryEntry::new("1", "my email", "a@b.com")];
        let (out, hits) = apply_dictionary("contact MY EMAIL please", &mut entries);
        assert_eq!(out, "contact a@b.com please");
        assert_eq!(hits, 1);
    }

    #[test]
    fn fillers_removed_tidy_kept_separate() {
        assert_eq!(remove_fillers("Um, let's go"), "let's go");
        assert_eq!(remove_fillers("well uh yeah"), "well yeah");
        assert_eq!(remove_fillers("the hum of uh-huh"), "the hum of uh-huh");
        assert_eq!(tidy_punctuation("hello , world !"), "hello, world!");
    }

    #[test]
    fn smart_punctuation_strips_dot_after_email() {
        assert_eq!(
            smart_trailing_punctuation("mail me at a@b.com."),
            "mail me at a@b.com."
        );
        assert_eq!(smart_trailing_punctuation("a@b.com."), "a@b.com");
        assert_eq!(smart_trailing_punctuation("Hello world."), "Hello world.");
    }

    #[test]
    fn old_dictionary_json_defaults_kind() {
        let e: DictionaryEntry = serde_json::from_str(
            r#"{"id":"1","trigger":"a","replacement":"b","enabled":true,"whole_word":true}"#,
        )
        .unwrap();
        assert_eq!(e.kind, "vocab");
        assert!(!e.starred);
        assert_eq!(e.usage, 0);
        assert!(!e.is_snippet());
        let mut snip = DictionaryEntry::new("2", "sign off", "Best regards");
        snip.kind = "snippet".into();
        assert!(snip.is_snippet());
    }

    #[test]
    fn sort_stars_then_usage() {
        let mut entries = vec![
            DictionaryEntry::new("1", "zeta", "z"),
            DictionaryEntry::new("2", "alpha", "a"),
        ];
        entries[0].usage = 3;
        entries[1].starred = true;
        sort_dictionary(&mut entries);
        assert_eq!(entries[0].id, "2");
        assert_eq!(entries[1].id, "1");
    }

    #[test]
    fn resample_passthrough_and_downsample() {
        let s = vec![0.0, 1.0, 0.0, -1.0];
        assert_eq!(resample_linear(&s, 16_000, 16_000), s);
        let down = resample_linear(&vec![0.0; 48_000], 48_000, 16_000);
        assert_eq!(down.len(), 16_000);
    }
}
