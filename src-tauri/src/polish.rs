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

pub const S1_SYSTEM_PROMPT: &str = "You are a text normalizer for speech-to-text transcripts. The input begins with a control line specifying the styling, structure, and context settings; clean the transcript to match those settings and output only the cleaned text.";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Secrets {
    #[serde(default)]
    pub audio_api_key: String,
    #[serde(default)]
    pub llm_api_key: String,
}

/// Keyring service name used on all platforms.
const KEYRING_SERVICE: &str = "DictFlow";
pub const CRED_AUDIO: &str = "audio_api_key";
pub const CRED_LLM: &str = "llm_api_key";

pub fn secrets_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("secrets.json")
}

/// Windows stores API keys in Credential Manager.
/// macOS used to use the login Keychain, which asks for the Mac password and
/// rejects it for this ad-hoc signed app. macOS now keeps the keys in
/// `secrets.json` inside the app data folder, readable only by this user.
#[cfg_attr(target_os = "macos", allow(dead_code))]
mod cred {
    use super::KEYRING_SERVICE;
    use anyhow::Result;

    pub fn read(key: &str) -> Option<String> {
        keyring::Entry::new(KEYRING_SERVICE, key)
            .ok()?
            .get_password()
            .ok()
            .filter(|s| !s.is_empty())
    }

    pub fn write(key: &str, secret: &str) -> Result<()> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, key)
            .map_err(|e| anyhow::anyhow!("keyring entry: {e}"))?;
        entry
            .set_password(secret)
            .map_err(|e| anyhow::anyhow!("keyring write: {e}"))
    }

    #[cfg(test)]
    pub fn delete(key: &str) -> bool {
        keyring::Entry::new(KEYRING_SERVICE, key)
            .ok()
            .and_then(|e| e.delete_credential().ok())
            .is_some()
    }
}

pub fn load_secrets(data_dir: &Path) -> Secrets {
    #[cfg(target_os = "macos")]
    {
        return load_file_secrets(data_dir);
    }
    #[cfg(not(target_os = "macos"))]
    {
        migrate_file_secrets(data_dir);
        Secrets {
            audio_api_key: cred::read(CRED_AUDIO).unwrap_or_default(),
            llm_api_key: cred::read(CRED_LLM).unwrap_or_default(),
        }
    }
}

pub fn save_secrets(data_dir: &Path, secrets: &Secrets) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let mut current = load_file_secrets(data_dir);
        if !secrets.audio_api_key.trim().is_empty() {
            current.audio_api_key = secrets.audio_api_key.trim().to_owned();
        }
        if !secrets.llm_api_key.trim().is_empty() {
            current.llm_api_key = secrets.llm_api_key.trim().to_owned();
        }
        return save_file_secrets(data_dir, &current);
    }
    #[cfg(not(target_os = "macos"))]
    {
        if !secrets.audio_api_key.trim().is_empty() {
            cred::write(CRED_AUDIO, secrets.audio_api_key.trim())?;
        }
        if !secrets.llm_api_key.trim().is_empty() {
            cred::write(CRED_LLM, secrets.llm_api_key.trim())?;
        }
        let _ = std::fs::remove_file(secrets_path(data_dir));
        Ok(())
    }
}

