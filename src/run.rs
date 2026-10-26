//! Run state machine (ADR-0024 stop/pause; 0008 canonical DAG).
//!
//! Phase completion and timeouts are driven by the Phase Outcome apply path
//! ([`crate::outcome`]) and poll/wait ([`crate::watch`]).

use std::path::PathBuf;

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
    resume_repo: Option<PathBuf>,
}

enum RunPrep {
    Done(Box<StatusView>),
    NeedsProbe(RunProbeCtx),
    Second {
        adopt_addr: Option<String>,
        first_epoch: u64,
        new_epoch: u64,
        original: Box<RunState>,
        was_singular: bool,
        isolation: bool,
        view: Box<StatusView>,
    },
}

enum SecondRun {
    No,
    Yes { adopt_addr: Option<String> },
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
    let second = second_run_preflight(record)?;
    let prep = with_run_state_lock(record, || {
        prepare_run_locked(record, track_id, driver, picked, second)
    })?;
    match prep {
        RunPrep::Done(view) => Ok(*view),
        RunPrep::NeedsProbe(ctx) => {
            let answer = crate::workflow::reuse::probe_resume(
                record,
                ctx.number,
                ctx.resume_repo.as_deref(),
            );
            with_run_state_lock(record, || finish_probed_run(record, &ctx, &answer))
        }
        RunPrep::Second {
            adopt_addr,
            first_epoch,
            new_epoch,
            original,
            was_singular,
            isolation,
            view,
        } => finish_second_run(
            record,
            SecondFinish {
                adopt_addr: adopt_addr.as_deref(),
                first_epoch,
                new_epoch,
                original: &original,
                was_singular,
                isolation,
                view: *view,
            },
        ),
    }
}

fn second_run_preflight(record: &ProjectRecord) -> Result<SecondRun> {
    match crate::state::load_run_book(record)? {
        crate::state::RunBook::One(state)
            if matches!(state.status, RunStatus::Running | RunStatus::Paused) =>
        {
            let adopt_addr = if crate::worktree::isolation_enabled(record).unwrap_or(false) {
                crate::harness::pool::second_run_adopt_addr(record, state.run_epoch)?
            } else {
                None
            };
            Ok(SecondRun::Yes { adopt_addr })
        }
        crate::state::RunBook::Many(envelope)
            if envelope
                .runs
                .iter()
                .any(|entry| matches!(entry.status, RunStatus::Running | RunStatus::Paused)) =>
        {
            Ok(SecondRun::Yes { adopt_addr: None })
        }
        _ => Ok(SecondRun::No),
    }
}

struct SecondFinish<'a> {
    adopt_addr: Option<&'a str>,
    first_epoch: u64,
    new_epoch: u64,
    original: &'a RunState,
    was_singular: bool,
    isolation: bool,
    view: StatusView,
}

fn finish_second_run(record: &ProjectRecord, finish: SecondFinish<'_>) -> Result<StatusView> {
    let SecondFinish {
        adopt_addr,
        first_epoch,
        new_epoch,
        original,
        was_singular,
        isolation,
        view,
    } = finish;
    if let Some(addr) = adopt_addr
        && !crate::harness::pool::adopt_holder_epoch(addr, first_epoch).unwrap_or(false)
    {
        if crate::harness::pool::release_holder_epoch_rpc(addr).unwrap_or(false) && was_singular {
            let _ = with_run_state_lock(record, || {
                crate::harness::pool::restore_singular_sidecars(record, first_epoch)?;
                crate::state::collapse_to_singular(record, original)?;
                Ok(())
            });
            return Err(CoordinatorError::Message("adopt epoch failed".into()));
        }
        return Err(CoordinatorError::Message(
            "ReleaseEpoch failed; leaving the book".into(),
        ));
    }
    let prepared = with_run_state_lock(record, || {
        let _scope = crate::state::RunScope::enter(new_epoch)?;
        crate::worktree::prepare_epoch_in(record, new_epoch, isolation)
    });
    if let Err(err) = prepared {
        let released = match adopt_addr {
            Some(addr) => crate::harness::pool::release_holder_epoch_rpc(addr).unwrap_or(false),
            None => true,
        };
        if released && was_singular {
            let _ = with_run_state_lock(record, || {
                crate::harness::pool::restore_singular_sidecars(record, first_epoch)?;
                crate::state::collapse_to_singular(record, original)?;
                Ok(())
            });
        } else if released {
            let _ = with_run_state_lock(record, || {
                let crate::state::RunBook::Many(mut envelope) =
                    crate::state::load_run_book(record)?
                else {
                    return Ok(());
                };
                envelope.runs.retain(|entry| entry.run_epoch != new_epoch);
                if envelope.runs.len() >= 2 {
                    crate::state::write_run_book(record, &envelope)?;
                }
                Ok(())
            });
        }
        return Err(err);
    }
    Ok(view)
}

fn admit_second_run(
    record: &ProjectRecord,
    track_id: Option<String>,
    driver: WorkflowDriver,
    adopt_addr: Option<String>,
    isolation: bool,
) -> Result<RunPrep> {
    let book = crate::state::load_run_book(record)?;
    let floor = book.epoch_floor();
    let was_singular = !book.is_book();
    let mut entries = book.into_entries();
    for entry in &entries {
        if let Some(err) = blocked_hold(record, entry) {
            return Err(err);
        }
    }
    let track = track_id
        .as_deref()
        .map(str::trim)
        .filter(|track| !track.is_empty())
        .ok_or_else(|| CoordinatorError::Message("pass --track".into()))?
        .to_string();
    if entries.iter().any(|entry| {
        matches!(entry.status, RunStatus::Running | RunStatus::Paused)
            && entry.track_id.as_deref() == Some(track.as_str())
    }) {
        return Err(CoordinatorError::Message("track already running".into()));
    }
    let original = entries
        .iter()
        .find(|entry| matches!(entry.status, RunStatus::Running | RunStatus::Paused))
        .cloned()
        .ok_or_else(|| CoordinatorError::Message("pass --track".into()))?;
    if was_singular {
        crate::harness::pool::move_singular_sidecars(record, original.run_epoch)?;
    }
    let next = crate::state::next_run_epoch(&entries, floor);
    let mut created = RunState::idle(&record.id);
    created.run_epoch = next;
    created.status = RunStatus::Running;
    created.phase = workflow::graph::PHASE_PLAN.into();
    created.workflow = Some(WORKFLOW_ID.into());
    created.driver = driver;
    created.track_id = Some(track.clone());
    created.phase_started_at = Some(chrono::Utc::now());
    created.updated_at = chrono::Utc::now();
    created.last_event = format!("run: started {WORKFLOW_ID} track={track}");
    crate::state::copy_project_maps(&mut created, &original);
    entries.push(created.clone());
    let envelope = crate::state::RunEnvelope::new(floor, entries);
    if let Err(err) = crate::state::write_run_book(record, &envelope) {
        if was_singular {
            let _ = crate::harness::pool::restore_singular_sidecars(record, original.run_epoch);
        }
        return Err(err);
    }
    let _scope = crate::state::RunScope::enter(created.run_epoch)?;
    crate::workflow::trusted_evidence::write_for_epoch(
        record,
        &created,
        crate::workflow::trusted_evidence::ProducerHook::RunBeginFreshRun,
    );
    Ok(RunPrep::Second {
        adopt_addr: if was_singular { adopt_addr } else { None },
        first_epoch: original.run_epoch,
        new_epoch: created.run_epoch,
        original: Box::new(original),
        was_singular,
        isolation,
        view: Box::new(StatusView::from_record(record, &created)),
    })
}

