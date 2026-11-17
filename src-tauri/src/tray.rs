//! Tray icon, overlay window, and main-window show/hide.
//!
//! Close and minimize hide to the tray on both platforms. Reopen from the
//! tray menu, a Windows tray double-click, a second app launch, or the macOS
//! Dock (`RunEvent::Reopen`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use tauri::{Emitter, Manager};

use crate::dictation::toggle_recording;
use crate::focus;
use crate::overlay;
use crate::state::AppState;

pub(crate) fn wants_tray_launch<I, S>(args: I, onboarded: bool) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    onboarded && args.into_iter().any(|a| a.as_ref() == "--minimized")
}

pub(crate) fn hide_main_to_tray(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        // Unminimize first: a miniaturized+hidden NSWindow often refuses to
        // come back on the next Dock click.
        let _ = w.unminimize();
        let _ = w.hide();
        // Windows only — on macOS skip_taskbar can drop the Dock icon, which
        // is how users reopen the hidden window.
        #[cfg(target_os = "windows")]
        let _ = w.set_skip_taskbar(true);
    }
}

pub(crate) fn show_main_window(app: &tauri::AppHandle) {
    #[cfg(target_os = "macos")]
    {
        let _ = app.show();
        activate_nsapp();
    }
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.set_skip_taskbar(false);
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// Close and the yellow/minimize button both park the UI in the tray.
pub(crate) fn on_main_window_event(window: &tauri::Window, event: &tauri::WindowEvent) {
    match event {
        tauri::WindowEvent::CloseRequested { api, .. } => {
            api.prevent_close();
            hide_main_to_tray(window.app_handle());
            apply_overlay_visibility(window.app_handle());
        }
        tauri::WindowEvent::Moved(_) | tauri::WindowEvent::Resized(_)
            if window.is_minimized().unwrap_or(false) =>
        {
            hide_main_to_tray(window.app_handle());
            apply_overlay_visibility(window.app_handle());
        }
        // The yellow button often reports the resize or the focus loss before
        // is_minimized() is true. Looking only at that instant leaves the
        // window miniaturized, so the mark stays inside it.
        tauri::WindowEvent::Resized(_) | tauri::WindowEvent::Focused(false) => {
            schedule_park_if_minimized(window.app_handle());
        }
        _ => {}
    }
}

static PARK_CHECK_PENDING: AtomicBool = AtomicBool::new(false);
static OVERLAY_DRAGGING: AtomicBool = AtomicBool::new(false);

/// Frame the overlay was last placed at: x, y, width, height in the same
/// top-left global coordinates as the pointer.
type Frame = (i32, i32, i32, i32);
static LAST_FRAME: Mutex<Option<Frame>> = Mutex::new(None);
static DRAG_START: Mutex<Option<Frame>> = Mutex::new(None);

fn remember_frame(frame: Frame) {
    if let Ok(mut last) = LAST_FRAME.lock() {
        *last = Some(frame);
    }
}

/// Whether the pointer is on the pill. The pill sits centered in a larger frame,
/// so only the pill's own area counts. The area grows a little once the pointer is
/// on it, which keeps the hover from flickering at the edge.
pub(crate) fn pointer_on_pill(already_hovering: bool) -> bool {
    let Some(frame) = LAST_FRAME.lock().ok().and_then(|f| *f) else {
        return false;
    };
    let Some((px, py)) = focus::cursor_point() else {
        return false;
    };
    let (cx, cy) = (frame.0 + frame.2 / 2, frame.1 + frame.3 / 2);
    let scale = focus::display_at(cx, cy)
        .map(|d| d.scale)
        .filter(|s| s.is_finite() && *s > 0.0)
        .unwrap_or(1.0);
    // Idle pill is 48 by 11 and the hover pill 116 by 20, in logical pixels.
    let (long, short) = if already_hovering { (116.0, 20.0) } else { (52.0, 15.0) };
    let (half_w, half_h) = if frame.2 >= frame.3 {
        (long / 2.0, short / 2.0)
    } else {
        (short / 2.0, long / 2.0)
    };
    f64::from((px - cx).abs()) <= half_w * scale && f64::from((py - cy).abs()) <= half_h * scale
}

/// Where a finished drag landed, in logical pixels on the display it was dropped on.
pub(crate) struct DroppedPose {
    pub(crate) center_x: i32,
    pub(crate) center_y: i32,
    pub(crate) screen_w: i32,
    pub(crate) screen_h: i32,
}

/// Start a drag from wherever the pill is now. The follow loop pauses until the drop.
pub(crate) fn begin_overlay_drag() {
    if let (Ok(last), Ok(mut start)) = (LAST_FRAME.lock(), DRAG_START.lock()) {
        *start = *last;
    }
    OVERLAY_DRAGGING.store(true, Ordering::Release);
}

/// Move the pill by the pointer travel since the drag began, in logical pixels.
pub(crate) fn drag_overlay_by(app: &tauri::AppHandle, dx: f64, dy: f64) {
    #[cfg(target_os = "macos")]
    if unsafe { libc::pthread_main_np() } == 0 {
        let app = app.clone();
        let schedule = app.clone();
        let _ = schedule.run_on_main_thread(move || drag_overlay_by_inner(&app, dx, dy));
        return;
    }
    drag_overlay_by_inner(app, dx, dy);
}

fn drag_overlay_by_inner(app: &tauri::AppHandle, dx: f64, dy: f64) {
    let Some(win) = app.get_webview_window("overlay") else {
        return;
    };
    let Some(start) = DRAG_START.lock().ok().and_then(|s| *s) else {
        return;
    };
    let scale = focus::display_at(start.0 + start.2 / 2, start.1 + start.3 / 2)
        .map(|d| d.scale)
        .filter(|s| s.is_finite() && *s > 0.0)
        .unwrap_or(1.0);
    let frame = (
        start.0 + (dx * scale).round() as i32,
        start.1 + (dy * scale).round() as i32,
        start.2,
        start.3,
    );
    focus::present_overlay(&win, frame.0, frame.1, frame.2, frame.3);
    remember_frame(frame);
}

/// End a drag. Returns the drop position when the pill actually moved.
pub(crate) fn end_overlay_drag(moved: bool) -> Option<DroppedPose> {
    let dropped = if moved {
        LAST_FRAME.lock().ok().and_then(|f| *f).and_then(|frame| {
            let cx = frame.0 + frame.2 / 2;
            let cy = frame.1 + frame.3 / 2;
            let screen = focus::display_at(cx, cy)?;
            let scale = if screen.scale.is_finite() && screen.scale > 0.0 {
                screen.scale
            } else {
                1.0
            };
            Some(DroppedPose {
                center_x: (f64::from(cx - screen.x) / scale).round() as i32,
                center_y: (f64::from(cy - screen.y) / scale).round() as i32,
                screen_w: (f64::from(screen.width) / scale).round() as i32,
                screen_h: (f64::from(screen.height) / scale).round() as i32,
            })
        })
    } else {
        None
    };
    OVERLAY_DRAGGING.store(false, Ordering::Release);
    dropped
}

fn overlay_dragging() -> bool {
    OVERLAY_DRAGGING.load(Ordering::Acquire)
}

fn schedule_park_if_minimized(app: &tauri::AppHandle) {
    if PARK_CHECK_PENDING.swap(true, Ordering::AcqRel) {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(280)).await;
        PARK_CHECK_PENDING.store(false, Ordering::Release);
        let app_for_main = app.clone();
        let _ = app.run_on_main_thread(move || {
            let Some(window) = app_for_main.get_webview_window("main") else {
                return;
            };
            if window.is_minimized().unwrap_or(false) {
                hide_main_to_tray(&app_for_main);
                apply_overlay_visibility(&app_for_main);
            }
        });
    });
}

