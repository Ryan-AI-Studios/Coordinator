//! Live `gh` + `git` backend. Default `cargo test` never constructs this
//! against a real network; tests inject [`super::backend::ScriptedBackend`].

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::ENV_COORDINATOR_GH_BIN;
use crate::error::{CoordinatorError, Result};

use super::backend::{
    CheckBucket, CheckItem, CheckSnapshot, CiBackend, CiTarget, MergeResult, PrHint,
};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(30);

pub struct GhCli;

impl CiBackend for GhCli {
    fn resolve_pr(&self, cwd: &Path, hint: Option<&PrHint>) -> Result<Option<CiTarget>> {
        if let Some(h) = hint
            && let Some(n) = h.number
            && let Some(t) = pr_view(cwd, Some(n))?
        {
            return Ok(Some(t));
        }
        if let Some(t) = pr_view(cwd, None)? {
            return Ok(Some(t));
        }
        if let Ok(branch) = git_stdout(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])
            && let Some(t) = pr_list_head(cwd, branch.trim())?
        {
            return Ok(Some(t));
        }
        let sha = git_stdout(cwd, &["rev-parse", "HEAD"])?.trim().to_string();
        if sha.is_empty() {
            return Ok(None);
        }
        if head_is_default_branch(cwd)? {
            return Ok(Some(CiTarget::HeadSha { sha }));
        }
        Ok(None)
    }

    fn checks(&self, cwd: &Path, target: &CiTarget) -> Result<CheckSnapshot> {
        match target {
            CiTarget::PullRequest { number, .. } => pr_checks(cwd, *number),
            CiTarget::HeadSha { sha } => run_list(cwd, sha),
        }
    }

    fn squash_merge(
        &self,
        cwd: &Path,
        pr_number: u64,
        head_oid: Option<&str>,
    ) -> Result<MergeResult> {
        let n = pr_number.to_string();
        let mut args = vec!["pr", "merge", n.as_str(), "--squash"];
        if let Some(oid) = head_oid {
            args.push("--match-head-commit");
            args.push(oid);
        }
        let out = gh_capture(cwd, &args)?;
        if out.exit == 4 {
            return Err(CoordinatorError::Message("gh auth required".into()));
        }
        if !out.ok && head_oid.is_some() && looks_like_unknown_flag(&out.stderr) {
            let retry = gh_capture(cwd, &["pr", "merge", n.as_str(), "--squash"])?;
            return Ok(merge_from_output(&retry));
        }
        Ok(merge_from_output(&out))
    }
}

fn merge_from_output(out: &ProcOut) -> MergeResult {
    let blob = format!("{}\n{}", out.stdout, out.stderr);
    let queued = blob.to_ascii_lowercase().contains("queued")
        || blob.to_ascii_lowercase().contains("merge queue");
    MergeResult {
        ok: out.ok,
        queued,
        message: if out.stderr.trim().is_empty() {
            out.stdout.clone()
        } else {
            out.stderr.clone()
        },
    }
}

fn looks_like_unknown_flag(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("unknown flag") || s.contains("unknown command") || s.contains("unknown shorthand")
}

fn pr_view(cwd: &Path, number: Option<u64>) -> Result<Option<CiTarget>> {
    let n = number.map(|v| v.to_string());
    let mut args = vec!["pr", "view"];
    if let Some(ref n) = n {
        args.push(n.as_str());
    }
    args.extend([
        "--json",
        "number,url,isDraft,state,headRefName,mergeable,headRefOid",
    ]);
    let out = gh_capture(cwd, &args)?;
    if out.exit == 4 {
        return Err(CoordinatorError::Message("gh auth required".into()));
    }
    if !out.ok {
        return Ok(None);
    }
    parse_pr_view(&out.stdout)
}

fn parse_pr_view(stdout: &str) -> Result<Option<CiTarget>> {
    let v: serde_json::Value = match serde_json::from_str(stdout) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    let number = match v.get("number").and_then(|x| x.as_u64()) {
        Some(n) => n,
        None => return Ok(None),
    };
    let url = v
        .get("url")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let is_draft = v.get("isDraft").and_then(|x| x.as_bool()).unwrap_or(false);
    let state = v.get("state").and_then(|x| x.as_str()).unwrap_or("");
    let merged = state.eq_ignore_ascii_case("merged")
        || v.get("mergedAt").map(|x| !x.is_null()).unwrap_or(false);
    let head_oid = v
        .get("headRefOid")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    Ok(Some(CiTarget::PullRequest {
        number,
        url,
        is_draft,
        merged,
        head_oid,
    }))
}

