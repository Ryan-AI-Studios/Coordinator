//! Session pool keyed by `project_id` plus CLI holder + persist metadata.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::error::{CoordinatorError, Result};
use crate::harness::capabilities::ContextUsage;
use crate::harness::grok::{
    ENV_ACP_HARNESS, ENV_CURSOR_BIN, ENV_GROK_BIN, GrokSession, PromptResult,
    failure_class_for_message, resolve_cursor_binary,
};
use crate::harness::terminal::SpawnTally;
use crate::harness::{grok_cwd, resolve_grok_binary};

/// Child-only model pin for the detached holder (never `set_var` on the parent).
pub(crate) const ENV_GROK_MODEL: &str = "COORDINATOR_GROK_MODEL";
use crate::harness::no_change::{self, CaptureStatus, VerdictOutcome};
use crate::outcome::{
    ApplyIdentity, FailureClass, LAST_EVENT_MESSAGE_CAP, OutcomeSource, PhaseOutcome,
    write_and_apply, write_and_apply_prepared,
};
use crate::persist::atomic_write_json;
use crate::registry::ProjectRecord;
use crate::state::{RunStatus, StatusView, ensure_state_dir, load_run_state, resolve_state_dir};
use crate::workflow::execution_repo::{PreparedInjection, ResolvedExecution};

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
            adapter: s.adapter.clone(),
            context_usage: s.context_usage.clone(),
            last_signal: s.last_signal.clone(),
            last_tool_title: s.last_tool_title.clone(),
            model: s.model.clone(),
            bin: Some(s.bin.clone()),
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
    #[serde(default = "default_adapter")]
    pub adapter: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_usage: Option<ContextUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_signal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tool_title: Option<String>,
    /// Live session model (0110 identity). Missing on a pre-upgrade persist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Live session binary (0110 identity). Missing on a pre-upgrade persist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin: Option<PathBuf>,
}

impl GrokHarnessStatus {
    pub fn missing() -> Self {
        Self {
            alive: false,
            session_id: None,
            cwd: None,
            supports_compact: false,
            pid: None,
            adapter: default_adapter(),
            context_usage: None,
            last_signal: None,
            last_tool_title: None,
            model: None,
            bin: None,
        }
    }
}

fn default_adapter() -> String {
    "grok".into()
}

/// Live-session identity (0110, OD-110-7).
///
/// The binding defines what the NEXT start must use; a live holder carries the identity it
/// was started with. `adapter` alone was the old predicate, which made a same-harness
/// model/command change a silent no-op.
pub(crate) fn identity_matches_parts(
    live: &GrokHarnessStatus,
    harness: &str,
    model: Option<&str>,
    resolved_bin: Option<&Path>,
) -> bool {
    let adapter = if live.adapter.trim().is_empty() {
        "grok"
    } else {
        live.adapter.as_str()
    };
    if !adapter.eq_ignore_ascii_case(harness.trim()) {
        return false;
    }
    let live_model = live
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    let want_model = model.map(str::trim).filter(|m| !m.is_empty());
    if live_model != want_model {
        return false;
    }
    if let Some(bin) = resolved_bin {
        // A pre-upgrade persist has no `bin`: it mismatches a known binding binary and
        // forces one honest recycle.
        let Some(live_bin) = live.bin.as_ref() else {
            return false;
        };
        if !crate::worktree::paths_same(live_bin, bin) {
            return false;
        }
    }
    true
}

pub(crate) fn identity_matches(
    live: &GrokHarnessStatus,
    binding: &crate::config::RoleBinding,
    resolved_bin: Option<&Path>,
) -> bool {
    identity_matches_parts(
        live,
        &binding.harness,
        binding.model.as_deref(),
        resolved_bin,
    )
}

fn default_supports_compact() -> bool {
    true
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
    #[serde(default = "default_adapter")]
    adapter: String,
    #[serde(default = "default_supports_compact")]
    supports_compact: bool,
    alive: bool,
    /// Mid-`session/prompt` (0027). Missing key = false. No persist version bump.
    #[serde(default)]
    prompt_in_flight: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context_usage: Option<ContextUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_signal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_tool_title: Option<String>,
    /// 0110: live session model. No persist version bump (same pattern as
    /// `prompt_in_flight`); missing key = unknown identity, which recycles once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// 0110: the resolved binary this session was spawned from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bin: Option<PathBuf>,
}

impl PersistedGrokHandle {
    fn to_status(&self) -> GrokHarnessStatus {
        GrokHarnessStatus {
            alive: self.alive,
            session_id: self.session_id.clone(),
            cwd: self.cwd.clone(),
            supports_compact: self.supports_compact,
            pid: self.pid,
            adapter: self.adapter.clone(),
            context_usage: self.context_usage.clone(),
            last_signal: self.last_signal.clone(),
            last_tool_title: self.last_tool_title.clone(),
            model: self.model.clone(),
            bin: self.bin.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum HoldRequest {
    Ping,
    Prompt {
        text: String,
    },
    /// Adapter-only prepared turn: frozen target identity plus prompt.
    PreparedPrompt {
        text: String,
        prepared: Box<PreparedInjection>,
    },
    Compact,
    Status,
    Shutdown,
    Cancel,
    AdoptEpoch {
        epoch: u64,
    },
    ReleaseEpoch,
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
    /// Prepared-prompt protocol the holder understands. Absent on old holders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prepared_protocol: Option<u32>,
    /// Epoch adoption the holder understands. Absent or 0 refuses a second run.
    /// This is not [`PREPARED_PROTOCOL_VERSION`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    epoch_protocol: Option<u32>,
}

/// Current prepared-prompt capability advertised in `Ping`.
pub const PREPARED_PROTOCOL_VERSION: u32 = 1;

/// Epoch adoption advertised in `Ping`. Not a bump of [`PREPARED_PROTOCOL_VERSION`].
pub const EPOCH_PROTOCOL_VERSION: u32 = 1;

fn prepared_protocol_supported(protocol: Option<u32>) -> bool {
    protocol.unwrap_or(0) >= PREPARED_PROTOCOL_VERSION
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreparedCapabilityPlan {
    Send,
    RecycleOnce,
    Fail,
}

/// One compatibility recycle per dispatch. A second incompatible probe fails.
fn prepared_capability_plan(
    protocol: Option<u32>,
    already_recycled: bool,
) -> PreparedCapabilityPlan {
    if prepared_protocol_supported(protocol) {
        PreparedCapabilityPlan::Send
    } else if already_recycled {
        PreparedCapabilityPlan::Fail
    } else {
        PreparedCapabilityPlan::RecycleOnce
    }
}

/// Verdict-time identity validation: re-resolve only to prove the frozen target is
/// still the same. A changed or unresolved target keeps the baseline, skips the
/// verdict and is journaled by the caller.
fn revalidate_target(
    record: &ProjectRecord,
    state: &crate::state::RunState,
    phase: &str,
    target: &ResolvedExecution,
) -> std::result::Result<(), String> {
    match crate::workflow::execution_repo::resolve_track_target(record, state, phase) {
        Ok(now) if now.same_identity(target) => Ok(()),
        Ok(_) => Err("target-changed".into()),
        Err(_) => Err("unresolved".into()),
    }
}

pub fn persist_path(record: &ProjectRecord) -> Result<PathBuf> {
    persist_path_for(record, crate::state::active_run_epoch())
}

/// Legacy `{state_dir}/harness-grok.json`, or `sessions/{epoch}/harness-grok.json`
/// while a book is open. An open book and `epoch == None` is `ambiguous run`.
pub fn persist_path_for(record: &ProjectRecord, epoch: Option<u64>) -> Result<PathBuf> {
    let dir = resolve_state_dir(record)?;
    match crate::state::load_run_book(record)? {
        crate::state::RunBook::Many(_) => {
            let Some(epoch) = epoch else {
                return Err(crate::state::ambiguous_run_error());
            };
            Ok(dir
                .join("sessions")
                .join(epoch.to_string())
                .join("harness-grok.json"))
        }
        crate::state::RunBook::One(_) => Ok(dir.join("harness-grok.json")),
    }
}

fn load_persist(record: &ProjectRecord) -> Result<Option<PersistedGrokHandle>> {
    load_persist_at(record, crate::state::active_run_epoch())
}

fn load_persist_at(
    record: &ProjectRecord,
    epoch: Option<u64>,
) -> Result<Option<PersistedGrokHandle>> {
    let path = persist_path_for(record, epoch)?;
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    Ok(Some(serde_json::from_str(&text)?))
}

/// Read-only projection of the durable holder identity used by the 0107 reaper.
///
/// Every identity field maps 1:1 by name to [`PersistedGrokHandle`]. `model`/`bin` are
/// the 0110 live identity (compared only when both sides carry them) and `error` is the
/// **non-identity** producer discriminator [`classify_persist`] needs to tell a failed
/// start from a pending one — it is never compared by [`prove_holder`]. The load-time
/// mtime travels in [`PersistRef`], not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PersistIdentity {
    pub project_id: String,
    pub session_id: Option<String>,
    pub cwd: Option<PathBuf>,
    pub pid: Option<u32>,
    pub holder_pid: Option<u32>,
    pub control_addr: Option<String>,
    pub adapter: String,
    pub alive: bool,
    pub prompt_in_flight: bool,
    pub model: Option<String>,
    pub bin: Option<PathBuf>,
    pub error: Option<String>,
}

impl PersistIdentity {
    fn from_handle(handle: &PersistedGrokHandle) -> Self {
        Self {
            project_id: handle.project_id.clone(),
            session_id: handle.session_id.clone(),
            cwd: handle.cwd.clone(),
            pid: handle.pid,
            holder_pid: handle.holder_pid,
            control_addr: handle.control_addr.clone(),
            adapter: handle.adapter.clone(),
            alive: handle.alive,
            prompt_in_flight: handle.prompt_in_flight,
            model: handle.model.clone(),
            bin: handle.bin.clone(),
            error: handle.error.clone(),
        }
    }

    /// The 0107/0110 identity fields only — `error` and `prompt_in_flight` are state,
    /// not identity, so a flag write between Ping and delivery is not "persist-changed".
    /// `model`/`bin` are compared only when both sides carry them (a pre-upgrade
    /// persist without them still proves identity).
    fn same_identity(&self, other: &Self) -> bool {
        let modelled = match (self.model.as_deref(), other.model.as_deref()) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        };
        let binned = match (self.bin.as_deref(), other.bin.as_deref()) {
            (Some(a), Some(b)) => crate::worktree::paths_same(a, b),
            _ => true,
        };
        modelled
            && binned
            && self.project_id == other.project_id
            && self.session_id == other.session_id
            && self.cwd == other.cwd
            && self.pid == other.pid
            && self.holder_pid == other.holder_pid
            && self.control_addr == other.control_addr
            && self.adapter == other.adapter
            && self.alive == other.alive
    }

    /// An attempt record (pre-spawn pending or failed start): no session, no holder,
    /// no address. `(None, None)` is not a unique key, so attempts are stamped instead.
    fn is_attempt(&self) -> bool {
        !self.alive
            && self.session_id.is_none()
            && self.holder_pid.is_none()
            && self.control_addr.is_none()
    }
}

/// Post-delivery wait for the old holder's own exit record.
pub(crate) const WAIT_GONE: Duration = Duration::from_secs(2);
/// Grace for the product's own `DeadAfterExit`/`FailedStart` record before a discard.
pub(crate) const DISCARD_GRACE: Duration = Duration::from_secs(10);
/// The pre-spawn pending record is never discarded inside this window (45 s budget + margin).
pub(crate) const PENDING_START_GRACE: Duration = Duration::from_secs(60);
/// Bounded wait for the in-process pool mutex before an owned-session teardown.
pub(crate) const POOL_LOCK_BUDGET: Duration = Duration::from_secs(5);
/// Child-only marker carrying the parent start claim's mtime (nanos since UNIX_EPOCH).
pub(crate) const ENV_HOLDER_ATTEMPT_MTIME: &str = "COORDINATOR_HOLDER_ATTEMPT_MTIME";

// ---- 0121: the holder-persist atomic fence ---------------------------------

/// Zero-byte sentinel for the exclusive holder-persist mutation lock.
///
/// The JSON file itself is never locked: Windows `LockFileEx` locks are enforced
/// against other handles' I/O, so locking `harness-grok.json` would make every
/// lock-free reader (`status`, `checkpoint`, `ui`) fail or block. Readers stay
/// lock-free because the write path replaces the file atomically.
pub(crate) const PERSIST_LOCK_FILE: &str = ".harness-grok.lock";
/// One critical section is a small read plus one atomic replace or unlink.
pub(crate) const PERSIST_LOCK_BUDGET: Duration = Duration::from_secs(2);
/// Retry cadence while the sentinel is held by a live writer.
const PERSIST_LOCK_RETRY: Duration = Duration::from_millis(2);
/// Coherent-snapshot attempts (2 retries) before the read refuses.
const PERSIST_SNAPSHOT_TRIES: u32 = 3;
/// Backoff between snapshot attempts: lets a retry outlast one fenced section.
const PERSIST_SNAPSHOT_BACKOFF: Duration = Duration::from_millis(2);

/// Three-way internal outcome of a compare-and-act writer (OD-121-10).
///
/// `Refused` is the existing `holder-changed` verdict: the record is not the one the
/// caller loaded. `Unavailable` is a fence failure (`persist-lock-timeout`,
/// `persist-lock-io`, `persist-lock-order` or an unreadable record) and must **never**
/// be re-labelled as a record change.
#[derive(Debug)]
pub(crate) enum PersistAct {
    Ok,
    Refused,
    Unavailable(CoordinatorError),
}

/// Map a compare-and-act outcome to the caller's `Result` shape: `Refused` becomes the
/// caller's existing `holder-changed` message, `Unavailable` propagates the named token.
pub(crate) fn persist_act_or_refused(
    act: PersistAct,
    refused: impl FnOnce() -> CoordinatorError,
) -> Result<()> {
    match act {
        PersistAct::Ok => Ok(()),
        PersistAct::Refused => Err(refused()),
        PersistAct::Unavailable(e) => Err(e),
    }
}

/// RAII guard proving the holder-persist fence is held. The OS releases the advisory
/// lock when the handle closes (drop) or the process dies, so a crashed writer never
/// leaves a held lock behind — only a harmless zero-byte sentinel.
///
/// Deliberately **`!Send`**: its [`crate::state::PersistLockScope`] decrements the
/// *acquiring* thread's depth, so moving the guard to another thread would leak the
/// depth and make every later `with_run_state_lock` on the origin thread fail closed.
pub(crate) struct PersistLock {
    /// Holds the OS lock; released on drop or process death.
    file: std::fs::File,
    /// `{state_dir}`.
    dir: PathBuf,
    /// `{state_dir}/harness-grok.json`.
    data: PathBuf,
    _order: crate::state::PersistLockScope,
    _not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}

fn persist_lock_path(record: &ProjectRecord) -> Result<PathBuf> {
    Ok(resolve_state_dir(record)?.join(PERSIST_LOCK_FILE))
}

/// Map every dir/open failure to the named token; the intended sentinel path is explicit.
fn lock_io(e: impl std::fmt::Display, sentinel: &Path) -> CoordinatorError {
    CoordinatorError::Message(format!(
        "persist-lock-io: {e} (lock: {})",
        sentinel.display()
    ))
}

#[cfg(test)]
thread_local! {
    /// Test-only lock-budget override so the bounded refusal is provable without 2 s.
    static TEST_PERSIST_LOCK_BUDGET: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

/// RAII test override for [`persist_lock_budget`]. Saves/restores the previous value so
/// nested overrides are legal; `#[must_use]` because dropping it immediately would make
/// the test silently use the production budget.
#[cfg(test)]
#[must_use = "the override resets when the guard drops"]
pub(crate) struct PersistLockBudgetGuard(Option<Duration>);

#[cfg(test)]
pub(crate) fn test_persist_lock_budget(duration: Duration) -> PersistLockBudgetGuard {
    let previous = TEST_PERSIST_LOCK_BUDGET.with(|cell| cell.replace(Some(duration)));
    PersistLockBudgetGuard(previous)
}

#[cfg(test)]
impl Drop for PersistLockBudgetGuard {
    fn drop(&mut self) {
        TEST_PERSIST_LOCK_BUDGET.with(|cell| cell.set(self.0));
    }
}

fn persist_lock_budget() -> Duration {
    #[cfg(test)]
    if let Some(budget) = TEST_PERSIST_LOCK_BUDGET.with(std::cell::Cell::get) {
        return budget;
    }
    PERSIST_LOCK_BUDGET
}

impl PersistLock {
    /// `sentinel` is locked; `data` is the record the holder will be allowed to mutate.
    ///
    /// The recursion check runs **before** `try_lock`: a nested acquisition on the same
    /// thread must fail fast instead of spending the whole budget and reporting a
    /// misleading timeout.
    pub(crate) fn acquire(sentinel: &Path, data: impl FnOnce() -> Result<PathBuf>) -> Result<Self> {
        if crate::state::persist_lock_held() {
            return Err(CoordinatorError::Message(format!(
                "persist-lock-order: recursive persist lock (lock: {})",
                sentinel.display()
            )));
        }
        // Read+write, never append-only: std documents that a lock fails on an
        // append-only handle. `create` keeps the sentinel a zero-byte file.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(sentinel)
            .map_err(|e| lock_io(e, sentinel))?;
        let started = std::time::Instant::now();
        let budget = persist_lock_budget();
        let mut resolve = Some(data);
        loop {
            match file.try_lock() {
                Ok(()) => {
                    let order = crate::state::persist_lock_scope()?;
                    let data = resolve.take().expect("persist path resolved once")()?;
                    let dir = data
                        .parent()
                        .filter(|p| !p.as_os_str().is_empty())
                        .or_else(|| sentinel.parent())
                        .unwrap_or_else(|| Path::new("."))
                        .to_path_buf();
                    return Ok(Self {
                        file,
                        dir,
                        data,
                        _order: order,
                        _not_send: std::marker::PhantomData,
                    });
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    let elapsed = started.elapsed();
                    if elapsed >= budget {
                        return Err(CoordinatorError::Message(format!(
                            "persist-lock-timeout: the holder persist lock was held for {}ms \
                             (lock: {})",
                            elapsed.as_millis(),
                            sentinel.display()
                        )));
                    }
                    std::thread::sleep(PERSIST_LOCK_RETRY);
                }
                // A non-contention fault (permissions, unsupported filesystem) is named,
                // never retried as contention and never re-labelled as a record change.
                Err(std::fs::TryLockError::Error(e)) => return Err(lock_io(e, sentinel)),
            }
        }
    }
}

impl Drop for PersistLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
        // `_order` drops next and decrements this thread's depth.
    }
}

/// Take the fence for one `read -> compare -> act`. `lock_persist` stays private: this is
/// the only acquisition entry point, so a writer cannot open a bare handle.
fn lock_persist(record: &ProjectRecord) -> Result<PersistLock> {
    let sentinel =
        persist_lock_path(record).map_err(|e| lock_io(e, Path::new(PERSIST_LOCK_FILE)))?;
    ensure_state_dir(record).map_err(|e| lock_io(e, &sentinel))?;
    PersistLock::acquire(&sentinel, || persist_path(record))
}

/// The one entry point every persist writer uses. The closure is synchronous, so the
/// lock cannot be held across an `.await` and no async lock can be taken inside it.
pub(crate) fn with_persist_lock<T>(
    record: &ProjectRecord,
    f: impl FnOnce(&PersistLock) -> Result<T>,
) -> Result<T> {
    let lock = lock_persist(record)?;
    f(&lock) // the lock drops after f: the act is inside the fence
}

/// W1's claim primitive: serialise first, then create exclusively (`create_new`).
fn create_persist_claim_locked(lock: &PersistLock, handle: &PersistedGrokHandle) -> Result<()> {
    use std::io::Write;
    let bytes = serde_json::to_vec(handle)?;
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock.data)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(CoordinatorError::Message(format!(
                "persist-appeared: another start holds the claim (persist: {})",
                lock.data.display()
            )));
        }
        Err(e) => return Err(e.into()),
    };
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

fn save_persist_locked(lock: &PersistLock, handle: &PersistedGrokHandle) -> Result<()> {
    std::fs::create_dir_all(&lock.dir)?;
    atomic_write_json(&lock.data, handle)
}

fn remove_persist_locked(lock: &PersistLock) -> Result<()> {
    match std::fs::remove_file(&lock.data) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()), // already settled
        Err(e) => Err(e.into()),
    }
}

/// Authorization token. Private fields; **only** [`prove_holder`] builds one, so no
/// `Shutdown` can be sent to an address that has not answered a matching `Ping`.
#[derive(Debug)]
pub(crate) struct VerifiedHolder {
    record: ProjectRecord,
    /// The identity the Ping answered for.
    want: PersistIdentity,
    /// `want.control_addr`, non-empty.
    addr: String,
    /// The persist mtime observed *before* the Ping.
    mtime: SystemTime,
    /// The Ping's own answer (session/cwd/pid/adapter/model/bin).
    harness: GrokHarnessStatus,
}

/// Why a caller terminated a holder. The only knob the delivery seam has: the
/// automatic sweep must never interrupt a turn; an explicit teardown may.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Intent {
    Sweep,
    Reap,
    Operator,
}

/// Producer-discriminated, not clock-discriminated no-address shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoSocketWhy {
    /// `alive:false`, `session_id:Some` — a clean exit (or a legacy record).
    DeadAfterExit,
    /// `alive:false`, `session_id:None`, `error:Some`.
    FailedStart,
    /// `alive:false`, `session_id:None`, `error:None` — the pre-spawn record.
    PendingStart,
    /// `alive:true`, `holder_pid == this process` (caller-relative).
    AliveSelfNoAddress,
    /// `alive:true`, another pid.
    AliveForeignNoAddress,
    /// `alive:true`, `holder_pid` `None`/0.
    AliveUnknownNoAddress,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HolderTermination {
    /// The holder answered `ok` to its own `Shutdown`. A delivery receipt, never exit.
    Delivered {
        holder_pid: Option<u32>,
        session_id: Option<String>,
    },
    /// Its own dead, address-less record was re-read still `same_record`.
    Exited {
        holder_pid: Option<u32>,
        session_id: Option<String>,
    },
    /// The address accepted no connection: no listener, not "it exited".
    NoListener { addr: String },
    /// There is no address to try.
    NoSocket { why: NoSocketWhy },
    /// An address existed; proof or delivery failed.
    Unproven { why: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GoneHow {
    Absent,
    Exited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitGone {
    Gone {
        how: GoneHow,
    },
    /// The persist now names a different record: write nothing.
    Superseded,
    /// Not success.
    TimedOut,
}

/// The two load outcomes are **not** identity classes: `load_persist` returns
/// `Ok(None)` for absent and `Err` for unreadable, and every caller refuses on `Err`
/// before classifying. Keeping them out of this enum is what makes "unreadable is not
/// absent" a type-level rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PersistClass {
    NoAddress(NoSocketWhy),
    Addressed { addr: String },
}

/// The record a caller loaded — identity **plus** the mtime it was read at. The mtime
/// is what makes one attempt record distinguishable from a different attempt that has
/// the same empty identity.
#[derive(Debug, Clone)]
pub(crate) struct PersistRef {
    pub identity: PersistIdentity,
    pub mtime: SystemTime,
}

impl PersistRef {
    pub(crate) fn new(identity: PersistIdentity, mtime: SystemTime) -> Self {
        Self { identity, mtime }
    }
}

/// The one discard rule for every caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoAddressAction {
    DiscardNow,
    WaitUntil(SystemTime),
    Refuse(&'static str),
    /// `AliveSelf`: the caller tears down the session it owns first.
    OwnedInProcess,
}

/// `old` is the record the caller must hand to the wait/compare helpers (identity plus
/// the load's mtime — never a post-Ping re-read, so an attempt stamp survives); it is
/// `None` only when the persist was already absent at load.
#[derive(Debug, Clone)]
pub(crate) struct Terminated {
    pub old: Option<PersistRef>,
    pub outcome: HolderTermination,
}

#[cfg(test)]
type AfterGoneHook = Arc<dyn Fn(&ProjectRecord) + Send + Sync>;
#[cfg(test)]
type PreClaimHook = Arc<dyn Fn(&ProjectRecord) + Send + Sync>;
#[cfg(test)]
type SpawnHook = Arc<dyn Fn(&ProjectRecord) -> Result<GrokHarnessStatus> + Send + Sync>;

/// Test-only seam invoked between a `Gone` verdict (or an `AliveSelf` load) and the
/// compare-and-act, so the `false`-is-a-refusal contract is deterministic.
#[cfg(test)]
pub(crate) static AFTER_GONE_HOOK: std::sync::Mutex<Option<AfterGoneHook>> =
    std::sync::Mutex::new(None);
/// Test-only seam invoked immediately before `claim_start` in both spawn paths.
#[cfg(test)]
pub(crate) static PRE_CLAIM_HOOK: std::sync::Mutex<Option<PreClaimHook>> =
    std::sync::Mutex::new(None);
/// Test-only spawn stand-in, invoked immediately after a successful `claim_start`
/// **instead of** the real spawn/readiness phase.
#[cfg(test)]
pub(crate) static SPAWN_HOOK: std::sync::Mutex<Option<SpawnHook>> = std::sync::Mutex::new(None);

/// 0121 test-only seam: pauses a writer *inside* the fenced section, after a successful
/// compare (W2-W6) or after the load (W8/W9), and before the mutation. Armed only on the
/// calling thread, so a retry thread never blocks on the test's gate. The `Arc` is cloned
/// out and the mutex guard dropped before the closure runs: a blocked hook cannot
/// deadlock another writer.
#[cfg(test)]
type PersistPauseHook = Arc<dyn Fn(&str) + Send + Sync>;
#[cfg(test)]
pub(crate) static PERSIST_PAUSE_HOOK: std::sync::Mutex<Option<PersistPauseHook>> =
    std::sync::Mutex::new(None);
#[cfg(test)]
thread_local! {
    static PERSIST_PAUSE_ARMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Arm the one-shot pause on this thread.
#[cfg(test)]
pub(crate) fn arm_persist_pause() {
    PERSIST_PAUSE_ARMED.with(|cell| cell.set(true));
}

/// Drop the test hooks and any thread-local arming (test cleanup).
#[cfg(test)]
pub(crate) fn clear_persist_hooks() {
    *PERSIST_PAUSE_HOOK.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *PERSIST_SNAPSHOT_HOOK
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    PERSIST_PAUSE_ARMED.with(|cell| cell.set(false));
    PERSIST_SNAPSHOT_ARMED.with(|cell| cell.set(false));
}

fn persist_pause_before_write(label: &str) {
    #[cfg(test)]
    {
        if !PERSIST_PAUSE_ARMED.with(std::cell::Cell::get) {
            return;
        }
        PERSIST_PAUSE_ARMED.with(|cell| cell.set(false));
        let hook = PERSIST_PAUSE_HOOK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(hook) = hook {
            hook(label);
        }
    }
    #[cfg(not(test))]
    let _ = label;
}

/// 0121 test-only seam between the first `mtime` and the content read of
/// [`persist_snapshot`]. One-shot (the armed flag clears before the hook runs) so a
/// retry cannot re-mutate the file.
#[cfg(test)]
type PersistSnapshotHook = Arc<dyn Fn() + Send + Sync>;
#[cfg(test)]
pub(crate) static PERSIST_SNAPSHOT_HOOK: std::sync::Mutex<Option<PersistSnapshotHook>> =
    std::sync::Mutex::new(None);
#[cfg(test)]
thread_local! {
    static PERSIST_SNAPSHOT_ARMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Thread-local so parallel tests cannot race the attempt count (T11).
    static PERSIST_SNAPSHOT_ATTEMPTS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn arm_persist_snapshot_hook(f: impl Fn() + Send + Sync + 'static) {
    *PERSIST_SNAPSHOT_HOOK
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(f));
    PERSIST_SNAPSHOT_ARMED.with(|cell| cell.set(true));
}

#[cfg(test)]
pub(crate) fn reset_persist_snapshot_attempts() {
    PERSIST_SNAPSHOT_ATTEMPTS.with(|cell| cell.set(0));
}

#[cfg(test)]
pub(crate) fn persist_snapshot_attempts() -> u32 {
    PERSIST_SNAPSHOT_ATTEMPTS.with(std::cell::Cell::get)
}

fn persist_snapshot_hook() {
    #[cfg(test)]
    {
        if !PERSIST_SNAPSHOT_ARMED.with(std::cell::Cell::get) {
            return;
        }
        PERSIST_SNAPSHOT_ARMED.with(|cell| cell.set(false));
        let hook = PERSIST_SNAPSHOT_HOOK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(hook) = hook {
            hook();
        }
    }
}

fn persist_snapshot_attempt() {
    #[cfg(test)]
    PERSIST_SNAPSHOT_ATTEMPTS.with(|cell| cell.set(cell.get() + 1));
}

pub(crate) fn after_gone(record: &ProjectRecord) {
    #[cfg(test)]
    {
        let hook = AFTER_GONE_HOOK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(hook) = hook {
            hook(record);
        }
    }
    #[cfg(not(test))]
    let _ = record;
}

/// Whether the in-process pool mutex can be taken right now (no guard retained).
pub(crate) fn pool_lock_available() -> bool {
    match POOL.get() {
        Some(pool) => pool.try_lock().is_ok(),
        None => true,
    }
}

fn pre_claim(record: &ProjectRecord) {
    #[cfg(test)]
    {
        let hook = PRE_CLAIM_HOOK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(hook) = hook {
            hook(record);
        }
    }
    #[cfg(not(test))]
    let _ = record;
}

/// `Some(result)` means the test stand-in replaced the real spawn for this call.
fn spawn_stand_in(record: &ProjectRecord) -> Option<Result<GrokHarnessStatus>> {
    #[cfg(test)]
    {
        let hook = SPAWN_HOOK.lock().unwrap_or_else(|e| e.into_inner()).clone();
        hook.map(|hook| hook(record))
    }
    #[cfg(not(test))]
    {
        let _ = record;
        None
    }
}

/// The §7.4 three-step recovery, in the imperative order.
pub(crate) fn recovery_hint(record: &ProjectRecord) -> String {
    let path = persist_path(record)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "{state_dir}/harness-grok.json".into());
    format!(
        "end any live `coordinator harness <adapter> hold` process for this project first, \
         then delete {path}, then retry"
    )
}

fn age_since(mtime: SystemTime, now: SystemTime) -> Duration {
    now.duration_since(mtime).unwrap_or(Duration::ZERO)
}

/// One classifier, used by `decide` (file-only) and `prove_holder` (needs a Ping), so
/// `status`, the sweep and the start path cannot disagree on the same file.
pub(crate) fn classify_persist(identity: &PersistIdentity) -> PersistClass {
    if let Some(addr) = identity
        .control_addr
        .as_deref()
        .filter(|addr| !addr.trim().is_empty())
    {
        return PersistClass::Addressed {
            addr: addr.to_string(),
        };
    }
    if identity.alive {
        return match identity.holder_pid {
            Some(pid) if pid == std::process::id() => {
                PersistClass::NoAddress(NoSocketWhy::AliveSelfNoAddress)
            }
            Some(pid) if pid != 0 => PersistClass::NoAddress(NoSocketWhy::AliveForeignNoAddress),
            _ => PersistClass::NoAddress(NoSocketWhy::AliveUnknownNoAddress),
        };
    }
    if identity.session_id.is_some() {
        return PersistClass::NoAddress(NoSocketWhy::DeadAfterExit);
    }
    if identity.error.is_some() {
        PersistClass::NoAddress(NoSocketWhy::FailedStart)
    } else {
        PersistClass::NoAddress(NoSocketWhy::PendingStart)
    }
}

/// One "is the file still the record I loaded?" predicate.
///
/// A record that **names a session or a holder** matches on `(project_id, session_id,
/// holder_pid)` and ignores mtime (the holder's own later writes keep that identity).
/// An **attempt** record (no session, no holder, no address) is stamped by
/// `(error presence, mtime)`. Absent, newer, older or a different error presence is a
/// different record → `false`. `(None, None)` alone is never compared.
pub(crate) fn same_record(old: &PersistRef, now: &PersistIdentity, now_mtime: SystemTime) -> bool {
    if old.identity.is_attempt() {
        return now.is_attempt()
            && now.project_id == old.identity.project_id
            && now.error.is_some() == old.identity.error.is_some()
            && now_mtime == old.mtime;
    }
    now.project_id == old.identity.project_id
        && now.session_id == old.identity.session_id
        && now.holder_pid == old.identity.holder_pid
}

/// The exclusive absent→pending claim: the only way to begin a start.
///
/// 0121: the whole create + read-back runs inside the persist fence, so a writer that
/// interleaves after the create cannot have its record destroyed by a stale compare.
pub(crate) fn claim_start(record: &ProjectRecord, harness: &str) -> Result<PersistRef> {
    let handle = PersistedGrokHandle {
        version: 1,
        model: None,
        bin: None,
        project_id: record.id.clone(),
        session_id: None,
        cwd: None,
        pid: None,
        holder_pid: None,
        control_addr: None,
        adapter: persist_adapter_name(harness),
        supports_compact: true,
        alive: false,
        prompt_in_flight: false,
        error: None,
        context_usage: None,
        last_signal: None,
        last_tool_title: None,
    };
    with_persist_lock(record, |lock| {
        create_persist_claim_locked(lock, &handle)?;
        let path = lock.data.clone();
        let Some((handle, mtime)) = persist_snapshot(record)? else {
            return Err(CoordinatorError::Message(format!(
                "persist-appeared: the claim vanished (persist: {})",
                path.display()
            )));
        };
        Ok(PersistRef::new(
            PersistIdentity::from_handle(&handle),
            mtime,
        ))
    })
}

/// Fenced attempt-error write. `Some(claim)` writes only while the file is still that
/// claim; `None` (an unclaimed direct `hold`) writes only when the file is absent —
/// never over an extant attempt or an identified record. `false` = nothing written.
pub(crate) fn mark_start_error(
    record: &ProjectRecord,
    claim: Option<&PersistRef>,
    err: &str,
) -> bool {
    matches!(mark_start_error_outcome(record, claim, err), PersistAct::Ok)
}

/// The fenced body behind [`mark_start_error`]; `Unavailable` keeps the named fence
/// failure visible to callers that need the distinction.
fn mark_start_error_outcome(
    record: &ProjectRecord,
    claim: Option<&PersistRef>,
    err: &str,
) -> PersistAct {
    let adapter = match claim {
        Some(claim) => claim.identity.adapter.clone(),
        None => persist_adapter_name(&holder_harness_slug()),
    };
    let handle = PersistedGrokHandle {
        version: 1,
        model: None,
        bin: None,
        project_id: record.id.clone(),
        session_id: None,
        cwd: None,
        pid: None,
        holder_pid: None,
        control_addr: None,
        adapter,
        supports_compact: true,
        alive: false,
        prompt_in_flight: false,
        error: Some(err.to_string()),
        context_usage: None,
        last_signal: None,
        last_tool_title: None,
    };
    match with_persist_lock(record, |lock| {
        let now = persist_snapshot(record)?;
        let accepted = match (claim, now) {
            (Some(claim), Some((handle, mtime))) => {
                same_record(claim, &PersistIdentity::from_handle(&handle), mtime)
            }
            (None, None) => true,
            _ => false,
        };
        if !accepted {
            return Ok(PersistAct::Refused);
        }
        persist_pause_before_write("error");
        save_persist_locked(lock, &handle)?;
        Ok(PersistAct::Ok)
    }) {
        Ok(act) => act,
        Err(e) => PersistAct::Unavailable(e),
    }
}

/// The pending identity shape both `claim_start` and the child's inherited stamp use.
fn attempt_identity(record: &ProjectRecord, harness: &str) -> PersistIdentity {
    PersistIdentity {
        project_id: record.id.clone(),
        session_id: None,
        cwd: None,
        pid: None,
        holder_pid: None,
        control_addr: None,
        adapter: persist_adapter_name(harness),
        alive: false,
        prompt_in_flight: false,
        model: None,
        bin: None,
        error: None,
    }
}

fn ready_handle(
    record: &ProjectRecord,
    session: &GrokSession,
    control_addr: Option<String>,
) -> PersistedGrokHandle {
    PersistedGrokHandle {
        version: 1,
        model: session.model.clone(),
        bin: Some(session.bin.clone()),
        project_id: record.id.clone(),
        session_id: Some(session.session_id.clone()),
        cwd: Some(session.cwd.clone()),
        pid: session.pid,
        holder_pid: Some(std::process::id()),
        control_addr,
        adapter: session.adapter.clone(),
        supports_compact: session.supports_compact,
        alive: true,
        prompt_in_flight: false,
        error: None,
        context_usage: session.context_usage.clone(),
        last_signal: session.last_signal.clone(),
        last_tool_title: session.last_tool_title.clone(),
    }
}

/// Fenced holder-ready write. `Some(stamp)` publishes only while the file is still the
/// parent's claim. `None` (a direct `hold`) publishes only over an absent record or a
/// `DeadAfterExit` no-address record. `false` = nothing written and the holder child
/// must tear its own ACP session down and exit rather than serve untracked.
/// Bool verdict kept by OD-121-6 for callers/readers that only need "written or not";
/// production verdict paths use the `_outcome` twin so contention is named. Exercised by
/// the fixture suite.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn write_holder_ready(
    record: &ProjectRecord,
    session: &GrokSession,
    control_addr: Option<String>,
    stamp: Option<&PersistRef>,
) -> bool {
    matches!(
        write_holder_ready_outcome(record, session, control_addr, stamp),
        PersistAct::Ok
    )
}

/// The fenced body behind [`write_holder_ready`]. `Refused` = the child must tear its own
/// session down; `Unavailable` = the fence failed and the caller retries, never a teardown.
fn write_holder_ready_outcome(
    record: &ProjectRecord,
    session: &GrokSession,
    control_addr: Option<String>,
    stamp: Option<&PersistRef>,
) -> PersistAct {
    let handle = ready_handle(record, session, control_addr);
    match with_persist_lock(record, |lock| {
        let now = persist_snapshot(record)?;
        let accepted = match (stamp, now) {
            (Some(stamp), Some((handle, mtime))) => {
                same_record(stamp, &PersistIdentity::from_handle(&handle), mtime)
            }
            (None, None) => true,
            (None, Some((handle, _))) => matches!(
                classify_persist(&PersistIdentity::from_handle(&handle)),
                PersistClass::NoAddress(NoSocketWhy::DeadAfterExit)
            ),
            (Some(_), None) => false,
        };
        if !accepted {
            return Ok(PersistAct::Refused);
        }
        persist_pause_before_write("ready");
        save_persist_locked(lock, &handle)?;
        Ok(PersistAct::Ok)
    }) {
        Ok(act) => act,
        Err(e) => PersistAct::Unavailable(e),
    }
}

/// Fenced holder-exit write: writes only while the persist still names this holder's
/// own `session_id` + `holder_pid`. A child whose ready write was refused has no
/// matching identity on disk, so its exit write is a no-op.
/// Bool verdict kept by OD-121-6 (see [`write_holder_ready`]).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn write_holder_exit(record: &ProjectRecord, session: &GrokSession) -> bool {
    matches!(write_holder_exit_outcome(record, session), PersistAct::Ok)
}

fn write_holder_exit_outcome(record: &ProjectRecord, session: &GrokSession) -> PersistAct {
    let mut own = ready_handle(record, session, None);
    own.alive = true;
    let own_identity = PersistIdentity::from_handle(&own);
    let dead = PersistedGrokHandle {
        version: 1,
        model: session.model.clone(),
        bin: Some(session.bin.clone()),
        project_id: record.id.clone(),
        session_id: Some(session.session_id.clone()),
        cwd: Some(session.cwd.clone()),
        pid: session.pid,
        holder_pid: Some(std::process::id()),
        control_addr: None,
        adapter: session.adapter.clone(),
        supports_compact: session.supports_compact,
        alive: false,
        prompt_in_flight: false,
        error: None,
        context_usage: session.context_usage.clone(),
        last_signal: session.last_signal.clone(),
        last_tool_title: session.last_tool_title.clone(),
    };
    match with_persist_lock(record, |lock| {
        let Some((handle, mtime)) = persist_snapshot(record)? else {
            return Ok(PersistAct::Refused);
        };
        let old = PersistRef::new(own_identity, mtime);
        if !same_record(&old, &PersistIdentity::from_handle(&handle), mtime) {
            return Ok(PersistAct::Refused);
        }
        persist_pause_before_write("exit");
        save_persist_locked(lock, &dead)?;
        Ok(PersistAct::Ok)
    }) {
        Ok(act) => act,
        Err(e) => PersistAct::Unavailable(e),
    }
}

/// The one discard rule. Only the clock is consulted here; the shape was produced by
/// [`classify_persist`].
pub(crate) fn no_address_decision(
    why: &NoSocketWhy,
    mtime: SystemTime,
    now: SystemTime,
) -> NoAddressAction {
    match why {
        NoSocketWhy::DeadAfterExit | NoSocketWhy::FailedStart => {
            let age = age_since(mtime, now);
            if age >= DISCARD_GRACE {
                NoAddressAction::DiscardNow
            } else {
                NoAddressAction::WaitUntil(mtime + (DISCARD_GRACE - age))
            }
        }
        NoSocketWhy::PendingStart => {
            let age = age_since(mtime, now);
            if age >= PENDING_START_GRACE {
                NoAddressAction::DiscardNow
            } else {
                NoAddressAction::Refuse("start-pending")
            }
        }
        NoSocketWhy::AliveSelfNoAddress => NoAddressAction::OwnedInProcess,
        NoSocketWhy::AliveForeignNoAddress => NoAddressAction::Refuse("foreign-holder"),
        NoSocketWhy::AliveUnknownNoAddress => NoAddressAction::Refuse("unknown-record"),
    }
}

/// The one coherent `(content, mtime)` snapshot every compare ref comes from.
///
/// Reads `mtime1 -> content -> mtime2` and accepts the pair only when both mtimes agree,
/// so a caller can never compare content of A against the mtime of B. Absence is decided
/// **only** from the `NotFound` of the first `metadata`: a `None` after a successful
/// metadata is a change, not absence, and is retried. A corrupt file still returns the
/// existing typed error; a sustained change is `persist-unstable` (a refusal, not a new
/// tolerance).
fn persist_snapshot(record: &ProjectRecord) -> Result<Option<(PersistedGrokHandle, SystemTime)>> {
    let path = persist_path(record)?;
    for attempt in 0..PERSIST_SNAPSHOT_TRIES {
        persist_snapshot_attempt();
        let first = match std::fs::metadata(&path) {
            Ok(meta) => meta.modified()?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        persist_snapshot_hook();
        let loaded = match load_persist(record) {
            Ok(loaded) => loaded,
            // Removed between `metadata` and the read: a change, not absence.
            Err(CoordinatorError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        if let Some(handle) = loaded {
            let second = match std::fs::metadata(&path) {
                Ok(meta) => meta.modified()?,
                // Removed after the content read: retry, never `Ok(None)`.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    std::thread::sleep(PERSIST_SNAPSHOT_BACKOFF);
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            if first == second {
                return Ok(Some((handle, second)));
            }
        }
        if attempt + 1 < PERSIST_SNAPSHOT_TRIES {
            std::thread::sleep(PERSIST_SNAPSHOT_BACKOFF);
        }
    }
    Err(CoordinatorError::Message(format!(
        "persist-unstable: the record changed while it was read (persist: {})",
        path.display()
    )))
}

/// Read-only view of the persist identity plus the file mtime.
///
/// `Ok(None)` means the persist file is absent; `Err` means it is unreadable or
/// corrupt. The reaper keeps the holder in both cases.
pub(crate) fn persist_identity(
    record: &ProjectRecord,
) -> Result<Option<(PersistIdentity, std::time::SystemTime)>> {
    Ok(persist_snapshot(record)?
        .map(|(handle, mtime)| (PersistIdentity::from_handle(&handle), mtime)))
}

/// Typed socket fault. "No connection" must stay a typed arm (`NoListener`), not text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RpcFault {
    ConnectRefused,
    Timeout,
    Closed,
    Invalid,
    Io,
}

#[derive(Debug)]
struct HolderRpcError {
    fault: RpcFault,
    /// Diagnostic only: reason tokens stay stable and never carry OS text.
    #[allow(dead_code)]
    detail: String,
}

impl HolderRpcError {
    fn reason(&self, op: &str) -> String {
        let suffix = match self.fault {
            RpcFault::ConnectRefused => "connect-refused",
            RpcFault::Timeout => "timeout",
            RpcFault::Closed => "closed",
            RpcFault::Invalid => "json",
            RpcFault::Io => "io",
        };
        format!("{op}-{suffix}")
    }
}

fn io_fault(e: &std::io::Error) -> RpcFault {
    match e.kind() {
        std::io::ErrorKind::ConnectionRefused => RpcFault::ConnectRefused,
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => RpcFault::Timeout,
        std::io::ErrorKind::BrokenPipe
        | std::io::ErrorKind::UnexpectedEof
        | std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::ConnectionAborted => RpcFault::Closed,
        _ => RpcFault::Io,
    }
}

/// Bounded synchronous holder RPC used by the seam.
///
/// The async [`holder_rpc`] has no timeout of its own; this runs on a blocking worker
/// and bounds every socket operation at `timeout`.
fn holder_rpc_sync(
    addr: &str,
    req: &HoldRequest,
    timeout: std::time::Duration,
) -> std::result::Result<HoldResponse, HolderRpcError> {
    use std::io::{BufRead, BufReader, Write};

    let closed = |detail: String| HolderRpcError {
        fault: RpcFault::Closed,
        detail,
    };
    let io = |e: &std::io::Error| HolderRpcError {
        fault: io_fault(e),
        detail: e.to_string(),
    };
    let sock: std::net::SocketAddr =
        addr.parse()
            .map_err(|e: std::net::AddrParseError| HolderRpcError {
                fault: RpcFault::Io,
                detail: format!("holder addr {addr}: {e}"),
            })?;
    let mut stream = std::net::TcpStream::connect_timeout(&sock, timeout).map_err(|e| {
        // A connect-phase timeout is "the address accepted no connection" on Windows:
        // a listening socket completes the handshake in the kernel even when the app
        // never calls accept, so only an absent listener leaves the SYN unanswered.
        match e.kind() {
            std::io::ErrorKind::ConnectionRefused => HolderRpcError {
                fault: RpcFault::ConnectRefused,
                detail: e.to_string(),
            },
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => HolderRpcError {
                fault: RpcFault::ConnectRefused,
                detail: format!("connect timed out: {e}"),
            },
            _ => HolderRpcError {
                fault: RpcFault::Io,
                detail: e.to_string(),
            },
        }
    })?;
    stream.set_read_timeout(Some(timeout)).map_err(|e| io(&e))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| io(&e))?;
    let payload = serde_json::to_string(req).map_err(|e| HolderRpcError {
        fault: RpcFault::Invalid,
        detail: e.to_string(),
    })?;
    stream.write_all(payload.as_bytes()).map_err(|e| io(&e))?;
    stream.write_all(b"\n").map_err(|e| io(&e))?;
    stream.flush().map_err(|e| io(&e))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).map_err(|e| io(&e))? == 0 {
        return Err(closed("holder closed without response".into()));
    }
    serde_json::from_str(line.trim()).map_err(|e| HolderRpcError {
        fault: RpcFault::Invalid,
        detail: e.to_string(),
    })
}

/// Prove the process behind `want.control_addr` is the holder named by the project's
/// persist record. No address → the classifier's `NoSocket`; no connection →
/// `NoListener`; any mismatch, timeout, closed connection, non-JSON response or
/// non-holder service is `Unproven`. The persist `alive` flag is never consulted here.
pub(crate) fn prove_holder(
    record: &ProjectRecord,
    want: &PersistIdentity,
    timeout: std::time::Duration,
) -> std::result::Result<VerifiedHolder, HolderTermination> {
    let addr = match classify_persist(want) {
        PersistClass::NoAddress(why) => return Err(HolderTermination::NoSocket { why }),
        PersistClass::Addressed { addr } => addr,
    };
    let mtime = match persist_identity(record) {
        Ok(Some((_, mtime))) => mtime,
        Ok(None) => {
            return Err(HolderTermination::Unproven {
                why: "persist-missing".into(),
            });
        }
        // A fence-read refusal is a distinct operator token, never "unreadable".
        Err(e) if e.to_string().starts_with("persist-unstable") => {
            return Err(HolderTermination::Unproven {
                why: "persist-unstable".into(),
            });
        }
        Err(_) => {
            return Err(HolderTermination::Unproven {
                why: "persist-unreadable".into(),
            });
        }
    };
    let resp = match holder_rpc_sync(&addr, &HoldRequest::Ping, timeout) {
        Ok(resp) => resp,
        Err(e) if e.fault == RpcFault::ConnectRefused => {
            return Err(HolderTermination::NoListener { addr });
        }
        Err(e) => {
            return Err(HolderTermination::Unproven {
                why: e.reason("ping"),
            });
        }
    };
    if !resp.ok {
        return Err(HolderTermination::Unproven {
            why: "ping-not-ok".into(),
        });
    }
    let Some(harness) = resp.harness else {
        return Err(HolderTermination::Unproven {
            why: "no-harness".into(),
        });
    };
    match (want.session_id.as_deref(), harness.session_id.as_deref()) {
        (Some(a), Some(b)) if a == b => {}
        _ => {
            return Err(HolderTermination::Unproven {
                why: "identity:session".into(),
            });
        }
    }
    match (want.cwd.as_deref(), harness.cwd.as_deref()) {
        (Some(a), Some(b)) if crate::worktree::paths_same(a, b) => {}
        _ => {
            return Err(HolderTermination::Unproven {
                why: "identity:cwd".into(),
            });
        }
    }
    if !harness.adapter.eq_ignore_ascii_case(&want.adapter) {
        return Err(HolderTermination::Unproven {
            why: "identity:adapter".into(),
        });
    }
    if let (Some(a), Some(b)) = (want.pid, harness.pid)
        && a != b
    {
        return Err(HolderTermination::Unproven {
            why: "identity:pid".into(),
        });
    }
    if let (Some(a), Some(b)) = (want.model.as_deref(), harness.model.as_deref())
        && a != b
    {
        return Err(HolderTermination::Unproven {
            why: "identity:model".into(),
        });
    }
    if let (Some(a), Some(b)) = (want.bin.as_deref(), harness.bin.as_deref())
        && !crate::worktree::paths_same(a, b)
    {
        return Err(HolderTermination::Unproven {
            why: "identity:bin".into(),
        });
    }
    Ok(VerifiedHolder {
        record: record.clone(),
        want: want.clone(),
        addr,
        mtime,
        harness,
    })
}

/// **The only function that sends a holder `Shutdown`.** Re-reads the persist in the
/// sealed 0107 order, then delivers. Takes an unforgeable [`VerifiedHolder`].
fn deliver_shutdown(v: &VerifiedHolder, intent: Intent) -> HolderTermination {
    let handle = match load_persist(&v.record) {
        Ok(Some(handle)) => handle,
        Ok(None) => {
            return HolderTermination::Unproven {
                why: "persist-missing".into(),
            };
        }
        Err(_) => {
            return HolderTermination::Unproven {
                why: "persist-unreadable".into(),
            };
        }
    };
    let path = match persist_path(&v.record) {
        Ok(path) => path,
        Err(_) => {
            return HolderTermination::Unproven {
                why: "persist-unreadable".into(),
            };
        }
    };
    let mtime = match std::fs::metadata(&path).and_then(|meta| meta.modified()) {
        Ok(mtime) => mtime,
        Err(_) => {
            return HolderTermination::Unproven {
                why: "persist-unreadable".into(),
            };
        }
    };
    let now = PersistIdentity::from_handle(&handle);
    match classify_persist(&now) {
        PersistClass::NoAddress(why) => {
            let old = PersistRef::new(v.want.clone(), v.mtime);
            if why == NoSocketWhy::DeadAfterExit && same_record(&old, &now, mtime) {
                return HolderTermination::Exited {
                    holder_pid: v.want.holder_pid,
                    session_id: v.want.session_id.clone(),
                };
            }
            // Any other no-address shape is a different producer's record, never
            // evidence that *this* holder exited.
            return HolderTermination::Unproven {
                why: "persist-changed".into(),
            };
        }
        PersistClass::Addressed { .. } => {}
    }
    if now.prompt_in_flight && intent == Intent::Sweep {
        return HolderTermination::Unproven {
            why: "prompt-in-flight".into(),
        };
    }
    if mtime != v.mtime {
        return HolderTermination::Unproven {
            why: "recently-active".into(),
        };
    }
    if !now.same_identity(&v.want) {
        return HolderTermination::Unproven {
            why: "persist-changed".into(),
        };
    }
    match holder_rpc_sync(
        &v.addr,
        &HoldRequest::Shutdown,
        crate::harness::reap::RPC_TIMEOUT,
    ) {
        Ok(resp) if resp.ok => HolderTermination::Delivered {
            holder_pid: now.holder_pid,
            session_id: now.session_id.clone(),
        },
        Ok(_) => HolderTermination::Unproven {
            why: "shutdown-rejected".into(),
        },
        Err(e) if e.fault == RpcFault::ConnectRefused => HolderTermination::NoListener {
            addr: v.addr.clone(),
        },
        Err(e) => HolderTermination::Unproven {
            why: e.reason("shutdown"),
        },
    }
}

/// `prove_holder` + `deliver_shutdown`.
pub(crate) fn terminate_holder(
    record: &ProjectRecord,
    want: &PersistIdentity,
    intent: Intent,
) -> HolderTermination {
    match prove_holder(record, want, crate::harness::reap::RPC_TIMEOUT) {
        Ok(v) => deliver_shutdown(&v, intent),
        Err(termination) => termination,
    }
}

/// The sweep's entry: proof already happened in this tick.
pub(crate) fn terminate_verified(v: VerifiedHolder, intent: Intent) -> HolderTermination {
    deliver_shutdown(&v, intent)
}

/// Load → ownership gate → terminate. `old` is `None` only when the persist was already
/// absent; the caller must hand it (never a post-Ping re-read) to the wait/compare
/// helpers so an attempt stamp survives.
pub(crate) fn terminate_persisted_holder(
    record: &ProjectRecord,
    intent: Intent,
) -> Result<Terminated> {
    let Some((identity, mtime)) = persist_identity(record)? else {
        return Ok(Terminated {
            old: None,
            outcome: HolderTermination::Unproven {
                why: "persist-missing".into(),
            },
        });
    };
    let old = PersistRef::new(identity.clone(), mtime);
    let outcome = if identity.project_id != record.id {
        HolderTermination::Unproven {
            why: "identity:project-id".into(),
        }
    } else if matches!(classify_persist(&identity), PersistClass::Addressed { .. })
        && !crate::harness::reap::cwd_is_owned(record, &identity)
    {
        // Only an addressed record can name a holder identity; an attempt/exit record
        // legitimately has no cwd.
        HolderTermination::Unproven {
            why: "identity:cwd-not-owned".into(),
        }
    } else {
        terminate_holder(record, &identity, intent)
    };
    Ok(Terminated {
        old: Some(old),
        outcome,
    })
}

/// Poll for the old holder's own exit record. Expiry is `TimedOut` — never success.
pub(crate) fn wait_holder_gone(
    record: &ProjectRecord,
    old: &PersistRef,
    budget: Duration,
) -> WaitGone {
    let deadline = std::time::Instant::now() + budget;
    loop {
        match persist_identity(record) {
            Ok(None) => {
                return WaitGone::Gone {
                    how: GoneHow::Absent,
                };
            }
            Ok(Some((now, mtime))) => {
                if !same_record(old, &now, mtime) {
                    return WaitGone::Superseded;
                }
                if classify_persist(&now) == PersistClass::NoAddress(NoSocketWhy::DeadAfterExit) {
                    return WaitGone::Gone {
                        how: GoneHow::Exited,
                    };
                }
            }
            // Unreadable is not absent; keep polling and let the budget decide.
            Err(_) => {}
        }
        if std::time::Instant::now() >= deadline {
            return WaitGone::TimedOut;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Compare-and-act: remove the persist only while it is still `old`.
///
/// **`false` is a verdict, not a hint** — the caller performs none of its success
/// effects (no spawn, detach, unregister, tree removal or dead status). Absent means
/// there is nothing to do (`true`).
/// Bool verdict kept by OD-121-6 (see [`write_holder_ready`]).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn clear_persist_if(record: &ProjectRecord, old: &PersistRef) -> bool {
    matches!(clear_persist_if_outcome(record, old), PersistAct::Ok)
}

/// The fenced body behind [`clear_persist_if`]. The lock-free absent fast path keeps a
/// never-started project from creating `{state_dir}` or the sentinel: no act is taken, so
/// there is nothing to fence and a claim created concurrently is a newer record the old
/// ref cannot own.
pub(crate) fn clear_persist_if_outcome(record: &ProjectRecord, old: &PersistRef) -> PersistAct {
    match persist_absent_fast_path(record) {
        Ok(true) => return PersistAct::Ok,
        Ok(false) => {}
        Err(e) => return PersistAct::Unavailable(e),
    }
    match with_persist_lock(record, |lock| {
        let Some((handle, mtime)) = persist_snapshot(record)? else {
            return Ok(PersistAct::Ok);
        };
        if !same_record(old, &PersistIdentity::from_handle(&handle), mtime) {
            return Ok(PersistAct::Refused);
        }
        persist_pause_before_write("clear");
        remove_persist_locked(lock)?;
        Ok(PersistAct::Ok)
    }) {
        Ok(act) => act,
        Err(e) => PersistAct::Unavailable(e),
    }
}

/// Compare-and-act: write the truthful dead record only while it is still `old`.
///
/// **`false` is a verdict** (see [`clear_persist_if`]). This is the only path to
/// [`persist_marked_dead`].
pub(crate) fn mark_dead_if(record: &ProjectRecord, old: &PersistRef) -> bool {
    matches!(mark_dead_if_outcome(record, old), PersistAct::Ok)
}

pub(crate) fn mark_dead_if_outcome(record: &ProjectRecord, old: &PersistRef) -> PersistAct {
    match persist_absent_fast_path(record) {
        Ok(true) => return PersistAct::Ok,
        Ok(false) => {}
        Err(e) => return PersistAct::Unavailable(e),
    }
    match with_persist_lock(record, |lock| {
        let Some((handle, mtime)) = persist_snapshot(record)? else {
            return Ok(PersistAct::Ok);
        };
        if !same_record(old, &PersistIdentity::from_handle(&handle), mtime) {
            return Ok(PersistAct::Refused);
        }
        persist_pause_before_write("dead");
        save_persist_locked(lock, &persist_marked_dead(&handle))?;
        Ok(PersistAct::Ok)
    }) {
        Ok(act) => act,
        Err(e) => PersistAct::Unavailable(e),
    }
}

/// `Ok(true)` = the persist is absent (nothing to fence); `Ok(false)` = present.
fn persist_absent_fast_path(record: &ProjectRecord) -> Result<bool> {
    let path = persist_path(record)?;
    match std::fs::metadata(&path) {
        Ok(_) => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e.into()),
    }
}

/// One discard receipt naming the identity that is being given up.
pub(crate) fn discard_line(record: &ProjectRecord, old: &PersistRef) {
    let holder = old
        .identity
        .holder_pid
        .map(|pid| pid.to_string())
        .unwrap_or_else(|| "-".into());
    let session = old.identity.session_id.as_deref().unwrap_or("-");
    let shape = match classify_persist(&old.identity) {
        PersistClass::NoAddress(why) => why_token(why).to_string(),
        PersistClass::Addressed { .. } => "addressed".into(),
    };
    crate::progress_log::append(
        record,
        "holder-persist-discarded",
        &format!(
            "shape={shape} holder_pid={holder} session_id={session} mtime={:?}",
            old.mtime
        ),
    );
}

pub(crate) fn why_token(why: NoSocketWhy) -> &'static str {
    match why {
        NoSocketWhy::DeadAfterExit => "dead-after-exit",
        NoSocketWhy::FailedStart => "failed-start",
        NoSocketWhy::PendingStart => "pending-start",
        NoSocketWhy::AliveSelfNoAddress => "alive-self",
        NoSocketWhy::AliveForeignNoAddress => "alive-foreign",
        NoSocketWhy::AliveUnknownNoAddress => "alive-unknown",
    }
}

/// 0110 `roles prove`: the holder's own bounded `Ping` (one Ping, one identity proof).
pub(crate) fn probe_holder(
    record: &ProjectRecord,
    _role: &str,
) -> Result<Option<GrokHarnessStatus>> {
    let Some(handle) = load_persist(record)? else {
        return Ok(None);
    };
    if !handle.alive || handle.control_addr.is_none() {
        return Ok(None);
    }
    let want = PersistIdentity::from_handle(&handle);
    let Ok(verified) = prove_holder(record, &want, crate::harness::reap::RPC_TIMEOUT) else {
        return Ok(None);
    };
    let status = verified.harness;
    let (Some(persisted), Some(pinged)) = (handle.pid, status.pid) else {
        return Ok(None);
    };
    if persisted != pinged {
        return Ok(None);
    }
    Ok(Some(status))
}

/// Test-only seed: every fixture goes through the real fenced path.
#[cfg(test)]
fn save_persist(record: &ProjectRecord, handle: &PersistedGrokHandle) -> Result<()> {
    with_persist_lock(record, |lock| save_persist_locked(lock, handle))
}

/// W10: the async `shutdown` cannot contain the lock literal (DoD-2); this sync helper
/// owns the fence and is called after the caller's last `.await`. Keeps 0117's
/// documented no-compare posture: in-process sessions exist only in tests.
fn save_in_process_shutdown(record: &ProjectRecord, dead: &PersistedGrokHandle) -> Result<()> {
    with_persist_lock(record, |lock| save_persist_locked(lock, dead))
}

fn persist_adapter_name(harness: &str) -> String {
    if harness.trim().is_empty() {
        default_adapter()
    } else {
        harness.to_string()
    }
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

/// One journaled line + stderr for a status-fidelity writer that could not take the
/// fence (OD-121-8: the dispatch semantics do not change, but the skip is observable).
fn journal_persist_lock_skip(record: &ProjectRecord, e: &CoordinatorError) {
    let text = e.to_string();
    let token = if text.contains("persist-lock-timeout") {
        "persist-lock-timeout"
    } else if text.contains("persist-lock-order") {
        "persist-lock-order"
    } else if text.contains("persist-lock-io") {
        "persist-lock-io"
    } else {
        "persist-unavailable"
    };
    crate::progress_log::append(
        record,
        "persist-lock",
        &format!("{token} project={}", record.id),
    );
    eprintln!(
        "coordinator: persist-lock: {token} project={} ({e})",
        record.id
    );
}

fn set_prompt_in_flight(record: &ProjectRecord, value: bool) {
    let result = with_persist_lock(record, |lock| {
        let Some((mut handle, _)) = persist_snapshot(record)? else {
            return Ok(());
        };
        handle.prompt_in_flight = value;
        persist_pause_before_write("flag");
        save_persist_locked(lock, &handle)
    });
    if let Err(e) = result {
        journal_persist_lock_skip(record, &e);
    }
}

/// Copy fidelity onto the existing persist file. Leaves `control_addr` and `prompt_in_flight` alone.
fn remember_fidelity(record: &ProjectRecord, session: &GrokSession) {
    let result = with_persist_lock(record, |lock| {
        let Some((mut handle, _)) = persist_snapshot(record)? else {
            return Ok(());
        };
        handle.context_usage = session.context_usage.clone();
        handle.last_signal = session.last_signal.clone();
        handle.last_tool_title = session.last_tool_title.clone();
        persist_pause_before_write("fidelity");
        save_persist_locked(lock, &handle)
    });
    if let Err(e) = result {
        journal_persist_lock_skip(record, &e);
    }
}

/// Holder `Cancel` RPC to that epoch's `control_addr`. A book with no epoch is
/// `ambiguous run` and does not read the legacy file.
pub(crate) async fn holder_cancel(record: &ProjectRecord, epoch: Option<u64>) -> Result<()> {
    let Some(h) = load_persist_at(record, epoch.or_else(crate::state::active_run_epoch))? else {
        return Ok(());
    };
    let Some(addr) = h.control_addr else {
        return Ok(());
    };
    let _ = holder_rpc(&addr, &HoldRequest::Cancel).await;
    Ok(())
}

/// Bounded Adopt / Release / Ping for the parent. Not held under the run-state lock.
pub(crate) fn adopt_holder_epoch(addr: &str, epoch: u64) -> Result<bool> {
    Ok(holder_rpc_bounded(addr, &HoldRequest::AdoptEpoch { epoch })?.ok)
}

pub(crate) fn release_holder_epoch_rpc(addr: &str) -> Result<bool> {
    Ok(holder_rpc_bounded(addr, &HoldRequest::ReleaseEpoch)?.ok)
}

/// Release the holder that remains when a book collapses to one entry.
/// No persist, or a holder that is not alive, is already clear. A missing
/// address or a refused RPC leaves the book in place.
pub(crate) fn release_kept_holder(record: &ProjectRecord, epoch: u64) -> bool {
    let Ok(Some(handle)) = load_persist_at(record, Some(epoch)) else {
        return true;
    };
    if !(handle.alive || handle.prompt_in_flight) {
        return true;
    }
    let Some(addr) = handle.control_addr else {
        return false;
    };
    release_holder_epoch_rpc(&addr).unwrap_or(false)
}

/// Book-only stop. Absent persist is already gone. A refused or timed-out Shutdown is `Ok(false)`.
pub(crate) fn shutdown_epoch(record: &ProjectRecord, epoch: u64) -> Result<bool> {
    let Some(handle) = load_persist_at(record, Some(epoch))? else {
        return Ok(true);
    };
    if !(handle.alive || handle.prompt_in_flight) {
        return Ok(true);
    }
    let Some(addr) = handle.control_addr else {
        return Ok(false);
    };
    let req = HoldRequest::Shutdown;
    match holder_rpc_bounded(&addr, &req) {
        Ok(resp) if resp.ok => {}
        _ => return Ok(false),
    }
    let ping = HoldRequest::Ping;
    match holder_rpc_bounded(&addr, &ping) {
        Ok(resp) if resp.ok => Ok(false),
        _ => Ok(true),
    }
}

fn holder_rpc_bounded(addr: &str, req: &HoldRequest) -> Result<HoldResponse> {
    holder_rpc_sync(addr, req, crate::harness::reap::RPC_TIMEOUT)
        .map_err(|err| CoordinatorError::Message(format!("holder rpc: {}", err.detail)))
}

/// Ping a singular live holder before a second run. `Ok(None)` means no Adopt is required.
/// `Ok(Some(addr))` means the caller must Adopt after the book is written.
pub(crate) fn second_run_adopt_addr(
    record: &ProjectRecord,
    first_epoch: u64,
) -> Result<Option<String>> {
    let Some(handle) = load_persist(record)? else {
        return Ok(None);
    };
    if !(handle.alive || handle.prompt_in_flight) {
        return Ok(None);
    }
    let Some(addr) = handle.control_addr.clone() else {
        return Err(CoordinatorError::Message(
            "holder unproven (no control_addr)".into(),
        ));
    };
    let dir = crate::worktree::epoch_dir(record, first_epoch)?;
    if handle
        .cwd
        .as_ref()
        .is_none_or(|cwd| !crate::worktree::paths_same(cwd, &dir))
    {
        return Err(CoordinatorError::Message(
            "holder cwd is not the first epoch worktree".into(),
        ));
    }
    let ping = holder_rpc_bounded(&addr, &HoldRequest::Ping)?;
    if ping.epoch_protocol != Some(EPOCH_PROTOCOL_VERSION) {
        return Err(CoordinatorError::Message(
            "holder has no epoch protocol".into(),
        ));
    }
    Ok(Some(addr))
}

fn move_tracked(
    src: &std::path::Path,
    dest: &std::path::Path,
    moved: &mut Vec<(std::path::PathBuf, std::path::PathBuf)>,
) -> Result<()> {
    if !src.exists() {
        return Ok(());
    }
    move_into(src, dest)?;
    moved.push((dest.to_path_buf(), src.to_path_buf()));
    Ok(())
}

fn rollback_moves(moved: &[(std::path::PathBuf, std::path::PathBuf)]) {
    for (dest, src) in moved.iter().rev() {
        if src.is_dir() {
            let _ = std::fs::remove_dir(src);
        }
        if let Some(parent) = src.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::rename(dest, src);
    }
}

fn move_into(src: &std::path::Path, dest: &std::path::Path) -> Result<()> {
    if !src.exists() {
        return Ok(());
    }
    if dest.exists() {
        return Err(CoordinatorError::Message(format!(
            "refusing to replace {}",
            dest.display()
        )));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(src, dest)?;
    Ok(())
}

/// Move the singular persist and sidecars onto `epoch` before the book is written.
pub(crate) fn move_singular_sidecars(record: &ProjectRecord, epoch: u64) -> Result<()> {
    let dir = crate::state::resolve_state_dir(record)?;
    with_persist_lock(record, |lock| {
        let mut moved = Vec::new();
        let result = (|| {
            let session = dir
                .join("sessions")
                .join(epoch.to_string())
                .join("harness-grok.json");
            move_tracked(&lock.data, &session, &mut moved)?;
            move_tracked(
                &dir.join("harness-progress.json"),
                &dir.join("harness-progress").join(format!("{epoch}.json")),
                &mut moved,
            )?;
            let outcomes = dir.join("outcomes");
            let scoped = outcomes.join(epoch.to_string());
            move_tracked(
                &outcomes.join("current.json"),
                &scoped.join("current.json"),
                &mut moved,
            )?;
            move_tracked(
                &outcomes.join("current.applied.json"),
                &scoped.join("current.applied.json"),
                &mut moved,
            )?;
            move_tracked(&outcomes.join("roles"), &scoped.join("roles"), &mut moved)?;
            let reviews = dir.join("reviews");
            if reviews.is_dir()
                && reviews
                    .read_dir()
                    .ok()
                    .and_then(|mut it| it.next())
                    .is_some()
            {
                let parked = dir.join(format!(".reviews-move-{epoch}"));
                move_tracked(&reviews, &parked, &mut moved)?;
                std::fs::create_dir_all(&reviews)?;
                move_tracked(&parked, &reviews.join(epoch.to_string()), &mut moved)?;
            }
            move_tracked(
                &dir.join("trusted-evidence").join("current.json"),
                &dir.join("trusted-evidence").join(format!("{epoch}.json")),
                &mut moved,
            )?;
            Ok(())
        })();
        if result.is_err() {
            rollback_moves(&moved);
        }
        result
    })
}

/// Inverse of [`move_singular_sidecars`] after a failed Adopt whose Release succeeded.
pub(crate) fn restore_singular_sidecars(record: &ProjectRecord, epoch: u64) -> Result<()> {
    let dir = crate::state::resolve_state_dir(record)?;
    let sentinel = persist_lock_path(record)?;
    crate::state::ensure_state_dir(record)?;
    let _lock = PersistLock::acquire(&sentinel, || Ok(dir.join("harness-grok.json")))?;
    {
        let session = dir
            .join("sessions")
            .join(epoch.to_string())
            .join("harness-grok.json");
        move_into(&session, &dir.join("harness-grok.json"))?;
        move_into(
            &dir.join("harness-progress").join(format!("{epoch}.json")),
            &dir.join("harness-progress.json"),
        )?;
        let outcomes = dir.join("outcomes");
        let scoped = outcomes.join(epoch.to_string());
        move_into(&scoped.join("current.json"), &outcomes.join("current.json"))?;
        move_into(
            &scoped.join("current.applied.json"),
            &outcomes.join("current.applied.json"),
        )?;
        move_into(&scoped.join("roles"), &outcomes.join("roles"))?;
        let nested = dir.join("reviews").join(epoch.to_string());
        if nested.exists() {
            let parked = dir.join(format!(".reviews-restore-{epoch}"));
            move_into(&nested, &parked)?;
            let reviews = dir.join("reviews");
            if reviews
                .read_dir()
                .ok()
                .and_then(|mut it| it.next())
                .is_none()
            {
                let _ = std::fs::remove_dir(&reviews);
            }
            move_into(&parked, &reviews)?;
        }
        move_into(
            &dir.join("trusted-evidence").join(format!("{epoch}.json")),
            &dir.join("trusted-evidence").join("current.json"),
        )?;
        Ok(())
    }
}

/// Sync snapshot for `StatusView` (in-process pool, else persist file).
///
/// A book is not answered from the parent pool. That map is keyed only by
/// `project_id`, so it can be the other epoch's session. A scoped call reads
/// `sessions/{epoch}/harness-grok.json`.
pub fn status_bundle_sync(record: &ProjectRecord) -> Option<HarnessStatusBundle> {
    let book_open = matches!(
        crate::state::load_run_book(record),
        Ok(crate::state::RunBook::Many(_))
    );
    if !book_open
        && let Some(pool) = POOL.get()
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

/// Armed only by the inject-thread proof. The thread still calls
/// `resolve_selected`; this only restores the home that proof registered.
#[cfg(test)]
pub(crate) static INJECT_RESOLVE_CAPTURE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
pub(crate) static INJECT_HOME: std::sync::Mutex<Option<std::path::PathBuf>> =
    std::sync::Mutex::new(None);

/// Hold the env lock and point `COORDINATOR_HOME` at the registry the tick
/// registered. A parallel test can retarget that var after `tick` spawns this
/// thread; `resolve_selected` then has to run against the registered home.
#[cfg(test)]
pub(crate) struct PinnedInjectHome {
    previous: Option<String>,
    _guard: Option<std::sync::MutexGuard<'static, ()>>,
}

#[cfg(test)]
pub(crate) fn pin_inject_home() -> PinnedInjectHome {
    // One inject thread consumes the capture. Leaving it armed redirects
    // every later inject in the process at this home.
    if !INJECT_RESOLVE_CAPTURE.swap(false, std::sync::atomic::Ordering::AcqRel) {
        return PinnedInjectHome {
            previous: None,
            _guard: None,
        };
    }
    let guard = crate::config::test_env_lock();
    let home = INJECT_HOME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let previous = std::env::var(crate::config::ENV_COORDINATOR_HOME).ok();
    if let Some(home) = home {
        unsafe { std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home) };
    }
    PinnedInjectHome {
        previous,
        _guard: Some(guard),
    }
}

#[cfg(test)]
impl Drop for PinnedInjectHome {
    fn drop(&mut self) {
        if self._guard.is_none() {
            return;
        }
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var(crate::config::ENV_COORDINATOR_HOME, value) },
            None => unsafe { std::env::remove_var(crate::config::ENV_COORDINATOR_HOME) },
        }
    }
}

fn resolve_record(project: Option<&str>, infer_cwd: bool) -> Result<ProjectRecord> {
    crate::api::resolve_selected(project, infer_cwd)
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

#[cfg(test)]
async fn apply_turn(
    record: &ProjectRecord,
    turn: std::result::Result<PromptResult, CoordinatorError>,
    harness: GrokHarnessStatus,
    injected_phase: &str,
    spawn: SpawnTally,
) -> Result<HarnessPromptView> {
    apply_turn_checked(
        record,
        turn,
        harness,
        injected_phase,
        spawn,
        None,
        None,
        None,
        false,
    )
    .await
}

fn commit_prepared(
    record: &ProjectRecord,
    outcome: PhaseOutcome,
    prepared: Option<&PreparedInjection>,
) -> Result<ApplyIdentity> {
    match prepared {
        Some(prepared) => write_and_apply_prepared(record, outcome, prepared),
        None => write_and_apply(record, outcome).map(ApplyIdentity::Applied),
    }
}

fn apply_outcome(
    record: &ProjectRecord,
    outcome: PhaseOutcome,
    prepared: Option<&PreparedInjection>,
    harness: GrokHarnessStatus,
) -> Result<std::result::Result<StatusView, HarnessPromptView>> {
    match commit_prepared(record, outcome, prepared)? {
        ApplyIdentity::Applied(view) => Ok(Ok(view)),
        ApplyIdentity::Stale(reason) => {
            let prepared = prepared.expect("stale identity requires a prepared injection");
            Ok(Err(prepared_skipped_view(
                record, harness, prepared, &reason,
            )))
        }
    }
}

fn prepared_skipped_view(
    record: &ProjectRecord,
    harness: GrokHarnessStatus,
    prepared: &PreparedInjection,
    reason: &str,
) -> HarnessPromptView {
    journal_sample_skip_for(
        record,
        prepared.target(),
        prepared.track_id.as_deref(),
        reason,
    );
    HarnessPromptView {
        text: None,
        stop_reason: None,
        applied: false,
        skipped: Some(true),
        error: None,
        failure_class: None,
        status: None,
        harness: Some(harness),
    }
}

fn journal_sample_skip(record: &ProjectRecord, target: Option<&ResolvedExecution>, reason: &str) {
    journal_sample_skip_for(record, target, None, reason);
}

/// A stale/unresolved skip journals the track from the frozen
/// `PreparedInjection.track_id` when the target is `None` (0098), never `(none)`.
fn journal_sample_skip_for(
    record: &ProjectRecord,
    target: Option<&ResolvedExecution>,
    fallback_track: Option<&str>,
    reason: &str,
) {
    let track = target
        .map(|t| t.track_id.as_str())
        .or(fallback_track)
        .unwrap_or("(none)");
    let source = target.map(|t| t.source.as_str()).unwrap_or("none");
    crate::progress_log::append(
        record,
        "sample-skip",
        &format!("leg sample skipped: track={track} source={source} reason={reason}"),
    );
}

/// Apply a detected terminal block through the frozen identity only.
///
/// Never bare `write_and_apply`: a stale identity is a structured skip that preserves
/// the turn text, writes no artifact and does not fail the newer run.
#[allow(clippy::too_many_arguments)]
fn apply_block_declaration(
    record: &ProjectRecord,
    reason: String,
    text: String,
    stop_reason: Option<String>,
    harness: GrokHarnessStatus,
    injected_phase: &str,
    injected_epoch: u64,
    identity: &PreparedInjection,
) -> Result<HarnessPromptView> {
    let outcome = PhaseOutcome::failure(
        injected_phase,
        FailureClass::Blocked,
        OutcomeSource::Adapter,
        Some(reason),
        Some(injected_epoch),
    );
    match write_and_apply_prepared(record, outcome, identity)? {
        ApplyIdentity::Applied(view) => Ok(HarnessPromptView {
            text: Some(text),
            stop_reason,
            applied: true,
            skipped: None,
            error: None,
            failure_class: Some(FailureClass::Blocked),
            status: Some(view),
            harness: Some(harness),
        }),
        ApplyIdentity::Stale(reason) => {
            journal_sample_skip_for(
                record,
                identity.target(),
                identity.track_id.as_deref(),
                &reason,
            );
            Ok(HarnessPromptView {
                text: Some(text),
                stop_reason,
                applied: false,
                skipped: Some(true),
                error: None,
                failure_class: None,
                status: None,
                harness: Some(harness),
            })
        }
    }
}

fn trusted_evidence_verdict(
    record: &ProjectRecord,
    prepared: &PreparedInjection,
) -> Option<crate::workflow::trusted_evidence::Evidence> {
    let record = record.clone();
    let prepared = prepared.clone();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        crate::workflow::trusted_evidence::read(&record, &prepared)
    })) {
        Ok(evidence) => Some(evidence),
        Err(payload) => {
            std::mem::forget(payload);
            None
        }
    }
}

/// `no_change` is the unchanged-tree verdict (0095) computed beside the prompt, where the session
/// cwd is known. `Some` turns a would-be success into a `HarnessCrash` failure.
///
/// `block_identity` is the block-only frozen identity captured at inject time (0098). It is
/// **never** passed in the `prepared` slot: `prepared` alone still drives the early skip, the
/// ordinary `commit_prepared` path, the continuation target and the no-change verdict.
#[allow(clippy::too_many_arguments)]
async fn apply_turn_checked(
    record: &ProjectRecord,
    turn: std::result::Result<PromptResult, CoordinatorError>,
    harness: GrokHarnessStatus,
    injected_phase: &str,
    spawn: SpawnTally,
    no_change: Option<String>,
    prepared: Option<&PreparedInjection>,
    block_identity: Option<&PreparedInjection>,
    manual_prepared_phase: bool,
) -> Result<HarnessPromptView> {
    let state = load_run_state(record)?;
    let drifted = state.phase != injected_phase;
    // Outcomes are stamped with the injected epoch, never a newly loaded one.
    let injected_epoch = prepared
        .or(block_identity)
        .map(|p| p.run_epoch)
        .unwrap_or(state.run_epoch);
    if let Some(prepared) = prepared
        && (state.status != RunStatus::Running || !prepared.matches_state(&state))
    {
        return Ok(prepared_skipped_view(
            record,
            harness,
            prepared,
            "stale-injection",
        ));
    }
    match turn {
        Ok(pr) => {
            let mut applied = false;
            let mut status = None;
            let mut error = None;
            let mut failure_class = None;
            let skip = state.status != RunStatus::Running
                || crate::harness::abort::stop_reason_is_cancelled(pr.stop_reason.as_deref())
                || harness_is_aborted(&state, &harness)
                || drifted;
            if !skip {
                let spawn_dead =
                    crate::workflow::graph::is_grok_bound(injected_phase) && spawn.all_failed();
                if spawn_dead {
                    let msg = spawn_fail_message(&spawn);
                    let outcome = PhaseOutcome::failure(
                        injected_phase,
                        FailureClass::HarnessCrash,
                        OutcomeSource::Adapter,
                        Some(msg.clone()),
                        Some(injected_epoch),
                    );
                    match apply_outcome(record, outcome, prepared, harness.clone())? {
                        Ok(view) => status = Some(view),
                        Err(skip_view) => return Ok(skip_view),
                    }
                    applied = true;
                    error = Some(msg);
                    failure_class = Some(FailureClass::HarnessCrash);
                } else {
                    let msg = if pr.text.is_empty() {
                        None
                    } else {
                        Some(pr.text.clone())
                    };
                    let mut trusted: Option<crate::workflow::trusted_evidence::Evidence> = None;
                    let next = if injected_phase == crate::workflow::graph::PHASE_ADVANCE {
                        match crate::workflow::prompts::parse_next_track_line(&pr.text) {
                            Some(Some(id)) => Some(id),
                            Some(None) => Some(String::new()),
                            None => None,
                        }
                    } else {
                        None
                    };
                    let outcome = match crate::harness::grok::harness_error_in_text(&pr.text) {
                        // The turn completed at the transport level but the payload is a
                        // harness failure. Record it as one — never advance on it.
                        Some(err) => {
                            let class = failure_class_for_message(&err);
                            error = Some(err.clone());
                            failure_class = Some(class);
                            PhaseOutcome::failure(
                                injected_phase,
                                class,
                                OutcomeSource::Adapter,
                                Some(err),
                                Some(injected_epoch),
                            )
                        }
                        None => {
                            // 0098: a terminal block declaration is evaluated on the current
                            // turn text only, after harness-error text and before self-check.
                            if let Some(reason) =
                                crate::workflow::block::classify(&pr.text, injected_phase)
                            {
                                let Some(identity) = prepared.or(block_identity) else {
                                    // No frozen identity (manual prompt into a non-Running or
                                    // non-six-phase run): structured skip, text preserved.
                                    journal_sample_skip(record, None, "block-no-identity");
                                    return Ok(HarnessPromptView {
                                        text: Some(pr.text),
                                        stop_reason: pr.stop_reason,
                                        applied: false,
                                        skipped: Some(true),
                                        error: None,
                                        failure_class: None,
                                        status: None,
                                        harness: Some(harness),
                                    });
                                };
                                return apply_block_declaration(
                                    record,
                                    reason,
                                    pr.text,
                                    pr.stop_reason,
                                    harness,
                                    injected_phase,
                                    injected_epoch,
                                    identity,
                                );
                            }
                            // A manual prompt in a prepared phase is not an adapter leg.
                            // Non-block text must not acquire a target or apply an ordinary
                            // outcome (0098 / 0101 interaction).
                            if manual_prepared_phase {
                                return Ok(HarnessPromptView {
                                    text: Some(pr.text),
                                    stop_reason: pr.stop_reason,
                                    applied: false,
                                    skipped: Some(true),
                                    error: None,
                                    failure_class: None,
                                    status: None,
                                    harness: Some(harness),
                                });
                            }
                            if crate::workflow::self_check::hold_continue(
                                record,
                                injected_phase,
                                pr.stop_reason.as_deref(),
                                &pr.text,
                                prepared.and_then(|p| p.target()),
                            )? {
                                return Ok(HarnessPromptView {
                                    text: Some(pr.text),
                                    stop_reason: pr.stop_reason,
                                    applied: false,
                                    skipped: None,
                                    error: None,
                                    failure_class: None,
                                    status: None,
                                    harness: Some(harness),
                                });
                            }
                            match no_change {
                                Some(m) => {
                                    error = Some(m.clone());
                                    failure_class = Some(FailureClass::HarnessCrash);
                                    if state.driver == crate::workflow::WorkflowDriver::Adapter
                                        && crate::harness::no_change::applies(
                                            &state,
                                            injected_phase,
                                        )
                                        && let Some(prepared) = prepared
                                    {
                                        trusted = trusted_evidence_verdict(record, prepared);
                                    }
                                    PhaseOutcome::failure(
                                        injected_phase,
                                        FailureClass::HarnessCrash,
                                        OutcomeSource::Adapter,
                                        Some(m),
                                        Some(injected_epoch),
                                    )
                                }
                                None => PhaseOutcome::success(
                                    injected_phase,
                                    OutcomeSource::Adapter,
                                    msg,
                                    next,
                                    Some(injected_epoch),
                                ),
                            }
                        }
                    };
                    match apply_outcome(record, outcome, prepared, harness.clone())? {
                        Ok(view) => {
                            if let Some(crate::workflow::trusted_evidence::Evidence::Current(cur)) =
                                trusted
                            {
                                crate::progress_log::append(
                                    record,
                                    "evidence",
                                    &format!(
                                        "current epoch={} phase={injected_phase} facts={}",
                                        cur.run_epoch,
                                        cur.facts.len()
                                    ),
                                );
                            }
                            status = Some(view);
                        }
                        Err(skip_view) => return Ok(skip_view),
                    }
                    applied = true;
                }
            }
            Ok(HarnessPromptView {
                text: Some(pr.text),
                stop_reason: pr.stop_reason,
                applied,
                skipped: if skip { Some(true) } else { None },
                error,
                failure_class,
                status,
                harness: Some(harness),
            })
        }
        Err(e) => {
            let raw = e.to_string();
            let skip = state.status != RunStatus::Running
                || harness_is_aborted(&state, &harness)
                || drifted;
            if skip {
                let class = failure_class_for_message(&raw);
                return Ok(HarnessPromptView {
                    text: None,
                    stop_reason: None,
                    applied: false,
                    skipped: Some(true),
                    error: Some(raw),
                    failure_class: Some(class),
                    status: None,
                    harness: Some(harness),
                });
            }
            if raw.starts_with("ACP stdout closed during ") {
                let sid = harness.session_id.as_deref();
                match crate::harness::abort::note_acp_stdout_close(record, &raw, sid) {
                    crate::harness::abort::StdoutCloseDisposition::Ignore
                    | crate::harness::abort::StdoutCloseDisposition::Retry => {
                        return Ok(HarnessPromptView {
                            text: None,
                            stop_reason: None,
                            applied: false,
                            skipped: Some(true),
                            error: None,
                            failure_class: None,
                            status: None,
                            harness: Some(harness),
                        });
                    }
                    crate::harness::abort::StdoutCloseDisposition::Fail { message } => {
                        let fallback = message.clone();
                        let rec = record.clone();
                        let phase = injected_phase.to_string();
                        let message = match tokio::task::spawn_blocking(move || {
                            crate::ci::with_green_pr_prefix(&rec, &phase, message)
                        })
                        .await
                        {
                            Ok(prefixed) => prefixed,
                            Err(_) => fallback,
                        };
                        let outcome = PhaseOutcome::failure(
                            injected_phase,
                            FailureClass::HarnessCrash,
                            OutcomeSource::Adapter,
                            Some(message.clone()),
                            Some(injected_epoch),
                        );
                        let status = Some(
                            match apply_outcome(record, outcome, prepared, harness.clone())? {
                                Ok(view) => view,
                                Err(skip_view) => return Ok(skip_view),
                            },
                        );
                        return Ok(HarnessPromptView {
                            text: None,
                            stop_reason: None,
                            applied: true,
                            skipped: None,
                            error: Some(message),
                            failure_class: Some(FailureClass::HarnessCrash),
                            status,
                            harness: Some(harness),
                        });
                    }
                }
            }
            if let CoordinatorError::Io(io) = &e
                && crate::harness::abort::io_error_is_transport_drop(io)
            {
                let sid = harness.session_id.as_deref();
                match crate::harness::abort::note_transport_drop(record, &e, sid) {
                    crate::harness::abort::StdoutCloseDisposition::Ignore
                    | crate::harness::abort::StdoutCloseDisposition::Retry => {
                        return Ok(HarnessPromptView {
                            text: None,
                            stop_reason: None,
                            applied: false,
                            skipped: Some(true),
                            error: None,
                            failure_class: None,
                            status: None,
                            harness: Some(harness),
                        });
                    }
                    crate::harness::abort::StdoutCloseDisposition::Fail { message } => {
                        let fallback = message.clone();
                        let rec = record.clone();
                        let phase = injected_phase.to_string();
                        let message = match tokio::task::spawn_blocking(move || {
                            crate::ci::with_green_pr_prefix(&rec, &phase, message)
                        })
                        .await
                        {
                            Ok(prefixed) => prefixed,
                            Err(_) => fallback,
                        };
                        let outcome = PhaseOutcome::failure(
                            injected_phase,
                            FailureClass::HarnessCrash,
                            OutcomeSource::Adapter,
                            Some(message.clone()),
                            Some(injected_epoch),
                        );
                        let status = Some(
                            match apply_outcome(record, outcome, prepared, harness.clone())? {
                                Ok(view) => view,
                                Err(skip_view) => return Ok(skip_view),
                            },
                        );
                        return Ok(HarnessPromptView {
                            text: None,
                            stop_reason: None,
                            applied: true,
                            skipped: None,
                            error: Some(message),
                            failure_class: Some(FailureClass::HarnessCrash),
                            status,
                            harness: Some(harness),
                        });
                    }
                }
            }
            let class = failure_class_for_message(&raw);
            let outcome = PhaseOutcome::failure(
                injected_phase,
                class,
                OutcomeSource::Adapter,
                Some(raw.clone()),
                Some(injected_epoch),
            );
            let status = Some(
                match apply_outcome(record, outcome, prepared, harness.clone())? {
                    Ok(view) => view,
                    Err(skip_view) => return Ok(skip_view),
                },
            );
            Ok(HarnessPromptView {
                text: None,
                stop_reason: None,
                applied: true,
                skipped: None,
                error: Some(raw),
                failure_class: Some(class),
                status,
                harness: Some(harness),
            })
        }
    }
}

fn spawn_fail_message(spawn: &SpawnTally) -> String {
    let mut msg = format!("terminal spawn {}/{} ok", spawn.ok, spawn.attempts);
    if let Some(err) = &spawn.last_error {
        msg.push_str(" — last: ");
        msg.push_str(err);
    }
    if msg.chars().count() <= LAST_EVENT_MESSAGE_CAP {
        msg
    } else {
        let cut: String = msg.chars().take(LAST_EVENT_MESSAGE_CAP).collect();
        format!("{cut}…")
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
    harness: &str,
) -> Result<GrokHarnessStatus> {
    let cwd = grok_cwd(record)?;
    pre_claim(record);
    let claim = claim_start(record, harness)?;
    if let Some(stand_in) = spawn_stand_in(record) {
        return stand_in;
    }
    let started = match bin {
        Some(b) => {
            GrokSession::start_with_harness(
                cwd,
                prompt_timeout_for(record),
                b,
                model.as_deref(),
                harness,
            )
            .await
        }
        None => GrokSession::start(cwd, prompt_timeout_for(record)).await,
    };
    let mut session = match started {
        Ok(s) => s,
        Err(e) => {
            // `false` = a newer claim owns the file; report the start failure anyway.
            let _ = mark_start_error(record, Some(&claim), &e.to_string());
            return Err(e);
        }
    };
    session.set_progress_record(record.clone());
    crate::harness::abort::register_cancel_handle(record.id.clone(), session.cancel_handle());
    publish_ready_or_teardown(record, &mut session, None, Some(&claim)).await?;
    let status = session_status(&session);
    global_pool()
        .lock()
        .await
        .insert(record.id.clone(), session);
    Ok(status)
}

pub async fn start(
    project: Option<&str>,
    in_process: bool,
    infer_cwd: bool,
) -> Result<GrokHarnessStatus> {
    start_inner(project, in_process, None, None, infer_cwd, "grok", None).await
}

/// Adapter ticks pass the already-resolved phase bin / model / harness. CLI start does not.
pub async fn start_with_bin(
    project: Option<&str>,
    in_process: bool,
    bin: PathBuf,
    model: Option<String>,
    infer_cwd: bool,
    harness: &str,
    epoch: Option<u64>,
) -> Result<GrokHarnessStatus> {
    start_inner(
        project,
        in_process,
        Some(bin),
        model,
        infer_cwd,
        harness,
        epoch,
    )
    .await
}

async fn start_inner(
    project: Option<&str>,
    in_process: bool,
    bin: Option<PathBuf>,
    model: Option<String>,
    infer_cwd: bool,
    harness: &str,
    epoch: Option<u64>,
) -> Result<GrokHarnessStatus> {
    let rec = resolve_record(project, infer_cwd)?;
    let refuse = crate::harness::abort::should_refuse_reuse(&rec);
    {
        let mut pool = global_pool().lock().await;
        if let Some(s) = pool.status_of(&rec.id)
            && s.alive
        {
            let want = grok_cwd(&rec)?;
            let cwd_mismatch = s
                .cwd
                .as_ref()
                .is_some_and(|cwd| !crate::worktree::paths_same(cwd, &want));
            let identity_ok = identity_matches_parts(&s, harness, model.as_deref(), bin.as_deref());
            if refuse || !identity_ok || cwd_mismatch {
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
        // This arm is inside the async fn: offload the blocking proof/delivery.
        // An absent persist has nothing to terminate (plan 1b); an unreadable one is
        // a refusal with the recovery.
        let existing = match load_persist(&rec) {
            Ok(found) => found,
            Err(e) => {
                return Err(CoordinatorError::Message(format!(
                    "holder persist unreadable on the refuse path: {e} (persist: {}). {}",
                    path_display(&rec),
                    recovery_hint(&rec)
                )));
            }
        };
        if existing.is_some() {
            let termination = run_holder_work({
                let rec = rec.clone();
                move || terminate_persisted_holder(&rec, Intent::Reap)
            })
            .await?;
            match termination.outcome {
                // A concurrent vanish after the load is benign: there is nothing left
                // to terminate, and `claim_start` decides the slot.
                HolderTermination::Unproven { why } if why == "persist-missing" => {}
                HolderTermination::Unproven { why } => {
                    return Err(CoordinatorError::Message(format!(
                        "holder identity unproven ({why}); refusing to start (persist: {}). {}",
                        path_display(&rec),
                        recovery_hint(&rec)
                    )));
                }
                // A successful delivery still fails this start at `claim_start`
                // (persist-appeared) until the holder's own exit record lands: do **not**
                // wait-and-clear here (that would be the clobber this track removes).
                HolderTermination::Delivered { .. } => {}
                other => {
                    settle_start_termination(&rec, termination.old.as_ref(), other)?;
                }
            }
        }
    } else if let Some(existing) =
        reuse_or_reap_existing(&rec, harness, model.as_deref(), bin.as_deref()).await?
    {
        return Ok(existing);
    }

    if in_process {
        spawn_in_process(&rec, bin, model, harness).await
    } else {
        start_holder(&rec, project, bin, model, harness, epoch).await
    }
}

fn path_display(record: &ProjectRecord) -> String {
    persist_path(record)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "{state_dir}/harness-grok.json".into())
}

fn holder_changed(record: &ProjectRecord, detail: &str) -> CoordinatorError {
    CoordinatorError::Message(format!(
        "holder-changed: {detail} (persist: {}). {}",
        path_display(record),
        recovery_hint(record)
    ))
}

fn holder_unproven(record: &ProjectRecord, why: &str) -> CoordinatorError {
    CoordinatorError::Message(format!(
        "holder unproven ({why}) (persist: {}). {}",
        path_display(record),
        recovery_hint(record)
    ))
}

/// One named helper for every async caller of the blocking proof/delivery/wait.
pub(crate) async fn run_holder_work<F, T>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(e) => Err(CoordinatorError::Message(format!(
            "holder work join failed: {e}"
        ))),
    }
}

fn load_ref(record: &ProjectRecord) -> Result<Option<(PersistedGrokHandle, PersistRef)>> {
    Ok(persist_snapshot(record)?.map(|(handle, mtime)| {
        let identity = PersistIdentity::from_handle(&handle);
        (handle, PersistRef::new(identity, mtime))
    }))
}

/// Reuse a live holder that still answers a matching Ping. Every other shape goes
/// through the one termination owner, or refuses.
pub(crate) async fn reuse_or_reap_existing(
    record: &ProjectRecord,
    harness: &str,
    model: Option<&str>,
    bin: Option<&Path>,
) -> Result<Option<GrokHarnessStatus>> {
    let Some((handle, old)) = load_ref(record)? else {
        return Ok(None);
    };
    let identity = old.identity.clone();
    match classify_persist(&identity) {
        PersistClass::NoAddress(why) => match why {
            NoSocketWhy::AliveSelfNoAddress => {
                // A stale record for this process's own dead session: record the
                // discarded identity, then compare-clear (never over a newer write).
                after_gone(record);
                discard_line(record, &old);
                persist_act_or_refused(clear_persist_if_outcome(record, &old), || {
                    holder_changed(record, "the record changed before the clear")
                })?;
                Ok(None)
            }
            NoSocketWhy::AliveForeignNoAddress | NoSocketWhy::AliveUnknownNoAddress => {
                let token = if why == NoSocketWhy::AliveForeignNoAddress {
                    "foreign-holder"
                } else {
                    "unknown-record"
                };
                Err(holder_unproven(record, token))
            }
            _ => {
                settle_start_no_address(record, &old, why).await?;
                Ok(None)
            }
        },
        PersistClass::Addressed { .. } => {
            if identity.project_id != record.id {
                return Err(holder_unproven(record, "identity:project-id"));
            }
            if !crate::harness::reap::cwd_is_owned(record, &identity) {
                return Err(holder_unproven(record, "identity:cwd-not-owned"));
            }
            let want = match grok_cwd(record) {
                Ok(path) => path,
                Err(_) => return Err(holder_unproven(record, "identity:cwd")),
            };
            let reuse_ok = identity.alive
                && identity
                    .cwd
                    .as_ref()
                    .is_some_and(|cwd| crate::worktree::paths_same(cwd, &want))
                && !crate::harness::abort::should_refuse_reuse(record)
                && identity_matches_parts(&handle.to_status(), harness, model, bin);
            if reuse_ok
                && let Ok(verified) =
                    prove_holder(record, &identity, crate::harness::reap::RPC_TIMEOUT)
            {
                return Ok(Some(verified.harness));
            }
            let termination = run_holder_work({
                let rec = record.clone();
                move || terminate_persisted_holder(&rec, Intent::Reap)
            })
            .await?;
            settle_start_termination(record, termination.old.as_ref(), termination.outcome)?;
            Ok(None)
        }
    }
}

/// Publish the holder's ready record, or tear its own session down and drop the
/// registration when a newer writer owns the file. A holder never serves untracked.
///
/// A fence failure (`Unavailable`) is **not** a record change: retry the whole fenced
/// write three times, then fail the start with the named token. The ordinary
/// start-failure path owns the child; this function never tears a healthy session down
/// for contention.
async fn publish_ready_or_teardown(
    record: &ProjectRecord,
    session: &mut GrokSession,
    control_addr: Option<String>,
    stamp: Option<&PersistRef>,
) -> Result<()> {
    let mut attempt = 0u8;
    loop {
        attempt += 1;
        match write_holder_ready_outcome(record, session, control_addr.clone(), stamp) {
            PersistAct::Ok => return Ok(()),
            PersistAct::Refused => break,
            PersistAct::Unavailable(e) => {
                if attempt >= 3 {
                    return Err(e);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    let _ = session.shutdown().await;
    crate::harness::abort::unregister_cancel_handle(&record.id);
    Err(CoordinatorError::Message(format!(
        "persist-changed: another writer owns the holder record; session torn down \
         (persist: {}). {}",
        path_display(record),
        recovery_hint(record)
    )))
}

/// Table A (an address was present) for the start/reuse caller.
fn settle_start_termination(
    record: &ProjectRecord,
    old: Option<&PersistRef>,
    outcome: HolderTermination,
) -> Result<()> {
    match outcome {
        HolderTermination::Delivered { .. } => {
            let Some(old) = old else {
                return Err(holder_changed(
                    record,
                    "the persist vanished after delivery",
                ));
            };
            match wait_holder_gone(record, old, WAIT_GONE) {
                WaitGone::Gone { .. } => {
                    after_gone(record);
                    persist_act_or_refused(clear_persist_if_outcome(record, old), || {
                        holder_changed(record, "the record changed before the clear")
                    })?;
                    Ok(())
                }
                WaitGone::Superseded => Err(holder_changed(
                    record,
                    "a newer holder owns the persist after the wait",
                )),
                WaitGone::TimedOut => Err(CoordinatorError::Message(format!(
                    "delivered; the holder has not written its exit record; retry (persist: {})",
                    path_display(record)
                ))),
            }
        }
        HolderTermination::Exited { .. } => {
            let Some(old) = old else {
                return Ok(());
            };
            after_gone(record);
            persist_act_or_refused(clear_persist_if_outcome(record, old), || {
                holder_changed(record, "the record changed before the clear")
            })?;
            Ok(())
        }
        HolderTermination::NoListener { addr } => {
            // The start path is the only arm that may act on "no listener".
            let Some(old) = old else {
                return Ok(());
            };
            discard_line(record, old);
            persist_act_or_refused(clear_persist_if_outcome(record, old), || {
                holder_changed(record, "the record changed before the clear")
            })?;
            eprintln!(
                "coordinator: holder-persist-discarded: project={} reason=connect-refused addr={addr}",
                record.id
            );
            Ok(())
        }
        HolderTermination::NoSocket { why } => settle_no_address_now(record, old, why),
        HolderTermination::Unproven { why } => Err(holder_unproven(record, &why)),
    }
}

/// Table B for a *loaded* no-address record: wait out the grace, re-read and
/// re-classify, then one receipt + compare-clear.
async fn settle_start_no_address(
    record: &ProjectRecord,
    old: &PersistRef,
    why: NoSocketWhy,
) -> Result<()> {
    let mut action = no_address_decision(&why, old.mtime, SystemTime::now());
    loop {
        match action {
            NoAddressAction::DiscardNow => return discard_now(record, old),
            NoAddressAction::WaitUntil(at) => {
                let now = SystemTime::now();
                if at > now
                    && let Ok(wait) = at.duration_since(now)
                {
                    tokio::time::sleep(wait).await;
                }
                let Some((identity, mtime)) = persist_identity(record)? else {
                    return Ok(());
                };
                if !same_record(old, &identity, mtime) {
                    return Err(holder_changed(
                        record,
                        "a newer record owns the persist during the grace wait",
                    ));
                }
                match classify_persist(&identity) {
                    PersistClass::NoAddress(next) => {
                        action = no_address_decision(&next, mtime, SystemTime::now());
                    }
                    PersistClass::Addressed { .. } => {
                        return Err(holder_changed(
                            record,
                            "the record gained an address during the grace wait",
                        ));
                    }
                }
            }
            NoAddressAction::Refuse(reason) => {
                return Err(CoordinatorError::Message(format!(
                    "{reason}: another holder lifecycle owns the persist (persist: {}). Do not \
                     delete anything; retry in a few seconds. {}",
                    path_display(record),
                    recovery_hint(record)
                )));
            }
            NoAddressAction::OwnedInProcess => {
                return Err(CoordinatorError::Message(format!(
                    "alive-self: this process still owns a session record (persist: {}). {}",
                    path_display(record),
                    recovery_hint(record)
                )));
            }
        }
    }
}

fn settle_no_address_now(
    record: &ProjectRecord,
    old: Option<&PersistRef>,
    why: NoSocketWhy,
) -> Result<()> {
    match no_address_decision(
        &why,
        old.map(|o| o.mtime).unwrap_or(SystemTime::now()),
        SystemTime::now(),
    ) {
        NoAddressAction::DiscardNow => match old {
            Some(old) => discard_now(record, old),
            None => Ok(()),
        },
        NoAddressAction::Refuse(reason) => Err(CoordinatorError::Message(format!(
            "{reason}: the record is still authoritative (persist: {}). {}",
            path_display(record),
            recovery_hint(record)
        ))),
        NoAddressAction::OwnedInProcess => match old {
            // The start path already dropped its own in-process session above; what is
            // left is a stale self record: record the identity, then compare-clear.
            Some(old) => discard_now(record, old),
            None => Ok(()),
        },
        NoAddressAction::WaitUntil(_) => Err(holder_changed(
            record,
            "the record is inside its discard grace",
        )),
    }
}

fn discard_now(record: &ProjectRecord, old: &PersistRef) -> Result<()> {
    discard_line(record, old);
    persist_act_or_refused(clear_persist_if_outcome(record, old), || {
        holder_changed(record, "the record changed before the clear")
    })
}

/// Persist fields the worktree live-check and sync shutdown need.
#[derive(Debug, Clone)]
pub(crate) struct PersistLive {
    pub cwd: Option<PathBuf>,
    pub alive: bool,
    pub prompt_in_flight: bool,
}

/// `Err` is unreadable/corrupt — **not** absent. `session_bound` treats it as bound.
pub(crate) fn persist_live(record: &ProjectRecord) -> Result<Option<PersistLive>> {
    persist_live_at(record, crate::state::active_run_epoch())
}

/// Like [`persist_live`], for one epoch's session file. `Err` is unreadable.
pub(crate) fn persist_live_at(
    record: &ProjectRecord,
    epoch: Option<u64>,
) -> Result<Option<PersistLive>> {
    let Some(handle) = load_persist_at(record, epoch)? else {
        return Ok(None);
    };
    Ok(Some(PersistLive {
        cwd: handle.cwd,
        alive: handle.alive,
        prompt_in_flight: handle.prompt_in_flight,
    }))
}

/// Bounded pool-lock wait, then the owned session's synchronous teardown.
/// `false` when no session matches or the lock is not acquired — never a drop.
pub(crate) fn teardown_pooled_if_cwd(
    project_id: &str,
    pred: impl Fn(&std::path::Path) -> bool,
    budget: Duration,
) -> bool {
    let Some(pool) = POOL.get() else {
        return false;
    };
    let deadline = std::time::Instant::now() + budget;
    loop {
        match pool.try_lock() {
            Ok(mut guard) => {
                let matches = guard
                    .status_of(project_id)
                    .and_then(|status| status.cwd)
                    .is_some_and(|cwd| pred(&cwd));
                if !matches {
                    return false;
                }
                let Some(mut session) = guard.remove(project_id) else {
                    return false;
                };
                session.shutdown_blocking();
                return true;
            }
            Err(_) => {
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Drop an in-memory session whose cwd matches `pred`. Lock contention leaves it.
pub(crate) fn detach_pooled_if_cwd(project_id: &str, pred: impl Fn(&std::path::Path) -> bool) {
    let Some(pool) = POOL.get() else {
        return;
    };
    let Ok(mut guard) = pool.try_lock() else {
        return;
    };
    let matches = guard
        .status_of(project_id)
        .and_then(|status| status.cwd)
        .is_some_and(|cwd| pred(&cwd));
    if matches {
        let _ = guard.remove(project_id);
    }
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
    _project: Option<&str>,
    bin: Option<PathBuf>,
    model: Option<String>,
    harness: &str,
    epoch: Option<u64>,
) -> Result<GrokHarnessStatus> {
    // CLI / HTTP start: implementor-first resolve. Adapter ticks pass the
    // already-resolved phase bin — do not call resolve_grok_binary() there
    // (that would fail plan when implementor is broken). Cursor CLI-none
    // resolves COORDINATOR_CURSOR_BIN / PATH cursor-agent, never grok.
    if bin.is_none() {
        if harness.eq_ignore_ascii_case("cursor") {
            let _ = resolve_cursor_binary()?;
        } else {
            let _ = resolve_grok_binary()?;
        }
    }
    pre_claim(record);
    let claim = claim_start(record, harness)?;
    if let Some(stand_in) = spawn_stand_in(record) {
        return stand_in;
    }
    let mut child = match spawn_holder_process(
        record,
        bin.as_deref(),
        model.as_deref(),
        harness,
        &claim,
        epoch,
    ) {
        Ok(c) => c,
        Err(e) => {
            let _ = mark_start_error(record, Some(&claim), &e.to_string());
            return Err(e);
        }
    };

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
        match child.try_wait() {
            Ok(Some(status)) => {
                let err = CoordinatorError::Message(format!(
                    "Grok holder exited before ready ({status})"
                ));
                let _ = mark_start_error(record, Some(&claim), &err.to_string());
                return Err(err);
            }
            Ok(None) => {}
            Err(e) => {
                let err = CoordinatorError::Message(format!("failed to wait for Grok holder: {e}"));
                let _ = mark_start_error(record, Some(&claim), &err.to_string());
                return Err(err);
            }
        }
        if std::time::Instant::now() >= deadline {
            let err = CoordinatorError::Message(
                "timed out waiting for Grok holder to become ready".into(),
            );
            let _ = mark_start_error(record, Some(&claim), &err.to_string());
            return Err(err);
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
}

/// Env pairs set only on the holder child Command — never `std::env::set_var`.
pub(crate) fn holder_child_env(
    bin: &std::path::Path,
    model: Option<&str>,
    harness: &str,
) -> Vec<(String, String)> {
    let mut env = vec![(ENV_ACP_HARNESS.to_string(), harness.to_string())];
    let pin = bin.to_string_lossy().into_owned();
    if harness.eq_ignore_ascii_case("cursor") {
        env.push((ENV_CURSOR_BIN.to_string(), pin));
    } else {
        env.push((ENV_GROK_BIN.to_string(), pin));
    }
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
    harness: &str,
) {
    if let Some(bin) = bin {
        for (k, v) in holder_child_env(bin, model, harness) {
            cmd.env(k, v);
        }
    } else {
        cmd.env(ENV_ACP_HARNESS, harness);
    }
}

pub(crate) fn build_holder_command(
    record: &ProjectRecord,
    bin: Option<&std::path::Path>,
    model: Option<&str>,
    harness: &str,
    stamp: &PersistRef,
    epoch: Option<u64>,
) -> Result<std::process::Command> {
    let project_spec = record
        .path
        .to_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(record.id.as_str());
    let exe = std::env::current_exe().map_err(|e| {
        CoordinatorError::Message(format!("cannot resolve coordinator executable: {e}"))
    })?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("harness")
        .arg("grok")
        .arg("hold")
        .arg("--project")
        .arg(project_spec);
    if let Some(epoch) = epoch {
        cmd.arg("--epoch").arg(epoch.to_string());
        cmd.current_dir(crate::worktree::epoch_dir(record, epoch)?);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    apply_holder_child_env(&mut cmd, bin, model, harness);
    cmd.env(
        ENV_HOLDER_ATTEMPT_MTIME,
        stamp_nanos(&stamp.mtime).to_string(),
    );
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0000_0008 | 0x0000_0200 | 0x0800_0000);
    }
    Ok(cmd)
}

fn spawn_holder_process(
    record: &ProjectRecord,
    bin: Option<&std::path::Path>,
    model: Option<&str>,
    harness: &str,
    stamp: &PersistRef,
    epoch: Option<u64>,
) -> Result<std::process::Child> {
    build_holder_command(record, bin, model, harness, stamp, epoch)?
        .spawn()
        .map_err(|e| CoordinatorError::Message(format!("failed to spawn grok holder: {e}")))
}

fn stamp_nanos(mtime: &SystemTime) -> u128 {
    mtime
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// The stamp this holder inherited from its parent's claim, if any. A malformed or
/// absent marker is "unclaimed" (a direct `harness grok hold`).
fn inherited_attempt_stamp(record: &ProjectRecord, harness: &str) -> Option<PersistRef> {
    let raw = std::env::var(ENV_HOLDER_ATTEMPT_MTIME).ok()?;
    let nanos: u64 = raw.trim().parse().ok()?;
    let mtime = SystemTime::UNIX_EPOCH + Duration::from_nanos(nanos);
    Some(PersistRef::new(attempt_identity(record, harness), mtime))
}

pub async fn hold_loop(project: Option<&str>, epoch: Option<u64>) -> Result<()> {
    let rec = match resolve_record(project, false) {
        Ok(r) => r,
        Err(e) => return Err(e),
    };
    if let Some(epoch) = epoch {
        crate::state::set_holder_epoch(epoch)?;
        if crate::worktree::isolation_enabled(&rec).unwrap_or(false) {
            let dir = crate::worktree::epoch_dir(&rec, epoch)?;
            if !dir.is_dir() {
                return Err(CoordinatorError::Message(format!(
                    "worktree for epoch {epoch} is missing"
                )));
            }
            std::env::set_current_dir(&dir)?;
        }
    } else if matches!(
        crate::state::load_run_book(&rec)?,
        crate::state::RunBook::Many(_)
    ) {
        return Err(crate::state::ambiguous_run_error());
    }
    let cwd = grok_cwd(&rec)?;
    let model = std::env::var(ENV_GROK_MODEL)
        .ok()
        .filter(|s| !s.trim().is_empty());
    let harness = holder_harness_slug();
    // Read the parent claim's stamp once, before the ACP start.
    let stamp = inherited_attempt_stamp(&rec, &harness);
    let started = async {
        let bin = resolve_holder_binary(&harness)?;
        GrokSession::start_with_harness(
            cwd,
            prompt_timeout_for(&rec),
            bin,
            model.as_deref(),
            &harness,
        )
        .await
    }
    .await;
    let session = match started {
        Ok(s) => s,
        Err(e) => {
            match mark_start_error_outcome(&rec, stamp.as_ref(), &e.to_string()) {
                PersistAct::Ok => {}
                PersistAct::Refused => eprintln!(
                    "coordinator: holder child refused to publish its error record (persist changed)"
                ),
                // A fence failure is named, never reported as a record change.
                PersistAct::Unavailable(lock_err) => eprintln!(
                    "coordinator: holder child could not publish its error record: {lock_err}"
                ),
            }
            return Err(e);
        }
    };

    hold_accept_loop(rec, session, stamp).await
}

fn holder_harness_slug() -> String {
    std::env::var(ENV_ACP_HARNESS)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "grok".into())
}

fn resolve_holder_binary(harness: &str) -> Result<PathBuf> {
    if harness.eq_ignore_ascii_case("cursor") {
        resolve_cursor_binary()
    } else {
        resolve_grok_binary()
    }
}

/// Scripted holder for tests (mock ACP session).
#[cfg(test)]
pub async fn hold_loop_with_session(record: ProjectRecord, session: GrokSession) -> Result<()> {
    let stamp = inherited_attempt_stamp(&record, &session.adapter);
    hold_accept_loop(record, session, stamp).await
}

async fn hold_accept_loop(
    rec: ProjectRecord,
    mut session: GrokSession,
    stamp: Option<PersistRef>,
) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| CoordinatorError::Message(format!("holder bind 127.0.0.1:0 failed: {e}")))?;
    let addr = listener.local_addr()?.to_string();
    session.set_progress_record(rec.clone());
    crate::harness::abort::register_cancel_handle(rec.id.clone(), session.cancel_handle());
    publish_ready_or_teardown(&rec, &mut session, Some(addr.clone()), stamp.as_ref()).await?;

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
    match write_holder_exit_outcome(&rec, &session) {
        PersistAct::Ok => {}
        PersistAct::Refused => eprintln!(
            "coordinator: holder exit record not written: the persist no longer names this holder"
        ),
        PersistAct::Unavailable(e) => {
            eprintln!("coordinator: holder exit record not written: {e}");
        }
    }
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

/// Revalidate the frozen identity and re-resolve the target against the *same*
/// injected state. A resolution failure or a different target is a structured skip.
fn validate_prepared(
    record: &ProjectRecord,
    prepared: &PreparedInjection,
) -> std::result::Result<(), String> {
    let state = load_run_state(record).map_err(|e| e.to_string())?;
    if state.status != RunStatus::Running {
        return Err("stale-injection".into());
    }
    if !prepared.matches_state(&state) {
        return Err("stale-injection".into());
    }
    let Some(target) = prepared.target() else {
        return Err("unresolved".into());
    };
    match crate::workflow::execution_repo::resolve_track_target(record, &state, &state.phase) {
        Ok(now) if now.same_identity(target) => Ok(()),
        Ok(_) => Err("target-changed".into()),
        Err(_) => Err("unresolved".into()),
    }
}

fn prepared_skip_response(shared: &HolderShared, reason: &str) -> (HoldResponse, bool) {
    let _ = reason;
    (
        HoldResponse {
            ok: true,
            error: None,
            text: None,
            stop_reason: None,
            status: None,
            harness: Some(snapshot_status(shared)),
            applied: Some(false),
            skipped: Some(true),
            failure_class: None,
            prepared_protocol: Some(PREPARED_PROTOCOL_VERSION),
            epoch_protocol: None,
        },
        false,
    )
}

async fn handle_prepared_prompt(
    record: &ProjectRecord,
    shared: &std::sync::Arc<HolderShared>,
    text: String,
    prepared: PreparedInjection,
) -> Result<(HoldResponse, bool)> {
    if let Err(reason) = validate_prepared(record, &prepared) {
        journal_sample_skip(record, prepared.target(), &reason);
        return Ok(prepared_skip_response(shared, &reason));
    }
    let _gate = match shared.prompt_gate.try_lock() {
        Ok(gate) => gate,
        Err(_) => {
            // Gate contention is a structured skip, never a HarnessCrash.
            journal_sample_skip(record, prepared.target(), "prompt-gate-contention");
            return Ok(prepared_skip_response(shared, "prompt-gate-contention"));
        }
    };
    let state = load_run_state(record)?;
    let phase = prepared.phase.clone();
    let Some(target) = prepared.target().cloned() else {
        journal_sample_skip(record, None, "unresolved");
        return Ok(prepared_skip_response(shared, "unresolved"));
    };
    let capture = no_change::note_baseline(record, &state, &phase, &target);
    if let CaptureStatus::Skipped(reason) = &capture {
        journal_sample_skip(record, Some(&target), reason);
    }
    if matches!(capture, CaptureStatus::UntrustedTargetChange) {
        journal_sample_skip(record, Some(&target), "target-changed");
        return Ok(prepared_skip_response(shared, "target-changed"));
    }

    set_prompt_in_flight(record, true);
    let turn = {
        let mut session = shared.session.lock().await;
        // Revalidate immediately before send while the holder session lock is held.
        if let Err(reason) = validate_prepared(record, &prepared) {
            session.clear_turn_roots();
            set_prompt_in_flight(record, false);
            journal_sample_skip(record, Some(&target), &reason);
            return Ok(prepared_skip_response(shared, &reason));
        }
        session.install_turn_root(&target.prompt_path);
        if let Ok(mut snap) = shared.snapshot.lock() {
            *snap = session_status(&session);
        }
        crate::workflow::watchdog::note_progress(
            record,
            crate::workflow::watchdog::ProgressKind::Inject,
            Some(&session.session_id),
            false,
        );
        let turn = session
            .inject_prompt(&text, prompt_timeout_for(record))
            .await;
        session.clear_turn_roots();
        turn
    };
    let normal = matches!(&turn, Ok(pr) if !crate::harness::abort::stop_reason_is_cancelled(pr.stop_reason.as_deref()));
    if normal {
        set_prompt_in_flight(record, false);
    } else if let Err(e) = &turn {
        let msg = e.to_string().to_ascii_lowercase();
        if msg.contains("timed out") || msg.contains("timeout") {
            crate::harness::abort::abort_stuck_prompt_sync(
                record,
                crate::harness::abort::AbortReason::PromptTimeout,
                crate::state::active_run_epoch().unwrap_or(0),
            );
        }
    }
    let (tally, verdict) = {
        let session = shared.session.lock().await;
        if let Ok(mut snap) = shared.snapshot.lock() {
            *snap = session_status(&session);
        }
        remember_fidelity(record, &session);
        let verdict = match revalidate_target(record, &state, &phase, &target) {
            Ok(()) => no_change::evaluate_turn(
                record,
                &state,
                &phase,
                &target,
                &capture.trust(),
                &session.adapter,
                session.last_signal.as_deref(),
                &session.cwd,
            ),
            Err(reason) => {
                journal_sample_skip(record, Some(&target), &reason);
                VerdictOutcome::Clean
            }
        };
        (session.terminal_spawn_tally(), verdict)
    };
    let no_change = match verdict {
        VerdictOutcome::NoChange(message) => Some(message),
        VerdictOutcome::CwdMisdirected => {
            journal_sample_skip(record, Some(&target), "cwd-misdirected");
            None
        }
        VerdictOutcome::TargetChanged(reason) => {
            journal_sample_skip(record, Some(&target), &reason);
            None
        }
        VerdictOutcome::StaleInjection => {
            journal_sample_skip(record, Some(&target), "stale-injection");
            None
        }
        VerdictOutcome::Clean => None,
    };
    let view = apply_turn_checked(
        record,
        turn,
        snapshot_status(shared),
        &phase,
        tally,
        no_change,
        Some(&prepared),
        None,
        false,
    )
    .await?;
    Ok((view_to_hold(view), false))
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
                prepared_protocol: Some(PREPARED_PROTOCOL_VERSION),
                epoch_protocol: Some(EPOCH_PROTOCOL_VERSION),
            },
            false,
        ),
        HoldRequest::AdoptEpoch { epoch } => {
            let resp = match crate::state::set_holder_epoch(epoch) {
                Ok(()) => HoldResponse {
                    ok: true,
                    error: None,
                    text: None,
                    stop_reason: None,
                    status: None,
                    harness: None,
                    applied: None,
                    skipped: None,
                    failure_class: None,
                    prepared_protocol: None,
                    epoch_protocol: Some(EPOCH_PROTOCOL_VERSION),
                },
                Err(err) => HoldResponse {
                    ok: false,
                    error: Some(err.to_string()),
                    text: None,
                    stop_reason: None,
                    status: None,
                    harness: None,
                    applied: None,
                    skipped: None,
                    failure_class: None,
                    prepared_protocol: None,
                    epoch_protocol: Some(EPOCH_PROTOCOL_VERSION),
                },
            };
            (resp, false)
        }
        HoldRequest::ReleaseEpoch => {
            crate::state::release_holder_epoch();
            (
                HoldResponse {
                    ok: true,
                    error: None,
                    text: None,
                    stop_reason: None,
                    status: None,
                    harness: None,
                    applied: None,
                    skipped: None,
                    failure_class: None,
                    prepared_protocol: None,
                    epoch_protocol: Some(EPOCH_PROTOCOL_VERSION),
                },
                false,
            )
        }
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
                    prepared_protocol: None,
                    epoch_protocol: None,
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
                    prepared_protocol: None,
                    epoch_protocol: None,
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
                        prepared_protocol: None,
                        epoch_protocol: None,
                    },
                    false,
                ),
                Ok(_gate) => {
                    let state = load_run_state(record)?;
                    let injected_phase = state.phase.clone();
                    set_prompt_in_flight(record, true);
                    let (turn, block_identity) = {
                        let mut session = shared.session.lock().await;
                        // Legacy/manual prompts never inherit a prepared turn's ACP roots.
                        session.clear_turn_roots();
                        if let Ok(mut snap) = shared.snapshot.lock() {
                            *snap = session_status(&session);
                        }
                        crate::workflow::watchdog::note_progress(
                            record,
                            crate::workflow::watchdog::ProgressKind::Inject,
                            Some(&session.session_id),
                            false,
                        );
                        // 0098: capture the block-only identity immediately before the send,
                        // so a long turn that moves epoch or phase is seen as stale.
                        let block_identity = load_run_state(record)
                            .ok()
                            .filter(crate::workflow::block::capture_allowed)
                            .map(|s| PreparedInjection::legacy(&s));
                        // Legacy Prompt is the manual CLI/HTTP and prompt-only phase seam:
                        // it never resolves a target, captures a baseline or emits a verdict.
                        let turn = session
                            .inject_prompt(&text, prompt_timeout_for(record))
                            .await;
                        (turn, block_identity)
                    };
                    let normal = matches!(&turn, Ok(pr) if !crate::harness::abort::stop_reason_is_cancelled(pr.stop_reason.as_deref()));
                    if normal {
                        set_prompt_in_flight(record, false);
                    }
                    // Err leaves prompt_in_flight set so should_refuse_reuse starts a new session.
                    if let Err(e) = &turn {
                        let msg = e.to_string().to_ascii_lowercase();
                        if msg.contains("timed out") || msg.contains("timeout") {
                            crate::harness::abort::abort_stuck_prompt_sync(
                                record,
                                crate::harness::abort::AbortReason::PromptTimeout,
                                crate::state::active_run_epoch().unwrap_or(0),
                            );
                        }
                    }
                    let tally = {
                        let session = shared.session.lock().await;
                        if let Ok(mut snap) = shared.snapshot.lock() {
                            *snap = session_status(&session);
                        }
                        remember_fidelity(record, &session);
                        session.terminal_spawn_tally()
                    };
                    let view = apply_turn_checked(
                        record,
                        turn,
                        snapshot_status(&shared),
                        &injected_phase,
                        tally,
                        None,
                        None,
                        block_identity.as_ref(),
                        crate::workflow::execution_repo::prepares_target(&injected_phase),
                    )
                    .await?;
                    (view_to_hold(view), false)
                }
            },
        },
        HoldRequest::PreparedPrompt { text, prepared } => {
            handle_prepared_prompt(record, &shared, text, *prepared).await?
        }
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
                        prepared_protocol: None,
                        epoch_protocol: None,
                    },
                    false,
                )
            } else {
                drop(session);
                let mut session = shared.session.lock().await;
                match session.compact(prompt_timeout_for(record)).await {
                    Ok(pr) => {
                        if let Ok(mut snap) = shared.snapshot.lock() {
                            *snap = session_status(&session);
                        }
                        remember_fidelity(record, &session);
                        (
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
                                prepared_protocol: None,
                                epoch_protocol: None,
                            },
                            false,
                        )
                    }
                    Err(e) => {
                        if let Ok(mut snap) = shared.snapshot.lock() {
                            *snap = session_status(&session);
                        }
                        remember_fidelity(record, &session);
                        (
                            HoldResponse {
                                ok: false,
                                error: Some(e.to_string()),
                                text: None,
                                stop_reason: None,
                                status: None,
                                harness: Some(session_status(&session)),
                                applied: Some(false),
                                skipped: None,
                                failure_class: Some(failure_class_for_message(&e.to_string())),
                                prepared_protocol: None,
                                epoch_protocol: None,
                            },
                            false,
                        )
                    }
                }
            }
        }
        HoldRequest::Shutdown => {
            if let Some(h) = crate::harness::abort::cancel_handle_for(&record.id) {
                let _ = h.cancel().await;
            }
            let _ = shared.shutdown_tx.send(true);
            let mut snap = snapshot_status(&shared);
            snap.alive = false;
            (
                HoldResponse {
                    ok: true,
                    error: None,
                    text: None,
                    stop_reason: None,
                    status: None,
                    harness: Some(snap),
                    applied: None,
                    skipped: None,
                    failure_class: None,
                    prepared_protocol: None,
                    epoch_protocol: None,
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
        adapter: session.adapter.clone(),
        context_usage: session.context_usage.clone(),
        last_signal: session.last_signal.clone(),
        last_tool_title: session.last_tool_title.clone(),
        model: session.model.clone(),
        bin: Some(session.bin.clone()),
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
        prepared_protocol: Some(PREPARED_PROTOCOL_VERSION),
        epoch_protocol: None,
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
    if !resp.ok && resp.applied != Some(true) && resp.skipped != Some(true) {
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

pub async fn prompt(
    project: Option<&str>,
    text: String,
    infer_cwd: bool,
) -> Result<HarnessPromptView> {
    prompt_impl(project, text, infer_cwd, None).await
}

/// Adapter-only prepared entry point. Legacy `prompt` never resolves a target,
/// captures a baseline or emits a verdict.
pub async fn prompt_prepared(
    project: Option<&str>,
    text: String,
    infer_cwd: bool,
    prepared: PreparedInjection,
) -> Result<HarnessPromptView> {
    prompt_impl(project, text, infer_cwd, Some(prepared)).await
}

async fn prompt_impl(
    project: Option<&str>,
    text: String,
    infer_cwd: bool,
    prepared: Option<PreparedInjection>,
) -> Result<HarnessPromptView> {
    let rec = resolve_record(project, infer_cwd)?;
    refuse_if_paused(&rec)?;
    if let Some(prepared) = &prepared
        && let Err(reason) = validate_prepared(&rec, prepared)
    {
        journal_sample_skip(&rec, prepared.target(), &reason);
        return Ok(prepared_skipped_view(
            &rec,
            current_status(&rec).await,
            prepared,
            &reason,
        ));
    }
    if let Some(h) = load_persist(&rec)?
        && h.alive
        && let Some(addr) = &h.control_addr
        && !global_pool().lock().await.contains(&rec.id)
    {
        if let Some(prepared) = prepared.clone() {
            return prompt_prepared_holder(&rec, text, prepared, addr).await;
        }
        let resp = holder_rpc(addr, &HoldRequest::Prompt { text }).await?;
        return hold_view(resp);
    }

    let state = load_run_state(&rec)?;
    let injected_phase = state.phase.clone();
    let target = prepared.as_ref().and_then(|p| p.target()).cloned();
    let capture = target
        .as_ref()
        .map(|target| no_change::note_baseline(&rec, &state, &injected_phase, target));
    if let Some(CaptureStatus::Skipped(reason)) = &capture {
        journal_sample_skip(&rec, target.as_ref(), reason);
    }
    if matches!(capture, Some(CaptureStatus::UntrustedTargetChange)) {
        journal_sample_skip(&rec, target.as_ref(), "target-changed");
        let prepared = prepared.expect("target change requires a prepared injection");
        return Ok(prepared_skipped_view(
            &rec,
            current_status(&rec).await,
            &prepared,
            "target-changed",
        ));
    }

    set_prompt_in_flight(&rec, true);
    let (turn, block_identity) = {
        let mut pool = global_pool().lock().await;
        let session = pool.get_mut(&rec.id).ok_or_else(|| {
            CoordinatorError::Message(
                "no Grok session; run `coordinator harness grok start` first".into(),
            )
        })?;
        if let Some(prepared) = &prepared
            && let Err(reason) = validate_prepared(&rec, prepared)
        {
            set_prompt_in_flight(&rec, false);
            journal_sample_skip(&rec, target.as_ref(), &reason);
            return Ok(prepared_skipped_view(
                &rec,
                current_status(&rec).await,
                prepared,
                &reason,
            ));
        }
        if let Some(target) = &target {
            session.install_turn_root(&target.prompt_path);
        } else {
            session.clear_turn_roots();
        }
        crate::workflow::watchdog::note_progress(
            &rec,
            crate::workflow::watchdog::ProgressKind::Inject,
            Some(&session.session_id),
            false,
        );
        // 0098: block-only identity captured at inject start. Only an Adapter run in a
        // six-phase, Running state may classify a block.
        let block_identity = load_run_state(&rec)
            .ok()
            .filter(crate::workflow::block::capture_allowed)
            .map(|s| PreparedInjection::legacy(&s));
        let turn = session.inject_prompt(&text, prompt_timeout_for(&rec)).await;
        session.clear_turn_roots();
        (turn, block_identity)
    };
    let normal = matches!(&turn, Ok(pr) if !crate::harness::abort::stop_reason_is_cancelled(pr.stop_reason.as_deref()));
    if normal {
        set_prompt_in_flight(&rec, false);
    } else if let Err(e) = &turn {
        // Err leaves prompt_in_flight set so should_refuse_reuse starts a new session.
        let msg = e.to_string().to_ascii_lowercase();
        if msg.contains("timed out") || msg.contains("timeout") {
            crate::harness::abort::abort_stuck_prompt_sync(
                &rec,
                crate::harness::abort::AbortReason::PromptTimeout,
                crate::state::active_run_epoch().unwrap_or(0),
            );
        }
    }
    {
        let mut pool = global_pool().lock().await;
        if let Some(session) = pool.get_mut(&rec.id) {
            remember_fidelity(&rec, session);
        }
    }
    let harness = current_status(&rec).await;
    let (tally, no_change) = {
        let mut pool = global_pool().lock().await;
        match pool.get_mut(&rec.id) {
            Some(s) => {
                let no_change = match (&capture, &target) {
                    (Some(capture), Some(target)) => {
                        if let Err(reason) =
                            revalidate_target(&rec, &state, &injected_phase, target)
                        {
                            journal_sample_skip(&rec, Some(target), &reason);
                            None
                        } else {
                            match no_change::evaluate_turn(
                                &rec,
                                &state,
                                &injected_phase,
                                target,
                                &capture.trust(),
                                &s.adapter,
                                s.last_signal.as_deref(),
                                &s.cwd,
                            ) {
                                VerdictOutcome::NoChange(message) => Some(message),
                                VerdictOutcome::CwdMisdirected => {
                                    journal_sample_skip(&rec, Some(target), "cwd-misdirected");
                                    None
                                }
                                VerdictOutcome::TargetChanged(reason) => {
                                    journal_sample_skip(&rec, Some(target), &reason);
                                    None
                                }
                                VerdictOutcome::StaleInjection => {
                                    journal_sample_skip(&rec, Some(target), "stale-injection");
                                    None
                                }
                                VerdictOutcome::Clean => None,
                            }
                        }
                    }
                    _ => None,
                };
                (s.terminal_spawn_tally(), no_change)
            }
            None => (SpawnTally::default(), None),
        }
    };
    apply_turn_checked(
        &rec,
        turn,
        harness,
        &injected_phase,
        tally,
        no_change,
        prepared.as_ref(),
        block_identity.as_ref(),
        prepared.is_none() && crate::workflow::execution_repo::prepares_target(&injected_phase),
    )
    .await
}

/// Detached-holder prepared turn. Missing capability recycles the owned holder once;
/// a post-send transport failure is applied once and never retried.
async fn prompt_prepared_holder(
    rec: &ProjectRecord,
    text: String,
    prepared: PreparedInjection,
    addr: &str,
) -> Result<HarnessPromptView> {
    let capability = holder_rpc(addr, &HoldRequest::Ping)
        .await
        .ok()
        .and_then(|resp| resp.prepared_protocol)
        .unwrap_or(0);
    if prepared_protocol_supported(Some(capability)) {
        return send_prepared_rpc(rec, text, prepared, addr).await;
    }

    // Outside pool/session locks: shut down the owned old holder and respawn with
    // the current binary. At most one compatibility recycle per dispatch.
    if let Err(e) = recycle_without_pool_lock(rec).await {
        return prepared_failure_after_recycle(
            rec,
            prepared,
            &format!("old holder shutdown failed: {e}"),
        );
    }
    let binding = match crate::harness::roles::resolve_phase_binding(&prepared.phase) {
        Ok(binding) => binding,
        Err(e) => {
            return prepared_failure_after_recycle(
                rec,
                prepared,
                &format!("replacement holder binding failed: {e}"),
            );
        }
    };
    let bin = match crate::harness::roles::resolve_phase_binary(&prepared.phase) {
        Ok(bin) => bin,
        Err(e) => {
            return prepared_failure_after_recycle(
                rec,
                prepared,
                &format!("replacement holder binary failed: {e}"),
            );
        }
    };
    let selector = rec.path.to_string_lossy().to_string();
    if let Err(e) = crate::harness::start_with_bin(
        Some(&selector),
        false,
        bin,
        binding.model.clone(),
        false,
        &binding.harness,
        crate::state::active_run_epoch(),
    )
    .await
    {
        return prepared_failure_after_recycle(
            rec,
            prepared,
            &format!("replacement holder start failed: {e}"),
        );
    }
    let Some(handle) = load_persist(rec)? else {
        return prepared_failure_after_recycle(rec, prepared, "replacement holder did not start");
    };
    let Some(new_addr) = handle.control_addr.as_deref() else {
        return prepared_failure_after_recycle(rec, prepared, "replacement holder has no address");
    };
    let second = holder_rpc(new_addr, &HoldRequest::Ping)
        .await
        .ok()
        .and_then(|resp| resp.prepared_protocol)
        .unwrap_or(0);
    if prepared_capability_plan(Some(second), true) == PreparedCapabilityPlan::Fail {
        return prepared_failure_after_recycle(
            rec,
            prepared,
            "replacement holder does not support prepared prompts",
        );
    }
    send_prepared_rpc(rec, text, prepared, new_addr).await
}

/// A holder that applied (or structurally skipped) the turn has a real view even
/// when `ok` is false because the applied outcome is a failure. Only a response
/// with no disposition is a rejected opcode/uncertain delivery.
fn structured_prepared_response(resp: &HoldResponse) -> bool {
    resp.ok || resp.applied.is_some() || resp.skipped.is_some()
}

async fn send_prepared_rpc(
    rec: &ProjectRecord,
    text: String,
    prepared: PreparedInjection,
    addr: &str,
) -> Result<HarnessPromptView> {
    match holder_rpc(
        addr,
        &HoldRequest::PreparedPrompt {
            text,
            prepared: Box::new(prepared.clone()),
        },
    )
    .await
    {
        Ok(resp) if structured_prepared_response(&resp) => hold_view(resp),
        Ok(resp) => {
            // A rejected prepared opcode is uncertain delivery: refuse holder reuse
            // and never fall back to a raw Prompt or resend.
            let _ = recycle_without_pool_lock(rec).await;
            prepared_failure_after_recycle(
                rec,
                prepared,
                &format!(
                    "holder rejected the prepared prompt ({})",
                    resp.error.unwrap_or_else(|| "no error text".into())
                ),
            )
        }
        Err(e) => {
            let _ = recycle_without_pool_lock(rec).await;
            prepared_failure_after_recycle(
                rec,
                prepared,
                &format!("prepared prompt transport failed after send ({e}); no automatic retry"),
            )
        }
    }
}

/// One explicit HarnessCrash for the injected identity, zero prompts.
fn prepared_failure_after_recycle(
    rec: &ProjectRecord,
    prepared: PreparedInjection,
    detail: &str,
) -> Result<HarnessPromptView> {
    let message = format!(
        "prepared prompt unavailable: {detail}. Run `coordinator harness grok shutdown` \
         (if it reports unproven: {} — persist: {}), run `coordinator failure resolve`, then \
         `coordinator run --track {}`.",
        crate::harness::pool::recovery_hint(rec),
        crate::harness::pool::persist_path(rec)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "{state_dir}/harness-grok.json".into()),
        prepared.track_id.as_deref().unwrap_or("(none)")
    );
    let outcome = PhaseOutcome::failure(
        prepared.phase.clone(),
        FailureClass::HarnessCrash,
        OutcomeSource::Adapter,
        Some(message.clone()),
        Some(prepared.run_epoch),
    );
    match write_and_apply_prepared(rec, outcome, &prepared)? {
        ApplyIdentity::Applied(view) => Ok(HarnessPromptView {
            text: None,
            stop_reason: None,
            applied: true,
            skipped: None,
            error: Some(message),
            failure_class: Some(FailureClass::HarnessCrash),
            status: Some(view),
            harness: None,
        }),
        ApplyIdentity::Stale(reason) => {
            journal_sample_skip(rec, prepared.target(), &reason);
            Ok(prepared_skipped_view(
                rec,
                GrokHarnessStatus::missing(),
                &prepared,
                &reason,
            ))
        }
    }
}

pub async fn compact(project: Option<&str>, infer_cwd: bool) -> Result<HarnessPromptView> {
    let rec = resolve_record(project, infer_cwd)?;
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
        Ok(pr) => {
            remember_fidelity(&rec, session);
            Ok(HarnessPromptView {
                text: Some(pr.text),
                stop_reason: pr.stop_reason,
                applied: false,
                skipped: None,
                error: None,
                failure_class: None,
                status: None,
                harness: Some(session_status(session)),
            })
        }
        Err(e) => {
            remember_fidelity(&rec, session);
            Ok(HarnessPromptView {
                text: None,
                stop_reason: None,
                applied: false,
                skipped: None,
                error: Some(e.to_string()),
                failure_class: Some(failure_class_for_message(&e.to_string())),
                status: None,
                harness: Some(session_status(session)),
            })
        }
    }
}

pub async fn status(project: Option<&str>, infer_cwd: bool) -> Result<GrokHarnessStatus> {
    let rec = resolve_record(project, infer_cwd)?;
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

/// The 0110/0027/0035 recycle. **Classifies the persist before any side effect**, so
/// its addressed-holder branch is pool- and cancel-neutral, and an unreadable persist
/// is its own refusal before any effect.
pub(crate) async fn recycle_without_pool_lock(record: &ProjectRecord) -> Result<()> {
    let unreadable = |e: CoordinatorError| {
        CoordinatorError::Message(format!(
            "persist-unreadable: {e} (persist: {}). {}",
            path_display(record),
            recovery_hint(record)
        ))
    };
    let Some((_handle, old)) = load_ref(record).map_err(unreadable)? else {
        return recycle_pool_only(record).await;
    };
    let identity = old.identity.clone();
    match classify_persist(&identity) {
        PersistClass::NoAddress(why) => match why {
            // The record is already dead and this caller has nothing to dispose; the
            // following start applies the one grace rule.
            NoSocketWhy::DeadAfterExit | NoSocketWhy::FailedStart => Ok(()),
            NoSocketWhy::PendingStart => Err(CoordinatorError::Message(format!(
                "start-pending: a holder start is in progress (persist: {}). Do not delete \
                 anything; retry in a few seconds.",
                path_display(record)
            ))),
            NoSocketWhy::AliveSelfNoAddress => recycle_in_process(record, &old).await,
            NoSocketWhy::AliveForeignNoAddress | NoSocketWhy::AliveUnknownNoAddress => {
                Err(holder_unproven(record, why_token(why)))
            }
        },
        PersistClass::Addressed { .. } => {
            let termination = run_holder_work({
                let rec = record.clone();
                move || terminate_persisted_holder(&rec, Intent::Reap)
            })
            .await?;
            let outcome = termination.outcome;
            let Some(old) = termination.old else {
                return Ok(());
            };
            match outcome {
                HolderTermination::Delivered { .. } => {
                    match wait_holder_gone(record, &old, WAIT_GONE) {
                        WaitGone::Gone { .. } => {
                            after_gone(record);
                            // `false` is a no-op verdict here: the write is this branch's
                            // only effect, and a benign supersession must not surface a
                            // spurious HarnessCrash at the prepared-prompt caller.
                            let _ = mark_dead_if(record, &old);
                            Ok(())
                        }
                        WaitGone::Superseded => Ok(()),
                        WaitGone::TimedOut => Err(CoordinatorError::Message(format!(
                            "delivered; the holder has not written its exit record; retry (persist: {})",
                            path_display(record)
                        ))),
                    }
                }
                // Already truthful; this caller disposes of nothing.
                HolderTermination::Exited { .. } => Ok(()),
                HolderTermination::NoListener { addr } => Err(CoordinatorError::Message(format!(
                    "no listener at {addr}: the holder control socket accepted no connection. \
                     This is not evidence the holder exited; the start path is the only arm \
                     that may clear it (persist: {}). {}",
                    path_display(record),
                    recovery_hint(record)
                ))),
                HolderTermination::NoSocket { why } => Err(holder_unproven(record, why_token(why))),
                HolderTermination::Unproven { why } => Err(holder_unproven(record, &why)),
            }
        }
    }
}

/// No persist at all: pool-only, teardown **then** unregister, no persist write.
async fn recycle_pool_only(record: &ProjectRecord) -> Result<()> {
    let deadline = std::time::Instant::now() + POOL_LOCK_BUDGET;
    loop {
        match global_pool().try_lock() {
            Ok(mut pool) => {
                if let Some(mut session) = pool.remove(&record.id) {
                    let _ = session.shutdown().await;
                }
                crate::harness::abort::unregister_cancel_handle(&record.id);
                return Ok(());
            }
            Err(_) => {
                if std::time::Instant::now() >= deadline {
                    return Err(CoordinatorError::Message(format!(
                        "pool lock busy: the live session was not torn down and its cancel \
                         registration is kept (persist: {}). Retry.",
                        path_display(record)
                    )));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

/// `AliveSelf`: not a holder. Tear the owned pool session down, **then** unregister,
/// then the last compare-and-write. A false compare is a refusal.
async fn recycle_in_process(record: &ProjectRecord, old: &PersistRef) -> Result<()> {
    let deadline = std::time::Instant::now() + POOL_LOCK_BUDGET;
    let mut owned = false;
    loop {
        match global_pool().try_lock() {
            Ok(mut pool) => {
                if let Some(mut session) = pool.remove(&record.id) {
                    let _ = session.shutdown().await;
                    owned = true;
                }
                break;
            }
            Err(_) => {
                if std::time::Instant::now() >= deadline {
                    return Err(CoordinatorError::Message(format!(
                        "pool lock busy: the owned session is still running and its cancel \
                         registration is kept; the persist stays honest (persist: {}). Retry.",
                        path_display(record)
                    )));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    crate::harness::abort::unregister_cancel_handle(&record.id);
    after_gone(record);
    if owned {
        persist_act_or_refused(mark_dead_if_outcome(record, old), || {
            holder_changed(record, "the record changed before the dead write")
        })?;
        return Ok(());
    }
    persist_act_or_refused(clear_persist_if_outcome(record, old), || {
        holder_changed(record, "the record changed before the clear")
    })?;
    let holder = old
        .identity
        .holder_pid
        .map(|pid| pid.to_string())
        .unwrap_or_else(|| "-".into());
    crate::progress_log::append(
        record,
        "holder-reap",
        &format!("pid={holder} reason=stale-self cleared"),
    );
    Ok(())
}

pub async fn shutdown(project: Option<&str>, infer_cwd: bool) -> Result<GrokHarnessStatus> {
    let rec = resolve_record(project, infer_cwd)?;
    if let Some(mut session) = global_pool().lock().await.remove(&rec.id) {
        crate::harness::abort::unregister_cancel_handle(&rec.id);
        let _ = session.shutdown().await;
        // Test-only in-process exception (DoD-6): derived from the session just torn
        // down, no persist compare; CLI and HTTP start detached holders.
        let dead = PersistedGrokHandle {
            version: 1,
            model: session.model.clone(),
            bin: Some(session.bin.clone()),
            project_id: rec.id.clone(),
            session_id: Some(session.session_id.clone()),
            cwd: Some(session.cwd.clone()),
            pid: session.pid,
            holder_pid: None,
            control_addr: None,
            adapter: session.adapter.clone(),
            supports_compact: session.supports_compact,
            alive: false,
            prompt_in_flight: false,
            error: None,
            context_usage: session.context_usage.clone(),
            last_signal: session.last_signal.clone(),
            last_tool_title: session.last_tool_title.clone(),
        };
        match save_in_process_shutdown(&rec, &dead) {
            Ok(()) => return Ok(dead.to_status()),
            Err(e) => {
                // Nothing was written: report the truthful on-disk status, never the
                // in-memory dead record.
                eprintln!("coordinator: shutdown: holder persist write skipped: {e}");
                return Ok(current_persist_status(&rec));
            }
        }
    }

    let termination = run_holder_work({
        let rec = rec.clone();
        move || terminate_persisted_holder(&rec, Intent::Operator)
    })
    .await?;
    let outcome = termination.outcome;
    let Some(old) = termination.old else {
        return Ok(GrokHarnessStatus::missing());
    };
    match outcome {
        HolderTermination::Delivered { .. } => match wait_holder_gone(&rec, &old, WAIT_GONE) {
            WaitGone::Gone { .. } => {
                after_gone(&rec);
                persist_act_or_refused(mark_dead_if_outcome(&rec, &old), || {
                    holder_changed(&rec, "the record changed before the dead write")
                })?;
                Ok(current_persist_status(&rec))
            }
            WaitGone::Superseded => Err(holder_changed(
                &rec,
                "a newer holder owns the persist after the wait",
            )),
            WaitGone::TimedOut => Err(CoordinatorError::Message(format!(
                "delivered; the holder has not written its exit record; retry (persist: {})",
                path_display(&rec)
            ))),
        },
        HolderTermination::Exited { .. } => {
            after_gone(&rec);
            persist_act_or_refused(mark_dead_if_outcome(&rec, &old), || {
                holder_changed(&rec, "the record changed before the dead write")
            })?;
            Ok(current_persist_status(&rec))
        }
        HolderTermination::NoListener { addr } => Err(CoordinatorError::Message(format!(
            "no listener at {addr}: the holder control socket accepted no connection. This is \
             not evidence the holder exited. Retry the start path, which may clear it; if that \
             still fails: {} (persist: {}).",
            recovery_hint(&rec),
            path_display(&rec)
        ))),
        HolderTermination::NoSocket { why } => match why {
            // Already truthful; no write.
            NoSocketWhy::DeadAfterExit | NoSocketWhy::FailedStart => {
                Ok(current_persist_status(&rec))
            }
            NoSocketWhy::PendingStart => Err(CoordinatorError::Message(format!(
                "start-in-progress: a holder start is in progress (persist: {}). Do not delete \
                 anything; retry in a few seconds.",
                path_display(&rec)
            ))),
            NoSocketWhy::AliveSelfNoAddress => {
                after_gone(&rec);
                persist_act_or_refused(clear_persist_if_outcome(&rec, &old), || {
                    holder_changed(&rec, "the record changed before the clear")
                })?;
                Ok(GrokHarnessStatus::missing())
            }
            NoSocketWhy::AliveForeignNoAddress | NoSocketWhy::AliveUnknownNoAddress => {
                Err(holder_unproven(&rec, why_token(why)))
            }
        },
        HolderTermination::Unproven { why } => Err(holder_unproven(&rec, &why)),
    }
}

fn current_persist_status(record: &ProjectRecord) -> GrokHarnessStatus {
    load_persist(record)
        .ok()
        .flatten()
        .map(|handle| handle.to_status())
        .unwrap_or_else(GrokHarnessStatus::missing)
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
        adapter: h.adapter.clone(),
        supports_compact: h.supports_compact,
        alive: false,
        prompt_in_flight: false,
        model: h.model.clone(),
        bin: h.bin.clone(),
        error: None,
        context_usage: h.context_usage.clone(),
        last_signal: h.last_signal.clone(),
        last_tool_title: h.last_tool_title.clone(),
    }
}

/// Insert a mock session (tests).
pub async fn insert_test_session(project_id: String, session: GrokSession) {
    crate::harness::abort::register_cancel_handle(project_id.clone(), session.cancel_handle());
    global_pool().lock().await.insert(project_id, session);
}

#[cfg(test)]
#[allow(clippy::await_holding_lock)]
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
            std::env::remove_var(ENV_ACP_HARNESS);
            std::env::remove_var(ENV_CURSOR_BIN);
        }
        let env = holder_child_env(
            std::path::Path::new(r"C:\phase\grok.exe"),
            Some("grok-build"),
            "grok",
        );
        assert!(
            env.iter().any(|(k, v)| k == ENV_ACP_HARNESS && v == "grok"),
            "env={env:?}"
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == ENV_GROK_BIN && v == r"C:\phase\grok.exe"),
            "env={env:?}"
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == ENV_GROK_MODEL && v == "grok-build"),
            "env={env:?}"
        );
        assert!(
            env.iter().all(|(k, _)| k != ENV_CURSOR_BIN),
            "grok holder must not pin COORDINATOR_CURSOR_BIN: {env:?}"
        );
        assert!(
            std::env::var(ENV_GROK_BIN).is_err(),
            "must not set_var COORDINATOR_GROK_BIN on the parent"
        );
        assert!(
            std::env::var(ENV_GROK_MODEL).is_err(),
            "must not set_var COORDINATOR_GROK_MODEL on the parent"
        );
        assert!(
            std::env::var(ENV_ACP_HARNESS).is_err(),
            "must not set_var COORDINATOR_ACP_HARNESS on the parent"
        );
    }

    #[test]
    fn holder_child_env_cursor_pins_cursor_bin_not_grok() {
        let _guard = test_env_lock();
        unsafe {
            std::env::remove_var(ENV_GROK_BIN);
            std::env::remove_var(ENV_CURSOR_BIN);
            std::env::remove_var(ENV_ACP_HARNESS);
            std::env::remove_var(ENV_GROK_MODEL);
        }
        let env = holder_child_env(
            std::path::Path::new(r"C:\phase\cursor-agent.cmd"),
            None,
            "cursor",
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == ENV_ACP_HARNESS && v == "cursor"),
            "env={env:?}"
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == ENV_CURSOR_BIN && v == r"C:\phase\cursor-agent.cmd"),
            "env={env:?}"
        );
        assert!(
            env.iter().all(|(k, _)| k != ENV_GROK_BIN),
            "cursor holder must not pin COORDINATOR_GROK_BIN: {env:?}"
        );
        assert!(
            std::env::var(ENV_CURSOR_BIN).is_err(),
            "must not set_var COORDINATOR_CURSOR_BIN on the parent"
        );
        assert!(
            std::env::var(ENV_ACP_HARNESS).is_err(),
            "must not set_var COORDINATOR_ACP_HARNESS on the parent"
        );
        assert!(
            std::env::var(ENV_GROK_BIN).is_err(),
            "must not set_var COORDINATOR_GROK_BIN on the parent"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn holder_cursor_env_spawn_path_is_yolo_trust_acp() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        let stub = home.path().join("cursor-holder.cmd");
        std::fs::write(&stub, "@echo off\r\n>\"%~dp0argv.txt\" echo %*\r\n").unwrap();
        let prev_home = std::env::var_os(ENV_COORDINATOR_HOME);
        let prev_harness = std::env::var_os(ENV_ACP_HARNESS);
        let prev_cursor = std::env::var_os(ENV_CURSOR_BIN);
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(ENV_ACP_HARNESS, "cursor");
            std::env::set_var(ENV_CURSOR_BIN, &stub);
            std::env::remove_var(ENV_GROK_BIN);
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();

        let err = hold_loop(Some(&rec.id), None).await.unwrap_err();
        let _ = err;
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var(ENV_COORDINATOR_HOME, v),
                None => std::env::remove_var(ENV_COORDINATOR_HOME),
            }
            match prev_harness {
                Some(v) => std::env::set_var(ENV_ACP_HARNESS, v),
                None => std::env::remove_var(ENV_ACP_HARNESS),
            }
            match prev_cursor {
                Some(v) => std::env::set_var(ENV_CURSOR_BIN, v),
                None => std::env::remove_var(ENV_CURSOR_BIN),
            }
        }
        let text = std::fs::read_to_string(home.path().join("argv.txt"))
            .unwrap_or_else(|e| panic!("argv.txt missing after hold_loop spawn path: {e}"));
        assert!(text.contains("--yolo"), "argv={text:?}");
        assert!(text.contains("--trust"), "argv={text:?}");
        assert!(text.contains("acp"), "argv={text:?}");
        assert!(!text.contains("agent"), "argv={text:?}");
        assert!(!text.contains("stdio"), "argv={text:?}");
        let persist = std::fs::read_to_string(persist_path(&rec).unwrap()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&persist).unwrap();
        assert_eq!(v["adapter"], "cursor");
        assert_eq!(v["alive"], false);
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
            crate::harness::grok_cwd(&rec).expect("cwd"),
            lines,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        insert_test_session(rec.id.clone(), session).await;

        let view = prompt(Some(&rec.id), "hello".into(), false).await.unwrap();
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
            crate::harness::grok_cwd(&rec).expect("cwd"),
            lines,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        insert_test_session(rec.id.clone(), session).await;

        let stopped = run::stop(&rec).unwrap();
        assert_eq!(stopped.phase, STUB_PHASE_STOPPED);
        let st = status(Some(&rec.id), false).await.unwrap();
        assert!(st.alive, "stop must leave Grok session in the pool");
        assert_eq!(st.session_id.as_deref(), Some("sess-alive"));

        let down = shutdown(Some(&rec.id), false).await.unwrap();
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
            crate::harness::grok_cwd(&rec).expect("cwd"),
            mock_handshake_ok("sess-p"),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        insert_test_session(rec.id.clone(), session).await;

        let err = prompt(Some(&rec.id), "nope".into(), false)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            CoordinatorError::InvalidTransition {
                action: "harness prompt",
                ..
            }
        ));
        // Session still in pool
        assert!(global_pool().lock().await.contains(&rec.id));
        let _ = shutdown(Some(&rec.id), false).await;

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
            crate::harness::grok_cwd(&rec).expect("cwd"),
            mock_handshake_ok("sess-skip"),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        session.set_supports_compact(false);
        insert_test_session(rec.id.clone(), session).await;

        let view = compact(Some(&rec.id), false).await.unwrap();
        assert_eq!(view.skipped, Some(true));
        assert!(!view.applied);
        let _ = shutdown(Some(&rec.id), false).await;
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
            crate::harness::grok_cwd(&rec).expect("cwd"),
            lines,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        insert_test_session(rec.id.clone(), session).await;

        let view = compact(Some(&rec.id), false).await.unwrap();
        assert!(!view.applied);
        let st = run::status(&rec).unwrap();
        assert_eq!(st.status, crate::state::RunStatus::Running);
        assert_eq!(st.phase, STUB_PHASE_ACTIVE);
        let _ = shutdown(Some(&rec.id), false).await;
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
            crate::harness::grok_cwd(&rec).expect("cwd"),
            lines,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        insert_test_session(rec.id.clone(), session).await;

        let view = prompt(Some(&rec.id), "x".into(), false).await.unwrap();
        assert_eq!(view.failure_class, Some(FailureClass::HarnessCrash));
        assert!(view.applied);
        assert_eq!(
            view.status.as_ref().unwrap().failure_class,
            Some(FailureClass::HarnessCrash)
        );
        let _ = shutdown(Some(&rec.id), false).await;
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    fn arm_stdout_phase(rec: &ProjectRecord, phase: &str, retries: u32) {
        crate::state::with_run_state_lock(rec, || {
            let mut s = load_run_state(rec)?;
            s.driver = crate::workflow::WorkflowDriver::Adapter;
            s.phase = phase.into();
            s.status = RunStatus::Running;
            s.acp_stdout_retries = retries;
            s.last_driven_phase = Some(phase.into());
            crate::state::save_run_state(rec, &s)
        })
        .unwrap();
    }

    async fn prompt_after_mock_close(rec: &ProjectRecord, sid: &str) -> HarnessPromptView {
        let mut session = GrokSession::start_mock(
            crate::harness::grok_cwd(rec).expect("cwd"),
            mock_handshake_ok(sid),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        session.mock_close();
        insert_test_session(rec.id.clone(), session).await;
        prompt(Some(&rec.id), "x".into(), false).await.unwrap()
    }

    async fn prompt_after_transport(
        rec: &ProjectRecord,
        sid: &str,
        kind: std::io::ErrorKind,
    ) -> HarnessPromptView {
        let mut session = GrokSession::start_mock(
            crate::harness::grok_cwd(rec).expect("cwd"),
            mock_handshake_ok(sid),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        session.mock_transport_error(std::io::Error::new(kind, "reset"));
        insert_test_session(rec.id.clone(), session).await;
        prompt(Some(&rec.id), "x".into(), false).await.unwrap()
    }

    fn seed_alive_persist(rec: &ProjectRecord, session_id: &str, in_flight: bool) {
        let path = persist_path(rec).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let flag = if in_flight { "true" } else { "false" };
        // The in-process shape names this process as the holder (AliveSelf) and has no
        // control address, exactly what `spawn_in_process` writes.
        let body = format!(
            r#"{{"version":1,"project_id":"{id}","adapter":"grok","supports_compact":true,"alive":true,"prompt_in_flight":{flag},"holder_pid":{pid},"session_id":"{session_id}"}}"#,
            id = rec.id,
            pid = std::process::id(),
        );
        std::fs::write(path, body).unwrap();
    }

    /// A port that accepted a listener and then lost it: connecting is refused (the
    /// typed `NoListener` arm), unlike an unbound low port which can time out on Windows.
    fn closed_addr() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        drop(listener);
        addr
    }

    /// A live, real-protocol in-process holder: it writes its own ready persist and its
    /// own dead exit record. Returns the join handle.
    async fn spawn_live_holder(
        rec: &ProjectRecord,
        session_id: &str,
        cwd: Option<PathBuf>,
        harness: &str,
    ) -> tokio::task::JoinHandle<Result<()>> {
        let session = GrokSession::start_mock_for(
            cwd.unwrap_or_else(|| crate::harness::grok_cwd(rec).expect("cwd")),
            mock_handshake_ok(session_id),
            Duration::from_secs(2),
            harness,
        )
        .await
        .unwrap();
        let rec_hold = rec.clone();
        let hold = tokio::spawn(async move { hold_loop_with_session(rec_hold, session).await });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(Some(h)) = load_persist(rec)
                && h.control_addr.is_some()
            {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("live holder never became ready");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        hold
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn mock_close_retries_then_stops() {
        let _guard = test_env_lock();
        let _pr = crate::ci::set_test_open_green_pr(None);
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        crate::run::run_stub(&rec, Some("0064".into())).unwrap();
        arm_stdout_phase(&rec, "implement", 0);

        let first = prompt_after_mock_close(&rec, "sess-stdout-1").await;
        assert!(!first.applied);
        assert_eq!(first.skipped, Some(true));
        assert!(first.error.is_none());
        let st = run::status(&rec).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, "implement");
        let state = load_run_state(&rec).unwrap();
        assert_eq!(state.acp_stdout_retries, 1);
        assert!(state.last_driven_phase.is_none());
        assert!(
            state.last_event.contains("recycle: acp-stdout 1/2"),
            "{}",
            state.last_event
        );
        assert!(
            state.last_event.contains("child=unknown"),
            "{}",
            state.last_event
        );
        assert!(
            state.last_event.contains('\u{2014}'),
            "{}",
            state.last_event
        );
        assert!(crate::notify::artifact::existing_path(&rec).is_none());

        let second = prompt_after_mock_close(&rec, "sess-stdout-2").await;
        assert!(!second.applied);
        assert_eq!(load_run_state(&rec).unwrap().acp_stdout_retries, 2);

        let third = prompt_after_mock_close(&rec, "sess-stdout-3").await;
        assert!(third.applied);
        assert_eq!(third.failure_class, Some(FailureClass::HarnessCrash));
        let stopped = run::status(&rec).unwrap();
        assert_eq!(stopped.status, RunStatus::Stopped);
        assert_eq!(stopped.phase, "implement");
        assert_eq!(stopped.failure_class, Some(FailureClass::HarnessCrash));
        assert!(crate::notify::artifact::existing_path(&rec).is_some());
        let _ = shutdown(Some(&rec.id), false).await;
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn stdout_close_after_cancel_does_not_retry() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        crate::run::run_stub(&rec, Some("0064".into())).unwrap();
        arm_stdout_phase(&rec, "implement", 0);

        let mut session = GrokSession::start_mock(
            crate::harness::grok_cwd(&rec).expect("cwd"),
            mock_handshake_ok("sess-cancel-eof"),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        session.cancel_handle().cancel().await.unwrap();
        session.mock_close();
        insert_test_session(rec.id.clone(), session).await;
        let view = prompt(Some(&rec.id), "x".into(), false).await.unwrap();
        assert!(!view.applied);
        assert_eq!(view.skipped, Some(true));
        assert!(view.error.is_none());
        let state = load_run_state(&rec).unwrap();
        assert_eq!(state.acp_stdout_retries, 0);
        assert_eq!(state.status, RunStatus::Running);
        assert!(crate::notify::artifact::existing_path(&rec).is_none());
        let _ = shutdown(Some(&rec.id), false).await;
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn transport_drop_retries_then_stops() {
        let _guard = test_env_lock();
        let _pr = crate::ci::set_test_open_green_pr(Some(91));
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        crate::run::run_stub(&rec, Some("0091".into())).unwrap();
        arm_stdout_phase(&rec, "implement", 0);

        let first = prompt_after_transport(
            &rec,
            "sess-transport-1",
            std::io::ErrorKind::ConnectionReset,
        )
        .await;
        assert!(!first.applied);
        assert_eq!(first.skipped, Some(true));
        assert!(first.error.is_none());
        let st = run::status(&rec).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, "implement");
        let state = load_run_state(&rec).unwrap();
        assert_eq!(state.acp_stdout_retries, 1);
        assert!(state.last_driven_phase.is_none());
        assert!(
            state
                .last_event
                .starts_with("recycle: transport 1/2 \u{2014}"),
            "{}",
            state.last_event
        );
        assert!(crate::notify::artifact::existing_path(&rec).is_none());
        assert!(crate::harness::abort::should_refuse_reuse(&rec));

        let second = prompt_after_transport(
            &rec,
            "sess-transport-2",
            std::io::ErrorKind::ConnectionReset,
        )
        .await;
        assert!(!second.applied);
        assert_eq!(load_run_state(&rec).unwrap().acp_stdout_retries, 2);

        seed_alive_persist(&rec, "sess-transport-2", false);
        let third = prompt_after_transport(
            &rec,
            "sess-transport-3",
            std::io::ErrorKind::ConnectionReset,
        )
        .await;
        assert!(third.applied);
        assert_eq!(third.failure_class, Some(FailureClass::HarnessCrash));
        let err = third.error.unwrap_or_default();
        assert!(
            err.starts_with("PR #91 green; merge not performed \u{2014} "),
            "{err}"
        );
        assert!(err.contains("I/O error:"), "{err}");
        let stopped = run::status(&rec).unwrap();
        assert_eq!(stopped.status, RunStatus::Stopped);
        assert_eq!(stopped.failure_class, Some(FailureClass::HarnessCrash));
        assert!(crate::notify::artifact::existing_path(&rec).is_some());
        let ended = load_run_state(&rec).unwrap();
        assert!(
            !ended.last_event.starts_with("recycle:"),
            "{}",
            ended.last_event
        );
        assert!(
            ended.last_event.starts_with("outcome: failure"),
            "{}",
            ended.last_event
        );
        let persisted = load_persist(&rec).unwrap().expect("persist");
        assert!(persisted.prompt_in_flight);
        let _ = shutdown(Some(&rec.id), false).await;
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn transport_not_found_is_immediate_harness_crash() {
        let _guard = test_env_lock();
        let _pr = crate::ci::set_test_open_green_pr(None);
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        crate::run::run_stub(&rec, Some("0091".into())).unwrap();
        arm_stdout_phase(&rec, "implement", 0);
        let view =
            prompt_after_transport(&rec, "sess-not-found", std::io::ErrorKind::NotFound).await;
        assert!(view.applied);
        assert_eq!(view.failure_class, Some(FailureClass::HarnessCrash));
        let err = view.error.unwrap_or_default();
        assert!(!err.contains("merge not performed"), "{err}");
        assert_eq!(load_run_state(&rec).unwrap().acp_stdout_retries, 0);
        let _ = shutdown(Some(&rec.id), false).await;
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn stdout_and_transport_share_one_budget() {
        let _guard = test_env_lock();
        let _pr = crate::ci::set_test_open_green_pr(None);
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        crate::run::run_stub(&rec, Some("0091".into())).unwrap();
        arm_stdout_phase(&rec, "implement", 0);

        let stdout = prompt_after_mock_close(&rec, "sess-stdout-a").await;
        assert!(!stdout.applied);
        assert_eq!(load_run_state(&rec).unwrap().acp_stdout_retries, 1);

        let drop =
            prompt_after_transport(&rec, "sess-transport-b", std::io::ErrorKind::BrokenPipe).await;
        assert!(!drop.applied);
        assert_eq!(load_run_state(&rec).unwrap().acp_stdout_retries, 2);

        let stop = prompt_after_transport(
            &rec,
            "sess-transport-c",
            std::io::ErrorKind::ConnectionReset,
        )
        .await;
        assert!(stop.applied);
        assert_eq!(stop.failure_class, Some(FailureClass::HarnessCrash));
        assert_eq!(run::status(&rec).unwrap().status, RunStatus::Stopped);
        assert_eq!(load_run_state(&rec).unwrap().acp_stdout_retries, 2);
        let _ = shutdown(Some(&rec.id), false).await;
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn transport_drop_after_cancel_does_not_retry() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        crate::run::run_stub(&rec, Some("0091".into())).unwrap();
        arm_stdout_phase(&rec, "implement", 0);

        let mut session = GrokSession::start_mock(
            crate::harness::grok_cwd(&rec).expect("cwd"),
            mock_handshake_ok("sess-cancel-drop"),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        session.cancel_handle().cancel().await.unwrap();
        session.mock_transport_error(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset",
        ));
        insert_test_session(rec.id.clone(), session).await;
        let view = prompt(Some(&rec.id), "x".into(), false).await.unwrap();
        assert!(!view.applied);
        assert_eq!(view.skipped, Some(true));
        assert!(view.error.is_none());
        let state = load_run_state(&rec).unwrap();
        assert_eq!(state.acp_stdout_retries, 0);
        assert_eq!(state.status, RunStatus::Running);
        assert!(crate::notify::artifact::existing_path(&rec).is_none());
        let _ = shutdown(Some(&rec.id), false).await;
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn transport_recycle_drops_aborted_session_on_next_start() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        crate::run::run_stub(&rec, Some("0091".into())).unwrap();
        arm_stdout_phase(&rec, "implement", 0);

        let first =
            prompt_after_transport(&rec, "sess-old", std::io::ErrorKind::ConnectionReset).await;
        assert!(!first.applied);
        assert!(crate::harness::abort::should_refuse_reuse(&rec));
        seed_alive_persist(&rec, "sess-old", false);

        let missing = proj.path().join("missing-grok-0091.exe");
        let started = start_with_bin(Some(&rec.id), true, missing, None, false, "grok", None).await;
        assert!(started.is_err(), "{started:?}");
        let st = status(Some(&rec.id), false).await.unwrap();
        assert_ne!(st.session_id.as_deref(), Some("sess-old"));
        let path = persist_path(&rec).unwrap();
        assert!(path.exists(), "failed spawn writes a dead persist");
        let raw = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["alive"], false);
        assert_ne!(value["session_id"], "sess-old");
        let _ = shutdown(Some(&rec.id), false).await;
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn stdout_closed_rpc_error_is_immediate_harness_crash() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        crate::run::run_stub(&rec, Some("0064".into())).unwrap();
        arm_stdout_phase(&rec, "implement", 0);

        let mut lines = mock_handshake_ok("sess-rpc-stdout");
        lines.push(crate::harness::grok::rpc_error(4, "stdout closed"));
        let session = GrokSession::start_mock(
            crate::harness::grok_cwd(&rec).expect("cwd"),
            lines,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        insert_test_session(rec.id.clone(), session).await;
        let view = prompt(Some(&rec.id), "x".into(), false).await.unwrap();
        assert!(view.applied);
        assert_eq!(view.failure_class, Some(FailureClass::HarnessCrash));
        assert_eq!(load_run_state(&rec).unwrap().acp_stdout_retries, 0);
        let _ = shutdown(Some(&rec.id), false).await;
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn exhaust_names_green_pr_on_implement_and_address_findings_only() {
        let _guard = test_env_lock();
        let _pr = crate::ci::set_test_open_green_pr(Some(505));
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        let prefix = "PR #505 green; merge not performed \u{2014} ";

        crate::run::run_stub(&rec, Some("0064".into())).unwrap();
        arm_stdout_phase(&rec, "implement", 2);
        let view = prompt_after_mock_close(&rec, "sess-pr-impl").await;
        assert!(view.applied);
        let err = view.error.unwrap_or_default();
        assert!(err.starts_with(prefix), "{err}");
        assert!(err.contains('\u{2014}'));
        let st = run::status(&rec).unwrap();
        assert_eq!(st.status, RunStatus::Stopped);
        assert_eq!(st.phase, "implement");

        crate::run::run_stub(&rec, Some("0064".into())).unwrap();
        arm_stdout_phase(&rec, "address-findings", 2);
        let view = prompt_after_mock_close(&rec, "sess-pr-addr").await;
        let err = view.error.unwrap_or_default();
        assert!(err.starts_with(prefix), "{err}");
        assert_eq!(run::status(&rec).unwrap().phase, "address-findings");

        crate::run::run_stub(&rec, Some("0064".into())).unwrap();
        arm_stdout_phase(&rec, "plan", 2);
        let view = prompt_after_mock_close(&rec, "sess-pr-plan").await;
        let err = view.error.unwrap_or_default();
        assert!(!err.contains("merge not performed"), "{err}");
        assert!(err.starts_with("ACP stdout closed"), "{err}");
        assert_eq!(view.failure_class, Some(FailureClass::HarnessCrash));
        let _ = shutdown(Some(&rec.id), false).await;
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    async fn reuse_reaps_when_persisted_cwd_differs() {
        let dir = tempdir().unwrap();
        let rec = ProjectRecord {
            id: "cwd-mismatch".into(),
            path: dir.path().to_path_buf(),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: Default::default(),
            state_dir: Some(dir.path().join("state")),
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
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now(),
        };
        crate::state::ensure_state_dir(&rec).unwrap();
        // An owned but different epoch cwd: the holder is provable and gets terminated.
        let old_epoch = dir.path().join("old-epoch");
        std::fs::create_dir_all(&old_epoch).unwrap();
        let hold = spawn_live_holder(&rec, "sess-old-epoch", Some(old_epoch), "grok").await;
        let got = reuse_or_reap_existing(&rec, "grok", None, None)
            .await
            .unwrap();
        assert!(got.is_none(), "a different epoch cwd is not reusable");
        assert!(
            load_persist(&rec).unwrap().is_none(),
            "the delivered holder's own exit record is cleared"
        );
        let _ = tokio::time::timeout(Duration::from_secs(2), hold).await;
    }

    #[tokio::test]
    async fn reuse_refuses_a_cwd_outside_the_project_roots() {
        let dir = tempdir().unwrap();
        let rec = ProjectRecord {
            id: "cwd-foreign".into(),
            path: dir.path().to_path_buf(),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: Default::default(),
            state_dir: Some(dir.path().join("state")),
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
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now(),
        };
        crate::state::ensure_state_dir(&rec).unwrap();
        let handle = PersistedGrokHandle {
            version: 1,
            model: None,
            bin: None,
            project_id: rec.id.clone(),
            session_id: Some("sess".into()),
            cwd: Some(PathBuf::from(r"C:\other\epoch")),
            pid: None,
            holder_pid: None,
            control_addr: Some(closed_addr()),
            adapter: "grok".into(),
            supports_compact: true,
            alive: true,
            prompt_in_flight: false,
            error: None,

            context_usage: None,
            last_signal: None,
            last_tool_title: None,
        };
        save_persist(&rec, &handle).unwrap();
        let before = std::fs::read(persist_path(&rec).unwrap()).unwrap();
        let err = reuse_or_reap_existing(&rec, "grok", None, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("cwd-not-owned"), "{err}");
        assert_eq!(
            before,
            std::fs::read(persist_path(&rec).unwrap()).unwrap(),
            "a foreign cwd persist is never touched"
        );
    }

    #[test]
    fn identity_predicate_covers_adapter_model_and_bin() {
        let base = GrokHarnessStatus {
            alive: true,
            session_id: Some("s".into()),
            cwd: None,
            supports_compact: true,
            pid: Some(1),
            adapter: "codex".into(),
            context_usage: None,
            last_signal: None,
            last_tool_title: None,
            model: Some("gpt-6.1-sol[medium]".into()),
            bin: Some(PathBuf::from(r"C:\bin\codex-acp.cmd")),
        };
        let bin = PathBuf::from(r"C:\bin\codex-acp.cmd");
        assert!(identity_matches_parts(
            &base,
            "codex",
            Some("gpt-6.1-sol[medium]"),
            Some(&bin)
        ));
        // model-only change (the track's headline case)
        assert!(!identity_matches_parts(
            &base,
            "codex",
            Some("gpt-6-luna"),
            Some(&bin)
        ));
        // adapter-only change
        assert!(!identity_matches_parts(
            &base,
            "claude",
            Some("gpt-6.1-sol[medium]"),
            Some(&bin)
        ));
        // bin-only change
        assert!(!identity_matches_parts(
            &base,
            "codex",
            Some("gpt-6.1-sol[medium]"),
            Some(Path::new(r"C:\bin\other.exe"))
        ));
        // None binding model matches only None live model
        let unpinned = GrokHarnessStatus {
            model: None,
            ..base.clone()
        };
        assert!(identity_matches_parts(&unpinned, "codex", None, Some(&bin)));
        assert!(!identity_matches_parts(
            &unpinned,
            "codex",
            Some("gpt-6-luna"),
            Some(&bin)
        ));
        // pre-upgrade persist: no bin identity, mismatches a resolved binding bin
        let mut legacy = base.clone();
        legacy.bin = None;
        assert!(!identity_matches_parts(
            &legacy,
            "codex",
            Some("gpt-6.1-sol[medium]"),
            Some(&bin)
        ));
        assert!(identity_matches_parts(
            &legacy,
            "codex",
            Some("gpt-6.1-sol[medium]"),
            None
        ));
    }

    #[tokio::test]
    async fn persist_reuse_with_a_different_model_is_reaped_not_returned() {
        let dir = tempdir().unwrap();
        let rec = ProjectRecord {
            id: "p-model".into(),
            path: dir.path().to_path_buf(),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: Default::default(),
            state_dir: Some(dir.path().join("state")),
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
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now(),
        };
        crate::state::ensure_state_dir(&rec).unwrap();
        let hold = spawn_live_holder(&rec, "sess-model", None, "grok").await;
        let got = reuse_or_reap_existing(&rec, "grok", Some("gpt-6-luna"), None)
            .await
            .unwrap();
        assert!(got.is_none(), "same adapter + different model must reap");
        assert!(load_persist(&rec).unwrap().is_none());
        let _ = tokio::time::timeout(Duration::from_secs(2), hold).await;
    }

    #[tokio::test]
    async fn retry_after_failed_start_clears_stale_error() {
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
            notify_progress: false,
            worktree_isolation: false,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            ci_fix_routing: false,
            review_comment_routing: false,
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now(),
        };
        let stale = PersistedGrokHandle {
            version: 1,
            model: None,
            bin: None,
            project_id: rec.id.clone(),
            session_id: None,
            cwd: None,
            pid: None,
            holder_pid: Some(1),
            control_addr: None,
            adapter: "grok".into(),
            supports_compact: false,
            alive: false,
            prompt_in_flight: false,
            error: Some("ACP authenticate: not logged in".into()),

            context_usage: None,
            last_signal: None,
            last_tool_title: None,
        };
        save_persist(&rec, &stale).unwrap();
        assert!(load_persist(&rec).unwrap().unwrap().error.is_some());

        // Stale error would be returned immediately if we did not discard.
        assert!(matches!(
            interpret_persist_after_spawn(&stale),
            PersistWait::Failed(_)
        ));
        // Age past DISCARD_GRACE: only the start path may discard it, with one line.
        let aged = SystemTime::now() - (DISCARD_GRACE + Duration::from_secs(1));
        std::fs::OpenOptions::new()
            .write(true)
            .open(persist_path(&rec).unwrap())
            .unwrap()
            .set_modified(aged)
            .unwrap();
        let got = reuse_or_reap_existing(&rec, "grok", None, None)
            .await
            .unwrap();
        assert!(got.is_none(), "an aged failed start is not reusable");
        assert!(
            load_persist(&rec).unwrap().is_none(),
            "retry must discard the prior failure record through the one grace rule"
        );
        let claim = claim_start(&rec, "grok").unwrap();
        assert!(
            claim.identity.is_attempt(),
            "the claim writes the pending attempt shape"
        );
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
            notify_progress: false,
            worktree_isolation: false,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            ci_fix_routing: false,
            review_comment_routing: false,
            skill_aliases: std::collections::BTreeMap::new(),
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
            notify_progress: false,
            worktree_isolation: false,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            ci_fix_routing: false,
            review_comment_routing: false,
            skill_aliases: std::collections::BTreeMap::new(),
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
            notify_progress: false,
            worktree_isolation: false,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            ci_fix_routing: false,
            review_comment_routing: false,
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now(),
        };
        let p = persist_path(&rec).unwrap();
        assert!(p.ends_with("harness-grok.json"));
    }

    #[test]
    fn persist_missing_supports_compact_loads_true() {
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
            notify_progress: false,
            worktree_isolation: false,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            ci_fix_routing: false,
            review_comment_routing: false,
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now(),
        };
        ensure_state_dir(&rec).unwrap();
        let path = persist_path(&rec).unwrap();
        std::fs::write(
            &path,
            r#"{"version":1,"project_id":"p","alive":true,"session_id":"s"}"#,
        )
        .unwrap();
        let h = load_persist(&rec).unwrap().expect("persist");
        assert!(
            h.supports_compact,
            "legacy files without supports_compact must load true"
        );
    }

    #[test]
    fn persist_missing_adapter_loads_grok() {
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
            notify_progress: false,
            worktree_isolation: false,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            ci_fix_routing: false,
            review_comment_routing: false,
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now(),
        };
        ensure_state_dir(&rec).unwrap();
        let path = persist_path(&rec).unwrap();
        std::fs::write(
            &path,
            r#"{"version":1,"project_id":"p","alive":true,"supports_compact":true}"#,
        )
        .unwrap();
        let h = load_persist(&rec).unwrap().expect("persist");
        assert_eq!(h.adapter, "grok");
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
            // `timeout` exits immediately when stdin is redirected; `ping` is the
            // portable long-lived sleeper here.
            std::process::Command::new("cmd")
                .args(["/C", "ping", "-n", "300", "127.0.0.1"])
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
            notify_progress: false,
            worktree_isolation: false,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            ci_fix_routing: false,
            review_comment_routing: false,
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now(),
        };
        assert_eq!(grok_cwd(&rec).expect("cwd"), exec);
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
            crate::harness::grok_cwd(&rec).expect("cwd"),
            lines,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        insert_test_session(rec.id.clone(), session).await;

        let view = prompt(Some(&rec.id), "x".into(), false).await.unwrap();
        assert!(!view.applied, "cancelled must not apply");
        assert_eq!(view.skipped, Some(true));
        let st = run::status(&rec).unwrap();
        assert_eq!(st.status, crate::state::RunStatus::Running);
        assert_eq!(st.phase, crate::workflow::graph::PHASE_PLAN);
        assert!(crate::notify::artifact::existing_path(&rec).is_none());
        assert_eq!(
            crate::state::load_run_state(&rec)
                .unwrap()
                .acp_stdout_retries,
            0
        );
        let _ = shutdown(Some(&rec.id), false).await;
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
            crate::harness::grok_cwd(&rec).expect("cwd"),
            lines,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        insert_test_session(rec.id.clone(), session).await;

        let view = prompt(Some(&rec.id), "x".into(), false).await.unwrap();
        assert!(!view.applied);
        let st = run::status(&rec).unwrap();
        assert_eq!(st.status, crate::state::RunStatus::Running);
        assert_eq!(st.phase, crate::workflow::graph::PHASE_PLAN);
        assert!(crate::notify::artifact::existing_path(&rec).is_none());
        let _ = shutdown(Some(&rec.id), false).await;
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
            crate::harness::grok_cwd(&rec).expect("cwd"),
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
    async fn holder_cancel_uses_that_epoch_control_addr() {
        let _guard = test_env_lock();
        struct ClearHolder;
        impl Drop for ClearHolder {
            fn drop(&mut self) {
                crate::state::release_holder_epoch();
            }
        }
        let _clear = ClearHolder;
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(crate::harness::abort::ENV_CANCEL_WAIT_SECS, "0");
        }
        let mut reg = Registry::default();
        let mut rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        rec.worktree_isolation = true;
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        crate::state::ensure_state_dir(&rec).unwrap();
        let mut first = crate::state::RunState::idle(&rec.id);
        first.run_epoch = 3;
        first.status = crate::state::RunStatus::Running;
        first.phase = crate::workflow::graph::PHASE_PLAN.into();
        let mut second = first.clone();
        second.run_epoch = 4;
        crate::state::write_run_book(
            &rec,
            &crate::state::RunEnvelope::new(0, vec![first, second]),
        )
        .unwrap();
        crate::state::set_holder_epoch(3).unwrap();
        let session = GrokSession::start_mock(
            proj.path().to_path_buf(),
            mock_handshake_ok("sess-a"),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        let rec_hold = rec.clone();
        let hold = tokio::spawn(async move { hold_loop_with_session(rec_hold, session).await });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let addr = loop {
            if let Ok(Some(h)) = load_persist_at(&rec, Some(3))
                && let Some(addr) = h.control_addr
            {
                break addr;
            }
            if std::time::Instant::now() >= deadline {
                panic!("epoch 3 holder never published a control address");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr_b = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            for _ in 0..4 {
                let Ok((stream, _)) = listener.accept() else {
                    break;
                };
                let mut writer = stream.try_clone().unwrap();
                let mut reader = std::io::BufReader::new(stream);
                let mut line = String::new();
                let _ = std::io::BufRead::read_line(&mut reader, &mut line);
                let _ = std::io::Write::write_all(&mut writer, b"{\"ok\":true}\n");
            }
        });
        let state_dir = crate::state::resolve_state_dir(&rec).unwrap();
        let session_dir = state_dir.join("sessions").join("4");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("harness-grok.json"),
            serde_json::json!({
                "version": 1,
                "project_id": rec.id,
                "adapter": "grok",
                "supports_compact": true,
                "alive": true,
                "control_addr": addr_b,
            })
            .to_string(),
        )
        .unwrap();
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
        holder_cancel(&rec, Some(3)).await.unwrap();
        let prompt_view = tokio::time::timeout(Duration::from_secs(2), prompt_task)
            .await
            .expect("prompt task")
            .expect("join")
            .expect("rpc");
        assert_eq!(prompt_view.stop_reason.as_deref(), Some("cancelled"));
        let ping = holder_rpc(&addr_b, &HoldRequest::Ping).await.unwrap();
        assert!(ping.ok, "sibling ping failed");
        let legacy = state_dir.join("harness-grok.json");
        assert!(
            !legacy.exists(),
            "cancel read the legacy persist instead of sessions/3"
        );
        let _ = holder_rpc(&addr, &HoldRequest::Shutdown).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), hold).await;
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
            crate::harness::grok_cwd(&rec).expect("cwd"),
            lines,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        insert_test_session(rec.id.clone(), session).await;

        let view = prompt(Some(&rec.id), "x".into(), false).await.unwrap();
        assert!(!view.applied);
        assert_eq!(view.skipped, Some(true));
        let st = run::status(&rec).unwrap();
        assert_eq!(st.status, crate::state::RunStatus::Running);
        assert!(crate::notify::artifact::existing_path(&rec).is_none());
        let _ = shutdown(Some(&rec.id), false).await;
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
            model: None,
            bin: None,
            project_id: rec.id.clone(),
            session_id: Some("sess-wedged".into()),
            cwd: Some(proj.path().to_path_buf()),
            pid: None,
            holder_pid: None,
            control_addr: Some(closed_addr()),
            adapter: "grok".into(),
            supports_compact: true,
            alive: true,
            prompt_in_flight: true,
            error: None,

            context_usage: None,
            last_signal: None,
            last_tool_title: None,
        };
        save_persist(&rec, &old).unwrap();

        let reused = reuse_or_reap_existing(&rec, "grok", None, None)
            .await
            .unwrap();
        assert!(reused.is_none());
        assert!(
            load_persist(&rec).unwrap().is_none(),
            "a closed listener is the start path's arm A: clear + start"
        );
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
            notify_progress: false,
            worktree_isolation: false,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            ci_fix_routing: false,
            review_comment_routing: false,
            skill_aliases: std::collections::BTreeMap::new(),
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

    #[tokio::test]
    async fn remember_fidelity_keeps_control_addr() {
        let dir = tempdir().unwrap();
        let rec = ProjectRecord {
            id: "fidelity".into(),
            path: dir.path().to_path_buf(),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: Default::default(),
            state_dir: Some(dir.path().join("state")),
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
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now(),
        };
        let handle = PersistedGrokHandle {
            version: 1,
            model: None,
            bin: None,
            project_id: rec.id.clone(),
            session_id: Some("sess".into()),
            cwd: Some(dir.path().to_path_buf()),
            pid: Some(7),
            holder_pid: Some(8),
            control_addr: Some("127.0.0.1:9".into()),
            adapter: "grok".into(),
            supports_compact: true,
            alive: true,
            prompt_in_flight: true,
            error: None,
            context_usage: None,
            last_signal: None,
            last_tool_title: None,
        };
        save_persist(&rec, &handle).unwrap();
        let mut session = GrokSession::start_mock(
            dir.path().to_path_buf(),
            mock_handshake_ok("sess"),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        session.context_usage = Some(ContextUsage {
            used: 4,
            size: 8,
            cost: Some("0.01 USD".into()),
        });
        session.last_signal = Some(crate::harness::capabilities::APPROVALS_AUTO_ALLOWED.into());
        session.last_tool_title = Some("read file".into());
        remember_fidelity(&rec, &session);
        let loaded = load_persist(&rec).unwrap().unwrap();
        assert_eq!(loaded.control_addr.as_deref(), Some("127.0.0.1:9"));
        assert!(loaded.prompt_in_flight);
        assert_eq!(loaded.holder_pid, Some(8));
        let usage = loaded.context_usage.as_ref().expect("usage");
        assert_eq!(usage.used, 4);
        assert_eq!(usage.size, 8);
        assert_eq!(usage.cost.as_deref(), Some("0.01 USD"));
        assert_eq!(
            loaded.last_signal.as_deref(),
            Some(crate::harness::capabilities::APPROVALS_AUTO_ALLOWED)
        );
        assert_eq!(loaded.last_tool_title.as_deref(), Some("read file"));
        let value = serde_json::to_value(loaded.to_status()).unwrap();
        assert!(value.get("holder_pid").is_none());
        assert_eq!(value["context_usage"]["used"], 4);
        assert_eq!(value["last_tool_title"], "read file");
    }

    #[test]
    fn old_harness_persist_without_fidelity_fields_loads() {
        let raw = r#"{"version":1,"project_id":"p","adapter":"grok","supports_compact":true,"alive":true,"prompt_in_flight":false,"holder_pid":9}"#;
        let handle: PersistedGrokHandle = serde_json::from_str(raw).unwrap();
        assert!(handle.context_usage.is_none());
        assert!(handle.last_signal.is_none());
        assert!(handle.last_tool_title.is_none());
        assert_eq!(handle.holder_pid, Some(9));
        let value = serde_json::to_value(handle.to_status()).unwrap();
        assert!(value.get("holder_pid").is_none());
        assert!(value.get("context_usage").is_none());
        assert!(value.get("last_signal").is_none());
        assert!(value.get("last_tool_title").is_none());
        let parsed: GrokHarnessStatus =
            serde_json::from_str(r#"{"alive":false,"supports_compact":false,"adapter":"cursor"}"#)
                .unwrap();
        assert!(!parsed.alive);
        assert!(parsed.context_usage.is_none());
        assert_eq!(parsed.adapter, "cursor");
    }

    fn harness_stub() -> GrokHarnessStatus {
        GrokHarnessStatus {
            alive: true,
            session_id: Some("sess-apply".into()),
            cwd: None,
            supports_compact: false,
            pid: None,
            adapter: "grok".into(),
            context_usage: None,
            last_signal: None,
            last_tool_title: None,
            model: None,
            bin: None,
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
            SpawnTally::default(),
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

    fn prepare_implement(on: bool) -> (tempfile::TempDir, tempfile::TempDir, ProjectRecord) {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        let mut rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        crate::run::run_with_driver(
            &rec,
            Some("0070".into()),
            crate::workflow::WorkflowDriver::Adapter,
        )
        .unwrap();
        rec.self_continuation = on;
        crate::state::with_run_state_lock(&rec, || {
            let mut state = crate::state::load_run_state(&rec)?;
            state.phase = crate::workflow::graph::PHASE_IMPLEMENT.into();
            crate::state::save_run_state(&rec, &state)
        })
        .unwrap();
        (home, proj, rec)
    }

    fn continue_line() -> PromptResult {
        PromptResult {
            text: "step body\nself-check: continue finding=none action=progress\n".into(),
            stop_reason: Some("end_turn".into()),
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn self_check_continue_skips_outcome_and_sets_pending() {
        let _guard = test_env_lock();
        let (_home, _proj, rec) = prepare_implement(true);
        let before = crate::state::load_run_state(&rec).unwrap();
        let view = apply_turn(
            &rec,
            Ok(continue_line()),
            harness_stub(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
        )
        .await
        .unwrap();
        assert!(!view.applied);
        assert!(view.skipped.is_none());
        assert!(view.error.is_none());
        let state = crate::state::load_run_state(&rec).unwrap();
        assert_eq!(state.phase, crate::workflow::graph::PHASE_IMPLEMENT);
        assert_eq!(state.status, crate::state::RunStatus::Running);
        assert!(state.failure_class.is_none());
        assert_eq!(state.phase_started_at, before.phase_started_at);
        let snap = state.self_check.expect("snap");
        assert!(snap.pending_inject);
        assert_eq!(snap.steps, 1);
        assert!(state.last_event.starts_with("self-check: step 1 "));
        assert!(state.last_event.contains("wall_ms="));
        assert!(state.last_event.contains("tools_ms="));
        assert!(state.last_event.contains("lines="));
        assert!(crate::notify::artifact::existing_path(&rec).is_none());
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn self_check_done_and_missing_line_still_succeed() {
        let _guard = test_env_lock();
        let (_home, _proj, rec) = prepare_implement(true);
        let done = PromptResult {
            text: "self-check: done".into(),
            stop_reason: Some("end_turn".into()),
        };
        let view = apply_turn(
            &rec,
            Ok(done),
            harness_stub(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
        )
        .await
        .unwrap();
        assert!(view.applied);
        assert!(view.error.is_none());
        let state = crate::state::load_run_state(&rec).unwrap();
        assert_eq!(state.phase, crate::workflow::graph::PHASE_CROSS_MODEL);
        assert!(state.self_check.is_none());

        let (_home2, _proj2, rec2) = prepare_implement(true);
        let missing = PromptResult {
            text: "finished the phase".into(),
            stop_reason: Some("end_turn".into()),
        };
        let view = apply_turn(
            &rec2,
            Ok(missing),
            harness_stub(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
        )
        .await
        .unwrap();
        assert!(view.applied);
        assert_eq!(
            crate::state::load_run_state(&rec2).unwrap().phase,
            crate::workflow::graph::PHASE_CROSS_MODEL
        );

        let (_home3, _proj3, rec3) = prepare_implement(true);
        let no_stop = PromptResult {
            text: "self-check: continue finding=none action=progress".into(),
            stop_reason: None,
        };
        let view = apply_turn(
            &rec3,
            Ok(no_stop),
            harness_stub(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
        )
        .await
        .unwrap();
        assert!(
            view.applied,
            "missing stopReason stays on today's success path"
        );
        assert!(
            crate::state::load_run_state(&rec3)
                .unwrap()
                .self_check
                .is_none()
        );
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn self_check_step_cap_completes() {
        let _guard = test_env_lock();
        let (_home, _proj, rec) = prepare_implement(true);
        crate::state::with_run_state_lock(&rec, || {
            let mut state = crate::state::load_run_state(&rec)?;
            state.self_check = Some(crate::state::SelfCheckState {
                steps: crate::workflow::self_check::SELF_CHECK_MAX_STEPS,
                ..crate::state::SelfCheckState::default()
            });
            crate::state::save_run_state(&rec, &state)
        })
        .unwrap();
        let view = apply_turn(
            &rec,
            Ok(continue_line()),
            harness_stub(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
        )
        .await
        .unwrap();
        assert!(view.applied);
        let state = crate::state::load_run_state(&rec).unwrap();
        assert_eq!(state.phase, crate::workflow::graph::PHASE_CROSS_MODEL);
        assert!(state.failure_class.is_none());
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn self_check_does_not_extend_phase_timeout() {
        let _guard = test_env_lock();
        let (_home, _proj, rec) = prepare_implement(true);
        let before = crate::state::load_run_state(&rec).unwrap();
        let budget =
            crate::workflow::timeout_for_phase(&rec, crate::workflow::graph::PHASE_IMPLEMENT);
        let view = apply_turn(
            &rec,
            Ok(continue_line()),
            harness_stub(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
        )
        .await
        .unwrap();
        assert!(!view.applied);
        let after = crate::state::load_run_state(&rec).unwrap();
        assert_eq!(after.phase_started_at, before.phase_started_at);
        assert_eq!(
            crate::workflow::timeout_for_phase(&rec, crate::workflow::graph::PHASE_IMPLEMENT),
            budget
        );
        assert_eq!(budget, std::time::Duration::from_secs(7200));

        let (_off_home, _off_proj, off) = prepare_implement(false);
        let view = apply_turn(
            &off,
            Ok(continue_line()),
            harness_stub(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
        )
        .await
        .unwrap();
        assert!(view.applied, "flag off keeps the success path");
        assert_eq!(
            crate::state::load_run_state(&off).unwrap().phase,
            crate::workflow::graph::PHASE_CROSS_MODEL
        );
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn self_check_paused_or_stopped_does_not_set_pending() {
        let _guard = test_env_lock();
        let (_home, _proj, rec) = prepare_implement(true);
        run::pause(&rec).unwrap();
        let view = apply_turn(
            &rec,
            Ok(continue_line()),
            harness_stub(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
        )
        .await
        .unwrap();
        assert!(!view.applied);
        assert_eq!(view.skipped, Some(true));
        assert!(
            crate::state::load_run_state(&rec)
                .unwrap()
                .self_check
                .is_none()
        );

        let (_home2, _proj2, rec2) = prepare_implement(true);
        crate::state::with_run_state_lock(&rec2, || {
            let mut state = crate::state::load_run_state(&rec2)?;
            state.status = crate::state::RunStatus::Stopped;
            crate::state::save_run_state(&rec2, &state)
        })
        .unwrap();
        let view = apply_turn(
            &rec2,
            Ok(continue_line()),
            harness_stub(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
        )
        .await
        .unwrap();
        assert_eq!(view.skipped, Some(true));
        assert!(
            crate::state::load_run_state(&rec2)
                .unwrap()
                .self_check
                .is_none()
        );

        let (_home3, _proj3, rec3) = prepare_implement(true);
        let cancelled = PromptResult {
            text: "self-check: continue finding=none action=repair".into(),
            stop_reason: Some("cancelled".into()),
        };
        let view = apply_turn(
            &rec3,
            Ok(cancelled),
            harness_stub(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
        )
        .await
        .unwrap();
        assert!(!view.applied);
        assert_eq!(view.skipped, Some(true));
        let state = crate::state::load_run_state(&rec3).unwrap();
        assert_eq!(state.phase, crate::workflow::graph::PHASE_IMPLEMENT);
        assert!(state.self_check.is_none());
        assert!(state.failure_class.is_none());
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
            SpawnTally::default(),
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
        std::fs::write(
            proj.path().join("conductor").join("conductor.md"),
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0001-Example](0001-Example/spec.md) | `.` | **Ready — not started** | one |\n\
             | [0002-Next](0002-Next/spec.md) | `.` | **Ready — not started** | next |\n",
        )
        .unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        let rec = reg
            .add(
                proj.path(),
                ProjectAddOptions {
                    auto_start: Some(crate::registry::AutoStartPolicy::Full),
                    ..Default::default()
                },
            )
            .unwrap();
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
            SpawnTally::default(),
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

    fn git_in(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
    }

    /// Adapter run parked at `phase` in a real git repo. Returns the record and its tempdirs.
    fn implement_leg_fixture(phase: &str) -> (ProjectRecord, tempfile::TempDir, tempfile::TempDir) {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        git_in(proj.path(), &["init", "-b", "main"]);
        std::fs::write(proj.path().join("f.txt"), "base\n").unwrap();
        git_in(proj.path(), &["add", "f.txt"]);
        git_in(proj.path(), &["commit", "-m", "base"]);
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        crate::run::run_with_driver(
            &rec,
            Some("0095".into()),
            crate::workflow::WorkflowDriver::Adapter,
        )
        .unwrap();
        crate::state::with_run_state_lock(&rec, || {
            let mut st = crate::state::load_run_state(&rec)?;
            st.phase = phase.into();
            crate::state::save_run_state(&rec, &st)
        })
        .unwrap();
        (rec, home, proj)
    }

    fn target_for(
        rec: &ProjectRecord,
        st: &crate::state::RunState,
        phase: &str,
        path: &std::path::Path,
    ) -> ResolvedExecution {
        ResolvedExecution {
            project_id: rec.id.clone(),
            run_epoch: st.run_epoch,
            phase: phase.into(),
            track_id: st.track_id.clone().unwrap_or_else(|| "0095".into()),
            declared_path: path.to_path_buf(),
            prompt_path: path.to_path_buf(),
            sample_path: path.to_path_buf(),
            source: crate::workflow::execution_repo::ResolutionSource::ProjectDefault,
            planning_only: false,
            late_metadata: Vec::new(),
        }
    }

    #[test]
    fn leg_verdict_fires_only_when_the_tree_is_exactly_as_the_leg_found_it() {
        let _guard = test_env_lock();
        let (rec, _home, proj) = implement_leg_fixture(crate::workflow::graph::PHASE_IMPLEMENT);
        let st = crate::state::load_run_state(&rec).unwrap();
        let phase = crate::workflow::graph::PHASE_IMPLEMENT;
        let target = target_for(&rec, &st, phase, proj.path());
        let status = crate::harness::no_change::note_baseline(&rec, &st, phase, &target);
        let v = crate::harness::no_change::verdict_after_turn(
            &rec,
            &st,
            phase,
            &target,
            &status.trust(),
            "codex",
            Some("approvals auto-allowed"),
            proj.path(),
        );
        let m = v.expect("unchanged tree must be a verdict");
        assert!(m.starts_with(crate::harness::no_change::NO_CHANGE_PREFIX));
        assert!(m.contains("codex") && m.contains("approvals auto-allowed"));
        // a second turn of the same leg keeps the original baseline
        std::fs::write(proj.path().join("new.txt"), "x\n").unwrap();
        let status = crate::harness::no_change::note_baseline(&rec, &st, phase, &target);
        assert!(
            crate::harness::no_change::verdict_after_turn(
                &rec,
                &st,
                phase,
                &target,
                &status.trust(),
                "codex",
                None,
                proj.path()
            )
            .is_none(),
            "a written file is a change"
        );
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[test]
    fn leg_verdict_is_silent_for_other_phases_drivers_and_unreadable_trees() {
        let _guard = test_env_lock();
        let (rec, _home, proj) = implement_leg_fixture(crate::workflow::graph::PHASE_FOLD);
        let mut st = crate::state::load_run_state(&rec).unwrap();
        let fold = crate::workflow::graph::PHASE_FOLD;
        let fold_target = target_for(&rec, &st, fold, proj.path());
        let fold_status = crate::harness::no_change::note_baseline(&rec, &st, fold, &fold_target);
        assert!(
            crate::harness::no_change::verdict_after_turn(
                &rec,
                &st,
                fold,
                &fold_target,
                &fold_status.trust(),
                "codex",
                None,
                proj.path()
            )
            .is_none(),
            "fold is never checked"
        );
        let imp = crate::workflow::graph::PHASE_IMPLEMENT;
        let target = target_for(&rec, &st, imp, proj.path());
        st.driver = crate::workflow::WorkflowDriver::Stub;
        let stub_status = crate::harness::no_change::note_baseline(&rec, &st, imp, &target);
        assert!(
            crate::harness::no_change::verdict_after_turn(
                &rec,
                &st,
                imp,
                &target,
                &stub_status.trust(),
                "codex",
                None,
                proj.path()
            )
            .is_none(),
            "stub driver is never checked"
        );
        st.driver = crate::workflow::WorkflowDriver::Adapter;
        let status = crate::harness::no_change::note_baseline(&rec, &st, imp, &target);
        let mut gone = target.clone();
        gone.sample_path = proj.path().join("not-a-repo-dir");
        assert!(
            crate::harness::no_change::verdict_after_turn(
                &rec,
                &st,
                imp,
                &gone,
                &status.trust(),
                "codex",
                None,
                proj.path()
            )
            .is_none(),
            "unreadable tree must not fire"
        );
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn no_change_verdict_fails_the_leg_instead_of_advancing() {
        let _guard = test_env_lock();
        let (rec, _home, _proj) = implement_leg_fixture(crate::workflow::graph::PHASE_IMPLEMENT);
        let turn = Ok(PromptResult {
            text: "done".into(),
            stop_reason: Some("end_turn".into()),
        });
        let msg = crate::harness::no_change::no_change_message("implement", "codex", None);
        let view = apply_turn_checked(
            &rec,
            turn,
            harness_stub(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
            Some(msg.clone()),
            None,
            None,
            false,
        )
        .await
        .unwrap();
        assert!(view.applied);
        assert_eq!(view.failure_class, Some(FailureClass::HarnessCrash));
        assert_eq!(view.error.as_deref(), Some(msg.as_str()));
        let st = crate::state::load_run_state(&rec).unwrap();
        assert_eq!(st.status, crate::state::RunStatus::Stopped);
        assert_eq!(st.failure_class, Some(FailureClass::HarnessCrash));
        assert_eq!(st.phase, crate::workflow::graph::PHASE_IMPLEMENT);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn no_verdict_still_advances_the_leg() {
        let _guard = test_env_lock();
        let (rec, _home, _proj) = implement_leg_fixture(crate::workflow::graph::PHASE_IMPLEMENT);
        let turn = Ok(PromptResult {
            text: "done".into(),
            stop_reason: Some("end_turn".into()),
        });
        let view = apply_turn_checked(
            &rec,
            turn,
            harness_stub(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
            None,
            None,
            None,
            false,
        )
        .await
        .unwrap();
        assert!(view.applied);
        assert!(view.failure_class.is_none());
        let st = crate::state::load_run_state(&rec).unwrap();
        assert_ne!(st.status, crate::state::RunStatus::Stopped);
        assert_eq!(st.phase, crate::workflow::graph::PHASE_CROSS_MODEL);
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
            SpawnTally::default(),
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
        std::fs::create_dir_all(proj.path().join("conductor").join("0001-Example")).unwrap();
        std::fs::create_dir_all(proj.path().join("conductor").join("0002-Next")).unwrap();
        std::fs::write(
            proj.path().join("conductor").join("conductor.md"),
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0001-Example](0001-Example/spec.md) | `.` | **Ready — not started** | one |\n\
             | [0002-Next](0002-Next/spec.md) | `.` | **Ready — not started** | next |\n",
        )
        .unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        let mut reg = Registry::default();
        let rec = reg
            .add(
                proj.path(),
                ProjectAddOptions {
                    auto_start: Some(crate::registry::AutoStartPolicy::Full),
                    ..Default::default()
                },
            )
            .unwrap();
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
            SpawnTally::default(),
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

    fn spawn_fail_tally() -> SpawnTally {
        SpawnTally {
            attempts: 2,
            ok: 0,
            last_error: Some("terminal/create spawn: program not found".into()),
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn apply_turn_all_spawn_fail_is_harness_crash_not_gate() {
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
            Some("0032".into()),
            crate::workflow::WorkflowDriver::Adapter,
        )
        .unwrap();
        crate::state::with_run_state_lock(&rec, || {
            let mut s = crate::state::load_run_state(&rec)?;
            s.phase = crate::workflow::graph::PHASE_IMPLEMENT.into();
            crate::state::save_run_state(&rec, &s)
        })
        .unwrap();

        let turn = Ok(PromptResult {
            text: "implement claimed done".into(),
            stop_reason: Some("end_turn".into()),
        });
        let view = apply_turn(
            &rec,
            turn,
            harness_stub(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            spawn_fail_tally(),
        )
        .await
        .unwrap();
        assert!(view.applied);
        assert_eq!(view.failure_class, Some(FailureClass::HarnessCrash));
        let st = view.status.expect("applied");
        assert_eq!(st.status, crate::state::RunStatus::Stopped);
        assert_eq!(st.phase, crate::workflow::graph::PHASE_IMPLEMENT);
        assert_eq!(st.failure_class, Some(FailureClass::HarnessCrash));
        assert!(st.review.is_none() || st.review.as_ref().unwrap().attempted.is_empty());
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn apply_turn_mixed_spawn_stays_success() {
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
            Some("0032".into()),
            crate::workflow::WorkflowDriver::Adapter,
        )
        .unwrap();
        crate::state::with_run_state_lock(&rec, || {
            let mut s = crate::state::load_run_state(&rec)?;
            s.phase = crate::workflow::graph::PHASE_IMPLEMENT.into();
            crate::state::save_run_state(&rec, &s)
        })
        .unwrap();

        let turn = Ok(PromptResult {
            text: "mixed".into(),
            stop_reason: Some("end_turn".into()),
        });
        let view = apply_turn(
            &rec,
            turn,
            harness_stub(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally {
                attempts: 2,
                ok: 1,
                last_error: Some("terminal/create spawn: program not found".into()),
            },
        )
        .await
        .unwrap();
        assert!(view.applied);
        assert!(view.failure_class.is_none());
        let st = view.status.expect("applied");
        assert_eq!(st.status, crate::state::RunStatus::Running);
        assert_eq!(st.phase, crate::workflow::graph::PHASE_CROSS_MODEL);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn prompt_all_terminal_create_spawn_fail_stops_implement() {
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
            Some("0032".into()),
            crate::workflow::WorkflowDriver::Adapter,
        )
        .unwrap();
        crate::state::with_run_state_lock(&rec, || {
            let mut s = crate::state::load_run_state(&rec)?;
            s.phase = crate::workflow::graph::PHASE_IMPLEMENT.into();
            crate::state::save_run_state(&rec, &s)
        })
        .unwrap();

        let mut lines = mock_handshake_ok("sess-spawn-dead");
        lines.push(crate::harness::terminal::terminal_create(
            99,
            "sess-spawn-dead",
            "__coord_no_such_bin_0032__",
            &["x"],
        ));
        lines.push(rpc_result(4, json!({ "stopReason": "end_turn" })));
        let session = GrokSession::start_mock(
            crate::harness::grok_cwd(&rec).expect("cwd"),
            lines,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        insert_test_session(rec.id.clone(), session).await;

        let view = prompt(Some(&rec.id), "go".into(), false).await.unwrap();
        assert!(view.applied);
        assert_eq!(view.failure_class, Some(FailureClass::HarnessCrash));
        let st = view.status.expect("applied");
        assert_eq!(st.status, crate::state::RunStatus::Stopped);
        assert_eq!(st.phase, crate::workflow::graph::PHASE_IMPLEMENT);
        assert!(st.review.is_none() || st.review.as_ref().unwrap().attempted.is_empty());
        let _ = shutdown(Some(&rec.id), false).await;
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    fn holder_shared(rec: &ProjectRecord, session: GrokSession) -> std::sync::Arc<HolderShared> {
        let snapshot = session_status(&session);
        let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
        std::sync::Arc::new(HolderShared {
            session: tokio::sync::Mutex::new(session),
            prompt_gate: tokio::sync::Mutex::new(()),
            rec: rec.clone(),
            snapshot: std::sync::Mutex::new(snapshot),
            shutdown_tx,
        })
    }

    fn prepared_fixture() -> (tempfile::TempDir, ProjectRecord, PreparedInjection) {
        let dir = tempdir().unwrap();
        let product = dir.path().join("product");
        std::fs::create_dir_all(&product).unwrap();
        for args in [
            &["init", "-b", "main"][..],
            &["add", "f.txt"][..],
            &["commit", "-m", "base"][..],
        ] {
            if args[0] == "add" {
                std::fs::write(product.join("f.txt"), "base\n").unwrap();
            }
            let out = crate::git_command::command()
                .args(args)
                .current_dir(&product)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        }
        let conductor = dir.path().join("conductor");
        std::fs::create_dir_all(conductor.join("0099-Example")).unwrap();
        let write_row = |cell: &str| {
            std::fs::write(
                conductor.join("conductor.md"),
                format!(
                    "| Track | Execution | Status | Summary |\n| --- | --- | --- | --- |\n| [0099-Example](0099-Example/spec.md) | `{cell}` | **Ready - not started** | target |\n"
                ),
            )
            .unwrap();
        };
        write_row(&product.display().to_string());
        let rec = ProjectRecord {
            id: "p-0099".into(),
            path: dir.path().to_path_buf(),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: Some(product),
            execution_repos: std::collections::BTreeMap::new(),
            state_dir: Some(dir.path().join("state")),
            auto_merge: true,
            phase_timeouts_secs: std::collections::BTreeMap::new(),
            notify_progress: false,
            worktree_isolation: false,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            ci_fix_routing: false,
            review_comment_routing: false,
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now(),
        };
        let mut state = crate::state::RunState::idle(&rec.id);
        state.status = RunStatus::Running;
        state.driver = crate::workflow::WorkflowDriver::Adapter;
        state.phase = crate::workflow::graph::PHASE_IMPLEMENT.into();
        state.track_id = Some("0099-Example".into());
        state.run_epoch = 1;
        crate::state::save_run_state(&rec, &state).unwrap();
        let target = crate::workflow::execution_repo::resolve_track_target(
            &rec,
            &state,
            crate::workflow::graph::PHASE_IMPLEMENT,
        )
        .unwrap();
        (dir, rec, PreparedInjection::for_target(target))
    }

    #[test]
    fn sample_skip_journal_records_capture_and_verdict_stages() {
        let (_dir, rec, prepared) = prepared_fixture();
        for reason in [
            "planning-only",
            "git-unreadable",
            "capture-write-failed",
            "unresolved",
            "cwd-misdirected",
        ] {
            journal_sample_skip(&rec, prepared.target(), reason);
        }
        let log = std::fs::read_to_string(crate::progress_log::path(&rec)).unwrap();
        let mut last = 0;
        for reason in [
            "planning-only",
            "git-unreadable",
            "capture-write-failed",
            "unresolved",
            "cwd-misdirected",
        ] {
            let needle = format!(
                "leg sample skipped: track=0099-Example source=row_execution reason={reason}"
            );
            let at = log
                .find(&needle)
                .unwrap_or_else(|| panic!("missing {needle}: {log}"));
            assert!(at >= last, "journal order changed: {log}");
            last = at;
        }
        assert!(log.contains("sample-skip"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn manual_prompt_in_session_cwd_does_not_fail_sibling_guard() {
        let _guard = test_env_lock();
        let prev_home = std::env::var_os(ENV_COORDINATOR_HOME);
        let home = tempdir().unwrap();
        unsafe { std::env::set_var(ENV_COORDINATOR_HOME, home.path()) };
        let proj = tempdir().unwrap();
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        // An active sibling-target implement leg.
        let conductor = proj.path().join("conductor");
        std::fs::create_dir_all(conductor.join("0070-Example")).unwrap();
        let sibling = tempdir().unwrap();
        std::fs::write(
            conductor.join("conductor.md"),
            format!(
                "| Track | Execution | Status | Summary |\n| --- | --- | --- | --- |\n| [0070-Example](0070-Example/spec.md) | `{}` | **Ready - not started** | s |\n",
                sibling.path().display()
            ),
        )
        .unwrap();
        crate::run::run_with_driver(
            &rec,
            Some("0070".into()),
            crate::workflow::WorkflowDriver::Adapter,
        )
        .unwrap();
        crate::state::with_run_state_lock(&rec, || {
            let mut state = crate::state::load_run_state(&rec)?;
            state.phase = crate::workflow::graph::PHASE_IMPLEMENT.into();
            state.driver = crate::workflow::WorkflowDriver::Adapter;
            state.last_driven_phase = Some(crate::workflow::graph::PHASE_IMPLEMENT.into());
            crate::state::save_run_state(&rec, &state)
        })
        .unwrap();
        let baseline = crate::state::resolve_state_dir(&rec)
            .unwrap()
            .join("leg-baseline.json");
        assert!(!baseline.exists());
        // A baseline from an earlier adapter leg must not be overwritten by a manual turn.
        std::fs::create_dir_all(baseline.parent().unwrap()).unwrap();
        std::fs::write(&baseline, b"sentinel-baseline").unwrap();
        let before = std::fs::read(&baseline).unwrap();
        let mut lines = mock_handshake_ok("sess-manual");
        lines.push(rpc_result(4, json!({ "stopReason": "end_turn" })));
        let session = GrokSession::start_mock(
            crate::harness::grok_cwd(&rec).expect("cwd"),
            lines,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        insert_test_session(rec.id.clone(), session).await;

        let view = prompt(Some(&rec.id), "manual operator work".into(), false)
            .await
            .unwrap();
        assert_eq!(view.failure_class, None);
        assert_eq!(
            std::fs::read(&baseline).unwrap(),
            before,
            "manual prompt must not capture or overwrite a baseline"
        );
        assert!(
            !crate::progress_log::path(&rec).exists()
                || !std::fs::read_to_string(crate::progress_log::path(&rec))
                    .unwrap_or_default()
                    .contains("sample-skip")
        );
        let _ = shutdown(Some(&rec.id), false).await;
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var(ENV_COORDINATOR_HOME, v),
                None => std::env::remove_var(ENV_COORDINATOR_HOME),
            }
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn inprocess_prepared_route_captures_and_verdicts() {
        let _guard = test_env_lock();
        let prev_home = std::env::var_os(ENV_COORDINATOR_HOME);
        let home = tempdir().unwrap();
        unsafe { std::env::set_var(ENV_COORDINATOR_HOME, home.path()) };
        let proj = tempdir().unwrap();
        let product = proj.path().join("product");
        std::fs::create_dir_all(&product).unwrap();
        for args in [
            &["init", "-b", "main"][..],
            &["add", "f.txt"][..],
            &["commit", "-m", "base"][..],
        ] {
            if args[0] == "add" {
                std::fs::write(product.join("f.txt"), "base\n").unwrap();
            }
            let out = crate::git_command::command()
                .args(args)
                .current_dir(&product)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        }
        let conductor = proj.path().join("conductor");
        std::fs::create_dir_all(conductor.join("0070-Example")).unwrap();
        std::fs::write(
            conductor.join("conductor.md"),
            format!(
                "| Track | Execution | Status | Summary |\n| --- | --- | --- | --- |\n| [0070-Example](0070-Example/spec.md) | `{}` | **Ready - not started** | s |\n",
                product.display()
            ),
        )
        .unwrap();
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.projects[0].execution_repo = Some(product);
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        crate::run::run_with_driver(
            &rec,
            Some("0070".into()),
            crate::workflow::WorkflowDriver::Adapter,
        )
        .unwrap();
        let state = crate::state::with_run_state_lock(&rec, || {
            let mut state = crate::state::load_run_state(&rec)?;
            state.phase = crate::workflow::graph::PHASE_IMPLEMENT.into();
            state.driver = crate::workflow::WorkflowDriver::Adapter;
            state.last_driven_phase = None;
            crate::state::save_run_state(&rec, &state)?;
            crate::state::load_run_state(&rec)
        })
        .unwrap();
        let target = crate::workflow::execution_repo::resolve_track_target(
            &rec,
            &state,
            crate::workflow::graph::PHASE_IMPLEMENT,
        )
        .unwrap();
        let prepared = PreparedInjection::for_target(target);
        let mut lines = mock_handshake_ok("sess-prepared-inproc");
        lines.push(rpc_result(4, json!({ "stopReason": "end_turn" })));
        let session = GrokSession::start_mock(
            crate::harness::grok_cwd(&rec).expect("cwd"),
            lines,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        insert_test_session(rec.id.clone(), session).await;

        let view = prompt_prepared(Some(&rec.id), "work".into(), false, prepared)
            .await
            .unwrap();
        assert!(view.applied);
        assert_eq!(view.failure_class, Some(FailureClass::HarnessCrash));
        assert!(
            view.error
                .unwrap_or_default()
                .contains("produced no changes")
        );
        let baseline = crate::state::resolve_state_dir(&rec)
            .unwrap()
            .join("leg-baseline.json");
        assert!(baseline.is_file(), "prepared route must capture");
        let log = std::fs::read_to_string(crate::progress_log::path(&rec)).unwrap_or_default();
        assert!(!log.contains("source=none"), "{log}");
        let _ = shutdown(Some(&rec.id), false).await;
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var(ENV_COORDINATOR_HOME, v),
                None => std::env::remove_var(ENV_COORDINATOR_HOME),
            }
        }
    }

    #[tokio::test]
    async fn holder_resolution_failure_prevents_inject() {
        let (dir, rec, prepared) = prepared_fixture();
        // An invalid explicit target: resolution fails before any holder/session prompt.
        std::fs::write(
            dir.path().join("conductor/conductor.md"),
            "| Track | Execution | Status | Summary |\n| --- | --- | --- | --- |\n| [0099-Example](0099-Example/spec.md) | `not-a-target or key` | **Ready - not started** | s |\n",
        )
        .unwrap();
        let session = GrokSession::start_mock(
            rec.path.clone(),
            crate::harness::grok::mock_handshake_ok("sess-res-fail"),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        let shared = holder_shared(&rec, session);
        let (resp, _) = handle_prepared_prompt(&rec, &shared, "work".into(), prepared)
            .await
            .unwrap();
        assert_eq!(resp.skipped, Some(true));
        assert_eq!(resp.applied, Some(false));
        let session = shared.session.lock().await;
        assert!(session.turn_roots().is_empty());
        // Only the handshake was written; no session/prompt request exists.
        let written = session.mock_written().unwrap_or_default();
        assert!(
            !written.iter().any(|line| line.contains("session/prompt")),
            "{written:?}"
        );
    }

    #[tokio::test]
    async fn holder_prepared_route_captures_and_verdicts() {
        let (_dir, rec, prepared) = prepared_fixture();
        let target = prepared.target().unwrap().clone();
        let mut lines = crate::harness::grok::mock_handshake_ok("sess-holder-prepared");
        lines.push(crate::harness::grok::rpc_result(
            4,
            serde_json::json!({ "stopReason": "end_turn" }),
        ));
        let session =
            GrokSession::start_mock(target.prompt_path.clone(), lines, Duration::from_secs(2))
                .await
                .unwrap();
        let shared = holder_shared(&rec, session);
        let (resp, _) = handle_prepared_prompt(&rec, &shared, "work".into(), prepared)
            .await
            .unwrap();
        assert_eq!(resp.applied, Some(true));
        assert_eq!(resp.failure_class, Some(FailureClass::HarnessCrash));
        let baseline = crate::state::resolve_state_dir(&rec)
            .unwrap()
            .join("leg-baseline.json");
        assert!(baseline.is_file());
        let session = shared.session.lock().await;
        assert!(
            session.turn_roots().is_empty(),
            "roots cleared after the turn"
        );
    }

    #[tokio::test]
    async fn holder_manual_prompt_leaves_baseline_and_journal_untouched() {
        let (_dir, rec, _prepared) = prepared_fixture();
        let target = crate::workflow::execution_repo::resolve_track_target(
            &rec,
            &crate::state::load_run_state(&rec).unwrap(),
            crate::workflow::graph::PHASE_IMPLEMENT,
        )
        .unwrap();
        let baseline = crate::state::resolve_state_dir(&rec)
            .unwrap()
            .join("leg-baseline.json");
        std::fs::write(&baseline, b"sentinel").unwrap();
        let before = std::fs::read(&baseline).unwrap();
        let mut lines = crate::harness::grok::mock_handshake_ok("sess-holder-manual");
        lines.push(crate::harness::grok::rpc_result(
            4,
            serde_json::json!({ "stopReason": "end_turn" }),
        ));
        let session =
            GrokSession::start_mock(target.prompt_path.clone(), lines, Duration::from_secs(2))
                .await
                .unwrap();
        let shared = holder_shared(&rec, session);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_shared = shared.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_hold_conn(stream, server_shared).await
        });
        let mut client = TcpStream::connect(addr).await.unwrap();
        let request = serde_json::to_string(&HoldRequest::Prompt {
            text: "manual operator work".into(),
        })
        .unwrap();
        client
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let response: HoldResponse = serde_json::from_str(&line).unwrap();
        assert!(response.ok, "{response:?}");
        let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
        assert_eq!(
            std::fs::read(&baseline).unwrap(),
            before,
            "baseline untouched"
        );
        let log = std::fs::read_to_string(crate::progress_log::path(&rec)).unwrap_or_default();
        assert!(!log.contains("sample-skip"), "{log}");
        let session = shared.session.lock().await;
        assert!(
            session
                .mock_written()
                .unwrap_or_default()
                .iter()
                .any(|line| line.contains("session/prompt")),
            "the manual prompt still reaches the harness"
        );
        assert!(session.turn_roots().is_empty());
    }

    #[tokio::test]
    async fn stale_snapshot_installs_no_target_root() {
        let (_dir, rec, prepared) = prepared_fixture();
        bump_epoch(&rec);
        let session = GrokSession::start_mock(
            rec.path.clone(),
            crate::harness::grok::mock_handshake_ok("sess-stale-root"),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        let shared = holder_shared(&rec, session);
        let (resp, _) = handle_prepared_prompt(&rec, &shared, "work".into(), prepared)
            .await
            .unwrap();
        assert_eq!(resp.skipped, Some(true));
        assert_eq!(resp.applied, Some(false));
        let session = shared.session.lock().await;
        assert!(session.turn_roots().is_empty());
        assert!(
            !session
                .mock_written()
                .unwrap_or_default()
                .iter()
                .any(|line| line.contains("session/prompt"))
        );
    }

    #[test]
    fn holder_applied_failure_is_a_structured_view_not_a_rejected_opcode() {
        let applied = HoldResponse {
            ok: false,
            error: Some("prepared turn failed".into()),
            text: None,
            stop_reason: None,
            status: None,
            harness: None,
            applied: Some(true),
            skipped: None,
            failure_class: Some(FailureClass::HarnessCrash),
            prepared_protocol: Some(PREPARED_PROTOCOL_VERSION),
            epoch_protocol: None,
        };
        assert!(structured_prepared_response(&applied));
        let skipped = HoldResponse {
            ok: true,
            error: None,
            text: None,
            stop_reason: None,
            status: None,
            harness: None,
            applied: Some(false),
            skipped: Some(true),
            failure_class: None,
            prepared_protocol: Some(PREPARED_PROTOCOL_VERSION),
            epoch_protocol: None,
        };
        assert!(structured_prepared_response(&skipped));
        let rejected = HoldResponse {
            ok: false,
            error: Some("unknown op".into()),
            text: None,
            stop_reason: None,
            status: None,
            harness: None,
            applied: None,
            skipped: None,
            failure_class: None,
            prepared_protocol: None,
            epoch_protocol: None,
        };
        assert!(!structured_prepared_response(&rejected));
    }

    #[test]
    fn old_holder_cannot_drop_prepared_identity() {
        let (_dir, _rec, prepared) = prepared_fixture();
        let json = serde_json::to_string(&prepared).unwrap();
        let back: PreparedInjection = serde_json::from_str(&json).unwrap();
        assert_eq!(back, prepared);
        let target = back.target().expect("target survives");
        assert_eq!(target.track_id, "0099-Example");
        assert!(target.sample_path.is_absolute());
    }

    #[test]
    fn stale_epoch_snapshot_skips_inject() {
        let (_dir, rec, prepared) = prepared_fixture();
        crate::state::with_run_state_lock(&rec, || {
            let mut state = crate::state::load_run_state(&rec)?;
            state.run_epoch += 1;
            crate::state::save_run_state(&rec, &state)
        })
        .unwrap();
        let reason = validate_prepared(&rec, &prepared).unwrap_err();
        assert_eq!(reason, "stale-injection");
    }

    #[test]
    fn verdict_revalidates_target_after_send() {
        let (dir, rec, prepared) = prepared_fixture();
        let state = crate::state::load_run_state(&rec).unwrap();
        let target = prepared.target().unwrap().clone();
        let baseline = crate::state::resolve_state_dir(&rec)
            .unwrap()
            .join("leg-baseline.json");
        let status = no_change::note_baseline(&rec, &state, &state.phase, &target);
        assert!(status.trust().is_trusted());
        let before = std::fs::read(&baseline).unwrap();
        let other = dir.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(
            dir.path().join("conductor/conductor.md"),
            format!(
                "| Track | Execution | Status | Summary |\n| --- | --- | --- | --- |\n| [0099-Example](0099-Example/spec.md) | `{}` | **Ready - not started** | target |\n",
                other.display()
            ),
        )
        .unwrap();
        assert_eq!(
            revalidate_target(&rec, &state, &state.phase, &target).unwrap_err(),
            "target-changed"
        );
        assert_eq!(std::fs::read(&baseline).unwrap(), before, "baseline kept");
    }

    #[test]
    fn row_edit_does_not_split_prompt_and_capture_target() {
        let (dir, rec, prepared) = prepared_fixture();
        let other = dir.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(
            dir.path().join("conductor/conductor.md"),
            format!(
                "| Track | Execution | Status | Summary |\n| --- | --- | --- | --- |\n| [0099-Example](0099-Example/spec.md) | `{}` | **Ready - not started** | target |\n",
                other.display()
            ),
        )
        .unwrap();
        let reason = validate_prepared(&rec, &prepared).unwrap_err();
        assert_eq!(reason, "target-changed");
        assert_eq!(
            prepared.target().unwrap().prompt_path,
            prepared.target().unwrap().declared_path
        );
    }

    #[test]
    fn old_holder_missing_capability_recycles_once_before_send() {
        assert_eq!(
            prepared_capability_plan(None, false),
            PreparedCapabilityPlan::RecycleOnce
        );
        assert_eq!(
            prepared_capability_plan(Some(0), false),
            PreparedCapabilityPlan::RecycleOnce
        );
        // The replacement is probed once more; still unsupported is terminal.
        assert_eq!(
            prepared_capability_plan(None, true),
            PreparedCapabilityPlan::Fail
        );
        assert_eq!(
            prepared_capability_plan(Some(PREPARED_PROTOCOL_VERSION), false),
            PreparedCapabilityPlan::Send
        );
        assert_eq!(
            prepared_capability_plan(Some(PREPARED_PROTOCOL_VERSION), true),
            PreparedCapabilityPlan::Send
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn old_holder_missing_capability_recycles_once_then_fails_without_prompt() {
        use tokio::io::AsyncReadExt;
        let _guard = test_env_lock();
        let prev_home = std::env::var_os(ENV_COORDINATOR_HOME);
        let home = tempdir().unwrap();
        unsafe { std::env::set_var(ENV_COORDINATOR_HOME, home.path()) };
        std::fs::write(
            home.path().join("config.json"),
            r#"{"version":1,"scan_roots":[],"role_bindings":{"implementor":{"harness":"grok","command":"missing-0099-holder.exe"}}}"#,
        )
        .unwrap();
        let (_dir, rec, prepared) = prepared_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = accepted.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut buf = vec![0u8; 4096];
            let _ = stream.read(&mut buf).await;
            let _ = stream.write_all(b"{\"ok\":true}\n").await;
        });
        let handle = PersistedGrokHandle {
            version: 1,
            model: None,
            bin: None,
            project_id: rec.id.clone(),
            session_id: Some("sess-old".into()),
            cwd: Some(rec.path.clone()),
            pid: None,
            holder_pid: None,
            control_addr: Some(addr.clone()),
            adapter: "grok".into(),
            supports_compact: true,
            alive: true,
            prompt_in_flight: false,
            error: None,
            context_usage: None,
            last_signal: None,
            last_tool_title: None,
        };
        save_persist(&rec, &handle).unwrap();
        let view = prompt_prepared_holder(&rec, "work".into(), prepared, &addr)
            .await
            .unwrap();
        assert!(view.applied);
        assert_eq!(view.failure_class, Some(FailureClass::HarnessCrash));
        let error = view.error.unwrap_or_default();
        assert!(error.contains("old holder shutdown failed"), "{error}");
        assert!(error.contains("failure resolve"), "{error}");
        let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
        assert_eq!(
            accepted.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "one capability probe, then no prepared prompt is sent"
        );
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var(ENV_COORDINATOR_HOME, v),
                None => std::env::remove_var(ENV_COORDINATOR_HOME),
            }
        }
    }

    #[test]
    fn replacement_holder_incompatible_stops_without_prompt() {
        let (_dir, rec, prepared) = prepared_fixture();
        let view = prepared_failure_after_recycle(
            &rec,
            prepared,
            "replacement holder does not support prepared prompts",
        )
        .unwrap();
        assert!(view.applied);
        assert_eq!(view.failure_class, Some(FailureClass::HarnessCrash));
        assert!(view.skipped.is_none());
        let state = crate::state::load_run_state(&rec).unwrap();
        assert_eq!(state.status, RunStatus::Stopped);
        assert_eq!(state.failure_class, Some(FailureClass::HarnessCrash));
    }

    fn bump_epoch(rec: &ProjectRecord) {
        crate::state::with_run_state_lock(rec, || {
            let mut state = crate::state::load_run_state(rec)?;
            state.run_epoch += 1;
            crate::state::save_run_state(rec, &state)
        })
        .unwrap();
    }

    #[tokio::test]
    async fn mid_turn_epoch_advance_skips_outcome() {
        let (_dir, rec, prepared) = prepared_fixture();
        bump_epoch(&rec);
        let success = apply_turn_checked(
            &rec,
            Ok(PromptResult {
                text: "done".into(),
                stop_reason: Some("end_turn".into()),
            }),
            GrokHarnessStatus::missing(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
            None,
            Some(&prepared),
            None,
            false,
        )
        .await
        .unwrap();
        assert!(!success.applied);
        assert_eq!(success.skipped, Some(true));
        let state = crate::state::load_run_state(&rec).unwrap();
        assert_eq!(state.status, RunStatus::Running);
        assert_eq!(state.run_epoch, 2);

        let error = apply_turn_checked(
            &rec,
            Err(CoordinatorError::Message("transport died".into())),
            GrokHarnessStatus::missing(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
            None,
            Some(&prepared),
            None,
            false,
        )
        .await
        .unwrap();
        assert!(!error.applied);
        assert_eq!(error.skipped, Some(true));
    }

    #[tokio::test]
    async fn mid_turn_stop_discards_outcome() {
        let (_dir, rec, prepared) = prepared_fixture();
        crate::state::with_run_state_lock(&rec, || {
            let mut state = crate::state::load_run_state(&rec)?;
            state.status = RunStatus::Stopped;
            crate::state::save_run_state(&rec, &state)
        })
        .unwrap();
        let view = apply_turn_checked(
            &rec,
            Ok(PromptResult {
                text: "done".into(),
                stop_reason: Some("end_turn".into()),
            }),
            GrokHarnessStatus::missing(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
            None,
            Some(&prepared),
            None,
            false,
        )
        .await
        .unwrap();
        assert!(!view.applied);
        assert_eq!(view.skipped, Some(true));
    }

    #[tokio::test]
    async fn changed_track_skips_outcome() {
        let (_dir, rec, prepared) = prepared_fixture();
        crate::state::with_run_state_lock(&rec, || {
            let mut state = crate::state::load_run_state(&rec)?;
            state.track_id = Some("0404-Other".into());
            crate::state::save_run_state(&rec, &state)
        })
        .unwrap();
        let view = apply_turn_checked(
            &rec,
            Ok(PromptResult {
                text: "done".into(),
                stop_reason: Some("end_turn".into()),
            }),
            GrokHarnessStatus::missing(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
            None,
            Some(&prepared),
            None,
            false,
        )
        .await
        .unwrap();
        assert!(!view.applied);
        assert_eq!(view.skipped, Some(true));
    }

    #[tokio::test]
    async fn epoch_advance_at_apply_lock_skips_without_new_run_artifact() {
        let (_dir, rec, prepared) = prepared_fixture();
        bump_epoch(&rec);
        let outcome = PhaseOutcome::success(
            crate::workflow::graph::PHASE_IMPLEMENT,
            OutcomeSource::Adapter,
            Some("done".into()),
            None,
            Some(prepared.run_epoch),
        );
        let applied = crate::outcome::write_and_apply_prepared(&rec, outcome, &prepared).unwrap();
        assert!(matches!(applied, ApplyIdentity::Stale(_)));
        let state = crate::state::load_run_state(&rec).unwrap();
        assert_eq!(state.status, RunStatus::Running);
        assert_eq!(state.run_epoch, 2);
        assert!(state.last_applied_outcome_hash.is_none());
    }

    #[test]
    fn poll_try_apply_consumes_stale_disposition() {
        let (_dir, rec, prepared) = prepared_fixture();
        let outcome = PhaseOutcome::success(
            crate::workflow::graph::PHASE_IMPLEMENT,
            OutcomeSource::Adapter,
            Some("done".into()),
            None,
            Some(prepared.run_epoch),
        );
        crate::outcome::save_current_outcome(&rec, &outcome).unwrap();
        bump_epoch(&rec);
        let applied = crate::outcome::poll_try_apply(&rec, outcome).unwrap();
        assert!(applied.is_none());
        assert!(!crate::outcome::outcome_current_path(&rec).unwrap().exists());
    }

    #[tokio::test]
    async fn rejected_prepared_opcode_or_closed_response_never_retries_prompt() {
        use tokio::io::AsyncReadExt;
        let (_dir, rec, prepared) = prepared_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = accepted.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut buf = vec![0u8; 4096];
            let _ = stream.read(&mut buf).await;
            // Close without a response: uncertain delivery after send.
        });
        let view = send_prepared_rpc(&rec, "work".into(), prepared, &addr.to_string())
            .await
            .unwrap();
        assert!(view.applied, "one identity-checked failure is applied");
        assert_eq!(view.failure_class, Some(FailureClass::HarnessCrash));
        let error = view.error.unwrap_or_default();
        assert!(error.contains("transport failed after send"), "{error}");
        assert!(error.contains("no automatic retry"), "{error}");
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), server).await;
        assert_eq!(
            accepted.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no resend"
        );
        if let Ok(Some(handle)) = load_persist(&rec) {
            assert!(!handle.alive, "the holder is refused for reuse");
        }
        let state = crate::state::load_run_state(&rec).unwrap();
        assert_eq!(state.status, RunStatus::Stopped);
    }

    fn block_turn(text: &str) -> std::result::Result<PromptResult, CoordinatorError> {
        Ok(PromptResult {
            text: text.into(),
            stop_reason: Some("end_turn".into()),
        })
    }

    fn status_md(rec: &ProjectRecord) -> String {
        std::fs::read_to_string(crate::progress_log::path(rec)).unwrap_or_default()
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn unprepared_plan_block_stops_same_phase_with_one_artifact() {
        let _guard = test_env_lock();
        let (rec, _home, _proj) = implement_leg_fixture(crate::workflow::graph::PHASE_PLAN);
        let state = crate::state::load_run_state(&rec).unwrap();
        assert!(state.delivery_target.is_none());
        let identity = PreparedInjection::legacy(&state);
        let view = apply_turn_checked(
            &rec,
            block_turn("work done\nBlocked: the repository needs a credential"),
            harness_stub(),
            crate::workflow::graph::PHASE_PLAN,
            SpawnTally::default(),
            None,
            None,
            Some(&identity),
            false,
        )
        .await
        .unwrap();
        assert!(view.applied);
        assert_eq!(view.failure_class, Some(FailureClass::Blocked));
        let st = crate::state::load_run_state(&rec).unwrap();
        assert_eq!(st.status, RunStatus::Stopped);
        assert_eq!(st.failure_class, Some(FailureClass::Blocked));
        assert_eq!(
            st.phase,
            crate::workflow::graph::PHASE_PLAN,
            "never advances"
        );
        assert!(
            crate::notify::artifact::existing_path(&rec).is_some(),
            "one Failure Artifact"
        );
        let log = status_md(&rec);
        assert_eq!(log.matches("  fail  ").count(), 1, "{log}");
        assert!(log.contains("phase=plan"), "{log}");
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    async fn cancelled_turn_never_classifies_a_block() {
        let (_dir, rec, prepared) = prepared_fixture();
        let turn = Ok(PromptResult {
            text: "Blocked: the turn was cancelled".into(),
            stop_reason: Some("cancelled".into()),
        });
        let view = apply_turn_checked(
            &rec,
            turn,
            GrokHarnessStatus::missing(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
            None,
            Some(&prepared),
            None,
            false,
        )
        .await
        .unwrap();
        assert!(!view.applied);
        assert_eq!(view.skipped, Some(true));
        let st = crate::state::load_run_state(&rec).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert!(st.failure_class.is_none());
    }

    #[tokio::test]
    async fn block_overrides_no_change_and_precedes_self_check() {
        let (_dir, mut rec, prepared) = prepared_fixture();
        rec.self_continuation = true;
        let text = "Blocked: prerequisite missing\nself-check: continue finding=f action=repair";
        let view = apply_turn_checked(
            &rec,
            block_turn(text),
            GrokHarnessStatus::missing(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
            Some("no change detected".into()),
            Some(&prepared),
            None,
            false,
        )
        .await
        .unwrap();
        assert!(view.applied);
        assert_eq!(view.failure_class, Some(FailureClass::Blocked));
        let st = crate::state::load_run_state(&rec).unwrap();
        assert_eq!(st.status, RunStatus::Stopped);
        assert_eq!(st.failure_class, Some(FailureClass::Blocked));
        assert!(st.self_check.is_none(), "block precedes self-check");
    }

    #[tokio::test]
    async fn harness_error_text_precedes_block() {
        let (_dir, rec, prepared) = prepared_fixture();
        let text = "Blocked: a reason\nError: RetriableError from the harness";
        let view = apply_turn_checked(
            &rec,
            block_turn(text),
            GrokHarnessStatus::missing(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
            None,
            Some(&prepared),
            None,
            false,
        )
        .await
        .unwrap();
        assert!(view.applied);
        assert_eq!(view.failure_class, Some(FailureClass::HarnessCrash));
        let st = crate::state::load_run_state(&rec).unwrap();
        assert_eq!(st.failure_class, Some(FailureClass::HarnessCrash));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn stale_unprepared_block_is_a_skip_journaling_the_frozen_track() {
        let _guard = test_env_lock();
        let (rec, _home, _proj) = implement_leg_fixture(crate::workflow::graph::PHASE_PLAN);
        crate::state::with_run_state_lock(&rec, || {
            let mut st = crate::state::load_run_state(&rec)?;
            st.track_id = Some("0098".into());
            crate::state::save_run_state(&rec, &st)
        })
        .unwrap();
        let state = crate::state::load_run_state(&rec).unwrap();
        let identity = PreparedInjection::legacy(&state);
        bump_epoch(&rec);
        let view = apply_turn_checked(
            &rec,
            block_turn("Blocked: old epoch"),
            harness_stub(),
            crate::workflow::graph::PHASE_PLAN,
            SpawnTally::default(),
            None,
            None,
            Some(&identity),
            false,
        )
        .await
        .unwrap();
        assert!(!view.applied);
        assert_eq!(view.skipped, Some(true));
        assert_eq!(view.text.as_deref(), Some("Blocked: old epoch"));
        assert!(crate::notify::artifact::existing_path(&rec).is_none());
        let log = status_md(&rec);
        assert!(log.contains("track=0098"), "{log}");
        assert!(!log.contains("track=(none)"), "{log}");
        let st = crate::state::load_run_state(&rec).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.run_epoch, state.run_epoch + 1);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn manual_prompt_into_stopped_keeps_text_and_applies_nothing() {
        let _guard = test_env_lock();
        let (rec, _home, _proj) = implement_leg_fixture(crate::workflow::graph::PHASE_PLAN);
        crate::state::with_run_state_lock(&rec, || {
            let mut st = crate::state::load_run_state(&rec)?;
            st.status = RunStatus::Stopped;
            crate::state::save_run_state(&rec, &st)
        })
        .unwrap();
        let view = apply_turn_checked(
            &rec,
            block_turn("Blocked: should not apply"),
            harness_stub(),
            crate::workflow::graph::PHASE_PLAN,
            SpawnTally::default(),
            None,
            None,
            None,
            false,
        )
        .await
        .unwrap();
        assert!(!view.applied);
        assert_eq!(view.skipped, Some(true));
        assert_eq!(view.text.as_deref(), Some("Blocked: should not apply"));
        let st = crate::state::load_run_state(&rec).unwrap();
        assert_eq!(st.status, RunStatus::Stopped);
        assert!(crate::notify::artifact::existing_path(&rec).is_none());
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    async fn manual_prepared_block_applies_with_target_none_and_no_mutation() {
        let (_dir, rec, _prepared) = prepared_fixture();
        let state = crate::state::load_run_state(&rec).unwrap();
        assert!(state.delivery_target.is_none());
        let identity = PreparedInjection::legacy(&state);
        assert!(identity.target().is_none());
        let view = apply_turn_checked(
            &rec,
            block_turn("Blocked: missing prerequisite"),
            GrokHarnessStatus::missing(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
            None,
            None,
            Some(&identity),
            true,
        )
        .await
        .unwrap();
        assert!(view.applied);
        let st = crate::state::load_run_state(&rec).unwrap();
        assert_eq!(st.status, RunStatus::Stopped);
        assert_eq!(st.failure_class, Some(FailureClass::Blocked));
        assert!(st.delivery_target.is_none(), "block is not a delivery gate");
    }

    #[tokio::test]
    async fn non_block_manual_prepared_turn_applies_nothing() {
        let (_dir, rec, _prepared) = prepared_fixture();
        let state = crate::state::load_run_state(&rec).unwrap();
        let identity = PreparedInjection::legacy(&state);
        let view = apply_turn_checked(
            &rec,
            block_turn("ordinary manual prompt"),
            GrokHarnessStatus::missing(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
            None,
            None,
            Some(&identity),
            true,
        )
        .await
        .unwrap();
        assert!(!view.applied);
        assert_eq!(view.skipped, Some(true));
        assert_eq!(view.text.as_deref(), Some("ordinary manual prompt"));
        let st = crate::state::load_run_state(&rec).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, crate::workflow::graph::PHASE_IMPLEMENT);
    }

    #[tokio::test]
    async fn prepared_block_with_moved_target_uses_frozen_identity() {
        let (_dir, rec, prepared) = prepared_fixture();
        crate::state::with_run_state_lock(&rec, || {
            let mut st = crate::state::load_run_state(&rec)?;
            st.delivery_target = Some(ResolvedExecution {
                declared_path: std::path::PathBuf::from(r"C:\dev\other"),
                prompt_path: std::path::PathBuf::from(r"C:\dev\other"),
                sample_path: std::path::PathBuf::from(r"C:\dev\other"),
                ..prepared.target().unwrap().clone()
            });
            crate::state::save_run_state(&rec, &st)
        })
        .unwrap();
        let view = apply_turn_checked(
            &rec,
            block_turn("Blocked: moved row"),
            GrokHarnessStatus::missing(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
            None,
            Some(&prepared),
            None,
            false,
        )
        .await
        .unwrap();
        assert!(view.applied);
        let st = crate::state::load_run_state(&rec).unwrap();
        assert_eq!(st.status, RunStatus::Stopped);
        assert_eq!(
            st.delivery_target.as_ref().map(|t| t.declared_path.clone()),
            Some(std::path::PathBuf::from(r"C:\dev\other")),
            "block does not rewrite delivery_target"
        );
    }

    /// 0107 DoD-1 production integration: the synchronous reaper RPC must round-trip
    /// against the real `hold_accept_loop` (real Ping/Shutdown framing), not only the
    /// scripted holder. Bounded persist wait and bounded join, matching the 2 s
    /// precedent in `holder_cancel_and_status_return_during_hung_prompt`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)]
    async fn holder_rpc_sync_round_trips_with_real_accept_loop() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        let _env = crate::config::TestEnvGuard::capture(&[
            ENV_COORDINATOR_HOME,
            crate::config::ENV_COORDINATOR_STATE_DIR,
        ]);
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::remove_var(crate::config::ENV_COORDINATOR_STATE_DIR);
        }
        let rec = Registry::default()
            .add(proj.path(), ProjectAddOptions::default())
            .unwrap();
        let session = GrokSession::start_mock(
            crate::harness::grok_cwd(&rec).expect("cwd"),
            mock_handshake_ok("sess-sync"),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        let rec_hold = rec.clone();
        let hold = tokio::spawn(async move { hold_loop_with_session(rec_hold, session).await });

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(Some(h)) = load_persist(&rec)
                && h.control_addr.is_some()
            {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("holder persist never became ready");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let rec_identity = rec.clone();
        let (identity, _mtime) =
            tokio::task::spawn_blocking(move || persist_identity(&rec_identity))
                .await
                .expect("identity join")
                .expect("identity read")
                .expect("identity present");
        let rec_prove = rec.clone();
        let want = identity.clone();
        let verified = tokio::task::spawn_blocking(move || {
            prove_holder(&rec_prove, &want, Duration::from_millis(750))
        })
        .await
        .expect("prove join")
        .expect("identity must be proven over the real accept loop");
        assert_eq!(
            verified.want.session_id.as_deref(),
            identity.session_id.as_deref()
        );

        let outcome =
            tokio::task::spawn_blocking(move || terminate_verified(verified, Intent::Reap))
                .await
                .expect("shutdown join");
        assert!(
            matches!(outcome, HolderTermination::Delivered { .. }),
            "the real holder must answer its own Shutdown: {outcome:?}"
        );

        tokio::time::timeout(Duration::from_secs(2), hold)
            .await
            .expect("holder join must complete")
            .expect("holder task")
            .expect("holder loop");
        let dead = load_persist(&rec).unwrap().expect("persist present");
        assert!(!dead.alive);
        assert!(dead.control_addr.is_none());
    }

    // ---- 0117: fixtures ------------------------------------------------------

    fn record_in(dir: &tempfile::TempDir, id: &str) -> ProjectRecord {
        ProjectRecord {
            id: id.into(),
            path: dir.path().to_path_buf(),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: Default::default(),
            state_dir: Some(dir.path().join("state")),
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
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn make_handle(
        rec: &ProjectRecord,
        session: Option<&str>,
        cwd: Option<PathBuf>,
        addr: Option<String>,
        holder_pid: Option<u32>,
        alive: bool,
        prompt: bool,
        error: Option<String>,
    ) -> PersistedGrokHandle {
        PersistedGrokHandle {
            version: 1,
            model: None,
            bin: None,
            project_id: rec.id.clone(),
            session_id: session.map(str::to_string),
            cwd,
            pid: Some(4242),
            holder_pid,
            control_addr: addr,
            adapter: "grok".into(),
            supports_compact: true,
            alive,
            prompt_in_flight: prompt,
            error,
            context_usage: None,
            last_signal: None,
            last_tool_title: None,
        }
    }

    fn attempt_handle(rec: &ProjectRecord, error: Option<&str>) -> PersistedGrokHandle {
        make_handle(
            rec,
            None,
            None,
            None,
            None,
            false,
            false,
            error.map(str::to_string),
        )
    }

    fn write_handle(rec: &ProjectRecord, handle: &PersistedGrokHandle) {
        save_persist(rec, handle).unwrap();
    }

    fn ref_of(rec: &ProjectRecord) -> PersistRef {
        load_ref(rec).unwrap().expect("persist present").1
    }

    fn file_bytes(rec: &ProjectRecord) -> Vec<u8> {
        std::fs::read(persist_path(rec).unwrap()).unwrap()
    }

    fn set_file_mtime(rec: &ProjectRecord, at: SystemTime) {
        std::fs::OpenOptions::new()
            .write(true)
            .open(persist_path(rec).unwrap())
            .unwrap()
            .set_modified(at)
            .unwrap();
    }

    fn age_file(rec: &ProjectRecord, age: Duration) {
        set_file_mtime(rec, SystemTime::now() - age);
    }

    /// Register `rec` in an isolated machine home so the CLI-resolution paths
    /// (`shutdown`, `start_with_bin`) can find it. Returns the home/temp guard.
    fn register_rec(rec: &ProjectRecord) -> (tempfile::TempDir, crate::config::TestEnvGuard) {
        let home = tempdir().unwrap();
        let guard = crate::config::TestEnvGuard::capture(&[ENV_COORDINATOR_HOME]);
        unsafe { std::env::set_var(ENV_COORDINATOR_HOME, home.path()) };
        let mut reg = Registry::default();
        reg.projects.push(rec.clone());
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        (home, guard)
    }

    fn mock_session_for(rec: &ProjectRecord, session_id: &str) -> GrokSession {
        futures_lite_block_on(GrokSession::start_mock(
            crate::harness::grok_cwd(rec).expect("cwd"),
            mock_handshake_ok(session_id),
            Duration::from_secs(2),
        ))
        .unwrap()
    }

    /// Tiny blocking test helper: these fence tests are `#[test]` and need one mock
    /// session; a current-thread runtime is enough.
    fn futures_lite_block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    // ---- 0117: the exclusive start claim + attempt fences --------------------

    #[test]
    fn claim_start_is_exclusive() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "claim-exclusive");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let rec = rec.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                claim_start(&rec, "grok")
            }));
        }
        let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            1,
            "exactly one caller owns the absent->pending claim"
        );
        let loser = results
            .iter()
            .find(|r| r.is_err())
            .and_then(|r| r.as_ref().err())
            .map(|e| e.to_string())
            .unwrap();
        assert!(loser.contains("persist-appeared"), "{loser}");
        let (identity, _) = persist_identity(&rec).unwrap().unwrap();
        assert!(identity.is_attempt(), "the winner wrote the pending record");
        assert!(identity.error.is_none());
        let before = file_bytes(&rec);
        assert!(claim_start(&rec, "grok").is_err());
        assert_eq!(
            before,
            file_bytes(&rec),
            "a loser never removes the winner's bytes"
        );
    }

    #[test]
    fn claim_start_refuses_an_existing_persist() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "claim-existing");
        write_handle(
            &rec,
            &make_handle(
                &rec,
                Some("sess-live"),
                Some(crate::harness::grok_cwd(&rec).expect("cwd")),
                Some("127.0.0.1:9".into()),
                Some(4242),
                true,
                false,
                None,
            ),
        );
        let before = file_bytes(&rec);
        let err = claim_start(&rec, "grok").unwrap_err().to_string();
        assert!(err.contains("persist-appeared"), "{err}");
        assert_eq!(
            before,
            file_bytes(&rec),
            "the claim never truncates a record"
        );
    }

    #[test]
    fn start_claims_after_the_decision_not_before_it() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "claim-race");
        assert!(persist_identity(&rec).unwrap().is_none());
        // Another start wins between the decision and this claim.
        write_handle(&rec, &attempt_handle(&rec, None));
        let before = file_bytes(&rec);
        let err = claim_start(&rec, "grok").unwrap_err().to_string();
        assert!(err.contains("persist-appeared"), "{err}");
        assert_eq!(before, file_bytes(&rec));
    }

    #[test]
    fn same_record_distinguishes_pending_from_failed_start() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "same-record");

        write_handle(&rec, &attempt_handle(&rec, None));
        let pending = ref_of(&rec);
        write_handle(&rec, &attempt_handle(&rec, Some("boom")));
        let failed = ref_of(&rec);
        assert!(
            !same_record(&pending, &failed.identity, failed.mtime),
            "a failed start is not the pending attempt"
        );

        // Force distinct mtimes: the filesystem does not guarantee a new tick.
        set_file_mtime(&rec, failed.mtime + Duration::from_secs(1));
        let failed_later = ref_of(&rec);
        assert_ne!(failed.mtime, failed_later.mtime);
        assert!(!same_record(
            &failed,
            &failed_later.identity,
            failed_later.mtime
        ));

        // A named holder rewritten by itself (mtime changed) is still the same record.
        write_handle(
            &rec,
            &make_handle(
                &rec,
                Some("sess-1"),
                Some(crate::harness::grok_cwd(&rec).expect("cwd")),
                Some("127.0.0.1:9".into()),
                Some(4242),
                true,
                false,
                None,
            ),
        );
        let named = ref_of(&rec);
        set_file_mtime(&rec, named.mtime + Duration::from_secs(2));
        let rewritten = ref_of(&rec);
        assert!(same_record(&named, &rewritten.identity, rewritten.mtime));
    }

    #[test]
    fn start_error_write_is_fenced() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "start-error-fence");
        let claim = claim_start(&rec, "grok").unwrap();
        // A newer pending record owns the slot (forced distinct mtime).
        write_handle(&rec, &attempt_handle(&rec, None));
        set_file_mtime(&rec, claim.mtime + Duration::from_secs(1));
        let newer = ref_of(&rec);
        assert_ne!(claim.mtime, newer.mtime);
        let before = file_bytes(&rec);
        assert!(
            !mark_start_error(&rec, Some(&claim), "stale"),
            "a stale claim cannot write its error"
        );
        assert_eq!(before, file_bytes(&rec));

        // With the claim still on disk the failure is written.
        let _ = std::fs::remove_file(persist_path(&rec).unwrap());
        let own = claim_start(&rec, "grok").unwrap();
        assert!(mark_start_error(&rec, Some(&own), "spawn failed"));
        let (identity, _) = persist_identity(&rec).unwrap().unwrap();
        assert!(identity.error.as_deref() == Some("spawn failed"));
    }

    #[test]
    fn stale_child_error_write_is_fenced() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "child-error-fence");
        let a = claim_start(&rec, "grok").unwrap();
        assert!(mark_start_error(&rec, Some(&a), "holder failed"));
        let failed = ref_of(&rec);
        assert!(
            clear_persist_if(&rec, &failed),
            "the retry clears the aged failure"
        );

        let _b = claim_start(&rec, "grok").unwrap();
        set_file_mtime(&rec, a.mtime + Duration::from_secs(1));
        let b = ref_of(&rec);
        assert_ne!(a.mtime, b.mtime, "forced distinct attempt stamps");
        let before = file_bytes(&rec);
        assert!(
            !mark_start_error(&rec, Some(&a), "late child error"),
            "the times-out child cannot clobber the retry's claim"
        );
        assert_eq!(before, file_bytes(&rec));

        // The same child against its own live claim does write.
        let current = ref_of(&rec);
        assert!(clear_persist_if(&rec, &current));
        let own = claim_start(&rec, "grok").unwrap();
        assert!(mark_start_error(&rec, Some(&own), "late child error"));
    }

    #[tokio::test]
    async fn stale_child_ready_write_is_fenced_and_tears_down() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "child-ready-fence");
        let a = claim_start(&rec, "grok").unwrap();
        let _b = claim_start(&rec, "grok").unwrap_err();
        // Simulate the ordinary retry: the timed-out start's record is replaced.
        assert!(mark_start_error(&rec, Some(&a), "timed out"));
        let failed = ref_of(&rec);
        assert!(clear_persist_if(&rec, &failed));
        let _retry = claim_start(&rec, "grok").unwrap();
        set_file_mtime(&rec, a.mtime + Duration::from_secs(1));
        let before = file_bytes(&rec);

        let mut session = GrokSession::start_mock(
            crate::harness::grok_cwd(&rec).expect("cwd"),
            mock_handshake_ok("sess-child"),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        let err = publish_ready_or_teardown(&rec, &mut session, None, Some(&a))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("persist-changed"), "{err}");
        assert_eq!(
            before,
            file_bytes(&rec),
            "the refused ready write changes nothing"
        );
        assert_eq!(
            session.shutdown_probe(),
            1,
            "the child tore its own ACP session down"
        );
        assert!(
            crate::harness::abort::cancel_handle_for(&rec.id).is_none(),
            "the refused in-process ready write leaves no dead registration"
        );
        assert!(
            !global_pool().lock().await.contains(&rec.id),
            "no session inserted"
        );
    }

    #[test]
    fn unclaimed_hold_does_not_overwrite_an_attempt() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "unclaimed");
        let session = mock_session_for(&rec, "sess-direct");

        for error in [None, Some("prior failure")] {
            write_handle(&rec, &attempt_handle(&rec, error));
            let before = file_bytes(&rec);
            assert!(
                !mark_start_error(&rec, None, "direct failure"),
                "an unclaimed hold never writes over an attempt"
            );
            assert!(
                !write_holder_ready(&rec, &session, None, None),
                "an unclaimed hold never publishes over an attempt"
            );
            assert_eq!(before, file_bytes(&rec));
        }

        // A live/identified record is refused too.
        write_handle(
            &rec,
            &make_handle(
                &rec,
                Some("sess-live"),
                Some(crate::harness::grok_cwd(&rec).expect("cwd")),
                Some("127.0.0.1:9".into()),
                Some(4242),
                true,
                false,
                None,
            ),
        );
        let before = file_bytes(&rec);
        assert!(!write_holder_ready(&rec, &session, None, None));
        assert_eq!(before, file_bytes(&rec));

        // Absent and DeadAfterExit records are the direct-hold path.
        let _ = std::fs::remove_file(persist_path(&rec).unwrap());
        assert!(write_holder_ready(&rec, &session, None, None));

        write_handle(
            &rec,
            &make_handle(
                &rec,
                Some("sess-clean-exit"),
                Some(crate::harness::grok_cwd(&rec).expect("cwd")),
                None,
                Some(4242),
                false,
                false,
                None,
            ),
        );
        assert!(write_holder_ready(&rec, &session, None, None));
    }

    #[test]
    fn exit_persist_write_is_fenced() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "exit-fence");
        let session = mock_session_for(&rec, "sess-own");
        // Another session owns the record: the exit write is a no-op.
        write_handle(
            &rec,
            &make_handle(
                &rec,
                Some("sess-other"),
                Some(session.cwd.clone()),
                Some("127.0.0.1:9".into()),
                Some(4242),
                true,
                false,
                None,
            ),
        );
        let before = file_bytes(&rec);
        assert!(!write_holder_exit(&rec, &session));
        assert_eq!(before, file_bytes(&rec));

        // Its own identity: the truthful dead record is written.
        write_handle(
            &rec,
            &make_handle(
                &rec,
                Some(&session.session_id),
                Some(session.cwd.clone()),
                Some("127.0.0.1:9".into()),
                Some(std::process::id()),
                true,
                false,
                None,
            ),
        );
        assert!(write_holder_exit(&rec, &session));
        let (identity, _) = persist_identity(&rec).unwrap().unwrap();
        assert!(!identity.alive);
        assert!(identity.control_addr.is_none());
        assert_eq!(identity.session_id.as_deref(), Some("sess-own"));
    }

    #[test]
    fn set_prompt_in_flight_and_fidelity_do_not_clobber_a_newer_record() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "flag-fence");
        write_handle(
            &rec,
            &make_handle(
                &rec,
                Some("sess-newer"),
                Some(crate::harness::grok_cwd(&rec).expect("cwd")),
                Some("127.0.0.1:9".into()),
                Some(7),
                true,
                false,
                None,
            ),
        );
        set_prompt_in_flight(&rec, true);
        let (identity, _) = persist_identity(&rec).unwrap().unwrap();
        assert_eq!(identity.session_id.as_deref(), Some("sess-newer"));
        assert!(identity.prompt_in_flight);
        assert_eq!(identity.holder_pid, Some(7));
    }

    // ---- 0117: scripted control-socket holder --------------------------------

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum RawMode {
        Normal,
        PingNoResponse,
        ShutdownNoResponse,
        ShutdownRejected,
        PingNoHarness,
        PingMutatePrompt,
        PingMutateSession,
        PingRemovePersist,
        PingCorruptPersist,
        PingTouchMtime,
        PingPending,
        PingFailedStart,
        /// Answers `Shutdown` but never writes the exit record.
        ShutdownNoExit,
        /// Answers `Shutdown` after 120 ms, then writes the dead record.
        ShutdownDelayedExit,
        /// Rewrites the persist to a different live holder, then answers `Shutdown`.
        ShutdownMutate,
    }

    struct RawHolder {
        addr: String,
        ops: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl RawHolder {
        fn seen(&self) -> Vec<String> {
            self.ops.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }

        fn stop(&mut self) {
            if let Some(handle) = self.handle.take() {
                let sock = self.addr.parse::<std::net::SocketAddr>().ok();
                if let Some(sock) = sock
                    && let Ok(mut stream) =
                        std::net::TcpStream::connect_timeout(&sock, Duration::from_millis(200))
                {
                    use std::io::Write;
                    let _ = stream.write_all(b"{\"op\":\"stop-test\"}\n");
                }
                let _ = handle.join();
            }
        }
    }

    impl Drop for RawHolder {
        fn drop(&mut self) {
            self.stop();
        }
    }

    fn mutate_persist_json(path: &Option<PathBuf>, f: impl Fn(&mut serde_json::Value)) {
        let Some(path) = path else { return };
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(mut body) = serde_json::from_str::<serde_json::Value>(&text) else {
            return;
        };
        f(&mut body);
        let _ = std::fs::write(path, body.to_string());
    }

    fn raw_holder(
        persist: Option<PathBuf>,
        session: &str,
        cwd: &Path,
        holder_pid: u32,
        mode: RawMode,
    ) -> RawHolder {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let ops = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let ops_thread = ops.clone();
        let session = session.to_string();
        let cwd = cwd.to_path_buf();
        let _ = holder_pid;
        let handle = std::thread::spawn(move || {
            use std::io::{BufRead, BufReader, Write};
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let Ok(mut reader) = stream.try_clone().map(BufReader::new) else {
                    continue;
                };
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                let op = serde_json::from_str::<serde_json::Value>(line.trim())
                    .ok()
                    .and_then(|v| {
                        v.get("op")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                    })
                    .unwrap_or_default();
                ops_thread
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(op.clone());
                if op == "stop-test" {
                    break;
                }
                match op.as_str() {
                    "ping" => {
                        if mode == RawMode::PingNoResponse {
                            std::thread::sleep(Duration::from_millis(1200));
                            continue;
                        }
                        let before_mtime = persist
                            .as_ref()
                            .and_then(|p| std::fs::metadata(p).ok())
                            .and_then(|m| m.modified().ok());
                        match mode {
                            RawMode::PingMutatePrompt => {
                                mutate_persist_json(&persist, |b| {
                                    b["prompt_in_flight"] = serde_json::Value::Bool(true)
                                });
                            }
                            RawMode::PingMutateSession => {
                                mutate_persist_json(&persist, |b| {
                                    b["session_id"] = serde_json::Value::String("spoofed".into())
                                });
                            }
                            RawMode::PingRemovePersist => {
                                if let Some(path) = &persist {
                                    let _ = std::fs::remove_file(path);
                                }
                            }
                            RawMode::PingCorruptPersist => {
                                if let Some(path) = &persist {
                                    let _ = std::fs::write(path, b"{not json");
                                }
                            }
                            RawMode::PingTouchMtime => {
                                if let Some(path) = &persist {
                                    let later = SystemTime::now() + Duration::from_secs(5);
                                    if let Ok(file) =
                                        std::fs::OpenOptions::new().write(true).open(path)
                                    {
                                        let _ = file.set_modified(later);
                                    }
                                }
                            }
                            RawMode::PingPending => {
                                mutate_persist_json(&persist, |b| {
                                    b["alive"] = serde_json::Value::Bool(false);
                                    b["control_addr"] = serde_json::Value::Null;
                                    b["session_id"] = serde_json::Value::Null;
                                    b["cwd"] = serde_json::Value::Null;
                                    b["holder_pid"] = serde_json::Value::Null;
                                    b["error"] = serde_json::Value::Null;
                                });
                            }
                            RawMode::PingFailedStart => {
                                mutate_persist_json(&persist, |b| {
                                    b["alive"] = serde_json::Value::Bool(false);
                                    b["control_addr"] = serde_json::Value::Null;
                                    b["session_id"] = serde_json::Value::Null;
                                    b["cwd"] = serde_json::Value::Null;
                                    b["holder_pid"] = serde_json::Value::Null;
                                    b["error"] =
                                        serde_json::Value::String("ACP handshake failed".into());
                                });
                            }
                            _ => {}
                        }
                        // Preserve the mtime for every mutation except the explicit touch,
                        // so the arm under test (not "recently-active") is the one that fires.
                        if mode != RawMode::PingTouchMtime
                            && let (Some(path), Some(before)) = (&persist, before_mtime)
                            && let Ok(file) = std::fs::OpenOptions::new().write(true).open(path)
                        {
                            let _ = file.set_modified(before);
                        }
                        let body = if mode == RawMode::PingNoHarness {
                            serde_json::json!({ "ok": true })
                        } else {
                            serde_json::json!({
                                "ok": true,
                                "harness": {
                                    "alive": true,
                                    "session_id": session,
                                    "cwd": cwd,
                                    "pid": 4242,
                                    "adapter": "grok",
                                    "supports_compact": true,
                                }
                            })
                        };
                        let _ = stream.write_all(body.to_string().as_bytes());
                        let _ = stream.write_all(b"\n");
                        let _ = stream.flush();
                    }
                    "shutdown" => {
                        if mode == RawMode::ShutdownNoResponse {
                            std::thread::sleep(Duration::from_millis(1200));
                            continue;
                        }
                        if mode == RawMode::ShutdownRejected {
                            let _ = stream
                                .write_all(b"{\"ok\":false,\"error\":\"shutdown rejected\"}\n");
                            continue;
                        }
                        if mode == RawMode::ShutdownMutate {
                            mutate_persist_json(&persist, |b| {
                                b["session_id"] =
                                    serde_json::Value::String("sess-newer-holder".into());
                            });
                        }
                        if mode == RawMode::ShutdownDelayedExit {
                            std::thread::sleep(Duration::from_millis(120));
                        }
                        let _ = stream.write_all(b"{\"ok\":true}\n");
                        let _ = stream.flush();
                        if matches!(mode, RawMode::Normal | RawMode::ShutdownDelayedExit)
                            && let Some(path) = &persist
                        {
                            mutate_persist_json(&Some(path.clone()), |b| {
                                b["alive"] = serde_json::Value::Bool(false);
                                b["control_addr"] = serde_json::Value::Null;
                                b["prompt_in_flight"] = serde_json::Value::Bool(false);
                            });
                        }
                        if mode != RawMode::ShutdownNoExit && mode != RawMode::ShutdownMutate {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        });
        RawHolder {
            addr,
            ops,
            handle: Some(handle),
        }
    }

    /// Persist that matches `addr` for a raw holder.
    fn write_live_handle(rec: &ProjectRecord, session: &str, addr: &str) {
        write_handle(
            rec,
            &make_handle(
                rec,
                Some(session),
                Some(crate::harness::grok_cwd(rec).expect("cwd")),
                Some(addr.to_string()),
                Some(4242),
                true,
                false,
                None,
            ),
        );
    }

    fn prove_live(rec: &ProjectRecord) -> std::result::Result<VerifiedHolder, HolderTermination> {
        let (identity, _) = persist_identity(rec).unwrap().unwrap();
        prove_holder(rec, &identity, Duration::from_millis(750))
    }

    // ---- 0117: the delivery seam --------------------------------------------

    #[test]
    fn deliver_shutdown_is_the_only_shutdown() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "deliver-ok");
        let holder = raw_holder(
            Some(persist_path(&rec).unwrap()),
            "sess-d",
            &crate::harness::grok_cwd(&rec).expect("cwd"),
            4242,
            RawMode::Normal,
        );
        write_live_handle(&rec, "sess-d", &holder.addr);
        let v = prove_live(&rec).expect("proven");
        let outcome = terminate_verified(v, Intent::Reap);
        assert!(
            matches!(outcome, HolderTermination::Delivered { .. }),
            "{outcome:?}"
        );
        assert_eq!(holder.seen(), vec!["ping", "shutdown"]);
    }

    #[test]
    fn deliver_shutdown_rechecks_before_send() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "deliver-prompt");
        let holder = raw_holder(
            Some(persist_path(&rec).unwrap()),
            "sess-d",
            &crate::harness::grok_cwd(&rec).expect("cwd"),
            4242,
            RawMode::PingMutatePrompt,
        );
        write_live_handle(&rec, "sess-d", &holder.addr);
        let v = prove_live(&rec).expect("proven");
        let outcome = terminate_verified(v, Intent::Sweep);
        assert_eq!(
            outcome,
            HolderTermination::Unproven {
                why: "prompt-in-flight".into()
            }
        );
        assert_eq!(
            holder.seen(),
            vec!["ping"],
            "the sweep never interrupts a turn"
        );
    }

    #[test]
    fn deliver_shutdown_delivers_during_a_prompt_for_operator() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "deliver-operator-prompt");
        let holder = raw_holder(
            Some(persist_path(&rec).unwrap()),
            "sess-d",
            &crate::harness::grok_cwd(&rec).expect("cwd"),
            4242,
            RawMode::PingMutatePrompt,
        );
        write_live_handle(&rec, "sess-d", &holder.addr);
        let v = prove_live(&rec).expect("proven");
        let outcome = terminate_verified(v, Intent::Operator);
        assert!(
            matches!(outcome, HolderTermination::Delivered { .. }),
            "{outcome:?}"
        );
        assert_eq!(holder.seen(), vec!["ping", "shutdown"]);
    }

    #[test]
    fn deliver_shutdown_refuses_persist_changed() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "deliver-changed");
        let holder = raw_holder(
            Some(persist_path(&rec).unwrap()),
            "sess-d",
            &crate::harness::grok_cwd(&rec).expect("cwd"),
            4242,
            RawMode::PingMutateSession,
        );
        write_live_handle(&rec, "sess-d", &holder.addr);
        let v = prove_live(&rec).expect("proven");
        assert_eq!(
            terminate_verified(v, Intent::Reap),
            HolderTermination::Unproven {
                why: "persist-changed".into()
            }
        );
        assert_eq!(holder.seen(), vec!["ping"]);
    }

    #[test]
    fn deliver_shutdown_keeps_when_persist_removed_after_ping() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "deliver-removed");
        let holder = raw_holder(
            Some(persist_path(&rec).unwrap()),
            "sess-d",
            &crate::harness::grok_cwd(&rec).expect("cwd"),
            4242,
            RawMode::PingRemovePersist,
        );
        write_live_handle(&rec, "sess-d", &holder.addr);
        let v = prove_live(&rec).expect("proven");
        assert_eq!(
            terminate_verified(v, Intent::Reap),
            HolderTermination::Unproven {
                why: "persist-missing".into()
            }
        );
        assert_eq!(holder.seen(), vec!["ping"]);
    }

    #[test]
    fn deliver_shutdown_keeps_when_persist_unreadable_after_ping() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "deliver-unreadable");
        let holder = raw_holder(
            Some(persist_path(&rec).unwrap()),
            "sess-d",
            &crate::harness::grok_cwd(&rec).expect("cwd"),
            4242,
            RawMode::PingCorruptPersist,
        );
        write_live_handle(&rec, "sess-d", &holder.addr);
        let v = prove_live(&rec).expect("proven");
        assert_eq!(
            terminate_verified(v, Intent::Reap),
            HolderTermination::Unproven {
                why: "persist-unreadable".into()
            }
        );
        assert_eq!(holder.seen(), vec!["ping"]);
    }

    #[test]
    fn deliver_shutdown_refuses_recently_active() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "deliver-active");
        let holder = raw_holder(
            Some(persist_path(&rec).unwrap()),
            "sess-d",
            &crate::harness::grok_cwd(&rec).expect("cwd"),
            4242,
            RawMode::PingTouchMtime,
        );
        write_live_handle(&rec, "sess-d", &holder.addr);
        let v = prove_live(&rec).expect("proven");
        assert_eq!(
            terminate_verified(v, Intent::Reap),
            HolderTermination::Unproven {
                why: "recently-active".into()
            }
        );
        assert_eq!(holder.seen(), vec!["ping"]);
    }

    #[test]
    fn deliver_shutdown_rejected_is_unproven() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "deliver-rejected");
        let holder = raw_holder(
            Some(persist_path(&rec).unwrap()),
            "sess-d",
            &crate::harness::grok_cwd(&rec).expect("cwd"),
            4242,
            RawMode::ShutdownRejected,
        );
        write_live_handle(&rec, "sess-d", &holder.addr);
        let v = prove_live(&rec).expect("proven");
        assert_eq!(
            terminate_verified(v, Intent::Reap),
            HolderTermination::Unproven {
                why: "shutdown-rejected".into()
            }
        );
        assert_eq!(holder.seen(), vec!["ping", "shutdown"]);
    }

    #[test]
    fn deliver_shutdown_times_out() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "deliver-timeout");
        let holder = raw_holder(
            Some(persist_path(&rec).unwrap()),
            "sess-d",
            &crate::harness::grok_cwd(&rec).expect("cwd"),
            4242,
            RawMode::ShutdownNoResponse,
        );
        write_live_handle(&rec, "sess-d", &holder.addr);
        let v = prove_live(&rec).expect("proven");
        assert_eq!(
            terminate_verified(v, Intent::Reap),
            HolderTermination::Unproven {
                why: "shutdown-timeout".into()
            }
        );
        assert_eq!(holder.seen(), vec!["ping", "shutdown"]);
    }

    #[test]
    fn deliver_shutdown_pending_after_ping_is_not_exited() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "deliver-pending");
        let holder = raw_holder(
            Some(persist_path(&rec).unwrap()),
            "sess-d",
            &crate::harness::grok_cwd(&rec).expect("cwd"),
            4242,
            RawMode::PingPending,
        );
        write_live_handle(&rec, "sess-d", &holder.addr);
        let v = prove_live(&rec).expect("proven");
        assert_eq!(
            terminate_verified(v, Intent::Reap),
            HolderTermination::Unproven {
                why: "persist-changed".into()
            },
            "another producer's record is not this holder's exit"
        );
        assert_eq!(holder.seen(), vec!["ping"]);
    }

    #[test]
    fn deliver_shutdown_failed_start_after_ping_is_not_exited() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "deliver-failed-start");
        let holder = raw_holder(
            Some(persist_path(&rec).unwrap()),
            "sess-d",
            &crate::harness::grok_cwd(&rec).expect("cwd"),
            4242,
            RawMode::PingFailedStart,
        );
        write_live_handle(&rec, "sess-d", &holder.addr);
        let v = prove_live(&rec).expect("proven");
        assert_eq!(
            terminate_verified(v, Intent::Reap),
            HolderTermination::Unproven {
                why: "persist-changed".into()
            }
        );
        assert_eq!(holder.seen(), vec!["ping"]);
    }

    #[test]
    fn prove_holder_no_listener_is_not_gone() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "prove-no-listener");
        let addr = closed_addr();
        write_live_handle(&rec, "sess-d", &addr);
        match prove_live(&rec) {
            Err(HolderTermination::NoListener { addr: got }) => assert_eq!(got, addr),
            other => panic!("expected NoListener, got {other:?}"),
        }
    }

    #[test]
    fn prove_holder_no_response_is_unproven() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "prove-no-response");
        let holder = raw_holder(
            Some(persist_path(&rec).unwrap()),
            "sess-d",
            &crate::harness::grok_cwd(&rec).expect("cwd"),
            4242,
            RawMode::PingNoResponse,
        );
        write_live_handle(&rec, "sess-d", &holder.addr);
        match prove_live(&rec) {
            Err(HolderTermination::Unproven { why }) => assert_eq!(why, "ping-timeout"),
            other => panic!("expected ping-timeout, got {other:?}"),
        }
        assert_eq!(holder.seen(), vec!["ping"]);
    }

    #[test]
    fn prove_holder_no_socket_classifies_producer() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "prove-no-socket");
        let shapes = [
            (
                make_handle(&rec, Some("s"), None, None, Some(4242), false, false, None),
                NoSocketWhy::DeadAfterExit,
            ),
            (
                make_handle(&rec, None, None, None, None, false, false, Some("e".into())),
                NoSocketWhy::FailedStart,
            ),
            (
                make_handle(&rec, None, None, None, None, false, false, None),
                NoSocketWhy::PendingStart,
            ),
            (
                make_handle(
                    &rec,
                    Some("s"),
                    None,
                    None,
                    Some(std::process::id()),
                    true,
                    false,
                    None,
                ),
                NoSocketWhy::AliveSelfNoAddress,
            ),
            (
                make_handle(&rec, Some("s"), None, None, Some(4242), true, false, None),
                NoSocketWhy::AliveForeignNoAddress,
            ),
            (
                make_handle(&rec, Some("s"), None, None, None, true, false, None),
                NoSocketWhy::AliveUnknownNoAddress,
            ),
        ];
        for (handle, want) in shapes {
            write_handle(&rec, &handle);
            let identity = PersistIdentity::from_handle(&handle);
            assert_eq!(
                classify_persist(&identity),
                PersistClass::NoAddress(want),
                "one classifier for every caller"
            );
            match prove_holder(&rec, &identity, Duration::from_millis(100)) {
                Err(HolderTermination::NoSocket { why }) => assert_eq!(why, want),
                other => panic!("expected NoSocket({want:?}), got {other:?}"),
            }
        }
    }

    #[test]
    fn probe_holder_uses_one_ping() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "probe-one-ping");
        let holder = raw_holder(
            Some(persist_path(&rec).unwrap()),
            "sess-d",
            &crate::harness::grok_cwd(&rec).expect("cwd"),
            4242,
            RawMode::Normal,
        );
        write_live_handle(&rec, "sess-d", &holder.addr);
        let status = probe_holder(&rec, "implementor").unwrap().expect("proven");
        assert_eq!(status.pid, Some(4242));
        assert_eq!(holder.seen(), vec!["ping"], "one Ping, one identity proof");

        // A pid mismatch is not the holder.
        let mut handle = load_persist(&rec).unwrap().unwrap();
        handle.pid = Some(9999);
        write_handle(&rec, &handle);
        assert!(probe_holder(&rec, "implementor").unwrap().is_none());
    }

    // ---- 0117: start-path table B --------------------------------------------

    struct HookInstalled;
    impl Drop for HookInstalled {
        fn drop(&mut self) {
            *AFTER_GONE_HOOK.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    fn install_after_gone(f: impl Fn(&ProjectRecord) + Send + Sync + 'static) -> HookInstalled {
        *AFTER_GONE_HOOK.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::sync::Arc::new(f));
        HookInstalled
    }

    #[tokio::test]
    async fn start_refuses_while_a_start_is_pending() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "start-pending-token");
        write_handle(&rec, &attempt_handle(&rec, None));
        age_file(&rec, Duration::from_secs(5));
        let err = reuse_or_reap_existing(&rec, "grok", None, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("start-pending"), "{err}");
    }

    #[tokio::test]
    async fn start_refuses_a_pending_record_does_not_touch_it() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "start-pending-bytes");
        write_handle(&rec, &attempt_handle(&rec, None));
        age_file(&rec, Duration::from_secs(5));
        let before = file_bytes(&rec);
        let _ = reuse_or_reap_existing(&rec, "grok", None, None).await;
        assert_eq!(before, file_bytes(&rec));
        assert!(
            !global_pool().lock().await.contains(&rec.id),
            "nothing spawned"
        );
    }

    #[tokio::test]
    async fn start_discards_pending_persist_after_the_start_window() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "start-aged-pending");
        write_handle(&rec, &attempt_handle(&rec, None));
        age_file(&rec, PENDING_START_GRACE + Duration::from_secs(1));
        let got = reuse_or_reap_existing(&rec, "grok", None, None)
            .await
            .unwrap();
        assert!(got.is_none());
        assert!(load_persist(&rec).unwrap().is_none());
        let log = std::fs::read_to_string(rec.path.join("status.md")).unwrap_or_default();
        assert!(log.contains("holder-persist-discarded"), "{log}");
    }

    #[tokio::test]
    async fn start_waits_out_the_grace_then_discards() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "start-grace-wait");
        write_handle(
            &rec,
            &make_handle(
                &rec,
                Some("sess-exit"),
                Some(crate::harness::grok_cwd(&rec).expect("cwd")),
                None,
                Some(4242),
                false,
                false,
                None,
            ),
        );
        age_file(&rec, DISCARD_GRACE - Duration::from_millis(200));
        let started = std::time::Instant::now();
        let got = reuse_or_reap_existing(&rec, "grok", None, None)
            .await
            .unwrap();
        assert!(got.is_none());
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "the start waited out the remaining grace"
        );
        assert!(load_persist(&rec).unwrap().is_none());
    }

    #[tokio::test]
    async fn start_discards_dead_no_address_after_grace() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "start-aged-dead");
        write_handle(
            &rec,
            &make_handle(
                &rec,
                Some("sess-exit"),
                Some(crate::harness::grok_cwd(&rec).expect("cwd")),
                None,
                Some(4242),
                false,
                false,
                None,
            ),
        );
        age_file(&rec, DISCARD_GRACE + Duration::from_secs(1));
        let got = reuse_or_reap_existing(&rec, "grok", None, None)
            .await
            .unwrap();
        assert!(got.is_none());
        assert!(load_persist(&rec).unwrap().is_none());
        let log = std::fs::read_to_string(rec.path.join("status.md")).unwrap_or_default();
        assert!(
            log.contains("holder_pid=4242"),
            "the receipt names the old identity: {log}"
        );
    }

    #[tokio::test]
    async fn start_refuses_alive_no_address_foreign_holder() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "start-foreign-no-addr");
        write_handle(
            &rec,
            &make_handle(
                &rec,
                Some("sess"),
                None,
                None,
                Some(4242),
                true,
                false,
                None,
            ),
        );
        let err = reuse_or_reap_existing(&rec, "grok", None, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("foreign-holder"), "{err}");

        write_handle(
            &rec,
            &make_handle(&rec, Some("sess"), None, None, None, true, false, None),
        );
        let err = reuse_or_reap_existing(&rec, "grok", None, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown-record"), "{err}");
    }

    #[tokio::test]
    async fn start_refuses_when_termination_unproven() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "start-unproven");
        let holder = raw_holder(
            Some(persist_path(&rec).unwrap()),
            "sess-other",
            &crate::harness::grok_cwd(&rec).expect("cwd"),
            4242,
            RawMode::Normal,
        );
        write_live_handle(&rec, "sess-ours", &holder.addr);
        let before = file_bytes(&rec);
        let err = reuse_or_reap_existing(&rec, "grok", None, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("identity:session"), "{err}");
        assert_eq!(
            before,
            file_bytes(&rec),
            "an unproven holder is never clobbered"
        );
        let ops = holder.seen();
        assert!(
            !ops.is_empty() && ops.iter().all(|op| op == "ping"),
            "the proof never sends a Shutdown to an unproven holder: {ops:?}"
        );
    }

    #[tokio::test]
    async fn reuse_clears_persist_when_no_listener() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "reuse-no-listener");
        write_live_handle(&rec, "sess-d", &closed_addr());
        let got = reuse_or_reap_existing(&rec, "grok", None, None)
            .await
            .unwrap();
        assert!(got.is_none());
        assert!(
            load_persist(&rec).unwrap().is_none(),
            "a closed listener is the start path's arm A"
        );
    }

    #[tokio::test]
    async fn start_refuses_when_delivered_holder_never_goes_dead() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "start-never-dead");
        let holder = raw_holder(
            Some(persist_path(&rec).unwrap()),
            "sess-d",
            &crate::harness::grok_cwd(&rec).expect("cwd"),
            4242,
            RawMode::ShutdownNoExit,
        );
        write_live_handle(&rec, "sess-d", &holder.addr);
        let before = file_bytes(&rec);
        let err = reuse_or_reap_existing(&rec, "grok", Some("phase-model"), None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("retry"), "{err}");
        assert_eq!(before, file_bytes(&rec), "TimedOut writes nothing");
        assert_eq!(holder.seen(), vec!["ping", "shutdown"]);
    }

    #[tokio::test]
    async fn start_does_not_clear_a_newer_holder_persist() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "start-superseded");
        let holder = raw_holder(
            Some(persist_path(&rec).unwrap()),
            "sess-d",
            &crate::harness::grok_cwd(&rec).expect("cwd"),
            4242,
            RawMode::ShutdownMutate,
        );
        write_live_handle(&rec, "sess-d", &holder.addr);
        let err = reuse_or_reap_existing(&rec, "grok", Some("phase-model"), None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("holder-changed"), "{err}");
        let (now, _) = persist_identity(&rec).unwrap().unwrap();
        assert_eq!(
            now.session_id.as_deref(),
            Some("sess-newer-holder"),
            "the successor record survives byte-for-byte"
        );
    }

    #[tokio::test]
    async fn gone_then_changed_start_does_not_spawn() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "gone-changed-start");
        let holder = raw_holder(
            Some(persist_path(&rec).unwrap()),
            "sess-d",
            &crate::harness::grok_cwd(&rec).expect("cwd"),
            4242,
            RawMode::Normal,
        );
        write_live_handle(&rec, "sess-d", &holder.addr);
        let rec_hook = rec.clone();
        let _hook = install_after_gone(move |_| {
            write_handle(
                &rec_hook,
                &make_handle(
                    &rec_hook,
                    Some("sess-successor"),
                    Some(crate::harness::grok_cwd(&rec_hook).expect("cwd")),
                    Some("127.0.0.1:9".into()),
                    Some(99),
                    true,
                    false,
                    None,
                ),
            );
        });
        let err = reuse_or_reap_existing(&rec, "grok", Some("phase-model"), None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("holder-changed"), "{err}");
        let (now, _) = persist_identity(&rec).unwrap().unwrap();
        assert_eq!(now.session_id.as_deref(), Some("sess-successor"));
        assert!(
            !global_pool().lock().await.contains(&rec.id),
            "nothing spawned"
        );
    }

    #[tokio::test]
    async fn delivered_waits_for_the_old_persist_before_clear() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "delivered-late-write");
        let holder = raw_holder(
            Some(persist_path(&rec).unwrap()),
            "sess-d",
            &crate::harness::grok_cwd(&rec).expect("cwd"),
            4242,
            RawMode::ShutdownDelayedExit,
        );
        write_live_handle(&rec, "sess-d", &holder.addr);
        let got = reuse_or_reap_existing(&rec, "grok", Some("phase-model"), None)
            .await
            .unwrap();
        assert!(got.is_none());
        assert!(
            load_persist(&rec).unwrap().is_none(),
            "the wait observed the late exit record before clearing"
        );
    }

    // ---- 0117: the recycle ------------------------------------------------

    #[tokio::test]
    async fn recycle_holder_kind_uses_shutdown_rpc() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "recycle-holder");
        let holder = raw_holder(
            Some(persist_path(&rec).unwrap()),
            "sess-d",
            &crate::harness::grok_cwd(&rec).expect("cwd"),
            4242,
            RawMode::Normal,
        );
        write_live_handle(&rec, "sess-d", &holder.addr);
        recycle_without_pool_lock(&rec).await.unwrap();
        assert_eq!(holder.seen(), vec!["ping", "shutdown"]);
        let (now, _) = persist_identity(&rec).unwrap().unwrap();
        assert!(!now.alive);
        assert!(now.control_addr.is_none());
    }

    #[tokio::test]
    async fn recycle_unproven_leaves_persist() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "recycle-unproven");
        let holder = raw_holder(
            Some(persist_path(&rec).unwrap()),
            "sess-other",
            &crate::harness::grok_cwd(&rec).expect("cwd"),
            4242,
            RawMode::Normal,
        );
        write_live_handle(&rec, "sess-ours", &holder.addr);
        let before = file_bytes(&rec);
        let err = recycle_without_pool_lock(&rec)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("identity:session"), "{err}");
        assert_eq!(before, file_bytes(&rec));
    }

    #[tokio::test]
    async fn recycle_no_listener_keeps_persist() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "recycle-no-listener");
        write_live_handle(&rec, "sess-d", &closed_addr());
        let before = file_bytes(&rec);
        let err = recycle_without_pool_lock(&rec)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no listener"), "{err}");
        assert_eq!(before, file_bytes(&rec));
    }

    #[tokio::test]
    async fn recycle_without_a_persist_only_touches_the_pool() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "recycle-pool-only");
        let session = GrokSession::start_mock(
            crate::harness::grok_cwd(&rec).expect("cwd"),
            mock_handshake_ok("sess-pool"),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        insert_test_session(rec.id.clone(), session).await;
        assert!(global_pool().lock().await.contains(&rec.id));
        recycle_without_pool_lock(&rec).await.unwrap();
        assert!(!global_pool().lock().await.contains(&rec.id));
        assert!(
            !persist_path(&rec).unwrap().exists(),
            "no persist is created"
        );
        assert!(crate::harness::abort::cancel_handle_for(&rec.id).is_none());
    }

    #[tokio::test]
    async fn recycle_pending_start_leaves_the_record() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "recycle-pending");
        write_handle(&rec, &attempt_handle(&rec, None));
        let before = file_bytes(&rec);
        let err = recycle_without_pool_lock(&rec)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("start-pending"), "{err}");
        assert_eq!(before, file_bytes(&rec));
    }

    #[tokio::test]
    async fn recycle_in_process_tears_down_owned_session() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "recycle-in-process");
        let session = GrokSession::start_mock(
            crate::harness::grok_cwd(&rec).expect("cwd"),
            mock_handshake_ok("sess-self"),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        let counter = session.shutdown_counter();
        insert_test_session(rec.id.clone(), session).await;
        write_handle(
            &rec,
            &make_handle(
                &rec,
                Some("sess-self"),
                Some(crate::harness::grok_cwd(&rec).expect("cwd")),
                None,
                Some(std::process::id()),
                true,
                false,
                None,
            ),
        );
        recycle_without_pool_lock(&rec).await.unwrap();
        assert!(!global_pool().lock().await.contains(&rec.id));
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the owned session was torn down"
        );
        let (now, _) = persist_identity(&rec).unwrap().unwrap();
        assert!(!now.alive, "a truthful dead record");
        assert!(now.control_addr.is_none());
    }

    #[tokio::test]
    async fn recycle_in_process_busy_leaves_persist_honest() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "recycle-busy");
        write_handle(
            &rec,
            &make_handle(
                &rec,
                Some("sess-self"),
                Some(crate::harness::grok_cwd(&rec).expect("cwd")),
                None,
                Some(std::process::id()),
                true,
                false,
                None,
            ),
        );
        let held = tokio::spawn(async {
            let _guard = global_pool().lock().await;
            tokio::time::sleep(Duration::from_millis(6000)).await;
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let err = recycle_without_pool_lock(&rec)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("pool lock busy"), "{err}");
        let (now, _) = persist_identity(&rec).unwrap().unwrap();
        assert!(now.alive, "the persist stays honest");
        assert!(now.control_addr.is_none());
        held.abort();
    }

    #[tokio::test]
    async fn recycle_in_process_without_an_owned_session_clears_only_the_record() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "recycle-stale-self");
        write_handle(
            &rec,
            &make_handle(
                &rec,
                Some("sess-self"),
                Some(crate::harness::grok_cwd(&rec).expect("cwd")),
                None,
                Some(std::process::id()),
                true,
                false,
                None,
            ),
        );
        recycle_without_pool_lock(&rec).await.unwrap();
        assert!(load_persist(&rec).unwrap().is_none());
        let log = std::fs::read_to_string(rec.path.join("status.md")).unwrap_or_default();
        assert!(log.contains("stale-self"), "{log}");
    }

    #[tokio::test]
    async fn recycle_leaves_a_fresh_failed_start_for_the_start_grace() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "recycle-fresh-failure");
        write_handle(&rec, &attempt_handle(&rec, Some("prior failure")));
        age_file(&rec, Duration::from_secs(1));
        let before = file_bytes(&rec);
        recycle_without_pool_lock(&rec).await.unwrap();
        assert_eq!(before, file_bytes(&rec), "recycle writes nothing");
        let log = std::fs::read_to_string(rec.path.join("status.md")).unwrap_or_default();
        assert!(!log.contains("holder-persist-discarded"), "no line either");

        // The following start waits out the rest of the grace, then discards + starts.
        age_file(&rec, DISCARD_GRACE - Duration::from_millis(200));
        let got = reuse_or_reap_existing(&rec, "grok", None, None)
            .await
            .unwrap();
        assert!(got.is_none());
        assert!(load_persist(&rec).unwrap().is_none());
    }

    #[tokio::test]
    async fn recycle_leaves_a_newer_holder_persist_byte_identical() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "recycle-superseded");
        let holder = raw_holder(
            Some(persist_path(&rec).unwrap()),
            "sess-d",
            &crate::harness::grok_cwd(&rec).expect("cwd"),
            4242,
            RawMode::ShutdownMutate,
        );
        write_live_handle(&rec, "sess-d", &holder.addr);
        recycle_without_pool_lock(&rec).await.unwrap();
        let (now, _) = persist_identity(&rec).unwrap().unwrap();
        assert_eq!(now.session_id.as_deref(), Some("sess-newer-holder"));
        assert!(now.alive);
    }

    #[tokio::test]
    async fn recycle_alive_self_false_compare_is_a_refusal() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "recycle-self-changed");
        let session = GrokSession::start_mock(
            crate::harness::grok_cwd(&rec).expect("cwd"),
            mock_handshake_ok("sess-self"),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        insert_test_session(rec.id.clone(), session).await;
        write_handle(
            &rec,
            &make_handle(
                &rec,
                Some("sess-self"),
                Some(crate::harness::grok_cwd(&rec).expect("cwd")),
                None,
                Some(std::process::id()),
                true,
                false,
                None,
            ),
        );
        let rec_hook = rec.clone();
        let _hook = install_after_gone(move |_| {
            write_handle(
                &rec_hook,
                &make_handle(
                    &rec_hook,
                    Some("sess-successor"),
                    Some(crate::harness::grok_cwd(&rec_hook).expect("cwd")),
                    Some("127.0.0.1:9".into()),
                    Some(99),
                    true,
                    false,
                    None,
                ),
            );
        });
        let err = recycle_without_pool_lock(&rec)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("holder-changed"), "{err}");
        let (now, _) = persist_identity(&rec).unwrap().unwrap();
        assert_eq!(now.session_id.as_deref(), Some("sess-successor"));
        assert!(now.alive, "no dead write on the newer record");
    }

    #[tokio::test]
    async fn gone_then_changed_recycle_keeps_the_record() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "recycle-gone-changed");
        let holder = raw_holder(
            Some(persist_path(&rec).unwrap()),
            "sess-d",
            &crate::harness::grok_cwd(&rec).expect("cwd"),
            4242,
            RawMode::Normal,
        );
        write_live_handle(&rec, "sess-d", &holder.addr);
        let rec_hook = rec.clone();
        let _hook = install_after_gone(move |_| {
            write_handle(
                &rec_hook,
                &make_handle(
                    &rec_hook,
                    Some("sess-successor"),
                    Some(crate::harness::grok_cwd(&rec_hook).expect("cwd")),
                    Some("127.0.0.1:9".into()),
                    Some(99),
                    true,
                    false,
                    None,
                ),
            );
        });
        recycle_without_pool_lock(&rec).await.unwrap();
        let (now, _) = persist_identity(&rec).unwrap().unwrap();
        assert_eq!(now.session_id.as_deref(), Some("sess-successor"));
        assert!(now.alive);
    }

    #[tokio::test]
    async fn recycle_holder_kind_leaves_the_pool_and_cancel_untouched() {
        let _guard = test_env_lock();
        let modes = [
            RawMode::Normal,         // Delivered -> Gone -> mark_dead (false: write skipped)
            RawMode::ShutdownNoExit, // TimedOut
            RawMode::ShutdownMutate, // Superseded
        ];
        for mode in modes {
            let dir = tempdir().unwrap();
            let rec = record_in(&dir, &format!("recycle-neutral-{mode:?}"));
            let holder = raw_holder(
                Some(persist_path(&rec).unwrap()),
                "sess-d",
                &crate::harness::grok_cwd(&rec).expect("cwd"),
                4242,
                mode,
            );
            write_live_handle(&rec, "sess-d", &holder.addr);
            let session = GrokSession::start_mock(
                crate::harness::grok_cwd(&rec).expect("cwd"),
                mock_handshake_ok("sess-pool"),
                Duration::from_secs(2),
            )
            .await
            .unwrap();
            insert_test_session(rec.id.clone(), session).await;
            assert!(crate::harness::abort::cancel_handle_for(&rec.id).is_some());
            let _ = recycle_without_pool_lock(&rec).await;
            assert!(
                global_pool().lock().await.contains(&rec.id),
                "the addressed-holder branch never touches the pool ({mode:?})"
            );
            assert!(
                crate::harness::abort::cancel_handle_for(&rec.id).is_some(),
                "nor the cancel registration ({mode:?})"
            );
            global_pool().lock().await.remove(&rec.id);
            crate::harness::abort::unregister_cancel_handle(&rec.id);
        }
    }

    #[tokio::test]
    async fn recycle_unreadable_persist_is_a_refusal() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "recycle-unreadable");
        std::fs::create_dir_all(persist_path(&rec).unwrap().parent().unwrap()).unwrap();
        std::fs::write(persist_path(&rec).unwrap(), b"{not json").unwrap();
        let session = GrokSession::start_mock(
            crate::harness::grok_cwd(&rec).expect("cwd"),
            mock_handshake_ok("sess-pool"),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        insert_test_session(rec.id.clone(), session).await;
        let before = file_bytes(&rec);
        let err = recycle_without_pool_lock(&rec)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("persist-unreadable"), "{err}");
        assert_eq!(before, file_bytes(&rec));
        assert!(global_pool().lock().await.contains(&rec.id));
        assert!(crate::harness::abort::cancel_handle_for(&rec.id).is_some());
        global_pool().lock().await.remove(&rec.id);
        crate::harness::abort::unregister_cancel_handle(&rec.id);
    }

    // ---- 0117: operator path ------------------------------------------------

    #[tokio::test]
    async fn operator_shutdown_is_rpc_only() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "operator-rpc-only");
        let _registered = register_rec(&rec);
        let holder = raw_holder(
            Some(persist_path(&rec).unwrap()),
            "sess-d",
            &crate::harness::grok_cwd(&rec).expect("cwd"),
            4242,
            RawMode::ShutdownNoResponse,
        );
        write_live_handle(&rec, "sess-d", &holder.addr);
        let before = file_bytes(&rec);
        let err = shutdown(Some(&rec.id), false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("shutdown-timeout"), "{err}");
        assert_eq!(
            before,
            file_bytes(&rec),
            "an unproven operator teardown writes nothing"
        );
        assert_eq!(holder.seen(), vec!["ping", "shutdown"]);
    }

    #[tokio::test]
    async fn operator_shutdown_never_kills_a_foreign_pid() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "operator-foreign-pid");
        let _registered = register_rec(&rec);
        let mut child = spawn_dummy_long_child();
        let mut handle = make_handle(
            &rec,
            Some("sess-d"),
            Some(crate::harness::grok_cwd(&rec).expect("cwd")),
            Some(closed_addr()),
            None,
            true,
            false,
            None,
        );
        handle.pid = Some(child.id());
        write_handle(&rec, &handle);
        let before = file_bytes(&rec);
        let err = shutdown(Some(&rec.id), false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no listener"), "{err}");
        assert!(
            !wait_child_dead(&mut child, 100),
            "the dummy PID must still be alive"
        );
        assert_eq!(before, file_bytes(&rec));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[tokio::test]
    async fn operator_shutdown_does_not_overwrite_a_newer_persist() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "operator-superseded");
        let _registered = register_rec(&rec);
        let holder = raw_holder(
            Some(persist_path(&rec).unwrap()),
            "sess-d",
            &crate::harness::grok_cwd(&rec).expect("cwd"),
            4242,
            RawMode::ShutdownMutate,
        );
        write_live_handle(&rec, "sess-d", &holder.addr);
        let err = shutdown(Some(&rec.id), false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("holder-changed"), "{err}");
        let (now, _) = persist_identity(&rec).unwrap().unwrap();
        assert_eq!(now.session_id.as_deref(), Some("sess-newer-holder"));
    }

    #[tokio::test]
    async fn operator_shutdown_timeout_does_not_suggest_delete() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "operator-timeout");
        let _registered = register_rec(&rec);
        let holder = raw_holder(
            Some(persist_path(&rec).unwrap()),
            "sess-d",
            &crate::harness::grok_cwd(&rec).expect("cwd"),
            4242,
            RawMode::ShutdownNoExit,
        );
        write_live_handle(&rec, "sess-d", &holder.addr);
        let err = shutdown(Some(&rec.id), false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("retry"), "{err}");
        assert!(!err.contains("delete"), "{err}");
    }

    #[tokio::test]
    async fn gone_then_changed_operator_keeps_the_record() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "operator-gone-changed");
        let _registered = register_rec(&rec);
        let holder = raw_holder(
            Some(persist_path(&rec).unwrap()),
            "sess-d",
            &crate::harness::grok_cwd(&rec).expect("cwd"),
            4242,
            RawMode::Normal,
        );
        write_live_handle(&rec, "sess-d", &holder.addr);
        let rec_hook = rec.clone();
        let _hook = install_after_gone(move |_| {
            write_handle(
                &rec_hook,
                &make_handle(
                    &rec_hook,
                    Some("sess-successor"),
                    Some(crate::harness::grok_cwd(&rec_hook).expect("cwd")),
                    Some("127.0.0.1:9".into()),
                    Some(99),
                    true,
                    false,
                    None,
                ),
            );
        });
        let err = shutdown(Some(&rec.id), false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("holder-changed"), "{err}");
        let (now, _) = persist_identity(&rec).unwrap().unwrap();
        assert_eq!(now.session_id.as_deref(), Some("sess-successor"));
        assert!(now.alive, "no dead status/write on the successor");
    }

    #[tokio::test]
    async fn alive_self_false_compare_is_a_refusal_in_the_operator_path() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "operator-self-changed");
        let _registered = register_rec(&rec);
        write_handle(
            &rec,
            &make_handle(
                &rec,
                Some("sess-self"),
                Some(crate::harness::grok_cwd(&rec).expect("cwd")),
                None,
                Some(std::process::id()),
                true,
                false,
                None,
            ),
        );
        let rec_hook = rec.clone();
        let _hook = install_after_gone(move |_| {
            write_handle(
                &rec_hook,
                &make_handle(
                    &rec_hook,
                    Some("sess-successor"),
                    Some(crate::harness::grok_cwd(&rec_hook).expect("cwd")),
                    Some("127.0.0.1:9".into()),
                    Some(99),
                    true,
                    false,
                    None,
                ),
            );
        });
        let err = shutdown(Some(&rec.id), false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("holder-changed"), "{err}");
        let (now, _) = persist_identity(&rec).unwrap().unwrap();
        assert_eq!(now.session_id.as_deref(), Some("sess-successor"));
    }

    // ---- 0117: abort recycle under the lock ---------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)]
    async fn abort_recycle_in_process_under_lock() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let dir = tempdir().unwrap();
        let _env = crate::config::TestEnvGuard::capture(&[
            ENV_COORDINATOR_HOME,
            crate::harness::abort::ENV_CANCEL_WAIT_SECS,
        ]);
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(crate::harness::abort::ENV_CANCEL_WAIT_SECS, "0");
        }
        let rec = record_in(&dir, "abort-in-process");
        crate::run::run_stub(&rec, Some("0027".into())).unwrap();
        let session = GrokSession::start_mock(
            crate::harness::grok_cwd(&rec).expect("cwd"),
            mock_handshake_ok("sess-self"),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        let counter = session.shutdown_counter();
        insert_test_session(rec.id.clone(), session).await;
        write_handle(
            &rec,
            &make_handle(
                &rec,
                Some("sess-self"),
                Some(crate::harness::grok_cwd(&rec).expect("cwd")),
                None,
                Some(std::process::id()),
                true,
                true,
                None,
            ),
        );
        crate::harness::abort::abort_stuck_prompt_sync(
            &rec,
            crate::harness::abort::AbortReason::Stall,
            crate::state::active_run_epoch().unwrap_or(0),
        );
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
        let (now, _) = persist_identity(&rec).unwrap().unwrap();
        assert!(!now.alive);
        assert!(!now.prompt_in_flight);
    }

    // ---- 0117: two real start paths, exactly one spawn ----------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[allow(clippy::await_holding_lock)]
    async fn both_start_paths_claim_and_exactly_one_spawns() {
        use std::sync::Barrier;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let _guard = test_env_lock();
        let previous_hook = SPAWN_HOOK.lock().unwrap_or_else(|e| e.into_inner()).take();
        let spawns = std::sync::Arc::new(AtomicUsize::new(0));
        {
            let spawns = spawns.clone();
            *SPAWN_HOOK.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(std::sync::Arc::new(move |rec: &ProjectRecord| {
                    spawns.fetch_add(1, Ordering::SeqCst);
                    // The claim must already be the only write on disk.
                    let (identity, _) = persist_identity(rec).unwrap().unwrap();
                    assert!(
                        identity.is_attempt(),
                        "the spawn stand-in observes the winner's PendingStart claim"
                    );
                    assert!(identity.error.is_none());
                    Ok(GrokHarnessStatus::missing())
                }));
        }

        for (arm, a_in_process, b_in_process) in [
            ("detached", false, false),
            ("in-process", true, true),
            ("cross", false, true),
        ] {
            let home = tempdir().unwrap();
            let proj = tempdir().unwrap();
            unsafe {
                std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            }
            let mut reg = Registry::default();
            let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
            reg.save(&crate::config::registry_path().unwrap()).unwrap();
            let _ = std::fs::remove_file(persist_path(&rec).unwrap());
            global_pool().lock().await.remove(&rec.id);
            crate::harness::abort::unregister_cancel_handle(&rec.id);
            spawns.store(0, Ordering::SeqCst);

            let barrier = std::sync::Arc::new(Barrier::new(2));
            {
                let barrier = barrier.clone();
                *PRE_CLAIM_HOOK.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(std::sync::Arc::new(move |_: &ProjectRecord| {
                        barrier.wait();
                    }));
            }

            let id_a = rec.id.clone();
            let bin = PathBuf::from("missing-0117-holder.exe");
            let first = tokio::spawn({
                let bin = bin.clone();
                async move {
                    start_with_bin(Some(&id_a), a_in_process, bin, None, false, "grok", None).await
                }
            });
            let id_b = rec.id.clone();
            let second = tokio::spawn(async move {
                start_with_bin(Some(&id_b), b_in_process, bin, None, false, "grok", None).await
            });
            let results = [first.await.unwrap(), second.await.unwrap()];
            assert_eq!(
                spawns.load(Ordering::SeqCst),
                1,
                "exactly one spawn per arm ({arm})"
            );
            let winners = results.iter().filter(|r| r.is_ok()).count();
            assert_eq!(winners, 1, "one start owns the claim ({arm})");
            let loser = results
                .iter()
                .find(|r| r.is_err())
                .and_then(|r| r.as_ref().err())
                .map(|e| e.to_string())
                .unwrap();
            assert!(
                loser.contains("persist-appeared"),
                "the loser refuses without spawning ({arm}): {loser}"
            );
            let (identity, _) = persist_identity(&rec).unwrap().unwrap();
            assert!(identity.is_attempt());
            assert!(identity.error.is_none());

            *PRE_CLAIM_HOOK.lock().unwrap_or_else(|e| e.into_inner()) = None;
            let _ = std::fs::remove_file(persist_path(&rec).unwrap());
            global_pool().lock().await.remove(&rec.id);
            crate::harness::abort::unregister_cancel_handle(&rec.id);
        }
        *SPAWN_HOOK.lock().unwrap_or_else(|e| e.into_inner()) = previous_hook;
    }

    #[tokio::test]
    async fn refuse_path_with_no_persist_is_not_blocked_by_the_owner() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        let _env = crate::config::TestEnvGuard::capture(&[ENV_COORDINATOR_HOME]);
        unsafe { std::env::set_var(ENV_COORDINATOR_HOME, home.path()) };
        let mut reg = Registry::default();
        let rec = reg.add(proj.path(), ProjectAddOptions::default()).unwrap();
        reg.save(&crate::config::registry_path().unwrap()).unwrap();
        crate::run::run_stub(&rec, Some("0027".into())).unwrap();
        crate::state::with_run_state_lock(&rec, || {
            let mut s = crate::state::load_run_state(&rec)?;
            s.last_event = crate::harness::abort::RECYCLE_STALL_EVENT.into();
            crate::state::save_run_state(&rec, &s)
        })
        .unwrap();
        assert!(!persist_path(&rec).unwrap().exists());
        assert!(crate::harness::abort::should_refuse_reuse(&rec));

        let missing = proj.path().join("missing-grok-refuse.exe");
        let started = start_with_bin(Some(&rec.id), true, missing, None, false, "grok", None).await;
        let err = started.unwrap_err().to_string();
        assert!(
            !err.contains("persist-missing"),
            "an absent persist is nothing to terminate, not a refusal: {err}"
        );
        let (identity, _) = persist_identity(&rec).unwrap().unwrap();
        assert!(!identity.alive);
        assert!(
            identity.error.is_some(),
            "the real spawn failure is recorded"
        );
    }

    #[test]
    fn same_identity_covers_model_and_bin() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "same-identity");
        let mut a = attempt_handle(&rec, None);
        a.alive = true;
        a.session_id = Some("sess".into());
        a.holder_pid = Some(4242);
        a.model = Some("gpt-6.1-sol[medium]".into());
        a.bin = Some(PathBuf::from(r"C:\bin\codex-acp.cmd"));
        let base = PersistIdentity::from_handle(&a);

        let mut b = a.clone();
        b.model = Some("gpt-6-luna".into());
        assert!(
            !base.same_identity(&PersistIdentity::from_handle(&b)),
            "a model change between Ping and delivery is persist-changed"
        );

        let mut c = a.clone();
        c.bin = Some(PathBuf::from(r"C:\bin\other.exe"));
        assert!(!base.same_identity(&PersistIdentity::from_handle(&c)));

        // A pre-upgrade persist without model/bin still proves identity.
        let mut d = a.clone();
        d.model = None;
        assert!(base.same_identity(&PersistIdentity::from_handle(&d)));

        let mut e = a.clone();
        e.bin = None;
        assert!(base.same_identity(&PersistIdentity::from_handle(&e)));
    }

    // ---- 0121: persist-level atomic fence -----------------------------------

    /// A blocking gate for the in-fence pause seam.
    #[derive(Default)]
    struct Gate {
        open: std::sync::Mutex<bool>,
        cv: std::sync::Condvar,
    }

    impl Gate {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn release(&self) {
            *self.open.lock().unwrap_or_else(|e| e.into_inner()) = true;
            self.cv.notify_all();
        }

        fn wait(&self) {
            let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
            while !*open {
                open = self.cv.wait(open).unwrap_or_else(|e| e.into_inner());
            }
        }
    }

    /// Releases a paused writer if an assertion panics (no stuck sentinel/thread).
    struct GateRelease(Arc<Gate>);

    impl Drop for GateRelease {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    fn install_pause_hook(hook: PersistPauseHook) {
        *PERSIST_PAUSE_HOOK.lock().unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    /// A live thread holding the record's sentinel; dropping it releases the fence.
    struct HeldFence {
        release: std::sync::mpsc::Sender<()>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for HeldFence {
        fn drop(&mut self) {
            let _ = self.release.send(());
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn hold_sentinel(rec: &ProjectRecord) -> HeldFence {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let held = rec.clone();
        let thread = std::thread::spawn(move || {
            let _ = with_persist_lock(&held, |_| {
                let _ = ready_tx.send(());
                let _ = release_rx.recv();
                Ok(())
            });
        });
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the holder thread takes the sentinel");
        HeldFence {
            release: release_tx,
            thread: Some(thread),
        }
    }

    #[test]
    fn persist_lock_is_exclusive_between_handles() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let sentinel = dir.path().join(PERSIST_LOCK_FILE);
        let data = dir.path().join("harness-grok.json");
        let first = PersistLock::acquire(&sentinel, || Ok(data.clone())).unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<bool>();
        let second_sentinel = sentinel.clone();
        let second_data = data.clone();
        let second = std::thread::spawn(move || {
            let outcome = PersistLock::acquire(&second_sentinel, || Ok(second_data.clone()));
            let _ = done_tx.send(outcome.is_ok());
        });
        assert!(
            done_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "a second handle must not enter the critical section"
        );
        drop(first);
        assert!(
            done_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("the second acquire completes once the first drops"),
            "the second handle acquires after the first releases"
        );
        second.join().unwrap();
    }

    #[test]
    fn persist_lock_rejects_a_recursive_acquire() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "0121-recursive");
        with_persist_lock(&rec, |_| {
            let err = with_persist_lock(&rec, |_| Ok(())).unwrap_err();
            let text = err.to_string();
            assert!(
                text.contains("persist-lock-order: recursive"),
                "expected the named order refusal, got {text}"
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn persist_lock_reuses_the_sentinel_and_never_deletes_it() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "0121-sentinel");
        let sentinel = persist_lock_path(&rec).unwrap();
        for _ in 0..5 {
            with_persist_lock(&rec, |_| Ok(())).unwrap();
        }
        let first = std::fs::metadata(&sentinel).unwrap();
        assert_eq!(first.len(), 0, "the sentinel stays zero bytes");
        let created = first.created().ok();
        for _ in 0..2 {
            with_persist_lock(&rec, |_| Ok(())).unwrap();
        }
        let second = std::fs::metadata(&sentinel).unwrap();
        assert_eq!(second.len(), 0);
        assert_eq!(
            created,
            second.created().ok(),
            "the same file is reused, never recreated or deleted"
        );
        assert!(sentinel.exists());
    }

    #[test]
    fn persist_lock_is_released_on_panic() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "0121-panic");
        let started = std::time::Instant::now();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = with_persist_lock(&rec, |_| -> Result<()> {
                panic!("boom inside the fenced section");
            });
        }));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the unwinding closure releases the OS lock and the order depth"
        );
        with_persist_lock(&rec, |_| Ok(())).expect("the fence is reusable after a panic");
    }

    fn unreref(rec: &ProjectRecord) -> PersistRef {
        PersistRef::new(attempt_identity(rec, "grok"), SystemTime::now())
    }

    #[test]
    fn old_child_error_cannot_land_inside_the_compare_act_window() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "0121-t1");
        let a = claim_start(&rec, "grok").unwrap();

        let gate = Gate::new();
        let release = GateRelease(gate.clone());
        let (compared_tx, compared_rx) = std::sync::mpsc::channel::<()>();
        {
            let gate = gate.clone();
            install_pause_hook(Arc::new(move |_label: &str| {
                let _ = compared_tx.send(());
                gate.wait();
            }));
        }

        let o_rec = rec.clone();
        let o_a = a.clone();
        let old_child = std::thread::spawn(move || {
            arm_persist_pause();
            mark_start_error(&o_rec, Some(&o_a), "timed out")
        });
        compared_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the old child compared before the retry starts");

        let (done_tx, done_rx) = std::sync::mpsc::channel::<(String, Option<String>, bool)>();
        let r_rec = rec.clone();
        let r_a = a.clone();
        let retry = std::thread::spawn(move || match clear_persist_if_outcome(&r_rec, &r_a) {
            PersistAct::Ok => {
                let claimed = claim_start(&r_rec, "grok").is_ok();
                let _ = done_tx.send(("ok".into(), None, claimed));
            }
            PersistAct::Refused => {
                let _ = done_tx.send(("refused".into(), None, false));
            }
            PersistAct::Unavailable(e) => {
                let _ = done_tx.send(("unavailable".into(), Some(e.to_string()), false));
            }
        });
        assert!(
            done_rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "the retry entered the compare/act window"
        );
        drop(release);
        assert!(
            old_child.join().unwrap(),
            "the old child's error record is the one write that happened"
        );
        let (kind, err, claimed) = done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the retry finishes after the fence is released");
        assert_eq!(kind, "refused", "compare refusal, not a timeout: {err:?}");
        assert!(!claimed, "the retry never claims");
        retry.join().unwrap();

        let (identity, _) = persist_identity(&rec).unwrap().unwrap();
        assert_eq!(identity.error.as_deref(), Some("timed out"));
        assert!(!identity.is_attempt() || identity.error.is_some());

        // Phase 2: the newer claim is unchanged by any stale writer.
        let failed = ref_of(&rec);
        assert!(clear_persist_if(&rec, &failed));
        let _b = claim_start(&rec, "grok").unwrap();
        set_file_mtime(&rec, a.mtime + Duration::from_secs(1));
        let b = ref_of(&rec);
        assert_ne!(a.mtime, b.mtime, "forced distinct attempt stamps");
        let bytes = file_bytes(&rec);
        let mtime = std::fs::metadata(persist_path(&rec).unwrap())
            .unwrap()
            .modified()
            .unwrap();
        let session = mock_session_for(&rec, "sess-t1");
        assert!(
            !mark_start_error(&rec, Some(&a), "late child error"),
            "the stale child cannot clobber the retry's claim"
        );
        assert!(
            !write_holder_ready(&rec, &session, None, Some(&a)),
            "the stale child cannot publish ready over the retry's claim"
        );
        assert_eq!(bytes, file_bytes(&rec), "B's bytes are unchanged");
        assert_eq!(
            mtime,
            std::fs::metadata(persist_path(&rec).unwrap())
                .unwrap()
                .modified()
                .unwrap(),
            "B's mtime is unchanged"
        );
        clear_persist_hooks();
    }

    #[test]
    fn old_child_ready_cannot_land_inside_the_window() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "0121-t2");
        let a = claim_start(&rec, "grok").unwrap();

        let gate = Gate::new();
        let release = GateRelease(gate.clone());
        let (compared_tx, compared_rx) = std::sync::mpsc::channel::<()>();
        {
            let gate = gate.clone();
            install_pause_hook(Arc::new(move |_label: &str| {
                let _ = compared_tx.send(());
                gate.wait();
            }));
        }

        let o_rec = rec.clone();
        let o_a = a.clone();
        let old_child = std::thread::spawn(move || {
            let session = mock_session_for(&o_rec, "sess-t2-child");
            arm_persist_pause();
            write_holder_ready(&o_rec, &session, Some("127.0.0.1:9".into()), Some(&o_a))
        });
        compared_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the old child compared before the retry starts");

        let (done_tx, done_rx) = std::sync::mpsc::channel::<(String, Option<String>, bool)>();
        let r_rec = rec.clone();
        let r_a = a.clone();
        let retry = std::thread::spawn(move || match clear_persist_if_outcome(&r_rec, &r_a) {
            PersistAct::Ok => {
                let claimed = claim_start(&r_rec, "grok").is_ok();
                let _ = done_tx.send(("ok".into(), None, claimed));
            }
            PersistAct::Refused => {
                let _ = done_tx.send(("refused".into(), None, false));
            }
            PersistAct::Unavailable(e) => {
                let _ = done_tx.send(("unavailable".into(), Some(e.to_string()), false));
            }
        });
        assert!(
            done_rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "the retry entered the compare/act window"
        );
        drop(release);
        assert!(
            old_child.join().unwrap(),
            "the old child published its ready record"
        );
        let (kind, err, claimed) = done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(kind, "refused", "{err:?}");
        assert!(!claimed);
        retry.join().unwrap();

        // Phase 2: a stale ready write cannot land over the newer claim.
        let ready = ref_of(&rec);
        assert!(clear_persist_if(&rec, &ready));
        let _b = claim_start(&rec, "grok").unwrap();
        set_file_mtime(&rec, a.mtime + Duration::from_secs(1));
        let b = ref_of(&rec);
        assert_ne!(a.mtime, b.mtime);
        let bytes = file_bytes(&rec);
        let session = mock_session_for(&rec, "sess-t2-stale");
        assert!(!write_holder_ready(
            &rec,
            &session,
            Some("127.0.0.1:9".into()),
            Some(&a)
        ));
        assert_eq!(bytes, file_bytes(&rec));
        clear_persist_hooks();
    }

    #[test]
    fn ready_contention_does_not_tear_down_a_healthy_session() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "0121-t2b");
        let claim = claim_start(&rec, "grok").unwrap();
        let before = file_bytes(&rec);
        let held = hold_sentinel(&rec);
        let mut session = mock_session_for(&rec, "sess-t2b");
        let err = {
            let _budget = test_persist_lock_budget(Duration::from_millis(50));
            futures_lite_block_on(publish_ready_or_teardown(
                &rec,
                &mut session,
                None,
                Some(&claim),
            ))
            .unwrap_err()
        };
        let text = err.to_string();
        assert!(text.contains("persist-lock-timeout"), "{text}");
        assert!(!text.contains("persist-changed"), "{text}");
        assert_eq!(
            session.shutdown_probe(),
            0,
            "contention never tears down a healthy session"
        );
        assert_eq!(before, file_bytes(&rec));
        drop(held);
        assert!(
            write_holder_ready(&rec, &session, None, Some(&claim)),
            "the fence is free once the holder releases"
        );
        clear_persist_hooks();
    }

    fn seed_named(rec: &ProjectRecord) {
        write_handle(
            rec,
            &make_handle(
                rec,
                Some("sess-old"),
                Some(crate::harness::grok_cwd(rec).expect("cwd")),
                Some("127.0.0.1:9".into()),
                Some(4242),
                true,
                false,
                None,
            ),
        );
    }

    fn run_merge_writer(rec: &ProjectRecord, writer: &str, session: Option<GrokSession>) {
        match writer {
            "flag" => set_prompt_in_flight(rec, true),
            _ => remember_fidelity(rec, session.as_ref().unwrap()),
        }
    }

    fn merge_writer_session(rec: &ProjectRecord, writer: &str) -> Option<GrokSession> {
        if writer == "fidelity" {
            let mut session = mock_session_for(rec, "sess-old");
            session.last_signal = Some("fid-t3".into());
            Some(session)
        } else {
            None
        }
    }

    type RetryReport = (String, Option<String>, bool);
    type RetryRun = (
        std::thread::JoinHandle<()>,
        std::sync::mpsc::Receiver<RetryReport>,
    );

    fn spawn_retry(rec: &ProjectRecord, old: &PersistRef, arm_pause: bool) -> RetryRun {
        let (done_tx, done_rx) = std::sync::mpsc::channel::<RetryReport>();
        let r_rec = rec.clone();
        let r_old = old.clone();
        let thread = std::thread::spawn(move || {
            if arm_pause {
                arm_persist_pause();
            }
            match clear_persist_if_outcome(&r_rec, &r_old) {
                PersistAct::Ok => {
                    let claimed = claim_start(&r_rec, "grok").is_ok();
                    let _ = done_tx.send(("ok".into(), None, claimed));
                }
                PersistAct::Refused => {
                    let _ = done_tx.send(("refused".into(), None, false));
                }
                PersistAct::Unavailable(e) => {
                    let _ = done_tx.send(("unavailable".into(), Some(e.to_string()), false));
                }
            }
        });
        (thread, done_rx)
    }

    fn assert_final_is_retry_claim(rec: &ProjectRecord) {
        let (identity, _) = persist_identity(rec).unwrap().unwrap();
        assert!(
            identity.is_attempt(),
            "the retry's claim is the final record"
        );
        assert_eq!(
            identity.session_id, None,
            "the old session was not resurrected"
        );
        assert_eq!(identity.cwd, None);
        assert_eq!(identity.holder_pid, None);
        assert_eq!(identity.pid, None);
        assert!(!identity.prompt_in_flight);
        // (c) Capture B, then run the stale writers that lost the interleaving: neither
        // may rewrite a byte of the retry's claim.
        let b_bytes = file_bytes(rec);
        let stale = PersistRef::new(
            attempt_identity(rec, "grok"),
            SystemTime::now() - Duration::from_secs(3600),
        );
        assert!(
            !mark_start_error(rec, Some(&stale), "stale merge writer"),
            "a stale attempt cannot rewrite B"
        );
        let stale_session = mock_session_for(rec, "sess-stale");
        assert!(
            !write_holder_ready(rec, &stale_session, None, Some(&stale)),
            "a stale ready write cannot publish over B"
        );
        assert_eq!(
            b_bytes,
            file_bytes(rec),
            "B's bytes are unchanged after the stale writers"
        );
    }

    #[test]
    fn fidelity_and_flag_writes_cannot_clobber_a_newer_claim() {
        let _guard = test_env_lock();
        for writer in ["flag", "fidelity"] {
            // ---- Phase A (the discriminator): while the merge writer is paused after
            // its snapshot, the retry must not be able to clear+claim. Without the fence
            // the retry completes inside the window and the merge then overwrites B.
            let dir = tempdir().unwrap();
            let rec = record_in(&dir, &format!("0121-t3a-{writer}"));
            seed_named(&rec);
            let old = ref_of(&rec);
            let gate = Gate::new();
            let release = GateRelease(gate.clone());
            let (compared_tx, compared_rx) = std::sync::mpsc::channel::<()>();
            {
                let gate = gate.clone();
                install_pause_hook(Arc::new(move |_label: &str| {
                    let _ = compared_tx.send(());
                    gate.wait();
                }));
            }
            let o_rec = rec.clone();
            let o_writer = writer.to_string();
            let o_session = merge_writer_session(&rec, writer);
            let merge = std::thread::spawn(move || {
                arm_persist_pause();
                run_merge_writer(&o_rec, &o_writer, o_session);
            });
            compared_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("the merge writer paused after its snapshot");

            let (retry, done_rx) = spawn_retry(&rec, &old, false);
            assert!(
                done_rx.recv_timeout(Duration::from_millis(200)).is_err(),
                "the retry entered the merge/act window"
            );
            drop(release);
            merge.join().unwrap();
            let (kind, err, claimed) = done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(kind, "ok", "{err:?}");
            assert!(claimed);
            retry.join().unwrap();
            assert_final_is_retry_claim(&rec);

            // ---- Phase B (the spec's observation): pause the retry just before its
            // unlink, so the merge's landing on the old record is directly visible.
            let dir = tempdir().unwrap();
            let rec = record_in(&dir, &format!("0121-t3b-{writer}"));
            seed_named(&rec);
            let old = ref_of(&rec);
            let gate_o = Gate::new();
            let gate_r = Gate::new();
            let release_o = GateRelease(gate_o.clone());
            let release_r = GateRelease(gate_r.clone());
            let (compared_tx, compared_rx) = std::sync::mpsc::channel::<()>();
            let (clear_tx, clear_rx) = std::sync::mpsc::channel::<()>();
            {
                let gate_o = gate_o.clone();
                let gate_r = gate_r.clone();
                install_pause_hook(Arc::new(move |label: &str| {
                    if label == "clear" {
                        let _ = clear_tx.send(());
                        gate_r.wait();
                    } else {
                        let _ = compared_tx.send(());
                        gate_o.wait();
                    }
                }));
            }
            let o_rec = rec.clone();
            let o_writer = writer.to_string();
            let o_session = merge_writer_session(&rec, writer);
            let merge = std::thread::spawn(move || {
                arm_persist_pause();
                run_merge_writer(&o_rec, &o_writer, o_session);
            });
            compared_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("the merge writer paused after its snapshot");
            let (retry, done_rx) = spawn_retry(&rec, &old, true);
            assert!(
                done_rx.recv_timeout(Duration::from_millis(200)).is_err(),
                "the retry entered the merge/act window"
            );
            drop(release_o);
            merge.join().unwrap();
            clear_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("the retry reached its own clear under the fence");
            // (a) The merge landed on the old record while the retry is paused.
            let merged = load_persist(&rec).unwrap().expect("old record present");
            if writer == "flag" {
                assert!(
                    merged.prompt_in_flight,
                    "the flag write landed on the old record"
                );
            } else {
                assert_eq!(merged.last_signal.as_deref(), Some("fid-t3"));
            }
            drop(release_r);
            let (kind, err, claimed) = done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(kind, "ok", "{err:?}");
            assert!(claimed);
            retry.join().unwrap();
            // (b) + (c) the final record is B and stays B.
            assert_final_is_retry_claim(&rec);
            clear_persist_hooks();
        }
    }

    struct ChildGuard(std::process::Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn persist_lock_child_holds_until_killed() {
        let Some(path) = std::env::var_os("COORDINATOR_TEST_PERSIST_LOCK_PATH") else {
            return;
        };
        let Some(marker) = std::env::var_os("COORDINATOR_TEST_PERSIST_LOCK_MARKER") else {
            return;
        };
        let sentinel = PathBuf::from(path);
        let _lock =
            PersistLock::acquire(&sentinel, || Ok(sentinel.with_file_name("unused.json"))).unwrap();
        std::fs::write(marker, b"ready").unwrap();
        // Bounded so a leaked child cannot outlive the test binary for long.
        for _ in 0..30 {
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    #[test]
    fn persist_lock_survives_a_crashed_holder() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let mut rec = record_in(&dir, "0121-t4");
        // Pin the parent record to the child's state dir: `persist_lock_path` is exactly
        // the sentinel the child holds (not `<dir>/state/.harness-grok.lock`).
        let child_temp = tempdir().unwrap();
        rec.state_dir = Some(child_temp.path().to_path_buf());
        let sentinel = child_temp.path().join(PERSIST_LOCK_FILE);
        assert_eq!(persist_lock_path(&rec).unwrap(), sentinel);
        let marker = child_temp.path().join("child-ready.marker");
        let exe = std::env::current_exe().unwrap();
        let mut cmd = std::process::Command::new(exe);
        cmd.args([
            "--exact",
            "harness::pool::tests::persist_lock_child_holds_until_killed",
            "--nocapture",
        ]);
        cmd.env("COORDINATOR_TEST_PERSIST_LOCK_PATH", &sentinel);
        cmd.env("COORDINATOR_TEST_PERSIST_LOCK_MARKER", &marker);
        let child = ChildGuard(cmd.spawn().unwrap());

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !marker.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "the child never took the sentinel"
            );
            std::thread::sleep(Duration::from_millis(25));
        }

        let handle = attempt_handle(&rec, None);
        {
            let _budget = test_persist_lock_budget(Duration::from_millis(50));
            let err = with_persist_lock(&rec, |lock| save_persist_locked(lock, &handle))
                .unwrap_err()
                .to_string();
            assert!(err.contains("persist-lock-timeout"), "{err}");
        }

        let mut child = child;
        let _ = child.0.kill();
        let _ = child.0.wait();
        let mut acquired = false;
        for _ in 0..200 {
            if with_persist_lock(&rec, |lock| save_persist_locked(lock, &handle)).is_ok() {
                acquired = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            acquired,
            "a killed holder must not wedge the next production writer"
        );
        assert!(sentinel.exists(), "the sentinel is reusable");
        assert_eq!(std::fs::metadata(&sentinel).unwrap().len(), 0);
    }

    #[test]
    fn readers_stay_lock_free_while_a_writer_holds_the_lock() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "0121-t5");
        let a = claim_start(&rec, "grok").unwrap();
        let before = file_bytes(&rec);

        let gate = Gate::new();
        let release = GateRelease(gate.clone());
        let (compared_tx, compared_rx) = std::sync::mpsc::channel::<()>();
        {
            let gate = gate.clone();
            install_pause_hook(Arc::new(move |_label: &str| {
                let _ = compared_tx.send(());
                gate.wait();
            }));
        }
        let o_rec = rec.clone();
        let writer = std::thread::spawn(move || {
            arm_persist_pause();
            mark_start_error(&o_rec, Some(&a), "timed out")
        });
        compared_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        // A lock-free reader must still see the pre-write record.
        let (identity, _) = persist_identity(&rec).unwrap().unwrap();
        assert!(
            identity.error.is_none(),
            "the reader sees the pre-write record"
        );
        assert!(status_bundle_sync(&rec).is_none());
        assert_eq!(before, file_bytes(&rec));

        drop(release);
        assert!(writer.join().unwrap());
        clear_persist_hooks();
    }

    #[test]
    fn lock_order_is_run_state_then_persist() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "0121-t6");
        crate::state::with_run_state_lock(&rec, || with_persist_lock(&rec, |_| Ok(())))
            .expect("run_state -> persist is the legal order");
        let err = with_persist_lock(&rec, |_| crate::state::with_run_state_lock(&rec, || Ok(())))
            .unwrap_err();
        assert!(err.to_string().contains("persist-lock-order"), "{err}");
        let state_dir = resolve_state_dir(&rec).unwrap();
        assert!(
            !state_dir.join(".run-state.lock").exists(),
            "the reverse acquisition never creates the run-state lock"
        );
        assert!(persist_lock_path(&rec).unwrap().exists());
    }

    #[test]
    fn persist_lock_timeout_is_named_and_writes_nothing() {
        let _guard = test_env_lock();

        // claim_start
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "0121-t7-claim");
        let held = hold_sentinel(&rec);
        {
            let _budget = test_persist_lock_budget(Duration::from_millis(50));
            let err = claim_start(&rec, "grok").unwrap_err().to_string();
            assert!(err.contains("persist-lock-timeout"), "{err}");
        }
        drop(held);
        assert!(!persist_path(&rec).unwrap().exists());

        // W2: a claim the writer would accept.
        let dir = tempdir().unwrap();
        let rec_w2 = record_in(&dir, "0121-t7-w2");
        let claim = claim_start(&rec_w2, "grok").unwrap();
        let before = file_bytes(&rec_w2);
        let held = hold_sentinel(&rec_w2);
        {
            let _budget = test_persist_lock_budget(Duration::from_millis(50));
            match mark_start_error_outcome(&rec_w2, Some(&claim), "timed out") {
                PersistAct::Unavailable(e) => {
                    assert!(e.to_string().contains("persist-lock-timeout"), "{e}");
                }
                other => panic!("expected Unavailable, got {other:?}"),
            }
        }
        drop(held);
        assert_eq!(before, file_bytes(&rec_w2));

        // W4: the live record the exit writer would accept.
        let dir = tempdir().unwrap();
        let rec_w4 = record_in(&dir, "0121-t7-w4");
        let session = mock_session_for(&rec_w4, "sess-t7");
        write_handle(
            &rec_w4,
            &ready_handle(&rec_w4, &session, Some("127.0.0.1:9".into())),
        );
        let before = file_bytes(&rec_w4);
        let held = hold_sentinel(&rec_w4);
        {
            let _budget = test_persist_lock_budget(Duration::from_millis(50));
            match write_holder_exit_outcome(&rec_w4, &session) {
                PersistAct::Unavailable(e) => {
                    assert!(e.to_string().contains("persist-lock-timeout"), "{e}");
                }
                other => panic!("expected Unavailable, got {other:?}"),
            }
        }
        drop(held);
        assert_eq!(before, file_bytes(&rec_w4));

        // W8/W9: a named record, no write, one `persist-lock` status line.
        let dir = tempdir().unwrap();
        let rec_flag = record_in(&dir, "0121-t7-flag");
        write_handle(
            &rec_flag,
            &make_handle(
                &rec_flag,
                Some("sess-flag"),
                Some(crate::harness::grok_cwd(&rec_flag).expect("cwd")),
                Some("127.0.0.1:9".into()),
                Some(4242),
                true,
                false,
                None,
            ),
        );
        let before = file_bytes(&rec_flag);
        let held = hold_sentinel(&rec_flag);
        {
            let _budget = test_persist_lock_budget(Duration::from_millis(50));
            set_prompt_in_flight(&rec_flag, true);
            remember_fidelity(&rec_flag, &session);
        }
        drop(held);
        assert_eq!(before, file_bytes(&rec_flag));
        let status = std::fs::read_to_string(crate::progress_log::path(&rec_flag)).unwrap();
        assert!(status.contains("persist-lock"), "{status}");

        // W10: the sync helper refuses; the shutdown-equivalent reports on-disk truth.
        let dir = tempdir().unwrap();
        let rec_w10 = record_in(&dir, "0121-t7-w10");
        let live = make_handle(
            &rec_w10,
            Some("sess-w10"),
            Some(crate::harness::grok_cwd(&rec_w10).expect("cwd")),
            Some("127.0.0.1:9".into()),
            Some(4242),
            true,
            false,
            None,
        );
        write_handle(&rec_w10, &live);
        let on_disk = current_persist_status(&rec_w10);
        let dead = persist_marked_dead(&live);
        let held = hold_sentinel(&rec_w10);
        {
            let _budget = test_persist_lock_budget(Duration::from_millis(50));
            let err = save_in_process_shutdown(&rec_w10, &dead)
                .unwrap_err()
                .to_string();
            assert!(err.contains("persist-lock-timeout"), "{err}");
        }
        drop(held);
        assert_eq!(
            on_disk,
            current_persist_status(&rec_w10),
            "nothing was written: the truthful on-disk status, not the dead one"
        );
    }

    #[test]
    fn snapshot_pair_is_coherent_under_a_mid_read_replacement() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "0121-t8");
        write_handle(&rec, &attempt_handle(&rec, Some("a")));
        let a_mtime = std::fs::metadata(persist_path(&rec).unwrap())
            .unwrap()
            .modified()
            .unwrap();
        let b_mtime = Arc::new(std::sync::Mutex::new(None));
        let rec_hook = rec.clone();
        let slot = b_mtime.clone();
        arm_persist_snapshot_hook(move || {
            save_persist(&rec_hook, &attempt_handle(&rec_hook, Some("b"))).unwrap();
            let forced = a_mtime + Duration::from_secs(1);
            set_file_mtime(&rec_hook, forced);
            let mtime = std::fs::metadata(persist_path(&rec_hook).unwrap())
                .unwrap()
                .modified()
                .unwrap();
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(mtime);
        });
        let result = persist_identity(&rec);
        let b_mtime = b_mtime
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .expect("the snapshot hook fired");
        assert_ne!(a_mtime, b_mtime);
        match result {
            Ok(Some((identity, mtime))) => {
                assert_eq!(identity.error.as_deref(), Some("b"), "content is B");
                assert_eq!(mtime, b_mtime, "the mtime is B's");
            }
            Ok(None) => panic!("the record is present"),
            Err(e) => assert!(e.to_string().contains("persist-unstable"), "{e}"),
        }
        clear_persist_hooks();

        // The existing typed outcomes are unchanged.
        std::fs::write(persist_path(&rec).unwrap(), b"{not json").unwrap();
        assert!(persist_identity(&rec).is_err(), "corrupt is a typed error");
        std::fs::remove_file(persist_path(&rec).unwrap()).unwrap();
        assert!(
            persist_identity(&rec).unwrap().is_none(),
            "absent stays Ok(None)"
        );
    }

    struct ReadonlyGuard {
        path: PathBuf,
        original: std::fs::Permissions,
    }

    impl Drop for ReadonlyGuard {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.path, self.original.clone());
        }
    }

    #[test]
    fn persist_lock_io_is_named_and_fails_closed() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "0121-t9");
        let sentinel = persist_lock_path(&rec).unwrap();
        std::fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
        std::fs::write(&sentinel, b"").unwrap();
        write_handle(&rec, &attempt_handle(&rec, None));
        let before = file_bytes(&rec);
        let old = ref_of(&rec);
        let original = std::fs::metadata(&sentinel).unwrap().permissions();
        let mut perms = original.clone();
        perms.set_readonly(true);
        std::fs::set_permissions(&sentinel, perms).unwrap();
        let restore = ReadonlyGuard {
            path: sentinel.clone(),
            original,
        };

        let err = claim_start(&rec, "grok").unwrap_err().to_string();
        assert!(err.contains("persist-lock-io"), "{err}");
        assert!(!err.contains("persist-lock-timeout"), "{err}");
        match clear_persist_if_outcome(&rec, &old) {
            PersistAct::Unavailable(e) => {
                let text = e.to_string();
                assert!(text.contains("persist-lock-io"), "{text}");
                assert!(!text.contains("persist-lock-timeout"), "{text}");
            }
            other => panic!("expected persist-lock-io, got {other:?}"),
        }
        assert_eq!(before, file_bytes(&rec), "no record was written");
        drop(restore);

        // A state dir whose parent is a regular file fails the same way.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let mut bad = record_in(&dir, "0121-t9-dir");
        bad.state_dir = Some(blocker.join("state"));
        let err = claim_start(&bad, "grok").unwrap_err().to_string();
        assert!(err.contains("persist-lock-io"), "{err}");
        assert!(!err.contains("persist-lock-timeout"), "{err}");
    }

    #[test]
    fn clear_on_absent_persist_does_not_create_the_state_dir() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "0121-t10");
        let state = resolve_state_dir(&rec).unwrap();
        assert!(!state.exists());
        let old = unreref(&rec);
        assert!(clear_persist_if(&rec, &old), "absent is the clear verdict");
        assert!(mark_dead_if(&rec, &old), "absent is the dead verdict");
        assert!(!state.exists(), "the absent fast path creates no state dir");
        assert!(!persist_lock_path(&rec).unwrap().exists());

        claim_start(&rec, "grok").expect("a claim creates the state dir and sentinel");
        assert!(state.exists());
        assert!(persist_lock_path(&rec).unwrap().exists());
    }

    #[test]
    fn snapshot_absence_is_only_first_metadata_not_found() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = record_in(&dir, "0121-t11");
        write_handle(&rec, &attempt_handle(&rec, None));
        let path = persist_path(&rec).unwrap();
        let remove = path.clone();
        arm_persist_snapshot_hook(move || {
            let _ = std::fs::remove_file(&remove);
        });
        reset_persist_snapshot_attempts();
        assert!(persist_identity(&rec).unwrap().is_none());
        assert_eq!(
            persist_snapshot_attempts(),
            2,
            "a None after a successful metadata is a change and must retry"
        );
        assert!(!path.exists());
        clear_persist_hooks();
    }

    // ---- 0117: the single-owner source scan --------------------------------

    /// A **known-token regression guard, not semantic proof** (agy-m1). The real
    /// guarantee is the private `VerifiedHolder` + the deleted functions.
    ///
    /// Production shard = each scan-set file truncated at its **own** `mod tests`
    /// (`pool.rs` also has two earlier `#[cfg(test)]` regions at `apply_turn` and
    /// `hold_loop_with_session`; they are not test modules and stay in the shard).
    #[test]
    fn termination_single_owner_scan() {
        let files: [(&str, &str); 9] = [
            ("pool", include_str!("pool.rs")),
            ("reap", include_str!("reap.rs")),
            ("grok", include_str!("grok.rs")),
            ("abort", include_str!("abort.rs")),
            ("worktree", include_str!("../worktree.rs")),
            ("drive", include_str!("../workflow/drive.rs")),
            ("state", include_str!("../state.rs")),
            ("api", include_str!("../api.rs")),
            ("cli", include_str!("../cli.rs")),
        ];
        let shards: Vec<(&str, &str)> = files
            .iter()
            .map(|(name, text)| {
                let cut = text.find("\nmod tests {").unwrap_or(text.len());
                (*name, &text[..cut])
            })
            .collect();
        let pool_shard = shards
            .iter()
            .find(|(name, _)| *name == "pool")
            .map(|(_, text)| *text)
            .unwrap();
        let joined: String = shards.iter().map(|(_, text)| *text).collect();

        assert_eq!(
            pool_shard.matches("&HoldRequest::Shutdown").count(),
            1,
            "exactly one Shutdown construction"
        );
        assert_eq!(pool_shard.matches("deliver_shutdown(").count(), 3);
        assert_eq!(pool_shard.matches("fn deliver_shutdown(").count(), 1);
        assert_eq!(pool_shard.matches("struct VerifiedHolder {").count(), 1);
        assert_eq!(pool_shard.matches("VerifiedHolder {").count(), 2);
        assert_eq!(pool_shard.matches("fn same_record(").count(), 1);
        assert_eq!(pool_shard.matches("create_new(true)").count(), 1);
        assert_eq!(pool_shard.matches("persist_marked_dead(").count(), 2);
        assert_eq!(pool_shard.matches("claim_start(").count(), 3);

        for token in [
            "kill_persist_pids",
            "kill_pid_best_effort",
            "kill_live_pids",
            "verify_holder_identity",
            "HolderReapRpc",
        ] {
            assert!(
                !joined.contains(token),
                "{token} must appear nowhere in the production shards"
            );
        }
        assert!(
            !pool_shard.contains("taskkill"),
            "taskkill is the ACP child's teardown in grok only"
        );
        assert!(
            shards
                .iter()
                .any(|(name, text)| *name == "grok" && text.contains("taskkill"))
        );

        for name in [
            "deliver_shutdown(",
            "wait_holder_gone(",
            "mark_start_error_outcome(",
            "write_holder_ready_outcome(",
            "write_holder_exit_outcome(",
            "clear_persist_if_outcome(",
            "mark_dead_if_outcome(",
        ] {
            let body = fn_body(pool_shard, name);
            assert!(
                code_only(body).contains("same_record("),
                "{name} must use the one compare rule"
            );
        }
        assert!(code_only(fn_body(pool_shard, "same_record(")).contains("is_attempt()"));

        for caller in [
            "reuse_or_reap_existing(",
            "recycle_without_pool_lock(",
            "shutdown(project:",
            "start_inner(",
        ] {
            let body = fn_body(pool_shard, caller);
            assert_eq!(
                body.matches("run_holder_work(").count(),
                1,
                "{caller} offloads the blocking seam exactly once"
            );
        }

        let start_holder = fn_body(pool_shard, "start_holder(");
        assert_eq!(start_holder.matches("claim_start(").count(), 1);
        let claim_at = start_holder.rfind("claim_start(").unwrap();
        let spawn_at = start_holder.find("spawn_holder_process(").unwrap();
        assert!(
            claim_at < spawn_at,
            "the claim is the last step before spawn"
        );
        assert!(
            !start_holder[claim_at..spawn_at].contains("save_persist("),
            "no write between the claim and the spawn"
        );

        let in_process = fn_body(pool_shard, "spawn_in_process(");
        assert_eq!(in_process.matches("claim_start(").count(), 1);
        let claim_at = in_process.rfind("claim_start(").unwrap();
        let start_at = in_process.find("GrokSession::start").unwrap();
        assert!(claim_at < start_at);
        assert!(
            !in_process[claim_at..start_at].contains("save_persist("),
            "no write between the claim and the ACP start"
        );

        // `remove_file(` is scoped to pool.rs's shard; drive.rs removes unrelated
        // mailbox/review files, so a whole-set scan is wrong.
        let remove_at = pool_shard.find("std::fs::remove_file(").unwrap();
        let enclosing = pool_shard[..remove_at]
            .rfind("fn ")
            .map(|i| &pool_shard[i..])
            .unwrap_or("");
        assert!(
            enclosing.starts_with("fn remove_persist_locked("),
            "remove_file( lives only in remove_persist_locked"
        );

        // No async worker sleeps on the thread.
        for (name, body) in async_fns(pool_shard) {
            assert!(
                !body.contains("std::thread::sleep"),
                "{name} must not block an async worker"
            );
        }
    }

    /// 0121 DoD-1/DoD-2: every persist writer body takes the fence, no async body takes
    /// it directly, and the three write primitives are the only write sites in the
    /// product. A token-only guard is not semantic proof; this is the inventory.
    #[test]
    fn every_persist_writer_takes_the_lock() {
        let files: [(&str, &str); 9] = [
            ("pool", include_str!("pool.rs")),
            ("reap", include_str!("reap.rs")),
            ("grok", include_str!("grok.rs")),
            ("abort", include_str!("abort.rs")),
            ("worktree", include_str!("../worktree.rs")),
            ("drive", include_str!("../workflow/drive.rs")),
            ("state", include_str!("../state.rs")),
            ("api", include_str!("../api.rs")),
            ("cli", include_str!("../cli.rs")),
        ];
        let shards: Vec<(&str, &str)> = files
            .iter()
            .map(|(name, text)| {
                let cut = text.find("\nmod tests {").unwrap_or(text.len());
                (*name, &text[..cut])
            })
            .collect();
        let pool_shard = shards
            .iter()
            .find(|(name, _)| *name == "pool")
            .map(|(_, text)| *text)
            .unwrap();
        let pool_code = code_only(pool_shard);

        for name in [
            "mark_start_error_outcome(",
            "write_holder_ready_outcome(",
            "write_holder_exit_outcome(",
            "clear_persist_if_outcome(",
            "mark_dead_if_outcome(",
            "claim_start(",
            "set_prompt_in_flight(",
            "remember_fidelity(",
            "save_in_process_shutdown(",
        ] {
            assert!(
                code_only(fn_body(pool_shard, name)).contains("with_persist_lock("),
                "{name} must own the fence"
            );
        }
        for (wrapper, twin) in [
            ("mark_start_error(", "mark_start_error_outcome("),
            ("write_holder_ready(", "write_holder_ready_outcome("),
            ("write_holder_exit(", "write_holder_exit_outcome("),
            ("clear_persist_if(", "clear_persist_if_outcome("),
            ("mark_dead_if(", "mark_dead_if_outcome("),
        ] {
            assert!(
                code_only(fn_body(pool_shard, wrapper)).contains(twin),
                "the public bool verdict {wrapper} delegates to {twin}"
            );
        }

        // W10 is not an exception: the async body calls the sync helper and never the lock.
        let shutdown = code_only(fn_body(pool_shard, "shutdown(project:"));
        assert!(shutdown.contains("save_in_process_shutdown("));
        assert!(!shutdown.contains("with_persist_lock("));

        for (name, body) in async_fns(pool_shard) {
            assert!(
                !code_only(&body).contains("with_persist_lock("),
                "{name} is async and must not acquire the persist lock directly"
            );
        }

        // One atomic writer, one remover, one exclusive create.
        assert_eq!(
            pool_code.matches("atomic_write_json(").count(),
            1,
            "atomic_write_json( has one production call site"
        );
        let write_at = pool_code.find("atomic_write_json(").unwrap();
        let write_fn = pool_code[..write_at]
            .rfind("fn ")
            .map(|i| &pool_code[i..])
            .unwrap_or("");
        assert!(
            write_fn.starts_with("fn save_persist_locked("),
            "the atomic write lives only in save_persist_locked"
        );
        assert_eq!(
            pool_code.matches("std::fs::remove_file(").count(),
            1,
            "remove_file( has one production call site"
        );
        let remove_at = pool_code.find("std::fs::remove_file(").unwrap();
        let remove_fn = pool_code[..remove_at]
            .rfind("fn ")
            .map(|i| &pool_code[i..])
            .unwrap_or("");
        assert!(
            remove_fn.starts_with("fn remove_persist_locked("),
            "the unlink lives only in remove_persist_locked"
        );
        assert_eq!(
            pool_code.matches("create_new(true)").count(),
            1,
            "create_new(true) has one production call site"
        );
        let create_at = pool_code.find("create_new(true)").unwrap();
        let create_fn = pool_code[..create_at]
            .rfind("fn ")
            .map(|i| &pool_code[i..])
            .unwrap_or("");
        assert!(
            create_fn.starts_with("fn create_persist_claim_locked("),
            "the exclusive create lives only in create_persist_claim_locked"
        );

        // `save_persist` is a test-only seed.
        assert_eq!(
            pool_code.matches("fn save_persist(").count(),
            1,
            "exactly one save_persist definition"
        );
        let save_at = pool_shard
            .find("fn save_persist(")
            .expect("the seed is defined");
        assert!(
            pool_shard[..save_at].trim_end().ends_with("#[cfg(test)]"),
            "save_persist is a #[cfg(test)] seed"
        );
        assert_eq!(
            pool_code.matches("save_persist(").count(),
            1,
            "no production caller of save_persist remains"
        );

        assert_eq!(pool_code.matches("const PERSIST_LOCK_FILE").count(), 1);
        for forbidden in [
            "remove_file(&persist_lock_path",
            "remove_file(persist_lock_path",
            "remove_dir(&persist_lock_path",
            "remove_dir(persist_lock_path",
        ] {
            assert!(
                !pool_code.contains(forbidden),
                "the sentinel is never deleted or renamed: {forbidden}"
            );
        }

        // The whole-product claim: no shard outside `pool` writes the persist target.
        for (name, shard) in &shards {
            let code = code_only(shard);
            for pattern in [
                "atomic_write_json(persist_path",
                "std::fs::write(persist_path",
                "std::fs::remove_file(persist_path",
            ] {
                assert!(
                    !code.contains(pattern),
                    "{name} must not write the holder persist directly ({pattern})"
                );
            }
        }
    }

    /// Strip comments, strings and char literals so a guard cannot be satisfied by prose.
    fn code_only(shard: &str) -> String {
        let bytes = shard.as_bytes();
        let mut out = String::with_capacity(shard.len());
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'"' => i = skip_string(bytes, i),
                b'\'' => match skip_char_or_lifetime(bytes, i) {
                    Some(next) => i = next,
                    None => {
                        out.push('\'');
                        i += 1;
                    }
                },
                b'/' if bytes.get(i + 1) == Some(&b'/') => {
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                }
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    i += 2;
                    while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                        i += 1;
                    }
                    i += 2;
                }
                other => {
                    out.push(if other.is_ascii() { other as char } else { ' ' });
                    i += 1;
                }
            }
        }
        out
    }

    /// Literal-aware body extractor: skips `"…"`, raw strings, char literals and
    /// `//` / `/* */` comments, and returns the text through the matching `}`.
    fn fn_body<'a>(shard: &'a str, sig_prefix: &str) -> &'a str {
        let needle = format!("fn {sig_prefix}");
        let found = shard.find(&needle).or_else(|| {
            // Generic parameters sit between the name and `(`.
            sig_prefix
                .strip_suffix('(')
                .and_then(|stem| shard.find(&format!("fn {stem}<")))
        });
        let start = found.unwrap_or_else(|| panic!("missing fn {sig_prefix}"));
        let bytes = shard.as_bytes();
        let mut i = start;
        let mut depth = 0i32;
        let mut seen_brace = false;
        while i < bytes.len() {
            match bytes[i] {
                b'"' => {
                    i = skip_string(bytes, i);
                    continue;
                }
                b'\'' => {
                    if let Some(next) = skip_char_or_lifetime(bytes, i) {
                        i = next;
                        continue;
                    }
                }
                b'/' if bytes.get(i + 1) == Some(&b'/') => {
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    i += 2;
                    while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                        i += 1;
                    }
                    i += 2;
                    continue;
                }
                b'{' => {
                    depth += 1;
                    seen_brace = true;
                }
                b'}' => {
                    depth -= 1;
                    if seen_brace && depth == 0 {
                        return &shard[start..=i];
                    }
                }
                _ => {}
            }
            i += 1;
        }
        &shard[start..]
    }

    fn skip_string(bytes: &[u8], start: usize) -> usize {
        let mut i = start + 1;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => i += 2,
                b'"' => return i + 1,
                _ => i += 1,
            }
        }
        bytes.len()
    }

    /// A char literal (`'a'`, `'\n'`) is consumed; a lifetime (`'a`) is not.
    fn skip_char_or_lifetime(bytes: &[u8], start: usize) -> Option<usize> {
        match bytes.get(start + 1) {
            Some(b'\\') => {
                let mut i = start + 2;
                while i < bytes.len() && bytes[i] != b'\'' {
                    i += 1;
                }
                Some(i + 1)
            }
            Some(_) if bytes.get(start + 2) == Some(&b'\'') => Some(start + 3),
            _ => None,
        }
    }

    fn async_fns(shard: &str) -> Vec<(String, String)> {
        let mut names: Vec<String> = Vec::new();
        let mut rest = shard;
        while let Some(at) = rest.find("async fn ") {
            rest = &rest[at + "async fn ".len()..];
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() && !names.contains(&name) {
                names.push(name);
            }
        }
        names
            .into_iter()
            .map(|name| {
                let body = fn_body(shard, &format!("{name}(")).to_string();
                (name, body)
            })
            .collect()
    }

    fn scrub_stamps(text: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        let mut out = String::new();
        let mut index = 0;
        while index < chars.len() {
            if stamp_at(&chars[index..]) {
                out.push_str("STAMP");
                while index < chars.len() && !chars[index].is_whitespace() {
                    index += 1;
                }
            } else {
                out.push(chars[index]);
                index += 1;
            }
        }
        out
    }

    fn stamp_at(chars: &[char]) -> bool {
        chars.len() >= 20
            && chars[0].is_ascii_digit()
            && chars[1].is_ascii_digit()
            && chars[2].is_ascii_digit()
            && chars[3].is_ascii_digit()
            && chars[4] == '-'
            && chars[10] == 'T'
    }

    fn scrub_json(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                map.remove("updated_at");
                map.remove("written_at");
                map.remove("last_applied_outcome_hash");
                for child in map.values_mut() {
                    scrub_json(child);
                }
            }
            serde_json::Value::Array(items) => {
                for child in items {
                    scrub_json(child);
                }
            }
            serde_json::Value::String(text) if stamp_at(&text.chars().collect::<Vec<_>>()) => {
                *text = "STAMP".into();
            }
            _ => {}
        }
    }

    fn leg_segment(before: &[u8], record: &ProjectRecord) -> String {
        let path = crate::progress_log::path(record);
        let after = std::fs::read(&path).unwrap_or_default();
        let start = before.len().min(after.len());
        String::from_utf8_lossy(&after[start..]).to_string()
    }

    fn evidence_details(segment: &str) -> Vec<String> {
        segment
            .lines()
            .filter_map(|line| line.split("  evidence  ").nth(1).map(str::to_string))
            .collect()
    }

    async fn no_change_turn(
        rec: &ProjectRecord,
        prepared: Option<&crate::workflow::execution_repo::PreparedInjection>,
        manual: bool,
    ) -> HarnessPromptView {
        let msg = crate::harness::no_change::no_change_message("implement", "codex", None);
        apply_turn_checked(
            rec,
            Ok(PromptResult {
                text: "tree unchanged".into(),
                stop_reason: Some("end_turn".into()),
            }),
            harness_stub(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
            Some(msg),
            prepared,
            None,
            manual,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn trusted_evidence_no_evidence_matches_current_decision() {
        let _guard = test_env_lock();
        let (rec, _home, proj) = implement_leg_fixture(crate::workflow::graph::PHASE_IMPLEMENT);
        crate::progress_log::set_test_rotate_bytes(Some(u64::MAX));
        struct ClearRotate;
        impl Drop for ClearRotate {
            fn drop(&mut self) {
                crate::progress_log::set_test_rotate_bytes(None);
            }
        }
        let _pin = ClearRotate;
        let baseline = crate::state::load_run_state(&rec).unwrap();
        let prepared = crate::workflow::execution_repo::PreparedInjection::for_target(target_for(
            &rec,
            &baseline,
            crate::workflow::graph::PHASE_IMPLEMENT,
            proj.path(),
        ));
        let root = crate::state::resolve_state_dir(&rec)
            .unwrap()
            .join("trusted-evidence");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_file(crate::notify::artifact::path(&rec).unwrap());
        let journal_before = std::fs::read(crate::progress_log::path(&rec)).unwrap_or_default();
        let absent = no_change_turn(&rec, Some(&prepared), false).await;
        let absent_segment = leg_segment(&journal_before, &rec);
        let absent_state = crate::state::load_run_state(&rec).unwrap();
        let absent_failure = crate::notify::artifact::read(&rec).unwrap().unwrap().body;
        assert!(evidence_details(&absent_segment).is_empty());
        assert_eq!(absent.failure_class, Some(FailureClass::HarnessCrash));
        assert!(absent.applied);

        crate::state::save_run_state(&rec, &baseline).unwrap();
        let _ = std::fs::remove_file(crate::notify::artifact::path(&rec).unwrap());
        crate::state::with_run_state_lock(&rec, || {
            crate::workflow::trusted_evidence::write_for_epoch(
                &rec,
                &baseline,
                crate::workflow::trusted_evidence::ProducerHook::WorkflowAutoStart,
            );
            Ok(())
        })
        .unwrap();
        let journal_before = std::fs::read(crate::progress_log::path(&rec)).unwrap_or_default();
        let current = no_change_turn(&rec, Some(&prepared), false).await;
        let current_segment = leg_segment(&journal_before, &rec);
        let current_state = crate::state::load_run_state(&rec).unwrap();
        let current_failure = crate::notify::artifact::read(&rec).unwrap().unwrap().body;
        let details = evidence_details(&current_segment);
        assert_eq!(details.len(), 1);
        assert!(details[0].starts_with(&format!(
            "current epoch={} phase=implement facts=",
            baseline.run_epoch
        )));
        assert_eq!(current.failure_class, absent.failure_class);
        assert_eq!(current.error, absent.error);
        assert_eq!(current.applied, absent.applied);
        assert_eq!(current.text, absent.text);
        let mut absent_view = serde_json::to_value(&absent).unwrap();
        let mut current_view = serde_json::to_value(&current).unwrap();
        scrub_json(&mut absent_view);
        scrub_json(&mut current_view);
        assert_eq!(absent_view, current_view);
        let mut absent_state_json = serde_json::to_value(&absent_state).unwrap();
        let mut current_state_json = serde_json::to_value(&current_state).unwrap();
        scrub_json(&mut absent_state_json);
        scrub_json(&mut current_state_json);
        assert_eq!(absent_state_json, current_state_json);
        assert_eq!(
            scrub_stamps(&absent_failure),
            scrub_stamps(&current_failure)
        );
        let mut absent_lines: Vec<_> = absent_segment
            .lines()
            .filter(|line| !line.contains("  evidence  "))
            .map(scrub_stamps)
            .collect();
        let mut current_lines: Vec<_> = current_segment
            .lines()
            .filter(|line| !line.contains("  evidence  "))
            .map(scrub_stamps)
            .collect();
        absent_lines.sort();
        current_lines.sort();
        assert_eq!(absent_lines, current_lines);
        let message = crate::harness::no_change::no_change_message("implement", "codex", None);
        let golden_last_event = if message.chars().count() <= 200 {
            format!(
                "outcome: failure class=harness_crash phase=implement source=adapter — {message}"
            )
        } else {
            let truncated: String = message.chars().take(200).collect();
            format!(
                "outcome: failure class=harness_crash phase=implement source=adapter — {truncated}…"
            )
        };
        assert_eq!(
            golden_last_event,
            "outcome: failure class=harness_crash phase=implement source=adapter — implement leg produced no changes: phase implement, harness codex, session mode adapter-default, last permission signal none; the tree is exactly as the leg found it. If the plan was already satisfied…"
        );
        assert_eq!(absent_state.last_event, golden_last_event);
        assert_eq!(current_state.last_event, golden_last_event);
        assert_eq!(absent_state.status, crate::state::RunStatus::Stopped);
        assert_eq!(absent_state.phase, crate::workflow::graph::PHASE_IMPLEMENT);
        assert!(absent_state.phase_started_at.is_none());
        assert_eq!(message, GOLDEN_NO_CHANGE_ERROR);
        assert_golden_no_change_view(&absent, baseline.run_epoch, "absent");
        assert_golden_no_change_view(&current, baseline.run_epoch, "current");
        assert_eq!(absent.error.as_deref(), Some(message.as_str()));
        assert_eq!(current.error.as_deref(), Some(message.as_str()));
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    const GOLDEN_NO_CHANGE_ERROR: &str = "implement leg produced no changes: phase implement, harness codex, session mode adapter-default, last permission signal none; the tree is exactly as the leg found it. If the plan was already satisfied, advance by hand; otherwise the adapter may be sandboxed read-only.";
    const GOLDEN_LAST_EVENT: &str = "outcome: failure class=harness_crash phase=implement source=adapter — implement leg produced no changes: phase implement, harness codex, session mode adapter-default, last permission signal none; the tree is exactly as the leg found it. If the plan was already satisfied…";

    fn assert_golden_no_change_view(view: &HarnessPromptView, epoch: u64, label: &str) {
        assert_eq!(view.text.as_deref(), Some("tree unchanged"), "{label}");
        assert_eq!(view.stop_reason.as_deref(), Some("end_turn"), "{label}");
        assert!(view.applied, "{label}");
        assert_eq!(view.skipped, None, "{label}");
        assert_eq!(
            view.error.as_deref(),
            Some(GOLDEN_NO_CHANGE_ERROR),
            "{label}"
        );
        assert_eq!(
            view.failure_class,
            Some(FailureClass::HarnessCrash),
            "{label}"
        );
        let status = view.status.as_ref().expect(label);
        assert_eq!(status.status, crate::state::RunStatus::Stopped, "{label}");
        assert_eq!(
            status.phase,
            crate::workflow::graph::PHASE_IMPLEMENT,
            "{label}"
        );
        assert_eq!(status.run_epoch, epoch, "{label}");
        assert_eq!(
            status.failure_class,
            Some(FailureClass::HarnessCrash),
            "{label}"
        );
        assert_eq!(status.last_event, GOLDEN_LAST_EVENT, "{label}");
        assert!(status.phase_started_at.is_none(), "{label}");
        assert!(status.ci.is_none(), "{label}");
        assert!(status.review.is_none(), "{label}");
        assert!(status.stall.is_none(), "{label}");
        assert!(status.next_track.is_none(), "{label}");
        let workflow = status.workflow.as_ref().expect(label);
        assert_eq!(
            workflow.id.as_deref(),
            Some(crate::workflow::graph::WORKFLOW_ID),
            "{label}"
        );
        assert_eq!(workflow.driver, "adapter", "{label}");
        assert!(workflow.pending_roles.is_empty(), "{label}");
        let harness = view.harness.as_ref().expect(label);
        assert!(harness.alive, "{label}");
        assert_eq!(harness.session_id.as_deref(), Some("sess-apply"), "{label}");
        assert!(harness.cwd.is_none(), "{label}");
        assert!(!harness.supports_compact, "{label}");
        assert!(harness.pid.is_none(), "{label}");
        assert_eq!(harness.adapter, "grok", "{label}");
        assert!(harness.context_usage.is_none(), "{label}");
        assert!(harness.last_signal.is_none(), "{label}");
        assert!(harness.last_tool_title.is_none(), "{label}");
        assert!(harness.model.is_none(), "{label}");
        assert!(harness.bin.is_none(), "{label}");
    }

    async fn assert_silent_leg(
        rec: &ProjectRecord,
        prepared: Option<&crate::workflow::execution_repo::PreparedInjection>,
        message: &str,
        label: &str,
    ) -> HarnessPromptView {
        let before = std::fs::read(crate::progress_log::path(rec)).unwrap_or_default();
        let view = no_change_turn(rec, prepared, false).await;
        let details = evidence_details(&leg_segment(&before, rec));
        assert!(details.is_empty(), "{label} appended {details:?}");
        assert_eq!(
            view.failure_class,
            Some(FailureClass::HarnessCrash),
            "{label}"
        );
        assert_eq!(view.text.as_deref(), Some("tree unchanged"), "{label}");
        assert_eq!(view.stop_reason.as_deref(), Some("end_turn"), "{label}");
        assert_eq!(view.skipped, None, "{label}");
        assert_eq!(message, GOLDEN_NO_CHANGE_ERROR, "{label}");
        let state = crate::state::load_run_state(rec).unwrap();
        assert_golden_no_change_view(&view, state.run_epoch, label);
        assert_eq!(state.last_event, GOLDEN_LAST_EVENT, "{label}");
        assert_eq!(state.status, crate::state::RunStatus::Stopped, "{label}");
        assert_eq!(
            state.phase,
            crate::workflow::graph::PHASE_IMPLEMENT,
            "{label}"
        );
        assert_eq!(
            state.failure_class,
            Some(FailureClass::HarnessCrash),
            "{label}"
        );
        view
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn trusted_evidence_every_no_evidence_reason_is_silent() {
        let _guard = test_env_lock();
        let (rec, _home, proj) = implement_leg_fixture(crate::workflow::graph::PHASE_IMPLEMENT);
        let baseline = crate::state::load_run_state(&rec).unwrap();
        let prepared = crate::workflow::execution_repo::PreparedInjection::for_target(target_for(
            &rec,
            &baseline,
            crate::workflow::graph::PHASE_IMPLEMENT,
            proj.path(),
        ));
        let message = crate::harness::no_change::no_change_message("implement", "codex", None);
        let restore = || {
            crate::state::save_run_state(&rec, &baseline).unwrap();
            if let Ok(path) = crate::notify::artifact::path(&rec) {
                let _ = std::fs::remove_file(path);
            }
        };
        let root = crate::state::resolve_state_dir(&rec)
            .unwrap()
            .join("trusted-evidence");
        let leaf = root.join("current.json");

        crate::progress_log::set_test_rotate_bytes(Some(u64::MAX));
        struct ClearRotate;
        impl Drop for ClearRotate {
            fn drop(&mut self) {
                crate::progress_log::set_test_rotate_bytes(None);
            }
        }
        let _pin = ClearRotate;
        let _ = std::fs::remove_dir_all(&root);
        assert_silent_leg(&rec, Some(&prepared), &message, "absent").await;
        restore();

        let mut wrong = baseline.clone();
        wrong.run_epoch = baseline.run_epoch.saturating_add(3);
        crate::state::with_run_state_lock(&rec, || {
            crate::workflow::trusted_evidence::write_for_epoch(
                &rec,
                &wrong,
                crate::workflow::trusted_evidence::ProducerHook::RunBeginFreshRun,
            );
            Ok(())
        })
        .unwrap();
        assert_silent_leg(&rec, Some(&prepared), &message, "wrong-epoch").await;
        restore();

        let skill = crate::workflow::reuse::execution_skill_path_for_epoch(
            &rec,
            baseline.run_epoch,
            "implement",
        );
        if let Some(parent) = skill.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&skill, "before\n").unwrap();
        crate::state::with_run_state_lock(&rec, || {
            crate::workflow::trusted_evidence::write_for_epoch(
                &rec,
                &baseline,
                crate::workflow::trusted_evidence::ProducerHook::RunBeginFreshRun,
            );
            Ok(())
        })
        .unwrap();
        std::fs::write(&skill, "after\n").unwrap();
        assert_silent_leg(&rec, Some(&prepared), &message, "fact-drifted").await;
        restore();

        crate::state::with_run_state_lock(&rec, || {
            crate::workflow::trusted_evidence::write_for_epoch(
                &rec,
                &baseline,
                crate::workflow::trusted_evidence::ProducerHook::RunBeginFreshRun,
            );
            Ok(())
        })
        .unwrap();
        std::fs::write(&leaf, b"{").unwrap();
        assert_silent_leg(&rec, Some(&prepared), &message, "malformed").await;
        restore();

        crate::state::with_run_state_lock(&rec, || {
            crate::workflow::trusted_evidence::write_for_epoch(
                &rec,
                &baseline,
                crate::workflow::trusted_evidence::ProducerHook::RunBeginFreshRun,
            );
            Ok(())
        })
        .unwrap();
        let raced = leaf.clone();
        let _race = crate::workflow::trusted_evidence::BetweenReadsGuard::arm(move || {
            std::fs::write(&raced, b"{\"replaced\":true}").unwrap();
        });
        assert_silent_leg(&rec, Some(&prepared), &message, "capture-race").await;
        drop(_race);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn trusted_evidence_current_oversize_still_journals_one_line() {
        let _guard = test_env_lock();
        let (rec, _home, _proj) = implement_leg_fixture(crate::workflow::graph::PHASE_IMPLEMENT);
        let baseline = crate::state::load_run_state(&rec).unwrap();
        for name in ["implement", "onboarding"] {
            let path = crate::workflow::reuse::execution_skill_path_for_epoch(
                &rec,
                baseline.run_epoch,
                name,
            );
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, "x").unwrap();
            let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.set_len(crate::workflow::trusted_evidence::SKILL_MAX_BYTES + 1)
                .unwrap();
        }
        crate::state::with_run_state_lock(&rec, || {
            crate::workflow::trusted_evidence::write_for_epoch(
                &rec,
                &baseline,
                crate::workflow::trusted_evidence::ProducerHook::RunBeginFreshRun,
            );
            Ok(())
        })
        .unwrap();
        let prepared = crate::workflow::execution_repo::PreparedInjection {
            project_id: rec.id.clone(),
            run_epoch: baseline.run_epoch,
            phase: baseline.phase.clone(),
            track_id: baseline.track_id.clone(),
            target: None,
        };
        let before = std::fs::read(crate::progress_log::path(&rec)).unwrap_or_default();
        let view = no_change_turn(&rec, Some(&prepared), false).await;
        let details = evidence_details(&leg_segment(&before, &rec));
        assert_eq!(details.len(), 1);
        assert!(details[0].contains("facts=2"));
        assert_eq!(view.failure_class, Some(FailureClass::HarnessCrash));
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn trusted_evidence_reader_stays_off_other_arms_and_panic_is_silent() {
        let _guard = test_env_lock();
        let (rec, _home, proj) = implement_leg_fixture(crate::workflow::graph::PHASE_IMPLEMENT);
        crate::progress_log::set_test_rotate_bytes(Some(u64::MAX));
        struct ClearRotate;
        impl Drop for ClearRotate {
            fn drop(&mut self) {
                crate::progress_log::set_test_rotate_bytes(None);
            }
        }
        let _pin = ClearRotate;
        let baseline = crate::state::load_run_state(&rec).unwrap();
        let prepared = crate::workflow::execution_repo::PreparedInjection::for_target(target_for(
            &rec,
            &baseline,
            crate::workflow::graph::PHASE_IMPLEMENT,
            proj.path(),
        ));
        let counter = crate::workflow::trusted_evidence::ReadCounterGuard::arm();
        let restore = || {
            crate::state::save_run_state(&rec, &baseline).unwrap();
            if let Ok(path) = crate::notify::artifact::path(&rec) {
                let _ = std::fs::remove_file(path);
            }
        };

        let before = counter.count();
        let _ = no_change_turn(&rec, None, false).await;
        assert_eq!(counter.count(), before);
        restore();

        let before = counter.count();
        apply_turn_checked(
            &rec,
            Ok(PromptResult {
                text: "done".into(),
                stop_reason: Some("end_turn".into()),
            }),
            harness_stub(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
            None,
            Some(&prepared),
            None,
            false,
        )
        .await
        .unwrap();
        assert_eq!(counter.count(), before);
        restore();

        let before = counter.count();
        let _ = no_change_turn(&rec, Some(&prepared), true).await;
        assert_eq!(counter.count(), before);

        let before = counter.count();
        apply_turn_checked(
            &rec,
            Ok(PromptResult {
                text: "Blocked: a real reason".into(),
                stop_reason: Some("end_turn".into()),
            }),
            harness_stub(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
            None,
            Some(&prepared),
            None,
            false,
        )
        .await
        .unwrap();
        assert_eq!(counter.count(), before);
        restore();

        let before = counter.count();
        apply_turn_checked(
            &rec,
            Ok(PromptResult {
                text: "Error: boom".into(),
                stop_reason: Some("end_turn".into()),
            }),
            harness_stub(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
            None,
            Some(&prepared),
            None,
            false,
        )
        .await
        .unwrap();
        assert_eq!(counter.count(), before);
        restore();

        let mut stub = baseline.clone();
        stub.driver = crate::workflow::WorkflowDriver::Stub;
        crate::state::save_run_state(&rec, &stub).unwrap();
        let before = counter.count();
        let _ = no_change_turn(&rec, Some(&prepared), false).await;
        assert_eq!(counter.count(), before);
        restore();

        let mut waiting = baseline.clone();
        waiting.driver = crate::workflow::WorkflowDriver::FileWait;
        crate::state::save_run_state(&rec, &waiting).unwrap();
        let before = counter.count();
        let _ = no_change_turn(&rec, Some(&prepared), false).await;
        assert_eq!(counter.count(), before);
        restore();

        let mut checking = rec.clone();
        checking.self_continuation = true;
        let before = counter.count();
        apply_turn_checked(
            &checking,
            Ok(PromptResult {
                text: "self-check: continue finding=none action=progress".into(),
                stop_reason: Some("end_turn".into()),
            }),
            harness_stub(),
            crate::workflow::graph::PHASE_IMPLEMENT,
            SpawnTally::default(),
            Some("tree unchanged".into()),
            Some(&prepared),
            None,
            false,
        )
        .await
        .unwrap();
        assert_eq!(counter.count(), before);
        restore();

        let before = counter.count();
        let _panic = crate::workflow::trusted_evidence::PanicOnceGuard::arm();
        let before_journal = std::fs::read(crate::progress_log::path(&rec)).unwrap_or_default();
        let panicked = no_change_turn(&rec, Some(&prepared), false).await;
        assert_eq!(counter.count(), before + 1);
        assert!(evidence_details(&leg_segment(&before_journal, &rec)).is_empty());
        assert_eq!(panicked.failure_class, Some(FailureClass::HarnessCrash));
        drop(_panic);
        restore();

        crate::state::with_run_state_lock(&rec, || {
            crate::workflow::trusted_evidence::write_for_epoch(
                &rec,
                &baseline,
                crate::workflow::trusted_evidence::ProducerHook::RunBeginFreshRun,
            );
            Ok(())
        })
        .unwrap();
        let rec_hook = rec.clone();
        let _stale = crate::workflow::trusted_evidence::AfterReadGuard::arm(move || {
            crate::state::with_run_state_lock(&rec_hook, || {
                let mut state = crate::state::load_run_state(&rec_hook)?;
                state.run_epoch = state.run_epoch.saturating_add(1);
                crate::state::save_run_state(&rec_hook, &state)
            })
            .unwrap();
        });
        let before_journal = std::fs::read(crate::progress_log::path(&rec)).unwrap_or_default();
        let stale = no_change_turn(&rec, Some(&prepared), false).await;
        assert!(evidence_details(&leg_segment(&before_journal, &rec)).is_empty());
        assert!(!stale.applied);
        assert_eq!(stale.skipped, Some(true));
        drop(_stale);

        let active = crate::progress_log::path(&rec);
        let prefix = std::fs::read(&active).unwrap_or_default();
        let recorded = prefix.len();
        restore();
        crate::state::with_run_state_lock(&rec, || {
            crate::workflow::trusted_evidence::write_for_epoch(
                &rec,
                &baseline,
                crate::workflow::trusted_evidence::ProducerHook::RunBeginFreshRun,
            );
            Ok(())
        })
        .unwrap();
        let _ = no_change_turn(&rec, Some(&prepared), false).await;
        let after = std::fs::read(&active).unwrap_or_default();
        assert_eq!(crate::progress_log::path(&rec), active);
        assert!(after.len() >= recorded);
        assert_eq!(&after[..recorded], &prefix[..]);
        assert!(String::from_utf8_lossy(&after).contains("  evidence  "));
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }
}

#[cfg(test)]
mod holder_command_tests {
    use super::*;
    use std::time::SystemTime;

    fn record(path: &std::path::Path) -> crate::registry::ProjectRecord {
        crate::registry::ProjectRecord {
            id: "holder-cwd".into(),
            path: path.to_path_buf(),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: std::collections::BTreeMap::new(),
            state_dir: Some(path.join("state")),
            auto_merge: true,
            phase_timeouts_secs: std::collections::BTreeMap::new(),
            notify_progress: false,
            worktree_isolation: true,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            ci_fix_routing: false,
            review_comment_routing: false,
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn two_holders_keep_their_own_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let rec = record(dir.path());
        let stamp = PersistRef::new(attempt_identity(&rec, "grok"), SystemTime::UNIX_EPOCH);
        let first = build_holder_command(&rec, None, None, "grok", &stamp, Some(3)).unwrap();
        let second = build_holder_command(&rec, None, None, "grok", &stamp, Some(4)).unwrap();
        let args = |cmd: &std::process::Command| -> Vec<String> {
            cmd.get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect()
        };
        let first_args = args(&first);
        let second_args = args(&second);
        assert!(first_args.windows(2).any(|pair| pair == ["--epoch", "3"]));
        assert!(second_args.windows(2).any(|pair| pair == ["--epoch", "4"]));
        assert!(
            first
                .get_current_dir()
                .unwrap()
                .ends_with(std::path::Path::new("worktrees").join("3"))
        );
        assert!(
            second
                .get_current_dir()
                .unwrap()
                .ends_with(std::path::Path::new("worktrees").join("4"))
        );
        let singular = build_holder_command(&rec, None, None, "grok", &stamp, None).unwrap();
        assert!(!args(&singular).iter().any(|arg| arg == "--epoch"));
        assert!(singular.get_current_dir().is_none());
    }

    #[test]
    fn failed_sidecar_move_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let rec = record(dir.path());
        let state = rec.state_dir.clone().unwrap();
        std::fs::create_dir_all(state.join("trusted-evidence")).unwrap();
        std::fs::write(state.join("harness-progress.json"), b"{}").unwrap();
        std::fs::write(state.join("trusted-evidence").join("3.json"), b"{}").unwrap();
        std::fs::write(state.join("trusted-evidence").join("current.json"), b"{}").unwrap();
        let err = move_singular_sidecars(&rec, 3).unwrap_err();
        assert!(err.to_string().contains("refusing to replace"), "{err}");
        assert!(state.join("harness-progress.json").is_file());
        assert!(!state.join("harness-progress").join("3.json").exists());
    }

    #[test]
    fn stop_refusal_leaves_the_book() {
        let dir = tempfile::tempdir().unwrap();
        let rec = record(dir.path());
        let state_dir = rec.state_dir.clone().unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for _ in 0..4 {
                let Ok((stream, _)) = listener.accept() else {
                    break;
                };
                let mut writer = stream.try_clone().unwrap();
                let mut reader = std::io::BufReader::new(stream);
                let mut line = String::new();
                let _ = std::io::BufRead::read_line(&mut reader, &mut line);
                let _ = std::io::Write::write_all(&mut writer, b"{\"ok\":true}\n");
            }
        });
        let mut first = crate::state::RunState::idle(&rec.id);
        first.run_epoch = 3;
        first.status = crate::state::RunStatus::Running;
        first.phase = "plan".into();
        first.track_id = Some("A".into());
        let mut second = crate::state::RunState::idle(&rec.id);
        second.run_epoch = 4;
        second.status = crate::state::RunStatus::Running;
        second.phase = "implement".into();
        second.track_id = Some("B".into());
        crate::state::write_run_book(
            &rec,
            &crate::state::RunEnvelope::new(0, vec![first, second]),
        )
        .unwrap();
        let session = state_dir.join("sessions").join("3");
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(
            session.join("harness-grok.json"),
            format!(
                r#"{{"version":1,"project_id":"{}","adapter":"grok","supports_compact":true,"alive":true,"control_addr":"{addr}"}}"#,
                rec.id
            ),
        )
        .unwrap();
        let view = crate::run::stop(&rec).unwrap();
        assert!(
            matches!(
                crate::state::load_run_book(&rec).unwrap(),
                crate::state::RunBook::Many(_)
            ),
            "book collapsed: {view:?}"
        );
    }

    fn write_session(rec: &ProjectRecord, epoch: u64, addr: &str, cwd: Option<&std::path::Path>) {
        let state_dir = rec.state_dir.clone().unwrap();
        let session = state_dir.join("sessions").join(epoch.to_string());
        std::fs::create_dir_all(&session).unwrap();
        let body = serde_json::json!({
            "version": 1,
            "project_id": rec.id,
            "adapter": "grok",
            "supports_compact": true,
            "alive": true,
            "control_addr": addr,
            "cwd": cwd,
        });
        std::fs::write(
            session.join("harness-grok.json"),
            serde_json::to_string(&body).unwrap(),
        )
        .unwrap();
    }

    fn spawn_real_holder(
        rec: ProjectRecord,
        epoch: u64,
        session_id: &str,
        use_mutex: bool,
    ) -> (
        std::thread::JoinHandle<()>,
        std::sync::mpsc::Receiver<String>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let session_id = session_id.to_string();
        let handle = std::thread::spawn(move || {
            let _scope = if use_mutex {
                crate::state::set_holder_epoch(epoch).expect("holder epoch");
                None
            } else {
                Some(crate::state::RunScope::enter(epoch).expect("scope"))
            };
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async move {
                let cwd = crate::worktree::epoch_dir(&rec, epoch).expect("epoch dir");
                std::fs::create_dir_all(&cwd).expect("epoch dir");
                let session = crate::harness::grok::GrokSession::start_mock(
                    cwd,
                    crate::harness::grok::mock_handshake_ok(&session_id),
                    std::time::Duration::from_secs(2),
                )
                .await
                .expect("mock session");
                let rec_hold = rec.clone();
                let hold =
                    tokio::spawn(
                        async move { super::hold_loop_with_session(rec_hold, session).await },
                    );
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                let addr = loop {
                    if let Ok(Some(handle)) = super::load_persist_at(&rec, Some(epoch))
                        && let Some(addr) = handle.control_addr.clone()
                    {
                        break addr;
                    }
                    if std::time::Instant::now() >= deadline {
                        panic!("epoch {epoch} holder never published a control address");
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                };
                tx.send(addr).expect("addr");
                hold.await.expect("holder join").expect("holder loop");
            });
            let _ = _scope;
        });
        (handle, rx)
    }

    fn holder_answers_ping(addr: &str) -> bool {
        matches!(
            super::holder_rpc_bounded(addr, &super::HoldRequest::Ping),
            Ok(resp) if resp.ok
        )
    }

    #[tokio::test]
    async fn scoped_status_reads_the_epoch_persist_not_the_parent_pool() {
        let dir = tempfile::tempdir().unwrap();
        let rec = record(dir.path());
        std::fs::create_dir_all(rec.state_dir.as_ref().unwrap()).unwrap();
        let mut first = crate::state::RunState::idle(&rec.id);
        first.run_epoch = 2;
        first.status = crate::state::RunStatus::Running;
        let mut second = crate::state::RunState::idle(&rec.id);
        second.run_epoch = 5;
        second.status = crate::state::RunStatus::Running;
        crate::state::write_run_book(
            &rec,
            &crate::state::RunEnvelope::new(0, vec![first, second]),
        )
        .unwrap();
        let epoch_cwd = dir.path().join("epoch-5");
        std::fs::create_dir_all(&epoch_cwd).unwrap();
        let session = rec.state_dir.as_ref().unwrap().join("sessions").join("5");
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(
            session.join("harness-grok.json"),
            serde_json::json!({
                "version": 1,
                "project_id": rec.id,
                "adapter": "grok",
                "alive": true,
                "session_id": "epoch-5",
                "cwd": epoch_cwd,
            })
            .to_string(),
        )
        .unwrap();
        let pool_cwd = dir.path().join("pool-cwd");
        std::fs::create_dir_all(&pool_cwd).unwrap();
        let pooled = crate::harness::grok::GrokSession::start_mock(
            pool_cwd,
            crate::harness::grok::mock_handshake_ok("pool-sess"),
            std::time::Duration::from_secs(2),
        )
        .await
        .unwrap();
        insert_test_session(rec.id.clone(), pooled).await;
        assert!(
            status_bundle_sync(&rec).is_none(),
            "an unscoped book answered from the parent pool"
        );
        let scoped = {
            let _scope = crate::state::RunScope::enter(5).unwrap();
            status_bundle_sync(&rec)
        };
        global_pool().lock().await.remove(&rec.id);
        let grok = scoped
            .expect("scoped status missing")
            .grok
            .expect("no harness");
        assert_eq!(grok.session_id.as_deref(), Some("epoch-5"));
    }

    #[test]
    fn stop_all_collapses_to_singular_project_key() {
        struct ReleaseHolder;
        impl Drop for ReleaseHolder {
            fn drop(&mut self) {
                crate::state::release_holder_epoch();
            }
        }
        let _release = ReleaseHolder;
        let dir = tempfile::tempdir().unwrap();
        let rec = record(dir.path());
        std::fs::create_dir_all(rec.state_dir.as_ref().unwrap()).unwrap();
        let mut first = crate::state::RunState::idle(&rec.id);
        first.run_epoch = 3;
        first.status = crate::state::RunStatus::Running;
        let mut second = crate::state::RunState::idle(&rec.id);
        second.run_epoch = 7;
        second.status = crate::state::RunStatus::Paused;
        crate::state::write_run_book(
            &rec,
            &crate::state::RunEnvelope::new(0, vec![first, second]),
        )
        .unwrap();
        let (thread_a, addr_a) = spawn_real_holder(rec.clone(), 3, "sess-3", true);
        let (thread_b, addr_b) = spawn_real_holder(rec.clone(), 7, "sess-7", false);
        let addr_a = addr_a
            .recv_timeout(std::time::Duration::from_secs(4))
            .expect("epoch 3 address");
        let addr_b = addr_b
            .recv_timeout(std::time::Duration::from_secs(4))
            .expect("epoch 7 address");
        assert!(holder_answers_ping(&addr_a), "epoch 3 did not answer Ping");
        assert!(holder_answers_ping(&addr_b), "epoch 7 did not answer Ping");
        crate::run::stop(&rec).unwrap();
        let text = std::fs::read_to_string(rec.state_dir.as_ref().unwrap().join("run-state.json"))
            .unwrap();
        assert!(text.contains("project_id"), "{text}");
        assert!(!text.contains("run-book-v1"), "{text}");
        assert!(matches!(
            crate::state::load_run_book(&rec).unwrap(),
            crate::state::RunBook::One(_)
        ));
        let sessions = rec.state_dir.as_ref().unwrap().join("sessions");
        assert!(!sessions.join("3").join("harness-grok.json").exists());
        assert!(!sessions.join("7").join("harness-grok.json").exists());
        assert!(!holder_answers_ping(&addr_a), "epoch 3 still answers Ping");
        assert!(!holder_answers_ping(&addr_b), "epoch 7 still answers Ping");
        thread_a.join().expect("epoch 3 thread");
        thread_b.join().expect("epoch 7 thread");
    }

    #[test]
    fn old_holder_refuses_second_run() {
        let dir = tempfile::tempdir().unwrap();
        let rec = record(dir.path());
        let epoch_dir = crate::worktree::epoch_dir(&rec, 3).unwrap();
        std::fs::create_dir_all(&epoch_dir).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let seen_t = seen.clone();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut writer = stream.try_clone().unwrap();
                let mut reader = std::io::BufReader::new(stream);
                let mut line = String::new();
                let _ = std::io::BufRead::read_line(&mut reader, &mut line);
                *seen_t.lock().unwrap() = line;
                let _ = std::io::Write::write_all(&mut writer, b"{\"ok\":true}\n");
            }
        });
        let state_dir = rec.state_dir.clone().unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();
        let body = serde_json::json!({
            "version": 1,
            "project_id": rec.id,
            "adapter": "grok",
            "supports_compact": true,
            "alive": true,
            "control_addr": addr,
            "cwd": epoch_dir,
        });
        std::fs::write(
            state_dir.join("harness-grok.json"),
            serde_json::to_string(&body).unwrap(),
        )
        .unwrap();
        let err = second_run_adopt_addr(&rec, 3).unwrap_err();
        assert!(
            err.to_string().contains("holder has no epoch protocol"),
            "{err}"
        );
        let line = seen.lock().unwrap().clone();
        assert!(line.contains("\"ping\""), "{line}");
        assert!(!line.contains("adopt_epoch"), "{line}");
    }

    #[test]
    fn release_epoch_failure_leaves_the_book() {
        let dir = tempfile::tempdir().unwrap();
        let rec = record(dir.path());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut writer = stream.try_clone().unwrap();
                let mut reader = std::io::BufReader::new(stream);
                let mut line = String::new();
                let _ = std::io::BufRead::read_line(&mut reader, &mut line);
                let _ = std::io::Write::write_all(&mut writer, b"{\"ok\":false}\n");
            }
        });
        let mut finished = crate::state::RunState::idle(&rec.id);
        finished.run_epoch = 3;
        finished.status = crate::state::RunStatus::Stopped;
        let mut kept = crate::state::RunState::idle(&rec.id);
        kept.run_epoch = 4;
        kept.status = crate::state::RunStatus::Running;
        crate::state::write_run_book(
            &rec,
            &crate::state::RunEnvelope::new(0, vec![finished, kept]),
        )
        .unwrap();
        write_session(&rec, 4, &addr, None);
        assert!(!crate::state::release_remaining_holder(&rec, 3));
        match crate::state::load_run_book(&rec).unwrap() {
            crate::state::RunBook::Many(envelope) => {
                let epochs: Vec<u64> = envelope.runs.iter().map(|e| e.run_epoch).collect();
                assert_eq!(epochs, vec![3, 4]);
            }
            crate::state::RunBook::One(_) => panic!("failed ReleaseEpoch collapsed the book"),
        }
    }

    #[test]
    fn stop_idle_holder_leaves_the_book() {
        let dir = tempfile::tempdir().unwrap();
        let rec = record(dir.path());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            for _ in 0..4 {
                let Ok((stream, _)) = listener.accept() else {
                    break;
                };
                let mut writer = stream.try_clone().unwrap();
                let mut reader = std::io::BufReader::new(stream);
                let mut line = String::new();
                let _ = std::io::BufRead::read_line(&mut reader, &mut line);
                let _ = std::io::Write::write_all(&mut writer, b"{\"ok\":true}\n");
            }
        });
        let mut idle = crate::state::RunState::idle(&rec.id);
        idle.run_epoch = 3;
        idle.status = crate::state::RunStatus::Idle;
        let mut stopped = crate::state::RunState::idle(&rec.id);
        stopped.run_epoch = 4;
        stopped.status = crate::state::RunStatus::Stopped;
        crate::state::write_run_book(
            &rec,
            &crate::state::RunEnvelope::new(0, vec![idle, stopped]),
        )
        .unwrap();
        write_session(&rec, 3, &addr, None);
        crate::run::stop(&rec).unwrap();
        assert!(
            matches!(
                crate::state::load_run_book(&rec).unwrap(),
                crate::state::RunBook::Many(_)
            ),
            "a live idle holder was collapsed"
        );
    }
}
