//! Run state machine (ADR-0024 stop/pause; 0008 canonical DAG).
//!
//! Phase completion and timeouts are driven by the Phase Outcome apply path
//! ([`crate::outcome`]) and poll/wait ([`crate::watch`]).

use crate::error::{CoordinatorError, Result};
use crate::outcome::clear_active_outcome_file;
use crate::registry::ProjectRecord;
use crate::state::{
    CiWatchState, RunState, RunStatus, STOP_LAST_EVENT, STUB_PHASE_ACTIVE, STUB_PHASE_COMPLETED,
    STUB_PHASE_FAILED, STUB_PHASE_STOPPED, StatusView, ensure_state_dir, load_run_state,
    save_run_state, with_run_state_lock,
};
use crate::workflow::{self, WORKFLOW_ID, WorkflowDriver};

/// Apply `run`: Idle/Stopped → Running at `plan` (`canonical_v1`).
///
/// If `--track` is omitted, prior `track_id` is **retained**. `next_track` is cleared.
pub fn run(record: &ProjectRecord, track_id: Option<String>) -> Result<StatusView> {
    run_with_driver(record, track_id, workflow::resolve_driver(None)?)
}

/// Start the canonical workflow with an explicit driver.
///
/// Public primitive: omit `track_id` still **retains**. Pick lives in `api::cmd_run`.
pub fn run_with_driver(
    record: &ProjectRecord,
    track_id: Option<String>,
    driver: WorkflowDriver,
) -> Result<StatusView> {
    run_with_origin(record, track_id, driver, false)
}

struct RunProbeCtx {
    snap_epoch: u64,
    snap_track: Option<String>,
    snap_status: RunStatus,
    track_arg: Option<String>,
    driver: WorkflowDriver,
    picked: bool,
    number: u64,
    head_sha: String,
    effective_track: String,
}

enum RunPrep {
    Done(Box<StatusView>),
    NeedsProbe(RunProbeCtx),
}

struct FreshRun {
    track_arg: Option<String>,
    driver: WorkflowDriver,
    picked: bool,
    archive_slugs: Vec<String>,
    journal_unreadable: bool,
    /// When set, phase is `ci-wait` with this PR number and head sha planted.
    ci_wait: Option<(u64, String)>,
}

/// Inner start used by `cmd_run`. `picked` tags `last_event` `(next Ready)` in the same lock.
///
/// A receipt that still needs `gh pr view` releases the lock before the probe.
/// `prepare_epoch` runs only after that answer, and only if the snapshot still matches.
pub(crate) fn run_with_origin(
    record: &ProjectRecord,
    track_id: Option<String>,
    driver: WorkflowDriver,
    picked: bool,
) -> Result<StatusView> {
    let prep = with_run_state_lock(record, || {
        prepare_run_locked(record, track_id, driver, picked)
    })?;
    match prep {
        RunPrep::Done(view) => Ok(*view),
        RunPrep::NeedsProbe(ctx) => {
            let answer = crate::workflow::reuse::probe_resume(record, ctx.number);
            with_run_state_lock(record, || finish_probed_run(record, &ctx, &answer))
        }
    }
}

