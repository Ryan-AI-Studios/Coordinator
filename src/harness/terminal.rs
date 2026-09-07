//! ACP `terminal/*` client methods (we advertise `clientCapabilities.terminal`).
//!
//! Unanswered `terminal/create` hangs Grok `run_terminal_command` the same way
//! unanswered `fs/read_text_file` hung `read_file`.
//!
//! Track **0032**: Windows empty-`args` wrapper is `pwsh` → Windows PowerShell 5.1
//! → `cmd.exe` (first existing file). 100% `cmd.spawn()` fail this prompt is
//! `HarnessCrash`, not Success.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::sync::{Mutex as TokioMutex, Notify};

use super::grok::rpc_error_value;

const HOST_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const HOST_PROBE_LINE: &str = "echo coordinator-spawn-probe";

/// Per-prompt `terminal/create` spawn counters (reset at `session/prompt`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpawnTally {
    pub attempts: u64,
    pub ok: u64,
    pub last_error: Option<String>,
}

impl SpawnTally {
    pub fn all_failed(&self) -> bool {
        self.attempts > 0 && self.ok == 0
    }
}

#[derive(Clone)]
pub struct TerminalHub {
    next_id: Arc<AtomicU64>,
    inner: Arc<TokioMutex<HashMap<String, LiveTerm>>>,
    spawn_attempts: Arc<AtomicU64>,
    spawn_ok: Arc<AtomicU64>,
    last_spawn_error: Arc<Mutex<Option<String>>>,
    probe_ok: Arc<AtomicBool>,
}

impl std::fmt::Debug for TerminalHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalHub").finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct LiveTerm {
    output: Arc<TokioMutex<String>>,
    truncated: Arc<AtomicBool>,
    exit: Arc<TokioMutex<Option<TermExit>>>,
    done: Arc<Notify>,
    kill: Arc<TokioMutex<Option<tokio::sync::oneshot::Sender<()>>>>,
}

