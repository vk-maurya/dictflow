//! Keep dictation paste in the app the user was typing in.
//!
//! Clicking a normal always-on-top window activates it and the caret leaves
//! the editor. We avoid that by (1) not activating the overlay and (2)
//! re-activating the last foreign window before the synthetic paste.
//!
//! Platform abstractions:
//!   Windows — HWND-based via user32/kernel32 FFI (WS_EX_NOACTIVATE, etc.)
//!   macOS   — PID-based via NSWorkspace / NSRunningApplication (objc2-app-kit)

/// Platform-agnostic focus target.
/// Windows: HWND cast to isize.
/// macOS:   process PID cast to isize (isize fits all pid_t values).
pub type FocusHandle = isize;

pub fn is_live(h: FocusHandle) -> bool {
    if h == 0 {
        return false;
    }
    platform::is_live_impl(h)
}

pub fn foreground_handle() -> Option<FocusHandle> {
    platform::foreground_impl()
}

pub fn our_handles(app: &tauri::AppHandle) -> Vec<FocusHandle> {
    platform::our_handles_impl(app)
}

pub fn should_remember(foreground: FocusHandle, ours: &[FocusHandle]) -> bool {
    foreground != 0 && !ours.contains(&foreground)
}

pub fn pick_paste_target(
    foreground: Option<FocusHandle>,
    last_saved: Option<FocusHandle>,
    ours: &[FocusHandle],
) -> Option<FocusHandle> {
    if let Some(fg) = foreground {
        if should_remember(fg, ours) {
            return Some(fg);
        }
    }
    last_saved.filter(|h| should_remember(*h, ours))
}

/// Keep the overlay on every Mission Control Space. No-op on Windows
/// (topmost + tool window is the virtual-desktop equivalent).
pub fn spawn_overlay_space_follow(app: &tauri::AppHandle) {
    #[cfg(target_os = "macos")]
    platform::spawn_overlay_space_follow(app);
    #[cfg(not(target_os = "macos"))]
    let _ = app;
}

/// Make the overlay non-activating so clicks don't steal the caret.
pub fn make_non_activating(win: &tauri::WebviewWindow) {
    platform::make_non_activating_impl(win);
}

/// Bring `target` back without clicking it (caret stays where it was).
/// Returns true when we actually switched away from our own window.
#[cfg(not(target_os = "macos"))]
pub fn restore_handle(target: Option<FocusHandle>) -> bool {
    let Some(h) = target.filter(|h| is_live(*h)) else {
        return false;
    };
    if foreground_handle() == Some(h) {
        return false;
    }
    platform::restore_impl(h)
}

/// macOS only: perform the app-activation that Windows `restore_handle` would
/// do, but from a context guaranteed to be the **main thread**.
/// `NSRunningApplication::activateWithOptions` must run on the main thread;
/// calling it from a tokio worker raises an uncaught ObjC exception.
#[cfg(target_os = "macos")]
pub fn restore_on_main_thread(h: FocusHandle) {
    platform::restore_impl(h);
}

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------
#[cfg(target_os = "windows")]
mod platform {
    use super::FocusHandle;
    use std::ffi::c_void;
    use tauri::Manager;

    const GWL_EXSTYLE: i32 = -20;
    const WS_EX_NOACTIVATE: isize = 0x0800_0000;
    const WS_EX_TOOLWINDOW: isize = 0x0000_0080;
    const WS_EX_LAYERED: isize = 0x0008_0000;
    const SWP_NOSIZE: u32 = 0x0001;
    const SWP_NOMOVE: u32 = 0x0002;
    const SWP_NOACTIVATE: u32 = 0x0010;
    const SWP_FRAMECHANGED: u32 = 0x0020;
    const HWND_TOPMOST: FocusHandle = -1;
    const ASFW_ANY: u32 = 0xFFFF_FFFF;

    #[link(name = "user32")]
    extern "system" {
        fn GetForegroundWindow() -> *mut c_void;
        fn SetForegroundWindow(hwnd: *mut c_void) -> i32;
        fn IsWindow(hwnd: *mut c_void) -> i32;
        fn GetWindowLongPtrW(hwnd: *mut c_void, index: i32) -> isize;
        fn SetWindowLongPtrW(hwnd: *mut c_void, index: i32, value: isize) -> isize;
        fn SetWindowPos(
            hwnd: *mut c_void,
            insert_after: *mut c_void,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            flags: u32,
        ) -> i32;
        fn GetWindowThreadProcessId(hwnd: *mut c_void, pid: *mut u32) -> u32;
        fn AttachThreadInput(id_attach: u32, id_attach_to: u32, attach: i32) -> i32;
        fn AllowSetForegroundWindow(process_id: u32) -> i32;
    }

