//! Per-`run_epoch` detached git worktree (track 0066).
//!
//! The session pool stays keyed by `project_id`. When isolation is on, one epoch
//! directory under the project state dir is the product cwd. Flag off is a no-op.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use crate::error::{CoordinatorError, Result};
use crate::registry::{ProjectRecord, paths_equal};
use crate::state::{RunStatus, StatusView, load_run_state, resolve_state_dir};

pub const ENV_WORKTREE: &str = "COORDINATOR_WORKTREE";
pub const ENV_WORKTREE_REAP_SECS: &str = "COORDINATOR_WORKTREE_REAP_SECS";

const DEFAULT_REAP_SECS: u64 = 86_400;
const REMOVE_TRIES: u32 = 3;

/// `off` forces off. `1` / `true` / `on` forces on. Unset uses the record.
/// Any other set value, including empty, fails closed.
pub fn isolation_enabled(record: &ProjectRecord) -> Result<bool> {
    match std::env::var(ENV_WORKTREE) {
        Err(_) => Ok(record.worktree_isolation),
        Ok(value) if value.eq_ignore_ascii_case("off") => Ok(false),
        Ok(value) if env_on(&value) => Ok(true),
        Ok(value) => Err(CoordinatorError::Message(format!(
            "COORDINATOR_WORKTREE={value:?} is invalid; expected unset, off, 1, true, or on"
        ))),
    }
}

fn env_on(value: &str) -> bool {
    value.eq_ignore_ascii_case("1")
        || value.eq_ignore_ascii_case("true")
        || value.eq_ignore_ascii_case("on")
}

pub fn worktrees_root(record: &ProjectRecord) -> Result<PathBuf> {
    Ok(resolve_state_dir(record)?.join("worktrees"))
}

pub fn epoch_dir(record: &ProjectRecord, epoch: u64) -> Result<PathBuf> {
    Ok(worktrees_root(record)?.join(epoch.to_string()))
}

/// Flag on and the epoch directory is on disk.
pub fn active_epoch_dir(record: &ProjectRecord) -> Option<PathBuf> {
    if !isolation_enabled(record).ok()? {
        return None;
    }
    let state = load_run_state(record).ok()?;
    let dir = epoch_dir(record, state.run_epoch).ok()?;
    if dir.is_dir() { Some(dir) } else { None }
}

/// Active worktree when that directory exists, otherwise the layout execution repo.
pub fn product_git_cwd(record: &ProjectRecord) -> Option<PathBuf> {
    if let Some(dir) = active_epoch_dir(record) {
        return Some(dir);
    }
    crate::layout::resolve(record).execution_repo
}

/// Absolute `{state_dir}/cargo-target` when isolation is on and `cwd` is inside
/// `{state_dir}/worktrees`. Otherwise `None` (no `CARGO_TARGET_DIR` injection).
pub fn cargo_target_dir_for(record: &ProjectRecord, cwd: &Path) -> Option<PathBuf> {
    if !isolation_enabled(record).unwrap_or(false) {
        return None;
    }
    let root = worktrees_root(record).ok()?;
    if !path_is_inside(cwd, &root) {
        return None;
    }
    let dir = resolve_state_dir(record).ok()?.join("cargo-target");
    Some(absolutize(dir))
}

/// Create the detached worktree for `next_epoch` before run state becomes Running.
///
/// Flag off returns immediately. A git failure leaves run state unsaved.
pub fn prepare_epoch(record: &ProjectRecord, next_epoch: u64) -> Result<()> {
    if !isolation_enabled(record)? {
        return Ok(());
    }
    let exec = execution_repo(record)?;
    let root = worktrees_root(record)?;
    let target = root.join(next_epoch.to_string());
    shutdown_under(record, &root, Some(&target));
    crate::workflow::watchdog::clear_progress(record);
    std::fs::create_dir_all(&root)?;
    let children = numeric_children(&root);
    if let Some(live) = children.iter().find(|child| {
        is_live(record, child)
            && !paths_same(child, &target)
            && !replaced_by_next_epoch(record, child, &target)
    }) {
        return Err(CoordinatorError::Message(format!(
            "worktree: epoch still live at {}",
            live.display()
        )));
    }
    let target_live = children
        .iter()
        .any(|child| paths_same(child, &target) && is_live(record, child));
    for child in &children {
        if is_live(record, child) && !replaced_by_next_epoch(record, child, &target) {
            continue;
        }
        remove_worktree(record, &exec, child)?;
    }
    if target_live || (target.is_dir() && registered_worktree(&exec, &target)?) {
        return Ok(());
    }
    if target.exists() {
        return Err(CoordinatorError::Message(format!(
            "worktree: {} still present after reap",
            target.display()
        )));
    }
    git(
        &exec,
        &[
            "worktree",
            "add",
            "--detach",
            &target.display().to_string(),
            "HEAD",
        ],
    )?;
    Ok(())
}

