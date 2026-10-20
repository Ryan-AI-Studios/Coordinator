//! Built-in canonical conductor-track workflow (`canonical_v1`).

pub mod block;
pub mod bundle;
pub mod conductor_md;
pub mod decision;
pub mod drive;
pub mod evidence_stamp;
pub mod execution_repo;
pub mod graph;
pub mod plan_review;
pub mod prompts;
pub mod reuse;
pub mod self_check;
pub mod shipped;
pub mod timeouts;
pub mod watchdog;

use serde::{Deserialize, Serialize};

use crate::error::{CoordinatorError, Result};
use crate::outcome::{FailureClass, LAST_EVENT_MESSAGE_CAP, PhaseOutcome};
use crate::registry::{AutoStartPolicy, ProjectRecord};
use crate::state::{RunState, RunStatus, load_run_state, save_run_state, with_run_state_lock};

pub use conductor_md::{
    ReadyPickState, pick_next_ready, pick_next_ready_excluding, pick_next_ready_excluding_sticky,
    should_pick_next_ready, track_row_nostart,
};
pub use drive::tick;
pub use graph::{WORKFLOW_ID, is_canonical, is_stub_phase, resolve_track_dir, successor};
pub use shipped::{
    MergedTrackProbe, merged_search_query, pr_title_is_track, track_is_shipped_local,
};
pub use timeouts::{ENV_PHASE_TIMEOUT_SECS, TimeoutSource, timeout_for_phase, timeout_source};

/// `finish_advance` null/`next_track` Idle last_event (0030 pick trigger).
pub const LAST_EVENT_BACKLOG_CLEAR: &str = "workflow: backlog clear";

/// Env fallback when CLI/HTTP omit `--driver`.
pub const ENV_WORKFLOW_DRIVER: &str = "COORDINATOR_WORKFLOW_DRIVER";

/// Persisted run driver (skip is per-phase, not a run-level driver).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowDriver {
    #[default]
    Adapter,
    FileWait,
    Stub,
}

impl WorkflowDriver {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Adapter => "adapter",
            Self::FileWait => "file_wait",
            Self::Stub => "stub",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s.trim() {
            "adapter" => Ok(Self::Adapter),
            "file_wait" => Ok(Self::FileWait),
            "stub" => Ok(Self::Stub),
            other => Err(CoordinatorError::Message(format!(
                "unknown workflow driver '{other}'; expected adapter | file_wait | stub"
            ))),
        }
    }
}

impl std::fmt::Display for WorkflowDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// CLI/HTTP value, else `COORDINATOR_WORKFLOW_DRIVER`, else adapter.
pub fn resolve_driver(explicit: Option<&str>) -> Result<WorkflowDriver> {
    if let Some(s) = explicit {
        return WorkflowDriver::parse(s);
    }
    match std::env::var(ENV_WORKFLOW_DRIVER) {
        Ok(s) if !s.trim().is_empty() => WorkflowDriver::parse(&s),
        _ => Ok(WorkflowDriver::Adapter),
    }
}

/// `address-ci` success with no tracked diff stops inside this apply.
pub enum AddressCiFollowUp {
    Continue,
    Stopped {
        message: String,
    },
    /// Successor reuse still needs `gh pr view`. Caller probes after both locks drop.
    ReuseProbe(Box<reuse::ReuseProbe>),
}

const FOLD_WITHHELD_KIND: &str = "fold-withheld";
/// Progress-log kind when the fold promotes a completed placeholder row to Ready.
const FOLD_PROMOTED_KIND: &str = "fold-promoted";
const FOLD_WITHHELD_UNREADABLE: &str = "workflow: fold withheld (cannot read track row)";

fn fold_withheld_status(status_clean: &str) -> String {
    let capped = cap_fold_status(status_clean);
    format!("workflow: fold withheld (row status: {capped})")
}

/// At most 120 Unicode scalar values. A longer status keeps 119 and appends `…`.
fn cap_fold_status(status_clean: &str) -> String {
    if status_clean.chars().count() <= 120 {
        return status_clean.to_string();
    }
    let mut out: String = status_clean.chars().take(119).collect();
    out.push('\u{2026}');
    out
}

/// Apply-table hook: canonical success → successor (stay Running/Paused) or advance.
pub fn on_success(
    record: &ProjectRecord,
    state: &mut RunState,
    outcome: &PhaseOutcome,
) -> AddressCiFollowUp {
    // 0088 / 0090: undo a non-terminal Ready downgrade. The helper does not take the
    // run-state lock (`apply` already holds it). When it reports consumed, clear the
    // in-memory map so the caller's later save persists the empty snapshot.
    let ready = crate::workflow::drive::restore_ready_rows_after_phase(record);
    if ready.consumed {
        state.registry_status_before.clear();
    }
    if !ready.restored.is_empty() {
        state.last_event = format!(
            "workflow: restored Ready after downgrade ({})",
            ready.restored.join(", ")
        );
    }
    if let Some(ref meta) = outcome.metadata {
        if let Some(ref next) = meta.next_track {
            let t = next.trim();
            state.next_track = if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            };
        }
        if meta.pr_number.is_some() || meta.pr_url.is_some() {
            let mut ci = state.ci.take().unwrap_or_default();
            if meta.pr_number.is_some() {
                ci.pr_number = meta.pr_number;
            }
            if meta.pr_url.is_some() {
                ci.pr_url = meta.pr_url.clone();
            }
            state.ci = Some(ci);
        }
    }
    state.failure_class = None;
    state.last_driven_phase = None;

    if state.phase == graph::PHASE_ADDRESS_CI {
        match crate::ci::fix::classify_diff(record, state) {
            crate::ci::fix::DiffClass::Changed => {}
            crate::ci::fix::DiffClass::Empty { sha } => {
                let message = format!("address-ci: no diff from {sha}");
                stop_address_ci(state, &message);
                return AddressCiFollowUp::Stopped { message };
            }
            crate::ci::fix::DiffClass::Unreadable => {
                let message = "address-ci: diff unreadable".to_string();
                stop_address_ci(state, &message);
                return AddressCiFollowUp::Stopped { message };
            }
        }
    }

    if state.phase == graph::PHASE_ADVANCE {
        if let Some(probe) = apply_advance(record, state) {
            return AddressCiFollowUp::ReuseProbe(Box::new(probe));
        }
        return AddressCiFollowUp::Continue;
    }

    if let Some(next) = successor(&state.phase) {
        // 0097: a fold may override the successor (see `fold_review_reentry`).
        let mut next = next;
        // 0554: an implement leg that reports success without committing leaves the chain to
        // fail two phases later (cross-model gate, then the next epoch's checkpoint).
        let implement_message =
            if state.phase == graph::PHASE_IMPLEMENT && state.driver == WorkflowDriver::Adapter {
                implement_artifact_gate(record, state)
            } else {
                None
            };
        if let Some(message) = implement_message {
            state.status = RunStatus::Stopped;
            state.failure_class = Some(FailureClass::Gate);
            state.last_event = message.clone();
            crate::progress_log::append(record, IMPLEMENT_GATE_KIND, &message);
            return AddressCiFollowUp::Continue;
        }
        if state.phase == graph::PHASE_FOLD && next == graph::PHASE_IMPLEMENT {
            // The leg's `success` is not evidence. Check the artifacts before implementing.
            if let Some(message) = fold_artifact_gate(record, state) {
                state.status = RunStatus::Stopped;
                state.failure_class = Some(FailureClass::Gate);
                state.last_event = message.clone();
                crate::progress_log::append(record, FOLD_GATE_KIND, &message);
                return AddressCiFollowUp::Continue;
            }
            match conductor_md::fold_advance(record, state.track_id.as_deref()) {
                conductor_md::FoldAdvance::Allow => match fold_review_reentry(record, state) {
                    FoldReentry::None => {
                        if let Err(e) = crate::checkpoint::ensure_implement_ref(record, state) {
                            apply_checkpoint_refusal(state, &e);
                            crate::progress_log::append(
                                record,
                                "checkpoint-refused",
                                &state.last_event,
                            );
                            return AddressCiFollowUp::Continue;
                        }
                    }
                    FoldReentry::Reenter => next = graph::PHASE_PLAN_REVIEW,
                    FoldReentry::Exhausted => return AddressCiFollowUp::Continue,
                },
                conductor_md::FoldAdvance::Withhold { status_clean } => {
                    state.status = RunStatus::Stopped;
                    state.failure_class = None;
                    state.last_event = fold_withheld_status(&status_clean);
                    crate::progress_log::append(record, FOLD_WITHHELD_KIND, &state.last_event);
                    return AddressCiFollowUp::Continue;
                }
                conductor_md::FoldAdvance::PromotePlaceholder { key, ready } => {
                    let promoted = conductor_md::promote_placeholder(record, &key, &ready);
                    match promoted {
                        Ok(true) => {
                            state.last_event = format!(
                                "workflow: fold promoted {key} placeholder -> {ready} \
                                 (spec/plan pass complete); advancing"
                            );
                            crate::progress_log::append(
                                record,
                                FOLD_PROMOTED_KIND,
                                &state.last_event,
                            );
                            if let Err(e) = crate::checkpoint::ensure_implement_ref(record, state) {
                                apply_checkpoint_refusal(state, &e);
                                crate::progress_log::append(
                                    record,
                                    "checkpoint-refused",
                                    &state.last_event,
                                );
                                return AddressCiFollowUp::Continue;
                            }
                        }
                        // The row was there for the gate and gone for the write, or the
                        // write failed. Fail closed: never advance on an unverified promotion.
                        Ok(false) | Err(_) => {
                            state.status = RunStatus::Stopped;
                            state.failure_class = None;
                            state.last_event = FOLD_WITHHELD_UNREADABLE.to_string();
                            crate::progress_log::append(
                                record,
                                FOLD_WITHHELD_KIND,
                                &state.last_event,
                            );
                            return AddressCiFollowUp::Continue;
                        }
                    }
                }
                conductor_md::FoldAdvance::Unreadable => {
                    state.status = RunStatus::Stopped;
                    state.failure_class = None;
                    state.last_event = FOLD_WITHHELD_UNREADABLE.to_string();
                    crate::progress_log::append(record, FOLD_WITHHELD_KIND, &state.last_event);
                    return AddressCiFollowUp::Continue;
                }
                conductor_md::FoldAdvance::RowMissing { key, parsed } => {
                    state.status = RunStatus::Stopped;
                    state.failure_class = None;
                    // The registry was readable and parsed; this row simply is not in the
                    // table. Name it and the count -- the usual cause is a heading or `---`
                    // that ended the table before the row.
                    state.last_event = format!(
                        "workflow: fold withheld (track {key} absent from the registry table; \
                         {parsed} rows parsed - a heading or `---` may have ended the table \
                         before its row)"
                    );
                    crate::progress_log::append(record, FOLD_WITHHELD_KIND, &state.last_event);
                    return AddressCiFollowUp::Continue;
                }
            }
        }
        let from = state.phase.clone();
        state.phase = next.to_string();
        reset_phase_clock(state);
        if next == graph::PHASE_PLAN_REVIEW {
            state.pending_roles = graph::review_slugs()
                .iter()
                .map(|s| (*s).to_string())
                .collect();
        } else {
            state.pending_roles.clear();
        }
        if next == graph::PHASE_CROSS_MODEL {
            state.review = None;
        }
        if let Some(ref m) = outcome.message {
            if m.starts_with("skip:")
                || m.starts_with("compact:")
                || m.starts_with("plan-review:")
                || m.starts_with("ci-wait:")
                || m.starts_with("cross-model:")
            {
                state.last_event = m.clone();
            } else {
                state.last_event = advance_event(&from, next, state.status == RunStatus::Paused);
            }
        } else {
            state.last_event = advance_event(&from, next, state.status == RunStatus::Paused);
        }
    } else {
        state.status = RunStatus::Idle;
        state.phase_started_at = None;
        state.pause_started_at = None;
        state.last_event = "workflow: graph complete".into();
    }
    AddressCiFollowUp::Continue
}

/// A delivery-phase resolution error is `permission` (spec §3.3/§3.5); a legacy
/// checkpoint failure (dirty tree, unreadable Git) keeps the historical `fold` refusal.
fn apply_checkpoint_refusal(state: &mut RunState, error: &CoordinatorError) {
    state.status = RunStatus::Stopped;
    let message = error.to_string();
    state.failure_class = if state.driver == WorkflowDriver::Adapter
        && message.starts_with(execution_repo::RESOLUTION_PREFIX)
    {
        Some(FailureClass::Permission)
    } else {
        None
    };
    state.last_event = message;
}

fn stop_address_ci(state: &mut RunState, message: &str) {
    state.status = RunStatus::Stopped;
    state.failure_class = Some(FailureClass::CiFailed);
    state.phase_started_at = None;
    state.pause_started_at = None;
    state.last_event = message.to_string();
}

/// 0097 — max consecutive `fold -> plan-review` re-entries in one run.
const MAX_FOLD_REVIEW_REENTRIES: u32 = 2;
const FOLD_REENTRY_KIND: &str = "fold-reentry";
const REVIEW_NEVER_SETTLES_KIND: &str = "review-never-settles";

enum FoldReentry {
    /// Nothing changed, or there is no receipt to compare against: advance as before.
    None,
    /// The fold rewrote a reviewed artifact; the successor must become `plan-review`.
    Reenter,
    /// The cap is spent; `state` is already stopped with `review_never_settles`.
    Exhausted,
}

/// Option (b) part 2: a fold re-entry counts toward [`MAX_FOLD_REVIEW_REENTRIES`] only
/// when it reproduces the identical spec/plan pair it saw at the previous re-entry.
///
/// A *changing* pair means the rewrite is converging — the next plan-review covers new
/// bytes, so treating that as non-progress is what killed 0553 (x3) and 0555 (x2) on
/// 2026-10-20. A *repeated* pair is the genuine "review never settles" case and still
/// trips the cap, so the guard keeps its teeth.
fn fold_counter_step(counter: u32, repeated: bool) -> u32 {
    if repeated {
        counter.saturating_add(1)
    } else {
        0
    }
}

fn short_sha(sha: &str) -> &str {
    sha.get(..8).unwrap_or(sha)
}

/// 0097 — did this fold rewrite an artifact the last review receipt covers?
///
/// Compares the on-disk `spec.md`/`plan.md` hashes with the ones `plan-review` recorded in
/// `reuse.json`. Without those reviews a fold that changes them hands `implement` artifacts
/// nobody has read, and a spec that requires fresh reviews of the current pair can never be
/// satisfied — every fold invalidates the reviews that preceded it, and the graph had no
/// `fold -> plan-review` edge. A missing receipt means there is nothing to compare against,
/// so the historical `fold -> implement` behaviour is kept rather than inventing a re-review.
///
/// Bounded by [`MAX_FOLD_REVIEW_REENTRIES`]: a fold that always rewrites stops the run with
/// [`FailureClass::ReviewNeverSettles`] instead of spinning.
const FOLD_GATE_KIND: &str = "fold-gate";

