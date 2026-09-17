//! Platform talk keys: Windows polls `GetAsyncKeyState`; macOS uses a
//! session CGEventTap (SpeakType-style, Accessibility — not HID / Input Monitoring).
//!
//! Utility shortcuts (paste-last / copy-last) are registered through the
//! shared Tauri global-shortcut plugin on both OSes.

use std::sync::{Mutex, OnceLock};

use tauri::Manager;
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut};

use crate::shortcuts;
use crate::state::AppState;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "macos")]
pub(crate) mod macos;

pub(crate) const HOTKEY: &str = "Ctrl+Alt+Space";

static EDGE_TX: OnceLock<tokio::sync::mpsc::UnboundedSender<TalkEdge>> = OnceLock::new();

/// Talk-key edge events forwarded to the async consumer.
#[derive(Debug, Clone, Copy)]
pub(crate) enum TalkEdge {
    Pressed,
    Released,
    /// Another key went down while the talk key was held (e.g. Ctrl+C on a
    /// Left-Ctrl talk key) — discard, don't transcribe. Mirrors SpeakType's
    /// modifier-combo cancel. Only applies to hotkey-owned takes.
    Cancel,
    /// Esc while any recording is live — dedicated discard (P1).
    Escape,
}

pub(crate) fn spawn_hotkey_thread(
    app: tauri::AppHandle,
    tx: tokio::sync::mpsc::UnboundedSender<TalkEdge>,
) {
    let _ = EDGE_TX.set(tx.clone());
    #[cfg(target_os = "windows")]
    windows::spawn(app, tx);

    #[cfg(target_os = "macos")]
    macos::ensure(app, tx);
}

/// Keep the Mac Fn tap in Fn / Fn+Ctrl / idle mode after settings save.
pub(crate) fn set_fn_mode(key: &str) {
    #[cfg(target_os = "macos")]
    macos::set_fn_mode(key);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = key;
    }
}

/// After TCC is granted, try again to attach CGEventTap / Fn tap.
pub(crate) fn retry_macos_taps(app: &tauri::AppHandle) {
    #[cfg(target_os = "macos")]
    if let Some(tx) = EDGE_TX.get() {
        macos::ensure(app.clone(), tx.clone());
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
    }
}

/// Whether the configured talk key can currently see events on this OS.
pub(crate) fn talk_key_live(key: &str) -> bool {
    if key == "CtrlAltSpace" {
        return true;
    }
    #[cfg(target_os = "windows")]
    {
        let _ = key;
        true
    }
    #[cfg(target_os = "macos")]
    {
        macos::taps_live_for(key)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = key;
        true
    }
}