fn load_file_secrets(data_dir: &Path) -> Secrets {
    std::fs::read(secrets_path(data_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_file_secrets(data_dir: &Path, secrets: &Secrets) -> Result<()> {
    if let Some(parent) = secrets_path(data_dir).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let path = secrets_path(data_dir);
    std::fs::write(&path, serde_json::to_vec_pretty(secrets)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// One-shot: copy leftover `secrets.json` into Credential Manager, then delete the file.
#[cfg(not(target_os = "macos"))]
fn migrate_file_secrets(data_dir: &Path) {
    let path = secrets_path(data_dir);
    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };
    if let Ok(file) = serde_json::from_slice::<Secrets>(&bytes) {
        if cred::read(CRED_AUDIO).unwrap_or_default().is_empty()
            && !file.audio_api_key.trim().is_empty()
        {
            let _ = cred::write(CRED_AUDIO, file.audio_api_key.trim());
        }
        if cred::read(CRED_LLM).unwrap_or_default().is_empty()
            && !file.llm_api_key.trim().is_empty()
        {
            let _ = cred::write(CRED_LLM, file.llm_api_key.trim());
        }
    }
    let _ = std::fs::remove_file(&path);
}

pub fn mask_key(key: &str) -> String {
    let t = key.trim();
    if t.len() <= 8 {
        return if t.is_empty() {
            String::new()
        } else {
            "••••".into()
        };
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

pub const OPENROUTER_BASE: &str = "https://openrouter.ai/api/v1";
const OPENROUTER_FALLBACKS: &[&str] = &[
    "cohere/north-mini-code:free",
    "dots-studio/dots-3-note-preview:free",
];
const OPENROUTER_REFERER: &str = "https://github.com/vk-maurya/dictflow";
const OPENROUTER_TITLE: &str = "DictFlow";

pub fn default_base(backend: &str) -> &'static str {
    match backend {
        "local" => "http://127.0.0.1:11434/v1",
        "anthropic" => "https://api.anthropic.com",
        "openai_compat" => "https://api.openai.com/v1",
        "openrouter" => OPENROUTER_BASE,
        _ => "",
    }
}

pub fn is_openrouter(backend: &str, base: &str) -> bool {
    backend == "openrouter" || base.to_ascii_lowercase().contains("openrouter.ai")
}

/// Point an OpenRouter setup at the API root. A pasted `/chat/completions`
/// URL still works, and the caller adds OpenRouter's attribution headers.
pub fn canonical_llm_base(backend: &str, base: &str) -> String {
    if is_openrouter(backend, base) {
        return OPENROUTER_BASE.to_owned();
    }
    let trimmed = base.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return default_base(backend).to_owned();
    }
    trimmed.to_owned()
}

/// Rewrites a pasted OpenRouter URL onto the dedicated backend.
/// `None` means the stored values can stay as they are.
pub fn migrate_openrouter(backend: &str, base: &str) -> Option<(String, String)> {
    if !is_openrouter(backend, base) {
        return None;
    }
    let next_base = OPENROUTER_BASE.to_owned();
    if backend == "openrouter" && base.trim().trim_end_matches('/') == OPENROUTER_BASE {
        return None;
    }
    Some(("openrouter".to_owned(), next_base))
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
    ["clean", "professional", "casual", "message", "bullets", "refine"]
        .into_iter()
        .map(|k| (k.to_owned(), preset_prompt(k).to_owned()))
        .collect()
}

pub fn system_prompt(preset: &str, custom: &str) -> String {
    // Selection refine must not inherit a dictation custom prompt.
    if preset == "refine" {
        return preset_prompt("refine").to_owned();
    }
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

fn strip_s1_template_artifacts(text: &str) -> String {
    let trimmed = text.trim();
    for prefix in [
        "<think></think>",
        "<think>\n</think>",
        "<think>\n\n</think>",
    ] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return rest.trim().to_owned();
        }
    }
    trimmed.to_owned()
}

pub fn s1_control_line(styling: &str, structure: &str, context: &str) -> String {
    format!("[Styling: {styling}] [Structure: {structure}] [Context: {context}]")
}

/// Conservative chunks for the model card's recommended ~1,000-token input.
/// Sentence/newline boundaries are preferred; very long runs fall back to
/// Unicode-safe whitespace boundaries.
pub fn chunk_s1_input(text: &str) -> Vec<String> {
    const MAX_CHARS: usize = 3_200;
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    if text.chars().count() <= MAX_CHARS {
        return vec![text.to_owned()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let extra = usize::from(!current.is_empty()) + word.chars().count();
        if !current.is_empty() && current.chars().count() + extra > MAX_CHARS {
            chunks.push(current);
            current = String::new();
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
        if current.chars().count() >= MAX_CHARS.saturating_sub(200)
            && word.ends_with(['.', '!', '?', ';'])
        {
            chunks.push(current);
            current = String::new();
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
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
pub fn polish(args: &PolishArgs<'_>) -> Result<Option<String>> {
    if args.backend == "off" || args.text.trim().is_empty() {
        return Ok(None);
    }
    let system = system_prompt(args.preset, args.custom_prompt);
    let openrouter = is_openrouter(args.backend, args.base_url);
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
            openrouter,
        ),
    };
    match result {
        Ok(s) if !s.trim().is_empty() => Ok(Some(s)),
        Ok(_) => Ok(None),
        Err(e) => {
            log::warn!("polish skipped: {e:#}");
            Err(e)
        }
    }
}

fn brief_api_error(body: &str) -> String {
    let trimmed = body.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        if let Some(message) = value["error"]["message"]
            .as_str()
            .or_else(|| value["error"].as_str())
            .or_else(|| value["message"].as_str())
        {
            return message.chars().take(180).collect();
        }
    }
    if trimmed.is_empty() {
        "no error details".to_owned()
    } else {
        trimmed.chars().take(180).collect()
    }
}

fn read_json(resp: reqwest::blocking::Response, what: &str) -> Result<Value> {
    let status = resp.status();
    let body = resp
        .text()
        .with_context(|| format!("{what}: read body"))?;
    if !status.is_success() {
        anyhow::bail!("{what}: HTTP {status}: {}", brief_api_error(&body));
    }
    serde_json::from_str(&body).with_context(|| format!("{what}: invalid JSON"))
}

fn openrouter_model_chain(configured: &str) -> Vec<String> {
    let mut models = Vec::new();
    let configured = configured.trim();
    if !configured.is_empty() {
        models.push(configured.to_owned());
    }
    for model in OPENROUTER_FALLBACKS {
        if !models.iter().any(|existing| existing == model) {
            models.push((*model).to_owned());
        }
    }
    models
}

fn polish_openai(
    base: &str,
    key: &str,
    model: &str,
    timeout_ms: u64,
    temperature: f32,
    system: &str,
    user: &str,
    openrouter: bool,
) -> Result<String> {
    if openrouter && key.trim().is_empty() {
        anyhow::bail!("OpenRouter requires an API key from openrouter.ai/keys");
    }
    let models = if openrouter {
        openrouter_model_chain(model)
    } else {
        vec![model.to_owned()]
    };
    let mut last_error = None;
    for candidate in &models {
        match polish_openai_once(
            base,
            key,
            candidate,
            timeout_ms,
            temperature,
            system,
            user,
            openrouter,
        ) {
            Ok(text) if !text.trim().is_empty() => {
                if openrouter {
                    log::info!("openrouter cleanup used {candidate}");
                }
                return Ok(text);
            }
            Ok(_) => {
                log::warn!("openrouter model {candidate} returned empty text");
            }
            Err(error) => {
                log::warn!("openrouter model {candidate} failed: {error:#}");
                last_error = Some(error);
            }
        }
        if !openrouter {
            break;
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("cleanup models returned empty text")))
}

fn polish_openai_once(
    base: &str,
    key: &str,
    model: &str,
    timeout_ms: u64,
    temperature: f32,
    system: &str,
    user: &str,
    openrouter: bool,
) -> Result<String> {
    let url = join_url(base, "chat/completions");
    let attempt_timeout = if openrouter {
        timeout_ms.min(20_000).max(1_000)
    } else {
        timeout_ms
    };
    let mut body = openai_chat_body(model, system, user, temperature);
    if openrouter {
        body["max_tokens"] = json!(1500);
    }
    let mut req = client(attempt_timeout)?.post(&url).json(&body);
    if !key.trim().is_empty() {
        req = req.bearer_auth(key.trim());
    }
    if openrouter {
        req = req
            .header("HTTP-Referer", OPENROUTER_REFERER)
            .header("X-Title", OPENROUTER_TITLE);
    }
    let v = read_json(req.send().context("openai chat")?, "openai chat")?;
    extract_openai_text(&v).context("empty openai content")
}

pub fn polish_s1(
    base: &str,
    timeout_ms: u64,
    styling: &str,
    structure: &str,
    context: &str,
    text: &str,
) -> Result<String> {
    let chunks = chunk_s1_input(text);
    if chunks.is_empty() {
        anyhow::bail!("empty S1-mini input");
    }
    let url = join_url(base, "chat/completions");
    let client = client(timeout_ms)?;
    let control = s1_control_line(styling, structure, context);
    let mut outputs = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let user = format!("{control}\n{chunk}");
        let estimated_tokens = (user.chars().count() / 4).max(1);
        let max_tokens = ((estimated_tokens as f32 * 1.3) as usize + 32).clamp(64, 1536);
        let body = json!({
            "model": "s1-mini",
            "temperature": 0,
            "max_tokens": max_tokens,
            "messages": [
                {"role": "system", "content": S1_SYSTEM_PROMPT},
                {"role": "user", "content": user}
            ]
        });
        let value: Value = client
            .post(&url)
            .json(&body)
            .send()
            .context("S1-mini request")?
            .error_for_status()
            .context("S1-mini response")?
            .json()
            .context("S1-mini JSON")?;
        let output = value["choices"][0]["message"]["content"]
            .as_str()
            .context("S1-mini response did not contain text")?;
        let output = strip_s1_template_artifacts(output);
        if output.chars().count() > chunk.chars().count().saturating_mul(4).max(512) {
            anyhow::bail!("S1-mini returned unexpectedly large text");
        }
        outputs.push(output);
    }
    Ok(outputs.join(" ").trim().to_owned())
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
    let v = read_json(req.send().context("anthropic")?, "anthropic")?;
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
    let v: Value = req
        .send()
        .context("audio api")?
        .error_for_status()?
        .json()?;
    v["text"]
        .as_str()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .context("empty audio transcript")
}

#[allow(dead_code)]
pub fn ping_llm(
    backend: &str,
    base_url: &str,
    api_key: &str,
    model: &str,
    timeout_ms: u64,
) -> Result<String> {
    match polish(&PolishArgs {
        backend,
        base_url,
        api_key,
        model,
        preset: "clean",
        custom_prompt: "",
        temperature: 0.0,
        timeout_ms,
        text: "ping",
    })? {
        Some(text) => Ok(text),
        None => anyhow::bail!("LLM returned empty"),
    }
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
        assert_eq!(
            join_url("https://api.anthropic.com", "v1/messages"),
            "https://api.anthropic.com/v1/messages"
        );
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
        assert_eq!(
            canonical_llm_base(
                "openai_compat",
                "https://openrouter.ai/api/v1/chat/completions"
            ),
            OPENROUTER_BASE
        );
        assert_eq!(canonical_llm_base("openrouter", ""), OPENROUTER_BASE);
        assert_eq!(
            openrouter_model_chain("google/gemma-4-26b-a4b-it:free"),
            vec![
                "google/gemma-4-26b-a4b-it:free",
                "cohere/north-mini-code:free",
                "dots-studio/dots-3-note-preview:free",
            ]
        );
        assert_eq!(
            openrouter_model_chain("cohere/north-mini-code:free"),
            vec![
                "cohere/north-mini-code:free",
                "dots-studio/dots-3-note-preview:free",
            ]
        );
        assert_eq!(
            canonical_llm_base("local", ""),
            "http://127.0.0.1:11434/v1"
        );
    }

    #[test]
    fn openrouter_without_a_key_fails_before_the_request() {
        let err = polish(&PolishArgs {
            backend: "openrouter",
            base_url: OPENROUTER_BASE,
            api_key: "",
            model: "openai/gpt-4o-mini",
            preset: "clean",
            custom_prompt: "",
            temperature: 0.0,
            timeout_ms: 1000,
            text: "hello",
        })
        .unwrap_err();
        assert!(err.to_string().contains("API key"));
    }

    #[test]
    fn openrouter_request_uses_chat_completions_and_attribution_headers() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;
        use std::sync::mpsc;

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            let body_start;
            loop {
                let n = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..n]);
                if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    body_start = pos + 4;
                    break;
                }
            }
            let headers = String::from_utf8_lossy(&request[..body_start]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap();
            while request.len() < body_start + content_length {
                let n = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..n]);
            }
            tx.send(String::from_utf8_lossy(&request).into_owned())
                .unwrap();
            let body = r#"{"choices":[{"message":{"content":"Hello."}}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });

        let output = polish(&PolishArgs {
            backend: "openrouter",
            base_url: &format!("http://127.0.0.1:{port}/v1"),
            api_key: "test-key",
            model: "openai/gpt-4o-mini",
            preset: "clean",
            custom_prompt: "",
            temperature: 0.0,
            timeout_ms: 2_000,
            text: "um hello",
        })
        .unwrap()
        .unwrap();
        server.join().unwrap();
        let request = rx.recv().unwrap();
        assert_eq!(output, "Hello.");
        let headers = request.to_ascii_lowercase();
        assert!(request.starts_with("POST /v1/chat/completions "));
        assert!(headers.contains("authorization: bearer test-key"));
        assert!(headers.contains("http-referer: https://github.com/vk-maurya/dictflow"));
        assert!(headers.contains("x-title: dictflow"));
        assert!(request.contains(r#""model":"openai/gpt-4o-mini""#));
        assert!(request.contains(r#""max_tokens":1500"#));
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
    fn s1_contract_is_exact_and_deterministic() {
        assert_eq!(
            s1_control_line("semi-formal", "prose", "general"),
            "[Styling: semi-formal] [Structure: prose] [Context: general]"
        );
        assert!(S1_SYSTEM_PROMPT.starts_with("You are a text normalizer"));
        assert_eq!(
            strip_s1_template_artifacts("<think>\n\n</think>\nHello."),
            "Hello."
        );
    }

    #[test]
    fn s1_chunking_preserves_words_and_bounds_chunks() {
        let input = (0..1800)
            .map(|i| format!("word{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let chunks = chunk_s1_input(&input);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.chars().count() <= 3_250));
        assert_eq!(chunks.join(" "), input);
        assert!(chunk_s1_input("  ").is_empty());
    }

    #[test]
    fn s1_request_uses_required_contract_and_cleans_response() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;
        use std::sync::mpsc;

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            let body_start;
            loop {
                let n = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..n]);
                if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    body_start = pos + 4;
                    break;
                }
            }
            let headers = String::from_utf8_lossy(&request[..body_start]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap();
            while request.len() < body_start + content_length {
                let n = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..n]);
            }
            tx.send(String::from_utf8_lossy(&request).into_owned())
                .unwrap();
            let body =
                r#"{"choices":[{"message":{"content":"<think>\n\n</think>\nHello, world."}}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });

        let output = polish_s1(
            &format!("http://127.0.0.1:{port}/v1"),
            2_000,
            "semi-formal",
            "prose",
            "general",
            "um hello world",
        )
        .unwrap();
        server.join().unwrap();
        let request = rx.recv().unwrap();
        assert_eq!(output, "Hello, world.");
        assert!(request.starts_with("POST /v1/chat/completions "));
        assert!(request.contains(S1_SYSTEM_PROMPT));
        assert!(request.contains(r#""temperature":0"#));
        assert!(request.contains(
            r#"[Styling: semi-formal] [Structure: prose] [Context: general]\num hello world"#
        ));
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
        .unwrap()
        .is_none());
    }

    /// Keyring roundtrip — skipped on CI where keychains are unavailable.
    /// Run manually: `cargo test credential_roundtrip -- --ignored`
    #[test]
    #[ignore]
    fn credential_roundtrip() {
        const KEY: &str = "unit-test-key";
        let _ = cred::delete(KEY);
        cred::write(KEY, "sk-test-secret").expect("keyring write");
        assert_eq!(cred::read(KEY).as_deref(), Some("sk-test-secret"));
        assert!(cred::delete(KEY));
        assert_eq!(cred::read(KEY), None);
    }
}
