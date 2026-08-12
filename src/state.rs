//! Per-project run-state persistence under `.coordinator/`.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::state_dir_override;
use crate::error::Result;
use crate::persist::atomic_write_json;
use crate::registry::ProjectRecord;

/// Run lifecycle status (stub phases only in track 0004).
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

/// Default stub phase while Running/Paused (never auto-advances in 0004).
pub const STUB_PHASE_ACTIVE: &str = "stub:active";

/// Default phase when Idle.
pub const STUB_PHASE_IDLE: &str = "stub:idle";

/// Phase after Stop.
pub const STUB_PHASE_STOPPED: &str = "stub:stopped";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunState {
    pub project_id: String,
    pub status: RunStatus,
    /// Stub phase string — does not auto-advance (timeouts → 0005+).
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_id: Option<String>,
    pub updated_at: DateTime<Utc>,
    pub last_event: String,
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
        }
    }
}

/// Minimum status JSON fields (CLI + GET /v1/status).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusView {
    pub project_id: String,
    pub path: PathBuf,
    pub status: RunStatus,
    pub phase: String,
    pub track_id: Option<String>,
    pub last_event: String,
}

impl StatusView {
    pub fn from_record(record: &ProjectRecord, state: &RunState) -> Self {
        Self {
            project_id: record.id.clone(),
            path: record.path.clone(),
            status: state.status,
            phase: state.phase.clone(),
            track_id: state.track_id.clone(),
            last_event: state.last_event.clone(),
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
pub fn resolve_state_dir(record: &ProjectRecord) -> PathBuf {
    if let Some(over) = state_dir_override() {
        return over.join(&record.id);
    }
    if let Some(ref sd) = record.state_dir {
        return sd.clone();
    }
    record.path.join(".coordinator")
}

pub fn run_state_path(record: &ProjectRecord) -> PathBuf {
    resolve_state_dir(record).join("run-state.json")
}

pub fn load_run_state(record: &ProjectRecord) -> Result<RunState> {
    let path = run_state_path(record);
    if !path.exists() {
        return Ok(RunState::idle(&record.id));
    }
    let text = std::fs::read_to_string(&path)?;
    let state: RunState = serde_json::from_str(&text)?;
    Ok(state)
}

pub fn save_run_state(record: &ProjectRecord, state: &RunState) -> Result<()> {
    let path = run_state_path(record);
    atomic_write_json(&path, state)
}

/// Ensure state dir exists (creates `.coordinator` as needed).
pub fn ensure_state_dir(record: &ProjectRecord) -> Result<PathBuf> {
    let dir = resolve_state_dir(record);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
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
            layout_profile: "nested".into(),
            state_dir: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn default_state_dir_is_dot_coordinator() {
        let dir = tempdir().unwrap();
        let rec = sample_record(dir.path());
        assert_eq!(resolve_state_dir(&rec), dir.path().join(".coordinator"));
    }

    #[test]
    fn save_load_run_state() {
        let dir = tempdir().unwrap();
        let rec = sample_record(dir.path());
        let mut state = RunState::idle(&rec.id);
        state.status = RunStatus::Running;
        state.phase = STUB_PHASE_ACTIVE.into();
        state.last_event = "run: started stub".into();
        save_run_state(&rec, &state).unwrap();
        assert!(run_state_path(&rec).exists());
        let loaded = load_run_state(&rec).unwrap();
        assert_eq!(loaded.status, RunStatus::Running);
        assert_eq!(loaded.phase, STUB_PHASE_ACTIVE);
    }

    #[test]
    fn state_dir_env_override() {
        use crate::config::ENV_COORDINATOR_STATE_DIR;
        use std::sync::{Mutex, OnceLock};

        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _guard = LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();

        let proj = tempdir().unwrap();
        let over = tempdir().unwrap();
        let rec = sample_record(proj.path());

        // SAFETY: serialized by LOCK; restored before drop.
        unsafe {
            std::env::set_var(ENV_COORDINATOR_STATE_DIR, over.path());
        }
        let resolved = resolve_state_dir(&rec);
        assert_eq!(resolved, over.path().join(&rec.id));
        assert_eq!(
            run_state_path(&rec),
            over.path().join(&rec.id).join("run-state.json")
        );
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_STATE_DIR);
        }
    }

    #[test]
    fn state_dir_env_keys_by_project_id() {
        use crate::config::ENV_COORDINATOR_STATE_DIR;
        use std::sync::{Mutex, OnceLock};

        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _guard = LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();

        let over = tempdir().unwrap();
        let p1 = tempdir().unwrap();
        let p2 = tempdir().unwrap();
        let r1 = sample_record(p1.path());
        let r2 = sample_record(p2.path());

        unsafe {
            std::env::set_var(ENV_COORDINATOR_STATE_DIR, over.path());
        }
        assert_ne!(resolve_state_dir(&r1), resolve_state_dir(&r2));
        assert!(resolve_state_dir(&r1).ends_with(std::path::Path::new(&r1.id)));
        assert!(resolve_state_dir(&r2).ends_with(std::path::Path::new(&r2.id)));
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
        assert_eq!(resolve_state_dir(&rec), over.path());
    }
}
