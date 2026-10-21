//! Poll loop for Phase Outcome files and stub phase timeouts (track 0005).
//!
//! Used by CLI `run` (default tick), `coordinator wait`, and the `serve`
//! background task. File discovery is poll-based (no `notify` crate); transient
//! Windows share/parse errors skip a tick.

use std::time::{Duration, Instant};

use crate::config::{ENV_OUTCOME_POLL_MS, ENV_STUB_PHASE_TIMEOUT_SECS, outcome_poll_interval};
use crate::error::{CoordinatorError, Result};
use crate::outcome::{self, FailureClass, try_load_current_outcome};
use crate::registry::{AutoStartPolicy, ProjectRecord};
use crate::run;
use crate::state::{RunStatus, StatusView, load_run_state, save_run_state, with_run_state_lock};

/// One poll tick: **tick first**, then file apply, then timeout, then progress watchdog.
///
/// Returns `Some(view)` when an outcome was applied this tick, or when the
/// progress watchdog first fires / clears a stall. `None` otherwise.
/// Never panics on unreadable/partial JSON (skips file apply for this tick).
///
/// 0104: reconciles the owner stall-page marker **before and after** the body, so
/// every return path leaves durable paging state consistent (a phase advance or
/// stop inside the tick cannot strand an open page). Paging is a side effect only:
/// the return value and errors are unchanged. Normal polls deliver detached.
pub fn poll_once(record: &ProjectRecord) -> Result<Option<StatusView>> {
    crate::workflow::stall_paging::tick(record);
    let result = poll_once_inner(record);
    crate::workflow::stall_paging::tick(record);
    result
}

fn poll_once_inner(record: &ProjectRecord) -> Result<Option<StatusView>> {
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
            // 0104: claim the begin before the 0027 recycle clears `stalled_at` in
            // this same tick. Load-bearing, not scope creep.
            crate::workflow::stall_paging::tick(record);
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

/// Why the ticker did not start anything this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoStartSkip {
    FeatureOff,
    /// Hitl / Never (Never also from a mid-tick Full -> Never flip).
    Policy(AutoStartPolicy),
    /// Stopped+retain, next_track, non-idle, unreadable run state.
    NotEligible,
    DurableFailure,
    /// No eligible Ready row (benign, no write).
    NoReadyRow,
    /// Running/Paused at gate 3, or a lost start race.
    AlreadyActive,
    Held,
    ProjectGone,
    /// Io read glitch: no write, retry next tick.
    TransientRead,
}

/// One auto-start attempt for one project. Never retries internally.
// Rust-side observability only (not serialized). `StatusView` is the real start
// payload; boxing it would churn every caller for a test/loop-only enum.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoStartAttempt {
    Skipped(AutoStartSkip),
    Started(StatusView),
    Failed {
        failure_class: FailureClass,
        detail: String,
    },
}

/// One auto-start attempt for one project (0103).
///
/// Stateless: the feature gate, policy, run state and Ready rows are re-read on
/// every call. `auto_start: full` only; `hitl`/`never` report and wait. The start
/// uses the canonical [`crate::api::cmd_run`] omit-pick path — there is no second
/// launch path. Never retries internally.
pub fn try_auto_start(record: &ProjectRecord) -> AutoStartAttempt {
    // Gate (a): live re-read every attempt (0102 semantics; never cache).
    if !crate::features::is_enabled(crate::features::Duty::AutoStart) {
        return AutoStartAttempt::Skipped(AutoStartSkip::FeatureOff);
    }
    // Gate (b): only `full` may auto-start.
    match record.auto_start {
        AutoStartPolicy::Full => {}
        other => return AutoStartAttempt::Skipped(AutoStartSkip::Policy(other)),
    }
    // Gate (c): fresh run state.
    let state = match load_run_state(record) {
        Ok(s) => s,
        Err(_) => return AutoStartAttempt::Skipped(AutoStartSkip::NotEligible),
    };
    if matches!(state.status, RunStatus::Running | RunStatus::Paused) {
        return AutoStartAttempt::Skipped(AutoStartSkip::AlreadyActive);
    }
    // Durable dedupe must precede gate (e): `should_pick_next_ready` is true for an
    // empty track_id even when a failure class is present.
    if state.failure_class.is_some() || crate::notify::artifact::existing_path(record).is_some() {
        return AutoStartAttempt::Skipped(AutoStartSkip::DurableFailure);
    }
    let mut pick = crate::workflow::ReadyPickState::from(&state);
    pick.auto_start = record.auto_start;
    if !crate::workflow::should_pick_next_ready(&pick) {
        return AutoStartAttempt::Skipped(AutoStartSkip::NotEligible);
    }
    // Gate (f): probeless "is there anything to start" guard. The canonical pick
    // inside `cmd_run` still decides (merged-PR probe, sticky overlay, skip list).
    match crate::workflow::pick_next_ready_excluding_sticky(
        record,
        &[],
        None,
        &state.sticky_ready_ids,
    ) {
        Ok(_id) => {}
        Err(e) => {
            if crate::workflow::conductor_md::ready_pick_miss_is_benign(&e) {
                return AutoStartAttempt::Skipped(AutoStartSkip::NoReadyRow);
            }
            // Transient read glitch (Windows share, delete in flight): skip the tick,
            // write nothing. A persistent Io skips like poll_once and never latches
            // harness_crash.
            if matches!(e, CoordinatorError::Io(_)) {
                return AutoStartAttempt::Skipped(AutoStartSkip::TransientRead);
            }
            return record_auto_start_failure(record, &e);
        }
    }
    // Gate (g): the canonical launch. Omit the id on purpose: pinning would bypass
    // the merged-PR probe, the sticky overlay and the skip list.
    match crate::api::cmd_run(Some(&record.id), None, None, false, false) {
        Ok(view) => AutoStartAttempt::Started(view),
        Err(e) => auto_start_after_error(record, &e),
    }
}

/// `spawn_blocking` wrapper, mirroring [`poll_once_async`].
pub async fn try_auto_start_async(record: &ProjectRecord) -> Result<AutoStartAttempt> {
    let rec = record.clone();
    tokio::task::spawn_blocking(move || try_auto_start(&rec))
        .await
        .map_err(|e| CoordinatorError::Message(format!("auto-start task join: {e}")))
}

