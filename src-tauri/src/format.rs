//! Offline spoken-command, list, and backtrack passes (P2). No LLM.
//!
//! Port of BetterWispr `VoiceCommands.swift` plus deviations:
//!
//! - token-walk engine (like `text.rs`) instead of one left-to-right regex
//!   alternation: the `regex` crate has no look-around, so context rules
//!   (noun-marker guard, `@handle`/`#number` attachment, `%` on a preceding
//!   number, domain join) are decided by scanning whitespace-separated tokens;
//! - deliberate deviation: there is NO capitalization pass after `. ! ?`
//!   sentence boundaries (BetterWispr re-capitalizes). Adding one would break
//!   the existing tests and is out of P0 scope.

use regex::Regex;

/// How a matched command phrase is emitted.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Behavior {
    /// Attach to the previous token with no space (`. , ! ? ; : … ) ]`).
    Punct,
    /// Emit with normal spacing (openers, dashes, standalone symbols).
    SpacePunct,
    /// `comma` / `period`: blocked by a preceding noun-marker.
    Requested,
    /// `new line` / `new paragraph` … -> newline(s).
    Break,
    /// `at the rate` / `at sign` -> `@` attached to the next word / initials.
    AtHandle,
    /// `hash` / `hashtag` / `pound sign` / `number sign` -> `#` + next number.
    HashNumber,
    /// `dollar` / `dollar sign` -> `$` + next number.
    DollarNumber,
    /// `percent` / `percent sign` -> `%` on a preceding number.
    PercentNumber,
}

/// One spoken-command entry. `words` is the normalized (lowercase) phrase.
struct Entry {
    words: &'static [&'static str],
    repl: &'static str,
    behavior: Behavior,
}

/// All spoken commands. Longest match wins (chosen at match time), so order
/// here is only for readability.
static ENTRIES: &[Entry] = &[
    // alwaysPunctuation — breaks.
    Entry { words: &["new", "paragraph"], repl: "\n\n", behavior: Behavior::Break },
    Entry { words: &["next", "paragraph"], repl: "\n\n", behavior: Behavior::Break },
    Entry { words: &["new", "line"], repl: "\n", behavior: Behavior::Break },
    Entry { words: &["next", "line"], repl: "\n", behavior: Behavior::Break },
    Entry { words: &["newline"], repl: "\n", behavior: Behavior::Break },
    Entry { words: &["line", "break"], repl: "\n", behavior: Behavior::Break },
    // alwaysPunctuation — punctuation.
    Entry { words: &["question", "mark"], repl: "?", behavior: Behavior::Punct },
    Entry { words: &["exclamation", "mark"], repl: "!", behavior: Behavior::Punct },
    Entry { words: &["exclamation", "point"], repl: "!", behavior: Behavior::Punct },
    Entry { words: &["full", "stop"], repl: ".", behavior: Behavior::Punct },
    Entry { words: &["semi", "colon"], repl: ";", behavior: Behavior::Punct },
    Entry { words: &["semicolon"], repl: ";", behavior: Behavior::Punct },
    Entry { words: &["colon"], repl: ":", behavior: Behavior::Punct },
    Entry { words: &["ellipsis"], repl: "…", behavior: Behavior::Punct },
    Entry { words: &["dot", "dot", "dot"], repl: "…", behavior: Behavior::Punct },
    Entry { words: &["em", "dash"], repl: "—", behavior: Behavior::SpacePunct },
    Entry { words: &["hyphen"], repl: "-", behavior: Behavior::SpacePunct },
    Entry { words: &["dash"], repl: "-", behavior: Behavior::SpacePunct },
    Entry { words: &["open", "parenthesis"], repl: "(", behavior: Behavior::SpacePunct },
    Entry { words: &["close", "parenthesis"], repl: ")", behavior: Behavior::Punct },
    Entry { words: &["open", "bracket"], repl: "[", behavior: Behavior::SpacePunct },
    Entry { words: &["close", "bracket"], repl: "]", behavior: Behavior::Punct },
    Entry { words: &["open", "quote"], repl: "\"", behavior: Behavior::SpacePunct },
    Entry { words: &["close", "quote"], repl: "\"", behavior: Behavior::Punct },
    Entry { words: &["unquote"], repl: "\"", behavior: Behavior::Punct },
    Entry { words: &["quote"], repl: "\"", behavior: Behavior::SpacePunct },
    Entry { words: &["asterisk"], repl: "*", behavior: Behavior::SpacePunct },
    Entry { words: &["ampersand"], repl: "&", behavior: Behavior::SpacePunct },
    Entry { words: &["underscore"], repl: "_", behavior: Behavior::SpacePunct },
    Entry { words: &["backslash"], repl: "\\", behavior: Behavior::SpacePunct },
    Entry { words: &["forward", "slash"], repl: "/", behavior: Behavior::SpacePunct },
    Entry { words: &["slash"], repl: "/", behavior: Behavior::SpacePunct },
    Entry { words: &["degrees", "celsius"], repl: "°C", behavior: Behavior::SpacePunct },
    Entry { words: &["degrees", "fahrenheit"], repl: "°F", behavior: Behavior::SpacePunct },
    Entry { words: &["degree", "sign"], repl: "°", behavior: Behavior::SpacePunct },
    Entry { words: &["degrees"], repl: "°", behavior: Behavior::SpacePunct },
    // requestedPunctuation (noun-marker guarded).
    Entry { words: &["comma"], repl: ",", behavior: Behavior::Requested },
    Entry { words: &["period"], repl: ".", behavior: Behavior::Requested },
    // symbols with attachment.
    Entry { words: &["at", "the", "rate"], repl: "@", behavior: Behavior::AtHandle },
    Entry { words: &["at", "sign"], repl: "@", behavior: Behavior::AtHandle },
    Entry { words: &["hash"], repl: "#", behavior: Behavior::HashNumber },
    Entry { words: &["hashtag"], repl: "#", behavior: Behavior::HashNumber },
    Entry { words: &["pound", "sign"], repl: "#", behavior: Behavior::HashNumber },
    Entry { words: &["number", "sign"], repl: "#", behavior: Behavior::HashNumber },
    Entry { words: &["percent", "sign"], repl: "%", behavior: Behavior::PercentNumber },
    Entry { words: &["percent"], repl: "%", behavior: Behavior::PercentNumber },
    Entry { words: &["dollar", "sign"], repl: "$", behavior: Behavior::DollarNumber },
    Entry { words: &["dollar"], repl: "$", behavior: Behavior::DollarNumber },
];

