//! Grok ACP stdio client (JSON-RPC 2.0, line-delimited).
//!
//! Pinned shapes (Grok 1.0.3 / docs.x.ai; re-verified 2026-08-12):
//! - `initialize` `{ protocolVersion: 1, clientCapabilities: { fs, terminal } }`
//! - `authenticate` `{ methodId, _meta: { headless: true } }`
//! - `session/new` `{ cwd, mcpServers: [] }`
//! - `session/prompt` `{ sessionId, prompt: [{ type: "text", text }] }`
//!
//! Windows I/O: every stdin write is `json + '\n'` then **flush**; stdout is a line reader.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::error::{CoordinatorError, Result};
use crate::outcome::FailureClass;

/// Absolute override for the `grok` binary.
pub const ENV_GROK_BIN: &str = "COORDINATOR_GROK_BIN";

/// Set to `1` to run ignored live ACP tests.
pub const ENV_GROK_LIVE: &str = "COORDINATOR_GROK_LIVE";

/// Result of one `session/prompt` turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptResult {
    pub text: String,
    pub stop_reason: Option<String>,
}

/// Long-lived Grok ACP session (one child + `sessionId`).
#[derive(Debug)]
pub struct GrokSession {
    transport: AcpTransport,
    pub session_id: String,
    pub cwd: PathBuf,
    pub pid: Option<u32>,
    pub supports_compact: bool,
    next_id: u64,
    collected_text: String,
}

enum AcpTransport {
    Process {
        child: Box<tokio::process::Child>,
        stdin: tokio::process::ChildStdin,
        stdout: BufReader<tokio::process::ChildStdout>,
    },
    Mock(MockTransport),
}

impl std::fmt::Debug for AcpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Process { child, .. } => f
                .debug_struct("Process")
                .field("pid", &child.id())
                .finish_non_exhaustive(),
            Self::Mock(m) => f.debug_tuple("Mock").field(m).finish(),
        }
    }
}

#[derive(Debug)]
struct MockTransport {
    responses: VecDeque<String>,
    written: Vec<String>,
}

