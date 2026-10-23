//! Abort a wedged ACP Prompt and recycle the holder (track **0027**).
//! First stall recycles unless sidecar `tool_in_flight` (track **0035**).
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

    if let Err(e) = crate::harness::pool::recycle_without_pool_lock(record).await {
        eprintln!("coordinator: abort recycle: project={} {e}", record.id);
    }
    Ok(())
}

/// First stall this phase: stamp recycle, then abort — unless sidecar
/// `tool_in_flight`. `session/update` text is not a skip. Second stall: surface only.
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
        if crate::workflow::watchdog::sidecar_tool_in_flight(record) {
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
        let track = state.track_id.as_deref().unwrap_or("-");
        crate::progress_log::append(
            record,
            "stall",
            &format!("track={track} phase={}  {}", state.phase, state.last_event),
        );
        Ok(Some(StatusView::from_record(record, &state)))
    })
    .ok()
    .flatten();

    if stamped.is_some() {
        if crate::workflow::watchdog::sidecar_tool_in_flight(record) {
            return revert_recycle_stamp(record);
        }
        abort_stuck_prompt(record, AbortReason::Stall);
    }
    stamped
}

/// Two stdout-close restarts, then stop (three attempts).
pub const ACP_STDOUT_RETRY_CAP: u32 = 2;

/// `note_acp_stdout_close` result. Not a `Result` — a lock error is `Fail`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StdoutCloseDisposition {
    Ignore,
    Retry,
    Fail { message: String },
}

fn stdout_message_after_cancel(msg: &str) -> bool {
    let Some(rest) = msg.strip_prefix("ACP stdout closed during ") else {
        return false;
    };
    let head = rest.split(';').next().unwrap_or(rest);
    head.ends_with(" after cancel")
}

fn child_token_from_message(msg: &str) -> &str {
    let Some(idx) = msg.find("child=") else {
        return "child=unknown";
    };
    let rest = &msg[idx..];
    let end = rest.find(';').unwrap_or(rest.len());
    let token = rest[..end].trim();
    if token.is_empty() {
        "child=unknown"
    } else {
        token
    }
}

fn flatten_progress_detail(detail: &str) -> String {
    detail.replace(['\r', '\n'], " ")
}

fn session_matches_aborted(state: &crate::state::RunState, session_id: Option<&str>) -> bool {
    match (state.aborted_session_id.as_deref(), session_id) {
        (Some(aborted), Some(sid)) => aborted == sid,
        _ => false,
    }
}

/// Dropped ACP pipe or TCP stream. Not a missing listener and not a generic OS error.
/// `ErrorKind` is non-exhaustive, so this stays `matches!`.
pub(crate) fn io_error_is_transport_drop(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::UnexpectedEof
    )
}

fn transport_recycle_event(n: u32, raw: Option<i32>, kind: std::io::ErrorKind) -> String {
    match raw {
        Some(code) => {
            format!("recycle: transport {n}/{ACP_STDOUT_RETRY_CAP} \u{2014} os={code}")
        }
        None => {
            format!("recycle: transport {n}/{ACP_STDOUT_RETRY_CAP} \u{2014} kind={kind:?}")
        }
    }
}

/// Bounded transport-drop policy. Shares [`ACP_STDOUT_RETRY_CAP`] with stdout close.
/// Does not pid-kill the holder. A lock error fails closed with the original Display.
pub fn note_transport_drop(
    record: &ProjectRecord,
    err: &CoordinatorError,
    session_id: Option<&str>,
) -> StdoutCloseDisposition {
    let CoordinatorError::Io(io) = err else {
        return StdoutCloseDisposition::Fail {
            message: err.to_string(),
        };
    };
    if !io_error_is_transport_drop(io) {
        return StdoutCloseDisposition::Fail {
            message: err.to_string(),
        };
    }
    let message = err.to_string();
    let raw = io.raw_os_error();
    let kind = io.kind();
    let fail = || StdoutCloseDisposition::Fail {
        message: message.clone(),
    };
    let updated = with_run_state_lock(record, || {
        let mut state = load_run_state(record)?;
        if state.status != RunStatus::Running {
            return Ok(StdoutCloseDisposition::Ignore);
        }
        // Argument wins. Persist covers a serve stamp that only has `session_id: None`.
        let drop_id = session_id
            .map(str::to_string)
            .or_else(|| crate::harness::pool::persist_session_id(record));
        if drop_id
            .as_deref()
            .is_some_and(|id| state.aborted_session_id.as_deref() == Some(id))
        {
            return Ok(StdoutCloseDisposition::Ignore);
        }
        if !crate::workflow::graph::is_grok_bound(&state.phase) {
            return Ok(fail());
        }
        let adapter = state.driver == crate::workflow::WorkflowDriver::Adapter;
        if state.acp_stdout_retries < ACP_STDOUT_RETRY_CAP && adapter {
            state.acp_stdout_retries = state.acp_stdout_retries.saturating_add(1);
            if state.aborted_session_id.is_none() {
                state.aborted_session_id = drop_id;
            }
            state.last_driven_phase = None;
            state.stalled_at = None;
            let n = state.acp_stdout_retries;
            state.last_event = transport_recycle_event(n, raw, kind);
            state.updated_at = chrono::Utc::now();
            save_run_state(record, &state)?;
            crate::progress_log::append(record, "transport", &flatten_progress_detail(&message));
            return Ok(StdoutCloseDisposition::Retry);
        }
        Ok(fail())
    });
    match updated {
        Ok(disposition) => disposition,
        Err(_) => fail(),
    }
}

