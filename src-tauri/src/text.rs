//! Offline text post-processing + audio helpers.
//!
//! Ports of SpeakType's pure-logic passes so behavior matches macOS:
//!
//! - `DictionaryService` (trigger → replacement snippet/vocab rules)
//! - `WhisperService` filler-word "Auto Edit"
//! - `SmartTrailingPunctuation` (don't punctuate emails/URLs/numbers)
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

/// Apply every enabled rule. Word-boundary-aware, case-insensitive; spaces in
/// the trigger match any whitespace run (mirrors `DictionaryService.replace`).
pub fn apply_dictionary(text: &str, entries: &[DictionaryEntry]) -> String {
    let mut result = text.to_owned();
    for entry in entries.iter().filter(|e| e.enabled) {
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
            result = re.replace_all(&result, template).into_owned();
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Auto Edit (filler-word removal, SpeakType `applyAutoEdit`)
// ---------------------------------------------------------------------------

pub fn auto_edit(text: &str) -> String {
    // The `regex` crate has no look-around, so instead of SpeakType's single
    // lookahead pattern we filter whitespace-separated tokens: a token whose
    // punctuation-stripped core is a filler word is dropped ("Um," → dropped,
    // "hum" and "uh-huh" survive, exactly like the original pattern).
    let filler = Regex::new(r"(?i)^(uh+|um+|umm+|uhm+|erm+|hmm+)$")
        .expect("filler pattern compiles");
    let kept: Vec<&str> = text
        .split_whitespace()
        .filter(|tok| {
            let core = tok.trim_matches(|c: char| c.is_ascii_punctuation());
            core.is_empty() || !filler.is_match(core)
        })
        .collect();
    let joined = kept.join(" ");

    let tidy = Regex::new(r"\s+([,.;:!?])").expect("tidy pattern compiles");
    let tidy = tidy.replace_all(&joined, "$1");
    let spaces = Regex::new(r"\s+").expect("spaces pattern compiles");
    spaces.replace_all(&tidy, " ").trim().to_owned()
}

// ---------------------------------------------------------------------------
// Smart trailing punctuation (SpeakType `SmartTrailingPunctuation`)
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
    fn dictionary_replaces_whole_words_case_insensitively() {
        let entries = vec![DictionaryEntry {
            id: "1".into(),
            trigger: "my email".into(),
            replacement: "a@b.com".into(),
            enabled: true,
            whole_word: true,
        }];
        assert_eq!(
            apply_dictionary("contact MY EMAIL please", &entries),
            "contact a@b.com please"
        );
        // Whole-word: no match inside longer words.
        let entries = vec![DictionaryEntry {
            id: "1".into(),
            trigger: "fig".into(),
            replacement: "FigJam".into(),
            enabled: true,
            whole_word: true,
        }];
        assert_eq!(apply_dictionary("a figment", &entries), "a figment");
    }

    #[test]
    fn auto_edit_removes_fillers() {
        assert_eq!(auto_edit("Um, let's go"), "let's go");
        assert_eq!(auto_edit("well uh yeah"), "well yeah");
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
    fn resample_passthrough_and_downsample() {
        let s = vec![0.0, 1.0, 0.0, -1.0];
        assert_eq!(resample_linear(&s, 16_000, 16_000), s);
        let down = resample_linear(&vec![0.0; 48_000], 48_000, 16_000);
        assert_eq!(down.len(), 16_000);
    }
}
