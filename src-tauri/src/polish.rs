//! Optional LLM rewrite after rule cleanup (P3). Fail-open: errors return None.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

pub const PRESETS: &[&str] = &[
    "clean",
    "professional",
    "casual",
    "message",
    "bullets",
    "custom",
];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Secrets {
    #[serde(default)]
    pub audio_api_key: String,
    #[serde(default)]
    pub llm_api_key: String,
}

pub const CRED_AUDIO: &str = "DictFlow/audio_api_key";
pub const CRED_LLM: &str = "DictFlow/llm_api_key";

pub fn secrets_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("secrets.json")
}

pub fn load_secrets(data_dir: &Path) -> Secrets {
    migrate_file_secrets(data_dir);
    Secrets {
        audio_api_key: wincred::read(CRED_AUDIO).unwrap_or_default(),
        llm_api_key: wincred::read(CRED_LLM).unwrap_or_default(),
    }
}

pub fn save_secrets(data_dir: &Path, secrets: &Secrets) -> Result<()> {
    if !secrets.audio_api_key.trim().is_empty() {
        wincred::write(CRED_AUDIO, secrets.audio_api_key.trim())?;
    }
    if !secrets.llm_api_key.trim().is_empty() {
        wincred::write(CRED_LLM, secrets.llm_api_key.trim())?;
    }
    let _ = std::fs::remove_file(secrets_path(data_dir));
    Ok(())
}

/// One-shot: copy leftover `secrets.json` into Credential Manager, then delete the file.
fn migrate_file_secrets(data_dir: &Path) {
    let path = secrets_path(data_dir);
    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };
    if let Ok(file) = serde_json::from_slice::<Secrets>(&bytes) {
        if wincred::read(CRED_AUDIO).unwrap_or_default().is_empty() && !file.audio_api_key.trim().is_empty()
        {
            let _ = wincred::write(CRED_AUDIO, file.audio_api_key.trim());
        }
        if wincred::read(CRED_LLM).unwrap_or_default().is_empty() && !file.llm_api_key.trim().is_empty() {
            let _ = wincred::write(CRED_LLM, file.llm_api_key.trim());
        }
    }
    let _ = std::fs::remove_file(&path);
}

/// Generic credentials in Windows Credential Manager (`advapi32` CredWrite / CredRead).
mod wincred {
    use anyhow::{bail, Result};
    use std::ffi::c_void;

    const CRED_TYPE_GENERIC: u32 = 1;
    const CRED_PERSIST_LOCAL_MACHINE: u32 = 2;

    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    #[repr(C)]
    struct CredentialW {
        flags: u32,
        type_: u32,
        target_name: *const u16,
        comment: *const u16,
        last_written: FileTime,
        credential_blob_size: u32,
        credential_blob: *const u8,
        persist: u32,
        attribute_count: u32,
        attributes: *const c_void,
        target_alias: *const u16,
        user_name: *const u16,
    }

    #[link(name = "advapi32")]
    extern "system" {
        fn CredWriteW(credential: *const CredentialW, flags: u32) -> i32;
        fn CredReadW(
            target_name: *const u16,
            type_: u32,
            flags: u32,
            credential: *mut *mut CredentialW,
        ) -> i32;
        fn CredFree(buffer: *mut c_void);
        fn CredDeleteW(target_name: *const u16, type_: u32, flags: u32) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn write(target: &str, secret: &str) -> Result<()> {
        let target_w = wide(target);
        let user_w = wide("DictFlow");
        let blob = secret.as_bytes();
        let cred = CredentialW {
            flags: 0,
            type_: CRED_TYPE_GENERIC,
            target_name: target_w.as_ptr(),
            comment: std::ptr::null(),
            last_written: FileTime { low: 0, high: 0 },
            credential_blob_size: blob.len() as u32,
            credential_blob: blob.as_ptr(),
            persist: CRED_PERSIST_LOCAL_MACHINE,
            attribute_count: 0,
            attributes: std::ptr::null(),
            target_alias: std::ptr::null(),
            user_name: user_w.as_ptr(),
        };
        // SAFETY: target/user/blob pointers stay alive for the CredWriteW call.
        let ok = unsafe { CredWriteW(&cred, 0) };
        if ok == 0 {
            bail!("Windows Credential Manager rejected the key");
        }
        Ok(())
    }

    pub fn read(target: &str) -> Option<String> {
        let target_w = wide(target);
        let mut ptr: *mut CredentialW = std::ptr::null_mut();
        // SAFETY: CredReadW allocates; we copy the blob then CredFree.
        let ok = unsafe { CredReadW(target_w.as_ptr(), CRED_TYPE_GENERIC, 0, &mut ptr) };
        if ok == 0 || ptr.is_null() {
            return None;
        }
        let secret = unsafe {
            let size = (*ptr).credential_blob_size as usize;
            let p = (*ptr).credential_blob;
            let bytes = if p.is_null() || size == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(p, size).to_vec()
            };
            CredFree(ptr.cast());
            String::from_utf8(bytes).ok()
        };
        secret.filter(|s| !s.is_empty())
    }

