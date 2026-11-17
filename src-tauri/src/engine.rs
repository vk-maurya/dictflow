//! Local STT engines: in-process Parakeet (sherpa-onnx) and whisper-cli sidecar.
//!
//! The Parakeet recognizer is `!Send`, so it lives on one dedicated thread
//! for the life of the app. Whisper is a subprocess (`whisper-cli` /
//! `whisper-cli.exe`).

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use anyhow::Context;

use crate::clean;
use crate::devices;
use crate::email;
use crate::format;
use crate::models::{self, EngineKind};
use crate::numbers;
use crate::polish;
use crate::s1_runtime::S1Runtime;
use crate::segment_jobs::{
    assemble, next_take_id, Assembly, FinishedSegments, SegmentWorker, SttFn,
};
use crate::segmenter::{make_source, plan_cuts_with, segments_from_cuts, CutKind, SegmentCfg};
use crate::settings::Settings;
use crate::text;

pub(crate) fn resolve_binary(data_dir: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let bin_name = "whisper-cli.exe";
    #[cfg(not(target_os = "windows"))]
    let bin_name = "whisper-cli";

    let local = data_dir.join("bin").join(bin_name);
    if local.exists() {
        return Some(local);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let p = dir.join(bin_name);
            p.exists().then_some(p)
        })
    })
}

// ---------------------------------------------------------------------------
// Parakeet engine: dedicated thread owning the sherpa-onnx recognizer
// ---------------------------------------------------------------------------

enum EngineJob {
    Preload {
        model_id: String,
    },
    Transcribe {
        model_id: String,
        samples_16k: Vec<f32>,
        reply: mpsc::Sender<Result<String, String>>,
    },
}

#[derive(Clone)]
pub(crate) struct Transcriber {
    tx: mpsc::Sender<EngineJob>,
}

impl Transcriber {
    pub(crate) fn spawn(data_dir: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("dictflow-transcriber".to_owned())
            .spawn(move || transcriber_loop(rx, data_dir))
            .expect("spawn transcriber thread");
        Transcriber { tx }
    }

    pub(crate) fn preload(&self, model_id: &str) {
        let _ = self.tx.send(EngineJob::Preload {
            model_id: model_id.to_owned(),
        });
    }

    /// Blocking decode on the transcriber thread. Call from `spawn_blocking`.
    pub(crate) fn transcribe(
        &self,
        model_id: &str,
        samples_16k: Vec<f32>,
    ) -> Result<String, String> {
        let (rep_tx, rep_rx) = mpsc::channel();
        self.tx
            .send(EngineJob::Transcribe {
                model_id: model_id.to_owned(),
                samples_16k,
                reply: rep_tx,
            })
            .map_err(|e| format!("transcriber gone: {e}"))?;
        rep_rx
            .recv()
            .map_err(|e| format!("transcriber dropped reply: {e}"))?
    }
}

fn num_threads() -> i32 {
    std::thread::available_parallelism()
        .map(|n| n.get().min(4) as i32)
        .unwrap_or(2)
}

fn ensure_parakeet<'a>(
    loaded: &'a mut Option<(String, sherpa_onnx::OfflineRecognizer)>,
    data_dir: &Path,
    model_id: &str,
) -> Result<&'a sherpa_onnx::OfflineRecognizer, String> {
    let needs_load = !matches!(loaded, Some((id, _)) if id == model_id);
    if needs_load {
        let entry = models::find(model_id).ok_or_else(|| format!("unknown model: {model_id}"))?;
        if entry.engine != EngineKind::Parakeet {
            return Err(format!("{model_id} is not a Parakeet model"));
        }
        if !entry.is_downloaded(data_dir) {
            return Err(format!(
                "{} is not downloaded yet — open Models and download it first",
                entry.name
            ));
        }
        let dir = entry.dir(data_dir);
        let file = |name: &str| dir.join(name).display().to_string();
        let mut cfg = sherpa_onnx::OfflineRecognizerConfig::default();
        cfg.model_config.transducer = sherpa_onnx::OfflineTransducerModelConfig {
            encoder: Some(file("encoder.int8.onnx")),
            decoder: Some(file("decoder.int8.onnx")),
            joiner: Some(file("joiner.int8.onnx")),
        };
        cfg.model_config.tokens = Some(file("tokens.txt"));
        cfg.model_config.provider = Some("cpu".to_owned());
        cfg.model_config.num_threads = num_threads();
        let recognizer = sherpa_onnx::OfflineRecognizer::create(&cfg)
            .ok_or_else(|| "load Parakeet model failed (create returned null)".to_owned())?;
        *loaded = Some((model_id.to_owned(), recognizer));
    }
    Ok(&loaded.as_ref().expect("recognizer just loaded").1)
}

fn decode_parakeet(
    recognizer: &sherpa_onnx::OfflineRecognizer,
    samples_16k: &[f32],
) -> Result<String, String> {
    if samples_16k.is_empty() {
        return Err("no audio captured".to_owned());
    }
    let stream = recognizer.create_stream();
    stream.accept_waveform(16_000, samples_16k);
    recognizer.decode(&stream);
    stream
        .get_result()
        .map(|r| r.text.trim().to_owned())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| "transcription was empty".to_owned())
}

