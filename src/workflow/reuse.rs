//! Content-addressed plan-review reuse (track 0083).
//!
//! A fresh `run` starts at `ci-wait` only when `spec.md`, `plan.md`, the
//! review-track skill, both slot bindings, and both review files still match
//! `reuse.json`, and `gh pr view` still shows that receipt PR on this track's
//! branch. The probe runs outside the run-state lock.
//!
//! 0101: `PrReceipt` records the delivery repo. The resume probe uses that repo when it still
//! exists, else the current gate target, else the documented legacy primary. A receipt repo
//! that disagrees with the resolved gate target falls through to plan instead of resuming.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{ENV_COORDINATOR_AGY_BIN, ENV_COORDINATOR_OPENCODE_BIN, RoleBinding};
use crate::error::{CoordinatorError, Result};
use crate::registry::ProjectRecord;
use crate::state::{RunState, RunStatus, load_run_state};

use super::graph::{
    REVIEW_SLUG_OPENCODE, ROLE_REVIEWER_AGY, ROLE_REVIEWER_OPENCODE, resolve_track_dir,
    review_slugs,
};

const RECEIPT_VERSION: u32 = 1;

/// `gh pr view` fields the resume probe needs. OPEN and CLOSED stay distinct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumePr {
    pub state: String,
    pub head_oid: String,
    pub head_ref: String,
}

/// Advance/run snapshot captured before `gh pr view`. The caller probes outside
/// both the apply mutex and the run-state lock, then commits only if these
/// fields still match.
#[derive(Debug, Clone)]
pub struct ReuseProbe {
    pub successor_id: String,
    pub closed_epoch: u64,
    pub snap_epoch: u64,
    pub snap_track: Option<String>,
    pub snap_status: RunStatus,
    pub number: u64,
    pub head_sha: String,
    pub head_ref: String,
    /// 0101: receipt repo for the `gh pr view` cwd.
    pub repo: Option<PathBuf>,
}

/// What a fresh start should do before any `gh` call.
#[derive(Debug, Clone)]
pub enum ResumeChoice {
    Plan {
        archive_slugs: Vec<String>,
        journal_unreadable: bool,
    },
    Probe {
        number: u64,
        head_sha: String,
        head_ref: String,
        repo: Option<PathBuf>,
    },
}

