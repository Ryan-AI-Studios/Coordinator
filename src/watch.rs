//! Poll loop for Phase Outcome files and stub phase timeouts (track 0005).
//!
//! Used by `coordinator wait` and the `serve` background task. File discovery is
//! poll-based (no `notify` crate); transient Windows share/parse errors skip a tick.

use std::time::{Duration, Instant};

use crate::config::{ENV_OUTCOME_POLL_MS, ENV_STUB_PHASE_TIMEOUT_SECS, outcome_poll_interval};
use crate::error::{CoordinatorError, Result};
use crate::outcome::{self, try_load_current_outcome};
use crate::registry::ProjectRecord;
use crate::run;
use crate::state::{RunStatus, StatusView, load_run_state};

/// One poll tick: **tick first**, then file apply, then timeout, then progress watchdog.
///
/// Returns `Some(view)` when an outcome was applied this tick, or when the
/// progress watchdog first fires / clears a stall. `None` otherwise.
/// Never panics on unreadable/partial JSON (skips file apply for this tick).
pub fn poll_once(record: &ProjectRecord) -> Result<Option<StatusView>> {
    // 0) Drive the canonical graph (inject / stub / named drives / join).
    //    `cross-model-review` and `ci-wait` still tick while Paused (finish current phase).
    match crate::workflow::tick(record) {
        Ok(Some(view)) => return Ok(Some(view)),
        Ok(None) => {}
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("apply race")
                || msg.contains("cannot apply outcome while status is")
                || msg.contains("does not match current phase")
            {
                // skip
            } else {
                return Err(e);
            }
        }
    }

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

    // 2) Timeout while Running only — decide+apply under the same run-state lock
    // so a concurrent pause cannot lose to a stale pre-lock snapshot.
    match outcome::try_timeout_under_lock(record) {
        Ok(Some(view)) => return Ok(Some(view)),
        Ok(None) => {}
        // Race lost to another apply (e.g. late success): not a poll crash; retry next tick.
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("apply race")
                || msg.contains("cannot apply outcome while status is")
                || msg.contains("timeout outcome rejected")
            {
                // skip
            } else {
                return Err(e);
            }
        }
    }

    // 3) Progress watchdog — surface only. Never fails the poll (torn sidecar = skip).
    match crate::workflow::watchdog::check_stall(record) {
        Ok(Some(view)) => return Ok(Some(view)),
        Ok(None) => {}
        Err(_) => {}
    }

    Ok(None)
}

