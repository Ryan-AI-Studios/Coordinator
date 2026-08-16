//! Grok ACP stdio client (JSON-RPC 2.0, line-delimited).
//!
//! Pinned shapes (Grok 1.0.3 / docs.x.ai; re-verified 2026-08-12):
//! - `initialize` `{ protocolVersion: 1, clientCapabilities: { fs, terminal } }`
//! - `authenticate` `{ methodId, _meta: { headless: true } }`
//! - `session/new` `{ cwd, mcpServers: [] }`
//! - `session/prompt` `{ sessionId, prompt: [{ type: "text", text }] }`
//! - Agent → client `fs/read_text_file` / `fs/write_text_file` (also camelCase aliases)
//!   because `initialize` advertises `fs.readTextFile` / `fs.writeTextFile`. Unanswered
//!   fs requests hang Grok `read_file` until the stall/phase clocks.
//!
//! Windows I/O: every stdin write is `json + '\n'` then **flush**; stdout is a line reader.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex as TokioMutex;

use crate::error::{CoordinatorError, Result};
use crate::outcome::FailureClass;
use crate::registry::ProjectRecord;

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
    /// When set, every ACP `session/update` writes `{state_dir}/harness-progress.json`.
    progress_record: Option<ProjectRecord>,
    /// Shared with [`CancelHandle`] so cancel can flush stdin during `inject_prompt`.
    writer: AcpWriter,
    in_flight_id: Arc<AtomicU64>,
    session_id_shared: Arc<std::sync::Mutex<String>>,
    cancel_requested: Arc<AtomicBool>,
}

/// Cloneable stdin writer so abort can send `session/cancel` without the pool lock.
#[derive(Clone)]
pub struct CancelHandle {
    session_id: Arc<std::sync::Mutex<String>>,
    writer: AcpWriter,
    in_flight_id: Arc<AtomicU64>,
    cancel_requested: Arc<AtomicBool>,
}

impl std::fmt::Debug for CancelHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sid = self
            .session_id
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default();
        f.debug_struct("CancelHandle")
            .field("session_id", &sid)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
struct AcpWriter {
    inner: AcpWriterInner,
}

#[derive(Clone)]
enum AcpWriterInner {
    Process(Arc<TokioMutex<tokio::process::ChildStdin>>),
    Mock(MockIo),
}

impl std::fmt::Debug for AcpWriterInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Process(_) => f.debug_tuple("Process").finish_non_exhaustive(),
            Self::Mock(m) => f.debug_tuple("Mock").field(m).finish(),
        }
    }
}

#[derive(Clone, Debug)]
struct MockIo {
    written: Arc<std::sync::Mutex<Vec<String>>>,
    incoming_tx: tokio::sync::mpsc::UnboundedSender<String>,
}

enum AcpTransport {
    Process {
        child: Box<tokio::process::Child>,
        stdout: BufReader<tokio::process::ChildStdout>,
    },
    Mock {
        incoming_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
        written: Arc<std::sync::Mutex<Vec<String>>>,
    },
}

impl std::fmt::Debug for AcpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Process { child, .. } => f
                .debug_struct("Process")
                .field("pid", &child.id())
                .finish_non_exhaustive(),
            Self::Mock { .. } => f.debug_tuple("Mock").finish(),
        }
    }
}

impl GrokSession {
    /// Spawn `grok agent stdio`, initialize, authenticate, `session/new`.
    ///
    /// CLI / HTTP start path: implementor-first binary, no `-m`.
    pub async fn start(cwd: PathBuf, timeout: Duration) -> Result<Self> {
        let bin = resolve_grok_binary()?;
        Self::start_with_bin(cwd, timeout, bin, None).await
    }

