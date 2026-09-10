//! Machine-local Project Registry (`{COORDINATOR_HOME}/registry.json`).

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

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
    /// Omit = default true (ADR-0019).
    pub auto_merge: Option<bool>,
    /// Initial per-project phase wall clocks (seconds). Empty = table/machine.
    pub phase_timeouts_secs: BTreeMap<String, u64>,
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
    /// Omit = leave unchanged.
    pub auto_merge: Option<bool>,
    /// Omit = leave unchanged. Opt-in Hermes progress POSTs (track 0033).
    pub notify_progress: Option<bool>,
    /// Overlay keys (None = no overlay). Merge; does not replace the map.
    pub phase_timeouts_secs: Option<BTreeMap<String, u64>>,
    /// Wipe the project timeout map before overlay.
    pub clear_phase_timeouts: bool,
    /// Drop these stored keys (repeatable) before overlay.
    pub clear_phase_timeout: Vec<String>,
    /// Overlay ready-status aliases (None = no overlay). Additive; skip after `status_clean` dup.
    pub ready_aliases: Option<Vec<String>>,
    /// Wipe stored ready aliases (back to default phrase) before overlay.
    pub clear_ready_aliases: bool,
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
    /// Squash-merge when CI is green (ADR-0019). Missing field on old records = on.
    #[serde(default = "default_true")]
    pub auto_merge: bool,
    /// Per-project phase wall clocks (seconds). Empty omits the key on save.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub phase_timeouts_secs: BTreeMap<String, u64>,
    /// Opt-in Hermes progress POSTs (track 0033). Missing field on old records = off.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub notify_progress: bool,
    /// Extra omit-`--track` Ready phrases (0042). Empty = default `Ready — not started`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ready_aliases: Vec<String>,
    pub created_at: DateTime<Utc>,
}

fn default_true() -> bool {
    true
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

        crate::workflow::timeouts::validate_phase_timeout_map(&opts.phase_timeouts_secs)?;

        let record = ProjectRecord {
            id: Uuid::new_v4().to_string(),
            path: canonical,
            display_name,
            layout_profile: opts.layout_profile,
            conductor_dir,
            execution_repo,
            execution_repos,
            state_dir,
            auto_merge: opts.auto_merge.unwrap_or(true),
            phase_timeouts_secs: opts.phase_timeouts_secs,
            notify_progress: false,
            ready_aliases: Vec::new(),
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

        if let Some(ref map) = opts.phase_timeouts_secs {
            crate::workflow::timeouts::validate_phase_timeout_map(map)?;
        }
        for phase in &opts.clear_phase_timeout {
            crate::workflow::timeouts::validate_phase_timeout_key(phase)?;
        }

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
        if let Some(v) = opts.auto_merge {
            rec.auto_merge = v;
        }
        if let Some(v) = opts.notify_progress {
            rec.notify_progress = v;
        }
        // Clears first, then overlay so clear-all + plan=3600 leaves only plan.
        if opts.clear_phase_timeouts {
            rec.phase_timeouts_secs.clear();
        }
        for phase in &opts.clear_phase_timeout {
            rec.phase_timeouts_secs.remove(phase);
        }
        if let Some(map) = opts.phase_timeouts_secs {
            rec.phase_timeouts_secs.extend(map);
        }
        if opts.clear_ready_aliases {
            rec.ready_aliases.clear();
        }
        if let Some(incoming) = opts.ready_aliases {
            crate::workflow::conductor_md::extend_ready_aliases(&mut rec.ready_aliases, &incoming);
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
    ///
    /// Does **not** read process cwd or last-used (track **0029** test wrapper).
    pub fn resolve_project(&self, project: Option<&str>) -> Result<&ProjectRecord> {
        self.resolve_project_in(project, None, None)
    }

    /// Resolve a selector with optional injected cwd and last-used id.
    ///
    /// Order when `project` is omitted and `projects.len() > 1`: unique cwd
    /// containment, else last-used if still registered, else error. Ambiguous
    /// cwd does **not** fall through to last-used. `cwd == None` skips
    /// containment (HTTP omit / this wrapper).
    pub fn resolve_project_in(
        &self,
        project: Option<&str>,
        cwd: Option<&Path>,
        last_used: Option<&str>,
    ) -> Result<&ProjectRecord> {
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
                    match unique_cwd_match(self, cwd) {
                        CwdMatch::Unique(rec) => Ok(rec),
                        CwdMatch::Ambiguous(ids) => Err(CoordinatorError::Message(format!(
                            "cwd matches multiple projects ({}); pass --project <path|id>",
                            ids.join(", ")
                        ))),
                        CwdMatch::None => {
                            if let Some(id) = last_used.filter(|s| !s.is_empty())
                                && let Some(p) = self.find_by_id(id)
                            {
                                return Ok(p);
                            }
                            Err(CoordinatorError::Message(
                                "multiple projects registered; cwd is not inside a registered \
                                 workspace or execution repo; pass --project <path|id>"
                                    .into(),
                            ))
                        }
                    }
                }
            }
        }
    }
}

