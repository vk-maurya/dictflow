//! Microphone capture. cpal's `Stream` is `!Send`, so it lives on one thread.
//!
//! Device names and fallback policy are shared; WASAPI vs CoreAudio is cpal's
//! host. Error copy is platform-gated.

use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;

use anyhow::Context;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::Serialize;
use tauri::State;

use crate::devices;
use crate::state::{ActiveRecording, AppState};
use crate::text;

fn device_label(dev: &cpal::Device) -> Option<String> {
    let desc = dev.description().ok()?;
    Some(devices::display_name(desc.name(), desc.extended()))
}

fn device_matches(dev: &cpal::Device, want: &str) -> bool {
    let Ok(desc) = dev.description() else {
        return false;
    };
    devices::name_matches(want, desc.name(), desc.extended())
}

pub(crate) fn default_input_name() -> Option<String> {
    device_label(&cpal::default_host().default_input_device()?)
}

fn list_input_names() -> (Vec<String>, Option<String>) {
    let host = cpal::default_host();
    let default_name = host.default_input_device().as_ref().and_then(device_label);
    let names = host
        .input_devices()
        .map(|devs| {
            let mut names = Vec::new();
            for d in devs {
                if let Some(n) = device_label(&d) {
                    if !names.iter().any(|e| e == &n) {
                        names.push(n);
                    }
                }
            }
            names
        })
        .unwrap_or_default();
    (names, default_name)
}

fn label_for_preferred(want: &str) -> Option<String> {
    let host = cpal::default_host();
    let devs = host.input_devices().ok()?;
    for d in devs {
        if device_matches(&d, want) {
            return device_label(&d);
        }
    }
    None
}

pub(crate) fn resolve_capture_device(preferred: Option<&str>) -> anyhow::Result<devices::DeviceChoice> {
    let (names, default_name) = list_input_names();
    if let Some(want) = preferred.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(label) = label_for_preferred(want) {
            return Ok(devices::DeviceChoice {
                name: label,
                fell_back: false,
            });
        }
    }
    devices::choose_device(&names, default_name.as_deref(), preferred).map_err(anyhow::Error::msg)
}

pub(crate) fn open_input_named(name: &str) -> anyhow::Result<cpal::Device> {
    let host = cpal::default_host();
    if let Ok(devs) = host.input_devices() {
        for d in devs {
            if device_matches(&d, name) {
                return Ok(d);
            }
        }
    }
    #[cfg(target_os = "windows")]
    let hint = "no input device found — connect a microphone and check \
                Windows Settings → Privacy & security → Microphone (allow desktop apps)";
    #[cfg(target_os = "macos")]
    let hint = "no input device found — connect a microphone and grant \
                Microphone access in System Settings → Privacy & Security → Microphone";
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let hint = "no input device found — connect a microphone and check microphone permissions";
    host.default_input_device().context(hint)
}

