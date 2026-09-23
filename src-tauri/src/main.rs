#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

// DictFlow — offline voice dictation (Wispr Flow-style, 100% local).
// Stack: Tauri v2 + Rust backend + whisper.cpp sidecar (Whisper)
//        + in-process sherpa-onnx (Parakeet) + cpal audio + OS paste.
//
// Supports: Windows (WASAPI, HWND focus, GetAsyncKeyState) and
//           macOS (CoreAudio, NSWorkspace/NSRunningApplication, CGEventTap).
//
// Pipeline (mirrors SpeakType): hotkey → mic capture → STT engine →
// spoken commands / backtrack / lists → dictionary → auto-edit → paste.

mod audio;
mod commands;
mod devices;
mod dictation;
mod engine;
mod focus;
mod format;
mod hotkey;
mod models;
mod onboarding;
mod overlay;
mod paste;
mod perm;
mod polish;
mod prompts;
mod session;
mod settings;
mod shortcuts;
mod state;
mod stats;
mod sys;
mod text;
mod tray;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use tauri::{Emitter, Manager, State};
use tauri_plugin_global_shortcut::{Code, Modifiers, Shortcut, ShortcutState};

use crate::audio::{get_audio_devices, test_microphone};
use crate::commands::*;
use crate::dictation::{cancel_dictation, toggle_recording, transcribe_file};
use crate::models::EngineKind;
use crate::perm::{get_permissions, request_permission};
use crate::state::AppState;
use crate::sys::{
    open_accessibility_settings, open_input_monitoring_settings, open_mic_settings,
};

