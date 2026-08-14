//! Per-project run-state persistence under `.coordinator/`.

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::state_dir_override;
use crate::error::Result;
use crate::outcome::FailureClass;
use crate::persist::atomic_write_json;
use crate::registry::ProjectRecord;

/// Run lifecycle status (stub phases + outcome-driven completion in 0005).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum RunStatus {
    Idle,
    Running,
    Paused,
    Stopped,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Running => "Running",
            Self::Paused => "Paused",
            Self::Stopped => "Stopped",
        }
    }
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// ADR-0024 stop note (persisted in run-state / status JSON).
pub const STOP_LAST_EVENT: &str = "stopped: no merge; sessions left for attach";

/// Default stub phase while Running/Paused.
pub const STUB_PHASE_ACTIVE: &str = "stub:active";

/// Default phase when Idle (no active run).
pub const STUB_PHASE_IDLE: &str = "stub:idle";

/// Phase after operator Stop (not outcome failure).
pub const STUB_PHASE_STOPPED: &str = "stub:stopped";

/// Phase after successful outcome apply (single stub leftover).
pub const STUB_PHASE_COMPLETED: &str = "stub:completed";

/// Phase after failure outcome apply (including timeout).
pub const STUB_PHASE_FAILED: &str = "stub:failed";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunState {
    pub project_id: String,
    pub status: RunStatus,
    /// Stub phase string; advanced by Phase Outcome apply (0005), not by wall clock alone.
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_id: Option<String>,
    pub updated_at: DateTime<Utc>,
    pub last_event: String,
    /// Monotonic epoch; incremented each successful `run` into Running.
    #[serde(default)]
    pub run_epoch: u64,
    /// When the current phase clock started (Running entry / fresh run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_started_at: Option<DateTime<Utc>>,
    /// Accumulated pause duration for the current phase (timeout freeze).
    #[serde(default)]
    pub total_paused_ms: u64,
    /// When the current Pause began (None if not paused).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_started_at: Option<DateTime<Utc>>,
    /// Last applied failure class (cleared on success / fresh run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<FailureClass>,
    /// Planner handoff from outcome `metadata.next_track`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_track: Option<String>,
    /// Content hash of last successfully applied outcome (consume marker).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_applied_outcome_hash: Option<String>,
    /// `"canonical_v1"` when started by public `run`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// Driver chosen at `run` (serde default `adapter`).
    #[serde(default)]
    pub driver: crate::workflow::WorkflowDriver,
    /// Slugs still open in `plan-review`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_roles: Vec<String>,
    /// Tick inject-once marker (`run_epoch` + phase).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_driven_phase: Option<String>,
    /// Token-idle CI watch (0010). Cleared on fresh `run`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci: Option<CiWatchState>,
    /// Cross-model review gate (0011). Cleared on fresh `run`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewWatchState>,
    /// First time the progress watchdog fired this inject (0026). Cleared on resume / fresh run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stalled_at: Option<DateTime<Utc>>,
    /// Completed pause intervals for this phase (stall idle window; 0026).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pause_spans: Vec<PauseSpan>,
}

/// One completed pause interval (start inclusive, end exclusive-ish).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PauseSpan {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

/// Persisted `cross-model-review` watcher (additive; old run-state.json loads as None).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewWatchState {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempted: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    /// `PASS` | `PASS_WITH_LOWS` | `FAIL`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    /// Relative name e.g. `review.codex.md`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<String>,
}

/// Persisted `ci-wait` watcher (additive; old run-state.json loads as None).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CiWatchState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_poll_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_summary: Option<String>,
    /// `"done"` | `"skipped"` | `"queued"`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge: Option<String>,
    /// Fingerprint of last check/run set (interval reset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_key: Option<String>,
    /// Interval to wait before the next spawn (ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_interval_ms: Option<u64>,
}

