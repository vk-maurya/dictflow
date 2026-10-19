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

/// Result of the rules-only chain (`rules_pipeline`).
struct RulesOut {
    text: String,
    /// Vocabulary hits (entries whose `usage` was bumped) — drives the save.
    hits: u32,
    /// True when the transcript became an email layout: the LLM polish stage
    /// is skipped so the layout survives.
    is_email: bool,
}

/// Rules-only chain applied to the raw transcript, before the optional LLM
/// polish (docs/TEXT_PIPELINE_PLAN.md §3):
///   command fixes → spoken numbers → spoken commands → backtrack → lists →
///   email (short-circuit) | clean → vocabulary → tidy.
/// `cleanup == "off"` skips both `clean.rs` and the tidy pass.
fn rules_pipeline(
    raw_text: &str,
    language: &str,
    cleanup: &str,
    dictionary: &mut [text::DictionaryEntry],
) -> RulesOut {
    // Command fixes first: a dictionary entry whose replacement is a spoken
    // command phrase is expanded so the command pass can act on it. Hits are
    // discarded — command fixes never bump `usage`, so there is nothing to save.
    let (cmd_fixed, _) = text::apply_command_fixes(raw_text, dictionary);
    let with_numbers = numbers::apply_spoken_numbers(&cmd_fixed);
    let commands = format::apply_spoken_commands(&with_numbers);
    // `apply_backtrack` still owns the "scratch that / never mind / remove that"
    // clause delete; `clean.rs` covers the conversational repair cues.
    let commanded = format::apply_lists(&format::apply_backtrack(&commands));

    if language == "en" || language == "auto" {
        if let Some(mail) = email::compose_email(&commanded) {
            return RulesOut {
                text: mail,
                hits: 0,
                is_email: true,
            };
        }
    }

    let cleaned = match cleanup {
        "light" | "medium" | "full" | "high" => clean::clean(&commanded, language),
        _ => commanded,
    };
    let (vocab, hits) = text::apply_vocabulary(&cleaned, dictionary);
    let text = match cleanup {
        "full" | "high" => text::tidy_punctuation(&vocab),
        _ => vocab,
    };
    RulesOut {
        text,
        hits,
        is_email: false,
    }
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
    let raw = if input.polish.audio_backend == "openai_compat" {
        let wav = std::fs::read(&input.wav_path).context("read wav for audio API")?;
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
            text::write_wav_mono_16k(&tmp, &samples, 16_000)?;
            transcribe_whisper(
                &input.data_dir,
                &input.model_id,
                &input.language,
                input.translate,
                &tmp,
            )?
        }
        EngineKind::Parakeet => parakeet
            .transcribe(&input.model_id, samples)
            .map_err(anyhow::Error::msg)?,
    }
    };
    let raw_text = raw.trim().to_owned();

    // P0 text pipeline (docs/TEXT_PIPELINE_PLAN.md §3), rules-only (no LLM).
    // `raw_text` is deliberately left untouched for history.
    let mut dictionary = input.dictionary.clone();
    let rules = rules_pipeline(
        &raw_text,
        &input.language,
        &input.cleanup,
        &mut dictionary,
    );
    let mut out = rules.text;
    if rules.hits > 0 {
        text::sort_dictionary(&mut dictionary);
        let _ = text::save_dictionary(&input.data_dir, &dictionary);
    }
    out = text::smart_trailing_punctuation(&out);
    let want_llm = !rules.is_email
        && (input.polish.llm_enabled
            || input.cleanup == "medium"
            || input.cleanup == "high");
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
        dict_hits: rules.hits,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, from: &str, to: &str) -> text::DictionaryEntry {
        text::DictionaryEntry::new(id, from, to)
    }

    #[test]
    fn rules_numbers_before_commands() {
        let mut d = Vec::new();
        let out = rules_pipeline("issue hash forty two", "en", "light", &mut d);
        assert_eq!(out.text, "issue #42");
        assert_eq!(out.hits, 0);
        assert!(!out.is_email);
    }

    #[test]
    fn rules_command_fix_precedes_command_pass() {
        // A dictionary entry whose replacement is a spoken command is expanded
        // first, so the command engine turns it into punctuation.
        let mut d = vec![entry("1", "kocia mark", "question mark")];
        let out = rules_pipeline("are you there kocia mark", "en", "light", &mut d);
        assert_eq!(out.text, "are you there?");
        // Command fixes never bump usage.
        assert_eq!(d[0].usage, 0);
    }

    #[test]
    fn rules_cleanup_then_vocabulary() {
        let mut d = vec![entry("1", "my email", "a@b.com")];
        let out = rules_pipeline("contact my um email please", "en", "light", &mut d);
        assert_eq!(out.text, "contact a@b.com please");
        assert_eq!(out.hits, 1);
        assert_eq!(d[0].usage, 1);
    }

    #[test]
    fn rules_backtrack_cues_survive_cleanup() {
        let mut d = Vec::new();
        let out = rules_pipeline("hello world scratch that goodbye", "en", "light", &mut d);
        assert_eq!(out.text, "goodbye");
    }

    #[test]
    fn rules_email_short_circuits_and_is_language_gated() {
        let mut d = Vec::new();
        let out = rules_pipeline(
            "draft an email to Sarah telling her that I will be late tomorrow, cheers, Sarah",
            "en",
            "light",
            &mut d,
        );
        assert!(out.is_email);
        assert_eq!(
            out.text,
            "Hi Sarah,\n\nI will be late tomorrow.\n\nCheers,\nSarah"
        );

        // Non-English never builds an email layout.
        let mut d = Vec::new();
        let out = rules_pipeline(
            "write an email to Sarah saying hi, best regards, Sarah",
            "de",
            "light",
            &mut d,
        );
        assert!(!out.is_email);
    }

    #[test]
    fn rules_cleanup_off_leaves_text_alone() {
        let mut d = Vec::new();
        let out = rules_pipeline("um, hello", "en", "off", &mut d);
        assert_eq!(out.text, "um, hello");
    }

    #[test]
    fn rules_newlines_survive_light_and_full_cleanup() {
        for cleanup in ["light", "full"] {
            let mut d = Vec::new();
            let out = rules_pipeline(
                "first new line second new paragraph third",
                "en",
                cleanup,
                &mut d,
            );
            assert_eq!(out.text, "first\nsecond\n\nthird", "cleanup: {cleanup}");
        }
    }

    /// The examples published in `docs/P0_MANUAL_TEST.md`, run through the whole
    /// rules chain with the app default cleanup (`full`). Keeps the doc honest.
    #[test]
    fn documented_examples_match_pipeline() {
        let cases: &[(&str, &str)] = &[
            ("four point five stars", "4.5 stars"),
            ("one lakh twenty thousand", "1,20,000"),
            ("two hundred fifty dollars", "250 dollars"),
            ("hello comma world period", "hello, world."),
            ("are you there question mark", "are you there?"),
            ("done exclamation mark", "done!"),
            ("open parenthesis hello close parenthesis", "(hello)"),
            (
                "first new line second new paragraph third",
                "first\nsecond\n\nthird",
            ),
            ("the period of time", "the period of time"),
            ("send a comma", "send a comma"),
            ("ping at the rate KV", "ping @KV"),
            (
                "send it to name at the rate gmail dot com",
                "send it to name@gmail.com",
            ),
            ("issue hash forty two", "issue #42"),
            ("save fifty percent", "save 50%"),
            ("what percent of users", "what percent of users"),
            ("hash out a plan", "hash out a plan"),
            ("dollar twenty", "dollar twenty"),
            (
                "Let's ship Friday sorry remove that Monday",
                "Let's ship Monday",
            ),
            ("hello world scratch that goodbye", "goodbye"),
            ("the meeting is at 5 actually 3", "the meeting is at 3"),
            ("the color is blue I mean red", "the color is red"),
            ("Um, let's go", "Let's go"),
            ("well uh yeah", "well yeah"),
            ("It was, you know, fine", "It was, fine"),
            (
                "and then and then we shipped it",
                "and then we shipped it",
            ),
            (
                "Drinking more and more and more",
                "Drinking more and more and more",
            ),
            (
                "I live in Delhi no sorry I live in Ahmedabad",
                "I live in Ahmedabad",
            ),
            (
                "one buy milk two call mom three send email",
                "1. buy milk\n2. call mom\n3. send email",
            ),
            ("bullet milk bullet eggs", "- milk\n- eggs"),
            (
                "first wash dishes second take out trash",
                "1. wash dishes\n2. take out trash",
            ),
            (
                "write an email to Sarah saying I will be on holiday tomorrow, best regards, Kartik",
                "Hi Sarah,\n\nI will be on holiday tomorrow.\n\nBest regards,\nKartik",
            ),
        ];
        for (input, want) in cases {
            let mut d: Vec<text::DictionaryEntry> = Vec::new();
            let out = rules_pipeline(input, "en", "full", &mut d);
            assert_eq!(out.text, *want, "input: {input:?}");
        }

        // Dictionary entries (set up in Settings → Dictionary).
        let mut d = vec![entry("1", "my email", "a@b.com")];
        let out = rules_pipeline("contact my email please", "en", "full", &mut d);
        assert_eq!(out.text, "contact a@b.com please");

        // A dictionary entry whose replacement is a spoken command.
        let mut d = vec![entry("1", "kocia mark", "question mark")];
        let out = rules_pipeline("are you there kocia mark", "en", "full", &mut d);
        assert_eq!(out.text, "are you there?");

        // Cleanup off / non-English gate.
        let mut d = Vec::new();
        assert_eq!(
            rules_pipeline("um, hello", "en", "off", &mut d).text,
            "um, hello"
        );
        let mut d = Vec::new();
        assert_eq!(
            rules_pipeline("Um, hello world", "de", "full", &mut d).text,
            "Um, hello world"
        );
    }
}

