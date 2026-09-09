//! Live one-shot review CLI spawn (Codex / Claude / OpenCode).
//!
//! Per-spawn silence stall + tree-kill (track **0036**): drain stdio in
//! chunked reader threads; kill an alive-idle child after
//! `progress_stall_interval()` with stderr `reviewer stall — no progress for`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use crate::config::{
    ENV_COORDINATOR_CLAUDE_BIN, ENV_COORDINATOR_CODEX_BIN, ENV_COORDINATOR_OPENCODE_BIN,
};
use crate::error::{CoordinatorError, Result};
use crate::harness::grok::reject_or_replace_ps1;
use crate::harness::resolve_command;

use super::backend::{ReviewBackend, ReviewRequest, ReviewResult};
use super::prompt::VERDICT_SCHEMA_JSON;

pub struct LiveCli;

impl ReviewBackend for LiveCli {
    fn run(&self, req: &ReviewRequest) -> Result<ReviewResult> {
        let bin = resolve_review_bin(&req.harness, &req.command)?;
        let tmp = std::env::temp_dir().join(format!(
            "coordinator-review-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&tmp)?;
        let last_path = tmp.join("last-message.txt");
        let schema_path = tmp.join("verdict.schema.json");
        crate::persist::atomic_write(&schema_path, VERDICT_SCHEMA_JSON.as_bytes())?;

        let args = argv_for(req, &last_path, &schema_path);
        // Codex (`exec -`) and OpenCode (`run` with empty positional): prompt on
        // stdin so cmd.exe /C + npm `.cmd` `%*` cannot strip TRACK: / newlines.
        let stdin = if req.harness.eq_ignore_ascii_case("codex")
            || req.harness.eq_ignore_ascii_case("opencode")
        {
            Some(req.prompt.as_bytes())
        } else {
            None
        };
        let mut watch_paths = vec![last_path.clone()];
        if let Some(ref td) = req.track_dir {
            watch_paths.push(td.join(format!("review.{}.md", req.slug)));
        }
        let out = run_process(
            &bin,
            &args,
            &req.exec_repo,
            ProcessWait {
                timeout: req.remaining_timeout,
                stall: crate::workflow::watchdog::progress_stall_interval(),
                watch_paths: &watch_paths,
            },
            &[],
            stdin,
        )?;
        let last_message = std::fs::read_to_string(&last_path).unwrap_or_default();
        let last_message = if last_message.trim().is_empty() {
            out.stdout.clone()
        } else {
            last_message
        };
        let _ = std::fs::remove_dir_all(&tmp);
        Ok(ReviewResult {
            exit: out.exit,
            stdout: out.stdout,
            stderr: out.stderr,
            last_message,
        })
    }
}

pub(crate) fn argv_for(req: &ReviewRequest, last_path: &Path, schema_path: &Path) -> Vec<String> {
    match req.harness.to_ascii_lowercase().as_str() {
        "claude" => claude_argv(req, schema_path),
        "opencode" => opencode_argv(req),
        _ => codex_argv(req, last_path, schema_path),
    }
}

fn codex_argv(req: &ReviewRequest, last_path: &Path, schema_path: &Path) -> Vec<String> {
    let mut args = vec![
        "exec".into(),
        "-C".into(),
        req.exec_repo.to_string_lossy().into_owned(),
        "-s".into(),
        "read-only".into(),
        "--ephemeral".into(),
        "-o".into(),
        last_path.to_string_lossy().into_owned(),
        "--output-schema".into(),
        schema_path.to_string_lossy().into_owned(),
        "--add-dir".into(),
        req.workspace_root.to_string_lossy().into_owned(),
    ];
    if let Some(ref model) = req.model
        && !model.trim().is_empty()
    {
        args.push("-m".into());
        args.push(model.clone());
    }
    // `-` = read the audit prompt from stdin (see LiveCli::run).
    args.push("-".into());
    args
}

fn claude_argv(req: &ReviewRequest, _schema_path: &Path) -> Vec<String> {
    let mut args = vec![
        "-p".into(),
        req.prompt.clone(),
        "--permission-mode".into(),
        "dontAsk".into(),
        "--allowedTools".into(),
        "Read,Glob,Grep".into(),
        "--disallowedTools".into(),
        "Edit,Write,NotebookEdit".into(),
        "--output-format".into(),
        "text".into(),
        "--add-dir".into(),
        req.workspace_root.to_string_lossy().into_owned(),
        "--json-schema".into(),
        // Claude wants the schema JSON inline, not a path (file path parses as "C").
        super::prompt::VERDICT_SCHEMA_JSON.to_string(),
    ];
    if let Some(ref model) = req.model
        && !model.trim().is_empty()
    {
        args.push("--model".into());
        args.push(model.clone());
    }
    args
}

/// 0011 OpenCode argv. Prompt is stdin (see LiveCli), never a positional —
/// npm `opencode.cmd` forwards `%*` and cmd.exe truncates at `<LF>`.
fn opencode_argv(req: &ReviewRequest) -> Vec<String> {
    let mut args = vec![
        "run".into(),
        "--dir".into(),
        req.exec_repo.to_string_lossy().into_owned(),
        "--format".into(),
        "default".into(),
    ];
    if let Some(ref model) = req.model
        && !model.trim().is_empty()
    {
        args.push("--model".into());
        args.push(model.clone());
    }
    args
}

fn env_for_harness(harness: &str) -> &'static str {
    match harness.to_ascii_lowercase().as_str() {
        "claude" => ENV_COORDINATOR_CLAUDE_BIN,
        "opencode" => ENV_COORDINATOR_OPENCODE_BIN,
        _ => ENV_COORDINATOR_CODEX_BIN,
    }
}

pub(crate) fn resolve_review_bin(harness: &str, command: &str) -> Result<PathBuf> {
    let raw = match std::env::var(env_for_harness(harness)) {
        Ok(s) if !s.trim().is_empty() => s,
        _ => command.to_string(),
    };
    if raw.trim().is_empty() {
        return Err(CoordinatorError::Message(
            "command not found on PATH: (empty)".into(),
        ));
    }
    let resolved = resolve_command(&raw)?;
    reject_or_replace_ps1(resolved)
}

pub(crate) struct ProcOut {
    pub exit: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Wait / stall options for [`run_process`]. `Path` is unsized — watch paths
/// are `PathBuf`.
pub(crate) struct ProcessWait<'a> {
    pub timeout: Duration,
    pub stall: Option<Duration>,
    pub watch_paths: &'a [PathBuf],
}

pub(crate) fn is_reviewer_stall(stderr: &str) -> bool {
    stderr.contains("reviewer stall")
}

pub(crate) fn reviewer_stall_stderr(idle: Duration) -> String {
    format!("reviewer stall — no progress for {}s", idle.as_secs())
}

pub(crate) fn run_process(
    bin: &Path,
    args: &[String],
    cwd: &Path,
    wait: ProcessWait<'_>,
    extra_env: &[(&str, String)],
    stdin: Option<&[u8]>,
) -> Result<ProcOut> {
    let mut cmd = spawn_command(bin);
    cmd.args(args)
        .current_dir(cwd)
        .env("NO_COLOR", "1")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(CoordinatorError::Message(format!(
                "command not found on PATH: {}",
                bin.display()
            )));
        }
        Err(e) => {
            return Err(CoordinatorError::Message(format!(
                "failed to spawn {}: {e}",
                bin.display()
            )));
        }
    };
    if let Some(bytes) = stdin
        && let Some(mut pipe) = child.stdin.take()
    {
        use std::io::Write;
        let _ = pipe.write_all(bytes);
    }

    let stdout_bytes = Arc::new(AtomicU64::new(0));
    let stderr_bytes = Arc::new(AtomicU64::new(0));
    let stdout_buf = Arc::new(Mutex::new(Vec::new()));
    let stderr_buf = Arc::new(Mutex::new(Vec::new()));
    let mut stdout_h = child
        .stdout
        .take()
        .map(|p| drain_pipe(p, Arc::clone(&stdout_bytes), Arc::clone(&stdout_buf)));
    let mut stderr_h = child
        .stderr
        .take()
        .map(|p| drain_pipe(p, Arc::clone(&stderr_bytes), Arc::clone(&stderr_buf)));

    let start = Instant::now();
    let mut last_progress = Instant::now();
    let mut last_out = 0u64;
    let mut last_err = 0u64;
    let mut last_watch = sample_watch(wait.watch_paths);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = join_drain(stdout_h.take(), stdout_buf);
                let stderr = join_drain(stderr_h.take(), stderr_buf);
                let exit = status.code().unwrap_or(-1);
                return Ok(ProcOut {
                    exit,
                    stdout,
                    stderr,
                });
            }
            Ok(None) => {
                let out_n = stdout_bytes.load(Ordering::Relaxed);
                let err_n = stderr_bytes.load(Ordering::Relaxed);
                let now_watch = sample_watch(wait.watch_paths);
                if out_n > last_out || err_n > last_err || now_watch != last_watch {
                    last_progress = Instant::now();
                    last_out = out_n;
                    last_err = err_n;
                    last_watch = now_watch;
                }
                let idle = last_progress.elapsed();
                if let Some(s) = wait.stall
                    && !s.is_zero()
                    && s < wait.timeout
                    && idle >= s
                {
                    tree_kill(&mut child);
                    let _ = join_drain(stdout_h.take(), stdout_buf);
                    let _ = join_drain(stderr_h.take(), stderr_buf);
                    return Ok(ProcOut {
                        exit: 124,
                        stdout: String::new(),
                        stderr: reviewer_stall_stderr(idle),
                    });
                }
                if start.elapsed() >= wait.timeout {
                    tree_kill(&mut child);
                    let _ = join_drain(stdout_h.take(), stdout_buf);
                    let _ = join_drain(stderr_h.take(), stderr_buf);
                    return Ok(ProcOut {
                        exit: 124,
                        stdout: String::new(),
                        stderr: format!("review CLI timed out after {}s", wait.timeout.as_secs()),
                    });
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                tree_kill(&mut child);
                let _ = join_drain(stdout_h.take(), stdout_buf);
                let _ = join_drain(stderr_h.take(), stderr_buf);
                return Err(CoordinatorError::Message(format!(
                    "wait failed for {}: {e}",
                    bin.display()
                )));
            }
        }
    }
}