fn prepare_run_locked(
    record: &ProjectRecord,
    track_id: Option<String>,
    driver: WorkflowDriver,
    picked: bool,
) -> Result<RunPrep> {
    ensure_state_dir(record)?;
    let state = load_run_state(record)?;
    match state.status {
        RunStatus::Idle | RunStatus::Stopped => {
            if track_id.is_none()
                && let Some(id) = state
                    .track_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                && crate::workflow::track_is_shipped_local(record, &state, id)
            {
                return Err(CoordinatorError::Message(format!(
                    "omit-pick: shipped track {id} is not resumable; pass --track <id>"
                )));
            }
            let effective = if track_id.is_some() {
                track_id.clone()
            } else {
                state.track_id.clone()
            };
            match crate::workflow::reuse::decide_resume(record, effective.as_deref()) {
                crate::workflow::reuse::ResumeChoice::Probe {
                    number,
                    head_sha,
                    head_ref: _,
                } => {
                    let effective_track = effective.unwrap_or_default();
                    Ok(RunPrep::NeedsProbe(RunProbeCtx {
                        snap_epoch: state.run_epoch,
                        snap_track: state.track_id.clone(),
                        snap_status: state.status,
                        track_arg: track_id,
                        driver,
                        picked,
                        number,
                        head_sha,
                        effective_track,
                    }))
                }
                crate::workflow::reuse::ResumeChoice::Plan {
                    archive_slugs,
                    journal_unreadable,
                } => {
                    let mut state = state;
                    let view = begin_fresh_run(
                        record,
                        &mut state,
                        FreshRun {
                            track_arg: track_id,
                            driver,
                            picked,
                            archive_slugs,
                            journal_unreadable,
                            ci_wait: None,
                        },
                    )?;
                    Ok(RunPrep::Done(Box::new(view)))
                }
            }
        }
        other => Err(CoordinatorError::InvalidTransition {
            action: "run",
            from: other.to_string(),
        }),
    }
}

fn finish_probed_run(
    record: &ProjectRecord,
    ctx: &RunProbeCtx,
    answer: &Result<Option<crate::workflow::reuse::ResumePr>>,
) -> Result<StatusView> {
    let mut state = load_run_state(record)?;
    if state.run_epoch != ctx.snap_epoch
        || state.track_id != ctx.snap_track
        || state.status != ctx.snap_status
    {
        return Ok(StatusView::from_record(record, &state));
    }
    let ci_wait =
        if crate::workflow::reuse::probe_accepts(&ctx.effective_track, &ctx.head_sha, answer) {
            Some((ctx.number, ctx.head_sha.clone()))
        } else {
            None
        };
    begin_fresh_run(
        record,
        &mut state,
        FreshRun {
            track_arg: ctx.track_arg.clone(),
            driver: ctx.driver,
            picked: ctx.picked,
            archive_slugs: Vec::new(),
            journal_unreadable: false,
            ci_wait,
        },
    )
}