fn pr_list_head(cwd: &Path, branch: &str) -> Result<Option<CiTarget>> {
    let out = gh_capture(
        cwd,
        &[
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "open",
            "--json",
            "number,url",
            "--limit",
            "1",
        ],
    )?;
    if out.exit == 4 {
        return Err(CoordinatorError::Message("gh auth required".into()));
    }
    if !out.ok {
        return Ok(None);
    }
    let arr: Vec<serde_json::Value> = serde_json::from_str(&out.stdout).unwrap_or_default();
    let Some(first) = arr.first() else {
        return Ok(None);
    };
    let Some(number) = first.get("number").and_then(|x| x.as_u64()) else {
        return Ok(None);
    };
    // Need isDraft/merged from `pr view`. Do not invent a non-draft target.
    pr_view(cwd, Some(number))
}

fn pr_checks(cwd: &Path, number: u64) -> Result<CheckSnapshot> {
    let n = number.to_string();
    let out = gh_capture(
        cwd,
        &["pr", "checks", n.as_str(), "--json", "bucket,name,state"],
    )?;
    if out.exit == 4 {
        return Err(CoordinatorError::Message("gh auth required".into()));
    }
    // exit 8 = checks pending — still parse JSON
    parse_pr_checks(&out.stdout, out.exit)
}

fn parse_pr_checks(stdout: &str, raw_exit: i32) -> Result<CheckSnapshot> {
    if stdout.trim().is_empty() {
        return Ok(CheckSnapshot {
            items: Vec::new(),
            raw_exit,
        });
    }
    let arr: Vec<serde_json::Value> = serde_json::from_str(stdout)
        .map_err(|e| CoordinatorError::Message(format!("gh pr checks json: {e}")))?;
    let items = arr
        .into_iter()
        .map(|row| {
            let name = row
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("check")
                .to_string();
            let bucket = row
                .get("bucket")
                .and_then(|x| x.as_str())
                .map(CheckBucket::parse)
                .unwrap_or(CheckBucket::Pending);
            CheckItem { name, bucket }
        })
        .collect();
    Ok(CheckSnapshot { items, raw_exit })
}

fn run_list(cwd: &Path, sha: &str) -> Result<CheckSnapshot> {
    let out = gh_capture(
        cwd,
        &[
            "run",
            "list",
            "--commit",
            sha,
            "--json",
            "status,conclusion,name,databaseId",
            "--limit",
            "20",
        ],
    )?;
    if out.exit == 4 {
        return Err(CoordinatorError::Message("gh auth required".into()));
    }
    if !out.ok && out.exit != 0 {
        return Ok(CheckSnapshot {
            items: Vec::new(),
            raw_exit: out.exit,
        });
    }
    parse_run_list(&out.stdout, out.exit)
}

fn parse_run_list(stdout: &str, raw_exit: i32) -> Result<CheckSnapshot> {
    if stdout.trim().is_empty() {
        return Ok(CheckSnapshot {
            items: Vec::new(),
            raw_exit,
        });
    }
    let arr: Vec<serde_json::Value> = serde_json::from_str(stdout).unwrap_or_default();
    let items = arr
        .into_iter()
        .map(|row| {
            let name = row
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("run")
                .to_string();
            let status = row.get("status").and_then(|x| x.as_str()).unwrap_or("");
            let conclusion = row.get("conclusion").and_then(|x| x.as_str()).unwrap_or("");
            let bucket = if !status.is_empty() && !status.eq_ignore_ascii_case("completed") {
                CheckBucket::Pending
            } else {
                match conclusion.to_ascii_lowercase().as_str() {
                    "failure" | "timed_out" | "startup_failure" => CheckBucket::Fail,
                    "cancelled" | "canceled" => CheckBucket::Cancel,
                    "success" => CheckBucket::Pass,
                    "skipped" | "neutral" | "" => CheckBucket::Skipping,
                    other => CheckBucket::parse(other),
                }
            };
            CheckItem { name, bucket }
        })
        .collect();
    Ok(CheckSnapshot { items, raw_exit })
}

