//! Token-idle `ci-wait` watcher (track 0010).
//!
//! Polls a [`CiBackend`] on an adaptive interval. Never injects a harness prompt.
//! `tick` must not sleep and must not spawn `gh` on every 500ms wake.

pub mod backend;
pub mod gh;

use std::path::Path;
use std::time::Duration;

#[cfg(test)]
use std::cell::RefCell;
#[cfg(test)]
use std::sync::Arc;

use chrono::Utc;

use crate::config::ENV_COORDINATOR_CI_POLL_MS;
use crate::error::{CoordinatorError, Result};
use crate::outcome::{
    FailureClass, LAST_EVENT_MESSAGE_CAP, OutcomeSource, PhaseOutcome, write_and_apply,
};
use crate::registry::ProjectRecord;
use crate::state::{
    CiWatchState, RunState, StatusView, load_run_state, save_run_state, with_run_state_lock,
};
use crate::workflow::WorkflowDriver;

pub use backend::{
    CallCounts, CheckBucket, CheckItem, CheckSnapshot, CiBackend, CiTarget, MergeResult, PrHint,
    RecordingBackend, ScriptedBackend,
};
pub use gh::GhCli;

const TWO_MIN: Duration = Duration::from_secs(120);
const TEN_MIN: Duration = Duration::from_secs(600);

