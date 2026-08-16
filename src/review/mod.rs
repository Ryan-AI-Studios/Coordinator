//! One-shot cross-model review gate (track 0011).
//!
//! Runs the Role Binding chain Codex → Claude → OpenCode. Never injects a
//! Grok prompt. Default `cargo test` uses a scripted backend.

pub mod backend;
pub mod parse;
pub mod prompt;
pub mod spawn;

use std::time::Duration;

#[cfg(test)]
use std::cell::RefCell;
#[cfg(test)]
use std::sync::Arc;

use chrono::Utc;

use crate::error::Result;
use crate::outcome::{
    FailureClass, LAST_EVENT_MESSAGE_CAP, OutcomeSource, PhaseOutcome, write_and_apply,
};
use crate::registry::ProjectRecord;
use crate::state::{
    ReviewWatchState, RunState, StatusView, load_run_state, save_run_state, with_run_state_lock,
};
use crate::workflow::WorkflowDriver;
use crate::workflow::bundle::normalize_newlines;
use crate::workflow::graph::{self, cross_model_roles};
use crate::workflow::timeouts::timeout_for_phase;

pub use backend::{
    CallCounts, RecordingBackend, ReviewBackend, ReviewRequest, ReviewResult, ScriptedBackend,
};
pub use parse::{ParsedVerdict, TierClass, classify_error, classify_result, parse_report};
pub use spawn::LiveCli;

const MIN_TIER_BUDGET: Duration = Duration::from_secs(60);

