//! Track execution-target resolution (track 0099).
//!
//! The unchanged-tree guard must sample the repository the track delivers in, not the
//! harness session cwd. This module resolves exactly one target for a prepared
//! `implement` / `address-findings` turn and freezes its identity for the prompt, the
//! sampler and the ACP target roots.
//!
//! Precedence is the conductor row's execution cell, then leading spec metadata, then
//! the project layout default. An explicit but invalid value never falls through; a
//! relative value anchors at the record path (so `.` is the workspace, not the primary
//! product repo). Everything is read-only: resolution never creates a directory,
//! mutates a layout profile or falls back to the harness session cwd.
//!
//! 0101 adds one stable delivery provenance. `gate_target` freezes `RunState.delivery_target`
//! for local gates and re-resolves only to validate; `mutation_target` always re-resolves
//! `conductor.md` immediately before a remote mutation and refuses a target that does not
//! equal both the frozen value and the `ci-wait` latch.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::registry::ProjectRecord;
use crate::state::{RunState, with_run_state_lock};

use super::graph::{
    PHASE_ADDRESS_CI, PHASE_ADDRESS_FINDINGS, PHASE_ADDRESS_REVIEW, PHASE_CI_WAIT,
    PHASE_CROSS_MODEL, PHASE_IMPLEMENT, resolve_track_dir,
};

/// Stable resolution-error prefix (diagnostics and tests key on it).
pub const RESOLUTION_PREFIX: &str = "leg repo unresolved:";

/// Prepared phases are the adapter phases that receive a prepared prompt/target root.
/// 0101 adds `address-ci`: its leg is graded where it wrote.
pub fn prepares_target(phase: &str) -> bool {
    matches!(
        phase,
        PHASE_IMPLEMENT | PHASE_ADDRESS_FINDINGS | PHASE_ADDRESS_CI | PHASE_ADDRESS_REVIEW
    )
}

/// Phases whose gates may read the frozen delivery target.
pub fn uses_delivery_target(phase: &str) -> bool {
    prepares_target(phase) || matches!(phase, PHASE_CROSS_MODEL | PHASE_CI_WAIT)
}

/// Where the declared target came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionSource {
    RowExecution,
    SpecMetadata,
    ProjectDefault,
}

impl ResolutionSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RowExecution => "row_execution",
            Self::SpecMetadata => "spec_metadata",
            Self::ProjectDefault => "project_default",
        }
    }
}

impl std::fmt::Display for ResolutionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One immutable execution target for a prepared turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedExecution {
    pub project_id: String,
    pub run_epoch: u64,
    pub phase: String,
    pub track_id: String,
    pub declared_path: PathBuf,
    /// Where the adapter is told to work (epoch worktree when isolation applies).
    pub prompt_path: PathBuf,
    /// Git toplevel sampled for the unchanged-tree verdict.
    pub sample_path: PathBuf,
    pub source: ResolutionSource,
    pub planning_only: bool,
    /// Execution labels found after the metadata boundary. Diagnostic only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub late_metadata: Vec<String>,
}

impl ResolvedExecution {
    /// Identity comparison used for staleness checks. Paths are compared with
    /// `paths_same`, so a case or separator difference is not a new target.
    pub fn same_identity(&self, other: &Self) -> bool {
        self.project_id == other.project_id
            && self.run_epoch == other.run_epoch
            && self.phase == other.phase
            && self.track_id == other.track_id
            && paths_same(&self.declared_path, &other.declared_path)
            && paths_same(&self.prompt_path, &other.prompt_path)
            && paths_same(&self.sample_path, &other.sample_path)
    }
}

/// Serializable injection snapshot carrying the frozen identity and target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedInjection {
    pub project_id: String,
    pub run_epoch: u64,
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_id: Option<String>,
    /// `Some` only for prepared phases. Legacy prompt-only phases carry the identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ResolvedExecution>,
}

impl PreparedInjection {
    pub fn for_target(target: ResolvedExecution) -> Self {
        Self {
            project_id: target.project_id.clone(),
            run_epoch: target.run_epoch,
            phase: target.phase.clone(),
            track_id: Some(target.track_id.clone()),
            target: Some(target),
        }
    }

    /// Identity-only snapshot for unprepared legs (`plan`, `fold`, `advance`) and for
    /// manual six-phase turns that may classify a block. It carries no target.
    pub fn legacy(state: &RunState) -> Self {
        Self {
            project_id: state.project_id.clone(),
            run_epoch: state.run_epoch,
            phase: state.phase.clone(),
            track_id: state.track_id.clone(),
            target: None,
        }
    }

    pub fn target(&self) -> Option<&ResolvedExecution> {
        self.target.as_ref()
    }

    /// True when the injected identity is still the current run identity.
    pub fn matches_state(&self, state: &RunState) -> bool {
        self.project_id == state.project_id
            && self.run_epoch == state.run_epoch
            && self.phase == state.phase
            && self.track_id == state.track_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoResolutionError {
    pub track: Option<String>,
    pub source: Option<ResolutionSource>,
    pub path: Option<PathBuf>,
    pub reason: String,
}

impl RepoResolutionError {
    fn new(track: Option<&str>, reason: impl Into<String>) -> Self {
        Self {
            track: track.map(str::to_string),
            source: None,
            path: None,
            reason: reason.into(),
        }
    }

    fn with_source(mut self, source: ResolutionSource) -> Self {
        self.source = Some(source);
        self
    }

    fn with_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }
}

impl std::fmt::Display for RepoResolutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{RESOLUTION_PREFIX}")?;
        if let Some(track) = &self.track {
            write!(f, " track={track}")?;
        }
        if let Some(source) = self.source {
            write!(f, " source={source}")?;
        }
        if let Some(path) = &self.path {
            write!(f, " path={}", path.display())?;
        }
        write!(f, " reason={}", self.reason)
    }
}

impl std::error::Error for RepoResolutionError {}

/// Resolve the one execution target for a prepared phase.
///
/// The caller supplies the state it pinned (`project_id`, `run_epoch`, `phase`,
/// `track_id`); this function never loads or selects a different epoch.
pub fn resolve_track_target(
    record: &ProjectRecord,
    state: &RunState,
    phase: &str,
) -> Result<ResolvedExecution, RepoResolutionError> {
    let track_id = state
        .track_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| RepoResolutionError::new(None, "no track id is pinned on the run"))?;

    let rows = super::conductor_md::load_track_rows(record).ok_or_else(|| {
        RepoResolutionError::new(
            Some(track_id),
            "cannot read the track registry (store or conductor.md) track rows",
        )
    })?;
    let matching: Vec<_> = rows
        .iter()
        .filter(|row| super::conductor_md::row_matches_track(row, track_id))
        .collect();
    let row = match matching.as_slice() {
        [] => {
            return Err(RepoResolutionError::new(
                Some(track_id),
                format!("no conductor.md row matches track {track_id}"),
            ));
        }
        [row] => *row,
        _ => {
            return Err(RepoResolutionError::new(
                Some(track_id),
                format!("more than one conductor.md row matches track {track_id}"),
            ));
        }
    };

    let (declared, source, late_metadata) = if let Some(cell) = row
        .execution
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        let declared = parse_execution_cell(cell, record).map_err(|reason| {
            RepoResolutionError::new(Some(track_id), reason)
                .with_source(ResolutionSource::RowExecution)
                .with_path(cell)
        })?;
        (
            mapped_token_path(record, &declared),
            ResolutionSource::RowExecution,
            Vec::new(),
        )
    } else if let Some(meta) = read_spec_execution_metadata(record, track_id)? {
        (
            mapped_token_path(record, &meta.path.to_string_lossy()),
            ResolutionSource::SpecMetadata,
            meta.late,
        )
    } else {
        (
            layout_default(record, track_id)?,
            ResolutionSource::ProjectDefault,
            Vec::new(),
        )
    };

    let declared = canonicalize_existing_dir(&record.path, &declared).map_err(|reason| {
        RepoResolutionError::new(Some(track_id), reason)
            .with_source(source)
            .with_path(&declared)
    })?;

    let planning_only = is_planning_only(record, &declared);
    let (prompt_path, sample_path) = if planning_only {
        let sample = git_toplevel(&declared).unwrap_or_else(|| declared.clone());
        (declared.clone(), sample)
    } else {
        prompt_and_sample_paths(record, state, phase, &declared).map_err(|reason| {
            RepoResolutionError::new(Some(track_id), reason)
                .with_source(source)
                .with_path(&declared)
        })?
    };

    Ok(ResolvedExecution {
        project_id: record.id.clone(),
        run_epoch: state.run_epoch,
        phase: phase.to_string(),
        track_id: track_id.to_string(),
        declared_path: declared,
        prompt_path,
        sample_path,
        source,
        planning_only,
        late_metadata,
    })
}

/// Where a gate target came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetProvenance {
    Frozen,
    ReResolved,
}

impl TargetProvenance {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Frozen => "frozen",
            Self::ReResolved => "re-resolved",
        }
    }
}

impl std::fmt::Display for TargetProvenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A validated target for one gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateTarget {
    pub execution: ResolvedExecution,
    pub provenance: TargetProvenance,
    pub registered: bool,
}

/// Typed refusal for a remote mutation (spec §3.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MutationRefusal {
    NotDeliveryPhase(String),
    Unresolved(RepoResolutionError),
    PlanningOnly(PathBuf),
    Unregistered(PathBuf),
    Changed { from: Option<PathBuf>, to: PathBuf },
}

impl std::fmt::Display for MutationRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotDeliveryPhase(phase) => write!(f, "phase {phase} has no delivery target"),
            Self::Unresolved(err) => write!(f, "{err}"),
            Self::PlanningOnly(declared) => write!(
                f,
                "planning-only track has no product delivery to publish (target {})",
                declared.display()
            ),
            Self::Unregistered(declared) => write!(
                f,
                "target {} is not a registered execution binding; register it (project add/set) \
                 or declare a registered target, then failure resolve and run --track <id>",
                declared.display()
            ),
            Self::Changed { from, to } => write!(
                f,
                "execution target changed from {} to {} since the ci-wait target was latched",
                from.as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<none>".into()),
                to.display()
            ),
        }
    }
}

