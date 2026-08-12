//! Machine-local Project Registry (`{COORDINATOR_HOME}/registry.json`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{CoordinatorError, Result};
use crate::layout::{LayoutProfile, auto_detect_nested_execution};
use crate::persist::atomic_write_json;

pub const REGISTRY_VERSION: u32 = 1;

/// Stable project identifier.
pub type ProjectId = String;

/// Optional fields for `project add` / HTTP POST.
#[derive(Debug, Clone, Default)]
pub struct ProjectAddOptions {
    pub layout_profile: LayoutProfile,
    pub execution_repo: Option<PathBuf>,
    pub conductor_dir: Option<PathBuf>,
    pub state_dir: Option<PathBuf>,
    pub display_name: Option<String>,
    /// multi_sibling: name for primary map entry when `execution_repo` is set.
    pub execution_repo_name: Option<String>,
    pub execution_repos: BTreeMap<String, PathBuf>,
}

/// Fields mutatable via `project set` (workspace `path` is immutable this track).
#[derive(Debug, Clone, Default)]
pub struct ProjectSetOptions {
    pub layout_profile: Option<LayoutProfile>,
    pub execution_repo: Option<PathBuf>,
    pub clear_execution_repo: bool,
    pub conductor_dir: Option<PathBuf>,
    pub clear_conductor_dir: bool,
    pub state_dir: Option<PathBuf>,
    pub clear_state_dir: bool,
    pub display_name: Option<String>,
    pub execution_repos: Option<BTreeMap<String, PathBuf>>,
    pub execution_repo_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectRecord {
    pub id: ProjectId,
    /// Absolute, dunce-normalized path (workspace root). Immutable via `project set`.
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default)]
    pub layout_profile: LayoutProfile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conductor_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_repo: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub execution_repos: BTreeMap<String, PathBuf>,
    /// Optional per-record state dir override (absolute).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_dir: Option<PathBuf>,
    pub created_at: DateTime<Utc>,
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
        if reg.version != REGISTRY_VERSION {
            return Err(CoordinatorError::Message(format!(
                "unsupported registry schema version {}; expected {REGISTRY_VERSION} \
                 (re-register projects or migrate the file)",
                reg.version
            )));
        }
        Ok(reg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        atomic_write_json(path, self)
    }

    /// Add a project path: canonicalize, dedupe, assign id, apply layout options.
    pub fn add(&mut self, path: &Path, opts: ProjectAddOptions) -> Result<ProjectRecord> {
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

        let display_name = opts.display_name.or_else(|| {
            canonical
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
        });

        let mut execution_repo = opts
            .execution_repo
            .map(|p| prefer_absolute_path(&p))
            .transpose()?;
        let mut execution_repos = BTreeMap::new();
        for (k, v) in opts.execution_repos {
            execution_repos.insert(k, prefer_absolute_path(&v)?);
        }

        // Nested auto-detect when primary not provided.
        if execution_repo.is_none() && opts.layout_profile == LayoutProfile::Nested {
            execution_repo = auto_detect_nested_execution(&canonical);
        }

        // multi_sibling: optional name for primary map entry
        if let (Some(name), Some(exec)) = (&opts.execution_repo_name, &execution_repo) {
            execution_repos
                .entry(name.clone())
                .or_insert_with(|| exec.clone());
        }

        let conductor_dir = opts
            .conductor_dir
            .map(|p| prefer_absolute_path(&p))
            .transpose()?;
        let state_dir = opts
            .state_dir
            .map(|p| prefer_absolute_path(&p))
            .transpose()?;

        let record = ProjectRecord {
            id: Uuid::new_v4().to_string(),
            path: canonical,
            display_name,
            layout_profile: opts.layout_profile,
            conductor_dir,
            execution_repo,
            execution_repos,
            state_dir,
            created_at: Utc::now(),
        };
        self.projects.push(record.clone());
        Ok(record)
    }

    /// Mutate path bindings / profile for an existing project (path immutable).
    pub fn set(&mut self, id: &str, opts: ProjectSetOptions) -> Result<ProjectRecord> {
        let idx = self
            .projects
            .iter()
            .position(|p| p.id == id)
            .ok_or_else(|| CoordinatorError::ProjectNotFound(id.to_string()))?;

        let rec = &mut self.projects[idx];
        if let Some(p) = opts.layout_profile {
            rec.layout_profile = p;
        }
        if opts.clear_execution_repo {
            rec.execution_repo = None;
        } else if let Some(p) = opts.execution_repo {
            rec.execution_repo = Some(prefer_absolute_path(&p)?);
        }
        if opts.clear_conductor_dir {
            rec.conductor_dir = None;
        } else if let Some(p) = opts.conductor_dir {
            rec.conductor_dir = Some(prefer_absolute_path(&p)?);
        }
        if opts.clear_state_dir {
            rec.state_dir = None;
        } else if let Some(p) = opts.state_dir {
            rec.state_dir = Some(prefer_absolute_path(&p)?);
        }
        if let Some(n) = opts.display_name {
            rec.display_name = Some(n);
        }
        if let Some(map) = opts.execution_repos {
            let mut abs = BTreeMap::new();
            for (k, v) in map {
                abs.insert(k, prefer_absolute_path(&v)?);
            }
            rec.execution_repos = abs;
        }
        if let (Some(name), Some(exec)) = (&opts.execution_repo_name, &rec.execution_repo) {
            rec.execution_repos
                .entry(name.clone())
                .or_insert_with(|| exec.clone());
        }
        Ok(rec.clone())
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

/// Prefer absolute path for stored bindings: canonicalize when the path exists,
/// else require absolute (reject bare relative so cwd cannot drift later).
pub fn prefer_absolute_path(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(CoordinatorError::Message(
            "path binding must not be empty".into(),
        ));
    }
    if path.exists() {
        return canonicalize_path(path);
    }
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Err(CoordinatorError::Message(format!(
        "path binding must be absolute (or an existing path): {}",
        path.display()
    )))
}

