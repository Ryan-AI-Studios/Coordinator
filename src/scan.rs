//! Optional project scan of configured roots (ADR-0026, track **0006**).
//!
//! Depth: each root itself + **immediate children** only. No recursive walk;
//! directory symlinks/junctions are treated as single entries (marker check only).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::layout::{LayoutProfile, auto_detect_nested_execution, count_nested_product_candidates};
use crate::registry::{Registry, canonicalize_path, paths_equal};

/// One scan hit (registered or not).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScanCandidate {
    pub path: PathBuf,
    pub detected_profile: LayoutProfile,
    pub already_registered: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_repo_hint: Option<PathBuf>,
}

/// True when `dir` contains `conductor/conductor.md`.
pub fn has_conductor_marker(dir: &Path) -> bool {
    dir.join("conductor").join("conductor.md").is_file()
}

/// Heuristic profile when discovering a workspace with a conductor marker.
///
/// - Workspace has `Cargo.toml` at root → `single_root` (product is the workspace).
/// - Else exactly one nested product child → `nested` + execution hint.
/// - Else → `nested` with null execution hint.
pub fn detect_profile_for_workspace(workspace: &Path) -> (LayoutProfile, Option<PathBuf>) {
    if workspace.join("Cargo.toml").is_file() {
        return (LayoutProfile::SingleRoot, Some(workspace.to_path_buf()));
    }
    let n = count_nested_product_candidates(workspace);
    if n == 1 {
        (
            LayoutProfile::Nested,
            auto_detect_nested_execution(workspace),
        )
    } else {
        (LayoutProfile::Nested, None)
    }
}

fn consider_dir(dir: &Path, registry: &Registry, out: &mut Vec<ScanCandidate>) {
    if !dir.is_dir() || !has_conductor_marker(dir) {
        return;
    }
    let path = canonicalize_path(dir).unwrap_or_else(|_| dir.to_path_buf());
    // Dedupe by path
    if out.iter().any(|c| paths_equal(&c.path, &path)) {
        return;
    }
    let already = registry
        .projects
        .iter()
        .any(|p| paths_equal(&p.path, &path));
    let (detected_profile, execution_repo_hint) = detect_profile_for_workspace(&path);
    out.push(ScanCandidate {
        path,
        detected_profile,
        already_registered: already,
        execution_repo_hint,
    });
}

/// Scan roots: each root + its immediate children for conductor markers.
pub fn scan_roots(roots: &[PathBuf], registry: &Registry) -> Result<Vec<ScanCandidate>> {
    let mut out = Vec::new();
    for root in roots {
        if !root.exists() {
            continue;
        }
        consider_dir(root, registry, &mut out);
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for ent in entries.flatten() {
            let path = ent.path();
            // Do not recurse; treat junctions/symlinks as this one directory only.
            if path.is_dir() {
                consider_dir(&path, registry, &mut out);
            }
        }
    }
    out.sort_by(|a, b| {
        a.path
            .to_string_lossy()
            .to_ascii_lowercase()
            .cmp(&b.path.to_string_lossy().to_ascii_lowercase())
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::LayoutProfile;
    use crate::registry::ProjectAddOptions;
    use tempfile::tempdir;

    fn fixture_project(parent: &Path, name: &str) -> PathBuf {
        let p = parent.join(name);
        std::fs::create_dir_all(p.join("conductor")).unwrap();
        std::fs::write(p.join("conductor").join("conductor.md"), "# tracks\n").unwrap();
        p
    }

    #[test]
    fn scan_finds_child_with_marker() {
        let root = tempdir().unwrap();
        let proj = fixture_project(root.path(), "ProjA");
        let product = proj.join("ProductApp");
        std::fs::create_dir_all(&product).unwrap();
        std::fs::write(product.join("Cargo.toml"), "[package]\nname=\"p\"\n").unwrap();

        let reg = Registry::default();
        let hits = scan_roots(&[root.path().to_path_buf()], &reg).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(paths_equal(
            &hits[0].path,
            &canonicalize_path(&proj).unwrap()
        ));
        assert!(!hits[0].already_registered);
        assert_eq!(hits[0].detected_profile, LayoutProfile::Nested);
        assert!(hits[0].execution_repo_hint.is_some());
    }

    #[test]
    fn scan_includes_root_itself() {
        let root = tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("conductor")).unwrap();
        std::fs::write(root.path().join("conductor").join("conductor.md"), "# t\n").unwrap();
        let reg = Registry::default();
        let hits = scan_roots(&[root.path().to_path_buf()], &reg).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn scan_no_deep_recursion() {
        let root = tempdir().unwrap();
        // Nested two levels: root/mid/deep — deep must NOT be found via recursion
        let deep = root.path().join("mid").join("deep");
        std::fs::create_dir_all(deep.join("conductor")).unwrap();
        std::fs::write(deep.join("conductor").join("conductor.md"), "# t\n").unwrap();
        let reg = Registry::default();
        let hits = scan_roots(&[root.path().to_path_buf()], &reg).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn scan_marks_already_registered() {
        let root = tempdir().unwrap();
        let proj = fixture_project(root.path(), "RegMe");
        let mut reg = Registry::default();
        reg.add(&proj, ProjectAddOptions::default()).unwrap();
        let hits = scan_roots(&[root.path().to_path_buf()], &reg).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].already_registered);
    }

    #[test]
    fn single_root_heuristic_when_cargo_at_workspace() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("conductor")).unwrap();
        std::fs::write(dir.path().join("conductor").join("conductor.md"), "# t\n").unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"solo\"\n").unwrap();
        let (profile, exec) = detect_profile_for_workspace(dir.path());
        assert_eq!(profile, LayoutProfile::SingleRoot);
        assert_eq!(exec.as_ref().unwrap(), dir.path());
    }
}
