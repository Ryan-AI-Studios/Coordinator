//! Product-side self-reap of stale terminal-run harness holders (track **0107**).
//!
//! Per registered project, under the run-state lock: read `run-state.json` and
//! `harness-grok.json`; only a terminal (`Idle`/`Stopped`) run whose state and
//! persist are both older than [`STALE_AFTER`] is a candidate. Ownership is
//! proven by the project's own persist record, the cwd ownership roots and a
//! live control-socket identity handshake. Termination is the holder's own
//! `Shutdown` RPC. Every unknown or unreadable state keeps the holder.

use std::collections::BTreeSet;
use std::path::{Component, Path};
use std::sync::Mutex;

use chrono::{DateTime, Utc};

use crate::error::{CoordinatorError, Result};
use crate::features::{self, Duty};
use crate::harness::pool::{self, HolderReapRpc, PersistIdentity};
use crate::registry::ProjectRecord;
use crate::state::{
    RunStatus, load_run_state, resolve_state_dir, run_state_path, with_run_state_lock,
};

/// A terminal run must be quiet for this long before its holder is a candidate.
pub const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);
/// Serve-tick sweep cadence.
pub const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60 * 60);
/// Bounded connect/read/write budget for each holder RPC.
pub(crate) const RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(750);

static WARNED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

fn warn_once(key: impl Into<String>, line: &str) {
    let first = {
        let mut warned = WARNED.lock().unwrap_or_else(|e| e.into_inner());
        warned.insert(key.into())
    };
    if first {
        eprintln!("{line}");
    }
}

