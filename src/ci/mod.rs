//! Token-idle `ci-wait` watcher (track 0010).
//!
//! Polls a [`CiBackend`] on an adaptive interval. Never injects a harness prompt.
//! `tick` must not sleep and must not spawn `gh` on every 500ms wake.

pub mod backend;
pub mod fix;
pub mod gh;

use std::path::Path;
use std::time::Duration;

#[cfg(test)]
use std::cell::RefCell;
#[cfg(test)]
use std::sync::{Arc, Mutex, MutexGuard};

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
use crate::workflow::{MergedTrackProbe, WorkflowDriver};

pub use backend::{
    AutoPublishResult, CallCounts, CheckBucket, CheckItem, CheckSnapshot, CheckView, CiBackend,
    CiTarget, MergeResult, MergeStateStatus, PrHint, RecordingBackend, ScriptedBackend,
};
pub use gh::{GhCli, GhMergedTrackProbe};

const TWO_MIN: Duration = Duration::from_secs(120);
const TEN_MIN: Duration = Duration::from_secs(600);
/// Transient push/create misses before `ci_failed`. Separate from `CI_FIX_CAP`.
const PUBLISH_TRANSIENT_CAP: u32 = 2;

#[cfg(test)]
thread_local! {
    static TEST_BACKEND: RefCell<Option<Arc<dyn CiBackend>>> = const { RefCell::new(None) };
}

// None = unset (tests do not call gh). Some(None) forces no PR. Some(Some(n)) forces the sentence.
#[cfg(test)]
static TEST_OPEN_GREEN_PR: Mutex<Option<Option<u64>>> = Mutex::new(None);

