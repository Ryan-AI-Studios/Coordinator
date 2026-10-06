//! Contextual state policies (track 0069).
//!
//! Four builtin rules. Adapter `ci-wait` calls [`decide`] before `try_auto_publish`
//! and before `squash_merge`. An unreadable read is a block. `COORDINATOR_STATE_POLICIES=off`
//! skips the read.

use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(test)]
use std::sync::Arc;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{CoordinatorError, Result};
use crate::registry::ProjectRecord;
use crate::state::RunState;

pub const ENV_STATE_POLICIES: &str = "COORDINATOR_STATE_POLICIES";

pub const NAME_DEPENDENCY: &str = "dependency-manifest";
pub const NAME_CI_WORKFLOW: &str = "ci-workflow-path";
pub const NAME_FAILURES: &str = "consecutive-failures";
pub const NAME_RESTORE: &str = "restore-before-publish";

pub const BUILTIN_NAMES: [&str; 4] = [
    NAME_DEPENDENCY,
    NAME_CI_WORKFLOW,
    NAME_FAILURES,
    NAME_RESTORE,
];

pub const DEFAULT_FAILURE_THRESHOLD: u32 = 3;

const MANIFEST_NAMES: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "package.json",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
];

const WORKFLOW_PREFIX: &str = ".github/workflows/";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyAction {
    Allow,
    Report,
    RequireApproval,
    Block,
}

