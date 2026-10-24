//! Opt-in route from the gate's failing checks into same-epoch `address-ci` (0071, 0079).

use std::path::Path;

use chrono::Utc;

use crate::error::Result;
use crate::notify::artifact::numeric_track_id;
use crate::outcome::FailureClass;
use crate::registry::ProjectRecord;
use crate::state::{
    CiFixCheck, CiFixRequest, RunState, StatusView, load_run_state, save_run_state,
    with_run_state_lock,
};
use crate::workflow::graph::{CI_FIX_CAP, PHASE_ADDRESS_CI};
use crate::workflow::shipped::pr_title_is_track;

use super::backend::{CheckBucket, CheckItem, CiTarget};
use super::gh::branch_is_track;

/// `COORDINATOR_CI_FIX=off` forces the route off. No other value turns it on.
pub const ENV_CI_FIX: &str = "COORDINATOR_CI_FIX";

/// Failed engine push attempts before a terminal `ci_failed` stop. Not [`CI_FIX_CAP`].
pub const CI_FIX_PUSH_RETRIES: u32 = 3;
/// Bound on waiting for the PR head to observe a delivered ci-fix push.
pub const CI_FIX_PUSH_WAIT_MAX_MS: i64 = 600_000;

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
    Changed { head: String },
    Empty { sha: String },
    Unreadable,
}

/// What a ci-fix delivery helper wants the caller to do.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum CiFixDelivery {
    Continue,
    Done(Option<StatusView>),
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
    judged: &[CheckItem],
) -> Result<RouteOutcome> {
    let Some(owned) = owned_pr(state, target) else {
        return Ok(RouteOutcome::Declined);
    };
    let Some(checks) = failing_checks(judged) else {
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

pub(crate) struct OwnedPr {
    pub number: u64,
    pub from_sha: String,
    pub head_oid: Option<String>,
}

pub(crate) fn owned_pr(state: &RunState, target: &CiTarget) -> Option<OwnedPr> {
    // Any digit run, not exactly four. A project may register `T374`, whose digit run is the
    // three-digit `374`; a four-digit floor here returned None for such a track, so the chain
    // could never remediate CI on its own PR. Same family as `branch_is_track` -- see the
    // four-digit floor removed from `numeric_track_id` (0075). `numeric_track_id` already
    // returns None for an empty or zero-digit run, so no further guard is needed here.
    let numeric = state.track_id.as_deref().and_then(numeric_track_id)?;
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
            let from_sha = head_oid
                .as_deref()
                .map(str::trim)
                .filter(|sha| !sha.is_empty())?;
            Some(OwnedPr {
                number: *number,
                from_sha: from_sha.to_string(),
                head_oid: Some(from_sha.to_string()),
            })
        }
        CiTarget::HeadSha { .. } => None,
    }
}

