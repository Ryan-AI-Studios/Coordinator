//! Operator-invoked build/smoke/swap of the installed binary (track 0108).
//!
//! `coordinator self-update` is the product-owned replacement for the external
//! build-and-swap cron. It is **operator-invoked only**: there is no timer, no
//! serve-tick hook and no autonomous deploy. A live `coordinator.exe` is a
//! refusal, never a target; the product never stops, restarts or kills anything
//! except its own `cargo build` child on build timeout.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::error::{CoordinatorError, Result};
use crate::features::{self, Duty};
use crate::state::RunStatus;
use crate::workflow::execution_repo::paths_same;

mod process;
mod swap;

pub(crate) use crate::selfupdate::process::{
    ProcessRow, ProcessSource, WindowsProcessSource, is_build_tool_image, is_peer_image,
};
pub(crate) use crate::selfupdate::swap::{
    SwapError, SwapReceipt, SwapRequest, finish, journal_path, read_journal, recovery_message,
    rollback, sha256_file, swap,
};

/// Files under these roots with `now - mtime < 120 s` block the swap.
pub(crate) const RECENT_WRITE_WINDOW: Duration = Duration::from_secs(120);
/// Production entry cap for the tree-write walk. Never lowered for a test.
pub(crate) const TREE_SCAN_MAX: usize = 10_000;
/// Maximum directory depth for the tree-write walk.
pub(crate) const TREE_SCAN_MAX_DEPTH: usize = 8;
/// A build child is our own process: kill+reap on this wall clock.
pub(crate) const BUILD_TIMEOUT: Duration = Duration::from_secs(900);
/// Each smoke invocation is bounded.
pub(crate) const SMOKE_TIMEOUT: Duration = Duration::from_secs(60);
/// A product lock older than this is broken once and retried.
pub(crate) const LOCK_STALE: Duration = Duration::from_secs(30 * 60);

/// Resolved CLI options.
#[derive(Debug, Clone)]
pub struct Options {
    pub repo: PathBuf,
    pub install: PathBuf,
    pub artifact: Option<PathBuf>,
    pub check: bool,
}

/// Repo facts read by the gate.
#[derive(Debug, Clone)]
pub(crate) struct RepoFacts {
    pub toplevel: PathBuf,
    pub branch: String,
    pub head_sha: String,
    pub dirty: Vec<String>,
    #[allow(dead_code)]
    pub index_lock: PathBuf,
    pub index_lock_exists: bool,
    pub package_name_ok: bool,
}

/// One recent tree write.
#[derive(Debug, Clone, Serialize)]
pub struct RecentWrite {
    pub path: PathBuf,
    pub age_secs: u64,
}

/// Injectable repository probe.
pub(crate) trait RepoProbe {
    fn inspect(&self, repo: &Path) -> Result<RepoFacts>;
}

/// Production git + Cargo.toml probe.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct GitRepoProbe;

impl RepoProbe for GitRepoProbe {
    fn inspect(&self, repo: &Path) -> Result<RepoFacts> {
        let toplevel = PathBuf::from(git_read(repo, &["rev-parse", "--show-toplevel"])?);
        let branch = git_read(repo, &["rev-parse", "--abbrev-ref", "HEAD"])?;
        let head_sha = git_read(repo, &["rev-parse", "HEAD"])?;
        let git_index_path = git_read(repo, &["rev-parse", "--git-path", "index.lock"])?;
        let mut index_lock = PathBuf::from(&git_index_path);
        if !index_lock.is_absolute() {
            index_lock = repo.join(index_lock);
        }
        let index_lock_exists = index_lock.exists();
        Ok(RepoFacts {
            toplevel,
            branch,
            head_sha,
            dirty: crate::checkpoint::committable_dirt(repo)?,
            index_lock,
            index_lock_exists,
            package_name_ok: package_name_is_coordinator(repo),
        })
    }
}

fn git_read(repo: &Path, args: &[&str]) -> Result<String> {
    let output = crate::git_command::command()
        .args(args)
        .current_dir(repo)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()?;
    if !output.status.success() {
        return Err(CoordinatorError::Message(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `[package] name = "coordinator"`, before the next section.
fn package_name_is_coordinator(repo: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(repo.join("Cargo.toml")) else {
        return false;
    };
    let mut in_package = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some((key, value)) = line.split_once('=')
            && key.trim() == "name"
        {
            return value.trim().trim_matches('"') == "coordinator";
        }
    }
    false
}

/// Files with a fresh mtime under the scan roots; `incomplete` is a refusal.
#[allow(dead_code)]
pub(crate) fn recent_writes(repo: &Path, now: DateTime<Utc>) -> Result<(Vec<RecentWrite>, bool)> {
    recent_writes_with(repo, now, TREE_SCAN_MAX)
}

/// Production wrapper over the injectable-cap walk.
pub(crate) fn recent_writes_with(
    repo: &Path,
    now: DateTime<Utc>,
    max_entries: usize,
) -> Result<(Vec<RecentWrite>, bool)> {
    let now_secs = now.timestamp();
    let mut recent = Vec::new();
    let mut entries = 0usize;
    let mut incomplete = false;

    let mut stack: Vec<(PathBuf, usize)> = Vec::new();
    for dir in [repo.join("src"), repo.join(".github").join("workflows")] {
        match std::fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() => stack.push((dir, 0)),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => incomplete = true,
        }
    }
    for file in [
        "Cargo.toml",
        "Cargo.lock",
        "build.rs",
        "README.md",
        "AGENTS.md",
    ] {
        let path = repo.join(file);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                incomplete = true;
                continue;
            }
        }
        entries += 1;
        if entries > max_entries {
            return Ok((recent, true));
        }
        match fresh_write(&path, now_secs) {
            Ok(Some(found)) => recent.push(found),
            Ok(None) => {}
            Err(()) => incomplete = true,
        }
    }

    while let Some((dir, depth)) = stack.pop() {
        let read = match std::fs::read_dir(&dir) {
            Ok(read) => read,
            // A root removed between enqueue and read is simply gone, not an
            // unreadable answer; anything else is incomplete (fail closed).
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                incomplete = true;
                continue;
            }
        };
        for entry in read {
            let Ok(entry) = entry else {
                incomplete = true;
                continue;
            };
            entries += 1;
            if entries > max_entries {
                return Ok((recent, true));
            }
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => {
                    incomplete = true;
                    continue;
                }
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if depth + 1 > TREE_SCAN_MAX_DEPTH {
                    incomplete = true;
                } else {
                    stack.push((path, depth + 1));
                }
            } else {
                match fresh_write(&path, now_secs) {
                    Ok(Some(found)) => recent.push(found),
                    Ok(None) => {}
                    Err(()) => incomplete = true,
                }
            }
        }
    }
    Ok((recent, incomplete))
}

fn fresh_write(path: &Path, now_secs: i64) -> std::result::Result<Option<RecentWrite>, ()> {
    // `symlink_metadata`: a symlink is skipped, never followed.
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    if metadata.is_symlink() || !metadata.is_file() {
        return Ok(None);
    }
    let modified = metadata.modified().map_err(|_| ())?;
    let mtime_secs = modified
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let age = now_secs - mtime_secs;
    // A negative age (clock moved back) is recent: refuse rather than guess.
    if age < 0 || (age as u64) < RECENT_WRITE_WINDOW.as_secs() {
        Ok(Some(RecentWrite {
            path: path.to_path_buf(),
            age_secs: age.max(0) as u64,
        }))
    } else {
        Ok(None)
    }
}

/// One gate collection. Every signal is populated; none short-circuits another.
#[derive(Debug, Clone, Default, Serialize)]
pub struct BusyReport {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub feature_off: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub not_product_repo: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub not_on_main: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dirty: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recent_writes: Vec<RecentWrite>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub index_lock: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub build_tools: Vec<ProcessRow>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub live_coordinator: Vec<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub active_runs: Vec<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub process_table_error: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub tree_scan_incomplete: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub lock_held: bool,
    pub head_sha: String,
    #[serde(skip)]
    pub branch: String,
}

impl BusyReport {
    /// Exhaustive over every refusal field; `head_sha` is recorded, not a refusal.
    pub(crate) fn reasons(&self) -> Vec<&'static str> {
        let mut reasons = Vec::new();
        if self.feature_off {
            reasons.push("feature-off");
        }
        if self.not_product_repo {
            reasons.push("not-product-repo");
        }
        if self.not_on_main {
            reasons.push("not-on-main");
        }
        if !self.dirty.is_empty() {
            reasons.push("dirty-tree");
        }
        if !self.recent_writes.is_empty() {
            reasons.push("recent-writes");
        }
        if self.index_lock {
            reasons.push("git-index-lock");
        }
        if !self.build_tools.is_empty() {
            reasons.push("build-tool-running");
        }
        if !self.live_coordinator.is_empty() {
            reasons.push("live-coordinator");
        }
        if !self.active_runs.is_empty() {
            reasons.push("active-run");
        }
        if self.process_table_error {
            reasons.push("process-table-unreadable");
        }
        if self.tree_scan_incomplete {
            reasons.push("tree-scan-incomplete");
        }
        if self.lock_held {
            reasons.push("lock-held");
        }
        reasons
    }

    pub(crate) fn refuses(&self) -> bool {
        !self.reasons().is_empty()
    }
}

/// sha256 + size identity of one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileIdentity {
    pub path: PathBuf,
    pub sha256: String,
    pub size: u64,
}

/// The process that made the decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DecidedBy {
    pub pid: u32,
    pub exe: PathBuf,
    pub sha256: String,
    pub version: String,
}

/// Ledger schema v1 (`{COORDINATOR_HOME}/self-update.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct LedgerV1 {
    pub version: u32,
    pub head_sha: String,
    pub branch: String,
    pub artifact: FileIdentity,
    pub installed: FileIdentity,
    pub previous: Option<FileIdentity>,
    pub deployed_at: DateTime<Utc>,
    pub decided_by: DecidedBy,
}

/// CLI/JSON outcome. Every stop before a verified install is `refused`.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub outcome: &'static str,
    pub reasons: Vec<&'static str>,
    pub would_deploy: Option<bool>,
    pub already_deployed: bool,
    pub busy: BusyReport,
    pub repo: PathBuf,
    pub install: PathBuf,
    pub head_sha: String,
    pub branch: String,
    pub artifact: Option<FileIdentity>,
    pub installed: Option<FileIdentity>,
    pub old: Option<FileIdentity>,
    pub ledger: Option<PathBuf>,
    pub decided_by: Option<DecidedBy>,
    pub rotated_old: Option<PathBuf>,
    pub detail: Option<String>,
}

impl Report {
    pub fn ok(&self) -> bool {
        matches!(self.outcome, "deployed" | "already-deployed")
            || (self.outcome == "check" && self.reasons.is_empty())
    }

