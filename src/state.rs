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
pub const STOP_LAST_EVENT: &str = "stopped: no merge; sessions-for-attach deferred to 0007+";

/// Default stub phase while Running/Paused.
pub const STUB_PHASE_ACTIVE: &str = "stub:active";

/// Default phase when Idle (no active run).
pub const STUB_PHASE_IDLE: &str = "stub:idle";

/// Phase after operator Stop (not outcome failure).
pub const STUB_PHASE_STOPPED: &str = "stub:stopped";

/// Phase after successful outcome apply (single stub; multi-phase → 0008).
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
    /// Resolved primary execution repo (null when nested and unknown).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_repo: Option<PathBuf>,
    /// Resolved conductor directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conductor_dir: Option<PathBuf>,
}

impl StatusView {
    pub fn from_record(record: &ProjectRecord, state: &RunState) -> Self {
        let paths = crate::layout::resolve(record);
        Self {
            project_id: record.id.clone(),
            path: record.path.clone(),
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
        }
    }
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