#[cfg(target_os = "macos")]
fn activate_nsapp() {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    unsafe {
        let cls = objc2::class!(NSApplication);
        let app: *mut AnyObject = msg_send![cls, sharedApplication];
        if !app.is_null() {
            // activateIgnoringOtherApps: returns void. Declaring a bool
            // return makes objc2 panic ("expected v, found B"), and that
            // panic aborts inside tao's applicationShouldHandleReopen.
            let _: () = msg_send![app, activateIgnoringOtherApps: true];
        }
    }
}

fn spawn_tray_toggle(app: &tauri::AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state: tauri::State<'_, Mutex<AppState>> = app.state();
        let _ = toggle_recording(app.clone(), state).await;
    });
}

pub(crate) fn build_tray(app: &tauri::AppHandle) -> anyhow::Result<()> {
    use tauri::menu::{Menu, MenuItem};

    let toggle = MenuItem::with_id(app, "toggle", "Start/Stop dictation", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "Open DictFlow", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&toggle, &settings, &quit])?;

    // tauri.conf.json already creates the "main" tray — attach to it so we
    // don't end up with a second icon.
    let tray = app
        .tray_by_id("main")
        .ok_or_else(|| anyhow::anyhow!("missing tray icon"))?;
    tray.set_menu(Some(menu))?;
    tray.set_tooltip(Some("DictFlow — offline voice dictation"))?;
    // On macOS the convention for menu-bar apps is that a single click opens
    // the menu.  We set show_menu_on_left_click(true) so the Start/Stop toggle
    // is always one click away regardless of which mouse button the user uses.
    tray.set_show_menu_on_left_click(true)?;
    tray.on_menu_event(|app, event| match event.id.as_ref() {
        "quit" => app.exit(0),
        "settings" => show_main_window(app),
        "toggle" => spawn_tray_toggle(app),
        _ => {}
    });
    tray.on_tray_icon_event(|tray, event| {
        use tauri::tray::{MouseButton, TrayIconEvent};
        if let TrayIconEvent::DoubleClick {
            button: MouseButton::Left,
            ..
        } = event
        {
            show_main_window(tray.app_handle());
        }
    });
    Ok(())
}

