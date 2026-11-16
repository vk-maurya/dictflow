//! Spoken-number normalization (P0a).
//!
//! Port of BetterWispr `SpokenNumbers.swift` plus any deviations:
//!
//! - fixed-width `u64` state machine with `saturating_*` arithmetic everywhere
//!   (the Swift original leans on `Int`); user text can never panic us;
//! - decimals accumulate a *digit string* instead of a float, so
//!   `zero point one point one` renders `0.1.1` verbatim;
//! - a dangling `and` / `point` at the end of a reading falls back to words so
//!   `one hundred and` is never corrupted;
//! - Indian grouping (`2,50,000`) is applied only when `lakh`/`crore` was
//!   spoken; western grouping otherwise (`250,000`);
//! - commas are only inserted at ≥ 10,000 so `twelve hundred` stays `1200`;
//! - hyphenated number words (`twenty-one`, `seventy-five thousand`) collapse
//!   into digits; a hyphenated chunk with any non-number part is left verbatim
//!   (`one-hundred-dollar`, `uh-huh`).
//!
//! Guardrails ported from the original:
//!
//! - a reading of a single small number stays words (`give me five minutes`);
//! - ordinals (`first`, `fifth`) are not in the lexicon and always stay;
//! - adjacent "small family" words stay words (year pairs `nineteen ninety
//!   nine`, digit runs `fifty fifty`, `twenty four seven`, `one two three`);
//! - a partial read keeps its readable prefix (`ten thousand hundred
//!   followers` → `10,000 hundred followers`).
//!
//! Pure function: `&str -> String`, no I/O, no clocks, no locks.

/// Decimal digit-string length cap: past this the reading falls back to words
/// instead of growing a runaway string.
const DECIMAL_CAP: usize = 32;

/// Values below this are printed without grouping commas (`1200`, not
/// `1,200`) to match the reference behavior for `twelve hundred`.
const GROUP_THRESHOLD: u64 = 10_000;

// ---------------------------------------------------------------------------
// Lexicon
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// The article `a`/`an` — only meaningful directly before `hundred`/scale.
    Article,
    Zero,
    Unit(u64),
    Teen(u64),
    Tens(u64),
    Hundred,
    /// scale multiplier: thousand / lakh / crore / million / …
    Scale(u64),
    And,
    Point,
    Percent,
}

/// Map one lowercase word to its lexicon meaning.
fn lex_word(w: &str) -> Option<Kind> {
    let k = match w {
        "a" | "an" => Kind::Article,
        "zero" => Kind::Zero,
        "one" => Kind::Unit(1),
        "two" => Kind::Unit(2),
        "three" => Kind::Unit(3),
        "four" => Kind::Unit(4),
        "five" => Kind::Unit(5),
        "six" => Kind::Unit(6),
        "seven" => Kind::Unit(7),
        "eight" => Kind::Unit(8),
        "nine" => Kind::Unit(9),
        "ten" => Kind::Teen(10),
        "eleven" => Kind::Teen(11),
        "twelve" => Kind::Teen(12),
        "thirteen" => Kind::Teen(13),
        "fourteen" => Kind::Teen(14),
        "fifteen" => Kind::Teen(15),
        "sixteen" => Kind::Teen(16),
        "seventeen" => Kind::Teen(17),
        "eighteen" => Kind::Teen(18),
        "nineteen" => Kind::Teen(19),
        "twenty" => Kind::Tens(20),
        "thirty" => Kind::Tens(30),
        "forty" => Kind::Tens(40),
        "fifty" => Kind::Tens(50),
        "sixty" => Kind::Tens(60),
        "seventy" => Kind::Tens(70),
        "eighty" => Kind::Tens(80),
        "ninety" => Kind::Tens(90),
        "hundred" | "hundreds" => Kind::Hundred,
        "thousand" | "thousands" => Kind::Scale(1_000),
        "lakh" | "lakhs" => Kind::Scale(100_000),
        "million" | "millions" => Kind::Scale(1_000_000),
        "crore" | "crores" => Kind::Scale(10_000_000),
        "billion" | "billions" => Kind::Scale(1_000_000_000),
        "trillion" | "trillions" => Kind::Scale(1_000_000_000_000),
        "and" => Kind::And,
        "point" => Kind::Point,
        "percent" => Kind::Percent,
        _ => return None,
    };
    Some(k)
}

