//! Adapter progress heartbeat + stall detect (track 0026).
//!
//! Detects and surfaces a silent ACP hang. Does **not** write `FAILURE.md`,
//! toast, or stop the run. First stall this phase is recycled by **0027**
//! only when the last heartbeat was inject (not `session/update`).

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::load_machine_config;
use crate::error::Result;
use crate::persist::atomic_write_json;
use crate::registry::ProjectRecord;
use crate::state::{
    RunState, RunStatus, StatusView, load_run_state, resolve_state_dir, save_run_state,
    with_run_state_lock,
};
use crate::workflow::graph::{self, is_canonical, is_stub_phase};
use crate::workflow::{WorkflowDriver, timeout_for_phase};

/// Env: stall interval in seconds. `0` disables. Overrides machine config.
pub const ENV_PROGRESS_STALL_SECS: &str = "COORDINATOR_PROGRESS_STALL_SECS";

/// Default stall interval when env and machine config are unset.
pub const DEFAULT_PROGRESS_STALL_SECS: u64 = 600;

const SIDECAR_NAME: &str = "harness-progress.json";
const SIDECAR_VERSION: u32 = 1;
const WRITE_DEBOUNCE: Duration = Duration::from_secs(2);

/// Diagnostic kind written to `{state_dir}/harness-progress.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProgressKind {
    #[serde(rename = "inject")]
    Inject,
    #[serde(rename = "session/update")]
    SessionUpdate,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ProgressSidecar {
    version: u32,
    last_progress_at: DateTime<Utc>,
    kind: ProgressKind,
    #[serde(default)]
    session_id: String,
}

/// Sidecar path: same `resolve_state_dir` as `harness-grok.json`.
pub fn progress_path(record: &ProjectRecord) -> Result<std::path::PathBuf> {
    Ok(resolve_state_dir(record)?.join(SIDECAR_NAME))
}

/// Delete the progress sidecar (fresh `run` / auto-start). Missing is fine.
pub fn clear_progress(record: &ProjectRecord) {
    if let Ok(path) = progress_path(record) {
        let _ = std::fs::remove_file(path);
    }
}

/// Last sidecar heartbeat, if a readable v1 file exists.
pub fn last_progress_at(record: &ProjectRecord) -> Option<DateTime<Utc>> {
    read_sidecar(record).map(|s| s.last_progress_at)
}

/// Idle after `last_progress_at`, minus pause overlap only in `[last_progress_at, now]`.
pub fn idle_since(last_progress: DateTime<Utc>, state: &RunState, now: DateTime<Utc>) -> Duration {
    let wall_ms = (now - last_progress).num_milliseconds().max(0) as u64;
    Duration::from_millis(wall_ms.saturating_sub(pause_overlap_ms(last_progress, now, state)))
}

/// Resolve stall interval: env → machine `progress_stall_secs` → 600. `0` = disabled.
pub fn progress_stall_interval() -> Option<Duration> {
    let secs = std::env::var(ENV_PROGRESS_STALL_SECS)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .or_else(|| {
            load_machine_config()
                .ok()
                .and_then(|c| c.progress_stall_secs)
        })
        .unwrap_or(DEFAULT_PROGRESS_STALL_SECS);
    if secs == 0 {
        None
    } else {
        Some(Duration::from_secs(secs))
    }
}

/// Write a heartbeat to the sidecar. Never takes the run-state lock.
///
/// Debounce: `session/update` writes are skipped when the previous write was
/// less than 2s ago, **except** the first update after `inject`.
pub fn note_progress(record: &ProjectRecord, kind: ProgressKind, session_id: Option<&str>) {
    let _ = note_progress_inner(record, kind, session_id);
}

fn note_progress_inner(
    record: &ProjectRecord,
    kind: ProgressKind,
    session_id: Option<&str>,
) -> Result<()> {
    let now = Utc::now();
    if kind == ProgressKind::SessionUpdate
        && let Some(prev) = read_sidecar(record)
        && prev.kind != ProgressKind::Inject
    {
        let gap = (now - prev.last_progress_at)
            .to_std()
            .unwrap_or(Duration::MAX);
        if gap < WRITE_DEBOUNCE {
            return Ok(());
        }
    }
    let dir = crate::state::ensure_state_dir(record)?;
    let path = dir.join(SIDECAR_NAME);
    let sidecar = ProgressSidecar {
        version: SIDECAR_VERSION,
        last_progress_at: now,
        kind,
        session_id: session_id.unwrap_or("").to_string(),
    };
    atomic_write_json(&path, &sidecar)
}