/// Fold gate: never advance to `implement` on artifacts that say the plan is not ready.
///
/// A phase leg self-reports `success`; the conductor must not treat that as evidence when
/// the artifacts say otherwise. Observed on 0553 (2026-10-19): `plan.md` was absent and
/// both review artifacts still said `## Verdict: Needs re-plan`, the fold returned
/// `success`, the conductor advanced, implement wrote nothing, and the run was recorded as
/// `harness_crash` - the wrong diagnosis for a gate failure. Checking here makes the
/// failure land on the phase that actually failed, and stops the track before it spends
/// a leg on nothing.
fn fold_artifact_gate(record: &ProjectRecord, state: &RunState) -> Option<String> {
    let track = state
        .track_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let dir = graph::resolve_track_dir(record, track)?;
    let plan_ok = std::fs::read_to_string(dir.join("plan.md")).is_ok_and(|t| !t.trim().is_empty());
    if !plan_ok {
        return Some(format!(
            "workflow: fold gate - {track} reached implement with no usable plan.md              (the leg reported success)"
        ));
    }
    let mut offenders: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let lower = name.to_ascii_lowercase();
            if !lower.contains("review") || !lower.ends_with(".md") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if review_asks_for_replan(&text) {
                offenders.push(name.to_string());
            }
        }
    }
    offenders.sort();
    if offenders.is_empty() {
        return None;
    }
    Some(format!(
        "workflow: fold gate - {track} reached implement while review still asked for a          re-plan: {}",
        offenders.join(", ")
    ))
}

/// Does this review artifact's verdict ask for a re-plan?
fn review_asks_for_replan(text: &str) -> bool {
    text.lines().any(|line| {
        let trimmed = line.trim();
        let body = trimmed.trim_start_matches('#').trim();
        let Some(rest) = body.strip_prefix("Verdict:") else {
            return false;
        };
        let rest = rest.trim().to_ascii_lowercase();
        rest.contains("needs re-plan") || rest.contains("needs replan")
    })
}

fn fold_review_reentry(record: &ProjectRecord, state: &mut RunState) -> FoldReentry {
    let Some(track) = state.track_id.as_deref() else {
        return FoldReentry::None;
    };
    let Some(dir) = graph::resolve_track_dir(record, track) else {
        return FoldReentry::None;
    };
    let Some((spec_now, plan_now)) = reuse::current_artifact_pair(&dir) else {
        return FoldReentry::None;
    };
    let Some((spec_rev, plan_rev)) = reuse::reviewed_artifact_pair(record, track) else {
        return FoldReentry::None;
    };
    if spec_now == spec_rev && plan_now == plan_rev {
        // Settled: a later rewrite in this run starts a fresh count.
        state.fold_review_reentries = 0;
        state.fold_last_pair = None;
        return FoldReentry::None;
    }
    let pair_now = (spec_now.clone(), plan_now.clone());
    let repeated = state.fold_last_pair.as_ref() == Some(&pair_now);
    state.fold_last_pair = Some(pair_now.clone());
    state.fold_review_reentries = fold_counter_step(state.fold_review_reentries, repeated);
    if state.fold_review_reentries >= MAX_FOLD_REVIEW_REENTRIES {
        state.status = RunStatus::Stopped;
        state.failure_class = Some(FailureClass::ReviewNeverSettles);
        state.last_event = format!(
            "workflow: review never settles — artifacts rewritten {} times; spec {} plan {} \
             (reviewed spec {} plan {})",
            MAX_FOLD_REVIEW_REENTRIES,
            short_sha(&spec_now),
            short_sha(&plan_now),
            short_sha(&spec_rev),
            short_sha(&plan_rev)
        );
        crate::progress_log::append(record, REVIEW_NEVER_SETTLES_KIND, &state.last_event);
        return FoldReentry::Exhausted;
    }
    state.fold_review_reentries = state.fold_review_reentries.saturating_add(1);
    let message = format!(
        "advance fold -> plan-review (artifacts changed: spec {} plan {})",
        short_sha(&spec_now),
        short_sha(&plan_now)
    );
    state.last_event = message.clone();
    crate::progress_log::append(record, FOLD_REENTRY_KIND, &message);
    FoldReentry::Reenter
}

fn advance_event(from: &str, next: &str, paused: bool) -> String {
    if paused {
        format!("workflow: advance {from} → {next} (paused)")
    } else {
        format!("workflow: advance {from} → {next}")
    }
}

pub fn reset_phase_clock(state: &mut RunState) {
    let now = chrono::Utc::now();
    state.phase_started_at = Some(now);
    state.total_paused_ms = 0;
    state.pause_spans.clear();
    state.stalled_at = None;
    state.stall_recycles = 0;
    state.aborted_session_id = None;
    state.acp_stdout_retries = 0;
    state.plan_review_spawned.clear();
    state.plan_review_join_retries = 0;
    state.plan_review_slot_ran.clear();
    if state.status == RunStatus::Paused {
        state.pause_started_at = Some(now);
    } else {
        state.pause_started_at = None;
    }
}

/// `advance` success: Ready-walk successor, then `full` auto-starts;
/// hitl/never/`nostart` park. Idle when no eligible Ready. Pause holds until resume.
pub fn apply_advance(record: &ProjectRecord, state: &mut RunState) -> Option<reuse::ReuseProbe> {
    if state.status == RunStatus::Paused {
        state.last_driven_phase = Some(graph::PHASE_ADVANCE.into());
        state.last_event = "workflow: advance held until resume".into();
        return None;
    }
    finish_advance(record, state)
}

/// First eligible Ready after excluding `state.track_id` (0030 sequencer, no `gh`).
fn advance_successor(record: &ProjectRecord, state: &RunState) -> crate::error::Result<String> {
    let excluded: Vec<String> = state
        .track_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .into_iter()
        .collect();
    let id = conductor_md::pick_next_ready_excluding_sticky(
        record,
        &excluded,
        None,
        &state.sticky_ready_ids,
    )?;
    if let Some(file) = conductor_md::overlay_file_status(record, &id) {
        crate::progress_log::append(
            record,
            "advance",
            &format!("sticky Ready {id} (file={file})"),
        );
    }
    Ok(id)
}

fn planner_recorded_id(state: &RunState) -> Option<String> {
    state
        .next_track
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn journal_advance_override(record: &ProjectRecord, planner_id: Option<&str>, cand: Option<&str>) {
    let detail = match (planner_id, cand) {
        (Some(p), Some(c)) if !crate::notify::artifact::track_ids_match(p, c) => {
            Some(format!("override next_track {p} → {c}"))
        }
        (None, Some(c)) => Some(format!("override next_track null → {c}")),
        (Some(p), None) => Some(format!("override next_track {p} → (none)")),
        _ => None,
    };
    if let Some(detail) = detail {
        crate::progress_log::append(record, "advance", &detail);
    }
}

fn finish_advance(record: &ProjectRecord, state: &mut RunState) -> Option<reuse::ReuseProbe> {
    // 0098: a Blocked hold keeps the held track and phase; advance does nothing.
    if state.failure_class == Some(FailureClass::Blocked) {
        return None;
    }
    let _ = crate::checkpoint::reap_completed(record);
    if let Some(id) = state
        .track_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        && shipped::track_is_shipped_local(record, state, id)
    {
        crate::progress_log::append(record, "advance", &skip_merged_detail(state, id));
    }
    let planner = planner_recorded_id(state);
    match advance_successor(record, state) {
        Ok(cand) => {
            journal_advance_override(record, planner.as_deref(), Some(cand.as_str()));
            start_or_park(record, state, &cand)
        }
        Err(e) => {
            journal_advance_override(record, planner.as_deref(), None);
            let msg = e.to_string();
            let gist = msg.split("; pass --track").next().unwrap_or(&msg);
            let detail = match conductor_md::parsed_track_row_count(record) {
                Some(n) => format!("{gist} ({n} rows parsed)"),
                None => gist.to_string(),
            };
            crate::progress_log::append(record, "advance", &detail);
            apply_backlog_clear(record, state);
            None
        }
    }
}

fn skip_merged_detail(state: &RunState, id: &str) -> String {
    match state.ci.as_ref().and_then(|c| c.pr_number) {
        Some(n) => format!("skip merged {id} #{n}"),
        None => format!("skip merged {id}"),
    }
}

fn start_or_park(
    record: &ProjectRecord,
    state: &mut RunState,
    id: &str,
) -> Option<reuse::ReuseProbe> {
    if let Some(policy) = park_policy(record, id) {
        park_next(record, state, id, policy);
        return None;
    }
    match reuse::decide_resume(record, Some(id)) {
        reuse::ResumeChoice::Probe {
            number,
            head_sha,
            head_ref,
            repo,
        } => {
            // Keep the phase marked driven so a tick during `gh` does not inject advance again.
            state.last_driven_phase = Some(graph::PHASE_ADVANCE.into());
            Some(reuse::ReuseProbe {
                successor_id: id.to_string(),
                closed_epoch: state.run_epoch,
                snap_epoch: state.run_epoch,
                snap_track: state.track_id.clone(),
                snap_status: state.status,
                number,
                head_sha,
                head_ref,
                repo,
            })
        }
        reuse::ResumeChoice::Plan {
            archive_slugs,
            journal_unreadable,
        } => {
            let next_epoch = state.run_epoch.saturating_add(1);
            if let Err(e) = crate::worktree::prepare_epoch(record, next_epoch) {
                state.status = RunStatus::Stopped;
                state.failure_class = Some(FailureClass::HarnessCrash);
                state.last_event = e.to_string();
                state.phase_started_at = None;
                state.pause_started_at = None;
                return None;
            }
            let closed = state.run_epoch;
            if journal_unreadable {
                crate::progress_log::append(record, "reuse", "reuse: receipt unreadable");
            }
            auto_start(state, id);
            crate::outcome::clear_active_outcome_file(record);
            drive::clear_plan_review_artifacts(
                record,
                &drive::ArtifactClear {
                    track_id: Some(id.to_string()),
                    closed_epoch: closed,
                    archive_slugs,
                },
            );
            crate::notify::clear_artifact(record);
            watchdog::clear_progress(record);
            None
        }
    }
}

fn apply_backlog_clear(record: &ProjectRecord, state: &mut RunState) {
    // 0098: backlog clear must preserve a Blocked hold and its artifact.
    if state.failure_class == Some(FailureClass::Blocked) {
        return;
    }
    let _ = crate::checkpoint::reap_completed(record);
    state.status = RunStatus::Idle;
    state.next_track = None;
    state.last_event = LAST_EVENT_BACKLOG_CLEAR.into();
    state.phase_started_at = None;
    state.pause_started_at = None;
    // Defense-in-depth (0040): same-track leftover. Incident fix is
    // `failure resolve` + SUPERSEDED display — this branch is
    // fixture-reachable after a failure apply (Stopped rejects apply,
    // so a new `run` already cleared the file).
    if let Ok(Some(shown)) = crate::notify::artifact::read(record)
        && let Some(ref track) = state.track_id
    {
        let meta = crate::notify::artifact::parse_metadata(&shown.body);
        if let Some(ref art_track) = meta.track_id
            && crate::notify::artifact::track_ids_match(art_track, track)
        {
            crate::notify::settle_failure(record, state, crate::notify::AUTO_CLEAR_DETAIL, false);
        }
    }
}

/// `None` = in-process `auto_start`. `Some` = park reason word.
fn park_policy(record: &ProjectRecord, id: &str) -> Option<&'static str> {
    if track_row_nostart(record, id) {
        return Some("nostart");
    }
    match record.auto_start {
        AutoStartPolicy::Full => None,
        AutoStartPolicy::Hitl => Some("hitl"),
        AutoStartPolicy::Never => Some("never"),
    }
}

fn park_next(record: &ProjectRecord, state: &mut RunState, id: &str, policy: &str) {
    apply_backlog_clear(record, state);
    state.next_track = None;
    state.parked_next = Some(id.to_string());
    crate::progress_log::append(
        record,
        "advance",
        &format!("parked-next {id} (policy={policy})"),
    );
}

/// Bounce only GateFail (`difficulty`) from `cross-model-review` under the cap.
pub fn should_bounce_gate_fail(state: &RunState, outcome: &PhaseOutcome) -> bool {
    graph::is_canonical(&outcome.phase)
        && outcome.phase == graph::PHASE_CROSS_MODEL
        && matches!(outcome.failure_class, Some(FailureClass::Difficulty))
        && state.address_findings_attempts < graph::ADDRESS_FINDINGS_CAP
}

/// Best-effort archive of live gate reports to `*.gate{n}.md`, then delete live names.
///
/// Track-dir half is skipped when `resolve_track_dir` is None. IO errors do not abort.
pub fn archive_gate_reports(record: &ProjectRecord, state: &RunState, n: u32) {
    let mut slugs: std::collections::BTreeSet<String> = state
        .review
        .as_ref()
        .map(|rv| rv.attempted.iter().cloned().collect())
        .unwrap_or_default();

    let track_dir = state
        .track_id
        .as_deref()
        .and_then(|id| graph::resolve_track_dir(record, id));
    // State-dir `reviews/` is wiped on fresh `run`, so live files there are this run.
    let state_reviews = bundle::reviews_dir(record).ok();
    if let Some(ref dir) = state_reviews
        && let Ok(rd) = std::fs::read_dir(dir)
    {
        for ent in rd.flatten() {
            if let Some(slug) = live_state_review_slug(&ent.file_name().to_string_lossy()) {
                slugs.insert(slug);
            }
        }
    }
    // Track-dir leftovers may remain; only pair with a this-run state-dir live copy.
    if let Some(ref dir) = track_dir
        && let Some(ref state_dir) = state_reviews
        && let Ok(rd) = std::fs::read_dir(dir)
    {
        for ent in rd.flatten() {
            if let Some(slug) = live_track_review_slug(&ent.file_name().to_string_lossy()) {
                let paired = state_dir.join(format!("cross-model-{slug}.md"));
                if paired.is_file() {
                    slugs.insert(slug);
                }
            }
        }
    }

    for slug in slugs {
        if let Some(ref dir) = track_dir {
            let live = dir.join(format!("review.{slug}.md"));
            let dest = dir.join(format!("review.{slug}.gate{n}.md"));
            archive_one(&live, &dest);
        }
        if let Ok(dir) = bundle::reviews_dir(record) {
            let live = dir.join(format!("cross-model-{slug}.md"));
            let dest = dir.join(format!("cross-model-{slug}.gate{n}.md"));
            archive_one(&live, &dest);
        }
    }
}