impl RunState {
    pub fn idle(project_id: impl Into<String>) -> Self {
        Self {
            project_id: project_id.into(),
            status: RunStatus::Idle,
            phase: STUB_PHASE_IDLE.into(),
            track_id: None,
            updated_at: Utc::now(),
            last_event: "idle: no run".into(),
            run_epoch: 0,
            phase_started_at: None,
            total_paused_ms: 0,
            pause_started_at: None,
            failure_class: None,
            next_track: None,
            last_applied_outcome_hash: None,
            workflow: None,
            driver: crate::workflow::WorkflowDriver::default(),
            pending_roles: Vec::new(),
            last_driven_phase: None,
            ci: None,
            review: None,
            stalled_at: None,
            pause_spans: Vec::new(),
        }
    }

    /// Effective running elapsed for timeout (excludes paused intervals).
    ///
    /// While currently Paused, the open pause interval is also excluded (timer frozen).
    pub fn effective_running_elapsed(&self, now: DateTime<Utc>) -> Duration {
        let Some(started) = self.phase_started_at else {
            return Duration::ZERO;
        };
        let wall = (now - started).num_milliseconds().max(0) as u64;
        let mut paused = self.total_paused_ms;
        if let Some(pstart) = self.pause_started_at {
            paused = paused.saturating_add((now - pstart).num_milliseconds().max(0) as u64);
        }
        Duration::from_millis(wall.saturating_sub(paused))
    }
}

/// Status JSON fields (CLI + GET /v1/status). Additive layout fields from 0006.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusView {
    pub project_id: String,
    pub path: PathBuf,
    /// Copied from `ProjectRecord` so cards do not join the registry again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub status: RunStatus,
    pub phase: String,
    pub track_id: Option<String>,
    pub last_event: String,
    #[serde(default)]
    pub run_epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<FailureClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_track: Option<String>,
    /// Layout profile (nested | multi_sibling | single_root).
    #[serde(default)]
    pub layout_profile: crate::layout::LayoutProfile,
    /// Resolved primary execution repo (`null` when nested and unknown).
    #[serde(default)]
    pub execution_repo: Option<PathBuf>,
    /// Resolved conductor directory.
    #[serde(default)]
    pub conductor_dir: Option<PathBuf>,
    /// Additive harness summary (`harness.grok`) when a session exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<crate::harness::HarnessStatusBundle>,
    /// Additive workflow object (`id`, `driver`, `pending_roles`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<WorkflowView>,
    /// Path to `{state_dir}/FAILURE.md` when present; `null` when absent.
    #[serde(default)]
    pub failure_artifact: Option<PathBuf>,
    /// Token-idle CI watch; `null` when phase ≠ `ci-wait` and no persisted state.
    #[serde(default)]
    pub ci: Option<CiStatusView>,
    /// Cross-model review; `null` when phase ≠ `cross-model-review` and no persisted state.
    #[serde(default)]
    pub review: Option<ReviewStatusView>,
    /// Last adapter heartbeat (`harness-progress.json`); omitted when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_progress_at: Option<DateTime<Utc>>,
    /// Progress stall (0026). `null` when not stalled. Run stays Running.
    #[serde(default)]
    pub stall: Option<StallView>,
}

/// Status JSON `stall` object (0026).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StallView {
    pub since: DateTime<Utc>,
    pub idle_secs: u64,
}

/// Status JSON `review` object (0011).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewStatusView {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempted: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<String>,
}

/// Status JSON `ci` object (0010).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CiStatusView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_summary: Option<String>,
    pub interval_ms: u64,
    pub auto_merge: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge: Option<String>,
}

/// Status JSON `workflow` object (0008).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub driver: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_roles: Vec<String>,
}

impl StatusView {
    pub fn from_record(record: &ProjectRecord, state: &RunState) -> Self {
        let paths = crate::layout::resolve(record);
        Self {
            project_id: record.id.clone(),
            path: record.path.clone(),
            display_name: record.display_name.clone(),
            status: state.status,
            phase: state.phase.clone(),
            track_id: state.track_id.clone(),
            last_event: state.last_event.clone(),
            run_epoch: state.run_epoch,
            phase_started_at: state.phase_started_at,
            failure_class: state.failure_class,
            next_track: state.next_track.clone(),
            layout_profile: record.layout_profile,
            execution_repo: paths.execution_repo,
            conductor_dir: Some(paths.conductor_dir),
            harness: crate::harness::status_bundle_sync(record),
            workflow: Some(WorkflowView {
                id: state.workflow.clone(),
                driver: state.driver.as_str().to_string(),
                pending_roles: state.pending_roles.clone(),
            }),
            failure_artifact: crate::notify::artifact::existing_path(record),
            ci: ci_status_view(record, state),
            review: review_status_view(state),
            last_progress_at: if state.status == RunStatus::Running {
                crate::workflow::watchdog::last_progress_at(record)
            } else {
                None
            },
            stall: stall_status_view(record, state),
        }
    }
}

