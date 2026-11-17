//! Background transcription of recording segments.
//!
//! A `SegmentSession` watches a live recording (through `PauseSegmenter`),
//! and every time a cut appears it hands that slice of audio to a per-take
//! worker thread. The worker decodes segments strictly in order while the user
//! keeps talking; when the user stops, only the tail is left to decode. The raw
//! texts are then joined in order and the normal text pipeline runs once.
//!
//! The STT call is injected (`SttFn`), so all of this is testable without a
//! speech model.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Instant;

use crate::devices;
use crate::segmenter::{Cut, CutKind, PauseSegmenter, PauseSource, SegmentCfg};
use crate::text;

/// Sample rate every segment is delivered at.
pub const SEGMENT_RATE: u32 = 16_000;

/// Speech-to-text for one segment: `(segment index, mono 16 kHz samples)`.
pub type SttFn = Arc<dyn Fn(usize, Vec<f32>) -> Result<String, String> + Send + Sync>;

static TAKE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Unique id for a take (used to name temp files so takes never collide).
pub fn next_take_id() -> u64 {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    ms.wrapping_mul(1000)
        .wrapping_add(TAKE_COUNTER.fetch_add(1, Ordering::Relaxed) % 1000)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentOutcome {
    Text(String),
    /// Silence, too short, or the engine found no speech. Not an error.
    Empty,
    /// The engine failed twice on this segment.
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct SegmentResult {
    pub index: usize,
    /// True when the cut that ended this segment was a forced one.
    pub forced_end: bool,
    pub outcome: SegmentOutcome,
    pub audio_secs: f64,
    pub stt_ms: u64,
}

struct Job {
    index: usize,
    forced_end: bool,
    samples: Vec<f32>,
}

/// True for engine errors that mean "no speech here" rather than a failure.
pub fn is_empty_error(message: &str) -> bool {
    if devices::is_blank_audio_error(message) {
        return true;
    }
    let m = message.to_ascii_lowercase();
    m.contains("transcription was empty")
        || m.contains("no audio captured")
        || m.contains("empty audio transcript")
}

/// Audible pieces shorter than this are padded with silence before decoding.
const MIN_DECODE_SECS: f64 = 0.3;

fn run_job(stt: &SttFn, cancelled: &AtomicBool, job: Job) -> SegmentResult {
    let audio_secs = job.samples.len() as f64 / SEGMENT_RATE as f64;
    let started = Instant::now();
    let finish = |outcome: SegmentOutcome| SegmentResult {
        index: job.index,
        forced_end: job.forced_end,
        outcome,
        audio_secs,
        stt_ms: started.elapsed().as_millis() as u64,
    };
    if cancelled.load(Ordering::SeqCst) {
        return finish(SegmentOutcome::Empty);
    }
    // Only true silence is skipped. A short but audible piece can be the last
    // word of a take, so it is padded up to what the engines accept.
    let silent = job.samples.is_empty()
        || devices::level_from_samples(&job.samples).peak < devices::SILENCE_PEAK;
    if silent {
        return finish(SegmentOutcome::Empty);
    }
    let floor = (SEGMENT_RATE as f64 * MIN_DECODE_SECS) as usize;
    let mut samples = job.samples.clone();
    if samples.len() < floor {
        samples.resize(floor, 0.0);
    }
    let mut last_error = String::new();
    for attempt in 0..2 {
        if attempt > 0 && cancelled.load(Ordering::SeqCst) {
            return finish(SegmentOutcome::Empty);
        }
        match stt(job.index, samples.clone()) {
            Ok(text) => {
                let t = text.trim();
                return finish(if t.is_empty() {
                    SegmentOutcome::Empty
                } else {
                    SegmentOutcome::Text(t.to_owned())
                });
            }
            Err(e) if is_empty_error(&e) => return finish(SegmentOutcome::Empty),
            Err(e) => {
                log::warn!("segment {} attempt {} failed: {e}", job.index, attempt + 1);
                last_error = e;
            }
        }
    }
    finish(SegmentOutcome::Failed(last_error))
}

/// One worker thread per take, decoding segments in the order they arrive.
pub struct SegmentWorker {
    tx: Option<mpsc::Sender<Job>>,
    handle: Option<JoinHandle<Vec<SegmentResult>>>,
    cancelled: Arc<AtomicBool>,
    queued: usize,
}

impl SegmentWorker {
    pub fn spawn(stt: SttFn) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let handle = std::thread::Builder::new()
            .name("dictflow-segments".to_owned())
            .spawn(move || {
                let mut results = Vec::new();
                for job in rx {
                    results.push(run_job(&stt, &flag, job));
                }
                results
            })
            .expect("spawn segment worker");
        SegmentWorker {
            tx: Some(tx),
            handle: Some(handle),
            cancelled,
            queued: 0,
        }
    }

    /// Queue one segment (mono 16 kHz). Returns its index.
    pub fn enqueue(&mut self, samples_16k: Vec<f32>, forced_end: bool) -> usize {
        let index = self.queued;
        self.queued += 1;
        if let Some(tx) = &self.tx {
            let _ = tx.send(Job {
                index,
                forced_end,
                samples: samples_16k,
            });
        }
        index
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.queued
    }

    /// Stop decoding queued segments. The segment already in the engine runs
    /// to completion; nothing after it is started.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Close the queue, wait for every segment, and return them in order.
    pub fn finish(mut self) -> Vec<SegmentResult> {
        drop(self.tx.take());
        match self.handle.take() {
            Some(handle) => handle.join().unwrap_or_default(),
            None => Vec::new(),
        }
    }
}

impl Drop for SegmentWorker {
    fn drop(&mut self) {
        // A worker dropped without `finish` means the take was abandoned
        // (cancelled or errored): stop starting new work, do not block.
        self.cancelled.store(true, Ordering::SeqCst);
        drop(self.tx.take());
    }
}

// ---------------------------------------------------------------------------
// Joining
// ---------------------------------------------------------------------------

/// Join segment texts in order with a single space.
///
/// `(text, forced_end)`: when a segment was ended by a forced cut (mid-speech)
/// its trailing full stop is dropped, because the recognizer punctuates every
/// clip as if it were a finished sentence. `?` and `!` are kept.
pub fn join_raw(parts: &[(String, bool)]) -> String {
    let mut out = String::new();
    let mut prev_forced = false;
    for (text, forced_end) in parts {
        let t = text.trim();
        if t.is_empty() {
            continue;
        }
        if !out.is_empty() {
            if prev_forced && out.ends_with('.') && !out.ends_with("..") {
                out.pop();
            }
            out.push(' ');
        }
        out.push_str(t);
        prev_forced = *forced_end;
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assembly {
    /// Every segment decoded; this is the joined raw text.
    Joined(String),
    /// No segment contained speech.
    AllEmpty,
    /// At least one segment failed even after its retry.
    NeedsFullPass(String),
}

pub fn assemble(results: &[SegmentResult]) -> Assembly {
    if let Some(err) = results.iter().find_map(|r| match &r.outcome {
        SegmentOutcome::Failed(e) => Some(e.clone()),
        _ => None,
    }) {
        return Assembly::NeedsFullPass(err);
    }
    let parts: Vec<(String, bool)> = results
        .iter()
        .filter_map(|r| match &r.outcome {
            SegmentOutcome::Text(t) => Some((t.clone(), r.forced_end)),
            _ => None,
        })
        .collect();
    if parts.is_empty() {
        Assembly::AllEmpty
    } else {
        Assembly::Joined(join_raw(&parts))
    }
}

// ---------------------------------------------------------------------------
// Live session
// ---------------------------------------------------------------------------

/// A live recording's segmenter plus its background worker.
pub struct SegmentSession {
    segmenter: PauseSegmenter,
    rate: u32,
    stt: SttFn,
    worker: Option<SegmentWorker>,
    consumed: usize,
    last_cut: usize,
    cuts: Vec<Cut>,
    source_label: &'static str,
}

/// What `finish` hands to the stop path.
pub struct FinishedSegments {
    pub worker: SegmentWorker,
    /// Audio after the last cut, at 16 kHz.
    pub tail_16k: Vec<f32>,
    pub cuts: Vec<Cut>,
    pub source_label: &'static str,
}

impl SegmentSession {
    pub fn new(
        rate: u32,
        cfg: SegmentCfg,
        source: Box<dyn PauseSource>,
        source_label: &'static str,
        stt: SttFn,
    ) -> Self {
        SegmentSession {
            segmenter: PauseSegmenter::new(rate, cfg, source),
            rate,
            stt,
            worker: None,
            consumed: 0,
            last_cut: 0,
            cuts: Vec::new(),
            source_label,
        }
    }

    #[cfg(test)]
    pub fn cut_count(&self) -> usize {
        self.cuts.len()
    }

    pub fn source_label(&self) -> &'static str {
        self.source_label
    }

    /// Scan the new part of the recording buffer and queue any finished
    /// segment. `all` is the whole device-rate buffer so far. Returns how many
    /// segments were queued.
    pub fn poll(&mut self, all: &[f32]) -> usize {
        if all.len() <= self.consumed {
            return 0;
        }
        let new = &all[self.consumed..];
        self.consumed = all.len();
        let cuts = self.segmenter.feed(new);
        for cut in &cuts {
            let slice = &all[self.last_cut..cut.index];
            let samples = text::resample_linear(slice, self.rate, SEGMENT_RATE);
            let stt = self.stt.clone();
            let worker = self.worker.get_or_insert_with(|| SegmentWorker::spawn(stt));
            worker.enqueue(samples, cut.kind == CutKind::Forced);
            self.last_cut = cut.index;
            self.cuts.push(*cut);
        }
        cuts.len()
    }

    /// Stop segmenting. `None` when the take never got a cut (the caller then
    /// uses the normal whole-take path).
    pub fn finish(self, all: &[f32]) -> Option<FinishedSegments> {
        let worker = self.worker?;
        let tail = &all[self.last_cut.min(all.len())..];
        Some(FinishedSegments {
            worker,
            tail_16k: text::resample_linear(tail, self.rate, SEGMENT_RATE),
            cuts: self.cuts,
            source_label: self.source_label,
        })
    }

    /// Abandon the take: stop queued work.
    pub fn cancel(self) {
        if let Some(w) = &self.worker {
            w.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segmenter::EnergyPauseSource;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    fn tone(secs: f64, rate: u32) -> Vec<f32> {
        let n = (secs * rate as f64) as usize;
        (0..n)
            .map(|i| 0.3 * (2.0 * std::f32::consts::PI * 220.0 * i as f32 / rate as f32).sin())
            .collect()
    }

    fn quiet(secs: f64, rate: u32) -> Vec<f32> {
        vec![0.0; (secs * rate as f64) as usize]
    }

    fn stt_ok(text: &'static str) -> SttFn {
        Arc::new(move |_, _| Ok(text.to_owned()))
    }

    fn text_of(r: &SegmentResult) -> &str {
        match &r.outcome {
            SegmentOutcome::Text(t) => t,
            _ => "",
        }
    }

    #[test]
    fn finish_with_no_jobs_returns_immediately() {
        let worker = SegmentWorker::spawn(stt_ok("x"));
        assert_eq!(worker.len(), 0);
        assert!(worker.finish().is_empty());
    }

    #[test]
    fn results_come_back_in_order_even_when_the_first_is_slow() {
        let stt: SttFn = Arc::new(|idx, _| {
            if idx == 0 {
                std::thread::sleep(Duration::from_millis(150));
            }
            Ok(format!("part{idx}"))
        });
        let mut w = SegmentWorker::spawn(stt);
        for _ in 0..3 {
            w.enqueue(tone(1.0, SEGMENT_RATE), false);
        }
        let res = w.finish();
        let texts: Vec<&str> = res.iter().map(text_of).collect();
        assert_eq!(texts, ["part0", "part1", "part2"]);
        assert_eq!(res.iter().map(|r| r.index).collect::<Vec<_>>(), [0, 1, 2]);
    }

    #[test]
    fn silent_segments_skip_the_engine_and_short_audible_ones_are_padded() {
        let lens = Arc::new(std::sync::Mutex::new(Vec::<usize>::new()));
        let l = lens.clone();
        let stt: SttFn = Arc::new(move |_, samples| {
            l.lock().unwrap().push(samples.len());
            Ok("speech".to_owned())
        });
        let mut w = SegmentWorker::spawn(stt);
        w.enqueue(quiet(2.0, SEGMENT_RATE), false); // silence
        w.enqueue(Vec::new(), false); // nothing at all
        w.enqueue(tone(0.1, SEGMENT_RATE), false); // short but audible: maybe a last word
        w.enqueue(tone(2.0, SEGMENT_RATE), false); // real
        let res = w.finish();
        assert_eq!(res[0].outcome, SegmentOutcome::Empty);
        assert_eq!(res[1].outcome, SegmentOutcome::Empty);
        assert_eq!(res[2].outcome, SegmentOutcome::Text("speech".into()));
        assert_eq!(res[3].outcome, SegmentOutcome::Text("speech".into()));
        let seen = lens.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "silence never reaches the engine");
        assert_eq!(seen[0], (SEGMENT_RATE as f64 * MIN_DECODE_SECS) as usize);
        assert_eq!(seen[1], (SEGMENT_RATE * 2) as usize);
    }

    #[test]
    fn engine_no_speech_errors_become_empty_not_failures() {
        for msg in [
            "transcription was empty",
            "no audio captured",
            "only silence — nothing to transcribe",
            "empty audio transcript",
        ] {
            let m = msg.to_owned();
            let stt: SttFn = Arc::new(move |_, _| Err(m.clone()));
            let mut w = SegmentWorker::spawn(stt);
            w.enqueue(tone(1.0, SEGMENT_RATE), false);
            let res = w.finish();
            assert_eq!(res[0].outcome, SegmentOutcome::Empty, "{msg}");
        }
    }

    #[test]
    fn all_empty_segments_assemble_to_all_empty() {
        let mut w = SegmentWorker::spawn(stt_ok("x"));
        w.enqueue(quiet(1.0, SEGMENT_RATE), false);
        w.enqueue(quiet(1.0, SEGMENT_RATE), true);
        assert_eq!(assemble(&w.finish()), Assembly::AllEmpty);
    }

    #[test]
    fn a_segment_that_fails_once_is_retried() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let stt: SttFn = Arc::new(move |_, _| {
            if c.fetch_add(1, Ordering::SeqCst) == 0 {
                Err("model busy".to_owned())
            } else {
                Ok("recovered".to_owned())
            }
        });
        let mut w = SegmentWorker::spawn(stt);
        w.enqueue(tone(1.0, SEGMENT_RATE), false);
        let res = w.finish();
        assert_eq!(res[0].outcome, SegmentOutcome::Text("recovered".into()));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_segment_that_fails_twice_needs_the_full_pass() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let stt: SttFn = Arc::new(move |idx, _| {
            c.fetch_add(1, Ordering::SeqCst);
            if idx == 1 {
                Err("engine crashed".to_owned())
            } else {
                Ok(format!("p{idx}"))
            }
        });
        let mut w = SegmentWorker::spawn(stt);
        for _ in 0..3 {
            w.enqueue(tone(1.0, SEGMENT_RATE), false);
        }
        let res = w.finish();
        // The failure of one segment does not stop the others.
        assert_eq!(res.len(), 3);
        assert_eq!(text_of(&res[2]), "p2");
        assert_eq!(calls.load(Ordering::SeqCst), 4); // 1 + 2 + 1
        assert_eq!(
            assemble(&res),
            Assembly::NeedsFullPass("engine crashed".into())
        );
    }

    #[test]
    fn cancel_stops_queued_segments_from_reaching_the_engine() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let started_tx = std::sync::Mutex::new(started_tx);
        let stt: SttFn = Arc::new(move |_, _| {
            c.fetch_add(1, Ordering::SeqCst);
            let _ = started_tx.lock().unwrap().send(());
            std::thread::sleep(Duration::from_millis(200));
            Ok("x".to_owned())
        });
        let mut w = SegmentWorker::spawn(stt);
        w.enqueue(tone(1.0, SEGMENT_RATE), false);
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        w.cancel();
        for _ in 0..5 {
            w.enqueue(tone(1.0, SEGMENT_RATE), false);
        }
        let res = w.finish();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(res.len(), 6);
        assert!(res[1..].iter().all(|r| r.outcome == SegmentOutcome::Empty));
    }

    #[test]
    fn dropping_the_worker_does_not_block_or_keep_decoding() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let stt: SttFn = Arc::new(move |_, _| {
            c.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(150));
            Ok("x".to_owned())
        });
        let mut w = SegmentWorker::spawn(stt);
        for _ in 0..4 {
            w.enqueue(tone(1.0, SEGMENT_RATE), false);
        }
        let t = Instant::now();
        drop(w);
        assert!(t.elapsed() < Duration::from_millis(100));
        std::thread::sleep(Duration::from_millis(700));
        assert!(calls.load(Ordering::SeqCst) <= 2);
    }

    // ---- join_raw -------------------------------------------------------------

    fn p(t: &str, forced: bool) -> (String, bool) {
        (t.to_owned(), forced)
    }

    #[test]
    fn join_trims_and_skips_empty_parts() {
        let got = join_raw(&[
            p("  hello there. ", false),
            p("", false),
            p("how are you?  ", false),
        ]);
        assert_eq!(got, "hello there. how are you?");
        assert_eq!(join_raw(&[]), "");
        assert_eq!(join_raw(&[p("   ", false)]), "");
    }

    #[test]
    fn join_leaves_text_alone_after_a_pause_cut() {
        let got = join_raw(&[p("First sentence.", false), p("Second one.", false)]);
        assert_eq!(got, "First sentence. Second one.");
    }

    #[test]
    fn join_strips_only_a_trailing_full_stop_after_a_forced_cut() {
        assert_eq!(
            join_raw(&[p("we went to the.", true), p("market today.", false)]),
            "we went to the market today."
        );
        assert_eq!(
            join_raw(&[p("is it really?", true), p("yes.", false)]),
            "is it really? yes."
        );
        assert_eq!(join_raw(&[p("wait!", true), p("no.", false)]), "wait! no.");
        assert_eq!(
            join_raw(&[p("and then...", true), p("it ended.", false)]),
            "and then... it ended."
        );
    }

    #[test]
    fn join_keeps_every_word_in_order_across_segments() {
        let words: Vec<String> = (0..200).map(|i| format!("w{i}")).collect();
        let bounds = [0usize, 17, 60, 61, 120, 199, 200];
        let parts: Vec<(String, bool)> = bounds
            .windows(2)
            .map(|b| (words[b[0]..b[1]].join(" "), false))
            .collect();
        let joined = join_raw(&parts);
        let got: Vec<&str> = joined.split_whitespace().collect();
        assert_eq!(got.len(), 200);
        for (i, w) in got.iter().enumerate() {
            assert_eq!(*w, format!("w{i}"));
        }
    }

    // ---- session ---------------------------------------------------------------

    fn record_len_stt(log: Arc<std::sync::Mutex<Vec<usize>>>) -> SttFn {
        Arc::new(move |idx, samples| {
            log.lock().unwrap().push(samples.len());
            Ok(format!("seg{idx}"))
        })
    }

    fn live_signal(rate: u32) -> Vec<f32> {
        // 9.3 s speech + 0.7 s pause, repeated: 55 s.
        let mut out = Vec::new();
        for _ in 0..5 {
            out.extend(tone(9.3, rate));
            out.extend(quiet(0.7, rate));
        }
        out.extend(tone(5.0, rate));
        out
    }

    #[test]
    fn session_covers_every_sample_exactly_once() {
        let rate = 16_000u32;
        let sig = live_signal(rate);
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut session = SegmentSession::new(
            rate,
            SegmentCfg::PARAKEET,
            Box::new(EnergyPauseSource::new()),
            "energy",
            record_len_stt(log.clone()),
        );
        // Simulate a live recording: the buffer grows 200 ms at a time.
        let step = rate as usize / 5;
        let mut end = 0;
        let mut queued = 0;
        while end < sig.len() {
            end = (end + step).min(sig.len());
            queued += session.poll(&sig[..end]);
        }
        assert!(queued >= 2, "expected several cuts, got {queued}");
        let fin = session.finish(&sig).expect("cuts were found");
        assert_eq!(fin.cuts.len(), queued);
        let mut worker = fin.worker;
        worker.enqueue(fin.tail_16k, false);
        let res = worker.finish();
        assert_eq!(res.len(), queued + 1);
        let total: usize = log.lock().unwrap().iter().sum::<usize>();
        // The tail is non-blank, so every segment reached the engine, and the
        // segment lengths add up to the whole buffer (rate is already 16 kHz).
        assert_eq!(total, sig.len());
    }

    #[test]
    fn session_at_48k_keeps_total_duration() {
        let rate = 48_000u32;
        let sig = live_signal(rate);
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut session = SegmentSession::new(
            rate,
            SegmentCfg::PARAKEET,
            Box::new(EnergyPauseSource::new()),
            "energy",
            record_len_stt(log.clone()),
        );
        let step = rate as usize / 5;
        let mut end = 0;
        while end < sig.len() {
            end = (end + step).min(sig.len());
            session.poll(&sig[..end]);
        }
        let fin = session.finish(&sig).expect("cuts");
        let mut worker = fin.worker;
        worker.enqueue(fin.tail_16k, false);
        let _ = worker.finish();
        let total: usize = log.lock().unwrap().iter().sum();
        let expect = sig.len() / 3;
        // Per-segment resampling may differ by a sample or two per segment.
        assert!(
            (total as i64 - expect as i64).abs() <= 8,
            "total {total} vs {expect}"
        );
    }

    #[test]
    fn session_without_a_cut_returns_none() {
        let rate = 16_000u32;
        let sig = tone(6.0, rate);
        let mut session = SegmentSession::new(
            rate,
            SegmentCfg::PARAKEET,
            Box::new(EnergyPauseSource::new()),
            "energy",
            stt_ok("x"),
        );
        assert_eq!(session.poll(&sig), 0);
        assert_eq!(session.cut_count(), 0);
        assert!(session.finish(&sig).is_none());
    }

    #[test]
    fn session_poll_is_idempotent_for_an_unchanged_buffer() {
        let rate = 16_000u32;
        let sig = live_signal(rate);
        let mut session = SegmentSession::new(
            rate,
            SegmentCfg::PARAKEET,
            Box::new(EnergyPauseSource::new()),
            "energy",
            stt_ok("x"),
        );
        let first = session.poll(&sig);
        assert!(first > 0);
        assert_eq!(session.poll(&sig), 0);
        assert_eq!(session.poll(&sig[..1000]), 0);
    }

    #[test]
    fn cancelled_session_stops_decoding() {
        let rate = 16_000u32;
        let sig = live_signal(rate);
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let stt: SttFn = Arc::new(move |_, _| {
            c.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(150));
            Ok("x".to_owned())
        });
        let mut session = SegmentSession::new(
            rate,
            SegmentCfg::PARAKEET,
            Box::new(EnergyPauseSource::new()),
            "energy",
            stt,
        );
        session.poll(&sig);
        assert!(session.cut_count() >= 2);
        session.cancel();
        std::thread::sleep(Duration::from_millis(600));
        assert!(calls.load(Ordering::SeqCst) <= 1);
    }

    #[test]
    fn take_ids_are_unique() {
        let ids: Vec<u64> = (0..50).map(|_| next_take_id()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len());
    }
}