    /// Spawn with an already-resolved phase binary and optional `-m` model.
    pub async fn start_with_bin(
        cwd: PathBuf,
        timeout: Duration,
        bin: PathBuf,
        model: Option<&str>,
    ) -> Result<Self> {
        let mut cmd = tokio::process::Command::new(&bin);
        for arg in grok_agent_argv(model) {
            cmd.arg(arg);
        }
        cmd.stdin(Stdio::piped())
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
        let writer = AcpWriter {
            inner: AcpWriterInner::Process(Arc::new(TokioMutex::new(stdin))),
        };
        let in_flight_id = Arc::new(AtomicU64::new(0));
        let session_id_shared = Arc::new(std::sync::Mutex::new(String::new()));
        let cancel_requested = Arc::new(AtomicBool::new(false));
        let mut session = Self {
            transport: AcpTransport::Process {
                child: Box::new(child),
                stdout: BufReader::new(stdout),
            },
            session_id: String::new(),
            cwd,
            pid,
            supports_compact: true,
            next_id: 1,
            collected_text: String::new(),
            progress_record: None,
            writer,
            in_flight_id,
            session_id_shared,
            cancel_requested,
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
        let (incoming_tx, incoming_rx) = tokio::sync::mpsc::unbounded_channel();
        for line in responses {
            incoming_tx
                .send(line)
                .map_err(|_| CoordinatorError::Message("mock inbox closed".into()))?;
        }
        let written = Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = AcpWriter {
            inner: AcpWriterInner::Mock(MockIo {
                written: written.clone(),
                incoming_tx,
            }),
        };
        let in_flight_id = Arc::new(AtomicU64::new(0));
        let session_id_shared = Arc::new(std::sync::Mutex::new(String::new()));
        let cancel_requested = Arc::new(AtomicBool::new(false));
        let mut session = Self {
            transport: AcpTransport::Mock {
                incoming_rx,
                written,
            },
            session_id: String::new(),
            cwd,
            pid: Some(4242),
            supports_compact: true,
            next_id: 1,
            collected_text: String::new(),
            progress_record: None,
            writer,
            in_flight_id,
            session_id_shared,
            cancel_requested,
        };
        session.handshake(timeout).await?;
        Ok(session)
    }

    /// Recorded JSON-RPC request payloads (mock only).
    pub fn mock_written(&self) -> Option<Vec<String>> {
        match &self.transport {
            AcpTransport::Mock { written, .. } => Some(written.lock().ok()?.clone()),
            AcpTransport::Process { .. } => None,
        }
    }

    /// Queue extra mock response lines (after handshake). Wakes a pending `read_line`.
    pub fn mock_push_responses(&mut self, lines: impl IntoIterator<Item = String>) {
        if let AcpWriterInner::Mock(m) = &self.writer.inner {
            for line in lines {
                let _ = m.incoming_tx.send(line);
            }
        }
    }

    /// Cloneable cancel writer (safe to use while `inject_prompt` holds `&mut self`).
    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle {
            session_id: self.session_id_shared.clone(),
            writer: self.writer.clone(),
            in_flight_id: self.in_flight_id.clone(),
            cancel_requested: self.cancel_requested.clone(),
        }
    }

    /// ACP `session/cancel` notification (no JSON-RPC `id`) on the same stdio child.
    pub async fn cancel(&self) -> Result<()> {
        self.cancel_handle().cancel().await
    }

    pub fn set_supports_compact(&mut self, value: bool) {
        self.supports_compact = value;
    }