/// Copy of one unmatched track review into `prior/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveOutcome {
    Absent,
    Archived,
    Conflict,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Receipt {
    version: u32,
    track_id: String,
    spec_sha256: String,
    plan_sha256: String,
    skill_sha256: String,
    slots: BTreeMap<String, SlotReceipt>,
    /// 0119: the `run_epoch` that **produced** `slots` - not the live epoch.
    ///
    /// `note_pr` round-trips the struct and must never rewrite it. Additive and defaulted;
    /// `RECEIPT_VERSION` stays 1 so pre-0119 receipts stay readable (they simply fail the
    /// same-epoch predicate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    epoch: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pr: Option<PrReceipt>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SlotReceipt {
    command: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    env_bin: Option<String>,
    review_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PrReceipt {
    number: u64,
    head_sha: String,
    head_ref: String,
    /// 0101: the delivery repo the PR was opened/published in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    repo: Option<PathBuf>,
}

struct SharedHashes {
    spec: String,
    plan: String,
    skill: String,
}

enum LoadedReceipt {
    Missing,
    Unreadable,
    Ready(Box<Receipt>),
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex_lower(Sha256::digest(bytes).as_slice())
}

fn skill_markdown(root: &Path, dir_name: &str) -> PathBuf {
    root.join(".agents")
        .join("skills")
        .join(dir_name)
        .join("SKILL.md")
}

/// Alias directory segment, or the canonical name when the map has no entry.
pub fn skill_directory_name(record: &ProjectRecord, canonical: &str) -> String {
    record
        .skill_aliases
        .get(canonical)
        .cloned()
        .unwrap_or_else(|| canonical.to_string())
}

/// Primary `{workspace}/.agents/skills/{dir}/SKILL.md`, then conductor-parent fallback.
pub fn workspace_skill_candidates(record: &ProjectRecord, canonical: &str) -> (PathBuf, PathBuf) {
    let dir_name = skill_directory_name(record, canonical);
    let paths = crate::layout::resolve(record);
    let primary = skill_markdown(&paths.workspace_root, &dir_name);
    let fallback = match paths.conductor_dir.parent() {
        Some(parent) => skill_markdown(parent, &dir_name),
        None => primary.clone(),
    };
    (primary, fallback)
}

/// Workspace skill, else `{conductor_dir.parent()}/.agents/skills/{alias}/SKILL.md`.
pub fn review_track_skill_candidates(record: &ProjectRecord) -> (PathBuf, PathBuf) {
    workspace_skill_candidates(record, "review-track")
}

/// Execution root `execution_skill` already uses. No conductor-parent fallback.
pub fn execution_skill_path(record: &ProjectRecord, canonical: &str) -> PathBuf {
    let dir_name = skill_directory_name(record, canonical);
    let paths = crate::layout::resolve(record);
    let fallback = paths.execution_repo.clone().unwrap_or(paths.workspace_root);
    let root = crate::worktree::active_epoch_dir(record).unwrap_or(fallback);
    skill_markdown(&root, &dir_name)
}

/// Chosen file when one exists, otherwise the primary path and `false`.
pub fn chosen_workspace_skill(record: &ProjectRecord, canonical: &str) -> (PathBuf, bool) {
    let (primary, fallback) = workspace_skill_candidates(record, canonical);
    if primary.is_file() {
        return (primary, true);
    }
    if fallback != primary && fallback.is_file() {
        return (fallback, true);
    }
    (primary, false)
}

/// Paths to name when neither workspace candidate is a file.
pub fn missing_workspace_skill(record: &ProjectRecord, canonical: &str) -> Vec<PathBuf> {
    let (primary, fallback) = workspace_skill_candidates(record, canonical);
    if primary.is_file() || (fallback != primary && fallback.is_file()) {
        return Vec::new();
    }
    if primary == fallback {
        vec![primary]
    } else {
        vec![primary, fallback]
    }
}

fn missing_execution_skill(record: &ProjectRecord, canonical: &str) -> Vec<PathBuf> {
    let path = execution_skill_path(record, canonical);
    if path.is_file() {
        Vec::new()
    } else {
        vec![path]
    }
}

/// Skills this phase must have on disk before an adapter inject or plan-review spawn.
///
/// Plan shares `plan_phase_requires_skill` with the prompt body. A workspace name is
/// satisfied when either candidate is a file.
pub fn missing_required_skill_paths(
    record: &ProjectRecord,
    phase: &str,
    track_id: Option<&str>,
) -> Vec<PathBuf> {
    use super::graph::{
        PHASE_ADDRESS_FINDINGS, PHASE_ADVANCE, PHASE_FOLD, PHASE_IMPLEMENT, PHASE_PLAN,
        PHASE_PLAN_REVIEW,
    };
    match phase {
        PHASE_PLAN => {
            if !super::prompts::plan_phase_requires_skill(record, track_id) {
                return Vec::new();
            }
            missing_workspace_skill(record, "plan")
        }
        PHASE_FOLD => missing_workspace_skill(record, "foldin"),
        PHASE_ADVANCE => missing_workspace_skill(record, "plan"),
        PHASE_IMPLEMENT | PHASE_ADDRESS_FINDINGS => {
            let mut missing = missing_execution_skill(record, "implement");
            missing.extend(missing_execution_skill(record, "onboarding"));
            missing
        }
        PHASE_PLAN_REVIEW => missing_workspace_skill(record, "review-track"),
        _ => Vec::new(),
    }
}

pub fn skill_missing_message(paths: &[PathBuf]) -> String {
    let listed = paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join("; ");
    format!("skill_missing: {listed}")
}

pub fn review_track_skill_file(record: &ProjectRecord) -> Option<PathBuf> {
    let (primary, fallback) = review_track_skill_candidates(record);
    if primary.is_file() {
        return Some(primary);
    }
    if fallback.is_file() {
        return Some(fallback);
    }
    None
}

/// Prompt fragment: the resolved file, or both candidates when neither exists.
pub fn review_track_skill_prompt(record: &ProjectRecord) -> String {
    match review_track_skill_file(record) {
        Some(path) => path.display().to_string(),
        None => {
            let (primary, fallback) = review_track_skill_candidates(record);
            if primary == fallback {
                primary.display().to_string()
            } else {
                format!("{} and {}", primary.display(), fallback.display())
            }
        }
    }
}

fn hash_file(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(sha256_hex(&bytes))
}

fn shared_hashes(record: &ProjectRecord, track_dir: &Path) -> Option<SharedHashes> {
    let spec = hash_file(&track_dir.join("spec.md"))?;
    let plan = hash_file(&track_dir.join("plan.md"))?;
    let skill = hash_file(review_track_skill_file(record)?.as_path())?;
    Some(SharedHashes { spec, plan, skill })
}

/// 0097: current `spec.md`/`plan.md` content hashes for a track directory.
pub fn current_artifact_pair(track_dir: &Path) -> Option<(String, String)> {
    Some((
        hash_file(&track_dir.join("spec.md"))?,
        hash_file(&track_dir.join("plan.md"))?,
    ))
}

/// 0097: the `spec.md`/`plan.md` hashes the last `plan-review` receipt covers. `None` when
/// there is no readable receipt — a fold with nothing to compare against keeps the old
/// behaviour rather than inventing a re-review.
pub fn reviewed_artifact_pair(record: &ProjectRecord, track_id: &str) -> Option<(String, String)> {
    let dir = crate::workflow::graph::resolve_track_dir(record, track_id)?;
    match load_receipt(&dir) {
        LoadedReceipt::Ready(r) => Some((r.spec_sha256, r.plan_sha256)),
        _ => None,
    }
}

fn shared_match(record: &ProjectRecord, track_dir: &Path, receipt: &Receipt) -> bool {
    match shared_hashes(record, track_dir) {
        Some(h) => {
            h.spec == receipt.spec_sha256
                && h.plan == receipt.plan_sha256
                && h.skill == receipt.skill_sha256
        }
        None => false,
    }
}

fn norm_model(model: Option<&str>) -> Option<String> {
    model
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn env_bin(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn load_slot_binding(slug: &str) -> RoleBinding {
    let bindings = crate::harness::load_role_bindings()
        .unwrap_or_else(|_| crate::config::default_role_bindings());
    match slug {
        REVIEW_SLUG_OPENCODE => {
            bindings
                .get(ROLE_REVIEWER_OPENCODE)
                .cloned()
                .unwrap_or(RoleBinding {
                    harness: "opencode".into(),
                    command: "opencode".into(),
                    model: None,
                })
        }
        _ => bindings
            .get(ROLE_REVIEWER_AGY)
            .cloned()
            .unwrap_or(RoleBinding {
                harness: "antigravity".into(),
                command: "agy".into(),
                model: None,
            }),
    }
}

fn slot_env_key(slug: &str) -> &'static str {
    match slug {
        REVIEW_SLUG_OPENCODE => ENV_COORDINATOR_OPENCODE_BIN,
        _ => ENV_COORDINATOR_AGY_BIN,
    }
}

fn current_slot(slug: &str) -> SlotReceipt {
    let binding = load_slot_binding(slug);
    SlotReceipt {
        command: binding.command,
        model: norm_model(binding.model.as_deref()),
        env_bin: env_bin(slot_env_key(slug)),
        review_sha256: String::new(),
    }
}

fn review_bytes(track_dir: &Path, slug: &str) -> Option<Vec<u8>> {
    let path = track_dir.join(format!("{slug}-review.md"));
    let bytes = std::fs::read(path).ok()?;
    if bytes.is_empty() { None } else { Some(bytes) }
}

fn slot_ok(track_dir: &Path, slug: &str, receipt: &Receipt) -> bool {
    let Some(stored) = receipt.slots.get(slug) else {
        return false;
    };
    let live = current_slot(slug);
    if stored.command != live.command
        || norm_model(stored.model.as_deref()) != live.model
        || stored.env_bin != live.env_bin
    {
        return false;
    }
    let Some(bytes) = review_bytes(track_dir, slug) else {
        return false;
    };
    !stored.review_sha256.is_empty() && sha256_hex(&bytes) == stored.review_sha256
}

fn live_review_slugs(track_dir: &Path) -> Vec<String> {
    review_slugs()
        .iter()
        .filter(|slug| track_dir.join(format!("{slug}-review.md")).is_file())
        .map(|slug| (*slug).to_string())
        .collect()
}

fn receipt_path(track_dir: &Path) -> PathBuf {
    track_dir.join("reuse.json")
}

fn load_receipt(track_dir: &Path) -> LoadedReceipt {
    let path = receipt_path(track_dir);
    if !path.is_file() {
        return LoadedReceipt::Missing;
    }
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return LoadedReceipt::Unreadable,
    };
    match serde_json::from_slice::<Receipt>(&bytes) {
        Ok(receipt) if receipt.version == RECEIPT_VERSION => {
            LoadedReceipt::Ready(Box::new(receipt))
        }
        _ => LoadedReceipt::Unreadable,
    }
}

fn write_receipt(path: &Path, receipt: &Receipt) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(receipt)
        .map_err(|e| CoordinatorError::Message(format!("reuse.json: {e}")))?;
    bytes.push(b'\n');
    crate::persist::atomic_write(path, &bytes)
}