// ---------------------------------------------------------------------------
// State machine
// ---------------------------------------------------------------------------

/// Outcome of offering a token to the current reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    /// Token joined the reading.
    Take,
    /// Token does not fit; flush the reading and re-evaluate the token.
    Burn,
}

/// `Reading { total, group, scale, scales, last }` from the Swift original.
///
/// `total` holds completed, scaled groups; `group` is the `< 1000` group under
/// construction (so `two million three hundred thousand` adds `300 * 1_000`
/// rather than scaling the whole total again).
#[derive(Debug, Default, Clone)]
struct Reading {
    total: u64,
    group: u64,
    scales: u32,
    indian: bool,
    /// Adjacent small-number pattern seen (year / digit run) — flush as words.
    pair: bool,
    /// `None` until a `point` token; then Some(digits) with possible `.`
    /// separators ("1.1" for `one point one point one`).
    decimal: Option<String>,
    percent: bool,
    len: usize,
    last: Option<Kind>,
}

impl Reading {
    fn active(&self) -> bool {
        self.len > 0
    }

    /// A reading may only begin on a numeric-ish token. `and`/`percent`
    /// require a preceding value.
    fn can_start(&self, k: Kind) -> bool {
        !matches!(k, Kind::And | Kind::Percent)
    }

    fn consume(&mut self, k: Kind) -> Op {
        if self.decimal.is_some() {
            return self.consume_decimal(k);
        }
        match self.last {
            None => match k {
                Kind::And | Kind::Percent => Op::Burn,
                _ => self.take(k),
            },
            Some(prev) => self.follow(prev, k),
        }
    }

    /// Transition table. `Take` applies the value; `Burn` flushes.
    fn follow(&mut self, prev: Kind, k: Kind) -> Op {
        use Kind::*;
        match (prev, k) {
            // "a hundred", "a thousand".
            (Article, Hundred | Scale(_)) => self.take(k),
            (Article, _) => Op::Burn,

            // "zero point …"
            (Zero, Point) => self.take(k),
            (Zero, Percent) => self.take_percent(),
            (Zero, _) => Op::Burn,

            // unit: "two hundred", "two lakh", "five thousand", "five point two"
            (Unit(_), Hundred | Scale(_) | Point) => self.take(k),
            (Unit(_), Percent) => self.take_percent(),
            // adjacent units: digit run ("one two three") → words
            (Unit(_), Unit(_)) => self.take_pair(k),
            (Unit(_), _) => Op::Burn,

            // teen: "twelve hundred", "ten thousand", "nineteen hundred"
            (Teen(_), Hundred | Scale(_) | Point) => self.take(k),
            (Teen(_), Percent) => self.take_percent(),
            // "nineteen ninety nine" (year), "nineteen ten", "nineteen one"
            (Teen(_), Tens(_) | Teen(_) | Unit(_)) => self.take_pair(k),
            (Teen(_), _) => Op::Burn,

            // tens: "forty two", "two hundred thirty", "fifty thousand"
            (Tens(_), Unit(_) | Hundred | Scale(_) | And | Point) => self.take(k),
            (Tens(_), Percent) => self.take_percent(),
            // "fifty fifty" (digit run), "twenty eleven" (year)
            (Tens(_), Tens(_) | Teen(_)) => self.take_pair(k),
            (Tens(_), _) => Op::Burn,

            // hundred
            (Hundred, Unit(_) | Teen(_) | Tens(_) | Scale(_) | And | Point) => self.take(k),
            (Hundred, Percent) => self.take_percent(),
            (Hundred, _) => Op::Burn,

            // scale: a `hundred` *directly* after a scale is the partial-read
            // case ("ten thousand hundred followers"); a unit in between is
            // fine ("ten thousand five hundred").
            (Scale(_), Unit(_) | Teen(_) | Tens(_) | And | Point) => self.take(k),
            (Scale(_), Percent) => self.take_percent(),
            (Scale(_), _) => Op::Burn,

            // "and": "two hundred and fifty", "one thousand and one"
            (And, Unit(_) | Teen(_) | Tens(_)) => self.take(k),
            (And, _) => Op::Burn,

            (Point | Percent, _) => Op::Burn,
        }
    }