/// Block until the run reaches Idle/Stopped or `timeout_secs` elapses.
///
/// Intermediate phase applies keep the loop going so `--driver stub` can walk
/// the full graph in one `wait`.
///
/// Exit mapping for CLI:
/// - Ok(view) → exit 0 (terminal success **or** failure applied, including timeout)
/// - Err(WaitBudgetExpired) → exit 2
/// - other Err → exit 1 / mapped codes
pub fn wait_for_outcome(record: &ProjectRecord, timeout_secs: u64) -> Result<StatusView> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let interval = outcome_poll_interval();
    let mut last: Option<StatusView> = None;

    loop {
        if let Some(view) = poll_once(record)? {
            last = Some(view.clone());
            if matches!(view.status, RunStatus::Idle | RunStatus::Stopped) {
                return Ok(view);
            }
        }
        let state = load_run_state(record)?;
        if matches!(state.status, RunStatus::Idle | RunStatus::Stopped) {
            if let Some(v) = last {
                return Ok(v);
            }
            return run::status(record);
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
    use crate::config::{ENV_OUTCOME_POLL_MS, ENV_STUB_PHASE_TIMEOUT_SECS, test_env_lock};
    use crate::outcome::{
        FailureClass, OutcomeSource, PhaseOutcome, outcome_current_path, save_current_outcome,
        write_and_apply,
    };
    use crate::state::STUB_PHASE_ACTIVE;
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
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn poll_applies_file_drop() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        crate::run::run_stub(&r, None).unwrap();
        let o = PhaseOutcome::success(STUB_PHASE_ACTIVE, OutcomeSource::File, None, None, None);
        save_current_outcome(&r, &o).unwrap();
        assert!(outcome_current_path(&r).unwrap().exists());
        let view = poll_once(&r).unwrap().expect("should apply");
        assert_eq!(view.status, RunStatus::Idle);
        assert!(!outcome_current_path(&r).unwrap().exists());
    }

    #[test]
    fn wait_sees_file_drop() {
        let _guard = test_env_lock();
        unsafe {
            std::env::set_var(ENV_OUTCOME_POLL_MS, "50");
        }
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        crate::run::run_stub(&r, None).unwrap();

        let path = r.path.clone();
        let id = r.id.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            let rec = ProjectRecord {
                id,
                path,
                display_name: None,
                layout_profile: crate::layout::LayoutProfile::Nested,
                conductor_dir: None,
                execution_repo: None,
                execution_repos: std::collections::BTreeMap::new(),
                state_dir: None,
                auto_merge: true,
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
        let _guard = test_env_lock();
        unsafe {
            std::env::set_var(ENV_STUB_PHASE_TIMEOUT_SECS, "1");
            std::env::set_var(ENV_OUTCOME_POLL_MS, "50");
        }
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        crate::run::run_stub(&r, None).unwrap();
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
        let _guard = test_env_lock();
        unsafe {
            std::env::set_var(ENV_STUB_PHASE_TIMEOUT_SECS, "1");
            std::env::set_var(ENV_OUTCOME_POLL_MS, "50");
        }
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        crate::run::run_stub(&r, None).unwrap();
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
        crate::run::run_stub(&r, None).unwrap();
        outcome::ensure_outcomes_dir(&r).unwrap();
        std::fs::write(outcome_current_path(&r).unwrap(), b"{not valid json").unwrap();
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
        let _guard = test_env_lock();
        // Huge stub budget so timeout synthesizer does not fire; wait itself expires.
        unsafe {
            std::env::set_var(ENV_STUB_PHASE_TIMEOUT_SECS, "3600");
            std::env::set_var(ENV_OUTCOME_POLL_MS, "50");
        }
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        crate::run::run_stub(&r, None).unwrap();
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
        crate::run::run_stub(&r, None).unwrap();
        let o = PhaseOutcome::success(STUB_PHASE_ACTIVE, OutcomeSource::Cli, None, None, None);
        let v = write_and_apply(&r, o).unwrap();
        assert_eq!(v.status, RunStatus::Idle);
    }

    #[test]
    fn wait_budget_expires_during_long_adapter_inject() {
        use crate::notify::artifact;
        use crate::run::run_with_driver;
        use crate::workflow::drive::arm_slow_adapter_inject;
        use crate::workflow::{ENV_PHASE_TIMEOUT_SECS, WorkflowDriver};

        let _guard = test_env_lock();
        unsafe {
            std::env::set_var(ENV_OUTCOME_POLL_MS, "50");
            std::env::set_var(ENV_PHASE_TIMEOUT_SECS, "3600");
            std::env::set_var(ENV_STUB_PHASE_TIMEOUT_SECS, "3600");
        }
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0013".into()), WorkflowDriver::Adapter).unwrap();
        let _inject = arm_slow_adapter_inject(Duration::from_secs(30));

        let started = Instant::now();
        let err = wait_for_outcome(&r, 1).unwrap_err();
        let elapsed = started.elapsed();
        assert!(
            matches!(err, CoordinatorError::WaitBudgetExpired),
            "err={err}"
        );
        assert!(
            elapsed < Duration::from_secs(4),
            "wait must not sit in inject; elapsed={elapsed:?}"
        );

        let s = crate::run::status(&r).unwrap();
        assert_eq!(s.status, RunStatus::Running);
        assert!(s.failure_class.is_none());
        assert_eq!(s.phase, crate::workflow::graph::PHASE_PLAN);
        assert!(artifact::existing_path(&r).is_none());
        let st = crate::state::load_run_state(&r).unwrap();
        assert_eq!(st.last_driven_phase.as_deref(), Some(st.phase.as_str()));

        unsafe {
            std::env::remove_var(ENV_OUTCOME_POLL_MS);
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
            std::env::remove_var(ENV_STUB_PHASE_TIMEOUT_SECS);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn poll_once_returns_while_hang_mock_prompt_in_flight() {
        use crate::config::ENV_COORDINATOR_HOME;
        use crate::harness::grok::mock_handshake_ok;
        use crate::harness::pool::insert_test_session;
        use crate::notify::ENV_COORDINATOR_NOTIFY;
        use crate::registry::{ProjectAddOptions, Registry};
        use crate::run::run_with_driver;
        use crate::workflow::{ENV_PHASE_TIMEOUT_SECS, WorkflowDriver};

        let _guard = test_env_lock();
        let home = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(ENV_PHASE_TIMEOUT_SECS, "2");
            std::env::set_var(ENV_OUTCOME_POLL_MS, "50");
            std::env::set_var(ENV_COORDINATOR_NOTIFY, "off");
        }
        let mut reg = Registry::default();
        let r = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        run_with_driver(&r, Some("0013".into()), WorkflowDriver::Adapter).unwrap();

        let session = crate::harness::GrokSession::start_mock(
            crate::harness::grok_cwd(&r),
            mock_handshake_ok("sess-hang"),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        insert_test_session(r.id.clone(), session).await;

        let started = Instant::now();
        let tick = poll_once(&r).unwrap();
        let elapsed = started.elapsed();
        assert!(
            tick.is_none(),
            "inject must not apply before the prompt returns"
        );
        assert!(
            elapsed < Duration::from_millis(800),
            "poll_once must not block on session/prompt; elapsed={elapsed:?}"
        );
        let st = crate::state::load_run_state(&r).unwrap();
        assert_eq!(st.last_driven_phase.as_deref(), Some(st.phase.as_str()));
        assert_eq!(st.status, RunStatus::Running);
        let pool_alive = match crate::harness::global_pool().try_lock() {
            Ok(p) => p.contains(&r.id),
            Err(_) => true, // prompt still holds the pool lock
        };
        assert!(pool_alive, "in-flight inject must leave the session alive");

        // Let the hang mock hit the 2s ACP timeout so it drops the pool lock.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        let _ = crate::harness::shutdown(Some(&r.id)).await;

        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
            std::env::remove_var(ENV_OUTCOME_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }
}