    /// Bind this session to a project so ACP `session/update` writes the progress sidecar.
    pub fn set_progress_record(&mut self, record: ProjectRecord) {
        self.progress_record = Some(record);
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
        if let Ok(mut shared) = self.session_id_shared.lock() {
            *shared = self.session_id.clone();
        }
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
            AcpTransport::Mock { .. } => {}
        }
        Ok(())
    }

    pub fn is_process_alive(&mut self) -> bool {
        match &mut self.transport {
            AcpTransport::Mock { .. } => true,
            AcpTransport::Process { child, .. } => matches!(child.try_wait(), Ok(None)),
        }
    }

    async fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.in_flight_id.store(id, Ordering::SeqCst);
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
                self.in_flight_id.store(0, Ordering::SeqCst);
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
                });
            let line = match line {
                Ok(inner) => inner?,
                Err(e) => {
                    self.in_flight_id.store(0, Ordering::SeqCst);
                    return Err(e);
                }
            };
            let Some(line) = line else {
                self.in_flight_id.store(0, Ordering::SeqCst);
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
                if let Some(ref rec) = self.progress_record {
                    let sid = if self.session_id.is_empty() {
                        None
                    } else {
                        Some(self.session_id.as_str())
                    };
                    crate::workflow::watchdog::note_progress(
                        rec,
                        crate::workflow::watchdog::ProgressKind::SessionUpdate,
                        sid,
                    );
                }
                continue;
            }
            if v.get("method").and_then(|m| m.as_str()) == Some("session/request_permission")
                && self.cancel_requested.load(Ordering::SeqCst)
            {
                if let Some(perm_id) = v.get("id").cloned() {
                    let reply = json!({
                        "jsonrpc": "2.0",
                        "id": perm_id,
                        "result": { "outcome": "cancelled" }
                    });
                    self.write_line(&reply.to_string()).await?;
                }
                continue;
            }
            if self.handle_agent_fs(&v).await? {
                continue;
            }
            if v.get("id") == Some(&json!(id)) {
                self.in_flight_id.store(0, Ordering::SeqCst);
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

    /// Reply to agent→client filesystem RPCs advertised at `initialize`.
    async fn handle_agent_fs(&mut self, v: &Value) -> Result<bool> {
        let Some(method) = v.get("method").and_then(|m| m.as_str()) else {
            return Ok(false);
        };
        let read = method == "fs/read_text_file" || method == "fs/readTextFile";
        let write = method == "fs/write_text_file" || method == "fs/writeTextFile";
        if !read && !write {
            return Ok(false);
        }
        let Some(req_id) = v.get("id").cloned() else {
            return Ok(true);
        };
        if let Some(ref rec) = self.progress_record {
            let sid = if self.session_id.is_empty() {
                None
            } else {
                Some(self.session_id.as_str())
            };
            crate::workflow::watchdog::note_progress(
                rec,
                crate::workflow::watchdog::ProgressKind::SessionUpdate,
                sid,
            );
        }
        let roots = self.fs_allowed_roots();
        let reply = if read {
            fs_read_reply(req_id, v.get("params"), &roots)
        } else {
            fs_write_reply(req_id, v.get("params"), &roots)
        };
        self.write_line(&reply).await?;
        Ok(true)
    }

    /// Session cwd plus layout workspace / execution (planning tree is outside grok_cwd).
    fn fs_allowed_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.cwd.clone()];
        if let Some(rec) = &self.progress_record {
            let paths = crate::layout::resolve(rec);
            roots.push(paths.workspace_root);
            if let Some(exec) = paths.execution_repo {
                roots.push(exec);
            }
        }
        roots
    }

    async fn write_line(&self, json_line: &str) -> Result<()> {
        self.writer.write_line(json_line).await
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
            AcpTransport::Mock { incoming_rx, .. } => match incoming_rx.recv().await {
                Some(line) => Ok(Some(line)),
                None => Ok(None),
            },
        }
    }
}

impl AcpWriter {
    async fn write_line(&self, json_line: &str) -> Result<()> {
        match &self.inner {
            AcpWriterInner::Process(stdin) => {
                let mut guard = stdin.lock().await;
                guard.write_all(json_line.as_bytes()).await?;
                guard.write_all(b"\n").await?;
                guard.flush().await?;
            }
            AcpWriterInner::Mock(m) => {
                if let Ok(mut written) = m.written.lock() {
                    written.push(json_line.to_string());
                }
            }
        }
        Ok(())
    }
}

