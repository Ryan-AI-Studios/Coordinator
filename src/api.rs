//! Shared Control Plane operations for CLI and HTTP (no divergent logic).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::registry_path;
use crate::error::Result;
use crate::registry::{ProjectRecord, Registry};
use crate::run;
use crate::state::StatusView;

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
