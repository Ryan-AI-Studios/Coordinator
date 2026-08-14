//! Built-in canonical conductor-track workflow (`canonical_v1`).

pub mod bundle;
pub mod drive;
pub mod graph;
pub mod prompts;
pub mod timeouts;
pub mod watchdog;

use serde::{Deserialize, Serialize};

use crate::error::{CoordinatorError, Result};
use crate::outcome::PhaseOutcome;
use crate::registry::ProjectRecord;
use crate::state::{RunState, RunStatus, load_run_state, save_run_state, with_run_state_lock};

pub use drive::tick;
pub use graph::{WORKFLOW_ID, is_canonical, is_stub_phase, resolve_track_dir, successor};
pub use timeouts::{ENV_PHASE_TIMEOUT_SECS, timeout_for_phase};

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
    if state.status == RunStatus::Paused {
        state.pause_started_at = Some(now);
    } else {
        state.pause_started_at = None;
    }
}

/// `advance` success: auto-start valid track, Idle on null/invalid; Pause holds.
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
        None => {
            state.status = RunStatus::Idle;
            state.last_event = "workflow: backlog clear".into();
            state.phase_started_at = None;
            state.pause_started_at = None;
        }
        Some(id) => {
            let id = id.to_string();
            if resolve_track_dir(record, &id).is_some() {
                auto_start(state, &id);
                crate::outcome::clear_active_outcome_file(record);
                crate::workflow::drive::clear_plan_review_artifacts(record);
                crate::notify::clear_artifact(record);
                crate::workflow::watchdog::clear_progress(record);
            } else {
                state.status = RunStatus::Idle;
                state.last_event = format!("workflow: invalid next_track {id}");
                state.phase_started_at = None;
                state.pause_started_at = None;
            }
        }
    }
}

pub fn auto_start(state: &mut RunState, track_id: &str) {
    state.run_epoch = state.run_epoch.saturating_add(1);
    state.track_id = Some(track_id.to_string());
    state.next_track = None;
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
        save_run_state(record, &state)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ENV_OUTCOME_POLL_MS, test_env_lock};
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
        let view = wait_for_outcome(&r, 15).unwrap();
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

    #[test]
    fn advance_valid_auto_starts() {
        let dir = tempdir().unwrap();
        let cond = dir.path().join("conductor");
        std::fs::create_dir_all(cond.join("0001-Example")).unwrap();
        std::fs::create_dir_all(cond.join("0002-Next")).unwrap();
        let r = rec(dir.path());
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
        assert_eq!(view.phase, graph::PHASE_ADVANCE);
        assert_eq!(view.next_track.as_deref(), Some("0002"));
        let resumed = run::resume(&r).unwrap();
        assert_eq!(resumed.status, RunStatus::Running);
        assert_eq!(resumed.phase, graph::PHASE_PLAN);
        assert_eq!(resumed.track_id.as_deref(), Some("0002"));
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
