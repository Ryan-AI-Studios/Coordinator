//! Validated, reversible Role Binding swap (track **0110**).
//!
//! One CLI verb changes a role's harness / command / model on a running machine. The
//! engine has four jobs:
//!
//! 1. **Capability matrix** — decide which harnesses a role class may drive, and which
//!    resolved command basename belongs to that pair. A plan-review slot is a SLOT name:
//!    it may keep its native driver or adopt `codex`, never an unrelated driver.
//! 2. **Truthful probe** — [`crate::harness::preflight::probe_target`] resolves the binary
//!    the *runtime* would spawn for that role class, runs its `--version`, runs the
//!    (separate) companion auth CLI where one exists, and checks the harness model list
//!    where the harness exposes one.
//! 3. **Atomic, field-exact write** — unique snapshot of the on-disk bytes, re-load, write
//!    only the target role, save, reload, compare field-by-field. A mismatch restores the
//!    snapshot.
//! 4. **Durable deferral** — a swap requested while the role is in use is recorded in
//!    `{COORDINATOR_HOME}/role-swap.json` (schema 1) and applied in-process at the next
//!    phase boundary that resolves that role. No restart, no mid-leg write, no second
//!    termination path.
//!
//! Nothing here spawns, kills or recycles a harness. The existing
//! `recycle_mismatch` path applies the widened holder identity at the next inject.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{self, MachineConfig, RoleBinding};
use crate::error::{CoordinatorError, Result};
use crate::harness::roles::{ROLE_FOLD, ROLE_NEXT, load_role_bindings, resolve_phase_role_key};
use crate::workflow::graph::{
    PHASE_CROSS_MODEL, PHASE_PLAN_REVIEW, ROLE_CROSS_MODEL_PRIMARY, ROLE_CROSS_MODEL_SECONDARY,
    ROLE_CROSS_MODEL_TERTIARY, ROLE_IMPLEMENTOR, ROLE_PLANNER, ROLE_REVIEWER_AGY,
    ROLE_REVIEWER_OPENCODE, all_phase_ids, cross_model_roles,
};

/// Ledger schema (OD-110-1). Adding a field is a schema decision; this track is v1.
pub const ROLE_SWAP_LEDGER_VERSION: u32 = 1;
/// A lock older than this is broken **once** (self-update precedent, 0108).
pub const LOCK_STALE: std::time::Duration = std::time::Duration::from_secs(30 * 60);
/// Snapshot collisions are retried with a fresh nonce this many times.
const SNAPSHOT_ATTEMPTS: usize = 5;

// ---------------------------------------------------------------------------
// Capability matrix
// ---------------------------------------------------------------------------

/// Role class decides the runtime resolver and the allowed harness set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleClass {
    Adapter,
    PlanReviewAgy,
    PlanReviewOpenCode,
    CrossModel,
    Other,
}

impl RoleClass {
    pub fn as_str(self) -> &'static str {
        match self {
            RoleClass::Adapter => "adapter",
            RoleClass::PlanReviewAgy => "plan_review_agy",
            RoleClass::PlanReviewOpenCode => "plan_review_opencode",
            RoleClass::CrossModel => "cross_model",
            RoleClass::Other => "other",
        }
    }
}

pub fn class_for_role(role: &str) -> RoleClass {
    match role {
        ROLE_PLANNER | ROLE_IMPLEMENTOR | ROLE_FOLD | ROLE_NEXT => RoleClass::Adapter,
        ROLE_REVIEWER_AGY => RoleClass::PlanReviewAgy,
        ROLE_REVIEWER_OPENCODE => RoleClass::PlanReviewOpenCode,
        ROLE_CROSS_MODEL_PRIMARY | ROLE_CROSS_MODEL_SECONDARY | ROLE_CROSS_MODEL_TERTIARY => {
            RoleClass::CrossModel
        }
        _ => RoleClass::Other,
    }
}

/// The default command a harness needs for a role class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetProfile {
    pub harness: String,
    pub command: String,
}

impl TargetProfile {
    fn new(harness: &str, command: &str) -> Self {
        Self {
            harness: harness.to_string(),
            command: command.to_string(),
        }
    }
}

fn adapter_command(harness: &str) -> Option<&'static str> {
    match harness.to_ascii_lowercase().as_str() {
        "grok" => Some("grok"),
        "cursor" => Some("cursor-agent"),
        "claude" => Some("claude-agent-acp"),
        "codex" => Some("codex-acp"),
        _ => None,
    }
}

fn plan_review_agy_command(harness: &str) -> Option<(&'static str, &'static str)> {
    match harness.to_ascii_lowercase().as_str() {
        "antigravity" | "agy" => Some(("antigravity", "agy")),
        "codex" => Some(("codex", "codex")),
        _ => None,
    }
}

fn plan_review_opencode_command(harness: &str) -> Option<(&'static str, &'static str)> {
    match harness.to_ascii_lowercase().as_str() {
        "opencode" => Some(("opencode", "opencode")),
        "codex" => Some(("codex", "codex")),
        _ => None,
    }
}

fn cross_model_command(harness: &str) -> Option<(&'static str, &'static str)> {
    match harness.to_ascii_lowercase().as_str() {
        "codex" => Some(("codex", "codex")),
        "claude" => Some(("claude", "claude")),
        "opencode" => Some(("opencode", "opencode")),
        _ => None,
    }
}

fn allowed_harnesses(class: RoleClass) -> &'static str {
    match class {
        RoleClass::Adapter => "grok, cursor, claude, codex",
        RoleClass::PlanReviewAgy => "antigravity, codex",
        RoleClass::PlanReviewOpenCode => "opencode, codex",
        RoleClass::CrossModel => "codex, claude, opencode",
        RoleClass::Other => "(none)",
    }
}