fn stall_status_view(record: &ProjectRecord, state: &RunState) -> Option<StallView> {
    if state.status != RunStatus::Running {
        return None;
    }
    let since = state.stalled_at?;
    let last = crate::workflow::watchdog::last_progress_at(record)
        .or(state.phase_started_at)
        .unwrap_or(since);
    let idle = crate::workflow::watchdog::idle_since(last, state, Utc::now());
    Some(StallView {
        since,
        idle_secs: idle.as_secs(),
    })
}

fn ci_status_view(record: &ProjectRecord, state: &RunState) -> Option<CiStatusView> {
    if state.phase != crate::workflow::graph::PHASE_CI_WAIT && state.ci.is_none() {
        return None;
    }
    let interval_ms = state
        .ci
        .as_ref()
        .and_then(|c| c.next_interval_ms)
        .unwrap_or_else(crate::ci::initial_interval_ms);
    let (pr, pr_url, head_sha, last_summary, merge) = match state.ci.as_ref() {
        Some(c) => (
            c.pr_number,
            c.pr_url.clone(),
            c.head_sha.clone(),
            c.last_summary.clone(),
            c.merge.clone(),
        ),
        None => (None, None, None, None, None),
    };
    Some(CiStatusView {
        pr,
        pr_url,
        head_sha,
        last_summary,
        interval_ms,
        auto_merge: record.auto_merge,
        merge,
    })
}

fn review_status_view(state: &RunState) -> Option<ReviewStatusView> {
    if state.phase != crate::workflow::graph::PHASE_CROSS_MODEL && state.review.is_none() {
        return None;
    }
    let (attempted, active, verdict, report) = match state.review.as_ref() {
        Some(r) => (
            r.attempted.clone(),
            r.active.clone(),
            r.verdict.clone(),
            r.report.clone(),
        ),
        None => (Vec::new(), None, None, None),
    };
    Some(ReviewStatusView {
        attempted,
        active,
        verdict,
        report,
    })
}

/// Resolve state directory for a project.
///
/// Priority:
/// 1. `COORDINATOR_STATE_DIR` env → `{override}/{project_id}/` (keyed per project;
///    avoids multi-project collisions when the env override is shared)
/// 2. `record.state_dir` (already project-specific)
/// 3. `{workspace_path}/.coordinator`
///
/// Empty `COORDINATOR_STATE_DIR` is an error.
pub fn resolve_state_dir(record: &ProjectRecord) -> Result<PathBuf> {
    if let Some(over) = state_dir_override()? {
        return Ok(over.join(&record.id));
    }
    if let Some(ref sd) = record.state_dir {
        return Ok(sd.clone());
    }
    Ok(record.path.join(".coordinator"))
}

pub fn run_state_path(record: &ProjectRecord) -> Result<PathBuf> {
    Ok(resolve_state_dir(record)?.join("run-state.json"))
}

pub fn load_run_state(record: &ProjectRecord) -> Result<RunState> {
    let path = run_state_path(record)?;
    if !path.exists() {
        return Ok(RunState::idle(&record.id));
    }
    let text = std::fs::read_to_string(&path)?;
    let state: RunState = serde_json::from_str(&text)?;
    Ok(state)
}

pub fn save_run_state(record: &ProjectRecord, state: &RunState) -> Result<()> {
    let path = run_state_path(record)?;
    atomic_write_json(&path, state)
}