/// The slice is the one the gate judged. A Fail on that slice is the route.
fn failing_checks(judged: &[CheckItem]) -> Option<Vec<CiFixCheck>> {
    let failing: Vec<CiFixCheck> = judged
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

/// Read-only `HEAD` versus `from_sha` in the address-ci leg's frozen target.
/// Untracked files do not count. No re-resolution happens under the apply lock.
pub fn classify_diff(record: &ProjectRecord, state: &RunState) -> DiffClass {
    let Some(request) = state.ci_fix_request.as_ref() else {
        return DiffClass::Unreadable;
    };
    let from_sha = request.from_sha.trim();
    if from_sha.is_empty() {
        return DiffClass::Unreadable;
    }
    let Some(target) = state.delivery_target.as_ref() else {
        return DiffClass::Unreadable;
    };
    if target.project_id != record.id
        || target.run_epoch != state.run_epoch
        || state.track_id.as_deref() != Some(target.track_id.as_str())
        || target.phase != state.phase
        || target.planning_only
    {
        return DiffClass::Unreadable;
    }
    let cwd = &target.sample_path;
    let Some(head) = git_text(cwd, &["rev-parse", "HEAD"]) else {
        return DiffClass::Unreadable;
    };
    if head.is_empty() {
        return DiffClass::Unreadable;
    }
    let Some(names) = git_text(cwd, &["diff", "--name-only", from_sha, "HEAD"]) else {
        return DiffClass::Unreadable;
    };
    let changed = names.lines().any(|line| !line.trim().is_empty());
    if head != from_sha && changed {
        DiffClass::Changed { head }
    } else {
        DiffClass::Empty {
            sha: from_sha.to_string(),
        }
    }
}

fn git_text(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = crate::git_command::command()
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

/// First 7 bytes, same slice as `review::short`. Display only.
fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

#[cfg(test)]
thread_local! {
    static PRESET_PUSH_REMOTE: std::cell::RefCell<Option<Result<Option<String>>>> =
        const { std::cell::RefCell::new(None) };
    static PRESET_GIT_PUSH: std::cell::RefCell<Option<Result<Option<String>>>> =
        const { std::cell::RefCell::new(None) };
}

/// One-shot `push_remote` result. Production never calls this.
#[cfg(test)]
pub(crate) fn preset_next_push_remote(result: Result<Option<String>>) {
    PRESET_PUSH_REMOTE.with(|slot| *slot.borrow_mut() = Some(result));
}

/// One-shot `git_push` result. Production never calls this.
#[cfg(test)]
pub(crate) fn preset_next_git_push(result: Result<Option<String>>) {
    PRESET_GIT_PUSH.with(|slot| *slot.borrow_mut() = Some(result));
}

#[cfg(test)]
pub(crate) fn clear_push_presets() {
    PRESET_PUSH_REMOTE.with(|slot| *slot.borrow_mut() = None);
    PRESET_GIT_PUSH.with(|slot| *slot.borrow_mut() = None);
}

fn call_push_remote(cwd: &Path) -> Result<Option<String>> {
    #[cfg(test)]
    if let Some(preset) = PRESET_PUSH_REMOTE.with(|slot| slot.borrow_mut().take()) {
        return preset;
    }
    super::gh::push_remote(cwd)
}

fn call_git_push(cwd: &Path, remote: &str, branch: &str) -> Result<Option<String>> {
    #[cfg(test)]
    if let Some(preset) = PRESET_GIT_PUSH.with(|slot| slot.borrow_mut().take()) {
        return preset;
    }
    super::gh::git_push(cwd, remote, branch)
}

fn stop_ci_fix(
    record: &ProjectRecord,
    state: &RunState,
    message: String,
) -> Result<Option<StatusView>> {
    super::apply_failure(record, state, FailureClass::CiFailed, message)
}

fn hold_ci_fix(record: &ProjectRecord, state: &RunState, event: &str) -> Result<CiFixDelivery> {
    let now = Utc::now();
    let key = match state.ci.as_ref().and_then(|c| c.pr_number) {
        Some(n) => format!("pr:{n}"),
        None => "ci-push".to_string(),
    };
    let interval = super::next_interval_ms(super::elapsed(state, now), false);
    super::persist_watch(record, Some(event), |ci| {
        super::stamp_poll(ci, now, event, &key, interval);
    })?;
    Ok(CiFixDelivery::Done(None))
}

fn note_push_miss(
    record: &ProjectRecord,
    state: &RunState,
    pending: &str,
    number: u64,
    text: &str,
) -> Result<CiFixDelivery> {
    let reason = super::review::truncate_scalars_with_ellipsis(text, 160);
    let attempts = state
        .ci
        .as_ref()
        .map(|c| c.ci_fix_push_attempts)
        .unwrap_or(0)
        .saturating_add(1);
    if attempts >= CI_FIX_PUSH_RETRIES {
        let event = format!("ci-push: {reason} ({attempts}/{CI_FIX_PUSH_RETRIES})");
        return Ok(CiFixDelivery::Done(stop_ci_fix(record, state, event)?));
    }
    let now = Utc::now();
    let interval = super::next_interval_ms(super::elapsed(state, now), false);
    let key = format!("pr:{number}");
    let event = format!("ci-push: {reason} (retry {attempts}/{CI_FIX_PUSH_RETRIES})");
    super::persist_watch(record, Some(&event), |ci| {
        ci.ci_fix_push_attempts = attempts;
        ci.ci_fix_push_pending = Some(pending.to_string());
        super::stamp_poll(ci, now, "ci-push retry", &key, interval);
    })?;
    Ok(CiFixDelivery::Done(None))
}

fn mutation_cwd(
    record: &ProjectRecord,
    state: &RunState,
) -> std::result::Result<std::path::PathBuf, crate::workflow::execution_repo::MutationRefusal> {
    let latched = state.ci.as_ref().and_then(|c| c.target_repo.as_deref());
    crate::workflow::execution_repo::mutation_target(record, state, latched)
        .map(|target| target.execution.sample_path)
}

/// Engine delivery of an `address-ci` commit. `Continue` only when nothing is pending.
/// Every `Some(pending)` arm returns `Done` and ends the tick.
pub(crate) fn deliver_ci_fix_push(
    record: &ProjectRecord,
    state: &RunState,
    target: &CiTarget,
) -> Result<CiFixDelivery> {
    let Some(pending) = state
        .ci
        .as_ref()
        .and_then(|c| c.ci_fix_push_pending.clone())
    else {
        return Ok(CiFixDelivery::Continue);
    };
    let (number, head_ref) = match target {
        CiTarget::PullRequest {
            number,
            head_ref,
            merged,
            closed,
            ..
        } => {
            if *merged {
                let event = format!(
                    "ci-push: PR #{number} merged before the CI fix was delivered (local {} on {head_ref})",
                    short(&pending)
                );
                return Ok(CiFixDelivery::Done(stop_ci_fix(record, state, event)?));
            }
            if *closed {
                let event = format!(
                    "ci-push: PR #{number} closed before the CI fix was delivered (local {} on {head_ref})",
                    short(&pending)
                );
                return Ok(CiFixDelivery::Done(stop_ci_fix(record, state, event)?));
            }
            (*number, head_ref.clone())
        }
        CiTarget::HeadSha { .. } => return hold_ci_fix(record, state, "ci-push: waiting for PR"),
    };
    let Some(request) = state.ci_fix_request.as_ref() else {
        return hold_ci_fix(record, state, "ci-push: waiting for request");
    };
    if request.pr_number != number {
        let event = format!("ci-push: foreign PR #{number}");
        return Ok(CiFixDelivery::Done(stop_ci_fix(record, state, event)?));
    }
    let from_sha = request.from_sha.clone();
    let cwd = match mutation_cwd(record, state) {
        Ok(cwd) => cwd,
        Err(refusal) => {
            return Ok(CiFixDelivery::Done(super::apply_mutation_refusal(
                record, state, "ci-push", &refusal,
            )?));
        }
    };
    let live_branch = super::gh::symbolic_head_branch(&cwd).unwrap_or_default();
    let numeric = state
        .track_id
        .as_deref()
        .and_then(numeric_track_id)
        .unwrap_or_default();
    if live_branch != head_ref || !branch_is_track(&live_branch, numeric) {
        let event = format!("ci-push: branch {live_branch} is not the PR head {head_ref}");
        return Ok(CiFixDelivery::Done(stop_ci_fix(record, state, event)?));
    }
    let Some(names) = git_text(&cwd, &["diff", "--name-only", &from_sha, "HEAD"]) else {
        return Ok(CiFixDelivery::Done(stop_ci_fix(
            record,
            state,
            "ci-push: diff unreadable".into(),
        )?));
    };
    if !names.lines().any(|line| !line.trim().is_empty()) {
        let event = format!("ci-push: no diff from {}", short(&from_sha));
        return Ok(CiFixDelivery::Done(stop_ci_fix(record, state, event)?));
    }
    let Some(head) = super::gh::git_head_sha(&cwd) else {
        return Ok(CiFixDelivery::Done(stop_ci_fix(
            record,
            state,
            "ci-push: head unreadable".into(),
        )?));
    };
    let gate = crate::policy::decide(record, state, &cwd, &head);
    match super::release_for_mutation(record, state, &gate)? {
        super::MutationGate::Stop(Some(view)) => return Ok(CiFixDelivery::Done(Some(*view))),
        super::MutationGate::Stop(None) => {
            return hold_ci_fix(record, state, "ci-push: policy hold");
        }
        super::MutationGate::Go => {}
    }
    let remote = match call_push_remote(&cwd) {
        Ok(Some(remote)) => remote,
        Ok(None) => {
            return Ok(CiFixDelivery::Done(stop_ci_fix(
                record,
                state,
                "ci-push: no GitHub remote".into(),
            )?));
        }
        Err(err) => return note_push_miss(record, state, &pending, number, &err.to_string()),
    };
    match call_git_push(&cwd, &remote, &live_branch) {
        Ok(None) => {
            let Some(tip) = super::gh::git_head_sha(&cwd) else {
                return Ok(CiFixDelivery::Done(stop_ci_fix(
                    record,
                    state,
                    "ci-push: head unreadable".into(),
                )?));
            };
            let now = Utc::now();
            with_run_state_lock(record, || {
                let mut saved = load_run_state(record)?;
                let mut ci = saved.ci.take().unwrap_or_default();
                ci.ci_fix_push_pending = None;
                ci.ci_fix_pushed_sha = Some(tip.clone());
                ci.ci_fix_push_attempts = 0;
                ci.ci_fix_push_waiting_since = None;
                ci.last_poll_at = None;
                saved.ci = Some(ci);
                saved.ci_fix_request = None;
                saved.last_event = format!("ci-push: pushed {}", short(&tip));
                saved.updated_at = now;
                save_run_state(record, &saved)
            })?;
            Ok(CiFixDelivery::Done(None))
        }
        Ok(Some(reason)) => note_push_miss(record, state, &pending, number, &reason),
        Err(err) => note_push_miss(record, state, &pending, number, &err.to_string()),
    }
}

/// Wait until the PR observes the pushed ci-fix head. Clears the marker on a match.
pub(crate) fn wait_for_ci_fix_head(
    record: &ProjectRecord,
    state: &RunState,
    target: &CiTarget,
) -> Result<CiFixDelivery> {
    let Some(pushed) = state.ci.as_ref().and_then(|c| c.ci_fix_pushed_sha.clone()) else {
        return Ok(CiFixDelivery::Continue);
    };
    let CiTarget::PullRequest {
        head_oid,
        number,
        head_ref,
        closed,
        ..
    } = target
    else {
        return Ok(CiFixDelivery::Continue);
    };
    if head_oid.as_deref().map(str::trim) == Some(pushed.as_str()) {
        with_run_state_lock(record, || {
            let mut saved = load_run_state(record)?;
            if let Some(ci) = saved.ci.as_mut() {
                ci.ci_fix_pushed_sha = None;
                ci.ci_fix_push_waiting_since = None;
            }
            saved.updated_at = Utc::now();
            save_run_state(record, &saved)
        })?;
        return Ok(CiFixDelivery::Continue);
    }
    if *closed {
        let event = format!(
            "ci-push: PR #{number} closed before the CI fix head was observed (local {} on {head_ref})",
            short(&pushed)
        );
        return Ok(CiFixDelivery::Done(stop_ci_fix(record, state, event)?));
    }
    let now = Utc::now();
    let since = state
        .ci
        .as_ref()
        .and_then(|c| c.ci_fix_push_waiting_since)
        .unwrap_or(now);
    if (now - since).num_milliseconds() > CI_FIX_PUSH_WAIT_MAX_MS {
        let event = format!("ci-push: head moved ({})", short(&pushed));
        return Ok(CiFixDelivery::Done(stop_ci_fix(record, state, event)?));
    }
    let interval = super::next_interval_ms(super::elapsed(state, now), false);
    let key = format!("pr:{number}");
    super::persist_watch(
        record,
        Some("ci-wait: waiting for pushed CI fix head"),
        |ci| {
            if ci.ci_fix_push_waiting_since.is_none() {
                ci.ci_fix_push_waiting_since = Some(now);
            }
            super::stamp_poll(ci, now, "waiting for pushed CI fix head", &key, interval);
        },
    )?;
    Ok(CiFixDelivery::Done(None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ci::backend::MergeStateStatus;
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
            review_comment_routing: false,
            skill_aliases: std::collections::BTreeMap::new(),
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

    /// A three-digit track id is a normal id, not an error. AI-Brains registers `T374`, whose
    /// digit run is `374`; the four-digit floor made `owned_pr` return None for it, so the chain
    /// could never remediate CI on its own pull request. This is the same defect as the floor
    /// already removed from `branch_is_track` and `numeric_track_id`.
    #[test]
    fn owned_pr_accepts_a_short_track_id() {
        let mut state = RunState::idle("p");
        state.track_id = Some("374".into());
        let target = CiTarget::PullRequest {
            number: 71,
            url: "https://example/pr/71".into(),
            is_draft: false,
            merged: false,
            closed: false,
            head_oid: Some("abc111".into()),
            merge_state: MergeStateStatus::Clean,
            head_ref: "track/374".into(),
            title: "track(374): agy stop-hook quoting".into(),
        };
        assert!(
            owned_pr(&state, &target).is_some(),
            "a three-digit id owns its own PR"
        );
        // A different id never owns it.
        state.track_id = Some("375".into());
        assert!(owned_pr(&state, &target).is_none());
        // Nor does a run with no track at all.
        state.track_id = None;
        assert!(owned_pr(&state, &target).is_none());
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
        let out = crate::git_command::command()
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
        // Real conductor row naming the fixture repo: grading reads the frozen target,
        // and address-ci now follows the prepared route.
        let conductor = record.path.join("conductor");
        std::fs::create_dir_all(&conductor).unwrap();
        let row = format!(
            "| [0071-Example](0071-Example/spec.md) | `{}` | **Ready - not started** | fixture |\n",
            record
                .execution_repo
                .as_deref()
                .unwrap_or(record.path.as_path())
                .display()
        );
        std::fs::write(
            conductor.join("conductor.md"),
            format!("| Track | Execution | Status | Summary |\n| --- | --- | --- | --- |\n{row}"),
        )
        .unwrap();
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
            let repo = record
                .execution_repo
                .as_deref()
                .unwrap_or(record.path.as_path());
            state.delivery_target = Some(crate::workflow::execution_repo::seeded_delivery_target(
                record,
                "0071",
                state.run_epoch,
                PHASE_ADDRESS_CI,
                repo,
            ));
            crate::workflow::execution_repo::assert_fixture_has_delivery_target(&state);
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
        let head = git_text(dir.path(), &["rev-parse", "HEAD"]).unwrap();
        let state = load_run_state(&record).unwrap();
        assert_eq!(state.ci_fix_attempts, 1);
        assert!(state.ci_fix_request.is_some());
        let ci = state.ci.as_ref().expect("watch state");
        assert_eq!(ci.ci_fix_push_pending.as_deref(), Some(head.as_str()));
        assert!(ci.review_push_pending.is_none());
        assert!(ci.review_pushed_sha.is_none());
        assert!(ci.review_push_waiting_since.is_none());
        assert_eq!(ci.review_push_attempts, 0);
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

    #[test]
    fn failing_checks_ignores_empty_pass_and_cancel() {
        assert!(failing_checks(&[]).is_none());
        assert!(failing_checks(&[CheckItem::new("lint", CheckBucket::Pass)]).is_none());
        assert!(failing_checks(&[CheckItem::new("risk", CheckBucket::Cancel)]).is_none());
        let got = failing_checks(&[
            CheckItem::new("lint", CheckBucket::Pass),
            CheckItem::new("risk", CheckBucket::Fail),
        ])
        .expect("fail");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "risk");
        assert_eq!(got[0].bucket, "fail");
    }

    #[test]
    fn ci_fix_gate_fixtures_seed_the_delivery_target() {
        let _lock = test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let sha = seed_repo(dir.path());
        let record = sample(dir.path(), true);
        arm(&record, &sha);
        let state = load_run_state(&record).unwrap();
        crate::workflow::execution_repo::assert_fixture_has_delivery_target(&state);
    }

    #[test]
    fn classify_diff_reads_target_head() {
        let _lock = test_env_lock();
        let primary = tempfile::tempdir().unwrap();
        let _primary_sha = seed_repo(primary.path());
        let sibling = tempfile::tempdir().unwrap();
        let sibling_sha = seed_repo(sibling.path());
        let record = sample(primary.path(), true);
        run_with_driver(&record, Some("0071".into()), WorkflowDriver::Adapter).unwrap();
        crate::state::with_run_state_lock(&record, || {
            let mut state = load_run_state(&record)?;
            state.phase = PHASE_ADDRESS_CI.into();
            state.ci_fix_attempts = 1;
            state.ci_fix_request = Some(CiFixRequest {
                pr_number: 71,
                from_sha: sibling_sha.clone(),
                checks: Vec::new(),
            });
            state.delivery_target = Some(crate::workflow::execution_repo::seeded_delivery_target(
                &record,
                "0071",
                state.run_epoch,
                PHASE_ADDRESS_CI,
                sibling.path(),
            ));
            save_run_state(&record, &state)
        })
        .unwrap();
        let state = load_run_state(&record).unwrap();
        assert!(matches!(
            classify_diff(&record, &state),
            DiffClass::Empty { .. }
        ));
        // A commit in the primary is invisible to the target leg.
        std::fs::write(primary.path().join("a.rs"), b"fn a() {}\n").unwrap();
        git(primary.path(), &["add", "a.rs"]);
        git(
            primary.path(),
            &[
                "-c",
                "user.email=ci@example.com",
                "-c",
                "user.name=ci",
                "commit",
                "-m",
                "primary only",
            ],
        );
        assert!(matches!(
            classify_diff(&record, &state),
            DiffClass::Empty { .. }
        ));
        std::fs::write(sibling.path().join("b.rs"), b"fn b() {}\n").unwrap();
        git(sibling.path(), &["add", "b.rs"]);
        git(
            sibling.path(),
            &[
                "-c",
                "user.email=ci@example.com",
                "-c",
                "user.name=ci",
                "commit",
                "-m",
                "target fix",
            ],
        );
        let head = git_text(sibling.path(), &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(classify_diff(&record, &state), DiffClass::Changed { head });
    }

    fn owned_ci_target(head: Option<&str>) -> CiTarget {
        CiTarget::PullRequest {
            number: 10,
            url: "https://example/pr/10".into(),
            is_draft: false,
            merged: false,
            closed: false,
            head_oid: head.map(str::to_string),
            merge_state: MergeStateStatus::Clean,
            head_ref: "track/0010-Fixture".into(),
            title: "track(0010): fixture".into(),
        }
    }

    fn failing_check() -> Vec<CheckItem> {
        vec![CheckItem::new("ci", CheckBucket::Fail)]
    }

    fn save_route_state(record: &ProjectRecord, attempts: u32) -> RunState {
        let mut state = RunState::idle(&record.id);
        state.track_id = Some("0010".into());
        state.phase = crate::workflow::graph::PHASE_CI_WAIT.into();
        state.status = RunStatus::Running;
        state.ci_fix_attempts = attempts;
        state.ci_fix_request = None;
        save_run_state(record, &state).unwrap();
        state
    }

    fn assert_declined(record: &ProjectRecord, attempts: u32, outcome: &RouteOutcome) {
        assert!(matches!(outcome, RouteOutcome::Declined));
        let saved = load_run_state(record).unwrap();
        assert_eq!(saved.ci_fix_attempts, attempts);
        assert!(saved.ci_fix_request.is_none());
        assert_ne!(saved.phase, PHASE_ADDRESS_CI);
        assert!(!saved.last_event.contains("exhausted"));
    }

    #[test]
    fn missing_head_oid_does_not_route() {
        let _lock = test_env_lock();
        let _env = EnvFix::enter();
        unsafe {
            std::env::remove_var(ENV_CI_FIX);
        }
        let dir = tempfile::tempdir().unwrap();
        let record = sample(dir.path(), true);
        let state = save_route_state(&record, 0);
        let outcome =
            try_route_ci_failure(&record, &state, &owned_ci_target(None), &failing_check())
                .unwrap();
        assert_declined(&record, 0, &outcome);
        let state = save_route_state(&record, 0);
        let outcome = try_route_ci_failure(
            &record,
            &state,
            &owned_ci_target(Some("abc111")),
            &failing_check(),
        )
        .unwrap();
        assert!(matches!(outcome, RouteOutcome::Routed(_)));
        let saved = load_run_state(&record).unwrap();
        assert_eq!(saved.phase, PHASE_ADDRESS_CI);
        assert_eq!(saved.ci_fix_attempts, 1);
        assert_eq!(
            saved.ci_fix_request.as_ref().map(|r| r.from_sha.as_str()),
            Some("abc111")
        );
    }

    #[test]
    fn blank_head_oid_does_not_route() {
        let _lock = test_env_lock();
        let _env = EnvFix::enter();
        unsafe {
            std::env::remove_var(ENV_CI_FIX);
        }
        let dir = tempfile::tempdir().unwrap();
        let record = sample(dir.path(), true);
        for blank in [Some(""), Some("  ")] {
            let state = save_route_state(&record, 0);
            let outcome =
                try_route_ci_failure(&record, &state, &owned_ci_target(blank), &failing_check())
                    .unwrap();
            assert_declined(&record, 0, &outcome);
        }
        let state = save_route_state(&record, 0);
        let outcome = try_route_ci_failure(
            &record,
            &state,
            &owned_ci_target(Some("abc111")),
            &failing_check(),
        )
        .unwrap();
        assert!(matches!(outcome, RouteOutcome::Routed(_)));
        assert_eq!(
            load_run_state(&record)
                .unwrap()
                .ci_fix_request
                .unwrap()
                .from_sha,
            "abc111"
        );
    }

    #[test]
    fn missing_head_at_cap_is_declined_not_exhausted() {
        let _lock = test_env_lock();
        let _env = EnvFix::enter();
        unsafe {
            std::env::remove_var(ENV_CI_FIX);
        }
        let dir = tempfile::tempdir().unwrap();
        let record = sample(dir.path(), true);
        let state = save_route_state(&record, CI_FIX_CAP);
        let outcome =
            try_route_ci_failure(&record, &state, &owned_ci_target(None), &failing_check())
                .unwrap();
        assert_declined(&record, CI_FIX_CAP, &outcome);
        assert!(!matches!(outcome, RouteOutcome::Exhausted));
        let state = save_route_state(&record, CI_FIX_CAP);
        let outcome = try_route_ci_failure(
            &record,
            &state,
            &owned_ci_target(Some("abc111")),
            &failing_check(),
        )
        .unwrap();
        assert!(matches!(outcome, RouteOutcome::Exhausted));
        let saved = load_run_state(&record).unwrap();
        assert_eq!(saved.ci_fix_attempts, CI_FIX_CAP);
        assert!(saved.ci_fix_request.is_none());
        assert_ne!(saved.phase, PHASE_ADDRESS_CI);
    }
}