/// Case-insensitive path equality on Windows for registry dedupe / scan.
pub fn paths_equal(a: &Path, b: &Path) -> bool {
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
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        assert!(!rec.id.is_empty());
        assert!(rec.path.is_absolute());
        assert_eq!(rec.layout_profile, LayoutProfile::Nested);
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
        let a = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        let b = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        assert_eq!(a.id, b.id);
        assert_eq!(reg.projects.len(), 1);
    }

    #[test]
    fn reject_missing_path() {
        let mut reg = Registry::default();
        let err = reg
            .add(
                Path::new("C:\\does\\not\\exist\\coordinator-xyz"),
                ProjectAddOptions::default(),
            )
            .unwrap_err();
        assert!(matches!(err, CoordinatorError::Message(_)));
    }

    #[test]
    fn resolve_single_default() {
        let proj = tempdir().unwrap();
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        let resolved = reg.resolve_project(None).unwrap();
        assert_eq!(resolved.id, rec.id);
    }

    #[test]
    fn resolve_requires_project_when_multiple() {
        let p1 = tempdir().unwrap();
        let p2 = tempdir().unwrap();
        let mut reg = Registry::default();
        reg.add(p1.path(), ProjectAddOptions::default()).unwrap();
        reg.add(p2.path(), ProjectAddOptions::default()).unwrap();
        assert!(reg.resolve_project(None).is_err());
    }

    #[test]
    fn reject_unsupported_version() {
        let home = tempdir().unwrap();
        let reg_path = home.path().join("registry.json");
        std::fs::write(&reg_path, r#"{"version":99,"projects":[]}"#).unwrap();
        let err = Registry::load(&reg_path).unwrap_err();
        assert!(
            err.to_string()
                .contains("unsupported registry schema version")
        );
    }

    #[test]
    fn load_minimal_old_style_registry() {
        let home = tempdir().unwrap();
        let reg_path = home.path().join("registry.json");
        let proj = tempdir().unwrap();
        let path_json = serde_json::to_string(&proj.path()).unwrap();
        let json = format!(
            r#"{{"version":1,"projects":[{{"id":"abc","path":{path_json},"layout_profile":"nested","created_at":"2026-01-01T00:00:00Z"}}]}}"#
        );
        std::fs::write(&reg_path, json).unwrap();
        let loaded = Registry::load(&reg_path).unwrap();
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].layout_profile, LayoutProfile::Nested);
        assert!(loaded.projects[0].execution_repos.is_empty());
        assert!(loaded.projects[0].execution_repo.is_none());
    }

    #[test]
    fn reject_unknown_profile_string() {
        let home = tempdir().unwrap();
        let reg_path = home.path().join("registry.json");
        let proj = tempdir().unwrap();
        let path_json = serde_json::to_string(&proj.path()).unwrap();
        let json = format!(
            r#"{{"version":1,"projects":[{{"id":"abc","path":{path_json},"layout_profile":"flat","created_at":"2026-01-01T00:00:00Z"}}]}}"#
        );
        std::fs::write(&reg_path, json).unwrap();
        assert!(Registry::load(&reg_path).is_err());
    }

    #[test]
    fn set_profile_and_execution_map() {
        let proj = tempdir().unwrap();
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        let mut map = BTreeMap::new();
        map.insert("ledgerful".into(), PathBuf::from(r"C:\dev\ledgerful"));
        let updated = reg
            .set(
                &rec.id,
                ProjectSetOptions {
                    layout_profile: Some(LayoutProfile::MultiSibling),
                    execution_repos: Some(map.clone()),
                    execution_repo: Some(PathBuf::from(r"C:\dev\ledgerful")),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(updated.layout_profile, LayoutProfile::MultiSibling);
        assert_eq!(updated.execution_repos, map);
        // Round-trip save/load
        let home = tempdir().unwrap();
        let reg_path = home.path().join("registry.json");
        reg.save(&reg_path).unwrap();
        let loaded = Registry::load(&reg_path).unwrap();
        assert_eq!(loaded.projects[0].execution_repos, map);
    }

    #[test]
    fn nested_add_auto_detects_single_child() {
        let ws = tempdir().unwrap();
        let product = ws.path().join("ProductApp");
        std::fs::create_dir_all(&product).unwrap();
        std::fs::write(product.join("Cargo.toml"), "[package]\nname=\"p\"\n").unwrap();
        let mut reg = Registry::default();
        let rec = reg.add(ws.path(), ProjectAddOptions::default()).unwrap();
        assert!(rec.execution_repo.is_some());
        let exec = rec.execution_repo.unwrap();
        assert!(
            paths_equal(&exec, &canonicalize_path(&product).unwrap())
                || exec.ends_with("ProductApp")
        );
    }

    #[test]
    fn set_does_not_mutate_workspace_path() {
        let proj = tempdir().unwrap();
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        let original = rec.path.clone();
        let updated = reg
            .set(
                &rec.id,
                ProjectSetOptions {
                    layout_profile: Some(LayoutProfile::SingleRoot),
                    execution_repo: Some(PathBuf::from(r"C:\dev\stale")),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(updated.path, original);
        assert_eq!(updated.layout_profile, LayoutProfile::SingleRoot);
    }

    #[test]
    fn reject_relative_execution_repo_binding() {
        let proj = tempdir().unwrap();
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        let err = reg
            .set(
                &rec.id,
                ProjectSetOptions {
                    execution_repo: Some(PathBuf::from("relative\\not\\absolute")),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("absolute"));
    }
}
