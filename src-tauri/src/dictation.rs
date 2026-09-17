//! Dictation session: start/stop, live meter, transcribe, paste.

use std::path::Path;
use std::sync::{Mutex, atomic::Ordering};
use std::time::Duration;

use serde::Serialize;
use tauri::{Emitter, Manager, State};

use crate::audio::{start_recording, stop_and_save_wav};
use crate::devices;
use crate::engine::{polish_cfg, run_transcription, TranscribeInput};
use crate::overlay;
use crate::paste::{paste_target_for, paste_text, remember_paste_target};
use crate::session;
use crate::settings::{active_speech_id, truncate, NewHistory};
use crate::state::AppState;
use crate::text;

#[derive(Debug, Clone, Serialize)]
struct LevelEvent {
    peak: f32,
    rms: f32,
    bins: Vec<f32>,
    elapsed: f64,
}

#[derive(Debug, Clone, Serialize)]
struct DeviceFallback {
    from: String,
    to: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CommittedEvent {
    text: String,
    chip_ms: u64,
}

pub(crate) fn committed(text: String) -> CommittedEvent {
    CommittedEvent {
        text,
        chip_ms: overlay::COPY_CHIP_MS,
    }
}

pub(crate) fn set_transcribing(app: &tauri::AppHandle, on: bool) {
    if let Ok(mut s) = app.state::<Mutex<AppState>>().lock() {
        s.transcribing = on;
    }
    let _ = app.emit("dictflow://transcribing", on);
}

pub(crate) fn emit_device_fallback(app: &tauri::AppHandle, preferred: Option<&str>, choice: &devices::DeviceChoice) {
    if choice.fell_back {
        let _ = app.emit(
            "dictflow://device-fallback",
            DeviceFallback {
                from: preferred.unwrap_or("").to_owned(),
                to: choice.name.clone(),
            },
        );
    }
}

/// Live peak/RMS + 19-minute warn / optional 20-minute cap. Not used for the
/// 1.5 s Setup mic test.
pub(crate) fn spawn_record_ticker(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut warned = false;
        loop {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let snap = {
                let st = app.state::<Mutex<AppState>>();
                let Ok(s) = st.lock() else {
                    break;
                };
                if !s.recording {
                    break;
                }
                let Some(rec) = s.active.as_ref() else {
                    break;
                };
                let samples = rec.samples.lock().unwrap();
                let start = samples.len().saturating_sub(1920);
                let slice = &samples[start..];
                let level = devices::level_from_samples(slice);
                let bins = devices::peak_bins(slice, devices::WAVE_BINS);
                drop(samples);
                let elapsed = rec.started_at.elapsed().as_secs_f64();
                let err = rec.stream_error.load(Ordering::SeqCst);
                let cap = s.settings.session_cap;
                (level, bins, elapsed, err, cap)
            };
            let (level, bins, elapsed, stream_err, cap) = snap;
            let _ = app.emit("dictflow://level", LevelEvent {
                peak: level.peak,
                rms: level.rms,
                bins,
                elapsed,
            });
            if stream_err {
                let st: State<'_, Mutex<AppState>> = app.state();
                if let Ok(mut guard) = st.lock() {
                    if guard.recording {
                        let _ = cancel_recording(&mut guard);
                    }
                }
                let _ = app.emit("dictflow://recording", false);
                let _ = app.emit(
                    "dictflow://device-fallback",
                    DeviceFallback {
                        from: String::new(),
                        to: "stream error — take discarded".to_owned(),
                    },
                );
                break;
            }
            match session::session_tick(elapsed, warned, cap) {
                session::SessionTick::Warn => {
                    warned = true;
                    let _ = app.emit("dictflow://session-warn", ());
                }
                session::SessionTick::Cap => {
                    let st: State<'_, Mutex<AppState>> = app.state();
                    match stop_transcribe(&app, st).await {
                        Ok(msg) => log::info!("session cap: {msg}"),
                        Err(e) => log::error!("session cap stop failed: {e}"),
                    }
                    break;
                }
                session::SessionTick::None => {}
            }
        }
    });
}

