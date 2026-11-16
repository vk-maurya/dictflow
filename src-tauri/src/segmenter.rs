//! Pause-based segmentation of a live recording.
//!
//! A long dictation is cut into segments at natural pauses so each segment can
//! be transcribed in the background while the user keeps talking. The pause
//! detector (VAD) decides *where* a cut goes; time only supplies guardrails:
//!
//! - no cut before `min_secs` of audio since the previous cut (no fragments),
//! - a forced cut at `hard_secs` when no pause showed up (non-stop talking, or
//!   a room so noisy that the detector never sees silence).
//!
//! Segments are `[0,c1) [c1,c2) ... [ck,end)` over one shared sample buffer, so
//! every sample belongs to exactly one segment: nothing is overlapped or
//! dropped. Everything here is pure (no Tauri, no threads) except the optional
//! Silero adapter, which wraps the sherpa-onnx VAD already linked for Parakeet.

use std::ops::Range;
use std::path::{Path, PathBuf};

/// Sample rate the neural VAD expects.
pub const VAD_RATE: u32 = 16_000;

/// Analysis frame length. Cut decisions are made once per completed frame.
const FRAME_MS: usize = 20;
/// A pause cut keeps this much silence on each side of the cut point.
const CUT_MARGIN_MS: f64 = 80.0;
/// Window used to find the quietest spot for a forced cut.
const FORCED_WINDOW_MS: usize = 100;
/// A forced cut only looks this far back from the hard cap.
const FORCED_LOOKBACK_SECS: f64 = 8.0;

/// Energy detector bounds (RMS of f32 samples in -1..1).
const ENERGY_ABS_MIN: f32 = 0.005;
const ENERGY_ABS_MAX: f32 = 0.03;
const ENERGY_FLOOR_MULT: f32 = 3.0;
const ENERGY_LEVEL_FRACTION: f32 = 0.25;
const ENERGY_RING_FRAMES: usize = 250;
const ENERGY_STATS_EVERY: usize = 5;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Segment length guardrails and the pause schedule.
///
/// The pause needed to cut shrinks linearly from `pause_at_min_ms` (segment is
/// `min_secs` long) to `pause_at_hard_ms` (segment is `hard_secs` long): a
/// longer-running chunk accepts a shorter pause rather than wait for a long one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SegmentCfg {
    pub min_secs: f64,
    pub hard_secs: f64,
    pub pause_at_min_ms: f64,
    pub pause_at_hard_ms: f64,
}

impl SegmentCfg {
    /// In-process Parakeet: a decode call is cheap, so shorter segments mean a
    /// shorter wait when the user stops.
    pub const PARAKEET: SegmentCfg = SegmentCfg {
        min_secs: 10.0,
        hard_secs: 30.0,
        pause_at_min_ms: 700.0,
        pause_at_hard_ms: 250.0,
    };

    /// Whisper sidecar and cloud audio API: every call has fixed overhead (a
    /// whisper-cli process reloading the model, or an HTTP round trip), so
    /// fewer, longer segments are better.
    pub const SLOW_ENGINE: SegmentCfg = SegmentCfg {
        min_secs: 20.0,
        hard_secs: 40.0,
        pause_at_min_ms: 700.0,
        pause_at_hard_ms: 250.0,
    };

    /// Silence needed (seconds) to cut a segment that is `seg_secs` long.
    pub fn required_pause_secs(&self, seg_secs: f64) -> f64 {
        let span = (self.hard_secs - self.min_secs).max(1e-9);
        let t = ((seg_secs - self.min_secs) / span).clamp(0.0, 1.0);
        (self.pause_at_min_ms + (self.pause_at_hard_ms - self.pause_at_min_ms) * t) / 1000.0
    }
}

/// How a cut was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CutKind {
    /// Inside a real pause.
    Pause,
    /// No pause arrived before the hard cap; cut at the quietest spot.
    Forced,
}

/// One cut: `index` is an absolute sample index in the device-rate stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cut {
    pub index: usize,
    pub kind: CutKind,
}

/// Segment ranges implied by `cuts` over a buffer of `len` samples.
pub fn segments_from_cuts(len: usize, cuts: &[Cut]) -> Vec<Range<usize>> {
    let mut out = Vec::with_capacity(cuts.len() + 1);
    let mut start = 0;
    for c in cuts {
        out.push(start..c.index);
        start = c.index;
    }
    out.push(start..len);
    out
}

// ---------------------------------------------------------------------------
// Pause sources
// ---------------------------------------------------------------------------

/// Tells the segmenter where silence is. Fed one frame at a time so results do
/// not depend on how the caller chunks the audio.
pub trait PauseSource: Send {
    /// Consume one frame. `start` is the absolute device-rate index of the
    /// frame's first sample; `rms` is its RMS level.
    fn push_frame(&mut self, start: usize, frame: &[f32], rms: f32);

    /// If the audio fed so far ends in confirmed silence, the absolute index
    /// where that silence run began. `None` while speech is (or may be) active.
    fn silence_start(&self) -> Option<usize>;
}

/// Adaptive-threshold RMS detector. No model file needed.
///
/// The threshold sits above the recent noise floor (10th percentile of the
/// last five seconds) but never above a fraction of the recent speech level
/// (90th percentile), so quiet speakers still count as speech. Two consecutive
/// speech frames are needed to break a silence run, so a click is ignored.
pub struct EnergyPauseSource {
    ring: Vec<f32>,
    ring_pos: usize,
    filled: usize,
    since_stats: usize,
    floor: f32,
    level: f32,
    speech_streak: u32,
    run_start: Option<usize>,
    scratch: Vec<f32>,
}

impl Default for EnergyPauseSource {
    fn default() -> Self {
        Self::new()
    }
}