/// While DictFlow is open, keep the bar in its placed spot on the display under the pointer.
/// The pointer picks the display only. It never moves the bar within it.
pub(crate) fn follow_overlay_screen(app: &tauri::AppHandle) {
    if overlay_dragging() {
        return;
    }
    apply_overlay_visibility_inner(app);
}

pub(crate) fn apply_overlay_visibility(app: &tauri::AppHandle) {
    // NSWindow level and collection behavior abort off the main thread
    // ("Must only be used from the main thread"). The 250ms space follow-up
    // runs on a tokio worker, so hop back before touching the overlay.
    #[cfg(target_os = "macos")]
    if unsafe { libc::pthread_main_np() } == 0 {
        let app = app.clone();
        let schedule = app.clone();
        let _ = schedule.run_on_main_thread(move || apply_overlay_visibility_inner(&app));
        return;
    }
    apply_overlay_visibility_inner(app);
}

fn apply_overlay_visibility_inner(app: &tauri::AppHandle) {
    let Some(win) = app.get_webview_window("overlay") else {
        return;
    };
    let settings = app
        .state::<Mutex<AppState>>()
        .lock()
        .ok()
        .map(|s| s.settings.clone());
    let Some(settings) = settings else {
        return;
    };
    if !settings.overlay_enabled {
        let _ = win.hide();
        return;
    }
    prepare_overlay_window(&win);
    let edge = overlay::DockEdge::parse(&settings.overlay_edge).unwrap_or(overlay::DockEdge::Bottom);
    let (frame_w, frame_h) = overlay::orientation_size(edge);
    let point = focus::cursor_point();
    if let Some(point) = point {
        if let Some(frame) = frame_near_point(point, frame_w, frame_h, |_, _, sw, sh, fw, fh, scale| {
            if settings.overlay_free {
                // Saved in logical pixels on the display it was placed on.
                // Work in logical pixels, then convert to this display's units.
                let logical = |v: i32| (f64::from(v) / scale).round() as i32;
                let (x, y) = overlay::place_free_scaled(
                    settings.overlay_x,
                    settings.overlay_y,
                    settings.overlay_screen_w,
                    settings.overlay_screen_h,
                    logical(sw),
                    logical(sh),
                    frame_w,
                    frame_h,
                );
                ((f64::from(x) * scale).round() as i32, (f64::from(y) * scale).round() as i32)
            } else {
                overlay::pose_to_xy(edge, settings.overlay_offset, sw, sh, fw, fh)
            }
        }) {
            show_overlay_frame(&win, frame);
        }
    }
    let _ = app.emit("dictflow://overlay-edge", settings.overlay_edge);
}

