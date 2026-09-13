//! Offline spoken-command, list, and backtrack passes (P2). No LLM.

use regex::Regex;

/// Longest-first spoken commands. Phrase must be a whole word / phrase.
const COMMANDS: &[(&str, &str)] = &[
    ("new paragraph", "\n\n"),
    ("next paragraph", "\n\n"),
    ("new line", "\n"),
    ("next line", "\n"),
    ("newline", "\n"),
    ("line break", "\n"),
    ("question mark", "?"),
    ("exclamation mark", "!"),
    ("exclamation point", "!"),
    ("full stop", "."),
    ("semi colon", ";"),
    ("semicolon", ";"),
    ("colon", ":"),
    ("comma", ","),
    ("ellipsis", "…"),
    ("dot dot dot", "…"),
    ("em dash", "—"),
    ("open parenthesis", "("),
    ("close parenthesis", ")"),
    ("open bracket", "["),
    ("close bracket", "]"),
    ("open quote", "\""),
    ("close quote", "\""),
    ("quote", "\""),
    ("unquote", "\""),
    ("degrees celsius", "°C"),
    ("degrees fahrenheit", "°F"),
    ("degree sign", "°"),
    ("degrees", "°"),
    ("percent sign", "%"),
    ("dollar sign", "$"),
    ("at sign", "@"),
    ("ampersand", "&"),
    ("asterisk", "*"),
    ("hashtag", "#"),
    ("pound sign", "#"),
    ("underscore", "_"),
    ("backslash", "\\"),
    ("forward slash", "/"),
    ("period", "."),
    ("slash", "/"),
    ("hyphen", "-"),
    ("dash", "-"),
    ("percent", "%"),
];

fn command_re() -> Regex {
    let alts: Vec<String> = COMMANDS
        .iter()
        .map(|(p, _)| regex::escape(p).replace(' ', r"\s+"))
        .collect();
    Regex::new(&format!(r"(?i)\b(?:{})\b", alts.join("|"))).expect("spoken-command pattern")
}

fn replacement_for(phrase: &str) -> String {
    let key = phrase.split_whitespace().collect::<Vec<_>>().join(" ");
    COMMANDS
        .iter()
        .find(|(p, _)| p.eq_ignore_ascii_case(&key))
        .map(|(_, r)| (*r).to_owned())
        .unwrap_or_else(|| phrase.to_owned())
}

/// Replace spoken punctuation / newline phrases. Longest phrases win
/// because they are listed first in the alternation? Rust regex is
/// left-to-right, so we list longer phrases first in COMMANDS.
pub fn apply_spoken_commands(text: &str) -> String {
    let re = command_re();
    let mut out = String::new();
    let mut last = 0;
    for m in re.find_iter(text) {
        out.push_str(&text[last..m.start()]);
        let repl = replacement_for(m.as_str());
        // No extra space before closing punctuation.
        if matches!(repl.as_str(), "." | "," | "!" | "?" | ";" | ":" | "…" | ")" | "]" | "\"") {
            let trimmed = out.trim_end();
            out.truncate(trimmed.len());
        }
        out.push_str(&repl);
        last = m.end();
    }
    out.push_str(&text[last..]);
    collapse_space_around_breaks(&out)
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

const DISCARD: &[&str] = &["scratch that", "never mind", "disregard that"];
const PRONOUNS: &[&str] = &["i", "we", "they", "he", "she", "you"];

/// Conservative backtrack: drop a clause after “scratch that” / “never mind”,
/// and rewrite “X actually Y” / “X I mean Y” when X is not a pronoun.
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
    let re = Regex::new(&format!(
        r"(?i)(?:^|[.!?]\s*|\n)[^.!?\n]*?\b(?:{alts})\b[,\.]?\s*"
    ))
    .expect("discard pattern");
    let s = re.replace_all(text, |caps: &regex::Captures| {
        let m = caps.get(0).map(|g| g.as_str()).unwrap_or("");
        if m.starts_with('.') || m.starts_with('!') || m.starts_with('?') {
            m.chars().next().unwrap().to_string() + " "
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

/// Full P2 offline pass, before user dictionary + cleanup.
pub fn apply_offline_format(text: &str) -> String {
    let spoken = apply_spoken_commands(text);
    let back = apply_backtrack(&spoken);
    apply_lists(&back)
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
        let out = apply_offline_format("one milk comma two eggs period");
        assert_eq!(out, "1. milk,\n2. eggs.");
    }
}
