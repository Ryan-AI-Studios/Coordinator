//! Poll loop for Phase Outcome files and stub phase timeouts (track 0005).
//!
//! Used by CLI `run` (default tick), `coordinator wait`, and the `serve`
//! background task. File discovery is poll-based (no `notify` crate); transient
//! Windows share/parse errors skip a tick.

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
    let pre_timeout = load_run_state(record).ok();
    match outcome::try_timeout_under_lock(record) {
        Ok(Some(view)) => {
            if pre_timeout
                .as_ref()
                .is_some_and(crate::harness::abort::should_abort_on_timeout_state)
            {
                crate::harness::abort::abort_stuck_prompt(
                    record,
                    crate::harness::abort::AbortReason::Timeout,
                );
            }
            return Ok(Some(view));
        }
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
        Ok(Some(view)) => {
            if let Some(stamped) = crate::harness::abort::maybe_stamp_and_abort_stall(record) {
                return Ok(Some(stamped));
            }
            return Ok(Some(view));
        }
        Ok(None) => {}
        Err(_) => {}
    }

    Ok(None)
}

/// Block until the run reaches Idle/Stopped or an optional poll budget elapses.
///
/// `None` ticks until Idle/Stopped (no CLI poll deadline). Phase wall clocks,
/// stall/abort, and Stop still apply. `Some(n)` is today's wait budget
/// (`Some(0)` expires after at most one `poll_once`).
///
/// Intermediate phase applies keep the loop going so `--driver stub` can walk
/// the full graph in one `wait` / CLI `run`.
///
/// Exit mapping for CLI:
/// - Ok(view) → exit 0 (terminal success **or** failure applied, including timeout)
/// - Err(WaitBudgetExpired) → exit 2
/// - other Err → exit 1 / mapped codes
pub fn wait_for_outcome(record: &ProjectRecord, timeout_secs: Option<u64>) -> Result<StatusView> {
    let deadline = timeout_secs.map(|secs| Instant::now() + Duration::from_secs(secs));
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
        match deadline {
            Some(deadline) => {
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
            None => std::thread::sleep(interval),
        }
    }
}

/// True when Coordinator `serve` answers `/health` on `127.0.0.1:{port}`.
///
/// Requires JSON `{ ok: true, service: "coordinator" }`. Connection refused,
/// timeout, non-JSON, or a non-coordinator occupant is `false` (caller ticks).
pub fn coordinator_serve_listening(port: u16) -> bool {
    const PROBE: Duration = Duration::from_millis(200);
    let url = format!("http://127.0.0.1:{port}/health");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(PROBE))
        .max_redirects(0)
        .proxy(None)
        .build()
        .into();
    let Ok(mut resp) = agent.get(&url).call() else {
        return false;
    };
    let Ok(body) = resp.body_mut().read_to_string() else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
        return false;
    };
    v.get("ok") == Some(&serde_json::Value::Bool(true))
        && v.get("service").and_then(|s| s.as_str()) == Some("coordinator")
}

/// How CLI `run` looks for an already-on `serve`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeProbe {
    /// Do not probe (`--detach`, or tests that force a local tick).
    Skip,
    /// Healthy lease port, else 7420, else none.
    Auto,
    /// Probe this port only (`--serve-port` / `--check --port`).
    Port(u16),
}

/// Port whose `/health` is coordinator, following `probe`. Uses default 7420.
pub fn listening_port(probe: ServeProbe) -> Option<u16> {
    listening_port_with_default(probe, crate::config::DEFAULT_SERVE_PORT)
}

/// Same as [`listening_port`] with an injectable default (tests; do not change 7420).
pub fn listening_port_with_default(probe: ServeProbe, default_port: u16) -> Option<u16> {
    match probe {
        ServeProbe::Skip => None,
        ServeProbe::Port(n) => coordinator_serve_listening(n).then_some(n),
        ServeProbe::Auto => {
            if let Some(lease) = crate::serve_lease::read_serve_lease()
                && coordinator_serve_listening(lease.port)
            {
                return Some(lease.port);
            }
            if coordinator_serve_listening(default_port) {
                return Some(default_port);
            }
            None
        }
    }
}

