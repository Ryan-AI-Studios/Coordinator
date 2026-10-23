//! One-shot cross-model review gate (track 0011).
//!
//! Runs the Role Binding chain Codex → Claude → OpenCode. Never injects a
//! Grok prompt. Default `cargo test` uses a scripted backend.
//!
//! 0101: the reviewer cwd/prompt and `reviewed_oid` use the track delivery target. A
//! resolution failure is `permission` with no backend call. Publish-only degrade checks
//! `checkpoint::committable_dirt` in the target; a planning-only target never degrades.

pub mod backend;
pub mod parse;
pub mod prompt;
pub mod slot_quality;
pub mod spawn;

use std::path::Path;
use std::time::{Duration, Instant};

#[cfg(test)]
use std::cell::RefCell;
#[cfg(test)]
use std::sync::Arc;

use chrono::{SecondsFormat, Utc};

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
    // 0101: review the track delivery target. Fail-closed resolution is a Permission with
    // no backend call and no review mutations.
    let gate = match crate::workflow::execution_repo::gate_target(record, state) {
        Ok(gate) => gate,
        Err(err) => {
            return apply_failure(
                record,
                state,
                FailureClass::Permission,
                format!(
                    "cross-model: {err}; repair conductor.md or spec metadata, then failure \
                     resolve and run --track {}",
                    state.track_id.as_deref().unwrap_or("<id>")
                ),
            );
        }
    };
    let _ = crate::workflow::execution_repo::note_gate_target(record, &gate);
    let target = &gate.execution;
    let exec_repo = target.prompt_path.clone();

    let remaining = remaining_budget(record, state);
    if remaining < MIN_TIER_BUDGET {
        return apply_failure(
            record,
            state,
            FailureClass::Timeout,
            "cross-model: timeout (remaining < 60s)".into(),
        );
    }

    // 0110: a pending role swap applies at gate start only, before the bindings are read.
    // A tick after a tier was attempted never swaps a later tier mid-gate.
    let gate_start = state
        .review
        .as_ref()
        .map(|r| r.attempted.is_empty())
        .unwrap_or(true);
    if gate_start {
        let _ = crate::harness::roles_swap::apply_pending_for_roles_for(
            graph::cross_model_roles(),
            record,
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

        // 0110 defensive check: `argv_for`'s `_` arm is codex, so a tier bound to any other
        // harness used to run codex silently. That is now a Permission skip with a message.
        if !matches!(
            slug.to_ascii_lowercase().as_str(),
            "codex" | "claude" | "opencode"
        ) {
            saw_permission = true;
            last_note = format!(
                "tier {role} harness {slug} is unsupported (allowed: codex, claude, opencode; \
                 this tier previously ran codex instead)"
            );
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
        // One stamp per tier. Do not reuse it across roles in this loop.
        let reviewed_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        let prompt_meta = prompt::AuditPromptMeta {
            phase: &state.phase,
            reviewed_at: &reviewed_at,
            address_findings_attempts: state.address_findings_attempts,
        };
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
                &prompt_meta,
            ),
            remaining_timeout: remaining,
        };

        any_started = true;
        let t0 = Instant::now();
        let class = match run_tier(record, state, backend, &req, slug, t0)? {
            TierEval::Finished { class, body } => {
                let degrade = class == TierClass::GateFail
                    && parse::is_publish_only_fail(&body)
                    && !target.planning_only
                    && publish_preconditions_ok(track_dir.as_deref(), &target.sample_path);
                let (verdict, event) = if degrade {
                    (
                        "DEGRADED_PUBLISH",
                        format!("cross-model: degraded to ci-wait (publish-only ({slug}))"),
                    )
                } else {
                    match class {
                        TierClass::Pass => ("PASS", format!("cross-model: pass ({slug})")),
                        TierClass::PassWithLows => (
                            "PASS_WITH_LOWS",
                            format!("cross-model: pass with lows ({slug})"),
                        ),
                        _ => ("FAIL", format!("cross-model: gate failed ({slug})")),
                    }
                };
                let reviewed_oid = if target.planning_only {
                    None
                } else {
                    exec_head_oid(&target.sample_path)
                };
                persist_review(record, |rv| {
                    rv.active = Some(slug.to_string());
                    rv.verdict = Some(verdict.into());
                    rv.report = Some(format!("review.{slug}.md"));
                    rv.reviewed_oid = reviewed_oid;
                })?;
                return if class == TierClass::GateFail && !degrade {
                    apply_failure(record, state, FailureClass::Difficulty, event)
                } else {
                    apply_success(record, state, event, OutcomeSource::Adapter)
                };
            }
            TierEval::FallThrough { class, note } => {
                last_note = note;
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

enum TierEval {
    Finished { class: TierClass, body: String },
    FallThrough { class: TierClass, note: String },
}

fn review_body(result: &ReviewResult) -> &str {
    if result.last_message.trim().is_empty() {
        &result.stdout
    } else {
        &result.last_message
    }
}

pub(crate) fn record_slot_dud(record: &ProjectRecord, file: &str, bytes: usize) -> Result<()> {
    let note = format!("{file} ({bytes})");
    persist_review_state(record, |s, rv| {
        if !rv.duds.iter().any(|d| d == &note) {
            rv.duds.push(note);
        }
        s.last_event = truncate_msg(&slot_quality::dud_message(file, bytes));
    })
}

/// 0119: record a plan-review dud note only for the entry that produced it.
///
/// The entry check lives **inside** `persist_review_state`'s closure (which already owns the
/// run-state lock), so this helper never nests a second lock. It is called outside the commit
/// closure that writes the verdict/outcome: the note is diagnostic, the verdict is the cause.
pub(crate) fn record_slot_dud_if(
    record: &ProjectRecord,
    entry: crate::state::PlanReviewEntry,
    file: &str,
    bytes: usize,
) -> Result<()> {
    let note = format!("{file} ({bytes})");
    persist_review_state(record, |s, rv| {
        if s.plan_review_entry_current() != Some(entry) {
            return;
        }
        if !rv.duds.iter().any(|d| d == &note) {
            rv.duds.push(note);
        }
        s.last_event = truncate_msg(&slot_quality::dud_message(file, bytes));
    })
}

fn unlink_cross_model_reports(record: &ProjectRecord, state: &RunState, slug: &str) {
    if let Ok(dir) = crate::workflow::bundle::reviews_dir(record) {
        let _ = std::fs::remove_file(dir.join(format!("cross-model-{slug}.md")));
    }
    if let Some(ref track_id) = state.track_id
        && let Some(track_dir) = graph::resolve_track_dir(record, track_id)
    {
        let _ = std::fs::remove_file(track_dir.join(format!("review.{slug}.md")));
    }
}

fn archive_cross_model_dud(state: &RunState, record: &ProjectRecord, slug: &str, body: &str) {
    if let Some(ref track_id) = state.track_id
        && let Some(track_dir) = graph::resolve_track_dir(record, track_id)
    {
        let _ = std::fs::write(track_dir.join(format!("review.{slug}.dud1.md")), body);
    }
}

fn run_tier(
    record: &ProjectRecord,
    state: &RunState,
    backend: &dyn ReviewBackend,
    req: &ReviewRequest,
    slug: &str,
    t0: Instant,
) -> Result<TierEval> {
    let mut current = req.clone();
    let mut retried = false;
    loop {
        let result = match backend.run(&current) {
            Ok(r) => r,
            Err(e) => {
                return Ok(TierEval::FallThrough {
                    class: classify_error(&e.to_string()),
                    note: truncate_msg(&format!("{slug}: {e}")),
                });
            }
        };
        if result.exit == 124 && spawn::is_reviewer_stall(&result.stderr) {
            crate::harness::journal::record_reviewer_stall(
                record,
                slug,
                &crate::harness::journal::argv_head(&current.command, &[] as &[&str]),
                t0.elapsed().as_millis() as u64,
            );
            persist_review_state(record, |s, rv| {
                if !rv.stalled.iter().any(|x| x == slug) {
                    rv.stalled.push(slug.to_string());
                }
                rv.active = None;
                s.last_event = truncate_msg(&format!("reviewer stall ({slug})"));
            })?;
        }
        if result.exit == 124 || spawn::is_reviewer_stall(&result.stderr) {
            let class = classify_result(&result);
            return Ok(TierEval::FallThrough {
                class,
                note: truncate_msg(&format!("{slug}: {}", classify_note(&result, class))),
            });
        }

        let class = classify_result(&result);
        let body = review_body(&result);
        let would_persist = matches!(
            class,
            TierClass::Pass | TierClass::PassWithLows | TierClass::GateFail
        );
        let schema_dud = crate::review::parse::is_schema_only_json(body);
        if (schema_dud || would_persist)
            && let Err(dud) = slot_quality::check(
                slot_quality::SlotFamily::CrossModel,
                body,
                state.track_id.as_deref(),
            )
        {
            let file = format!("review.{slug}.md");
            archive_cross_model_dud(state, record, slug, body);
            unlink_cross_model_reports(record, state, slug);
            record_slot_dud(record, &file, dud.bytes)?;
            let latest = load_run_state(record).unwrap_or_else(|_| state.clone());
            let remaining = remaining_budget(record, &latest);
            if !retried && remaining >= MIN_TIER_BUDGET {
                retried = true;
                current.remaining_timeout = remaining;
                continue;
            }
            return Ok(TierEval::FallThrough {
                class: TierClass::Crash,
                note: truncate_msg(&slot_quality::dud_message(&file, dud.bytes)),
            });
        }
        if would_persist {
            write_reports(record, state, slug, body)?;
            return Ok(TierEval::Finished {
                class,
                body: body.to_string(),
            });
        }
        return Ok(TierEval::FallThrough {
            class,
            note: truncate_msg(&format!("{slug}: {}", classify_note(&result, class))),
        });
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

/// 0080/0101: the publish-only degrade requires real evidence and a committable-clean
/// target. `committable_dirt` ignores CRLF phantoms (deferred row 148).
fn publish_preconditions_ok(track_dir: Option<&Path>, sample_path: &Path) -> bool {
    let Some(dir) = track_dir else {
        return false;
    };
    let evidence = match std::fs::read_to_string(dir.join("evidence.md")) {
        Ok(s) => !s.trim().is_empty(),
        Err(_) => false,
    };
    evidence
        && crate::checkpoint::committable_dirt(sample_path)
            .map(|paths| paths.is_empty())
            .unwrap_or(false)
}

/// Execution-repo `HEAD` at verdict time; `None` when unreadable (the label then never applies).
fn exec_head_oid(exec_repo: &Path) -> Option<String> {
    let out = crate::git_command::command()
        .args(["rev-parse", "HEAD"])
        .current_dir(exec_repo)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

fn persist_review(record: &ProjectRecord, f: impl FnOnce(&mut ReviewWatchState)) -> Result<()> {
    persist_review_state(record, |_, rv| f(rv))
}

fn persist_review_state(
    record: &ProjectRecord,
    f: impl FnOnce(&mut RunState, &mut ReviewWatchState),
) -> Result<()> {
    with_run_state_lock(record, || {
        let mut s = load_run_state(record)?;
        let mut rv = s.review.take().unwrap_or_default();
        f(&mut s, &mut rv);
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
        let mut r = rec(path);
        r.execution_repo = None;
        r
    }

    fn review_git(cwd: &std::path::Path, args: &[&str]) {
        let out = crate::git_command::command()
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
    }

    /// Real temp Git repo + conductor row; cross-model re-resolves when no frozen target.
    fn seed_review_delivery(r: &ProjectRecord, track: &str) -> std::path::PathBuf {
        let repo = r.execution_repo.clone().unwrap_or_else(|| r.path.clone());
        std::fs::create_dir_all(&repo).unwrap();
        if !repo.join(".git").is_dir() {
            review_git(&repo, &["init", "-b", "main"]);
            review_git(&repo, &["config", "user.email", "probe@example.com"]);
            review_git(&repo, &["config", "user.name", "probe"]);
            if !repo.join("f.txt").is_file() {
                std::fs::write(repo.join("f.txt"), "base\n").unwrap();
            }
            review_git(&repo, &["add", "."]);
            review_git(&repo, &["commit", "-m", "base"]);
        }
        let conductor = r.path.join("conductor");
        std::fs::create_dir_all(&conductor).unwrap();
        let path = conductor.join("conductor.md");
        let row = format!(
            "| [{track}-Example]({track}-Example/spec.md) | `{}` | **Ready - not started** | fixture |\n",
            repo.display()
        );
        if !path.is_file() {
            std::fs::write(
                &path,
                format!(
                    "| Track | Execution | Status | Summary |\n| --- | --- | --- | --- |\n{row}"
                ),
            )
            .unwrap();
        } else {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            if !text.contains(&format!("[{track}-Example]")) {
                std::fs::write(&path, format!("{text}{row}")).unwrap();
            }
        }
        repo
    }

    fn jump_cross_model(r: &ProjectRecord, driver: WorkflowDriver) {
        let repo = seed_review_delivery(r, "0011");
        run_with_driver(r, Some("0011".into()), driver).unwrap();
        let mut s = load_run_state(r).unwrap();
        s.phase = graph::PHASE_CROSS_MODEL.into();
        s.last_driven_phase = None;
        s.delivery_target = Some(crate::workflow::execution_repo::seeded_delivery_target(
            r,
            "0011",
            s.run_epoch,
            graph::PHASE_CROSS_MODEL,
            &repo,
        ));
        crate::workflow::execution_repo::assert_fixture_has_delivery_target(&s);
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
            last_message: "## Verdict: PASS WITH DEFERRED P3\n\n## Findings\n\nP3 leftover\n"
                .into(),
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

    fn stall_text() -> ReviewResult {
        ReviewResult {
            exit: 124,
            stdout: String::new(),
            stderr: "reviewer stall — no progress for 600s".into(),
            last_message: String::new(),
        }
    }

    fn timeout_text() -> ReviewResult {
        ReviewResult {
            exit: 124,
            stdout: String::new(),
            stderr: "review CLI timed out after 60s".into(),
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
    fn primary_stall_secondary_pass_stamps_stalled_not_stalled_at() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        jump_cross_model(&r, WorkflowDriver::Adapter);
        let scripted = ScriptedBackend::new()
            .push_ok(stall_text())
            .push_ok(pass_text());
        let (_hook, counts) = hook(scripted);
        let view = crate::workflow::tick(&r).unwrap().expect("pass");
        assert_eq!(view.phase, graph::PHASE_CI_WAIT);
        assert!(view.last_event.contains("cross-model: pass (claude)"));
        assert_eq!(counts.n(), 2);
        assert_eq!(counts.slugs(), vec!["codex", "claude"]);
        let st = load_run_state(&r).unwrap();
        assert_eq!(
            st.review.as_ref().map(|x| x.stalled.clone()),
            Some(vec!["codex".into()])
        );
        assert!(st.stalled_at.is_none());
        assert!(
            view.review
                .as_ref()
                .map(|v| v.stalled == ["codex"])
                .unwrap_or(false),
            "status review.stalled={:?}",
            view.review
        );
        let attempted = st.review.as_ref().map(|x| x.attempted.clone());
        assert!(
            attempted
                .as_ref()
                .is_some_and(|a| a.iter().any(|s| s == "codex") && a.iter().any(|s| s == "claude")),
            "attempted={attempted:?}"
        );
        clear_env();
    }

    #[test]
    fn timeout_124_without_stall_marker_does_not_stamp_stalled() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        jump_cross_model(&r, WorkflowDriver::Adapter);
        let scripted = ScriptedBackend::new()
            .push_ok(timeout_text())
            .push_ok(pass_text());
        let (_hook, counts) = hook(scripted);
        let view = crate::workflow::tick(&r).unwrap().expect("pass");
        assert_eq!(view.phase, graph::PHASE_CI_WAIT);
        assert_eq!(counts.n(), 2);
        let st = load_run_state(&r).unwrap();
        assert!(
            st.review
                .as_ref()
                .map(|x| x.stalled.is_empty())
                .unwrap_or(true),
            "stalled={:?}",
            st.review.as_ref().map(|x| &x.stalled)
        );
        assert!(st.stalled_at.is_none());
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
        let track = dir.path().join("conductor").join("0011-Example");
        std::fs::create_dir_all(&track).unwrap();
        let r = rec(dir.path());
        jump_cross_model(&r, WorkflowDriver::Adapter);
        let scripted = ScriptedBackend::new()
            .push_ok(fail_text())
            .push_ok(pass_text());
        let (_hook, counts) = hook(scripted);
        let view = crate::workflow::tick(&r).unwrap().expect("fail");
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.phase, graph::PHASE_ADDRESS_FINDINGS);
        assert!(view.failure_class.is_none());
        assert!(
            view.last_event.contains("address-findings 1/2"),
            "last_event={}",
            view.last_event
        );
        assert_eq!(counts.n(), 1);
        assert!(track.join("review.codex.gate1.md").is_file());
        assert!(!track.join("review.codex.md").exists());
        assert!(!track.join("review.md").exists());
        assert!(crate::notify::artifact::existing_path(&r).is_none());
        clear_env();
    }

    #[test]
    fn gate_fail_then_address_success_fresh_pass() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        let track = dir.path().join("conductor").join("0011-Example");
        std::fs::create_dir_all(&track).unwrap();
        let r = rec(dir.path());
        jump_cross_model(&r, WorkflowDriver::Adapter);
        let token = "UNIQUE_TOKEN_fresh_gate_body";
        let mut fail = fail_text();
        fail.last_message = format!("## Verdict: FAIL\n\n## Findings\n\n{token}\n");
        let mut pass = pass_text();
        pass.last_message = "## Verdict: PASS\n\n## Findings\n\nsecond gate body\n".into();
        let scripted = ScriptedBackend::new().push_ok(fail).push_ok(pass);
        let (_hook, counts) = hook(scripted);
        let bounce = crate::workflow::tick(&r).unwrap().expect("bounce");
        assert_eq!(bounce.phase, graph::PHASE_ADDRESS_FINDINGS);
        assert_eq!(counts.n(), 1);
        let reviews = crate::state::resolve_state_dir(&r).unwrap().join("reviews");
        let gate_track = std::fs::read_to_string(track.join("review.codex.gate1.md")).unwrap();
        let gate_state =
            std::fs::read_to_string(reviews.join("cross-model-codex.gate1.md")).unwrap();
        assert!(gate_track.contains(token), "track archive={gate_track}");
        assert!(gate_state.contains(token), "state archive={gate_state}");
        assert!(!track.join("review.codex.md").exists());
        assert!(!reviews.join("cross-model-codex.md").exists());
        let success = PhaseOutcome::success(
            graph::PHASE_ADDRESS_FINDINGS,
            OutcomeSource::Test,
            Some("addressed".into()),
            None,
            None,
        );
        let after = write_and_apply(&r, success).unwrap();
        assert_eq!(after.phase, graph::PHASE_CROSS_MODEL);
        assert!(load_run_state(&r).unwrap().review.is_none());
        let view = crate::workflow::tick(&r).unwrap().expect("fresh pass");
        assert_eq!(view.phase, graph::PHASE_CI_WAIT);
        assert!(counts.n() >= 2, "second gate must spawn, n={}", counts.n());
        let live = std::fs::read_to_string(track.join("review.codex.md")).unwrap();
        assert!(
            !live.contains(token),
            "fresh live report leaked token: {live}"
        );
        assert!(
            std::fs::read_to_string(track.join("review.codex.gate1.md"))
                .unwrap()
                .contains(token)
        );
        clear_env();
    }

    #[test]
    fn gate_fail_cap_stops() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("conductor").join("0011-Example")).unwrap();
        let r = rec(dir.path());
        jump_cross_model(&r, WorkflowDriver::Adapter);
        let scripted = ScriptedBackend::new()
            .push_ok(fail_text())
            .push_ok(fail_text())
            .push_ok(fail_text())
            .push_ok(pass_text());
        let (_hook, counts) = hook(scripted);
        crate::workflow::tick(&r).unwrap().expect("bounce1");
        write_and_apply(
            &r,
            PhaseOutcome::success(
                graph::PHASE_ADDRESS_FINDINGS,
                OutcomeSource::Test,
                None,
                None,
                None,
            ),
        )
        .unwrap();
        crate::workflow::tick(&r).unwrap().expect("bounce2");
        write_and_apply(
            &r,
            PhaseOutcome::success(
                graph::PHASE_ADDRESS_FINDINGS,
                OutcomeSource::Test,
                None,
                None,
                None,
            ),
        )
        .unwrap();
        let view = crate::workflow::tick(&r).unwrap().expect("cap");
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.failure_class, Some(FailureClass::Difficulty));
        assert!(
            view.last_event.contains("address-findings exhausted"),
            "last_event={}",
            view.last_event
        );
        assert!(crate::notify::artifact::existing_path(&r).is_some());
        assert_eq!(counts.n(), 3);
        let none = crate::workflow::tick(&r).unwrap();
        assert!(none.is_none());
        assert_eq!(counts.n(), 3);
        clear_env();
    }

    #[test]
    fn gate_fail_at_cap_stops_fast_path() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("conductor").join("0011-Example")).unwrap();
        let r = rec(dir.path());
        jump_cross_model(&r, WorkflowDriver::Adapter);
        let mut s = load_run_state(&r).unwrap();
        s.address_findings_attempts = graph::ADDRESS_FINDINGS_CAP;
        save_run_state(&r, &s).unwrap();
        let scripted = ScriptedBackend::new()
            .push_ok(fail_text())
            .push_ok(pass_text());
        let (_hook, counts) = hook(scripted);
        let view = crate::workflow::tick(&r).unwrap().expect("cap");
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.failure_class, Some(FailureClass::Difficulty));
        assert!(view.last_event.contains("address-findings exhausted"));
        assert!(crate::notify::artifact::existing_path(&r).is_some());
        assert_eq!(counts.n(), 1);
        clear_env();
    }

    #[test]
    fn pause_then_gate_fail_holds_at_address_findings() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("conductor").join("0011-Example")).unwrap();
        let r = rec(dir.path());
        jump_cross_model(&r, WorkflowDriver::Adapter);
        run::pause(&r).unwrap();
        let scripted = ScriptedBackend::new().push_ok(fail_text());
        let (_hook, counts) = hook(scripted);
        let view = crate::workflow::tick(&r).unwrap().expect("bounce");
        assert_eq!(view.status, RunStatus::Paused);
        assert_eq!(view.phase, graph::PHASE_ADDRESS_FINDINGS);
        assert_eq!(counts.n(), 1);
        assert!(crate::notify::artifact::existing_path(&r).is_none());
        clear_env();
    }

    #[test]
    fn address_findings_difficulty_stops() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0011".into()), WorkflowDriver::FileWait).unwrap();
        let mut s = load_run_state(&r).unwrap();
        s.phase = graph::PHASE_ADDRESS_FINDINGS.into();
        s.address_findings_attempts = 1;
        save_run_state(&r, &s).unwrap();
        let view = write_and_apply(
            &r,
            PhaseOutcome::failure(
                graph::PHASE_ADDRESS_FINDINGS,
                FailureClass::Difficulty,
                OutcomeSource::File,
                Some("still stuck".into()),
                None,
            ),
        )
        .unwrap();
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.phase, graph::PHASE_ADDRESS_FINDINGS);
        assert_eq!(view.failure_class, Some(FailureClass::Difficulty));
        assert_eq!(load_run_state(&r).unwrap().address_findings_attempts, 1);
        assert!(crate::notify::artifact::existing_path(&r).is_some());
    }

    #[test]
    fn fresh_run_clears_address_attempts() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, None, WorkflowDriver::FileWait).unwrap();
        let mut s = load_run_state(&r).unwrap();
        s.address_findings_attempts = 2;
        save_run_state(&r, &s).unwrap();
        run::stop(&r).unwrap();
        run_with_driver(&r, None, WorkflowDriver::FileWait).unwrap();
        assert_eq!(load_run_state(&r).unwrap().address_findings_attempts, 0);
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
    fn review_resolution_failure_applies_permission_without_spawn() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        // Deliberate row-less failure fixture: the guard is not applied here.
        let r = rec_no_exec(dir.path());
        run_with_driver(&r, Some("0011".into()), WorkflowDriver::Adapter).unwrap();
        let mut s = load_run_state(&r).unwrap();
        s.phase = graph::PHASE_CROSS_MODEL.into();
        s.last_driven_phase = None;
        s.delivery_target = None;
        save_run_state(&r, &s).unwrap();
        let scripted = ScriptedBackend::new().push_ok(pass_text());
        let (_hook, counts) = hook(scripted);
        let view = crate::workflow::tick(&r).unwrap().expect("perm");
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.failure_class, Some(FailureClass::Permission));
        assert!(
            view.last_event.contains("leg repo unresolved:"),
            "{}",
            view.last_event
        );
        assert_eq!(counts.n(), 0);
        clear_env();
    }

    #[test]
    fn cross_model_tier_bound_to_an_unsupported_harness_is_permission_with_zero_spawns() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        jump_cross_model(&r, WorkflowDriver::Adapter);
        // Before 0110 this config fell through `argv_for`'s `_` arm and ran codex.
        let mut cfg = crate::config::load_machine_config().unwrap();
        for role in graph::cross_model_roles() {
            let binding = cfg.role_bindings.get_mut(*role).unwrap();
            binding.harness = "grok".into();
            binding.command = "grok".into();
            binding.model = None;
        }
        crate::config::save_machine_config(&cfg).unwrap();
        let scripted = ScriptedBackend::new().push_ok(pass_text());
        let (_hook, counts) = hook(scripted);
        let view = crate::workflow::tick(&r).unwrap().expect("permission");
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.failure_class, Some(FailureClass::Permission));
        assert_eq!(counts.n(), 0, "unsupported tier must not spawn");
        assert!(
            view.last_event.contains("all reviewers unavailable"),
            "{}",
            view.last_event
        );
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

    fn schema_only_pass() -> ReviewResult {
        ReviewResult {
            exit: 0,
            stdout: String::new(),
            stderr: String::new(),
            last_message: r#"{"verdict":"PASS","highest":"None"}"#.into(),
        }
    }

    fn schema_only_deferred() -> ReviewResult {
        ReviewResult {
            exit: 0,
            stdout: String::new(),
            stderr: String::new(),
            last_message: r#"{"verdict":"PASS_WITH_DEFERRED_P3","highest":"low"}"#.into(),
        }
    }

    #[test]
    fn cross_model_dud_retries_then_pass() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("conductor").join("0011-Example")).unwrap();
        let r = rec(dir.path());
        jump_cross_model(&r, WorkflowDriver::Adapter);
        let scripted = ScriptedBackend::new()
            .push_ok(schema_only_pass())
            .push_ok(pass_text());
        let (_hook, counts) = hook(scripted);
        let view = crate::workflow::tick(&r).unwrap().expect("pass");
        assert_eq!(view.phase, graph::PHASE_CI_WAIT);
        assert!(view.last_event.contains("cross-model: pass (codex)"));
        assert_eq!(counts.n(), 2);
        assert_eq!(counts.slugs(), vec!["codex", "codex"]);
        let st = load_run_state(&r).unwrap();
        let duds = st
            .review
            .as_ref()
            .map(|x| x.duds.clone())
            .unwrap_or_default();
        assert!(
            duds.iter().any(|d| d.contains("review.codex.md")),
            "duds={duds:?}"
        );
        assert!(
            !dir.path()
                .join("conductor")
                .join("0011-Example")
                .join("review.codex.md")
                .exists()
                || std::fs::read_to_string(
                    dir.path()
                        .join("conductor")
                        .join("0011-Example")
                        .join("review.codex.md")
                )
                .unwrap()
                .contains("## Verdict:")
        );
        clear_env();
    }

    #[test]
    fn cross_model_dud_dud_falls_through_to_next_tier() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        jump_cross_model(&r, WorkflowDriver::Adapter);
        let scripted = ScriptedBackend::new()
            .push_ok(schema_only_pass())
            .push_ok(schema_only_pass())
            .push_ok(pass_text());
        let (_hook, counts) = hook(scripted);
        let view = crate::workflow::tick(&r).unwrap().expect("pass");
        assert_eq!(view.phase, graph::PHASE_CI_WAIT);
        assert!(view.last_event.contains("cross-model: pass (claude)"));
        assert_eq!(counts.n(), 3);
        assert_eq!(counts.slugs(), vec!["codex", "codex", "claude"]);
        let st = load_run_state(&r).unwrap();
        let duds = st
            .review
            .as_ref()
            .map(|x| x.duds.clone())
            .unwrap_or_default();
        assert!(
            duds.iter().any(|d| d.contains("review.codex.md")),
            "duds={duds:?}"
        );
        clear_env();
    }

    #[test]
    fn schema_only_pass_json_is_dud_not_pass() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        jump_cross_model(&r, WorkflowDriver::Adapter);
        let scripted = ScriptedBackend::new()
            .push_ok(schema_only_pass())
            .push_ok(schema_only_pass())
            .push_ok(schema_only_deferred())
            .push_ok(schema_only_deferred())
            .push_ok(pass_text());
        let (_hook, counts) = hook(scripted);
        let view = crate::workflow::tick(&r).unwrap().expect("pass");
        assert!(view.last_event.contains("cross-model: pass (opencode)"));
        assert_eq!(counts.n(), 5);
        assert!(!view.last_event.contains("cross-model: pass (codex)"));
        assert!(!view.last_event.contains("cross-model: pass (claude)"));
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

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let out = crate::git_command::command()
            .args(args)
            .current_dir(dir)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn init_clean_git(dir: &std::path::Path) {
        run_git(dir, &["init"]);
        run_git(dir, &["config", "user.email", "test@example.com"]);
        run_git(dir, &["config", "user.name", "Test"]);
        std::fs::write(dir.join("README"), "x").unwrap();
        run_git(dir, &["add", "-A"]);
        run_git(dir, &["-c", "commit.gpgsign=false", "commit", "-m", "init"]);
    }

    fn write_evidence(workspace: &std::path::Path) {
        let track = workspace.join("conductor").join("0011-Example");
        std::fs::create_dir_all(&track).unwrap();
        std::fs::write(track.join("evidence.md"), "coordinator 0.1.0\n").unwrap();
    }

    fn rec_clean_exec(workspace: &std::path::Path, with_evidence: bool) -> ProjectRecord {
        let exec = workspace.join("exec");
        std::fs::create_dir_all(&exec).unwrap();
        std::fs::create_dir_all(workspace.join("conductor").join("0011-Example")).unwrap();
        if with_evidence {
            write_evidence(workspace);
        }
        init_clean_git(&exec);
        let mut r = rec(workspace);
        r.execution_repo = Some(exec);
        r
    }

    fn publish_fail_body() -> ReviewResult {
        ReviewResult {
            exit: 0,
            stdout: String::new(),
            stderr: String::new(),
            last_message: "## Verdict: FAIL\n\n## Findings\n\nNone at P0–P2 against the implementation itself.\n\n## Publish Status\n\nPR not squash-merged; registry still Ready; CI pending.\n".into(),
        }
    }

    #[test]
    fn publish_only_fail_degrades_to_ci_wait() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        let r = rec_clean_exec(dir.path(), true);
        jump_cross_model(&r, WorkflowDriver::Adapter);
        let scripted = ScriptedBackend::new().push_ok(publish_fail_body());
        let (_hook, counts) = hook(scripted);
        let view = crate::workflow::tick(&r).unwrap().expect("degrade");
        assert_eq!(view.phase, graph::PHASE_CI_WAIT);
        assert_ne!(view.status, RunStatus::Stopped);
        assert!(crate::notify::artifact::existing_path(&r).is_none());
        assert!(
            view.last_event
                .contains("degraded to ci-wait (publish-only"),
            "last_event={}",
            view.last_event
        );
        assert_eq!(
            view.review.as_ref().and_then(|rv| rv.verdict.as_deref()),
            Some("DEGRADED_PUBLISH")
        );
        assert_eq!(counts.n(), 1);
        clear_env();
    }

    #[test]
    fn publish_plus_p1_still_gate_fail() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        let r = rec_clean_exec(dir.path(), true);
        jump_cross_model(&r, WorkflowDriver::Adapter);
        let mut body = publish_fail_body();
        body.last_message = "## Verdict: FAIL\n\n## Findings\n\nNone at P0–P2 against the implementation itself.\n| P1 | broken |\n\n## Publish Status\n\nPR not squash-merged.\n".into();
        let scripted = ScriptedBackend::new().push_ok(body);
        let (_hook, _counts) = hook(scripted);
        let view = crate::workflow::tick(&r).unwrap().expect("fail");
        assert_eq!(view.phase, graph::PHASE_ADDRESS_FINDINGS);
        assert!(
            view.last_event.contains("address-findings 1/2"),
            "last_event={}",
            view.last_event
        );
        assert!(!view.last_event.contains("degraded"));
        assert!(crate::notify::artifact::existing_path(&r).is_none());
        clear_env();
    }

    #[test]
    fn publish_shape_without_preconditions_stays_gate_fail() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        let r = rec_clean_exec(dir.path(), false);
        jump_cross_model(&r, WorkflowDriver::Adapter);
        let scripted = ScriptedBackend::new().push_ok(publish_fail_body());
        let (_hook, _counts) = hook(scripted);
        let view = crate::workflow::tick(&r).unwrap().expect("no evidence");
        assert_eq!(view.phase, graph::PHASE_ADDRESS_FINDINGS);
        assert!(view.last_event.contains("address-findings 1/2"));

        let dir2 = tempdir().unwrap();
        let r2 = rec_clean_exec(dir2.path(), true);
        std::fs::write(dir2.path().join("exec").join("dirty.txt"), "uncommitted").unwrap();
        jump_cross_model(&r2, WorkflowDriver::Adapter);
        let scripted2 = ScriptedBackend::new().push_ok(publish_fail_body());
        let (_hook2, _c2) = hook(scripted2);
        let view2 = crate::workflow::tick(&r2).unwrap().expect("dirty");
        assert_eq!(view2.phase, graph::PHASE_ADDRESS_FINDINGS);
        assert!(view2.last_event.contains("address-findings 1/2"));
        clear_env();
    }

    #[test]
    fn lifecycle_p1_only_degrades() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        let r = rec_clean_exec(dir.path(), true);
        jump_cross_model(&r, WorkflowDriver::Adapter);
        let body = ReviewResult {
            exit: 0,
            stdout: String::new(),
            stderr: String::new(),
            last_message: "## Verdict: FAIL\n\n## Findings\n\n| P1 | PR not squash-merged |\n"
                .into(),
        };
        let scripted = ScriptedBackend::new().push_ok(body);
        let (_hook, _counts) = hook(scripted);
        let view = crate::workflow::tick(&r).unwrap().expect("degrade");
        assert_eq!(view.phase, graph::PHASE_CI_WAIT);
        assert_eq!(
            view.review.as_ref().and_then(|rv| rv.verdict.as_deref()),
            Some("DEGRADED_PUBLISH")
        );
        clear_env();
    }

    #[test]
    fn gate2_p1_never_degrades() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        let r = rec_clean_exec(dir.path(), true);
        jump_cross_model(&r, WorkflowDriver::Adapter);
        let body = ReviewResult {
            exit: 0,
            stdout: String::new(),
            stderr: String::new(),
            last_message: "## Verdict: FAIL\n\n## Findings\n\n| P1 | evidence.md missing |\n"
                .into(),
        };
        let scripted = ScriptedBackend::new().push_ok(body);
        let (_hook, _counts) = hook(scripted);
        let view = crate::workflow::tick(&r).unwrap().expect("gate2");
        assert_eq!(view.phase, graph::PHASE_ADDRESS_FINDINGS);
        assert!(!view.last_event.contains("degraded"));
        clear_env();
    }

    struct ExecRecorder {
        exec_repo: std::sync::Mutex<Option<std::path::PathBuf>>,
        prompt: std::sync::Mutex<Option<String>>,
        result: ReviewResult,
    }

    impl ReviewBackend for ExecRecorder {
        fn run(&self, req: &ReviewRequest) -> Result<ReviewResult> {
            *self.exec_repo.lock().unwrap() = Some(req.exec_repo.clone());
            *self.prompt.lock().unwrap() = Some(req.prompt.clone());
            Ok(self.result.clone())
        }
    }

    fn cross_model_state_with_target(
        r: &ProjectRecord,
        repo: &std::path::Path,
    ) -> (RunState, crate::workflow::execution_repo::ResolvedExecution) {
        run_with_driver(r, Some("0011".into()), WorkflowDriver::Adapter).unwrap();
        let mut state = load_run_state(r).unwrap();
        state.phase = graph::PHASE_CROSS_MODEL.into();
        state.last_driven_phase = None;
        let target = crate::workflow::execution_repo::seeded_delivery_target(
            r,
            "0011",
            state.run_epoch,
            graph::PHASE_CROSS_MODEL,
            repo,
        );
        state.delivery_target = Some(target.clone());
        save_run_state(r, &state).unwrap();
        (state, target)
    }

    #[test]
    fn review_spawns_in_the_target_repo() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        let primary = rec_clean_exec(&dir.path().join("primary"), false);
        let sibling = rec_clean_exec(&dir.path().join("sibling"), true);
        let mut r = sibling.clone();
        r.execution_repo = primary.execution_repo.clone();
        let (state, target) =
            cross_model_state_with_target(&r, sibling.execution_repo.as_ref().unwrap());
        let recorder = ExecRecorder {
            exec_repo: std::sync::Mutex::new(None),
            prompt: std::sync::Mutex::new(None),
            result: pass_text(),
        };
        let view = drive_with(&r, &state, &recorder).unwrap().expect("pass");
        assert_eq!(view.phase, graph::PHASE_CI_WAIT);
        let spawned = recorder.exec_repo.lock().unwrap().clone().unwrap();
        assert!(crate::workflow::execution_repo::paths_same(
            &spawned,
            &target.prompt_path
        ));
        let prompt = recorder.prompt.lock().unwrap().clone().unwrap();
        assert!(
            prompt.contains(&format!(
                "Product / execution repo (cwd): {}",
                target.prompt_path.display()
            )),
            "{prompt}"
        );
        clear_env();
    }

    #[test]
    fn reviewed_oid_is_the_target_head() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        let exec = dir.path().join("exec");
        std::fs::create_dir_all(&exec).unwrap();
        let mut r = rec(dir.path());
        r.execution_repo = Some(exec.clone());
        init_clean_git(&exec);
        std::fs::create_dir_all(dir.path().join("conductor").join("0011-Example")).unwrap();
        write_evidence(dir.path());
        let (state, target) = cross_model_state_with_target(&r, &exec);
        let head = exec_head_oid(&target.sample_path).expect("target HEAD");
        let scripted = ScriptedBackend::new().push_ok(pass_text());
        let (_hook, _counts) = hook(scripted);
        let _ = crate::workflow::tick(&r).unwrap().expect("pass");
        let saved = load_run_state(&r).unwrap();
        assert_eq!(
            saved.review.as_ref().and_then(|rv| rv.reviewed_oid.clone()),
            Some(head)
        );
        let _ = state;
        clear_env();
    }

    #[test]
    fn publish_only_degrade_uses_the_committable_predicate() {
        let dir = tempdir().unwrap();
        let track = dir.path().join("conductor").join("0011-Example");
        std::fs::create_dir_all(&track).unwrap();
        std::fs::write(track.join("evidence.md"), "evidence\n").unwrap();
        let exec = dir.path().join("exec");
        std::fs::create_dir_all(&exec).unwrap();
        init_clean_git(&exec);
        let dirt = crate::checkpoint::committable_dirt(&exec).unwrap();
        assert!(dirt.is_empty(), "dirt={dirt:?}");
        assert!(publish_preconditions_ok(Some(&track), &exec));
        std::fs::write(exec.join("untracked.rs"), b"x\n").unwrap();
        assert!(!publish_preconditions_ok(Some(&track), &exec));
        std::fs::write(exec.join(".gitignore"), b"untracked.rs\n").unwrap();
        run_git(&exec, &["add", ".gitignore"]);
        run_git(
            &exec,
            &["-c", "commit.gpgsign=false", "commit", "-m", "ignore"],
        );
        assert!(publish_preconditions_ok(Some(&track), &exec));
    }

    #[test]
    fn planning_only_review_cannot_degrade_publish() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        let r = rec_clean_exec(dir.path(), true);
        jump_cross_model(&r, WorkflowDriver::Adapter);
        let mut state = load_run_state(&r).unwrap();
        let mut target = state.delivery_target.clone().expect("target");
        target.planning_only = true;
        state.delivery_target = Some(target);
        save_run_state(&r, &state).unwrap();
        let scripted = ScriptedBackend::new().push_ok(publish_fail_body());
        let (_hook, counts) = hook(scripted);
        let view = crate::workflow::tick(&r).unwrap().expect("fail");
        assert_ne!(view.phase, graph::PHASE_CI_WAIT);
        assert!(!view.last_event.contains("degraded"), "{}", view.last_event);
        assert!(
            view.last_event.contains("gate failed"),
            "phase={} event={}",
            view.phase,
            view.last_event
        );
        assert_eq!(counts.n(), 1);
        clear_env();
    }

    #[test]
    fn review_prompt_names_the_target_repo() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        let exec = dir.path().join("exec");
        std::fs::create_dir_all(&exec).unwrap();
        let mut r = rec(dir.path());
        r.execution_repo = Some(exec.clone());
        init_clean_git(&exec);
        std::fs::create_dir_all(dir.path().join("conductor").join("0011-Example")).unwrap();
        write_evidence(dir.path());
        let (state, target) = cross_model_state_with_target(&r, &exec);
        let recorder = ExecRecorder {
            exec_repo: std::sync::Mutex::new(None),
            prompt: std::sync::Mutex::new(None),
            result: pass_text(),
        };
        let _ = drive_with(&r, &state, &recorder).unwrap().expect("pass");
        let prompt = recorder.prompt.lock().unwrap().clone().unwrap();
        assert!(
            prompt.contains(&format!(
                "Product / execution repo (cwd): {}",
                target.prompt_path.display()
            )),
            "{prompt}"
        );
        clear_env();
    }

    #[test]
    fn review_gate_fixtures_seed_the_delivery_target() {
        let (_home, _g) = adapter_env();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        jump_cross_model(&r, WorkflowDriver::Adapter);
        let state = load_run_state(&r).unwrap();
        crate::workflow::execution_repo::assert_fixture_has_delivery_target(&state);
        clear_env();
    }

    #[test]
    fn publish_status_with_findings_is_not_slot_dud() {
        let body = "## Verdict: FAIL\n\n## Findings\n\nnone\n\n## Publish Status\n\nCI pending.\n";
        assert!(
            crate::review::slot_quality::check(
                crate::review::slot_quality::SlotFamily::CrossModel,
                body,
                Some("0011"),
            )
            .is_ok()
        );
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