    /// Exactly one stderr line on success; a specific refusal summary otherwise.
    pub fn summary(&self) -> String {
        match self.outcome {
            "deployed" => {
                let head10 = short_sha(&self.head_sha);
                let artifact10 = self
                    .artifact
                    .as_ref()
                    .map(|a| short_sha(&a.sha256))
                    .unwrap_or_else(|| "unknown".to_string());
                let install = self
                    .installed
                    .as_ref()
                    .map(|i| i.path.display().to_string())
                    .unwrap_or_else(|| self.install.display().to_string());
                let mut line = match self.old.as_ref() {
                    Some(previous) => format!(
                        "coordinator: self-update: deployed {head10} ({artifact10} -> {install}); \
                         previous kept at {}",
                        previous.path.display()
                    ),
                    None => format!(
                        "coordinator: self-update: deployed {head10} ({artifact10} -> {install}); \
                         first install (no previous binary)"
                    ),
                };
                if let Some(rotated) = &self.rotated_old {
                    line.push_str(&format!("; older copy rotated to {}", rotated.display()));
                }
                line
            }
            "already-deployed" => format!(
                "coordinator: self-update: already deployed {}",
                short_sha(&self.head_sha)
            ),
            "check" => format!(
                "coordinator: self-update: check would_deploy={} already_deployed={}",
                self.would_deploy.unwrap_or(false),
                self.already_deployed
            ),
            "rolled-back" => {
                let mut line = if self.old.is_some() {
                    "coordinator: self-update: rolled back; previous binary restored".to_string()
                } else {
                    "coordinator: self-update: rolled back; rejected install removed (first install; \
                     no previous binary)"
                        .to_string()
                };
                if let Some(detail) = &self.detail {
                    line.push_str(&format!(" ({detail})"));
                }
                line
            }
            _ => {
                let mut line = format!("self-update refused: {}", self.reasons.join(", "));
                if let Some(detail) = &self.detail {
                    line.push_str(&format!(": {detail}"));
                }
                line
            }
        }
    }
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(10).collect()
}

fn base_report(options: &Options, decided_by: Option<DecidedBy>) -> Report {
    Report {
        outcome: "refused",
        reasons: Vec::new(),
        would_deploy: None,
        already_deployed: false,
        busy: BusyReport::default(),
        repo: options.repo.clone(),
        install: options.install.clone(),
        head_sha: String::new(),
        branch: String::new(),
        artifact: None,
        installed: None,
        old: None,
        ledger: None,
        decided_by,
        rotated_old: None,
        detail: None,
    }
}

fn refused_report(
    options: &Options,
    busy: BusyReport,
    reasons: Vec<&'static str>,
    detail: Option<String>,
    decided_by: Option<DecidedBy>,
) -> Report {
    let mut report = base_report(options, decided_by);
    report.busy = busy;
    report.head_sha = report.busy.head_sha.clone();
    report.branch = report.busy.branch.clone();
    report.reasons = reasons;
    report.detail = detail;
    report
}

/// Injectable build step.
pub(crate) enum BuildResult {
    Ok {
        artifact: PathBuf,
        #[allow(dead_code)]
        tail: String,
    },
    Failed {
        code: Option<i32>,
        tail: String,
    },
    TimedOut,
}

pub(crate) trait Builder {
    fn build(&self, repo: &Path) -> BuildResult;
}

/// Injectable smoke step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SmokeResult {
    pub ok: bool,
    pub version: String,
    pub detail: String,
}

pub(crate) trait SmokeCheck {
    fn check(&self, exe: &Path) -> SmokeResult;
}

/// Production build: `cargo build --release --locked` with a bounded child.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct CargoBuilder;

impl Builder for CargoBuilder {
    fn build(&self, repo: &Path) -> BuildResult {
        match run_bounded(
            "cargo",
            &["build", "--release", "--locked"],
            Some(repo),
            BUILD_TIMEOUT,
        ) {
            Err(e) => BuildResult::Failed {
                code: None,
                tail: e.to_string(),
            },
            Ok(captured) if captured.timed_out => BuildResult::TimedOut,
            Ok(captured) => {
                let tail = last_chars(&format!("{}{}", captured.stdout, captured.stderr), 2_000);
                if captured.status != Some(0) {
                    return BuildResult::Failed {
                        code: captured.status,
                        tail,
                    };
                }
                let artifact = default_artifact(repo);
                if !artifact.exists() {
                    return BuildResult::Failed {
                        code: Some(0),
                        tail: "artifact missing".to_string(),
                    };
                }
                BuildResult::Ok { artifact, tail }
            }
        }
    }
}

/// Production smoke: `--version` prefix and `--help` literal `self-update`.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ExecSmoke;

impl SmokeCheck for ExecSmoke {
    fn check(&self, exe: &Path) -> SmokeResult {
        let version = match run_bounded(exe.as_os_str(), &["--version"], None, SMOKE_TIMEOUT) {
            Err(e) => {
                return SmokeResult {
                    ok: false,
                    version: String::new(),
                    detail: format!("{} --version failed to spawn: {e}", exe.display()),
                };
            }
            Ok(captured) if captured.timed_out => {
                return SmokeResult {
                    ok: false,
                    version: String::new(),
                    detail: format!("{} --version timed out", exe.display()),
                };
            }
            Ok(captured) => captured,
        };
        let line = version
            .stdout
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if version.status != Some(0) || !line.starts_with("coordinator ") {
            return SmokeResult {
                ok: false,
                version: line,
                detail: format!(
                    "{} --version exit {:?}; expected exit 0 and stdout starting 'coordinator '",
                    exe.display(),
                    version.status
                ),
            };
        }
        match run_bounded(exe.as_os_str(), &["--help"], None, SMOKE_TIMEOUT) {
            Err(e) => SmokeResult {
                ok: false,
                version: line,
                detail: format!("{} --help failed to spawn: {e}", exe.display()),
            },
            Ok(captured) if captured.timed_out => SmokeResult {
                ok: false,
                version: line,
                detail: format!("{} --help timed out", exe.display()),
            },
            Ok(captured) => {
                if captured.status != Some(0) || !captured.stdout.contains("self-update") {
                    SmokeResult {
                        ok: false,
                        version: line,
                        detail: format!(
                            "{} --help exit {:?}; expected exit 0 and a literal 'self-update'",
                            exe.display(),
                            captured.status
                        ),
                    }
                } else {
                    SmokeResult {
                        ok: true,
                        version: line,
                        detail: String::new(),
                    }
                }
            }
        }
    }
}

