//! Rewrite the current selection with the text-cleanup model already configured.
//!
//! Dictation prompts and local filler rules are not used. Remote models get a
//! dedicated grammar prompt. S1-mini keeps its trained control line, with
//! structure forced to prose.

use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};
use anyhow::{Context, Result};
use serde::Serialize;
use tauri::{Emitter, Manager};

use crate::engine::{polish_cfg, PolishCfg};
use crate::paste;
use crate::polish;
use crate::s1_runtime::S1Runtime;
use crate::state::AppState;

pub(crate) const REFINE_MAX_CHARS: usize = 8_000;
const REFINE_MIN_TIMEOUT_MS: u64 = 15_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Capture {
    NoSelection,
    TooLong { limit: usize },
    Text(String),
}

pub(crate) fn interpret_copy(sentinel: &str, after: Option<&str>, max_chars: usize) -> Capture {
    let Some(raw) = after else {
        return Capture::NoSelection;
    };
    if raw == sentinel {
        return Capture::NoSelection;
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Capture::NoSelection;
    }
    if trimmed.chars().count() > max_chars {
        return Capture::TooLong { limit: max_chars };
    }
    Capture::Text(trimmed.to_owned())
}

pub(crate) fn should_paste(selection: &str, rewritten: &str) -> bool {
    let next = rewritten.trim();
    !next.is_empty() && next != selection.trim()
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AfterRefine {
    /// Put the pre-copy clipboard snapshot back.
    Original,
    /// The clipboard changed out from under us; leave it alone.
    LeaveClipboard,
}

#[cfg(test)]
pub(crate) fn after_refine(selection: &str, rewritten: &str, clipboard_now: Option<&str>) -> AfterRefine {
    if !should_paste(selection, rewritten) {
        return AfterRefine::Original;
    }
    if clipboard_now == Some(rewritten.trim()) {
        AfterRefine::Original
    } else {
        AfterRefine::LeaveClipboard
    }
}

/// Text that should be on the clipboard when refine finishes.
/// `Some` is the original snapshot. `None` means leave the clipboard as it is.
#[cfg(test)]
pub(crate) fn restored_text<'a>(
    original: &'a str,
    selection: &str,
    rewritten: &str,
    clipboard_now: Option<&str>,
) -> Option<&'a str> {
    match after_refine(selection, rewritten, clipboard_now) {
        AfterRefine::Original => Some(original),
        AfterRefine::LeaveClipboard => None,
    }
}

pub(crate) fn gate_enter(busy: &AtomicBool, recording: bool, transcribing: bool) -> bool {
    if recording || transcribing {
        return false;
    }
    busy.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

pub(crate) fn gate_leave(busy: &AtomicBool) {
    busy.store(false, Ordering::SeqCst);
}

pub(crate) fn refine_remote(cfg: &PolishCfg, text: &str) -> Result<String> {
    let polished = polish::polish(&polish::PolishArgs {
        backend: &cfg.llm_backend,
        base_url: &cfg.llm_api_base,
        api_key: &cfg.llm_api_key,
        model: &cfg.llm_api_model,
        preset: "refine",
        custom_prompt: "",
        temperature: cfg.llm_temperature,
        timeout_ms: refine_timeout(cfg.llm_timeout_ms),
        text,
    })?;
    polished.context("The cleanup model returned nothing, so the selection was left unchanged.")
}

pub(crate) fn refine_s1_at(
    base: &str,
    timeout_ms: u64,
    styling: &str,
    context: &str,
    text: &str,
) -> Result<String> {
    let out = polish::polish_s1(base, timeout_ms, styling, "prose", context, text)?;
    if out.trim().is_empty() {
        anyhow::bail!("The cleanup model returned nothing, so the selection was left unchanged.");
    }
    Ok(out)
}

pub(crate) fn refine_selection_text(
    cfg: &PolishCfg,
    data_dir: &Path,
    s1: &S1Runtime,
    text: &str,
) -> Result<String> {
    if cfg.llm_backend == "off" || cfg.llm_backend.trim().is_empty() {
        anyhow::bail!("Set a model under Models → Text cleanup before refining selected text.");
    }
    if text.trim().is_empty() {
        anyhow::bail!("Select text, then press the refine shortcut.");
    }
    let timeout = refine_timeout(cfg.llm_timeout_ms);
    if cfg.llm_backend == "s1_mini" {
        let base = s1.ensure(data_dir, timeout)?;
        return refine_s1_at(&base, timeout, &cfg.s1_styling, &cfg.s1_context, text);
    }
    refine_remote(cfg, text)
}

fn refine_timeout(timeout_ms: u64) -> u64 {
    timeout_ms.max(REFINE_MIN_TIMEOUT_MS)
}

#[derive(Clone, Serialize)]
struct Notice {
    message: String,
    error: bool,
}

impl Notice {
    fn info(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            error: false,
        }
    }

    fn error(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            error: true,
        }
    }
}

