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

/// One display, in the same top-left coordinates as [`cursor_point`].
/// `scale` is 1 when those coordinates are already points (macOS). On Windows
/// the coordinates are physical pixels and `scale` is DPI / 96.
#[derive(Clone, Copy, Debug)]
pub struct DisplayRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub scale: f64,
}

pub fn cursor_point() -> Option<(i32, i32)> {
    platform::cursor_point_impl()
}

pub fn display_at(x: i32, y: i32) -> Option<DisplayRect> {
    platform::display_at_impl(x, y)
}

pub fn display_containing(x: i32, y: i32, displays: &[DisplayRect]) -> Option<DisplayRect> {
    displays.iter().copied().find(|d| {
        d.width > 0
            && d.height > 0
            && x >= d.x
            && y >= d.y
            && x < d.x.saturating_add(d.width)
            && y < d.y.saturating_add(d.height)
    })
}

/// Move the overlay and keep it on the desktop the user is looking at.
pub fn present_overlay(win: &tauri::WebviewWindow, x: i32, y: i32, w: i32, h: i32) {
    platform::present_overlay_impl(win, x, y, w, h);
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
        if should_remember(fg, ours) && !is_character_palette(fg) {
            return Some(fg);
        }
    }
    last_saved.filter(|h| should_remember(*h, ours) && !is_character_palette(*h))
}

pub fn is_character_palette(handle: FocusHandle) -> bool {
    #[cfg(target_os = "macos")]
    {
        platform::is_character_palette_impl(handle)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = handle;
        false
    }
}

/// Hide the macOS emoji panel. No-op on Windows.
pub fn dismiss_character_palette() {
    #[cfg(target_os = "macos")]
    platform::dismiss_character_palette_impl();
}

pub fn emoji_panel_on_screen() -> bool {
    #[cfg(target_os = "macos")]
    {
        platform::emoji_panel_on_screen_impl()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Keep the dictation bar on the screen and desktop under the pointer.
pub fn spawn_overlay_space_follow(app: &tauri::AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(280)).await;
            let hopped = app.clone();
            if app
                .run_on_main_thread(move || crate::tray::follow_overlay_screen(&hopped))
                .is_err()
            {
                break;
            }
        }
    });
}

/// Tell the overlay page when the pointer is on the pill. The webview of a window
/// that is not active gets no hover events, so the pointer is read natively.
pub fn spawn_overlay_hover_watch(app: &tauri::AppHandle) {
    use tauri::Emitter;
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut hovering = false;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(70)).await;
            let now = crate::tray::pointer_on_pill(hovering);
            if now != hovering {
                hovering = now;
                let _ = app.emit_to("overlay", "dictflow://overlay-hover", now);
            }
        }
    });
}

/// Make the overlay non-activating so clicks don't steal the caret.
pub fn make_non_activating(win: &tauri::WebviewWindow) {
    platform::make_non_activating_impl(win);
}

/// Click-through toggle for the hover gate (`hover.rs`).
///
/// This deliberately bypasses tauri/tao's `set_ignore_cursor_events`: tao
/// recomputes the *whole* window style from its flag cache on every flip,
/// restoring the caption/sysmenu chrome and dropping `WS_EX_LAYERED` — which
/// visually resurrects an opaque box around the pill after the patches in
/// `make_non_activating` are applied. On Windows we flip only the
/// WS_EX_TRANSPARENT bit; the LAYERED bit (set by `make_non_activating`)
/// stays on so click-through + transparency keep working together.
///
/// Returns false when the window/platform could not apply the change.
pub fn set_ignore_cursor_events(win: &tauri::WebviewWindow, ignore: bool) -> bool {
    platform::set_ignore_impl(win, ignore)
}

/// Clip the overlay to exactly the pill's footprint (`x, y, w, h` logical px,
/// window-relative). Windows only: clips drawing AND mouse input to the
/// rounded pill shape, so the transparent glass around the pill can never
/// block clicks (Wispr-Flow-style cutout). Returns false when unsupported —
/// the hover gate on such platforms keeps the old behavior.
pub fn set_window_region(win: &tauri::WebviewWindow, x: i32, y: i32, w: i32, h: i32) -> bool {
    platform::set_window_region_impl(win, x, y, w, h)
}

