//! Keep dictation paste in the app the user was typing in.
//!
//! Clicking a normal always-on-top window activates it and the caret leaves
//! the editor. Wispr / SpeakType avoid that by (1) not activating the overlay
//! (`WS_EX_NOACTIVATE`) and (2) re-activating the last foreign window before
//! the synthetic paste. We do both.

use std::ffi::c_void;

use tauri::Manager;

pub type Hwnd = isize;

const GWL_EXSTYLE: i32 = -20;
const WS_EX_NOACTIVATE: isize = 0x0800_0000;
const WS_EX_TOOLWINDOW: isize = 0x0000_0080;
const SWP_NOSIZE: u32 = 0x0001;
const SWP_NOMOVE: u32 = 0x0002;
const SWP_NOACTIVATE: u32 = 0x0010;
const SWP_FRAMECHANGED: u32 = 0x0020;
const HWND_TOPMOST: Hwnd = -1;
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

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentThreadId() -> u32;
}

fn to_ptr(h: Hwnd) -> *mut c_void {
    h as *mut c_void
}

pub fn is_live(h: Hwnd) -> bool {
    h != 0 && unsafe { IsWindow(to_ptr(h)) } != 0
}

pub fn foreground_hwnd() -> Option<Hwnd> {
    let h = unsafe { GetForegroundWindow() } as Hwnd;
    if is_live(h) { Some(h) } else { None }
}

pub fn hwnd_of(win: &tauri::WebviewWindow) -> Option<Hwnd> {
    win.hwnd().ok().map(|h| h.0 as Hwnd).filter(|h| *h != 0)
}

pub fn our_hwnds(app: &tauri::AppHandle) -> Vec<Hwnd> {
    ["main", "overlay"]
        .iter()
        .filter_map(|label| app.get_webview_window(label))
        .filter_map(|w| hwnd_of(&w))
        .collect()
}

/// True when this HWND is some other app (a valid paste target).
pub fn should_remember(foreground: Hwnd, ours: &[Hwnd]) -> bool {
    foreground != 0 && !ours.contains(&foreground)
}

/// Prefer the live foreground app; if we already stole focus, use the last
/// foreign window we saw.
pub fn pick_paste_target(
    foreground: Option<Hwnd>,
    last_saved: Option<Hwnd>,
    ours: &[Hwnd],
) -> Option<Hwnd> {
    if let Some(fg) = foreground {
        if should_remember(fg, ours) {
            return Some(fg);
        }
    }
    last_saved.filter(|h| should_remember(*h, ours))
}

/// Overlay can receive clicks without becoming the foreground window.
pub fn make_non_activating(win: &tauri::WebviewWindow) {
    let Some(h) = hwnd_of(win) else {
        return;
    };
    unsafe {
        let style = GetWindowLongPtrW(to_ptr(h), GWL_EXSTYLE);
        SetWindowLongPtrW(
            to_ptr(h),
            GWL_EXSTYLE,
            style | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
        );
        SetWindowPos(
            to_ptr(h),
            to_ptr(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
    }
}

/// Bring `target` back without clicking it (caret stays where it was).
/// Returns true when we actually switched away from our own window.
pub fn restore_hwnd(target: Option<Hwnd>) -> bool {
    let Some(h) = target.filter(|h| is_live(*h)) else {
        return false;
    };
    if foreground_hwnd() == Some(h) {
        return false;
    }
    force_foreground(h)
}

fn force_foreground(target: Hwnd) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    const MAIN: Hwnd = 1;
    const OVERLAY: Hwnd = 2;
    const EDITOR: Hwnd = 100;
    const BROWSER: Hwnd = 200;
    const OURS: &[Hwnd] = &[MAIN, OVERLAY];

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
        assert_eq!(
            pick_paste_target(Some(OVERLAY), Some(EDITOR), OURS),
            Some(EDITOR)
        );
        assert_eq!(
            pick_paste_target(Some(MAIN), Some(EDITOR), OURS),
            Some(EDITOR)
        );
    }

    #[test]
    fn paste_ignores_a_stale_dictflow_handle() {
        assert_eq!(pick_paste_target(Some(OVERLAY), Some(MAIN), OURS), None);
        assert_eq!(pick_paste_target(None, None, OURS), None);
    }
}