struct Captured {
    status: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

fn run_bounded(
    program: impl AsRef<std::ffi::OsStr>,
    args: &[&str],
    cwd: Option<&Path>,
    timeout: Duration,
) -> std::io::Result<Captured> {
    let dir = crate::config::machine_home().unwrap_or_else(|_| std::env::temp_dir());
    std::fs::create_dir_all(&dir)?;
    let unique = format!(
        "coordinator-self-update-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    );
    let out_path = dir.join(format!("{unique}.out"));
    let err_path = dir.join(format!("{unique}.err"));
    let out = std::fs::File::create(&out_path)?;
    let err = std::fs::File::create(&err_path)?;

    let mut command = Command::new(program);
    command
        .args(args)
        .stdout(Stdio::from(out.try_clone()?))
        .stderr(Stdio::from(err.try_clone()?));
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn()?;

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            // Only our own child: this is the build/smoke bound, not a process kill.
            let _ = child.kill();
            let _ = child.wait();
            timed_out = true;
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let stdout = std::fs::read(&out_path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let stderr = std::fs::read(&err_path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let _ = std::fs::remove_file(&out_path);
    let _ = std::fs::remove_file(&err_path);
    Ok(Captured {
        status: status.and_then(|s| s.code()),
        stdout,
        stderr,
        timed_out,
    })
}

fn last_chars(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    if count <= limit {
        text.to_string()
    } else {
        text.chars().skip(count - limit).collect()
    }
}

fn default_artifact(repo: &Path) -> PathBuf {
    let target = match std::env::var_os("CARGO_TARGET_DIR") {
        Some(value) => {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                path
            } else {
                // Cargo resolves a relative target dir against its cwd (repo).
                repo.join(path)
            }
        }
        None => repo.join("target"),
    };
    target.join("release").join("coordinator.exe")
}

/// Product-side lock (`{COORDINATOR_HOME}/self-update.lock`).
struct UpdateLock {
    path: PathBuf,
}

impl UpdateLock {
    /// `Ok(None)` = held by another run. A stale lock is broken once and retried.
    fn acquire(home: &Path, now: DateTime<Utc>) -> Result<Option<Self>> {
        std::fs::create_dir_all(home)?;
        let path = home.join("self-update.lock");
        for attempt in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    let body = format!(
                        "{{\"pid\":{},\"started_at\":\"{}\"}}\n",
                        std::process::id(),
                        now.to_rfc3339()
                    );
                    if let Err(e) = file.write_all(body.as_bytes()) {
                        let _ = std::fs::remove_file(&path);
                        return Err(e.into());
                    }
                    return Ok(Some(Self { path }));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = lock_age_secs(&path, now)
                        .map(|age| age > LOCK_STALE.as_secs())
                        .unwrap_or(false);
                    if stale && attempt == 0 {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(None)
    }
}

impl Drop for UpdateLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn lock_age_secs(path: &Path, now: DateTime<Utc>) -> Option<u64> {
    let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
    let secs = mtime.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    Some((now.timestamp() - secs).max(0) as u64)
}

/// 0102 gate without creating `{COORDINATOR_HOME}` (`--check` must not touch it).
/// Same evaluation path as `features::is_enabled` (env > config > default,
/// fail closed), but the config path is read without `ensure_machine_home`.
fn self_update_enabled() -> bool {
    let loaded = match crate::config::machine_home() {
        Ok(home) => crate::config::load_machine_config_at(&home.join("config.json")),
        Err(e) => Err(e),
    };
    features::evaluate_loaded(&loaded).enabled(Duty::SelfUpdate)
}

/// Run-state gate: a registered project that owns this repo must be terminal.
///
/// Fail closed: an unreadable registry or run-state is reported as an active
/// run (never "idle" by default).
fn active_runs_for(repo: &Path) -> Vec<String> {
    // Read the registry without `ensure_machine_home`: `--check` must not create
    // the machine home (or any other file). Metadata errors and non-files fail
    // closed; only `NotFound` means "no registry yet".
    let registry_path = match crate::config::machine_home() {
        Ok(home) => home.join("registry.json"),
        Err(e) => return vec![format!("<registry unreadable: {e}>")],
    };
    let registry = match std::fs::symlink_metadata(&registry_path) {
        Ok(metadata) if metadata.is_file() => {
            match crate::registry::Registry::load(&registry_path) {
                Ok(registry) => registry,
                Err(e) => return vec![format!("<registry unreadable: {e}>")],
            }
        }
        Ok(_) => {
            return vec![format!(
                "<registry unreadable: {} is not a regular file>",
                registry_path.display()
            )];
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => crate::registry::Registry::default(),
        Err(e) => return vec![format!("<registry unreadable: {e}>")],
    };
    let mut active = Vec::new();
    for project in registry.list() {
        let owns = paths_same(&project.path, repo)
            || project
                .execution_repo
                .as_deref()
                .is_some_and(|path| paths_same(path, repo))
            || project
                .execution_repos
                .values()
                .any(|path| paths_same(path, repo));
        if !owns {
            continue;
        }
        match crate::state::run_state_path(project) {
            Ok(path) => match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_file() => match crate::state::load_run_state(project) {
                    Ok(state) if matches!(state.status, RunStatus::Running | RunStatus::Paused) => {
                        active.push(project.id.clone());
                    }
                    Ok(_) => {}
                    Err(e) => active.push(format!("{}: <run-state unreadable: {e}>", project.id)),
                },
                Ok(_) => active.push(format!(
                    "{}: <run-state unreadable: {} is not a regular file>",
                    project.id,
                    path.display()
                )),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => active.push(format!("{}: <run-state unreadable: {e}>", project.id)),
            },
            Err(e) => active.push(format!("{}: <run-state unreadable: {e}>", project.id)),
        }
    }
    active
}

fn collect_gates(
    repo: &Path,
    now: DateTime<Utc>,
    deps: &Deps<'_>,
    lock_held: bool,
) -> Result<BusyReport> {
    let facts = deps.repo.inspect(repo)?;
    let rows = deps.processes.snapshot();
    Ok(collect_from(
        repo,
        facts,
        rows,
        deps.recent_write_cap,
        lock_held,
        now,
    ))
}

fn collect_from(
    repo: &Path,
    facts: RepoFacts,
    rows: Result<Vec<ProcessRow>>,
    recent_write_cap: usize,
    lock_held: bool,
    now: DateTime<Utc>,
) -> BusyReport {
    let mut busy = BusyReport {
        lock_held,
        head_sha: facts.head_sha.clone(),
        branch: facts.branch.clone(),
        ..BusyReport::default()
    };
    busy.not_product_repo = !paths_same(&facts.toplevel, repo) || !facts.package_name_ok;
    busy.not_on_main = facts.branch != "main";
    busy.dirty = facts.dirty;
    busy.index_lock = facts.index_lock_exists;

    match recent_writes_with(repo, now, recent_write_cap) {
        Ok((recent, incomplete)) => {
            busy.recent_writes = recent;
            busy.tree_scan_incomplete = incomplete;
        }
        Err(_) => busy.tree_scan_incomplete = true,
    }

    match rows {
        Ok(rows) => {
            busy.build_tools = rows
                .iter()
                .filter(|row| is_build_tool_image(&row.image))
                .cloned()
                .collect();
            let own_pid = std::process::id();
            busy.live_coordinator = rows
                .iter()
                .filter(|row| is_peer_image(&row.image) && row.pid != own_pid)
                .map(|row| row.pid)
                .collect();
        }
        Err(_) => busy.process_table_error = true,
    }

    busy.active_runs = active_runs_for(repo);
    busy
}

fn decided_by() -> Result<DecidedBy> {
    let exe = std::env::current_exe()?;
    let (sha256, _) = sha256_file(&exe)?;
    Ok(DecidedBy {
        pid: std::process::id(),
        exe,
        sha256,
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

fn identity(path: &Path) -> Option<FileIdentity> {
    sha256_file(path).ok().map(|(sha256, size)| FileIdentity {
        path: path.to_path_buf(),
        sha256,
        size,
    })
}

fn bytes_equal(a: &Path, b: &Path) -> bool {
    match (sha256_file(a), sha256_file(b)) {
        (Ok((left, _)), Ok((right, _))) => left == right,
        _ => false,
    }
}

fn write_ledger(
    home: &Path,
    options: &Options,
    artifact_path: &Path,
    receipt: &SwapReceipt,
    facts: &RepoFacts,
    decided_by: &DecidedBy,
    now: DateTime<Utc>,
) -> Result<(PathBuf, LedgerV1)> {
    let artifact = identity(artifact_path).ok_or_else(|| {
        CoordinatorError::Message("cannot hash artifact for the ledger".to_string())
    })?;
    let installed = identity(&options.install).ok_or_else(|| {
        CoordinatorError::Message("cannot hash installed file for the ledger".to_string())
    })?;
    let previous = receipt.previous_path.as_deref().and_then(identity);
    let ledger = LedgerV1 {
        version: 1,
        head_sha: facts.head_sha.clone(),
        branch: facts.branch.clone(),
        artifact,
        installed,
        previous,
        deployed_at: now,
        decided_by: decided_by.clone(),
    };
    let path = home.join("self-update.json");
    crate::persist::atomic_write_json(&path, &ledger)?;
    Ok((path, ledger))
}

/// Injectables for one run (production passes the real implementations).
pub(crate) struct Deps<'a> {
    pub processes: &'a dyn ProcessSource,
    pub repo: &'a dyn RepoProbe,
    pub builder: &'a dyn Builder,
    pub smoke: &'a dyn SmokeCheck,
    pub recent_write_cap: usize,
}

/// Production swap, with a test-only one-shot hook so the divergence row can
/// prove that `run_with` (not `swap_with`) owns the rollback.
fn perform_swap(req: &SwapRequest) -> std::result::Result<SwapReceipt, SwapError> {
    #[cfg(test)]
    {
        if let Some(hook) = TEST_SWAP_HOOK.with(|slot| slot.borrow_mut().take()) {
            return hook(req);
        }
    }
    swap(req)
}

#[cfg(test)]
type TestSwapHook = Box<dyn Fn(&SwapRequest) -> std::result::Result<SwapReceipt, SwapError>>;

#[cfg(test)]
thread_local! {
    static TEST_SWAP_HOOK: std::cell::RefCell<Option<TestSwapHook>> =
        const { std::cell::RefCell::new(None) };
}

/// Installs a one-shot swap hook for this test thread; clears it on drop.
#[cfg(test)]
struct TestSwapGuard;

#[cfg(test)]
impl TestSwapGuard {
    fn install(hook: TestSwapHook) -> Self {
        TEST_SWAP_HOOK.with(|slot| *slot.borrow_mut() = Some(hook));
        Self
    }
}

#[cfg(test)]
impl Drop for TestSwapGuard {
    fn drop(&mut self) {
        TEST_SWAP_HOOK.with(|slot| {
            slot.borrow_mut().take();
        });
    }
}

/// The whole decision. Read-only until a verified install; `--check` never writes.
pub(crate) fn run_with(options: &Options, deps: &Deps<'_>, now: DateTime<Utc>) -> Result<Report> {
    // 0102 veto first: no process snapshot, git read, build or file write.
    if !self_update_enabled() {
        let busy = BusyReport {
            feature_off: true,
            ..BusyReport::default()
        };
        return Ok(refused_report(
            options,
            busy,
            vec!["feature-off"],
            Some("Duty::SelfUpdate is disabled".to_string()),
            None,
        ));
    }

    let home = crate::config::machine_home()?;
    let decided = decided_by()?;

    let mut lock = None;
    if !options.check {
        match UpdateLock::acquire(&home, now)? {
            Some(held) => lock = Some(held),
            None => {
                let busy = BusyReport {
                    lock_held: true,
                    ..BusyReport::default()
                };
                return Ok(refused_report(
                    options,
                    busy,
                    vec!["lock-held"],
                    Some(format!(
                        "another self-update holds {}",
                        home.join("self-update.lock").display()
                    )),
                    Some(decided),
                ));
            }
        }
    }
    let lock_held = options.check && home.join("self-update.lock").exists();

    let busy = match collect_gates(&options.repo, now, deps, lock_held) {
        Ok(busy) => busy,
        Err(e) => {
            return Ok(refused_report(
                options,
                BusyReport::default(),
                vec!["repo-unreadable"],
                Some(e.to_string()),
                Some(decided),
            ));
        }
    };
    if busy.refuses() {
        return Ok(refused_report(
            options,
            busy.clone(),
            busy.reasons(),
            None,
            Some(decided),
        ));
    }

    // Entry-journal check: before `--check`'s decision, before the build and
    // before the already-deployed short-circuit. A bytes-equal crash state must
    // never hide behind `already-deployed`. The journal is kept for the operator.
    match read_journal(&journal_path(&options.install)) {
        Ok(Some(journal)) => {
            let detail = recovery_message(&journal);
            if options.check {
                let mut report = refused_report(
                    options,
                    busy,
                    vec!["interrupted"],
                    Some(detail),
                    Some(decided),
                );
                report.outcome = "check";
                report.would_deploy = Some(false);
                report.already_deployed = false;
                return Ok(report);
            }
            return Ok(refused_report(
                options,
                busy,
                vec!["interrupted"],
                Some(detail),
                Some(decided),
            ));
        }
        Ok(None) => {}
        Err(e) => {
            return Ok(refused_report(
                options,
                busy,
                vec!["swap-io"],
                Some(e.to_string()),
                Some(decided),
            ));
        }
    }

    if options.check {
        let artifact = options
            .artifact
            .clone()
            .unwrap_or_else(|| default_artifact(&options.repo));
        let already = artifact.exists()
            && options.install.exists()
            && bytes_equal(&options.install, &artifact);
        let mut report = base_report(options, Some(decided));
        report.outcome = "check";
        report.would_deploy = Some(!already);
        report.already_deployed = already;
        report.busy = busy.clone();
        report.head_sha = busy.head_sha;
        report.branch = busy.branch;
        report.artifact = identity(&artifact);
        report.installed = identity(&options.install);
        return Ok(report);
    }

    let pre_head = busy.head_sha.clone();
    let artifact = if let Some(artifact) = &options.artifact {
        if !artifact.exists() {
            return Ok(refused_report(
                options,
                busy,
                vec!["artifact-missing"],
                Some(format!("artifact not found: {}", artifact.display())),
                Some(decided),
            ));
        }
        artifact.clone()
    } else {
        match deps.builder.build(&options.repo) {
            BuildResult::Ok { artifact, .. } => artifact,
            BuildResult::Failed { code, tail } => {
                let reason = if code == Some(0) && tail.contains("artifact missing") {
                    "artifact-missing"
                } else {
                    "build-failed"
                };
                return Ok(refused_report(
                    options,
                    busy,
                    vec![reason],
                    Some(tail),
                    Some(decided),
                ));
            }
            BuildResult::TimedOut => {
                return Ok(refused_report(
                    options,
                    busy,
                    vec!["build-timeout"],
                    Some(format!(
                        "cargo build did not finish within {} s",
                        BUILD_TIMEOUT.as_secs()
                    )),
                    Some(decided),
                ));
            }
        }
    };

    // Second gate: the build must not have raced a working tree or moved HEAD.
    let facts2 = match deps.repo.inspect(&options.repo) {
        Ok(facts) => facts,
        Err(e) => {
            return Ok(refused_report(
                options,
                BusyReport::default(),
                vec!["repo-unreadable"],
                Some(e.to_string()),
                Some(decided),
            ));
        }
    };
    let rows2 = deps.processes.snapshot();
    let busy2 = collect_from(
        &options.repo,
        facts2.clone(),
        rows2,
        deps.recent_write_cap,
        false,
        now,
    );
    if facts2.head_sha != pre_head || busy2.refuses() {
        let mut reasons: Vec<&'static str> = Vec::new();
        if facts2.head_sha != pre_head {
            reasons.push("changed-during-build");
        }
        for reason in busy2.reasons() {
            if !reasons.contains(&reason) {
                reasons.push(reason);
            }
        }
        return Ok(refused_report(options, busy2, reasons, None, Some(decided)));
    }

    let install_sha = if options.install.exists() {
        match sha256_file(&options.install) {
            Ok((sha, _)) => Some(sha),
            Err(e) => {
                return Ok(refused_report(
                    options,
                    busy2,
                    vec!["swap-io"],
                    Some(format!(
                        "cannot hash install {} before the swap: {e}",
                        options.install.display()
                    )),
                    Some(decided),
                ));
            }
        }
    } else {
        None
    };
    let artifact_sha = match sha256_file(&artifact) {
        Ok((sha, _)) => sha,
        Err(e) => {
            return Ok(refused_report(
                options,
                busy2,
                vec!["swap-io"],
                Some(format!(
                    "cannot hash artifact {} before the swap: {e}",
                    artifact.display()
                )),
                Some(decided),
            ));
        }
    };
    if install_sha.as_deref() == Some(artifact_sha.as_str()) {
        let mut report = base_report(options, Some(decided));
        report.outcome = "already-deployed";
        report.already_deployed = true;
        report.busy = busy2;
        report.head_sha = facts2.head_sha;
        report.branch = facts2.branch;
        report.artifact = identity(&artifact);
        report.installed = identity(&options.install);
        return Ok(report);
    }

    let pre = deps.smoke.check(&artifact);
    if !pre.ok {
        return Ok(refused_report(
            options,
            busy2,
            vec!["smoke-failed"],
            Some(pre.detail),
            Some(decided),
        ));
    }

    let request = SwapRequest {
        install: options.install.clone(),
        artifact: artifact.clone(),
        head_sha: pre_head.clone(),
        previous_sha256: install_sha.clone(),
    };
    let receipt = match perform_swap(&request) {
        Ok(receipt) => receipt,
        Err(e) => {
            return Ok(refused_report(
                options,
                busy2,
                vec![swap_error_reason(&e)],
                Some(e.detail()),
                Some(decided),
            ));
        }
    };

    let post = deps.smoke.check(&options.install);
    if !post.ok || receipt.installed_sha256 != receipt.artifact_sha256 {
        if let Err(e) = rollback(&request, install_sha.as_deref()) {
            return Ok(refused_report(
                options,
                busy2,
                vec![swap_error_reason(&e)],
                Some(e.detail()),
                Some(decided),
            ));
        }
        let mut report = base_report(options, Some(decided));
        report.outcome = "rolled-back";
        report.busy = busy2;
        report.head_sha = facts2.head_sha;
        report.branch = facts2.branch;
        report.installed = identity(&options.install);
        report.old = receipt.previous_path.as_deref().and_then(identity);
        report.detail = Some(if post.ok {
            format!(
                "sha mismatch: installed {} != artifact {}",
                short_sha(&receipt.installed_sha256),
                short_sha(&receipt.artifact_sha256)
            )
        } else {
            post.detail
        });
        return Ok(report);
    }

    let (ledger_path, ledger) =
        match write_ledger(&home, options, &artifact, &receipt, &facts2, &decided, now) {
            Ok(written) => written,
            Err(e) => {
                // The install is already the new bytes; the journal stays at
                // `verifying` so the next run's recovery sees the real state.
                return Ok(refused_report(
                    options,
                    busy2,
                    vec!["swap-io"],
                    Some(format!("ledger write failed after verified swap: {e}")),
                    Some(decided),
                ));
            }
        };
    let cleanup_detail = match finish(&options.install) {
        Ok(()) => None,
        // The install is already verified and the ledger records it; a failed
        // journal removal must not relabel a verified deploy as refused (that
        // would leave a ledger behind a non-Deployed outcome). The journal is
        // kept and the next run's `interrupted` recovery removes it.
        Err(e) => Some(format!("journal cleanup failed: {e}")),
    };

    let mut report = base_report(options, Some(decided));
    report.outcome = "deployed";
    report.busy = busy2;
    report.head_sha = ledger.head_sha;
    report.branch = ledger.branch;
    report.artifact = Some(ledger.artifact);
    report.installed = Some(ledger.installed);
    report.old = ledger.previous;
    report.ledger = Some(ledger_path);
    report.rotated_old = receipt.rotated_old;
    report.detail = cleanup_detail;
    drop(lock);
    Ok(report)
}

/// Wildcard-free arm mapping: a new `SwapError` variant must fail to compile.
pub(crate) fn swap_error_reason(e: &SwapError) -> &'static str {
    match e {
        SwapError::Io { .. } => "swap-io",
        SwapError::RollbackMismatch { .. } => "rollback-mismatch",
        SwapError::Pinned { .. } => "old-pinned",
        SwapError::Interrupted { .. } => "interrupted",
        SwapError::StagingMismatch { .. } => "staging-mismatch",
    }
}

/// CLI entry: resolve defaults, use production deps.
pub(crate) fn cmd_self_update(
    repo: Option<PathBuf>,
    install: Option<PathBuf>,
    artifact: Option<PathBuf>,
    check: bool,
) -> Result<Report> {
    let cwd = std::env::current_dir()?;
    let repo = match repo {
        Some(repo) => repo,
        None => {
            // The 0102 veto must run before any git read, including default-repo
            // resolution. `run_with` re-checks live and returns the refusal.
            if self_update_enabled() {
                GitRepoProbe
                    .inspect(&cwd)
                    .map(|facts| facts.toplevel)
                    .unwrap_or(cwd)
            } else {
                cwd
            }
        }
    };
    let install = match install {
        Some(install) => install,
        None => std::env::current_exe()?,
    };
    let options = Options {
        repo: std::path::absolute(repo)?,
        install: std::path::absolute(install)?,
        artifact: artifact.map(std::path::absolute).transpose()?,
        check,
    };
    let processes = WindowsProcessSource;
    let probe = GitRepoProbe;
    let builder = CargoBuilder;
    let smoke = ExecSmoke;
    let deps = Deps {
        processes: &processes,
        repo: &probe,
        builder: &builder,
        smoke: &smoke,
        recent_write_cap: TREE_SCAN_MAX,
    };
    run_with(&options, &deps, Utc::now())
}

#[cfg(test)]
mod tests {
    //! Track 0108 test inventory (52 rows). Every fixture is a temp directory;
    //! no test touches `~/.cargo/bin/coordinator.exe`, kills a process or starts
    //! `serve`.

    use std::collections::VecDeque;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime};

    use tempfile::{TempDir, tempdir};

    use super::*;
    use crate::config::{ENV_COORDINATOR_HOME, TestEnvGuard, test_env_lock};
    use crate::selfupdate::swap::{
        self, JOURNAL_VERSION, Journal, OldDisposition, clear_old_with, read_journal,
        remove_journal_if_present, stage_copy_with, swap_with, write_journal,
    };

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    // ---------------------------------------------------------------------------
    // Fixtures
    // ---------------------------------------------------------------------------

    /// Isolated `COORDINATOR_HOME` + repo + install dir; the env lock is held for
    /// the whole test and the guard restores ambient values on drop.
    struct EnvFixture {
        _guard: TestEnvGuard,
        _lock: std::sync::MutexGuard<'static, ()>,
        home: TempDir,
        repo: TempDir,
        install_dir: TempDir,
    }

    fn env_fixture() -> EnvFixture {
        let lock = test_env_lock();
        let guard = TestEnvGuard::capture(&[
            ENV_COORDINATOR_HOME,
            "COORDINATOR_FEATURE_SELF_UPDATE",
            "CARGO_TARGET_DIR",
        ]);
        let home = tempdir().unwrap();
        let repo = tempdir().unwrap();
        let install_dir = tempdir().unwrap();
        // SAFETY: test_env_lock serializes every env-mutating fixture.
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::remove_var("COORDINATOR_FEATURE_SELF_UPDATE");
            std::env::remove_var("CARGO_TARGET_DIR");
        }
        EnvFixture {
            _guard: guard,
            _lock: lock,
            home,
            repo,
            install_dir,
        }
    }

    fn write_bytes(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, bytes).unwrap();
    }

    struct InstallFixture {
        install: PathBuf,
        artifact: PathBuf,
        old: PathBuf,
        previous: Vec<u8>,
        new: Vec<u8>,
    }

    fn install_fixture(env: &EnvFixture) -> InstallFixture {
        let install = env.install_dir.path().join("coordinator.exe");
        let artifact = env.install_dir.path().join("artifact.exe");
        let previous = b"previous-binary-bytes-0108".to_vec();
        let new = b"new-artifact-bytes-0108".to_vec();
        write_bytes(&install, &previous);
        write_bytes(&artifact, &new);
        InstallFixture {
            old: swap::old_path(&install),
            install,
            artifact,
            previous,
            new,
        }
    }

    fn sha(path: &Path) -> String {
        sha256_file(path).unwrap().0
    }

    fn clean_facts(repo: &Path) -> RepoFacts {
        RepoFacts {
            toplevel: repo.to_path_buf(),
            branch: "main".to_string(),
            head_sha: HEAD.to_string(),
            dirty: Vec::new(),
            index_lock: repo.join(".git").join("index.lock"),
            index_lock_exists: false,
            package_name_ok: true,
        }
    }

    #[derive(Clone)]
    struct FakeRepo {
        facts: Arc<Mutex<RepoFacts>>,
        calls: Arc<AtomicUsize>,
    }

    impl FakeRepo {
        fn new(facts: RepoFacts) -> Self {
            Self {
                facts: Arc::new(Mutex::new(facts)),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn set_head(&self, head: &str) {
            self.facts.lock().unwrap().head_sha = head.to_string();
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl RepoProbe for FakeRepo {
        fn inspect(&self, _repo: &Path) -> Result<RepoFacts> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.facts.lock().unwrap().clone())
        }
    }

    struct FakeProcesses {
        rows: Vec<ProcessRow>,
        calls: AtomicUsize,
    }

    impl FakeProcesses {
        fn new(rows: Vec<ProcessRow>) -> Self {
            Self {
                rows,
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl ProcessSource for FakeProcesses {
        fn snapshot(&self) -> Result<Vec<ProcessRow>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.rows.clone())
        }
    }

    struct UnreadableProcesses;

    impl ProcessSource for UnreadableProcesses {
        fn snapshot(&self) -> Result<Vec<ProcessRow>> {
            Err(CoordinatorError::Message("snapshot failed".to_string()))
        }
    }

    struct ScriptedProcesses {
        queue: Mutex<VecDeque<Vec<ProcessRow>>>,
    }

    impl ScriptedProcesses {
        fn new(mut first: Vec<ProcessRow>, second: Vec<ProcessRow>) -> Self {
            let mut queue = VecDeque::new();
            queue.push_back(std::mem::take(&mut first));
            queue.push_back(second);
            Self {
                queue: Mutex::new(queue),
            }
        }
    }

    impl ProcessSource for ScriptedProcesses {
        fn snapshot(&self) -> Result<Vec<ProcessRow>> {
            Ok(self.queue.lock().unwrap().pop_front().unwrap_or_default())
        }
    }

    struct FakeBuilder {
        result: Mutex<BuildResult>,
        calls: AtomicUsize,
        on_build: Option<Box<dyn Fn() + Send + Sync>>,
    }

    impl FakeBuilder {
        fn ok(artifact: PathBuf) -> Self {
            Self {
                result: Mutex::new(BuildResult::Ok {
                    artifact,
                    tail: String::new(),
                }),
                calls: AtomicUsize::new(0),
                on_build: None,
            }
        }

        fn failed(code: Option<i32>, tail: &str) -> Self {
            Self {
                result: Mutex::new(BuildResult::Failed {
                    code,
                    tail: tail.to_string(),
                }),
                calls: AtomicUsize::new(0),
                on_build: None,
            }
        }

        fn timed_out() -> Self {
            Self {
                result: Mutex::new(BuildResult::TimedOut),
                calls: AtomicUsize::new(0),
                on_build: None,
            }
        }

        fn with_hook(mut self, hook: impl Fn() + Send + Sync + 'static) -> Self {
            self.on_build = Some(Box::new(hook));
            self
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Builder for FakeBuilder {
        fn build(&self, _repo: &Path) -> BuildResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(hook) = &self.on_build {
                hook();
            }
            match &*self.result.lock().unwrap() {
                BuildResult::Ok { artifact, tail } => BuildResult::Ok {
                    artifact: artifact.clone(),
                    tail: tail.clone(),
                },
                BuildResult::Failed { code, tail } => BuildResult::Failed {
                    code: *code,
                    tail: tail.clone(),
                },
                BuildResult::TimedOut => BuildResult::TimedOut,
            }
        }
    }

    fn ok_smoke() -> SmokeResult {
        SmokeResult {
            ok: true,
            version: "coordinator 0.1.0".to_string(),
            detail: String::new(),
        }
    }

    fn failed_smoke(detail: &str) -> SmokeResult {
        SmokeResult {
            ok: false,
            version: String::new(),
            detail: detail.to_string(),
        }
    }

    struct FakeSmoke {
        results: Mutex<VecDeque<SmokeResult>>,
        pin: Option<PathBuf>,
        pinned: Mutex<Option<std::fs::File>>,
    }

    impl FakeSmoke {
        fn ok_twice() -> Self {
            Self {
                results: Mutex::new(VecDeque::from([ok_smoke(), ok_smoke()])),
                pin: None,
                pinned: Mutex::new(None),
            }
        }

        fn from(results: Vec<SmokeResult>) -> Self {
            Self {
                results: Mutex::new(results.into()),
                pin: None,
                pinned: Mutex::new(None),
            }
        }

        fn pinning(install: &Path) -> Self {
            Self {
                results: Mutex::new(VecDeque::from([ok_smoke(), ok_smoke()])),
                pin: Some(install.to_path_buf()),
                pinned: Mutex::new(None),
            }
        }
    }

    impl SmokeCheck for FakeSmoke {
        fn check(&self, _exe: &Path) -> SmokeResult {
            if let Some(path) = &self.pin {
                let mut pinned = self.pinned.lock().unwrap();
                if pinned.is_none() {
                    *pinned = open_pinned(path);
                }
            }
            self.results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(ok_smoke)
        }
    }

    #[cfg(windows)]
    fn open_pinned(path: &Path) -> Option<std::fs::File> {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(path)
            .ok()
    }

    #[cfg(not(windows))]
    fn open_pinned(path: &Path) -> Option<std::fs::File> {
        std::fs::File::open(path).ok()
    }

    #[allow(clippy::too_many_arguments)]
    fn run_case_cap(
        env: &EnvFixture,
        install: &Path,
        artifact: Option<PathBuf>,
        check: bool,
        processes: &dyn ProcessSource,
        repo: &FakeRepo,
        builder: &dyn Builder,
        smoke: &dyn SmokeCheck,
        cap: usize,
    ) -> Report {
        let options = Options {
            repo: env.repo.path().to_path_buf(),
            install: install.to_path_buf(),
            artifact,
            check,
        };
        let deps = Deps {
            processes,
            repo,
            builder,
            smoke,
            recent_write_cap: cap,
        };
        run_with(&options, &deps, Utc::now()).unwrap()
    }

    fn run_case(
        env: &EnvFixture,
        install: &Path,
        artifact: Option<PathBuf>,
        processes: &dyn ProcessSource,
        repo: &FakeRepo,
        builder: &dyn Builder,
        smoke: &dyn SmokeCheck,
    ) -> Report {
        run_case_cap(
            env,
            install,
            artifact,
            false,
            processes,
            repo,
            builder,
            smoke,
            TREE_SCAN_MAX,
        )
    }

    fn seed_old(install: &Path, bytes: &[u8]) -> PathBuf {
        let old = swap::old_path(install);
        write_bytes(&old, bytes);
        old
    }

    fn journal_for(install: &Path, artifact: &Path, phase: &str) -> Journal {
        Journal {
            version: JOURNAL_VERSION,
            phase: phase.to_string(),
            install: install.to_path_buf(),
            old: swap::old_path(install),
            artifact: artifact.to_path_buf(),
            head_sha: HEAD.to_string(),
            previous_sha256: Some("p".repeat(64)),
            staged: swap::staged_path(install),
        }
    }

    fn seed_registry(home: &Path, repo: &Path, execution: &Path, status: &str) {
        let state_dir = home.join("project-state");
        std::fs::create_dir_all(&state_dir).unwrap();
        write_bytes(
            &state_dir.join("run-state.json"),
            serde_json::to_vec(&serde_json::json!({
                "project_id": "p-0108",
                "status": status,
                "phase": "implement",
                "updated_at": Utc::now().to_rfc3339(),
                "last_event": "seed",
            }))
            .unwrap()
            .as_slice(),
        );
        write_bytes(
            &home.join("registry.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "projects": [{
                    "id": "p-0108",
                    "path": repo,
                    "execution_repo": execution,
                    "state_dir": state_dir,
                    "created_at": Utc::now().to_rfc3339(),
                }],
            }))
            .unwrap()
            .as_slice(),
        );
    }

    fn snapshot_tree(dir: &Path) -> Vec<(PathBuf, u64)> {
        fn walk(dir: &Path, out: &mut Vec<(PathBuf, u64)>) {
            let Ok(read) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in read.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if let Ok(meta) = std::fs::metadata(&path) {
                    out.push((path, meta.len()));
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, &mut out);
        out.sort();
        out
    }

    fn set_mtime(path: &Path, when: SystemTime) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(when).unwrap();
    }

    // ---------------------------------------------------------------------------
    // Gates (DoD-1 seeds)
    // ---------------------------------------------------------------------------

    #[test]
    fn self_update_refuses_on_cargo_process() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let repo = FakeRepo::new(clean_facts(env.repo.path()));
        let processes = FakeProcesses::new(vec![ProcessRow {
            pid: 4242,
            image: "cargo.exe".to_string(),
        }]);
        let builder = FakeBuilder::ok(fx.artifact.clone());
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &processes,
            &repo,
            &builder,
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "refused");
        assert!(report.reasons.contains(&"build-tool-running"));
        assert_eq!(report.busy.build_tools.len(), 1);
        assert_eq!(builder.calls(), 0);
        assert_eq!(std::fs::read(&fx.install).unwrap(), fx.previous);
        assert!(!fx.old.exists());
        assert!(!swap::journal_path(&fx.install).exists());
        assert!(!env.home.path().join("self-update.json").exists());
    }