impl EnergyPauseSource {
    pub fn new() -> Self {
        EnergyPauseSource {
            ring: vec![0.0; ENERGY_RING_FRAMES],
            ring_pos: 0,
            filled: 0,
            since_stats: 0,
            floor: 0.0,
            level: 0.0,
            speech_streak: 0,
            run_start: None,
            scratch: Vec::with_capacity(ENERGY_RING_FRAMES),
        }
    }

    fn refresh_stats(&mut self) {
        self.scratch.clear();
        self.scratch.extend_from_slice(&self.ring[..self.filled]);
        self.scratch
            .sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = self.scratch.len();
        if n == 0 {
            return;
        }
        self.floor = self.scratch[(n / 10).min(n - 1)];
        self.level = self.scratch[(n * 9 / 10).min(n - 1)];
    }

    fn threshold(&self) -> f32 {
        (self.floor * ENERGY_FLOOR_MULT)
            .min(self.level * ENERGY_LEVEL_FRACTION)
            .clamp(ENERGY_ABS_MIN, ENERGY_ABS_MAX)
    }
}

impl PauseSource for EnergyPauseSource {
    fn push_frame(&mut self, start: usize, _frame: &[f32], rms: f32) {
        self.ring[self.ring_pos] = rms;
        self.ring_pos = (self.ring_pos + 1) % self.ring.len();
        self.filled = (self.filled + 1).min(self.ring.len());
        // Refresh quickly while warming up, then every few frames.
        if self.filled <= ENERGY_STATS_EVERY || self.since_stats + 1 >= ENERGY_STATS_EVERY {
            self.refresh_stats();
            self.since_stats = 0;
        } else {
            self.since_stats += 1;
        }

        if rms > self.threshold() {
            self.speech_streak += 1;
            if self.speech_streak >= 2 {
                self.run_start = None;
            }
        } else {
            self.speech_streak = 0;
            if self.run_start.is_none() {
                self.run_start = Some(start);
            }
        }
    }

    fn silence_start(&self) -> Option<usize> {
        self.run_start
    }
}

/// Streaming linear resampler (device rate to 16 kHz for the VAD). Keeps its
/// phase across calls so chunk boundaries do not matter.
pub struct StreamResampler {
    ratio: f64,
    buf: Vec<f32>,
    base: f64,
    next_pos: f64,
}

impl StreamResampler {
    pub fn new(from_rate: u32, to_rate: u32) -> Self {
        StreamResampler {
            ratio: from_rate as f64 / to_rate.max(1) as f64,
            buf: Vec::new(),
            base: 0.0,
            next_pos: 0.0,
        }
    }

    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        self.buf.extend_from_slice(input);
        let mut out = Vec::with_capacity((input.len() as f64 / self.ratio) as usize + 2);
        loop {
            let rel = self.next_pos - self.base;
            let lo = rel.floor() as usize;
            if lo + 1 >= self.buf.len() {
                break;
            }
            let frac = (rel - lo as f64) as f32;
            out.push(self.buf[lo] * (1.0 - frac) + self.buf[lo + 1] * frac);
            self.next_pos += self.ratio;
        }
        let rel = (self.next_pos - self.base).floor() as usize;
        let keep_from = rel.min(self.buf.len().saturating_sub(1));
        self.buf.drain(..keep_from);
        self.base += keep_from as f64;
        out
    }
}

/// Map a 16 kHz stream index back to the device-rate stream.
pub fn device_index_from_16k(idx16: usize, device_rate: u32) -> usize {
    (idx16 as u64 * device_rate as u64 / VAD_RATE as u64) as usize
}

/// Silero VAD through the sherpa-onnx runtime already linked for Parakeet.
///
/// The VAD emits a speech segment once the speech has ended and a minimum of
/// silence has passed; the gap after the last emitted segment is a confirmed
/// pause. The VAD never splits on its own (its max speech duration is far above
/// the hard cap), so every cut decision stays in `PauseSegmenter`.
pub struct SileroPauseSource {
    vad: sherpa_onnx::VoiceActivityDetector,
    device_rate: u32,
    resampler: StreamResampler,
    last_speech_end: usize,
}

/// The bundled model (k2-fsa export of Silero VAD, 16 kHz). The runtime throws
/// a C++ exception on a bad model file and that aborts the whole process, so
/// only a file matching this size and hash is ever handed to it.
pub const SILERO_MODEL_BYTES: u64 = 643_854;
pub const SILERO_MODEL_SHA256: &str =
    "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6";

/// True when `path` is exactly the bundled Silero model.
pub fn verify_silero_model(path: &Path) -> bool {
    use sha2::{Digest, Sha256};
    use std::io::Read as _;
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if meta.len() != SILERO_MODEL_BYTES {
        return false;
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut hash = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hash.update(&buf[..n]),
            Err(_) => return false,
        }
    }
    format!("{:x}", hash.finalize()) == SILERO_MODEL_SHA256
}

impl SileroPauseSource {
    /// `None` when the model file is missing, is not the verified bundled
    /// model, or the runtime rejects it.
    pub fn open(model: &Path, device_rate: u32) -> Option<Self> {
        if !verify_silero_model(model) {
            return None;
        }
        let mut cfg = sherpa_onnx::VadModelConfig::default();
        cfg.silero_vad.model = Some(model.display().to_string());
        cfg.silero_vad.threshold = 0.5;
        cfg.silero_vad.min_silence_duration = 0.25;
        cfg.silero_vad.min_speech_duration = 0.15;
        cfg.silero_vad.window_size = 512;
        cfg.silero_vad.max_speech_duration = 100.0;
        cfg.sample_rate = VAD_RATE as i32;
        cfg.num_threads = 1;
        cfg.provider = Some("cpu".to_owned());
        let vad = sherpa_onnx::VoiceActivityDetector::create(&cfg, 120.0)?;
        Some(SileroPauseSource {
            vad,
            device_rate,
            resampler: StreamResampler::new(device_rate, VAD_RATE),
            last_speech_end: 0,
        })
    }
}