fn begin_fresh_run(
    record: &ProjectRecord,
    state: &mut RunState,
    fresh: FreshRun,
) -> Result<StatusView> {
    let closed_epoch = state.run_epoch;
    let next_epoch = state.run_epoch.saturating_add(1);
    crate::checkpoint::reap_abandoned(record, next_epoch)?;
    crate::worktree::prepare_epoch(record, next_epoch)?;
    let leftover =
        state.failure_class.is_some() || crate::notify::artifact::existing_path(record).is_some();
    let already_settled = state.last_event == crate::notify::SETTLED_DETAIL;
    state.status = RunStatus::Running;
    state.phase = workflow::graph::PHASE_PLAN.into();
    state.workflow = Some(WORKFLOW_ID.into());
    state.driver = fresh.driver;
    state.pending_roles.clear();
    state.last_driven_phase = None;
    if fresh.track_arg.is_some() {
        state.track_id = fresh.track_arg;
    }
    state.run_epoch = state.run_epoch.saturating_add(1);
    state.phase_started_at = Some(chrono::Utc::now());
    state.total_paused_ms = 0;
    state.pause_started_at = None;
    state.failure_class = None;
    state.state_gate = None;
    state.self_check = None;
    state.next_track = None;
    state.parked_next = None;
    state.last_applied_outcome_hash = None;
    state.ci = None;
    state.review = None;
    state.stalled_at = None;
    state.pause_spans.clear();
    state.stall_recycles = 0;
    state.aborted_session_id = None;
    state.acp_stdout_retries = 0;
    state.plan_review_spawned.clear();
    state.plan_review_join_retries = 0;
    state.plan_review_slot_ran.clear();
    state.address_findings_attempts = 0;
    state.ci_fix_attempts = 0;
    state.ci_fix_request = None;
    state.sticky_ready_ids =
        crate::workflow::conductor_md::capture_sticky_ready_ids(record, &state.sticky_ready_ids);
    if fresh.picked {
        let id = state.track_id.as_deref().unwrap_or("-");
        state.last_event = format!("run: started {WORKFLOW_ID} track={id} (next Ready)");
    } else {
        state.last_event = format!("run: started {WORKFLOW_ID}");
    }
    if let Some((number, sha)) = fresh.ci_wait {
        state.phase = workflow::graph::PHASE_CI_WAIT.into();
        state.ci = Some(CiWatchState {
            pr_number: Some(number),
            head_sha: Some(sha),
            ..Default::default()
        });
        state.last_event = format!("run: started {WORKFLOW_ID} resume ci-wait pr {number}");
    }
    state.updated_at = chrono::Utc::now();
    clear_active_outcome_file(record);
    if fresh.journal_unreadable {
        crate::progress_log::append(record, "reuse", "reuse: receipt unreadable");
    }
    crate::workflow::drive::clear_plan_review_artifacts(
        record,
        &crate::workflow::drive::ArtifactClear {
            track_id: state.track_id.clone(),
            closed_epoch,
            archive_slugs: fresh.archive_slugs,
        },
    );
    crate::notify::clear_artifact(record);
    crate::workflow::watchdog::clear_progress(record);
    if leftover && !already_settled {
        crate::progress_log::append(record, "start", crate::notify::START_CLEAR_DETAIL);
    }
    save_run_state(record, state)?;
    let track = state.track_id.as_deref().unwrap_or("-");
    crate::progress_log::append(
        record,
        "start",
        &format!("track={track} phase={}  {}", state.phase, state.last_event),
    );
    Ok(StatusView::from_record(record, state))
}

/// Test-only leftover stub entry (does not go through public `run`).
#[cfg(test)]
pub fn run_stub(record: &ProjectRecord, track_id: Option<String>) -> Result<StatusView> {
    with_run_state_lock(record, || {
        ensure_state_dir(record)?;
        let mut state = load_run_state(record)?;
        match state.status {
            RunStatus::Idle | RunStatus::Stopped => {
                let leftover = state.failure_class.is_some()
                    || crate::notify::artifact::existing_path(record).is_some();
                let already_settled = state.last_event == crate::notify::SETTLED_DETAIL;
                state.status = RunStatus::Running;
                state.phase = STUB_PHASE_ACTIVE.into();
                state.workflow = None;
                state.driver = WorkflowDriver::Stub;
                state.pending_roles.clear();
                state.last_driven_phase = None;
                if track_id.is_some() {
                    state.track_id = track_id;
                }
                state.run_epoch = state.run_epoch.saturating_add(1);
                state.phase_started_at = Some(chrono::Utc::now());
                state.total_paused_ms = 0;
                state.pause_started_at = None;
                state.failure_class = None;
                state.state_gate = None;
                state.self_check = None;
                state.parked_next = None;
                state.last_applied_outcome_hash = None;
                state.stalled_at = None;
                state.pause_spans.clear();
                state.stall_recycles = 0;
                state.aborted_session_id = None;
                state.acp_stdout_retries = 0;
                state.plan_review_spawned.clear();
                state.plan_review_join_retries = 0;
                state.plan_review_slot_ran.clear();
                state.last_event = "run: started stub".into();
                state.updated_at = chrono::Utc::now();
                clear_active_outcome_file(record);
                crate::notify::clear_artifact(record);
                crate::workflow::watchdog::clear_progress(record);
                if leftover && !already_settled {
                    crate::progress_log::append(record, "start", crate::notify::START_CLEAR_DETAIL);
                }
                save_run_state(record, &state)?;
                Ok(StatusView::from_record(record, &state))
            }
            other => Err(CoordinatorError::InvalidTransition {
                action: "run",
                from: other.to_string(),
            }),
        }
    })
}