/// Repair tao-style drift on the overlay (tauri flag calls can resurrect
/// caption chrome / drop the layered bit). True = drifted and re-patched.
/// Cheap no-op when the style is already clean.
pub fn assert_overlay_style(win: &tauri::WebviewWindow) -> bool {
    platform::assert_overlay_style_impl(win)
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

    const GWL_STYLE: i32 = -16;
    const GWL_EXSTYLE: i32 = -20;
    const WS_POPUP: isize = 0x8000_0000u32 as isize;
    const WS_CAPTION: isize = 0x00C0_0000;
    const WS_SYSMENU: isize = 0x0008_0000;
    const WS_THICKFRAME: isize = 0x0004_0000;
    const WS_MINIMIZEBOX: isize = 0x0002_0000;
    const WS_MAXIMIZEBOX: isize = 0x0001_0000;
    const WS_EX_NOACTIVATE: isize = 0x0800_0000;
    const WS_EX_TOOLWINDOW: isize = 0x0000_0080;
    const WS_EX_LAYERED: isize = 0x0008_0000;
    const WS_EX_TRANSPARENT: isize = 0x0000_0020;
    const SWP_NOSIZE: u32 = 0x0001;
    const SWP_NOMOVE: u32 = 0x0002;
    const SWP_NOACTIVATE: u32 = 0x0010;
    const SWP_FRAMECHANGED: u32 = 0x0020;
    const HWND_TOPMOST: FocusHandle = -1;
    const ASFW_ANY: u32 = 0xFFFF_FFFF;
    const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
    const DWMWCP_DONOTROUND: u32 = 1;
    const DWMWA_BORDER_COLOR: u32 = 34;
    const DWMWA_CAPTION_COLOR: u32 = 35;
    const DWMWA_COLOR_NONE: u32 = 0xFFFF_FFFE;
    const DWMWA_SYSTEMBACKDROP_TYPE: u32 = 38;
    const DWMSBT_NONE: u32 = 1;

    /// Drop caption / sysmenu / resize-border so Win11 does not paint a
    /// tool-window bubble (outline + close button) over the overlay pill.
    pub(crate) fn overlay_window_style(style: isize) -> isize {
        (style
            & !WS_CAPTION
            & !WS_THICKFRAME
            & !WS_SYSMENU
            & !WS_MINIMIZEBOX
            & !WS_MAXIMIZEBOX)
            | WS_POPUP
    }

    pub(crate) fn overlay_window_exstyle(style: isize) -> isize {
        style | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_LAYERED
    }

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
        // Window regions: device-pixel coords relative to the window; used to
        // clip the overlay to the pill shape for both drawing and input.
        fn CreateRoundRectRgn(
            left: i32,
            top: i32,
            right: i32,
            bottom: i32,
            ellipse_w: i32,
            ellipse_h: i32,
        ) -> *mut c_void;
        fn SetWindowRgn(hwnd: *mut c_void, hrgn: *mut c_void, redraw: i32) -> i32;
        fn DeleteObject(obj: *mut c_void) -> i32;
        fn GetWindowThreadProcessId(hwnd: *mut c_void, pid: *mut u32) -> u32;
        fn AttachThreadInput(id_attach: u32, id_attach_to: u32, attach: i32) -> i32;
        fn AllowSetForegroundWindow(process_id: u32) -> i32;
        fn GetCursorPos(point: *mut WinPoint) -> i32;
        fn MonitorFromPoint(point: WinPoint, flags: u32) -> *mut c_void;
        fn GetMonitorInfoW(monitor: *mut c_void, info: *mut MonitorInfo) -> i32;
    }

    #[link(name = "shcore")]
    extern "system" {
        fn GetDpiForMonitor(
            monitor: *mut c_void,
            kind: i32,
            dpi_x: *mut u32,
            dpi_y: *mut u32,
        ) -> i32;
    }

    #[repr(C)]
    struct MonitorInfo {
        cb_size: u32,
        rc_monitor: WinRect,
        rc_work: WinRect,
        flags: u32,
    }

    #[repr(C)]
    struct WinPoint {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    struct WinRect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[link(name = "dwmapi")]
    extern "system" {
        fn DwmExtendFrameIntoClientArea(hwnd: *mut c_void, margins: *const i32) -> i32;
        fn DwmSetWindowAttribute(
            hwnd: *mut c_void,
            attr: u32,
            value: *const c_void,
            size: u32,
        ) -> i32;
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

    pub fn cursor_point_impl() -> Option<(i32, i32)> {
        unsafe {
            let mut cursor = WinPoint { x: 0, y: 0 };
            if GetCursorPos(&mut cursor) != 0 {
                return Some((cursor.x, cursor.y));
            }
        }
        None
    }

    pub fn display_at_impl(x: i32, y: i32) -> Option<super::DisplayRect> {
        unsafe {
            let monitor = MonitorFromPoint(WinPoint { x, y }, 0);
            if monitor.is_null() {
                return None;
            }
            let mut info = MonitorInfo {
                cb_size: std::mem::size_of::<MonitorInfo>() as u32,
                rc_monitor: WinRect {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                },
                rc_work: WinRect {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                },
                flags: 0,
            };
            if GetMonitorInfoW(monitor, &mut info) == 0 {
                return None;
            }
            let mut dpi_x = 96u32;
            let mut dpi_y = 96u32;
            let scale = if GetDpiForMonitor(monitor, 0, &mut dpi_x, &mut dpi_y) == 0 && dpi_x > 0 {
                dpi_x as f64 / 96.0
            } else {
                1.0
            };
            let width = info.rc_monitor.right - info.rc_monitor.left;
            let height = info.rc_monitor.bottom - info.rc_monitor.top;
            if width <= 0 || height <= 0 {
                return None;
            }
            Some(super::DisplayRect {
                x: info.rc_monitor.left,
                y: info.rc_monitor.top,
                width,
                height,
                scale,
            })
        }
    }

    pub fn present_overlay_impl(win: &tauri::WebviewWindow, x: i32, y: i32, w: i32, h: i32) {
        let Some(hwnd) = hwnd_of_impl(win) else {
            return;
        };
        const SWP_SHOWWINDOW: u32 = 0x0040;
        unsafe {
            SetWindowPos(
                to_ptr(hwnd),
                to_ptr(HWND_TOPMOST),
                x,
                y,
                w,
                h,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
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

    unsafe fn dwm_attr(hwnd: *mut c_void, attr: u32, value: u32) {
        let _ = DwmSetWindowAttribute(hwnd, attr, (&value as *const u32).cast(), 4);
    }

    pub fn make_non_activating_impl(win: &tauri::WebviewWindow) {
        let Some(h) = hwnd_of_impl(win) else { return; };
        let hwnd = to_ptr(h);
        unsafe {
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
            SetWindowLongPtrW(hwnd, GWL_STYLE, overlay_window_style(style));
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, overlay_window_exstyle(ex));
            // Win11 still draws a rounded mica card + 1px border on popups
            // unless these DWM attributes are cleared.
            dwm_attr(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND);
            dwm_attr(hwnd, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE);
            dwm_attr(hwnd, DWMWA_CAPTION_COLOR, DWMWA_COLOR_NONE);
            dwm_attr(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, DWMSBT_NONE);
            // -1 margins: per-pixel alpha so the 268x56 window is invisible
            // around the pill. Must run after caption bits are stripped,
            // otherwise DWM paints the close button into the glass frame.
            let margins = [-1i32; 4];
            let _ = DwmExtendFrameIntoClientArea(hwnd, margins.as_ptr());
            SetWindowPos(
                hwnd,
                to_ptr(HWND_TOPMOST),
                0, 0, 0, 0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
    }

    /// Flip only WS_EX_TRANSPARENT so the hover gate can switch the whole
    /// window between pass-through and input without disturbing the
    /// chrome-stripped, layered style `make_non_activating` installed.
    pub fn set_ignore_impl(win: &tauri::WebviewWindow, ignore: bool) -> bool {
        let Some(h) = hwnd_of_impl(win) else { return false; };
        let hwnd = to_ptr(h);
        unsafe {
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            let next = if ignore {
                ex | WS_EX_TRANSPARENT
            } else {
                ex & !WS_EX_TRANSPARENT
            };
            if next != ex {
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, next);
            }
            // SetWindowLongPtrW's return value can legitimately be 0, so
            // verify by reading the style back.
            GetWindowLongPtrW(hwnd, GWL_EXSTYLE) == next
        }
    }

    /// Clip the overlay to the pill's rounded shape (`x, y, w, h` logical,
    /// window-relative). The window region limits BOTH drawing and mouse
    /// hit-testing: outside the region the OS treats the window as if it does
    /// not exist, so clicks fall through to the desktop/apps below with zero
    /// toggling or flicker — the Wispr Flow pattern.
    pub fn set_window_region_impl(win: &tauri::WebviewWindow, x: i32, y: i32, w: i32, h: i32) -> bool {
        let Some(handle) = hwnd_of_impl(win) else { return false; };
        let hwnd = to_ptr(handle);
        let Ok(scale) = win.scale_factor() else { return false; };
        let s = scale as f64;
        // Small logical halo so the CSS border/box-shadow is not shaved off.
        const HALO: f64 = 2.0;
        let rx = ((x as f64 - HALO) * s).round() as i32;
        let ry = ((y as f64 - HALO) * s).round() as i32;
        let rw = ((w as f64 + 2.0 * HALO) * s).round() as i32;
        let rh = ((h as f64 + 2.0 * HALO) * s).round() as i32;
        if rw <= 0 || rh <= 0 {
            return false;
        }
        // Fully rounded pill ends: corner diameter = region height.
        let r = rh / 2;
        unsafe {
            let hrgn = CreateRoundRectRgn(rx, ry, rx + rw, ry + rh, r, r);
            if hrgn.is_null() {
                return false;
            }
            // The OS owns the region after a successful SetWindowRgn — do not
            // delete it then.
            if SetWindowRgn(hwnd, hrgn, 1) == 0 {
                DeleteObject(hrgn);
                return false;
            }
        }
        true
    }

    /// Style health check for the overlay: tao may recompute the full window
    /// style later (any tauri flag call), resurrecting caption chrome and
    /// dropping the layered bit — visible as an opaque box + title around the
    /// pill. Detect the drift and re-run the `make_non_activating` patches.
    /// Returns true when repairs were applied.
    pub fn assert_overlay_style_impl(win: &tauri::WebviewWindow) -> bool {
        let Some(h) = hwnd_of_impl(win) else { return false; };
        let hwnd = to_ptr(h);
        unsafe {
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            let clean = (style & (WS_CAPTION | WS_SYSMENU | WS_THICKFRAME)) == 0
                && (ex & WS_EX_LAYERED) != 0
                && (ex & WS_EX_NOACTIVATE) != 0
                && (ex & WS_EX_TOOLWINDOW) != 0;
            if clean {
                return false;
            }
        }
        make_non_activating_impl(win);
        true
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

    fn mouse_point() -> Option<(i32, i32)> {
        use std::ffi::c_void;

        #[repr(C)]
        struct CgPoint {
            x: f64,
            y: f64,
        }
        #[link(name = "CoreGraphics", kind = "framework")]
        extern "C" {
            fn CGEventCreate(source: *const c_void) -> *mut c_void;
            fn CGEventGetLocation(event: *mut c_void) -> CgPoint;
        }
        #[link(name = "CoreFoundation", kind = "framework")]
        extern "C" {
            fn CFRelease(cf: *const c_void);
        }
        unsafe {
            let event = CGEventCreate(std::ptr::null());
            if event.is_null() {
                return None;
            }
            let point = CGEventGetLocation(event);
            CFRelease(event);
            Some((point.x.round() as i32, point.y.round() as i32))
        }
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

    /// On macOS tauri's setter is a plain `NSWindow.setIgnoresMouseEvents`
    /// call — it does not rewrite any other window property, so it is safe
    /// to use for the hover gate.
    pub fn set_ignore_impl(win: &tauri::WebviewWindow, ignore: bool) -> bool {
        win.set_ignore_cursor_events(ignore).is_ok()
    }

    /// macOS has no SetWindowRgn equivalent; the hover gate handles it.
    pub fn set_window_region_impl(
        _win: &tauri::WebviewWindow,
        _x: i32,
        _y: i32,
        _w: i32,
        _h: i32,
    ) -> bool {
        false
    }

    /// Nothing recycles macOS styles the way tao does; never drifts.
    pub fn assert_overlay_style_impl(_win: &tauri::WebviewWindow) -> bool {
        false
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
        let _: () = msg_send![w, setCanHide: false];
        let _: () = msg_send![w, setReleasedWhenClosed: false];
        // Collection behavior is owned by present_overlay. Setting it here on
        // every follow tick puts the window back on the ordinary desktop.
        // NSScreenSaverWindowLevel. Popup level 101 is still covered by the
        // front app, and Tauri's always-on-top level (3) stays on the desktop
        // where the window was created.
        let _: () = msg_send![w, setLevel: OVERLAY_WINDOW_LEVEL];
        let _: () = msg_send![w, orderFrontRegardless];
        let _: () = msg_send![w, setLevel: OVERLAY_WINDOW_LEVEL];
    }

    /// NSScreenSaverWindowLevel. High enough to stay above normal app windows
    /// on every desktop, which is the level Wispr uses for its visible bar.
    const OVERLAY_WINDOW_LEVEL: i64 = 1000;

    /// Display the overlay was last presented on, as a key from its origin.
    static LAST_DISPLAY: std::sync::atomic::AtomicI64 =
        std::sync::atomic::AtomicI64::new(i64::MIN);

    fn order_overlay(window: &objc2::runtime::AnyObject) {
        use objc2::msg_send;
        unsafe {
            let _: () = msg_send![window, setLevel: OVERLAY_WINDOW_LEVEL];
            let _: () = msg_send![window, orderFrontRegardless];
            let _: () = msg_send![window, setLevel: OVERLAY_WINDOW_LEVEL];
        }
    }

    /// CanJoinAllSpaces | IgnoresCycle | FullScreenAuxiliary | CanJoinAllApplications.
    ///
    /// Stationary is omitted: on this Mac it pinned the bar to the desktop
    /// where DictFlow opened, so the frame could sit on screen 2 while the
    /// window stayed off the desktop the user was looking at.
    /// Transient is omitted because it hides the bar whenever DictFlow is not
    /// the front app.
    pub(crate) const fn overlay_space_behavior() -> u64 {
        0x1 | 0x40 | 0x100 | (1 << 18)
    }

    pub fn cursor_point_impl() -> Option<(i32, i32)> {
        mouse_point()
    }

    pub fn display_at_impl(x: i32, y: i32) -> Option<super::DisplayRect> {
        super::display_containing(x, y, &active_displays())
    }

    fn active_displays() -> Vec<super::DisplayRect> {
        use objc2_foundation::CGRect;

        #[link(name = "CoreGraphics", kind = "framework")]
        extern "C" {
            fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
            fn CGDisplayBounds(display: u32) -> CGRect;
        }
        unsafe {
            let mut ids = [0u32; 16];
            let mut count = 0u32;
            if CGGetActiveDisplayList(16, ids.as_mut_ptr(), &mut count) != 0 {
                return Vec::new();
            }
            (0..count as usize)
                .filter_map(|index| {
                    let bounds = CGDisplayBounds(ids[index]);
                    let width = bounds.size.width.round() as i32;
                    let height = bounds.size.height.round() as i32;
                    if width <= 0 || height <= 0 {
                        return None;
                    }
                    Some(super::DisplayRect {
                        x: bounds.origin.x.round() as i32,
                        y: bounds.origin.y.round() as i32,
                        width,
                        height,
                        scale: 1.0,
                    })
                })
                .collect()
        }
    }

    pub fn present_overlay_impl(win: &tauri::WebviewWindow, x: i32, y: i32, w: i32, h: i32) {
        let Ok(ptr) = win.ns_window() else {
            return;
        };
        if ptr.is_null() || w <= 0 || h <= 0 {
            return;
        }
        use objc2::msg_send;
        use objc2::runtime::AnyObject;
        use objc2_foundation::{CGPoint, CGRect, CGSize};

        #[link(name = "CoreGraphics", kind = "framework")]
        extern "C" {
            fn CGMainDisplayID() -> u32;
            fn CGDisplayBounds(display: u32) -> CGRect;
        }

        unsafe {
            let window = &*(ptr as *const AnyObject);
            let primary = CGDisplayBounds(CGMainDisplayID());
            let origin_y = primary.size.height - f64::from(y) - f64::from(h);
            let rect = CGRect::new(
                CGPoint::new(f64::from(x), origin_y),
                CGSize::new(f64::from(w), f64::from(h)),
            );
            let current: CGRect = msg_send![window, frame];
            let moved = (current.origin.x - rect.origin.x).abs() > 1.5
                || (current.origin.y - rect.origin.y).abs() > 1.5
                || (current.size.width - rect.size.width).abs() > 1.5
                || (current.size.height - rect.size.height).abs() > 1.5;
            let _: () = msg_send![window, setHidesOnDeactivate: false];
            let _: () = msg_send![window, setCanHide: false];
            // A window carried to another display keeps the space membership it
            // had on the old one. Taking it off screen and clearing the behavior
            // first makes the window server rebuild that membership from the
            // all-spaces flag on the new display.
            let display_key = super::display_at(x + w / 2, y + h / 2)
                .map(|d| i64::from(d.x) * 100_000 + i64::from(d.y))
                .unwrap_or(0);
            let previous = LAST_DISPLAY.swap(display_key, std::sync::atomic::Ordering::AcqRel);
            if previous != display_key {
                let _: () = msg_send![window, orderOut: std::ptr::null::<AnyObject>()];
                let _: () = msg_send![window, setCollectionBehavior: 0u64];
            }
            if moved {
                let _: () = msg_send![window, setFrame: rect display: objc2::runtime::Bool::NO];
            }
            let _: () = msg_send![window, setCollectionBehavior: overlay_space_behavior()];
            order_overlay(window);
            // CanJoinAllSpaces alone leaves the bar off the desktop or full-screen
            // app that is showing on the other display. Adding the window to every
            // desktop that is showing now puts it on top of whatever is open there.
            let window_number: isize = msg_send![window, windowNumber];
            if window_number > 0 {
                // Asking the window server where the window lives makes it settle
                // the window's pending move before the join below is applied.
                let _active: bool = msg_send![window, isOnActiveSpace];
                join_showing_spaces(window_number);
                order_overlay(window);
            }
        }
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFNumberCreate(
            allocator: *const std::ffi::c_void,
            kind: isize,
            value: *const std::ffi::c_void,
        ) -> *const std::ffi::c_void;
        fn CFArrayCreate(
            allocator: *const std::ffi::c_void,
            values: *const *const std::ffi::c_void,
            count: isize,
            callbacks: *const std::ffi::c_void,
        ) -> *const std::ffi::c_void;
        fn CFRelease(cf: *const std::ffi::c_void);        static kCFTypeArrayCallBacks: CFArrayCallBacks;
    }

    #[repr(C)]
    struct CFArrayCallBacks {
        version: isize,
        retain: *const std::ffi::c_void,
        release: *const std::ffi::c_void,
        copy_description: *const std::ffi::c_void,
        equal: *const std::ffi::c_void,
    }

    struct SpaceFns {
        conn: unsafe extern "C" fn() -> i32,
        copy: unsafe extern "C" fn(i32, i32) -> *const std::ffi::c_void,
        add: unsafe extern "C" fn(i32, *const std::ffi::c_void, *const std::ffi::c_void),
    }

    /// Private SkyLight calls. A missing symbol turns the join into a no-op.
    fn space_fns() -> Option<&'static SpaceFns> {
        use std::ffi::{c_void, CString};
        use std::sync::OnceLock;

        fn load() -> Option<SpaceFns> {
            let path = CString::new(
                "/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight",
            )
            .ok()?;
            let lib = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_LAZY) };
            if lib.is_null() {
                return None;
            }
            unsafe fn raw(lib: *mut c_void, name: &str) -> Option<*mut c_void> {
                let c = CString::new(name).ok()?;
                let ptr = libc::dlsym(lib, c.as_ptr());
                if ptr.is_null() { None } else { Some(ptr) }
            }
            unsafe {
                Some(SpaceFns {
                    conn: std::mem::transmute(raw(lib, "CGSMainConnectionID")?),
                    copy: std::mem::transmute(raw(lib, "CGSCopySpaces")?),
                    add: std::mem::transmute(raw(lib, "CGSAddWindowsToSpaces")?),
                })
            }
        }

        static FNS: OnceLock<Option<SpaceFns>> = OnceLock::new();
        FNS.get_or_init(load).as_ref()
    }

    /// Put one window on every desktop and full-screen space that is showing now.
    fn join_showing_spaces(window_number: isize) {
        let Some(fns) = space_fns() else {
            return;
        };
        unsafe {
            let cid = (fns.conn)();
            // Mask 5 is the spaces on screen now, one entry per display.
            let spaces = (fns.copy)(cid, 5);
            if spaces.is_null() {
                return;
            }
            let id = window_number as i32;
            // kCFNumberSInt32Type = 3
            let number = CFNumberCreate(
                std::ptr::null(),
                3isize,
                &id as *const i32 as *const std::ffi::c_void,
            );
            if !number.is_null() {
                let values = [number];
                let windows = CFArrayCreate(
                    std::ptr::null(),
                    values.as_ptr(),
                    1,
                    &kCFTypeArrayCallBacks as *const CFArrayCallBacks as *const std::ffi::c_void,
                );
                if !windows.is_null() {
                    (fns.add)(cid, windows, spaces);
                    CFRelease(windows);
                }
                CFRelease(number);
            }
            CFRelease(spaces);
        }
    }

    pub fn is_character_palette_impl(handle: FocusHandle) -> bool {
        if handle <= 0 {
            return false;
        }
        use objc2_app_kit::NSRunningApplication;
        let Some(app) = (unsafe {
            NSRunningApplication::runningApplicationWithProcessIdentifier(handle as libc::pid_t)
        }) else {
            return false;
        };
        let Some(bundle) = (unsafe { app.bundleIdentifier() }) else {
            return false;
        };
        palette_bundle(&bundle.to_string())
    }

    pub fn dismiss_character_palette_impl() {
        use objc2_app_kit::NSRunningApplication;
        use objc2_foundation::NSString;

        let id = NSString::from_str("com.apple.CharacterPaletteIM");
        let apps = unsafe { NSRunningApplication::runningApplicationsWithBundleIdentifier(&id) };
        for index in 0..apps.count() {
            let app = unsafe { apps.objectAtIndex(index) };
            let _ = unsafe { app.hide() };
        }
    }

    fn palette_bundle(bundle: &str) -> bool {
        bundle == "com.apple.CharacterPaletteIM" || bundle.contains("CharacterPalette")
    }

    pub fn emoji_panel_on_screen_impl() -> bool {
        use std::ffi::c_void;

        #[link(name = "CoreGraphics", kind = "framework")]
        extern "C" {
            fn CGWindowListCopyWindowInfo(option: u32, relative: u32) -> *const c_void;
        }
        #[link(name = "CoreFoundation", kind = "framework")]
        extern "C" {
            fn CFArrayGetCount(array: *const c_void) -> isize;
            fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
            fn CFDictionaryGetValue(dict: *const c_void, key: *const c_void) -> *const c_void;
            fn CFStringCreateWithCString(
                alloc: *const c_void,
                c_str: *const i8,
                encoding: u32,
            ) -> *const c_void;
            fn CFStringGetCString(
                string: *const c_void,
                buffer: *mut i8,
                buffer_size: isize,
                encoding: u32,
            ) -> u8;
            fn CFGetTypeID(cf: *const c_void) -> usize;
            fn CFStringGetTypeID() -> usize;
            fn CFRelease(cf: *const c_void);
        }

        fn cf_string(bytes: &[u8]) -> *const c_void {
            unsafe { CFStringCreateWithCString(std::ptr::null(), bytes.as_ptr().cast(), 0x0800_0100) }
        }

        fn copy_string(value: *const c_void) -> String {
            if value.is_null() || unsafe { CFGetTypeID(value) } != unsafe { CFStringGetTypeID() } {
                return String::new();
            }
            let mut buffer = [0i8; 256];
            let ok = unsafe {
                CFStringGetCString(value, buffer.as_mut_ptr(), buffer.len() as isize, 0x0800_0100)
            };
            if ok == 0 {
                return String::new();
            }
            unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        }

        unsafe {
            let list = CGWindowListCopyWindowInfo(1, 0);
            if list.is_null() {
                return false;
            }
            let owner_key = cf_string(b"kCGWindowOwnerName\0");
            let name_key = cf_string(b"kCGWindowName\0");
            let mut found = false;
            for index in 0..CFArrayGetCount(list) {
                let item = CFArrayGetValueAtIndex(list, index);
                if item.is_null() {
                    continue;
                }
                let owner = copy_string(CFDictionaryGetValue(item, owner_key));
                let name = copy_string(CFDictionaryGetValue(item, name_key));
                let label = format!("{owner} {name}").to_ascii_lowercase();
                if label.contains("characterpalette")
                    || label.contains("character palette")
                    || label.contains("emoji")
                {
                    found = true;
                    break;
                }
            }
            if !owner_key.is_null() {
                CFRelease(owner_key);
            }
            if !name_key.is_null() {
                CFRelease(name_key);
            }
            CFRelease(list);
            found
        }
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
    pub fn cursor_point_impl() -> Option<(i32, i32)> { None }
    pub fn display_at_impl(_x: i32, _y: i32) -> Option<super::DisplayRect> { None }
    pub fn present_overlay_impl(_win: &tauri::WebviewWindow, _x: i32, _y: i32, _w: i32, _h: i32) {}
    pub fn our_handles_impl(_app: &tauri::AppHandle) -> Vec<FocusHandle> { vec![] }
    pub fn make_non_activating_impl(_win: &tauri::WebviewWindow) {}
    pub fn set_ignore_impl(_win: &tauri::WebviewWindow, _ignore: bool) -> bool { true }
    pub fn set_window_region_impl(
        _win: &tauri::WebviewWindow,
        _x: i32,
        _y: i32,
        _w: i32,
        _h: i32,
    ) -> bool {
        false
    }
    pub fn assert_overlay_style_impl(_win: &tauri::WebviewWindow) -> bool { false }
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
        assert!(!is_character_palette(0));
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
        assert_eq!(bits & (1 << 18), 1 << 18, "CanJoinAllApplications");
        assert_eq!(bits & 0x8, 0, "Transient hides the bar when DictFlow is in the background");
        assert_eq!(bits & 0x10, 0, "Stationary pins the bar to the desktop where it opened");
        assert_eq!(bits & 0x2, 0, "must not set MoveToActiveSpace");
    }

    #[test]
    fn pointer_on_the_upper_display_selects_that_display() {
        let displays = [
            DisplayRect { x: 0, y: 0, width: 1470, height: 956, scale: 1.0 },
            DisplayRect { x: -586, y: -1440, width: 2560, height: 1440, scale: 1.0 },
        ];
        let upper = display_containing(-100, -100, &displays).expect("upper screen");
        assert_eq!(upper.y, -1440);
        let main = display_containing(1101, 176, &displays).expect("main screen");
        assert_eq!(main.y, 0);
        assert!(display_containing(4000, 4000, &displays).is_none());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn overlay_style_hides_tool_window_chrome() {
        const WS_VISIBLE: isize = 0x1000_0000;
        const WS_CAPTION: isize = 0x00C0_0000;
        const WS_SYSMENU: isize = 0x0008_0000;
        const WS_POPUP: isize = 0x8000_0000u32 as isize;
        let input = WS_VISIBLE | WS_CAPTION | WS_SYSMENU;
        let out = platform::overlay_window_style(input);
        assert_eq!(out & WS_CAPTION, 0, "caption/border must go");
        assert_eq!(out & WS_SYSMENU, 0, "close button must go");
        assert_ne!(out & WS_VISIBLE, 0, "keep visible");
        assert_ne!(out & WS_POPUP, 0);
        let ex = platform::overlay_window_exstyle(0);
        assert_ne!(ex & 0x0800_0000, 0, "NOACTIVATE");
        assert_ne!(ex & 0x0000_0080, 0, "TOOLWINDOW");
        assert_ne!(ex & 0x0008_0000, 0, "LAYERED");
    }
}
