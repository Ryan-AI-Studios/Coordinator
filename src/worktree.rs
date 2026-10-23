//! Per-`run_epoch` detached git worktree (track 0066).
//!
//! The session pool stays keyed by `project_id`. When isolation is on, one epoch
//! directory under the project state dir is the product cwd. Flag off is a no-op.

use std::path::{Path, PathBuf};
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

/// Prepare the detached worktree for `next_epoch` before run state becomes Running.
///
/// Flag off returns immediately. A git failure leaves run state unsaved. An unproven
/// holder fails the epoch closed **before** `clear_progress`/`create_dir_all`.
pub fn prepare_epoch(record: &ProjectRecord, next_epoch: u64) -> Result<()> {
    if !isolation_enabled(record)? {
        return Ok(());
    }
    let exec = execution_repo(record)?;
    let root = worktrees_root(record)?;
    let target = root.join(next_epoch.to_string());
    shutdown_under(record, &root, Some(&target)).map_err(|e| {
        CoordinatorError::Message(format!(
            "worktree: epoch {next_epoch} holder unproven; refusing to prepare the next epoch \
             (persist: {}): {e}",
            persist_display(record)
        ))
    })?;
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
/// A failed remove is journaled and does not roll the Idle commit back; an unproven
/// holder keeps the tree.
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
    if !shutdown_if_cwd(record, &dir) {
        return;
    }
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

pub(crate) fn path_is_inside(path: &Path, root: &Path) -> bool {
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
    // An unreadable persist is bound: keep the tree rather than guess.
    let Ok(snap) = crate::harness::pool::persist_live(record) else {
        return true;
    };
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

/// What a worktree termination did. `Unchanged` means nothing in this epoch was bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShutdownOutcome {
    Unchanged,
    Handled,
}

fn persist_display(record: &ProjectRecord) -> String {
    crate::harness::pool::persist_path(record)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "{state_dir}/harness-grok.json".into())
}

fn holder_refused(record: &ProjectRecord, detail: &str) -> CoordinatorError {
    CoordinatorError::Message(format!(
        "holder-changed: {detail} (persist: {}). {}",
        persist_display(record),
        crate::harness::pool::recovery_hint(record)
    ))
}

fn holder_unproven(record: &ProjectRecord, why: &str) -> CoordinatorError {
    CoordinatorError::Message(format!(
        "holder unproven ({why}) (persist: {}). {}",
        persist_display(record),
        crate::harness::pool::recovery_hint(record)
    ))
}