/// Decide plan vs ci-wait probe from the receipt alone. Does not call `gh`.
pub fn decide_resume(record: &ProjectRecord, track_id: Option<&str>) -> ResumeChoice {
    let Some(track_id) = track_id.map(str::trim).filter(|s| !s.is_empty()) else {
        return ResumeChoice::Plan {
            archive_slugs: Vec::new(),
            journal_unreadable: false,
        };
    };
    let Some(track_dir) = resolve_track_dir(record, track_id) else {
        return ResumeChoice::Plan {
            archive_slugs: Vec::new(),
            journal_unreadable: false,
        };
    };
    let plan = |journal_unreadable: bool| ResumeChoice::Plan {
        archive_slugs: live_review_slugs(&track_dir),
        journal_unreadable,
    };
    let receipt = match load_receipt(&track_dir) {
        LoadedReceipt::Missing => {
            return ResumeChoice::Plan {
                archive_slugs: live_review_slugs(&track_dir),
                journal_unreadable: false,
            };
        }
        LoadedReceipt::Unreadable => return plan(true),
        LoadedReceipt::Ready(receipt) => *receipt,
    };
    if !shared_match(record, &track_dir, &receipt) {
        return plan(false);
    }
    let mut archive_slugs = Vec::new();
    let mut both = true;
    for slug in review_slugs() {
        if slot_ok(&track_dir, slug, &receipt) {
            continue;
        }
        both = false;
        if track_dir.join(format!("{slug}-review.md")).is_file() {
            archive_slugs.push((*slug).to_string());
        }
    }
    if both
        && let Some(pr) = receipt.pr.as_ref()
        && !pr.head_sha.is_empty()
    {
        // 0101: a recorded repo that still resolves to a different gate target must not
        // resume; fall through to plan so the leg is replayed.
        if let Some(repo) = pr.repo.as_ref()
            && repo.is_dir()
            && let Ok(mut state) = load_run_state(record)
        {
            state.track_id = Some(track_id.to_string());
            state.phase = crate::workflow::graph::PHASE_CI_WAIT.into();
            if let Ok(target) = crate::workflow::execution_repo::resolve_track_target(
                record,
                &state,
                crate::workflow::graph::PHASE_CI_WAIT,
            ) && !crate::workflow::execution_repo::same_git_repository(repo, &target.sample_path)
            {
                return plan(false);
            }
        }
        return ResumeChoice::Probe {
            number: pr.number,
            head_sha: pr.head_sha.clone(),
            head_ref: pr.head_ref.clone(),
            repo: pr.repo.clone(),
        };
    }
    ResumeChoice::Plan {
        archive_slugs,
        journal_unreadable: false,
    }
}