/// Bounded ACP stdout-close policy. State update holds `with_run_state_lock`.
/// A lock error fails closed with the original diagnosis.
pub fn note_acp_stdout_close(
    record: &ProjectRecord,
    msg: &str,
    session_id: Option<&str>,
) -> StdoutCloseDisposition {
    let fail = || StdoutCloseDisposition::Fail {
        message: msg.to_string(),
    };
    if !msg.starts_with("ACP stdout closed during ") {
        return fail();
    }
    let updated = with_run_state_lock(record, || {
        let mut state = load_run_state(record)?;
        if stdout_message_after_cancel(msg)
            || session_matches_aborted(&state, session_id)
            || state.status != RunStatus::Running
        {
            return Ok(StdoutCloseDisposition::Ignore);
        }
        if !crate::workflow::graph::is_grok_bound(&state.phase) {
            return Ok(fail());
        }
        let adapter = state.driver == crate::workflow::WorkflowDriver::Adapter;
        if state.acp_stdout_retries < ACP_STDOUT_RETRY_CAP && adapter {
            state.acp_stdout_retries = state.acp_stdout_retries.saturating_add(1);
            if let Some(sid) = session_id {
                state.aborted_session_id = Some(sid.to_string());
            }
            state.last_driven_phase = None;
            state.stalled_at = None;
            let n = state.acp_stdout_retries;
            let child = child_token_from_message(msg);
            state.last_event = format!("recycle: acp-stdout {n}/{ACP_STDOUT_RETRY_CAP} — {child}");
            state.updated_at = chrono::Utc::now();
            save_run_state(record, &state)?;
            crate::progress_log::append(record, "acp-stdout", &flatten_progress_detail(msg));
            return Ok(StdoutCloseDisposition::Retry);
        }
        Ok(fail())
    });
    match updated {
        Ok(disposition) => disposition,
        Err(_) => fail(),
    }
}

