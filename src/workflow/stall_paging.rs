//! Owner stall paging (track 0104).
//!
//! At most one owner page (`begin`) and one matching resolution (`clear`) per
//! `(project, track, run_epoch)`. The begin is claimed **before** the 0027 recycle
//! clears `stalled_at` (the pre-abort tick), so the first genuine stall is not
//! lost; the durable [`crate::state::StallPagingState`] marker — not `stalled_at`,
//! which the recycle/heartbeat/fresh-run all clear — is what prevents a second
//! page in the same key. A page is open while the key is unchanged, the status is
//! `Running`/`Paused`, the phase is unchanged, and no sidecar heartbeat landed
//! after the claim; it closes once on progress, phase, track or run end. Delivery
//! has two modes: [`tick`] posts detached (normal `poll_once` polls) and
//! [`tick_blocking`] posts synchronously (operator `run::stop` only, bounded by the
//! adapter's 5 s `POST_TIMEOUT`). Both claim durable state before any POST and
//! never fail the caller; there is no retry.

use chrono::{DateTime, Utc};

use crate::error::Result;
use crate::features::{self, Duty};
use crate::notify::{EVENT_TYPE_STALL, StallClearReason, StallEvent, StallKind};
use crate::registry::ProjectRecord;
use crate::state::{
    RunState, RunStatus, StallPagingState, load_run_state, save_run_state, with_run_state_lock,
};
use crate::workflow::watchdog;

/// Reconcile the durable paging marker with the current run state.
///
/// Returns at most one event per pass (never posts) and swallows every IO/lock
/// error: paging must never fail `poll_once`.
pub fn reconcile(record: &ProjectRecord) -> Option<StallEvent> {
    // Lock-free pre-check: an ordinary tick (no marker, no stamped stall) pays one
    // state read and no extra lock.
    let state = load_run_state(record).ok()?;
    if state.stall_paging.is_idle() && state.stalled_at.is_none() {
        return None;
    }
    with_run_state_lock(record, || reconcile_locked(record))
        .ok()
        .flatten()
}

/// Reconcile, then deliver through the detached Hermes transport (normal polls).
pub fn tick(record: &ProjectRecord) {
    if let Some(event) = reconcile(record) {
        crate::notify::on_stall(&event);
    }
}

/// Reconcile, then deliver **synchronously** (operator `run::stop` only).
///
/// A one-shot CLI stop prints and exits right after `run::stop`, so a detached
/// POST can be killed mid-flight. Bounded by `POST_TIMEOUT` (5 s); a failure is
/// logged and not retried.
pub fn tick_blocking(record: &ProjectRecord) {
    if let Some(event) = reconcile(record) {
        crate::notify::on_stall_blocking(&event);
    }
}

fn reconcile_locked(record: &ProjectRecord) -> Result<Option<StallEvent>> {
    let mut state = load_run_state(record)?;
    let now = Utc::now();
    let marker = state.stall_paging.clone();

    // 1) Key change: the marker belongs to another (epoch, track) pair.
    if marker.paged_epoch.is_some() {
        let epoch_changed = marker.paged_epoch != Some(state.run_epoch);
        let track_changed = marker.paged_track != state.track_id;
        if epoch_changed || track_changed {
            let event = if marker.cleared {
                None
            } else {
                // Precedence: run_ended (epoch changed / not live) before track.
                let reason = if epoch_changed || !is_live(&state) {
                    StallClearReason::RunEnded
                } else {
                    StallClearReason::Track
                };
                Some(clear_event(record, &state, &marker, reason, now))
            };
            state.stall_paging = StallPagingState::default();
            save_run_state(record, &state)?;
            return Ok(event);
        }

        // 2) Same key, open: clear exactly once when the page is no longer open.
        if !marker.cleared {
            if is_open(record, &state, &marker) {
                return Ok(None);
            }
            let reason = clear_reason(&state, &marker);
            let event = clear_event(record, &state, &marker, reason, now);
            state.stall_paging.cleared = true;
            save_run_state(record, &state)?;
            return Ok(Some(event));
        }

        // 3) Same key, already cleared: no second begin, no second clear.
        return Ok(None);
    }

    // 4) No marker for this key: claim a begin only when every gate holds.
    if !begin_eligible(record, &state) {
        return Ok(None);
    }
    let Some(stalled_at) = state.stalled_at else {
        return Ok(None);
    };
    let base = watchdog::last_progress_at(record)
        .or(state.phase_started_at)
        .unwrap_or(stalled_at);
    let idle = watchdog::idle_since(base, &state, now);

    // Claim before delivery: a lost POST can never cause a re-claim.
    state.stall_paging = StallPagingState {
        paged_epoch: Some(state.run_epoch),
        paged_track: state.track_id.clone(),
        paged_phase: Some(state.phase.clone()),
        paged_at: Some(now),
        cleared: false,
    };
    save_run_state(record, &state)?;
    Ok(Some(begin_event(record, &state, stalled_at, idle, now)))
}

