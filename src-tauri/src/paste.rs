//! Clipboard + key-injection paste.
//!
//! Windows: enigo Ctrl+V from a worker thread.
//! macOS: SpeakType-style CGEvent Cmd+V on the main queue (AppKit / TSM
//! assert if we inject off-main). No enigo on Mac.
//!
//! Refine-selection also posts Cmd/Ctrl+C, then pastes the rewrite and puts
//! back the clipboard snapshot taken before that copy.

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

/// Text or image that was on the clipboard before we touched it.
pub(crate) enum ClipboardSnapshot {
    Text(String),
    Image(arboard::ImageData<'static>),
    Empty,
}

impl ClipboardSnapshot {
    pub(crate) fn capture(cb: &mut arboard::Clipboard) -> Self {
        if let Ok(text) = cb.get_text() {
            return ClipboardSnapshot::Text(text);
        }
        if let Ok(image) = cb.get_image() {
            return ClipboardSnapshot::Image(image);
        }
        ClipboardSnapshot::Empty
    }

    pub(crate) fn write(&self, cb: &mut arboard::Clipboard) -> anyhow::Result<()> {
        match self {
            ClipboardSnapshot::Text(text) => {
                cb.set_text(text.clone()).context("restore clipboard text")
            }
            ClipboardSnapshot::Image(image) => {
                cb.set_image(image.clone()).context("restore clipboard image")
            }
            ClipboardSnapshot::Empty => cb.clear().context("clear clipboard"),
        }
    }
}

pub(crate) struct SelectionRead {
    pub snapshot: ClipboardSnapshot,
    pub sentinel: String,
    /// Clipboard text after the synthetic copy. `None` when it never changed.
    pub after: Option<String>,
}

pub(crate) fn capture_selection(app: &tauri::AppHandle) -> anyhow::Result<SelectionRead> {
    let mut cb = arboard::Clipboard::new().context("open clipboard")?;
    let snapshot = ClipboardSnapshot::capture(&mut cb);
    let sentinel = new_sentinel();
    if let Err(err) = cb.set_text(sentinel.clone()) {
        let _ = snapshot.write(&mut cb);
        return Err(err).context("set clipboard");
    }
    std::thread::sleep(Duration::from_millis(60));
    if let Err(err) = inject_copy(app) {
        let _ = snapshot.write(&mut cb);
        return Err(err);
    }
    let after = wait_for_clipboard_change(&mut cb, &sentinel);
    Ok(SelectionRead {
        snapshot,
        sentinel,
        after,
    })
}

pub(crate) fn restore_clipboard(snapshot: &ClipboardSnapshot) -> anyhow::Result<()> {
    let mut cb = arboard::Clipboard::new().context("open clipboard")?;
    snapshot.write(&mut cb)
}

pub(crate) fn paste_text(
    app: &tauri::AppHandle,
    text: &str,
    target: Option<focus::FocusHandle>,
) -> anyhow::Result<()> {
    paste_text_inner(app, text, target, None)
}

/// Paste `text`, then put `restore` back when the clipboard still holds `text`.
pub(crate) fn paste_replacing(
    app: &tauri::AppHandle,
    text: &str,
    target: Option<focus::FocusHandle>,
    restore: ClipboardSnapshot,
) -> anyhow::Result<()> {
    paste_text_inner(app, text, target, Some(restore))
}

fn paste_text_inner(
    app: &tauri::AppHandle,
    text: &str,
    target: Option<focus::FocusHandle>,
    restore: Option<ClipboardSnapshot>,
) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::paste_text(app, text, target, restore)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        windows::paste_text(text, target, restore)
    }
}

fn inject_copy(app: &tauri::AppHandle) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::inject_copy(app)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        windows::inject_copy()
    }
}

fn new_sentinel() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("\u{2060}dictflow-refine-{nanos}")
}

