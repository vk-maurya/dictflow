//! macOS talk keys via a CGEventTap on the main run loop.
//!
//! The Globe key opens Emoji & Symbols from Keyboard settings
//! (`com.apple.HIToolbox` `AppleFnUsageType`), before a session tap can see
//! the key. While Fn is the talk key we set that action to Do Nothing and
//! put the previous value back on quit. Input Monitoring lets the tap run
//! early enough to swallow the key as well.

use crate::focus;
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
const VK_F19: i64 = 0x50;
const VK_ESC: i64 = 53;
/// Apple Globe / Fn HID usage. Remapped to F19 while Fn is the talk key.
const GLOBE_HID_USAGE: u64 = 0xFF00_0000_03;
const F19_HID_USAGE: u64 = 0x0007_0000_006E;

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
    sync_globe_action(key);
}

pub fn restore_globe_action() {
    restore_globe();
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
/// 0 = none, 1 = session tap (Accessibility), 2 = HID tap (Input Monitoring).
static TAP_KIND: AtomicU8 = AtomicU8::new(0);
static HID_FAILED: AtomicBool = AtomicBool::new(false);
static GLOBE_REMAP_ON: AtomicBool = AtomicBool::new(false);
static GLOBE_REMAP_SAVED: Mutex<Option<Vec<HidMap>>> = Mutex::new(None);

#[derive(Clone, Copy)]
struct HidMap {
    src: u64,
    dst: u64,
}
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
    fn CGPreflightListenEventAccess() -> bool;
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
    fn CFStringCreateWithCString(
        alloc: *const std::ffi::c_void,
        c_str: *const i8,
        encoding: u32,
    ) -> *const std::ffi::c_void;
    fn CFNumberCreate(
        alloc: *const std::ffi::c_void,
        the_type: isize,
        value_ptr: *const std::ffi::c_void,
    ) -> *const std::ffi::c_void;
    fn CFNumberGetValue(
        number: *const std::ffi::c_void,
        the_type: isize,
        value_ptr: *mut std::ffi::c_void,
    ) -> bool;
    fn CFGetTypeID(cf: *const std::ffi::c_void) -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFPreferencesCopyValue(
        key: *const std::ffi::c_void,
        application_id: *const std::ffi::c_void,
        user_name: *const std::ffi::c_void,
        host_name: *const std::ffi::c_void,
    ) -> *const std::ffi::c_void;
    fn CFPreferencesSetValue(
        key: *const std::ffi::c_void,
        value: *const std::ffi::c_void,
        application_id: *const std::ffi::c_void,
        user_name: *const std::ffi::c_void,
        host_name: *const std::ffi::c_void,
    );
    fn CFPreferencesSynchronize(
        application_id: *const std::ffi::c_void,
        user_name: *const std::ffi::c_void,
        host_name: *const std::ffi::c_void,
    ) -> u8;
    fn CFNotificationCenterGetDistributedCenter() -> *mut std::ffi::c_void;
    fn CFNotificationCenterPostNotification(
        center: *mut std::ffi::c_void,
        name: *const std::ffi::c_void,
        object: *const std::ffi::c_void,
        user_info: *const std::ffi::c_void,
        deliver_immediately: u8,
    );
    static kCFPreferencesAnyApplication: *const std::ffi::c_void;
    static kCFPreferencesCurrentUser: *const std::ffi::c_void;
    static kCFPreferencesAnyHost: *const std::ffi::c_void;
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

fn swallow_fn_event(key: &str, keycode: i64) -> bool {
    matches!(key, "Fn" | "FnCtrl") && keycode == VK_FN
}

fn fn_owns_globe(key: &str) -> bool {
    matches!(key, "Fn" | "FnCtrl")
}

/// Previous `AppleFnUsageType` in each domain. `None` means the key was absent.
struct GlobeSaved {
    global: Option<i32>,
    hitoolbox: Option<i32>,
}

static GLOBE_SAVED: Mutex<Option<GlobeSaved>> = Mutex::new(None);

const FN_USAGE_DO_NOTHING: i32 = 0;
const CF_NUMBER_SINT32: isize = 3;
const CF_STRING_UTF8: u32 = 0x0800_0100;

fn sync_globe_action(key: &str) {
    if fn_owns_globe(key) {
        claim_globe();
    } else {
        restore_globe();
    }
}

fn claim_globe() {
    let mut saved = GLOBE_SAVED.lock().unwrap_or_else(|e| e.into_inner());
    if saved.is_some() {
        return;
    }
    *saved = Some(GlobeSaved {
        global: read_fn_usage(None),
        hitoolbox: read_fn_usage(Some(GLOBE_APP_DOMAIN)),
    });
    drop(saved);
    let global_ok = write_fn_usage(None, Some(FN_USAGE_DO_NOTHING));
    let toolbox_ok = write_fn_usage(Some(GLOBE_APP_DOMAIN), Some(FN_USAGE_DO_NOTHING));
    if global_ok || toolbox_ok {
        notify_fn_usage_changed();
        reload_keyboard_settings();
        log::info!("globe key action set to Do Nothing while Fn is the talk key");
    }
    install_globe_remap();
}

fn restore_globe() {
    let previous = {
        let mut saved = GLOBE_SAVED.lock().unwrap_or_else(|e| e.into_inner());
        saved.take()
    };
    remove_globe_remap();
    let Some(previous) = previous else {
        return;
    };
    let global_ok = write_fn_usage(None, previous.global);
    let toolbox_ok = write_fn_usage(Some(GLOBE_APP_DOMAIN), previous.hitoolbox);
    if global_ok || toolbox_ok {
        notify_fn_usage_changed();
        reload_keyboard_settings();
        log::info!("restored globe key action");
    }
}

const GLOBE_APP_DOMAIN: &[u8] = b"com.apple.HIToolbox\0";

fn fn_usage_key() -> *const std::ffi::c_void {
    unsafe {
        CFStringCreateWithCString(
            std::ptr::null(),
            b"AppleFnUsageType\0".as_ptr().cast(),
            CF_STRING_UTF8,
        )
    }
}

/// `None` is the global domain (`-g`). A name is an application domain such
/// as `com.apple.HIToolbox`, which is what Keyboard settings actually reads.
fn with_pref_domain(name: Option<&[u8]>, mut body: impl FnMut(*const std::ffi::c_void)) {
    unsafe {
        let (domain, owned) = if let Some(name) = name {
            (
                CFStringCreateWithCString(std::ptr::null(), name.as_ptr().cast(), CF_STRING_UTF8),
                true,
            )
        } else {
            (kCFPreferencesAnyApplication, false)
        };
        if domain.is_null() {
            return;
        }
        body(domain);
        if owned {
            CFRelease(domain);
        }
    }
}

fn read_fn_usage(domain_name: Option<&[u8]>) -> Option<i32> {
    let key = fn_usage_key();
    if key.is_null() {
        return None;
    }
    let mut found = None;
    with_pref_domain(domain_name, |domain| unsafe {
        let value = CFPreferencesCopyValue(
            key,
            domain,
            kCFPreferencesCurrentUser,
            kCFPreferencesAnyHost,
        );
        if value.is_null() || CFGetTypeID(value) != CFNumberGetTypeID() {
            if !value.is_null() {
                CFRelease(value);
            }
            return;
        }
        let mut n: i32 = 0;
        let ok = CFNumberGetValue(value, CF_NUMBER_SINT32, &mut n as *mut i32 as *mut _);
        CFRelease(value);
        if ok {
            found = Some(n);
        }
    });
    unsafe { CFRelease(key) };
    found
}

fn write_fn_usage(domain_name: Option<&[u8]>, value: Option<i32>) -> bool {
    let key = fn_usage_key();
    if key.is_null() {
        return false;
    }
    let mut ok = false;
    with_pref_domain(domain_name, |domain| unsafe {
        let number = if let Some(n) = value {
            CFNumberCreate(std::ptr::null(), CF_NUMBER_SINT32, &n as *const i32 as *const _)
        } else {
            std::ptr::null()
        };
        CFPreferencesSetValue(
            key,
            number,
            domain,
            kCFPreferencesCurrentUser,
            kCFPreferencesAnyHost,
        );
        ok = CFPreferencesSynchronize(
            domain,
            kCFPreferencesCurrentUser,
            kCFPreferencesAnyHost,
        ) != 0;
        if !number.is_null() {
            CFRelease(number);
        }
    });
    unsafe { CFRelease(key) };
    ok
}

fn notify_fn_usage_changed() {
    unsafe {
        let center = CFNotificationCenterGetDistributedCenter();
        let name = CFStringCreateWithCString(
            std::ptr::null(),
            b"com.apple.HIToolbox.AppleFnUsageTypeChanged\0"
                .as_ptr()
                .cast(),
            CF_STRING_UTF8,
        );
        if !center.is_null() && !name.is_null() {
            CFNotificationCenterPostNotification(center, name, std::ptr::null(), std::ptr::null(), 1);
        }
        if !name.is_null() {
            CFRelease(name);
        }
    }
}

fn listen_access() -> bool {
    unsafe { CGPreflightListenEventAccess() }
}

fn read_hid_mappings() -> Vec<HidMap> {
    let Ok(output) = std::process::Command::new("hidutil")
        .args(["property", "--get", "UserKeyMapping"])
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.trim();
    if text.is_empty() || text == "(null)" {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    value
        .get("UserKeyMapping")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    Some(HidMap {
                        src: item.get("HIDKeyboardModifierMappingSrc")?.as_u64()?,
                        dst: item.get("HIDKeyboardModifierMappingDst")?.as_u64()?,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn write_hid_mappings(maps: &[HidMap]) -> bool {
    let body = maps
        .iter()
        .map(|map| {
            format!(
                r#"{{"HIDKeyboardModifierMappingSrc":{},"HIDKeyboardModifierMappingDst":{}}}"#,
                map.src, map.dst
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let json = format!(r#"{{"UserKeyMapping":[{body}]}}"#);
    std::process::Command::new("hidutil")
        .args(["property", "--set", &json])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn install_globe_remap() {
    let mut saved = GLOBE_REMAP_SAVED.lock().unwrap_or_else(|e| e.into_inner());
    if saved.is_some() {
        return;
    }
    let previous: Vec<HidMap> = read_hid_mappings()
        .into_iter()
        .filter(|map| map.src != GLOBE_HID_USAGE)
        .collect();
    *saved = Some(previous.clone());
    drop(saved);
    let mut next = previous;
    next.push(HidMap {
        src: GLOBE_HID_USAGE,
        dst: F19_HID_USAGE,
    });
    if write_hid_mappings(&next) {
        GLOBE_REMAP_ON.store(true, Ordering::SeqCst);
        // The input agent keeps the old "show emoji" action in memory.
        let _ = std::process::Command::new("killall")
            .arg("TextInputMenuAgent")
            .status();
        let _ = std::process::Command::new("killall")
            .arg("CharacterPalette")
            .status();
        log::info!("Globe key remapped away from the emoji panel while DictFlow is open");
    } else {
        log::warn!("could not remap the Globe key");
    }
}

fn remove_globe_remap() {
    let previous = {
        let mut saved = GLOBE_REMAP_SAVED.lock().unwrap_or_else(|e| e.into_inner());
        saved.take()
    };
    let Some(previous) = previous else {
        return;
    };
    if write_hid_mappings(&previous) {
        GLOBE_REMAP_ON.store(false, Ordering::SeqCst);
        let _ = std::process::Command::new("killall")
            .arg("TextInputMenuAgent")
            .status();
        log::info!("restored Globe key mapping");
    }
}

fn reload_keyboard_settings() {
    // Writing the preference is not enough. macOS keeps the old Globe action
    // until this reload, which is why the emoji panel was still opening.
    let path = "/System/Library/PrivateFrameworks/SystemAdministration.framework/Resources/activateSettings";
    match std::process::Command::new(path).arg("-u").status() {
        Ok(status) if status.success() => {
            log::info!("keyboard settings reloaded");
        }
        Ok(status) => log::warn!("keyboard settings reload exited {status}"),
        Err(err) => log::warn!("keyboard settings reload failed: {err}"),
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
            // The emoji panel opens just after this event. Remember the real
            // app first, then close the panel so it cannot take the paste.
            focus::dismiss_character_palette();
            let app = app.clone();
            std::thread::spawn(move || {
                for wait in [90u64, 280] {
                    std::thread::sleep(std::time::Duration::from_millis(wait));
                    let again = app.clone();
                    let _ = again.run_on_main_thread(focus::dismiss_character_palette);
                }
            });
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
        if keycode == VK_ESC && recording {
            emit_edge(TalkEdge::Escape);
            return event;
        }
        if GLOBE_REMAP_ON.load(Ordering::Relaxed) && keycode == VK_F19 && fn_owns_globe(&key) {
            let ctrl_down = (flags & CTRL_MASK) != 0;
            let matched = key == "Fn" || (key == "FnCtrl" && ctrl_down);
            if matched {
                WAS_DOWN.with(|w| {
                    if !w.get() {
                        w.set(true);
                        emit_edge(TalkEdge::Pressed);
                    }
                });
            }
            return std::ptr::null_mut();
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

    if etype == KEY_UP
        && GLOBE_REMAP_ON.load(Ordering::Relaxed)
        && keycode == VK_F19
        && fn_owns_globe(&key)
    {
        WAS_DOWN.with(|w| {
            if w.get() {
                w.set(false);
                emit_edge(TalkEdge::Released);
            }
        });
        return std::ptr::null_mut();
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
    WAS_DOWN.with(|was| {
        if down == was.get() {
            return;
        }
        was.set(down);
        emit_edge(if down {
            TalkEdge::Pressed
        } else {
            TalkEdge::Released
        });
    });

    // Every Globe/Fn flagsChanged, including repeats. One leaked repeat is
    // enough for macOS to open Emoji & Symbols.
    if swallow_fn_event(&key, keycode) {
        std::ptr::null_mut()
    } else {
        event
    }
}

unsafe fn attach_tap() {
    // HID (0) runs before the emoji picker. Session (1) is the Accessibility fallback.
    let want_hid = listen_access();
    let want_kind: u8 = if want_hid { 2 } else { 1 };
    if LIVE.load(Ordering::Relaxed) && TAP_KIND.load(Ordering::Relaxed) == want_kind {
        SPAWNING.store(false, Ordering::SeqCst);
        return;
    }
    let old = TAP_PORT.swap(std::ptr::null_mut(), Ordering::SeqCst);
    if !old.is_null() {
        CGEventTapEnable(old, 0);
        LIVE.store(false, Ordering::SeqCst);
        TAP_KIND.store(0, Ordering::SeqCst);
    }
    // flagsChanged | keyDown | keyUp (F9 / Esc / combo-cancel, same as Windows).
    let mask = (1u64 << FLAGS_CHANGED) | (1u64 << KEY_DOWN) | (1u64 << KEY_UP);
    let location: u32 = if want_hid { 0 } else { 1 };
    let mut tap = CGEventTapCreate(location, 0, 0, mask, on_event, std::ptr::null_mut());
    let mut kind = want_kind;
    if tap.is_null() && want_hid {
        HID_FAILED.store(true, Ordering::SeqCst);
        tap = CGEventTapCreate(1, 0, 0, mask, on_event, std::ptr::null_mut());
        kind = 1;
    }
    if tap.is_null() {
        log::warn!("mac event tap failed — grant Accessibility, then Enable again");
        SPAWNING.store(false, Ordering::SeqCst);
        return;
    }
    TAP_PORT.store(tap, Ordering::SeqCst);
    let src = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
    CFRunLoopAddSource(CFRunLoopGetMain(), src, kCFRunLoopCommonModes);
    CGEventTapEnable(tap, 1);
    LIVE.store(true, Ordering::SeqCst);
    TAP_KIND.store(kind, Ordering::SeqCst);
    SPAWNING.store(false, Ordering::SeqCst);
    if kind == 2 {
        log::info!("mac hid tap attached (Fn swallowed before the emoji picker)");
    } else {
        log::info!("mac session tap attached (Fn / talk key, Accessibility)");
    }
}

pub fn ensure(app: tauri::AppHandle, tx: tokio::sync::mpsc::UnboundedSender<TalkEdge>) {
    let hotkey = app
        .state::<Mutex<AppState>>()
        .lock()
        .map(|s| s.settings.hotkey_key.clone())
        .unwrap_or_default();
    sync_globe_action(&hotkey);
    let initial_mode = mode_value(&hotkey);
    let _ = APP.set(app.clone());
    let _ = TX.set(tx);
    let _ = MODE.set(Arc::new(AtomicU8::new(initial_mode)));
    let upgrading = LIVE.load(Ordering::Relaxed)
        && TAP_KIND.load(Ordering::Relaxed) == 1
        && !HID_FAILED.load(Ordering::Relaxed)
        && unsafe { CGPreflightListenEventAccess() };
    if LIVE.load(Ordering::Relaxed) && !upgrading {
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

    #[test]
    fn fn_events_are_swallowed_and_globe_is_owned_by_fn() {
        assert!(swallow_fn_event("Fn", VK_FN));
        assert!(swallow_fn_event("FnCtrl", VK_FN));
        assert!(!swallow_fn_event("LeftCtrl", VK_FN));
        assert!(!swallow_fn_event("Fn", VK_LEFT_CTRL));
        assert!(fn_owns_globe("Fn"));
        assert!(fn_owns_globe("FnCtrl"));
        assert!(!fn_owns_globe("RightCtrl"));
        assert_eq!(GLOBE_HID_USAGE, 0xFF00_0000_03);
        assert_eq!(F19_HID_USAGE, 0x0007_0000_006E);
        assert_eq!(VK_F19, 0x50);
    }
}
