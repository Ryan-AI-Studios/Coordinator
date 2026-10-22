//! Shared Control Plane operations for CLI and HTTP (no divergent logic).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{self, registry_path, resolve_scan_roots};
use crate::error::{CoordinatorError, Result};
use crate::layout::{self, LayoutProfile, WorkspacePaths, nested_execution_null_hint};
use crate::outcome::{
    self, FailureClass, OutcomeSource, OutcomeStatus, PhaseOutcome, load_current_outcome,
    parse_outcome_status, write_and_apply,
};
use crate::registry::{ProjectAddOptions, ProjectRecord, ProjectSetOptions, Registry};
use crate::run;
use crate::scan::{self, ScanCandidate};
use crate::state::StatusView;
use crate::watch;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectAddRequest {
    pub path: String,
    #[serde(default)]
    pub layout_profile: Option<String>,
    #[serde(default)]
    pub execution_repo: Option<String>,
    #[serde(default)]
    pub conductor_dir: Option<String>,
    #[serde(default)]
    pub state_dir: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub execution_repo_name: Option<String>,
    #[serde(default)]
    pub execution_repos: Option<BTreeMap<String, PathBuf>>,
    #[serde(default)]
    pub auto_merge: Option<bool>,
    #[serde(default)]
    pub phase_timeouts_secs: Option<BTreeMap<String, u64>>,
    #[serde(default)]
    pub auto_start: Option<crate::registry::AutoStartPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSetRequest {
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub layout_profile: Option<String>,
    #[serde(default)]
    pub execution_repo: Option<String>,
    #[serde(default)]
    pub conductor_dir: Option<String>,
    #[serde(default)]
    pub state_dir: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub execution_repos: Option<BTreeMap<String, PathBuf>>,
    #[serde(default)]
    pub execution_repo_name: Option<String>,
    #[serde(default)]
    pub auto_merge: Option<bool>,
    #[serde(default)]
    pub notify_progress: Option<bool>,
    #[serde(default)]
    pub worktree_isolation: Option<bool>,
    #[serde(default)]
    pub self_continuation: Option<bool>,
    #[serde(default)]
    pub ci_fix_routing: Option<bool>,
    #[serde(default)]
    pub phase_timeouts_secs: Option<BTreeMap<String, u64>>,
    #[serde(default)]
    pub clear_phase_timeouts: Option<bool>,
    #[serde(default)]
    pub clear_phase_timeout: Option<Vec<String>>,
    #[serde(default)]
    pub ready_aliases: Option<Vec<String>>,
    #[serde(default)]
    pub clear_ready_aliases: Option<bool>,
    #[serde(default)]
    pub auto_start: Option<crate::registry::AutoStartPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectScanRequest {
    #[serde(default)]
    pub roots: Vec<PathBuf>,
    #[serde(default)]
    pub add: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRefBody {
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub track: Option<String>,
    #[serde(default)]
    pub driver: Option<String>,
    /// Skip harness preflight on adapter `run`. Serde default false.
    #[serde(default)]
    pub skip_preflight: bool,
}

/// Effective phase wall clock for `project show`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PhaseTimeoutView {
    pub secs: u64,
    pub source: String,
}

/// Show response: raw record + resolved paths + optional remediation hint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectShowView {
    pub project: ProjectRecord,
    pub resolved: WorkspacePaths,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Effective `{secs, source}` for every canonical phase.
    pub phase_timeouts: BTreeMap<String, PhaseTimeoutView>,
    /// Read-only track execution-target preview (`project show --track <id>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_track: Option<TrackTargetView>,
}

/// Read-only resolved-target preview. No run state mutation and no prompt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrackTargetView {
    pub track: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub planning_only: bool,
    /// 0101: the target is the primary or a registered execution binding.
    #[serde(default)]
    pub registered: bool,
    /// 0101: publish/PR/merge/label are permitted for this target.
    #[serde(default)]
    pub publishable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// POST /v1/restore body. `discard` defaults false. Not added onto [`ProjectRefBody`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreBody {
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub discard: bool,
}

/// POST /v1/outcome body: Phase Outcome fields + optional project selector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutcomeWriteBody {
    #[serde(default)]
    pub project: Option<String>,
    pub version: Option<u32>,
    pub phase: String,
    pub status: OutcomeStatus,
    #[serde(default)]
    pub failure_class: Option<FailureClass>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub written_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub source: Option<OutcomeSource>,
    #[serde(default)]
    pub metadata: Option<outcome::OutcomeMetadata>,
    #[serde(default)]
    pub run_epoch: Option<u64>,
    /// Convenience: set metadata.next_track without full metadata object.
    #[serde(default)]
    pub next_track: Option<String>,
}

/// Load registry from machine home.
pub fn load_registry() -> Result<Registry> {
    let path = registry_path()?;
    Registry::load(&path)
}

fn save_registry(reg: &Registry) -> Result<()> {
    let path = registry_path()?;
    reg.save(&path)
}

/// Operator selector: explicit spec wins; else unique cwd (when `infer_cwd`);
/// else last-used; else error. Persist last-used on success when the id changed
/// (best-effort). HTTP callers pass `infer_cwd = false`.
pub fn resolve_selected(project: Option<&str>, infer_cwd: bool) -> Result<ProjectRecord> {
    let last = config::load_last_used_id();
    let cwd = if infer_cwd {
        std::env::current_dir().ok()
    } else {
        None
    };
    let rec = load_registry()?
        .resolve_project_in(project, cwd.as_deref(), last.as_deref())?
        .clone();
    if last.as_deref() != Some(rec.id.as_str()) {
        let _ = config::save_last_used_id(&rec.id);
    }
    Ok(rec)
}

/// Read-only operator selector for `roles prove` (folded m3): same resolution rules as
/// [`resolve_selected`] but it never writes `last-used.json`.
pub fn resolve_selected_readonly(project: Option<&str>, infer_cwd: bool) -> Result<ProjectRecord> {
    let last = config::load_last_used_id();
    let cwd = if infer_cwd {
        std::env::current_dir().ok()
    } else {
        None
    };
    Ok(load_registry()?
        .resolve_project_in(project, cwd.as_deref(), last.as_deref())?
        .clone())
}

fn parse_optional_path(s: Option<String>) -> Option<PathBuf> {
    s.filter(|p| !p.is_empty()).map(PathBuf::from)
}

fn opts_from_add_request(req: &ProjectAddRequest) -> Result<ProjectAddOptions> {
    let layout_profile = match &req.layout_profile {
        Some(s) => LayoutProfile::parse(s)?,
        None => LayoutProfile::Nested,
    };
    Ok(ProjectAddOptions {
        layout_profile,
        execution_repo: parse_optional_path(req.execution_repo.clone()),
        conductor_dir: parse_optional_path(req.conductor_dir.clone()),
        state_dir: parse_optional_path(req.state_dir.clone()),
        display_name: req.display_name.clone(),
        execution_repo_name: req.execution_repo_name.clone(),
        execution_repos: req.execution_repos.clone().unwrap_or_default(),
        auto_merge: req.auto_merge,
        phase_timeouts_secs: req.phase_timeouts_secs.clone().unwrap_or_default(),
        auto_start: req.auto_start,
        skill_aliases: BTreeMap::new(),
        clear_skill_aliases: false,
        clear_skill_alias: Vec::new(),
    })
}

/// `project add <path>` with layout options.
pub fn project_add(path: &Path, opts: ProjectAddOptions) -> Result<ProjectRecord> {
    let mut reg = load_registry()?;
    let rec = reg.add(path, opts)?;
    save_registry(&reg)?;
    Ok(rec)
}

/// HTTP POST /v1/projects
pub fn project_add_request(req: ProjectAddRequest) -> Result<ProjectRecord> {
    let opts = opts_from_add_request(&req)?;
    project_add(Path::new(&req.path), opts)
}

/// `project list`
pub fn project_list() -> Result<Vec<ProjectRecord>> {
    Ok(load_registry()?.list().to_vec())
}

/// `project show` — raw + resolved + nested null hint.
pub fn project_show(project: Option<&str>, infer_cwd: bool) -> Result<ProjectShowView> {
    project_show_with_track(project, None, infer_cwd)
}

/// `project show --track <id>`: adds a read-only resolved-target preview.
pub fn project_show_with_track(
    project: Option<&str>,
    track: Option<&str>,
    infer_cwd: bool,
) -> Result<ProjectShowView> {
    let rec = resolve_selected(project, infer_cwd)?;
    let resolved = layout::resolve(&rec);
    let hint = if rec.layout_profile == LayoutProfile::Nested && rec.execution_repo.is_none() {
        let spec = project
            .map(|s| s.to_string())
            .unwrap_or_else(|| rec.path.display().to_string());
        Some(nested_execution_null_hint(&spec))
    } else {
        None
    };
    let mut phase_timeouts = BTreeMap::new();
    for phase in crate::workflow::graph::all_phase_ids() {
        phase_timeouts.insert(
            (*phase).to_string(),
            PhaseTimeoutView {
                secs: crate::workflow::timeout_for_phase(&rec, phase).as_secs(),
                source: crate::workflow::timeout_source(&rec, phase)
                    .as_str()
                    .to_string(),
            },
        );
    }
    phase_timeouts.insert(
        crate::workflow::timeouts::TIMEOUT_KEY_PLAN_REVIEW_SLOT.to_string(),
        PhaseTimeoutView {
            secs: crate::workflow::timeout_for_phase(
                &rec,
                crate::workflow::timeouts::TIMEOUT_KEY_PLAN_REVIEW_SLOT,
            )
            .as_secs(),
            source: crate::workflow::timeout_source(
                &rec,
                crate::workflow::timeouts::TIMEOUT_KEY_PLAN_REVIEW_SLOT,
            )
            .as_str()
            .to_string(),
        },
    );
    let resolved_track = match track {
        None => None,
        Some(track) => {
            let mut state = crate::state::load_run_state(&rec)?;
            state.track_id = Some(track.to_string());
            state.phase = crate::workflow::graph::PHASE_IMPLEMENT.into();
            let view = match crate::workflow::execution_repo::resolve_track_target(
                &rec,
                &state,
                crate::workflow::graph::PHASE_IMPLEMENT,
            ) {
                Ok(target) => {
                    let registered =
                        crate::workflow::execution_repo::registered_execution_target(&rec, &target);
                    TrackTargetView {
                        track: track.to_string(),
                        declared_path: Some(target.declared_path),
                        prompt_path: Some(target.prompt_path),
                        sample_path: Some(target.sample_path),
                        source: Some(target.source.to_string()),
                        planning_only: target.planning_only,
                        registered,
                        publishable: registered && !target.planning_only,
                        error: None,
                    }
                }
                Err(err) => TrackTargetView {
                    track: track.to_string(),
                    declared_path: None,
                    prompt_path: None,
                    sample_path: None,
                    source: None,
                    planning_only: false,
                    registered: false,
                    publishable: false,
                    error: Some(err.to_string()),
                },
            };
            Some(view)
        }
    };

    Ok(ProjectShowView {
        project: rec,
        resolved,
        hint,
        phase_timeouts,
        resolved_track,
    })
}

/// `project set` — mutate bindings; workspace path immutable.
pub fn project_set(
    project: Option<&str>,
    opts: ProjectSetOptions,
    infer_cwd: bool,
) -> Result<ProjectRecord> {
    let id = resolve_selected(project, infer_cwd)?.id;
    let mut reg = load_registry()?;
    let rec = reg.set(&id, opts)?;
    save_registry(&reg)?;
    Ok(rec)
}

/// HTTP PATCH-style set via body.
pub fn project_set_request(req: ProjectSetRequest) -> Result<ProjectRecord> {
    let layout_profile = match req.layout_profile {
        Some(s) => Some(LayoutProfile::parse(&s)?),
        None => None,
    };
    let opts = ProjectSetOptions {
        layout_profile,
        execution_repo: parse_optional_path(req.execution_repo),
        clear_execution_repo: false,
        conductor_dir: parse_optional_path(req.conductor_dir),
        clear_conductor_dir: false,
        state_dir: parse_optional_path(req.state_dir),
        clear_state_dir: false,
        display_name: req.display_name,
        execution_repos: req.execution_repos,
        execution_repo_name: req.execution_repo_name,
        auto_merge: req.auto_merge,
        notify_progress: req.notify_progress,
        worktree_isolation: req.worktree_isolation,
        self_continuation: req.self_continuation,
        ci_fix_routing: req.ci_fix_routing,
        phase_timeouts_secs: req.phase_timeouts_secs,
        clear_phase_timeouts: req.clear_phase_timeouts.unwrap_or(false),
        clear_phase_timeout: req.clear_phase_timeout.unwrap_or_default(),
        ready_aliases: req.ready_aliases,
        clear_ready_aliases: req.clear_ready_aliases.unwrap_or(false),
        auto_start: req.auto_start,
        skill_aliases: None,
        clear_skill_aliases: false,
        clear_skill_alias: Vec::new(),
    };
    project_set(req.project.as_deref(), opts, false)
}