/// Discard the live recording without transcribing, filing, or pasting
/// (SpeakType's cancel path: modifier-combo pressed mid-take).
pub(crate) fn cancel_recording(state: &mut AppState) -> anyhow::Result<()> {
    state.hotkey_owned = false;
    let (wav, _) = stop_and_save_wav(state)?;
    let _ = std::fs::remove_file(wav);
    Ok(())
}

#[tauri::command]
pub(crate) fn cancel_dictation(
    app: tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
) -> Result<String, String> {
    let mut s = state.lock().unwrap();
    if !s.recording {
        return Err("not recording".to_owned());
    }
    cancel_recording(&mut s).map_err(|e| e.to_string())?;
    s.transcribing = false;
    drop(s);
    let _ = app.emit("dictflow://recording", false);
    let _ = app.emit("dictflow://transcribing", false);
    Ok("cancelled".to_owned())
}

pub(crate) fn last_text(state: &AppState) -> Option<String> {
    state.history.first().map(|h| h.text.clone()).filter(|t| !t.is_empty())
}

#[tauri::command]
pub(crate) async fn toggle_recording(
    app: tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
) -> Result<String, String> {
    remember_paste_target(&app);
    if !state.lock().unwrap().recording {
        let mut s = state.lock().unwrap();
        let preferred = s.settings.audio_device.clone();
        let choice = start_recording(&mut s).map_err(|e| e.to_string())?;
        s.hotkey_owned = false;
        drop(s);
        emit_device_fallback(&app, preferred.as_deref(), &choice);
        let _ = app.emit("dictflow://recording", true);
        spawn_record_ticker(app.clone());
        return Ok("recording… press hotkey again to transcribe".to_owned());
    }
    stop_transcribe(&app, state).await
}

/// Stop the live recording, transcribe, file it in history, and paste.
/// Shared by the UI toggle, the tray item, and the talk-key handlers.
pub(crate) async fn stop_transcribe(
    app: &tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
) -> Result<String, String> {

    // Snapshot everything the heavy work needs, then release the lock.
    let (input, parakeet, duration_secs, history_model, audio, cleanup) = {
        let mut s = state.lock().unwrap();
        s.hotkey_owned = false;
        let (wav, duration_secs) = stop_and_save_wav(&mut s).map_err(|e| e.to_string())?;
        let model_id = s.settings.model_id.clone();
        let history_model = active_speech_id(&s.settings);
        let cleanup = s.settings.cleanup.clone();
        let audio = wav.display().to_string();
        let input = TranscribeInput {
            wav_path: wav,
            model_id: model_id.clone(),
            language: s.settings.language.clone(),
            cleanup: cleanup.clone(),
            translate: s.settings.translate,
            data_dir: s.data_dir.clone(),
            dictionary: text::load_dictionary(&s.data_dir),
            polish: polish_cfg(&s.settings, &s.data_dir),
        };
        (input, s.transcriber.clone(), duration_secs, history_model, audio, cleanup)
    };
    set_transcribing(app, true);
    let _ = app.emit("dictflow://recording", false);

    // STT + post-processing can take seconds (model load, inference) — keep it
    // off the async runtime.
    let wav_for_cleanup = input.wav_path.clone();
    let result = tokio::task::spawn_blocking(move || run_transcription(&input, &parakeet)).await;
    set_transcribing(app, false);
    let result = result
        .map_err(|e| format!("transcription task failed: {e}"))
        .and_then(|r| r.map_err(|e| format!("{e:#}")));
    if let Err(e) = &result {
        if devices::is_blank_audio_error(e) {
            let _ = std::fs::remove_file(&wav_for_cleanup);
        }
        return Err(e.clone());
    }
    let result = result.unwrap();
    let text = result.text.clone();

    let (auto_paste, saved_target) = {
        let mut s = state.lock().unwrap();
        s.push_history(NewHistory {
            text: result.text,
            raw_text: result.raw_text,
            duration_secs,
            model: history_model,
            audio_path: Some(audio),
            dict_hits: result.dict_hits,
            cleanup,
        });
        (s.settings.auto_paste, s.paste_target)
    };
    let _ = app.emit("dictflow://history-updated", ());
    let _ = app.emit("dictflow://committed", committed(text.clone()));

    if !auto_paste {
        return Ok(format!("transcribed (auto-paste off): {}", truncate(&text, 160)));
    }
    // Pasting must not kill the transcription if the foreground app rejects keys.
    match paste_text(app, &text, paste_target_for(app, saved_target)) {
        Ok(()) => Ok(format!("pasted: {}", truncate(&text, 80))),
        Err(e) => Ok(format!(
            "transcribed (paste failed: {e}): {}",
            truncate(&text, 160)
        )),
    }
}