/// Words that make a following `comma`/`period` a noun rather than a cue.
const NOUN_MARKERS: &[&str] = &[
    "a", "an", "the", "this", "that", "my", "your", "his", "her", "its", "our",
    "their", "any", "every", "no",
];

/// Number words accepted by `#`/`$`/`%` attachment (mirrors `numbers.rs` scope).
const NUMBER_WORDS: &[&str] = &[
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight",
    "nine", "ten", "eleven", "twelve", "thirteen", "fourteen", "fifteen",
    "sixteen", "seventeen", "eighteen", "nineteen", "twenty", "thirty",
    "forty", "fifty", "sixty", "seventy", "eighty", "ninety", "hundred",
    "thousand", "million", "billion", "trillion",
];

/// Lowercase a token and strip its edge punctuation (matches `text.rs` style).
fn normalize(tok: &str) -> String {
    tok.trim_matches(|c: char| c.is_ascii_punctuation())
        .to_ascii_lowercase()
}

/// True when `phrase`, normalized, is one of the spoken-command phrases.
/// Used by the vocabulary command-fix pass (`text::is_command_replacement`).
pub fn is_spoken_command(phrase: &str) -> bool {
    let norm: Vec<String> = phrase.split_whitespace().map(normalize).collect();
    if norm.is_empty() || norm.iter().any(|s| s.is_empty()) {
        return false;
    }
    ENTRIES.iter().any(|e| {
        e.words.len() == norm.len() && e.words.iter().zip(norm.iter()).all(|(w, n)| *w == n.as_str())
    })
}

/// A whitespace-separated input token, remembering whether a newline preceded it.
struct Tok {
    text: String,
    nl_before: bool,
}

fn tokenize(text: &str) -> Vec<Tok> {
    let mut out: Vec<Tok> = Vec::new();
    let mut cur = String::new();
    let mut nl = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !cur.is_empty() {
                out.push(Tok { text: std::mem::take(&mut cur), nl_before: nl });
                nl = false;
            }
            if ch == '\n' {
                nl = true;
            }
        } else {
            cur.push(ch);
        }
    }
    if !cur.is_empty() {
        out.push(Tok { text: cur, nl_before: nl });
    }
    out
}