/// Evaluate stall after the phase-timeout step. Never fails `poll_once`.
///
/// On first stall: set `stalled_at`, `last_event`, stay **Running**. No artifact.
/// On heartbeat after stall: clear `stalled_at`, `last_event=watchdog: progress`.
pub fn check_stall(record: &ProjectRecord) -> Result<Option<StatusView>> {
    match check_stall_inner(record) {
        Ok(v) => Ok(v),
        Err(_) => Ok(None),
    }
}

fn check_stall_inner(record: &ProjectRecord) -> Result<Option<StatusView>> {
    with_run_state_lock(record, || {
        let mut state = load_run_state(record)?;
        if !should_watch(&state) {
            return Ok(None);
        }
        let Some(stall) = progress_stall_interval() else {
            return Ok(None);
        };
        let now = Utc::now();
        let budget = timeout_for_phase(record, &state.phase);
        let elapsed = state.effective_running_elapsed(now);
        let remaining = budget.saturating_sub(elapsed);
        if stall >= remaining {
            return Ok(None);
        }

        let last = match read_sidecar(record) {
            Some(s) => s.last_progress_at,
            None => match state.phase_started_at {
                Some(t) => t,
                None => return Ok(None),
            },
        };
        let idle = idle_since(last, &state, now);

        if idle < stall {
            if state.stalled_at.is_some() {
                state.stalled_at = None;
                state.last_event = "watchdog: progress".into();
                state.updated_at = now;
                save_run_state(record, &state)?;
                return Ok(Some(StatusView::from_record(record, &state)));
            }
            return Ok(None);
        }

        if state.stalled_at.is_some() {
            // Pause/resume overwrite last_event; re-stamp once so the surface stays honest.
            if !state.last_event.contains("watchdog: stall") {
                state.last_event = format!(
                    "watchdog: stall — no harness progress for {}s",
                    idle.as_secs()
                );
                state.updated_at = now;
                save_run_state(record, &state)?;
                return Ok(Some(StatusView::from_record(record, &state)));
            }
            return Ok(None);
        }
        state.stalled_at = Some(now);
        state.last_event = format!(
            "watchdog: stall — no harness progress for {}s",
            idle.as_secs()
        );
        state.updated_at = now;
        save_run_state(record, &state)?;
        Ok(Some(StatusView::from_record(record, &state)))
    })
}

fn should_watch(state: &RunState) -> bool {
    if state.status != RunStatus::Running {
        return false;
    }
    if state.driver != WorkflowDriver::Adapter {
        return false;
    }
    if is_stub_phase(&state.phase) {
        return false;
    }
    if !is_canonical(&state.phase) {
        return false;
    }
    if matches!(
        state.phase.as_str(),
        graph::PHASE_PLAN_REVIEW | graph::PHASE_CROSS_MODEL | graph::PHASE_CI_WAIT
    ) {
        return false;
    }
    state.last_driven_phase.as_deref() == Some(state.phase.as_str())
}

/// True when the sidecar's last heartbeat was an ACP `session/update` (Grok already worked).
pub fn last_progress_was_session_update(record: &ProjectRecord) -> bool {
    matches!(
        read_sidecar(record).map(|s| s.kind),
        Some(ProgressKind::SessionUpdate)
    )
}

fn read_sidecar(record: &ProjectRecord) -> Option<ProgressSidecar> {
    let path = progress_path(record).ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    let parsed: ProgressSidecar = serde_json::from_str(&text).ok()?;
    if parsed.version != SIDECAR_VERSION {
        return None;
    }
    Some(parsed)
}

