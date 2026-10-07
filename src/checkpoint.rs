//! Epoch checkpoint ref and operator restore (track 0067).
//!
//! The ref is `refs/coordinator/checkpoints/{project_id}/{epoch}`. Create runs
//! only from the fold → implement edge. Restore is operator-only. Reap helpers
//! do not take [`with_run_state_lock`].

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{CoordinatorError, Result};
use crate::registry::ProjectRecord;
use crate::state::{
    RunState, RunStatus, StatusView, load_run_state, save_run_state, with_run_state_lock,
};

const ZERO_OID: &str = "0000000000000000000000000000000000000000";
const DIRT_PATH_CAP: usize = 20;

pub(crate) fn ref_name(project_id: &str, epoch: u64) -> String {
    format!("refs/coordinator/checkpoints/{project_id}/{epoch}")
}

/// Create-only checkpoint at `HEAD`. An existing ref is left in place.
///
/// Does not take the run-state lock. On failure the caller keeps phase `fold`.
pub(crate) fn ensure_implement_ref(record: &ProjectRecord, state: &mut RunState) -> Result<()> {
    let cwd = require_cwd(record)?;
    let sha = git_ok(&cwd, &["rev-parse", "HEAD"])?;
    let sha = sha.trim();
    if sha.is_empty() {
        return Err(CoordinatorError::Message(
            "checkpoint: git rev-parse HEAD: empty".into(),
        ));
    }
    let dirt = committable_dirt(&cwd)?;
    if !dirt.is_empty() {
        return Err(CoordinatorError::Message(dirt_message(
            "checkpoint refused: dirty tree",
            &dirt,
        )));
    }
    let name = ref_name(&record.id, state.run_epoch);
    let fmt = git(&cwd, &["check-ref-format", &name])?;
    if !fmt.ok {
        return Err(CoordinatorError::Message(format!(
            "checkpoint: git check-ref-format {name}: {}",
            fmt.stderr.trim()
        )));
    }
    let created = git(&cwd, &["update-ref", &name, sha, ZERO_OID])?;
    if !created.ok {
        if ref_exists(&cwd, &name) {
            return Ok(());
        }
        return Err(CoordinatorError::Message(format!(
            "checkpoint: git update-ref {name}: {}",
            created.stderr.trim()
        )));
    }
    state.checkpoint_branch = short_head(&cwd);
    Ok(())
}

/// Operator restore. Takes the run-state lock. Does not change `run_epoch`.
pub(crate) fn restore(record: &ProjectRecord, discard: bool) -> Result<StatusView> {
    with_run_state_lock(record, || restore_locked(record, discard))
}

fn restore_locked(record: &ProjectRecord, discard: bool) -> Result<StatusView> {
    let mut state = load_run_state(record)?;
    if matches!(state.status, RunStatus::Running | RunStatus::Paused) {
        return refuse(record, "restore refused: stop first");
    }
    if let Some(msg) = persist_blocks(record)? {
        return refuse(record, &msg);
    }
    let cwd = match require_cwd(record) {
        Ok(cwd) => cwd,
        Err(e) => return refuse(record, &e.to_string()),
    };
    let name = ref_name(&record.id, state.run_epoch);
    let sha = match git(&cwd, &["rev-parse", "--verify", "--quiet", &name]) {
        Ok(out) if out.ok && !out.stdout.trim().is_empty() => out.stdout.trim().to_string(),
        Ok(_) => return refuse(record, "restore refused: no checkpoint for this epoch"),
        Err(e) => return refuse(record, &e.to_string()),
    };
    if !discard {
        match committable_dirt(&cwd) {
            Ok(paths) if paths.is_empty() => {}
            Ok(paths) => {
                return refuse(record, &dirt_message("restore refused: dirty tree", &paths));
            }
            Err(e) => return refuse(record, &e.to_string()),
        }
    }
    if let Err(e) = restore_tree(&cwd, state.checkpoint_branch.as_deref(), &sha) {
        return refuse(record, &e.to_string());
    }
    let detail = format!("{name} discard={discard}");
    crate::progress_log::append(record, "restore", &detail);
    state.restored_epoch = Some(state.run_epoch);
    state.last_event = format!("restore: {detail}");
    state.updated_at = chrono::Utc::now();
    save_run_state(record, &state)?;
    Ok(StatusView::from_record(record, &state))
}

fn restore_tree(cwd: &Path, branch: Option<&str>, sha: &str) -> Result<()> {
    if let Some(branch) = branch.map(str::trim).filter(|s| !s.is_empty()) {
        if short_head(cwd).as_deref() != Some(branch) {
            git_ok(cwd, &["checkout", branch])?;
        }
        git_ok(cwd, &["reset", "--hard", sha])?;
    } else {
        git_ok(cwd, &["checkout", "--detach", sha])?;
    }
    git_ok(cwd, &["clean", "-fd"])?;
    Ok(())
}

fn refuse(record: &ProjectRecord, message: &str) -> Result<StatusView> {
    crate::progress_log::append(record, "restore-refused", message);
    Err(CoordinatorError::Message(message.to_string()))
}

