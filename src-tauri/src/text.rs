//! Offline text post-processing + audio helpers.
//!
//! Ports of SpeakType's pure-logic passes so behavior matches macOS:
//!
//! - `DictionaryService` (trigger → replacement snippet/vocab rules)
//! - `WhisperService` filler-word "Auto Edit"
//! - `SmartTrailingPunctuation` (don't punctuate emails/URLs/numbers)
//!
//! Plus WAV loading and linear resampling to the 16 kHz mono both engines need.

// Port of BetterWispr VocabularyProcessor.swift plus deviations: manual Unicode
// boundary checks in the match loop instead of regex look-around, and a single
// alternation compiled over every trigger instead of one regex per entry.

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

/// Thin wrapper kept for existing call sites (`engine.rs`). Now delegates to the
/// single-regex two-phase engine, so semantics are: longest-trigger-first, one
/// pass (no cascade), Unicode-aware word boundaries, case-insensitive, spaces
/// matching any whitespace run, and literal replacements.
pub fn apply_dictionary(text: &str, entries: &mut [DictionaryEntry]) -> (String, u32) {
    apply_vocabulary(text, entries)
}

/// True when a replacement is command-shaped — empty, or made up only of
/// punctuation/symbols/whitespace (`?`, `%`, `\n`, `#`, `@`, `.`) — rather than
/// a spoken word such as `FigJam`.
// Wired into the two-phase engine order in a separate change; public API (P0d).
#[allow(dead_code)]
pub fn is_command_replacement(replacement: &str) -> bool {
    // TODO(P0b): delegate to crate::format::is_spoken_command
    replacement.chars().all(|c| !c.is_alphanumeric())
}

/// Post-cleanup vocabulary pass. Every enabled, non-empty trigger compiles into
/// a single `(?i)(?:alt|alt|…)` alternation, longest trigger first, and every
/// match is spliced in by hand. Counts a hit only when the replacement actually
/// differs from the matched text, incrementing `usage` (saturating) per entry.
pub fn apply_vocabulary(text: &str, entries: &mut [DictionaryEntry]) -> (String, u32) {
    let (out, hits, deltas) = run_vocabulary(text, entries, |_| true);
    for (idx, count) in deltas {
        entries[idx].usage = entries[idx].usage.saturating_add(count);
    }
    (out, hits)
}

/// Pre-command pass (BetterWispr `correctedCommands`). Identical machinery, but
/// only entries whose replacement is command-shaped (see
/// [`is_command_replacement`]) fire, and `usage` is never touched.
// Wired into the two-phase engine order in a separate change; public API (P0d).
#[allow(dead_code)]
pub fn apply_command_fixes(text: &str, entries: &[DictionaryEntry]) -> (String, u32) {
    let (out, hits, _) = run_vocabulary(text, entries, |e| is_command_replacement(&e.replacement));
    (out, hits)
}

/// One compiled alternation drives both vocabulary passes. Returns the rewritten
/// text, the number of real replacements, and the per-entry hit counts.
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

