//! Shared Control Plane operations for CLI and HTTP (no divergent logic).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::registry_path;
use crate::error::Result;
use crate::outcome::{
    self, FailureClass, OutcomeSource, OutcomeStatus, PhaseOutcome, load_current_outcome,
    parse_outcome_status, write_and_apply,
};
use crate::registry::{ProjectRecord, Registry};
use crate::run;
use crate::state::StatusView;
use crate::watch;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectAddRequest {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRefBody {
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub track: Option<String>,
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

/// `project add <path>`
pub fn project_add(path: &Path) -> Result<ProjectRecord> {
    let mut reg = load_registry()?;
    let rec = reg.add(path)?;
    save_registry(&reg)?;
    Ok(rec)
}

/// `project list`
pub fn project_list() -> Result<Vec<ProjectRecord>> {
    Ok(load_registry()?.list().to_vec())
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
