//! macOS talk keys via a **session** CGEventTap (SpeakType-style).
//!
//! SpeakType uses `kCGSessionEventTap` on the **main** run loop plus
//! Accessibility — not a HID tap (which needs Input Monitoring). Windows
//! still polls GetAsyncKeyState in [`super::windows`].

use crate::hotkey::TalkEdge;
use crate::paste;
use crate::state::AppState;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tauri::Manager;

/// Carbon virtual key codes (SpeakType `HotkeyOption`).
const VK_FN: i64 = 63;
const VK_LEFT_CMD: i64 = 55;
const VK_RIGHT_CMD: i64 = 54;
const VK_LEFT_CTRL: i64 = 59;
const VK_RIGHT_CTRL: i64 = 62;
const VK_LEFT_OPT: i64 = 58;
const VK_RIGHT_OPT: i64 = 61;
const VK_F9: i64 = 0x65;
const VK_ESC: i64 = 53;
const VK_F19: i64 = 0x50;

const FN_MASK: u64 = 0x0080_0000;
const CTRL_MASK: u64 = 0x0004_0000;
const CMD_MASK: u64 = 0x0010_0000;
const ALT_MASK: u64 = 0x0008_0000;

const FLAGS_CHANGED: u32 = 12;
const KEY_DOWN: u32 = 10;
const KEY_UP: u32 = 11;
const TAP_DISABLED_TIMEOUT: u32 = 0xFFFF_FFFE;
const TAP_DISABLED_USER: u32 = 0xFFFF_FFFF;
const KEYCODE_FIELD: u32 = 9; // kCGKeyboardEventKeycode

pub fn talk_keys(key: &str) -> Option<Vec<i64>> {
    match key {
        "Fn" | "FnCtrl" => Some(vec![VK_FN]),
        "LeftCtrl" => Some(vec![VK_LEFT_CTRL]),
        "RightCtrl" => Some(vec![VK_RIGHT_CTRL]),
        "LeftAlt" => Some(vec![VK_LEFT_OPT]),
        "RightAlt" => Some(vec![VK_RIGHT_OPT]),
        "LeftWin" | "LeftCmd" => Some(vec![VK_LEFT_CMD]),
        "RightWin" | "RightCmd" => Some(vec![VK_RIGHT_CMD]),
        "CtrlWin" | "CtrlCmd" => Some(vec![VK_LEFT_CTRL, VK_LEFT_CMD]),
        "F9" => Some(vec![VK_F9]),
        "ScrollLock" => None,
        _ => None,
    }
}

pub fn set_fn_mode(key: &str) {
    if let Some(m) = MODE.get() {
        m.store(mode_value(key), Ordering::Relaxed);
    }
}

fn mode_value(key: &str) -> u8 {
    match key {
        "Fn" => 1,
        "FnCtrl" => 2,
        _ => 0,
    }
}

pub fn taps_live_for(key: &str) -> bool {
    if key == "CtrlAltSpace" {
        return true;
    }
    LIVE.load(Ordering::Relaxed)
}

static LIVE: AtomicBool = AtomicBool::new(false);
static SPAWNING: AtomicBool = AtomicBool::new(false);
static MODE: OnceLock<Arc<AtomicU8>> = OnceLock::new();
static APP: OnceLock<tauri::AppHandle> = OnceLock::new();
static TX: OnceLock<tokio::sync::mpsc::UnboundedSender<TalkEdge>> = OnceLock::new();
static TAP_PORT: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

#[allow(non_camel_case_types)]
type CGEventRef = *mut std::ffi::c_void;
#[allow(non_camel_case_types)]
type CFMachPortRef = *mut std::ffi::c_void;
#[allow(non_camel_case_types)]
type CFRunLoopSourceRef = *mut std::ffi::c_void;