/// `project scan` — dry-run by default; `--add` registers new candidates.
pub fn project_scan(
    roots: &[PathBuf],
    add: bool,
) -> Result<(Vec<ScanCandidate>, Vec<ProjectRecord>)> {
    let roots = resolve_scan_roots(roots)?;
    if roots.is_empty() {
        return Err(CoordinatorError::Message(
            "no scan roots: pass --root <path> or set scan_roots in config.json".into(),
        ));
    }
    let mut reg = load_registry()?;
    let candidates = scan::scan_roots(&roots, &reg)?;
    let mut added = Vec::new();
    if add {
        for c in &candidates {
            if c.already_registered {
                continue;
            }
            let opts = ProjectAddOptions {
                layout_profile: c.detected_profile,
                execution_repo: c.execution_repo_hint.clone(),
                ..Default::default()
            };
            let rec = reg.add(&c.path, opts)?;
            added.push(rec);
        }
        if !added.is_empty() {
            save_registry(&reg)?;
        }
    }
    // Re-scan registration flags after add for accurate response
    let candidates = scan::scan_roots(&roots, &reg)?;
    Ok((candidates, added))
}

fn attach_operator_view(mut view: StatusView) -> StatusView {
    view.ticker = Some(watch::ticker_view());
    view.features = Some(crate::features::resolve_all());
    view
}

/// Resolve project selector then return status view.
pub fn status(project: Option<&str>, infer_cwd: bool) -> Result<StatusView> {
    let rec = resolve_selected(project, infer_cwd)?;
    let mut view = attach_operator_view(run::status(&rec)?);
    view.registry = Some(crate::registry_store::status_view_for(&rec));
    Ok(view)
}

/// Status for all projects (aggregate). One health probe and one feature
/// snapshot; both cloned onto every view so status and fleet agree.
pub fn status_all() -> Result<Vec<StatusView>> {
    let reg = load_registry()?;
    let ticker = watch::ticker_view();
    let features = crate::features::resolve_all();
    let mut out = Vec::with_capacity(reg.projects.len());
    for rec in reg.list() {
        let mut view = run::status(rec)?;
        view.ticker = Some(ticker.clone());
        view.features = Some(features.clone());
        out.push(view);
    }
    Ok(out)
}

pub fn cmd_run(
    project: Option<&str>,
    track: Option<String>,
    driver: Option<&str>,
    skip_preflight: bool,
    infer_cwd: bool,
) -> Result<StatusView> {
    let rec = resolve_selected(project, infer_cwd)?;
    let _ = settle_if_conductor_completed(&rec);
    // 0098: the durable Blocked hold refuses every fresh run (any driver / track)
    // before driver resolution, `gh` omit-pick and machine preflight.
    let state = crate::state::load_run_state(&rec)?;
    if let Some(err) = crate::run::blocked_hold(&rec, &state) {
        return Err(err);
    }
    let driver = crate::workflow::resolve_driver(driver)?;
    let (track, picked) = resolve_run_track_for_driver(&rec, track, driver)?;
    if driver == crate::workflow::WorkflowDriver::Adapter && !skip_preflight {
        let report = crate::harness::preflight::probe_machine()?;
        if !report.ok {
            return Err(CoordinatorError::Preflight {
                report: Box::new(report),
            });
        }
    }
    run::run_with_origin(&rec, track, driver, picked)
}

/// Explicit nonempty `--track` wins (no Ready check). Else pick when
/// [`crate::workflow::should_pick_next_ready`]; otherwise retain (`None`).
/// Probeless (Stub / FileWait / existing tests).
pub fn resolve_run_track(
    record: &crate::registry::ProjectRecord,
    explicit: Option<String>,
) -> Result<(Option<String>, bool)> {
    resolve_run_track_with_probe(record, explicit, None)
}

/// Shipped retain that must not come back as `Ok((None, false))`.
///
/// Local terminal state and `track_is_shipped_local` refuse without `gh`.
/// A probe `Err` fail-opens (same as the picker). Explicit `--track` never
/// reaches here.
pub(crate) fn refused_retain_id(
    record: &crate::registry::ProjectRecord,
    state: &crate::state::RunState,
    probe: Option<&dyn crate::workflow::MergedTrackProbe>,
) -> Option<String> {
    let id = state
        .track_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let local = crate::workflow::track_is_shipped_local(record, state, id);
    let rows = crate::workflow::conductor_md::load_track_rows(record);
    let terminal = rows
        .as_ref()
        .is_some_and(|rows| crate::workflow::conductor_md::track_row_terminal(rows, id));
    if local || terminal {
        return Some(id.to_string());
    }
    // 0109: a store that exists but cannot be read is not "row absent". Refuse the
    // retain instead of failing open onto a stale or hand-added row.
    if rows.is_none() && crate::registry_store::store_path_for(record).is_file() {
        return Some(id.to_string());
    }
    let merged = probe.is_some_and(|probe| {
        let Some(cwd) = crate::layout::resolve(record).execution_repo else {
            return false;
        };
        let Some(nid) = crate::notify::artifact::numeric_track_id(id) else {
            return false;
        };
        probe
            .merged_pr_for_track(&cwd, nid)
            .ok()
            .flatten()
            .is_some()
    });
    if merged { Some(id.to_string()) } else { None }
}

/// Adapter omit-pick: optional merged-PR probe. Explicit `--track` still wins.
pub fn resolve_run_track_with_probe(
    record: &crate::registry::ProjectRecord,
    explicit: Option<String>,
    probe: Option<&dyn crate::workflow::MergedTrackProbe>,
) -> Result<(Option<String>, bool)> {
    let trimmed = explicit
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(t) = trimmed {
        return Ok((Some(t), false));
    }
    let state = crate::state::load_run_state(record)?;
    let mut pick = crate::workflow::ReadyPickState::from(&state);
    pick.auto_start = record.auto_start;
    if record.auto_start == crate::registry::AutoStartPolicy::Never {
        let mut no_policy = pick.clone();
        no_policy.auto_start = crate::registry::AutoStartPolicy::Hitl;
        if crate::workflow::should_pick_next_ready(&no_policy) {
            return Err(CoordinatorError::Message(
                "auto_start=never; pass --track".into(),
            ));
        }
    }
    let mut refuse_skip: Vec<String> = Vec::new();
    if !crate::workflow::should_pick_next_ready(&pick) {
        match refused_retain_id(record, &state, probe) {
            Some(id) => {
                if record.auto_start == crate::registry::AutoStartPolicy::Never {
                    return Err(CoordinatorError::Message(
                        "auto_start=never; pass --track".into(),
                    ));
                }
                crate::progress_log::append(
                    record,
                    "start",
                    &format!("omit-pick: refuse shipped {id}"),
                );
                refuse_skip.push(id);
            }
            None => return Ok((None, false)),
        }
    }
    let id = crate::workflow::pick_next_ready_excluding_sticky(
        record,
        &refuse_skip,
        probe,
        &state.sticky_ready_ids,
    )?;
    if let Some(file) = crate::workflow::conductor_md::overlay_file_status(record, &id) {
        crate::progress_log::append(record, "start", &format!("sticky Ready {id} (file={file})"));
    }
    Ok((Some(id), true))
}

fn resolve_run_track_for_driver(
    record: &crate::registry::ProjectRecord,
    explicit: Option<String>,
    driver: crate::workflow::WorkflowDriver,
) -> Result<(Option<String>, bool)> {
    if driver != crate::workflow::WorkflowDriver::Adapter {
        return resolve_run_track(record, explicit);
    }
    adapter_omit_pick(record, explicit)
}

#[cfg(test)]
fn adapter_omit_pick(
    record: &crate::registry::ProjectRecord,
    explicit: Option<String>,
) -> Result<(Option<String>, bool)> {
    let hooked = crate::workflow::shipped::installed_merged_probe();
    resolve_run_track_with_probe(record, explicit, hooked.as_deref())
}

#[cfg(not(test))]
fn adapter_omit_pick(
    record: &crate::registry::ProjectRecord,
    explicit: Option<String>,
) -> Result<(Option<String>, bool)> {
    let probe = crate::ci::GhMergedTrackProbe::default();
    resolve_run_track_with_probe(record, explicit, Some(&probe))
}

/// Probe machine Role Bindings + `gh`. Optional `--project` only validates the selector.
pub fn cmd_doctor(project: Option<&str>) -> Result<crate::harness::preflight::DoctorReport> {
    if let Some(p) = project {
        let _ = load_registry()?.resolve_project(Some(p))?;
    }
    crate::harness::preflight::probe_machine()
}

pub fn cmd_pause(project: Option<&str>, infer_cwd: bool) -> Result<StatusView> {
    let rec = resolve_selected(project, infer_cwd)?;
    run::pause(&rec)
}

pub fn cmd_resume(project: Option<&str>, infer_cwd: bool) -> Result<StatusView> {
    let rec = resolve_selected(project, infer_cwd)?;
    run::resume(&rec)
}

pub fn cmd_stop(project: Option<&str>, infer_cwd: bool) -> Result<StatusView> {
    let rec = resolve_selected(project, infer_cwd)?;
    run::stop(&rec)
}

/// CLI and `POST /v1/restore`. Project omit matches `stop` (`infer_cwd` is false for HTTP).
pub fn cmd_restore(project: Option<&str>, discard: bool, infer_cwd: bool) -> Result<StatusView> {
    let rec = resolve_selected(project, infer_cwd)?;
    crate::checkpoint::restore(&rec, discard)
}

/// CLI/HTTP: write Phase Outcome and apply via the single apply path.
#[allow(clippy::too_many_arguments)]
pub fn cmd_outcome_write(
    project: Option<&str>,
    phase: String,
    status: &str,
    failure_class: Option<&str>,
    message: Option<String>,
    next_track: Option<String>,
    source: Option<&str>,
    infer_cwd: bool,
) -> Result<StatusView> {
    let rec = resolve_selected(project, infer_cwd)?;
    let status = parse_outcome_status(status)?;
    let source = match source {
        Some(s) => OutcomeSource::parse(s)?,
        None => OutcomeSource::Cli,
    };
    let failure_class = match failure_class {
        Some(s) => Some(FailureClass::parse(s)?),
        None => None,
    };
    let outcome = PhaseOutcome {
        version: outcome::OUTCOME_VERSION,
        phase,
        status,
        failure_class,
        message,
        written_at: chrono::Utc::now(),
        source,
        metadata: next_track.map(|t| outcome::OutcomeMetadata {
            next_track: Some(t),
            ..Default::default()
        }),
        run_epoch: None,
    };
    outcome.validate()?;
    write_and_apply(&rec, outcome)
}

/// Build outcome from HTTP body and apply.
pub fn cmd_outcome_post(body: OutcomeWriteBody) -> Result<StatusView> {
    let rec = resolve_selected(body.project.as_deref(), false)?;
    let mut metadata = body.metadata.unwrap_or_default();
    if let Some(nt) = body.next_track {
        metadata.next_track = Some(nt);
    }
    let metadata = if metadata.next_track.is_none() && metadata.role.is_none() {
        None
    } else {
        Some(metadata)
    };
    let outcome = PhaseOutcome {
        version: body.version.unwrap_or(outcome::OUTCOME_VERSION),
        phase: body.phase,
        status: body.status,
        failure_class: body.failure_class,
        message: body.message,
        written_at: body.written_at.unwrap_or_else(chrono::Utc::now),
        source: body.source.unwrap_or(OutcomeSource::Http),
        metadata,
        run_epoch: body.run_epoch,
    };
    write_and_apply(&rec, outcome)
}

/// Show current.json if present (CLI / GET).
pub fn cmd_outcome_show(project: Option<&str>, infer_cwd: bool) -> Result<Option<PhaseOutcome>> {
    let rec = resolve_selected(project, infer_cwd)?;
    load_current_outcome(&rec)
}

/// Show `{state_dir}/FAILURE.md` if present (CLI / GET /v1/failure).
pub fn cmd_failure_show(
    project: Option<&str>,
    infer_cwd: bool,
) -> Result<Option<crate::notify::FailureShow>> {
    let rec = resolve_selected(project, infer_cwd)?;
    let mut shown = crate::notify::artifact::read(&rec)?;
    if let Some(ref mut s) = shown
        && let Ok(state) = crate::state::load_run_state(&rec)
    {
        s.superseded =
            crate::workflow::conductor_md::failure_superseded(&rec, &state, Some(&s.body));
    }
    Ok(shown)
}

/// Operator ack: delete FAILURE.md and drop `failure_class` (Idle/Stopped only).
pub fn cmd_failure_resolve(project: Option<&str>, infer_cwd: bool) -> Result<StatusView> {
    let rec = resolve_selected(project, infer_cwd)?;
    crate::state::with_run_state_lock(&rec, || {
        let mut state = crate::state::load_run_state(&rec)?;
        if matches!(
            state.status,
            crate::state::RunStatus::Running | crate::state::RunStatus::Paused
        ) {
            return Err(CoordinatorError::InvalidTransition {
                action: "failure resolve",
                from: state.status.to_string(),
            });
        }
        // 0098: `cmd_failure_resolve` is the only Blocked release. Persist the cleared
        // class and last_event BEFORE the artifact is removed, so a failed save leaves
        // the on-disk hold and FAILURE.md unchanged. A retry then succeeds.
        if state.failure_class == Some(crate::outcome::FailureClass::Blocked) {
            state.failure_class = None;
            state.last_event = crate::notify::LAST_EVENT_FAILURE_RESOLVED.into();
            crate::state::save_run_state(&rec, &state)?;
        }
        if !crate::notify::settle_failure(
            &rec,
            &mut state,
            crate::notify::LAST_EVENT_FAILURE_RESOLVED,
            true,
        ) {
            state.last_event = crate::notify::LAST_EVENT_FAILURE_RESOLVED.into();
            crate::progress_log::append(
                &rec,
                "resolve",
                crate::notify::LAST_EVENT_FAILURE_RESOLVED,
            );
        }
        crate::state::save_run_state(&rec, &state)?;
        Ok(StatusView::from_record(&rec, &state))
    })
}