/// Map a `cmd_run` error without writing for the benign cases (0103).
///
/// Only setup/parse failures and `Preflight` become durable; a lost race, a
/// policy flip, a transient `Io` read, a 0098 hold and a vanished project never
/// write.
pub(crate) fn auto_start_after_error(
    record: &ProjectRecord,
    err: &CoordinatorError,
) -> AutoStartAttempt {
    use CoordinatorError as E;
    match err {
        E::BlockedHold { .. } => AutoStartAttempt::Skipped(AutoStartSkip::Held),
        E::ProjectNotFound(_) => AutoStartAttempt::Skipped(AutoStartSkip::ProjectGone),
        E::InvalidTransition { .. } => AutoStartAttempt::Skipped(AutoStartSkip::AlreadyActive),
        E::Message(m) if m.starts_with("auto_start=never") => {
            AutoStartAttempt::Skipped(AutoStartSkip::Policy(AutoStartPolicy::Never))
        }
        E::Message(_) if crate::workflow::conductor_md::ready_pick_miss_is_benign(err) => {
            AutoStartAttempt::Skipped(AutoStartSkip::NoReadyRow)
        }
        E::Io(_) => {
            let active = load_run_state(record)
                .map(|s| matches!(s.status, RunStatus::Running | RunStatus::Paused))
                .unwrap_or(false);
            if active {
                AutoStartAttempt::Skipped(AutoStartSkip::AlreadyActive)
            } else {
                AutoStartAttempt::Skipped(AutoStartSkip::TransientRead)
            }
        }
        _ => record_auto_start_failure(record, err),
    }
}

/// Cap `last_event` the way `truncate_msg` does: at most
/// [`crate::outcome::LAST_EVENT_MESSAGE_CAP`] chars plus `…` when cut.
fn cap_auto_start_event(msg: &str) -> String {
    if msg.chars().count() <= crate::outcome::LAST_EVENT_MESSAGE_CAP {
        return msg.to_string();
    }
    let cut: String = msg
        .chars()
        .take(crate::outcome::LAST_EVENT_MESSAGE_CAP)
        .collect();
    format!("{cut}…")
}

/// Persist a launch failure in the existing `RunState` fields (0103).
///
/// Never overwrites Running/Paused and never downgrades a 0098 `Blocked` hold.
/// No `FAILURE.md`, no toast, no new `FailureClass`, no new journal kind. The
/// lock/save failure arm is **not durable**: nothing is written, one stderr line
/// is emitted, and the next tick may retry.
pub(crate) fn record_auto_start_failure(
    record: &ProjectRecord,
    err: &CoordinatorError,
) -> AutoStartAttempt {
    let class = match err {
        CoordinatorError::Preflight { .. } => FailureClass::Permission,
        _ => FailureClass::HarnessCrash,
    };
    let detail = err.to_string();
    let event = cap_auto_start_event(&format!("auto-start: launch failed ({class}): {detail}"));
    match with_run_state_lock(record, || {
        let mut state = load_run_state(record)?;
        if matches!(state.status, RunStatus::Running | RunStatus::Paused) {
            return Ok(AutoStartAttempt::Skipped(AutoStartSkip::AlreadyActive));
        }
        // Never downgrade an existing 0098 hold to a generic class.
        if state.failure_class == Some(FailureClass::Blocked) {
            return Ok(AutoStartAttempt::Skipped(AutoStartSkip::Held));
        }
        state.failure_class = Some(class);
        state.last_event = event.clone();
        state.updated_at = chrono::Utc::now();
        save_run_state(record, &state)?;
        Ok(AutoStartAttempt::Failed {
            failure_class: class,
            detail: event.clone(),
        })
    }) {
        Ok(attempt) => attempt,
        // Lock/IO failure: nothing persisted, so the next tick may retry (NOT durable).
        // Report it once on stderr — the serve loop discards the attempt value.
        Err(e) => {
            eprintln!("coordinator: auto-start: failed to persist failure ({e})");
            AutoStartAttempt::Failed {
                failure_class: class,
                detail: e.to_string(),
            }
        }
    }
}