impl std::error::Error for MutationRefusal {}

/// Compare the repository identity of two targets. Phase is ignored: implement and
/// address-ci are different phases on the same delivery repo.
pub fn same_delivery_repo(a: &ResolvedExecution, b: &ResolvedExecution) -> bool {
    paths_same(&a.declared_path, &b.declared_path)
        && paths_same(&a.prompt_path, &b.prompt_path)
        && paths_same(&a.sample_path, &b.sample_path)
}

/// True when the target is the primary execution repo or lies under a registered
/// `execution_repos` binding. The binding's Git toplevel also counts.
pub fn registered_execution_target(record: &ProjectRecord, target: &ResolvedExecution) -> bool {
    let mut roots: Vec<PathBuf> = Vec::new();
    let paths = crate::layout::resolve(record);
    if let Some(primary) = paths.execution_repo {
        roots.push(primary);
    }
    if let Some(primary) = &record.execution_repo {
        roots.push(primary.clone());
    }
    roots.extend(record.execution_repos.values().cloned());
    let declared = &target.declared_path;
    let sample = &target.sample_path;
    for root in roots {
        // The common case is an exact binding or a subdirectory of it; avoid spawning
        // `git rev-parse` on every poll tick unless the direct compare misses.
        if paths_same(declared, &root)
            || path_under(declared, &root)
            || paths_same(sample, &root)
            || path_under(sample, &root)
        {
            return true;
        }
        let toplevel = git_toplevel(&root).unwrap_or_else(|| root.clone());
        if paths_same(declared, &toplevel)
            || path_under(declared, &toplevel)
            || paths_same(sample, &toplevel)
            || path_under(sample, &toplevel)
        {
            return true;
        }
    }
    false
}

/// The delivery gate target for a read path.
///
/// Rule 1: a frozen `delivery_target` with the pinned `(project, epoch, track)` and a
/// delivery phase is authority as-is. Rule 2: otherwise re-resolve `state.phase` and
/// validate the full identity. Rule 3: a non-delivery phase is a typed error.
pub fn gate_target(
    record: &ProjectRecord,
    state: &RunState,
) -> Result<GateTarget, RepoResolutionError> {
    if !uses_delivery_target(&state.phase) {
        return Err(RepoResolutionError::new(
            state.track_id.as_deref(),
            format!("phase {} has no delivery target", state.phase),
        ));
    }
    if let Some(frozen) = state.delivery_target.as_ref()
        && frozen.project_id == record.id
        && frozen.run_epoch == state.run_epoch
        && state.track_id.as_deref() == Some(frozen.track_id.as_str())
        && uses_delivery_target(&frozen.phase)
    {
        return Ok(GateTarget {
            registered: registered_execution_target(record, frozen),
            execution: frozen.clone(),
            provenance: TargetProvenance::Frozen,
        });
    }
    let target = resolve_track_target(record, state, &state.phase)?;
    if target.project_id != record.id
        || target.run_epoch != state.run_epoch
        || state.track_id.as_deref() != Some(target.track_id.as_str())
        || target.phase != state.phase
    {
        return Err(RepoResolutionError::new(
            Some(target.track_id.as_str()),
            "resolved target identity does not match the pinned run state",
        ));
    }
    Ok(GateTarget {
        registered: registered_execution_target(record, &target),
        execution: target,
        provenance: TargetProvenance::ReResolved,
    })
}

/// Re-resolve immediately before a remote mutation and require the fresh target to
/// equal the frozen value (when present) and the `ci-wait` latch (when present).
///
/// G1: `delivery_target = None` (legacy state or an 0083 ci-wait resume) is valid; the
/// fresh target is validated against the latch/registration and never against
/// `product_git_cwd`.
pub fn mutation_target(
    record: &ProjectRecord,
    state: &RunState,
    latched: Option<&Path>,
) -> Result<GateTarget, MutationRefusal> {
    if !uses_delivery_target(&state.phase) {
        return Err(MutationRefusal::NotDeliveryPhase(state.phase.clone()));
    }
    let fresh =
        resolve_track_target(record, state, &state.phase).map_err(MutationRefusal::Unresolved)?;
    if fresh.project_id != record.id
        || fresh.run_epoch != state.run_epoch
        || state.track_id.as_deref() != Some(fresh.track_id.as_str())
        || fresh.phase != state.phase
    {
        return Err(MutationRefusal::Unresolved(RepoResolutionError::new(
            Some(fresh.track_id.as_str()),
            "resolved target identity does not match the pinned run state",
        )));
    }
    if fresh.planning_only {
        return Err(MutationRefusal::PlanningOnly(fresh.declared_path));
    }
    if !registered_execution_target(record, &fresh) {
        return Err(MutationRefusal::Unregistered(fresh.declared_path));
    }
    if let Some(frozen) = state.delivery_target.as_ref() {
        let identity_matches = frozen.project_id == record.id
            && frozen.run_epoch == state.run_epoch
            && frozen.track_id == fresh.track_id
            && uses_delivery_target(&frozen.phase);
        if !identity_matches || !same_delivery_repo(&fresh, frozen) {
            return Err(MutationRefusal::Changed {
                from: Some(frozen.declared_path.clone()),
                to: fresh.declared_path.clone(),
            });
        }
    }
    if let Some(latched) = latched
        && !paths_same(&fresh.sample_path, latched)
    {
        return Err(MutationRefusal::Changed {
            from: Some(latched.to_path_buf()),
            to: fresh.declared_path.clone(),
        });
    }
    Ok(GateTarget {
        registered: true,
        execution: fresh,
        provenance: TargetProvenance::ReResolved,
    })
}

/// One persisted memo key per `(track, phase, provenance, repo)`.
pub fn gate_target_journal_key(gate: &GateTarget) -> String {
    let sample = gate
        .execution
        .sample_path
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    format!(
        "{}|{}|{}|{}",
        gate.execution.track_id, gate.execution.phase, gate.provenance, sample
    )
}

/// Journal a gate target once, on first non-frozen use or a repo change. The memo is
/// persisted by the caller's existing locked write.
pub fn journal_gate_target(
    record: &ProjectRecord,
    state: &mut RunState,
    gate: &GateTarget,
) -> bool {
    // A frozen target is the leg's own recorded provenance: no re-resolution happened and
    // the repo cannot change, so it never needs a journal line.
    if gate.provenance == TargetProvenance::Frozen {
        return false;
    }
    let key = gate_target_journal_key(gate);
    if state.gate_target_journal.as_deref() == Some(key.as_str()) {
        return false;
    }
    crate::progress_log::append(
        record,
        "gate-target",
        &format!(
            "gate-target: track={} phase={} provenance={} repo={} source={}",
            gate.execution.track_id,
            gate.execution.phase,
            gate.provenance,
            gate.execution.sample_path.display(),
            gate.execution.source
        ),
    );
    state.gate_target_journal = Some(key);
    true
}

/// Journal through the run-state lock for gates that do not already hold it.
pub fn note_gate_target(record: &ProjectRecord, gate: &GateTarget) -> crate::error::Result<()> {
    // Frozen targets never journal; skip the lock and state read on every poll tick.
    if gate.provenance == TargetProvenance::Frozen {
        return Ok(());
    }
    with_run_state_lock(record, || {
        let mut state = crate::state::load_run_state(record)?;
        if journal_gate_target(record, &mut state, gate) {
            state.updated_at = chrono::Utc::now();
            crate::state::save_run_state(record, &state)?;
        }
        Ok(())
    })
}

/// Test constructor for a fixture delivery target. The repo should be a real temp Git
/// repo when the gate reads HEAD or dirt.
#[cfg(test)]
pub(crate) fn seeded_delivery_target(
    record: &ProjectRecord,
    track: &str,
    epoch: u64,
    phase: &str,
    repo: &Path,
) -> ResolvedExecution {
    let declared = dunce::canonicalize(repo)
        .map(|p| PathBuf::from(dunce::simplified(&p)))
        .unwrap_or_else(|_| repo.to_path_buf());
    let sample = git_toplevel(&declared).unwrap_or_else(|| declared.clone());
    ResolvedExecution {
        project_id: record.id.clone(),
        run_epoch: epoch,
        phase: phase.into(),
        track_id: track.into(),
        declared_path: declared.clone(),
        prompt_path: declared,
        sample_path: sample,
        source: ResolutionSource::RowExecution,
        planning_only: false,
        late_metadata: Vec::new(),
    }
}

/// Fixture guard (spec §3.7): a delivery-phase fixture must seed `delivery_target`.
#[cfg(test)]
pub(crate) fn assert_fixture_has_delivery_target(state: &RunState) {
    assert!(
        !uses_delivery_target(&state.phase) || state.delivery_target.is_some(),
        "fixture state for phase {} has no delivery_target; use seeded_delivery_target",
        state.phase
    );
}

/// The project layout default, made explicit. MultiSibling without an explicit
/// `execution_repo` is ambiguous when more than one binding exists and must fail
/// rather than take the BTreeMap-first repo.
fn layout_default(record: &ProjectRecord, track_id: &str) -> Result<PathBuf, RepoResolutionError> {
    let paths = crate::layout::resolve(record);
    match record.layout_profile {
        crate::layout::LayoutProfile::SingleRoot => Ok(paths.workspace_root),
        crate::layout::LayoutProfile::Nested => paths.execution_repo.ok_or_else(|| {
            RepoResolutionError::new(
                Some(track_id),
                "nested project has no execution_repo and the track declares no target",
            )
        }),
        crate::layout::LayoutProfile::MultiSibling => {
            if let Some(explicit) = record.execution_repo.clone() {
                return Ok(explicit);
            }
            let bindings = &record.execution_repos;
            match bindings.len() {
                0 => Err(RepoResolutionError::new(
                    Some(track_id),
                    "multi_sibling project has no execution bindings and the track declares no target",
                )),
                1 => Ok(bindings.values().next().expect("one binding").clone()),
                _ => {
                    let names: Vec<String> = bindings
                        .iter()
                        .map(|(name, path)| format!("{name}={}", path.display()))
                        .collect();
                    Err(RepoResolutionError::new(
                        Some(track_id),
                        format!(
                            "multi_sibling project has no explicit execution_repo; declare one \
                             execution target in the track row or set execution_repo (candidates: {})",
                            names.join(", ")
                        ),
                    ))
                }
            }
        }
    }
}

