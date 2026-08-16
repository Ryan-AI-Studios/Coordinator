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
    pub phase_timeouts_secs: Option<BTreeMap<String, u64>>,
    #[serde(default)]
    pub clear_phase_timeouts: Option<bool>,
    #[serde(default)]
    pub clear_phase_timeout: Option<Vec<String>>,
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
        auto_merge: req.auto_merge,
        phase_timeouts_secs: req.phase_timeouts_secs.clone().unwrap_or_default(),
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
    let mut phase_timeouts = BTreeMap::new();
    for phase in crate::workflow::graph::canonical_phases() {
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
    Ok(ProjectShowView {
        project: rec,
        resolved,
        hint,
        phase_timeouts,
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
        auto_merge: req.auto_merge,
        phase_timeouts_secs: req.phase_timeouts_secs,
        clear_phase_timeouts: req.clear_phase_timeouts.unwrap_or(false),
        clear_phase_timeout: req.clear_phase_timeout.unwrap_or_default(),
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

pub fn cmd_run(
    project: Option<&str>,
    track: Option<String>,
    driver: Option<&str>,
) -> Result<StatusView> {
    let reg = load_registry()?;
    let rec = reg.resolve_project(project)?.clone();
    let driver = crate::workflow::resolve_driver(driver)?;
    run::run_with_driver(&rec, track, driver)
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
            ..Default::default()
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

/// Show `{state_dir}/FAILURE.md` if present (CLI / GET /v1/failure).
pub fn cmd_failure_show(project: Option<&str>) -> Result<Option<crate::notify::FailureShow>> {
    let reg = load_registry()?;
    let rec = reg.resolve_project(project)?;
    crate::notify::artifact::read(rec)
}

/// Block until outcome applied or wait budget expires.
pub fn cmd_wait(project: Option<&str>, timeout_secs: u64) -> Result<StatusView> {
    let reg = load_registry()?;
    let rec = reg.resolve_project(project)?.clone();
    watch::wait_for_outcome(&rec, Some(timeout_secs))
}

/// CLI-only `run` options. HTTP / UI / [`cmd_run`] stay write-only.
#[derive(Debug, Clone)]
pub struct RunCliOpts {
    pub detach: bool,
    pub timeout_secs: Option<u64>,
    pub detect_serve_port: Option<u16>,
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
    let view = cmd_run(project, track, driver)?;
    if opts.detach {
        return Ok(view);
    }
    if let Some(port) = opts.detect_serve_port
        && watch::coordinator_serve_listening(port)
    {
        eprintln!("serve owns the ticker on 127.0.0.1:{port}; not waiting");
        return Ok(view);
    }
    let reg = load_registry()?;
    let rec = reg.resolve_project(project)?.clone();
    watch::wait_for_outcome(&rec, opts.timeout_secs)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ENV_COORDINATOR_HOME, ENV_OUTCOME_POLL_MS, ENV_STUB_PHASE_TIMEOUT_SECS, test_env_lock,
    };
    use crate::notify::artifact;
    use crate::state::RunStatus;
    use crate::workflow::ENV_PHASE_TIMEOUT_SECS;
    use tempfile::tempdir;

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
        unsafe {
            std::env::set_var(ENV_OUTCOME_POLL_MS, "10");
            std::env::set_var(ENV_PHASE_TIMEOUT_SECS, "30");
        }
        let (_home, _proj, rec) = add_isolated_project();
        let view = cmd_run_cli(
            Some(&rec.id),
            Some("0020".into()),
            Some("stub"),
            RunCliOpts {
                detach: false,
                timeout_secs: None,
                detect_serve_port: None,
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
    fn cmd_run_cli_detach_stays_running_at_plan() {
        let _guard = test_env_lock();
        let (_home, _proj, rec) = add_isolated_project();
        let view = cmd_run_cli(
            Some(&rec.id),
            Some("0020".into()),
            Some("stub"),
            RunCliOpts {
                detach: true,
                timeout_secs: None,
                detect_serve_port: None,
            },
        )
        .unwrap();
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.phase, crate::workflow::graph::PHASE_PLAN);
        clear_home();
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
                detect_serve_port: None,
            },
        )
        .unwrap_err();
        assert!(
            matches!(err, CoordinatorError::WaitBudgetExpired),
            "err={err}"
        );
        let s = status(Some(&rec.id)).unwrap();
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
                detect_serve_port: None,
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("timeout-secs must be > 0"),
            "err={err}"
        );
        let s = status(Some(&rec.id)).unwrap();
        assert_eq!(s.status, RunStatus::Idle);
        clear_home();
    }

    #[test]
    fn cmd_run_cli_serve_owns_skips_wait() {
        let _guard = test_env_lock();
        let (_home, _proj, rec) = add_isolated_project();
        let port = spawn_health_once(r#"{"ok":true,"service":"coordinator"}"#);
        let view = cmd_run_cli(
            Some(&rec.id),
            Some("0020".into()),
            Some("file_wait"),
            RunCliOpts {
                detach: false,
                timeout_secs: Some(1),
                detect_serve_port: Some(port),
            },
        )
        .unwrap();
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.phase, crate::workflow::graph::PHASE_PLAN);
        assert!(artifact::existing_path(&rec).is_none());
        clear_home();
    }

    #[test]
    fn cmd_run_cli_non_coordinator_health_ticks() {
        let _guard = test_env_lock();
        unsafe {
            std::env::set_var(ENV_OUTCOME_POLL_MS, "10");
            std::env::set_var(ENV_PHASE_TIMEOUT_SECS, "30");
        }
        let (_home, _proj, rec) = add_isolated_project();
        let port = spawn_health_once(r#"{"ok":true}"#);
        let view = cmd_run_cli(
            Some(&rec.id),
            Some("0020".into()),
            Some("stub"),
            RunCliOpts {
                detach: false,
                timeout_secs: None,
                detect_serve_port: Some(port),
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
                detect_serve_port: None,
            },
        )
        .unwrap();
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.phase, crate::workflow::graph::PHASE_PLAN);
        clear_home();
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
        )
        .unwrap();
        let view = project_show(Some(&rec.id)).unwrap();
        for phase in crate::workflow::graph::canonical_phases() {
            assert!(
                view.phase_timeouts.contains_key(*phase),
                "show missing effective timeout for {phase}"
            );
        }
        assert_eq!(view.phase_timeouts["plan"].secs, 3600);
        assert_eq!(view.phase_timeouts["plan"].source, "project");
        assert_eq!(view.phase_timeouts["implement"].secs, 7200);
        assert_eq!(view.phase_timeouts["implement"].source, "table");
        assert_eq!(view.project.phase_timeouts_secs.get("plan"), Some(&3600));
        unsafe {
            std::env::set_var(ENV_PHASE_TIMEOUT_SECS, "7");
        }
        let view = project_show(Some(&rec.id)).unwrap();
        assert_eq!(view.phase_timeouts["plan"].secs, 7);
        assert_eq!(view.phase_timeouts["plan"].source, "env");
        assert_eq!(view.phase_timeouts["implement"].source, "env");
        unsafe {
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
        }
        clear_home();
    }
}