/// Apply `pause`: Running → Paused (timeout budget freezes).
pub fn pause(record: &ProjectRecord) -> Result<StatusView> {
    transition(record, "pause", |state| match state.status {
        RunStatus::Running => {
            state.status = RunStatus::Paused;
            state.pause_started_at = Some(chrono::Utc::now());
            state.last_event = "pause: hold".into();
            Ok(())
        }
        other => Err(CoordinatorError::InvalidTransition {
            action: "pause",
            from: other.to_string(),
        }),
    })
}

enum ResumePrep {
    Done(Box<StatusView>),
    Probe(crate::workflow::reuse::ReuseProbe),
}

/// Apply `resume`: Paused → Running (accumulate paused duration for timeout freeze).
///
/// If the phase already completed/failed while held, release to Idle instead of
/// re-entering `Running` without a phase clock (would hang forever under autonomy).
/// An advance successor that still needs `gh pr view` is probed after the lock drops.
pub fn resume(record: &ProjectRecord) -> Result<StatusView> {
    let prep = with_run_state_lock(record, || resume_locked(record))?;
    match prep {
        ResumePrep::Done(view) => Ok(*view),
        ResumePrep::Probe(probe) => {
            let answer = crate::workflow::reuse::probe_resume(record, probe.number);
            with_run_state_lock(record, || {
                crate::workflow::commit_probed_successor(record, &probe, &answer)
            })
        }
    }
}

fn resume_locked(record: &ProjectRecord) -> Result<ResumePrep> {
    ensure_state_dir(record)?;
    let mut state = load_run_state(record)?;
    match state.status {
        RunStatus::Paused => {
            if state.phase == STUB_PHASE_COMPLETED || state.phase == STUB_PHASE_FAILED {
                state.status = RunStatus::Idle;
                state.pause_started_at = None;
                state.phase_started_at = None;
                state.last_event = "resume: release hold after phase outcome".into();
                state.updated_at = chrono::Utc::now();
                save_run_state(record, &state)?;
                return Ok(ResumePrep::Done(Box::new(StatusView::from_record(
                    record, &state,
                ))));
            }
            if state.phase == workflow::graph::PHASE_ADVANCE
                && state.last_driven_phase.as_deref() == Some(workflow::graph::PHASE_ADVANCE)
            {
                let probe = crate::workflow::apply_advance_on_resume(record, &mut state);
                state.updated_at = chrono::Utc::now();
                save_run_state(record, &state)?;
                return Ok(match probe {
                    Some(probe) => ResumePrep::Probe(probe),
                    None => ResumePrep::Done(Box::new(StatusView::from_record(record, &state))),
                });
            }
            let now = chrono::Utc::now();
            if let Some(pstart) = state.pause_started_at.take() {
                let delta = (now - pstart).num_milliseconds().max(0) as u64;
                state.total_paused_ms = state.total_paused_ms.saturating_add(delta);
                state.pause_spans.push(crate::state::PauseSpan {
                    start: pstart,
                    end: now,
                });
            }
            state.status = RunStatus::Running;
            if state.phase.is_empty() {
                state.phase = STUB_PHASE_ACTIVE.into();
            }
            if state.phase_started_at.is_none() {
                state.phase_started_at = Some(now);
                state.total_paused_ms = 0;
            }
            state.last_event = "resume: continue".into();
            state.updated_at = chrono::Utc::now();
            save_run_state(record, &state)?;
            Ok(ResumePrep::Done(Box::new(StatusView::from_record(
                record, &state,
            ))))
        }
        other => Err(CoordinatorError::InvalidTransition {
            action: "resume",
            from: other.to_string(),
        }),
    }
}

