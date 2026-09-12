//! Live `gh` + `git` backend. Default `cargo test` never constructs this
//! against a real network; tests inject [`super::backend::ScriptedBackend`].

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::ENV_COORDINATOR_GH_BIN;
use crate::error::{CoordinatorError, Result};

use super::backend::{
    AutoPublishResult, CheckBucket, CheckItem, CheckSnapshot, CheckView, CiBackend, CiTarget,
    MergeResult, MergeStateStatus, PrHint,
};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(30);
const GIT_PUSH_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_PR_BODY: &str =
    "Opened by Coordinator ci-wait after a local track commit with no GitHub PR.";

/// `gh pr view --json` fields. `mergedAt` is requested so `parse_pr_view` can
/// see it; `state == MERGED` remains the primary shipped signal.
const PR_VIEW_JSON_FIELDS: &str =
    "number,url,isDraft,state,headRefName,mergeable,headRefOid,mergedAt,mergeStateStatus";

/// Pure argv for `gh pr list --head` (no spawn). `state` is `open` then `merged`.
fn pr_list_head_args(branch: &str, state: &str) -> Vec<String> {
    vec![
        "pr".into(),
        "list".into(),
        "--head".into(),
        branch.into(),
        "--state".into(),
        state.into(),
        "--json".into(),
        "number,url".into(),
        "--limit".into(),
        "1".into(),
    ]
}

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
            CiTarget::PullRequest {
                number,
                merge_state,
                ..
            } => pr_checks_required_and_all(cwd, *number, *merge_state),
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

    fn try_auto_publish(&self, cwd: &Path, track_id: &str) -> Result<AutoPublishResult> {
        live_auto_publish(self, cwd, track_id)
    }
}