/// After a commit that landed Idle, shut down a session on this epoch and remove it.
/// A failed remove is journaled and does not roll the Idle commit back.
pub fn release_if_idle(record: &ProjectRecord, view: &StatusView) {
    if view.status != RunStatus::Idle {
        return;
    }
    if !isolation_enabled(record).unwrap_or(false) {
        return;
    }
    let Ok(exec) = execution_repo(record) else {
        return;
    };
    let Ok(dir) = epoch_dir(record, view.run_epoch) else {
        return;
    };
    shutdown_if_cwd(record, &dir);
    let _ = remove_worktree(record, &exec, &dir);
}

/// Age sweep for Idle and Stopped projects. Does not poll and does not kill sessions.
pub fn reap_aged(record: &ProjectRecord) -> Result<()> {
    if !isolation_enabled(record)? {
        return Ok(());
    }
    let exec = match execution_repo(record) {
        Ok(path) => path,
        Err(_) => return Ok(()),
    };
    let root = worktrees_root(record)?;
    let max_age = Duration::from_secs(reap_secs());
    for child in numeric_children(&root) {
        if is_live(record, &child) || !older_than(&child, max_age) {
            continue;
        }
        let _ = remove_worktree(record, &exec, &child);
    }
    Ok(())
}

/// Slash- and case-insensitive path compare (git porcelain vs Windows paths).
pub(crate) fn paths_same(a: &Path, b: &Path) -> bool {
    if slash_eq(a, b) || paths_equal(a, b) {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => slash_eq(&ca, &cb),
        _ => false,
    }
}

fn execution_repo(record: &ProjectRecord) -> Result<PathBuf> {
    crate::layout::resolve(record)
        .execution_repo
        .ok_or_else(|| CoordinatorError::Message("worktree: no execution repo".into()))
}

fn reap_secs() -> u64 {
    match std::env::var(ENV_WORKTREE_REAP_SECS) {
        Ok(value) => value.trim().parse().unwrap_or(DEFAULT_REAP_SECS),
        Err(_) => DEFAULT_REAP_SECS,
    }
}

fn absolutize(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        return path;
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(&path))
        .unwrap_or(path)
}

fn slash_key(path: &Path) -> String {
    let folded = path.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    {
        folded.to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        folded
    }
}

fn slash_eq(a: &Path, b: &Path) -> bool {
    slash_key(a) == slash_key(b)
}

fn path_is_inside(path: &Path, root: &Path) -> bool {
    let path = slash_key(path);
    let root = slash_key(root);
    let root = root.trim_end_matches('/');
    path.starts_with(&format!("{root}/"))
}

fn numeric_children(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for ent in entries.flatten() {
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_digit()) {
            out.push(ent.path());
        }
    }
    out.sort();
    out
}

fn older_than(path: &Path, max_age: Duration) -> bool {
    if max_age.is_zero() {
        return true;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age >= max_age)
}

fn is_live(record: &ProjectRecord, path: &Path) -> bool {
    if let Ok(state) = load_run_state(record)
        && matches!(state.status, RunStatus::Running | RunStatus::Paused)
        && epoch_dir(record, state.run_epoch).is_ok_and(|dir| paths_same(&dir, path))
    {
        return true;
    }
    session_bound(record, path)
}

/// The on-disk run still names this directory, and `target` is the epoch that
/// replaces it. A session still bound to the directory is not replaced.
fn replaced_by_next_epoch(record: &ProjectRecord, path: &Path, target: &Path) -> bool {
    if paths_same(path, target) || session_bound(record, path) {
        return false;
    }
    let Ok(state) = load_run_state(record) else {
        return false;
    };
    if !matches!(state.status, RunStatus::Running | RunStatus::Paused) {
        return false;
    }
    epoch_dir(record, state.run_epoch).is_ok_and(|dir| paths_same(&dir, path))
}

