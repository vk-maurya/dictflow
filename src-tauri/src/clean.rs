//! Transcript cleanup (P0c): fillers, conversational repairs, destutter.
//!
//! Port of BetterWispr `TranscriptCleaner.swift` plus deviations:
//!
//! - the English gate is a V1 settings-only check (`language == "en"` or
//!   `"auto"`) with **no `whatlang` auto-detection** — that is a later task
//!   (P1d). Non-English input is returned `trim()`-only, untouched.
//! - the token IR keeps `word` + `trailing` (punctuation + whitespace) so
//!   re-joining a token list is lossless; punctuation is never dropped, only
//!   *carried* onto the previous surviving token.
//! - `drop_repairs` is a conservative edit-episode parser: every cue needs a
//!   repeated anchor (`last repeated word before the cue`) before anything is
//!   deleted, so the default is a no-op rather than a wrong deletion.
//! - `destutter` protects number words / year pairs and protects emphasis
//!   chains like `more and more and more` (`X and X and …`), while still
//!   collapsing `and then and then we`.
//!
//! Pure functions: `&str -> String`, no I/O, no clocks, no locks, no `unwrap`
//! on user text, saturating counters.

use regex::Regex;

/// One word plus the exact punctuation/whitespace that follows it. A synthetic
/// token with an empty `word` carries any leading text before the first word so
/// that `word + trailing` for every token rebuilds the input byte-for-byte.
#[derive(Clone, Debug)]
struct Token {
    word: String,
    trailing: String,
}

/// Standalone filler words, matched case-insensitively on the whole word.
const FILLERS: &[&str] = &[
    "uh", "um", "umm", "uhm", "er", "erm", "hmm", "hmmm", "mm", "mmm", "ah", "eh", "huh",
];

/// Words whose immediate double is meaningful and must survive (`very very`).
const KEPT_DOUBLES: &[&str] = &["very", "really", "so", "no"];

/// Coordination words that can form an emphasis chain (`more and more and more`).
const CONJUNCTIONS: &[&str] = &["and", "or", "but"];

/// A pronoun restart needs a two-word match, never a single word.
const PRONOUNS: &[&str] = &["i", "we", "they", "he", "she", "you"];

/// A lone `sorry` only restarts when the repair opens with a preposition.
const PREPOSITIONS: &[&str] = &["in", "at", "on", "from", "near", "into", "by", "with"];

/// Number words that protect a following `mm` (millimetres) and a repeated word
/// (`twenty twenty five` is a year pair, not a stutter).
const NUMBER_WORDS: &[&str] = &[
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
    "eleven", "twelve", "thirteen", "fourteen", "fifteen", "sixteen", "seventeen", "eighteen",
    "nineteen", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
    "hundred", "thousand", "million", "billion", "trillion", "dozen", "lakh", "lakhs", "crore",
    "crores", "point",
];

/// How far before/after a cue an anchor may sit (tokens).
const WINDOW: usize = 4;

fn lower(s: &str) -> String {
    s.to_lowercase()
}

fn is_number_like(word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    if word.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    NUMBER_WORDS.contains(&lower(word).as_str())
}

fn capitalize_first(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// True when `trailing` closes a sentence (so the following word is
/// sentence-initial and should be capitalized after a filler drop).
fn ends_sentence(trailing: &str) -> bool {
    trailing
        .trim_end()
        .chars()
        .last()
        .is_some_and(|c| matches!(c, '.' | '!' | '?' | '\n'))
}

/// Split on Unicode word characters, keeping every non-word byte as the
/// following/preceding token's `trailing`. Rebuilding = concat `word+trailing`.
fn tokenize(text: &str) -> Vec<Token> {
    let re = Regex::new(r"[\p{L}\p{M}\p{N}_'’-]+").expect("token pattern compiles");
    let mut toks: Vec<Token> = Vec::new();
    let mut last = 0usize;
    for m in re.find_iter(text) {
        if let Some(prev) = toks.last_mut() {
            prev.trailing = text[last..m.start()].to_owned();
        } else if m.start() > 0 {
            toks.push(Token {
                word: String::new(),
                trailing: text[..m.start()].to_owned(),
            });
        }
        toks.push(Token {
            word: text[m.start()..m.end()].to_owned(),
            trailing: String::new(),
        });
        last = m.end();
    }
    if let Some(prev) = toks.last_mut() {
        prev.trailing = text[last..].to_owned();
    } else if !text.is_empty() {
        toks.push(Token {
            word: String::new(),
            trailing: text.to_owned(),
        });
    }
    toks
}

/// Drop filler words, set-off `you know`, and fix up capitalization. A dropped
/// token's trailing punctuation is carried onto the previous surviving token.
fn drop_fillers(toks: Vec<Token>) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::with_capacity(toks.len());
    let mut capitalize_next = false;
    let mut i = 0;
    while i < toks.len() {
        let cur = lower(&toks[i].word);

        // "you know" is only an aside when comma-bounded on both sides.
        if cur == "you" && toks.get(i + 1).is_some_and(|t| lower(&t.word) == "know") {
            let prev_comma = out.last().is_some_and(|t| t.trailing.starts_with(','));
            if prev_comma && toks[i + 1].trailing.starts_with(',') {
                let trailing = toks[i + 1].trailing.clone();
                if let Some(last) = out.last_mut() {
                    last.trailing = trailing;
                }
                i += 2;
                continue;
            }
        }

        if FILLERS.contains(&cur.as_str()) {
            // `mm` after a number means millimetres: keep it.
            let keep_mm = cur == "mm"
                && out
                    .iter()
                    .rev()
                    .find(|t| !t.word.is_empty())
                    .is_some_and(|t| is_number_like(&t.word));
            if !keep_mm {
                let sentence_initial = match out.iter().rev().find(|t| !t.word.is_empty()) {
                    None => true,
                    Some(prev) => ends_sentence(&prev.trailing),
                };
                let trailing = toks[i].trailing.clone();
                if let Some(last) = out.last_mut() {
                    last.trailing = trailing;
                }
                if sentence_initial {
                    capitalize_next = true;
                }
                i += 1;
                continue;
            }
        }

        let mut tok = toks[i].clone();
        if capitalize_next && !tok.word.is_empty() {
            tok.word = capitalize_first(&tok.word);
            capitalize_next = false;
        }
        out.push(tok);
        i += 1;
    }
    out
}

