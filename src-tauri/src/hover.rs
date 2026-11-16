//! Hover-driven click-through for the floating overlay pill.
//!
//! Platform split:
//! - Windows: the overlay is clipped to the pill's rounded shape with
//!   `SetWindowRgn` (drawing AND input) — the OS itself routes clicks outside
//!   the region to whatever is below, with no toggling or flicker. This loop
//!   just re-asserts style + region every ~2s so tao restyles / DPI moves
//!   can't leave the pill broken.
//! - macOS/Linux: NSWindow/Solid surfaces don't take shaped input regions, so
//!   this module polls the cursor every few frames and toggles pass-through
//!   unless the cursor is inside the pill's hit rect (or a press is latched).
//!
//! The pill's window-relative footprint is reported by the overlay webview
//! (`set_overlay_hit_rect`) so the gate tracks whatever the CSS morphs to.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use tauri::Manager;

/// Hover poll cadence: tight enough that the pill feels instantly clickable,
/// loose enough to stay invisible in a profiler.
const POLL_MS: u64 = 40;
/// Logical-pixel slack around the pill so edge hovers still register.
#[cfg_attr(target_os = "windows", allow(dead_code))] // hover gate: not Windows
const PAD: i32 = 3;
/// Re-apply the pass-through state every ~2s even when the cache claims it is
/// current (guards against tao style recomputes from other flag changes).
const RESYNC_EVERY: u32 = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HitRect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl HitRect {
    /// Idle pill centered in the window. The webview overwrites this with the
    /// live copy as soon as it loads and on every further phase change.
    const fn idle() -> Self {
        Self {
            x: (crate::overlay::PILL_W - crate::overlay::PILL_IDLE_W) / 2,
            y: (crate::overlay::PILL_H - crate::overlay::PILL_IDLE_H) / 2,
            w: crate::overlay::PILL_IDLE_W,
            h: crate::overlay::PILL_IDLE_H,
        }
    }

    #[cfg_attr(target_os = "windows", allow(dead_code))] // hover gate: not Windows
    fn contains(&self, x: f64, y: f64, pad: i32) -> bool {
        let lo_x = f64::from(self.x - pad);
        let hi_x = f64::from(self.x + self.w + pad);
        let lo_y = f64::from(self.y - pad);
        let hi_y = f64::from(self.y + self.h + pad);
        x >= lo_x && x <= hi_x && y >= lo_y && y <= hi_y
    }
}

static HIT: Mutex<HitRect> = Mutex::new(HitRect::idle());

/// Last value successfully applied to the overlay window (`None` = unknown /
/// someone else may have flipped it on the side; see `invalidate`).
static APPLIED: Mutex<Option<bool>> = Mutex::new(None);

/// True while the overlay webview is mid-press (click or drag). The hover gate
/// must not flip the window into pass-through under the pointer while the
/// pointer capture belongs to us.
static PRESS_LATCH: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_press(active: bool) {
    PRESS_LATCH.store(active, Ordering::Relaxed);
}

/// The overlay window's ignore-cursor flag was changed on the side (e.g.
/// `apply_overlay_visibility` during a pose update) — force a re-apply on the
/// next tick instead of trusting the cache.
pub(crate) fn invalidate() {
    if let Ok(mut applied) = APPLIED.lock() {
        *applied = None;
    }
}

fn parse_hit_rect(x: f64, y: f64, w: f64, h: f64) -> Option<HitRect> {
    if w < 1.0 || h < 1.0 {
        return None;
    }
    Some(HitRect {
        x: x as i32,
        y: y as i32,
        w: w as i32,
        h: h as i32,
    })
}

pub(crate) fn set_hit_rect(app: &tauri::AppHandle, x: f64, y: f64, w: f64, h: f64) {
    let Some(hit) = parse_hit_rect(x, y, w, h) else {
        return;
    };
    if let Ok(mut cur) = HIT.lock() {
        *cur = hit;
    }
    // Windows: clip the window to the pill immediately. Outside the region
    // the OS treats the window as nonexistent (drawing AND input), so the
    // glass around the pill can never block clicks.
    if let Some(win) = app.get_webview_window("overlay") {
        let _ = crate::focus::set_window_region(&win, hit.x, hit.y, hit.w, hit.h);
    }
}

/// Region for the idle pill before the webview reports a live rect at boot.
pub(crate) fn sync_idle_region(win: &tauri::WebviewWindow) {
    let hit = HitRect::idle();
    let _ = crate::focus::set_window_region(win, hit.x, hit.y, hit.w, hit.h);
}

/// Background loop toggling the overlay between pass-through and clickable.
pub(crate) fn spawn_hover_follow(app: tauri::AppHandle) {
    let spawned = std::thread::Builder::new()
        .name("overlay-hover".into())
        .spawn(move || {
            let mut counter: u32 = 0;
            loop {
                std::thread::sleep(Duration::from_millis(POLL_MS));
                tick(&app, &mut counter);
            }
        });
    if let Err(e) = spawned {
        log::error!("overlay hover thread: {e}");
    }
}

