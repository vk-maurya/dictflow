//! Clipboard + key-injection paste.
//!
//! Windows: enigo Ctrl+V from a worker thread.
//! macOS: SpeakType-style CGEvent Cmd+V on the main queue (AppKit / TSM
//! assert if we inject off-main). No enigo on Mac.

use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use tauri::Manager;

use crate::focus;
use crate::state::AppState;

pub(crate) fn remember_paste_target(app: &tauri::AppHandle) {
    let ours = focus::our_handles(app);
    let Some(fg) = focus::foreground_handle() else {
        return;
    };
    if !focus::should_remember(fg, &ours) {
        return;
    }
    if let Ok(mut s) = app.state::<Mutex<AppState>>().lock() {
        s.paste_target = Some(fg);
    }
}

pub(crate) fn paste_target_for(
    app: &tauri::AppHandle,
    last_saved: Option<focus::FocusHandle>,
) -> Option<focus::FocusHandle> {
    focus::pick_paste_target(focus::foreground_handle(), last_saved, &focus::our_handles(app))
}

pub(crate) fn paste_text(
    app: &tauri::AppHandle,
    text: &str,
    target: Option<focus::FocusHandle>,
) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::paste_text(app, text, target)
    }
    #[cfg(not(target_os = "macos"))]
    {
        windows::paste_text(text, target)
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::ffi::c_void;

    const VK_CMD: u16 = 0x37;
    const VK_V: u16 = 0x09;
    const CMD_MASK: u64 = 0x0010_0000;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventSourceCreate(state_id: u32) -> *mut c_void;
        fn CGEventCreateKeyboardEvent(source: *mut c_void, virtual_key: u16, key_down: bool) -> *mut c_void;
        fn CGEventSetFlags(event: *mut c_void, flags: u64);
        fn CGEventPost(tap: u32, event: *mut c_void);
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(cf: *const c_void);
    }

    fn post_cmd_v() -> Result<(), String> {
        unsafe {
            let src = CGEventSourceCreate(1);
            if src.is_null() {
                return Err("CGEventSourceCreate failed".into());
            }
            let post = |vk: u16, down: bool, flags: u64| -> Result<(), String> {
                let ev = CGEventCreateKeyboardEvent(src, vk, down);
                if ev.is_null() {
                    return Err("CGEventCreateKeyboardEvent failed".to_string());
                }
                CGEventSetFlags(ev, flags);
                CGEventPost(0, ev);
                CFRelease(ev);
                Ok(())
            };
            let r: Result<(), String> = (|| {
                post(VK_CMD, true, CMD_MASK)?;
                post(VK_V, true, CMD_MASK)?;
                post(VK_V, false, CMD_MASK)?;
                post(VK_CMD, false, 0)?;
                Ok(())
            })();
            CFRelease(src);
            r
        }
    }

    pub(crate) fn paste_text(
        app: &tauri::AppHandle,
        text: &str,
        target: Option<focus::FocusHandle>,
    ) -> anyhow::Result<()> {
        let mut cb = arboard::Clipboard::new().context("open clipboard")?;
        let previous = cb.get_text().ok();
        cb.set_text(text.to_owned()).context("set clipboard")?;
        std::thread::sleep(Duration::from_millis(60));

        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(0);
        let _ = app.run_on_main_thread(move || {
            if let Some(h) = target {
                if focus::is_live(h) && focus::foreground_handle() != Some(h) {
                    focus::restore_on_main_thread(h);
                    std::thread::sleep(Duration::from_millis(180));
                }
            }
            let _ = tx.send(post_cmd_v());
        });

        rx.recv_timeout(Duration::from_secs(5))
            .map_err(|_| anyhow::anyhow!("paste timed out waiting for main thread"))?
            .map_err(|e| anyhow::anyhow!("paste key injection failed: {e}"))?;

        std::thread::sleep(Duration::from_millis(350));
        if let Some(prev) = previous {
            if cb.get_text().map(|t| t == text).unwrap_or(false) {
                let _ = cb.set_text(prev);
            }
        }
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
mod windows {
    use super::*;

    pub(crate) fn paste_text(text: &str, target: Option<focus::FocusHandle>) -> anyhow::Result<()> {
        if focus::restore_handle(target) {
            std::thread::sleep(Duration::from_millis(180));
        }

        let mut cb = arboard::Clipboard::new().context("open clipboard")?;
        let previous = cb.get_text().ok();
        cb.set_text(text.to_owned()).context("set clipboard")?;
        std::thread::sleep(Duration::from_millis(120));

        use enigo::{Direction, Enigo, Key, Keyboard, Settings};
        let mut enigo = Enigo::new(&Settings::default()).context("init key injector")?;
        enigo.key(Key::Control, Direction::Press).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        enigo.key(Key::Unicode('v'), Direction::Click).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        enigo.key(Key::Control, Direction::Release).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        std::thread::sleep(Duration::from_millis(350));
        if let Some(prev) = previous {
            if cb.get_text().map(|t| t == text).unwrap_or(false) {
                let _ = cb.set_text(prev);
            }
        }
        Ok(())
    }
}