fn shutdown_under(
    record: &ProjectRecord,
    root: &Path,
    keep: Option<&Path>,
) -> Result<ShutdownOutcome> {
    use crate::harness::pool::{self, Intent, PersistClass, PersistRef, WaitGone};

    let (identity, mtime) = match pool::persist_identity(record) {
        Ok(Some(found)) => found,
        Ok(None) => {
            detach_if(record, |cwd| {
                path_is_inside(cwd, root) && keep.is_none_or(|keep| !paths_same(cwd, keep))
            });
            return Ok(ShutdownOutcome::Unchanged);
        }
        // Unreadable is `Unproven`, never absent: keep the persist **and** the cancel
        // registration, before `clear_progress`/`create_dir_all`.
        Err(e) => {
            return Err(CoordinatorError::Message(format!(
                "holder persist unreadable ({e}); keeping it (persist: {})",
                persist_display(record)
            )));
        }
    };
    let old = PersistRef::new(identity.clone(), mtime);
    if let PersistClass::Addressed { .. } = pool::classify_persist(&identity) {
        let Some(cwd) = identity.cwd.as_deref() else {
            return Ok(ShutdownOutcome::Unchanged);
        };
        if !path_is_inside(cwd, root) || keep.is_some_and(|keep| paths_same(cwd, keep)) {
            return Ok(ShutdownOutcome::Unchanged);
        }
    }

    match pool::classify_persist(&identity) {
        PersistClass::NoAddress(why) => {
            if worktree_settle_no_address(record, &old, why)? {
                // Table B: one line + compare-clear + detach. The pooled session (if
                // any) is dropped only after a successful clear.
                teardown_epoch(record, old.identity.cwd.as_deref(), root);
                Ok(ShutdownOutcome::Handled)
            } else {
                Ok(ShutdownOutcome::Unchanged)
            }
        }
        PersistClass::Addressed { .. } => {
            let termination = pool::terminate_persisted_holder(record, Intent::Reap)?;
            let outcome = termination.outcome;
            let Some(old) = termination.old else {
                return Ok(ShutdownOutcome::Unchanged);
            };
            match outcome {
                pool::HolderTermination::Delivered { .. } => {
                    match pool::wait_holder_gone(record, &old, pool::WAIT_GONE) {
                        WaitGone::Gone { .. } => {
                            pool::after_gone(record);
                            if !pool::clear_persist_if(record, &old) {
                                return Err(holder_refused(
                                    record,
                                    "the record changed before the clear",
                                ));
                            }
                            teardown_epoch(record, old.identity.cwd.as_deref(), root);
                            Ok(ShutdownOutcome::Handled)
                        }
                        WaitGone::Superseded => Err(holder_refused(
                            record,
                            "a newer holder owns the persist after the wait",
                        )),
                        WaitGone::TimedOut => Err(CoordinatorError::Message(format!(
                            "delivered; the holder has not written its exit record; retry (persist: {})",
                            persist_display(record)
                        ))),
                    }
                }
                pool::HolderTermination::Exited { .. } => {
                    pool::after_gone(record);
                    if !pool::clear_persist_if(record, &old) {
                        return Err(holder_refused(
                            record,
                            "the record changed before the clear",
                        ));
                    }
                    teardown_epoch(record, old.identity.cwd.as_deref(), root);
                    Ok(ShutdownOutcome::Handled)
                }
                pool::HolderTermination::NoListener { addr } => {
                    Err(CoordinatorError::Message(format!(
                        "no listener at {addr}: the holder control socket accepted no connection \
                         (persist: {}). {}",
                        persist_display(record),
                        crate::harness::pool::recovery_hint(record)
                    )))
                }
                pool::HolderTermination::NoSocket { why } => {
                    if worktree_settle_no_address(record, &old, why)? {
                        Ok(ShutdownOutcome::Handled)
                    } else {
                        Ok(ShutdownOutcome::Unchanged)
                    }
                }
                pool::HolderTermination::Unproven { why } => Err(holder_unproven(record, &why)),
            }
        }
    }
}

fn teardown_epoch(record: &ProjectRecord, cwd: Option<&Path>, root: &Path) {
    let cwd = cwd.map(|cwd| cwd.to_path_buf());
    detach_if(record, |candidate| match &cwd {
        Some(cwd) => paths_same(candidate, cwd),
        None => path_is_inside(candidate, root),
    });
    crate::harness::abort::unregister_cancel_handle(&record.id);
}

