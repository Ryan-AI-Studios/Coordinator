//! Session pool keyed by `project_id` plus CLI holder + persist metadata.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::error::{CoordinatorError, Result};
use crate::harness::grok::{ENV_GROK_BIN, GrokSession, PromptResult, map_failure_class};
use crate::harness::{grok_cwd, resolve_grok_binary};

/// Child-only model pin for the detached holder (never `set_var` on the parent).
pub(crate) const ENV_GROK_MODEL: &str = "COORDINATOR_GROK_MODEL";
use crate::outcome::{FailureClass, OutcomeSource, PhaseOutcome, write_and_apply};
use crate::persist::atomic_write_json;
use crate::registry::ProjectRecord;
use crate::state::{RunStatus, StatusView, ensure_state_dir, load_run_state, resolve_state_dir};

/// In-process pool (tests / `insert_test_session`). CLI and HTTP start use a detached holder.
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
    /// Mid-`session/prompt` (0027). Missing key = false. No persist version bump.
    #[serde(default)]
    prompt_in_flight: bool,
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
    Cancel,
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
    prompt_in_flight: bool,
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
        prompt_in_flight,
        error: None,
    };
    let _ = save_persist(record, &handle);
}

pub(crate) fn persist_prompt_in_flight(record: &ProjectRecord) -> bool {
    load_persist(record)
        .ok()
        .flatten()
        .is_some_and(|h| h.prompt_in_flight)
}

pub(crate) fn persist_session_id(record: &ProjectRecord) -> Option<String> {
    load_persist(record)
        .ok()
        .flatten()
        .and_then(|h| h.session_id)
}

fn set_prompt_in_flight(record: &ProjectRecord, value: bool) {
    if let Ok(Some(mut h)) = load_persist(record) {
        h.prompt_in_flight = value;
        let _ = save_persist(record, &h);
    }
}