impl CancelHandle {
    /// Notification only — never a JSON-RPC request (`id` must be absent).
    pub async fn cancel(&self) -> Result<()> {
        let sid = self
            .session_id
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default();
        if sid.is_empty() {
            return Err(CoordinatorError::Message(
                "no Grok sessionId; start the session first".into(),
            ));
        }
        self.cancel_requested.store(true, Ordering::SeqCst);
        let payload = json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": sid }
        });
        debug_assert!(
            payload.get("id").is_none(),
            "session/cancel must be a notification"
        );
        self.writer.write_line(&payload.to_string()).await?;
        if let AcpWriterInner::Mock(m) = &self.writer.inner {
            let id = self.in_flight_id.load(Ordering::SeqCst);
            if id > 0 {
                let _ = m
                    .incoming_tx
                    .send(rpc_result(id, json!({ "stopReason": "cancelled" })));
            }
        }
        Ok(())
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

/// `grok agent [-m {model}] stdio`. Omit `-m` when model is none/empty.
pub fn grok_agent_argv(model: Option<&str>) -> Vec<String> {
    let mut args = vec!["agent".into()];
    if let Some(m) = model {
        let t = m.trim();
        if !t.is_empty() {
            args.push("-m".into());
            args.push(t.to_string());
        }
    }
    args.push("stdio".into());
    args
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

/// Replace Windows npm shims (`.ps1` or extensionless `#!` shebang) with a
/// sibling `.exe` then `.cmd`. Never `Command::new` the sh script.
pub(crate) fn reject_or_replace_ps1(path: PathBuf) -> Result<PathBuf> {
    if !needs_shim_replace(&path) {
        return Ok(path);
    }
    if let Some(parent) = path.parent()
        && let Some(stem) = path.file_stem()
    {
        for alt_ext in ["exe", "cmd"] {
            let alt = parent.join(stem).with_extension(alt_ext);
            if alt.is_file() {
                return Ok(alt);
            }
        }
    }
    Err(CoordinatorError::Message(format!(
        "refusing to spawn shim: {}",
        path.display()
    )))
}

fn needs_shim_replace(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "ps1" {
        return true;
    }
    if !ext.is_empty() {
        return false;
    }
    is_shebang(path)
}

fn is_shebang(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 2];
    matches!(f.read_exact(&mut buf), Ok(()) if &buf == b"#!")
}

pub fn rpc_result(id: u64, result: Value) -> String {
    json!({"jsonrpc":"2.0","id":id,"result":result}).to_string()
}

pub fn rpc_error(id: u64, message: &str) -> String {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":message}}).to_string()
}

fn rpc_error_id(id: Value, code: i32, message: &str) -> String {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}).to_string()
}

/// ACP `line` is 1-based. Missing / `0` starts at the first line.
fn slice_text_lines(content: &str, line: Option<u64>, limit: Option<u64>) -> String {
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let start = match line {
        Some(n) if n > 0 => (n as usize).saturating_sub(1),
        _ => 0,
    };
    if start >= lines.len() {
        return String::new();
    }
    let end = match limit {
        Some(n) => start.saturating_add(n as usize).min(lines.len()),
        None => lines.len(),
    };
    lines[start..end].concat()
}

fn path_is_under(root: &Path, candidate: &Path) -> bool {
    let root_s = match dunce::canonicalize(root) {
        Ok(r) => dunce::simplified(&r).to_path_buf(),
        Err(_) => dunce::simplified(root).to_path_buf(),
    };
    let mut cur = dunce::simplified(candidate).to_path_buf();
    loop {
        if let Ok(c) = dunce::canonicalize(&cur) {
            return dunce::simplified(&c).starts_with(&root_s);
        }
        match cur.parent() {
            Some(parent) if parent != cur => cur = parent.to_path_buf(),
            _ => break,
        }
    }
    dunce::simplified(candidate).starts_with(&root_s)
}

