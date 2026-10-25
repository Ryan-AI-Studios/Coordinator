//! Drivers: adapter / file_wait / stub / skip. `tick` is the single drive entry.

use crate::error::{CoordinatorError, Result};
use crate::harness::roles::{resolve_phase_binary, resolve_phase_binding, resolve_phase_role_key};
use crate::outcome::{
    FailureClass, OutcomeSource, OutcomeStatus, PhaseOutcome, outcome_roles_dir, write_and_apply,
};
use crate::registry::ProjectRecord;
use crate::state::{
    PlanReviewEntry, RunState, RunStatus, load_run_state, save_run_state, with_run_state_lock,
};

use super::bundle::{self, review_file, reviews_dir};
use super::graph::{
    PHASE_COMPACT, PHASE_PLAN, PHASE_PLAN_REVIEW, is_canonical, is_recognized_role,
    resolve_track_dir, review_slugs, role_phase,
};
use super::prompts;
use super::{WorkflowDriver, mark_driven_with_target};

/// Idempotent drive step. No-op when Stopped / Idle / leftover stub.
///
/// `cross-model-review` and `ci-wait` still tick while **Paused** (finish current phase).
pub fn tick(record: &ProjectRecord) -> Result<Option<crate::state::StatusView>> {
    if let Some(waiting) = shared_tree_wait(record)? {
        return Ok(Some(waiting));
    }
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
        WorkflowDriver::Stub => {
            stub_epoch_write(record, &state)?;
            synth_success(record, &state, None, OutcomeSource::Test)
        }
        WorkflowDriver::FileWait => Ok(None),
        WorkflowDriver::Adapter => drive_adapter(record, &state),
    }
}

fn shared_tree_phase(phase: &str) -> bool {
    matches!(
        phase,
        super::graph::PHASE_PLAN
            | super::graph::PHASE_FOLD
            | super::graph::PHASE_ADVANCE
            | PHASE_PLAN_REVIEW
    )
}

/// First check in [`tick`]. The lowest epoch is only the empty-holder tie-break.
fn shared_tree_wait(record: &ProjectRecord) -> Result<Option<crate::state::StatusView>> {
    let crate::state::RunBook::Many(envelope) = crate::state::load_run_book(record)? else {
        return Ok(None);
    };
    let Some(epoch) = crate::state::active_run_epoch() else {
        return Ok(None);
    };
    let Some(entry) = envelope.runs.iter().find(|entry| entry.run_epoch == epoch) else {
        return Ok(None);
    };
    let in_set = shared_tree_phase(&entry.phase);
    let holder = envelope.shared_tree_holder;
    if !in_set {
        if holder == Some(epoch) {
            let _ = with_run_state_lock(record, || {
                crate::state::set_shared_tree_holder(record, None)
            });
        }
        return Ok(None);
    }
    if holder == Some(epoch) {
        return Ok(None);
    }
    if let Some(held) = holder
        && envelope.runs.iter().any(|other| {
            other.run_epoch == held && other.run_epoch != epoch && shared_tree_phase(&other.phase)
        })
    {
        return wait_for_holder(record, held);
    }
    let lowest = envelope
        .runs
        .iter()
        .filter(|other| {
            shared_tree_phase(&other.phase)
                && matches!(other.status, RunStatus::Running | RunStatus::Paused)
        })
        .map(|other| other.run_epoch)
        .min();
    if let Some(lowest) = lowest {
        if holder != Some(lowest) {
            let _ = with_run_state_lock(record, || {
                crate::state::set_shared_tree_holder(record, Some(lowest))
            });
        }
        if lowest != epoch {
            return wait_for_holder(record, lowest);
        }
    }
    Ok(None)
}

fn wait_for_holder(record: &ProjectRecord, held: u64) -> Result<Option<crate::state::StatusView>> {
    with_run_state_lock(record, || {
        let mut state = load_run_state(record)?;
        state.last_event = format!("wait: shared tree held by epoch {held}");
        state.updated_at = chrono::Utc::now();
        save_run_state(record, &state)?;
        Ok(Some(crate::state::StatusView::from_record(record, &state)))
    })
}