fn persist_blocks(record: &ProjectRecord) -> Result<Option<String>> {
    let path = crate::harness::persist_path(record)?;
    if !path.exists() {
        return Ok(None);
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => {
            return Ok(Some(format!(
                "restore refused: harness persist unreadable: {e}"
            )));
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(e) => {
            return Ok(Some(format!(
                "restore refused: harness persist unreadable: {e}"
            )));
        }
    };
    let alive = value
        .get("alive")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let in_flight = value
        .get("prompt_in_flight")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if alive || in_flight {
        Ok(Some("restore refused: harness grok shutdown first".into()))
    } else {
        Ok(None)
    }
}

/// Delete every checkpoint ref for this project. A failed delete is journalled.
pub(crate) fn reap_completed(record: &ProjectRecord) -> Result<()> {
    reap(record, |_| true)
}

/// Delete checkpoint epochs strictly older than `next_epoch - 1`.
///
/// Epochs that do not parse are kept. Commit timestamps are not read.
pub(crate) fn reap_abandoned(record: &ProjectRecord, next_epoch: u64) -> Result<()> {
    let keep_from = next_epoch.saturating_sub(1);
    reap(record, |epoch| epoch.is_some_and(|n| n < keep_from))
}

fn reap(record: &ProjectRecord, drop_epoch: impl Fn(Option<u64>) -> bool) -> Result<()> {
    let Some(cwd) = git_cwd(record) else {
        return Ok(());
    };
    let refs = match list_refs(&cwd, &record.id) {
        Ok(refs) => refs,
        Err(e) => {
            crate::progress_log::append(record, "checkpoint-reap", &e.to_string());
            return Ok(());
        }
    };
    let prefix = format!("refs/coordinator/checkpoints/{}/", record.id);
    for name in refs {
        if !name.starts_with(&prefix) {
            continue;
        }
        let epoch = name.rsplit('/').next().and_then(|s| s.parse().ok());
        if drop_epoch(epoch) {
            delete_ref(record, &cwd, &name);
        }
    }
    Ok(())
}

fn list_refs(cwd: &Path, project_id: &str) -> Result<Vec<String>> {
    let pattern = format!("refs/coordinator/checkpoints/{project_id}");
    let out = git(cwd, &["for-each-ref", "--format=%(refname)", &pattern])?;
    if !out.ok {
        return Err(CoordinatorError::Message(format!(
            "checkpoint: git for-each-ref {pattern}: {}",
            out.stderr.trim()
        )));
    }
    Ok(out
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

fn delete_ref(record: &ProjectRecord, cwd: &Path, name: &str) {
    match git(cwd, &["update-ref", "-d", name]) {
        Ok(out) if out.ok => {}
        Ok(out) => crate::progress_log::append(
            record,
            "checkpoint-reap",
            &format!(
                "checkpoint: git update-ref -d {name}: {}",
                out.stderr.trim()
            ),
        ),
        Err(e) => crate::progress_log::append(record, "checkpoint-reap", &e.to_string()),
    }
}

fn require_cwd(record: &ProjectRecord) -> Result<PathBuf> {
    match git_cwd(record) {
        Some(cwd) => Ok(cwd),
        None => Err(CoordinatorError::Message(
            "checkpoint: no execution repo".into(),
        )),
    }
}

fn git_cwd(record: &ProjectRecord) -> Option<PathBuf> {
    let cwd = crate::worktree::product_git_cwd(record)?;
    if cwd.is_dir() { Some(cwd) } else { None }
}

fn short_head(cwd: &Path) -> Option<String> {
    let out = git(cwd, &["symbolic-ref", "--short", "HEAD"]).ok()?;
    if !out.ok {
        return None;
    }
    let branch = out.stdout.trim();
    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch.to_string())
    }
}

fn ref_exists(cwd: &Path, name: &str) -> bool {
    git(cwd, &["rev-parse", "--verify", "--quiet", name])
        .ok()
        .is_some_and(|out| out.ok && !out.stdout.trim().is_empty())
}

struct GitOut {
    ok: bool,
    stdout: String,
    stderr: String,
}

fn git(cwd: &Path, args: &[&str]) -> Result<GitOut> {
    let shown = args.join(" ");
    let out = Command::new(Path::new("git"))
        .args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .map_err(|e| CoordinatorError::Message(format!("checkpoint: git {shown}: {e}")))?;
    Ok(GitOut {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

fn git_ok(cwd: &Path, args: &[&str]) -> Result<String> {
    let shown = args.join(" ");
    let out = git(cwd, args)?;
    if !out.ok {
        return Err(CoordinatorError::Message(format!(
            "checkpoint: git {shown}: {}",
            out.stderr.trim()
        )));
    }
    Ok(out.stdout)
}

/// Paths a plain `git commit` would record, plus non-ignored untracked files.
///
/// `git diff HEAD` is the worktree. `git diff --cached HEAD` is the index.
/// A line-ending phantom is absent from both, so it is not committable.
fn committable_dirt(cwd: &Path) -> Result<Vec<String>> {
    let worktree = git_ok(cwd, &["diff", "-z", "--name-only", "HEAD"])?;
    let index = git_ok(cwd, &["diff", "-z", "--cached", "--name-only", "HEAD"])?;
    let untracked = git_ok(cwd, &["ls-files", "-z", "--others", "--exclude-standard"])?;
    let mut paths = Vec::new();
    push_nul_paths(&mut paths, &worktree);
    push_nul_paths(&mut paths, &index);
    push_nul_paths(&mut paths, &untracked);
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn push_nul_paths(paths: &mut Vec<String>, stdout: &str) {
    paths.extend(
        stdout
            .split('\0')
            .filter(|field| !field.is_empty())
            .map(str::to_string),
    );
}

fn dirt_message(prefix: &str, paths: &[String]) -> String {
    let shown = if paths.len() <= DIRT_PATH_CAP {
        paths.join(", ")
    } else {
        let extra = paths.len() - DIRT_PATH_CAP;
        format!("{} (+{extra})", paths[..DIRT_PATH_CAP].join(", "))
    };
    format!("{prefix}: {shown}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::CoordinatorError;
    use crate::outcome::{OutcomeSource, PhaseOutcome, write_and_apply};
    use crate::run;
    use crate::state::{RunState, RunStatus, ensure_state_dir, load_run_state, save_run_state};
    use crate::workflow::WorkflowDriver;
    use crate::workflow::graph::{
        PHASE_ADDRESS_FINDINGS, PHASE_CROSS_MODEL, PHASE_FOLD, PHASE_PLAN,
    };
    use chrono::Utc;
    use std::path::Path;
    use std::process::Command;
    use tempfile::TempDir;
    use uuid::Uuid;

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
            created_at: Utc::now(),
        }
    }

    fn git_cmd(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_stdout(cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn init_repo() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        git_cmd(dir.path(), &["init", "-b", "main"]);
        git_cmd(
            dir.path(),
            &["config", "user.email", "checkpoint-test@example.com"],
        );
        git_cmd(dir.path(), &["config", "user.name", "checkpoint-test"]);
        std::fs::write(
            dir.path().join(".gitignore"),
            ".ledgerful/\n.env\n.coordinator/\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("README.md"), b"seed\n").unwrap();
        git_cmd(dir.path(), &["add", ".gitignore", "README.md"]);
        git_cmd(dir.path(), &["commit", "-m", "seed"]);
        dir
    }

    fn fixture(on: bool) -> (TempDir, TempDir, TempDir, ProjectRecord) {
        let repo = init_repo();
        let ws = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let rec = record(
            ws.path().to_path_buf(),
            Some(repo.path().to_path_buf()),
            Some(state.path().to_path_buf()),
            on,
        );
        write_row(ws.path(), "**Ready — not started**");
        (repo, ws, state, rec)
    }

    fn save_phase(rec: &ProjectRecord, status: RunStatus, phase: &str, epoch: u64) {
        ensure_state_dir(rec).unwrap();
        let mut state = RunState::idle(&rec.id);
        state.status = status;
        state.phase = phase.into();
        state.run_epoch = epoch;
        state.workflow = Some(crate::workflow::WORKFLOW_ID.into());
        state.driver = WorkflowDriver::FileWait;
        state.track_id = Some("0001".into());
        save_run_state(rec, &state).unwrap();
    }

    /// Registry lives on the workspace temp dir so the git execution repo stays clean.
    fn write_row(ws: &std::path::Path, status: &str) {
        let cond = ws.join("conductor");
        std::fs::create_dir_all(&cond).unwrap();
        let md = format!(
            "| Track | Status | Summary |\n\
             | --- | --- | --- |\n\
             | [0001-One](0001-One/spec.md) | {status} | ok |\n"
        );
        std::fs::write(cond.join("conductor.md"), md).unwrap();
    }

    fn head(cwd: &Path) -> String {
        git_stdout(cwd, &["rev-parse", "HEAD"])
    }

    fn porcelain(cwd: &Path) -> String {
        git_stdout(cwd, &["status", "--porcelain"])
    }

    fn log_text(rec: &ProjectRecord) -> String {
        std::fs::read_to_string(crate::progress_log::path(rec)).unwrap_or_default()
    }

    /// The `checkpoint-refused` progress detail is `last_event` (`progress_log` writes
    /// `- {ts}  {kind}  {detail}`).
    fn assert_checkpoint_refused_detail(rec: &ProjectRecord, last_event: &str) {
        let log = log_text(rec);
        let marker = "checkpoint-refused  ";
        let detail = log
            .lines()
            .find_map(|line| line.find(marker).map(|index| &line[index + marker.len()..]));
        assert_eq!(detail, Some(last_event), "{log}");
    }

    fn untracked_names(cwd: &Path) -> Vec<String> {
        let mut names: Vec<String> =
            git_stdout(cwd, &["ls-files", "-z", "--others", "--exclude-standard"])
                .split('\0')
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .collect();
        names.sort();
        names
    }

    fn fold_success(rec: &ProjectRecord, epoch: u64) -> StatusView {
        write_and_apply(
            rec,
            PhaseOutcome::success(PHASE_FOLD, OutcomeSource::Test, None, None, Some(epoch)),
        )
        .unwrap()
    }

    #[test]
    fn old_run_state_omits_checkpoint_branch() {
        let state: RunState = serde_json::from_str(
            r#"{"project_id":"p","status":"Idle","phase":"stub:idle","updated_at":"2026-01-01T00:00:00Z","last_event":"x"}"#,
        )
        .unwrap();
        assert!(state.checkpoint_branch.is_none());
        let back = serde_json::to_string(&state).unwrap();
        assert!(!back.contains("checkpoint_branch"));
    }

    #[test]
    fn fold_proposed_stops_before_implement_ref() {
        let (repo, ws, _state, rec) = fixture(false);
        write_row(
            ws.path(),
            "**Proposed — placeholder, needs full spec/plan pass**",
        );
        save_phase(&rec, RunStatus::Running, PHASE_FOLD, 4);
        let view = fold_success(&rec, 4);
        assert_eq!(view.status, RunStatus::Stopped, "{}", view.last_event);
        assert_eq!(view.phase, PHASE_FOLD);
        assert!(view.failure_class.is_none());
        assert!(
            view.last_event
                .starts_with("workflow: fold withheld (row status: Proposed - placeholder"),
            "{}",
            view.last_event
        );
        assert!(log_text(&rec).contains("fold-withheld"));
        assert!(crate::notify::artifact::existing_path(&rec).is_none());
        let listed = git(
            repo.path(),
            &["rev-parse", "--verify", "--quiet", &ref_name(&rec.id, 4)],
        )
        .unwrap();
        assert!(!listed.ok);
    }

    #[test]
    fn fold_allow_rows_create_ref_and_implement() {
        for status in [
            "**Ready — not started**",
            "**In progress**",
            "**Ready — folded @ sha1234**",
        ] {
            let (repo, ws, _state, rec) = fixture(false);
            write_row(ws.path(), status);
            save_phase(&rec, RunStatus::Running, PHASE_FOLD, 4);
            let view = fold_success(&rec, 4);
            assert_eq!(
                view.phase,
                crate::workflow::graph::PHASE_IMPLEMENT,
                "{status} {}",
                view.last_event
            );
            assert_eq!(view.status, RunStatus::Running);
            assert!(view.failure_class.is_none());
            assert_eq!(
                git_stdout(repo.path(), &["rev-parse", &ref_name(&rec.id, 4)]),
                head(repo.path())
            );
        }
    }

    #[test]
    fn fold_proposed_dirty_reports_withhold_not_checkpoint() {
        let (repo, ws, _state, rec) = fixture(false);
        write_row(
            ws.path(),
            "**Proposed — placeholder, needs full spec/plan pass**",
        );
        save_phase(&rec, RunStatus::Running, PHASE_FOLD, 2);
        std::fs::write(repo.path().join("dirty.rs"), b"x\n").unwrap();
        let view = fold_success(&rec, 2);
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.phase, PHASE_FOLD);
        assert!(view.failure_class.is_none());
        assert!(
            view.last_event.starts_with("workflow: fold withheld"),
            "{}",
            view.last_event
        );
        assert!(
            !view.last_event.contains("checkpoint refused"),
            "{}",
            view.last_event
        );
        assert!(repo.path().join("dirty.rs").is_file());
    }

    #[test]
    fn fold_missing_conductor_stops_unreadable() {
        let (repo, ws, _state, rec) = fixture(false);
        std::fs::remove_file(ws.path().join("conductor").join("conductor.md")).unwrap();
        save_phase(&rec, RunStatus::Running, PHASE_FOLD, 1);
        let view = fold_success(&rec, 1);
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.phase, PHASE_FOLD);
        assert!(view.failure_class.is_none());
        assert_eq!(
            view.last_event,
            "workflow: fold withheld (cannot read track row)"
        );
        assert!(log_text(&rec).contains("fold-withheld"));
        assert!(crate::notify::artifact::existing_path(&rec).is_none());
        let listed = git(
            repo.path(),
            &["rev-parse", "--verify", "--quiet", &ref_name(&rec.id, 1)],
        )
        .unwrap();
        assert!(!listed.ok);
    }

    #[test]
    fn clean_fold_creates_ref_then_implement() {
        let (repo, _ws, _state, rec) = fixture(false);
        let before = head(repo.path());
        save_phase(&rec, RunStatus::Running, PHASE_FOLD, 4);
        let view = fold_success(&rec, 4);
        assert_eq!(view.phase, crate::workflow::graph::PHASE_IMPLEMENT);
        assert_eq!(view.status, RunStatus::Running);
        assert!(view.failure_class.is_none());
        let name = ref_name(&rec.id, 4);
        assert_eq!(git_stdout(repo.path(), &["rev-parse", &name]), before);
        let saved = load_run_state(&rec).unwrap();
        assert_eq!(saved.checkpoint_branch.as_deref(), Some("main"));
        assert!(crate::notify::artifact::existing_path(&rec).is_none());
    }

    #[test]
    fn second_ensure_does_not_move_ref_or_branch() {
        let (repo, _ws, _state, rec) = fixture(false);
        save_phase(&rec, RunStatus::Running, PHASE_FOLD, 4);
        fold_success(&rec, 4);
        let pinned = git_stdout(repo.path(), &["rev-parse", &ref_name(&rec.id, 4)]);
        std::fs::write(repo.path().join("README.md"), b"later\n").unwrap();
        git_cmd(repo.path(), &["add", "README.md"]);
        git_cmd(repo.path(), &["commit", "-m", "later"]);
        assert_ne!(head(repo.path()), pinned);
        let mut state = load_run_state(&rec).unwrap();
        let branch = state.checkpoint_branch.clone();
        ensure_implement_ref(&rec, &mut state).unwrap();
        assert_eq!(state.checkpoint_branch, branch);
        assert_eq!(
            git_stdout(repo.path(), &["rev-parse", &ref_name(&rec.id, 4)]),
            pinned
        );
        save_run_state(&rec, &state).unwrap();
    }

    #[test]
    fn dirty_fold_stays_fold_without_ref_or_failure_artifact() {
        let (repo, _ws, _state, rec) = fixture(false);
        save_phase(&rec, RunStatus::Running, PHASE_FOLD, 2);
        std::fs::write(repo.path().join("dirty.rs"), b"x\n").unwrap();
        let view = fold_success(&rec, 2);
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.phase, PHASE_FOLD);
        assert!(view.failure_class.is_none());
        assert!(view.last_event.contains("checkpoint refused: dirty tree"));
        assert!(view.last_event.contains("dirty.rs"), "{}", view.last_event);
        let listed = git(
            repo.path(),
            &["rev-parse", "--verify", "--quiet", &ref_name(&rec.id, 2)],
        )
        .unwrap();
        assert!(!listed.ok);
        assert!(crate::notify::artifact::existing_path(&rec).is_none());
        assert_checkpoint_refused_detail(&rec, &view.last_event);
        assert!(repo.path().join("dirty.rs").is_file());
    }

    #[test]
    fn missing_repo_refuses_fold() {
        let ws = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let rec = record(
            ws.path().to_path_buf(),
            None,
            Some(state.path().to_path_buf()),
            false,
        );
        write_row(ws.path(), "**Ready — not started**");
        save_phase(&rec, RunStatus::Running, PHASE_FOLD, 1);
        let view = fold_success(&rec, 1);
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.phase, PHASE_FOLD);
        assert!(view.failure_class.is_none());
        assert!(view.last_event.contains("checkpoint: no execution repo"));
        assert!(crate::notify::artifact::existing_path(&rec).is_none());
    }

    #[test]
    fn address_findings_and_implement_tick_do_not_move_ref() {
        let (repo, _ws, _state, rec) = fixture(false);
        save_phase(&rec, RunStatus::Running, PHASE_FOLD, 5);
        fold_success(&rec, 5);
        let pinned = git_stdout(repo.path(), &["rev-parse", &ref_name(&rec.id, 5)]);
        std::fs::write(repo.path().join("more.rs"), b"m\n").unwrap();
        git_cmd(repo.path(), &["add", "more.rs"]);
        git_cmd(repo.path(), &["commit", "-m", "more"]);
        let mut state = load_run_state(&rec).unwrap();
        state.phase = PHASE_ADDRESS_FINDINGS.into();
        save_run_state(&rec, &state).unwrap();
        let view = write_and_apply(
            &rec,
            PhaseOutcome::success(
                PHASE_ADDRESS_FINDINGS,
                OutcomeSource::Test,
                None,
                None,
                Some(5),
            ),
        )
        .unwrap();
        assert_eq!(view.phase, PHASE_CROSS_MODEL);
        assert_eq!(
            git_stdout(repo.path(), &["rev-parse", &ref_name(&rec.id, 5)]),
            pinned
        );
        let mut state = load_run_state(&rec).unwrap();
        state.phase = crate::workflow::graph::PHASE_IMPLEMENT.into();
        state.status = RunStatus::Running;
        state.driver = WorkflowDriver::Stub;
        save_run_state(&rec, &state).unwrap();
        let ticked = crate::workflow::drive::tick(&rec).unwrap().unwrap();
        assert_eq!(ticked.phase, PHASE_CROSS_MODEL);
        assert_eq!(
            git_stdout(repo.path(), &["rev-parse", &ref_name(&rec.id, 5)]),
            pinned
        );
    }

    #[test]
    fn running_and_paused_refuse_without_git_write() {
        let (repo, _ws, _state, rec) = fixture(false);
        save_phase(&rec, RunStatus::Running, PHASE_FOLD, 1);
        fold_success(&rec, 1);
        std::fs::write(repo.path().join("README.md"), b"moved\n").unwrap();
        git_cmd(repo.path(), &["add", "README.md"]);
        git_cmd(repo.path(), &["commit", "-m", "moved"]);
        let moved = head(repo.path());
        let err = restore(&rec, true).unwrap_err();
        assert!(err.to_string().contains("stop first"), "{err}");
        assert_eq!(head(repo.path()), moved);
        run::pause(&rec).unwrap();
        let err = restore(&rec, false).unwrap_err();
        assert!(err.to_string().contains("stop first"), "{err}");
        assert_eq!(head(repo.path()), moved);
        assert!(log_text(&rec).contains("restore-refused"));
    }

    #[test]
    fn persist_alive_or_in_flight_refuses() {
        let (repo, _ws, state_dir, rec) = fixture(false);
        save_phase(&rec, RunStatus::Running, PHASE_FOLD, 1);
        fold_success(&rec, 1);
        std::fs::write(repo.path().join("README.md"), b"moved\n").unwrap();
        git_cmd(repo.path(), &["add", "README.md"]);
        git_cmd(repo.path(), &["commit", "-m", "moved"]);
        let moved = head(repo.path());
        run::stop(&rec).unwrap();
        let persist = state_dir.path().join("harness-grok.json");
        std::fs::write(&persist, r#"{"alive":true,"prompt_in_flight":false}"#).unwrap();
        let err = restore(&rec, true).unwrap_err();
        assert!(err.to_string().contains("harness grok shutdown"), "{err}");
        assert_eq!(head(repo.path()), moved);
        std::fs::write(&persist, r#"{"alive":false,"prompt_in_flight":true}"#).unwrap();
        let err = restore(&rec, true).unwrap_err();
        assert!(err.to_string().contains("harness grok shutdown"), "{err}");
        assert_eq!(head(repo.path()), moved);
    }

    #[test]
    fn unreadable_persist_journals_and_does_not_write_git() {
        let (repo, _ws, state_dir, rec) = fixture(false);
        save_phase(&rec, RunStatus::Running, PHASE_FOLD, 1);
        fold_success(&rec, 1);
        std::fs::write(repo.path().join("README.md"), b"moved\n").unwrap();
        git_cmd(repo.path(), &["add", "README.md"]);
        git_cmd(repo.path(), &["commit", "-m", "moved"]);
        let moved = head(repo.path());
        run::stop(&rec).unwrap();
        std::fs::write(state_dir.path().join("harness-grok.json"), b"not-json").unwrap();
        let err = restore(&rec, true).unwrap_err();
        assert!(err.to_string().contains("unreadable"), "{err}");
        assert_eq!(head(repo.path()), moved);
        let log = log_text(&rec);
        assert!(log.contains("restore-refused"), "{log}");
        assert!(log.contains("unreadable"), "{log}");
    }

    #[test]
    fn dirty_restore_refuses_and_keeps_untracked() {
        let (repo, _ws, _state, rec) = fixture(false);
        save_phase(&rec, RunStatus::Running, PHASE_FOLD, 3);
        fold_success(&rec, 3);
        run::stop(&rec).unwrap();
        std::fs::write(repo.path().join("keep-me.rs"), b"stay\n").unwrap();
        let err = restore(&rec, false).unwrap_err();
        assert!(err.to_string().contains("dirty tree"), "{err}");
        assert!(err.to_string().contains("keep-me.rs"), "{err}");
        assert!(repo.path().join("keep-me.rs").is_file());
        assert!(log_text(&rec).contains("restore-refused"));
        assert!(log_text(&rec).contains("dirty tree"));
    }

    #[test]
    fn autocrlf_phantom_fold_creates_ref() {
        let (repo, _ws, _state, rec) = fixture(false);
        git_cmd(repo.path(), &["config", "core.autocrlf", "true"]);
        git_cmd(repo.path(), &["config", "core.safecrlf", "false"]);
        std::fs::write(repo.path().join("README.md"), b"seed\r\n").unwrap();
        let porcelain = porcelain(repo.path());
        assert!(
            !porcelain.is_empty(),
            "porcelain must be dirty so the old guard would refuse: {porcelain:?}"
        );
        assert!(
            git_stdout(repo.path(), &["diff", "--name-only", "HEAD"]).is_empty(),
            "worktree diff"
        );
        assert!(
            git_stdout(repo.path(), &["diff", "--cached", "--name-only", "HEAD"]).is_empty(),
            "index diff"
        );
        let before = head(repo.path());
        save_phase(&rec, RunStatus::Running, PHASE_FOLD, 4);
        let view = fold_success(&rec, 4);
        assert_eq!(
            view.phase,
            crate::workflow::graph::PHASE_IMPLEMENT,
            "{}",
            view.last_event
        );
        assert_eq!(view.status, RunStatus::Running);
        assert!(view.failure_class.is_none());
        assert_eq!(
            git_stdout(repo.path(), &["rev-parse", &ref_name(&rec.id, 4)]),
            before
        );
    }

    #[test]
    fn tracked_content_change_refuses_fold() {
        let (repo, _ws, _state, rec) = fixture(false);
        save_phase(&rec, RunStatus::Running, PHASE_FOLD, 2);
        std::fs::write(repo.path().join("README.md"), b"seed2\n").unwrap();
        let view = fold_success(&rec, 2);
        assert_refused_fold(&rec, &repo, &view, "README.md");
    }

    #[test]
    fn staged_content_change_refuses_fold() {
        let (repo, _ws, _state, rec) = fixture(false);
        save_phase(&rec, RunStatus::Running, PHASE_FOLD, 2);
        std::fs::write(repo.path().join("README.md"), b"seed2\n").unwrap();
        git_cmd(repo.path(), &["add", "README.md"]);
        assert!(
            !git_stdout(repo.path(), &["diff", "--name-only", "HEAD"]).is_empty(),
            "staged worktree must differ from HEAD"
        );
        let view = fold_success(&rec, 2);
        assert_refused_fold(&rec, &repo, &view, "README.md");
    }

    #[test]
    fn staged_index_only_change_refuses_fold() {
        let (repo, _ws, _state, rec) = fixture(false);
        save_phase(&rec, RunStatus::Running, PHASE_FOLD, 2);
        std::fs::write(repo.path().join("staged-only.rs"), b"fn x() {}\n").unwrap();
        git_cmd(repo.path(), &["add", "staged-only.rs"]);
        std::fs::remove_file(repo.path().join("staged-only.rs")).unwrap();
        assert!(
            git_stdout(repo.path(), &["diff", "--name-only", "HEAD"]).is_empty(),
            "worktree diff must be empty"
        );
        assert_eq!(
            git_stdout(repo.path(), &["diff", "--cached", "--name-only", "HEAD"]),
            "staged-only.rs"
        );
        let view = fold_success(&rec, 2);
        assert_refused_fold(&rec, &repo, &view, "staged-only.rs");
    }

    #[test]
    fn ignored_env_file_does_not_refuse_fold() {
        let (repo, _ws, _state, rec) = fixture(false);
        std::fs::write(repo.path().join(".env"), b"SECRET=1\n").unwrap();
        let before = head(repo.path());
        save_phase(&rec, RunStatus::Running, PHASE_FOLD, 4);
        let view = fold_success(&rec, 4);
        assert_eq!(
            view.phase,
            crate::workflow::graph::PHASE_IMPLEMENT,
            "{}",
            view.last_event
        );
        assert_eq!(view.status, RunStatus::Running);
        assert!(view.failure_class.is_none());
        assert_eq!(
            git_stdout(repo.path(), &["rev-parse", &ref_name(&rec.id, 4)]),
            before
        );
    }

    #[test]
    fn autocrlf_phantom_restore_without_discard_succeeds() {
        let (repo, _ws, _state, rec) = fixture(false);
        save_phase(&rec, RunStatus::Running, PHASE_FOLD, 3);
        fold_success(&rec, 3);
        run::stop(&rec).unwrap();
        git_cmd(repo.path(), &["config", "core.autocrlf", "true"]);
        git_cmd(repo.path(), &["config", "core.safecrlf", "false"]);
        std::fs::write(repo.path().join("README.md"), b"seed\r\n").unwrap();
        let view = restore(&rec, false).unwrap();
        assert!(
            !view.last_event.contains("dirty tree"),
            "{}",
            view.last_event
        );
        assert_eq!(
            std::fs::read(repo.path().join("README.md")).unwrap(),
            b"seed\r\n"
        );
    }

    #[test]
    fn dirty_path_list_truncation_caps_at_twenty() {
        let names25: Vec<String> = (0..25).map(|i| format!("p{i:02}")).collect();
        let (repo, _ws, _state, rec) = fixture(false);
        save_phase(&rec, RunStatus::Running, PHASE_FOLD, 2);
        for name in &names25 {
            std::fs::write(repo.path().join(name), b"x\n").unwrap();
        }
        assert_eq!(untracked_names(repo.path()), names25);
        let view = fold_success(&rec, 2);
        let shown = names25[..20].join(", ");
        assert_eq!(
            view.last_event,
            format!("checkpoint refused: dirty tree: {shown} (+5)")
        );
        assert!(!view.last_event.contains("p20"), "{}", view.last_event);
        assert!(!view.last_event.contains(", (+"), "{}", view.last_event);
        assert_refused_fold(&rec, &repo, &view, "p00");

        let names20: Vec<String> = (0..20).map(|i| format!("p{i:02}")).collect();
        let (repo, _ws, _state, rec) = fixture(false);
        save_phase(&rec, RunStatus::Running, PHASE_FOLD, 2);
        for name in &names20 {
            std::fs::write(repo.path().join(name), b"x\n").unwrap();
        }
        assert_eq!(untracked_names(repo.path()), names20);
        let view = fold_success(&rec, 2);
        let shown = names20.join(", ");
        assert_eq!(
            view.last_event,
            format!("checkpoint refused: dirty tree: {shown}")
        );
        assert!(!view.last_event.contains(" (+"), "{}", view.last_event);
        assert_refused_fold(&rec, &repo, &view, "p19");
    }

    fn assert_refused_fold(rec: &ProjectRecord, repo: &TempDir, view: &StatusView, path: &str) {
        assert_eq!(view.status, RunStatus::Stopped, "{}", view.last_event);
        assert_eq!(view.phase, PHASE_FOLD);
        assert!(view.failure_class.is_none());
        assert!(
            view.last_event.contains("checkpoint refused: dirty tree"),
            "{}",
            view.last_event
        );
        assert!(view.last_event.contains(path), "{}", view.last_event);
        let listed = git(
            repo.path(),
            &["rev-parse", "--verify", "--quiet", &ref_name(&rec.id, 2)],
        )
        .unwrap();
        assert!(!listed.ok);
        assert!(crate::notify::artifact::existing_path(rec).is_none());
        assert_checkpoint_refused_detail(rec, &view.last_event);
    }

    #[test]
    fn discard_resets_recorded_branch_and_keeps_ignored() {
        let (repo, _ws, _state, rec) = fixture(false);
        save_phase(&rec, RunStatus::Running, PHASE_FOLD, 3);
        let view = fold_success(&rec, 3);
        let pinned = git_stdout(repo.path(), &["rev-parse", &ref_name(&rec.id, 3)]);
        assert_eq!(view.phase, crate::workflow::graph::PHASE_IMPLEMENT);
        std::fs::write(repo.path().join("README.md"), b"dirty-tracked\n").unwrap();
        git_cmd(repo.path(), &["add", "README.md"]);
        git_cmd(repo.path(), &["commit", "-m", "past"]);
        std::fs::create_dir_all(repo.path().join("scratch")).unwrap();
        std::fs::write(repo.path().join("scratch").join("extra.rs"), b"x\n").unwrap();
        std::fs::create_dir_all(repo.path().join(".ledgerful")).unwrap();
        std::fs::write(repo.path().join(".ledgerful").join("keep.txt"), b"ledger\n").unwrap();
        std::fs::write(repo.path().join(".env"), b"SECRET=1\n").unwrap();
        run::stop(&rec).unwrap();
        let view = restore(&rec, true).unwrap();
        assert_eq!(view.run_epoch, 3);
        assert_eq!(head(repo.path()), pinned);
        assert_eq!(
            git_stdout(repo.path(), &["symbolic-ref", "--short", "HEAD"]),
            "main"
        );
        assert!(porcelain(repo.path()).is_empty());
        assert!(!repo.path().join("scratch").exists());
        assert_eq!(
            std::fs::read_to_string(repo.path().join(".ledgerful").join("keep.txt")).unwrap(),
            "ledger\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join(".env")).unwrap(),
            "SECRET=1\n"
        );
        assert!(log_text(&rec).contains("restore"));
        assert!(log_text(&rec).contains("discard=true"));
        assert!(!log_text(&rec).contains("git clean -fdx"));
    }

    #[test]
    fn restore_checks_out_recorded_branch_without_detaching() {
        let (repo, _ws, _state, rec) = fixture(false);
        save_phase(&rec, RunStatus::Running, PHASE_FOLD, 6);
        fold_success(&rec, 6);
        let pinned = git_stdout(repo.path(), &["rev-parse", &ref_name(&rec.id, 6)]);
        git_cmd(repo.path(), &["checkout", "--detach", "HEAD"]);
        run::stop(&rec).unwrap();
        let view = restore(&rec, false).unwrap();
        assert_eq!(view.run_epoch, 6);
        assert_eq!(head(repo.path()), pinned);
        assert_eq!(
            git_stdout(repo.path(), &["symbolic-ref", "--short", "HEAD"]),
            "main"
        );
        assert!(porcelain(repo.path()).is_empty());
        assert_eq!(load_run_state(&rec).unwrap().restored_epoch, Some(6));
    }

    #[test]
    fn missing_ref_refuses() {
        let (repo, _ws, _state, rec) = fixture(false);
        save_phase(&rec, RunStatus::Stopped, PHASE_FOLD, 9);
        let before = head(repo.path());
        let err = restore(&rec, true).unwrap_err();
        assert!(
            err.to_string().contains("no checkpoint for this epoch"),
            "{err}"
        );
        assert_eq!(head(repo.path()), before);
        assert!(load_run_state(&rec).unwrap().restored_epoch.is_none());
    }

    #[test]
    fn detached_worktree_restore_leaves_shared_head() {
        let (repo, _ws, _state, rec) = fixture(true);
        crate::worktree::prepare_epoch(&rec, 1).unwrap();
        save_phase(&rec, RunStatus::Running, PHASE_FOLD, 1);
        let shared = head(repo.path());
        fold_success(&rec, 1);
        let epoch = crate::worktree::epoch_dir(&rec, 1).unwrap();
        assert_eq!(load_run_state(&rec).unwrap().checkpoint_branch, None);
        std::fs::write(epoch.join("local.txt"), b"local\n").unwrap();
        git_cmd(&epoch, &["add", "local.txt"]);
        git_cmd(&epoch, &["commit", "-m", "worktree only"]);
        assert_eq!(head(repo.path()), shared);
        assert_ne!(head(&epoch), shared);
        let branches_before = git_stdout(repo.path(), &["branch", "--format=%(refname)"]);
        run::stop(&rec).unwrap();
        restore(&rec, true).unwrap();
        assert_eq!(head(&epoch), shared);
        assert_eq!(head(repo.path()), shared);
        assert!(!epoch.join("local.txt").exists());
        let sym = Command::new("git")
            .args(["symbolic-ref", "--short", "HEAD"])
            .current_dir(&epoch)
            .output()
            .unwrap();
        assert!(!sym.status.success(), "epoch HEAD must stay detached");
        assert_eq!(
            git_stdout(repo.path(), &["branch", "--format=%(refname)"]),
            branches_before
        );
    }

    #[test]
    fn completed_reap_leaves_no_refs_for_project() {
        let (repo, _ws, _state, rec) = fixture(false);
        let sha = head(repo.path());
        for epoch in [1_u64, 2, 3] {
            git_cmd(
                repo.path(),
                &["update-ref", &ref_name(&rec.id, epoch), &sha],
            );
        }
        reap_completed(&rec).unwrap();
        let listed = git_stdout(
            repo.path(),
            &[
                "for-each-ref",
                "--format=%(refname)",
                &format!("refs/coordinator/checkpoints/{}", rec.id),
            ],
        );
        assert!(listed.is_empty(), "{listed}");
    }

    #[test]
    fn advance_reaps_checkpoint_and_still_idles() {
        let (repo, ws, _state, mut rec) = fixture(false);
        save_phase(&rec, RunStatus::Running, PHASE_FOLD, 1);
        fold_success(&rec, 1);
        assert!(
            git(
                repo.path(),
                &["rev-parse", "--verify", "--quiet", &ref_name(&rec.id, 1)],
            )
            .unwrap()
            .ok
        );
        let mut state = load_run_state(&rec).unwrap();
        state.phase = crate::workflow::graph::PHASE_ADVANCE.into();
        save_run_state(&rec, &state).unwrap();
        rec.auto_start = crate::registry::AutoStartPolicy::Hitl;
        let view = write_and_apply(
            &rec,
            PhaseOutcome::success(
                crate::workflow::graph::PHASE_ADVANCE,
                OutcomeSource::Test,
                None,
                None,
                Some(1),
            ),
        )
        .unwrap();
        assert_eq!(view.status, RunStatus::Idle, "{}", view.last_event);
        let listed = git_stdout(
            repo.path(),
            &[
                "for-each-ref",
                "--format=%(refname)",
                &format!("refs/coordinator/checkpoints/{}", rec.id),
            ],
        );
        assert!(listed.is_empty(), "{listed}");
        assert!(log_text(&rec).contains("backlog clear") || ws.path().join("status.md").is_file());
    }

    #[test]
    fn abandoned_reap_keeps_previous_epoch_not_commit_date() {
        let (repo, _ws, _state, rec) = fixture(false);
        std::fs::write(repo.path().join("old.txt"), b"old\n").unwrap();
        git_cmd(repo.path(), &["add", "old.txt"]);
        let mut cmd = Command::new("git");
        cmd.args(["commit", "-m", "old"])
            .current_dir(repo.path())
            .env("GIT_AUTHOR_DATE", "2001-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2001-01-01T00:00:00Z")
            .env("GIT_OPTIONAL_LOCKS", "0");
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let old_sha = head(repo.path());
        let old_ct = git_stdout(repo.path(), &["log", "-1", "--format=%ct"]);
        assert!(old_ct.parse::<u64>().unwrap() < 1_100_000_000, "{old_ct}");
        std::fs::write(repo.path().join("new.txt"), b"new\n").unwrap();
        git_cmd(repo.path(), &["add", "new.txt"]);
        git_cmd(repo.path(), &["commit", "-m", "new"]);
        let new_sha = head(repo.path());
        git_cmd(
            repo.path(),
            &["update-ref", &ref_name(&rec.id, 1), &new_sha],
        );
        git_cmd(
            repo.path(),
            &["update-ref", &ref_name(&rec.id, 2), &new_sha],
        );
        git_cmd(
            repo.path(),
            &["update-ref", &ref_name(&rec.id, 3), &old_sha],
        );
        git_cmd(
            repo.path(),
            &["update-ref", &ref_name(&rec.id, 4), &old_sha],
        );
        reap_abandoned(&rec, 3).unwrap();
        let listed = git_stdout(
            repo.path(),
            &[
                "for-each-ref",
                "--format=%(refname)",
                &format!("refs/coordinator/checkpoints/{}", rec.id),
            ],
        );
        assert!(!listed.contains(&ref_name(&rec.id, 1)), "{listed}");
        for epoch in [2_u64, 3, 4] {
            assert!(
                listed.contains(&ref_name(&rec.id, epoch)),
                "{listed} missing {epoch}"
            );
        }
        assert_eq!(
            git_stdout(repo.path(), &["rev-parse", &ref_name(&rec.id, 4)]),
            old_sha
        );
    }

    #[test]
    fn flag_off_restore_then_run_replans_at_checkpoint() {
        let (repo, _ws, _state, rec) = fixture(false);
        save_phase(&rec, RunStatus::Running, PHASE_FOLD, 1);
        fold_success(&rec, 1);
        let pinned = git_stdout(repo.path(), &["rev-parse", &ref_name(&rec.id, 1)]);
        std::fs::write(repo.path().join("README.md"), b"past\n").unwrap();
        git_cmd(repo.path(), &["add", "README.md"]);
        git_cmd(repo.path(), &["commit", "-m", "past"]);
        run::stop(&rec).unwrap();
        restore(&rec, true).unwrap();
        let started = run::run(&rec, Some("0067".into())).unwrap();
        assert_eq!(started.run_epoch, 2);
        assert_eq!(started.phase, PHASE_PLAN);
        assert_eq!(started.status, RunStatus::Running);
        assert_eq!(head(repo.path()), pinned);
        assert!(porcelain(repo.path()).is_empty());
        let again = run::run(&rec, None).unwrap_err();
        assert!(
            matches!(
                again,
                CoordinatorError::InvalidTransition { action: "run", .. }
            ),
            "{again}"
        );
    }

    #[test]
    fn flag_on_next_epoch_worktree_matches_checkpoint_sha() {
        let (repo, _ws, _state, rec) = fixture(true);
        crate::worktree::prepare_epoch(&rec, 1).unwrap();
        save_phase(&rec, RunStatus::Running, PHASE_FOLD, 1);
        let shared = head(repo.path());
        fold_success(&rec, 1);
        let epoch = crate::worktree::epoch_dir(&rec, 1).unwrap();
        std::fs::write(epoch.join("only-here.txt"), b"x\n").unwrap();
        git_cmd(&epoch, &["add", "only-here.txt"]);
        git_cmd(&epoch, &["commit", "-m", "epoch only"]);
        assert_eq!(head(repo.path()), shared);
        run::stop(&rec).unwrap();
        restore(&rec, true).unwrap();
        assert_eq!(head(repo.path()), shared);
        let started = run::run(&rec, Some("0067".into())).unwrap();
        assert_eq!(started.run_epoch, 2);
        assert_eq!(started.phase, PHASE_PLAN);
        let next = crate::worktree::product_git_cwd(&rec).unwrap();
        assert_eq!(head(&next), shared);
        assert_eq!(head(repo.path()), shared);
        assert!(porcelain(&next).is_empty());
    }
}
