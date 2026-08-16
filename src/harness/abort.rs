//! Abort a wedged ACP Prompt and recycle the holder (track **0027**).
//!
//! CancelHandle registry lives **outside** `global_pool()` — `prompt()` holds
//! that mutex for the whole inject.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::error::{CoordinatorError, Result};
use crate::harness::grok::CancelHandle;
use crate::registry::ProjectRecord;
use crate::state::{RunStatus, StatusView, load_run_state, save_run_state, with_run_state_lock};

/// Env: how long to wait after `session/cancel` before recycle. Default **10**.
/// `0` skips the wait (recycle immediately — useful in tests).
pub const ENV_CANCEL_WAIT_SECS: &str = "COORDINATOR_CANCEL_WAIT_SECS";

/// Default cancel-wait when the env is unset.
pub const DEFAULT_CANCEL_WAIT_SECS: u64 = 10;

/// Stall recycle `last_event` (stay Running; no FAILURE.md).
pub const RECYCLE_STALL_EVENT: &str = "recycle: stall — new session";

/// Why abort ran. Timeout apply text is never overwritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbortReason {
    Timeout,
    Stall,
    PromptTimeout,
}

static CANCEL_HANDLES: OnceLock<Mutex<HashMap<String, CancelHandle>>> = OnceLock::new();

fn handles() -> &'static Mutex<HashMap<String, CancelHandle>> {
    CANCEL_HANDLES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a per-project cancel writer. Not behind `global_pool()`.
pub fn register_cancel_handle(project_id: impl Into<String>, handle: CancelHandle) {
    handles()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(project_id.into(), handle);
}

/// Drop the handle when the session is removed / shutdown.
pub fn unregister_cancel_handle(project_id: &str) {
    handles()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(project_id);
}

/// Clone the handle if this process owns the session.
pub fn cancel_handle_for(project_id: &str) -> Option<CancelHandle> {
    handles()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(project_id)
        .cloned()
}

/// Cancel-wait duration. `0` = skip wait, recycle now.
pub fn cancel_wait() -> Duration {
    let secs = std::env::var(ENV_CANCEL_WAIT_SECS)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_CANCEL_WAIT_SECS);
    Duration::from_secs(secs)
}

pub fn last_event_is_recycle(last_event: &str) -> bool {
    last_event.starts_with("recycle:")
}

pub fn last_event_is_stall(last_event: &str) -> bool {
    last_event.contains("watchdog: stall")
}

pub fn stop_reason_is_cancelled(reason: Option<&str>) -> bool {
    reason.is_some_and(|s| s.eq_ignore_ascii_case("cancelled"))
}

/// Timeout abort only for adapter Grok-bound phases (not stub / plan-review / CI / cross-model).
/// Call on the **pre-apply** run state — after apply, plan-review may have advanced to fold.
pub fn should_abort_on_timeout(record: &ProjectRecord) -> bool {
    load_run_state(record)
        .ok()
        .is_some_and(|s| should_abort_on_timeout_state(&s))
}

pub fn should_abort_on_timeout_state(state: &crate::state::RunState) -> bool {
    if state.driver != crate::workflow::WorkflowDriver::Adapter {
        return false;
    }
    if crate::workflow::graph::is_stub_phase(&state.phase) {
        return false;
    }
    if !crate::workflow::graph::is_canonical(&state.phase) {
        return false;
    }
    !matches!(
        state.phase.as_str(),
        crate::workflow::graph::PHASE_PLAN_REVIEW
            | crate::workflow::graph::PHASE_CROSS_MODEL
            | crate::workflow::graph::PHASE_CI_WAIT
    )
}

/// Refuse Ping-reuse of a mid-Prompt / stall / recycle persist.
pub fn should_refuse_reuse(record: &ProjectRecord) -> bool {
    if crate::harness::pool::persist_prompt_in_flight(record) {
        return true;
    }
    match load_run_state(record) {
        Ok(state) => {
            state.stalled_at.is_some()
                || last_event_is_stall(&state.last_event)
                || last_event_is_recycle(&state.last_event)
        }
        Err(_) => false,
    }
}

/// Fire-and-forget abort. Never returns `Err` to `poll_once`.
pub fn abort_stuck_prompt(record: &ProjectRecord, reason: AbortReason) {
    let rec = record.clone();
    let _ = std::thread::Builder::new()
        .name(format!("abort-stuck-{}", rec.id))
        .spawn(move || {
            abort_stuck_prompt_sync(&rec, reason);
        });
}

