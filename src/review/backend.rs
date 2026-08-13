//! `ReviewBackend` trait plus scripted / recording test doubles.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::error::{CoordinatorError, Result};

#[derive(Debug, Clone)]
pub struct ReviewRequest {
    pub slug: String,
    pub harness: String,
    pub command: String,
    pub model: Option<String>,
    pub exec_repo: PathBuf,
    pub workspace_root: PathBuf,
    pub track_dir: Option<PathBuf>,
    pub prompt: String,
    pub remaining_timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewResult {
    pub exit: i32,
    pub stdout: String,
    pub stderr: String,
    pub last_message: String,
}

pub trait ReviewBackend: Send + Sync {
    fn run(&self, req: &ReviewRequest) -> Result<ReviewResult>;
}

/// Programmed sequence of results. Exhausted sequences repeat the last value.
pub struct ScriptedBackend {
    inner: Mutex<ScriptedInner>,
}

struct ScriptedInner {
    results: VecDeque<Result<ReviewResult>>,
    last: Option<Result<ReviewResult>>,
}

impl ScriptedBackend {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(ScriptedInner {
                results: VecDeque::new(),
                last: None,
            }),
        }
    }

    pub fn push(&self, v: Result<ReviewResult>) {
        self.inner
            .lock()
            .expect("scripted lock")
            .results
            .push_back(v);
    }

    pub fn push_ok(self, result: ReviewResult) -> Self {
        self.push(Ok(result));
        self
    }
}

impl Default for ScriptedBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl ReviewBackend for ScriptedBackend {
    fn run(&self, _req: &ReviewRequest) -> Result<ReviewResult> {
        let mut g = self.inner.lock().expect("scripted lock");
        let next = g
            .results
            .pop_front()
            .or_else(|| g.last.as_ref().map(clone_result));
        if let Some(v) = next {
            g.last = Some(clone_result(&v));
            return v;
        }
        Err(CoordinatorError::Message(
            "scripted backend: no review result programmed".into(),
        ))
    }
}

fn clone_result(v: &Result<ReviewResult>) -> Result<ReviewResult> {
    match v {
        Ok(r) => Ok(r.clone()),
        Err(e) => Err(CoordinatorError::Message(e.to_string())),
    }
}

#[derive(Clone, Default)]
pub struct CallCounts {
    pub runs: Arc<AtomicUsize>,
    pub slugs: Arc<Mutex<Vec<String>>>,
}

impl CallCounts {
    pub fn n(&self) -> usize {
        self.runs.load(Ordering::SeqCst)
    }

    pub fn slugs(&self) -> Vec<String> {
        self.slugs.lock().expect("slugs lock").clone()
    }
}

/// Wraps another backend and records spawn count + slugs.
pub struct RecordingBackend {
    inner: Arc<dyn ReviewBackend>,
    pub counts: CallCounts,
}

impl RecordingBackend {
    pub fn wrap(inner: Arc<dyn ReviewBackend>) -> Self {
        Self {
            inner,
            counts: CallCounts::default(),
        }
    }
}

impl ReviewBackend for RecordingBackend {
    fn run(&self, req: &ReviewRequest) -> Result<ReviewResult> {
        self.counts.runs.fetch_add(1, Ordering::SeqCst);
        self.counts
            .slugs
            .lock()
            .expect("slugs lock")
            .push(req.slug.clone());
        self.inner.run(req)
    }
}