fn tick(app: &tauri::AppHandle, counter: &mut u32) {
    let Some(win) = app.get_webview_window("overlay") else {
        return;
    };
    if !win.is_visible().unwrap_or(false) {
        return; // hidden overlay — leave window state alone
    }
    #[cfg(target_os = "windows")]
    {
        // Windows: the window region already confines input+drawing to the
        // pill, so no click-through toggling happens here. Just re-assert
        // style + region every ~2s in case a tao restyle or a DPI change
        // (dragged across monitors) left the pill wrong.
        if *counter % RESYNC_EVERY == 0 {
            if crate::focus::assert_overlay_style(&win) {
                log::debug!("overlay window style drifted; re-patched");
            }
            let hit = *HIT.lock().unwrap_or_else(|e| e.into_inner());
            let _ = crate::focus::set_window_region(&win, hit.x, hit.y, hit.w, hit.h);
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        // macOS/Linux: hover-driven pass-through (no window-region support).
        let latch = PRESS_LATCH.load(Ordering::Relaxed);
        let ignore = !latch && !hovering(&win);
        let force = *counter % RESYNC_EVERY == 0;
        apply_ignore(&win, ignore, force);
    }
    *counter = counter.wrapping_add(1);
}

#[cfg_attr(target_os = "windows", allow(dead_code))] // hover gate runs off Windows only
fn apply_ignore(win: &tauri::WebviewWindow, ignore: bool, force: bool) {
    let cached = *APPLIED.lock().unwrap_or_else(|e| e.into_inner());
    if cached == Some(ignore) && !force {
        return;
    }
    if crate::focus::set_ignore_cursor_events(win, ignore) {
        if let Ok(mut applied) = APPLIED.lock() {
            *applied = Some(ignore);
        }
    }
}

#[cfg_attr(target_os = "windows", allow(dead_code))] // hover gate: not Windows
fn hovering(win: &tauri::WebviewWindow) -> bool {
    let Some((cx, cy)) = platform::cursor_rel_logical(win) else {
        // Cursor position unknown (e.g. Linux fallback): stay clickable so
        // the pill keeps working — the old ungated behavior.
        return true;
    };
    let hit = HIT.lock().unwrap_or_else(|e| e.into_inner()).to_owned();
    hit.contains(cx, cy, PAD)
}

// ---------------------------------------------------------------------------
// Windows cursor probe (GetCursorPos + GetWindowRect, divided by the
// window's scale). Kept even on Windows builds only to satisfy the hover
// gate used on other platforms — on Windows the region approach replaces it.
// ---------------------------------------------------------------------------
#[cfg(target_os = "windows")]
#[cfg_attr(target_os = "windows", allow(dead_code))]
mod platform {
    use tauri::WebviewWindow;
    use std::ffi::c_void;

    #[repr(C)]
    struct POINT {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    struct RECT {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[link(name = "user32")]
    extern "system" {
        fn GetCursorPos(point: *mut POINT) -> i32;
        fn GetWindowRect(hwnd: *mut c_void, rect: *mut RECT) -> i32;
    }

    /// Cursor position relative to the overlay window, in logical pixels.
    pub(crate) fn cursor_rel_logical(win: &WebviewWindow) -> Option<(f64, f64)> {
        let hwnd = win.hwnd().ok()?.0 as *mut c_void;
        unsafe {
            let mut cur = POINT { x: 0, y: 0 };
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if GetCursorPos(&mut cur) == 0 || GetWindowRect(hwnd, &mut rect) == 0 {
                return None;
            }
            let scale = win.scale_factor().ok()? as f64;
            let rel_x = f64::from(cur.x - rect.left) / scale;
            let rel_y = f64::from(cur.y - rect.top) / scale;
            Some((rel_x, rel_y))
        }
    }
}

// ---------------------------------------------------------------------------
// macOS — `[NSEvent mouseLocation]` and `[NSWindow frame]` share the global
// bottom-left display coordinate space, so window-relative math needs no
// screen mapping at all (both are in logical points).
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
mod platform {
    use objc2::{class, msg_send};
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSPoint, NSRect};
    use tauri::WebviewWindow;

    pub(crate) fn cursor_rel_logical(win: &WebviewWindow) -> Option<(f64, f64)> {
        let ns = win.ns_window().ok()? as *mut AnyObject;
        if ns.is_null() {
            return None;
        }
        // SAFETY: plain getter selectors; frame/mouseLocation both return
        // plain structs with no references retained.
        unsafe {
            let frame: NSRect = msg_send![ns, frame];
            let mouse: NSPoint = msg_send![class!(NSEvent), mouseLocation];
            let rel_x = mouse.x - frame.origin.x;
            // Flip Y: display coords grow upward, the DOM grows downward.
            let rel_y = frame.size.height - (mouse.y - frame.origin.y);
            Some((rel_x, rel_y))
        }
    }
}

// ---------------------------------------------------------------------------
// Fallback (Linux and others) — no cursor probe; the hover gate keeps the
// pill clickable (previous behavior).
// ---------------------------------------------------------------------------
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
mod platform {
    pub(crate) fn cursor_rel_logical(_win: &tauri::WebviewWindow) -> Option<(f64, f64)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_rect_is_centered() {
        let r = HitRect::idle();
        assert_eq!(r.x, (crate::overlay::PILL_W - r.w) / 2);
        assert_eq!(r.y, (crate::overlay::PILL_H - r.h) / 2);
    }

    #[test]
    fn contains_matches_center_inside_outside() {
        let r = HitRect { x: 10, y: 10, w: 50, h: 20 };
        assert!(r.contains(35.0, 20.0, 0));
        assert!(!r.contains(64.0, 20.0, 0));
        assert!(r.contains(63.0, 20.0, 3));
    }

    #[test]
    fn parse_hit_rect_ignores_junk() {
        assert_eq!(parse_hit_rect(0.0, 0.0, 0.5, 30.0), None);
        assert_eq!(
            parse_hit_rect(4.0, 5.0, 250.0, 44.0),
            Some(HitRect { x: 4, y: 5, w: 250, h: 44 })
        );
        assert_eq!(
            parse_hit_rect(0.0, 0.0, 200.0, 5.0),
            Some(HitRect { x: 0, y: 0, w: 200, h: 5 })
        );
    }
}