#[derive(Clone, Copy, Debug)]
struct TermExit {
    exit_code: Option<i64>,
    signal: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalMethod {
    Create,
    Output,
    WaitForExit,
    Kill,
    Release,
}

impl TerminalHub {
    pub fn new() -> Self {
        Self {
            next_id: Arc::new(AtomicU64::new(1)),
            inner: Arc::new(TokioMutex::new(HashMap::new())),
            spawn_attempts: Arc::new(AtomicU64::new(0)),
            spawn_ok: Arc::new(AtomicU64::new(0)),
            last_spawn_error: Arc::new(Mutex::new(None)),
            probe_ok: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn reset_prompt_counters(&self) {
        self.spawn_attempts.store(0, Ordering::SeqCst);
        self.spawn_ok.store(0, Ordering::SeqCst);
        if let Ok(mut g) = self.last_spawn_error.lock() {
            *g = None;
        }
    }

    pub fn spawn_tally(&self) -> SpawnTally {
        SpawnTally {
            attempts: self.spawn_attempts.load(Ordering::SeqCst),
            ok: self.spawn_ok.load(Ordering::SeqCst),
            last_error: self.last_spawn_error.lock().ok().and_then(|g| g.clone()),
        }
    }

    /// Host-side shell probe (no LLM). Cached on first success.
    pub async fn probe_host_shell(&self) -> std::result::Result<(), String> {
        if self.probe_ok.load(Ordering::SeqCst) {
            return Ok(());
        }
        let mut cmd = spawn_command(HOST_PROBE_LINE, &[]);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .current_dir(std::env::temp_dir());
        #[cfg(windows)]
        {
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd.spawn().map_err(|e| {
            format!("terminal host probe spawn: {e} (no Windows shell for terminal/create)")
        })?;
        match tokio::time::timeout(HOST_PROBE_TIMEOUT, child.wait()).await {
            Ok(Ok(status)) if status.success() => {
                self.probe_ok.store(true, Ordering::SeqCst);
                Ok(())
            }
            Ok(Ok(status)) => Err(format!(
                "terminal host probe exit {} (no usable shell for terminal/create)",
                status.code().unwrap_or(-1)
            )),
            Ok(Err(e)) => Err(format!("terminal host probe wait: {e}")),
            Err(_) => {
                let _ = child.start_kill();
                Err("terminal host probe timed out (no usable shell for terminal/create)".into())
            }
        }
    }

    pub fn classify(method: &str) -> Option<TerminalMethod> {
        match method {
            "terminal/create" => Some(TerminalMethod::Create),
            "terminal/output" => Some(TerminalMethod::Output),
            "terminal/wait_for_exit" | "terminal/waitForExit" => Some(TerminalMethod::WaitForExit),
            "terminal/kill" => Some(TerminalMethod::Kill),
            "terminal/release" => Some(TerminalMethod::Release),
            _ => None,
        }
    }

    pub async fn handle_sync(
        &self,
        kind: TerminalMethod,
        req_id: Value,
        params: Option<&Value>,
        default_cwd: &Path,
    ) -> String {
        match kind {
            TerminalMethod::Create => self.create(req_id, params, default_cwd).await,
            TerminalMethod::Output => self.output(req_id, params).await,
            TerminalMethod::Kill => self.kill(req_id, params).await,
            TerminalMethod::Release => self.release(req_id, params).await,
            TerminalMethod::WaitForExit => {
                rpc_error_value(&req_id, "terminal/wait_for_exit must be awaited")
            }
        }
    }

    pub async fn wait_for_exit_reply(&self, req_id: Value, params: Option<&Value>) -> String {
        let Some(tid) = params
            .and_then(|p| p.get("terminalId"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
        else {
            return rpc_error_value(&req_id, "terminal/wait_for_exit missing terminalId");
        };
        let term = {
            let guard = self.inner.lock().await;
            guard.get(&tid).cloned()
        };
        let Some(term) = term else {
            return rpc_error_value(&req_id, "unknown terminalId");
        };
        loop {
            if term.exit.lock().await.is_some() {
                break;
            }
            let notified = term.done.notified();
            if term.exit.lock().await.is_some() {
                break;
            }
            notified.await;
        }
        match *term.exit.lock().await {
            Some(ex) => json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "exitCode": ex.exit_code,
                    "signal": ex.signal
                }
            })
            .to_string(),
            None => rpc_error_value(&req_id, "terminal exited without a status"),
        }
    }

    async fn create(&self, req_id: Value, params: Option<&Value>, default_cwd: &Path) -> String {
        let Some(command) = params
            .and_then(|p| p.get("command"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
        else {
            return rpc_error_value(&req_id, "terminal/create missing command");
        };
        let args: Vec<String> = params
            .and_then(|p| p.get("args"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let cwd = params
            .and_then(|p| p.get("cwd"))
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .unwrap_or_else(|| default_cwd.to_path_buf());
        if let Some(raw) = params.and_then(|p| p.get("cwd")).and_then(|v| v.as_str())
            && !Path::new(raw).is_absolute()
        {
            return rpc_error_value(&req_id, "terminal/create cwd must be absolute");
        }
        let limit = params
            .and_then(|p| p.get("outputByteLimit"))
            .and_then(|v| v.as_u64())
            .unwrap_or(1_048_576) as usize;
        let env_pairs: Vec<(String, String)> = params
            .and_then(|p| p.get("env"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| {
                        let name = e.get("name")?.as_str()?.to_string();
                        let value = e.get("value")?.as_str()?.to_string();
                        Some((name, value))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut cmd = spawn_command(&command, &args);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .current_dir(&cwd);
        for (k, v) in env_pairs {
            cmd.env(k, v);
        }
        #[cfg(windows)]
        {
            cmd.creation_flags(0x0800_0000);
        }
        self.spawn_attempts.fetch_add(1, Ordering::SeqCst);
        let mut child = match cmd.spawn() {
            Ok(c) => {
                self.spawn_ok.fetch_add(1, Ordering::SeqCst);
                c
            }
            Err(e) => {
                let msg = format!("terminal/create spawn: {e}");
                if let Ok(mut g) = self.last_spawn_error.lock() {
                    *g = Some(msg.clone());
                }
                return rpc_error_value(&req_id, &msg);
            }
        };

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (kill_tx, kill_rx) = tokio::sync::oneshot::channel();
        let term = LiveTerm {
            output: Arc::new(TokioMutex::new(String::new())),
            truncated: Arc::new(AtomicBool::new(false)),
            exit: Arc::new(TokioMutex::new(None)),
            done: Arc::new(Notify::new()),
            kill: Arc::new(TokioMutex::new(Some(kill_tx))),
        };
        if let Some(out) = stdout {
            spawn_reader(out, term.output.clone(), term.truncated.clone(), limit);
        }
        if let Some(err) = stderr {
            spawn_reader(err, term.output.clone(), term.truncated.clone(), limit);
        }
        let waiter = term.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status,
                _ = kill_rx => {
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            let ex = match status {
                Ok(s) => TermExit {
                    exit_code: s.code().map(|c| c as i64),
                    signal: None,
                },
                Err(_) => TermExit {
                    exit_code: None,
                    signal: Some("error"),
                },
            };
            *waiter.exit.lock().await = Some(ex);
            waiter.done.notify_waiters();
        });

        let tid = format!("term-{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        self.inner.lock().await.insert(tid.clone(), term);
        json!({"jsonrpc":"2.0","id":req_id,"result":{"terminalId":tid}}).to_string()
    }

    async fn output(&self, req_id: Value, params: Option<&Value>) -> String {
        let Some(tid) = params
            .and_then(|p| p.get("terminalId"))
            .and_then(|v| v.as_str())
        else {
            return rpc_error_value(&req_id, "terminal/output missing terminalId");
        };
        let term = {
            let guard = self.inner.lock().await;
            guard.get(tid).cloned()
        };
        let Some(term) = term else {
            return rpc_error_value(&req_id, "unknown terminalId");
        };
        let output = term.output.lock().await.clone();
        let truncated = term.truncated.load(Ordering::SeqCst);
        let mut result = json!({
            "output": output,
            "truncated": truncated
        });
        if let Some(ex) = *term.exit.lock().await {
            result["exitStatus"] = json!({
                "exitCode": ex.exit_code,
                "signal": ex.signal
            });
        }
        json!({"jsonrpc":"2.0","id":req_id,"result":result}).to_string()
    }

    async fn kill(&self, req_id: Value, params: Option<&Value>) -> String {
        let Some(tid) = params
            .and_then(|p| p.get("terminalId"))
            .and_then(|v| v.as_str())
        else {
            return rpc_error_value(&req_id, "terminal/kill missing terminalId");
        };
        let term = {
            let guard = self.inner.lock().await;
            guard.get(tid).cloned()
        };
        let Some(term) = term else {
            return rpc_error_value(&req_id, "unknown terminalId");
        };
        if let Some(tx) = term.kill.lock().await.take() {
            let _ = tx.send(());
        }
        json!({"jsonrpc":"2.0","id":req_id,"result":{}}).to_string()
    }

    async fn release(&self, req_id: Value, params: Option<&Value>) -> String {
        let Some(tid) = params
            .and_then(|p| p.get("terminalId"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
        else {
            return rpc_error_value(&req_id, "terminal/release missing terminalId");
        };
        let term = self.inner.lock().await.remove(&tid);
        if let Some(term) = term
            && let Some(tx) = term.kill.lock().await.take()
        {
            let _ = tx.send(());
        }
        json!({"jsonrpc":"2.0","id":req_id,"result":{}}).to_string()
    }
}

impl Default for TerminalHub {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpawnPlan {
    pub program: PathBuf,
    pub args: Vec<String>,
}

fn spawn_command(command: &str, args: &[String]) -> tokio::process::Command {
    let plan = plan_spawn(command, args);
    let mut cmd = tokio::process::Command::new(&plan.program);
    cmd.args(&plan.args);
    cmd
}

pub(crate) fn plan_spawn(command: &str, args: &[String]) -> SpawnPlan {
    if !args.is_empty() {
        return SpawnPlan {
            program: PathBuf::from(command),
            args: args.to_vec(),
        };
    }
    // Grok `run_terminal_command` is one shell line; ACP `args` may be omitted.
    #[cfg(windows)]
    {
        if let Some(plan) = explicit_windows_exe(command) {
            return plan;
        }
        wrap_windows_shell(command)
    }
    #[cfg(not(windows))]
    {
        SpawnPlan {
            program: PathBuf::from("sh"),
            args: vec!["-c".into(), command.to_string()],
        }
    }
}

/// Doctor detail when `pwsh` is missing. Never flips a required row to refuse `run`.
pub fn grok_terminal_shell_detail() -> Option<String> {
    #[cfg(not(windows))]
    {
        return None;
    }
    #[cfg(windows)]
    {
        shell_detail_from(pwsh_resolvable(), fallback_shell_resolvable())
    }
}

pub(crate) fn shell_detail_from(pwsh: bool, fallback: bool) -> Option<String> {
    if pwsh {
        None
    } else if fallback {
        Some("pwsh not resolvable; terminal/create falls back to powershell.exe or cmd.exe".into())
    } else {
        Some("no Windows shell for terminal/create (pwsh/powershell.exe/cmd.exe)".into())
    }
}

#[cfg(windows)]
fn wrap_windows_shell(command: &str) -> SpawnPlan {
    let shell = resolve_windows_shell().unwrap_or_else(|| PathBuf::from("cmd.exe"));
    let stem = shell
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if stem == "cmd" {
        SpawnPlan {
            program: shell,
            args: vec!["/C".into(), command.to_string()],
        }
    } else {
        SpawnPlan {
            program: shell,
            args: vec!["-NoProfile".into(), "-Command".into(), command.to_string()],
        }
    }
}

#[cfg(windows)]
fn explicit_windows_exe(command: &str) -> Option<SpawnPlan> {
    let (token, rest) = first_token(command)?;
    let path = Path::new(token);
    let resolved = if path.is_file() {
        Some(path.to_path_buf())
    } else if is_known_host(token) {
        path_lookup(token).or_else(|| well_known_for_host(token))
    } else {
        None
    }?;
    Some(SpawnPlan {
        program: resolved,
        args: remainder_args(rest),
    })
}

#[cfg(windows)]
fn first_token(command: &str) -> Option<(&str, &str)> {
    let s = command.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(inner) = s.strip_prefix('"') {
        let end = inner.find('"')?;
        let token = &inner[..end];
        let remainder = inner[end + 1..].trim_start();
        return Some((token, remainder));
    }
    let end = s.find([' ', '\t']).unwrap_or(s.len());
    Some((&s[..end], s[end..].trim_start()))
}

#[cfg(windows)]
fn remainder_args(rest: &str) -> Vec<String> {
    rest.split([' ', '\t'])
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(windows)]
fn is_known_host(token: &str) -> bool {
    matches!(
        token.to_ascii_lowercase().as_str(),
        "cmd.exe" | "powershell.exe" | "pwsh" | "pwsh.exe"
    )
}

#[cfg(windows)]
pub(crate) fn resolve_windows_shell() -> Option<PathBuf> {
    resolve_windows_shell_from(&windows_shell_candidates())
}

#[cfg(windows)]
pub(crate) fn resolve_windows_shell_from(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|p| p.is_file()).cloned()
}

#[cfg(windows)]
pub(crate) fn windows_shell_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = path_lookup("pwsh") {
        out.push(p);
    }
    if let Some(pf) = std::env::var_os("ProgramFiles").map(PathBuf::from) {
        out.push(pf.join("PowerShell").join("7").join("pwsh.exe"));
        out.push(pf.join("PowerShell").join("7-preview").join("pwsh.exe"));
    }
    if let Some(root) = std::env::var_os("SystemRoot").map(PathBuf::from) {
        out.push(
            root.join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe"),
        );
    }
    if let Some(p) = path_lookup("powershell.exe") {
        out.push(p);
    }
    if let Some(root) = std::env::var_os("SystemRoot").map(PathBuf::from) {
        out.push(root.join("System32").join("cmd.exe"));
    }
    if let Some(p) = path_lookup("cmd.exe") {
        out.push(p);
    }
    out
}

#[cfg(windows)]
fn pwsh_resolvable() -> bool {
    windows_shell_candidates().into_iter().any(|p| {
        p.is_file() && {
            let n = p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            n == "pwsh.exe" || n == "pwsh"
        }
    })
}

#[cfg(windows)]
fn fallback_shell_resolvable() -> bool {
    windows_shell_candidates().into_iter().any(|p| {
        p.is_file() && {
            let n = p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            n == "powershell.exe" || n == "cmd.exe"
        }
    })
}

#[cfg(windows)]
fn well_known_for_host(token: &str) -> Option<PathBuf> {
    let key = token.to_ascii_lowercase();
    let root = std::env::var_os("SystemRoot").map(PathBuf::from)?;
    let pf = std::env::var_os("ProgramFiles").map(PathBuf::from);
    let cand: Vec<PathBuf> = match key.as_str() {
        "cmd.exe" => vec![root.join("System32").join("cmd.exe")],
        "powershell.exe" => vec![
            root.join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe"),
        ],
        "pwsh" | "pwsh.exe" => pf
            .map(|pf| {
                vec![
                    pf.join("PowerShell").join("7").join("pwsh.exe"),
                    pf.join("PowerShell").join("7-preview").join("pwsh.exe"),
                ]
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    cand.into_iter().find(|p| p.is_file())
}

#[cfg(windows)]
fn path_lookup(command: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let has_ext = Path::new(command)
        .extension()
        .is_some_and(|e| !e.is_empty());
    let exts: Vec<String> = if cfg!(windows) && !has_ext {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into())
            .split(';')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    } else {
        Vec::new()
    };
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(command);
        if candidate.is_file() {
            return Some(candidate);
        }
        for ext in &exts {
            let with_ext = dir.join(format!("{command}{ext}"));
            if with_ext.is_file() {
                return Some(with_ext);
            }
        }
    }
    None
}

fn spawn_reader<R>(
    reader: R,
    output: Arc<TokioMutex<String>>,
    truncated: Arc<AtomicBool>,
    limit: usize,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = BufReader::new(reader);
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]);
                    let mut out = output.lock().await;
                    push_output(&mut out, &truncated, limit, &chunk);
                }
            }
        }
    });
}

fn push_output(buf: &mut String, truncated: &AtomicBool, limit: usize, chunk: &str) {
    if limit == 0 {
        truncated.store(true, Ordering::SeqCst);
        buf.clear();
        return;
    }
    buf.push_str(chunk);
    if buf.len() <= limit {
        return;
    }
    truncated.store(true, Ordering::SeqCst);
    let mut cut = buf.len() - limit;
    while cut < buf.len() && !buf.is_char_boundary(cut) {
        cut += 1;
    }
    buf.replace_range(..cut, "");
}

/// Agent → client `terminal/create`.
pub fn terminal_create(id: u64, session_id: &str, command: &str, args: &[&str]) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "terminal/create",
        "params": {
            "sessionId": session_id,
            "command": command,
            "args": args
        }
    })
    .to_string()
}

pub fn terminal_output(id: u64, session_id: &str, terminal_id: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "terminal/output",
        "params": { "sessionId": session_id, "terminalId": terminal_id }
    })
    .to_string()
}