fn head_is_default_branch(cwd: &Path) -> Result<bool> {
    let head = git_stdout(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let head = head.trim();
    if let Ok(def) = gh_default_branch(cwd) {
        return Ok(head == def);
    }
    if let Ok(sym) = git_stdout(cwd, &["symbolic-ref", "refs/remotes/origin/HEAD"]) {
        let name = sym.trim().rsplit('/').next().unwrap_or("");
        return Ok(!name.is_empty() && head == name);
    }
    Ok(false)
}

fn gh_default_branch(cwd: &Path) -> Result<String> {
    let out = gh_capture(cwd, &["repo", "view", "--json", "defaultBranchRef"])?;
    if out.exit == 4 {
        return Err(CoordinatorError::Message("gh auth required".into()));
    }
    if !out.ok {
        return Err(CoordinatorError::Message("gh repo view failed".into()));
    }
    let v: serde_json::Value = serde_json::from_str(&out.stdout)
        .map_err(|e| CoordinatorError::Message(format!("gh repo view json: {e}")))?;
    v.get("defaultBranchRef")
        .and_then(|d| d.get("name"))
        .and_then(|n| n.as_str())
        .map(str::to_string)
        .ok_or_else(|| CoordinatorError::Message("gh repo view: no defaultBranchRef.name".into()))
}

fn gh_bin() -> PathBuf {
    if let Ok(p) = std::env::var(ENV_COORDINATOR_GH_BIN) {
        let t = p.trim();
        if !t.is_empty() {
            return PathBuf::from(t);
        }
    }
    #[cfg(windows)]
    {
        PathBuf::from("gh.exe")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("gh")
    }
}

struct ProcOut {
    exit: i32,
    ok: bool,
    stdout: String,
    stderr: String,
}

fn gh_capture(cwd: &Path, args: &[&str]) -> Result<ProcOut> {
    let bin = gh_bin();
    match run_process(&bin, args, cwd) {
        Ok(out) => Ok(out),
        Err(e) if e.to_string().contains("not found") && cfg!(windows) => {
            if bin != Path::new("gh") {
                run_process(Path::new("gh"), args, cwd)
            } else {
                Err(e)
            }
        }
        Err(e) => Err(e),
    }
}

fn git_stdout(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = run_process(Path::new("git"), args, cwd)?;
    if !out.ok {
        return Err(CoordinatorError::Message(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            truncate(&out.stderr)
        )));
    }
    Ok(out.stdout)
}

fn run_process(bin: &Path, args: &[&str], cwd: &Path) -> Result<ProcOut> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .current_dir(cwd)
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(CoordinatorError::Message(format!(
                "gh not found or not executable: {}",
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
                let exit = status.code().unwrap_or(1);
                return Ok(ProcOut {
                    exit,
                    ok: status.success(),
                    stdout,
                    stderr,
                });
            }
            Ok(None) if start.elapsed() >= PROCESS_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CoordinatorError::Message("ci-wait: gh timed out".into()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                return Err(CoordinatorError::Message(format!(
                    "wait {}: {e}",
                    bin.display()
                )));
            }
        }
    }
}

fn truncate(s: &str) -> String {
    let t = s.trim();
    if t.chars().count() <= 200 {
        return t.to_string();
    }
    let cut: String = t.chars().take(200).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    #[test]
    fn parse_pr_view_draft() {
        let json = r#"{"number":7,"url":"https://example/pr/7","isDraft":true,"state":"OPEN","headRefOid":"abc"}"#;
        let t = parse_pr_view(json).unwrap().unwrap();
        match t {
            CiTarget::PullRequest {
                number,
                is_draft,
                merged,
                ..
            } => {
                assert_eq!(number, 7);
                assert!(is_draft);
                assert!(!merged);
            }
            _ => panic!("expected PR"),
        }
    }

    #[test]
    fn parse_pr_checks_buckets() {
        let json = r#"[{"name":"ci","bucket":"pass","state":"SUCCESS"},{"name":"lint","bucket":"pending","state":"PENDING"}]"#;
        let snap = parse_pr_checks(json, 8).unwrap();
        assert_eq!(snap.raw_exit, 8);
        assert_eq!(snap.items[0].bucket, CheckBucket::Pass);
        assert_eq!(snap.items[1].bucket, CheckBucket::Pending);
    }

    #[test]
    fn parse_run_list_maps_conclusions() {
        let json = r#"[
            {"name":"ok","status":"completed","conclusion":"success"},
            {"name":"bad","status":"completed","conclusion":"failure"},
            {"name":"wait","status":"in_progress","conclusion":""}
        ]"#;
        let snap = parse_run_list(json, 0).unwrap();
        assert_eq!(snap.items[0].bucket, CheckBucket::Pass);
        assert_eq!(snap.items[1].bucket, CheckBucket::Fail);
        assert_eq!(snap.items[2].bucket, CheckBucket::Pending);
    }
}

#[cfg(test)]
mod live {
    use super::*;
    use crate::config::ENV_COORDINATOR_GH_LIVE;

    #[test]
    #[ignore]
    fn ci_live_gh_on_path() {
        if std::env::var(ENV_COORDINATOR_GH_LIVE).ok().as_deref() != Some("1") {
            return;
        }
        let out = Command::new(gh_bin())
            .arg("--version")
            .env("GH_PROMPT_DISABLED", "1")
            .env("NO_COLOR", "1")
            .output()
            .expect("gh --version");
        assert!(out.status.success(), "gh --version failed");
    }
}
