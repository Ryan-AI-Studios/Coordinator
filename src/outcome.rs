//! Phase Outcome File schema, paths, and single apply path (track 0005).
//!
//! All writers (CLI, HTTP, file poll, timeout synthesizer) must use [`apply`].

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{CoordinatorError, Result};
use crate::persist::atomic_write_json;
use crate::registry::ProjectRecord;
use crate::state::{
    RunState, RunStatus, STUB_PHASE_COMPLETED, STUB_PHASE_FAILED, StatusView, ensure_state_dir,
    load_run_state, resolve_state_dir, save_run_state, with_run_state_lock,
};
use crate::workflow::execution_repo::PreparedInjection;

/// Result of a prepared-turn apply. A stale identity is a structured skip, not an error.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum ApplyIdentity {
    Applied(StatusView),
    Stale(String),
}

/// Typed disposition consumed by the outcome poller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyDisposition {
    Applicable,
    Stale(String),
}

/// Cap for free-text message when copied into `last_event`.
pub const LAST_EVENT_MESSAGE_CAP: usize = 200;

/// Schema version this crate understands.
pub const OUTCOME_VERSION: u32 = 1;

/// Outcome status (success or failure only).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeStatus {
    Success,
    Failure,
}

impl OutcomeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
        }
    }
}

impl std::fmt::Display for OutcomeStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// ADR-0009 failure classes (v1 snake_case JSON strings).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    Permission,
    ModelExhaustion,
    Difficulty,
    HarnessCrash,
    Timeout,
    CiFailed,
    SkillMissing,
    /// 0097: a fold kept rewriting the artifacts that review had just passed.
    ReviewNeverSettles,
    /// A leg reported success, but the artifacts say the phase did not actually
    /// complete: no `plan.md`, or a review artifact still asking for a re-plan.
    Gate,
    /// 0098: the leg declared a terminal prerequisite block in its turn text. The run
    /// stops and the project holds until `failure resolve`.
    Blocked,
    /// 0078: unresolved owned-PR review comments, a capped `address-review` leg, a
    /// truncated/rate-limited/unreadable surface, or a review push that could not land.
    ReviewComments,
}

impl FailureClass {
    pub const ALL: [FailureClass; 11] = [
        Self::Permission,
        Self::ModelExhaustion,
        Self::Difficulty,
        Self::HarnessCrash,
        Self::Timeout,
        Self::CiFailed,
        Self::SkillMissing,
        Self::ReviewNeverSettles,
        Self::Gate,
        Self::Blocked,
        Self::ReviewComments,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::ModelExhaustion => "model_exhaustion",
            Self::Difficulty => "difficulty",
            Self::HarnessCrash => "harness_crash",
            Self::Timeout => "timeout",
            Self::CiFailed => "ci_failed",
            Self::SkillMissing => "skill_missing",
            Self::ReviewNeverSettles => "review_never_settles",
            Self::Gate => "gate",
            Self::Blocked => "blocked",
            Self::ReviewComments => "review_comments",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "permission" => Ok(Self::Permission),
            "model_exhaustion" => Ok(Self::ModelExhaustion),
            "difficulty" => Ok(Self::Difficulty),
            "harness_crash" => Ok(Self::HarnessCrash),
            "timeout" => Ok(Self::Timeout),
            "ci_failed" => Ok(Self::CiFailed),
            "skill_missing" => Ok(Self::SkillMissing),
            "review_never_settles" => Ok(Self::ReviewNeverSettles),
            "gate" => Ok(Self::Gate),
            "blocked" => Ok(Self::Blocked),
            "review_comments" => Ok(Self::ReviewComments),
            other => Err(CoordinatorError::Message(format!(
                "unknown failure_class: {other}"
            ))),
        }
    }
}

impl std::fmt::Display for FailureClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Who wrote the outcome.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeSource {
    File,
    Http,
    Cli,
    Timeout,
    Test,
    /// Harness adapter (e.g. Grok ACP) wrote the outcome.
    Adapter,
}

impl OutcomeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Http => "http",
            Self::Cli => "cli",
            Self::Timeout => "timeout",
            Self::Test => "test",
            Self::Adapter => "adapter",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "file" => Ok(Self::File),
            "http" => Ok(Self::Http),
            "cli" => Ok(Self::Cli),
            "timeout" => Ok(Self::Timeout),
            "test" => Ok(Self::Test),
            "adapter" => Ok(Self::Adapter),
            other => Err(CoordinatorError::Message(format!(
                "unknown outcome source: {other}"
            ))),
        }
    }
}

impl std::fmt::Display for OutcomeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Opaque metadata bag; recognized keys are optional.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutcomeMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_track: Option<String>,
    /// Recognized: planner, implementor, plan_reviewer_agy, plan_reviewer_opencode. Unknown ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Implement-phase hint for `ci-wait` target resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    /// 0119: the plan-review entry that produced this outcome.
    ///
    /// Additive and internal: it is not a Status-JSON or HTTP key. A plan-review role outcome must
    /// carry both the live `run_epoch` and this entry id; an absent pair is refused by the
    /// consumer (fail closed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<u64>,
}

/// Phase Outcome File schema v1.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PhaseOutcome {
    pub version: u32,
    pub phase: String,
    pub status: OutcomeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<FailureClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub written_at: DateTime<Utc>,
    pub source: OutcomeSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<OutcomeMetadata>,
    /// Stronger stale check when present; omitted by most hooks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_epoch: Option<u64>,
}