/// Transcribe an audio file on disk (WAV for now — the loader reads PCM WAV;
/// mp3/video need a decoder and are a v0.3 item). Saves to history like a
/// dictation. Mirrors SpeakType's TranscribeAudioView minus drag-drop.
#[tauri::command]
pub(crate) async fn transcribe_file(
    app: tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
    path: String,
) -> Result<String, String> {
    if !Path::new(&path).is_file() {
        return Err("file not found".to_owned());
    }
    // Copy into our recordings folder: playback keeps working if the original
    // moves, and the webview can load it back by file name.
    let owned = {
        let s = state.lock().unwrap();
        let dir = s.data_dir.join("recordings");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        dir.join(format!("file-{stamp}.wav"))
    };
    std::fs::copy(&path, &owned).map_err(|e| format!("copy audio file: {e}"))?;
    let (input, parakeet) = {
        let s = state.lock().unwrap();
        let input = TranscribeInput {
            wav_path: owned.clone(),
            model_id: s.settings.model_id.clone(),
            language: s.settings.language.clone(),
            cleanup: s.settings.cleanup.clone(),
            translate: s.settings.translate,
            data_dir: s.data_dir.clone(),
            dictionary: text::load_dictionary(&s.data_dir),
            polish: polish_cfg(&s.settings, &s.data_dir),
        };
        (input, s.transcriber.clone())
    };
    set_transcribing(&app, true);
    let result = tokio::task::spawn_blocking(move || run_transcription(&input, &parakeet)).await;
    set_transcribing(&app, false);
    let result = result
        .map_err(|e| format!("transcription task failed: {e}"))
        .and_then(|r| r.map_err(|e| format!("{e:#}")));
    if let Err(e) = &result {
        if devices::is_blank_audio_error(e) {
            let _ = std::fs::remove_file(&owned);
        }
        return Err(e.clone());
    }
    let result = result.unwrap();
    {
        let mut s = state.lock().unwrap();
        let model = active_speech_id(&s.settings);
        let cleanup = s.settings.cleanup.clone();
        s.push_history(NewHistory {
            text: result.text.clone(),
            raw_text: result.raw_text,
            duration_secs: result.duration_secs,
            model,
            audio_path: Some(owned.display().to_string()),
            dict_hits: result.dict_hits,
            cleanup,
        });
    }
    let _ = app.emit("dictflow://history-updated", ());
    let _ = app.emit("dictflow://committed", committed(result.text.clone()));
    Ok(result.text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Transcriber;
    use crate::settings::{NewHistory, Settings};
    use crate::stats::UsageStats;
    use std::collections::HashMap;

    fn test_state() -> AppState {
        let dir = std::env::temp_dir().join(format!(
            "dictflow-cancel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        AppState {
            recording: false,
            transcribing: false,
            hotkey_owned: false,
            active: None,
            settings: Settings::default(),
            history: Vec::new(),
            stats: UsageStats::default(),
            downloading: HashMap::new(),
            utility_shortcuts: Vec::new(),
            data_dir: dir,
            transcriber: Transcriber::spawn(std::env::temp_dir()),
            paste_target: None,
        }
    }

    #[test]
    fn cancel_does_not_touch_stats() {
        let mut s = test_state();
        s.push_history(NewHistory {
            text: "keep".into(),
            raw_text: "keep".into(),
            duration_secs: 1.0,
            model: "test".into(),
            audio_path: None,
            dict_hits: 0,
            cleanup: "full".into(),
        });
        assert_eq!(s.stats.lifetime_dictations, 1);
        assert_eq!(s.history.len(), 1);
        assert!(cancel_recording(&mut s).is_err());
        assert_eq!(s.stats.lifetime_dictations, 1);
        assert_eq!(s.history.len(), 1);
        let _ = std::fs::remove_dir_all(&s.data_dir);
    }
}