fn pause_overlap_ms(from: DateTime<Utc>, to: DateTime<Utc>, state: &RunState) -> u64 {
    let mut ms = 0u64;
    for span in &state.pause_spans {
        ms = ms.saturating_add(clipped_ms(from, to, span.start, span.end));
    }
    if let Some(pstart) = state.pause_started_at {
        ms = ms.saturating_add(clipped_ms(from, to, pstart, to));
    }
    ms
}

fn clipped_ms(
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
    span_start: DateTime<Utc>,
    span_end: DateTime<Utc>,
) -> u64 {
    let start = if span_start > window_start {
        span_start
    } else {
        window_start
    };
    let end = if span_end < window_end {
        span_end
    } else {
        window_end
    };
    (end - start).num_milliseconds().max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ENV_COORDINATOR_HOME, test_env_lock};
    use crate::notify::artifact;
    use crate::run::{self, run_with_driver};
    use crate::state::PauseSpan;
    use crate::watch::poll_once;
    use crate::workflow::{ENV_PHASE_TIMEOUT_SECS, WorkflowDriver, mark_driven};
    use std::time::Duration;
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
            created_at: Utc::now(),
        }
    }

    fn start_driven_adapter(r: &ProjectRecord) {
        run_with_driver(r, Some("0026".into()), WorkflowDriver::Adapter).unwrap();
        mark_driven(r, graph::PHASE_PLAN).unwrap();
    }

    fn write_sidecar_at(
        r: &ProjectRecord,
        at: DateTime<Utc>,
        kind: ProgressKind,
        session_id: &str,
    ) {
        crate::state::ensure_state_dir(r).unwrap();
        let sidecar = ProgressSidecar {
            version: SIDECAR_VERSION,
            last_progress_at: at,
            kind,
            session_id: session_id.into(),
        };
        atomic_write_json(&progress_path(r).unwrap(), &sidecar).unwrap();
    }

    fn isolate_clocks(home: &std::path::Path, stall_secs: &str, phase_secs: &str) {
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home);
            std::env::set_var(ENV_PROGRESS_STALL_SECS, stall_secs);
            std::env::set_var(ENV_PHASE_TIMEOUT_SECS, phase_secs);
            std::env::set_var(crate::harness::abort::ENV_CANCEL_WAIT_SECS, "0");
        }
    }

    fn clear_clocks() {
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
            std::env::remove_var(ENV_PROGRESS_STALL_SECS);
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
            std::env::remove_var(crate::harness::abort::ENV_CANCEL_WAIT_SECS);
        }
    }

    #[test]
    fn stall_visible_on_poll_once_no_artifact() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let dir = tempdir().unwrap();
        isolate_clocks(home.path(), "1", "3600");
        let r = rec(dir.path());
        start_driven_adapter(&r);
        write_sidecar_at(
            &r,
            Utc::now() - chrono::Duration::seconds(5),
            ProgressKind::Inject,
            "sess-1",
        );

        let view = poll_once(&r).unwrap().expect("stall should fire");
        assert_eq!(view.status, RunStatus::Running);
        assert!(
            view.last_event.contains("recycle: stall"),
            "last_event={}",
            view.last_event
        );
        assert!(view.stall.is_none(), "recycle clears stalled_at");
        assert!(view.failure_class.is_none());
        assert!(artifact::existing_path(&r).is_none());
        let loaded = load_run_state(&r).unwrap();
        assert_eq!(loaded.stall_recycles, 1);
        assert!(loaded.last_driven_phase.is_none());
        clear_clocks();
    }

    #[test]
    fn heartbeat_inside_window_does_not_stall() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let dir = tempdir().unwrap();
        isolate_clocks(home.path(), "1", "3600");
        let r = rec(dir.path());
        start_driven_adapter(&r);
        note_progress(&r, ProgressKind::Inject, Some("sess-1"));

        let tick = poll_once(&r).unwrap();
        assert!(
            tick.is_none()
                || tick
                    .as_ref()
                    .is_some_and(|v| !v.last_event.contains("stall"))
        );
        let s = run::status(&r).unwrap();
        assert_eq!(s.status, RunStatus::Running);
        assert!(s.last_event.contains("run: started"));
        assert!(s.stall.is_none());
        clear_clocks();
    }

    #[test]
    fn stall_then_heartbeat_resumes() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let dir = tempdir().unwrap();
        isolate_clocks(home.path(), "1", "3600");
        let r = rec(dir.path());
        start_driven_adapter(&r);
        write_sidecar_at(
            &r,
            Utc::now() - chrono::Duration::seconds(5),
            ProgressKind::Inject,
            "sess-1",
        );
        let stalled = check_stall(&r).unwrap().expect("stall");
        assert!(stalled.last_event.contains("watchdog: stall"));

        note_progress(&r, ProgressKind::SessionUpdate, Some("sess-1"));
        let view = check_stall(&r).unwrap().expect("resume");
        assert_eq!(view.last_event, "watchdog: progress");
        assert!(view.stall.is_none());
        assert_eq!(view.status, RunStatus::Running);
        let loaded = load_run_state(&r).unwrap();
        assert!(loaded.stalled_at.is_none());
        clear_clocks();
    }

    #[test]
    fn paused_skips_stall_evaluation() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let dir = tempdir().unwrap();
        isolate_clocks(home.path(), "1", "3600");
        let r = rec(dir.path());
        start_driven_adapter(&r);
        write_sidecar_at(
            &r,
            Utc::now() - chrono::Duration::seconds(30),
            ProgressKind::Inject,
            "sess-1",
        );
        run::pause(&r).unwrap();
        let tick = poll_once(&r).unwrap();
        assert!(tick.is_none(), "Paused must not evaluate stall");
        let s = run::status(&r).unwrap();
        assert_eq!(s.status, RunStatus::Paused);
        assert!(s.stall.is_none());
        assert!(!s.last_event.contains("watchdog: stall"));
        clear_clocks();
    }

    #[test]
    fn idle_subtracts_only_post_heartbeat_pause() {
        let mut state = RunState::idle("p");
        let hb = Utc::now() - chrono::Duration::seconds(20);
        // Pre-heartbeat pause (should not count).
        state.pause_spans.push(PauseSpan {
            start: hb - chrono::Duration::seconds(30),
            end: hb - chrono::Duration::seconds(10),
        });
        // Post-heartbeat pause of 5s.
        state.pause_spans.push(PauseSpan {
            start: hb + chrono::Duration::seconds(2),
            end: hb + chrono::Duration::seconds(7),
        });
        let now = hb + chrono::Duration::seconds(12);
        let idle = idle_since(hb, &state, now);
        // wall 12s − 5s post-heartbeat pause = 7s (pre-heartbeat 20s must not apply)
        assert!(idle.as_secs() >= 6 && idle.as_secs() <= 8, "idle={idle:?}");
    }

    #[test]
    fn post_heartbeat_pause_does_not_fire_immediately_on_resume() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let dir = tempdir().unwrap();
        isolate_clocks(home.path(), "1", "3600");
        let r = rec(dir.path());
        start_driven_adapter(&r);
        note_progress(&r, ProgressKind::Inject, Some("sess-1"));
        run::pause(&r).unwrap();
        std::thread::sleep(Duration::from_millis(1200));
        run::resume(&r).unwrap();
        let tick = poll_once(&r).unwrap();
        assert!(
            tick.is_none()
                || tick
                    .as_ref()
                    .is_some_and(|v| !v.last_event.contains("watchdog: stall")),
            "post-heartbeat pause must not count as idle"
        );
        let s = run::status(&r).unwrap();
        assert!(s.stall.is_none());
        assert!(!s.last_event.contains("watchdog: stall"));
        clear_clocks();
    }

    #[test]
    fn pre_heartbeat_pause_does_not_delay_stall() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let dir = tempdir().unwrap();
        isolate_clocks(home.path(), "1", "3600");
        let r = rec(dir.path());
        start_driven_adapter(&r);
        run::pause(&r).unwrap();
        std::thread::sleep(Duration::from_millis(400));
        run::resume(&r).unwrap();
        write_sidecar_at(
            &r,
            Utc::now() - chrono::Duration::seconds(3),
            ProgressKind::SessionUpdate,
            "sess-1",
        );
        let view = poll_once(&r).unwrap().expect("stall despite earlier pause");
        assert!(
            view.last_event.contains("recycle: stall")
                || view.last_event.contains("watchdog: stall"),
            "pre-heartbeat pause must not delay stall; last_event={}",
            view.last_event
        );
        clear_clocks();
    }

    #[test]
    fn stub_and_file_wait_do_not_stall() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let dir = tempdir().unwrap();
        isolate_clocks(home.path(), "1", "3600");

        let stub = rec(&dir.path().join("stub"));
        std::fs::create_dir_all(&stub.path).unwrap();
        crate::run::run_stub(&stub, None).unwrap();
        mark_driven(&stub, crate::state::STUB_PHASE_ACTIVE).unwrap();
        write_sidecar_at(
            &stub,
            Utc::now() - chrono::Duration::seconds(30),
            ProgressKind::Inject,
            "sess",
        );
        assert!(check_stall(&stub).unwrap().is_none());
        let s = run::status(&stub).unwrap();
        assert!(!s.last_event.contains("watchdog: stall"));

        let fw = rec(&dir.path().join("fw"));
        std::fs::create_dir_all(&fw.path).unwrap();
        run_with_driver(&fw, Some("0026".into()), WorkflowDriver::FileWait).unwrap();
        mark_driven(&fw, graph::PHASE_PLAN).unwrap();
        write_sidecar_at(
            &fw,
            Utc::now() - chrono::Duration::seconds(30),
            ProgressKind::Inject,
            "sess",
        );
        assert!(check_stall(&fw).unwrap().is_none());
        clear_clocks();
    }

    #[test]
    fn named_self_progress_phases_do_not_stall() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let dir = tempdir().unwrap();
        isolate_clocks(home.path(), "1", "3600");
        let r = rec(dir.path());
        start_driven_adapter(&r);
        write_sidecar_at(
            &r,
            Utc::now() - chrono::Duration::seconds(30),
            ProgressKind::Inject,
            "sess",
        );
        for phase in [
            graph::PHASE_PLAN_REVIEW,
            graph::PHASE_CROSS_MODEL,
            graph::PHASE_CI_WAIT,
        ] {
            with_run_state_lock(&r, || {
                let mut s = load_run_state(&r)?;
                s.phase = phase.into();
                s.last_driven_phase = Some(phase.into());
                s.pending_roles = vec!["agy".into(), "opencode".into()];
                save_run_state(&r, &s)
            })
            .unwrap();
            assert!(
                check_stall(&r).unwrap().is_none(),
                "phase {phase} must not stall"
            );
        }
        clear_clocks();
    }

    #[test]
    fn stall_skipped_when_interval_meets_remaining_budget() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let dir = tempdir().unwrap();
        isolate_clocks(home.path(), "10", "2");
        let r = rec(dir.path());
        start_driven_adapter(&r);
        write_sidecar_at(
            &r,
            Utc::now() - chrono::Duration::seconds(30),
            ProgressKind::Inject,
            "sess",
        );
        assert!(
            check_stall(&r).unwrap().is_none(),
            "stall >= remaining phase budget must skip"
        );
        let s = run::status(&r).unwrap();
        assert_eq!(s.status, RunStatus::Running);
        assert!(s.failure_class.is_none());
        clear_clocks();
    }

    #[test]
    fn torn_sidecar_does_not_fail_poll() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let dir = tempdir().unwrap();
        isolate_clocks(home.path(), "1", "3600");
        let r = rec(dir.path());
        start_driven_adapter(&r);
        crate::state::ensure_state_dir(&r).unwrap();
        std::fs::write(progress_path(&r).unwrap(), b"{not valid json").unwrap();
        with_run_state_lock(&r, || {
            let mut s = load_run_state(&r)?;
            s.phase_started_at = Some(Utc::now() - chrono::Duration::seconds(5));
            save_run_state(&r, &s)
        })
        .unwrap();
        let view = poll_once(&r).unwrap().expect("last-resort stall");
        assert!(
            view.last_event.contains("recycle: stall")
                || view.last_event.contains("watchdog: stall"),
            "last_event={}",
            view.last_event
        );
        assert_eq!(view.status, RunStatus::Running);
        clear_clocks();
    }

    #[test]
    fn zero_disables_watchdog() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let dir = tempdir().unwrap();
        isolate_clocks(home.path(), "0", "3600");
        let r = rec(dir.path());
        start_driven_adapter(&r);
        write_sidecar_at(
            &r,
            Utc::now() - chrono::Duration::seconds(30),
            ProgressKind::Inject,
            "sess",
        );
        assert!(check_stall(&r).unwrap().is_none());
        assert!(progress_stall_interval().is_none());
        clear_clocks();
    }

    #[test]
    fn fresh_run_clears_stall_and_sidecar() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let dir = tempdir().unwrap();
        isolate_clocks(home.path(), "1", "3600");
        let r = rec(dir.path());
        start_driven_adapter(&r);
        write_sidecar_at(
            &r,
            Utc::now() - chrono::Duration::seconds(30),
            ProgressKind::Inject,
            "sess",
        );
        check_stall(&r).unwrap();
        assert!(progress_path(&r).unwrap().exists());
        assert!(load_run_state(&r).unwrap().stalled_at.is_some());
        with_run_state_lock(&r, || {
            let mut s = load_run_state(&r)?;
            s.stall_recycles = 1;
            save_run_state(&r, &s)
        })
        .unwrap();

        run::stop(&r).unwrap();
        let view = run_with_driver(&r, Some("0026".into()), WorkflowDriver::Adapter).unwrap();
        assert!(view.stall.is_none());
        assert!(load_run_state(&r).unwrap().stalled_at.is_none());
        assert_eq!(load_run_state(&r).unwrap().stall_recycles, 0);
        assert!(
            !progress_path(&r).unwrap().exists(),
            "fresh run must drop previous sidecar"
        );
        clear_clocks();
    }

    #[tokio::test]
    async fn mock_session_update_chunk_and_tool_call_move_sidecar() {
        use crate::harness::grok::{
            mock_handshake_ok, session_update_chunk, session_update_tool_call,
        };

        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        crate::state::ensure_state_dir(&r).unwrap();
        let mut lines = mock_handshake_ok("sess-wd");
        lines.push(session_update_chunk("hello"));
        lines.push(session_update_tool_call("read"));
        lines.push(crate::harness::grok::rpc_result(
            4,
            serde_json::json!({ "stopReason": "end_turn" }),
        ));
        let mut session = crate::harness::GrokSession::start_mock(
            dir.path().to_path_buf(),
            lines,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        session.set_progress_record(r.clone());
        assert!(last_progress_at(&r).is_none());
        session
            .inject_prompt("go", Duration::from_secs(2))
            .await
            .unwrap();
        let at = last_progress_at(&r).expect("sidecar after updates");
        assert!((Utc::now() - at).num_seconds().abs() < 5);
        let text = std::fs::read_to_string(progress_path(&r).unwrap()).unwrap();
        assert!(text.contains("session/update"), "kind={text}");
    }

    #[tokio::test]
    async fn mock_tool_call_alone_moves_sidecar() {
        use crate::harness::grok::{mock_handshake_ok, session_update_tool_call};

        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        crate::state::ensure_state_dir(&r).unwrap();
        let mut lines = mock_handshake_ok("sess-tool");
        lines.push(session_update_tool_call("read"));
        lines.push(crate::harness::grok::rpc_result(
            4,
            serde_json::json!({ "stopReason": "end_turn" }),
        ));
        let mut session = crate::harness::GrokSession::start_mock(
            dir.path().to_path_buf(),
            lines,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        session.set_progress_record(r.clone());
        session
            .inject_prompt("go", Duration::from_secs(2))
            .await
            .unwrap();
        assert!(last_progress_at(&r).is_some());
        let parsed: ProgressSidecar =
            serde_json::from_str(&std::fs::read_to_string(progress_path(&r).unwrap()).unwrap())
                .unwrap();
        assert_eq!(parsed.kind, ProgressKind::SessionUpdate);
    }

    #[test]
    fn machine_config_progress_stall_secs_when_env_unset() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::remove_var(ENV_PROGRESS_STALL_SECS);
        }
        let cfg = crate::config::MachineConfig {
            progress_stall_secs: Some(42),
            ..Default::default()
        };
        crate::config::save_machine_config(&cfg).unwrap();
        assert_eq!(progress_stall_interval(), Some(Duration::from_secs(42)));
        let cfg = crate::config::MachineConfig {
            progress_stall_secs: Some(0),
            ..Default::default()
        };
        crate::config::save_machine_config(&cfg).unwrap();
        assert!(progress_stall_interval().is_none());
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[test]
    fn idle_and_stopped_omit_last_progress_at() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let dir = tempdir().unwrap();
        isolate_clocks(home.path(), "1", "3600");
        let r = rec(dir.path());
        start_driven_adapter(&r);
        note_progress(&r, ProgressKind::Inject, Some("sess-1"));
        let running = run::status(&r).unwrap();
        assert!(running.last_progress_at.is_some());
        run::stop(&r).unwrap();
        let stopped = run::status(&r).unwrap();
        assert!(stopped.last_progress_at.is_none());
        assert!(stopped.stall.is_none());
        let json = serde_json::to_value(&stopped).unwrap();
        let last = json.get("last_progress_at");
        assert!(last.is_none() || last.is_some_and(|v| v.is_null()));
        assert!(json["stall"].is_null());
        clear_clocks();
    }

    #[test]
    fn second_stall_same_phase_does_not_recycle_again() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let dir = tempdir().unwrap();
        isolate_clocks(home.path(), "1", "3600");
        let r = rec(dir.path());
        start_driven_adapter(&r);
        write_sidecar_at(
            &r,
            Utc::now() - chrono::Duration::seconds(5),
            ProgressKind::Inject,
            "sess-1",
        );
        let first = poll_once(&r).unwrap().expect("first stall recycles");
        assert!(first.last_event.contains("recycle: stall"));
        assert_eq!(load_run_state(&r).unwrap().stall_recycles, 1);

        mark_driven(&r, graph::PHASE_PLAN).unwrap();
        write_sidecar_at(
            &r,
            Utc::now() - chrono::Duration::seconds(5),
            ProgressKind::Inject,
            "sess-2",
        );
        let second = poll_once(&r).unwrap().expect("second stall surfaces");
        assert!(
            second.last_event.contains("watchdog: stall"),
            "last_event={}",
            second.last_event
        );
        assert_eq!(load_run_state(&r).unwrap().stall_recycles, 1);
        assert!(artifact::existing_path(&r).is_none());
        assert_eq!(second.status, RunStatus::Running);
        clear_clocks();
    }

    #[test]
    fn first_stall_after_session_update_does_not_recycle() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let dir = tempdir().unwrap();
        isolate_clocks(home.path(), "1", "3600");
        let r = rec(dir.path());
        start_driven_adapter(&r);
        write_sidecar_at(
            &r,
            Utc::now() - chrono::Duration::seconds(5),
            ProgressKind::SessionUpdate,
            "sess-work",
        );
        let view = poll_once(&r).unwrap().expect("stall surfaces");
        assert!(
            view.last_event.contains("watchdog: stall"),
            "last_event={}",
            view.last_event
        );
        assert!(!view.last_event.contains("recycle: stall"));
        assert_eq!(load_run_state(&r).unwrap().stall_recycles, 0);
        assert_eq!(
            load_run_state(&r).unwrap().last_driven_phase.as_deref(),
            Some(graph::PHASE_PLAN)
        );
        assert_eq!(view.status, RunStatus::Running);
        clear_clocks();
    }

    #[test]
    fn pause_then_resume_restores_stall_last_event() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let dir = tempdir().unwrap();
        isolate_clocks(home.path(), "1", "3600");
        let r = rec(dir.path());
        start_driven_adapter(&r);
        write_sidecar_at(
            &r,
            Utc::now() - chrono::Duration::seconds(5),
            ProgressKind::Inject,
            "sess-1",
        );
        check_stall(&r).unwrap().expect("first stall surface");
        run::pause(&r).unwrap();
        run::resume(&r).unwrap();
        let view = check_stall(&r).unwrap().expect("re-stamp stall event");
        assert!(
            view.last_event.contains("watchdog: stall"),
            "last_event={}",
            view.last_event
        );
        assert!(view.stall.is_some());
        clear_clocks();
    }
}