    #[allow(dead_code)]
    pub fn delete(target: &str) -> bool {
        let target_w = wide(target);
        unsafe { CredDeleteW(target_w.as_ptr(), CRED_TYPE_GENERIC, 0) != 0 }
    }
}

pub fn mask_key(key: &str) -> String {
    let t = key.trim();
    if t.len() <= 8 {
        return if t.is_empty() { String::new() } else { "••••".into() };
    }
    format!("{}…{}", &t[..4], &t[t.len() - 4..])
}

pub fn join_url(base: &str, path: &str) -> String {
    let base = base.trim().trim_end_matches('/');
    let path = path.trim().trim_start_matches('/');
    if base.is_empty() {
        return format!("/{path}");
    }
    // Users often paste the full Groq/OpenAI endpoint from the docs
    // (`…/v1/audio/transcriptions`). Do not append the path twice.
    if base.ends_with(path) || base.ends_with(&format!("/{path}")) {
        return base.to_owned();
    }
    format!("{base}/{path}")
}

pub fn default_base(backend: &str) -> &'static str {
    match backend {
        "local" => "http://127.0.0.1:11434/v1",
        "anthropic" => "https://api.anthropic.com",
        "openai_compat" => "https://api.openai.com/v1",
        _ => "",
    }
}

/// Default system prompt used to process a transcript (Clean preset / Reset).
/// Body is imported from `crate::prompts`.
pub fn default_prompt() -> &'static str {
    crate::prompts::text("clean")
}

pub fn preset_prompt(preset: &str) -> &'static str {
    crate::prompts::text(preset)
}

pub fn prompt_catalog() -> std::collections::HashMap<String, String> {
    ["clean", "professional", "casual", "message", "bullets"]
        .into_iter()
        .map(|k| (k.to_owned(), preset_prompt(k).to_owned()))
        .collect()
}

pub fn system_prompt(preset: &str, custom: &str) -> String {
    if preset == "custom" && !custom.trim().is_empty() {
        custom.trim().to_owned()
    } else {
        preset_prompt(preset).to_owned()
    }
}

pub fn openai_chat_body(model: &str, system: &str, user: &str, temperature: f32) -> Value {
    json!({
        "model": model,
        "temperature": temperature,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user}
        ]
    })
}

pub fn anthropic_body(model: &str, system: &str, user: &str, temperature: f32) -> Value {
    json!({
        "model": model,
        "max_tokens": 1024,
        "temperature": temperature,
        "system": system,
        "messages": [{"role": "user", "content": user}]
    })
}

fn extract_openai_text(v: &Value) -> Option<String> {
    v["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn extract_anthropic_text(v: &Value) -> Option<String> {
    v["content"]
        .as_array()?
        .iter()
        .find_map(|b| b["text"].as_str())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn client(timeout_ms: u64) -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(timeout_ms.max(1000)))
        .build()
        .context("http client")
}

pub struct PolishArgs<'a> {
    pub backend: &'a str,
    pub base_url: &'a str,
    pub api_key: &'a str,
    pub model: &'a str,
    pub preset: &'a str,
    pub custom_prompt: &'a str,
    pub temperature: f32,
    pub timeout_ms: u64,
    pub text: &'a str,
}

/// Returns polished text, or None so the caller pastes the raw/rules text.
pub fn polish(args: &PolishArgs<'_>) -> Option<String> {
    if args.backend == "off" || args.text.trim().is_empty() {
        return None;
    }
    let system = system_prompt(args.preset, args.custom_prompt);
    let result = match args.backend {
        "anthropic" => polish_anthropic(
            args.base_url,
            args.api_key,
            args.model,
            args.timeout_ms,
            args.temperature,
            &system,
            args.text,
        ),
        _ => polish_openai(
            args.base_url,
            args.api_key,
            args.model,
            args.timeout_ms,
            args.temperature,
            &system,
            args.text,
        ),
    };
    match result {
        Ok(s) if !s.trim().is_empty() => Some(s),
        Ok(_) => None,
        Err(e) => {
            log::warn!("polish skipped: {e:#}");
            None
        }
    }
}

fn polish_openai(
    base: &str,
    key: &str,
    model: &str,
    timeout_ms: u64,
    temperature: f32,
    system: &str,
    user: &str,
) -> Result<String> {
    let url = join_url(base, "chat/completions");
    let mut req = client(timeout_ms)?.post(&url).json(&openai_chat_body(model, system, user, temperature));
    if !key.trim().is_empty() {
        req = req.bearer_auth(key.trim());
    }
    let v: Value = req.send().context("openai chat")?.error_for_status()?.json()?;
    extract_openai_text(&v).context("empty openai content")
}