fn fs_path_allowed(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path_is_under(root, path))
}

fn fs_read_reply(id: Value, params: Option<&Value>, roots: &[PathBuf]) -> String {
    let Some(path) = params.and_then(|p| p.get("path")).and_then(|v| v.as_str()) else {
        return rpc_error_id(id, -32602, "fs/read_text_file missing path");
    };
    let p = Path::new(path);
    if !p.is_absolute() {
        return rpc_error_id(id, -32602, "fs/read_text_file path must be absolute");
    }
    if !fs_path_allowed(p, roots) {
        return rpc_error_id(id, -32602, "fs/read_text_file path is outside the project");
    }
    let line = params.and_then(|p| p.get("line")).and_then(|v| v.as_u64());
    let limit = params.and_then(|p| p.get("limit")).and_then(|v| v.as_u64());
    match std::fs::read_to_string(p) {
        Ok(content) => {
            let content = slice_text_lines(&content, line, limit);
            json!({"jsonrpc":"2.0","id":id,"result":{"content":content}}).to_string()
        }
        Err(e) => rpc_error_id(id, -32000, &format!("fs/read_text_file: {e}")),
    }
}

fn fs_write_reply(id: Value, params: Option<&Value>, roots: &[PathBuf]) -> String {
    let Some(path) = params.and_then(|p| p.get("path")).and_then(|v| v.as_str()) else {
        return rpc_error_id(id, -32602, "fs/write_text_file missing path");
    };
    let p = Path::new(path);
    if !p.is_absolute() {
        return rpc_error_id(id, -32602, "fs/write_text_file path must be absolute");
    }
    if !fs_path_allowed(p, roots) {
        return rpc_error_id(id, -32602, "fs/write_text_file path is outside the project");
    }
    let Some(content) = params
        .and_then(|p| p.get("content"))
        .and_then(|v| v.as_str())
    else {
        return rpc_error_id(id, -32602, "fs/write_text_file missing content");
    };
    if let Some(parent) = p.parent()
        && !parent.as_os_str().is_empty()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return rpc_error_id(id, -32000, &format!("fs/write_text_file: {e}"));
    }
    match std::fs::write(p, content) {
        Ok(()) => json!({"jsonrpc":"2.0","id":id,"result":null}).to_string(),
        Err(e) => rpc_error_id(id, -32000, &format!("fs/write_text_file: {e}")),
    }
}

/// Agent → client `fs/read_text_file` (official ACP snake_case method).
pub fn fs_read_text_file(id: u64, session_id: &str, path: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "fs/read_text_file",
        "params": { "sessionId": session_id, "path": path }
    })
    .to_string()
}

/// Agent → client `fs/write_text_file`.
pub fn fs_write_text_file(id: u64, session_id: &str, path: &str, content: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "fs/write_text_file",
        "params": { "sessionId": session_id, "path": path, "content": content }
    })
    .to_string()
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

/// Mock ACP `session/update` with `tool_call` (any `sessionUpdate` kind is progress).
pub fn session_update_tool_call(title: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "call_1",
                "title": title,
                "kind": "read"
            }
        }
    })
    .to_string()
}

