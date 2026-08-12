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
    load_run_state, resolve_state_dir, save_run_state,
};

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
}

impl FailureClass {
    pub const ALL: [FailureClass; 6] = [
        Self::Permission,
        Self::ModelExhaustion,
        Self::Difficulty,
        Self::HarnessCrash,
        Self::Timeout,
        Self::CiFailed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::ModelExhaustion => "model_exhaustion",
            Self::Difficulty => "difficulty",
            Self::HarnessCrash => "harness_crash",
            Self::Timeout => "timeout",
            Self::CiFailed => "ci_failed",
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
}

impl OutcomeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Http => "http",
            Self::Cli => "cli",
            Self::Timeout => "timeout",
            Self::Test => "test",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "file" => Ok(Self::File),
            "http" => Ok(Self::Http),
            "cli" => Ok(Self::Cli),
            "timeout" => Ok(Self::Timeout),
            "test" => Ok(Self::Test),
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
    /// Free-form this track; 0008 may tighten recognized roles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
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
            role: None,
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
pub fn outcomes_dir(record: &ProjectRecord) -> PathBuf {
    resolve_state_dir(record).join("outcomes")
}

pub fn outcome_current_path(record: &ProjectRecord) -> PathBuf {
    outcomes_dir(record).join("current.json")
}

pub fn outcome_applied_path(record: &ProjectRecord) -> PathBuf {
    outcomes_dir(record).join("current.applied.json")
}

pub fn outcome_history_dir(record: &ProjectRecord) -> PathBuf {
    outcomes_dir(record).join("history")
}

/// Reserved layout for parallel role slots (0008); documented only this track.
pub fn outcome_roles_dir(record: &ProjectRecord) -> PathBuf {
    outcomes_dir(record).join("roles")
}

pub fn ensure_outcomes_dir(record: &ProjectRecord) -> Result<PathBuf> {
    ensure_state_dir(record)?;
    let dir = outcomes_dir(record);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Atomic write of the active Phase Outcome file.
pub fn save_current_outcome(record: &ProjectRecord, outcome: &PhaseOutcome) -> Result<()> {
    outcome.validate()?;
    ensure_outcomes_dir(record)?;
    atomic_write_json(&outcome_current_path(record), outcome)
}

/// Load `current.json` if present. Transient IO/parse errors bubble for CLI; poll uses soft path.
pub fn load_current_outcome(record: &ProjectRecord) -> Result<Option<PhaseOutcome>> {
    let path = outcome_current_path(record);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    let outcome: PhaseOutcome = serde_json::from_str(&text)?;
    Ok(Some(outcome))
}

/// Soft load for pollers: missing → None; share/parse errors → None (skip tick).
pub fn try_load_current_outcome(record: &ProjectRecord) -> Option<PhaseOutcome> {
    let path = outcome_current_path(record);
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

/// Single entry for mutating run-state from a Phase Outcome.
///
/// CLI / HTTP / file poll / timeout synthesizer must call this (not hand-roll transitions).
pub fn apply(record: &ProjectRecord, outcome: PhaseOutcome) -> Result<StatusView> {
    let _guard = apply_lock()
        .lock()
        .map_err(|_| CoordinatorError::Message("outcome apply lock poisoned".into()))?;
    apply_locked(record, outcome)
}

fn apply_locked(record: &ProjectRecord, outcome: PhaseOutcome) -> Result<StatusView> {
    outcome.validate()?;
    ensure_state_dir(record)?;
    let hash = outcome_content_hash(&outcome)?;
    let base = load_run_state(record)?;

    // Idempotent re-apply of the same content.
    if base.last_applied_outcome_hash.as_deref() == Some(hash.as_str()) {
        return Ok(StatusView::from_record(record, &base));
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

    if outcome.phase != base.phase {
        return Err(CoordinatorError::Message(format!(
            "outcome phase '{}' does not match current phase '{}'",
            outcome.phase, base.phase
        )));
    }

    let mut state = base.clone();
    match outcome.status {
        OutcomeStatus::Success => {
            state.phase = STUB_PHASE_COMPLETED.into();
            state.failure_class = None;
            if let Some(ref meta) = outcome.metadata
                && let Some(ref next) = meta.next_track
            {
                state.next_track = Some(next.clone());
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
        }
        OutcomeStatus::Failure => {
            let class = outcome.failure_class.expect("validated");
            state.status = RunStatus::Stopped;
            state.phase = STUB_PHASE_FAILED.into();
            state.failure_class = Some(class);
            state.last_event = format_failure_event(&outcome, class);
        }
    }

    state.updated_at = Utc::now();
    state.last_applied_outcome_hash = Some(hash.clone());
    // Phase clock no longer active after apply.
    state.phase_started_at = None;
    state.pause_started_at = None;

    // Cross-process fail-closed: re-load and require base snapshot still current
    // (first valid commit wins; concurrent CLI vs serve last-writer-wins is rejected).
    let fresh = load_run_state(record)?;
    if fresh.last_applied_outcome_hash.as_deref() == Some(hash.as_str()) {
        // Peer already applied identical content.
        clear_active_outcome_file(record);
        return Ok(StatusView::from_record(record, &fresh));
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
    let _ = atomic_write_json(&outcome_applied_path(record), &outcome);
    clear_active_outcome_file(record);

    save_run_state(record, &state)?;
    Ok(StatusView::from_record(record, &state))
}

/// Remove active `current.json` if present (non-fatal).
pub fn clear_active_outcome_file(record: &ProjectRecord) {
    let current = outcome_current_path(record);
    if current.exists() {
        let _ = std::fs::remove_file(&current);
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
    let dir = outcome_history_dir(record);
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
    match apply(record, outcome) {
        Ok(view) => Ok(Some(view)),
        Err(e) => {
            let msg = e.to_string();
            // Consume leftover active file so pollers do not spin on unapplicable content.
            if msg.contains("cannot apply outcome while status is")
                || msg.contains("does not match current phase")
                || msg.contains("does not match state run_epoch")
            {
                let current = outcome_current_path(record);
                if current.exists() {
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
pub fn write_and_apply(record: &ProjectRecord, outcome: PhaseOutcome) -> Result<StatusView> {
    outcome.validate()?;
    save_current_outcome(record, &outcome)?;
    match apply(record, outcome) {
        Ok(view) => Ok(view),
        Err(e) => {
            clear_active_outcome_file(record);
            Err(e)
        }
    }
}

/// Synthesize a timeout failure for the current phase and apply it.
pub fn apply_timeout(record: &ProjectRecord, state: &RunState) -> Result<StatusView> {
    let outcome = PhaseOutcome::failure(
        state.phase.clone(),
        FailureClass::Timeout,
        OutcomeSource::Timeout,
        Some("stub phase budget exceeded".into()),
        Some(state.run_epoch),
    );
    // Persist synthesized outcome for observability, then apply.
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
    use crate::run;
    use crate::state::STUB_PHASE_ACTIVE;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn rec(path: &Path) -> ProjectRecord {
        ProjectRecord {
            id: Uuid::new_v4().to_string(),
            path: path.to_path_buf(),
            display_name: None,
            layout_profile: "nested".into(),
            state_dir: None,
            created_at: Utc::now(),
        }
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
        run::run(&r, Some("0005".into())).unwrap();
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
        assert!(!outcome_current_path(&r).exists());
        assert!(outcome_applied_path(&r).exists());
    }

    #[test]
    fn apply_success_paused_stays_paused() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run::run(&r, None).unwrap();
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
        run::run(&r, None).unwrap();
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
    fn failure_class_round_trip_all() {
        for class in FailureClass::ALL {
            let dir = tempdir().unwrap();
            let r = rec(dir.path());
            run::run(&r, None).unwrap();
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
    fn reject_apply_after_stop() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run::run(&r, None).unwrap();
        run::stop(&r).unwrap();
        let o = PhaseOutcome::success(STUB_PHASE_ACTIVE, OutcomeSource::Cli, None, None, None);
        assert!(apply(&r, o).is_err());
    }

    #[test]
    fn reject_phase_mismatch() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run::run(&r, None).unwrap();
        let o = PhaseOutcome::success("other:phase", OutcomeSource::Cli, None, None, None);
        assert!(apply(&r, o).is_err());
    }

    #[test]
    fn reapply_same_hash_is_noop() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run::run(&r, None).unwrap();
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
        run::run(&r, None).unwrap();
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
        assert!(!outcome_current_path(&r).exists());
    }

    #[test]
    fn run_epoch_mismatch_rejects() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run::run(&r, None).unwrap();
        let mut o = PhaseOutcome::success(STUB_PHASE_ACTIVE, OutcomeSource::Cli, None, None, None);
        o.run_epoch = Some(999);
        assert!(apply(&r, o).is_err());
    }

    #[test]
    fn track_id_retained_on_run_without_track() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run::run(&r, Some("0005".into())).unwrap();
        run::stop(&r).unwrap();
        let s = run::run(&r, None).unwrap();
        assert_eq!(s.track_id.as_deref(), Some("0005"));
    }

    #[test]
    fn paths_under_state_dir() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        assert!(
            outcome_current_path(&r).ends_with(
                std::path::Path::new(".coordinator")
                    .join("outcomes")
                    .join("current.json")
            )
        );
        assert!(outcome_roles_dir(&r).ends_with("roles"));
    }

    #[test]
    fn write_while_idle_does_not_leave_replayable_file() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        let o = PhaseOutcome::success(STUB_PHASE_ACTIVE, OutcomeSource::Cli, None, None, None);
        assert!(write_and_apply(&r, o).is_err());
        assert!(
            !outcome_current_path(&r).exists(),
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
        assert!(outcome_current_path(&r).exists());
        run::run(&r, None).unwrap();
        assert!(
            !outcome_current_path(&r).exists(),
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
        run::run(&r, None).unwrap();
        run::pause(&r).unwrap();
        let o = PhaseOutcome::success(STUB_PHASE_ACTIVE, OutcomeSource::Cli, None, None, None);
        apply(&r, o).unwrap();
        let s = run::resume(&r).unwrap();
        assert_eq!(s.status, RunStatus::Idle);
        assert_eq!(s.phase, STUB_PHASE_COMPLETED);
        assert!(s.last_event.contains("release hold"));
    }
}