/// Validate a `(class, harness)` pair and return its canonical command.
pub fn target_profile(class: RoleClass, harness: &str) -> Result<TargetProfile> {
    let h = harness.trim();
    if h.is_empty() {
        return Err(CoordinatorError::Message(format!(
            "harness must not be empty (allowed: {})",
            allowed_harnesses(class)
        )));
    }
    let profile = match class {
        RoleClass::Adapter => adapter_command(h).map(|c| TargetProfile::new(h, c)),
        RoleClass::PlanReviewAgy => {
            plan_review_agy_command(h).map(|(n, c)| TargetProfile::new(n, c))
        }
        RoleClass::PlanReviewOpenCode => {
            plan_review_opencode_command(h).map(|(n, c)| TargetProfile::new(n, c))
        }
        RoleClass::CrossModel => cross_model_command(h).map(|(n, c)| TargetProfile::new(n, c)),
        RoleClass::Other => None,
    };
    profile.ok_or_else(|| {
        CoordinatorError::Message(format!(
            "harness {h} cannot drive role class {} (allowed: {})",
            class.as_str(),
            allowed_harnesses(class)
        ))
    })
}

/// The command basename the runtime must resolve for `(class, harness)`.
fn expected_command_basename(class: RoleClass, harness: &str) -> Option<&'static str> {
    match class {
        RoleClass::Adapter => adapter_command(harness),
        RoleClass::PlanReviewAgy => plan_review_agy_command(harness).map(|(_, c)| c),
        RoleClass::PlanReviewOpenCode => plan_review_opencode_command(harness).map(|(_, c)| c),
        RoleClass::CrossModel => cross_model_command(harness).map(|(_, c)| c),
        RoleClass::Other => None,
    }
}

/// Strip `.cmd`/`.exe` and lowercase for a command-class comparison.
pub fn command_basename(path: &Path) -> String {
    let mut name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if name.is_empty() {
        name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
    }
    name
}

fn command_matches(path: &Path, expected: &str) -> bool {
    command_basename(path) == expected.to_ascii_lowercase()
}

/// Refuse a command that belongs to another driver for this role class.
pub fn check_command_class(class: RoleClass, harness: &str, resolved: &Path) -> Result<()> {
    let expected = expected_command_basename(class, harness).ok_or_else(|| {
        CoordinatorError::Message(format!(
            "harness {harness} cannot drive role class {}",
            class.as_str()
        ))
    })?;
    if command_matches(resolved, expected) {
        return Ok(());
    }
    Err(CoordinatorError::Message(format!(
        "command {} does not belong to harness {harness} for role class {} \
         (expected basename {expected}; a custom wrapper must be renamed or used through raw \
         `roles set`, which is unsafe)",
        resolved.display(),
        class.as_str()
    )))
}

/// Duplicate cross-model harness: the tier loop is keyed by harness slug, so a second
/// tier bound to the same harness would never be exercised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateHarness {
    pub colliding_role: String,
    pub preexisting: bool,
}

pub fn check_duplicate_cross_model(
    role: &str,
    proposed_harness: &str,
    bindings: &BTreeMap<String, RoleBinding>,
) -> Result<Option<DuplicateHarness>> {
    if class_for_role(role) != RoleClass::CrossModel {
        return Ok(None);
    }
    let target = proposed_harness.trim().to_ascii_lowercase();
    let current = bindings
        .get(role)
        .map(|b| b.harness.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let changed = target != current;
    for other in cross_model_roles() {
        if *other == role {
            continue;
        }
        let Some(b) = bindings.get(*other) else {
            continue;
        };
        if !b.harness.trim().eq_ignore_ascii_case(&target) {
            continue;
        }
        // Already-duplicated config: a model/command-only change is allowed and marked.
        if changed {
            return Err(CoordinatorError::Message(format!(
                "cross-model harness {target} is already bound to {other}; the tier loop is \
                 keyed by harness, so this tier would never be exercised (choose a different \
                 harness or change {other} first)"
            )));
        }
        return Ok(Some(DuplicateHarness {
            colliding_role: (*other).to_string(),
            preexisting: true,
        }));
    }
    Ok(None)
}

/// Role keys a plan-review slot may own.
pub fn plan_review_slot_roles() -> [&'static str; 2] {
    [ROLE_REVIEWER_AGY, ROLE_REVIEWER_OPENCODE]
}

// ---------------------------------------------------------------------------
// Probe receipt
// ---------------------------------------------------------------------------

/// Model-list outcome. `Unverified` is explicit (OD-110-3): harnesses with no list are
/// not proof of availability. `Fail` means the list was readable and did not contain the
/// requested base selector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ModelCheck {
    NotRequested,
    Pass { detail: String },
    Unverified { detail: String },
    Fail { detail: String },
}

impl ModelCheck {
    pub fn is_fail(&self) -> bool {
        matches!(self, ModelCheck::Fail { .. })
    }

    pub fn detail(&self) -> Option<&str> {
        match self {
            ModelCheck::NotRequested => None,
            ModelCheck::Pass { detail }
            | ModelCheck::Unverified { detail }
            | ModelCheck::Fail { detail } => Some(detail),
        }
    }
}

/// A model probe that could not reach a verdict always refuses (timeout / unparseable /
/// ambiguous / list unavailable).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelProbeRefusal {
    Timeout { detail: String },
    Unparseable { detail: String },
    Ambiguous { detail: String },
    Unavailable { detail: String },
    Auth { detail: String },
}

impl ModelProbeRefusal {
    pub fn detail(&self) -> &str {
        match self {
            ModelProbeRefusal::Timeout { detail }
            | ModelProbeRefusal::Unparseable { detail }
            | ModelProbeRefusal::Ambiguous { detail }
            | ModelProbeRefusal::Unavailable { detail }
            | ModelProbeRefusal::Auth { detail } => detail,
        }
    }
}

impl std::fmt::Display for ModelProbeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "model probe: {}", self.detail())
    }
}

/// Named evidence for one proposed binding. No secret is stored (`redact_secrets`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeReceipt {
    pub role: String,
    pub class: RoleClass,
    pub harness: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auth_argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_detail: Option<String>,
    pub model_check: ModelCheck,
    pub probed_at: DateTime<Utc>,
    #[serde(default)]
    pub duplicate_harness_preexisting: bool,
}