/// Holder `Cancel` RPC (no-op if no live control addr).
pub(crate) async fn holder_cancel(record: &ProjectRecord) -> Result<()> {
    let Some(h) = load_persist(record)? else {
        return Ok(());
    };
    let Some(addr) = h.control_addr else {
        return Ok(());
    };
    let _ = holder_rpc(&addr, &HoldRequest::Cancel).await;
    Ok(())
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

fn prompt_timeout_for(record: &ProjectRecord) -> Duration {
    match load_run_state(record) {
        Ok(s) => crate::workflow::timeout_for_phase(record, &s.phase),
        Err(_) => crate::workflow::timeout_for_phase(record, crate::workflow::graph::PHASE_PLAN),
    }
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
    injected_phase: &str,
) -> Result<HarnessPromptView> {
    let state = load_run_state(record)?;
    let drifted = state.phase != injected_phase;
    match turn {
        Ok(pr) => {
            let mut applied = false;
            let mut status = None;
            let skip = state.status != RunStatus::Running
                || crate::harness::abort::stop_reason_is_cancelled(pr.stop_reason.as_deref())
                || harness_is_aborted(&state, &harness)
                || drifted;
            if !skip {
                let msg = if pr.text.is_empty() {
                    None
                } else {
                    Some(pr.text.clone())
                };
                let next = if injected_phase == crate::workflow::graph::PHASE_ADVANCE {
                    match crate::workflow::prompts::parse_next_track_line(&pr.text) {
                        Some(Some(id)) => Some(id),
                        Some(None) => Some(String::new()),
                        None => None,
                    }
                } else {
                    None
                };
                let outcome = PhaseOutcome::success(
                    injected_phase,
                    OutcomeSource::Adapter,
                    msg,
                    next,
                    Some(state.run_epoch),
                );
                status = Some(write_and_apply(record, outcome)?);
                applied = true;
            }
            Ok(HarnessPromptView {
                text: Some(pr.text),
                stop_reason: pr.stop_reason,
                applied,
                skipped: if skip { Some(true) } else { None },
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
            let skip = state.status != RunStatus::Running
                || harness_is_aborted(&state, &harness)
                || drifted;
            if !skip {
                let outcome = PhaseOutcome::failure(
                    injected_phase,
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
                skipped: if skip { Some(true) } else { None },
                error: Some(e.to_string()),
                failure_class: Some(class),
                status,
                harness: Some(harness),
            })
        }
    }
}

fn harness_is_aborted(state: &crate::state::RunState, harness: &GrokHarnessStatus) -> bool {
    match (
        state.aborted_session_id.as_deref(),
        harness.session_id.as_deref(),
    ) {
        (Some(aborted), Some(sid)) => aborted == sid,
        _ => false,
    }
}

async fn spawn_in_process(
    record: &ProjectRecord,
    bin: Option<PathBuf>,
    model: Option<String>,
) -> Result<GrokHarnessStatus> {
    let cwd = grok_cwd(record);
    let mut session = match bin {
        Some(b) => {
            GrokSession::start_with_bin(cwd, prompt_timeout_for(record), b, model.as_deref())
                .await?
        }
        None => GrokSession::start(cwd, prompt_timeout_for(record)).await?,
    };
    session.set_progress_record(record.clone());
    crate::harness::abort::register_cancel_handle(record.id.clone(), session.cancel_handle());
    write_session_persist(record, &session, None, false);
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
    start_inner(project, in_process, None, None).await
}

/// Adapter ticks pass the already-resolved phase bin / model. CLI start does not.
pub async fn start_with_bin(
    project: Option<&str>,
    in_process: bool,
    bin: PathBuf,
    model: Option<String>,
) -> Result<GrokHarnessStatus> {
    start_inner(project, in_process, Some(bin), model).await
}

async fn start_inner(
    project: Option<&str>,
    in_process: bool,
    bin: Option<PathBuf>,
    model: Option<String>,
) -> Result<GrokHarnessStatus> {
    let rec = resolve_record(project)?;
    let refuse = crate::harness::abort::should_refuse_reuse(&rec);
    {
        let mut pool = global_pool().lock().await;
        if let Some(s) = pool.status_of(&rec.id)
            && s.alive
        {
            if refuse {
                if let Some(mut session) = pool.remove(&rec.id) {
                    let _ = session.shutdown().await;
                }
                crate::harness::abort::unregister_cancel_handle(&rec.id);
            } else {
                return Ok(s);
            }
        }
    }
    if refuse {
        if let Ok(Some(existing)) = load_persist(&rec) {
            reap_stale_holder(&rec, &existing);
        }
    } else if let Some(existing) = reuse_or_reap_existing(&rec).await? {
        return Ok(existing);
    }

    if in_process {
        spawn_in_process(&rec, bin, model).await
    } else {
        start_holder(&rec, project, bin, model).await
    }
}

/// Reuse a live holder that still answers Ping. Otherwise treat leftover persist as
/// dead (kill `pid` then `holder_pid`) so a new start never clobbers a holder file
/// with an in-process persist (`control_addr: None`).
async fn reuse_or_reap_existing(record: &ProjectRecord) -> Result<Option<GrokHarnessStatus>> {
    let Some(existing) = load_persist(record)? else {
        return Ok(None);
    };
    if !existing.alive {
        return Ok(None);
    }
    if crate::harness::abort::should_refuse_reuse(record) {
        reap_stale_holder(record, &existing);
        return Ok(None);
    }
    if let Some(addr) = &existing.control_addr
        && holder_rpc(addr, &HoldRequest::Ping).await.is_ok()
    {
        return Ok(Some(existing.to_status()));
    }
    reap_stale_holder(record, &existing);
    Ok(None)
}

fn reap_stale_holder(record: &ProjectRecord, existing: &PersistedGrokHandle) {
    kill_persist_pids(existing);
    let _ = clear_stale_holder_persist(record);
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

async fn start_holder(
    record: &ProjectRecord,
    project: Option<&str>,
    bin: Option<PathBuf>,
    model: Option<String>,
) -> Result<GrokHarnessStatus> {
    // CLI / HTTP start: implementor-first resolve. Adapter ticks pass the
    // already-resolved phase bin — do not call resolve_grok_binary() there
    // (that would fail plan when implementor is broken).
    if bin.is_none() {
        let _ = resolve_grok_binary()?;
    }
    // A previous failed start writes `error` into harness-grok.json. Clear it
    // before spawn so we never return that stale error on retry.
    clear_stale_holder_persist(record)?;
    spawn_holder_process(
        project.unwrap_or(record.path.to_str().unwrap_or(&record.id)),
        bin.as_deref(),
        model.as_deref(),
    )?;

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

/// Env pairs set only on the holder child Command — never `std::env::set_var`.
pub(crate) fn holder_child_env(
    bin: &std::path::Path,
    model: Option<&str>,
) -> Vec<(String, String)> {
    let mut env = vec![(ENV_GROK_BIN.to_string(), bin.to_string_lossy().into_owned())];
    if let Some(m) = model {
        let t = m.trim();
        if !t.is_empty() {
            env.push((ENV_GROK_MODEL.to_string(), t.to_string()));
        }
    }
    env
}

fn apply_holder_child_env(
    cmd: &mut std::process::Command,
    bin: Option<&std::path::Path>,
    model: Option<&str>,
) {
    if let Some(bin) = bin {
        for (k, v) in holder_child_env(bin, model) {
            cmd.env(k, v);
        }
    }
}

fn spawn_holder_process(
    project_spec: &str,
    bin: Option<&std::path::Path>,
    model: Option<&str>,
) -> Result<()> {
    let exe = std::env::current_exe().map_err(|e| {
        CoordinatorError::Message(format!("cannot resolve coordinator executable: {e}"))
    })?;
    let mut cmd = std::process::Command::new(exe);
    // Inherit COORDINATOR_HOME / COORDINATOR_STATE_DIR so the holder writes
    // harness-grok.json and harness-progress.json to the same resolve_state_dir
    // as wait/serve. Do not invent a second location.
    cmd.arg("harness")
        .arg("grok")
        .arg("hold")
        .arg("--project")
        .arg(project_spec)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    apply_holder_child_env(&mut cmd, bin, model);
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
    let model = std::env::var(ENV_GROK_MODEL)
        .ok()
        .filter(|s| !s.trim().is_empty());
    let started = async {
        let bin = resolve_grok_binary()?;
        GrokSession::start_with_bin(cwd, prompt_timeout_for(&rec), bin, model.as_deref()).await
    }
    .await;
    let session = match started {
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
                prompt_in_flight: false,
                error: Some(e.to_string()),
            };
            let _ = save_persist(&rec, &handle);
            return Err(e);
        }
    };

    hold_accept_loop(rec, session).await
}

/// Scripted holder for tests (mock ACP session).
#[cfg(test)]
pub async fn hold_loop_with_session(record: ProjectRecord, session: GrokSession) -> Result<()> {
    hold_accept_loop(record, session).await
}

async fn hold_accept_loop(rec: ProjectRecord, mut session: GrokSession) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| CoordinatorError::Message(format!("holder bind 127.0.0.1:0 failed: {e}")))?;
    let addr = listener.local_addr()?.to_string();
    session.set_progress_record(rec.clone());
    crate::harness::abort::register_cancel_handle(rec.id.clone(), session.cancel_handle());
    write_session_persist(&rec, &session, Some(addr.clone()), false);

    let snapshot = std::sync::Mutex::new(session_status(&session));
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let shared = std::sync::Arc::new(HolderShared {
        session: tokio::sync::Mutex::new(session),
        prompt_gate: tokio::sync::Mutex::new(()),
        rec: rec.clone(),
        snapshot,
        shutdown_tx,
    });

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            acc = listener.accept() => {
                let (stream, _) = match acc {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let shared = shared.clone();
                tokio::spawn(async move {
                    let _ = handle_hold_conn(stream, shared).await;
                });
            }
        }
    }
    crate::harness::abort::unregister_cancel_handle(&rec.id);
    let mut session = shared.session.lock().await;
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
        prompt_in_flight: false,
        error: None,
    };
    let _ = save_persist(&rec, &dead);
    Ok(())
}