fn wait_for_clipboard_change(cb: &mut arboard::Clipboard, sentinel: &str) -> Option<String> {
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        if let Ok(text) = cb.get_text() {
            if text != sentinel {
                return Some(text);
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    match cb.get_text() {
        Ok(text) if text != sentinel => Some(text),
        _ => None,
    }
}

fn clipboard_still_has(cb: &mut arboard::Clipboard, text: &str) -> bool {
    cb.get_text().map(|current| current == text).unwrap_or(false)
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::ffi::c_void;

    use anyhow::Context;

    use crate::focus;

    const VK_CMD: u16 = 0x37;
    const VK_C: u16 = 0x08;
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

    fn post_cmd_key(vk: u16) -> Result<(), String> {
        unsafe {
            let src = CGEventSourceCreate(1);
            if src.is_null() {
                return Err("CGEventSourceCreate failed".into());
            }
            let post = |key: u16, down: bool, flags: u64| -> Result<(), String> {
                let ev = CGEventCreateKeyboardEvent(src, key, down);
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
                post(vk, true, CMD_MASK)?;
                post(vk, false, CMD_MASK)?;
                post(VK_CMD, false, 0)?;
                Ok(())
            })();
            CFRelease(src);
            r
        }
    }

    fn on_main(app: &tauri::AppHandle, op: impl FnOnce() -> Result<(), String> + Send + 'static) -> anyhow::Result<()> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(0);
        let _ = app.run_on_main_thread(move || {
            let _ = tx.send(op());
        });
        rx.recv_timeout(Duration::from_secs(5))
            .map_err(|_| anyhow::anyhow!("key injection timed out waiting for main thread"))?
            .map_err(|e| anyhow::anyhow!("key injection failed: {e}"))
    }

    pub(crate) fn inject_copy(app: &tauri::AppHandle) -> anyhow::Result<()> {
        on_main(app, || post_cmd_key(VK_C)).context("copy")
    }

    pub(crate) fn paste_text(
        app: &tauri::AppHandle,
        text: &str,
        target: Option<focus::FocusHandle>,
        restore: Option<super::ClipboardSnapshot>,
    ) -> anyhow::Result<()> {
        let mut cb = arboard::Clipboard::new().context("open clipboard")?;
        let previous = if restore.is_none() {
            cb.get_text().ok()
        } else {
            None
        };
        cb.set_text(text.to_owned()).context("set clipboard")?;
        std::thread::sleep(Duration::from_millis(60));

        let pasted = text.to_owned();
        if let Err(err) = on_main(app, move || {
            if let Some(h) = target {
                if focus::is_live(h) && focus::foreground_handle() != Some(h) {
                    focus::restore_on_main_thread(h);
                    std::thread::sleep(Duration::from_millis(180));
                }
            }
            post_cmd_key(VK_V)
        }) {
            if let Some(snapshot) = &restore {
                let _ = snapshot.write(&mut cb);
            } else if let Some(prev) = previous {
                let _ = cb.set_text(prev);
            }
            return Err(err);
        }

        std::thread::sleep(Duration::from_millis(350));
        if super::clipboard_still_has(&mut cb, &pasted) {
            if let Some(snapshot) = &restore {
                snapshot.write(&mut cb)?;
            } else if let Some(prev) = previous {
                let _ = cb.set_text(prev);
            }
        }
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
mod windows {
    use super::*;

    use anyhow::Context;

    use crate::focus;

    fn chord(letter: char) -> anyhow::Result<()> {
        use enigo::{Direction, Enigo, Key, Keyboard, Settings};
        let mut enigo = Enigo::new(&Settings::default()).context("init key injector")?;
        enigo
            .key(Key::Control, Direction::Press)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        enigo
            .key(Key::Unicode(letter), Direction::Click)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        enigo
            .key(Key::Control, Direction::Release)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        Ok(())
    }

    pub(crate) fn inject_copy() -> anyhow::Result<()> {
        // The refine hotkey includes Alt, so the physical Alt key is often
        // still held when this runs. A leftover Alt turns the copy into
        // Ctrl+Alt+C, which most targets ignore (and can drop the focused app
        // into its menu bar). Clear stuck modifiers, then send plain Ctrl+C.
        clear_stuck_modifiers();
        std::thread::sleep(Duration::from_millis(80));
        chord('c').context("copy")
    }

    fn clear_stuck_modifiers() {
        use enigo::{Direction, Enigo, Key, Keyboard, Settings};
        let Ok(mut enigo) = Enigo::new(&Settings::default()) else {
            return;
        };
        for key in [Key::Alt, Key::Shift, Key::Meta, Key::Control] {
            let _ = enigo.key(key, Direction::Release);
        }
    }

    pub(crate) fn paste_text(
        text: &str,
        target: Option<focus::FocusHandle>,
        restore: Option<super::ClipboardSnapshot>,
    ) -> anyhow::Result<()> {
        if focus::restore_handle(target) {
            std::thread::sleep(Duration::from_millis(180));
        }

        let mut cb = arboard::Clipboard::new().context("open clipboard")?;
        let previous = if restore.is_none() {
            cb.get_text().ok()
        } else {
            None
        };
        cb.set_text(text.to_owned()).context("set clipboard")?;
        std::thread::sleep(Duration::from_millis(120));

        if let Err(err) = chord('v') {
            if let Some(snapshot) = &restore {
                let _ = snapshot.write(&mut cb);
            } else if let Some(prev) = previous {
                let _ = cb.set_text(prev);
            }
            return Err(err);
        }
        std::thread::sleep(Duration::from_millis(350));
        if super::clipboard_still_has(&mut cb, text) {
            if let Some(snapshot) = &restore {
                snapshot.write(&mut cb)?;
            } else if let Some(prev) = previous {
                let _ = cb.set_text(prev);
            }
        }
        Ok(())
    }
}
