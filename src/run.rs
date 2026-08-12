//! Stub-phase run state machine (ADR-0024 stop/pause).
//!
//! Phase completion and timeouts are driven by the Phase Outcome apply path
//! ([`crate::outcome`]) and poll/wait ([`crate::watch`]) — track 0005.

use crate::error::{CoordinatorError, Result};
use crate::registry::ProjectRecord;
use crate::state::{
    RunState, RunStatus, STOP_LAST_EVENT, STUB_PHASE_ACTIVE, STUB_PHASE_STOPPED, StatusView,
    ensure_state_dir, load_run_state, save_run_state,
};

/// Apply `run`: Idle/Stopped → Running.
///
/// If `--track` is omitted, prior `track_id` is **retained** (0004 absorb / 0005 docs).
pub fn run(record: &ProjectRecord, track_id: Option<String>) -> Result<StatusView> {
    ensure_state_dir(record)?;
    let mut state = load_run_state(record)?;
    match state.status {
        RunStatus::Idle | RunStatus::Stopped => {
            state.status = RunStatus::Running;
            state.phase = STUB_PHASE_ACTIVE.into();
            if track_id.is_some() {
                state.track_id = track_id;
            }
            // Fresh run epoch + phase clock; invalidate prior consume markers.
            state.run_epoch = state.run_epoch.saturating_add(1);
            state.phase_started_at = Some(chrono::Utc::now());
            state.total_paused_ms = 0;
            state.pause_started_at = None;
            state.failure_class = None;
            state.last_applied_outcome_hash = None;
            state.last_event = "run: started stub".into();
            state.updated_at = chrono::Utc::now();
            save_run_state(record, &state)?;
            Ok(StatusView::from_record(record, &state))
        }
        other => Err(CoordinatorError::InvalidTransition {
            action: "run",
            from: other.to_string(),
        }),
    }
}

/// Apply `pause`: Running → Paused (timeout budget freezes).
pub fn pause(record: &ProjectRecord) -> Result<StatusView> {
    transition(record, "pause", |state| match state.status {
        RunStatus::Running => {
            state.status = RunStatus::Paused;
            state.pause_started_at = Some(chrono::Utc::now());
            state.last_event = "pause: hold stub".into();
            Ok(())
        }
        other => Err(CoordinatorError::InvalidTransition {
            action: "pause",
            from: other.to_string(),
        }),
    })
}

/// Apply `resume`: Paused → Running (accumulate paused duration for timeout freeze).
pub fn resume(record: &ProjectRecord) -> Result<StatusView> {
    transition(record, "resume", |state| match state.status {
        RunStatus::Paused => {
            let now = chrono::Utc::now();
            if let Some(pstart) = state.pause_started_at.take() {
                let delta = (now - pstart).num_milliseconds().max(0) as u64;
                state.total_paused_ms = state.total_paused_ms.saturating_add(delta);
            }
            state.status = RunStatus::Running;
            // Keep phase as-is when already advanced (e.g. success-while-paused);
            // for active stub, ensure active phase string for outcome match.
            if state.phase.is_empty() {
                state.phase = STUB_PHASE_ACTIVE.into();
            }
            state.last_event = "resume: continue stub".into();
            Ok(())
        }
        other => Err(CoordinatorError::InvalidTransition {
            action: "resume",
            from: other.to_string(),
        }),
    })
}

/// Apply `stop`: Running/Paused → Stopped; already Stopped = successful no-op.
pub fn stop(record: &ProjectRecord) -> Result<StatusView> {
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
    ensure_state_dir(record)?;
    let mut state = load_run_state(record)?;
    f(&mut state)?;
    state.updated_at = chrono::Utc::now();
    save_run_state(record, &state)?;
    Ok(StatusView::from_record(record, &state))
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
            layout_profile: "nested".into(),
            state_dir: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn happy_path_run_pause_resume_stop() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());

        let s = run(&r, Some("0004".into())).unwrap();
        assert_eq!(s.status, RunStatus::Running);
        assert_eq!(s.phase, STUB_PHASE_ACTIVE);
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
}
