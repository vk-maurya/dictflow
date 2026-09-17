//! Local STT engines: in-process Parakeet (sherpa-onnx) and whisper-cli sidecar.
//!
//! The Parakeet recognizer is `!Send`, so it lives on one dedicated thread
//! for the life of the app. Whisper is a subprocess (`whisper-cli` /
//! `whisper-cli.exe`).

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use anyhow::Context;

use crate::format;
use crate::models::{self, EngineKind};
use crate::polish;
use crate::vad;
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
    pub(crate) fn transcribe(&self, model_id: &str, samples_16k: Vec<f32>) -> Result<String, String> {
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
        let entry = models::find(model_id)
            .ok_or_else(|| format!("unknown model: {model_id}"))?;
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
}

pub(crate) fn polish_cfg(settings: &Settings, data_dir: &Path) -> PolishCfg {
    let secrets = polish::load_secrets(data_dir);
    let audio_base = if settings.audio_api_base.trim().is_empty() {
        polish::default_base("openai_compat").to_owned()
    } else {
        settings.audio_api_base.clone()
    };
    let llm_base = if settings.llm_api_base.trim().is_empty() {
        polish::default_base(&settings.llm_backend).to_owned()
    } else {
        settings.llm_api_base.clone()
    };
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
    }
}

fn transcribe_whisper(
    data_dir: &Path,
    model_id: &str,
    language: &str,
    translate: bool,
    wav_16k_path: &Path,
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
    let out_prefix = data_dir.join("last_transcription");
    let mut cmd = std::process::Command::new(&binary);
    cmd.arg("-m")
        .arg(&model_path)
        .arg("-f")
        .arg(wav_16k_path)
        .arg("-otxt")
        .arg("-of")
        .arg(&out_prefix)
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
}

/// Heavy work: runs inside `spawn_blocking` so the async runtime stays free.
pub(crate) fn run_transcription(
    input: &TranscribeInput,
    parakeet: &Transcriber,
) -> anyhow::Result<TranscriptResult> {
    let samples = text::load_wav_mono_16k(&input.wav_path)?;
    // Original length drives History duration/playback; STT gets the trimmed
    // clip below. `input.wav_path` itself is never modified.
    let duration_secs = samples.len() as f64 / 16_000.0;
    // Silero speech gate + edge trim. Falls back to the peak gate while the
    // model file is missing, and keeps the existing blank-audio copy either
    // way. Runs here on the finished buffer — never the capture thread.
    vad::ensure_model(&input.data_dir);
    let stt_samples = match vad::gate_and_trim(&samples, 16_000, &input.data_dir) {
        Ok(trimmed) => trimmed,
        Err(why) => anyhow::bail!("{}", why.message()),
    };
    let raw = if input.polish.audio_backend == "openai_compat" {
        let wav = vad::encode_wav_bytes_16k(&stt_samples)?;
        polish::transcribe_openai(
            &input.polish.audio_api_base,
            &input.polish.audio_api_key,
            &input.polish.audio_api_model,
            input.polish.llm_timeout_ms.max(15_000),
            &wav,
        )?
    } else {
    let entry = models::find(&input.model_id).context("unknown model")?;
    match entry.engine {
        EngineKind::Whisper => {
            let tmp = input.data_dir.join("last_16k.wav");
            text::write_wav_mono_16k(&tmp, &stt_samples, 16_000)?;
            transcribe_whisper(
                &input.data_dir,
                &input.model_id,
                &input.language,
                input.translate,
                &tmp,
            )?
        }
        EngineKind::Parakeet => parakeet
            .transcribe(&input.model_id, stt_samples)
            .map_err(anyhow::Error::msg)?,
    }
    };
    let raw_text = raw.trim().to_owned();

    // P2: spoken commands → backtrack → lists, then dictionary + cleanup.
    let formatted = format::apply_offline_format(&raw_text);
    let mut dictionary = input.dictionary.clone();
    let (mut out, dict_hits) = text::apply_dictionary(&formatted, &mut dictionary);
    if dict_hits > 0 {
        text::sort_dictionary(&mut dictionary);
        let _ = text::save_dictionary(&input.data_dir, &dictionary);
    }
    match input.cleanup.as_str() {
        "light" | "medium" => out = text::remove_fillers(&out),
        "full" | "high" => out = text::tidy_punctuation(&text::remove_fillers(&out)),
        _ => {}
    }
    out = text::smart_trailing_punctuation(&out);
    let want_llm = input.polish.llm_enabled
        || input.cleanup == "medium"
        || input.cleanup == "high";
    if want_llm && input.polish.llm_backend != "off" {
        let preset = match input.cleanup.as_str() {
            "high" => "professional",
            "medium" => "clean",
            _ => input.polish.llm_preset.as_str(),
        };
        if let Some(polished) = polish::polish(&polish::PolishArgs {
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
            out = polished;
        }
    }
    let out = out.trim().to_owned();
    if out.is_empty() {
        anyhow::bail!("transcription was empty");
    }
    Ok(TranscriptResult {
        text: out,
        raw_text,
        duration_secs,
        dict_hits,
    })
}