fn transcriber_loop(rx: mpsc::Receiver<EngineJob>, data_dir: PathBuf) {
    let mut loaded: Option<(String, sherpa_onnx::OfflineRecognizer)> = None;
    for job in rx {
        match job {
            EngineJob::Preload { model_id } => {
                match ensure_parakeet(&mut loaded, &data_dir, &model_id) {
                    Ok(_) => log::info!("parakeet warmed up: {model_id}"),
                    Err(e) => log::warn!("parakeet preload failed ({model_id}): {e}"),
                }
            }
            EngineJob::Transcribe {
                model_id,
                samples_16k,
                reply,
            } => {
                let out = ensure_parakeet(&mut loaded, &data_dir, &model_id)
                    .and_then(|rec| decode_parakeet(rec, &samples_16k));
                let _ = reply.send(out);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Transcription routing + post-processing
// ---------------------------------------------------------------------------

pub(crate) struct TranscribeInput {
    pub wav_path: PathBuf,
    pub model_id: String,
    pub language: String,
    pub cleanup: String,
    pub translate: bool,
    pub data_dir: PathBuf,
    pub dictionary: Vec<text::DictionaryEntry>,
    pub polish: PolishCfg,
    pub s1_runtime: S1Runtime,
}

#[derive(Clone)]
pub(crate) struct PolishCfg {
    pub audio_backend: String,
    pub audio_api_base: String,
    pub audio_api_model: String,
    pub audio_api_key: String,
    pub llm_backend: String,
    pub llm_api_base: String,
    pub llm_api_model: String,
    pub llm_api_key: String,
    pub llm_temperature: f32,
    pub llm_timeout_ms: u64,
    pub llm_preset: String,
    pub llm_custom_prompt: String,
    pub llm_enabled: bool,
    pub s1_styling: String,
    pub s1_structure: String,
    pub s1_context: String,
}

pub(crate) fn polish_cfg(settings: &Settings, data_dir: &Path) -> PolishCfg {
    let secrets = polish::load_secrets(data_dir);
    let audio_base = if settings.audio_api_base.trim().is_empty() {
        polish::default_base("openai_compat").to_owned()
    } else {
        settings.audio_api_base.clone()
    };
    let llm_base = polish::canonical_llm_base(&settings.llm_backend, &settings.llm_api_base);
    PolishCfg {
        audio_backend: settings.audio_backend.clone(),
        audio_api_base: audio_base,
        audio_api_model: settings.audio_api_model.clone(),
        audio_api_key: secrets.audio_api_key,
        llm_backend: settings.llm_backend.clone(),
        llm_api_base: llm_base,
        llm_api_model: settings.llm_api_model.clone(),
        llm_api_key: secrets.llm_api_key,
        llm_temperature: settings.llm_temperature,
        llm_timeout_ms: settings.llm_timeout_ms,
        llm_preset: settings.llm_preset.clone(),
        llm_custom_prompt: settings.llm_custom_prompt.clone(),
        llm_enabled: settings.llm_enabled,
        s1_styling: settings.s1_styling.clone(),
        s1_structure: settings.s1_structure.clone(),
        s1_context: settings.s1_context.clone(),
    }
}

/// Temp files for one whisper-cli call: `(16 kHz wav, output prefix)`. The tag
/// is unique per take and segment, so overlapping or back-to-back calls never
/// read each other's output.
pub(crate) fn whisper_temp_paths(data_dir: &Path, tag: &str) -> (PathBuf, PathBuf) {
    (
        data_dir.join(format!("seg-{tag}.wav")),
        data_dir.join(format!("seg-{tag}")),
    )
}

fn transcribe_whisper(
    data_dir: &Path,
    model_id: &str,
    language: &str,
    translate: bool,
    wav_16k_path: &Path,
    out_prefix: &Path,
) -> anyhow::Result<String> {
    let entry = models::find(model_id).context("unknown model")?;
    let model_path = entry
        .single_path(data_dir)
        .context("not a single-file model")?;
    if !model_path.exists() {
        anyhow::bail!(
            "model not downloaded yet: {} (open Models and download it first)",
            model_path.display()
        );
    }
    let Some(binary) = resolve_binary(data_dir) else {
        anyhow::bail!(
            "whisper-cli not found — place a whisper.cpp build at {} or put it on PATH",
            data_dir.join("bin").display()
        );
    };
    // whisper.cpp CLI: whisper-cli -m <model> -f <wav> -otxt -of <out-prefix>
    let mut cmd = std::process::Command::new(&binary);
    cmd.arg("-m")
        .arg(&model_path)
        .arg("-f")
        .arg(wav_16k_path)
        .arg("-otxt")
        .arg("-of")
        .arg(out_prefix)
        .arg("--no-prints");
    if language != "auto" && !language.is_empty() {
        cmd.arg("-l").arg(language);
    }
    // whisper.cpp `--translate` renders English from a foreign-language clip.
    // It needs an explicit source language, so it only applies off-"auto".
    if translate && language != "auto" && !language.is_empty() {
        cmd.arg("--translate");
    }
    let status = cmd
        .status()
        .with_context(|| format!("run {}", binary.display()))?;
    if !status.success() {
        anyhow::bail!("whisper-cli exited with {status}");
    }
    let txt_path = out_prefix.with_extension("txt");
    let text = std::fs::read_to_string(&txt_path)
        .with_context(|| format!("read {}", txt_path.display()))?;
    Ok(text)
}

pub(crate) struct TranscriptResult {
    pub text: String,
    pub raw_text: String,
    pub duration_secs: f64,
    pub dict_hits: u32,
    /// Shown when model cleanup was requested and local rules were used instead.
    pub cleanup_note: Option<String>,
}

/// P0 rules passes that the S1 pipeline doesn't cover, run before it:
/// dictionary command fixes (an entry whose replacement is a spoken command is
/// expanded so the command pass can act on it) → spoken punctuation/lists →
/// spoken numbers → digits → English email compose. Returns the processed text
/// plus whether an email layout short-circuited the rest of the pipeline.
pub(crate) fn rules_preprocess(
    raw_text: &str,
    language: &str,
    dictionary: &[text::DictionaryEntry],
) -> (String, bool) {
    let (cmd_fixed, _) = text::apply_command_fixes(raw_text, dictionary);
    // Numbers run before the command pass, matching the P0 order: otherwise
    // the spoken-command table would substitute the bare word "percent" with
    // "%" first and `fifty percent` could never become `50%`.
    let with_numbers = numbers::apply_spoken_numbers(&cmd_fixed);
    let formatted = format::apply_offline_format(&with_numbers);
    if language == "en" || language == "auto" {
        if let Some(mail) = email::compose_email(&formatted) {
            return (mail, true);
        }
    }
    (formatted, false)
}

pub(crate) fn polish_preset<'a>(cleanup: &str, preset: &'a str, custom: &str) -> &'a str {
    if preset == "custom" && !custom.trim().is_empty() {
        return "custom";
    }
    match cleanup {
        "high" => "professional",
        "medium" => "clean",
        _ => preset,
    }
}

fn cleanup_failure_note(err: &anyhow::Error) -> String {
    let raw = format!("{err:#}").replace('\n', " ");
    if raw.contains("429") {
        return "The cleanup model rate-limited this request (429), so your prompt did not run. Local cleanup was pasted instead. Wait a minute or switch off the free-tier model.".to_owned();
    }
    let brief: String = raw.chars().take(160).collect();
    format!("Text cleanup did not run, so local cleanup was pasted. {brief}")
}

fn transcript_is_english(audio_backend: &str, model_id: &str, language: &str, text: &str) -> bool {
    let whisper_forced_english = audio_backend != "openai_compat"
        && matches!(language, "en" | "en-US" | "en-GB")
        && models::find(model_id).is_some_and(|m| m.engine == EngineKind::Whisper);
    if whisper_forced_english {
        return true;
    }
    // Parakeet and generic audio APIs do not expose a reliable language code.
    // Skip S1 only when detection is confidently non-English; short or
    // ambiguous phrases should not be silently denied cleanup.
    !whatlang::detect(text)
        .is_some_and(|info| info.lang() != whatlang::Lang::Eng && info.confidence() >= 0.80)
}

/// Everything the speech-to-text step needs, without the text pipeline.
#[derive(Clone)]
pub(crate) struct SttSettings {
    pub model_id: String,
    pub language: String,
    pub translate: bool,
    pub data_dir: PathBuf,
    pub audio_backend: String,
    pub audio_api_base: String,
    pub audio_api_model: String,
    pub audio_api_key: String,
    pub timeout_ms: u64,
}

impl SttSettings {
    pub(crate) fn from_input(input: &TranscribeInput) -> Self {
        SttSettings {
            model_id: input.model_id.clone(),
            language: input.language.clone(),
            translate: input.translate,
            data_dir: input.data_dir.clone(),
            audio_backend: input.polish.audio_backend.clone(),
            audio_api_base: input.polish.audio_api_base.clone(),
            audio_api_model: input.polish.audio_api_model.clone(),
            audio_api_key: input.polish.audio_api_key.clone(),
            timeout_ms: input.polish.llm_timeout_ms,
        }
    }

    pub(crate) fn from_settings(settings: &Settings, data_dir: &Path) -> Self {
        let cfg = polish_cfg(settings, data_dir);
        SttSettings {
            model_id: settings.model_id.clone(),
            language: settings.language.clone(),
            translate: settings.translate,
            data_dir: data_dir.to_path_buf(),
            audio_backend: cfg.audio_backend,
            audio_api_base: cfg.audio_api_base,
            audio_api_model: cfg.audio_api_model,
            audio_api_key: cfg.audio_api_key,
            timeout_ms: cfg.llm_timeout_ms,
        }
    }

    /// Whisper sidecar and the cloud audio API pay a fixed cost per call, so
    /// they get longer segments than in-process Parakeet.
    pub(crate) fn segment_cfg(&self) -> SegmentCfg {
        if self.audio_backend == "openai_compat" {
            return SegmentCfg::SLOW_ENGINE;
        }
        match models::find(&self.model_id).map(|m| m.engine) {
            Some(EngineKind::Parakeet) => SegmentCfg::PARAKEET,
            _ => SegmentCfg::SLOW_ENGINE,
        }
    }
}

/// Speech-to-text only: mono 16 kHz samples in, raw recognizer text out.
/// `tag` names temp files (unique per take and segment).
pub(crate) fn transcribe_samples(
    stt: &SttSettings,
    parakeet: &Transcriber,
    samples_16k: Vec<f32>,
    tag: &str,
) -> anyhow::Result<String> {
    if stt.audio_backend == "openai_compat" {
        let wav = text::wav_bytes_mono_16k(&samples_16k, 16_000)?;
        return polish::transcribe_openai(
            &stt.audio_api_base,
            &stt.audio_api_key,
            &stt.audio_api_model,
            stt.timeout_ms.max(15_000),
            &wav,
        );
    }
    let entry = models::find(&stt.model_id).context("unknown model")?;
    match entry.engine {
        EngineKind::Whisper => {
            let (tmp, prefix) = whisper_temp_paths(&stt.data_dir, tag);
            text::write_wav_mono_16k(&tmp, &samples_16k, 16_000)?;
            let out = transcribe_whisper(
                &stt.data_dir,
                &stt.model_id,
                &stt.language,
                stt.translate,
                &tmp,
                &prefix,
            );
            let _ = std::fs::remove_file(&tmp);
            let _ = std::fs::remove_file(prefix.with_extension("txt"));
            out
        }
        EngineKind::Parakeet => parakeet
            .transcribe(&stt.model_id, samples_16k)
            .map_err(anyhow::Error::msg),
    }
}

/// STT callback for the segment worker. Each call gets its own temp-file tag.
pub(crate) fn segment_stt(stt: SttSettings, parakeet: Transcriber, take_id: u64) -> SttFn {
    std::sync::Arc::new(move |index, samples| {
        transcribe_samples(&stt, &parakeet, samples, &format!("{take_id}-{index}"))
            .map_err(|e| format!("{e:#}"))
    })
}

/// Heavy work: runs inside `spawn_blocking` so the async runtime stays free.
pub(crate) fn run_transcription(
    input: &TranscribeInput,
    parakeet: &Transcriber,
) -> anyhow::Result<TranscriptResult> {
    let samples = text::load_wav_mono_16k(&input.wav_path)?;
    let duration_secs = samples.len() as f64 / 16_000.0;
    if let Some(why) = devices::blank_audio(&samples, 16_000) {
        anyhow::bail!("{}", why.message());
    }
    let stt = SttSettings::from_input(input);
    let tag = format!("{}-full", next_take_id());
    let raw = transcribe_samples(&stt, parakeet, samples, &tag)?;
    Ok(postprocess(&raw, duration_secs, input))
}

/// One log line fragment per segment: `#index audio-seconds/stt-ms`.
fn describe_segments(results: &[crate::segment_jobs::SegmentResult]) -> String {
    results
        .iter()
        .map(|r| format!("#{} {:.1}s/{}ms", r.index, r.audio_secs, r.stt_ms))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Finish a take that was segmented while recording: queue the tail, wait for
/// every segment, join the raw texts in order, then run the text pipeline once
/// on the whole transcript. If any segment failed, decode the full WAV instead
/// so the result is never worse than the non-segmented path.
pub(crate) fn run_segmented(
    input: &TranscribeInput,
    parakeet: &Transcriber,
    finished: FinishedSegments,
    duration_secs: f64,
) -> anyhow::Result<TranscriptResult> {
    let started = std::time::Instant::now();
    let FinishedSegments {
        mut worker,
        tail_16k,
        cuts,
        source_label,
    } = finished;
    // A silent tail is just the pause after the last cut. A short but audible
    // tail can be a final word, so it is still decoded (the engine's own
    // "empty" result is handled like any other silent segment).
    let tail_silent =
        tail_16k.is_empty() || devices::level_from_samples(&tail_16k).peak < devices::SILENCE_PEAK;
    if !tail_silent {
        worker.enqueue(tail_16k, false);
    }
    let results = worker.finish();
    let forced = cuts.iter().filter(|c| c.kind == CutKind::Forced).count();
    log::info!(
        "segmented take ({source_label}): {} segments ({} forced cuts), {:.1}s audio, \
         stop-to-text {} ms, segments [{}]",
        results.len(),
        forced,
        duration_secs,
        started.elapsed().as_millis(),
        describe_segments(&results)
    );
    match assemble(&results) {
        Assembly::Joined(raw) => Ok(postprocess(&raw, duration_secs, input)),
        Assembly::AllEmpty => anyhow::bail!("only silence — nothing to transcribe"),
        Assembly::NeedsFullPass(reason) => {
            log::warn!("segment failed ({reason}); decoding the full recording instead");
            run_transcription(input, parakeet)
        }
    }
}

/// Segment a finished recording (an imported file) at its pauses and decode it
/// piece by piece. Falls back to the single-call path when there is nothing to
/// cut or the file is short.
pub(crate) fn run_file_transcription(
    input: &TranscribeInput,
    parakeet: &Transcriber,
    segmenting: Option<(SegmentCfg, Option<PathBuf>)>,
) -> anyhow::Result<TranscriptResult> {
    let Some((cfg, vad_model)) = segmenting else {
        return run_transcription(input, parakeet);
    };
    let samples = text::load_wav_mono_16k(&input.wav_path)?;
    let duration_secs = samples.len() as f64 / 16_000.0;
    if let Some(why) = devices::blank_audio(&samples, 16_000) {
        anyhow::bail!("{}", why.message());
    }
    if duration_secs < cfg.min_secs {
        return run_transcription(input, parakeet);
    }
    let (source, label) = make_source(16_000, vad_model.as_deref());
    let cuts = plan_cuts_with(&samples, 16_000, cfg, source);
    if cuts.is_empty() {
        return run_transcription(input, parakeet);
    }
    let stt = segment_stt(
        SttSettings::from_input(input),
        parakeet.clone(),
        next_take_id(),
    );
    let mut worker = SegmentWorker::spawn(stt);
    let ranges = segments_from_cuts(samples.len(), &cuts);
    let last = ranges.len() - 1;
    for (i, r) in ranges.into_iter().enumerate() {
        let forced = cuts.get(i).is_some_and(|c| c.kind == CutKind::Forced);
        worker.enqueue(samples[r].to_vec(), i < last && forced);
    }
    let results = worker.finish();
    log::info!(
        "segmented file ({label}): {} segments, {:.1}s audio, segments [{}]",
        results.len(),
        duration_secs,
        describe_segments(&results)
    );
    match assemble(&results) {
        Assembly::Joined(raw) => Ok(postprocess(&raw, duration_secs, input)),
        Assembly::AllEmpty => anyhow::bail!("only silence — nothing to transcribe"),
        Assembly::NeedsFullPass(reason) => {
            log::warn!("segment failed ({reason}); decoding the full file instead");
            run_transcription(input, parakeet)
        }
    }
}

/// The text pipeline, run once on the full raw transcript: dictionary command
/// fixes, spoken punctuation/lists/numbers, email compose, S1 or model cleanup,
/// dictionary, rule cleanup.
pub(crate) fn postprocess(
    raw: &str,
    duration_secs: f64,
    input: &TranscribeInput,
) -> TranscriptResult {
    let raw_text = raw.trim().to_owned();

    // P0 rules passes that the S1 pipeline doesn't cover run before it:
    // dictionary command fixes → spoken punctuation/lists → spoken numbers →
    // English email compose (which short-circuits everything below).
    let (pre, is_email) = rules_preprocess(&raw_text, &input.language, &input.dictionary);
    if is_email {
        return TranscriptResult {
            text: pre,
            raw_text,
            duration_secs,
            dict_hits: 0,
            cleanup_note: None,
        };
    }

    let want_llm = if input.polish.llm_backend == "s1_mini" {
        input.polish.llm_enabled
    } else {
        input.polish.llm_enabled || input.cleanup == "medium" || input.cleanup == "high"
    };
    let wants_s1 = want_llm
        && input.polish.llm_backend == "s1_mini"
        && transcript_is_english(
            &input.polish.audio_backend,
            &input.model_id,
            &input.language,
            &pre,
        );
    let mut cleanup_note = None;
    let normalized = if wants_s1 {
        match input
            .s1_runtime
            .ensure(&input.data_dir, input.polish.llm_timeout_ms.max(15_000))
            .and_then(|base| {
                polish::polish_s1(
                    &base,
                    input.polish.llm_timeout_ms.max(15_000),
                    &input.polish.s1_styling,
                    &input.polish.s1_structure,
                    &input.polish.s1_context,
                    &pre,
                )
            }) {
            Ok(text) => Some(text),
            Err(error) => {
                cleanup_note = Some(
                    "S1-mini was unavailable, so rules-only cleanup was used.".to_owned(),
                );
                log::warn!("S1-mini cleanup skipped: {error:#}");
                None
            }
        }
    } else {
        None
    };

    // Dictionary is intentionally after S1 so user-defined names win.
    let mut dictionary = input.dictionary.clone();
    let (mut out, dict_hits) =
        text::apply_dictionary(normalized.as_deref().unwrap_or(&pre), &mut dictionary);
    if dict_hits > 0 {
        text::sort_dictionary(&mut dictionary);
        let _ = text::save_dictionary(&input.data_dir, &dictionary);
    }
    // S1 already handles fillers and punctuation. Rules remain the fallback,
    // including the conversational repair cues from clean.rs ("actually",
    // "I mean", "no sorry").
    if normalized.is_none() {
        match input.cleanup.as_str() {
            "light" | "medium" | "full" | "high" => out = clean::clean(&out, &input.language),
            _ => {}
        }
        if matches!(input.cleanup.as_str(), "full" | "high") {
            out = text::tidy_punctuation(&out);
        }
    }
    out = text::smart_trailing_punctuation(&out);
    if want_llm && input.polish.llm_backend != "off" && input.polish.llm_backend != "s1_mini" {
        let preset = polish_preset(
            &input.cleanup,
            &input.polish.llm_preset,
            &input.polish.llm_custom_prompt,
        );
        match polish::polish(&polish::PolishArgs {
            backend: &input.polish.llm_backend,
            base_url: &input.polish.llm_api_base,
            api_key: &input.polish.llm_api_key,
            model: &input.polish.llm_api_model,
            preset,
            custom_prompt: &input.polish.llm_custom_prompt,
            temperature: input.polish.llm_temperature,
            timeout_ms: input.polish.llm_timeout_ms,
            text: &out,
        }) {
            Ok(Some(polished)) => out = polished,
            Ok(None) => {
                cleanup_note = Some(
                    "Text cleanup returned nothing, so local cleanup was pasted.".to_owned(),
                );
            }
            Err(error) => {
                cleanup_note = Some(cleanup_failure_note(&error));
            }
        }
    }
    let out = out.trim().to_owned();
    TranscriptResult {
        text: out,
        raw_text,
        duration_secs,
        dict_hits,
        cleanup_note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preprocess_spoken_numbers_become_digits() {
        let (out, is_email) = rules_preprocess("four point five stars", "en", &[]);
        assert_eq!(out, "4.5 stars");
        assert!(!is_email);

        let (out, _) = rules_preprocess("two hundred fifty dollars", "en", &[]);
        assert_eq!(out, "250 dollars");

        let (out, _) = rules_preprocess("save fifty percent", "en", &[]);
        assert_eq!(out, "save 50%");

        let (out, _) = rules_preprocess("one lakh twenty thousand", "en", &[]);
        assert_eq!(out, "1,20,000");
    }

    #[test]
    fn preprocess_email_short_circuits_and_is_language_gated() {
        let (out, is_email) = rules_preprocess(
            "draft an email to Sarah telling her that I will be late tomorrow, cheers, Sarah",
            "en",
            &[],
        );
        assert!(is_email);
        assert_eq!(
            out,
            "Hi Sarah,\n\nI will be late tomorrow.\n\nCheers,\nSarah"
        );

        // Non-English never builds an email layout.
        let (_, is_email) = rules_preprocess(
            "write an email to Sarah saying hi, best regards, Sarah",
            "de",
            &[],
        );
        assert!(!is_email);
    }

    #[test]
    fn preprocess_dictionary_command_fixes_expand_first() {
        // A dictionary entry whose replacement is a spoken command is expanded
        // first, so the command pass turns it into punctuation.
        let d = vec![text::DictionaryEntry::new("1", "kocia mark", "question mark")];
        let (out, _) = rules_preprocess("are you there kocia mark", "en", &d);
        assert_eq!(out, "are you there?");
    }

    #[test]
    fn s1_language_gate_handles_forced_and_detected_sources() {
        assert!(transcript_is_english("local", "base.en", "en", "hi"));
        assert!(transcript_is_english(
            "local",
            "parakeet-v3",
            "en",
            "I am begging pardon"
        ));
        assert!(transcript_is_english(
            "local",
            "parakeet-v3",
            "en",
            "This is a sufficiently clear English sentence for language detection."
        ));
        assert!(!transcript_is_english(
            "local",
            "parakeet-v3",
            "en",
            "Esta es una oración claramente escrita en español para detectar el idioma."
        ));
        assert!(!transcript_is_english(
            "openai_compat",
            "base.en",
            "en",
            "Ceci est une longue phrase clairement écrite en français pour vérifier la détection de la langue."
        ));
    }

    #[test]
    fn custom_prompt_is_not_replaced_by_cleanup_level() {
        assert_eq!(polish_preset("high", "custom", "Format lists"), "custom");
        assert_eq!(
            polish_preset("medium", "custom", "Format lists"),
            "custom"
        );
        assert_eq!(polish_preset("high", "clean", ""), "professional");
        assert_eq!(polish_preset("medium", "professional", ""), "clean");
        assert_eq!(polish_preset("full", "casual", ""), "casual");
        assert_eq!(polish_preset("high", "custom", "  "), "professional");
    }

    // ---- chunked dictation (group C) ----

    use crate::segment_jobs::join_raw;

    fn test_input() -> TranscribeInput {
        let dir = std::env::temp_dir().join("dictflow-engine-tests");
        let settings = Settings::default();
        TranscribeInput {
            wav_path: dir.join("unused.wav"),
            model_id: settings.model_id.clone(),
            language: "en".to_owned(),
            cleanup: "full".to_owned(),
            translate: false,
            data_dir: dir.clone(),
            dictionary: Vec::new(),
            polish: polish_cfg(&settings, &dir),
            s1_runtime: S1Runtime::default(),
        }
    }

    fn tone(secs: f32) -> Vec<f32> {
        (0..(16_000.0 * secs) as usize)
            .map(|i| (i as f32 * 0.1).sin() * 0.3)
            .collect()
    }

    fn parts(texts: &[&str]) -> Vec<(String, bool)> {
        texts.iter().map(|t| ((*t).to_owned(), false)).collect()
    }

    #[test]
    fn postprocess_of_joined_parts_equals_whole_text() {
        let input = test_input();
        let samples = [
            "save fifty percent on two hundred fifty dollars today",
            "first buy milk comma then eggs period and that is all",
            "four point five stars from one lakh twenty thousand users",
        ];
        for whole in samples {
            let expected = postprocess(whole, 5.0, &input).text;
            let words: Vec<&str> = whole.split_whitespace().collect();
            for split in 1..words.len() {
                let a = words[..split].join(" ");
                let b = words[split..].join(" ");
                let joined = join_raw(&parts(&[&a, &b]));
                assert_eq!(joined, whole, "join must rebuild the text at split {split}");
                assert_eq!(
                    postprocess(&joined, 5.0, &input).text,
                    expected,
                    "split {split} of {whole:?}"
                );
            }
        }
    }

    #[test]
    fn numbers_split_across_segments_are_still_recognised() {
        let input = test_input();
        let joined = join_raw(&parts(&["save fifty", "percent on everything"]));
        let out = postprocess(&joined, 4.0, &input).text;
        assert!(out.contains("50%"), "got {out:?}");

        // Running the pipeline per part instead would lose the number.
        let per_part: Vec<String> = ["save fifty", "percent on everything"]
            .iter()
            .map(|p| postprocess(p, 2.0, &input).text)
            .collect();
        assert!(!per_part.join(" ").contains("50%"));
    }

    #[test]
    fn email_command_split_across_segments_still_composes() {
        let input = test_input();
        let whole =
            "draft an email to Sarah telling her that I will be late tomorrow, cheers, Sarah";
        let words: Vec<&str> = whole.split_whitespace().collect();
        let expected = postprocess(whole, 6.0, &input).text;
        assert!(expected.starts_with("Hi Sarah,"));
        for split in 1..words.len() {
            let joined = join_raw(&parts(&[
                &words[..split].join(" "),
                &words[split..].join(" "),
            ]));
            assert_eq!(
                postprocess(&joined, 6.0, &input).text,
                expected,
                "split {split}"
            );
        }
    }

    #[test]
    fn postprocess_keeps_raw_text_trimmed() {
        let input = test_input();
        let r = postprocess("  hello there  ", 1.0, &input);
        assert_eq!(r.raw_text, "hello there");
        assert_eq!(r.duration_secs, 1.0);
    }

    #[test]
    fn whisper_temp_paths_are_unique_per_tag() {
        let dir = Path::new("/tmp/dictflow");
        let (w1, p1) = whisper_temp_paths(dir, "7-0");
        let (w2, p2) = whisper_temp_paths(dir, "7-1");
        let (w3, p3) = whisper_temp_paths(dir, "8-0");
        assert_ne!(w1, w2);
        assert_ne!(p1, p2);
        assert_ne!(w1, w3);
        assert_ne!(p1, p3);
        // The text file whisper writes is `<prefix>.txt`, also unique.
        assert_ne!(p1.with_extension("txt"), p2.with_extension("txt"));
        assert!(w1.starts_with(dir) && p1.starts_with(dir));
        // Never the old shared names.
        assert_ne!(p1, dir.join("last_transcription"));
        assert_ne!(w1, dir.join("last_16k.wav"));
    }

    #[test]
    fn stt_settings_pick_segment_profile_per_engine() {
        let mut stt = SttSettings::from_input(&test_input());
        stt.model_id = "parakeet-v3".to_owned();
        stt.audio_backend = "local".to_owned();
        assert_eq!(stt.segment_cfg().hard_secs, SegmentCfg::PARAKEET.hard_secs);
        stt.audio_backend = "openai_compat".to_owned();
        assert_eq!(
            stt.segment_cfg().hard_secs,
            SegmentCfg::SLOW_ENGINE.hard_secs
        );
        stt.audio_backend = "local".to_owned();
        stt.model_id = "base.en".to_owned();
        assert_eq!(
            stt.segment_cfg().hard_secs,
            SegmentCfg::SLOW_ENGINE.hard_secs
        );
    }

    fn stub_stt(texts: Vec<Result<&'static str, &'static str>>) -> SttFn {
        std::sync::Arc::new(move |index, _samples| match texts.get(index) {
            Some(Ok(t)) => Ok((*t).to_owned()),
            Some(Err(e)) => Err((*e).to_owned()),
            None => Err("unexpected extra segment".to_owned()),
        })
    }

    fn finished_with(
        stt: SttFn,
        first: Vec<f32>,
        tail: Vec<f32>,
        kind: CutKind,
    ) -> FinishedSegments {
        let mut worker = SegmentWorker::spawn(stt);
        worker.enqueue(first, kind == CutKind::Forced);
        FinishedSegments {
            worker,
            tail_16k: tail,
            cuts: vec![crate::segmenter::Cut {
                index: 16_000,
                kind,
            }],
            source_label: "energy",
        }
    }

    #[test]
    fn run_segmented_joins_segments_and_postprocesses_once() {
        let input = test_input();
        let parakeet = Transcriber::spawn(input.data_dir.clone());
        let finished = finished_with(
            stub_stt(vec![Ok("save fifty"), Ok("percent on everything")]),
            tone(1.0),
            tone(1.0),
            CutKind::Pause,
        );
        let out = run_segmented(&input, &parakeet, finished, 2.0).unwrap();
        assert_eq!(out.raw_text, "save fifty percent on everything");
        assert!(out.text.contains("50%"), "got {:?}", out.text);
    }

    #[test]
    fn run_segmented_skips_silent_tail() {
        let input = test_input();
        let parakeet = Transcriber::spawn(input.data_dir.clone());
        // Only one STT result exists; a queued tail would hit the stub's
        // "unexpected extra segment" error and force a full-pass fallback.
        let finished = finished_with(
            stub_stt(vec![Ok("hello world")]),
            tone(1.0),
            vec![0.0; 8_000],
            CutKind::Pause,
        );
        let out = run_segmented(&input, &parakeet, finished, 1.5).unwrap();
        assert_eq!(out.raw_text, "hello world");
    }

    #[test]
    fn run_segmented_decodes_a_short_audible_tail() {
        let input = test_input();
        let parakeet = Transcriber::spawn(input.data_dir.clone());
        // 0.1 s is under the 0.25 s blank-audio floor, but it can be a last
        // word, so it must still reach the engine.
        let finished = finished_with(
            stub_stt(vec![Ok("see you"), Ok("tomorrow")]),
            tone(1.0),
            tone(0.1),
            CutKind::Pause,
        );
        let out = run_segmented(&input, &parakeet, finished, 1.1).unwrap();
        assert_eq!(out.raw_text, "see you tomorrow");
    }

    #[test]
    fn run_segmented_all_silence_is_an_error() {
        let input = test_input();
        let parakeet = Transcriber::spawn(input.data_dir.clone());
        let finished = finished_with(
            stub_stt(vec![Err("transcription was empty")]),
            tone(1.0),
            vec![0.0; 8_000],
            CutKind::Pause,
        );
        let err = run_segmented(&input, &parakeet, finished, 1.5)
            .err()
            .expect("all-silence take must fail");
        assert!(err.to_string().to_lowercase().contains("silence"), "{err}");
    }

    #[test]
    fn run_segmented_forced_cut_drops_only_trailing_period() {
        let input = test_input();
        let parakeet = Transcriber::spawn(input.data_dir.clone());
        let finished = finished_with(
            stub_stt(vec![Ok("we went to the store."), Ok("and bought milk")]),
            tone(1.0),
            tone(1.0),
            CutKind::Forced,
        );
        let out = run_segmented(&input, &parakeet, finished, 2.0).unwrap();
        assert_eq!(out.raw_text, "we went to the store and bought milk");
    }

    // ---- synthetic end to end (group E) ----
    //
    // Each "word" is a tone at its own frequency. The stub recognizer finds the
    // words in whatever audio a segment holds and names them, so a word cut in
    // half, dropped, or duplicated by the segmenter shows up as a wrong
    // transcript.

    use crate::segment_jobs::SegmentSession;
    use crate::segmenter::{EnergyPauseSource, PauseSource};

    const WORDS: [&str; 8] = [
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
    ];

    fn word_freq(k: usize) -> f32 {
        300.0 + 100.0 * k as f32
    }

    /// Never two equal neighbours (the cleanup pass would merge those).
    fn word_at(i: usize) -> usize {
        (i * 3 + i / 8) % 8
    }

    struct Lcg(u64);
    impl Lcg {
        fn noise(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 0.004
        }
    }

    /// `count` words, 0.4 s each, 0.15 s apart; after every `phrase`-th word a
    /// `pause_secs` pause (0 = never). Pauses carry faint noise, not silence.
    fn speak(rate: u32, count: usize, phrase: usize, pause_secs: f32) -> (Vec<f32>, String) {
        let mut rng = Lcg(7);
        let mut out = Vec::new();
        let mut expected = Vec::new();
        let gap = |secs: f32, out: &mut Vec<f32>, rng: &mut Lcg| {
            for _ in 0..(rate as f32 * secs) as usize {
                out.push(rng.noise());
            }
        };
        gap(0.3, &mut out, &mut rng);
        for i in 0..count {
            let k = word_at(i);
            expected.push(WORDS[k]);
            let n = (rate as f32 * 0.4) as usize;
            for j in 0..n {
                let fade = (j.min(n - j) as f32 / (rate as f32 * 0.005)).min(1.0);
                let t = j as f32 / rate as f32;
                out.push((2.0 * std::f32::consts::PI * word_freq(k) * t).sin() * 0.3 * fade);
            }
            let long = phrase > 0 && (i + 1) % phrase == 0;
            gap(if long { pause_secs } else { 0.15 }, &mut out, &mut rng);
        }
        gap(0.3, &mut out, &mut rng);
        (out, expected.join(" "))
    }

    fn goertzel(x: &[f32], freq: f32, rate: f32) -> f32 {
        let w = 2.0 * std::f32::consts::PI * freq / rate;
        let c = 2.0 * w.cos();
        let (mut s1, mut s2) = (0.0f32, 0.0f32);
        for v in x {
            let s0 = v + c * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        s1 * s1 + s2 * s2 - c * s1 * s2
    }

    /// Name the words in a 16 kHz segment, in order.
    fn recognise_words(samples: &[f32]) -> String {
        let mut words: Vec<&str> = Vec::new();
        let mut current: Option<usize> = None;
        for win in samples.chunks(800) {
            let rms = (win.iter().map(|v| v * v).sum::<f32>() / win.len() as f32).sqrt();
            if rms < 0.03 {
                current = None;
                continue;
            }
            let best = (0..8)
                .max_by(|a, b| {
                    goertzel(win, word_freq(*a), 16_000.0)
                        .total_cmp(&goertzel(win, word_freq(*b), 16_000.0))
                })
                .unwrap();
            if current != Some(best) {
                words.push(WORDS[best]);
                current = Some(best);
            }
        }
        words.join(" ")
    }

    struct LiveRun {
        raw_text: String,
        cut_kinds: Vec<CutKind>,
        longest_piece_secs: f64,
    }

    /// Feed `signal` in `chunk`-sample pieces like the live capture does, then
    /// stop and finish exactly as `stop_transcribe` does.
    fn run_live(signal: &[f32], rate: u32, chunk: usize, cfg: SegmentCfg) -> LiveRun {
        let stt: SttFn = std::sync::Arc::new(|_, samples| Ok(recognise_words(&samples)));
        let source: Box<dyn PauseSource> = Box::new(EnergyPauseSource::new());
        let mut session = SegmentSession::new(rate, cfg, source, "energy", stt);
        let mut all: Vec<f32> = Vec::new();
        for piece in signal.chunks(chunk) {
            all.extend_from_slice(piece);
            session.poll(&all);
        }
        let finished = session.finish(&all).expect("a long take must be cut");
        let mut bounds = vec![0usize];
        bounds.extend(finished.cuts.iter().map(|c| c.index));
        bounds.push(all.len());
        let longest = bounds
            .windows(2)
            .map(|w| (w[1] - w[0]) as f64 / rate as f64)
            .fold(0.0, f64::max);
        let cut_kinds: Vec<CutKind> = finished.cuts.iter().map(|c| c.kind).collect();
        let input = test_input();
        let parakeet = Transcriber::spawn(input.data_dir.clone());
        let out = run_segmented(&input, &parakeet, finished, all.len() as f64 / rate as f64)
            .expect("segmented run");
        LiveRun {
            raw_text: out.raw_text,
            cut_kinds,
            longest_piece_secs: longest,
        }
    }

    #[test]
    fn recogniser_finds_every_word_in_one_piece() {
        let (sig, expected) = speak(16_000, 40, 0, 0.0);
        assert_eq!(recognise_words(&sig), expected);
    }

    #[test]
    fn two_minute_dictation_with_phrase_pauses_loses_nothing() {
        let (sig, expected) = speak(16_000, 210, 12, 0.9);
        assert!(sig.len() as f64 / 16_000.0 > 110.0);
        let run = run_live(&sig, 16_000, 1920, SegmentCfg::PARAKEET);
        assert_eq!(run.raw_text, expected);
        assert!(run.cut_kinds.len() >= 3, "{} cuts", run.cut_kinds.len());
        assert!(
            run.cut_kinds.iter().all(|k| *k == CutKind::Pause),
            "phrase pauses are long enough that no forced cut is needed"
        );
        assert!(run.longest_piece_secs <= SegmentCfg::PARAKEET.hard_secs + 0.5);
    }

    #[test]
    fn same_dictation_at_48k_and_odd_chunking_loses_nothing() {
        let (sig, expected) = speak(48_000, 150, 12, 0.9);
        let run = run_live(&sig, 48_000, 7777, SegmentCfg::PARAKEET);
        assert_eq!(run.raw_text, expected);
        assert!(run.cut_kinds.len() >= 2);
    }

    #[test]
    fn continuous_speech_is_force_cut_inside_gaps_and_loses_nothing() {
        // Only 150 ms word gaps, never a real pause: the hard cap has to cut.
        let (sig, expected) = speak(16_000, 140, 0, 0.0);
        assert!(sig.len() as f64 / 16_000.0 > 70.0);
        let run = run_live(&sig, 16_000, 1920, SegmentCfg::PARAKEET);
        assert_eq!(run.raw_text, expected, "a forced cut must not split a word");
        assert!(run.cut_kinds.contains(&CutKind::Forced));
        assert!(run.longest_piece_secs <= SegmentCfg::PARAKEET.hard_secs + 0.5);
    }

    #[test]
    fn slow_engine_profile_uses_longer_pieces() {
        let (sig, expected) = speak(16_000, 200, 12, 0.9);
        let fast = run_live(&sig, 16_000, 1920, SegmentCfg::PARAKEET);
        let slow = run_live(&sig, 16_000, 1920, SegmentCfg::SLOW_ENGINE);
        assert_eq!(fast.raw_text, expected);
        assert_eq!(slow.raw_text, expected);
        assert!(slow.cut_kinds.len() < fast.cut_kinds.len());
        assert!(slow.longest_piece_secs <= SegmentCfg::SLOW_ENGINE.hard_secs + 0.5);
    }

    // ---- real model smoke test (group F, ignored) ----
    //
    //   DICTFLOW_TEST_WAV=/path/to/long.wav DICTFLOW_TEST_DATA=/path/to/data \
    //     cargo test --locked real_model_segmented -- --ignored --nocapture
    //
    // DICTFLOW_TEST_DATA is a DictFlow data dir with the default model
    // downloaded. The test decodes the file whole and in segments and checks
    // the two transcripts agree.

    fn word_error_rate(reference: &str, hypothesis: &str) -> f64 {
        let norm = |s: &str| -> Vec<String> {
            s.split_whitespace()
                .map(|w| {
                    w.trim_matches(|c: char| !c.is_alphanumeric())
                        .to_lowercase()
                })
                // Short clips keep the fillers a long decode drops; cleanup
                // removes them later, so they are not content.
                .filter(|w| {
                    !w.is_empty() && !["uh", "um", "uhm", "hmm", "mm", "er", "ah"].contains(&w.as_str())
                })
                .collect()
        };
        let (r, h) = (norm(reference), norm(hypothesis));
        let mut prev: Vec<usize> = (0..=h.len()).collect();
        for i in 1..=r.len() {
            let mut cur = vec![i];
            for j in 1..=h.len() {
                let sub = prev[j - 1] + usize::from(r[i - 1] != h[j - 1]);
                cur.push(sub.min(prev[j] + 1).min(cur[j - 1] + 1));
            }
            prev = cur;
        }
        prev[h.len()] as f64 / r.len().max(1) as f64
    }

    #[test]
    fn word_error_rate_helper_is_sane() {
        assert_eq!(word_error_rate("a b c", "a b c"), 0.0);
        assert_eq!(word_error_rate("Hello, World.", "hello world"), 0.0);
        assert!((word_error_rate("a b c d", "a b x d") - 0.25).abs() < 1e-9);
        assert_eq!(word_error_rate("a b", ""), 1.0);
    }

    #[test]
    #[ignore = "needs DICTFLOW_TEST_WAV, DICTFLOW_TEST_DATA and a downloaded model"]
    fn real_model_segmented_matches_whole_file() {
        let (Ok(wav), Ok(data)) = (
            std::env::var("DICTFLOW_TEST_WAV"),
            std::env::var("DICTFLOW_TEST_DATA"),
        ) else {
            eprintln!("DICTFLOW_TEST_WAV / DICTFLOW_TEST_DATA not set; skipping");
            return;
        };
        let data = PathBuf::from(data);
        let mut input = test_input();
        input.wav_path = PathBuf::from(wav);
        input.data_dir = data.clone();
        input.model_id = crate::models::default_model_id();
        let parakeet = Transcriber::spawn(data.clone());
        let model = ["resources/silero_vad.onnx"]
            .iter()
            .map(|p| Path::new(env!("CARGO_MANIFEST_DIR")).join(p))
            .find(|p| p.exists());

        let started = std::time::Instant::now();
        let whole = run_transcription(&input, &parakeet).expect("whole-file decode");
        let whole_ms = started.elapsed().as_millis();
        let cfg = SttSettings::from_input(&input).segment_cfg();
        let started = std::time::Instant::now();
        let split = run_file_transcription(&input, &parakeet, Some((cfg, model)))
            .expect("segmented decode");
        let split_ms = started.elapsed().as_millis();

        let wer = word_error_rate(&whole.raw_text, &split.raw_text);
        eprintln!("whole {whole_ms} ms, segmented {split_ms} ms, word difference {wer:.3}");
        eprintln!(
            "words: whole {}, segmented {}",
            whole.raw_text.split_whitespace().count(),
            split.raw_text.split_whitespace().count()
        );
        if let Ok(dir) = std::env::var("DICTFLOW_TEST_DUMP") {
            let dir = PathBuf::from(dir);
            let _ = std::fs::write(dir.join("whole.txt"), &whole.raw_text);
            let _ = std::fs::write(dir.join("segmented.txt"), &split.raw_text);
        }
        assert!(wer < 0.05, "segmented transcript drifted: {wer:.3}");
    }
}
