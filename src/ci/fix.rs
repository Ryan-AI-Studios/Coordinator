//! Opt-in route from a required-check failure into same-epoch `address-ci` (0071).

use std::path::Path;
use std::process::Command;

use crate::error::Result;
use crate::notify::artifact::numeric_track_id;
use crate::registry::ProjectRecord;
use crate::state::{
    CiFixCheck, CiFixRequest, RunState, StatusView, load_run_state, save_run_state,
    with_run_state_lock,
};
use crate::workflow::graph::{CI_FIX_CAP, PHASE_ADDRESS_CI};
use crate::workflow::shipped::pr_title_is_track;

use super::backend::{CheckBucket, CheckItem, CheckSnapshot, CheckView, CiTarget};
use super::collapse_snapshot;
use super::gh::branch_is_track;

/// `COORDINATOR_CI_FIX=off` forces the route off. No other value turns it on.
pub const ENV_CI_FIX: &str = "COORDINATOR_CI_FIX";

/// Record flag on, and the env kill switch is not `off`.
pub fn enabled(record: &ProjectRecord) -> bool {
    if !record.ci_fix_routing {
        return false;
    }
    !matches!(
        std::env::var(ENV_CI_FIX),
        Ok(value) if value.trim().eq_ignore_ascii_case("off")
    )
}

/// What `on_success` should do with an `address-ci` tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffClass {
    Changed,
    Empty { sha: String },
    Unreadable,
}

/// What `ci::drive` should do with a `Decision::Fail`.
#[derive(Debug)]
pub(crate) enum RouteOutcome {
    Routed(Box<StatusView>),
    Exhausted,
    Declined,
}

pub(crate) fn try_route_ci_failure(
    record: &ProjectRecord,
    state: &RunState,
    target: &CiTarget,
    snap: &CheckSnapshot,
) -> Result<RouteOutcome> {
    let Some(owned) = owned_pr(state, target) else {
        return Ok(RouteOutcome::Declined);
    };
    let Some(checks) = failing_required(snap) else {
        return Ok(RouteOutcome::Declined);
    };
    if !enabled(record) {
        return Ok(RouteOutcome::Declined);
    }
    if state.ci_fix_attempts >= CI_FIX_CAP {
        return Ok(RouteOutcome::Exhausted);
    }
    let n = state.ci_fix_attempts.saturating_add(1);
    let request = CiFixRequest {
        pr_number: owned.number,
        from_sha: owned.from_sha,
        checks,
    };
    let view = with_run_state_lock(record, || {
        let mut saved = load_run_state(record)?;
        saved.ci_fix_attempts = n;
        saved.ci_fix_request = Some(request);
        saved.phase = PHASE_ADDRESS_CI.into();
        saved.last_driven_phase = None;
        saved.failure_class = None;
        crate::workflow::reset_phase_clock(&mut saved);
        if let Some(ci) = saved.ci.as_mut() {
            ci.head_sha = None;
            ci.set_key = None;
            ci.last_summary = None;
            ci.next_interval_ms = None;
        }
        saved.last_event = format!("ci-wait: address-ci {n}/{CI_FIX_CAP}");
        saved.updated_at = chrono::Utc::now();
        save_run_state(record, &saved)?;
        Ok(StatusView::from_record(record, &saved))
    })?;
    Ok(RouteOutcome::Routed(Box::new(view)))
}

struct OwnedPr {
    number: u64,
    from_sha: String,
}

fn owned_pr(state: &RunState, target: &CiTarget) -> Option<OwnedPr> {
    let numeric = state.track_id.as_deref().and_then(numeric_track_id)?;
    if numeric.len() != 4 {
        return None;
    }
    match target {
        CiTarget::PullRequest {
            number,
            is_draft,
            merged,
            head_oid,
            head_ref,
            title,
            ..
        } => {
            if *is_draft || *merged {
                return None;
            }
            if !branch_is_track(head_ref, numeric) || !pr_title_is_track(title, numeric) {
                return None;
            }
            Some(OwnedPr {
                number: *number,
                from_sha: head_oid.clone().unwrap_or_default(),
            })
        }
        CiTarget::HeadSha { .. } => None,
    }
}

/// Collapsed required items that contain a Fail. Advisory fallback is not a route.
fn failing_required(snap: &CheckSnapshot) -> Option<Vec<CiFixCheck>> {
    let (collapsed, _) = collapse_snapshot(snap);
    if collapsed.view != CheckView::Required || collapsed.items.is_empty() {
        return None;
    }
    let failing: Vec<CiFixCheck> = collapsed
        .items
        .iter()
        .filter(|item| item.bucket == CheckBucket::Fail)
        .map(check_from_item)
        .collect();
    if failing.is_empty() {
        None
    } else {
        Some(failing)
    }
}

