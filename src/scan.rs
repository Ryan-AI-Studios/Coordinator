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

/// Stable absolute path for reporting **without** following reparse points when possible.
///
/// Prefer canonicalize only for normal directories. When the entry is a
/// symlink/junction, keep the absolute path under the scan root so profile
/// detection does not re-root into the junction target tree.
fn scan_report_path(dir: &Path) -> PathBuf {
    if is_reparse_point(dir) {
        return absolute_no_follow(dir);
    }
    canonicalize_path(dir).unwrap_or_else(|_| absolute_no_follow(dir))
}

fn absolute_no_follow(dir: &Path) -> PathBuf {
    if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(dir))
            .unwrap_or_else(|_| dir.to_path_buf())
    }
}

/// Best-effort: directory is a symlink (and on Windows, often a junction).
fn is_reparse_point(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn consider_dir(dir: &Path, registry: &Registry, out: &mut Vec<ScanCandidate>) {
    // Marker check uses the entry as listed. We do **not** recurse into descendants.
    if !dir.is_dir() || !has_conductor_marker(dir) {
        return;
    }
    let path = scan_report_path(dir);
    if out.iter().any(|c| paths_equal(&c.path, &path)) {
        return;
    }
    let already = registry.projects.iter().any(|p| {
        paths_equal(&p.path, &path)
            || canonicalize_path(dir)
                .map(|c| paths_equal(&p.path, &c))
                .unwrap_or(false)
    });
    // Reparse points: marker-only entry; do not walk target children for product hints.
    let (detected_profile, execution_repo_hint) = if is_reparse_point(dir) {
        (LayoutProfile::Nested, None)
    } else {
        detect_profile_for_workspace(dir)
    };
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
            // Immediate children only. Reparse points are single entries (marker only).
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

    #[test]
    fn reparse_point_is_marker_only_no_target_execution_hint() {
        // Symlink directory: if the OS allows creating one, ensure we do not
        // surface the target's nested product as execution_repo_hint.
        let root = tempdir().unwrap();
        let real = fixture_project(root.path(), "RealProj");
        let product = real.join("ProductApp");
        std::fs::create_dir_all(&product).unwrap();
        std::fs::write(product.join("Cargo.toml"), "[package]\nname=\"p\"\n").unwrap();

        let link = root.path().join("LinkProj");
        #[cfg(windows)]
        let created = std::os::windows::fs::symlink_dir(&real, &link).is_ok();
        #[cfg(not(windows))]
        let created = std::os::unix::fs::symlink(&real, &link).is_ok();

        if !created {
            // CI/agent without symlink privilege — skip without failing the suite.
            return;
        }
        assert!(is_reparse_point(&link));
        let reg = Registry::default();
        let hits = scan_roots(&[root.path().to_path_buf()], &reg).unwrap();
        let link_hit = hits.iter().find(|h| {
            h.path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.eq_ignore_ascii_case("LinkProj"))
        });
        if let Some(hit) = link_hit {
            assert!(
                hit.execution_repo_hint.is_none(),
                "reparse entry must not inherit target product auto-detect"
            );
            assert_eq!(hit.detected_profile, LayoutProfile::Nested);
        }
    }
}