/// Stub phases write one product file into the active epoch worktree.
/// Flag off and a missing epoch directory leave the shared checkout untouched.
fn stub_epoch_write(record: &ProjectRecord, state: &RunState) -> Result<()> {
    if !crate::worktree::isolation_enabled(record).unwrap_or(false) {
        return Ok(());
    }
    let Some(dir) = crate::worktree::active_epoch_dir(record) else {
        return Ok(());
    };
    let body = format!("epoch={} phase={}\n", state.run_epoch, state.phase);
    std::fs::write(dir.join("epoch-write.txt"), body)
        .map_err(|e| CoordinatorError::Message(format!("stub epoch write: {e}")))
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
    match block_on_async(crate::harness::compact(Some(&selector), false)) {
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
/// In-process spawn stays for same-process tests / `insert_test_session`.
const ADAPTER_START_IN_PROCESS: bool = false;

#[cfg(test)]
static TEST_SLOW_INJECT: std::sync::Mutex<Option<std::time::Duration>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
thread_local! {
    /// Per-test capture slot. A process-global mutex let one guard drop clear another
    /// test's armed slot (0101 §3.7); prompt capture happens synchronously on the
    /// arming thread before any adapter thread spawns.
    static TEST_CAPTURE_PROMPTS: std::cell::RefCell<Option<Vec<String>>> =
        const { std::cell::RefCell::new(None) };
    static TEST_CAPTURE_PREPARED: std::cell::RefCell<Option<Vec<crate::workflow::execution_repo::PreparedInjection>>> =
        const { std::cell::RefCell::new(None) };
}

/// Arm a one-shot capture of adapter prompts. No harness process is started.
#[cfg(test)]
pub(crate) struct CapturePromptGuard;

#[cfg(test)]
pub(crate) fn arm_capture_prompts() -> CapturePromptGuard {
    TEST_CAPTURE_PROMPTS.with(|c| *c.borrow_mut() = Some(Vec::new()));
    TEST_CAPTURE_PREPARED.with(|c| *c.borrow_mut() = Some(Vec::new()));
    CapturePromptGuard
}

#[cfg(test)]
pub(crate) fn captured_prompts() -> Vec<String> {
    TEST_CAPTURE_PROMPTS.with(|c| c.borrow().clone().unwrap_or_default())
}

/// Prepared injections captured by the same seam, proving the route and its ACP root.
#[cfg(test)]
pub(crate) fn captured_prepared() -> Vec<crate::workflow::execution_repo::PreparedInjection> {
    TEST_CAPTURE_PREPARED.with(|c| c.borrow().clone().unwrap_or_default())
}

#[cfg(test)]
impl Drop for CapturePromptGuard {
    fn drop(&mut self) {
        TEST_CAPTURE_PROMPTS.with(|c| *c.borrow_mut() = None);
        TEST_CAPTURE_PREPARED.with(|c| *c.borrow_mut() = None);
    }
}

#[cfg(test)]
fn capture_slot_armed() -> bool {
    TEST_CAPTURE_PROMPTS.with(|c| c.borrow().is_some())
}

#[cfg(test)]
fn push_captured_prompt(prompt: &str) {
    TEST_CAPTURE_PROMPTS.with(|c| {
        if let Some(buf) = c.borrow_mut().as_mut() {
            buf.push(prompt.to_string());
        }
    });
}

#[cfg(test)]
fn push_captured_prepared(prepared: Option<&crate::workflow::execution_repo::PreparedInjection>) {
    if let Some(prepared) = prepared {
        TEST_CAPTURE_PREPARED.with(|c| {
            if let Some(buf) = c.borrow_mut().as_mut() {
                buf.push(prepared.clone());
            }
        });
    }
}

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

/// First adapter inject of `plan`, when spec and plan are already files.
/// Stub and file_wait never call this. A later tick returns before it.
fn refresh_plan_evidence_stamp(record: &ProjectRecord, state: &RunState) -> Result<()> {
    if state.phase != PHASE_PLAN {
        return Ok(());
    }
    let Some(track_id) = state.track_id.as_deref() else {
        return Ok(());
    };
    let Some(dir) = resolve_track_dir(record, track_id) else {
        return Ok(());
    };
    if !dir.join("spec.md").is_file() || !dir.join("plan.md").is_file() {
        return Ok(());
    }
    let utc = super::evidence_stamp::utc_stamp_line(chrono::Utc::now());
    super::evidence_stamp::refresh_evidence_file(
        &dir.join("evidence.md"),
        &super::evidence_stamp::version_line(),
        &utc,
    )
}

/// 0110: the live holder identity is the binding (adapter + model + resolved bin), not just
/// the adapter. A mismatch recycles through the existing `recycle_without_pool_lock` path.
fn holder_mismatch(
    live: &crate::harness::GrokHarnessStatus,
    binding: &crate::config::RoleBinding,
    bin: &std::path::Path,
) -> bool {
    live.alive && !crate::harness::pool::identity_matches(live, binding, Some(bin))
}

fn drive_adapter(
    record: &ProjectRecord,
    state: &RunState,
) -> Result<Option<crate::state::StatusView>> {
    let continuing = super::self_check::continuation_pending(state);
    if state.last_driven_phase.as_deref() == Some(state.phase.as_str()) && !continuing {
        return Ok(None);
    }

    // Resolve the one track execution target before any capture seam, claim_continuation,
    // mark_driven or watchdog Inject. Production and test capture share this resolution.
    let prepared = if super::execution_repo::prepares_target(&state.phase) {
        match super::execution_repo::resolve_track_target(record, state, &state.phase) {
            Ok(target) => Some(super::execution_repo::PreparedInjection::for_target(target)),
            Err(err) => {
                let track = state.track_id.as_deref().unwrap_or("<id>");
                return fail_phase(
                    record,
                    state,
                    FailureClass::Permission,
                    format!(
                        "{err}. Declare one execution target; repair conductor.md or spec \
                         metadata, then `coordinator failure resolve` and \
                         `coordinator run --track {track}`."
                    ),
                    OutcomeSource::Adapter,
                );
            }
        }
    } else {
        None
    };
    let prepared_target = prepared.as_ref().and_then(|p| p.target());

    // 0101: compare against the frozen delivery target before any prompt. A row edit that
    // moved the target after the checkpoint stops the phase with Permission and zero prompts.
    if let (Some(stored), Some(current)) = (state.delivery_target.as_ref(), prepared_target)
        && stored.project_id == current.project_id
        && stored.run_epoch == current.run_epoch
        && stored.track_id == current.track_id
        && super::execution_repo::uses_delivery_target(&stored.phase)
        && super::execution_repo::uses_delivery_target(&current.phase)
        && !super::execution_repo::same_delivery_repo(stored, current)
    {
        let track = state.track_id.as_deref().unwrap_or("<id>");
        return fail_phase(
            record,
            state,
            FailureClass::Permission,
            format!(
                "execution target changed after checkpoint: {} -> {}; stop and re-run --track {track}",
                stored.declared_path.display(),
                current.declared_path.display()
            ),
            OutcomeSource::Adapter,
        );
    }

    // Test seam: capture the continuation prompt without resolving a harness binary.
    #[cfg(test)]
    if continuing && capture_slot_armed() {
        let Some(prompt) = super::self_check::claim_continuation(record, prepared_target)? else {
            return Ok(None);
        };
        let inject_sid = crate::harness::status_bundle_sync(record)
            .and_then(|b| b.grok)
            .and_then(|g| g.session_id);
        crate::workflow::watchdog::note_progress(
            record,
            crate::workflow::watchdog::ProgressKind::Inject,
            inject_sid.as_deref(),
            false,
        );
        push_captured_prepared(prepared.as_ref());
        push_captured_prompt(&prompt);
        return Ok(None);
    }

    if !continuing && let Err(err) = refresh_plan_evidence_stamp(record, state) {
        return fail_phase(
            record,
            state,
            FailureClass::Permission,
            format!("evidence stamp: {err}"),
            OutcomeSource::Adapter,
        );
    }

    // 0110: a deferred role swap applies in-process at the first tick of the phase that
    // resolves the role, before the binding is read and before any spawn/recycle.
    if !continuing {
        let _ = crate::harness::roles_swap::apply_pending_for_phase_for(&state.phase, record);
    }

    // Resolve the phase binding *before* live-session reuse so a non-grok
    // planner cannot silently prompt an existing Grok session.
    let bindings = crate::harness::load_role_bindings()
        .unwrap_or_else(|_| crate::config::default_role_bindings());
    let role_key = resolve_phase_role_key(&state.phase, &bindings).ok_or_else(|| {
        CoordinatorError::Message(format!("no role binding for phase {}", state.phase))
    })?;
    let binding = match resolve_phase_binding(&state.phase) {
        Ok(b) => b,
        Err(e) => {
            return fail_phase(
                record,
                state,
                FailureClass::Permission,
                e.to_string(),
                OutcomeSource::Adapter,
            );
        }
    };
    if !crate::harness::is_acp_session_harness(&binding.harness) {
        return fail_phase(
            record,
            state,
            FailureClass::Permission,
            format!(
                "no long-lived adapter for harness {} (phase {} role {})",
                binding.harness, state.phase, role_key
            ),
            OutcomeSource::Adapter,
        );
    }

    let phase_bin = match resolve_phase_binary(&state.phase) {
        Ok(p) => p,
        Err(e) => {
            return fail_phase(
                record,
                state,
                FailureClass::Permission,
                e.to_string(),
                OutcomeSource::Adapter,
            );
        }
    };

    if !continuing {
        let missing = super::reuse::missing_required_skill_paths(
            record,
            &state.phase,
            state.track_id.as_deref(),
        );
        if !missing.is_empty() {
            return fail_phase(
                record,
                state,
                FailureClass::SkillMissing,
                super::reuse::skill_missing_message(&missing),
                OutcomeSource::Adapter,
            );
        }
    }

    let live = crate::harness::status_bundle_sync(record).and_then(|b| b.grok);
    // 0110: the live identity is adapter + model + resolved binary. A same-harness model or
    // command change therefore recycles here; the existing path below is the only recycle.
    let recycle_mismatch = live
        .as_ref()
        .is_some_and(|g| holder_mismatch(g, &binding, phase_bin.as_path()));

    let prompt = if continuing {
        match super::self_check::claim_continuation(record, prepared_target)? {
            Some(prompt) => prompt,
            None => return Ok(None),
        }
    } else {
        mark_driven_with_target(record, &state.phase, prepared_target)?;
        snapshot_registry_before_phase(record, &state.phase)?;
        prompts::phase_prompt_with_target(
            record,
            &state.phase,
            state.track_id.as_deref(),
            prepared_target,
        )
    };
    // Inject-start heartbeat so a slow ACP start is not an immediate stall.
    // Continuation uses the same kind. Classify and the self-check log do not.
    let inject_sid = crate::harness::status_bundle_sync(record)
        .and_then(|b| b.grok)
        .and_then(|g| g.session_id);
    crate::workflow::watchdog::note_progress(
        record,
        crate::workflow::watchdog::ProgressKind::Inject,
        inject_sid.as_deref(),
        false,
    );

    #[cfg(test)]
    {
        if capture_slot_armed() {
            push_captured_prepared(prepared.as_ref());
            push_captured_prompt(&prompt);
            return Ok(None);
        }
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

    let selector = record.path.to_string_lossy().to_string();
    let rec = record.clone();
    let model = binding.model.clone();
    let harness = binding.harness.clone();
    let prepared_for_finish = prepared.clone();
    let run_epoch = state.run_epoch;
    let hold_epoch = match crate::state::load_run_book(record) {
        Ok(crate::state::RunBook::Many(_)) => Some(run_epoch),
        _ => None,
    };
    // First tick starts the holder Prompt without blocking poll_once. Later ticks
    // are no-ops (`mark_driven`). The holder / pool apply path writes the outcome.
    std::thread::Builder::new()
        .name(format!("adapter-inject-{}", rec.id))
        .spawn(move || {
            let _scope = crate::state::RunScope::enter(run_epoch).ok();
            let result = block_on_async(async {
                if recycle_mismatch
                    && let Err(e) = crate::harness::pool::recycle_without_pool_lock(&rec).await
                {
                    eprintln!("coordinator: adapter recycle: project={} {e}", rec.id);
                }
                crate::harness::start_with_bin(
                    Some(&selector),
                    ADAPTER_START_IN_PROCESS,
                    phase_bin,
                    model,
                    false,
                    &harness,
                    hold_epoch,
                )
                .await?;
                match prepared {
                    Some(prepared) => {
                        crate::harness::prompt_prepared(Some(&selector), prompt, false, prepared)
                            .await
                    }
                    None => crate::harness::prompt(Some(&selector), prompt, false).await,
                }
            });
            finish_adapter_inject(&rec, result, prepared_for_finish.as_ref());
        })
        .map_err(|e| {
            CoordinatorError::Message(format!("failed to spawn adapter inject thread: {e}"))
        })?;
    Ok(None)
}

fn finish_adapter_inject(
    record: &ProjectRecord,
    result: Result<crate::harness::HarnessPromptView>,
    prepared: Option<&crate::workflow::execution_repo::PreparedInjection>,
) {
    let injected_phase = prepared
        .map(|p| p.phase.clone())
        .or_else(|| load_run_state(record).ok().map(|s| s.phase))
        .unwrap_or_default();
    match result {
        Ok(view) => {
            if view.applied || view.skipped == Some(true) {
                return;
            }
            if let Some(err) = view.error {
                let class = view.failure_class.unwrap_or(FailureClass::HarnessCrash);
                apply_adapter_failure(record, class, err, prepared, &injected_phase);
            }
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.starts_with("ACP stdout closed during ") {
                match crate::harness::abort::note_acp_stdout_close(record, &msg, None) {
                    crate::harness::abort::StdoutCloseDisposition::Ignore
                    | crate::harness::abort::StdoutCloseDisposition::Retry => {}
                    crate::harness::abort::StdoutCloseDisposition::Fail { message } => {
                        let message =
                            crate::ci::with_green_pr_prefix(record, &injected_phase, message);
                        apply_adapter_failure(
                            record,
                            FailureClass::HarnessCrash,
                            message,
                            prepared,
                            &injected_phase,
                        );
                    }
                }
            } else if matches!(
                &e,
                CoordinatorError::Io(io)
                    if crate::harness::abort::io_error_is_transport_drop(io)
            ) {
                match crate::harness::abort::note_transport_drop(record, &e, None) {
                    crate::harness::abort::StdoutCloseDisposition::Ignore
                    | crate::harness::abort::StdoutCloseDisposition::Retry => {}
                    crate::harness::abort::StdoutCloseDisposition::Fail { message } => {
                        let message =
                            crate::ci::with_green_pr_prefix(record, &injected_phase, message);
                        apply_adapter_failure(
                            record,
                            FailureClass::HarnessCrash,
                            message,
                            prepared,
                            &injected_phase,
                        );
                    }
                }
            } else {
                apply_adapter_failure(
                    record,
                    FailureClass::HarnessCrash,
                    msg,
                    prepared,
                    &injected_phase,
                );
            }
        }
    }
}

/// Apply one failure only if the injected identity is still current. Never fails a
/// newer run; a stale prepared turn is a structured skip.
fn apply_adapter_failure(
    record: &ProjectRecord,
    class: FailureClass,
    message: String,
    prepared: Option<&crate::workflow::execution_repo::PreparedInjection>,
    injected_phase: &str,
) {
    let Ok(state) = load_run_state(record) else {
        return;
    };
    if state.status != RunStatus::Running {
        return;
    }
    if let Some(prepared) = prepared {
        if !prepared.matches_state(&state) {
            journal_stale_injection(record, prepared);
            return;
        }
        let outcome = PhaseOutcome::failure(
            prepared.phase.clone(),
            class,
            OutcomeSource::Adapter,
            Some(message),
            Some(prepared.run_epoch),
        );
        match crate::outcome::write_and_apply_prepared(record, outcome, prepared) {
            Ok(crate::outcome::ApplyIdentity::Applied(_)) => {}
            Ok(crate::outcome::ApplyIdentity::Stale(_)) => {
                journal_stale_injection(record, prepared);
            }
            Err(_) => {}
        }
        return;
    }
    let _ = fail_phase(record, &state, class, message, OutcomeSource::Adapter);
    let _ = injected_phase;
}

fn journal_stale_injection(
    record: &ProjectRecord,
    prepared: &crate::workflow::execution_repo::PreparedInjection,
) {
    let track = prepared
        .target()
        .map(|t| t.track_id.as_str())
        .or(prepared.track_id.as_deref())
        .unwrap_or("(none)");
    let source = prepared
        .target()
        .map(|t| t.source.as_str())
        .unwrap_or("none");
    crate::progress_log::append(
        record,
        "sample-skip",
        &format!("leg sample skipped: track={track} source={source} reason=stale-injection"),
    );
}

fn drive_plan_review(
    record: &ProjectRecord,
    state: &RunState,
) -> Result<Option<crate::state::StatusView>> {
    if state.driver == WorkflowDriver::Adapter {
        let missing = super::reuse::missing_required_skill_paths(
            record,
            PHASE_PLAN_REVIEW,
            state.track_id.as_deref(),
        );
        if !missing.is_empty() {
            return fail_phase(
                record,
                state,
                FailureClass::SkillMissing,
                super::reuse::skill_missing_message(&missing),
                OutcomeSource::Adapter,
            );
        }
    }
    ensure_plan_review_pending(record, state)?;
    // 0119: re-load after adoption so the stub (which stamps the entry on its role outcomes)
    // and the spawn path both see the live entry, not the pre-adoption snapshot.
    let state = &load_run_state(record).unwrap_or_else(|_| state.clone());
    match state.driver {
        WorkflowDriver::Stub => write_stub_reviews(record, state)?,
        WorkflowDriver::Adapter => super::plan_review::maybe_spawn_plan_review(record, state)?,
        WorkflowDriver::FileWait => {}
    }
    consume_role_files(record)?;
    try_join(record)
}

/// 0119: the single adoption point for a `plan-review` phase that is already in flight.
///
/// A **current** entry owns its pending set: refilling an empty pending here would make the
/// join's `Wait` arm unobservable and re-add a produced/consumed slug on the next tick. A missing
/// entry is adopted and refilled only for a genuinely untouched legacy phase.
fn ensure_plan_review_pending(record: &ProjectRecord, state: &RunState) -> Result<()> {
    if state.plan_review_entry_current().is_some() {
        return Ok(());
    }
    with_run_state_lock(record, || {
        let mut s = load_run_state(record)?;
        if s.phase != PHASE_PLAN_REVIEW {
            return Ok(());
        }
        if s.plan_review_entry_current().is_some() {
            // Another ticker won the adoption.
            return Ok(());
        }
        // Never `begin()` here: a mid-phase resume must keep the books and must not re-spawn
        // live children.
        s.adopt_plan_review_entry();
        let untouched = s.plan_review_spawned.is_empty()
            && s.plan_review_slot_ran.is_empty()
            && s.plan_review_verdicts.is_empty()
            && s.plan_review_admission_failures.is_empty();
        if s.pending_roles.is_empty()
            && untouched
            && super::plan_review::remaining_budget(record, &s)
                >= super::plan_review::MIN_SPAWN_BUDGET
        {
            s.pending_roles = review_slugs().iter().map(|x| (*x).to_string()).collect();
        }
        s.updated_at = chrono::Utc::now();
        save_run_state(record, &s)
    })
}

fn write_stub_reviews(record: &ProjectRecord, state: &RunState) -> Result<()> {
    let roles_dir = outcome_roles_dir(record)?;
    std::fs::create_dir_all(&roles_dir)?;
    std::fs::create_dir_all(reviews_dir(record)?)?;
    let entry = state.plan_review_entry_current();
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
            // 0119: a stub role outcome is a role outcome like any other; the consumer fails
            // closed on a missing pair.
            entry_id: entry.map(|e| e.entry_id),
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
        // Best effort: the mailbox is the canonical evidence. A track-copy failure only makes
        // the receipt omit the slot (safe direction); it must never turn a produced review into
        // a failed attempt.
        if let Err(e) = crate::persist::atomic_write(&copy, text.as_bytes()) {
            crate::progress_log::append(
                record,
                "plan-review",
                &format!("plan-review: {slug} track review copy failed: {e}"),
            );
        }
    }
    Ok(dest)
}

/// Consume role outcomes for the live entry, one run-state lock per slug.
///
/// 0119: an outcome must carry **both** the live `run_epoch` and `metadata.entry_id`; an absent or
/// differing pair is journalled and the file is left in place (fail closed, OD-119-8). A Success
/// without a mailbox artifact is a named degrade - there is no track-copy adoption.
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
        let mut consumed = false;
        with_run_state_lock(record, || {
            let mut s = load_run_state(record)?;
            if s.phase != PHASE_PLAN_REVIEW {
                return Ok(());
            }
            let Some(live) = s.plan_review_entry_current() else {
                return Ok(());
            };
            if outcome.run_epoch != Some(live.run_epoch) {
                let detail = match outcome.run_epoch {
                    Some(epoch) => format!(
                        "plan-review: ignored {slug} outcome from epoch {epoch} (live {})",
                        live.run_epoch
                    ),
                    None => format!("plan-review: ignored {slug} outcome with no epoch/entry"),
                };
                crate::progress_log::append(record, "plan-review", &detail);
                return Ok(());
            }
            if outcome.metadata.as_ref().and_then(|m| m.entry_id) != Some(live.entry_id) {
                let detail = match outcome.metadata.as_ref().and_then(|m| m.entry_id) {
                    Some(entry) => format!(
                        "plan-review: ignored {slug} outcome from entry {entry} (live {})",
                        live.entry_id
                    ),
                    None => format!("plan-review: ignored {slug} outcome with no epoch/entry"),
                };
                crate::progress_log::append(record, "plan-review", &detail);
                return Ok(());
            }
            let verdict = match outcome.status {
                OutcomeStatus::Success => {
                    if bundle::review_produced(record, &s, &slug) {
                        None
                    } else {
                        Some("resolved: success outcome without a review artifact".to_string())
                    }
                }
                _ => {
                    let class = outcome.failure_class.unwrap_or(FailureClass::HarnessCrash);
                    let msg = outcome.message.as_deref().unwrap_or("");
                    Some(format!(
                        "failed: {}: {}",
                        class.as_str(),
                        super::plan_review::normalize_verdict(msg)
                    ))
                }
            };
            apply_pending_and_verdict(&mut s, &slug, verdict.as_deref());
            s.updated_at = chrono::Utc::now();
            #[cfg(test)]
            if take_pending_save_failure(&record.id, &slug) {
                return Err(CoordinatorError::Message(
                    "injected remove_pending failure".into(),
                ));
            }
            save_run_state(record, &s)?;
            consumed = true;
            Ok(())
        })?;
        // Save the acknowledgment BEFORE deleting the mailbox (0088 follow-up): a failed save
        // keeps the file, so the retry is idempotent.
        if consumed {
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(())
}

/// Every removal leaves a verdict, a produced mailbox artifact, or an admission-failure record.
///
/// Lock-free by design: the caller owns the single run-state lock for the path. `consume_role_files`
/// owns it per slug; `remove_pending_if_entry` takes it itself. No path nests the two.
fn apply_pending_and_verdict(s: &mut RunState, slug: &str, verdict: Option<&str>) -> bool {
    let before = s.pending_roles.len();
    s.pending_roles.retain(|x| x != slug);
    if let Some(v) = verdict {
        // First writer within an attempt wins: consume must not overwrite a worker's `ran; dud:`.
        s.plan_review_verdicts
            .entry(slug.to_string())
            .or_insert_with(|| super::plan_review::normalize_verdict(v));
    }
    s.pending_roles.len() != before
}

#[cfg(test)]
static TEST_FAIL_PENDING_SAVE: std::sync::Mutex<Option<(String, String)>> =
    std::sync::Mutex::new(None);

/// Test-only, RAII-disarmed `remove_pending` save failpoint (0113).
///
/// Scoped to one `(project_id, slug)` and cleared on drop, so a panic cannot leave it armed for a
/// sibling test. Consumed once by the next matching save.
#[cfg(test)]
pub(crate) struct PendingSaveFailpoint;

#[cfg(test)]
impl Drop for PendingSaveFailpoint {
    fn drop(&mut self) {
        *TEST_FAIL_PENDING_SAVE
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = None;
    }
}

#[cfg(test)]
pub(crate) fn fail_pending_save_once(project_id: &str, slug: &str) -> PendingSaveFailpoint {
    *TEST_FAIL_PENDING_SAVE
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = Some((project_id.to_string(), slug.to_string()));
    PendingSaveFailpoint
}

#[cfg(test)]
fn take_pending_save_failure(project_id: &str, slug: &str) -> bool {
    let mut armed = TEST_FAIL_PENDING_SAVE
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    match armed.as_ref() {
        Some((id, s)) if id == project_id && s == slug => {
            *armed = None;
            true
        }
        _ => false,
    }
}

/// Remove a pending slug only while the live entry and track still match, recording the verdict
/// in the same save. Takes the run-state lock itself and is **never** called from inside a lock.
///
/// 0119: `remove_pending` is deleted. `consume_role_files` uses the lock-free
/// `apply_pending_and_verdict` inside its own single lock; this is the only other removal.
pub(crate) fn remove_pending_if_entry(
    record: &ProjectRecord,
    slug: &str,
    entry: PlanReviewEntry,
    track_id: Option<&str>,
    verdict: Option<&str>,
) -> Result<bool> {
    with_run_state_lock(record, || {
        let mut s = load_run_state(record)?;
        if s.plan_review_entry_current() != Some(entry) || s.track_id.as_deref() != track_id {
            return Ok(false);
        }
        apply_pending_and_verdict(&mut s, slug, verdict);
        s.updated_at = chrono::Utc::now();
        #[cfg(test)]
        if take_pending_save_failure(&record.id, slug) {
            return Err(CoordinatorError::Message(
                "injected remove_pending failure".into(),
            ));
        }
        save_run_state(record, &s)?;
        Ok(true)
    })
}

/// 0119: purge one entry transition's plan-review mailboxes and role outcomes.
///
/// Removes only `{state}/reviews/<slug>-review.md` and `{outcomes}/roles/<slug>.json` for
/// `review_slugs()`. Cross-model files and the directories are left alone.
pub(crate) fn purge_plan_review_entry_artifacts(record: &ProjectRecord) {
    let roles_dir = outcome_roles_dir(record).ok();
    for slug in review_slugs() {
        if let Ok(path) = bundle::review_file(record, slug) {
            let _ = std::fs::remove_file(path);
        }
        if let Some(ref dir) = roles_dir {
            let _ = std::fs::remove_file(dir.join(format!("{slug}.json")));
        }
    }
}

/// Which track reviews to archive before a fresh start deletes mailboxes.
pub struct ArtifactClear {
    pub track_id: Option<String>,
    pub closed_epoch: u64,
    pub archive_slugs: Vec<String>,
}

/// `Cleared` archived every requested slug. `StayPlan` left a live file because
/// `prior/` already held different bytes or the copy failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearEffect {
    Cleared,
    StayPlan,
}

/// Drop leftover role/review mailboxes so a fresh `run` cannot join stale slots.
///
/// Does not load run-state. Archives only `archive_slugs`, then deletes those
/// live track files. A conflict leaves the live file in place.
pub fn clear_plan_review_artifacts(record: &ProjectRecord, clear: &ArtifactClear) -> ClearEffect {
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
    let mut stay = false;
    if let Some(track_id) = clear.track_id.as_deref()
        && let Some(track_dir) = resolve_track_dir(record, track_id)
    {
        for slug in &clear.archive_slugs {
            match super::reuse::archive_track_review(&track_dir, slug, clear.closed_epoch) {
                super::reuse::ArchiveOutcome::Archived => {
                    let _ = std::fs::remove_file(track_dir.join(format!("{slug}-review.md")));
                }
                super::reuse::ArchiveOutcome::Conflict => stay = true,
                super::reuse::ArchiveOutcome::Absent => {}
            }
        }
    }
    if stay {
        ClearEffect::StayPlan
    } else {
        ClearEffect::Cleared
    }
}

fn log_join_wait_once(record: &ProjectRecord, state: &RunState, missing: &[&str]) {
    let msg = format!(
        "plan-review: join waits {} (spawned this entry; phase clock bounds it)",
        missing.join(", ")
    );
    if state.last_event == msg {
        return;
    }
    let _ = with_run_state_lock(record, || {
        let mut s = load_run_state(record)?;
        if s.phase != PHASE_PLAN_REVIEW || s.last_event == msg {
            return Ok(());
        }
        s.last_event = msg.clone();
        s.updated_at = chrono::Utc::now();
        save_run_state(record, &s)
    });
    crate::progress_log::append(record, "plan-review", &msg);
}

fn try_join(record: &ProjectRecord) -> Result<Option<crate::state::StatusView>> {
    let state = load_run_state(record)?;
    if state.phase != PHASE_PLAN_REVIEW {
        return Ok(None);
    }
    // 0119: a synchronous admission failure stops the phase **before** the pending guard, so a
    // still-running sibling cannot mask it and nothing is degraded.
    let admission: Vec<(String, String)> = review_slugs()
        .iter()
        .filter_map(|slug| {
            state
                .plan_review_admission_failure(slug)
                .map(|cause| ((*slug).to_string(), cause.to_string()))
        })
        .collect();
    if !admission.is_empty() {
        let msg = admission
            .iter()
            .map(|(slug, cause)| format!("{slug} admission failed: {cause}"))
            .collect::<Vec<_>>()
            .join(", ");
        return fail_phase(
            record,
            &state,
            FailureClass::Permission,
            format!("plan-review: {msg}"),
            OutcomeSource::Test,
        );
    }
    if !state.pending_roles.is_empty() {
        return Ok(None);
    }
    let Some(entry) = state.plan_review_entry_current() else {
        let msg = "plan-review: join without an entry";
        if state.last_event != msg {
            crate::progress_log::append(record, "plan-review", msg);
        }
        return Ok(None);
    };
    let produced: Vec<&'static str> = review_slugs()
        .iter()
        .copied()
        .filter(|slug| bundle::review_produced(record, &state, slug))
        .collect();
    let missing: Vec<&'static str> = review_slugs()
        .iter()
        .copied()
        .filter(|slug| !bundle::review_produced(record, &state, slug))
        .collect();
    if produced.is_empty()
        && let Some(id) = state.track_id.as_deref()
        && resolve_track_dir(record, id).is_none()
    {
        return fail_phase(
            record,
            &state,
            FailureClass::HarnessCrash,
            format!("plan-review: cannot resolve track dir for id '{id}'"),
            OutcomeSource::Test,
        );
    }
    let budget = super::plan_review::remaining_budget(record, &state);
    match super::plan_review::join_gate(&state, &missing, budget) {
        super::plan_review::JoinGate::FailAdmission(v) => {
            let msg = v
                .iter()
                .map(|(slug, cause)| format!("{slug} admission failed: {cause}"))
                .collect::<Vec<_>>()
                .join(", ");
            return fail_phase(
                record,
                &state,
                FailureClass::Permission,
                format!("plan-review: {msg}"),
                OutcomeSource::Test,
            );
        }
        super::plan_review::JoinGate::Rearm(v) => {
            super::plan_review::rearm_missing(record, &v, entry)?;
            return Ok(None);
        }
        super::plan_review::JoinGate::Wait => {
            log_join_wait_once(record, &state, &missing);
            return Ok(None);
        }
        super::plan_review::JoinGate::NoBudget(v) => {
            return fail_phase(
                record,
                &state,
                FailureClass::HarnessCrash,
                format!(
                    "plan-review: {} never spawned (no remaining spawn budget)",
                    v.join(", ")
                ),
                OutcomeSource::Test,
            );
        }
        super::plan_review::JoinGate::Degrade => {}
    }
    if produced.is_empty() {
        // Only ran/failed slots reach here; the existing zero-output ladder is unchanged.
        if super::plan_review::maybe_rearm_join_retry(record)? {
            return Ok(None);
        }
        let latest = load_run_state(record)?;
        if !latest.pending_roles.is_empty() {
            return Ok(None);
        }
        return fail_phase(
            record,
            &latest,
            FailureClass::HarnessCrash,
            "plan-review: zero reviewers produced output".into(),
            OutcomeSource::Test,
        );
    }
    super::reuse::write_join_receipt(record, &state);
    bundle::assemble(record, &state)?;
    let msg = if missing.is_empty() {
        None
    } else {
        Some(super::plan_review::degrade_message(&state, &missing, false))
    };
    synth_success(record, &state, msg, OutcomeSource::Test)
}

/// What the plan-review timeout exit decided.
#[derive(Debug)]
pub enum TimeoutDecision {
    /// Apply this outcome (a success with named degradations, or a named failure).
    Apply(PhaseOutcome),
    /// Nothing is missing and pending is empty: the join owns that state.
    LetJoin,
}

/// Join-timeout helper (caller already holds apply + run-state locks).
///
/// 0119 spec §3.1.1: adopt a missing entry first, then classify **every** pending and missing
/// slug. A never-tried or admission-failed slot fails the phase (named), a produced leftover is
/// not degraded and is in the receipt, and an empty pending with nothing missing returns
/// `LetJoin` instead of the generic "phase budget exceeded". The failure/`LetJoin` paths perform
/// no pending clear, no receipt write and no assembly.
pub fn timeout_plan_review_outcome(
    record: &ProjectRecord,
    state: &RunState,
) -> Result<TimeoutDecision> {
    let mut snap = state.clone();
    if snap.phase == PHASE_PLAN_REVIEW && snap.plan_review_entry_current().is_none() {
        // A pre-0119 mid-phase state at the budget must not be classified from empty books.
        snap.adopt_plan_review_entry();
        snap.updated_at = chrono::Utc::now();
        save_run_state(record, &snap)?;
    }
    if snap.phase != PHASE_PLAN_REVIEW {
        return Ok(TimeoutDecision::LetJoin);
    }
    let mut missing: Vec<(String, super::plan_review::SlotCause)> = Vec::new();
    let mut produced_any = false;
    for slug in review_slugs() {
        if bundle::review_produced(record, &snap, slug) {
            produced_any = true;
            continue;
        }
        missing.push((
            (*slug).to_string(),
            super::plan_review::slot_cause(&snap, slug),
        ));
    }
    let never: Vec<&(String, super::plan_review::SlotCause)> = missing
        .iter()
        .filter(|(_, cause)| {
            matches!(
                cause,
                super::plan_review::SlotCause::NeverSpawned
                    | super::plan_review::SlotCause::AdmissionFailed(_)
            )
        })
        .collect();
    if !never.is_empty() {
        let class = if never
            .iter()
            .any(|(_, c)| matches!(c, super::plan_review::SlotCause::AdmissionFailed(_)))
        {
            FailureClass::Permission
        } else {
            FailureClass::Timeout
        };
        // Name every cause, not only the never-tried ones: the phase failure is the operator's
        // only record of the attempt, and the ran/failed leftovers explain why nothing produced.
        let parts = missing
            .iter()
            .map(|(slug, cause)| format!("{slug} {}", cause.describe()))
            .collect::<Vec<_>>()
            .join(", ");
        return Ok(TimeoutDecision::Apply(PhaseOutcome::failure(
            snap.phase.clone(),
            class,
            OutcomeSource::Timeout,
            Some(format!("plan-review: {parts}")),
            Some(snap.run_epoch),
        )));
    }
    if snap.pending_roles.is_empty() && missing.is_empty() {
        return Ok(TimeoutDecision::LetJoin);
    }
    if !produced_any {
        let parts = missing
            .iter()
            .map(|(slug, cause)| format!("{slug} {}", cause.describe()))
            .collect::<Vec<_>>()
            .join(", ");
        return Ok(TimeoutDecision::Apply(PhaseOutcome::failure(
            snap.phase.clone(),
            FailureClass::Timeout,
            OutcomeSource::Timeout,
            Some(format!(
                "plan-review: no reviewer produced output ({parts})"
            )),
            Some(snap.run_epoch),
        )));
    }
    let mut cleared = snap.clone();
    cleared.pending_roles.clear();
    save_run_state(record, &cleared)?;
    super::reuse::write_join_receipt(record, &cleared);
    bundle::assemble(record, &cleared)?;
    let degraded: Vec<&str> = missing.iter().map(|(slug, _)| slug.as_str()).collect();
    let msg = if degraded.is_empty() {
        None
    } else {
        Some(super::plan_review::degrade_message(&snap, &degraded, true))
    };
    Ok(TimeoutDecision::Apply(PhaseOutcome::success(
        snap.phase.clone(),
        OutcomeSource::Timeout,
        msg,
        None,
        Some(snap.run_epoch),
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

/// Snapshot the registry status cells before an `advance`/`fold` inject (0088 DoD-1).
///
/// Only those two phases are guarded: `advance` is where the owner-GO inversion was
/// observed, and `fold` is the phase that rewrites spec/plan banners and can carry the
/// cell with it. Other phases are not snapshot, to avoid pointless state churn.
fn snapshot_registry_before_phase(
    record: &crate::registry::ProjectRecord,
    phase: &str,
) -> crate::error::Result<()> {
    use crate::workflow::graph::{PHASE_ADVANCE, PHASE_FOLD};
    if phase != PHASE_ADVANCE && phase != PHASE_FOLD {
        return Ok(());
    }
    let snap = crate::workflow::conductor_md::status_snapshot_for(record);
    if snap.is_empty() {
        return Ok(());
    }
    crate::workflow::with_run_state_lock(record, || {
        let mut state = crate::state::load_run_state(record)?;
        state.registry_status_before = snap;
        state.updated_at = chrono::Utc::now();
        crate::state::save_run_state(record, &state)
    })
}

/// Restored ids plus whether the Ready snapshot was consumed (0090).
///
/// `consumed` is true only after the registry file was read and parsed, and either
/// nothing needed a rewrite or that rewrite was written. The caller clears the
/// in-memory map. This value does not itself save run state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyRestore {
    pub restored: Vec<String>,
    pub consumed: bool,
}

/// Undo a non-terminal Ready downgrade captured before advance/fold (0088, 0090).
///
/// Does not call [`crate::state::with_run_state_lock`] or [`crate::state::save_run_state`].
/// `apply` already holds the lock around `on_success`, and that later save is what
/// persists an in-memory clear. A nested lock from here would time out.
pub fn restore_ready_rows_after_phase(record: &crate::registry::ProjectRecord) -> ReadyRestore {
    let keep = || ReadyRestore {
        restored: Vec::new(),
        consumed: false,
    };
    let state = match crate::state::load_run_state(record) {
        Ok(s) => s,
        Err(_) => return keep(),
    };
    if state.registry_status_before.is_empty() {
        return keep();
    }
    let dir = crate::layout::resolve(record).conductor_dir;
    let result =
        crate::registry_store::with_registry_lock(&dir, || restore_locked(record, &state, &dir));
    match result {
        Ok(restore) => restore,
        Err(e) => {
            crate::progress_log::append(
                record,
                "registry-write-refused",
                &format!("restore withheld: {e}"),
            );
            keep()
        }
    }
}

fn restore_locked(
    record: &crate::registry::ProjectRecord,
    state: &crate::state::RunState,
    dir: &std::path::Path,
) -> crate::error::Result<ReadyRestore> {
    use crate::registry_store::StoreView;
    let keep = || ReadyRestore {
        restored: Vec::new(),
        consumed: false,
    };
    let snapshot = crate::registry_store::snapshot_at(dir)?;
    match &snapshot.view {
        StoreView::Absent => {
            let path = dir.join("conductor.md");
            let Ok(text) = crate::workflow::conductor_md::read_view_for_parse(&path) else {
                return Ok(keep());
            };
            if crate::workflow::conductor_md::parse_conductor_md(&text).is_err() {
                return Ok(keep());
            }
            let aliases = crate::workflow::conductor_md::ready_aliases_for(record);
            let (restored, body) = crate::workflow::conductor_md::restore_ready_rows(
                &text,
                &state.registry_status_before,
                &aliases,
            );
            if let Some(body) = body {
                if std::fs::write(&path, body).is_err() {
                    return Ok(keep());
                }
                crate::progress_log::append(
                    record,
                    "advance-downgrade-restored",
                    &format!("restored Ready: {}", restored.join(", ")),
                );
            }
            Ok(ReadyRestore {
                restored,
                consumed: true,
            })
        }
        StoreView::Drifted { expected, actual } => {
            // A view edit made during the phase is the defect. Withhold; never rewrite,
            // never report success while a silent downgrade could survive.
            crate::registry_store::report_drift_once(record, expected, actual);
            Ok(ReadyRestore {
                restored: Vec::new(),
                consumed: false,
            })
        }
        StoreView::Ok => {
            let aliases = crate::workflow::conductor_md::ready_aliases_for(record);
            let mut changes: Vec<(String, String)> = Vec::new();
            for row in &snapshot.rows {
                let Some(prev) = state.registry_status_before.get(&row.id) else {
                    continue;
                };
                let was_ready = crate::workflow::conductor_md::is_eligible_ready_in(prev, &aliases);
                let now_ready =
                    crate::workflow::conductor_md::is_eligible_ready_in(&row.status_raw, &aliases);
                if was_ready
                    && !now_ready
                    && !crate::workflow::conductor_md::is_terminal(&row.status_raw)
                    && !crate::workflow::conductor_md::is_reasoned_downgrade(&row.status_raw)
                {
                    changes.push((row.id.clone(), prev.clone()));
                }
            }
            if changes.is_empty() {
                return Ok(ReadyRestore {
                    restored: Vec::new(),
                    consumed: true,
                });
            }
            match crate::registry_store::set_statuses_locked(dir, &changes, "advance", false) {
                Ok(crate::registry_store::WriteOutcome::Changed(ids)) => {
                    crate::progress_log::append(
                        record,
                        "advance-downgrade-restored",
                        &format!("restored Ready: {}", ids.join(", ")),
                    );
                    Ok(ReadyRestore {
                        restored: ids,
                        consumed: true,
                    })
                }
                Ok(_) => Ok(ReadyRestore {
                    restored: Vec::new(),
                    consumed: true,
                }),
                Err(e) => {
                    crate::progress_log::append(
                        record,
                        "registry-write-refused",
                        &format!("restore withheld: {e}"),
                    );
                    Ok(keep())
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ENV_COORDINATOR_HOME, MachineConfig, RoleBinding, registry_path, save_machine_config,
        test_env_lock,
    };
    use crate::harness::grok::{ENV_CURSOR_BIN, ENV_GROK_BIN, mock_handshake_ok};
    use crate::harness::roles::{ROLE_FOLD, ROLE_NEXT};
    use crate::registry::{ProjectAddOptions, Registry};
    use crate::run::run_with_driver;
    use crate::state::load_run_state;
    use crate::watch::poll_once;
    use crate::workflow::WorkflowDriver;
    use crate::workflow::graph::{PHASE_PLAN, ROLE_IMPLEMENTOR, ROLE_PLANNER, WORKFLOW_ID};
    use std::ffi::OsString;
    use std::path::PathBuf;
    use tempfile::tempdir;
    use uuid::Uuid;

    struct IsolatedHome {
        prev_home: Option<OsString>,
        prev_bin: Option<OsString>,
        prev_cursor: Option<OsString>,
        prev_state: Option<OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
        _home: tempfile::TempDir,
    }

    impl IsolatedHome {
        fn enter() -> Self {
            let lock = test_env_lock();
            let prev_home = std::env::var_os(ENV_COORDINATOR_HOME);
            let prev_bin = std::env::var_os(ENV_GROK_BIN);
            let prev_cursor = std::env::var_os(ENV_CURSOR_BIN);
            let prev_state = std::env::var_os(crate::config::ENV_COORDINATOR_STATE_DIR);
            let home = tempdir().unwrap();
            unsafe {
                std::env::set_var(ENV_COORDINATOR_HOME, home.path());
                std::env::remove_var(ENV_GROK_BIN);
                std::env::remove_var(ENV_CURSOR_BIN);
                std::env::remove_var(crate::config::ENV_COORDINATOR_STATE_DIR);
            }
            Self {
                prev_home,
                prev_bin,
                prev_cursor,
                prev_state,
                _lock: lock,
                _home: home,
            }
        }

        fn write_bindings(
            &self,
            mutate: impl FnOnce(&mut std::collections::BTreeMap<String, RoleBinding>),
        ) {
            let mut cfg = MachineConfig::default();
            mutate(&mut cfg.role_bindings);
            save_machine_config(&cfg).unwrap();
        }
    }

    impl Drop for IsolatedHome {
        fn drop(&mut self) {
            unsafe {
                match &self.prev_home {
                    Some(v) => std::env::set_var(ENV_COORDINATOR_HOME, v),
                    None => std::env::remove_var(ENV_COORDINATOR_HOME),
                }
                match &self.prev_bin {
                    Some(v) => std::env::set_var(ENV_GROK_BIN, v),
                    None => std::env::remove_var(ENV_GROK_BIN),
                }
                match &self.prev_cursor {
                    Some(v) => std::env::set_var(ENV_CURSOR_BIN, v),
                    None => std::env::remove_var(ENV_CURSOR_BIN),
                }
                match &self.prev_state {
                    Some(v) => std::env::set_var(crate::config::ENV_COORDINATOR_STATE_DIR, v),
                    None => std::env::remove_var(crate::config::ENV_COORDINATOR_STATE_DIR),
                }
            }
        }
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
            review_comment_routing: false,
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now(),
        }
    }

    fn write_implementor_binding(home: &IsolatedHome) {
        let bin = dummy(home._home.path(), "impl-0099.exe");
        home.write_bindings(|b| {
            if let Some(slot) = b.get_mut(ROLE_IMPLEMENTOR) {
                slot.command = bin.to_string_lossy().into();
            }
        });
    }

    /// Real temp Git target plus a conductor row, for fixtures that actually tick
    /// the adapter into implement/address-findings (0099 §3.1b).
    fn implementation_target(root: &std::path::Path, track: &str) -> std::path::PathBuf {
        let product = root.join("product");
        std::fs::create_dir_all(&product).unwrap();
        let git = |args: &[&str]| {
            let out = crate::git_command::command()
                .args(args)
                .current_dir(&product)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        git(&["init", "-b", "main"]);
        std::fs::write(product.join("f.txt"), "base\n").unwrap();
        git(&["add", "f.txt"]);
        git(&["commit", "-m", "base"]);
        for name in ["implement", "onboarding"] {
            let skill = product.join(".agents").join("skills").join(name);
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(skill.join("SKILL.md"), "skill\n").unwrap();
        }

        let conductor = root.join("conductor");
        std::fs::create_dir_all(conductor.join(format!("{track}-Example"))).unwrap();
        std::fs::write(
            conductor.join("conductor.md"),
            format!(
                "# Conductor\n\n| Track | Execution | Status | Summary |\n| --- | --- | --- | --- |\n| [{track}-Example]({track}-Example/spec.md) | `{}` | **Ready - not started** | target |\n",
                product.display()
            ),
        )
        .unwrap();
        product
    }

    /// Compare the prompt's resolved target by path identity, not raw display text:
    /// Windows tempdirs can be short (`RUNNER~1`) while the resolver canonicalizes to
    /// the long form (`runneradmin`).
    fn assert_prompt_names_execution_repo(prompt: &str, expected: &std::path::Path) {
        let line = prompt
            .lines()
            .find(|line| line.starts_with("Execution repo: "))
            .unwrap_or_else(|| panic!("prompt has no Execution repo line:\n{prompt}"));
        let actual = line.trim_start_matches("Execution repo: ").trim();
        assert!(
            crate::workflow::execution_repo::paths_same(std::path::Path::new(actual), expected),
            "Execution repo line {actual:?} does not name {}",
            expected.display()
        );
    }

    fn plan_state(record: &ProjectRecord, phase: &str, track: &str) -> crate::state::RunState {
        let mut state = crate::state::RunState::idle(&record.id);
        state.phase = phase.into();
        state.track_id = Some(track.into());
        state
    }

    #[test]
    fn plan_refresh_keeps_evidence_body_when_spec_and_plan_exist() {
        let dir = tempdir().unwrap();
        let track = dir.path().join("conductor").join("0076-Example");
        std::fs::create_dir_all(&track).unwrap();
        std::fs::write(track.join("spec.md"), "# spec\n").unwrap();
        std::fs::write(track.join("plan.md"), "# plan\n").unwrap();
        std::fs::write(
            track.join("evidence.md"),
            "coordinator 0.0.1\n2020-01-01T00:00:00Z\nOWNER-MARKER\n",
        )
        .unwrap();
        let record = rec(dir.path());
        let state = plan_state(&record, PHASE_PLAN, "0076");
        refresh_plan_evidence_stamp(&record, &state).unwrap();
        let text = std::fs::read_to_string(track.join("evidence.md")).unwrap();
        let version = crate::workflow::evidence_stamp::version_line();
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some(version.as_str()));
        let stamp = lines.next().unwrap();
        assert_eq!(stamp.len(), 20);
        assert!(stamp.ends_with('Z'));
        assert_ne!(stamp, "2020-01-01T00:00:00Z");
        assert!(text.contains("OWNER-MARKER"));
        assert!(!text.contains("coordinator 0.0.1"));
    }

    #[test]
    fn non_plan_phase_does_not_refresh_evidence() {
        let dir = tempdir().unwrap();
        let track = dir.path().join("conductor").join("0076-Example");
        std::fs::create_dir_all(&track).unwrap();
        std::fs::write(track.join("spec.md"), "# spec\n").unwrap();
        std::fs::write(track.join("plan.md"), "# plan\n").unwrap();
        let body = "coordinator 0.0.1\n2020-01-01T00:00:00Z\nOWNER-MARKER\n";
        std::fs::write(track.join("evidence.md"), body).unwrap();
        let record = rec(dir.path());
        let state = plan_state(&record, "fold", "0076");
        refresh_plan_evidence_stamp(&record, &state).unwrap();
        assert_eq!(
            std::fs::read_to_string(track.join("evidence.md")).unwrap(),
            body
        );
    }

    #[test]
    fn missing_plan_file_skips_evidence_refresh() {
        let dir = tempdir().unwrap();
        let track = dir.path().join("conductor").join("0076-Example");
        std::fs::create_dir_all(&track).unwrap();
        std::fs::write(track.join("spec.md"), "# spec\n").unwrap();
        let body = "keep-me\n";
        std::fs::write(track.join("evidence.md"), body).unwrap();
        let record = rec(dir.path());
        let state = plan_state(&record, PHASE_PLAN, "0076");
        refresh_plan_evidence_stamp(&record, &state).unwrap();
        assert_eq!(
            std::fs::read_to_string(track.join("evidence.md")).unwrap(),
            body
        );
    }

    #[test]
    fn placeholder_plan_still_refreshes_evidence_body() {
        let dir = tempdir().unwrap();
        let track = dir.path().join("conductor").join("0076-Example");
        std::fs::create_dir_all(&track).unwrap();
        std::fs::write(track.join("spec.md"), "# spec\n").unwrap();
        let plan = "> Template. Phased checklist; map each phase to the DoD items in spec.md\n";
        assert!(crate::workflow::prompts::plan_markdown_is_placeholder(plan));
        std::fs::write(track.join("plan.md"), plan).unwrap();
        std::fs::write(
            track.join("evidence.md"),
            "coordinator 0.0.1\n2020-01-01T00:00:00Z\nOWNER-MARKER\n",
        )
        .unwrap();
        let record = rec(dir.path());
        let state = plan_state(&record, PHASE_PLAN, "0076");
        refresh_plan_evidence_stamp(&record, &state).unwrap();
        let text = std::fs::read_to_string(track.join("evidence.md")).unwrap();
        let version = crate::workflow::evidence_stamp::version_line();
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some(version.as_str()));
        let stamp = lines.next().unwrap();
        assert_eq!(stamp.len(), 20);
        assert!(stamp.ends_with('Z'));
        assert_ne!(stamp, "2020-01-01T00:00:00Z");
        assert!(text.contains("OWNER-MARKER"));
        assert!(!text.contains("coordinator 0.0.1"));
    }

    const MINTED_TEMPLATE_PLAN: &str = "# 0000 \u{2014} <Track Title> \u{2014} Plan\n\n> Template. Phased checklist; map each phase to the DoD items in spec.md.\n";

    fn minted_track(root: &std::path::Path, with_skill: bool) -> PathBuf {
        let track = root.join("conductor").join("0001-Minted");
        std::fs::create_dir_all(&track).unwrap();
        std::fs::write(track.join("spec.md"), "# template spec\n").unwrap();
        std::fs::write(track.join("plan.md"), MINTED_TEMPLATE_PLAN).unwrap();
        std::fs::write(
            track.join("evidence.md"),
            "coordinator 0.0.1\n2020-01-01T00:00:00Z\nOWNER-MARKER\n",
        )
        .unwrap();
        if with_skill {
            let skill = root.join(".agents").join("skills").join("plan");
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(skill.join("SKILL.md"), "plan skill\n").unwrap();
        }
        track
    }

    fn planner_home() -> IsolatedHome {
        let home = IsolatedHome::enter();
        let planner = dummy(home._home.path(), "valid-planner.exe");
        home.write_bindings(|b| {
            b.get_mut(ROLE_PLANNER).unwrap().command = planner.to_string_lossy().into();
        });
        home
    }

    fn clear_last_driven(record: &ProjectRecord) {
        crate::state::with_run_state_lock(record, || {
            let mut state = load_run_state(record)?;
            state.last_driven_phase = None;
            crate::state::save_run_state(record, &state)
        })
        .unwrap();
    }

    #[test]
    fn minted_template_tick_expands_then_real_plan_fast_skips() {
        let _home = planner_home();
        let dir = tempdir().unwrap();
        let track = minted_track(dir.path(), true);
        let record = rec(dir.path());
        let original = std::fs::read(track.join("plan.md")).unwrap();
        run_with_driver(&record, Some("0001".into()), WorkflowDriver::Adapter).unwrap();
        // refresh_plan_evidence_stamp runs in drive_adapter before role/binary
        // resolution and before the capture return (stamp call, then prompt build).
        // The evidence assertions depend on that order.
        let _capture = arm_capture_prompts();
        let first = tick(&record).unwrap();
        assert!(first.is_none(), "capture returns before a harness starts");
        let prompts = captured_prompts();
        assert_eq!(prompts.len(), 1, "{prompts:?}");
        let prompt = &prompts[0];
        assert!(prompt.contains("Honor project skills"), "{prompt}");
        assert!(prompt.contains("Write spec.md and plan.md"), "{prompt}");
        assert!(
            prompt.contains("Remove any placeholder-marker line"),
            "{prompt}"
        );
        assert!(!prompt.contains("Do not load the plan skill"), "{prompt}");
        assert!(
            !prompt.contains("placeholder present, skill absent"),
            "{prompt}"
        );
        assert_eq!(std::fs::read(track.join("plan.md")).unwrap(), original);
        let evidence = std::fs::read_to_string(track.join("evidence.md")).unwrap();
        assert!(evidence.contains("OWNER-MARKER"), "{evidence}");
        assert!(
            evidence.contains(&crate::workflow::evidence_stamp::version_line()),
            "{evidence}"
        );
        assert!(!evidence.contains("coordinator 0.0.1"), "{evidence}");
        drop(_capture);

        std::fs::write(track.join("spec.md"), "# spec\n").unwrap();
        std::fs::write(track.join("plan.md"), "# plan\n").unwrap();
        let written = std::fs::read(track.join("plan.md")).unwrap();
        assert_ne!(written, original);
        assert!(!crate::workflow::prompts::plan_markdown_is_placeholder(
            "# plan\n"
        ));

        clear_last_driven(&record);
        let _capture = arm_capture_prompts();
        let second = tick(&record).unwrap();
        assert!(second.is_none());
        let prompts = captured_prompts();
        assert_eq!(prompts.len(), 1, "{prompts:?}");
        let prompt = &prompts[0];
        assert!(prompt.contains("already exist"), "{prompt}");
        assert!(prompt.contains("Do not run cargo"), "{prompt}");
        assert!(prompt.contains("Do not load the plan skill"), "{prompt}");
        assert!(!prompt.contains("Write spec.md and plan.md"), "{prompt}");
        assert_eq!(std::fs::read(track.join("plan.md")).unwrap(), written);
    }

    fn write_plan_skill(root: &std::path::Path) {
        let skill = root.join(".agents").join("skills").join("plan");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "plan skill\n").unwrap();
    }

    #[test]
    fn expand_without_skill_stops_skill_missing() {
        let _home = planner_home();
        let dir = tempdir().unwrap();
        let track = minted_track(dir.path(), false);
        let record = rec(dir.path());
        let original = std::fs::read(track.join("plan.md")).unwrap();
        run_with_driver(&record, Some("0001".into()), WorkflowDriver::Adapter).unwrap();
        let _capture = arm_capture_prompts();
        let view = tick(&record).unwrap().expect("skill missing");
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.failure_class, Some(FailureClass::SkillMissing));
        let marker = view
            .last_event
            .find("skill_missing:")
            .expect(&view.last_event);
        assert!(
            view.last_event[..marker].trim_end().ends_with('\u{2014}'),
            "{}",
            view.last_event
        );
        assert!(
            view.last_event.contains("plan") && view.last_event.contains("SKILL.md"),
            "{}",
            view.last_event
        );
        assert!(captured_prompts().is_empty(), "{:?}", captured_prompts());
        assert_eq!(std::fs::read(track.join("plan.md")).unwrap(), original);
        let evidence = std::fs::read_to_string(track.join("evidence.md")).unwrap();
        assert!(evidence.contains("OWNER-MARKER"), "{evidence}");
        assert!(
            evidence.contains(&crate::workflow::evidence_stamp::version_line()),
            "{evidence}"
        );
        assert!(!evidence.contains("2020-01-01T00:00:00Z"), "{evidence}");
        let state = load_run_state(&record).unwrap();
        assert!(state.last_driven_phase.is_none());
    }

    #[test]
    fn fast_skip_without_skill_file_still_injects() {
        let _home = planner_home();
        let dir = tempdir().unwrap();
        let track = minted_track(dir.path(), false);
        std::fs::write(track.join("plan.md"), "# plan\n").unwrap();
        let record = rec(dir.path());
        run_with_driver(&record, Some("0001".into()), WorkflowDriver::Adapter).unwrap();
        let _capture = arm_capture_prompts();
        let view = tick(&record).unwrap();
        assert!(view.is_none(), "fast-skip still injects");
        let prompts = captured_prompts();
        assert_eq!(prompts.len(), 1, "{prompts:?}");
        assert!(prompts[0].contains("already exist"), "{}", prompts[0]);
        assert!(!prompts[0].contains("skill_missing"), "{}", prompts[0]);
        let state = load_run_state(&record).unwrap();
        assert_eq!(state.status, RunStatus::Running);
        assert!(state.failure_class.is_none());
    }

    #[test]
    fn prefixed_start_error_retries_without_artifact() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0064".into()), WorkflowDriver::Adapter).unwrap();
        crate::state::with_run_state_lock(&r, || {
            let mut s = load_run_state(&r)?;
            s.phase = crate::workflow::graph::PHASE_IMPLEMENT.into();
            s.last_driven_phase = Some("implement".into());
            crate::state::save_run_state(&r, &s)
        })
        .unwrap();
        finish_adapter_inject(
            &r,
            Err(CoordinatorError::Message(
                "ACP stdout closed during initialize; child=exited:42; stderr=".into(),
            )),
            None,
        );
        let s = load_run_state(&r).unwrap();
        assert_eq!(s.status, crate::state::RunStatus::Running);
        assert_eq!(s.phase, "implement");
        assert_eq!(s.acp_stdout_retries, 1);
        assert!(s.last_driven_phase.is_none());
        assert!(
            s.last_event.contains("recycle: acp-stdout 1/2"),
            "{}",
            s.last_event
        );
        assert!(s.last_event.contains("child=exited:42"), "{}", s.last_event);
        assert!(s.last_event.contains('\u{2014}'), "{}", s.last_event);
        assert!(crate::notify::artifact::existing_path(&r).is_none());
    }

    #[test]
    fn finish_adapter_inject_stale_identity_does_not_fail_newer_run() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0070".into()), WorkflowDriver::Adapter).unwrap();
        let state = load_run_state(&r).unwrap();
        let prepared = crate::workflow::execution_repo::PreparedInjection {
            project_id: state.project_id.clone(),
            run_epoch: state.run_epoch,
            phase: crate::workflow::graph::PHASE_IMPLEMENT.into(),
            track_id: state.track_id.clone(),
            target: None,
        };
        crate::state::with_run_state_lock(&r, || {
            let mut next = load_run_state(&r)?;
            next.run_epoch += 1;
            next.phase = crate::workflow::graph::PHASE_IMPLEMENT.into();
            crate::state::save_run_state(&r, &next)
        })
        .unwrap();
        finish_adapter_inject(
            &r,
            Err(CoordinatorError::Message(
                "replacement holder failed".into(),
            )),
            Some(&prepared),
        );
        let next = load_run_state(&r).unwrap();
        assert_eq!(next.status, crate::state::RunStatus::Running);
        assert_eq!(next.failure_class, None);
        assert!(crate::notify::artifact::existing_path(&r).is_none());
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap_or_default();
        assert!(log.contains("stale-injection"), "{log}");
    }

    #[test]
    fn non_prefix_start_error_is_immediate_harness_crash() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0064".into()), WorkflowDriver::Adapter).unwrap();
        finish_adapter_inject(
            &r,
            Err(CoordinatorError::Message(
                "terminal host probe exit 1 (no usable shell for terminal/create)".into(),
            )),
            None,
        );
        let s = load_run_state(&r).unwrap();
        assert_eq!(s.status, crate::state::RunStatus::Stopped);
        assert_eq!(s.failure_class, Some(FailureClass::HarnessCrash));
        assert_eq!(s.acp_stdout_retries, 0);
        assert!(crate::notify::artifact::existing_path(&r).is_some());
    }

    #[test]
    fn transport_drop_retries_without_artifact() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0091".into()), WorkflowDriver::Adapter).unwrap();
        crate::state::with_run_state_lock(&r, || {
            let mut s = load_run_state(&r)?;
            s.phase = crate::workflow::graph::PHASE_IMPLEMENT.into();
            s.last_driven_phase = Some("implement".into());
            crate::state::save_run_state(&r, &s)
        })
        .unwrap();
        finish_adapter_inject(
            &r,
            Err(CoordinatorError::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "reset",
            ))),
            None,
        );
        let s = load_run_state(&r).unwrap();
        assert_eq!(s.status, crate::state::RunStatus::Running);
        assert_eq!(s.phase, "implement");
        assert_eq!(s.acp_stdout_retries, 1);
        assert!(s.last_driven_phase.is_none());
        assert!(
            s.last_event.contains("recycle: transport 1/2"),
            "{}",
            s.last_event
        );
        assert!(
            s.last_event.contains("kind=ConnectionReset"),
            "{}",
            s.last_event
        );
        assert!(s.last_event.contains('\u{2014}'), "{}", s.last_event);
        assert!(crate::notify::artifact::existing_path(&r).is_none());
    }

    #[test]
    fn transport_drop_at_cap_prefixes_green_pr() {
        let _pr = crate::ci::set_test_open_green_pr(Some(91));
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0091".into()), WorkflowDriver::Adapter).unwrap();
        crate::state::with_run_state_lock(&r, || {
            let mut s = load_run_state(&r)?;
            s.phase = crate::workflow::graph::PHASE_IMPLEMENT.into();
            s.acp_stdout_retries = 2;
            crate::state::save_run_state(&r, &s)
        })
        .unwrap();
        finish_adapter_inject(
            &r,
            Err(CoordinatorError::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "reset",
            ))),
            None,
        );
        let s = load_run_state(&r).unwrap();
        assert_eq!(s.status, crate::state::RunStatus::Stopped);
        assert_eq!(s.failure_class, Some(FailureClass::HarnessCrash));
        assert_eq!(s.acp_stdout_retries, 2);
        assert!(
            s.last_event.contains("PR #91 green; merge not performed"),
            "{}",
            s.last_event
        );
        assert!(s.last_event.contains("I/O error:"), "{}", s.last_event);
        assert!(!s.last_event.starts_with("recycle:"), "{}", s.last_event);
        assert!(crate::notify::artifact::existing_path(&r).is_some());
    }

    fn dummy(dir: &std::path::Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, b"").unwrap();
        p
    }

    #[test]
    fn rebound_planner_permission_names_that_command_not_implementor() {
        let home = IsolatedHome::enter();
        let planner = home._home.path().join("rebound-planner-only.exe");
        let implementor = dummy(home._home.path(), "rebound-implementor.exe");
        home.write_bindings(|b| {
            b.get_mut(ROLE_PLANNER).unwrap().command = planner.to_string_lossy().into();
            b.get_mut(ROLE_IMPLEMENTOR).unwrap().command = implementor.to_string_lossy().into();
        });
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0019".into()), WorkflowDriver::Adapter).unwrap();
        let view = tick(&r).unwrap().expect("permission");
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.failure_class, Some(FailureClass::Permission));
        assert!(
            view.last_event.contains("planner") && view.last_event.contains("rebound-planner-only"),
            "last_event={}",
            view.last_event
        );
        assert!(
            !view.last_event.contains("rebound-implementor"),
            "plan must not use implementor: {}",
            view.last_event
        );
        assert_eq!(
            view.workflow.as_ref().unwrap().id.as_deref(),
            Some(WORKFLOW_ID)
        );
    }

    #[test]
    fn non_grok_harness_is_permission_no_inject() {
        let home = IsolatedHome::enter();
        home.write_bindings(|b| {
            b.get_mut(ROLE_PLANNER).unwrap().harness = "opencode".into();
            b.get_mut(ROLE_PLANNER).unwrap().command = "opencode".into();
        });
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0019".into()), WorkflowDriver::Adapter).unwrap();
        let view = tick(&r).unwrap().expect("permission");
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.failure_class, Some(FailureClass::Permission));
        assert!(
            view.last_event.contains("no long-lived adapter")
                && view.last_event.contains("opencode")
                && view.last_event.contains(PHASE_PLAN)
                && view.last_event.contains("planner"),
            "last_event={}",
            view.last_event
        );
        let state = load_run_state(&r).unwrap();
        assert!(
            state.last_driven_phase.is_none(),
            "non-grok must not mark_driven / start an inject thread"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn non_grok_harness_does_not_prompt_live_grok() {
        let home = IsolatedHome::enter();
        home.write_bindings(|b| {
            b.get_mut(ROLE_PLANNER).unwrap().harness = "opencode".into();
            b.get_mut(ROLE_PLANNER).unwrap().command = "opencode".into();
        });
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0019".into()), WorkflowDriver::Adapter).unwrap();
        let session = crate::harness::GrokSession::start_mock(
            crate::harness::grok_cwd(&r).expect("cwd"),
            mock_handshake_ok("sess-live-nongrok"),
            std::time::Duration::from_secs(2),
        )
        .await
        .unwrap();
        crate::harness::pool::insert_test_session(r.id.clone(), session).await;
        let written_before = {
            let mut pool = crate::harness::global_pool().lock().await;
            let s = pool.get_mut(&r.id).unwrap();
            s.mock_written().unwrap().len()
        };
        let view = tick(&r).unwrap().expect("permission");
        assert_eq!(view.failure_class, Some(FailureClass::Permission));
        assert!(view.last_event.contains("no long-lived adapter"));
        let written_after = {
            let mut pool = crate::harness::global_pool().lock().await;
            let s = pool.get_mut(&r.id).unwrap();
            s.mock_written().unwrap().len()
        };
        assert_eq!(
            written_before, written_after,
            "must not session/prompt the existing Grok session"
        );
    }

    #[test]
    fn is_acp_session_harness_cursor_is_true() {
        assert!(crate::harness::is_acp_session_harness("cursor"));
        assert!(crate::harness::is_acp_session_harness("CURSOR"));
        assert!(crate::harness::is_acp_session_harness("grok"));
        assert!(crate::harness::is_acp_session_harness("claude"));
        assert!(crate::harness::is_acp_session_harness("codex"));
        assert!(!crate::harness::is_acp_session_harness("opencode"));
    }

    #[test]
    fn cursor_unresolvable_is_permission_without_no_adapter() {
        let home = IsolatedHome::enter();
        home.write_bindings(|b| {
            b.get_mut(ROLE_PLANNER).unwrap().harness = "cursor".into();
            b.get_mut(ROLE_PLANNER).unwrap().command =
                r"C:\this\does\not\exist-cursor-planner.exe".into();
        });
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0038".into()), WorkflowDriver::Adapter).unwrap();
        let view = tick(&r).unwrap().expect("permission");
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.failure_class, Some(FailureClass::Permission));
        assert!(
            !view.last_event.contains("no long-lived adapter"),
            "cursor resolve failure must not use the non-ACP adapter sentence: {}",
            view.last_event
        );
        assert!(
            view.last_event.contains("planner")
                || view.last_event.contains("not resolvable")
                || view.last_event.contains("not found"),
            "last_event={}",
            view.last_event
        );
        let state = load_run_state(&r).unwrap();
        assert!(
            state.last_driven_phase.is_none(),
            "unresolvable cursor must not mark_driven"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn cursor_live_grok_mismatch_does_not_prompt() {
        let home = IsolatedHome::enter();
        let stub = home._home.path().join("cursor-dummy.cmd");
        std::fs::write(&stub, "@echo off\r\nexit /b 1\r\n").unwrap();
        home.write_bindings(|b| {
            b.get_mut(ROLE_PLANNER).unwrap().harness = "cursor".into();
            b.get_mut(ROLE_PLANNER).unwrap().command = stub.to_string_lossy().into();
        });
        let dir = tempdir().unwrap();
        let mut reg = Registry::default();
        let r = reg.add(dir.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&registry_path().unwrap()).unwrap();
        write_plan_skill(dir.path());
        run_with_driver(&r, Some("0038".into()), WorkflowDriver::Adapter).unwrap();
        let session = crate::harness::GrokSession::start_mock(
            crate::harness::grok_cwd(&r).expect("cwd"),
            mock_handshake_ok("sess-live-grok-mismatch"),
            std::time::Duration::from_secs(2),
        )
        .await
        .unwrap();
        crate::harness::pool::insert_test_session(r.id.clone(), session).await;
        let written_before = {
            let mut pool = crate::harness::global_pool().lock().await;
            let s = pool.get_mut(&r.id).unwrap();
            s.mock_written().unwrap().len()
        };
        assert!(
            written_before > 0,
            "mock grok handshake should have written ACP lines"
        );
        let first = tick(&r).unwrap();
        assert!(first.is_none(), "inject is fire-and-forget after recycle");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut recycled = false;
        while std::time::Instant::now() < deadline {
            if !crate::harness::global_pool().lock().await.contains(&r.id) {
                recycled = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            recycled,
            "cursor tick must recycle the in-process grok mock (no session/prompt)"
        );
        let persist_path = crate::harness::persist_path(&r).unwrap();
        let persist_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut adapter = String::new();
        while std::time::Instant::now() < persist_deadline {
            if let Ok(text) = std::fs::read_to_string(&persist_path)
                && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
            {
                adapter = v
                    .get("adapter")
                    .and_then(|a| a.as_str())
                    .unwrap_or("")
                    .to_string();
                if adapter.eq_ignore_ascii_case("cursor") {
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            adapter.eq_ignore_ascii_case("cursor"),
            "persist adapter must flip to cursor after mismatch recycle, got {adapter:?}"
        );
    }

    #[test]
    fn implementor_unresolvable_does_not_fail_plan_via_default_resolve() {
        let home = IsolatedHome::enter();
        let planner = dummy(home._home.path(), "valid-planner.exe");
        let implementor = home._home.path().join("broken-implementor.exe");
        home.write_bindings(|b| {
            b.get_mut(ROLE_PLANNER).unwrap().command = planner.to_string_lossy().into();
            b.get_mut(ROLE_IMPLEMENTOR).unwrap().command = implementor.to_string_lossy().into();
        });
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        write_plan_skill(dir.path());
        run_with_driver(&r, Some("0019".into()), WorkflowDriver::Adapter).unwrap();
        let first = tick(&r).unwrap();
        assert!(first.is_none(), "inject is fire-and-forget");
        let state = load_run_state(&r).unwrap();
        assert_eq!(state.status, RunStatus::Running);
        assert_eq!(state.last_driven_phase.as_deref(), Some(PHASE_PLAN));
        let view = crate::run::status(&r).unwrap();
        assert_ne!(view.failure_class, Some(FailureClass::Permission));
        assert!(
            !view.last_event.contains("broken-implementor"),
            "plan must not fail via implementor-first resolve: {}",
            view.last_event
        );
        let _ = poll_once(&r);
    }

    #[test]
    fn empty_command_permission_no_hang() {
        let home = IsolatedHome::enter();
        home.write_bindings(|b| {
            b.get_mut(ROLE_PLANNER).unwrap().command.clear();
        });
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0019".into()), WorkflowDriver::Adapter).unwrap();
        let view = tick(&r).unwrap().expect("permission");
        assert_eq!(view.failure_class, Some(FailureClass::Permission));
        assert!(
            view.last_event.contains("planner"),
            "last_event={}",
            view.last_event
        );
        assert_eq!(poll_once(&r).unwrap(), None);
    }

    #[test]
    fn stub_and_file_wait_do_not_resolve_rebound_binary() {
        let home = IsolatedHome::enter();
        home.write_bindings(|b| {
            b.get_mut(ROLE_PLANNER).unwrap().command =
                r"C:\this\does\not\exist-stub-planner.exe".into();
        });
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0019".into()), WorkflowDriver::Stub).unwrap();
        let view = tick(&r).unwrap().expect("stub success");
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.phase, crate::workflow::graph::PHASE_PLAN_REVIEW);
        assert!(view.failure_class.is_none());

        let dir2 = tempdir().unwrap();
        let r2 = rec(dir2.path());
        run_with_driver(&r2, Some("0019".into()), WorkflowDriver::FileWait).unwrap();
        assert!(tick(&r2).unwrap().is_none());
        let s2 = load_run_state(&r2).unwrap();
        assert_eq!(s2.status, RunStatus::Running);
        assert_eq!(s2.phase, PHASE_PLAN);
        assert!(s2.last_driven_phase.is_none());
    }

    #[test]
    fn optional_fold_key_is_recognized_by_resolve() {
        let home = IsolatedHome::enter();
        let fold = dummy(home._home.path(), "fold-bin.exe");
        home.write_bindings(|b| {
            b.insert(
                ROLE_FOLD.into(),
                RoleBinding {
                    harness: "grok".into(),
                    command: fold.to_string_lossy().into(),
                    model: None,
                },
            );
            b.insert(
                ROLE_NEXT.into(),
                RoleBinding {
                    harness: "grok".into(),
                    command: String::new(),
                    model: None,
                },
            );
        });
        assert_eq!(
            crate::harness::resolve_phase_binary(crate::workflow::graph::PHASE_FOLD).unwrap(),
            fold
        );
        assert_eq!(
            crate::harness::resolve_phase_role_key(
                crate::workflow::graph::PHASE_ADVANCE,
                &crate::harness::load_role_bindings().unwrap()
            )
            .as_deref(),
            Some(ROLE_PLANNER)
        );
    }

    #[test]
    fn self_check_pending_inject_spawns_one_continuation() {
        let _home = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        let mut r = rec(dir.path());
        r.self_continuation = true;
        r.execution_repo = Some(implementation_target(dir.path(), "0070"));
        run_with_driver(&r, Some("0070".into()), WorkflowDriver::Adapter).unwrap();
        crate::state::with_run_state_lock(&r, || {
            let mut state = load_run_state(&r)?;
            state.phase = crate::workflow::graph::PHASE_IMPLEMENT.into();
            state.last_driven_phase = Some(crate::workflow::graph::PHASE_IMPLEMENT.into());
            state.driver = WorkflowDriver::Adapter;
            state.self_check = Some(crate::state::SelfCheckState {
                steps: 1,
                pending_inject: true,
                last_finding: Some("none".into()),
                last_action: Some("progress".into()),
                ..crate::state::SelfCheckState::default()
            });
            crate::state::save_run_state(&r, &state)
        })
        .unwrap();
        let _capture = super::arm_capture_prompts();
        tick(&r).unwrap();
        let prompts = super::captured_prompts();
        assert_eq!(prompts.len(), 1, "{prompts:?}");
        assert!(prompts[0].contains("self-check"), "{}", prompts[0]);
        assert!(prompts[0].contains("step 2 of 8"), "{}", prompts[0]);
        assert!(prompts[0].contains("About 10 minutes"), "{}", prompts[0]);
        let state = load_run_state(&r).unwrap();
        assert!(!state.self_check.unwrap().pending_inject);
        assert_eq!(
            state.last_driven_phase.as_deref(),
            Some(crate::workflow::graph::PHASE_IMPLEMENT)
        );
        let sidecar = crate::workflow::watchdog::progress_path(&r).unwrap();
        let body = std::fs::read_to_string(&sidecar).unwrap();
        assert!(body.contains("inject"), "{body}");
        tick(&r).unwrap();
        assert_eq!(super::captured_prompts().len(), 1);
    }

    #[test]
    fn initial_implement_capture_uses_prepared_target() {
        let _home = IsolatedHome::enter();
        write_implementor_binding(&_home);
        let dir = tempdir().unwrap();
        let mut r = rec(dir.path());
        let product = implementation_target(dir.path(), "0070");
        r.execution_repo = Some(product.clone());
        run_with_driver(&r, Some("0070".into()), WorkflowDriver::Adapter).unwrap();
        crate::state::with_run_state_lock(&r, || {
            let mut state = load_run_state(&r)?;
            state.phase = crate::workflow::graph::PHASE_IMPLEMENT.into();
            state.driver = WorkflowDriver::Adapter;
            state.last_driven_phase = None;
            crate::state::save_run_state(&r, &state)
        })
        .unwrap();
        let _capture = super::arm_capture_prompts();
        tick(&r).unwrap();
        let prompts = super::captured_prompts();
        assert_eq!(prompts.len(), 1, "{prompts:?}");
        assert!(
            prompts[0].contains(&product.display().to_string()),
            "{}",
            prompts[0]
        );
        assert!(load_run_state(&r).unwrap().failure_class.is_none());
    }

    #[test]
    fn invalid_target_before_capture_claims_no_continuation() {
        let _home = IsolatedHome::enter();
        write_implementor_binding(&_home);
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        // No conductor.md row exists for this track.
        run_with_driver(&r, Some("0070".into()), WorkflowDriver::Adapter).unwrap();
        crate::state::with_run_state_lock(&r, || {
            let mut state = load_run_state(&r)?;
            state.phase = crate::workflow::graph::PHASE_IMPLEMENT.into();
            state.driver = WorkflowDriver::Adapter;
            state.last_driven_phase = Some(crate::workflow::graph::PHASE_IMPLEMENT.into());
            state.self_check = Some(crate::state::SelfCheckState {
                steps: 1,
                pending_inject: true,
                ..crate::state::SelfCheckState::default()
            });
            crate::state::save_run_state(&r, &state)
        })
        .unwrap();
        let _capture = super::arm_capture_prompts();
        tick(&r).unwrap();
        assert!(super::captured_prompts().is_empty());
        let state = load_run_state(&r).unwrap();
        assert_eq!(state.status, RunStatus::Stopped);
        assert_eq!(state.failure_class, Some(FailureClass::Permission));
        assert_eq!(
            state.self_check.unwrap().steps,
            1,
            "no continuation claimed"
        );
        let artifact = crate::notify::artifact::read(&r)
            .unwrap()
            .expect("Permission artifact");
        assert!(
            artifact.body.contains("Declare one execution target"),
            "{}",
            artifact.body
        );
        assert!(
            artifact.body.contains("repair conductor.md"),
            "{}",
            artifact.body
        );
        assert!(
            artifact.body.contains("coordinator failure resolve"),
            "{}",
            artifact.body
        );
        assert!(
            artifact.body.contains("coordinator run --track 0070"),
            "{}",
            artifact.body
        );
    }

    #[test]
    fn inprocess_resolution_failure_prevents_inject() {
        let _home = IsolatedHome::enter();
        write_implementor_binding(&_home);
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0070".into()), WorkflowDriver::Adapter).unwrap();
        crate::state::with_run_state_lock(&r, || {
            let mut state = load_run_state(&r)?;
            state.phase = crate::workflow::graph::PHASE_IMPLEMENT.into();
            state.driver = WorkflowDriver::Adapter;
            state.last_driven_phase = None;
            crate::state::save_run_state(&r, &state)
        })
        .unwrap();
        let _capture = super::arm_capture_prompts();
        tick(&r).unwrap();
        assert!(
            super::captured_prompts().is_empty(),
            "zero prepared prompts"
        );
        assert_eq!(
            load_run_state(&r).unwrap().failure_class,
            Some(FailureClass::Permission)
        );
    }

    #[test]
    fn manual_prompt_policy_unchanged() {
        // 0101: address-ci is now a prepared phase (prompt + ACP target root). Raw CLI
        // and HTTP prompts remain outside target resolution and install no root.
        assert!(crate::workflow::execution_repo::prepares_target(
            "address-ci"
        ));
        assert!(crate::workflow::execution_repo::prepares_target(
            "implement"
        ));
        assert!(crate::workflow::execution_repo::prepares_target(
            "address-findings"
        ));
        assert!(!crate::workflow::execution_repo::prepares_target("plan"));
    }

    #[test]
    fn prepared_phase_entry_counts_unchanged_on_invalid_target() {
        for attempts in [1u32, 2] {
            let _home = IsolatedHome::enter();
            let dir = tempdir().unwrap();
            let r = rec(dir.path());
            run_with_driver(&r, Some("0070".into()), WorkflowDriver::Adapter).unwrap();
            crate::state::with_run_state_lock(&r, || {
                let mut state = load_run_state(&r)?;
                state.phase = crate::workflow::graph::PHASE_ADDRESS_FINDINGS.into();
                state.driver = WorkflowDriver::Adapter;
                state.last_driven_phase = None;
                state.address_findings_attempts = attempts;
                crate::state::save_run_state(&r, &state)
            })
            .unwrap();
            let _capture = super::arm_capture_prompts();
            tick(&r).unwrap();
            assert!(super::captured_prompts().is_empty());
            let state = load_run_state(&r).unwrap();
            assert_eq!(state.failure_class, Some(FailureClass::Permission));
            assert_eq!(
                state.address_findings_attempts, attempts,
                "invalid target must not refund or increment entry count"
            );
        }
    }

    #[tokio::test]
    async fn permission_application_outside_locks() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0070".into()), WorkflowDriver::Adapter).unwrap();
        crate::state::with_run_state_lock(&r, || {
            let mut state = load_run_state(&r)?;
            state.phase = crate::workflow::graph::PHASE_IMPLEMENT.into();
            state.driver = WorkflowDriver::Adapter;
            crate::state::save_run_state(&r, &state)
        })
        .unwrap();
        // Another task holds the global pool lock while the failure applies.
        let pool_lock = crate::harness::pool::global_pool().lock().await;
        let owned = r.clone();
        let apply = tokio::task::spawn_blocking(move || crate::workflow::tick(&owned));
        let done = tokio::time::timeout(std::time::Duration::from_secs(5), apply).await;
        assert!(
            done.is_ok(),
            "failure application must not wait on the pool lock"
        );
        drop(pool_lock);
        let state = load_run_state(&r).unwrap();
        assert_eq!(state.status, RunStatus::Stopped);
        assert_eq!(state.failure_class, Some(FailureClass::Permission));
        assert!(crate::notify::artifact::existing_path(&r).is_some());
    }

    #[test]
    fn resolution_failure_is_project_local() {
        let dir_a = tempdir().unwrap();
        let dir_b = tempdir().unwrap();
        let a = rec(dir_a.path());
        let b = rec(dir_b.path());
        run_with_driver(&a, Some("0070".into()), WorkflowDriver::Adapter).unwrap();
        run_with_driver(&b, Some("0070".into()), WorkflowDriver::Adapter).unwrap();
        for rec in [&a, &b] {
            crate::state::with_run_state_lock(rec, || {
                let mut state = load_run_state(rec)?;
                state.phase = crate::workflow::graph::PHASE_IMPLEMENT.into();
                state.driver = WorkflowDriver::Adapter;
                state.last_driven_phase = None;
                crate::state::save_run_state(rec, &state)
            })
            .unwrap();
        }
        let b_before = load_run_state(&b).unwrap();
        let _capture = super::arm_capture_prompts();
        tick(&a).unwrap();
        assert_eq!(
            load_run_state(&a).unwrap().failure_class,
            Some(FailureClass::Permission)
        );
        let b_after = load_run_state(&b).unwrap();
        assert_eq!(b_before.phase, b_after.phase);
        assert_eq!(b_after.failure_class, None);
    }

    #[test]
    fn capture_prompts_are_thread_local() {
        let _guard = arm_capture_prompts();
        std::thread::spawn(|| {
            let other = arm_capture_prompts();
            assert!(captured_prompts().is_empty());
            drop(other);
        })
        .join()
        .unwrap();
        assert!(
            capture_slot_armed(),
            "another thread must not clear this slot"
        );
    }

    #[test]
    fn target_change_between_checkpoint_and_inject_stops_before_prompt() {
        let _home = IsolatedHome::enter();
        write_implementor_binding(&_home);
        let dir = tempdir().unwrap();
        let product = implementation_target(dir.path(), "0071");
        let other = dir.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        let out = crate::git_command::command()
            .args(["init", "-b", "main"])
            .current_dir(&other)
            .output()
            .unwrap();
        assert!(out.status.success());
        let mut r = rec(dir.path());
        r.execution_repo = Some(product.clone());
        run_with_driver(&r, Some("0071".into()), WorkflowDriver::Adapter).unwrap();
        crate::state::with_run_state_lock(&r, || {
            let mut state = load_run_state(&r)?;
            state.phase = crate::workflow::graph::PHASE_IMPLEMENT.into();
            state.last_driven_phase = None;
            state.delivery_target = Some(crate::workflow::execution_repo::seeded_delivery_target(
                &r,
                "0071",
                state.run_epoch,
                crate::workflow::graph::PHASE_IMPLEMENT,
                &other,
            ));
            crate::state::save_run_state(&r, &state)
        })
        .unwrap();
        let _capture = super::arm_capture_prompts();
        let view = tick(&r).unwrap().expect("permission");
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.failure_class, Some(FailureClass::Permission));
        assert!(
            view.last_event
                .contains("execution target changed after checkpoint"),
            "{}",
            view.last_event
        );
        assert!(super::captured_prompts().is_empty(), "zero prompts");
    }

    #[test]
    fn address_ci_uses_the_prepared_target_route() {
        let _home = IsolatedHome::enter();
        write_implementor_binding(&_home);
        let dir = tempdir().unwrap();
        let product = implementation_target(dir.path(), "0071");
        let mut r = rec(dir.path());
        r.execution_repo = Some(product.clone());
        r.ci_fix_routing = true;
        run_with_driver(&r, Some("0071".into()), WorkflowDriver::Adapter).unwrap();
        crate::state::with_run_state_lock(&r, || {
            let mut state = load_run_state(&r)?;
            state.phase = crate::workflow::graph::PHASE_ADDRESS_CI.into();
            state.driver = WorkflowDriver::Adapter;
            state.last_driven_phase = None;
            state.ci_fix_request = Some(crate::state::CiFixRequest {
                pr_number: 71,
                from_sha: "abc111".into(),
                checks: Vec::new(),
            });
            state.delivery_target = Some(crate::workflow::execution_repo::seeded_delivery_target(
                &r,
                "0071",
                state.run_epoch,
                crate::workflow::graph::PHASE_ADDRESS_CI,
                &product,
            ));
            crate::workflow::execution_repo::assert_fixture_has_delivery_target(&state);
            crate::state::save_run_state(&r, &state)
        })
        .unwrap();
        let _capture = super::arm_capture_prompts();
        tick(&r).unwrap();
        let prompts = super::captured_prompts();
        assert_eq!(prompts.len(), 1, "{prompts:?}");
        assert!(prompts[0].contains("address-ci"), "{}", prompts[0]);
        assert_prompt_names_execution_repo(&prompts[0], &product);
        let prepared = super::captured_prepared();
        assert_eq!(prepared.len(), 1, "{prepared:?}");
        let target = prepared[0].target().expect("prepared target");
        assert!(
            crate::workflow::execution_repo::paths_same(&target.prompt_path, &product),
            "ACP target root must be the delivery repo: {}",
            target.prompt_path.display()
        );
        let saved = load_run_state(&r).unwrap();
        assert!(saved.failure_class.is_none());
        assert!(
            saved
                .delivery_target
                .as_ref()
                .is_some_and(|t| t.phase == crate::workflow::graph::PHASE_ADDRESS_CI)
        );
    }

    #[test]
    fn address_ci_resolution_failure_is_permission_without_prompt() {
        let _home = IsolatedHome::enter();
        write_implementor_binding(&_home);
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0071".into()), WorkflowDriver::Adapter).unwrap();
        crate::state::with_run_state_lock(&r, || {
            let mut state = load_run_state(&r)?;
            state.phase = crate::workflow::graph::PHASE_ADDRESS_CI.into();
            state.driver = WorkflowDriver::Adapter;
            state.last_driven_phase = None;
            state.delivery_target = None;
            crate::state::save_run_state(&r, &state)
        })
        .unwrap();
        let _capture = super::arm_capture_prompts();
        tick(&r).unwrap();
        assert!(super::captured_prompts().is_empty(), "zero prompts");
        let saved = load_run_state(&r).unwrap();
        assert_eq!(saved.failure_class, Some(FailureClass::Permission));
        assert!(
            saved.last_event.contains("leg repo unresolved:"),
            "{}",
            saved.last_event
        );
    }

    #[test]
    fn address_ci_prompt_names_the_target() {
        let dir = tempdir().unwrap();
        let product = implementation_target(dir.path(), "0071");
        let r = rec(dir.path());
        let target = crate::workflow::execution_repo::seeded_delivery_target(
            &r,
            "0071",
            1,
            crate::workflow::graph::PHASE_ADDRESS_CI,
            &product,
        );
        let prompt = crate::workflow::prompts::phase_prompt_with_target(
            &r,
            crate::workflow::graph::PHASE_ADDRESS_CI,
            Some("0071"),
            Some(&target),
        );
        assert_prompt_names_execution_repo(&prompt, &product);
    }

    #[test]
    fn address_ci_installs_the_target_root() {
        let _home = IsolatedHome::enter();
        write_implementor_binding(&_home);
        let dir = tempdir().unwrap();
        let product = implementation_target(dir.path(), "0071");
        let mut r = rec(dir.path());
        r.execution_repo = Some(product.clone());
        run_with_driver(&r, Some("0071".into()), WorkflowDriver::Adapter).unwrap();
        crate::state::with_run_state_lock(&r, || {
            let mut state = load_run_state(&r)?;
            state.phase = crate::workflow::graph::PHASE_ADDRESS_CI.into();
            state.last_driven_phase = None;
            state.delivery_target = Some(crate::workflow::execution_repo::seeded_delivery_target(
                &r,
                "0071",
                state.run_epoch,
                crate::workflow::graph::PHASE_ADDRESS_CI,
                &product,
            ));
            crate::state::save_run_state(&r, &state)
        })
        .unwrap();
        let _capture = super::arm_capture_prompts();
        tick(&r).unwrap();
        let prepared = super::captured_prepared();
        assert_eq!(prepared.len(), 1, "{prepared:?}");
        assert!(
            prepared[0]
                .target()
                .is_some_and(|t| crate::workflow::execution_repo::paths_same(
                    &t.prompt_path,
                    &product
                )),
            "the prepared route must install the target root"
        );
    }

    #[test]
    fn address_ci_injects_one_target_aware_prompt_and_does_not_merge() {
        let _home = IsolatedHome::enter();
        let prev_poll = std::env::var_os(crate::config::ENV_COORDINATOR_CI_POLL_MS);
        let prev_notify = std::env::var_os(crate::notify::ENV_COORDINATOR_NOTIFY);
        let prev_fix = std::env::var_os(crate::ci::fix::ENV_CI_FIX);
        let prev_policies = std::env::var_os(crate::policy::ENV_STATE_POLICIES);
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_CI_POLL_MS, "1");
            std::env::set_var(crate::notify::ENV_COORDINATOR_NOTIFY, "off");
            std::env::remove_var(crate::ci::fix::ENV_CI_FIX);
            std::env::remove_var(crate::policy::ENV_STATE_POLICIES);
        }
        struct Restore {
            poll: Option<std::ffi::OsString>,
            notify: Option<std::ffi::OsString>,
            fix: Option<std::ffi::OsString>,
            policies: Option<std::ffi::OsString>,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                fn put(key: &str, prev: &Option<std::ffi::OsString>) {
                    unsafe {
                        match prev {
                            Some(v) => std::env::set_var(key, v),
                            None => std::env::remove_var(key),
                        }
                    }
                }
                put(crate::config::ENV_COORDINATOR_CI_POLL_MS, &self.poll);
                put(crate::notify::ENV_COORDINATOR_NOTIFY, &self.notify);
                put(crate::ci::fix::ENV_CI_FIX, &self.fix);
                put(crate::policy::ENV_STATE_POLICIES, &self.policies);
            }
        }
        let _restore = Restore {
            poll: prev_poll,
            notify: prev_notify,
            fix: prev_fix,
            policies: prev_policies,
        };
        let implementor = dummy(_home._home.path(), "address-ci-implementor.exe");
        _home.write_bindings(|b| {
            b.get_mut(ROLE_IMPLEMENTOR).unwrap().command = implementor.to_string_lossy().into();
        });
        let dir = tempdir().unwrap();
        let product = implementation_target(dir.path(), "0071");
        let mut r = rec(dir.path());
        r.execution_repo = Some(product.clone());
        r.ci_fix_routing = true;
        run_with_driver(&r, Some("0071".into()), WorkflowDriver::Adapter).unwrap();
        crate::state::with_run_state_lock(&r, || {
            let mut state = load_run_state(&r)?;
            state.phase = crate::workflow::graph::PHASE_CI_WAIT.into();
            state.last_driven_phase = None;
            crate::state::save_run_state(&r, &state)
        })
        .unwrap();
        let scripted = crate::ci::ScriptedBackend::new();
        scripted.push_resolve(Ok(Some(crate::ci::CiTarget::PullRequest {
            number: 71,
            url: "https://example/pr/71".into(),
            is_draft: false,
            merged: false,
            closed: false,
            head_oid: Some("abc111".into()),
            merge_state: crate::ci::MergeStateStatus::Clean,
            head_ref: "track/0071-CiFailureRoutesToImplementer".into(),
            title: "track(0071): route ci".into(),
        })));
        scripted.push_snapshot(Ok(crate::ci::CheckSnapshot {
            items: vec![crate::ci::CheckItem {
                name: "fmt".into(),
                bucket: crate::ci::CheckBucket::Fail,
                description: "rustfmt failed".into(),
                link: "https://example/checks/fmt".into(),
            }],
            raw_exit: 0,
            merge_state: crate::ci::MergeStateStatus::Clean,
            view: crate::ci::CheckView::Required,
            advisory: Vec::new(),
        }));
        let recorded = crate::ci::RecordingBackend::wrap(std::sync::Arc::new(scripted));
        let counts = recorded.counts.clone();
        let _backend = crate::ci::install_test_backend(std::sync::Arc::new(recorded));
        let routed = tick(&r).unwrap().expect("routed");
        assert_eq!(routed.phase, crate::workflow::graph::PHASE_ADDRESS_CI);
        assert_eq!(counts.merge_n(), 0);
        assert_eq!(counts.publish_n(), 0);
        let _capture = super::arm_capture_prompts();
        tick(&r).unwrap();
        let prompts = super::captured_prompts();
        assert_eq!(prompts.len(), 1, "{prompts:?}");
        let prompt = &prompts[0];
        assert!(prompt.contains("attempt 1 of 2"), "{prompt}");
        assert!(prompt.contains("fmt"), "{prompt}");
        assert!(prompt.contains("rustfmt failed"), "{prompt}");
        assert!(prompt.contains("https://example/checks/fmt"), "{prompt}");
        assert!(prompt.contains("bucket=fail"), "{prompt}");
        assert!(prompt.contains("git push"), "{prompt}");
        assert!(prompt.contains("gh pr create"), "{prompt}");
        assert!(prompt.contains("gh pr merge"), "{prompt}");
        assert!(!prompt.contains("self-check:"), "{prompt}");
        assert_prompt_names_execution_repo(prompt, &product);
        let prepared = super::captured_prepared();
        assert_eq!(prepared.len(), 1, "{prepared:?}");
        assert!(
            prepared[0]
                .target()
                .is_some_and(|t| crate::workflow::execution_repo::paths_same(
                    &t.prompt_path,
                    &product
                )),
            "prepared prompt must install the target root"
        );
        tick(&r).unwrap();
        assert_eq!(super::captured_prompts().len(), 1);
        let saved = load_run_state(&r).unwrap();
        crate::workflow::execution_repo::assert_fixture_has_delivery_target(&saved);
        assert_eq!(saved.address_findings_attempts, 0);
        assert_eq!(saved.ci_fix_attempts, 1);
        assert_eq!(
            saved.last_driven_phase.as_deref(),
            Some(crate::workflow::graph::PHASE_ADDRESS_CI)
        );
    }

    #[test]
    fn timeout_join_writes_reuse_for_the_produced_slot() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        let track = dir.path().join("conductor").join("0083-Example");
        std::fs::create_dir_all(&track).unwrap();
        std::fs::write(track.join("spec.md"), b"spec").unwrap();
        std::fs::write(track.join("plan.md"), b"plan").unwrap();
        let skill = dir
            .path()
            .join(".agents")
            .join("skills")
            .join("review-track")
            .join("SKILL.md");
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(&skill, b"skill").unwrap();
        std::fs::write(track.join("agy-review.md"), b"agy produced\n").unwrap();
        let state_review = crate::workflow::bundle::review_file(&r, "agy").unwrap();
        std::fs::create_dir_all(state_review.parent().unwrap()).unwrap();
        std::fs::write(&state_review, b"agy produced\n").unwrap();
        let mut state = crate::state::RunState::idle(&r.id);
        state.status = crate::state::RunStatus::Running;
        state.phase = super::PHASE_PLAN_REVIEW.into();
        state.track_id = Some("0083".into());
        state.pending_roles = vec!["opencode".into()];
        state.run_epoch = 1;
        state.adopt_plan_review_entry();
        // The leftover resolves as `ran; produced no review` (not never-spawned), so the timeout
        // helper takes the degrade-success arm.
        state.plan_review_slot_ran = vec!["opencode".into()];
        crate::state::save_run_state(&r, &state).unwrap();
        let outcome = match timeout_plan_review_outcome(&r, &state).unwrap() {
            TimeoutDecision::Apply(outcome) => outcome,
            other => panic!("expected a timeout decision, got {other:?}"),
        };
        assert_eq!(outcome.source, crate::outcome::OutcomeSource::Timeout);
        let msg = outcome.message.unwrap_or_default();
        assert!(
            msg.contains("opencode (timeout; ran; produced no review)"),
            "{msg}"
        );
        let parsed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(track.join("reuse.json")).unwrap()).unwrap();
        assert!(parsed["slots"]["agy"]["review_sha256"].is_string());
        assert!(parsed["slots"].get("opencode").is_none());
        assert_eq!(parsed["epoch"], 1);
        assert!(parsed.get("pr").is_none());
    }

    const READY_CELL: &str = "**Ready — not started**";
    const COMPLETED_CELL: &str = "**Completed** 2026-10-12 — PR **#87** squash `b9dc76e`";

    fn write_registry(ws: &std::path::Path, rows: &str) {
        let cond = ws.join("conductor");
        std::fs::create_dir_all(&cond).unwrap();
        let md = format!(
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             {rows}"
        );
        std::fs::write(cond.join("conductor.md"), md).unwrap();
    }

    fn linked_row(id: &str, status: &str) -> String {
        format!("| [{id}-T]({id}-T/spec.md) | `.` | {status} | go |\n")
    }

    fn seed_snapshot(record: &ProjectRecord, id: &str, cell: &str) {
        let mut state = crate::state::RunState::idle(&record.id);
        state
            .registry_status_before
            .insert(id.to_string(), cell.to_string());
        crate::state::save_run_state(record, &state).unwrap();
    }

    fn git_ok(cwd: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Local copy of the private checkpoint test helper. Not exported.
    fn init_exec_repo() -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        git_ok(dir.path(), &["init", "-b", "main"]);
        git_ok(
            dir.path(),
            &["config", "user.email", "restore-test@example.com"],
        );
        git_ok(dir.path(), &["config", "user.name", "restore-test"]);
        std::fs::write(
            dir.path().join(".gitignore"),
            ".ledgerful/\n.env\n.coordinator/\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("README.md"), b"seed\n").unwrap();
        git_ok(dir.path(), &["add", ".gitignore", "README.md"]);
        git_ok(dir.path(), &["commit", "-m", "seed"]);
        dir
    }

    #[test]
    fn snapshot_consumed_when_terminal_row_is_left() {
        let _home = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        let record = rec(dir.path());
        write_registry(dir.path(), &linked_row("0090", COMPLETED_CELL));
        seed_snapshot(&record, "0090", READY_CELL);
        let ready = restore_ready_rows_after_phase(&record);
        assert!(ready.consumed);
        assert!(ready.restored.is_empty());
        let text =
            std::fs::read_to_string(dir.path().join("conductor").join("conductor.md")).unwrap();
        assert!(text.contains(COMPLETED_CELL), "{text}");
        assert!(
            crate::state::load_run_state(&record)
                .unwrap()
                .registry_status_before
                .contains_key("0090")
        );
        let log = std::fs::read_to_string(dir.path().join("status.md")).unwrap_or_default();
        assert!(!log.contains("advance-downgrade-restored"), "{log}");
    }

    #[test]
    fn snapshot_consumed_when_row_stays_ready() {
        let _home = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        let record = rec(dir.path());
        write_registry(dir.path(), &linked_row("0090", READY_CELL));
        seed_snapshot(&record, "0090", READY_CELL);
        let ready = restore_ready_rows_after_phase(&record);
        assert!(ready.consumed);
        assert!(ready.restored.is_empty());
        assert!(
            crate::state::load_run_state(&record)
                .unwrap()
                .registry_status_before
                .contains_key("0090")
        );
        let log = std::fs::read_to_string(dir.path().join("status.md")).unwrap_or_default();
        assert!(!log.contains("advance-downgrade-restored"), "{log}");
    }

    #[test]
    fn snapshot_kept_when_conductor_missing() {
        let _home = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        let record = rec(dir.path());
        seed_snapshot(&record, "0090", READY_CELL);
        let ready = restore_ready_rows_after_phase(&record);
        assert!(!ready.consumed);
        assert!(ready.restored.is_empty());
        assert_eq!(
            crate::state::load_run_state(&record)
                .unwrap()
                .registry_status_before
                .get("0090")
                .map(String::as_str),
            Some(READY_CELL)
        );
    }

    #[test]
    fn snapshot_kept_when_conductor_unparseable() {
        let _home = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        let record = rec(dir.path());
        let cond = dir.path().join("conductor");
        std::fs::create_dir_all(&cond).unwrap();
        std::fs::write(cond.join("conductor.md"), "not a table\n").unwrap();
        seed_snapshot(&record, "0090", READY_CELL);
        let ready = restore_ready_rows_after_phase(&record);
        assert!(!ready.consumed);
        assert!(
            crate::state::load_run_state(&record)
                .unwrap()
                .registry_status_before
                .contains_key("0090")
        );
    }

    #[test]
    fn proposed_downgrade_restores_and_consumes_snapshot() {
        let _home = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        let record = rec(dir.path());
        write_registry(dir.path(), &linked_row("0090", "**Proposed**"));
        seed_snapshot(&record, "0090", READY_CELL);
        let ready = restore_ready_rows_after_phase(&record);
        assert!(ready.consumed);
        assert_eq!(ready.restored, vec!["0090".to_string()]);
        let text =
            std::fs::read_to_string(dir.path().join("conductor").join("conductor.md")).unwrap();
        let rows = crate::workflow::conductor_md::parse_conductor_md(&text).unwrap();
        assert!(crate::workflow::conductor_md::is_eligible_ready(
            &rows[0].status_raw
        ));
        assert!(
            crate::state::load_run_state(&record)
                .unwrap()
                .registry_status_before
                .contains_key("0090")
        );
        let log = std::fs::read_to_string(dir.path().join("status.md")).unwrap();
        assert!(log.contains("advance-downgrade-restored"), "{log}");
        assert!(log.contains("0090"), "{log}");
    }

    /// 0098: the fold wrote `Proposed - spec section 7 prerequisites remain open`, and the advance
    /// step replayed the pre-run `Ready` two seconds later. Implement then ran against unmet
    /// prerequisites, correctly wrote nothing, and was scored `harness_crash` - three times.
    /// A reasoned downgrade is a decision the phase just made, not lost bookkeeping.
    #[test]
    fn reasoned_proposed_downgrade_is_not_restored() {
        let _home = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        let record = rec(dir.path());
        write_registry(
            dir.path(),
            &linked_row(
                "0090",
                "**Proposed - spec section 7 prerequisites remain open**",
            ),
        );
        seed_snapshot(&record, "0090", READY_CELL);
        let ready = restore_ready_rows_after_phase(&record);
        let text =
            std::fs::read_to_string(dir.path().join("conductor").join("conductor.md")).unwrap();
        assert!(
            text.contains("prerequisites remain open"),
            "the reason must survive the advance step: {text}"
        );
        assert!(
            ready.restored.is_empty(),
            "a reasoned downgrade must not be replayed to Ready, restored {:?}",
            ready.restored
        );
    }

    /// The narrow counterpart: a bare `Proposed` is still the 0089 "re-plan me" marker and must
    /// keep being restored. If this ever fails, the fix above has widened too far.
    #[test]
    fn bare_proposed_still_restores() {
        let _home = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        let record = rec(dir.path());
        write_registry(dir.path(), &linked_row("0090", "**Proposed**"));
        seed_snapshot(&record, "0090", READY_CELL);
        let ready = restore_ready_rows_after_phase(&record);
        assert_eq!(ready.restored, vec!["0090".to_string()]);
    }

    #[test]
    fn fold_consume_survives_apply_and_completed_survives_implement() {
        let _home = IsolatedHome::enter();
        let ws = tempdir().unwrap();
        let repo = init_exec_repo();
        let mut record = rec(ws.path());
        record.execution_repo = Some(repo.path().to_path_buf());
        record.worktree_isolation = false;
        write_registry(ws.path(), &linked_row("0090", READY_CELL));
        let mut state = crate::state::RunState::idle(&record.id);
        state.status = crate::state::RunStatus::Running;
        state.phase = crate::workflow::graph::PHASE_FOLD.into();
        state.track_id = Some("0090".into());
        state.run_epoch = 3;
        state.workflow = Some(crate::workflow::WORKFLOW_ID.into());
        state
            .registry_status_before
            .insert("0090".into(), READY_CELL.to_string());
        crate::state::save_run_state(&record, &state).unwrap();

        let view = crate::outcome::write_and_apply(
            &record,
            crate::outcome::PhaseOutcome::success(
                crate::workflow::graph::PHASE_FOLD,
                crate::outcome::OutcomeSource::Test,
                None,
                None,
                Some(3),
            ),
        )
        .unwrap();
        assert_eq!(
            view.phase,
            crate::workflow::graph::PHASE_IMPLEMENT,
            "{}",
            view.last_event
        );
        assert_eq!(view.status, crate::state::RunStatus::Running);
        assert!(
            crate::state::load_run_state(&record)
                .unwrap()
                .registry_status_before
                .is_empty()
        );

        write_registry(ws.path(), &linked_row("0090", COMPLETED_CELL));
        // A real adapter implement leg commits its work: `implement-gate` refuses a tree that
        // still matches the epoch checkpoint, so the fixture has to model the commit.
        std::fs::write(repo.path().join("implemented.txt"), b"work\n").unwrap();
        git_ok(repo.path(), &["add", "."]);
        git_ok(repo.path(), &["commit", "-m", "track(0090): work"]);
        let view = crate::outcome::write_and_apply(
            &record,
            crate::outcome::PhaseOutcome::success(
                crate::workflow::graph::PHASE_IMPLEMENT,
                crate::outcome::OutcomeSource::Test,
                None,
                None,
                Some(3),
            ),
        )
        .unwrap();
        assert_eq!(
            view.phase,
            crate::workflow::graph::PHASE_CROSS_MODEL,
            "{}",
            view.last_event
        );
        let text =
            std::fs::read_to_string(ws.path().join("conductor").join("conductor.md")).unwrap();
        assert!(text.contains(COMPLETED_CELL), "{text}");
        assert!(
            crate::state::load_run_state(&record)
                .unwrap()
                .registry_status_before
                .is_empty()
        );
    }

    #[test]
    fn snapshot_skipped_for_plan_and_implement() {
        let _home = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        let record = rec(dir.path());
        write_registry(dir.path(), &linked_row("0090", READY_CELL));
        crate::state::save_run_state(&record, &crate::state::RunState::idle(&record.id)).unwrap();
        snapshot_registry_before_phase(&record, crate::workflow::graph::PHASE_PLAN).unwrap();
        snapshot_registry_before_phase(&record, crate::workflow::graph::PHASE_IMPLEMENT).unwrap();
        assert!(
            crate::state::load_run_state(&record)
                .unwrap()
                .registry_status_before
                .is_empty()
        );
        snapshot_registry_before_phase(&record, crate::workflow::graph::PHASE_FOLD).unwrap();
        let map = crate::state::load_run_state(&record)
            .unwrap()
            .registry_status_before;
        assert_eq!(map.get("0090").map(String::as_str), Some(READY_CELL));
    }

    #[test]
    fn a_model_or_bin_change_recycles_a_same_harness_holder() {
        let bin = std::path::PathBuf::from(r"C:\bin\codex-acp.cmd");
        let live = crate::harness::GrokHarnessStatus {
            alive: true,
            session_id: Some("s".into()),
            cwd: None,
            supports_compact: false,
            pid: Some(1),
            adapter: "codex".into(),
            context_usage: None,
            last_signal: None,
            last_tool_title: None,
            model: Some("gpt-6.1-sol[medium]".into()),
            bin: Some(bin.clone()),
        };
        let binding = crate::config::RoleBinding {
            harness: "codex".into(),
            command: "codex-acp".into(),
            model: Some("gpt-6.1-sol[medium]".into()),
        };
        assert!(!holder_mismatch(&live, &binding, &bin));
        let model_b = crate::config::RoleBinding {
            model: Some("gpt-6-luna[medium]".into()),
            ..binding.clone()
        };
        assert!(holder_mismatch(&live, &model_b, &bin), "model-only change");
        let other_harness = crate::config::RoleBinding {
            harness: "claude".into(),
            ..binding.clone()
        };
        assert!(
            holder_mismatch(&live, &other_harness, &bin),
            "harness change"
        );
        let other_bin = std::path::PathBuf::from(r"C:\bin\other.exe");
        assert!(
            holder_mismatch(&live, &binding, &other_bin),
            "resolved bin change"
        );
        let mut dead = live.clone();
        dead.alive = false;
        assert!(!holder_mismatch(&dead, &model_b, &bin), "dead holder");
    }

    /// 0088 regression: the acknowledgment save must land BEFORE the mailbox delete.
    ///
    /// On the pre-0088 delete-first order this test fails twice over: the mailbox is already gone
    /// when the save fails, so the retry has nothing to read and `pending_roles` keeps the slug.
    #[test]
    fn pending_ack_save_failure_retains_mailbox_and_retries() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        let record = rec(dir.path());
        crate::state::ensure_state_dir(&record).unwrap();
        let mut state = plan_state(&record, PHASE_PLAN_REVIEW, "0001");
        state.status = RunStatus::Running;
        state.pending_roles = vec!["agy".into()];
        state.adopt_plan_review_entry();
        crate::state::save_run_state(&record, &state).unwrap();
        let entry = state.plan_review_entry.unwrap();

        let roles = outcome_roles_dir(&record).unwrap();
        std::fs::create_dir_all(&roles).unwrap();
        let mailbox = roles.join("agy.json");
        let mut outcome = PhaseOutcome::failure(
            role_phase("agy"),
            FailureClass::Permission,
            OutcomeSource::Adapter,
            None,
            Some(state.run_epoch),
        );
        // 0119: the consumer fails closed on a missing epoch/entry pair.
        outcome.metadata = Some(crate::outcome::OutcomeMetadata {
            role: Some(crate::workflow::graph::ROLE_REVIEWER_AGY.into()),
            entry_id: Some(entry.entry_id),
            ..Default::default()
        });
        crate::persist::atomic_write_json(&mailbox, &outcome).unwrap();

        let _fail = fail_pending_save_once(&record.id, "agy");
        assert!(
            consume_role_files(&record).is_err(),
            "injected ack-save failure must surface as an error"
        );
        assert!(
            mailbox.is_file(),
            "a failed ack save must retain the role mailbox"
        );
        assert!(
            load_run_state(&record)
                .unwrap()
                .pending_roles
                .iter()
                .any(|s| s == "agy"),
            "the slug must still be pending after a failed ack save"
        );

        // Idempotent retry: the same mailbox is still there, the ack persists, then it is deleted.
        consume_role_files(&record).unwrap();
        assert!(!mailbox.exists(), "acked mailbox is deleted");
        assert!(
            load_run_state(&record).unwrap().pending_roles.is_empty(),
            "retry clears pending_roles"
        );
    }

    #[test]
    fn shared_tree_wait_runs_first() {
        let dir = tempdir().unwrap();
        let record = ProjectRecord {
            id: Uuid::new_v4().to_string(),
            path: dir.path().to_path_buf(),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: std::collections::BTreeMap::new(),
            state_dir: Some(dir.path().join("state")),
            auto_merge: true,
            phase_timeouts_secs: std::collections::BTreeMap::new(),
            notify_progress: false,
            worktree_isolation: true,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            ci_fix_routing: false,
            review_comment_routing: false,
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now(),
        };
        std::fs::create_dir_all(record.state_dir.as_ref().unwrap()).unwrap();
        let mut holder = crate::state::RunState::idle(&record.id);
        holder.run_epoch = 2;
        holder.status = RunStatus::Running;
        holder.phase = PHASE_PLAN.into();
        holder.driver = WorkflowDriver::Stub;
        let mut other = holder.clone();
        other.run_epoch = 5;
        other.track_id = Some("B".into());
        crate::state::write_run_book(
            &record,
            &crate::state::RunEnvelope::new(0, vec![holder, other]),
        )
        .unwrap();
        let review = {
            let _holder = crate::state::RunScope::enter(2).unwrap();
            write_review_markdown(&record, "agy", Some("Verdict: pass\n\nreal report\n")).unwrap()
        };
        let review_bytes = std::fs::read(&review).unwrap();
        assert!(
            review.ends_with(
                std::path::Path::new("reviews")
                    .join("2")
                    .join("agy-review.md")
            ),
            "{}",
            review.display()
        );
        let _scope = crate::state::RunScope::enter(5).unwrap();
        let view = tick(&record).unwrap().expect("sibling waits");
        assert!(
            view.last_event
                .contains("wait: shared tree held by epoch 2"),
            "{}",
            view.last_event
        );
        assert_eq!(std::fs::read(&review).unwrap(), review_bytes);
        let saved = {
            let _b = crate::state::RunScope::enter(5).unwrap();
            crate::state::load_run_state(&record).unwrap()
        };
        assert_eq!(saved.phase, PHASE_PLAN);
    }

    #[test]
    fn shared_tree_both_finish() {
        let dir = tempdir().unwrap();
        let record = ProjectRecord {
            id: Uuid::new_v4().to_string(),
            path: dir.path().to_path_buf(),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: std::collections::BTreeMap::new(),
            state_dir: Some(dir.path().join("state")),
            auto_merge: true,
            phase_timeouts_secs: std::collections::BTreeMap::new(),
            notify_progress: false,
            worktree_isolation: true,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            ci_fix_routing: false,
            review_comment_routing: false,
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now(),
        };
        std::fs::create_dir_all(record.state_dir.as_ref().unwrap()).unwrap();
        let shared = [
            PHASE_PLAN_REVIEW,
            crate::workflow::graph::PHASE_FOLD,
            crate::workflow::graph::PHASE_ADVANCE,
        ];
        for phase in shared {
            let mut holder = crate::state::RunState::idle(&record.id);
            holder.run_epoch = 2;
            holder.status = RunStatus::Running;
            holder.phase = phase.into();
            holder.driver = WorkflowDriver::Stub;
            holder.workflow = Some(WORKFLOW_ID.into());
            holder.track_id = Some("A".into());
            holder.last_event = "holder-ready".into();
            let mut other = holder.clone();
            other.run_epoch = 5;
            other.track_id = Some("B".into());
            other.last_event = "b-waiting".into();
            crate::state::write_run_book(
                &record,
                &crate::state::RunEnvelope::new(0, vec![holder, other]),
            )
            .unwrap();
            let waiting = {
                let _scope = crate::state::RunScope::enter(5).unwrap();
                tick(&record).unwrap()
            };
            assert!(
                waiting
                    .as_ref()
                    .unwrap()
                    .last_event
                    .contains("wait: shared tree held by epoch 2"),
                "{phase}: {waiting:?}"
            );
            let waited = {
                let _scope = crate::state::RunScope::enter(5).unwrap();
                crate::state::load_run_state(&record).unwrap()
            };
            assert_eq!(waited.phase, phase, "{phase}: B injected while A holds");
            let drove = {
                let _scope = crate::state::RunScope::enter(2).unwrap();
                tick(&record).unwrap()
            };
            assert!(
                drove
                    .as_ref()
                    .is_none_or(|view| !view.last_event.contains("wait: shared tree")),
                "{phase}: holder waited instead of injecting"
            );
            let holder_after = {
                let _scope = crate::state::RunScope::enter(2).unwrap();
                crate::state::load_run_state(&record).unwrap()
            };
            if phase == PHASE_PLAN_REVIEW {
                let review = record
                    .state_dir
                    .as_ref()
                    .unwrap()
                    .join("reviews")
                    .join("2")
                    .join("agy-review.md");
                assert!(
                    review.is_file(),
                    "plan-review holder did not write a report"
                );
                assert_eq!(waited.phase, PHASE_PLAN_REVIEW);
            }
            assert!(
                holder_after.phase != phase || holder_after.last_event != "holder-ready",
                "{phase}: holder did not inject ({})",
                holder_after.last_event
            );
            assert!(
                !holder_after.last_event.contains("wait: shared tree"),
                "{phase}: holder waited"
            );
            let other_still = {
                let _scope = crate::state::RunScope::enter(5).unwrap();
                crate::state::load_run_state(&record).unwrap()
            };
            assert_eq!(
                other_still.phase, phase,
                "{phase}: both injected on this round"
            );
            if !shared_tree_phase(&holder_after.phase) {
                let _finished = {
                    let _scope = crate::state::RunScope::enter(5).unwrap();
                    tick(&record).unwrap()
                };
                let other_done = {
                    let _scope = crate::state::RunScope::enter(5).unwrap();
                    crate::state::load_run_state(&record).unwrap()
                };
                assert_ne!(
                    other_done.phase, phase,
                    "{phase}: B did not finish after A left the set"
                );
            }
        }
    }

    #[test]
    fn inject_thread_prompts_its_own_epoch() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let dir = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        let bin = dir.path().join("grok.exe");
        std::fs::write(&bin, b"bin").unwrap();
        unsafe {
            std::env::set_var(crate::harness::grok::ENV_GROK_BIN, &bin);
        }
        let mut record = ProjectRecord {
            id: Uuid::new_v4().to_string(),
            path: dir.path().to_path_buf(),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: std::collections::BTreeMap::new(),
            state_dir: Some(dir.path().join("state")),
            auto_merge: true,
            phase_timeouts_secs: std::collections::BTreeMap::new(),
            notify_progress: false,
            worktree_isolation: true,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            ci_fix_routing: false,
            review_comment_routing: false,
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now(),
        };
        let mut reg = Registry::default();
        reg.projects.push(record.clone());
        reg.save(&registry_path().unwrap()).unwrap();
        std::fs::create_dir_all(record.state_dir.as_ref().unwrap()).unwrap();
        let skill = dir
            .path()
            .join(".agents")
            .join("skills")
            .join("plan")
            .join("SKILL.md");
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(&skill, b"plan\n").unwrap();
        let epoch_dir = |epoch: u64| {
            let path = crate::worktree::epoch_dir(&record, epoch).unwrap();
            std::fs::create_dir_all(&path).unwrap();
            path
        };
        let cwd_a = epoch_dir(3);
        let cwd_b = epoch_dir(5);
        let serve = |cwd: &std::path::Path, session: &str| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap().to_string();
            let ops = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let ops_t = ops.clone();
            let cwd = cwd.to_path_buf();
            let session = session.to_string();
            let bin = bin.clone();
            std::thread::spawn(move || {
                for _ in 0..6 {
                    let Ok((stream, _)) = listener.accept() else {
                        break;
                    };
                    let mut writer = stream.try_clone().unwrap();
                    let mut reader = std::io::BufReader::new(stream);
                    let mut line = String::new();
                    let _ = std::io::BufRead::read_line(&mut reader, &mut line);
                    let op = serde_json::from_str::<serde_json::Value>(line.trim())
                        .ok()
                        .and_then(|v| v.get("op").and_then(|op| op.as_str()).map(str::to_string))
                        .unwrap_or_default();
                    ops_t.lock().unwrap().push(op.clone());
                    let body = if op == "ping" {
                        serde_json::json!({
                            "ok": true,
                            "harness": {
                                "alive": true,
                                "session_id": session,
                                "cwd": cwd,
                                "adapter": "grok",
                                "supports_compact": true,
                                "bin": bin,
                            }
                        })
                    } else {
                        serde_json::json!({"ok": true})
                    };
                    let _ = std::io::Write::write_all(
                        &mut writer,
                        format!("{}\n", serde_json::to_string(&body).unwrap()).as_bytes(),
                    );
                    if op == "prompt" {
                        break;
                    }
                }
            });
            (addr, ops)
        };
        let (addr_a, ops_a) = serve(&cwd_a, "sess-a");
        let (addr_b, ops_b) = serve(&cwd_b, "sess-b");
        let persist = |epoch: u64, addr: &str, cwd: &std::path::Path, session: &str| {
            let path = record
                .state_dir
                .as_ref()
                .unwrap()
                .join("sessions")
                .join(epoch.to_string())
                .join("harness-grok.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                path,
                serde_json::json!({
                    "version": 1,
                    "project_id": record.id,
                    "adapter": "grok",
                    "supports_compact": true,
                    "alive": true,
                    "control_addr": addr,
                    "cwd": cwd,
                    "session_id": session,
                    "bin": bin,
                })
                .to_string(),
            )
            .unwrap();
        };
        persist(3, &addr_a, &cwd_a, "sess-a");
        persist(5, &addr_b, &cwd_b, "sess-b");
        let mut holder = crate::state::RunState::idle(&record.id);
        holder.run_epoch = 3;
        holder.status = RunStatus::Running;
        holder.phase = PHASE_PLAN.into();
        holder.driver = WorkflowDriver::Adapter;
        holder.workflow = Some(WORKFLOW_ID.into());
        let mut injected = holder.clone();
        injected.run_epoch = 5;
        injected.track_id = Some("B".into());
        let mut envelope = crate::state::RunEnvelope::new(0, vec![holder, injected]);
        envelope.shared_tree_holder = Some(5);
        crate::state::write_run_book(&record, &envelope).unwrap();
        record.worktree_isolation = true;
        {
            let _scope = crate::state::RunScope::enter(5).unwrap();
            tick(&record).unwrap();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline
            && !ops_b.lock().unwrap().iter().any(|op| op == "prompt")
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            ops_b.lock().unwrap().iter().any(|op| op == "prompt"),
            "B's inject did not prompt B: {:?}",
            ops_b.lock().unwrap()
        );
        assert!(
            !ops_a.lock().unwrap().iter().any(|op| op == "prompt"),
            "B's inject prompted A: {:?}",
            ops_a.lock().unwrap()
        );
        let driven = {
            let _scope = crate::state::RunScope::enter(5).unwrap();
            crate::state::load_run_state(&record)
                .unwrap()
                .last_driven_phase
        };
        assert_eq!(driven.as_deref(), Some(PHASE_PLAN));
        let other = {
            let _scope = crate::state::RunScope::enter(3).unwrap();
            crate::state::load_run_state(&record)
                .unwrap()
                .last_driven_phase
        };
        assert!(other.is_none(), "A was driven: {other:?}");
        unsafe {
            std::env::remove_var(crate::config::ENV_COORDINATOR_HOME);
            std::env::remove_var(crate::harness::grok::ENV_GROK_BIN);
        }
    }
}