/// True when this slot's binding and track review bytes match the receipt for `run_epoch`.
///
/// 0119: the receipt must record the epoch that produced its slots. No production path calls this
/// after in-phase reuse was retired; it stays as the public reuse predicate (a pre-0119 receipt
/// with no epoch is refused).
pub fn slot_matches(record: &ProjectRecord, track_id: &str, slug: &str, run_epoch: u64) -> bool {
    let Some(track_dir) = resolve_track_dir(record, track_id) else {
        return false;
    };
    let LoadedReceipt::Ready(receipt) = load_receipt(&track_dir) else {
        return false;
    };
    receipt.epoch == Some(run_epoch)
        && shared_match(record, &track_dir, &receipt)
        && slot_ok(&track_dir, slug, &receipt)
}

/// Replace `reuse.json` from the reviews this entry actually produced.
///
/// The hash store is still the **track dir** (the store `slot_ok`/`decide_resume` verify), so
/// writer and verifier never split. The slot set is filtered by this entry's mailbox evidence
/// (`review_produced`), so a leftover track file for a degraded slot is not blessed, and `epoch`
/// records the producing epoch.
///
/// Omits `pr` (ci-wait `note_pr` attaches it later). A write failure is ignored so the join still
/// succeeds; the next run re-reviews.
pub fn write_join_receipt(record: &ProjectRecord, state: &RunState) {
    let Some(track_id) = state.track_id.as_deref() else {
        return;
    };
    let Some(track_dir) = resolve_track_dir(record, track_id) else {
        return;
    };
    let Some(hashes) = shared_hashes(record, &track_dir) else {
        return;
    };
    let mut slots = BTreeMap::new();
    for slug in review_slugs() {
        if !super::bundle::review_produced(record, state, slug) {
            continue;
        }
        let Some(bytes) = review_bytes(&track_dir, slug) else {
            continue;
        };
        let mut slot = current_slot(slug);
        slot.review_sha256 = sha256_hex(&bytes);
        slots.insert((*slug).to_string(), slot);
    }
    let receipt = Receipt {
        version: RECEIPT_VERSION,
        track_id: track_id.to_string(),
        spec_sha256: hashes.spec,
        plan_sha256: hashes.plan,
        skill_sha256: hashes.skill,
        slots,
        epoch: Some(state.run_epoch),
        pr: None,
    };
    let _ = write_receipt(&receipt_path(&track_dir), &receipt);
}

/// Attach or clear the receipt PR. Missing or unreadable receipts no-op.
///
/// The same number, head sha, and head ref return before hashing, so a later
/// spec edit does not rewrite the file. A shared-input mismatch clears `pr`
/// and keeps slots. Errors do not fail ci-wait.
pub fn note_pr(
    record: &ProjectRecord,
    track_id: &str,
    number: u64,
    head_sha: &str,
    head_ref: &str,
    repo: Option<&Path>,
) {
    let Some(track_dir) = resolve_track_dir(record, track_id) else {
        return;
    };
    let path = receipt_path(&track_dir);
    if !path.is_file() {
        return;
    }
    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };
    let Ok(mut receipt) = serde_json::from_slice::<Receipt>(&bytes) else {
        return;
    };
    if receipt.version != RECEIPT_VERSION {
        return;
    }
    let repo_same = match (receipt.pr.as_ref().and_then(|pr| pr.repo.as_deref()), repo) {
        (None, None) => true,
        (Some(a), Some(b)) => crate::workflow::execution_repo::paths_same(a, b),
        _ => false,
    };
    if let Some(pr) = receipt.pr.as_ref()
        && pr.number == number
        && pr.head_sha == head_sha
        && pr.head_ref == head_ref
        && repo_same
    {
        return;
    }
    if !shared_match(record, &track_dir, &receipt) {
        if receipt.pr.is_some() {
            receipt.pr = None;
            let _ = write_receipt(&path, &receipt);
        }
        return;
    }
    receipt.pr = Some(PrReceipt {
        number,
        head_sha: head_sha.to_string(),
        head_ref: head_ref.to_string(),
        repo: repo.map(Path::to_path_buf),
    });
    let _ = write_receipt(&path, &receipt);
}

/// `persist_target` / `try_auto_publish` hook. HeadSha and a pull request with no head
/// oid do not write. `repo` is the delivery repo the PR was published in.
pub fn note_ci_target(
    record: &ProjectRecord,
    track_id: &str,
    target: &crate::ci::backend::CiTarget,
    repo: Option<&Path>,
) {
    let crate::ci::backend::CiTarget::PullRequest {
        number,
        head_oid: Some(oid),
        head_ref,
        ..
    } = target
    else {
        return;
    };
    note_pr(record, track_id, *number, oid, head_ref, repo);
}

