//! Dictation session: start/stop, live meter, transcribe, paste.

use std::path::{Path, PathBuf};
use std::sync::{atomic::Ordering, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::{Emitter, Manager, State};

use crate::audio::{finish_recording, start_recording, stop_and_save_wav};
use crate::devices;
use crate::engine::{
    polish_cfg, run_file_transcription, run_segmented, run_transcription, segment_stt, SttSettings,
    TranscribeInput,
};
use crate::overlay;
use crate::paste::{paste_target_for, paste_text, remember_paste_target};
use crate::segment_jobs::{next_take_id, SegmentSession};
use crate::segmenter::{find_vad_model, make_source};
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

pub(crate) fn emit_device_fallback(
    app: &tauri::AppHandle,
    preferred: Option<&str>,
    choice: &devices::DeviceChoice,
) {
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
    spawn_segmenter(app.clone());
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
            let _ = app.emit(
                "dictflow://level",
                LevelEvent {
                    peak: level.peak,
                    rms: level.rms,
                    bins,
                    elapsed,
                },
            );
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

/// Bundled Silero VAD model file name.
const VAD_MODEL_FILE: &str = "silero_vad.onnx";

/// Segmenting starts this long after recording begins. Takes shorter than the
/// first segment never need it, and the 1.5 s Setup mic test never builds a VAD.
const SEGMENT_START_DELAY: Duration = Duration::from_millis(2000);

/// Find a verified Silero model, or `None` (the energy detector takes over).
pub(crate) fn resolve_vad_model(app: &tauri::AppHandle, data_dir: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(res) = app.path().resource_dir() {
        candidates.push(res.join("resources").join(VAD_MODEL_FILE));
        candidates.push(res.join(VAD_MODEL_FILE));
    }
    candidates.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("resources")
            .join(VAD_MODEL_FILE),
    );
    candidates.push(data_dir.join("models").join(VAD_MODEL_FILE));
    find_vad_model(&candidates)
}

/// Inputs for building a segment session outside the state lock.
struct SegmentBuild {
    rate: u32,
    stt: SttSettings,
    transcriber: crate::engine::Transcriber,
    data_dir: PathBuf,
}

/// While recording: cut the take at pauses and hand each piece to a background
/// worker, so most of the audio is already transcribed when the user stops.
fn spawn_segmenter(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut take_started: Option<std::time::Instant> = None;
        let mut vad_model: Option<Option<PathBuf>> = None;
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let build: Option<SegmentBuild> = {
                let st = app.state::<Mutex<AppState>>();
                let Ok(mut s) = st.lock() else {
                    break;
                };
                if !s.recording || !s.settings.segment_dictation {
                    break;
                }
                let Some(rec) = s.active.as_ref() else {
                    break;
                };
                let (started, rate) = (rec.started_at, rec.sample_rate);
                let has_session = rec.segments.is_some();
                let old_enough = started.elapsed() >= SEGMENT_START_DELAY;
                match take_started {
                    None => take_started = Some(started),
                    // A newer take replaced the one this task started on.
                    Some(t) if t != started => break,
                    Some(_) => {}
                }
                if has_session {
                    if let Some(rec) = s.active.as_mut() {
                        let samples = rec.samples.clone();
                        let all = samples.lock().unwrap();
                        if let Some(session) = rec.segments.as_mut() {
                            session.poll(&all);
                        }
                    }
                    None
                } else if !old_enough {
                    None
                } else {
                    Some(SegmentBuild {
                        rate,
                        stt: SttSettings::from_settings(&s.settings, &s.data_dir),
                        transcriber: s.transcriber.clone(),
                        data_dir: s.data_dir.clone(),
                    })
                }
            };
            if let Some(SegmentBuild {
                rate,
                stt,
                transcriber,
                data_dir,
            }) = build
            {
                {
                    let model = vad_model
                        .get_or_insert_with(|| resolve_vad_model(&app, &data_dir))
                        .clone();
                    let cfg = stt.segment_cfg();
                    let built = tokio::task::spawn_blocking(move || {
                        let (source, label) = make_source(rate, model.as_deref());
                        let stt_fn = segment_stt(stt, transcriber, next_take_id());
                        SegmentSession::new(rate, cfg, source, label, stt_fn)
                    })
                    .await;
                    let Ok(session) = built else {
                        break;
                    };
                    let st = app.state::<Mutex<AppState>>();
                    let Ok(mut s) = st.lock() else {
                        break;
                    };
                    let recording = s.recording;
                    match s.active.as_mut() {
                        Some(rec) if Some(rec.started_at) == take_started && recording => {
                            log::info!(
                                "dictation segmenting on ({}, min {:.0}s, hard {:.0}s)",
                                session.source_label(),
                                cfg.min_secs,
                                cfg.hard_secs
                            );
                            rec.segments = Some(session);
                        }
                        _ => {
                            session.cancel();
                            break;
                        }
                    }
                }
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
    state
        .history
        .first()
        .map(|h| h.text.clone())
        .filter(|t| !t.is_empty())
}

#[tauri::command]
pub(crate) async fn toggle_recording(
    app: tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
) -> Result<String, String> {
    remember_paste_target(&app);
    if state.lock().unwrap().transcribing {
        return Err("a transcription is already in progress".to_owned());
    }
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
    let (input, parakeet, duration_secs, history_model, audio, cleanup, segments) = {
        let mut s = state.lock().unwrap();
        s.hotkey_owned = false;
        let take = finish_recording(&mut s).map_err(|e| e.to_string())?;
        let (wav, duration_secs, segments) = (take.wav_path, take.duration_secs, take.segments);
        s.transcribing = true;
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
            s1_runtime: s.s1_runtime.clone(),
        };
        (
            input,
            s.transcriber.clone(),
            duration_secs,
            history_model,
            audio,
            cleanup,
            segments,
        )
    };
    set_transcribing(app, true);
    let _ = app.emit("dictflow://recording", false);

    // STT + post-processing can take seconds (model load, inference) — keep it
    // off the async runtime. A segmented take only has its tail left to decode.
    let wav_for_cleanup = input.wav_path.clone();
    let result = tokio::task::spawn_blocking(move || match segments {
        Some(finished) => run_segmented(&input, &parakeet, finished, duration_secs),
        None => run_transcription(&input, &parakeet),
    })
    .await;
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
    if let Some(note) = &result.cleanup_note {
        let _ = app.emit("dictflow://cleanup-fallback", note.clone());
    }
    let text = result.text.clone();
    if text.is_empty() {
        let _ = std::fs::remove_file(&wav_for_cleanup);
        return Ok("nothing to paste after cleanup".to_owned());
    }

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
        return Ok(format!(
            "transcribed (auto-paste off): {}",
            truncate(&text, 160)
        ));
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
    let owned = {
        let mut s = state.lock().unwrap();
        if s.recording || s.transcribing {
            return Err("finish the current recording or transcription first".to_owned());
        }
        let dir = s.data_dir.join("recordings");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        s.transcribing = true;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        dir.join(format!("file-{stamp}.wav"))
    };
    let _ = app.emit("dictflow://transcribing", true);
    // Copy into our recordings folder: playback keeps working if the original
    // moves, and the webview can load it back by file name.
    if let Err(error) = std::fs::copy(&path, &owned) {
        set_transcribing(&app, false);
        return Err(format!("copy audio file: {error}"));
    }
    let (input, parakeet, segment_cfg, data_dir) = {
        let s = state.lock().unwrap();
        let segment_cfg = s
            .settings
            .segment_dictation
            .then(|| SttSettings::from_settings(&s.settings, &s.data_dir).segment_cfg());
        let input = TranscribeInput {
            wav_path: owned.clone(),
            model_id: s.settings.model_id.clone(),
            language: s.settings.language.clone(),
            cleanup: s.settings.cleanup.clone(),
            translate: s.settings.translate,
            data_dir: s.data_dir.clone(),
            dictionary: text::load_dictionary(&s.data_dir),
            polish: polish_cfg(&s.settings, &s.data_dir),
            s1_runtime: s.s1_runtime.clone(),
        };
        (
            input,
            s.transcriber.clone(),
            segment_cfg,
            s.data_dir.clone(),
        )
    };
    let segmenting = segment_cfg.map(|cfg| (cfg, resolve_vad_model(&app, &data_dir)));
    let result =
        tokio::task::spawn_blocking(move || run_file_transcription(&input, &parakeet, segmenting))
            .await;
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
    if let Some(note) = &result.cleanup_note {
        let _ = app.emit("dictflow://cleanup-fallback", note.clone());
    }
    if result.text.is_empty() {
        let _ = std::fs::remove_file(&owned);
        return Ok("nothing to save after cleanup".to_owned());
    }
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
            refine_busy: std::sync::atomic::AtomicBool::new(false),
            refining: false,
            data_dir: dir,
            transcriber: Transcriber::spawn(std::env::temp_dir()),
            s1_runtime: crate::s1_runtime::S1Runtime::new(),
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

    // ---- chunked dictation (group D) ----

    use crate::segment_jobs::SttFn;
    use crate::state::ActiveRecording;
    use crate::segmenter::{EnergyPauseSource, PauseSource, SegmentCfg};
    use std::sync::{atomic::AtomicBool, Arc};

    const TEST_RATE: u32 = 16_000;

    /// Speech-like tone, a digital-silence pause, then more tone.
    fn talk_pause_talk() -> Vec<f32> {
        let tone = |secs: f32| -> Vec<f32> {
            (0..(TEST_RATE as f32 * secs) as usize)
                .map(|i| (i as f32 * 0.1).sin() * 0.3)
                .collect()
        };
        let mut v = tone(1.6);
        v.extend(std::iter::repeat_n(0.0, (TEST_RATE as f32 * 0.7) as usize));
        v.extend(tone(1.6));
        v
    }

    fn recording_with_session(samples: Vec<f32>) -> ActiveRecording {
        let cfg = SegmentCfg {
            min_secs: 1.0,
            hard_secs: 3.0,
            pause_at_min_ms: 300.0,
            pause_at_hard_ms: 150.0,
        };
        let stt: SttFn = Arc::new(|_, _| Ok("hello".to_owned()));
        let source: Box<dyn PauseSource> = Box::new(EnergyPauseSource::new());
        let mut session = SegmentSession::new(TEST_RATE, cfg, source, "energy", stt);
        session.poll(&samples);
        ActiveRecording {
            samples: Arc::new(Mutex::new(samples)),
            sample_rate: TEST_RATE,
            stop: Arc::new(AtomicBool::new(false)),
            done: Arc::new(AtomicBool::new(true)),
            started_at: std::time::Instant::now(),
            stream_error: Arc::new(AtomicBool::new(false)),
            segments: Some(session),
        }
    }

    fn wav_files(state: &AppState) -> usize {
        std::fs::read_dir(state.data_dir.join("recordings"))
            .map(|d| d.filter_map(|e| e.ok()).count())
            .unwrap_or(0)
    }

    #[test]
    fn cancel_with_active_segment_session_clears_state_and_file() {
        let mut s = test_state();
        s.recording = true;
        s.active = Some(recording_with_session(talk_pause_talk()));
        cancel_recording(&mut s).expect("cancel");
        assert!(s.active.is_none());
        assert!(!s.recording);
        assert_eq!(wav_files(&s), 0, "cancelled take must not leave a wav");
        assert!(s.history.is_empty());
        let _ = std::fs::remove_dir_all(&s.data_dir);
    }

    #[test]
    fn finish_recording_hands_back_segments_and_wav() {
        let mut s = test_state();
        s.recording = true;
        let samples = talk_pause_talk();
        let total = samples.len();
        s.active = Some(recording_with_session(samples));
        let take = finish_recording(&mut s).expect("finish");
        assert!(s.active.is_none());
        assert!(!s.recording);
        assert!(take.wav_path.exists());
        assert!(take.duration_secs >= 0.0);
        let finished = take.segments.expect("a pause inside the take must cut it");
        assert_eq!(finished.cuts.len(), 1);
        assert_eq!(finished.source_label, "energy");
        assert!(finished.cuts[0].index > 0 && finished.cuts[0].index < total);
        assert_eq!(finished.tail_16k.len(), total - finished.cuts[0].index);
        let results = finished.worker.finish();
        assert_eq!(results.len(), 1, "only the pre-cut piece was queued");
        let _ = std::fs::remove_dir_all(&s.data_dir);
    }

    #[test]
    fn stop_and_save_wav_discards_segment_work_but_keeps_the_file() {
        let mut s = test_state();
        s.recording = true;
        s.active = Some(recording_with_session(talk_pause_talk()));
        let (wav, _) = stop_and_save_wav(&mut s).expect("stop");
        assert!(wav.exists());
        assert!(s.active.is_none());
        let _ = std::fs::remove_dir_all(&s.data_dir);
    }

    #[test]
    fn short_take_without_a_cut_has_no_segments() {
        let mut s = test_state();
        s.recording = true;
        let samples: Vec<f32> = (0..TEST_RATE as usize / 2)
            .map(|i| (i as f32 * 0.1).sin() * 0.3)
            .collect();
        s.active = Some(recording_with_session(samples));
        let take = finish_recording(&mut s).expect("finish");
        assert!(take.segments.is_none());
        assert!(take.wav_path.exists());
        let _ = std::fs::remove_dir_all(&s.data_dir);
    }
}
