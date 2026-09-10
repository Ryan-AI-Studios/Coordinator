//! Built-in canonical conductor-track workflow (`canonical_v1`).

pub mod bundle;
pub mod conductor_md;
pub mod drive;
pub mod graph;
pub mod plan_review;
pub mod prompts;
pub mod timeouts;
pub mod watchdog;

use serde::{Deserialize, Serialize};

use crate::error::{CoordinatorError, Result};
use crate::outcome::{FailureClass, LAST_EVENT_MESSAGE_CAP, PhaseOutcome};
use crate::registry::{AutoStartPolicy, ProjectRecord};
use crate::state::{RunState, RunStatus, load_run_state, save_run_state, with_run_state_lock};

pub use conductor_md::{
    ReadyPickState, pick_next_ready, should_pick_next_ready, track_row_nostart,
};
pub use drive::tick;
pub use graph::{WORKFLOW_ID, is_canonical, is_stub_phase, resolve_track_dir, successor};
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

/// Apply-table hook: canonical success → successor (stay Running/Paused) or advance.
pub fn on_success(record: &ProjectRecord, state: &mut RunState, outcome: &PhaseOutcome) {
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

    if state.phase == graph::PHASE_ADVANCE {
        apply_advance(record, state);
        return;
    }

    if let Some(next) = successor(&state.phase) {
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
    state.plan_review_spawned.clear();
    if state.status == RunStatus::Paused {
        state.pause_started_at = Some(now);
    } else {
        state.pause_started_at = None;
    }
}

/// `advance` success: `full` auto-starts a valid track; hitl/never/`nostart` park.
/// Idle on null/invalid. Pause holds until resume.
pub fn apply_advance(record: &ProjectRecord, state: &mut RunState) {
    if state.status == RunStatus::Paused {
        state.last_driven_phase = Some(graph::PHASE_ADVANCE.into());
        state.last_event = "workflow: advance held until resume".into();
        return;
    }
    finish_advance(record, state);
}

fn finish_advance(record: &ProjectRecord, state: &mut RunState) {
    match state
        .next_track
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        None => apply_backlog_clear(record, state),
        Some(id) => {
            let id = id.to_string();
            if resolve_track_dir(record, &id).is_some() {
                if let Some(policy) = park_policy(record, &id) {
                    park_next(record, state, &id, policy);
                } else {
                    auto_start(state, &id);
                    crate::outcome::clear_active_outcome_file(record);
                    crate::workflow::drive::clear_plan_review_artifacts(record);
                    crate::notify::clear_artifact(record);
                    crate::workflow::watchdog::clear_progress(record);
                }
            } else {
                state.status = RunStatus::Idle;
                state.last_event = format!("workflow: invalid next_track {id}");
                state.phase_started_at = None;
                state.pause_started_at = None;
            }
        }
    }
}

fn apply_backlog_clear(record: &ProjectRecord, state: &mut RunState) {
    state.status = RunStatus::Idle;
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
            crate::notify::clear_artifact(record);
            state.failure_class = None;
            crate::progress_log::append(record, "advance", crate::notify::AUTO_CLEAR_DETAIL);
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
    state.stalled_at = None;
    state.address_findings_attempts = 0;
    reset_phase_clock(state);
    state.last_event = format!("workflow: auto-start {track_id}");
}

/// Resume after `advance` succeeded while Paused.
pub fn apply_advance_on_resume(record: &ProjectRecord, state: &mut RunState) {
    state.status = RunStatus::Running;
    state.pause_started_at = None;
    finish_advance(record, state);
}

pub fn mark_driven(record: &ProjectRecord, phase: &str) -> Result<()> {
    with_run_state_lock(record, || {
        let mut state = load_run_state(record)?;
        state.last_driven_phase = Some(phase.into());
        state.updated_at = chrono::Utc::now();
        save_run_state(record, &state)?;
        let track = state.track_id.as_deref().unwrap_or("-");
        crate::progress_log::append(record, "inject", &format!("track={track} phase={phase}"));
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ENV_COORDINATOR_HOME, ENV_OUTCOME_POLL_MS, test_env_lock};
    use crate::outcome::{FailureClass, OutcomeSource, write_and_apply};
    use crate::run::{self, run_stub, run_with_driver};
    use crate::state::{STUB_PHASE_STOPPED, StatusView};
    use crate::watch::{poll_once, wait_for_outcome};
    use tempfile::tempdir;
    use uuid::Uuid;

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
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            created_at: chrono::Utc::now(),
        }
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

    #[test]
    fn next_track_cleared_on_run_track_retained() {
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
        let r = rec(dir.path());
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
        let r = rec(dir.path());
        run_with_driver(&r, None, WorkflowDriver::Stub).unwrap();
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
        let cond = dir.path().join("conductor");
        std::fs::create_dir_all(cond.join("0001-Example")).unwrap();
        std::fs::create_dir_all(cond.join("0002-Next")).unwrap();
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
    fn advance_invalid_idles_without_fail() {
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
        assert!(view.last_event.contains("invalid next_track"));
        assert_eq!(view.track_id.as_deref(), Some("0001"));
    }

    #[test]
    fn pause_holds_auto_start_until_resume() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("conductor").join("0002-Next")).unwrap();
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
            Some("0002".into()),
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
    fn nostart_row_parks_even_under_full() {
        let dir = tempdir().unwrap();
        let cond = dir.path().join("conductor");
        std::fs::create_dir_all(cond.join("0002-Next")).unwrap();
        std::fs::write(
            cond.join("conductor.md"),
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0002-Next](0002-Next/spec.md) <!-- nostart --> | `.` | **Ready — not started** | lock |\n",
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
        assert_eq!(view.status, RunStatus::Idle);
        assert_eq!(view.parked_next.as_deref(), Some("0002"));
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap();
        assert!(log.contains("parked-next 0002 (policy=nostart)"), "{log}");
        let (track, picked) = crate::api::resolve_run_track(&r, Some("0002".into())).unwrap();
        assert!(!picked);
        assert_eq!(track.as_deref(), Some("0002"));
    }

    #[test]
    fn paused_hitl_parks_on_resume() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("conductor").join("0002-Next")).unwrap();
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
}
