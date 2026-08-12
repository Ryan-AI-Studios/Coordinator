//! Poll loop for Phase Outcome files and stub phase timeouts (track 0005).
//!
//! Used by `coordinator wait` and the `serve` background task. File discovery is
//! poll-based (no `notify` crate); transient Windows share/parse errors skip a tick.

use std::time::{Duration, Instant};

use crate::config::{
    ENV_OUTCOME_POLL_MS, ENV_STUB_PHASE_TIMEOUT_SECS, outcome_poll_interval, stub_phase_timeout,
};
use crate::error::{CoordinatorError, Result};
use crate::outcome::{self, try_load_current_outcome};
use crate::registry::ProjectRecord;
use crate::run;
use crate::state::{RunStatus, StatusView, load_run_state};

/// One poll tick: try file apply, then evaluate timeout while Running.
///
/// Returns `Some(view)` when an outcome was applied this tick; `None` otherwise.
/// Never panics on unreadable/partial JSON (skips file apply for this tick).
pub fn poll_once(record: &ProjectRecord) -> Result<Option<StatusView>> {
    // 1) File drop (Running or Paused may accept current-phase outcome).
    if let Some(outcome) = try_load_current_outcome(record) {
        match outcome::poll_try_apply(record, outcome) {
            Ok(Some(view)) => return Ok(Some(view)),
            Ok(None) => {}
            // Soft-skip unexpected apply errors that are not hard CP failures.
            Err(e) => {
                // Schema validation failures: leave file for operator; do not crash poll.
                let msg = e.to_string();
                if msg.contains("unsupported outcome version")
                    || msg.contains("failure_class")
                    || msg.contains("phase must be")
                {
                    // skip tick
                } else {
                    // still skip tick rather than kill serve
                    let _ = msg;
                }
            }
        }
    }

    // 2) Timeout while Running only (budget frozen while Paused).
    let state = load_run_state(record)?;
    if state.status == RunStatus::Running
        && let Some(started) = state.phase_started_at
    {
        let budget = stub_phase_timeout();
        let elapsed = state.effective_running_elapsed(chrono::Utc::now());
        if elapsed >= budget {
            let view = outcome::apply_timeout(record, &state)?;
            return Ok(Some(view));
        }
        let _ = started; // used via effective_running_elapsed
    }

    Ok(None)
}

/// Block until an outcome is applied or `timeout_secs` elapses.
///
/// Exit mapping for CLI:
/// - Ok(view) → exit 0 (success **or** failure applied, including timeout)
/// - Err(WaitBudgetExpired) → exit 2
/// - other Err → exit 1 / mapped codes
pub fn wait_for_outcome(record: &ProjectRecord, timeout_secs: u64) -> Result<StatusView> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let interval = outcome_poll_interval();

    loop {
        if let Some(view) = poll_once(record)? {
            return Ok(view);
        }
        if Instant::now() >= deadline {
            return Err(CoordinatorError::WaitBudgetExpired);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let sleep_for = interval.min(remaining);
        if sleep_for.is_zero() {
            return Err(CoordinatorError::WaitBudgetExpired);
        }
        std::thread::sleep(sleep_for);
    }
}

/// Async variant for the serve background loop (one project).
pub async fn poll_once_async(record: &ProjectRecord) -> Result<Option<StatusView>> {
    // Blocking file/state IO is short; spawn_blocking keeps the runtime responsive.
    let rec = record.clone();
    tokio::task::spawn_blocking(move || poll_once(&rec))
        .await
        .map_err(|e| CoordinatorError::Message(format!("poll task join: {e}")))?
}