/// Apply `stop`: Running/Paused → Stopped; already Stopped = successful no-op.
pub fn stop(record: &ProjectRecord) -> Result<StatusView> {
    with_run_state_lock(record, || {
        ensure_state_dir(record)?;
        let mut state = load_run_state(record)?;
        match state.status {
            RunStatus::Running | RunStatus::Paused => {
                state.status = RunStatus::Stopped;
                state.phase = STUB_PHASE_STOPPED.into();
                state.last_event = STOP_LAST_EVENT.into();
                state.phase_started_at = None;
                state.pause_started_at = None;
                state.updated_at = chrono::Utc::now();
                save_run_state(record, &state)?;
                let track = state.track_id.as_deref().unwrap_or("-");
                crate::progress_log::append(
                    record,
                    "stop",
                    &format!("track={track} phase={}  {}", state.phase, state.last_event),
                );
                Ok(StatusView::from_record(record, &state))
            }
            RunStatus::Stopped => {
                // Idempotent re-stop: successful no-op (ADR / DoD-2).
                if state.last_event != STOP_LAST_EVENT {
                    state.last_event = STOP_LAST_EVENT.into();
                    state.updated_at = chrono::Utc::now();
                    save_run_state(record, &state)?;
                }
                Ok(StatusView::from_record(record, &state))
            }
            other => Err(CoordinatorError::InvalidTransition {
                action: "stop",
                from: other.to_string(),
            }),
        }
    })
}

/// Read status without mutation.
pub fn status(record: &ProjectRecord) -> Result<StatusView> {
    let state = load_run_state(record)?;
    Ok(StatusView::from_record(record, &state))
}