/// Table B for the worktree callers. `Ok(true)` = handled/clearable, `Ok(false)` =
/// nothing to do, `Err` = refuse and keep everything.
fn worktree_settle_no_address(
    record: &ProjectRecord,
    old: &crate::harness::pool::PersistRef,
    why: crate::harness::pool::NoSocketWhy,
) -> Result<bool> {
    use crate::harness::pool::{self, NoAddressAction, PersistClass};

    let mut action = pool::no_address_decision(&why, old.mtime, SystemTime::now());
    loop {
        match action {
            NoAddressAction::DiscardNow => {
                pool::discard_line(record, old);
                pool::after_gone(record);
                if !pool::clear_persist_if(record, old) {
                    return Err(holder_refused(
                        record,
                        "the record changed before the clear",
                    ));
                }
                return Ok(true);
            }
            NoAddressAction::WaitUntil(at) => {
                let now = SystemTime::now();
                if at > now {
                    std::thread::sleep(at.duration_since(now).unwrap_or(Duration::ZERO));
                }
                let Some((identity, mtime)) = pool::persist_identity(record)? else {
                    return Ok(false);
                };
                if !pool::same_record(old, &identity, mtime) {
                    return Err(holder_refused(
                        record,
                        "a newer record owns the persist during the grace wait",
                    ));
                }
                match pool::classify_persist(&identity) {
                    PersistClass::NoAddress(next) => {
                        action = pool::no_address_decision(&next, mtime, SystemTime::now());
                    }
                    PersistClass::Addressed { .. } => {
                        return Err(holder_refused(
                            record,
                            "the record gained an address during the grace wait",
                        ));
                    }
                }
            }
            NoAddressAction::Refuse(reason) => {
                return Err(CoordinatorError::Message(format!(
                    "{reason}: refusing to prepare or release a worktree over an authoritative \
                     record (persist: {}). {}",
                    persist_display(record),
                    pool::recovery_hint(record)
                )));
            }
            NoAddressAction::OwnedInProcess => {
                let cwd = old.identity.cwd.clone();
                let torn = pool::teardown_pooled_if_cwd(
                    &record.id,
                    |candidate| match &cwd {
                        Some(cwd) => paths_same(candidate, cwd),
                        None => true,
                    },
                    pool::POOL_LOCK_BUDGET,
                );
                if !torn && !pool::pool_lock_available() {
                    return Err(CoordinatorError::Message(format!(
                        "pool lock busy: the owned session was not torn down and its cancel \
                         registration is kept (persist: {}). Retry.",
                        persist_display(record)
                    )));
                }
                pool::after_gone(record);
                if !pool::clear_persist_if(record, old) {
                    return Err(holder_refused(
                        record,
                        "the record changed before the clear",
                    ));
                }
                // The owned session is gone (or this is a stale self record): drop the
                // registration only now, never before the teardown.
                crate::harness::abort::unregister_cancel_handle(&record.id);
                return Ok(true);
            }
        }
    }
}

/// `false` means "keep the tree": unproven, unreadable, superseded, timed out, a false
/// compare, a foreign no-address record, or a lock that is still held.
fn shutdown_if_cwd(record: &ProjectRecord, epoch_path: &Path) -> bool {
    shutdown_if_cwd_inner(record, epoch_path).unwrap_or_else(|e| {
        eprintln!("coordinator: worktree: release refused: {e}");
        false
    })
}