#[cfg(test)]
static GREEN_PR_TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub struct GreenPrGuard {
    _lock: MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for GreenPrGuard {
    fn drop(&mut self) {
        if let Ok(mut slot) = TEST_OPEN_GREEN_PR.lock() {
            *slot = None;
        }
    }
}

#[cfg(test)]
pub fn set_test_open_green_pr(value: Option<u64>) -> GreenPrGuard {
    let lock = GREEN_PR_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    *TEST_OPEN_GREEN_PR.lock().unwrap_or_else(|p| p.into_inner()) = Some(value);
    GreenPrGuard { _lock: lock }
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
    let Some(cwd) = crate::worktree::product_git_cwd(record) else {
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

    let head_gate = crate::policy::decide(record, state, &cwd, "HEAD");
    if let crate::policy::GateDecision::Block(gate) = &head_gate {
        crate::policy::remember_gate(record, Some(gate))?;
        return apply_failure(record, state, FailureClass::Permission, gate.detail.clone());
    }
    crate::policy::remember_gate(record, head_gate.alert())?;

    let hint = pr_hint(state);
    let target = match resolve_target(state, backend, &cwd, hint.as_ref()) {
        Ok(t) => t,
        Err(e) => return classify_backend_err(record, state, e),
    };
    // Probe after resolve (outside the apply lock). Err / missing → fail-open wait.
    let target = target.or_else(|| try_merged_track_target(state, &cwd));

    let mut just_opened: Option<u64> = None;
    let target = match target {
        Some(t) => Some(t),
        None => {
            if let MutationGate::Stop(view) = release_for_mutation(record, state, &head_gate)? {
                return Ok(view.map(|view| *view));
            }
            match try_auto_publish_target(record, state, backend, &cwd, now) {
                Ok(AutoPublishDrive::Opened(t)) => {
                    if let CiTarget::PullRequest { number, .. } = &t {
                        just_opened = Some(*number);
                    }
                    Some(t)
                }
                Ok(AutoPublishDrive::Wait) => return Ok(None),
                Ok(AutoPublishDrive::Stopped(view)) => return Ok(view.map(|view| *view)),
                Err(e) => return classify_backend_err(record, state, e),
            }
        }
    };

    let Some(target) = target else {
        persist_watch(record, Some("ci-wait: waiting for PR"), |ci| {
            ci.head_sha = None;
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
        head_ref,
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
            pr_merged_event(*number, head_ref, ""),
            OutcomeSource::Adapter,
        );
    }

    if state.ci.as_ref().and_then(|c| c.merge.as_deref()) == Some("done")
        || state.ci.as_ref().and_then(|c| c.merge.as_deref()) == Some("queued")
    {
        let queued = state.ci.as_ref().and_then(|c| c.merge.as_deref()) == Some("queued");
        let msg = match &target {
            CiTarget::PullRequest {
                number, head_ref, ..
            } => {
                let tail = if queued { " (queued)" } else { "" };
                pr_merged_event(*number, head_ref, tail)
            }
            CiTarget::HeadSha { .. } => {
                if queued {
                    "ci-wait: merged #0 (queued)".to_string()
                } else {
                    "ci-wait: merged #0".to_string()
                }
            }
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
    // One collapse for the whole tick. The pull-request decision, both
    // `set_key` targets, and the route borrow this binding. `interpret_runs`
    // still collapses on its own for a HeadSha decision.
    let (collapsed, disagreed) = collapse_snapshot(&snap);
    let decision = match &target {
        CiTarget::PullRequest { .. } => annotate_disagreed(
            interpret_pr_inner(&collapsed, record.auto_merge),
            &disagreed,
        ),
        CiTarget::HeadSha { .. } => interpret_runs(&snap, phase_elapsed),
    };
    let summary = decision.summary();
    let key = set_key(&target, fail_set(&collapsed, record.auto_merge));
    let changed = state.ci.as_ref().and_then(|c| c.set_key.as_deref()) != Some(key.as_str());
    let interval = next_interval_ms(phase_elapsed, changed);

    persist_watch(record, None, |ci| {
        apply_target(ci, &target);
        stamp_poll(ci, now, &summary, &key, interval);
    })?;

    match decision {
        Decision::Pending { event, .. } => {
            let event = if let Some(n) = just_opened {
                format!("ci-wait: opened #{n}")
            } else {
                event
            };
            persist_watch(record, Some(&event), |_| {})?;
            Ok(None)
        }
        Decision::Fail { message, .. } => {
            match fix::try_route_ci_failure(
                record,
                state,
                &target,
                fail_set(&collapsed, record.auto_merge),
            )? {
                fix::RouteOutcome::Routed(view) => Ok(Some(*view)),
                fix::RouteOutcome::Exhausted => apply_failure(
                    record,
                    state,
                    FailureClass::CiFailed,
                    format!("{message}; address-ci exhausted (2/2)"),
                ),
                fix::RouteOutcome::Declined => {
                    apply_failure(record, state, FailureClass::CiFailed, message)
                }
            }
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
            number,
            head_oid,
            head_ref,
            ..
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
            let merge_gate = match head_oid.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(oid) => crate::policy::decide(record, state, cwd, oid),
                None => crate::policy::unresolved_tip_block(record),
            };
            if let MutationGate::Stop(view) = release_for_mutation(record, state, &merge_gate)? {
                return Ok(view.map(|view| *view));
            }
            let merge = match backend.squash_merge(cwd, *number, head_oid.as_deref()) {
                Ok(m) => m,
                Err(e) => return classify_backend_err(record, state, e),
            };
            if !merge.ok {
                if is_policy_block_merge(&merge.message) {
                    persist_watch(
                        record,
                        Some("ci-wait: waiting (merge blocked by base branch policy)"),
                        |_| {},
                    )?;
                    return Ok(None);
                }
                return apply_failure(
                    record,
                    state,
                    FailureClass::CiFailed,
                    format!("ci-wait: merge failed: {}", truncate_msg(&merge.message)),
                );
            }
            let (field, event) = if merge.queued {
                ("queued", pr_merged_event(*number, head_ref, " (queued)"))
            } else {
                ("done", pr_merged_event(*number, head_ref, ""))
            };
            persist_watch(record, None, |ci| {
                ci.merge = Some(field.into());
            })?;
            match apply_success(record, state, event.clone(), OutcomeSource::Adapter) {
                Ok(v) => Ok(v),
                Err(e) => {
                    let msg = pr_merged_event(
                        *number,
                        head_ref,
                        &format!(" (state apply failed: {})", truncate_msg(&e.to_string())),
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

/// One effective bucket per check name (same SHA). Recency is not a tiebreaker.
fn collapse_by_name(items: &[CheckItem]) -> (Vec<CheckItem>, Vec<String>) {
    use std::collections::BTreeMap;
    let mut by_name: BTreeMap<&str, Vec<&CheckItem>> = BTreeMap::new();
    for i in items {
        by_name.entry(i.name.as_str()).or_default().push(i);
    }
    let mut out = Vec::with_capacity(by_name.len());
    let mut disagreed = Vec::new();
    for (name, group) in by_name {
        let has_pass = group.iter().any(|i| i.bucket == CheckBucket::Pass);
        let has_fail = group.iter().any(|i| i.bucket == CheckBucket::Fail);
        let has_pending = group.iter().any(|i| i.bucket == CheckBucket::Pending);
        let has_cancel = group.iter().any(|i| i.bucket == CheckBucket::Cancel);
        let bucket = if has_pass {
            CheckBucket::Pass
        } else if has_pending {
            CheckBucket::Pending
        } else if has_cancel {
            CheckBucket::Cancel
        } else if has_fail {
            CheckBucket::Fail
        } else {
            CheckBucket::Skipping
        };
        if has_pass && has_fail {
            disagreed.push(name.to_string());
        }
        let source = group
            .iter()
            .copied()
            .find(|i| i.bucket == bucket)
            .unwrap_or(group[0]);
        out.push(CheckItem {
            name: name.to_string(),
            bucket,
            description: source.description.clone(),
            link: source.link.clone(),
        });
    }
    (out, disagreed)
}

pub(crate) fn collapse_snapshot(snap: &CheckSnapshot) -> (CheckSnapshot, Vec<String>) {
    let (items, mut disagreed) = collapse_by_name(&snap.items);
    let (advisory, adv_disagreed) = collapse_by_name(&snap.advisory);
    disagreed.extend(adv_disagreed);
    disagreed.sort_unstable();
    disagreed.dedup();
    (
        CheckSnapshot {
            items,
            raw_exit: snap.raw_exit,
            merge_state: snap.merge_state,
            view: snap.view,
            advisory,
        },
        disagreed,
    )
}

fn annotate_disagreed(d: Decision, disagreed: &[String]) -> Decision {
    if disagreed.is_empty() {
        return d;
    }
    let tag = format!(" (disagreed: {})", disagreed.join(", "));
    match d {
        Decision::Green { summary } => Decision::Green {
            summary: format!("{summary}{tag}"),
        },
        Decision::Fail { message, summary } => Decision::Fail {
            message: format!("{message}{tag}"),
            summary: format!("{summary}{tag}"),
        },
        Decision::Pending { event, summary } => Decision::Pending {
            event: truncate_msg(&format!("{event}{tag}")),
            summary: format!("{summary}{tag}"),
        },
    }
}

/// PR buckets. Caller must already have rejected draft / already-merged.
fn interpret_pr(snap: &CheckSnapshot, auto_merge: bool) -> Decision {
    let (collapsed, disagreed) = collapse_snapshot(snap);
    annotate_disagreed(interpret_pr_inner(&collapsed, auto_merge), &disagreed)
}

fn interpret_pr_inner(snap: &CheckSnapshot, auto_merge: bool) -> Decision {
    match gate_slice(snap, auto_merge) {
        GateSlice::Pending => required_pending(snap),
        GateSlice::Judge(items) => {
            let d = interpret_items(snap, items);
            if snap.view == CheckView::Required && !snap.items.is_empty() {
                match d {
                    Decision::Green { summary } => {
                        apply_required_merge_gate(snap.merge_state, auto_merge, summary)
                    }
                    other => other,
                }
            } else {
                d
            }
        }
    }
}

fn apply_required_merge_gate(
    merge_state: MergeStateStatus,
    auto_merge: bool,
    summary: String,
) -> Decision {
    match merge_state {
        MergeStateStatus::Clean | MergeStateStatus::Unstable | MergeStateStatus::HasHooks => {
            Decision::Green { summary }
        }
        MergeStateStatus::Unspecified => Decision::Green { summary },
        MergeStateStatus::Blocked if !auto_merge => Decision::Green { summary },
        _ => Decision::Pending {
            event: format!("ci-wait: waiting ({summary})"),
            summary,
        },
    }
}

fn is_policy_block_merge(message: &str) -> bool {
    let s = message.to_ascii_lowercase();
    s.contains("policy prohibits the merge")
        || s.contains("base branch policy")
        || s.contains("required status check")
}

fn required_pending(snap: &CheckSnapshot) -> Decision {
    let summary = decision_summary(snap);
    Decision::Pending {
        event: format!("ci-wait: waiting ({summary})"),
        summary,
    }
}

fn interpret_items(snap: &CheckSnapshot, items: &[CheckItem]) -> Decision {
    let summary = decision_summary(snap);
    if items.iter().any(|i| matches!(i.bucket, CheckBucket::Fail)) {
        return Decision::Fail {
            message: format!("ci-wait: checks failed ({summary})"),
            summary,
        };
    }
    if items
        .iter()
        .any(|i| matches!(i.bucket, CheckBucket::Cancel))
    {
        let mut names: Vec<&str> = items
            .iter()
            .filter(|i| i.bucket == CheckBucket::Cancel)
            .map(|i| i.name.as_str())
            .collect();
        names.sort_unstable();
        names.dedup();
        return Decision::Pending {
            event: truncate_msg(&format!(
                "ci-wait: waiting (cancelled: {}) ({summary})",
                names.join(", ")
            )),
            summary,
        };
    }
    if items
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

pub(crate) enum GateSlice<'a> {
    Judge(&'a [CheckItem]),
    Pending,
}

pub(crate) fn gate_slice<'a>(snap: &'a CheckSnapshot, auto_merge: bool) -> GateSlice<'a> {
    match snap.view {
        CheckView::Unspecified => GateSlice::Judge(&snap.items),
        CheckView::Required if !snap.items.is_empty() => GateSlice::Judge(&snap.items),
        CheckView::Required if uses_advisory_fallback(snap, auto_merge) => {
            GateSlice::Judge(&snap.advisory)
        }
        _ => GateSlice::Pending,
    }
}

fn uses_advisory_fallback(snap: &CheckSnapshot, auto_merge: bool) -> bool {
    if snap.view != CheckView::Required || !snap.items.is_empty() {
        return false;
    }
    match snap.merge_state {
        MergeStateStatus::Clean | MergeStateStatus::Unstable | MergeStateStatus::HasHooks => true,
        MergeStateStatus::Blocked if !auto_merge && !snap.advisory.is_empty() => true,
        _ => false,
    }
}

fn fail_set(snap: &CheckSnapshot, auto_merge: bool) -> &[CheckItem] {
    match gate_slice(snap, auto_merge) {
        GateSlice::Judge(items) => items,
        GateSlice::Pending => &snap.items,
    }
}

fn decision_summary(snap: &CheckSnapshot) -> String {
    if snap.view == CheckView::Required && !snap.advisory.is_empty() {
        let required = if snap.items.is_empty() {
            "0 required".into()
        } else {
            summarize(&snap.items)
        };
        return format!("{required}; {}", advisory_tail(&snap.advisory));
    }
    summarize(&snap.items)
}

fn advisory_tail(items: &[CheckItem]) -> String {
    let mut fail = 0u32;
    let mut pending = 0u32;
    let mut cancel = 0u32;
    for i in items {
        match i.bucket {
            CheckBucket::Fail => fail += 1,
            CheckBucket::Pending => pending += 1,
            CheckBucket::Cancel => cancel += 1,
            _ => {}
        }
    }
    let mut parts = Vec::new();
    if fail > 0 {
        parts.push(format!("{fail} advisory fail"));
    }
    if pending > 0 {
        parts.push(format!("{pending} advisory pending"));
    }
    if cancel > 0 {
        parts.push(format!("{cancel} advisory cancel"));
    }
    if parts.is_empty() {
        "advisory".into()
    } else {
        parts.join(", ")
    }
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
    interpret_pr(snap, true)
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
    let track_id = state.track_id.as_deref();
    if let Some(ci) = state.ci.as_ref()
        && let Some(n) = ci.pr_number
    {
        let hinted = PrHint {
            number: Some(n),
            url: ci.pr_url.clone(),
        };
        // A stored number is tried once. A dropped PullRequest or a miss gets one
        // unhinted resolve. A dropped HeadSha does not. Never a third call.
        match backend.resolve_pr(cwd, Some(&hinted), track_id)? {
            first @ Some(CiTarget::HeadSha { .. }) => {
                return Ok(accept_resolved_target(state, cwd, first));
            }
            Some(pr @ CiTarget::PullRequest { .. }) => {
                let kept = accept_resolved_target(state, cwd, Some(pr));
                if kept.is_some() {
                    return Ok(kept);
                }
            }
            None => {}
        }
        let resolved = backend.resolve_pr(cwd, None, track_id)?;
        return Ok(accept_resolved_target(state, cwd, resolved));
    }
    let resolved = backend.resolve_pr(cwd, hint, track_id)?;
    Ok(accept_resolved_target(state, cwd, resolved))
}

/// HeadSha is legal only when no local `track/NNNN-*` tip is ahead of that sha.
fn accept_resolved_target(
    state: &RunState,
    cwd: &Path,
    target: Option<CiTarget>,
) -> Option<CiTarget> {
    match target {
        Some(CiTarget::HeadSha { sha }) => {
            let Some(numeric) = state
                .track_id
                .as_deref()
                .and_then(crate::notify::artifact::numeric_track_id)
            else {
                return Some(CiTarget::HeadSha { sha });
            };
            if gh::track_tip_blocks_head_sha(cwd, numeric, &sha) {
                None
            } else {
                Some(CiTarget::HeadSha { sha })
            }
        }
        Some(pr @ CiTarget::PullRequest { .. }) => {
            let CiTarget::PullRequest { head_ref, .. } = &pr else {
                return None;
            };
            let Some(numeric) = state
                .track_id
                .as_deref()
                .and_then(crate::notify::artifact::numeric_track_id)
            else {
                return Some(pr);
            };
            if gh::branch_is_track(head_ref, numeric) {
                Some(pr)
            } else {
                None
            }
        }
        None => None,
    }
}

/// Adapter merged-PR probe. Tests never construct a live `GhMergedTrackProbe`.
#[cfg(test)]
fn ci_merged_probe() -> Option<Arc<dyn MergedTrackProbe>> {
    crate::workflow::shipped::installed_merged_probe()
}

#[cfg(not(test))]
fn ci_merged_probe() -> Option<GhMergedTrackProbe> {
    Some(GhMergedTrackProbe::default())
}

enum AutoPublishDrive {
    Opened(CiTarget),
    Wait,
    /// Publish exhaustion. Not an `Err`, so `classify_backend_err` cannot swallow it.
    /// Boxed like `MutationGate::Stop` so the enum stays small.
    Stopped(Option<Box<StatusView>>),
}

fn publish_already_attempted(state: &RunState, cwd: &Path) -> bool {
    let Some(latched) = state
        .ci
        .as_ref()
        .and_then(|c| c.publish_attempted_sha.as_deref())
        .filter(|s| !s.is_empty())
    else {
        return false;
    };
    match gh::git_head_sha(cwd) {
        Some(head) => latched == head,
        None => true,
    }
}

fn latch_sha_from_target(target: &CiTarget) -> Option<String> {
    match target {
        CiTarget::PullRequest {
            head_oid, number, ..
        } => head_oid.clone().or_else(|| Some(format!("pr:{number}"))),
        CiTarget::HeadSha { sha } => Some(sha.clone()),
    }
}

enum MutationGate {
    Go,
    /// `None` is a hold (stay Running). `Some` is a permission failure view.
    Stop(Option<Box<StatusView>>),
}

/// Allow and Report clear the restore flag and fall through. Hold stays in `ci-wait`.
fn release_for_mutation(
    record: &ProjectRecord,
    state: &RunState,
    decision: &crate::policy::GateDecision,
) -> Result<MutationGate> {
    match decision {
        crate::policy::GateDecision::Block(gate) => {
            crate::policy::remember_gate(record, Some(gate))?;
            let view = apply_failure(record, state, FailureClass::Permission, gate.detail.clone())?;
            Ok(MutationGate::Stop(view.map(Box::new)))
        }
        crate::policy::GateDecision::Hold(gate) => {
            persist_policy_hold(record, gate)?;
            Ok(MutationGate::Stop(None))
        }
        crate::policy::GateDecision::Report(gate) => {
            crate::policy::remember_gate(record, Some(gate))?;
            crate::policy::consume_restore(record);
            Ok(MutationGate::Go)
        }
        crate::policy::GateDecision::Allow => {
            crate::policy::remember_gate(record, None)?;
            crate::policy::consume_restore(record);
            Ok(MutationGate::Go)
        }
    }
}

fn persist_policy_hold(record: &ProjectRecord, gate: &crate::policy::StateGate) -> Result<()> {
    with_run_state_lock(record, || {
        let mut stored = load_run_state(record)?;
        stored.state_gate = Some(gate.clone());
        stored.last_event = gate.detail.clone();
        stored.updated_at = Utc::now();
        save_run_state(record, &stored)
    })
}

/// `Some` head from the publish result is the counter key. `None` reads HEAD once.
/// A missing read does not reset the previous head's count.
fn next_transient_attempt(
    state: &RunState,
    cwd: &Path,
    attempted_sha: Option<String>,
) -> (u32, Option<String>) {
    let prev_attempts = state
        .ci
        .as_ref()
        .map(|c| c.publish_transient_attempts)
        .unwrap_or(0);
    let prev_sha = state
        .ci
        .as_ref()
        .and_then(|c| c.publish_transient_sha.clone());
    let key = match attempted_sha {
        Some(sha) => Some(sha),
        None => gh::git_head_sha(cwd),
    };
    let mut attempts = prev_attempts;
    if let Some(ref k) = key
        && prev_sha.as_deref() != Some(k.as_str())
    {
        attempts = 0;
    }
    (attempts.saturating_add(1), key)
}

fn drive_retryable_skip(
    record: &ProjectRecord,
    state: &RunState,
    cwd: &Path,
    now: chrono::DateTime<Utc>,
    event: &str,
    attempted_sha: Option<String>,
) -> Result<AutoPublishDrive> {
    let (attempts, key) = next_transient_attempt(state, cwd, attempted_sha);
    if attempts >= PUBLISH_TRANSIENT_CAP {
        persist_watch(record, None, |ci| {
            ci.publish_transient_attempts = attempts;
            if let Some(ref k) = key {
                ci.publish_transient_sha = Some(k.clone());
            }
        })?;
        let view = apply_failure(
            record,
            state,
            FailureClass::CiFailed,
            format!("ci-wait: publish exhausted ({attempts}/{PUBLISH_TRANSIENT_CAP}): {event}"),
        )?;
        return Ok(AutoPublishDrive::Stopped(view.map(Box::new)));
    }
    let summary = format!("publish retry {attempts}");
    persist_watch(record, Some(event), |ci| {
        ci.head_sha = None;
        ci.publish_transient_attempts = attempts;
        if let Some(ref k) = key {
            ci.publish_transient_sha = Some(k.clone());
        }
        stamp_poll(
            ci,
            now,
            &summary,
            "wait-pr",
            next_interval_ms(elapsed(state, now), false),
        );
    })?;
    Ok(AutoPublishDrive::Wait)
}

fn try_auto_publish_target(
    record: &ProjectRecord,
    state: &RunState,
    backend: &dyn CiBackend,
    cwd: &Path,
    now: chrono::DateTime<Utc>,
) -> Result<AutoPublishDrive> {
    let Some(numeric) = state
        .track_id
        .as_deref()
        .and_then(crate::notify::artifact::numeric_track_id)
    else {
        persist_watch(record, Some("ci-wait: waiting for PR"), |ci| {
            ci.head_sha = None;
            stamp_poll(
                ci,
                now,
                "waiting for PR",
                "wait-pr",
                next_interval_ms(elapsed(state, now), false),
            );
        })?;
        return Ok(AutoPublishDrive::Wait);
    };
    if publish_already_attempted(state, cwd) {
        persist_watch(
            record,
            Some("ci-wait: publish attempted — waiting for PR"),
            |ci| {
                ci.head_sha = None;
                stamp_poll(
                    ci,
                    now,
                    "publish attempted",
                    "wait-pr",
                    next_interval_ms(elapsed(state, now), false),
                );
            },
        )?;
        return Ok(AutoPublishDrive::Wait);
    }
    match backend.try_auto_publish(cwd, numeric) {
        Ok(AutoPublishResult::Opened(target)) => {
            let n = match &target {
                CiTarget::PullRequest { number, .. } => *number,
                CiTarget::HeadSha { .. } => 0,
            };
            if let Some(id) = state.track_id.as_deref() {
                crate::workflow::reuse::note_ci_target(record, id, &target);
            }
            persist_watch(record, Some(&format!("ci-wait: opened #{n}")), |ci| {
                apply_target(ci, &target);
                ci.publish_attempted_sha = latch_sha_from_target(&target);
                ci.publish_transient_attempts = 0;
                ci.publish_transient_sha = None;
                stamp_poll(
                    ci,
                    now,
                    &format!("opened #{n}"),
                    "wait-pr",
                    next_interval_ms(elapsed(state, now), false),
                );
            })?;
            Ok(AutoPublishDrive::Opened(target))
        }
        Ok(AutoPublishResult::Skipped {
            event,
            attempted_sha,
            retryable,
        }) => {
            if retryable {
                return drive_retryable_skip(record, state, cwd, now, &event, attempted_sha);
            }
            persist_watch(record, Some(&event), |ci| {
                ci.head_sha = None;
                if let Some(sha) = attempted_sha {
                    ci.publish_attempted_sha = Some(sha);
                }
                stamp_poll(
                    ci,
                    now,
                    "waiting for PR",
                    "wait-pr",
                    next_interval_ms(elapsed(state, now), false),
                );
            })?;
            Ok(AutoPublishDrive::Wait)
        }
        Err(e) => Err(e),
    }
}

/// Title-probe fallback when `resolve_pr` is still `None`. Fail-open on Err.
fn try_merged_track_target(state: &RunState, cwd: &Path) -> Option<CiTarget> {
    let numeric = state
        .track_id
        .as_deref()
        .and_then(crate::notify::artifact::numeric_track_id)?;
    let n = ci_merged_probe().and_then(|p| p.merged_pr_for_track(cwd, numeric).ok().flatten())?;
    let confirmed = gh::confirm_track_pr(cwd, n, numeric).ok().flatten()?;
    accept_resolved_target(state, cwd, Some(confirmed))
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
    if let Ok(state) = load_run_state(record)
        && let Some(id) = state.track_id.as_deref()
    {
        crate::workflow::reuse::note_ci_target(record, id, target);
    }
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

fn pr_merged_event(number: u64, head_ref: &str, tail: &str) -> String {
    format!("ci-wait: merged #{number} ({head_ref}){tail}")
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
    if msg.contains("git timed out") {
        persist_watch(record, Some("ci-wait: git timed out"), |ci| {
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

/// Read-only green open PR. Empty checks plus Clean, Unstable, or HasHooks is green.
/// Unspecified and Blocked do not qualify. Never merges and never publishes.
pub(crate) fn open_pr_is_green(
    state: &RunState,
    backend: &dyn CiBackend,
    cwd: &Path,
) -> Result<Option<u64>> {
    let Some(target) = resolve_target(state, backend, cwd, pr_hint(state).as_ref())? else {
        return Ok(None);
    };
    let number = match &target {
        CiTarget::PullRequest {
            number,
            is_draft,
            merged,
            merge_state,
            ..
        } => {
            if *is_draft || *merged {
                return Ok(None);
            }
            if !matches!(
                merge_state,
                MergeStateStatus::Clean | MergeStateStatus::Unstable | MergeStateStatus::HasHooks
            ) {
                return Ok(None);
            }
            *number
        }
        CiTarget::HeadSha { .. } => return Ok(None),
    };
    let snap = backend.checks(cwd, &target)?;
    let (collapsed, _) = collapse_snapshot(&snap);
    // Clean, Unstable, and HasHooks ignore `auto_merge`, so `true` matches the fallback.
    let items = match gate_slice(&collapsed, true) {
        GateSlice::Judge(items) => items,
        GateSlice::Pending => collapsed.items.as_slice(),
    };
    match interpret_items(&collapsed, items) {
        Decision::Green { .. } => Ok(Some(number)),
        _ => Ok(None),
    }
}

fn lookup_green_open_pr(record: &ProjectRecord) -> Result<Option<u64>> {
    #[cfg(test)]
    {
        let _ = record;
        let forced = *TEST_OPEN_GREEN_PR.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(value) = forced {
            return Ok(value);
        }
        Ok(None)
    }
    #[cfg(not(test))]
    {
        let state = load_run_state(record)?;
        let Some(cwd) = crate::worktree::product_git_cwd(record) else {
            return Ok(None);
        };
        open_pr_is_green(&state, &GhCli, &cwd)
    }
}

pub(crate) fn with_green_pr_prefix(record: &ProjectRecord, phase: &str, message: String) -> String {
    if phase != crate::workflow::graph::PHASE_IMPLEMENT
        && phase != crate::workflow::graph::PHASE_ADDRESS_FINDINGS
    {
        return message;
    }
    match lookup_green_open_pr(record) {
        Ok(Some(n)) => format!("PR #{n} green; merge not performed \u{2014} {message}"),
        _ => message,
    }
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
            phase_timeouts_secs: Default::default(),
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
        pr_state(n, draft, merged, MergeStateStatus::Unspecified)
    }

    fn pr_with_head(n: u64, draft: bool, merged: bool, head: &str) -> CiTarget {
        let mut target = pr(n, draft, merged);
        if let CiTarget::PullRequest { head_ref, .. } = &mut target {
            *head_ref = head.into();
        }
        target
    }

    fn pr_state(n: u64, draft: bool, merged: bool, merge_state: MergeStateStatus) -> CiTarget {
        CiTarget::PullRequest {
            number: n,
            url: format!("https://example/pr/{n}"),
            is_draft: draft,
            merged,
            head_oid: Some("abc".into()),
            merge_state,
            head_ref: "track/0010-Fixture".into(),
            title: String::new(),
        }
    }

    fn pair_items(pairs: &[(&str, CheckBucket)]) -> Vec<CheckItem> {
        pairs.iter().map(|(n, b)| CheckItem::new(*n, *b)).collect()
    }

    fn items(pairs: &[(&str, CheckBucket)]) -> CheckSnapshot {
        CheckSnapshot {
            items: pair_items(pairs),
            raw_exit: 0,
            ..CheckSnapshot::empty()
        }
    }

    fn required_snap(
        pairs: &[(&str, CheckBucket)],
        advisory: &[(&str, CheckBucket)],
        merge: MergeStateStatus,
    ) -> CheckSnapshot {
        CheckSnapshot {
            items: pair_items(pairs),
            raw_exit: 0,
            merge_state: merge,
            view: CheckView::Required,
            advisory: pair_items(advisory),
        }
    }

    fn hook(scripted: ScriptedBackend) -> (TestBackendGuard, CallCounts) {
        let rec = RecordingBackend::wrap(Arc::new(scripted));
        let counts = rec.counts.clone();
        let g = install_test_backend(Arc::new(rec));
        (g, counts)
    }

    fn recorded(target: CiTarget, snap: CheckSnapshot) -> RecordingBackend {
        RecordingBackend::wrap(Arc::new(ScriptedBackend::new().with_pr(target, snap)))
    }

    fn required_clean(pairs: &[(&str, CheckBucket)], merge: MergeStateStatus) -> CheckSnapshot {
        CheckSnapshot {
            items: pair_items(pairs),
            raw_exit: 0,
            merge_state: merge,
            view: CheckView::Required,
            advisory: Vec::new(),
        }
    }

    #[test]
    fn open_pr_is_green_is_read_only() {
        let dir = tempdir().unwrap();
        let state = crate::state::RunState::idle("p");
        let cwd = dir.path();
        for merge in [
            MergeStateStatus::Clean,
            MergeStateStatus::Unstable,
            MergeStateStatus::HasHooks,
        ] {
            let backend = recorded(
                pr_state(505, false, false, merge),
                required_clean(&[("ci", CheckBucket::Pass)], merge),
            );
            assert_eq!(
                open_pr_is_green(&state, &backend, cwd).unwrap(),
                Some(505),
                "{merge:?}"
            );
            assert_eq!(backend.counts.merge_n(), 0);
            assert_eq!(backend.counts.publish_n(), 0);
        }
        let empty = recorded(
            pr_state(9, false, false, MergeStateStatus::Clean),
            required_clean(&[], MergeStateStatus::Clean),
        );
        assert_eq!(open_pr_is_green(&state, &empty, cwd).unwrap(), Some(9));
        assert_eq!(empty.counts.merge_n(), 0);
        assert_eq!(empty.counts.publish_n(), 0);

        let negatives = [
            pr_state(1, true, false, MergeStateStatus::Clean),
            pr_state(1, false, true, MergeStateStatus::Clean),
            pr_state(1, false, false, MergeStateStatus::Unspecified),
            pr_state(1, false, false, MergeStateStatus::Blocked),
        ];
        for target in negatives {
            let backend = recorded(
                target,
                required_clean(&[("ci", CheckBucket::Pass)], MergeStateStatus::Clean),
            );
            assert_eq!(open_pr_is_green(&state, &backend, cwd).unwrap(), None);
            assert_eq!(backend.counts.merge_n(), 0);
            assert_eq!(backend.counts.publish_n(), 0);
        }
        for bucket in [CheckBucket::Pending, CheckBucket::Cancel, CheckBucket::Fail] {
            let backend = recorded(
                pr_state(1, false, false, MergeStateStatus::Clean),
                required_clean(&[("ci", bucket)], MergeStateStatus::Clean),
            );
            assert_eq!(
                open_pr_is_green(&state, &backend, cwd).unwrap(),
                None,
                "{bucket:?}"
            );
            assert_eq!(backend.counts.merge_n(), 0);
            assert_eq!(backend.counts.publish_n(), 0);
        }
        let missing = RecordingBackend::wrap(Arc::new(ScriptedBackend::new()));
        assert_eq!(open_pr_is_green(&state, &missing, cwd).unwrap(), None);
        assert_eq!(missing.counts.merge_n(), 0);
        assert_eq!(missing.counts.publish_n(), 0);
        let broken = ScriptedBackend::new();
        broken.push_resolve(Err(crate::error::CoordinatorError::Message(
            "gh down".into(),
        )));
        let broken = RecordingBackend::wrap(Arc::new(broken));
        assert!(open_pr_is_green(&state, &broken, cwd).is_err());
        assert_eq!(broken.counts.merge_n(), 0);
        assert_eq!(broken.counts.publish_n(), 0);
    }

    #[test]
    fn green_pr_prefix_only_for_implement_and_address_findings() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        let _guard = set_test_open_green_pr(Some(505));
        let msg = "ACP stdout closed during session/prompt; child=unknown; stderr=".to_string();
        let prefix = "PR #505 green; merge not performed \u{2014} ";
        let implement = with_green_pr_prefix(&r, "implement", msg.clone());
        assert!(implement.starts_with(prefix), "{implement}");
        let address = with_green_pr_prefix(&r, "address-findings", msg.clone());
        assert!(address.starts_with(prefix), "{address}");
        let plan = with_green_pr_prefix(&r, "plan", msg.clone());
        assert!(!plan.contains("merge not performed"), "{plan}");
        assert!(plan.starts_with("ACP stdout closed"), "{plan}");
        let address_ci = with_green_pr_prefix(&r, "address-ci", msg);
        assert!(!address_ci.contains("merge not performed"), "{address_ci}");
    }

    #[test]
    fn interpret_fail_cancel_pending_green_empty() {
        assert!(matches!(
            interpret_pr(&items(&[("a", CheckBucket::Fail)]), true),
            Decision::Fail { .. }
        ));
        assert!(matches!(
            interpret_pr(&items(&[("a", CheckBucket::Cancel)]), true),
            Decision::Pending { event, .. } if event == "ci-wait: waiting (cancelled: a) (1 cancel)"
        ));
        assert!(matches!(
            interpret_pr(
                &items(&[("b", CheckBucket::Cancel), ("a", CheckBucket::Cancel)]),
                true
            ),
            Decision::Pending { event, .. } if event == "ci-wait: waiting (cancelled: a, b) (2 cancel)"
        ));
        assert!(matches!(
            interpret_pr(
                &items(&[("a", CheckBucket::Cancel), ("b", CheckBucket::Fail)]),
                true
            ),
            Decision::Fail { message, .. } if message.contains("checks failed")
        ));
        assert!(matches!(
            interpret_pr(
                &items(&[("a", CheckBucket::Cancel), ("b", CheckBucket::Pending)]),
                true
            ),
            Decision::Pending { event, .. } if event == "ci-wait: waiting (cancelled: a) (1 pending, 1 cancel)"
        ));
        assert!(matches!(
            interpret_pr(
                &items(&[("a", CheckBucket::Pass), ("b", CheckBucket::Pending)]),
                true
            ),
            Decision::Pending { .. }
        ));
        assert!(matches!(
            interpret_pr(
                &items(&[("a", CheckBucket::Pass), ("b", CheckBucket::Skipping)]),
                true
            ),
            Decision::Green { .. }
        ));
        assert!(matches!(
            interpret_pr(&CheckSnapshot::empty(), true),
            Decision::Green { .. }
        ));
        assert_eq!(
            summarize(&pair_items(&[("a", CheckBucket::Cancel)])),
            "1 cancel"
        );
        assert_eq!(
            advisory_tail(&pair_items(&[("bot", CheckBucket::Cancel)])),
            "1 advisory cancel"
        );
        assert!(matches!(
            interpret_pr(
                &items(&[("fmt", CheckBucket::Cancel), ("fmt", CheckBucket::Cancel)]),
                true
            ),
            Decision::Pending { event, .. }
                if event == "ci-wait: waiting (cancelled: fmt) (1 cancel)"
        ));
        assert_eq!(
            summarize(&pair_items(&[
                ("fmt", CheckBucket::Cancel),
                ("fmt", CheckBucket::Cancel)
            ])),
            "2 cancel"
        );
        let long = "c".repeat(180);
        match interpret_pr(&items(&[(&long, CheckBucket::Cancel)]), true) {
            Decision::Pending { event, summary } => {
                assert!(event.starts_with("ci-wait: waiting (cancelled: "));
                assert_eq!(event.chars().count(), LAST_EVENT_MESSAGE_CAP + 1);
                assert!(event.ends_with('…'));
                assert_eq!(summary, "1 cancel");
            }
            other => panic!("expected pending, got {other:?}"),
        }
    }

    #[test]
    fn interpret_duplicate_name_pass_fail_is_green() {
        for pairs in [
            &[("fmt", CheckBucket::Fail), ("fmt", CheckBucket::Pass)][..],
            &[("fmt", CheckBucket::Pass), ("fmt", CheckBucket::Fail)][..],
        ] {
            match interpret_pr(&items(pairs), true) {
                Decision::Green { summary } => {
                    assert!(summary.contains("disagreed: fmt"), "summary={summary}");
                    assert!(
                        !summary.contains("fail"),
                        "collapsed summary must not count the suppressed fail: {summary}"
                    );
                }
                other => panic!("expected green, got {other:?}"),
            }
        }
        assert!(matches!(
            interpret_pr(&items(&[("fmt", CheckBucket::Fail)]), true),
            Decision::Fail { .. }
        ));
        assert!(matches!(
            interpret_pr(
                &items(&[("fmt", CheckBucket::Fail), ("fmt", CheckBucket::Fail)]),
                true
            ),
            Decision::Fail { .. }
        ));
        assert!(matches!(
            interpret_pr(
                &items(&[("fmt", CheckBucket::Fail), ("bot", CheckBucket::Pass)]),
                true
            ),
            Decision::Fail { .. }
        ));
    }

    #[test]
    fn interpret_duplicate_name_fail_pending_stays_pending() {
        match interpret_pr(
            &items(&[("fmt", CheckBucket::Fail), ("fmt", CheckBucket::Pending)]),
            true,
        ) {
            Decision::Pending { event, summary } => {
                assert!(event.contains("waiting"), "event={event}");
                assert!(!event.contains("checks failed"), "event={event}");
                assert!(!summary.contains("disagreed"), "summary={summary}");
            }
            other => panic!("expected pending, got {other:?}"),
        }
    }

    #[test]
    fn interpret_duplicate_name_pass_cancel_is_green() {
        match interpret_pr(
            &items(&[("fmt", CheckBucket::Pass), ("fmt", CheckBucket::Cancel)]),
            true,
        ) {
            Decision::Green { summary } => {
                assert!(!summary.contains("cancel"), "summary={summary}");
                assert!(!summary.contains("disagreed"), "summary={summary}");
            }
            other => panic!("expected green (pass dominates cancel), got {other:?}"),
        }
    }

    #[test]
    fn interpret_duplicate_name_fail_cancel_stays_pending() {
        match interpret_pr(
            &items(&[("fmt", CheckBucket::Fail), ("fmt", CheckBucket::Cancel)]),
            true,
        ) {
            Decision::Pending { event, .. } => {
                assert!(event.contains("cancelled: fmt"), "event={event}");
                assert!(!event.contains("checks failed"), "event={event}");
            }
            other => panic!("expected pending cancel, got {other:?}"),
        }
    }

    #[test]
    fn interpret_duplicate_name_fail_pass_fail_is_green() {
        match interpret_pr(
            &items(&[
                ("fmt", CheckBucket::Fail),
                ("fmt", CheckBucket::Pass),
                ("fmt", CheckBucket::Fail),
            ]),
            true,
        ) {
            Decision::Green { summary } => {
                assert!(summary.contains("disagreed: fmt"), "summary={summary}");
                assert!(
                    !summary.contains("fail"),
                    "collapsed summary must not count suppressed fails: {summary}"
                );
            }
            other => panic!("expected green (pass wins among three twins), got {other:?}"),
        }
    }

    #[test]
    fn interpret_runs_duplicate_name_pass_fail_is_green() {
        match interpret_runs(
            &items(&[("fmt", CheckBucket::Fail), ("fmt", CheckBucket::Pass)]),
            Duration::from_secs(30),
        ) {
            Decision::Green { summary } => {
                assert!(summary.contains("disagreed: fmt"), "summary={summary}");
                assert!(
                    !summary.contains("fail"),
                    "HeadSha path must collapse via interpret_pr: {summary}"
                );
            }
            other => panic!("expected green via interpret_runs, got {other:?}"),
        }
    }

    #[test]
    fn interpret_required_empty_clean_advisory_pass_fail_disagreed() {
        match interpret_pr(
            &required_snap(
                &[],
                &[
                    ("fmt clippy test", CheckBucket::Fail),
                    ("fmt clippy test", CheckBucket::Pass),
                ],
                MergeStateStatus::Clean,
            ),
            true,
        ) {
            Decision::Green { summary } => {
                assert!(summary.contains("0 required"), "summary={summary}");
                assert!(
                    summary.contains("disagreed: fmt clippy test"),
                    "summary={summary}"
                );
                assert!(!summary.contains("advisory fail"), "summary={summary}");
            }
            other => panic!("expected green, got {other:?}"),
        }
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
    fn interpret_required_empty_unknown_is_pending() {
        let snap = required_snap(&[], &[], MergeStateStatus::Unknown);
        assert!(matches!(
            interpret_pr(&snap, true),
            Decision::Pending { .. }
        ));
        assert!(matches!(
            interpret_pr(&snap, false),
            Decision::Pending { .. }
        ));
    }

    #[test]
    fn interpret_required_empty_clean_falls_back_to_advisory() {
        assert!(matches!(
            interpret_pr(&required_snap(&[], &[], MergeStateStatus::Clean), true),
            Decision::Green { .. }
        ));
        assert!(matches!(
            interpret_pr(
                &required_snap(
                    &[],
                    &[("ci", CheckBucket::Pending)],
                    MergeStateStatus::Clean
                ),
                true
            ),
            Decision::Pending { .. }
        ));
        assert!(matches!(
            interpret_pr(&required_snap(&[], &[], MergeStateStatus::Blocked), true),
            Decision::Pending { .. }
        ));
        assert!(matches!(
            interpret_pr(&required_snap(&[], &[], MergeStateStatus::Unstable), true),
            Decision::Green { .. }
        ));
        assert!(matches!(
            interpret_pr(&required_snap(&[], &[], MergeStateStatus::HasHooks), true),
            Decision::Green { .. }
        ));
    }

    #[test]
    fn interpret_required_empty_unspecified_is_pending() {
        let snap = required_snap(&[], &[], MergeStateStatus::Unspecified);
        assert!(matches!(
            interpret_pr(&snap, true),
            Decision::Pending { .. }
        ));
        assert!(matches!(
            interpret_pr(&snap, false),
            Decision::Pending { .. }
        ));
    }

    #[test]
    fn interpret_required_empty_blocked_auto_merge_false_uses_advisory() {
        assert!(matches!(
            interpret_pr(
                &required_snap(
                    &[],
                    &[("ci", CheckBucket::Pending)],
                    MergeStateStatus::Blocked
                ),
                false
            ),
            Decision::Pending { .. }
        ));
        assert!(matches!(
            interpret_pr(
                &required_snap(&[], &[("ci", CheckBucket::Pass)], MergeStateStatus::Blocked),
                false
            ),
            Decision::Green { .. }
        ));
    }

    #[test]
    fn is_policy_block_merge_matches_pinned_substrings() {
        assert!(is_policy_block_merge(
            "X Pull request #1 is not mergeable: the base branch policy prohibits the merge."
        ));
        assert!(is_policy_block_merge("Base branch policy"));
        assert!(is_policy_block_merge(
            "required status check \"fmt\" has not passed"
        ));
        assert!(!is_policy_block_merge(
            "GraphQL: Pull Request is not mergeable"
        ));
    }

    #[test]
    fn interpret_required_green_blocked_depends_on_auto_merge() {
        let snap = required_snap(
            &[("fmt", CheckBucket::Pass)],
            &[],
            MergeStateStatus::Blocked,
        );
        assert!(matches!(
            interpret_pr(&snap, true),
            Decision::Pending { .. }
        ));
        assert!(matches!(interpret_pr(&snap, false), Decision::Green { .. }));
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
            std::env::remove_var(crate::policy::ENV_STATE_POLICIES);
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
    fn required_green_advisory_fail_does_not_ci_failed() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr(329, false, false))));
        s.push_snapshot(Ok(required_snap(
            &[
                ("fmt", CheckBucket::Pass),
                ("clippy", CheckBucket::Pass),
                ("test", CheckBucket::Pass),
                ("deny", CheckBucket::Pass),
                ("semgrep", CheckBucket::Pass),
                ("windows", CheckBucket::Pass),
            ],
            &[("risk", CheckBucket::Fail)],
            MergeStateStatus::Unstable,
        )));
        s.push_merge(Ok(MergeResult {
            ok: true,
            queued: false,
            message: "merged".into(),
        }));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r)
            .unwrap()
            .expect("advisory ignored");
        assert_eq!(view.phase, graph::PHASE_COMPACT);
        assert_ne!(view.failure_class, Some(FailureClass::CiFailed));
        assert!(
            view.last_event.contains("ci-wait: merged #329"),
            "last_event={}",
            view.last_event
        );
        let st = load_run_state(&r).unwrap();
        let summary = st
            .ci
            .as_ref()
            .and_then(|c| c.last_summary.as_deref())
            .unwrap_or("");
        assert!(summary.contains("advisory fail"), "last_summary={summary}");
        assert!(
            !summary.starts_with("6 pass, 1 fail"),
            "last_summary={summary}"
        );
        assert_eq!(counts.merge_n(), 1);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn duplicate_advisory_pass_fail_merges_and_keeps_disagreed_summary() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr_state(
            59,
            false,
            false,
            MergeStateStatus::Clean,
        ))));
        s.push_snapshot(Ok(required_snap(
            &[],
            &[
                ("fmt clippy test", CheckBucket::Fail),
                ("fmt clippy test", CheckBucket::Pass),
            ],
            MergeStateStatus::Clean,
        )));
        s.push_merge(Ok(MergeResult {
            ok: true,
            queued: false,
            message: "merged".into(),
        }));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r)
            .unwrap()
            .expect("duplicate name must not ci_failed");
        assert_eq!(view.phase, graph::PHASE_COMPACT);
        assert_ne!(view.failure_class, Some(FailureClass::CiFailed));
        assert!(
            view.last_event.contains("ci-wait: merged #59"),
            "last_event={}",
            view.last_event
        );
        assert!(
            view.last_event.chars().count() <= LAST_EVENT_MESSAGE_CAP + 1,
            "last_event len"
        );
        let st = load_run_state(&r).unwrap();
        let summary = st
            .ci
            .as_ref()
            .and_then(|c| c.last_summary.as_deref())
            .unwrap_or("");
        assert!(
            summary.contains("disagreed: fmt clippy test"),
            "last_summary={summary}"
        );
        assert!(!summary.contains("advisory fail"), "last_summary={summary}");
        assert_eq!(
            st.ci.as_ref().and_then(|c| c.merge.as_deref()),
            Some("done")
        );
        assert_eq!(counts.merge_n(), 1);
        assert!(crate::notify::artifact::existing_path(&r).is_none());
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn duplicate_long_disagreed_name_caps_last_event_keeps_last_summary() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let long = format!("fmt {}", "x".repeat(180));
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr_state(
            60,
            false,
            false,
            MergeStateStatus::Clean,
        ))));
        s.push_snapshot(Ok(required_snap(
            &[],
            &[
                (long.as_str(), CheckBucket::Fail),
                (long.as_str(), CheckBucket::Pass),
            ],
            MergeStateStatus::Clean,
        )));
        s.push_merge(Ok(MergeResult {
            ok: true,
            queued: false,
            message: "merged".into(),
        }));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap().expect("merged");
        assert!(
            view.last_event.contains("ci-wait: merged #60"),
            "last_event={}",
            view.last_event
        );
        assert!(
            view.last_event.chars().count() <= LAST_EVENT_MESSAGE_CAP + 1,
            "last_event={}",
            view.last_event
        );
        let st = load_run_state(&r).unwrap();
        let summary = st
            .ci
            .as_ref()
            .and_then(|c| c.last_summary.as_deref())
            .unwrap_or("");
        assert!(
            summary.contains(&format!("disagreed: {long}")),
            "last_summary={summary}"
        );
        assert_eq!(counts.merge_n(), 1);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn required_green_advisory_fail_auto_merge_false_zero_merges() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), false);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr(329, false, false))));
        s.push_snapshot(Ok(required_snap(
            &[("fmt", CheckBucket::Pass)],
            &[("risk", CheckBucket::Fail)],
            MergeStateStatus::Unstable,
        )));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap().expect("skip merge");
        assert_eq!(view.phase, graph::PHASE_COMPACT);
        assert!(view.last_event.contains("merge skipped (auto_merge=false)"));
        assert_eq!(counts.merge_n(), 0);
        let st = load_run_state(&r).unwrap();
        assert_eq!(
            st.ci.as_ref().and_then(|c| c.merge.as_deref()),
            Some("skipped")
        );
        let summary = st
            .ci
            .as_ref()
            .and_then(|c| c.last_summary.as_deref())
            .unwrap_or("");
        assert!(summary.contains("advisory fail"), "last_summary={summary}");
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn required_green_advisory_cancel_does_not_fail() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr(12, false, false))));
        s.push_snapshot(Ok(required_snap(
            &[("fmt", CheckBucket::Pass)],
            &[("bot", CheckBucket::Cancel)],
            MergeStateStatus::Unstable,
        )));
        s.push_merge(Ok(MergeResult {
            ok: true,
            queued: false,
            message: "merged".into(),
        }));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap().expect("advisory cancel");
        assert_eq!(view.phase, graph::PHASE_COMPACT);
        assert_ne!(view.failure_class, Some(FailureClass::CiFailed));
        assert_eq!(counts.merge_n(), 1);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn required_fail_writes_ci_failed_even_with_advisory_fail() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr(13, false, false))));
        s.push_snapshot(Ok(required_snap(
            &[("fmt", CheckBucket::Fail)],
            &[("risk", CheckBucket::Fail)],
            MergeStateStatus::Blocked,
        )));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap().expect("required fail");
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
    fn required_cancel_stays_pending_and_does_not_fail() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr(14, false, false))));
        s.push_snapshot(Ok(required_snap(
            &[("fmt", CheckBucket::Cancel)],
            &[],
            MergeStateStatus::Blocked,
        )));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap();
        assert!(view.is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert!(st.failure_class.is_none());
        assert!(crate::notify::artifact::existing_path(&r).is_none());
        assert!(st.last_event.contains("cancelled: fmt"));
        assert_eq!(counts.merge_n(), 0);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn required_empty_clean_advisory_cancel_stays_pending() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr_state(
            16,
            false,
            false,
            MergeStateStatus::Clean,
        ))));
        s.push_snapshot(Ok(required_snap(
            &[],
            &[("bot", CheckBucket::Cancel)],
            MergeStateStatus::Clean,
        )));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap();
        assert!(view.is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert!(st.failure_class.is_none());
        assert!(crate::notify::artifact::existing_path(&r).is_none());
        assert!(st.last_event.contains("cancelled: bot"));
        assert_eq!(counts.merge_n(), 0);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn head_sha_cancel_stays_pending_zero_merges() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(CiTarget::HeadSha {
            sha: "deadbeef".into(),
        })));
        s.push_snapshot(Ok(items(&[("ci", CheckBucket::Cancel)])));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap();
        assert!(view.is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert!(st.failure_class.is_none());
        assert!(crate::notify::artifact::existing_path(&r).is_none());
        assert!(st.last_event.contains("cancelled: ci"));
        assert_eq!(counts.merge_n(), 0);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn required_empty_blocked_auto_merge_false_advisory_cancel_stays_pending() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), false);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr_state(
            17,
            false,
            false,
            MergeStateStatus::Blocked,
        ))));
        s.push_snapshot(Ok(required_snap(
            &[],
            &[("bot", CheckBucket::Cancel)],
            MergeStateStatus::Blocked,
        )));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap();
        assert!(view.is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert!(st.failure_class.is_none());
        assert!(crate::notify::artifact::existing_path(&r).is_none());
        assert!(st.last_event.contains("cancelled: bot"));
        assert_eq!(counts.merge_n(), 0);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn required_cancel_then_pass_merges() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr_state(
            18,
            false,
            false,
            MergeStateStatus::Blocked,
        ))));
        s.push_resolve(Ok(Some(pr_state(
            18,
            false,
            false,
            MergeStateStatus::Clean,
        ))));
        s.push_snapshot(Ok(required_snap(
            &[("fmt", CheckBucket::Cancel)],
            &[],
            MergeStateStatus::Blocked,
        )));
        s.push_snapshot(Ok(required_snap(
            &[("fmt", CheckBucket::Pass)],
            &[],
            MergeStateStatus::Clean,
        )));
        s.push_merge(Ok(MergeResult {
            ok: true,
            queued: false,
            message: "merged".into(),
        }));
        let (_hook, counts) = hook(s);
        let first = crate::workflow::tick(&r).unwrap();
        assert!(first.is_none());
        assert_eq!(counts.merge_n(), 0);
        std::thread::sleep(Duration::from_millis(5));
        let view = crate::workflow::tick(&r)
            .unwrap()
            .expect("cancel then pass");
        assert_eq!(view.phase, graph::PHASE_COMPACT);
        assert_ne!(view.failure_class, Some(FailureClass::CiFailed));
        assert!(crate::notify::artifact::existing_path(&r).is_none());
        assert_eq!(counts.merge_n(), 1);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn cancelled_long_name_caps_last_event_keeps_last_summary() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let long = "c".repeat(180);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr(19, false, false))));
        s.push_snapshot(Ok(items(&[(&long, CheckBucket::Cancel)])));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap();
        assert!(view.is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert_eq!(st.last_event.chars().count(), LAST_EVENT_MESSAGE_CAP + 1);
        assert!(st.last_event.ends_with('…'));
        let loaded = load_run_state(&r).unwrap();
        assert_eq!(
            loaded.ci.as_ref().and_then(|c| c.last_summary.as_deref()),
            Some("1 cancel")
        );
        assert_eq!(counts.merge_n(), 0);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn required_pending_advisory_fail_stays_pending() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), false);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr(15, false, false))));
        s.push_snapshot(Ok(required_snap(
            &[("fmt", CheckBucket::Pending)],
            &[("risk", CheckBucket::Fail)],
            MergeStateStatus::Blocked,
        )));
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
    fn required_skipping_and_pass_is_green() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr(16, false, false))));
        s.push_snapshot(Ok(required_snap(
            &[("fmt", CheckBucket::Pass), ("opt", CheckBucket::Skipping)],
            &[],
            MergeStateStatus::Clean,
        )));
        s.push_merge(Ok(MergeResult {
            ok: true,
            queued: false,
            message: "merged".into(),
        }));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap().expect("skipping green");
        assert_eq!(view.phase, graph::PHASE_COMPACT);
        assert_eq!(counts.merge_n(), 1);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn required_empty_unknown_does_not_merge() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr(17, false, false))));
        s.push_snapshot(Ok(required_snap(&[], &[], MergeStateStatus::Unknown)));
        s.push_merge(Ok(MergeResult {
            ok: true,
            queued: false,
            message: "should not run".into(),
        }));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap();
        assert!(view.is_none());
        assert_eq!(counts.merge_n(), 0);
        drop(_hook);
        let dir2 = tempdir().unwrap();
        let r2 = rec(dir2.path(), false);
        jump_ci_wait(&r2, WorkflowDriver::Adapter);
        let s2 = ScriptedBackend::new();
        s2.push_resolve(Ok(Some(pr(18, false, false))));
        s2.push_snapshot(Ok(required_snap(&[], &[], MergeStateStatus::Unknown)));
        let (_hook2, counts2) = hook(s2);
        assert!(crate::workflow::tick(&r2).unwrap().is_none());
        assert_eq!(counts2.merge_n(), 0);
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
    fn policy_block_merge_stays_pending() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr_state(1, false, false, MergeStateStatus::Clean))));
        s.push_snapshot(Ok(required_snap(
            &[("fmt", CheckBucket::Pass)],
            &[],
            MergeStateStatus::Clean,
        )));
        s.push_merge(Ok(MergeResult {
            ok: false,
            queued: false,
            message:
                "X Pull request #1 is not mergeable: the base branch policy prohibits the merge."
                    .into(),
        }));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap();
        assert!(view.is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert!(
            st.last_event
                .contains("merge blocked by base branch policy"),
            "last_event={}",
            st.last_event
        );
        let loaded = load_run_state(&r).unwrap();
        let ci = loaded.ci.as_ref().expect("ci");
        assert!(ci.merge.is_none(), "ci.merge={:?}", ci.merge);
        assert_eq!(ci.next_interval_ms, Some(1));
        assert!(
            ci.set_key
                .as_deref()
                .is_some_and(|k| k.contains("fmt:pass")),
            "set_key={:?}",
            ci.set_key
        );
        assert!(crate::notify::artifact::existing_path(&r).is_none());
        assert_eq!(counts.merge_n(), 1);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn auto_publish_empty_required_unspecified_does_not_merge() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(None));
        s.push_publish(Ok(AutoPublishResult::Opened(pr_state(
            7,
            false,
            false,
            MergeStateStatus::Unspecified,
        ))));
        s.push_snapshot(Ok(required_snap(&[], &[], MergeStateStatus::Unspecified)));
        s.push_merge(Ok(MergeResult {
            ok: true,
            queued: false,
            message: "should not run".into(),
        }));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap();
        assert!(view.is_none());
        let loaded = load_run_state(&r).unwrap();
        assert_eq!(loaded.ci.as_ref().and_then(|c| c.pr_number), Some(7));
        assert!(
            loaded
                .ci
                .as_ref()
                .and_then(|c| c.merge.as_deref())
                .is_none(),
            "ci.merge={:?}",
            loaded.ci.as_ref().and_then(|c| c.merge.as_deref())
        );
        assert_eq!(counts.merge_n(), 0);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn empty_required_unspecified_then_required_green_merges_once() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr_state(
            7,
            false,
            false,
            MergeStateStatus::Unspecified,
        ))));
        s.push_resolve(Ok(Some(pr_state(7, false, false, MergeStateStatus::Clean))));
        s.push_snapshot(Ok(required_snap(&[], &[], MergeStateStatus::Unspecified)));
        s.push_snapshot(Ok(required_snap(
            &[("fmt", CheckBucket::Pass)],
            &[],
            MergeStateStatus::Clean,
        )));
        s.push_merge(Ok(MergeResult {
            ok: true,
            queued: false,
            message: "merged".into(),
        }));
        let (_hook, counts) = hook(s);
        let first = crate::workflow::tick(&r).unwrap();
        assert!(first.is_none());
        assert_eq!(counts.merge_n(), 0);
        let loaded = load_run_state(&r).unwrap();
        assert!(
            loaded
                .ci
                .as_ref()
                .and_then(|c| c.merge.as_deref())
                .is_none(),
            "pending tick must not set ci.merge"
        );
        std::thread::sleep(Duration::from_millis(5));
        let view = crate::workflow::tick(&r).unwrap().expect("recovered");
        assert_eq!(view.phase, graph::PHASE_COMPACT);
        assert_eq!(counts.merge_n(), 1);
        let loaded = load_run_state(&r).unwrap();
        assert_eq!(
            loaded.ci.as_ref().and_then(|c| c.merge.as_deref()),
            Some("done")
        );
        assert!(crate::notify::artifact::existing_path(&r).is_none());
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
        assert!(crate::workflow::tick(&r).unwrap().is_none());
        let gate = PhaseOutcome::success(
            graph::PHASE_CROSS_MODEL,
            OutcomeSource::File,
            Some("cross-model: stub (no review)".into()),
            None,
            None,
        );
        crate::outcome::save_current_outcome(&r, &gate).unwrap();
        let after = poll_once(&r).unwrap().expect("file apply cross-model");
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

    fn clear_poll_env() {
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn foreign_pr_is_not_merged() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr_with_head(27, false, true, "track/0029-camera"))));
        let (_hook, counts) = hook(s);
        assert!(crate::workflow::tick(&r).unwrap().is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert!(
            !st.last_event.contains("ci-wait: merged"),
            "last_event={}",
            st.last_event
        );
        assert_eq!(counts.resolve_n(), 1);
        clear_poll_env();
    }

    #[test]
    fn empty_head_ref_is_not_merged() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr_with_head(27, false, true, ""))));
        let (_hook, _counts) = hook(s);
        assert!(crate::workflow::tick(&r).unwrap().is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert!(
            !st.last_event.contains("ci-wait: merged"),
            "last_event={}",
            st.last_event
        );
        clear_poll_env();
    }

    #[test]
    fn hinted_foreign_falls_through_to_owned_pr() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let mut st = load_run_state(&r).unwrap();
        st.ci = Some(CiWatchState {
            pr_number: Some(27),
            ..Default::default()
        });
        save_run_state(&r, &st).unwrap();
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr_with_head(27, false, true, "track/0029-camera"))));
        s.push_resolve(Ok(Some(pr(81, false, true))));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap().expect("owned merge");
        assert!(
            view.last_event
                .contains("ci-wait: merged #81 (track/0010-Fixture)"),
            "last_event={}",
            view.last_event
        );
        assert_eq!(counts.resolve_n(), 2);
        let done = load_run_state(&r).unwrap();
        assert_eq!(done.ci.as_ref().and_then(|c| c.pr_number), Some(81));
        clear_poll_env();
    }

    #[test]
    fn accept_keeps_pull_request_when_numeric_missing() {
        let dir = tempdir().unwrap();
        let state = crate::state::RunState::idle("p");
        let kept = accept_resolved_target(&state, dir.path(), Some(pr(505, false, false)));
        match kept {
            Some(CiTarget::PullRequest { number, .. }) => assert_eq!(number, 505),
            other => panic!("kept {other:?}"),
        }
    }

    #[test]
    fn merged_probe_without_confirming_head_waits() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(None));
        let (_hook, _counts) = hook(s);
        let _pg = crate::workflow::shipped::install_test_merged_probe(Arc::new(
            crate::workflow::shipped::ScriptedMergedProbe::found("0010", 66),
        ));
        assert!(crate::workflow::tick(&r).unwrap().is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert!(
            !st.last_event.contains("ci-wait: merged"),
            "last_event={}",
            st.last_event
        );
        clear_poll_env();
    }

    #[test]
    fn merged_probe_foreign_head_waits() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(None));
        let (_hook, _counts) = hook(s);
        let _pg = crate::workflow::shipped::install_test_merged_probe(Arc::new(
            crate::workflow::shipped::ScriptedMergedProbe::found("0010", 66),
        ));
        let _cg =
            gh::install_confirm_track_pr(Some(pr_with_head(66, false, true, "track/0029-camera")));
        assert!(crate::workflow::tick(&r).unwrap().is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert!(
            !st.last_event.contains("ci-wait: merged"),
            "last_event={}",
            st.last_event
        );
        clear_poll_env();
    }

    #[test]
    fn queued_merge_log_names_head() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr(12, false, false))));
        s.push_snapshot(Ok(items(&[("ci", CheckBucket::Pass)])));
        s.push_merge(Ok(MergeResult {
            ok: true,
            queued: true,
            message: "queued".into(),
        }));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap().expect("queued merge");
        assert!(
            view.last_event.contains("ci-wait: merged #12"),
            "last_event={}",
            view.last_event
        );
        assert!(
            view.last_event.contains("(track/0010-Fixture) (queued)"),
            "last_event={}",
            view.last_event
        );
        assert_eq!(counts.merge_n(), 1);
        clear_poll_env();
    }

    #[test]
    fn pr_merged_event_embeds_head() {
        let head = "track/0010-Fixture";
        for tail in ["", " (queued)", " (state apply failed: boom)"] {
            let event = pr_merged_event(81, head, tail);
            assert!(event.contains("(track/0010-Fixture)"), "{event}");
            assert!(event.contains("ci-wait: merged #81"), "{event}");
            assert!(event.ends_with(tail), "{event}");
        }
    }

    #[test]
    fn waiting_for_pr_merged_track_probe_succeeds() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(None));
        let (_hook, counts) = hook(s);
        let _pg = crate::workflow::shipped::install_test_merged_probe(Arc::new(
            crate::workflow::shipped::ScriptedMergedProbe::found("0010", 66),
        ));
        let _cg = gh::install_confirm_track_pr(Some(pr(66, false, true)));
        let view = crate::workflow::tick(&r)
            .unwrap()
            .expect("merged via title probe");
        assert_eq!(view.phase, graph::PHASE_COMPACT);
        assert!(
            view.last_event
                .contains("ci-wait: merged #66 (track/0010-Fixture)"),
            "last_event={}",
            view.last_event
        );
        let st = load_run_state(&r).unwrap();
        assert_eq!(
            st.ci.as_ref().and_then(|c| c.merge.as_deref()),
            Some("done")
        );
        assert_eq!(counts.merge_n(), 0);
        assert_eq!(counts.publish_n(), 0);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn waiting_for_pr_stays_pending_when_unresolved() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(None));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap();
        assert!(view.is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert!(
            st.last_event.contains("waiting for PR"),
            "last_event={}",
            st.last_event
        );
        assert_eq!(counts.merge_n(), 0);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn waiting_for_pr_probe_err_fail_open() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(None));
        let (_hook, counts) = hook(s);
        let _pg = crate::workflow::shipped::install_test_merged_probe(Arc::new(
            crate::workflow::shipped::ScriptedMergedProbe::err("0010", "gh pr list failed"),
        ));
        let view = crate::workflow::tick(&r).unwrap();
        assert!(view.is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert!(
            st.last_event.contains("waiting for PR"),
            "last_event={}",
            st.last_event
        );
        assert_ne!(st.failure_class, Some(FailureClass::Permission));
        assert_eq!(counts.merge_n(), 0);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn hinted_pr_view_miss_falls_through_to_merged_target() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let mut st = load_run_state(&r).unwrap();
        st.ci = Some(CiWatchState {
            pr_number: Some(66),
            ..Default::default()
        });
        save_run_state(&r, &st).unwrap();
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(None));
        s.push_resolve(Ok(Some(pr(66, false, true))));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r)
            .unwrap()
            .expect("hinted miss → merged");
        assert_eq!(view.phase, graph::PHASE_COMPACT);
        assert!(
            view.last_event.contains("ci-wait: merged #66"),
            "last_event={}",
            view.last_event
        );
        assert_eq!(counts.resolve_n(), 2, "resolve_n={}", counts.resolve_n());
        assert_eq!(counts.merge_n(), 0);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn hinted_miss_does_not_headsha_pr_oid() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let mut st = load_run_state(&r).unwrap();
        st.ci = Some(CiWatchState {
            pr_number: Some(66),
            head_sha: Some("pr-oid-not-on-default".into()),
            ..Default::default()
        });
        save_run_state(&r, &st).unwrap();
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(None));
        s.push_resolve(Ok(None));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap();
        assert!(view.is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert!(
            st.last_event.contains("waiting for PR"),
            "last_event={}",
            st.last_event
        );
        assert!(
            !st.last_event.contains("default branch, no PR"),
            "last_event={}",
            st.last_event
        );
        assert_eq!(counts.merge_n(), 0);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn waiting_for_pr_auto_publish_opens_and_latches() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(None));
        s.push_publish(Ok(AutoPublishResult::Opened(pr(7, false, false))));
        s.push_snapshot(Ok(items(&[("ci", CheckBucket::Pending)])));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap();
        assert!(view.is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert!(
            st.last_event.contains("ci-wait: opened #7"),
            "last_event={}",
            st.last_event
        );
        let loaded = load_run_state(&r).unwrap();
        assert_eq!(loaded.ci.as_ref().and_then(|c| c.pr_number), Some(7));
        assert_eq!(
            loaded
                .ci
                .as_ref()
                .and_then(|c| c.publish_attempted_sha.as_deref()),
            Some("abc")
        );
        assert_eq!(counts.publish_n(), 1);
        assert_eq!(counts.merge_n(), 0);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn auto_publish_opened_masks_cancelled_then_later_tick_names_it() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(None));
        s.push_resolve(Ok(Some(pr(20, false, false))));
        s.push_publish(Ok(AutoPublishResult::Opened(pr(20, false, false))));
        s.push_snapshot(Ok(items(&[("fmt", CheckBucket::Cancel)])));
        s.push_snapshot(Ok(items(&[("fmt", CheckBucket::Cancel)])));
        let (_hook, counts) = hook(s);
        let first = crate::workflow::tick(&r).unwrap();
        assert!(first.is_none());
        let st = run::status(&r).unwrap();
        assert!(
            st.last_event.contains("ci-wait: opened #20"),
            "last_event={}",
            st.last_event
        );
        assert!(!st.last_event.contains("cancelled:"));
        assert_eq!(counts.merge_n(), 0);
        std::thread::sleep(Duration::from_millis(5));
        let second = crate::workflow::tick(&r).unwrap();
        assert!(second.is_none());
        let st = run::status(&r).unwrap();
        assert!(
            st.last_event.contains("cancelled: fmt"),
            "last_event={}",
            st.last_event
        );
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert_eq!(counts.merge_n(), 0);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn waiting_for_pr_auto_publish_once_per_sha() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let mut st = load_run_state(&r).unwrap();
        st.ci = Some(CiWatchState {
            publish_attempted_sha: Some("abc".into()),
            ..Default::default()
        });
        save_run_state(&r, &st).unwrap();
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(None));
        s.push_publish(Ok(AutoPublishResult::Opened(pr(8, false, false))));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap();
        assert!(view.is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert!(
            st.last_event.contains("publish attempted"),
            "last_event={}",
            st.last_event
        );
        assert_eq!(counts.publish_n(), 0);
        assert_eq!(counts.merge_n(), 0);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn waiting_for_pr_open_target_skips_publish() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr(3, false, false))));
        s.push_snapshot(Ok(items(&[("ci", CheckBucket::Pending)])));
        s.push_publish(Ok(AutoPublishResult::Opened(pr(99, false, false))));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap();
        assert!(view.is_none());
        assert_eq!(counts.publish_n(), 0);
        let loaded = load_run_state(&r).unwrap();
        assert_eq!(loaded.ci.as_ref().and_then(|c| c.pr_number), Some(3));
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn waiting_for_pr_auto_publish_skip_keeps_waiting() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(None));
        s.push_publish(Ok(AutoPublishResult::skipped(
            "ci-wait: dirty tree — waiting for PR",
        )));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap();
        assert!(view.is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert!(
            st.last_event.contains("dirty tree"),
            "last_event={}",
            st.last_event
        );
        assert_ne!(st.failure_class, Some(FailureClass::Permission));
        let loaded = load_run_state(&r).unwrap();
        assert!(
            loaded
                .ci
                .as_ref()
                .and_then(|c| c.publish_attempted_sha.as_deref())
                .is_none()
        );
        assert_eq!(counts.merge_n(), 0);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn waiting_for_pr_auto_publish_safety_skips() {
        let _g = poll_env();
        for event in [
            "ci-wait: detached HEAD — waiting for PR",
            "ci-wait: on default branch — waiting for PR",
            "ci-wait: no GitHub remote — waiting for PR",
        ] {
            let dir = tempdir().unwrap();
            let r = rec(dir.path(), true);
            jump_ci_wait(&r, WorkflowDriver::Adapter);
            let s = ScriptedBackend::new();
            s.push_resolve(Ok(None));
            s.push_publish(Ok(AutoPublishResult::skipped(event)));
            let (_hook, counts) = hook(s);
            let view = crate::workflow::tick(&r).unwrap();
            assert!(view.is_none(), "event={event}");
            let st = run::status(&r).unwrap();
            assert_eq!(st.status, RunStatus::Running);
            assert!(
                st.last_event
                    .contains(event.split(" — ").next().unwrap_or(event)),
                "last_event={} event={event}",
                st.last_event
            );
            assert_ne!(st.failure_class, Some(FailureClass::Permission));
            let loaded = load_run_state(&r).unwrap();
            assert!(
                loaded
                    .ci
                    .as_ref()
                    .and_then(|c| c.publish_attempted_sha.as_deref())
                    .is_none(),
                "event={event}"
            );
            assert_eq!(counts.merge_n(), 0);
        }
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn waiting_for_pr_git_timeout_stays_running() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(None));
        s.push_publish(Err(CoordinatorError::Message(
            "ci-wait: git timed out".into(),
        )));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap();
        assert!(view.is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert!(
            st.last_event.contains("git timed out"),
            "last_event={}",
            st.last_event
        );
        assert_ne!(st.failure_class, Some(FailureClass::Permission));
        let loaded = load_run_state(&r).unwrap();
        assert_eq!(
            loaded
                .ci
                .as_ref()
                .map(|c| c.publish_transient_attempts)
                .unwrap_or(0),
            0
        );
        assert_eq!(counts.merge_n(), 0);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn is_grok_bound_false_for_ci_wait() {
        assert!(!graph::is_grok_bound(graph::PHASE_CI_WAIT));
        assert!(!graph::is_skip_phase(graph::PHASE_CI_WAIT));
    }

    fn git_ok(cwd: &std::path::Path, args: &[&str]) -> std::process::Output {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "probe")
            .env("GIT_AUTHOR_EMAIL", "probe@example.com")
            .env("GIT_COMMITTER_NAME", "probe")
            .env("GIT_COMMITTER_EMAIL", "probe@example.com")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    fn init_track_repo(dir: &std::path::Path) -> String {
        git_ok(dir, &["init", "-b", "main"]);
        git_ok(dir, &["config", "user.email", "probe@example.com"]);
        git_ok(dir, &["config", "user.name", "probe"]);
        std::fs::write(dir.join("f.txt"), "base\n").unwrap();
        git_ok(dir, &["add", "f.txt"]);
        git_ok(dir, &["commit", "-m", "base"]);
        let main_sha = String::from_utf8(git_ok(dir, &["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_string();
        git_ok(dir, &["switch", "-c", "track/0412-foo"]);
        std::fs::write(dir.join("f.txt"), "ahead\n").unwrap();
        git_ok(dir, &["commit", "-am", "ahead"]);
        git_ok(dir, &["switch", "main"]);
        main_sha
    }

    fn latch_track(r: &ProjectRecord, head_sha: Option<String>) {
        let mut st = load_run_state(r).unwrap();
        st.track_id = Some("0412-regex-whitespace-trigrams".into());
        st.ci = Some(CiWatchState {
            head_sha,
            ..Default::default()
        });
        save_run_state(r, &st).unwrap();
    }

    fn force_due(r: &ProjectRecord) {
        let mut st = load_run_state(r).unwrap();
        if let Some(ci) = st.ci.as_mut() {
            ci.last_poll_at = None;
        }
        save_run_state(r, &st).unwrap();
    }

    fn seed_transient(r: &ProjectRecord, attempts: u32, sha: Option<&str>) {
        let mut st = load_run_state(r).unwrap();
        st.ci = Some(CiWatchState {
            publish_transient_attempts: attempts,
            publish_transient_sha: sha.map(str::to_string),
            ..Default::default()
        });
        save_run_state(r, &st).unwrap();
    }

    #[test]
    fn push_rejected_retries_then_opens() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(None));
        s.push_publish(Ok(AutoPublishResult::skipped_retryable(
            "ci-wait: push rejected — waiting for PR",
            None,
        )));
        s.push_resolve(Ok(None));
        s.push_publish(Ok(AutoPublishResult::Opened(pr(99, false, false))));
        s.push_snapshot(Ok(items(&[("ci", CheckBucket::Pending)])));
        let (_hook, counts) = hook(s);
        assert!(crate::workflow::tick(&r).unwrap().is_none());
        force_due(&r);
        assert!(crate::workflow::tick(&r).unwrap().is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert!(
            st.last_event.contains("opened #99"),
            "last_event={}",
            st.last_event
        );
        let loaded = load_run_state(&r).unwrap();
        let ci = loaded.ci.as_ref().unwrap();
        assert_eq!(ci.publish_attempted_sha.as_deref(), Some("abc"));
        assert_eq!(ci.publish_transient_attempts, 0);
        assert!(ci.publish_transient_sha.is_none());
        assert_eq!(counts.publish_n(), 2);
        assert_eq!(counts.merge_n(), 0);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn publish_retry_waits_for_the_poll_interval() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(None));
        s.push_publish(Ok(AutoPublishResult::skipped_retryable(
            "ci-wait: push rejected — waiting for PR",
            Some("abc".into()),
        )));
        let (_hook, counts) = hook(s);
        assert!(crate::workflow::tick(&r).unwrap().is_none());
        let loaded = load_run_state(&r).unwrap();
        let ci = loaded.ci.as_ref().unwrap();
        assert!(ci.last_poll_at.is_some());
        assert!(ci.next_interval_ms.is_some());
        assert_eq!(ci.publish_transient_attempts, 1);
        assert_eq!(ci.last_summary.as_deref(), Some("publish retry 1"));
        assert!(ci.publish_attempted_sha.is_none());
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
        }
        let mut st = load_run_state(&r).unwrap();
        let ci = st.ci.as_mut().unwrap();
        ci.last_poll_at = Some(Utc::now());
        ci.next_interval_ms = Some(60_000);
        save_run_state(&r, &st).unwrap();
        assert!(crate::workflow::tick(&r).unwrap().is_none());
        assert_eq!(counts.publish_n(), 1);
        let held = load_run_state(&r).unwrap();
        assert_eq!(held.ci.as_ref().unwrap().publish_transient_attempts, 1);
        force_due(&r);
        assert!(crate::workflow::tick(&r).unwrap().is_some());
        assert_eq!(counts.publish_n(), 2);
        let stopped = load_run_state(&r).unwrap();
        assert_eq!(stopped.status, RunStatus::Stopped);
        assert_eq!(stopped.failure_class, Some(FailureClass::CiFailed));
        assert_eq!(stopped.ci.as_ref().unwrap().publish_transient_attempts, 2);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn publish_retry_exhausts_without_address_ci() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let mut r = rec(dir.path(), true);
        r.ci_fix_routing = true;
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(None));
        s.push_publish(Ok(AutoPublishResult::skipped_retryable(
            "ci-wait: waiting for PR",
            Some("abc".into()),
        )));
        s.push_resolve(Ok(None));
        s.push_publish(Ok(AutoPublishResult::skipped_retryable(
            "ci-wait: waiting for PR",
            Some("abc".into()),
        )));
        let (_hook, counts) = hook(s);
        assert!(crate::workflow::tick(&r).unwrap().is_none());
        force_due(&r);
        let view = crate::workflow::tick(&r).unwrap();
        assert!(view.is_some());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Stopped);
        assert_eq!(st.failure_class, Some(FailureClass::CiFailed));
        assert!(
            st.last_event.contains("publish exhausted"),
            "last_event={}",
            st.last_event
        );
        assert_ne!(st.phase, graph::PHASE_ADDRESS_CI);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        let loaded = load_run_state(&r).unwrap();
        assert_eq!(loaded.ci_fix_attempts, 0);
        let ci = loaded.ci.as_ref().unwrap();
        assert!(ci.publish_attempted_sha.is_none());
        assert_eq!(ci.publish_transient_attempts, 2);
        assert_eq!(ci.publish_transient_sha.as_deref(), Some("abc"));
        assert_eq!(counts.publish_n(), 2);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn publish_retry_first_miss_stores_repo_head() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let head = init_track_repo(dir.path());
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(None));
        s.push_publish(Ok(AutoPublishResult::skipped_retryable(
            "ci-wait: push rejected — waiting for PR",
            None,
        )));
        let (_hook, _counts) = hook(s);
        assert!(crate::workflow::tick(&r).unwrap().is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        let loaded = load_run_state(&r).unwrap();
        let ci = loaded.ci.as_ref().unwrap();
        assert_eq!(ci.publish_transient_attempts, 1);
        assert_eq!(ci.publish_transient_sha.as_deref(), Some(head.as_str()));
        assert!(ci.publish_attempted_sha.is_none());
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn publish_retry_uses_result_head_as_counter_key() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        seed_transient(&r, 2, Some("old"));
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(None));
        s.push_publish(Ok(AutoPublishResult::skipped_retryable(
            "ci-wait: push rejected — waiting for PR",
            Some("new".into()),
        )));
        let (_hook, _counts) = hook(s);
        assert!(crate::workflow::tick(&r).unwrap().is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        let ci = load_run_state(&r).unwrap().ci.unwrap();
        assert_eq!(ci.publish_transient_attempts, 1);
        assert_eq!(ci.publish_transient_sha.as_deref(), Some("new"));
        assert!(ci.publish_attempted_sha.is_none());
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn publish_retry_missing_head_does_not_reset() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        seed_transient(&r, 1, Some("old"));
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(None));
        s.push_publish(Ok(AutoPublishResult::skipped_retryable(
            "ci-wait: waiting for PR",
            None,
        )));
        let (_hook, _counts) = hook(s);
        assert!(crate::workflow::tick(&r).unwrap().is_some());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Stopped);
        assert_eq!(st.failure_class, Some(FailureClass::CiFailed));
        let ci = load_run_state(&r).unwrap().ci.unwrap();
        assert_eq!(ci.publish_transient_attempts, 2);
        assert_eq!(ci.publish_transient_sha.as_deref(), Some("old"));
        assert!(ci.publish_attempted_sha.is_none());
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn terminal_skip_latches_across_due_ticks() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(None));
        s.push_publish(Ok(AutoPublishResult::skipped_latched(
            "ci-wait: detached HEAD — waiting for PR",
            "abc",
        )));
        s.push_resolve(Ok(None));
        let (_hook, counts) = hook(s);
        assert!(crate::workflow::tick(&r).unwrap().is_none());
        force_due(&r);
        assert!(crate::workflow::tick(&r).unwrap().is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert!(
            st.last_event.contains("publish attempted"),
            "last_event={}",
            st.last_event
        );
        let ci = load_run_state(&r).unwrap().ci.unwrap();
        assert_eq!(ci.publish_attempted_sha.as_deref(), Some("abc"));
        assert_eq!(counts.publish_n(), 1);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn sticky_sha_on_track_branch_does_not_complete() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        latch_track(&r, Some("mainsha".into()));
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(None));
        s.push_publish(Ok(AutoPublishResult::skipped("ci-wait: waiting for PR")));
        let (_hook, counts) = hook(s);
        assert!(crate::workflow::tick(&r).unwrap().is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert!(
            st.last_event.contains("waiting for PR"),
            "{}",
            st.last_event
        );
        let loaded = load_run_state(&r).unwrap();
        assert_eq!(loaded.ci.as_ref().and_then(|c| c.head_sha.clone()), None);
        assert_eq!(loaded.ci.as_ref().and_then(|c| c.merge.clone()), None);
        assert_eq!(counts.merge_n(), 0);
        assert!(crate::notify::artifact::existing_path(&r).is_none());
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn sticky_head_sha_cleared_when_track_branch_not_ancestor() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let main_sha = init_track_repo(dir.path());
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        latch_track(&r, Some(main_sha.clone()));
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(CiTarget::HeadSha { sha: main_sha })));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap();
        assert!(view.is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert!(
            st.last_event.contains("waiting for PR"),
            "last_event={}",
            st.last_event
        );
        assert!(
            !st.last_event.contains("default branch, no PR"),
            "last_event={}",
            st.last_event
        );
        let loaded = load_run_state(&r).unwrap();
        assert_eq!(loaded.ci.as_ref().and_then(|c| c.head_sha.clone()), None);
        assert_eq!(loaded.ci.as_ref().and_then(|c| c.merge.clone()), None);
        assert_eq!(
            loaded
                .ci
                .as_ref()
                .and_then(|c| c.publish_attempted_sha.clone()),
            None
        );
        assert_eq!(counts.merge_n(), 0);
        assert!(crate::notify::artifact::existing_path(&r).is_none());
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn head_sha_green_when_track_tip_is_ancestor() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let _base = init_track_repo(dir.path());
        git_ok(dir.path(), &["merge", "--ff-only", "track/0412-foo"]);
        let merged = String::from_utf8(git_ok(dir.path(), &["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_string();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        latch_track(&r, Some(merged.clone()));
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(CiTarget::HeadSha { sha: merged })));
        s.push_snapshot(Ok(items(&[("ci", CheckBucket::Pass)])));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r)
            .unwrap()
            .expect("ancestor headsha");
        assert_eq!(view.phase, graph::PHASE_COMPACT);
        assert!(view.last_event.contains("default branch, no PR"));
        assert_eq!(counts.merge_n(), 0);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn sticky_clear_then_pr_merge_reaches_compact_once() {
        let _g = poll_env();
        let dir = tempdir().unwrap();
        let main_sha = init_track_repo(dir.path());
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        latch_track(&r, Some(main_sha.clone()));
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(CiTarget::HeadSha { sha: main_sha })));
        s.push_resolve(Ok(Some(pr_with_head(7, false, false, "track/0412-foo"))));
        s.push_snapshot(Ok(items(&[("ci", CheckBucket::Pass)])));
        s.push_merge(Ok(MergeResult {
            ok: true,
            queued: false,
            message: "merged".into(),
        }));
        let (_hook, counts) = hook(s);
        assert!(crate::workflow::tick(&r).unwrap().is_none());
        let mid = load_run_state(&r).unwrap();
        assert_eq!(mid.phase, graph::PHASE_CI_WAIT);
        assert_eq!(mid.ci.as_ref().and_then(|c| c.head_sha.clone()), None);
        force_due(&r);
        let view = crate::workflow::tick(&r).unwrap().expect("recovered");
        assert_eq!(view.phase, graph::PHASE_COMPACT);
        let done = load_run_state(&r).unwrap();
        assert_eq!(
            done.ci.as_ref().and_then(|c| c.merge.clone()).as_deref(),
            Some("done")
        );
        assert_eq!(counts.merge_n(), 1);
        assert!(crate::notify::artifact::existing_path(&r).is_none());
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    struct PolicyHome(#[allow(dead_code)] tempfile::TempDir);

    impl Drop for PolicyHome {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var(crate::config::ENV_COORDINATOR_HOME);
            }
        }
    }

    fn policy_home() -> PolicyHome {
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
            std::env::remove_var(crate::policy::ENV_STATE_POLICIES);
        }
        PolicyHome(home)
    }

    fn rule(name: &str, action: crate::policy::PolicyAction) -> crate::policy::PolicyRule {
        crate::policy::PolicyRule {
            name: name.into(),
            action,
            threshold: None,
        }
    }

    fn reader(
        paths: std::result::Result<Vec<String>, String>,
        failures: u32,
        restored: Option<u64>,
    ) -> crate::policy::TestReaderGuard {
        crate::policy::install_test_reader(Arc::new(crate::policy::FixedRead {
            paths: std::sync::Mutex::new(paths),
            failures: std::sync::Mutex::new(Ok(failures)),
            restored: std::sync::Mutex::new(Ok(restored)),
        }))
    }

    fn publish_pending() -> ScriptedBackend {
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(None));
        s.push_publish(Ok(AutoPublishResult::Opened(pr(3, false, false))));
        s.push_snapshot(Ok(items(&[("ci", CheckBucket::Pending)])));
        s
    }

    #[test]
    fn state_policy_block_stops_each_trigger_before_publish_or_merge() {
        let _g = poll_env();
        let _home = policy_home();
        let cases = [
            crate::policy::NAME_DEPENDENCY,
            crate::policy::NAME_CI_WORKFLOW,
            crate::policy::NAME_FAILURES,
            crate::policy::NAME_RESTORE,
        ];
        for name in cases {
            let _guard = match name {
                crate::policy::NAME_DEPENDENCY => reader(Ok(vec!["Cargo.toml".into()]), 0, None),
                crate::policy::NAME_CI_WORKFLOW => {
                    reader(Ok(vec![".github/workflows/ci.yml".into()]), 0, None)
                }
                crate::policy::NAME_FAILURES => reader(Ok(vec![]), 3, None),
                _ => reader(Ok(vec![]), 0, Some(2)),
            };
            let dir = tempdir().unwrap();
            let mut r = rec(dir.path(), true);
            r.state_policies
                .push(rule(name, crate::policy::PolicyAction::Block));
            jump_ci_wait(&r, WorkflowDriver::Adapter);
            let s = ScriptedBackend::new();
            s.push_resolve(Ok(Some(pr(1, false, false))));
            s.push_snapshot(Ok(items(&[("ci", CheckBucket::Pass)])));
            s.push_merge(Ok(MergeResult {
                ok: true,
                queued: false,
                message: "should not merge".into(),
            }));
            s.push_publish(Ok(AutoPublishResult::Opened(pr(1, false, false))));
            let (_hook, counts) = hook(s);
            let view = crate::workflow::tick(&r).unwrap().expect("policy block");
            assert_eq!(view.status, RunStatus::Stopped, "{name}");
            assert_eq!(view.failure_class, Some(FailureClass::Permission), "{name}");
            assert!(
                view.last_event.contains("policy: block"),
                "{}",
                view.last_event
            );
            assert!(view.last_event.contains(name), "{}", view.last_event);
            assert!(
                crate::notify::artifact::existing_path(&r).is_some(),
                "{name}"
            );
            assert_eq!(counts.publish_n(), 0, "{name}");
            assert_eq!(counts.merge_n(), 0, "{name}");
            assert_eq!(counts.resolve_n(), 0, "{name}");
        }
    }

    #[test]
    fn state_policy_report_still_publishes_and_sets_state_gate() {
        let _g = poll_env();
        let _home = policy_home();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let _reader = reader(Ok(vec!["Cargo.toml".into()]), 0, None);
        let (_hook, counts) = hook(publish_pending());
        let view = crate::workflow::tick(&r).unwrap();
        assert!(view.is_none(), "pending checks stay in ci-wait");
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        let gate = st.state_gate.expect("report sets state_gate");
        assert_eq!(gate.name, crate::policy::NAME_DEPENDENCY);
        assert_eq!(gate.action, "report");
        assert!(gate.detail.starts_with("policy: report"), "{}", gate.detail);
        assert_eq!(counts.publish_n(), 1);
        assert_eq!(counts.merge_n(), 0);
        assert!(crate::notify::artifact::existing_path(&r).is_none());
    }

    #[test]
    fn state_policy_unreadable_diff_blocks_with_zero_calls() {
        let _g = poll_env();
        let _home = policy_home();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let _reader = reader(Err("missing checkpoint".into()), 0, None);
        let (_hook, counts) = hook(publish_pending());
        let view = crate::workflow::tick(&r).unwrap().expect("unreadable");
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.failure_class, Some(FailureClass::Permission));
        assert!(
            view.last_event.contains("policy: block"),
            "{}",
            view.last_event
        );
        assert!(
            view.last_event.contains("unreadable"),
            "{}",
            view.last_event
        );
        assert_eq!(counts.publish_n(), 0);
        assert_eq!(counts.merge_n(), 0);
        assert!(crate::notify::artifact::existing_path(&r).is_some());
    }

    #[test]
    fn state_policy_empty_diff_does_not_block() {
        let _g = poll_env();
        let _home = policy_home();
        let dir = tempdir().unwrap();
        let r = rec(dir.path(), true);
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let _reader = reader(Ok(vec!["src/lib.rs".into()]), 0, None);
        let (_hook, counts) = hook(publish_pending());
        let view = crate::workflow::tick(&r).unwrap();
        assert!(view.is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert!(st.state_gate.is_none());
        assert!(st.failure_class.is_none());
        assert_eq!(counts.publish_n(), 1);
        assert!(crate::notify::artifact::existing_path(&r).is_none());
    }

    #[test]
    fn state_policy_hold_then_approve_publishes_once() {
        let _g = poll_env();
        let _home = policy_home();
        let dir = tempdir().unwrap();
        let mut r = rec(dir.path(), true);
        r.state_policies.push(rule(
            crate::policy::NAME_DEPENDENCY,
            crate::policy::PolicyAction::RequireApproval,
        ));
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let _reader = reader(Ok(vec!["Cargo.toml".into()]), 0, None);
        let (_hook, counts) = hook(publish_pending());
        let held = crate::workflow::tick(&r).unwrap();
        assert!(held.is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert!(st.failure_class.is_none());
        assert!(
            st.last_event.contains("policy: require-approval"),
            "{}",
            st.last_event
        );
        assert!(crate::notify::artifact::existing_path(&r).is_none());
        assert_eq!(counts.publish_n(), 0);
        let cwd = crate::worktree::product_git_cwd(&r).unwrap();
        crate::state::with_run_state_lock(&r, || {
            let mut state = load_run_state(&r)?;
            crate::policy::approve(&r, &mut state, &cwd, crate::policy::NAME_DEPENDENCY)?;
            save_run_state(&r, &state)
        })
        .unwrap();
        let opened = crate::workflow::tick(&r).unwrap();
        assert!(opened.is_none());
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert!(st.failure_class.is_none());
        assert_eq!(counts.publish_n(), 1);
    }

    #[test]
    fn state_policy_off_skips_the_read_and_still_publishes() {
        let _g = poll_env();
        let _home = policy_home();
        unsafe {
            std::env::set_var(crate::policy::ENV_STATE_POLICIES, "OFF");
        }
        let dir = tempdir().unwrap();
        let mut r = rec(dir.path(), true);
        r.state_policies.push(rule(
            crate::policy::NAME_DEPENDENCY,
            crate::policy::PolicyAction::Block,
        ));
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let _reader = reader(Err("would block".into()), 0, None);
        let (_hook, counts) = hook(publish_pending());
        let view = crate::workflow::tick(&r).unwrap();
        assert!(view.is_none());
        assert_eq!(counts.publish_n(), 1);
        assert_eq!(counts.merge_n(), 0);
        let st = run::status(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert!(st.failure_class.is_none());
        assert!(crate::notify::artifact::existing_path(&r).is_none());
        unsafe {
            std::env::remove_var(crate::policy::ENV_STATE_POLICIES);
        }
    }

    #[test]
    fn state_policy_block_refuses_squash_of_a_green_pr() {
        let _g = poll_env();
        let _home = policy_home();
        let dir = tempdir().unwrap();
        let mut r = rec(dir.path(), true);
        r.state_policies.push(rule(
            crate::policy::NAME_CI_WORKFLOW,
            crate::policy::PolicyAction::Block,
        ));
        jump_ci_wait(&r, WorkflowDriver::Adapter);
        let _reader = reader(Ok(vec![".github/workflows/ci.yml".into()]), 0, None);
        let s = ScriptedBackend::new();
        s.push_resolve(Ok(Some(pr_state(9, false, false, MergeStateStatus::Clean))));
        s.push_snapshot(Ok(required_clean(
            &[("ci", CheckBucket::Pass)],
            MergeStateStatus::Clean,
        )));
        s.push_merge(Ok(MergeResult {
            ok: true,
            queued: false,
            message: "should not merge".into(),
        }));
        let (_hook, counts) = hook(s);
        let view = crate::workflow::tick(&r).unwrap().expect("blocked");
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.failure_class, Some(FailureClass::Permission));
        assert_eq!(counts.merge_n(), 0);
        assert_eq!(counts.publish_n(), 0);
        assert_eq!(counts.resolve_n(), 0);
    }

    const OWNED_REF: &str = "track/0071-CiFailureRoutesToImplementer";
    const OWNED_TITLE: &str = "track(0071): route ci";

    struct RouteEnv {
        _poll: std::sync::MutexGuard<'static, ()>,
        prev_fix: Option<std::ffi::OsString>,
    }

    impl RouteEnv {
        fn enter() -> Self {
            let poll = poll_env();
            let prev_fix = std::env::var_os(super::fix::ENV_CI_FIX);
            unsafe {
                std::env::remove_var(super::fix::ENV_CI_FIX);
            }
            Self {
                _poll: poll,
                prev_fix,
            }
        }
    }

    impl Drop for RouteEnv {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var(ENV_COORDINATOR_CI_POLL_MS);
                std::env::remove_var(ENV_COORDINATOR_NOTIFY);
                match &self.prev_fix {
                    Some(v) => std::env::set_var(super::fix::ENV_CI_FIX, v),
                    None => std::env::remove_var(super::fix::ENV_CI_FIX),
                }
            }
        }
    }

    fn item(name: &str, bucket: CheckBucket, description: &str, link: &str) -> CheckItem {
        CheckItem {
            name: name.into(),
            bucket,
            description: description.into(),
            link: link.into(),
        }
    }

    fn owned_pr(head_ref: &str, title: &str) -> CiTarget {
        CiTarget::PullRequest {
            number: 71,
            url: "https://example/pr/71".into(),
            is_draft: false,
            merged: false,
            head_oid: Some("abc111".into()),
            merge_state: MergeStateStatus::Clean,
            head_ref: head_ref.into(),
            title: title.into(),
        }
    }

    fn required_items(items: Vec<CheckItem>, advisory: Vec<CheckItem>) -> CheckSnapshot {
        CheckSnapshot {
            items,
            raw_exit: 0,
            merge_state: MergeStateStatus::Clean,
            view: CheckView::Required,
            advisory,
        }
    }

    fn start_owned(routing: bool) -> (tempfile::TempDir, ProjectRecord) {
        start_owned_merge(routing, true)
    }

    fn start_owned_merge(routing: bool, auto_merge: bool) -> (tempfile::TempDir, ProjectRecord) {
        let dir = tempdir().unwrap();
        let mut r = rec(dir.path(), auto_merge);
        r.ci_fix_routing = routing;
        run_with_driver(&r, Some("0071".into()), WorkflowDriver::Adapter).unwrap();
        let mut s = load_run_state(&r).unwrap();
        s.phase = graph::PHASE_CI_WAIT.into();
        s.last_driven_phase = None;
        s.ci = Some(crate::state::CiWatchState {
            publish_attempted_sha: Some("keep-publish".into()),
            merge: Some("skipped".into()),
            ..crate::state::CiWatchState::default()
        });
        save_run_state(&r, &s).unwrap();
        (dir, r)
    }

    fn script(
        r: &ProjectRecord,
        target: CiTarget,
        snap: CheckSnapshot,
    ) -> (TestBackendGuard, CallCounts) {
        let backend = ScriptedBackend::new();
        backend.push_resolve(Ok(Some(target)));
        backend.push_snapshot(Ok(snap));
        let hooked = hook(backend);
        let _ = r;
        hooked
    }

    fn assert_declined(r: &ProjectRecord) {
        let st = load_run_state(r).unwrap();
        assert_eq!(st.status, RunStatus::Stopped);
        assert_eq!(st.failure_class, Some(FailureClass::CiFailed));
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert_eq!(st.ci_fix_attempts, 0);
        assert!(st.ci_fix_request.is_none());
        assert_eq!(st.address_findings_attempts, 0);
        assert!(
            !st.last_event.contains("address-ci"),
            "last_event={}",
            st.last_event
        );
        assert!(crate::notify::artifact::existing_path(r).is_some());
    }

    #[test]
    fn required_fail_on_owned_pr_routes_once_when_flag_on() {
        let _env = RouteEnv::enter();
        let (_dir, r) = start_owned(true);
        let long = "x".repeat(1100);
        let (_hook, counts) = script(
            &r,
            owned_pr(OWNED_REF, OWNED_TITLE),
            required_items(
                vec![
                    item(
                        "fmt",
                        CheckBucket::Fail,
                        "rustfmt failed",
                        "https://example/checks/fmt",
                    ),
                    item("lint", CheckBucket::Fail, "", ""),
                    item(
                        "notes",
                        CheckBucket::Fail,
                        &long,
                        "https://example/checks/notes",
                    ),
                    item("ci", CheckBucket::Pass, "ok", "https://example/checks/ci"),
                ],
                Vec::new(),
            ),
        );
        let view = crate::workflow::tick(&r).unwrap().expect("routed");
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.phase, graph::PHASE_ADDRESS_CI);
        assert!(view.failure_class.is_none());
        assert_eq!(view.run_epoch, 1);
        assert_eq!(counts.merge_n(), 0);
        assert_eq!(counts.publish_n(), 0);
        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("\"ci_fix_attempts\":1"), "{json}");
        assert!(crate::notify::artifact::existing_path(&r).is_none());
        let st = load_run_state(&r).unwrap();
        assert_eq!(st.ci_fix_attempts, 1);
        assert_eq!(st.address_findings_attempts, 0);
        assert_eq!(st.run_epoch, 1);
        assert_eq!(st.last_event, "ci-wait: address-ci 1/2");
        assert!(st.last_driven_phase.is_none());
        let req = st.ci_fix_request.expect("request");
        assert_eq!(req.pr_number, 71);
        assert_eq!(req.from_sha, "abc111");
        assert_eq!(req.checks.len(), 3);
        assert_eq!(req.checks[0].name, "fmt");
        assert_eq!(req.checks[0].bucket, "fail");
        assert_eq!(req.checks[0].description, "rustfmt failed");
        assert_eq!(req.checks[0].link, "https://example/checks/fmt");
        assert_eq!(req.checks[1].name, "lint");
        assert!(req.checks[1].description.is_empty());
        assert_eq!(req.checks[2].description.chars().count(), 1024);
        assert!(req.checks.iter().all(|c| c.name != "ci"));
        let ci = st.ci.expect("watch");
        assert_eq!(ci.pr_number, Some(71));
        assert!(ci.pr_url.is_some());
        assert!(ci.head_sha.is_none());
        assert!(ci.set_key.is_none());
        assert!(ci.last_summary.is_none());
        assert!(ci.next_interval_ms.is_none());
        assert!(ci.last_poll_at.is_some());
        assert_eq!(ci.merge.as_deref(), Some("skipped"));
        assert_eq!(ci.publish_attempted_sha.as_deref(), Some("keep-publish"));
    }

    #[test]
    fn second_required_fail_increments_address_ci_to_two() {
        let _env = RouteEnv::enter();
        let (_dir, r) = start_owned(true);
        let (_hook, counts) = script(
            &r,
            owned_pr(OWNED_REF, OWNED_TITLE),
            required_items(
                vec![item(
                    "fmt",
                    CheckBucket::Fail,
                    "rustfmt failed",
                    "https://example/fmt",
                )],
                Vec::new(),
            ),
        );
        crate::workflow::tick(&r).unwrap().expect("first route");
        crate::state::with_run_state_lock(&r, || {
            let mut s = load_run_state(&r)?;
            s.phase = graph::PHASE_CI_WAIT.into();
            s.last_driven_phase = None;
            if let Some(ci) = s.ci.as_mut() {
                ci.last_poll_at = None;
            }
            save_run_state(&r, &s)
        })
        .unwrap();
        let view = crate::workflow::tick(&r).unwrap().expect("second route");
        assert_eq!(view.phase, graph::PHASE_ADDRESS_CI);
        assert_eq!(view.status, RunStatus::Running);
        assert!(view.failure_class.is_none());
        let st = load_run_state(&r).unwrap();
        assert_eq!(st.ci_fix_attempts, 2);
        assert_eq!(st.address_findings_attempts, 0);
        assert_eq!(st.last_event, "ci-wait: address-ci 2/2");
        assert!(crate::notify::artifact::existing_path(&r).is_none());
        assert_eq!(counts.merge_n(), 0);
        assert_eq!(counts.publish_n(), 0);
    }

    #[test]
    fn address_ci_cap_stops_ci_failed() {
        let _env = RouteEnv::enter();
        let (_dir, r) = start_owned(true);
        {
            let mut s = load_run_state(&r).unwrap();
            s.ci_fix_attempts = 2;
            save_run_state(&r, &s).unwrap();
        }
        let (_hook, counts) = script(
            &r,
            owned_pr(OWNED_REF, OWNED_TITLE),
            required_items(
                vec![item(
                    "fmt",
                    CheckBucket::Fail,
                    "rustfmt failed",
                    "https://example/fmt",
                )],
                Vec::new(),
            ),
        );
        let view = crate::workflow::tick(&r).unwrap().expect("exhausted");
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.failure_class, Some(FailureClass::CiFailed));
        assert_eq!(view.phase, graph::PHASE_CI_WAIT);
        assert!(
            view.last_event.contains("address-ci exhausted (2/2)"),
            "last_event={}",
            view.last_event
        );
        let st = load_run_state(&r).unwrap();
        assert_eq!(st.ci_fix_attempts, 2);
        assert_eq!(st.address_findings_attempts, 0);
        assert!(crate::notify::artifact::existing_path(&r).is_some());
        assert_eq!(counts.merge_n(), 0);
        assert_eq!(counts.publish_n(), 0);
    }

    #[test]
    fn flag_off_required_fail_stays_ci_failed() {
        let _env = RouteEnv::enter();
        unsafe {
            std::env::set_var(super::fix::ENV_CI_FIX, "1");
        }
        let (_dir, r) = start_owned(false);
        let (_hook, counts) = script(
            &r,
            owned_pr(OWNED_REF, OWNED_TITLE),
            required_items(
                vec![item(
                    "fmt",
                    CheckBucket::Fail,
                    "rustfmt failed",
                    "https://example/fmt",
                )],
                Vec::new(),
            ),
        );
        crate::workflow::tick(&r).unwrap().expect("declined");
        assert_declined(&r);
        assert_eq!(counts.merge_n(), 0);
        assert_eq!(counts.publish_n(), 0);
    }

    #[test]
    fn env_off_required_fail_stays_ci_failed() {
        let _env = RouteEnv::enter();
        unsafe {
            std::env::set_var(super::fix::ENV_CI_FIX, "off");
        }
        let (_dir, r) = start_owned(true);
        let (_hook, _counts) = script(
            &r,
            owned_pr(OWNED_REF, OWNED_TITLE),
            required_items(
                vec![item(
                    "fmt",
                    CheckBucket::Fail,
                    "rustfmt failed",
                    "https://example/fmt",
                )],
                Vec::new(),
            ),
        );
        crate::workflow::tick(&r).unwrap().expect("declined");
        assert_declined(&r);
    }

    #[test]
    fn required_cancel_stays_pending_and_does_not_route() {
        let _env = RouteEnv::enter();
        let (_dir, r) = start_owned(true);
        let (_hook, counts) = script(
            &r,
            owned_pr(OWNED_REF, OWNED_TITLE),
            required_items(
                vec![item(
                    "fmt",
                    CheckBucket::Cancel,
                    "cancelled",
                    "https://example/fmt",
                )],
                Vec::new(),
            ),
        );
        assert!(crate::workflow::tick(&r).unwrap().is_none());
        let st = load_run_state(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert_eq!(st.ci_fix_attempts, 0);
        assert!(st.ci_fix_request.is_none());
        assert!(st.last_event.contains("cancelled"), "{}", st.last_event);
        assert!(crate::notify::artifact::existing_path(&r).is_none());
        assert_eq!(counts.merge_n(), 0);
        assert_eq!(counts.publish_n(), 0);
    }

    fn assert_advisory_routed(r: &ProjectRecord, counts: &CallCounts, name: &str) {
        let st = load_run_state(r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_ADDRESS_CI);
        assert!(st.failure_class.is_none());
        assert_eq!(st.ci_fix_attempts, 1);
        assert_eq!(st.last_event, "ci-wait: address-ci 1/2");
        let req = st.ci_fix_request.expect("request");
        assert_eq!(req.checks.len(), 1);
        assert_eq!(req.checks[0].name, name);
        assert_eq!(req.checks[0].bucket, "fail");
        assert!(crate::notify::artifact::existing_path(r).is_none());
        assert_eq!(counts.merge_n(), 0);
        assert_eq!(counts.publish_n(), 0);
        let ci = st.ci.expect("watch");
        assert!(ci.set_key.is_none(), "set_key={:?}", ci.set_key);
    }

    fn route_advisory_merge(merge: MergeStateStatus) {
        let _env = RouteEnv::enter();
        let (_dir, r) = start_owned(true);
        let (_hook, counts) = script(
            &r,
            owned_pr(OWNED_REF, OWNED_TITLE),
            required_snap(&[], &[("risk", CheckBucket::Fail)], merge),
        );
        let view = crate::workflow::tick(&r).unwrap().expect("routed");
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.phase, graph::PHASE_ADDRESS_CI);
        assert!(view.failure_class.is_none());
        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("\"ci_fix_attempts\":1"), "{json}");
        assert_advisory_routed(&r, &counts, "risk");
    }

    #[test]
    fn advisory_fallback_fail_routes_when_flag_on() {
        route_advisory_merge(MergeStateStatus::Clean);
    }

    #[test]
    fn advisory_fallback_fail_unstable_routes_when_flag_on() {
        route_advisory_merge(MergeStateStatus::Unstable);
    }

    #[test]
    fn advisory_fallback_fail_has_hooks_routes_when_flag_on() {
        route_advisory_merge(MergeStateStatus::HasHooks);
    }

    #[test]
    fn flag_off_advisory_fallback_stays_ci_failed() {
        let _env = RouteEnv::enter();
        let (_dir, r) = start_owned(false);
        let (_hook, counts) = script(
            &r,
            owned_pr(OWNED_REF, OWNED_TITLE),
            required_snap(&[], &[("risk", CheckBucket::Fail)], MergeStateStatus::Clean),
        );
        crate::workflow::tick(&r).unwrap().expect("declined");
        assert_declined(&r);
        assert_eq!(counts.merge_n(), 0);
        assert_eq!(counts.publish_n(), 0);
    }

    #[test]
    fn blocked_auto_merge_true_advisory_fail_stays_pending() {
        let _env = RouteEnv::enter();
        let (_dir, r) = start_owned(true);
        let (_hook, _counts) = script(
            &r,
            owned_pr(OWNED_REF, OWNED_TITLE),
            required_snap(
                &[],
                &[("risk", CheckBucket::Fail)],
                MergeStateStatus::Blocked,
            ),
        );
        assert!(crate::workflow::tick(&r).unwrap().is_none());
        let st = load_run_state(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert_eq!(st.ci_fix_attempts, 0);
        assert!(st.last_event.contains("waiting"), "{}", st.last_event);
    }

    #[test]
    fn blocked_auto_merge_false_advisory_fail_routes() {
        let _env = RouteEnv::enter();
        let (_dir, r) = start_owned_merge(true, false);
        let (_hook, counts) = script(
            &r,
            owned_pr(OWNED_REF, OWNED_TITLE),
            required_snap(
                &[],
                &[("risk", CheckBucket::Fail)],
                MergeStateStatus::Blocked,
            ),
        );
        crate::workflow::tick(&r).unwrap().expect("routed");
        assert_advisory_routed(&r, &counts, "risk");
    }

    #[test]
    fn unspecified_merge_advisory_fail_stays_pending() {
        let _env = RouteEnv::enter();
        let (_dir, r) = start_owned(true);
        let (_hook, _counts) = script(
            &r,
            owned_pr(OWNED_REF, OWNED_TITLE),
            required_snap(
                &[],
                &[("risk", CheckBucket::Fail)],
                MergeStateStatus::Unspecified,
            ),
        );
        assert!(crate::workflow::tick(&r).unwrap().is_none());
        let st = load_run_state(&r).unwrap();
        assert_eq!(st.ci_fix_attempts, 0);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
    }

    #[test]
    fn advisory_cancel_on_fallback_stays_pending() {
        let _env = RouteEnv::enter();
        let (_dir, r) = start_owned(true);
        let (_hook, _counts) = script(
            &r,
            owned_pr(OWNED_REF, OWNED_TITLE),
            required_snap(
                &[],
                &[("risk", CheckBucket::Cancel), ("lint", CheckBucket::Pass)],
                MergeStateStatus::Clean,
            ),
        );
        assert!(crate::workflow::tick(&r).unwrap().is_none());
        let st = load_run_state(&r).unwrap();
        assert_eq!(st.ci_fix_attempts, 0);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert!(st.last_event.contains("cancelled"), "{}", st.last_event);
        let key = st.ci.expect("watch").set_key.expect("set_key");
        assert!(key.contains("risk:cancel"), "{key}");
        assert!(key.contains("lint:pass"), "{key}");
    }

    #[test]
    fn advisory_fallback_cap_stops() {
        let _env = RouteEnv::enter();
        let (_dir, r) = start_owned(true);
        {
            let mut s = load_run_state(&r).unwrap();
            s.ci_fix_attempts = 2;
            save_run_state(&r, &s).unwrap();
        }
        let (_hook, counts) = script(
            &r,
            owned_pr(OWNED_REF, OWNED_TITLE),
            required_snap(&[], &[("risk", CheckBucket::Fail)], MergeStateStatus::Clean),
        );
        let view = crate::workflow::tick(&r).unwrap().expect("exhausted");
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.failure_class, Some(FailureClass::CiFailed));
        assert!(
            view.last_event.contains("address-ci exhausted (2/2)"),
            "last_event={}",
            view.last_event
        );
        assert_eq!(counts.merge_n(), 0);
        assert_eq!(counts.publish_n(), 0);
    }

    #[test]
    fn required_fail_ignores_advisory_sibling() {
        let _env = RouteEnv::enter();
        let (_dir, r) = start_owned(true);
        let (_hook, counts) = script(
            &r,
            owned_pr(OWNED_REF, OWNED_TITLE),
            required_items(
                vec![item(
                    "fmt",
                    CheckBucket::Fail,
                    "rustfmt failed",
                    "https://example/fmt",
                )],
                vec![item("risk", CheckBucket::Fail, "", "")],
            ),
        );
        crate::workflow::tick(&r).unwrap().expect("routed");
        assert_advisory_routed(&r, &counts, "fmt");
        let req = load_run_state(&r).unwrap().ci_fix_request.expect("request");
        assert!(req.checks.iter().all(|c| c.name != "risk"));
    }

    #[test]
    fn advisory_pass_fail_disagreement_does_not_route() {
        let _env = RouteEnv::enter();
        let (_dir, r) = start_owned(true);
        let backend = ScriptedBackend::new();
        backend.push_resolve(Ok(Some(owned_pr(OWNED_REF, OWNED_TITLE))));
        backend.push_snapshot(Ok(required_snap(
            &[],
            &[
                ("fmt clippy test", CheckBucket::Fail),
                ("fmt clippy test", CheckBucket::Pass),
            ],
            MergeStateStatus::Clean,
        )));
        backend.push_merge(Ok(MergeResult {
            ok: true,
            queued: false,
            message: "merged".into(),
        }));
        let (_hook, counts) = hook(backend);
        let view = crate::workflow::tick(&r).unwrap().expect("green merge");
        assert_ne!(view.phase, graph::PHASE_ADDRESS_CI);
        assert_eq!(counts.merge_n(), 1);
        let st = load_run_state(&r).unwrap();
        assert_eq!(st.ci_fix_attempts, 0);
        assert!(st.ci_fix_request.is_none());
    }

    #[test]
    fn unspecified_view_pr_fail_routes_when_flag_on() {
        let _env = RouteEnv::enter();
        let (_dir, r) = start_owned(true);
        let (_hook, counts) = script(
            &r,
            owned_pr(OWNED_REF, OWNED_TITLE),
            items(&[("ci", CheckBucket::Fail)]),
        );
        crate::workflow::tick(&r).unwrap().expect("routed");
        assert_advisory_routed(&r, &counts, "ci");
    }

    #[test]
    fn head_sha_fail_does_not_route() {
        let _env = RouteEnv::enter();
        let (_dir, r) = start_owned(true);
        let (_hook, _counts) = script(
            &r,
            CiTarget::HeadSha {
                sha: "abc111".into(),
            },
            items(&[("ci", CheckBucket::Fail)]),
        );
        crate::workflow::tick(&r).unwrap().expect("head sha stop");
        assert_declined(&r);
    }

    #[test]
    fn foreign_title_does_not_route() {
        let _env = RouteEnv::enter();
        let (_dir, r) = start_owned(true);
        let (_hook, _counts) = script(
            &r,
            owned_pr(OWNED_REF, "fix the build"),
            required_items(
                vec![item(
                    "fmt",
                    CheckBucket::Fail,
                    "rustfmt failed",
                    "https://example/fmt",
                )],
                Vec::new(),
            ),
        );
        crate::workflow::tick(&r).unwrap().expect("foreign title");
        assert_declined(&r);
    }

    #[test]
    fn foreign_head_ref_does_not_route() {
        let _env = RouteEnv::enter();
        let (_dir, r) = start_owned(true);
        let (_hook, counts) = script(
            &r,
            owned_pr("feature/x", OWNED_TITLE),
            required_items(
                vec![item(
                    "fmt",
                    CheckBucket::Fail,
                    "rustfmt failed",
                    "https://example/fmt",
                )],
                Vec::new(),
            ),
        );
        assert!(crate::workflow::tick(&r).unwrap().is_none());
        let st = load_run_state(&r).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        assert_eq!(st.phase, graph::PHASE_CI_WAIT);
        assert_eq!(st.failure_class, None);
        assert_eq!(st.ci_fix_attempts, 0);
        assert!(
            !st.last_event.contains("ci-wait: merged"),
            "last_event={}",
            st.last_event
        );
        assert!(
            !st.last_event.contains("address-ci"),
            "last_event={}",
            st.last_event
        );
        assert!(crate::notify::artifact::existing_path(&r).is_none());
        assert_eq!(counts.checks_n(), 0);
        assert_eq!(counts.merge_n(), 0);
    }
}