impl PhaseOutcome {
    /// Validate schema rules (version, phase, failure_class pairing).
    pub fn validate(&self) -> Result<()> {
        if self.version != OUTCOME_VERSION {
            return Err(CoordinatorError::Message(format!(
                "unsupported outcome version: {} (expected {OUTCOME_VERSION})",
                self.version
            )));
        }
        if self.phase.trim().is_empty() {
            return Err(CoordinatorError::Message(
                "outcome phase must be non-empty".into(),
            ));
        }
        match self.status {
            OutcomeStatus::Success => {
                if self.failure_class.is_some() {
                    return Err(CoordinatorError::Message(
                        "failure_class must be null when status=success".into(),
                    ));
                }
            }
            OutcomeStatus::Failure => {
                if self.failure_class.is_none() {
                    return Err(CoordinatorError::Message(
                        "failure_class is required when status=failure".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Build a success outcome for writers.
    pub fn success(
        phase: impl Into<String>,
        source: OutcomeSource,
        message: Option<String>,
        next_track: Option<String>,
        run_epoch: Option<u64>,
    ) -> Self {
        let metadata = next_track.map(|t| OutcomeMetadata {
            next_track: Some(t),
            ..Default::default()
        });
        Self {
            version: OUTCOME_VERSION,
            phase: phase.into(),
            status: OutcomeStatus::Success,
            failure_class: None,
            message,
            written_at: Utc::now(),
            source,
            metadata,
            run_epoch,
        }
    }

    /// Build a failure outcome for writers / timeout synthesizer.
    pub fn failure(
        phase: impl Into<String>,
        class: FailureClass,
        source: OutcomeSource,
        message: Option<String>,
        run_epoch: Option<u64>,
    ) -> Self {
        Self {
            version: OUTCOME_VERSION,
            phase: phase.into(),
            status: OutcomeStatus::Failure,
            failure_class: Some(class),
            message,
            written_at: Utc::now(),
            source,
            metadata: None,
            run_epoch,
        }
    }
}

/// Stable non-crypto digest of a validated outcome (consume marker).
pub fn outcome_content_hash(outcome: &PhaseOutcome) -> Result<String> {
    let bytes = serde_json::to_vec(outcome)?;
    Ok(fnv1a64_hex(&bytes))
}

fn fnv1a64_hex(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

fn truncate_msg(msg: &str) -> String {
    if msg.chars().count() <= LAST_EVENT_MESSAGE_CAP {
        return msg.to_string();
    }
    let t: String = msg.chars().take(LAST_EVENT_MESSAGE_CAP).collect();
    format!("{t}…")
}

/// `{state_dir}/outcomes`
pub fn outcomes_dir(record: &ProjectRecord) -> Result<PathBuf> {
    Ok(resolve_state_dir(record)?.join("outcomes"))
}

fn scoped_outcomes_dir(record: &ProjectRecord) -> Result<PathBuf> {
    let base = outcomes_dir(record)?;
    match crate::state::active_scope(record)? {
        Some(epoch) => Ok(base.join(epoch.to_string())),
        None => Ok(base),
    }
}

pub fn outcome_current_path(record: &ProjectRecord) -> Result<PathBuf> {
    Ok(scoped_outcomes_dir(record)?.join("current.json"))
}

pub fn outcome_applied_path(record: &ProjectRecord) -> Result<PathBuf> {
    Ok(scoped_outcomes_dir(record)?.join("current.applied.json"))
}

pub fn outcome_history_dir(record: &ProjectRecord) -> Result<PathBuf> {
    Ok(outcomes_dir(record)?.join("history"))
}

/// Reserved layout for parallel role slots (0008); documented only this track.
pub fn outcome_roles_dir(record: &ProjectRecord) -> Result<PathBuf> {
    Ok(scoped_outcomes_dir(record)?.join("roles"))
}

pub fn ensure_outcomes_dir(record: &ProjectRecord) -> Result<PathBuf> {
    ensure_state_dir(record)?;
    let dir = outcomes_dir(record)?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Atomic write of the active Phase Outcome file.
pub fn save_current_outcome(record: &ProjectRecord, outcome: &PhaseOutcome) -> Result<()> {
    outcome.validate()?;
    ensure_outcomes_dir(record)?;
    atomic_write_json(&outcome_current_path(record)?, outcome)
}

/// Load `current.json` if present. Transient IO/parse errors bubble for CLI; poll uses soft path.
pub fn load_current_outcome(record: &ProjectRecord) -> Result<Option<PhaseOutcome>> {
    let path = outcome_current_path(record)?;
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    let outcome: PhaseOutcome = serde_json::from_str(&text)?;
    Ok(Some(outcome))
}

/// Soft load for pollers: missing → None; share/parse errors → None (skip tick).
pub fn try_load_current_outcome(record: &ProjectRecord) -> Option<PhaseOutcome> {
    let path = outcome_current_path(record).ok()?;
    if !path.exists() {
        return None;
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return None,
    };
    serde_json::from_str(&text).ok()
}

/// Process-wide apply gate (CLI single-shot + serve share one process mutex).
fn apply_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Commit returned by [`apply_locked`]. Notify is fired **after** both apply
/// locks drop so WinRT toast COM cannot stall concurrent applies.
struct ApplyCommit {
    view: StatusView,
    notify: Option<crate::notify::NotifyEvent>,
    progress: Option<crate::notify::ProgressEvent>,
    reuse_probe: Option<Box<crate::workflow::reuse::ReuseProbe>>,
}

/// Single entry for mutating run-state from a Phase Outcome.
///
/// CLI / HTTP / file poll / timeout synthesizer must call this (not hand-roll transitions).
/// Serializes with a process mutex **and** a cross-process state-dir lock so concurrent
/// CLI vs `serve` first-commit-wins (no last-writer-wins on run-state).
///
/// Hard-failure notify runs **after** both locks drop (track 0009).
pub fn apply(record: &ProjectRecord, outcome: PhaseOutcome) -> Result<StatusView> {
    match apply_impl(record, outcome, None)? {
        ApplyIdentity::Applied(view) => Ok(view),
        ApplyIdentity::Stale(reason) => Err(CoordinatorError::Message(reason)),
    }
}

/// Apply an outcome only if the injected prepared identity is still current.
///
/// The identity is rechecked under the existing apply lock and the run-state lock,
/// so the check/write race cannot apply this turn to a newer run.
pub fn apply_prepared(
    record: &ProjectRecord,
    outcome: PhaseOutcome,
    prepared: &PreparedInjection,
) -> Result<ApplyIdentity> {
    apply_impl(record, outcome, Some(prepared))
}

fn outcome_scope(
    record: &ProjectRecord,
    epoch: Option<u64>,
) -> Result<Option<crate::state::RunScope>> {
    if !matches!(
        crate::state::load_run_book(record)?,
        crate::state::RunBook::Many(_)
    ) {
        return Ok(None);
    }
    match epoch.or_else(crate::state::active_run_epoch) {
        Some(epoch) if crate::state::active_run_epoch() == Some(epoch) => Ok(None),
        Some(epoch) => Ok(Some(crate::state::RunScope::enter(epoch)?)),
        None => Err(crate::state::ambiguous_run_error()),
    }
}

fn apply_impl(
    record: &ProjectRecord,
    outcome: PhaseOutcome,
    prepared: Option<&PreparedInjection>,
) -> Result<ApplyIdentity> {
    outcome.validate()?;
    let _scope = outcome_scope(record, outcome.run_epoch)?;
    let mut commit = {
        let _guard = apply_lock()
            .lock()
            .map_err(|_| CoordinatorError::Message("outcome apply lock poisoned".into()))?;
        with_run_state_lock(record, || {
            if let Some(prepared) = prepared {
                let state = load_run_state(record)?;
                if state.status != RunStatus::Running || !prepared.matches_state(&state) {
                    return Ok(None);
                }
            }
            apply_locked(record, outcome).map(Some)
        })?
    };
    let Some(mut commit) = commit.take() else {
        return Ok(ApplyIdentity::Stale("stale-injection".into()));
    };
    if let Some(probe) = commit.reuse_probe.take() {
        let answer =
            crate::workflow::reuse::probe_resume(record, probe.number, probe.repo.as_deref());
        let _guard = apply_lock()
            .lock()
            .map_err(|_| CoordinatorError::Message("outcome apply lock poisoned".into()))?;
        let view = with_run_state_lock(record, || {
            crate::workflow::commit_probed_successor(record, &probe, &answer)
        })?;
        let planted = view.run_epoch != commit.view.run_epoch
            || view.phase != commit.view.phase
            || view.track_id != commit.view.track_id;
        if planted {
            if let Some(progress) = commit.progress.as_mut() {
                progress.to_phase = if view.status == RunStatus::Idle {
                    "idle".into()
                } else {
                    view.phase.clone()
                };
                progress.last_event = view.last_event.clone();
                progress.next_track = if view.run_epoch != progress.run_epoch {
                    view.track_id.clone()
                } else {
                    view.next_track.clone()
                };
            }
        } else {
            commit.progress = None;
            commit.notify = None;
        }
        commit.view = view;
    }
    fire_pending_notify(record, &commit);
    crate::worktree::release_if_idle(record, &commit.view);
    Ok(ApplyIdentity::Applied(refresh_failure_artifact(
        record,
        commit.view,
    )))
}

/// Typed pre-check used by [`poll_try_apply`] so message classification is no longer
/// the only stale-state detector.
pub fn outcome_disposition(
    record: &ProjectRecord,
    outcome: &PhaseOutcome,
) -> Result<ApplyDisposition> {
    let _scope = outcome_scope(record, outcome.run_epoch)?;
    let base = load_run_state(record)?;
    if base.status != RunStatus::Running && base.status != RunStatus::Paused {
        return Ok(ApplyDisposition::Stale(format!(
            "cannot apply outcome while status is {}",
            base.status
        )));
    }
    if outcome.phase != base.phase {
        return Ok(ApplyDisposition::Stale(format!(
            "outcome phase {} does not match current phase {}",
            outcome.phase, base.phase
        )));
    }
    if let Some(epoch) = outcome.run_epoch
        && epoch != base.run_epoch
    {
        return Ok(ApplyDisposition::Stale(format!(
            "outcome run_epoch {epoch} does not match state run_epoch {}",
            base.run_epoch
        )));
    }
    Ok(ApplyDisposition::Applicable)
}

fn fire_pending_notify(record: &ProjectRecord, commit: &ApplyCommit) {
    if let Some(event) = &commit.notify {
        crate::notify::on_hard_failure(record, event);
    }
    if let Some(event) = &commit.progress {
        crate::notify::on_progress(event);
    }
}

/// Artifact is written after apply locks drop; refresh the path on the returned view.
/// Count committed failures for the track that just finished.
///
/// A gate bounce and a `policy:` failure do not increment. Success stores `0`
/// for that track (the key stays). Fresh `run` does not touch this map.
fn note_consecutive_failures(
    state: &mut RunState,
    outcome: &PhaseOutcome,
    track: &Option<String>,
    bounced: bool,
) {
    let Some(id) = track.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
        return;
    };
    match outcome.status {
        OutcomeStatus::Success => {
            state.consecutive_failures.insert(id.to_string(), 0);
        }
        OutcomeStatus::Failure => {
            if bounced {
                return;
            }
            // 0098: a blocked turn is a hold, not a failed attempt. It must not move the
            // policy checkpoint counter; earlier failures still do.
            if outcome.failure_class == Some(FailureClass::Blocked) {
                return;
            }
            if outcome
                .message
                .as_deref()
                .unwrap_or("")
                .starts_with("policy:")
            {
                return;
            }
            let n = state
                .consecutive_failures
                .get(id)
                .copied()
                .unwrap_or(0)
                .saturating_add(1);
            state.consecutive_failures.insert(id.to_string(), n);
        }
    }
}

fn refresh_failure_artifact(record: &ProjectRecord, mut view: StatusView) -> StatusView {
    view.failure_artifact = crate::notify::artifact::existing_path(record);
    view
}

fn apply_locked(record: &ProjectRecord, outcome: PhaseOutcome) -> Result<ApplyCommit> {
    outcome.validate()?;
    ensure_state_dir(record)?;
    let hash = outcome_content_hash(&outcome)?;
    let base = load_run_state(record)?;

    // Idempotent re-apply of the same content.
    if base.last_applied_outcome_hash.as_deref() == Some(hash.as_str()) {
        return Ok(ApplyCommit {
            view: StatusView::from_record(record, &base),
            notify: None,
            progress: None,
            reuse_probe: None,
        });
    }

    // Optional stronger epoch check when the writer set it.
    if let Some(epoch) = outcome.run_epoch
        && epoch != base.run_epoch
    {
        return Err(CoordinatorError::Message(format!(
            "outcome run_epoch {epoch} does not match state run_epoch {}",
            base.run_epoch
        )));
    }

    match base.status {
        RunStatus::Idle | RunStatus::Stopped => {
            return Err(CoordinatorError::Message(format!(
                "cannot apply outcome while status is {}",
                base.status
            )));
        }
        RunStatus::Running | RunStatus::Paused => {}
    }

    // Timeout may only fire while Running (budget frozen while Paused).
    if outcome.source == OutcomeSource::Timeout && base.status != RunStatus::Running {
        return Err(CoordinatorError::Message(
            "timeout outcome rejected: status is not Running".into(),
        ));
    }

    if outcome.phase != base.phase {
        return Err(CoordinatorError::Message(format!(
            "outcome phase '{}' does not match current phase '{}'",
            outcome.phase, base.phase
        )));
    }

    let from_phase = base.phase.clone();
    let snap_track = base.track_id.clone();
    let snap_epoch = base.run_epoch;
    let elapsed_secs = if base.phase_started_at.is_some() {
        Some(base.effective_running_elapsed(outcome.written_at).as_secs())
    } else {
        None
    };

    let mut state = base.clone();
    let canonical = crate::workflow::is_canonical(&outcome.phase);
    let mut bounced = false;
    let mut address_ci_stop: Option<String> = None;
    let mut reuse_probe = None;
    match outcome.status {
        OutcomeStatus::Success => {
            if canonical {
                match crate::workflow::on_success(record, &mut state, &outcome) {
                    crate::workflow::AddressCiFollowUp::Stopped { message } => {
                        address_ci_stop = Some(message);
                    }
                    crate::workflow::AddressCiFollowUp::Continue => {}
                    crate::workflow::AddressCiFollowUp::ReuseProbe(probe) => {
                        reuse_probe = Some(probe);
                    }
                }
            } else {
                state.phase = STUB_PHASE_COMPLETED.into();
                state.failure_class = None;
                if let Some(ref meta) = outcome.metadata
                    && let Some(ref next) = meta.next_track
                {
                    let t = next.trim();
                    state.next_track = if t.is_empty() {
                        None
                    } else {
                        Some(t.to_string())
                    };
                }
                match state.status {
                    RunStatus::Running => {
                        state.status = RunStatus::Idle;
                        state.last_event = format_success_event(&outcome, false);
                    }
                    RunStatus::Paused => {
                        // Finished current phase while held (ADR-0024).
                        state.last_event = format_success_event(&outcome, true);
                    }
                    _ => unreachable!(),
                }
                // Phase clock no longer active after leftover stub apply.
                state.phase_started_at = None;
                state.pause_started_at = None;
            }
        }
        OutcomeStatus::Failure => {
            let class = outcome.failure_class.expect("validated");
            bounced = crate::workflow::should_bounce_gate_fail(&state, &outcome);
            if bounced {
                crate::workflow::on_gate_retry(record, &mut state, &outcome);
            } else {
                state.status = RunStatus::Stopped;
                if !canonical {
                    state.phase = STUB_PHASE_FAILED.into();
                }
                state.failure_class = Some(class);
                state.last_event = format_failure_event(&outcome, class);
                if outcome.phase == crate::workflow::graph::PHASE_CROSS_MODEL
                    && class == FailureClass::Difficulty
                    && state.address_findings_attempts
                        >= crate::workflow::graph::ADDRESS_FINDINGS_CAP
                {
                    let cap = crate::workflow::graph::ADDRESS_FINDINGS_CAP;
                    state.last_event = format!(
                        "{}; address-findings exhausted ({cap}/{cap})",
                        state.last_event
                    );
                }
                state.phase_started_at = None;
                state.pause_started_at = None;
            }
        }
    }

    note_consecutive_failures(&mut state, &outcome, &snap_track, bounced);

    state.updated_at = Utc::now();
    state.last_applied_outcome_hash = Some(hash.clone());

    // Cross-process fail-closed: re-load and require base snapshot still current
    // (first valid commit wins; concurrent CLI vs serve last-writer-wins is rejected).
    let fresh = load_run_state(record)?;
    if fresh.last_applied_outcome_hash.as_deref() == Some(hash.as_str()) {
        // Peer already applied identical content.
        clear_active_outcome_file(record);
        return Ok(ApplyCommit {
            view: StatusView::from_record(record, &fresh),
            notify: None,
            progress: None,
            reuse_probe: None,
        });
    }
    if fresh.run_epoch != base.run_epoch
        || fresh.status != base.status
        || fresh.phase != base.phase
        || fresh.last_applied_outcome_hash != base.last_applied_outcome_hash
    {
        return Err(CoordinatorError::Message(
            "apply race: run-state changed before commit; retry after status check".into(),
        ));
    }

    // Consume pattern: history → applied snapshot → remove current → save state.
    best_effort_history(record, &outcome);
    let _ = ensure_outcomes_dir(record);
    if let Ok(applied) = outcome_applied_path(record) {
        let _ = atomic_write_json(&applied, &outcome);
    }
    clear_active_outcome_file(record);

    save_run_state(record, &state)?;
    let track = state.track_id.as_deref().unwrap_or("-");
    let mut kind = match outcome.status {
        OutcomeStatus::Success => "end",
        OutcomeStatus::Failure => "fail",
    };
    let mut notify = match outcome.status {
        OutcomeStatus::Failure => outcome.failure_class.map(|class| {
            let artifact_path = crate::notify::artifact::path(record)
                .unwrap_or_else(|_| record.path.join(".coordinator").join("FAILURE.md"));
            crate::notify::NotifyEvent {
                project_id: record.id.clone(),
                track_id: state.track_id.clone(),
                phase: state.phase.clone(),
                failure_class: class,
                message: outcome.message.clone(),
                last_event: state.last_event.clone(),
                artifact_path,
                written_at: outcome.written_at,
                run_epoch: state.run_epoch,
            }
        }),
        OutcomeStatus::Success => None,
    };
    if bounced {
        kind = "end";
        notify = None;
    }
    if let Some(message) = address_ci_stop.clone() {
        kind = "fail";
        let artifact_path = crate::notify::artifact::path(record)
            .unwrap_or_else(|_| record.path.join(".coordinator").join("FAILURE.md"));
        notify = Some(crate::notify::NotifyEvent {
            project_id: record.id.clone(),
            track_id: state.track_id.clone(),
            phase: state.phase.clone(),
            failure_class: state.failure_class.unwrap_or(FailureClass::CiFailed),
            message: Some(message),
            last_event: state.last_event.clone(),
            artifact_path,
            written_at: outcome.written_at,
            run_epoch: state.run_epoch,
        });
    }
    let progress = if address_ci_stop.is_some() {
        None
    } else if crate::notify::progress_enabled(record)
        && ((matches!(outcome.status, OutcomeStatus::Success) && canonical) || bounced)
    {
        let to_phase = if state.status == RunStatus::Idle {
            "idle".to_string()
        } else {
            state.phase.clone()
        };
        let next_track = if state.run_epoch != snap_epoch {
            state.track_id.clone()
        } else {
            state.next_track.clone()
        };
        Some(crate::notify::ProgressEvent {
            event_type: crate::notify::EVENT_TYPE_PROGRESS.into(),
            project_id: record.id.clone(),
            track_id: snap_track,
            from_phase,
            to_phase,
            elapsed_secs,
            last_event: state.last_event.clone(),
            message: outcome.message.clone(),
            written_at: outcome.written_at,
            run_epoch: snap_epoch,
            next_track,
        })
    } else {
        None
    };
    crate::progress_log::append(
        record,
        kind,
        &format!("track={track} phase={}  {}", state.phase, state.last_event),
    );
    Ok(ApplyCommit {
        view: StatusView::from_record(record, &state),
        notify,
        progress,
        reuse_probe,
    })
}

/// Remove this entry's active outcome file. A book also removes its applied file.
/// A book with no scope removes nothing.
pub fn clear_active_outcome_file(record: &ProjectRecord) {
    let scoped = match crate::state::active_scope(record) {
        Ok(scope) => scope,
        Err(_) => return,
    };
    if let Ok(current) = outcome_current_path(record)
        && current.exists()
    {
        let _ = std::fs::remove_file(current);
    }
    if scoped.is_some()
        && let Ok(applied) = outcome_applied_path(record)
        && applied.exists()
    {
        let _ = std::fs::remove_file(applied);
    }
}

fn format_success_event(outcome: &PhaseOutcome, paused: bool) -> String {
    let mut s = if paused {
        format!(
            "outcome: success (paused) phase={} source={}",
            outcome.phase, outcome.source
        )
    } else {
        format!(
            "outcome: success phase={} source={}",
            outcome.phase, outcome.source
        )
    };
    if let Some(ref m) = outcome.message {
        s.push_str(" — ");
        s.push_str(&truncate_msg(m));
    }
    s
}

fn format_failure_event(outcome: &PhaseOutcome, class: FailureClass) -> String {
    let mut s = format!(
        "outcome: failure class={} phase={} source={}",
        class, outcome.phase, outcome.source
    );
    if let Some(ref m) = outcome.message {
        s.push_str(" — ");
        s.push_str(&truncate_msg(m));
    }
    s
}

fn best_effort_history(record: &ProjectRecord, outcome: &PhaseOutcome) {
    let Ok(dir) = outcome_history_dir(record) else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let stamp = outcome.written_at.format("%Y%m%dT%H%M%SZ");
    let safe_phase: String = outcome
        .phase
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let path = dir.join(format!("{stamp}_{safe_phase}.json"));
    let _ = atomic_write_json(&path, outcome);
}

/// Soft apply for file pollers: on Idle/Stopped/stale reject, best-effort consume the file.
///
/// Returns `Ok(Some(view))` when state transitioned or idempotent re-apply;
/// `Ok(None)` when nothing actionable; never panics on share/parse issues (caller loads softly).
pub fn poll_try_apply(record: &ProjectRecord, outcome: PhaseOutcome) -> Result<Option<StatusView>> {
    if let ApplyDisposition::Stale(_) = outcome_disposition(record, &outcome)? {
        if let Ok(current) = outcome_current_path(record)
            && current.exists()
        {
            let _ = std::fs::remove_file(&current);
        }
        return Ok(None);
    }
    match apply(record, outcome) {
        Ok(view) => Ok(Some(view)),
        Err(e) => {
            let msg = e.to_string();
            // Consume leftover active file so pollers do not spin on unapplicable content.
            if msg.contains("cannot apply outcome while status is")
                || msg.contains("does not match current phase")
                || msg.contains("does not match state run_epoch")
            {
                if let Ok(current) = outcome_current_path(record)
                    && current.exists()
                {
                    let _ = std::fs::remove_file(&current);
                }
                return Ok(None);
            }
            Err(e)
        }
    }
}

/// Write + apply from CLI/HTTP builders (drops `current.json` then applies).
///
/// If apply rejects, the active file is removed so a later `run` cannot replay it.
pub fn write_and_apply_prepared(
    record: &ProjectRecord,
    outcome: PhaseOutcome,
    prepared: &PreparedInjection,
) -> Result<ApplyIdentity> {
    outcome.validate()?;
    // Prepared turns apply synchronously. Do not persist an active outcome before
    // the locked identity check: a stale turn must leave no replayable file.
    match apply_prepared(record, outcome, prepared) {
        Ok(ApplyIdentity::Stale(reason)) => {
            clear_active_outcome_file(record);
            Ok(ApplyIdentity::Stale(reason))
        }
        other => other,
    }
}

pub fn write_and_apply(record: &ProjectRecord, outcome: PhaseOutcome) -> Result<StatusView> {
    outcome.validate()?;
    let _scope = outcome_scope(record, outcome.run_epoch)?;
    save_current_outcome(record, &outcome)?;
    match apply(record, outcome) {
        Ok(view) => Ok(view),
        Err(e) => {
            clear_active_outcome_file(record);
            Err(e)
        }
    }
}

/// Evaluate stub phase budget and apply a timeout **under the run-state lock**.
///
/// Decision and commit share one lock so a concurrent `pause` cannot lose to a
/// stale pre-lock timeout snapshot. Returns `Ok(None)` when not timed out.
pub fn try_timeout_under_lock(record: &ProjectRecord) -> Result<Option<StatusView>> {
    let commit = {
        let _guard = apply_lock()
            .lock()
            .map_err(|_| CoordinatorError::Message("outcome apply lock poisoned".into()))?;
        with_run_state_lock(record, || {
            let state = load_run_state(record)?;
            if state.status != RunStatus::Running {
                return Ok(None);
            }
            let Some(_started) = state.phase_started_at else {
                return Ok(None);
            };
            let budget = crate::workflow::timeout_for_phase(record, &state.phase);
            let elapsed = state.effective_running_elapsed(Utc::now());
            if elapsed < budget {
                return Ok(None);
            }
            if state.phase == crate::workflow::graph::PHASE_COMPACT {
                let outcome = PhaseOutcome::success(
                    state.phase.clone(),
                    OutcomeSource::Timeout,
                    Some("compact: skipped".into()),
                    None,
                    Some(state.run_epoch),
                );
                let _ = save_current_outcome(record, &outcome);
                return apply_locked(record, outcome).map(Some);
            }
            if state.phase == crate::workflow::graph::PHASE_PLAN_REVIEW {
                match crate::workflow::drive::timeout_plan_review_outcome(record, &state)? {
                    crate::workflow::drive::TimeoutDecision::Apply(outcome) => {
                        let _ = save_current_outcome(record, &outcome);
                        return apply_locked(record, outcome).map(Some);
                    }
                    // 0119: pending empty and nothing missing - the join owns that state; do not
                    // synthesize the generic "phase budget exceeded".
                    crate::workflow::drive::TimeoutDecision::LetJoin => return Ok(None),
                }
            }
            let outcome = PhaseOutcome::failure(
                state.phase.clone(),
                FailureClass::Timeout,
                OutcomeSource::Timeout,
                Some("phase budget exceeded".into()),
                Some(state.run_epoch),
            );
            let _ = save_current_outcome(record, &outcome);
            apply_locked(record, outcome).map(Some)
        })?
    };
    match commit {
        Some(c) => {
            fire_pending_notify(record, &c);
            Ok(Some(refresh_failure_artifact(record, c.view)))
        }
        None => Ok(None),
    }
}

/// Synthesize a timeout failure for the current phase and apply it.
///
/// Prefer [`try_timeout_under_lock`] from pollers (atomic decide+apply).
pub fn apply_timeout(record: &ProjectRecord, state: &RunState) -> Result<StatusView> {
    if state.status != RunStatus::Running {
        return Err(CoordinatorError::Message(
            "timeout apply requires status Running (budget frozen while Paused)".into(),
        ));
    }
    if state.phase == crate::workflow::graph::PHASE_COMPACT {
        let outcome = PhaseOutcome::success(
            state.phase.clone(),
            OutcomeSource::Timeout,
            Some("compact: skipped".into()),
            None,
            Some(state.run_epoch),
        );
        let _ = save_current_outcome(record, &outcome);
        return apply(record, outcome);
    }
    let outcome = PhaseOutcome::failure(
        state.phase.clone(),
        FailureClass::Timeout,
        OutcomeSource::Timeout,
        Some("phase budget exceeded".into()),
        Some(state.run_epoch),
    );
    let _ = save_current_outcome(record, &outcome);
    apply(record, outcome)
}

/// Parse CLI/HTTP status string.
pub fn parse_outcome_status(s: &str) -> Result<OutcomeStatus> {
    match s {
        "success" => Ok(OutcomeStatus::Success),
        "failure" => Ok(OutcomeStatus::Failure),
        other => Err(CoordinatorError::Message(format!(
            "status must be success|failure, got {other}"
        ))),
    }
}

/// Whether `path` is under an outcomes layout (helper for tests/docs).
pub fn is_outcomes_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|n| n == "current.json" || n == "current.applied.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::{self, run_stub};
    use crate::state::STUB_PHASE_ACTIVE;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn resolve_for_test(record: &ProjectRecord) {
        let mut state = load_run_state(record).unwrap();
        crate::notify::settle_failure(
            record,
            &mut state,
            crate::notify::LAST_EVENT_FAILURE_RESOLVED,
            true,
        );
        save_run_state(record, &state).unwrap();
    }

    fn rec(path: &Path) -> ProjectRecord {
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

    #[test]
    fn adapter_source_round_trip() {
        let o = PhaseOutcome::success("stub:active", OutcomeSource::Adapter, None, None, None);
        o.validate().unwrap();
        let json = serde_json::to_string(&o).unwrap();
        assert!(json.contains("\"adapter\""));
        let o2: PhaseOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(o2.source, OutcomeSource::Adapter);
        assert_eq!(
            OutcomeSource::parse("adapter").unwrap(),
            OutcomeSource::Adapter
        );
        assert_eq!(OutcomeSource::Adapter.as_str(), "adapter");
    }

    #[test]
    fn valid_success_schema() {
        let o = PhaseOutcome::success("stub:active", OutcomeSource::Cli, None, None, None);
        o.validate().unwrap();
    }

    #[test]
    fn valid_failure_with_class() {
        let o = PhaseOutcome::failure(
            "stub:active",
            FailureClass::Timeout,
            OutcomeSource::Test,
            None,
            None,
        );
        o.validate().unwrap();
    }

    #[test]
    fn reject_success_with_class() {
        let mut o = PhaseOutcome::success("stub:active", OutcomeSource::Cli, None, None, None);
        o.failure_class = Some(FailureClass::Difficulty);
        assert!(o.validate().is_err());
    }

    #[test]
    fn reject_failure_without_class() {
        let mut o = PhaseOutcome::failure(
            "stub:active",
            FailureClass::Timeout,
            OutcomeSource::Test,
            None,
            None,
        );
        o.failure_class = None;
        assert!(o.validate().is_err());
    }

    #[test]
    fn reject_unknown_version() {
        let mut o = PhaseOutcome::success("stub:active", OutcomeSource::Cli, None, None, None);
        o.version = 99;
        assert!(o.validate().is_err());
    }

    #[test]
    fn reject_unknown_failure_class_json() {
        let json = r#"{
            "version": 1,
            "phase": "stub:active",
            "status": "failure",
            "failure_class": "not_a_class",
            "written_at": "2026-08-12T12:00:00Z",
            "source": "cli"
        }"#;
        let err = serde_json::from_str::<PhaseOutcome>(json).unwrap_err();
        assert!(err.to_string().contains("not_a_class") || err.is_data());
    }

    #[test]
    fn reject_empty_phase() {
        let mut o = PhaseOutcome::success("  ", OutcomeSource::Cli, None, None, None);
        o.phase = "   ".into();
        assert!(o.validate().is_err());
    }

    #[test]
    fn serde_round_trip_spec_example() {
        let json = r#"{
            "version": 1,
            "phase": "stub:active",
            "status": "success",
            "failure_class": null,
            "message": "optional human/agent note",
            "written_at": "2026-08-12T12:00:00Z",
            "source": "cli",
            "metadata": {
                "next_track": null,
                "role": null
            }
        }"#;
        let o: PhaseOutcome = serde_json::from_str(json).unwrap();
        o.validate().unwrap();
        assert_eq!(o.status, OutcomeStatus::Success);
        assert_eq!(o.source, OutcomeSource::Cli);
        let back = serde_json::to_string(&o).unwrap();
        let o2: PhaseOutcome = serde_json::from_str(&back).unwrap();
        assert_eq!(o, o2);
    }

    #[test]
    fn apply_success_running_to_idle() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_stub(&r, Some("0005".into())).unwrap();
        let o = PhaseOutcome::success(
            STUB_PHASE_ACTIVE,
            OutcomeSource::Cli,
            Some("done".into()),
            Some("0006".into()),
            None,
        );
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.status, RunStatus::Idle);
        assert_eq!(view.phase, STUB_PHASE_COMPLETED);
        assert_eq!(view.next_track.as_deref(), Some("0006"));
        assert!(view.failure_class.is_none());
        assert!(!outcome_current_path(&r).unwrap().exists());
        assert!(outcome_applied_path(&r).unwrap().exists());
    }

    #[test]
    fn book_outcome_applies_only_its_epoch() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_stub(&r, Some("0005".into())).unwrap();
        let base = crate::state::load_run_state(&r).unwrap();
        let mut first = base.clone();
        first.run_epoch = 2;
        first.track_id = Some("0005".into());
        let mut second = base;
        second.run_epoch = 4;
        second.track_id = Some("0006".into());
        second.phase = first.phase.clone();
        second.status = RunStatus::Running;
        crate::state::write_run_book(
            &r,
            &crate::state::RunEnvelope::new(0, vec![first.clone(), second.clone()]),
        )
        .unwrap();
        let outcome = PhaseOutcome::success(
            first.phase.clone(),
            OutcomeSource::Cli,
            Some("done".into()),
            None,
            Some(2),
        );
        let view = write_and_apply(&r, outcome).unwrap();
        assert_eq!(view.run_epoch, 2);
        let book = crate::state::load_run_book(&r).unwrap();
        let crate::state::RunBook::Many(envelope) = book else {
            panic!("outcome apply collapsed the book");
        };
        let kept = envelope
            .runs
            .iter()
            .find(|entry| entry.run_epoch == 4)
            .unwrap();
        assert_eq!(kept.status, RunStatus::Running);
        assert_eq!(kept.phase, second.phase);
        assert!(!r_state_current(&r));
    }

    fn spawn_epoch_holder(
        rec: ProjectRecord,
        epoch: u64,
        session_id: &str,
    ) -> (
        std::thread::JoinHandle<()>,
        std::sync::mpsc::Receiver<String>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let session_id = session_id.to_string();
        let handle = std::thread::spawn(move || {
            let _scope = crate::state::RunScope::enter(epoch).expect("scope");
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async move {
                let cwd = crate::worktree::epoch_dir(&rec, epoch).expect("epoch dir");
                assert!(cwd.is_dir(), "epoch {epoch} worktree missing");
                let session = crate::harness::grok::GrokSession::start_mock(
                    cwd,
                    crate::harness::grok::mock_handshake_ok(&session_id),
                    std::time::Duration::from_secs(2),
                )
                .await
                .expect("mock session");
                let rec_hold = rec.clone();
                let hold = tokio::spawn(async move {
                    crate::harness::pool::hold_loop_with_session(rec_hold, session).await
                });
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                let persist = crate::state::resolve_state_dir(&rec)
                    .expect("state dir")
                    .join("sessions")
                    .join(epoch.to_string())
                    .join("harness-grok.json");
                let addr = loop {
                    if let Some(addr) = std::fs::read_to_string(&persist)
                        .ok()
                        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                        .and_then(|body| {
                            body.get("control_addr")
                                .and_then(|value| value.as_str())
                                .map(str::to_string)
                        })
                        .filter(|addr| !addr.is_empty())
                    {
                        break addr;
                    }
                    if std::time::Instant::now() >= deadline {
                        panic!("epoch {epoch} holder never published a control address");
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                };
                tx.send(addr).expect("addr");
                hold.await.expect("holder join").expect("holder loop");
            });
            let _ = _scope;
        });
        (handle, rx)
    }

    #[test]
    fn finishing_one_run_releases_only_that_epoch() {
        let _guard = crate::config::test_env_lock();
        let repo = tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = crate::git_command::command()
                .args(args)
                .current_dir(repo.path())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?} {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(repo.path().join("README.md"), b"seed\n").unwrap();
        git(&["add", "README.md"]);
        git(&["commit", "-m", "seed"]);
        let mut r = rec(repo.path());
        r.execution_repo = Some(repo.path().to_path_buf());
        crate::run::run_with_driver(
            &r,
            Some("0005".into()),
            crate::workflow::WorkflowDriver::FileWait,
        )
        .unwrap();
        r.worktree_isolation = true;
        let base = crate::state::load_run_state(&r).unwrap();
        let mut first = base.clone();
        first.run_epoch = 2;
        first.status = RunStatus::Running;
        first.phase = crate::workflow::graph::PHASE_ADVANCE.into();
        first.track_id = Some("0005".into());
        let mut second = base;
        second.run_epoch = 4;
        second.status = RunStatus::Running;
        second.phase = crate::workflow::graph::PHASE_IMPLEMENT.into();
        second.track_id = Some("0006".into());
        crate::state::write_run_book(
            &r,
            &crate::state::RunEnvelope::new(0, vec![first.clone(), second.clone()]),
        )
        .unwrap();
        {
            let _scope = crate::state::RunScope::enter(2).unwrap();
            crate::worktree::prepare_epoch_in(&r, 2, true).unwrap();
        }
        {
            let _scope = crate::state::RunScope::enter(4).unwrap();
            crate::worktree::prepare_epoch_in(&r, 4, true).unwrap();
        }
        let tree_a = crate::worktree::epoch_dir(&r, 2).unwrap();
        let tree_b = crate::worktree::epoch_dir(&r, 4).unwrap();
        assert!(tree_a.is_dir() && tree_b.is_dir());
        let (thread_a, addr_a) = spawn_epoch_holder(r.clone(), 2, "sess-2");
        let (thread_b, addr_b) = spawn_epoch_holder(r.clone(), 4, "sess-4");
        let addr_a = addr_a
            .recv_timeout(std::time::Duration::from_secs(4))
            .expect("epoch 2 address");
        let addr_b = addr_b
            .recv_timeout(std::time::Duration::from_secs(4))
            .expect("epoch 4 address");
        let ping = |addr: &str,
                    session: &str,
                    tree: &std::path::Path|
         -> std::result::Result<(), String> {
            let mut stream = std::net::TcpStream::connect(addr).map_err(|e| e.to_string())?;
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .map_err(|e| e.to_string())?;
            stream
                .set_write_timeout(Some(std::time::Duration::from_secs(2)))
                .map_err(|e| e.to_string())?;
            std::io::Write::write_all(&mut stream, b"{\"op\":\"ping\"}\n")
                .map_err(|e| e.to_string())?;
            let mut line = String::new();
            std::io::BufRead::read_line(&mut std::io::BufReader::new(&stream), &mut line)
                .map_err(|e| e.to_string())?;
            let body: serde_json::Value =
                serde_json::from_str(line.trim()).map_err(|_| line.clone())?;
            if body["ok"] != true {
                return Err(line);
            }
            if body["harness"]["session_id"] != session {
                return Err(line);
            }
            let cwd = body["harness"]["cwd"]
                .as_str()
                .ok_or_else(|| line.clone())?;
            if !crate::worktree::paths_same(std::path::Path::new(cwd), tree) {
                return Err(format!("holder cwd {cwd} is not {}", tree.display()));
            }
            Ok(())
        };
        ping(&addr_a, "sess-2", &tree_a).expect("epoch 2 holder did not answer before finish");
        ping(&addr_b, "sess-4", &tree_b).expect("epoch 4 holder did not answer before finish");
        let ref_a = format!("refs/coordinator/checkpoints/{}/2", r.id);
        let ref_b = format!("refs/coordinator/checkpoints/{}/4", r.id);
        git(&["update-ref", &ref_a, "HEAD"]);
        git(&["update-ref", &ref_b, "HEAD"]);
        let outcome = PhaseOutcome::success(
            crate::workflow::graph::PHASE_ADVANCE,
            OutcomeSource::Test,
            None,
            None,
            Some(2),
        );
        let view = write_and_apply(&r, outcome).unwrap();
        assert_eq!(view.status, RunStatus::Idle);
        assert!(
            view.last_event.contains("backlog clear"),
            "{}",
            view.last_event
        );
        let a_answers = ping(&addr_a, "sess-2", &tree_a).is_ok();
        assert!(
            tree_a.is_dir() || !a_answers,
            "A's worktree was removed while A still answers Ping"
        );
        assert!(
            !tree_a.is_dir() && !a_answers,
            "finishing A left the epoch mounted after the holder stopped answering: tree={} ping={a_answers}",
            tree_a.is_dir()
        );
        assert!(tree_b.is_dir(), "B's worktree was removed");
        let (kept_epoch, kept_status, kept_phase) = match crate::state::load_run_book(&r).unwrap() {
            crate::state::RunBook::One(state) => (state.run_epoch, state.status, state.phase),
            crate::state::RunBook::Many(envelope) => {
                let kept = envelope
                    .runs
                    .iter()
                    .find(|entry| entry.run_epoch == 4)
                    .expect("B missing from the book");
                (kept.run_epoch, kept.status, kept.phase.clone())
            }
        };
        assert_eq!(kept_epoch, 4);
        assert_eq!(kept_status, RunStatus::Running);
        assert_eq!(kept_phase, crate::workflow::graph::PHASE_IMPLEMENT);
        let shown = crate::git_command::command()
            .args(["show-ref", &ref_b])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            shown.status.success(),
            "B's checkpoint ref was removed: {}",
            String::from_utf8_lossy(&shown.stderr)
        );
        ping(&addr_b, "sess-4", &tree_b).expect("B stopped answering Ping");
        let shutdown = |addr: &str| {
            if let Ok(mut stream) = std::net::TcpStream::connect(addr) {
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                let _ = std::io::Write::write_all(&mut stream, b"{\"op\":\"shutdown\"}\n");
                let mut line = String::new();
                let _ =
                    std::io::BufRead::read_line(&mut std::io::BufReader::new(stream), &mut line);
            }
        };
        shutdown(&addr_a);
        shutdown(&addr_b);
        thread_a.join().expect("epoch 2 thread");
        thread_b.join().expect("epoch 4 thread");
    }

    fn r_state_current(record: &ProjectRecord) -> bool {
        crate::state::resolve_state_dir(record)
            .unwrap()
            .join("outcomes")
            .join("current.json")
            .exists()
    }

    #[test]
    fn apply_success_paused_stays_paused() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_stub(&r, None).unwrap();
        run::pause(&r).unwrap();
        let o = PhaseOutcome::success(STUB_PHASE_ACTIVE, OutcomeSource::File, None, None, None);
        let view = apply(&r, o).unwrap();
        assert_eq!(view.status, RunStatus::Paused);
        assert_eq!(view.phase, STUB_PHASE_COMPLETED);
        assert!(view.last_event.contains("paused"));
    }

