//! ACP `terminal/*` client methods (we advertise `clientCapabilities.terminal`).
//!
//! Unanswered `terminal/create` hangs Grok `run_terminal_command` the same way
//! unanswered `fs/read_text_file` hung `read_file`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::sync::{Mutex as TokioMutex, Notify};

use super::grok::rpc_error_value;

#[derive(Clone)]
pub struct TerminalHub {
    next_id: Arc<AtomicU64>,
    inner: Arc<TokioMutex<HashMap<String, LiveTerm>>>,
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
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return rpc_error_value(&req_id, &format!("terminal/create spawn: {e}")),
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

fn spawn_command(command: &str, args: &[String]) -> tokio::process::Command {
    if !args.is_empty() {
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args);
        return cmd;
    }
    // Grok `run_terminal_command` is one shell line; ACP `args` may be omitted.
    if cfg!(windows) {
        let mut cmd = tokio::process::Command::new("pwsh");
        cmd.args(["-NoProfile", "-Command", command]);
        cmd
    } else {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", command]);
        cmd
    }
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
}