fn live_auto_publish(cli: &GhCli, cwd: &Path, track_id: &str) -> Result<AutoPublishResult> {
    let numeric = crate::notify::artifact::numeric_track_id(track_id).unwrap_or(track_id);

    let branch = match git_stdout(cwd, &["symbolic-ref", "--short", "HEAD"]) {
        Ok(b) => {
            let b = b.trim().to_string();
            if b.is_empty() || b == "HEAD" {
                return Ok(AutoPublishResult::skipped(
                    "ci-wait: detached HEAD — waiting for PR",
                ));
            }
            b
        }
        Err(_) => {
            return Ok(AutoPublishResult::skipped(
                "ci-wait: detached HEAD — waiting for PR",
            ));
        }
    };

    match git_stdout(cwd, &["status", "--porcelain"]) {
        Ok(p) if !p.trim().is_empty() => {
            return Ok(AutoPublishResult::skipped(
                "ci-wait: dirty tree — waiting for PR",
            ));
        }
        Ok(_) => {}
        Err(_) => {
            return Ok(AutoPublishResult::skipped(
                "ci-wait: dirty tree — waiting for PR",
            ));
        }
    }

    let default_branch = match resolve_default_branch_name(cwd) {
        Ok(d) => d,
        Err(e) if e.to_string().contains("auth required") => return Err(e),
        Err(_) => String::new(),
    };
    if !default_branch.is_empty() && branch == default_branch {
        return Ok(AutoPublishResult::skipped(
            "ci-wait: on default branch — waiting for PR",
        ));
    }

    let subject = match git_stdout(cwd, &["log", "-1", "--format=%s"]) {
        Ok(s) => s,
        Err(_) => {
            return Ok(AutoPublishResult::skipped("ci-wait: waiting for PR"));
        }
    };
    if !crate::workflow::shipped::pr_title_is_track(subject.trim(), numeric) {
        return Ok(AutoPublishResult::skipped("ci-wait: waiting for PR"));
    }

    let Some(remote) = resolve_live_push_remote(cwd)? else {
        return Ok(AutoPublishResult::skipped(
            "ci-wait: no GitHub remote — waiting for PR",
        ));
    };

    let head = match git_stdout(cwd, &["rev-parse", "HEAD"]) {
        Ok(h) => h.trim().to_string(),
        Err(_) => {
            return Ok(AutoPublishResult::skipped("ci-wait: waiting for PR"));
        }
    };
    if head.is_empty() {
        return Ok(AutoPublishResult::skipped("ci-wait: waiting for PR"));
    }
    if !default_branch.is_empty() {
        let def_ref = format!("refs/remotes/{remote}/{default_branch}");
        if let Ok(def_sha) = git_stdout(cwd, &["rev-parse", &def_ref])
            && def_sha.trim() == head
        {
            return Ok(AutoPublishResult::skipped(
                "ci-wait: on default branch — waiting for PR",
            ));
        }
    }

    let push_args = git_push_args(&remote, &branch);
    let push_refs: Vec<&str> = push_args.iter().map(String::as_str).collect();
    match run_process_timeout(Path::new("git"), &push_refs, cwd, GIT_PUSH_TIMEOUT) {
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("timed out") || msg.contains("auth required") {
                return Err(e);
            }
            return Ok(AutoPublishResult::skipped_latched(
                "ci-wait: push rejected — waiting for PR",
                head,
            ));
        }
        Ok(out) if !out.ok => {
            return Ok(AutoPublishResult::skipped_latched(
                "ci-wait: push rejected — waiting for PR",
                head,
            ));
        }
        Ok(_) => {}
    }

    let body = git_stdout(cwd, &["log", "-1", "--format=%b"])
        .ok()
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| DEFAULT_PR_BODY.to_string());
    let repo = owner_repo_for_remote(cwd, &remote);
    let base = if default_branch.is_empty() {
        "main".to_string()
    } else {
        default_branch
    };
    let create_args = pr_create_args(subject.trim(), &body, &base, &branch, repo.as_deref());
    let create_refs: Vec<&str> = create_args.iter().map(String::as_str).collect();
    let out = gh_capture(cwd, &create_refs)?;
    if out.exit == 4 {
        return Err(CoordinatorError::Message("gh auth required".into()));
    }
    if out.ok
        && let Some(number) = parse_pr_url_number(&out.stdout)
    {
        let url = out
            .stdout
            .lines()
            .find(|l| l.contains("/pull/"))
            .unwrap_or(out.stdout.trim());
        return Ok(AutoPublishResult::Opened(CiTarget::PullRequest {
            number,
            url: url.trim().to_string(),
            is_draft: false,
            merged: false,
            head_oid: Some(head),
            merge_state: MergeStateStatus::Unspecified,
        }));
    }
    match cli.resolve_pr(cwd, None) {
        Ok(Some(t)) => Ok(AutoPublishResult::Opened(t)),
        Ok(None) => Ok(AutoPublishResult::skipped_latched(
            "ci-wait: waiting for PR",
            head,
        )),
        Err(e) if e.to_string().contains("auth required") => Err(e),
        Err(_) => Ok(AutoPublishResult::skipped_latched(
            "ci-wait: waiting for PR",
            head,
        )),
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
    args.extend(["--json", PR_VIEW_JSON_FIELDS]);
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
    let merge_state = v
        .get("mergeStateStatus")
        .and_then(|x| x.as_str())
        .map(MergeStateStatus::parse_live)
        .unwrap_or(MergeStateStatus::Unknown);
    Ok(Some(CiTarget::PullRequest {
        number,
        url,
        is_draft,
        merged,
        head_oid,
        merge_state,
    }))
}

fn pr_list_head(cwd: &Path, branch: &str) -> Result<Option<CiTarget>> {
    // Open first so an in-flight PR wins over an older merged PR on the same head.
    for state in ["open", "merged"] {
        let args = pr_list_head_args(branch, state);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = gh_capture(cwd, &refs)?;
        if out.exit == 4 {
            return Err(CoordinatorError::Message("gh auth required".into()));
        }
        if !out.ok {
            continue;
        }
        let arr: Vec<serde_json::Value> = serde_json::from_str(&out.stdout).unwrap_or_default();
        let Some(first) = arr.first() else {
            continue;
        };
        let Some(number) = first.get("number").and_then(|x| x.as_u64()) else {
            continue;
        };
        // Need isDraft/merged from `pr view`. Do not invent a non-draft target.
        return pr_view(cwd, Some(number));
    }
    Ok(None)
}