// ---------------------------------------------------------------------------
// Auto Edit (filler-word removal, SpeakType `applyAutoEdit`)
// ---------------------------------------------------------------------------

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
    fn vocabulary_unicode_word_boundaries() {
        // `é` is a Unicode letter: the whole-word trigger `café` fires, but not
        // inside `cafés` (the trailing `s` is a neighbour word character).
        let mut e = vec![DictionaryEntry::new("1", "café", "coffee")];
        assert_eq!(apply_vocabulary("a café please", &mut e).0, "a coffee please");
        assert_eq!(e[0].usage, 1);
        let mut e = vec![DictionaryEntry::new("1", "café", "coffee")];
        assert_eq!(apply_vocabulary("a cafés please", &mut e).0, "a cafés please");
        assert_eq!(e[0].usage, 0);

        // Documented decision: `cafe` is not a substring of `café` at all, so
        // the meaningful non-ASCII case is a shorter ASCII trigger like `caf`.
        // An ASCII `\b` would have matched here (é counts as non-word), but the
        // Unicode-aware boundary treats `é` as a word character and skips it.
        let mut e = vec![DictionaryEntry::new("1", "caf", "coffee")];
        assert_eq!(apply_vocabulary("a café", &mut e).0, "a café");
        assert_eq!(e[0].usage, 0);
        let mut e = vec![DictionaryEntry::new("1", "caf", "coffee")];
        assert_eq!(apply_vocabulary("a caf please", &mut e).0, "a coffee please");
    }

    #[test]
    fn vocabulary_longest_trigger_wins() {
        let mut e = vec![
            DictionaryEntry::new("1", "my email", "a@b.com"),
            DictionaryEntry::new("2", "my email address", "B@C.com"),
        ];
        let (out, hits) = apply_vocabulary("my email address", &mut e);
        assert_eq!(out, "B@C.com");
        assert_eq!(hits, 1);
        assert_eq!(e[1].usage, 1);
        assert_eq!(e[0].usage, 0);
    }

    #[test]
    fn vocabulary_empty_replacement_deletes_and_counts() {
        let mut e = vec![DictionaryEntry::new("1", "delete me", "")];
        let (out, hits) = apply_vocabulary("please delete me now", &mut e);
        assert_eq!(out, "please  now");
        assert_eq!(hits, 1);
        assert_eq!(e[0].usage, 1);
    }

    #[test]
    fn vocabulary_replacement_dollar_and_backslash_are_literal() {
        let mut e = vec![
            DictionaryEntry::new("1", "dollar", "$1"),
            DictionaryEntry::new("2", "backslash", r"\n"),
        ];
        let (out, hits) = apply_vocabulary("dollar and backslash", &mut e);
        assert_eq!(out, "$1 and \\n");
        assert_eq!(hits, 2);
    }

    #[test]
    fn vocabulary_regex_metacharacters_trigger_is_literal() {
        let mut e = vec![DictionaryEntry::new("1", "c++", "cpp")];
        let (out, hits) = apply_vocabulary("I love c++ today", &mut e);
        assert_eq!(out, "I love cpp today");
        assert_eq!(hits, 1);
        // Still whole-word: no match glued to a longer word.
        let mut e = vec![DictionaryEntry::new("1", "c++", "cpp")];
        assert_eq!(apply_vocabulary("xc++", &mut e).0, "xc++");
    }

    #[test]
    fn vocabulary_case_insensitive() {
        let mut e = vec![DictionaryEntry::new("1", "teh", "the")];
        let (out, hits) = apply_vocabulary("TEH cat and Teh dog", &mut e);
        assert_eq!(out, "the cat and the dog");
        assert_eq!(hits, 2);
        assert_eq!(e[0].usage, 2);
    }

    #[test]
    fn vocabulary_whole_word_false_matches_inside_word() {
        let mut entry = DictionaryEntry::new("1", "fig", "FigJam");
        entry.whole_word = false;
        let mut e = vec![entry];
        let (out, hits) = apply_vocabulary("a figment", &mut e);
        assert_eq!(out, "a FigJamment");
        assert_eq!(hits, 1);
    }

    #[test]
    fn vocabulary_hits_and_usage_saturate() {
        // A replacement equal to the matched text is not a hit.
        let mut e = vec![DictionaryEntry::new("1", "same", "same")];
        let (out, hits) = apply_vocabulary("same same", &mut e);
        assert_eq!(out, "same same");
        assert_eq!(hits, 0);
        assert_eq!(e[0].usage, 0);

        // One hit per real replacement.
        let mut e = vec![DictionaryEntry::new("1", "cat", "dog")];
        let (out, hits) = apply_vocabulary("cat cat cat", &mut e);
        assert_eq!(out, "dog dog dog");
        assert_eq!(hits, 3);
        assert_eq!(e[0].usage, 3);

        // usage saturates instead of overflowing.
        let mut e = vec![DictionaryEntry::new("1", "cat", "dog")];
        e[0].usage = u32::MAX;
        let (_, hits) = apply_vocabulary("cat", &mut e);
        assert_eq!(hits, 1);
        assert_eq!(e[0].usage, u32::MAX);
    }

    #[test]
    fn command_fixes_only_command_replacements_and_skip_usage() {
        assert!(is_command_replacement("%"));
        assert!(is_command_replacement(""));
        assert!(is_command_replacement("\n"));
        assert!(is_command_replacement("#"));
        assert!(is_command_replacement("@."));
        assert!(!is_command_replacement("FigJam"));
        assert!(!is_command_replacement("a@b.com"));

        let entries = vec![
            DictionaryEntry::new("1", "percent", "%"),
            DictionaryEntry::new("2", "fig", "FigJam"),
        ];
        let (out, hits) = apply_command_fixes("50 percent fig", &entries);
        assert_eq!(out, "50 % fig");
        assert_eq!(hits, 1);
        // `&[DictionaryEntry]`: usage is untouched by the command pass.
        assert_eq!(entries[0].usage, 0);
        assert_eq!(entries[1].usage, 0);
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
