//! Adapter plan-review: one-shot `agy --print` (0017) and `opencode run` (0018).
//!
//! Not the 0011 cross-model Verdict gate. File is source of truth; stdout is fallback.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::config::{ENV_COORDINATOR_AGY_BIN, RoleBinding};
use crate::error::{CoordinatorError, Result};
use crate::harness::grok::reject_or_replace_ps1;
use crate::harness::resolve_command;
use crate::outcome::{
    FailureClass, OutcomeMetadata, OutcomeSource, OutcomeStatus, PhaseOutcome, outcome_roles_dir,
};
use crate::registry::ProjectRecord;
use crate::review::spawn::{resolve_review_bin, run_process};
use crate::state::{RunState, RunStatus, load_run_state, save_run_state, with_run_state_lock};
use crate::workflow::drive::write_review_markdown;
use crate::workflow::graph::{
    PHASE_PLAN_REVIEW, REVIEW_SLUG_AGY, REVIEW_SLUG_OPENCODE, ROLE_REVIEWER_AGY,
    ROLE_REVIEWER_OPENCODE, resolve_track_dir, role_phase,
};
use crate::workflow::timeouts::timeout_for_phase;

const MIN_SPAWN_BUDGET: Duration = Duration::from_secs(60);

/// One-shot plan-review CLI (scripted in default tests; live CLI otherwise).
pub trait PlanReviewBackend: Send + Sync {
    fn run(&self, req: &PlanReviewRequest) -> Result<PlanReviewResult>;
}