/// Agent → client `session/request_permission` (JSON-RPC request; client must reply).
pub fn session_request_permission(id: u64, session_id: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/request_permission",
        "params": {
            "sessionId": session_id,
            "toolCall": { "toolCallId": "call_perm", "title": "write" },
            "options": []
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

    #[test]
    fn grok_agent_argv_includes_model_between_agent_and_stdio() {
        let args = grok_agent_argv(Some("grok-build"));
        assert_eq!(args, ["agent", "-m", "grok-build", "stdio"]);
        assert!(!args.iter().any(|a| a == "--always-approve"));
        assert!(!args.iter().any(|a| a == "--yolo"));
    }

    #[test]
    fn grok_agent_argv_omits_empty_model() {
        assert_eq!(grok_agent_argv(None), ["agent", "stdio"]);
        assert_eq!(grok_agent_argv(Some("")), ["agent", "stdio"]);
        assert_eq!(grok_agent_argv(Some("   ")), ["agent", "stdio"]);
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
        assert!(new["params"].get("model").is_none());
        assert!(new["params"].get("_meta").is_none());

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

    #[tokio::test]
    async fn cancel_writes_notification_without_id_and_unblocks_prompt() {
        let dir = tempdir().unwrap();
        let lines = mock_handshake_ok("sess-cancel");
        let mut session = GrokSession::start_mock(dir.path().to_path_buf(), lines, timeout())
            .await
            .unwrap();
        let handle = session.cancel_handle();
        let prompt = session.inject_prompt("hang", Duration::from_secs(2));
        let cancel = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            handle.cancel().await.unwrap();
        };
        let (result, _) = tokio::join!(prompt, cancel);
        let result = result.unwrap();
        assert_eq!(result.stop_reason.as_deref(), Some("cancelled"));

        let written = session.mock_written().unwrap();
        let cancel_line = written
            .iter()
            .map(|s| serde_json::from_str::<Value>(s).unwrap())
            .find(|v| v["method"] == "session/cancel")
            .expect("session/cancel written");
        assert!(cancel_line.get("id").is_none(), "cancel is a notification");
        assert_eq!(cancel_line["params"]["sessionId"], "sess-cancel");
        assert_eq!(cancel_line["jsonrpc"], "2.0");
    }

    #[tokio::test]
    async fn permission_request_during_prompt_is_answered_cancelled() {
        let dir = tempdir().unwrap();
        let mut lines = mock_handshake_ok("sess-perm");
        lines.push(session_request_permission(99, "sess-perm"));
        lines.push(rpc_result(4, json!({ "stopReason": "end_turn" })));
        let mut session = GrokSession::start_mock(dir.path().to_path_buf(), lines, timeout())
            .await
            .unwrap();
        session.cancel().await.unwrap();
        let result = session.inject_prompt("ok", timeout()).await.unwrap();
        assert_eq!(result.stop_reason.as_deref(), Some("end_turn"));
        let written = session.mock_written().unwrap();
        let reply = written
            .iter()
            .map(|s| serde_json::from_str::<Value>(s).unwrap())
            .find(|v| v.get("id") == Some(&json!(99)))
            .expect("permission result written");
        assert_eq!(reply["result"]["outcome"], "cancelled");
        assert!(reply.get("method").is_none());
    }

    #[tokio::test]
    async fn fs_read_text_file_during_prompt_returns_content() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("outside").join("note.md");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "hello from parent\nline2\n").unwrap();
        let mut lines = mock_handshake_ok("sess-fs");
        lines.push(fs_read_text_file(99, "sess-fs", file.to_str().unwrap()));
        lines.push(rpc_result(4, json!({ "stopReason": "end_turn" })));
        let mut session = GrokSession::start_mock(dir.path().to_path_buf(), lines, timeout())
            .await
            .unwrap();
        let result = session.inject_prompt("read", timeout()).await.unwrap();
        assert_eq!(result.stop_reason.as_deref(), Some("end_turn"));
        let written = session.mock_written().unwrap();
        let reply = written
            .iter()
            .map(|s| serde_json::from_str::<Value>(s).unwrap())
            .find(|v| v.get("id") == Some(&json!(99)))
            .expect("fs/read_text_file result written");
        assert_eq!(reply["result"]["content"], "hello from parent\nline2\n");
        assert!(reply.get("method").is_none());
    }

    #[tokio::test]
    async fn fs_read_text_file_camel_case_alias() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "ok").unwrap();
        let mut lines = mock_handshake_ok("sess-fs2");
        lines.push(
            json!({
                "jsonrpc": "2.0",
                "id": 77,
                "method": "fs/readTextFile",
                "params": { "sessionId": "sess-fs2", "path": file.to_str().unwrap() }
            })
            .to_string(),
        );
        lines.push(rpc_result(4, json!({ "stopReason": "end_turn" })));
        let mut session = GrokSession::start_mock(dir.path().to_path_buf(), lines, timeout())
            .await
            .unwrap();
        session.inject_prompt("read", timeout()).await.unwrap();
        let written = session.mock_written().unwrap();
        let reply = written
            .iter()
            .map(|s| serde_json::from_str::<Value>(s).unwrap())
            .find(|v| v.get("id") == Some(&json!(77)))
            .expect("camelCase fs reply");
        assert_eq!(reply["result"]["content"], "ok");
    }

    #[tokio::test]
    async fn fs_read_text_file_relative_path_is_invalid() {
        let dir = tempdir().unwrap();
        let mut lines = mock_handshake_ok("sess-fs3");
        lines.push(fs_read_text_file(55, "sess-fs3", "relative.txt"));
        lines.push(rpc_result(4, json!({ "stopReason": "end_turn" })));
        let mut session = GrokSession::start_mock(dir.path().to_path_buf(), lines, timeout())
            .await
            .unwrap();
        session.inject_prompt("read", timeout()).await.unwrap();
        let written = session.mock_written().unwrap();
        let reply = written
            .iter()
            .map(|s| serde_json::from_str::<Value>(s).unwrap())
            .find(|v| v.get("id") == Some(&json!(55)))
            .expect("relative path error");
        assert_eq!(reply["error"]["code"], -32602);
        assert!(
            reply["error"]["message"]
                .as_str()
                .unwrap()
                .contains("absolute")
        );
    }

    #[tokio::test]
    async fn fs_write_text_file_during_prompt_creates_file() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("track").join("evidence.md");
        let mut lines = mock_handshake_ok("sess-fsw");
        lines.push(fs_write_text_file(
            88,
            "sess-fsw",
            file.to_str().unwrap(),
            "coordinator 0.1.0\n",
        ));
        lines.push(rpc_result(4, json!({ "stopReason": "end_turn" })));
        let mut session = GrokSession::start_mock(dir.path().to_path_buf(), lines, timeout())
            .await
            .unwrap();
        session.inject_prompt("write", timeout()).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "coordinator 0.1.0\n"
        );
        let written = session.mock_written().unwrap();
        let reply = written
            .iter()
            .map(|s| serde_json::from_str::<Value>(s).unwrap())
            .find(|v| v.get("id") == Some(&json!(88)))
            .expect("fs/write_text_file result");
        assert!(reply["result"].is_null());
    }

    #[tokio::test]
    async fn fs_read_workspace_parent_allowed_when_progress_bound() {
        let ws = tempdir().unwrap();
        let exec = ws.path().join("hands");
        std::fs::create_dir_all(&exec).unwrap();
        let file = ws.path().join("conductor").join("0099").join("spec.md");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "probe spec\n").unwrap();
        let mut lines = mock_handshake_ok("sess-ws");
        lines.push(fs_read_text_file(41, "sess-ws", file.to_str().unwrap()));
        lines.push(rpc_result(4, json!({ "stopReason": "end_turn" })));
        let mut session = GrokSession::start_mock(exec.clone(), lines, timeout())
            .await
            .unwrap();
        session.set_progress_record(ProjectRecord {
            id: "hh".into(),
            path: ws.path().to_path_buf(),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: Some(exec),
            execution_repos: std::collections::BTreeMap::new(),
            state_dir: None,
            auto_merge: false,
            phase_timeouts_secs: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now(),
        });
        session.inject_prompt("read", timeout()).await.unwrap();
        let written = session.mock_written().unwrap();
        let reply = written
            .iter()
            .map(|s| serde_json::from_str::<Value>(s).unwrap())
            .find(|v| v.get("id") == Some(&json!(41)))
            .expect("workspace parent read");
        assert_eq!(reply["result"]["content"], "probe spec\n");
    }

    #[tokio::test]
    async fn fs_read_outside_project_is_invalid() {
        let dir = tempdir().unwrap();
        let other = tempdir().unwrap();
        let file = other.path().join("secret.txt");
        std::fs::write(&file, "nope").unwrap();
        let mut lines = mock_handshake_ok("sess-deny");
        lines.push(fs_read_text_file(42, "sess-deny", file.to_str().unwrap()));
        lines.push(rpc_result(4, json!({ "stopReason": "end_turn" })));
        let mut session = GrokSession::start_mock(dir.path().to_path_buf(), lines, timeout())
            .await
            .unwrap();
        session.inject_prompt("read", timeout()).await.unwrap();
        let written = session.mock_written().unwrap();
        let reply = written
            .iter()
            .map(|s| serde_json::from_str::<Value>(s).unwrap())
            .find(|v| v.get("id") == Some(&json!(42)))
            .expect("outside project error");
        assert_eq!(reply["error"]["code"], -32602);
        assert!(
            reply["error"]["message"]
                .as_str()
                .unwrap()
                .contains("outside")
        );
    }

    #[tokio::test]
    async fn fs_write_through_junction_outside_project_is_invalid() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let link = dir.path().join("escape");
        let status = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                link.to_str().unwrap(),
                outside.path().to_str().unwrap(),
            ])
            .status()
            .expect("mklink");
        if !status.success() {
            eprintln!("skip: mklink /J not available");
            return;
        }
        let file = link.join("pwned.txt");
        let mut lines = mock_handshake_ok("sess-junc");
        lines.push(fs_write_text_file(
            44,
            "sess-junc",
            file.to_str().unwrap(),
            "escaped\n",
        ));
        lines.push(rpc_result(4, json!({ "stopReason": "end_turn" })));
        let mut session = GrokSession::start_mock(dir.path().to_path_buf(), lines, timeout())
            .await
            .unwrap();
        session.inject_prompt("write", timeout()).await.unwrap();
        assert!(!file.exists());
        assert!(!outside.path().join("pwned.txt").exists());
        let written = session.mock_written().unwrap();
        let reply = written
            .iter()
            .map(|s| serde_json::from_str::<Value>(s).unwrap())
            .find(|v| v.get("id") == Some(&json!(44)))
            .expect("junction escape error");
        assert_eq!(reply["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn fs_write_missing_content_is_invalid() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("x.md");
        let mut lines = mock_handshake_ok("sess-noc");
        lines.push(
            json!({
                "jsonrpc": "2.0",
                "id": 43,
                "method": "fs/write_text_file",
                "params": { "sessionId": "sess-noc", "path": file.to_str().unwrap() }
            })
            .to_string(),
        );
        lines.push(rpc_result(4, json!({ "stopReason": "end_turn" })));
        let mut session = GrokSession::start_mock(dir.path().to_path_buf(), lines, timeout())
            .await
            .unwrap();
        session.inject_prompt("write", timeout()).await.unwrap();
        assert!(!file.exists());
        let written = session.mock_written().unwrap();
        let reply = written
            .iter()
            .map(|s| serde_json::from_str::<Value>(s).unwrap())
            .find(|v| v.get("id") == Some(&json!(43)))
            .expect("missing content error");
        assert_eq!(reply["error"]["code"], -32602);
    }

    #[test]
    fn slice_text_lines_line_and_limit() {
        let text = "a\nb\nc\nd\n";
        assert_eq!(slice_text_lines(text, Some(2), Some(2)), "b\nc\n");
        assert_eq!(slice_text_lines(text, Some(1), None), text);
        assert_eq!(slice_text_lines(text, Some(10), Some(2)), "");
        assert_eq!(slice_text_lines(text, None, Some(1)), "a\n");
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
