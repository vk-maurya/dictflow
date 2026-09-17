//! Tray icon, overlay window, and main-window show/hide.
//!
//! Close and minimize hide to the tray on both platforms. Reopen from the
//! tray menu, a Windows tray double-click, a second app launch, or the macOS
//! Dock (`RunEvent::Reopen`).

use std::sync::Mutex;

use tauri::Manager;

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
        _ => {}
    }
}

#[cfg(target_os = "macos")]
fn activate_nsapp() {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    unsafe {
        let cls = objc2::class!(NSApplication);
        let app: *mut AnyObject = msg_send![cls, sharedApplication];
        if !app.is_null() {
            let _: bool = msg_send![app, activateIgnoringOtherApps: true];
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

pub(crate) fn apply_overlay_visibility(app: &tauri::AppHandle) {
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
    focus::make_non_activating(&win);
    let _ = win.set_ignore_cursor_events(false);
    let _ = win.set_always_on_top(true);
    let _ = win.set_visible_on_all_workspaces(true);
    let _ = win.set_background_color(Some(tauri::window::Color(0, 0, 0, 0)));
    let _ = win.set_size(tauri::LogicalSize::new(
        overlay::PILL_W as f64,
        overlay::PILL_H as f64,
    ));
    if let Ok(Some(m)) = win.current_monitor() {
        let scale = m.scale_factor();
        let size = m.size();
        let origin = m.position();
        let screen_w = (size.width as f64 / scale).round() as i32;
        let screen_h = (size.height as f64 / scale).round() as i32;
        let edge = overlay::DockEdge::parse(&settings.overlay_edge).unwrap_or(overlay::DockEdge::Bottom);
        let (x, y) = overlay::pose_to_xy(
            edge,
            settings.overlay_offset,
            screen_w,
            screen_h,
            overlay::PILL_W,
            overlay::PILL_H,
        );
        let _ = win.set_position(tauri::LogicalPosition::new(
            origin.x as f64 / scale + f64::from(x),
            origin.y as f64 / scale + f64::from(y),
        ));
    }
    let _ = win.show();
    focus::make_non_activating(&win);
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