/// Clear the process-wide warn-once set. Mirrors
/// `features::reset_feature_warnings_for_tests`.
#[cfg(test)]
pub(crate) fn reset_reap_warnings_for_tests() {
    let mut warned = WARNED.lock().unwrap_or_else(|e| e.into_inner());
    warned.clear();
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReapSkip {
    FeatureOff,
    MissingRunState,
    UnreadableState,
    LockFailed,
    Active,
    TooYoung,
    RecentlyActive,
    MissingPersist,
    UnreadablePersist,
    NotAlive,
    PromptInFlight,
    NoHolderPid,
    SelfPid,
    NoSession,
    NoCwd,
    NoControlAddr,
    CwdNotOwned,
    IdentityUnproven(String),
    ShutdownUnproven(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReapOutcome {
    Skipped(ReapSkip),
    Reaped {
        holder_pid: u32,
        session_id: String,
        status: &'static str,
        age_secs: u64,
    },
}

/// Decide one project. The live feature gate and the run-state presence check
/// run first; all state/persist decisions then run under the project's
/// run-state lock so a concurrent `run` entry cannot flip the status.
pub(crate) fn tick_project(record: &ProjectRecord, now: DateTime<Utc>) -> ReapOutcome {
    if !features::is_enabled(Duty::SelfReap) {
        return ReapOutcome::Skipped(ReapSkip::FeatureOff);
    }
    let state_path = match run_state_path(record) {
        Ok(path) => path,
        Err(e) => {
            warn_once(
                format!("{}:unreadable-state", record.id),
                &format!(
                    "coordinator: holder-reap: project {} state path unreadable: {e}",
                    record.id
                ),
            );
            return ReapOutcome::Skipped(ReapSkip::UnreadableState);
        }
    };
    // `load_run_state` synthesizes an Idle stub for a missing file; a missing
    // run-state is unknown, not terminal.
    if !state_path.exists() {
        return ReapOutcome::Skipped(ReapSkip::MissingRunState);
    }
    match with_run_state_lock(record, || Ok(tick_locked(record, now))) {
        Ok(outcome) => outcome,
        Err(e) => {
            warn_once(
                format!("{}:lock", record.id),
                &format!(
                    "coordinator: holder-reap: project {} run-state lock failed: {e}",
                    record.id
                ),
            );
            ReapOutcome::Skipped(ReapSkip::LockFailed)
        }
    }
}

fn tick_locked(record: &ProjectRecord, now: DateTime<Utc>) -> ReapOutcome {
    let state = match load_run_state(record) {
        Ok(state) => state,
        Err(e) => {
            warn_once(
                format!("{}:unreadable-state", record.id),
                &format!(
                    "coordinator: holder-reap: project {} run-state unreadable: {e}",
                    record.id
                ),
            );
            return ReapOutcome::Skipped(ReapSkip::UnreadableState);
        }
    };
    match state.status {
        RunStatus::Running | RunStatus::Paused => return ReapOutcome::Skipped(ReapSkip::Active),
        RunStatus::Idle | RunStatus::Stopped => {}
    }
    if !old_enough(now, state.updated_at) {
        return ReapOutcome::Skipped(ReapSkip::TooYoung);
    }

    let (identity, mtime) = match pool::persist_identity(record) {
        Ok(Some(found)) => found,
        Ok(None) => return ReapOutcome::Skipped(ReapSkip::MissingPersist),
        Err(e) => {
            warn_once(
                format!("{}:unreadable-persist", record.id),
                &format!(
                    "coordinator: holder-reap: project {} persist unreadable: {e}",
                    record.id
                ),
            );
            return ReapOutcome::Skipped(ReapSkip::UnreadablePersist);
        }
    };
    if identity.project_id != record.id {
        return ReapOutcome::Skipped(ReapSkip::IdentityUnproven("project-id".into()));
    }
    if !identity.alive {
        return ReapOutcome::Skipped(ReapSkip::NotAlive);
    }
    if identity.prompt_in_flight {
        return ReapOutcome::Skipped(ReapSkip::PromptInFlight);
    }
    match identity.holder_pid {
        None | Some(0) => return ReapOutcome::Skipped(ReapSkip::NoHolderPid),
        Some(pid) if pid == std::process::id() => {
            return ReapOutcome::Skipped(ReapSkip::SelfPid);
        }
        Some(_) => {}
    }
    if identity.session_id.is_none() {
        return ReapOutcome::Skipped(ReapSkip::NoSession);
    }
    if identity.cwd.is_none() {
        return ReapOutcome::Skipped(ReapSkip::NoCwd);
    }
    if identity.control_addr.is_none() {
        return ReapOutcome::Skipped(ReapSkip::NoControlAddr);
    }
    if !old_enough(now, DateTime::<Utc>::from(mtime)) {
        return ReapOutcome::Skipped(ReapSkip::RecentlyActive);
    }
    if !cwd_is_owned(record, &identity) {
        return ReapOutcome::Skipped(ReapSkip::CwdNotOwned);
    }

    match pool::verify_holder_identity(&identity, RPC_TIMEOUT) {
        HolderReapRpc::Verified => {}
        HolderReapRpc::IdentityUnproven(why) => {
            warn_once(
                format!(
                    "{}:identity:{}",
                    record.id,
                    identity.session_id.as_deref().unwrap_or("")
                ),
                &format!(
                    "coordinator: holder-reap: project {} identity unproven ({why}); keeping holder",
                    record.id
                ),
            );
            return ReapOutcome::Skipped(ReapSkip::IdentityUnproven(why));
        }
        HolderReapRpc::ShutdownUnproven(why) => {
            warn_once(
                format!(
                    "{}:shutdown:{}",
                    record.id,
                    identity.session_id.as_deref().unwrap_or("")
                ),
                &format!(
                    "coordinator: holder-reap: project {} shutdown unproven ({why}); keeping holder",
                    record.id
                ),
            );
            return ReapOutcome::Skipped(ReapSkip::ShutdownUnproven(why));
        }
    }

    // Gate 6: re-read the persist immediately before termination. Classify in a
    // fixed order so the manual-use double-check is deterministic.
    let (again, mtime_again) = match pool::persist_identity(record) {
        Ok(Some(found)) => found,
        Ok(None) => return ReapOutcome::Skipped(ReapSkip::MissingPersist),
        Err(e) => {
            warn_once(
                format!("{}:unreadable-persist", record.id),
                &format!(
                    "coordinator: holder-reap: project {} persist unreadable on re-read: {e}",
                    record.id
                ),
            );
            return ReapOutcome::Skipped(ReapSkip::UnreadablePersist);
        }
    };
    if !again.alive {
        return ReapOutcome::Skipped(ReapSkip::NotAlive);
    }
    if again.prompt_in_flight {
        return ReapOutcome::Skipped(ReapSkip::PromptInFlight);
    }
    if mtime_again != mtime {
        return ReapOutcome::Skipped(ReapSkip::RecentlyActive);
    }
    if again != identity {
        return ReapOutcome::Skipped(ReapSkip::IdentityUnproven("persist-changed".into()));
    }

    match pool::request_holder_shutdown(&identity, RPC_TIMEOUT) {
        HolderReapRpc::Verified => {
            let holder_pid = identity.holder_pid.unwrap_or_default();
            let session_id = identity.session_id.clone().unwrap_or_default();
            let status = state.status.as_str();
            let age_secs = age_secs(now, state.updated_at);
            let age_h = age_secs / 3600;
            crate::progress_log::append(
                record,
                "holder-reap",
                &format!("pid={holder_pid} session={session_id} status={status} age_h={age_h}"),
            );
            eprintln!(
                "coordinator: holder-reap: project={} pid={holder_pid} adapter={} status={status} age_h={age_h}",
                record.id, identity.adapter
            );
            ReapOutcome::Reaped {
                holder_pid,
                session_id,
                status,
                age_secs,
            }
        }
        HolderReapRpc::ShutdownUnproven(why) => {
            warn_once(
                format!(
                    "{}:shutdown:{}",
                    record.id,
                    identity.session_id.as_deref().unwrap_or("")
                ),
                &format!(
                    "coordinator: holder-reap: project {} shutdown unproven ({why}); keeping holder",
                    record.id
                ),
            );
            ReapOutcome::Skipped(ReapSkip::ShutdownUnproven(why))
        }
        HolderReapRpc::IdentityUnproven(why) => {
            warn_once(
                format!(
                    "{}:identity:{}",
                    record.id,
                    identity.session_id.as_deref().unwrap_or("")
                ),
                &format!(
                    "coordinator: holder-reap: project {} identity unproven ({why}); keeping holder",
                    record.id
                ),
            );
            ReapOutcome::Skipped(ReapSkip::IdentityUnproven(why))
        }
    }
}

/// Positive age check: a backwards clock (negative age) is never old enough.
fn old_enough(now: DateTime<Utc>, then: DateTime<Utc>) -> bool {
    let delta = now.signed_duration_since(then);
    delta.num_seconds() >= 0 && delta.to_std().is_ok_and(|age| age >= STALE_AFTER)
}

fn age_secs(now: DateTime<Utc>, then: DateTime<Utc>) -> u64 {
    let delta = now.signed_duration_since(then);
    if delta.num_seconds() < 0 {
        0
    } else {
        delta.num_seconds() as u64
    }
}

/// True when the persist `cwd` is equal to or under one of the project roots.
/// Equality is the normal nested/multi-sibling shape, so it is mandatory.
fn cwd_is_owned(record: &ProjectRecord, identity: &PersistIdentity) -> bool {
    let Some(cwd) = identity.cwd.as_deref() else {
        return false;
    };
    let paths = crate::layout::resolve(record);
    let mut roots = vec![paths.workspace_root];
    if let Some(execution_repo) = paths.execution_repo {
        roots.push(execution_repo);
    }
    roots.extend(paths.execution_repos.into_values());
    match resolve_state_dir(record) {
        Ok(state_dir) => roots.push(state_dir),
        Err(_) => return false,
    }
    roots.iter().any(|root| owned(cwd, root))
}

fn owned(cwd: &Path, root: &Path) -> bool {
    // Reject traversal components before any lexical prefix test.
    if cwd
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return false;
    }
    match (cwd.canonicalize(), root.canonicalize()) {
        (Ok(cwd), Ok(root)) => {
            crate::worktree::paths_same(&cwd, &root) || crate::worktree::path_is_inside(&cwd, &root)
        }
        _ => crate::worktree::paths_same(cwd, root) || crate::worktree::path_is_inside(cwd, root),
    }
}

/// Sweep every registered project. A registry read failure yields no outcomes.
pub fn sweep(now: DateTime<Utc>) -> Vec<(String, ReapOutcome)> {
    match crate::api::load_registry() {
        Ok(registry) => registry
            .list()
            .iter()
            .map(|record| (record.id.clone(), tick_project(record, now)))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Due-gate seam: `None` is due (serve start), an interval of one hour is due,
/// a backwards clock is not due.
pub(crate) fn due(last: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    match last {
        None => true,
        Some(last) => {
            let delta = now.signed_duration_since(last);
            delta.num_seconds() >= 0 && delta.to_std().is_ok_and(|age| age >= SWEEP_INTERVAL)
        }
    }
}

/// One serve tick. The gate-off path returns before stamping, so enabling the
/// gate takes effect on the next tick.
pub fn tick(last: &mut Option<DateTime<Utc>>, now: DateTime<Utc>) {
    if !features::is_enabled(Duty::SelfReap) {
        return;
    }
    if !due(*last, now) {
        return;
    }
    *last = Some(now);
    let _ = sweep(now);
}

/// Async serve wrapper: stamp on the caller side, then sweep on a blocking
/// worker (the RPC and the run-state lock are synchronous).
pub async fn tick_async(last: &mut Option<DateTime<Utc>>, now: DateTime<Utc>) -> Result<()> {
    if !features::is_enabled(Duty::SelfReap) {
        return Ok(());
    }
    if !due(*last, now) {
        return Ok(());
    }
    *last = Some(now);
    let _ = tokio::task::spawn_blocking(move || sweep(now))
        .await
        .map_err(|e| CoordinatorError::Message(format!("holder reap task join: {e}")))?;
    Ok(())
}

/// Best-effort shutdown sweep: ignores the due stamp, still honours the gate and
/// every ownership gate. A join failure is reported and swallowed so shutdown
/// is never blocked.
pub async fn sweep_on_shutdown_async() -> Result<()> {
    if !features::is_enabled(Duty::SelfReap) {
        return Ok(());
    }
    match tokio::task::spawn_blocking(|| sweep(Utc::now())).await {
        Ok(_) => Ok(()),
        Err(e) => {
            eprintln!("coordinator: holder-reap: shutdown sweep join failed: {e}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ENV_COORDINATOR_HOME, ENV_COORDINATOR_STATE_DIR, TestEnvGuard, test_env_lock,
    };
    use crate::layout::LayoutProfile;
    use crate::registry::{AutoStartPolicy, ProjectRecord, Registry};
    use crate::state::RunState;
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex as StdMutex, mpsc};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant, SystemTime};
    use tempfile::TempDir;

    const SELF_REAP_ENV: &str = "COORDINATOR_FEATURE_SELF_REAP";

    fn unique_id() -> String {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        format!("reap-{}", NEXT.fetch_add(1, Ordering::SeqCst))
    }

    fn test_holder_pid() -> u32 {
        if std::process::id() == 4242 {
            4243
        } else {
            4242
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum HolderBehaviour {
        Normal,
        MismatchSession,
        MismatchCwd,
        MismatchAdapter,
        MismatchPid,
        PingNoResponse,
        ShutdownNoResponse,
        NoResponse,
        MutatePersistOnPing,
    }

    struct HolderSpec {
        session_id: String,
        cwd: PathBuf,
        pid: Option<u32>,
        adapter: String,
        behaviour: HolderBehaviour,
        persist_path: Option<PathBuf>,
        pin_path: Option<PathBuf>,
    }

    struct ScriptedHolder {
        addr: String,
        session_id: String,
        cwd: PathBuf,
        pid: Option<u32>,
        adapter: String,
        ops: Arc<StdMutex<Vec<String>>>,
        handle: Option<JoinHandle<()>>,
    }

    impl ScriptedHolder {
        fn start(spec: HolderSpec) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap().to_string();
            let ops = Arc::new(StdMutex::new(Vec::new()));
            let ops_thread = ops.clone();
            let (ready_tx, ready_rx) = mpsc::channel();
            let session_id = spec.session_id.clone();
            let cwd = spec.cwd.clone();
            let pid = spec.pid;
            let adapter = spec.adapter.clone();
            let handle = std::thread::spawn(move || {
                let pin = spec.pin_path.as_deref().and_then(open_pinned);
                ready_tx.send(()).ok();
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { break };
                    let Ok(mut reader) = stream.try_clone().map(BufReader::new) else {
                        continue;
                    };
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        continue;
                    }
                    let Ok(request) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                        continue;
                    };
                    let op = request
                        .get("op")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    ops_thread
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(op.clone());
                    if op == "stop-test" {
                        break;
                    }
                    match op.as_str() {
                        "ping" => {
                            if matches!(
                                spec.behaviour,
                                HolderBehaviour::NoResponse | HolderBehaviour::PingNoResponse
                            ) {
                                std::thread::sleep(Duration::from_millis(150));
                                continue;
                            }
                            if spec.behaviour == HolderBehaviour::MutatePersistOnPing {
                                mutate_persist(&spec);
                            }
                            respond(&mut stream, &ping_body(&spec));
                        }
                        "shutdown" => {
                            if matches!(
                                spec.behaviour,
                                HolderBehaviour::NoResponse | HolderBehaviour::ShutdownNoResponse
                            ) {
                                std::thread::sleep(Duration::from_millis(150));
                                continue;
                            }
                            respond(&mut stream, &shutdown_body(&spec));
                            if let Some(path) = &spec.persist_path {
                                write_dead_persist(path);
                            }
                            break;
                        }
                        _ => {}
                    }
                }
                drop(pin);
            });
            ready_rx.recv().unwrap();
            Self {
                addr,
                session_id,
                cwd,
                pid,
                adapter,
                ops,
                handle: Some(handle),
            }
        }

        fn saw(&self, op: &str) -> bool {
            self.seen_ops().iter().any(|seen| seen == op)
        }

        fn seen_ops(&self) -> Vec<String> {
            self.ops.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }

        fn is_finished(&self) -> bool {
            self.handle
                .as_ref()
                .map(JoinHandle::is_finished)
                .unwrap_or(true)
        }

        fn ping(&self) -> bool {
            let Ok(sock) = self.addr.parse::<std::net::SocketAddr>() else {
                return false;
            };
            let Ok(mut stream) = TcpStream::connect_timeout(&sock, Duration::from_millis(750))
            else {
                return false;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_millis(750)));
            if stream.write_all(b"{\"op\":\"ping\"}\n").is_err() {
                return false;
            }
            let mut line = String::new();
            if BufReader::new(stream).read_line(&mut line).unwrap_or(0) == 0 {
                return false;
            }
            serde_json::from_str::<serde_json::Value>(line.trim())
                .map(|body| body["ok"] == true)
                .unwrap_or(false)
        }

        fn join(&mut self, timeout: Duration) {
            let Some(handle) = self.handle.take() else {
                return;
            };
            let deadline = Instant::now() + timeout;
            while !handle.is_finished() {
                if Instant::now() >= deadline {
                    panic!("scripted holder did not exit");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = handle.join();
        }

        /// Consume the holder after the expected `Shutdown` delivery and bound the
        /// thread join. The holder exits on its own; the reaper never waits on a PID.
        fn finish(mut self) {
            self.join(Duration::from_secs(2));
        }

        fn stop(&mut self) {
            if !self.is_finished()
                && let Ok(mut stream) = TcpStream::connect(&self.addr)
            {
                let _ = stream.write_all(b"{\"op\":\"stop-test\"}\n");
            }
            self.join(Duration::from_secs(2));
        }
    }

    fn open_pinned(path: &Path) -> Option<std::fs::File> {
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(path)
                .ok()
        }
        #[cfg(not(windows))]
        {
            std::fs::File::open(path).ok()
        }
    }

    fn respond(stream: &mut TcpStream, body: &serde_json::Value) {
        let payload = serde_json::to_string(body).unwrap();
        let _ = stream.write_all(payload.as_bytes());
        let _ = stream.write_all(b"\n");
        let _ = stream.flush();
    }

    fn ping_body(spec: &HolderSpec) -> serde_json::Value {
        let session_id = if spec.behaviour == HolderBehaviour::MismatchSession {
            "mismatched-session".to_string()
        } else {
            spec.session_id.clone()
        };
        let cwd = if spec.behaviour == HolderBehaviour::MismatchCwd {
            spec.cwd.join("elsewhere")
        } else {
            spec.cwd.clone()
        };
        let adapter = if spec.behaviour == HolderBehaviour::MismatchAdapter {
            "mismatched-adapter".to_string()
        } else {
            spec.adapter.clone()
        };
        let pid = if spec.behaviour == HolderBehaviour::MismatchPid {
            spec.pid.map(|pid| pid.wrapping_add(1))
        } else {
            spec.pid
        };
        serde_json::json!({
            "ok": true,
            "harness": {
                "alive": true,
                "session_id": session_id,
                "cwd": cwd,
                "pid": pid,
                "adapter": adapter,
                "supports_compact": true,
            }
        })
    }

    fn shutdown_body(spec: &HolderSpec) -> serde_json::Value {
        let mut body = ping_body(spec);
        body["harness"]["alive"] = serde_json::Value::Bool(false);
        body
    }

    fn mutate_persist(spec: &HolderSpec) {
        let Some(path) = &spec.persist_path else {
            return;
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(mut body) = serde_json::from_str::<serde_json::Value>(&text) else {
            return;
        };
        body["prompt_in_flight"] = serde_json::Value::Bool(true);
        let _ = std::fs::write(path, serde_json::to_vec(&body).unwrap());
    }

    fn write_dead_persist(path: &Path) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(mut body) = serde_json::from_str::<serde_json::Value>(&text) else {
            return;
        };
        body["alive"] = serde_json::Value::Bool(false);
        body["control_addr"] = serde_json::Value::Null;
        body["prompt_in_flight"] = serde_json::Value::Bool(false);
        let _ = std::fs::write(path, serde_json::to_vec(&body).unwrap());
    }

    struct PersistSpec {
        session_id: Option<String>,
        cwd: Option<PathBuf>,
        pid: Option<u32>,
        holder_pid: Option<u32>,
        adapter: String,
        alive: bool,
        prompt_in_flight: bool,
        control_addr: Option<String>,
    }

    impl PersistSpec {
        fn matching(holder: &ScriptedHolder) -> Self {
            Self {
                session_id: Some(holder.session_id.clone()),
                cwd: Some(holder.cwd.clone()),
                pid: holder.pid,
                holder_pid: Some(test_holder_pid()),
                adapter: holder.adapter.clone(),
                alive: true,
                prompt_in_flight: false,
                control_addr: Some(holder.addr.clone()),
            }
        }
    }

    struct Fixture {
        _home: TempDir,
        _ws: TempDir,
        _state: TempDir,
        _env: TestEnvGuard,
        record: ProjectRecord,
        persist_path: PathBuf,
        run_state_path: PathBuf,
    }

    impl Fixture {
        fn new(now: DateTime<Utc>, status: RunStatus, age: Duration) -> Self {
            let home = TempDir::new().unwrap();
            let workspace = TempDir::new().unwrap();
            let state = TempDir::new().unwrap();
            let env = TestEnvGuard::capture(&[
                ENV_COORDINATOR_HOME,
                ENV_COORDINATOR_STATE_DIR,
                SELF_REAP_ENV,
            ]);
            unsafe {
                std::env::set_var(ENV_COORDINATOR_HOME, home.path());
                std::env::remove_var(ENV_COORDINATOR_STATE_DIR);
                std::env::remove_var(SELF_REAP_ENV);
            }
            reset_reap_warnings_for_tests();
            let record = ProjectRecord {
                id: unique_id(),
                path: workspace.path().to_path_buf(),
                display_name: None,
                layout_profile: LayoutProfile::Nested,
                conductor_dir: None,
                execution_repo: None,
                execution_repos: BTreeMap::new(),
                state_dir: Some(state.path().to_path_buf()),
                auto_merge: false,
                phase_timeouts_secs: BTreeMap::new(),
                notify_progress: false,
                worktree_isolation: false,
                ready_aliases: Vec::new(),
                auto_start: AutoStartPolicy::Never,
                state_policies: Vec::new(),
                self_continuation: false,
                ci_fix_routing: false,
                skill_aliases: BTreeMap::new(),
                created_at: now,
            };
            let run_state_path = crate::state::run_state_path(&record).unwrap();
            let mut run = RunState::idle(&record.id);
            run.status = status;
            run.updated_at = now - chrono::Duration::from_std(age).unwrap();
            run.phase = "stub:test".into();
            run.last_event = "reap fixture".into();
            std::fs::write(&run_state_path, serde_json::to_vec(&run).unwrap()).unwrap();
            let persist_path = crate::harness::pool::persist_path(&record).unwrap();
            Self {
                _home: home,
                _ws: workspace,
                _state: state,
                _env: env,
                record,
                persist_path,
                run_state_path,
            }
        }

        fn holder(&self, behaviour: HolderBehaviour) -> ScriptedHolder {
            self.holder_with(
                behaviour,
                self.record.path.clone(),
                Some(7000),
                "claude",
                None,
            )
        }

        fn holder_with(
            &self,
            behaviour: HolderBehaviour,
            cwd: PathBuf,
            pid: Option<u32>,
            adapter: &str,
            pin_path: Option<PathBuf>,
        ) -> ScriptedHolder {
            ScriptedHolder::start(HolderSpec {
                session_id: format!("session-{}", self.record.id),
                cwd,
                pid,
                adapter: adapter.to_string(),
                behaviour,
                persist_path: Some(self.persist_path.clone()),
                pin_path,
            })
        }

        fn write_persist(
            &self,
            _holder: &ScriptedHolder,
            spec: PersistSpec,
            persist_age: Duration,
        ) {
            let body = serde_json::json!({
                "version": 1,
                "project_id": self.record.id,
                "session_id": spec.session_id,
                "cwd": spec.cwd,
                "pid": spec.pid,
                "holder_pid": spec.holder_pid,
                "control_addr": spec.control_addr,
                "adapter": spec.adapter,
                "supports_compact": true,
                "alive": spec.alive,
                "prompt_in_flight": spec.prompt_in_flight,
            });
            std::fs::write(&self.persist_path, serde_json::to_vec(&body).unwrap()).unwrap();
            self.set_persist_mtime(persist_age);
        }

        fn write_raw_persist(&self, body: &str, persist_age: Duration) {
            std::fs::write(&self.persist_path, body).unwrap();
            self.set_persist_mtime(persist_age);
        }

        fn set_persist_mtime(&self, persist_age: Duration) {
            let mtime = SystemTime::now()
                .checked_sub(persist_age)
                .unwrap_or_else(SystemTime::now);
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&self.persist_path)
                .unwrap();
            file.set_modified(mtime).unwrap();
        }

        fn remove_persist(&self) {
            let _ = std::fs::remove_file(&self.persist_path);
        }

        fn save_registry(&self) {
            let mut registry = Registry::default();
            registry.projects.push(self.record.clone());
            registry
                .save(&crate::config::registry_path().unwrap())
                .unwrap();
        }

        fn run_state_bytes(&self) -> Vec<u8> {
            std::fs::read(&self.run_state_path).unwrap()
        }

        fn status_log(&self) -> String {
            std::fs::read_to_string(self.record.path.join("status.md")).unwrap_or_default()
        }
    }

    fn persist_old() -> Duration {
        Duration::from_secs(7 * 60 * 60)
    }

    fn assert_skipped(outcome: ReapOutcome, expected: ReapSkip) {
        assert_eq!(outcome, ReapOutcome::Skipped(expected));
    }

    fn assert_reaped(outcome: &ReapOutcome, holder: &ScriptedHolder) -> u64 {
        match outcome {
            ReapOutcome::Reaped {
                holder_pid,
                session_id,
                status: _,
                age_secs,
            } => {
                assert_eq!(*holder_pid, test_holder_pid());
                assert_eq!(session_id, &holder.session_id);
                *age_secs
            }
            other => panic!("expected Reaped, got {other:?}"),
        }
    }

    // ---- DoD-1 -------------------------------------------------------------

    #[test]
    fn reap_shuts_down_terminal_run_holder() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let holder = fixture.holder(HolderBehaviour::Normal);
        fixture.write_persist(&holder, PersistSpec::matching(&holder), persist_old());

        let outcome = tick_project(&fixture.record, now);
        let age_secs = assert_reaped(&outcome, &holder);
        assert!(age_secs >= STALE_AFTER.as_secs());
        assert_eq!(holder.seen_ops(), vec!["ping", "shutdown"]);
        holder.finish();
        let (dead, _) = pool::persist_identity(&fixture.record).unwrap().unwrap();
        assert!(!dead.alive, "holder writes its own dead persist");
        assert!(dead.control_addr.is_none());
    }

    #[test]
    fn reap_keeps_running_run_holder() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Running, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::Normal);
        fixture.write_persist(&holder, PersistSpec::matching(&holder), persist_old());

        assert_skipped(tick_project(&fixture.record, now), ReapSkip::Active);
        assert!(holder.seen_ops().is_empty());
        assert!(holder.ping());
        holder.stop();
    }

    #[test]
    fn reap_keeps_paused_run_holder() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Paused, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::Normal);
        fixture.write_persist(&holder, PersistSpec::matching(&holder), persist_old());

        assert_skipped(tick_project(&fixture.record, now), ReapSkip::Active);
        assert!(holder.seen_ops().is_empty());
        holder.stop();
    }

    // ---- DoD-2 -------------------------------------------------------------

    #[test]
    fn reap_keeps_unreadable_run_state() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::Normal);
        fixture.write_persist(&holder, PersistSpec::matching(&holder), persist_old());
        std::fs::write(&fixture.run_state_path, b"{not json").unwrap();

        assert_skipped(
            tick_project(&fixture.record, now),
            ReapSkip::UnreadableState,
        );
        assert!(holder.seen_ops().is_empty());
        holder.stop();
    }

    #[test]
    fn reap_keeps_missing_run_state() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::Normal);
        fixture.write_persist(&holder, PersistSpec::matching(&holder), persist_old());
        std::fs::remove_file(&fixture.run_state_path).unwrap();

        assert_skipped(
            tick_project(&fixture.record, now),
            ReapSkip::MissingRunState,
        );
        assert!(holder.seen_ops().is_empty());
        holder.stop();
    }

    #[test]
    fn reap_keeps_too_young_terminal_run() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, Duration::from_secs(60 * 60));
        let mut holder = fixture.holder(HolderBehaviour::Normal);
        fixture.write_persist(&holder, PersistSpec::matching(&holder), persist_old());

        assert_skipped(tick_project(&fixture.record, now), ReapSkip::TooYoung);
        assert!(holder.seen_ops().is_empty());
        holder.stop();
    }

    #[test]
    fn reap_keeps_recently_active_persist() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::Normal);
        fixture.write_persist(&holder, PersistSpec::matching(&holder), Duration::ZERO);

        assert_skipped(tick_project(&fixture.record, now), ReapSkip::RecentlyActive);
        assert!(holder.seen_ops().is_empty());
        holder.stop();
    }

    #[test]
    fn reap_keeps_prompt_in_flight() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::Normal);
        let mut spec = PersistSpec::matching(&holder);
        spec.prompt_in_flight = true;
        fixture.write_persist(&holder, spec, persist_old());

        assert_skipped(tick_project(&fixture.record, now), ReapSkip::PromptInFlight);
        assert!(holder.seen_ops().is_empty());
        holder.stop();
    }

    #[test]
    fn reap_keeps_project_id_mismatch() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::Normal);
        let body = serde_json::json!({
            "version": 1,
            "project_id": "some-other-project",
            "session_id": holder.session_id,
            "cwd": holder.cwd,
            "pid": holder.pid,
            "holder_pid": test_holder_pid(),
            "control_addr": holder.addr,
            "adapter": holder.adapter,
            "supports_compact": true,
            "alive": true,
            "prompt_in_flight": false,
        });
        fixture.write_raw_persist(&serde_json::to_string(&body).unwrap(), persist_old());

        match tick_project(&fixture.record, now) {
            ReapOutcome::Skipped(ReapSkip::IdentityUnproven(reason)) => {
                assert_eq!(reason, "project-id")
            }
            other => panic!("expected IdentityUnproven(project-id), got {other:?}"),
        }
        assert!(holder.seen_ops().is_empty());
        holder.stop();
    }

    #[test]
    fn reap_keeps_dead_persist() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::Normal);
        let mut spec = PersistSpec::matching(&holder);
        spec.alive = false;
        fixture.write_persist(&holder, spec, persist_old());

        assert_skipped(tick_project(&fixture.record, now), ReapSkip::NotAlive);
        assert!(holder.seen_ops().is_empty());
        holder.stop();
    }

    #[test]
    fn reap_keeps_missing_persist() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::Normal);

        assert_skipped(tick_project(&fixture.record, now), ReapSkip::MissingPersist);
        assert!(holder.seen_ops().is_empty());
        holder.stop();
    }

    #[test]
    fn reap_keeps_unreadable_persist() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::Normal);
        fixture.write_raw_persist("{not json", persist_old());

        assert_skipped(
            tick_project(&fixture.record, now),
            ReapSkip::UnreadablePersist,
        );
        assert!(holder.seen_ops().is_empty());
        holder.stop();
    }

    #[test]
    fn reap_keeps_lock_failed() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::Normal);
        fixture.write_persist(&holder, PersistSpec::matching(&holder), persist_old());
        let lock_path = crate::state::resolve_state_dir(&fixture.record)
            .unwrap()
            .join(".run-state.lock");
        std::fs::create_dir_all(&lock_path).unwrap();
        let _deadline = crate::state::test_run_state_lock_wait(Duration::from_millis(50));

        assert_skipped(tick_project(&fixture.record, now), ReapSkip::LockFailed);
        assert!(holder.seen_ops().is_empty());
        std::fs::remove_dir_all(&lock_path).unwrap();
        holder.stop();
    }

    #[test]
    fn reap_keeps_no_holder_pid() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::Normal);
        let mut spec = PersistSpec::matching(&holder);
        spec.holder_pid = None;
        fixture.write_persist(&holder, spec, persist_old());

        assert_skipped(tick_project(&fixture.record, now), ReapSkip::NoHolderPid);
        assert!(holder.seen_ops().is_empty());
        holder.stop();
    }

    #[test]
    fn reap_keeps_no_session() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::Normal);
        let mut spec = PersistSpec::matching(&holder);
        spec.session_id = None;
        fixture.write_persist(&holder, spec, persist_old());

        assert_skipped(tick_project(&fixture.record, now), ReapSkip::NoSession);
        assert!(holder.seen_ops().is_empty());
        holder.stop();
    }

    #[test]
    fn reap_keeps_no_cwd() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::Normal);
        let mut spec = PersistSpec::matching(&holder);
        spec.cwd = None;
        fixture.write_persist(&holder, spec, persist_old());

        assert_skipped(tick_project(&fixture.record, now), ReapSkip::NoCwd);
        assert!(holder.seen_ops().is_empty());
        holder.stop();
    }

    #[test]
    fn reap_keeps_no_control_addr() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::Normal);
        let mut spec = PersistSpec::matching(&holder);
        spec.control_addr = None;
        fixture.write_persist(&holder, spec, persist_old());

        assert_skipped(tick_project(&fixture.record, now), ReapSkip::NoControlAddr);
        assert!(holder.seen_ops().is_empty());
        holder.stop();
    }

    #[test]
    fn reap_keeps_self_pid() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::Normal);
        let mut spec = PersistSpec::matching(&holder);
        spec.holder_pid = Some(std::process::id());
        fixture.write_persist(&holder, spec, persist_old());

        assert_skipped(tick_project(&fixture.record, now), ReapSkip::SelfPid);
        assert!(holder.seen_ops().is_empty());
        holder.stop();
    }

    #[test]
    fn reap_keeps_cwd_outside_project_roots() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let outside = TempDir::new().unwrap();
        let mut holder = fixture.holder_with(
            HolderBehaviour::Normal,
            outside.path().to_path_buf(),
            Some(7000),
            "claude",
            None,
        );
        fixture.write_persist(&holder, PersistSpec::matching(&holder), persist_old());

        assert_skipped(tick_project(&fixture.record, now), ReapSkip::CwdNotOwned);
        assert!(holder.seen_ops().is_empty());
        holder.stop();
    }

    #[test]
    fn reap_cwd_equal_to_execution_root_is_owned() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let mut fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let execution = TempDir::new().unwrap();
        fixture.record.execution_repo = Some(execution.path().to_path_buf());
        let holder = fixture.holder_with(
            HolderBehaviour::Normal,
            execution.path().to_path_buf(),
            Some(7000),
            "claude",
            None,
        );
        fixture.write_persist(&holder, PersistSpec::matching(&holder), persist_old());

        let outcome = tick_project(&fixture.record, now);
        assert_reaped(&outcome, &holder);
        holder.finish();
    }

    #[test]
    fn reap_keeps_identity_mismatch_session() {
        identity_mismatch_case(HolderBehaviour::MismatchSession);
    }

    #[test]
    fn reap_keeps_identity_mismatch_cwd() {
        identity_mismatch_case(HolderBehaviour::MismatchCwd);
    }

    #[test]
    fn reap_keeps_identity_mismatch_adapter() {
        identity_mismatch_case(HolderBehaviour::MismatchAdapter);
    }

    #[test]
    fn reap_keeps_identity_mismatch_pid() {
        identity_mismatch_case(HolderBehaviour::MismatchPid);
    }

    fn identity_mismatch_case(behaviour: HolderBehaviour) {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(behaviour);
        fixture.write_persist(&holder, PersistSpec::matching(&holder), persist_old());

        match tick_project(&fixture.record, now) {
            ReapOutcome::Skipped(ReapSkip::IdentityUnproven(_)) => {}
            other => panic!("expected IdentityUnproven, got {other:?}"),
        }
        assert_eq!(holder.seen_ops(), vec!["ping"]);
        assert!(!holder.is_finished());
        holder.stop();
    }

    #[test]
    fn reap_keeps_ping_timeout() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::PingNoResponse);
        fixture.write_persist(&holder, PersistSpec::matching(&holder), persist_old());

        match tick_project(&fixture.record, now) {
            ReapOutcome::Skipped(ReapSkip::IdentityUnproven(_)) => {}
            other => panic!("expected IdentityUnproven, got {other:?}"),
        }
        assert_eq!(holder.seen_ops(), vec!["ping"]);
        holder.stop();
    }

    #[test]
    fn reap_keeps_no_response_at_all() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::NoResponse);
        fixture.write_persist(&holder, PersistSpec::matching(&holder), persist_old());

        match tick_project(&fixture.record, now) {
            ReapOutcome::Skipped(ReapSkip::IdentityUnproven(_)) => {}
            other => panic!("expected IdentityUnproven, got {other:?}"),
        }
        assert_eq!(holder.seen_ops(), vec!["ping"]);
        holder.stop();
    }

    #[test]
    fn reap_keeps_when_shutdown_unanswered() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::ShutdownNoResponse);
        fixture.write_persist(&holder, PersistSpec::matching(&holder), persist_old());

        match tick_project(&fixture.record, now) {
            ReapOutcome::Skipped(ReapSkip::ShutdownUnproven(_)) => {}
            other => panic!("expected ShutdownUnproven, got {other:?}"),
        }
        assert_eq!(holder.seen_ops(), vec!["ping", "shutdown"]);
        assert!(!holder.is_finished());
        holder.stop();
    }

    #[test]
    fn reap_feature_off_performs_no_rpc() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::Normal);
        fixture.write_persist(&holder, PersistSpec::matching(&holder), persist_old());
        unsafe { std::env::set_var(SELF_REAP_ENV, "0") };

        assert_skipped(tick_project(&fixture.record, now), ReapSkip::FeatureOff);
        assert!(holder.seen_ops().is_empty());
        holder.stop();
    }

    #[test]
    fn reap_due_gate_first_tick_then_hour() {
        let now = Utc::now();
        assert!(due(None, now));
        assert!(!due(Some(now - chrono::Duration::minutes(59)), now));
        assert!(due(Some(now - chrono::Duration::minutes(61)), now));
        assert!(!due(Some(now + chrono::Duration::minutes(5)), now));
    }

    #[test]
    fn reap_gate_off_does_not_stamp() {
        let _guard = test_env_lock();
        let _env = TestEnvGuard::capture(&[SELF_REAP_ENV, ENV_COORDINATOR_HOME]);
        let home = TempDir::new().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(SELF_REAP_ENV, "0");
        }
        let now = Utc::now();
        let mut last = None;
        tick(&mut last, now);
        assert!(last.is_none());
        unsafe { std::env::remove_var(SELF_REAP_ENV) };
        tick(&mut last, now);
        assert_eq!(last, Some(now));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)]
    async fn reap_shutdown_sweep_ignores_due_stamp() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::Normal);
        fixture.write_persist(&holder, PersistSpec::matching(&holder), persist_old());
        fixture.save_registry();

        sweep_on_shutdown_async().await.unwrap();
        assert!(holder.saw("shutdown"));
        holder.stop();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)]
    async fn reap_shutdown_sweep_gate_off_does_nothing() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::Normal);
        fixture.write_persist(&holder, PersistSpec::matching(&holder), persist_old());
        fixture.save_registry();
        unsafe { std::env::set_var(SELF_REAP_ENV, "0") };

        sweep_on_shutdown_async().await.unwrap();
        assert!(holder.seen_ops().is_empty());
        holder.stop();
    }

    #[test]
    fn reap_does_not_touch_other_projects_holder() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let terminal = Fixture::new(now, RunStatus::Stopped, persist_old());
        let live = Fixture::new(now, RunStatus::Running, persist_old());
        let terminal_holder = terminal.holder(HolderBehaviour::Normal);
        terminal.write_persist(
            &terminal_holder,
            PersistSpec::matching(&terminal_holder),
            persist_old(),
        );
        let mut live_holder = live.holder(HolderBehaviour::Normal);
        live.write_persist(
            &live_holder,
            PersistSpec::matching(&live_holder),
            persist_old(),
        );

        assert_reaped(&tick_project(&terminal.record, now), &terminal_holder);
        terminal_holder.finish();
        assert_skipped(tick_project(&live.record, now), ReapSkip::Active);
        assert!(live_holder.seen_ops().is_empty());
        live_holder.stop();
    }

    #[cfg(windows)]
    #[test]
    fn reap_scripted_swap_releases_pinned_old_binary() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let swap_dir = TempDir::new().unwrap();
        let old = swap_dir.path().join("coordinator.exe.old");
        let installed = swap_dir.path().join("coordinator.exe");
        let fresh = swap_dir.path().join("coordinator.new");
        std::fs::write(&old, b"old-binary").unwrap();
        std::fs::write(&installed, b"installed-binary").unwrap();
        std::fs::write(&fresh, b"new-binary").unwrap();

        let holder = fixture.holder_with(
            HolderBehaviour::Normal,
            fixture.record.path.clone(),
            Some(7000),
            "claude",
            Some(old.clone()),
        );
        fixture.write_persist(&holder, PersistSpec::matching(&holder), persist_old());
        assert!(
            std::fs::remove_file(&old).is_err(),
            "share_mode(0) must pin the old binary while the holder is alive"
        );

        assert_reaped(&tick_project(&fixture.record, now), &holder);
        holder.finish();
        std::fs::remove_file(&old).expect("holder released the old binary pin");
        std::fs::rename(&installed, &old).expect("rotate installed binary");
        std::fs::copy(&fresh, &installed).expect("install new binary");
        assert_eq!(std::fs::read(&installed).unwrap(), b"new-binary");
    }

    #[test]
    fn reap_writes_progress_and_no_run_state_mutation() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Idle, persist_old());
        let holder = fixture.holder(HolderBehaviour::Normal);
        fixture.write_persist(&holder, PersistSpec::matching(&holder), persist_old());
        let before = fixture.run_state_bytes();

        assert_reaped(&tick_project(&fixture.record, now), &holder);
        holder.finish();
        let after = fixture.run_state_bytes();
        assert_eq!(before, after, "reaper must not write run-state.json");
        assert!(fixture.status_log().contains("holder-reap"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)]
    async fn serve_poll_loop_reaps_terminal_holder() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::Normal);
        fixture.write_persist(&holder, PersistSpec::matching(&holder), persist_old());
        fixture.save_registry();

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let loop_task = tokio::spawn(crate::watch::serve_poll_loop(shutdown_rx));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !holder.saw("shutdown") {
            if Instant::now() >= deadline {
                panic!("serve poll loop did not reap the terminal holder");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = shutdown_tx.send(true);
        tokio::time::timeout(Duration::from_secs(2), loop_task)
            .await
            .expect("serve poll loop must exit")
            .expect("serve poll loop task");
        holder.stop();
    }

    // ---- Phase 1 seam ------------------------------------------------------

    #[test]
    fn persist_identity_reads_holder_fields() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::Normal);
        let mut spec = PersistSpec::matching(&holder);
        spec.cwd = Some(fixture.record.path.clone());
        fixture.write_persist(&holder, spec, persist_old());

        let (identity, _mtime) = pool::persist_identity(&fixture.record)
            .unwrap()
            .expect("persist present");
        assert_eq!(identity.project_id, fixture.record.id);
        assert_eq!(
            identity.session_id.as_deref(),
            Some(holder.session_id.as_str())
        );
        assert_eq!(identity.cwd.as_deref(), Some(fixture.record.path.as_path()));
        assert_eq!(identity.pid, Some(7000));
        assert_eq!(identity.holder_pid, Some(test_holder_pid()));
        assert_eq!(identity.control_addr.as_deref(), Some(holder.addr.as_str()));
        assert_eq!(identity.adapter, "claude");
        assert!(identity.alive);
        assert!(!identity.prompt_in_flight);

        fixture.remove_persist();
        assert!(pool::persist_identity(&fixture.record).unwrap().is_none());
        fixture.write_raw_persist("{not json", persist_old());
        assert!(pool::persist_identity(&fixture.record).is_err());
        holder.stop();
    }

    #[test]
    fn persist_identity_does_not_mutate_the_file() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::Normal);
        fixture.write_persist(&holder, PersistSpec::matching(&holder), persist_old());
        let before = std::fs::read(&fixture.persist_path).unwrap();

        let _ = pool::persist_identity(&fixture.record).unwrap().unwrap();
        let after = std::fs::read(&fixture.persist_path).unwrap();
        assert_eq!(before, after);
        holder.stop();
    }

    #[test]
    fn verify_holder_identity_requires_identity() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::MismatchSession);
        fixture.write_persist(&holder, PersistSpec::matching(&holder), persist_old());

        let (identity, _) = pool::persist_identity(&fixture.record).unwrap().unwrap();
        match pool::verify_holder_identity(&identity, RPC_TIMEOUT) {
            HolderReapRpc::IdentityUnproven(_) => {}
            other => panic!("expected IdentityUnproven, got {other:?}"),
        }
        assert_eq!(holder.seen_ops(), vec!["ping"]);
        holder.stop();
    }

    #[test]
    fn verify_holder_identity_accepts_matching_identity() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::Normal);
        fixture.write_persist(&holder, PersistSpec::matching(&holder), persist_old());

        let (identity, _) = pool::persist_identity(&fixture.record).unwrap().unwrap();
        assert_eq!(
            pool::verify_holder_identity(&identity, RPC_TIMEOUT),
            HolderReapRpc::Verified
        );
        assert_eq!(holder.seen_ops(), vec!["ping"]);
        holder.stop();
    }

    #[test]
    fn request_holder_shutdown_times_out() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::ShutdownNoResponse);
        fixture.write_persist(&holder, PersistSpec::matching(&holder), persist_old());

        let (identity, _) = pool::persist_identity(&fixture.record).unwrap().unwrap();
        assert_eq!(
            pool::verify_holder_identity(&identity, RPC_TIMEOUT),
            HolderReapRpc::Verified
        );
        match pool::request_holder_shutdown(&identity, RPC_TIMEOUT) {
            HolderReapRpc::ShutdownUnproven(_) => {}
            other => panic!("expected ShutdownUnproven, got {other:?}"),
        }
        assert!(!holder.is_finished());
        holder.stop();
    }

    #[test]
    fn reap_keeps_when_persist_changes_after_ping() {
        let _guard = test_env_lock();
        let now = Utc::now();
        let fixture = Fixture::new(now, RunStatus::Stopped, persist_old());
        let mut holder = fixture.holder(HolderBehaviour::MutatePersistOnPing);
        fixture.write_persist(&holder, PersistSpec::matching(&holder), persist_old());

        assert_skipped(tick_project(&fixture.record, now), ReapSkip::PromptInFlight);
        assert_eq!(holder.seen_ops(), vec!["ping"]);
        holder.stop();
    }
}
