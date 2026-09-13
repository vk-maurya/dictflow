//! Model catalog: every downloadable STT model DictFlow ships, for both engines.
//!
//! Mirrors SpeakType's `AIModel.availableModels` (names, speed/accuracy scores,
//! RAM guidance) but targets Windows runtimes: whisper.cpp `ggml` binaries and
//! sherpa-onnx int8 Parakeet transducer bundles.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    Whisper,
    Parakeet,
}

impl EngineKind {
    pub fn label(self) -> &'static str {
        match self {
            EngineKind::Whisper => "Whisper",
            EngineKind::Parakeet => "Parakeet",
        }
    }
}

/// One downloadable file belonging to a model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelFile {
    /// File name on disk (and last URL path segment).
    pub name: String,
    /// Full download URL.
    pub url: String,
    /// Approximate size in bytes (progress display only).
    pub expected_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    pub name: String,
    pub engine: EngineKind,
    /// e.g. "English-only" or "25 languages".
    pub languages: String,
    pub size_label: String,
    /// 0–10, higher is faster (mirrors SpeakType scoring).
    pub speed: f64,
    /// 0–10, higher is more accurate.
    pub accuracy: f64,
    /// One-line spotlight tag ("Best all-rounder", "Fastest", …).
    pub essence: String,
    pub min_ram_gb: u64,
    pub files: Vec<ModelFile>,
}

impl ModelEntry {
    /// Directory holding this model's files: `<data>/models/<id>/`.
    ///
    /// (Whisper ids match the ggml variant — `base.en` → `ggml-base.en.bin` —
    /// so installs from DictFlow v0.1 keep working.)
    pub fn dir(&self, data_dir: &Path) -> PathBuf {
        data_dir.join("models").join(&self.id)
    }

    pub fn is_downloaded(&self, data_dir: &Path) -> bool {
        let dir = self.dir(data_dir);
        self.files
            .iter()
            .all(|f| dir.join(&f.name).is_file())
    }

    /// Convenience for single-file (Whisper) models.
    pub fn single_path(&self, data_dir: &Path) -> Option<PathBuf> {
        if self.files.len() == 1 {
            Some(self.dir(data_dir).join(&self.files[0].name))
        } else {
            None
        }
    }
}

/// Shared speed/accuracy/essence metadata for catalog entries.
struct CatalogMeta<'a> {
    speed: f64,
    accuracy: f64,
    essence: &'a str,
}

fn whisper(
    id: &str,
    name: &str,
    size_label: &str,
    expected_mb: u64,
    english_only: bool,
    meta: CatalogMeta<'_>,
) -> ModelEntry {
    ModelEntry {
        id: id.to_owned(),
        name: name.to_owned(),
        engine: EngineKind::Whisper,
        languages: if english_only { "English-only".to_owned() } else { "Multilingual".to_owned() },
        size_label: size_label.to_owned(),
        speed: meta.speed,
        accuracy: meta.accuracy,
        essence: meta.essence.to_owned(),
        min_ram_gb: if expected_mb > 300 { 8 } else if expected_mb > 100 { 4 } else { 2 },
        files: vec![ModelFile {
            name: format!("ggml-{id}.bin"),
            url: format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{id}.bin"),
            expected_bytes: expected_mb * 1_000_000,
        }],
    }
}

fn parakeet_file(repo: &str, name: &str, expected_bytes: u64) -> ModelFile {
    ModelFile {
        name: name.to_owned(),
        url: format!("https://huggingface.co/{repo}/resolve/main/{name}"),
        expected_bytes,
    }
}

fn parakeet(
    id: &str,
    name: &str,
    repo: &str,
    languages: &str,
    size_label: &str,
    meta: CatalogMeta<'_>,
) -> ModelEntry {
    ModelEntry {
        id: id.to_owned(),
        name: name.to_owned(),
        engine: EngineKind::Parakeet,
        languages: languages.to_owned(),
        size_label: size_label.to_owned(),
        speed: meta.speed,
        accuracy: meta.accuracy,
        essence: meta.essence.to_owned(),
        min_ram_gb: 4,
        files: ["encoder.int8.onnx", "decoder.int8.onnx", "joiner.int8.onnx", "tokens.txt"]
            .into_iter()
            .map(|f| {
                let bytes = match f {
                    "encoder.int8.onnx" => 640_000_000,
                    "decoder.int8.onnx" => 18_000_000,
                    "joiner.int8.onnx" => 7_000_000,
                    _ => 4_000,
                };
                parakeet_file(repo, f, bytes)
            })
            .collect(),
    }
}

/// Full catalog, best default first.
pub fn catalog() -> Vec<ModelEntry> {
    let m = |speed: f64, accuracy: f64, essence: &'static str| CatalogMeta {
        speed,
        accuracy,
        essence,
    };
    vec![
        parakeet(
            "parakeet-v3",
            "Parakeet TDT 0.6B v3",
            "csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8",
            "25 languages",
            "~670 MB",
            m(9.7, 9.2, "Best all-rounder"),
        ),
        parakeet(
            "parakeet-v2",
            "Parakeet TDT 0.6B v2",
            "csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8",
            "English-only",
            "~660 MB",
            m(9.8, 9.1, "Fastest English"),
        ),
        whisper("small.en", "Whisper Small", "~470 MB", 466, true, m(8.0, 8.5, "Reliable")),
        whisper("base.en", "Whisper Base", "~145 MB", 142, true, m(9.0, 7.5, "Fast & light")),
        whisper("tiny.en", "Whisper Tiny", "~75 MB", 75, true, m(9.5, 6.0, "Featherweight")),
        whisper("small", "Whisper Small Multi", "~490 MB", 488, false, m(7.8, 8.5, "Multilingual")),
    ]
}

pub fn find(id: &str) -> Option<ModelEntry> {
    catalog().into_iter().find(|m| m.id == id)
}

/// Default model for fresh installs: in-process (no sidecar needed),
/// multilingual, best accuracy-per-MB in the catalog.
pub fn default_model_id() -> String {
    "parakeet-v3".to_owned()
}