impl ProbeReceipt {
    pub(crate) fn empty(role: &str, class: RoleClass, binding: &RoleBinding) -> Self {
        Self {
            role: role.to_string(),
            class,
            harness: binding.harness.clone(),
            command: binding.command.clone(),
            model: binding.model.clone(),
            runtime_path: None,
            runtime_version: None,
            auth_path: None,
            auth_argv: Vec::new(),
            auth_detail: None,
            model_check: ModelCheck::NotRequested,
            probed_at: Utc::now(),
            duplicate_harness_preexisting: false,
        }
    }
}

/// A refusal carries the partial receipt plus every failed check so the CLI can name all
/// of them (DoD-2).
#[derive(Debug, Clone)]
pub struct ProbeRefusal {
    pub receipt: Box<ProbeReceipt>,
    pub failures: Vec<String>,
}

impl ProbeRefusal {
    pub fn message(&self) -> String {
        format!(
            "probe refused for role {} ({}/{}): {}",
            self.receipt.role,
            self.receipt.harness,
            self.receipt.command,
            self.failures.join("; ")
        )
    }
}

impl std::fmt::Display for ProbeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

impl From<ProbeRefusal> for CoordinatorError {
    fn from(value: ProbeRefusal) -> Self {
        CoordinatorError::Message(value.message())
    }
}

