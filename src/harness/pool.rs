//! Session pool keyed by `project_id` plus CLI holder + persist metadata.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::error::{CoordinatorError, Result};
use crate::harness::grok::{GrokSession, PromptResult, map_failure_class};
use crate::harness::{grok_cwd, resolve_grok_binary};
use crate::outcome::{FailureClass, OutcomeSource, PhaseOutcome, write_and_apply};
use crate::persist::atomic_write_json;
use crate::registry::ProjectRecord;
use crate::state::{RunStatus, StatusView, ensure_state_dir, load_run_state, resolve_state_dir};

/// In-process pool (HTTP `serve` + tests). CLI uses a detached holder.
static POOL: OnceLock<tokio::sync::Mutex<SessionPool>> = OnceLock::new();

pub fn global_pool() -> &'static tokio::sync::Mutex<SessionPool> {
    POOL.get_or_init(|| tokio::sync::Mutex::new(SessionPool::new()))
}

#[derive(Debug, Default)]
pub struct SessionPool {
    sessions: HashMap<String, GrokSession>,
}

impl SessionPool {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }

    pub fn insert(&mut self, project_id: String, session: GrokSession) {
        self.sessions.insert(project_id, session);
    }

    pub fn get_mut(&mut self, project_id: &str) -> Option<&mut GrokSession> {
        self.sessions.get_mut(project_id)
    }

    pub fn remove(&mut self, project_id: &str) -> Option<GrokSession> {
        self.sessions.remove(project_id)
    }

    pub fn contains(&self, project_id: &str) -> bool {
        self.sessions.contains_key(project_id)
    }

    pub fn status_of(&mut self, project_id: &str) -> Option<GrokHarnessStatus> {
        let s = self.sessions.get_mut(project_id)?;
        Some(GrokHarnessStatus {
            alive: s.is_process_alive(),
            session_id: Some(s.session_id.clone()),
            cwd: Some(s.cwd.clone()),
            supports_compact: s.supports_compact,
            pid: s.pid,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GrokHarnessStatus {
    pub alive: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    pub supports_compact: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

impl GrokHarnessStatus {
    pub fn missing() -> Self {
        Self {
            alive: false,
            session_id: None,
            cwd: None,
            supports_compact: false,
            pid: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarnessStatusBundle {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grok: Option<GrokHarnessStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarnessPromptView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    pub applied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<FailureClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<StatusView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<GrokHarnessStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedGrokHandle {
    version: u32,
    project_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    holder_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    control_addr: Option<String>,
    #[serde(default)]
    supports_compact: bool,
    alive: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl PersistedGrokHandle {
    fn to_status(&self) -> GrokHarnessStatus {
        GrokHarnessStatus {
            alive: self.alive,
            session_id: self.session_id.clone(),
            cwd: self.cwd.clone(),
            supports_compact: self.supports_compact,
            pid: self.pid,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum HoldRequest {
    Ping,
    Prompt { text: String },
    Compact,
    Status,
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HoldResponse {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    status: Option<StatusView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    harness: Option<GrokHarnessStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    applied: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    skipped: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failure_class: Option<FailureClass>,
}

pub fn persist_path(record: &ProjectRecord) -> Result<PathBuf> {
    Ok(resolve_state_dir(record)?.join("harness-grok.json"))
}

fn load_persist(record: &ProjectRecord) -> Result<Option<PersistedGrokHandle>> {
    let path = persist_path(record)?;
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    Ok(Some(serde_json::from_str(&text)?))
}

fn save_persist(record: &ProjectRecord, handle: &PersistedGrokHandle) -> Result<()> {
    ensure_state_dir(record)?;
    atomic_write_json(&persist_path(record)?, handle)
}

fn write_session_persist(
    record: &ProjectRecord,
    session: &GrokSession,
    control_addr: Option<String>,
) {
    let handle = PersistedGrokHandle {
        version: 1,
        project_id: record.id.clone(),
        session_id: Some(session.session_id.clone()),
        cwd: Some(session.cwd.clone()),
        pid: session.pid,
        holder_pid: Some(std::process::id()),
        control_addr,
        supports_compact: session.supports_compact,
        alive: true,
        error: None,
    };
    let _ = save_persist(record, &handle);
}

/// Sync snapshot for `StatusView` (in-process pool, else persist file).
pub fn status_bundle_sync(record: &ProjectRecord) -> Option<HarnessStatusBundle> {
    if let Some(pool) = POOL.get()
        && let Ok(mut guard) = pool.try_lock()
        && let Some(s) = guard.status_of(&record.id)
    {
        return Some(HarnessStatusBundle { grok: Some(s) });
    }
    match load_persist(record) {
        Ok(Some(h)) if h.session_id.is_some() || h.alive => Some(HarnessStatusBundle {
            grok: Some(h.to_status()),
        }),
        _ => None,
    }
}

fn resolve_record(project: Option<&str>) -> Result<ProjectRecord> {
    let reg = crate::api::load_registry()?;
    Ok(reg.resolve_project(project)?.clone())
}

fn prompt_timeout() -> Duration {
    crate::config::stub_phase_timeout()
}

fn refuse_if_paused(record: &ProjectRecord) -> Result<()> {
    let state = load_run_state(record)?;
    if state.status == RunStatus::Paused {
        return Err(CoordinatorError::InvalidTransition {
            action: "harness prompt",
            from: "Paused".into(),
        });
    }
    Ok(())
}

async fn apply_turn(
    record: &ProjectRecord,
    turn: std::result::Result<PromptResult, CoordinatorError>,
    harness: GrokHarnessStatus,
) -> Result<HarnessPromptView> {
    let state = load_run_state(record)?;
    match turn {
        Ok(pr) => {
            let mut applied = false;
            let mut status = None;
            if state.status == RunStatus::Running {
                let msg = if pr.text.is_empty() {
                    None
                } else {
                    Some(pr.text.clone())
                };
                let outcome = PhaseOutcome::success(
                    state.phase.clone(),
                    OutcomeSource::Adapter,
                    msg,
                    None,
                    Some(state.run_epoch),
                );
                status = Some(write_and_apply(record, outcome)?);
                applied = true;
            }
            Ok(HarnessPromptView {
                text: Some(pr.text),
                stop_reason: pr.stop_reason,
                applied,
                skipped: None,
                error: None,
                failure_class: None,
                status,
                harness: Some(harness),
            })
        }
        Err(e) => {
            let class = map_failure_class(&e.to_string());
            let mut status = None;
            let mut applied = false;
            if state.status == RunStatus::Running {
                let outcome = PhaseOutcome::failure(
                    state.phase.clone(),
                    class,
                    OutcomeSource::Adapter,
                    Some(e.to_string()),
                    Some(state.run_epoch),
                );
                status = Some(write_and_apply(record, outcome)?);
                applied = true;
            }
            Ok(HarnessPromptView {
                text: None,
                stop_reason: None,
                applied,
                skipped: None,
                error: Some(e.to_string()),
                failure_class: Some(class),
                status,
                harness: Some(harness),
            })
        }
    }
}

async fn spawn_in_process(record: &ProjectRecord) -> Result<GrokHarnessStatus> {
    let cwd = grok_cwd(record);
    let session = GrokSession::start(cwd, prompt_timeout()).await?;
    write_session_persist(record, &session, None);
    let status = GrokHarnessStatus {
        alive: true,
        session_id: Some(session.session_id.clone()),
        cwd: Some(session.cwd.clone()),
        supports_compact: session.supports_compact,
        pid: session.pid,
    };
    global_pool()
        .lock()
        .await
        .insert(record.id.clone(), session);
    Ok(status)
}

pub async fn start(project: Option<&str>, in_process: bool) -> Result<GrokHarnessStatus> {
    let rec = resolve_record(project)?;
    {
        let mut pool = global_pool().lock().await;
        if let Some(s) = pool.status_of(&rec.id)
            && s.alive
        {
            return Ok(s);
        }
    }
    if let Some(existing) = load_persist(&rec)?
        && existing.alive
        && let Some(addr) = &existing.control_addr
        && holder_rpc(addr, &HoldRequest::Ping).await.is_ok()
    {
        return Ok(existing.to_status());
    }

    if in_process {
        return spawn_in_process(&rec).await;
    }
    start_holder(&rec, project).await
}

/// Drop leftover persist so a retry does not treat a prior start failure as current.
fn clear_stale_holder_persist(record: &ProjectRecord) -> Result<()> {
    let path = persist_path(record)?;
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
    Ok(())
}

/// After persist was cleared, interpret a newly written handle.
#[derive(Debug, PartialEq, Eq)]
enum PersistWait {
    Pending,
    Ready,
    Failed(String),
}

fn interpret_persist_after_spawn(h: &PersistedGrokHandle) -> PersistWait {
    if let Some(err) = &h.error {
        return PersistWait::Failed(err.clone());
    }
    if h.alive && h.control_addr.is_some() {
        return PersistWait::Ready;
    }
    PersistWait::Pending
}

async fn start_holder(record: &ProjectRecord, project: Option<&str>) -> Result<GrokHarnessStatus> {
    // Fail fast if grok cannot be resolved (holder would exit immediately).
    let _ = resolve_grok_binary()?;
    // A previous failed start writes `error` into harness-grok.json. Clear it
    // before spawn so we never return that stale error on retry.
    clear_stale_holder_persist(record)?;
    spawn_holder_process(project.unwrap_or(record.path.to_str().unwrap_or(&record.id)))?;

    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    loop {
        if let Ok(Some(h)) = load_persist(record) {
            match interpret_persist_after_spawn(&h) {
                PersistWait::Failed(err) => return Err(CoordinatorError::Message(err)),
                PersistWait::Ready => {
                    if let Some(addr) = &h.control_addr
                        && holder_rpc(addr, &HoldRequest::Ping).await.is_ok()
                    {
                        return Ok(h.to_status());
                    }
                }
                PersistWait::Pending => {}
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(CoordinatorError::Message(
                "timed out waiting for Grok holder to become ready".into(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
}

fn spawn_holder_process(project_spec: &str) -> Result<()> {
    let exe = std::env::current_exe().map_err(|e| {
        CoordinatorError::Message(format!("cannot resolve coordinator executable: {e}"))
    })?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("harness")
        .arg("grok")
        .arg("hold")
        .arg("--project")
        .arg(project_spec)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
        cmd.creation_flags(0x0000_0008 | 0x0000_0200 | 0x0800_0000);
    }
    cmd.spawn()
        .map_err(|e| CoordinatorError::Message(format!("failed to spawn grok holder: {e}")))?;
    Ok(())
}

pub async fn hold_loop(project: Option<&str>) -> Result<()> {
    let rec = match resolve_record(project) {
        Ok(r) => r,
        Err(e) => return Err(e),
    };
    let cwd = grok_cwd(&rec);
    let mut session = match GrokSession::start(cwd, prompt_timeout()).await {
        Ok(s) => s,
        Err(e) => {
            let handle = PersistedGrokHandle {
                version: 1,
                project_id: rec.id.clone(),
                session_id: None,
                cwd: None,
                pid: None,
                holder_pid: Some(std::process::id()),
                control_addr: None,
                supports_compact: true,
                alive: false,
                error: Some(e.to_string()),
            };
            let _ = save_persist(&rec, &handle);
            return Err(e);
        }
    };

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| CoordinatorError::Message(format!("holder bind 127.0.0.1:0 failed: {e}")))?;
    let addr = listener.local_addr()?.to_string();
    write_session_persist(&rec, &session, Some(addr));

    loop {
        let (stream, _) = listener.accept().await?;
        match handle_hold_conn(stream, &mut session, &rec).await {
            Ok(true) => break,
            Ok(false) => {}
            Err(_) => {}
        }
    }
    let _ = session.shutdown().await;
    let dead = PersistedGrokHandle {
        version: 1,
        project_id: rec.id.clone(),
        session_id: Some(session.session_id.clone()),
        cwd: Some(session.cwd.clone()),
        pid: session.pid,
        holder_pid: Some(std::process::id()),
        control_addr: None,
        supports_compact: session.supports_compact,
        alive: false,
        error: None,
    };
    let _ = save_persist(&rec, &dead);
    Ok(())
}

async fn handle_hold_conn(
    stream: TcpStream,
    session: &mut GrokSession,
    record: &ProjectRecord,
) -> Result<bool> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let Some(line) = lines.next_line().await? else {
        return Ok(false);
    };
    let req: HoldRequest = serde_json::from_str(&line)?;
    let (resp, shutdown) = match req {
        HoldRequest::Ping => (
            HoldResponse {
                ok: true,
                error: None,
                text: None,
                stop_reason: None,
                status: None,
                harness: Some(session_status(session)),
                applied: None,
                skipped: None,
                failure_class: None,
            },
            false,
        ),
        HoldRequest::Status => (
            HoldResponse {
                ok: true,
                error: None,
                text: None,
                stop_reason: None,
                status: None,
                harness: Some(session_status(session)),
                applied: None,
                skipped: None,
                failure_class: None,
            },
            false,
        ),
        HoldRequest::Prompt { text } => match refuse_if_paused(record) {
            Err(e) => (
                HoldResponse {
                    ok: false,
                    error: Some(e.to_string()),
                    text: None,
                    stop_reason: None,
                    status: None,
                    harness: Some(session_status(session)),
                    applied: None,
                    skipped: None,
                    failure_class: None,
                },
                false,
            ),
            Ok(()) => {
                let turn = session.inject_prompt(&text, prompt_timeout()).await;
                let view = apply_turn(record, turn, session_status(session)).await?;
                (view_to_hold(view), false)
            }
        },
        HoldRequest::Compact => {
            if !session.supports_compact {
                (
                    HoldResponse {
                        ok: true,
                        error: None,
                        text: None,
                        stop_reason: None,
                        status: None,
                        harness: Some(session_status(session)),
                        applied: Some(false),
                        skipped: Some(true),
                        failure_class: None,
                    },
                    false,
                )
            } else {
                match session.compact(prompt_timeout()).await {
                    Ok(pr) => (
                        HoldResponse {
                            ok: true,
                            error: None,
                            text: Some(pr.text),
                            stop_reason: pr.stop_reason,
                            status: None,
                            harness: Some(session_status(session)),
                            applied: Some(false),
                            skipped: None,
                            failure_class: None,
                        },
                        false,
                    ),
                    Err(e) => (
                        HoldResponse {
                            ok: false,
                            error: Some(e.to_string()),
                            text: None,
                            stop_reason: None,
                            status: None,
                            harness: Some(session_status(session)),
                            applied: Some(false),
                            skipped: None,
                            failure_class: Some(map_failure_class(&e.to_string())),
                        },
                        false,
                    ),
                }
            }
        }
        HoldRequest::Shutdown => (
            HoldResponse {
                ok: true,
                error: None,
                text: None,
                stop_reason: None,
                status: None,
                harness: Some(GrokHarnessStatus {
                    alive: false,
                    session_id: Some(session.session_id.clone()),
                    cwd: Some(session.cwd.clone()),
                    supports_compact: session.supports_compact,
                    pid: session.pid,
                }),
                applied: None,
                skipped: None,
                failure_class: None,
            },
            true,
        ),
    };
    let payload = serde_json::to_string(&resp)?;
    writer.write_all(payload.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(shutdown)
}

fn session_status(session: &GrokSession) -> GrokHarnessStatus {
    GrokHarnessStatus {
        alive: true,
        session_id: Some(session.session_id.clone()),
        cwd: Some(session.cwd.clone()),
        supports_compact: session.supports_compact,
        pid: session.pid,
    }
}

fn view_to_hold(view: HarnessPromptView) -> HoldResponse {
    HoldResponse {
        ok: view.error.is_none(),
        error: view.error,
        text: view.text,
        stop_reason: view.stop_reason,
        status: view.status,
        harness: view.harness,
        applied: Some(view.applied),
        skipped: view.skipped,
        failure_class: view.failure_class,
    }
}

async fn holder_rpc(addr: &str, req: &HoldRequest) -> Result<HoldResponse> {
    let stream = TcpStream::connect(addr)
        .await
        .map_err(|e| CoordinatorError::Message(format!("holder connect {addr}: {e}")))?;
    let (reader, mut writer) = stream.into_split();
    let payload = serde_json::to_string(req)?;
    writer.write_all(payload.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    let mut lines = BufReader::new(reader).lines();
    let line = lines
        .next_line()
        .await?
        .ok_or_else(|| CoordinatorError::Message("holder closed without response".into()))?;
    Ok(serde_json::from_str(&line)?)
}

fn hold_view(resp: HoldResponse) -> Result<HarnessPromptView> {
    // If the holder already applied a failure outcome, return the structured
    // view (same as the in-process path) instead of dropping it as a hard error.
    if !resp.ok && resp.applied != Some(true) {
        return Err(CoordinatorError::Message(
            resp.error.unwrap_or_else(|| "holder request failed".into()),
        ));
    }
    Ok(HarnessPromptView {
        text: resp.text,
        stop_reason: resp.stop_reason,
        applied: resp.applied.unwrap_or(false),
        skipped: resp.skipped,
        error: resp.error,
        failure_class: resp.failure_class,
        status: resp.status,
        harness: resp.harness,
    })
}

pub async fn prompt(project: Option<&str>, text: String) -> Result<HarnessPromptView> {
    let rec = resolve_record(project)?;
    refuse_if_paused(&rec)?;
    if let Some(h) = load_persist(&rec)?
        && h.alive
        && let Some(addr) = &h.control_addr
        && !global_pool().lock().await.contains(&rec.id)
    {
        let resp = holder_rpc(addr, &HoldRequest::Prompt { text }).await?;
        return hold_view(resp);
    }
    let turn = {
        let mut pool = global_pool().lock().await;
        let session = pool.get_mut(&rec.id).ok_or_else(|| {
            CoordinatorError::Message(
                "no Grok session; run `coordinator harness grok start` first".into(),
            )
        })?;
        session.inject_prompt(&text, prompt_timeout()).await
    };
    let harness = current_status(&rec).await;
    apply_turn(&rec, turn, harness).await
}

pub async fn compact(project: Option<&str>) -> Result<HarnessPromptView> {
    let rec = resolve_record(project)?;
    refuse_if_paused(&rec)?;
    if let Some(h) = load_persist(&rec)?
        && h.alive
        && let Some(addr) = &h.control_addr
        && !global_pool().lock().await.contains(&rec.id)
    {
        let resp = holder_rpc(addr, &HoldRequest::Compact).await?;
        return hold_view(resp);
    }
    let mut pool = global_pool().lock().await;
    let session = pool.get_mut(&rec.id).ok_or_else(|| {
        CoordinatorError::Message(
            "no Grok session; run `coordinator harness grok start` first".into(),
        )
    })?;
    if !session.supports_compact {
        return Ok(HarnessPromptView {
            text: None,
            stop_reason: None,
            applied: false,
            skipped: Some(true),
            error: None,
            failure_class: None,
            status: None,
            harness: Some(session_status(session)),
        });
    }
    // Compact is not a phase-completion signal (ADR-0021 skip-not-fail).
    match session.compact(prompt_timeout()).await {
        Ok(pr) => Ok(HarnessPromptView {
            text: Some(pr.text),
            stop_reason: pr.stop_reason,
            applied: false,
            skipped: None,
            error: None,
            failure_class: None,
            status: None,
            harness: Some(session_status(session)),
        }),
        Err(e) => Ok(HarnessPromptView {
            text: None,
            stop_reason: None,
            applied: false,
            skipped: None,
            error: Some(e.to_string()),
            failure_class: Some(map_failure_class(&e.to_string())),
            status: None,
            harness: Some(session_status(session)),
        }),
    }
}

pub async fn status(project: Option<&str>) -> Result<GrokHarnessStatus> {
    let rec = resolve_record(project)?;
    Ok(current_status(&rec).await)
}

async fn current_status(record: &ProjectRecord) -> GrokHarnessStatus {
    {
        let mut pool = global_pool().lock().await;
        if let Some(s) = pool.status_of(&record.id) {
            return s;
        }
    }
    if let Ok(Some(h)) = load_persist(record)
        && h.alive
        && let Some(addr) = &h.control_addr
        && let Ok(resp) = holder_rpc(addr, &HoldRequest::Status).await
        && let Some(st) = resp.harness
    {
        return st;
    }
    load_persist(record)
        .ok()
        .flatten()
        .map(|h| h.to_status())
        .unwrap_or_else(GrokHarnessStatus::missing)
}

pub async fn shutdown(project: Option<&str>) -> Result<GrokHarnessStatus> {
    let rec = resolve_record(project)?;
    if let Some(mut session) = global_pool().lock().await.remove(&rec.id) {
        let _ = session.shutdown().await;
        let st = GrokHarnessStatus {
            alive: false,
            session_id: Some(session.session_id.clone()),
            cwd: Some(session.cwd.clone()),
            supports_compact: session.supports_compact,
            pid: session.pid,
        };
        let dead = PersistedGrokHandle {
            version: 1,
            project_id: rec.id.clone(),
            session_id: st.session_id.clone(),
            cwd: st.cwd.clone(),
            pid: st.pid,
            holder_pid: None,
            control_addr: None,
            supports_compact: st.supports_compact,
            alive: false,
            error: None,
        };
        let _ = save_persist(&rec, &dead);
        return Ok(st);
    }
    if let Some(h) = load_persist(&rec)?
        && let Some(addr) = h.control_addr
    {
        let _ = holder_rpc(&addr, &HoldRequest::Shutdown).await;
    }
    let st = GrokHarnessStatus {
        alive: false,
        session_id: load_persist(&rec).ok().flatten().and_then(|h| h.session_id),
        cwd: None,
        supports_compact: true,
        pid: None,
    };
    Ok(st)
}

/// Insert a mock session (tests).
pub async fn insert_test_session(project_id: String, session: GrokSession) {
    global_pool().lock().await.insert(project_id, session);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ENV_COORDINATOR_HOME, test_env_lock};
    use crate::harness::grok::{mock_handshake_ok, rpc_result, session_update_chunk};
    use crate::registry::{ProjectAddOptions, Registry};
    use crate::run;
    use crate::state::{STUB_PHASE_ACTIVE, STUB_PHASE_COMPLETED, STUB_PHASE_STOPPED};
    use serde_json::json;
    use tempfile::tempdir;

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn pool_prompt_outcome_stop_shutdown() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();

        run::run(&rec, None).unwrap();

        let mut lines = mock_handshake_ok("sess-pool");
        lines.push(session_update_chunk("ok"));
        lines.push(rpc_result(4, json!({ "stopReason": "end_turn" })));
        let session = GrokSession::start_mock(
            crate::harness::grok_cwd(&rec),
            lines,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        insert_test_session(rec.id.clone(), session).await;

        let view = prompt(Some(&rec.id), "hello".into()).await.unwrap();
        assert_eq!(view.text.as_deref(), Some("ok"));
        assert!(view.applied);
        assert_eq!(view.status.as_ref().unwrap().phase, STUB_PHASE_COMPLETED);
        assert!(
            view.status
                .as_ref()
                .unwrap()
                .last_event
                .contains("source=adapter")
        );

        // New run + session still in pool after stop (do not kill).
        run::run(&rec, None).unwrap();
        let mut lines = mock_handshake_ok("sess-alive");
        lines.push(rpc_result(4, json!({ "stopReason": "end_turn" })));
        let session = GrokSession::start_mock(
            crate::harness::grok_cwd(&rec),
            lines,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        insert_test_session(rec.id.clone(), session).await;

        let stopped = run::stop(&rec).unwrap();
        assert_eq!(stopped.phase, STUB_PHASE_STOPPED);
        let st = status(Some(&rec.id)).await.unwrap();
        assert!(st.alive, "stop must leave Grok session in the pool");
        assert_eq!(st.session_id.as_deref(), Some("sess-alive"));

        let down = shutdown(Some(&rec.id)).await.unwrap();
        assert!(!down.alive);
        assert!(!global_pool().lock().await.contains(&rec.id));

        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn pause_refuses_prompt() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        run::run(&rec, None).unwrap();
        run::pause(&rec).unwrap();

        let session = GrokSession::start_mock(
            crate::harness::grok_cwd(&rec),
            mock_handshake_ok("sess-p"),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        insert_test_session(rec.id.clone(), session).await;

        let err = prompt(Some(&rec.id), "nope".into()).await.unwrap_err();
        assert!(matches!(
            err,
            CoordinatorError::InvalidTransition {
                action: "harness prompt",
                ..
            }
        ));
        // Session still in pool
        assert!(global_pool().lock().await.contains(&rec.id));
        let _ = shutdown(Some(&rec.id)).await;

        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn compact_skip_when_unsupported() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();

        let mut session = GrokSession::start_mock(
            crate::harness::grok_cwd(&rec),
            mock_handshake_ok("sess-skip"),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        session.set_supports_compact(false);
        insert_test_session(rec.id.clone(), session).await;

        let view = compact(Some(&rec.id)).await.unwrap();
        assert_eq!(view.skipped, Some(true));
        assert!(!view.applied);
        let _ = shutdown(Some(&rec.id)).await;
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn compact_does_not_apply_phase_outcome() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        run::run(&rec, None).unwrap();

        let mut lines = mock_handshake_ok("sess-cmp");
        lines.push(rpc_result(4, json!({ "stopReason": "end_turn" })));
        let session = GrokSession::start_mock(
            crate::harness::grok_cwd(&rec),
            lines,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        insert_test_session(rec.id.clone(), session).await;

        let view = compact(Some(&rec.id)).await.unwrap();
        assert!(!view.applied);
        let st = run::status(&rec).unwrap();
        assert_eq!(st.status, crate::state::RunStatus::Running);
        assert_eq!(st.phase, STUB_PHASE_ACTIVE);
        let _ = shutdown(Some(&rec.id)).await;
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn failure_prompt_sets_harness_crash() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        run::run(&rec, None).unwrap();

        let mut lines = mock_handshake_ok("sess-fail");
        lines.push(crate::harness::grok::rpc_error(4, "broken pipe"));
        let session = GrokSession::start_mock(
            crate::harness::grok_cwd(&rec),
            lines,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        insert_test_session(rec.id.clone(), session).await;

        let view = prompt(Some(&rec.id), "x".into()).await.unwrap();
        assert_eq!(view.failure_class, Some(FailureClass::HarnessCrash));
        assert!(view.applied);
        assert_eq!(
            view.status.as_ref().unwrap().failure_class,
            Some(FailureClass::HarnessCrash)
        );
        let _ = shutdown(Some(&rec.id)).await;
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[test]
    fn retry_after_failed_start_clears_stale_error() {
        let dir = tempdir().unwrap();
        let rec = ProjectRecord {
            id: "p".into(),
            path: dir.path().to_path_buf(),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: Default::default(),
            state_dir: Some(dir.path().join("state")),
            created_at: chrono::Utc::now(),
        };
        let stale = PersistedGrokHandle {
            version: 1,
            project_id: rec.id.clone(),
            session_id: None,
            cwd: None,
            pid: None,
            holder_pid: Some(1),
            control_addr: None,
            supports_compact: false,
            alive: false,
            error: Some("ACP authenticate: not logged in".into()),
        };
        save_persist(&rec, &stale).unwrap();
        assert!(load_persist(&rec).unwrap().unwrap().error.is_some());

        // Stale error would be returned immediately if we did not clear.
        assert!(matches!(
            interpret_persist_after_spawn(&stale),
            PersistWait::Failed(_)
        ));
        clear_stale_holder_persist(&rec).unwrap();
        assert!(
            load_persist(&rec).unwrap().is_none(),
            "retry must drop the prior failure record"
        );

        let ready = PersistedGrokHandle {
            version: 1,
            project_id: rec.id.clone(),
            session_id: Some("sess-retry".into()),
            cwd: Some(dir.path().to_path_buf()),
            pid: Some(9),
            holder_pid: Some(2),
            control_addr: Some("127.0.0.1:9".into()),
            supports_compact: true,
            alive: true,
            error: None,
        };
        assert_eq!(interpret_persist_after_spawn(&ready), PersistWait::Ready);
    }

    #[test]
    fn persist_path_under_state_dir() {
        let dir = tempdir().unwrap();
        let rec = ProjectRecord {
            id: "p".into(),
            path: dir.path().to_path_buf(),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: Default::default(),
            state_dir: None,
            created_at: chrono::Utc::now(),
        };
        let p = persist_path(&rec).unwrap();
        assert!(p.ends_with("harness-grok.json"));
    }

    #[test]
    fn cwd_prefers_execution_repo() {
        let dir = tempdir().unwrap();
        let exec = dir.path().join("product");
        std::fs::create_dir_all(&exec).unwrap();
        let rec = ProjectRecord {
            id: "p".into(),
            path: dir.path().to_path_buf(),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: Some(exec.clone()),
            execution_repos: Default::default(),
            state_dir: None,
            created_at: chrono::Utc::now(),
        };
        assert_eq!(grok_cwd(&rec), exec);
    }
}