type TapCB = unsafe extern "C" fn(
    *mut std::ffi::c_void,
    u32,
    CGEventRef,
    *mut std::ffi::c_void,
) -> CGEventRef;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: TapCB,
        user_info: *mut std::ffi::c_void,
    ) -> CFMachPortRef;
    fn CGEventGetFlags(event: CGEventRef) -> u64;
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: u8);
    fn CGEventSourceCreate(state_id: u32) -> *mut std::ffi::c_void;
    fn CGEventCreateKeyboardEvent(
        source: *mut std::ffi::c_void,
        virtual_key: u16,
        key_down: bool,
    ) -> CGEventRef;
    fn CGEventPost(tap: u32, event: CGEventRef);
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFMachPortCreateRunLoopSource(
        allocator: *const std::ffi::c_void,
        port: CFMachPortRef,
        order: std::ffi::c_long,
    ) -> CFRunLoopSourceRef;
    fn CFRunLoopAddSource(
        rl: *mut std::ffi::c_void,
        source: CFRunLoopSourceRef,
        mode: *const std::ffi::c_void,
    );
    fn CFRunLoopGetMain() -> *mut std::ffi::c_void;
    fn CFRelease(cf: *const std::ffi::c_void);
    static kCFRunLoopCommonModes: *const std::ffi::c_void;
}