/// Pure argv for `gh pr checks` (no spawn).
fn pr_checks_args(number: u64, required: bool) -> Vec<String> {
    let mut args = vec![
        "pr".into(),
        "checks".into(),
        number.to_string(),
        "--json".into(),
        "bucket,name,state".into(),
    ];
    if required {
        args.insert(3, "--required".into());
    }
    args
}

fn pr_checks_required_and_all(
    cwd: &Path,
    number: u64,
    merge_state: MergeStateStatus,
) -> Result<CheckSnapshot> {
    let required = pr_checks(cwd, number, true)?;
    let all = pr_checks(cwd, number, false)?;
    Ok(compose_required_snapshot(required, all, merge_state))
}

fn compose_required_snapshot(
    required: CheckSnapshot,
    all: CheckSnapshot,
    merge_state: MergeStateStatus,
) -> CheckSnapshot {
    if required.items.is_empty() {
        return CheckSnapshot {
            items: Vec::new(),
            raw_exit: required.raw_exit,
            merge_state,
            view: CheckView::Required,
            advisory: all.items,
        };
    }
    let required_names: std::collections::HashSet<String> =
        required.items.iter().map(|i| i.name.clone()).collect();
    let advisory = all
        .items
        .into_iter()
        .filter(|i| !required_names.contains(&i.name))
        .collect();
    CheckSnapshot {
        items: required.items,
        raw_exit: required.raw_exit,
        merge_state,
        view: CheckView::Required,
        advisory,
    }
}

fn pr_checks(cwd: &Path, number: u64, required: bool) -> Result<CheckSnapshot> {
    let args = pr_checks_args(number, required);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = gh_capture(cwd, &refs)?;
    if out.exit == 4 {
        return Err(CoordinatorError::Message("gh auth required".into()));
    }
    // exit 1 = no checks / no required; exit 8 = pending — still parse JSON
    parse_pr_checks(&out.stdout, out.exit)
}

fn parse_pr_checks(stdout: &str, raw_exit: i32) -> Result<CheckSnapshot> {
    if stdout.trim().is_empty() {
        return Ok(CheckSnapshot {
            items: Vec::new(),
            raw_exit,
            ..CheckSnapshot::empty()
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
    Ok(CheckSnapshot {
        items,
        raw_exit,
        ..CheckSnapshot::empty()
    })
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
            ..CheckSnapshot::empty()
        });
    }
    parse_run_list(&out.stdout, out.exit)
}