impl PauseSource for SileroPauseSource {
    fn push_frame(&mut self, _start: usize, frame: &[f32], _rms: f32) {
        let mono16 = self.resampler.process(frame);
        if !mono16.is_empty() {
            self.vad.accept_waveform(&mono16);
        }
        while let Some(seg) = self.vad.front() {
            let end16 = (seg.start().max(0) as usize).saturating_add(seg.n().max(0) as usize);
            let end = device_index_from_16k(end16, self.device_rate);
            self.last_speech_end = self.last_speech_end.max(end);
            drop(seg);
            self.vad.pop();
        }
    }

    fn silence_start(&self) -> Option<usize> {
        if self.vad.detected() {
            None
        } else {
            Some(self.last_speech_end)
        }
    }
}

/// First candidate that is the verified bundled model.
pub fn find_vad_model(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|p| verify_silero_model(p)).cloned()
}

/// Pick the best pause source: Silero when its model loads, else the energy
/// detector. The label is for logs.
pub fn make_source(device_rate: u32, model: Option<&Path>) -> (Box<dyn PauseSource>, &'static str) {
    if let Some(path) = model {
        if let Some(silero) = SileroPauseSource::open(path, device_rate) {
            return (Box::new(silero), "silero");
        }
        log::warn!(
            "silero VAD model unusable at {}; using the energy detector",
            path.display()
        );
    }
    (Box::new(EnergyPauseSource::new()), "energy")
}

// ---------------------------------------------------------------------------
// Segmenter
// ---------------------------------------------------------------------------

/// Incremental cut finder over a device-rate sample stream.
///
/// Audio is consumed in fixed 20 ms frames and decisions are made once per
/// completed frame, so the cuts are identical however the caller chunks `feed`.
pub struct PauseSegmenter {
    cfg: SegmentCfg,
    rate: u32,
    frame_len: usize,
    source: Box<dyn PauseSource>,
    pending: Vec<f32>,
    /// Samples consumed in whole frames.
    fed: usize,
    /// Start of the current segment (the last cut, or 0).
    seg_start: usize,
    /// Frame RMS from global frame `rms_base` onward.
    rms: Vec<f32>,
    rms_base: usize,
}

impl PauseSegmenter {
    pub fn new(rate: u32, cfg: SegmentCfg, source: Box<dyn PauseSource>) -> Self {
        let frame_len = ((rate as usize * FRAME_MS) / 1000).max(1);
        PauseSegmenter {
            cfg,
            rate,
            frame_len,
            source,
            pending: Vec::with_capacity(frame_len),
            fed: 0,
            seg_start: 0,
            rms: Vec::new(),
            rms_base: 0,
        }
    }

    /// Start of the segment currently being built.
    #[cfg(test)]
    pub fn seg_start(&self) -> usize {
        self.seg_start
    }

    /// Samples consumed so far (whole frames only).
    #[cfg(test)]
    pub fn fed(&self) -> usize {
        self.fed
    }

    /// Feed new audio; returns any cuts found, as absolute sample indices.
    pub fn feed(&mut self, new: &[f32]) -> Vec<Cut> {
        let mut cuts = Vec::new();
        let frame_len = self.frame_len;
        let mut input = new;
        if !self.pending.is_empty() {
            let take = (frame_len - self.pending.len()).min(input.len());
            self.pending.extend_from_slice(&input[..take]);
            input = &input[take..];
            if self.pending.len() < frame_len {
                return cuts;
            }
            let frame = std::mem::take(&mut self.pending);
            self.process_frame(&frame, &mut cuts);
        }
        let mut chunks = input.chunks_exact(frame_len);
        for frame in &mut chunks {
            self.process_frame(frame, &mut cuts);
        }
        self.pending.extend_from_slice(chunks.remainder());
        cuts
    }

    fn secs_to_samples(&self, secs: f64) -> usize {
        (secs * self.rate as f64).round().max(0.0) as usize
    }

    fn process_frame(&mut self, frame: &[f32], cuts: &mut Vec<Cut>) {
        let rms = frame_rms(frame);
        self.rms.push(rms);
        self.source.push_frame(self.fed, frame, rms);
        self.fed += self.frame_len;
        if let Some(cut) = self.evaluate() {
            self.seg_start = cut.index;
            let first_keep = cut.index / self.frame_len;
            let drop_n = first_keep.saturating_sub(self.rms_base).min(self.rms.len());
            self.rms.drain(..drop_n);
            self.rms_base += drop_n;
            cuts.push(cut);
        }
    }

    fn frame_rms_at(&self, frame_idx: usize) -> f32 {
        self.rms[frame_idx - self.rms_base]
    }

    fn evaluate(&self) -> Option<Cut> {
        let seg_len = self.fed - self.seg_start;
        let min = self.secs_to_samples(self.cfg.min_secs);
        if seg_len < min {
            return None;
        }
        let seg_secs = seg_len as f64 / self.rate as f64;

        if let Some(silence_from) = self.source.silence_start() {
            let run_start = silence_from.max(self.seg_start);
            let gap = self.fed.saturating_sub(run_start);
            let need = self.secs_to_samples(self.cfg.required_pause_secs(seg_secs));
            if gap >= need {
                let margin = self.secs_to_samples(CUT_MARGIN_MS / 1000.0);
                let lo = (run_start + margin).max(self.seg_start + min);
                let hi = self.fed.saturating_sub(margin);
                if let Some(index) = self.quietest_frame_center(lo, hi) {
                    return Some(Cut {
                        index,
                        kind: CutKind::Pause,
                    });
                }
            }
        }

        let hard = self.secs_to_samples(self.cfg.hard_secs);
        if seg_len >= hard {
            return self.forced_cut(min).map(|index| Cut {
                index,
                kind: CutKind::Forced,
            });
        }
        None
    }