#[cfg(test)]
thread_local! {
    static TEST_BACKEND: RefCell<Option<Arc<dyn ReviewBackend>>> = const { RefCell::new(None) };
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
pub fn install_test_backend(backend: Arc<dyn ReviewBackend>) -> TestBackendGuard {
    TEST_BACKEND.with(|c| *c.borrow_mut() = Some(backend));
    TestBackendGuard
}

/// Drive one `cross-model-review` tick. Stub synths immediately; file_wait waits.
pub fn drive(record: &ProjectRecord, state: &RunState) -> Result<Option<StatusView>> {
    match state.driver {
        WorkflowDriver::Stub => apply_success(
            record,
            state,
            "cross-model: stub (no review)".into(),
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
            drive_with(record, state, &LiveCli)
        }
    }
}

pub fn drive_with(
    record: &ProjectRecord,
    state: &RunState,
    backend: &dyn ReviewBackend,
) -> Result<Option<StatusView>> {
    let paths = crate::layout::resolve(record);
    let Some(exec_repo) = paths.execution_repo else {
        return apply_failure(
            record,
            state,
            FailureClass::Permission,
            "cross-model: no execution repo".into(),
        );
    };

    let remaining = remaining_budget(record, state);
    if remaining < MIN_TIER_BUDGET {
        return apply_failure(
            record,
            state,
            FailureClass::Timeout,
            "cross-model: timeout (remaining < 60s)".into(),
        );
    }

    let bindings = crate::harness::load_role_bindings()
        .unwrap_or_else(|_| crate::config::default_role_bindings());
    let already = state
        .review
        .as_ref()
        .map(|r| r.attempted.clone())
        .unwrap_or_default();

    let mut saw_exhaustion = false;
    let mut saw_crash = false;
    let mut saw_permission = false;
    let mut any_started = false;
    let mut last_note = String::new();

    for role in cross_model_roles() {
        let Some(binding) = bindings.get(*role) else {
            saw_permission = true;
            continue;
        };
        let slug = binding.harness.trim();
        if slug.is_empty() {
            saw_permission = true;
            continue;
        }
        if already.iter().any(|s| s == slug) {
            continue;
        }
        if binding.command.trim().is_empty() {
            persist_review(record, |rv| {
                if !rv.attempted.iter().any(|s| s == slug) {
                    rv.attempted.push(slug.to_string());
                }
            })?;
            saw_permission = true;
            last_note = format!("empty command ({slug})");
            continue;
        }

        let remaining = remaining_budget(
            record,
            &load_run_state(record).unwrap_or_else(|_| state.clone()),
        );
        if remaining < MIN_TIER_BUDGET {
            if !any_started {
                return apply_failure(
                    record,
                    state,
                    FailureClass::Timeout,
                    "cross-model: timeout (remaining < 60s)".into(),
                );
            }
            break;
        }

        persist_review(record, |rv| {
            if !rv.attempted.iter().any(|s| s == slug) {
                rv.attempted.push(slug.to_string());
            }
            rv.active = Some(slug.to_string());
        })?;

        let deferred = paths.conductor_dir.join("deferred.md");
        let track_dir = state
            .track_id
            .as_deref()
            .and_then(|id| graph::resolve_track_dir(record, id));
        let req = ReviewRequest {
            slug: slug.to_string(),
            harness: binding.harness.clone(),
            command: binding.command.clone(),
            model: binding.model.clone(),
            exec_repo: exec_repo.clone(),
            workspace_root: paths.workspace_root.clone(),
            track_dir: track_dir.clone(),
            prompt: prompt::audit_prompt(
                &paths.workspace_root,
                &exec_repo,
                track_dir.as_deref(),
                state.track_id.as_deref(),
                &deferred,
            ),
            remaining_timeout: remaining,
        };

        any_started = true;
        let class = match backend.run(&req) {
            Ok(result) => {
                let class = classify_result(&result);
                if matches!(
                    class,
                    TierClass::Pass | TierClass::PassWithLows | TierClass::GateFail
                ) {
                    let body = if result.last_message.trim().is_empty() {
                        &result.stdout
                    } else {
                        &result.last_message
                    };
                    write_reports(record, state, slug, body)?;
                    let (verdict, event) = match class {
                        TierClass::Pass => ("PASS", format!("cross-model: pass ({slug})")),
                        TierClass::PassWithLows => (
                            "PASS_WITH_LOWS",
                            format!("cross-model: pass with lows ({slug})"),
                        ),
                        _ => ("FAIL", format!("cross-model: gate failed ({slug})")),
                    };
                    persist_review(record, |rv| {
                        rv.active = Some(slug.to_string());
                        rv.verdict = Some(verdict.into());
                        rv.report = Some(format!("review.{slug}.md"));
                    })?;
                    return if class == TierClass::GateFail {
                        apply_failure(record, state, FailureClass::Difficulty, event)
                    } else {
                        apply_success(record, state, event, OutcomeSource::Adapter)
                    };
                }
                last_note = truncate_msg(&format!("{slug}: {}", classify_note(&result, class)));
                class
            }
            Err(e) => {
                let class = classify_error(&e.to_string());
                last_note = truncate_msg(&format!("{slug}: {e}"));
                class
            }
        };

        match class {
            TierClass::Exhaustion => saw_exhaustion = true,
            TierClass::Permission => saw_permission = true,
            TierClass::Crash => saw_crash = true,
            TierClass::Pass | TierClass::PassWithLows | TierClass::GateFail => {}
        }
    }

    let remaining = remaining_budget(
        record,
        &load_run_state(record).unwrap_or_else(|_| state.clone()),
    );
    if remaining < MIN_TIER_BUDGET && !any_started {
        return apply_failure(
            record,
            state,
            FailureClass::Timeout,
            "cross-model: timeout (remaining < 60s)".into(),
        );
    }
    if remaining < MIN_TIER_BUDGET {
        let msg = if last_note.is_empty() {
            "cross-model: timeout (remaining < 60s)".into()
        } else {
            format!("cross-model: timeout (remaining < 60s); {last_note}")
        };
        return apply_failure(record, state, FailureClass::Timeout, msg);
    }

    if saw_exhaustion {
        let msg = if last_note.is_empty() {
            "cross-model: all tiers exhausted".into()
        } else {
            format!("cross-model: all tiers exhausted ({last_note})")
        };
        return apply_failure(record, state, FailureClass::ModelExhaustion, msg);
    }
    if saw_permission && !saw_crash {
        return apply_failure(
            record,
            state,
            FailureClass::Permission,
            "cross-model: all reviewers unavailable".into(),
        );
    }
    let crash_msg = if last_note.is_empty() {
        "cross-model: all reviewers failed".into()
    } else {
        format!("cross-model: all reviewers failed ({last_note})")
    };
    apply_failure(record, state, FailureClass::HarnessCrash, crash_msg)
}

fn remaining_budget(record: &ProjectRecord, state: &RunState) -> Duration {
    let budget = timeout_for_phase(record, &state.phase);
    let elapsed = state.effective_running_elapsed(Utc::now());
    budget.saturating_sub(elapsed)
}

fn classify_note(result: &ReviewResult, class: TierClass) -> String {
    match class {
        TierClass::Exhaustion => "exhausted".into(),
        TierClass::Permission => "unavailable".into(),
        TierClass::Crash => {
            if result.stderr.trim().is_empty() {
                format!("exit {}", result.exit)
            } else {
                truncate_msg(&result.stderr)
            }
        }
        _ => class_name(class).into(),
    }
}

fn class_name(class: TierClass) -> &'static str {
    match class {
        TierClass::Pass => "pass",
        TierClass::PassWithLows => "pass with lows",
        TierClass::GateFail => "gate fail",
        TierClass::Exhaustion => "exhaustion",
        TierClass::Permission => "permission",
        TierClass::Crash => "crash",
    }
}

