//! Layout Profiles and workspace path resolution (track **0006**).
//!
//! Resolve is pure + profile-specific: stored fields that do not apply to the
//! current profile are ignored for path meaning (not auto-cleared on flip).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CoordinatorError, Result};
use crate::registry::ProjectRecord;

/// Supported layout shapes (ADR-0013). JSON: `nested` | `multi_sibling` | `single_root`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LayoutProfile {
    #[default]
    Nested,
    MultiSibling,
    SingleRoot,
}

impl LayoutProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Nested => "nested",
            Self::MultiSibling => "multi_sibling",
            Self::SingleRoot => "single_root",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s.trim() {
            "nested" => Ok(Self::Nested),
            "multi_sibling" => Ok(Self::MultiSibling),
            "single_root" => Ok(Self::SingleRoot),
            other => Err(CoordinatorError::Message(format!(
                "unknown layout_profile '{other}'; expected nested | multi_sibling | single_root"
            ))),
        }
    }
}

impl std::fmt::Display for LayoutProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Resolved paths for a registered project (show / status / later harness spawn).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspacePaths {
    pub workspace_root: PathBuf,
    pub conductor_dir: PathBuf,
    /// Primary execution cwd (null when nested and unknown).
    pub execution_repo: Option<PathBuf>,
    /// Named map (multi_sibling); empty for other profiles after resolve.
    pub execution_repos: BTreeMap<String, PathBuf>,
    pub state_dir: PathBuf,
}

/// Child basenames never treated as nested product candidates (case-insensitive).
const NESTED_SKIP: &[&str] = &["conductor", ".git", ".agents", "docs", "mock"];

fn basename_skipped(name: &str) -> bool {
    NESTED_SKIP.iter().any(|s| name.eq_ignore_ascii_case(s))
}

fn child_is_product_candidate(child: &Path) -> bool {
    if !child.is_dir() {
        return false;
    }
    let Some(name) = child.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if basename_skipped(name) {
        return false;
    }
    child.join("Cargo.toml").is_file() || child.join(".git").exists()
}

/// Auto-detect nested execution repo: **immediate children only** (never workspace root).
///
/// Exactly one eligible child → `Some(path)`; zero or many → `None`.
pub fn auto_detect_nested_execution(workspace: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(workspace).ok()?;
    let mut found: Vec<PathBuf> = Vec::new();
    for ent in entries.flatten() {
        let path = ent.path();
        if child_is_product_candidate(&path) {
            found.push(path);
        }
    }
    if found.len() == 1 { found.pop() } else { None }
}

/// Count eligible nested product children (for scan profile heuristics).
pub fn count_nested_product_candidates(workspace: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(workspace) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| child_is_product_candidate(&e.path()))
        .count()
}

/// Default state dir under workspace when record has no override and no env.
pub fn default_state_dir(workspace: &Path) -> PathBuf {
    workspace.join(".coordinator")
}

/// Default conductor dir under workspace.
pub fn default_conductor_dir(workspace: &Path) -> PathBuf {
    workspace.join("conductor")
}

/// Pure resolve: profile-specific use of stored bindings (stale fields inert, not cleared).
pub fn resolve(record: &ProjectRecord) -> WorkspacePaths {
    let workspace_root = record.path.clone();
    let conductor_dir = record
        .conductor_dir
        .clone()
        .unwrap_or_else(|| default_conductor_dir(&workspace_root));
    // State: record override only here; env override applied by `state::resolve_state_dir`.
    let state_dir = record
        .state_dir
        .clone()
        .unwrap_or_else(|| default_state_dir(&workspace_root));

    match record.layout_profile {
        LayoutProfile::Nested => WorkspacePaths {
            workspace_root,
            conductor_dir,
            execution_repo: record.execution_repo.clone(),
            execution_repos: BTreeMap::new(),
            state_dir,
        },
        LayoutProfile::MultiSibling => {
            let execution_repos = record.execution_repos.clone();
            let primary = record
                .execution_repo
                .clone()
                .or_else(|| execution_repos.iter().next().map(|(_, p)| p.clone()));
            WorkspacePaths {
                workspace_root,
                conductor_dir,
                execution_repo: primary,
                execution_repos,
                state_dir,
            }
        }
        LayoutProfile::SingleRoot => {
            // Always execution == workspace; stored execution fields are inert.
            WorkspacePaths {
                workspace_root: workspace_root.clone(),
                conductor_dir,
                execution_repo: Some(workspace_root),
                execution_repos: BTreeMap::new(),
                state_dir,
            }
        }
    }
}

