//! Content-addressed plan-review reuse (track 0083).
//!
//! A fresh `run` starts at `ci-wait` only when `spec.md`, `plan.md`, the
//! review-track skill, both slot bindings, and both review files still match
//! `reuse.json`, and `gh pr view` still shows that receipt PR on this track's
//! branch. The probe runs outside the run-state lock.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{ENV_COORDINATOR_AGY_BIN, ENV_COORDINATOR_OPENCODE_BIN, RoleBinding};
use crate::error::{CoordinatorError, Result};
use crate::registry::ProjectRecord;
use crate::state::RunStatus;

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
}

struct SharedHashes {
    spec: String,
    plan: String,
    skill: String,
}

enum LoadedReceipt {
    Missing,
    Unreadable,
    Ready(Receipt),
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

fn skill_file(root: &Path) -> PathBuf {
    root.join(".agents")
        .join("skills")
        .join("review-track")
        .join("SKILL.md")
}

/// Workspace skill, else `{conductor_dir.parent()}/.agents/skills/review-track/SKILL.md`.
pub fn review_track_skill_candidates(record: &ProjectRecord) -> (PathBuf, PathBuf) {
    let paths = crate::layout::resolve(record);
    let primary = skill_file(&paths.workspace_root);
    let fallback = match paths.conductor_dir.parent() {
        Some(parent) => skill_file(parent),
        None => primary.clone(),
    };
    (primary, fallback)
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
        Ok(receipt) if receipt.version == RECEIPT_VERSION => LoadedReceipt::Ready(receipt),
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
        LoadedReceipt::Ready(receipt) => receipt,
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
        return ResumeChoice::Probe {
            number: pr.number,
            head_sha: pr.head_sha.clone(),
            head_ref: pr.head_ref.clone(),
        };
    }
    ResumeChoice::Plan {
        archive_slugs,
        journal_unreadable: false,
    }
}

/// True when this slot's binding and track review bytes match the receipt.
pub fn slot_matches(record: &ProjectRecord, track_id: &str, slug: &str) -> bool {
    let Some(track_dir) = resolve_track_dir(record, track_id) else {
        return false;
    };
    let LoadedReceipt::Ready(receipt) = load_receipt(&track_dir) else {
        return false;
    };
    shared_match(record, &track_dir, &receipt) && slot_ok(&track_dir, slug, &receipt)
}

/// Replace `reuse.json` from the reviews this join actually produced.
///
/// Omits `pr` (ci-wait `note_pr` attaches it later). A write failure is ignored
/// so the join still succeeds; the next run re-reviews.
pub fn write_join_receipt(record: &ProjectRecord, track_id: &str) {
    let Some(track_dir) = resolve_track_dir(record, track_id) else {
        return;
    };
    let Some(hashes) = shared_hashes(record, &track_dir) else {
        return;
    };
    let mut slots = BTreeMap::new();
    for slug in review_slugs() {
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
    if let Some(pr) = receipt.pr.as_ref()
        && pr.number == number
        && pr.head_sha == head_sha
        && pr.head_ref == head_ref
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
    });
    let _ = write_receipt(&path, &receipt);
}

