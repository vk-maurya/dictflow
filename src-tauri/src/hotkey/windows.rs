//! Windows talk-key polling via GetAsyncKeyState (user32).
//!
//! Bare modifier keys cannot be registered with RegisterHotKey, so we poll
//! every 10 ms. macOS uses CGEventTap instead — see [`super::macos`].

use std::sync::Mutex;
use std::time::Duration;

use tauri::Manager;

use crate::hotkey::TalkEdge;
use crate::paste;
use crate::state::AppState;

/// Virtual-key code for a configured talk key, or `None` for the combo.
pub fn hotkey_vk(key: &str) -> Option<i32> {
    match key {
        "LeftCtrl" => Some(0xA2),
        "RightCtrl" => Some(0xA3),
        "LeftAlt" => Some(0xA4),
        "RightAlt" => Some(0xA5),
        "LeftWin" => Some(0x5B),
        "RightWin" => Some(0x5C),
        "ScrollLock" => Some(0x91),
        "F9" => Some(0x78),
        _ => None,
    }
}

/// VK list for a talk key: singles → 1 element, `CtrlWin` → LeftCtrl+LeftWin.
pub fn hotkey_vks(key: &str) -> Option<Vec<i32>> {
    if key == "CtrlWin" {
        return Some(vec![0xA2, 0x5B]);
    }
    hotkey_vk(key).map(|vk| vec![vk])
}

#[link(name = "user32")]
extern "system" {
    fn GetAsyncKeyState(v_key: i32) -> i16;
    fn GetKeyboardState(lp_key_state: *mut u8) -> i32;
}

fn snapshot_keys() -> [u8; 256] {
    let mut buf = [0u8; 256];
    unsafe { GetKeyboardState(buf.as_mut_ptr()); }
    buf
}

pub fn spawn(app: tauri::AppHandle, tx: tokio::sync::mpsc::UnboundedSender<TalkEdge>) {
    std::thread::Builder::new()
        .name("dictflow-hotkey".to_owned())
        .spawn(move || {
            let mut was_down = false;
            let mut was_esc = false;
            let mut last_change = std::time::Instant::now();
            let mut last_esc = std::time::Instant::now();
            let mut prev_keys = snapshot_keys();
            loop {
                std::thread::sleep(Duration::from_millis(10));
                paste::remember_paste_target(&app);
                let (vks, recording) = app
                    .state::<Mutex<AppState>>()
                    .lock()
                    .map(|s| (hotkey_vks(&s.settings.hotkey_key), s.recording))
                    .unwrap_or((None, false));
                if recording {
                    let esc = unsafe { GetAsyncKeyState(0x1B) } < 0;
                    if esc && !was_esc && last_esc.elapsed() > Duration::from_millis(30) {
                        last_esc = std::time::Instant::now();
                        if tx.send(TalkEdge::Escape).is_err() { break; }
                    }
                    was_esc = esc;
                } else {
                    was_esc = false;
                }
                let Some(vks) = vks else {
                    was_down = false;
                    prev_keys = snapshot_keys();
                    continue;
                };
                let down = vks.iter().all(|vk| unsafe { GetAsyncKeyState(*vk) } < 0);
                if down != was_down && last_change.elapsed() > Duration::from_millis(30) {
                    was_down = down;
                    last_change = std::time::Instant::now();
                    let edge = if down { TalkEdge::Pressed } else { TalkEdge::Released };
                    if tx.send(edge).is_err() { break; }
                }
                if was_down {
                    let cur = snapshot_keys();
                    let intruded = cur.iter().enumerate().any(|(i, &b)| {
                        !vks.contains(&(i as i32))
                            && !(1..=6).contains(&i)
                            && b & 0x80 != 0
                            && prev_keys[i] & 0x80 == 0
                    });
                    prev_keys = cur;
                    if intruded && tx.send(TalkEdge::Cancel).is_err() { break; }
                } else {
                    prev_keys = snapshot_keys();
                }
            }
        })
        .expect("spawn hotkey thread");
}