fn shutdown_if_cwd_inner(record: &ProjectRecord, epoch_path: &Path) -> Result<bool> {
    use crate::harness::pool::{self, Intent, PersistClass, PersistRef, WaitGone};

    let (identity, mtime) = match pool::persist_identity(record) {
        Ok(Some(found)) => found,
        Ok(None) => {
            detach_if(record, |cwd| {
                paths_same(cwd, epoch_path) || path_is_inside(cwd, epoch_path)
            });
            return Ok(true);
        }
        Err(e) => {
            return Err(CoordinatorError::Message(format!(
                "holder persist unreadable ({e}); keeping it (persist: {})",
                persist_display(record)
            )));
        }
    };
    let old = PersistRef::new(identity.clone(), mtime);
    if let PersistClass::Addressed { .. } = pool::classify_persist(&identity) {
        let Some(cwd) = identity.cwd.as_deref() else {
            return Ok(true);
        };
        if !(paths_same(cwd, epoch_path) || path_is_inside(cwd, epoch_path)) {
            // Another epoch's session: nothing here to dispose.
            return Ok(true);
        }
    }

    match pool::classify_persist(&identity) {
        PersistClass::NoAddress(why) => {
            if worktree_settle_no_address(record, &old, why)? {
                detach_if(record, |cwd| {
                    paths_same(cwd, epoch_path) || path_is_inside(cwd, epoch_path)
                });
                // The owned session is gone (or this was a stale self record): drop the
                // registration only after the clear + detach.
                crate::harness::abort::unregister_cancel_handle(&record.id);
            }
            Ok(true)
        }
        PersistClass::Addressed { .. } => {
            let termination = pool::terminate_persisted_holder(record, Intent::Reap)?;
            let outcome = termination.outcome;
            let Some(old) = termination.old else {
                return Ok(true);
            };
            let cleared = match outcome {
                pool::HolderTermination::Delivered { .. } => {
                    match pool::wait_holder_gone(record, &old, pool::WAIT_GONE) {
                        WaitGone::Gone { .. } => {
                            pool::after_gone(record);
                            pool::clear_persist_if(record, &old)
                        }
                        WaitGone::Superseded | WaitGone::TimedOut => return Ok(false),
                    }
                }
                pool::HolderTermination::Exited { .. } => {
                    pool::after_gone(record);
                    pool::clear_persist_if(record, &old)
                }
                pool::HolderTermination::NoListener { .. } => return Ok(false),
                pool::HolderTermination::NoSocket { why } => {
                    return Ok(
                        worktree_settle_no_address(record, &old, why).unwrap_or_else(|e| {
                            eprintln!("coordinator: worktree: release refused: {e}");
                            false
                        }),
                    );
                }
                pool::HolderTermination::Unproven { why } => {
                    return Err(holder_unproven(record, &why));
                }
            };
            if !cleared {
                return Err(holder_refused(
                    record,
                    "the record changed before the clear",
                ));
            }
            teardown_epoch(record, old.identity.cwd.as_deref(), epoch_path);
            Ok(true)
        }
    }
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
    let out = crate::git_command::command()
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
#[allow(clippy::await_holding_lock)]
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
            ci_fix_routing: false,
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: Utc::now(),
        }
    }

    fn git_ok(cwd: &Path, args: &[&str]) {
        let out = crate::git_command::command()
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
        let out = crate::git_command::command()
            .args(["status", "--porcelain"])
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn branch_exists(cwd: &Path, name: &str) -> bool {
        let out = crate::git_command::command()
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

    // ---- 0117: worktree paths fail closed ------------------------------------

    fn persist_body(
        rec: &ProjectRecord,
        session: Option<&str>,
        cwd: Option<&Path>,
        alive: bool,
        addr: Option<&str>,
        holder: Option<u32>,
        error: Option<&str>,
    ) -> serde_json::Value {
        serde_json::json!({
            "version": 1,
            "project_id": rec.id,
            "session_id": session,
            "cwd": cwd.map(|p| p.display().to_string()),
            "pid": 4242,
            "holder_pid": holder,
            "control_addr": addr,
            "adapter": "grok",
            "supports_compact": true,
            "alive": alive,
            "prompt_in_flight": false,
            "error": error,
        })
    }

    fn write_persist(rec: &ProjectRecord, body: &serde_json::Value) -> std::path::PathBuf {
        let path = crate::harness::pool::persist_path(rec).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body.to_string()).unwrap();
        path
    }

    /// Register a real cancel handle for the sync worktree tests.
    fn register_dummy_cancel(rec: &ProjectRecord) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let session = rt
            .block_on(crate::harness::grok::GrokSession::start_mock(
                rec.path.clone(),
                crate::harness::grok::mock_handshake_ok("sess-cancel"),
                Duration::from_secs(2),
            ))
            .unwrap();
        crate::harness::abort::register_cancel_handle(rec.id.clone(), session.cancel_handle());
    }

    fn aged(path: &Path, age: std::time::Duration) {
        let at = SystemTime::now() - age;
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(at)
            .unwrap();
    }

    enum WtMode {
        Normal,
        PingMismatch,
        ShutdownNoExit,
        ShutdownMutate,
    }

    struct WtHolder {
        addr: String,
        /// Ordered ops the scripted holder saw (asserted by the refusal tests).
        #[allow(dead_code)]
        ops: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for WtHolder {
        fn drop(&mut self) {
            if let Some(handle) = self.handle.take() {
                if let Ok(sock) = self.addr.parse::<std::net::SocketAddr>()
                    && let Ok(mut stream) =
                        std::net::TcpStream::connect_timeout(&sock, Duration::from_millis(200))
                {
                    use std::io::Write;
                    let _ = stream.write_all(b"{\"op\":\"stop-test\"}\n");
                }
                let _ = handle.join();
            }
        }
    }

    fn wt_holder(persist: std::path::PathBuf, session: &str, cwd: &Path, mode: WtMode) -> WtHolder {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let ops = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let ops2 = ops.clone();
        let session = session.to_string();
        let cwd = cwd.display().to_string();
        let handle = std::thread::spawn(move || {
            use std::io::{BufRead, BufReader, Write};
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let Ok(mut reader) = stream.try_clone().map(BufReader::new) else {
                    continue;
                };
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                let op = serde_json::from_str::<serde_json::Value>(line.trim())
                    .ok()
                    .and_then(|v| {
                        v.get("op")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                    })
                    .unwrap_or_default();
                ops2.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(op.clone());
                if op == "stop-test" {
                    break;
                }
                match op.as_str() {
                    "ping" => {
                        let sid = if matches!(mode, WtMode::PingMismatch) {
                            "mismatched".to_string()
                        } else {
                            session.clone()
                        };
                        let body = serde_json::json!({
                            "ok": true,
                            "harness": {
                                "alive": true,
                                "session_id": sid,
                                "cwd": cwd,
                                "pid": 4242,
                                "adapter": "grok",
                                "supports_compact": true,
                            }
                        });
                        let _ = stream.write_all(body.to_string().as_bytes());
                        let _ = stream.write_all(b"\n");
                    }
                    "shutdown" => {
                        if matches!(mode, WtMode::ShutdownMutate) {
                            let mut body: serde_json::Value =
                                serde_json::from_str(&std::fs::read_to_string(&persist).unwrap())
                                    .unwrap();
                            body["session_id"] = serde_json::Value::String("sess-newer".into());
                            let _ = std::fs::write(&persist, body.to_string());
                        }
                        let _ = stream.write_all(b"{\"ok\":true}\n");
                        if matches!(mode, WtMode::Normal) {
                            let mut body: serde_json::Value =
                                serde_json::from_str(&std::fs::read_to_string(&persist).unwrap())
                                    .unwrap();
                            body["alive"] = serde_json::Value::Bool(false);
                            body["control_addr"] = serde_json::Value::Null;
                            let _ = std::fs::write(&persist, body.to_string());
                        }
                        if !matches!(mode, WtMode::ShutdownNoExit | WtMode::ShutdownMutate) {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        });
        WtHolder {
            addr,
            ops,
            handle: Some(handle),
        }
    }

    fn epoch_fixture() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        ProjectRecord,
        std::path::PathBuf,
    ) {
        let repo = init_repo();
        let ws = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let rec = record(
            ws.path().to_path_buf(),
            Some(repo.path().to_path_buf()),
            Some(state.path().to_path_buf()),
            true,
        );
        // A stray override would move every state path under a sibling test's dir.
        unsafe { std::env::remove_var(crate::config::ENV_COORDINATOR_STATE_DIR) };
        let target = epoch_dir(&rec, 1).unwrap();
        // The audit trail must survive a refusal (clear_progress runs after it).
        let progress = crate::workflow::watchdog::progress_path(&rec).unwrap();
        std::fs::create_dir_all(progress.parent().unwrap()).unwrap();
        std::fs::write(&progress, b"{}").unwrap();
        (repo, state, rec, target)
    }

    #[test]
    fn prepare_epoch_refuses_when_holder_unproven() {
        let _guard = crate::config::test_env_lock();
        let (_repo, _state, rec, target) = epoch_fixture();
        let progress = crate::workflow::watchdog::progress_path(&rec).unwrap();
        let path = write_persist(
            &rec,
            &persist_body(&rec, Some("sess"), None, true, None, Some(999_991), None),
        );
        register_dummy_cancel(&rec);
        let before = std::fs::read(&path).unwrap();
        let err = prepare_epoch(&rec, 1).unwrap_err().to_string();
        assert!(err.contains("foreign-holder"), "{err}");
        assert!(!target.exists(), "the next epoch is not created");
        assert_eq!(before, std::fs::read(&path).unwrap());
        assert!(
            crate::harness::abort::cancel_handle_for(&rec.id).is_some(),
            "the cancel registration survives"
        );
        assert!(progress.exists(), "clear_progress did not run");
        crate::harness::abort::unregister_cancel_handle(&rec.id);
    }

    #[test]
    fn prepare_epoch_refuses_on_ping_unproven() {
        let _guard = crate::config::test_env_lock();
        let (_repo, _state, rec, _first) = epoch_fixture();
        prepare_epoch(&rec, 1).unwrap();
        let epoch = epoch_dir(&rec, 1).unwrap();
        let path = crate::harness::pool::persist_path(&rec).unwrap();
        let holder = wt_holder(path.clone(), "sess", &epoch, WtMode::PingMismatch);
        write_persist(
            &rec,
            &persist_body(
                &rec,
                Some("sess"),
                Some(&epoch),
                true,
                Some(&holder.addr),
                Some(4242),
                None,
            ),
        );
        let target = epoch_dir(&rec, 2).unwrap();
        let err = prepare_epoch(&rec, 2).unwrap_err().to_string();
        assert!(err.contains("identity:session"), "{err}");
        assert!(!target.exists());
    }

    #[test]
    fn prepare_epoch_refuses_when_delivered_holder_never_goes_dead() {
        let _guard = crate::config::test_env_lock();
        let (_repo, _state, rec, _first) = epoch_fixture();
        prepare_epoch(&rec, 1).unwrap();
        let epoch = epoch_dir(&rec, 1).unwrap();
        let path = crate::harness::pool::persist_path(&rec).unwrap();
        let holder = wt_holder(path.clone(), "sess", &epoch, WtMode::ShutdownNoExit);
        write_persist(
            &rec,
            &persist_body(
                &rec,
                Some("sess"),
                Some(&epoch),
                true,
                Some(&holder.addr),
                Some(4242),
                None,
            ),
        );
        let target = epoch_dir(&rec, 2).unwrap();
        let before = std::fs::read(&path).unwrap();
        let err = prepare_epoch(&rec, 2).unwrap_err().to_string();
        assert!(err.contains("retry"), "{err}");
        assert!(!target.exists());
        assert_eq!(before, std::fs::read(&path).unwrap());
    }

    #[test]
    fn prepare_epoch_refuses_when_persist_unreadable() {
        let _guard = crate::config::test_env_lock();
        let (_repo, _state, rec, target) = epoch_fixture();
        let path = crate::harness::pool::persist_path(&rec).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        let progress = crate::workflow::watchdog::progress_path(&rec).unwrap();
        let err = prepare_epoch(&rec, 1).unwrap_err().to_string();
        assert!(err.contains("unreadable"), "{err}");
        assert!(!target.exists());
        assert!(progress.exists(), "the refusal precedes clear_progress");
        assert_eq!(b"{not json", std::fs::read(&path).unwrap().as_slice());
    }

    #[test]
    fn prepare_epoch_refuses_when_the_persist_is_superseded() {
        let _guard = crate::config::test_env_lock();
        let (_repo, _state, rec, _first) = epoch_fixture();
        prepare_epoch(&rec, 1).unwrap();
        let epoch = epoch_dir(&rec, 1).unwrap();
        let path = crate::harness::pool::persist_path(&rec).unwrap();
        let holder = wt_holder(path.clone(), "sess", &epoch, WtMode::ShutdownMutate);
        write_persist(
            &rec,
            &persist_body(
                &rec,
                Some("sess"),
                Some(&epoch),
                true,
                Some(&holder.addr),
                Some(4242),
                None,
            ),
        );
        let target = epoch_dir(&rec, 2).unwrap();
        let err = prepare_epoch(&rec, 2).unwrap_err().to_string();
        assert!(err.contains("holder-changed"), "{err}");
        assert!(!target.exists());
        let (now, _) = crate::harness::pool::persist_identity(&rec)
            .unwrap()
            .unwrap();
        assert_eq!(now.session_id.as_deref(), Some("sess-newer"));
    }

    #[test]
    fn worktree_refuses_a_fresh_pending_start() {
        let _guard = crate::config::test_env_lock();
        let (_repo, _state, rec, target) = epoch_fixture();
        let path = write_persist(
            &rec,
            &persist_body(&rec, None, None, false, None, None, None),
        );
        let before = std::fs::read(&path).unwrap();
        let err = prepare_epoch(&rec, 1).unwrap_err().to_string();
        assert!(err.contains("start-pending"), "{err}");
        assert!(!target.exists());
        assert_eq!(before, std::fs::read(&path).unwrap());
    }

    #[test]
    fn worktree_discards_an_aged_pending_start() {
        let _guard = crate::config::test_env_lock();
        let (_repo, _state, rec, target) = epoch_fixture();
        let path = write_persist(
            &rec,
            &persist_body(&rec, None, None, false, None, None, None),
        );
        register_dummy_cancel(&rec);
        aged(&path, std::time::Duration::from_secs(61));
        let root = worktrees_root(&rec).unwrap();
        let outcome = shutdown_under(&rec, &root, Some(&target)).unwrap();
        assert_eq!(outcome, ShutdownOutcome::Handled);
        assert!(!path.exists(), "the aged pending record is discarded");
        assert!(
            crate::harness::abort::cancel_handle_for(&rec.id).is_none(),
            "a successful discard detaches and drops the registration"
        );
        let log = std::fs::read_to_string(rec.path.join("status.md")).unwrap_or_default();
        assert!(log.contains("holder-persist-discarded"), "{log}");
    }

    #[test]
    fn release_if_idle_keeps_worktree_when_unproven() {
        let _guard = crate::config::test_env_lock();
        let (_repo, _state, rec, _target) = epoch_fixture();
        prepare_epoch(&rec, 1).unwrap();
        let epoch = epoch_dir(&rec, 1).unwrap();
        assert!(epoch.is_dir());
        write_persist(
            &rec,
            &persist_body(
                &rec,
                Some("sess"),
                Some(&epoch),
                true,
                None,
                Some(999_991),
                None,
            ),
        );
        let state = crate::state::RunState::idle(&rec.id);
        let mut view = StatusView::from_record(&rec, &state);
        view.run_epoch = 1;
        release_if_idle(&rec, &view);
        assert!(epoch.is_dir(), "an unproven holder keeps the tree");
    }

    #[test]
    fn release_if_idle_keeps_worktree_when_unreadable() {
        let _guard = crate::config::test_env_lock();
        let (_repo, _state, rec, _target) = epoch_fixture();
        prepare_epoch(&rec, 1).unwrap();
        let epoch = epoch_dir(&rec, 1).unwrap();
        let path = crate::harness::pool::persist_path(&rec).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        let state = crate::state::RunState::idle(&rec.id);
        let mut view = StatusView::from_record(&rec, &state);
        view.run_epoch = 1;
        release_if_idle(&rec, &view);
        assert!(epoch.is_dir(), "an unreadable persist keeps the tree");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn release_if_idle_tears_down_owned_in_process_session() {
        let _guard = crate::config::test_env_lock();
        let (_repo, _state, rec, _target) = epoch_fixture();
        prepare_epoch(&rec, 1).unwrap();
        let epoch = epoch_dir(&rec, 1).unwrap();
        let session = crate::harness::grok::GrokSession::start_mock(
            epoch.clone(),
            crate::harness::grok::mock_handshake_ok("sess-self"),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        let counter = session.shutdown_counter();
        crate::harness::pool::insert_test_session(rec.id.clone(), session).await;
        write_persist(
            &rec,
            &persist_body(
                &rec,
                Some("sess-self"),
                Some(&epoch),
                true,
                None,
                Some(std::process::id()),
                None,
            ),
        );
        let state = crate::state::RunState::idle(&rec.id);
        let mut view = StatusView::from_record(&rec, &state);
        view.run_epoch = 1;
        release_if_idle(&rec, &view);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(
            !epoch.exists(),
            "the tree is removed after the owned teardown"
        );
        assert!(
            crate::harness::pool::persist_identity(&rec)
                .unwrap()
                .is_none(),
            "the stale self record is cleared"
        );
        assert!(
            crate::harness::abort::cancel_handle_for(&rec.id).is_none(),
            "the registration is dropped only after the owned session is gone"
        );
    }

    #[test]
    fn alive_self_false_compare_is_a_refusal_in_the_worktree_paths() {
        let _guard = crate::config::test_env_lock();
        let (_repo, _state, rec, _first) = epoch_fixture();
        prepare_epoch(&rec, 1).unwrap();
        let epoch = epoch_dir(&rec, 1).unwrap();
        let target = epoch_dir(&rec, 2).unwrap();
        write_persist(
            &rec,
            &persist_body(
                &rec,
                Some("sess-self"),
                Some(&epoch),
                true,
                None,
                Some(std::process::id()),
                None,
            ),
        );
        register_dummy_cancel(&rec);
        let rec_hook = rec.clone();
        let epoch_hook = epoch.clone();
        *crate::harness::pool::AFTER_GONE_HOOK
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(std::sync::Arc::new(move |_| {
            write_persist(
                &rec_hook,
                &persist_body(
                    &rec_hook,
                    Some("sess-successor"),
                    Some(&epoch_hook),
                    true,
                    Some("127.0.0.1:9"),
                    Some(99),
                    None,
                ),
            );
        }));
        let root = worktrees_root(&rec).unwrap();
        let err = shutdown_under(&rec, &root, Some(&target))
            .unwrap_err()
            .to_string();
        assert!(err.contains("holder-changed"), "{err}");
        assert!(!shutdown_if_cwd(&rec, &epoch), "the release refuses too");
        *crate::harness::pool::AFTER_GONE_HOOK
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        crate::harness::abort::unregister_cancel_handle(&rec.id);
        let (now, _) = crate::harness::pool::persist_identity(&rec)
            .unwrap()
            .unwrap();
        assert_eq!(now.session_id.as_deref(), Some("sess-successor"));
        assert!(epoch.is_dir(), "the owned session's tree stays");
    }

    #[test]
    fn gone_then_changed_worktree_keeps_the_tree() {
        let _guard = crate::config::test_env_lock();
        let (_repo, _state, rec, _first) = epoch_fixture();
        prepare_epoch(&rec, 1).unwrap();
        let epoch = epoch_dir(&rec, 1).unwrap();
        let target = epoch_dir(&rec, 2).unwrap();
        let path = crate::harness::pool::persist_path(&rec).unwrap();
        let holder = wt_holder(path.clone(), "sess", &epoch, WtMode::Normal);
        write_persist(
            &rec,
            &persist_body(
                &rec,
                Some("sess"),
                Some(&epoch),
                true,
                Some(&holder.addr),
                Some(4242),
                None,
            ),
        );
        register_dummy_cancel(&rec);
        let rec_hook = rec.clone();
        let epoch_hook = epoch.clone();
        *crate::harness::pool::AFTER_GONE_HOOK
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(std::sync::Arc::new(move |_| {
            write_persist(
                &rec_hook,
                &persist_body(
                    &rec_hook,
                    Some("sess-successor"),
                    Some(&epoch_hook),
                    true,
                    Some("127.0.0.1:9"),
                    Some(99),
                    None,
                ),
            );
        }));
        let root = worktrees_root(&rec).unwrap();
        let err = shutdown_under(&rec, &root, Some(&target))
            .unwrap_err()
            .to_string();
        *crate::harness::pool::AFTER_GONE_HOOK
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        assert!(err.contains("holder-changed"), "{err}");
        assert!(epoch.is_dir(), "the tree stays");
        assert!(path.exists(), "the successor persist stays");
        assert!(
            crate::harness::abort::cancel_handle_for(&rec.id).is_some(),
            "the cancel registration stays"
        );
        crate::harness::abort::unregister_cancel_handle(&rec.id);
    }
}