fn session_bound(record: &ProjectRecord, path: &Path) -> bool {
    let snap = crate::harness::pool::persist_live(record);
    if let Some(ref live) = snap
        && live.cwd.as_ref().is_some_and(|cwd| paths_same(cwd, path))
        && (live.alive || live.prompt_in_flight)
    {
        return true;
    }
    if crate::workflow::watchdog::sidecar_tool_in_flight(record)
        && snap
            .as_ref()
            .and_then(|live| live.cwd.as_ref())
            .is_some_and(|cwd| paths_same(cwd, path))
    {
        return true;
    }
    if let Some(bundle) = crate::harness::status_bundle_sync(record)
        && let Some(grok) = bundle.grok
        && grok.alive
        && grok.cwd.as_ref().is_some_and(|cwd| paths_same(cwd, path))
    {
        return true;
    }
    false
}

fn shutdown_under(record: &ProjectRecord, root: &Path, keep: Option<&Path>) {
    let Some(live) = crate::harness::pool::persist_live(record) else {
        detach_if(record, |cwd| {
            path_is_inside(cwd, root) && keep.is_none_or(|keep| !paths_same(cwd, keep))
        });
        return;
    };
    let Some(cwd) = live.cwd.as_ref() else {
        return;
    };
    if !path_is_inside(cwd, root) {
        return;
    }
    if keep.is_some_and(|keep| paths_same(cwd, keep)) {
        return;
    }
    crate::harness::pool::kill_live_pids(&live);
    crate::harness::pool::clear_persist_file(record);
    crate::harness::abort::unregister_cancel_handle(&record.id);
    detach_if(record, |cwd| path_is_inside(cwd, root));
}

fn shutdown_if_cwd(record: &ProjectRecord, epoch_path: &Path) {
    if let Some(live) = crate::harness::pool::persist_live(record)
        && live
            .cwd
            .as_ref()
            .is_some_and(|cwd| paths_same(cwd, epoch_path) || path_is_inside(cwd, epoch_path))
    {
        crate::harness::pool::kill_live_pids(&live);
        crate::harness::pool::clear_persist_file(record);
        crate::harness::abort::unregister_cancel_handle(&record.id);
    }
    detach_if(record, |cwd| {
        paths_same(cwd, epoch_path) || path_is_inside(cwd, epoch_path)
    });
}

fn detach_if(record: &ProjectRecord, pred: impl Fn(&Path) -> bool) {
    crate::harness::pool::detach_pooled_if_cwd(&record.id, pred);
}

fn remove_worktree(record: &ProjectRecord, exec: &Path, dir: &Path) -> Result<()> {
    if !dir.exists() {
        git(exec, &["worktree", "prune"])?;
        return Ok(());
    }
    let path = dir.display().to_string();
    let mut last = String::from("worktree remove failed");
    for attempt in 0..REMOVE_TRIES {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(1));
        }
        if !dir.exists() {
            git(exec, &["worktree", "prune"])?;
            return Ok(());
        }
        match git(exec, &["worktree", "remove", "--force", &path]) {
            Ok(()) => {
                if !dir.exists() {
                    return Ok(());
                }
                last = format!("worktree: {path} still present after remove");
            }
            Err(e) => {
                last = e.to_string();
                if !dir.exists() {
                    git(exec, &["worktree", "prune"])?;
                    return Ok(());
                }
            }
        }
    }
    crate::progress_log::append(record, "worktree", &format!("remove {path} failed: {last}"));
    Err(CoordinatorError::Message(last))
}