fn live_track_review_slug(name: &str) -> Option<String> {
    let rest = name.strip_prefix("review.")?;
    let stem = rest.strip_suffix(".md")?;
    // Live names are `review.{slug}.md` (slug has no extra dots).
    // Reject `review.codex.fail.md`, `review.codex.gate1.md`, `review.md`.
    if stem.is_empty() || stem.contains('.') {
        return None;
    }
    Some(stem.to_string())
}

fn live_state_review_slug(name: &str) -> Option<String> {
    let rest = name.strip_prefix("cross-model-")?;
    let stem = rest.strip_suffix(".md")?;
    if stem.is_empty() || stem.contains('.') {
        return None;
    }
    Some(stem.to_string())
}

fn archive_one(live: &std::path::Path, dest: &std::path::Path) {
    if !live.is_file() {
        return;
    }
    let Ok(bytes) = std::fs::read(live) else {
        return;
    };
    if crate::persist::atomic_write(dest, &bytes).is_ok() {
        let _ = std::fs::remove_file(live);
    }
}

fn truncate_event_msg(msg: &str) -> String {
    if msg.chars().count() <= LAST_EVENT_MESSAGE_CAP {
        return msg.to_string();
    }
    let cut: String = msg.chars().take(LAST_EVENT_MESSAGE_CAP).collect();
    format!("{cut}…")
}

/// Stay Running/Paused at `address-findings`; archive live reports; clear review.
pub fn on_gate_retry(record: &ProjectRecord, state: &mut RunState, outcome: &PhaseOutcome) {
    // 0098: a Blocked hold is never released or mutated by a gate retry.
    if state.failure_class == Some(FailureClass::Blocked) {
        return;
    }
    state.address_findings_attempts = state.address_findings_attempts.saturating_add(1);
    let n = state.address_findings_attempts;
    archive_gate_reports(record, state, n);
    state.review = None;
    state.last_driven_phase = None;
    state.failure_class = None;
    state.pending_roles.clear();
    state.phase = graph::PHASE_ADDRESS_FINDINGS.into();
    reset_phase_clock(state);
    let msg = outcome
        .message
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(truncate_event_msg)
        .unwrap_or_else(|| "cross-model: gate failed".into());
    state.last_event = format!(
        "{msg}; address-findings {n}/{}",
        graph::ADDRESS_FINDINGS_CAP
    );
}

pub fn auto_start(state: &mut RunState, track_id: &str) {
    // 0098: never release a Blocked hold or mutate the held state.
    if state.failure_class == Some(FailureClass::Blocked) {
        return;
    }
    state.run_epoch = state.run_epoch.saturating_add(1);
    state.track_id = Some(track_id.to_string());
    state.next_track = None;
    state.parked_next = None;
    state.phase = graph::PHASE_PLAN.into();
    state.workflow = Some(WORKFLOW_ID.into());
    state.status = RunStatus::Running;
    state.pending_roles.clear();
    state.last_driven_phase = None;
    state.failure_class = None;
    state.last_applied_outcome_hash = None;
    state.ci = None;
    state.review = None;
    state.delivery_target = None;
    state.gate_target_journal = None;
    state.stalled_at = None;
    state.address_findings_attempts = 0;
    state.ci_fix_attempts = 0;
    state.ci_fix_request = None;
    reset_phase_clock(state);
    state.last_event = format!("workflow: auto-start {track_id}");
}

/// Resume after `advance` succeeded while Paused.
///
/// Returns a probe when the successor still needs `gh pr view`. The caller must
/// not invoke `gh` while holding the run-state lock.
pub fn apply_advance_on_resume(
    record: &ProjectRecord,
    state: &mut RunState,
) -> Option<reuse::ReuseProbe> {
    state.status = RunStatus::Running;
    state.pause_started_at = None;
    finish_advance(record, state)
}

/// Commit a successor after `gh pr view` returned. Caller holds the run-state lock.
pub fn commit_probed_successor(
    record: &ProjectRecord,
    probe: &reuse::ReuseProbe,
    answer: &Result<Option<reuse::ResumePr>>,
) -> Result<crate::state::StatusView> {
    let mut state = load_run_state(record)?;
    if state.run_epoch != probe.snap_epoch
        || state.track_id != probe.snap_track
        || state.status != probe.snap_status
    {
        return Ok(crate::state::StatusView::from_record(record, &state));
    }
    let next_epoch = state.run_epoch.saturating_add(1);
    if let Err(e) = crate::worktree::prepare_epoch(record, next_epoch) {
        state.status = RunStatus::Stopped;
        state.failure_class = Some(FailureClass::HarnessCrash);
        state.last_event = e.to_string();
        state.phase_started_at = None;
        state.pause_started_at = None;
        state.updated_at = chrono::Utc::now();
        save_run_state(record, &state)?;
        return Ok(crate::state::StatusView::from_record(record, &state));
    }
    let kept_hash = state.last_applied_outcome_hash.clone();
    let closed = probe.closed_epoch;
    auto_start(&mut state, &probe.successor_id);
    state.last_applied_outcome_hash = kept_hash;
    if reuse::probe_accepts(&probe.successor_id, &probe.head_sha, answer) {
        state.phase = graph::PHASE_CI_WAIT.into();
        state.ci = Some(crate::state::CiWatchState {
            pr_number: Some(probe.number),
            head_sha: Some(probe.head_sha.clone()),
            ..Default::default()
        });
        state.last_event = format!(
            "run: started {WORKFLOW_ID} resume ci-wait pr {}",
            probe.number
        );
        // G2 (second resume path): freeze ci-wait provenance from tick 0.
        match execution_repo::resolve_track_target(record, &state, graph::PHASE_CI_WAIT) {
            Ok(target) => state.delivery_target = Some(target),
            Err(err) => crate::progress_log::append(
                record,
                "reuse",
                &format!("reuse: ci-wait target unresolved — {err}"),
            ),
        }
    }
    crate::outcome::clear_active_outcome_file(record);
    drive::clear_plan_review_artifacts(
        record,
        &drive::ArtifactClear {
            track_id: Some(probe.successor_id.clone()),
            closed_epoch: closed,
            archive_slugs: Vec::new(),
        },
    );
    crate::notify::clear_artifact(record);
    watchdog::clear_progress(record);
    state.updated_at = chrono::Utc::now();
    save_run_state(record, &state)?;
    let track = state.track_id.as_deref().unwrap_or("-");
    crate::progress_log::append(
        record,
        "start",
        &format!("track={track} phase={}  {}", state.phase, state.last_event),
    );
    Ok(crate::state::StatusView::from_record(record, &state))
}

pub fn mark_driven(record: &ProjectRecord, phase: &str) -> Result<()> {
    mark_driven_with_target(record, phase, None)
}

/// Persist `last_driven_phase` and, when a delivery target was prepared, the frozen
/// `delivery_target` in the same locked write. A stale tick (a newer epoch/phase/track
/// or another project) writes nothing and journals the skip.
pub fn mark_driven_with_target(
    record: &ProjectRecord,
    phase: &str,
    target: Option<&execution_repo::ResolvedExecution>,
) -> Result<()> {
    with_run_state_lock(record, || {
        let mut state = load_run_state(record)?;
        if let Some(target) = target
            && (state.project_id != target.project_id
                || state.run_epoch != target.run_epoch
                || state.phase != target.phase
                || state.track_id.as_deref() != Some(target.track_id.as_str()))
        {
            crate::progress_log::append(
                record,
                "inject",
                &format!("inject: stale target skipped phase={phase}"),
            );
            return Ok(());
        }
        state.last_driven_phase = Some(phase.into());
        if let Some(target) = target {
            state.delivery_target = Some(target.clone());
        }
        state.updated_at = chrono::Utc::now();
        save_run_state(record, &state)?;
        let track = state.track_id.as_deref().unwrap_or("-");
        crate::progress_log::append(record, "inject", &format!("track={track} phase={phase}"));
        Ok(())
    })
}

const IMPLEMENT_GATE_KIND: &str = "implement-gate";