fn check_from_item(item: &CheckItem) -> CiFixCheck {
    let mut description = item.description.clone();
    if description.chars().count() > 1024 {
        description = description.chars().take(1024).collect();
    }
    CiFixCheck {
        name: item.name.clone(),
        bucket: item.bucket.as_str().to_string(),
        description,
        link: item.link.clone(),
    }
}

/// Read-only `HEAD` versus `from_sha`. Untracked files do not count.
pub fn classify_diff(record: &ProjectRecord, state: &RunState) -> DiffClass {
    let Some(request) = state.ci_fix_request.as_ref() else {
        return DiffClass::Unreadable;
    };
    let from_sha = request.from_sha.trim();
    if from_sha.is_empty() {
        return DiffClass::Unreadable;
    }
    let Some(cwd) = crate::worktree::product_git_cwd(record) else {
        return DiffClass::Unreadable;
    };
    let Some(head) = git_text(&cwd, &["rev-parse", "HEAD"]) else {
        return DiffClass::Unreadable;
    };
    if head.is_empty() {
        return DiffClass::Unreadable;
    }
    let Some(names) = git_text(&cwd, &["diff", "--name-only", from_sha, "HEAD"]) else {
        return DiffClass::Unreadable;
    };
    let changed = names.lines().any(|line| !line.trim().is_empty());
    if head != from_sha && changed {
        DiffClass::Changed
    } else {
        DiffClass::Empty {
            sha: from_sha.to_string(),
        }
    }
}