    /// Centre of the quietest whole frame inside `[lo, hi)`. Ties go to the
    /// frame nearest the middle of the range.
    fn quietest_frame_center(&self, lo: usize, hi: usize) -> Option<usize> {
        let first = lo.div_ceil(self.frame_len);
        let last = hi / self.frame_len; // exclusive
        if first >= last || first < self.rms_base {
            return None;
        }
        let mid = (first + last) / 2;
        let mut best: Option<(f32, usize, usize)> = None;
        for k in first..last {
            let level = self.frame_rms_at(k);
            let dist = k.abs_diff(mid);
            let better = match best {
                None => true,
                Some((bl, bd, _)) => level < bl || (level == bl && dist < bd),
            };
            if better {
                best = Some((level, dist, k));
            }
        }
        best.map(|(_, _, k)| k * self.frame_len + self.frame_len / 2)
    }

    /// Centre of the quietest 100 ms window in the last 8 s of the segment.
    /// Ties go to the latest window, which keeps segments long.
    fn forced_cut(&self, min: usize) -> Option<usize> {
        let window = (FORCED_WINDOW_MS / FRAME_MS).max(1);
        let lookback = self.secs_to_samples(FORCED_LOOKBACK_SECS);
        let lo = self.fed.saturating_sub(lookback).max(self.seg_start + min);
        let first = lo.div_ceil(self.frame_len).max(self.rms_base);
        let end_frame = self.fed / self.frame_len; // exclusive
        if first + window > end_frame {
            return None;
        }
        let mut best: Option<(f32, usize)> = None;
        for k in first..=(end_frame - window) {
            let sum: f32 = (k..k + window).map(|i| self.frame_rms_at(i)).sum();
            if best.is_none_or(|(b, _)| sum <= b) {
                best = Some((sum, k));
            }
        }
        best.map(|(_, k)| k * self.frame_len + (window * self.frame_len) / 2)
    }
}

fn frame_rms(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    let sum: f32 = frame.iter().map(|s| s * s).sum();
    (sum / frame.len() as f32).sqrt()
}

/// Cuts for a whole buffer in one pass with the given pause source. Same code
/// path as live segmentation (used for imported files and tests).
pub fn plan_cuts_with(
    samples: &[f32],
    rate: u32,
    cfg: SegmentCfg,
    source: Box<dyn PauseSource>,
) -> Vec<Cut> {
    PauseSegmenter::new(rate, cfg, source).feed(samples)
}