fn run_capture(
    ready_tx: mpsc::Sender<Result<u32, String>>,
    samples: Arc<Mutex<Vec<f32>>>,
    stop: Arc<AtomicBool>,
    device_name: String,
    stream_error: Arc<AtomicBool>,
) {
    // NOTE: the stream MUST be returned out of this closure — if it is dropped
    // here, capture stops instantly and every recording comes back silent.
    let result = (|| -> anyhow::Result<(cpal::Stream, u32)> {
        let device = open_input_named(&device_name)?;
        let supported = device
            .default_input_config()
            .context("no default input config")?;

        let writer = samples.clone();
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => {
                let config: cpal::StreamConfig = supported.clone().into();
                let flag = stream_error.clone();
                device.build_input_stream(
                    &config,
                    move |data: &[f32], _| {
                        writer.lock().unwrap().extend_from_slice(data);
                    },
                    {
                        let flag = flag.clone();
                        move |err| {
                            log::warn!("audio stream error: {err}");
                            flag.store(true, Ordering::SeqCst);
                        }
                    },
                    None,
                )?
            }
            cpal::SampleFormat::I16 => {
                let config: cpal::StreamConfig = supported.clone().into();
                let flag = stream_error.clone();
                device.build_input_stream(
                    &config,
                    move |data: &[i16], _| {
                        let mut lock = writer.lock().unwrap();
                        lock.extend(data.iter().map(|s| *s as f32 / i16::MAX as f32));
                    },
                    move |err| {
                        log::warn!("audio stream error: {err}");
                        flag.store(true, Ordering::SeqCst);
                    },
                    None,
                )?
            }
            cpal::SampleFormat::U16 => {
                let config: cpal::StreamConfig = supported.clone().into();
                let flag = stream_error.clone();
                device.build_input_stream(
                    &config,
                    move |data: &[u16], _| {
                        let mut lock = writer.lock().unwrap();
                        lock.extend(
                            data.iter()
                                .map(|s| (*s as f32 / u16::MAX as f32) * 2.0 - 1.0),
                        );
                    },
                    move |err| {
                        log::warn!("audio stream error: {err}");
                        flag.store(true, Ordering::SeqCst);
                    },
                    None,
                )?
            }
            other => anyhow::bail!("unsupported sample format: {other:?}"),
        };
        stream.play()?;
        Ok((stream, supported.sample_rate()))
    })();

    match result {
        Ok((_stream, rate)) => {
            let _ = ready_tx.send(Ok(rate));
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(50));
            }
            // `_stream` is dropped here, releasing the microphone.
        }
        Err(e) => {
            let _ = ready_tx.send(Err(format!("{e:#}")));
        }
    }
}

pub(crate) fn start_recording(state: &mut AppState) -> anyhow::Result<devices::DeviceChoice> {
    if state.recording {
        anyhow::bail!("already recording");
    }
    state.active = None; // defensive: drop any stale session

    let preferred = state.settings.audio_device.clone();
    let choice = resolve_capture_device(preferred.as_deref())?;

    let samples: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let stream_error = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = mpsc::channel();

    let t_samples = samples.clone();
    let t_stop = stop.clone();
    let t_done = done.clone();
    let t_err = stream_error.clone();
    let t_name = choice.name.clone();
    std::thread::Builder::new()
        .name("dictflow-capture".to_owned())
        .spawn(move || {
            run_capture(ready_tx, t_samples, t_stop, t_name, t_err);
            t_done.store(true, Ordering::SeqCst);
        })
        .context("spawn capture thread")?;

    // Block until the stream is actually playing so the first syllable isn't
    // lost — and so a missing microphone surfaces as an error, not silence.
    let sample_rate = ready_rx
        .recv_timeout(Duration::from_secs(5))
        .context("capture thread did not respond")?
        .map_err(anyhow::Error::msg)?;

    state.recording = true;
    state.active = Some(ActiveRecording {
        samples,
        sample_rate,
        stop,
        done,
        started_at: std::time::Instant::now(),
        stream_error,
    });
    Ok(choice)
}

