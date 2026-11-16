//! Catalog and on-disk layout for local transcript-normalization models.

use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

pub const S1_ID: &str = "s1-mini";
pub const S1_NAME: &str = "S1-mini by Superwhisper";
pub const S1_FILE: &str = "s1-mini-q4_k_m.gguf";
pub const S1_BYTES: u64 = 484_219_808;
pub const S1_SHA256: &str = "3b41ebe2502cbd03e811d5d16b022f5ab551eda58d62597d152f89535003c634";
pub const S1_URL: &str =
    "https://huggingface.co/superwhisper/s1-mini-GGUF/resolve/cc7cf0d8443f3bdbb21d547f83a6a43e0c1b30b9/s1-mini-q4_k_m.gguf";
pub const S1_LICENSE_URL: &str =
    "https://huggingface.co/superwhisper/s1-mini-GGUF/resolve/34add00a48a2e5d24e5a4ee5405a99620a3a240c/LICENSE";
pub const S1_LICENSE_SHA256: &str =
    "f715982df6ce767ae64864d74b95644e5d658aab54717dbceeb252eb3cbcb421";
pub const S1_NOTICE_URL: &str =
    "https://huggingface.co/superwhisper/s1-mini-GGUF/resolve/34add00a48a2e5d24e5a4ee5405a99620a3a240c/NOTICE";
pub const S1_NOTICE_SHA256: &str =
    "6eb1e98a0d0046dd1b71da19d3b64ebccc12622e7d62e5efcad10298b4d66292";

pub const LLAMA_DIR_NAME: &str = "llama-b11575";

#[derive(Debug, Clone, Serialize)]
pub struct TextModelStatus {
    pub id: String,
    pub name: String,
    pub size_label: String,
    pub languages: String,
    pub downloaded: bool,
    pub runtime_ready: bool,
    pub downloading: bool,
    pub active: bool,
    pub attribution: String,
}

#[derive(Debug, Clone, Copy)]
pub struct RuntimeArchive {
    pub url: &'static str,
    pub sha256: &'static str,
    pub file_name: &'static str,
    pub expected_bytes: u64,
}

pub fn model_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("models").join(S1_ID)
}

pub fn model_path(data_dir: &Path) -> PathBuf {
    model_dir(data_dir).join(S1_FILE)
}

pub fn runtime_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("bin").join(LLAMA_DIR_NAME)
}

pub fn runtime_binary(data_dir: &Path) -> PathBuf {
    #[cfg(target_os = "windows")]
    let name = "llama-server.exe";
    #[cfg(not(target_os = "windows"))]
    let name = "llama-server";
    runtime_dir(data_dir).join(name)
}

pub fn runtime_ready(data_dir: &Path) -> bool {
    let Ok(spec) = runtime_archive() else {
        return false;
    };
    runtime_binary(data_dir).is_file()
        && std::fs::read_to_string(runtime_dir(data_dir).join(".archive-sha256"))
            .map(|value| value.trim() == spec.sha256)
            .unwrap_or(false)
}

pub fn runtime_archive() -> Result<RuntimeArchive, String> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return Ok(RuntimeArchive {
            url: "https://github.com/ggml-org/llama.cpp/releases/download/b11575/llama-b11575-bin-macos-arm64.tar.gz",
            sha256: "1c21e43d03ed73ffa63eb2e3428a0f98d866afca63f361c3e2b3cb622c3b5097",
            file_name: "llama-b11575-bin-macos-arm64.tar.gz",
            expected_bytes: 12_112_159,
        });
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        return Ok(RuntimeArchive {
            url: "https://github.com/ggml-org/llama.cpp/releases/download/b11575/llama-b11575-bin-macos-x64.tar.gz",
            sha256: "1054ae3e5e075be68867413039e941a10fdfc36b4c3fa55c9caf95b1b2bb9078",
            file_name: "llama-b11575-bin-macos-x64.tar.gz",
            expected_bytes: 11_607_623,
        });
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        return Ok(RuntimeArchive {
            url: "https://github.com/ggml-org/llama.cpp/releases/download/b11575/llama-b11575-bin-win-cpu-x64.zip",
            sha256: "8c0508c796e1fe5d1e6bcf1d659b55be834348d5fff08aaba1b6d068abef55e6",
            file_name: "llama-b11575-bin-win-cpu-x64.zip",
            expected_bytes: 19_509_236,
        });
    }
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    {
        return Ok(RuntimeArchive {
            url: "https://github.com/ggml-org/llama.cpp/releases/download/b11575/llama-b11575-bin-win-cpu-arm64.zip",
            sha256: "a8b0457866db7842f576013306f78d4a1619f2a4ea6ee5abae3c73a96847951e",
            file_name: "llama-b11575-bin-win-cpu-arm64.zip",
            expected_bytes: 12_380_032,
        });
    }
    #[allow(unreachable_code)]
    Err("S1-mini runtime is supported on macOS and Windows (arm64/x64)".to_owned())
}

pub fn status(data_dir: &Path, downloading: bool, active: bool) -> TextModelStatus {
    TextModelStatus {
        id: S1_ID.to_owned(),
        name: S1_NAME.to_owned(),
        size_label: "462 MB model + local runtime".to_owned(),
        languages: "English-only".to_owned(),
        downloaded: valid_model_size(data_dir),
        runtime_ready: runtime_ready(data_dir),
        downloading,
        active,
        attribution: "S1-mini by Superwhisper; Apache 2.0 plus naming clause".to_owned(),
    }
}

pub fn valid_model_size(data_dir: &Path) -> bool {
    std::fs::metadata(model_path(data_dir))
        .map(|m| m.is_file() && m.len() == S1_BYTES)
        .unwrap_or(false)
}

pub fn valid_model_checksum(data_dir: &Path) -> bool {
    let path = model_path(data_dir);
    if !valid_model_size(data_dir) {
        return false;
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    use std::io::Read as _;
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => hash.update(&buffer[..n]),
            Err(_) => return false,
        }
    }
    format!("{:x}", hash.finalize()) == S1_SHA256
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_layout_is_separate_from_audio_models() {
        let root = Path::new("/tmp/dictflow");
        assert_eq!(
            model_path(root),
            root.join("models").join("s1-mini").join(S1_FILE)
        );
        assert!(runtime_binary(root).starts_with(root.join("bin")));
    }

    #[test]
    fn pinned_model_metadata_is_complete() {
        assert_eq!(S1_SHA256.len(), 64);
        assert!(S1_URL.contains("cc7cf0d"));
        assert_eq!(S1_BYTES, 484_219_808);
    }
}
