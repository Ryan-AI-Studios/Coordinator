//! Drivers: adapter / file_wait / stub / skip. `tick` is the single drive entry.

use crate::error::{CoordinatorError, Result};
use crate::outcome::{
    FailureClass, OutcomeSource, OutcomeStatus, PhaseOutcome, outcome_roles_dir, write_and_apply,
};
use crate::registry::ProjectRecord;
use crate::state::{RunState, RunStatus, load_run_state, save_run_state, with_run_state_lock};

use super::bundle::{self, review_file, reviews_dir};
use super::graph::{
    PHASE_COMPACT, PHASE_PLAN_REVIEW, is_canonical, is_recognized_role, resolve_track_dir,
    review_slugs, role_phase,
};
use super::prompts;
use super::{WorkflowDriver, mark_driven};

/// Idempotent drive step. No-op when Stopped / Idle / leftover stub.
///
/// `cross-model-review` and `ci-wait` still tick while **Paused** (finish current phase).
pub fn tick(record: &ProjectRecord) -> Result<Option<crate::state::StatusView>> {
    let state = load_run_state(record)?;
    let tick_while_paused = state.status == RunStatus::Paused
        && (state.phase == super::graph::PHASE_CROSS_MODEL
            || state.phase == super::graph::PHASE_CI_WAIT);
    if state.status != RunStatus::Running && !tick_while_paused {
        return Ok(None);
    }
    if !is_canonical(&state.phase) {
        return Ok(None);
    }

    if state.phase == super::graph::PHASE_CROSS_MODEL {
        return crate::review::drive(record, &state);
    }
    if state.phase == super::graph::PHASE_CI_WAIT {
        return crate::ci::drive(record, &state);
    }
    if state.phase == PHASE_COMPACT {
        return drive_compact(record, &state);
    }
    if state.phase == PHASE_PLAN_REVIEW {
        return drive_plan_review(record, &state);
    }

    match state.driver {
        WorkflowDriver::Stub => synth_success(record, &state, None, OutcomeSource::Test),
        WorkflowDriver::FileWait => Ok(None),
        WorkflowDriver::Adapter => drive_adapter(record, &state),
    }
}

fn synth_success(
    record: &ProjectRecord,
    state: &RunState,
    message: Option<String>,
    source: OutcomeSource,
) -> Result<Option<crate::state::StatusView>> {
    let outcome = PhaseOutcome::success(
        state.phase.clone(),
        source,
        message,
        None,
        Some(state.run_epoch),
    );
    write_and_apply(record, outcome).map(Some)
}

fn fail_phase(
    record: &ProjectRecord,
    state: &RunState,
    class: FailureClass,
    message: String,
    source: OutcomeSource,
) -> Result<Option<crate::state::StatusView>> {
    let outcome = PhaseOutcome::failure(
        state.phase.clone(),
        class,
        source,
        Some(message),
        Some(state.run_epoch),
    );
    write_and_apply(record, outcome).map(Some)
}

fn drive_compact(
    record: &ProjectRecord,
    state: &RunState,
) -> Result<Option<crate::state::StatusView>> {
    let msg = compact_message(record, state.driver);
    synth_success(record, state, Some(msg), OutcomeSource::Test)
}

const COMPACT_REASON_CAP: usize = 200;

fn compact_message(record: &ProjectRecord, driver: WorkflowDriver) -> String {
    if driver != WorkflowDriver::Adapter {
        return "compact: skipped".into();
    }
    match try_compact(record) {
        CompactAttempt::Ok => "compact: ok".into(),
        CompactAttempt::Skipped(None) => "compact: skipped".into(),
        CompactAttempt::Skipped(Some(reason)) => {
            format!("compact: skipped — {}", truncate_reason(&reason))
        }
    }
}

enum CompactAttempt {
    Ok,
    Skipped(Option<String>),
}

fn try_compact(record: &ProjectRecord) -> CompactAttempt {
    let selector = record.path.to_string_lossy().to_string();
    match block_on_async(crate::harness::compact(Some(&selector))) {
        Ok(view) => {
            if let Some(err) = view.error {
                CompactAttempt::Skipped(Some(err))
            } else if view.skipped == Some(true) {
                CompactAttempt::Skipped(None)
            } else {
                CompactAttempt::Ok
            }
        }
        Err(e) => CompactAttempt::Skipped(Some(e.to_string())),
    }
}

