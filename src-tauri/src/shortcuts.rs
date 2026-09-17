//! Parse user-facing shortcut strings (`Shift+Alt+Z`) without Tauri types.
//!
//! Utility shortcuts (paste-last / copy-last) always need a modifier so we
//! never steal bare typing. `Ctrl+Alt+Space` is reserved for the talk combo.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutSpec {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
    pub key: String,
}

pub fn default_paste_last() -> String {
    "Shift+Alt+Z".to_owned()
}

pub fn default_copy_last() -> String {
    "Shift+Alt+X".to_owned()
}

/// Split on `+`, case-insensitive. Keys: A–Z, 0–9, Space, Escape, F1–F12.
pub fn parse_shortcut(s: &str) -> Result<ShortcutSpec, String> {
    let raw = s.trim();
    if raw.is_empty() {
        return Err("shortcut is empty".to_owned());
    }
    let mut spec = ShortcutSpec {
        ctrl: false,
        alt: false,
        shift: false,
        meta: false,
        key: String::new(),
    };
    for part in raw.split('+') {
        let token = part.trim();
        if token.is_empty() {
            return Err("shortcut has an empty token".to_owned());
        }
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => spec.ctrl = true,
            "alt" | "option" => spec.alt = true,
            "shift" => spec.shift = true,
            "meta" | "win" | "super" | "cmd" => spec.meta = true,
            other => {
                if !spec.key.is_empty() {
                    return Err(format!("shortcut has two keys: {} and {other}", spec.key));
                }
                spec.key = normalize_key(other)?;
            }
        }
    }
    if spec.key.is_empty() {
        return Err("shortcut has no key".to_owned());
    }
    Ok(spec)
}

fn normalize_key(token: &str) -> Result<String, String> {
    let t = token.trim();
    if t.len() == 1 {
        let c = t.chars().next().unwrap();
        if c.is_ascii_alphabetic() {
            return Ok(c.to_ascii_uppercase().to_string());
        }
        if c.is_ascii_digit() {
            return Ok(c.to_string());
        }
    }
    let lower = t.to_ascii_lowercase();
    if lower == "space" {
        return Ok("Space".to_owned());
    }
    if lower == "escape" || lower == "esc" {
        return Ok("Escape".to_owned());
    }
    if let Some(n) = lower.strip_prefix('f') {
        if let Ok(n) = n.parse::<u8>() {
            if (1..=12).contains(&n) {
                return Ok(format!("F{n}"));
            }
        }
    }
    Err(format!("unknown shortcut key: {token}"))
}

pub fn format_shortcut(s: &ShortcutSpec) -> String {
    let mut parts = Vec::new();
    if s.ctrl {
        parts.push("Ctrl");
    }
    if s.shift {
        parts.push("Shift");
    }
    if s.alt {
        parts.push("Alt");
    }
    if s.meta {
        // Show platform-native label for the meta/super key.
        #[cfg(target_os = "macos")]
        parts.push("Cmd");
        #[cfg(not(target_os = "macos"))]
        parts.push("Win");
    }
    parts.push(s.key.as_str());
    parts.join("+")
}

/// Letter/digit keys must carry at least one modifier.
pub fn is_safe_utility(s: &ShortcutSpec) -> bool {
    let needs_mod = s.key.len() == 1
        && s.key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric());
    if needs_mod {
        s.ctrl || s.alt || s.shift || s.meta
    } else {
        true
    }
}

/// Talk combo is reserved; paste/copy must not steal it.
pub fn is_talk_combo(s: &ShortcutSpec) -> bool {
    s.ctrl && s.alt && !s.shift && !s.meta && s.key == "Space"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shift_alt_z() {
        let s = parse_shortcut("Shift+Alt+Z").unwrap();
        assert!(s.shift && s.alt && !s.ctrl);
        assert_eq!(s.key, "Z");
        assert!(is_safe_utility(&s));
        assert!(!is_talk_combo(&s));
    }

    #[test]
    fn lowercase_round_trips() {
        let s = parse_shortcut("shift+alt+z").unwrap();
        assert_eq!(format_shortcut(&s), "Shift+Alt+Z");
    }

    #[test]
    fn bare_letter_is_unsafe() {
        let s = parse_shortcut("Z").unwrap();
        assert!(!is_safe_utility(&s));
    }

    #[test]
    fn rejects_empty_and_garbage() {
        assert!(parse_shortcut("").is_err());
        assert!(parse_shortcut("Shift+Nope").is_err());
        assert!(parse_shortcut("Ctrl+").is_err());
    }

    #[test]
    fn talk_combo_detected() {
        let s = parse_shortcut("Ctrl+Alt+Space").unwrap();
        assert!(is_talk_combo(&s));
        assert!(is_safe_utility(&s));
    }

    #[test]
    fn defaults_parse() {
        assert!(parse_shortcut(&default_paste_last()).is_ok());
        assert!(parse_shortcut(&default_copy_last()).is_ok());
        let a = parse_shortcut(&default_paste_last()).unwrap();
        let b = parse_shortcut(&default_copy_last()).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn format_round_trip_known() {
        let raw = "Ctrl+Shift+X";
        let s = parse_shortcut(raw).unwrap();
        assert_eq!(parse_shortcut(&format_shortcut(&s)).unwrap(), s);
    }
}