/// An output fragment. `glue` => no separator before it; otherwise use a space
/// or (when `nl_before`) a newline.
struct Piece {
    text: String,
    glue: bool,
    nl_before: bool,
}

fn literal(tok: &Tok) -> Piece {
    Piece { text: tok.text.clone(), glue: false, nl_before: tok.nl_before }
}

fn join_pieces(pieces: &[Piece]) -> String {
    let mut out = String::new();
    for (idx, p) in pieces.iter().enumerate() {
        if idx > 0 && !p.glue {
            out.push(if p.nl_before { '\n' } else { ' ' });
        }
        out.push_str(&p.text);
    }
    out
}

fn is_number_token(tok: &str) -> bool {
    let n = normalize(tok);
    if n.is_empty() {
        return false;
    }
    if n.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    let has_digit = n.chars().any(|c| c.is_ascii_digit());
    let numeric = has_digit
        && n.chars().all(|c| c.is_ascii_digit() || c == '.' || c == ',');
    if numeric {
        return true;
    }
    NUMBER_WORDS.contains(&n.as_str())
}

fn is_initial(tok: &str) -> bool {
    let n = normalize(tok);
    let mut chars = n.chars();
    matches!((chars.next(), chars.next()), (Some(c), None) if c.is_alphabetic())
}

fn is_noun_marker(word: &str) -> bool {
    NOUN_MARKERS.contains(&word)
}

