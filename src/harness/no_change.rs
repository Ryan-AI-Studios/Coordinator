//! Unchanged-tree guard for adapter implement legs (track 0095; target provenance 0099).
//!
//! An `implement` / `address-findings` turn that finishes without touching the tree is a dead leg:
//! the adapter cannot know it did nothing, so it reports success and the run would advance (and
//! later fail at `ci-wait`, see 0094). The leg baseline is sampled from the resolved track
//! execution target before the first turn of the leg and compared after each turn, inside the
//! pool (before `apply_turn` builds a success outcome) because the detached holder applies the
//! outcome itself.
//!
//! Detection is fail-open on purpose: if either git sample cannot be read the guard does not fire,
//! and 0094's `ci-wait` empty-leg failure remains the backstop. Resolution errors are different:
//! they fail closed before inject (see `workflow::execution_repo`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::registry::ProjectRecord;
use crate::state::{RunState, resolve_state_dir};
use crate::workflow::WorkflowDriver;
use crate::workflow::execution_repo::{ResolutionSource, ResolvedExecution};
use crate::workflow::graph::{PHASE_ADDRESS_FINDINGS, PHASE_IMPLEMENT};

/// Stable prefix of the failure message (`failure show` / tests key on it).
pub const NO_CHANGE_PREFIX: &str = "implement leg produced no changes";

const BASELINE_FILE: &str = "leg-baseline.json";
const BASELINE_VERSION: u32 = 1;

/// What the tree looked like at one instant. `diff` hashes `git diff HEAD` so a further edit to a
/// file that was already dirty still counts as a change. Branch is provenance, not equality.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeSample {
    pub head: String,
    pub porcelain: BTreeSet<String>,
    pub diff: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Baseline {
    #[serde(default)]
    version: u32,
    phase: String,
    epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    track_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    repo: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    declared_repo: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<ResolutionSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    sample: TreeSample,
}

/// Opaque per-injection trust token. Disk content alone can never authorize a verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureTrust {
    Trusted {
        project_id: String,
        epoch: u64,
        phase: String,
        track_id: String,
        declared_repo: PathBuf,
        repo: PathBuf,
        fingerprint: String,
    },
    Untrusted {
        reason: String,
    },
}

impl CaptureTrust {
    pub fn is_trusted(&self) -> bool {
        matches!(self, Self::Trusted { .. })
    }

    fn untrusted(reason: impl Into<String>) -> Self {
        Self::Untrusted {
            reason: reason.into(),
        }
    }
}

/// Result of pre-turn capture, including the token the verdict must present.
#[derive(Debug, Clone)]
pub enum CaptureStatus {
    Captured(CaptureTrust),
    Reused(CaptureTrust),
    Skipped(String),
    UntrustedTargetChange,
}

impl CaptureStatus {
    pub fn trust(&self) -> CaptureTrust {
        match self {
            Self::Captured(t) | Self::Reused(t) => t.clone(),
            Self::Skipped(reason) => CaptureTrust::untrusted(reason.clone()),
            Self::UntrustedTargetChange => CaptureTrust::untrusted("target-changed"),
        }
    }

    pub fn reason(&self) -> &str {
        match self {
            Self::Captured(_) => "captured",
            Self::Reused(_) => "reused",
            Self::Skipped(reason) => reason,
            Self::UntrustedTargetChange => "target-changed",
        }
    }
}

/// Verdict evaluation, split so the pool can journal a skip without inventing a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerdictOutcome {
    /// Trusted, readable, unchanged target.
    NoChange(String),
    /// Trusted and unchanged, but the session cwd is not the prompt target.
    CwdMisdirected,
    /// Trusted and unchanged, but the run identity moved during the turn.
    StaleInjection,
    /// The target changed or can no longer be resolved.
    TargetChanged(String),
    /// Nothing to compare (legacy/corrupt/untrusted/read failure) or a real change.
    Clean,
}

/// Only adapter-driven `implement` / `address-findings` legs are checked.
pub fn applies(state: &RunState, phase: &str) -> bool {
    state.driver == WorkflowDriver::Adapter
        && (phase == PHASE_IMPLEMENT || phase == PHASE_ADDRESS_FINDINGS)
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = crate::git_command::command()
        .args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `None` when `cwd` is not a readable git work tree.
pub fn sample(cwd: &Path) -> Option<TreeSample> {
    let head = git(cwd, &["rev-parse", "HEAD"])?.trim().to_string();
    if head.is_empty() {
        return None;
    }
    let porcelain = git(cwd, &["status", "--porcelain"])?
        .lines()
        .map(|l| l.trim_end().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let diff = git(cwd, &["diff", "HEAD"])?;
    let mut h = Sha256::new();
    h.update(diff.as_bytes());
    let diff = h
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    Some(TreeSample {
        head,
        porcelain,
        diff,
    })
}

fn branch(cwd: &Path) -> Option<String> {
    let branch = git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?
        .trim()
        .to_string();
    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch)
    }
}