fn registered_worktree(exec: &Path, dir: &Path) -> Result<bool> {
    let text = git_stdout(exec, &["worktree", "list", "--porcelain"])?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("worktree ")
            && paths_same(Path::new(rest.trim()), dir)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn git(cwd: &Path, args: &[&str]) -> Result<()> {
    let _ = git_stdout(cwd, args)?;
    Ok(())
}

fn git_stdout(cwd: &Path, args: &[&str]) -> Result<String> {
    let shown = args.join(" ");
    let out = Command::new(Path::new("git"))
        .args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .map_err(|e| CoordinatorError::Message(format!("worktree: git {shown}: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(CoordinatorError::Message(format!(
            "worktree: git {shown}: {}",
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::auto_detect_nested_execution;
    use crate::outcome::{OutcomeSource, PhaseOutcome, outcome_current_path, write_and_apply};
    use crate::registry::ProjectRecord;
    use crate::run::{self, run, run_with_driver};
    use crate::state::{ensure_state_dir, run_state_path, save_run_state};
    use crate::workflow::WorkflowDriver;
    use crate::workflow::drive::tick;
    use crate::workflow::graph::{
        PHASE_ADDRESS_FINDINGS, PHASE_ADVANCE, PHASE_IMPLEMENT, PHASE_PLAN,
    };
    use chrono::Utc;
    use std::process::Command;
    use tempfile::TempDir;
    use uuid::Uuid;

    struct EnvSet {
        key: &'static str,
    }

    impl EnvSet {
        fn set(key: &'static str, value: &str) -> Self {
            unsafe { std::env::set_var(key, value) };
            Self { key }
        }
    }

    impl Drop for EnvSet {
        fn drop(&mut self) {
            unsafe { std::env::remove_var(self.key) };
        }
    }

    struct RestoreEnv {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }

    impl RestoreEnv {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, prev }
        }
    }

    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            unsafe {
                match self.prev.take() {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    fn record(
        path: PathBuf,
        exec: Option<PathBuf>,
        state: Option<PathBuf>,
        on: bool,
    ) -> ProjectRecord {
        ProjectRecord {
            id: Uuid::new_v4().to_string(),
            path,
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: exec,
            execution_repos: std::collections::BTreeMap::new(),
            state_dir: state,
            auto_merge: true,
            phase_timeouts_secs: std::collections::BTreeMap::new(),
            notify_progress: false,
            worktree_isolation: on,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            created_at: Utc::now(),
        }
    }

    fn git_ok(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn init_repo() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        git_ok(dir.path(), &["init"]);
        git_ok(
            dir.path(),
            &["config", "user.email", "worktree-test@example.com"],
        );
        git_ok(dir.path(), &["config", "user.name", "worktree-test"]);
        std::fs::write(dir.path().join("README.md"), b"seed\n").unwrap();
        git_ok(dir.path(), &["add", "README.md"]);
        git_ok(dir.path(), &["commit", "-m", "seed"]);
        git_ok(dir.path(), &["branch", "track/0099-keep"]);
        dir
    }

    fn porcelain(cwd: &Path) -> String {
        let out = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn branch_exists(cwd: &Path, name: &str) -> bool {
        let out = Command::new("git")
            .args(["branch", "--list", name])
            .current_dir(cwd)
            .output()
            .unwrap();
        out.status.success() && !String::from_utf8_lossy(&out.stdout).trim().is_empty()
    }

    #[test]
    fn flag_off_skips_git_and_cargo_target() {
        let ws = tempfile::tempdir().unwrap();
        let rec = record(ws.path().to_path_buf(), None, None, false);
        assert!(!isolation_enabled(&rec).unwrap());
        prepare_epoch(&rec, 1).unwrap();
        assert!(!worktrees_root(&rec).unwrap().exists());
        let cwd = ws.path().join("worktrees").join("1");
        assert!(cargo_target_dir_for(&rec, &cwd).is_none());
    }

    #[test]
    fn bad_env_fails_closed_and_off_on_override_the_record() {
        let _guard = crate::config::test_env_lock();
        let ws = tempfile::tempdir().unwrap();
        let off_rec = record(ws.path().to_path_buf(), None, None, false);
        let on_rec = record(ws.path().to_path_buf(), None, None, true);
        for bad in ["", "0", "false", "no", "yes"] {
            let _set = EnvSet::set(ENV_WORKTREE, bad);
            assert!(isolation_enabled(&off_rec).is_err(), "{bad}");
        }
        {
            let _set = EnvSet::set(ENV_WORKTREE, "off");
            assert!(!isolation_enabled(&on_rec).unwrap());
        }
        for good in ["1", "true", "on"] {
            let _set = EnvSet::set(ENV_WORKTREE, good);
            assert!(isolation_enabled(&off_rec).unwrap(), "{good}");
        }
    }

    #[test]
    fn epoch_path_is_under_resolve_state_dir() {
        let ws = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let rec = record(
            ws.path().to_path_buf(),
            None,
            Some(state.path().to_path_buf()),
            true,
        );
        let dir = epoch_dir(&rec, 4).unwrap();
        assert_eq!(dir, state.path().join("worktrees").join("4"));
        assert!(dir.starts_with(resolve_state_dir(&rec).unwrap()));
    }

    #[test]
    fn nested_scan_ignores_git_file_under_state_worktrees() {
        let ws = tempfile::tempdir().unwrap();
        let product = ws.path().join("product");
        std::fs::create_dir(&product).unwrap();
        std::fs::write(product.join("Cargo.toml"), b"[package]\nname=\"p\"\n").unwrap();
        let state = tempfile::tempdir().unwrap();
        let hidden = state.path().join("worktrees").join("1");
        std::fs::create_dir_all(&hidden).unwrap();
        std::fs::write(hidden.join(".git"), b"gitdir: /tmp/example\n").unwrap();
        let nested = ws.path().join(".coordinator").join("worktrees").join("1");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join(".git"), b"gitdir: /tmp/example\n").unwrap();
        assert_eq!(auto_detect_nested_execution(ws.path()), Some(product));
    }

    #[test]
    fn add_detach_lists_the_worktree_and_dirty_remove_uses_force() {
        let repo = init_repo();
        let ws = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let rec = record(
            ws.path().to_path_buf(),
            Some(repo.path().to_path_buf()),
            Some(state.path().to_path_buf()),
            true,
        );
        prepare_epoch(&rec, 1).unwrap();
        let epoch = epoch_dir(&rec, 1).unwrap();
        assert!(epoch.join("README.md").is_file());
        let listed = git_stdout(repo.path(), &["worktree", "list", "--porcelain"]).unwrap();
        assert!(
            listed.contains(&epoch.display().to_string())
                || listed.lines().any(|l| {
                    l.strip_prefix("worktree ")
                        .is_some_and(|p| paths_same(Path::new(p.trim()), &epoch))
                })
        );
        std::fs::write(epoch.join("dirty.txt"), b"dirty\n").unwrap();
        let mut state_run = crate::state::RunState::idle(&rec.id);
        state_run.run_epoch = 1;
        state_run.status = RunStatus::Idle;
        let view = StatusView::from_record(&rec, &state_run);
        release_if_idle(&rec, &view);
        assert!(!epoch.exists());
        assert!(porcelain(repo.path()).is_empty());
    }

    #[test]
    fn idle_alive_holder_blocks_age_reap() {
        let _guard = crate::config::test_env_lock();
        let _reap = EnvSet::set(ENV_WORKTREE_REAP_SECS, "0");
        let repo = init_repo();
        let ws = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let rec = record(
            ws.path().to_path_buf(),
            Some(repo.path().to_path_buf()),
            Some(state.path().to_path_buf()),
            true,
        );
        prepare_epoch(&rec, 1).unwrap();
        let epoch = epoch_dir(&rec, 1).unwrap();
        let persist = crate::harness::persist_path(&rec).unwrap();
        std::fs::write(
            &persist,
            format!(
                r#"{{"version":1,"project_id":"{}","cwd":"{}","adapter":"grok","supports_compact":true,"alive":true,"prompt_in_flight":false}}"#,
                rec.id,
                epoch.display().to_string().replace('\\', "\\\\")
            ),
        )
        .unwrap();
        reap_aged(&rec).unwrap();
        assert!(epoch.is_dir(), "alive holder must keep the epoch directory");
    }

    #[test]
    fn missing_directory_prunes_admin_metadata() {
        let repo = init_repo();
        let ws = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let rec = record(
            ws.path().to_path_buf(),
            Some(repo.path().to_path_buf()),
            Some(state.path().to_path_buf()),
            true,
        );
        prepare_epoch(&rec, 1).unwrap();
        let epoch = epoch_dir(&rec, 1).unwrap();
        std::fs::remove_dir_all(&epoch).unwrap();
        let mut state_run = crate::state::RunState::idle(&rec.id);
        state_run.run_epoch = 1;
        let view = StatusView::from_record(&rec, &state_run);
        release_if_idle(&rec, &view);
        let listed = git_stdout(repo.path(), &["worktree", "list", "--porcelain"]).unwrap();
        assert!(
            !listed.lines().any(|l| l
                .strip_prefix("worktree ")
                .is_some_and(|p| { paths_same(Path::new(p.trim()), &epoch) })),
            "prune must drop the missing worktree: {listed}"
        );
    }

    #[test]
    fn layout_routes_implement_at_the_worktree_and_plan_keeps_shared_repo() {
        let ws = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let exec = ws.path().join("exec");
        std::fs::create_dir_all(&exec).unwrap();
        let rec = record(
            ws.path().to_path_buf(),
            Some(exec.clone()),
            Some(state.path().to_path_buf()),
            true,
        );
        ensure_state_dir(&rec).unwrap();
        let mut state_run = crate::state::RunState::idle(&rec.id);
        state_run.run_epoch = 3;
        save_run_state(&rec, &state_run).unwrap();
        let epoch = epoch_dir(&rec, 3).unwrap();
        std::fs::create_dir_all(&epoch).unwrap();
        let plan = crate::workflow::prompts::layout_block(&rec, None, PHASE_PLAN);
        assert!(plan.contains(&format!("Execution repo: {}", exec.display())));
        assert!(plan.contains(&format!("Epoch worktree: {}", epoch.display())));
        let implement = crate::workflow::prompts::layout_block(&rec, None, PHASE_IMPLEMENT);
        assert!(implement.contains(&format!("Execution repo: {}", epoch.display())));
        let address = crate::workflow::prompts::layout_block(&rec, None, PHASE_ADDRESS_FINDINGS);
        assert!(address.contains(&format!("Execution repo: {}", epoch.display())));
        let prompt = crate::workflow::prompts::phase_prompt(&rec, PHASE_IMPLEMENT, Some("0066"));
        let folded = prompt.replace('\\', "/");
        let epoch_folded = epoch.display().to_string().replace('\\', "/");
        assert!(folded.contains(&format!("{epoch_folded}/.agents/skills/implement/SKILL.md")));
        let review = crate::workflow::plan_review::agy_prompt(&rec, None);
        assert!(review.contains(&format!("Execution repo: {}", exec.display())));
        assert!(review.contains("Epoch worktree:"));
        let off = record(
            ws.path().to_path_buf(),
            Some(exec.clone()),
            Some(state.path().to_path_buf()),
            false,
        );
        let off_plan = crate::workflow::prompts::layout_block(&off, None, PHASE_PLAN);
        assert!(!off_plan.contains("Epoch worktree:"));
        assert!(cargo_target_dir_for(&off, &epoch).is_none());
        let target = cargo_target_dir_for(&rec, &epoch).unwrap();
        assert!(target.is_absolute());
        assert_eq!(target, state.path().join("cargo-target"));
    }

    #[test]
    fn stopped_age_sweep_reaps_without_poll_and_running_epoch_stays() {
        let _guard = crate::config::test_env_lock();
        let _reap = EnvSet::set(ENV_WORKTREE_REAP_SECS, "0");
        let repo = init_repo();
        let ws = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let rec = record(
            ws.path().to_path_buf(),
            Some(repo.path().to_path_buf()),
            Some(state.path().to_path_buf()),
            true,
        );
        prepare_epoch(&rec, 1).unwrap();
        prepare_epoch(&rec, 2).unwrap();
        let old = epoch_dir(&rec, 1).unwrap();
        let current = epoch_dir(&rec, 2).unwrap();
        assert!(!old.exists(), "prepare reaps the earlier epoch");
        let mut running = crate::state::RunState::idle(&rec.id);
        running.status = RunStatus::Running;
        running.run_epoch = 2;
        save_run_state(&rec, &running).unwrap();
        reap_aged(&rec).unwrap();
        assert!(current.is_dir());
        running.status = RunStatus::Stopped;
        save_run_state(&rec, &running).unwrap();
        reap_aged(&rec).unwrap();
        assert!(!current.exists());
    }

    #[test]
    fn sequential_runs_isolate_epochs_and_keep_track_branches() {
        let repo = init_repo();
        let ws = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let rec = record(
            ws.path().to_path_buf(),
            Some(repo.path().to_path_buf()),
            Some(state.path().to_path_buf()),
            true,
        );
        let first = run_with_driver(&rec, Some("0066".into()), WorkflowDriver::Stub).unwrap();
        assert_eq!(first.run_epoch, 1);
        assert_eq!(first.status, RunStatus::Running);
        let epoch1 = epoch_dir(&rec, 1).unwrap();
        let wrote = tick(&rec).unwrap().expect("stub plan tick");
        assert_eq!(wrote.phase, "plan-review");
        assert_eq!(
            std::fs::read_to_string(epoch1.join("epoch-write.txt")).unwrap(),
            "epoch=1 phase=plan\n"
        );
        assert!(porcelain(repo.path()).is_empty());
        assert!(!repo.path().join("epoch-write.txt").exists());
        assert!(branch_exists(repo.path(), "track/0099-keep"));
        run::stop(&rec).unwrap();
        assert!(epoch1.is_dir(), "operator stop must leave the epoch");
        let second = run_with_driver(&rec, Some("0066".into()), WorkflowDriver::Stub).unwrap();
        assert_eq!(second.run_epoch, 2);
        assert!(!epoch1.exists());
        let epoch2 = epoch_dir(&rec, 2).unwrap();
        tick(&rec).unwrap().expect("second stub plan tick");
        assert_eq!(
            std::fs::read_to_string(epoch2.join("epoch-write.txt")).unwrap(),
            "epoch=2 phase=plan\n"
        );
        assert!(porcelain(repo.path()).is_empty());
        assert!(branch_exists(repo.path(), "track/0099-keep"));
        assert!(!repo.path().join("epoch-write.txt").exists());
        let state_dir = resolve_state_dir(&rec).unwrap();
        assert!(run_state_path(&rec).unwrap().starts_with(&state_dir));
        assert!(outcome_current_path(&rec).unwrap().starts_with(&state_dir));
        assert!(
            crate::workflow::watchdog::progress_path(&rec)
                .unwrap()
                .starts_with(&state_dir)
        );
        assert!(
            crate::harness::persist_path(&rec)
                .unwrap()
                .starts_with(&state_dir)
        );
        assert!(!epoch2.join(".coordinator").exists());
        assert!(!epoch2.join("run-state.json").exists());
    }

    #[test]
    fn auto_start_prepares_the_next_epoch_worktree() {
        let repo = init_repo();
        let ws = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let cond = ws.path().join("conductor");
        std::fs::create_dir_all(cond.join("0001-Example")).unwrap();
        std::fs::create_dir_all(cond.join("0002-Next")).unwrap();
        std::fs::write(
            cond.join("conductor.md"),
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0001-Example](0001-Example/spec.md) | `.` | **Ready - not started** | one |\n\
             | [0002-Next](0002-Next/spec.md) | `.` | **Ready - not started** | next |\n",
        )
        .unwrap();
        let mut rec = record(
            ws.path().to_path_buf(),
            Some(repo.path().to_path_buf()),
            Some(state.path().to_path_buf()),
            true,
        );
        rec.auto_start = crate::registry::AutoStartPolicy::Full;
        let started = run_with_driver(&rec, Some("0001".into()), WorkflowDriver::Stub).unwrap();
        assert_eq!(started.run_epoch, 1);
        assert!(epoch_dir(&rec, 1).unwrap().is_dir());
        let mut saved = load_run_state(&rec).unwrap();
        saved.phase = PHASE_ADVANCE.into();
        save_run_state(&rec, &saved).unwrap();
        let outcome = PhaseOutcome::success(PHASE_ADVANCE, OutcomeSource::Test, None, None, None);
        let view = write_and_apply(&rec, outcome).unwrap();
        assert_eq!(view.status, RunStatus::Running, "{}", view.last_event);
        assert_eq!(view.track_id.as_deref(), Some("0002"));
        assert_eq!(view.run_epoch, 2);
        assert_eq!(view.phase, PHASE_PLAN);
        let epoch2 = epoch_dir(&rec, 2).unwrap();
        assert!(epoch2.is_dir(), "successor epoch directory");
        assert!(
            !epoch_dir(&rec, 1).unwrap().exists(),
            "previous epoch is reaped"
        );
        assert!(paths_same(&product_git_cwd(&rec).unwrap(), &epoch2));
        tick(&rec).unwrap().expect("successor plan tick");
        assert_eq!(
            std::fs::read_to_string(epoch2.join("epoch-write.txt")).unwrap(),
            "epoch=2 phase=plan\n"
        );
        assert!(porcelain(repo.path()).is_empty());
        assert!(!repo.path().join("epoch-write.txt").exists());
        assert!(branch_exists(repo.path(), "track/0099-keep"));
    }

    #[test]
    fn serve_poll_loop_reaps_stopped_epoch_without_polling() {
        let _guard = crate::config::test_env_lock();
        let home = tempfile::tempdir().unwrap();
        let _home_env = RestoreEnv::set(
            crate::config::ENV_COORDINATOR_HOME,
            home.path().to_str().unwrap(),
        );
        let _poll_env = RestoreEnv::set(crate::config::ENV_OUTCOME_POLL_MS, "30");
        let _reap_env = RestoreEnv::set(ENV_WORKTREE_REAP_SECS, "0");
        let repo = init_repo();
        let ws = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let opts = crate::registry::ProjectAddOptions {
            layout_profile: crate::layout::LayoutProfile::Nested,
            execution_repo: Some(repo.path().to_path_buf()),
            state_dir: Some(state.path().to_path_buf()),
            ..Default::default()
        };
        let mut reg = crate::registry::Registry::default();
        reg.add(ws.path(), opts).unwrap();
        reg.projects[0].worktree_isolation = true;
        let rec = reg.projects[0].clone();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        prepare_epoch(&rec, 1).unwrap();
        let epoch = epoch_dir(&rec, 1).unwrap();
        assert!(epoch.is_dir());
        let mut stopped = crate::state::RunState::idle(&rec.id);
        stopped.status = RunStatus::Stopped;
        stopped.run_epoch = 1;
        save_run_state(&rec, &stopped).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (tx, rx) = tokio::sync::watch::channel(false);
            let task = tokio::spawn(crate::watch::serve_poll_loop(rx));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            while epoch.exists() && std::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            }
            let _ = tx.send(true);
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
        });
        assert!(!epoch.exists(), "idle sweep removes a Stopped epoch");
        let after = load_run_state(&rec).unwrap();
        assert_eq!(after.status, RunStatus::Stopped);
        assert_eq!(after.run_epoch, 1);
    }

    #[test]
    fn flag_off_run_creates_no_worktree() {
        let repo = init_repo();
        let ws = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let rec = record(
            ws.path().to_path_buf(),
            Some(repo.path().to_path_buf()),
            Some(state.path().to_path_buf()),
            false,
        );
        let view = run(&rec, Some("0066".into())).unwrap();
        assert_eq!(view.status, RunStatus::Running);
        assert!(!worktrees_root(&rec).unwrap().exists());
        assert!(porcelain(repo.path()).is_empty());
        assert!(cargo_target_dir_for(&rec, repo.path()).is_none());
    }

    #[test]
    fn idle_apply_removes_the_epoch_after_shutdown() {
        let repo = init_repo();
        let ws = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let rec = record(
            ws.path().to_path_buf(),
            Some(repo.path().to_path_buf()),
            Some(state.path().to_path_buf()),
            true,
        );
        run(&rec, Some("0066".into())).unwrap();
        let epoch = epoch_dir(&rec, 1).unwrap();
        assert!(epoch.is_dir());
        let mut saved = load_run_state(&rec).unwrap();
        saved.phase = PHASE_ADVANCE.into();
        save_run_state(&rec, &saved).unwrap();
        let outcome = PhaseOutcome::success(PHASE_ADVANCE, OutcomeSource::Test, None, None, None);
        let view = write_and_apply(&rec, outcome).unwrap();
        assert_eq!(view.status, RunStatus::Idle);
        assert!(!epoch.exists());
    }

    #[test]
    fn no_execution_repo_fails_closed() {
        let ws = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let rec = record(
            ws.path().to_path_buf(),
            None,
            Some(state.path().to_path_buf()),
            true,
        );
        let err = prepare_epoch(&rec, 1).unwrap_err();
        assert!(err.to_string().contains("no execution repo"), "{err}");
        assert!(!worktrees_root(&rec).unwrap().join("1").exists());
    }
}