/// Longest phrase (by word count) matching the tokens starting at `i`.
fn match_phrase(toks: &[Tok], i: usize) -> Option<(&'static Entry, usize)> {
    let mut best: Option<(&'static Entry, usize)> = None;
    for e in ENTRIES {
        let n = e.words.len();
        if i + n > toks.len() {
            continue;
        }
        if (0..n).all(|k| normalize(&toks[i + k].text) == e.words[k]) {
            match best {
                Some((_, bn)) if bn >= n => {}
                _ => best = Some((e, n)),
            }
        }
    }
    best
}

/// Replace spoken punctuation / newline / symbol phrases with a token walk, so
/// context rules (guards, `@#%$` attachment, domain join) can be expressed.
pub fn apply_spoken_commands(text: &str) -> String {
    let toks = tokenize(text);
    let mut pieces: Vec<Piece> = Vec::new();
    let mut prev_word: Option<String> = None;
    let mut i = 0;
    while i < toks.len() {
        let Some((entry, n)) = match_phrase(&toks, i) else {
            pieces.push(literal(&toks[i]));
            prev_word = Some(normalize(&toks[i].text));
            i += 1;
            continue;
        };
        let first_nl = toks[i].nl_before;
        match entry.behavior {
            Behavior::Break => {
                pieces.push(Piece { text: entry.repl.to_owned(), glue: false, nl_before: first_nl });
                prev_word = None;
                i += n;
            }
            Behavior::Punct => {
                pieces.push(Piece { text: entry.repl.to_owned(), glue: true, nl_before: false });
                prev_word = None;
                i += n;
            }
            Behavior::SpacePunct => {
                pieces.push(Piece { text: entry.repl.to_owned(), glue: false, nl_before: first_nl });
                prev_word = None;
                i += n;
            }
            Behavior::Requested => {
                let blocked = prev_word.as_deref().is_some_and(is_noun_marker);
                if blocked {
                    pieces.push(literal(&toks[i]));
                    prev_word = Some(normalize(&toks[i].text));
                    i += 1;
                } else {
                    pieces.push(Piece { text: entry.repl.to_owned(), glue: true, nl_before: false });
                    prev_word = None;
                    i += n;
                }
            }
            Behavior::AtHandle => {
                pieces.push(Piece { text: "@".to_owned(), glue: false, nl_before: first_nl });
                i += n;
                if i < toks.len() {
                    let initial = is_initial(&toks[i].text);
                    pieces.push(Piece { text: toks[i].text.clone(), glue: true, nl_before: false });
                    prev_word = Some(normalize(&toks[i].text));
                    i += 1;
                    if initial {
                        while i < toks.len() && is_initial(&toks[i].text) {
                            pieces.push(Piece {
                                text: toks[i].text.clone(),
                                glue: true,
                                nl_before: false,
                            });
                            prev_word = Some(normalize(&toks[i].text));
                            i += 1;
                        }
                    }
                }
            }
            Behavior::HashNumber | Behavior::DollarNumber => {
                let sym = if entry.behavior == Behavior::HashNumber { "#" } else { "$" };
                if i + n < toks.len() && is_number_token(&toks[i + n].text) {
                    pieces.push(Piece { text: sym.to_owned(), glue: false, nl_before: first_nl });
                    pieces.push(Piece {
                        text: toks[i + n].text.clone(),
                        glue: true,
                        nl_before: false,
                    });
                    prev_word = Some(normalize(&toks[i + n].text));
                    i += n + 1;
                } else {
                    pieces.push(literal(&toks[i]));
                    prev_word = Some(normalize(&toks[i].text));
                    i += 1;
                }
            }
            Behavior::PercentNumber => {
                let on_number = prev_word.as_deref().is_some_and(is_number_token);
                if on_number {
                    pieces.push(Piece { text: "%".to_owned(), glue: true, nl_before: false });
                    prev_word = None;
                    i += n;
                } else {
                    pieces.push(literal(&toks[i]));
                    prev_word = Some(normalize(&toks[i].text));
                    i += 1;
                }
            }
        }
    }
    let joined = join_pieces(&pieces);
    collapse_space_around_breaks(&joining_domains(&joined))
}

/// Join spoken domains after the command pass: `gmail dot com` -> `gmail.com`,
/// `gmail. com` -> `gmail.com`, `name @gmail.com` -> `name@gmail.com`.
fn joining_domains(text: &str) -> String {
    let dot = Regex::new(r"(?i)\b([\p{L}\p{N}_-]+)\s+dot\s+([\p{L}\p{N}_-]+)\b")
        .expect("domain dot pattern");
    // Case-sensitive: only join a lowercase label + lowercase TLD-ish word, so
    // real sentence breaks ("End. Next") are left alone.
    let literal = Regex::new(r"\b([a-z0-9_-]+)\.\s+([a-z]{2,24})\b")
        .expect("domain literal pattern");
    let email = Regex::new(r"([\p{L}\p{N}_.-]+)\s+@([\p{L}\p{N}-]+(?:\.[\p{L}\p{N}-]+)+)")
        .expect("email join pattern");
    let mut s = text.to_owned();
    loop {
        let next = dot.replace_all(&s, "$1.$2").into_owned();
        if next == s {
            break;
        }
        s = next;
    }
    let s = literal.replace_all(&s, "$1.$2").into_owned();
    email.replace_all(&s, "$1@$2").into_owned()
}

fn collapse_space_around_breaks(text: &str) -> String {
    let after_open = Regex::new(r#"([\(\["])\s+"#).expect("open punct");
    let before_close = Regex::new(r#"\s+([,.;:!?…\)\]"])"#).expect("close punct");
    let around_nl = Regex::new(r"[^\S\n]*\n[^\S\n]*").expect("nl");
    let s = after_open.replace_all(text, "$1");
    let s = before_close.replace_all(&s, "$1");
    around_nl.replace_all(&s, "\n").into_owned()
}

const CARDINALS: &[&str] = &[
    "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
];
const ORDINALS: &[&str] = &[
    "first", "second", "third", "fourth", "fifth", "sixth", "seventh", "eighth",
    "ninth", "tenth",
];

fn marker_index(word: &str, family: &[&str]) -> Option<usize> {
    let core = word
        .trim_matches(|c: char| c.is_ascii_punctuation())
        .to_ascii_lowercase();
    family.iter().position(|m| *m == core)
}

/// Numbered / bulleted lists when the utterance *starts* with a marker
/// sequence (avoids “I have one apple and two friends”).
pub fn apply_lists(text: &str) -> String {
    let trimmed = text.trim();
    if let Some(out) = try_marker_list(trimmed, CARDINALS) {
        return out;
    }
    if let Some(out) = try_marker_list(trimmed, ORDINALS) {
        return out;
    }
    if let Some(out) = try_bullet_list(trimmed) {
        return out;
    }
    text.to_owned()
}

fn try_marker_list(text: &str, family: &[&str]) -> Option<String> {
    let toks: Vec<&str> = text.split_whitespace().collect();
    if toks.is_empty() || marker_index(toks[0], family) != Some(0) {
        return None;
    }
    let mut items: Vec<String> = Vec::new();
    let mut expected = 0usize;
    let mut cur = String::new();
    for tok in &toks {
        if let Some(idx) = marker_index(tok, family) {
            if idx == expected {
                if expected > 0 {
                    let item = cur.trim().to_owned();
                    if item.is_empty() {
                        return None;
                    }
                    items.push(item);
                    cur.clear();
                }
                expected += 1;
                continue;
            }
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(tok);
    }
    if !cur.trim().is_empty() {
        items.push(cur.trim().to_owned());
    }
    if items.len() < 2 || expected < 2 {
        return None;
    }
    Some(
        items
            .iter()
            .enumerate()
            .map(|(i, t)| format!("{}. {t}", i + 1))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn try_bullet_list(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let starts_bullet = lower.starts_with("bullet ") || lower.starts_with("bullet\n");
    if !starts_bullet {
        return None;
    }
    let re = Regex::new(r"(?i)(?:^|\s+)bullet\s+").expect("bullet split");
    let parts: Vec<String> = re
        .split(text)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    if parts.len() < 2 {
        return None;
    }
    Some(parts.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n"))
}

const DISCARD: &[&str] = &["scratch that", "never mind", "disregard that", "remove that"];
const APOLOGIES: &[&str] = &["sorry", "oops", "wait"];
const PRONOUNS: &[&str] = &["i", "we", "they", "he", "she", "you"];

/// Conservative backtrack: drop a clause after a discard cue (“scratch that”,
/// “never mind”, “disregard that”, “remove that”), or just the word before an
/// apology that immediately precedes a cue (“Friday sorry remove that”), and
/// rewrite “X actually Y” / “X I mean Y” when X is not a pronoun.
pub fn apply_backtrack(text: &str) -> String {
    let mut out = drop_discard_commands(text);
    out = rewrite_actually(&out);
    rewrite_i_mean(&out)
}

fn drop_discard_commands(text: &str) -> String {
    let alts = DISCARD
        .iter()
        .map(|p| regex::escape(p).replace(' ', r"\s+"))
        .collect::<Vec<_>>()
        .join("|");
    let apologies = APOLOGIES.join("|");
    // Group 1 (optional): an apology immediately before the cue. When present we
    // keep the preceding clause minus its last word instead of deleting it.
    let re = Regex::new(&format!(
        r"(?i)(?:^|[.!?]\s*|\n)([^.!?\n]*?)(?:\b({apologies})\b\s+)?\b(?:{alts})\b[,\.]?\s*"
    ))
    .expect("discard pattern");
    let s = re.replace_all(text, |caps: &regex::Captures| {
        let whole = caps.get(0).map(|g| g.as_str()).unwrap_or("");
        if caps.get(2).is_some() {
            let pre = caps.get(1).map(|g| g.as_str()).unwrap_or("");
            let trimmed = pre.trim_end();
            match trimmed.rfind(char::is_whitespace) {
                Some(p) => format!("{} ", &trimmed[..p]),
                None => String::new(),
            }
        } else if whole.starts_with('.') || whole.starts_with('!') || whole.starts_with('?') {
            whole.chars().next().map(|c| format!("{c} ")).unwrap_or_default()
        } else {
            String::new()
        }
    });
    collapse_space_around_breaks(s.trim())
}

fn rewrite_actually(text: &str) -> String {
    let toks: Vec<&str> = text.split_whitespace().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        if toks[i].eq_ignore_ascii_case("actually") && i + 1 < toks.len() && !out.is_empty() {
            let prev = out.last().unwrap().trim_matches(|c: char| c.is_ascii_punctuation());
            if !PRONOUNS.contains(&prev.to_ascii_lowercase().as_str()) {
                out.pop();
                out.push(toks[i + 1].to_owned());
                i += 2;
                continue;
            }
        }
        out.push(toks[i].to_owned());
        i += 1;
    }
    out.join(" ")
}

fn rewrite_i_mean(text: &str) -> String {
    let toks: Vec<&str> = text.split_whitespace().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let is_i = toks[i].eq_ignore_ascii_case("i")
            || toks[i].eq_ignore_ascii_case("i,");
        if is_i
            && i + 2 < toks.len()
            && toks[i + 1].eq_ignore_ascii_case("mean")
            && !out.is_empty()
        {
            out.pop();
            out.push(toks[i + 2].to_owned());
            i += 3;
            continue;
        }
        out.push(toks[i].to_owned());
        i += 1;
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spoken_period_and_comma() {
        assert_eq!(
            apply_spoken_commands("hello comma world period"),
            "hello, world."
        );
    }

    #[test]
    fn spoken_newline_and_paragraph() {
        let out = apply_spoken_commands("first new line second new paragraph third");
        assert_eq!(out, "first\nsecond\n\nthird");
    }

    #[test]
    fn spoken_does_not_eat_partial_words() {
        assert_eq!(apply_spoken_commands("periodic table"), "periodic table");
        assert_eq!(apply_spoken_commands("commander"), "commander");
    }

    #[test]
    fn list_from_one_two_three() {
        assert_eq!(
            apply_lists("one buy milk two call mom three send email"),
            "1. buy milk\n2. call mom\n3. send email"
        );
    }

    #[test]
    fn list_ignores_prose_numbers() {
        assert_eq!(
            apply_lists("I have one apple and two friends"),
            "I have one apple and two friends"
        );
    }

    #[test]
    fn list_from_first_second() {
        assert_eq!(
            apply_lists("first wash dishes second take out trash"),
            "1. wash dishes\n2. take out trash"
        );
    }

    #[test]
    fn bullet_list() {
        assert_eq!(
            apply_lists("bullet milk bullet eggs"),
            "- milk\n- eggs"
        );
    }

    #[test]
    fn scratch_that_drops_clause() {
        assert_eq!(
            apply_backtrack("hello world scratch that goodbye"),
            "goodbye"
        );
        assert_eq!(
            apply_backtrack("Keep this. Wrong take scratch that Right take"),
            "Keep this. Right take"
        );
    }

    #[test]
    fn actually_rewrites_number_not_enjoyed() {
        assert_eq!(
            apply_backtrack("the meeting is at 5 actually 3"),
            "the meeting is at 3"
        );
        assert_eq!(
            apply_backtrack("I actually enjoyed it"),
            "I actually enjoyed it"
        );
    }

    #[test]
    fn i_mean_replaces_previous_word() {
        assert_eq!(
            apply_backtrack("the color is blue I mean red"),
            "the color is red"
        );
    }

    #[test]
    fn pipeline_spoken_then_list() {
        // The engine wires these three in order (commands → backtrack → lists).
        let out = apply_lists(&apply_backtrack(&apply_spoken_commands(
            "one milk comma two eggs period",
        )));
        assert_eq!(out, "1. milk,\n2. eggs.");
    }

    #[test]
    fn remove_that_and_apology_cues() {
        assert_eq!(
            apply_backtrack("Let's ship Friday sorry remove that Monday"),
            "Let's ship Monday"
        );
        assert_eq!(
            apply_backtrack("hello world remove that goodbye"),
            "goodbye"
        );
    }

    #[test]
    fn table_driven_spoken_commands() {
        let cases: &[(&str, &str)] = &[
            // alwaysPunctuation.
            ("hello comma world period", "hello, world."),
            ("first new line second new paragraph third", "first\nsecond\n\nthird"),
            ("question mark", "?"),
            ("done exclamation mark", "done!"),
            ("wait ellipsis", "wait…"),
            ("dot dot dot", "…"),
            // not commands / partial words.
            ("periodic table", "periodic table"),
            ("commander", "commander"),
            // noun-marker guard on requested punctuation.
            ("the period of time", "the period of time"),
            ("send a comma", "send a comma"),
            // symbols with attachment.
            ("issue hash 42", "issue #42"),
            ("hash out a plan", "hash out a plan"),
            ("50 percent", "50%"),
            ("what percent of users", "what percent of users"),
            ("what percentage of users", "what percentage of users"),
            ("dollar 20", "$20"),
            // handles and domains.
            ("ping at the rate KV", "ping @KV"),
            ("at the rate K V", "@KV"),
            ("name at the rate gmail dot com", "name@gmail.com"),
            ("gmail dot com", "gmail.com"),
            ("gmail. com", "gmail.com"),
        ];
        for (input, want) in cases {
            assert_eq!(apply_spoken_commands(input), *want, "input: {input:?}");
        }
    }

    #[test]
    fn is_spoken_command_helper() {
        assert!(is_spoken_command("question mark"));
        assert!(is_spoken_command("Question Mark"));
        assert!(is_spoken_command("comma"));
        assert!(!is_spoken_command("hello"));
        assert!(!is_spoken_command("periodic"));
        assert!(!is_spoken_command(""));
    }
}
