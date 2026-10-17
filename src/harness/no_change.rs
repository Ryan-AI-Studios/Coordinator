//! Unchanged-tree guard for adapter implement legs (track 0095).
//!
//! An `implement` / `address-findings` turn that finishes without touching the tree is a dead leg:
//! the adapter cannot know it did nothing, so it reports success and the run would advance (and
//! later fail at `ci-wait`, see 0094). The leg baseline is sampled before the first turn of the leg
//! and compared after each turn, inside the pool (before `apply_turn` builds a success outcome)
//! because the detached holder applies the outcome itself.
//!
//! Detection is fail-open on purpose: if either git sample cannot be read the guard does not fire,
//! and 0094's `ci-wait` empty-leg failure remains the backstop.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::registry::ProjectRecord;
use crate::state::{RunState, resolve_state_dir};
use crate::workflow::WorkflowDriver;
use crate::workflow::graph::{PHASE_ADDRESS_FINDINGS, PHASE_IMPLEMENT};

/// Stable prefix of the failure message (`failure show` / tests key on it).
pub const NO_CHANGE_PREFIX: &str = "implement leg produced no changes";

const BASELINE_FILE: &str = "leg-baseline.json";

/// What the tree looked like at one instant. `diff` hashes `git diff HEAD` so a further edit to a
/// file that was already dirty still counts as a change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeSample {
    pub head: String,
    pub porcelain: BTreeSet<String>,
    pub diff: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Baseline {
    phase: String,
    epoch: u64,
    sample: TreeSample,
}

/// Only adapter-driven `implement` / `address-findings` legs are checked.
pub fn applies(state: &RunState, phase: &str) -> bool {
    state.driver == WorkflowDriver::Adapter
        && (phase == PHASE_IMPLEMENT || phase == PHASE_ADDRESS_FINDINGS)
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
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

fn baseline_path(record: &ProjectRecord) -> Option<PathBuf> {
    resolve_state_dir(record)
        .ok()
        .map(|d| d.join(BASELINE_FILE))
}

fn load_baseline(record: &ProjectRecord) -> Option<Baseline> {
    let text = std::fs::read_to_string(baseline_path(record)?).ok()?;
    serde_json::from_str(&text).ok()
}

/// Record the leg baseline before a turn. A later turn of the same leg (same phase and run epoch,
/// e.g. a self-check continuation) keeps the original baseline.
pub fn note_baseline(record: &ProjectRecord, state: &RunState, phase: &str, cwd: &Path) {
    if !applies(state, phase) {
        return;
    }
    if let Some(b) = load_baseline(record)
        && b.phase == phase
        && b.epoch == state.run_epoch
    {
        return;
    }
    let (Some(path), Some(sample)) = (baseline_path(record), sample(cwd)) else {
        return;
    };
    let baseline = Baseline {
        phase: phase.to_string(),
        epoch: state.run_epoch,
        sample,
    };
    if let Ok(text) = serde_json::to_string(&baseline) {
        let _ = std::fs::write(path, text);
    }
}

/// `Some(message)` when the turn left the tree exactly as the leg found it.
pub fn verdict_after_turn(
    record: &ProjectRecord,
    state: &RunState,
    phase: &str,
    cwd: &Path,
    harness: &str,
    last_signal: Option<&str>,
) -> Option<String> {
    if !applies(state, phase) {
        return None;
    }
    let baseline = load_baseline(record)?;
    if baseline.phase != phase || baseline.epoch != state.run_epoch {
        return None;
    }
    let now = sample(cwd)?;
    if now != baseline.sample {
        return None;
    }
    Some(no_change_message(phase, harness, last_signal))
}

pub fn no_change_message(phase: &str, harness: &str, last_signal: Option<&str>) -> String {
    format!(
        "{NO_CHANGE_PREFIX}: phase {phase}, harness {harness}, session mode adapter-default, \
         last permission signal {}; the tree is exactly as the leg found it. If the plan was \
         already satisfied, advance by hand; otherwise the adapter may be sandboxed read-only.",
        last_signal.unwrap_or("none")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_ok(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
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
    fn message_has_stable_prefix_and_names_harness_mode_signal() {
        let m = no_change_message("implement", "codex", Some("approvals auto-allowed"));
        assert!(m.starts_with(NO_CHANGE_PREFIX));
        assert!(m.contains("harness codex"));
        assert!(m.contains("session mode"));
        assert!(m.contains("approvals auto-allowed"));
        assert!(no_change_message("implement", "codex", None).contains("signal none"));
    }

    #[test]
    fn applies_only_to_adapter_implement_and_address_findings() {
        let mut s = RunState::idle("p");
        s.driver = WorkflowDriver::Adapter;
        assert!(applies(&s, PHASE_IMPLEMENT));
        assert!(applies(&s, PHASE_ADDRESS_FINDINGS));
        assert!(!applies(&s, "fold"));
        assert!(!applies(&s, "cross-model-review"));
        s.driver = WorkflowDriver::Stub;
        assert!(!applies(&s, PHASE_IMPLEMENT));
        s.driver = WorkflowDriver::FileWait;
        assert!(!applies(&s, PHASE_IMPLEMENT));
    }
}
