//! OS permission status and prompts.
//!
//! macOS: Microphone + Accessibility (SpeakType). Session-tap Fn does not
//! need Input Monitoring. Windows has no equivalent; those fields stay granted.

use serde::Serialize;
use tauri::{AppHandle, Manager};
#[cfg(target_os = "macos")]
use tauri::Emitter;

use crate::state::AppState;
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct PermStatus {
    pub microphone: bool,
    pub accessibility: bool,
    pub input_monitoring: bool,
    /// True when the configured talk key can actually see key events.
    pub talk_key_live: bool,
}

#[tauri::command]
pub(crate) fn get_permissions(app: AppHandle) -> PermStatus {
    status(&app)
}

#[tauri::command]
pub(crate) fn request_permission(app: AppHandle, kind: String) -> Result<PermStatus, String> {
    match kind.as_str() {
        "microphone" => request_microphone(),
        "accessibility" => request_accessibility(),
        "input_monitoring" => request_input_monitoring(),
        other => return Err(format!("unknown permission: {other}")),
    }
    crate::hotkey::retry_macos_taps(&app);
    Ok(status(&app))
}

pub(crate) fn status(app: &AppHandle) -> PermStatus {
    let key = app
        .state::<Mutex<AppState>>()
        .lock()
        .map(|s| s.settings.hotkey_key.clone())
        .unwrap_or_default();
    PermStatus {
        microphone: microphone_granted(),
        accessibility: accessibility_granted(),
        input_monitoring: input_monitoring_granted(),
        talk_key_live: crate::hotkey::talk_key_live(&key),
    }
}

/// Poll TCC, retry the session tap, and relaunch once if Accessibility is on
/// but the tap is still dead (rare; session taps usually attach after grant).
pub(crate) fn spawn_watcher(app: AppHandle) {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
    }
    #[cfg(target_os = "macos")]
    {
        tauri::async_runtime::spawn(async move {
            let mut last = status(&app);
            let _ = app.emit("dictflow://permissions", last.clone());
            let boot = std::time::Instant::now();
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                let now = status(&app);
                if now != last {
                    let _ = app.emit("dictflow://permissions", now.clone());
                    last = now.clone();
                }
                if boot.elapsed() < std::time::Duration::from_secs(3) {
                    continue;
                }
                if now.accessibility {
                    crate::hotkey::retry_macos_taps(&app);
                    let now = status(&app);
                    if now != last {
                        let _ = app.emit("dictflow://permissions", now.clone());
                        last = now.clone();
                    }
                    if !now.talk_key_live && should_relaunch_for_taps(&app) {
                        log::info!("accessibility granted — relaunching so the Fn talk key can attach");
                        app.restart();
                    }
                }
            }
        });
    }
}

#[cfg(target_os = "macos")]
fn should_relaunch_for_taps(app: &AppHandle) -> bool {
    let dir = app
        .state::<Mutex<AppState>>()
        .lock()
        .map(|s| s.data_dir.clone())
        .unwrap_or_else(|_| std::env::temp_dir());
    let stamp = dir.join(".im-relaunch");
    if let Ok(meta) = std::fs::metadata(&stamp) {
        if let Ok(mtime) = meta.modified() {
            if mtime.elapsed().map(|d| d.as_secs() < 90).unwrap_or(false) {
                return false;
            }
        }
    }
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&stamp, b"1");
    true
}

fn microphone_granted() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::microphone_granted()
    }
    #[cfg(not(target_os = "macos"))]
    {
        crate::audio::default_input_name().is_some()
    }
}

fn accessibility_granted() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::accessibility_granted()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

fn input_monitoring_granted() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::input_monitoring_granted()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

fn request_microphone() {
    #[cfg(target_os = "macos")]
    macos::request_microphone();
    #[cfg(not(target_os = "macos"))]
    {
        let _ = crate::sys::open_mic_settings();
    }
}

fn request_accessibility() {
    #[cfg(target_os = "macos")]
    macos::request_accessibility();
}

fn request_input_monitoring() {
    #[cfg(target_os = "macos")]
    macos::request_input_monitoring();
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::c_void;

    use crate::sys;

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> u8;
        fn AXIsProcessTrustedWithOptions(options: *const c_void) -> u8;
        static kAXTrustedCheckOptionPrompt: *const c_void;
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGPreflightListenEventAccess() -> bool;
        fn CGRequestListenEventAccess() -> bool;
        fn CGRequestPostEventAccess() -> bool;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFBooleanTrue: *const c_void;
        fn CFDictionaryCreate(
            allocator: *const c_void,
            keys: *const *const c_void,
            values: *const *const c_void,
            num_values: isize,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> *mut c_void;
        fn CFRelease(cf: *const c_void);
    }

    #[link(name = "AVFoundation", kind = "framework")]
    extern "C" {}

    pub fn accessibility_granted() -> bool {
        unsafe { AXIsProcessTrusted() != 0 }
    }

    pub fn input_monitoring_granted() -> bool {
        unsafe { CGPreflightListenEventAccess() }
    }

    pub fn microphone_granted() -> bool {
        use objc2::msg_send;
        use objc2_foundation::NSString;

        let media = NSString::from_str("soun");
        let cls = objc2::class!(AVCaptureDevice);
        let status: i64 =
            unsafe { msg_send![cls, authorizationStatusForMediaType: &*media] };
        status == 3
    }

    pub fn request_accessibility() {
        unsafe {
            let keys = [kAXTrustedCheckOptionPrompt];
            let vals = [kCFBooleanTrue];
            let dict = CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                vals.as_ptr(),
                1,
                std::ptr::null(),
                std::ptr::null(),
            );
            if !dict.is_null() {
                let _ = AXIsProcessTrustedWithOptions(dict);
                CFRelease(dict);
            }
            let _ = CGRequestPostEventAccess();
        }
        let _ = sys::open_accessibility_settings();
    }

    pub fn request_input_monitoring() {
        unsafe {
            let _ = CGRequestListenEventAccess();
        }
        let _ = sys::open_input_monitoring_settings();
    }

    pub fn request_microphone() {
        trigger_mic_prompt();
        if !microphone_granted() {
            let _ = sys::open_mic_settings();
        }
    }

    fn trigger_mic_prompt() {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        let Some(dev) = cpal::default_host().default_input_device() else {
            return;
        };
        let Ok(cfg) = dev.default_input_config() else {
            return;
        };
        let stream = dev.build_input_stream(
            &cfg.into(),
            |_data: &[f32], _| {},
            |_| {},
            None,
        );
        if let Ok(stream) = stream {
            let _ = stream.play();
            std::thread::sleep(std::time::Duration::from_millis(120));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_like_hosts_treat_ax_and_im_as_granted() {
        #[cfg(not(target_os = "macos"))]
        {
            assert!(accessibility_granted());
            assert!(input_monitoring_granted());
        }
        #[cfg(target_os = "macos")]
        {
            let _ = accessibility_granted();
            let _ = input_monitoring_granted();
            let _ = microphone_granted();
        }
    }
}