/// `serve --check` JSON `source`: `flag` | `lease` | `default`.
pub fn check_source(probe: ServeProbe, found: Option<u16>) -> &'static str {
    match probe {
        ServeProbe::Port(_) => "flag",
        ServeProbe::Skip | ServeProbe::Auto => {
            if let (Some(p), Some(lease)) = (found, crate::serve_lease::read_serve_lease())
                && lease.port == p
            {
                "lease"
            } else {
                "default"
            }
        }
    }
}

/// One-shot `--check` result. Does not bind and does not write a lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeCheckReport {
    pub ok: bool,
    pub port: u16,
    pub source: &'static str,
}

pub fn serve_check_report(port_flag: Option<u16>) -> ServeCheckReport {
    serve_check_report_with_default(port_flag, crate::config::DEFAULT_SERVE_PORT)
}

pub fn serve_check_report_with_default(
    port_flag: Option<u16>,
    default_port: u16,
) -> ServeCheckReport {
    let probe = match port_flag {
        Some(n) => ServeProbe::Port(n),
        None => ServeProbe::Auto,
    };
    let found = listening_port_with_default(probe, default_port);
    let source = check_source(probe, found);
    match found {
        Some(p) => ServeCheckReport {
            ok: true,
            port: p,
            source,
        },
        None => ServeCheckReport {
            ok: false,
            port: port_flag.unwrap_or(default_port),
            source,
        },
    }
}

/// Additive ticker for `api::status` / `status_all` / `cmd_run_cli` only.
pub fn ticker_view() -> crate::state::TickerView {
    ticker_view_with_default(crate::config::DEFAULT_SERVE_PORT)
}

pub fn ticker_view_with_default(default_port: u16) -> crate::state::TickerView {
    match listening_port_with_default(ServeProbe::Auto, default_port) {
        Some(p) => crate::state::TickerView {
            owner: "serve".into(),
            port: Some(p),
        },
        None => crate::state::TickerView {
            owner: "none".into(),
            port: None,
        },
    }
}

/// Status Surface attach vs start vs skip (health JSON, never bind-probe for “up”).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeAttach {
    Attach { port: u16 },
    Start { port: u16 },
    SkipOccupied { port: u16 },
}

pub fn decide_serve_attach(requested: u16) -> ServeAttach {
    decide_serve_attach_with_default(requested, crate::config::DEFAULT_SERVE_PORT)
}

pub fn decide_serve_attach_with_default(requested: u16, default_port: u16) -> ServeAttach {
    if coordinator_serve_listening(requested) {
        return ServeAttach::Attach { port: requested };
    }
    if requested == default_port
        && let Some(lease) = crate::serve_lease::read_serve_lease()
        && coordinator_serve_listening(lease.port)
    {
        return ServeAttach::Attach { port: lease.port };
    }
    match std::net::TcpListener::bind(("127.0.0.1", requested)) {
        Ok(_listener) => ServeAttach::Start { port: requested },
        Err(_) => ServeAttach::SkipOccupied { port: requested },
    }
}

/// Repeating `/health` listener for multi-probe tests. Drop to stop.
#[cfg(test)]
pub struct HealthHold {
    pub port: u16,
    stop: Option<std::sync::mpsc::Sender<()>>,
}

#[cfg(test)]
impl Drop for HealthHold {
    fn drop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
    }
}