    fn take(&mut self, k: Kind) -> Op {
        self.apply_value(k);
        self.len = self.len.saturating_add(1);
        self.last = Some(k);
        Op::Take
    }

    /// Accept a token that is valid but marks the whole reading as a
    /// year/digit-run that must stay words.
    fn take_pair(&mut self, k: Kind) -> Op {
        self.pair = true;
        self.take(k)
    }

    fn take_percent(&mut self) -> Op {
        if self.total == 0 && self.group == 0 && self.decimal.is_none() {
            return Op::Burn;
        }
        self.percent = true;
        self.len = self.len.saturating_add(1);
        self.last = Some(Kind::Percent);
        Op::Take
    }

    fn apply_value(&mut self, k: Kind) {
        match k {
            Kind::Unit(v) | Kind::Teen(v) | Kind::Tens(v) => {
                self.group = self.group.saturating_add(v);
            }
            Kind::Hundred => {
                self.group = if self.group == 0 {
                    100
                } else {
                    self.group.saturating_mul(100)
                };
            }
            Kind::Scale(s) => {
                if self.total == 0 && self.group == 0 && self.scales == 0 {
                    // Bare scale ("thousand five hundred") implies one scale.
                    self.total = s;
                } else {
                    self.total = self.total.saturating_add(self.group.saturating_mul(s));
                    self.group = 0;
                }
                self.scales = self.scales.saturating_add(1);
                if s == 100_000 || s == 10_000_000 {
                    self.indian = true;
                }
            }
            Kind::Point => self.decimal = Some(String::new()),
            Kind::Article | Kind::Zero | Kind::And | Kind::Percent => {}
        }
    }

    /// Digits after `point`: each word becomes one digit (`five` → 5),
    /// multi-digit words append their value (`fifteen` → 15), a second `point`
    /// appends a literal `.` (so `0.1.1` round-trips), `percent` closes.
    fn consume_decimal(&mut self, k: Kind) -> Op {
        match k {
            Kind::Zero => self.push_decimal("0"),
            Kind::Unit(v) | Kind::Teen(v) | Kind::Tens(v) => self.push_decimal(&v.to_string()),
            Kind::Point => {
                let ok = self.decimal.as_ref().is_some_and(|d| !d.is_empty());
                if !ok {
                    return Op::Burn;
                }
                self.push_decimal(".")
            }
            Kind::Percent => {
                self.percent = true;
                self.len = self.len.saturating_add(1);
                Op::Take
            }
            _ => Op::Burn,
        }
    }

    fn push_decimal(&mut self, s: &str) -> Op {
        match self.decimal.as_mut() {
            Some(d) if d.len() + s.len() <= DECIMAL_CAP => {
                d.push_str(s);
                self.len = self.len.saturating_add(1);
                Op::Take
            }
            _ => Op::Burn,
        }
    }

    /// True when the reading must be emitted exactly as spoken.
    fn should_words(&self) -> bool {
        if self.pair {
            return true;
        }
        if self.last == Some(Kind::And) {
            return true;
        }
        if self.decimal.as_ref().is_some_and(|d| d.is_empty()) {
            return true;
        }
        if self.len <= 1 {
            return true;
        }
        self.total == 0 && self.group == 0 && self.decimal.is_none()
    }

    /// Rendered digits for a flush-worthy reading.
    fn value(&self) -> String {
        let int = self.total.saturating_add(self.group);
        let mut s = if int >= GROUP_THRESHOLD {
            group_int(int, self.indian)
        } else {
            int.to_string()
        };
        if let Some(d) = self.decimal.as_deref().filter(|d| !d.is_empty()) {
            s.push('.');
            s.push_str(d);
        }
        if self.percent {
            s.push('%');
        }
        s
    }
}