thread_local! {
    static WAS_DOWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static LAST_EDGE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// SpeakType: `event.keyCode == hotkey.keyCode` plus the matching modifier flag.
/// Unrelated flagsChanged events are ignored so Left Ctrl is not Right Ctrl.
fn talk_pressed(key: &str, flags: u64, keycode: i64) -> Option<bool> {
    let fn_down = (flags & FN_MASK) != 0;
    let ctrl_down = (flags & CTRL_MASK) != 0;
    let cmd_down = (flags & CMD_MASK) != 0;
    let alt_down = (flags & ALT_MASK) != 0;
    match key {
        "Fn" => (keycode == VK_FN).then_some(fn_down),
        "FnCtrl" => matches!(keycode, VK_FN | VK_LEFT_CTRL | VK_RIGHT_CTRL)
            .then_some(fn_down && ctrl_down),
        "LeftCtrl" => (keycode == VK_LEFT_CTRL).then_some(ctrl_down),
        "RightCtrl" => (keycode == VK_RIGHT_CTRL).then_some(ctrl_down),
        "LeftAlt" => (keycode == VK_LEFT_OPT).then_some(alt_down),
        "RightAlt" => (keycode == VK_RIGHT_OPT).then_some(alt_down),
        "LeftWin" | "LeftCmd" => (keycode == VK_LEFT_CMD).then_some(cmd_down),
        "RightWin" | "RightCmd" => (keycode == VK_RIGHT_CMD).then_some(cmd_down),
        "CtrlWin" | "CtrlCmd" => {
            matches!(
                keycode,
                VK_LEFT_CTRL | VK_RIGHT_CTRL | VK_LEFT_CMD | VK_RIGHT_CMD
            )
            .then_some(ctrl_down && cmd_down)
        }
        _ => None,
    }
}

fn is_talk_keycode(key: &str, keycode: i64) -> bool {
    talk_keys(key).is_some_and(|codes| codes.contains(&keycode))
}

fn frontmost_is_terminal() -> bool {
    use objc2_app_kit::NSWorkspace;

    let workspace = unsafe { NSWorkspace::sharedWorkspace() };
    let Some(app) = (unsafe { workspace.frontmostApplication() }) else {
        return false;
    };
    let Some(bid) = (unsafe { app.bundleIdentifier() }) else {
        return false;
    };
    matches!(
        bid.to_string().as_str(),
        "com.apple.Terminal"
            | "com.googlecode.iterm2"
            | "com.mitchellh.ghostty"
            | "io.alacritty"
            | "org.alacritty"
            | "net.kovidgoyal.kitty"
            | "com.github.wez.wezterm"
            | "dev.warp.Warp-Stable"
            | "dev.warp.Warp"
            | "co.zeit.hyper"
            | "org.tabby"
            | "com.microsoft.VSCode"
            | "com.vscodium"
            | "com.todesktop.230313mzl4w4u92"
    )
}

/// SpeakType: inject F19 so Globe/Fn does not open the emoji picker.
fn suppress_emoji_picker() {
    if frontmost_is_terminal() {
        return;
    }
    unsafe {
        let src = CGEventSourceCreate(1); // kCGEventSourceStateHIDSystemState
        if src.is_null() {
            return;
        }
        let down = CGEventCreateKeyboardEvent(src, VK_F19 as u16, true);
        let up = CGEventCreateKeyboardEvent(src, VK_F19 as u16, false);
        if !down.is_null() {
            CGEventPost(0, down);
            CFRelease(down);
        }
        if !up.is_null() {
            CGEventPost(0, up);
            CFRelease(up);
        }
        CFRelease(src);
    }
}

fn emit_edge(edge: TalkEdge) {
    let t = now_ms();
    let skip = LAST_EDGE.with(|last| {
        if t.saturating_sub(last.get()) < 50 {
            true
        } else {
            last.set(t);
            false
        }
    });
    if skip {
        return;
    }
    if matches!(edge, TalkEdge::Pressed) {
        if let Some(app) = APP.get() {
            paste::remember_paste_target(app);
        }
        let fn_mode = MODE.get().map(|m| m.load(Ordering::Relaxed)).unwrap_or(0);
        if fn_mode != 0 {
            suppress_emoji_picker();
        }
    }
    if let Some(tx) = TX.get() {
        let _ = tx.send(edge);
    }
}

fn reenable_tap() {
    let p = TAP_PORT.load(Ordering::Relaxed);
    if !p.is_null() {
        unsafe { CGEventTapEnable(p, 1) };
    }
}

unsafe extern "C" fn on_event(
    _proxy: *mut std::ffi::c_void,
    etype: u32,
    event: CGEventRef,
    _user_info: *mut std::ffi::c_void,
) -> CGEventRef {
    if etype == TAP_DISABLED_TIMEOUT || etype == TAP_DISABLED_USER {
        reenable_tap();
        return event;
    }

    let key = APP
        .get()
        .and_then(|app| {
            app.state::<Mutex<AppState>>()
                .lock()
                .ok()
                .map(|s| s.settings.hotkey_key.clone())
        })
        .unwrap_or_default();
    if key.is_empty() || key == "CtrlAltSpace" {
        return event;
    }

    let flags = CGEventGetFlags(event);
    let keycode = CGEventGetIntegerValueField(event, KEYCODE_FIELD);
    let recording = APP
        .get()
        .and_then(|app| app.state::<Mutex<AppState>>().lock().ok().map(|s| s.recording))
        .unwrap_or(false);

    if etype == KEY_DOWN {
        // SpeakType: ignore synthetic F19 or it cancels the take it just started.
        if keycode == VK_F19 {
            return event;
        }
        if keycode == VK_ESC && recording {
            emit_edge(TalkEdge::Escape);
            return event;
        }
        if key == "F9" && keycode == VK_F9 {
            WAS_DOWN.with(|w| {
                if !w.get() {
                    w.set(true);
                    emit_edge(TalkEdge::Pressed);
                }
            });
            return event;
        }
        let held = WAS_DOWN.with(|w| w.get());
        if held && !is_talk_keycode(&key, keycode) {
            emit_edge(TalkEdge::Cancel);
        }
        return event;
    }

    if etype == KEY_UP && key == "F9" && keycode == VK_F9 {
        WAS_DOWN.with(|w| {
            if w.get() {
                w.set(false);
                emit_edge(TalkEdge::Released);
            }
        });
        return event;
    }

    if etype != FLAGS_CHANGED {
        return event;
    }

    let Some(down) = talk_pressed(&key, flags, keycode) else {
        return event;
    };
    let mut swallow = false;
    WAS_DOWN.with(|was| {
        if down == was.get() {
            return;
        }
        was.set(down);
        swallow = matches!(key.as_str(), "Fn" | "FnCtrl") && keycode == VK_FN;
        emit_edge(if down {
            TalkEdge::Pressed
        } else {
            TalkEdge::Released
        });
    });

    // SpeakType: consume Fn flagsChanged so terminals do not print CSI garbage.
    if swallow {
        std::ptr::null_mut()
    } else {
        event
    }
}

unsafe fn attach_tap() {
    if LIVE.load(Ordering::Relaxed) {
        SPAWNING.store(false, Ordering::SeqCst);
        return;
    }
    // flagsChanged | keyDown | keyUp (F9 / Esc / combo-cancel, same as Windows).
    let mask = (1u64 << FLAGS_CHANGED) | (1u64 << KEY_DOWN) | (1u64 << KEY_UP);
    // kCGSessionEventTap = 1 (Accessibility). HID (0) needs Input Monitoring.
    let tap = CGEventTapCreate(1, 0, 0, mask, on_event, std::ptr::null_mut());
    if tap.is_null() {
        log::warn!("mac session tap failed — grant Accessibility, then Enable again");
        SPAWNING.store(false, Ordering::SeqCst);
        return;
    }
    TAP_PORT.store(tap, Ordering::SeqCst);
    let src = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
    // SpeakType attaches the tap to the main run loop, not a worker thread.
    CFRunLoopAddSource(CFRunLoopGetMain(), src, kCFRunLoopCommonModes);
    CGEventTapEnable(tap, 1);
    LIVE.store(true, Ordering::SeqCst);
    SPAWNING.store(false, Ordering::SeqCst);
    log::info!("mac session tap attached (Fn / talk key, Accessibility)");
}

pub fn ensure(app: tauri::AppHandle, tx: tokio::sync::mpsc::UnboundedSender<TalkEdge>) {
    let initial_mode = app
        .state::<Mutex<AppState>>()
        .lock()
        .map(|s| mode_value(&s.settings.hotkey_key))
        .unwrap_or(0);
    let _ = APP.set(app.clone());
    let _ = TX.set(tx);
    let _ = MODE.set(Arc::new(AtomicU8::new(initial_mode)));
    if LIVE.load(Ordering::Relaxed) {
        return;
    }
    if SPAWNING.swap(true, Ordering::SeqCst) {
        return;
    }

    // Must run on the AppKit main thread — session taps use the WindowServer
    // connection that lives there. Calling run_on_main_thread from setup()
    // (already on main) would deadlock, so branch on pthread_main_np.
    if unsafe { libc::pthread_main_np() != 0 } {
        unsafe { attach_tap() };
    } else {
        let _ = app.run_on_main_thread(|| unsafe { attach_tap() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speaktype_carbon_codes() {
        assert_eq!(talk_keys("Fn"), Some(vec![63]));
        assert_eq!(talk_keys("LeftCmd"), Some(vec![55]));
        assert_eq!(talk_keys("RightCmd"), Some(vec![54]));
        assert_eq!(talk_keys("LeftCtrl"), Some(vec![59]));
        assert_eq!(talk_keys("RightCtrl"), Some(vec![62]));
        assert_eq!(talk_keys("LeftAlt"), Some(vec![58]));
        assert_eq!(talk_keys("RightAlt"), Some(vec![61]));
        assert!(talk_keys("ScrollLock").is_none());
    }

    #[test]
    fn left_ctrl_is_not_right_ctrl() {
        assert_eq!(talk_pressed("LeftCtrl", CTRL_MASK, VK_LEFT_CTRL), Some(true));
        assert_eq!(talk_pressed("LeftCtrl", CTRL_MASK, VK_RIGHT_CTRL), None);
        assert_eq!(talk_pressed("Fn", FN_MASK, VK_FN), Some(true));
        assert_eq!(talk_pressed("Fn", FN_MASK, VK_LEFT_CTRL), None);
    }
}