#[cfg(test)]
pub fn spawn_health_hold(body: &str) -> HealthHold {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let body = body.to_string();
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        loop {
            if rx.try_recv().is_ok() {
                break;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let mut buf = [0u8; 2048];
                    let _ = stream.read(&mut buf);
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes());
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
    std::thread::sleep(Duration::from_millis(20));
    HealthHold {
        port,
        stop: Some(tx),
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
            phase_timeouts_secs: std::collections::BTreeMap::new(),
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
                phase_timeouts_secs: std::collections::BTreeMap::new(),
                created_at: chrono::Utc::now(),
            };
            let o = PhaseOutcome::success(STUB_PHASE_ACTIVE, OutcomeSource::File, None, None, None);
            save_current_outcome(&rec, &o).unwrap();
        });

        let view = wait_for_outcome(&r, Some(5)).unwrap();
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
            std::env::set_var(crate::harness::abort::ENV_CANCEL_WAIT_SECS, "0");
        }
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        crate::run::run_stub(&r, None).unwrap();
        let view = wait_for_outcome(&r, Some(5)).unwrap();
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.failure_class, Some(FailureClass::Timeout));
        unsafe {
            std::env::remove_var(ENV_STUB_PHASE_TIMEOUT_SECS);
            std::env::remove_var(ENV_OUTCOME_POLL_MS);
            std::env::remove_var(crate::harness::abort::ENV_CANCEL_WAIT_SECS);
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
        let view = wait_for_outcome(&r, Some(5)).unwrap();
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
        let err = wait_for_outcome(&r, Some(1)).unwrap_err();
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
        let err = wait_for_outcome(&r, Some(1)).unwrap_err();
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

    #[test]
    fn wait_none_walks_stub_to_idle() {
        use crate::run::run_with_driver;
        use crate::workflow::{ENV_PHASE_TIMEOUT_SECS, WorkflowDriver};

        let _guard = test_env_lock();
        unsafe {
            std::env::set_var(ENV_OUTCOME_POLL_MS, "10");
            std::env::set_var(ENV_PHASE_TIMEOUT_SECS, "30");
        }
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0020".into()), WorkflowDriver::Stub).unwrap();
        let view = wait_for_outcome(&r, None).unwrap();
        assert_eq!(view.status, RunStatus::Idle);
        unsafe {
            std::env::remove_var(ENV_OUTCOME_POLL_MS);
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
        }
    }

    #[test]
    fn wait_some_zero_expires_immediately() {
        let _guard = test_env_lock();
        unsafe {
            std::env::set_var(ENV_STUB_PHASE_TIMEOUT_SECS, "3600");
            std::env::set_var(ENV_OUTCOME_POLL_MS, "50");
        }
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        crate::run::run_stub(&r, None).unwrap();
        let started = Instant::now();
        let err = wait_for_outcome(&r, Some(0)).unwrap_err();
        assert!(
            matches!(err, CoordinatorError::WaitBudgetExpired),
            "err={err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "Some(0) must expire immediately"
        );
        let s = crate::run::status(&r).unwrap();
        assert_eq!(s.status, RunStatus::Running);
        unsafe {
            std::env::remove_var(ENV_STUB_PHASE_TIMEOUT_SECS);
            std::env::remove_var(ENV_OUTCOME_POLL_MS);
        }
    }

    fn spawn_health_once(body: &str) -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = body.to_string();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        std::thread::sleep(Duration::from_millis(20));
        port
    }

    #[test]
    fn serve_listening_requires_coordinator_service() {
        let port = spawn_health_once(r#"{"ok":true,"service":"coordinator"}"#);
        assert!(coordinator_serve_listening(port));

        let port = spawn_health_once(r#"{"ok":true}"#);
        assert!(!coordinator_serve_listening(port));

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let refused = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(!coordinator_serve_listening(refused));
    }

    fn isolate_home() -> tempfile::TempDir {
        use crate::config::ENV_COORDINATOR_HOME;
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        home
    }

    fn clear_home() {
        unsafe {
            std::env::remove_var(crate::config::ENV_COORDINATOR_HOME);
        }
    }

    fn refused_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    #[test]
    fn listening_port_skip_and_explicit() {
        assert_eq!(listening_port(ServeProbe::Skip), None);
        let port = spawn_health_once(r#"{"ok":true,"service":"coordinator"}"#);
        assert_eq!(listening_port(ServeProbe::Port(port)), Some(port));
        let dead = refused_port();
        assert_eq!(listening_port(ServeProbe::Port(dead)), None);
    }

    #[test]
    fn listening_port_auto_stale_lease_falls_through_to_default() {
        let _guard = test_env_lock();
        let _home = isolate_home();
        let dead = refused_port();
        crate::serve_lease::write_serve_lease(dead).unwrap();
        assert_eq!(
            listening_port_with_default(ServeProbe::Auto, dead),
            None,
            "stale lease + dead default must not invent a port"
        );
        let hold = spawn_health_hold(r#"{"ok":true,"service":"coordinator"}"#);
        assert_eq!(
            listening_port_with_default(ServeProbe::Auto, hold.port),
            Some(hold.port)
        );
        let report = serve_check_report_with_default(None, hold.port);
        assert!(report.ok);
        assert_eq!(report.port, hold.port);
        assert_eq!(report.source, "default");
        assert_eq!(check_source(ServeProbe::Auto, Some(hold.port)), "default");
        clear_home();
    }

    #[test]
    fn listening_port_auto_healthy_lease_wins() {
        let _guard = test_env_lock();
        let _home = isolate_home();
        let hold = spawn_health_hold(r#"{"ok":true,"service":"coordinator"}"#);
        crate::serve_lease::write_serve_lease(hold.port).unwrap();
        let other = refused_port();
        assert_eq!(
            listening_port_with_default(ServeProbe::Auto, other),
            Some(hold.port)
        );
        let report = serve_check_report_with_default(None, other);
        assert!(report.ok);
        assert_eq!(report.source, "lease");
        assert_eq!(report.port, hold.port);
        clear_home();
    }

    #[test]
    fn explicit_port_ignores_healthy_lease() {
        let _guard = test_env_lock();
        let _home = isolate_home();
        let hold = spawn_health_hold(r#"{"ok":true,"service":"coordinator"}"#);
        crate::serve_lease::write_serve_lease(hold.port).unwrap();
        let dead = refused_port();
        assert_eq!(listening_port(ServeProbe::Port(dead)), None);
        let report = serve_check_report(Some(dead));
        assert!(!report.ok);
        assert_eq!(report.port, dead);
        assert_eq!(report.source, "flag");
        clear_home();
    }

    #[test]
    fn serve_check_does_not_write_lease() {
        let _guard = test_env_lock();
        let home = isolate_home();
        let port = spawn_health_once(r#"{"ok":true,"service":"coordinator"}"#);
        let report = serve_check_report(Some(port));
        assert!(report.ok);
        assert_eq!(report.source, "flag");
        assert!(!home.path().join("serve.json").exists());
        let dead = refused_port();
        let miss = serve_check_report(Some(dead));
        assert!(!miss.ok);
        assert_eq!(miss.source, "flag");
        assert!(!home.path().join("serve.json").exists());
        clear_home();
    }

    #[test]
    fn ticker_view_serve_vs_none() {
        let _guard = test_env_lock();
        let _home = isolate_home();
        let dead = refused_port();
        let none = ticker_view_with_default(dead);
        assert_eq!(none.owner, "none");
        assert!(none.port.is_none());
        let hold = spawn_health_hold(r#"{"ok":true,"service":"coordinator"}"#);
        crate::serve_lease::write_serve_lease(hold.port).unwrap();
        let serve = ticker_view_with_default(dead);
        assert_eq!(serve.owner, "serve");
        assert_eq!(serve.port, Some(hold.port));
        clear_home();
    }

    #[test]
    fn decide_attach_lease_when_requested_is_default() {
        let _guard = test_env_lock();
        let _home = isolate_home();
        let hold = spawn_health_hold(r#"{"ok":true,"service":"coordinator"}"#);
        crate::serve_lease::write_serve_lease(hold.port).unwrap();
        let fake_default = refused_port();
        assert_ne!(fake_default, hold.port);
        assert_eq!(
            decide_serve_attach_with_default(fake_default, fake_default),
            ServeAttach::Attach { port: hold.port }
        );
        clear_home();
    }

    #[test]
    fn decide_attach_requested_healthy_wins() {
        let _guard = test_env_lock();
        let _home = isolate_home();
        let requested = spawn_health_hold(r#"{"ok":true,"service":"coordinator"}"#);
        let lease = spawn_health_hold(r#"{"ok":true,"service":"coordinator"}"#);
        crate::serve_lease::write_serve_lease(lease.port).unwrap();
        assert_eq!(
            decide_serve_attach(requested.port),
            ServeAttach::Attach {
                port: requested.port
            }
        );
        clear_home();
    }

    #[test]
    fn decide_skip_occupied_non_coordinator() {
        let _guard = test_env_lock();
        let _home = isolate_home();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_eq!(
            decide_serve_attach(port),
            ServeAttach::SkipOccupied { port }
        );
        drop(listener);
        clear_home();
    }

    #[test]
    fn decide_start_when_requested_free() {
        let _guard = test_env_lock();
        let _home = isolate_home();
        let port = refused_port();
        assert_eq!(decide_serve_attach(port), ServeAttach::Start { port });
        clear_home();
    }
}
