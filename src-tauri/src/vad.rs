//! Slice-A speech gate: Silero VAD + edge trim.
//!
//! Wispr parity note: VAD is a *filter*, not the UX. Hold-mode stop
//! semantics do not change — key up still means "transcribe this take."
//! This module only decides (a) whether a finished take contains speech
//! and (b) which sub-range to feed STT. History playback always uses the
//! original (untrimmed) recording.
//!
//! Runs on the finished 16 kHz mono buffer, never inside the cpal
//! callback — that thread stays cheap. If `silero_vad.onnx` is missing
//! (first launch, offline), every entry point falls back to the peak
//! gate in `devices::blank_audio` so dictation keeps working.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use crate::devices::{self, BlankAudio};

/// File name under `data_dir` (sibling of `models/`, `bin/`, …).
pub const MODEL_FILENAME: &str = "silero_vad.onnx";
/// k2-fsa export, 16 kHz only — matches our STT buffers. ~629 KB.
pub const MODEL_URL: &str =
    "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx";

/// Silero speech probability threshold. Kept at 0.5 so whispered speech
/// is not mistaken for silence.
pub const THRESHOLD: f32 = 0.5;
/// Minimum speech duration before a segment counts (mirrors
/// `devices::MIN_SPEECH_SECS`).
pub const MIN_SPEECH_SECS: f32 = 0.25;
/// Minimum silence duration the VAD itself enforces between segments.
pub const MIN_SILENCE_SECS: f32 = 0.30;
/// Padding kept around the first/last speech edge so the first and last
/// phoneme are not clipped.
pub const EDGE_PAD_SECS: f32 = 0.15;
/// Silero window at 16 kHz.
pub const WINDOW_SIZE: i32 = 512;

pub fn model_path(data_dir: &Path) -> PathBuf {
    data_dir.join(MODEL_FILENAME)
}

/// Best-effort fetch of the ~629 KB Silero model into `data_dir`.
///
/// Blocking: call from `spawn_blocking` (transcription) or a background
/// setup thread — never the async runtime or the capture thread.
/// Missing file is *not* an error: the gate falls back to the peak check.
pub fn ensure_model(data_dir: &Path) {
    let dest = model_path(data_dir);
    if dest.is_file() {
        return;
    }
    if let Some(parent) = dest.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let bytes = std::thread::Builder::new()
        .name("dictflow-vad-fetch".to_owned())
        .spawn(|| {
            reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .ok()?
                .get(MODEL_URL)
                .send()
                .ok()?
                .error_for_status()
                .ok()?
                .bytes()
                .ok()
        })
        .ok()
        .and_then(|h| h.join().ok())
        .flatten();
    let Some(bytes) = bytes else {
        log::warn!("VAD model fetch failed — using energy fallback");
        return;
    };
    if bytes.is_empty() {
        return;
    }
    let part = dest.with_extension("part");
    if std::fs::write(&part, &bytes).is_err() {
        let _ = std::fs::remove_file(&part);
        return;
    }
    if std::fs::rename(&part, &dest).is_err() {
        let _ = std::fs::remove_file(&part);
        return;
    }
    log::info!("VAD model ready: {}", dest.display());
}

fn create_detector(data_dir: &Path, buffer_secs: f32) -> Option<sherpa_onnx::VoiceActivityDetector> {
    let model = model_path(data_dir);
    if !model.is_file() {
        return None;
    }
    let mut cstrings_holder: Vec<std::ffi::CString> = Vec::new();
    let _ = &mut cstrings_holder; // config builder owns its copies below
    let config = sherpa_onnx::VadModelConfig {
        silero_vad: sherpa_onnx::SileroVadModelConfig {
            model: Some(model.display().to_string()),
            threshold: THRESHOLD,
            min_silence_duration: MIN_SILENCE_SECS,
            min_speech_duration: MIN_SPEECH_SECS,
            window_size: WINDOW_SIZE,
            // Long takes (up to the 19/20 min session cap) must not be
            // chopped: we trim first-start..last-end anyway, so a huge cap
            // just means "don't force-split speech".
            max_speech_duration: 10_000.0,
        },
        sample_rate: 16_000,
        num_threads: 1,
        provider: Some("cpu".to_owned()),
        debug: false,
        ..Default::default()
    };
    sherpa_onnx::VoiceActivityDetector::create(&config, buffer_secs.max(30.0))
}