    #[link(name = "dwmapi")]
    extern "system" {
        fn DwmExtendFrameIntoClientArea(hwnd: *mut c_void, margins: *const i32) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThreadId() -> u32;
    }

    fn to_ptr(h: FocusHandle) -> *mut c_void {
        h as *mut c_void
    }

    pub fn is_live_impl(h: FocusHandle) -> bool {
        unsafe { IsWindow(to_ptr(h)) != 0 }
    }

    pub fn foreground_impl() -> Option<FocusHandle> {
        let h = unsafe { GetForegroundWindow() } as FocusHandle;
        if h != 0 && is_live_impl(h) { Some(h) } else { None }
    }

    pub fn hwnd_of_impl(win: &tauri::WebviewWindow) -> Option<FocusHandle> {
        win.hwnd().ok().map(|h| h.0 as FocusHandle).filter(|h| *h != 0)
    }

    pub fn our_handles_impl(app: &tauri::AppHandle) -> Vec<FocusHandle> {
        ["main", "overlay"]
            .iter()
            .filter_map(|label| app.get_webview_window(label))
            .filter_map(|w| hwnd_of_impl(&w))
            .collect()
    }

    pub fn make_non_activating_impl(win: &tauri::WebviewWindow) {
        let Some(h) = hwnd_of_impl(win) else { return; };
        unsafe {
            let style = GetWindowLongPtrW(to_ptr(h), GWL_EXSTYLE);
            SetWindowLongPtrW(
                to_ptr(h),
                GWL_EXSTYLE,
                style | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
            );
            SetWindowPos(
                to_ptr(h),
                to_ptr(HWND_TOPMOST),
                0, 0, 0, 0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
            // -1 margins: per-pixel alpha so the SpeakType pill is transparent
            // over the desktop, same as the macOS overlay.
            let margins = [-1i32; 4];
            let _ = DwmExtendFrameIntoClientArea(to_ptr(h), margins.as_ptr());
        }
    }

    pub fn restore_impl(target: FocusHandle) -> bool {
        unsafe {
            AllowSetForegroundWindow(ASFW_ANY);
            if SetForegroundWindow(to_ptr(target)) != 0 {
                return true;
            }
            let fg = GetForegroundWindow();
            let our_tid = GetCurrentThreadId();
            let fg_tid = GetWindowThreadProcessId(fg, std::ptr::null_mut());
            let target_tid = GetWindowThreadProcessId(to_ptr(target), std::ptr::null_mut());
            if fg_tid != 0 {
                AttachThreadInput(our_tid, fg_tid, 1);
            }
            if target_tid != 0 && target_tid != fg_tid {
                AttachThreadInput(our_tid, target_tid, 1);
            }
            let ok = SetForegroundWindow(to_ptr(target)) != 0;
            if fg_tid != 0 {
                AttachThreadInput(our_tid, fg_tid, 0);
            }
            if target_tid != 0 && target_tid != fg_tid {
                AttachThreadInput(our_tid, target_tid, 0);
            }
            ok
        }
    }
}

// ---------------------------------------------------------------------------
// macOS implementation
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
mod platform {
    use super::FocusHandle;

    pub fn is_live_impl(h: FocusHandle) -> bool {
        // signal 0 checks process existence without delivering a signal.
        // ESRCH = no such process; EPERM = exists but no permission.
        let result = unsafe { libc::kill(h as libc::pid_t, 0) };
        result == 0 || (result == -1 && unsafe { *libc::__error() } == libc::EPERM)
    }

    pub fn foreground_impl() -> Option<FocusHandle> {
        use objc2_app_kit::NSWorkspace;

        // SAFETY: NSWorkspace must be called from the main thread on macOS.
        // In DictFlow the hotkey thread calls remember_paste_target which calls
        // foreground_handle. That is safe because NSWorkspace is thread-safe for
        // sharedWorkspace + frontmostApplication reads.
        let workspace = unsafe { NSWorkspace::sharedWorkspace() };
        let app = unsafe { workspace.frontmostApplication() }?;
        let pid = unsafe { app.processIdentifier() };
        if pid > 0 { Some(pid as FocusHandle) } else { None }
    }

    pub fn our_handles_impl(_app: &tauri::AppHandle) -> Vec<FocusHandle> {
        // All DictFlow windows are in the same process.
        vec![std::process::id() as FocusHandle]
    }

    pub fn make_non_activating_impl(win: &tauri::WebviewWindow) {
        let Ok(ptr) = win.ns_window() else {
            return;
        };
        unsafe { configure_overlay_ns_window(ptr) };
    }