/// The four ordered begin gates: live feature gate, no child tool, `Running`,
/// canonical driven phase, a watchdog stamp, and no progress since that stamp.
fn begin_eligible(record: &ProjectRecord, state: &RunState) -> bool {
    if !features::is_enabled(Duty::StallPaging) {
        return false;
    }
    if watchdog::sidecar_tool_in_flight(record) {
        return false;
    }
    if state.status != RunStatus::Running {
        return false;
    }
    if !watchdog::should_watch(state) {
        return false;
    }
    let Some(stalled_at) = state.stalled_at else {
        return false;
    };
    watchdog::last_progress_at(record).is_none_or(|progress| progress <= stalled_at)
}

/// A page is still open when the key, status, phase and progress watermark all
/// still agree with the claim. Deliberately progress-based, not `stalled_at`-based:
/// a recycle is not a resolution.
fn is_open(record: &ProjectRecord, state: &RunState, marker: &StallPagingState) -> bool {
    let Some(paged_epoch) = marker.paged_epoch else {
        return false;
    };
    let Some(paged_at) = marker.paged_at else {
        // Incomplete marker = not provable open; take the clear path once.
        return false;
    };
    let Some(paged_phase) = marker.paged_phase.as_deref() else {
        return false;
    };
    state.run_epoch == paged_epoch
        && state.track_id == marker.paged_track
        && matches!(state.status, RunStatus::Running | RunStatus::Paused)
        && state.phase == paged_phase
        && watchdog::last_progress_at(record).is_none_or(|progress| progress <= paged_at)
}

/// Clear reason precedence: run_ended → track → phase → progress.
fn clear_reason(state: &RunState, marker: &StallPagingState) -> StallClearReason {
    if !is_live(state) {
        return StallClearReason::RunEnded;
    }
    if marker.paged_track != state.track_id {
        return StallClearReason::Track;
    }
    if marker.paged_phase.as_deref() != Some(state.phase.as_str()) {
        return StallClearReason::Phase;
    }
    StallClearReason::Progress
}

fn is_live(state: &RunState) -> bool {
    matches!(state.status, RunStatus::Running | RunStatus::Paused)
}

fn begin_event(
    record: &ProjectRecord,
    state: &RunState,
    stalled_at: DateTime<Utc>,
    idle: std::time::Duration,
    now: DateTime<Utc>,
) -> StallEvent {
    StallEvent {
        event_type: EVENT_TYPE_STALL.into(),
        kind: StallKind::Begin,
        project_id: record.id.clone(),
        track_id: state.track_id.clone(),
        phase: state.phase.clone(),
        run_epoch: state.run_epoch,
        stalled_at: Some(stalled_at),
        idle_secs: Some(idle.as_secs()),
        cleared_at: None,
        clear_reason: None,
        last_event: state.last_event.clone(),
        written_at: now,
    }
}