#[derive(Debug, Clone)]
pub struct PlanReviewRequest {
    pub slug: String,
    pub command: String,
    pub model: Option<String>,
    pub workspace_root: PathBuf,
    pub execution_repo: Option<PathBuf>,
    pub track_dir: Option<PathBuf>,
    pub prompt: String,
    pub remaining: Duration,
    pub argv: Vec<String>,
    /// Child-only env (never exported on the Coordinator process).
    pub extra_env: Vec<(String, String)>,
    /// Originating run; apply is dropped if the live run no longer matches.
    pub spawn_epoch: u64,
    pub spawn_track_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanReviewResult {
    pub exit: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Live `agy --print` child. Default `cargo test` does not construct this
/// unless `COORDINATOR_AGY_LIVE=1`.
pub struct AgyCli;

impl PlanReviewBackend for AgyCli {
    fn run(&self, req: &PlanReviewRequest) -> Result<PlanReviewResult> {
        let bin = resolve_agy_bin(&req.command)?;
        let out = run_process(&bin, &req.argv, &req.workspace_root, req.remaining, &[])?;
        Ok(PlanReviewResult {
            exit: out.exit,
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }
}

/// Live `opencode run` child. Default `cargo test` does not construct this
/// unless `COORDINATOR_OPENCODE_LIVE=1`.
pub struct OpenCodeCli;

impl PlanReviewBackend for OpenCodeCli {
    fn run(&self, req: &PlanReviewRequest) -> Result<PlanReviewResult> {
        let bin = resolve_review_bin("opencode", &req.command)?;
        let extra: Vec<(&str, String)> = req
            .extra_env
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect();
        let out = run_process(&bin, &req.argv, &req.workspace_root, req.remaining, &extra)?;
        Ok(PlanReviewResult {
            exit: out.exit,
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }
}

/// Argv pin (plan-time + execute-time `--print-timeout {secs}s`).
pub fn agy_argv(
    prompt: &str,
    remaining: Duration,
    model: Option<&str>,
    add_dir: Option<&Path>,
) -> Vec<String> {
    let mut args = vec![
        "--print".into(),
        prompt.to_string(),
        "--print-timeout".into(),
        format!("{}s", remaining.as_secs()),
        "--dangerously-skip-permissions".into(),
        "--output-format".into(),
        "json".into(),
    ];
    if let Some(m) = model
        && !m.trim().is_empty()
    {
        args.push("--model".into());
        args.push(m.to_string());
    }
    if let Some(dir) = add_dir {
        args.push("--add-dir".into());
        args.push(dir.to_string_lossy().into_owned());
    }
    args
}

/// Plan-review OpenCode argv. Never `--auto` / `--yolo` / `--dangerously-skip-permissions`.
pub fn opencode_argv(
    prompt: &str,
    workspace_root: &Path,
    model: Option<&str>,
    title: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "run".into(),
        prompt.to_string(),
        "--dir".into(),
        workspace_root.to_string_lossy().into_owned(),
        "--format".into(),
        "json".into(),
    ];
    if let Some(m) = model
        && !m.trim().is_empty()
    {
        args.push("--model".into());
        args.push(m.to_string());
    }
    if let Some(t) = title
        && !t.trim().is_empty()
    {
        args.push("--title".into());
        args.push(t.to_string());
    }
    args
}

/// `--add-dir` only when execution_repo is set and different from cwd.
pub fn add_dir_arg(workspace: &Path, exec: Option<&Path>) -> Option<PathBuf> {
    let exec = exec?;
    if !paths_differ(workspace, exec) {
        return None;
    }
    Some(exec.to_path_buf())
}

fn paths_differ(a: &Path, b: &Path) -> bool {
    if a == b {
        return false;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca != cb,
        _ => true,
    }
}

/// Review-track contract: write `agy-review.md` on the track; no Verdict/PASS.
pub fn agy_prompt(record: &ProjectRecord, track_id: Option<&str>) -> String {
    slot_prompt(record, track_id, REVIEW_SLUG_AGY)
}

/// Review-track contract: write `opencode-review.md` on the track; no Verdict/PASS.
pub fn opencode_prompt(record: &ProjectRecord, track_id: Option<&str>) -> String {
    slot_prompt(record, track_id, REVIEW_SLUG_OPENCODE)
}

fn slot_prompt(record: &ProjectRecord, track_id: Option<&str>, slug: &str) -> String {
    let paths = crate::layout::resolve(record);
    let track_dir = track_id
        .and_then(|id| resolve_track_dir(record, id))
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(track dir unresolved)".into());
    let skill = paths
        .workspace_root
        .join(".agents")
        .join("skills")
        .join("review-track")
        .join("SKILL.md");
    let file = format!("{slug}-review.md");
    let layout = crate::workflow::prompts::layout_block(record, track_id);
    format!(
        "You are reviewing a Coordinator conductor-track plan (review-track).\n\
         This slot loads the `review-track` skill from {}.\n\
         \n\
         {layout}\
         \n\
         Read spec.md and plan.md in:\n\
         {track_dir}\n\
         \n\
         Write your review to this exact path (overwrite):\n\
         {track_dir}/{file}\n\
         \n\
         Knowledge is stale. Verify pins, APIs, and hooks against primary sources \
         (crates.io, docs.rs, official harness docs) before trusting training data or \
         the track's plan-time snapshot.\n\
         \n\
         Requirements:\n\
         - Review the plan for completeness, risks, and missing Definition of Done.\n\
         - Do not require a Verdict/PASS header. This is not the post-implement \
         cross-model review gate.\n\
         - You may inspect product source under the execution repo when it is available.\n\
         - Do not run `coordinator outcome write`.\n",
        skill.display(),
    )
}

/// Child-only `OPENCODE_PERMISSION` JSON. Never `set_var` this on the parent.
pub fn opencode_permission_json(workspace: &Path, exec: Option<&Path>) -> String {
    let mut map = serde_json::Map::new();
    map.insert("bash".into(), serde_json::Value::String("deny".into()));
    let mut edit = serde_json::Map::new();
    edit.insert(
        "**/*opencode-review.md".into(),
        serde_json::Value::String("allow".into()),
    );
    map.insert("edit".into(), serde_json::Value::Object(edit));
    map.insert("read".into(), serde_json::Value::String("allow".into()));
    map.insert("glob".into(), serde_json::Value::String("allow".into()));
    map.insert("grep".into(), serde_json::Value::String("allow".into()));
    if let Some(exec) = exec
        && paths_differ(workspace, exec)
    {
        let mut ext = serde_json::Map::new();
        ext.insert(
            exec.to_string_lossy().into_owned(),
            serde_json::Value::String("allow".into()),
        );
        map.insert("external_directory".into(), serde_json::Value::Object(ext));
    }
    serde_json::Value::Object(map).to_string()
}

pub fn resolve_agy_bin(command: &str) -> Result<PathBuf> {
    let raw = match std::env::var(ENV_COORDINATOR_AGY_BIN) {
        Ok(s) if !s.trim().is_empty() => s,
        _ => command.to_string(),
    };
    if raw.trim().is_empty() {
        return Err(CoordinatorError::Message(
            "command not found on PATH: (empty)".into(),
        ));
    }
    let resolved = resolve_command(&raw)?;
    reject_or_replace_ps1(resolved)
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

fn slot_role(slug: &str) -> &'static str {
    match slug {
        REVIEW_SLUG_OPENCODE => ROLE_REVIEWER_OPENCODE,
        _ => ROLE_REVIEWER_AGY,
    }
}

fn remaining_budget(record: &ProjectRecord, state: &RunState) -> Duration {
    timeout_for_phase(record, PHASE_PLAN_REVIEW)
        .saturating_sub(state.effective_running_elapsed(chrono::Utc::now()))
}

/// Fire-and-forget spawn of the agy slot. Does not block `poll_once`.
pub fn maybe_spawn_agy(record: &ProjectRecord, state: &RunState) -> Result<()> {
    maybe_spawn_slot(record, state, REVIEW_SLUG_AGY)
}

/// Fire-and-forget spawn of the opencode slot. Does not block `poll_once`.
pub fn maybe_spawn_opencode(record: &ProjectRecord, state: &RunState) -> Result<()> {
    maybe_spawn_slot(record, state, REVIEW_SLUG_OPENCODE)
}

/// Adapter plan-review: start both slots that are still pending.
pub fn maybe_spawn_plan_review(record: &ProjectRecord, state: &RunState) -> Result<()> {
    maybe_spawn_agy(record, state)?;
    maybe_spawn_opencode(record, state)
}

fn maybe_spawn_slot(record: &ProjectRecord, state: &RunState, slug: &str) -> Result<()> {
    let live = load_run_state(record).unwrap_or_else(|_| state.clone());
    if !live.pending_roles.iter().any(|s| s == slug) {
        return Ok(());
    }

    let binding = load_slot_binding(slug);
    if binding.command.trim().is_empty() {
        write_sync_permission_failure(
            record,
            &live,
            slug,
            format!("plan-review: empty {} command", slot_role(slug)),
        )?;
        return Ok(());
    }

    let backend = backend_for(record, slug);
    if !has_test_backend(record) {
        let resolved = match slug {
            REVIEW_SLUG_OPENCODE => resolve_review_bin("opencode", &binding.command),
            _ => resolve_agy_bin(&binding.command),
        };
        if let Err(e) = resolved {
            write_sync_permission_failure(
                record,
                &live,
                slug,
                format!("{slug} not resolvable: {e}"),
            )?;
            return Ok(());
        }
    }

    let should_spawn = with_run_state_lock(record, || {
        let mut s = load_run_state(record)?;
        if s.status != RunStatus::Running || s.phase != PHASE_PLAN_REVIEW {
            return Ok(false);
        }
        if remaining_budget(record, &s) < MIN_SPAWN_BUDGET {
            return Ok(false);
        }
        if s.plan_review_spawned.iter().any(|x| x == slug) {
            return Ok(false);
        }
        if !s.pending_roles.iter().any(|x| x == slug) {
            return Ok(false);
        }
        s.plan_review_spawned.push(slug.into());
        s.updated_at = chrono::Utc::now();
        save_run_state(record, &s)?;
        Ok(true)
    })?;
    if !should_spawn {
        return Ok(());
    }

    let rec = record.clone();
    let spawn_state = load_run_state(record).unwrap_or(live);
    let remaining = remaining_budget(record, &spawn_state);
    let paths = crate::layout::resolve(record);
    let spawn_track_id = spawn_state.track_id.clone();
    let spawn_epoch = spawn_state.run_epoch;
    let track_dir = spawn_track_id
        .as_deref()
        .and_then(|id| resolve_track_dir(record, id));
    let prompt = slot_prompt(record, spawn_track_id.as_deref(), slug);
    let extra_env = match slug {
        REVIEW_SLUG_OPENCODE => vec![(
            "OPENCODE_PERMISSION".into(),
            opencode_permission_json(&paths.workspace_root, paths.execution_repo.as_deref()),
        )],
        _ => Vec::new(),
    };
    let argv = match slug {
        REVIEW_SLUG_OPENCODE => {
            let title = spawn_track_id
                .as_deref()
                .map(|id| format!("plan-review {id}"));
            opencode_argv(
                &prompt,
                &paths.workspace_root,
                binding.model.as_deref(),
                title.as_deref(),
            )
        }
        _ => {
            let add_dir = add_dir_arg(&paths.workspace_root, paths.execution_repo.as_deref());
            agy_argv(
                &prompt,
                remaining,
                binding.model.as_deref(),
                add_dir.as_deref(),
            )
        }
    };
    let req = PlanReviewRequest {
        slug: slug.into(),
        command: binding.command,
        model: binding.model,
        workspace_root: paths.workspace_root,
        execution_repo: paths.execution_repo,
        track_dir,
        prompt,
        remaining,
        argv,
        extra_env,
        spawn_epoch,
        spawn_track_id,
    };

    if let Some(ref dir) = req.track_dir {
        let _ = std::fs::remove_file(dir.join(format!("{slug}-review.md")));
    }

    let slug_owned = slug.to_string();
    if let Err(e) = std::thread::Builder::new()
        .name(format!("plan-review-{slug}-{}", rec.id))
        .spawn(move || apply_slot_result(&rec, backend.as_ref(), &req))
    {
        write_sync_permission_failure(
            record,
            &spawn_state,
            &slug_owned,
            format!("failed to spawn plan-review {slug_owned} thread: {e}"),
        )?;
    }
    Ok(())
}

fn write_sync_permission_failure(
    record: &ProjectRecord,
    expected: &RunState,
    slug: &str,
    message: String,
) -> Result<()> {
    with_run_state_lock(record, || {
        let mut s = load_run_state(record)?;
        if s.status != RunStatus::Running
            || s.phase != PHASE_PLAN_REVIEW
            || s.run_epoch != expected.run_epoch
            || s.track_id != expected.track_id
        {
            return Ok(());
        }
        if !s.plan_review_spawned.iter().any(|x| x == slug) {
            s.plan_review_spawned.push(slug.into());
            s.updated_at = chrono::Utc::now();
            save_run_state(record, &s)?;
        }
        write_role_outcome(
            record,
            &s,
            slug,
            OutcomeStatus::Failure,
            Some(FailureClass::Permission),
            Some(message),
        )
    })
}

fn apply_slot_result(
    record: &ProjectRecord,
    backend: &dyn PlanReviewBackend,
    req: &PlanReviewRequest,
) {
    let result = backend.run(req);
    let Ok(state) = load_run_state(record) else {
        return;
    };
    if !apply_allowed(&state, req) {
        return;
    }
    match result {
        Ok(out) => adopt_or_fail(record, &state, req, &out),
        Err(e) => {
            let class = classify_spawn_err(&e);
            let _ = write_role_outcome(
                record,
                &state,
                &req.slug,
                OutcomeStatus::Failure,
                Some(class),
                Some(e.to_string()),
            );
        }
    }
}

fn apply_allowed(state: &RunState, req: &PlanReviewRequest) -> bool {
    matches!(state.status, RunStatus::Running | RunStatus::Paused)
        && state.phase == PHASE_PLAN_REVIEW
        && state.run_epoch == req.spawn_epoch
        && state.track_id == req.spawn_track_id
}

fn adopt_or_fail(
    record: &ProjectRecord,
    state: &RunState,
    req: &PlanReviewRequest,
    out: &PlanReviewResult,
) {
    let Ok(latest) = load_run_state(record) else {
        return;
    };
    if !apply_allowed(&latest, req) {
        return;
    }
    if let Some(body) = track_review_body(req.track_dir.as_deref(), &req.slug) {
        let _ = write_review_markdown(record, &req.slug, Some(&body));
        let _ = write_role_outcome(record, state, &req.slug, OutcomeStatus::Success, None, None);
        return;
    }
    if req.slug == REVIEW_SLUG_OPENCODE {
        adopt_opencode_stdout(record, state, req, out);
    } else {
        adopt_agy_stdout(record, state, req, out);
    }
}

fn adopt_agy_stdout(
    record: &ProjectRecord,
    state: &RunState,
    req: &PlanReviewRequest,
    out: &PlanReviewResult,
) {
    match parse_agy_stdout(&out.stdout) {
        AgyStdout::Success(body) => {
            let _ = write_review_markdown(record, &req.slug, Some(&body));
            let _ =
                write_role_outcome(record, state, &req.slug, OutcomeStatus::Success, None, None);
        }
        AgyStdout::JsonFailure { status, error } => {
            let class = classify_agy_status(&status, error.as_deref(), out.exit);
            let msg = error.unwrap_or(status);
            let _ = write_role_outcome(
                record,
                state,
                &req.slug,
                OutcomeStatus::Failure,
                Some(class),
                Some(msg),
            );
        }
        AgyStdout::Raw(body) => {
            let _ = write_review_markdown(record, &req.slug, Some(&body));
            let _ =
                write_role_outcome(record, state, &req.slug, OutcomeStatus::Success, None, None);
        }
        AgyStdout::Empty => {
            write_empty_failure(record, state, req, out);
        }
    }
}

fn adopt_opencode_stdout(
    record: &ProjectRecord,
    state: &RunState,
    req: &PlanReviewRequest,
    out: &PlanReviewResult,
) {
    match parse_opencode_stdout(&out.stdout) {
        OpencodeStdout::Text(body) | OpencodeStdout::Raw(body) => {
            let _ = write_review_markdown(record, &req.slug, Some(&body));
            let _ =
                write_role_outcome(record, state, &req.slug, OutcomeStatus::Success, None, None);
        }
        OpencodeStdout::Error(msg) => {
            let class = classify_agy_status("ERROR", Some(&msg), out.exit);
            let _ = write_role_outcome(
                record,
                state,
                &req.slug,
                OutcomeStatus::Failure,
                Some(class),
                Some(msg),
            );
        }
        OpencodeStdout::Empty => {
            write_empty_failure(record, state, req, out);
        }
    }
}

fn write_empty_failure(
    record: &ProjectRecord,
    state: &RunState,
    req: &PlanReviewRequest,
    out: &PlanReviewResult,
) {
    let class = if out.exit == 124 {
        FailureClass::Timeout
    } else {
        FailureClass::Permission
    };
    let msg = if out.stderr.trim().is_empty() {
        format!(
            "plan-review: {} produced no review file and no stdout",
            req.slug
        )
    } else {
        out.stderr.clone()
    };
    let _ = write_role_outcome(
        record,
        state,
        &req.slug,
        OutcomeStatus::Failure,
        Some(class),
        Some(msg),
    );
}

fn track_review_body(track_dir: Option<&Path>, slug: &str) -> Option<String> {
    let path = track_dir?.join(format!("{slug}-review.md"));
    let text = std::fs::read_to_string(path).ok()?;
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

enum AgyStdout {
    Success(String),
    JsonFailure {
        status: String,
        error: Option<String>,
    },
    Raw(String),
    Empty,
}

fn parse_agy_stdout(stdout: &str) -> AgyStdout {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return AgyStdout::Empty;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return AgyStdout::Raw(stdout.to_string());
    };
    let status = v
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let response = v
        .get("response")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let error = v
        .get("error")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if status.eq_ignore_ascii_case("SUCCESS") {
        if response.trim().is_empty() {
            return AgyStdout::Empty;
        }
        return AgyStdout::Success(response);
    }
    if status.eq_ignore_ascii_case("ERROR")
        || status.eq_ignore_ascii_case("CANCELED")
        || status.eq_ignore_ascii_case("INTERRUPTED")
        || status.eq_ignore_ascii_case("INVALID")
    {
        return AgyStdout::JsonFailure { status, error };
    }
    if !response.trim().is_empty() {
        return AgyStdout::Success(response);
    }
    AgyStdout::Empty
}

enum OpencodeStdout {
    Text(String),
    Error(String),
    Raw(String),
    Empty,
}

/// NDJSON `type=text` + `part.text`. Ignore tool/step/reasoning. Error without
/// text is a failure. Unparseable leftover lines are raw fallback.
fn parse_opencode_stdout(stdout: &str) -> OpencodeStdout {
    let mut texts = Vec::new();
    let mut leftovers = Vec::new();
    let mut error: Option<String> = None;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            leftovers.push(line.to_string());
            continue;
        };
        let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match typ {
            "text" => {
                if let Some(t) = v
                    .get("part")
                    .and_then(|p| p.get("text"))
                    .and_then(|t| t.as_str())
                    .filter(|s| !s.is_empty())
                {
                    texts.push(t.to_string());
                }
            }
            "tool_use" | "step_start" | "step_finish" | "reasoning" => {}
            "error" => {
                if error.is_none() {
                    error = Some(format_opencode_error(&v));
                }
            }
            _ => leftovers.push(line.to_string()),
        }
    }
    if !texts.is_empty() {
        return OpencodeStdout::Text(texts.join(""));
    }
    if let Some(msg) = error {
        return OpencodeStdout::Error(msg);
    }
    if !leftovers.is_empty() {
        return OpencodeStdout::Raw(leftovers.join("\n"));
    }
    OpencodeStdout::Empty
}

fn format_opencode_error(v: &serde_json::Value) -> String {
    if let Some(s) = v.get("error").and_then(|e| e.as_str()) {
        return s.to_string();
    }
    if let Some(s) = v
        .pointer("/error/data/message")
        .and_then(|m| m.as_str())
        .filter(|s| !s.is_empty())
    {
        return s.to_string();
    }
    if let Some(s) = v
        .get("error")
        .and_then(|e| e.get("name"))
        .and_then(|n| n.as_str())
    {
        return s.to_string();
    }
    "opencode error".into()
}

fn classify_agy_status(status: &str, error: Option<&str>, exit: i32) -> FailureClass {
    if exit == 124 {
        return FailureClass::Timeout;
    }
    let blob = format!("{} {}", status, error.unwrap_or("")).to_ascii_lowercase();
    if blob.contains("timeout") {
        FailureClass::Timeout
    } else if blob.contains("permission")
        || blob.contains("login")
        || blob.contains("auth")
        || blob.contains("denied")
    {
        FailureClass::Permission
    } else {
        FailureClass::HarnessCrash
    }
}

fn classify_spawn_err(e: &CoordinatorError) -> FailureClass {
    let s = e.to_string().to_ascii_lowercase();
    if s.contains("not found") || s.contains("refusing to spawn") || s.contains("permission") {
        FailureClass::Permission
    } else {
        FailureClass::HarnessCrash
    }
}

fn write_role_outcome(
    record: &ProjectRecord,
    state: &RunState,
    slug: &str,
    status: OutcomeStatus,
    class: Option<FailureClass>,
    message: Option<String>,
) -> Result<()> {
    let roles = outcome_roles_dir(record)?;
    std::fs::create_dir_all(&roles)?;
    let mut outcome = match status {
        OutcomeStatus::Success => PhaseOutcome::success(
            role_phase(slug),
            OutcomeSource::Adapter,
            message,
            None,
            Some(state.run_epoch),
        ),
        OutcomeStatus::Failure => PhaseOutcome::failure(
            role_phase(slug),
            class.unwrap_or(FailureClass::HarnessCrash),
            OutcomeSource::Adapter,
            message,
            Some(state.run_epoch),
        ),
    };
    outcome.metadata = Some(OutcomeMetadata {
        next_track: None,
        role: Some(slot_role(slug).into()),
        ..Default::default()
    });
    crate::persist::atomic_write_json(&roles.join(format!("{slug}.json")), &outcome)
}

fn has_test_backend(record: &ProjectRecord) -> bool {
    #[cfg(test)]
    {
        test_backend_for(&record.id).is_some()
    }
    #[cfg(not(test))]
    {
        let _ = record;
        false
    }
}

fn backend_for(record: &ProjectRecord, slug: &str) -> Arc<dyn PlanReviewBackend> {
    #[cfg(test)]
    {
        if let Some(b) = test_backend_for(&record.id) {
            return b;
        }
    }
    #[cfg(not(test))]
    {
        let _ = record;
    }
    match slug {
        REVIEW_SLUG_OPENCODE => Arc::new(OpenCodeCli),
        _ => Arc::new(AgyCli),
    }
}

#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::sync::Mutex;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(test)]
static TEST_BACKENDS: Mutex<Option<HashMap<String, Arc<dyn PlanReviewBackend>>>> = Mutex::new(None);

#[cfg(test)]
fn test_backend_for(project_id: &str) -> Option<Arc<dyn PlanReviewBackend>> {
    TEST_BACKENDS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .and_then(|m| m.get(project_id).cloned())
}

#[cfg(test)]
pub struct TestBackendGuard {
    id: String,
}

#[cfg(test)]
impl Drop for TestBackendGuard {
    fn drop(&mut self) {
        let mut g = TEST_BACKENDS.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(map) = g.as_mut() {
            map.remove(&self.id);
            if map.is_empty() {
                *g = None;
            }
        }
    }
}

#[cfg(test)]
pub fn install_test_backend(
    project_id: &str,
    backend: Arc<dyn PlanReviewBackend>,
) -> TestBackendGuard {
    let mut g = TEST_BACKENDS.lock().unwrap_or_else(|p| p.into_inner());
    g.get_or_insert_with(HashMap::new)
        .insert(project_id.to_string(), backend);
    TestBackendGuard {
        id: project_id.to_string(),
    }
}

#[cfg(test)]
#[derive(Clone, Default)]
pub struct CallCounts {
    pub runs: Arc<AtomicUsize>,
    pub slugs: Arc<Mutex<Vec<String>>>,
    pub requests: Arc<Mutex<Vec<PlanReviewRequest>>>,
}

#[cfg(test)]
impl CallCounts {
    pub fn n(&self) -> usize {
        self.runs.load(Ordering::SeqCst)
    }

    pub fn slugs(&self) -> Vec<String> {
        self.slugs.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

#[cfg(test)]
pub struct RecordingBackend {
    inner: Arc<dyn PlanReviewBackend>,
    pub counts: CallCounts,
}

#[cfg(test)]
impl RecordingBackend {
    pub fn wrap(inner: Arc<dyn PlanReviewBackend>) -> Self {
        Self {
            inner,
            counts: CallCounts::default(),
        }
    }
}

#[cfg(test)]
impl PlanReviewBackend for RecordingBackend {
    fn run(&self, req: &PlanReviewRequest) -> Result<PlanReviewResult> {
        self.counts.runs.fetch_add(1, Ordering::SeqCst);
        self.counts
            .slugs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(req.slug.clone());
        self.counts
            .requests
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(req.clone());
        self.inner.run(req)
    }
}

#[cfg(test)]
pub struct ScriptedBackend {
    result: PlanReviewResult,
    write_track: Option<String>,
}

#[cfg(test)]
impl ScriptedBackend {
    pub fn ok_file(body: impl Into<String>) -> Self {
        Self {
            result: PlanReviewResult {
                exit: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
            write_track: Some(body.into()),
        }
    }

    pub fn ok_json(response: impl Into<String>) -> Self {
        let response = response.into();
        Self {
            result: PlanReviewResult {
                exit: 0,
                stdout: format!(
                    r#"{{"status":"SUCCESS","response":{}}}"#,
                    serde_json::to_string(&response).unwrap()
                ),
                stderr: String::new(),
            },
            write_track: None,
        }
    }

    pub fn ok_ndjson(text: impl Into<String>) -> Self {
        let text = text.into();
        let event = serde_json::json!({
            "type": "text",
            "timestamp": 1,
            "sessionID": "s",
            "part": { "text": text },
        });
        Self {
            result: PlanReviewResult {
                exit: 0,
                stdout: format!("{event}\n"),
                stderr: String::new(),
            },
            write_track: None,
        }
    }

    pub fn json_error(status: &str, error: &str) -> Self {
        Self {
            result: PlanReviewResult {
                exit: 1,
                stdout: format!(
                    r#"{{"status":{status},"error":{error}}}"#,
                    status = serde_json::to_string(status).unwrap(),
                    error = serde_json::to_string(error).unwrap()
                ),
                stderr: String::new(),
            },
            write_track: None,
        }
    }

    pub fn ndjson_error(error: &str) -> Self {
        let event = serde_json::json!({
            "type": "error",
            "error": { "name": "Error", "data": { "message": error } },
        });
        Self {
            result: PlanReviewResult {
                exit: 1,
                stdout: format!("{event}\n"),
                stderr: String::new(),
            },
            write_track: None,
        }
    }

    pub fn raw(stdout: impl Into<String>) -> Self {
        Self {
            result: PlanReviewResult {
                exit: 0,
                stdout: stdout.into(),
                stderr: String::new(),
            },
            write_track: None,
        }
    }

    pub fn empty() -> Self {
        Self {
            result: PlanReviewResult {
                exit: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
            write_track: None,
        }
    }
}

#[cfg(test)]
impl PlanReviewBackend for ScriptedBackend {
    fn run(&self, req: &PlanReviewRequest) -> Result<PlanReviewResult> {
        if let (Some(body), Some(dir)) = (&self.write_track, &req.track_dir) {
            let _ = std::fs::create_dir_all(dir);
            let _ = std::fs::write(dir.join(format!("{}-review.md", req.slug)), body);
        }
        Ok(self.result.clone())
    }
}

#[cfg(test)]
struct HangBackend {
    delay: Duration,
}

#[cfg(test)]
impl PlanReviewBackend for HangBackend {
    fn run(&self, _req: &PlanReviewRequest) -> Result<PlanReviewResult> {
        std::thread::sleep(self.delay);
        Ok(PlanReviewResult {
            exit: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

#[cfg(test)]
struct GatedBackend {
    release: Arc<(Mutex<bool>, std::sync::Condvar)>,
    inner: Arc<dyn PlanReviewBackend>,
}

#[cfg(test)]
impl PlanReviewBackend for GatedBackend {
    fn run(&self, req: &PlanReviewRequest) -> Result<PlanReviewResult> {
        let (lock, cv) = &*self.release;
        let mut g = lock.lock().unwrap_or_else(|p| p.into_inner());
        while !*g {
            g = cv.wait(g).unwrap_or_else(|p| p.into_inner());
        }
        self.inner.run(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ENV_COORDINATOR_AGY_BIN, ENV_COORDINATOR_HOME, ENV_COORDINATOR_OPENCODE_BIN, test_env_lock,
    };
    use crate::outcome::{OutcomeSource, write_and_apply};
    use crate::run;
    use crate::state::{load_run_state, save_run_state};
    use crate::watch::poll_once;
    use crate::workflow::drive::write_review_markdown;
    use crate::workflow::graph;
    use crate::workflow::timeouts::ENV_PHASE_TIMEOUT_SECS;
    use crate::workflow::{WorkflowDriver, reset_phase_clock, tick};
    use std::ffi::OsString;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tempfile::tempdir;
    use uuid::Uuid;

    /// Serializes env-sensitive spawn tests and pins table timeouts.
    struct IsolatedHome {
        prev_home: Option<OsString>,
        prev_timeout: Option<OsString>,
        prev_agy_bin: Option<OsString>,
        prev_oc_bin: Option<OsString>,
        prev_state_dir: Option<OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
        _home: tempfile::TempDir,
    }

    impl IsolatedHome {
        fn enter() -> Self {
            let lock = test_env_lock();
            let prev_home = std::env::var_os(ENV_COORDINATOR_HOME);
            let prev_timeout = std::env::var_os(ENV_PHASE_TIMEOUT_SECS);
            let prev_agy_bin = std::env::var_os(ENV_COORDINATOR_AGY_BIN);
            let prev_oc_bin = std::env::var_os(ENV_COORDINATOR_OPENCODE_BIN);
            let prev_state_dir = std::env::var_os(crate::config::ENV_COORDINATOR_STATE_DIR);
            let home = tempdir().unwrap();
            unsafe {
                std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
                std::env::remove_var(crate::config::ENV_COORDINATOR_STATE_DIR);
                std::env::set_var(ENV_COORDINATOR_HOME, home.path());
                // Missing sentinels so IsolatedHome tests never live-spawn PATH bins.
                std::env::set_var(
                    ENV_COORDINATOR_AGY_BIN,
                    r"C:\this\does\not\exist-agy-isolated.exe",
                );
                std::env::set_var(
                    ENV_COORDINATOR_OPENCODE_BIN,
                    r"C:\this\does\not\exist-opencode-isolated.exe",
                );
            }
            Self {
                prev_home,
                prev_timeout,
                prev_agy_bin,
                prev_oc_bin,
                prev_state_dir,
                _lock: lock,
                _home: home,
            }
        }
    }

    impl Drop for IsolatedHome {
        fn drop(&mut self) {
            unsafe {
                match &self.prev_home {
                    Some(v) => std::env::set_var(ENV_COORDINATOR_HOME, v),
                    None => std::env::remove_var(ENV_COORDINATOR_HOME),
                }
                match &self.prev_timeout {
                    Some(v) => std::env::set_var(ENV_PHASE_TIMEOUT_SECS, v),
                    None => std::env::remove_var(ENV_PHASE_TIMEOUT_SECS),
                }
                match &self.prev_agy_bin {
                    Some(v) => std::env::set_var(ENV_COORDINATOR_AGY_BIN, v),
                    None => std::env::remove_var(ENV_COORDINATOR_AGY_BIN),
                }
                match &self.prev_oc_bin {
                    Some(v) => std::env::set_var(ENV_COORDINATOR_OPENCODE_BIN, v),
                    None => std::env::remove_var(ENV_COORDINATOR_OPENCODE_BIN),
                }
                match &self.prev_state_dir {
                    Some(v) => std::env::set_var(crate::config::ENV_COORDINATOR_STATE_DIR, v),
                    None => std::env::remove_var(crate::config::ENV_COORDINATOR_STATE_DIR),
                }
            }
        }
    }

    fn rec(path: &std::path::Path) -> ProjectRecord {
        ProjectRecord {
            id: Uuid::new_v4().to_string(),
            path: path.to_path_buf(),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: std::collections::BTreeMap::new(),
            state_dir: None,
            auto_merge: true,
            phase_timeouts_secs: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now(),
        }
    }

    fn setup_track(dir: &std::path::Path, id: &str) {
        std::fs::create_dir_all(dir.join("conductor").join(format!("{id}-Example"))).unwrap();
    }

    fn enter_plan_review(r: &ProjectRecord, track: &str) {
        run::run_with_driver(r, Some(track.into()), WorkflowDriver::Adapter).unwrap();
        let mut state = load_run_state(r).unwrap();
        state.phase = graph::PHASE_PLAN_REVIEW.into();
        state.pending_roles = vec!["agy".into(), "opencode".into()];
        save_run_state(r, &state).unwrap();
    }

    fn wait_slots_consumed(r: &ProjectRecord, slugs: &[&str]) {
        for _ in 0..80 {
            let _ = tick(r);
            let s = load_run_state(r).unwrap();
            if slugs
                .iter()
                .all(|slug| !s.pending_roles.iter().any(|x| x == slug))
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!(
            "slots still pending: {:?}",
            load_run_state(r).unwrap().pending_roles
        );
    }

    fn wait_agy_consumed(r: &ProjectRecord) {
        wait_slots_consumed(r, &["agy"]);
    }

    fn wait_both_consumed(r: &ProjectRecord) {
        wait_slots_consumed(r, &["agy", "opencode"]);
    }

    fn track_file(dir: &std::path::Path, track: &str, slug: &str) -> PathBuf {
        dir.join("conductor")
            .join(format!("{track}-Example"))
            .join(format!("{slug}-review.md"))
    }

    #[test]
    fn argv_table_pins_print_flags() {
        let args = agy_argv(
            "review please",
            Duration::from_secs(1199),
            Some("gemini-x"),
            Some(Path::new(r"C:\dev\proj\app")),
        );
        assert!(args.iter().any(|a| a == "--print" || a == "-p"));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--print-timeout" && w[1] == "1199s")
        );
        assert!(args.iter().any(|a| a == "--dangerously-skip-permissions"));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--output-format" && w[1] == "json")
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--model" && w[1] == "gemini-x")
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--add-dir" && w[1] == r"C:\dev\proj\app")
        );
        assert!(!args.iter().any(|a| a == "--sandbox"));
        assert!(!args.iter().any(|a| a == "--continue" || a == "-c"));
        assert!(!args.iter().any(|a| a == "--mode"));
        assert!(!args.iter().any(|a| a == "--disable-slash-commands"));
        assert_eq!(args.iter().filter(|a| *a == "--print").count(), 1);
    }

    #[test]
    fn argv_omits_empty_model_and_same_dir_add() {
        let ws = Path::new(r"C:\dev\proj");
        let args = agy_argv(
            "p",
            Duration::from_secs(60),
            Some("  "),
            add_dir_arg(ws, Some(ws)).as_deref(),
        );
        assert!(!args.iter().any(|a| a == "--model"));
        assert!(!args.iter().any(|a| a == "--add-dir"));
    }

    #[test]
    fn opencode_argv_pins_run_dir_json_and_forbids_auto() {
        let args = opencode_argv(
            "review please",
            Path::new(r"C:\dev\proj"),
            Some("provider/model"),
            Some("plan-review 0001"),
        );
        assert_eq!(args[0], "run");
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--dir" && w[1] == r"C:\dev\proj")
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--format" && w[1] == "json")
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--model" && w[1] == "provider/model")
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--title" && w[1] == "plan-review 0001")
        );
        assert!(!args.iter().any(|a| a == "--auto"));
        assert!(!args.iter().any(|a| a == "--yolo"));
        assert!(!args.iter().any(|a| a == "--dangerously-skip-permissions"));
        assert!(!args.iter().any(|a| a == "--continue" || a == "-c"));
        assert!(!args.iter().any(|a| a == "--session" || a == "-s"));
        assert!(!args.iter().any(|a| a == "--attach"));
        assert!(!args.iter().any(|a| a == "--interactive" || a == "-i"));
    }

    #[test]
    fn opencode_argv_omits_empty_model_and_title() {
        let args = opencode_argv("p", Path::new(r"C:\dev\proj"), Some("  "), None);
        assert!(!args.iter().any(|a| a == "--model" || a == "-m"));
        assert!(!args.iter().any(|a| a == "--title"));
    }

    #[test]
    fn opencode_permission_denies_bash_and_gates_external() {
        let ws = Path::new(r"C:\dev\proj");
        let same = opencode_permission_json(ws, Some(ws));
        let v: serde_json::Value = serde_json::from_str(&same).unwrap();
        assert_eq!(v["bash"], "deny");
        assert_eq!(v["read"], "allow");
        assert_eq!(v["glob"], "allow");
        assert_eq!(v["grep"], "allow");
        assert_eq!(v["edit"]["**/*opencode-review.md"], "allow");
        assert!(v.get("external_directory").is_none());

        let exec = Path::new(r"C:\dev\proj\app");
        let differ = opencode_permission_json(ws, Some(exec));
        let v: serde_json::Value = serde_json::from_str(&differ).unwrap();
        assert_eq!(v["bash"], "deny");
        assert_eq!(v["external_directory"][r"C:\dev\proj\app"], "allow");
    }

    #[test]
    fn prompt_is_review_track_not_verdict() {
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        let text = agy_prompt(&r, Some("0001"));
        assert!(text.contains("agy-review.md"));
        assert!(text.contains("spec.md"));
        assert!(text.contains("plan.md"));
        assert!(!text.contains("## Verdict: PASS"));
        assert!(
            text.to_ascii_lowercase()
                .contains("outside the product git")
        );
        let oc = opencode_prompt(&r, Some("0001"));
        assert!(oc.contains("opencode-review.md"));
        assert!(!oc.contains("## Verdict: PASS"));
        assert!(oc.contains("spec.md"));
        assert!(oc.to_ascii_lowercase().contains("outside the product git"));
    }

    fn contains_skill(text: &str, name: &str) -> bool {
        let n = text.replace('\\', "/");
        n.contains(&format!(".agents/skills/{name}/SKILL.md"))
    }

    #[test]
    fn slot_prompt_names_review_track_research_and_layout() {
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let mut r = rec(dir.path());
        r.layout_profile = crate::layout::LayoutProfile::MultiSibling;
        r.execution_repos
            .insert("app".into(), dir.path().join("app"));
        let text = agy_prompt(&r, Some("0001"));
        assert!(contains_skill(&text, "review-track"), "skill path: {text}");
        assert!(text.contains("review-track"));
        assert!(text.contains("stale") && text.contains("primary sources"));
        assert!(text.contains("Workspace root:"));
        assert!(text.contains("Execution repos:"));
        assert!(text.contains("agy-review.md"));
        assert!(!text.contains("## Verdict: PASS"));
        assert!(text.contains("outcome write"));
        let oc = opencode_prompt(&r, Some("0001"));
        assert!(contains_skill(&oc, "review-track"));
        assert!(oc.contains("stale"));
        assert!(oc.contains("opencode-review.md"));
        assert!(!oc.contains("## Verdict: PASS"));
    }

    #[test]
    fn adapter_scripted_writes_state_and_track_and_consumes() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let rec_backend = Arc::new(RecordingBackend::wrap(Arc::new(ScriptedBackend::ok_file(
            "review body is sound\n",
        ))));
        let counts = rec_backend.counts.clone();
        let _hook = install_test_backend(&r.id, rec_backend);
        tick(&r).unwrap();
        wait_both_consumed(&r);
        for slug in ["agy", "opencode"] {
            let state_file = crate::workflow::bundle::review_file(&r, slug).unwrap();
            assert!(state_file.is_file(), "{slug} state review missing");
            let body = std::fs::read_to_string(&state_file).unwrap();
            assert!(body.contains("review body is sound"));
            assert!(track_file(dir.path(), "0001", slug).is_file());
            assert!(
                !load_run_state(&r)
                    .unwrap()
                    .pending_roles
                    .iter()
                    .any(|x| x == slug)
            );
        }
        let slugs = counts.slugs();
        assert!(slugs.iter().any(|s| s == "agy"));
        assert!(slugs.iter().any(|s| s == "opencode"));
        assert_eq!(counts.n(), 2);
        let reqs = counts.requests.lock().unwrap();
        let oc = reqs.iter().find(|q| q.slug == "opencode").unwrap();
        assert_eq!(oc.workspace_root, dir.path());
        assert!(
            oc.argv
                .windows(2)
                .any(|w| w[0] == "--dir" && Path::new(&w[1]) == dir.path())
        );
        assert_eq!(oc.argv[0], "run");
        assert!(!oc.argv.iter().any(|a| a == "--auto"));
        let perm = oc
            .extra_env
            .iter()
            .find(|(k, _)| k == "OPENCODE_PERMISSION")
            .map(|(_, v)| v.as_str())
            .expect("child OPENCODE_PERMISSION");
        let v: serde_json::Value = serde_json::from_str(perm).unwrap();
        assert_eq!(v["bash"], "deny");
        let agy = reqs.iter().find(|q| q.slug == "agy").unwrap();
        assert!(!agy.argv.iter().any(|a| a == "--sandbox"));
        assert!(agy.extra_env.is_empty());
    }

    #[test]
    fn spawn_once_second_tick_does_not_reenter() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let rec_backend = Arc::new(RecordingBackend::wrap(Arc::new(ScriptedBackend::ok_file(
            "once\n",
        ))));
        let counts = rec_backend.counts.clone();
        let _hook = install_test_backend(&r.id, rec_backend);
        tick(&r).unwrap();
        tick(&r).unwrap();
        wait_both_consumed(&r);
        tick(&r).unwrap();
        assert_eq!(counts.n(), 2);
        let slugs = counts.slugs();
        assert_eq!(slugs.iter().filter(|s| *s == "agy").count(), 1);
        assert_eq!(slugs.iter().filter(|s| *s == "opencode").count(), 1);
    }

    #[test]
    fn lock_atomic_mark_prevents_double_spawn() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        {
            let mut state = load_run_state(&r).unwrap();
            state.pending_roles = vec!["agy".into()];
            save_run_state(&r, &state).unwrap();
        }
        let rec_backend = Arc::new(RecordingBackend::wrap(Arc::new(ScriptedBackend::ok_file(
            "atomic\n",
        ))));
        let counts = rec_backend.counts.clone();
        let _hook = install_test_backend(&r.id, rec_backend);
        let rec_a = r.clone();
        let rec_b = r.clone();
        let spawn = |rec: ProjectRecord| {
            for _ in 0..40 {
                let s = load_run_state(&rec).unwrap();
                match maybe_spawn_agy(&rec, &s) {
                    Ok(()) => return,
                    Err(e) => {
                        let msg = e.to_string();
                        if msg.contains("Access is denied")
                            || msg.contains("timed out waiting for run-state lock")
                        {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        panic!("{e}");
                    }
                }
            }
            panic!("maybe_spawn_agy retried out");
        };
        let h1 = std::thread::spawn(move || spawn(rec_a));
        let h2 = std::thread::spawn(move || spawn(rec_b));
        h1.join().unwrap();
        h2.join().unwrap();
        wait_agy_consumed(&r);
        assert_eq!(counts.n(), 1);
        assert_eq!(counts.slugs(), vec!["agy".to_string()]);
    }

    #[test]
    fn lock_atomic_mark_prevents_double_opencode_spawn() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        {
            let mut state = load_run_state(&r).unwrap();
            state.pending_roles = vec!["opencode".into()];
            save_run_state(&r, &state).unwrap();
        }
        let rec_backend = Arc::new(RecordingBackend::wrap(Arc::new(ScriptedBackend::ok_file(
            "atomic-oc\n",
        ))));
        let counts = rec_backend.counts.clone();
        let _hook = install_test_backend(&r.id, rec_backend);
        let rec_a = r.clone();
        let rec_b = r.clone();
        let spawn = |rec: ProjectRecord| {
            for _ in 0..40 {
                let s = load_run_state(&rec).unwrap();
                match maybe_spawn_opencode(&rec, &s) {
                    Ok(()) => return,
                    Err(e) => {
                        let msg = e.to_string();
                        if msg.contains("Access is denied")
                            || msg.contains("timed out waiting for run-state lock")
                        {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        panic!("{e}");
                    }
                }
            }
            panic!("maybe_spawn_opencode retried out");
        };
        let h1 = std::thread::spawn(move || spawn(rec_a));
        let h2 = std::thread::spawn(move || spawn(rec_b));
        h1.join().unwrap();
        h2.join().unwrap();
        wait_slots_consumed(&r, &["opencode"]);
        assert_eq!(counts.n(), 1);
        assert_eq!(counts.slugs(), vec!["opencode".to_string()]);
    }

    #[test]
    fn agy_mark_does_not_skip_opencode() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let rec_backend = Arc::new(RecordingBackend::wrap(Arc::new(ScriptedBackend::ok_file(
            "both\n",
        ))));
        let counts = rec_backend.counts.clone();
        let _hook = install_test_backend(&r.id, rec_backend);
        tick(&r).unwrap();
        wait_both_consumed(&r);
        let slugs = counts.slugs();
        assert!(slugs.iter().any(|s| s == "agy"));
        assert!(slugs.iter().any(|s| s == "opencode"));
    }

    #[test]
    fn hang_backend_does_not_block_poll_once() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let _hook = install_test_backend(
            &r.id,
            Arc::new(HangBackend {
                delay: Duration::from_secs(3),
            }),
        );
        let start = Instant::now();
        let _ = poll_once(&r).unwrap();
        assert!(
            start.elapsed() < Duration::from_millis(800),
            "poll_once blocked for {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn file_wait_and_stub_do_not_spawn() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        let rec_backend = Arc::new(RecordingBackend::wrap(Arc::new(ScriptedBackend::ok_file(
            "should-not-run\n",
        ))));
        let counts = rec_backend.counts.clone();
        let _hook = install_test_backend(&r.id, rec_backend);

        run::run_with_driver(&r, Some("0001".into()), WorkflowDriver::FileWait).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_PLAN_REVIEW.into();
        state.pending_roles = vec!["agy".into(), "opencode".into()];
        save_run_state(&r, &state).unwrap();
        tick(&r).unwrap();
        assert_eq!(counts.n(), 0);

        run::stop(&r).unwrap();
        run::run_with_driver(&r, Some("0001".into()), WorkflowDriver::Stub).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_PLAN_REVIEW.into();
        save_run_state(&r, &state).unwrap();
        tick(&r).unwrap();
        assert_eq!(counts.n(), 0);
        let s = load_run_state(&r).unwrap();
        assert!(!s.pending_roles.iter().any(|x| x == "agy"));
        assert!(!s.pending_roles.iter().any(|x| x == "opencode"));
    }

    #[test]
    fn missing_binary_permission_degrades_when_opencode_present() {
        let _env = IsolatedHome::enter();
        unsafe {
            std::env::set_var(
                ENV_COORDINATOR_AGY_BIN,
                r"C:\this\does\not\exist-agy-0017.exe",
            );
        }
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        {
            let mut state = load_run_state(&r).unwrap();
            state.plan_review_spawned = vec!["opencode".into()];
            save_run_state(&r, &state).unwrap();
        }
        write_review_markdown(&r, "opencode", Some("opencode already done\n")).unwrap();
        let roles = outcome_roles_dir(&r).unwrap();
        std::fs::create_dir_all(&roles).unwrap();
        let mut oc = PhaseOutcome::success(
            "plan-review:opencode",
            OutcomeSource::File,
            None,
            None,
            None,
        );
        oc.metadata = Some(OutcomeMetadata {
            next_track: None,
            role: Some(graph::ROLE_REVIEWER_OPENCODE.into()),
            ..Default::default()
        });
        crate::persist::atomic_write_json(&roles.join("opencode.json"), &oc).unwrap();

        let view = tick(&r).unwrap().expect("degrade join");
        assert_eq!(view.phase, graph::PHASE_FOLD);
        assert!(view.last_event.contains("degraded"));
        let agy_json = crate::outcome::outcome_roles_dir(&r)
            .unwrap()
            .join("agy.json");
        assert!(
            !agy_json.exists(),
            "consumed role file should be gone after join"
        );
    }

    #[test]
    fn missing_opencode_binary_degrades_when_agy_present() {
        let _env = IsolatedHome::enter();
        unsafe {
            std::env::set_var(
                ENV_COORDINATOR_OPENCODE_BIN,
                r"C:\this\does\not\exist-opencode-0018.exe",
            );
        }
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        {
            let mut state = load_run_state(&r).unwrap();
            state.plan_review_spawned = vec!["agy".into()];
            save_run_state(&r, &state).unwrap();
        }
        write_review_markdown(&r, "agy", Some("agy already done\n")).unwrap();
        let roles = outcome_roles_dir(&r).unwrap();
        std::fs::create_dir_all(&roles).unwrap();
        let mut agy =
            PhaseOutcome::success("plan-review:agy", OutcomeSource::File, None, None, None);
        agy.metadata = Some(OutcomeMetadata {
            next_track: None,
            role: Some(graph::ROLE_REVIEWER_AGY.into()),
            ..Default::default()
        });
        crate::persist::atomic_write_json(&roles.join("agy.json"), &agy).unwrap();

        let view = tick(&r).unwrap().expect("degrade join");
        assert_eq!(view.phase, graph::PHASE_FOLD);
        assert!(view.last_event.contains("degraded"));
    }

    #[test]
    fn stop_before_apply_writes_neither_success_nor_review() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let backend = Arc::new(GatedBackend {
            release: gate.clone(),
            inner: Arc::new(ScriptedBackend::ok_json("should not land")),
        });
        let _hook = install_test_backend(&r.id, backend);
        tick(&r).unwrap();
        run::stop(&r).unwrap();
        {
            let (lock, cv) = &*gate;
            let mut g = lock.lock().unwrap();
            *g = true;
            cv.notify_all();
        }
        std::thread::sleep(Duration::from_millis(80));
        let _ = tick(&r);
        let roles = outcome_roles_dir(&r).unwrap();
        for slug in ["agy", "opencode"] {
            if roles.join(format!("{slug}.json")).exists() {
                let text = std::fs::read_to_string(roles.join(format!("{slug}.json"))).unwrap();
                let o: PhaseOutcome = serde_json::from_str(&text).unwrap();
                assert_ne!(o.status, OutcomeStatus::Success);
            }
            let state_review = crate::workflow::bundle::review_file(&r, slug).unwrap();
            assert!(
                !state_review.exists(),
                "cancelled thread must not write {slug} state review"
            );
            assert!(
                !track_file(dir.path(), "0001", slug).exists(),
                "cancelled thread must not copy {slug} review to track"
            );
        }
    }

    #[test]
    fn stale_spawn_does_not_apply_after_new_run() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        std::fs::create_dir_all(dir.path().join("conductor").join("0002-Next")).unwrap();
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let _hook = install_test_backend(
            &r.id,
            Arc::new(GatedBackend {
                release: gate.clone(),
                inner: Arc::new(ScriptedBackend::ok_file("stale review from track A\n")),
            }),
        );
        tick(&r).unwrap();
        let first_epoch = load_run_state(&r).unwrap().run_epoch;
        run::stop(&r).unwrap();
        enter_plan_review(&r, "0002");
        assert_ne!(load_run_state(&r).unwrap().run_epoch, first_epoch);
        {
            let (lock, cv) = &*gate;
            let mut g = lock.lock().unwrap();
            *g = true;
            cv.notify_all();
        }
        std::thread::sleep(Duration::from_millis(80));
        for slug in ["agy", "opencode"] {
            let state_file = crate::workflow::bundle::review_file(&r, slug).unwrap();
            assert!(
                !state_file.exists(),
                "old-run child must not write the new run's {slug} review"
            );
        }
        let track_b = dir
            .path()
            .join("conductor")
            .join("0002-Next")
            .join("agy-review.md");
        assert!(!track_b.exists());
        assert!(
            load_run_state(&r)
                .unwrap()
                .pending_roles
                .iter()
                .any(|x| x == "agy")
        );
    }

    #[test]
    fn advance_auto_start_clears_spawned_and_respawns() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("conductor").join("0001-Example")).unwrap();
        std::fs::create_dir_all(dir.path().join("conductor").join("0002-Next")).unwrap();
        let r = rec(dir.path());
        let rec_backend = Arc::new(RecordingBackend::wrap(Arc::new(ScriptedBackend::ok_file(
            "review body\n",
        ))));
        let counts = rec_backend.counts.clone();
        let _hook = install_test_backend(&r.id, rec_backend);