fn polish_anthropic(
    base: &str,
    key: &str,
    model: &str,
    timeout_ms: u64,
    temperature: f32,
    system: &str,
    user: &str,
) -> Result<String> {
    let url = join_url(base, "v1/messages");
    let req = client(timeout_ms)?
        .post(&url)
        .header("x-api-key", key.trim())
        .header("anthropic-version", "2023-06-01")
        .json(&anthropic_body(model, system, user, temperature));
    let v: Value = req.send().context("anthropic")?.error_for_status()?.json()?;
    extract_anthropic_text(&v).context("empty anthropic content")
}

pub fn transcribe_openai(
    base: &str,
    key: &str,
    model: &str,
    timeout_ms: u64,
    wav: &[u8],
) -> Result<String> {
    let url = join_url(base, "audio/transcriptions");
    let part = reqwest::blocking::multipart::Part::bytes(wav.to_vec())
        .file_name("audio.wav")
        .mime_str("audio/wav")?;
    let form = reqwest::blocking::multipart::Form::new()
        .text("model", model.to_owned())
        .part("file", part);
    let mut req = client(timeout_ms)?.post(&url).multipart(form);
    if !key.trim().is_empty() {
        req = req.bearer_auth(key.trim());
    }
    let v: Value = req.send().context("audio api")?.error_for_status()?.json()?;
    v["text"]
        .as_str()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .context("empty audio transcript")
}

pub fn ping_llm(
    backend: &str,
    base_url: &str,
    api_key: &str,
    model: &str,
    timeout_ms: u64,
) -> Result<String> {
    polish(&PolishArgs {
        backend,
        base_url,
        api_key,
        model,
        preset: "clean",
        custom_prompt: "",
        temperature: 0.0,
        timeout_ms,
        text: "ping",
    })
    .context("LLM returned empty")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_base_and_path() {
        assert_eq!(
            join_url("http://127.0.0.1:11434/v1/", "/chat/completions"),
            "http://127.0.0.1:11434/v1/chat/completions"
        );
        assert_eq!(join_url("https://api.anthropic.com", "v1/messages"), "https://api.anthropic.com/v1/messages");
        assert_eq!(
            join_url("https://api.groq.com/openai/v1", "audio/transcriptions"),
            "https://api.groq.com/openai/v1/audio/transcriptions"
        );
        assert_eq!(
            join_url(
                "https://api.groq.com/openai/v1/audio/transcriptions",
                "audio/transcriptions"
            ),
            "https://api.groq.com/openai/v1/audio/transcriptions"
        );
    }

    #[test]
    fn masks_keys() {
        assert_eq!(mask_key(""), "");
        assert_eq!(mask_key("sk-abcdefghij"), "sk-a…ghij");
    }

    #[test]
    fn clean_preset_is_default() {
        assert_eq!(system_prompt("clean", ""), default_prompt());
        assert_eq!(system_prompt("custom", ""), default_prompt());
        assert_eq!(system_prompt("custom", "Only fix names"), "Only fix names");
        assert!(prompt_catalog()["clean"].contains("transcript editor"));
        assert!(prompt_catalog()["clean"].contains("spelling"));
    }

    #[test]
    fn openai_body_has_two_messages() {
        let v = openai_chat_body("gpt-4o-mini", "sys", "hi", 0.2);
        assert_eq!(v["messages"].as_array().unwrap().len(), 2);
        assert_eq!(v["model"], "gpt-4o-mini");
    }

    #[test]
    fn extracts_chat_content() {
        let v = json!({"choices":[{"message":{"content":"  Hello  "}}]});
        assert_eq!(extract_openai_text(&v).as_deref(), Some("Hello"));
        let a = json!({"content":[{"type":"text","text":"Hi"}]});
        assert_eq!(extract_anthropic_text(&a).as_deref(), Some("Hi"));
    }

    #[test]
    fn polish_off_is_none() {
        assert!(polish(&PolishArgs {
            backend: "off",
            base_url: "",
            api_key: "",
            model: "",
            preset: "clean",
            custom_prompt: "",
            temperature: 0.2,
            timeout_ms: 1000,
            text: "hello",
        })
        .is_none());
    }

    #[test]
    fn credential_manager_roundtrip() {
        const TARGET: &str = "DictFlow/unit-test";
        let _ = wincred::delete(TARGET);
        wincred::write(TARGET, "sk-test-secret").expect("CredWrite");
        assert_eq!(wincred::read(TARGET).as_deref(), Some("sk-test-secret"));
        assert!(wincred::delete(TARGET));
        assert_eq!(wincred::read(TARGET), None);
    }
}