/// `250000` → `250,000` (western) / `2,50,000` (Indian, first group of 3 then
/// groups of 2).
fn group_int(v: u64, indian: bool) -> String {
    let digits: Vec<char> = v.to_string().chars().collect();
    let mut groups: Vec<String> = Vec::new();
    let mut end = digits.len();
    let mut first = true;
    while end > 0 {
        let take = if first {
            3
        } else if indian {
            2
        } else {
            3
        };
        let start = end.saturating_sub(take);
        groups.push(digits[start..end].iter().collect());
        end = start;
        first = false;
    }
    groups.reverse();
    groups.join(",")
}

// ---------------------------------------------------------------------------
// Tokenization
// ---------------------------------------------------------------------------

/// One input chunk plus the trivia needed to render it back.
struct Tok {
    /// whitespace before this chunk (empty for hyphen expansions)
    sep: String,
    /// leading punctuation kept around a rendered number (`(` in `(two`)
    lead: String,
    /// trailing punctuation kept around a rendered number (`,` in `two,`)
    trail: String,
    /// exact text for the words fallback
    word: String,
    lex: Option<Kind>,
}

fn is_edge_punct(c: char) -> bool {
    c.is_ascii_punctuation()
}

/// Split into whitespace-separated chunks, expanding hyphenated number words
/// (`twenty-one`) into adjacent tokens whose word strings still reconstruct
/// the original text if the reading falls back to words.
fn tokenize(text: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut sep = String::new();
    let mut chunk = String::new();
    for c in text.chars() {
        if c.is_whitespace() {
            if !chunk.is_empty() {
                push_chunk(&mut out, &sep, &chunk);
                sep.clear();
                chunk.clear();
            }
            sep.push(c);
        } else {
            chunk.push(c);
        }
    }
    if !chunk.is_empty() {
        push_chunk(&mut out, &sep, &chunk);
    }
    out
}

fn push_chunk(out: &mut Vec<Tok>, sep: &str, chunk: &str) {
    let start = chunk.len() - chunk.trim_start_matches(is_edge_punct).len();
    let end = chunk.trim_end_matches(is_edge_punct).len();
    // All-punctuation chunk: nothing to parse, keep verbatim (would also make
    // the `start..end` slice below invert).
    if start >= end {
        push_verbatim(out, sep, chunk);
        return;
    }
    let lead = &chunk[..start];
    let trail = &chunk[end..];
    let core = &chunk[start..end];

    // No interior hyphen: one plain number word ("twenty") or a miss.
    if !core.contains('-') {
        let lex = lex_word(&core.to_lowercase());
        out.push(Tok {
            sep: sep.to_owned(),
            lead: lead.to_owned(),
            trail: trail.to_owned(),
            word: chunk.to_owned(),
            lex,
        });
        return;
    }

    // Internal hyphens with every part a number word ("twenty-one").
    let parts: Vec<&str> = core.split('-').collect();
    let kinds: Option<Vec<Kind>> = parts
        .iter()
        .map(|p| lex_word(&p.to_lowercase()))
        .collect();
    let Some(kinds) = kinds else {
        push_verbatim(out, sep, chunk);
        return;
    };
    if parts.len() < 2 {
        push_verbatim(out, sep, chunk);
        return;
    }

    let last = parts.len() - 1;
    for (idx, (part, kind)) in parts.iter().zip(kinds).enumerate() {
        let mut word = String::new();
        if idx == 0 {
            word.push_str(lead);
        }
        word.push_str(part);
        if idx < last {
            word.push('-');
        } else {
            word.push_str(trail);
        }
        out.push(Tok {
            sep: if idx == 0 { sep.to_owned() } else { String::new() },
            lead: if idx == 0 { lead.to_owned() } else { String::new() },
            trail: if idx == last { trail.to_owned() } else { String::new() },
            word,
            lex: Some(kind),
        });
    }
}

fn push_verbatim(out: &mut Vec<Tok>, sep: &str, chunk: &str) {
    out.push(Tok {
        sep: sep.to_owned(),
        lead: String::new(),
        trail: String::new(),
        word: chunk.to_owned(),
        lex: None,
    });
}