/// 0098: a durable project-wide hold derived from a Blocked failure class.
///
/// The refusal names the held track, its current conductor row status (or `unknown`),
/// and the only release path. It never rewrites the row or any run state.
pub fn blocked_hold(record: &ProjectRecord, state: &RunState) -> Option<CoordinatorError> {
    if state.failure_class != Some(crate::outcome::FailureClass::Blocked) {
        return None;
    }
    let track = state
        .track_id
        .clone()
        .unwrap_or_else(|| "(none)".to_string());
    let row_status = crate::workflow::conductor_md::load_track_rows(record)
        .and_then(|rows| {
            rows.into_iter()
                .find(|row| crate::workflow::conductor_md::row_matches_track(row, &track))
        })
        .map(|row| crate::workflow::conductor_md::status_clean(&row.status_raw))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());
    Some(CoordinatorError::BlockedHold { track, row_status })
}

fn prepare_run_locked(
    record: &ProjectRecord,
    track_id: Option<String>,
    driver: WorkflowDriver,
    picked: bool,
    second: SecondRun,
) -> Result<RunPrep> {
    ensure_state_dir(record)?;
    if let SecondRun::Yes { adopt_addr } = &second
        && crate::state::any_entry_live(record)?
    {
        // The preflight read is stale. `COORDINATOR_WORKTREE` can change before
        // this lock, and isolation off must stay `InvalidTransition`.
        let isolation = crate::worktree::isolation_enabled(record)?;
        if isolation {
            return admit_second_run(record, track_id, driver, adopt_addr.clone(), isolation);
        }
        // A singular Running row keeps today's `from` (`RunStatus`'s display).
        // A live book cannot fall through: unscoped load is `ambiguous run`.
        if crate::state::load_run_book(record)?.is_book() {
            return Err(CoordinatorError::InvalidTransition {
                action: "run",
                from: "Running".into(),
            });
        }
    }
    let retain = match crate::state::load_run_book(record)? {
        crate::state::RunBook::Many(envelope) if !crate::state::any_entry_live(record)? => {
            envelope.runs.iter().map(|entry| entry.run_epoch).max()
        }
        _ => None,
    };
    let _retained = match retain {
        Some(epoch) => Some(crate::state::RunScope::enter(epoch)?),
        None => None,
    };
    let state = load_run_state(record)?;
    if let Some(err) = blocked_hold(record, &state) {
        return Err(err);
    }
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
                    repo,
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
                        resume_repo: repo,
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
    let _retained = if crate::state::load_run_book(record)?.is_book() {
        Some(crate::state::RunScope::enter(ctx.snap_epoch)?)
    } else {
        None
    };
    let mut state = load_run_state(record)?;
    if state.run_epoch != ctx.snap_epoch
        || state.track_id != ctx.snap_track
        || state.status != ctx.snap_status
    {
        return Ok(StatusView::from_record(record, &state));
    }
    // 0098: recheck the hold after the `gh` probe, before `begin_fresh_run`.
    if let Some(err) = blocked_hold(record, &state) {
        return Err(err);
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
    let next_epoch = crate::state::allocate_next_epoch(record, state)?;
    crate::checkpoint::reap_abandoned(record, state, next_epoch)?;
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
    state.run_epoch = next_epoch;
    crate::workflow::trusted_evidence::write_for_epoch(
        record,
        state,
        crate::workflow::trusted_evidence::ProducerHook::RunBeginFreshRun,
    );
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
    state.delivery_target = None;
    state.gate_target_journal = None;
    state.review = None;
    state.stalled_at = None;
    state.pause_spans.clear();
    state.stall_recycles = 0;
    state.aborted_session_id = None;
    state.acp_stdout_retries = 0;
    state.registry_status_before.clear();
    state.clear_plan_review_books();
    // 0119: a new epoch starts a new entry-id space.
    state.plan_review_entry = None;
    state.address_findings_attempts = 0;
    state.ci_fix_attempts = 0;
    state.ci_fix_request = None;
    state.review_fix_attempts = 0;
    state.review_fix_request = None;
    state.review_route = None;
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
        // G2: freeze a ci-wait resume's provenance from tick 0. A planning-only target is
        // stored too, so the first gate takes its explicit refusal arm. A resolution error
        // leaves `None` and journals; the first ci-wait gate emits the typed Permission.
        match crate::workflow::execution_repo::resolve_track_target(
            record,
            state,
            workflow::graph::PHASE_CI_WAIT,
        ) {
            Ok(target) => state.delivery_target = Some(target),
            Err(err) => crate::progress_log::append(
                record,
                "reuse",
                &format!("reuse: ci-wait target unresolved — {err}"),
            ),
        }
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
    crate::notify::clear_artifact(record, closed_epoch);
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
        if let Some(err) = blocked_hold(record, &state) {
            return Err(err);
        }
        match state.status {
            RunStatus::Idle | RunStatus::Stopped => {
                let closed_epoch = state.run_epoch;
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
                state.clear_plan_review_books();
                // 0119: a new epoch starts a new entry-id space.
                state.plan_review_entry = None;
                state.last_event = "run: started stub".into();
                state.updated_at = chrono::Utc::now();
                clear_active_outcome_file(record);
                crate::notify::clear_artifact(record, closed_epoch);
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

/// Which run a pause, resume, or stop command names.
///
/// [`Self::Bare`] is the no-selector path. It is not [`Self::All`]. There is no
/// `Default`: a caller that means every match passes [`Self::All`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunControl {
    Bare,
    Epoch(u64),
    Track(String),
    EpochAndTrack { epoch: u64, track: String },
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlVerb {
    Pause,
    Resume,
    Stop,
}

impl ControlVerb {
    fn action(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Stop => "stop",
        }
    }

    fn matches(self, status: RunStatus) -> bool {
        match self {
            Self::Pause => status == RunStatus::Running,
            Self::Resume => status == RunStatus::Paused,
            Self::Stop => matches!(status, RunStatus::Running | RunStatus::Paused),
        }
    }
}

enum ControlPick {
    Apply(Vec<usize>),
    /// Stop of an entry that is already `Stopped`.
    IdempotentStop(usize),
    NoMatch,
}

fn describe_entries(entries: &[RunState], indices: &[usize]) -> String {
    indices
        .iter()
        .map(|index| {
            let entry = &entries[*index];
            format!(
                "epoch {} track {}",
                entry.run_epoch,
                entry.track_id.as_deref().unwrap_or("-")
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn ambiguous_control(
    verb: ControlVerb,
    entries: &[RunState],
    indices: &[usize],
) -> CoordinatorError {
    CoordinatorError::AmbiguousControlTarget {
        action: verb.action(),
        matches: describe_entries(entries, indices),
    }
}

fn disagree_control(verb: ControlVerb, detail: String) -> CoordinatorError {
    CoordinatorError::AmbiguousControlTarget {
        action: verb.action(),
        matches: detail,
    }
}

fn pick_named(entries: &[RunState], verb: ControlVerb, index: usize) -> Result<ControlPick> {
    let entry = &entries[index];
    if verb == ControlVerb::Stop && entry.status == RunStatus::Stopped {
        return Ok(ControlPick::IdempotentStop(index));
    }
    if !verb.matches(entry.status) {
        return Err(CoordinatorError::InvalidTransition {
            action: verb.action(),
            from: entry.status.to_string(),
        });
    }
    Ok(ControlPick::Apply(vec![index]))
}

fn pick_control(
    entries: &[RunState],
    verb: ControlVerb,
    target: &RunControl,
) -> Result<ControlPick> {
    match target {
        RunControl::Bare | RunControl::All => {
            let hits: Vec<usize> = entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| verb.matches(entry.status))
                .map(|(index, _)| index)
                .collect();
            if matches!(target, RunControl::Bare) && hits.len() > 1 {
                return Err(ambiguous_control(verb, entries, &hits));
            }
            if hits.is_empty() {
                Ok(ControlPick::NoMatch)
            } else {
                Ok(ControlPick::Apply(hits))
            }
        }
        RunControl::Epoch(epoch) => {
            match entries.iter().position(|entry| entry.run_epoch == *epoch) {
                Some(index) => pick_named(entries, verb, index),
                None => Err(crate::state::unknown_run_epoch_error(*epoch)),
            }
        }
        RunControl::Track(track) => {
            let same: Vec<usize> = entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.track_id.as_deref() == Some(track.as_str()))
                .map(|(index, _)| index)
                .collect();
            if same.len() > 1 {
                return Err(ambiguous_control(verb, entries, &same));
            }
            let Some(index) = same.first().copied() else {
                return Err(CoordinatorError::InvalidTransition {
                    action: verb.action(),
                    from: format!("no such track {track}"),
                });
            };
            pick_named(entries, verb, index)
        }
        RunControl::EpochAndTrack { epoch, track } => {
            let Some(index) = entries.iter().position(|entry| entry.run_epoch == *epoch) else {
                return Err(disagree_control(
                    verb,
                    format!("epoch {epoch} disagrees with track {track}"),
                ));
            };
            let found = entries[index].track_id.as_deref().unwrap_or("-");
            if found != track {
                return Err(disagree_control(
                    verb,
                    format!("epoch {epoch} track {found} disagrees with track {track}"),
                ));
            }
            pick_named(entries, verb, index)
        }
    }
}

fn pause_entry(entry: &mut RunState, now: chrono::DateTime<chrono::Utc>) {
    entry.status = RunStatus::Paused;
    entry.pause_started_at = Some(now);
    entry.last_event = "pause: hold".into();
    entry.updated_at = now;
}

/// Apply `pause` with no selector. One matching run, or a refusal. Not [`RunControl::All`].
pub fn pause(record: &ProjectRecord) -> Result<StatusView> {
    pause_target(record, RunControl::Bare)
}

/// Apply `pause` to `target`. Sends no Shutdown.
pub fn pause_target(record: &ProjectRecord, target: RunControl) -> Result<StatusView> {
    with_run_state_lock(record, || {
        ensure_state_dir(record)?;
        match crate::state::load_run_book(record)? {
            crate::state::RunBook::Many(mut envelope) => {
                match pick_control(&envelope.runs, ControlVerb::Pause, &target)? {
                    ControlPick::NoMatch => Err(CoordinatorError::InvalidTransition {
                        action: "pause",
                        from: "not running".into(),
                    }),
                    ControlPick::IdempotentStop(_) => Err(CoordinatorError::InvalidTransition {
                        action: "pause",
                        from: RunStatus::Stopped.to_string(),
                    }),
                    ControlPick::Apply(indices) => {
                        let now = chrono::Utc::now();
                        for index in indices {
                            pause_entry(&mut envelope.runs[index], now);
                        }
                        crate::state::write_run_book(record, &envelope)?;
                        Ok(compose_book_status(record, &envelope))
                    }
                }
            }
            crate::state::RunBook::One(mut state) => {
                match pick_control(std::slice::from_ref(&state), ControlVerb::Pause, &target)? {
                    ControlPick::Apply(_) => {
                        pause_entry(&mut state, chrono::Utc::now());
                        save_run_state(record, &state)?;
                        Ok(StatusView::from_record(record, &state))
                    }
                    ControlPick::NoMatch | ControlPick::IdempotentStop(_) => {
                        Err(CoordinatorError::InvalidTransition {
                            action: "pause",
                            from: state.status.to_string(),
                        })
                    }
                }
            }
        }
    })
}

enum ResumePrep {
    Done(Box<StatusView>),
    Probe(crate::workflow::reuse::ReuseProbe),
}

/// Apply `resume` with no selector. One paused run, or a refusal. Not [`RunControl::All`].
///
/// If the phase already completed/failed while held, release to Idle instead of
/// re-entering `Running` without a phase clock (would hang forever under autonomy).
/// An advance successor that still needs `gh pr view` is probed after the lock drops.
/// `commit_probed_successor` runs inside `RunScope` for `probe.snap_epoch`: the scope
/// held by `resume_locked` drops when that function returns.
pub fn resume(record: &ProjectRecord) -> Result<StatusView> {
    resume_target(record, RunControl::Bare)
}

/// Apply `resume` to `target`.
pub fn resume_target(record: &ProjectRecord, target: RunControl) -> Result<StatusView> {
    let parts = with_run_state_lock(record, || resume_locked(record, &target))?;
    let mut last = None;
    for part in parts {
        last = Some(match part {
            ResumePrep::Done(view) => *view,
            ResumePrep::Probe(probe) => {
                let answer = crate::workflow::reuse::probe_resume(
                    record,
                    probe.number,
                    probe.repo.as_deref(),
                );
                let _scope = crate::state::RunScope::enter(probe.snap_epoch)?;
                with_run_state_lock(record, || {
                    crate::workflow::commit_probed_successor(record, &probe, &answer)
                })?
            }
        });
    }
    last.ok_or_else(|| CoordinatorError::InvalidTransition {
        action: "resume",
        from: "not paused".into(),
    })
}

fn resume_locked(record: &ProjectRecord, target: &RunControl) -> Result<Vec<ResumePrep>> {
    ensure_state_dir(record)?;
    match crate::state::load_run_book(record)? {
        crate::state::RunBook::Many(envelope) => {
            match pick_control(&envelope.runs, ControlVerb::Resume, target)? {
                ControlPick::NoMatch => Err(CoordinatorError::InvalidTransition {
                    action: "resume",
                    from: "not paused".into(),
                }),
                ControlPick::IdempotentStop(_) => Err(CoordinatorError::InvalidTransition {
                    action: "resume",
                    from: RunStatus::Stopped.to_string(),
                }),
                ControlPick::Apply(indices) => {
                    let epochs: Vec<u64> = indices
                        .iter()
                        .map(|index| envelope.runs[*index].run_epoch)
                        .collect();
                    let mut parts = Vec::with_capacity(epochs.len());
                    for epoch in epochs {
                        let _scope = crate::state::RunScope::enter(epoch)?;
                        parts.push(resume_scoped(record)?);
                    }
                    Ok(parts)
                }
            }
        }
        crate::state::RunBook::One(state) => {
            match pick_control(std::slice::from_ref(&state), ControlVerb::Resume, target)? {
                ControlPick::Apply(_) => Ok(vec![resume_scoped(record)?]),
                ControlPick::NoMatch | ControlPick::IdempotentStop(_) => {
                    Err(CoordinatorError::InvalidTransition {
                        action: "resume",
                        from: state.status.to_string(),
                    })
                }
            }
        }
    }
}

fn resume_scoped(record: &ProjectRecord) -> Result<ResumePrep> {
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

struct StopPrep {
    view: StatusView,
    /// Epochs this call transitioned to `Stopped`. Stall close is scoped to these.
    transitioned: Vec<u64>,
    /// Epochs that receive `shutdown_epoch`. Empty is not "every session is gone".
    shutdown: Vec<u64>,
    /// Collapse only after every entry is `Stopped` and every session is gone.
    try_collapse: bool,
}

/// Apply `stop` with no selector. One matching run, or a refusal. Not [`RunControl::All`].
///
/// 0104: an operator stop also closes an open stall page (one `run_ended` clear).
/// The close runs **after** the run-state lock is released and only on the `Ok`
/// path, inside `RunScope` for each epoch this call transitioned. Its POST is
/// **synchronous** (bounded by `POST_TIMEOUT`, 5 s). A failure is logged and not
/// retried. A refusal returns before any close.
pub fn stop(record: &ProjectRecord) -> Result<StatusView> {
    stop_target(record, RunControl::Bare)
}

/// Apply `stop` to `target`.
pub fn stop_target(record: &ProjectRecord, target: RunControl) -> Result<StatusView> {
    let prep = with_run_state_lock(record, || stop_locked(record, &target))?;
    if prep.shutdown.is_empty() && prep.transitioned.is_empty() {
        return Ok(prep.view);
    }
    let mut shutdown_ok = Vec::with_capacity(prep.shutdown.len());
    for epoch in &prep.shutdown {
        let ok = crate::harness::pool::shutdown_epoch(record, *epoch).unwrap_or(false);
        shutdown_ok.push((*epoch, ok));
    }
    // Close each transitioned page while the book still has that entry.
    for epoch in &prep.transitioned {
        let _scope = crate::state::RunScope::enter(*epoch)?;
        crate::workflow::stall_paging::tick_blocking(record);
    }
    if prep.try_collapse && stop_sessions_clear(record, &shutdown_ok)? {
        return with_run_state_lock(record, || collapse_stopped_book(record));
    }
    if prep.try_collapse {
        return status(record);
    }
    Ok(prep.view)
}

fn stop_sessions_clear(record: &ProjectRecord, shutdown_ok: &[(u64, bool)]) -> Result<bool> {
    let crate::state::RunBook::Many(envelope) = crate::state::load_run_book(record)? else {
        return Ok(false);
    };
    if !envelope
        .runs
        .iter()
        .all(|entry| entry.status == RunStatus::Stopped)
    {
        return Ok(false);
    }
    Ok(envelope.runs.iter().all(|entry| {
        match shutdown_ok
            .iter()
            .find(|(epoch, _)| *epoch == entry.run_epoch)
        {
            Some((_, ok)) => *ok,
            None => crate::harness::pool::session_is_gone(record, entry.run_epoch),
        }
    }))
}

/// Read status without mutation. A book fills `runs` and the lowest epoch's top-level fields.
pub fn status(record: &ProjectRecord) -> Result<StatusView> {
    match crate::state::load_run_book(record)? {
        crate::state::RunBook::One(state) => Ok(StatusView::from_record(record, &state)),
        crate::state::RunBook::Many(envelope) => Ok(compose_book_status(record, &envelope)),
    }
}

fn compose_book_status(record: &ProjectRecord, envelope: &crate::state::RunEnvelope) -> StatusView {
    let lowest = envelope
        .runs
        .iter()
        .min_by_key(|entry| entry.run_epoch)
        .cloned()
        .unwrap_or_else(|| RunState::idle(&record.id));
    let _scope = crate::state::RunScope::enter(lowest.run_epoch).ok();
    let mut view = StatusView::from_record(record, &lowest);
    view.runs = envelope
        .runs
        .iter()
        .map(|entry| {
            let mut summary = crate::state::RunSummary::from_state(entry);
            summary.failure_artifact = crate::notify::artifact::run_path(record, entry.run_epoch)
                .ok()
                .filter(|path| path.is_file());
            summary.failure_superseded = crate::state::failure_superseded_for(record, entry);
            summary
        })
        .collect();
    view
}

fn mark_stopped(record: &ProjectRecord, entry: &mut RunState, now: chrono::DateTime<chrono::Utc>) {
    entry.status = RunStatus::Stopped;
    entry.phase = STUB_PHASE_STOPPED.into();
    entry.last_event = STOP_LAST_EVENT.into();
    entry.phase_started_at = None;
    entry.pause_started_at = None;
    entry.updated_at = now;
    let track = entry.track_id.as_deref().unwrap_or("-");
    crate::progress_log::append(
        record,
        "stop",
        &format!("track={track} phase={}  {}", entry.phase, entry.last_event),
    );
}

fn note_idempotent_stop(entry: &mut RunState) -> bool {
    if entry.last_event != STOP_LAST_EVENT {
        entry.last_event = STOP_LAST_EVENT.into();
        entry.updated_at = chrono::Utc::now();
        true
    } else {
        false
    }
}

fn stop_locked(record: &ProjectRecord, target: &RunControl) -> Result<StopPrep> {
    ensure_state_dir(record)?;
    match crate::state::load_run_book(record)? {
        crate::state::RunBook::Many(mut envelope) => stop_book(record, target, &mut envelope),
        crate::state::RunBook::One(mut state) => stop_one(record, target, &mut state),
    }
}

fn stop_one(record: &ProjectRecord, target: &RunControl, state: &mut RunState) -> Result<StopPrep> {
    match pick_control(std::slice::from_ref(state), ControlVerb::Stop, target)? {
        ControlPick::Apply(_) => {
            let epoch = state.run_epoch;
            mark_stopped(record, state, chrono::Utc::now());
            save_run_state(record, state)?;
            Ok(StopPrep {
                view: StatusView::from_record(record, state),
                transitioned: vec![epoch],
                shutdown: vec![epoch],
                try_collapse: false,
            })
        }
        ControlPick::IdempotentStop(_) | ControlPick::NoMatch
            if state.status == RunStatus::Stopped =>
        {
            if note_idempotent_stop(state) {
                save_run_state(record, state)?;
            }
            Ok(StopPrep {
                view: StatusView::from_record(record, state),
                transitioned: Vec::new(),
                shutdown: Vec::new(),
                try_collapse: false,
            })
        }
        ControlPick::NoMatch | ControlPick::IdempotentStop(_) => {
            Err(CoordinatorError::InvalidTransition {
                action: "stop",
                from: state.status.to_string(),
            })
        }
    }
}

fn stop_book(
    record: &ProjectRecord,
    target: &RunControl,
    envelope: &mut crate::state::RunEnvelope,
) -> Result<StopPrep> {
    match pick_control(&envelope.runs, ControlVerb::Stop, target)? {
        ControlPick::NoMatch if matches!(target, RunControl::All) && book_all_stopped(envelope) => {
            Ok(StopPrep {
                view: compose_book_status(record, envelope),
                transitioned: Vec::new(),
                shutdown: envelope.runs.iter().map(|entry| entry.run_epoch).collect(),
                try_collapse: true,
            })
        }
        ControlPick::NoMatch => Ok(StopPrep {
            view: compose_book_status(record, envelope),
            transitioned: Vec::new(),
            shutdown: Vec::new(),
            try_collapse: false,
        }),
        ControlPick::IdempotentStop(index) => {
            if note_idempotent_stop(&mut envelope.runs[index]) {
                crate::state::write_run_book(record, envelope)?;
            }
            Ok(StopPrep {
                view: compose_book_status(record, envelope),
                transitioned: Vec::new(),
                shutdown: Vec::new(),
                try_collapse: false,
            })
        }
        ControlPick::Apply(indices) => {
            let epochs: Vec<u64> = indices
                .iter()
                .map(|index| envelope.runs[*index].run_epoch)
                .collect();
            let now = chrono::Utc::now();
            for index in indices {
                mark_stopped(record, &mut envelope.runs[index], now);
            }
            crate::state::write_run_book(record, envelope)?;
            let try_collapse = book_all_stopped(envelope);
            Ok(StopPrep {
                view: compose_book_status(record, envelope),
                transitioned: epochs.clone(),
                shutdown: epochs,
                try_collapse,
            })
        }
    }
}

fn book_all_stopped(envelope: &crate::state::RunEnvelope) -> bool {
    !envelope.runs.is_empty()
        && envelope
            .runs
            .iter()
            .all(|entry| entry.status == RunStatus::Stopped)
}

fn collapse_stopped_book(record: &ProjectRecord) -> Result<StatusView> {
    let crate::state::RunBook::Many(envelope) = crate::state::load_run_book(record)? else {
        return status(record);
    };
    let kept_epoch = envelope
        .runs
        .iter()
        .map(|entry| entry.run_epoch)
        .max()
        .unwrap_or(0);
    let dropped_epochs: Vec<u64> = envelope
        .runs
        .iter()
        .filter(|entry| entry.run_epoch != kept_epoch)
        .map(|entry| entry.run_epoch)
        .collect();
    let dropped = dropped_epochs.iter().copied().max().unwrap_or(0);
    let mut kept = envelope
        .runs
        .into_iter()
        .find(|entry| entry.run_epoch == kept_epoch)
        .ok_or_else(|| CoordinatorError::Message("run book requires two entries".into()))?;
    kept.epoch_floor = envelope.epoch_floor.max(dropped);
    kept.status = RunStatus::Stopped;
    if crate::harness::pool::restore_singular_sidecars(record, kept.run_epoch).is_err() {
        return status(record);
    }
    crate::workflow::trusted_evidence::restore_current_leaf(record, kept.run_epoch);
    crate::state::collapse_to_singular(record, &kept)?;
    remove_dropped_session_persists(record, &dropped_epochs);
    Ok(StatusView::from_record(record, &kept))
}

fn remove_dropped_session_persists(record: &ProjectRecord, epochs: &[u64]) {
    let Ok(dir) = crate::state::resolve_state_dir(record) else {
        return;
    };
    for epoch in epochs {
        let session = dir.join("sessions").join(epoch.to_string());
        let _ = std::fs::remove_file(session.join("harness-grok.json"));
        let _ = std::fs::remove_dir(&session);
    }
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
            review_comment_routing: false,
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: Utc::now(),
        }
    }

    fn blocked_state(r: &ProjectRecord, track: &str) {
        let mut state = load_run_state(r).unwrap();
        state.status = RunStatus::Stopped;
        state.phase = crate::workflow::graph::PHASE_PLAN.into();
        state.track_id = Some(track.into());
        state.driver = WorkflowDriver::Adapter;
        state.failure_class = Some(crate::outcome::FailureClass::Blocked);
        state.last_event = "outcome: failure class=blocked phase=plan source=adapter".into();
        save_run_state(r, &state).unwrap();
    }

    #[test]
    fn blocked_hold_refuses_locked_preparation_probe_and_test_stub() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        blocked_state(&r, "0098");
        let before = load_run_state(&r).unwrap();

        match prepare_run_locked(
            &r,
            Some("0098".into()),
            WorkflowDriver::Stub,
            false,
            SecondRun::No,
        ) {
            Err(CoordinatorError::BlockedHold { track, .. }) => assert_eq!(track, "0098"),
            Ok(_) => panic!("expected BlockedHold"),
            Err(other) => panic!("expected BlockedHold, got {other}"),
        }
        match run_stub(&r, Some("0098".into())) {
            Err(CoordinatorError::BlockedHold { .. }) => {}
            other => panic!("expected BlockedHold, got {other:?}"),
        }
        let ctx = RunProbeCtx {
            snap_epoch: before.run_epoch,
            snap_track: before.track_id.clone(),
            snap_status: before.status,
            track_arg: Some("0098".into()),
            driver: WorkflowDriver::Stub,
            picked: false,
            number: 1,
            head_sha: "deadbeef".into(),
            effective_track: "0098".into(),
            resume_repo: None,
        };
        let err = finish_probed_run(&r, &ctx, &Ok(None)).unwrap_err();
        assert!(matches!(err, CoordinatorError::BlockedHold { .. }));

        let after = load_run_state(&r).unwrap();
        assert_eq!(after.status, before.status);
        assert_eq!(after.phase, before.phase);
        assert_eq!(after.run_epoch, before.run_epoch);
        assert_eq!(after.failure_class, before.failure_class);
    }

    #[test]
    fn restop_replaces_last_event_but_retains_blocked_class_and_artifact() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        blocked_state(&r, "0098");
        let artifact = crate::notify::artifact::path(&r).unwrap();
        std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        std::fs::write(
            &artifact,
            "# Coordinator failure\n- failure_class: blocked\n",
        )
        .unwrap();
        let view = stop(&r).unwrap();
        assert_eq!(view.last_event, STOP_LAST_EVENT);
        assert_eq!(
            view.failure_class,
            Some(crate::outcome::FailureClass::Blocked),
            "stop is not an acknowledgment"
        );
        assert!(artifact.is_file(), "re-stop keeps the artifact and hold");
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

    fn fresh_ci_fixture(cell: &str) -> (tempfile::TempDir, tempfile::TempDir, ProjectRecord) {
        let ws = tempdir().unwrap();
        let repo = tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = crate::git_command::command()
                .args(args)
                .current_dir(repo.path())
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        git(&["init", "-b", "main"]);
        std::fs::write(repo.path().join("f.txt"), "base\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "base"]);
        let conductor = ws.path().join("conductor");
        std::fs::create_dir_all(conductor.join("0101-Example")).unwrap();
        std::fs::write(
            conductor.join("conductor.md"),
            format!(
                "| Track | Execution | Status | Summary |\n| --- | --- | --- | --- |\n| [0101-Example](0101-Example/spec.md) | {cell} | **Ready - not started** | s |\n"
            ),
        )
        .unwrap();
        let mut r = rec(ws.path());
        r.execution_repo = Some(repo.path().to_path_buf());
        (ws, repo, r)
    }

    #[test]
    fn ci_wait_resume_seeds_the_delivery_target() {
        let (ws, repo, r) = fresh_ci_fixture("`not-a-target`");
        std::fs::write(
            ws.path().join("conductor").join("conductor.md"),
            format!(
                "| Track | Execution | Status | Summary |\n| --- | --- | --- | --- |\n| [0101-Example](0101-Example/spec.md) | `{}` | **Ready - not started** | s |\n",
                repo.path().display()
            ),
        )
        .unwrap();
        let mut state = RunState::idle(&r.id);
        let view = with_run_state_lock(&r, || {
            begin_fresh_run(
                &r,
                &mut state,
                FreshRun {
                    track_arg: Some("0101".into()),
                    driver: WorkflowDriver::Adapter,
                    picked: false,
                    archive_slugs: Vec::new(),
                    journal_unreadable: false,
                    ci_wait: Some((7, "abc".into())),
                },
            )
        })
        .unwrap();
        assert_eq!(view.phase, crate::workflow::graph::PHASE_CI_WAIT);
        let saved = load_run_state(&r).unwrap();
        assert!(saved.delivery_target.as_ref().is_some_and(|target| {
            crate::workflow::execution_repo::paths_same(&target.declared_path, repo.path())
        }));
    }

    #[test]
    fn ci_wait_resume_unresolved_keeps_none_and_journals() {
        let (_ws, _repo, r) = fresh_ci_fixture("`not-a-target`");
        let mut state = RunState::idle(&r.id);
        with_run_state_lock(&r, || {
            begin_fresh_run(
                &r,
                &mut state,
                FreshRun {
                    track_arg: Some("0101".into()),
                    driver: WorkflowDriver::Adapter,
                    picked: false,
                    archive_slugs: Vec::new(),
                    journal_unreadable: false,
                    ci_wait: Some((7, "abc".into())),
                },
            )
        })
        .unwrap();
        let saved = load_run_state(&r).unwrap();
        assert!(saved.delivery_target.is_none());
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap_or_default();
        assert!(log.contains("reuse: ci-wait target unresolved"), "{log}");
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
            state
                .registry_status_before
                .insert("0069".into(), "**Ready — not started**".into());
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
        assert!(state.registry_status_before.is_empty());
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
    fn locked_prepare_rereads_isolation_before_admit() {
        let _guard = crate::config::test_env_lock();
        let dir = tempdir().unwrap();
        let mut r = rec(dir.path());
        r.worktree_isolation = true;
        let mut first = crate::state::RunState::idle(&r.id);
        first.run_epoch = 2;
        first.status = RunStatus::Running;
        first.track_id = Some("A".into());
        let mut second = crate::state::RunState::idle(&r.id);
        second.run_epoch = 4;
        second.status = RunStatus::Running;
        second.track_id = Some("B".into());
        crate::state::write_run_book(&r, &crate::state::RunEnvelope::new(0, vec![first, second]))
            .unwrap();
        unsafe {
            std::env::set_var(crate::worktree::ENV_WORKTREE, "off");
        }
        struct ClearEnv;
        impl Drop for ClearEnv {
            fn drop(&mut self) {
                unsafe {
                    std::env::remove_var(crate::worktree::ENV_WORKTREE);
                }
            }
        }
        let _clear = ClearEnv;
        let prepared = with_run_state_lock(&r, || {
            prepare_run_locked(
                &r,
                Some("C".into()),
                WorkflowDriver::Stub,
                false,
                SecondRun::Yes { adopt_addr: None },
            )
        });
        assert!(
            matches!(
                prepared,
                Err(CoordinatorError::InvalidTransition { action: "run", .. })
            ),
            "isolation off admitted a second run"
        );
        let crate::state::RunBook::Many(envelope) = crate::state::load_run_book(&r).unwrap() else {
            panic!("isolation off collapsed the book");
        };
        assert_eq!(envelope.runs.len(), 2);
    }

    #[test]
    fn isolation_off_live_book_is_invalid_transition() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        let mut first = crate::state::RunState::idle(&r.id);
        first.run_epoch = 2;
        first.status = RunStatus::Running;
        first.track_id = Some("A".into());
        let mut second = crate::state::RunState::idle(&r.id);
        second.run_epoch = 4;
        second.status = RunStatus::Running;
        second.track_id = Some("B".into());
        crate::state::write_run_book(&r, &crate::state::RunEnvelope::new(0, vec![first, second]))
            .unwrap();
        let err = run(&r, Some("C".into())).unwrap_err();
        match err {
            CoordinatorError::InvalidTransition { action: "run", .. } => {}
            other => panic!("expected InvalidTransition, got {other}"),
        }
        let crate::state::RunBook::Many(envelope) = crate::state::load_run_book(&r).unwrap() else {
            panic!("isolation off collapsed the book");
        };
        assert_eq!(envelope.runs.len(), 2);
    }

    #[test]
    fn bare_run_retains_the_greatest_idle_epoch() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        let mut low = crate::state::RunState::idle(&r.id);
        low.run_epoch = 2;
        low.status = RunStatus::Stopped;
        low.track_id = Some("A".into());
        let mut high = crate::state::RunState::idle(&r.id);
        high.run_epoch = 9;
        high.status = RunStatus::Idle;
        high.track_id = Some("B".into());
        crate::state::write_run_book(&r, &crate::state::RunEnvelope::new(0, vec![low, high]))
            .unwrap();
        let view = run(&r, None).unwrap();
        assert_eq!(
            view.run_epoch, 10,
            "restart did not continue the greatest epoch"
        );
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.track_id.as_deref(), Some("B"));
        let crate::state::RunBook::Many(envelope) = crate::state::load_run_book(&r).unwrap() else {
            panic!("bare run collapsed the book");
        };
        let low = envelope
            .runs
            .iter()
            .find(|entry| entry.run_epoch == 2)
            .unwrap();
        assert_eq!(low.status, RunStatus::Stopped);
        assert_eq!(low.track_id.as_deref(), Some("A"));
        assert!(envelope.runs.iter().any(|entry| {
            entry.run_epoch == 10
                && entry.status == RunStatus::Running
                && entry.track_id.as_deref() == Some("B")
        }));
        assert_eq!(envelope.runs.len(), 2);
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

    #[test]
    fn adopt_timeout_sends_release() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        crate::state::ensure_state_dir(&r).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_t = seen.clone();
        std::thread::spawn(move || {
            for n in 0..2 {
                let Ok((stream, _)) = listener.accept() else {
                    break;
                };
                let mut writer = stream.try_clone().unwrap();
                let mut reader = std::io::BufReader::new(stream);
                let mut line = String::new();
                let _ = std::io::BufRead::read_line(&mut reader, &mut line);
                seen_t.lock().unwrap().push(line);
                if n == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(1200));
                } else {
                    let _ = std::io::Write::write_all(&mut writer, b"{\"ok\":false}\n");
                }
            }
        });
        let mut first = crate::state::RunState::idle(&r.id);
        first.run_epoch = 3;
        first.status = RunStatus::Running;
        let mut second = crate::state::RunState::idle(&r.id);
        second.run_epoch = 4;
        second.status = RunStatus::Running;
        crate::state::write_run_book(
            &r,
            &crate::state::RunEnvelope::new(0, vec![first.clone(), second.clone()]),
        )
        .unwrap();
        let view = crate::state::StatusView::from_record(&r, &second);
        let err = finish_second_run(
            &r,
            SecondFinish {
                adopt_addr: Some(&addr),
                first_epoch: 3,
                new_epoch: 4,
                original: &first,
                was_singular: true,
                isolation: true,
                view,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("ReleaseEpoch failed"), "{err}");
        let lines = seen.lock().unwrap().clone();
        assert!(lines[0].contains("adopt_epoch"), "{lines:?}");
        assert!(lines[1].contains("release_epoch"), "{lines:?}");
        assert!(matches!(
            crate::state::load_run_book(&r).unwrap(),
            crate::state::RunBook::Many(_)
        ));
    }

    #[test]
    fn second_run_leaves_sibling_sidecars() {
        let dir = tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = crate::git_command::command()
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?} {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.path().join("README.md"), b"seed\n").unwrap();
        git(&["add", "README.md"]);
        git(&["commit", "-m", "seed"]);
        let mut r = rec(dir.path());
        r.worktree_isolation = true;
        r.execution_repo = Some(dir.path().to_path_buf());
        run_with_driver(&r, Some("A".into()), WorkflowDriver::Stub).unwrap();
        let first = crate::state::load_run_state(&r).unwrap();
        let epoch = first.run_epoch;
        crate::state::ensure_state_dir(&r).unwrap();
        let state_dir = crate::state::resolve_state_dir(&r).unwrap();
        std::fs::write(state_dir.join("harness-progress.json"), b"progress-a").unwrap();
        std::fs::create_dir_all(state_dir.join("outcomes")).unwrap();
        std::fs::write(
            state_dir.join("outcomes").join("current.json"),
            b"outcome-a",
        )
        .unwrap();
        let report = "## Verdict: PASS\n\n## Findings\n\nnone\n";
        crate::review::write_reports(&r, &first, "codex", report).unwrap();
        let review = state_dir.join("reviews").join("cross-model-codex.md");
        let review_bytes = std::fs::read(&review).unwrap();
        assert!(
            std::str::from_utf8(&review_bytes)
                .unwrap()
                .contains("Verdict: PASS"),
            "production writer did not leave a cross-model report"
        );
        std::fs::create_dir_all(state_dir.join("trusted-evidence")).unwrap();
        std::fs::write(
            state_dir.join("trusted-evidence").join("current.json"),
            b"evidence-a",
        )
        .unwrap();
        let failure = format!("- track_id: A\n- run_epoch: {epoch}\n");
        std::fs::write(state_dir.join("FAILURE.md"), &failure).unwrap();
        let ref_name = format!("refs/coordinator/checkpoints/{}/{epoch}", r.id);
        git(&["update-ref", &ref_name, "HEAD"]);
        run_with_driver(&r, Some("B".into()), WorkflowDriver::Stub).unwrap();
        let epoch_name = epoch.to_string();
        assert_eq!(
            std::fs::read(
                state_dir
                    .join("harness-progress")
                    .join(format!("{epoch_name}.json"))
            )
            .unwrap(),
            b"progress-a"
        );
        assert_eq!(
            std::fs::read(
                state_dir
                    .join("outcomes")
                    .join(&epoch_name)
                    .join("current.json")
            )
            .unwrap(),
            b"outcome-a"
        );
        assert_eq!(
            std::fs::read(
                state_dir
                    .join("reviews")
                    .join(&epoch_name)
                    .join("cross-model-codex.md")
            )
            .unwrap(),
            review_bytes
        );
        assert_eq!(
            std::fs::read(
                state_dir
                    .join("trusted-evidence")
                    .join(format!("{epoch_name}.json"))
            )
            .unwrap(),
            b"evidence-a"
        );
        assert_eq!(
            std::fs::read(state_dir.join("FAILURE.md")).unwrap(),
            failure.as_bytes()
        );
        let shown = crate::git_command::command()
            .args(["show-ref", &ref_name])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(shown.status.success(), "A's checkpoint ref was removed");
        let book = crate::state::load_run_book(&r).unwrap();
        let crate::state::RunBook::Many(envelope) = book else {
            panic!("public run did not admit the second track");
        };
        assert!(envelope.runs.iter().any(|entry| {
            entry.track_id.as_deref() == Some("A") && entry.status == RunStatus::Running
        }));
        assert!(envelope.runs.iter().any(|entry| {
            entry.track_id.as_deref() == Some("B") && entry.status == RunStatus::Running
        }));
    }

    fn pair(id: &str, epoch: u64, status: RunStatus, track: &str) -> RunState {
        let mut state = RunState::idle(id);
        state.run_epoch = epoch;
        state.status = status;
        state.track_id = Some(track.into());
        state.phase = "plan".into();
        state.last_event = format!("seed {epoch}");
        state
    }

    fn write_pair(r: &ProjectRecord, left: RunState, right: RunState) {
        ensure_state_dir(r).unwrap();
        crate::state::write_run_book(r, &crate::state::RunEnvelope::new(0, vec![left, right]))
            .unwrap();
    }

    fn book_runs(r: &ProjectRecord) -> Vec<RunState> {
        match crate::state::load_run_book(r).unwrap() {
            crate::state::RunBook::Many(envelope) => envelope.runs,
            crate::state::RunBook::One(state) => panic!("expected a book, got {state:?}"),
        }
    }

    fn assert_control_refusal(err: &CoordinatorError) {
        assert!(
            matches!(err, CoordinatorError::AmbiguousControlTarget { .. }),
            "{err:?}"
        );
        assert_eq!(err.exit_code(), 2);
        assert_eq!(err.http_status(), 409);
        assert!(!matches!(err, CoordinatorError::Message(_)));
        assert!(!matches!(err, CoordinatorError::AmbiguousRunTrack { .. }));
        assert_ne!(
            err.to_string(),
            crate::state::ambiguous_run_error().to_string()
        );
        assert!(!err.to_string().contains("two paused"), "{err}");
    }

    #[test]
    fn bare_controls_refuse_two_matches_without_a_write_or_stall_close() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        let left = pair(&r.id, 3, RunStatus::Running, "A");
        let right = pair(&r.id, 4, RunStatus::Running, "B");
        write_pair(&r, left.clone(), right.clone());
        let _ = crate::workflow::stall_paging::take_tick_blocking_scopes_for_tests();
        let err = pause_target(&r, RunControl::Bare).unwrap_err();
        assert_control_refusal(&err);
        assert!(err.to_string().contains("epoch 3 track A"), "{err}");
        assert!(err.to_string().contains("epoch 4 track B"), "{err}");
        assert_eq!(book_runs(&r), vec![left.clone(), right.clone()]);
        let err = stop_target(&r, RunControl::Bare).unwrap_err();
        assert_control_refusal(&err);
        assert!(crate::workflow::stall_paging::take_tick_blocking_scopes_for_tests().is_empty());
        assert_eq!(book_runs(&r), vec![left, right]);

        let paused_l = pair(&r.id, 3, RunStatus::Paused, "A");
        let paused_r = pair(&r.id, 4, RunStatus::Paused, "B");
        write_pair(&r, paused_l.clone(), paused_r.clone());
        let err = resume_target(&r, RunControl::Bare).unwrap_err();
        assert_control_refusal(&err);
        assert_eq!(book_runs(&r), vec![paused_l, paused_r]);
    }

    #[test]
    fn epoch_changes_one_entry_and_scopes_the_stall_close() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        let left = pair(&r.id, 3, RunStatus::Running, "A");
        let right = pair(&r.id, 4, RunStatus::Running, "B");
        write_pair(&r, left.clone(), right.clone());
        let _ = crate::workflow::stall_paging::take_tick_blocking_scopes_for_tests();
        pause_target(&r, RunControl::Epoch(3)).unwrap();
        let runs = book_runs(&r);
        assert_eq!(runs[0].status, RunStatus::Paused);
        assert_eq!(runs[0].last_event, "pause: hold");
        assert!(runs[0].pause_started_at.is_some());
        assert_eq!(runs[1], right);

        let paused = book_runs(&r);
        resume_target(&r, RunControl::Epoch(3)).unwrap();
        let runs = book_runs(&r);
        assert_eq!(runs[0].status, RunStatus::Running);
        assert_eq!(runs[1], paused[1]);

        write_pair(&r, left.clone(), right.clone());
        stop_target(&r, RunControl::Epoch(3)).unwrap();
        let runs = book_runs(&r);
        assert_eq!(runs[0].status, RunStatus::Stopped);
        assert_eq!(runs[0].last_event, STOP_LAST_EVENT);
        assert_eq!(runs[1], right);
        assert!(matches!(
            crate::state::load_run_book(&r).unwrap(),
            crate::state::RunBook::Many(_)
        ));
        assert_eq!(
            crate::workflow::stall_paging::take_tick_blocking_scopes_for_tests(),
            vec![Some(3)]
        );
    }

    #[test]
    fn all_changes_every_match_and_leaves_a_non_match() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        let running = pair(&r.id, 3, RunStatus::Running, "A");
        let idle = pair(&r.id, 4, RunStatus::Idle, "B");
        write_pair(&r, running, idle.clone());
        pause_target(&r, RunControl::All).unwrap();
        let runs = book_runs(&r);
        assert_eq!(runs[0].status, RunStatus::Paused);
        assert_eq!(runs[1], idle);

        let paused = pair(&r.id, 5, RunStatus::Paused, "C");
        let idle = pair(&r.id, 6, RunStatus::Idle, "D");
        write_pair(&r, paused, idle.clone());
        resume_target(&r, RunControl::All).unwrap();
        let runs = book_runs(&r);
        assert_eq!(runs[0].status, RunStatus::Running);
        assert_eq!(runs[1], idle);

        let running = pair(&r.id, 7, RunStatus::Running, "E");
        let idle = pair(&r.id, 8, RunStatus::Idle, "F");
        write_pair(&r, running, idle.clone());
        stop_target(&r, RunControl::All).unwrap();
        let runs = book_runs(&r);
        assert_eq!(runs[0].status, RunStatus::Stopped);
        assert_eq!(runs[1], idle);
        assert!(matches!(
            crate::state::load_run_book(&r).unwrap(),
            crate::state::RunBook::Many(_)
        ));
    }

    #[test]
    fn running_sibling_without_a_session_stays_in_the_book() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        let left = pair(&r.id, 3, RunStatus::Running, "A");
        let right = pair(&r.id, 9, RunStatus::Running, "B");
        write_pair(&r, left, right.clone());
        stop_target(&r, RunControl::Epoch(3)).unwrap();
        let crate::state::RunBook::Many(envelope) = crate::state::load_run_book(&r).unwrap() else {
            panic!("book collapsed");
        };
        let kept = envelope
            .runs
            .iter()
            .find(|entry| entry.run_epoch == 9)
            .unwrap();
        assert_eq!(kept, &right);
        assert_eq!(kept.status, RunStatus::Running);
    }

    #[test]
    fn singular_bare_and_matching_epoch_still_work_and_a_miss_is_unknown() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0128".into()), WorkflowDriver::Stub).unwrap();
        let before = load_run_state(&r).unwrap();
        let epoch = before.run_epoch;
        pause(&r).unwrap();
        assert_eq!(load_run_state(&r).unwrap().status, RunStatus::Paused);
        resume_target(&r, RunControl::Epoch(epoch)).unwrap();
        assert_eq!(load_run_state(&r).unwrap().status, RunStatus::Running);
        stop(&r).unwrap();
        assert_eq!(load_run_state(&r).unwrap().status, RunStatus::Stopped);
        let err = stop_target(&r, RunControl::Epoch(epoch + 5)).unwrap_err();
        assert!(
            matches!(err, CoordinatorError::Message(ref text) if text.contains("unknown run epoch")),
            "{err}"
        );
        assert_eq!(load_run_state(&r).unwrap().status, RunStatus::Stopped);
        stop(&r).unwrap();
        assert_eq!(load_run_state(&r).unwrap().status, RunStatus::Stopped);
    }

    #[test]
    fn bare_stop_with_no_match_does_not_collapse_an_all_stopped_book() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        let left = pair(&r.id, 3, RunStatus::Stopped, "A");
        let right = pair(&r.id, 4, RunStatus::Stopped, "B");
        write_pair(&r, left.clone(), right.clone());
        stop(&r).unwrap();
        assert_eq!(book_runs(&r), vec![left, right]);
        stop_target(&r, RunControl::All).unwrap();
        assert!(
            matches!(
                crate::state::load_run_book(&r).unwrap(),
                crate::state::RunBook::One(state) if state.status == RunStatus::Stopped
            ),
            "all-stopped --all did not collapse"
        );
    }

    #[test]
    fn track_selectors_refuse_duplicates_and_disagreement() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        let left = pair(&r.id, 3, RunStatus::Running, "A");
        let right = pair(&r.id, 4, RunStatus::Running, "A");
        write_pair(&r, left.clone(), right.clone());
        let err = pause_target(&r, RunControl::Track("A".into())).unwrap_err();
        assert_control_refusal(&err);
        assert_eq!(book_runs(&r), vec![left.clone(), right.clone()]);
        let err = stop_target(
            &r,
            RunControl::EpochAndTrack {
                epoch: 3,
                track: "B".into(),
            },
        )
        .unwrap_err();
        assert_control_refusal(&err);
        assert!(err.to_string().contains("disagrees"), "{err}");
        assert_eq!(book_runs(&r), vec![left, right]);
    }

    #[test]
    fn resume_of_one_paused_entry_leaves_the_other_paused() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        let left = pair(&r.id, 3, RunStatus::Paused, "A");
        let right = pair(&r.id, 4, RunStatus::Paused, "B");
        write_pair(&r, left, right.clone());
        resume_target(&r, RunControl::Epoch(3)).unwrap();
        let runs = book_runs(&r);
        assert_eq!(runs[0].status, RunStatus::Running);
        assert_eq!(runs[1], right);
    }

    #[test]
    fn advance_probe_resume_stays_in_scope_and_leaves_the_sibling() {
        let dir = tempdir().unwrap();
        let mut r = rec(dir.path());
        r.auto_start = crate::registry::AutoStartPolicy::Full;
        let cond = dir.path().join("conductor");
        std::fs::create_dir_all(cond.join("0001-Example")).unwrap();
        std::fs::create_dir_all(cond.join("0002-Next")).unwrap();
        std::fs::write(
            cond.join("conductor.md"),
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0001-Example](0001-Example/spec.md) | `.` | **Completed** | done |\n\
             | [0002-Next](0002-Next/spec.md) | `.` | **Ready — not started** | next |\n",
        )
        .unwrap();
        let next = cond.join("0002-Next");
        std::fs::write(next.join("spec.md"), b"spec\n").unwrap();
        std::fs::write(next.join("plan.md"), b"plan\n").unwrap();
        let skill = dir
            .path()
            .join(".agents")
            .join("skills")
            .join("review-track")
            .join("SKILL.md");
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(&skill, b"skill\n").unwrap();
        let mut seeded = RunState::idle(&r.id);
        seeded.track_id = Some("0002".into());
        seeded.run_epoch = 1;
        seeded.status = RunStatus::Running;
        ensure_state_dir(&r).unwrap();
        save_run_state(&r, &seeded).unwrap();
        crate::workflow::drive::write_review_markdown(&r, "agy", Some("agy review\n")).unwrap();
        crate::workflow::drive::write_review_markdown(&r, "opencode", Some("oc review\n")).unwrap();
        crate::workflow::reuse::write_join_receipt(&r, &seeded);
        crate::workflow::reuse::note_pr(&r, "0002", 80, "abc123", "track/0002-next", None);
        assert!(
            matches!(
                crate::workflow::reuse::decide_resume(&r, Some("0002")),
                crate::workflow::reuse::ResumeChoice::Probe { .. }
            ),
            "fixture did not reach the probe arm"
        );
        let mut paused = pair(&r.id, 3, RunStatus::Paused, "0001");
        paused.phase = crate::workflow::graph::PHASE_ADVANCE.into();
        paused.last_driven_phase = Some(crate::workflow::graph::PHASE_ADVANCE.into());
        let sibling = pair(&r.id, 8, RunStatus::Running, "SIB");
        write_pair(&r, paused, sibling.clone());
        let _probe = crate::workflow::reuse::install_test_probe(std::sync::Arc::new(|_| {
            Ok(Some(crate::workflow::reuse::ResumePr {
                state: "OPEN".into(),
                head_oid: "abc123".into(),
                head_ref: "track/0002-next".into(),
            }))
        }));
        let view = resume(&r).unwrap();
        assert!(
            !view.last_event.contains("ambiguous run"),
            "{}",
            view.last_event
        );
        let runs = book_runs(&r);
        let kept = runs.iter().find(|entry| entry.run_epoch == 8).unwrap();
        assert_eq!(kept, &sibling);
        let resumed = runs.iter().find(|entry| entry.run_epoch != 8).unwrap();
        assert_eq!(resumed.phase, crate::workflow::graph::PHASE_CI_WAIT);
        assert!(
            resumed.last_event.contains("resume ci-wait"),
            "{}",
            resumed.last_event
        );
    }

    #[test]
    fn book_status_copies_last_event_and_per_run_superseded() {
        let _guard = crate::config::test_env_lock();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        let cond = dir.path().join("conductor");
        std::fs::create_dir_all(cond.join("0128-Done")).unwrap();
        std::fs::create_dir_all(cond.join("0130-Open")).unwrap();
        std::fs::write(
            cond.join("conductor.md"),
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0128-Done | `.` | **Completed** | done |\n\
             | 0130-Open | `.` | **Ready - not started** | open |\n",
        )
        .unwrap();
        let mut low = crate::state::RunState::idle(&r.id);
        low.run_epoch = 1;
        low.status = RunStatus::Idle;
        low.phase = "stub:idle".into();
        low.track_id = Some("0128".into());
        low.failure_class = Some(crate::outcome::FailureClass::Timeout);
        low.last_event = "wait: shared tree held by epoch 2".into();
        let mut high = crate::state::RunState::idle(&r.id);
        high.run_epoch = 2;
        high.status = RunStatus::Running;
        high.phase = "implement".into();
        high.track_id = Some("0130".into());
        high.last_event = "pause: hold".into();
        crate::state::write_run_book(&r, &crate::state::RunEnvelope::new(0, vec![low, high]))
            .unwrap();
        let view = status(&r).unwrap();
        assert_eq!(view.runs.len(), 2);
        assert_eq!(view.runs[0].last_event, "wait: shared tree held by epoch 2");
        assert_eq!(view.runs[1].last_event, "pause: hold");
        assert_eq!(
            view.runs[0].failure_superseded.as_deref(),
            Some(crate::notify::SUPERSEDED_COMPLETED)
        );
        assert_eq!(view.runs[1].failure_superseded, None);
        assert_eq!(view.runs[0].failure_artifact, None);
        assert_eq!(view.runs[1].failure_artifact, None);
    }
}