enum CwdMatch<'a> {
    None,
    Unique(&'a ProjectRecord),
    Ambiguous(Vec<String>),
}

fn match_roots(rec: &ProjectRecord) -> impl Iterator<Item = &Path> {
    std::iter::once(rec.path.as_path())
        .chain(rec.execution_repo.as_deref())
        .chain(rec.execution_repos.values().map(|p| p.as_path()))
        .filter(|p| !p.as_os_str().is_empty())
}

fn unique_cwd_match<'a>(reg: &'a Registry, cwd: Option<&Path>) -> CwdMatch<'a> {
    let Some(cwd) = cwd else {
        return CwdMatch::None;
    };
    let mut scored: Vec<(&ProjectRecord, usize)> = Vec::new();
    for rec in &reg.projects {
        let mut best: Option<usize> = None;
        for root in match_roots(rec) {
            if path_contains_cwd(root, cwd) {
                let len = normalize_for_containment(root).components().count();
                best = Some(best.map_or(len, |b| b.max(len)));
            }
        }
        if let Some(len) = best {
            scored.push((rec, len));
        }
    }
    if scored.is_empty() {
        return CwdMatch::None;
    }
    let max_len = scored.iter().map(|(_, l)| *l).max().unwrap_or(0);
    let mut winners: Vec<&ProjectRecord> = scored
        .into_iter()
        .filter(|(_, l)| *l == max_len)
        .map(|(r, _)| r)
        .collect();
    if winners.len() == 1 {
        CwdMatch::Unique(winners.remove(0))
    } else {
        let mut ids: Vec<String> = winners.into_iter().map(|p| p.id.clone()).collect();
        ids.sort();
        CwdMatch::Ambiguous(ids)
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

fn normalize_for_containment(path: &Path) -> PathBuf {
    if path.exists() {
        return canonicalize_path(path).unwrap_or_else(|_| path.to_path_buf());
    }
    // Missing tail (lexical fallback): canonicalize the longest existing
    // ancestor so Windows 8.3 / junction parents still match stored roots.
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    while let Some(name) = cur.file_name() {
        tail.push(name.to_os_string());
        if !cur.pop() {
            break;
        }
        if cur.exists() {
            let mut base = canonicalize_path(&cur).unwrap_or(cur);
            for c in tail.iter().rev() {
                base.push(c);
            }
            return base;
        }
    }
    path.to_path_buf()
}

fn components_eq(a: Component<'_>, b: Component<'_>) -> bool {
    #[cfg(windows)]
    {
        a.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        a == b
    }
}

/// True when `cwd` is `root` or a descendant, matching whole path components
/// (not string prefixes). Windows compares components with
/// `eq_ignore_ascii_case`. Canonicalize when the path exists; on failure use
/// lexical components. Do **not** use `Path::starts_with` (case-sensitive on
/// Windows) and do **not** unify with `harness/grok.rs` `path_is_under`.
pub fn path_contains_cwd(root: &Path, cwd: &Path) -> bool {
    let root_n = normalize_for_containment(root);
    let cwd_n = normalize_for_containment(cwd);
    let root_cs: Vec<Component<'_>> = root_n.components().collect();
    let cwd_cs: Vec<Component<'_>> = cwd_n.components().collect();
    if root_cs.len() > cwd_cs.len() {
        return false;
    }
    root_cs
        .iter()
        .zip(cwd_cs.iter())
        .all(|(r, c)| components_eq(*r, *c))
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
        assert!(
            loaded.projects[0].auto_merge,
            "missing auto_merge on old registry JSON defaults true"
        );
        assert!(
            loaded.projects[0].phase_timeouts_secs.is_empty(),
            "missing phase_timeouts_secs on old registry JSON defaults empty"
        );
        assert!(
            !loaded.projects[0].notify_progress,
            "missing notify_progress on old registry JSON defaults false"
        );
        assert!(
            loaded.projects[0].ready_aliases.is_empty(),
            "missing ready_aliases on old registry JSON defaults empty"
        );
        let _guard = crate::config::test_env_lock();
        let isolated = tempdir().unwrap();
        unsafe {
            std::env::remove_var(crate::workflow::timeouts::ENV_PHASE_TIMEOUT_SECS);
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, isolated.path());
        }
        assert_eq!(
            crate::workflow::timeout_for_phase(&loaded.projects[0], "plan"),
            std::time::Duration::from_secs(1800)
        );
        unsafe {
            std::env::remove_var(crate::config::ENV_COORDINATOR_HOME);
        }
    }

    #[test]
    fn set_auto_merge_false_round_trip() {
        let proj = tempdir().unwrap();
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        assert!(rec.auto_merge);
        let updated = reg
            .set(
                &rec.id,
                ProjectSetOptions {
                    auto_merge: Some(false),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(!updated.auto_merge);
        let home = tempdir().unwrap();
        let reg_path = home.path().join("registry.json");
        reg.save(&reg_path).unwrap();
        let loaded = Registry::load(&reg_path).unwrap();
        assert!(!loaded.projects[0].auto_merge);
    }

    #[test]
    fn set_notify_progress_true_false_round_trip() {
        let proj = tempdir().unwrap();
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        assert!(!rec.notify_progress);
        let on = reg
            .set(
                &rec.id,
                ProjectSetOptions {
                    notify_progress: Some(true),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(on.notify_progress);
        let home = tempdir().unwrap();
        let reg_path = home.path().join("registry.json");
        reg.save(&reg_path).unwrap();
        let loaded = Registry::load(&reg_path).unwrap();
        assert!(loaded.projects[0].notify_progress);
        let mut r = Registry::load(&reg_path).unwrap();
        let off = r
            .set(
                &rec.id,
                ProjectSetOptions {
                    notify_progress: Some(false),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(!off.notify_progress);
        r.save(&reg_path).unwrap();
        let again = Registry::load(&reg_path).unwrap();
        assert!(!again.projects[0].notify_progress);
        let text = std::fs::read_to_string(&reg_path).unwrap();
        assert!(
            !text.contains("notify_progress"),
            "false notify_progress must omit the key: {text}"
        );
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

    #[test]
    fn set_phase_timeouts_merge_and_second_project_stays_table() {
        let _guard = crate::config::test_env_lock();
        let home = tempdir().unwrap();
        unsafe {
            std::env::remove_var(crate::workflow::timeouts::ENV_PHASE_TIMEOUT_SECS);
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        let p1 = tempdir().unwrap();
        let p2 = tempdir().unwrap();
        let mut reg = Registry::default();
        let a = reg.add(p1.path(), ProjectAddOptions::default()).unwrap();
        let b = reg.add(p2.path(), ProjectAddOptions::default()).unwrap();
        assert!(a.phase_timeouts_secs.is_empty());

        let mut first = BTreeMap::new();
        first.insert("plan".into(), 3600);
        first.insert("implement".into(), 10800);
        let updated = reg
            .set(
                &a.id,
                ProjectSetOptions {
                    phase_timeouts_secs: Some(first),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(updated.phase_timeouts_secs.get("plan"), Some(&3600));
        assert_eq!(updated.phase_timeouts_secs.get("implement"), Some(&10800));
        assert_eq!(
            crate::workflow::timeout_for_phase(&updated, "plan"),
            std::time::Duration::from_secs(3600)
        );
        assert_eq!(
            crate::workflow::timeout_for_phase(&updated, "implement"),
            std::time::Duration::from_secs(10800)
        );

        let other = reg.find_by_id(&b.id).unwrap();
        assert!(other.phase_timeouts_secs.is_empty());
        assert_eq!(
            crate::workflow::timeout_for_phase(other, "plan"),
            std::time::Duration::from_secs(1800)
        );
        assert_eq!(
            crate::workflow::timeout_for_phase(other, "implement"),
            std::time::Duration::from_secs(7200)
        );

        let json_path = home.path().join("registry.json");
        reg.save(&json_path).unwrap();
        let saved = std::fs::read_to_string(&json_path).unwrap();
        assert!(saved.contains("phase_timeouts_secs"));
        let loaded = Registry::load(&json_path).unwrap();
        assert!(
            loaded
                .find_by_id(&b.id)
                .unwrap()
                .phase_timeouts_secs
                .is_empty()
        );
        unsafe {
            std::env::remove_var(crate::config::ENV_COORDINATOR_HOME);
        }
    }

    #[test]
    fn set_phase_timeouts_merges_across_calls() {
        let proj = tempdir().unwrap();
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        let mut plan = BTreeMap::new();
        plan.insert("plan".into(), 3600);
        reg.set(
            &rec.id,
            ProjectSetOptions {
                phase_timeouts_secs: Some(plan),
                ..Default::default()
            },
        )
        .unwrap();
        let mut implement = BTreeMap::new();
        implement.insert("implement".into(), 10800);
        let updated = reg
            .set(
                &rec.id,
                ProjectSetOptions {
                    phase_timeouts_secs: Some(implement),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(updated.phase_timeouts_secs.get("plan"), Some(&3600));
        assert_eq!(updated.phase_timeouts_secs.get("implement"), Some(&10800));
    }

    #[test]
    fn set_rejects_zero_and_unknown_before_write() {
        let proj = tempdir().unwrap();
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        let mut bad_zero = BTreeMap::new();
        bad_zero.insert("plan".into(), 0);
        let err = reg
            .set(
                &rec.id,
                ProjectSetOptions {
                    phase_timeouts_secs: Some(bad_zero),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("0"));
        assert!(
            reg.find_by_id(&rec.id)
                .unwrap()
                .phase_timeouts_secs
                .is_empty()
        );

        let mut bad_key = BTreeMap::new();
        bad_key.insert("planner".into(), 1);
        let err = reg
            .set(
                &rec.id,
                ProjectSetOptions {
                    phase_timeouts_secs: Some(bad_key),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("unknown phase"));
        assert!(
            reg.find_by_id(&rec.id)
                .unwrap()
                .phase_timeouts_secs
                .is_empty()
        );

        let err = reg
            .set(
                &rec.id,
                ProjectSetOptions {
                    clear_phase_timeout: vec!["nope".into()],
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("unknown phase"));
        assert!(
            reg.find_by_id(&rec.id)
                .unwrap()
                .phase_timeouts_secs
                .is_empty()
        );
    }

    #[test]
    fn set_clear_one_all_and_clear_all_then_overlay() {
        let proj = tempdir().unwrap();
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        let mut both = BTreeMap::new();
        both.insert("plan".into(), 3600);
        both.insert("implement".into(), 10800);
        reg.set(
            &rec.id,
            ProjectSetOptions {
                phase_timeouts_secs: Some(both),
                ..Default::default()
            },
        )
        .unwrap();

        let after_one = reg
            .set(
                &rec.id,
                ProjectSetOptions {
                    clear_phase_timeout: vec!["plan".into()],
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(!after_one.phase_timeouts_secs.contains_key("plan"));
        assert_eq!(after_one.phase_timeouts_secs.get("implement"), Some(&10800));

        let mut overlay = BTreeMap::new();
        overlay.insert("plan".into(), 11);
        let after_all_overlay = reg
            .set(
                &rec.id,
                ProjectSetOptions {
                    clear_phase_timeouts: true,
                    phase_timeouts_secs: Some(overlay),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(after_all_overlay.phase_timeouts_secs.len(), 1);
        assert_eq!(after_all_overlay.phase_timeouts_secs.get("plan"), Some(&11));

        let cleared = reg
            .set(
                &rec.id,
                ProjectSetOptions {
                    clear_phase_timeouts: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(cleared.phase_timeouts_secs.is_empty());
        let home = tempdir().unwrap();
        let reg_path = home.path().join("registry.json");
        reg.save(&reg_path).unwrap();
        let json = std::fs::read_to_string(&reg_path).unwrap();
        assert!(
            !json.contains("phase_timeouts_secs"),
            "empty map must omit the key on save"
        );
    }

    #[test]
    fn add_validates_initial_phase_timeout_map() {
        let proj = tempdir().unwrap();
        let mut reg = Registry::default();
        let mut bad = BTreeMap::new();
        bad.insert("plan".into(), 0);
        let err = reg
            .add(
                proj.path(),
                ProjectAddOptions {
                    phase_timeouts_secs: bad,
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("0"));
        assert!(reg.projects.is_empty());
    }

    fn two_projects() -> (tempfile::TempDir, tempfile::TempDir, Registry) {
        let a = tempdir().unwrap();
        let b = tempdir().unwrap();
        let mut reg = Registry::default();
        reg.add(a.path(), ProjectAddOptions::default()).unwrap();
        reg.add(b.path(), ProjectAddOptions::default()).unwrap();
        (a, b, reg)
    }

    #[test]
    fn resolve_cwd_under_b_workspace() {
        let (_a, b, reg) = two_projects();
        let id_b = reg.projects[1].id.clone();
        let resolved = reg.resolve_project_in(None, Some(b.path()), None).unwrap();
        assert_eq!(resolved.id, id_b);
    }

    #[test]
    fn resolve_cwd_under_b_nested_execution() {
        let a = tempdir().unwrap();
        let b_ws = tempdir().unwrap();
        let hands = b_ws.path().join("hands");
        std::fs::create_dir_all(&hands).unwrap();
        let mut reg = Registry::default();
        reg.add(a.path(), ProjectAddOptions::default()).unwrap();
        let rec_b = reg
            .add(
                b_ws.path(),
                ProjectAddOptions {
                    execution_repo: Some(hands.clone()),
                    ..Default::default()
                },
            )
            .unwrap();
        let from_ws = reg
            .resolve_project_in(None, Some(b_ws.path()), None)
            .unwrap();
        assert_eq!(from_ws.id, rec_b.id);
        let from_exec = reg.resolve_project_in(None, Some(&hands), None).unwrap();
        assert_eq!(from_exec.id, rec_b.id);
        let child = hands.join("src");
        std::fs::create_dir_all(&child).unwrap();
        let from_child = reg.resolve_project_in(None, Some(&child), None).unwrap();
        assert_eq!(from_child.id, rec_b.id);
    }

    #[test]
    fn resolve_last_used_when_cwd_matches_neither() {
        let (_a, _b, reg) = two_projects();
        let id_b = reg.projects[1].id.clone();
        let elsewhere = tempdir().unwrap();
        let resolved = reg
            .resolve_project_in(None, Some(elsewhere.path()), Some(&id_b))
            .unwrap();
        assert_eq!(resolved.id, id_b);
    }

    #[test]
    fn resolve_no_cwd_no_last_used_errors_with_new_message() {
        let (_a, _b, reg) = two_projects();
        let err = reg.resolve_project(None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("cwd is not inside a registered workspace or execution repo"),
            "{msg}"
        );
        assert!(msg.contains("pass --project"), "{msg}");
        let elsewhere = tempdir().unwrap();
        let stale = reg
            .resolve_project_in(None, Some(elsewhere.path()), Some("missing-id"))
            .unwrap_err()
            .to_string();
        assert!(
            stale.contains("cwd is not inside a registered workspace or execution repo"),
            "{stale}"
        );
    }

    #[test]
    fn resolve_longest_prefix_nested_workspaces() {
        let parent = tempdir().unwrap();
        let child = parent.path().join("child");
        std::fs::create_dir_all(&child).unwrap();
        let mut reg = Registry::default();
        let rec_parent = reg
            .add(
                parent.path(),
                ProjectAddOptions {
                    layout_profile: LayoutProfile::SingleRoot,
                    ..Default::default()
                },
            )
            .unwrap();
        let rec_child = reg
            .add(
                &child,
                ProjectAddOptions {
                    layout_profile: LayoutProfile::SingleRoot,
                    ..Default::default()
                },
            )
            .unwrap();
        let nested = child.join("src");
        std::fs::create_dir_all(&nested).unwrap();
        let resolved = reg.resolve_project_in(None, Some(&nested), None).unwrap();
        assert_eq!(resolved.id, rec_child.id);
        let at_parent = reg
            .resolve_project_in(None, Some(parent.path()), None)
            .unwrap();
        assert_eq!(at_parent.id, rec_parent.id);
    }

    #[test]
    fn resolve_equal_component_length_is_ambiguous_even_with_last_used() {
        let shared = tempdir().unwrap();
        let a_ws = tempdir().unwrap();
        let b_ws = tempdir().unwrap();
        let mut reg = Registry::default();
        let rec_a = reg
            .add(
                a_ws.path(),
                ProjectAddOptions {
                    execution_repo: Some(shared.path().to_path_buf()),
                    ..Default::default()
                },
            )
            .unwrap();
        let rec_b = reg
            .add(
                b_ws.path(),
                ProjectAddOptions {
                    execution_repo: Some(shared.path().to_path_buf()),
                    ..Default::default()
                },
            )
            .unwrap();
        let err = reg
            .resolve_project_in(None, Some(shared.path()), Some(&rec_a.id))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cwd matches multiple projects"), "{err}");
        assert!(err.contains(&rec_a.id), "{err}");
        assert!(err.contains(&rec_b.id), "{err}");
        let pos_a = err.find(&rec_a.id).unwrap();
        let pos_b = err.find(&rec_b.id).unwrap();
        if rec_a.id < rec_b.id {
            assert!(pos_a < pos_b, "ids must be sorted: {err}");
        } else {
            assert!(pos_b < pos_a, "ids must be sorted: {err}");
        }
    }

    #[test]
    fn resolve_explicit_wins_over_cwd() {
        let (a, b, reg) = two_projects();
        let id_a = reg.projects[0].id.clone();
        let resolved = reg
            .resolve_project_in(Some(&id_a), Some(b.path()), None)
            .unwrap();
        assert_eq!(resolved.id, id_a);
        let _ = a;
    }

    #[test]
    fn resolve_single_project_ignores_cwd_elsewhere() {
        let proj = tempdir().unwrap();
        let elsewhere = tempdir().unwrap();
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        let resolved = reg
            .resolve_project_in(None, Some(elsewhere.path()), None)
            .unwrap();
        assert_eq!(resolved.id, rec.id);
    }

    #[test]
    fn path_contains_cwd_mixed_case_windows() {
        let root = tempdir().unwrap();
        let cwd = root.path().join("src");
        std::fs::create_dir_all(&cwd).unwrap();
        assert!(path_contains_cwd(root.path(), &cwd));
        #[cfg(windows)]
        {
            let mixed = PathBuf::from(cwd.to_string_lossy().to_uppercase());
            assert!(
                path_contains_cwd(root.path(), &mixed),
                "mixed-case cwd must match: {}",
                mixed.display()
            );
        }
    }

    #[test]
    fn path_contains_cwd_rejects_string_prefix() {
        let parent = tempdir().unwrap();
        let orca = parent.path().join("Orca");
        let extra = parent.path().join("Orca-extra");
        std::fs::create_dir_all(&orca).unwrap();
        std::fs::create_dir_all(&extra).unwrap();
        assert!(path_contains_cwd(&orca, &orca));
        assert!(!path_contains_cwd(&orca, &extra));
        let mut reg = Registry::default();
        let rec_orca = reg.add(&orca, ProjectAddOptions::default()).unwrap();
        let rec_extra = reg.add(&extra, ProjectAddOptions::default()).unwrap();
        let got = reg.resolve_project_in(None, Some(&extra), None).unwrap();
        assert_eq!(got.id, rec_extra.id);
        assert_ne!(got.id, rec_orca.id);
    }

    #[test]
    fn resolve_multi_sibling_execution_repos_outside_hub() {
        let hub = tempdir().unwrap();
        let sibling = tempdir().unwrap();
        let other = tempdir().unwrap();
        let mut map = BTreeMap::new();
        map.insert("ledgerful".into(), sibling.path().to_path_buf());
        let mut reg = Registry::default();
        let rec_hub = reg
            .add(
                hub.path(),
                ProjectAddOptions {
                    layout_profile: LayoutProfile::MultiSibling,
                    execution_repos: map,
                    ..Default::default()
                },
            )
            .unwrap();
        reg.add(other.path(), ProjectAddOptions::default()).unwrap();
        let resolved = reg
            .resolve_project_in(None, Some(sibling.path()), None)
            .unwrap();
        assert_eq!(resolved.id, rec_hub.id);
        let at_hub = reg
            .resolve_project_in(None, Some(hub.path()), None)
            .unwrap();
        assert_eq!(at_hub.id, rec_hub.id);
    }

    #[test]
    fn resolve_lexical_fallback_when_cwd_does_not_exist() {
        let (_a, b, reg) = two_projects();
        let id_b = reg.projects[1].id.clone();
        let missing = b.path().join("not-created-yet");
        assert!(!missing.exists());
        let resolved = reg.resolve_project_in(None, Some(&missing), None).unwrap();
        assert_eq!(resolved.id, id_b);
    }

    #[test]
    fn set_ready_aliases_additive_dedup_and_clear() {
        let proj = tempdir().unwrap();
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        assert!(rec.ready_aliases.is_empty());
        let updated = reg
            .set(
                &rec.id,
                ProjectSetOptions {
                    ready_aliases: Some(vec![
                        "Ready — full plan @ 072399b6".into(),
                        "Ready — not started".into(),
                        "Ready - not started".into(),
                    ]),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            updated.ready_aliases,
            vec!["Ready — full plan @ 072399b6".to_string()]
        );
        let more = reg
            .set(
                &rec.id,
                ProjectSetOptions {
                    ready_aliases: Some(vec!["Ready — implement on GO".into()]),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            more.ready_aliases,
            vec![
                "Ready — full plan @ 072399b6".to_string(),
                "Ready — implement on GO".to_string(),
            ]
        );
        let home = tempdir().unwrap();
        let reg_path = home.path().join("registry.json");
        reg.save(&reg_path).unwrap();
        let json = std::fs::read_to_string(&reg_path).unwrap();
        assert!(json.contains("ready_aliases"));
        let loaded = Registry::load(&reg_path).unwrap();
        assert_eq!(loaded.projects[0].ready_aliases, more.ready_aliases);
        let cleared = reg
            .set(
                &rec.id,
                ProjectSetOptions {
                    clear_ready_aliases: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(cleared.ready_aliases.is_empty());
        reg.save(&reg_path).unwrap();
        let after = std::fs::read_to_string(&reg_path).unwrap();
        assert!(
            !after.contains("ready_aliases"),
            "empty aliases omit the key"
        );
    }
}