/// Background poll all registered projects that are Running or Paused, and one
/// auto-start attempt per non-active project (0103).
pub async fn serve_poll_loop(mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let mut last_fleet: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut last_reap: Option<chrono::DateTime<chrono::Utc>> = None;
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
                    if matches!(state.status, RunStatus::Running | RunStatus::Paused) {
                        let rec = rec.clone();
                        let _ = poll_once_async(&rec).await;
                        continue;
                    }
                    if state.failure_class.is_some()
                        || crate::notify::artifact::existing_path(rec).is_some()
                    {
                        let rec = rec.clone();
                        let _ = crate::api::settle_if_conductor_completed(&rec);
                    }
                    // 0106: restore registry truth for a parked/idle project before the 0103 picker runs.
                    // The backstop refuses Running/Paused under the run-state lock, so it cannot race the
                    // fold's own promotion. The spawn_blocking wrapper keeps the async loop off the lock.
                    let _ = crate::workflow::placeholder_backstop::tick_async(rec).await;
                    // 0103: one attempt per project per tick; the helper is the only
                    // start point. Awaited so two projects never start concurrently.
                    let _ = try_auto_start_async(rec).await;
                }
                for rec in reg.list() {
                    let _ = crate::worktree::reap_aged(rec);
                }
            }
            Err(_) => {
                // Registry missing/unreadable: skip tick.
            }
        }
        let _ = crate::harness::reap::tick_async(&mut last_reap, chrono::Utc::now()).await;
        crate::notify::fleet_tick(&mut last_fleet, chrono::Utc::now());

        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
        }
    }
    let _ = crate::harness::reap::sweep_on_shutdown_async().await;
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
    use crate::config::{
        ENV_COORDINATOR_HOME, ENV_OUTCOME_POLL_MS, ENV_STUB_PHASE_TIMEOUT_SECS, MachineConfig,
        save_machine_config, test_env_lock,
    };
    use crate::outcome::{
        FailureClass, OutcomeSource, PhaseOutcome, outcome_current_path, save_current_outcome,
        write_and_apply,
    };
    use crate::state::STUB_PHASE_ACTIVE;
    use crate::workflow::graph::{ROLE_IMPLEMENTOR, ROLE_PLANNER};
    use tempfile::tempdir;
    use uuid::Uuid;

    fn pin_dummy_adapter_bindings(home: &std::path::Path) {
        let dummy = home.join("dummy-grok.exe");
        std::fs::write(&dummy, b"").unwrap();
        let cmd = dummy.to_string_lossy().into_owned();
        let mut cfg = MachineConfig::default();
        if let Some(b) = cfg.role_bindings.get_mut(ROLE_PLANNER) {
            b.command = cmd.clone();
        }
        if let Some(b) = cfg.role_bindings.get_mut(ROLE_IMPLEMENTOR) {
            b.command = cmd;
        }
        save_machine_config(&cfg).unwrap();
    }

    fn write_plan_skill(root: &std::path::Path) {
        let skill = root.join(".agents").join("skills").join("plan");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "plan skill\n").unwrap();
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
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn stopped_completed_sweep_settles_without_poll_once() {
        let dir = tempdir().unwrap();
        let cond = dir.path().join("conductor");
        std::fs::create_dir_all(cond.join("0045-Done")).unwrap();
        std::fs::write(
            cond.join("conductor.md"),
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0045-Done](0045-Done/spec.md) | `.` | **Completed** | shipped |\n",
        )
        .unwrap();
        let r = rec(dir.path());
        crate::state::ensure_state_dir(&r).unwrap();
        let mut state = crate::state::load_run_state(&r).unwrap();
        state.status = RunStatus::Stopped;
        state.track_id = Some("0045".into());
        state.run_epoch = 3;
        state.failure_class = Some(FailureClass::Timeout);
        crate::state::save_run_state(&r, &state).unwrap();
        let event = crate::notify::NotifyEvent {
            project_id: r.id.clone(),
            track_id: Some("0045".into()),
            phase: "ci-wait".into(),
            failure_class: FailureClass::Timeout,
            message: Some("stale".into()),
            last_event: "timeout".into(),
            artifact_path: crate::notify::artifact::path(&r).unwrap(),
            written_at: chrono::Utc::now(),
            run_epoch: 3,
        };
        crate::notify::artifact::write(&r, &event).unwrap();
        assert!(crate::api::settle_if_conductor_completed(&r).unwrap());
        let loaded = crate::state::load_run_state(&r).unwrap();
        assert_eq!(loaded.status, RunStatus::Stopped);
        assert!(loaded.failure_class.is_none());
        assert_eq!(loaded.last_event, crate::notify::SETTLED_DETAIL);
        assert!(crate::notify::artifact::existing_path(&r).is_none());
        assert_eq!(loaded.phase, crate::state::STUB_PHASE_IDLE);
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
                notify_progress: false,
                worktree_isolation: false,
                ready_aliases: Vec::new(),
                auto_start: Default::default(),
                state_policies: Vec::new(),
                self_continuation: false,
                ci_fix_routing: false,
                skill_aliases: std::collections::BTreeMap::new(),
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
        let home = tempdir().unwrap();
        let prev_home = std::env::var_os(ENV_COORDINATOR_HOME);
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        pin_dummy_adapter_bindings(home.path());
        let dir = tempdir().unwrap();
        write_plan_skill(dir.path());
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
            match prev_home {
                Some(v) => std::env::set_var(ENV_COORDINATOR_HOME, v),
                None => std::env::remove_var(ENV_COORDINATOR_HOME),
            }
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
        pin_dummy_adapter_bindings(home.path());
        let mut reg = Registry::default();
        let r = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        write_plan_skill(proj.path());
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
            // The mock prompt hangs for 2 s, so blocking would cost the full 2 s. A 1500 ms
            // ceiling still proves non-blocking while tolerating scheduler noise on a loaded
            // CI runner (observed 822 ms against the old 800 ms bound).
            elapsed < Duration::from_millis(1500),
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
        let _ = crate::harness::shutdown(Some(&r.id), false).await;

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
        let repo = clean_exec();
        let mut r = rec(dir.path());
        r.execution_repo = Some(repo.path().to_path_buf());
        crate::workflow::conductor_md::write_ready_fixture(dir.path(), "0020").unwrap();
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

    // ---- 0103 ticker auto-start fixtures -------------------------------------

    use crate::registry::{ProjectAddOptions, Registry};
    use crate::workflow::conductor_md::write_ready_fixture;

    fn auto_start_env_guard() -> crate::config::TestEnvGuard {
        let mut names = crate::features::test_feature_env_names();
        names.push(crate::workflow::ENV_WORKFLOW_DRIVER);
        names.push(ENV_OUTCOME_POLL_MS);
        names.push(crate::workflow::ENV_PHASE_TIMEOUT_SECS);
        names.push(ENV_STUB_PHASE_TIMEOUT_SECS);
        names.push(crate::worktree::ENV_WORKTREE);
        names.push(crate::notify::ENV_COORDINATOR_NOTIFY);
        names.push(crate::notify::ENV_COORDINATOR_NOTIFY_FLEET);
        crate::config::TestEnvGuard::capture(&names)
    }

    fn set_auto_start_env(driver: &str) {
        unsafe {
            std::env::set_var(crate::workflow::ENV_WORKFLOW_DRIVER, driver);
            std::env::set_var(ENV_OUTCOME_POLL_MS, "30");
            std::env::set_var(crate::workflow::ENV_PHASE_TIMEOUT_SECS, "3600");
            std::env::set_var(ENV_STUB_PHASE_TIMEOUT_SECS, "3600");
            std::env::set_var(crate::notify::ENV_COORDINATOR_NOTIFY, "off");
            std::env::set_var(crate::notify::ENV_COORDINATOR_NOTIFY_FLEET, "off");
            std::env::remove_var(crate::worktree::ENV_WORKTREE);
        }
    }

    /// Registered project (so `cmd_run(Some(id))` can re-resolve it) + optional
    /// Ready fixture. The home is set here; the guard restores it on drop.
    fn auto_start_project(
        policy: AutoStartPolicy,
        ready: bool,
        worktree_isolation: bool,
        exec: Option<std::path::PathBuf>,
    ) -> (tempfile::TempDir, tempfile::TempDir, ProjectRecord) {
        let home = tempdir().unwrap();
        let ws = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        reg.add(
            ws.path(),
            ProjectAddOptions {
                layout_profile: crate::layout::LayoutProfile::Nested,
                execution_repo: exec,
                auto_start: Some(policy),
                ..Default::default()
            },
        )
        .unwrap();
        if worktree_isolation {
            reg.projects[0].worktree_isolation = true;
        }
        let rec = reg.projects[0].clone();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        if ready {
            write_ready_fixture(ws.path(), "0020").unwrap();
        }
        (home, ws, rec)
    }

    fn with_ready_aliases(mut rec: ProjectRecord, aliases: &[&str]) -> ProjectRecord {
        rec.ready_aliases = aliases.iter().map(|s| (*s).to_string()).collect();
        let path = crate::config::registry_path().unwrap();
        let mut reg = Registry::load(&path).unwrap();
        if let Some(p) = reg.projects.iter_mut().find(|p| p.id == rec.id) {
            p.ready_aliases = rec.ready_aliases.clone();
        }
        reg.save(&path).unwrap();
        rec
    }

    fn write_features_config(home: &std::path::Path, features: serde_json::Value) {
        let body = serde_json::json!({
            "version": 1,
            "scan_roots": [],
            "features": features,
        });
        std::fs::write(home.join("config.json"), body.to_string()).unwrap();
    }

    fn write_conductor_row(ws: &std::path::Path, row: &str) {
        let cond = ws.join("conductor");
        std::fs::create_dir_all(&cond).unwrap();
        let md = format!(
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             {row}\n"
        );
        std::fs::write(cond.join("conductor.md"), md).unwrap();
    }

    fn write_track_dir(ws: &std::path::Path, name: &str) {
        let dir = ws.join("conductor").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("spec.md"), b"fixture spec\n").unwrap();
        std::fs::write(dir.join("plan.md"), b"fixture plan\n").unwrap();
    }

    fn on_disk(rec: &ProjectRecord) -> crate::state::RunState {
        load_run_state(rec).unwrap()
    }

    fn assert_untouched(rec: &ProjectRecord, before: &crate::state::RunState) {
        let after = on_disk(rec);
        assert_eq!(after.status, before.status);
        assert_eq!(after.run_epoch, before.run_epoch);
        assert_eq!(after.last_event, before.last_event);
        assert_eq!(after.track_id, before.track_id);
        assert_eq!(after.failure_class, before.failure_class);
    }

    fn status_log(rec: &ProjectRecord) -> String {
        std::fs::read_to_string(crate::progress_log::path(rec)).unwrap_or_default()
    }

    #[test]
    fn full_policy_starts_one_ready_track_across_three_ticks() {
        let _guard = test_env_lock();
        let _env = auto_start_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        set_auto_start_env("stub");
        let (_home, _ws, rec) = auto_start_project(AutoStartPolicy::Full, true, false, None);

        let first = try_auto_start(&rec);
        let AutoStartAttempt::Started(view) = first else {
            panic!("expected Started, got {first:?}");
        };
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.phase, crate::workflow::graph::PHASE_PLAN);
        assert_eq!(view.track_id.as_deref(), Some("0020"));
        assert_eq!(view.run_epoch, 1);

        for _ in 0..2 {
            assert_eq!(
                try_auto_start(&rec),
                AutoStartAttempt::Skipped(AutoStartSkip::AlreadyActive)
            );
        }
        let st = on_disk(&rec);
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.run_epoch, 1, "no double start");
        assert!(st.failure_class.is_none());
        let log = status_log(&rec);
        assert!(log.contains("start  track=0020"), "status.md was: {log}");
    }

    #[test]
    fn hitl_policy_never_starts_and_status_reports_hitl() {
        let _guard = test_env_lock();
        let _env = auto_start_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        set_auto_start_env("stub");
        let (_home, _ws, rec) = auto_start_project(AutoStartPolicy::Hitl, true, false, None);
        let mut st = load_run_state(&rec).unwrap();
        st.status = RunStatus::Idle;
        st.last_event = crate::workflow::LAST_EVENT_BACKLOG_CLEAR.into();
        save_run_state(&rec, &st).unwrap();

        for _ in 0..3 {
            assert_eq!(
                try_auto_start(&rec),
                AutoStartAttempt::Skipped(AutoStartSkip::Policy(AutoStartPolicy::Hitl))
            );
        }
        let after = on_disk(&rec);
        assert_eq!(after.status, RunStatus::Idle);
        assert_eq!(after.run_epoch, 0);
        assert!(after.track_id.is_none());
        assert!(after.failure_class.is_none());
        assert_eq!(run::status(&rec).unwrap().auto_start, AutoStartPolicy::Hitl);
    }

    #[test]
    fn never_policy_never_starts_even_with_ready_row() {
        let _guard = test_env_lock();
        let _env = auto_start_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        set_auto_start_env("stub");
        let (_home, _ws, rec) = auto_start_project(AutoStartPolicy::Never, true, false, None);

        for _ in 0..3 {
            assert_eq!(
                try_auto_start(&rec),
                AutoStartAttempt::Skipped(AutoStartSkip::Policy(AutoStartPolicy::Never))
            );
        }
        let after = on_disk(&rec);
        assert_eq!(after.status, RunStatus::Idle);
        assert_eq!(after.run_epoch, 0);
        assert!(after.track_id.is_none());
        assert!(after.failure_class.is_none());
    }

    #[test]
    fn running_paused_and_stopped_retain_do_not_auto_start() {
        let _guard = test_env_lock();
        let _env = auto_start_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        set_auto_start_env("stub");

        // (i) Running.
        let (_h1, _w1, running) = auto_start_project(AutoStartPolicy::Full, true, false, None);
        run::run_stub(&running, None).unwrap();
        let before = on_disk(&running);
        assert_eq!(before.status, RunStatus::Running);
        assert_eq!(
            try_auto_start(&running),
            AutoStartAttempt::Skipped(AutoStartSkip::AlreadyActive)
        );
        let after = on_disk(&running);
        assert_eq!(after.status, RunStatus::Running);
        assert_eq!(after.run_epoch, before.run_epoch);
        assert_eq!(after.last_event, before.last_event);
        assert!(after.failure_class.is_none());

        // (ii) Paused.
        let (_h2, _w2, paused) = auto_start_project(AutoStartPolicy::Full, true, false, None);
        run::run_stub(&paused, None).unwrap();
        run::pause(&paused).unwrap();
        let before = on_disk(&paused);
        assert_eq!(before.status, RunStatus::Paused);
        assert_eq!(
            try_auto_start(&paused),
            AutoStartAttempt::Skipped(AutoStartSkip::AlreadyActive)
        );
        let after = on_disk(&paused);
        assert_eq!(after.status, RunStatus::Paused);
        assert_eq!(after.run_epoch, before.run_epoch);
        assert_eq!(after.last_event, before.last_event);
        assert!(after.failure_class.is_none());

        // (iii) Stopped with a retained track is not re-armed.
        let (_h3, _w3, stopped) = auto_start_project(AutoStartPolicy::Full, true, false, None);
        run::run_stub(&stopped, Some("0020".into())).unwrap();
        run::stop(&stopped).unwrap();
        assert_eq!(
            try_auto_start(&stopped),
            AutoStartAttempt::Skipped(AutoStartSkip::NotEligible)
        );
        let after = on_disk(&stopped);
        assert_eq!(after.status, RunStatus::Stopped);
        assert_eq!(after.track_id.as_deref(), Some("0020"));
        assert_eq!(after.last_event, crate::state::STOP_LAST_EVENT);
        assert_eq!(after.run_epoch, 1);
    }

    #[test]
    fn feature_gate_config_off_then_live_on_starts() {
        let _guard = test_env_lock();
        let _env = auto_start_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        set_auto_start_env("stub");
        let (home, _ws, rec) = auto_start_project(AutoStartPolicy::Full, true, false, None);
        write_features_config(home.path(), serde_json::json!({ "auto_start": false }));

        assert_eq!(
            try_auto_start(&rec),
            AutoStartAttempt::Skipped(AutoStartSkip::FeatureOff)
        );
        assert_eq!(on_disk(&rec).run_epoch, 0);

        write_features_config(home.path(), serde_json::json!({ "auto_start": true }));
        let started = try_auto_start(&rec);
        assert!(
            matches!(started, AutoStartAttempt::Started(_)),
            "{started:?}"
        );
        assert_eq!(on_disk(&rec).run_epoch, 1, "live re-read; no restart");
    }

    #[test]
    fn feature_gate_env_off_blocks_and_env_removal_starts() {
        let _guard = test_env_lock();
        let _env = auto_start_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        set_auto_start_env("stub");
        let (_home, _ws, rec) = auto_start_project(AutoStartPolicy::Full, true, false, None);
        unsafe {
            std::env::set_var("COORDINATOR_FEATURE_AUTO_START", "off");
        }

        assert_eq!(
            try_auto_start(&rec),
            AutoStartAttempt::Skipped(AutoStartSkip::FeatureOff)
        );
        assert_eq!(on_disk(&rec).run_epoch, 0);

        unsafe {
            std::env::remove_var("COORDINATOR_FEATURE_AUTO_START");
        }
        let started = try_auto_start(&rec);
        assert!(
            matches!(started, AutoStartAttempt::Started(_)),
            "{started:?}"
        );
        assert_eq!(on_disk(&rec).run_epoch, 1);
    }

    #[test]
    fn invalid_feature_gate_fails_closed() {
        let _guard = test_env_lock();
        let _env = auto_start_env_guard();
        set_auto_start_env("stub");

        // (i) unreadable/invalid JSON.
        let (home, _ws, rec) = auto_start_project(AutoStartPolicy::Full, true, false, None);
        std::fs::write(home.path().join("config.json"), b"{not valid json").unwrap();
        crate::features::reset_feature_warnings_for_tests();
        assert_eq!(
            try_auto_start(&rec),
            AutoStartAttempt::Skipped(AutoStartSkip::FeatureOff)
        );
        assert!(!crate::features::is_enabled(
            crate::features::Duty::AutoStart
        ));
        assert_eq!(on_disk(&rec).run_epoch, 0);
        assert!(on_disk(&rec).failure_class.is_none());
        assert!(
            crate::features::take_feature_warnings().len() <= 1,
            "one line per condition"
        );

        // (ii) type-invalid table value.
        let (home, _ws, rec) = auto_start_project(AutoStartPolicy::Full, true, false, None);
        write_features_config(home.path(), serde_json::json!({ "auto_start": "yes" }));
        crate::features::reset_feature_warnings_for_tests();
        assert_eq!(
            try_auto_start(&rec),
            AutoStartAttempt::Skipped(AutoStartSkip::FeatureOff)
        );
        assert!(!crate::features::is_enabled(
            crate::features::Duty::AutoStart
        ));
        assert_eq!(on_disk(&rec).run_epoch, 0);
        assert!(on_disk(&rec).failure_class.is_none());
        assert!(crate::features::take_feature_warnings().len() <= 1);
    }

    #[test]
    fn empty_backlog_without_ready_row_is_a_silent_noop() {
        let _guard = test_env_lock();
        let _env = auto_start_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        set_auto_start_env("stub");

        // (i) only a Completed row.
        let (_h1, w1, r1) = auto_start_project(AutoStartPolicy::Full, false, false, None);
        write_track_dir(w1.path(), "0029-Done");
        write_conductor_row(w1.path(), "| 0029-Done | `.` | **Completed** | shipped |");

        // (ii) a Ready row marked `<!-- nostart -->`.
        let (_h2, w2, r2) = auto_start_project(AutoStartPolicy::Full, false, false, None);
        write_track_dir(w2.path(), "0020-Fixture");
        write_conductor_row(
            w2.path(),
            "| 0020-Fixture | `.` | **Ready — not started** <!-- nostart --> | fixture |",
        );

        // (iii) custom aliases, only a Completed row (alias error branch).
        let (_h3, w3, r3) = auto_start_project(AutoStartPolicy::Full, false, false, None);
        let r3 = with_ready_aliases(r3, &["Ready - full plan @ 0020"]);
        write_track_dir(w3.path(), "0029-Done");
        write_conductor_row(w3.path(), "| 0029-Done | `.` | **Completed** | shipped |");

        for rec in [&r1, &r2, &r3] {
            assert_eq!(
                try_auto_start(rec),
                AutoStartAttempt::Skipped(AutoStartSkip::NoReadyRow)
            );
            let after = on_disk(rec);
            assert_eq!(after.run_epoch, 0);
            assert!(after.failure_class.is_none());
            assert!(
                !after.last_event.starts_with("auto-start:"),
                "no churn: {}",
                after.last_event
            );
        }
    }

    #[test]
    fn ready_row_without_track_dir_is_a_recorded_setup_failure() {
        let _guard = test_env_lock();
        let _env = auto_start_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        set_auto_start_env("stub");
        let (_home, ws, rec) = auto_start_project(AutoStartPolicy::Full, false, false, None);
        write_conductor_row(
            ws.path(),
            "| 0021-Missing | `.` | **Ready — not started** | fixture |",
        );

        let first = try_auto_start(&rec);
        assert!(
            matches!(
                first,
                AutoStartAttempt::Failed {
                    failure_class: FailureClass::HarnessCrash,
                    ..
                }
            ),
            "{first:?}"
        );
        let st = on_disk(&rec);
        assert_eq!(st.failure_class, Some(FailureClass::HarnessCrash));
        assert!(
            st.last_event
                .starts_with("auto-start: launch failed (harness_crash):"),
            "{}",
            st.last_event
        );
        assert!(st.last_event.contains("no matching conductor directory"));
        assert_eq!(st.run_epoch, 0);

        assert_eq!(
            try_auto_start(&rec),
            AutoStartAttempt::Skipped(AutoStartSkip::DurableFailure)
        );
    }

    #[test]
    fn launch_failure_is_recorded_once_and_surfaces_in_fleet() {
        let _guard = test_env_lock();
        let _env = auto_start_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        set_auto_start_env("file_wait");
        let not_git = tempdir().unwrap();
        let (_home, _ws, rec) = auto_start_project(
            AutoStartPolicy::Full,
            true,
            true,
            Some(not_git.path().to_path_buf()),
        );

        let first = try_auto_start(&rec);
        assert!(
            matches!(
                first,
                AutoStartAttempt::Failed {
                    failure_class: FailureClass::HarnessCrash,
                    ..
                }
            ),
            "{first:?}"
        );
        let st = on_disk(&rec);
        assert_eq!(st.failure_class, Some(FailureClass::HarnessCrash));
        assert!(
            st.last_event
                .starts_with("auto-start: launch failed (harness_crash):"),
            "{}",
            st.last_event
        );
        assert_eq!(st.run_epoch, 0);
        assert!(
            crate::notify::artifact::existing_path(&rec).is_none(),
            "no FAILURE.md for auto-start"
        );
        let recorded_event = st.last_event.clone();

        for _ in 0..2 {
            assert_eq!(
                try_auto_start(&rec),
                AutoStartAttempt::Skipped(AutoStartSkip::DurableFailure)
            );
        }
        // Process-restart equivalent: a fresh read still refuses.
        let reloaded = on_disk(&rec);
        assert_eq!(reloaded.failure_class, Some(FailureClass::HarnessCrash));
        assert_eq!(reloaded.last_event, recorded_event);
        assert_eq!(reloaded.run_epoch, 0);

        let fleet = crate::notify::build_fleet_summary_event(chrono::Utc::now()).unwrap();
        let project = fleet
            .projects
            .iter()
            .find(|p| p.project_id == rec.id)
            .expect("failed project is in the fleet payload");
        assert_eq!(project.failure_class, Some(FailureClass::HarnessCrash));
        assert_eq!(project.last_event, recorded_event);

        let resolved = crate::api::cmd_failure_resolve(Some(&rec.id), false).unwrap();
        assert!(resolved.failure_class.is_none());
        assert!(on_disk(&rec).failure_class.is_none());
    }

    #[test]
    fn no_conductor_is_a_recorded_setup_failure() {
        let _guard = test_env_lock();
        let _env = auto_start_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        set_auto_start_env("stub");
        let (_home, _ws, rec) = auto_start_project(AutoStartPolicy::Full, false, false, None);

        let first = try_auto_start(&rec);
        assert!(
            matches!(
                first,
                AutoStartAttempt::Failed {
                    failure_class: FailureClass::HarnessCrash,
                    ..
                }
            ),
            "{first:?}"
        );
        let st = on_disk(&rec);
        assert_eq!(st.failure_class, Some(FailureClass::HarnessCrash));
        assert!(
            st.last_event.contains("no conductor.md at"),
            "{}",
            st.last_event
        );
        assert_eq!(st.run_epoch, 0);

        assert_eq!(
            try_auto_start(&rec),
            AutoStartAttempt::Skipped(AutoStartSkip::DurableFailure)
        );
        assert!(crate::notify::artifact::existing_path(&rec).is_none());
    }

    #[test]
    fn auto_start_error_classification() {
        let _guard = test_env_lock();
        let _env = auto_start_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        set_auto_start_env("stub");

        // Policy flip: a mid-tick `Full -> Never` is a skip, not a crash.
        let (_h1, _w1, r1) = auto_start_project(AutoStartPolicy::Full, true, false, None);
        let before = on_disk(&r1);
        assert_eq!(
            auto_start_after_error(
                &r1,
                &CoordinatorError::Message("auto_start=never; pass --track".into())
            ),
            AutoStartAttempt::Skipped(AutoStartSkip::Policy(AutoStartPolicy::Never))
        );
        assert_untouched(&r1, &before);

        // Lost start race.
        let (_h2, _w2, r2) = auto_start_project(AutoStartPolicy::Full, true, false, None);
        let before = on_disk(&r2);
        assert_eq!(
            auto_start_after_error(
                &r2,
                &CoordinatorError::InvalidTransition {
                    action: "run",
                    from: "running".into(),
                }
            ),
            AutoStartAttempt::Skipped(AutoStartSkip::AlreadyActive)
        );
        assert_untouched(&r2, &before);

        // Transient read.
        let (_h3, _w3, r3) = auto_start_project(AutoStartPolicy::Full, true, false, None);
        let before = on_disk(&r3);
        assert_eq!(
            auto_start_after_error(&r3, &CoordinatorError::Io(std::io::Error::other("share"))),
            AutoStartAttempt::Skipped(AutoStartSkip::TransientRead)
        );
        assert_untouched(&r3, &before);

        // A benign pick miss raced in after gate (f).
        let (_h4, _w4, r4) = auto_start_project(AutoStartPolicy::Full, true, false, None);
        let before = on_disk(&r4);
        assert_eq!(
            auto_start_after_error(
                &r4,
                &CoordinatorError::Message(
                    "no Ready — not started track in conductor.md; pass --track <id>".into()
                )
            ),
            AutoStartAttempt::Skipped(AutoStartSkip::NoReadyRow)
        );
        assert_untouched(&r4, &before);

        // Parse/setup errors are recorded.
        let (_h5, _w5, r5) = auto_start_project(AutoStartPolicy::Full, true, false, None);
        assert!(matches!(
            auto_start_after_error(
                &r5,
                &CoordinatorError::Message("could not parse track registry: bad".into())
            ),
            AutoStartAttempt::Failed {
                failure_class: FailureClass::HarnessCrash,
                ..
            }
        ));
        assert_eq!(on_disk(&r5).failure_class, Some(FailureClass::HarnessCrash));

        let (_h6, _w6, r6) = auto_start_project(AutoStartPolicy::Full, true, false, None);
        assert!(matches!(
            auto_start_after_error(
                &r6,
                &CoordinatorError::Message(
                    "no conductor.md at C:\\x\\conductor.md; pass --track <id>".into()
                )
            ),
            AutoStartAttempt::Failed {
                failure_class: FailureClass::HarnessCrash,
                ..
            }
        ));
        assert_eq!(on_disk(&r6).failure_class, Some(FailureClass::HarnessCrash));
    }

    #[test]
    fn custom_ready_alias_row_starts_and_empty_alias_backlog_is_benign() {
        let _guard = test_env_lock();
        let _env = auto_start_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        set_auto_start_env("stub");
        let alias = "Ready - full plan @ 0020";

        // (i) the custom phrase is the only Ready token.
        let (_h1, w1, r1) = auto_start_project(AutoStartPolicy::Full, false, false, None);
        let r1 = with_ready_aliases(r1, &[alias]);
        write_track_dir(w1.path(), "0020-Fixture");
        write_conductor_row(
            w1.path(),
            "| 0020-Fixture | `.` | **Ready - full plan @ 0020** | fixture |",
        );
        let first = try_auto_start(&r1);
        let AutoStartAttempt::Started(view) = first else {
            panic!("expected Started, got {first:?}");
        };
        assert_eq!(view.track_id.as_deref(), Some("0020"));
        assert_eq!(view.run_epoch, 1);

        // (ii) the only row carrying that phrase is Completed → alias benign miss.
        let (_h2, w2, r2) = auto_start_project(AutoStartPolicy::Full, false, false, None);
        let r2 = with_ready_aliases(r2, &[alias]);
        write_track_dir(w2.path(), "0029-Done");
        write_conductor_row(
            w2.path(),
            "| 0029-Done | `.` | **Completed - Ready - full plan @ 0020** | shipped |",
        );
        assert_eq!(
            try_auto_start(&r2),
            AutoStartAttempt::Skipped(AutoStartSkip::NoReadyRow)
        );
        let after = on_disk(&r2);
        assert_eq!(after.run_epoch, 0);
        assert!(after.failure_class.is_none());
    }

    #[test]
    fn preflight_error_maps_to_permission_and_cap_is_respected() {
        use crate::harness::preflight::{DoctorReport, DoctorRow, RowStatus};

        let _guard = test_env_lock();
        let _env = auto_start_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        set_auto_start_env("stub");

        let (_h1, _w1, r1) = auto_start_project(AutoStartPolicy::Full, true, false, None);
        let report = DoctorReport {
            ok: false,
            rows: vec![DoctorRow {
                role: "planner".into(),
                harness: "grok".into(),
                command: "grok".into(),
                path: None,
                status: RowStatus::Missing,
                required: true,
                login: Some("grok login".into()),
                detail: None,
            }],
        };
        let out = record_auto_start_failure(
            &r1,
            &CoordinatorError::Preflight {
                report: Box::new(report),
            },
        );
        assert!(
            matches!(
                out,
                AutoStartAttempt::Failed {
                    failure_class: FailureClass::Permission,
                    ..
                }
            ),
            "{out:?}"
        );
        let st = on_disk(&r1);
        assert_eq!(st.failure_class, Some(FailureClass::Permission));
        assert!(
            st.last_event
                .starts_with("auto-start: launch failed (permission):"),
            "{}",
            st.last_event
        );

        let (_h2, _w2, r2) = auto_start_project(AutoStartPolicy::Full, true, false, None);
        let out = record_auto_start_failure(&r2, &CoordinatorError::Message("x".repeat(400)));
        assert!(matches!(out, AutoStartAttempt::Failed { .. }), "{out:?}");
        let st = on_disk(&r2);
        assert!(
            st.last_event.chars().count() <= 201,
            "len={}",
            st.last_event.chars().count()
        );
        assert!(st.last_event.ends_with('…'), "{}", st.last_event);
    }

    #[test]
    fn record_failure_does_not_overwrite_running_paused_or_blocked() {
        let _guard = test_env_lock();
        let _env = auto_start_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        set_auto_start_env("stub");

        // Running.
        let (_h1, _w1, running) = auto_start_project(AutoStartPolicy::Full, true, false, None);
        run::run_stub(&running, None).unwrap();
        let before = on_disk(&running);
        assert_eq!(
            record_auto_start_failure(&running, &CoordinatorError::Message("boom".into())),
            AutoStartAttempt::Skipped(AutoStartSkip::AlreadyActive)
        );
        let after = on_disk(&running);
        assert_eq!(after.status, RunStatus::Running);
        assert_eq!(after.run_epoch, before.run_epoch);
        assert_eq!(after.last_event, before.last_event);
        assert!(after.failure_class.is_none());

        // Paused.
        let (_h2, _w2, paused) = auto_start_project(AutoStartPolicy::Full, true, false, None);
        run::run_stub(&paused, None).unwrap();
        run::pause(&paused).unwrap();
        let before = on_disk(&paused);
        assert_eq!(
            record_auto_start_failure(&paused, &CoordinatorError::Message("boom".into())),
            AutoStartAttempt::Skipped(AutoStartSkip::AlreadyActive)
        );
        let after = on_disk(&paused);
        assert_eq!(after.status, RunStatus::Paused);
        assert_eq!(after.last_event, before.last_event);
        assert!(after.failure_class.is_none());

        // Blocked hold is never downgraded.
        let (_h3, _w3, blocked) = auto_start_project(AutoStartPolicy::Full, true, false, None);
        let mut st = load_run_state(&blocked).unwrap();
        st.status = RunStatus::Stopped;
        st.phase = crate::workflow::graph::PHASE_PLAN.into();
        st.track_id = Some("0020".into());
        st.run_epoch = 2;
        st.failure_class = Some(FailureClass::Blocked);
        st.last_event = "blocked: prerequisite".into();
        save_run_state(&blocked, &st).unwrap();

        assert_eq!(
            record_auto_start_failure(&blocked, &CoordinatorError::Message("boom".into())),
            AutoStartAttempt::Skipped(AutoStartSkip::Held)
        );
        let after = on_disk(&blocked);
        assert_eq!(after.failure_class, Some(FailureClass::Blocked));
        assert_eq!(after.track_id.as_deref(), Some("0020"));
        assert_eq!(after.run_epoch, 2);
        assert_eq!(after.last_event, "blocked: prerequisite");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn serve_poll_loop_starts_one_ready_track_then_polls_it() {
        let _guard = test_env_lock();
        let _env = auto_start_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        set_auto_start_env("file_wait");
        let (_home, ws, rec) = auto_start_project(AutoStartPolicy::Full, true, false, None);
        let conductor_before =
            std::fs::read(ws.path().join("conductor").join("conductor.md")).unwrap();

        let (tx, rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(serve_poll_loop(rx));

        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let st = load_run_state(&rec).unwrap();
            if st.status == RunStatus::Running
                && st.track_id.as_deref() == Some("0020")
                && st.run_epoch == 1
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "loop did not start within 15s: status={:?} track={:?} epoch={}",
                st.status,
                st.track_id,
                st.run_epoch
            );
            tokio::time::sleep(Duration::from_millis(30)).await;
        }

        let outcome = PhaseOutcome::success(
            crate::workflow::graph::PHASE_PLAN,
            OutcomeSource::Test,
            None,
            None,
            None,
        );
        save_current_outcome(&rec, &outcome).unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let st = load_run_state(&rec).unwrap();
            if st.phase == crate::workflow::graph::PHASE_PLAN_REVIEW {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "loop did not poll the Running project to plan-review: phase={} status={:?}",
                st.phase,
                st.status
            );
            tokio::time::sleep(Duration::from_millis(30)).await;
        }

        let _ = tx.send(true);
        handle.await.unwrap();

        let st = on_disk(&rec);
        assert_eq!(st.run_epoch, 1, "no double start");
        assert_eq!(st.track_id.as_deref(), Some("0020"));
        assert!(st.failure_class.is_none());
        let conductor_after =
            std::fs::read(ws.path().join("conductor").join("conductor.md")).unwrap();
        assert_eq!(conductor_before, conductor_after, "no registry write");
        let log = status_log(&rec);
        assert!(log.contains("start  track=0020"), "status.md was: {log}");
    }

    /// 0106: the no-run case the fold cannot cover. A `Stopped` project with a retained
    /// `track_id`, a `Proposed — placeholder…` row and a real spec/plan is promoted by the
    /// serve tick, without a run. The retained id keeps 0103 from picking, so this proves
    /// the promotion only. Call order (backstop before `try_auto_start_async`) is by
    /// construction — two sequential awaits — and is not asserted here.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn serve_poll_loop_promotes_stopped_planned_placeholder_without_starting_it() {
        let _guard = test_env_lock();
        let _env = auto_start_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        set_auto_start_env("file_wait");
        let (_home, ws, rec) = auto_start_project(AutoStartPolicy::Full, false, false, None);
        write_conductor_row(
            ws.path(),
            "| 0020-Fixture | `.` | **Proposed — placeholder, needs full spec/plan pass** | fixture |",
        );
        write_track_dir(ws.path(), "0020-Fixture");
        let mut state = crate::state::RunState::idle(&rec.id);
        state.status = RunStatus::Stopped;
        state.track_id = Some("0099".into());
        state.run_epoch = 3;
        save_run_state(&rec, &state).unwrap();

        let (tx, rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(serve_poll_loop(rx));

        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let conductor =
                std::fs::read_to_string(ws.path().join("conductor").join("conductor.md")).unwrap();
            let log = status_log(&rec);
            // The tick writes the registry under the lock and journals just after releasing
            // it, so wait for both halves rather than racing the journal line.
            if conductor.contains("**Ready — not started**") && log.contains("placeholder-promoted")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "row was not promoted + journaled within 15s: conductor={conductor} log={log}"
            );
            tokio::time::sleep(Duration::from_millis(30)).await;
        }

        let log = status_log(&rec);
        assert!(log.contains("placeholder-promoted"), "status.md was: {log}");
        assert!(
            !log.contains("start  track=0020"),
            "no start path may run: {log}"
        );

        let _ = tx.send(true);
        handle.await.unwrap();

        let st = on_disk(&rec);
        assert_eq!(st.status, RunStatus::Stopped);
        assert_eq!(st.track_id.as_deref(), Some("0099"));
        assert_eq!(st.run_epoch, 3);
    }
}