/// Drop self-corrections (edit episodes). Deletes a contiguous range by
/// carrying the range's final trailing onto the token that precedes it.
fn drop_repairs(mut toks: Vec<Token>) -> Vec<Token> {
    let mut i = 0;
    while i < toks.len() {
        if let Some((a, b)) = repair_at(&toks, i) {
            if a >= 1 && b < toks.len() && a <= b {
                toks[a - 1].trailing = toks[b].trailing.clone();
                toks.drain(a..=b);
            }
        }
        i += 1;
    }
    toks
}

/// Decide whether a repair/edit episode starts at `i`; returns the inclusive
/// token range to delete, or `None` to leave everything untouched.
fn repair_at(toks: &[Token], i: usize) -> Option<(usize, usize)> {
    let w = lower(&toks[i].word);
    if w.is_empty() {
        return None;
    }
    let cue_len =
        if w == "i" && toks.get(i + 1).is_some_and(|t| lower(&t.word) == "mean") {
            2
        } else {
            1
        };
    let cue_end = i + cue_len;
    match w.as_str() {
        // Floor from the old `format.rs`: `X actually Y` -> `Y` unless X is a
        // pronoun (`I actually enjoyed it` stays).
        "actually" => {
            if i == 0 || cue_end >= toks.len() {
                return None;
            }
            let prev = lower(&toks[i - 1].word);
            if PRONOUNS.contains(&prev.as_str()) {
                return None;
            }
            Some((i - 1, i))
        }
        // `no` / `i mean` only act when comma-set-off.
        "no" => {
            if !toks[i].trailing.starts_with(',') {
                return None;
            }
            anchor_repair(toks, i, cue_end)
        }
        "i" => {
            if cue_len != 2 || !toks[i + 1].trailing.starts_with(',') {
                return None;
            }
            anchor_repair(toks, i, cue_end)
        }
        "sorry" | "wait" | "oops" => {
            if let Some(r) = anchor_repair(toks, i, cue_end) {
                return Some(r);
            }
            // A lone `sorry` may restart only when the repair opens with a
            // preposition; otherwise prefer the no-op.
            if w == "sorry"
                && toks
                    .get(cue_end)
                    .is_some_and(|t| PREPOSITIONS.contains(&lower(&t.word).as_str()))
            {
                return Some((i, cue_end - 1));
            }
            None
        }
        _ => None,
    }
}

/// Anchor = the last repeated word before the cue. Requires the text after the
/// cue to repeat that word; a pronoun anchor additionally needs a two-word
/// match. Returns the inclusive range `[anchor+1, repeat]` to delete.
fn anchor_repair(toks: &[Token], cue: usize, cue_end: usize) -> Option<(usize, usize)> {
    if cue == 0 {
        return None;
    }
    let start = cue.saturating_sub(WINDOW);
    let limit = cue_end.saturating_add(WINDOW).min(toks.len());
    for j in (start..cue).rev() {
        let jw = lower(&toks[j].word);
        if jw.is_empty() {
            continue;
        }
        let Some(m) = (cue_end..limit).find(|&m| lower(&toks[m].word) == jw) else {
            continue;
        };
        if PRONOUNS.contains(&jw.as_str()) {
            let two_word = j >= 1 && m >= 1 && lower(&toks[j - 1].word) == lower(&toks[m - 1].word);
            if !two_word {
                return None;
            }
        }
        return Some((j + 1, m));
    }
    None
}