/// Stamp raced a live tool: drop CAP, surface stall, do not abort.
fn revert_recycle_stamp(record: &ProjectRecord) -> Option<StatusView> {
    with_run_state_lock(record, || {
        let mut state = load_run_state(record)?;
        state.stall_recycles = 0;
        state.aborted_session_id = None;
        state.last_driven_phase = Some(state.phase.clone());
        if state.stalled_at.is_none() {
            state.stalled_at = Some(chrono::Utc::now());
        }
        state.last_event = "watchdog: stall".into();
        state.updated_at = chrono::Utc::now();
        save_run_state(record, &state)?;
        Ok(Some(StatusView::from_record(record, &state)))
    })
    .ok()
    .flatten()
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

    #[test]
    fn stdout_retry_clears_stall_and_flattens_progress() {
        use crate::registry::ProjectRecord;
        use crate::state::{RunStatus, load_run_state, save_run_state};
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
        crate::run::run_with_driver(
            &rec,
            Some("0064".into()),
            crate::workflow::WorkflowDriver::Adapter,
        )
        .unwrap();
        {
            let mut state = load_run_state(&rec).unwrap();
            state.phase = crate::workflow::graph::PHASE_IMPLEMENT.into();
            state.stalled_at = Some(chrono::Utc::now());
            state.stall_recycles = 1;
            state.last_driven_phase = Some("implement".into());
            save_run_state(&rec, &state).unwrap();
        }
        let msg =
            "ACP stdout closed during session/prompt; child=exited:42; stderr=line1\nline2\rline3";
        let disposition = note_acp_stdout_close(&rec, msg, Some("sess-stall"));
        assert_eq!(disposition, StdoutCloseDisposition::Retry);
        let state = load_run_state(&rec).unwrap();
        assert_eq!(state.status, RunStatus::Running);
        assert!(state.stalled_at.is_none());
        assert_eq!(state.stall_recycles, 1);
        assert!(state.last_driven_phase.is_none());
        assert_eq!(state.aborted_session_id.as_deref(), Some("sess-stall"));
        assert!(
            state.last_event.starts_with("recycle: acp-stdout 1/2"),
            "{}",
            state.last_event
        );
        assert!(
            state.last_event.contains('\u{2014}'),
            "{}",
            state.last_event
        );
        assert!(
            state.last_event.contains("child=exited:42"),
            "{}",
            state.last_event
        );
        let log = std::fs::read_to_string(crate::progress_log::path(&rec)).unwrap();
        assert!(log.contains("line1 line2 line3"), "{log}");
        assert!(!log.contains("line1\nline2"), "{log}");
        assert_eq!(state.acp_stdout_retries, 1);
    }

    fn transport_err(kind: std::io::ErrorKind, message: &str) -> CoordinatorError {
        CoordinatorError::Io(std::io::Error::new(kind, message))
    }

    fn adapter_implement(dir: &std::path::Path) -> ProjectRecord {
        let rec = ProjectRecord {
            id: uuid::Uuid::new_v4().to_string(),
            path: dir.to_path_buf(),
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
        crate::run::run_with_driver(
            &rec,
            Some("0091".into()),
            crate::workflow::WorkflowDriver::Adapter,
        )
        .unwrap();
        {
            let mut state = load_run_state(&rec).unwrap();
            state.phase = crate::workflow::graph::PHASE_IMPLEMENT.into();
            state.stalled_at = Some(chrono::Utc::now());
            state.last_driven_phase = Some("implement".into());
            save_run_state(&rec, &state).unwrap();
        }
        rec
    }

    #[test]
    fn transport_kinds_match_the_drop_set() {
        for kind in [
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::ConnectionAborted,
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::UnexpectedEof,
        ] {
            let err = std::io::Error::new(kind, "x");
            assert!(io_error_is_transport_drop(&err), "{kind:?}");
        }
        for kind in [
            std::io::ErrorKind::ConnectionRefused,
            std::io::ErrorKind::NotFound,
        ] {
            let err = std::io::Error::new(kind, "x");
            assert!(!io_error_is_transport_drop(&err), "{kind:?}");
        }
        #[cfg(windows)]
        {
            let raw64 = std::io::Error::from_raw_os_error(64);
            assert!(!io_error_is_transport_drop(&raw64), "{:?}", raw64.kind());
            let refused = std::io::Error::from_raw_os_error(10061);
            assert!(
                !io_error_is_transport_drop(&refused),
                "{:?}",
                refused.kind()
            );
        }
    }

    #[test]
    fn transport_reset_retries_and_names_kind() {
        let dir = tempfile::tempdir().unwrap();
        let rec = adapter_implement(dir.path());
        let err = transport_err(std::io::ErrorKind::ConnectionReset, "line1\nline2\rline3");
        let disposition = note_transport_drop(&rec, &err, Some("sess-transport-1"));
        assert_eq!(disposition, StdoutCloseDisposition::Retry);
        let state = load_run_state(&rec).unwrap();
        assert_eq!(state.status, RunStatus::Running);
        assert_eq!(state.acp_stdout_retries, 1);
        assert!(state.stalled_at.is_none());
        assert!(state.last_driven_phase.is_none());
        assert_eq!(
            state.aborted_session_id.as_deref(),
            Some("sess-transport-1")
        );
        assert!(
            state
                .last_event
                .starts_with("recycle: transport 1/2 \u{2014}"),
            "{}",
            state.last_event
        );
        assert!(
            state.last_event.contains("kind=ConnectionReset"),
            "{}",
            state.last_event
        );
        assert!(!state.last_event.contains("os="), "{}", state.last_event);
        let log = std::fs::read_to_string(crate::progress_log::path(&rec)).unwrap();
        assert!(log.contains("  transport  "), "{log}");
        assert!(log.contains("line1 line2 line3"), "{log}");
        assert!(!log.contains("line1\nline2"), "{log}");

        let again = note_transport_drop(&rec, &err, Some("sess-transport-1"));
        assert_eq!(again, StdoutCloseDisposition::Ignore);
        assert_eq!(load_run_state(&rec).unwrap().acp_stdout_retries, 1);

        let second = note_transport_drop(
            &rec,
            &transport_err(std::io::ErrorKind::BrokenPipe, "pipe"),
            Some("sess-transport-2"),
        );
        assert_eq!(second, StdoutCloseDisposition::Retry);
        assert_eq!(load_run_state(&rec).unwrap().acp_stdout_retries, 2);

        let third = note_transport_drop(
            &rec,
            &transport_err(std::io::ErrorKind::UnexpectedEof, "eof"),
            Some("sess-transport-3"),
        );
        assert!(matches!(third, StdoutCloseDisposition::Fail { .. }));
        let stopped_clock = load_run_state(&rec).unwrap();
        assert_eq!(stopped_clock.acp_stdout_retries, 2);
        assert_eq!(stopped_clock.status, RunStatus::Running);
        assert!(
            stopped_clock
                .last_event
                .starts_with("recycle: transport 2/2"),
            "{}",
            stopped_clock.last_event
        );
    }

    #[cfg(windows)]
    #[test]
    fn transport_raw_10054_names_os_code() {
        let dir = tempfile::tempdir().unwrap();
        let rec = adapter_implement(dir.path());
        let err = CoordinatorError::Io(std::io::Error::from_raw_os_error(10054));
        let disposition = note_transport_drop(&rec, &err, Some("sess-10054"));
        assert_eq!(disposition, StdoutCloseDisposition::Retry);
        let state = load_run_state(&rec).unwrap();
        assert!(
            state.last_event.contains("os=10054"),
            "{}",
            state.last_event
        );
        assert!(
            state
                .last_event
                .starts_with("recycle: transport 1/2 \u{2014}"),
            "{}",
            state.last_event
        );
        assert_eq!(state.aborted_session_id.as_deref(), Some("sess-10054"));
        assert!(state.stalled_at.is_none());
        assert!(state.last_driven_phase.is_none());
    }

    #[test]
    fn transport_not_found_and_message_do_not_touch_the_counter() {
        let dir = tempfile::tempdir().unwrap();
        let rec = adapter_implement(dir.path());
        let missing = note_transport_drop(
            &rec,
            &transport_err(std::io::ErrorKind::NotFound, "no such file"),
            Some("sess-missing"),
        );
        assert!(matches!(missing, StdoutCloseDisposition::Fail { .. }));
        let refused = note_transport_drop(
            &rec,
            &transport_err(std::io::ErrorKind::ConnectionRefused, "refused"),
            Some("sess-refused"),
        );
        assert!(matches!(refused, StdoutCloseDisposition::Fail { .. }));
        let message = note_transport_drop(
            &rec,
            &CoordinatorError::Message("holder connect 127.0.0.1:1: refused".into()),
            Some("sess-msg"),
        );
        assert!(matches!(message, StdoutCloseDisposition::Fail { .. }));
        let state = load_run_state(&rec).unwrap();
        assert_eq!(state.acp_stdout_retries, 0);
        assert!(state.aborted_session_id.is_none());
        assert!(state.stalled_at.is_some());
        assert_eq!(state.last_driven_phase.as_deref(), Some("implement"));
    }

    #[test]
    fn transport_none_session_ignores_when_persist_id_is_already_aborted() {
        let dir = tempfile::tempdir().unwrap();
        let rec = adapter_implement(dir.path());
        let err = transport_err(std::io::ErrorKind::ConnectionAborted, "abort");
        assert_eq!(
            note_transport_drop(&rec, &err, Some("sess-X")),
            StdoutCloseDisposition::Retry
        );
        let path = crate::harness::pool::persist_path(&rec).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"version":1,"project_id":"p","adapter":"grok","supports_compact":true,"alive":true,"prompt_in_flight":false,"session_id":"sess-X"}"#,
        )
        .unwrap();
        assert_eq!(
            note_transport_drop(&rec, &err, None),
            StdoutCloseDisposition::Ignore
        );
        assert_eq!(load_run_state(&rec).unwrap().acp_stdout_retries, 1);
    }

    #[test]
    fn transport_empty_ids_increment_once_and_leave_aborted_empty() {
        let dir = tempfile::tempdir().unwrap();
        let rec = adapter_implement(dir.path());
        let err = transport_err(std::io::ErrorKind::ConnectionReset, "reset");
        assert_eq!(
            note_transport_drop(&rec, &err, None),
            StdoutCloseDisposition::Retry
        );
        let state = load_run_state(&rec).unwrap();
        assert!(state.aborted_session_id.is_none());
        assert_eq!(state.acp_stdout_retries, 1);
    }

    #[test]
    fn transport_stopped_does_not_consume_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let rec = adapter_implement(dir.path());
        {
            let mut state = load_run_state(&rec).unwrap();
            state.status = RunStatus::Stopped;
            save_run_state(&rec, &state).unwrap();
        }
        let err = transport_err(std::io::ErrorKind::ConnectionReset, "reset");
        assert_eq!(
            note_transport_drop(&rec, &err, Some("sess-stopped")),
            StdoutCloseDisposition::Ignore
        );
        assert_eq!(load_run_state(&rec).unwrap().acp_stdout_retries, 0);
    }
}