impl GrokSession {
    /// Spawn `grok agent stdio`, initialize, authenticate, `session/new`.
    pub async fn start(cwd: PathBuf, timeout: Duration) -> Result<Self> {
        let bin = resolve_grok_binary()?;
        let mut cmd = tokio::process::Command::new(&bin);
        cmd.arg("agent")
            .arg("stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false)
            .current_dir(&cwd);
        #[cfg(windows)]
        {
            // CREATE_NO_WINDOW — avoid flashing a console for the ACP child.
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd.spawn().map_err(|e| {
            CoordinatorError::Message(format!(
                "failed to spawn grok agent stdio ({}): {e}",
                bin.display()
            ))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| CoordinatorError::Message("grok child stdin not piped".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CoordinatorError::Message("grok child stdout not piped".into()))?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            });
        }
        let pid = child.id();
        let mut session = Self {
            transport: AcpTransport::Process {
                child: Box::new(child),
                stdin,
                stdout: BufReader::new(stdout),
            },
            session_id: String::new(),
            cwd,
            pid,
            supports_compact: true,
            next_id: 1,
            collected_text: String::new(),
        };
        session.handshake(timeout).await?;
        Ok(session)
    }

    /// Mock transport for unit tests (scripted JSON-RPC lines).
    pub async fn start_mock(
        cwd: PathBuf,
        responses: Vec<String>,
        timeout: Duration,
    ) -> Result<Self> {
        let mut session = Self {
            transport: AcpTransport::Mock(MockTransport {
                responses: responses.into(),
                written: Vec::new(),
            }),
            session_id: String::new(),
            cwd,
            pid: Some(4242),
            supports_compact: true,
            next_id: 1,
            collected_text: String::new(),
        };
        session.handshake(timeout).await?;
        Ok(session)
    }

    /// Recorded JSON-RPC request payloads (mock only).
    pub fn mock_written(&self) -> Option<&[String]> {
        match &self.transport {
            AcpTransport::Mock(m) => Some(&m.written),
            AcpTransport::Process { .. } => None,
        }
    }

    /// Queue extra mock response lines (after handshake).
    pub fn mock_push_responses(&mut self, lines: impl IntoIterator<Item = String>) {
        if let AcpTransport::Mock(m) = &mut self.transport {
            m.responses.extend(lines);
        }
    }

    pub fn set_supports_compact(&mut self, value: bool) {
        self.supports_compact = value;
    }

    async fn handshake(&mut self, timeout: Duration) -> Result<()> {
        let init = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {
                        "fs": { "readTextFile": true, "writeTextFile": true },
                        "terminal": true
                    }
                }),
                timeout,
            )
            .await?;
        self.supports_compact = compact_supported(&init);

        let method_id = pick_auth_method(&init)?;
        self.request(
            "authenticate",
            json!({
                "methodId": method_id,
                "_meta": { "headless": true }
            }),
            timeout,
        )
        .await?;

        let cwd = self.cwd.to_string_lossy().into_owned();
        let created = self
            .request(
                "session/new",
                json!({
                    "cwd": cwd,
                    "mcpServers": []
                }),
                timeout,
            )
            .await?;
        let session_id = created
            .get("sessionId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                CoordinatorError::Message("session/new result missing sessionId".into())
            })?;
        self.session_id = session_id.to_string();
        Ok(())
    }

    /// Inject a prompt and wait for the prompt result (collecting `session/update` text).
    pub async fn inject_prompt(&mut self, text: &str, timeout: Duration) -> Result<PromptResult> {
        if self.session_id.is_empty() {
            return Err(CoordinatorError::Message(
                "no Grok sessionId; start the session first".into(),
            ));
        }
        self.collected_text.clear();
        let result = self
            .request(
                "session/prompt",
                json!({
                    "sessionId": self.session_id,
                    "prompt": [{ "type": "text", "text": text }]
                }),
                timeout,
            )
            .await?;
        let stop_reason = result
            .get("stopReason")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Ok(PromptResult {
            text: self.collected_text.clone(),
            stop_reason,
        })
    }

    /// Compact via `session/prompt` `/compact` (not a separate RPC).
    pub async fn compact(&mut self, timeout: Duration) -> Result<PromptResult> {
        if !self.supports_compact {
            return Err(CoordinatorError::Message(
                "compact is not supported by this Grok session".into(),
            ));
        }
        self.inject_prompt("/compact", timeout).await
    }

    /// Kill the ACP child (explicit teardown). Mock transport is a no-op drop.
    pub async fn shutdown(&mut self) -> Result<()> {
        match &mut self.transport {
            AcpTransport::Process { child, .. } => {
                let _ = child.kill().await;
            }
            AcpTransport::Mock(_) => {}
        }
        Ok(())
    }

    pub fn is_process_alive(&mut self) -> bool {
        match &mut self.transport {
            AcpTransport::Mock(_) => true,
            AcpTransport::Process { child, .. } => matches!(child.try_wait(), Ok(None)),
        }
    }

    async fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        self.write_line(&payload.to_string()).await?;

        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(CoordinatorError::Message(format!(
                    "ACP {method} timed out after {}s",
                    timeout.as_secs()
                )));
            }
            let line = tokio::time::timeout(remaining, self.read_line())
                .await
                .map_err(|_| {
                    CoordinatorError::Message(format!(
                        "ACP {method} timed out after {}s",
                        timeout.as_secs()
                    ))
                })??;
            let Some(line) = line else {
                return Err(CoordinatorError::Message(format!(
                    "ACP stdout closed during {method}"
                )));
            };
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(&line)
                .map_err(|e| CoordinatorError::Message(format!("invalid ACP JSON line: {e}")))?;
            if v.get("method").and_then(|m| m.as_str()) == Some("session/update") {
                collect_update(&v, &mut self.collected_text);
                continue;
            }
            if v.get("id") == Some(&json!(id)) {
                if let Some(err) = v.get("error") {
                    let msg = err
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("ACP error")
                        .to_string();
                    return Err(CoordinatorError::Message(format!("ACP {method}: {msg}")));
                }
                return Ok(v.get("result").cloned().unwrap_or(json!({})));
            }
        }
    }

    async fn write_line(&mut self, json_line: &str) -> Result<()> {
        match &mut self.transport {
            AcpTransport::Process { stdin, .. } => {
                stdin.write_all(json_line.as_bytes()).await?;
                stdin.write_all(b"\n").await?;
                stdin.flush().await?;
            }
            AcpTransport::Mock(m) => {
                m.written.push(json_line.to_string());
            }
        }
        Ok(())
    }

    async fn read_line(&mut self) -> Result<Option<String>> {
        match &mut self.transport {
            AcpTransport::Process { stdout, .. } => {
                let mut line = String::new();
                let n = stdout.read_line(&mut line).await?;
                if n == 0 {
                    return Ok(None);
                }
                if line.ends_with('\n') {
                    line.pop();
                    if line.ends_with('\r') {
                        line.pop();
                    }
                }
                Ok(Some(line))
            }
            AcpTransport::Mock(m) => {
                if let Some(line) = m.responses.pop_front() {
                    Ok(Some(line))
                } else {
                    std::future::pending::<()>().await;
                    Ok(None)
                }
            }
        }
    }
}

