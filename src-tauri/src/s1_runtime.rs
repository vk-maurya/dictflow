//! Managed llama.cpp server used only for S1-mini transcript normalization.

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::text_models;

#[derive(Default)]
struct RuntimeState {
    child: Option<Child>,
    port: Option<u16>,
}

#[derive(Clone, Default)]
pub(crate) struct S1Runtime {
    inner: Arc<Mutex<RuntimeState>>,
}

impl S1Runtime {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn ensure(&self, data_dir: &Path, timeout_ms: u64) -> Result<String> {
        let binary = text_models::runtime_binary(data_dir);
        let model = text_models::model_path(data_dir);
        let mut state = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("S1 runtime lock poisoned"))?;
        let existing_port = state.port;
        if let (Some(child), Some(port)) = (state.child.as_mut(), existing_port) {
            if child.try_wait()?.is_none() && healthy(port, 500, &model) {
                return Ok(format!("http://127.0.0.1:{port}/v1"));
            }
            stop_locked(&mut state);
        }

        if !text_models::runtime_ready(data_dir) {
            anyhow::bail!("S1-mini runtime is missing or incomplete");
        }
        if !text_models::valid_model_checksum(data_dir) {
            anyhow::bail!("S1-mini model is missing or failed checksum verification");
        }

        let port = free_port()?;
        let mut cmd = Command::new(&binary);
        cmd.current_dir(text_models::runtime_dir(data_dir));
        cmd.arg("-m")
            .arg(&model)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(port.to_string())
            .arg("--ctx-size")
            .arg("4096")
            .arg("--jinja")
            .arg("--chat-template-kwargs")
            .arg(r#"{"enable_thinking":false}"#)
            .arg("--temp")
            .arg("0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = cmd
            .spawn()
            .with_context(|| format!("start {}", binary.display()))?;
        state.child = Some(child);
        state.port = Some(port);

        let deadline = Instant::now() + Duration::from_millis(timeout_ms.clamp(2_000, 60_000));
        loop {
            if state
                .child
                .as_mut()
                .is_some_and(|child| child.try_wait().ok().flatten().is_some())
            {
                stop_locked(&mut state);
                anyhow::bail!("S1-mini runtime exited while loading the model");
            }
            if healthy(port, 750, &model) {
                return Ok(format!("http://127.0.0.1:{port}/v1"));
            }
            if Instant::now() >= deadline {
                stop_locked(&mut state);
                anyhow::bail!("S1-mini runtime did not become ready in time");
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    pub(crate) fn shutdown(&self) {
        if let Ok(mut state) = self.inner.lock() {
            stop_locked(&mut state);
        }
    }

    #[cfg(test)]
    pub(crate) fn is_running(&self) -> bool {
        let Ok(mut state) = self.inner.lock() else {
            return false;
        };
        let Some(child) = state.child.as_mut() else {
            return false;
        };
        child.try_wait().ok().flatten().is_none()
    }
}

fn free_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).context("reserve S1 runtime port")?;
    Ok(listener.local_addr()?.port())
}

fn healthy(port: u16, timeout_ms: u64, expected_model: &Path) -> bool {
    let Ok(client) = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .build()
    else {
        return false;
    };
    let Ok(health) = client.get(format!("http://127.0.0.1:{port}/health")).send() else {
        return false;
    };
    if !health.status().is_success() {
        return false;
    }
    let Ok(props) = client
        .get(format!("http://127.0.0.1:{port}/props"))
        .send()
        .and_then(|response| response.error_for_status())
        .and_then(|response| response.json::<serde_json::Value>())
    else {
        return false;
    };
    let expected_name = expected_model.file_name();
    props["model_path"]
        .as_str()
        .and_then(|path| Path::new(path).file_name())
        == expected_name
}

fn stop_locked(state: &mut RuntimeState) {
    if let Some(mut child) = state.child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    state.port = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocated_port_is_loopback_reachable() {
        let port = free_port().unwrap();
        assert!(port > 0);
    }

    #[test]
    fn shutdown_is_idempotent() {
        let runtime = S1Runtime::new();
        runtime.shutdown();
        runtime.shutdown();
        assert!(!runtime.is_running());
    }
}