/// Idle/Stopped + conductor row Completed → settle stored failure (0045).
pub(crate) fn settle_if_conductor_completed(
    record: &crate::registry::ProjectRecord,
) -> Result<bool> {
    crate::state::with_run_state_lock(record, || {
        let mut state = crate::state::load_run_state(record)?;
        if !matches!(
            state.status,
            crate::state::RunStatus::Idle | crate::state::RunStatus::Stopped
        ) {
            return Ok(false);
        }
        // 0098: a Blocked hold is released only by `failure resolve`, never by a
        // conductor-row edit, including the held track's Completed row.
        if state.failure_class == Some(crate::outcome::FailureClass::Blocked) {
            return Ok(false);
        }
        let body = crate::notify::artifact::read(record)?.map(|s| s.body);
        let reason =
            crate::workflow::conductor_md::failure_superseded(record, &state, body.as_deref());
        if reason.as_deref() != Some(crate::notify::SUPERSEDED_COMPLETED) {
            return Ok(false);
        }
        let changed =
            crate::notify::settle_failure(record, &mut state, crate::notify::SETTLED_DETAIL, true);
        if changed {
            crate::state::save_run_state(record, &state)?;
        }
        Ok(changed)
    })
}

/// Block until outcome applied or wait budget expires.
pub fn cmd_wait(project: Option<&str>, timeout_secs: u64, infer_cwd: bool) -> Result<StatusView> {
    let rec = resolve_selected(project, infer_cwd)?;
    watch::wait_for_outcome(&rec, Some(timeout_secs))
}

pub use crate::watch::ServeProbe;

/// CLI-only `run` options. HTTP / UI / [`cmd_run`] stay write-only.
#[derive(Debug, Clone)]
pub struct RunCliOpts {
    pub detach: bool,
    pub timeout_secs: Option<u64>,
    pub probe: ServeProbe,
    pub skip_preflight: bool,
}

/// Start a run, then tick until Idle/Stopped unless detached or serve owns the loop.
pub fn cmd_run_cli(
    project: Option<&str>,
    track: Option<String>,
    driver: Option<&str>,
    opts: RunCliOpts,
) -> Result<StatusView> {
    if opts.timeout_secs == Some(0) {
        return Err(CoordinatorError::Message(
            "timeout-secs must be > 0; omit the flag to tick until Idle/Stopped".into(),
        ));
    }
    let view = cmd_run(project, track, driver, opts.skip_preflight, true)?;
    if opts.detach {
        return Ok(attach_operator_view(view));
    }
    if let Some(port) = watch::listening_port(opts.probe) {
        eprintln!("serve owns the ticker on 127.0.0.1:{port}; not waiting");
        let mut view = view;
        view.ticker = Some(crate::state::TickerView {
            owner: "serve".into(),
            port: Some(port),
        });
        // 0102: attach the snapshot directly — `attach_operator_view` would
        // overwrite the probe-specific ticker with `ServeProbe::Auto`.
        view.features = Some(crate::features::resolve_all());
        return Ok(view);
    }
    let rec = load_registry()?
        .find_by_id(&view.project_id)
        .cloned()
        .ok_or_else(|| CoordinatorError::ProjectNotFound(view.project_id.clone()))?;
    Ok(attach_operator_view(watch::wait_for_outcome(
        &rec,
        opts.timeout_secs,
    )?))
}

/// POST /v1/harness/grok/prompt body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessPromptBody {
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub file: Option<String>,
}

pub async fn cmd_harness_grok_start(
    project: Option<&str>,
    in_process: bool,
    infer_cwd: bool,
) -> Result<crate::harness::GrokHarnessStatus> {
    crate::harness::start(project, in_process, infer_cwd).await
}

pub async fn cmd_harness_grok_prompt(
    project: Option<&str>,
    text: String,
    infer_cwd: bool,
) -> Result<crate::harness::HarnessPromptView> {
    crate::harness::prompt(project, text, infer_cwd).await
}

pub async fn cmd_harness_grok_prompt_body(
    body: HarnessPromptBody,
) -> Result<crate::harness::HarnessPromptView> {
    let text = match (body.text, body.file) {
        (Some(t), None) => t,
        (None, Some(p)) => std::fs::read_to_string(p)?,
        (Some(_), Some(_)) => {
            return Err(CoordinatorError::Message(
                "pass only one of text or file".into(),
            ));
        }
        (None, None) => {
            return Err(CoordinatorError::Message(
                "prompt requires text or file".into(),
            ));
        }
    };
    crate::harness::prompt(body.project.as_deref(), text, false).await
}

pub async fn cmd_harness_grok_compact(
    project: Option<&str>,
    infer_cwd: bool,
) -> Result<crate::harness::HarnessPromptView> {
    crate::harness::compact(project, infer_cwd).await
}

pub async fn cmd_harness_grok_status(
    project: Option<&str>,
    infer_cwd: bool,
) -> Result<crate::harness::GrokHarnessStatus> {
    crate::harness::grok_status(project, infer_cwd).await
}

pub async fn cmd_harness_grok_shutdown(
    project: Option<&str>,
    infer_cwd: bool,
) -> Result<crate::harness::GrokHarnessStatus> {
    crate::harness::shutdown(project, infer_cwd).await
}

pub async fn cmd_harness_grok_hold(project: Option<&str>) -> Result<()> {
    crate::harness::hold(project).await
}

/// `decision record`. CLI passes `infer_cwd = true`.
pub fn cmd_decision_record(
    project: Option<&str>,
    track: &str,
    by: &str,
    sentence: &str,
    supersede: bool,
    infer_cwd: bool,
) -> Result<crate::workflow::decision::RecordEffect> {
    let record = resolve_selected(project, infer_cwd)?;
    crate::workflow::decision::record_for(
        &record,
        track,
        by,
        sentence,
        supersede,
        chrono::Utc::now(),
    )
}

/// `decision show`. CLI passes `infer_cwd = true`.
pub fn cmd_decision_show(
    project: Option<&str>,
    track: &str,
    json: bool,
    infer_cwd: bool,
) -> Result<String> {
    let record = resolve_selected(project, infer_cwd)?;
    crate::workflow::decision::show_for(&record, track, json)
}

/// `policy show`. CLI passes `infer_cwd = true`.
pub fn cmd_policy_show(project: Option<&str>, infer_cwd: bool) -> Result<String> {
    let record = resolve_selected(project, infer_cwd)?;
    Ok(crate::policy::format_show(&crate::policy::resolved(
        &record,
    )))
}

/// `policy set`. Writes the project record only. Unknown names are an error.
pub fn cmd_policy_set(
    project: Option<&str>,
    name: &str,
    action: &str,
    threshold: Option<u32>,
    infer_cwd: bool,
) -> Result<ProjectRecord> {
    let name = crate::policy::require_name(name)?;
    let action = crate::policy::PolicyAction::parse(action)?;
    let selected = resolve_selected(project, infer_cwd)?;
    let mut reg = load_registry()?;
    let row = reg
        .projects
        .iter_mut()
        .find(|p| p.id == selected.id)
        .ok_or_else(|| CoordinatorError::ProjectNotFound(selected.id.clone()))?;
    if let Some(rule) = row.state_policies.iter_mut().find(|r| r.name == name) {
        rule.action = action;
        rule.threshold = threshold;
    } else {
        row.state_policies.push(crate::policy::PolicyRule {
            name: name.to_string(),
            action,
            threshold,
        });
    }
    let saved = row.clone();
    save_registry(&reg)?;
    Ok(saved)
}

/// `policy approve`. Stores nothing when the fingerprint cannot be read.
pub fn cmd_policy_approve(project: Option<&str>, name: &str, infer_cwd: bool) -> Result<String> {
    let record = resolve_selected(project, infer_cwd)?;
    // 0101: approve the same repo as the ci-wait policy gate. A non-delivery phase is a
    // typed error, not a primary fallback. Prefer the ci-wait latch and require agreement.
    let snapshot = crate::state::load_run_state(&record)?;
    let gate = crate::workflow::execution_repo::gate_target(&record, &snapshot)
        .map_err(|err| CoordinatorError::Message(format!("policy approve: {err}")))?;
    if gate.execution.planning_only {
        return Err(CoordinatorError::Message(format!(
            "policy approve: planning-only target {} has no product policy diff",
            gate.execution.declared_path.display()
        )));
    }
    let mut cwd = gate.execution.sample_path.clone();
    if let Some(latched) = snapshot.ci.as_ref().and_then(|ci| ci.target_repo.as_ref()) {
        if !crate::workflow::execution_repo::paths_same(latched, &cwd) {
            return Err(CoordinatorError::Message(format!(
                "policy approve: delivery target changed from {} to {} since the ci-wait latch",
                latched.display(),
                cwd.display()
            )));
        }
        cwd = latched.clone();
    }
    crate::state::with_run_state_lock(&record, || {
        let mut state = crate::state::load_run_state(&record)?;
        let current = crate::workflow::execution_repo::gate_target(&record, &state)
            .map_err(|err| CoordinatorError::Message(format!("policy approve: {err}")))?;
        if current.execution.project_id != snapshot.project_id
            || current.execution.run_epoch != snapshot.run_epoch
            || current.execution.track_id != gate.execution.track_id
            || !crate::workflow::execution_repo::paths_same(&current.execution.sample_path, &cwd)
        {
            return Err(CoordinatorError::Message(
                "policy approve: delivery target changed while approving; retry after the run settles"
                    .into(),
            ));
        }
        let fingerprint = crate::policy::approve(&record, &mut state, &cwd, name)?;
        crate::state::save_run_state(&record, &state)?;
        Ok(fingerprint)
    })
}

/// `decision list`. CLI passes `infer_cwd = true`.
pub fn cmd_decision_list(
    project: Option<&str>,
    track: &str,
    json: bool,
    infer_cwd: bool,
) -> Result<String> {
    let record = resolve_selected(project, infer_cwd)?;
    crate::workflow::decision::list_for(&record, track, json)
}

