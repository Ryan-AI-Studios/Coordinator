//! `CiBackend` trait plus scripted / recording test doubles.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::error::{CoordinatorError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiTarget {
    PullRequest {
        number: u64,
        url: String,
        is_draft: bool,
        merged: bool,
        head_oid: Option<String>,
    },
    HeadSha {
        sha: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckBucket {
    Pass,
    Fail,
    Pending,
    Skipping,
    Cancel,
}

impl CheckBucket {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Pending => "pending",
            Self::Skipping => "skipping",
            Self::Cancel => "cancel",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "pass" | "success" => Self::Pass,
            "fail" | "failure" | "error" | "timed_out" | "startup_failure" => Self::Fail,
            "pending" | "in_progress" | "queued" | "waiting" => Self::Pending,
            "skipping" | "skipped" | "neutral" | "" => Self::Skipping,
            "cancel" | "cancelled" | "canceled" => Self::Cancel,
            _ => Self::Pending,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckItem {
    pub name: String,
    pub bucket: CheckBucket,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckSnapshot {
    pub items: Vec<CheckItem>,
    pub raw_exit: i32,
}

impl CheckSnapshot {
    pub fn empty() -> Self {
        Self {
            items: Vec::new(),
            raw_exit: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeResult {
    pub ok: bool,
    pub queued: bool,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrHint {
    pub number: Option<u64>,
    pub url: Option<String>,
}

pub trait CiBackend: Send + Sync {
    fn resolve_pr(&self, cwd: &Path, hint: Option<&PrHint>) -> Result<Option<CiTarget>>;
    fn checks(&self, cwd: &Path, target: &CiTarget) -> Result<CheckSnapshot>;
    fn squash_merge(
        &self,
        cwd: &Path,
        pr_number: u64,
        head_oid: Option<&str>,
    ) -> Result<MergeResult>;
}

/// Programmed sequence of results. Exhausted sequences repeat the last value.
pub struct ScriptedBackend {
    inner: Mutex<ScriptedInner>,
}

struct ScriptedInner {
    resolves: VecDeque<Result<Option<CiTarget>>>,
    snapshots: VecDeque<Result<CheckSnapshot>>,
    merges: VecDeque<Result<MergeResult>>,
    last_resolve: Option<Result<Option<CiTarget>>>,
    last_snap: Option<Result<CheckSnapshot>>,
    last_merge: Option<Result<MergeResult>>,
}

impl ScriptedBackend {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(ScriptedInner {
                resolves: VecDeque::new(),
                snapshots: VecDeque::new(),
                merges: VecDeque::new(),
                last_resolve: None,
                last_snap: None,
                last_merge: None,
            }),
        }
    }

    pub fn push_resolve(&self, v: Result<Option<CiTarget>>) {
        self.inner
            .lock()
            .expect("scripted lock")
            .resolves
            .push_back(v);
    }

    pub fn push_snapshot(&self, v: Result<CheckSnapshot>) {
        self.inner
            .lock()
            .expect("scripted lock")
            .snapshots
            .push_back(v);
    }

    pub fn push_merge(&self, v: Result<MergeResult>) {
        self.inner
            .lock()
            .expect("scripted lock")
            .merges
            .push_back(v);
    }

    pub fn with_pr(self, target: CiTarget, snap: CheckSnapshot) -> Self {
        self.push_resolve(Ok(Some(target)));
        self.push_snapshot(Ok(snap));
        self
    }
}

impl Default for ScriptedBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl CiBackend for ScriptedBackend {
    fn resolve_pr(&self, _cwd: &Path, _hint: Option<&PrHint>) -> Result<Option<CiTarget>> {
        let mut g = self.inner.lock().expect("scripted lock");
        let next = g
            .resolves
            .pop_front()
            .or_else(|| g.last_resolve.as_ref().map(clone_resolve));
        if let Some(v) = next {
            g.last_resolve = Some(clone_resolve(&v));
            return v;
        }
        Ok(None)
    }

    fn checks(&self, _cwd: &Path, _target: &CiTarget) -> Result<CheckSnapshot> {
        let mut g = self.inner.lock().expect("scripted lock");
        let next = g
            .snapshots
            .pop_front()
            .or_else(|| g.last_snap.as_ref().map(clone_snap));
        if let Some(v) = next {
            g.last_snap = Some(clone_snap(&v));
            return v;
        }
        Ok(CheckSnapshot::empty())
    }

    fn squash_merge(
        &self,
        _cwd: &Path,
        _pr_number: u64,
        _head_oid: Option<&str>,
    ) -> Result<MergeResult> {
        let mut g = self.inner.lock().expect("scripted lock");
        let next = g
            .merges
            .pop_front()
            .or_else(|| g.last_merge.as_ref().map(clone_merge));
        if let Some(v) = next {
            g.last_merge = Some(clone_merge(&v));
            return v;
        }
        Err(CoordinatorError::Message(
            "scripted backend: no merge result programmed".into(),
        ))
    }
}

fn clone_resolve(v: &Result<Option<CiTarget>>) -> Result<Option<CiTarget>> {
    match v {
        Ok(t) => Ok(t.clone()),
        Err(e) => Err(CoordinatorError::Message(e.to_string())),
    }
}

fn clone_snap(v: &Result<CheckSnapshot>) -> Result<CheckSnapshot> {
    match v {
        Ok(s) => Ok(s.clone()),
        Err(e) => Err(CoordinatorError::Message(e.to_string())),
    }
}

fn clone_merge(v: &Result<MergeResult>) -> Result<MergeResult> {
    match v {
        Ok(m) => Ok(m.clone()),
        Err(e) => Err(CoordinatorError::Message(e.to_string())),
    }
}

#[derive(Clone, Default)]
pub struct CallCounts {
    pub resolve: Arc<AtomicUsize>,
    pub checks: Arc<AtomicUsize>,
    pub merge: Arc<AtomicUsize>,
}

impl CallCounts {
    pub fn resolve_n(&self) -> usize {
        self.resolve.load(Ordering::SeqCst)
    }
    pub fn checks_n(&self) -> usize {
        self.checks.load(Ordering::SeqCst)
    }
    pub fn merge_n(&self) -> usize {
        self.merge.load(Ordering::SeqCst)
    }
}

/// Wraps another backend and counts calls.
pub struct RecordingBackend {
    inner: Arc<dyn CiBackend>,
    pub counts: CallCounts,
}

impl RecordingBackend {
    pub fn wrap(inner: Arc<dyn CiBackend>) -> Self {
        Self {
            inner,
            counts: CallCounts::default(),
        }
    }
}

impl CiBackend for RecordingBackend {
    fn resolve_pr(&self, cwd: &Path, hint: Option<&PrHint>) -> Result<Option<CiTarget>> {
        self.counts.resolve.fetch_add(1, Ordering::SeqCst);
        self.inner.resolve_pr(cwd, hint)
    }

    fn checks(&self, cwd: &Path, target: &CiTarget) -> Result<CheckSnapshot> {
        self.counts.checks.fetch_add(1, Ordering::SeqCst);
        self.inner.checks(cwd, target)
    }

    fn squash_merge(
        &self,
        cwd: &Path,
        pr_number: u64,
        head_oid: Option<&str>,
    ) -> Result<MergeResult> {
        self.counts.merge.fetch_add(1, Ordering::SeqCst);
        self.inner.squash_merge(cwd, pr_number, head_oid)
    }
}