// ---------------------------------------------------------------------------
// Ledger (OD-110-1)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingSwap {
    pub role: String,
    pub previous: RoleBinding,
    pub proposed: RoleBinding,
    pub receipt: ProbeReceipt,
    pub requested_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwapAction {
    Swap,
    Rollback,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapRecord {
    pub role: String,
    pub action: SwapAction,
    pub previous: RoleBinding,
    pub applied: RoleBinding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<ProbeReceipt>,
    pub applied_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwapErrorKind {
    ProbeFailed,
    PreviousChanged,
    WriteMismatch,
    RestoreFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapError {
    pub role: String,
    pub kind: SwapErrorKind,
    pub message: String,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleSwapLedger {
    pub version: u32,
    #[serde(default)]
    pub pending: BTreeMap<String, PendingSwap>,
    #[serde(default)]
    pub last: BTreeMap<String, SwapRecord>,
    #[serde(default)]
    pub errors: BTreeMap<String, SwapError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapOutcome {
    pub role: String,
    pub applied: bool,
    pub deferred: bool,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<ProbeReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockInfo {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusView {
    pub ledger: RoleSwapLedger,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock: Option<LockInfo>,
}

/// `roles prove` output (DoD-1). `proven` requires the requested role to be the current
/// phase's role and adapter+model+bin to equal that role's current binding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProveView {
    pub role: String,
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<RoleBinding>,
    pub proven: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Partial update requested by the CLI. Unsupplied fields are preserved.
#[derive(Debug, Clone, Default)]
pub struct PartialBinding {
    pub harness: Option<String>,
    pub command: Option<String>,
    pub model: Option<String>,
    pub clear_model: bool,
}

impl PartialBinding {
    fn is_empty(&self) -> bool {
        self.harness.is_none()
            && self.command.is_none()
            && self.model.is_none()
            && !self.clear_model
    }
}

fn home() -> Result<PathBuf> {
    config::ensure_machine_home()
}

pub fn ledger_path() -> Result<PathBuf> {
    Ok(home()?.join("role-swap.json"))
}

pub fn lock_path() -> Result<PathBuf> {
    Ok(home()?.join("role-swap.lock"))
}

pub fn load_ledger() -> Result<RoleSwapLedger> {
    let path = ledger_path()?;
    if !path.exists() {
        return Ok(new_ledger());
    }
    let text = std::fs::read_to_string(&path)?;
    let ledger: RoleSwapLedger = serde_json::from_str(&text)?;
    if ledger.version != ROLE_SWAP_LEDGER_VERSION {
        return Err(CoordinatorError::Message(format!(
            "unsupported role-swap ledger version {}; expected {ROLE_SWAP_LEDGER_VERSION}",
            ledger.version
        )));
    }
    Ok(ledger)
}

fn save_ledger(ledger: &RoleSwapLedger) -> Result<()> {
    crate::persist::atomic_write_json(&ledger_path()?, ledger)
}

fn new_ledger() -> RoleSwapLedger {
    RoleSwapLedger {
        version: ROLE_SWAP_LEDGER_VERSION,
        ..RoleSwapLedger::default()
    }
}

// ---------------------------------------------------------------------------
// Lock (OD-110-5 arm A / self-update precedent)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LockBody {
    pid: u32,
    started_at: DateTime<Utc>,
}

pub struct RoleSwapLock {
    path: PathBuf,
    pid: u32,
}

impl RoleSwapLock {
    /// Blocking refusal on contention: name the holder and the recovery verb.
    pub fn acquire() -> Result<Self> {
        match Self::try_acquire()? {
            Some(lock) => Ok(lock),
            None => Err(CoordinatorError::Message(format!(
                "another roles swap/rollback is in progress ({})",
                describe_lock_holder(&lock_path()?)?
            ))),
        }
    }

    /// Non-blocking: the boundary apply keeps its pending entry when this returns `None`.
    pub fn try_acquire() -> Result<Option<Self>> {
        let path = lock_path()?;
        let pid = std::process::id();
        let body = LockBody {
            pid,
            started_at: Utc::now(),
        };
        let bytes = serde_json::to_vec(&body)?;
        for attempt in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    file.write_all(&bytes)?;
                    file.sync_all()?;
                    return Ok(Some(Self { path, pid }));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if attempt == 0 && lock_is_stale(&path)? {
                        // One stale break, then retry exactly once.
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(None)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for RoleSwapLock {
    fn drop(&mut self) {
        // Only remove a lock this process wrote: a stale break race must not delete a peer.
        if let Ok(Some(body)) = read_lock_body(&self.path)
            && body.pid == self.pid
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn read_lock_body(path: &Path) -> Result<Option<LockBody>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)?;
    let body: LockBody = serde_json::from_str(text.trim())?;
    Ok(Some(body))
}

fn lock_is_stale(path: &Path) -> Result<bool> {
    let meta = std::fs::metadata(path)?;
    let modified = meta.modified()?;
    let age = std::time::SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    Ok(age > LOCK_STALE)
}

fn describe_lock_holder(path: &Path) -> Result<String> {
    match read_lock_body(path)? {
        Some(body) => Ok(format!(
            "lock {} pid={} started_at={}; if that process is gone run `roles swap --unlock`",
            path.display(),
            body.pid,
            body.started_at.to_rfc3339()
        )),
        None => Ok(format!(
            "lock {} (unreadable holder; run `roles swap --unlock`)",
            path.display()
        )),
    }
}

/// Explicit recovery for a crash-left lock. Prints holder metadata and removes the file.
pub fn unlock() -> Result<LockInfo> {
    let path = lock_path()?;
    let info = lock_info(&path)?;
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(info)
}

fn lock_info(path: &Path) -> Result<LockInfo> {
    let body = read_lock_body(path).unwrap_or(None);
    Ok(LockInfo {
        path: path.to_path_buf(),
        pid: body.as_ref().map(|b| b.pid),
        started_at: body.map(|b| b.started_at),
    })
}

// ---------------------------------------------------------------------------
// Feature gate
// ---------------------------------------------------------------------------

fn model_swap_gate() -> Result<()> {
    let snapshot = crate::features::resolve_all();
    match snapshot.duties.get("model_swap") {
        Some(view) if view.enabled => Ok(()),
        Some(view) => Err(CoordinatorError::Message(format!(
            "model_swap is disabled (source {:?}{}); no write",
            view.source,
            view.error
                .as_ref()
                .map(|e| format!(": {e}"))
                .unwrap_or_default()
        ))),
        None => Err(CoordinatorError::Message(
            "model_swap duty is missing from the feature snapshot; no write".into(),
        )),
    }
}

fn boundary_apply_gate_open() -> bool {
    model_swap_gate().is_ok()
}

// ---------------------------------------------------------------------------
// Snapshot + config write
// ---------------------------------------------------------------------------

fn snapshot_stamp(now: DateTime<Utc>) -> String {
    now.format("%Y%m%dT%H%M%S%3fZ").to_string()
}

fn snapshot_nonce() -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    id.chars().take(4).collect()
}

fn sanitize_role(role: &str) -> String {
    role.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Snapshot the exact on-disk `config.json` bytes under a unique name.
pub fn snapshot_config(role: &str) -> Result<PathBuf> {
    let cfg_path = config::machine_config_path()?;
    let bytes = std::fs::read(&cfg_path).unwrap_or_default();
    write_unique_snapshot(role, &bytes, &cfg_path)
}

fn write_unique_snapshot(role: &str, bytes: &[u8], cfg_path: &Path) -> Result<PathBuf> {
    let home = cfg_path
        .parent()
        .ok_or_else(|| CoordinatorError::Message("config.json has no parent directory".into()))?;
    let now = Utc::now();
    for _ in 0..SNAPSHOT_ATTEMPTS {
        let name = format!(
            "config.json.bak-roles-{}-{}-{}",
            sanitize_role(role),
            snapshot_stamp(now),
            snapshot_nonce()
        );
        let target = home.join(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
        {
            Ok(file) => {
                // Close the reservation handle before the atomic replace: Windows refuses
                // MoveFileExW onto a path that still has an open handle.
                drop(file);
                crate::persist::atomic_write(&target, bytes)?;
                return Ok(target);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(CoordinatorError::Message(format!(
        "could not create a unique snapshot for role {role} after {SNAPSHOT_ATTEMPTS} attempts"
    )))
}

/// Re-load → set only `role` → save → reload → compare field-by-field.
///
/// `expected_others` is the object just re-loaded before the write; every non-target role
/// must still match it afterwards. On any mismatch the snapshot bytes are restored.
fn write_binding_verified(
    role: &str,
    proposed: &RoleBinding,
    expected_others: &MachineConfig,
) -> Result<PathBuf> {
    let cfg_path = config::machine_config_path()?;
    let before = std::fs::read(&cfg_path).unwrap_or_default();
    let snapshot = write_unique_snapshot(role, &before, &cfg_path)?;

    let mut cfg = config::load_machine_config()?;
    cfg.role_bindings.insert(role.to_string(), proposed.clone());
    config::save_machine_config(&cfg)?;

    let reloaded = config::load_machine_config()?;
    let target_ok = reloaded.role_bindings.get(role) == Some(proposed);
    let others_ok = expected_others
        .role_bindings
        .iter()
        .all(|(key, value)| key == role || reloaded.role_bindings.get(key) == Some(value));
    if target_ok && others_ok {
        return Ok(snapshot);
    }

    let restore = crate::persist::atomic_write(&cfg_path, &before);
    let got = reloaded.role_bindings.get(role).cloned();
    Err(CoordinatorError::Message(match restore {
        Ok(()) => format!(
            "post-write verification failed for role {role}: wrote {proposed:?}, read {got:?}; \
             restored snapshot {}",
            snapshot.display()
        ),
        Err(e) => format!(
            "post-write verification failed for role {role}: wrote {proposed:?}, read {got:?}; \
             restoring snapshot {} failed: {e}",
            snapshot.display()
        ),
    }))
}

// ---------------------------------------------------------------------------
// Live-run scan
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InUse {
    pub project_id: String,
    pub track: Option<String>,
    pub phase: String,
    pub run_epoch: u64,
}

fn role_in_use(role: &str, bindings: &BTreeMap<String, RoleBinding>) -> Result<Option<InUse>> {
    let registry = crate::api::load_registry()?;
    for record in registry.list() {
        let state = match crate::state::load_run_state(record) {
            Ok(state) => state,
            // Unreadable state counts as in use: never write underneath a live run.
            Err(_) => {
                return Ok(Some(InUse {
                    project_id: record.id.clone(),
                    track: None,
                    phase: String::new(),
                    run_epoch: 0,
                }));
            }
        };
        if !matches!(
            state.status,
            crate::state::RunStatus::Running | crate::state::RunStatus::Paused
        ) {
            continue;
        }
        let matches = if state.phase == PHASE_PLAN_REVIEW {
            plan_review_slot_roles().contains(&role)
        } else if state.phase == PHASE_CROSS_MODEL {
            cross_model_roles().contains(&role)
        } else {
            resolve_phase_role_key(&state.phase, bindings).as_deref() == Some(role)
        };
        if matches {
            return Ok(Some(InUse {
                project_id: record.id.clone(),
                track: state.track_id.clone(),
                phase: state.phase.clone(),
                run_epoch: state.run_epoch,
            }));
        }
    }
    Ok(None)
}

/// The next phase that resolves `role`, for the deferred message.
fn next_role_phase(role: &str, bindings: &BTreeMap<String, RoleBinding>, current: &str) -> String {
    if class_for_role(role) == RoleClass::PlanReviewOpenCode
        || class_for_role(role) == RoleClass::PlanReviewAgy
    {
        return "this phase's pending slot spawn; if the slot already spawned, on the next \
                plan-review phase"
            .to_string();
    }
    if class_for_role(role) == RoleClass::CrossModel {
        return "the next cross-model-review gate start".to_string();
    }
    let phases = all_phase_ids();
    let start = phases
        .iter()
        .position(|p| *p == current)
        .map(|i| i + 1)
        .unwrap_or(0);
    for offset in 0..phases.len() {
        let phase = phases[(start + offset) % phases.len()];
        if phase == current {
            continue;
        }
        if resolve_phase_role_key(phase, bindings).as_deref() == Some(role) {
            return phase.to_string();
        }
    }
    "the next phase that resolves the role".to_string()
}

fn deferred_message(
    role: &str,
    in_use: &InUse,
    previous: &RoleBinding,
    bindings: &BTreeMap<String, RoleBinding>,
) -> String {
    let next = next_role_phase(role, bindings, &in_use.phase);
    format!(
        "deferred: role={role} project={} track={} phase={} run_epoch={}\n\
         config unchanged; in-flight leg keeps the previous binding ({}/{}/{})\n\
         pending={role}; applies in-process at {next}; no serve restart\n\
         if the run ends first: pending remains; applies on the next run at the next {role} phase",
        in_use.project_id,
        in_use.track.as_deref().unwrap_or("<none>"),
        if in_use.phase.is_empty() {
            "<unreadable>"
        } else {
            &in_use.phase
        },
        in_use.run_epoch,
        previous.harness,
        previous.command,
        previous.model.as_deref().unwrap_or("-")
    )
}

// ---------------------------------------------------------------------------
// swap / rollback / cancel / status
// ---------------------------------------------------------------------------

/// Build the proposed binding from the current object + only supplied fields.
pub fn propose(
    role: &str,
    current: &RoleBinding,
    class: RoleClass,
    partial: &PartialBinding,
) -> Result<RoleBinding> {
    if partial.is_empty() {
        return Err(CoordinatorError::Message(
            "no change requested: supply --harness/--command/--model/--clear-model".into(),
        ));
    }
    let mut proposed = current.clone();

    let harness_supplied = partial
        .harness
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty());

    if let Some(harness) = harness_supplied {
        let profile = target_profile(class, harness)?;
        let harness_changed = !current
            .harness
            .trim()
            .eq_ignore_ascii_case(&profile.harness);
        if harness_changed
            && current.model.is_some()
            && partial.model.is_none()
            && !partial.clear_model
        {
            return Err(CoordinatorError::Message(format!(
                "harness change for {role} would leave model pin {} without --model or \
                 --clear-model",
                current.model.as_deref().unwrap_or("-")
            )));
        }
        proposed.harness = profile.harness;
        // An explicit --command below wins; otherwise the profile's default command.
        proposed.command = profile.command;
    }

    if let Some(command) = partial
        .command
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        proposed.command = command.to_string();
    }

    if partial.clear_model {
        proposed.model = None;
    } else if let Some(model) = partial
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        proposed.model = Some(model.to_string());
    }

    if proposed == *current {
        return Err(CoordinatorError::Message(format!(
            "binding unchanged for role {role}; no write"
        )));
    }
    Ok(proposed)
}

/// Validated, reversible swap. See the module docs for the ordering contract.
pub fn swap(role: &str, partial: PartialBinding) -> Result<SwapOutcome> {
    model_swap_gate()?;
    let role = role.trim();
    if role.is_empty() {
        return Err(CoordinatorError::Message("role must not be empty".into()));
    }
    let _lock = RoleSwapLock::acquire()?;

    let mut ledger = load_ledger()?;
    if let Some(pending) = ledger.pending.get(role) {
        return Err(CoordinatorError::Message(format!(
            "role {role} already has a deferred swap requested at {}; run `roles swap --role \
             {role} --cancel` first",
            pending.requested_at.to_rfc3339()
        )));
    }

    let cfg = config::load_machine_config()?;
    let current = cfg.role_bindings.get(role).cloned().ok_or_else(|| {
        CoordinatorError::Message(format!(
            "unknown role {role} (known: {})",
            cfg.role_bindings
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })?;
    let class = class_for_role(role);
    if class == RoleClass::Other {
        return Err(CoordinatorError::Message(format!(
            "role {role} is not swappable (adapter, plan-review or cross-model role required)"
        )));
    }

    let proposed = propose(role, &current, class, &partial)?;
    // Validate the harness pair even when --harness was not supplied (bad config).
    target_profile(class, &proposed.harness)?;
    let resolved_command = crate::harness::grok::resolve_command(&proposed.command)
        .map_err(|e| CoordinatorError::Message(format!("command {}: {e}", proposed.command)))?;
    check_command_class(class, &proposed.harness, &resolved_command)?;

    let duplicate = check_duplicate_cross_model(role, &proposed.harness, &cfg.role_bindings)?;

    let mut receipt: ProbeReceipt = crate::harness::preflight::probe_target(role, &proposed)?;
    if let Some(dup) = duplicate {
        receipt.duplicate_harness_preexisting = dup.preexisting;
        receipt.auth_detail = Some(match receipt.auth_detail {
            Some(existing) => format!(
                "{existing}; duplicate harness preexisting with {}",
                dup.colliding_role
            ),
            None => format!("duplicate harness preexisting with {}", dup.colliding_role),
        });
    }

    if let Some(in_use) = role_in_use(role, &cfg.role_bindings)? {
        let message = deferred_message(role, &in_use, &current, &cfg.role_bindings);
        ledger.pending.insert(
            role.to_string(),
            PendingSwap {
                role: role.to_string(),
                previous: current,
                proposed,
                receipt: receipt.clone(),
                requested_at: Utc::now(),
            },
        );
        save_ledger(&ledger)?;
        return Ok(SwapOutcome {
            role: role.to_string(),
            applied: false,
            deferred: true,
            message,
            receipt: Some(receipt),
            snapshot: None,
        });
    }

    // Re-load immediately before the write: a raw writer between request and save must
    // not be clobbered.
    let reloaded = config::load_machine_config()?;
    let now_current = reloaded.role_bindings.get(role).cloned();
    if now_current.as_ref() != Some(&current) {
        return Err(CoordinatorError::Message(format!(
            "binding for role {role} changed since the request ({} -> {}); no write",
            describe_binding(Some(&current)),
            describe_binding(now_current.as_ref())
        )));
    }

    let snapshot = write_binding_verified(role, &proposed, &reloaded)?;
    ledger.last.insert(
        role.to_string(),
        SwapRecord {
            role: role.to_string(),
            action: SwapAction::Swap,
            previous: current.clone(),
            applied: proposed.clone(),
            snapshot: Some(snapshot.clone()),
            receipt: Some(receipt.clone()),
            applied_at: Utc::now(),
        },
    );
    ledger.errors.remove(role);
    save_ledger(&ledger)?;

    Ok(SwapOutcome {
        role: role.to_string(),
        applied: true,
        deferred: false,
        message: format!(
            "role-swap: {role} {} -> {} (snapshot {})",
            describe_binding(Some(&current)),
            describe_binding(Some(&proposed)),
            snapshot
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("<snapshot>")
        ),
        receipt: Some(receipt),
        snapshot: Some(snapshot),
    })
}

fn describe_binding(binding: Option<&RoleBinding>) -> String {
    match binding {
        Some(b) => format!(
            "{}/{}/{}",
            b.harness,
            b.command,
            b.model.as_deref().unwrap_or("-")
        ),
        None => "<absent>".to_string(),
    }
}

/// Restore the exact previous `RoleBinding` from a snapshot (default: `last[role].snapshot`).
pub fn rollback(role: &str, from: Option<&Path>) -> Result<SwapOutcome> {
    let role = role.trim();
    if role.is_empty() {
        return Err(CoordinatorError::Message("role must not be empty".into()));
    }
    let _lock = RoleSwapLock::acquire()?;
    let mut ledger = load_ledger()?;
    if let Some(pending) = ledger.pending.get(role) {
        return Err(CoordinatorError::Message(format!(
            "role {role} has a deferred swap from {} that was not applied; use `roles swap \
             --role {role} --cancel`",
            pending.requested_at.to_rfc3339()
        )));
    }

    let source = match from {
        Some(path) => path.to_path_buf(),
        None => ledger
            .last
            .get(role)
            .and_then(|rec| rec.snapshot.clone())
            .ok_or_else(|| {
                CoordinatorError::Message(format!(
                    "no snapshot recorded for role {role}; expected {}last[{role}].snapshot or \
                     an explicit --from <path>",
                    ledger_path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| "role-swap.json ".into())
                ))
            })?,
    };
    if !source.is_file() {
        return Err(CoordinatorError::Message(format!(
            "snapshot {} does not exist",
            source.display()
        )));
    }
    let text = std::fs::read_to_string(&source)?;
    let snapshot_cfg: MachineConfig = serde_json::from_str(&text).map_err(|e| {
        CoordinatorError::Message(format!(
            "snapshot {} is not config JSON: {e}",
            source.display()
        ))
    })?;
    let restored = snapshot_cfg
        .role_bindings
        .get(role)
        .cloned()
        .ok_or_else(|| {
            CoordinatorError::Message(format!("snapshot {} has no role {role}", source.display()))
        })?;

    let current_cfg = config::load_machine_config()?;
    let current = current_cfg
        .role_bindings
        .get(role)
        .cloned()
        .ok_or_else(|| {
            CoordinatorError::Message(format!("unknown role {role}; snapshot not applied"))
        })?;

    let snapshot = write_binding_verified(role, &restored, &current_cfg)?;
    let after = config::load_machine_config()?;
    if after.role_bindings.get(role) != Some(&restored) {
        return Err(CoordinatorError::Message(format!(
            "rollback verification failed for role {role}; expected {}, read {}",
            describe_binding(Some(&restored)),
            describe_binding(after.role_bindings.get(role))
        )));
    }

    ledger.last.insert(
        role.to_string(),
        SwapRecord {
            role: role.to_string(),
            action: SwapAction::Rollback,
            previous: current,
            applied: restored.clone(),
            snapshot: Some(snapshot.clone()),
            receipt: None,
            applied_at: Utc::now(),
        },
    );
    ledger.errors.remove(role);
    save_ledger(&ledger)?;

    Ok(SwapOutcome {
        role: role.to_string(),
        applied: true,
        deferred: false,
        message: format!(
            "role-swap: rollback {role} -> {} (from {}; new snapshot {})",
            describe_binding(Some(&restored)),
            source.display(),
            snapshot.display()
        ),
        receipt: None,
        snapshot: Some(snapshot),
    })
}

/// Remove only the target role's pending entry. No config write.
pub fn cancel(role: &str) -> Result<RoleSwapLedger> {
    let role = role.trim();
    let _lock = RoleSwapLock::acquire()?;
    let mut ledger = load_ledger()?;
    if ledger.pending.remove(role).is_none() {
        return Err(CoordinatorError::Message(format!(
            "no pending swap for role {role}"
        )));
    }
    save_ledger(&ledger)?;
    Ok(ledger)
}

pub fn status() -> Result<StatusView> {
    let ledger = load_ledger()?;
    let path = lock_path()?;
    let lock = if path.exists() {
        Some(lock_info(&path)?)
    } else {
        None
    };
    Ok(StatusView { ledger, lock })
}

/// Resolve the expected binding binary for `roles prove`; an error must make the proof fail.
pub(crate) fn prove_expected_bin(binding: &RoleBinding) -> std::result::Result<PathBuf, String> {
    crate::harness::preflight::resolve_runtime_bin(RoleClass::Adapter, "prove", binding)
        .map_err(|e| e.to_string())
}

/// Read-only live identity proof for an adapter role (DoD-1).
///
/// Never mutates: no `last-used.json` write (read-only project resolve), no snapshot, no
/// ledger entry, no config write. A different role's holder is never accepted as proof.
pub fn prove(role: &str, project: Option<&str>) -> Result<ProveView> {
    let role = role.trim();
    if class_for_role(role) != RoleClass::Adapter {
        return Err(CoordinatorError::Message(format!(
            "roles prove supports adapter roles only (planner, implementor, fold, next); \
             {role} is {}",
            class_for_role(role).as_str()
        )));
    }
    let record = crate::api::resolve_selected_readonly(project, true)?;
    let bindings = load_role_bindings()?;
    let state = match crate::state::load_run_state(&record) {
        Ok(state) => state,
        Err(e) => {
            return Ok(ProveView {
                role: role.to_string(),
                phase: String::new(),
                active_role: None,
                expected: bindings.get(role).cloned(),
                proven: false,
                adapter: None,
                model: None,
                bin: None,
                session_id: None,
                pid: None,
                reason: Some(format!("run state unreadable: {e}")),
            });
        }
    };
    let active_role = resolve_phase_role_key(&state.phase, &bindings);
    let expected = bindings.get(role).cloned();
    let live = crate::harness::pool::probe_holder(&record, role)?;
    let Some(live) = live else {
        return Ok(ProveView {
            role: role.to_string(),
            phase: state.phase,
            active_role,
            expected,
            proven: false,
            adapter: None,
            model: None,
            bin: None,
            session_id: None,
            pid: None,
            reason: Some(
                "no live holder control socket (use a serve-owned run, not an in-process start)"
                    .into(),
            ),
        });
    };

    let mut reason: Option<String> = None;
    if active_role.as_deref() != Some(role) {
        reason = Some(format!(
            "requested role {role} is not the current phase's role ({})",
            active_role.as_deref().unwrap_or("<none>")
        ));
    }
    let expected = match expected {
        Some(expected) => expected,
        None => {
            return Ok(ProveView {
                role: role.to_string(),
                phase: state.phase,
                active_role,
                expected: None,
                proven: false,
                adapter: Some(live.adapter),
                model: live.model,
                bin: live.bin,
                session_id: live.session_id,
                pid: live.pid,
                reason: Some(format!("no binding for role {role}")),
            });
        }
    };
    let resolved_bin = match prove_expected_bin(&expected) {
        Ok(bin) => Some(bin),
        Err(e) => {
            // Fail closed: never claim `proven` when the binding's runtime binary cannot be
            // resolved. Skipping the bin comparison here would accept any live holder.
            reason = Some(format!("expected binding binary not resolvable: {e}"));
            None
        }
    };
    if reason.is_none()
        && !crate::harness::pool::identity_matches(&live, &expected, resolved_bin.as_deref())
    {
        reason = Some(format!(
            "live identity does not match the binding (live {}/{}/{}, expected {}/{}/{})",
            live.adapter,
            live.model.as_deref().unwrap_or("-"),
            live.bin
                .as_ref()
                .map(|b| b.display().to_string())
                .unwrap_or_else(|| "-".into()),
            expected.harness,
            expected.model.as_deref().unwrap_or("-"),
            resolved_bin
                .as_ref()
                .map(|b| b.display().to_string())
                .unwrap_or_else(|| "-".into())
        ));
    }

    Ok(ProveView {
        role: role.to_string(),
        phase: state.phase,
        active_role,
        expected: Some(expected),
        proven: reason.is_none(),
        adapter: Some(live.adapter),
        model: live.model,
        bin: live.bin,
        session_id: live.session_id,
        pid: live.pid,
        reason,
    })
}

// ---------------------------------------------------------------------------
// Deferred boundary apply
// ---------------------------------------------------------------------------

/// Apply a pending swap only for the phase's single role key.
pub fn apply_pending_for_phase(phase: &str) -> Vec<SwapOutcome> {
    let bindings = load_role_bindings().unwrap_or_else(|_| config::default_role_bindings());
    match resolve_phase_role_key(phase, &bindings) {
        Some(key) => apply_pending_roles(&[key.as_str()], phase, None),
        None => Vec::new(),
    }
}

/// Apply a pending swap for exactly these role keys.
pub fn apply_pending_for_roles(roles: &[&str]) -> Vec<SwapOutcome> {
    apply_pending_roles(roles, "", None)
}

/// Variant that also writes the progress line (a live record is available at the call site).
pub fn apply_pending_for_phase_for(
    phase: &str,
    record: &crate::registry::ProjectRecord,
) -> Vec<SwapOutcome> {
    let bindings = load_role_bindings().unwrap_or_else(|_| config::default_role_bindings());
    match resolve_phase_role_key(phase, &bindings) {
        Some(key) => apply_pending_roles(&[key.as_str()], phase, Some(record)),
        None => Vec::new(),
    }
}

pub fn apply_pending_for_roles_for(
    roles: &[&str],
    record: &crate::registry::ProjectRecord,
) -> Vec<SwapOutcome> {
    apply_pending_roles(roles, "", Some(record))
}

fn apply_pending_roles(
    roles: &[&str],
    boundary_phase: &str,
    record: Option<&crate::registry::ProjectRecord>,
) -> Vec<SwapOutcome> {
    let mut outcomes = Vec::new();
    if roles.is_empty() {
        return outcomes;
    }
    let ledger = match load_ledger() {
        Ok(ledger) => ledger,
        Err(e) => {
            // Fail open to the previous binding. Never fail the run.
            eprintln!("coordinator: role-swap: ledger unreadable ({e}); pending swaps not applied");
            return outcomes;
        }
    };
    if ledger.pending.is_empty() {
        return outcomes;
    }

    for role in roles {
        if !ledger.pending.contains_key(*role) {
            continue;
        }
        if !boundary_apply_gate_open() {
            eprintln!(
                "coordinator: role-swap: model_swap disabled; pending {role} kept for a later tick"
            );
            continue;
        }

        let _lock = match RoleSwapLock::try_acquire() {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                // Keep the pending; retry on the next tick. Never a run failure.
                eprintln!(
                    "coordinator: role-swap: lock held; pending {role} kept for a later tick"
                );
                continue;
            }
            Err(e) => {
                eprintln!("coordinator: role-swap: lock error ({e}); pending {role} kept");
                continue;
            }
        };

        // Re-read under the lock: a concurrent `--cancel` (or another swap) must win.
        let mut ledger = match load_ledger() {
            Ok(ledger) => ledger,
            Err(e) => {
                eprintln!("coordinator: role-swap: ledger unreadable ({e}); pending kept");
                continue;
            }
        };
        let Some(pending) = ledger.pending.get(*role).cloned() else {
            continue;
        };

        let current_cfg = match config::load_machine_config() {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("coordinator: role-swap: config unreadable ({e}); pending {role} kept");
                continue;
            }
        };
        let current = current_cfg.role_bindings.get(*role).cloned();
        if current.as_ref() != Some(&pending.previous) {
            ledger.pending.remove(*role);
            ledger.errors.insert(
                (*role).to_string(),
                SwapError {
                    role: (*role).to_string(),
                    kind: SwapErrorKind::PreviousChanged,
                    message: format!(
                        "binding changed since the deferred request ({} -> {}); previous binding \
                         kept",
                        describe_binding(Some(&pending.previous)),
                        describe_binding(current.as_ref())
                    ),
                    at: Utc::now(),
                },
            );
            let _ = save_ledger(&ledger);
            progress_line(
                record,
                boundary_phase,
                &format!("role-swap: deferred {role} refused (binding changed since request)"),
            );
            continue;
        }

        let receipt = match crate::harness::preflight::probe_target(role, &pending.proposed) {
            Ok(receipt) => receipt,
            Err(refusal) => {
                ledger.pending.remove(*role);
                ledger.errors.insert(
                    (*role).to_string(),
                    SwapError {
                        role: (*role).to_string(),
                        kind: SwapErrorKind::ProbeFailed,
                        message: refusal.message(),
                        at: Utc::now(),
                    },
                );
                let _ = save_ledger(&ledger);
                progress_line(
                    record,
                    boundary_phase,
                    &format!("role-swap: deferred {role} refused (probe failed); previous kept"),
                );
                continue;
            }
        };
        let _ = receipt;

        match write_binding_verified(role, &pending.proposed, &current_cfg) {
            Ok(snapshot) => {
                ledger.pending.remove(*role);
                ledger.errors.remove(*role);
                ledger.last.insert(
                    (*role).to_string(),
                    SwapRecord {
                        role: (*role).to_string(),
                        action: SwapAction::Swap,
                        previous: pending.previous.clone(),
                        applied: pending.proposed.clone(),
                        snapshot: Some(snapshot.clone()),
                        receipt: Some(pending.receipt.clone()),
                        applied_at: Utc::now(),
                    },
                );
                let _ = save_ledger(&ledger);
                progress_line(
                    record,
                    boundary_phase,
                    &format!(
                        "role-swap: applied {role} (deferred at {}) -> {}",
                        if boundary_phase.is_empty() {
                            "phase boundary"
                        } else {
                            boundary_phase
                        },
                        describe_binding(Some(&pending.proposed))
                    ),
                );
                outcomes.push(SwapOutcome {
                    role: (*role).to_string(),
                    applied: true,
                    deferred: false,
                    message: format!(
                        "role-swap: applied {role} ({}) at the boundary",
                        describe_binding(Some(&pending.proposed))
                    ),
                    receipt: Some(pending.receipt.clone()),
                    snapshot: Some(snapshot),
                });
            }
            Err(e) => {
                ledger.pending.remove(*role);
                ledger.errors.insert(
                    (*role).to_string(),
                    SwapError {
                        role: (*role).to_string(),
                        kind: SwapErrorKind::WriteMismatch,
                        message: e.to_string(),
                        at: Utc::now(),
                    },
                );
                let _ = save_ledger(&ledger);
                progress_line(
                    record,
                    boundary_phase,
                    &format!("role-swap: deferred {role} failed to apply; previous kept"),
                );
            }
        }
    }
    outcomes
}

fn progress_line(record: Option<&crate::registry::ProjectRecord>, boundary: &str, detail: &str) {
    match record {
        Some(record) => {
            let detail = if boundary.is_empty() {
                detail.to_string()
            } else {
                format!("{detail} (phase {boundary})")
            };
            crate::progress_log::append(record, "role-swap", &detail);
        }
        None => eprintln!("coordinator: {detail}"),
    }
}

// Re-exported so callers do not re-import graph phase names for the adapter call site.
pub use crate::workflow::graph::{
    PHASE_ADDRESS_CI as ADAPTER_ADDRESS_CI, PHASE_ADDRESS_FINDINGS as ADAPTER_ADDRESS_FINDINGS,
    PHASE_FOLD as ADAPTER_FOLD, PHASE_IMPLEMENT as ADAPTER_IMPLEMENT, PHASE_PLAN as ADAPTER_PLAN,
};

#[cfg(test)]
mod tests;
