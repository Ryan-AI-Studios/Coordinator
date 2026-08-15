//! Live one-shot review CLI spawn (Codex / Claude / OpenCode).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
        let out = run_process(&bin, &args, &req.exec_repo, req.remaining_timeout, &[])?;
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
    ];
    if let Some(ref model) = req.model
        && !model.trim().is_empty()
    {
        args.push("-m".into());
        args.push(model.clone());
    }
    args.push(req.prompt.clone());
    args
}

fn claude_argv(req: &ReviewRequest, schema_path: &Path) -> Vec<String> {
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
        schema_path.to_string_lossy().into_owned(),
    ];
    if let Some(ref model) = req.model
        && !model.trim().is_empty()
    {
        args.push("--model".into());
        args.push(model.clone());
    }
    args
}

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
    args.push(req.prompt.clone());
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

pub(crate) fn run_process(
    bin: &Path,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
    extra_env: &[(&str, String)],
) -> Result<ProcOut> {
    let mut cmd = spawn_command(bin);
    cmd.args(args)
        .current_dir(cwd)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
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
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = String::new();
                let mut stderr = String::new();
                if let Some(mut s) = child.stdout.take() {
                    use std::io::Read;
                    let _ = s.read_to_string(&mut stdout);
                }
                if let Some(mut s) = child.stderr.take() {
                    use std::io::Read;
                    let _ = s.read_to_string(&mut stderr);
                }
                let exit = status.code().unwrap_or(-1);
                return Ok(ProcOut {
                    exit,
                    stdout,
                    stderr,
                });
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(ProcOut {
                        exit: 124,
                        stdout: String::new(),
                        stderr: format!("review CLI timed out after {}s", timeout.as_secs()),
                    });
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                return Err(CoordinatorError::Message(format!(
                    "wait failed for {}: {e}",
                    bin.display()
                )));
            }
        }
    }
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
    fn codex_argv_has_no_add_dir_or_exec_review() {
        let r = req("codex");
        let args = argv_for(&r, Path::new("last.txt"), Path::new("schema.json"));
        assert_eq!(args[0], "exec");
        assert!(!args.iter().any(|a| a == "review"));
        assert!(!args.iter().any(|a| a == "--add-dir"));
        assert!(args.iter().any(|a| a == "--ephemeral"));
        assert!(args.iter().any(|a| a == "--output-schema"));
        assert!(args.iter().any(|a| a == "-s"));
        assert!(!args.iter().any(|a| a == "-m"));
        assert_eq!(args.last().map(String::as_str), Some("audit please"));
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
}