/// Implement gate: the `implement` phase is done only when its work is committed.
///
/// `success` from the leg is not evidence. Observed on 0554 (2026-10-20): the leg wrote
/// 18 files (+1938/-211) and committed nothing, so the cross-model gate failed twice on
/// "no EXEC commit, worktree dirty", the `address-findings` budget was exhausted, and the
/// next epoch's fold -> implement checkpoint refused the dirty tree. Every refusal named a
/// phase other than the one that failed, and the cost was two wasted legs plus a manual
/// commit on the operator.
///
/// Fail-open when git cannot be read (no execution repo, unreadable HEAD): the checkpoint
/// and `harness::no_change` remain the backstops, matching that guard's posture.
fn implement_artifact_gate(record: &ProjectRecord, state: &mut RunState) -> Option<String> {
    // Frozen first; otherwise re-resolve the fold -> implement edge; otherwise keep the
    // existing fail-open `None` posture. A planning-only target has no product delivery.
    let gate = match execution_repo::gate_target(record, state) {
        Ok(gate) => gate,
        Err(_) => {
            let target =
                execution_repo::resolve_track_target(record, state, graph::PHASE_IMPLEMENT).ok()?;
            execution_repo::GateTarget {
                registered: execution_repo::registered_execution_target(record, &target),
                execution: target,
                provenance: execution_repo::TargetProvenance::ReResolved,
            }
        }
    };
    if gate.execution.planning_only {
        return None;
    }
    let cwd = gate.execution.sample_path.clone();
    let dirt = crate::checkpoint::committable_dirt(&cwd).ok()?;
    if !dirt.is_empty() {
        let shown = dirt.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
        let more = dirt.len().saturating_sub(5);
        let tail = if more > 0 {
            format!(" (+{more} more)")
        } else {
            String::new()
        };
        return Some(format!(
            "implement-gate: the implement leg reported success with {} uncommitted path(s): {shown}{tail}; commit the work before the phase can advance",
            dirt.len()
        ));
    }
    let head = crate::checkpoint::git_ok(&cwd, &["rev-parse", "HEAD"]).ok()?;
    let head = head.trim().to_string();
    if head.is_empty() {
        return None;
    }
    let base_ref = crate::checkpoint::ref_name(&record.id, state.run_epoch);
    let base = crate::checkpoint::git_ok(&cwd, &["rev-parse", "--verify", "--quiet", &base_ref])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    let message = if base == head {
        Some(format!(
            "implement-gate: the implement leg reported success but HEAD {head} is still the epoch checkpoint, so the phase produced no commit to review"
        ))
    } else {
        None
    };
    // Journal only once the repository reads succeeded; the memo rides the caller's save.
    execution_repo::journal_gate_target(record, state, &gate);
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ENV_COORDINATOR_HOME, ENV_OUTCOME_POLL_MS, test_env_lock};
    use crate::outcome::{FailureClass, OutcomeSource, write_and_apply};
    use crate::run::{self, run_stub, run_with_driver};
    use crate::state::{CiWatchState, STUB_PHASE_STOPPED, StatusView};
    use crate::watch::{poll_once, wait_for_outcome};
    use tempfile::tempdir;
    use uuid::Uuid;

    #[test]
    fn fold_counter_resets_while_the_pair_converges() {
        // A changing pair on every pass is progress: it never reaches the cap.
        let mut c = 0;
        c = fold_counter_step(c, false);
        c = fold_counter_step(c, false);
        c = fold_counter_step(c, false);
        assert_eq!(c, 0);
        assert!(c < MAX_FOLD_REVIEW_REENTRIES);
    }

    #[test]
    fn fold_counter_still_stops_a_repeated_pair() {
        // The identical pair reproduced on each pass is the real never-settles case.
        let mut c = 0;
        for _ in 0..MAX_FOLD_REVIEW_REENTRIES {
            c = fold_counter_step(c, true);
        }
        assert!(
            c >= MAX_FOLD_REVIEW_REENTRIES,
            "repeated pair must trip the cap: {c}"
        );
    }

    #[test]
    fn fold_withheld_status_caps_at_120_scalars() {
        assert_eq!(
            fold_withheld_status("Proposed"),
            "workflow: fold withheld (row status: Proposed)"
        );
        let exact = "b".repeat(120);
        assert_eq!(
            fold_withheld_status(&exact),
            format!("workflow: fold withheld (row status: {exact})")
        );
        let long = "a".repeat(121);
        let msg = fold_withheld_status(&long);
        let inside = msg
            .strip_prefix("workflow: fold withheld (row status: ")
            .and_then(|s| s.strip_suffix(')'))
            .unwrap();
        assert_eq!(inside.chars().count(), 120);
        assert!(inside.ends_with('\u{2026}'));
        assert!(!msg.contains("checkpoint"));
    }

    fn clean_exec() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = crate::git_command::command()
                .args(args)
                .current_dir(dir.path())
                .env("GIT_OPTIONAL_LOCKS", "0")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.email", "checkpoint-walk@example.com"]);
        git(&["config", "user.name", "checkpoint-walk"]);
        std::fs::write(dir.path().join("README.md"), b"seed\n").unwrap();
        git(&["add", "README.md"]);
        git(&["commit", "-m", "seed"]);
        dir
    }

    fn rec(path: &std::path::Path) -> crate::registry::ProjectRecord {
        crate::registry::ProjectRecord {
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
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn blocked_hold_helpers_cannot_release_or_mutate_the_held_state() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        let mut state = crate::state::RunState::idle(&r.id);
        state.status = crate::state::RunStatus::Stopped;
        state.track_id = Some("0098".into());
        state.phase = graph::PHASE_ADVANCE.into();
        state.run_epoch = 3;
        state.failure_class = Some(FailureClass::Blocked);
        state.last_event = "outcome: failure class=blocked".into();
        state.phase_started_at = Some(chrono::Utc::now());
        let snap = state.clone();

        auto_start(&mut state, "0100");
        assert_eq!(state.run_epoch, snap.run_epoch);
        assert_eq!(state.track_id, snap.track_id);
        assert_eq!(state.failure_class, snap.failure_class);
        assert_eq!(state.last_event, snap.last_event);

        let outcome = crate::outcome::PhaseOutcome::failure(
            graph::PHASE_CROSS_MODEL,
            FailureClass::Difficulty,
            OutcomeSource::Test,
            Some("gate".into()),
            None,
        );
        on_gate_retry(&r, &mut state, &outcome);
        assert_eq!(state.failure_class, snap.failure_class);
        assert_eq!(state.phase, snap.phase);
        assert_eq!(
            state.address_findings_attempts,
            snap.address_findings_attempts
        );

        assert!(finish_advance(&r, &mut state).is_none());
        assert_eq!(state.track_id, snap.track_id);
        assert_eq!(state.failure_class, snap.failure_class);

        apply_backlog_clear(&r, &mut state);
        assert_eq!(state.status, snap.status);
        assert_eq!(state.failure_class, snap.failure_class);
        assert_eq!(state.last_event, snap.last_event);
        assert_eq!(state.next_track, snap.next_track);
    }

    fn walk_until<F>(r: &crate::registry::ProjectRecord, mut pred: F) -> StatusView
    where
        F: FnMut(&StatusView) -> bool,
    {
        let mut last = run::status(r).unwrap();
        for _ in 0..24 {
            if pred(&last) {
                return last;
            }
            if let Some(v) = poll_once(r).unwrap() {
                last = v;
            } else {
                last = run::status(r).unwrap();
            }
        }
        last
    }

    #[test]
    fn run_starts_canonical_at_plan() {
        let _env = test_env_lock();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        let s = run::run(&r, Some("0008".into())).unwrap();
        assert_eq!(s.status, RunStatus::Running);
        assert_eq!(s.phase, graph::PHASE_PLAN);
        assert_eq!(
            s.workflow.as_ref().unwrap().id.as_deref(),
            Some(WORKFLOW_ID)
        );
        assert_eq!(s.track_id.as_deref(), Some("0008"));
    }

    const REUSE_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn seed_track(root: &std::path::Path, id: &str) -> std::path::PathBuf {
        let track = root.join("conductor").join(format!("{id}-Example"));
        std::fs::create_dir_all(&track).unwrap();
        track
    }

    fn seed_matching_receipt(root: &std::path::Path, id: &str) -> crate::registry::ProjectRecord {
        let r = rec(root);
        let track = seed_track(root, id);
        std::fs::write(track.join("spec.md"), b"spec-v1").unwrap();
        std::fs::write(track.join("plan.md"), b"plan-v1").unwrap();
        let skill = root
            .join(".agents")
            .join("skills")
            .join("review-track")
            .join("SKILL.md");
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(&skill, b"skill-v1").unwrap();
        std::fs::write(track.join("agy-review.md"), b"agy body\n").unwrap();
        std::fs::write(track.join("opencode-review.md"), b"oc body\n").unwrap();
        reuse::write_join_receipt(&r, id);
        r
    }

    fn install_open(branch: &str) -> reuse::TestProbeGuard {
        let branch = branch.to_string();
        reuse::install_test_probe(std::sync::Arc::new(move |_| {
            Ok(Some(reuse::ResumePr {
                state: "OPEN".into(),
                head_oid: REUSE_SHA.into(),
                head_ref: branch.clone(),
            }))
        }))
    }

    #[test]
    fn corrupt_receipt_is_plan() {
        let _env = test_env_lock();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        let track = seed_track(dir.path(), "0083");
        std::fs::write(track.join("reuse.json"), b"{").unwrap();
        let view = run::run(&r, Some("0083".into())).unwrap();
        assert_eq!(view.phase, graph::PHASE_PLAN);
        let log = std::fs::read_to_string(dir.path().join("status.md")).unwrap();
        assert!(log.contains("reuse: receipt unreadable"), "{log}");
    }

    #[test]
    fn matching_pr_selects_ci_wait() {
        let _env = test_env_lock();
        let dir = tempdir().unwrap();
        let r = seed_matching_receipt(dir.path(), "0083");
        reuse::note_pr(&r, "0083", 80, REUSE_SHA, "track/0083-Slug", None);
        let _probe = install_open("track/0083-Slug");
        let view = run::run(&r, Some("0083".into())).unwrap();
        assert_eq!(view.phase, graph::PHASE_CI_WAIT);
        assert_eq!(
            view.last_event,
            format!("run: started {WORKFLOW_ID} resume ci-wait pr 80")
        );
    }

    #[test]
    fn gh_error_selects_plan() {
        let _env = test_env_lock();
        let dir = tempdir().unwrap();
        let r = seed_matching_receipt(dir.path(), "0083");
        reuse::note_pr(&r, "0083", 80, REUSE_SHA, "track/0083-Slug", None);
        let _probe = reuse::install_test_probe(std::sync::Arc::new(|_| {
            Err(crate::error::CoordinatorError::Message(
                "gh auth required".into(),
            ))
        }));
        let view = run::run(&r, Some("0083".into())).unwrap();
        assert_eq!(view.phase, graph::PHASE_PLAN);
        assert!(
            dir.path()
                .join("conductor")
                .join("0083-Example")
                .join("agy-review.md")
                .is_file()
        );
    }

    #[test]
    fn closed_pr_selects_plan() {
        let _env = test_env_lock();
        let dir = tempdir().unwrap();
        let r = seed_matching_receipt(dir.path(), "0083");
        reuse::note_pr(&r, "0083", 80, REUSE_SHA, "track/0083-Slug", None);
        let _probe = reuse::install_test_probe(std::sync::Arc::new(|_| {
            Ok(Some(reuse::ResumePr {
                state: "CLOSED".into(),
                head_oid: REUSE_SHA.into(),
                head_ref: "track/0083-Slug".into(),
            }))
        }));
        let view = run::run(&r, Some("0083".into())).unwrap();
        assert_eq!(view.phase, graph::PHASE_PLAN);
    }

    #[test]
    fn foreign_head_ref_selects_plan() {
        let _env = test_env_lock();
        let dir = tempdir().unwrap();
        let r = seed_matching_receipt(dir.path(), "0083");
        reuse::note_pr(&r, "0083", 80, REUSE_SHA, "track/0083-Slug", None);
        let _probe = install_open("track/0084-Other");
        let view = run::run(&r, Some("0083".into())).unwrap();
        assert_eq!(view.phase, graph::PHASE_PLAN);
    }

    #[test]
    fn bare_track_ref_selects_ci_wait() {
        let _env = test_env_lock();
        let dir = tempdir().unwrap();
        let r = seed_matching_receipt(dir.path(), "0083");
        reuse::note_pr(&r, "0083", 80, REUSE_SHA, "track/0083", None);
        let _probe = install_open("track/0083");
        let view = run::run(&r, Some("0083".into())).unwrap();
        assert_eq!(view.phase, graph::PHASE_CI_WAIT);
    }

    #[test]
    fn probe_runs_outside_the_run_state_lock() {
        let _env = test_env_lock();
        let prev = std::env::var_os(crate::config::ENV_COORDINATOR_STATE_DIR);
        unsafe {
            std::env::remove_var(crate::config::ENV_COORDINATOR_STATE_DIR);
        }
        struct RestoreStateDir(Option<std::ffi::OsString>);
        impl Drop for RestoreStateDir {
            fn drop(&mut self) {
                unsafe {
                    match &self.0 {
                        Some(v) => {
                            std::env::set_var(crate::config::ENV_COORDINATOR_STATE_DIR, v);
                        }
                        None => std::env::remove_var(crate::config::ENV_COORDINATOR_STATE_DIR),
                    }
                }
            }
        }
        let _restore = RestoreStateDir(prev);
        let dir = tempdir().unwrap();
        let r = seed_matching_receipt(dir.path(), "0083");
        reuse::note_pr(&r, "0083", 80, REUSE_SHA, "track/0083-Slug", None);
        let lock = crate::state::resolve_state_dir(&r)
            .unwrap()
            .join(".run-state.lock");
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = seen.clone();
        let _probe = reuse::install_test_probe(std::sync::Arc::new(move |_| {
            if !lock.exists() {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(Some(reuse::ResumePr {
                state: "OPEN".into(),
                head_oid: REUSE_SHA.into(),
                head_ref: "track/0083-Slug".into(),
            }))
        }));
        let view = run::run(&r, Some("0083".into())).unwrap();
        assert!(
            seen.load(std::sync::atomic::Ordering::SeqCst),
            "run-state lock was visible during gh probe"
        );
        assert_eq!(view.phase, graph::PHASE_CI_WAIT);
    }

    #[test]
    fn resume_ci_wait_plants_pr_number_without_publish_latch() {
        let _env = test_env_lock();
        let dir = tempdir().unwrap();
        let r = seed_matching_receipt(dir.path(), "0083");
        reuse::note_pr(&r, "0083", 80, REUSE_SHA, "track/0083-Slug", None);
        let _probe = install_open("track/0083-Slug");
        let view = run::run(&r, Some("0083".into())).unwrap();
        assert_eq!(view.phase, graph::PHASE_CI_WAIT);
        let state = crate::state::load_run_state(&r).unwrap();
        let ci = state.ci.expect("ci planted");
        assert_eq!(ci.pr_number, Some(80));
        assert_eq!(ci.head_sha.as_deref(), Some(REUSE_SHA));
        assert!(ci.publish_attempted_sha.is_none());
        assert!(ci.merge.is_none());
        assert!(ci.last_poll_at.is_none());
        assert_eq!(ci.publish_transient_attempts, 0);
        assert!(ci.publish_transient_sha.is_none());
    }

    #[test]
    fn prior_archive_survives_clear() {
        let _env = test_env_lock();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        let track = seed_track(dir.path(), "0083");
        run::run(&r, Some("0083".into())).unwrap();
        run::stop(&r).unwrap();
        std::fs::write(track.join("agy-review.md"), b"kept-review").unwrap();
        assert!(!track.join("prior").exists());
        run::run(&r, Some("0083".into())).unwrap();
        let archived = track.join("prior").join("1-agy-review.md");
        assert_eq!(std::fs::read(&archived).unwrap(), b"kept-review");
        assert!(!track.join("agy-review.md").exists());
    }

    #[test]
    fn next_track_cleared_on_run_track_retained() {
        let _env = test_env_lock();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_stub(&r, Some("0004".into())).unwrap();
        let o = PhaseOutcome::success(
            crate::state::STUB_PHASE_ACTIVE,
            OutcomeSource::Test,
            None,
            Some("0006".into()),
            None,
        );
        write_and_apply(&r, o).unwrap();
        let after = run::status(&r).unwrap();
        assert_eq!(after.next_track.as_deref(), Some("0006"));
        let s = run::run(&r, None).unwrap();
        assert!(s.next_track.is_none());
        assert_eq!(s.track_id.as_deref(), Some("0004"));
        assert_eq!(s.phase, graph::PHASE_PLAN);
    }

    #[test]
    fn stub_driver_walks_full_graph_to_idle() {
        let _guard = test_env_lock();
        unsafe {
            std::env::set_var(ENV_OUTCOME_POLL_MS, "10");
            std::env::set_var(ENV_PHASE_TIMEOUT_SECS, "30");
        }
        let dir = tempdir().unwrap();
        let repo = clean_exec();
        let mut r = rec(dir.path());
        r.execution_repo = Some(repo.path().to_path_buf());
        conductor_md::write_ready_fixture(dir.path(), "0001").unwrap();
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::Stub).unwrap();
        let view = wait_for_outcome(&r, Some(15)).unwrap();
        assert_eq!(view.status, RunStatus::Idle);
        assert!(
            view.last_event.contains("backlog clear"),
            "last_event={}",
            view.last_event
        );
        unsafe {
            std::env::remove_var(ENV_OUTCOME_POLL_MS);
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
        }
    }

    #[test]
    fn skip_events_visible() {
        let dir = tempdir().unwrap();
        let repo = clean_exec();
        let mut r = rec(dir.path());
        r.execution_repo = Some(repo.path().to_path_buf());
        conductor_md::write_ready_fixture(dir.path(), "0001").unwrap();
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::Stub).unwrap();
        let after_xmodel = walk_until(&r, |v| v.last_event.contains("cross-model: stub"));
        assert!(
            after_xmodel
                .last_event
                .contains("cross-model: stub (no review)"),
            "last_event={}",
            after_xmodel.last_event
        );
        assert!(!after_xmodel.last_event.contains("skip: deferred to"));
        let after_ci = walk_until(&r, |v| v.last_event.contains("ci-wait: stub"));
        assert!(
            after_ci.last_event.contains("ci-wait: stub (no gh)"),
            "last_event={}",
            after_ci.last_event
        );
        assert!(!after_ci.last_event.contains("skip: deferred to 0010"));
        assert!(!after_ci.last_event.contains("skip: deferred to 0011"));
    }

    #[test]
    fn pause_blocks_tick() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, None, WorkflowDriver::Stub).unwrap();
        run::pause(&r).unwrap();
        let tick = poll_once(&r).unwrap();
        assert!(tick.is_none());
        let s = run::status(&r).unwrap();
        assert_eq!(s.status, RunStatus::Paused);
        assert_eq!(s.phase, graph::PHASE_PLAN);
    }

    #[test]
    fn canonical_failure_keeps_phase_id() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, None, WorkflowDriver::FileWait).unwrap();
        let o = PhaseOutcome::failure(
            graph::PHASE_PLAN,
            FailureClass::Difficulty,
            OutcomeSource::Cli,
            Some("hard".into()),
            None,
        );
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.phase, graph::PHASE_PLAN);
        assert_eq!(view.failure_class, Some(FailureClass::Difficulty));
    }

    fn jump_cross_model_file_wait(r: &crate::registry::ProjectRecord, track: &str) {
        run_with_driver(r, Some(track.into()), WorkflowDriver::FileWait).unwrap();
        let mut s = load_run_state(r).unwrap();
        s.phase = graph::PHASE_CROSS_MODEL.into();
        save_run_state(r, &s).unwrap();
    }

    #[test]
    fn file_wait_cross_model_difficulty_bounces_to_address_findings() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("conductor").join("0031-Example")).unwrap();
        let r = rec(dir.path());
        jump_cross_model_file_wait(&r, "0031");
        let o = PhaseOutcome::failure(
            graph::PHASE_CROSS_MODEL,
            FailureClass::Difficulty,
            OutcomeSource::File,
            Some("cross-model: gate failed (codex)".into()),
            None,
        );
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.phase, graph::PHASE_ADDRESS_FINDINGS);
        assert_eq!(view.failure_class, None);
        assert_eq!(
            view.workflow.as_ref().map(|w| w.address_findings_attempts),
            Some(1)
        );
        assert!(crate::notify::artifact::existing_path(&r).is_none());
        let st = load_run_state(&r).unwrap();
        assert!(st.review.is_none());
        let json = serde_json::to_string(&view.workflow).unwrap();
        assert!(
            json.contains("\"address_findings_attempts\":1"),
            "workflow json={json}"
        );
        run::stop(&r).unwrap();
        let fresh = run_with_driver(&r, None, WorkflowDriver::FileWait).unwrap();
        let fresh_json = serde_json::to_string(&fresh.workflow).unwrap();
        assert!(
            !fresh_json.contains("address_findings_attempts"),
            "fresh workflow json={fresh_json}"
        );
        assert_eq!(load_run_state(&r).unwrap().address_findings_attempts, 0);
    }

    #[test]
    fn bounce_archives_both_live_copies_and_deletes_live_names() {
        let dir = tempdir().unwrap();
        let track = dir.path().join("conductor").join("0031-Example");
        std::fs::create_dir_all(&track).unwrap();
        let r = rec(dir.path());
        jump_cross_model_file_wait(&r, "0031");
        let token = "UNIQUE_TOKEN_gate1_archive";
        let live_track = track.join("review.codex.md");
        std::fs::write(&live_track, token).unwrap();
        let reviews = crate::state::resolve_state_dir(&r).unwrap().join("reviews");
        std::fs::create_dir_all(&reviews).unwrap();
        let live_state = reviews.join("cross-model-codex.md");
        std::fs::write(&live_state, token).unwrap();
        let o = PhaseOutcome::failure(
            graph::PHASE_CROSS_MODEL,
            FailureClass::Difficulty,
            OutcomeSource::File,
            Some("gate failed".into()),
            None,
        );
        write_and_apply(&r, o).unwrap();
        let gate_track = track.join("review.codex.gate1.md");
        let gate_state = reviews.join("cross-model-codex.gate1.md");
        assert_eq!(std::fs::read_to_string(&gate_track).unwrap(), token);
        assert_eq!(std::fs::read_to_string(&gate_state).unwrap(), token);
        assert!(!live_track.exists());
        assert!(!live_state.exists());
        assert!(!track.join("review.md").exists());
    }

    #[test]
    fn bounce_does_not_archive_review_codex_fail_md() {
        let dir = tempdir().unwrap();
        let track = dir.path().join("conductor").join("0031-Example");
        std::fs::create_dir_all(&track).unwrap();
        let r = rec(dir.path());
        jump_cross_model_file_wait(&r, "0031");
        let fail_copy = track.join("review.codex.fail.md");
        std::fs::write(&fail_copy, "prior FAIL audit").unwrap();
        std::fs::write(track.join("review.codex.md"), "this run gate").unwrap();
        let reviews = crate::state::resolve_state_dir(&r).unwrap().join("reviews");
        std::fs::create_dir_all(&reviews).unwrap();
        std::fs::write(reviews.join("cross-model-codex.md"), "this run gate").unwrap();
        let o = PhaseOutcome::failure(
            graph::PHASE_CROSS_MODEL,
            FailureClass::Difficulty,
            OutcomeSource::File,
            Some("gate failed".into()),
            None,
        );
        write_and_apply(&r, o).unwrap();
        assert_eq!(
            std::fs::read_to_string(&fail_copy).unwrap(),
            "prior FAIL audit"
        );
        assert!(!track.join("review.codex.fail.gate1.md").exists());
        assert_eq!(
            std::fs::read_to_string(track.join("review.codex.gate1.md")).unwrap(),
            "this run gate"
        );
        assert!(!track.join("review.codex.md").exists());
    }

    #[test]
    fn bounce_ignores_leftover_track_dir_review_without_state_pair() {
        let dir = tempdir().unwrap();
        let track = dir.path().join("conductor").join("0031-Example");
        std::fs::create_dir_all(&track).unwrap();
        let r = rec(dir.path());
        jump_cross_model_file_wait(&r, "0031");
        std::fs::write(track.join("review.claude.md"), "prior run leftover live").unwrap();
        std::fs::write(
            track.join("review.claude.gate1.md"),
            "prior run leftover gate",
        )
        .unwrap();
        std::fs::write(track.join("review.codex.md"), "this run").unwrap();
        let reviews = crate::state::resolve_state_dir(&r).unwrap().join("reviews");
        std::fs::create_dir_all(&reviews).unwrap();
        std::fs::write(reviews.join("cross-model-codex.md"), "this run").unwrap();
        let o = PhaseOutcome::failure(
            graph::PHASE_CROSS_MODEL,
            FailureClass::Difficulty,
            OutcomeSource::File,
            Some("gate failed".into()),
            None,
        );
        write_and_apply(&r, o).unwrap();
        assert_eq!(
            std::fs::read_to_string(track.join("review.claude.md")).unwrap(),
            "prior run leftover live"
        );
        assert_eq!(
            std::fs::read_to_string(track.join("review.claude.gate1.md")).unwrap(),
            "prior run leftover gate"
        );
        assert_eq!(
            std::fs::read_to_string(track.join("review.codex.gate1.md")).unwrap(),
            "this run"
        );
        assert!(!track.join("review.codex.md").exists());
        assert!(!reviews.join("cross-model-claude.gate1.md").exists());
    }

    #[test]
    fn bounce_without_track_dir_still_archives_state_dir() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        jump_cross_model_file_wait(&r, "0031");
        let token = "UNIQUE_TOKEN_no_track_dir";
        let reviews = crate::state::resolve_state_dir(&r).unwrap().join("reviews");
        std::fs::create_dir_all(&reviews).unwrap();
        let live_state = reviews.join("cross-model-codex.md");
        std::fs::write(&live_state, token).unwrap();
        let o = PhaseOutcome::failure(
            graph::PHASE_CROSS_MODEL,
            FailureClass::Difficulty,
            OutcomeSource::File,
            Some("gate failed".into()),
            None,
        );
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.phase, graph::PHASE_ADDRESS_FINDINGS);
        assert_eq!(
            std::fs::read_to_string(reviews.join("cross-model-codex.gate1.md")).unwrap(),
            token
        );
        assert!(!live_state.exists());
    }

    #[test]
    fn cap_exhaust_stops_with_artifact() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        jump_cross_model_file_wait(&r, "0031");
        let mut s = load_run_state(&r).unwrap();
        s.address_findings_attempts = graph::ADDRESS_FINDINGS_CAP;
        save_run_state(&r, &s).unwrap();
        let o = PhaseOutcome::failure(
            graph::PHASE_CROSS_MODEL,
            FailureClass::Difficulty,
            OutcomeSource::File,
            Some("cross-model: gate failed (codex)".into()),
            None,
        );
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.phase, graph::PHASE_CROSS_MODEL);
        assert_eq!(view.failure_class, Some(FailureClass::Difficulty));
        assert!(
            view.last_event.contains("address-findings exhausted"),
            "last_event={}",
            view.last_event
        );
        assert!(crate::notify::artifact::existing_path(&r).is_some());
    }

    #[test]
    fn cross_model_timeout_does_not_bounce() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        jump_cross_model_file_wait(&r, "0031");
        let o = PhaseOutcome::failure(
            graph::PHASE_CROSS_MODEL,
            FailureClass::Timeout,
            OutcomeSource::Timeout,
            Some("timed out".into()),
            None,
        );
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.phase, graph::PHASE_CROSS_MODEL);
        assert_eq!(view.failure_class, Some(FailureClass::Timeout));
        assert_eq!(view.phase, graph::PHASE_CROSS_MODEL);
        assert_ne!(view.phase, graph::PHASE_ADDRESS_FINDINGS);
    }

    #[test]
    fn stop_during_plan_sets_stub_stopped() {
        let _env = test_env_lock();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run::run(&r, Some("0008".into())).unwrap();
        let s = run::stop(&r).unwrap();
        assert_eq!(s.phase, STUB_PHASE_STOPPED);
        assert_eq!(s.status, RunStatus::Stopped);
    }

    #[test]
    fn fresh_run_clears_stale_review_artifacts() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, None, WorkflowDriver::FileWait).unwrap();
        let roles = crate::outcome::outcome_roles_dir(&r).unwrap();
        std::fs::create_dir_all(&roles).unwrap();
        std::fs::write(roles.join("agy.json"), b"{}").unwrap();
        crate::workflow::drive::write_review_markdown(&r, "agy", Some("stale\n")).unwrap();
        run::stop(&r).unwrap();
        run_with_driver(&r, None, WorkflowDriver::FileWait).unwrap();
        assert!(
            !roles.join("agy.json").exists(),
            "stale role outcome must not survive run"
        );
        let review = crate::workflow::bundle::review_file(&r, "agy").unwrap();
        assert!(
            !review.exists(),
            "stale review markdown must not survive run"
        );
    }

    #[test]
    fn empty_next_track_treated_as_null() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, None, WorkflowDriver::FileWait).unwrap();
        let o = PhaseOutcome::success(
            graph::PHASE_PLAN,
            OutcomeSource::Test,
            None,
            Some("   ".into()),
            None,
        );
        let view = write_and_apply(&r, o).unwrap();
        assert!(view.next_track.is_none());
        assert_eq!(view.phase, graph::PHASE_PLAN_REVIEW);
    }

    #[test]
    fn advance_null_idles_backlog_clear() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, None, WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        save_run_state(&r, &state).unwrap();
        let o = PhaseOutcome::success(graph::PHASE_ADVANCE, OutcomeSource::Test, None, None, None);
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.status, RunStatus::Idle);
        assert!(view.last_event.contains("backlog clear"));
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap_or_default();
        assert!(
            !log.contains("override next_track"),
            "planner-null + no Ready must not journal override: {log}"
        );
    }

    fn write_failure_md(r: &crate::registry::ProjectRecord, track: &str, epoch: u64) {
        let event = crate::notify::NotifyEvent {
            project_id: r.id.clone(),
            track_id: Some(track.into()),
            phase: "plan".into(),
            failure_class: FailureClass::HarnessCrash,
            message: Some("fixture".into()),
            last_event: "fixture".into(),
            artifact_path: crate::notify::artifact::path(r).unwrap(),
            written_at: chrono::Utc::now(),
            run_epoch: epoch,
        };
        crate::notify::artifact::write(r, &event).unwrap();
    }

    #[test]
    fn advance_backlog_clear_same_track_clears_artifact() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0040".into()), WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        state.failure_class = Some(FailureClass::HarnessCrash);
        state.next_track = None;
        save_run_state(&r, &state).unwrap();
        write_failure_md(&r, "0040", state.run_epoch);
        let o = PhaseOutcome::success(graph::PHASE_ADVANCE, OutcomeSource::Test, None, None, None);
        let view = write_and_apply(&r, o).unwrap();
        assert!(
            crate::notify::artifact::existing_path(&r).is_none(),
            "same-track artifact must clear"
        );
        assert!(view.failure_class.is_none());
        assert_eq!(view.last_event, LAST_EVENT_BACKLOG_CLEAR);
        assert!(should_pick_next_ready(&ReadyPickState::from(&view)));
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap();
        assert!(log.contains(crate::notify::AUTO_CLEAR_DETAIL));
    }

    #[test]
    fn advance_backlog_clear_cross_track_keeps_artifact() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0040".into()), WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        state.failure_class = Some(FailureClass::HarnessCrash);
        state.next_track = None;
        save_run_state(&r, &state).unwrap();
        write_failure_md(&r, "0038", state.run_epoch);
        let o = PhaseOutcome::success(graph::PHASE_ADVANCE, OutcomeSource::Test, None, None, None);
        let view = write_and_apply(&r, o).unwrap();
        assert!(
            crate::notify::artifact::existing_path(&r).is_some(),
            "cross-track leftover must stay"
        );
        assert_eq!(view.last_event, LAST_EVENT_BACKLOG_CLEAR);
    }

    #[test]
    fn advance_valid_auto_starts() {
        let dir = tempdir().unwrap();
        write_both_ready(dir.path());
        let mut r = rec(dir.path());
        r.auto_start = AutoStartPolicy::Full;
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        save_run_state(&r, &state).unwrap();
        let o = PhaseOutcome::success(
            graph::PHASE_ADVANCE,
            OutcomeSource::Test,
            None,
            Some("0002".into()),
            None,
        );
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.phase, graph::PHASE_PLAN);
        assert_eq!(view.track_id.as_deref(), Some("0002"));
        assert!(view.last_event.contains("auto-start"));
        assert!(view.next_track.is_none());
        assert!(view.run_epoch >= 2);
        assert_eq!(
            view.workflow
                .as_ref()
                .map(|w| w.address_findings_attempts)
                .unwrap_or(0),
            0
        );
        assert_eq!(load_run_state(&r).unwrap().address_findings_attempts, 0);
    }

    #[test]
    fn advance_full_proposed_rewrite_does_not_autostart() {
        let dir = tempdir().unwrap();
        write_both_ready(dir.path());
        let mut r = rec(dir.path());
        r.auto_start = AutoStartPolicy::Full;
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::FileWait).unwrap();
        let captured = load_run_state(&r).unwrap().sticky_ready_ids.clone();
        assert!(captured.iter().any(|id| id == "0002"), "{captured:?}");
        let cond = dir.path().join("conductor").join("conductor.md");
        let rewritten = "\
| Track | Execution path | Status | Summary |\n\
| --- | --- | --- | --- |\n\
| [0001-Example](0001-Example/spec.md) | `.` | **In progress** | one |\n\
| [0002-Next](0002-Next/spec.md) | `.` | **Proposed — placeholder, needs full spec/plan pass** | next |\n";
        std::fs::write(&cond, rewritten).unwrap();
        let before = std::fs::read(&cond).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        save_run_state(&r, &state).unwrap();
        let o = PhaseOutcome::success(graph::PHASE_ADVANCE, OutcomeSource::Test, None, None, None);
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.status, RunStatus::Idle);
        assert_eq!(view.last_event, LAST_EVENT_BACKLOG_CLEAR);
        assert_ne!(view.track_id.as_deref(), Some("0002"));
        let after = std::fs::read(&cond).unwrap();
        assert_eq!(before, after, "overlay must not write conductor.md");
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap();
        assert!(!log.contains("sticky Ready 0002"), "{log}");
        assert!(!log.contains("auto-start 0002"), "{log}");
    }

    #[test]
    fn advance_full_auto_starts_sticky_after_unaliased_rewrite() {
        let dir = tempdir().unwrap();
        write_both_ready(dir.path());
        let mut r = rec(dir.path());
        r.auto_start = AutoStartPolicy::Full;
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::FileWait).unwrap();
        let captured = load_run_state(&r).unwrap().sticky_ready_ids.clone();
        assert!(captured.iter().any(|id| id == "0002"), "{captured:?}");
        let cond = dir.path().join("conductor").join("conductor.md");
        let rewritten = "\
| Track | Execution path | Status | Summary |\n\
| --- | --- | --- | --- |\n\
| [0001-Example](0001-Example/spec.md) | `.` | **In progress** | one |\n\
| [0002-Next](0002-Next/spec.md) | `.` | **Ready — folded @ sha1234** | next |\n";
        std::fs::write(&cond, rewritten).unwrap();
        let before = std::fs::read(&cond).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        save_run_state(&r, &state).unwrap();
        let o = PhaseOutcome::success(graph::PHASE_ADVANCE, OutcomeSource::Test, None, None, None);
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.track_id.as_deref(), Some("0002"));
        assert!(
            view.last_event.contains("auto-start 0002"),
            "{}",
            view.last_event
        );
        let after = std::fs::read(&cond).unwrap();
        assert_eq!(before, after, "overlay must not write conductor.md");
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap();
        assert!(
            log.contains("sticky Ready 0002 (file=Ready - folded @ sha1234)"),
            "{log}"
        );
    }

    #[test]
    fn advance_full_never_ready_tail_backlog_clears() {
        let dir = tempdir().unwrap();
        let cond = dir.path().join("conductor");
        std::fs::create_dir_all(cond.join("0001-Example")).unwrap();
        std::fs::create_dir_all(cond.join("0002-Next")).unwrap();
        std::fs::write(
            cond.join("conductor.md"),
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0001-Example](0001-Example/spec.md) | `.` | **Ready — not started** | one |\n\
             | [0002-Next](0002-Next/spec.md) | `.` | **Proposed — placeholder, needs full spec/plan pass** | next |\n",
        )
        .unwrap();
        let mut r = rec(dir.path());
        r.auto_start = AutoStartPolicy::Full;
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::FileWait).unwrap();
        let sticky = load_run_state(&r).unwrap().sticky_ready_ids.clone();
        assert!(sticky.iter().any(|id| id == "0001"), "{sticky:?}");
        assert!(!sticky.iter().any(|id| id == "0002"), "{sticky:?}");
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        save_run_state(&r, &state).unwrap();
        let o = PhaseOutcome::success(graph::PHASE_ADVANCE, OutcomeSource::Test, None, None, None);
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.status, RunStatus::Idle);
        assert_eq!(view.last_event, LAST_EVENT_BACKLOG_CLEAR);
    }

    #[test]
    fn run_with_origin_captures_ready_ids() {
        let dir = tempdir().unwrap();
        write_both_ready(dir.path());
        let r = rec(dir.path());
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::FileWait).unwrap();
        let sticky = load_run_state(&r).unwrap().sticky_ready_ids;
        assert!(sticky.iter().any(|id| id == "0001"), "{sticky:?}");
        assert!(sticky.iter().any(|id| id == "0002"), "{sticky:?}");
    }

    #[test]
    fn advance_unknown_id_backlog_clears_without_fail() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        save_run_state(&r, &state).unwrap();
        let o = PhaseOutcome::success(
            graph::PHASE_ADVANCE,
            OutcomeSource::Test,
            None,
            Some("does-not-exist".into()),
            None,
        );
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.status, RunStatus::Idle);
        assert!(view.failure_class.is_none());
        assert_eq!(view.last_event, LAST_EVENT_BACKLOG_CLEAR);
        assert!(!view.last_event.contains("invalid next_track"));
        assert_eq!(view.track_id.as_deref(), Some("0001"));
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap();
        assert!(
            log.contains("override next_track does-not-exist → (none)"),
            "{log}"
        );
    }

    #[test]
    fn pause_holds_auto_start_until_resume() {
        let dir = tempdir().unwrap();
        write_two_ready(dir.path());
        let mut r = rec(dir.path());
        r.auto_start = AutoStartPolicy::Full;
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::FileWait).unwrap();
        run::pause(&r).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        save_run_state(&r, &state).unwrap();
        let o = PhaseOutcome::success(
            graph::PHASE_ADVANCE,
            OutcomeSource::Test,
            None,
            Some("0002".into()),
            None,
        );
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.status, RunStatus::Paused);
        assert_eq!(view.phase, graph::PHASE_ADVANCE);
        assert_eq!(view.next_track.as_deref(), Some("0002"));
        let resumed = run::resume(&r).unwrap();
        assert_eq!(resumed.status, RunStatus::Running);
        assert_eq!(resumed.phase, graph::PHASE_PLAN);
        assert_eq!(resumed.track_id.as_deref(), Some("0002"));
    }

    fn write_blank_line_completed_and_ready(dir: &std::path::Path) {
        let cond = dir.join("conductor");
        std::fs::create_dir_all(cond.join("0001-Example")).unwrap();
        std::fs::create_dir_all(cond.join("0002-Next")).unwrap();
        std::fs::write(
            cond.join("conductor.md"),
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0001-Example](0001-Example/spec.md) | `.` | **Completed** | done |\n\
             \n\
             | [0002-Next](0002-Next/spec.md) | `.` | **Ready — not started** | next |\n",
        )
        .unwrap();
    }

    fn write_thematic_break_completed_and_ready(dir: &std::path::Path) {
        let cond = dir.join("conductor");
        std::fs::create_dir_all(cond.join("0001-Example")).unwrap();
        std::fs::create_dir_all(cond.join("0002-Next")).unwrap();
        std::fs::write(
            cond.join("conductor.md"),
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0001-Example](0001-Example/spec.md) | `.` | **Completed** | done |\n\
             ---\n\
             | [0002-Next](0002-Next/spec.md) | `.` | **Ready — not started** | next |\n",
        )
        .unwrap();
    }

    fn write_completed_and_proposed(dir: &std::path::Path) {
        let cond = dir.join("conductor");
        std::fs::create_dir_all(cond.join("0001-Example")).unwrap();
        std::fs::create_dir_all(cond.join("0002-Next")).unwrap();
        std::fs::write(
            cond.join("conductor.md"),
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0001-Example](0001-Example/spec.md) | `.` | **Completed** | done |\n\
             | [0002-Next](0002-Next/spec.md) | `.` | **Proposed — placeholder, needs full spec/plan pass** | later |\n",
        )
        .unwrap();
    }

    fn write_two_ready(dir: &std::path::Path) {
        let cond = dir.join("conductor");
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
    }

    fn write_both_ready(dir: &std::path::Path) {
        let cond = dir.join("conductor");
        std::fs::create_dir_all(cond.join("0001-Example")).unwrap();
        std::fs::create_dir_all(cond.join("0002-Next")).unwrap();
        std::fs::write(
            cond.join("conductor.md"),
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0001-Example](0001-Example/spec.md) | `.` | **Ready — not started** | one |\n\
             | [0002-Next](0002-Next/spec.md) | `.` | **Ready — not started** | next |\n",
        )
        .unwrap();
    }

    fn write_only_ready(dir: &std::path::Path, id: &str, slug: &str) {
        let cond = dir.join("conductor");
        std::fs::create_dir_all(cond.join(format!("{id}-{slug}"))).unwrap();
        std::fs::write(
            cond.join("conductor.md"),
            format!(
                "| Track | Execution path | Status | Summary |\n\
                 | --- | --- | --- | --- |\n\
                 | [{id}-{slug}]({id}-{slug}/spec.md) | `.` | **Ready — not started** | only |\n"
            ),
        )
        .unwrap();
    }

    fn write_completed_and_two_ready(dir: &std::path::Path) {
        let cond = dir.join("conductor");
        std::fs::create_dir_all(cond.join("0001-Example")).unwrap();
        std::fs::create_dir_all(cond.join("0002-Next")).unwrap();
        std::fs::create_dir_all(cond.join("0003-Later")).unwrap();
        std::fs::write(
            cond.join("conductor.md"),
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0001-Example](0001-Example/spec.md) | `.` | **Completed** | done |\n\
             | [0002-Next](0002-Next/spec.md) | `.` | **Ready — not started** | next |\n\
             | [0003-Later](0003-Later/spec.md) | `.` | **Ready — not started** | skip |\n",
        )
        .unwrap();
    }

    fn write_ready_row_without_dir(dir: &std::path::Path) {
        let cond = dir.join("conductor");
        std::fs::create_dir_all(cond.join("0001-Example")).unwrap();
        std::fs::write(
            cond.join("conductor.md"),
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0001-Example](0001-Example/spec.md) | `.` | **Completed** | done |\n\
             | [0002-Next](0002-Next/spec.md) | `.` | **Ready — not started** | ghost |\n",
        )
        .unwrap();
    }

    #[test]
    fn advance_planner_skip_starts_first_ready() {
        let dir = tempdir().unwrap();
        write_completed_and_two_ready(dir.path());
        let mut r = rec(dir.path());
        r.auto_start = AutoStartPolicy::Full;
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        save_run_state(&r, &state).unwrap();
        let o = PhaseOutcome::success(
            graph::PHASE_ADVANCE,
            OutcomeSource::Test,
            None,
            Some("0003".into()),
            None,
        );
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.track_id.as_deref(), Some("0002"));
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.phase, graph::PHASE_PLAN);
        assert!(view.last_event.contains("auto-start 0002"), "{view:?}");
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap();
        assert!(log.contains("override next_track 0003 → 0002"), "{log}");
    }

    #[test]
    fn advance_planner_null_starts_ready() {
        let dir = tempdir().unwrap();
        write_two_ready(dir.path());
        let mut r = rec(dir.path());
        r.auto_start = AutoStartPolicy::Full;
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        state.next_track = None;
        save_run_state(&r, &state).unwrap();
        let o = PhaseOutcome::success(graph::PHASE_ADVANCE, OutcomeSource::Test, None, None, None);
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.track_id.as_deref(), Some("0002"));
        assert_eq!(view.status, RunStatus::Running);
        assert!(view.last_event.contains("auto-start 0002"));
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap();
        assert!(log.contains("override next_track null → 0002"), "{log}");
    }

    #[test]
    fn advance_full_blank_line_table_starts_ready() {
        let dir = tempdir().unwrap();
        write_blank_line_completed_and_ready(dir.path());
        let mut r = rec(dir.path());
        r.auto_start = AutoStartPolicy::Full;
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        state.next_track = None;
        save_run_state(&r, &state).unwrap();
        let o = PhaseOutcome::success(graph::PHASE_ADVANCE, OutcomeSource::Test, None, None, None);
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.track_id.as_deref(), Some("0002"));
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.phase, graph::PHASE_PLAN);
        assert!(view.last_event.contains("auto-start 0002"), "{view:?}");
        assert!(!view.last_event.contains("backlog clear"), "{view:?}");
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap();
        assert!(log.contains("override next_track null → 0002"), "{log}");
    }

    #[test]
    fn advance_full_thematic_break_table_starts_ready() {
        let dir = tempdir().unwrap();
        write_thematic_break_completed_and_ready(dir.path());
        let mut r = rec(dir.path());
        r.auto_start = AutoStartPolicy::Full;
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        state.next_track = None;
        save_run_state(&r, &state).unwrap();
        let o = PhaseOutcome::success(graph::PHASE_ADVANCE, OutcomeSource::Test, None, None, None);
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.track_id.as_deref(), Some("0002"));
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.phase, graph::PHASE_PLAN);
        assert!(view.last_event.contains("auto-start 0002"), "{view:?}");
        assert!(!view.last_event.contains("backlog clear"), "{view:?}");
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap();
        assert!(log.contains("override next_track null → 0002"), "{log}");
    }

    #[test]
    fn advance_empty_ready_walk_journals_row_count() {
        let dir = tempdir().unwrap();
        write_completed_and_proposed(dir.path());
        let mut r = rec(dir.path());
        r.auto_start = AutoStartPolicy::Full;
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        state.next_track = None;
        save_run_state(&r, &state).unwrap();
        let o = PhaseOutcome::success(graph::PHASE_ADVANCE, OutcomeSource::Test, None, None, None);
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.status, RunStatus::Idle);
        assert_eq!(view.last_event, LAST_EVENT_BACKLOG_CLEAR);
        assert_eq!(view.track_id.as_deref(), Some("0001"));
        assert!(view.next_track.is_none());
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap();
        assert!(
            log.contains("no Ready — not started track in conductor.md"),
            "{log}"
        );
        assert!(log.contains("2 rows parsed"), "{log}");
    }

    #[test]
    fn advance_missing_planner_id_parks_or_starts_ready() {
        let dir = tempdir().unwrap();
        write_two_ready(dir.path());
        let r = rec(dir.path());
        assert_eq!(r.auto_start, AutoStartPolicy::Hitl);
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        save_run_state(&r, &state).unwrap();
        let o = PhaseOutcome::success(
            graph::PHASE_ADVANCE,
            OutcomeSource::Test,
            None,
            Some("0099".into()),
            None,
        );
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.status, RunStatus::Idle);
        assert_eq!(view.last_event, LAST_EVENT_BACKLOG_CLEAR);
        assert!(!view.last_event.contains("invalid next_track"));
        assert_eq!(view.parked_next.as_deref(), Some("0002"));
        assert!(should_pick_next_ready(&ReadyPickState::from(&view)));
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap();
        assert!(log.contains("override next_track 0099 → 0002"), "{log}");
        assert!(log.contains("parked-next 0002 (policy=hitl)"), "{log}");
        let (track, picked) = crate::api::resolve_run_track(&r, None).unwrap();
        assert!(picked);
        assert_eq!(track.as_deref(), Some("0002"));
    }

    fn merge_done_on(state: &mut crate::state::RunState, pr: Option<u64>) {
        state.ci = Some(CiWatchState {
            merge: Some("done".into()),
            pr_number: pr,
            ..Default::default()
        });
    }

    #[test]
    fn advance_skips_same_track_merge_done_auto_starts_next() {
        let dir = tempdir().unwrap();
        write_both_ready(dir.path());
        let mut r = rec(dir.path());
        r.auto_start = AutoStartPolicy::Full;
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        merge_done_on(&mut state, Some(56));
        save_run_state(&r, &state).unwrap();
        let o = PhaseOutcome::success(
            graph::PHASE_ADVANCE,
            OutcomeSource::Test,
            None,
            Some("0001".into()),
            None,
        );
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.track_id.as_deref(), Some("0002"));
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.phase, graph::PHASE_PLAN);
        assert!(view.run_epoch >= 2);
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap();
        assert!(log.contains("skip merged 0001"), "{log}");
        assert!(
            log.contains("#56") || log.contains("skip merged 0001 #56"),
            "{log}"
        );
    }

    #[test]
    fn advance_skips_same_track_hitl_parks_next() {
        let dir = tempdir().unwrap();
        write_both_ready(dir.path());
        let r = rec(dir.path());
        assert_eq!(r.auto_start, AutoStartPolicy::Hitl);
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        merge_done_on(&mut state, None);
        save_run_state(&r, &state).unwrap();
        let o = PhaseOutcome::success(
            graph::PHASE_ADVANCE,
            OutcomeSource::Test,
            None,
            Some("0001".into()),
            None,
        );
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.status, RunStatus::Idle);
        assert_eq!(view.last_event, LAST_EVENT_BACKLOG_CLEAR);
        assert_eq!(view.parked_next.as_deref(), Some("0002"));
        assert!(view.next_track.is_none());
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap();
        assert!(log.contains("skip merged 0001"), "{log}");
    }

    #[test]
    fn advance_skips_completed_local_not_0001() {
        let dir = tempdir().unwrap();
        write_two_ready(dir.path());
        let mut r = rec(dir.path());
        r.auto_start = AutoStartPolicy::Full;
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        save_run_state(&r, &state).unwrap();
        let o = PhaseOutcome::success(
            graph::PHASE_ADVANCE,
            OutcomeSource::Test,
            None,
            Some("0001".into()),
            None,
        );
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.track_id.as_deref(), Some("0002"));
        assert_ne!(view.track_id.as_deref(), Some("0001"));
        assert_eq!(view.status, RunStatus::Running);
    }

    #[test]
    fn advance_skip_last_ready_backlog_clears_next_track() {
        let dir = tempdir().unwrap();
        write_only_ready(dir.path(), "0001", "Example");
        let mut r = rec(dir.path());
        r.auto_start = AutoStartPolicy::Full;
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        merge_done_on(&mut state, None);
        save_run_state(&r, &state).unwrap();
        let o = PhaseOutcome::success(
            graph::PHASE_ADVANCE,
            OutcomeSource::Test,
            None,
            Some("0001".into()),
            None,
        );
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.status, RunStatus::Idle);
        assert_eq!(view.last_event, LAST_EVENT_BACKLOG_CLEAR);
        assert!(view.next_track.is_none());
        assert!(should_pick_next_ready(&ReadyPickState::from(&view)));
    }

    #[test]
    fn advance_unshipped_next_still_auto_starts() {
        let dir = tempdir().unwrap();
        write_both_ready(dir.path());
        let mut r = rec(dir.path());
        r.auto_start = AutoStartPolicy::Full;
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        merge_done_on(&mut state, None);
        save_run_state(&r, &state).unwrap();
        let o = PhaseOutcome::success(
            graph::PHASE_ADVANCE,
            OutcomeSource::Test,
            None,
            Some("0002".into()),
            None,
        );
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.track_id.as_deref(), Some("0002"));
        assert_eq!(view.status, RunStatus::Running);
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap();
        assert!(!log.contains("skip merged 0002"), "{log}");
    }

    #[test]
    fn advance_missing_dir_backlog_clears() {
        let dir = tempdir().unwrap();
        write_only_ready(dir.path(), "0001", "Example");
        let mut r = rec(dir.path());
        r.auto_start = AutoStartPolicy::Full;
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        merge_done_on(&mut state, None);
        save_run_state(&r, &state).unwrap();
        let o = PhaseOutcome::success(
            graph::PHASE_ADVANCE,
            OutcomeSource::Test,
            None,
            Some("0099".into()),
            None,
        );
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.status, RunStatus::Idle);
        assert_eq!(view.last_event, LAST_EVENT_BACKLOG_CLEAR);
        assert!(!view.last_event.contains("invalid next_track"));
        assert_eq!(view.track_id.as_deref(), Some("0001"));
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap();
        assert!(log.contains("override next_track 0099 → (none)"), "{log}");
    }

    #[test]
    fn advance_ready_missing_dir_journals_miss() {
        let dir = tempdir().unwrap();
        write_ready_row_without_dir(dir.path());
        let mut r = rec(dir.path());
        r.auto_start = AutoStartPolicy::Full;
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        save_run_state(&r, &state).unwrap();
        let o = PhaseOutcome::success(
            graph::PHASE_ADVANCE,
            OutcomeSource::Test,
            None,
            Some("0099".into()),
            None,
        );
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.status, RunStatus::Idle);
        assert_eq!(view.last_event, LAST_EVENT_BACKLOG_CLEAR);
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap();
        assert!(log.contains("no matching conductor directory"), "{log}");
        assert!(log.contains("override next_track 0099 → (none)"), "{log}");
    }

    #[test]
    fn default_hitl_parks_valid_next_and_omit_still_picks() {
        let dir = tempdir().unwrap();
        write_two_ready(dir.path());
        let r = rec(dir.path());
        assert_eq!(r.auto_start, AutoStartPolicy::Hitl);
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        save_run_state(&r, &state).unwrap();
        let o = PhaseOutcome::success(
            graph::PHASE_ADVANCE,
            OutcomeSource::Test,
            None,
            Some("0003".into()),
            None,
        );
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.status, RunStatus::Idle);
        assert_eq!(view.last_event, LAST_EVENT_BACKLOG_CLEAR);
        assert!(view.next_track.is_none());
        assert_eq!(view.parked_next.as_deref(), Some("0002"));
        assert_eq!(view.auto_start, AutoStartPolicy::Hitl);
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap();
        assert!(log.contains("parked-next 0002 (policy=hitl)"), "{log}");
        assert!(log.contains("override next_track 0003 → 0002"), "{log}");
        let (track, picked) = crate::api::resolve_run_track(&r, None).unwrap();
        assert!(picked);
        assert_eq!(track.as_deref(), Some("0002"));
        let started = run_with_driver(&r, Some("0002".into()), WorkflowDriver::FileWait).unwrap();
        assert!(started.parked_next.is_none());
    }

    #[test]
    fn from_run_state_defaults_hitl_resolve_overlays_never() {
        let dir = tempdir().unwrap();
        write_two_ready(dir.path());
        let mut r = rec(dir.path());
        r.auto_start = AutoStartPolicy::Never;
        crate::state::ensure_state_dir(&r).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.last_event = LAST_EVENT_BACKLOG_CLEAR.into();
        save_run_state(&r, &state).unwrap();
        let pick = ReadyPickState::from(&state);
        assert_eq!(pick.auto_start, AutoStartPolicy::Hitl);
        assert!(should_pick_next_ready(&pick));
        let err = crate::api::resolve_run_track(&r, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("auto_start=never; pass --track"), "{err}");
    }

    #[test]
    fn never_parks_and_omit_errors() {
        let dir = tempdir().unwrap();
        write_two_ready(dir.path());
        let mut r = rec(dir.path());
        r.auto_start = AutoStartPolicy::Never;
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        save_run_state(&r, &state).unwrap();
        let o = PhaseOutcome::success(
            graph::PHASE_ADVANCE,
            OutcomeSource::Test,
            None,
            Some("0002".into()),
            None,
        );
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.status, RunStatus::Idle);
        assert_eq!(view.parked_next.as_deref(), Some("0002"));
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap();
        assert!(log.contains("parked-next 0002 (policy=never)"), "{log}");
        let err = crate::api::resolve_run_track(&r, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("auto_start=never; pass --track"), "{err}");
        let (track, picked) = crate::api::resolve_run_track(&r, Some("0002".into())).unwrap();
        assert!(!picked);
        assert_eq!(track.as_deref(), Some("0002"));
    }

    #[test]
    fn nostart_row_skipped_full_starts_next_ready() {
        let dir = tempdir().unwrap();
        let cond = dir.path().join("conductor");
        std::fs::create_dir_all(cond.join("0002-Next")).unwrap();
        std::fs::create_dir_all(cond.join("0003-Later")).unwrap();
        std::fs::write(
            cond.join("conductor.md"),
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0001-Example](0001-Example/spec.md) | `.` | **Completed** | done |\n\
             | [0002-Next](0002-Next/spec.md) <!-- nostart --> | `.` | **Ready — not started** | lock |\n\
             | [0003-Later](0003-Later/spec.md) | `.` | **Ready — not started** | next |\n",
        )
        .unwrap();
        let mut r = rec(dir.path());
        r.auto_start = AutoStartPolicy::Full;
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        save_run_state(&r, &state).unwrap();
        let o = PhaseOutcome::success(
            graph::PHASE_ADVANCE,
            OutcomeSource::Test,
            None,
            Some("0002".into()),
            None,
        );
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.track_id.as_deref(), Some("0003"));
        assert!(view.last_event.contains("auto-start 0003"));
        assert!(view.parked_next.is_none());
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap();
        assert!(log.contains("override next_track 0002 → 0003"), "{log}");
        assert!(!log.contains("parked-next 0002"), "{log}");
        let (track, picked) = crate::api::resolve_run_track(&r, Some("0002".into())).unwrap();
        assert!(!picked);
        assert_eq!(track.as_deref(), Some("0002"));
    }

    #[test]
    fn paused_hitl_parks_on_resume() {
        let dir = tempdir().unwrap();
        write_two_ready(dir.path());
        let r = rec(dir.path());
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::FileWait).unwrap();
        run::pause(&r).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        save_run_state(&r, &state).unwrap();
        let o = PhaseOutcome::success(
            graph::PHASE_ADVANCE,
            OutcomeSource::Test,
            None,
            Some("0002".into()),
            None,
        );
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.status, RunStatus::Paused);
        assert_eq!(view.next_track.as_deref(), Some("0002"));
        let resumed = run::resume(&r).unwrap();
        assert_eq!(resumed.status, RunStatus::Idle);
        assert_eq!(resumed.last_event, LAST_EVENT_BACKLOG_CLEAR);
        assert!(resumed.next_track.is_none());
        assert_eq!(resumed.parked_next.as_deref(), Some("0002"));
        assert_eq!(resumed.track_id.as_deref(), Some("0001"));
    }

    #[test]
    fn plan_review_bundle_and_degrade() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("conductor").join("0001-Example")).unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0001".into()), WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_PLAN_REVIEW.into();
        state.pending_roles = vec!["agy".into(), "opencode".into()];
        save_run_state(&r, &state).unwrap();

        drive::write_review_markdown(&r, "agy", Some("agy says ok\n")).unwrap();
        let roles = crate::outcome::outcome_roles_dir(&r).unwrap();
        std::fs::create_dir_all(&roles).unwrap();
        let mut agy =
            PhaseOutcome::success("plan-review:agy", OutcomeSource::File, None, None, None);
        agy.metadata = Some(crate::outcome::OutcomeMetadata {
            next_track: None,
            role: Some(graph::ROLE_REVIEWER_AGY.into()),
            ..Default::default()
        });
        crate::persist::atomic_write_json(&roles.join("agy.json"), &agy).unwrap();
        let oc = PhaseOutcome::failure(
            "plan-review:opencode",
            FailureClass::Timeout,
            OutcomeSource::File,
            None,
            None,
        );
        crate::persist::atomic_write_json(&roles.join("opencode.json"), &oc).unwrap();

        let view = tick(&r).unwrap().expect("join");
        assert_eq!(view.phase, graph::PHASE_FOLD);
        let bundle = dir.path().join("AI-review.md");
        assert!(bundle.is_file());
        let text = std::fs::read_to_string(&bundle).unwrap();
        assert!(text.contains("## agy"));
        assert!(text.contains("agy says ok"));
        assert!(text.contains("degraded — not produced"));
    }

    #[test]
    fn plan_review_both_fail_stops() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, None, WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_PLAN_REVIEW.into();
        state.pending_roles = vec!["agy".into(), "opencode".into()];
        save_run_state(&r, &state).unwrap();
        let roles = crate::outcome::outcome_roles_dir(&r).unwrap();
        std::fs::create_dir_all(&roles).unwrap();
        for slug in ["agy", "opencode"] {
            let o = PhaseOutcome::failure(
                format!("plan-review:{slug}"),
                FailureClass::HarnessCrash,
                OutcomeSource::File,
                None,
                None,
            );
            crate::persist::atomic_write_json(&roles.join(format!("{slug}.json")), &o).unwrap();
        }
        let view = tick(&r).unwrap().expect("fail");
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.phase, graph::PHASE_PLAN_REVIEW);
        assert_eq!(view.failure_class, Some(FailureClass::HarnessCrash));
    }

    #[test]
    fn join_timeout_degrades_when_one_done() {
        let _guard = test_env_lock();
        unsafe {
            std::env::set_var(ENV_PHASE_TIMEOUT_SECS, "1");
        }
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, None, WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_PLAN_REVIEW.into();
        state.pending_roles = vec!["opencode".into()];
        state.phase_started_at = Some(chrono::Utc::now() - chrono::Duration::seconds(5));
        save_run_state(&r, &state).unwrap();
        drive::write_review_markdown(&r, "agy", Some("agy done\n")).unwrap();
        let view = crate::outcome::try_timeout_under_lock(&r)
            .unwrap()
            .expect("degrade join");
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.phase, graph::PHASE_FOLD);
        assert!(view.last_event.contains("degraded"));
        unsafe {
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
        }
    }

    #[test]
    fn join_timeout_table_default_does_not_degrade_at_1300s() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let prev_home = std::env::var_os(ENV_COORDINATOR_HOME);
        let prev_timeout = std::env::var_os(ENV_PHASE_TIMEOUT_SECS);
        unsafe {
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, None, WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_PLAN_REVIEW.into();
        state.pending_roles = vec!["opencode".into()];
        state.phase_started_at = Some(chrono::Utc::now() - chrono::Duration::seconds(1300));
        save_run_state(&r, &state).unwrap();
        drive::write_review_markdown(&r, "agy", Some("agy done\n")).unwrap();
        let none = crate::outcome::try_timeout_under_lock(&r).unwrap();
        assert!(
            none.is_none(),
            "table 2400s join must not degrade leftover at 1300s, got {none:?}"
        );
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var(ENV_COORDINATOR_HOME, v),
                None => std::env::remove_var(ENV_COORDINATOR_HOME),
            }
            match prev_timeout {
                Some(v) => std::env::set_var(ENV_PHASE_TIMEOUT_SECS, v),
                None => std::env::remove_var(ENV_PHASE_TIMEOUT_SECS),
            }
        }
    }

    #[test]
    fn join_timeout_plan_review_1200_overlay_still_degrades_at_1300s() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let prev_home = std::env::var_os(ENV_COORDINATOR_HOME);
        let prev_timeout = std::env::var_os(ENV_PHASE_TIMEOUT_SECS);
        unsafe {
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let dir = tempdir().unwrap();
        let mut r = rec(dir.path());
        r.phase_timeouts_secs
            .insert(graph::PHASE_PLAN_REVIEW.into(), 1200);
        run_with_driver(&r, None, WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_PLAN_REVIEW.into();
        state.pending_roles = vec!["opencode".into()];
        state.phase_started_at = Some(chrono::Utc::now() - chrono::Duration::seconds(1300));
        save_run_state(&r, &state).unwrap();
        drive::write_review_markdown(&r, "agy", Some("agy done\n")).unwrap();
        let view = crate::outcome::try_timeout_under_lock(&r)
            .unwrap()
            .expect("degrade join under 1200 overlay");
        assert_eq!(view.status, RunStatus::Running);
        assert!(
            view.last_event.contains("degraded"),
            "last_event={}",
            view.last_event
        );
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var(ENV_COORDINATOR_HOME, v),
                None => std::env::remove_var(ENV_COORDINATOR_HOME),
            }
            match prev_timeout {
                Some(v) => std::env::set_var(ENV_PHASE_TIMEOUT_SECS, v),
                None => std::env::remove_var(ENV_PHASE_TIMEOUT_SECS),
            }
        }
    }

    #[test]
    fn compact_skip_includes_adapter_error() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, None, WorkflowDriver::Adapter).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_COMPACT.into();
        save_run_state(&r, &state).unwrap();
        let view = tick(&r).unwrap().expect("compact");
        assert!(
            view.last_event.contains("compact: skipped —"),
            "last_event={}",
            view.last_event
        );
        assert!(view.failure_class.is_none());
        assert!(crate::notify::artifact::existing_path(&r).is_none());
    }

    #[test]
    fn stub_apply_still_idles() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_stub(&r, None).unwrap();
        let o = PhaseOutcome::success(
            crate::state::STUB_PHASE_ACTIVE,
            OutcomeSource::Test,
            None,
            None,
            None,
        );
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.status, RunStatus::Idle);
        assert_eq!(view.phase, crate::state::STUB_PHASE_COMPLETED);
    }

    // --- 0097: fold -> plan-review re-entry ---------------------------------------------

    fn sha_hex_test(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(bytes);
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Write a track dir whose `reuse.json` covers exactly `spec`/`plan`.
    fn receipt_for(dir: &std::path::Path, spec: &[u8], plan: &[u8]) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("spec.md"), spec).unwrap();
        std::fs::write(dir.join("plan.md"), plan).unwrap();
        let spec_sha = sha_hex_test(spec);
        let plan_sha = sha_hex_test(plan);
        let body = format!(
            "{{\"version\":1,\"track_id\":\"0097\",\"spec_sha256\":\"{spec_sha}\",\
             \"plan_sha256\":\"{plan_sha}\",\"skill_sha256\":\"{spec_sha}\",\"slots\":{{}}}}"
        );
        std::fs::write(dir.join("reuse.json"), body).unwrap();
    }

    fn track_dir_0097(ws: &std::path::Path) -> std::path::PathBuf {
        let dir = ws.join("conductor").join("0097-Test");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn gate_state(record: &ProjectRecord) -> RunState {
        let mut state = RunState::idle(&record.id);
        state.track_id = Some("0097".into());
        state
    }

    #[test]
    fn fold_gate_blocks_when_plan_is_missing() {
        let ws = tempdir().unwrap();
        let record = rec(ws.path());
        let dir = track_dir_0097(ws.path());
        std::fs::write(dir.join("spec.md"), b"spec").unwrap();
        let state = gate_state(&record);
        let message = fold_artifact_gate(&record, &state).expect("gate must fire");
        assert!(message.contains("no usable plan.md"), "{message}");
    }

    #[test]
    fn fold_gate_blocks_on_a_needs_re_plan_verdict() {
        let ws = tempdir().unwrap();
        let record = rec(ws.path());
        let dir = track_dir_0097(ws.path());
        std::fs::write(dir.join("plan.md"), b"a real plan").unwrap();
        std::fs::write(
            dir.join("agy-review.md"),
            b"## Verdict: Needs re-plan\n\nfindings\n",
        )
        .unwrap();
        let state = gate_state(&record);
        let message = fold_artifact_gate(&record, &state).expect("gate must fire");
        assert!(message.contains("agy-review.md"), "{message}");
        assert!(message.contains("re-plan"), "{message}");
    }

    #[test]
    fn fold_gate_allows_a_plan_with_passing_verdicts() {
        let ws = tempdir().unwrap();
        let record = rec(ws.path());
        let dir = track_dir_0097(ws.path());
        std::fs::write(dir.join("plan.md"), b"a real plan").unwrap();
        std::fs::write(dir.join("agy-review.md"), b"## Verdict: PASS\n").unwrap();
        std::fs::write(
            dir.join("opencode-review.md"),
            b"## Verdict: Ready after fixes\n",
        )
        .unwrap();
        let state = gate_state(&record);
        assert!(fold_artifact_gate(&record, &state).is_none());
    }

    fn impl_git(cwd: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
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
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repo whose HEAD is also this epoch's checkpoint ref, i.e. the moment `fold` hands
    /// over to `implement`.
    fn implement_fixture(
        epoch: u64,
    ) -> (tempfile::TempDir, crate::registry::ProjectRecord, String) {
        let repo = tempfile::tempdir().unwrap();
        impl_git(repo.path(), &["init", "-b", "main"]);
        impl_git(
            repo.path(),
            &["config", "user.email", "impl-gate@example.com"],
        );
        impl_git(repo.path(), &["config", "user.name", "impl-gate"]);
        std::fs::write(repo.path().join("README.md"), b"seed\n").unwrap();
        impl_git(repo.path(), &["add", "."]);
        impl_git(repo.path(), &["commit", "-m", "seed"]);
        let mut record = rec(repo.path());
        record.execution_repo = Some(repo.path().to_path_buf());
        let ref_name = crate::checkpoint::ref_name(&record.id, epoch);
        impl_git(repo.path(), &["update-ref", &ref_name, "HEAD"]);
        (repo, record, ref_name)
    }

    fn implement_state(record: &crate::registry::ProjectRecord, epoch: u64) -> RunState {
        let mut state = RunState::idle(&record.id);
        state.track_id = Some("0554".into());
        state.run_epoch = epoch;
        state.phase = graph::PHASE_IMPLEMENT.into();
        let repo = record
            .execution_repo
            .as_deref()
            .unwrap_or(record.path.as_path());
        state.delivery_target = Some(crate::workflow::execution_repo::seeded_delivery_target(
            record,
            "0554",
            epoch,
            graph::PHASE_IMPLEMENT,
            repo,
        ));
        crate::workflow::execution_repo::assert_fixture_has_delivery_target(&state);
        state
    }

    #[test]
    fn implement_gate_fixtures_seed_the_delivery_target() {
        let (_repo, record, _) = implement_fixture(1);
        let state = implement_state(&record, 1);
        assert!(state.delivery_target.is_some());
        crate::workflow::execution_repo::assert_fixture_has_delivery_target(&state);
    }

    #[test]
    fn implement_gate_blocks_when_the_tree_is_dirty() {
        let (repo, record, _) = implement_fixture(1);
        std::fs::write(repo.path().join("src_new.rs"), b"fn work() {}\n").unwrap();
        let mut state = implement_state(&record, 1);
        let message = implement_artifact_gate(&record, &mut state).expect("gate must fire");
        assert!(message.contains("uncommitted"), "{message}");
        assert!(message.contains("src_new.rs"), "{message}");
    }

    #[test]
    fn implement_gate_blocks_when_nothing_was_committed_since_the_checkpoint() {
        let (_repo, record, _) = implement_fixture(1);
        let mut state = implement_state(&record, 1);
        let message = implement_artifact_gate(&record, &mut state).expect("gate must fire");
        assert!(message.contains("no commit to review"), "{message}");
    }

    fn implement_sibling_state(
        epoch: u64,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        crate::registry::ProjectRecord,
        RunState,
    ) {
        let primary = tempfile::tempdir().unwrap();
        impl_git(primary.path(), &["init", "-b", "main"]);
        impl_git(
            primary.path(),
            &["config", "user.email", "impl-gate@example.com"],
        );
        impl_git(primary.path(), &["config", "user.name", "impl-gate"]);
        std::fs::write(primary.path().join("README.md"), b"seed\n").unwrap();
        impl_git(primary.path(), &["add", "."]);
        impl_git(primary.path(), &["commit", "-m", "seed"]);
        let sibling = tempfile::tempdir().unwrap();
        impl_git(sibling.path(), &["init", "-b", "main"]);
        impl_git(
            sibling.path(),
            &["config", "user.email", "impl-gate@example.com"],
        );
        impl_git(sibling.path(), &["config", "user.name", "impl-gate"]);
        std::fs::write(sibling.path().join("README.md"), b"seed\n").unwrap();
        impl_git(sibling.path(), &["add", "."]);
        impl_git(sibling.path(), &["commit", "-m", "seed"]);
        let mut record = rec(primary.path());
        record.execution_repo = Some(primary.path().to_path_buf());
        let ref_name = crate::checkpoint::ref_name(&record.id, epoch);
        impl_git(sibling.path(), &["update-ref", &ref_name, "HEAD"]);
        let mut state = RunState::idle(&record.id);
        state.track_id = Some("0554".into());
        state.run_epoch = epoch;
        state.phase = graph::PHASE_IMPLEMENT.into();
        state.delivery_target = Some(crate::workflow::execution_repo::seeded_delivery_target(
            &record,
            "0554",
            epoch,
            graph::PHASE_IMPLEMENT,
            sibling.path(),
        ));
        crate::workflow::execution_repo::assert_fixture_has_delivery_target(&state);
        (primary, sibling, record, state)
    }

    #[test]
    fn implement_gate_measures_the_target_head() {
        let (primary, sibling, record, mut state) = implement_sibling_state(1);
        std::fs::write(primary.path().join("primary-only.txt"), b"not the target\n").unwrap();
        std::fs::write(sibling.path().join("src_new.rs"), b"fn work() {}\n").unwrap();
        impl_git(sibling.path(), &["add", "."]);
        impl_git(
            sibling.path(),
            &["commit", "-m", "track(0554): the leg's work"],
        );
        assert!(
            implement_artifact_gate(&record, &mut state).is_none(),
            "the primary's dirt must not fail a sibling-target leg"
        );
    }

    #[test]
    fn implement_gate_still_fails_an_uncommitted_sibling_leg() {
        let (_primary, sibling, record, mut state) = implement_sibling_state(1);
        std::fs::write(sibling.path().join("src_new.rs"), b"fn work() {}\n").unwrap();
        let message = implement_artifact_gate(&record, &mut state).expect("gate must fire");
        assert!(message.contains("uncommitted"), "{message}");
        assert!(message.contains("src_new.rs"), "{message}");
    }

    #[test]
    fn planning_only_target_skips_the_implement_gate() {
        let dir = tempfile::tempdir().unwrap();
        let record = rec(dir.path());
        let mut state = RunState::idle(&record.id);
        state.track_id = Some("0101".into());
        state.run_epoch = 1;
        state.phase = graph::PHASE_IMPLEMENT.into();
        let mut target = crate::workflow::execution_repo::seeded_delivery_target(
            &record,
            "0101",
            1,
            graph::PHASE_IMPLEMENT,
            dir.path(),
        );
        target.planning_only = true;
        state.delivery_target = Some(target);
        assert!(implement_artifact_gate(&record, &mut state).is_none());
    }

    #[test]
    fn two_repo_b_only_commit_is_delivered_and_graded() {
        let (primary, sibling, record, mut state) = implement_sibling_state(1);
        let base = impl_git(
            sibling.path(),
            &["rev-parse", &crate::checkpoint::ref_name(&record.id, 1)],
        );
        // A-only work does not satisfy the B leg.
        std::fs::write(primary.path().join("a-only.rs"), b"fn a() {}\n").unwrap();
        impl_git(primary.path(), &["add", "."]);
        impl_git(primary.path(), &["commit", "-m", "a only"]);
        let message = implement_artifact_gate(&record, &mut state).expect("B leg empty");
        assert!(message.contains("no commit to review"), "{message}");
        // A B-only commit is delivered and graded in B.
        std::fs::write(sibling.path().join("b-only.rs"), b"fn b() {}\n").unwrap();
        impl_git(sibling.path(), &["add", "."]);
        impl_git(sibling.path(), &["commit", "-m", "track(0554): b only"]);
        assert!(implement_artifact_gate(&record, &mut state).is_none());
        state.phase = graph::PHASE_ADDRESS_CI.into();
        if let Some(target) = state.delivery_target.as_mut() {
            target.phase = graph::PHASE_ADDRESS_CI.into();
        }
        state.ci_fix_request = Some(crate::state::CiFixRequest {
            pr_number: 71,
            from_sha: base,
            checks: Vec::new(),
        });
        assert_eq!(
            crate::ci::fix::classify_diff(&record, &state),
            crate::ci::fix::DiffClass::Changed
        );
    }

    #[test]
    fn implement_gate_allows_a_clean_committed_tree() {
        let (repo, record, _) = implement_fixture(1);
        std::fs::write(repo.path().join("src_new.rs"), b"fn work() {}\n").unwrap();
        impl_git(repo.path(), &["add", "."]);
        impl_git(
            repo.path(),
            &["commit", "-m", "track(0554): the leg's work"],
        );
        let mut state = implement_state(&record, 1);
        assert!(implement_artifact_gate(&record, &mut state).is_none());
    }

    #[test]
    fn fold_reentry_is_none_when_artifacts_match_the_receipt() {
        let ws = tempdir().unwrap();
        let record = rec(ws.path());
        let dir = track_dir_0097(ws.path());
        receipt_for(&dir, b"spec", b"plan");
        let mut state = RunState::idle(&record.id);
        state.track_id = Some("0097".into());
        assert!(matches!(
            fold_review_reentry(&record, &mut state),
            FoldReentry::None
        ));
        assert_eq!(state.fold_review_reentries, 0);
    }

    #[test]
    fn fold_reentry_reenters_when_the_fold_rewrote_the_spec() {
        let ws = tempdir().unwrap();
        let record = rec(ws.path());
        let dir = track_dir_0097(ws.path());
        receipt_for(&dir, b"spec", b"plan");
        std::fs::write(dir.join("spec.md"), b"rewritten spec").unwrap();
        let mut state = RunState::idle(&record.id);
        state.track_id = Some("0097".into());
        assert!(matches!(
            fold_review_reentry(&record, &mut state),
            FoldReentry::Reenter
        ));
        assert_eq!(state.fold_review_reentries, 1);
        assert!(
            state.last_event.contains("advance fold -> plan-review"),
            "{}",
            state.last_event
        );
        assert!(
            state.last_event.contains("artifacts changed"),
            "{}",
            state.last_event
        );
    }

    #[test]
    fn fold_reentry_is_none_without_a_receipt() {
        let ws = tempdir().unwrap();
        let record = rec(ws.path());
        let dir = track_dir_0097(ws.path());
        std::fs::write(dir.join("spec.md"), b"spec").unwrap();
        std::fs::write(dir.join("plan.md"), b"plan").unwrap();
        let mut state = RunState::idle(&record.id);
        state.track_id = Some("0097".into());
        assert!(
            matches!(fold_review_reentry(&record, &mut state), FoldReentry::None),
            "a track with no receipt keeps the historical fold -> implement behaviour"
        );
    }

    #[test]
    fn fold_reentry_stops_with_review_never_settles_at_the_cap() {
        let ws = tempdir().unwrap();
        let record = rec(ws.path());
        let dir = track_dir_0097(ws.path());
        receipt_for(&dir, b"spec", b"plan");
        std::fs::write(dir.join("plan.md"), b"rewritten plan").unwrap();
        let mut state = RunState::idle(&record.id);
        state.track_id = Some("0097".into());
        state.status = RunStatus::Running;
        state.fold_review_reentries = MAX_FOLD_REVIEW_REENTRIES;
        state.fold_last_pair = crate::workflow::reuse::current_artifact_pair(&dir);
        assert!(matches!(
            fold_review_reentry(&record, &mut state),
            FoldReentry::Exhausted
        ));
        assert_eq!(state.status, RunStatus::Stopped);
        assert_eq!(state.failure_class, Some(FailureClass::ReviewNeverSettles));
        assert!(
            state.last_event.contains("review never settles"),
            "{}",
            state.last_event
        );
    }
}