/// Collapse immediate unpunctuated repeats of 1/2/3-grams.
fn destutter(mut toks: Vec<Token>) -> Vec<Token> {
    let mut changed = true;
    while changed {
        changed = false;
        'scan: for n in [3usize, 2, 1] {
            if toks.len() < 2 * n {
                continue;
            }
            for p in 0..=toks.len() - 2 * n {
                let q = p + n;
                if !unpunctuated(&toks, p, q + n - 1) {
                    continue;
                }
                if !words_equal(&toks, p, q, n) {
                    continue;
                }
                if protect_repeat(&toks, p, n) {
                    continue;
                }
                toks[q - 1].trailing = toks[q + n - 1].trailing.clone();
                toks.drain(q..q + n);
                changed = true;
                break 'scan;
            }
        }
    }
    toks
}

/// True when no token in `[from, to)` carries punctuation (a punctuation break
/// ends a possible repeat).
fn unpunctuated(toks: &[Token], from: usize, to: usize) -> bool {
    toks[from..to].iter().all(|t| t.trailing.trim().is_empty())
}

fn words_equal(toks: &[Token], p: usize, q: usize, n: usize) -> bool {
    (0..n).all(|k| lower(&toks[p + k].word) == lower(&toks[q + k].word))
}

/// Protect number words / year pairs, kept doubles, and emphasis chains.
fn protect_repeat(toks: &[Token], p: usize, n: usize) -> bool {
    let unit: Vec<String> = toks[p..p + n].iter().map(|t| lower(&t.word)).collect();
    if n == 1 && (KEPT_DOUBLES.contains(&unit[0].as_str()) || is_number_like(&toks[p].word)) {
        return true;
    }
    if unit.iter().any(|w| CONJUNCTIONS.contains(&w.as_str())) {
        let next_in = toks
            .get(p + 2 * n)
            .is_some_and(|t| unit.contains(&lower(&t.word)));
        let prev_in = p > 0 && unit.contains(&lower(&toks[p - 1].word));
        if next_in || prev_in {
            return true;
        }
    }
    false
}

/// V1 English gate: settings only, no `whatlang` (P1d). `spoken` is reserved for
/// the detector so the signature does not change later.
fn is_english(spoken: &str, language: &str) -> bool {
    let _ = spoken;
    let lang = language.trim().to_ascii_lowercase();
    lang == "en" || lang == "auto"
}

/// Full cleanup pass: gate → tokenize → fillers → repairs → destutter → rebuild.
pub fn clean(text: &str, language: &str) -> String {
    if !is_english(text, language) {
        return text.trim().to_owned();
    }
    let toks = tokenize(text);
    let toks = drop_fillers(toks);
    let toks = drop_repairs(toks);
    let toks = destutter(toks);
    let mut out = String::with_capacity(text.len());
    for tok in toks {
        out.push_str(&tok.word);
        out.push_str(&tok.trailing);
    }
    out.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleaner_table() {
        let cases: &[(&str, &str, &str)] = &[
            ("Um, let's go", "en", "Let's go"),
            ("well uh yeah", "en", "well yeah"),
            ("the hum of uh-huh", "en", "the hum of uh-huh"),
            ("hello, um, world", "en", "hello, world"),
            ("twelve mm", "en", "twelve mm"),
            ("It was, you know, fine", "en", "It was, fine"),
            ("you know what I mean", "en", "you know what I mean"),
            (
                "Should we go, no, we should stay",
                "en",
                "Should we go, no, we should stay",
            ),
            ("Drinking more and more and more", "en", "Drinking more and more and more"),
            ("and then and then we shipped it", "en", "and then we shipped it"),
            ("twenty twenty five", "en", "twenty twenty five"),
            (
                "I live in Delhi no sorry I live in Ahmedabad",
                "en",
                "I live in Ahmedabad",
            ),
            ("Um, hello world", "de", "Um, hello world"),
            ("hello world", "auto", "hello world"),
        ];
        for (input, lang, expected) in cases {
            assert_eq!(clean(input, lang), *expected, "clean({input:?}, {lang:?})");
        }
    }

    #[test]
    fn actually_floor_from_format_rs() {
        assert_eq!(clean("the meeting is at 5 actually 3", "en"), "the meeting is at 3");
        assert_eq!(clean("I actually enjoyed it", "en"), "I actually enjoyed it");
    }

    #[test]
    fn tokenize_rebuild_is_lossless() {
        for text in [
            "Um, let's go",
            "  leading & trailing!! ",
            "no words here",
            "",
            "a\tb\nc  d",
        ] {
            let rebuilt: String = tokenize(text)
                .iter()
                .map(|t| format!("{}{}", t.word, t.trailing))
                .collect();
            assert_eq!(rebuilt, text);
        }
    }

    #[test]
    fn never_panics_on_junk() {
        for text in [
            "",
            "   ",
            "!!!",
            "…—–",
            "🙂🙂🙂",
            "a\u{0301}\u{0301}",
            "''''",
            "'—'",
            "I I I I I",
            "you know you know you know",
            "no no no sorry sorry",
            "um uh er erm hmm",
            "\n\n\n",
            "🤖 twelve mm 🤖",
        ] {
            for lang in ["en", "auto", "de", "zz"] {
                // Must return, never panic, on any input.
                let _ = clean(text, lang);
            }
            // Non-English input is untouched apart from trimming.
            assert_eq!(clean(text, "de"), text.trim());
        }
    }
}