fn collect_update(msg: &Value, out: &mut String) {
    let update = msg.get("params").and_then(|p| p.get("update"));
    let Some(update) = update else {
        return;
    };
    if update.get("sessionUpdate").and_then(|s| s.as_str()) != Some("agent_message_chunk") {
        return;
    }
    if let Some(text) = update
        .get("content")
        .and_then(|c| c.get("text"))
        .and_then(|t| t.as_str())
    {
        out.push_str(text);
    }
}

fn compact_supported(init: &Value) -> bool {
    let Some(cmds) = init.get("availableCommands").and_then(|c| c.as_array()) else {
        return true;
    };
    cmds.iter().any(|c| {
        matches!(
            c.get("name").and_then(|n| n.as_str()),
            Some("compact") | Some("/compact")
        )
    })
}

fn pick_auth_method(init: &Value) -> Result<String> {
    let methods: Vec<String> = init
        .get("authMethods")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(|id| id.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    // Spec: prefer cached_token, else xai.api_key when XAI_API_KEY is set.
    if methods.iter().any(|m| m == "cached_token") || methods.is_empty() {
        return Ok("cached_token".into());
    }
    if std::env::var("XAI_API_KEY").is_ok() && methods.iter().any(|m| m == "xai.api_key") {
        return Ok("xai.api_key".into());
    }
    Err(CoordinatorError::Message(
        "no usable Grok auth method (need grok login / cached_token, or XAI_API_KEY)".into(),
    ))
}

/// Map ACP/auth/timeout errors onto ADR-0009 failure classes.
pub fn map_failure_class(err: &str) -> FailureClass {
    let e = err.to_ascii_lowercase();
    if e.contains("timed out") || e.contains("timeout") {
        return FailureClass::Timeout;
    }
    if e.contains("auth")
        || e.contains("permission")
        || e.contains("unauthor")
        || e.contains("login")
        || e.contains("methodid")
        || e.contains("no usable grok auth")
    {
        return FailureClass::Permission;
    }
    if e.contains("quota")
        || e.contains("rate limit")
        || e.contains("exhaust")
        || e.contains("resource_exhausted")
    {
        return FailureClass::ModelExhaustion;
    }
    FailureClass::HarnessCrash
}

/// Resolve `COORDINATOR_GROK_BIN` or the role-binding `grok` command on PATH.
pub fn resolve_grok_binary() -> Result<PathBuf> {
    if let Ok(over) = std::env::var(ENV_GROK_BIN)
        && !over.is_empty()
    {
        return resolve_command(&over);
    }
    let cmd = crate::harness::roles::resolve_grok_command()?;
    resolve_command(&cmd)
}

/// Walk PATH (+ Windows PATHEXT) or accept an absolute file. No `which` crate.
pub fn resolve_command(command: &str) -> Result<PathBuf> {
    if command.is_empty() {
        return Err(CoordinatorError::Message(
            "command must not be empty".into(),
        ));
    }
    let path = Path::new(command);
    if path.is_absolute() {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        return Err(CoordinatorError::Message(format!(
            "command not found: {}",
            path.display()
        )));
    }
    if command.contains('/') || command.contains('\\') {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        return Err(CoordinatorError::Message(format!(
            "command not found: {command}"
        )));
    }

    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let has_ext = Path::new(command)
        .extension()
        .is_some_and(|e| !e.is_empty());
    let exts: Vec<String> = if cfg!(windows) && !has_ext {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into())
            .split(';')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect()
    } else {
        Vec::new()
    };

    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(command);
        if candidate.is_file() {
            return Ok(candidate);
        }
        for ext in &exts {
            let with_ext = dir.join(format!("{command}{ext}"));
            if with_ext.is_file() {
                return Ok(with_ext);
            }
        }
    }
    Err(CoordinatorError::Message(format!(
        "command not found on PATH: {command}"
    )))
}

