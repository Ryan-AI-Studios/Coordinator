//! Machine-local Project Registry (`{COORDINATOR_HOME}/registry.json`).

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{CoordinatorError, Result};
use crate::persist::atomic_write_json;

pub const REGISTRY_VERSION: u32 = 1;

/// Stable project identifier.
pub type ProjectId = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectRecord {
    pub id: ProjectId,
    /// Absolute, dunce-normalized path (workspace root).
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Stub field for 0006 Layout Profiles; default nested.
    #[serde(default = "default_layout_profile")]
    pub layout_profile: String,
    /// Optional per-record state dir override (absolute).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_dir: Option<PathBuf>,
    pub created_at: DateTime<Utc>,
}

fn default_layout_profile() -> String {
    "nested".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Registry {
    pub version: u32,
    pub projects: Vec<ProjectRecord>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            version: REGISTRY_VERSION,
            projects: Vec::new(),
        }
    }
}

impl Registry {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        let reg: Registry = serde_json::from_str(&text)?;
        Ok(reg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        atomic_write_json(path, self)
    }

    /// Add a project path: canonicalize, dedupe, assign id.
    pub fn add(&mut self, path: &Path) -> Result<ProjectRecord> {
        if !path.exists() {
            return Err(CoordinatorError::Message(format!(
                "path does not exist: {}",
                path.display()
            )));
        }
        if !path.is_dir() {
            return Err(CoordinatorError::Message(format!(
                "path is not a directory: {}",
                path.display()
            )));
        }

        let canonical = canonicalize_path(path)?;
        if let Some(existing) = self
            .projects
            .iter()
            .find(|p| paths_equal(&p.path, &canonical))
        {
            return Ok(existing.clone());
        }

        let display_name = canonical
            .file_name()
            .map(|s| s.to_string_lossy().into_owned());

        let record = ProjectRecord {
            id: Uuid::new_v4().to_string(),
            path: canonical,
            display_name,
            layout_profile: default_layout_profile(),
            state_dir: None,
            created_at: Utc::now(),
        };
        self.projects.push(record.clone());
        Ok(record)
    }

    pub fn list(&self) -> &[ProjectRecord] {
        &self.projects
    }

    pub fn find_by_id(&self, id: &str) -> Option<&ProjectRecord> {
        self.projects.iter().find(|p| p.id == id)
    }

    pub fn find_by_path(&self, path: &Path) -> Option<&ProjectRecord> {
        let canonical = canonicalize_path(path).ok();
        self.projects.iter().find(|p| {
            if let Some(ref c) = canonical {
                paths_equal(&p.path, c)
            } else {
                paths_equal(&p.path, path)
            }
        })
    }

    /// Resolve `--project` as id or path; single-project default when omitted.
    pub fn resolve_project(&self, project: Option<&str>) -> Result<&ProjectRecord> {
        match project {
            Some(spec) => {
                if let Some(p) = self.find_by_id(spec) {
                    return Ok(p);
                }
                let path = PathBuf::from(spec);
                if let Some(p) = self.find_by_path(&path) {
                    return Ok(p);
                }
                // Try canonicalize even if not yet matching store form
                if path.exists()
                    && let Ok(c) = canonicalize_path(&path)
                    && let Some(p) = self.projects.iter().find(|p| paths_equal(&p.path, &c))
                {
                    return Ok(p);
                }
                Err(CoordinatorError::ProjectNotFound(spec.to_string()))
            }
            None => {
                if self.projects.len() == 1 {
                    Ok(&self.projects[0])
                } else if self.projects.is_empty() {
                    Err(CoordinatorError::Message(
                        "no projects registered; run `coordinator project add <path>`".into(),
                    ))
                } else {
                    Err(CoordinatorError::Message(
                        "multiple projects registered; pass --project <path|id>".into(),
                    ))
                }
            }
        }
    }
}

/// Canonical absolute path without Windows `\\?\` noise.
pub fn canonicalize_path(path: &Path) -> Result<PathBuf> {
    let canon = dunce::canonicalize(path).map_err(|e| {
        CoordinatorError::Message(format!("cannot canonicalize {}: {e}", path.display()))
    })?;
    Ok(canon)
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    // Case-insensitive compare on Windows for registry dedupe.
    #[cfg(windows)]
    {
        a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        a == b
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn add_list_round_trip() {
        let home = tempdir().unwrap();
        let reg_path = home.path().join("registry.json");
        let proj = tempdir().unwrap();

        let mut reg = Registry::default();
        let rec = reg.add(proj.path()).unwrap();
        assert!(!rec.id.is_empty());
        assert!(rec.path.is_absolute());
        reg.save(&reg_path).unwrap();

        let loaded = Registry::load(&reg_path).unwrap();
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].id, rec.id);
        assert_eq!(loaded.list().len(), 1);
    }

    #[test]
    fn dedupe_same_path() {
        let proj = tempdir().unwrap();
        let mut reg = Registry::default();
        let a = reg.add(proj.path()).unwrap();
        let b = reg.add(proj.path()).unwrap();
        assert_eq!(a.id, b.id);
        assert_eq!(reg.projects.len(), 1);
    }

    #[test]
    fn reject_missing_path() {
        let mut reg = Registry::default();
        let err = reg
            .add(Path::new("C:\\does\\not\\exist\\coordinator-xyz"))
            .unwrap_err();
        assert!(matches!(err, CoordinatorError::Message(_)));
    }

    #[test]
    fn resolve_single_default() {
        let proj = tempdir().unwrap();
        let mut reg = Registry::default();
        let rec = reg.add(proj.path()).unwrap();
        let resolved = reg.resolve_project(None).unwrap();
        assert_eq!(resolved.id, rec.id);
    }

    #[test]
    fn resolve_requires_project_when_multiple() {
        let p1 = tempdir().unwrap();
        let p2 = tempdir().unwrap();
        let mut reg = Registry::default();
        reg.add(p1.path()).unwrap();
        reg.add(p2.path()).unwrap();
        assert!(reg.resolve_project(None).is_err());
    }
}