struct HolderShared {
    session: tokio::sync::Mutex<GrokSession>,
    prompt_gate: tokio::sync::Mutex<()>,
    rec: ProjectRecord,
    snapshot: std::sync::Mutex<GrokHarnessStatus>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
}

fn snapshot_status(shared: &HolderShared) -> GrokHarnessStatus {
    shared
        .snapshot
        .lock()
        .map(|s| s.clone())
        .unwrap_or_else(|_| GrokHarnessStatus::missing())
}

async fn handle_hold_conn(stream: TcpStream, shared: std::sync::Arc<HolderShared>) -> Result<bool> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let Some(line) = lines.next_line().await? else {
        return Ok(false);
    };
    let req: HoldRequest = serde_json::from_str(&line)?;
    let record = &shared.rec;
    let (resp, shutdown) = match req {
        HoldRequest::Ping | HoldRequest::Status => (
            HoldResponse {
                ok: true,
                error: None,
                text: None,
                stop_reason: None,
                status: None,
                harness: Some(snapshot_status(&shared)),
                applied: None,
                skipped: None,
                failure_class: None,
            },
            false,
        ),
        HoldRequest::Cancel => {
            if let Some(h) = crate::harness::abort::cancel_handle_for(&record.id) {
                let _ = h.cancel().await;
            }
            (
                HoldResponse {
                    ok: true,
                    error: None,
                    text: None,
                    stop_reason: None,
                    status: None,
                    harness: Some(snapshot_status(&shared)),
                    applied: None,
                    skipped: None,
                    failure_class: None,
                },
                false,
            )
        }
        HoldRequest::Prompt { text } => match refuse_if_paused(record) {
            Err(e) => (
                HoldResponse {
                    ok: false,
                    error: Some(e.to_string()),
                    text: None,
                    stop_reason: None,
                    status: None,
                    harness: Some(snapshot_status(&shared)),
                    applied: None,
                    skipped: None,
                    failure_class: None,
                },
                false,
            ),
            Ok(()) => match shared.prompt_gate.try_lock() {
                Err(_) => (
                    HoldResponse {
                        ok: false,
                        error: Some("prompt already in flight".into()),
                        text: None,
                        stop_reason: None,
                        status: None,
                        harness: Some(snapshot_status(&shared)),
                        applied: None,
                        skipped: None,
                        failure_class: None,
                    },
                    false,
                ),
                Ok(_gate) => {
                    let injected_phase = load_run_state(record)?.phase;
                    set_prompt_in_flight(record, true);
                    let turn = {
                        let mut session = shared.session.lock().await;
                        if let Ok(mut snap) = shared.snapshot.lock() {
                            *snap = session_status(&session);
                        }
                        crate::workflow::watchdog::note_progress(
                            record,
                            crate::workflow::watchdog::ProgressKind::Inject,
                            Some(&session.session_id),
                        );
                        session
                            .inject_prompt(&text, prompt_timeout_for(record))
                            .await
                    };
                    let normal = matches!(&turn, Ok(pr) if !crate::harness::abort::stop_reason_is_cancelled(pr.stop_reason.as_deref()));
                    if normal {
                        set_prompt_in_flight(record, false);
                    }
                    if let Err(e) = &turn {
                        let msg = e.to_string().to_ascii_lowercase();
                        if msg.contains("timed out") || msg.contains("timeout") {
                            crate::harness::abort::abort_stuck_prompt_sync(
                                record,
                                crate::harness::abort::AbortReason::PromptTimeout,
                            );
                        }
                    }
                    let view =
                        apply_turn(record, turn, snapshot_status(&shared), &injected_phase).await?;
                    (view_to_hold(view), false)
                }
            },
        },
        HoldRequest::Compact => {
            let session = shared.session.lock().await;
            if !session.supports_compact {
                (
                    HoldResponse {
                        ok: true,
                        error: None,
                        text: None,
                        stop_reason: None,
                        status: None,
                        harness: Some(session_status(&session)),
                        applied: Some(false),
                        skipped: Some(true),
                        failure_class: None,
                    },
                    false,
                )
            } else {
                drop(session);
                let mut session = shared.session.lock().await;
                match session.compact(prompt_timeout_for(record)).await {
                    Ok(pr) => (
                        HoldResponse {
                            ok: true,
                            error: None,
                            text: Some(pr.text),
                            stop_reason: pr.stop_reason,
                            status: None,
                            harness: Some(session_status(&session)),
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
                            harness: Some(session_status(&session)),
                            applied: Some(false),
                            skipped: None,
                            failure_class: Some(map_failure_class(&e.to_string())),
                        },
                        false,
                    ),
                }
            }
        }
        HoldRequest::Shutdown => {
            if let Some(h) = crate::harness::abort::cancel_handle_for(&record.id) {
                let _ = h.cancel().await;
            }
            let _ = shared.shutdown_tx.send(true);
            (
                HoldResponse {
                    ok: true,
                    error: None,
                    text: None,
                    stop_reason: None,
                    status: None,
                    harness: Some(GrokHarnessStatus {
                        alive: false,
                        session_id: snapshot_status(&shared).session_id,
                        cwd: snapshot_status(&shared).cwd,
                        supports_compact: snapshot_status(&shared).supports_compact,
                        pid: snapshot_status(&shared).pid,
                    }),
                    applied: None,
                    skipped: None,
                    failure_class: None,
                },
                true,
            )
        }
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
    let injected_phase = load_run_state(&rec)?.phase;
    set_prompt_in_flight(&rec, true);
    let turn = {
        let mut pool = global_pool().lock().await;
        let session = pool.get_mut(&rec.id).ok_or_else(|| {
            CoordinatorError::Message(
                "no Grok session; run `coordinator harness grok start` first".into(),
            )
        })?;
        crate::workflow::watchdog::note_progress(
            &rec,
            crate::workflow::watchdog::ProgressKind::Inject,
            Some(&session.session_id),
        );
        session.inject_prompt(&text, prompt_timeout_for(&rec)).await
    };
    let normal = matches!(&turn, Ok(pr) if !crate::harness::abort::stop_reason_is_cancelled(pr.stop_reason.as_deref()));
    if normal {
        set_prompt_in_flight(&rec, false);
    } else if let Err(e) = &turn {
        let msg = e.to_string().to_ascii_lowercase();
        if msg.contains("timed out") || msg.contains("timeout") {
            crate::harness::abort::abort_stuck_prompt_sync(
                &rec,
                crate::harness::abort::AbortReason::PromptTimeout,
            );
        }
    }
    let harness = current_status(&rec).await;
    apply_turn(&rec, turn, harness, &injected_phase).await
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
    match session.compact(prompt_timeout_for(&rec)).await {
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

/// Recycle without waiting on `global_pool()` (prompt() holds that mutex).
/// CancelHandle + persist pid-kill + dead persist. Best-effort pool remove.
pub(crate) async fn recycle_without_pool_lock(record: &ProjectRecord) -> Result<()> {
    crate::harness::abort::unregister_cancel_handle(&record.id);
    if let Ok(mut pool) = global_pool().try_lock()
        && let Some(mut session) = pool.remove(&record.id)
    {
        let _ = session.shutdown().await;
    }
    if let Ok(Some(h)) = load_persist(record) {
        kill_persist_pids(&h);
        let dead = persist_marked_dead(&h);
        let _ = save_persist(record, &dead);
    }
    Ok(())
}

/// How long the CLI will wait for a holder `Shutdown` RPC before falling back to pid-kill.
const HOLDER_SHUTDOWN_RPC_TIMEOUT: Duration = Duration::from_millis(750);

pub async fn shutdown(project: Option<&str>) -> Result<GrokHarnessStatus> {
    let rec = resolve_record(project)?;
    if let Some(mut session) = global_pool().lock().await.remove(&rec.id) {
        crate::harness::abort::unregister_cancel_handle(&rec.id);
        let _ = session.shutdown().await;
        if let Ok(Some(h)) = load_persist(&rec) {
            kill_persist_pids(&h);
        }
        let dead = PersistedGrokHandle {
            version: 1,
            project_id: rec.id.clone(),
            session_id: Some(session.session_id.clone()),
            cwd: Some(session.cwd.clone()),
            pid: session.pid,
            holder_pid: None,
            control_addr: None,
            supports_compact: session.supports_compact,
            alive: false,
            prompt_in_flight: false,
            error: None,
        };
        let _ = save_persist(&rec, &dead);
        return Ok(dead.to_status());
    }

    let persist = load_persist(&rec)?;
    if let Some(h) = &persist
        && let Some(addr) = &h.control_addr
    {
        let _ = tokio::time::timeout(
            HOLDER_SHUTDOWN_RPC_TIMEOUT,
            holder_rpc(addr, &HoldRequest::Shutdown),
        )
        .await;
    }
    if let Some(h) = persist {
        if h.alive {
            kill_persist_pids(&h);
        }
        let dead = persist_marked_dead(&h);
        let _ = save_persist(&rec, &dead);
        return Ok(dead.to_status());
    }

    Ok(GrokHarnessStatus::missing())
}

fn persist_marked_dead(h: &PersistedGrokHandle) -> PersistedGrokHandle {
    PersistedGrokHandle {
        version: h.version,
        project_id: h.project_id.clone(),
        session_id: h.session_id.clone(),
        cwd: h.cwd.clone(),
        pid: h.pid,
        holder_pid: h.holder_pid,
        control_addr: None,
        supports_compact: h.supports_compact,
        alive: false,
        prompt_in_flight: false,
        error: None,
    }
}

/// Best-effort kill of persist `pid` then `holder_pid`. Never kills this process.
/// `taskkill` missing from PATH, or "process not found", is success.
fn kill_persist_pids(h: &PersistedGrokHandle) {
    if let Some(pid) = h.pid {
        kill_pid_best_effort(pid);
    }
    if let Some(holder_pid) = h.holder_pid
        && h.pid != Some(holder_pid)
    {
        kill_pid_best_effort(holder_pid);
    }
}

fn kill_pid_best_effort(pid: u32) {
    if pid == 0 || pid == std::process::id() {
        return;
    }
    #[cfg(windows)]
    {
        // "The process … not found" is already-dead success. Missing taskkill is not an error.
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Insert a mock session (tests).
pub async fn insert_test_session(project_id: String, session: GrokSession) {
    crate::harness::abort::register_cancel_handle(project_id.clone(), session.cancel_handle());
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

    #[test]
    fn holder_child_env_sets_bin_not_process_var() {
        let _guard = test_env_lock();
        unsafe {
            std::env::remove_var(ENV_GROK_BIN);
            std::env::remove_var(ENV_GROK_MODEL);
        }
        let env = holder_child_env(
            std::path::Path::new(r"C:\phase\grok.exe"),
            Some("grok-build"),
        );
        assert_eq!(env[0].0, ENV_GROK_BIN);
        assert_eq!(env[0].1, r"C:\phase\grok.exe");
        assert_eq!(env[1].0, ENV_GROK_MODEL);
        assert_eq!(env[1].1, "grok-build");
        assert!(
            std::env::var(ENV_GROK_BIN).is_err(),
            "must not set_var COORDINATOR_GROK_BIN on the parent"
        );
        assert!(
            std::env::var(ENV_GROK_MODEL).is_err(),
            "must not set_var COORDINATOR_GROK_MODEL on the parent"
        );
    }

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

        crate::run::run_stub(&rec, None).unwrap();

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
        crate::run::run_stub(&rec, None).unwrap();
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
        crate::run::run_stub(&rec, None).unwrap();
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
        crate::run::run_stub(&rec, None).unwrap();

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
        crate::run::run_stub(&rec, None).unwrap();

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
            auto_merge: true,
            phase_timeouts_secs: Default::default(),
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
            prompt_in_flight: false,
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
            prompt_in_flight: false,
            error: None,
        };
        assert_eq!(interpret_persist_after_spawn(&ready), PersistWait::Ready);
    }

    #[test]
    fn prompt_timeout_uses_current_phase_budget() {
        use crate::config::{ENV_COORDINATOR_HOME, test_env_lock};
        use crate::workflow::timeouts::ENV_PHASE_TIMEOUT_SECS;
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        unsafe {
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
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
            auto_merge: true,
            phase_timeouts_secs: Default::default(),
            created_at: chrono::Utc::now(),
        };
        crate::run::run_with_driver(&rec, None, crate::workflow::WorkflowDriver::FileWait).unwrap();
        let t = prompt_timeout_for(&rec);
        assert_eq!(
            t,
            crate::workflow::timeout_for_phase(&rec, crate::workflow::graph::PHASE_PLAN)
        );
        assert_ne!(t, Duration::from_secs(300));
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[test]
    fn prompt_timeout_missing_file_is_stub_not_plan() {
        use crate::config::{ENV_COORDINATOR_HOME, test_env_lock};
        use crate::workflow::timeouts::ENV_PHASE_TIMEOUT_SECS;
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        unsafe {
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
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
            auto_merge: true,
            phase_timeouts_secs: Default::default(),
            created_at: chrono::Utc::now(),
        };
        let t = prompt_timeout_for(&rec);
        assert_eq!(t, Duration::from_secs(300));
        assert_ne!(
            t,
            crate::workflow::timeout_for_phase(&rec, crate::workflow::graph::PHASE_PLAN)
        );
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
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
            auto_merge: true,
            phase_timeouts_secs: Default::default(),
            created_at: chrono::Utc::now(),
        };
        let p = persist_path(&rec).unwrap();
        assert!(p.ends_with("harness-grok.json"));
    }

    fn taskkill_on_path() -> bool {
        std::process::Command::new("taskkill")
            .arg("/?")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    }

    fn wait_child_dead(child: &mut std::process::Child, budget_ms: u64) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_millis(budget_ms);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                _ => return false,
            }
        }
    }

    fn spawn_dummy_long_child() -> std::process::Child {
        #[cfg(windows)]
        {
            std::process::Command::new("cmd")
                .args(["/C", "timeout", "/T", "300", "/NOBREAK"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn dummy child")
        }
        #[cfg(not(windows))]
        {
            std::process::Command::new("sleep")
                .arg("300")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn dummy child")
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn shutdown_kills_persist_pid_and_writes_dead() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();

        let mut child = spawn_dummy_long_child();
        let pid = child.id();
        let handle = PersistedGrokHandle {
            version: 1,
            project_id: rec.id.clone(),
            session_id: Some("sess-dummy".into()),
            cwd: Some(proj.path().to_path_buf()),
            pid: Some(pid),
            holder_pid: None,
            control_addr: None,
            supports_compact: true,
            alive: true,
            prompt_in_flight: false,
            error: None,
        };
        save_persist(&rec, &handle).unwrap();
        assert!(
            !global_pool().lock().await.contains(&rec.id),
            "pool must be empty so shutdown uses persist pid-kill"
        );

        let st = shutdown(Some(&rec.id)).await.unwrap();
        let persist = load_persist(&rec).unwrap().expect("persist written");
        assert!(!persist.alive, "persist must be written dead");
        assert!(!st.alive, "status must match persist");
        assert_eq!(st.alive, persist.alive);
        assert_eq!(st.session_id.as_deref(), Some("sess-dummy"));

        if taskkill_on_path() {
            assert!(
                wait_child_dead(&mut child, 1000),
                "taskkill is present; persist pid must be dead"
            );
        } else if !wait_child_dead(&mut child, 50) {
            let _ = child.kill();
            let _ = child.wait();
        }

        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn ping_fail_reaps_leftover_holder_pids() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();

        let mut child = spawn_dummy_long_child();
        let handle = PersistedGrokHandle {
            version: 1,
            project_id: rec.id.clone(),
            session_id: Some("sess-stale".into()),
            cwd: Some(proj.path().to_path_buf()),
            pid: Some(child.id()),
            holder_pid: None,
            control_addr: Some("127.0.0.1:1".into()),
            supports_compact: true,
            alive: true,
            prompt_in_flight: false,
            error: None,
        };
        save_persist(&rec, &handle).unwrap();

        let reused = reuse_or_reap_existing(&rec).await.unwrap();
        assert!(reused.is_none(), "dead control_addr must not reuse");
        assert!(
            load_persist(&rec).unwrap().is_none(),
            "stale holder persist must be cleared before a replacement start"
        );
        if taskkill_on_path() {
            assert!(
                wait_child_dead(&mut child, 1000),
                "taskkill is present; leftover persist pid must be reaped"
            );
        } else if !wait_child_dead(&mut child, 50) {
            let _ = child.kill();
            let _ = child.wait();
        }

        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[test]
    fn kill_missing_pid_is_success() {
        kill_pid_best_effort(u32::MAX);
        kill_pid_best_effort(std::process::id());
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
            auto_merge: true,
            phase_timeouts_secs: Default::default(),
            created_at: chrono::Utc::now(),
        };
        assert_eq!(grok_cwd(&rec), exec);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn cancelled_stop_reason_does_not_apply_phase() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(crate::harness::abort::ENV_CANCEL_WAIT_SECS, "0");
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        crate::run::run_with_driver(
            &rec,
            Some("0027".into()),
            crate::workflow::WorkflowDriver::Adapter,
        )
        .unwrap();

        let mut lines = mock_handshake_ok("sess-cancel-apply");
        lines.push(rpc_result(4, json!({ "stopReason": "cancelled" })));
        let session = GrokSession::start_mock(
            crate::harness::grok_cwd(&rec),
            lines,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        insert_test_session(rec.id.clone(), session).await;

        let view = prompt(Some(&rec.id), "x".into()).await.unwrap();
        assert!(!view.applied, "cancelled must not apply");
        assert_eq!(view.skipped, Some(true));
        let st = run::status(&rec).unwrap();
        assert_eq!(st.status, crate::state::RunStatus::Running);
        assert_eq!(st.phase, crate::workflow::graph::PHASE_PLAN);
        assert!(crate::notify::artifact::existing_path(&rec).is_none());
        let _ = shutdown(Some(&rec.id)).await;
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
            std::env::remove_var(crate::harness::abort::ENV_CANCEL_WAIT_SECS);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn recycle_stamp_skips_inject_error_apply() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(crate::harness::abort::ENV_CANCEL_WAIT_SECS, "0");
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        crate::run::run_with_driver(
            &rec,
            Some("0027".into()),
            crate::workflow::WorkflowDriver::Adapter,
        )
        .unwrap();
        crate::state::with_run_state_lock(&rec, || {
            let mut s = crate::state::load_run_state(&rec)?;
            s.last_event = crate::harness::abort::RECYCLE_STALL_EVENT.into();
            s.aborted_session_id = Some("sess-rec-err".into());
            crate::state::save_run_state(&rec, &s)
        })
        .unwrap();

        let mut lines = mock_handshake_ok("sess-rec-err");
        lines.push(crate::harness::grok::rpc_error(4, "stdout closed"));
        let session = GrokSession::start_mock(
            crate::harness::grok_cwd(&rec),
            lines,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        insert_test_session(rec.id.clone(), session).await;

        let view = prompt(Some(&rec.id), "x".into()).await.unwrap();
        assert!(!view.applied);
        let st = run::status(&rec).unwrap();
        assert_eq!(st.status, crate::state::RunStatus::Running);
        assert_eq!(st.phase, crate::workflow::graph::PHASE_PLAN);
        assert!(crate::notify::artifact::existing_path(&rec).is_none());
        let _ = shutdown(Some(&rec.id)).await;
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
            std::env::remove_var(crate::harness::abort::ENV_CANCEL_WAIT_SECS);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn holder_cancel_and_status_return_during_hung_prompt() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(crate::harness::abort::ENV_CANCEL_WAIT_SECS, "0");
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        crate::run::run_with_driver(
            &rec,
            Some("0027".into()),
            crate::workflow::WorkflowDriver::Adapter,
        )
        .unwrap();

        let session = GrokSession::start_mock(
            crate::harness::grok_cwd(&rec),
            mock_handshake_ok("sess-hold"),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        let rec_hold = rec.clone();
        let hold = tokio::spawn(async move { hold_loop_with_session(rec_hold, session).await });

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let addr = loop {
            if let Ok(Some(h)) = load_persist(&rec)
                && let Some(addr) = h.control_addr
            {
                break addr;
            }
            if std::time::Instant::now() >= deadline {
                panic!("holder persist never became ready");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };

        let prompt_addr = addr.clone();
        let prompt_task = tokio::spawn(async move {
            holder_rpc(
                &prompt_addr,
                &HoldRequest::Prompt {
                    text: "hang".into(),
                },
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let started = std::time::Instant::now();
        let status = holder_rpc(&addr, &HoldRequest::Status).await.unwrap();
        let ping = holder_rpc(&addr, &HoldRequest::Ping).await.unwrap();
        let cancel = holder_rpc(&addr, &HoldRequest::Cancel).await.unwrap();
        let elapsed = started.elapsed();
        assert!(status.ok && ping.ok && cancel.ok);
        assert!(
            elapsed < Duration::from_millis(800),
            "Status/Ping/Cancel must not wait on Prompt; elapsed={elapsed:?}"
        );

        let prompt_view = tokio::time::timeout(Duration::from_secs(2), prompt_task)
            .await
            .expect("prompt task")
            .expect("join")
            .expect("rpc");
        assert_eq!(prompt_view.stop_reason.as_deref(), Some("cancelled"));

        let _ = holder_rpc(&addr, &HoldRequest::Shutdown).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), hold).await;
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
            std::env::remove_var(crate::harness::abort::ENV_CANCEL_WAIT_SECS);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn abort_recycle_marks_persist_dead() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(crate::harness::abort::ENV_CANCEL_WAIT_SECS, "0");
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();

        let mut child = spawn_dummy_long_child();
        let handle = PersistedGrokHandle {
            version: 1,
            project_id: rec.id.clone(),
            session_id: Some("sess-old".into()),
            cwd: Some(proj.path().to_path_buf()),
            pid: Some(child.id()),
            holder_pid: None,
            control_addr: None,
            supports_compact: true,
            alive: true,
            prompt_in_flight: true,
            error: None,
        };
        save_persist(&rec, &handle).unwrap();
        crate::harness::abort::abort_stuck_prompt_sync(
            &rec,
            crate::harness::abort::AbortReason::Stall,
        );
        let persist = load_persist(&rec).unwrap().expect("persist");
        assert!(!persist.alive);
        assert!(!persist.prompt_in_flight);
        if taskkill_on_path() {
            assert!(wait_child_dead(&mut child, 1000));
        } else if !wait_child_dead(&mut child, 50) {
            let _ = child.kill();
            let _ = child.wait();
        }
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
            std::env::remove_var(crate::harness::abort::ENV_CANCEL_WAIT_SECS);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn aborted_session_id_skips_error_apply_without_recycle_prefix() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(crate::harness::abort::ENV_CANCEL_WAIT_SECS, "0");
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        crate::run::run_with_driver(
            &rec,
            Some("0027".into()),
            crate::workflow::WorkflowDriver::Adapter,
        )
        .unwrap();
        crate::state::with_run_state_lock(&rec, || {
            let mut s = crate::state::load_run_state(&rec)?;
            s.aborted_session_id = Some("sess-stall-skip".into());
            s.last_event = "resume: continue".into();
            crate::state::save_run_state(&rec, &s)
        })
        .unwrap();

        let mut lines = mock_handshake_ok("sess-stall-skip");
        lines.push(crate::harness::grok::rpc_error(4, "stdout closed"));
        let session = GrokSession::start_mock(
            crate::harness::grok_cwd(&rec),
            lines,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        insert_test_session(rec.id.clone(), session).await;

        let view = prompt(Some(&rec.id), "x".into()).await.unwrap();
        assert!(!view.applied);
        assert_eq!(view.skipped, Some(true));
        let st = run::status(&rec).unwrap();
        assert_eq!(st.status, crate::state::RunStatus::Running);
        assert!(crate::notify::artifact::existing_path(&rec).is_none());
        let _ = shutdown(Some(&rec.id)).await;
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
            std::env::remove_var(crate::harness::abort::ENV_CANCEL_WAIT_SECS);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn start_refuses_wedged_persist_session() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(crate::harness::abort::ENV_CANCEL_WAIT_SECS, "0");
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        crate::run::run_with_driver(
            &rec,
            Some("0027".into()),
            crate::workflow::WorkflowDriver::Adapter,
        )
        .unwrap();
        crate::state::with_run_state_lock(&rec, || {
            let mut s = crate::state::load_run_state(&rec)?;
            s.last_event = crate::harness::abort::RECYCLE_STALL_EVENT.into();
            crate::state::save_run_state(&rec, &s)
        })
        .unwrap();

        let old = PersistedGrokHandle {
            version: 1,
            project_id: rec.id.clone(),
            session_id: Some("sess-wedged".into()),
            cwd: Some(proj.path().to_path_buf()),
            pid: None,
            holder_pid: None,
            control_addr: Some("127.0.0.1:1".into()),
            supports_compact: true,
            alive: true,
            prompt_in_flight: true,
            error: None,
        };
        save_persist(&rec, &old).unwrap();

        let reused = reuse_or_reap_existing(&rec).await.unwrap();
        assert!(reused.is_none());
        assert!(
            crate::harness::abort::should_refuse_reuse(&rec),
            "recycle last_event must refuse reuse"
        );
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
            std::env::remove_var(crate::harness::abort::ENV_CANCEL_WAIT_SECS);
        }
    }

    #[test]
    fn fresh_run_clears_stall_recycles() {
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
            auto_merge: true,
            phase_timeouts_secs: Default::default(),
            created_at: chrono::Utc::now(),
        };
        crate::run::run_with_driver(
            &rec,
            Some("0027".into()),
            crate::workflow::WorkflowDriver::Adapter,
        )
        .unwrap();
        crate::state::with_run_state_lock(&rec, || {
            let mut s = crate::state::load_run_state(&rec)?;
            s.stall_recycles = 3;
            s.status = crate::state::RunStatus::Stopped;
            crate::state::save_run_state(&rec, &s)
        })
        .unwrap();
        crate::run::run_with_driver(
            &rec,
            Some("0027".into()),
            crate::workflow::WorkflowDriver::Adapter,
        )
        .unwrap();
        assert_eq!(
            crate::state::load_run_state(&rec).unwrap().stall_recycles,
            0
        );
    }

    fn harness_stub() -> GrokHarnessStatus {
        GrokHarnessStatus {
            alive: true,
            session_id: Some("sess-apply".into()),
            cwd: None,
            supports_compact: false,
            pid: None,
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn apply_turn_skips_ok_when_phase_drifted() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        crate::run::run_with_driver(
            &rec,
            Some("0021".into()),
            crate::workflow::WorkflowDriver::Adapter,
        )
        .unwrap();
        let o = crate::outcome::PhaseOutcome::success(
            crate::workflow::graph::PHASE_PLAN,
            crate::outcome::OutcomeSource::Test,
            None,
            None,
            None,
        );
        crate::outcome::write_and_apply(&rec, o).unwrap();
        assert_eq!(
            crate::state::load_run_state(&rec).unwrap().phase,
            crate::workflow::graph::PHASE_PLAN_REVIEW
        );

        let turn = Ok(PromptResult {
            text: "plan done".into(),
            stop_reason: Some("end_turn".into()),
        });
        let view = apply_turn(
            &rec,
            turn,
            harness_stub(),
            crate::workflow::graph::PHASE_PLAN,
        )
        .await
        .unwrap();
        assert!(!view.applied);
        assert_eq!(view.skipped, Some(true));
        let st = run::status(&rec).unwrap();
        assert_eq!(st.phase, crate::workflow::graph::PHASE_PLAN_REVIEW);
        assert_eq!(st.status, crate::state::RunStatus::Running);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn apply_turn_skips_err_when_phase_drifted() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        crate::run::run_with_driver(
            &rec,
            Some("0021".into()),
            crate::workflow::WorkflowDriver::Adapter,
        )
        .unwrap();
        let o = crate::outcome::PhaseOutcome::success(
            crate::workflow::graph::PHASE_PLAN,
            crate::outcome::OutcomeSource::Test,
            None,
            None,
            None,
        );
        crate::outcome::write_and_apply(&rec, o).unwrap();

        let turn = Err(CoordinatorError::Message("inject failed".into()));
        let view = apply_turn(
            &rec,
            turn,
            harness_stub(),
            crate::workflow::graph::PHASE_PLAN,
        )
        .await
        .unwrap();
        assert!(!view.applied);
        assert_eq!(view.skipped, Some(true));
        let st = run::status(&rec).unwrap();
        assert_eq!(st.phase, crate::workflow::graph::PHASE_PLAN_REVIEW);
        assert_eq!(st.status, crate::state::RunStatus::Running);
        assert!(crate::notify::artifact::existing_path(&rec).is_none());
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn apply_turn_advance_line_auto_starts() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        std::fs::create_dir_all(proj.path().join("conductor").join("0001-Example")).unwrap();
        std::fs::create_dir_all(proj.path().join("conductor").join("0002-Next")).unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        crate::run::run_with_driver(
            &rec,
            Some("0001".into()),
            crate::workflow::WorkflowDriver::Adapter,
        )
        .unwrap();
        crate::state::with_run_state_lock(&rec, || {
            let mut s = crate::state::load_run_state(&rec)?;
            s.phase = crate::workflow::graph::PHASE_ADVANCE.into();
            crate::state::save_run_state(&rec, &s)
        })
        .unwrap();

        let turn = Ok(PromptResult {
            text: "picking next\nnext_track: 0002\n".into(),
            stop_reason: Some("end_turn".into()),
        });
        let view = apply_turn(
            &rec,
            turn,
            harness_stub(),
            crate::workflow::graph::PHASE_ADVANCE,
        )
        .await
        .unwrap();
        assert!(view.applied);
        let st = view.status.expect("applied status");
        assert_eq!(st.status, crate::state::RunStatus::Running);
        assert_eq!(st.phase, crate::workflow::graph::PHASE_PLAN);
        assert_eq!(st.track_id.as_deref(), Some("0002"));
        assert!(st.last_event.contains("auto-start"));
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn apply_turn_explicit_null_clears_stale_next_track() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        crate::run::run_with_driver(
            &rec,
            Some("0001".into()),
            crate::workflow::WorkflowDriver::Adapter,
        )
        .unwrap();
        crate::state::with_run_state_lock(&rec, || {
            let mut s = crate::state::load_run_state(&rec)?;
            s.phase = crate::workflow::graph::PHASE_ADVANCE.into();
            s.next_track = Some("stale-leftover".into());
            crate::state::save_run_state(&rec, &s)
        })
        .unwrap();

        let turn = Ok(PromptResult {
            text: "backlog empty\nnext_track: null\n".into(),
            stop_reason: Some("end_turn".into()),
        });
        let view = apply_turn(
            &rec,
            turn,
            harness_stub(),
            crate::workflow::graph::PHASE_ADVANCE,
        )
        .await
        .unwrap();
        assert!(view.applied);
        let st = view.status.expect("applied status");
        assert_eq!(st.status, crate::state::RunStatus::Idle);
        assert!(st.next_track.is_none());
        assert!(st.last_event.contains("backlog clear"));
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn apply_turn_no_line_leaves_stale_next_track() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        std::fs::create_dir_all(proj.path().join("conductor").join("0002-Next")).unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        crate::run::run_with_driver(
            &rec,
            Some("0001".into()),
            crate::workflow::WorkflowDriver::Adapter,
        )
        .unwrap();
        crate::state::with_run_state_lock(&rec, || {
            let mut s = crate::state::load_run_state(&rec)?;
            s.phase = crate::workflow::graph::PHASE_ADVANCE.into();
            s.next_track = Some("0002".into());
            crate::state::save_run_state(&rec, &s)
        })
        .unwrap();

        let turn = Ok(PromptResult {
            text: "advance finished with no next_track line".into(),
            stop_reason: Some("end_turn".into()),
        });
        let view = apply_turn(
            &rec,
            turn,
            harness_stub(),
            crate::workflow::graph::PHASE_ADVANCE,
        )
        .await
        .unwrap();
        assert!(view.applied);
        let st = view.status.expect("applied status");
        assert_eq!(st.status, crate::state::RunStatus::Running);
        assert_eq!(st.phase, crate::workflow::graph::PHASE_PLAN);
        assert_eq!(st.track_id.as_deref(), Some("0002"));
        assert!(st.last_event.contains("auto-start"));
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }
}
