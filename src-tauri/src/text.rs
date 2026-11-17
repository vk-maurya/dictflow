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
use std::collections::HashMap;
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

/// True when a replacement is a spoken-command phrase (`question mark`, `at the
/// rate`, …), i.e. the corrected text must be fed through the command engine
/// rather than inserted literally. Delegates to the command table.
pub fn is_command_replacement(replacement: &str) -> bool {
    crate::format::is_spoken_command(replacement)
}

/// Pre-command pass (BetterWispr `correctedCommands`). Identical machinery to
/// the vocabulary splice, but only entries whose replacement is a
/// spoken-command phrase (see [`is_command_replacement`]) fire, and `usage` is
/// never touched (command fixes are an expansion, not a usage event).
pub fn apply_command_fixes(text: &str, entries: &[DictionaryEntry]) -> (String, u32) {
    let (out, hits, _) = run_vocabulary(text, entries, |e| is_command_replacement(&e.replacement));
    (out, hits)
}

/// One compiled alternation drives the command-fix splice. Returns the
/// rewritten text, the number of real replacements, and the per-entry hit
/// counts (kept for symmetry with the vocabulary pass; command fixes discard
/// them).
fn run_vocabulary(
    text: &str,
    entries: &[DictionaryEntry],
    include: impl Fn(&DictionaryEntry) -> bool,
) -> (String, u32, Vec<(usize, u32)>) {
    // Enabled + non-empty + pass-filtered triggers, longest first so the
    // leftmost-first alternation prefers `my email address` over `my email`.
    let mut cands: Vec<usize> = (0..entries.len())
        .filter(|&i| {
            let e = &entries[i];
            e.enabled && include(e) && !e.trigger.trim().is_empty()
        })
        .collect();
    cands.sort_by(|&a, &b| {
        let la = entries[a].trigger.trim().chars().count();
        let lb = entries[b].trigger.trim().chars().count();
        lb.cmp(&la)
    });
    if cands.is_empty() {
        return (text.to_owned(), 0, Vec::new());
    }

    let mut alts = Vec::with_capacity(cands.len());
    let mut lookup: HashMap<String, (usize, bool)> = HashMap::new();
    for &i in &cands {
        let trigger = entries[i].trigger.trim();
        // Space in a trigger matches a whitespace run; everything else literal.
        alts.push(regex::escape(trigger).replace(' ', r"\s+"));
        lookup
            .entry(normalize_key(trigger))
            .or_insert((i, entries[i].whole_word));
    }
    let Ok(re) = Regex::new(&format!("(?i)(?:{})", alts.join("|"))) else {
        return (text.to_owned(), 0, Vec::new());
    };

    // The `regex` crate has no look-around, so whole-word matches are verified
    // by inspecting the characters flanking the match after the fact.
    let mut found: Vec<(usize, usize, usize)> = Vec::new();
    for m in re.find_iter(text) {
        let (start, end) = (m.start(), m.end());
        let Some(&(idx, whole_word)) = lookup.get(&normalize_key(m.as_str())) else {
            continue;
        };
        if whole_word && !boundaries_ok(text, start, end) {
            continue;
        }
        found.push((start, end, idx));
    }

    // Splice in reverse index order so earlier byte ranges stay valid, and never
    // through regex templates (they would interpret `$` and `\`).
    let mut result = text.to_owned();
    let mut hits: u32 = 0;
    let mut deltas: HashMap<usize, u32> = HashMap::new();
    for &(start, end, idx) in found.iter().rev() {
        let replacement = entries[idx].replacement.as_str();
        if replacement == &text[start..end] {
            continue;
        }
        hits = hits.saturating_add(1);
        let count = deltas.entry(idx).or_insert(0);
        *count = count.saturating_add(1);
        result.replace_range(start..end, replacement);
    }

    let mut per_entry: Vec<(usize, u32)> = deltas.into_iter().collect();
    per_entry.sort_unstable_by_key(|&(idx, _)| idx);
    (result, hits, per_entry)
}

/// Lowercase + collapse whitespace runs to one space, so a trigger and the text
/// it matched (which may contain a `\s+` run) map to the same lookup key.
fn normalize_key(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            in_ws = false;
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// Unicode word character (`[\p{L}\p{M}\p{N}_]` approximation).
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// A whole-word match must not be flanked by a word character. `whole_word`
/// entries whose first/last character is itself non-word (e.g. `c++`) still get
/// both flanks checked, per the two-phase spec.
fn boundaries_ok(text: &str, start: usize, end: usize) -> bool {
    let before = text[..start].chars().next_back();
    let after = text[end..].chars().next();
    !before.is_some_and(is_word_char) && !after.is_some_and(is_word_char)
}

/// Drop filler words ("um", "uh", …). Token-based so no look-around is needed
/// (the `regex` crate doesn't support it); lone punctuation tokens survive for
/// the tidy pass.
pub fn remove_fillers(text: &str) -> String {
    // The `regex` crate has no look-around, so instead of SpeakType's single
    // lookahead pattern we filter whitespace-separated tokens: a token whose
    // punctuation-stripped core is a filler word is dropped ("Um," → dropped,
    // "hum" and "uh-huh" survive, exactly like the original pattern).
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

/// Encode mono f32 samples as an in-memory 16-bit PCM WAV (for upload APIs).
pub fn wav_bytes_mono_16k(samples: &[f32], sample_rate: u32) -> Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = std::io::Cursor::new(Vec::with_capacity(44 + samples.len() * 2));
    {
        let mut writer = hound::WavWriter::new(&mut cursor, spec).context("start wav in memory")?;
        for s in samples {
            writer.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
        }
        writer.finalize()?;
    }
    Ok(cursor.into_inner())
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

    #[test]
    fn wav_bytes_round_trip() {
        let samples: Vec<f32> = (0..1_600)
            .map(|i| ((i as f32) * 0.05).sin() * 0.5)
            .collect();
        let bytes = wav_bytes_mono_16k(&samples, 16_000).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        let mut reader = hound::WavReader::new(std::io::Cursor::new(bytes)).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 16_000);
        let back: Vec<f32> = reader
            .samples::<i16>()
            .map(|s| s.unwrap() as f32 / i16::MAX as f32)
            .collect();
        assert_eq!(back.len(), samples.len());
        for (a, b) in samples.iter().zip(&back) {
            assert!((a - b).abs() < 1e-3);
        }
    }

    #[test]
    fn wav_bytes_clamps_and_handles_empty() {
        let bytes = wav_bytes_mono_16k(&[2.0, -2.0], 16_000).unwrap();
        let mut reader = hound::WavReader::new(std::io::Cursor::new(bytes)).unwrap();
        let v: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap()).collect();
        assert_eq!(v, vec![i16::MAX, -i16::MAX]);
        assert!(wav_bytes_mono_16k(&[], 16_000).is_ok());
    }
}
