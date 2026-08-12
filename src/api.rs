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
}

/// Show response: raw record + resolved paths + optional remediation hint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectShowView {
    pub project: ProjectRecord,
    pub resolved: WorkspacePaths,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
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
pub fn project_show(project: Option<&str>) -> Result<ProjectShowView> {
    let reg = load_registry()?;
    let rec = reg.resolve_project(project)?.clone();
    let resolved = layout::resolve(&rec);
    let hint = if rec.layout_profile == LayoutProfile::Nested && rec.execution_repo.is_none() {
        let spec = project
            .map(|s| s.to_string())
            .unwrap_or_else(|| rec.path.display().to_string());
        Some(nested_execution_null_hint(&spec))
    } else {
        None
    };
    Ok(ProjectShowView {
        project: rec,
        resolved,
        hint,
    })
}

/// `project set` — mutate bindings; workspace path immutable.
pub fn project_set(project: Option<&str>, opts: ProjectSetOptions) -> Result<ProjectRecord> {
    let mut reg = load_registry()?;
    let id = reg.resolve_project(project)?.id.clone();
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
    };
    project_set(req.project.as_deref(), opts)
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

/// Resolve project selector then return status view.
pub fn status(project: Option<&str>) -> Result<StatusView> {
    let reg = load_registry()?;
    let rec = reg.resolve_project(project)?;
    run::status(rec)
}

/// Status for all projects (aggregate).
pub fn status_all() -> Result<Vec<StatusView>> {
    let reg = load_registry()?;
    let mut out = Vec::with_capacity(reg.projects.len());
    for rec in reg.list() {
        out.push(run::status(rec)?);
    }
    Ok(out)
}

pub fn cmd_run(project: Option<&str>, track: Option<String>) -> Result<StatusView> {
    let reg = load_registry()?;
    let rec = reg.resolve_project(project)?.clone();
    run::run(&rec, track)
}

pub fn cmd_pause(project: Option<&str>) -> Result<StatusView> {
    let reg = load_registry()?;
    let rec = reg.resolve_project(project)?.clone();
    run::pause(&rec)
}

pub fn cmd_resume(project: Option<&str>) -> Result<StatusView> {
    let reg = load_registry()?;
    let rec = reg.resolve_project(project)?.clone();
    run::resume(&rec)
}

pub fn cmd_stop(project: Option<&str>) -> Result<StatusView> {
    let reg = load_registry()?;
    let rec = reg.resolve_project(project)?.clone();
    run::stop(&rec)
}

/// CLI/HTTP: write Phase Outcome and apply via the single apply path.
pub fn cmd_outcome_write(
    project: Option<&str>,
    phase: String,
    status: &str,
    failure_class: Option<&str>,
    message: Option<String>,
    next_track: Option<String>,
    source: Option<&str>,
) -> Result<StatusView> {
    let reg = load_registry()?;
    let rec = reg.resolve_project(project)?.clone();
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
            role: None,
        }),
        run_epoch: None,
    };
    outcome.validate()?;
    write_and_apply(&rec, outcome)
}

/// Build outcome from HTTP body and apply.
pub fn cmd_outcome_post(body: OutcomeWriteBody) -> Result<StatusView> {
    let reg = load_registry()?;
    let rec = reg.resolve_project(body.project.as_deref())?.clone();
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
pub fn cmd_outcome_show(project: Option<&str>) -> Result<Option<PhaseOutcome>> {
    let reg = load_registry()?;
    let rec = reg.resolve_project(project)?;
    load_current_outcome(rec)
}

/// Block until outcome applied or wait budget expires.
pub fn cmd_wait(project: Option<&str>, timeout_secs: u64) -> Result<StatusView> {
    let reg = load_registry()?;
    let rec = reg.resolve_project(project)?.clone();
    watch::wait_for_outcome(&rec, timeout_secs)
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
) -> Result<crate::harness::GrokHarnessStatus> {
    crate::harness::start(project, in_process).await
}

pub async fn cmd_harness_grok_prompt(
    project: Option<&str>,
    text: String,
) -> Result<crate::harness::HarnessPromptView> {
    crate::harness::prompt(project, text).await
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
    crate::harness::prompt(body.project.as_deref(), text).await
}

pub async fn cmd_harness_grok_compact(
    project: Option<&str>,
) -> Result<crate::harness::HarnessPromptView> {
    crate::harness::compact(project).await
}

pub async fn cmd_harness_grok_status(
    project: Option<&str>,
) -> Result<crate::harness::GrokHarnessStatus> {
    crate::harness::grok_status(project).await
}

pub async fn cmd_harness_grok_shutdown(
    project: Option<&str>,
) -> Result<crate::harness::GrokHarnessStatus> {
    crate::harness::shutdown(project).await
}

pub async fn cmd_harness_grok_hold(project: Option<&str>) -> Result<()> {
    crate::harness::hold(project).await
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