/// An exact `execution_repos` key maps to its bound path; anything else is a path.
fn mapped_token_path(record: &ProjectRecord, token: &str) -> PathBuf {
    record
        .execution_repos
        .get(token)
        .cloned()
        .unwrap_or_else(|| PathBuf::from(token))
}

fn canonicalize_existing_dir(anchor: &Path, declared: &Path) -> Result<PathBuf, String> {
    let joined = if declared.is_absolute() {
        declared.to_path_buf()
    } else {
        anchor.join(declared)
    };
    let canonical = dunce::canonicalize(&joined).map_err(|e| {
        format!(
            "execution target {} is not an existing directory: {e}; declare one execution target",
            joined.display()
        )
    })?;
    if !canonical.is_dir() {
        return Err(format!(
            "execution target {} is not a directory; declare one execution target",
            canonical.display()
        ));
    }
    Ok(PathBuf::from(dunce::simplified(&canonical)))
}

fn git_toplevel(path: &Path) -> Option<PathBuf> {
    let out = crate::git_command::command()
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(path)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        return None;
    }
    let path = PathBuf::from(text);
    dunce::canonicalize(&path)
        .ok()
        .map(|p| PathBuf::from(dunce::simplified(&p)))
}

fn git_common_dir(path: &Path) -> Option<PathBuf> {
    let out = crate::git_command::command()
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(path)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        return None;
    }
    dunce::canonicalize(PathBuf::from(text))
        .ok()
        .map(|p| PathBuf::from(dunce::simplified(&p)))
}

fn prompt_and_sample_paths(
    record: &ProjectRecord,
    state: &RunState,
    phase: &str,
    declared: &Path,
) -> Result<(PathBuf, PathBuf), String> {
    let isolation = crate::worktree::isolation_enabled(record)
        .map_err(|e| format!("worktree isolation setting is invalid: {e}"))?;
    if !isolation || !uses_delivery_target(phase) {
        let sample = git_toplevel(declared).unwrap_or_else(|| declared.to_path_buf());
        return Ok((declared.to_path_buf(), sample));
    }

    // Only a target inside the primary execution repo uses the epoch worktree.
    // An explicitly selected external sibling stays direct and creates nothing.
    let primary = layout_default(record, &state.track_id.clone().unwrap_or_default())
        .and_then(|p| {
            canonicalize_existing_dir(&record.path, &p)
                .map_err(|reason| RepoResolutionError::new(state.track_id.as_deref(), reason))
        })
        .map_err(|e| e.reason)?;
    if !(paths_same(&primary, declared) || path_under(declared, &primary)) {
        let sample = git_toplevel(declared).unwrap_or_else(|| declared.to_path_buf());
        return Ok((declared.to_path_buf(), sample));
    }

    let epoch = crate::worktree::epoch_dir(record, state.run_epoch)
        .map_err(|e| format!("cannot resolve epoch directory: {e}"))?;
    if !epoch.is_dir() {
        return Err(format!(
            "epoch {} worktree is missing at {}; prepare the epoch before injecting",
            state.run_epoch,
            epoch.display()
        ));
    }
    let primary_common = git_common_dir(&primary).ok_or_else(|| {
        format!(
            "cannot read the primary Git common dir at {}",
            primary.display()
        )
    })?;
    let epoch_common = git_common_dir(&epoch).ok_or_else(|| {
        format!(
            "cannot read the epoch Git common dir at {}",
            epoch.display()
        )
    })?;
    if !paths_same(&primary_common, &epoch_common) {
        return Err(format!(
            "epoch {} at {} belongs to a different Git repository; refusing a shared-root fallback",
            state.run_epoch,
            epoch.display()
        ));
    }

    let suffix = declared
        .strip_prefix(&primary)
        .unwrap_or_else(|_| Path::new(""));
    let prompt = if suffix.as_os_str().is_empty() {
        epoch.clone()
    } else {
        epoch.join(suffix)
    };
    let prompt = canonicalize_existing_dir(&epoch, &prompt).map_err(|reason| {
        format!(
            "epoch {} does not contain the declared subdirectory: {reason}",
            epoch.display()
        )
    })?;
    let sample = git_toplevel(&prompt).unwrap_or_else(|| epoch.clone());
    Ok((prompt, sample))
}

/// Planning-only predicate. Reads only this record's layout-resolved product paths.
fn is_planning_only(record: &ProjectRecord, candidate: &Path) -> bool {
    if record.layout_profile == crate::layout::LayoutProfile::SingleRoot {
        return false;
    }
    let paths = crate::layout::resolve(record);
    let in_planning_scope =
        paths_same(candidate, &paths.workspace_root) || path_under(candidate, &paths.conductor_dir);
    if !in_planning_scope {
        return false;
    }
    let mut product_roots: Vec<PathBuf> = Vec::new();
    if let Some(exec) = &record.execution_repo {
        product_roots.push(exec.clone());
    }
    product_roots.extend(record.execution_repos.values().cloned());
    for root in product_roots {
        let root = dunce::canonicalize(&root).unwrap_or(root);
        if paths_same(candidate, &root) || path_under(candidate, &root) {
            return false;
        }
        if let Some(toplevel) = git_toplevel(&root)
            && (paths_same(candidate, &toplevel) || path_under(candidate, &toplevel))
        {
            return false;
        }
    }
    true
}

/// Repository identity for a receipt/epoch comparison. Two linked worktrees of the same
/// repository share a Git common dir; otherwise fall back to path equality.
pub(crate) fn same_git_repository(a: &Path, b: &Path) -> bool {
    match (git_common_dir(a), git_common_dir(b)) {
        (Some(a), Some(b)) => paths_same(&a, &b),
        _ => paths_same(a, b),
    }
}

pub(crate) fn paths_same(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        p.to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    };
    // Windows CI temp paths may be 8.3 short names on one side and long names
    // on the other. Compare canonical forms when both exist.
    if let (Ok(a), Ok(b)) = (dunce::canonicalize(a), dunce::canonicalize(b)) {
        return norm(&a) == norm(&b);
    }
    norm(a) == norm(b)
}

fn path_under(path: &Path, root: &Path) -> bool {
    let p = path
        .to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase();
    let r = root
        .to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase();
    p.starts_with(&format!("{r}\\"))
}

// ---------------------------------------------------------------------------
// Row / metadata token grammar
// ---------------------------------------------------------------------------

struct SpecMetadata {
    path: PathBuf,
    late: Vec<String>,
}

fn read_spec_execution_metadata(
    record: &ProjectRecord,
    track_id: &str,
) -> Result<Option<SpecMetadata>, RepoResolutionError> {
    let Some(dir) = resolve_track_dir(record, track_id) else {
        return Ok(None);
    };
    let spec = dir.join("spec.md");
    if !spec.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&spec).map_err(|e| {
        RepoResolutionError::new(
            Some(track_id),
            format!("cannot read spec metadata at {}: {e}", spec.display()),
        )
        .with_path(&spec)
    })?;
    parse_spec_execution_metadata(&text, record)
        .map(|opt| {
            opt.map(|(path, late)| SpecMetadata {
                path: PathBuf::from(path),
                late,
            })
        })
        .map_err(|reason| RepoResolutionError::new(Some(track_id), reason).with_path(&spec))
}

/// Parse leading metadata: H1 is ignored, the first numbered H2-H6 ends the section.
/// Fenced code and quoted lines are ignored. Late labels are reported, never selected.
fn parse_spec_execution_metadata(
    text: &str,
    record: &ProjectRecord,
) -> Result<Option<(String, Vec<String>)>, String> {
    let anchor = &record.path;
    let mut in_fence = false;
    let mut stopped = false;
    let mut values: Vec<(String, String)> = Vec::new();
    let mut late = Vec::new();
    for raw in text.lines() {
        let line = raw.trim_end_matches('\r');
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if let Some(level) = heading_level(trimmed) {
            if level >= 2 && numbered_section_title(trimmed, level) {
                stopped = true;
                continue;
            }
            // The initial H1 track title is not a metadata boundary.
            continue;
        }
        if let Some((label, value)) = metadata_label(line) {
            if stopped {
                late.push(label);
                continue;
            }
            if value.trim().is_empty() {
                continue;
            }
            let parsed = parse_token_value(value.trim(), record)
                .map_err(|reason| format!("execution metadata `{label}` is invalid: {reason}"))?;
            values.push((label, parsed));
        }
    }
    if values.is_empty() {
        return Ok(None);
    }
    let first = values[0].1.clone();
    if let Some((label, other)) = values
        .iter()
        .find(|(_, v)| !token_paths_same(&first, v, anchor))
    {
        return Err(format!(
            "conflicting execution metadata: `{}` = {first} and `{label}` = {other}; \
             declare one execution target",
            values[0].0
        ));
    }
    Ok(Some((first, late)))
}

fn heading_level(line: &str) -> Option<usize> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &line[hashes..];
    if rest.starts_with(' ') || rest.is_empty() {
        Some(hashes)
    } else {
        None
    }
}

fn numbered_section_title(line: &str, level: usize) -> bool {
    let rest = line[level..].trim_start();
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if !(1..=2).contains(&digits) {
        return false;
    }
    rest[digits..]
        .chars()
        .next()
        .is_some_and(|c| c == '.' || c == ')' || c.is_ascii_whitespace())
}