fn parse_run_list(stdout: &str, raw_exit: i32) -> Result<CheckSnapshot> {
    if stdout.trim().is_empty() {
        return Ok(CheckSnapshot {
            items: Vec::new(),
            raw_exit,
            ..CheckSnapshot::empty()
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
    Ok(CheckSnapshot {
        items,
        raw_exit,
        ..CheckSnapshot::empty()
    })
}

fn normalize_github_identity(url: &str) -> Option<(String, String, String)> {
    let u = url.trim();
    if u.is_empty() {
        return None;
    }
    if let Some(rest) = u.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        return split_owner_repo(host, path);
    }
    let rest = u
        .strip_prefix("ssh://git@")
        .or_else(|| u.strip_prefix("ssh://"))
        .or_else(|| u.strip_prefix("https://"))
        .or_else(|| u.strip_prefix("http://"))?;
    let rest = rest.strip_prefix("git@").unwrap_or(rest);
    let (host, path) = rest.split_once('/')?;
    split_owner_repo(host, path)
}

fn split_owner_repo(host: &str, path: &str) -> Option<(String, String, String)> {
    let path = path.trim_start_matches('/').trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim();
    if owner.is_empty() || repo.is_empty() || host.trim().is_empty() {
        return None;
    }
    Some((
        host.trim().to_ascii_lowercase(),
        owner.to_ascii_lowercase(),
        repo.to_ascii_lowercase(),
    ))
}

fn resolve_push_remote_from_inputs(
    remotes: &[&str],
    fetch_urls: &[(&str, &str)],
    push_remote: Option<&str>,
    push_default: Option<&str>,
    branch_remote: Option<&str>,
    gh_url: Option<&str>,
) -> Option<String> {
    let has = |name: &str| remotes.contains(&name);
    for cand in [push_remote, push_default] {
        if let Some(n) = cand.map(str::trim).filter(|s| !s.is_empty() && has(s)) {
            return Some(n.to_string());
        }
    }
    if let Some(n) = branch_remote
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "." && has(s))
    {
        return Some(n.to_string());
    }
    if let Some(gh) = gh_url.and_then(normalize_github_identity) {
        for (name, url) in fetch_urls {
            if normalize_github_identity(url).is_some_and(|id| id == gh) && has(name) {
                return Some((*name).to_string());
            }
        }
    }
    if remotes.len() == 1 {
        return Some(remotes[0].to_string());
    }
    if has("origin") {
        return Some("origin".into());
    }
    None
}

fn git_push_args(remote: &str, branch: &str) -> Vec<String> {
    vec!["push".into(), "-u".into(), remote.into(), branch.into()]
}

fn pr_create_args(
    title: &str,
    body: &str,
    base: &str,
    head: &str,
    repo: Option<&str>,
) -> Vec<String> {
    let mut a = vec![
        "pr".into(),
        "create".into(),
        "--title".into(),
        title.into(),
        "--body".into(),
        body.into(),
        "--base".into(),
        base.into(),
        "--head".into(),
        head.into(),
    ];
    if let Some(r) = repo.filter(|s| !s.is_empty()) {
        a.push("--repo".into());
        a.push(r.into());
    }
    a
}

fn parse_pr_url_number(s: &str) -> Option<u64> {
    let idx = s.find("/pull/")?;
    let rest = &s[idx + 6..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn timeout_message(bin: &Path) -> &'static str {
    if bin_is_git(bin) {
        "ci-wait: git timed out"
    } else {
        "ci-wait: gh timed out"
    }
}

fn not_found_message(bin: &Path) -> String {
    if bin_is_git(bin) {
        format!("git not found or not executable: {}", bin.display())
    } else {
        format!("gh not found or not executable: {}", bin.display())
    }
}

fn bin_is_git(bin: &Path) -> bool {
    bin.file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("git"))
}

pub(crate) fn git_head_sha(cwd: &Path) -> Option<String> {
    git_stdout(cwd, &["rev-parse", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn resolve_default_branch_name(cwd: &Path) -> Result<String> {
    if let Ok(def) = gh_default_branch(cwd) {
        return Ok(def);
    }
    if let Some(remote) = resolve_live_push_remote(cwd)? {
        let sym = format!("refs/remotes/{remote}/HEAD");
        if let Ok(s) = git_stdout(cwd, &["symbolic-ref", &sym]) {
            let name = s.trim().rsplit('/').next().unwrap_or("").trim();
            if !name.is_empty() {
                return Ok(name.to_string());
            }
        }
    }
    Err(CoordinatorError::Message("no default branch".into()))
}

fn resolve_live_push_remote(cwd: &Path) -> Result<Option<String>> {
    let listing = match git_stdout(cwd, &["remote"]) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    let remotes: Vec<String> = listing
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if remotes.is_empty() {
        return Ok(None);
    }
    let verbose = git_stdout(cwd, &["remote", "-v"]).unwrap_or_default();
    let mut fetch_urls: Vec<(String, String)> = Vec::new();
    for line in verbose.lines() {
        if !line.contains("(fetch)") {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(name) = parts.next() else { continue };
        let Some(url) = parts.next() else { continue };
        fetch_urls.push((name.to_string(), url.to_string()));
    }
    let branch = git_stdout(cwd, &["symbolic-ref", "--short", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "HEAD");
    let push_remote = branch.as_ref().and_then(|b| {
        git_stdout(cwd, &["config", "--get", &format!("branch.{b}.pushRemote")])
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    });
    let push_default = git_stdout(cwd, &["config", "--get", "remote.pushDefault"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let branch_remote = branch.as_ref().and_then(|b| {
        git_stdout(cwd, &["config", "--get", &format!("branch.{b}.remote")])
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    });
    let gh_url = gh_capture(cwd, &["repo", "view", "--json", "url"])
        .ok()
        .filter(|o| o.ok)
        .and_then(|o| serde_json::from_str::<serde_json::Value>(&o.stdout).ok())
        .and_then(|v| v.get("url").and_then(|u| u.as_str()).map(str::to_string));
    let remote_refs: Vec<&str> = remotes.iter().map(String::as_str).collect();
    let url_refs: Vec<(&str, &str)> = fetch_urls
        .iter()
        .map(|(n, u)| (n.as_str(), u.as_str()))
        .collect();
    Ok(resolve_push_remote_from_inputs(
        &remote_refs,
        &url_refs,
        push_remote.as_deref(),
        push_default.as_deref(),
        branch_remote.as_deref(),
        gh_url.as_deref(),
    ))
}

fn owner_repo_for_remote(cwd: &Path, remote: &str) -> Option<String> {
    let verbose = git_stdout(cwd, &["remote", "-v"]).ok()?;
    for line in verbose.lines() {
        if !line.contains("(fetch)") {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(name) = parts.next() else { continue };
        if name != remote {
            continue;
        }
        let Some(url) = parts.next() else { continue };
        let (_, owner, repo) = normalize_github_identity(url)?;
        return Some(format!("{owner}/{repo}"));
    }
    None
}

fn head_is_default_branch(cwd: &Path) -> Result<bool> {
    let head = git_stdout(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let head = head.trim();
    if let Ok(def) = gh_default_branch(cwd) {
        return Ok(head == def);
    }
    if let Some(remote) = resolve_live_push_remote(cwd)? {
        let sym = format!("refs/remotes/{remote}/HEAD");
        if let Ok(s) = git_stdout(cwd, &["symbolic-ref", &sym]) {
            let name = s.trim().rsplit('/').next().unwrap_or("");
            return Ok(!name.is_empty() && head == name);
        }
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
    run_process_timeout(bin, args, cwd, PROCESS_TIMEOUT)
}

fn run_process_timeout(bin: &Path, args: &[&str], cwd: &Path, dur: Duration) -> Result<ProcOut> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .current_dir(cwd)
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if bin_is_git(bin) {
        cmd.env("GIT_OPTIONAL_LOCKS", "0");
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(CoordinatorError::Message(not_found_message(bin)));
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
            Ok(None) if start.elapsed() >= dur => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CoordinatorError::Message(timeout_message(bin).into()));
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

/// Production Adapter omit-pick probe. Default branch is resolved once per instance.
#[derive(Default)]
pub struct GhMergedTrackProbe {
    cached_base: std::cell::RefCell<Option<String>>,
}

impl crate::workflow::MergedTrackProbe for GhMergedTrackProbe {
    fn merged_pr_for_track(&self, cwd: &Path, numeric_id: &str) -> Result<Option<u64>> {
        let base = self.default_branch(cwd)?;
        let search = crate::workflow::merged_search_query(numeric_id);
        let out = gh_capture(
            cwd,
            &[
                "pr",
                "list",
                "--state",
                "merged",
                "--base",
                &base,
                "--search",
                &search,
                "--limit",
                "10",
                "--json",
                "number,title,mergedAt,baseRefName",
            ],
        )?;
        if out.exit == 4 {
            return Err(CoordinatorError::Message("gh auth required".into()));
        }
        if !out.ok {
            return Err(CoordinatorError::Message("gh pr list failed".into()));
        }
        crate::workflow::shipped::first_merged_pr_for_track(&out.stdout, numeric_id, Some(&base))
    }
}

impl GhMergedTrackProbe {
    fn default_branch(&self, cwd: &Path) -> Result<String> {
        if let Some(b) = self.cached_base.borrow().clone() {
            return Ok(b);
        }
        let resolved = resolve_omit_pick_default_branch(cwd)?;
        *self.cached_base.borrow_mut() = Some(resolved.clone());
        Ok(resolved)
    }
}

/// Git `symbolic-ref` first, then `gh repo view` (once per omit-pick instance).
fn resolve_omit_pick_default_branch(cwd: &Path) -> Result<String> {
    if let Ok(Some(remote)) = resolve_live_push_remote(cwd) {
        let sym = format!("refs/remotes/{remote}/HEAD");
        if let Ok(s) = git_stdout(cwd, &["symbolic-ref", &sym]) {
            let name = s.trim().rsplit('/').next().unwrap_or("").trim();
            if !name.is_empty() {
                return Ok(name.to_string());
            }
        }
    }
    gh_default_branch(cwd)
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
    fn pr_list_head_args_open_then_merged_never_closed() {
        let open = pr_list_head_args("feat/foo", "open");
        let merged = pr_list_head_args("feat/foo", "merged");
        let open: Vec<&str> = open.iter().map(String::as_str).collect();
        let merged: Vec<&str> = merged.iter().map(String::as_str).collect();
        assert!(open.windows(2).any(|w| w == ["--state", "open"]));
        assert!(open.windows(2).any(|w| w == ["--head", "feat/foo"]));
        assert!(merged.windows(2).any(|w| w == ["--state", "merged"]));
        assert!(merged.windows(2).any(|w| w == ["--head", "feat/foo"]));
        let joined = [open.as_slice(), merged.as_slice()].concat();
        assert!(
            !joined.iter().any(|t| *t == "closed" || *t == "all"),
            "joined={joined:?}"
        );
        assert!(
            PR_VIEW_JSON_FIELDS.split(',').any(|f| f == "mergedAt"),
            "{PR_VIEW_JSON_FIELDS}"
        );
        assert!(
            PR_VIEW_JSON_FIELDS
                .split(',')
                .any(|f| f == "mergeStateStatus"),
            "{PR_VIEW_JSON_FIELDS}"
        );
    }

    #[test]
    fn pr_checks_args_required_and_all() {
        let required = pr_checks_args(329, true);
        let all = pr_checks_args(329, false);
        let required: Vec<&str> = required.iter().map(String::as_str).collect();
        let all: Vec<&str> = all.iter().map(String::as_str).collect();
        assert!(required.windows(2).any(|w| w == ["checks", "329"]));
        assert!(required.contains(&"--required"));
        assert!(
            required
                .windows(2)
                .any(|w| w == ["--json", "bucket,name,state"])
        );
        assert!(all.windows(2).any(|w| w == ["--json", "bucket,name,state"]));
        assert!(
            !all.contains(&"--required"),
            "all-checks argv must not contain --required: {all:?}"
        );
    }

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
    fn parse_pr_view_merged() {
        let json = r#"{"number":66,"url":"https://example/pr/66","isDraft":false,"state":"MERGED","headRefOid":"abc"}"#;
        let t = parse_pr_view(json).unwrap().unwrap();
        match t {
            CiTarget::PullRequest { number, merged, .. } => {
                assert_eq!(number, 66);
                assert!(merged);
            }
            _ => panic!("expected PR"),
        }
    }

    #[test]
    fn parse_pr_view_merge_state_unstable() {
        let json = r#"{"number":329,"url":"https://example/pr/329","isDraft":false,"state":"OPEN","headRefOid":"abc","mergeStateStatus":"UNSTABLE"}"#;
        let t = parse_pr_view(json).unwrap().unwrap();
        match t {
            CiTarget::PullRequest { merge_state, .. } => {
                assert_eq!(merge_state, MergeStateStatus::Unstable);
            }
            _ => panic!("expected PR"),
        }
    }

    #[test]
    fn parse_pr_view_missing_merge_state_is_unknown() {
        let json = r#"{"number":7,"url":"https://example/pr/7","isDraft":true,"state":"OPEN","headRefOid":"abc"}"#;
        let t = parse_pr_view(json).unwrap().unwrap();
        match t {
            CiTarget::PullRequest { merge_state, .. } => {
                assert_eq!(merge_state, MergeStateStatus::Unknown);
            }
            _ => panic!("expected PR"),
        }
    }

    #[test]
    fn parse_pr_view_merged_at_without_merged_state() {
        let json = r#"{"number":66,"url":"https://example/pr/66","isDraft":false,"state":"CLOSED","mergedAt":"2026-09-10T00:00:00Z","headRefOid":"abc"}"#;
        let t = parse_pr_view(json).unwrap().unwrap();
        match t {
            CiTarget::PullRequest { merged, .. } => assert!(merged),
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
    fn git_push_and_pr_create_args() {
        let push = git_push_args("ledgerful", "track/0317-x");
        assert_eq!(push, ["push", "-u", "ledgerful", "track/0317-x"]);
        assert!(!push.iter().any(|a| a == "HEAD"));
        let args = pr_create_args(
            "track(0010): foo",
            "body",
            "main",
            "track/0010-x",
            Some("owner/repo"),
        );
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        assert!(
            args.windows(2)
                .any(|w| w == ["--title", "track(0010): foo"])
        );
        assert!(args.windows(2).any(|w| w == ["--body", "body"]));
        assert!(args.windows(2).any(|w| w == ["--base", "main"]));
        assert!(args.windows(2).any(|w| w == ["--head", "track/0010-x"]));
        assert!(args.windows(2).any(|w| w == ["--repo", "owner/repo"]));
        assert!(!args.contains(&"--draft"));
        assert!(!args.contains(&"--fill"));
        let no_repo = pr_create_args("t", "b", "main", "head", None);
        assert!(!no_repo.iter().any(|a| a == "--repo"));
        assert_eq!(GIT_PUSH_TIMEOUT, Duration::from_secs(120));
        assert_eq!(timeout_message(Path::new("git")), "ci-wait: git timed out");
        assert_eq!(
            timeout_message(Path::new("gh.exe")),
            "ci-wait: gh timed out"
        );
        assert!(crate::workflow::shipped::pr_title_is_track(
            "track(0010): x",
            "0010"
        ));
        assert_eq!(
            parse_pr_url_number("https://github.com/o/r/pull/344\n"),
            Some(344)
        );
    }

    #[test]
    fn resolve_push_remote_hierarchy() {
        let sole = resolve_push_remote_from_inputs(
            &["ledgerful"],
            &[("ledgerful", "https://github.com/Ryan-AI-Studios/Ledgerful")],
            None,
            None,
            None,
            Some("https://github.com/Ryan-AI-Studios/Ledgerful"),
        );
        assert_eq!(sole.as_deref(), Some("ledgerful"));

        let push_remote_wins = resolve_push_remote_from_inputs(
            &["origin", "fork"],
            &[
                ("origin", "https://github.com/upstream/repo"),
                ("fork", "https://github.com/me/repo"),
            ],
            Some("fork"),
            None,
            Some("origin"),
            Some("https://github.com/upstream/repo"),
        );
        assert_eq!(push_remote_wins.as_deref(), Some("fork"));

        let dot_ignored = resolve_push_remote_from_inputs(
            &["origin"],
            &[("origin", "https://github.com/o/r.git")],
            None,
            None,
            Some("."),
            None,
        );
        assert_eq!(dot_ignored.as_deref(), Some("origin"));

        let none = resolve_push_remote_from_inputs(&[], &[], None, None, None, None);
        assert_eq!(none, None);

        let https = normalize_github_identity("https://github.com/Owner/Repo.git");
        let ssh = normalize_github_identity("git@github.com:Owner/Repo.git");
        assert_eq!(https, ssh);
        assert_eq!(
            https,
            Some(("github.com".into(), "owner".into(), "repo".into()))
        );
        let ghes = normalize_github_identity("https://git.example.com/Acme/App");
        assert_eq!(
            ghes,
            Some(("git.example.com".into(), "acme".into(), "app".into()))
        );
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