fn drain_pipe(
    mut pipe: impl Read + Send + 'static,
    bytes: Arc<AtomicU64>,
    buf: Arc<Mutex<Vec<u8>>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    bytes.fetch_add(n as u64, Ordering::Relaxed);
                    if let Ok(mut g) = buf.lock() {
                        g.extend_from_slice(&chunk[..n]);
                    }
                }
                Err(_) => break,
            }
        }
    })
}

fn join_drain(handle: Option<std::thread::JoinHandle<()>>, buf: Arc<Mutex<Vec<u8>>>) -> String {
    if let Some(h) = handle {
        let _ = h.join();
    }
    let bytes = buf.lock().map(|g| g.clone()).unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn sample_watch(paths: &[PathBuf]) -> Vec<Option<(SystemTime, u64)>> {
    paths
        .iter()
        .map(|p| {
            let meta = std::fs::metadata(p).ok()?;
            Some((meta.modified().ok()?, meta.len()))
        })
        .collect()
}

/// Tree-kill a live child. Local to spawn (do not reuse Grok `kill_pid_best_effort`).
fn tree_kill(child: &mut Child) {
    let pid = child.id();
    if pid != 0 && pid != std::process::id() {
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        #[cfg(not(windows))]
        {
            let _ = Command::new("kill")
                .arg("-9")
                .arg(pid.to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn spawn_command(bin: &Path) -> Command {
    let ext = bin
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "cmd" || ext == "bat" {
        let mut c = Command::new("cmd.exe");
        c.arg("/C").arg(bin);
        c
    } else {
        Command::new(bin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn req(harness: &str) -> ReviewRequest {
        ReviewRequest {
            slug: harness.into(),
            harness: harness.into(),
            command: harness.into(),
            model: None,
            exec_repo: PathBuf::from(r"C:\dev\proj\app"),
            workspace_root: PathBuf::from(r"C:\dev\proj"),
            track_dir: None,
            prompt: "audit please".into(),
            remaining_timeout: Duration::from_secs(120),
        }
    }

    #[test]
    fn codex_argv_reads_prompt_from_stdin_and_adds_workspace() {
        let r = req("codex");
        let args = argv_for(&r, Path::new("last.txt"), Path::new("schema.json"));
        assert_eq!(args[0], "exec");
        assert!(!args.iter().any(|a| a == "review"));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--add-dir" && w[1] == r"C:\dev\proj")
        );
        assert!(args.iter().any(|a| a == "--ephemeral"));
        assert!(args.iter().any(|a| a == "--output-schema"));
        assert!(args.iter().any(|a| a == "-s"));
        assert!(!args.iter().any(|a| a == "-m"));
        assert_eq!(args.last().map(String::as_str), Some("-"));
        assert!(!args.iter().any(|a| a == "audit please"));
    }

    #[test]
    fn shebang_plus_cmd_resolves_to_cmd() {
        let dir = tempfile::tempdir().unwrap();
        let sh = dir.path().join("tool");
        std::fs::write(&sh, "#!/bin/sh\necho hi\n").unwrap();
        let cmd = dir.path().join("tool.cmd");
        std::fs::write(&cmd, "@echo off\n").unwrap();
        let got = reject_or_replace_ps1(sh).unwrap();
        assert_eq!(got, cmd);
    }

    #[test]
    fn shebang_only_is_permission() {
        let dir = tempfile::tempdir().unwrap();
        let sh = dir.path().join("tool");
        std::fs::write(&sh, "#!/bin/sh\necho hi\n").unwrap();
        let err = reject_or_replace_ps1(sh).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("refusing to spawn shim"),
            "expected permission, got {msg}"
        );
    }

    #[test]
    fn claude_argv_has_no_bare() {
        let r = req("claude");
        let args = argv_for(&r, Path::new("last.txt"), Path::new("schema.json"));
        assert!(!args.iter().any(|a| a == "--bare"));
        assert!(args.iter().any(|a| a == "--permission-mode"));
        assert!(args.iter().any(|a| a == "--add-dir"));
        assert!(args.iter().any(|a| a == "--json-schema"));
        assert!(args.iter().any(|a| a == "-p"));
        let schema = args
            .windows(2)
            .find(|w| w[0] == "--json-schema")
            .map(|w| w[1].as_str())
            .expect("schema arg");
        assert!(
            schema.trim_start().starts_with('{'),
            "Claude --json-schema must be inline JSON, got {schema}"
        );
        assert!(
            !schema.ends_with(".json"),
            "Claude --json-schema must not be a file path: {schema}"
        );
    }

    #[test]
    fn opencode_argv_has_no_auto() {
        let r = req("opencode");
        let args = argv_for(&r, Path::new("last.txt"), Path::new("schema.json"));
        assert_eq!(args[0], "run");
        assert!(!args.iter().any(|a| a == "--auto"));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--dir" && w[1] == r"C:\dev\proj\app")
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--format" && w[1] == "default")
        );
        assert!(
            !args.iter().any(|a| a == "audit please" || a.contains('\n')),
            "prompt must be stdin, not argv: {args:?}"
        );
    }

    #[test]
    fn model_override_adds_flag() {
        let mut r = req("codex");
        r.model = Some("gpt-5.6-terra".into());
        let args = argv_for(&r, Path::new("last.txt"), Path::new("schema.json"));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-m" && w[1] == "gpt-5.6-terra")
        );
    }

    fn wait(timeout: Duration, stall: Option<Duration>, watch: &[PathBuf]) -> ProcessWait<'_> {
        ProcessWait {
            timeout,
            stall,
            watch_paths: watch,
        }
    }

    fn write_cmd(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    fn pid_is_live(pid: u32) -> bool {
        let out = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output();
        match out {
            Ok(o) => {
                let s = String::from_utf8_lossy(&o.stdout);
                s.contains(&pid.to_string()) && !s.to_ascii_lowercase().contains("no tasks")
            }
            Err(_) => false,
        }
    }

    #[test]
    fn zero_stdio_sleeper_stall_kills_with_stall_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = write_cmd(
            dir.path(),
            "sleep.cmd",
            "@echo off\r\nping -n 30 127.0.0.1 >nul 2>&1\r\n",
        );
        let t0 = Instant::now();
        let out = run_process(
            &cmd,
            &[],
            dir.path(),
            wait(Duration::from_secs(30), Some(Duration::from_secs(1)), &[]),
            &[],
            None,
        )
        .unwrap();
        assert!(
            t0.elapsed() < Duration::from_secs(8),
            "stall-kill should finish in a few seconds, took {:?}",
            t0.elapsed()
        );
        assert_eq!(out.exit, 124);
        assert!(is_reviewer_stall(&out.stderr), "stderr={}", out.stderr);
        assert!(
            out.stderr.contains("reviewer stall — no progress for"),
            "stderr={}",
            out.stderr
        );
        assert!(
            !out.stderr.contains("timed out after"),
            "stderr={}",
            out.stderr
        );
        assert!(
            !out.stderr.to_ascii_lowercase().contains("exhausted"),
            "stderr={}",
            out.stderr
        );
    }

    #[test]
    fn chatty_stderr_child_exits_zero() {
        let dir = tempfile::tempdir().unwrap();
        let args = vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            "1..15 | ForEach-Object { [Console]::Error.WriteLine('tick'); Start-Sleep -Milliseconds 200 }"
                .into(),
        ];
        let out = run_process(
            Path::new("powershell.exe"),
            &args,
            dir.path(),
            wait(Duration::from_secs(10), Some(Duration::from_secs(1)), &[]),
            &[],
            None,
        )
        .unwrap();
        assert_eq!(out.exit, 0, "stderr={} stdout={}", out.stderr, out.stdout);
        assert!(!is_reviewer_stall(&out.stderr), "stderr={}", out.stderr);
    }

    #[test]
    fn artifact_mtime_child_exits_zero() {
        let dir = tempfile::tempdir().unwrap();
        let watch = dir.path().join("watch.txt");
        std::fs::write(&watch, "start\n").unwrap();
        let args = vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            "$ErrorActionPreference='Stop'; 1..15 | ForEach-Object { Set-Content -LiteralPath $env:COORDINATOR_TEST_WATCH -Value $_; Start-Sleep -Milliseconds 200 }".into(),
        ];
        let watch_paths = vec![watch.clone()];
        let out = run_process(
            Path::new("powershell.exe"),
            &args,
            dir.path(),
            wait(
                Duration::from_secs(10),
                Some(Duration::from_secs(1)),
                &watch_paths,
            ),
            &[(
                "COORDINATOR_TEST_WATCH",
                watch.to_string_lossy().into_owned(),
            )],
            None,
        )
        .unwrap();
        assert_eq!(out.exit, 0, "stderr={} stdout={}", out.stderr, out.stdout);
        assert!(!is_reviewer_stall(&out.stderr), "stderr={}", out.stderr);
    }

    #[test]
    fn stall_disabled_uses_remaining_timeout_string() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = write_cmd(
            dir.path(),
            "sleep.cmd",
            "@echo off\r\nping -n 30 127.0.0.1 >nul 2>&1\r\n",
        );
        let out = run_process(
            &cmd,
            &[],
            dir.path(),
            wait(Duration::from_secs(1), None, &[]),
            &[],
            None,
        )
        .unwrap();
        assert_eq!(out.exit, 124);
        assert!(
            out.stderr.contains("review CLI timed out after"),
            "stderr={}",
            out.stderr
        );
        assert!(!is_reviewer_stall(&out.stderr), "stderr={}", out.stderr);
    }

    #[test]
    fn stall_at_least_remaining_uses_timeout_string() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = write_cmd(
            dir.path(),
            "sleep.cmd",
            "@echo off\r\nping -n 30 127.0.0.1 >nul 2>&1\r\n",
        );
        let out = run_process(
            &cmd,
            &[],
            dir.path(),
            wait(Duration::from_secs(1), Some(Duration::from_secs(5)), &[]),
            &[],
            None,
        )
        .unwrap();
        assert_eq!(out.exit, 124);
        assert!(
            out.stderr.contains("review CLI timed out after"),
            "stderr={}",
            out.stderr
        );
        assert!(!is_reviewer_stall(&out.stderr), "stderr={}", out.stderr);
    }

    #[test]
    fn cmd_tree_kill_reaps_grandchild_pid() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("grandchild.pid");
        let cmd = write_cmd(
            dir.path(),
            "tree.cmd",
            "@echo off\r\npowershell.exe -NoProfile -NonInteractive -Command \"$p = Start-Process -FilePath ping.exe -ArgumentList '-n','120','127.0.0.1' -WindowStyle Hidden -PassThru; Set-Content -LiteralPath $env:COORDINATOR_TEST_PIDFILE -Value $p.Id; Wait-Process -Id $p.Id\"\r\n",
        );
        let pidfile_s = pidfile.to_string_lossy().into_owned();
        let handle = std::thread::spawn({
            let cmd = cmd.clone();
            let cwd = dir.path().to_path_buf();
            let pidfile_s = pidfile_s.clone();
            move || {
                run_process(
                    &cmd,
                    &[],
                    &cwd,
                    wait(Duration::from_secs(30), Some(Duration::from_secs(2)), &[]),
                    &[("COORDINATOR_TEST_PIDFILE", pidfile_s)],
                    None,
                )
            }
        });
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut pid = None;
        while Instant::now() < deadline {
            if let Ok(s) = std::fs::read_to_string(&pidfile)
                && let Ok(p) = s.trim().parse::<u32>()
                && p != 0
                && pid_is_live(p)
            {
                pid = Some(p);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let pid = pid.expect("grandchild PID file should appear while child is live");
        let out = handle.join().expect("run_process thread").unwrap();
        assert_eq!(out.exit, 124);
        assert!(is_reviewer_stall(&out.stderr), "stderr={}", out.stderr);
        let gone_deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < gone_deadline && pid_is_live(pid) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !pid_is_live(pid),
            "grandchild PID {pid} should be gone after tree-kill"
        );
    }
}