fn transition<F>(record: &ProjectRecord, _action: &str, f: F) -> Result<StatusView>
where
    F: FnOnce(&mut RunState) -> Result<()>,
{
    with_run_state_lock(record, || {
        ensure_state_dir(record)?;
        let mut state = load_run_state(record)?;
        f(&mut state)?;
        state.updated_at = chrono::Utc::now();
        save_run_state(record, &state)?;
        Ok(StatusView::from_record(record, &state))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ProjectRecord;
    use chrono::Utc;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn rec(path: &std::path::Path) -> ProjectRecord {
        ProjectRecord {
            id: Uuid::new_v4().to_string(),
            path: path.to_path_buf(),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: std::collections::BTreeMap::new(),
            state_dir: None,
            auto_merge: true,
            phase_timeouts_secs: std::collections::BTreeMap::new(),
            notify_progress: false,
            worktree_isolation: false,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            ci_fix_routing: false,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn happy_path_run_pause_resume_stop() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());

        let s = run(&r, Some("0004".into())).unwrap();
        assert_eq!(s.status, RunStatus::Running);
        assert_eq!(s.phase, crate::workflow::graph::PHASE_PLAN);
        assert_eq!(s.track_id.as_deref(), Some("0004"));
        assert!(s.last_event.contains("run:"));

        let s = pause(&r).unwrap();
        assert_eq!(s.status, RunStatus::Paused);

        let s = resume(&r).unwrap();
        assert_eq!(s.status, RunStatus::Running);

        let s = stop(&r).unwrap();
        assert_eq!(s.status, RunStatus::Stopped);
        assert_eq!(s.last_event, STOP_LAST_EVENT);
        assert_eq!(s.phase, STUB_PHASE_STOPPED);
        let log = std::fs::read_to_string(dir.path().join("status.md")).unwrap();
        assert!(log.contains("start  track=0004"));
        assert!(log.contains("stop  track=0004"));
    }

    #[test]
    fn fresh_run_clears_state_gate_and_keeps_durable_policy_state() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run(&r, Some("0069".into())).unwrap();
        stop(&r).unwrap();
        {
            let mut state = load_run_state(&r).unwrap();
            state.consecutive_failures.insert("0069".into(), 2);
            state.restored_epoch = Some(1);
            state.policy_approvals.push(crate::policy::PolicyApproval {
                name: crate::policy::NAME_DEPENDENCY.into(),
                fingerprint: "abc".into(),
                at: Utc::now(),
            });
            state.state_gate = Some(crate::policy::StateGate {
                name: crate::policy::NAME_DEPENDENCY.into(),
                action: "report".into(),
                detail: "policy: report dependency-manifest: Cargo.toml".into(),
            });
            state.self_check = Some(crate::state::SelfCheckState {
                steps: 3,
                pending_inject: true,
                ..crate::state::SelfCheckState::default()
            });
            save_run_state(&r, &state).unwrap();
        }
        let view = run(&r, Some("0069".into())).unwrap();
        assert!(view.state_gate.is_none());
        assert!(view.self_check.is_none());
        let state = load_run_state(&r).unwrap();
        assert_eq!(state.consecutive_failures.get("0069"), Some(&2));
        assert_eq!(state.restored_epoch, Some(1));
        assert_eq!(state.policy_approvals.len(), 1);
        assert!(state.state_gate.is_none());
        assert!(state.self_check.is_none());
        assert!(!r.self_continuation);
    }

    #[test]
    fn invalid_pause_from_idle() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        let err = pause(&r).unwrap_err();
        assert!(matches!(
            err,
            CoordinatorError::InvalidTransition {
                action: "pause",
                ..
            }
        ));
    }

    #[test]
    fn invalid_resume_from_running() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run(&r, None).unwrap();
        let err = resume(&r).unwrap_err();
        assert!(matches!(
            err,
            CoordinatorError::InvalidTransition {
                action: "resume",
                ..
            }
        ));
    }

    #[test]
    fn invalid_run_from_running() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run(&r, None).unwrap();
        let err = run(&r, None).unwrap_err();
        assert!(matches!(
            err,
            CoordinatorError::InvalidTransition { action: "run", .. }
        ));
    }

    #[test]
    fn idempotent_re_stop() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run(&r, None).unwrap();
        stop(&r).unwrap();
        let s = stop(&r).unwrap();
        assert_eq!(s.status, RunStatus::Stopped);
        assert_eq!(s.last_event, STOP_LAST_EVENT);
    }

    #[test]
    fn stop_from_idle_is_invalid() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        let err = stop(&r).unwrap_err();
        assert!(matches!(
            err,
            CoordinatorError::InvalidTransition { action: "stop", .. }
        ));
    }

    #[test]
    fn run_after_stop_restarts() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run(&r, None).unwrap();
        stop(&r).unwrap();
        let s = run(&r, None).unwrap();
        assert_eq!(s.status, RunStatus::Running);
    }

    #[test]
    fn run_stub_journals_start_clear_for_leftover() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        crate::state::ensure_state_dir(&r).unwrap();
        let mut state = crate::state::RunState::idle(&r.id);
        state.status = RunStatus::Stopped;
        state.failure_class = Some(crate::outcome::FailureClass::Timeout);
        crate::state::save_run_state(&r, &state).unwrap();
        run_stub(&r, None).unwrap();
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap();
        assert!(log.contains(crate::notify::START_CLEAR_DETAIL));
        assert!(!log.contains(crate::notify::SETTLED_DETAIL));
    }

    #[test]
    fn run_stub_clears_parked_next() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        crate::state::ensure_state_dir(&r).unwrap();
        let mut state = crate::state::RunState::idle(&r.id);
        state.parked_next = Some("0002".into());
        crate::state::save_run_state(&r, &state).unwrap();
        state.acp_stdout_retries = 2;
        crate::state::save_run_state(&r, &state).unwrap();
        let view = run_stub(&r, Some("0002".into())).unwrap();
        assert!(view.parked_next.is_none());
        assert_eq!(view.track_id.as_deref(), Some("0002"));
        assert_eq!(
            crate::state::load_run_state(&r).unwrap().acp_stdout_retries,
            0
        );
    }
}