fn metadata_label(line: &str) -> Option<(String, String)> {
    let mut text = line.trim();
    if text.starts_with('>') {
        return None;
    }
    text = text.trim_start_matches(['-', '*', '+']).trim_start();
    loop {
        let next = text
            .strip_prefix("**")
            .or_else(|| text.strip_prefix("__"))
            .or_else(|| text.strip_prefix('*'))
            .or_else(|| text.strip_prefix('_'));
        match next {
            Some(rest) => text = rest.trim_start(),
            None => break,
        }
    }
    let lower = text.to_ascii_lowercase();
    for label in [
        "execution repo / tool cwd",
        "execution repo",
        "execution path",
    ] {
        if lower.starts_with(label) {
            let rest = &text[label.len()..];
            let rest = rest.trim_start_matches(['*', '_']).trim_start();
            if let Some(value) = rest.strip_prefix(':').or_else(|| rest.strip_prefix('=')) {
                let value = value
                    .trim()
                    .trim_start_matches('*')
                    .trim_end_matches('*')
                    .trim();
                return Some((label.to_string(), value.to_string()));
            }
        }
    }
    None
}

/// Parse one execution cell/metadata value into a path string or map key.
fn parse_execution_cell(cell: &str, record: &ProjectRecord) -> Result<String, String> {
    let cell = cell.trim();
    if cell.is_empty() {
        return Err("empty execution target".into());
    }

    // Leading planning-only marker with a parenthesized path (HH 0099 shape).
    let lower = cell.to_ascii_lowercase();
    if lower.starts_with("planning-only") || lower.starts_with("planning only") {
        if let Some((_, inner)) = balanced_first_paren(cell) {
            let inner = inner.trim();
            if inner.is_empty() {
                return Err(
                    "planning-only marker has no path; put the execution path in the cell".into(),
                );
            }
            return parse_token_value(inner, record).map_err(|e| {
                format!(
                    "{e}; put the execution path in the cell (planning-only marker alone is not a target)"
                )
            });
        }
        return Err(
            "planning-only marker has no parenthesized path; put the execution path in the cell"
                .into(),
        );
    }

    // product + Project <absolute path> historical prefix.
    if lower.starts_with("product +") {
        let rest = cell["product +".len()..].trim_start();
        let rest = rest
            .strip_prefix("Project")
            .or_else(|| rest.strip_prefix("project"))
            .map(str::trim_start)
            .unwrap_or(rest);
        if rest.is_empty() {
            return Err(
                "product + Project form has no path; put the execution path in the cell".into(),
            );
        }
        return parse_token_value(rest, record);
    }

    parse_token_value(cell, record)
}

/// One bounded token: link, backtick path + optional annotation, bare path + optional
/// annotation, or exact map key. Multiple distinct targets are rejected.
fn parse_token_value(value: &str, record: &ProjectRecord) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("empty execution target".into());
    }

    if value.starts_with('[')
        && let Some((text, target, rest)) = split_markdown_link(value)
    {
        let mut candidates = Vec::new();
        if let Some(decoded) = decode_file_uri(&target)? {
            candidates.push(decoded);
        } else if looks_like_path(&target) {
            candidates.push(target.clone());
        } else {
            if looks_like_path(&text) {
                candidates.push(text.clone());
            }
            if candidates.is_empty() && is_map_key(&text, record) {
                candidates.push(text.clone());
            }
        }
        if candidates.is_empty() {
            return Err(format!(
                "markdown link [{text}]({target}) has no execution path destination or map key"
            ));
        }
        let chosen = candidates.remove(0);
        let extra = path_tokens_in(&rest);
        if !extra.is_empty() {
            return Err(multiple_target_error(&chosen, &extra));
        }
        return Ok(chosen);
    }

    // Whole backtick path, optionally followed by one trailing annotation.
    if let Some(stripped) = value.strip_prefix('`') {
        let Some(end) = stripped.find('`') else {
            return Err("unterminated backtick execution path".into());
        };
        let token = stripped[..end].trim();
        if token.is_empty() {
            return Err("empty backtick execution path".into());
        }
        let rest = stripped[end + 1..].trim();
        validate_trailing(rest, token)?;
        return finish_token(token, record);
    }

    // Bare path (no spaces unless quoted) with one trailing parenthetical annotation.
    if let Some((token, rest)) = split_trailing_paren(value) {
        validate_trailing(rest, token)?;
        return finish_token(token.trim(), record);
    }

    if value.contains(char::is_whitespace) {
        // A bare path containing spaces must be backtick-quoted; prose is not a target.
        let tokens = path_tokens_in(value);
        if tokens.len() > 1 {
            return Err(multiple_target_error(&tokens[0], &tokens[1..]));
        }
        if tokens.len() == 1 {
            return Err(format!(
                "execution target {} contains spaces and must be backtick-quoted",
                tokens[0]
            ));
        }
        return Err(format!(
            "unsupported execution target {value:?}; use a path, map key, link or backtick path"
        ));
    }
    finish_token(value, record)
}

fn finish_token(token: &str, record: &ProjectRecord) -> Result<String, String> {
    let token = token.trim();
    if token.is_empty() {
        return Err("empty execution path".into());
    }
    if looks_like_path(token) {
        return Ok(token.to_string());
    }
    if is_map_key(token, record) {
        return Ok(token.to_string());
    }
    Err(format!(
        "unknown execution target {token:?}; use an absolute path, a dot-relative path, \
         an execution_repos key or a link"
    ))
}

fn validate_trailing(rest: &str, token: &str) -> Result<(), String> {
    if rest.is_empty() {
        return Ok(());
    }
    let Some(inner) = rest
        .strip_prefix('(')
        .and_then(|r| r.strip_suffix(')'))
        .map(str::trim)
    else {
        let extra = path_tokens_in(rest);
        if !extra.is_empty() {
            return Err(multiple_target_error(token, &extra));
        }
        return Err(format!(
            "unsupported text after the execution target {token:?}; only one parenthetical annotation is allowed"
        ));
    };
    let extra = path_tokens_in(inner);
    if !extra.is_empty() {
        return Err(multiple_target_error(token, &extra));
    }
    Ok(())
}

fn multiple_target_error(first: &str, extra: &[String]) -> String {
    let mut all = vec![first.to_string()];
    all.extend(extra.iter().cloned());
    format!(
        "multiple distinct execution targets: {}; declare one execution target",
        all.join(", ")
    )
}

fn split_markdown_link(value: &str) -> Option<(String, String, String)> {
    let rest = value.strip_prefix('[')?;
    let close = rest.find("](")?;
    let text = rest[..close].to_string();
    let after = &rest[close + 2..];
    let end = after.find(')')?;
    let target = after[..end].to_string();
    Some((text, target, after[end + 1..].to_string()))
}

fn split_trailing_paren(value: &str) -> Option<(&str, &str)> {
    let end = value.rfind(')')?;
    if !value.ends_with(')') {
        return None;
    }
    let open = value[..end].rfind('(')?;
    if open == 0 {
        return None;
    }
    let token = &value[..open];
    if token.trim().is_empty() {
        return None;
    }
    Some((token, &value[open..]))
}

fn balanced_first_paren(value: &str) -> Option<(usize, &str)> {
    let open = value.find('(')?;
    let end = value.rfind(')')?;
    if end <= open {
        return None;
    }
    Some((open, &value[open + 1..end]))
}

fn looks_like_path(token: &str) -> bool {
    let t = token.trim();
    if t.is_empty() {
        return false;
    }
    if t.starts_with("file://") {
        return false;
    }
    let bytes = t.as_bytes();
    let drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    drive
        || t.starts_with("\\\\")
        || t.starts_with('/')
        || t == "."
        || t == ".."
        || t.starts_with("./")
        || t.starts_with("../")
        || t.starts_with(".\\")
        || t.starts_with("..\\")
}

fn is_map_key(token: &str, record: &ProjectRecord) -> bool {
    record.execution_repos.contains_key(token.trim())
}