/// `one, two, three, four` → `1, 2, 3, 4`.
///
/// A comma-separated run of ≥3 lone small number words is an enumeration, not
/// one composed quantity, so every word in it becomes a digit. Two items stay
/// with the state machine (`twenty, two` → `22`, the spoken-hyphen case), and
/// requiring the commas keeps the year/digit-run heuristic for a bare
/// `one two three four` intact.
fn rewrite_enumerations(toks: &mut [Tok]) {
    // An item is a lone small number word (units/teens/tens; `zero` too).
    // Hyphen-split halves ("twenty" in `twenty-one`) are excluded by the
    // comma rule below, so quantities never match.
    let value = |t: &Tok| match t.lex {
        Some(Kind::Zero) => Some(0u64),
        Some(Kind::Unit(v)) | Some(Kind::Teen(v)) | Some(Kind::Tens(v)) => Some(v),
        _ => None,
    };
    fn core(t: &Tok) -> &str {
        t.word.trim_matches(|c: char| c.is_ascii_punctuation())
    }
    // A connector sits between items: a standalone comma chunk, or the words
    // `and`/`or` (either may carry a trailing comma: `four, and twelve`).
    let is_comma_chunk = |t: &Tok| t.lex.is_none() && core(t).is_empty() && t.word.contains(',');
    let is_and = |t: &Tok| matches!(t.lex, Some(Kind::And)) && core(t).eq_ignore_ascii_case("and");
    let is_or = |t: &Tok| t.lex.is_none() && core(t).eq_ignore_ascii_case("or");
    let is_connector = |t: &Tok| is_comma_chunk(t) || is_and(t) || is_or(t);

    let n = toks.len();
    let mut items: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < n {
        if value(&toks[i]).is_none() {
            i += 1;
            continue;
        }
        // Walk the maximal run of items linked by connectors: the item before
        // a link must carry the comma, so `one two` (bare pair, year heuristic)
        // never links.
        items.clear();
        items.push(i);
        let mut j = i + 1;
        while j < n {
            if value(&toks[j]).is_some() {
                // An item that opens a composed quantity (`two hundred`,
                // `three thousand`) is not an enumeration member.
                if toks
                    .get(j + 1)
                    .is_some_and(|t| matches!(t.lex, Some(Kind::Hundred) | Some(Kind::Scale(_))))
                {
                    break;
                }
                if !toks[*items.last().unwrap()].word.ends_with(',') {
                    break;
                }
                items.push(j);
                j += 1;
            } else if is_connector(&toks[j]) {
                let Some(k) = (j + 1..n).find(|&k| value(&toks[k]).is_some()) else {
                    break;
                };
                if !(j + 1..k).all(|m| is_connector(&toks[m]))
                    || !toks[*items.last().unwrap()].word.ends_with(',')
                    || toks
                        .get(k + 1)
                        .is_some_and(|t| matches!(t.lex, Some(Kind::Hundred) | Some(Kind::Scale(_))))
                {
                    break;
                }
                items.push(k);
                j = k + 1;
            } else {
                break;
            }
        }
        if items.len() >= 3 {
            for &idx in &items {
                let v = value(&toks[idx]).unwrap();
                let word = &toks[idx].word;
                let lead_len = word.len()
                    - word.trim_start_matches(|c: char| c.is_ascii_punctuation())
                        .len();
                let trail_start = word.trim_end_matches(|c: char| c.is_ascii_punctuation())
                    .len();
                let word = format!(
                    "{}{v}{}",
                    &word[..lead_len],
                    &word[trail_start..]
                );
                toks[idx].word = word;
                toks[idx].lex = None; // the main loop passes digits through verbatim
            }
            i = items[items.len() - 1] + 1;
        } else {
            i += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Replace spoken number words with digits, leaving everything else intact.
///
/// Runs as the first rule pass so later passes (spoken commands, dictionary)
/// see digits — `issue hash forty two` → `issue hash 42`.
pub fn apply_spoken_numbers(text: &str) -> String {
    let mut toks = tokenize(text);
    rewrite_enumerations(&mut toks);
    let toks = toks;
    let mut out = String::new();
    let mut rd = Reading::default();
    let mut rd_start = 0usize;
    let mut i = 0usize;

    while i < toks.len() {
        match toks[i].lex {
            None => {
                if rd.active() {
                    flush(&mut out, &toks, rd_start, i, &rd);
                    rd = Reading::default();
                }
                out.push_str(&toks[i].sep);
                out.push_str(&toks[i].word);
                i += 1;
            }
            Some(k) => {
                if !rd.active() {
                    if rd.can_start(k) {
                        rd.consume(k);
                        rd_start = i;
                        i += 1;
                    } else {
                        out.push_str(&toks[i].sep);
                        out.push_str(&toks[i].word);
                        i += 1;
                    }
                } else {
                    match rd.consume(k) {
                        Op::Take => i += 1,
                        Op::Burn => {
                            // Partial read: flush what we have, then let this
                            // token start (or word-ify) on its own.
                            flush(&mut out, &toks, rd_start, i, &rd);
                            rd = Reading::default();
                        }
                    }
                }
            }
        }
    }
    if rd.active() {
        flush(&mut out, &toks, rd_start, toks.len(), &rd);
    }
    out
}

fn flush(out: &mut String, toks: &[Tok], start: usize, end: usize, rd: &Reading) {
    if start >= end {
        return;
    }
    if rd.should_words() {
        for t in &toks[start..end] {
            out.push_str(&t.sep);
            out.push_str(&t.word);
        }
    } else {
        out.push_str(&toks[start].sep);
        out.push_str(&toks[start].lead);
        out.push_str(&rd.value());
        out.push_str(&toks[end - 1].trail);
    }
}

#[cfg(test)]
mod tests {
    use super::apply_spoken_numbers;

    fn check(cases: &[(&str, &str)]) {
        for (input, want) in cases {
            assert_eq!(
                apply_spoken_numbers(input),
                *want,
                "input: {input:?}"
            );
        }
    }

    #[test]
    fn converts_units_tens_teens() {
        check(&[
            ("I have twenty two apples", "I have 22 apples"),
            ("ninety nine bottles", "99 bottles"),
            ("thirty one days", "31 days"),
            ("eleven hundred", "1100"),
            ("one hundred and one dalmatians", "101 dalmatians"),
            ("nine hundred and ninety nine", "999"),
            ("ten thousand", "10,000"),
            ("ten thousand five hundred", "10,500"),
            ("two hundred and fifty thousand", "250,000"),
            ("seventy-five thousand", "75,000"),
            ("five hundred", "500"),
            ("two hundred thirty four", "234"),
        ]);
    }

    #[test]
    fn hundred_scale_shortcuts() {
        check(&[
            ("a hundred bucks", "100 bucks"),
            ("a thousand apologies", "1000 apologies"),
            ("a hundred thousand", "100,000"),
            ("twelve hundred", "1200"),
            ("nineteen hundred", "1900"),
            ("thirty hundred", "3000"),
            ("one thousand five hundred", "1500"),
            ("two thousand and forty", "2040"),
            ("two million three hundred thousand", "2,300,000"),
            ("hundred followers", "hundred followers"),
        ]);
    }

    #[test]
    fn indian_grouping() {
        check(&[
            ("two lakh rupees", "2,00,000 rupees"),
            ("two lakh fifty thousand rupees", "2,50,000 rupees"),
            ("three crore", "3,00,00,000"),
            ("five crores", "5,00,00,000"),
            ("six lakh ten thousand", "6,10,000"),
        ]);
    }

    #[test]
    fn decimals_and_percent() {
        check(&[
            ("two point five", "2.5"),
            ("zero point five", "0.5"),
            ("point five", "0.5"),
            ("zero point one point one", "0.1.1"),
            ("ten point two three", "10.23"),
            ("two point five crore", "2.5 crore"),
            ("ten thousand point five", "10,000.5"),
            ("ten percent", "10%"),
            ("ten point five percent", "10.5%"),
            ("fifty percent of users agree", "50% of users agree"),
            ("TEN PERCENT", "10%"),
        ]);
    }

    #[test]
    fn enumerations_become_digits() {
        check(&[
            ("one, two, three, four", "1, 2, 3, 4"),
            (
                "it's one, two, three, four, and twelve",
                "it's 1, 2, 3, 4, and 12",
            ),
            ("pick one, two, or three", "pick 1, 2, or 3"),
            ("eleven, twelve, thirteen", "11, 12, 13"),
            ("(zero, one, two)", "(0, 1, 2)"),
            // No commas → the year/digit-run heuristic keeps them as words.
            ("one two three four", "one two three four"),
            // Non-number words break the run.
            ("I have one apple and two friends", "I have one apple and two friends"),
            // A quantity after the run is not part of it.
            ("one, two hundred dollars", "one, two hundred dollars"),
        ]);
    }

    #[test]
    fn lone_small_numbers_stay_words() {
        check(&[
            ("give me five minutes", "give me five minutes"),
            ("I have one apple and two friends", "I have one apple and two friends"),
            ("it costs two rupees", "it costs two rupees"),
            ("rewind thirty seconds", "rewind thirty seconds"),
            ("set a timer for fifteen minutes", "set a timer for fifteen minutes"),
            ("seventeen seasons", "seventeen seasons"),
            ("zero defects", "zero defects"),
        ]);
    }

    #[test]
    fn ordinals_stay_words() {
        check(&[
            ("the fifth place", "the fifth place"),
            ("first second third", "first second third"),
            ("my twenty first attempt", "my twenty first attempt"),
        ]);
    }

    #[test]
    fn year_pairs_and_digit_runs_stay_words() {
        check(&[
            ("nineteen ninety nine", "nineteen ninety nine"),
            ("nineteen eighty four", "nineteen eighty four"),
            ("twenty twenty five", "twenty twenty five"),
            ("nineteen ten", "nineteen ten"),
            ("fifty fifty", "fifty fifty"),
            ("twenty four seven", "twenty four seven"),
            ("one two three", "one two three"),
            ("call two one two", "call two one two"),
            ("seven eleven", "seven eleven"),
            ("ten ten", "ten ten"),
        ]);
    }

    #[test]
    fn partial_reads_keep_readable_prefix() {
        check(&[
            (
                "ten thousand hundred followers",
                "10,000 hundred followers",
            ),
            ("two hundred hundred", "200 hundred"),
            ("ten point", "ten point"),
        ]);
    }

    #[test]
    fn commands_and_dictionary_targets() {
        check(&[
            ("issue hash forty two", "issue hash 42"),
            ("Ollie ate two hundred and fifty six grapes", "Ollie ate 256 grapes"),
            ("open issue number one thousand twenty four", "open issue number 1024"),
        ]);
    }

    #[test]
    fn punctuation_whitespace_and_hyphens() {
        check(&[
            ("forty-two caps", "42 caps"),
            ("twenty-one", "21"),
            ("(two hundred) reasons", "(200) reasons"),
            ("I have twenty, two dollars", "I have 22 dollars"),
            ("a one-hundred-dollar bill", "a one-hundred-dollar bill"),
            ("uh-huh yes", "uh-huh yes"),
            ("twenty  one", "21"),
        ]);
    }

    #[test]
    fn percentages_prose_not_swallowed() {
        check(&[
            ("what percentage of users", "what percentage of users"),
            ("percent", "percent"),
            ("a percent of the pie", "a percent of the pie"),
        ]);
    }

    #[test]
    fn never_panics_on_junk() {
        for s in [
            "",
            "   ",
            "-",
            "- - -",
            "hundred hundred hundred",
            "thousand thousand",
            "point point five",
            "and and and",
            "lakh",
            "a a a",
        ] {
            let _ = apply_spoken_numbers(s);
        }
    }

    #[test]
    fn saturates_instead_of_overflowing() {
        // No lexicon word can actually reach u64::MAX, but the reading must
        // never panic even if scales stack absurdly.
        let big = "nine hundred ninety nine trillion trillion";
        let out = apply_spoken_numbers(big);
        assert!(!out.is_empty());
    }
}