/// Run Silero over a finished 16 kHz mono buffer.
///
/// Returns `None` when the model file is missing/unusable — the caller
/// must fall back to `devices::blank_audio`. `Some(vec)` with an empty
/// vec means "ran cleanly, no speech found".
pub fn detect_segments(samples_16k: &[f32], data_dir: &Path) -> Option<Vec<(usize, usize)>> {
    if samples_16k.is_empty() {
        return Some(Vec::new());
    }
    let buffer_secs = samples_16k.len() as f32 / 16_000.0 + 5.0;
    let vad = create_detector(data_dir, buffer_secs)?;
    vad.accept_waveform(samples_16k);
    vad.flush();
    let mut out = Vec::new();
    while !vad.is_empty() {
        let Some(seg) = vad.front() else {
            break;
        };
        let start = seg.start().max(0) as usize;
        let end = start.saturating_add(seg.n().max(0) as usize);
        // `front()` borrows the queued segment; copy the span, then pop.
        out.push((start, end));
        vad.pop();
    }
    Some(out)
}

/// First-speech-start minus pad … last-speech-end plus pad, clamped.
///
/// Interior pauses are untouched: the range spans the islands, it never
/// stitches them. `None` = no segments.
pub fn trim_range(
    segments: &[(usize, usize)],
    total_len: usize,
    sample_rate: u32,
    pad_secs: f32,
) -> Option<(usize, usize)> {
    if segments.is_empty() || total_len == 0 || sample_rate == 0 {
        return None;
    }
    let pad = (pad_secs.max(0.0) * sample_rate as f32) as usize;
    let first = segments.iter().map(|s| s.0).min()?;
    let last = segments.iter().map(|s| s.1).max()?;
    if first >= total_len || last == 0 || first >= last {
        return None;
    }
    Some((
        first.saturating_sub(pad),
        last.saturating_add(pad).min(total_len),
    ))
}

/// Gate + trim used by tests and by the runtime once segments exist.
///
/// Pure — no model access, so `cargo test` covers the contract without
/// downloading anything:
/// - empty / too-short buffers keep the existing `BlankAudio` copy,
/// - a loud buffer with *no* speech segments is `Silence` (this is the
///   HVAC/keyboard case the peak gate lets through),
/// - otherwise the buffer is trimmed at the edges only; interior pauses
///   stay in the returned clip.
pub fn gate_with_segments(
    samples_16k: &[f32],
    sample_rate: u32,
    segments: &[(usize, usize)],
) -> Result<Vec<f32>, BlankAudio> {
    if let Some(why) = devices::blank_audio(samples_16k, sample_rate) {
        if why != BlankAudio::Silence {
            return Err(why);
        }
        // A loud buffer still needs a speech verdict: without segments it
        // is non-speech noise, not dictation.
        if segments.is_empty() {
            return Err(BlankAudio::Silence);
        }
    } else if segments.is_empty() {
        return Err(BlankAudio::Silence);
    }
    let total = samples_16k.len();
    match trim_range(segments, total, sample_rate, EDGE_PAD_SECS) {
        Some((start, end)) if end > start => Ok(samples_16k[start..end].to_vec()),
        _ => Err(BlankAudio::Silence),
    }
}

/// Full runtime gate: empty/short keep the existing copy, then Silero
/// decides speech vs. noise, then edges are trimmed for STT only.
///
/// Falls back to `devices::blank_audio` when the model file is missing
/// so dictation works on first launch before the fetch lands.
pub fn gate_and_trim(
    samples_16k: &[f32],
    sample_rate: u32,
    data_dir: &Path,
) -> Result<Vec<f32>, BlankAudio> {
    if let Some(why) = devices::blank_audio(samples_16k, sample_rate) {
        if why != BlankAudio::Silence {
            return Err(why);
        }
        // Peak-quiet: rejected with or without the model.
        match detect_segments(samples_16k, data_dir) {
            None => Err(BlankAudio::Silence),
            Some(segs) => gate_with_segments(samples_16k, sample_rate, &segs),
        }
    } else {
        match detect_segments(samples_16k, data_dir) {
            None => Ok(samples_16k.to_vec()),
            Some(segs) => gate_with_segments(samples_16k, sample_rate, &segs),
        }
    }
}