/// Byte-copy `prior/{epoch}-{slug}-review.md`. Identical bytes count as archived.
/// Different bytes or an IO error leave both files (`Conflict`).
pub fn archive_track_review(track_dir: &Path, slug: &str, closed_epoch: u64) -> ArchiveOutcome {
    let src = track_dir.join(format!("{slug}-review.md"));
    if !src.is_file() {
        return ArchiveOutcome::Absent;
    }
    let Ok(bytes) = std::fs::read(&src) else {
        return ArchiveOutcome::Conflict;
    };
    let dest = track_dir
        .join("prior")
        .join(format!("{closed_epoch}-{slug}-review.md"));
    if dest.is_file() {
        return match std::fs::read(&dest) {
            Ok(existing) if existing == bytes => ArchiveOutcome::Archived,
            _ => ArchiveOutcome::Conflict,
        };
    }
    if crate::persist::atomic_write(&dest, &bytes).is_err() {
        return ArchiveOutcome::Conflict;
    }
    ArchiveOutcome::Archived
}

/// OPEN or MERGED, head oid equals the receipt, and `branch_is_track`.
pub fn probe_accepts(track_id: &str, expect_sha: &str, answer: &Result<Option<ResumePr>>) -> bool {
    let Ok(Some(pr)) = answer else {
        return false;
    };
    let Some(numeric) = crate::notify::artifact::numeric_track_id(track_id) else {
        return false;
    };
    let state_ok = pr.state.eq_ignore_ascii_case("open") || pr.state.eq_ignore_ascii_case("merged");
    state_ok && pr.head_oid == expect_sha && crate::ci::gh::branch_is_track(&pr.head_ref, numeric)
}

#[cfg(test)]
use std::sync::Arc;

#[cfg(test)]
type ResumeTestProbe = Arc<dyn Fn(u64) -> Result<Option<ResumePr>> + Send + Sync>;