pub(crate) fn start(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        let entered = {
            let state = app.state::<Mutex<AppState>>();
            let Ok(s) = state.lock() else {
                return;
            };
            let ok = gate_enter(&s.refine_busy, s.recording, s.transcribing);
            if !ok {
                log::info!(
                    "refine skipped: busy gate (recording={}, transcribing={}, already refining={})",
                    s.recording,
                    s.transcribing,
                    s.refine_busy.load(std::sync::atomic::Ordering::SeqCst)
                );
            }
            ok
        };
        if !entered {
            return;
        }
        let worker = app.clone();
        let result = tokio::task::spawn_blocking(move || blocking_refine(worker)).await;
        if let Ok(mut s) = app.state::<Mutex<AppState>>().lock() {
            s.refining = false;
            gate_leave(&s.refine_busy);
        }
        let _ = app.emit("dictflow://refining", false);
        match result {
            Ok(Ok(())) => {}
            Ok(Err(notice)) => {
                let _ = app.emit("dictflow://refine-notice", notice);
            }
            Err(err) => {
                let _ = app.emit("dictflow://refine-notice", Notice::error(err.to_string()));
            }
        }
    });
}

fn blocking_refine(app: tauri::AppHandle) -> Result<(), Notice> {
    paste::remember_paste_target(&app);
    let target = app
        .state::<Mutex<AppState>>()
        .lock()
        .ok()
        .and_then(|s| s.paste_target);
    let fg = crate::focus::foreground_handle();
    let ours = crate::focus::our_handles(&app);
    let focus_ours = fg.is_some_and(|h| ours.contains(&h));
    log::info!("refine: foreground is a DictFlow window: {focus_ours}");
    let read = paste::capture_selection(&app).map_err(|err| {
        log::warn!("refine: selection capture failed: {err:#}");
        Notice::error(brief(&err))
    })?;
    let captured = interpret_copy(&read.sentinel, read.after.as_deref(), REFINE_MAX_CHARS);
    log::info!("refine: capture = {:?}", match &captured {
        Capture::NoSelection => "no selection".to_owned(),
        Capture::TooLong { limit } => format!("too long (> {limit})"),
        Capture::Text(t) => {
            let head: String = t.chars().take(60).collect();
            format!("{} chars: {head:?}", t.chars().count())
        }
    });
    let text = match captured {
        Capture::NoSelection => {
            let _ = paste::restore_clipboard(&read.snapshot);
            return Err(Notice::info(
                "Select text, then press the refine shortcut.",
            ));
        }
        Capture::TooLong { limit } => {
            let _ = paste::restore_clipboard(&read.snapshot);
            return Err(Notice::info(format!(
                "That selection is longer than {limit} characters, so it was left unchanged."
            )));
        }
        Capture::Text(text) => text,
    };

    if let Ok(mut s) = app.state::<Mutex<AppState>>().lock() {
        s.refining = true;
    }
    let _ = app.emit("dictflow://refining", true);

    let state_slot = app.state::<Mutex<AppState>>();
    let state = state_slot
        .lock()
        .map_err(|_| Notice::error("DictFlow is busy. Try the refine shortcut again."))?;
    let cfg = polish_cfg(&state.settings, &state.data_dir);
    let data_dir = state.data_dir.clone();
    let s1 = state.s1_runtime.clone();
    drop(state);
    let rewritten = match refine_selection_text(&cfg, &data_dir, &s1, &text) {
        Ok(value) => {
            log::info!(
                "refine: model ok ({} -> {} chars)",
                text.chars().count(),
                value.chars().count()
            );
            value
        }
        Err(err) => {
            log::warn!("refine: model failed: {err:#}");
            let _ = paste::restore_clipboard(&read.snapshot);
            return Err(Notice::error(brief(&err)));
        }
    };
    if !should_paste(&text, &rewritten) {
        let _ = paste::restore_clipboard(&read.snapshot);
        return Ok(());
    }
    let cleaned = rewritten.trim().to_owned();
    paste::paste_replacing(&app, &cleaned, target, read.snapshot)
        .map_err(|err| Notice::error(format!("Couldn't paste the refined text. {}", brief(&err))))
}