/// Encode 16 kHz mono f32 as 16-bit PCM WAV bytes for the audio API.
pub fn encode_wav_bytes_16k(samples: &[f32]) -> anyhow::Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut buf = Cursor::new(Vec::new());
    {
        let mut writer = hound::WavWriter::new(&mut buf, spec)?;
        for s in samples {
            writer.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
        }
        writer.finalize()?;
    }
    Ok(buf.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 16_000;

    /// 2 s buffer: 0.5 s silence, 1 s loud "speech", 0.5 s silence.
    fn padded_speech() -> Vec<f32> {
        let mut v = vec![0.0; 2 * SR as usize];
        for s in &mut v[8000..24_000] {
            *s = 0.4;
        }
        v
    }

    #[test]
    fn empty_and_too_short_keep_existing_copy() {
        assert_eq!(
            gate_with_segments(&[], SR, &[(0, 16_000)]),
            Err(BlankAudio::Empty)
        );
        assert_eq!(
            gate_with_segments(&[0.5; 800], SR, &[(0, 800)]),
            Err(BlankAudio::TooShort)
        );
    }

    #[test]
    fn loud_non_speech_is_rejected() {
        // The HVAC/keyboard case: passes the peak gate, but Silero found no
        // speech segments → same "only silence" copy as before.
        let loud_noise = vec![0.3; SR as usize];
        assert!(devices::blank_audio(&loud_noise, SR).is_none());
        assert_eq!(gate_with_segments(&loud_noise, SR, &[]), Err(BlankAudio::Silence));
    }

    #[test]
    fn edges_trimmed_with_pad() {
        let samples = padded_speech();
        let out = gate_with_segments(&samples, SR, &[(8000, 24_000)]).unwrap();
        let pad = (EDGE_PAD_SECS * SR as f32) as usize;
        assert_eq!(out.len(), 16_000 + 2 * pad);
        // Pad reaches into the surrounding silence.
        assert_eq!(out[0], 0.0);
        assert_eq!(out[pad], 0.4);
        assert_eq!(out[out.len() - 1], 0.0);
    }

    #[test]
    fn interior_silence_is_kept() {
        // Speech — 0.5 s pause — speech. Trim spans the islands; the pause
        // between them must survive for polish ("4 pm, actually 3 pm").
        let mut samples = vec![0.0; 4 * SR as usize];
        for s in &mut samples[8000..16_000] {
            *s = 0.4;
        }
        for s in &mut samples[24_000..32_000] {
            *s = 0.4;
        }
        let segs = vec![(8000, 16_000), (24_000, 32_000)];
        let out = gate_with_segments(&samples, SR, &segs).unwrap();
        let pad = (EDGE_PAD_SECS * SR as f32) as usize;
        assert_eq!(out.len(), (32_000 + pad) - (8000 - pad));
        let gap_start = 16_000 - (8000 - pad);
        let gap_end = 24_000 - (8000 - pad);
        assert!(out[gap_start..gap_end].iter().all(|&s| s == 0.0));
        assert_eq!(out[gap_end], 0.4);
    }

    #[test]
    fn trim_range_clamps_and_rejects_degenerate() {
        assert_eq!(trim_range(&[], 100, SR, EDGE_PAD_SECS), None);
        // Pad clamped at both ends.
        assert_eq!(
            trim_range(&[(10, 90)], 100, 100, 1.0),
            Some((0, 100))
        );
        assert_eq!(trim_range(&[(90, 10)], 100, SR, EDGE_PAD_SECS), None);
    }

    #[test]
    fn missing_model_falls_back_to_peak_gate() {
        let dir = std::env::temp_dir().join("dictflow-vad-test-missing-model");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Silent → rejected, loud → let through (old behaviour) until the
        // model lands.
        assert_eq!(
            gate_and_trim(&vec![0.0; SR as usize], SR, &dir),
            Err(BlankAudio::Silence)
        );
        let loud = vec![0.3; SR as usize];
        assert_eq!(gate_and_trim(&loud, SR, &dir), Ok(loud));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wav_bytes_round_trip() {
        let samples = vec![0.0, 0.5, -0.5, 1.0];
        let bytes = encode_wav_bytes_16k(&samples).unwrap();
        assert!(bytes.len() > 44); // header + 4 frames
    }
}