/// `plan_cuts_with` using the energy detector.
#[cfg(test)]
pub fn plan_cuts(samples: &[f32], rate: u32, cfg: SegmentCfg) -> Vec<Cut> {
    plan_cuts_with(samples, rate, cfg, Box::new(EnergyPauseSource::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const R16: u32 = 16_000;

    fn tone(secs: f64, rate: u32, amp: f32) -> Vec<f32> {
        let n = (secs * rate as f64) as usize;
        (0..n)
            .map(|i| amp * (2.0 * std::f32::consts::PI * 220.0 * i as f32 / rate as f32).sin())
            .collect()
    }

    fn quiet(secs: f64, rate: u32) -> Vec<f32> {
        vec![0.0; (secs * rate as f64) as usize]
    }

    /// Deterministic noise with the given RMS.
    fn noise(secs: f64, rate: u32, rms: f32, seed: &mut u64) -> Vec<f32> {
        let n = (secs * rate as f64) as usize;
        // Uniform in [-a, a] has rms a / sqrt(3).
        let a = rms * 3.0f32.sqrt();
        (0..n).map(|_| (lcg(seed) * 2.0 - 1.0) * a).collect()
    }

    fn lcg(state: &mut u64) -> f32 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((*state >> 33) as f32) / ((1u64 << 31) as f32)
    }

    fn concat(parts: &[Vec<f32>]) -> Vec<f32> {
        parts.iter().flat_map(|p| p.iter().copied()).collect()
    }

    fn frame_samples(rate: u32) -> usize {
        rate as usize * FRAME_MS / 1000
    }

    fn assert_partition(samples: &[f32], cuts: &[Cut]) {
        let mut prev = 0usize;
        for c in cuts {
            assert!(c.index > prev, "cuts must strictly increase: {cuts:?}");
            assert!(c.index < samples.len(), "cut past end: {cuts:?}");
            prev = c.index;
        }
        let segs = segments_from_cuts(samples.len(), cuts);
        let mut joined = Vec::with_capacity(samples.len());
        for r in &segs {
            joined.extend_from_slice(&samples[r.clone()]);
        }
        assert_eq!(joined.len(), samples.len());
        assert!(joined == samples, "segments must reassemble the buffer");
    }

    /// Fake pause source with exact scripted silence runs `[start, end)`.
    struct Scripted {
        runs: Vec<(usize, usize)>,
        fed: usize,
    }

    impl Scripted {
        fn secs(runs: &[(f64, f64)], rate: u32) -> Box<dyn PauseSource> {
            Box::new(Scripted {
                runs: runs
                    .iter()
                    .map(|(a, b)| ((a * rate as f64) as usize, (b * rate as f64) as usize))
                    .collect(),
                fed: 0,
            })
        }
    }

    impl PauseSource for Scripted {
        fn push_frame(&mut self, start: usize, frame: &[f32], _rms: f32) {
            self.fed = start + frame.len();
        }
        fn silence_start(&self) -> Option<usize> {
            self.runs
                .iter()
                .find(|(s, e)| *s < self.fed && self.fed <= *e)
                .map(|(s, _)| *s)
        }
    }

    // ---- configuration -----------------------------------------------------

    #[test]
    fn profiles_have_expected_numbers() {
        assert_eq!(SegmentCfg::PARAKEET.min_secs, 10.0);
        assert_eq!(SegmentCfg::PARAKEET.hard_secs, 30.0);
        assert_eq!(SegmentCfg::SLOW_ENGINE.min_secs, 20.0);
        assert_eq!(SegmentCfg::SLOW_ENGINE.hard_secs, 40.0);
        assert_eq!(SegmentCfg::PARAKEET.pause_at_min_ms, 700.0);
        assert_eq!(SegmentCfg::PARAKEET.pause_at_hard_ms, 250.0);
    }

    #[test]
    fn required_pause_shrinks_linearly_and_clamps() {
        let c = SegmentCfg::PARAKEET;
        assert!((c.required_pause_secs(10.0) - 0.7).abs() < 1e-9);
        assert!((c.required_pause_secs(30.0) - 0.25).abs() < 1e-9);
        assert!((c.required_pause_secs(20.0) - 0.475).abs() < 1e-9);
        assert!((c.required_pause_secs(5.0) - 0.7).abs() < 1e-9);
        assert!((c.required_pause_secs(99.0) - 0.25).abs() < 1e-9);
    }

    // ---- resampler and index mapping ---------------------------------------

    #[test]
    fn device_index_mapping_is_exact() {
        assert_eq!(device_index_from_16k(16_000, 48_000), 48_000);
        assert_eq!(device_index_from_16k(16_000, 44_100), 44_100);
        assert_eq!(device_index_from_16k(8_000, 44_100), 22_050);
        assert_eq!(device_index_from_16k(0, 48_000), 0);
        assert_eq!(device_index_from_16k(512, 16_000), 512);
    }

    #[test]
    fn stream_resampler_matches_across_chunkings() {
        let mut seed = 7;
        let input = noise(1.0, 48_000, 0.2, &mut seed);
        let mut whole = StreamResampler::new(48_000, 16_000);
        let a = whole.process(&input);
        let mut chunked = StreamResampler::new(48_000, 16_000);
        let mut b = Vec::new();
        for ch in input.chunks(960) {
            b.extend(chunked.process(ch));
        }
        assert!((a.len() as i64 - 16_000).abs() <= 2, "len {}", a.len());
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(&b) {
            assert!((x - y).abs() < 1e-6);
        }
    }

    #[test]
    fn stream_resampler_is_identity_at_equal_rates() {
        let mut r = StreamResampler::new(16_000, 16_000);
        let input: Vec<f32> = (0..100).map(|i| i as f32).collect();
        let out = r.process(&input);
        // Holds back the final sample until the next chunk arrives.
        assert!(out.len() >= 98);
        for (i, v) in out.iter().enumerate() {
            assert!((v - i as f32).abs() < 1e-6);
        }
    }

    // ---- energy source ------------------------------------------------------

    fn run_energy(signal: &[f32], rate: u32) -> EnergyPauseSource {
        let mut src = EnergyPauseSource::new();
        let f = frame_samples(rate);
        for (k, fr) in signal.chunks_exact(f).enumerate() {
            src.push_frame(k * f, fr, frame_rms(fr));
        }
        src
    }

    #[test]
    fn energy_detects_silence_after_speech() {
        let sig = concat(&[tone(2.0, R16, 0.3), quiet(1.0, R16)]);
        let src = run_energy(&sig, R16);
        let s = src.silence_start().expect("silence run");
        assert!((s as i64 - 2 * R16 as i64).abs() <= frame_samples(R16) as i64);
    }

    #[test]
    fn energy_reports_no_silence_during_speech() {
        let sig = tone(3.0, R16, 0.3);
        assert!(run_energy(&sig, R16).silence_start().is_none());
    }

    #[test]
    fn energy_handles_digital_silence_without_nan() {
        let sig = quiet(5.0, R16);
        let src = run_energy(&sig, R16);
        assert_eq!(src.silence_start(), Some(0));
        assert!(src.threshold().is_finite());
    }

    #[test]
    fn energy_quiet_speaker_counts_as_speech() {
        // Peak 0.02 (rms about 0.014) over a 0.001 noise floor.
        let mut seed = 3;
        let mut parts = Vec::new();
        for _ in 0..4 {
            parts.push(tone(3.0, R16, 0.02));
            parts.push(noise(0.5, R16, 0.001, &mut seed));
        }
        parts.push(tone(8.0, R16, 0.02));
        let sig = concat(&parts);
        assert!(run_energy(&sig, R16).silence_start().is_none());
    }

    #[test]
    fn energy_low_noise_between_words_is_silence() {
        let mut seed = 5;
        let sig = concat(&[tone(3.0, R16, 0.3), noise(1.0, R16, 0.01, &mut seed)]);
        assert!(run_energy(&sig, R16).silence_start().is_some());
    }

    #[test]
    fn energy_single_click_does_not_break_a_silence_run() {
        let mut sig = concat(&[tone(2.0, R16, 0.3), quiet(1.0, R16)]);
        // One loud frame in the middle of the silence.
        let f = frame_samples(R16);
        let at = 2 * R16 as usize + 10 * f;
        for s in &mut sig[at..at + f] {
            *s = 0.5;
        }
        let src = run_energy(&sig, R16);
        let s = src.silence_start().expect("run survives a one-frame click");
        assert!((s as i64 - 2 * R16 as i64).abs() <= f as i64);
    }

    // ---- segmenter: cut rules -----------------------------------------------

    fn cuts_for(signal: &[f32], rate: u32, cfg: SegmentCfg) -> Vec<Cut> {
        plan_cuts(signal, rate, cfg)
    }

    fn assert_silent_around(signal: &[f32], index: usize, rate: u32, ms: usize) {
        let w = rate as usize * ms / 1000;
        let lo = index.saturating_sub(w);
        let hi = (index + w).min(signal.len());
        assert!(
            signal[lo..hi].iter().all(|s| s.abs() < 1e-6),
            "audio around the cut at {index} must be silent"
        );
    }

    #[test]
    fn no_cut_before_min_even_with_pauses() {
        let mut parts = Vec::new();
        for _ in 0..4 {
            parts.push(tone(1.8, R16, 0.3));
            parts.push(quiet(0.7, R16)); // 4 x 2.5 s = 10 s total, last pause ends at the min
        }
        let sig = concat(&parts);
        assert!(sig.len() as f64 / R16 as f64 <= 10.0 + 1e-6);
        assert!(cuts_for(&sig, R16, SegmentCfg::PARAKEET).is_empty());
    }

    #[test]
    fn pause_at_28s_cuts_once_inside_the_pause() {
        let sig = concat(&[tone(28.0, R16, 0.3), quiet(1.0, R16), tone(11.0, R16, 0.3)]);
        let cuts = cuts_for(&sig, R16, SegmentCfg::PARAKEET);
        assert_eq!(cuts.len(), 1, "{cuts:?}");
        assert_eq!(cuts[0].kind, CutKind::Pause);
        let at = cuts[0].index;
        assert!(at >= 28 * R16 as usize + frame_samples(R16));
        assert!(at <= 29 * R16 as usize);
        assert_silent_around(&sig, at, R16, 50);
        assert_partition(&sig, &cuts);
    }

    #[test]
    fn pause_cut_works_at_common_device_rates() {
        for rate in [16_000u32, 44_100, 48_000] {
            let sig = concat(&[
                tone(28.0, rate, 0.3),
                quiet(1.0, rate),
                tone(11.0, rate, 0.3),
            ]);
            let cuts = cuts_for(&sig, rate, SegmentCfg::PARAKEET);
            assert_eq!(cuts.len(), 1, "rate {rate}: {cuts:?}");
            assert_eq!(cuts[0].kind, CutKind::Pause);
            assert_silent_around(&sig, cuts[0].index, rate, 50);
            assert_partition(&sig, &cuts);
        }
    }

    #[test]
    fn short_gap_early_is_ignored_but_same_gap_late_is_accepted() {
        // 300 ms gap at 11 s: needs about 677 ms there, so no cut.
        let early = concat(&[tone(11.0, R16, 0.3), quiet(0.3, R16), tone(14.0, R16, 0.3)]);
        assert!(cuts_for(&early, R16, SegmentCfg::PARAKEET).is_empty());

        // The same gap at 29 s needs about 266 ms: accepted.
        let late = concat(&[tone(29.0, R16, 0.3), quiet(0.3, R16), tone(6.0, R16, 0.3)]);
        let cuts = cuts_for(&late, R16, SegmentCfg::PARAKEET);
        assert_eq!(cuts.len(), 1, "{cuts:?}");
        assert_eq!(cuts[0].kind, CutKind::Pause);
        assert_silent_around(&late, cuts[0].index, R16, 50);
    }

    #[test]
    fn sentence_pause_just_after_min_is_accepted() {
        let sig = concat(&[tone(10.5, R16, 0.3), quiet(0.7, R16), tone(5.0, R16, 0.3)]);
        let cuts = cuts_for(&sig, R16, SegmentCfg::PARAKEET);
        assert_eq!(cuts.len(), 1, "{cuts:?}");
        assert_eq!(cuts[0].kind, CutKind::Pause);
        assert!(cuts[0].index >= 10 * R16 as usize);
        assert_silent_around(&sig, cuts[0].index, R16, 50);
    }

    #[test]
    fn forced_cut_lands_on_the_quiet_dip() {
        // Continuous tone with a 150 ms near-silent dip at 26 s, no real pause.
        let dip_at = 26.0;
        let mut sig = tone(45.0, R16, 0.3);
        let a = (dip_at * R16 as f64) as usize;
        let b = a + (0.15 * R16 as f64) as usize;
        for s in &mut sig[a..b] {
            *s *= 0.03;
        }
        let cuts = cuts_for(&sig, R16, SegmentCfg::PARAKEET);
        assert!(!cuts.is_empty());
        assert_eq!(cuts[0].kind, CutKind::Forced);
        let center = (a + b) / 2;
        let off = (cuts[0].index as i64 - center as i64).abs();
        assert!(off <= (0.1 * R16 as f64) as i64, "off by {off} samples");
        assert!(cuts[0].index <= 30 * R16 as usize + frame_samples(R16));
        assert_partition(&sig, &cuts);
    }

    #[test]
    fn constant_signal_is_force_cut_at_the_hard_cap() {
        let sig = tone(100.0, R16, 0.3);
        let cuts = cuts_for(&sig, R16, SegmentCfg::PARAKEET);
        assert!(cuts.len() >= 3, "{cuts:?}");
        assert!(cuts.iter().all(|c| c.kind == CutKind::Forced));
        let hard = 30 * R16 as usize + frame_samples(R16);
        let mut prev = 0;
        for c in &cuts {
            assert!(c.index - prev <= hard, "segment too long");
            assert!(c.index - prev >= 10 * R16 as usize, "segment under min");
            prev = c.index;
        }
        assert_partition(&sig, &cuts);
    }

    fn periodic_pauses(total_secs: f64, period: f64, pause: f64, rate: u32) -> Vec<f32> {
        let mut parts = Vec::new();
        let mut t = 0.0;
        while t + 1e-9 < total_secs {
            parts.push(tone(period - pause, rate, 0.3));
            parts.push(quiet(pause, rate));
            t += period;
        }
        concat(&parts)
    }

    #[test]
    fn two_minute_take_with_pause_every_10s_parakeet_profile() {
        let sig = periodic_pauses(120.0, 10.0, 0.7, R16);
        let cuts = cuts_for(&sig, R16, SegmentCfg::PARAKEET);
        let segs = segments_from_cuts(sig.len(), &cuts);
        assert!((5..=12).contains(&segs.len()), "{} segments", segs.len());
        check_segment_bounds(&segs, R16, SegmentCfg::PARAKEET);
        assert_partition(&sig, &cuts);
    }

    #[test]
    fn two_minute_take_with_pause_every_10s_slow_profile() {
        let sig = periodic_pauses(120.0, 10.0, 0.7, R16);
        let cuts = cuts_for(&sig, R16, SegmentCfg::SLOW_ENGINE);
        let segs = segments_from_cuts(sig.len(), &cuts);
        assert!((3..=6).contains(&segs.len()), "{} segments", segs.len());
        check_segment_bounds(&segs, R16, SegmentCfg::SLOW_ENGINE);
        assert_partition(&sig, &cuts);
    }

    fn check_segment_bounds(segs: &[Range<usize>], rate: u32, cfg: SegmentCfg) {
        let min = (cfg.min_secs * rate as f64) as usize;
        let hard = (cfg.hard_secs * rate as f64) as usize + frame_samples(rate);
        for s in &segs[..segs.len() - 1] {
            let len = s.end - s.start;
            assert!(len >= min, "segment {s:?} shorter than min");
            assert!(len <= hard, "segment {s:?} longer than hard cap");
        }
    }

    #[test]
    fn noisy_pauses_still_cut() {
        let mut seed = 11;
        let sig = concat(&[
            tone(28.0, R16, 0.3),
            noise(1.0, R16, 0.01, &mut seed),
            tone(11.0, R16, 0.3),
        ]);
        let cuts = cuts_for(&sig, R16, SegmentCfg::PARAKEET);
        assert_eq!(cuts.len(), 1, "{cuts:?}");
        assert_eq!(cuts[0].kind, CutKind::Pause);
        assert_partition(&sig, &cuts);
    }

    #[test]
    fn quiet_speaker_is_not_cut_mid_speech() {
        let sig = tone(25.0, R16, 0.02);
        let cuts = cuts_for(&sig, R16, SegmentCfg::PARAKEET);
        assert!(cuts.is_empty(), "quiet continuous speech cut: {cuts:?}");
    }

    #[test]
    fn digital_silence_take_is_handled() {
        let sig = quiet(90.0, R16);
        let cuts = cuts_for(&sig, R16, SegmentCfg::PARAKEET);
        assert_partition(&sig, &cuts);
        // Silence is a pause everywhere, so cuts are Pause and spaced by min.
        assert!(cuts.iter().all(|c| c.kind == CutKind::Pause));
    }

    // ---- segmenter: incremental behaviour -----------------------------------

    #[test]
    fn chunked_feeding_matches_one_shot() {
        let mut seed = 21;
        let sig = concat(&[
            tone(14.0, R16, 0.3),
            noise(0.9, R16, 0.004, &mut seed),
            tone(19.0, R16, 0.25),
            quiet(0.6, R16),
            tone(12.0, R16, 0.3),
        ]);
        let want = cuts_for(&sig, R16, SegmentCfg::PARAKEET);
        assert!(!want.is_empty());
        for chunk in [1usize, 441, 1920, 4800, 7777] {
            let mut seg = PauseSegmenter::new(
                R16,
                SegmentCfg::PARAKEET,
                Box::new(EnergyPauseSource::new()),
            );
            let mut got = Vec::new();
            for piece in sig.chunks(chunk) {
                got.extend(seg.feed(piece));
            }
            assert_eq!(got, want, "chunk size {chunk}");
        }
    }

    #[test]
    fn cut_indices_never_exceed_samples_fed() {
        let sig = periodic_pauses(80.0, 10.0, 0.8, R16);
        let mut seg = PauseSegmenter::new(
            R16,
            SegmentCfg::PARAKEET,
            Box::new(EnergyPauseSource::new()),
        );
        let mut total = 0usize;
        for piece in sig.chunks(3200) {
            total += piece.len();
            for c in seg.feed(piece) {
                assert!(c.index <= total);
                assert!(c.index <= seg.fed());
            }
        }
        assert!(seg.seg_start() > 0);
    }

    // ---- segmenter with a scripted pause source ------------------------------

    #[test]
    fn scripted_pause_cuts_inside_the_run_and_after_min() {
        // Speech everywhere except the scripted run, whose samples are silent.
        let mut sig = tone(40.0, R16, 0.3);
        let (a, b) = (15 * R16 as usize, 16 * R16 as usize);
        for s in &mut sig[a..b] {
            *s = 0.0;
        }
        let src = Scripted::secs(&[(15.0, 16.0)], R16);
        let cuts = plan_cuts_with(&sig, R16, SegmentCfg::PARAKEET, src);
        assert_eq!(cuts.len(), 1, "{cuts:?}");
        assert_eq!(cuts[0].kind, CutKind::Pause);
        assert!(cuts[0].index > a && cuts[0].index < b);
        assert_silent_around(&sig, cuts[0].index, R16, 50);
    }

    #[test]
    fn scripted_run_that_ends_before_min_never_cuts() {
        let mut sig = tone(30.0, R16, 0.3);
        for s in &mut sig[8 * R16 as usize..9 * R16 as usize + 8000] {
            *s = 0.0;
        }
        let src = Scripted::secs(&[(8.0, 9.5)], R16);
        let cuts = plan_cuts_with(&sig, R16, SegmentCfg::PARAKEET, src);
        assert!(cuts.iter().all(|c| c.index >= 10 * R16 as usize));
    }

    #[test]
    fn scripted_silence_starting_before_min_waits_until_min() {
        // Silence runs from 9 s to 14 s: the cut must wait for the 10 s floor.
        let mut sig = tone(30.0, R16, 0.3);
        for s in &mut sig[9 * R16 as usize..14 * R16 as usize] {
            *s = 0.0;
        }
        let src = Scripted::secs(&[(9.0, 14.0)], R16);
        let cuts = plan_cuts_with(&sig, R16, SegmentCfg::PARAKEET, src);
        assert_eq!(cuts.len(), 1, "{cuts:?}");
        assert!(cuts[0].index >= 10 * R16 as usize);
        assert!(cuts[0].index < 14 * R16 as usize);
    }

    // ---- randomized invariants ----------------------------------------------

    #[test]
    fn random_signals_keep_the_partition_and_bounds() {
        let rate = 8_000u32;
        let mut seed = 0x5eed_u64;
        for case in 0..200 {
            let mut parts: Vec<Vec<f32>> = Vec::new();
            let mut total = 0.0;
            let target = 5.0 + lcg(&mut seed) as f64 * 65.0;
            while total < target {
                let secs = 0.2 + lcg(&mut seed) as f64 * 9.0;
                let part = match (lcg(&mut seed) * 4.0) as u32 {
                    0 => quiet(secs, rate),
                    1 => noise(secs, rate, 0.003 + lcg(&mut seed) * 0.02, &mut seed),
                    2 => tone(secs, rate, 0.02 + lcg(&mut seed) * 0.5),
                    _ => noise(secs, rate, 0.05 + lcg(&mut seed) * 0.2, &mut seed),
                };
                total += secs;
                parts.push(part);
            }
            let sig = concat(&parts);
            let cfg = if case % 2 == 0 {
                SegmentCfg::PARAKEET
            } else {
                SegmentCfg::SLOW_ENGINE
            };
            let cuts = plan_cuts(&sig, rate, cfg);
            assert_partition(&sig, &cuts);
            let segs = segments_from_cuts(sig.len(), &cuts);
            check_segment_bounds(&segs, rate, cfg);
        }
    }

    // ---- silero adapter -----------------------------------------------------

    fn model_path() -> PathBuf {
        PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/resources/silero_vad.onnx"
        ))
    }

    #[test]
    fn find_vad_model_returns_first_existing_candidate() {
        let missing = PathBuf::from("/definitely/not/here/silero_vad.onnx");
        let real = model_path();
        if !real.is_file() {
            assert!(find_vad_model(&[missing]).is_none());
            return;
        }
        assert_eq!(find_vad_model(&[missing, real.clone()]), Some(real));
    }

    #[test]
    fn missing_model_falls_back_to_the_energy_detector() {
        let bad = PathBuf::from("/definitely/not/here/silero_vad.onnx");
        let (mut src, label) = make_source(48_000, Some(&bad));
        assert_eq!(label, "energy");
        let f = vec![0.0f32; 960];
        src.push_frame(0, &f, 0.0);
        assert_eq!(src.silence_start(), Some(0));
        let (_, label) = make_source(48_000, None);
        assert_eq!(label, "energy");
    }

    #[test]
    fn corrupt_model_never_reaches_the_runtime() {
        // A bad file would abort the process inside the C++ runtime, so it
        // must be rejected by the size and hash check first.
        let dir = std::env::temp_dir().join(format!("dictflow-vad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("silero_vad.onnx");
        std::fs::write(&bad, b"not an onnx model").unwrap();
        assert!(!verify_silero_model(&bad));
        assert!(find_vad_model(std::slice::from_ref(&bad)).is_none());
        let (_, label) = make_source(16_000, Some(&bad));
        assert_eq!(label, "energy");

        // Right size, wrong content: still rejected.
        std::fs::write(&bad, vec![7u8; SILERO_MODEL_BYTES as usize]).unwrap();
        assert!(!verify_silero_model(&bad));
        let (_, label) = make_source(16_000, Some(&bad));
        assert_eq!(label, "energy");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bundled_model_matches_the_pinned_hash() {
        let path = model_path();
        if !path.is_file() {
            eprintln!("skipping: {} not present", path.display());
            return;
        }
        assert!(verify_silero_model(&path));
    }

    #[test]
    fn silero_reports_silence_for_digital_silence() {
        let path = model_path();
        if !path.is_file() {
            eprintln!("skipping: {} not present", path.display());
            return;
        }
        for rate in [16_000u32, 44_100, 48_000] {
            let mut src = SileroPauseSource::open(&path, rate).expect("model loads");
            let f = frame_samples(rate);
            let frame = vec![0.0f32; f];
            for k in 0..150 {
                src.push_frame(k * f, &frame, 0.0);
            }
            assert_eq!(src.silence_start(), Some(0), "rate {rate}");
        }
    }

    #[test]
    fn silero_segmenter_does_not_cut_before_min_on_silence() {
        let path = model_path();
        if !path.is_file() {
            eprintln!("skipping: {} not present", path.display());
            return;
        }
        let (src, label) = make_source(R16, Some(&path));
        assert_eq!(label, "silero");
        let sig = quiet(9.0, R16);
        let cuts = plan_cuts_with(&sig, R16, SegmentCfg::PARAKEET, src);
        assert!(cuts.is_empty());
    }

    /// Needs a real speech WAV: `DICTFLOW_TEST_WAV=/path/to.wav cargo test
    /// silero_cuts_real_speech -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn silero_cuts_real_speech() {
        let wav = std::env::var("DICTFLOW_TEST_WAV").expect("set DICTFLOW_TEST_WAV");
        let samples = crate::text::load_wav_mono_16k(Path::new(&wav)).expect("load wav");
        let (src, label) = make_source(R16, Some(&model_path()));
        let cuts = plan_cuts_with(&samples, R16, SegmentCfg::PARAKEET, src);
        println!(
            "{label}: {} cuts over {:.1}s",
            cuts.len(),
            samples.len() as f64 / 16_000.0
        );
        assert_partition(&samples, &cuts);
    }
}
