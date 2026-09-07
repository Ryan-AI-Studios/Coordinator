//! ACP `terminal/*` client methods (we advertise `clientCapabilities.terminal`).
//!
//! Unanswered `terminal/create` hangs Grok `run_terminal_command` the same way
//! unanswered `fs/read_text_file` hung `read_file`.
//!
//! Track **0032**: Windows empty-`args` wrapper is `pwsh` → Windows PowerShell 5.1
//! → `cmd.exe` (first existing file). 100% `cmd.spawn()` fail this prompt is
//! `HarnessCrash`, not Success.
//!
//! Track **0034**: bound hubs journal child completion / spawn-fail as JSONL.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::journal::{self, JournalEvent, JournalHub, JournalSnap};

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
    record: Arc<Mutex<Option<crate::registry::ProjectRecord>>>,
    journal: Arc<Mutex<JournalHub>>,
    running: Arc<AtomicU64>,
    wait_count: Arc<AtomicU64>,
    tool_ids: Arc<Mutex<HashSet<String>>>,
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
            record: Arc::new(Mutex::new(None)),
            journal: Arc::new(Mutex::new(JournalHub::new())),
            running: Arc::new(AtomicU64::new(0)),
            wait_count: Arc::new(AtomicU64::new(0)),
            tool_ids: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Bind the project so child completions write `{state_dir}/journal/`. Unbound = no-op.
    pub fn bind_record(&self, record: crate::registry::ProjectRecord) {
        if let Ok(mut g) = self.record.lock() {
            *g = Some(record);
        }
    }

    pub fn reset_prompt_counters(&self) {
        self.spawn_attempts.store(0, Ordering::SeqCst);
        self.spawn_ok.store(0, Ordering::SeqCst);
        if let Ok(mut g) = self.last_spawn_error.lock() {
            *g = None;
        }
        if let Ok(mut g) = self.journal.lock() {
            g.reset();
        }
    }

    pub fn spawn_tally(&self) -> SpawnTally {
        SpawnTally {
            attempts: self.spawn_attempts.load(Ordering::SeqCst),
            ok: self.spawn_ok.load(Ordering::SeqCst),
            last_error: self.last_spawn_error.lock().ok().and_then(|g| g.clone()),
        }
    }

    fn create_snapshot(&self, command: &str) -> Option<JournalSnap> {
        if command == HOST_PROBE_LINE {
            return None;
        }
        let record = self.record.lock().ok().and_then(|g| g.clone())?;
        Some(journal::snapshot(&record))
    }

    pub fn inc_wait(&self) {
        self.wait_count.fetch_add(1, Ordering::SeqCst);
    }

    pub fn dec_wait(&self) {
        let _ = self
            .wait_count
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                Some(n.saturating_sub(1))
            });
        self.clear_tool_ids_if_idle();
        self.note_bound_progress();
    }

    pub fn tool_in_flight(&self) -> bool {
        let ids = self.tool_ids.lock().map(|g| !g.is_empty()).unwrap_or(false);
        ids || self.running.load(Ordering::SeqCst) > 0 || self.wait_count.load(Ordering::SeqCst) > 0
    }

    pub fn apply_acp_tool_update(&self, msg: &serde_json::Value) {
        let Some(update) = msg.get("params").and_then(|p| p.get("update")) else {
            return;
        };
        let kind = update.get("sessionUpdate").and_then(|s| s.as_str());
        if kind != Some("tool_call") && kind != Some("tool_call_update") {
            return;
        }
        let Some(id) = update.get("toolCallId").and_then(|s| s.as_str()) else {
            return;
        };
        let status = update.get("status").and_then(|s| s.as_str());
        if let Ok(mut g) = self.tool_ids.lock() {
            match status {
                None | Some("pending") | Some("in_progress") => {
                    g.insert(id.to_string());
                }
                Some("completed") | Some("failed") => {
                    g.remove(id);
                }
                _ => {}
            }
        }
    }

    fn clear_tool_ids_if_idle(&self) {
        if self.running.load(Ordering::SeqCst) == 0
            && self.wait_count.load(Ordering::SeqCst) == 0
            && let Ok(mut g) = self.tool_ids.lock()
        {
            g.clear();
        }
    }

    fn note_bound_progress(&self) {
        let Some(rec) = self.record.lock().ok().and_then(|g| g.clone()) else {
            return;
        };
        crate::workflow::watchdog::note_progress(
            &rec,
            crate::workflow::watchdog::ProgressKind::SessionUpdate,
            None,
            self.tool_in_flight(),
        );
    }

    fn journal_spawn_fail(&self, snap: Option<&JournalSnap>, argv: &str) {
        let Some(snap) = snap else {
            return;
        };
        if let Ok(mut g) = self.journal.lock() {
            g.record(JournalEvent {
                snap,
                argv_head: argv,
                exit: None,
                dur_ms: 0,
                ok: false,
                signal: None,
                spawn_fail: true,
            });
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
        let snap = self.create_snapshot(&command);
        let argv = journal::argv_head(&command, &args);
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
                self.journal_spawn_fail(snap.as_ref(), &argv);
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
        self.running.fetch_add(1, Ordering::SeqCst);
        let waiter = term.clone();
        let journal = self.journal.clone();
        let hub = self.clone();
        let started = Instant::now();
        tokio::spawn(async move {
            let mut killed = false;
            let status = tokio::select! {
                status = child.wait() => status,
                _ = kill_rx => {
                    killed = true;
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            let (exit_code, acp_signal) = match status {
                Ok(s) => (s.code().map(|c| c as i64), None),
                Err(_) => (None, Some("error")),
            };
            let ex = TermExit {
                exit_code,
                signal: acp_signal,
            };
            if let Some(ref snap) = snap
                && let Ok(mut g) = journal.lock()
            {
                let journal_signal = if killed { Some("killed") } else { None };
                g.record(JournalEvent {
                    snap,
                    argv_head: &argv,
                    exit: exit_code,
                    dur_ms: started.elapsed().as_millis() as u64,
                    ok: exit_code == Some(0),
                    signal: journal_signal,
                    spawn_fail: false,
                });
            }
            let _ = hub
                .running
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                    Some(n.saturating_sub(1))
                });
            hub.clear_tool_ids_if_idle();
            hub.note_bound_progress();
            *waiter.exit.lock().await = Some(ex);
            waiter.done.notify_waiters();
        });
        self.note_bound_progress();

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

    fn fail_args() -> (&'static str, Vec<&'static str>) {
        if cfg!(windows) {
            ("cmd.exe", vec!["/C", "exit 1"])
        } else {
            ("sh", vec!["-c", "exit 1"])
        }
    }

    fn hang_args() -> (&'static str, Vec<&'static str>) {
        if cfg!(windows) {
            ("cmd.exe", vec!["/C", "ping", "-n", "60", "127.0.0.1"])
        } else {
            ("sleep", vec!["60"])
        }
    }

    fn journal_test_rec(ws: &Path) -> crate::registry::ProjectRecord {
        crate::registry::ProjectRecord {
            id: "j34-term".into(),
            path: ws.to_path_buf(),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: std::collections::BTreeMap::new(),
            state_dir: Some(ws.join("state")),
            auto_merge: true,
            phase_timeouts_secs: std::collections::BTreeMap::new(),
            notify_progress: false,
            created_at: chrono::Utc::now(),
        }
    }

    fn seed_journal_run(rec: &crate::registry::ProjectRecord) {
        let mut s = crate::state::RunState::idle(&rec.id);
        s.phase = "implement".into();
        s.track_id = Some("0034".into());
        s.run_epoch = 7;
        crate::state::save_run_state(rec, &s).unwrap();
    }

    fn journal_path(rec: &crate::registry::ProjectRecord) -> PathBuf {
        rec.state_dir
            .as_ref()
            .unwrap()
            .join("journal")
            .join("0034-7.jsonl")
    }

    fn read_journal(rec: &crate::registry::ProjectRecord) -> Vec<Value> {
        let text = std::fs::read_to_string(journal_path(rec)).unwrap();
        text.lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str::<Value>(l).expect(l))
            .collect()
    }

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    async fn create_and_wait(hub: &TerminalHub, command: &str, args: &[&str]) -> Value {
        let cwd = std::env::temp_dir();
        let args: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
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
        if let Some(tid) = create
            .get("result")
            .and_then(|r| r.get("terminalId"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
        {
            let _ = hub
                .wait_for_exit_reply(json!(2), Some(&json!({"terminalId": tid})))
                .await;
        }
        create
    }

    #[test]
    fn bound_echo_writes_one_journal_line() {
        let _lock = crate::config::test_env_lock();
        block_on(async {
            let home = tempfile::tempdir().unwrap();
            let ws = tempfile::tempdir().unwrap();
            unsafe {
                std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
                std::env::remove_var(crate::config::ENV_COORDINATOR_STATE_DIR);
                std::env::remove_var(journal::ENV_COORDINATOR_JOURNAL);
            }
            let rec = journal_test_rec(ws.path());
            seed_journal_run(&rec);
            let hub = TerminalHub::new();
            hub.bind_record(rec.clone());
            let (command, args) = echo_args();
            let create = create_and_wait(&hub, command, &args).await;
            assert!(create.get("result").is_some(), "create={create}");
            let lines = read_journal(&rec);
            assert_eq!(lines.len(), 1);
            let v = &lines[0];
            assert!(v.get("env").is_none());
            assert_eq!(v["harness"], "grok");
            assert_eq!(v["phase"], "implement");
            assert_eq!(v["ok"], true);
            assert_eq!(v["exit"], 0);
            assert!(v.get("signal").is_none());
            assert!(v["argv_head"].as_str().unwrap().contains("echo"));
            unsafe {
                std::env::remove_var(crate::config::ENV_COORDINATOR_HOME);
            }
        });
    }

    #[test]
    fn unbound_create_does_not_write_journal() {
        let _lock = crate::config::test_env_lock();
        block_on(async {
            let home = tempfile::tempdir().unwrap();
            let ws = tempfile::tempdir().unwrap();
            unsafe {
                std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
                std::env::remove_var(crate::config::ENV_COORDINATOR_STATE_DIR);
                std::env::remove_var(journal::ENV_COORDINATOR_JOURNAL);
            }
            let rec = journal_test_rec(ws.path());
            seed_journal_run(&rec);
            let hub = TerminalHub::new();
            let (command, args) = echo_args();
            let create = create_and_wait(&hub, command, &args).await;
            assert!(create.get("result").is_some(), "create={create}");
            assert!(!journal_path(&rec).exists());
            unsafe {
                std::env::remove_var(crate::config::ENV_COORDINATOR_HOME);
            }
        });
    }

    #[test]
    fn journal_off_bound_create_writes_nothing() {
        let _lock = crate::config::test_env_lock();
        block_on(async {
            let home = tempfile::tempdir().unwrap();
            let ws = tempfile::tempdir().unwrap();
            unsafe {
                std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
                std::env::remove_var(crate::config::ENV_COORDINATOR_STATE_DIR);
                std::env::set_var(journal::ENV_COORDINATOR_JOURNAL, "off");
            }
            let rec = journal_test_rec(ws.path());
            seed_journal_run(&rec);
            let hub = TerminalHub::new();
            hub.bind_record(rec.clone());
            let (command, args) = echo_args();
            let create = create_and_wait(&hub, command, &args).await;
            assert!(create.get("result").is_some(), "create={create}");
            assert!(!journal_path(&rec).exists());
            unsafe {
                std::env::remove_var(journal::ENV_COORDINATOR_JOURNAL);
                std::env::remove_var(crate::config::ENV_COORDINATOR_HOME);
            }
        });
    }

    #[test]
    fn spawn_missing_program_journals_one_fail_line() {
        let _lock = crate::config::test_env_lock();
        block_on(async {
            let home = tempfile::tempdir().unwrap();
            let ws = tempfile::tempdir().unwrap();
            unsafe {
                std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
                std::env::remove_var(crate::config::ENV_COORDINATOR_STATE_DIR);
                std::env::remove_var(journal::ENV_COORDINATOR_JOURNAL);
            }
            let rec = journal_test_rec(ws.path());
            seed_journal_run(&rec);
            let hub = TerminalHub::new();
            hub.bind_record(rec.clone());
            let create = create_and_wait(&hub, "__coord_no_such_bin_0034__", &["x"]).await;
            assert!(create.get("error").is_some(), "create={create}");
            let lines = read_journal(&rec);
            assert_eq!(lines.len(), 1);
            assert!(lines[0]["exit"].is_null());
            assert_eq!(lines[0]["dur_ms"], 0);
            assert_eq!(lines[0]["ok"], false);
            assert!(lines[0].get("env").is_none());
            unsafe {
                std::env::remove_var(crate::config::ENV_COORDINATOR_HOME);
            }
        });
    }

    #[test]
    fn two_failing_commands_one_loop_suspect() {
        let _lock = crate::config::test_env_lock();
        block_on(async {
            let home = tempfile::tempdir().unwrap();
            let ws = tempfile::tempdir().unwrap();
            unsafe {
                std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
                std::env::remove_var(crate::config::ENV_COORDINATOR_STATE_DIR);
                std::env::remove_var(journal::ENV_COORDINATOR_JOURNAL);
            }
            let rec = journal_test_rec(ws.path());
            seed_journal_run(&rec);
            let hub = TerminalHub::new();
            hub.bind_record(rec.clone());
            let (command, args) = fail_args();
            let _ = create_and_wait(&hub, command, &args).await;
            let _ = create_and_wait(&hub, command, &args).await;
            let _ = create_and_wait(&hub, command, &args).await;
            assert_eq!(read_journal(&rec).len(), 3);
            let status = std::fs::read_to_string(crate::progress_log::path(&rec)).unwrap();
            assert_eq!(status.matches("loop_suspect").count(), 1, "status={status}");
            unsafe {
                std::env::remove_var(crate::config::ENV_COORDINATOR_HOME);
            }
        });
    }

    #[test]
    fn two_kills_journal_killed_without_loop_suspect() {
        let _lock = crate::config::test_env_lock();
        block_on(async {
            let home = tempfile::tempdir().unwrap();
            let ws = tempfile::tempdir().unwrap();
            unsafe {
                std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
                std::env::remove_var(crate::config::ENV_COORDINATOR_STATE_DIR);
                std::env::remove_var(journal::ENV_COORDINATOR_JOURNAL);
            }
            let rec = journal_test_rec(ws.path());
            seed_journal_run(&rec);
            let hub = TerminalHub::new();
            hub.bind_record(rec.clone());
            let cwd = std::env::temp_dir();
            let (command, args) = hang_args();
            let args: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
            for i in 0..2 {
                let create = serde_json::from_str::<Value>(
                    &hub.handle_sync(
                        TerminalMethod::Create,
                        json!(i),
                        Some(&json!({"command": command, "args": args})),
                        &cwd,
                    )
                    .await,
                )
                .unwrap();
                let tid = create["result"]["terminalId"].as_str().unwrap().to_string();
                let kill = serde_json::from_str::<Value>(
                    &hub.handle_sync(
                        TerminalMethod::Kill,
                        json!(100 + i),
                        Some(&json!({"terminalId": tid})),
                        &cwd,
                    )
                    .await,
                )
                .unwrap();
                assert!(kill.get("result").is_some(), "kill={kill}");
                let wait = serde_json::from_str::<Value>(
                    &hub.wait_for_exit_reply(json!(200 + i), Some(&json!({"terminalId": tid})))
                        .await,
                )
                .unwrap();
                // ACP wait_for_exit signal stays unset on Ok(wait); journal-only "killed".
                assert!(
                    wait["result"]["signal"].is_null(),
                    "ACP signal must stay None, wait={wait}"
                );
            }
            let lines = read_journal(&rec);
            assert_eq!(lines.len(), 2);
            assert_eq!(lines[0]["signal"], "killed");
            assert_eq!(lines[1]["signal"], "killed");
            assert_eq!(lines[0]["ok"], false);
            let status_path = crate::progress_log::path(&rec);
            assert!(
                !status_path.exists()
                    || !std::fs::read_to_string(&status_path)
                        .unwrap()
                        .contains("loop_suspect")
            );
            unsafe {
                std::env::remove_var(crate::config::ENV_COORDINATOR_HOME);
            }
        });
    }

    #[test]
    fn waiter_term_exit_clears_tool_in_flight_without_session_update() {
        let _lock = crate::config::test_env_lock();
        block_on(async {
            let home = tempfile::tempdir().unwrap();
            let ws = tempfile::tempdir().unwrap();
            unsafe {
                std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
                std::env::remove_var(crate::config::ENV_COORDINATOR_STATE_DIR);
            }
            let rec = journal_test_rec(ws.path());
            seed_journal_run(&rec);
            crate::state::ensure_state_dir(&rec).unwrap();
            let hub = TerminalHub::new();
            hub.bind_record(rec.clone());
            let (command, args) = hang_args();
            let cwd = std::env::temp_dir();
            let args: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
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
            assert!(hub.tool_in_flight());
            let v: Value = serde_json::from_str(
                &std::fs::read_to_string(crate::workflow::watchdog::progress_path(&rec).unwrap())
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(v["tool_in_flight"], true, "sidecar={v}");
            let _ = hub
                .handle_sync(
                    TerminalMethod::Kill,
                    json!(2),
                    Some(&json!({"terminalId": tid})),
                    &cwd,
                )
                .await;
            let _ = hub
                .wait_for_exit_reply(json!(3), Some(&json!({"terminalId": tid})))
                .await;
            assert!(!hub.tool_in_flight());
            let v: Value = serde_json::from_str(
                &std::fs::read_to_string(crate::workflow::watchdog::progress_path(&rec).unwrap())
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(v["tool_in_flight"], false, "sidecar after exit={v}");
            unsafe {
                std::env::remove_var(crate::config::ENV_COORDINATOR_HOME);
            }
        });
    }

    #[test]
    fn host_probe_does_not_journal() {
        let _lock = crate::config::test_env_lock();
        block_on(async {
            let home = tempfile::tempdir().unwrap();
            let ws = tempfile::tempdir().unwrap();
            unsafe {
                std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
                std::env::remove_var(crate::config::ENV_COORDINATOR_STATE_DIR);
                std::env::remove_var(journal::ENV_COORDINATOR_JOURNAL);
            }
            let rec = journal_test_rec(ws.path());
            seed_journal_run(&rec);
            let hub = TerminalHub::new();
            hub.bind_record(rec.clone());
            hub.probe_host_shell().await.expect("host probe");
            assert!(!journal_path(&rec).exists());
            unsafe {
                std::env::remove_var(crate::config::ENV_COORDINATOR_HOME);
            }
        });
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