fn git_text(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new(Path::new("git"))
        .args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    Some(text.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_env_lock;
    use crate::layout::LayoutProfile;
    use crate::outcome::{OutcomeSource, PhaseOutcome, write_and_apply};
    use crate::run::run_with_driver;
    use crate::state::{CiFixRequest, RunStatus, load_run_state, save_run_state};
    use crate::workflow::WorkflowDriver;
    use crate::workflow::graph::PHASE_ADDRESS_CI;
    use std::path::Path;

    fn sample(dir: &Path, on: bool) -> ProjectRecord {
        ProjectRecord {
            id: "p".into(),
            path: dir.to_path_buf(),
            display_name: None,
            layout_profile: LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: Some(dir.to_path_buf()),
            execution_repos: Default::default(),
            state_dir: None,
            auto_merge: true,
            phase_timeouts_secs: Default::default(),
            notify_progress: false,
            worktree_isolation: false,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            ci_fix_routing: on,
            created_at: chrono::Utc::now(),
        }
    }

    struct EnvFix {
        prev: Option<std::ffi::OsString>,
    }

    impl EnvFix {
        fn enter() -> Self {
            let prev = std::env::var_os(ENV_CI_FIX);
            Self { prev }
        }
    }

    impl Drop for EnvFix {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var(ENV_CI_FIX, v),
                    None => std::env::remove_var(ENV_CI_FIX),
                }
            }
        }
    }

    #[test]
    fn env_off_disables_and_other_values_do_not_enable() {
        let _lock = test_env_lock();
        let _env = EnvFix::enter();
        let dir = tempfile::tempdir().unwrap();
        let off = sample(dir.path(), false);
        let on = sample(dir.path(), true);
        unsafe {
            std::env::set_var(ENV_CI_FIX, "1");
        }
        assert!(!enabled(&off));
        assert!(enabled(&on));
        unsafe {
            std::env::set_var(ENV_CI_FIX, "off");
        }
        assert!(!enabled(&on));
        unsafe {
            std::env::remove_var(ENV_CI_FIX);
        }
        assert!(enabled(&on));
        assert!(!enabled(&off));
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn seed_repo(dir: &Path) -> String {
        git(dir, &["init", "-b", "main"]);
        git(dir, &["config", "user.email", "ci@example.com"]);
        git(dir, &["config", "user.name", "ci"]);
        std::fs::write(dir.join("README.md"), b"seed\n").unwrap();
        git(dir, &["add", "README.md"]);
        git(
            dir,
            &[
                "-c",
                "user.email=ci@example.com",
                "-c",
                "user.name=ci",
                "commit",
                "-m",
                "seed",
            ],
        );
        git_text(dir, &["rev-parse", "HEAD"]).unwrap()
    }

    fn arm(record: &ProjectRecord, sha: &str) {
        run_with_driver(record, Some("0071".into()), WorkflowDriver::Adapter).unwrap();
        crate::state::with_run_state_lock(record, || {
            let mut state = load_run_state(record)?;
            state.phase = PHASE_ADDRESS_CI.into();
            state.ci_fix_attempts = 1;
            state.ci_fix_request = Some(CiFixRequest {
                pr_number: 71,
                from_sha: sha.into(),
                checks: Vec::new(),
            });
            save_run_state(record, &state)
        })
        .unwrap();
    }

    fn succeed(record: &ProjectRecord) -> crate::state::StatusView {
        write_and_apply(
            record,
            PhaseOutcome::success(
                PHASE_ADDRESS_CI,
                OutcomeSource::Adapter,
                Some("fixed".into()),
                None,
                Some(1),
            ),
        )
        .unwrap()
    }

    struct NotifyOff {
        prev: Option<std::ffi::OsString>,
    }

    impl NotifyOff {
        fn enter() -> Self {
            let prev = std::env::var_os(crate::notify::ENV_COORDINATOR_NOTIFY);
            unsafe {
                std::env::set_var(crate::notify::ENV_COORDINATOR_NOTIFY, "off");
            }
            Self { prev }
        }
    }

    impl Drop for NotifyOff {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var(crate::notify::ENV_COORDINATOR_NOTIFY, v),
                    None => std::env::remove_var(crate::notify::ENV_COORDINATOR_NOTIFY),
                }
            }
        }
    }

    #[test]
    fn changed_tracked_file_advances_to_ci_wait() {
        let _lock = test_env_lock();
        let _notify = NotifyOff::enter();
        let dir = tempfile::tempdir().unwrap();
        let sha = seed_repo(dir.path());
        let record = sample(dir.path(), true);
        arm(&record, &sha);
        std::fs::write(dir.path().join("README.md"), b"changed\n").unwrap();
        git(dir.path(), &["add", "README.md"]);
        git(
            dir.path(),
            &[
                "-c",
                "user.email=ci@example.com",
                "-c",
                "user.name=ci",
                "commit",
                "-m",
                "fix",
            ],
        );
        let view = succeed(&record);
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.phase, crate::workflow::graph::PHASE_CI_WAIT);
        assert!(view.failure_class.is_none());
        let state = load_run_state(&record).unwrap();
        assert_eq!(state.ci_fix_attempts, 1);
        assert!(crate::notify::artifact::existing_path(&record).is_none());
    }

    #[test]
    fn same_sha_stops_with_no_diff() {
        let _lock = test_env_lock();
        let _notify = NotifyOff::enter();
        let dir = tempfile::tempdir().unwrap();
        let sha = seed_repo(dir.path());
        let record = sample(dir.path(), true);
        arm(&record, &sha);
        let view = succeed(&record);
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.phase, PHASE_ADDRESS_CI);
        assert_eq!(
            view.failure_class,
            Some(crate::outcome::FailureClass::CiFailed)
        );
        assert_eq!(view.last_event, format!("address-ci: no diff from {sha}"));
        let state = load_run_state(&record).unwrap();
        assert_eq!(state.ci_fix_attempts, 1);
        assert!(crate::notify::artifact::existing_path(&record).is_some());
    }

    #[test]
    fn untracked_file_alone_stops_with_no_diff() {
        let _lock = test_env_lock();
        let _notify = NotifyOff::enter();
        let dir = tempfile::tempdir().unwrap();
        let sha = seed_repo(dir.path());
        let record = sample(dir.path(), true);
        arm(&record, &sha);
        std::fs::write(dir.path().join("notes.txt"), b"untracked\n").unwrap();
        let view = succeed(&record);
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.last_event, format!("address-ci: no diff from {sha}"));
        assert_eq!(load_run_state(&record).unwrap().ci_fix_attempts, 1);
    }

    #[test]
    fn bad_from_sha_stops_unreadable() {
        let _lock = test_env_lock();
        let _notify = NotifyOff::enter();
        let dir = tempfile::tempdir().unwrap();
        let _sha = seed_repo(dir.path());
        let record = sample(dir.path(), true);
        arm(&record, "0123456789abcdef0123456789abcdef01234567");
        let view = succeed(&record);
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(
            view.failure_class,
            Some(crate::outcome::FailureClass::CiFailed)
        );
        assert_eq!(view.last_event, "address-ci: diff unreadable");
        assert_eq!(load_run_state(&record).unwrap().ci_fix_attempts, 1);
        assert!(crate::notify::artifact::existing_path(&record).is_some());
    }
}