    #[test]
    fn apply_failure_stops_with_class() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_stub(&r, None).unwrap();
        let o = PhaseOutcome::failure(
            STUB_PHASE_ACTIVE,
            FailureClass::Permission,
            OutcomeSource::Http,
            Some("denied".into()),
            None,
        );
        let view = apply(&r, o).unwrap();
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.phase, STUB_PHASE_FAILED);
        assert_eq!(view.failure_class, Some(FailureClass::Permission));
    }

    #[test]
    fn consecutive_failures_skip_policy_and_bounce_and_reset_on_success() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_stub(&r, Some("0069".into())).unwrap();
        let fail = |phase: &str, class: FailureClass, msg: &str| {
            apply(
                &r,
                PhaseOutcome::failure(phase, class, OutcomeSource::Test, Some(msg.into()), None),
            )
            .unwrap();
        };
        fail(STUB_PHASE_ACTIVE, FailureClass::HarnessCrash, "boom");
        assert_eq!(
            load_run_state(&r)
                .unwrap()
                .consecutive_failures
                .get("0069")
                .copied(),
            Some(1)
        );
        run_stub(&r, Some("0069".into())).unwrap();
        fail(
            STUB_PHASE_ACTIVE,
            FailureClass::Permission,
            "policy: block dependency-manifest: Cargo.toml",
        );
        assert_eq!(
            load_run_state(&r)
                .unwrap()
                .consecutive_failures
                .get("0069")
                .copied(),
            Some(1),
            "a policy block must not increment"
        );

        run_stub(&r, Some("0069".into())).unwrap();
        {
            let mut state = load_run_state(&r).unwrap();
            state.phase = crate::workflow::graph::PHASE_CROSS_MODEL.into();
            save_run_state(&r, &state).unwrap();
        }
        let bounced = apply(
            &r,
            PhaseOutcome::failure(
                crate::workflow::graph::PHASE_CROSS_MODEL,
                FailureClass::Difficulty,
                OutcomeSource::Test,
                Some("gate".into()),
                None,
            ),
        )
        .unwrap();
        assert_eq!(
            bounced.phase,
            crate::workflow::graph::PHASE_ADDRESS_FINDINGS
        );
        assert_eq!(
            load_run_state(&r)
                .unwrap()
                .consecutive_failures
                .get("0069")
                .copied(),
            Some(1),
            "a gate bounce must not increment"
        );

        run::stop(&r).unwrap();
        run_stub(&r, Some("0069".into())).unwrap();
        apply(
            &r,
            PhaseOutcome::success(
                STUB_PHASE_ACTIVE,
                OutcomeSource::Test,
                Some("ok".into()),
                None,
                None,
            ),
        )
        .unwrap();
        assert_eq!(
            load_run_state(&r)
                .unwrap()
                .consecutive_failures
                .get("0069")
                .copied(),
            Some(0)
        );
    }

    #[test]
    fn failure_class_round_trip_all() {
        for class in FailureClass::ALL {
            let dir = tempdir().unwrap();
            let r = rec(dir.path());
            run_stub(&r, None).unwrap();
            let o =
                PhaseOutcome::failure(STUB_PHASE_ACTIVE, class, OutcomeSource::Test, None, None);
            let view = apply(&r, o).unwrap();
            assert_eq!(view.failure_class, Some(class));
            let status = run::status(&r).unwrap();
            assert_eq!(status.failure_class, Some(class));
        }
    }

    #[test]
    fn reject_apply_while_idle() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        let o = PhaseOutcome::success(STUB_PHASE_ACTIVE, OutcomeSource::Cli, None, None, None);
        assert!(apply(&r, o).is_err());
    }

    #[test]
    fn failure_class_all_is_eleven_with_blocked_at_nine_and_serde_blocked() {
        assert_eq!(FailureClass::ALL.len(), 11);
        assert_eq!(FailureClass::ALL[9], FailureClass::Blocked);
        assert_eq!(FailureClass::ALL[10], FailureClass::ReviewComments);
        assert_eq!(FailureClass::ReviewComments.as_str(), "review_comments");
        assert_eq!(
            FailureClass::parse("review_comments").unwrap(),
            FailureClass::ReviewComments
        );
        assert_eq!(FailureClass::Blocked.as_str(), "blocked");
        assert_eq!(
            FailureClass::parse("blocked").unwrap(),
            FailureClass::Blocked
        );
        assert_eq!(
            serde_json::to_string(&FailureClass::Blocked).unwrap(),
            "\"blocked\""
        );
        assert_eq!(
            serde_json::from_str::<FailureClass>("\"blocked\"").unwrap(),
            FailureClass::Blocked
        );
        for class in FailureClass::ALL {
            assert_eq!(FailureClass::parse(class.as_str()).unwrap(), class);
        }
    }

    #[test]
    fn old_schema_readers_qualify_the_blocked_downgrade_symptoms() {
        // Mirror of the pre-0098 enum: an older binary cannot deserialize `blocked`.
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "snake_case")]
        #[allow(dead_code)]
        enum LegacyFailureClass {
            Permission,
            ModelExhaustion,
            Difficulty,
            HarnessCrash,
            Timeout,
            CiFailed,
            SkillMissing,
            ReviewNeverSettles,
            Gate,
        }
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct LegacyRunState {
            failure_class: Option<LegacyFailureClass>,
        }
        assert!(serde_json::from_str::<LegacyFailureClass>("\"blocked\"").is_err());

        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_stub(&r, Some("0098".into())).unwrap();
        ensure_outcomes_dir(&r).unwrap();

        // `current.json`: an unreadable class is a soft skip for the poller and an
        // error for the direct CLI loader (an old binary's tick is skipped).
        let mut current = serde_json::to_value(PhaseOutcome::failure(
            STUB_PHASE_ACTIVE,
            FailureClass::Blocked,
            OutcomeSource::File,
            Some("blocked".into()),
            None,
        ))
        .unwrap();
        current["failure_class"] = serde_json::json!("legacy_unknown");
        std::fs::write(
            outcome_current_path(&r).unwrap(),
            serde_json::to_vec(&current).unwrap(),
        )
        .unwrap();
        assert!(try_load_current_outcome(&r).is_none());
        assert!(load_current_outcome(&r).is_err());

        // `run-state.json`: the same kind of unknown value hard-fails the run-state load,
        // so status / run / failure resolve are unavailable to the old binary.
        let state_path = crate::state::resolve_state_dir(&r)
            .unwrap()
            .join("run-state.json");
        let mut state_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
        state_json["failure_class"] = serde_json::json!("legacy_unknown");
        std::fs::write(&state_path, serde_json::to_vec_pretty(&state_json).unwrap()).unwrap();
        assert!(load_run_state(&r).is_err());
        assert!(
            serde_json::from_str::<LegacyRunState>(&std::fs::read_to_string(&state_path).unwrap())
                .is_err()
        );

        // The current schema reads `blocked` in both files.
        state_json["failure_class"] = serde_json::json!("blocked");
        std::fs::write(&state_path, serde_json::to_vec_pretty(&state_json).unwrap()).unwrap();
        assert_eq!(
            load_run_state(&r).unwrap().failure_class,
            Some(FailureClass::Blocked)
        );
        current["failure_class"] = serde_json::json!("blocked");
        std::fs::write(
            outcome_current_path(&r).unwrap(),
            serde_json::to_vec(&current).unwrap(),
        )
        .unwrap();
        assert_eq!(
            load_current_outcome(&r).unwrap().unwrap().failure_class,
            Some(FailureClass::Blocked)
        );
    }

    #[test]
    fn blocked_failure_does_not_increment_or_reset_consecutive_failures() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_stub(&r, Some("0098".into())).unwrap();
        // Zero -> blocked stays zero.
        apply(
            &r,
            PhaseOutcome::failure(
                STUB_PHASE_ACTIVE,
                FailureClass::Blocked,
                OutcomeSource::Test,
                Some("prerequisite".into()),
                None,
            ),
        )
        .unwrap();
        let state = load_run_state(&r).unwrap();
        assert_eq!(state.status, RunStatus::Stopped);
        assert_eq!(state.failure_class, Some(FailureClass::Blocked));
        assert_eq!(state.consecutive_failures.get("0098").copied(), None);

        // A blocked turn after a real failure keeps the nonzero count; `failure resolve`
        // retains it too.
        resolve_for_test(&r);
        run_stub(&r, Some("0098".into())).unwrap();
        apply(
            &r,
            PhaseOutcome::failure(
                STUB_PHASE_ACTIVE,
                FailureClass::HarnessCrash,
                OutcomeSource::Test,
                Some("boom".into()),
                None,
            ),
        )
        .unwrap();
        assert_eq!(
            load_run_state(&r)
                .unwrap()
                .consecutive_failures
                .get("0098")
                .copied(),
            Some(1)
        );
        resolve_for_test(&r);
        run_stub(&r, Some("0098".into())).unwrap();
        apply(
            &r,
            PhaseOutcome::failure(
                STUB_PHASE_ACTIVE,
                FailureClass::Blocked,
                OutcomeSource::Test,
                Some("prerequisite".into()),
                None,
            ),
        )
        .unwrap();
        assert_eq!(
            load_run_state(&r)
                .unwrap()
                .consecutive_failures
                .get("0098")
                .copied(),
            Some(1),
            "blocked must not increment an existing count"
        );
        resolve_for_test(&r);
        assert_eq!(
            load_run_state(&r)
                .unwrap()
                .consecutive_failures
                .get("0098")
                .copied(),
            Some(1),
            "resolve retains the count"
        );

        // A later ordinary success resets it.
        run_stub(&r, Some("0098".into())).unwrap();
        apply(
            &r,
            PhaseOutcome::success(
                STUB_PHASE_ACTIVE,
                OutcomeSource::Test,
                Some("ok".into()),
                None,
                None,
            ),
        )
        .unwrap();
        assert_eq!(
            load_run_state(&r)
                .unwrap()
                .consecutive_failures
                .get("0098")
                .copied(),
            Some(0)
        );
    }

    #[test]
    fn reject_apply_after_stop() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_stub(&r, None).unwrap();
        run::stop(&r).unwrap();
        let o = PhaseOutcome::success(STUB_PHASE_ACTIVE, OutcomeSource::Cli, None, None, None);
        assert!(apply(&r, o).is_err());
    }

    #[test]
    fn reject_phase_mismatch() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_stub(&r, None).unwrap();
        let o = PhaseOutcome::success("other:phase", OutcomeSource::Cli, None, None, None);
        assert!(apply(&r, o).is_err());
    }

    #[test]
    fn reapply_same_hash_is_noop() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_stub(&r, None).unwrap();
        let o = PhaseOutcome::success(
            STUB_PHASE_ACTIVE,
            OutcomeSource::Cli,
            Some("same".into()),
            None,
            None,
        );
        let v1 = apply(&r, o.clone()).unwrap();
        assert_eq!(v1.status, RunStatus::Idle);
        // Second apply of identical payload is idempotent no-op (even though Idle).
        let v2 = apply(&r, o).unwrap();
        assert_eq!(v2.status, RunStatus::Idle);
        assert_eq!(v2.phase, STUB_PHASE_COMPLETED);
    }

    #[test]
    fn leftover_current_after_idle_poll_consumes_without_transition() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_stub(&r, None).unwrap();
        let o = PhaseOutcome::success(STUB_PHASE_ACTIVE, OutcomeSource::Cli, None, None, None);
        apply(&r, o).unwrap();
        // Drop a stale success file while Idle (different content so hash differs).
        let stale = PhaseOutcome::success(
            STUB_PHASE_ACTIVE,
            OutcomeSource::File,
            Some("stale leftover".into()),
            None,
            None,
        );
        save_current_outcome(&r, &stale).unwrap();
        let before = run::status(&r).unwrap();
        let applied = poll_try_apply(&r, stale).unwrap();
        assert!(applied.is_none());
        let after = run::status(&r).unwrap();
        assert_eq!(before.status, after.status);
        assert!(!outcome_current_path(&r).unwrap().exists());
    }

    #[test]
    fn run_epoch_mismatch_rejects() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_stub(&r, None).unwrap();
        let mut o = PhaseOutcome::success(STUB_PHASE_ACTIVE, OutcomeSource::Cli, None, None, None);
        o.run_epoch = Some(999);
        assert!(apply(&r, o).is_err());
    }

    #[test]
    fn track_id_retained_on_run_without_track() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_stub(&r, Some("0005".into())).unwrap();
        run::stop(&r).unwrap();
        let s = run_stub(&r, None).unwrap();
        assert_eq!(s.track_id.as_deref(), Some("0005"));
    }

    #[test]
    fn paths_under_state_dir() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        assert!(
            outcome_current_path(&r).unwrap().ends_with(
                std::path::Path::new(".coordinator")
                    .join("outcomes")
                    .join("current.json")
            )
        );
        assert!(outcome_roles_dir(&r).unwrap().ends_with("roles"));
    }

    #[test]
    fn write_while_idle_does_not_leave_replayable_file() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        let o = PhaseOutcome::success(STUB_PHASE_ACTIVE, OutcomeSource::Cli, None, None, None);
        assert!(write_and_apply(&r, o).is_err());
        assert!(
            !outcome_current_path(&r).unwrap().exists(),
            "failed write must not leave current.json"
        );
    }

    #[test]
    fn new_run_clears_stale_current_json() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        let stale = PhaseOutcome::success(
            STUB_PHASE_ACTIVE,
            OutcomeSource::File,
            Some("stale from prior".into()),
            None,
            None,
        );
        save_current_outcome(&r, &stale).unwrap();
        assert!(outcome_current_path(&r).unwrap().exists());
        run_stub(&r, None).unwrap();
        assert!(
            !outcome_current_path(&r).unwrap().exists(),
            "run must clear active outcome file"
        );
        let tick = crate::watch::poll_once(&r).unwrap();
        assert!(tick.is_none());
        let s = run::status(&r).unwrap();
        assert_eq!(s.status, RunStatus::Running);
        assert_eq!(s.phase, STUB_PHASE_ACTIVE);
    }

    #[test]
    fn resume_after_paused_success_goes_idle() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_stub(&r, None).unwrap();
        run::pause(&r).unwrap();
        let o = PhaseOutcome::success(STUB_PHASE_ACTIVE, OutcomeSource::Cli, None, None, None);
        apply(&r, o).unwrap();
        let s = run::resume(&r).unwrap();
        assert_eq!(s.status, RunStatus::Idle);
        assert_eq!(s.phase, STUB_PHASE_COMPLETED);
        assert!(s.last_event.contains("release hold"));
    }

    #[test]
    fn try_timeout_under_lock_fires_on_project_plan_budget() {
        use crate::config::{ENV_COORDINATOR_HOME, test_env_lock};
        use crate::run::run_with_driver;
        use crate::workflow::WorkflowDriver;
        use crate::workflow::timeouts::ENV_PHASE_TIMEOUT_SECS;
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        unsafe {
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let dir = tempdir().unwrap();
        let mut r = rec(dir.path());
        r.phase_timeouts_secs.insert("plan".into(), 1);
        run_with_driver(&r, None, WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase_started_at = Some(Utc::now() - chrono::Duration::seconds(5));
        save_run_state(&r, &state).unwrap();
        let view = try_timeout_under_lock(&r).unwrap().expect("timeout");
        assert_eq!(view.failure_class, Some(FailureClass::Timeout));
        assert_eq!(view.status, RunStatus::Stopped);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }
}
