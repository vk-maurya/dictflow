// Port of BetterWispr EmailDictation.swift plus deviations:
// - Closing phrases are exactly best regards | kind regards | regards |
//   thank you | thanks | cheers | sincerely | warmly (no bare "best",
//   "wishes", "yours", or "warm regards").
// - "thank you" renders as "Thanks" (not "Thank you").
// - When a closing has no signature, the addressee's name is used to sign.
// - A greeting/body with no recognised closing yields no sign-off block.
// - The addressee name is case-sensitive (one or two capitalized words) and
//   its trailing punctuation is stripped.
// - `send` and a leading `please` are accepted as the intent verb/prefix.
// - No tone or polish gating here; the caller (engine.rs) gates EN/auto.
//
// The module is not yet wired into engine.rs (that lands with the caller task),
// so silence dead-code until the call site exists.
#![allow(dead_code)]

use std::sync::OnceLock;

use regex::Regex;

/// Recognised closings, longest/most-specific first so the alternation and the
/// last-occurrence scan prefer "best regards" over "regards", etc.
const CLOSINGS: &[(&str, &str)] = &[
    ("best regards", "Best regards"),
    ("kind regards", "Kind regards"),
    ("regards", "Regards"),
    ("thank you", "Thanks"),
    ("thanks", "Thanks"),
    ("cheers", "Cheers"),
    ("sincerely", "Sincerely"),
    ("warmly", "Warmly"),
];

fn intent_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\s*(?i:please\s+)?(?i:write|draft|compose|send)\s+(?i:an?\s+)?(?i:e-?mail|email)\s+(?i:to)\s+(?P<name>[A-Z][A-Za-z'’.\-]*(?:\s+[A-Z][A-Za-z'’.\-]*)?)\s*,?\s+(?i:saying|telling\s+(?:him|her)|letting\s+(?:him|her)\s+know|that)(?:\s+that)?\s*[,:]?\s+(?P<body>(?s:.+))$",
        )
        .expect("email intent pattern")
    })
}

fn closing_phrase_alt() -> String {
    CLOSINGS
        .iter()
        .map(|(phrase, _)| regex::escape(phrase))
        .collect::<Vec<_>>()
        .join("|")
}

fn closing_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            r"(?i:\b(?P<closing>{})\b)(?:\s*,\s*(?P<sig>[A-Z][A-Za-z'’.\-]*(?:\s+[A-Z][A-Za-z'’.\-]*)?))?",
            closing_phrase_alt()
        ))
        .expect("email closing pattern")
    })
}

/// Lay out "write an email to Sam saying …" as a greeting, body and sign-off.
/// Returns `None` for text without an explicit email intent.
pub fn compose_email(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let caps = intent_re().captures(trimmed)?;
    let name = strip_trailing_punct(caps.name("name")?.as_str());
    if name.is_empty() {
        return None;
    }

    let rest = caps.name("body")?.as_str().trim();
    if rest.is_empty() {
        return None;
    }

    let (body_raw, closing) = split_closing(rest);
    let body = normalize_body(&body_raw);
    if body.is_empty() {
        return None;
    }

    let greeting = format!("Hi {},", title_case_words(&name));

    let Some((phrase, signature)) = closing else {
        return Some(format!("{greeting}\n\n{body}"));
    };

    let closing_line = render_closing(&phrase);
    let signer = match signature {
        Some(sig) if !sig.is_empty() => title_case_words(&sig),
        _ => title_case_words(&name),
    };
    Some(format!("{greeting}\n\n{body}\n\n{closing_line},\n{signer}"))
}

/// Split the post-connector remainder into body text and the *last* closing
/// occurrence (phrase + optional signature).
fn split_closing(rest: &str) -> (String, Option<(String, Option<String>)>) {
    let mut last: Option<regex::Captures<'_>> = None;
    for caps in closing_re().captures_iter(rest) {
        last = Some(caps);
    }
    let Some(caps) = last else {
        return (rest.to_owned(), None);
    };
    let Some(m) = caps.get(0) else {
        return (rest.to_owned(), None);
    };
    let body = rest[..m.start()].to_owned();
    let phrase = caps
        .name("closing")
        .map(|c| c.as_str().to_lowercase())
        .unwrap_or_default();
    let signature = caps
        .name("sig")
        .map(|c| strip_trailing_punct(c.as_str()))
        .filter(|s| !s.is_empty());
    (body, Some((phrase, signature)))
}

fn render_closing(phrase: &str) -> String {
    CLOSINGS
        .iter()
        .find(|(p, _)| p.eq_ignore_ascii_case(phrase))
        .map(|(_, rendered)| (*rendered).to_owned())
        .unwrap_or_else(|| capitalize_first(phrase))
}

/// Trim surrounding whitespace and any trailing sentence punctuation left over
/// from a hand-off to a closing phrase.
fn normalize_body(body: &str) -> String {
    let trimmed = body.trim().trim_end_matches([' ', '\t', '\r', '\n', ',', ';', ':']);
    if trimmed.is_empty() {
        return String::new();
    }
    let mut out = capitalize_first(trimmed);
    if !matches!(out.chars().last(), Some('.' | '!' | '?')) {
        out.push('.');
    }
    out
}

fn strip_trailing_punct(s: &str) -> String {
    s.trim().trim_end_matches([',', '.', ';', ':', '!', '?']).trim().to_owned()
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn title_case_words(s: &str) -> String {
    s.split_whitespace()
        .map(capitalize_first)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lays_out_email_intent() {
        let cases: &[(&str, Option<&str>)] = &[
            (
                "write an email to Sarah saying I will be on holiday tomorrow, thanks, best regards, Kartik",
                Some("Hi Sarah,\n\nI will be on holiday tomorrow, thanks.\n\nBest regards,\nKartik"),
            ),
            (
                "draft an email to Sarah telling her that I will be late tomorrow, cheers, Sarah",
                Some("Hi Sarah,\n\nI will be late tomorrow.\n\nCheers,\nSarah"),
            ),
            (
                "compose an e-mail to John letting him know that the report is ready, kind regards, Jane",
                Some("Hi John,\n\nThe report is ready.\n\nKind regards,\nJane"),
            ),
            (
                "send an email to Ravi saying thanks for the update, thanks, Ravi",
                Some("Hi Ravi,\n\nThanks for the update.\n\nThanks,\nRavi"),
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(compose_email(input).as_deref(), *expected, "input: {input}");
        }
    }

    #[test]
    fn returns_none_without_intent() {
        for input in ["what is the email to Sarah", "hello world", "email"] {
            assert_eq!(compose_email(input), None, "input: {input}");
        }
    }

    #[test]
    fn signer_defaults_to_addressee_when_closing_has_no_name() {
        assert_eq!(
            compose_email("write an email to Sarah saying I will be late, regards").as_deref(),
            Some("Hi Sarah,\n\nI will be late.\n\nRegards,\nSarah"),
        );
    }

    #[test]
    fn never_panics_on_junk() {
        let junk = [
            "",
            " ",
            "email to ",
            "write an email to",
            "write an email to saying hi",
            "write an email to Sarah",
            "write an email to Sarah saying",
            "compose an email to  saying hi",
            "write an email to 123 saying hello",
            "WRITE AN EMAIL TO SARAH SAYING HELLO, THANKS",
            "write an email to Sarah\nsaying hello",
            "draft an email to Sarah telling her letting him know that hi, thanks, x",
            "write an email to Sarah saying ,,,,,",
            "write an email to Jean-Luc saying bonjour, sincerely",
        ];
        for input in junk {
            let _ = compose_email(input);
        }
    }
}