fn baseline_path(record: &ProjectRecord) -> Option<PathBuf> {
    resolve_state_dir(record)
        .ok()
        .map(|d| d.join(BASELINE_FILE))
}

fn load_baseline_bytes(record: &ProjectRecord) -> Option<(Vec<u8>, Baseline)> {
    let bytes = std::fs::read(baseline_path(record)?).ok()?;
    let baseline = serde_json::from_slice(&bytes).ok()?;
    Some((bytes, baseline))
}

fn fingerprint(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn paths_same(a: &Path, b: &Path) -> bool {
    crate::workflow::execution_repo::paths_same(a, b)
}

fn same_target(baseline: &Baseline, target: &ResolvedExecution) -> bool {
    baseline
        .repo
        .as_deref()
        .is_some_and(|repo| paths_same(repo, &target.sample_path))
        && baseline
            .declared_repo
            .as_deref()
            .is_some_and(|repo| paths_same(repo, &target.declared_path))
}

fn same_leg(
    baseline: &Baseline,
    state: &RunState,
    phase: &str,
    target: &ResolvedExecution,
) -> bool {
    baseline.version == BASELINE_VERSION
        && baseline.phase == phase
        && baseline.epoch == state.run_epoch
        && baseline.track_id.as_deref() == Some(target.track_id.as_str())
}

fn trust_for(bytes: &[u8], target: &ResolvedExecution) -> CaptureTrust {
    CaptureTrust::Trusted {
        project_id: target.project_id.clone(),
        epoch: target.run_epoch,
        phase: target.phase.clone(),
        track_id: target.track_id.clone(),
        declared_repo: target.declared_path.clone(),
        repo: target.sample_path.clone(),
        fingerprint: fingerprint(bytes),
    }
}

fn write_baseline(path: &Path, baseline: &Baseline) -> Result<Vec<u8>, String> {
    let bytes = serde_json::to_vec(baseline).map_err(|e| format!("serialize baseline: {e}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create baseline dir: {e}"))?;
    }
    crate::persist::atomic_write(path, &bytes).map_err(|e| format!("write baseline: {e}"))?;
    Ok(bytes)
}

/// Record (or reuse) the leg baseline before a turn and return the injection trust token.
pub fn note_baseline(
    record: &ProjectRecord,
    state: &RunState,
    phase: &str,
    target: &ResolvedExecution,
) -> CaptureStatus {
    if !applies(state, phase) {
        return CaptureStatus::Skipped("phase-not-applicable".into());
    }
    if target.planning_only {
        return CaptureStatus::Skipped("planning-only".into());
    }
    let Some(path) = baseline_path(record) else {
        return CaptureStatus::Skipped("capture-write-failed".into());
    };

    if let Some((bytes, baseline)) = load_baseline_bytes(record)
        && same_leg(&baseline, state, phase, target)
    {
        {
            if same_target(&baseline, target) {
                // Source-only provenance changes keep the first sample.
                if baseline.source != Some(target.source) {
                    let updated = Baseline {
                        source: Some(target.source),
                        ..baseline
                    };
                    match write_baseline(&path, &updated) {
                        Ok(bytes) => return CaptureStatus::Reused(trust_for(&bytes, target)),
                        Err(_) => {
                            let _ = std::fs::remove_file(&path);
                            return CaptureStatus::Skipped("capture-write-failed".into());
                        }
                    }
                }
                return CaptureStatus::Reused(trust_for(&bytes, target));
            }
            // Same leg but the actual target moved: keep the original sample, skip this leg.
            return CaptureStatus::UntrustedTargetChange;
        }
    }

    let Some(sample) = sample(&target.sample_path) else {
        let _ = std::fs::remove_file(&path);
        return CaptureStatus::Skipped("git-unreadable".into());
    };
    let baseline = Baseline {
        version: BASELINE_VERSION,
        phase: phase.to_string(),
        epoch: state.run_epoch,
        track_id: Some(target.track_id.clone()),
        repo: Some(target.sample_path.clone()),
        declared_repo: Some(target.declared_path.clone()),
        source: Some(target.source),
        branch: branch(&target.sample_path),
        sample,
    };
    match write_baseline(&path, &baseline) {
        Ok(bytes) => CaptureStatus::Captured(trust_for(&bytes, target)),
        Err(_) => {
            let _ = std::fs::remove_file(&path);
            CaptureStatus::Skipped("capture-write-failed".into())
        }
    }
}