/// Remediation hint when nested has no primary execution_repo.
pub fn nested_execution_null_hint(project_spec: &str) -> String {
    format!("hint: coordinator project set --project {project_spec} --execution-repo <path>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tempfile::tempdir;

    fn base_record(path: PathBuf, profile: LayoutProfile) -> ProjectRecord {
        ProjectRecord {
            id: "test".into(),
            path,
            display_name: None,
            layout_profile: profile,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: BTreeMap::new(),
            state_dir: None,
            auto_merge: true,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn parse_profiles() {
        assert_eq!(
            LayoutProfile::parse("nested").unwrap(),
            LayoutProfile::Nested
        );
        assert_eq!(
            LayoutProfile::parse("multi_sibling").unwrap(),
            LayoutProfile::MultiSibling
        );
        assert_eq!(
            LayoutProfile::parse("single_root").unwrap(),
            LayoutProfile::SingleRoot
        );
        assert!(LayoutProfile::parse("flat").is_err());
    }

    #[test]
    fn nested_resolve_with_execution() {
        let ws = PathBuf::from(r"C:\dev\Orca");
        let mut rec = base_record(ws.clone(), LayoutProfile::Nested);
        rec.execution_repo = Some(ws.join("OrcaSlicer-ZR"));
        let paths = resolve(&rec);
        assert_eq!(paths.workspace_root, ws);
        assert_eq!(paths.conductor_dir, ws.join("conductor"));
        assert_eq!(
            paths.execution_repo.as_ref().unwrap(),
            &ws.join("OrcaSlicer-ZR")
        );
        assert!(paths.execution_repos.is_empty());
        assert_eq!(paths.state_dir, ws.join(".coordinator"));
    }

    #[test]
    fn multi_sibling_map_primary_sorted() {
        let hub = PathBuf::from(r"C:\dev\coordinated");
        let mut rec = base_record(hub.clone(), LayoutProfile::MultiSibling);
        rec.execution_repos
            .insert("ledgerful".into(), PathBuf::from(r"C:\dev\ledgerful"));
        rec.execution_repos
            .insert("ai-brains".into(), PathBuf::from(r"C:\dev\ai-brains"));
        let paths = resolve(&rec);
        // BTreeMap: first key alphabetically is primary when execution_repo unset
        assert_eq!(
            paths.execution_repo.as_ref().unwrap(),
            &PathBuf::from(r"C:\dev\ai-brains")
        );
        assert_eq!(paths.execution_repos.len(), 2);
        assert_eq!(paths.conductor_dir, hub.join("conductor"));
    }

    #[test]
    fn multi_sibling_explicit_primary_wins() {
        let hub = PathBuf::from(r"C:\dev\coordinated");
        let mut rec = base_record(hub, LayoutProfile::MultiSibling);
        rec.execution_repo = Some(PathBuf::from(r"C:\dev\ledgerful"));
        rec.execution_repos
            .insert("ai-brains".into(), PathBuf::from(r"C:\dev\ai-brains"));
        let paths = resolve(&rec);
        assert_eq!(
            paths.execution_repo.as_ref().unwrap(),
            &PathBuf::from(r"C:\dev\ledgerful")
        );
    }

    #[test]
    fn single_root_forces_execution_workspace() {
        let ws = PathBuf::from(r"C:\dev\solo");
        let mut rec = base_record(ws.clone(), LayoutProfile::SingleRoot);
        rec.execution_repo = Some(PathBuf::from(r"C:\dev\stale"));
        rec.execution_repos
            .insert("x".into(), PathBuf::from(r"C:\dev\other"));
        let paths = resolve(&rec);
        assert_eq!(paths.execution_repo.as_ref().unwrap(), &ws);
        assert!(paths.execution_repos.is_empty());
    }

    #[test]
    fn profile_flip_keeps_raw_stale_fields() {
        let ws = PathBuf::from(r"C:\dev\flip");
        let mut rec = base_record(ws.clone(), LayoutProfile::Nested);
        rec.execution_repo = Some(ws.join("product"));
        // Flip to single_root: raw field remains; resolve ignores it
        rec.layout_profile = LayoutProfile::SingleRoot;
        assert_eq!(rec.execution_repo.as_ref().unwrap(), &ws.join("product"));
        let paths = resolve(&rec);
        assert_eq!(paths.execution_repo.as_ref().unwrap(), &ws);
    }

    #[test]
    fn auto_detect_exactly_one_child() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        std::fs::create_dir_all(ws.join("conductor")).unwrap();
        std::fs::write(ws.join("conductor").join("conductor.md"), "# t").unwrap();
        let product = ws.join("ProductApp");
        std::fs::create_dir_all(&product).unwrap();
        std::fs::write(product.join("Cargo.toml"), "[package]\nname=\"p\"\n").unwrap();

        let detected = auto_detect_nested_execution(ws).unwrap();
        assert_eq!(detected, product);
    }

    #[test]
    fn auto_detect_skips_workspace_root_markers() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        // Root looks like single_root product — must NOT count as nested candidate
        std::fs::write(ws.join("Cargo.toml"), "[package]\nname=\"root\"\n").unwrap();
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        assert!(auto_detect_nested_execution(ws).is_none());
    }

    #[test]
    fn auto_detect_multi_candidates_null() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        for name in ["A", "B"] {
            let p = ws.join(name);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        }
        assert!(auto_detect_nested_execution(ws).is_none());
    }

    #[test]
    fn auto_detect_skips_conductor_docs_mock() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        for name in ["conductor", "docs", "mock", ".agents"] {
            let p = ws.join(name);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        }
        assert!(auto_detect_nested_execution(ws).is_none());
    }

    #[test]
    fn child_git_counts() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        let product = ws.join("App");
        std::fs::create_dir_all(product.join(".git")).unwrap();
        assert_eq!(auto_detect_nested_execution(ws).unwrap(), product);
    }
}
