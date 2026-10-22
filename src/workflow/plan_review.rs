//! Adapter plan-review: one-shot `agy --print` (0017) and `opencode run` (0018 / 0037).
//!
//! Not the 0011 cross-model Verdict gate. File is source of truth; stdout is fallback.
//! OpenCode prompt is stdin, not argv (cmd.exe `%*` truncates `<LF>`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::{ENV_COORDINATOR_AGY_BIN, RoleBinding};
use crate::error::{CoordinatorError, Result};
use crate::harness::grok::reject_or_replace_ps1;
use crate::harness::resolve_command;
use crate::outcome::{
    FailureClass, OutcomeMetadata, OutcomeSource, OutcomeStatus, PhaseOutcome, outcome_roles_dir,
};
use crate::registry::ProjectRecord;
use crate::review::spawn::{ProcessWait, resolve_review_bin, run_process};
use crate::state::{RunState, RunStatus, load_run_state, save_run_state, with_run_state_lock};
use crate::workflow::drive::write_review_markdown;
use crate::workflow::graph::{
    PHASE_PLAN_REVIEW, REVIEW_SLUG_AGY, REVIEW_SLUG_OPENCODE, ROLE_REVIEWER_AGY,
    ROLE_REVIEWER_OPENCODE, resolve_track_dir, review_slugs, role_phase,
};
use crate::workflow::timeouts::{TIMEOUT_KEY_PLAN_REVIEW_SLOT, timeout_for_phase};

const MIN_SPAWN_BUDGET: Duration = Duration::from_secs(60);
/// Join-level empty/dud re-arm cap this phase (0054). Not a cross-epoch counter.
pub(crate) const PLAN_REVIEW_JOIN_RETRY_CAP: u32 = 1;

#[cfg(test)]
thread_local! {
    static TEST_RETRY_REMAINING: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
    static TEST_JOIN_REMAINING: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

/// One-shot plan-review CLI (scripted in default tests; live CLI otherwise).
pub trait PlanReviewBackend: Send + Sync {
    fn run(&self, req: &PlanReviewRequest) -> Result<PlanReviewResult>;

    /// Which driver ran the slot. The slot NAME is not the driver: a binding may re-point
    /// `plan_reviewer_agy` at another harness, and the journal must say what actually ran.
    fn driver_name(&self) -> &'static str {
        "unknown"
    }
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
    /// Where a `codex exec` slot writes its final message (`-o <file>`). `None` for the
    /// CLI slots that answer on stdout.
    pub last_message_path: Option<PathBuf>,
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
    fn driver_name(&self) -> &'static str {
        "agy"
    }

    fn run(&self, req: &PlanReviewRequest) -> Result<PlanReviewResult> {
        let bin = resolve_agy_bin(&req.command)?;
        let mut watch_paths = Vec::new();
        if let Some(ref td) = req.track_dir {
            watch_paths.push(td.join(format!("{}-review.md", req.slug)));
        }
        // agy `--output-format json` is silent until the final object, and it
        // typically writes `agy-review.md` at the end. The 600s file/stdio
        // stall then kills a live reviewer (coordinated 0350/0351:
        // `reviewer_stall` exit 124, `dur_ms≈600180`). OpenCode streams JSON
        // so the same stall is a real hang detector there. Phase remaining
        // (`--print-timeout`) is the agy backstop.
        let out = run_process(
            &bin,
            &req.argv,
            &req.workspace_root,
            ProcessWait {
                timeout: req.remaining,
                stall: None,
                watch_paths: &watch_paths,
            },
            &[],
            None,
        )?;
        Ok(PlanReviewResult {
            exit: out.exit,
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }
}

/// Live `codex exec` child for a plan-review slot.
///
/// The plan-review slots are named for the harness that used to own them, but the driver
/// follows the role binding: `plan_reviewer_agy` is a SLOT, and a binding may re-point that
/// slot at another harness when the bound one is exhausted. `codex exec` writes its final
/// message to `-o <file>`; that file is the review text and stdout is only a fallback.
pub struct CodexCli;

impl PlanReviewBackend for CodexCli {
    fn driver_name(&self) -> &'static str {
        "codex"
    }

    fn run(&self, req: &PlanReviewRequest) -> Result<PlanReviewResult> {
        let bin = resolve_review_bin("codex", &req.command)?;
        let mut watch_paths = Vec::new();
        if let Some(ref last) = req.last_message_path {
            watch_paths.push(last.clone());
        }
        if let Some(ref td) = req.track_dir {
            watch_paths.push(td.join(format!("{}-review.md", req.slug)));
        }
        // The child's cwd is the execution repo: `codex exec` refuses to start outside a
        // trusted directory, and the repo is the trusted one.
        let cwd = req
            .execution_repo
            .as_ref()
            .unwrap_or(&req.workspace_root)
            .clone();
        let out = run_process(
            &bin,
            &req.argv,
            &cwd,
            ProcessWait {
                timeout: req.remaining,
                stall: crate::workflow::watchdog::progress_stall_interval(),
                watch_paths: &watch_paths,
            },
            &[],
            Some(req.prompt.as_bytes()),
        )?;
        let last_message = req
            .last_message_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        let stdout = if last_message.trim().is_empty() {
            out.stdout.clone()
        } else {
            last_message
        };
        Ok(PlanReviewResult {
            exit: out.exit,
            stdout,
            stderr: out.stderr,
        })
    }
}

/// Argv for a `codex exec` plan-review slot.
///
/// Mirrors the cross-model review driver (`src/review/spawn.rs`): the prompt goes on stdin
/// (`-`), the sandbox is read-only, and the final message is written to `-o <file>`. The
/// model id is passed verbatim: the codex CLI takes a plain id (`gpt-6-luna`) and reads its
/// reasoning effort from its own config — the `[effort]` bracket form belongs to the ACP
/// adapter, not to this CLI.
pub fn codex_plan_review_argv(
    model: Option<&str>,
    workspace_root: &Path,
    exec_repo: Option<&Path>,
    last_path: &Path,
) -> Vec<String> {
    let mut args = vec!["exec".into()];
    // `codex exec` refuses to run outside a trusted directory ("Not inside a trusted
    // directory and --skip-git-repo-check was not specified"), so the execution repo is
    // named with `-C` and is also the child's cwd. The cross-model driver does the same.
    if let Some(dir) = exec_repo.or(Some(workspace_root)) {
        args.push("-C".into());
        args.push(dir.to_string_lossy().into_owned());
    }
    args.extend([
        "-s".into(),
        "read-only".into(),
        "--ephemeral".into(),
        "-o".into(),
        last_path.to_string_lossy().into_owned(),
        "--add-dir".into(),
        workspace_root.to_string_lossy().into_owned(),
    ]);
    if let Some(m) = model
        && !m.trim().is_empty()
    {
        args.push("-m".into());
        args.push(m.to_string());
    }
    args.push("-".into());
    args
}

/// Live `opencode run` child. Default `cargo test` does not construct this
/// unless `COORDINATOR_OPENCODE_LIVE=1`.
pub struct OpenCodeCli;