/// Persist a scan root into machine config (optional convenience).
pub fn save_scan_root(root: &Path) -> Result<config::MachineConfig> {
    let mut cfg = config::load_machine_config()?;
    let path = config::normalize_scan_root(root)?;
    if !cfg.scan_roots.iter().any(|r| r == &path) {
        cfg.scan_roots.push(path);
        config::save_machine_config(&cfg)?;
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ENV_COORDINATOR_HOME, ENV_OUTCOME_POLL_MS, ENV_STUB_PHASE_TIMEOUT_SECS, test_env_lock,
    };
    use crate::notify::artifact;
    use crate::state::{RunState, RunStatus};
    use crate::workflow::ENV_PHASE_TIMEOUT_SECS;
    use tempfile::tempdir;

    #[test]
    fn project_set_request_notify_progress_round_trip() {
        let raw = r#"{"project":"abc","notify_progress":true}"#;
        let req: ProjectSetRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.notify_progress, Some(true));
        let omitted: ProjectSetRequest = serde_json::from_str(r#"{"project":"abc"}"#).unwrap();
        assert_eq!(omitted.notify_progress, None);
    }

    #[test]
    fn project_add_set_request_auto_start_round_trip() {
        let add: ProjectAddRequest =
            serde_json::from_str(r#"{"path":"C:\\dev\\x","auto_start":"full"}"#).unwrap();
        assert_eq!(add.auto_start, Some(crate::registry::AutoStartPolicy::Full));
        let omitted_add: ProjectAddRequest =
            serde_json::from_str(r#"{"path":"C:\\dev\\x"}"#).unwrap();
        assert_eq!(omitted_add.auto_start, None);
        let set: ProjectSetRequest =
            serde_json::from_str(r#"{"project":"abc","auto_start":"never"}"#).unwrap();
        assert_eq!(
            set.auto_start,
            Some(crate::registry::AutoStartPolicy::Never)
        );
    }

    /// Fold → implement refuses a missing git cwd. Full stub walks need a clean repo.
    fn attach_clean_exec(rec: &ProjectRecord) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = crate::git_command::command()
                .args(args)
                .current_dir(dir.path())
                .env("GIT_OPTIONAL_LOCKS", "0")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.email", "checkpoint-walk@example.com"]);
        git(&["config", "user.name", "checkpoint-walk"]);
        std::fs::write(dir.path().join("README.md"), b"seed\n").unwrap();
        git(&["add", "README.md"]);
        git(&["commit", "-m", "seed"]);
        let mut reg = load_registry().unwrap();
        let slot = reg
            .projects
            .iter_mut()
            .find(|p| p.id == rec.id)
            .expect("project");
        slot.execution_repo = Some(dir.path().to_path_buf());
        save_registry(&reg).unwrap();
        // Fold → implement allows this row. The git repo stays the other temp dir.
        crate::workflow::conductor_md::write_ready_fixture(rec.path.as_path(), "0020").unwrap();
        dir
    }

    fn add_isolated_project() -> (tempfile::TempDir, tempfile::TempDir, ProjectRecord) {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let rec = project_add(proj.path(), ProjectAddOptions::default()).unwrap();
        (home, proj, rec)
    }

    fn clear_home() {
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    /// 0102 fixture guard: machine home, legacy fleet/Hermes env, every duty gate,
    /// and the adapter poll/timeout knobs (restored on drop, panic-safe).
    fn feature_env_guard() -> crate::config::TestEnvGuard {
        let mut names = crate::features::test_feature_env_names();
        names.push(ENV_OUTCOME_POLL_MS);
        names.push(ENV_PHASE_TIMEOUT_SECS);
        names.push(ENV_STUB_PHASE_TIMEOUT_SECS);
        crate::config::TestEnvGuard::capture(&names)
    }

    fn write_feature_config(home: &std::path::Path, features: serde_json::Value) {
        let body = serde_json::json!({
            "version": 1,
            "scan_roots": [],
            "features": features,
        });
        std::fs::write(home.join("config.json"), body.to_string()).unwrap();
    }

    fn duties_match(
        view: &StatusView,
        duty: crate::features::Duty,
        enabled: bool,
        source: crate::features::FeatureSource,
    ) {
        let features = view.features.as_ref().expect("features attached");
        let gate = &features.duties[duty.as_str()];
        assert_eq!(gate.enabled, enabled, "{} enabled", duty.as_str());
        assert_eq!(gate.source, source, "{} source", duty.as_str());
    }

    fn spawn_health_once(body: &str) -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = body.to_string();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        port
    }

    #[test]
    fn cmd_run_cli_stub_ticks_to_idle() {
        let _guard = test_env_lock();
        let _env = feature_env_guard();
        unsafe {
            std::env::set_var(ENV_OUTCOME_POLL_MS, "10");
            std::env::set_var(ENV_PHASE_TIMEOUT_SECS, "30");
        }
        let (_home, _proj, rec) = add_isolated_project();
        let _repo = attach_clean_exec(&rec);
        let view = cmd_run_cli(
            Some(&rec.id),
            Some("0020".into()),
            Some("stub"),
            RunCliOpts {
                detach: false,
                timeout_secs: None,
                probe: ServeProbe::Skip,
                skip_preflight: false,
            },
        )
        .unwrap();
        assert_eq!(view.status, RunStatus::Idle);
        // `wait_for_outcome` return carries the snapshot too (0102 DoD-2).
        let features = view.features.as_ref().expect("features on wait return");
        assert_eq!(features.duties.len(), 6);
    }

    #[test]
    fn cmd_run_cli_detach_stays_running_at_plan() {
        let _guard = test_env_lock();
        let _env = feature_env_guard();
        let (home, _proj, rec) = add_isolated_project();
        write_feature_config(home.path(), serde_json::json!({"fleet_notify": false}));
        let view = cmd_run_cli(
            Some(&rec.id),
            Some("0020".into()),
            Some("stub"),
            RunCliOpts {
                detach: true,
                timeout_secs: None,
                probe: ServeProbe::Skip,
                skip_preflight: false,
            },
        )
        .unwrap();
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.phase, crate::workflow::graph::PHASE_PLAN);
        let features = view.features.as_ref().expect("features on detach return");
        assert_eq!(features.duties.len(), 6);
        duties_match(
            &view,
            crate::features::Duty::FleetNotify,
            false,
            crate::features::FeatureSource::Config,
        );
    }

    #[test]
    fn cmd_run_cli_timeout_expires_without_abort() {
        let _guard = test_env_lock();
        unsafe {
            std::env::set_var(ENV_OUTCOME_POLL_MS, "50");
            std::env::set_var(ENV_STUB_PHASE_TIMEOUT_SECS, "3600");
            std::env::set_var(ENV_PHASE_TIMEOUT_SECS, "3600");
        }
        let (_home, _proj, rec) = add_isolated_project();
        let err = cmd_run_cli(
            Some(&rec.id),
            Some("0020".into()),
            Some("file_wait"),
            RunCliOpts {
                detach: false,
                timeout_secs: Some(1),
                probe: ServeProbe::Skip,
                skip_preflight: false,
            },
        )
        .unwrap_err();
        assert!(
            matches!(err, CoordinatorError::WaitBudgetExpired),
            "err={err}"
        );
        let s = status(Some(&rec.id), false).unwrap();
        assert_eq!(s.status, RunStatus::Running);
        assert!(s.failure_class.is_none());
        assert!(artifact::existing_path(&rec).is_none());
        unsafe {
            std::env::remove_var(ENV_OUTCOME_POLL_MS);
            std::env::remove_var(ENV_STUB_PHASE_TIMEOUT_SECS);
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
        }
        clear_home();
    }

    #[test]
    fn cmd_run_cli_timeout_zero_rejected_before_write() {
        let _guard = test_env_lock();
        let (_home, _proj, rec) = add_isolated_project();
        let err = cmd_run_cli(
            Some(&rec.id),
            Some("0020".into()),
            Some("stub"),
            RunCliOpts {
                detach: false,
                timeout_secs: Some(0),
                probe: ServeProbe::Skip,
                skip_preflight: false,
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("timeout-secs must be > 0"),
            "err={err}"
        );
        let s = status(Some(&rec.id), false).unwrap();
        assert_eq!(s.status, RunStatus::Idle);
        clear_home();
    }

    #[test]
    fn cmd_run_cli_serve_owns_skips_wait() {
        let _guard = test_env_lock();
        let _env = feature_env_guard();
        let (home, _proj, rec) = add_isolated_project();
        write_feature_config(home.path(), serde_json::json!({"fleet_notify": true}));
        let port = spawn_health_once(r#"{"ok":true,"service":"coordinator"}"#);
        let view = cmd_run_cli(
            Some(&rec.id),
            Some("0020".into()),
            Some("file_wait"),
            RunCliOpts {
                detach: false,
                timeout_secs: Some(1),
                probe: ServeProbe::Port(port),
                skip_preflight: false,
            },
        )
        .unwrap();
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.phase, crate::workflow::graph::PHASE_PLAN);
        assert!(artifact::existing_path(&rec).is_none());
        let ticker = view.ticker.as_ref().expect("ticker");
        assert_eq!(ticker.owner, "serve");
        // Attaching through `attach_operator_view` would probe `ServeProbe::Auto`
        // and clobber this assert; omitting features would fail the next one.
        assert_eq!(ticker.port, Some(port));
        duties_match(
            &view,
            crate::features::Duty::FleetNotify,
            true,
            crate::features::FeatureSource::Config,
        );
    }

    #[test]
    fn cmd_run_cli_non_coordinator_health_ticks() {
        let _guard = test_env_lock();
        unsafe {
            std::env::set_var(ENV_OUTCOME_POLL_MS, "10");
            std::env::set_var(ENV_PHASE_TIMEOUT_SECS, "30");
        }
        let (_home, _proj, rec) = add_isolated_project();
        let _repo = attach_clean_exec(&rec);
        let port = spawn_health_once(r#"{"ok":true}"#);
        let view = cmd_run_cli(
            Some(&rec.id),
            Some("0020".into()),
            Some("stub"),
            RunCliOpts {
                detach: false,
                timeout_secs: None,
                probe: ServeProbe::Port(port),
                skip_preflight: false,
            },
        )
        .unwrap();
        assert_eq!(view.status, RunStatus::Idle);
        unsafe {
            std::env::remove_var(ENV_OUTCOME_POLL_MS);
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
        }
        clear_home();
    }

    #[test]
    fn cmd_run_cli_file_wait_detach_stays_running() {
        let _guard = test_env_lock();
        let (_home, _proj, rec) = add_isolated_project();
        let view = cmd_run_cli(
            Some(&rec.id),
            Some("0020".into()),
            Some("file_wait"),
            RunCliOpts {
                detach: true,
                timeout_secs: None,
                probe: ServeProbe::Skip,
                skip_preflight: false,
            },
        )
        .unwrap();
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.phase, crate::workflow::graph::PHASE_PLAN);
        clear_home();
    }

    #[test]
    fn cmd_run_cli_auto_healthy_lease_skips_wait() {
        let _guard = test_env_lock();
        let (_home, _proj, rec) = add_isolated_project();
        let hold = watch::spawn_health_hold(r#"{"ok":true,"service":"coordinator"}"#);
        crate::serve_lease::write_serve_lease(hold.port).unwrap();
        let view = cmd_run_cli(
            Some(&rec.id),
            Some("0020".into()),
            Some("file_wait"),
            RunCliOpts {
                detach: false,
                timeout_secs: Some(1),
                probe: ServeProbe::Auto,
                skip_preflight: false,
            },
        )
        .unwrap();
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.phase, crate::workflow::graph::PHASE_PLAN);
        let ticker = view.ticker.expect("ticker");
        assert_eq!(ticker.owner, "serve");
        assert_eq!(ticker.port, Some(hold.port));
        clear_home();
    }

    #[test]
    fn cmd_run_cli_auto_stale_lease_ticks() {
        let _guard = test_env_lock();
        unsafe {
            std::env::set_var(ENV_OUTCOME_POLL_MS, "10");
            std::env::set_var(ENV_PHASE_TIMEOUT_SECS, "30");
        }
        let (_home, _proj, rec) = add_isolated_project();
        let _repo = attach_clean_exec(&rec);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = listener.local_addr().unwrap().port();
        drop(listener);
        crate::serve_lease::write_serve_lease(dead).unwrap();
        if watch::coordinator_serve_listening(crate::config::DEFAULT_SERVE_PORT) {
            unsafe {
                std::env::remove_var(ENV_OUTCOME_POLL_MS);
                std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
            }
            clear_home();
            return;
        }
        let view = cmd_run_cli(
            Some(&rec.id),
            Some("0020".into()),
            Some("stub"),
            RunCliOpts {
                detach: false,
                timeout_secs: None,
                probe: ServeProbe::Auto,
                skip_preflight: false,
            },
        )
        .unwrap();
        assert_eq!(view.status, RunStatus::Idle);
        unsafe {
            std::env::remove_var(ENV_OUTCOME_POLL_MS);
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
        }
        clear_home();
    }

    #[test]
    fn cmd_run_cli_explicit_port_ignores_healthy_lease() {
        let _guard = test_env_lock();
        unsafe {
            std::env::set_var(ENV_OUTCOME_POLL_MS, "10");
            std::env::set_var(ENV_PHASE_TIMEOUT_SECS, "30");
        }
        let (_home, _proj, rec) = add_isolated_project();
        let _repo = attach_clean_exec(&rec);
        let hold = watch::spawn_health_hold(r#"{"ok":true,"service":"coordinator"}"#);
        crate::serve_lease::write_serve_lease(hold.port).unwrap();
        let other = spawn_health_once(r#"{"ok":true}"#);
        let view = cmd_run_cli(
            Some(&rec.id),
            Some("0020".into()),
            Some("stub"),
            RunCliOpts {
                detach: false,
                timeout_secs: None,
                probe: ServeProbe::Port(other),
                skip_preflight: false,
            },
        )
        .unwrap();
        assert_eq!(view.status, RunStatus::Idle);
        unsafe {
            std::env::remove_var(ENV_OUTCOME_POLL_MS);
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
        }
        clear_home();
    }

    #[test]
    fn api_status_ticker_serve_vs_none() {
        let _guard = test_env_lock();
        let (_home, _proj, rec) = add_isolated_project();
        if !watch::coordinator_serve_listening(crate::config::DEFAULT_SERVE_PORT) {
            let view = status(Some(&rec.id), false).unwrap();
            let ticker = view.ticker.expect("ticker");
            assert_eq!(ticker.owner, "none");
            assert!(ticker.port.is_none());
        }
        let hold = watch::spawn_health_hold(r#"{"ok":true,"service":"coordinator"}"#);
        crate::serve_lease::write_serve_lease(hold.port).unwrap();
        let view = status(Some(&rec.id), false).unwrap();
        let ticker = view.ticker.expect("ticker");
        assert_eq!(ticker.owner, "serve");
        assert_eq!(ticker.port, Some(hold.port));
        clear_home();
    }

    #[test]
    fn status_all_clones_one_ticker() {
        let _guard = test_env_lock();
        let (_home, _proj, rec) = add_isolated_project();
        let _proj2 = tempfile::tempdir().unwrap();
        let rec2 = project_add(_proj2.path(), ProjectAddOptions::default()).unwrap();
        assert_ne!(rec.id, rec2.id);
        let hold = watch::spawn_health_hold(r#"{"ok":true,"service":"coordinator"}"#);
        crate::serve_lease::write_serve_lease(hold.port).unwrap();
        let all = status_all().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].ticker, all[1].ticker);
        let ticker = all[0].ticker.as_ref().expect("ticker");
        assert_eq!(ticker.owner, "serve");
        assert_eq!(ticker.port, Some(hold.port));
        clear_home();
    }

    #[test]
    fn run_status_and_from_record_omit_ticker_and_features() {
        let _guard = test_env_lock();
        let _env = feature_env_guard();
        let (_home, _proj, rec) = add_isolated_project();
        let view = run::status(&rec).unwrap();
        assert!(view.ticker.is_none());
        assert!(view.features.is_none(), "poll path omits features (0102)");
        let json = serde_json::to_value(&view).unwrap();
        assert!(
            !json.as_object().unwrap().contains_key("ticker"),
            "poll-path status must omit ticker"
        );
        assert!(
            !json.as_object().unwrap().contains_key("features"),
            "poll-path status must omit features"
        );
    }

    #[test]
    fn api_status_renders_each_duty_state_and_source() {
        let _guard = test_env_lock();
        let _env = feature_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        let (home, _proj, rec) = add_isolated_project();

        let view = status(Some(&rec.id), false).unwrap();
        for duty in crate::features::Duty::ALL {
            duties_match(&view, duty, true, crate::features::FeatureSource::Default);
        }

        write_feature_config(home.path(), serde_json::json!({"fleet_notify": false}));
        let view = status(Some(&rec.id), false).unwrap();
        duties_match(
            &view,
            crate::features::Duty::FleetNotify,
            false,
            crate::features::FeatureSource::Config,
        );
        duties_match(
            &view,
            crate::features::Duty::AutoStart,
            true,
            crate::features::FeatureSource::Default,
        );

        unsafe {
            std::env::set_var(crate::features::Duty::StallPaging.env_var(), "off");
        }
        let view = status(Some(&rec.id), false).unwrap();
        duties_match(
            &view,
            crate::features::Duty::StallPaging,
            false,
            crate::features::FeatureSource::Env,
        );

        write_feature_config(home.path(), serde_json::json!({"fleet_notify": "yes"}));
        let view = status(Some(&rec.id), false).unwrap();
        let features = view.features.as_ref().expect("features");
        assert!(
            features.unknown_keys.is_empty(),
            "the type-invalid path does not enumerate keys"
        );
        for duty in crate::features::Duty::ALL {
            let gate = &features.duties[duty.as_str()];
            assert!(!gate.enabled, "{} fail-closed", duty.as_str());
            assert_eq!(gate.source, crate::features::FeatureSource::Config);
            assert!(gate.error.is_some());
        }
    }

    #[test]
    fn api_status_feature_view_flips_with_live_edits() {
        let _guard = test_env_lock();
        let _env = feature_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        let (home, _proj, rec) = add_isolated_project();

        for enabled in [true, false, true] {
            write_feature_config(home.path(), serde_json::json!({"fleet_notify": enabled}));
            let view = status(Some(&rec.id), false).unwrap();
            duties_match(
                &view,
                crate::features::Duty::FleetNotify,
                enabled,
                crate::features::FeatureSource::Config,
            );
            // The gate reads the same snapshot the status surface renders.
            assert_eq!(
                crate::features::resolve_all().enabled(crate::features::Duty::FleetNotify),
                enabled
            );
        }
    }

    #[test]
    fn status_json_has_the_six_stable_duty_keys() {
        let _guard = test_env_lock();
        let _env = feature_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        let (_home, _proj, rec) = add_isolated_project();
        let view = status(Some(&rec.id), false).unwrap();
        let json = serde_json::to_value(&view).unwrap();
        let features = json["features"].as_object().expect("features object");
        let mut keys: Vec<&str> = features.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "auto_start",
                "fleet_notify",
                "model_swap",
                "self_reap",
                "self_update",
                "stall_paging"
            ]
        );
        assert!(!features.contains_key("unknown_keys"));
    }

    #[test]
    fn status_all_clones_one_feature_snapshot() {
        let _guard = test_env_lock();
        let _env = feature_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        let (_home, _proj, rec) = add_isolated_project();
        let proj2 = tempfile::tempdir().unwrap();
        let rec2 = project_add(proj2.path(), ProjectAddOptions::default()).unwrap();
        assert_ne!(rec.id, rec2.id);
        let all = status_all().unwrap();
        assert_eq!(all.len(), 2);
        assert!(all[0].features.is_some());
        assert_eq!(all[0].features, all[1].features);
    }

    #[test]
    fn unknown_keys_are_visible_and_fail_closed_in_status() {
        let _guard = test_env_lock();
        let _env = feature_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        let (home, _proj, rec) = add_isolated_project();
        write_feature_config(
            home.path(),
            serde_json::json!({"auto_strat": true, "fleet_notify": false}),
        );
        let view = status(Some(&rec.id), false).unwrap();
        let features = view.features.as_ref().expect("features");
        assert_eq!(features.unknown_keys, vec!["auto_strat".to_string()]);
        for duty in crate::features::Duty::ALL {
            let gate = &features.duties[duty.as_str()];
            assert!(
                !gate.enabled,
                "{} fail-closed on an unknown key",
                duty.as_str()
            );
            assert_eq!(gate.source, crate::features::FeatureSource::Config);
            assert!(gate.error.is_some());
        }
    }

    #[test]
    fn project_set_phase_timeouts_round_trip() {
        let _guard = test_env_lock();
        unsafe {
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
        }
        let (_home, _proj, rec) = add_isolated_project();
        let mut map = BTreeMap::new();
        map.insert("plan".into(), 3600);
        map.insert("implement".into(), 10800);
        let updated = project_set(
            Some(&rec.id),
            ProjectSetOptions {
                phase_timeouts_secs: Some(map),
                ..Default::default()
            },
            false,
        )
        .unwrap();
        assert_eq!(updated.phase_timeouts_secs.get("plan"), Some(&3600));
        assert_eq!(updated.phase_timeouts_secs.get("implement"), Some(&10800));
        let loaded = load_registry()
            .unwrap()
            .find_by_id(&rec.id)
            .unwrap()
            .clone();
        assert_eq!(loaded.phase_timeouts_secs.get("plan"), Some(&3600));
        clear_home();
    }

    #[test]
    fn project_show_sources_project_table_and_env() {
        let _guard = test_env_lock();
        unsafe {
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
        }
        let (_home, _proj, rec) = add_isolated_project();
        let mut map = BTreeMap::new();
        map.insert("plan".into(), 3600);
        project_set(
            Some(&rec.id),
            ProjectSetOptions {
                phase_timeouts_secs: Some(map),
                ..Default::default()
            },
            false,
        )
        .unwrap();
        let view = project_show(Some(&rec.id), false).unwrap();
        for phase in crate::workflow::graph::all_phase_ids() {
            assert!(
                view.phase_timeouts.contains_key(*phase),
                "show missing effective timeout for {phase}"
            );
        }
        assert_eq!(view.phase_timeouts["address-findings"].secs, 3600);
        assert_eq!(view.phase_timeouts["address-findings"].source, "table");
        assert_eq!(view.phase_timeouts["plan"].secs, 3600);
        assert_eq!(view.phase_timeouts["plan"].source, "project");
        assert_eq!(view.phase_timeouts["implement"].secs, 7200);
        assert_eq!(view.phase_timeouts["implement"].source, "table");
        assert_eq!(view.phase_timeouts["plan-review"].secs, 2400);
        assert_eq!(view.phase_timeouts["plan-review"].source, "table");
        assert_eq!(view.phase_timeouts["plan_review_slot"].secs, 2400);
        assert_eq!(view.phase_timeouts["plan_review_slot"].source, "table");
        assert_eq!(view.project.phase_timeouts_secs.get("plan"), Some(&3600));
        unsafe {
            std::env::set_var(ENV_PHASE_TIMEOUT_SECS, "7");
        }
        let view = project_show(Some(&rec.id), false).unwrap();
        assert_eq!(view.phase_timeouts["plan"].secs, 7);
        assert_eq!(view.phase_timeouts["plan"].source, "env");
        assert_eq!(view.phase_timeouts["implement"].source, "env");
        unsafe {
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
        }
        clear_home();
    }

    #[test]
    fn resolve_selected_persists_explicit_and_ignores_stale() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let a = tempdir().unwrap();
        let b = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let rec_a = project_add(a.path(), ProjectAddOptions::default()).unwrap();
        let rec_b = project_add(b.path(), ProjectAddOptions::default()).unwrap();
        let got = resolve_selected(Some(&rec_b.id), false).unwrap();
        assert_eq!(got.id, rec_b.id);
        assert_eq!(
            crate::config::load_last_used_id().as_deref(),
            Some(rec_b.id.as_str())
        );
        let path = home.path().join("last-used.json");
        let before = std::fs::read(&path).unwrap();
        let again = resolve_selected(Some(&rec_b.id), false).unwrap();
        assert_eq!(again.id, rec_b.id);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "same-id explicit resolve must not rewrite last-used.json"
        );
        let via_last = resolve_selected(None, false).unwrap();
        assert_eq!(via_last.id, rec_b.id);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "last-used-only hit must not rewrite last-used.json"
        );
        crate::config::save_last_used_id("not-registered").unwrap();
        let err = resolve_selected(None, false).unwrap_err().to_string();
        assert!(
            err.contains("cwd is not inside a registered workspace or execution repo"),
            "{err}"
        );
        let _ = rec_a;
        clear_home();
    }

    #[test]
    fn cmd_run_cli_omit_unique_cwd_wait_attach_uses_view_id() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let a = tempdir().unwrap();
        let b = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(ENV_OUTCOME_POLL_MS, "10");
            std::env::set_var(ENV_PHASE_TIMEOUT_SECS, "30");
        }
        project_add(a.path(), ProjectAddOptions::default()).unwrap();
        let rec_b = project_add(b.path(), ProjectAddOptions::default()).unwrap();
        let _repo = attach_clean_exec(&rec_b);
        crate::workflow::conductor_md::write_ready_fixture(b.path(), "0029").unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(b.path()).unwrap();
        let view = cmd_run_cli(
            None,
            Some("0029".into()),
            Some("stub"),
            RunCliOpts {
                detach: false,
                timeout_secs: None,
                probe: ServeProbe::Skip,
                skip_preflight: false,
            },
        );
        std::env::set_current_dir(prev).unwrap();
        let view = view.expect("omit run with unique cwd must wait-attach");
        assert_eq!(view.project_id, rec_b.id);
        assert_eq!(view.status, RunStatus::Idle);
        unsafe {
            std::env::remove_var(ENV_OUTCOME_POLL_MS);
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
        }
        clear_home();
    }

    #[test]
    fn cmd_run_running_empty_track_is_invalid_transition_not_pick() {
        let _guard = test_env_lock();
        let (_home, proj, rec) = add_isolated_project();
        crate::workflow::conductor_md::write_ready_fixture(proj.path(), "0030").unwrap();
        cmd_run(
            Some(&rec.id),
            Some("0020".into()),
            Some("stub"),
            false,
            false,
        )
        .unwrap();
        let mut state = crate::state::load_run_state(&rec).unwrap();
        state.track_id = None;
        crate::state::save_run_state(&rec, &state).unwrap();
        let err = cmd_run(Some(&rec.id), None, Some("stub"), false, false).unwrap_err();
        match err {
            CoordinatorError::InvalidTransition { action, from } => {
                assert_eq!(action, "run");
                assert_eq!(from, "Running");
            }
            other => panic!("expected InvalidTransition, got {other}"),
        }
        clear_home();
    }

    #[test]
    fn cmd_run_omit_picks_ready_and_tags_last_event() {
        let _guard = test_env_lock();
        let (_home, proj, rec) = add_isolated_project();
        crate::workflow::conductor_md::write_ready_fixture(proj.path(), "0030").unwrap();
        let view = cmd_run(Some(&rec.id), None, Some("stub"), false, false).unwrap();
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.track_id.as_deref(), Some("0030"));
        assert!(
            view.last_event.contains("(next Ready)"),
            "{}",
            view.last_event
        );
        clear_home();
    }

    fn write_two_ready_tracks(ws: &std::path::Path) {
        let cond = ws.join("conductor");
        std::fs::create_dir_all(cond.join("0001-One")).unwrap();
        std::fs::create_dir_all(cond.join("0002-Two")).unwrap();
        std::fs::write(
            cond.join("conductor.md"),
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0001-One | `.` | **Ready — not started** | a |\n\
             | 0002-Two | `.` | **Ready — not started** | b |\n",
        )
        .unwrap();
    }

    #[test]
    fn resolve_run_track_with_probe_omit_skips_merged() {
        let _guard = test_env_lock();
        let (_home, proj, mut rec) = add_isolated_project();
        write_two_ready_tracks(proj.path());
        rec.execution_repo = Some(proj.path().to_path_buf());
        let probe = crate::workflow::shipped::ScriptedMergedProbe::found("0001", 56);
        let (track, picked) = resolve_run_track_with_probe(&rec, None, Some(&probe)).unwrap();
        assert!(picked);
        assert_eq!(track.as_deref(), Some("0002"));
        clear_home();
    }

    #[test]
    fn resolve_run_track_with_probe_explicit_wins() {
        let _guard = test_env_lock();
        let (_home, proj, mut rec) = add_isolated_project();
        write_two_ready_tracks(proj.path());
        rec.execution_repo = Some(proj.path().to_path_buf());
        let probe = crate::workflow::shipped::ScriptedMergedProbe::found("0001", 56);
        let (track, picked) =
            resolve_run_track_with_probe(&rec, Some("0001".into()), Some(&probe)).unwrap();
        assert!(!picked);
        assert_eq!(track.as_deref(), Some("0001"));
        let (plain, _) = resolve_run_track(&rec, None).unwrap();
        assert_eq!(plain.as_deref(), Some("0001"));
        clear_home();
    }

    #[test]
    fn resolve_run_track_with_probe_err_fail_opens() {
        let _guard = test_env_lock();
        let (_home, proj, mut rec) = add_isolated_project();
        write_two_ready_tracks(proj.path());
        rec.execution_repo = Some(proj.path().to_path_buf());
        let probe = crate::workflow::shipped::ScriptedMergedProbe::err("0001", "boom");
        let (track, picked) = resolve_run_track_with_probe(&rec, None, Some(&probe)).unwrap();
        assert!(picked);
        assert_eq!(track.as_deref(), Some("0001"));
        clear_home();
    }

    struct CountingProbe {
        hits: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl crate::workflow::MergedTrackProbe for CountingProbe {
        fn merged_pr_for_track(
            &self,
            _cwd: &std::path::Path,
            numeric_id: &str,
        ) -> Result<Option<u64>> {
            if numeric_id == "0060" {
                self.hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(None)
        }
    }

    fn write_status_rows(ws: &std::path::Path, rows: &[(&str, &str)]) {
        let cond = ws.join("conductor");
        std::fs::create_dir_all(&cond).unwrap();
        let mut md = String::from(
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n",
        );
        for (slug, status) in rows {
            std::fs::create_dir_all(cond.join(slug)).unwrap();
            md.push_str(&format!("| {slug} | `.` | {status} | row |\n"));
        }
        std::fs::write(cond.join("conductor.md"), md).unwrap();
    }

    fn retain_state(rec: &ProjectRecord, id: &str, stopped: bool) {
        let mut state = crate::state::load_run_state(rec).unwrap();
        state.status = if stopped {
            RunStatus::Stopped
        } else {
            RunStatus::Idle
        };
        state.track_id = Some(id.into());
        state.next_track = None;
        state.failure_class = None;
        state.last_event = if stopped {
            "stop: hold".into()
        } else {
            "idle: retained".into()
        };
        state.sticky_ready_ids.clear();
        crate::state::save_run_state(rec, &state).unwrap();
    }

    /// Epochs 30/31/32 (2026-09-26) resumed shipped 0060 while its cell read
    /// `In progress` #60, `In progress` #62, then `**Completed**`.
    fn assert_refused_shipped_picks_sibling(stopped: bool) {
        let _guard = test_env_lock();
        let (_home, proj, mut rec) = add_isolated_project();
        write_status_rows(
            proj.path(),
            &[
                ("0060-Shipped", "**In progress** PR #60"),
                ("0061-Next", "**Ready — not started**"),
            ],
        );
        rec.execution_repo = Some(proj.path().to_path_buf());
        retain_state(&rec, "0060", stopped);
        let probe = crate::workflow::shipped::ScriptedMergedProbe::found("0060", 60);
        let (track, picked) = resolve_run_track_with_probe(&rec, None, Some(&probe)).unwrap();
        assert!(picked);
        assert_eq!(track.as_deref(), Some("0061"));
        let log = std::fs::read_to_string(crate::progress_log::path(&rec)).unwrap_or_default();
        assert!(log.contains("omit-pick: refuse shipped 0060"), "{log}");
        clear_home();
    }

    #[test]
    fn omit_stopped_merged_in_progress_picks_other_ready() {
        assert_refused_shipped_picks_sibling(true);
    }

    #[test]
    fn omit_idle_without_backlog_merged_in_progress_picks_other_ready() {
        assert_refused_shipped_picks_sibling(false);
    }

    #[test]
    fn omit_stopped_merged_in_progress_empty_ready_leaves_state() {
        let _guard = test_env_lock();
        let (_home, proj, mut rec) = add_isolated_project();
        write_status_rows(proj.path(), &[("0060-Shipped", "**In progress** PR #60")]);
        rec.execution_repo = Some(proj.path().to_path_buf());
        retain_state(&rec, "0060", true);
        let state_path = crate::state::run_state_path(&rec).unwrap();
        let before_state = std::fs::read(&state_path).unwrap();
        let before_md = std::fs::read(proj.path().join("conductor").join("conductor.md")).unwrap();
        let probe = crate::workflow::shipped::ScriptedMergedProbe::found("0060", 60);
        let err = resolve_run_track_with_probe(&rec, None, Some(&probe))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no Ready"), "{err}");
        assert_eq!(std::fs::read(&state_path).unwrap(), before_state);
        assert_eq!(
            std::fs::read(proj.path().join("conductor").join("conductor.md")).unwrap(),
            before_md
        );
        let loaded = crate::state::load_run_state(&rec).unwrap();
        assert_eq!(loaded.status, RunStatus::Stopped);
        assert_eq!(loaded.track_id.as_deref(), Some("0060"));
        clear_home();
    }

    #[test]
    fn omit_stopped_completed_does_not_call_probe() {
        let _guard = test_env_lock();
        let (_home, proj, mut rec) = add_isolated_project();
        write_status_rows(
            proj.path(),
            &[
                ("0060-Shipped", "**Completed**"),
                ("0061-Next", "**Ready — not started**"),
            ],
        );
        rec.execution_repo = Some(proj.path().to_path_buf());
        retain_state(&rec, "0060", true);
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let probe = CountingProbe { hits: hits.clone() };
        let (track, picked) = resolve_run_track_with_probe(&rec, None, Some(&probe)).unwrap();
        assert!(picked);
        assert_eq!(track.as_deref(), Some("0061"));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
        clear_home();
    }

    #[test]
    fn omit_stopped_in_progress_probe_none_retains() {
        let _guard = test_env_lock();
        let (_home, proj, mut rec) = add_isolated_project();
        write_status_rows(
            proj.path(),
            &[
                ("0060-Open", "**In progress** PR #60"),
                ("0061-Next", "**Ready — not started**"),
            ],
        );
        rec.execution_repo = Some(proj.path().to_path_buf());
        retain_state(&rec, "0060", true);
        let probe = crate::workflow::shipped::ScriptedMergedProbe::default();
        let (track, picked) = resolve_run_track_with_probe(&rec, None, Some(&probe)).unwrap();
        assert!(!picked);
        assert!(track.is_none());
        clear_home();
    }

    #[test]
    fn omit_stopped_in_progress_probe_err_retains() {
        let _guard = test_env_lock();
        let (_home, proj, mut rec) = add_isolated_project();
        write_status_rows(proj.path(), &[("0060-Open", "**In progress** PR #60")]);
        rec.execution_repo = Some(proj.path().to_path_buf());
        retain_state(&rec, "0060", true);
        let probe = crate::workflow::shipped::ScriptedMergedProbe::err("0060", "gh down");
        let (track, picked) = resolve_run_track_with_probe(&rec, None, Some(&probe)).unwrap();
        assert!(!picked);
        assert!(track.is_none());
        clear_home();
    }

    #[test]
    fn omit_stopped_merged_never_does_not_pick() {
        let _guard = test_env_lock();
        let (_home, proj, mut rec) = add_isolated_project();
        write_status_rows(
            proj.path(),
            &[
                ("0060-Shipped", "**In progress** PR #60"),
                ("0061-Next", "**Ready — not started**"),
            ],
        );
        rec.execution_repo = Some(proj.path().to_path_buf());
        rec.auto_start = crate::registry::AutoStartPolicy::Never;
        retain_state(&rec, "0060", true);
        let state_path = crate::state::run_state_path(&rec).unwrap();
        let before_state = std::fs::read(&state_path).unwrap();
        let probe = crate::workflow::shipped::ScriptedMergedProbe::found("0060", 60);
        let err = resolve_run_track_with_probe(&rec, None, Some(&probe))
            .unwrap_err()
            .to_string();
        assert!(err.contains("auto_start=never"), "{err}");
        assert_eq!(std::fs::read(&state_path).unwrap(), before_state);
        clear_home();
    }

    #[test]
    fn omit_stopped_absorbed_sticky_does_not_return_self() {
        let _guard = test_env_lock();
        let (_home, proj, rec) = add_isolated_project();
        write_status_rows(proj.path(), &[("0060-Gone", "**Absorbed**")]);
        retain_state(&rec, "0060", true);
        let mut state = crate::state::load_run_state(&rec).unwrap();
        state.sticky_ready_ids = vec!["0060".into()];
        crate::state::save_run_state(&rec, &state).unwrap();
        let err = resolve_run_track_with_probe(&rec, None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no Ready"), "{err}");
        let loaded = crate::state::load_run_state(&rec).unwrap();
        assert_eq!(loaded.status, RunStatus::Stopped);
        assert_eq!(loaded.track_id.as_deref(), Some("0060"));
        clear_home();
    }

    #[test]
    fn run_with_origin_refuses_completed_retain() {
        let _guard = test_env_lock();
        let (_home, proj, rec) = add_isolated_project();
        write_status_rows(
            proj.path(),
            &[(
                "0060-Shipped",
                "**Completed** 2026-09-26 — PR **#60** squash `0d8244a`",
            )],
        );
        retain_state(&rec, "0060", true);
        let err = run::run_with_origin(&rec, None, crate::workflow::WorkflowDriver::Stub, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("0060"), "{err}");
        assert!(err.contains("not resumable"), "{err}");
        let loaded = crate::state::load_run_state(&rec).unwrap();
        assert_eq!(loaded.status, RunStatus::Stopped);
        assert_eq!(loaded.track_id.as_deref(), Some("0060"));
        clear_home();
    }

    #[test]
    fn run_with_origin_keeps_open_stopped_retain() {
        let _guard = test_env_lock();
        let (_home, proj, rec) = add_isolated_project();
        write_status_rows(proj.path(), &[("0060-Open", "**In progress** PR #60")]);
        retain_state(&rec, "0060", true);
        let view =
            run::run_with_origin(&rec, None, crate::workflow::WorkflowDriver::Stub, false).unwrap();
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.track_id.as_deref(), Some("0060"));
        clear_home();
    }

    #[test]
    fn cmd_run_stub_omit_ignores_installed_probe() {
        let _guard = test_env_lock();
        let (_home, proj, rec) = add_isolated_project();
        write_two_ready_tracks(proj.path());
        let probe = crate::workflow::shipped::ScriptedMergedProbe::found("0001", 56);
        let _g = crate::workflow::shipped::install_test_merged_probe(std::sync::Arc::new(probe));
        let view = cmd_run(Some(&rec.id), None, Some("stub"), false, false).unwrap();
        assert_eq!(view.track_id.as_deref(), Some("0001"));
        clear_home();
    }

    #[test]
    fn resolve_run_track_with_probe_only_merged_ready_errors() {
        let _guard = test_env_lock();
        let (_home, proj, mut rec) = add_isolated_project();
        crate::workflow::conductor_md::write_ready_fixture(proj.path(), "0001").unwrap();
        rec.execution_repo = Some(proj.path().to_path_buf());
        let probe = crate::workflow::shipped::ScriptedMergedProbe::found("0001", 56);
        let err = resolve_run_track_with_probe(&rec, None, Some(&probe))
            .unwrap_err()
            .to_string();
        assert!(err.contains("pass --track"), "{err}");
        clear_home();
    }

    #[test]
    fn omit_track_rejects_sticky_proposed_after_idle() {
        let _guard = test_env_lock();
        let (_home, proj, rec) = add_isolated_project();
        let cond = proj.path().join("conductor");
        std::fs::create_dir_all(cond.join("0001-Done")).unwrap();
        std::fs::create_dir_all(cond.join("0002-Next")).unwrap();
        std::fs::write(
            cond.join("conductor.md"),
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0001-Done | `.` | **Completed** | done |\n\
             | 0002-Next | `.` | **Proposed — placeholder, needs full spec/plan pass** | later |\n",
        )
        .unwrap();
        let mut state = crate::state::load_run_state(&rec).unwrap();
        state.status = crate::state::RunStatus::Idle;
        state.track_id = Some("0001".into());
        state.next_track = None;
        state.failure_class = None;
        state.last_event = crate::workflow::LAST_EVENT_BACKLOG_CLEAR.into();
        state.sticky_ready_ids = vec!["0002".into()];
        crate::state::save_run_state(&rec, &state).unwrap();
        let err = resolve_run_track(&rec, None).unwrap_err().to_string();
        assert!(err.contains("no Ready"), "{err}");
        assert!(err.contains("--track"), "{err}");
        clear_home();
    }

    #[test]
    fn cmd_run_proposed_only_fail_closed() {
        let _guard = test_env_lock();
        let (_home, proj, rec) = add_isolated_project();
        let cond = proj.path().join("conductor");
        std::fs::create_dir_all(cond.join("0044-Later")).unwrap();
        std::fs::write(
            cond.join("conductor.md"),
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0044-Later | `.` | **Proposed — placeholder, needs full spec/plan pass** | no |\n",
        )
        .unwrap();
        let err = cmd_run(Some(&rec.id), None, Some("stub"), false, false).unwrap_err();
        assert!(err.to_string().contains("no Ready"), "{err}");
        let st = run::status(&rec).unwrap();
        assert_eq!(st.status, RunStatus::Idle);
        assert!(st.track_id.is_none());
        clear_home();
    }

    #[test]
    fn cmd_run_explicit_proposed_still_starts() {
        let _guard = test_env_lock();
        let (_home, proj, rec) = add_isolated_project();
        let cond = proj.path().join("conductor");
        std::fs::create_dir_all(cond.join("0044-Later")).unwrap();
        std::fs::write(
            cond.join("conductor.md"),
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0044-Later | `.` | **Proposed — placeholder, needs full spec/plan pass** | no |\n",
        )
        .unwrap();
        let view = cmd_run(
            Some(&rec.id),
            Some("0044".into()),
            Some("stub"),
            false,
            false,
        )
        .unwrap();
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.track_id.as_deref(), Some("0044"));
        assert!(
            !view.last_event.contains("(next Ready)"),
            "{}",
            view.last_event
        );
        clear_home();
    }

    #[test]
    fn cmd_run_stopped_omit_retains_despite_other_ready() {
        let _guard = test_env_lock();
        let (_home, proj, rec) = add_isolated_project();
        crate::workflow::conductor_md::write_ready_fixture(proj.path(), "0030").unwrap();
        cmd_run(
            Some(&rec.id),
            Some("0020".into()),
            Some("stub"),
            false,
            false,
        )
        .unwrap();
        cmd_stop(Some(&rec.id), false).unwrap();
        let view = cmd_run(Some(&rec.id), None, Some("stub"), false, false).unwrap();
        assert_eq!(view.track_id.as_deref(), Some("0020"));
        assert!(
            !view.last_event.contains("(next Ready)"),
            "{}",
            view.last_event
        );
        clear_home();
    }

    #[test]
    fn cmd_run_idle_failure_omit_retains() {
        let _guard = test_env_lock();
        let (_home, proj, rec) = add_isolated_project();
        crate::workflow::conductor_md::write_ready_fixture(proj.path(), "0030").unwrap();
        cmd_run(
            Some(&rec.id),
            Some("0020".into()),
            Some("stub"),
            false,
            false,
        )
        .unwrap();
        let mut state = crate::state::load_run_state(&rec).unwrap();
        state.status = RunStatus::Idle;
        state.failure_class = Some(crate::outcome::FailureClass::Timeout);
        state.last_event = "failure".into();
        crate::state::save_run_state(&rec, &state).unwrap();
        let view = cmd_run(Some(&rec.id), None, Some("stub"), false, false).unwrap();
        assert_eq!(view.track_id.as_deref(), Some("0020"));
        assert!(
            !view.last_event.contains("(next Ready)"),
            "{}",
            view.last_event
        );
        clear_home();
    }

    #[test]
    fn cmd_run_backlog_clear_omit_picks_next_ready() {
        let _guard = test_env_lock();
        unsafe {
            std::env::set_var(ENV_OUTCOME_POLL_MS, "10");
            std::env::set_var(ENV_PHASE_TIMEOUT_SECS, "30");
        }
        let (_home, proj, rec) = add_isolated_project();
        let _repo = attach_clean_exec(&rec);
        // 0020 is In progress so fold allows it and the later omit-pick stays on Ready 0030.
        let cond = proj.path().join("conductor");
        std::fs::create_dir_all(cond.join("0020-Fixture")).unwrap();
        std::fs::create_dir_all(cond.join("0030-Fixture")).unwrap();
        std::fs::write(
            cond.join("conductor.md"),
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0020-Fixture | `.` | **In progress** | current |\n\
             | 0030-Fixture | `.` | **Ready — not started** | next |\n",
        )
        .unwrap();
        let idle = cmd_run_cli(
            Some(&rec.id),
            Some("0020".into()),
            Some("stub"),
            RunCliOpts {
                detach: false,
                timeout_secs: None,
                probe: ServeProbe::Skip,
                skip_preflight: false,
            },
        )
        .unwrap();
        assert_eq!(idle.status, RunStatus::Idle);
        assert_eq!(idle.last_event, crate::workflow::LAST_EVENT_BACKLOG_CLEAR);
        assert_eq!(idle.track_id.as_deref(), Some("0020"));
        let view = cmd_run(Some(&rec.id), None, Some("stub"), false, false).unwrap();
        assert_eq!(view.track_id.as_deref(), Some("0030"));
        assert!(
            view.last_event.contains("(next Ready)"),
            "{}",
            view.last_event
        );
        unsafe {
            std::env::remove_var(ENV_OUTCOME_POLL_MS);
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
        }
        clear_home();
    }

    fn write_failure(rec: &ProjectRecord, track: &str, epoch: u64) {
        let event = crate::notify::NotifyEvent {
            project_id: rec.id.clone(),
            track_id: Some(track.into()),
            phase: "plan".into(),
            failure_class: crate::outcome::FailureClass::HarnessCrash,
            message: Some("test crash".into()),
            last_event: "test".into(),
            artifact_path: artifact::path(rec).unwrap(),
            written_at: chrono::Utc::now(),
            run_epoch: epoch,
        };
        artifact::write(rec, &event).unwrap();
    }

    #[test]
    fn project_show_previews_resolved_target_and_reason() {
        let _guard = test_env_lock();
        let (_home, proj, rec) = add_isolated_project();
        let conductor = proj.path().join("conductor");
        std::fs::create_dir_all(conductor.join("0091-Preview")).unwrap();
        std::fs::write(
            conductor.join("conductor.md"),
            "| Track | Execution | Status | Summary |\n| --- | --- | --- | --- |\n| [0091-Preview](0091-Preview/spec.md) | `.` | **Ready - not started** | preview |\n",
        )
        .unwrap();

        let view = project_show_with_track(Some(&rec.id), Some("0091-Preview"), false).unwrap();
        let target = view.resolved_track.expect("track preview");
        assert!(target.error.is_none(), "{target:?}");
        assert_eq!(target.track, "0091-Preview");
        assert_eq!(target.source.as_deref(), Some("row_execution"));
        assert!(target.declared_path.is_some());
        assert!(target.prompt_path.is_some());
        assert!(target.sample_path.is_some());

        let missing = project_show_with_track(Some(&rec.id), Some("0404-Missing"), false).unwrap();
        let target = missing.resolved_track.expect("track preview");
        assert!(
            target
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with(crate::workflow::execution_repo::RESOLUTION_PREFIX)),
            "{target:?}"
        );
        clear_home();
    }

    #[test]
    fn cmd_failure_show_reads_body_when_present() {
        let _guard = test_env_lock();
        let (_home, _proj, rec) = add_isolated_project();
        let mut state = crate::state::load_run_state(&rec).unwrap();
        state.status = RunStatus::Stopped;
        state.track_id = Some("0040".into());
        state.run_epoch = 2;
        state.failure_class = Some(crate::outcome::FailureClass::HarnessCrash);
        crate::state::save_run_state(&rec, &state).unwrap();
        write_failure(&rec, "0040", 2);
        let shown = cmd_failure_show(Some(&rec.id), false)
            .unwrap()
            .expect("artifact");
        assert!(shown.body.contains("track_id: 0040"));
        assert!(shown.body.contains("run_epoch: 2"));
        assert!(shown.superseded.is_none());
        clear_home();
    }

    #[test]
    fn cmd_failure_show_none_when_absent() {
        let _guard = test_env_lock();
        let (_home, _proj, rec) = add_isolated_project();
        assert!(cmd_failure_show(Some(&rec.id), false).unwrap().is_none());
        clear_home();
    }

    #[test]
    fn cmd_failure_resolve_clears_artifact_keeps_hash() {
        let _guard = test_env_lock();
        let (_home, proj, rec) = add_isolated_project();
        let mut state = crate::state::load_run_state(&rec).unwrap();
        state.status = RunStatus::Stopped;
        state.track_id = Some("0040".into());
        state.run_epoch = 2;
        state.failure_class = Some(crate::outcome::FailureClass::HarnessCrash);
        state.last_applied_outcome_hash = Some("keep-me".into());
        crate::state::save_run_state(&rec, &state).unwrap();
        write_failure(&rec, "0040", 2);
        let view = cmd_failure_resolve(Some(&rec.id), false).unwrap();
        assert!(artifact::existing_path(&rec).is_none());
        assert!(view.failure_class.is_none());
        assert_eq!(view.last_event, crate::notify::LAST_EVENT_FAILURE_RESOLVED);
        let loaded = crate::state::load_run_state(&rec).unwrap();
        assert_eq!(loaded.last_applied_outcome_hash.as_deref(), Some("keep-me"));
        assert_eq!(loaded.track_id.as_deref(), Some("0040"));
        assert_eq!(loaded.run_epoch, 2);
        let log = std::fs::read_to_string(crate::progress_log::path(&rec)).unwrap();
        assert!(log.contains(crate::notify::LAST_EVENT_FAILURE_RESOLVED));
        let pick = crate::workflow::ReadyPickState::from(&view);
        assert!(!crate::workflow::should_pick_next_ready(&pick));
        let _ = proj;
        clear_home();
    }

    #[test]
    fn cmd_failure_resolve_absent_is_ok() {
        let _guard = test_env_lock();
        let (_home, _proj, rec) = add_isolated_project();
        let view = cmd_failure_resolve(Some(&rec.id), false).unwrap();
        assert!(view.failure_class.is_none());
        assert_eq!(view.last_event, crate::notify::LAST_EVENT_FAILURE_RESOLVED);
        clear_home();
    }

    #[test]
    fn cmd_failure_resolve_running_is_invalid_transition() {
        let _guard = test_env_lock();
        let (_home, _proj, rec) = add_isolated_project();
        cmd_run(
            Some(&rec.id),
            Some("0040".into()),
            Some("file_wait"),
            false,
            false,
        )
        .unwrap();
        let err = cmd_failure_resolve(Some(&rec.id), false).unwrap_err();
        match err {
            CoordinatorError::InvalidTransition { action, from } => {
                assert_eq!(action, "failure resolve");
                assert_eq!(from, "Running");
            }
            other => panic!("expected InvalidTransition, got {other}"),
        }
        clear_home();
    }

    #[test]
    fn cmd_failure_show_superseded_when_conductor_completed() {
        let _guard = test_env_lock();
        let (_home, proj, rec) = add_isolated_project();
        let cond = proj.path().join("conductor");
        std::fs::create_dir_all(cond.join("0038-Done")).unwrap();
        std::fs::write(
            cond.join("conductor.md"),
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0038-Done](0038-Done/spec.md) | `.` | **Completed** | shipped |\n",
        )
        .unwrap();
        let mut state = crate::state::load_run_state(&rec).unwrap();
        state.status = RunStatus::Stopped;
        state.track_id = Some("0038".into());
        state.run_epoch = 2;
        state.failure_class = Some(crate::outcome::FailureClass::HarnessCrash);
        crate::state::save_run_state(&rec, &state).unwrap();
        write_failure(&rec, "0038", 2);
        let shown = cmd_failure_show(Some(&rec.id), false)
            .unwrap()
            .expect("artifact");
        assert_eq!(
            shown.superseded.as_deref(),
            Some(crate::notify::SUPERSEDED_COMPLETED)
        );
        assert!(shown.body.contains("track_id: 0038"));
        clear_home();
    }

    fn write_conductor_rows(proj: &std::path::Path, rows: &str) {
        let cond = proj.join("conductor");
        std::fs::create_dir_all(cond.join("0045-Done")).unwrap();
        std::fs::create_dir_all(cond.join("0046-Next")).unwrap();
        std::fs::write(
            cond.join("conductor.md"),
            format!(
                "| Track | Execution path | Status | Summary |\n\
                 | --- | --- | --- | --- |\n\
                 {rows}"
            ),
        )
        .unwrap();
    }

    fn stopped_failure(rec: &ProjectRecord, track: &str, epoch: u64) {
        let mut state = crate::state::load_run_state(rec).unwrap();
        state.status = RunStatus::Stopped;
        state.track_id = Some(track.into());
        state.run_epoch = epoch;
        state.failure_class = Some(crate::outcome::FailureClass::Timeout);
        crate::state::save_run_state(rec, &state).unwrap();
        write_failure(rec, track, epoch);
    }

    #[test]
    fn settle_completed_row_keeps_stopped_and_omit_picks_next() {
        let _guard = test_env_lock();
        let (_home, proj, rec) = add_isolated_project();
        write_conductor_rows(
            proj.path(),
            "| [0045-Done](0045-Done/spec.md) | `.` | **Completed** | shipped |\n\
             | [0046-Next](0046-Next/spec.md) | `.` | **Ready — not started** | next |\n",
        );
        stopped_failure(&rec, "0045", 3);
        assert!(settle_if_conductor_completed(&rec).unwrap());
        let state = crate::state::load_run_state(&rec).unwrap();
        assert_eq!(state.status, RunStatus::Stopped);
        assert!(state.failure_class.is_none());
        assert!(artifact::existing_path(&rec).is_none());
        assert_eq!(state.last_event, crate::notify::SETTLED_DETAIL);
        assert_eq!(state.track_id.as_deref(), Some("0045"));
        assert_eq!(state.run_epoch, 3);
        let log = std::fs::read_to_string(crate::progress_log::path(&rec)).unwrap();
        assert!(log.contains(crate::notify::SETTLED_DETAIL));
        assert!(cmd_failure_show(Some(&rec.id), false).unwrap().is_none());
        // 0063: a Completed retain is not resumable. Settle stays Stopped;
        // the next omit picks the Ready sibling instead of keeping 0045.
        let (track, picked) = resolve_run_track(&rec, None).unwrap();
        assert!(picked);
        assert_eq!(track.as_deref(), Some("0046"));
        assert!(!settle_if_conductor_completed(&rec).unwrap());
        let log = std::fs::read_to_string(crate::progress_log::path(&rec)).unwrap();
        assert_eq!(log.matches(crate::notify::SETTLED_DETAIL).count(), 1);
        let _ = proj;
        clear_home();
    }

    #[test]
    fn settle_then_explicit_track_starts_without_resolve() {
        let _guard = test_env_lock();
        let (_home, proj, rec) = add_isolated_project();
        write_conductor_rows(
            proj.path(),
            "| [0045-Done](0045-Done/spec.md) | `.` | **Completed** | shipped |\n\
             | [0046-Next](0046-Next/spec.md) | `.` | **Ready — not started** | next |\n",
        );
        stopped_failure(&rec, "0045", 3);
        let view = cmd_run(
            Some(&rec.id),
            Some("0046".into()),
            Some("file_wait"),
            false,
            false,
        )
        .unwrap();
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.track_id.as_deref(), Some("0046"));
        assert!(view.failure_class.is_none());
        assert!(artifact::existing_path(&rec).is_none());
        let log = std::fs::read_to_string(crate::progress_log::path(&rec)).unwrap();
        assert!(log.contains(crate::notify::SETTLED_DETAIL));
        let _ = proj;
        clear_home();
    }

    #[test]
    fn settle_skips_mismatched_and_uncompleted_tracks() {
        let _guard = test_env_lock();
        let (_home, proj, rec) = add_isolated_project();
        write_conductor_rows(
            proj.path(),
            "| [0045-Done](0045-Done/spec.md) | `.` | **Completed** | shipped |\n\
             | [0046-Next](0046-Next/spec.md) | `.` | **Ready — not started** | next |\n",
        );
        stopped_failure(&rec, "0046", 1);
        assert!(!settle_if_conductor_completed(&rec).unwrap());
        assert!(artifact::existing_path(&rec).is_some());
        assert_eq!(
            crate::state::load_run_state(&rec)
                .unwrap()
                .failure_class
                .as_ref()
                .map(|c| c.to_string())
                .as_deref(),
            Some("timeout")
        );
        let loaded = crate::state::load_run_state(&rec).unwrap();
        let view = crate::state::StatusView::from_record(&rec, &loaded);
        assert_eq!(
            crate::ui::model::card_state(&view),
            crate::ui::model::CardState::HardFailure
        );
        let _ = proj;
        clear_home();
    }

    #[test]
    fn run_start_journals_unsettled_leftover_clear() {
        let _guard = test_env_lock();
        let (_home, proj, rec) = add_isolated_project();
        write_conductor_rows(
            proj.path(),
            "| [0045-Done](0045-Done/spec.md) | `.` | **In progress** | wip |\n",
        );
        stopped_failure(&rec, "0045", 2);
        cmd_run(
            Some(&rec.id),
            Some("0045".into()),
            Some("file_wait"),
            false,
            false,
        )
        .unwrap();
        assert!(artifact::existing_path(&rec).is_none());
        let log = std::fs::read_to_string(crate::progress_log::path(&rec)).unwrap();
        assert!(log.contains(crate::notify::START_CLEAR_DETAIL));
        assert!(!log.contains(crate::notify::SETTLED_DETAIL));
        let _ = proj;
        clear_home();
    }

    fn api_git(cwd: &std::path::Path, args: &[&str]) -> String {
        let out = crate::git_command::command()
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn api_repo(root: &std::path::Path) -> std::path::PathBuf {
        std::fs::create_dir_all(root).unwrap();
        api_git(root, &["init", "-b", "main"]);
        std::fs::write(root.join("f.txt"), "base\n").unwrap();
        api_git(root, &["add", "."]);
        api_git(root, &["commit", "-m", "base"]);
        root.to_path_buf()
    }

    fn write_api_row(root: &std::path::Path, track: &str, target: &std::path::Path) {
        let conductor = root.join("conductor");
        std::fs::create_dir_all(conductor.join(format!("{track}-Example"))).unwrap();
        std::fs::write(
            conductor.join("conductor.md"),
            format!(
                "| Track | Execution | Status | Summary |\n| --- | --- | --- | --- |\n| [{track}-Example]({track}-Example/spec.md) | `{}` | **Ready - not started** | s |\n",
                target.display()
            ),
        )
        .unwrap();
    }

    fn register_primary_and_sibling(
        rec: &ProjectRecord,
        primary: &std::path::Path,
        sibling: &std::path::Path,
    ) {
        let mut reg = load_registry().unwrap();
        let row = reg.projects.iter_mut().find(|p| p.id == rec.id).unwrap();
        row.execution_repo = Some(primary.to_path_buf());
        row.execution_repos
            .insert("sibling".into(), sibling.to_path_buf());
        save_registry(&reg).unwrap();
    }

    fn save_delivery_state(rec: &ProjectRecord, track: &str, target: &std::path::Path) -> RunState {
        crate::state::ensure_state_dir(rec).unwrap();
        let mut state = RunState::idle(&rec.id);
        state.status = RunStatus::Running;
        state.phase = crate::workflow::graph::PHASE_CI_WAIT.into();
        state.track_id = Some(track.into());
        state.run_epoch = 1;
        state.delivery_target = Some(crate::workflow::execution_repo::seeded_delivery_target(
            rec,
            track,
            1,
            crate::workflow::graph::PHASE_CI_WAIT,
            target,
        ));
        state.ci = Some(crate::state::CiWatchState {
            target_repo: Some(target.to_path_buf()),
            ..Default::default()
        });
        crate::state::save_run_state(rec, &state).unwrap();
        state
    }

    #[test]
    fn policy_approve_reads_the_gate_repo() {
        let _guard = test_env_lock();
        let (_home, proj, rec) = add_isolated_project();
        let primary = api_repo(&proj.path().join("primary"));
        let sibling = api_repo(&proj.path().join("sibling"));
        register_primary_and_sibling(&rec, &primary, &sibling);
        write_api_row(proj.path(), "0010", &sibling);
        save_delivery_state(&rec, "0010", &sibling);
        // A checkpoint base in both repos; only the target adds a dependency manifest.
        for repo in [&primary, &sibling] {
            api_git(
                repo,
                &[
                    "update-ref",
                    &crate::checkpoint::ref_name(&rec.id, 1),
                    "HEAD",
                ],
            );
        }
        std::fs::write(sibling.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        api_git(&sibling, &["add", "Cargo.toml"]);
        api_git(&sibling, &["commit", "-m", "dependency"]);
        let got = cmd_policy_approve(Some(&rec.id), "dependency-manifest", false).unwrap();
        let mut expected_state = RunState::idle(&rec.id);
        expected_state.track_id = Some("0010".into());
        expected_state.run_epoch = 1;
        let expected = crate::policy::approve(
            &load_registry()
                .unwrap()
                .projects
                .into_iter()
                .find(|p| p.id == rec.id)
                .unwrap(),
            &mut expected_state,
            &sibling,
            "dependency-manifest",
        )
        .unwrap();
        assert_eq!(got, expected);
        let saved = crate::state::load_run_state(&rec).unwrap();
        assert!(
            saved
                .policy_approvals
                .iter()
                .any(|a| a.name == "dependency-manifest")
        );
        clear_home();
    }

    #[test]
    fn project_show_reports_publish_ownership() {
        let _guard = test_env_lock();
        let (_home, proj, rec) = add_isolated_project();
        let primary = api_repo(&proj.path().join("primary"));
        let sibling = api_repo(&proj.path().join("sibling"));
        let outside = api_repo(&proj.path().join("outside"));
        register_primary_and_sibling(&rec, &primary, &sibling);
        write_api_row(proj.path(), "0010", &sibling);
        let view = project_show_with_track(Some(&rec.id), Some("0010"), false).unwrap();
        let target = view.resolved_track.expect("preview");
        assert!(target.registered);
        assert!(target.publishable);
        write_api_row(proj.path(), "0010", &outside);
        let view = project_show_with_track(Some(&rec.id), Some("0010"), false).unwrap();
        let target = view.resolved_track.expect("preview");
        assert!(!target.registered);
        assert!(!target.publishable);
        clear_home();
    }

    fn write_blocked_row(proj: &std::path::Path, track: &str, status: &str) {
        let conductor = proj.join("conductor");
        std::fs::create_dir_all(&conductor).unwrap();
        std::fs::write(
            conductor.join("conductor.md"),
            format!(
                "| Track | Execution | Status | Summary |\n| --- | --- | --- | --- |\n| [{track}]({track}/spec.md) | `.` | {status} | blocked fixture |\n"
            ),
        )
        .unwrap();
    }

    fn blocked_fixture(track: &str) -> (tempfile::TempDir, tempfile::TempDir, ProjectRecord) {
        let (home, proj, rec) = add_isolated_project();
        write_blocked_row(proj.path(), track, "**In progress**");
        let mut state = crate::state::load_run_state(&rec).unwrap();
        state.status = RunStatus::Stopped;
        state.phase = crate::workflow::graph::PHASE_PLAN.into();
        state.track_id = Some(track.into());
        state.driver = crate::workflow::WorkflowDriver::Adapter;
        state.failure_class = Some(crate::outcome::FailureClass::Blocked);
        state.last_event =
            "outcome: failure class=blocked phase=plan source=adapter — prerequisite".into();
        state.run_epoch = 2;
        crate::state::save_run_state(&rec, &state).unwrap();
        let event = crate::notify::NotifyEvent {
            project_id: rec.id.clone(),
            track_id: Some(track.into()),
            phase: crate::workflow::graph::PHASE_PLAN.into(),
            failure_class: crate::outcome::FailureClass::Blocked,
            message: Some("prerequisite".into()),
            last_event: state.last_event.clone(),
            artifact_path: artifact::path(&rec).unwrap(),
            written_at: chrono::Utc::now(),
            run_epoch: 2,
        };
        artifact::write(&rec, &event).unwrap();
        (home, proj, rec)
    }

    fn assert_blocked_hold(err: CoordinatorError, track: &str, row: &str) {
        match err {
            CoordinatorError::BlockedHold {
                track: got,
                row_status,
            } => {
                assert_eq!(got, track);
                assert_eq!(row_status, row, "refusal carries the conductor row status");
            }
            other => panic!("expected BlockedHold, got {other:?}"),
        }
    }

    #[test]
    fn blocked_hold_refuses_cmd_run_for_every_driver_and_track_before_probe() {
        let _guard = test_env_lock();
        let (_home, _proj, rec) = blocked_fixture("0098");
        let before = crate::state::load_run_state(&rec).unwrap();
        for driver in ["adapter", "stub", "file_wait"] {
            for track in [Some("0098".to_string()), Some("0404".into()), None] {
                let err = cmd_run(Some(&rec.id), track, Some(driver), false, false).unwrap_err();
                assert_blocked_hold(err, "0098", "In progress");
            }
        }
        let after = crate::state::load_run_state(&rec).unwrap();
        assert_eq!(after.status, before.status);
        assert_eq!(after.phase, before.phase);
        assert_eq!(after.run_epoch, before.run_epoch);
        assert_eq!(after.failure_class, before.failure_class);
        assert!(artifact::existing_path(&rec).is_some());
        assert_eq!(
            CoordinatorError::BlockedHold {
                track: "0098".into(),
                row_status: "In progress".into()
            }
            .http_status(),
            409
        );
        clear_home();
    }

    #[test]
    fn direct_run_primitives_refuse_the_blocked_hold_without_mutation() {
        let _guard = test_env_lock();
        let (_home, _proj, rec) = blocked_fixture("0098");
        let before = crate::state::load_run_state(&rec).unwrap();
        let err = crate::run::run_with_origin(
            &rec,
            Some("0098".into()),
            crate::workflow::WorkflowDriver::Stub,
            false,
        )
        .unwrap_err();
        assert_blocked_hold(err, "0098", "In progress");
        let err = crate::run::run_stub(&rec, Some("0098".into())).unwrap_err();
        assert_blocked_hold(err, "0098", "In progress");
        let after = crate::state::load_run_state(&rec).unwrap();
        assert_eq!(after.status, before.status);
        assert_eq!(after.run_epoch, before.run_epoch);
        assert_eq!(after.failure_class, before.failure_class);
        clear_home();
    }

    #[test]
    fn completed_row_does_not_release_a_blocked_hold() {
        let _guard = test_env_lock();
        let (_home, proj, rec) = blocked_fixture("0098");
        write_blocked_row(proj.path(), "0098", "**Completed**");
        assert!(!settle_if_conductor_completed(&rec).unwrap());
        assert!(artifact::existing_path(&rec).is_some());
        let shown = cmd_failure_show(Some(&rec.id), false)
            .unwrap()
            .expect("artifact stays live");
        assert!(
            shown.superseded.is_none(),
            "a Blocked failure is not SUPERSEDED by a Completed row"
        );
        let err = cmd_run(
            Some(&rec.id),
            Some("0098".into()),
            Some("stub"),
            false,
            false,
        )
        .unwrap_err();
        assert_blocked_hold(err, "0098", "Completed");

        let view = cmd_failure_resolve(Some(&rec.id), false).unwrap();
        assert!(view.failure_class.is_none());
        assert!(artifact::existing_path(&rec).is_none());
        let running = cmd_run(
            Some(&rec.id),
            Some("0098".into()),
            Some("stub"),
            false,
            false,
        )
        .unwrap();
        assert_eq!(running.status, RunStatus::Running);
        clear_home();
    }

    #[test]
    #[allow(clippy::permissions_set_readonly_false)]
    fn blocked_resolve_save_fault_retains_hold_then_retry_succeeds() {
        let _guard = test_env_lock();
        let (_home, _proj, rec) = blocked_fixture("0098");
        let state_path = crate::state::resolve_state_dir(&rec)
            .unwrap()
            .join("run-state.json");
        let mut perms = std::fs::metadata(&state_path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&state_path, perms).unwrap();

        let err = cmd_failure_resolve(Some(&rec.id), false).unwrap_err();
        assert!(
            err.to_string().contains("I/O") || err.to_string().contains("access"),
            "{err}"
        );
        assert!(
            artifact::existing_path(&rec).is_some(),
            "a failed save leaves the artifact"
        );
        let held = crate::state::load_run_state(&rec).unwrap();
        assert_eq!(
            held.failure_class,
            Some(crate::outcome::FailureClass::Blocked),
            "a failed save leaves the on-disk hold"
        );

        let mut perms = std::fs::metadata(&state_path).unwrap().permissions();
        perms.set_readonly(false);
        std::fs::set_permissions(&state_path, perms).unwrap();
        let view = cmd_failure_resolve(Some(&rec.id), false).unwrap();
        assert!(view.failure_class.is_none());
        assert!(artifact::existing_path(&rec).is_none());
        clear_home();
    }

    #[test]
    fn another_project_is_not_held_by_a_blocked_project() {
        let _guard = test_env_lock();
        let (_home, _proj, rec) = blocked_fixture("0098");
        let other = tempdir().unwrap();
        let other_rec =
            project_add(other.path(), crate::registry::ProjectAddOptions::default()).unwrap();
        let view = cmd_run(
            Some(&other_rec.id),
            Some("0001".into()),
            Some("stub"),
            false,
            false,
        )
        .unwrap();
        assert_eq!(view.status, RunStatus::Running);
        let err = cmd_run(
            Some(&rec.id),
            Some("0098".into()),
            Some("stub"),
            false,
            false,
        )
        .unwrap_err();
        assert_blocked_hold(err, "0098", "In progress");
        clear_home();
    }
}