fn brief(err: &anyhow::Error) -> String {
    let raw = format!("{err:#}").replace('\n', " ");
    raw.chars().take(180).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    fn test_cfg(backend: &str, base: &str) -> PolishCfg {
        PolishCfg {
            audio_backend: "local".into(),
            audio_api_base: String::new(),
            audio_api_model: String::new(),
            audio_api_key: String::new(),
            llm_backend: backend.into(),
            llm_api_base: base.into(),
            llm_api_model: "test-model".into(),
            llm_api_key: "sk-test".into(),
            llm_temperature: 0.2,
            llm_timeout_ms: 1_000,
            llm_preset: "custom".into(),
            llm_custom_prompt: "speech-to-text ONLY CUSTOM".into(),
            llm_enabled: false,
            s1_styling: "formal".into(),
            s1_structure: "lists".into(),
            s1_context: "email".into(),
        }
    }

    #[test]
    fn capture_interpreter_and_restore_decision() {
        let sentinel = "\u{2060}sentinel";
        assert_eq!(
            interpret_copy(sentinel, Some(sentinel), REFINE_MAX_CHARS),
            Capture::NoSelection
        );
        assert_eq!(
            interpret_copy(sentinel, None, REFINE_MAX_CHARS),
            Capture::NoSelection
        );
        assert_eq!(
            interpret_copy(sentinel, Some("   "), REFINE_MAX_CHARS),
            Capture::NoSelection
        );
        assert_eq!(
            interpret_copy(sentinel, Some("  hi there  "), REFINE_MAX_CHARS),
            Capture::Text("hi there".into())
        );
        let long = "a".repeat(REFINE_MAX_CHARS + 1);
        assert_eq!(
            interpret_copy(sentinel, Some(&long), REFINE_MAX_CHARS),
            Capture::TooLong {
                limit: REFINE_MAX_CHARS
            }
        );

        assert!(should_paste("teh cat", "the cat"));
        assert!(!should_paste("  hello  ", "hello"));
        assert!(!should_paste("hello", "   "));

        let original = "keep me";
        let selection = "teh cat";
        assert_ne!(original, selection);
        assert_eq!(
            restored_text(original, selection, "the cat", Some("the cat")),
            Some(original)
        );
        assert_ne!(
            restored_text(original, selection, "the cat", Some("the cat")),
            Some(selection)
        );
        assert_eq!(
            restored_text(original, selection, selection, Some(selection)),
            Some(original)
        );
        assert_eq!(
            restored_text(original, selection, "the cat", Some("user changed")),
            None
        );
    }

    #[test]
    fn busy_gate_ignores_a_second_call_and_dictation() {
        let busy = AtomicBool::new(false);
        assert!(gate_enter(&busy, false, false));
        assert!(!gate_enter(&busy, false, false));
        gate_leave(&busy);
        assert!(!gate_enter(&busy, true, false));
        assert!(!gate_enter(&busy, false, true));
        assert!(gate_enter(&busy, false, false));
        gate_leave(&busy);
    }

    #[test]
    fn refine_prompt_ignores_the_dictation_custom_prompt() {
        let prompt = polish::system_prompt("refine", "speech-to-text ONLY CUSTOM");
        assert_eq!(prompt, crate::prompts::text("refine"));
        assert!(!prompt.to_ascii_lowercase().contains("speech-to-text"));
        assert!(prompt.contains("Return only the rewritten text"));
        assert!(!polish::PRESETS.contains(&"refine"));
        assert!(polish::prompt_catalog()["refine"].contains("grammar"));
    }

    #[test]
    fn backend_off_does_not_produce_replacement_text() {
        let cfg = test_cfg("off", "");
        let err = refine_selection_text(&cfg, Path::new("."), &S1Runtime::new(), "teh cat")
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("Text cleanup"), "{message}");
        assert!(!message.contains("teh cat"));
    }

    fn serve_once(body: &'static str) -> (u16, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let n = stream.read(&mut buffer).unwrap();
                if n == 0 {
                    return;
                }
                request.extend_from_slice(&buffer[..n]);
                let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let body_start = pos + 4;
                let headers = String::from_utf8_lossy(&request[..body_start]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                while request.len() < body_start + content_length {
                    let n = stream.read(&mut buffer).unwrap();
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..n]);
                }
                tx.send(String::from_utf8_lossy(&request).into_owned()).unwrap();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
                return;
            }
        });
        (port, rx, server)
    }

    #[test]
    fn refine_request_uses_the_refine_prompt_not_the_dictation_prompt() {
        let body = r#"{"choices":[{"message":{"content":"The cat sat."}}]}"#;
        let (port, rx, server) = serve_once(body);
        let cfg = test_cfg("openai_compat", &format!("http://127.0.0.1:{port}/v1"));
        let output = refine_selection_text(&cfg, Path::new("."), &S1Runtime::new(), "teh cat sat")
            .unwrap();
        server.join().unwrap();
        let request = rx.recv().unwrap();
        assert_eq!(output, "The cat sat.");
        assert!(request.contains("Fix grammar, spelling, capitalization"));
        assert!(request.contains("teh cat sat"));
        assert!(!request.contains("ONLY CUSTOM"));
        assert!(!request.contains("transcript editor"));
    }

    #[test]
    fn s1_refine_keeps_the_trained_prompt_and_forces_prose() {
        let body = r#"{"choices":[{"message":{"content":"The cat sat."}}]}"#;
        let (port, rx, server) = serve_once(body);
        let output = refine_s1_at(
            &format!("http://127.0.0.1:{port}/v1"),
            2_000,
            "formal",
            "email",
            "teh cat sat",
        )
        .unwrap();
        server.join().unwrap();
        let request = rx.recv().unwrap();
        assert_eq!(output, "The cat sat.");
        assert!(request.contains(polish::S1_SYSTEM_PROMPT));
        assert!(request.contains("[Styling: formal] [Structure: prose] [Context: email]"));
        assert!(!request.contains("[Structure: lists]"));
        assert!(request.contains("teh cat sat"));
    }
}