#[cfg(test)]
thread_local! {
    static TEST_PROBE: std::cell::RefCell<Option<ResumeTestProbe>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub struct TestProbeGuard;

#[cfg(test)]
impl Drop for TestProbeGuard {
    fn drop(&mut self) {
        TEST_PROBE.with(|cell| *cell.borrow_mut() = None);
    }
}

#[cfg(test)]
pub fn install_test_probe(probe: ResumeTestProbe) -> TestProbeGuard {
    TEST_PROBE.with(|cell| *cell.borrow_mut() = Some(probe));
    TestProbeGuard
}

/// Live `gh pr view` outside tests. An unset test probe returns an error so
/// default `cargo test` never reaches the network and resume falls through to `plan`.
/// Receipt repo first (when it still exists), then the current gate target, then the
/// documented legacy primary. `None` means no probe is possible.
pub(crate) fn resume_probe_cwd(record: &ProjectRecord, repo: Option<&Path>) -> Option<PathBuf> {
    repo.filter(|path| path.is_dir())
        .map(Path::to_path_buf)
        .or_else(|| {
            let state = load_run_state(record).ok()?;
            crate::workflow::execution_repo::gate_target(record, &state)
                .ok()
                .map(|gate| gate.execution.sample_path)
        })
        .or_else(|| crate::worktree::product_git_cwd(record))
}

pub fn probe_resume(
    record: &ProjectRecord,
    number: u64,
    repo: Option<&Path>,
) -> Result<Option<ResumePr>> {
    #[cfg(test)]
    {
        let _ = (record, repo);
        let installed = TEST_PROBE.with(|cell| cell.borrow().clone());
        match installed {
            Some(probe) => probe(number),
            None => Err(CoordinatorError::Message(
                "resume probe unset in test".into(),
            )),
        }
    }
    #[cfg(not(test))]
    {
        let Some(cwd) = resume_probe_cwd(record, repo) else {
            return Err(CoordinatorError::Message("no execution repo".into()));
        };
        let head = crate::ci::gh::pr_view_resume(&cwd, number)?;
        Ok(head.map(|h| ResumePr {
            state: h.state,
            head_oid: h.head_oid,
            head_ref: h.head_ref,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ci::backend::{CiTarget, MergeStateStatus};
    use crate::config::test_env_lock;
    use crate::state::RunState;
    use crate::workflow::graph::REVIEW_SLUG_AGY;
    use chrono::Utc;
    use std::collections::BTreeMap;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn rec(path: &std::path::Path) -> ProjectRecord {
        ProjectRecord {
            id: Uuid::new_v4().to_string(),
            path: path.to_path_buf(),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: BTreeMap::new(),
            state_dir: None,
            auto_merge: true,
            phase_timeouts_secs: BTreeMap::new(),
            notify_progress: false,
            worktree_isolation: false,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            ci_fix_routing: false,
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: Utc::now(),
        }
    }

    fn track_dir_of(root: &Path, id: &str) -> PathBuf {
        let dir = root.join("conductor").join(format!("{id}-Example"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_skill(root: &Path, bytes: &[u8]) -> PathBuf {
        let path = root
            .join(".agents")
            .join("skills")
            .join("review-track")
            .join("SKILL.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// Restore one process variable when the fixture (or a panicking test) drops it.
    struct RestoreEnv {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }

    impl RestoreEnv {
        fn set_path(key: &'static str, value: &Path) -> Self {
            let prev = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, prev }
        }

        fn remove(key: &'static str) -> Self {
            let prev = std::env::var_os(key);
            unsafe { std::env::remove_var(key) };
            Self { key, prev }
        }
    }

    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            unsafe {
                match self.prev.take() {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    /// Machine-config isolation plus a populated track for the whole caller body.
    ///
    /// Field order is the drop order: env guards first, then the temp dirs they
    /// point at, then the non-reentrant test lock. A panic inside a test
    /// restores the developer's environment before the dirs are removed and
    /// before the next test can acquire the lock.
    struct ReuseFixture {
        _restore_agy: RestoreEnv,
        _restore_home: RestoreEnv,
        root: tempfile::TempDir,
        home: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
        record: ProjectRecord,
        /// 0119: the epoch recorded by the fixture receipt.
        epoch: u64,
    }

    impl ReuseFixture {
        fn record(&self) -> &ProjectRecord {
            &self.record
        }

        fn root(&self) -> &Path {
            self.root.path()
        }

        fn home(&self) -> &Path {
            self.home.path()
        }

        fn epoch(&self) -> u64 {
            self.epoch
        }
    }

    /// 0119: seed the mailbox copies the receipt writer filters on and build a matching state.
    fn join_state(r: &ProjectRecord, id: &str) -> RunState {
        let track = resolve_track_dir(r, id).expect("fixture track dir");
        let mut state = RunState::idle(&r.id);
        state.track_id = Some(id.to_string());
        state.run_epoch = 1;
        for slug in review_slugs() {
            let src = track.join(format!("{slug}-review.md"));
            if let Ok(bytes) = std::fs::read(&src) {
                let dest = crate::workflow::bundle::review_file(r, slug).unwrap();
                std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
                std::fs::write(dest, bytes).unwrap();
            }
        }
        state
    }

    fn matching_fixture(id: &str) -> ReuseFixture {
        matching_fixture_locked(id, test_env_lock())
    }

    /// Build the fixture with an already-acquired lock. The caller holds the only
    /// non-reentrant guard, so it can set up sentinel values before construction.
    fn matching_fixture_locked(id: &str, lock: std::sync::MutexGuard<'static, ()>) -> ReuseFixture {
        // `slot_matches` resolves the live binding from the machine config, so without
        // isolation every caller reads the developer's real `config.json`. A host that
        // pins `plan_reviewer_agy.model` would make the null-model assertions fail while
        // CI (which has no config) passes on the same commit. The fixture points the
        // machine home at a temp dir and clears the binding override for the whole body.
        let root = tempdir().unwrap();
        let home = tempdir().unwrap();
        let restore_home = RestoreEnv::set_path(crate::config::ENV_COORDINATOR_HOME, home.path());
        let restore_agy = RestoreEnv::remove(ENV_COORDINATOR_AGY_BIN);

        let r = rec(root.path());
        let dir = track_dir_of(root.path(), id);
        std::fs::write(dir.join("spec.md"), b"spec-v1").unwrap();
        std::fs::write(dir.join("plan.md"), b"plan-v1").unwrap();
        write_skill(root.path(), b"skill-v1");
        std::fs::write(dir.join("agy-review.md"), b"agy body\n").unwrap();
        std::fs::write(dir.join("opencode-review.md"), b"oc body\n").unwrap();
        let state = join_state(&r, id);
        write_join_receipt(&r, &state);

        ReuseFixture {
            _restore_agy: restore_agy,
            _restore_home: restore_home,
            root,
            home,
            _lock: lock,
            record: r,
            epoch: state.run_epoch,
        }
    }

    #[test]
    fn byte_change_refuses_the_slot() {
        let f = matching_fixture("0083");
        let r = f.record();
        assert!(slot_matches(r, "0083", REVIEW_SLUG_AGY, f.epoch()));
        let review = track_dir_of(f.root(), "0083").join("agy-review.md");
        std::fs::write(&review, b"agy body!\n").unwrap();
        assert!(!slot_matches(r, "0083", REVIEW_SLUG_AGY, f.epoch()));
        assert!(slot_matches(r, "0083", REVIEW_SLUG_OPENCODE, f.epoch()));
    }

    #[test]
    fn binding_or_skill_change_refuses_the_slot() {
        // The fixture owns the non-reentrant env lock and the AGY_BIN restore.
        let f = matching_fixture("0083");
        let r = f.record();
        assert!(slot_matches(r, "0083", REVIEW_SLUG_AGY, f.epoch()));
        let skill = write_skill(f.root(), b"skill-v1-changed");
        assert!(!slot_matches(r, "0083", REVIEW_SLUG_AGY, f.epoch()));
        std::fs::write(&skill, b"skill-v1").unwrap();
        assert!(slot_matches(r, "0083", REVIEW_SLUG_AGY, f.epoch()));
        let receipt = track_dir_of(f.root(), "0083").join("reuse.json");
        let mut v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
        v["slots"]["agy"]["model"] = serde_json::json!("other-model");
        std::fs::write(&receipt, serde_json::to_vec(&v).unwrap()).unwrap();
        assert!(!slot_matches(r, "0083", REVIEW_SLUG_AGY, f.epoch()));
        v["slots"]["agy"]["model"] = serde_json::Value::Null;
        std::fs::write(&receipt, serde_json::to_vec(&v).unwrap()).unwrap();
        assert!(slot_matches(r, "0083", REVIEW_SLUG_AGY, f.epoch()));
        unsafe {
            std::env::set_var(ENV_COORDINATOR_AGY_BIN, r"C:\custom\agy.exe");
        }
        assert!(!slot_matches(r, "0083", REVIEW_SLUG_AGY, f.epoch()));
    }

    #[test]
    fn reuse_fixture_restores_env_on_panic_and_recovers_poisoned_lock() {
        let prev_agy = r"C:\sentinel\agy.exe";
        let prev_home = r"C:\sentinel\home";
        let lock = test_env_lock();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_AGY_BIN, prev_agy);
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, prev_home);
        }

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _fixture = matching_fixture_locked("0083", lock);
            panic!("fixture panic with values present");
        }));
        assert!(panicked.is_err());
        assert_eq!(
            std::env::var_os(ENV_COORDINATOR_AGY_BIN).as_deref(),
            Some(std::ffi::OsStr::new(prev_agy))
        );
        assert_eq!(
            std::env::var_os(crate::config::ENV_COORDINATOR_HOME).as_deref(),
            Some(std::ffi::OsStr::new(prev_home))
        );
        // The unwind poisoned the shared lock; the helper must recover it.
        drop(test_env_lock());

        let lock = test_env_lock();
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_AGY_BIN);
            std::env::remove_var(crate::config::ENV_COORDINATOR_HOME);
        }
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _fixture = matching_fixture_locked("0083", lock);
            panic!("fixture panic with values absent");
        }));
        assert!(panicked.is_err());
        assert!(std::env::var_os(ENV_COORDINATOR_AGY_BIN).is_none());
        assert!(std::env::var_os(crate::config::ENV_COORDINATOR_HOME).is_none());
        drop(test_env_lock());
    }

    #[test]
    fn matching_fixture_isolates_config_for_entire_test() {
        let f = matching_fixture("0083");
        assert_eq!(
            std::env::var_os(crate::config::ENV_COORDINATOR_HOME).as_deref(),
            Some(f.home().as_os_str()),
            "machine home must stay redirected for the whole body"
        );

        // A binding pinned only in the fixture home must be the one slot_matches
        // sees; the receipt recorded the compiled default before this write.
        std::fs::write(
            f.home().join("config.json"),
            r#"{"version":1,"scan_roots":[],"role_bindings":{"plan_reviewer_agy":{"harness":"antigravity","command":"agy","model":"host-model"}}}"#,
        )
        .unwrap();
        assert!(!slot_matches(
            f.record(),
            "0083",
            REVIEW_SLUG_AGY,
            f.epoch()
        ));
    }

    #[test]
    fn skill_only_on_conductor_parent_matches() {
        let dir = tempdir().unwrap();
        let product = dir.path().join("product");
        let conductor = dir.path().join("conductor");
        std::fs::create_dir_all(&product).unwrap();
        let track = conductor.join("0083-Example");
        std::fs::create_dir_all(&track).unwrap();
        std::fs::write(track.join("spec.md"), b"spec").unwrap();
        std::fs::write(track.join("plan.md"), b"plan").unwrap();
        std::fs::write(track.join("agy-review.md"), b"agy").unwrap();
        std::fs::write(track.join("opencode-review.md"), b"oc").unwrap();
        let skill = dir
            .path()
            .join(".agents")
            .join("skills")
            .join("review-track")
            .join("SKILL.md");
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(&skill, b"parent-skill").unwrap();
        let mut r = rec(&product);
        r.conductor_dir = Some(conductor);
        assert!(
            !product
                .join(".agents")
                .join("skills")
                .join("review-track")
                .join("SKILL.md")
                .is_file()
        );
        assert_eq!(
            review_track_skill_file(&r).as_deref(),
            Some(skill.as_path())
        );
        write_join_receipt(&r, &join_state(&r, "0083"));
        assert!(slot_matches(&r, "0083", REVIEW_SLUG_AGY, 1));
        assert!(slot_matches(&r, "0083", REVIEW_SLUG_OPENCODE, 1));
    }

    #[test]
    fn note_pr_attaches_when_hashes_match() {
        let f = matching_fixture("0083");
        let r = f.record();
        note_pr(r, "0083", 80, "abc123", "track/0083-Relaunch", None);
        let v: serde_json::Value = serde_json::from_slice(
            &std::fs::read(track_dir_of(f.root(), "0083").join("reuse.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["pr"]["number"], 80);
        assert_eq!(v["pr"]["head_sha"], "abc123");
        assert_eq!(v["pr"]["head_ref"], "track/0083-Relaunch");
        assert!(v["slots"]["agy"]["review_sha256"].is_string());
    }

    #[test]
    fn note_pr_strips_pr_when_inputs_mismatch() {
        let f = matching_fixture("0083");
        let r = f.record();
        note_pr(r, "0083", 80, "abc123", "track/0083-Relaunch", None);
        std::fs::write(track_dir_of(f.root(), "0083").join("spec.md"), b"spec-v2").unwrap();
        note_pr(r, "0083", 81, "def456", "track/0083-Relaunch", None);
        let v: serde_json::Value = serde_json::from_slice(
            &std::fs::read(track_dir_of(f.root(), "0083").join("reuse.json")).unwrap(),
        )
        .unwrap();
        assert!(v.get("pr").is_none() || v["pr"].is_null());
        assert!(v["slots"]["agy"]["review_sha256"].is_string());
        assert!(v["slots"]["opencode"]["review_sha256"].is_string());
    }

    #[test]
    fn note_pr_no_op_when_pr_already_matching() {
        let f = matching_fixture("0083");
        let r = f.record();
        note_pr(r, "0083", 80, "abc123", "track/0083-Relaunch", None);
        let path = track_dir_of(f.root(), "0083").join("reuse.json");
        let before = std::fs::read(&path).unwrap();
        std::fs::write(
            track_dir_of(f.root(), "0083").join("spec.md"),
            b"spec-later",
        )
        .unwrap();
        note_pr(r, "0083", 80, "abc123", "track/0083-Relaunch", None);
        let after = std::fs::read(&path).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn resume_probe_uses_the_recorded_repo() {
        let f = matching_fixture("0083");
        let recorded = tempdir().unwrap();
        assert_eq!(
            resume_probe_cwd(f.record(), Some(recorded.path())).as_deref(),
            Some(recorded.path())
        );
    }

    #[test]
    fn legacy_receipt_without_repo_falls_back() {
        let f = matching_fixture("0083");
        let r = f.record();
        note_pr(r, "0083", 80, "abc123", "track/0083-Relaunch", None);
        match decide_resume(r, Some("0083")) {
            ResumeChoice::Probe { repo, .. } => assert!(repo.is_none()),
            other => panic!("legacy receipt must still probe: {other:?}"),
        }
        // The gate target is the next fallback when the receipt has no repo.
        let repo = tempdir().unwrap();
        let mut state = RunState::idle(&r.id);
        state.phase = crate::workflow::graph::PHASE_CI_WAIT.into();
        state.track_id = Some("0083".into());
        state.run_epoch = 1;
        state.delivery_target = Some(crate::workflow::execution_repo::seeded_delivery_target(
            r,
            "0083",
            1,
            crate::workflow::graph::PHASE_CI_WAIT,
            repo.path(),
        ));
        crate::state::ensure_state_dir(r).unwrap();
        crate::state::save_run_state(r, &state).unwrap();
        let resolved = resume_probe_cwd(r, None).expect("gate target fallback");
        assert!(
            crate::workflow::execution_repo::paths_same(&resolved, repo.path()),
            "{} != {}",
            resolved.display(),
            repo.path().display()
        );
    }

    #[test]
    fn receipt_repo_mismatch_does_not_resume() {
        let f = matching_fixture("0083");
        let mut r = f.record().clone();
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        note_pr(
            &r,
            "0083",
            80,
            "abc123",
            "track/0083-Relaunch",
            Some(first.path()),
        );
        let conductor = r.path.join("conductor");
        std::fs::create_dir_all(&conductor).unwrap();
        std::fs::write(
            conductor.join("conductor.md"),
            format!(
                "| Track | Execution | Status | Summary |\n| --- | --- | --- | --- |\n| [0083-Example](0083-Example/spec.md) | `{}` | **Ready - not started** | s |\n",
                second.path().display()
            ),
        )
        .unwrap();
        r.execution_repo = Some(second.path().to_path_buf());
        let mut state = RunState::idle(&r.id);
        state.phase = crate::workflow::graph::PHASE_CI_WAIT.into();
        state.track_id = Some("0083".into());
        state.run_epoch = 1;
        crate::state::ensure_state_dir(&r).unwrap();
        crate::state::save_run_state(&r, &state).unwrap();
        assert!(matches!(
            decide_resume(&r, Some("0083")),
            ResumeChoice::Plan { .. }
        ));
    }

    #[test]
    fn head_sha_target_does_not_note_pr() {
        let f = matching_fixture("0083");
        let r = f.record();
        note_pr(r, "0083", 80, "abc123", "track/0083-Relaunch", None);
        let path = track_dir_of(f.root(), "0083").join("reuse.json");
        let before = std::fs::read(&path).unwrap();
        note_ci_target(
            r,
            "0083",
            &CiTarget::HeadSha {
                sha: "deadbeef".into(),
            },
            None,
        );
        note_ci_target(
            r,
            "0083",
            &CiTarget::PullRequest {
                number: 9,
                url: String::new(),
                is_draft: false,
                merged: false,
                head_oid: None,
                merge_state: MergeStateStatus::Unknown,
                head_ref: "track/0083-Relaunch".into(),
                title: String::new(),
            },
            None,
        );
        let after = std::fs::read(&path).unwrap();
        assert_eq!(before, after);
    }
}