#[cfg(test)]
thread_local! {
    static TEST_BACKEND: RefCell<Option<Arc<dyn CiBackend>>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub struct TestBackendGuard;

#[cfg(test)]
impl Drop for TestBackendGuard {
    fn drop(&mut self) {
        TEST_BACKEND.with(|c| *c.borrow_mut() = None);
    }
}

#[cfg(test)]
pub fn install_test_backend(backend: Arc<dyn CiBackend>) -> TestBackendGuard {
    TEST_BACKEND.with(|c| *c.borrow_mut() = Some(backend));
    TestBackendGuard
}

/// Drive one `ci-wait` tick. Stub synths immediately; file_wait waits on `current.json`.
pub fn drive(record: &ProjectRecord, state: &RunState) -> Result<Option<StatusView>> {
    match state.driver {
        WorkflowDriver::Stub => apply_success(
            record,
            state,
            "ci-wait: stub (no gh)".into(),
            OutcomeSource::Test,
        ),
        WorkflowDriver::FileWait => Ok(None),
        WorkflowDriver::Adapter => {
            #[cfg(test)]
            {
                let hooked = TEST_BACKEND.with(|c| c.borrow().clone());
                if let Some(b) = hooked {
                    return drive_with(record, state, b.as_ref());
                }
            }
            drive_with(record, state, &GhCli)
        }
    }
}

pub fn drive_with(
    record: &ProjectRecord,
    state: &RunState,
    backend: &dyn CiBackend,
) -> Result<Option<StatusView>> {
    let Some(cwd) = crate::layout::resolve(record).execution_repo else {
        return apply_failure(
            record,
            state,
            FailureClass::Permission,
            "no execution repo to watch CI".into(),
        );
    };

    let now = Utc::now();
    if !due_for_poll(state, now) {
        return Ok(None);
    }

    let hint = pr_hint(state);
    let target = match resolve_target(state, backend, &cwd, hint.as_ref()) {
        Ok(t) => t,
        Err(e) => return classify_backend_err(record, state, e),
    };

    let Some(target) = target else {
        persist_watch(record, Some("ci-wait: waiting for PR"), |ci| {
            stamp_poll(
                ci,
                now,
                "waiting for PR",
                "wait-pr",
                next_interval_ms(elapsed(state, now), false),
            );
        })?;
        return Ok(None);
    };

    persist_target(record, &target)?;

    if let CiTarget::PullRequest { is_draft: true, .. } = &target {
        persist_watch(record, Some("ci-wait: waiting (draft PR)"), |ci| {
            apply_target(ci, &target);
            stamp_poll(
                ci,
                now,
                "draft",
                &set_key(&target, &[]),
                next_interval_ms(elapsed(state, now), false),
            );
        })?;
        return Ok(None);
    }

    if let CiTarget::PullRequest {
        merged: true,
        number,
        ..
    } = &target
    {
        persist_watch(record, None, |ci| {
            apply_target(ci, &target);
            ci.merge = Some("done".into());
            stamp_poll(ci, now, "already merged", &set_key(&target, &[]), 15_000);
        })?;
        return apply_success(
            record,
            state,
            format!("ci-wait: merged #{number}"),
            OutcomeSource::Adapter,
        );
    }

    if state.ci.as_ref().and_then(|c| c.merge.as_deref()) == Some("done")
        || state.ci.as_ref().and_then(|c| c.merge.as_deref()) == Some("queued")
    {
        let n = match &target {
            CiTarget::PullRequest { number, .. } => *number,
            CiTarget::HeadSha { .. } => 0,
        };
        let msg = if state.ci.as_ref().and_then(|c| c.merge.as_deref()) == Some("queued") {
            format!("ci-wait: merged #{n} (queued)")
        } else {
            format!("ci-wait: merged #{n}")
        };
        persist_watch(record, None, |ci| {
            stamp_poll(ci, now, "already merged", &set_key(&target, &[]), 15_000);
        })?;
        return apply_success(record, state, msg, OutcomeSource::Adapter);
    }

    let snap = match backend.checks(&cwd, &target) {
        Ok(s) => s,
        Err(e) => return classify_backend_err(record, state, e),
    };

    let phase_elapsed = elapsed(state, now);
    let decision = match &target {
        CiTarget::PullRequest { .. } => interpret_pr(&snap),
        CiTarget::HeadSha { .. } => interpret_runs(&snap, phase_elapsed),
    };
    let summary = decision.summary();
    let key = set_key(&target, &snap.items);
    let changed = state.ci.as_ref().and_then(|c| c.set_key.as_deref()) != Some(key.as_str());
    let interval = next_interval_ms(phase_elapsed, changed);

    persist_watch(record, None, |ci| {
        apply_target(ci, &target);
        stamp_poll(ci, now, &summary, &key, interval);
    })?;

    match decision {
        Decision::Pending { event, .. } => {
            persist_watch(record, Some(&event), |_| {})?;
            Ok(None)
        }
        Decision::Fail { message, .. } => {
            apply_failure(record, state, FailureClass::CiFailed, message)
        }
        Decision::Green { .. } => finish_green(record, state, backend, &cwd, &target, &summary),
    }
}

fn finish_green(
    record: &ProjectRecord,
    state: &RunState,
    backend: &dyn CiBackend,
    cwd: &Path,
    target: &CiTarget,
    _summary: &str,
) -> Result<Option<StatusView>> {
    match target {
        CiTarget::HeadSha { .. } => {
            persist_watch(record, None, |ci| {
                ci.merge = Some("skipped".into());
            })?;
            apply_success(
                record,
                state,
                "ci-wait: green (default branch, no PR)".into(),
                OutcomeSource::Adapter,
            )
        }
        CiTarget::PullRequest {
            number, head_oid, ..
        } => {
            if !record.auto_merge {
                persist_watch(record, None, |ci| {
                    ci.merge = Some("skipped".into());
                })?;
                return apply_success(
                    record,
                    state,
                    "ci-wait: green; merge skipped (auto_merge=false)".into(),
                    OutcomeSource::Adapter,
                );
            }
            let merge = match backend.squash_merge(cwd, *number, head_oid.as_deref()) {
                Ok(m) => m,
                Err(e) => return classify_backend_err(record, state, e),
            };
            if !merge.ok {
                return apply_failure(
                    record,
                    state,
                    FailureClass::CiFailed,
                    format!("ci-wait: merge failed: {}", truncate_msg(&merge.message)),
                );
            }
            let (field, event) = if merge.queued {
                ("queued", format!("ci-wait: merged #{number} (queued)"))
            } else {
                ("done", format!("ci-wait: merged #{number}"))
            };
            persist_watch(record, None, |ci| {
                ci.merge = Some(field.into());
            })?;
            match apply_success(record, state, event.clone(), OutcomeSource::Adapter) {
                Ok(v) => Ok(v),
                Err(e) => {
                    let msg = format!(
                        "ci-wait: merged #{number} (state apply failed: {})",
                        truncate_msg(&e.to_string())
                    );
                    persist_watch(record, Some(&msg), |ci| {
                        ci.merge = Some(field.into());
                    })?;
                    Ok(Some(crate::run::status(record)?))
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Decision {
    Green { summary: String },
    Pending { event: String, summary: String },
    Fail { message: String, summary: String },
}

impl Decision {
    fn summary(&self) -> String {
        match self {
            Self::Green { summary }
            | Self::Pending { summary, .. }
            | Self::Fail { summary, .. } => summary.clone(),
        }
    }
}

/// PR buckets. Caller must already have rejected draft / already-merged.
fn interpret_pr(snap: &CheckSnapshot) -> Decision {
    let summary = summarize(&snap.items);
    if snap
        .items
        .iter()
        .any(|i| matches!(i.bucket, CheckBucket::Fail | CheckBucket::Cancel))
    {
        return Decision::Fail {
            message: format!("ci-wait: checks failed ({summary})"),
            summary,
        };
    }
    if snap
        .items
        .iter()
        .any(|i| matches!(i.bucket, CheckBucket::Pending))
    {
        return Decision::Pending {
            event: format!("ci-wait: waiting ({summary})"),
            summary,
        };
    }
    Decision::Green { summary }
}

/// HeadSha run-list mapping. Empty list: pending for &lt; 2 min, then green.
fn interpret_runs(snap: &CheckSnapshot, elapsed: Duration) -> Decision {
    let summary = summarize(&snap.items);
    if snap.items.is_empty() {
        if elapsed < TWO_MIN {
            return Decision::Pending {
                event: "ci-wait: waiting for runs".into(),
                summary,
            };
        }
        return Decision::Green { summary };
    }
    interpret_pr(snap)
}

pub fn summarize(items: &[CheckItem]) -> String {
    if items.is_empty() {
        return "0 checks".into();
    }
    let mut pass = 0u32;
    let mut fail = 0u32;
    let mut pending = 0u32;
    let mut skipping = 0u32;
    let mut cancel = 0u32;
    for i in items {
        match i.bucket {
            CheckBucket::Pass => pass += 1,
            CheckBucket::Fail => fail += 1,
            CheckBucket::Pending => pending += 1,
            CheckBucket::Skipping => skipping += 1,
            CheckBucket::Cancel => cancel += 1,
        }
    }
    let mut parts = Vec::new();
    if pass > 0 {
        parts.push(format!("{pass} pass"));
    }
    if fail > 0 {
        parts.push(format!("{fail} fail"));
    }
    if pending > 0 {
        parts.push(format!("{pending} pending"));
    }
    if skipping > 0 {
        parts.push(format!("{skipping} skipping"));
    }
    if cancel > 0 {
        parts.push(format!("{cancel} cancel"));
    }
    if parts.is_empty() {
        "0 checks".into()
    } else {
        parts.join(", ")
    }
}

pub fn initial_interval_ms() -> u64 {
    fixed_interval_ms().unwrap_or(15_000)
}

pub fn fixed_interval_ms() -> Option<u64> {
    std::env::var(ENV_COORDINATOR_CI_POLL_MS)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|n| n.max(1))
}

pub fn next_interval_ms(elapsed: Duration, set_changed: bool) -> u64 {
    if let Some(fixed) = fixed_interval_ms() {
        return fixed;
    }
    if set_changed {
        return 15_000;
    }
    let raw = if elapsed < TWO_MIN {
        15_000
    } else if elapsed < TEN_MIN {
        30_000
    } else {
        60_000
    };
    raw.min(120_000)
}

fn due_for_poll(state: &RunState, now: chrono::DateTime<Utc>) -> bool {
    let Some(ci) = state.ci.as_ref() else {
        return true;
    };
    let Some(last) = ci.last_poll_at else {
        return true;
    };
    let interval = fixed_interval_ms()
        .or(ci.next_interval_ms)
        .unwrap_or_else(initial_interval_ms);
    let due = last + chrono::Duration::milliseconds(interval as i64);
    now >= due
}

fn elapsed(state: &RunState, now: chrono::DateTime<Utc>) -> Duration {
    state.effective_running_elapsed(now)
}

fn pr_hint(state: &RunState) -> Option<PrHint> {
    let ci = state.ci.as_ref()?;
    if ci.pr_number.is_none() && ci.pr_url.is_none() {
        return None;
    }
    Some(PrHint {
        number: ci.pr_number,
        url: ci.pr_url.clone(),
    })
}

fn resolve_target(
    state: &RunState,
    backend: &dyn CiBackend,
    cwd: &Path,
    hint: Option<&PrHint>,
) -> Result<Option<CiTarget>> {
    if let Some(ci) = state.ci.as_ref() {
        if let Some(n) = ci.pr_number {
            let hinted = PrHint {
                number: Some(n),
                url: ci.pr_url.clone(),
            };
            if let Some(t) = backend.resolve_pr(cwd, Some(&hinted))? {
                return Ok(Some(t));
            }
            // Do not invent is_draft=false on a transient view miss.
            return Ok(None);
        }
        if let Some(ref sha) = ci.head_sha {
            return Ok(Some(CiTarget::HeadSha { sha: sha.clone() }));
        }
    }
    backend.resolve_pr(cwd, hint)
}

fn set_key(target: &CiTarget, items: &[CheckItem]) -> String {
    let kind = match target {
        CiTarget::PullRequest { number, .. } => format!("pr:{number}"),
        CiTarget::HeadSha { sha } => format!("sha:{sha}"),
    };
    let mut parts: Vec<String> = items
        .iter()
        .map(|i| format!("{}:{}", i.name, i.bucket.as_str()))
        .collect();
    parts.sort();
    format!("{kind}|{}", parts.join(","))
}

fn apply_target(ci: &mut CiWatchState, target: &CiTarget) {
    match target {
        CiTarget::PullRequest {
            number,
            url,
            head_oid,
            ..
        } => {
            ci.pr_number = Some(*number);
            if !url.is_empty() {
                ci.pr_url = Some(url.clone());
            }
            if let Some(oid) = head_oid {
                ci.head_sha = Some(oid.clone());
            }
        }
        CiTarget::HeadSha { sha } => {
            ci.head_sha = Some(sha.clone());
        }
    }
}

fn stamp_poll(
    ci: &mut CiWatchState,
    now: chrono::DateTime<Utc>,
    summary: &str,
    key: &str,
    interval: u64,
) {
    ci.last_poll_at = Some(now);
    ci.last_summary = Some(summary.to_string());
    ci.set_key = Some(key.to_string());
    ci.next_interval_ms = Some(interval);
}

fn persist_target(record: &ProjectRecord, target: &CiTarget) -> Result<()> {
    persist_watch(record, None, |ci| apply_target(ci, target)).map(|_| ())
}

fn persist_watch(
    record: &ProjectRecord,
    last_event: Option<&str>,
    f: impl FnOnce(&mut CiWatchState),
) -> Result<StatusView> {
    with_run_state_lock(record, || {
        let mut s = load_run_state(record)?;
        let mut ci = s.ci.take().unwrap_or_default();
        f(&mut ci);
        s.ci = Some(ci);
        if let Some(ev) = last_event {
            s.last_event = ev.to_string();
        }
        s.updated_at = Utc::now();
        save_run_state(record, &s)?;
        Ok(StatusView::from_record(record, &s))
    })
}

fn apply_success(
    record: &ProjectRecord,
    state: &RunState,
    message: String,
    source: OutcomeSource,
) -> Result<Option<StatusView>> {
    let outcome = PhaseOutcome::success(
        state.phase.clone(),
        source,
        Some(message),
        None,
        Some(state.run_epoch),
    );
    write_and_apply(record, outcome).map(Some)
}

fn apply_failure(
    record: &ProjectRecord,
    state: &RunState,
    class: FailureClass,
    message: String,
) -> Result<Option<StatusView>> {
    let outcome = PhaseOutcome::failure(
        state.phase.clone(),
        class,
        OutcomeSource::Adapter,
        Some(message),
        Some(state.run_epoch),
    );
    write_and_apply(record, outcome).map(Some)
}

fn classify_backend_err(
    record: &ProjectRecord,
    state: &RunState,
    e: CoordinatorError,
) -> Result<Option<StatusView>> {
    let msg = e.to_string();
    if msg.contains("no execution repo")
        || msg.contains("gh not found")
        || msg.contains("not executable")
        || msg.contains("auth required")
    {
        return apply_failure(record, state, FailureClass::Permission, msg);
    }
    if msg.contains("gh timed out") {
        persist_watch(record, Some("ci-wait: gh timed out"), |ci| {
            ci.last_poll_at = Some(Utc::now());
            ci.next_interval_ms = Some(next_interval_ms(elapsed(state, Utc::now()), false));
        })?;
        return Ok(None);
    }
    persist_watch(
        record,
        Some(&format!("ci-wait: {}", truncate_msg(&msg))),
        |ci| {
            ci.last_poll_at = Some(Utc::now());
            ci.next_interval_ms = Some(next_interval_ms(elapsed(state, Utc::now()), false));
        },
    )?;
    Ok(None)
}

fn truncate_msg(s: &str) -> String {
    let t = s.trim();
    if t.chars().count() <= LAST_EVENT_MESSAGE_CAP {
        return t.to_string();
    }
    let cut: String = t.chars().take(LAST_EVENT_MESSAGE_CAP).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_env_lock;
    use crate::notify::ENV_COORDINATOR_NOTIFY;
    use crate::outcome::OutcomeMetadata;
    use crate::run::{self, run_with_driver};
    use crate::state::{RunStatus, load_run_state, save_run_state};
    use crate::watch::poll_once;
    use crate::workflow::graph;
    use std::sync::Arc;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn rec(path: &std::path::Path, auto_merge: bool) -> ProjectRecord {
        ProjectRecord {
            id: Uuid::new_v4().to_string(),
            path: path.to_path_buf(),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: Some(path.to_path_buf()),
            execution_repos: Default::default(),
            state_dir: None,
            auto_merge,
            created_at: Utc::now(),
        }
    }

    fn rec_no_exec(path: &std::path::Path) -> ProjectRecord {
        let mut r = rec(path, true);
        r.execution_repo = None;
        r
    }

    fn jump_ci_wait(r: &ProjectRecord, driver: WorkflowDriver) {
        run_with_driver(r, Some("0010".into()), driver).unwrap();
        let mut s = load_run_state(r).unwrap();
        s.phase = graph::PHASE_CI_WAIT.into();
        s.last_driven_phase = None;
        save_run_state(r, &s).unwrap();
    }

    fn pr(n: u64, draft: bool, merged: bool) -> CiTarget {
        CiTarget::PullRequest {
            number: n,
            url: format!("https://example/pr/{n}"),
            is_draft: draft,
            merged,
            head_oid: Some("abc".into()),
        }
    }

    fn items(pairs: &[(&str, CheckBucket)]) -> CheckSnapshot {
        CheckSnapshot {
            items: pairs
                .iter()
                .map(|(n, b)| CheckItem {
                    name: (*n).into(),
                    bucket: *b,
                })
                .collect(),
            raw_exit: 0,
        }
    }

    fn hook(scripted: ScriptedBackend) -> (TestBackendGuard, CallCounts) {
        let rec = RecordingBackend::wrap(Arc::new(scripted));
        let counts = rec.counts.clone();
        let g = install_test_backend(Arc::new(rec));
        (g, counts)
    }

    #[test]
    fn interpret_fail_cancel_pending_green_empty() {
        assert!(matches!(
            interpret_pr(&items(&[("a", CheckBucket::Fail)])),
            Decision::Fail { .. }
        ));
        assert!(matches!(
            interpret_pr(&items(&[("a", CheckBucket::Cancel)])),
            Decision::Fail { .. }
        ));
        assert!(matches!(
            interpret_pr(&items(&[
                ("a", CheckBucket::Pass),
                ("b", CheckBucket::Pending)
            ])),
            Decision::Pending { .. }
        ));
        assert!(matches!(
            interpret_pr(&items(&[
                ("a", CheckBucket::Pass),
                ("b", CheckBucket::Skipping)
            ])),
            Decision::Green { .. }
        ));
        assert!(matches!(
            interpret_pr(&CheckSnapshot::empty()),
            Decision::Green { .. }
        ));
    }

    #[test]
    fn interpret_runs_empty_two_minute_rule() {
        assert!(matches!(
            interpret_runs(&CheckSnapshot::empty(), Duration::from_secs(30)),
            Decision::Pending { event, .. } if event.contains("waiting for runs")
        ));
        assert!(matches!(
            interpret_runs(&CheckSnapshot::empty(), Duration::from_secs(120)),
            Decision::Green { .. }
        ));
    }

    #[test]
    fn interval_table_and_fixed_env() {
        let _g = test_env_lock();
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
        }
        assert_eq!(next_interval_ms(Duration::from_secs(10), false), 15_000);
        assert_eq!(next_interval_ms(Duration::from_secs(180), false), 30_000);
        assert_eq!(next_interval_ms(Duration::from_secs(700), false), 60_000);
        assert_eq!(next_interval_ms(Duration::from_secs(700), true), 15_000);
        unsafe {
            std::env::set_var(ENV_COORDINATOR_CI_POLL_MS, "10");
        }
        assert_eq!(next_interval_ms(Duration::from_secs(700), false), 10);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
        }
    }

    fn poll_env() -> std::sync::MutexGuard<'static, ()> {
        let g = test_env_lock();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_CI_POLL_MS, "1");
            std::env::set_var(ENV_COORDINATOR_NOTIFY, "off");
        }
        g
    }

    #[test]
    fn pending_does_not_apply() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr(7, false, false))));
        s.push_snapshot(Ok(items(&[("ci", CheckBucket::Pending)])));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap();
        assert!(view.is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert!(st.last_event.contains("waiting"));
        assert_eq!(counts.merge_n(), 0);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn fail_writes_ci_failed_artifact() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr(7, false, false))));
        s.push_snapshot(Ok(items(&[("ci", CheckBucket::Fail)])));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap().expect("fail apply");
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.failure_class, Some(FailureClass::CiFailed));
        assert!(crate::notify::artifact::existing_path(&r).is_some());
        assert_eq!(counts.merge_n(), 0);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn green_auto_merge_calls_squash_once() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr(9, false, false))));
        s.push_snapshot(Ok(items(&[("ci", CheckBucket::Pass)])));
        s.push_merge(Ok(MergeResult {
            ok: true,
            queued: false,
            message: "merged".into(),
        }));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap().expect("merged");
        assert_eq!(view.phase, graph::PHASE_COMPACT);
        assert!(
            view.last_event.contains("ci-wait: merged #9"),
            "last_event={}",
            view.last_event
        );
        assert_eq!(counts.merge_n(), 1);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn green_auto_merge_false_zero_merges() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), false);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr(3, false, false))));
        s.push_snapshot(Ok(items(&[("ci", CheckBucket::Pass)])));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap().expect("skip merge");
        assert_eq!(view.phase, graph::PHASE_COMPACT);
        assert!(view.last_event.contains("merge skipped (auto_merge=false)"));
        assert_eq!(counts.merge_n(), 0);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn head_sha_green_zero_merges() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(CiTarget::HeadSha {
            sha: "deadbeef".into(),
        })));
        s.push_snapshot(Ok(items(&[("ci", CheckBucket::Pass)])));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap().expect("headsha");
        assert_eq!(view.phase, graph::PHASE_COMPACT);
        assert!(view.last_event.contains("default branch, no PR"));
        assert_eq!(counts.merge_n(), 0);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn draft_stays_pending_even_when_green() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr(4, true, false))));
        s.push_snapshot(Ok(items(&[("ci", CheckBucket::Pass)])));
        s.push_merge(Ok(MergeResult {
            ok: true,
            queued: false,
            message: "should not run".into(),
        }));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap();
        assert!(view.is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert!(st.last_event.contains("ci-wait: waiting (draft PR)"));
        assert_eq!(counts.merge_n(), 0);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn stop_during_pending_zero_merges_no_artifact() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr(1, false, false))));
        s.push_snapshot(Ok(items(&[("ci", CheckBucket::Pending)])));
        let (_hook, counts) = hook(s);
        let _ = crate::workflow::tick(&r).unwrap();
        let stopped = run::stop(&r).unwrap();
        assert_eq!(stopped.status, RunStatus::Stopped);
        assert_eq!(stopped.last_event, crate::state::STOP_LAST_EVENT);
        let again = crate::workflow::tick(&r).unwrap();
        assert!(again.is_none());
        assert_eq!(counts.merge_n(), 0);
        assert!(crate::notify::artifact::existing_path(&r).is_none());
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn pause_then_green_advances_to_compact_paused() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr(8, false, false))));
        s.push_snapshot(Ok(items(&[("ci", CheckBucket::Pending)])));
        s.push_snapshot(Ok(items(&[("ci", CheckBucket::Pass)])));
        s.push_merge(Ok(MergeResult {
            ok: true,
            queued: false,
            message: "ok".into(),
        }));
        let (_hook, _c) = hook(s);
        assert!(crate::workflow::tick(&r).unwrap().is_none());
        run::pause(&r).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let view = crate::workflow::tick(&r).unwrap().expect("paused finish");
        assert_eq!(view.status, RunStatus::Paused);
        assert_eq!(view.phase, graph::PHASE_COMPACT);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn merge_nonzero_applies_ci_failed() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr(11, false, false))));
        s.push_snapshot(Ok(items(&[("ci", CheckBucket::Pass)])));
        s.push_merge(Ok(MergeResult {
            ok: false,
            queued: false,
            message: "GraphQL: Pull Request is not mergeable".into(),
        }));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap().expect("merge fail");
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.failure_class, Some(FailureClass::CiFailed));
        assert!(view.last_event.contains("merge failed"));
        assert!(crate::notify::artifact::existing_path(&r).is_some());
        assert_eq!(counts.merge_n(), 1);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn already_merged_skips_squash_and_succeeds() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr(5, false, true))));
        s.push_merge(Ok(MergeResult {
            ok: true,
            queued: false,
            message: "should not run".into(),
        }));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap().expect("merged");
        assert_eq!(view.phase, graph::PHASE_COMPACT);
        assert!(view.last_event.contains("ci-wait: merged #5"));
        assert_eq!(counts.merge_n(), 0);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn adapter_path_does_not_mark_driven_or_prompt() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr(2, false, false))));
        s.push_snapshot(Ok(items(&[("ci", CheckBucket::Pending)])));
        let (_hook, _c) = hook(s);
        assert!(crate::workflow::tick(&r).unwrap().is_none());
        let st = load_run_state(&r).unwrap();
        assert!(
            st.last_driven_phase.is_none(),
            "ci-wait must not use inject-once last_driven_phase"
        );
        assert!(crate::harness::status_bundle_sync(&r).is_none());
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn missing_execution_repo_is_permission() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec_no_exec(dir.path());
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let view = crate::workflow::tick(&r).unwrap().expect("perm");
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.failure_class, Some(FailureClass::Permission));
        assert!(view.last_event.contains("no execution repo"));
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn file_wait_success_advances() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::FileWait);
        assert!(crate::workflow::tick(&r).unwrap().is_none());
        let o = PhaseOutcome::success(graph::PHASE_CI_WAIT, OutcomeSource::File, None, None, None);
        crate::outcome::save_current_outcome(&r, &o).unwrap();
        let view = poll_once(&r).unwrap().expect("file apply");
        assert_eq!(view.phase, graph::PHASE_COMPACT);
    }

    #[test]
    fn implement_pr_hint_copied_onto_ci() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        run_with_driver(&r, None, WorkflowDriver::FileWait).unwrap();
        let mut s = load_run_state(&r).unwrap();
        s.phase = graph::PHASE_IMPLEMENT.into();
        save_run_state(&r, &s).unwrap();
        let mut o = PhaseOutcome::success(
            graph::PHASE_IMPLEMENT,
            OutcomeSource::Test,
            None,
            None,
            None,
        );
        o.metadata = Some(OutcomeMetadata {
            pr_number: Some(42),
            pr_url: Some("https://example/pr/42".into()),
            ..Default::default()
        });
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.phase, graph::PHASE_CROSS_MODEL);
        let st = load_run_state(&r).unwrap();
        assert_eq!(st.ci.as_ref().and_then(|c| c.pr_number), Some(42));
        let after = crate::workflow::tick(&r).unwrap().expect("skip 0011");
        assert_eq!(after.phase, graph::PHASE_CI_WAIT);
        let st = load_run_state(&r).unwrap();
        assert_eq!(st.ci.as_ref().and_then(|c| c.pr_number), Some(42));
        assert_eq!(
            st.ci.as_ref().and_then(|c| c.pr_url.as_deref()),
            Some("https://example/pr/42")
        );
        let status = run::status(&r).unwrap();
        assert!(status.ci.is_some());
        assert!(status.ci.as_ref().unwrap().auto_merge);
        assert_eq!(status.ci.as_ref().unwrap().pr, Some(42));
    }

    #[test]
    fn fresh_run_clears_ci() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        run_with_driver(&r, None, WorkflowDriver::FileWait).unwrap();
        let mut s = load_run_state(&r).unwrap();
        s.ci = Some(CiWatchState {
            pr_number: Some(1),
            ..Default::default()
        });
        save_run_state(&r, &s).unwrap();
        run::stop(&r).unwrap();
        run_with_driver(&r, None, WorkflowDriver::FileWait).unwrap();
        let st = load_run_state(&r).unwrap();
        assert!(st.ci.is_none());
    }

    #[test]
    fn is_grok_bound_false_for_ci_wait() {
        assert!(!graph::is_grok_bound(graph::PHASE_CI_WAIT));
        assert!(!graph::is_skip_phase(graph::PHASE_CI_WAIT));
    }
}