pub fn rpc_result(id: u64, result: Value) -> String {
    json!({"jsonrpc":"2.0","id":id,"result":result}).to_string()
}

pub fn rpc_error(id: u64, message: &str) -> String {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":message}}).to_string()
}

pub fn session_update_chunk(text: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "text": text }
            }
        }
    })
    .to_string()
}

/// Scripted handshake: initialize (cached_token + compact) → authenticate → session/new.
pub fn mock_handshake_ok(session_id: &str) -> Vec<String> {
    vec![
        rpc_result(
            1,
            json!({
                "protocolVersion": 1,
                "authMethods": [{ "id": "cached_token" }],
                "availableCommands": [{ "name": "compact", "input": { "hint": "optional context" } }]
            }),
        ),
        rpc_result(2, json!({})),
        rpc_result(3, json!({ "sessionId": session_id })),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn timeout() -> Duration {
        Duration::from_secs(2)
    }

    #[tokio::test]
    async fn mock_start_and_prompt_happy_path() {
        let dir = tempdir().unwrap();
        let mut lines = mock_handshake_ok("sess-1");
        lines.push(session_update_chunk("pong"));
        lines.push(rpc_result(4, json!({ "stopReason": "end_turn" })));
        let mut session = GrokSession::start_mock(dir.path().to_path_buf(), lines, timeout())
            .await
            .unwrap();
        assert_eq!(session.session_id, "sess-1");
        assert!(session.supports_compact);
        let result = session
            .inject_prompt("Reply with exactly: pong", timeout())
            .await
            .unwrap();
        assert_eq!(result.text, "pong");
        assert_eq!(result.stop_reason.as_deref(), Some("end_turn"));

        let written = session.mock_written().unwrap();
        assert_eq!(written.len(), 4);
        let init: Value = serde_json::from_str(&written[0]).unwrap();
        assert_eq!(init["method"], "initialize");
        assert_eq!(init["params"]["protocolVersion"], 1);
        assert_eq!(init["params"]["clientCapabilities"]["terminal"], true);
        assert_eq!(
            init["params"]["clientCapabilities"]["fs"]["readTextFile"],
            true
        );
        assert_eq!(
            init["params"]["clientCapabilities"]["fs"]["writeTextFile"],
            true
        );

        let auth: Value = serde_json::from_str(&written[1]).unwrap();
        assert_eq!(auth["method"], "authenticate");
        assert_eq!(auth["params"]["methodId"], "cached_token");
        assert_eq!(auth["params"]["_meta"]["headless"], true);

        let new: Value = serde_json::from_str(&written[2]).unwrap();
        assert_eq!(new["method"], "session/new");
        assert!(new["params"]["mcpServers"].as_array().unwrap().is_empty());
        assert!(new["params"]["cwd"].as_str().is_some());

        let prompt: Value = serde_json::from_str(&written[3]).unwrap();
        assert_eq!(prompt["method"], "session/prompt");
        assert_eq!(prompt["params"]["sessionId"], "sess-1");
        assert_eq!(prompt["params"]["prompt"][0]["type"], "text");
        assert_eq!(
            prompt["params"]["prompt"][0]["text"],
            "Reply with exactly: pong"
        );
    }

    #[tokio::test]
    async fn mock_auth_failure() {
        let dir = tempdir().unwrap();
        let lines = vec![
            rpc_result(
                1,
                json!({
                    "protocolVersion": 1,
                    "authMethods": [{ "id": "cached_token" }]
                }),
            ),
            rpc_error(2, "not logged in"),
        ];
        let err = GrokSession::start_mock(dir.path().to_path_buf(), lines, timeout())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not logged in"));
        assert_eq!(
            map_failure_class(&err.to_string()),
            FailureClass::Permission
        );
    }

    #[tokio::test]
    async fn mock_prompt_timeout() {
        let dir = tempdir().unwrap();
        let lines = mock_handshake_ok("sess-t");
        // no prompt result — read_line pending until timeout
        let mut session = GrokSession::start_mock(dir.path().to_path_buf(), lines, timeout())
            .await
            .unwrap();
        let err = session
            .inject_prompt("hang", Duration::from_millis(80))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"));
        assert_eq!(map_failure_class(&err.to_string()), FailureClass::Timeout);
    }

    #[tokio::test]
    async fn compact_injects_slash_command() {
        let dir = tempdir().unwrap();
        let mut lines = mock_handshake_ok("sess-c");
        lines.push(rpc_result(4, json!({ "stopReason": "end_turn" })));
        let mut session = GrokSession::start_mock(dir.path().to_path_buf(), lines, timeout())
            .await
            .unwrap();
        session.compact(timeout()).await.unwrap();
        let written = session.mock_written().unwrap();
        let prompt: Value = serde_json::from_str(written.last().unwrap()).unwrap();
        assert_eq!(prompt["params"]["prompt"][0]["text"], "/compact");
    }

    #[tokio::test]
    async fn compact_unsupported_errors() {
        let dir = tempdir().unwrap();
        let lines = mock_handshake_ok("sess-u");
        let mut session = GrokSession::start_mock(dir.path().to_path_buf(), lines, timeout())
            .await
            .unwrap();
        session.set_supports_compact(false);
        let err = session.compact(timeout()).await.unwrap_err();
        assert!(err.to_string().contains("not supported"));
    }

    #[test]
    fn resolve_absolute_missing_errors() {
        let err = resolve_command(r"C:\this\does\not\exist-grok.exe").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn resolve_empty_errors() {
        assert!(resolve_command("").is_err());
    }

    #[test]
    fn map_exhaustion() {
        assert_eq!(
            map_failure_class("resource_exhausted: quota"),
            FailureClass::ModelExhaustion
        );
        assert_eq!(
            map_failure_class("child io broken pipe"),
            FailureClass::HarnessCrash
        );
    }
}

/// Live ACP smoke. Default `cargo test` ignores this. Owner machine:
/// `$env:COORDINATOR_GROK_LIVE='1'; cargo test grok_live -- --ignored --nocapture`
#[cfg(test)]
mod live_tests {
    use super::*;
    use std::time::Duration;

    use crate::config::{ENV_COORDINATOR_HOME, test_env_lock};
    use crate::registry::{ProjectAddOptions, Registry};
    use tempfile::tempdir;

    fn live_enabled() -> bool {
        std::env::var(ENV_GROK_LIVE).ok().as_deref() == Some("1")
    }

    #[tokio::test]
    #[ignore = "requires grok on PATH + login; set COORDINATOR_GROK_LIVE=1"]
    #[allow(clippy::await_holding_lock)]
    async fn grok_live_start_prompt_shutdown() {
        if !live_enabled() {
            eprintln!("skip: {ENV_GROK_LIVE} != 1");
            return;
        }
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();

        let cwd = crate::harness::grok_cwd(&rec);
        let mut session = GrokSession::start(cwd, Duration::from_secs(60))
            .await
            .expect("live grok start");
        assert!(!session.session_id.is_empty());
        let result = session
            .inject_prompt("Reply with exactly: pong", Duration::from_secs(60))
            .await
            .expect("live grok prompt");
        eprintln!("live text={:?} stop={:?}", result.text, result.stop_reason);
        session.shutdown().await.unwrap();
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }
}