pub(crate) fn stop_and_save_wav(state: &mut AppState) -> anyhow::Result<(PathBuf, f64)> {
    let rec = state.active.take().context("not recording")?;
    state.recording = false;
    let duration_secs = rec.started_at.elapsed().as_secs_f64();
    rec.stop.store(true, Ordering::SeqCst);

    // Wait (bounded) for the capture thread to drop the stream and release
    // the microphone before reading the buffer.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !rec.done.load(Ordering::SeqCst) {
        if std::time::Instant::now() > deadline {
            anyhow::bail!("capture thread did not stop in time");
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let samples = rec.samples.lock().unwrap().clone();
    if samples.is_empty() {
        #[cfg(target_os = "windows")]
        anyhow::bail!(
            "captured no audio from the default microphone — run the Setup \
             microphone test, and check Windows Settings → Privacy & security \
             → Microphone (allow desktop apps)"
        );
        #[cfg(target_os = "macos")]
        anyhow::bail!(
            "captured no audio from the default microphone — run the Setup \
             microphone test, and grant Microphone access in System Settings \
             → Privacy & Security → Microphone"
        );
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        anyhow::bail!("captured no audio from the default microphone — check microphone permissions");
    }
    // Each recording gets its own file so History items can play back their
    // audio (SpeakType keeps audioFileURL per item; files die with the item).
    let dir = state.data_dir.join("recordings");
    std::fs::create_dir_all(&dir).context("create recordings dir")?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let wav_path = dir.join(format!("rec-{stamp}.wav"));
    text::write_wav_mono_16k(&wav_path, &samples, rec.sample_rate)?;
    Ok((wav_path, duration_secs))
}

// ---------------------------------------------------------------------------
// Microphone diagnostics + Windows settings deep-link
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AudioDeviceInfo {
    pub name: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub is_default: bool,
    pub is_selected: bool,
}

/// List all input devices. Unlike macOS (per-app mic prompt), Windows guards
/// the mic with a global privacy toggle — if this comes back empty, the
/// toggle (or a missing mic) is the cause.
#[tauri::command]
pub(crate) fn get_audio_devices(state: State<'_, Mutex<AppState>>) -> Result<Vec<AudioDeviceInfo>, String> {
    let preferred = state
        .lock()
        .ok()
        .and_then(|s| s.settings.audio_device.clone());
    let host = cpal::default_host();
    let default_name = host.default_input_device().as_ref().and_then(device_label);
    let mut out = Vec::new();
    let devices = host.input_devices().map_err(|e| e.to_string())?;
    for dev in devices {
        let name = device_label(&dev).unwrap_or_else(|| "(unnamed device)".to_owned());
        let (sample_rate, channels) = dev
            .default_input_config()
            .map(|c| (c.sample_rate(), c.channels()))
            .unwrap_or((0, 0));
        let is_default = Some(&name) == default_name.as_ref();
        let is_selected = match preferred.as_deref() {
            Some(p) if !p.is_empty() => device_matches(&dev, p),
            _ => is_default,
        };
        out.push(AudioDeviceInfo {
            is_default,
            is_selected,
            name,
            sample_rate,
            channels,
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct MicTest {
    pub duration_secs: f64,
    pub peak: f32,
    pub samples: usize,
    pub device: String,
    pub sample_rate: u32,
    pub channels: u16,
    /// Whether any audio callback fired at all. `false` with a present device
    /// points at exclusive-mode holds or a dead endpoint, not permission.
    pub callbacks: bool,
}

/// Record ~1.5 s and report the peak level without transcribing or saving
/// history — the fastest way to tell a dead mic from a broken pipeline.
#[tauri::command]
pub(crate) async fn test_microphone(state: State<'_, Mutex<AppState>>) -> Result<MicTest, String> {
    {
        let mut s = state.lock().unwrap();
        if s.recording {
            return Err("already recording — stop first".to_owned());
        }
        start_recording(&mut s).map_err(|e| e.to_string())?;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let (wav, duration_secs, callbacks) = {
        let mut s = state.lock().unwrap();
        let buffered = s
            .active
            .as_ref()
            .map(|a| a.samples.lock().unwrap().len())
            .unwrap_or(0);
        let (wav, duration_secs) = stop_and_save_wav(&mut s).map_err(|e| e.to_string())?;
        (wav, duration_secs, buffered > 0)
    };
    let preferred = state
        .lock()
        .ok()
        .and_then(|s| s.settings.audio_device.clone());
    let choice = resolve_capture_device(preferred.as_deref()).ok();
    let host = cpal::default_host();
    let (device, sample_rate, channels) = choice
        .as_ref()
        .and_then(|c| {
            let d = open_input_named(&c.name).ok()?;
            let cfg = d.default_input_config().ok()?;
            Some((c.name.clone(), cfg.sample_rate(), cfg.channels()))
        })
        .or_else(|| {
            host.default_input_device().and_then(|d| {
                let name = device_label(&d)?;
                let cfg = d.default_input_config().ok()?;
                Some((name, cfg.sample_rate(), cfg.channels()))
            })
        })
        .unwrap_or(("(none)".to_owned(), 0, 0));
    let samples = text::load_wav_mono_16k(&wav).map_err(|e| format!("{e:#}"))?;
    let peak = samples.iter().fold(0.0f32, |a, s| a.max(s.abs()));
    Ok(MicTest {
        duration_secs,
        peak,
        samples: samples.len(),
        device,
        sample_rate,
        channels,
        callbacks,
    })
}