pub fn run() {
    #[cfg(target_os = "macos")]
    crate::sys::relaunch_from_app_bundle_if_needed();

    let builder = tauri::Builder::default()
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .targets([
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::LogDir {
                        file_name: Some("dictflow".into()),
                    }),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Webview),
                ])
                .build(),
        )
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            crate::tray::show_main_window(app);
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--minimized"]),
        ))
        .plugin(
            // Do not pre-register Ctrl+Alt+Space here. Plugin setup runs inside
            // applicationDidFinishLaunching, and a failed RegisterEventHotKey
            // returns Err. Tauri turns that into panic!("Failed to setup app"),
            // which aborts across the extern "C" callback (panic_cannot_unwind)
            // before any window appears. Talk and utility shortcuts are
            // registered in setup, where a failure is logged and the app stays up.
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state != ShortcutState::Pressed {
                        return;
                    }
                    let expected = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Space);
                    if *shortcut == expected {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let state: State<'_, Mutex<AppState>> = app.state();
                            match crate::dictation::toggle_recording(app.clone(), state).await {
                                Ok(msg) => log::info!("dictation: {msg}"),
                                Err(e) => log::error!("dictation error: {e}"),
                            }
                        });
                        return;
                    }
                    let (paste, copy) = {
                        let st = app.state::<Mutex<AppState>>();
                        let Ok(s) = st.lock() else {
                            return;
                        };
                        let paste = shortcuts::parse_shortcut(&s.settings.paste_last_key)
                            .ok()
                            .and_then(|sp| crate::hotkey::spec_to_shortcut(&sp).ok());
                        let copy = shortcuts::parse_shortcut(&s.settings.copy_last_key)
                            .ok()
                            .and_then(|sp| crate::hotkey::spec_to_shortcut(&sp).ok());
                        (paste, copy)
                    };
                    if paste.as_ref() == Some(shortcut) {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let state: State<'_, Mutex<AppState>> = app.state();
                            match crate::commands::paste_last(app.clone(), state) {
                                Ok(msg) => log::info!("{msg}"),
                                Err(e) => log::error!("paste-last: {e}"),
                            }
                        });
                    } else if copy.as_ref() == Some(shortcut) {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let state: State<'_, Mutex<AppState>> = app.state();
                            match crate::commands::copy_last(state) {
                                Ok(msg) => log::info!("{msg}"),
                                Err(e) => log::error!("copy-last: {e}"),
                            }
                        });
                    }
                })
                .build(),
        )
        .setup(|app| {
            let data_dir = app
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("dictflow");
            let _ = std::fs::create_dir_all(data_dir.join("models"));
            let _ = std::fs::create_dir_all(data_dir.join("bin"));

            let transcriber = crate::engine::Transcriber::spawn(data_dir.clone());
            let mut st = crate::state::AppState {
                recording: false,
                transcribing: false,
                hotkey_owned: false,
                active: None,
                settings: crate::settings::Settings::default(),
                history: Vec::new(),
                stats: crate::stats::UsageStats::default(),
                downloading: HashMap::new(),
                utility_shortcuts: Vec::new(),
                data_dir,
                transcriber,
                paste_target: None,
            };
            st.load_settings();
            st.load_history();
            st.stats = crate::stats::UsageStats::load(&st.data_dir);
            st.seed_stats_from_history();
            // Warm up the selected Parakeet model in the background so the
            // first dictation doesn't pay the model-load cost.
            if let Some(entry) = models::find(&st.settings.model_id) {
                if entry.engine == EngineKind::Parakeet && entry.is_downloaded(&st.data_dir) {
                    st.transcriber.preload(&st.settings.model_id);
                }
            }
            app.manage(Mutex::new(st));
            if let Err(e) = crate::tray::build_tray(app.handle()) {
                log::error!("tray setup failed: {e}");
            }
            crate::hotkey::apply_hotkey_registration(app.handle());
            crate::hotkey::apply_utility_shortcuts(app.handle());
            crate::tray::apply_overlay_visibility(app.handle());
            crate::focus::spawn_overlay_space_follow(app.handle());
            let onboarded = app
                .state::<Mutex<AppState>>()
                .lock()
                .map(|s| s.settings.onboarded)
                .unwrap_or(false);
            if crate::tray::wants_tray_launch(std::env::args(), onboarded) {
                crate::tray::hide_main_to_tray(app.handle());
            }

            // Single-key talk-button edges → start/stop. Lives for the app's
            // lifetime; the polling thread exits if the channel closes.
            let hk_app = app.handle().clone();
            let (hk_tx, mut hk_rx) =
                tokio::sync::mpsc::unbounded_channel::<crate::hotkey::TalkEdge>();
            crate::hotkey::spawn_hotkey_thread(hk_app.clone(), hk_tx);
            crate::perm::spawn_watcher(app.handle().clone());
            tauri::async_runtime::spawn(async move {
                // Tracks the moment of the last key-release when not recording.
                // Used to detect a double-tap (two taps within 500 ms) in
                // "double_tap" mode — hold-Fn to start, tap again to stop.
                let mut last_tap_end: Option<std::time::Instant> = None;

                while let Some(edge) = hk_rx.recv().await {
                    match edge {
                        crate::hotkey::TalkEdge::Pressed => {
                            let (mode, recording) = {
                                let s: State<'_, Mutex<AppState>> = hk_app.state();
                                let guard = s.lock().unwrap();
                                (guard.settings.recording_mode.clone(), guard.recording)
                            };
                            match mode.as_str() {
                                "toggle" => {
                                    if recording {
                                        let st: State<'_, Mutex<AppState>> = hk_app.state();
                                        match crate::dictation::stop_transcribe(&hk_app, st).await {
                                            Ok(msg) => log::info!("dictation: {msg}"),
                                            Err(e) => log::error!("dictation error: {e}"),
                                        }
                                    } else {
                                        let st: State<'_, Mutex<AppState>> = hk_app.state();
                                        let mut guard = st.lock().unwrap();
                                        let preferred = guard.settings.audio_device.clone();
                                        match crate::audio::start_recording(&mut guard) {
                                            Ok(choice) => {
                                                guard.hotkey_owned = true;
                                                drop(guard);
                                                crate::dictation::emit_device_fallback(&hk_app, preferred.as_deref(), &choice);
                                                let _ = hk_app.emit("dictflow://recording", true);
                                                crate::dictation::spawn_record_ticker(hk_app.clone());
                                            }
                                            Err(e) => log::error!("hotkey record failed: {e:#}"),
                                        }
                                    }
                                }
                                "double_tap" => {
                                    if recording {
                                        // Any press stops continuous recording.
                                        let st: State<'_, Mutex<AppState>> = hk_app.state();
                                        match crate::dictation::stop_transcribe(&hk_app, st).await {
                                            Ok(msg) => log::info!("dictation (double-tap stop): {msg}"),
                                            Err(e) => log::error!("dictation error: {e}"),
                                        }
                                        last_tap_end = None;
                                    } else {
                                        let is_double = last_tap_end
                                            .map(|t| t.elapsed() < std::time::Duration::from_millis(500))
                                            .unwrap_or(false);
                                        if is_double {
                                            // Second tap within 500 ms → start continuous dictation.
                                            last_tap_end = None;
                                            let st: State<'_, Mutex<AppState>> = hk_app.state();
                                            let mut guard = st.lock().unwrap();
                                            let preferred = guard.settings.audio_device.clone();
                                            match crate::audio::start_recording(&mut guard) {
                                                Ok(choice) => {
                                                    guard.hotkey_owned = true;
                                                    drop(guard);
                                                    crate::dictation::emit_device_fallback(&hk_app, preferred.as_deref(), &choice);
                                                    let _ = hk_app.emit("dictflow://recording", true);
                                                    crate::dictation::spawn_record_ticker(hk_app.clone());
                                                }
                                                Err(e) => log::error!("hotkey record failed: {e:#}"),
                                            }
                                        }
                                        // else: first tap — wait for possible second tap (handled in Released).
                                    }
                                }
                                _ => {
                                    // "hold" (default): Pressed starts, Released stops.
                                    if !recording {
                                        let st: State<'_, Mutex<AppState>> = hk_app.state();
                                        let mut guard = st.lock().unwrap();
                                        let preferred = guard.settings.audio_device.clone();
                                        match crate::audio::start_recording(&mut guard) {
                                            Ok(choice) => {
                                                guard.hotkey_owned = true;
                                                drop(guard);
                                                crate::dictation::emit_device_fallback(&hk_app, preferred.as_deref(), &choice);
                                                let _ = hk_app.emit("dictflow://recording", true);
                                                crate::dictation::spawn_record_ticker(hk_app.clone());
                                            }
                                            Err(e) => log::error!("hotkey record failed: {e:#}"),
                                        }
                                    }
                                }
                            }
                        }
                        crate::hotkey::TalkEdge::Released => {
                            let (mode, recording, owned) = {
                                let s: State<'_, Mutex<AppState>> = hk_app.state();
                                let guard = s.lock().unwrap();
                                (
                                    guard.settings.recording_mode.clone(),
                                    guard.recording,
                                    guard.hotkey_owned,
                                )
                            };
                            match mode.as_str() {
                                "hold" => {
                                    if recording && owned {
                                        let st: State<'_, Mutex<AppState>> = hk_app.state();
                                        match crate::dictation::stop_transcribe(&hk_app, st).await {
                                            Ok(msg) => log::info!("dictation: {msg}"),
                                            Err(e) => log::error!("dictation error: {e}"),
                                        }
                                    }
                                }
                                // Record when the key came back up so we can
                                // detect a second tap within 500 ms.
                                "double_tap" if !recording => {
                                    last_tap_end = Some(std::time::Instant::now());
                                }
                                _ => {} // "toggle": nothing to do on release
                            }
                        }
                        crate::hotkey::TalkEdge::Cancel => {
                            let owned = {
                                let s: State<'_, Mutex<AppState>> = hk_app.state();
                                let guard = s.lock().unwrap();
                                guard.recording && guard.hotkey_owned
                            };
                            if owned {
                                let st: State<'_, Mutex<AppState>> = hk_app.state();
                                let mut guard = st.lock().unwrap();
                                match crate::dictation::cancel_recording(&mut guard) {
                                    Ok(()) => {
                                        drop(guard);
                                        let _ = hk_app.emit("dictflow://recording", false);
                                        log::info!("dictation cancelled (other key pressed)");
                                    }
                                    Err(e) => log::error!("dictation cancel failed: {e:#}"),
                                }
                            }
                        }
                        crate::hotkey::TalkEdge::Escape => {
                            let recording = {
                                let s: State<'_, Mutex<AppState>> = hk_app.state();
                                let rec = s.lock().unwrap().recording;
                                rec
                            };
                            if recording {
                                let st: State<'_, Mutex<AppState>> = hk_app.state();
                                let mut guard = st.lock().unwrap();
                                match crate::dictation::cancel_recording(&mut guard) {
                                    Ok(()) => {
                                        drop(guard);
                                        let _ = hk_app.emit("dictflow://recording", false);
                                        log::info!("dictation cancelled (Esc)");
                                    }
                                    Err(e) => log::error!("dictation cancel failed: {e:#}"),
                                }
                            }
                        }
                    }
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() != "main" {
                return;
            }
            crate::tray::on_main_window_event(window, event);
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_models,
            select_model,
            download_model,
            cancel_download,
            get_history,
            get_stats,
            delete_history_item,
            clear_history,
            get_dictionary,
            add_dictionary_entry,
            set_dictionary_enabled,
            delete_dictionary_entry,
            add_symbol_presets,
            export_dictionary,
            import_dictionary,
            star_dictionary_entry,
            add_correction,
            check_for_updates,
            get_settings,
            set_settings,
            get_provider_status,
            set_provider_secrets,
            test_llm,
            test_audio_api,
            toggle_recording,
            cancel_dictation,
            copy_last,
            paste_last,
            snap_overlay,
            set_overlay_pose,
            onboard_step,
            copy_chip_open,
            transcribe_file,
            delete_model,
            get_audio_devices,
            test_microphone,
            open_mic_settings,
            open_accessibility_settings,
            open_input_monitoring_settings,
            get_permissions,
            request_permission,
            read_audio_file
        ]);

    builder
        .build(tauri::generate_context!())
        .expect("error while building DictFlow")
        .run(|app, event| {
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = event {
                crate::tray::show_main_window(app);
            }
            #[cfg(not(target_os = "macos"))]
            let _ = (app, event);
        });
}

fn main() {
    run();
}