/// Clear echoes the **stored** key. `paged_phase` never unwraps: a corrupt or
/// incomplete marker falls back to the current phase and is reset after one clear.
fn clear_event(
    record: &ProjectRecord,
    state: &RunState,
    marker: &StallPagingState,
    reason: StallClearReason,
    now: DateTime<Utc>,
) -> StallEvent {
    let phase = marker
        .paged_phase
        .as_deref()
        .unwrap_or(state.phase.as_str())
        .to_string();
    StallEvent {
        event_type: EVENT_TYPE_STALL.into(),
        kind: StallKind::Clear,
        project_id: record.id.clone(),
        track_id: marker.paged_track.clone(),
        phase,
        run_epoch: marker.paged_epoch.unwrap_or(state.run_epoch),
        stalled_at: None,
        idle_secs: None,
        cleared_at: Some(now),
        clear_reason: Some(reason),
        last_event: state.last_event.clone(),
        written_at: now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::Duration as ChronoDuration;
    use serde_json::json;
    use tempfile::tempdir;
    use uuid::Uuid;

    use crate::config::{
        ENV_COORDINATOR_HOME, ENV_COORDINATOR_STATE_DIR, MACHINE_CONFIG_VERSION, TestEnvGuard,
        test_env_lock,
    };
    use crate::features::{
        Duty, reset_feature_warnings_for_tests, resolve_all, take_feature_warnings,
    };
    use crate::notify::artifact;
    use crate::notify::hermes::{
        CapturedRequest, ScriptedOutcome, TestHermesGuard, install_recording, install_scripted,
    };
    use crate::notify::{
        ENV_COORDINATOR_NOTIFY_PROGRESS, StallDelivery, take_stall_delivery_modes_for_tests,
    };
    use crate::run::{self, run_with_driver};
    use crate::state::{load_run_state, run_state_path, with_run_state_lock};
    use crate::watch::poll_once;
    use crate::workflow::watchdog::{ENV_PROGRESS_STALL_SECS, ProgressKind, note_progress};
    use crate::workflow::{ENV_PHASE_TIMEOUT_SECS, WorkflowDriver, graph, mark_driven};

    const HERMES_URL: &str = "http://127.0.0.1:8644/webhooks/coordinator-stall";
    const HERMES_SECRET: &str = "s3cret-stall";

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

    fn env_guard() -> TestEnvGuard {
        TestEnvGuard::capture(&[
            ENV_COORDINATOR_HOME,
            ENV_COORDINATOR_STATE_DIR,
            ENV_PROGRESS_STALL_SECS,
            ENV_PHASE_TIMEOUT_SECS,
            crate::harness::abort::ENV_CANCEL_WAIT_SECS,
            "COORDINATOR_FEATURE_STALL_PAGING",
            crate::notify::hermes::ENV_COORDINATOR_HERMES,
            crate::notify::hermes::ENV_COORDINATOR_HERMES_URL,
            crate::notify::hermes::ENV_COORDINATOR_HERMES_SECRET,
            ENV_COORDINATOR_NOTIFY_PROGRESS,
        ])
    }

    /// Isolate clocks + feature gate + Hermes env. `phase_secs` is large by default
    /// so the watchdog budget-skip arm cannot fire unless a test asks for it.
    fn isolate(home: &std::path::Path, stall_secs: &str, phase_secs: &str) {
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home);
            std::env::remove_var(ENV_COORDINATOR_STATE_DIR);
            std::env::set_var(ENV_PROGRESS_STALL_SECS, stall_secs);
            std::env::set_var(ENV_PHASE_TIMEOUT_SECS, phase_secs);
            std::env::set_var(crate::harness::abort::ENV_CANCEL_WAIT_SECS, "0");
            std::env::remove_var("COORDINATOR_FEATURE_STALL_PAGING");
            std::env::remove_var(crate::notify::hermes::ENV_COORDINATOR_HERMES);
            std::env::remove_var(crate::notify::hermes::ENV_COORDINATOR_HERMES_URL);
            std::env::remove_var(crate::notify::hermes::ENV_COORDINATOR_HERMES_SECRET);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY_PROGRESS);
        }
        reset_feature_warnings_for_tests();
        let _ = take_stall_delivery_modes_for_tests();
    }

    fn write_config(home: &std::path::Path, features: serde_json::Value) {
        let body = json!({
            "version": MACHINE_CONFIG_VERSION,
            "features": features,
        });
        std::fs::write(home.join("config.json"), body.to_string()).unwrap();
    }

    fn write_malformed_config(home: &std::path::Path) {
        let body = json!({"version": MACHINE_CONFIG_VERSION, "features": {"stall_paging": "yes"}});
        std::fs::write(home.join("config.json"), body.to_string()).unwrap();
    }

    fn sidecar(r: &ProjectRecord, at: DateTime<Utc>, tool_in_flight: bool) {
        crate::state::ensure_state_dir(r).unwrap();
        let body = json!({
            "version": 1,
            "last_progress_at": at,
            "kind": "inject",
            "session_id": "sess-1",
            "tool_in_flight": tool_in_flight,
        });
        std::fs::write(
            crate::workflow::watchdog::progress_path(r).unwrap(),
            body.to_string(),
        )
        .unwrap();
    }

    fn old_sidecar(r: &ProjectRecord) {
        sidecar(r, Utc::now() - ChronoDuration::seconds(5), false);
    }

    fn start_driven_adapter(r: &ProjectRecord) {
        run_with_driver(r, Some("0104".into()), WorkflowDriver::Adapter).unwrap();
        mark_driven(r, graph::PHASE_PLAN).unwrap();
    }

    fn set_phase(r: &ProjectRecord, phase: &str) {
        with_run_state_lock(r, || {
            let mut state = load_run_state(r)?;
            state.phase = phase.into();
            state.last_driven_phase = Some(phase.into());
            save_run_state(r, &state)
        })
        .unwrap();
    }

    fn set_stall_recycles(r: &ProjectRecord, n: u32) {
        with_run_state_lock(r, || {
            let mut state = load_run_state(r)?;
            state.stall_recycles = n;
            save_run_state(r, &state)
        })
        .unwrap();
    }

    fn stall_requests(guard: &TestHermesGuard) -> Vec<CapturedRequest> {
        guard
            .take()
            .into_iter()
            .filter(|request| header(request, "X-Coordinator-Event") == Some("stall"))
            .collect()
    }

    fn events(guard: &TestHermesGuard) -> Vec<StallEvent> {
        stall_requests(guard)
            .into_iter()
            .map(|request: CapturedRequest| serde_json::from_slice(&request.body).unwrap())
            .collect()
    }

    fn body_value(request: &CapturedRequest) -> serde_json::Value {
        serde_json::from_slice(&request.body).unwrap()
    }

    fn header<'a>(request: &'a CapturedRequest, name: &str) -> Option<&'a str> {
        request
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn begin_then_clear_fixture(
        r: &ProjectRecord,
        guard: &TestHermesGuard,
    ) -> (StallEvent, StallEvent) {
        old_sidecar(r);
        let view = poll_once(r).unwrap().expect("stall should fire");
        assert!(view.last_event.contains("recycle: stall"));
        let first = events(guard);
        assert_eq!(first.len(), 1, "one begin");
        let begin = first[0].clone();
        // The recycle cleared `last_driven_phase`; keep the fixture from re-injecting
        // (a bare tempdir has no phase skill) before the heartbeat clear.
        mark_driven(r, graph::PHASE_PLAN).unwrap();
        note_progress(r, ProgressKind::Inject, Some("sess-2"), false);
        let _ = poll_once(r).unwrap();
        let second = events(guard);
        assert_eq!(second.len(), 1, "one clear");
        (begin, second[0].clone())
    }

    #[test]
    fn genuine_stall_pages_one_begin_then_one_clear_on_heartbeat() {
        let _lock = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        isolate(home.path(), "1", "3600");
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        start_driven_adapter(&r);
        let guard = install_recording(HERMES_URL, HERMES_SECRET);

        old_sidecar(&r);
        let view = poll_once(&r).unwrap().expect("stall should fire");
        assert!(view.last_event.contains("recycle: stall"));
        let captured = stall_requests(&guard);
        assert_eq!(captured.len(), 1, "exactly one begin request");
        let request = &captured[0];
        assert_eq!(header(request, "X-Coordinator-Event"), Some("stall"));
        let body = body_value(request);
        assert_eq!(body["event_type"], "stall");
        assert_eq!(body["kind"], "begin");
        assert_eq!(body["project_id"], r.id);
        assert_eq!(body["track_id"], "0104");
        assert_eq!(body["phase"], graph::PHASE_PLAN);
        assert_eq!(body["run_epoch"], 1);
        assert!(body["stalled_at"].is_string());
        assert!(body["idle_secs"].as_u64().unwrap_or(0) >= 1);
        assert!(body.get("cleared_at").is_none());
        assert!(body.get("clear_reason").is_none());

        // A second stall in the same key must not page again.
        mark_driven(&r, graph::PHASE_PLAN).unwrap();
        sidecar(&r, Utc::now() - ChronoDuration::seconds(60), false);
        let _ = poll_once(&r).unwrap();
        assert!(
            stall_requests(&guard).is_empty(),
            "second stall is not a second page"
        );

        note_progress(&r, ProgressKind::Inject, Some("sess-2"), false);
        let _ = poll_once(&r).unwrap();
        let clears = events(&guard);
        assert_eq!(clears.len(), 1, "exactly one clear");
        assert_eq!(clears[0].kind, StallKind::Clear);
        assert_eq!(clears[0].clear_reason, Some(StallClearReason::Progress));
        assert_eq!(clears[0].run_epoch, 1);
        assert_eq!(clears[0].track_id.as_deref(), Some("0104"));
        assert_eq!(clears[0].phase, graph::PHASE_PLAN);
        assert!(clears[0].cleared_at.is_some());
        assert!(clears[0].stalled_at.is_none());
        assert!(clears[0].idle_secs.is_none());

        let _ = poll_once(&r).unwrap();
        assert!(
            stall_requests(&guard).is_empty(),
            "no duplicate after the clear"
        );
        assert!(artifact::existing_path(&r).is_none());
    }

    #[test]
    fn recycle_path_claims_begin_before_abort() {
        let _lock = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        isolate(home.path(), "1", "3600");
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        start_driven_adapter(&r);
        let guard = install_recording(HERMES_URL, HERMES_SECRET);

        old_sidecar(&r);
        let _ = poll_once(&r).unwrap();
        let state = load_run_state(&r).unwrap();
        assert_eq!(state.stall_paging.paged_epoch, Some(1));
        assert_eq!(state.stall_paging.paged_track.as_deref(), Some("0104"));
        assert!(!state.stall_paging.cleared);
        assert!(
            state.stalled_at.is_none(),
            "the recycle clears stalled_at after the claim"
        );
        assert_eq!(state.last_event, crate::harness::abort::RECYCLE_STALL_EVENT);
        assert_eq!(stall_requests(&guard).len(), 1);
    }

    #[test]
    fn busy_build_tool_in_flight_pages_nothing_and_no_spurious_clear() {
        let _lock = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        isolate(home.path(), "1", "3600");
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        start_driven_adapter(&r);
        let guard = install_recording(HERMES_URL, HERMES_SECRET);

        sidecar(&r, Utc::now() - ChronoDuration::seconds(5), true);
        let view = poll_once(&r).unwrap().expect("watchdog surfaces");
        assert!(view.last_event.contains("watchdog: stall"));
        assert!(
            stall_requests(&guard).is_empty(),
            "busy build pages nothing"
        );

        let _ = poll_once(&r).unwrap();
        assert!(
            stall_requests(&guard).is_empty(),
            "still no page while the build runs"
        );

        note_progress(&r, ProgressKind::SessionUpdate, Some("sess-1"), false);
        let _ = poll_once(&r).unwrap();
        assert!(
            stall_requests(&guard).is_empty(),
            "build end + heartbeat: no clear"
        );
        let state = load_run_state(&r).unwrap();
        assert_eq!(state.last_event, "watchdog: progress");
        assert!(!state.last_event.contains("recycle: stall"));
        assert!(state.stall_paging.is_idle());
    }

    #[test]
    fn restall_same_epoch_after_clear_pages_nothing() {
        let _lock = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        isolate(home.path(), "1", "3600");
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        start_driven_adapter(&r);
        let guard = install_recording(HERMES_URL, HERMES_SECRET);

        let _ = begin_then_clear_fixture(&r, &guard);

        // Force a second stall in the same phase/epoch. The large captured phase
        // timeout keeps the budget-skip arm from making this vacuous.
        set_stall_recycles(&r, 1);
        mark_driven(&r, graph::PHASE_PLAN).unwrap();
        old_sidecar(&r);
        let _ = poll_once(&r).unwrap();
        let state = load_run_state(&r).unwrap();
        assert!(
            state.stalled_at.is_some(),
            "the re-stall stamp must land before the zero-page assertion"
        );
        assert!(
            state.last_event.contains("watchdog: stall"),
            "last_event={}",
            state.last_event
        );
        assert_eq!(
            stall_requests(&guard).len(),
            0,
            "same-key re-stall does not re-page"
        );

        let _ = poll_once(&r).unwrap();
        assert!(stall_requests(&guard).is_empty());
        note_progress(&r, ProgressKind::Inject, Some("sess-3"), false);
        let _ = poll_once(&r).unwrap();
        assert!(
            stall_requests(&guard).is_empty(),
            "no second clear for a cleared key"
        );
    }

    #[test]
    fn fresh_run_epoch_pages_a_new_begin() {
        let _lock = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        isolate(home.path(), "1", "3600");
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        start_driven_adapter(&r);
        let guard = install_recording(HERMES_URL, HERMES_SECRET);

        // Unpaged epoch N: no marker, no request.
        assert!(load_run_state(&r).unwrap().stall_paging.is_idle());
        assert!(reconcile(&r).is_none());
        assert!(stall_requests(&guard).is_empty());

        run::stop(&r).unwrap();
        run_with_driver(&r, Some("0104".into()), WorkflowDriver::Adapter).unwrap();
        mark_driven(&r, graph::PHASE_PLAN).unwrap();
        assert_eq!(load_run_state(&r).unwrap().run_epoch, 2);
        old_sidecar(&r);
        let _ = poll_once(&r).unwrap();

        let captured = events(&guard);
        assert_eq!(captured.len(), 1, "exactly one begin, zero clears");
        assert_eq!(captured[0].kind, StallKind::Begin);
        assert_eq!(captured[0].run_epoch, 2);
    }

    #[test]
    fn stale_marker_from_old_epoch_is_closed_once_then_reset() {
        let _lock = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        isolate(home.path(), "1", "3600");
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        start_driven_adapter(&r);
        let guard = install_recording(HERMES_URL, HERMES_SECRET);

        let _ = begin_then_clear_fixture(&r, &guard);
        let before = load_run_state(&r).unwrap().stall_paging;
        assert!(before.cleared);
        assert_eq!(before.paged_epoch, Some(1));

        // A closed page is not re-cleared by the operator stop, and
        // `begin_fresh_run` must leave the marker intact so the epoch change is
        // observable (spec §4.3).
        run::stop(&r).unwrap();
        assert!(
            stall_requests(&guard).is_empty(),
            "closed marker: the stop clears nothing"
        );
        run_with_driver(&r, Some("0104".into()), WorkflowDriver::Adapter).unwrap();
        let state = load_run_state(&r).unwrap();
        assert_eq!(state.run_epoch, 2);
        assert_eq!(
            state.stall_paging, before,
            "begin_fresh_run leaves the marker"
        );

        // Simulate an open page from the old epoch.
        with_run_state_lock(&r, || {
            let mut state = load_run_state(&r)?;
            state.stall_paging.cleared = false;
            save_run_state(&r, &state)
        })
        .unwrap();

        let clear = reconcile(&r).expect("one stored-key clear");
        assert_eq!(clear.kind, StallKind::Clear);
        assert_eq!(clear.clear_reason, Some(StallClearReason::RunEnded));
        assert_eq!(clear.run_epoch, 1);
        assert_eq!(clear.track_id.as_deref(), Some("0104"));
        assert_eq!(clear.phase, graph::PHASE_PLAN);
        assert!(load_run_state(&r).unwrap().stall_paging.is_idle());

        assert!(reconcile(&r).is_none(), "the pass after the reset is empty");

        // A corrupt/incomplete marker is not provably open: one clear through the
        // no-unwrap phase fallback, then reset.
        with_run_state_lock(&r, || {
            let mut state = load_run_state(&r)?;
            state.stall_paging = StallPagingState {
                paged_epoch: Some(state.run_epoch),
                paged_track: state.track_id.clone(),
                paged_phase: None,
                paged_at: None,
                cleared: false,
            };
            save_run_state(&r, &state)
        })
        .unwrap();
        let incomplete = reconcile(&r).expect("incomplete marker clears once");
        assert_eq!(incomplete.phase, graph::PHASE_PLAN, "no-unwrap fallback");
        assert!(reconcile(&r).is_none());
    }

    #[test]
    fn clear_without_begin_pages_nothing() {
        let _lock = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        isolate(home.path(), "1", "3600");
        write_config(home.path(), json!({"stall_paging": false}));
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        start_driven_adapter(&r);
        set_stall_recycles(&r, 1);
        let guard = install_recording(HERMES_URL, HERMES_SECRET);

        old_sidecar(&r);
        let _ = poll_once(&r).unwrap();
        let state = load_run_state(&r).unwrap();
        assert!(state.stalled_at.is_some(), "the stall surfaced");
        assert!(state.stall_paging.is_idle());
        assert!(stall_requests(&guard).is_empty(), "gate off: no begin");

        note_progress(&r, ProgressKind::Inject, Some("sess-2"), false);
        assert!(reconcile(&r).is_none());
        assert!(stall_requests(&guard).is_empty(), "no marker: no clear");
    }

    #[test]
    fn phase_advance_after_page_emits_one_phase_clear() {
        let _lock = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        isolate(home.path(), "1", "3600");
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        start_driven_adapter(&r);
        let guard = install_recording(HERMES_URL, HERMES_SECRET);

        old_sidecar(&r);
        let _ = poll_once(&r).unwrap();
        assert_eq!(stall_requests(&guard).len(), 1);

        set_phase(&r, graph::PHASE_IMPLEMENT);
        let clear = reconcile(&r).expect("one phase clear");
        assert_eq!(clear.kind, StallKind::Clear);
        assert_eq!(clear.clear_reason, Some(StallClearReason::Phase));
        assert_eq!(clear.phase, graph::PHASE_PLAN);
        assert!(reconcile(&r).is_none(), "second pass: nothing");
    }

    #[test]
    fn track_change_after_page_emits_one_track_clear() {
        let _lock = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        isolate(home.path(), "1", "3600");
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        start_driven_adapter(&r);
        let guard = install_recording(HERMES_URL, HERMES_SECRET);

        old_sidecar(&r);
        let _ = poll_once(&r).unwrap();
        assert_eq!(stall_requests(&guard).len(), 1);

        with_run_state_lock(&r, || {
            let mut state = load_run_state(&r)?;
            state.track_id = Some("0105-FleetProgressNotify".into());
            save_run_state(&r, &state)
        })
        .unwrap();

        let clear = reconcile(&r).expect("one track clear");
        assert_eq!(clear.kind, StallKind::Clear);
        assert_eq!(clear.clear_reason, Some(StallClearReason::Track));
        assert_eq!(
            clear.track_id.as_deref(),
            Some("0104"),
            "stored track echoed"
        );
        assert_eq!(clear.run_epoch, 1);
        assert!(reconcile(&r).is_none(), "second pass: nothing");
    }

    #[test]
    fn pause_and_resume_keep_the_page_open_without_duplicate() {
        let _lock = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        isolate(home.path(), "1", "3600");
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        start_driven_adapter(&r);
        let guard = install_recording(HERMES_URL, HERMES_SECRET);

        old_sidecar(&r);
        let _ = poll_once(&r).unwrap();
        assert_eq!(stall_requests(&guard).len(), 1);

        mark_driven(&r, graph::PHASE_PLAN).unwrap();
        run::pause(&r).unwrap();
        assert!(stall_requests(&guard).is_empty(), "pause does not tick");
        assert!(!load_run_state(&r).unwrap().stall_paging.cleared);

        run::resume(&r).unwrap();
        assert!(stall_requests(&guard).is_empty(), "resume does not tick");
        assert!(!load_run_state(&r).unwrap().stall_paging.cleared);

        note_progress(&r, ProgressKind::Inject, Some("sess-2"), false);
        let _ = poll_once(&r).unwrap();
        let clears = events(&guard);
        assert_eq!(clears.len(), 1, "one clear after progress");
        assert_eq!(clears[0].clear_reason, Some(StallClearReason::Progress));
    }

    #[test]
    fn stop_after_page_emits_one_run_ended_clear() {
        let _lock = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        isolate(home.path(), "1", "3600");
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        start_driven_adapter(&r);
        let guard = install_recording(HERMES_URL, HERMES_SECRET);

        old_sidecar(&r);
        let _ = poll_once(&r).unwrap();
        assert_eq!(stall_requests(&guard).len(), 1);
        assert_eq!(
            take_stall_delivery_modes_for_tests(),
            vec![StallDelivery::Detached],
            "the begin used the detached transport"
        );

        // The real operator path: no synthetic poll/reconcile after this call.
        let view = run::stop(&r).unwrap();
        assert_eq!(view.status, RunStatus::Stopped);
        let captured = stall_requests(&guard);
        assert_eq!(captured.len(), 1, "exactly one run_ended clear");
        let body = body_value(&captured[0]);
        assert_eq!(body["kind"], "clear");
        assert_eq!(body["clear_reason"], "run_ended");
        assert_eq!(body["run_epoch"], 1);
        assert_eq!(body["track_id"], "0104");
        assert_eq!(body["phase"], graph::PHASE_PLAN, "stored phase echoed");
        assert!(body["cleared_at"].is_string());
        assert_eq!(
            take_stall_delivery_modes_for_tests(),
            vec![StallDelivery::Blocking],
            "the stop clear was delivered synchronously"
        );

        let second = run::stop(&r).unwrap();
        assert_eq!(second.status, RunStatus::Stopped);
        assert!(
            stall_requests(&guard).is_empty(),
            "idempotent re-stop pages nothing"
        );
        assert!(take_stall_delivery_modes_for_tests().is_empty());
    }

    #[test]
    fn blocking_stop_is_bounded_and_never_fails_the_stop() {
        let _lock = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        isolate(home.path(), "1", "3600");
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        start_driven_adapter(&r);

        let recording = install_recording(HERMES_URL, HERMES_SECRET);
        old_sidecar(&r);
        let _ = poll_once(&r).unwrap();
        assert_eq!(stall_requests(&recording).len(), 1, "begin");
        drop(recording);
        let _ = take_stall_delivery_modes_for_tests();

        let scripted = install_scripted(HERMES_URL, HERMES_SECRET, ScriptedOutcome::Timeout);
        let view = run::stop(&r).unwrap();
        assert_eq!(
            view.status,
            RunStatus::Stopped,
            "a failed POST never fails stop"
        );
        assert_eq!(stall_requests(&scripted).len(), 1, "exactly one attempt");
        assert_eq!(
            take_stall_delivery_modes_for_tests(),
            vec![StallDelivery::Blocking]
        );
        assert!(stall_requests(&scripted).is_empty(), "no retry");
        let state = load_run_state(&r).unwrap();
        assert!(state.stall_paging.cleared);
    }

    #[test]
    fn reconcile_without_stall_is_a_noop() {
        let _lock = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        isolate(home.path(), "1", "3600");
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        assert!(reconcile(&r).is_none());
        assert!(
            !run_state_path(&r).unwrap().exists(),
            "the lock-free pre-check must not write state"
        );
    }

    #[test]
    fn gate_off_pages_nothing_and_gate_on_later_pages_the_open_stall() {
        let _lock = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        isolate(home.path(), "1", "3600");
        write_config(home.path(), json!({"stall_paging": false}));
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        start_driven_adapter(&r);
        set_stall_recycles(&r, 1);
        let guard = install_recording(HERMES_URL, HERMES_SECRET);

        old_sidecar(&r);
        let _ = poll_once(&r).unwrap();
        let state = load_run_state(&r).unwrap();
        assert!(state.stalled_at.is_some(), "stall still live");
        assert!(state.stall_paging.is_idle());
        assert!(stall_requests(&guard).is_empty(), "gate off: no page");

        write_config(home.path(), json!({"stall_paging": true}));
        let _ = poll_once(&r).unwrap();
        let began = events(&guard);
        assert_eq!(began.len(), 1, "late begin while still stalled");
        assert_eq!(began[0].kind, StallKind::Begin);
        assert!(began[0].idle_secs.unwrap_or(0) >= 1);
        assert!(began[0].stalled_at.is_some());
    }

    #[test]
    fn gate_env_off_and_invalid_token_fail_closed() {
        let _lock = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        isolate(home.path(), "1", "3600");
        write_config(home.path(), json!({"stall_paging": true}));
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        start_driven_adapter(&r);
        set_stall_recycles(&r, 1);
        let guard = install_recording(HERMES_URL, HERMES_SECRET);
        old_sidecar(&r);

        unsafe {
            std::env::set_var("COORDINATOR_FEATURE_STALL_PAGING", "off");
        }
        let _ = poll_once(&r).unwrap();
        assert!(load_run_state(&r).unwrap().stalled_at.is_some());
        assert!(stall_requests(&guard).is_empty(), "env off: no page");

        reset_feature_warnings_for_tests();
        unsafe {
            std::env::set_var("COORDINATOR_FEATURE_STALL_PAGING", "of");
        }
        let _ = poll_once(&r).unwrap();
        assert!(
            stall_requests(&guard).is_empty(),
            "invalid token fails closed"
        );
        assert_eq!(take_feature_warnings().len(), 1, "one warning");

        reset_feature_warnings_for_tests();
        unsafe {
            std::env::remove_var("COORDINATOR_FEATURE_STALL_PAGING");
        }
        write_malformed_config(home.path());
        let _ = poll_once(&r).unwrap();
        assert!(
            stall_requests(&guard).is_empty(),
            "malformed table fails all duties closed"
        );
        assert_eq!(take_feature_warnings().len(), 1, "one warning");
        assert!(!resolve_all().enabled(Duty::StallPaging));
    }

    #[test]
    fn gate_off_at_clear_still_sends_the_matching_clear() {
        let _lock = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        isolate(home.path(), "1", "3600");
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        start_driven_adapter(&r);
        let guard = install_recording(HERMES_URL, HERMES_SECRET);

        old_sidecar(&r);
        let _ = poll_once(&r).unwrap();
        assert_eq!(stall_requests(&guard).len(), 1, "begin with the gate on");

        write_config(home.path(), json!({"stall_paging": false}));
        mark_driven(&r, graph::PHASE_PLAN).unwrap();
        note_progress(&r, ProgressKind::Inject, Some("sess-2"), false);
        let _ = poll_once(&r).unwrap();
        let clears = events(&guard);
        assert_eq!(clears.len(), 1, "pinned: the clear is still sent");
        assert_eq!(clears[0].kind, StallKind::Clear);
        assert_eq!(clears[0].clear_reason, Some(StallClearReason::Progress));

        assert!(reconcile(&r).is_none());
        assert!(
            stall_requests(&guard).is_empty(),
            "no new begin after the gate flip"
        );
    }

    #[test]
    fn gate_resolution_still_renders_for_status() {
        let _lock = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        isolate(home.path(), "1", "3600");
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        start_driven_adapter(&r);
        set_stall_recycles(&r, 1);
        let guard = install_recording(HERMES_URL, HERMES_SECRET);

        // default on
        let snapshot = resolve_all();
        assert!(snapshot.enabled(Duty::StallPaging));
        assert_eq!(
            snapshot.duties["stall_paging"].source,
            crate::features::FeatureSource::Default
        );
        reset_for_gate_round(&r);
        let _ = poll_once(&r).unwrap();
        assert_eq!(stall_requests(&guard).len(), 1, "default on pages");

        // config off
        write_config(home.path(), json!({"stall_paging": false}));
        let snapshot = resolve_all();
        assert!(!snapshot.enabled(Duty::StallPaging));
        assert_eq!(
            snapshot.duties["stall_paging"].source,
            crate::features::FeatureSource::Config
        );
        reset_for_gate_round(&r);
        let _ = poll_once(&r).unwrap();
        assert!(
            stall_requests(&guard).is_empty(),
            "config off does not page"
        );

        // env off over config on
        write_config(home.path(), json!({"stall_paging": true}));
        unsafe {
            std::env::set_var("COORDINATOR_FEATURE_STALL_PAGING", "off");
        }
        let snapshot = resolve_all();
        assert!(!snapshot.enabled(Duty::StallPaging));
        assert_eq!(
            snapshot.duties["stall_paging"].source,
            crate::features::FeatureSource::Env
        );
        reset_for_gate_round(&r);
        let _ = poll_once(&r).unwrap();
        assert!(stall_requests(&guard).is_empty(), "env off does not page");

        // malformed table: all off, error rendered
        unsafe {
            std::env::remove_var("COORDINATOR_FEATURE_STALL_PAGING");
        }
        write_malformed_config(home.path());
        let snapshot = resolve_all();
        assert!(!snapshot.enabled(Duty::StallPaging));
        assert_eq!(
            snapshot.duties["stall_paging"].source,
            crate::features::FeatureSource::Config
        );
        assert!(snapshot.duties["stall_paging"].error.is_some());
        reset_for_gate_round(&r);
        let _ = poll_once(&r).unwrap();
        assert!(
            stall_requests(&guard).is_empty(),
            "malformed table does not page"
        );
    }

    /// Reset the durable marker + force the same live stall so each gate round is
    /// non-vacuous (a stale `cleared=true` key would otherwise swallow the round).
    fn reset_for_gate_round(r: &ProjectRecord) {
        with_run_state_lock(r, || {
            let mut state = load_run_state(r)?;
            state.stall_paging = StallPagingState::default();
            state.stalled_at = Some(Utc::now());
            state.last_event = "watchdog: stall — no harness progress for 5s".into();
            state.stall_recycles = 1;
            state.last_driven_phase = Some(graph::PHASE_PLAN.into());
            state.phase = graph::PHASE_PLAN.into();
            state.status = RunStatus::Running;
            save_run_state(r, &state)
        })
        .unwrap();
        old_sidecar(r);
    }
}