/// Returns `Some(())` when `key` maps to a known platform talk-key trigger.
/// Used by `set_settings` to reject unknown keys before saving.
pub(crate) fn hotkey_vks(key: &str) -> Option<()> {
    #[cfg(target_os = "windows")]
    return windows::hotkey_vks(key).map(|_| ());
    #[cfg(target_os = "macos")]
    {
        // Fn and Fn+Ctrl are handled by the session tap, not a key list.
        if matches!(key, "Fn" | "FnCtrl") {
            return Some(());
        }
        macos::talk_keys(key).map(|_| ())
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    return if matches!(key, "LeftCtrl" | "RightCtrl" | "LeftAlt" | "RightAlt" | "F9") {
        Some(())
    } else {
        None
    };
}

/// Idempotently route the combo shortcut: registered only while the combo is
/// the configured talk key (re-registering a live hotkey fails as duplicate).
pub(crate) fn apply_hotkey_registration(app: &tauri::AppHandle) {
    let combo = app
        .state::<Mutex<AppState>>()
        .lock()
        .map(|s| s.settings.hotkey_key == "CtrlAltSpace")
        .unwrap_or(true);
    let shortcut: Shortcut = HOTKEY.parse().expect("hotkey parses");
    let gs = app.global_shortcut();
    let _ = gs.unregister(shortcut);
    if combo {
        if let Err(e) = gs.register(shortcut) {
            log::warn!("global shortcut registration failed ({HOTKEY}): {e}");
        }
    }
}

pub(crate) fn spec_to_shortcut(spec: &shortcuts::ShortcutSpec) -> Result<Shortcut, String> {
    let mut mods = Modifiers::empty();
    if spec.ctrl {
        mods |= Modifiers::CONTROL;
    }
    if spec.alt {
        mods |= Modifiers::ALT;
    }
    if spec.shift {
        mods |= Modifiers::SHIFT;
    }
    if spec.meta {
        mods |= Modifiers::SUPER;
    }
    let code = match spec.key.as_str() {
        "A" => Code::KeyA,
        "B" => Code::KeyB,
        "C" => Code::KeyC,
        "D" => Code::KeyD,
        "E" => Code::KeyE,
        "F" => Code::KeyF,
        "G" => Code::KeyG,
        "H" => Code::KeyH,
        "I" => Code::KeyI,
        "J" => Code::KeyJ,
        "K" => Code::KeyK,
        "L" => Code::KeyL,
        "M" => Code::KeyM,
        "N" => Code::KeyN,
        "O" => Code::KeyO,
        "P" => Code::KeyP,
        "Q" => Code::KeyQ,
        "R" => Code::KeyR,
        "S" => Code::KeyS,
        "T" => Code::KeyT,
        "U" => Code::KeyU,
        "V" => Code::KeyV,
        "W" => Code::KeyW,
        "X" => Code::KeyX,
        "Y" => Code::KeyY,
        "Z" => Code::KeyZ,
        "0" => Code::Digit0,
        "1" => Code::Digit1,
        "2" => Code::Digit2,
        "3" => Code::Digit3,
        "4" => Code::Digit4,
        "5" => Code::Digit5,
        "6" => Code::Digit6,
        "7" => Code::Digit7,
        "8" => Code::Digit8,
        "9" => Code::Digit9,
        "Space" => Code::Space,
        "Escape" => Code::Escape,
        "F1" => Code::F1,
        "F2" => Code::F2,
        "F3" => Code::F3,
        "F4" => Code::F4,
        "F5" => Code::F5,
        "F6" => Code::F6,
        "F7" => Code::F7,
        "F8" => Code::F8,
        "F9" => Code::F9,
        "F10" => Code::F10,
        "F11" => Code::F11,
        "F12" => Code::F12,
        other => return Err(format!("unsupported shortcut key: {other}")),
    };
    Ok(Shortcut::new(Some(mods), code))
}

pub(crate) fn validate_utility_key(raw: &str, other: &str) -> Result<shortcuts::ShortcutSpec, String> {
    let spec = shortcuts::parse_shortcut(raw)?;
    if !shortcuts::is_safe_utility(&spec) {
        return Err(format!("{raw} needs a modifier so it does not steal typing"));
    }
    if shortcuts::is_talk_combo(&spec) {
        return Err("that combo is reserved for the talk key".to_owned());
    }
    if raw.eq_ignore_ascii_case(other) {
        return Err("paste-last and copy-last must be different".to_owned());
    }
    Ok(spec)
}

pub(crate) fn apply_utility_shortcuts(app: &tauri::AppHandle) {
    let (paste_raw, copy_raw, old) = {
        let st = app.state::<Mutex<AppState>>();
        let Ok(mut s) = st.lock() else {
            return;
        };
        let old = std::mem::take(&mut s.utility_shortcuts);
        (s.settings.paste_last_key.clone(), s.settings.copy_last_key.clone(), old)
    };
    let gs = app.global_shortcut();
    for sc in old {
        let _ = gs.unregister(sc);
    }
    let mut next = Vec::new();
    for raw in [paste_raw, copy_raw] {
        match shortcuts::parse_shortcut(&raw).and_then(|s| spec_to_shortcut(&s)) {
            Ok(sc) => {
                if let Err(e) = gs.register(sc) {
                    log::warn!("utility shortcut registration failed ({raw}): {e}");
                } else {
                    next.push(sc);
                }
            }
            Err(e) => log::warn!("utility shortcut skipped ({raw}): {e}"),
        }
    }
    if let Ok(mut s) = app.state::<Mutex<AppState>>().lock() {
        s.utility_shortcuts = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{default_talk_key, normalize_talk_key};

    #[test]
    fn talk_key_is_known_on_this_platform() {
        #[cfg(target_os = "macos")]
        {
            assert!(hotkey_vks("Fn").is_some());
            assert!(hotkey_vks("FnCtrl").is_some());
            assert!(hotkey_vks("LeftCmd").is_some());
            assert!(hotkey_vks("ScrollLock").is_none());
            assert_eq!(normalize_talk_key("LeftWin"), "LeftCmd");
            assert_eq!(default_talk_key(), "Fn");
        }
        #[cfg(target_os = "windows")]
        {
            assert!(hotkey_vks("RightCtrl").is_some());
            assert!(hotkey_vks("LeftWin").is_some());
            assert!(hotkey_vks("Fn").is_none());
            assert_eq!(normalize_talk_key("Fn"), "RightCtrl");
            assert_eq!(default_talk_key(), "RightCtrl");
        }
    }
}