/// Evaluate the turn against the frozen target and the capture token.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_turn(
    record: &ProjectRecord,
    state: &RunState,
    phase: &str,
    target: &ResolvedExecution,
    trust: &CaptureTrust,
    harness: &str,
    last_signal: Option<&str>,
    session_cwd: &Path,
) -> VerdictOutcome {
    if !applies(state, phase) || target.planning_only {
        return VerdictOutcome::Clean;
    }
    if target.run_epoch != state.run_epoch
        || target.phase != phase
        || target.project_id != state.project_id
        || state.track_id.as_deref() != Some(target.track_id.as_str())
    {
        return VerdictOutcome::StaleInjection;
    }
    // Identity is rechecked against the *injected* state the caller passes; a newer
    // run epoch/phase/track never gets a verdict from this turn.
    if trust_binding_mismatch(trust, state, phase, target) {
        return VerdictOutcome::StaleInjection;
    }
    let CaptureTrust::Trusted { fingerprint, .. } = trust else {
        return VerdictOutcome::Clean;
    };
    let Some((bytes, baseline)) = load_baseline_bytes(record) else {
        return VerdictOutcome::Clean;
    };
    if &crate::harness::no_change::fingerprint(&bytes) != fingerprint {
        return VerdictOutcome::Clean;
    }
    if !same_leg(&baseline, state, phase, target) {
        return VerdictOutcome::Clean;
    }
    if !same_target(&baseline, target) {
        return VerdictOutcome::TargetChanged("target-changed".into());
    }
    let Some(now) = sample(&target.sample_path) else {
        return VerdictOutcome::Clean;
    };
    if now != baseline.sample {
        return VerdictOutcome::Clean;
    }
    if !paths_same(session_cwd, &target.prompt_path) {
        return VerdictOutcome::CwdMisdirected;
    }
    VerdictOutcome::NoChange(no_change_message_for(
        target,
        harness,
        last_signal,
        session_cwd,
    ))
}

fn trust_binding_mismatch(
    trust: &CaptureTrust,
    state: &RunState,
    phase: &str,
    target: &ResolvedExecution,
) -> bool {
    match trust {
        CaptureTrust::Trusted {
            project_id,
            epoch,
            phase: trusted_phase,
            track_id,
            declared_repo,
            repo,
            ..
        } => {
            project_id != &state.project_id
                || *epoch != state.run_epoch
                || trusted_phase != phase
                || track_id != &target.track_id
                || !paths_same(declared_repo, &target.declared_path)
                || !paths_same(repo, &target.sample_path)
        }
        CaptureTrust::Untrusted { .. } => true,
    }
}

/// `Some(message)` when the turn left the frozen target exactly as the leg found it.
#[allow(clippy::too_many_arguments)]
pub fn verdict_after_turn(
    record: &ProjectRecord,
    state: &RunState,
    phase: &str,
    target: &ResolvedExecution,
    trust: &CaptureTrust,
    harness: &str,
    last_signal: Option<&str>,
    session_cwd: &Path,
) -> Option<String> {
    match evaluate_turn(
        record,
        state,
        phase,
        target,
        trust,
        harness,
        last_signal,
        session_cwd,
    ) {
        VerdictOutcome::NoChange(message) => Some(message),
        _ => None,
    }
}

pub fn no_change_message(phase: &str, harness: &str, last_signal: Option<&str>) -> String {
    format!(
        "{NO_CHANGE_PREFIX}: phase {phase}, harness {harness}, session mode adapter-default, \
         last permission signal {}; the tree is exactly as the leg found it. If the plan was \
         already satisfied, advance by hand; otherwise the adapter may be sandboxed read-only.",
        last_signal.unwrap_or("none")
    )
}