/// `persist_target` hook. HeadSha and a pull request with no head oid do not write.
pub fn note_ci_target(
    record: &ProjectRecord,
    track_id: &str,
    target: &crate::ci::backend::CiTarget,
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
    note_pr(record, track_id, *number, oid, head_ref);
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
pub fn probe_resume(record: &ProjectRecord, number: u64) -> Result<Option<ResumePr>> {
    #[cfg(test)]
    {
        let _ = record;
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
        let Some(cwd) = crate::worktree::product_git_cwd(record) else {
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

    fn matching_fixture(root: &Path, id: &str) -> ProjectRecord {
        let r = rec(root);
        let dir = track_dir_of(root, id);
        std::fs::write(dir.join("spec.md"), b"spec-v1").unwrap();
        std::fs::write(dir.join("plan.md"), b"plan-v1").unwrap();
        write_skill(root, b"skill-v1");
        std::fs::write(dir.join("agy-review.md"), b"agy body\n").unwrap();
        std::fs::write(dir.join("opencode-review.md"), b"oc body\n").unwrap();
        write_join_receipt(&r, id);
        r
    }

    #[test]
    fn byte_change_refuses_the_slot() {
        let dir = tempdir().unwrap();
        let r = matching_fixture(dir.path(), "0083");
        assert!(slot_matches(&r, "0083", REVIEW_SLUG_AGY));
        let review = track_dir_of(dir.path(), "0083").join("agy-review.md");
        std::fs::write(&review, b"agy body!\n").unwrap();
        assert!(!slot_matches(&r, "0083", REVIEW_SLUG_AGY));
        assert!(slot_matches(&r, "0083", REVIEW_SLUG_OPENCODE));
    }

    #[test]
    fn binding_or_skill_change_refuses_the_slot() {
        let _lock = test_env_lock();
        let prev = std::env::var_os(ENV_COORDINATOR_AGY_BIN);
        let dir = tempdir().unwrap();
        let r = matching_fixture(dir.path(), "0083");
        assert!(slot_matches(&r, "0083", REVIEW_SLUG_AGY));
        let skill = write_skill(dir.path(), b"skill-v1-changed");
        assert!(!slot_matches(&r, "0083", REVIEW_SLUG_AGY));
        std::fs::write(&skill, b"skill-v1").unwrap();
        assert!(slot_matches(&r, "0083", REVIEW_SLUG_AGY));
        let receipt = track_dir_of(dir.path(), "0083").join("reuse.json");
        let mut v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
        v["slots"]["agy"]["model"] = serde_json::json!("other-model");
        std::fs::write(&receipt, serde_json::to_vec(&v).unwrap()).unwrap();
        assert!(!slot_matches(&r, "0083", REVIEW_SLUG_AGY));
        v["slots"]["agy"]["model"] = serde_json::Value::Null;
        std::fs::write(&receipt, serde_json::to_vec(&v).unwrap()).unwrap();
        assert!(slot_matches(&r, "0083", REVIEW_SLUG_AGY));
        unsafe {
            std::env::set_var(ENV_COORDINATOR_AGY_BIN, r"C:\custom\agy.exe");
        }
        assert!(!slot_matches(&r, "0083", REVIEW_SLUG_AGY));
        unsafe {
            match &prev {
                Some(v) => std::env::set_var(ENV_COORDINATOR_AGY_BIN, v),
                None => std::env::remove_var(ENV_COORDINATOR_AGY_BIN),
            }
        }
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
        write_join_receipt(&r, "0083");
        assert!(slot_matches(&r, "0083", REVIEW_SLUG_AGY));
        assert!(slot_matches(&r, "0083", REVIEW_SLUG_OPENCODE));
    }

    #[test]
    fn note_pr_attaches_when_hashes_match() {
        let dir = tempdir().unwrap();
        let r = matching_fixture(dir.path(), "0083");
        note_pr(&r, "0083", 80, "abc123", "track/0083-Relaunch");
        let v: serde_json::Value = serde_json::from_slice(
            &std::fs::read(track_dir_of(dir.path(), "0083").join("reuse.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["pr"]["number"], 80);
        assert_eq!(v["pr"]["head_sha"], "abc123");
        assert_eq!(v["pr"]["head_ref"], "track/0083-Relaunch");
        assert!(v["slots"]["agy"]["review_sha256"].is_string());
    }

    #[test]
    fn note_pr_strips_pr_when_inputs_mismatch() {
        let dir = tempdir().unwrap();
        let r = matching_fixture(dir.path(), "0083");
        note_pr(&r, "0083", 80, "abc123", "track/0083-Relaunch");
        std::fs::write(track_dir_of(dir.path(), "0083").join("spec.md"), b"spec-v2").unwrap();
        note_pr(&r, "0083", 81, "def456", "track/0083-Relaunch");
        let v: serde_json::Value = serde_json::from_slice(
            &std::fs::read(track_dir_of(dir.path(), "0083").join("reuse.json")).unwrap(),
        )
        .unwrap();
        assert!(v.get("pr").is_none() || v["pr"].is_null());
        assert!(v["slots"]["agy"]["review_sha256"].is_string());
        assert!(v["slots"]["opencode"]["review_sha256"].is_string());
    }

    #[test]
    fn note_pr_no_op_when_pr_already_matching() {
        let dir = tempdir().unwrap();
        let r = matching_fixture(dir.path(), "0083");
        note_pr(&r, "0083", 80, "abc123", "track/0083-Relaunch");
        let path = track_dir_of(dir.path(), "0083").join("reuse.json");
        let before = std::fs::read(&path).unwrap();
        std::fs::write(
            track_dir_of(dir.path(), "0083").join("spec.md"),
            b"spec-later",
        )
        .unwrap();
        note_pr(&r, "0083", 80, "abc123", "track/0083-Relaunch");
        let after = std::fs::read(&path).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn head_sha_target_does_not_note_pr() {
        let dir = tempdir().unwrap();
        let r = matching_fixture(dir.path(), "0083");
        note_pr(&r, "0083", 80, "abc123", "track/0083-Relaunch");
        let path = track_dir_of(dir.path(), "0083").join("reuse.json");
        let before = std::fs::read(&path).unwrap();
        note_ci_target(
            &r,
            "0083",
            &CiTarget::HeadSha {
                sha: "deadbeef".into(),
            },
        );
        note_ci_target(
            &r,
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
        );
        let after = std::fs::read(&path).unwrap();
        assert_eq!(before, after);
    }
}