/// Background poll all registered projects that are Running or Paused.
pub async fn serve_poll_loop(mut shutdown: tokio::sync::watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow() {
            break;
        }
        let interval = outcome_poll_interval();
        match crate::api::load_registry() {
            Ok(reg) => {
                for rec in reg.list() {
                    let state = match load_run_state(rec) {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    if !matches!(state.status, RunStatus::Running | RunStatus::Paused) {
                        continue;
                    }
                    let rec = rec.clone();
                    let _ = poll_once_async(&rec).await;
                }
            }
            Err(_) => {
                // Registry missing/unreadable: skip tick.
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
        }
    }
}

/// Documented env names (tests / docs).
pub fn env_knobs() -> (&'static str, &'static str) {
    (ENV_STUB_PHASE_TIMEOUT_SECS, ENV_OUTCOME_POLL_MS)
}

/// Convenience: status after wait (used by tests).
pub fn status_after_wait(record: &ProjectRecord) -> Result<StatusView> {
    run::status(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ENV_OUTCOME_POLL_MS, ENV_STUB_PHASE_TIMEOUT_SECS};
    use crate::outcome::{
        FailureClass, OutcomeSource, PhaseOutcome, outcome_current_path, save_current_outcome,
        write_and_apply,
    };
    use crate::state::STUB_PHASE_ACTIVE;
    use std::sync::{Mutex, OnceLock};
    use tempfile::tempdir;
    use uuid::Uuid;

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn rec(path: &std::path::Path) -> ProjectRecord {
        ProjectRecord {
            id: Uuid::new_v4().to_string(),
            path: path.to_path_buf(),
            display_name: None,
            layout_profile: "nested".into(),
            state_dir: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn poll_applies_file_drop() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run::run(&r, None).unwrap();
        let o = PhaseOutcome::success(STUB_PHASE_ACTIVE, OutcomeSource::File, None, None, None);
        save_current_outcome(&r, &o).unwrap();
        assert!(outcome_current_path(&r).exists());
        let view = poll_once(&r).unwrap().expect("should apply");
        assert_eq!(view.status, RunStatus::Idle);
        assert!(!outcome_current_path(&r).exists());
    }

    #[test]
    fn wait_sees_file_drop() {
        let _guard = env_lock().lock().unwrap();
        unsafe {
            std::env::set_var(ENV_OUTCOME_POLL_MS, "50");
        }
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run::run(&r, None).unwrap();

        let path = r.path.clone();
        let id = r.id.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            let rec = ProjectRecord {
                id,
                path,
                display_name: None,
                layout_profile: "nested".into(),
                state_dir: None,
                created_at: chrono::Utc::now(),
            };
            let o = PhaseOutcome::success(STUB_PHASE_ACTIVE, OutcomeSource::File, None, None, None);
            save_current_outcome(&rec, &o).unwrap();
        });

        let view = wait_for_outcome(&r, 5).unwrap();
        handle.join().unwrap();
        assert_eq!(view.status, RunStatus::Idle);
        unsafe {
            std::env::remove_var(ENV_OUTCOME_POLL_MS);
        }
    }

    #[test]
    fn short_budget_produces_timeout() {
        let _guard = env_lock().lock().unwrap();
        unsafe {
            std::env::set_var(ENV_STUB_PHASE_TIMEOUT_SECS, "1");
            std::env::set_var(ENV_OUTCOME_POLL_MS, "50");
        }
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run::run(&r, None).unwrap();
        let view = wait_for_outcome(&r, 5).unwrap();
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.failure_class, Some(FailureClass::Timeout));
        unsafe {
            std::env::remove_var(ENV_STUB_PHASE_TIMEOUT_SECS);
            std::env::remove_var(ENV_OUTCOME_POLL_MS);
        }
    }

    #[test]
    fn pause_freezes_timeout() {
        let _guard = env_lock().lock().unwrap();
        unsafe {
            std::env::set_var(ENV_STUB_PHASE_TIMEOUT_SECS, "1");
            std::env::set_var(ENV_OUTCOME_POLL_MS, "50");
        }
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run::run(&r, None).unwrap();
        run::pause(&r).unwrap();
        // Sleep longer than budget while paused — must not timeout.
        std::thread::sleep(Duration::from_millis(1500));
        let tick = poll_once(&r).unwrap();
        assert!(tick.is_none(), "timeout must not fire while Paused");
        let s = run::status(&r).unwrap();
        assert_eq!(s.status, RunStatus::Paused);
        assert!(s.failure_class.is_none());

        // Resume: remaining budget continues from freeze (elapsed before pause was ~0).
        run::resume(&r).unwrap();
        let view = wait_for_outcome(&r, 5).unwrap();
        assert_eq!(view.failure_class, Some(FailureClass::Timeout));
        unsafe {
            std::env::remove_var(ENV_STUB_PHASE_TIMEOUT_SECS);
            std::env::remove_var(ENV_OUTCOME_POLL_MS);
        }
    }

    #[test]
    fn partial_json_does_not_crash_poll() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run::run(&r, None).unwrap();
        outcome::ensure_outcomes_dir(&r).unwrap();
        std::fs::write(outcome_current_path(&r), b"{not valid json").unwrap();
        let tick = poll_once(&r).unwrap();
        // No apply; still Running.
        assert!(
            tick.is_none()
                || tick
                    .as_ref()
                    .is_some_and(|v| v.status == RunStatus::Running)
        );
        let s = run::status(&r).unwrap();
        assert_eq!(s.status, RunStatus::Running);
    }

    #[test]
    fn wait_budget_expires_without_apply() {
        let _guard = env_lock().lock().unwrap();
        // Huge stub budget so timeout synthesizer does not fire; wait itself expires.
        unsafe {
            std::env::set_var(ENV_STUB_PHASE_TIMEOUT_SECS, "3600");
            std::env::set_var(ENV_OUTCOME_POLL_MS, "50");
        }
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run::run(&r, None).unwrap();
        let err = wait_for_outcome(&r, 1).unwrap_err();
        assert!(matches!(err, CoordinatorError::WaitBudgetExpired));
        unsafe {
            std::env::remove_var(ENV_STUB_PHASE_TIMEOUT_SECS);
            std::env::remove_var(ENV_OUTCOME_POLL_MS);
        }
    }

    #[test]
    fn write_and_apply_still_works_under_watch_tests() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run::run(&r, None).unwrap();
        let o = PhaseOutcome::success(STUB_PHASE_ACTIVE, OutcomeSource::Cli, None, None, None);
        let v = write_and_apply(&r, o).unwrap();
        assert_eq!(v.status, RunStatus::Idle);
    }
}