impl PolicyAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Report => "report",
            Self::RequireApproval => "require-approval",
            Self::Block => "block",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s.trim() {
            "allow" => Ok(Self::Allow),
            "report" => Ok(Self::Report),
            "require-approval" => Ok(Self::RequireApproval),
            "block" => Ok(Self::Block),
            other => Err(CoordinatorError::Message(format!(
                "unknown policy action '{other}'; expected allow | report | require-approval | block"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRule {
    pub name: String,
    pub action: PolicyAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicySource {
    Off,
    Project,
    Machine,
    Default,
}

impl PolicySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Project => "project",
            Self::Machine => "machine",
            Self::Default => "default",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPolicy {
    pub name: &'static str,
    pub action: PolicyAction,
    pub threshold: u32,
    pub source: PolicySource,
}

/// Status JSON / run-state alert. Omitted when none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateGate {
    pub name: String,
    pub action: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyApproval {
    pub name: String,
    pub fingerprint: String,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    Allow,
    Report(StateGate),
    Hold(StateGate),
    Block(StateGate),
}

impl GateDecision {
    pub fn holds(&self) -> bool {
        matches!(self, Self::Hold(_))
    }

    pub fn permits_publish(&self) -> bool {
        matches!(self, Self::Allow | Self::Report(_))
    }

    pub fn alert(&self) -> Option<&StateGate> {
        match self {
            Self::Allow => None,
            Self::Report(g) | Self::Hold(g) | Self::Block(g) => Some(g),
        }
    }

    pub fn detail(&self) -> &str {
        self.alert().map(|g| g.detail.as_str()).unwrap_or("")
    }
}

pub fn policies_disabled() -> bool {
    matches!(
        std::env::var(ENV_STATE_POLICIES),
        Ok(s) if s.eq_ignore_ascii_case("off")
    )
}

pub fn require_name(name: &str) -> Result<&'static str> {
    BUILTIN_NAMES
        .into_iter()
        .find(|n| *n == name.trim())
        .ok_or_else(|| {
            CoordinatorError::Message(format!(
                "unknown policy name '{}'; expected {}",
                name.trim(),
                BUILTIN_NAMES.join(" | ")
            ))
        })
}

pub fn resolved(record: &ProjectRecord) -> Vec<ResolvedPolicy> {
    if policies_disabled() {
        return BUILTIN_NAMES
            .into_iter()
            .map(|name| ResolvedPolicy {
                name,
                action: PolicyAction::Allow,
                threshold: default_threshold(name),
                source: PolicySource::Off,
            })
            .collect();
    }
    let machine = crate::config::load_machine_config().ok();
    BUILTIN_NAMES
        .into_iter()
        .map(|name| resolve_one(record, machine.as_ref(), name))
        .collect()
}

fn resolve_one(
    record: &ProjectRecord,
    machine: Option<&crate::config::MachineConfig>,
    name: &'static str,
) -> ResolvedPolicy {
    if let Some(rule) = record.state_policies.iter().find(|r| r.name == name) {
        return from_rule(name, rule, PolicySource::Project);
    }
    if let Some(cfg) = machine
        && let Some(rule) = cfg.state_policies.iter().find(|r| r.name == name)
    {
        return from_rule(name, rule, PolicySource::Machine);
    }
    ResolvedPolicy {
        name,
        action: PolicyAction::Report,
        threshold: default_threshold(name),
        source: PolicySource::Default,
    }
}

fn from_rule(name: &'static str, rule: &PolicyRule, source: PolicySource) -> ResolvedPolicy {
    ResolvedPolicy {
        name,
        action: rule.action,
        threshold: rule.threshold.unwrap_or_else(|| default_threshold(name)),
        source,
    }
}

fn default_threshold(name: &str) -> u32 {
    if name == NAME_FAILURES {
        DEFAULT_FAILURE_THRESHOLD
    } else {
        0
    }
}

pub fn format_show(rules: &[ResolvedPolicy]) -> String {
    let mut out = String::new();
    for rule in rules {
        out.push_str(&format!(
            "{} action={} threshold={} source={}\n",
            rule.name,
            rule.action.as_str(),
            rule.threshold,
            rule.source.as_str()
        ));
    }
    out
}

/// One tick's view of the rules against `tip` (`HEAD` or a commit sha).
pub fn decide(record: &ProjectRecord, state: &RunState, cwd: &Path, tip: &str) -> GateDecision {
    if policies_disabled() {
        return GateDecision::Allow;
    }
    let rules = resolved(record);
    let base = crate::checkpoint::ref_name(&record.id, state.run_epoch);
    with_reader(state, |reader| {
        evaluate(record, state, reader, cwd, &base, tip, &rules)
    })
}

fn evaluate(
    record: &ProjectRecord,
    state: &RunState,
    reader: &dyn PolicyRead,
    cwd: &Path,
    base: &str,
    tip: &str,
    rules: &[ResolvedPolicy],
) -> GateDecision {
    let paths = reader.changed_paths(cwd, base, tip);
    let track = state.track_id.as_deref().unwrap_or("");
    let failures = reader.failure_count(track);
    let restored = reader.restored_epoch();

    let mut report: Option<StateGate> = None;
    for rule in rules {
        let read = match rule.name {
            NAME_DEPENDENCY | NAME_CI_WORKFLOW => match &paths {
                Ok(list) => Ok(matching_paths(rule.name, list)),
                Err(_) => Err(()),
            },
            NAME_FAILURES => match &failures {
                Ok(n) => Ok(if *n >= rule.threshold && !track.is_empty() {
                    vec![format!("failures:{track}:{n}")]
                } else {
                    Vec::new()
                }),
                Err(_) => Err(()),
            },
            NAME_RESTORE => match &restored {
                Ok(Some(epoch)) => Ok(vec![format!("restore:{epoch}")]),
                Ok(None) => Ok(Vec::new()),
                Err(_) => Err(()),
            },
            _ => Ok(Vec::new()),
        };
        let matched = match read {
            Err(()) => {
                let gate = block_gate(rule.name, "unreadable");
                crate::progress_log::append(record, "policy", &gate.detail);
                return GateDecision::Block(gate);
            }
            Ok(items) if items.is_empty() => continue,
            Ok(items) => items,
        };
        if rule.action == PolicyAction::Allow {
            continue;
        }
        let fingerprint = fingerprint_for(rule.name, &matched);
        if rule.action == PolicyAction::RequireApproval && approved(state, rule.name, &fingerprint)
        {
            continue;
        }
        let gate = StateGate {
            name: rule.name.to_string(),
            action: rule.action.as_str().to_string(),
            detail: detail_line(rule.action, rule.name, &matched),
        };
        crate::progress_log::append(record, "policy", &gate.detail);
        match rule.action {
            PolicyAction::Block => return GateDecision::Block(gate),
            PolicyAction::RequireApproval => return GateDecision::Hold(gate),
            PolicyAction::Report if report.is_none() => report = Some(gate),
            PolicyAction::Report | PolicyAction::Allow => {}
        }
    }
    match report {
        Some(gate) => GateDecision::Report(gate),
        None => GateDecision::Allow,
    }
}

fn block_gate(name: &str, why: &str) -> StateGate {
    StateGate {
        name: name.to_string(),
        action: PolicyAction::Block.as_str().to_string(),
        detail: format!("policy: block {name}: {why}"),
    }
}

fn detail_line(action: PolicyAction, name: &str, matched: &[String]) -> String {
    let shown = matched
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let extra = if matched.len() > 3 {
        format!(" +{}", matched.len() - 3)
    } else {
        String::new()
    };
    format!("policy: {} {name}: {shown}{extra}", action.as_str())
}

fn matching_paths(name: &str, paths: &[String]) -> Vec<String> {
    let mut out: Vec<String> = paths
        .iter()
        .filter(|p| match name {
            NAME_DEPENDENCY => is_manifest(p),
            NAME_CI_WORKFLOW => is_workflow(p),
            _ => false,
        })
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

fn is_manifest(path: &str) -> bool {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    MANIFEST_NAMES.contains(&name)
}

fn is_workflow(path: &str) -> bool {
    path.replace('\\', "/").starts_with(WORKFLOW_PREFIX)
}

fn fingerprint_for(name: &str, matched: &[String]) -> String {
    match name {
        NAME_FAILURES | NAME_RESTORE => matched.first().cloned().unwrap_or_default(),
        _ => fnv1a64_hex(matched.join("\n").as_bytes()),
    }
}

fn approved(state: &RunState, name: &str, fingerprint: &str) -> bool {
    state
        .policy_approvals
        .iter()
        .any(|a| a.name == name && a.fingerprint == fingerprint)
}

/// Store an approval for the current fingerprint. Refuses an unreadable read.
pub fn approve(
    record: &ProjectRecord,
    state: &mut RunState,
    cwd: &Path,
    name: &str,
) -> Result<String> {
    let name = require_name(name)?;
    if policies_disabled() {
        return Err(CoordinatorError::Message(
            "policy approve refused: COORDINATOR_STATE_POLICIES=off".into(),
        ));
    }
    let base = crate::checkpoint::ref_name(&record.id, state.run_epoch);
    let tip = "HEAD";
    let fingerprint = with_reader(state, |reader| {
        approval_fingerprint(state, reader, cwd, &base, tip, name)
    })?;
    state.policy_approvals.retain(|a| a.name != name);
    state.policy_approvals.push(PolicyApproval {
        name: name.to_string(),
        fingerprint: fingerprint.clone(),
        at: Utc::now(),
    });
    Ok(fingerprint)
}

fn approval_fingerprint(
    state: &RunState,
    reader: &dyn PolicyRead,
    cwd: &Path,
    base: &str,
    tip: &str,
    name: &str,
) -> Result<String> {
    let track = state.track_id.as_deref().unwrap_or("");
    let matched = match name {
        NAME_DEPENDENCY | NAME_CI_WORKFLOW => {
            let paths = reader.changed_paths(cwd, base, tip).map_err(|e| {
                CoordinatorError::Message(format!("policy: block {name}: unreadable ({e})"))
            })?;
            matching_paths(name, &paths)
        }
        NAME_FAILURES => {
            let n = reader.failure_count(track).map_err(|e| {
                CoordinatorError::Message(format!("policy: block {name}: unreadable ({e})"))
            })?;
            vec![format!("failures:{track}:{n}")]
        }
        NAME_RESTORE => {
            let epoch = reader.restored_epoch().map_err(|e| {
                CoordinatorError::Message(format!("policy: block {name}: unreadable ({e})"))
            })?;
            match epoch {
                Some(epoch) => vec![format!("restore:{epoch}")],
                None => Vec::new(),
            }
        }
        _ => Vec::new(),
    };
    Ok(fingerprint_for(name, &matched))
}

/// Merge tip had no commit id. That read is unknown, so the gate blocks.
pub fn unresolved_tip_block(record: &ProjectRecord) -> GateDecision {
    let gate = block_gate(NAME_DEPENDENCY, "unreadable");
    crate::progress_log::append(record, "policy", &gate.detail);
    GateDecision::Block(gate)
}

pub fn consume_restore(record: &ProjectRecord) {
    let Ok(()) = crate::state::with_run_state_lock(record, || {
        let mut state = crate::state::load_run_state(record)?;
        if state.restored_epoch.take().is_some() {
            crate::state::save_run_state(record, &state)?;
        }
        Ok(())
    }) else {
        return;
    };
}

pub fn remember_gate(record: &ProjectRecord, gate: Option<&StateGate>) -> Result<()> {
    crate::state::with_run_state_lock(record, || {
        let mut state = crate::state::load_run_state(record)?;
        state.state_gate = gate.cloned();
        state.updated_at = Utc::now();
        crate::state::save_run_state(record, &state)
    })
}

fn fnv1a64_hex(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

pub trait PolicyRead: Send + Sync {
    fn changed_paths(&self, cwd: &Path, base_ref: &str, tip: &str) -> Result<Vec<String>>;
    fn failure_count(&self, track: &str) -> Result<u32>;
    fn restored_epoch(&self) -> Result<Option<u64>>;
}

struct LiveRead<'a> {
    state: &'a RunState,
}

impl PolicyRead for LiveRead<'_> {
    fn changed_paths(&self, cwd: &Path, base_ref: &str, tip: &str) -> Result<Vec<String>> {
        match git_changed_paths(cwd, base_ref, tip) {
            Ok(paths) => Ok(paths),
            Err(e) => {
                // Test binaries jump `ci-wait` without a checkpoint and use fake PR oids.
                // Release builds do not compile this arm. A test that must observe the
                // failure installs [`install_test_reader`] or calls [`git_changed_paths`].
                #[cfg(test)]
                {
                    let _ = e;
                    Ok(Vec::new())
                }
                #[cfg(not(test))]
                {
                    Err(e)
                }
            }
        }
    }

    fn failure_count(&self, track: &str) -> Result<u32> {
        Ok(self
            .state
            .consecutive_failures
            .get(track)
            .copied()
            .unwrap_or(0))
    }

    fn restored_epoch(&self) -> Result<Option<u64>> {
        Ok(self.state.restored_epoch)
    }
}

fn with_reader<T>(state: &RunState, f: impl FnOnce(&dyn PolicyRead) -> T) -> T {
    #[cfg(test)]
    {
        if let Some(reader) = TEST_READ.with(|slot| slot.borrow().clone()) {
            return f(reader.as_ref());
        }
    }
    let live = LiveRead { state };
    f(&live)
}

/// `git diff --name-only <base> <tip>` (two-commit form). Missing ref is `Err`.
pub fn git_changed_paths(cwd: &Path, base_ref: &str, tip: &str) -> Result<Vec<String>> {
    if !cwd.is_dir() {
        return Err(CoordinatorError::Message(
            "policy: git cwd is not a directory".into(),
        ));
    }
    let verified = git(cwd, &["rev-parse", "--verify", "--quiet", base_ref])?;
    let verified_text = std::str::from_utf8(&verified.stdout).map_err(|_| {
        CoordinatorError::Message("policy: git rev-parse stdout is not utf-8".into())
    })?;
    if !verified.status.success() || verified_text.trim().is_empty() {
        return Err(CoordinatorError::Message(format!(
            "policy: unreadable checkpoint {base_ref}"
        )));
    }
    let diff = git(cwd, &["diff", "--name-only", base_ref, tip])?;
    if !diff.status.success() {
        return Err(CoordinatorError::Message(format!(
            "policy: git diff --name-only {base_ref} {tip}: {}",
            diff.stderr.trim()
        )));
    }
    let text = String::from_utf8(diff.stdout)
        .map_err(|_| CoordinatorError::Message("policy: git diff stdout is not utf-8".into()))?;
    Ok(normalize_paths(&text))
}

fn normalize_paths(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let path = line.trim().replace('\\', "/");
        if !path.is_empty() {
            out.push(path);
        }
    }
    out
}

struct GitCaptured {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: String,
}

fn git(cwd: &Path, args: &[&str]) -> Result<GitCaptured> {
    let shown = args.join(" ");
    let out = Command::new(PathBuf::from("git"))
        .args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .map_err(|e| CoordinatorError::Message(format!("policy: git {shown}: {e}")))?;
    Ok(GitCaptured {
        status: out.status,
        stdout: out.stdout,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

#[cfg(test)]
thread_local! {
    static TEST_READ: std::cell::RefCell<Option<Arc<dyn PolicyRead>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub struct TestReaderGuard;

#[cfg(test)]
impl Drop for TestReaderGuard {
    fn drop(&mut self) {
        TEST_READ.with(|slot| *slot.borrow_mut() = None);
    }
}

#[cfg(test)]
pub fn install_test_reader(reader: Arc<dyn PolicyRead>) -> TestReaderGuard {
    TEST_READ.with(|slot| *slot.borrow_mut() = Some(reader));
    TestReaderGuard
}

/// Fixed answers for gate tests.
#[derive(Debug)]
pub struct FixedRead {
    pub paths: Mutex<std::result::Result<Vec<String>, String>>,
    pub failures: Mutex<std::result::Result<u32, String>>,
    pub restored: Mutex<std::result::Result<Option<u64>, String>>,
}

impl FixedRead {
    pub fn ok(paths: Vec<String>, failures: u32, restored: Option<u64>) -> Self {
        Self {
            paths: Mutex::new(Ok(paths)),
            failures: Mutex::new(Ok(failures)),
            restored: Mutex::new(Ok(restored)),
        }
    }
}

impl PolicyRead for FixedRead {
    fn changed_paths(&self, _cwd: &Path, _base_ref: &str, _tip: &str) -> Result<Vec<String>> {
        match &*self.paths.lock().expect("paths") {
            Ok(paths) => Ok(paths.clone()),
            Err(msg) => Err(CoordinatorError::Message(msg.clone())),
        }
    }

    fn failure_count(&self, _track: &str) -> Result<u32> {
        match &*self.failures.lock().expect("failures") {
            Ok(n) => Ok(*n),
            Err(msg) => Err(CoordinatorError::Message(msg.clone())),
        }
    }

    fn restored_epoch(&self) -> Result<Option<u64>> {
        match &*self.restored.lock().expect("restored") {
            Ok(v) => Ok(*v),
            Err(msg) => Err(CoordinatorError::Message(msg.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ENV_COORDINATOR_HOME, MachineConfig, save_machine_config, test_env_lock};
    use crate::layout::LayoutProfile;
    use crate::state::{RunState, save_run_state};
    use std::process::Command;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn record(path: &Path) -> ProjectRecord {
        ProjectRecord {
            id: Uuid::new_v4().to_string(),
            path: path.to_path_buf(),
            display_name: None,
            layout_profile: LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: Some(path.to_path_buf()),
            execution_repos: Default::default(),
            state_dir: Some(path.join("state")),
            auto_merge: true,
            phase_timeouts_secs: Default::default(),
            notify_progress: false,
            worktree_isolation: false,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            ci_fix_routing: false,
            created_at: Utc::now(),
        }
    }

    fn rule(name: &str, action: PolicyAction) -> PolicyRule {
        PolicyRule {
            name: name.to_string(),
            action,
            threshold: None,
        }
    }

    #[test]
    fn defaults_are_report_and_project_overrides_machine() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::remove_var(ENV_STATE_POLICIES);
        }
        let mut machine = MachineConfig::default();
        machine
            .state_policies
            .push(rule(NAME_DEPENDENCY, PolicyAction::Block));
        save_machine_config(&machine).unwrap();
        let dir = tempdir().unwrap();
        let mut rec = record(dir.path());
        let resolved_machine = resolved(&rec);
        assert_eq!(
            resolved_machine[0].action,
            PolicyAction::Block,
            "machine overrides default"
        );
        assert_eq!(resolved_machine[0].source, PolicySource::Machine);
        assert_eq!(resolved_machine[2].threshold, DEFAULT_FAILURE_THRESHOLD);
        assert_eq!(resolved_machine[2].source, PolicySource::Default);
        rec.state_policies
            .push(rule(NAME_DEPENDENCY, PolicyAction::Allow));
        let resolved_project = resolved(&rec);
        assert_eq!(resolved_project[0].action, PolicyAction::Allow);
        assert_eq!(resolved_project[0].source, PolicySource::Project);
    }

    #[test]
    fn env_off_disables_without_reading() {
        let _guard = test_env_lock();
        unsafe {
            std::env::set_var(ENV_STATE_POLICIES, "off");
        }
        let dir = tempdir().unwrap();
        let rec = record(dir.path());
        let rules = resolved(&rec);
        assert!(rules.iter().all(|r| r.source == PolicySource::Off));
        let state = RunState::idle(&rec.id);
        let decision = decide(&rec, &state, dir.path(), "HEAD");
        assert_eq!(decision, GateDecision::Allow);
        unsafe {
            std::env::remove_var(ENV_STATE_POLICIES);
        }
    }

    #[test]
    fn unknown_name_is_an_error() {
        assert!(require_name("sandbox").is_err());
        assert_eq!(require_name(NAME_DEPENDENCY).unwrap(), NAME_DEPENDENCY);
    }

    #[test]
    fn manifest_and_workflow_match_and_source_does_not() {
        let paths = vec![
            "src/main.rs".into(),
            "crates/app/Cargo.toml".into(),
            ".github/workflows/ci.yml".into(),
            "pkg\\package.json".into(),
        ];
        assert_eq!(
            matching_paths(NAME_DEPENDENCY, &paths),
            vec![
                "crates/app/Cargo.toml".to_string(),
                "pkg\\package.json".to_string()
            ]
        );
        assert_eq!(
            matching_paths(NAME_CI_WORKFLOW, &paths),
            vec![".github/workflows/ci.yml".to_string()]
        );
    }

    #[test]
    fn block_on_manifest_and_report_records_the_gate() {
        let dir = tempdir().unwrap();
        let mut rec = record(dir.path());
        rec.state_policies
            .push(rule(NAME_DEPENDENCY, PolicyAction::Block));
        let mut state = RunState::idle(&rec.id);
        state.track_id = Some("0069".into());
        state.run_epoch = 1;
        let _reader =
            install_test_reader(Arc::new(FixedRead::ok(vec!["Cargo.toml".into()], 0, None)));
        let decision = decide(&rec, &state, dir.path(), "HEAD");
        match decision {
            GateDecision::Block(gate) => {
                assert_eq!(gate.name, NAME_DEPENDENCY);
                assert!(gate.detail.starts_with("policy: block dependency-manifest"));
            }
            other => panic!("expected block, got {other:?}"),
        }
    }

    #[test]
    fn unreadable_diff_blocks_even_when_action_is_report() {
        let dir = tempdir().unwrap();
        let rec = record(dir.path());
        let state = RunState::idle(&rec.id);
        let _reader = install_test_reader(Arc::new(FixedRead {
            paths: Mutex::new(Err("git diff failed".into())),
            failures: Mutex::new(Ok(0)),
            restored: Mutex::new(Ok(None)),
        }));
        let decision = decide(&rec, &state, dir.path(), "HEAD");
        match decision {
            GateDecision::Block(gate) => {
                assert!(gate.detail.contains("unreadable"), "{}", gate.detail);
            }
            other => panic!("expected block, got {other:?}"),
        }
    }

    #[test]
    fn empty_diff_is_allow() {
        let dir = tempdir().unwrap();
        let rec = record(dir.path());
        let state = RunState::idle(&rec.id);
        let _reader =
            install_test_reader(Arc::new(FixedRead::ok(vec!["src/lib.rs".into()], 0, None)));
        assert_eq!(
            decide(&rec, &state, dir.path(), "HEAD"),
            GateDecision::Allow
        );
    }

    #[test]
    fn require_approval_holds_until_fingerprint_matches() {
        let dir = tempdir().unwrap();
        let mut rec = record(dir.path());
        rec.state_policies
            .push(rule(NAME_DEPENDENCY, PolicyAction::RequireApproval));
        let mut state = RunState::idle(&rec.id);
        state.track_id = Some("0069".into());
        let _reader =
            install_test_reader(Arc::new(FixedRead::ok(vec!["Cargo.toml".into()], 0, None)));
        assert!(decide(&rec, &state, dir.path(), "HEAD").holds());
        approve(&rec, &mut state, dir.path(), NAME_DEPENDENCY).unwrap();
        assert!(decide(&rec, &state, dir.path(), "HEAD").permits_publish());
        let _reader = install_test_reader(Arc::new(FixedRead::ok(
            vec!["Cargo.toml".into(), "Cargo.lock".into()],
            0,
            None,
        )));
        assert!(
            decide(&rec, &state, dir.path(), "HEAD").holds(),
            "a new manifest path changes the fingerprint"
        );
    }

    #[test]
    fn approve_refuses_an_unreadable_fingerprint() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("state")).unwrap();
        let rec = record(dir.path());
        let mut state = RunState::idle(&rec.id);
        save_run_state(&rec, &state).unwrap();
        let _reader = install_test_reader(Arc::new(FixedRead {
            paths: Mutex::new(Err("missing ref".into())),
            failures: Mutex::new(Ok(0)),
            restored: Mutex::new(Ok(None)),
        }));
        assert!(approve(&rec, &mut state, dir.path(), NAME_DEPENDENCY).is_err());
        assert!(state.policy_approvals.is_empty());
    }

    #[test]
    fn consecutive_failures_and_restore_match() {
        let dir = tempdir().unwrap();
        let mut rec = record(dir.path());
        rec.state_policies
            .push(rule(NAME_FAILURES, PolicyAction::Block));
        rec.state_policies
            .push(rule(NAME_RESTORE, PolicyAction::Report));
        let mut state = RunState::idle(&rec.id);
        state.track_id = Some("0069".into());
        let _reader = install_test_reader(Arc::new(FixedRead::ok(vec![], 3, None)));
        match decide(&rec, &state, dir.path(), "HEAD") {
            GateDecision::Block(gate) => assert_eq!(gate.name, NAME_FAILURES),
            other => panic!("expected failure block, got {other:?}"),
        }
        rec.state_policies.clear();
        rec.state_policies
            .push(rule(NAME_RESTORE, PolicyAction::Report));
        let _reader = install_test_reader(Arc::new(FixedRead::ok(vec![], 0, Some(4))));
        match decide(&rec, &state, dir.path(), "HEAD") {
            GateDecision::Report(gate) => assert_eq!(gate.name, NAME_RESTORE),
            other => panic!("expected restore report, got {other:?}"),
        }
    }

    #[test]
    fn missing_checkpoint_ref_is_an_error_not_an_empty_diff() {
        let dir = tempdir().unwrap();
        git(dir.path(), &["init", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "policy@example.com"]);
        git(dir.path(), &["config", "user.name", "policy"]);
        std::fs::write(dir.path().join("README.md"), b"seed\n").unwrap();
        git(dir.path(), &["add", "README.md"]);
        git(dir.path(), &["commit", "-m", "seed"]);
        let err = git_changed_paths(dir.path(), "refs/coordinator/checkpoints/missing/1", "HEAD")
            .unwrap_err();
        assert!(err.to_string().contains("unreadable checkpoint"), "{err}");
    }

    #[test]
    fn git_diff_lists_a_manifest_added_after_the_checkpoint() {
        let dir = tempdir().unwrap();
        git(dir.path(), &["init", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "policy@example.com"]);
        git(dir.path(), &["config", "user.name", "policy"]);
        std::fs::write(dir.path().join("README.md"), b"seed\n").unwrap();
        git(dir.path(), &["add", "README.md"]);
        git(dir.path(), &["commit", "-m", "seed"]);
        let sha = git_stdout(dir.path(), &["rev-parse", "HEAD"]);
        git(
            dir.path(),
            &["update-ref", "refs/coordinator/checkpoints/p/1", &sha],
        );
        std::fs::write(dir.path().join("Cargo.toml"), b"[package]\nname=\"x\"\n").unwrap();
        git(dir.path(), &["add", "Cargo.toml"]);
        git(dir.path(), &["commit", "-m", "add manifest"]);
        let names =
            git_changed_paths(dir.path(), "refs/coordinator/checkpoints/p/1", "HEAD").unwrap();
        assert_eq!(names, vec!["Cargo.toml".to_string()]);
    }

    fn git(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_stdout(cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
}