pub fn terminal_wait_for_exit(id: u64, session_id: &str, terminal_id: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "terminal/wait_for_exit",
        "params": { "sessionId": session_id, "terminalId": terminal_id }
    })
    .to_string()
}

pub fn terminal_release(id: u64, session_id: &str, terminal_id: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "terminal/release",
        "params": { "sessionId": session_id, "terminalId": terminal_id }
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_accepts_snake_and_camel_wait() {
        assert_eq!(
            TerminalHub::classify("terminal/wait_for_exit"),
            Some(TerminalMethod::WaitForExit)
        );
        assert_eq!(
            TerminalHub::classify("terminal/waitForExit"),
            Some(TerminalMethod::WaitForExit)
        );
        assert!(TerminalHub::classify("session/update").is_none());
    }

    #[test]
    fn push_output_truncates_from_start_on_char_boundary() {
        let mut buf = String::new();
        let flag = AtomicBool::new(false);
        push_output(&mut buf, &flag, 4, "abcdef");
        assert!(flag.load(Ordering::SeqCst));
        assert_eq!(buf, "cdef");
        push_output(&mut buf, &flag, 4, "gh");
        assert_eq!(buf, "efgh");
    }

    #[tokio::test]
    async fn create_echo_wait_and_output() {
        let hub = TerminalHub::new();
        let cwd = std::env::temp_dir();
        let (command, args) = echo_args();
        let create = serde_json::from_str::<Value>(
            &hub.handle_sync(
                TerminalMethod::Create,
                json!(1),
                Some(&json!({"command": command, "args": args})),
                &cwd,
            )
            .await,
        )
        .unwrap();
        let tid = create["result"]["terminalId"].as_str().unwrap().to_string();
        let wait = serde_json::from_str::<Value>(
            &hub.wait_for_exit_reply(json!(2), Some(&json!({"terminalId": tid})))
                .await,
        )
        .unwrap();
        assert_eq!(wait["result"]["exitCode"], 0);
        let out = serde_json::from_str::<Value>(
            &hub.handle_sync(
                TerminalMethod::Output,
                json!(3),
                Some(&json!({"terminalId": tid})),
                &cwd,
            )
            .await,
        )
        .unwrap();
        let text = out["result"]["output"].as_str().unwrap();
        assert!(text.to_ascii_lowercase().contains("hi"), "output={text:?}");
        let rel = serde_json::from_str::<Value>(
            &hub.handle_sync(
                TerminalMethod::Release,
                json!(4),
                Some(&json!({"terminalId": tid})),
                &cwd,
            )
            .await,
        )
        .unwrap();
        assert!(rel.get("result").is_some());
    }

    fn echo_args() -> (&'static str, Vec<&'static str>) {
        if cfg!(windows) {
            ("cmd.exe", vec!["/C", "echo hi"])
        } else {
            ("echo", vec!["hi"])
        }
    }

    #[tokio::test]
    async fn empty_args_echo_probe_exits_zero() {
        let hub = TerminalHub::new();
        let cwd = std::env::temp_dir();
        let create = serde_json::from_str::<Value>(
            &hub.handle_sync(
                TerminalMethod::Create,
                json!(1),
                Some(&json!({"command": HOST_PROBE_LINE, "args": []})),
                &cwd,
            )
            .await,
        )
        .unwrap();
        assert!(create.get("result").is_some(), "create={create}");
        let tid = create["result"]["terminalId"].as_str().unwrap().to_string();
        let wait = serde_json::from_str::<Value>(
            &hub.wait_for_exit_reply(json!(2), Some(&json!({"terminalId": tid})))
                .await,
        )
        .unwrap();
        assert_eq!(wait["result"]["exitCode"], 0, "wait={wait}");
        assert_eq!(hub.spawn_tally().ok, 1);
    }

    #[test]
    fn spawn_err_formats_terminal_create_spawn() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let hub = TerminalHub::new();
            let cwd = std::env::temp_dir();
            let reply = serde_json::from_str::<Value>(
                &hub.handle_sync(
                    TerminalMethod::Create,
                    json!(1),
                    Some(&json!({
                        "command": "__coord_no_such_bin_0032__",
                        "args": ["x"]
                    })),
                    &cwd,
                )
                .await,
            )
            .unwrap();
            let msg = reply["error"]["message"].as_str().unwrap();
            assert!(msg.contains("terminal/create spawn:"), "msg={msg}");
            let tally = hub.spawn_tally();
            assert_eq!(tally.attempts, 1);
            assert_eq!(tally.ok, 0);
            assert!(tally.all_failed());
            assert!(
                tally
                    .last_error
                    .as_deref()
                    .is_some_and(|e| e.contains("terminal/create spawn:")),
                "tally={tally:?}"
            );
        });
    }

    #[test]
    fn shell_detail_warns_only_when_pwsh_missing() {
        assert!(shell_detail_from(true, true).is_none());
        assert!(shell_detail_from(true, false).is_none());
        let warn = shell_detail_from(false, true).unwrap();
        assert!(warn.contains("pwsh not resolvable"));
        assert!(warn.contains("powershell.exe"));
        let none = shell_detail_from(false, false).unwrap();
        assert!(none.contains("no Windows shell"));
    }

    #[tokio::test]
    async fn host_probe_echo_succeeds() {
        let hub = TerminalHub::new();
        hub.probe_host_shell().await.expect("host probe");
        hub.probe_host_shell().await.expect("cached probe");
    }

    #[cfg(windows)]
    #[test]
    fn resolver_without_pwsh_picks_powershell_or_cmd() {
        let filtered: Vec<PathBuf> = windows_shell_candidates()
            .into_iter()
            .filter(|p| {
                let n = p
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                n != "pwsh.exe" && n != "pwsh"
            })
            .collect();
        let got = resolve_windows_shell_from(&filtered).expect("fallback shell");
        assert!(got.is_file(), "got={}", got.display());
        let n = got
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        assert!(
            n == "powershell.exe" || n == "cmd.exe",
            "got={}",
            got.display()
        );
        let plan = wrap_windows_shell("echo coordinator-spawn-probe");
        assert!(plan.program.is_file() || plan.program == Path::new("cmd.exe"));
        assert_ne!(
            plan.program.file_name().and_then(|s| s.to_str()),
            Some("pwsh")
        );
    }

    #[cfg(windows)]
    #[test]
    fn empty_args_explicit_cmd_exe_is_not_rewrapped() {
        let cmd = PathBuf::from(r"C:\Windows\System32\cmd.exe");
        assert!(cmd.is_file(), "stock cmd.exe missing");
        let line = format!("{} /c echo hi", cmd.display());
        let plan = plan_spawn(&line, &[]);
        assert_eq!(plan.program, cmd);
        assert_eq!(plan.args, vec!["/c", "echo", "hi"]);
        let quoted = format!("\"{}\" /c echo hi", cmd.display());
        let plan_q = plan_spawn(&quoted, &[]);
        assert_eq!(plan_q.program, cmd);
        let known = plan_spawn("cmd.exe /c echo hi", &[]);
        assert_eq!(
            known
                .program
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap()
                .to_ascii_lowercase(),
            "cmd.exe"
        );
        assert_eq!(known.args, vec!["/c", "echo", "hi"]);
        let wrapped = plan_spawn("echo hi", &[]);
        let wrap_name = wrapped
            .program
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        assert!(
            wrap_name == "pwsh.exe" || wrap_name == "powershell.exe" || wrap_name == "cmd.exe",
            "wrap={}",
            wrapped.program.display()
        );
        assert!(
            wrapped.args.contains(&"echo hi".into())
                || wrapped.args.windows(2).any(|w| w == ["echo", "hi"]),
            "args={:?}",
            wrapped.args
        );
    }
}