fn truncate_reason(msg: &str) -> String {
    if msg.chars().count() <= COMPACT_REASON_CAP {
        return msg.to_string();
    }
    let t: String = msg.chars().take(COMPACT_REASON_CAP).collect();
    format!("{t}…")
}

/// Adapter ticks from `wait` / `serve` always use the detached holder (`in_process: false`).
/// In-process spawn stays for same-process tests / `insert_test_session` / HTTP start.
const ADAPTER_START_IN_PROCESS: bool = false;

#[cfg(test)]
static TEST_SLOW_INJECT: std::sync::Mutex<Option<std::time::Duration>> =
    std::sync::Mutex::new(None);

/// Arm a one-shot mock inject that does not block `poll_once` (DoD-1).
/// Disarm on drop so a panic cannot leak the hook into another test.
#[cfg(test)]
pub(crate) fn arm_slow_adapter_inject(delay: std::time::Duration) -> SlowInjectGuard {
    *TEST_SLOW_INJECT.lock().unwrap_or_else(|p| p.into_inner()) = Some(delay);
    SlowInjectGuard
}

#[cfg(test)]
pub(crate) struct SlowInjectGuard;

#[cfg(test)]
impl Drop for SlowInjectGuard {
    fn drop(&mut self) {
        *TEST_SLOW_INJECT.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

fn drive_adapter(
    record: &ProjectRecord,
    state: &RunState,
) -> Result<Option<crate::state::StatusView>> {
    if state.last_driven_phase.as_deref() == Some(state.phase.as_str()) {
        return Ok(None);
    }
    mark_driven(record, &state.phase)?;
    // Inject-start heartbeat so a slow ACP start is not an immediate stall.
    let inject_sid = crate::harness::status_bundle_sync(record)
        .and_then(|b| b.grok)
        .and_then(|g| g.session_id);
    crate::workflow::watchdog::note_progress(
        record,
        crate::workflow::watchdog::ProgressKind::Inject,
        inject_sid.as_deref(),
    );

    #[cfg(test)]
    {
        let delay = TEST_SLOW_INJECT
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(delay) = delay {
            // Simulate a long in-flight inject without holding the global pool lock.
            std::thread::spawn(move || std::thread::sleep(delay));
            return Ok(None);
        }
    }

    let has_live_session = crate::harness::status_bundle_sync(record)
        .and_then(|b| b.grok)
        .is_some_and(|g| g.alive);
    if !has_live_session && let Err(e) = crate::harness::resolve_grok_binary() {
        return fail_phase(
            record,
            state,
            FailureClass::Permission,
            format!("grok not resolvable: {e}"),
            OutcomeSource::Adapter,
        );
    }

    let prompt = prompts::phase_prompt(record, &state.phase, state.track_id.as_deref());
    let selector = record.path.to_string_lossy().to_string();
    let rec = record.clone();
    // First tick starts the holder Prompt without blocking poll_once. Later ticks
    // are no-ops (`mark_driven`). The holder / pool apply path writes the outcome.
    std::thread::Builder::new()
        .name(format!("adapter-inject-{}", rec.id))
        .spawn(move || {
            let result = block_on_async(async {
                crate::harness::start(Some(&selector), ADAPTER_START_IN_PROCESS).await?;
                crate::harness::prompt(Some(&selector), prompt).await
            });
            match result {
                Ok(view) => {
                    if view.applied || view.skipped == Some(true) {
                        return;
                    }
                    if let Some(err) = view.error {
                        let class = view.failure_class.unwrap_or(FailureClass::HarnessCrash);
                        apply_adapter_failure(&rec, class, err);
                    }
                }
                Err(e) => apply_adapter_failure(&rec, FailureClass::HarnessCrash, e.to_string()),
            }
        })
        .map_err(|e| {
            CoordinatorError::Message(format!("failed to spawn adapter inject thread: {e}"))
        })?;
    Ok(None)
}

fn apply_adapter_failure(record: &ProjectRecord, class: FailureClass, message: String) {
    let Ok(state) = load_run_state(record) else {
        return;
    };
    if state.status != RunStatus::Running {
        return;
    }
    if crate::harness::abort::last_event_is_recycle(&state.last_event) {
        return;
    }
    let _ = fail_phase(record, &state, class, message, OutcomeSource::Adapter);
}

fn drive_plan_review(
    record: &ProjectRecord,
    state: &RunState,
) -> Result<Option<crate::state::StatusView>> {
    ensure_plan_review_pending(record, state)?;
    if state.driver == WorkflowDriver::Stub {
        write_stub_reviews(record, state)?;
    }
    consume_role_files(record)?;
    try_join(record)
}

fn ensure_plan_review_pending(record: &ProjectRecord, state: &RunState) -> Result<()> {
    if !state.pending_roles.is_empty() {
        return Ok(());
    }
    with_run_state_lock(record, || {
        let mut s = load_run_state(record)?;
        if s.phase == PHASE_PLAN_REVIEW && s.pending_roles.is_empty() {
            s.pending_roles = review_slugs().iter().map(|x| (*x).to_string()).collect();
            s.updated_at = chrono::Utc::now();
            save_run_state(record, &s)?;
        }
        Ok(())
    })
}

fn write_stub_reviews(record: &ProjectRecord, state: &RunState) -> Result<()> {
    let roles_dir = outcome_roles_dir(record)?;
    std::fs::create_dir_all(&roles_dir)?;
    std::fs::create_dir_all(reviews_dir(record)?)?;
    for slug in review_slugs() {
        write_review_markdown(record, slug, Some(&format!("stub review ({slug})\n")))?;
        let mut outcome = PhaseOutcome::success(
            role_phase(slug),
            OutcomeSource::Test,
            None,
            None,
            Some(state.run_epoch),
        );
        outcome.metadata = Some(crate::outcome::OutcomeMetadata {
            next_track: None,
            role: Some(match *slug {
                "agy" => super::graph::ROLE_REVIEWER_AGY.into(),
                _ => super::graph::ROLE_REVIEWER_OPENCODE.into(),
            }),
            ..Default::default()
        });
        crate::persist::atomic_write_json(&roles_dir.join(format!("{slug}.json")), &outcome)?;
    }
    Ok(())
}

pub fn write_review_markdown(
    record: &ProjectRecord,
    slug: &str,
    body: Option<&str>,
) -> Result<std::path::PathBuf> {
    let dest = review_file(record, slug)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = bundle::normalize_newlines(body.unwrap_or(""));
    crate::persist::atomic_write(&dest, text.as_bytes())?;
    if let Ok(state) = load_run_state(record)
        && let Some(ref track_id) = state.track_id
        && let Some(track_dir) = resolve_track_dir(record, track_id)
    {
        let copy = track_dir.join(format!("{slug}-review.md"));
        crate::persist::atomic_write(&copy, text.as_bytes())?;
    }
    Ok(dest)
}

fn consume_role_files(record: &ProjectRecord) -> Result<()> {
    let state = load_run_state(record)?;
    if state.phase != PHASE_PLAN_REVIEW {
        return Ok(());
    }
    let roles_dir = outcome_roles_dir(record)?;
    for slug in state.pending_roles.clone() {
        let path = roles_dir.join(format!("{slug}.json"));
        if !path.exists() {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let outcome: PhaseOutcome = match serde_json::from_str(&text) {
            Ok(o) => o,
            Err(_) => continue,
        };
        if outcome.phase != role_phase(&slug) {
            continue;
        }
        if let Some(ref meta) = outcome.metadata
            && let Some(ref role) = meta.role
            && !is_recognized_role(role)
        {
            continue;
        }
        if outcome.status == OutcomeStatus::Success {
            adopt_track_review_if_missing(record, &slug)?;
        }
        let _ = std::fs::remove_file(&path);
        remove_pending(record, &slug)?;
    }
    Ok(())
}

fn remove_pending(record: &ProjectRecord, slug: &str) -> Result<()> {
    with_run_state_lock(record, || {
        let mut s = load_run_state(record)?;
        s.pending_roles.retain(|x| x != slug);
        s.updated_at = chrono::Utc::now();
        save_run_state(record, &s)
    })
}

fn review_produced(record: &ProjectRecord, slug: &str) -> bool {
    review_file(record, slug).ok().is_some_and(|p| {
        p.is_file() && std::fs::read_to_string(p).is_ok_and(|t| !t.trim().is_empty())
    })
}

fn adopt_track_review_if_missing(record: &ProjectRecord, slug: &str) -> Result<()> {
    let dest = review_file(record, slug)?;
    if dest.is_file() {
        return Ok(());
    }
    let Ok(state) = load_run_state(record) else {
        return Ok(());
    };
    let Some(ref track_id) = state.track_id else {
        return Ok(());
    };
    let Some(track_dir) = resolve_track_dir(record, track_id) else {
        return Ok(());
    };
    let src = track_dir.join(format!("{slug}-review.md"));
    if src.is_file() {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = std::fs::read_to_string(&src)?;
        crate::persist::atomic_write(&dest, bundle::normalize_newlines(&text).as_bytes())?;
    }
    Ok(())
}

/// Drop leftover role/review files so a fresh `run` cannot join stale slots.
pub fn clear_plan_review_artifacts(record: &ProjectRecord) {
    if let Ok(dir) = outcome_roles_dir(record)
        && dir.exists()
    {
        let _ = std::fs::remove_dir_all(&dir);
    }
    if let Ok(dir) = reviews_dir(record)
        && dir.exists()
    {
        let _ = std::fs::remove_dir_all(&dir);
    }
}

fn try_join(record: &ProjectRecord) -> Result<Option<crate::state::StatusView>> {
    let state = load_run_state(record)?;
    if state.phase != PHASE_PLAN_REVIEW {
        return Ok(None);
    }
    if !state.pending_roles.is_empty() {
        return Ok(None);
    }
    let produced: Vec<&'static str> = review_slugs()
        .iter()
        .copied()
        .filter(|slug| review_produced(record, slug))
        .collect();
    if produced.is_empty() {
        return fail_phase(
            record,
            &state,
            FailureClass::HarnessCrash,
            "plan-review: zero reviewers produced output".into(),
            OutcomeSource::Test,
        );
    }
    bundle::assemble(record, &state)?;
    let missing: Vec<&'static str> = review_slugs()
        .iter()
        .copied()
        .filter(|slug| !review_produced(record, slug))
        .collect();
    let msg = if missing.is_empty() {
        None
    } else {
        Some(format!("plan-review: degraded {}", missing.join(",")))
    };
    synth_success(record, &state, msg, OutcomeSource::Test)
}

/// Join-timeout helper (caller already holds apply + run-state locks).
///
/// Returns a parent success outcome when one reviewer finished and the leftover
/// slot can be degraded. Does not apply (avoids lock re-entry).
pub fn timeout_plan_review_outcome(
    record: &ProjectRecord,
    state: &RunState,
) -> Result<Option<PhaseOutcome>> {
    if state.pending_roles.len() != 1 {
        return Ok(None);
    }
    let leftover = state.pending_roles[0].clone();
    let other_done = review_slugs()
        .iter()
        .any(|slug| *slug != leftover.as_str() && review_produced(record, slug));
    if !other_done {
        return Ok(None);
    }
    let mut cleared = state.clone();
    cleared.pending_roles.clear();
    save_run_state(record, &cleared)?;
    bundle::assemble(record, &cleared)?;
    Ok(Some(PhaseOutcome::success(
        state.phase.clone(),
        OutcomeSource::Timeout,
        Some(format!("plan-review: degraded {leftover}")),
        None,
        Some(state.run_epoch),
    )))
}

fn block_on_async<T, E>(
    fut: impl std::future::Future<Output = std::result::Result<T, E>>,
) -> std::result::Result<T, E>
where
    E: From<CoordinatorError>,
{
    match tokio::runtime::Handle::try_current() {
        Ok(h) => tokio::task::block_in_place(|| h.block_on(fut)),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| {
                E::from(CoordinatorError::Message(format!(
                    "failed to start async runtime: {e}"
                )))
            })?
            .block_on(fut),
    }
}