fn path_tokens_in(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('`') else {
            break;
        };
        let inner = after[..end].trim();
        if looks_like_path(inner) || inner.starts_with("file://") {
            out.push(inner.to_string());
        }
        rest = &after[end + 1..];
    }
    // Bare drive-letter paths (e.g. two candidates joined by prose).
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i + 2 < chars.len() {
        if chars[i].is_ascii_alphabetic()
            && chars[i + 1] == ':'
            && (chars[i + 2] == '\\' || chars[i + 2] == '/')
        {
            let mut j = i + 3;
            while j < chars.len()
                && !chars[j].is_whitespace()
                && !matches!(chars[j], '`' | '|' | ')' | ']' | ',')
            {
                j += 1;
            }
            let token: String = chars[i..j].iter().collect();
            if !out.contains(&token) {
                out.push(token);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

fn token_paths_same(a: &str, b: &str, anchor: &Path) -> bool {
    let resolve = |t: &str| {
        let p = PathBuf::from(t);
        if p.is_absolute() { p } else { anchor.join(p) }
    };
    paths_same(&resolve(a), &resolve(b))
}

/// Bounded local `file:///C:/...` decoder: no dependencies, no remote authorities.
fn decode_file_uri(uri: &str) -> Result<Option<String>, String> {
    if !uri.starts_with("file://") {
        return Ok(None);
    }
    if uri.len() > 32 * 1024 {
        return Err("file URI exceeds the 32 KiB bound".into());
    }
    let rest = &uri["file://".len()..];
    if let Some(authority_end) = rest.find('/') {
        let authority = &rest[..authority_end];
        if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
            return Err(format!(
                "remote file URI authority {authority:?} is not allowed"
            ));
        }
    } else {
        return Err("file URI has no drive path".into());
    }
    let path_part = rest
        .strip_prefix("localhost")
        .unwrap_or(rest)
        .strip_prefix('/')
        .unwrap_or(rest);
    let drive_ok = {
        let b = path_part.as_bytes();
        b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && (b[2] == b'/' || b[2] == b'\\')
    };
    let drive_ok = drive_ok || {
        let b = rest.as_bytes();
        b.len() >= 4
            && b[0] == b'/'
            && b[1].is_ascii_alphabetic()
            && b[2] == b':'
            && (b[3] == b'/' || b[3] == b'\\')
    };
    if !drive_ok {
        return Err("file URI must carry an absolute Windows drive path".into());
    }
    let raw = rest.strip_prefix('/').unwrap_or(rest);
    if raw.contains('?') || raw.contains('#') {
        return Err("file URI queries and fragments are not allowed".into());
    }
    if raw.contains('%') {
        if raw.to_ascii_lowercase().contains("%25") {
            return Err("double-encoded file URI is not allowed".into());
        }
        for part in raw.split('%').skip(1) {
            let hex: String = part.chars().take(2).collect();
            if hex.len() != 2 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(format!("malformed percent escape in file URI near %{hex}"));
            }
            let value = u8::from_str_radix(&hex, 16).map_err(|_| "bad escape".to_string())?;
            if value == 0 {
                return Err("file URI contains NUL".into());
            }
            if matches!(value, b'/' | b'\\') {
                return Err("encoded path separators are not allowed".into());
            }
        }
    }
    let decoded = percent_decode(raw)?;
    if decoded.contains('\0') {
        return Err("file URI contains NUL".into());
    }
    Ok(Some(decoded.replace('/', "\\")))
}

fn percent_decode(raw: &str) -> Result<String, String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err("truncated percent escape in file URI".into());
            }
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                .map_err(|_| "invalid percent escape in file URI".to_string())?;
            let value = u8::from_str_radix(hex, 16)
                .map_err(|_| "invalid percent escape in file URI".to_string())?;
            out.push(value);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "file URI is not valid UTF-8".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::LayoutProfile;
    use crate::state::RunState;
    use chrono::Utc;
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    fn record(path: &Path, profile: LayoutProfile) -> ProjectRecord {
        ProjectRecord {
            id: "proj-0099".into(),
            path: path.to_path_buf(),
            display_name: None,
            layout_profile: profile,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: BTreeMap::new(),
            state_dir: None,
            auto_merge: true,
            phase_timeouts_secs: BTreeMap::new(),
            notify_progress: false,
            worktree_isolation: false,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            ci_fix_routing: false,
            review_comment_routing: false,
            skill_aliases: BTreeMap::new(),
            created_at: Utc::now(),
        }
    }

    fn running(track: &str) -> RunState {
        let mut state = RunState::idle("proj-0099");
        state.status = crate::state::RunStatus::Running;
        state.phase = PHASE_IMPLEMENT.into();
        state.track_id = Some(track.into());
        state.run_epoch = 1;
        state
    }

    fn registry(root: &Path, row_execution: Option<&str>) {
        let dir = root.join("conductor");
        std::fs::create_dir_all(&dir).unwrap();
        let cell = row_execution
            .map(|c| c.replace('|', "\\|"))
            .unwrap_or_default();
        std::fs::write(
            dir.join("conductor.md"),
            format!(
                "# Conductor\n\n| Track | Execution | Status | Summary |\n| --- | --- | --- | --- |\n| [0099-Example](0099-Example/spec.md) | {cell} | **Ready - not started** | target |\n"
            ),
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("0099-Example")).unwrap();
    }

    fn write_spec(root: &Path, body: &str) {
        std::fs::write(root.join("conductor/0099-Example/spec.md"), body).unwrap();
    }

    fn product_repo(root: &Path) -> PathBuf {
        let path = root.join("product");
        std::fs::create_dir_all(&path).unwrap();
        let out = crate::git_command::command()
            .args(["init", "-b", "main"])
            .current_dir(&path)
            .output()
            .unwrap();
        assert!(out.status.success());
        std::fs::write(path.join("f.txt"), "base\n").unwrap();
        for args in [&["add", "f.txt"][..], &["commit", "-m", "base"][..]] {
            let out = crate::git_command::command()
                .args(args)
                .current_dir(&path)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output()
                .unwrap();
            assert!(out.status.success());
        }
        PathBuf::from(dunce::simplified(&dunce::canonicalize(&path).unwrap()))
    }

    #[test]
    fn execution_headers_and_legacy_tables() {
        let text = "| Track | Execution repo | Status | Summary |\n| --- | --- | --- | --- |\n| [0099-A](0099-A/spec.md) | `C:\\dev\\x` | Ready | s |\n| [0100-B](0100-B/spec.md) | k | Ready | s |\n";
        let rows = crate::workflow::conductor_md::parse_conductor_md(text).unwrap();
        assert_eq!(rows[0].execution.as_deref(), Some("`C:\\dev\\x`"));
        assert_eq!(rows[1].execution.as_deref(), Some("k"));
        for header in ["Execution", "Execution path", "Execution Repo"] {
            let text = format!(
                "| Track | {header} | Status | Summary |\n| --- | --- | --- | --- |\n| [0099-A](0099-A/spec.md) | p | Ready | s |\n"
            );
            let rows = crate::workflow::conductor_md::parse_conductor_md(&text).unwrap();
            assert_eq!(rows[0].execution.as_deref(), Some("p"), "{header}");
        }
    }

    #[test]
    fn execution_target_precedence() {
        let dir = tempdir().unwrap();
        let product = product_repo(dir.path());
        let mut rec = record(dir.path(), LayoutProfile::Nested);
        rec.execution_repo = Some(product.clone());
        registry(dir.path(), None);
        let state = running("0099-Example");

        // No row cell, no metadata -> project default.
        let got = resolve_track_target(&rec, &state, PHASE_IMPLEMENT).unwrap();
        assert_eq!(got.source, ResolutionSource::ProjectDefault);
        assert_eq!(got.declared_path, product);

        // Leading metadata beats the default.
        let meta = dir.path().join("meta");
        std::fs::create_dir_all(&meta).unwrap();
        write_spec(
            dir.path(),
            &format!(
                "# 0099 — Title\n\n**Execution repo:** `{}`\n\n## 1. Objective\nprose\n",
                meta.display()
            ),
        );
        let got = resolve_track_target(&rec, &state, PHASE_IMPLEMENT).unwrap();
        assert_eq!(got.source, ResolutionSource::SpecMetadata);
        assert_eq!(got.declared_path, canonical(&meta));

        // The row cell beats both.
        let row_target = dir.path().join("row-target");
        std::fs::create_dir_all(&row_target).unwrap();
        registry(dir.path(), Some(&row_target.display().to_string()));
        let got = resolve_track_target(&rec, &state, PHASE_IMPLEMENT).unwrap();
        assert_eq!(got.source, ResolutionSource::RowExecution);
        assert_eq!(got.declared_path, canonical(&row_target));
    }

    fn canonical(path: &Path) -> PathBuf {
        PathBuf::from(dunce::simplified(&dunce::canonicalize(path).unwrap()))
    }

    #[test]
    fn empty_execution_cell_falls_through() {
        let dir = tempdir().unwrap();
        let product = product_repo(dir.path());
        let mut rec = record(dir.path(), LayoutProfile::Nested);
        rec.execution_repo = Some(product.clone());
        registry(dir.path(), Some("  "));
        let state = running("0099-Example");
        let got = resolve_track_target(&rec, &state, PHASE_IMPLEMENT).unwrap();
        assert_eq!(got.source, ResolutionSource::ProjectDefault);
        assert_eq!(got.declared_path, product);
    }

    #[test]
    fn invalid_explicit_target_never_defaults() {
        let dir = tempdir().unwrap();
        let product = product_repo(dir.path());
        let mut rec = record(dir.path(), LayoutProfile::Nested);
        rec.execution_repo = Some(product);
        registry(dir.path(), Some("not a path or key"));
        let state = running("0099-Example");
        let err = resolve_track_target(&rec, &state, PHASE_IMPLEMENT).unwrap_err();
        assert!(err.to_string().starts_with(RESOLUTION_PREFIX));
        assert!(
            err.reason.contains("unsupported execution target")
                || err.reason.contains("unknown execution target"),
            "{err}"
        );
    }

    #[test]
    fn relative_target_anchors_at_workspace() {
        let dir = tempdir().unwrap();
        let product = product_repo(dir.path());
        let mut rec = record(dir.path(), LayoutProfile::Nested);
        rec.execution_repo = Some(product);
        registry(dir.path(), Some("."));
        let state = running("0099-Example");
        let got = resolve_track_target(&rec, &state, PHASE_IMPLEMENT).unwrap();
        assert_eq!(
            got.declared_path,
            PathBuf::from(dunce::simplified(&dunce::canonicalize(dir.path()).unwrap()))
        );
    }

    #[test]
    fn dot_hub_vs_single_root() {
        let dir = tempdir().unwrap();
        let hub = dir.path().join("hub");
        std::fs::create_dir_all(&hub).unwrap();
        let mut rec = record(&hub, LayoutProfile::MultiSibling);
        rec.execution_repos
            .insert("one".into(), dir.path().join("one"));
        rec.execution_repos
            .insert("two".into(), dir.path().join("two"));
        for p in rec.execution_repos.values() {
            std::fs::create_dir_all(p).unwrap();
        }
        registry(&hub, Some("."));
        let state = running("0099-Example");
        assert!(
            resolve_track_target(&rec, &state, PHASE_IMPLEMENT)
                .unwrap()
                .planning_only
        );

        let single = dir.path().join("single");
        std::fs::create_dir_all(&single).unwrap();
        // conductor.md for the SingleRoot project
        registry(&single, Some("."));
        let mut rec2 = record(&single, LayoutProfile::SingleRoot);
        rec2.execution_repos.insert("one".into(), single.clone());
        let got = resolve_track_target(&rec2, &state, PHASE_IMPLEMENT).unwrap();
        assert!(!got.planning_only);
    }

    #[test]
    fn multi_sibling_multiple_bindings_without_primary_fails() {
        let dir = tempdir().unwrap();
        let hub = dir.path().join("hub");
        std::fs::create_dir_all(&hub).unwrap();
        let mut rec = record(&hub, LayoutProfile::MultiSibling);
        rec.execution_repos
            .insert("alpha".into(), dir.path().join("alpha"));
        rec.execution_repos
            .insert("beta".into(), dir.path().join("beta"));
        for p in rec.execution_repos.values() {
            std::fs::create_dir_all(p).unwrap();
        }
        registry(&hub, None);
        let state = running("0099-Example");
        let err = resolve_track_target(&rec, &state, PHASE_IMPLEMENT).unwrap_err();
        assert!(
            err.reason.contains("alpha=") && err.reason.contains("beta="),
            "{err}"
        );
    }

    #[test]
    fn single_root_nested_multi_sibling_defaults() {
        let dir = tempdir().unwrap();
        let single = dir.path().join("single");
        std::fs::create_dir_all(&single).unwrap();
        registry(&single, None);
        let state = running("0099-Example");
        let mut rec = record(&single, LayoutProfile::SingleRoot);
        rec.execution_repo = Some(dir.path().join("stale"));
        assert_eq!(
            resolve_track_target(&rec, &state, PHASE_IMPLEMENT)
                .unwrap()
                .declared_path,
            PathBuf::from(dunce::simplified(&dunce::canonicalize(&single).unwrap()))
        );

        let nested = dir.path().join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        registry(&nested, None);
        let mut rec = record(&nested, LayoutProfile::Nested);
        assert!(resolve_track_target(&rec, &state, PHASE_IMPLEMENT).is_err());
        rec.execution_repo = Some(product_repo(&nested));
        assert!(resolve_track_target(&rec, &state, PHASE_IMPLEMENT).is_ok());

        let hub = dir.path().join("hub");
        std::fs::create_dir_all(&hub).unwrap();
        registry(&hub, None);
        let mut rec = record(&hub, LayoutProfile::MultiSibling);
        let one = product_repo(&hub.join("one"));
        rec.execution_repos.insert("only".into(), one.clone());
        assert_eq!(
            resolve_track_target(&rec, &state, PHASE_IMPLEMENT)
                .unwrap()
                .declared_path,
            one
        );
    }

    #[test]
    fn map_key_resolves_to_the_bound_repo() {
        let dir = tempdir().unwrap();
        let hub = dir.path().join("hub");
        std::fs::create_dir_all(&hub).unwrap();
        let bound = product_repo(&dir.path().join("bound"));
        let mut rec = record(&hub, LayoutProfile::MultiSibling);
        // BTreeMap-first is a different repo; the explicit key must win.
        rec.execution_repos
            .insert("aaa-first".into(), product_repo(&dir.path().join("first")));
        rec.execution_repos.insert("primary".into(), bound.clone());
        registry(&hub, Some("primary"));
        let state = running("0099-Example");
        let got = resolve_track_target(&rec, &state, PHASE_IMPLEMENT).unwrap();
        assert_eq!(got.declared_path, bound);
    }

    #[test]
    fn absolute_target_outside_map() {
        let dir = tempdir().unwrap();
        let outside = product_repo(dir.path());
        let rec = record(dir.path(), LayoutProfile::Nested);
        registry(dir.path(), Some(&outside.display().to_string()));
        let state = running("0099-Example");
        let got = resolve_track_target(&rec, &state, PHASE_IMPLEMENT).unwrap();
        assert_eq!(got.declared_path, outside);
        assert_eq!(got.source, ResolutionSource::RowExecution);
    }

    #[test]
    fn annotated_live_execution_cells() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("some repo");
        std::fs::create_dir_all(&path).unwrap();
        let rec = record(dir.path(), LayoutProfile::Nested);
        let p = path.display().to_string();

        assert_eq!(
            parse_execution_cell(&format!("`{p}` (coordinated 0551)"), &rec).unwrap(),
            p
        );
        assert_eq!(
            parse_execution_cell(&format!("{p} (note about sub/ dir)"), &rec).unwrap(),
            p
        );
        assert_eq!(
            parse_execution_cell(&format!("planning-only (`{p}`)"), &rec).unwrap(),
            p
        );
        assert_eq!(
            parse_execution_cell(&format!("product + Project `{p}`"), &rec).unwrap(),
            p
        );
        assert!(parse_execution_cell("product + Helping Hands probe 0099", &rec).is_err());
        let other = dir.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        let err =
            parse_execution_cell(&format!("`{p}` or `{}`", other.display()), &rec).unwrap_err();
        assert!(
            err.contains(&p) && err.contains(&other.display().to_string()),
            "{err}"
        );
    }

    #[test]
    fn markdown_execution_destination() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("dest");
        std::fs::create_dir_all(&path).unwrap();
        let rec = record(dir.path(), LayoutProfile::Nested);
        assert_eq!(
            parse_execution_cell(&format!("[repo]({})", path.display()), &rec).unwrap(),
            path.display().to_string()
        );
        assert_eq!(
            parse_execution_cell(&format!("[{}](track)", path.display()), &rec).unwrap(),
            path.display().to_string()
        );
    }

    #[test]
    fn local_file_uri_decodes_spaces_rejects_remote_and_bad_escapes() {
        assert_eq!(
            decode_file_uri("file:///C:/dev/some%20repo")
                .unwrap()
                .as_deref(),
            Some(r"C:\dev\some repo")
        );
        assert!(decode_file_uri("file://server/share/repo").is_err());
        assert!(decode_file_uri("file:///C:/dev/%2").is_err());
        assert!(decode_file_uri("file:///C:/dev/a%2Fb").is_err());
        assert!(decode_file_uri("file:///C:/dev/a?x").is_err());
    }

    #[test]
    fn local_file_uri_bound_escapes_and_utf8_rejected() {
        let long = format!("file:///C:/{}/x", "a".repeat(32 * 1024));
        assert!(decode_file_uri(&long).is_err());
        assert!(decode_file_uri("file:///C:/dev/a%5Cb").is_err());
        assert!(decode_file_uri("file:///C:/dev/a%2520b").is_err());
        assert!(decode_file_uri("file:///C:/dev/a%00b").is_err());
        assert!(decode_file_uri("file:///C:/dev/%FF").is_err());
    }

    #[test]
    fn real_spec_h1_preserves_metadata() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        let text = format!(
            "# 0099 — Title\n\n**Execution repo / tool cwd:** `{}`\n\n## 1. Objective\nbody\n",
            target.display()
        );
        let (path, late) =
            parse_spec_execution_metadata(&text, &record(dir.path(), LayoutProfile::Nested))
                .unwrap()
                .unwrap();
        assert_eq!(path, target.display().to_string());
        assert!(late.is_empty());
    }

    #[test]
    fn metadata_after_objective_ignored() {
        let dir = tempdir().unwrap();
        let text = "**Execution repo:** C:\\dev\\first\n\n## 1. Objective\n\n\
                    **Execution repo:** C:\\dev\\late\n";
        let (path, late) =
            parse_spec_execution_metadata(text, &record(dir.path(), LayoutProfile::Nested))
                .unwrap()
                .unwrap();
        assert_eq!(path, "C:\\dev\\first");
        assert_eq!(late, vec!["execution repo".to_string()]);
    }

    #[test]
    fn late_metadata_ignored_and_reported() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("early");
        std::fs::create_dir_all(&target).unwrap();
        let text = format!(
            "**Execution path:** {}\n## 2. Evidence\n**Execution repo:** C:\\dev\\late\n```\n**Execution repo:** C:\\dev\\fenced\n```\n",
            target.display()
        );
        let (path, late) =
            parse_spec_execution_metadata(&text, &record(dir.path(), LayoutProfile::Nested))
                .unwrap()
                .unwrap();
        assert_eq!(path, target.display().to_string());
        assert_eq!(late, vec!["execution repo".to_string()]);
    }

    #[test]
    fn spec_metadata_conflicts_fail() {
        let dir = tempdir().unwrap();
        let text = "**Execution repo:** C:\\dev\\one\n**Execution path:** C:\\dev\\two\n";
        assert!(
            parse_spec_execution_metadata(text, &record(dir.path(), LayoutProfile::Nested))
                .is_err()
        );
    }

    #[test]
    fn exact_0081_multi_target_error_names_candidates() {
        let dir = tempdir().unwrap();
        let rec = record(dir.path(), LayoutProfile::Nested);
        let cell = "`C:\\dev\\coordinator` and `C:\\dev\\coordinated`";
        let err = parse_execution_cell(cell, &rec).unwrap_err();
        assert!(err.contains(r"C:\dev\coordinator"), "{err}");
        assert!(err.contains(r"C:\dev\coordinated"), "{err}");
        assert!(err.contains("declare one execution target"), "{err}");
    }

    #[test]
    fn coordinated_0002_multi_target_names_candidates() {
        let dir = tempdir().unwrap();
        let rec = record(dir.path(), LayoutProfile::Nested);
        let err = parse_execution_cell("`C:\\dev\\ledgerful` / `C:\\dev\\ledgerful-action`", &rec)
            .unwrap_err();
        assert!(err.contains("ledgerful-action"), "{err}");
        assert!(err.contains("declare one execution target"), "{err}");
    }

    #[test]
    fn ambiguous_or_missing_track_fails() {
        let dir = tempdir().unwrap();
        let rec = record(dir.path(), LayoutProfile::SingleRoot);
        registry(dir.path(), Some("."));
        let mut state = running("0099-Example");
        state.track_id = Some("0404-Missing".into());
        assert!(resolve_track_target(&rec, &state, PHASE_IMPLEMENT).is_err());
        state.track_id = None;
        assert!(resolve_track_target(&rec, &state, PHASE_IMPLEMENT).is_err());
    }

    #[test]
    fn planning_marker_cannot_exempt_product() {
        let dir = tempdir().unwrap();
        let product = product_repo(dir.path());
        let mut rec = record(dir.path(), LayoutProfile::Nested);
        rec.execution_repo = Some(product.clone());
        registry(
            dir.path(),
            Some(&format!("planning-only (`{}`)", product.display())),
        );
        let state = running("0099-Example");
        let got = resolve_track_target(&rec, &state, PHASE_IMPLEMENT).unwrap();
        assert!(!got.planning_only);
    }

    #[test]
    fn current_record_only_planning_predicate() {
        let dir = tempdir().unwrap();
        let product = product_repo(dir.path());
        let mut rec = record(dir.path(), LayoutProfile::Nested);
        rec.execution_repo = Some(product);
        assert!(is_planning_only(&rec, dir.path()));
        assert!(!is_planning_only(
            &rec,
            &rec.execution_repo.clone().unwrap()
        ));
    }

    #[test]
    fn prompt_names_epoch_subdir_and_sampler_uses_toplevel() {
        let dir = tempdir().unwrap();
        let repo = product_repo(dir.path());
        let sub = repo.join("crate");
        std::fs::create_dir_all(&sub).unwrap();
        let mut rec = record(dir.path(), LayoutProfile::Nested);
        rec.execution_repo = Some(repo.clone());
        registry(dir.path(), Some(&sub.display().to_string()));
        let state = running("0099-Example");
        let got = resolve_track_target(&rec, &state, PHASE_IMPLEMENT).unwrap();
        assert_eq!(
            got.prompt_path,
            PathBuf::from(dunce::simplified(&dunce::canonicalize(&sub).unwrap()))
        );
        assert_eq!(got.sample_path, repo);
    }

    #[test]
    fn isolation_on_external_sibling_subdir_stays_direct() {
        let dir = tempdir().unwrap();
        let primary = product_repo(&dir.path().join("primary"));
        let sibling = product_repo(&dir.path().join("sibling"));
        let sub = sibling.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let mut rec = record(dir.path(), LayoutProfile::MultiSibling);
        rec.execution_repo = Some(primary);
        rec.execution_repos.insert("sib".into(), sibling.clone());
        rec.worktree_isolation = true;
        rec.state_dir = Some(dir.path().join("state"));
        registry(dir.path(), Some(&sub.display().to_string()));
        let state = running("0099-Example");
        let got = resolve_track_target(&rec, &state, PHASE_IMPLEMENT).unwrap();
        assert_eq!(
            got.prompt_path,
            PathBuf::from(dunce::simplified(&dunce::canonicalize(&sub).unwrap()))
        );
        assert_eq!(got.sample_path, sibling);
        assert!(!dir.path().join("state").join("worktrees").exists());
    }

    #[test]
    fn missing_or_foreign_epoch_fails_before_inject() {
        let dir = tempdir().unwrap();
        let repo = product_repo(&dir.path().join("primary"));
        let mut rec = record(dir.path(), LayoutProfile::Nested);
        rec.execution_repo = Some(repo.clone());
        rec.worktree_isolation = true;
        rec.state_dir = Some(dir.path().join("state"));
        registry(dir.path(), Some(&repo.display().to_string()));
        let state = running("0099-Example");
        let err = resolve_track_target(&rec, &state, PHASE_IMPLEMENT).unwrap_err();
        assert!(err.reason.contains("epoch 1 worktree is missing"), "{err}");

        // A worktree for a different repository is also refused.
        let epoch = crate::worktree::epoch_dir(&rec, state.run_epoch).unwrap();
        std::fs::create_dir_all(epoch.parent().unwrap()).unwrap();
        let foreign = product_repo(&dir.path().join("foreign"));
        let out = crate::git_command::command()
            .args([
                "worktree",
                "add",
                "--detach",
                epoch.to_str().unwrap(),
                "HEAD",
            ])
            .current_dir(&foreign)
            .output()
            .unwrap();
        assert!(out.status.success());
        let err = resolve_track_target(&rec, &state, PHASE_IMPLEMENT).unwrap_err();
        assert!(err.reason.contains("different Git repository"), "{err}");
    }

    #[test]
    fn resolution_failure_recovery() {
        let dir = tempdir().unwrap();
        let repo = product_repo(&dir.path().join("product"));
        let mut rec = record(dir.path(), LayoutProfile::Nested);
        rec.execution_repo = Some(repo.clone());
        let conductor = dir.path().join("conductor");
        std::fs::create_dir_all(conductor.join("0099-Example")).unwrap();
        std::fs::write(
            conductor.join("conductor.md"),
            "| Track | Execution | Status | Summary |\n| --- | --- | --- | --- |\n| [0099-Example](0099-Example/spec.md) | `not-a-target` | **Ready - not started** | s |\n",
        )
        .unwrap();
        let state = running("0099-Example");
        assert!(resolve_track_target(&rec, &state, PHASE_IMPLEMENT).is_err());
        std::fs::write(
            conductor.join("conductor.md"),
            format!(
                "| Track | Execution | Status | Summary |\n| --- | --- | --- | --- |\n| [0099-Example](0099-Example/spec.md) | `{}` | **Ready - not started** | s |\n",
                repo.display()
            ),
        )
        .unwrap();
        let got = resolve_track_target(&rec, &state, PHASE_IMPLEMENT).unwrap();
        assert_eq!(got.declared_path, repo);
    }

    #[test]
    fn explicit_single_root_target_override() {
        let dir = tempdir().unwrap();
        let target = product_repo(&dir.path().join("target"));
        let rec = record(dir.path(), LayoutProfile::SingleRoot);
        registry(dir.path(), Some(&target.display().to_string()));
        let state = running("0099-Example");
        let got = resolve_track_target(&rec, &state, PHASE_IMPLEMENT).unwrap();
        assert_eq!(got.declared_path, target);
        assert!(!got.planning_only);
    }

    fn write_row(root: &Path, repo: &Path) {
        let conductor = root.join("conductor");
        std::fs::create_dir_all(&conductor).unwrap();
        std::fs::write(
            conductor.join("conductor.md"),
            format!(
                "| Track | Execution | Status | Summary |\n| --- | --- | --- | --- |\n| [0099-Example](0099-Example/spec.md) | `{}` | **Ready - not started** | s |\n",
                repo.display()
            ),
        )
        .unwrap();
    }

    fn stateful_record(root: &Path, primary: &Path, sibling: &Path) -> ProjectRecord {
        let mut rec = record(root, LayoutProfile::Nested);
        rec.execution_repo = Some(primary.to_path_buf());
        rec.execution_repos
            .insert("sibling".into(), sibling.to_path_buf());
        rec.state_dir = Some(root.join("state"));
        crate::state::ensure_state_dir(&rec).unwrap();
        rec
    }

    #[test]
    fn uses_delivery_target_phase_table() {
        for phase in [
            PHASE_IMPLEMENT,
            PHASE_ADDRESS_FINDINGS,
            PHASE_ADDRESS_CI,
            PHASE_ADDRESS_REVIEW,
            PHASE_CROSS_MODEL,
            PHASE_CI_WAIT,
        ] {
            assert!(uses_delivery_target(phase), "{phase}");
        }
        assert!(prepares_target(PHASE_ADDRESS_CI));
        assert!(prepares_target(PHASE_ADDRESS_REVIEW));
        for phase in [
            crate::workflow::graph::PHASE_PLAN,
            crate::workflow::graph::PHASE_FOLD,
        ] {
            assert!(!uses_delivery_target(phase), "{phase}");
        }
    }

    #[test]
    fn registered_predicate_covers_primary_bindings_and_epoch() {
        let dir = tempdir().unwrap();
        let primary = product_repo(&dir.path().join("primary"));
        let sibling = product_repo(&dir.path().join("sibling"));
        let mut rec = stateful_record(dir.path(), &primary, &sibling);
        rec.worktree_isolation = false;
        let state = running("0099-Example");
        let primary_target =
            seeded_delivery_target(&rec, "0099-Example", 1, PHASE_IMPLEMENT, &primary);
        let sibling_target =
            seeded_delivery_target(&rec, "0099-Example", 1, PHASE_IMPLEMENT, &sibling);
        let outside = product_repo(&dir.path().join("outside"));
        let outside_target =
            seeded_delivery_target(&rec, "0099-Example", 1, PHASE_IMPLEMENT, &outside);
        assert!(registered_execution_target(&rec, &primary_target));
        assert!(registered_execution_target(&rec, &sibling_target));
        assert!(!registered_execution_target(&rec, &outside_target));
        let subdir = primary.join("sub");
        std::fs::create_dir_all(&subdir).unwrap();
        let subdir_target =
            seeded_delivery_target(&rec, "0099-Example", 1, PHASE_IMPLEMENT, &subdir);
        assert!(registered_execution_target(&rec, &subdir_target));
        let _ = state;
    }

    #[test]
    fn delivery_target_is_frozen_for_the_leg() {
        let dir = tempdir().unwrap();
        let primary = product_repo(&dir.path().join("primary"));
        let sibling = product_repo(&dir.path().join("sibling"));
        let rec = stateful_record(dir.path(), &primary, &sibling);
        let mut state = running("0099-Example");
        let frozen = seeded_delivery_target(&rec, "0099-Example", 1, PHASE_IMPLEMENT, &primary);
        state.delivery_target = Some(frozen.clone());
        // The row now names the sibling; a frozen local gate must not re-read it.
        write_row(dir.path(), &sibling);
        let gate = gate_target(&rec, &state).unwrap();
        assert_eq!(gate.provenance, TargetProvenance::Frozen);
        assert_eq!(gate.execution.declared_path, primary);
        assert!(gate.registered);
    }

    #[test]
    fn legacy_state_without_target_reresolves_and_validates() {
        let dir = tempdir().unwrap();
        let primary = product_repo(&dir.path().join("primary"));
        let sibling = product_repo(&dir.path().join("sibling"));
        let rec = stateful_record(dir.path(), &primary, &sibling);
        write_row(dir.path(), &sibling);
        let mut state = running("0099-Example");
        state.delivery_target = None;
        let gate = gate_target(&rec, &state).unwrap();
        assert_eq!(gate.provenance, TargetProvenance::ReResolved);
        assert_eq!(gate.execution.declared_path, sibling);
        assert_eq!(gate.execution.project_id, rec.id);
        assert_eq!(gate.execution.run_epoch, state.run_epoch);
    }

    #[test]
    fn status_reports_the_delivery_target() {
        let dir = tempdir().unwrap();
        let primary = product_repo(&dir.path().join("primary"));
        let sibling = product_repo(&dir.path().join("sibling"));
        let rec = stateful_record(dir.path(), &primary, &sibling);
        let mut state = running("0099-Example");
        let target = seeded_delivery_target(&rec, "0099-Example", 1, PHASE_CROSS_MODEL, &sibling);
        state.phase = PHASE_CROSS_MODEL.into();
        state.delivery_target = Some(target.clone());
        let view = crate::state::StatusView::from_record(&rec, &state);
        assert_eq!(view.delivery_target, Some(target));
        let json = serde_json::to_value(&view).unwrap();
        assert!(json.get("delivery_target").is_some());
    }

    #[test]
    fn stale_target_does_not_mark_the_newer_run() {
        let dir = tempdir().unwrap();
        let primary = product_repo(&dir.path().join("primary"));
        let sibling = product_repo(&dir.path().join("sibling"));
        let rec = stateful_record(dir.path(), &primary, &sibling);
        let mut state = running("0099-Example");
        state.run_epoch = 2;
        crate::state::save_run_state(&rec, &state).unwrap();
        let stale = seeded_delivery_target(&rec, "0099-Example", 1, PHASE_IMPLEMENT, &primary);
        crate::workflow::mark_driven_with_target(&rec, PHASE_IMPLEMENT, Some(&stale)).unwrap();
        let saved = crate::state::load_run_state(&rec).unwrap();
        assert!(saved.last_driven_phase.is_none());
        assert!(saved.delivery_target.is_none());
        assert!(
            std::fs::read_to_string(crate::progress_log::path(&rec))
                .unwrap_or_default()
                .contains("stale target skipped")
        );
    }

    #[test]
    fn mutation_target_accepts_the_frozen_and_latched_repo() {
        let dir = tempdir().unwrap();
        let primary = product_repo(&dir.path().join("primary"));
        let sibling = product_repo(&dir.path().join("sibling"));
        let rec = stateful_record(dir.path(), &primary, &sibling);
        write_row(dir.path(), &sibling);
        let mut state = running("0099-Example");
        let frozen = seeded_delivery_target(&rec, "0099-Example", 1, PHASE_IMPLEMENT, &sibling);
        state.delivery_target = Some(frozen);
        let got = mutation_target(&rec, &state, Some(&sibling)).unwrap();
        assert_eq!(got.provenance, TargetProvenance::ReResolved);
        assert!(got.registered);
    }

    #[test]
    fn mutation_target_accepts_a_legacy_resume_without_a_frozen_target() {
        let dir = tempdir().unwrap();
        let primary = product_repo(&dir.path().join("primary"));
        let sibling = product_repo(&dir.path().join("sibling"));
        let rec = stateful_record(dir.path(), &primary, &sibling);
        write_row(dir.path(), &sibling);
        let mut state = running("0099-Example");
        state.phase = PHASE_CI_WAIT.into();
        state.delivery_target = None;
        let got = mutation_target(&rec, &state, Some(&sibling)).unwrap();
        assert!(got.registered);
    }

    #[test]
    fn mutation_target_refuses_a_changed_row() {
        let dir = tempdir().unwrap();
        let primary = product_repo(&dir.path().join("primary"));
        let sibling = product_repo(&dir.path().join("sibling"));
        let rec = stateful_record(dir.path(), &primary, &sibling);
        let mut state = running("0099-Example");
        let frozen = seeded_delivery_target(&rec, "0099-Example", 1, PHASE_IMPLEMENT, &primary);
        state.delivery_target = Some(frozen);
        write_row(dir.path(), &sibling);
        let err = mutation_target(&rec, &state, Some(&primary)).unwrap_err();
        assert!(matches!(err, MutationRefusal::Changed { .. }), "{err}");
    }

    #[test]
    fn isolation_epoch_is_the_primary_delivery_target() {
        let dir = tempdir().unwrap();
        let primary = product_repo(&dir.path().join("primary"));
        let mut rec = record(dir.path(), LayoutProfile::Nested);
        rec.execution_repo = Some(primary.clone());
        rec.worktree_isolation = true;
        rec.state_dir = Some(dir.path().join("state"));
        crate::state::ensure_state_dir(&rec).unwrap();
        registry(dir.path(), Some(&primary.display().to_string()));
        let mut state = running("0099-Example");
        state.run_epoch = 1;
        crate::worktree::prepare_epoch(&rec, 1).unwrap();
        let epoch = crate::worktree::epoch_dir(&rec, 1).unwrap();
        let gate = gate_target(&rec, &state).unwrap();
        assert!(paths_same(&gate.execution.prompt_path, &epoch));
        assert!(paths_same(&gate.execution.sample_path, &epoch));
        state.delivery_target = Some(gate.execution.clone());
        assert_eq!(
            gate_target(&rec, &state).unwrap().provenance,
            TargetProvenance::Frozen
        );
    }

    #[test]
    fn gate_target_journal_is_bounded_by_the_memo() {
        let dir = tempdir().unwrap();
        let primary = product_repo(&dir.path().join("primary"));
        let sibling = product_repo(&dir.path().join("sibling"));
        let rec = stateful_record(dir.path(), &primary, &sibling);
        write_row(dir.path(), &sibling);
        let state = running("0099-Example");
        let gate = gate_target(&rec, &state).unwrap();
        let mut journal_state = state.clone();
        assert!(journal_gate_target(&rec, &mut journal_state, &gate));
        assert!(!journal_gate_target(&rec, &mut journal_state, &gate));
        let log = std::fs::read_to_string(crate::progress_log::path(&rec)).unwrap_or_default();
        assert_eq!(log.matches("gate-target:").count(), 1, "{log}");
        // A frozen target never journals.
        let mut frozen_state = state.clone();
        frozen_state.delivery_target = Some(gate.execution.clone());
        let frozen = gate_target(&rec, &frozen_state).unwrap();
        assert!(!journal_gate_target(&rec, &mut frozen_state, &frozen));
    }

    #[test]
    fn same_delivery_repo_ignores_phase() {
        let dir = tempdir().unwrap();
        let repo = product_repo(&dir.path().join("repo"));
        let rec = record(dir.path(), LayoutProfile::Nested);
        let mut a = seeded_delivery_target(&rec, "0099-Example", 1, PHASE_IMPLEMENT, &repo);
        let b = seeded_delivery_target(&rec, "0099-Example", 1, PHASE_ADDRESS_CI, &repo);
        assert!(same_delivery_repo(&a, &b));
        a.declared_path = dir.path().join("other");
        assert!(!same_delivery_repo(&a, &b));
    }
}