fn write_reports(record: &ProjectRecord, state: &RunState, slug: &str, body: &str) -> Result<()> {
    let text = normalize_newlines(body);
    let dir = crate::workflow::bundle::reviews_dir(record)?;
    std::fs::create_dir_all(&dir)?;
    let state_copy = dir.join(format!("cross-model-{slug}.md"));
    crate::persist::atomic_write(&state_copy, text.as_bytes())?;
    if let Some(ref track_id) = state.track_id
        && let Some(track_dir) = graph::resolve_track_dir(record, track_id)
    {
        let copy = track_dir.join(format!("review.{slug}.md"));
        crate::persist::atomic_write(&copy, text.as_bytes())?;
    }
    Ok(())
}

fn persist_review(record: &ProjectRecord, f: impl FnOnce(&mut ReviewWatchState)) -> Result<()> {
    with_run_state_lock(record, || {
        let mut s = load_run_state(record)?;
        let mut rv = s.review.take().unwrap_or_default();
        f(&mut rv);
        s.review = Some(rv);
        s.updated_at = Utc::now();
        save_run_state(record, &s)
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
    use crate::config::{ENV_COORDINATOR_HOME, ENV_COORDINATOR_REVIEW_LIVE, test_env_lock};
    use crate::notify::ENV_COORDINATOR_NOTIFY;
    use crate::run::{self, run_with_driver};
    use crate::state::{RunStatus, load_run_state, save_run_state};
    use crate::watch::poll_once;
    use crate::workflow::graph;
    use crate::workflow::timeouts::ENV_PHASE_TIMEOUT_SECS;
    use std::sync::Arc;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn rec(path: &std::path::Path) -> ProjectRecord {
        ProjectRecord {
            id: Uuid::new_v4().to_string(),
            path: path.to_path_buf(),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: Some(path.to_path_buf()),
            execution_repos: Default::default(),
            state_dir: None,
            auto_merge: true,
            phase_timeouts_secs: Default::default(),
            created_at: Utc::now(),
        }
    }

    fn rec_no_exec(path: &std::path::Path) -> ProjectRecord {
        let mut r = rec(path);
        r.execution_repo = None;
        r
    }

    fn jump_cross_model(r: &ProjectRecord, driver: WorkflowDriver) {
        run_with_driver(r, Some("0011".into()), driver).unwrap();
        let mut s = load_run_state(r).unwrap();
        s.phase = graph::PHASE_CROSS_MODEL.into();
        s.last_driven_phase = None;
        save_run_state(r, &s).unwrap();
    }

    fn pass_text() -> ReviewResult {
        ReviewResult {
            exit: 0,
            stdout: String::new(),
            stderr: String::new(),
            last_message: "## Verdict: PASS\n\n## Findings\n\nnone\n".into(),
        }
    }

    fn lows_text() -> ReviewResult {
        ReviewResult {
            exit: 0,
            stdout: String::new(),
            stderr: String::new(),
            last_message: "## Verdict: PASS WITH DEFERRED P3\n".into(),
        }
    }

    fn fail_text() -> ReviewResult {
        ReviewResult {
            exit: 0,
            stdout: String::new(),
            stderr: String::new(),
            last_message: "## Verdict: FAIL\n\n## Findings\n\n| P1 | broken |\n".into(),
        }
    }

    fn exhaust_text() -> ReviewResult {
        ReviewResult {
            exit: 1,
            stdout: String::new(),
            stderr: "quota exceeded / rate limit".into(),
            last_message: String::new(),
        }
    }

    fn hook(scripted: ScriptedBackend) -> (TestBackendGuard, CallCounts) {
        let rec = RecordingBackend::wrap(Arc::new(scripted));
        let counts = rec.counts.clone();
        let g = install_test_backend(Arc::new(rec));
        (g, counts)
    }

    fn adapter_env() -> (tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
        let home = tempdir().unwrap();
        let g = test_env_lock();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(ENV_COORDINATOR_NOTIFY, "off");
        }
        (home, g)
    }

    fn clear_env() {
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
        }
    }

    #[test]
    fn primary_pass_one_spawn() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("conductor").join("0011-Example")).unwrap();
        let r = rec(dir.path());
        jump_cross_model(&r, WorkflowDriver::Adapter);
        let scripted = ScriptedBackend::new().push_ok(pass_text());
        let (_hook, counts) = hook(scripted);
        let view = crate::workflow::tick(&r).unwrap().expect("pass");
        assert_eq!(view.phase, graph::PHASE_CI_WAIT);
        assert!(view.failure_class.is_none());
        assert!(view.last_event.contains("cross-model: pass (codex)"));
        assert_eq!(counts.n(), 1);
        assert_eq!(counts.slugs(), vec!["codex"]);
        let report = crate::state::resolve_state_dir(&r)
            .unwrap()
            .join("reviews")
            .join("cross-model-codex.md");
        assert!(report.is_file());
        let track = dir
            .path()
            .join("conductor")
            .join("0011-Example")
            .join("review.codex.md");
        assert!(track.is_file());
        assert!(
            !dir.path()
                .join("conductor")
                .join("0011-Example")
                .join("review.md")
                .exists()
        );
        let st = load_run_state(&r).unwrap();
        assert_eq!(
            st.review.as_ref().and_then(|x| x.verdict.as_deref()),
            Some("PASS")
        );
        assert!(view.review.is_some() || st.review.is_some());
        clear_env();
    }

    #[test]
    fn primary_exhaust_secondary_pass() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        jump_cross_model(&r, WorkflowDriver::Adapter);
        let scripted = ScriptedBackend::new()
            .push_ok(exhaust_text())
            .push_ok(pass_text());
        let (_hook, counts) = hook(scripted);
        let view = crate::workflow::tick(&r).unwrap().expect("pass");
        assert_eq!(view.phase, graph::PHASE_CI_WAIT);
        assert!(view.last_event.contains("cross-model: pass (claude)"));
        assert_eq!(counts.n(), 2);
        assert_eq!(counts.slugs(), vec!["codex", "claude"]);
        clear_env();
    }

    #[test]
    fn all_exhaust_applies_model_exhaustion() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        jump_cross_model(&r, WorkflowDriver::Adapter);
        let scripted = ScriptedBackend::new()
            .push_ok(exhaust_text())
            .push_ok(exhaust_text())
            .push_ok(exhaust_text());
        let (_hook, counts) = hook(scripted);
        let view = crate::workflow::tick(&r).unwrap().expect("fail");
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.failure_class, Some(FailureClass::ModelExhaustion));
        assert!(
            view.last_event.contains("all tiers exhausted")
                && view.last_event.contains("opencode: exhausted"),
            "last_event={}",
            view.last_event
        );
        assert!(crate::notify::artifact::existing_path(&r).is_some());
        assert_eq!(counts.n(), 3);
        clear_env();
    }

    #[test]
    fn nonzero_pass_falls_through_to_next_tier() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        jump_cross_model(&r, WorkflowDriver::Adapter);
        let mut crashed_pass = pass_text();
        crashed_pass.exit = 1;
        let scripted = ScriptedBackend::new()
            .push_ok(crashed_pass)
            .push_ok(pass_text());
        let (_hook, counts) = hook(scripted);
        let view = crate::workflow::tick(&r).unwrap().expect("pass");
        assert_eq!(view.phase, graph::PHASE_CI_WAIT);
        assert!(view.last_event.contains("cross-model: pass (claude)"));
        assert_eq!(counts.n(), 2);
        assert_eq!(counts.slugs(), vec!["codex", "claude"]);
        clear_env();
    }

    #[test]
    fn gate_fail_no_fallback() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("conductor").join("0011-Example")).unwrap();
        let r = rec(dir.path());
        jump_cross_model(&r, WorkflowDriver::Adapter);
        let scripted = ScriptedBackend::new()
            .push_ok(fail_text())
            .push_ok(pass_text());
        let (_hook, counts) = hook(scripted);
        let view = crate::workflow::tick(&r).unwrap().expect("fail");
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.failure_class, Some(FailureClass::Difficulty));
        assert!(view.last_event.contains("cross-model: gate failed (codex)"));
        assert_eq!(counts.n(), 1);
        assert!(
            dir.path()
                .join("conductor")
                .join("0011-Example")
                .join("review.codex.md")
                .is_file()
        );
        assert!(
            !dir.path()
                .join("conductor")
                .join("0011-Example")
                .join("review.md")
                .exists()
        );
        clear_env();
    }

    #[test]
    fn pass_with_deferred_succeeds() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        jump_cross_model(&r, WorkflowDriver::Adapter);
        let scripted = ScriptedBackend::new().push_ok(lows_text());
        let (_hook, counts) = hook(scripted);
        let view = crate::workflow::tick(&r).unwrap().expect("pass");
        assert_eq!(view.phase, graph::PHASE_CI_WAIT);
        assert!(
            view.last_event
                .contains("cross-model: pass with lows (codex)")
        );
        assert_eq!(counts.n(), 1);
        clear_env();
    }

    #[test]
    fn remaining_under_60s_zero_spawns_timeout() {
        let (_home, _g) = adapter_env();
        unsafe {
            std::env::set_var(ENV_PHASE_TIMEOUT_SECS, "30");
        }
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        jump_cross_model(&r, WorkflowDriver::Adapter);
        let scripted = ScriptedBackend::new().push_ok(pass_text());
        let (_hook, counts) = hook(scripted);
        let view = crate::workflow::tick(&r).unwrap().expect("timeout");
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.failure_class, Some(FailureClass::Timeout));
        assert_eq!(counts.n(), 0);
        clear_env();
    }

    #[test]
    fn missing_execution_repo_is_permission() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        let r = rec_no_exec(dir.path());
        jump_cross_model(&r, WorkflowDriver::Adapter);
        let scripted = ScriptedBackend::new().push_ok(pass_text());
        let (_hook, counts) = hook(scripted);
        let view = crate::workflow::tick(&r).unwrap().expect("perm");
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.failure_class, Some(FailureClass::Permission));
        assert!(view.last_event.contains("no execution repo"));
        assert_eq!(counts.n(), 0);
        clear_env();
    }

    #[test]
    fn stop_before_spawn_zero_runs() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        jump_cross_model(&r, WorkflowDriver::Adapter);
        let scripted = ScriptedBackend::new().push_ok(pass_text());
        let (_hook, counts) = hook(scripted);
        run::stop(&r).unwrap();
        let tick = crate::workflow::tick(&r).unwrap();
        assert!(tick.is_none());
        assert_eq!(counts.n(), 0);
        assert!(crate::notify::artifact::existing_path(&r).is_none());
        clear_env();
    }

    #[test]
    fn pause_then_pass_holds_at_ci_wait() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        jump_cross_model(&r, WorkflowDriver::Adapter);
        run::pause(&r).unwrap();
        let scripted = ScriptedBackend::new().push_ok(pass_text());
        let (_hook, counts) = hook(scripted);
        let view = crate::workflow::tick(&r).unwrap().expect("pass");
        assert_eq!(view.phase, graph::PHASE_CI_WAIT);
        assert_eq!(view.status, RunStatus::Paused);
        assert_eq!(counts.n(), 1);
        clear_env();
    }

    #[test]
    fn adapter_does_not_prompt_harness() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        jump_cross_model(&r, WorkflowDriver::Adapter);
        let scripted = ScriptedBackend::new().push_ok(pass_text());
        let (_hook, _counts) = hook(scripted);
        crate::workflow::tick(&r).unwrap().expect("pass");
        let st = load_run_state(&r).unwrap();
        assert!(
            st.last_driven_phase.is_none(),
            "cross-model-review must not use inject-once last_driven_phase"
        );
        assert!(crate::harness::status_bundle_sync(&r).is_none());
        clear_env();
    }

    #[test]
    fn file_wait_success_advances() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        jump_cross_model(&r, WorkflowDriver::FileWait);
        assert!(crate::workflow::tick(&r).unwrap().is_none());
        let o = PhaseOutcome::success(
            graph::PHASE_CROSS_MODEL,
            OutcomeSource::File,
            None,
            None,
            None,
        );
        crate::outcome::save_current_outcome(&r, &o).unwrap();
        let view = poll_once(&r).unwrap().expect("file apply");
        assert_eq!(view.phase, graph::PHASE_CI_WAIT);
    }

    #[test]
    fn stub_synths_without_backend() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        jump_cross_model(&r, WorkflowDriver::Stub);
        let view = crate::workflow::tick(&r).unwrap().expect("stub");
        assert_eq!(view.phase, graph::PHASE_CI_WAIT);
        assert!(view.last_event.contains("cross-model: stub (no review)"));
    }

    #[test]
    fn fresh_run_clears_review() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, None, WorkflowDriver::FileWait).unwrap();
        let mut s = load_run_state(&r).unwrap();
        s.review = Some(ReviewWatchState {
            attempted: vec!["codex".into()],
            verdict: Some("PASS".into()),
            ..Default::default()
        });
        save_run_state(&r, &s).unwrap();
        run::stop(&r).unwrap();
        run_with_driver(&r, None, WorkflowDriver::FileWait).unwrap();
        let st = load_run_state(&r).unwrap();
        assert!(st.review.is_none());
    }

    #[test]
    fn status_review_null_when_idle() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        let view = run::status(&r).unwrap();
        assert!(view.review.is_none());
        assert!(!graph::is_grok_bound(graph::PHASE_CROSS_MODEL));
    }

    #[test]
    #[ignore = "requires Codex/Claude/OpenCode on PATH + login; set COORDINATOR_REVIEW_LIVE=1"]
    fn review_live_binaries_resolve() {
        if std::env::var(ENV_COORDINATOR_REVIEW_LIVE).ok().as_deref() != Some("1") {
            return;
        }
        let _ = spawn::resolve_review_bin("codex", "codex");
        let _ = spawn::resolve_review_bin("claude", "claude");
        let _ = spawn::resolve_review_bin("opencode", "opencode");
    }
}