    unsafe fn configure_overlay_ns_window(ns_window: *mut std::ffi::c_void) {
        if ns_window.is_null() {
            return;
        }
        use objc2::msg_send;
        use objc2::runtime::AnyObject;

        let w = ns_window as *mut AnyObject;
        // Stay an NSWindow. Swapping the class to NSPanel and then sending
        // panel-only selectors aborts on macOS versions where the instance
        // sizes match but the object is still a window. CanJoinAllSpaces is
        // set below and again from the tray helper.
        let _: () = msg_send![w, setOpaque: false];
        let _: () = msg_send![w, setHasShadow: false];
        let cls = objc2::class!(NSColor);
        let clear: *mut AnyObject = msg_send![cls, clearColor];
        if !clear.is_null() {
            let _: () = msg_send![w, setBackgroundColor: clear];
        }
        let _: () = msg_send![w, setHidesOnDeactivate: false];
        let _: () = msg_send![w, setReleasedWhenClosed: false];
        // CanJoinAllSpaces | Transient | Stationary | IgnoresCycle | FullScreenAuxiliary
        let _: () = msg_send![w, setCollectionBehavior: overlay_space_behavior()];
        // NSStatusWindowLevel = 25, same band as menu extras — survives Space swipes.
        let _: () = msg_send![w, setLevel: 25i64];
        let _: () = msg_send![w, orderFrontRegardless];
    }

    /// CanJoinAllSpaces (1) | Transient (8) | Stationary (0x10) | IgnoresCycle (0x40)
    /// | FullScreenAuxiliary (0x100). Do not mix with MoveToActiveSpace.
    pub(crate) const fn overlay_space_behavior() -> u64 {
        0x1 | 0x8 | 0x10 | 0x40 | 0x100
    }

    pub fn spawn_overlay_space_follow(app: &tauri::AppHandle) {
        // Re-apply after Spaces exist; CanJoinAllSpaces is already set on the
        // overlay window. A second pass catches launch-time race with Mission
        // Control assigning the window to the first Space only.
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            crate::tray::apply_overlay_visibility(&app);
        });
    }

    pub fn restore_impl(target: FocusHandle) -> bool {
        use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};

        let target_pid = target as libc::pid_t;
        let app = unsafe { NSRunningApplication::runningApplicationWithProcessIdentifier(target_pid) };
        let Some(app) = app else { return false; };
        // NSApplicationActivateIgnoringOtherApps = 2
        // Deprecated in macOS 14 but still works; we target 12.0 minimum.
        #[allow(deprecated)]
        let _ = unsafe { app.activateWithOptions(NSApplicationActivationOptions(2)) };
        true
    }
}

// ---------------------------------------------------------------------------
// Fallback for other platforms (Linux, etc.) — no-ops
// ---------------------------------------------------------------------------
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
mod platform {
    use super::FocusHandle;

    pub fn is_live_impl(_h: FocusHandle) -> bool { false }
    pub fn foreground_impl() -> Option<FocusHandle> { None }
    pub fn our_handles_impl(_app: &tauri::AppHandle) -> Vec<FocusHandle> { vec![] }
    pub fn make_non_activating_impl(_win: &tauri::WebviewWindow) {}
    pub fn restore_impl(_target: FocusHandle) -> bool { false }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAIN: FocusHandle = 1;
    const OVERLAY: FocusHandle = 2;
    const EDITOR: FocusHandle = 100;
    const BROWSER: FocusHandle = 200;
    const OURS: &[FocusHandle] = &[MAIN, OVERLAY];

    #[test]
    fn remembers_only_foreign_windows() {
        assert!(should_remember(EDITOR, OURS));
        assert!(!should_remember(MAIN, OURS));
        assert!(!should_remember(OVERLAY, OURS));
        assert!(!should_remember(0, OURS));
    }

    #[test]
    fn paste_uses_current_app_when_we_did_not_steal_focus() {
        assert_eq!(
            pick_paste_target(Some(EDITOR), Some(BROWSER), OURS),
            Some(EDITOR)
        );
    }

    #[test]
    fn paste_falls_back_to_last_app_after_overlay_click() {
        assert_eq!(pick_paste_target(Some(OVERLAY), Some(EDITOR), OURS), Some(EDITOR));
        assert_eq!(pick_paste_target(Some(MAIN), Some(EDITOR), OURS), Some(EDITOR));
    }

    #[test]
    fn paste_ignores_a_stale_dictflow_handle() {
        assert_eq!(pick_paste_target(Some(OVERLAY), Some(MAIN), OURS), None);
        assert_eq!(pick_paste_target(None, None, OURS), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn overlay_joins_all_spaces_bits() {
        let bits = platform::overlay_space_behavior();
        assert_eq!(bits & 0x1, 0x1, "CanJoinAllSpaces");
        assert_eq!(bits & 0x100, 0x100, "FullScreenAuxiliary");
        assert_eq!(bits & 0x2, 0, "must not set MoveToActiveSpace");
    }
}