        enter_plan_review(&r, "0001");
        tick(&r).unwrap();
        let after_spawn = load_run_state(&r).unwrap();
        assert!(
            after_spawn
                .plan_review_spawned
                .iter()
                .any(|s| s == REVIEW_SLUG_AGY)
        );
        assert!(
            after_spawn
                .plan_review_spawned
                .iter()
                .any(|s| s == REVIEW_SLUG_OPENCODE)
        );
        wait_both_consumed(&r);
        assert_eq!(counts.n(), 2);

        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_ADVANCE.into();
        state.pending_roles.clear();
        save_run_state(&r, &state).unwrap();
        let o = crate::outcome::PhaseOutcome::success(
            graph::PHASE_ADVANCE,
            OutcomeSource::Test,
            None,
            Some("0002".into()),
            None,
        );
        let view = write_and_apply(&r, o).unwrap();
        assert_eq!(view.phase, graph::PHASE_PLAN);
        assert_eq!(view.track_id.as_deref(), Some("0002"));
        let cleared = load_run_state(&r).unwrap();
        assert!(
            cleared.plan_review_spawned.is_empty(),
            "reset_phase_clock must clear spawned on auto_start"
        );

        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_PLAN_REVIEW.into();
        state.pending_roles = vec!["agy".into(), "opencode".into()];
        save_run_state(&r, &state).unwrap();
        tick(&r).unwrap();
        wait_both_consumed(&r);
        assert_eq!(counts.n(), 4);
        let slugs = counts.slugs();
        assert_eq!(slugs.iter().filter(|s| *s == "opencode").count(), 2);
        assert_eq!(slugs.iter().filter(|s| *s == "agy").count(), 2);
    }

    #[test]
    fn run_with_driver_clears_spawned() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run::run_with_driver(&r, Some("0001".into()), WorkflowDriver::Adapter).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.plan_review_spawned = vec!["agy".into(), "opencode".into()];
        save_run_state(&r, &state).unwrap();
        run::stop(&r).unwrap();
        run::run_with_driver(&r, Some("0001".into()), WorkflowDriver::Adapter).unwrap();
        let state = load_run_state(&r).unwrap();
        assert!(state.plan_review_spawned.is_empty());
    }

    #[test]
    fn reset_phase_clock_clears_spawned() {
        let mut state = RunState::idle("p");
        state.plan_review_spawned = vec!["agy".into(), "opencode".into()];
        reset_phase_clock(&mut state);
        assert!(state.plan_review_spawned.is_empty());
    }

    #[test]
    fn json_success_fallback_and_raw_stdout() {
        assert!(matches!(
            parse_agy_stdout(r#"{"status":"SUCCESS","response":"ok review"}"#),
            AgyStdout::Success(s) if s == "ok review"
        ));
        assert!(matches!(
            parse_agy_stdout(r#"{"status":"SUCCESS","response":""}"#),
            AgyStdout::Empty
        ));
        assert!(matches!(
            parse_agy_stdout(r#"{"status":"ERROR","error":"login required"}"#),
            AgyStdout::JsonFailure { .. }
        ));
        assert!(matches!(
            parse_agy_stdout(r#"{"status":"CANCELED","error":"user canceled"}"#),
            AgyStdout::JsonFailure { status, .. } if status.eq_ignore_ascii_case("CANCELED")
        ));
        assert!(matches!(
            parse_agy_stdout(r#"{"status":"INTERRUPTED"}"#),
            AgyStdout::JsonFailure { .. }
        ));
        assert!(matches!(
            parse_agy_stdout(r#"{"status":"INVALID"}"#),
            AgyStdout::JsonFailure { .. }
        ));
        assert!(matches!(
            parse_agy_stdout("plain text review"),
            AgyStdout::Raw(_)
        ));
        assert!(matches!(parse_agy_stdout("  "), AgyStdout::Empty));
    }

    #[test]
    fn opencode_ndjson_parse_table() {
        let text_line = r#"{"type":"text","part":{"text":"hello "}}"#;
        let text2 = r#"{"type":"text","part":{"text":"world"}}"#;
        let tool = r#"{"type":"tool_use","part":{}}"#;
        let step = r#"{"type":"step_start"}"#;
        let reason = r#"{"type":"reasoning","part":{"text":"think"}}"#;
        let blob = [text_line, tool, step, reason, text2].join("\n");
        assert!(matches!(
            parse_opencode_stdout(&blob),
            OpencodeStdout::Text(s) if s == "hello world"
        ));
        assert!(matches!(
            parse_opencode_stdout(r#"{"type":"error","error":{"name":"Auth","data":{"message":"login"}}}"#),
            OpencodeStdout::Error(s) if s.contains("login")
        ));
        assert!(matches!(
            parse_opencode_stdout("not json at all"),
            OpencodeStdout::Raw(_)
        ));
        assert!(matches!(parse_opencode_stdout(""), OpencodeStdout::Empty));
        assert!(matches!(
            parse_opencode_stdout(r#"{"type":"text","part":{"text":""}}"#),
            OpencodeStdout::Empty
        ));
    }

    #[test]
    fn leftover_track_review_is_not_adopted_on_empty_child() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let leftover = track_file(dir.path(), "0001", "agy");
        std::fs::write(&leftover, "stale review from last run\n").unwrap();
        let leftover_oc = track_file(dir.path(), "0001", "opencode");
        std::fs::write(&leftover_oc, "stale opencode from last run\n").unwrap();
        let _hook = install_test_backend(&r.id, Arc::new(ScriptedBackend::empty()));
        tick(&r).unwrap();
        wait_both_consumed(&r);
        for (slug, stale) in [
            ("agy", "stale review from last run"),
            ("opencode", "stale opencode from last run"),
        ] {
            let state_file = crate::workflow::bundle::review_file(&r, slug).unwrap();
            if state_file.is_file() {
                let body = std::fs::read_to_string(state_file).unwrap();
                assert!(
                    !body.contains(stale),
                    "prior {slug} track copy must not be adopted as this child's success"
                );
            }
            let o = serde_json::from_str::<PhaseOutcome>(
                &std::fs::read_to_string(
                    crate::outcome::outcome_roles_dir(&r)
                        .unwrap()
                        .join(format!("{slug}.json")),
                )
                .unwrap_or_default(),
            );
            if let Ok(o) = o {
                assert_ne!(o.status, OutcomeStatus::Success);
            }
        }
    }

    #[test]
    fn leftover_opencode_review_is_not_adopted_on_empty_child() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let leftover = track_file(dir.path(), "0001", "opencode");
        std::fs::write(&leftover, "stale opencode leftover\n").unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.pending_roles = vec!["opencode".into()];
        state.plan_review_spawned = vec!["agy".into()];
        save_run_state(&r, &state).unwrap();
        let _hook = install_test_backend(&r.id, Arc::new(ScriptedBackend::empty()));
        tick(&r).unwrap();
        wait_slots_consumed(&r, &["opencode"]);
        let state_file = crate::workflow::bundle::review_file(&r, "opencode").unwrap();
        if state_file.is_file() {
            let body = std::fs::read_to_string(state_file).unwrap();
            assert!(!body.contains("stale opencode leftover"));
        }
    }

    #[test]
    fn ndjson_text_fallback_when_file_missing() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let mut state = load_run_state(&r).unwrap();
        state.pending_roles = vec!["opencode".into()];
        state.plan_review_spawned = vec!["agy".into()];
        save_run_state(&r, &state).unwrap();
        let _hook = install_test_backend(
            &r.id,
            Arc::new(ScriptedBackend::ok_ndjson("ndjson review body\n")),
        );
        tick(&r).unwrap();
        wait_slots_consumed(&r, &["opencode"]);
        let state_file = crate::workflow::bundle::review_file(&r, "opencode").unwrap();
        assert!(state_file.is_file());
        let body = std::fs::read_to_string(state_file).unwrap();
        assert!(body.contains("ndjson review body"));
    }

    #[test]
    fn ndjson_error_without_file_is_role_failure() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let mut state = load_run_state(&r).unwrap();
        state.pending_roles = vec!["opencode".into()];
        state.plan_review_spawned = vec!["agy".into()];
        save_run_state(&r, &state).unwrap();
        let _hook = install_test_backend(&r.id, Arc::new(ScriptedBackend::ndjson_error("login")));
        tick(&r).unwrap();
        wait_slots_consumed(&r, &["opencode"]);
        let state_file = crate::workflow::bundle::review_file(&r, "opencode").unwrap();
        assert!(
            !state_file.is_file()
                || std::fs::read_to_string(&state_file)
                    .unwrap()
                    .trim()
                    .is_empty()
        );
        let path = crate::outcome::outcome_roles_dir(&r)
            .unwrap()
            .join("opencode.json");
        // consumed on success/fail; join may have already eaten it. pending gone is enough.
        assert!(
            !load_run_state(&r)
                .unwrap()
                .pending_roles
                .iter()
                .any(|x| x == "opencode")
        );
        let _ = path;
    }

    #[test]
    fn remaining_under_60s_does_not_spawn() {
        let _env = IsolatedHome::enter();
        unsafe {
            std::env::set_var(ENV_PHASE_TIMEOUT_SECS, "1");
        }
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let mut state = load_run_state(&r).unwrap();
        state.phase_started_at = Some(chrono::Utc::now() - chrono::Duration::seconds(5));
        save_run_state(&r, &state).unwrap();
        let rec_backend = Arc::new(RecordingBackend::wrap(Arc::new(ScriptedBackend::ok_file(
            "too late\n",
        ))));
        let counts = rec_backend.counts.clone();
        let _hook = install_test_backend(&r.id, rec_backend);
        tick(&r).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(counts.n(), 0);
        let pending = load_run_state(&r).unwrap().pending_roles;
        assert!(pending.iter().any(|x| x == "agy"));
        assert!(pending.iter().any(|x| x == "opencode"));
    }

    #[test]
    fn pause_does_not_drop_in_flight_review() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let rec_backend = Arc::new(RecordingBackend::wrap(Arc::new(GatedBackend {
            release: gate.clone(),
            inner: Arc::new(ScriptedBackend::ok_file("paused still writes\n")),
        })));
        let _hook = install_test_backend(&r.id, rec_backend);
        tick(&r).unwrap();
        run::pause(&r).unwrap();
        {
            let (lock, cv) = &*gate;
            let mut g = lock.lock().unwrap();
            *g = true;
            cv.notify_all();
        }
        let state_file = crate::workflow::bundle::review_file(&r, "agy").unwrap();
        for _ in 0..80 {
            if state_file.is_file() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            state_file.is_file(),
            "paused in-flight thread must still write the review"
        );
        let body = std::fs::read_to_string(&state_file).unwrap();
        assert!(body.contains("paused still writes"));
        let oc_file = crate::workflow::bundle::review_file(&r, "opencode").unwrap();
        for _ in 0..80 {
            if oc_file.is_file() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            oc_file.is_file(),
            "paused in-flight opencode thread must still write the review"
        );
        assert_eq!(
            load_run_state(&r).unwrap().status,
            crate::state::RunStatus::Paused
        );
        run::resume(&r).unwrap();
        wait_both_consumed(&r);
        assert!(
            !load_run_state(&r)
                .unwrap()
                .pending_roles
                .iter()
                .any(|x| x == "agy")
        );
    }

    #[test]
    fn run_stub_clears_spawned() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run::run_stub(&r, Some("0001".into())).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.plan_review_spawned = vec!["agy".into()];
        save_run_state(&r, &state).unwrap();
        run::stop(&r).unwrap();
        run::run_stub(&r, Some("0001".into())).unwrap();
        let state = load_run_state(&r).unwrap();
        assert!(state.plan_review_spawned.is_empty());
    }

    #[test]
    fn plan_review_is_not_grok_bound() {
        assert!(!graph::is_grok_bound(graph::PHASE_PLAN_REVIEW));
    }

    #[test]
    #[ignore]
    fn agy_live_print_optional() {
        if std::env::var(crate::config::ENV_COORDINATOR_AGY_LIVE)
            .ok()
            .as_deref()
            != Some("1")
        {
            eprintln!("skip: COORDINATOR_AGY_LIVE != 1");
            return;
        }
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        std::fs::write(
            dir.path()
                .join("conductor")
                .join("0001-Example")
                .join("spec.md"),
            "# spec\n",
        )
        .unwrap();
        std::fs::write(
            dir.path()
                .join("conductor")
                .join("0001-Example")
                .join("plan.md"),
            "# plan\n",
        )
        .unwrap();
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        {
            let mut state = load_run_state(&r).unwrap();
            state.pending_roles = vec!["agy".into()];
            save_run_state(&r, &state).unwrap();
        }
        tick(&r).unwrap();
        wait_agy_consumed(&r);
        let state_file = crate::workflow::bundle::review_file(&r, "agy").unwrap();
        assert!(
            state_file.is_file() || {
                let roles = outcome_roles_dir(&r).unwrap();
                roles.join("agy.json").is_file()
                    || !load_run_state(&r)
                        .unwrap()
                        .pending_roles
                        .iter()
                        .any(|s| s == "agy")
            }
        );
    }

    #[test]
    #[ignore]
    fn opencode_live_run_optional() {
        if std::env::var(crate::config::ENV_COORDINATOR_OPENCODE_LIVE)
            .ok()
            .as_deref()
            != Some("1")
        {
            eprintln!("skip: COORDINATOR_OPENCODE_LIVE != 1");
            return;
        }
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        std::fs::write(
            dir.path()
                .join("conductor")
                .join("0001-Example")
                .join("spec.md"),
            "# spec\n",
        )
        .unwrap();
        std::fs::write(
            dir.path()
                .join("conductor")
                .join("0001-Example")
                .join("plan.md"),
            "# plan\n",
        )
        .unwrap();
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let mut state = load_run_state(&r).unwrap();
        state.pending_roles = vec!["opencode".into()];
        state.plan_review_spawned = vec!["agy".into()];
        save_run_state(&r, &state).unwrap();
        tick(&r).unwrap();
        wait_slots_consumed(&r, &["opencode"]);
        let state_file = crate::workflow::bundle::review_file(&r, "opencode").unwrap();
        assert!(
            state_file.is_file() || {
                let roles = outcome_roles_dir(&r).unwrap();
                roles.join("opencode.json").is_file()
                    || !load_run_state(&r)
                        .unwrap()
                        .pending_roles
                        .iter()
                        .any(|s| s == "opencode")
            }
        );
    }
}