#[cfg(test)]
mod review_wiring_tests {
    use super::*;
    use crate::workflow::block;
    use crate::workflow::graph;

    fn record(path: &Path) -> ProjectRecord {
        ProjectRecord {
            id: "proj-0078".into(),
            path: path.to_path_buf(),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: Some(path.to_path_buf()),
            execution_repos: Default::default(),
            state_dir: None,
            auto_merge: true,
            phase_timeouts_secs: Default::default(),
            notify_progress: false,
            worktree_isolation: false,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            ci_fix_routing: false,
            review_comment_routing: false,
            skill_aliases: Default::default(),
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn address_review_is_a_prepared_side_loop() {
        assert!(graph::is_canonical(graph::PHASE_ADDRESS_REVIEW));
        assert_eq!(
            graph::successor(graph::PHASE_ADDRESS_REVIEW),
            Some(graph::PHASE_CI_WAIT)
        );
        assert!(graph::is_grok_bound(graph::PHASE_ADDRESS_REVIEW));
        assert!(!graph::canonical_phases().contains(&graph::PHASE_ADDRESS_REVIEW));
        assert_eq!(
            crate::harness::roles::phase_role_key(graph::PHASE_ADDRESS_REVIEW),
            Some(graph::ROLE_IMPLEMENTOR)
        );
        assert!(block::phase_allowed(graph::PHASE_ADDRESS_REVIEW));
        assert_eq!(
            crate::workflow::timeouts::default_timeout_secs(graph::PHASE_ADDRESS_REVIEW),
            1800
        );
        assert!(prepares_target(graph::PHASE_ADDRESS_REVIEW));
        assert!(uses_delivery_target(graph::PHASE_ADDRESS_REVIEW));
        assert_eq!(graph::REVIEW_FIX_CAP, 2);
    }

    #[test]
    fn address_review_uses_the_prepared_target_route() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = record(dir.path());
        rec.execution_repo = Some(dir.path().to_path_buf());
        let target = seeded_delivery_target(
            &rec,
            "0078",
            1,
            crate::workflow::graph::PHASE_ADDRESS_REVIEW,
            dir.path(),
        );
        assert!(paths_same(&target.sample_path, dir.path()));
        assert_eq!(target.phase, crate::workflow::graph::PHASE_ADDRESS_REVIEW);
        assert!(prepares_target(&target.phase));
    }

    #[test]
    fn address_review_installs_the_target_root() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = record(dir.path());
        rec.execution_repo = Some(dir.path().to_path_buf());
        let target = seeded_delivery_target(
            &rec,
            "0078",
            1,
            crate::workflow::graph::PHASE_ADDRESS_REVIEW,
            dir.path(),
        );
        assert!(paths_same(&target.prompt_path, dir.path()));
        assert!(paths_same(&target.sample_path, dir.path()));
        assert!(!target.planning_only);
    }
}
