//! Bounded self-check between adapter turns (track **0070**).
//!
//! Opt-in, report-only, and only for `implement` and `address-findings`.
//! A yielded step does not write a phase outcome and does not move the phase clock.
//! `note_progress` is not called from classify or from the self-check log.

use chrono::Utc;

use crate::error::Result;
use crate::harness::abort::stop_reason_is_cancelled;
use crate::registry::ProjectRecord;
use crate::state::{
    RunState, RunStatus, SelfCheckState, load_run_state, save_run_state, with_run_state_lock,
};
use crate::workflow::WorkflowDriver;
use crate::workflow::graph::{PHASE_ADDRESS_FINDINGS, PHASE_IMPLEMENT};

/// Kill switch. `off` (any case) forces the feature off. No other value enables it.
pub const ENV_SELF_CONTINUATION: &str = "COORDINATOR_SELF_CONTINUATION";

/// Accepted `continue` lines before the next one takes the normal success path.
pub const SELF_CHECK_MAX_STEPS: u32 = 8;

const REPORT_REPAIR: &str = "self-check: non-convergence repair-repeat";
const REPORT_PLATEAU: &str = "self-check: non-convergence plateau";
const DIFF_UNREADABLE: &str = "self-check: diff unreadable";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Progress,
    Repair,
    Blocked,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Self::Progress => "progress",
            Self::Repair => "repair",
            Self::Blocked => "blocked",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "progress" => Some(Self::Progress),
            "repair" => Some(Self::Repair),
            "blocked" => Some(Self::Blocked),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Line {
    Continue { finding: String, action: Action },
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decide {
    ContinueStep,
    Complete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Report {
    RepairRepeat,
    Plateau,
}

impl Report {
    fn as_event(self) -> String {
        match self {
            Self::RepairRepeat => REPORT_REPAIR.to_string(),
            Self::Plateau => REPORT_PLATEAU.to_string(),
        }
    }
}

struct StepCost {
    wall_ms: u64,
    tools_ms: u64,
    lines: u64,
    diff_fp: Option<String>,
    diff_unreadable: bool,
}

/// Record flag on, and the env kill switch is not `off`.
pub fn enabled(record: &ProjectRecord) -> bool {
    flag_enabled(
        record.self_continuation,
        std::env::var(ENV_SELF_CONTINUATION).ok().as_deref(),
    )
}

fn flag_enabled(record_flag: bool, env: Option<&str>) -> bool {
    if !record_flag {
        return false;
    }
    !env.is_some_and(|v| v.eq_ignore_ascii_case("off"))
}

/// Paragraph for `phase_prompt`. Empty when the feature is off, so default prompts stay put.
pub fn phase_clause(record: &ProjectRecord) -> String {
    if !enabled(record) {
        return String::new();
    }
    "When a step can resume, end the reply with a last line \
     `self-check: continue finding=<token> action=<progress|repair|blocked>` \
     (`finding` is `none` or a short token; `action` is `progress`, `repair`, or `blocked`).\n\
     Finish the phase with a last line `self-check: done`.\n"
        .to_string()
}

pub fn continuation_prompt(phase: &str, track: &str, step: u32, previous: &str) -> String {
    format!(
        "Coordinator self-check step {step} of {max} for phase `{phase}` track `{track}`.\n\
         Previous line: {previous}\n\
         Continue the same phase. About 10 minutes for this step.\n\
         The last line of your reply must be \
         `self-check: continue finding=<token> action=<progress|repair|blocked>` \
         or `self-check: done`.\n",
        max = SELF_CHECK_MAX_STEPS,
    )
}

/// `last_driven_phase` already matches and a continuation prompt is owed.
pub fn continuation_pending(state: &RunState) -> bool {
    if state.driver != WorkflowDriver::Adapter {
        return false;
    }
    if state.last_driven_phase.as_deref() != Some(state.phase.as_str()) {
        return false;
    }
    if !phase_allowed(&state.phase) {
        return false;
    }
    state
        .self_check
        .as_ref()
        .is_some_and(|sc| sc.pending_inject)
}

/// Clear `pending_inject` and return the continuation prompt. `None` when it is no longer owed.
pub fn claim_continuation(record: &ProjectRecord) -> Result<Option<String>> {
    with_run_state_lock(record, || {
        let mut state = load_run_state(record)?;
        if state.status != RunStatus::Running || !continuation_pending(&state) {
            return Ok(None);
        }
        let Some(sc) = state.self_check.as_mut() else {
            return Ok(None);
        };
        sc.pending_inject = false;
        sc.step_started_at = Some(Utc::now());
        let step = sc.steps.saturating_add(1).min(SELF_CHECK_MAX_STEPS);
        let previous = match (sc.last_finding.as_deref(), sc.last_action.as_deref()) {
            (Some(finding), Some(action)) => {
                format!("self-check: continue finding={finding} action={action}")
            }
            _ => "self-check: continue finding=none action=progress".to_string(),
        };
        let phase = state.phase.clone();
        let track = state
            .track_id
            .clone()
            .unwrap_or_else(|| "(none)".to_string());
        state.updated_at = Utc::now();
        let prompt = continuation_prompt(&phase, &track, step, &previous);
        save_run_state(record, &state)?;
        Ok(Some(prompt))
    })
}

pub fn decide(
    record: &ProjectRecord,
    state: &RunState,
    injected_phase: &str,
    stop_reason: Option<&str>,
    text: &str,
) -> Decide {
    if !enabled(record)
        || state.status != RunStatus::Running
        || state.driver != WorkflowDriver::Adapter
        || state.phase != injected_phase
        || !phase_allowed(injected_phase)
        || stop_reason.is_none()
        || stop_reason_is_cancelled(stop_reason)
    {
        return Decide::Complete;
    }
    let steps = state.self_check.as_ref().map(|sc| sc.steps).unwrap_or(0);
    if steps >= SELF_CHECK_MAX_STEPS {
        return Decide::Complete;
    }
    match parse_line(text) {
        Some(Line::Continue { .. }) => Decide::ContinueStep,
        _ => Decide::Complete,
    }
}

/// Hold a normal success turn. `true` means no phase outcome was written.
pub fn hold_continue(
    record: &ProjectRecord,
    injected_phase: &str,
    stop_reason: Option<&str>,
    text: &str,
) -> Result<bool> {
    let preview = load_run_state(record)?;
    if decide(record, &preview, injected_phase, stop_reason, text) != Decide::ContinueStep {
        return Ok(false);
    }
    with_run_state_lock(record, || {
        let mut state = load_run_state(record)?;
        if decide(record, &state, injected_phase, stop_reason, text) != Decide::ContinueStep {
            return Ok(false);
        }
        let Some(Line::Continue { finding, action }) = parse_line(text) else {
            return Ok(false);
        };
        let cost = measure(record, &state);
        let prev = state.self_check.clone().unwrap_or_default();
        let repairs_in_a_row = if action == Action::Repair {
            prev.repairs_in_a_row.saturating_add(1)
        } else {
            0
        };
        let steps = prev.steps.saturating_add(1);
        let report = classify(&prev, action, cost.diff_fp.as_deref(), cost.lines);
        let step_line = format!(
            "self-check: step {steps} wall_ms={} tools_ms={} lines={}",
            cost.wall_ms, cost.tools_ms, cost.lines
        );
        let report_event = report.map(Report::as_event);
        state.last_event = report_event.clone().unwrap_or_else(|| step_line.clone());
        state.updated_at = Utc::now();
        state.self_check = Some(SelfCheckState {
            steps,
            pending_inject: true,
            repairs_in_a_row,
            last_finding: Some(finding),
            last_action: Some(action.as_str().to_string()),
            report: report_event.clone(),
            last_diff_fp: cost.diff_fp,
            last_journal_lines: cost.lines,
            step_started_at: prev.step_started_at,
        });
        save_run_state(record, &state)?;
        crate::progress_log::append(record, "self-check", &step_line);
        if let Some(event) = report_event {
            crate::progress_log::append(record, "self-check", &event);
        }
        if cost.diff_unreadable {
            crate::progress_log::append(record, "self-check", DIFF_UNREADABLE);
        }
        Ok(true)
    })
}

fn phase_allowed(phase: &str) -> bool {
    phase == PHASE_IMPLEMENT || phase == PHASE_ADDRESS_FINDINGS
}

fn parse_line(text: &str) -> Option<Line> {
    let last = text.lines().map(str::trim).rfind(|line| !line.is_empty())?;
    let rest = last.strip_prefix("self-check:")?.trim();
    if rest == "done" {
        return Some(Line::Done);
    }
    let mut parts = rest.split_whitespace();
    if parts.next()? != "continue" {
        return None;
    }
    let finding = parts.next()?.strip_prefix("finding=")?;
    let action = Action::parse(parts.next()?.strip_prefix("action=")?)?;
    if parts.next().is_some() || !valid_token(finding) {
        return None;
    }
    Some(Line::Continue {
        finding: finding.to_string(),
        action,
    })
}

fn valid_token(token: &str) -> bool {
    let mut chars = token.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() || token.len() > 41 {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

fn classify(
    prev: &SelfCheckState,
    action: Action,
    diff_fp: Option<&str>,
    journal_lines: u64,
) -> Option<Report> {
    if action == Action::Repair && prev.repairs_in_a_row >= 1 {
        return Some(Report::RepairRepeat);
    }
    match (prev.last_diff_fp.as_deref(), diff_fp) {
        (Some(prev_fp), Some(next_fp))
            if prev_fp == next_fp && journal_lines > prev.last_journal_lines =>
        {
            Some(Report::Plateau)
        }
        _ => None,
    }
}

fn measure(record: &ProjectRecord, state: &RunState) -> StepCost {
    let (diff_fp, diff_unreadable) = match crate::worktree::product_git_cwd(record) {
        Some(cwd) => match diff_fingerprint(&cwd) {
            Ok(fp) => (Some(fp), false),
            Err(()) => (None, true),
        },
        None => (None, false),
    };
    let (tools_ms, lines) = journal_tail(record, state);
    let start = state
        .self_check
        .as_ref()
        .and_then(|sc| sc.step_started_at)
        .or(state.phase_started_at);
    let wall_ms = start
        .map(|t| (Utc::now() - t).num_milliseconds().max(0) as u64)
        .unwrap_or(0);
    StepCost {
        wall_ms,
        tools_ms,
        lines,
        diff_fp,
        diff_unreadable,
    }
}

fn journal_tail(record: &ProjectRecord, state: &RunState) -> (u64, u64) {
    let previous = state
        .self_check
        .as_ref()
        .map(|sc| sc.last_journal_lines)
        .unwrap_or(0);
    let track = state.track_id.as_deref().unwrap_or("-");
    let Some(path) = crate::harness::journal::journal_file(
        record,
        &crate::harness::journal::sanitize_track(track),
        state.run_epoch,
    ) else {
        return (0, previous);
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return (0, 0),
        Err(_) => return (0, previous),
    };
    let mut total = 0u64;
    let mut tools = 0u64;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let index = total;
        total = total.saturating_add(1);
        if index >= previous
            && let Ok(row) = serde_json::from_str::<JournalDur>(line)
        {
            tools = tools.saturating_add(row.dur_ms);
        }
    }
    (tools, total)
}

#[derive(serde::Deserialize)]
struct JournalDur {
    dur_ms: u64,
}

pub(crate) fn diff_fingerprint(cwd: &std::path::Path) -> std::result::Result<String, ()> {
    let out = std::process::Command::new("git")
        .args(["diff", "--name-only", "HEAD"])
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .map_err(|_| ())?;
    if !out.status.success() {
        return Err(());
    }
    let text = std::str::from_utf8(&out.stdout).map_err(|_| ())?;
    let mut names: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    names.sort_unstable();
    Ok(fnv1a64_hex(names.join("\n").as_bytes()))
}

fn fnv1a64_hex(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_env_lock;
    use crate::layout::LayoutProfile;
    use crate::state::RunState;
    use chrono::Utc as ChronoUtc;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn sample(on: bool) -> ProjectRecord {
        ProjectRecord {
            id: "p".into(),
            path: PathBuf::from("ws"),
            display_name: None,
            layout_profile: LayoutProfile::Nested,
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
            self_continuation: on,
            ci_fix_routing: false,
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: ChronoUtc::now(),
        }
    }

    fn running(phase: &str) -> RunState {
        let mut state = RunState::idle("p");
        state.status = RunStatus::Running;
        state.phase = phase.into();
        state.driver = WorkflowDriver::Adapter;
        state.phase_started_at = Some(ChronoUtc::now());
        state
    }

    #[test]
    fn env_off_forces_disabled_and_other_values_do_not_enable() {
        assert!(!flag_enabled(false, None));
        assert!(!flag_enabled(false, Some("1")));
        assert!(!flag_enabled(false, Some("off")));
        assert!(flag_enabled(true, None));
        assert!(flag_enabled(true, Some("1")));
        assert!(flag_enabled(true, Some("true")));
        assert!(!flag_enabled(true, Some("off")));
        assert!(!flag_enabled(true, Some("OFF")));
        let _lock = test_env_lock();
        let rec = sample(true);
        unsafe {
            std::env::set_var(ENV_SELF_CONTINUATION, "Off");
        }
        assert!(!enabled(&rec));
        unsafe {
            std::env::remove_var(ENV_SELF_CONTINUATION);
        }
        assert!(enabled(&rec));
        assert!(!enabled(&sample(false)));
    }

    #[test]
    fn parser_table() {
        let ok = "noise\nself-check: continue finding=none action=progress\n";
        assert_eq!(
            parse_line(ok),
            Some(Line::Continue {
                finding: "none".into(),
                action: Action::Progress,
            })
        );
        assert_eq!(
            parse_line("self-check: continue finding=F10 action=repair"),
            Some(Line::Continue {
                finding: "F10".into(),
                action: Action::Repair,
            })
        );
        assert_eq!(
            parse_line("self-check: continue finding=a.b_c-1 action=blocked"),
            Some(Line::Continue {
                finding: "a.b_c-1".into(),
                action: Action::Blocked,
            })
        );
        assert_eq!(parse_line("self-check: done"), Some(Line::Done));
        assert_eq!(parse_line("self-check:  done"), Some(Line::Done));
        assert_eq!(parse_line("done\n\n"), None);
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("SELF-CHECK: done"), None);
        assert_eq!(parse_line("self-check: done extra"), None);
        assert_eq!(
            parse_line("self-check: continue finding=none action=progress extra"),
            None
        );
        assert_eq!(
            parse_line("self-check: continue action=repair finding=F10"),
            None
        );
        assert_eq!(
            parse_line("self-check: continue finding= action=progress"),
            None
        );
        assert_eq!(
            parse_line("self-check: continue finding=has space action=progress"),
            None
        );
        let long = format!(
            "self-check: continue finding={} action=progress",
            "a".repeat(41)
        );
        assert!(parse_line(&long).is_some());
        let too = format!(
            "self-check: continue finding={} action=progress",
            "a".repeat(42)
        );
        assert!(parse_line(&too).is_none());
        assert_eq!(
            parse_line("self-check: continue finding=.no action=progress"),
            None
        );
    }

    #[test]
    fn decide_completes_unless_a_live_continue_is_under_the_cap() {
        let rec = sample(true);
        let state = running(PHASE_IMPLEMENT);
        let line = "self-check: continue finding=none action=progress";
        assert_eq!(
            decide(&rec, &state, PHASE_IMPLEMENT, Some("end_turn"), line),
            Decide::ContinueStep
        );
        assert_eq!(
            decide(
                &sample(false),
                &state,
                PHASE_IMPLEMENT,
                Some("end_turn"),
                line
            ),
            Decide::Complete
        );
        assert_eq!(
            decide(
                &rec,
                &state,
                PHASE_IMPLEMENT,
                Some("end_turn"),
                "self-check: done"
            ),
            Decide::Complete
        );
        assert_eq!(
            decide(&rec, &state, PHASE_IMPLEMENT, Some("end_turn"), "finished"),
            Decide::Complete
        );
        assert_eq!(
            decide(&rec, &state, PHASE_IMPLEMENT, None, line),
            Decide::Complete
        );
        assert_eq!(
            decide(&rec, &state, PHASE_IMPLEMENT, Some("cancelled"), line),
            Decide::Complete
        );
        assert_eq!(
            decide(&rec, &state, "plan", Some("end_turn"), line),
            Decide::Complete
        );
        let mut paused = state.clone();
        paused.status = RunStatus::Paused;
        assert_eq!(
            decide(&rec, &paused, PHASE_IMPLEMENT, Some("end_turn"), line),
            Decide::Complete
        );
        let mut capped = state.clone();
        capped.self_check = Some(SelfCheckState {
            steps: SELF_CHECK_MAX_STEPS,
            ..SelfCheckState::default()
        });
        assert_eq!(
            decide(&rec, &capped, PHASE_IMPLEMENT, Some("end_turn"), line),
            Decide::Complete
        );
        let mut stub = state.clone();
        stub.driver = WorkflowDriver::Stub;
        assert_eq!(
            decide(&rec, &stub, PHASE_IMPLEMENT, Some("end_turn"), line),
            Decide::Complete
        );
        assert_eq!(
            decide(
                &rec,
                &running(PHASE_ADDRESS_FINDINGS),
                PHASE_ADDRESS_FINDINGS,
                Some("end_turn"),
                line
            ),
            Decide::ContinueStep
        );
    }

    #[test]
    fn repair_repeat_prefers_over_plateau_and_f10_then_f11_reports() {
        let mut prev = SelfCheckState::default();
        assert!(classify(&prev, Action::Repair, Some("aa"), 2).is_none());
        prev.repairs_in_a_row = 1;
        prev.last_finding = Some("F10".into());
        prev.last_action = Some("repair".into());
        prev.last_diff_fp = Some("aa".into());
        prev.last_journal_lines = 1;
        assert_eq!(
            classify(&prev, Action::Repair, Some("bb"), 3),
            Some(Report::RepairRepeat),
            "F10 then F11, both repair, reports even when the diff moved"
        );
        assert_eq!(
            classify(&prev, Action::Repair, Some("aa"), 4),
            Some(Report::RepairRepeat),
            "repair-repeat wins when the plateau would also fire"
        );
        prev.repairs_in_a_row = 0;
        prev.last_action = Some("progress".into());
        assert!(classify(&prev, Action::Repair, Some("zz"), 9).is_none());
    }

    #[test]
    fn plateau_requires_equal_fingerprint_and_journal_growth() {
        let prev = SelfCheckState {
            last_diff_fp: Some("aa".into()),
            last_journal_lines: 2,
            ..SelfCheckState::default()
        };
        assert_eq!(
            classify(&prev, Action::Progress, Some("aa"), 3),
            Some(Report::Plateau)
        );
        assert!(classify(&prev, Action::Progress, Some("aa"), 2).is_none());
        assert!(classify(&prev, Action::Blocked, Some("bb"), 8).is_none());
        assert!(classify(&prev, Action::Progress, None, 8).is_none());
        let fresh = SelfCheckState::default();
        assert!(classify(&fresh, Action::Progress, Some("aa"), 1).is_none());
    }

    #[test]
    fn clean_repo_fingerprint_is_the_empty_hash_and_a_non_repo_errors() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .env("GIT_OPTIONAL_LOCKS", "0")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.email", "self-check@example.com"]);
        git(&["config", "user.name", "self-check"]);
        std::fs::write(dir.path().join("README.md"), b"seed\n").unwrap();
        git(&["add", "README.md"]);
        git(&["commit", "-m", "seed"]);
        let clean = diff_fingerprint(dir.path()).unwrap();
        assert_eq!(clean, fnv1a64_hex(b""));
        assert_eq!(diff_fingerprint(dir.path()).unwrap(), clean);
        std::fs::write(dir.path().join("README.md"), b"changed\n").unwrap();
        let dirty = diff_fingerprint(dir.path()).unwrap();
        assert_ne!(dirty, clean);
        assert_eq!(diff_fingerprint(dir.path()).unwrap(), dirty);
        let missing = tempfile::tempdir().unwrap();
        assert!(diff_fingerprint(missing.path()).is_err());
    }

    #[test]
    fn phase_prompt_clause_only_when_enabled_for_the_two_phases() {
        let _lock = test_env_lock();
        unsafe {
            std::env::remove_var(ENV_SELF_CONTINUATION);
        }
        let mut rec = sample(false);
        let off = crate::workflow::prompts::phase_prompt(&rec, PHASE_IMPLEMENT, Some("0070"));
        assert!(!off.contains("self-check:"));
        rec.self_continuation = true;
        let on = crate::workflow::prompts::phase_prompt(&rec, PHASE_IMPLEMENT, Some("0070"));
        assert!(on.contains("self-check: continue"));
        assert!(on.contains("self-check: done"));
        let addr =
            crate::workflow::prompts::phase_prompt(&rec, PHASE_ADDRESS_FINDINGS, Some("0070"));
        assert!(addr.contains("self-check: done"));
        let fold = crate::workflow::prompts::phase_prompt(&rec, "fold", Some("0070"));
        assert!(!fold.contains("self-check: continue"));
        let plan = crate::workflow::prompts::phase_prompt(&rec, "plan", Some("0070"));
        assert!(!plan.contains("self-check: continue"));
        let address_ci = crate::workflow::prompts::phase_prompt(&rec, "address-ci", Some("0071"));
        assert!(address_ci.contains("git push"), "{address_ci}");
        assert!(address_ci.contains("gh pr create"), "{address_ci}");
        assert!(address_ci.contains("gh pr merge"), "{address_ci}");
        assert!(!address_ci.contains("self-check:"), "{address_ci}");
    }

    #[test]
    fn address_ci_does_not_continue_self_check() {
        let mut state = running("address-ci");
        state.last_driven_phase = Some("address-ci".into());
        state.self_check = Some(crate::state::SelfCheckState {
            pending_inject: true,
            steps: 1,
            ..crate::state::SelfCheckState::default()
        });
        assert!(!continuation_pending(&state));
    }

    #[test]
    fn classify_and_log_do_not_create_harness_progress() {
        let dir = tempfile::tempdir().unwrap();
        let rec = ProjectRecord {
            path: dir.path().to_path_buf(),
            ..sample(true)
        };
        let prev = SelfCheckState {
            repairs_in_a_row: 1,
            last_diff_fp: Some("aa".into()),
            last_journal_lines: 1,
            ..SelfCheckState::default()
        };
        assert_eq!(
            classify(&prev, Action::Repair, Some("aa"), 4),
            Some(Report::RepairRepeat)
        );
        crate::progress_log::append(&rec, "self-check", REPORT_REPAIR);
        let sidecar = dir
            .path()
            .join(".coordinator")
            .join("harness-progress.json");
        assert!(!sidecar.exists());
        let log = std::fs::read_to_string(dir.path().join("status.md")).unwrap();
        assert!(log.contains("self-check"));
        assert!(log.contains(REPORT_REPAIR));
    }

    #[test]
    fn idle_run_state_omits_self_check() {
        let state = RunState::idle("p");
        let value = serde_json::to_value(&state).unwrap();
        assert!(value.get("self_check").is_none());
    }

    #[test]
    fn hold_continue_logs_cost_and_git_error_skips_plateau() {
        let _lock = test_env_lock();
        let home = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        let proj = tempfile::tempdir().unwrap();
        let bad_git = tempfile::tempdir().unwrap();
        let mut rec = sample(true);
        rec.path = proj.path().to_path_buf();
        rec.execution_repo = Some(bad_git.path().to_path_buf());
        rec.state_dir = Some(proj.path().join("state"));
        crate::state::ensure_state_dir(&rec).unwrap();
        let mut state = running(PHASE_IMPLEMENT);
        state.track_id = Some("0070".into());
        state.run_epoch = 1;
        let started = state.phase_started_at;
        state.self_check = Some(SelfCheckState {
            steps: 1,
            repairs_in_a_row: 1,
            last_action: Some("repair".into()),
            last_finding: Some("F10".into()),
            last_diff_fp: Some("stale".into()),
            last_journal_lines: 1,
            ..SelfCheckState::default()
        });
        save_run_state(&rec, &state).unwrap();
        let journal = crate::harness::journal::journal_file(&rec, "0070", 1).unwrap();
        std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
        std::fs::write(
            &journal,
            "{\"dur_ms\":10,\"ok\":true}\n{\"dur_ms\":25,\"ok\":true}\n",
        )
        .unwrap();
        let held = hold_continue(
            &rec,
            PHASE_IMPLEMENT,
            Some("end_turn"),
            "self-check: continue finding=F11 action=repair",
        )
        .unwrap();
        assert!(held);
        let saved = load_run_state(&rec).unwrap();
        let snap = saved.self_check.unwrap();
        assert!(snap.pending_inject);
        assert_eq!(snap.steps, 2);
        assert_eq!(snap.report.as_deref(), Some(REPORT_REPAIR));
        assert!(snap.last_diff_fp.is_none());
        assert_eq!(saved.failure_class, None);
        assert_eq!(saved.phase, PHASE_IMPLEMENT);
        assert_eq!(saved.phase_started_at, started);
        let log = std::fs::read_to_string(proj.path().join("status.md")).unwrap();
        assert!(log.contains("tools_ms=25"), "{log}");
        assert!(log.contains("lines=2"), "{log}");
        assert!(log.contains("wall_ms="), "{log}");
        assert!(log.contains(DIFF_UNREADABLE), "{log}");
        assert!(log.contains(REPORT_REPAIR), "{log}");
        let sidecar = proj.path().join("state").join("harness-progress.json");
        assert!(!sidecar.exists());
        unsafe {
            std::env::remove_var(crate::config::ENV_COORDINATOR_HOME);
        }
    }
}