fn prepare_overlay_window(win: &tauri::WebviewWindow) {
    focus::make_non_activating(win);
    let _ = win.unminimize();
    let _ = win.set_minimizable(false);
    // Use the bit-only toggle (not tauri's) — tao's full restyle would strip
    // the chrome/layered patches `make_non_activating` just applied.
    crate::hover::invalidate();
    focus::set_ignore_cursor_events(win, false);
    let _ = win.set_always_on_top(true);
    let _ = win.set_visible_on_all_workspaces(true);
    let _ = win.set_shadow(false);
    let _ = win.set_background_color(Some(tauri::window::Color(0, 0, 0, 0)));
}

fn show_overlay_frame(win: &tauri::WebviewWindow, frame: (i32, i32, i32, i32)) {
    // show() makes the window key. Only use it to unhide; after that
    // present_overlay orders it front without taking the caret.
    if !win.is_visible().unwrap_or(false) {
        let _ = win.show();
    }
    let _ = win.set_visible_on_all_workspaces(true);
    focus::make_non_activating(win);
    crate::hover::sync_idle_region(win);
    // present_overlay is last. A later collection-behavior change puts the
    // window back on the display's ordinary desktop instead of the one showing.
    focus::present_overlay(win, frame.0, frame.1, frame.2, frame.3);
    remember_frame(frame);
}

/// `place` receives local x/y, screen width/height, and the frame size in the
/// same units as the pointer, plus the display scale.
fn frame_near_point(
    point: (i32, i32),
    logical_w: i32,
    logical_h: i32,
    place: impl FnOnce(i32, i32, i32, i32, i32, i32, f64) -> (i32, i32),
) -> Option<(i32, i32, i32, i32)> {
    let screen = focus::display_at(point.0, point.1)?;
    note_screen(screen);
    let scale = if screen.scale.is_finite() && screen.scale > 0.0 {
        screen.scale
    } else {
        1.0
    };
    let frame_w = (f64::from(logical_w) * scale).round() as i32;
    let frame_h = (f64::from(logical_h) * scale).round() as i32;
    let (x, y) = place(
        point.0 - screen.x,
        point.1 - screen.y,
        screen.width,
        screen.height,
        frame_w,
        frame_h,
        scale,
    );
    Some((screen.x + x, screen.y + y, frame_w, frame_h))
}

fn note_screen(screen: focus::DisplayRect) {
    static LAST: Mutex<Option<(i32, i32)>> = Mutex::new(None);
    let Ok(mut last) = LAST.lock() else {
        return;
    };
    if *last == Some((screen.x, screen.y)) {
        return;
    }
    *last = Some((screen.x, screen.y));
    log::info!(
        "dictation bar is on the screen at ({}, {})",
        screen.x,
        screen.y
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_minimized_requires_onboarded() {
        assert!(wants_tray_launch(["dictflow", "--minimized"], true));
        assert!(!wants_tray_launch(["dictflow", "--minimized"], false));
        assert!(!wants_tray_launch(["dictflow"], true));
    }
}