/// Ensure state dir exists (creates `.coordinator` as needed).
pub fn ensure_state_dir(record: &ProjectRecord) -> Result<PathBuf> {
    let dir = resolve_state_dir(record)?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Cross-process exclusive lock for run-state mutation (mkdir is atomic).
///
/// Held across load → decide → save so concurrent CLI vs serve first-commit-wins.
/// Stale locks older than 60s are broken (process crash recovery).
pub fn with_run_state_lock<T, F>(record: &ProjectRecord, f: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    ensure_state_dir(record)?;
    let lock_path = resolve_state_dir(record)?.join(".run-state.lock");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match std::fs::create_dir(&lock_path) {
            Ok(()) => {
                let result = f();
                let _ = std::fs::remove_dir(&lock_path);
                return result;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Break stale lock if older than 60s.
                if let Ok(meta) = std::fs::metadata(&lock_path)
                    && let Ok(modified) = meta.modified()
                    && let Ok(age) = std::time::SystemTime::now().duration_since(modified)
                    && age > std::time::Duration::from_secs(60)
                {
                    let _ = std::fs::remove_dir(&lock_path);
                    continue;
                }
                if std::time::Instant::now() >= deadline {
                    return Err(crate::error::CoordinatorError::Message(
                        "timed out waiting for run-state lock".into(),
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::path::Path;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn sample_record(path: &Path) -> ProjectRecord {
        ProjectRecord {
            id: Uuid::new_v4().to_string(),
            path: path.to_path_buf(),
            display_name: Some("test".into()),
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: std::collections::BTreeMap::new(),
            state_dir: None,
            auto_merge: true,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn default_state_dir_is_dot_coordinator() {
        let dir = tempdir().unwrap();
        let rec = sample_record(dir.path());
        assert_eq!(
            resolve_state_dir(&rec).unwrap(),
            dir.path().join(".coordinator")
        );
    }

    #[test]
    fn save_load_run_state() {
        let dir = tempdir().unwrap();
        // Explicit state_dir so parallel tests that set COORDINATOR_STATE_DIR cannot redirect us.
        let mut rec = sample_record(dir.path());
        rec.state_dir = Some(dir.path().join("explicit-state"));
        let mut state = RunState::idle(&rec.id);
        state.status = RunStatus::Running;
        state.phase = STUB_PHASE_ACTIVE.into();
        state.last_event = "run: started stub".into();
        state.run_epoch = 1;
        save_run_state(&rec, &state).unwrap();
        assert!(run_state_path(&rec).unwrap().exists());
        let loaded = load_run_state(&rec).unwrap();
        assert_eq!(loaded.status, RunStatus::Running);
        assert_eq!(loaded.phase, STUB_PHASE_ACTIVE);
        assert_eq!(loaded.run_epoch, 1);
    }

    #[test]
    fn loads_legacy_run_state_without_0005_fields() {
        let dir = tempdir().unwrap();
        let mut rec = sample_record(dir.path());
        rec.state_dir = Some(dir.path().join("explicit-state"));
        let path = run_state_path(&rec).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Minimal 0004-shaped JSON (no run_epoch / failure_class / etc.).
        std::fs::write(
            &path,
            r#"{
                "project_id": "legacy",
                "status": "Idle",
                "phase": "stub:idle",
                "updated_at": "2026-08-12T00:00:00Z",
                "last_event": "idle: no run"
            }"#,
        )
        .unwrap();
        let loaded = load_run_state(&rec).unwrap();
        assert_eq!(loaded.run_epoch, 0);
        assert!(loaded.failure_class.is_none());
        assert!(loaded.next_track.is_none());
    }

    #[test]
    fn state_dir_env_override() {
        use crate::config::{ENV_COORDINATOR_STATE_DIR, test_env_lock};

        let _guard = test_env_lock();

        let proj = tempdir().unwrap();
        let over = tempdir().unwrap();
        let rec = sample_record(proj.path());

        // SAFETY: serialized by test_env_lock; restored before drop.
        unsafe {
            std::env::set_var(ENV_COORDINATOR_STATE_DIR, over.path());
        }
        let resolved = resolve_state_dir(&rec).unwrap();
        assert_eq!(resolved, over.path().join(&rec.id));
        assert_eq!(
            run_state_path(&rec).unwrap(),
            over.path().join(&rec.id).join("run-state.json")
        );
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_STATE_DIR);
        }
    }

    #[test]
    fn state_dir_env_keys_by_project_id() {
        use crate::config::{ENV_COORDINATOR_STATE_DIR, test_env_lock};

        let _guard = test_env_lock();

        let over = tempdir().unwrap();
        let p1 = tempdir().unwrap();
        let p2 = tempdir().unwrap();
        let r1 = sample_record(p1.path());
        let r2 = sample_record(p2.path());

        unsafe {
            std::env::set_var(ENV_COORDINATOR_STATE_DIR, over.path());
        }
        assert_ne!(
            resolve_state_dir(&r1).unwrap(),
            resolve_state_dir(&r2).unwrap()
        );
        assert!(
            resolve_state_dir(&r1)
                .unwrap()
                .ends_with(std::path::Path::new(&r1.id))
        );
        assert!(
            resolve_state_dir(&r2)
                .unwrap()
                .ends_with(std::path::Path::new(&r2.id))
        );
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_STATE_DIR);
        }
    }

    #[test]
    fn state_dir_record_override() {
        let proj = tempdir().unwrap();
        let over = tempdir().unwrap();
        let mut rec = sample_record(proj.path());
        rec.state_dir = Some(over.path().to_path_buf());
        assert_eq!(resolve_state_dir(&rec).unwrap(), over.path());
    }

    #[test]
    fn empty_state_dir_env_rejected() {
        use crate::config::{ENV_COORDINATOR_STATE_DIR, test_env_lock};

        let _guard = test_env_lock();
        let proj = tempdir().unwrap();
        let rec = sample_record(proj.path());
        unsafe {
            std::env::set_var(ENV_COORDINATOR_STATE_DIR, "");
        }
        let err = resolve_state_dir(&rec).unwrap_err();
        assert!(err.to_string().contains("empty"));
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_STATE_DIR);
        }
    }

    #[test]
    fn status_serializes_null_execution_repo() {
        let dir = tempdir().unwrap();
        let rec = sample_record(dir.path());
        let state = RunState::idle(&rec.id);
        let view = StatusView::from_record(&rec, &state);
        let json = serde_json::to_value(&view).unwrap();
        assert!(json.as_object().unwrap().contains_key("execution_repo"));
        assert!(json["execution_repo"].is_null());
        assert!(json.as_object().unwrap().contains_key("conductor_dir"));
        assert_eq!(json["layout_profile"], "nested");
        assert!(json.as_object().unwrap().contains_key("failure_artifact"));
        assert!(json["failure_artifact"].is_null());
        assert!(json.as_object().unwrap().contains_key("ci"));
        assert!(json["ci"].is_null());
        assert!(json.as_object().unwrap().contains_key("review"));
        assert!(json["review"].is_null());
        assert!(json.as_object().unwrap().contains_key("stall"));
        assert!(json["stall"].is_null());
        let last = json.get("last_progress_at");
        assert!(
            last.is_none() || last.is_some_and(|v| v.is_null()),
            "idle last_progress_at must be omitted or null"
        );
    }

    #[test]
    fn status_serializes_display_name_when_some_omits_when_none() {
        let dir = tempdir().unwrap();
        let mut rec = sample_record(dir.path());
        rec.display_name = Some("Ops".into());
        let view = StatusView::from_record(&rec, &RunState::idle(&rec.id));
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["display_name"], "Ops");

        rec.display_name = None;
        let view = StatusView::from_record(&rec, &RunState::idle(&rec.id));
        let json = serde_json::to_value(&view).unwrap();
        assert!(
            !json.as_object().unwrap().contains_key("display_name"),
            "None must omit display_name"
        );
    }

    #[test]
    fn effective_elapsed_excludes_pause() {
        let mut state = RunState::idle("p");
        let start = Utc::now() - chrono::Duration::seconds(10);
        state.phase_started_at = Some(start);
        state.total_paused_ms = 3000;
        state.pause_started_at = None;
        let elapsed = state.effective_running_elapsed(Utc::now());
        // ~7s wall-pause; allow clock skew in CI
        assert!(elapsed.as_secs() >= 6 && elapsed.as_secs() <= 8);
    }
}