    #[test]
    fn self_update_refuses_on_cargo_nextest_process() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let repo = FakeRepo::new(clean_facts(env.repo.path()));
        let processes = FakeProcesses::new(vec![ProcessRow {
            pid: 4243,
            image: "cargo-nextest.exe".to_string(),
        }]);
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &processes,
            &repo,
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "refused");
        assert!(report.reasons.contains(&"build-tool-running"));
    }

    #[test]
    fn self_update_refuses_on_rustc_process() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let repo = FakeRepo::new(clean_facts(env.repo.path()));
        let processes = FakeProcesses::new(vec![ProcessRow {
            pid: 4244,
            image: "rustc.exe".to_string(),
        }]);
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &processes,
            &repo,
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "refused");
        assert!(report.reasons.contains(&"build-tool-running"));
    }

    #[test]
    fn self_update_refuses_on_live_coordinator_peer() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let repo = FakeRepo::new(clean_facts(env.repo.path()));
        let peer = std::process::id().wrapping_add(1);
        let processes = FakeProcesses::new(vec![ProcessRow {
            pid: peer,
            image: "coordinator.exe".to_string(),
        }]);
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &processes,
            &repo,
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "refused");
        assert!(report.reasons.contains(&"live-coordinator"));
        assert_eq!(report.busy.live_coordinator, vec![peer]);
    }

    #[test]
    fn self_update_excludes_own_pid_from_peer_check() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let repo = FakeRepo::new(clean_facts(env.repo.path()));
        let processes = FakeProcesses::new(vec![ProcessRow {
            pid: std::process::id(),
            image: "coordinator.exe".to_string(),
        }]);
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &processes,
            &repo,
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert!(!report.reasons.contains(&"live-coordinator"));
        assert_eq!(report.outcome, "deployed");
    }

    #[test]
    fn self_update_refuses_when_process_table_unreadable() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let repo = FakeRepo::new(clean_facts(env.repo.path()));
        let builder = FakeBuilder::ok(fx.artifact.clone());
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &UnreadableProcesses,
            &repo,
            &builder,
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "refused");
        assert!(report.reasons.contains(&"process-table-unreadable"));
        assert_eq!(builder.calls(), 0);
    }

    #[test]
    fn self_update_refuses_recent_tree_write() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let fresh = env.repo.path().join("src").join("main.rs");
        write_bytes(&fresh, b"fn main() {}\n");
        let repo = FakeRepo::new(clean_facts(env.repo.path()));
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &repo,
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "refused");
        assert!(report.reasons.contains(&"recent-writes"));
        assert!(
            report
                .busy
                .recent_writes
                .iter()
                .any(|write| write.path == fresh)
        );
        assert_eq!(std::fs::read(&fx.install).unwrap(), fx.previous);
    }

    #[test]
    fn self_update_allows_old_tree_writes() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let old = env.repo.path().join("src").join("main.rs");
        write_bytes(&old, b"fn main() {}\n");
        set_mtime(&old, SystemTime::now() - Duration::from_secs(3600));
        let repo = FakeRepo::new(clean_facts(env.repo.path()));
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &repo,
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert!(!report.reasons.contains(&"recent-writes"));
        assert_eq!(report.outcome, "deployed");
    }

    #[test]
    fn self_update_refuses_git_index_lock() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let mut facts = clean_facts(env.repo.path());
        facts.index_lock_exists = true;
        let repo = FakeRepo::new(facts);
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &repo,
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "refused");
        assert!(report.reasons.contains(&"git-index-lock"));
    }

    #[test]
    fn self_update_refuses_dirty_tree() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let mut facts = clean_facts(env.repo.path());
        facts.dirty = vec!["src/lib.rs".to_string()];
        let repo = FakeRepo::new(facts);
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &repo,
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "refused");
        assert!(report.reasons.contains(&"dirty-tree"));
    }

    #[test]
    fn self_update_refuses_not_on_main() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let mut facts = clean_facts(env.repo.path());
        facts.branch = "track/x".to_string();
        let repo = FakeRepo::new(facts);
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &repo,
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "refused");
        assert!(report.reasons.contains(&"not-on-main"));
    }

    #[test]
    fn self_update_refuses_not_product_repo() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let mut facts = clean_facts(env.repo.path());
        facts.package_name_ok = false;
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(facts),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "refused");
        assert!(report.reasons.contains(&"not-product-repo"));

        let other = tempdir().unwrap();
        let mut facts = clean_facts(env.repo.path());
        facts.toplevel = other.path().to_path_buf();
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(facts),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert!(report.reasons.contains(&"not-product-repo"));
    }

    #[test]
    fn self_update_refuses_feature_off() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        // SAFETY: test_env_lock held by env_fixture.
        unsafe { std::env::set_var("COORDINATOR_FEATURE_SELF_UPDATE", "0") };
        let repo = FakeRepo::new(clean_facts(env.repo.path()));
        let processes = FakeProcesses::new(Vec::new());
        let builder = FakeBuilder::ok(fx.artifact.clone());
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &processes,
            &repo,
            &builder,
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "refused");
        assert_eq!(report.reasons, vec!["feature-off"]);
        assert_eq!(processes.calls(), 0);
        assert_eq!(repo.calls(), 0);
        assert_eq!(builder.calls(), 0);
        assert_eq!(std::fs::read(&fx.install).unwrap(), fx.previous);
    }

    #[test]
    fn self_update_refuses_active_run_in_this_repo() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        seed_registry(env.home.path(), env.repo.path(), env.repo.path(), "Running");
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "refused");
        assert!(report.reasons.contains(&"active-run"));
        assert_eq!(report.busy.active_runs, vec!["p-0108".to_string()]);
    }

    #[test]
    fn self_update_ignores_active_run_in_another_repo() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let other = tempdir().unwrap();
        seed_registry(env.home.path(), other.path(), other.path(), "Running");
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert!(!report.reasons.contains(&"active-run"));
        assert_eq!(report.outcome, "deployed");
    }

    #[test]
    fn self_update_refuses_unreadable_registry_or_run_state() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        write_bytes(&env.home.path().join("registry.json"), b"{not json");
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "refused");
        assert!(report.reasons.contains(&"active-run"));
        assert!(
            report.busy.active_runs[0].contains("registry unreadable"),
            "active_runs = {:?}",
            report.busy.active_runs
        );

        std::fs::remove_file(env.home.path().join("registry.json")).unwrap();
        seed_registry(env.home.path(), env.repo.path(), env.repo.path(), "Stopped");
        write_bytes(
            &env.home.path().join("project-state").join("run-state.json"),
            b"{not json",
        );
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "refused");
        assert!(report.reasons.contains(&"active-run"));
        assert!(
            report.busy.active_runs[0].contains("run-state unreadable"),
            "active_runs = {:?}",
            report.busy.active_runs
        );

        // A non-file registry/run-state path is unreadable, not "idle".
        std::fs::remove_file(env.home.path().join("registry.json")).unwrap();
        std::fs::create_dir(env.home.path().join("registry.json")).unwrap();
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert!(report.reasons.contains(&"active-run"));
        assert!(
            report.busy.active_runs[0].contains("not a regular file"),
            "active_runs = {:?}",
            report.busy.active_runs
        );

        std::fs::remove_dir(env.home.path().join("registry.json")).unwrap();
        seed_registry(env.home.path(), env.repo.path(), env.repo.path(), "Stopped");
        let run_state = env.home.path().join("project-state").join("run-state.json");
        std::fs::remove_file(&run_state).unwrap();
        std::fs::create_dir(&run_state).unwrap();
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert!(report.reasons.contains(&"active-run"));
        assert!(
            report.busy.active_runs[0].contains("not a regular file"),
            "active_runs = {:?}",
            report.busy.active_runs
        );
    }

    #[test]
    fn self_update_relative_target_dir_resolves_against_repo() {
        let env = env_fixture();
        // SAFETY: test_env_lock held by env_fixture.
        unsafe { std::env::set_var("CARGO_TARGET_DIR", "relative-target") };
        let expected = env
            .repo
            .path()
            .join("relative-target")
            .join("release")
            .join("coordinator.exe");
        assert_eq!(default_artifact(env.repo.path()), expected);
    }

    #[test]
    fn self_update_refuses_second_concurrent_run() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        write_bytes(
            &env.home.path().join("self-update.lock"),
            b"{\"pid\":1,\"started_at\":\"now\"}\n",
        );
        let builder = FakeBuilder::ok(fx.artifact.clone());
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &builder,
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "refused");
        assert_eq!(report.reasons, vec!["lock-held"]);
        assert_eq!(builder.calls(), 0);
        assert_eq!(std::fs::read(&fx.install).unwrap(), fx.previous);
    }

    #[test]
    fn self_update_breaks_stale_lock_once() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let lock = env.home.path().join("self-update.lock");
        write_bytes(&lock, b"{\"pid\":1,\"started_at\":\"old\"}\n");
        set_mtime(&lock, SystemTime::now() - Duration::from_secs(31 * 60));
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "deployed");
        assert!(!lock.exists());
    }

    #[test]
    fn self_update_tree_scan_cap_refuses() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        for index in 0..4 {
            write_bytes(
                &env.repo.path().join("src").join(format!("f{index}.rs")),
                b"//\n",
            );
        }
        let (_, incomplete) = recent_writes_with(env.repo.path(), Utc::now(), 3).unwrap();
        assert!(incomplete);
        let report = run_case_cap(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            false,
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
            3,
        );
        assert_eq!(report.outcome, "refused");
        assert!(report.reasons.contains(&"tree-scan-incomplete"));
        assert_eq!(TREE_SCAN_MAX, 10_000);
    }

    #[test]
    fn self_update_gate_accepts_case_variant_repo_path() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let case_variant = PathBuf::from(
            env.repo
                .path()
                .to_string_lossy()
                .to_uppercase()
                .replace('\\', "/"),
        );
        let mut facts = clean_facts(env.repo.path());
        facts.toplevel = case_variant;
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(facts),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert!(!report.reasons.contains(&"not-product-repo"));
        assert_eq!(report.outcome, "deployed");
    }

    // ---------------------------------------------------------------------------
    // Swap, rollback and journal (DoD-2/3)
    // ---------------------------------------------------------------------------

    #[test]
    fn self_update_installs_and_keeps_old() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        seed_old(&fx.install, b"older-rollback-bytes");
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "deployed");
        assert_eq!(std::fs::read(&fx.install).unwrap(), fx.new);
        assert_eq!(std::fs::read(&fx.old).unwrap(), fx.previous);
        assert!(!swap::journal_path(&fx.install).exists());
        assert!(env.home.path().join("self-update.json").exists());
        assert_eq!(
            report.artifact.as_ref().unwrap().sha256,
            report.installed.as_ref().unwrap().sha256
        );
    }

    #[test]
    fn self_update_records_sha_equality_not_ledger() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        // Stale ledger trap: head matches today's HEAD, installed sha is wrong.
        write_bytes(
            &env.home.path().join("self-update.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "head_sha": HEAD,
                "installed": {"path": fx.install, "sha256": "0".repeat(64), "size": 1},
            }))
            .unwrap()
            .as_slice(),
        );
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "deployed");
        let installed = sha(&fx.install);
        let artifact = sha(&fx.artifact);
        assert_eq!(installed, artifact);
        let ledger: serde_json::Value = serde_json::from_slice(
            &std::fs::read(env.home.path().join("self-update.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(ledger["installed"]["sha256"], artifact);
        assert_eq!(ledger["artifact"]["sha256"], artifact);
    }

    #[test]
    fn self_update_already_deployed_is_noop() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        write_bytes(&fx.artifact, &fx.previous);
        seed_old(&fx.install, b"older-rollback-bytes");
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "already-deployed");
        assert!(report.ok());
        assert_eq!(std::fs::read(&fx.install).unwrap(), fx.previous);
        assert_eq!(std::fs::read(&fx.old).unwrap(), b"older-rollback-bytes");
        assert!(!env.home.path().join("self-update.json").exists());
    }

    #[test]
    fn self_update_interrupted_journal_beats_already_deployed() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        // Install bytes EQUAL the artifact, plus a live journal at `verifying`.
        write_bytes(&fx.install, &fx.new);
        write_journal(
            &swap::journal_path(&fx.install),
            &journal_for(&fx.install, &fx.artifact, "verifying"),
        )
        .unwrap();
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "refused");
        assert_eq!(report.reasons, vec!["interrupted"]);
        assert!(report.detail.as_deref().unwrap().contains("compare sha256"));
        assert!(swap::journal_path(&fx.install).exists());
        assert_eq!(std::fs::read(&fx.install).unwrap(), fx.new);
        assert!(!env.home.path().join("self-update.json").exists());

        let report = run_case_cap(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            true,
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
            TREE_SCAN_MAX,
        );
        assert_eq!(report.outcome, "check");
        assert_eq!(report.reasons, vec!["interrupted"]);
        assert!(!report.already_deployed);
        assert_eq!(report.would_deploy, Some(false));
        assert!(swap::journal_path(&fx.install).exists());
    }

    #[test]
    fn self_update_pre_swap_smoke_failure_leaves_install() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        seed_old(&fx.install, b"older-rollback-bytes");
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::from(vec![failed_smoke("artifact smoke failed")]),
        );
        assert_eq!(report.outcome, "refused");
        assert_eq!(report.reasons, vec!["smoke-failed"]);
        assert_eq!(std::fs::read(&fx.install).unwrap(), fx.previous);
        assert_eq!(std::fs::read(&fx.old).unwrap(), b"older-rollback-bytes");
        assert!(!swap::journal_path(&fx.install).exists());
        assert_eq!(report.busy.head_sha, HEAD);
    }

    #[test]
    fn self_update_post_swap_smoke_failure_rolls_back() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let ledger_path = env.home.path().join("self-update.json");
        write_bytes(&ledger_path, b"{\"sentinel\":\"keep\"}\n");
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::from(vec![ok_smoke(), failed_smoke("installed smoke failed")]),
        );
        assert_eq!(report.outcome, "rolled-back");
        assert!(!report.ok());
        assert_eq!(std::fs::read(&fx.install).unwrap(), fx.previous);
        assert!(fx.old.exists());
        assert_eq!(std::fs::read(&fx.old).unwrap(), fx.previous);
        assert_eq!(
            std::fs::read(&ledger_path).unwrap(),
            b"{\"sentinel\":\"keep\"}\n"
        );
    }

    #[test]
    fn self_update_rotates_pinned_old() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let old = seed_old(&fx.install, b"pinned-old-bytes");
        let request = SwapRequest {
            install: fx.install.clone(),
            artifact: fx.artifact.clone(),
            head_sha: HEAD.to_string(),
            previous_sha256: Some(sha(&fx.install)),
        };
        let receipt = swap_with(
            &request,
            |from, to| std::fs::copy(from, to),
            |_path| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
        )
        .unwrap();
        let rotated = receipt.rotated_old.clone().expect("rotated old");
        assert!(rotated.exists());
        assert_eq!(std::fs::read(&rotated).unwrap(), b"pinned-old-bytes");
        assert_eq!(std::fs::read(&fx.install).unwrap(), fx.new);
        assert_eq!(std::fs::read(&old).unwrap(), fx.previous);
        swap::finish(&fx.install).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn self_update_refuses_when_old_cannot_be_cleared() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let old = seed_old(&fx.install, b"pinned-old-bytes");
        let pin = open_pinned(&old).expect("pin old");
        let request = SwapRequest {
            install: fx.install.clone(),
            artifact: fx.artifact.clone(),
            head_sha: HEAD.to_string(),
            previous_sha256: Some(sha(&fx.install)),
        };
        let error = swap_with(
            &request,
            |from, to| std::fs::copy(from, to),
            |path| std::fs::remove_file(path),
        )
        .unwrap_err();
        assert!(matches!(error, SwapError::Pinned { .. }));
        assert_eq!(swap_error_reason(&error), "old-pinned");
        assert!(!swap::journal_path(&fx.install).exists());
        assert!(!swap::staged_path(&fx.install).exists());
        assert_eq!(std::fs::read(&fx.install).unwrap(), fx.previous);
        drop(pin);
        assert_eq!(std::fs::read(&old).unwrap(), b"pinned-old-bytes");
        assert!(
            read_journal(&swap::journal_path(&fx.install))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn self_update_staging_mismatch_refuses_and_removes_journal() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let request = SwapRequest {
            install: fx.install.clone(),
            artifact: fx.artifact.clone(),
            head_sha: HEAD.to_string(),
            previous_sha256: Some(sha(&fx.install)),
        };
        let error = swap_with(
            &request,
            |_from, to| {
                std::fs::write(to, b"divergent-staged-bytes")?;
                Ok(23)
            },
            |path| std::fs::remove_file(path),
        )
        .unwrap_err();
        assert!(matches!(error, SwapError::StagingMismatch { .. }));
        assert_eq!(swap_error_reason(&error), "staging-mismatch");
        assert!(!swap::journal_path(&fx.install).exists());
        assert!(!swap::staged_path(&fx.install).exists());
        assert_eq!(std::fs::read(&fx.install).unwrap(), fx.previous);
        assert!(
            read_journal(&swap::journal_path(&fx.install))
                .unwrap()
                .is_none()
        );
    }

    #[cfg(windows)]
    #[test]
    fn self_update_renaming_old_rename_failure_keeps_journal() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let seeded_old = seed_old(&fx.install, b"older-rollback-bytes");
        let smoke = FakeSmoke::pinning(&fx.install);
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &smoke,
        );
        assert_eq!(report.outcome, "refused");
        assert_eq!(report.reasons, vec!["swap-io"]);
        assert!(report.detail.as_deref().unwrap().contains("io error"));
        let journal = read_journal(&swap::journal_path(&fx.install))
            .unwrap()
            .expect("journal kept");
        assert_eq!(journal.phase, "renaming-old");
        assert!(!seeded_old.exists(), "clearing-old removed the seeded .old");
        assert!(!env.home.path().join("self-update.json").exists());
        drop(smoke);
        assert_eq!(std::fs::read(&fx.install).unwrap(), fx.previous);

        let second = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(second.outcome, "refused");
        assert_eq!(second.reasons, vec!["interrupted"]);
        assert!(second.detail.as_deref().unwrap().contains("delete"));
    }

    #[test]
    fn self_update_interrupted_journal_refuses() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        write_journal(
            &swap::journal_path(&fx.install),
            &journal_for(&fx.install, &fx.artifact, "renaming-old"),
        )
        .unwrap();
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "refused");
        assert_eq!(report.reasons, vec!["interrupted"]);
        assert_eq!(std::fs::read(&fx.install).unwrap(), fx.previous);
        assert!(swap::journal_path(&fx.install).exists());
    }

    #[test]
    fn self_update_interrupted_pre_rename_recovery_says_nothing_swapped() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        write_journal(
            &swap::journal_path(&fx.install),
            &journal_for(&fx.install, &fx.artifact, "clearing-old"),
        )
        .unwrap();
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        let detail = report.detail.as_deref().unwrap();
        assert!(detail.contains("nothing was swapped"));
        assert!(detail.contains("delete"));
        assert!(!detail.contains("copy"));
    }

    #[test]
    fn self_update_interrupted_post_rename_recovery_names_copy_old() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        write_journal(
            &swap::journal_path(&fx.install),
            &journal_for(&fx.install, &fx.artifact, "installing"),
        )
        .unwrap();
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        let detail = report.detail.as_deref().unwrap();
        assert!(detail.contains("compare sha256"));
        assert!(detail.contains("copy"));
        assert!(detail.contains("do not downgrade"));
        assert!(detail.contains("stop and ask the owner"));
    }

    #[test]
    fn self_update_interrupted_renaming_old_recovery_says_delete_and_rerun() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let mut journal = journal_for(&fx.install, &fx.artifact, "renaming-old");
        write_journal(&swap::journal_path(&fx.install), &journal).unwrap();
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        let detail = report.detail.as_deref().unwrap();
        assert!(detail.contains("delete"));
        assert!(detail.contains("rerun"));
        assert!(detail.contains("first install"));
        assert!(!detail.contains(&format!(
            "copy {} to {}",
            fx.old.display(),
            fx.install.display()
        )));

        // Install absent: the rename-aside completed; the guidance is the same.
        std::fs::remove_file(&fx.install).unwrap();
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        let detail = report.detail.as_deref().unwrap();
        assert!(detail.contains("delete"));
        assert!(detail.contains("rerun"));
        assert!(detail.contains("first install"));

        journal.previous_sha256 = None;
        let detail = recovery_message(&journal);
        assert!(detail.contains("first install"));
        assert!(detail.contains("creates no"));
    }

    #[test]
    fn self_update_interrupted_rolling_back_restores_previous() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        write_bytes(&fx.install, &fx.new);
        write_journal(
            &swap::journal_path(&fx.install),
            &journal_for(&fx.install, &fx.artifact, "rolling-back"),
        )
        .unwrap();
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        let detail = report.detail.as_deref().unwrap();
        assert!(detail.contains("unconditionally"));
        assert!(detail.contains("previous"));
        assert!(!detail.contains("swap completed"));
        assert!(!detail.contains("as success"));

        let mut first = journal_for(&fx.install, &fx.artifact, "rolling-back");
        first.previous_sha256 = None;
        let detail = recovery_message(&first);
        assert!(detail.contains("remove the rejected"));
        assert!(detail.contains("no previous binary"));
    }

    #[test]
    fn self_update_sha_divergence_returns_receipt_and_caller_rolls_back() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        seed_old(&fx.install, b"older-rollback-bytes");
        let artifact_for_hook = fx.artifact.clone();
        let phase_slot = Arc::new(Mutex::new(None::<String>));
        let phase_for_hook = phase_slot.clone();
        let install_for_hook = fx.install.clone();
        let _hook = TestSwapGuard::install(Box::new(move |req: &SwapRequest| {
            let artifact = artifact_for_hook.clone();
            let phase = phase_for_hook.clone();
            let install = install_for_hook.clone();
            let receipt = swap_with(
                req,
                |from, to| std::fs::copy(from, to),
                move |path| {
                    // Mutate the artifact after staging, then clear `.old`.
                    std::fs::write(&artifact, b"artifact-mutated-after-staging")?;
                    std::fs::remove_file(path)
                },
            )?;
            // `swap_with` never rolls back internally: the journal must still exist.
            *phase.lock().unwrap() = read_journal(&swap::journal_path(&install))
                .ok()
                .flatten()
                .map(|journal| journal.phase);
            Ok(receipt)
        }));
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(
            phase_slot.lock().unwrap().as_deref(),
            Some("verifying"),
            "swap_with must leave the journal for the caller"
        );
        assert_eq!(report.outcome, "rolled-back");
        assert!(report.detail.as_deref().unwrap().contains("sha mismatch"));
        assert_eq!(std::fs::read(&fx.install).unwrap(), fx.previous);
        assert_eq!(std::fs::read(&fx.old).unwrap(), fx.previous);
        assert!(!swap::journal_path(&fx.install).exists());
        assert!(!env.home.path().join("self-update.json").exists());
    }

    #[test]
    fn self_update_swap_error_arms_map_to_reasons() {
        let io = SwapError::Io {
            path: PathBuf::from("x"),
            source: std::io::Error::other("x"),
        };
        let mismatch = SwapError::RollbackMismatch {
            path: PathBuf::from("x"),
            expected: "a".to_string(),
            actual: "b".to_string(),
        };
        let pinned = SwapError::Pinned {
            path: PathBuf::from("x"),
        };
        let interrupted = SwapError::Interrupted {
            phase: "installing".to_string(),
        };
        let staging = SwapError::StagingMismatch {
            path: PathBuf::from("x"),
        };
        assert_eq!(swap_error_reason(&io), "swap-io");
        assert_eq!(swap_error_reason(&mismatch), "rollback-mismatch");
        assert_eq!(swap_error_reason(&pinned), "old-pinned");
        assert_eq!(swap_error_reason(&interrupted), "interrupted");
        assert_eq!(swap_error_reason(&staging), "staging-mismatch");
    }

    #[test]
    fn self_update_first_install_without_old() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        std::fs::remove_file(&fx.install).unwrap();
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "deployed");
        assert_eq!(std::fs::read(&fx.install).unwrap(), fx.new);
        assert!(!fx.old.exists());
        assert!(report.old.is_none());
        let summary = report.summary();
        assert!(summary.contains("first install (no previous binary)"));
        assert!(!summary.contains("coordinator.exe.old"));
        let ledger: serde_json::Value = serde_json::from_slice(
            &std::fs::read(env.home.path().join("self-update.json")).unwrap(),
        )
        .unwrap();
        assert!(ledger["previous"].is_null());
    }

    #[test]
    fn self_update_rollback_preserves_old_and_previous_bytes() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let previous_sha = sha(&fx.install);
        let request = SwapRequest {
            install: fx.install.clone(),
            artifact: fx.artifact.clone(),
            head_sha: HEAD.to_string(),
            previous_sha256: Some(sha(&fx.install)),
        };
        swap(&request).unwrap();
        assert_eq!(std::fs::read(&fx.install).unwrap(), fx.new);

        let error = rollback(&request, Some(&"d".repeat(64))).unwrap_err();
        assert!(matches!(error, SwapError::RollbackMismatch { .. }));
        assert!(fx.old.exists());
        assert!(swap::journal_path(&fx.install).exists());

        rollback(&request, Some(&previous_sha)).unwrap();
        assert_eq!(std::fs::read(&fx.install).unwrap(), fx.previous);
        assert_eq!(std::fs::read(&fx.old).unwrap(), fx.previous);
        assert!(!swap::journal_path(&fx.install).exists());
    }

    #[test]
    fn self_update_ledger_only_on_success() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let ledger_path = env.home.path().join("self-update.json");
        write_bytes(&ledger_path, b"{\"sentinel\":\"keep\"}\n");
        let report = run_case(
            &env,
            &fx.install,
            None,
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::failed(Some(101), "compile exploded"),
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "refused");
        assert_eq!(report.reasons, vec!["build-failed"]);
        assert!(
            report
                .detail
                .as_deref()
                .unwrap()
                .contains("compile exploded")
        );
        assert_eq!(
            std::fs::read(&ledger_path).unwrap(),
            b"{\"sentinel\":\"keep\"}\n"
        );

        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "deployed");
        let ledger: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&ledger_path).unwrap()).unwrap();
        assert_eq!(ledger["version"], 1);
        assert_eq!(ledger["head_sha"], HEAD);
        assert_eq!(ledger["artifact"]["sha256"], sha(&fx.artifact));
        assert_eq!(ledger["installed"]["sha256"], sha(&fx.install));
        assert_eq!(ledger["decided_by"]["pid"], std::process::id());
    }

    #[test]
    fn self_update_build_failure_leaves_install_untouched() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let builder = FakeBuilder::failed(Some(1), "error[E0001]: boom");
        let report = run_case(
            &env,
            &fx.install,
            None,
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &builder,
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "refused");
        assert_eq!(report.reasons, vec!["build-failed"]);
        assert!(report.detail.as_deref().unwrap().contains("E0001"));
        assert_eq!(builder.calls(), 1);
        assert_eq!(std::fs::read(&fx.install).unwrap(), fx.previous);
        assert!(!fx.old.exists());
    }

    #[test]
    fn self_update_build_timeout_is_reported() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let report = run_case(
            &env,
            &fx.install,
            None,
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::timed_out(),
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "refused");
        assert_eq!(report.reasons, vec!["build-timeout"]);
        assert_eq!(std::fs::read(&fx.install).unwrap(), fx.previous);
    }

    #[test]
    fn self_update_refuses_when_head_moves_during_build() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let repo = FakeRepo::new(clean_facts(env.repo.path()));
        let hook_repo = repo.clone();
        let builder = FakeBuilder::ok(fx.artifact.clone()).with_hook(move || {
            hook_repo.set_head(&"b".repeat(40));
        });
        let report = run_case(
            &env,
            &fx.install,
            None,
            &FakeProcesses::new(Vec::new()),
            &repo,
            &builder,
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "refused");
        assert_eq!(report.reasons.first(), Some(&"changed-during-build"));
        assert_eq!(std::fs::read(&fx.install).unwrap(), fx.previous);
        assert!(!swap::journal_path(&fx.install).exists());
    }

    #[test]
    fn self_update_second_gate_reports_changed_during_build_with_all_reasons() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let repo = FakeRepo::new(clean_facts(env.repo.path()));
        let hook_repo = repo.clone();
        let builder = FakeBuilder::ok(fx.artifact.clone()).with_hook(move || {
            hook_repo.set_head(&"b".repeat(40));
        });
        let peer = std::process::id().wrapping_add(9);
        let processes = ScriptedProcesses::new(
            Vec::new(),
            vec![
                ProcessRow {
                    pid: 5001,
                    image: "cargo.exe".to_string(),
                },
                ProcessRow {
                    pid: peer,
                    image: "coordinator.exe".to_string(),
                },
            ],
        );
        let report = run_case(
            &env,
            &fx.install,
            None,
            &processes,
            &repo,
            &builder,
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.reasons.first(), Some(&"changed-during-build"));
        assert!(report.reasons.contains(&"build-tool-running"));
        assert!(report.reasons.contains(&"live-coordinator"));
        assert_eq!(std::fs::read(&fx.install).unwrap(), fx.previous);

        // Head unchanged: the busy-only refusal names the actor, not a moved HEAD.
        let repo = FakeRepo::new(clean_facts(env.repo.path()));
        let processes = ScriptedProcesses::new(
            Vec::new(),
            vec![ProcessRow {
                pid: 5002,
                image: "cargo.exe".to_string(),
            }],
        );
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &processes,
            &repo,
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.reasons, vec!["build-tool-running"]);
        assert!(!report.reasons.contains(&"changed-during-build"));
    }

    #[test]
    fn self_update_artifact_missing_refuses() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let report = run_case(
            &env,
            &fx.install,
            Some(env.install_dir.path().join("absent.exe")),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        assert_eq!(report.outcome, "refused");
        assert_eq!(report.reasons, vec!["artifact-missing"]);
        assert_eq!(std::fs::read(&fx.install).unwrap(), fx.previous);
    }

    #[test]
    fn self_update_records_deciding_process() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let report = run_case(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &FakeBuilder::ok(fx.artifact.clone()),
            &FakeSmoke::ok_twice(),
        );
        let decided = report.decided_by.as_ref().unwrap();
        assert_eq!(decided.pid, std::process::id());
        assert_eq!(decided.exe, std::env::current_exe().unwrap());
        assert_eq!(decided.sha256.len(), 64);
        assert!(
            decided
                .sha256
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(decided.version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn self_update_check_writes_nothing() {
        let env = env_fixture();
        let fx = install_fixture(&env);
        let before_install = snapshot_tree(env.install_dir.path());
        let before_home = snapshot_tree(env.home.path());
        let builder = FakeBuilder::ok(fx.artifact.clone());
        let report = run_case_cap(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            true,
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &builder,
            &FakeSmoke::ok_twice(),
            TREE_SCAN_MAX,
        );
        assert_eq!(report.outcome, "check");
        assert_eq!(report.would_deploy, Some(true));
        assert_eq!(builder.calls(), 0);
        assert!(!env.home.path().join("self-update.lock").exists());
        assert_eq!(snapshot_tree(env.install_dir.path()), before_install);
        assert_eq!(snapshot_tree(env.home.path()), before_home);

        let lock = env.home.path().join("self-update.lock");
        write_bytes(&lock, b"{\"pid\":1,\"started_at\":\"now\"}\n");
        let before_home = snapshot_tree(env.home.path());
        let report = run_case_cap(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            true,
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &builder,
            &FakeSmoke::ok_twice(),
            TREE_SCAN_MAX,
        );
        assert!(report.reasons.contains(&"lock-held"));
        assert_eq!(snapshot_tree(env.home.path()), before_home);

        // A stale lock is still reported by `--check` (the mutating run breaks it).
        set_mtime(&lock, SystemTime::now() - Duration::from_secs(31 * 60));
        let report = run_case_cap(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            true,
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &builder,
            &FakeSmoke::ok_twice(),
            TREE_SCAN_MAX,
        );
        assert!(report.reasons.contains(&"lock-held"));

        // A `--check` with no machine home at all must not create one.
        std::fs::remove_dir_all(env.home.path()).unwrap();
        let report = run_case_cap(
            &env,
            &fx.install,
            Some(fx.artifact.clone()),
            true,
            &FakeProcesses::new(Vec::new()),
            &FakeRepo::new(clean_facts(env.repo.path())),
            &builder,
            &FakeSmoke::ok_twice(),
            TREE_SCAN_MAX,
        );
        assert_eq!(report.outcome, "check");
        assert!(
            !env.home.path().exists(),
            "--check must not create COORDINATOR_HOME"
        );
    }

    #[test]
    fn self_update_git_probe_reads_branch_head_and_dirt() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        let git = |args: &[&str]| {
            let output = crate::git_command::command()
                .args(args)
                .current_dir(repo)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.email", "0108@example.invalid"]);
        git(&["config", "user.name", "0108 fixture"]);
        write_bytes(
            &repo.join("Cargo.toml"),
            b"[package]\nname = \"coordinator\"\nversion = \"0.1.0\"\n",
        );
        write_bytes(&repo.join("src").join("lib.rs"), b"pub fn x() {}\n");
        git(&["add", "."]);
        git(&["commit", "-m", "seed"]);

        let facts = GitRepoProbe.inspect(repo).unwrap();
        assert_eq!(facts.branch, "main");
        assert_eq!(facts.head_sha.len(), 40);
        assert!(facts.package_name_ok);
        assert!(facts.dirty.is_empty());

        write_bytes(&repo.join("src").join("extra.rs"), b"pub fn y() {}\n");
        let facts = GitRepoProbe.inspect(repo).unwrap();
        assert!(
            facts.dirty.iter().any(|path| path == "src/extra.rs"),
            "dirty = {:?}",
            facts.dirty
        );
    }

    #[test]
    fn self_update_recent_writes_reports_fresh_and_skips_old() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        let fresh = repo.join("src").join("fresh.rs");
        let old = repo.join("src").join("old.rs");
        write_bytes(&fresh, b"//\n");
        write_bytes(&old, b"//\n");
        set_mtime(&old, SystemTime::now() - Duration::from_secs(3600));
        let (recent, incomplete) = recent_writes(repo, Utc::now()).unwrap();
        assert!(!incomplete);
        assert!(recent.iter().any(|write| write.path == fresh));
        assert!(!recent.iter().any(|write| write.path == old));
    }

    #[test]
    fn self_update_clear_old_seam_reports_cleared_and_rotated() {
        let dir = tempdir().unwrap();
        let old = dir.path().join("coordinator.exe.old");
        assert!(matches!(
            clear_old_with(&old, |path| std::fs::remove_file(path)).unwrap(),
            OldDisposition::Cleared
        ));
        write_bytes(&old, b"old");
        let rotated = clear_old_with(&old, |_path| {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        })
        .unwrap();
        assert!(matches!(rotated, OldDisposition::Rotated(_)));
        assert!(!old.exists());
    }

    #[test]
    fn self_update_stage_copy_seam_syncs_and_reports_copy_errors() {
        let dir = tempdir().unwrap();
        let artifact = dir.path().join("artifact.exe");
        let staged = dir.path().join("coordinator.exe.new");
        write_bytes(&artifact, b"artifact");
        stage_copy_with(&artifact, &staged, |from, to| std::fs::copy(from, to)).unwrap();
        assert_eq!(std::fs::read(&staged).unwrap(), b"artifact");
        let error = stage_copy_with(&missing_path(dir.path()), &staged, |_from, _to| {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        })
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    fn missing_path(dir: &Path) -> PathBuf {
        dir.join("missing.exe")
    }

    #[test]
    fn self_update_journal_helpers_are_idempotent() {
        let dir = tempdir().unwrap();
        let install = dir.path().join("coordinator.exe");
        let artifact = dir.path().join("artifact.exe");
        let journal = journal_for(&install, &artifact, "verifying");
        assert!(
            read_journal(&swap::journal_path(&install))
                .unwrap()
                .is_none()
        );
        write_journal(&swap::journal_path(&install), &journal).unwrap();
        assert_eq!(
            read_journal(&swap::journal_path(&install))
                .unwrap()
                .unwrap()
                .phase,
            "verifying"
        );
        remove_journal_if_present(&swap::journal_path(&install));
        remove_journal_if_present(&swap::journal_path(&install));
        assert!(!swap::journal_path(&install).exists());
    }
}