/// Deterministic order: prefix, track, repo, source, cwd when different, harness,
/// mode, signal. The prefix/track/repo head stays inside the 200-character
/// `last_event` cap; the bounded artifact keeps the complete line.
pub fn no_change_message_for(
    target: &ResolvedExecution,
    harness: &str,
    last_signal: Option<&str>,
    session_cwd: &Path,
) -> String {
    let mut fields = format!(
        "{NO_CHANGE_PREFIX}: track={}, repo={}, source={}",
        target.track_id,
        target.sample_path.display(),
        target.source
    );
    if !paths_same(session_cwd, &target.prompt_path) {
        fields.push_str(&format!(", cwd={}", session_cwd.display()));
    }
    fields.push_str(&format!(
        ", harness={harness}, mode=adapter-default, signal={}; the sampled target is exactly as the leg found it.",
        last_signal.unwrap_or("none")
    ));
    fields
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::LayoutProfile;
    use crate::state::RunState;
    use chrono::Utc;
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    fn git_ok(cwd: &Path, args: &[&str]) {
        let out = crate::git_command::command()
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
    }

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        git_ok(d.path(), &["init", "-b", "main"]);
        std::fs::write(d.path().join("f.txt"), "base\n").unwrap();
        git_ok(d.path(), &["add", "f.txt"]);
        git_ok(d.path(), &["commit", "-m", "base"]);
        d
    }

    fn record(path: &Path) -> ProjectRecord {
        // Keep the baseline file outside the sampled tree: putting it under the repo
        // would add an untracked `.coordinator/` entry and make every legacy fixture
        // look changed. Production state normally lives at the workspace root.
        let state_dir = path
            .parent()
            .unwrap_or(path)
            .join(format!("state-{}", uuid::Uuid::new_v4()));
        ProjectRecord {
            id: "p".into(),
            path: path.to_path_buf(),
            display_name: None,
            layout_profile: LayoutProfile::SingleRoot,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: BTreeMap::new(),
            state_dir: Some(state_dir),
            auto_merge: true,
            phase_timeouts_secs: BTreeMap::new(),
            notify_progress: false,
            worktree_isolation: false,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            ci_fix_routing: false,
            review_comment_routing: false,
            skill_aliases: BTreeMap::new(),
            created_at: Utc::now(),
        }
    }

    fn running() -> RunState {
        let mut s = RunState::idle("p");
        s.status = crate::state::RunStatus::Running;
        s.driver = WorkflowDriver::Adapter;
        s.phase = PHASE_IMPLEMENT.into();
        s.track_id = Some("0099".into());
        s.run_epoch = 3;
        s
    }

    fn target(repo: &Path, cwd: &Path) -> ResolvedExecution {
        ResolvedExecution {
            project_id: "p".into(),
            run_epoch: 3,
            phase: PHASE_IMPLEMENT.into(),
            track_id: "0099".into(),
            declared_path: repo.to_path_buf(),
            prompt_path: cwd.to_path_buf(),
            sample_path: repo.to_path_buf(),
            source: ResolutionSource::RowExecution,
            planning_only: false,
            late_metadata: Vec::new(),
        }
    }

    #[test]
    fn sample_is_stable_when_nothing_changes() {
        let d = repo();
        assert_eq!(sample(d.path()), sample(d.path()));
        assert!(sample(d.path()).is_some());
    }

    #[test]
    fn sample_changes_on_new_file_commit_and_reedit_of_a_dirty_file() {
        let d = repo();
        let s0 = sample(d.path()).unwrap();
        std::fs::write(d.path().join("new.txt"), "x\n").unwrap();
        let s1 = sample(d.path()).unwrap();
        assert_ne!(s0, s1);
        std::fs::write(d.path().join("f.txt"), "one\n").unwrap();
        let s2 = sample(d.path()).unwrap();
        std::fs::write(d.path().join("f.txt"), "two\n").unwrap();
        let s3 = sample(d.path()).unwrap();
        assert_ne!(s2, s3, "re-editing an already-dirty file must register");
        git_ok(d.path(), &["add", "-A"]);
        git_ok(d.path(), &["commit", "-m", "work"]);
        assert_ne!(s3, sample(d.path()).unwrap());
    }

    #[test]
    fn a_pre_existing_dirty_file_is_not_a_change() {
        let d = repo();
        std::fs::write(d.path().join("f.txt"), "dirty\n").unwrap();
        let before = sample(d.path()).unwrap();
        assert_eq!(before, sample(d.path()).unwrap());
    }

    #[test]
    fn unreadable_tree_yields_no_sample() {
        let d = tempfile::tempdir().unwrap();
        assert!(sample(d.path()).is_none());
        assert!(sample(&d.path().join("missing")).is_none());
    }

    #[test]
    fn applies_only_to_adapter_implement_and_address_findings() {
        let mut s = RunState::idle("p");
        s.driver = WorkflowDriver::Adapter;
        assert!(applies(&s, PHASE_IMPLEMENT));
        assert!(applies(&s, PHASE_ADDRESS_FINDINGS));
        assert!(!applies(&s, "fold"));
        s.driver = WorkflowDriver::Stub;
        assert!(!applies(&s, PHASE_IMPLEMENT));
    }

    #[test]
    fn commit_clean_tree_is_change_in_track_repo() {
        let d = repo();
        let rec = record(d.path());
        let state = running();
        let t = target(d.path(), d.path());
        let status = note_baseline(&rec, &state, PHASE_IMPLEMENT, &t);
        assert!(matches!(status, CaptureStatus::Captured(_)));
        std::fs::write(d.path().join("work.txt"), "delivered\n").unwrap();
        git_ok(d.path(), &["add", "-A"]);
        git_ok(d.path(), &["commit", "-m", "track 0551 delivery"]);
        assert!(
            verdict_after_turn(
                &rec,
                &state,
                PHASE_IMPLEMENT,
                &t,
                &status.trust(),
                "codex",
                None,
                d.path(),
            )
            .is_none()
        );
    }

    #[test]
    fn different_repo_change_is_still_noop() {
        let track = repo();
        let other = repo();
        let rec = record(track.path());
        let state = running();
        let t = target(track.path(), track.path());
        let status = note_baseline(&rec, &state, PHASE_IMPLEMENT, &t);
        std::fs::write(other.path().join("elsewhere.txt"), "x\n").unwrap();
        git_ok(other.path(), &["add", "-A"]);
        git_ok(other.path(), &["commit", "-m", "other"]);
        assert!(
            verdict_after_turn(
                &rec,
                &state,
                PHASE_IMPLEMENT,
                &t,
                &status.trust(),
                "codex",
                None,
                track.path(),
            )
            .is_some()
        );
    }

    #[test]
    fn branch_only_is_noop() {
        let d = repo();
        let rec = record(d.path());
        let state = running();
        let t = target(d.path(), d.path());
        let status = note_baseline(&rec, &state, PHASE_IMPLEMENT, &t);
        git_ok(d.path(), &["checkout", "-b", "track/0551"]);
        assert!(
            verdict_after_turn(
                &rec,
                &state,
                PHASE_IMPLEMENT,
                &t,
                &status.trust(),
                "codex",
                None,
                d.path(),
            )
            .is_some()
        );
    }

    #[test]
    fn legacy_matching_epoch_overwritten_before_inject() {
        let d = repo();
        let rec = record(d.path());
        let path = baseline_path(&rec).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"phase":"implement","epoch":3,"sample":{"head":"old","porcelain":[],"diff":""}}"#,
        )
        .unwrap();
        let state = running();
        let t = target(d.path(), d.path());
        let status = note_baseline(&rec, &state, PHASE_IMPLEMENT, &t);
        assert!(matches!(status, CaptureStatus::Captured(_)));
        let (_, b) = load_baseline_bytes(&rec).unwrap();
        assert_eq!(b.version, BASELINE_VERSION);
        assert_eq!(b.track_id.as_deref(), Some("0099"));
    }

    #[test]
    fn verdict_legacy_never_rewrites() {
        let d = repo();
        let rec = record(d.path());
        let path = baseline_path(&rec).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"phase":"implement","epoch":3,"sample":{"head":"old","porcelain":[],"diff":""}}"#,
        )
        .unwrap();
        let state = running();
        let t = target(d.path(), d.path());
        let trust = CaptureTrust::untrusted("capture-write-failed");
        let before = std::fs::read(&path).unwrap();
        assert!(
            verdict_after_turn(
                &rec,
                &state,
                PHASE_IMPLEMENT,
                &t,
                &trust,
                "codex",
                None,
                d.path(),
            )
            .is_none()
        );
        let after = std::fs::read(&path).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn wrong_track_repo_phase_epoch_or_version_untrusted() {
        let d = repo();
        let rec = record(d.path());
        let state = running();
        let t = target(d.path(), d.path());
        let status = note_baseline(&rec, &state, PHASE_IMPLEMENT, &t);
        let mut trust = status.trust();
        if let CaptureTrust::Trusted { epoch, .. } = &mut trust {
            *epoch = 99;
        }
        assert!(
            verdict_after_turn(
                &rec,
                &state,
                PHASE_IMPLEMENT,
                &t,
                &trust,
                "codex",
                None,
                d.path(),
            )
            .is_none()
        );
    }

    #[test]
    fn continuation_keeps_first_sample() {
        let d = repo();
        let rec = record(d.path());
        let state = running();
        let t = target(d.path(), d.path());
        let first = note_baseline(&rec, &state, PHASE_IMPLEMENT, &t);
        let (_, before) = load_baseline_bytes(&rec).unwrap();
        std::fs::write(d.path().join("later.txt"), "later\n").unwrap();
        let second = note_baseline(&rec, &state, PHASE_IMPLEMENT, &t);
        assert!(matches!(second, CaptureStatus::Reused(_)));
        let (_, after) = load_baseline_bytes(&rec).unwrap();
        assert_eq!(before.sample, after.sample);
        assert!(first.trust().is_trusted());
    }

    #[test]
    fn continuation_revalidates_first_sample_trust() {
        let d = repo();
        let rec = record(d.path());
        let state = running();
        let t = target(d.path(), d.path());
        let first = note_baseline(&rec, &state, PHASE_IMPLEMENT, &t);
        assert!(first.trust().is_trusted());
        std::fs::write(d.path().join("delivered.txt"), "x\n").unwrap();
        git_ok(d.path(), &["add", "-A"]);
        git_ok(d.path(), &["commit", "-m", "delivery"]);
        let second = note_baseline(&rec, &state, PHASE_IMPLEMENT, &t);
        let CaptureStatus::Reused(trust) = &second else {
            panic!("continuation must reuse the first sample: {second:?}");
        };
        assert!(trust.is_trusted());
        // The revalidated token compares against the first sample, so the commit is
        // still a change rather than a verdict.
        assert!(
            verdict_after_turn(
                &rec,
                &state,
                PHASE_IMPLEMENT,
                &t,
                trust,
                "codex",
                None,
                d.path(),
            )
            .is_none()
        );
    }

    #[test]
    fn untrusted_capture_cannot_borrow_prior_token() {
        let d = repo();
        let rec = record(d.path());
        let state = running();
        let t = target(d.path(), d.path());
        let first = note_baseline(&rec, &state, PHASE_IMPLEMENT, &t);
        assert!(first.trust().is_trusted());
        let untrusted = CaptureTrust::untrusted("capture-write-failed");
        assert!(
            verdict_after_turn(
                &rec,
                &state,
                PHASE_IMPLEMENT,
                &t,
                &untrusted,
                "codex",
                None,
                d.path(),
            )
            .is_none()
        );
    }

    #[test]
    fn verdict_requires_matching_capture_trust() {
        let d = repo();
        let rec = record(d.path());
        let state = running();
        let t = target(d.path(), d.path());
        let status = note_baseline(&rec, &state, PHASE_IMPLEMENT, &t);
        let mut other = t.clone();
        other.run_epoch = 99;
        assert!(
            verdict_after_turn(
                &rec,
                &state,
                PHASE_IMPLEMENT,
                &other,
                &status.trust(),
                "codex",
                None,
                d.path(),
            )
            .is_none()
        );
    }

    #[test]
    fn source_only_change_retains_first_sample() {
        let d = repo();
        let rec = record(d.path());
        let state = running();
        let t = target(d.path(), d.path());
        let first = note_baseline(&rec, &state, PHASE_IMPLEMENT, &t);
        let (_, before) = load_baseline_bytes(&rec).unwrap();
        let mut changed = t.clone();
        changed.source = ResolutionSource::SpecMetadata;
        let second = note_baseline(&rec, &state, PHASE_IMPLEMENT, &changed);
        assert!(matches!(second, CaptureStatus::Reused(_)));
        let (_, after) = load_baseline_bytes(&rec).unwrap();
        assert_eq!(before.sample, after.sample);
        assert_eq!(after.source, Some(ResolutionSource::SpecMetadata));
        assert!(first.trust().is_trusted());
    }

    #[test]
    fn midleg_target_change_does_not_recapture_delivered_work() {
        let d = repo();
        let other = repo();
        let rec = record(d.path());
        let state = running();
        let t = target(d.path(), d.path());
        let status = note_baseline(&rec, &state, PHASE_IMPLEMENT, &t);
        std::fs::write(d.path().join("delivered.txt"), "x\n").unwrap();
        git_ok(d.path(), &["add", "-A"]);
        git_ok(d.path(), &["commit", "-m", "delivered"]);
        let mut moved = t.clone();
        moved.sample_path = other.path().to_path_buf();
        moved.declared_path = other.path().to_path_buf();
        let second = note_baseline(&rec, &state, PHASE_IMPLEMENT, &moved);
        assert!(matches!(second, CaptureStatus::UntrustedTargetChange));
        assert!(
            verdict_after_turn(
                &rec,
                &state,
                PHASE_IMPLEMENT,
                &moved,
                &status.trust(),
                "codex",
                None,
                d.path(),
            )
            .is_none()
        );
    }

    #[test]
    fn verdict_resolution_failure_skips() {
        let d = repo();
        let rec = record(d.path());
        let state = running();
        let t = target(d.path(), d.path());
        let status = note_baseline(&rec, &state, PHASE_IMPLEMENT, &t);
        let mut moved = t.clone();
        moved.sample_path = d.path().join("deleted");
        assert!(
            verdict_after_turn(
                &rec,
                &state,
                PHASE_IMPLEMENT,
                &moved,
                &status.trust(),
                "codex",
                None,
                d.path(),
            )
            .is_none()
        );
    }

    #[test]
    fn cwd_misdirected_unchanged_target_skips_verdict() {
        let d = repo();
        let cwd = tempdir().unwrap();
        let rec = record(d.path());
        let state = running();
        let t = target(d.path(), d.path());
        let status = note_baseline(&rec, &state, PHASE_IMPLEMENT, &t);
        assert_eq!(
            evaluate_turn(
                &rec,
                &state,
                PHASE_IMPLEMENT,
                &t,
                &status.trust(),
                "codex",
                None,
                cwd.path(),
            ),
            VerdictOutcome::CwdMisdirected
        );
    }

    #[test]
    fn failed_baseline_write_never_trusts_stale_disk() {
        let d = repo();
        let rec = record(d.path());
        let state = running();
        let t = target(d.path(), d.path());
        let first = note_baseline(&rec, &state, PHASE_IMPLEMENT, &t);
        assert!(first.trust().is_trusted());
        // A stale same-identity-shaped file plus a capture that cannot sample must
        // leave the injection untrusted and repair/remove the stale file.
        let path = baseline_path(&rec).unwrap();
        let mut stale: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        stale["track_id"] = serde_json::json!("9999");
        std::fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();
        std::fs::remove_dir_all(d.path().join(".git")).unwrap();
        let second = note_baseline(&rec, &state, PHASE_IMPLEMENT, &t);
        assert!(matches!(second, CaptureStatus::Skipped(_)));
        assert!(!second.trust().is_trusted());
        assert!(!baseline_path(&rec).unwrap().exists());
    }

    #[test]
    fn corrupt_baseline_fail_open() {
        let d = repo();
        let rec = record(d.path());
        let path = baseline_path(&rec).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not json").unwrap();
        let state = running();
        let t = target(d.path(), d.path());
        let status = note_baseline(&rec, &state, PHASE_IMPLEMENT, &t);
        assert!(matches!(status, CaptureStatus::Captured(_)));
    }

    #[test]
    fn nongit_unborn_and_git_io_fail_open() {
        let d = tempfile::tempdir().unwrap();
        let rec = record(d.path());
        let state = running();
        let t = target(d.path(), d.path());
        let status = note_baseline(&rec, &state, PHASE_IMPLEMENT, &t);
        assert!(matches!(status, CaptureStatus::Skipped(_)));
        assert!(
            verdict_after_turn(
                &rec,
                &state,
                PHASE_IMPLEMENT,
                &t,
                &status.trust(),
                "codex",
                None,
                d.path(),
            )
            .is_none()
        );
    }

    #[test]
    fn none_to_some_after_init_never_crashes() {
        let d = tempfile::tempdir().unwrap();
        let rec = record(d.path());
        let state = running();
        let t = target(d.path(), d.path());
        let none = note_baseline(&rec, &state, PHASE_IMPLEMENT, &t);
        assert!(!none.trust().is_trusted());
        git_ok(d.path(), &["init", "-b", "main"]);
        std::fs::write(d.path().join("f.txt"), "x\n").unwrap();
        git_ok(d.path(), &["add", "f.txt"]);
        git_ok(d.path(), &["commit", "-m", "base"]);
        let some = note_baseline(&rec, &state, PHASE_IMPLEMENT, &t);
        assert!(matches!(some, CaptureStatus::Captured(_)));
        assert!(
            verdict_after_turn(
                &rec,
                &state,
                PHASE_IMPLEMENT,
                &t,
                &some.trust(),
                "codex",
                None,
                d.path(),
            )
            .is_some()
        );
    }

    #[test]
    fn hook_env_cannot_redirect_sampling() {
        let _lock = crate::config::test_env_lock();
        let foreign = repo();
        let fixture = repo();
        std::fs::write(foreign.path().join("foreign.txt"), "foreign\n").unwrap();
        git_ok(foreign.path(), &["add", "foreign.txt"]);
        git_ok(foreign.path(), &["commit", "-m", "foreign-only"]);
        struct Restore(&'static str, Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe {
                    match self.1.take() {
                        Some(v) => std::env::set_var(self.0, v),
                        None => std::env::remove_var(self.0),
                    }
                }
            }
        }
        let _git_dir = Restore("GIT_DIR", std::env::var_os("GIT_DIR"));
        let _work_tree = Restore("GIT_WORK_TREE", std::env::var_os("GIT_WORK_TREE"));
        unsafe {
            std::env::set_var("GIT_DIR", foreign.path().join(".git"));
            std::env::set_var("GIT_WORK_TREE", foreign.path());
        }
        let got = sample(fixture.path()).expect("sampler must clear hook env");
        assert_eq!(got, sample(fixture.path()).unwrap());
        assert_ne!(
            got.head,
            sample(foreign.path()).unwrap().head,
            "must not sample the hook's foreign repository"
        );
    }

    #[test]
    fn baseline_changed_after_capture_is_untrusted() {
        let d = repo();
        let rec = record(d.path());
        let state = running();
        let t = target(d.path(), d.path());
        let status = note_baseline(&rec, &state, PHASE_IMPLEMENT, &t);
        let path = baseline_path(&rec).unwrap();
        let mut b: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        b["sample"]["head"] = serde_json::json!("tampered");
        std::fs::write(&path, serde_json::to_vec(&b).unwrap()).unwrap();
        assert!(
            verdict_after_turn(
                &rec,
                &state,
                PHASE_IMPLEMENT,
                &t,
                &status.trust(),
                "codex",
                None,
                d.path(),
            )
            .is_none()
        );
    }

    #[test]
    fn untracked_content_rewrite_and_collapsed_directory_are_known_noops() {
        // Characterizes the retained limitation: the predicate hashes HEAD/porcelain/
        // diff only, so rewriting an already-untracked file is invisible.
        let d = repo();
        let rec = record(d.path());
        let state = running();
        let t = target(d.path(), d.path());
        std::fs::write(d.path().join("untracked.txt"), "one\n").unwrap();
        let status = note_baseline(&rec, &state, PHASE_IMPLEMENT, &t);
        std::fs::write(d.path().join("untracked.txt"), "two\n").unwrap();
        assert!(
            verdict_after_turn(
                &rec,
                &state,
                PHASE_IMPLEMENT,
                &t,
                &status.trust(),
                "codex",
                None,
                d.path(),
            )
            .is_some()
        );

        // A directory porcelain collapses to `dir/` behaves the same.
        std::fs::create_dir_all(d.path().join("dir")).unwrap();
        std::fs::write(d.path().join("dir/a.txt"), "a\n").unwrap();
        let mut next = running();
        next.run_epoch = 7;
        let mut t7 = t.clone();
        t7.run_epoch = 7;
        let status = note_baseline(&rec, &next, PHASE_IMPLEMENT, &t7);
        std::fs::write(d.path().join("dir/a.txt"), "b\n").unwrap();
        assert!(
            verdict_after_turn(
                &rec,
                &next,
                PHASE_IMPLEMENT,
                &t7,
                &status.trust(),
                "codex",
                None,
                d.path(),
            )
            .is_some()
        );
    }

    #[test]
    fn epoch357_two_repo_timeline() {
        // The historical epoch 357 / track 0551 shape, made hermetic: `a` stands in
        // for the harness repo (label c1aa5c38) and `b` for the track repo
        // (labels e450ce4 -> 5d8bc93). All commits are generated temp SHAs.
        let a = repo();
        let b = repo();
        let rec = record(b.path());
        let state = running();
        let t = target(b.path(), b.path());

        // A legacy pre-turn capture becomes B, not the harness repo.
        let captured = note_baseline(&rec, &state, PHASE_IMPLEMENT, &t);
        assert!(matches!(captured, CaptureStatus::Captured(_)));
        let (_, baseline) = load_baseline_bytes(&rec).unwrap();
        assert_eq!(baseline.repo.as_deref(), Some(b.path()));

        // The leg commits clean work in B: this is delivery, not a no-op.
        std::fs::write(b.path().join("bench.txt"), "2,446 insertions\n").unwrap();
        git_ok(b.path(), &["add", "-A"]);
        git_ok(
            b.path(),
            &["commit", "-m", "track 0551: qualify benchmark oracles"],
        );
        assert!(
            verdict_after_turn(
                &rec,
                &state,
                PHASE_IMPLEMENT,
                &t,
                &captured.trust(),
                "codex",
                None,
                b.path(),
            )
            .is_none()
        );

        // True no-op control on the same format.
        let mut next = running();
        next.run_epoch = 4;
        let mut t4 = t.clone();
        t4.run_epoch = 4;
        let noop = note_baseline(&rec, &next, PHASE_IMPLEMENT, &t4);
        assert!(
            verdict_after_turn(
                &rec,
                &next,
                PHASE_IMPLEMENT,
                &t4,
                &noop.trust(),
                "codex",
                None,
                b.path(),
            )
            .is_some()
        );

        // Other-repo change control: A moves, B does not.
        std::fs::write(a.path().join("harness.txt"), "harness only\n").unwrap();
        git_ok(a.path(), &["add", "-A"]);
        git_ok(a.path(), &["commit", "-m", "harness-only"]);
        let mut other = running();
        other.run_epoch = 5;
        let mut t5 = t.clone();
        t5.run_epoch = 5;
        let noop = note_baseline(&rec, &other, PHASE_IMPLEMENT, &t5);
        assert!(
            verdict_after_turn(
                &rec,
                &other,
                PHASE_IMPLEMENT,
                &t5,
                &noop.trust(),
                "codex",
                None,
                b.path(),
            )
            .is_some()
        );

        // Branch-only control: no content or HEAD movement in B.
        let mut branch = running();
        branch.run_epoch = 6;
        let mut t6 = t.clone();
        t6.run_epoch = 6;
        let noop = note_baseline(&rec, &branch, PHASE_IMPLEMENT, &t6);
        git_ok(b.path(), &["checkout", "-b", "track/0551"]);
        assert!(
            verdict_after_turn(
                &rec,
                &branch,
                PHASE_IMPLEMENT,
                &t6,
                &noop.trust(),
                "codex",
                None,
                b.path(),
            )
            .is_some()
        );
    }

    #[test]
    fn diagnostics_keep_prefix_and_both_cwds() {
        let d = repo();
        let t = target(d.path(), d.path());
        let other = tempdir().unwrap();
        let message = no_change_message_for(&t, "codex", Some("auto"), other.path());
        assert!(message.starts_with(NO_CHANGE_PREFIX));
        let head: String = message.chars().take(200).collect();
        assert!(head.contains("track=0099"));
        assert!(head.contains(&d.path().display().to_string()));
        assert!(message.contains("source=row_execution"));
        assert!(message.contains(&other.path().display().to_string()));
        assert!(message.contains("harness=codex"));
        assert!(message.contains("mode=adapter-default"));
        assert!(message.contains("signal=auto"));
        let order = [
            "track=", "repo=", "source=", "cwd=", "harness=", "mode=", "signal=",
        ];
        let mut last = 0;
        for needle in order {
            let at = message.find(needle).expect(needle);
            assert!(at >= last, "{needle} out of order: {message}");
            last = at;
        }
    }

    #[test]
    fn message_has_stable_prefix_and_names_harness_mode_signal() {
        let m = no_change_message("implement", "codex", Some("approvals auto-allowed"));
        assert!(m.starts_with(NO_CHANGE_PREFIX));
        assert!(m.contains("harness codex"));
        assert!(m.contains("session mode"));
        assert!(m.contains("approvals auto-allowed"));
    }
}