/// Block until cancel-wait + recycle finish. Used on ACP prompt-timeout
/// so persist is dead before `apply_turn` / the next `start`.
pub fn abort_stuck_prompt_sync(record: &ProjectRecord, reason: AbortReason) {
    let rec = record.clone();
    let join = std::thread::Builder::new()
        .name("abort-sync".into())
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| {
                    CoordinatorError::Message(format!("failed to start abort runtime: {e}"))
                })
                .and_then(|rt| rt.block_on(abort_stuck_prompt_async(&rec, reason)))
        });
    match join {
        Ok(handle) => match handle.join() {
            Ok(Err(e)) => tracing_or_eprint(record, &e),
            Ok(Ok(())) => {}
            Err(_) => eprintln!("coordinator abort/recycle: helper thread panicked"),
        },
        Err(e) => eprintln!("coordinator abort/recycle: failed to spawn helper: {e}"),
    }
}

fn note_aborted_session(record: &ProjectRecord) {
    let sid = crate::harness::pool::persist_session_id(record);
    let _ = with_run_state_lock(record, || {
        let mut state = load_run_state(record)?;
        if state.aborted_session_id.is_none() && sid.is_some() {
            state.aborted_session_id = sid;
            save_run_state(record, &state)?;
        }
        Ok(())
    });
}

fn tracing_or_eprint(record: &ProjectRecord, err: &CoordinatorError) {
    let _ = record;
    eprintln!("coordinator abort/recycle: {err}");
}

async fn abort_stuck_prompt_async(record: &ProjectRecord, reason: AbortReason) -> Result<()> {
    note_aborted_session(record);
    if let Some(handle) = cancel_handle_for(&record.id) {
        let _ = handle.cancel().await;
    } else {
        let _ = crate::harness::pool::holder_cancel(record).await;
    }

    // PromptTimeout already returned from inject_prompt — do not wait on ourselves.
    let wait = match reason {
        AbortReason::PromptTimeout => Duration::ZERO,
        _ => cancel_wait(),
    };
    if !wait.is_zero() {
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline {
            if !crate::harness::pool::persist_prompt_in_flight(record) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    let _ = crate::harness::pool::recycle_without_pool_lock(record).await;
    Ok(())
}

/// First stall this phase: stamp recycle, then abort. Second stall: surface only.
pub fn maybe_stamp_and_abort_stall(record: &ProjectRecord) -> Option<StatusView> {
    let stamped = with_run_state_lock(record, || {
        let mut state = load_run_state(record)?;
        if state.status != RunStatus::Running {
            return Ok(None);
        }
        if state.stall_recycles >= 1 {
            return Ok(None);
        }
        if state.stalled_at.is_none() && !last_event_is_stall(&state.last_event) {
            return Ok(None);
        }
        state.stall_recycles = state.stall_recycles.saturating_add(1);
        state.last_driven_phase = None;
        state.stalled_at = None;
        if state.aborted_session_id.is_none() {
            state.aborted_session_id = crate::harness::pool::persist_session_id(record);
        }
        state.last_event = RECYCLE_STALL_EVENT.into();
        state.updated_at = chrono::Utc::now();
        save_run_state(record, &state)?;
        Ok(Some(StatusView::from_record(record, &state)))
    })
    .ok()
    .flatten();

    if stamped.is_some() {
        abort_stuck_prompt(record, AbortReason::Stall);
    }
    stamped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_is_case_insensitive() {
        assert!(stop_reason_is_cancelled(Some("cancelled")));
        assert!(stop_reason_is_cancelled(Some("Cancelled")));
        assert!(!stop_reason_is_cancelled(Some("end_turn")));
        assert!(!stop_reason_is_cancelled(None));
    }

    #[test]
    fn recycle_prefix() {
        assert!(last_event_is_recycle(RECYCLE_STALL_EVENT));
        assert!(!last_event_is_recycle(
            "watchdog: stall — no harness progress for 12s"
        ));
    }

    #[test]
    fn timeout_abort_uses_pre_apply_phase() {
        use crate::registry::ProjectRecord;
        use tempfile::tempdir;
        use uuid::Uuid;

        let dir = tempdir().unwrap();
        let rec = ProjectRecord {
            id: Uuid::new_v4().to_string(),
            path: dir.path().to_path_buf(),
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
        crate::run::run_stub(&rec, None).unwrap();
        let stub = crate::state::load_run_state(&rec).unwrap();
        assert!(!should_abort_on_timeout_state(&stub));

        crate::run::stop(&rec).unwrap();
        crate::run::run_with_driver(
            &rec,
            Some("0027".into()),
            crate::workflow::WorkflowDriver::Adapter,
        )
        .unwrap();
        let plan = crate::state::load_run_state(&rec).unwrap();
        assert!(should_abort_on_timeout_state(&plan));

        let mut review = plan.clone();
        review.phase = crate::workflow::graph::PHASE_PLAN_REVIEW.into();
        assert!(!should_abort_on_timeout_state(&review));
        review.phase = crate::workflow::graph::PHASE_FOLD.into();
        assert!(
            should_abort_on_timeout_state(&review),
            "fold itself is Grok-bound; plan-review must be classified before apply"
        );
    }
}