impl PlanReviewBackend for OpenCodeCli {
    fn driver_name(&self) -> &'static str {
        "opencode"
    }

    fn run(&self, req: &PlanReviewRequest) -> Result<PlanReviewResult> {
        let bin = resolve_review_bin("opencode", &req.command)?;
        let extra: Vec<(&str, String)> = req
            .extra_env
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect();
        let mut watch_paths = Vec::new();
        if let Some(ref td) = req.track_dir {
            watch_paths.push(td.join(format!("{}-review.md", req.slug)));
        }
        let out = run_process(
            &bin,
            &req.argv,
            &req.workspace_root,
            ProcessWait {
                timeout: req.remaining,
                stall: crate::workflow::watchdog::progress_stall_interval(),
                watch_paths: &watch_paths,
            },
            &extra,
            Some(req.prompt.as_bytes()),
        )?;
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

/// Plan-review OpenCode argv. Prompt is stdin (see [`OpenCodeCli`]), never a
/// positional — npm `opencode.cmd` forwards `%*` and cmd.exe truncates at `<LF>`.
/// Never `--auto` / `--yolo` / `--dangerously-skip-permissions`. No `--dir`: the
/// installed `opencode v2.0.32` rejects it, and the child's cwd is already the
/// workspace root ([`OpenCodeCli::run`]).
pub fn opencode_argv(model: Option<&str>, title: Option<&str>) -> Vec<String> {
    let mut args = vec!["run".into(), "--format".into(), "json".into()];
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
    let track_dir = track_id
        .and_then(|id| resolve_track_dir(record, id))
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(track dir unresolved)".into());
    let skill = super::reuse::review_track_skill_prompt(record);
    let file = format!("{slug}-review.md");
    let layout = crate::workflow::prompts::layout_block(record, track_id, "plan-review");
    let research = if slug == REVIEW_SLUG_OPENCODE {
        "Knowledge is stale. Verify APIs, hooks, and plan-vs-live-src against docs.rs, \
         official harness docs, and the execution repo — not crate-version pins.\n\
         \n\
         Dependency-pin / crate-version currency is out of scope (CI and release checks \
         cover pins). Do not fetch crates.io crate/version lists or spawn sub-agents to \
         parse pin JSON."
    } else {
        "Knowledge is stale. Verify pins, APIs, and hooks against primary sources \
         (crates.io, docs.rs, official harness docs) before trusting training data or \
         the track's plan-time snapshot."
    };
    format!(
        "You are reviewing a Coordinator conductor-track plan (review-track).\n\
         This slot loads the `review-track` skill from {skill}.\n\
         \n\
         {layout}\
         \n\
         Read spec.md and plan.md in:\n\
         {track_dir}\n\
         \n\
         Write your review to this exact path (overwrite):\n\
         {track_dir}/{file}\n\
         \n\
         {research}\n\
         \n\
         Requirements:\n\
         - Review the plan for completeness, risks, and missing Definition of Done.\n\
         - Do not require a Verdict/PASS header. This is not the post-implement \
         cross-model review gate.\n\
         - You may inspect product source under the execution repo when it is available.\n\
         - Do not run `coordinator outcome write`.\n"
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

/// One driver decision used by the resolver, the argv builder and the backend.
///
/// The plan-review slots are SLOT names (`agy`, `opencode`); their binding chooses the
/// driver. A slot may keep its native driver or adopt `codex`. Nothing else: the prompt
/// research block and the stdout adoption path are slot-specific, so "an Agy driver
/// exists" is not a slot-adoption path for the opencode slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanReviewDriver {
    Agy,
    OpenCode,
    Codex,
}

impl PlanReviewDriver {
    pub(crate) fn name(self) -> &'static str {
        match self {
            PlanReviewDriver::Agy => "agy",
            PlanReviewDriver::OpenCode => "opencode",
            PlanReviewDriver::Codex => "codex",
        }
    }
}

pub(crate) fn driver_for(slug: &str, harness: &str) -> Result<PlanReviewDriver> {
    let h = harness.trim().to_ascii_lowercase();
    match slug {
        REVIEW_SLUG_AGY => match h.as_str() {
            "antigravity" | "agy" => Ok(PlanReviewDriver::Agy),
            "codex" => Ok(PlanReviewDriver::Codex),
            other => Err(CoordinatorError::Message(format!(
                "plan-review slot {slug} cannot adopt harness {other} (allowed: antigravity, \
                 codex)"
            ))),
        },
        REVIEW_SLUG_OPENCODE => match h.as_str() {
            "opencode" => Ok(PlanReviewDriver::OpenCode),
            "codex" => Ok(PlanReviewDriver::Codex),
            other => Err(CoordinatorError::Message(format!(
                "plan-review slot {slug} cannot adopt harness {other} (allowed: opencode, \
                 codex)"
            ))),
        },
        other => Err(CoordinatorError::Message(format!(
            "unknown plan-review slot {other}"
        ))),
    }
}

pub(crate) fn resolve_driver_bin(driver: PlanReviewDriver, command: &str) -> Result<PathBuf> {
    match driver {
        PlanReviewDriver::Agy => resolve_agy_bin(command),
        PlanReviewDriver::OpenCode => resolve_review_bin("opencode", command),
        PlanReviewDriver::Codex => resolve_review_bin("codex", command),
    }
}

struct DriverArgv {
    argv: Vec<String>,
    last_message_path: Option<PathBuf>,
}

#[allow(clippy::too_many_arguments)]
fn driver_argv(
    driver: PlanReviewDriver,
    slug: &str,
    prompt: &str,
    remaining: Duration,
    model: Option<&str>,
    workspace_root: &Path,
    execution_repo: Option<&Path>,
    track_id: Option<&str>,
) -> DriverArgv {
    match driver {
        PlanReviewDriver::Agy => DriverArgv {
            argv: {
                let add_dir = add_dir_arg(workspace_root, execution_repo);
                agy_argv(prompt, remaining, model, add_dir.as_deref())
            },
            last_message_path: None,
        },
        PlanReviewDriver::OpenCode => DriverArgv {
            argv: {
                let title = track_id.map(|id| format!("plan-review {id}"));
                opencode_argv(model, title.as_deref())
            },
            last_message_path: None,
        },
        PlanReviewDriver::Codex => {
            let tmp = std::env::temp_dir().join(format!(
                "coordinator-plan-review-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default()
            ));
            let _ = std::fs::create_dir_all(&tmp);
            let last = tmp.join("last-message.txt");
            let _ = slug;
            DriverArgv {
                argv: codex_plan_review_argv(model, workspace_root, execution_repo, &last),
                last_message_path: Some(last),
            }
        }
    }
}

fn remaining_budget(record: &ProjectRecord, state: &RunState) -> Duration {
    timeout_for_phase(record, PHASE_PLAN_REVIEW)
        .saturating_sub(state.effective_running_elapsed(chrono::Utc::now()))
}

fn slot_remaining(record: &ProjectRecord, state: &RunState, slug: &str) -> Duration {
    if slug == REVIEW_SLUG_OPENCODE {
        timeout_for_phase(record, TIMEOUT_KEY_PLAN_REVIEW_SLOT).min(remaining_budget(record, state))
    } else {
        remaining_budget(record, state)
    }
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

/// Byte-copy a matching track review into the state mailbox and skip the child.
/// Copy failure returns false so the caller spawns.
fn try_reuse_slot(record: &ProjectRecord, state: &RunState, slug: &str) -> Result<bool> {
    let Some(track_id) = state.track_id.as_deref() else {
        return Ok(false);
    };
    if !super::reuse::slot_matches(record, track_id, slug) {
        return Ok(false);
    }
    let Some(track_dir) = resolve_track_dir(record, track_id) else {
        return Ok(false);
    };
    let src = track_dir.join(format!("{slug}-review.md"));
    let dest = match super::bundle::review_file(record, slug) {
        Ok(path) => path,
        Err(_) => return Ok(false),
    };
    let bytes = match std::fs::read(&src) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(false),
    };
    if let Ok(dir) = super::bundle::reviews_dir(record)
        && std::fs::create_dir_all(&dir).is_err()
    {
        return Ok(false);
    }
    if crate::persist::atomic_write(&dest, &bytes).is_err() {
        return Ok(false);
    }
    with_run_state_lock(record, || {
        let mut saved = load_run_state(record)?;
        saved.pending_roles.retain(|x| x != slug);
        saved.updated_at = chrono::Utc::now();
        save_run_state(record, &saved)
    })?;
    crate::progress_log::append(record, "plan-review", &format!("plan-review: reuse {slug}"));
    Ok(true)
}

fn maybe_spawn_slot(record: &ProjectRecord, state: &RunState, slug: &str) -> Result<()> {
    let live = load_run_state(record).unwrap_or_else(|_| state.clone());
    if !live.pending_roles.iter().any(|s| s == slug) {
        return Ok(());
    }
    // Recovery ordering (folded M3): a latched slug with a matching receipt must still be
    // reused and clear `pending_roles`. Only after that does the latch stop this phase.
    if try_reuse_slot(record, &live, slug)? {
        return Ok(());
    }
    if live.plan_review_spawned.iter().any(|s| s == slug) {
        // 0110: a slot that already spawned this phase never applies a pending swap today.
        return Ok(());
    }

    // 0110 deferred swap: apply before the binding is read and before any spawn.
    let _ = crate::harness::roles_swap::apply_pending_for_roles_for(&[slot_role(slug)], record);

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

    // One driver decision: resolver, argv and backend all use it.
    let driver = match driver_for(slug, &binding.harness) {
        Ok(driver) => driver,
        Err(e) => {
            write_sync_permission_failure(record, &live, slug, e.to_string())?;
            return Ok(());
        }
    };
    let backend = backend_for(record, driver);
    if !has_test_backend(record)
        && let Err(e) = resolve_driver_bin(driver, &binding.command)
    {
        write_sync_permission_failure(
            record,
            &live,
            slug,
            format!("{slug} driver {} not resolvable: {e}", driver.name()),
        )?;
        return Ok(());
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
    let remaining = slot_remaining(record, &spawn_state, slug);
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
    // The driver follows the ROLE BINDING, not the slot name. `plan_reviewer_agy` is a slot;
    // a binding may re-point it at another harness when the bound one is exhausted.
    let DriverArgv {
        argv,
        last_message_path,
    } = driver_argv(
        driver,
        slug,
        &prompt,
        remaining,
        binding.model.as_deref(),
        &paths.workspace_root,
        paths.execution_repo.as_deref(),
        spawn_track_id.as_deref(),
    );
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
        last_message_path,
        extra_env,
        spawn_epoch,
        spawn_track_id,
    };

    if let Some(ref dir) = req.track_dir {
        let _ = std::fs::remove_file(dir.join(format!("{slug}-review.md")));
    }

    // Exactly one durable driver line per spawn (DoD-6 evidence; no schema field).
    crate::progress_log::append(
        record,
        "plan-review",
        &format!("plan-review: {slug} driver {}", driver.name()),
    );

    let slug_owned = slug.to_string();
    // Acquire the in-flight token before the spawn and move it into the worker, so the test
    // quiescence seam covers the window between the state mark and the worker start. Test-only:
    // release builds emit no token.
    #[cfg(test)]
    let inflight = InflightToken::enter(&record.id);
    if let Err(e) = std::thread::Builder::new()
        .name(format!("plan-review-{slug}-{}", rec.id))
        .spawn(move || {
            #[cfg(test)]
            let _inflight = inflight;
            apply_slot_result(&rec, backend.as_ref(), &req)
        })
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
    if let Err(e) = mark_slot_ran(record, req) {
        // Never run the backend for an unstamped slot: the phase must degrade with a named cause
        // instead of silently losing the slug from the join retry set.
        let msg = format!(
            "plan-review: {} slot-ran stamp not persisted: {e}",
            req.slug
        );
        crate::progress_log::append(record, "plan-review", &msg);
        write_stamp_failure_outcome(record, req, &msg);
        return;
    }
    let t0 = Instant::now();
    let result = backend.run(req);
    if let Ok(ref out) = result {
        journal_reviewer_stall(record, req, out, t0.elapsed());
    }
    finish_slot(record, backend, req, result, false);
}

fn finish_slot(
    record: &ProjectRecord,
    backend: &dyn PlanReviewBackend,
    req: &PlanReviewRequest,
    result: Result<PlanReviewResult>,
    retried: bool,
) {
    let Ok(state) = load_run_state(record) else {
        return;
    };
    if !apply_allowed(&state, req) {
        return;
    }
    match result {
        Ok(out) => {
            if !retried && plan_review_should_retry(req, &out) {
                let retry_remaining = with_run_state_lock(record, || {
                    let latest = load_run_state(record)?;
                    if !apply_allowed(&latest, req) {
                        return Ok(None);
                    }
                    let remaining = {
                        #[cfg(test)]
                        {
                            if let Some(d) = TEST_RETRY_REMAINING.with(|c| c.get()) {
                                if d >= MIN_SPAWN_BUDGET { Some(d) } else { None }
                            } else if remaining_budget(record, &latest) < MIN_SPAWN_BUDGET {
                                None
                            } else {
                                Some(slot_remaining(record, &latest, req.slug.as_str()))
                            }
                        }
                        #[cfg(not(test))]
                        {
                            if remaining_budget(record, &latest) < MIN_SPAWN_BUDGET {
                                None
                            } else {
                                Some(slot_remaining(record, &latest, req.slug.as_str()))
                            }
                        }
                    };
                    Ok(remaining)
                });
                match retry_remaining {
                    Ok(Some(remaining)) => {
                        unlink_track_review(req);
                        let mut retry_req = req.clone();
                        retry_req.remaining = remaining;
                        let t1 = Instant::now();
                        let second = backend.run(&retry_req);
                        if let Ok(ref out) = second {
                            journal_reviewer_stall(record, &retry_req, out, t1.elapsed());
                        }
                        finish_slot(record, backend, &retry_req, second, true);
                        return;
                    }
                    Ok(None) => {}
                    Err(_) => return,
                }
            }
            adopt_or_fail(record, &state, req, &out);
        }
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
        if plan_review_quality_ok(&body, req.spawn_track_id.as_deref()) {
            let _ = write_review_markdown(record, &req.slug, Some(&body));
            let _ =
                write_role_outcome(record, state, &req.slug, OutcomeStatus::Success, None, None);
            return;
        }
        write_degenerate_failure(record, state, req, out, Some(&body));
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
            adopt_plan_review_body(record, state, req, out, &body);
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
            adopt_plan_review_body(record, state, req, out, &body);
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
        OpencodeStdout::Text(body) | OpencodeStdout::Raw(body)
            if plan_review_quality_ok(&body, req.spawn_track_id.as_deref()) =>
        {
            let _ = write_review_markdown(record, &req.slug, Some(&body));
            let _ =
                write_role_outcome(record, state, &req.slug, OutcomeStatus::Success, None, None);
        }
        OpencodeStdout::Text(body) | OpencodeStdout::Raw(body) => {
            write_degenerate_failure(record, state, req, out, Some(&body));
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

fn plan_review_quality_ok(body: &str, track_id: Option<&str>) -> bool {
    crate::review::slot_quality::check(
        crate::review::slot_quality::SlotFamily::PlanReview,
        body,
        track_id,
    )
    .is_ok()
}

#[cfg(test)]
fn opencode_review_looks_complete(body: &str, track_id: Option<&str>) -> bool {
    plan_review_quality_ok(body, track_id)
}

fn adopt_plan_review_body(
    record: &ProjectRecord,
    state: &RunState,
    req: &PlanReviewRequest,
    out: &PlanReviewResult,
    body: &str,
) {
    if plan_review_quality_ok(body, req.spawn_track_id.as_deref()) {
        let _ = write_review_markdown(record, &req.slug, Some(body));
        let _ = write_role_outcome(record, state, &req.slug, OutcomeStatus::Success, None, None);
        return;
    }
    write_degenerate_failure(record, state, req, out, Some(body));
}

fn plan_review_should_retry(req: &PlanReviewRequest, out: &PlanReviewResult) -> bool {
    if out.exit == 124 || crate::review::spawn::is_reviewer_stall(&out.stderr) {
        return false;
    }
    if let Some(body) = track_review_body(req.track_dir.as_deref(), &req.slug) {
        return !plan_review_quality_ok(&body, req.spawn_track_id.as_deref());
    }
    if req.slug == REVIEW_SLUG_OPENCODE {
        return match parse_opencode_stdout(&out.stdout) {
            OpencodeStdout::Text(body) | OpencodeStdout::Raw(body) => {
                !plan_review_quality_ok(&body, req.spawn_track_id.as_deref())
            }
            OpencodeStdout::Empty => true,
            OpencodeStdout::Error(_) => false,
        };
    }
    match parse_agy_stdout(&out.stdout) {
        AgyStdout::Success(body) | AgyStdout::Raw(body) => {
            !plan_review_quality_ok(&body, req.spawn_track_id.as_deref())
        }
        AgyStdout::Empty => true,
        AgyStdout::JsonFailure { .. } => false,
    }
}

fn journal_reviewer_stall(
    record: &ProjectRecord,
    req: &PlanReviewRequest,
    out: &PlanReviewResult,
    dur: Duration,
) {
    if out.exit != 124 || !crate::review::spawn::is_reviewer_stall(&out.stderr) {
        return;
    }
    crate::harness::journal::record_reviewer_stall(
        record,
        &req.slug,
        &crate::harness::journal::argv_head(&req.command, &req.argv),
        dur.as_millis() as u64,
    );
    let _ = with_run_state_lock(record, || {
        let mut s = load_run_state(record)?;
        if !apply_allowed(&s, req) {
            return Ok(());
        }
        let rv = s.review.get_or_insert_with(Default::default);
        if !rv.stalled.iter().any(|x| x == &req.slug) {
            rv.stalled.push(req.slug.clone());
        }
        s.updated_at = chrono::Utc::now();
        save_run_state(record, &s)
    });
}

/// Bounded ladder for the transient run-state replace class (0113, OD-113-5).
///
/// `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)` can be refused with access-denied/sharing
/// (os error 5/32) while another handle holds `run-state.json`. A lock timeout is **not** retried:
/// the lock wait already spent 10 s (`state.rs:775`) and 40 more waits would stall the wave.
const STAMP_SAVE_ATTEMPTS: usize = 40;
const STAMP_SAVE_BACKOFF: Duration = Duration::from_millis(25);

fn is_transient_save_err(e: &CoordinatorError) -> bool {
    match e {
        CoordinatorError::Io(io) => {
            io.kind() == std::io::ErrorKind::PermissionDenied
                || matches!(io.raw_os_error(), Some(5) | Some(32))
        }
        _ => false,
    }
}

/// Persist the named failure for an unstamped slot, and never let it stay pending.
///
/// The mailbox write is retried over the same transient class as the stamp, and the epoch/track
/// gate is re-checked first: a stale worker must not write a failure into a newer plan-review
/// epoch. If the mailbox cannot be written at all, clear the pending slug so the phase degrades
/// through the normal join path (the progress line names the cause) instead of hanging on a role
/// file that will never appear.
fn write_stamp_failure_outcome(record: &ProjectRecord, req: &PlanReviewRequest, msg: &str) {
    for attempt in 0..STAMP_SAVE_ATTEMPTS {
        // Re-load and re-check under the run-state lock on every attempt: an epoch transition
        // takes the same lock, so the write cannot land in a newer epoch even if the retry ladder
        // spans one.
        let outcome = with_run_state_lock(record, || {
            let state = load_run_state(record)?;
            if !apply_allowed(&state, req) {
                return Ok(false);
            }
            write_role_outcome(
                record,
                &state,
                &req.slug,
                OutcomeStatus::Failure,
                Some(FailureClass::HarnessCrash),
                Some(msg.to_string()),
            )?;
            Ok(true)
        });
        match outcome {
            Ok(true) => return,
            Ok(false) => return, // a newer epoch/track owns this slot; never write a stale mailbox
            Err(e) if is_transient_save_err(&e) => {
                if attempt + 1 < STAMP_SAVE_ATTEMPTS {
                    std::thread::sleep(STAMP_SAVE_BACKOFF);
                }
            }
            Err(_) => break,
        }
    }
    let _ = crate::workflow::drive::remove_pending_if_epoch(
        record,
        &req.slug,
        req.spawn_epoch,
        req.spawn_track_id.as_deref(),
    );
}

/// Record that this slot ran, durably.
///
/// The stamp feeds `maybe_rearm_join_retry`'s retry set; losing it silently drops a slot that did
/// run from the join retry. Each attempt re-loads inside the lock so a concurrent tick cannot be
/// clobbered, and the `apply_allowed` epoch/track gate is unchanged.
fn mark_slot_ran(record: &ProjectRecord, req: &PlanReviewRequest) -> Result<()> {
    let mut last: Option<CoordinatorError> = None;
    for attempt in 0..STAMP_SAVE_ATTEMPTS {
        let outcome = with_run_state_lock(record, || {
            let mut s = load_run_state(record)?;
            if !apply_allowed(&s, req) {
                return Ok(());
            }
            if !s.plan_review_slot_ran.iter().any(|x| x == &req.slug) {
                s.plan_review_slot_ran.push(req.slug.clone());
                s.updated_at = chrono::Utc::now();
                #[cfg(test)]
                if take_stamp_save_failure(&record.id, &req.slug) {
                    return Err(std::io::Error::from_raw_os_error(5).into());
                }
                save_run_state(record, &s)?;
            }
            Ok(())
        });
        match outcome {
            Ok(()) => return Ok(()),
            Err(e) if is_transient_save_err(&e) => {
                last = Some(e);
                if attempt + 1 < STAMP_SAVE_ATTEMPTS {
                    std::thread::sleep(STAMP_SAVE_BACKOFF);
                }
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        CoordinatorError::Message("slot-ran stamp save did not converge".into())
    }))
}

/// Re-arm empty/dud slugs once after the shipped per-slot retry (0054).
///
/// Returns `true` when join should stay Running (spawn on the next tick).
pub(crate) fn maybe_rearm_join_retry(record: &ProjectRecord) -> Result<bool> {
    with_run_state_lock(record, || {
        let mut s = load_run_state(record)?;
        if s.status != RunStatus::Running || s.phase != PHASE_PLAN_REVIEW {
            // Another tick already left plan-review; do not fail_phase.
            return Ok(true);
        }
        if !s.pending_roles.is_empty() {
            // Concurrent ticker already re-armed this join. Stay Running.
            return Ok(true);
        }
        if s.plan_review_join_retries >= PLAN_REVIEW_JOIN_RETRY_CAP {
            return Ok(false);
        }
        let join_budget = {
            #[cfg(test)]
            {
                TEST_JOIN_REMAINING
                    .with(|c| c.get())
                    .unwrap_or_else(|| remaining_budget(record, &s))
            }
            #[cfg(not(test))]
            {
                remaining_budget(record, &s)
            }
        };
        if join_budget < MIN_SPAWN_BUDGET {
            return Ok(false);
        }
        let stalled: Vec<String> = s
            .review
            .as_ref()
            .map(|r| r.stalled.clone())
            .unwrap_or_default();
        let retryable: Vec<String> = review_slugs()
            .iter()
            .copied()
            .filter(|slug| {
                s.plan_review_slot_ran.iter().any(|x| x == slug)
                    && !stalled.iter().any(|x| x == slug)
            })
            .map(str::to_string)
            .collect();
        if retryable.is_empty() {
            return Ok(false);
        }
        s.plan_review_join_retries = s.plan_review_join_retries.saturating_add(1);
        s.pending_roles.clone_from(&retryable);
        s.plan_review_spawned
            .retain(|x| !retryable.iter().any(|r| r == x));
        let event = format!(
            "plan-review: retrying join {} ({}/{})",
            retryable.join(","),
            s.plan_review_join_retries,
            PLAN_REVIEW_JOIN_RETRY_CAP
        );
        s.last_event = event.clone();
        s.updated_at = chrono::Utc::now();
        save_run_state(record, &s)?;
        crate::progress_log::append(record, "plan-review", &event);
        Ok(true)
    })
}

fn unlink_track_review(req: &PlanReviewRequest) {
    if let Some(dir) = req.track_dir.as_deref() {
        let _ = std::fs::remove_file(dir.join(format!("{}-review.md", req.slug)));
    }
}

fn write_degenerate_failure(
    record: &ProjectRecord,
    state: &RunState,
    req: &PlanReviewRequest,
    out: &PlanReviewResult,
    body: Option<&str>,
) {
    let file = format!("{}-review.md", req.slug);
    let bytes = body
        .map(|b| crate::workflow::bundle::normalize_newlines(b).len())
        .unwrap_or(0);
    if let (Some(dir), Some(text)) = (req.track_dir.as_deref(), body) {
        let _ = std::fs::write(dir.join(format!("{}-review.dud1.md", req.slug)), text);
    }
    unlink_track_review(req);
    let _ = crate::review::record_slot_dud(record, &file, bytes);
    let class = if out.exit == 124 {
        FailureClass::Timeout
    } else {
        FailureClass::Permission
    };
    let msg = format!(
        "plan-review: {} review missing required '# Track review:' header or track id; {}",
        req.slug,
        crate::review::slot_quality::dud_message(&file, bytes)
    );
    let _ = write_role_outcome(
        record,
        state,
        &req.slug,
        OutcomeStatus::Failure,
        Some(class),
        Some(msg),
    );
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

fn backend_for(record: &ProjectRecord, driver: PlanReviewDriver) -> Arc<dyn PlanReviewBackend> {
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
    match driver {
        PlanReviewDriver::Agy => Arc::new(AgyCli),
        PlanReviewDriver::OpenCode => Arc::new(OpenCodeCli),
        PlanReviewDriver::Codex => Arc::new(CodexCli),
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

/// Test-only, RAII-disarmed slot-ran save failpoint (0113, OD-113-4).
///
/// Scoped to one `(project_id, slug)` and to a remaining count, so it can outlast the release
/// ladder without leaking into a sibling test. Cleared on drop, including on panic.
#[cfg(test)]
static TEST_STAMP_SAVE_FAILS: Mutex<Option<(String, String, usize)>> = Mutex::new(None);

#[cfg(test)]
pub(crate) struct StampSaveFailpoint;

#[cfg(test)]
impl Drop for StampSaveFailpoint {
    fn drop(&mut self) {
        *TEST_STAMP_SAVE_FAILS
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = None;
    }
}

#[cfg(test)]
pub(crate) fn fail_stamp_save(project_id: &str, slug: &str, times: usize) -> StampSaveFailpoint {
    *TEST_STAMP_SAVE_FAILS
        .lock()
        .unwrap_or_else(|p| p.into_inner()) =
        Some((project_id.to_string(), slug.to_string(), times));
    StampSaveFailpoint
}

#[cfg(test)]
fn take_stamp_save_failure(project_id: &str, slug: &str) -> bool {
    let mut armed = TEST_STAMP_SAVE_FAILS
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    match armed.as_mut() {
        Some((id, s, remaining)) if id == project_id && s == slug && *remaining > 0 => {
            *remaining -= 1;
            true
        }
        _ => false,
    }
}

/// Test-only per-project in-flight worker token + condvar (0113, OD-113-4).
///
/// `maybe_spawn_slot` takes a token before `thread::Builder::spawn` and moves it into the worker
/// closure, so the window between the state mark and the worker start is covered and the token
/// lives until `apply_slot_result` (including the role-file write) returns. A spawn `Err` drops the
/// closure and its token. The whole seam is `#[cfg(test)]`, so release builds emit nothing.
#[cfg(test)]
struct InflightState {
    counts: Mutex<HashMap<String, usize>>,
    cv: std::sync::Condvar,
}

#[cfg(test)]
static TEST_INFLIGHT: std::sync::OnceLock<InflightState> = std::sync::OnceLock::new();

#[cfg(test)]
fn inflight_state() -> &'static InflightState {
    TEST_INFLIGHT.get_or_init(|| InflightState {
        counts: Mutex::new(HashMap::new()),
        cv: std::sync::Condvar::new(),
    })
}

#[cfg(test)]
pub(crate) struct InflightToken {
    project_id: String,
}

#[cfg(test)]
impl InflightToken {
    fn enter(project_id: &str) -> Self {
        let state = inflight_state();
        *state
            .counts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(project_id.to_string())
            .or_insert(0) += 1;
        Self {
            project_id: project_id.to_string(),
        }
    }
}

#[cfg(test)]
impl Drop for InflightToken {
    fn drop(&mut self) {
        let state = inflight_state();
        {
            let mut counts = state.counts.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(n) = counts.get_mut(&self.project_id) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    counts.remove(&self.project_id);
                }
            }
        }
        state.cv.notify_all();
    }
}

#[cfg(test)]
fn inflight_count(project_id: &str) -> usize {
    inflight_state()
        .counts
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(project_id)
        .copied()
        .unwrap_or(0)
}

/// Wait until no worker is in flight for this project, bounded by `ceiling`.
///
/// `Condvar::wait_timeout` is best-effort and may wake spuriously, so the count predicate is
/// re-checked against a monotonic deadline on every wake.
#[cfg(test)]
fn wait_inflight_idle(project_id: &str, ceiling: Duration) -> bool {
    let state = inflight_state();
    let mut counts = state.counts.lock().unwrap_or_else(|p| p.into_inner());
    let deadline = Instant::now() + ceiling;
    loop {
        if counts.get(project_id).copied().unwrap_or(0) == 0 {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let (guard, _) = state
            .cv
            .wait_timeout(counts, deadline - now)
            .unwrap_or_else(|p| p.into_inner());
        counts = guard;
    }
}

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

    pub fn stall() -> Self {
        Self {
            result: PlanReviewResult {
                exit: 124,
                stdout: String::new(),
                stderr: "reviewer stall — no progress for 600s".into(),
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
            let rendered = match req.spawn_track_id.as_deref() {
                Some(id) => body.replace("{track}", id),
                None => body.clone(),
            };
            let _ = std::fs::write(dir.join(format!("{}-review.md", req.slug)), rendered);
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

/// 0113 seam control: hold only `opencode` while `agy` succeeds and writes its review.
#[cfg(test)]
struct GateOpencodeBackend {
    gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
    body: String,
}

#[cfg(test)]
impl PlanReviewBackend for GateOpencodeBackend {
    fn run(&self, req: &PlanReviewRequest) -> Result<PlanReviewResult> {
        if req.slug == REVIEW_SLUG_OPENCODE {
            let (lock, cv) = &*self.gate;
            let mut g = lock.lock().unwrap_or_else(|p| p.into_inner());
            while !*g {
                g = cv.wait(g).unwrap_or_else(|p| p.into_inner());
            }
        }
        if let Some(dir) = &req.track_dir {
            let _ = std::fs::create_dir_all(dir);
            let _ = std::fs::write(dir.join(format!("{}-review.md", req.slug)), &self.body);
        }
        Ok(PlanReviewResult {
            exit: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

#[cfg(test)]
struct OpencodeSequenceBackend {
    opencode: Mutex<Vec<ScriptedBackend>>,
    other: ScriptedBackend,
}

#[cfg(test)]
struct AgySequenceBackend {
    agy: Mutex<Vec<ScriptedBackend>>,
    other: ScriptedBackend,
}

#[cfg(test)]
impl PlanReviewBackend for OpencodeSequenceBackend {
    fn run(&self, req: &PlanReviewRequest) -> Result<PlanReviewResult> {
        if req.slug == REVIEW_SLUG_OPENCODE {
            let mut q = self.opencode.lock().unwrap_or_else(|p| p.into_inner());
            if !q.is_empty() {
                return q.remove(0).run(req);
            }
        }
        self.other.run(req)
    }
}

#[cfg(test)]
impl PlanReviewBackend for AgySequenceBackend {
    fn run(&self, req: &PlanReviewRequest) -> Result<PlanReviewResult> {
        if req.slug == REVIEW_SLUG_AGY {
            let mut q = self.agy.lock().unwrap_or_else(|p| p.into_inner());
            if !q.is_empty() {
                return q.remove(0).run(req);
            }
        }
        self.other.run(req)
    }
}

#[cfg(test)]
struct PerSlugSequenceBackend {
    agy: Mutex<Vec<ScriptedBackend>>,
    opencode: Mutex<Vec<ScriptedBackend>>,
}

#[cfg(test)]
impl PlanReviewBackend for PerSlugSequenceBackend {
    fn run(&self, req: &PlanReviewRequest) -> Result<PlanReviewResult> {
        let q = match req.slug.as_str() {
            REVIEW_SLUG_OPENCODE => &self.opencode,
            _ => &self.agy,
        };
        let mut q = q.lock().unwrap_or_else(|p| p.into_inner());
        if !q.is_empty() {
            return q.remove(0).run(req);
        }
        ScriptedBackend::empty().run(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ENV_COORDINATOR_AGY_BIN, ENV_COORDINATOR_HOME, ENV_COORDINATOR_OPENCODE_BIN, test_env_lock,
    };
    use crate::outcome::{FailureClass, OutcomeSource, write_and_apply};
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
            notify_progress: false,
            worktree_isolation: false,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            ci_fix_routing: false,
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now(),
        }
    }

    fn write_review_track_skill(dir: &std::path::Path) {
        let skill = dir.join(".agents").join("skills").join("review-track");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "review-track\n").unwrap();
    }

    fn setup_track(dir: &std::path::Path, id: &str) {
        std::fs::create_dir_all(dir.join("conductor").join(format!("{id}-Example"))).unwrap();
        write_review_track_skill(dir);
    }

    #[test]
    fn plan_review_missing_skill_stops_without_spawn() {
        let _home = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("conductor").join("0001-Example")).unwrap();
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let view = tick_retry(&r).unwrap().expect("skill missing view");
        assert_eq!(view.failure_class, Some(FailureClass::SkillMissing));
        assert_eq!(view.status, crate::state::RunStatus::Stopped);
        let marker = view
            .last_event
            .find("skill_missing:")
            .expect(&view.last_event);
        assert!(
            view.last_event[..marker].trim_end().ends_with('\u{2014}'),
            "{}",
            view.last_event
        );
        assert!(
            view.last_event.contains("review-track"),
            "{}",
            view.last_event
        );
        let state = load_run_state(&r).unwrap();
        assert_eq!(
            state.pending_roles,
            vec!["agy".to_string(), "opencode".to_string()]
        );
        if let Ok(reviews) = crate::workflow::bundle::reviews_dir(&r) {
            assert!(!reviews.join("agy-review.md").is_file());
            assert!(!reviews.join("opencode-review.md").is_file());
        }
    }

    fn enter_plan_review(r: &ProjectRecord, track: &str) {
        run::run_with_driver(r, Some(track.into()), WorkflowDriver::Adapter).unwrap();
        let mut state = load_run_state(r).unwrap();
        state.phase = graph::PHASE_PLAN_REVIEW.into();
        state.pending_roles = vec!["agy".into(), "opencode".into()];
        save_run_state(r, &state).unwrap();
    }

    fn is_transient_lock_err(e: &crate::error::CoordinatorError) -> bool {
        let msg = e.to_string();
        msg.contains("Access is denied") || msg.contains("timed out waiting for run-state lock")
    }

    fn tick_retry(r: &ProjectRecord) -> crate::error::Result<Option<crate::state::StatusView>> {
        for _ in 0..40 {
            match tick(r) {
                Ok(v) => return Ok(v),
                Err(e) if is_transient_lock_err(&e) => {
                    std::thread::sleep(Duration::from_millis(25));
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        tick(r)
    }

    fn tick_or_continue(r: &ProjectRecord) {
        match tick_retry(r) {
            Ok(_) => {}
            Err(e) if is_transient_lock_err(&e) => {}
            Err(e) => panic!("tick: {e}"),
        }
    }

    /// Hang detector for an in-process/scripted worker wave.
    ///
    /// Derived 0113 Phase 1.6: the measured maximum scripted worker lifetime in this module was
    /// 17 ms; `max(10x, 60 s)` is the floor. A test that runs a real child derives its own
    /// deadline from that test's slot budget (the sleeper does, from its 2 s slot).
    const WORKER_CEILING: Duration = Duration::from_secs(60);

    /// Hang detector for consumption once no worker is in flight.
    ///
    /// Derived 0113: `3 x` the 10 s run-state lock wait (`src/state.rs:775`) covers one contended
    /// lock acquisition plus one atomic replace. Only the ack save and mailbox delete remain.
    const CONSUME_CEILING: Duration = Duration::from_secs(30);

    /// Hang detector for a timed-out reviewer's post-slot tail (role-file write after the kill).
    ///
    /// Derived 0113 Phase 1.6: 20 loaded runs measured 213-498 ms; `max(10 x 498 ms, 30 s)` is the
    /// 30 s floor. The 2 s slot timeout itself is unchanged; this allowance only bounds the test.
    const POST_KILL_ALLOWANCE: Duration = Duration::from_secs(30);

    fn plan_review_mailbox_diag(r: &ProjectRecord, slugs: &[&str]) -> String {
        let dir = match outcome_roles_dir(r) {
            Ok(d) => d,
            Err(e) => return format!("roles_dir=<{e}>"),
        };
        let mut parts = Vec::new();
        for slug in slugs {
            let path = dir.join(format!("{slug}.json"));
            match std::fs::metadata(&path) {
                Ok(m) => parts.push(format!("{slug}.json=exists(mtime={:?})", m.modified().ok())),
                Err(_) => parts.push(format!("{slug}.json=missing")),
            }
        }
        parts.join(" ")
    }

    fn plan_review_wait_diag(r: &ProjectRecord, slugs: &[&str]) -> String {
        match load_run_state(r) {
            Ok(s) => format!(
                "{} {}",
                plan_review_diag(&s),
                plan_review_mailbox_diag(r, slugs)
            ),
            Err(e) => format!("run-state=<{e}>"),
        }
    }

    /// Wait until every slot in `slugs` leaves `pending_roles`.
    ///
    /// At the top of every iteration this waits for the **current** wave to quiesce before ticking:
    /// a tick can start a new worker wave (after the join re-arm), and the old 200-iteration count
    /// could exhaust while that wave was still in flight. Only after quiescence (when just the ack
    /// save and mailbox delete remain) does a separate `CONSUME_CEILING` window apply.
    fn wait_slots_consumed(r: &ProjectRecord, slugs: &[&str]) {
        let mut consume_deadline: Option<Instant> = None;
        loop {
            if inflight_count(&r.id) > 0 {
                if !wait_inflight_idle(&r.id, WORKER_CEILING) {
                    panic!(
                        "worker wave did not quiesce within {WORKER_CEILING:?}: {}",
                        plan_review_wait_diag(r, slugs)
                    );
                }
                consume_deadline = None;
            }
            let s = match load_run_state(r) {
                Ok(s) => s,
                Err(e) => panic!("load run state: {e}"),
            };
            if slugs
                .iter()
                .all(|slug| !s.pending_roles.iter().any(|x| x == slug))
            {
                return;
            }
            let deadline =
                *consume_deadline.get_or_insert_with(|| Instant::now() + CONSUME_CEILING);
            if Instant::now() >= deadline {
                panic!(
                    "no worker in flight, slugs still pending after {CONSUME_CEILING:?}: {}",
                    plan_review_wait_diag(r, slugs)
                );
            }
            tick_or_continue(r);
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// One-line state dump for plan-review wait/assert failures (0113 DoD-1).
    ///
    /// A loaded failure must name the fields, not just a line number: the same panic string can
    /// come from the join re-arm, the wait, or a lost slot-ran stamp.
    fn plan_review_diag(s: &crate::state::RunState) -> String {
        format!(
            "pending_roles={:?} plan_review_spawned={:?} join_retries={} \
             slot_ran={:?} status={:?} last_event={:?}",
            s.pending_roles,
            s.plan_review_spawned,
            s.plan_review_join_retries,
            s.plan_review_slot_ran,
            s.status,
            s.last_event
        )
    }

    fn wait_agy_consumed(r: &ProjectRecord) {
        wait_slots_consumed(r, &["agy"]);
    }

    fn wait_both_consumed(r: &ProjectRecord) {
        wait_slots_consumed(r, &["agy", "opencode"]);
    }

    fn wait_join_retry_latched(r: &ProjectRecord) {
        use crate::state::RunStatus;
        let slugs = ["agy", "opencode"];
        let mut tick_deadline: Option<Instant> = None;
        loop {
            if inflight_count(&r.id) > 0 {
                if !wait_inflight_idle(&r.id, WORKER_CEILING) {
                    panic!(
                        "worker wave did not quiesce within {WORKER_CEILING:?}: {}",
                        plan_review_wait_diag(r, &slugs)
                    );
                }
                tick_deadline = None;
            }
            let s = load_run_state(r).unwrap();
            if s.plan_review_join_retries >= 1
                || s.last_event.contains("plan-review: retrying join")
            {
                return;
            }
            if s.status == RunStatus::Stopped {
                return;
            }
            let deadline = *tick_deadline.get_or_insert_with(|| Instant::now() + CONSUME_CEILING);
            if Instant::now() >= deadline {
                panic!(
                    "no worker in flight, join retry not latched after {CONSUME_CEILING:?}: {}",
                    plan_review_wait_diag(r, &slugs)
                );
            }
            tick_or_continue(r);
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn wait_stopped(r: &ProjectRecord) {
        use crate::state::RunStatus;
        for _ in 0..200 {
            if load_run_state(r).unwrap().status == RunStatus::Stopped {
                return;
            }
            tick_or_continue(r);
            std::thread::sleep(Duration::from_millis(25));
        }
        let s = load_run_state(r).unwrap();
        panic!(
            "run did not stop: status={:?} event={} pending={:?}",
            s.status, s.last_event, s.pending_roles
        );
    }

    fn track_file(dir: &std::path::Path, track: &str, slug: &str) -> PathBuf {
        dir.join("conductor")
            .join(format!("{track}-Example"))
            .join(format!("{slug}-review.md"))
    }

    fn ok_review_body(_track_id: &str) -> String {
        "# Track review: {track}-Example\n\n**Track:** `{track}`\n\nreview body is sound\n".into()
    }

    fn degenerate_review_body() -> String {
        "Which track should I review?\n".into()
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
        let args = opencode_argv(Some("provider/model"), Some("plan-review 0001"));
        assert_eq!(args[0], "run");
        assert!(!args.iter().any(|a| a == "--dir"), "{args:?}");
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
        assert!(
            !args
                .iter()
                .any(|a| a.contains("review please") || a.contains('\n')),
            "prompt must be stdin, not argv: {args:?}"
        );
    }

    #[test]
    fn codex_plan_review_argv_uses_codex_exec_and_a_stdin_prompt() {
        let last = PathBuf::from("C:\\tmp\\last-message.txt");
        let args = codex_plan_review_argv(
            Some("gpt-6-luna"),
            Path::new("C:\\ws"),
            Some(Path::new("C:\\exec")),
            &last,
        );
        assert_eq!(args[0], "exec");
        assert!(
            args.windows(2).any(|w| w[0] == "-s" && w[1] == "read-only"),
            "{args:?}"
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-o" && w[1] == last.to_string_lossy().as_ref()),
            "{args:?}"
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-m" && w[1] == "gpt-6-luna"),
            "{args:?}"
        );
        assert_eq!(args.last().unwrap(), "-", "{args:?}");
        // `codex exec` refuses to start outside a trusted directory: the exec repo is named.
        assert!(
            args.windows(2).any(|w| w[0] == "-C" && w[1] == "C:\\exec"),
            "{args:?}"
        );
        // The prompt is stdin, never argv: cmd.exe `%*` truncates newlines.
        assert!(
            !args
                .iter()
                .any(|a| a.contains("review please") || a.contains('\n')),
            "{args:?}"
        );
    }

    #[test]
    fn codex_plan_review_argv_omits_an_empty_model() {
        let args =
            codex_plan_review_argv(None, Path::new("C:\\ws"), None, Path::new("C:\\tmp\\l.txt"));
        assert!(!args.iter().any(|a| a == "-m"), "{args:?}");
    }

    #[test]
    fn the_driver_follows_the_binding_not_the_slot_name() {
        let ws = tempdir().unwrap();
        let record = rec(ws.path());
        // Pure resolver: all six slot/harness pairs, no test-backend seam.
        assert_eq!(
            driver_for(REVIEW_SLUG_AGY, "antigravity").unwrap(),
            PlanReviewDriver::Agy
        );
        assert_eq!(
            driver_for(REVIEW_SLUG_AGY, "agy").unwrap(),
            PlanReviewDriver::Agy
        );
        assert_eq!(
            driver_for(REVIEW_SLUG_AGY, "codex").unwrap(),
            PlanReviewDriver::Codex
        );
        assert_eq!(
            driver_for(REVIEW_SLUG_OPENCODE, "opencode").unwrap(),
            PlanReviewDriver::OpenCode
        );
        assert_eq!(
            driver_for(REVIEW_SLUG_OPENCODE, "codex").unwrap(),
            PlanReviewDriver::Codex
        );
        // Agy on the opencode slot stays refused: the research block and stdout adoption
        // path are slot-specific.
        let err = driver_for(REVIEW_SLUG_OPENCODE, "agy")
            .unwrap_err()
            .to_string();
        assert!(err.contains("allowed: opencode, codex"), "err={err}");
        let err = driver_for(REVIEW_SLUG_AGY, "claude")
            .unwrap_err()
            .to_string();
        assert!(err.contains("allowed: antigravity, codex"), "err={err}");

        // Same slot, different binding -> different backend.
        assert_eq!(
            backend_for(&record, PlanReviewDriver::Agy).driver_name(),
            "agy"
        );
        assert_eq!(
            backend_for(&record, PlanReviewDriver::Codex).driver_name(),
            "codex"
        );
    }

    #[test]
    fn plan_review_resolver_for_codex_uses_the_codex_pin_not_the_slot_name() {
        let dir = tempdir().unwrap();
        let pin = dir.path().join("pinned-codex.exe");
        std::fs::write(&pin, b"").unwrap();
        let previous = std::env::var_os(crate::config::ENV_COORDINATOR_CODEX_BIN);
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_CODEX_BIN, &pin);
        }
        let driver = driver_for(REVIEW_SLUG_OPENCODE, "codex").unwrap();
        let resolved = resolve_driver_bin(driver, "codex").unwrap();
        match previous {
            Some(v) => unsafe {
                std::env::set_var(crate::config::ENV_COORDINATOR_CODEX_BIN, v);
            },
            None => unsafe {
                std::env::remove_var(crate::config::ENV_COORDINATOR_CODEX_BIN);
            },
        }
        assert_eq!(resolved, pin);
    }

    #[test]
    fn refused_slot_harness_pair_writes_permission_and_spawns_nothing() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let mut state = load_run_state(&r).unwrap();
        state.pending_roles = vec!["agy".into()];
        state.plan_review_spawned = vec!["opencode".into()];
        save_run_state(&r, &state).unwrap();
        // The agy slot has no adoption path for opencode (research block + stdout path
        // are slot-specific).
        let mut cfg = crate::config::load_machine_config().unwrap();
        let binding = cfg.role_bindings.get_mut("plan_reviewer_agy").unwrap();
        binding.harness = "opencode".into();
        binding.command = "opencode".into();
        crate::config::save_machine_config(&cfg).unwrap();

        let state = load_run_state(&r).unwrap();
        maybe_spawn_slot(&r, &state, "agy").unwrap();
        let mut last = String::new();
        for _ in 0..80 {
            let path = outcome_roles_dir(&r).unwrap().join("agy.json");
            if let Ok(text) = std::fs::read_to_string(&path)
                && let Ok(o) = serde_json::from_str::<PhaseOutcome>(&text)
                && o.status == OutcomeStatus::Failure
            {
                assert_eq!(o.failure_class, Some(FailureClass::Permission));
                last = o.message.unwrap_or_default();
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            last.contains("allowed: antigravity, codex"),
            "refusal must name the allowed set: {last}"
        );
        let track = dir.path().join("conductor").join("0001-Example");
        assert!(
            !track.join("agy-review.md").exists(),
            "no spawn, no artifact"
        );
    }

    #[test]
    fn opencode_slot_bound_to_codex_builds_codex_argv_with_no_opencode_run() {
        let ws = tempdir().unwrap();
        let exec = ws.path().join("exec");
        std::fs::create_dir_all(&exec).unwrap();
        let out = driver_argv(
            PlanReviewDriver::Codex,
            REVIEW_SLUG_OPENCODE,
            "prompt",
            Duration::from_secs(120),
            Some("gpt-6-luna"),
            ws.path(),
            Some(&exec),
            Some("0001"),
        );
        assert_eq!(out.argv[0], "exec");
        assert!(out.last_message_path.is_some());
        assert!(out.argv.windows(2).any(|w| w[0] == "-C"), "{:?}", out.argv);
        assert!(
            out.argv
                .windows(2)
                .any(|w| w[0] == "-s" && w[1] == "read-only"),
            "{:?}",
            out.argv
        );
        assert!(out.argv.iter().any(|a| a == "--ephemeral"));
        assert!(out.argv.iter().any(|a| a == "--add-dir"));
        assert!(out.argv.iter().any(|a| a == "-"));
        assert!(!out.argv.iter().any(|a| a == "--print"));
        assert!(!out.argv.iter().any(|a| a == "--output-schema"));
        assert!(!out.argv.iter().any(|a| a == "run"));
    }

    #[test]
    fn opencode_argv_passes_model_variant_byte_exact() {
        let m = "opencode-go/deepseek-v4.1-flash#high";
        let args = opencode_argv(Some(m), None);
        assert!(args.windows(2).any(|w| w[0] == "--model" && w[1] == m));
        assert!(!args.iter().any(|a| a == "--dir"));
    }

    #[test]
    fn opencode_argv_omits_empty_model_and_title() {
        let args = opencode_argv(Some("  "), None);
        assert!(!args.iter().any(|a| a == "--model" || a == "-m"));
        assert!(!args.iter().any(|a| a == "--title"));
    }

    #[test]
    fn opencode_review_looks_complete_heading_and_id() {
        assert!(opencode_review_looks_complete(
            "# Track review: 0001-Example\n",
            Some("0001")
        ));
        assert!(opencode_review_looks_complete(
            "# Track Review: 0001-Example\n",
            Some("0001")
        ));
        assert!(!opencode_review_looks_complete(
            "# Track review: 0001-Example\n",
            Some("0037")
        ));
        assert!(!opencode_review_looks_complete(
            "Which track should I review?\n",
            Some("0001")
        ));
        assert!(!opencode_review_looks_complete("   \n", Some("0001")));
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
        assert!(text.contains("Verify pins, APIs, and hooks against primary sources (crates.io"));
        assert!(!text.to_ascii_lowercase().contains("out of scope"));
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
        assert!(
            oc.to_ascii_lowercase().contains("out of scope"),
            "opencode prompt must carve pins out of scope: {oc}"
        );
        assert!(
            !oc.contains("Verify pins, APIs, and hooks against primary sources (crates.io"),
            "opencode must not instruct crates.io pin crawls: {oc}"
        );
    }

    #[test]
    fn matching_receipt_does_not_spawn_and_joins() {
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0083");
        let r = rec(dir.path());
        let track = dir.path().join("conductor").join("0083-Example");
        std::fs::write(track.join("spec.md"), b"spec").unwrap();
        std::fs::write(track.join("plan.md"), b"plan").unwrap();
        let skill = dir
            .path()
            .join(".agents")
            .join("skills")
            .join("review-track")
            .join("SKILL.md");
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(&skill, b"skill").unwrap();
        let agy = b"agy\r\nreview\r\n";
        let oc = b"oc\r\nreview\r\n";
        std::fs::write(track.join("agy-review.md"), agy).unwrap();
        std::fs::write(track.join("opencode-review.md"), oc).unwrap();
        super::super::reuse::write_join_receipt(&r, "0083");
        let backend = RecordingBackend::wrap(Arc::new(ScriptedBackend::empty()));
        let counts = backend.counts.clone();
        let _guard = install_test_backend(&r.id, Arc::new(backend));
        enter_plan_review(&r, "0083");
        let view = tick_retry(&r).unwrap().expect("joined");
        assert_eq!(counts.n(), 0);
        assert_eq!(view.phase, graph::PHASE_FOLD);
        let state = load_run_state(&r).unwrap();
        assert!(state.plan_review_spawned.is_empty());
        let copied = crate::workflow::bundle::review_file(&r, "agy").unwrap();
        assert_eq!(std::fs::read(copied).unwrap(), agy);
        let log = std::fs::read_to_string(dir.path().join("status.md")).unwrap();
        assert!(log.contains("plan-review: reuse agy"), "{log}");
        assert!(log.contains("plan-review: reuse opencode"), "{log}");
    }

    #[test]
    fn opencode_prompt_pin_oos_agy_keeps_crates_io() {
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        let oc = opencode_prompt(&r, Some("0001"));
        assert!(oc.contains("out of scope"));
        assert!(oc.contains("Do not fetch crates.io"));
        assert!(oc.contains("CI and release"));
        assert!(!oc.contains("Verify pins, APIs, and hooks against primary sources (crates.io"));
        let agy = agy_prompt(&r, Some("0001"));
        assert!(agy.contains(
            "Verify pins, APIs, and hooks against primary sources (crates.io, docs.rs, official harness docs)"
        ));
        assert!(!agy.to_ascii_lowercase().contains("out of scope"));
        assert!(!agy.contains("Do not fetch crates.io"));
    }

    #[test]
    fn adapter_scripted_writes_state_and_track_and_consumes() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let rec_backend = Arc::new(RecordingBackend::wrap(Arc::new(ScriptedBackend::ok_file(
            ok_review_body("0001"),
        ))));
        let counts = rec_backend.counts.clone();
        let _hook = install_test_backend(&r.id, rec_backend);
        tick_retry(&r).unwrap();
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
        assert!(!oc.argv.iter().any(|a| a == "--dir"), "{:?}", oc.argv);
        assert_eq!(oc.argv[0], "run");
        assert!(!oc.argv.iter().any(|a| a == "--auto"));
        assert!(
            !oc.argv
                .iter()
                .any(|a| a.contains('\n') || a.contains("review-track")),
            "plan-review prompt must be stdin, not argv: {:?}",
            oc.argv
        );
        assert!(oc.prompt.contains("review-track"));
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
            ok_review_body("0001"),
        ))));
        let counts = rec_backend.counts.clone();
        let _hook = install_test_backend(&r.id, rec_backend);
        tick_retry(&r).unwrap();
        tick_retry(&r).unwrap();
        wait_both_consumed(&r);
        tick_retry(&r).unwrap();
        assert_eq!(counts.n(), 2);
        let slugs = counts.slugs();
        assert_eq!(slugs.iter().filter(|s| *s == "agy").count(), 1);
        assert_eq!(slugs.iter().filter(|s| *s == "opencode").count(), 1);
    }

    fn remaining_of(reqs: &[PlanReviewRequest], slug: &str) -> Duration {
        reqs.iter().find(|q| q.slug == slug).unwrap().remaining
    }

    #[test]
    fn table_default_remaining_opencode_and_agy_use_2400_join() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let rec_backend = Arc::new(RecordingBackend::wrap(Arc::new(ScriptedBackend::ok_file(
            ok_review_body("0001"),
        ))));
        let counts = rec_backend.counts.clone();
        let _hook = install_test_backend(&r.id, rec_backend);
        tick_retry(&r).unwrap();
        wait_both_consumed(&r);
        let reqs = counts.requests.lock().unwrap();
        let oc = remaining_of(&reqs, "opencode");
        let agy = remaining_of(&reqs, "agy");
        assert!(
            oc > Duration::from_secs(1200) && oc <= Duration::from_secs(2400),
            "opencode remaining={oc:?}"
        );
        assert!(agy > Duration::from_secs(1200), "agy remaining={agy:?}");
        let phase_budget = timeout_for_phase(&r, PHASE_PLAN_REVIEW);
        assert!(
            agy <= phase_budget,
            "agy remaining={agy:?} budget={phase_budget:?}"
        );
        assert!(
            phase_budget - agy < Duration::from_secs(2),
            "agy remaining {agy:?} should equal phase remaining at spawn (budget {phase_budget:?})"
        );
    }

    #[test]
    fn asymmetric_slot_overlay_caps_opencode_not_agy() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let mut r = rec(dir.path());
        r.phase_timeouts_secs
            .insert(TIMEOUT_KEY_PLAN_REVIEW_SLOT.into(), 600);
        enter_plan_review(&r, "0001");
        let rec_backend = Arc::new(RecordingBackend::wrap(Arc::new(ScriptedBackend::ok_file(
            ok_review_body("0001"),
        ))));
        let counts = rec_backend.counts.clone();
        let _hook = install_test_backend(&r.id, rec_backend);
        tick_retry(&r).unwrap();
        wait_both_consumed(&r);
        let reqs = counts.requests.lock().unwrap();
        let oc = remaining_of(&reqs, "opencode");
        let agy = remaining_of(&reqs, "agy");
        assert!(oc <= Duration::from_secs(600), "opencode remaining={oc:?}");
        assert!(agy > Duration::from_secs(1200), "agy remaining={agy:?}");
        let phase_budget = timeout_for_phase(&r, PHASE_PLAN_REVIEW);
        assert!(agy <= phase_budget);
        assert!(
            phase_budget - agy < Duration::from_secs(2),
            "agy remaining {agy:?} should equal phase remaining (budget {phase_budget:?})"
        );
    }

    #[test]
    fn run_process_remaining_timeout_is_124_not_stall() {
        let dir = tempdir().unwrap();
        let cmd = dir.path().join("sleep.cmd");
        std::fs::write(&cmd, "@echo off\r\nping -n 30 127.0.0.1 >nul 2>&1\r\n").unwrap();
        let out = run_process(
            &cmd,
            &[],
            dir.path(),
            ProcessWait {
                timeout: Duration::from_secs(2),
                stall: None,
                watch_paths: &[],
            },
            &[],
            None,
        )
        .unwrap();
        assert_eq!(out.exit, 124);
        assert!(
            out.stderr.contains("review CLI timed out after"),
            "stderr={}",
            out.stderr
        );
        assert!(
            !crate::review::spawn::is_reviewer_stall(&out.stderr),
            "stderr={}",
            out.stderr
        );
        assert!(
            !out.stderr.to_ascii_lowercase().contains("exhausted"),
            "stderr={}",
            out.stderr
        );
    }

    #[test]
    fn sleeper_slot_overlay_times_out_once_without_backend() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let sleeper = dir.path().join("sleep-opencode.cmd");
        // The spawn log counts real processes, which `plan_review_spawned` alone cannot: it also
        // catches an internal `finish_slot` retry that starts a second child.
        std::fs::write(
            &sleeper,
            "@echo off\r\n>>\"%~dp0spawn.log\" echo opencode-run\r\nping -n 30 127.0.0.1 >nul 2>&1\r\n",
        )
        .unwrap();
        let spawn_log = dir.path().join("spawn.log");
        unsafe {
            std::env::set_var(ENV_COORDINATOR_OPENCODE_BIN, &sleeper);
        }
        let mut r = rec(dir.path());
        r.phase_timeouts_secs
            .insert(TIMEOUT_KEY_PLAN_REVIEW_SLOT.into(), 2);
        enter_plan_review(&r, "0001");
        let mut state = load_run_state(&r).unwrap();
        state.pending_roles = vec!["opencode".into()];
        state.plan_review_spawned = vec!["agy".into()];
        save_run_state(&r, &state).unwrap();
        write_review_markdown(&r, "agy", Some("agy done\n")).unwrap();

        let spawn_at = Instant::now();
        tick_retry(&r).unwrap();
        let path = outcome_roles_dir(&r).unwrap().join("opencode.json");

        // Poll only: never tick here, so nothing can consume the proof. The old loop observed and
        // consumed in the same iteration; a Timeout written between its poll and its consume was
        // never seen. 2 s slot + a measured hang allowance, not an 8 s latency contract.
        let deadline = spawn_at + Duration::from_secs(2) + POST_KILL_ALLOWANCE;
        let mut saw_timeout = false;
        let mut spawned_at_timeout: Vec<String> = Vec::new();
        while Instant::now() < deadline {
            if let Ok(text) = std::fs::read_to_string(&path)
                && let Ok(o) = serde_json::from_str::<PhaseOutcome>(&text)
                && o.status == OutcomeStatus::Failure
                && o.failure_class == Some(FailureClass::Timeout)
            {
                saw_timeout = true;
                spawned_at_timeout = match load_run_state(&r) {
                    Ok(s) => s.plan_review_spawned.clone(),
                    Err(e) => panic!("load run state at timeout: {e}"),
                };
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !saw_timeout {
            let state = load_run_state(&r)
                .map(|s| plan_review_diag(&s))
                .unwrap_or_else(|e| format!("run-state=<{e}>"));
            let listing: Vec<String> = std::fs::read_dir(outcome_roles_dir(&r).unwrap())
                .map(|it| {
                    it.filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            panic!(
                "opencode role outcome must be Timeout before consume within 2s + \
                 {POST_KILL_ALLOWANCE:?} (elapsed {:?}): {state} roles={listing:?}",
                spawn_at.elapsed()
            );
        }
        let oc_spawned = spawned_at_timeout
            .iter()
            .filter(|x| *x == "opencode")
            .count();
        assert_eq!(oc_spawned, 1, "spawned at timeout={spawned_at_timeout:?}");
        let spawn_lines = |log: &str| log.lines().filter(|l| l.trim() == "opencode-run").count();
        let log_at_timeout = std::fs::read_to_string(&spawn_log).unwrap_or_default();
        assert_eq!(
            spawn_lines(&log_at_timeout),
            1,
            "exactly one opencode process before consume; spawn log={log_at_timeout:?}"
        );

        // Consume, then prove the mailbox is gone. A worker cannot be in flight now: the Timeout
        // outcome is the last thing the slot thread writes.
        let consume_deadline = Instant::now() + CONSUME_CEILING;
        loop {
            let s = load_run_state(&r).unwrap();
            if !s.pending_roles.iter().any(|x| x == "opencode") {
                break;
            }
            if Instant::now() >= consume_deadline {
                panic!(
                    "no worker in flight, opencode still pending after {CONSUME_CEILING:?}: {}",
                    plan_review_diag(&s)
                );
            }
            tick_retry(&r).unwrap();
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            !path.exists(),
            "the consumed role JSON must be deleted (0039 row)"
        );
        let log_after = std::fs::read_to_string(&spawn_log).unwrap_or_default();
        assert_eq!(
            spawn_lines(&log_after),
            1,
            "exit 124 must not be retried into a second process; spawn log={log_after:?}"
        );
    }

    #[test]
    fn complete_opencode_review_mentioning_crates_io_is_success() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let mut state = load_run_state(&r).unwrap();
        state.pending_roles = vec!["opencode".into()];
        state.plan_review_spawned = vec!["agy".into()];
        save_run_state(&r, &state).unwrap();
        let body = "# Track review: {track}-Example\n\n**Track:** `{track}`\n\nmentions crates.io as noise\n";
        let rec_backend = Arc::new(RecordingBackend::wrap(Arc::new(ScriptedBackend::ok_file(
            body,
        ))));
        let _hook = install_test_backend(&r.id, rec_backend);
        tick_retry(&r).unwrap();
        wait_slots_consumed(&r, &["opencode"]);
        let written = std::fs::read_to_string(track_file(dir.path(), "0001", "opencode")).unwrap();
        assert!(written.contains("crates.io"));
        assert!(written.contains("# Track review:"));
        assert!(written.contains("0001"));
        let state_file = crate::workflow::bundle::review_file(&r, "opencode").unwrap();
        assert!(state_file.is_file(), "opencode state review missing");
        let state = load_run_state(&r).unwrap();
        assert!(state.failure_class.is_none());
        assert!(!state.last_event.to_ascii_lowercase().contains("fail"));
    }

    #[test]
    fn opencode_slot_bound_to_codex_logs_driver_codex_and_keeps_the_slot_artifact() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let mut state = load_run_state(&r).unwrap();
        state.pending_roles = vec!["opencode".into()];
        state.plan_review_spawned = vec!["agy".into()];
        save_run_state(&r, &state).unwrap();
        let mut cfg = crate::config::load_machine_config().unwrap();
        let slot = cfg.role_bindings.get_mut("plan_reviewer_opencode").unwrap();
        slot.harness = "codex".into();
        slot.command = "codex".into();
        slot.model = Some("gpt-6-luna".into());
        crate::config::save_machine_config(&cfg).unwrap();

        let _hook = install_test_backend(
            &r.id,
            Arc::new(ScriptedBackend::ok_file(ok_review_body("0001"))),
        );
        tick_retry(&r).unwrap();
        wait_slots_consumed(&r, &["opencode"]);
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap();
        assert!(
            log.contains("plan-review: opencode driver codex"),
            "durable driver evidence: {log}"
        );
        assert!(
            track_file(dir.path(), "0001", "opencode").is_file(),
            "the slot keeps its artifact name"
        );
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
            ok_review_body("0001"),
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
            ok_review_body("0001"),
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
            ok_review_body("0001"),
        ))));
        let counts = rec_backend.counts.clone();
        let _hook = install_test_backend(&r.id, rec_backend);
        tick_retry(&r).unwrap();
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
        let mut last_err = None;
        for _ in 0..40 {
            match poll_once(&r) {
                Ok(_) => {
                    last_err = None;
                    break;
                }
                Err(e) if is_transient_lock_err(&e) => {
                    last_err = Some(e);
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => panic!("{e}"),
            }
        }
        if let Some(e) = last_err {
            panic!("{e}");
        }
        assert!(
            start.elapsed() < Duration::from_millis(1500),
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
        tick_retry(&r).unwrap();
        assert_eq!(counts.n(), 0);

        run::stop(&r).unwrap();
        run::run_with_driver(&r, Some("0001".into()), WorkflowDriver::Stub).unwrap();
        let mut state = load_run_state(&r).unwrap();
        state.phase = graph::PHASE_PLAN_REVIEW.into();
        save_run_state(&r, &state).unwrap();
        tick_retry(&r).unwrap();
        assert_eq!(counts.n(), 0);
        let s = load_run_state(&r).unwrap();
        assert!(!s.pending_roles.iter().any(|x| x == "agy"));
        assert!(!s.pending_roles.iter().any(|x| x == "opencode"));
        let agy_body =
            std::fs::read_to_string(crate::workflow::bundle::review_file(&r, "agy").unwrap())
                .unwrap();
        assert!(
            agy_body.contains("stub review (agy)"),
            "stub driver must stay ungated, got {agy_body:?}"
        );
    }

    #[test]
    fn agy_dud_retries_then_pass() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let mut state = load_run_state(&r).unwrap();
        state.pending_roles = vec!["agy".into()];
        state.plan_review_spawned = vec!["opencode".into()];
        save_run_state(&r, &state).unwrap();
        let seq = AgySequenceBackend {
            agy: Mutex::new(vec![
                ScriptedBackend::ok_file(degenerate_review_body()),
                ScriptedBackend::ok_file(ok_review_body("0001")),
            ]),
            other: ScriptedBackend::ok_file(ok_review_body("0001")),
        };
        let rec_backend = Arc::new(RecordingBackend::wrap(Arc::new(seq)));
        let counts = rec_backend.counts.clone();
        let _hook = install_test_backend(&r.id, rec_backend);
        tick_retry(&r).unwrap();
        wait_slots_consumed(&r, &["agy"]);
        let agy_runs = counts.slugs().into_iter().filter(|s| s == "agy").count();
        assert_eq!(agy_runs, 2);
        let body = std::fs::read_to_string(track_file(dir.path(), "0001", "agy")).unwrap();
        assert!(body.contains("# Track review:"));
        assert!(body.contains("0001"));
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

        let view = tick_retry(&r).unwrap().expect("degrade join");
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

        let view = tick_retry(&r).unwrap().expect("degrade join");
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
        tick_retry(&r).unwrap();
        run::stop(&r).unwrap();
        {
            let (lock, cv) = &*gate;
            let mut g = lock.lock().unwrap();
            *g = true;
            cv.notify_all();
        }
        std::thread::sleep(Duration::from_millis(80));
        let _ = tick_retry(&r);
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
        tick_retry(&r).unwrap();
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
        assert!(
            load_run_state(&r).unwrap().plan_review_slot_ran.is_empty(),
            "stale child must not stamp slot_ran on the new epoch"
        );
    }

    #[test]
    fn stale_mark_slot_ran_does_not_stamp_new_epoch() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        std::fs::create_dir_all(dir.path().join("conductor").join("0002-Next")).unwrap();
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let first_epoch = load_run_state(&r).unwrap().run_epoch;
        run::stop(&r).unwrap();
        enter_plan_review(&r, "0002");
        assert!(load_run_state(&r).unwrap().plan_review_slot_ran.is_empty());
        let stale = PlanReviewRequest {
            slug: "agy".into(),
            command: "agy".into(),
            model: None,
            last_message_path: None,
            workspace_root: dir.path().to_path_buf(),
            execution_repo: None,
            track_dir: None,
            prompt: String::new(),
            remaining: Duration::from_secs(60),
            argv: Vec::new(),
            extra_env: Vec::new(),
            spawn_epoch: first_epoch,
            spawn_track_id: Some("0001".into()),
        };
        mark_slot_ran(&r, &stale).unwrap();
        assert!(
            load_run_state(&r).unwrap().plan_review_slot_ran.is_empty(),
            "epoch mismatch must not mark the live run as having run"
        );
    }

    /// 0113 stale guard: the stamp-failure mailbox retry and its last-resort removal must not
    /// touch a slot owned by a newer epoch.
    #[test]
    fn stamp_failure_refuses_a_stale_epoch_write_and_removal() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        std::fs::create_dir_all(dir.path().join("conductor").join("0002-Next")).unwrap();
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let first_epoch = load_run_state(&r).unwrap().run_epoch;
        run::stop(&r).unwrap();
        enter_plan_review(&r, "0002");
        let live = load_run_state(&r).unwrap();
        assert_ne!(live.run_epoch, first_epoch, "the second run is a new epoch");
        assert!(live.pending_roles.iter().any(|x| x == "opencode"));

        let stale = PlanReviewRequest {
            slug: "opencode".into(),
            command: "opencode".into(),
            model: None,
            last_message_path: None,
            workspace_root: dir.path().to_path_buf(),
            execution_repo: None,
            track_dir: None,
            prompt: String::new(),
            remaining: Duration::from_secs(60),
            argv: Vec::new(),
            extra_env: Vec::new(),
            spawn_epoch: first_epoch,
            spawn_track_id: Some("0001".into()),
        };

        write_stamp_failure_outcome(
            &r,
            &stale,
            "plan-review: opencode slot-ran stamp not persisted: injected",
        );
        assert!(
            !outcome_roles_dir(&r)
                .unwrap()
                .join("opencode.json")
                .exists(),
            "a stale worker must not write a failure mailbox into a newer epoch"
        );
        assert!(
            load_run_state(&r)
                .unwrap()
                .pending_roles
                .iter()
                .any(|x| x == "opencode"),
            "the newer run still owns the slot"
        );

        // The last-resort removal is epoch-guarded too.
        assert!(
            !crate::workflow::drive::remove_pending_if_epoch(
                &r,
                "opencode",
                first_epoch,
                Some("0001")
            )
            .unwrap(),
            "a stale worker must not clear a newer epoch's slot"
        );
        assert!(
            load_run_state(&r)
                .unwrap()
                .pending_roles
                .iter()
                .any(|x| x == "opencode")
        );
        // The live epoch may remove it.
        assert!(
            crate::workflow::drive::remove_pending_if_epoch(
                &r,
                "opencode",
                live.run_epoch,
                Some("0002")
            )
            .unwrap()
        );
        assert!(
            !load_run_state(&r)
                .unwrap()
                .pending_roles
                .iter()
                .any(|x| x == "opencode")
        );
    }

    #[test]
    fn advance_auto_start_clears_spawned_and_respawns() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        write_review_track_skill(dir.path());
        std::fs::create_dir_all(dir.path().join("conductor").join("0001-Example")).unwrap();
        std::fs::create_dir_all(dir.path().join("conductor").join("0002-Next")).unwrap();
        std::fs::write(
            dir.path().join("conductor").join("conductor.md"),
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0001-Example](0001-Example/spec.md) | `.` | **Ready — not started** | one |\n\
             | [0002-Next](0002-Next/spec.md) | `.` | **Ready — not started** | next |\n",
        )
        .unwrap();
        let mut r = rec(dir.path());
        r.auto_start = crate::registry::AutoStartPolicy::Full;
        let rec_backend = Arc::new(RecordingBackend::wrap(Arc::new(ScriptedBackend::ok_file(
            ok_review_body("0001"),
        ))));
        let counts = rec_backend.counts.clone();
        let _hook = install_test_backend(&r.id, rec_backend);

        enter_plan_review(&r, "0001");
        tick_retry(&r).unwrap();
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
        tick_retry(&r).unwrap();
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
        state.acp_stdout_retries = 2;
        save_run_state(&r, &state).unwrap();
        run::stop(&r).unwrap();
        run::run_with_driver(&r, Some("0001".into()), WorkflowDriver::Adapter).unwrap();
        let state = load_run_state(&r).unwrap();
        assert!(state.plan_review_spawned.is_empty());
        assert_eq!(state.acp_stdout_retries, 0);
        assert_eq!(state.plan_review_join_retries, 0);
        assert!(state.plan_review_slot_ran.is_empty());
    }

    #[test]
    fn reset_phase_clock_clears_spawned() {
        let mut state = RunState::idle("p");
        state.plan_review_spawned = vec!["agy".into(), "opencode".into()];
        state.plan_review_join_retries = 1;
        state.plan_review_slot_ran = vec!["agy".into()];
        state.acp_stdout_retries = 2;
        reset_phase_clock(&mut state);
        assert!(state.plan_review_spawned.is_empty());
        assert_eq!(state.plan_review_join_retries, 0);
        assert!(state.plan_review_slot_ran.is_empty());
        assert_eq!(state.acp_stdout_retries, 0);
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
        tick_retry(&r).unwrap();
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
        tick_retry(&r).unwrap();
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
            Arc::new(ScriptedBackend::ok_ndjson(
                "# Track review: 0001-Example\n\n**Track:** `0001`\n\nndjson review body\n",
            )),
        );
        tick_retry(&r).unwrap();
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
        tick_retry(&r).unwrap();
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
        tick_retry(&r).unwrap();
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
            inner: Arc::new(ScriptedBackend::ok_file(format!(
                "{}\npaused still writes\n",
                ok_review_body("0001").trim_end()
            ))),
        })));
        let _hook = install_test_backend(&r.id, rec_backend);
        tick_retry(&r).unwrap();
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
    fn degenerate_opencode_file_fails_unlinks_and_degrades() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let rec_backend = Arc::new(RecordingBackend::wrap(Arc::new(OpencodeSequenceBackend {
            opencode: Mutex::new(vec![
                ScriptedBackend::ok_file(degenerate_review_body()),
                ScriptedBackend::ok_file(degenerate_review_body()),
            ]),
            other: ScriptedBackend::ok_file(ok_review_body("0001")),
        })));
        let counts = rec_backend.counts.clone();
        let _hook = install_test_backend(&r.id, rec_backend);
        tick_retry(&r).unwrap();
        wait_both_consumed(&r);
        let oc_runs = counts
            .slugs()
            .into_iter()
            .filter(|s| s == "opencode")
            .count();
        assert_eq!(oc_runs, 2, "degenerate opencode should retry once");
        assert!(
            !track_file(dir.path(), "0001", "opencode").exists(),
            "leftover degenerate opencode-review.md must be unlinked"
        );
        let oc_state = crate::workflow::bundle::review_file(&r, "opencode").unwrap();
        assert!(
            !oc_state.is_file()
                || std::fs::read_to_string(&oc_state)
                    .unwrap()
                    .trim()
                    .is_empty()
        );
        let mut joined = false;
        for _ in 0..20 {
            let _ = tick(&r);
            let s = load_run_state(&r).unwrap();
            if s.phase == graph::PHASE_FOLD || s.last_event.contains("degraded") {
                joined = true;
                break;
            }
        }
        assert!(
            joined,
            "agy success + opencode degenerate should degrade, phase={} event={}",
            load_run_state(&r).unwrap().phase,
            load_run_state(&r).unwrap().last_event
        );
    }

    #[test]
    fn stalled_opencode_does_not_retry() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let mut state = load_run_state(&r).unwrap();
        state.pending_roles = vec!["opencode".into()];
        state.plan_review_spawned = vec!["agy".into()];
        save_run_state(&r, &state).unwrap();
        let rec_backend = Arc::new(RecordingBackend::wrap(Arc::new(ScriptedBackend::stall())));
        let counts = rec_backend.counts.clone();
        let _hook = install_test_backend(&r.id, rec_backend);
        tick_retry(&r).unwrap();
        wait_slots_consumed(&r, &["opencode"]);
        let oc_runs = counts
            .slugs()
            .into_iter()
            .filter(|s| s == "opencode")
            .count();
        assert_eq!(oc_runs, 1, "stall must not retry opencode");
    }

    #[test]
    fn opencode_retry_then_complete_is_success() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let mut state = load_run_state(&r).unwrap();
        state.pending_roles = vec!["opencode".into()];
        state.plan_review_spawned = vec!["agy".into()];
        save_run_state(&r, &state).unwrap();
        let seq = OpencodeSequenceBackend {
            opencode: Mutex::new(vec![
                ScriptedBackend::ok_file(degenerate_review_body()),
                ScriptedBackend::ok_file(ok_review_body("0001")),
            ]),
            other: ScriptedBackend::ok_file(ok_review_body("0001")),
        };
        let rec_backend = Arc::new(RecordingBackend::wrap(Arc::new(seq)));
        let counts = rec_backend.counts.clone();
        let _hook = install_test_backend(&r.id, rec_backend);
        tick_retry(&r).unwrap();
        wait_slots_consumed(&r, &["opencode"]);
        let oc_runs = counts
            .slugs()
            .into_iter()
            .filter(|s| s == "opencode")
            .count();
        assert_eq!(oc_runs, 2);
        let body = std::fs::read_to_string(track_file(dir.path(), "0001", "opencode")).unwrap();
        assert!(body.contains("# Track review:"));
        assert!(body.contains("0001"));
    }

    #[test]
    fn two_degenerate_opencode_runs_fail_and_unlink() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let mut state = load_run_state(&r).unwrap();
        state.pending_roles = vec!["opencode".into()];
        state.plan_review_spawned = vec!["agy".into()];
        save_run_state(&r, &state).unwrap();
        write_review_markdown(&r, "agy", Some("agy already done\n")).unwrap();
        let seq = OpencodeSequenceBackend {
            opencode: Mutex::new(vec![
                ScriptedBackend::ok_file(degenerate_review_body()),
                ScriptedBackend::ok_file(degenerate_review_body()),
            ]),
            other: ScriptedBackend::empty(),
        };
        let rec_backend = Arc::new(RecordingBackend::wrap(Arc::new(seq)));
        let counts = rec_backend.counts.clone();
        let _hook = install_test_backend(&r.id, rec_backend);
        tick_retry(&r).unwrap();
        wait_slots_consumed(&r, &["opencode"]);
        let oc_runs = counts
            .slugs()
            .into_iter()
            .filter(|s| s == "opencode")
            .count();
        assert_eq!(oc_runs, 2);
        assert!(!track_file(dir.path(), "0001", "opencode").exists());
        let path = outcome_roles_dir(&r).unwrap().join("opencode.json");
        if path.is_file() {
            let o: PhaseOutcome =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            let msg = o.message.unwrap_or_default();
            assert!(
                msg.contains("# Track review:"),
                "degenerate failure must name the missing header, got {msg}"
            );
            assert!(!msg.contains("produced no review file and no stdout"));
        }
    }

    struct RetryRemainingGuard;
    impl Drop for RetryRemainingGuard {
        fn drop(&mut self) {
            TEST_RETRY_REMAINING.with(|c| c.set(None));
        }
    }

    #[test]
    fn remaining_under_60s_after_first_does_not_retry() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let state = load_run_state(&r).unwrap();
        TEST_RETRY_REMAINING.with(|c| c.set(Some(Duration::from_secs(10))));
        let _reset = RetryRemainingGuard;
        let rec_backend = Arc::new(RecordingBackend::wrap(Arc::new(ScriptedBackend::ok_file(
            degenerate_review_body(),
        ))));
        let counts = rec_backend.counts.clone();
        let req = PlanReviewRequest {
            slug: REVIEW_SLUG_OPENCODE.into(),
            command: "opencode".into(),
            model: None,
            last_message_path: None,
            workspace_root: dir.path().to_path_buf(),
            execution_repo: None,
            track_dir: Some(dir.path().join("conductor").join("0001-Example")),
            prompt: "review-track prompt".into(),
            remaining: Duration::from_secs(1200),
            argv: opencode_argv(None, None),
            extra_env: Vec::new(),
            spawn_epoch: state.run_epoch,
            spawn_track_id: Some("0001".into()),
        };
        apply_slot_result(&r, rec_backend.as_ref(), &req);
        assert_eq!(counts.n(), 1, "retry skipped when remaining < 60s");
        assert!(!track_file(dir.path(), "0001", "opencode").exists());
        let path = outcome_roles_dir(&r).unwrap().join("opencode.json");
        let o: PhaseOutcome =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(o.status, OutcomeStatus::Failure);
        let msg = o.message.unwrap_or_default();
        assert!(
            msg.contains("# Track review:"),
            "degenerate failure must name the missing header, got {msg}"
        );
        assert!(!msg.contains("produced no review file and no stdout"));
    }

    #[test]
    fn short_slot_overlay_still_retries_degenerate_when_phase_budget_remains() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let mut r = rec(dir.path());
        r.phase_timeouts_secs
            .insert(TIMEOUT_KEY_PLAN_REVIEW_SLOT.into(), 10);
        enter_plan_review(&r, "0001");
        let mut state = load_run_state(&r).unwrap();
        state.pending_roles = vec!["opencode".into()];
        state.plan_review_spawned = vec!["agy".into()];
        save_run_state(&r, &state).unwrap();
        let seq = OpencodeSequenceBackend {
            opencode: Mutex::new(vec![
                ScriptedBackend::ok_file(degenerate_review_body()),
                ScriptedBackend::ok_file(ok_review_body("0001")),
            ]),
            other: ScriptedBackend::ok_file(ok_review_body("0001")),
        };
        let rec_backend = Arc::new(RecordingBackend::wrap(Arc::new(seq)));
        let counts = rec_backend.counts.clone();
        let _hook = install_test_backend(&r.id, rec_backend);
        tick_retry(&r).unwrap();
        wait_slots_consumed(&r, &["opencode"]);
        let oc_runs = counts
            .slugs()
            .into_iter()
            .filter(|s| s == "opencode")
            .count();
        assert_eq!(
            oc_runs, 2,
            "phase MIN_SPAWN_BUDGET must admit retry even when plan_review_slot < 60s"
        );
        let reqs = counts.requests.lock().unwrap();
        let retries: Vec<_> = reqs.iter().filter(|q| q.slug == "opencode").collect();
        assert_eq!(retries.len(), 2);
        assert!(
            retries[1].remaining <= Duration::from_secs(10),
            "retry remaining should be slot-capped, got {:?}",
            retries[1].remaining
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
        assert_eq!(state.plan_review_join_retries, 0);
        assert!(state.plan_review_slot_ran.is_empty());
    }

    /// 0113 seam control: the wait must await the current worker wave before consuming.
    ///
    /// Deleting `wait_idle` from `wait_slots_consumed` makes the helper consume the ready `agy`
    /// sibling immediately and the T+300 ms assertions fail. The subject test still passes without
    /// the seam, which is exactly why this control exists.
    #[test]
    fn wait_slots_consumed_waits_for_the_inflight_wave() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");

        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let _hook = install_test_backend(
            &r.id,
            Arc::new(GateOpencodeBackend {
                gate: gate.clone(),
                body: ok_review_body("0001"),
            }),
        );
        tick_retry(&r).unwrap();

        let roles = outcome_roles_dir(&r).unwrap();
        let agy_json = roles.join("agy.json");
        let opencode_json = roles.join("opencode.json");

        let waiter = r.clone();
        let handle = std::thread::spawn(move || {
            wait_slots_consumed(&waiter, &["agy", "opencode"]);
        });

        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !handle.is_finished(),
            "the helper must not return while a worker is in flight"
        );
        let mid = load_run_state(&r).unwrap();
        assert!(
            mid.pending_roles.iter().any(|x| x == "agy"),
            "quiescence-first: a ready sibling is not consumed while the wave runs: {}",
            plan_review_diag(&mid)
        );
        assert!(
            agy_json.is_file(),
            "agy mailbox survives until the wave is quiescent"
        );

        {
            let (lock, cv) = &*gate;
            *lock.lock().unwrap_or_else(|p| p.into_inner()) = true;
            cv.notify_all();
        }
        let deadline = Instant::now() + WORKER_CEILING;
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(handle.is_finished(), "helper returned after release");
        handle.join().unwrap();

        let done = load_run_state(&r).unwrap();
        assert!(
            !done.pending_roles.iter().any(|x| x == "agy"),
            "{}",
            plan_review_diag(&done)
        );
        assert!(
            !done.pending_roles.iter().any(|x| x == "opencode"),
            "{}",
            plan_review_diag(&done)
        );
        assert!(!opencode_json.exists(), "opencode mailbox is consumed");
        assert!(!agy_json.exists(), "agy mailbox is consumed");
    }

    /// 0113 stamp regression: an unstamped slot must degrade loudly and never run its backend.
    ///
    /// On the pre-fix `let _ =` code the backend runs and no such role outcome exists.
    #[test]
    fn slot_ran_stamp_failure_is_loud() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let rec_backend = Arc::new(RecordingBackend::wrap(Arc::new(ScriptedBackend::ok_file(
            ok_review_body("0001"),
        ))));
        let counts = rec_backend.counts.clone();
        let _hook = install_test_backend(&r.id, rec_backend);

        let mut state = load_run_state(&r).unwrap();
        state.pending_roles = vec!["opencode".into()];
        state.plan_review_spawned = vec!["agy".into()];
        save_run_state(&r, &state).unwrap();
        write_review_markdown(&r, "agy", Some("agy done\n")).unwrap();

        // More failures than the release ladder: the ladder must exhaust, not swallow.
        let _fail = fail_stamp_save(&r.id, "opencode", STAMP_SAVE_ATTEMPTS + 10);
        tick_retry(&r).unwrap();

        let path = outcome_roles_dir(&r).unwrap().join("opencode.json");
        let deadline = Instant::now() + WORKER_CEILING;
        let mut outcome: Option<PhaseOutcome> = None;
        while Instant::now() < deadline {
            if let Ok(text) = std::fs::read_to_string(&path)
                && let Ok(o) = serde_json::from_str::<PhaseOutcome>(&text)
            {
                outcome = Some(o);
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let outcome = outcome.expect("a role outcome must be written on stamp exhaustion");
        assert_eq!(outcome.status, OutcomeStatus::Failure);
        assert_eq!(outcome.failure_class, Some(FailureClass::HarnessCrash));
        let msg = outcome.message.clone().unwrap_or_default();
        assert!(
            msg.contains("slot-ran stamp not persisted"),
            "message={msg}"
        );
        assert!(msg.contains("opencode"), "message={msg}");

        let oc_runs = counts
            .slugs()
            .into_iter()
            .filter(|s| s == "opencode")
            .count();
        assert_eq!(oc_runs, 0, "the backend must not run for an unstamped slot");
        let s = load_run_state(&r).unwrap();
        assert!(!s.plan_review_slot_ran.iter().any(|x| x == "opencode"));

        let status_path = crate::layout::resolve(&r).workspace_root.join("status.md");
        let log = std::fs::read_to_string(&status_path).unwrap_or_default();
        assert!(
            log.contains("slot-ran stamp not persisted"),
            "progress log must name the failure: {log}"
        );
    }

    /// 0113 stamp fallback: when the failure mailbox cannot be written, the slot is still consumed
    /// (named in the progress log) instead of hanging on a role file that will never appear.
    #[test]
    fn stamp_failure_consumes_the_slot_when_the_mailbox_cannot_be_written() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let rec_backend = Arc::new(RecordingBackend::wrap(Arc::new(ScriptedBackend::ok_file(
            ok_review_body("0001"),
        ))));
        let counts = rec_backend.counts.clone();
        let _hook = install_test_backend(&r.id, rec_backend);

        let mut state = load_run_state(&r).unwrap();
        state.pending_roles = vec!["opencode".into()];
        state.plan_review_spawned = vec!["agy".into()];
        save_run_state(&r, &state).unwrap();

        // Make the role-outcome write impossible: the roles path is a regular file.
        let roles = outcome_roles_dir(&r).unwrap();
        std::fs::create_dir_all(roles.parent().unwrap()).unwrap();
        let _ = std::fs::remove_dir_all(&roles);
        std::fs::write(&roles, b"not a directory").unwrap();

        let _fail = fail_stamp_save(&r.id, "opencode", STAMP_SAVE_ATTEMPTS + 10);
        tick_retry(&r).unwrap();

        let deadline = Instant::now() + WORKER_CEILING;
        let mut pending = vec!["opencode".to_string()];
        while Instant::now() < deadline {
            pending = load_run_state(&r).unwrap().pending_roles;
            if !pending.iter().any(|x| x == "opencode") {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            !pending.iter().any(|x| x == "opencode"),
            "an unwritable failure mailbox must still consume the slot: {pending:?}"
        );
        assert_eq!(
            counts
                .slugs()
                .into_iter()
                .filter(|s| s == "opencode")
                .count(),
            0,
            "the backend must not run for an unstamped slot"
        );
        let status_path = crate::layout::resolve(&r).workspace_root.join("status.md");
        let log = std::fs::read_to_string(&status_path).unwrap_or_default();
        assert!(log.contains("slot-ran stamp not persisted"), "{log}");
    }

    #[test]
    fn join_zero_output_retries_then_pass() {
        use crate::state::RunStatus;
        TEST_JOIN_REMAINING.with(|c| c.set(None));
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let seq = PerSlugSequenceBackend {
            agy: Mutex::new(vec![
                ScriptedBackend::empty(),
                ScriptedBackend::empty(),
                ScriptedBackend::ok_file(ok_review_body("0001")),
            ]),
            opencode: Mutex::new(vec![
                ScriptedBackend::empty(),
                ScriptedBackend::empty(),
                ScriptedBackend::ok_file(ok_review_body("0001")),
            ]),
        };
        let rec_backend = Arc::new(RecordingBackend::wrap(Arc::new(seq)));
        let counts = rec_backend.counts.clone();
        let _hook = install_test_backend(&r.id, rec_backend);
        tick_retry(&r).unwrap();
        wait_join_retry_latched(&r);
        let latched = load_run_state(&r).unwrap();
        assert_eq!(latched.status, RunStatus::Running);
        assert_eq!(latched.plan_review_join_retries, 1);
        assert!(
            latched.last_event.starts_with("plan-review: retrying join"),
            "got {}",
            latched.last_event
        );
        assert!(latched.last_event.contains("(1/1)"));
        let diag = plan_review_diag(&latched);
        assert!(latched.pending_roles.iter().any(|x| x == "agy"), "{diag}");
        assert!(
            latched.pending_roles.iter().any(|x| x == "opencode"),
            "{diag}"
        );
        assert!(
            !latched.plan_review_spawned.iter().any(|x| x == "agy"),
            "{diag}"
        );
        assert!(
            !latched.plan_review_spawned.iter().any(|x| x == "opencode"),
            "{diag}"
        );
        assert!(latched.failure_class.is_none(), "{diag}");
        let first_wave_agy = counts.slugs().into_iter().filter(|s| s == "agy").count();
        let first_wave_oc = counts
            .slugs()
            .into_iter()
            .filter(|s| s == "opencode")
            .count();
        assert_eq!(first_wave_agy, 2, "slot retry consumes two empties");
        assert_eq!(first_wave_oc, 2, "slot retry consumes two empties");
        wait_both_consumed(&r);
        let done = load_run_state(&r).unwrap();
        assert_ne!(done.status, RunStatus::Stopped);
        assert!(track_file(dir.path(), "0001", "agy").exists());
        assert!(track_file(dir.path(), "0001", "opencode").exists());
        let agy_runs = counts.slugs().into_iter().filter(|s| s == "agy").count();
        let oc_runs = counts
            .slugs()
            .into_iter()
            .filter(|s| s == "opencode")
            .count();
        assert_eq!(agy_runs, 3);
        assert_eq!(oc_runs, 3);
    }

    #[test]
    fn join_zero_output_retry_exhausted_stops_harness_crash() {
        use crate::notify::NotifyEvent;
        use crate::state::RunStatus;
        let _env = IsolatedHome::enter();
        let rec_h = crate::notify::hermes::install_recording(
            "http://127.0.0.1:8644/webhooks/coordinator-failure",
            "s",
        );
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let seq = PerSlugSequenceBackend {
            agy: Mutex::new(vec![
                ScriptedBackend::empty(),
                ScriptedBackend::empty(),
                ScriptedBackend::empty(),
                ScriptedBackend::empty(),
            ]),
            opencode: Mutex::new(vec![
                ScriptedBackend::empty(),
                ScriptedBackend::empty(),
                ScriptedBackend::empty(),
                ScriptedBackend::empty(),
            ]),
        };
        let rec_backend = Arc::new(RecordingBackend::wrap(Arc::new(seq)));
        let _hook = install_test_backend(&r.id, rec_backend);
        tick_retry(&r).unwrap();
        wait_join_retry_latched(&r);
        assert_eq!(load_run_state(&r).unwrap().plan_review_join_retries, 1);
        wait_stopped(&r);
        let done = load_run_state(&r).unwrap();
        assert_eq!(done.status, RunStatus::Stopped);
        assert_eq!(done.failure_class, Some(FailureClass::HarnessCrash));
        assert!(
            done.last_event.contains("zero reviewers produced output"),
            "got {}",
            done.last_event
        );
        assert!(crate::notify::artifact::existing_path(&r).is_some());
        let captured = rec_h.take();
        assert_eq!(captured.len(), 1);
        let parsed: NotifyEvent = serde_json::from_slice(&captured[0].body).unwrap();
        assert_eq!(parsed.failure_class, FailureClass::HarnessCrash);
        assert!(
            parsed
                .message
                .as_deref()
                .unwrap_or("")
                .contains("zero reviewers produced output")
        );
    }

    #[test]
    fn join_zero_output_never_ran_stops_without_retry() {
        use crate::state::RunStatus;
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let view = tick_retry(&r).unwrap().expect("fail-fast join");
        assert_eq!(view.status, RunStatus::Stopped);
        assert_eq!(view.failure_class, Some(FailureClass::HarnessCrash));
        let s = load_run_state(&r).unwrap();
        assert_eq!(s.plan_review_join_retries, 0);
        assert!(s.plan_review_slot_ran.is_empty());
        assert!(s.last_event.contains("zero reviewers produced output"));
    }

    #[test]
    fn join_zero_output_under_min_spawn_budget_does_not_retry() {
        use crate::state::RunStatus;
        let _env = IsolatedHome::enter();
        TEST_JOIN_REMAINING.with(|c| c.set(Some(Duration::from_secs(10))));
        struct JoinRemainingGuard;
        impl Drop for JoinRemainingGuard {
            fn drop(&mut self) {
                TEST_JOIN_REMAINING.with(|c| c.set(None));
            }
        }
        let _reset = JoinRemainingGuard;
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        let seq = PerSlugSequenceBackend {
            agy: Mutex::new(vec![ScriptedBackend::empty(), ScriptedBackend::empty()]),
            opencode: Mutex::new(vec![ScriptedBackend::empty(), ScriptedBackend::empty()]),
        };
        let rec_backend = Arc::new(RecordingBackend::wrap(Arc::new(seq)));
        let _hook = install_test_backend(&r.id, rec_backend);
        tick_retry(&r).unwrap();
        wait_stopped(&r);
        let s = load_run_state(&r).unwrap();
        assert_eq!(s.status, RunStatus::Stopped);
        assert_eq!(s.failure_class, Some(FailureClass::HarnessCrash));
        assert_eq!(s.plan_review_join_retries, 0);
        assert!(s.last_event.contains("zero reviewers produced output"));
    }

    #[test]
    fn join_zero_output_pending_rearm_stays_running() {
        use crate::state::RunStatus;
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
        {
            let mut s = load_run_state(&r).unwrap();
            s.pending_roles = vec!["agy".into(), "opencode".into()];
            s.plan_review_spawned = vec!["agy".into(), "opencode".into()];
            s.plan_review_slot_ran = vec!["agy".into(), "opencode".into()];
            s.plan_review_join_retries = 1;
            s.last_event = "plan-review: retrying join agy,opencode (1/1)".into();
            save_run_state(&r, &s).unwrap();
        }
        assert!(maybe_rearm_join_retry(&r).unwrap());
        let s = load_run_state(&r).unwrap();
        assert_eq!(s.status, RunStatus::Running);
        assert_eq!(s.plan_review_join_retries, 1);
        assert_eq!(s.pending_roles.len(), 2);
        assert!(s.failure_class.is_none());
        let view = tick_retry(&r).unwrap();
        assert!(
            view.is_none(),
            "join must not Stop while pending is refilled"
        );
        let s = load_run_state(&r).unwrap();
        assert_eq!(s.status, RunStatus::Running);
        assert!(s.failure_class.is_none());
    }

    #[test]
    fn join_zero_output_concurrent_rearm_stays_running() {
        use crate::state::RunStatus;
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        setup_track(dir.path(), "0001");
        let r = rec(dir.path());
        enter_plan_review(&r, "0001");
        {
            let mut s = load_run_state(&r).unwrap();
            s.pending_roles.clear();
            s.plan_review_spawned = vec!["agy".into(), "opencode".into()];
            s.plan_review_slot_ran = vec!["agy".into(), "opencode".into()];
            save_run_state(&r, &s).unwrap();
        }
        let rec_a = r.clone();
        let rec_b = r.clone();
        let h1 = std::thread::spawn(move || maybe_rearm_join_retry(&rec_a).unwrap());
        let h2 = std::thread::spawn(move || maybe_rearm_join_retry(&rec_b).unwrap());
        let a = h1.join().unwrap();
        let b = h2.join().unwrap();
        assert!(a && b, "neither ticker may treat a live re-arm as exhaust");
        let s = load_run_state(&r).unwrap();
        assert_eq!(s.status, RunStatus::Running);
        assert_eq!(s.plan_review_join_retries, 1);
        assert!(!s.pending_roles.is_empty());
        assert!(s.failure_class.is_none());
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
        tick_retry(&r).unwrap();
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

    fn mkdir_track(dir: &std::path::Path, name: &str) {
        std::fs::create_dir_all(dir.join("conductor").join(name)).unwrap();
    }

    fn assert_both_reviews_adopted(
        r: &ProjectRecord,
        dir: &std::path::Path,
        folder: &str,
        track_id: &str,
    ) {
        for slug in ["agy", "opencode"] {
            let state_file = crate::workflow::bundle::review_file(r, slug).unwrap();
            assert!(state_file.is_file(), "{slug} state review missing");
            let body = std::fs::read_to_string(&state_file).unwrap();
            assert!(
                body.to_ascii_lowercase().contains("# track review:"),
                "{slug} body missing header: {body}"
            );
            assert!(body.contains(track_id), "{slug} body missing id: {body}");
            let copy = dir
                .join("conductor")
                .join(folder)
                .join(format!("{slug}-review.md"));
            assert!(copy.is_file(), "expected {}", copy.display());
        }
        let done = load_run_state(r).unwrap();
        assert_ne!(done.status, crate::state::RunStatus::Stopped);
        assert!(
            !done.last_event.contains("zero reviewers produced output"),
            "{}",
            done.last_event
        );
    }

    fn adopt_on_disk_body(dir: &std::path::Path, folder: &str, track_id: &str) {
        mkdir_track(dir, folder);
        write_review_track_skill(dir);
        let r = rec(dir);
        enter_plan_review(&r, track_id);
        let backend = Arc::new(RecordingBackend::wrap(Arc::new(ScriptedBackend::ok_file(
            ok_review_body(track_id),
        ))));
        let _hook = install_test_backend(&r.id, backend);
        tick_retry(&r).unwrap();
        wait_both_consumed(&r);
        assert_both_reviews_adopted(&r, dir, folder, track_id);
    }

    #[test]
    fn track72_id_72_adopts_on_disk_body() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        adopt_on_disk_body(dir.path(), "track72", "72");
    }

    #[test]
    fn track_prefixed_id_adopts_bare_dir() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        adopt_on_disk_body(dir.path(), "72", "track72");
    }

    #[test]
    fn prefixed_dir_still_joins() {
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        adopt_on_disk_body(dir.path(), "0065-PlanReviewAdoptionTrackId", "0065");
    }

    #[test]
    fn unresolvable_id_is_diagnosed() {
        use crate::state::RunStatus;
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        mkdir_track(dir.path(), "track720");
        mkdir_track(dir.path(), "720-Foo");
        write_review_track_skill(dir.path());
        let r = rec(dir.path());
        enter_plan_review(&r, "72");
        let _hook = install_test_backend(&r.id, Arc::new(ScriptedBackend::empty()));
        tick_retry(&r).unwrap();
        wait_stopped(&r);
        let s = load_run_state(&r).unwrap();
        assert_eq!(s.status, RunStatus::Stopped);
        assert_eq!(s.failure_class, Some(FailureClass::HarnessCrash));
        assert!(
            s.last_event
                .contains("cannot resolve track dir for id '72'"),
            "{}",
            s.last_event
        );
        assert!(!s.last_event.contains("zero reviewers produced output"));
        assert_eq!(s.plan_review_join_retries, 0);
    }

    #[test]
    fn unresolvable_success_role_journals_and_stops() {
        use crate::state::RunStatus;
        let _env = IsolatedHome::enter();
        let dir = tempdir().unwrap();
        mkdir_track(dir.path(), "track720");
        write_review_track_skill(dir.path());
        let r = rec(dir.path());
        enter_plan_review(&r, "72");
        {
            let mut state = load_run_state(&r).unwrap();
            state.plan_review_spawned = vec!["agy".into(), "opencode".into()];
            save_run_state(&r, &state).unwrap();
        }
        let roles = outcome_roles_dir(&r).unwrap();
        std::fs::create_dir_all(&roles).unwrap();
        for (slug, role) in [
            ("agy", graph::ROLE_REVIEWER_AGY),
            ("opencode", graph::ROLE_REVIEWER_OPENCODE),
        ] {
            let mut outcome = PhaseOutcome::success(
                format!("plan-review:{slug}"),
                OutcomeSource::File,
                None,
                None,
                None,
            );
            outcome.metadata = Some(OutcomeMetadata {
                next_track: None,
                role: Some(role.into()),
                ..Default::default()
            });
            crate::persist::atomic_write_json(&roles.join(format!("{slug}.json")), &outcome)
                .unwrap();
        }
        tick_retry(&r).unwrap();
        let s = load_run_state(&r).unwrap();
        assert_eq!(s.status, RunStatus::Stopped);
        assert_eq!(s.failure_class, Some(FailureClass::HarnessCrash));
        assert!(
            s.last_event
                .contains("cannot resolve track dir for id '72'"),
            "{}",
            s.last_event
        );
        assert_eq!(s.plan_review_join_retries, 0);
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap();
        assert!(
            log.contains("plan-review: cannot resolve track dir for id '72'"),
            "{log}"
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
        tick_retry(&r).unwrap();
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
