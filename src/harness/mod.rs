//! Grok harness adapter, session pool, and role-binding helpers (track **0007**).
//! Abort/recycle of a wedged ACP Prompt: track **0027**. Recycle gate is sidecar `tool_in_flight` (track **0035**).
//! Harness preflight (`doctor`): track **0028**.
//! Windows `terminal/create` shell fallback + spawn-fail `HarnessCrash`: track **0032**.
//! TerminalHub JSONL command journal + `loop_suspect`: track **0034**.
//! Product-side self-reap of stale terminal-run harness holders: track **0107**.

pub mod abort;
pub mod capabilities;
pub mod grok;
pub mod journal;
pub mod no_change;
pub mod pool;
pub mod preflight;
pub mod reap;
pub mod roles;
pub mod roles_swap;
pub mod terminal;

pub use grok::{
    CancelHandle, ENV_ACP_HARNESS, ENV_CURSOR_BIN, ENV_GROK_BIN, ENV_GROK_LIVE, GrokSession,
    PromptResult, grok_agent_argv, is_acp_session_harness, map_failure_class, resolve_command,
    resolve_grok_binary,
};
pub use pool::{
    GrokHarnessStatus, HarnessPromptView, HarnessStatusBundle, SessionPool, global_pool,
    persist_path, status_bundle_sync,
};
pub use roles::{
    load_role_bindings, resolve_grok_command, resolve_phase_binary, resolve_phase_binding,
    resolve_phase_role_key,
};

use std::path::PathBuf;

use crate::error::Result;
use crate::layout;
use crate::registry::ProjectRecord;

/// Cwd for Grok. A singular run uses the active epoch worktree when that
/// directory exists, otherwise `execution_repo`, otherwise `workspace_root`.
/// An open book with no scope returns `ambiguous run` and does not fall back.
pub fn grok_cwd(record: &ProjectRecord) -> Result<PathBuf> {
    match crate::state::load_run_book(record)? {
        crate::state::RunBook::Many(_) => {
            let Some(epoch) = crate::state::active_run_epoch() else {
                return Err(crate::state::ambiguous_run_error());
            };
            let dir = crate::worktree::epoch_dir(record, epoch)?;
            if !dir.is_dir() {
                return Err(crate::error::CoordinatorError::Message(format!(
                    "worktree for epoch {epoch} is missing"
                )));
            }
            Ok(dir)
        }
        crate::state::RunBook::One(_) => {
            if let Some(dir) = crate::worktree::active_epoch_dir(record) {
                return Ok(dir);
            }
            let paths = layout::resolve(record);
            Ok(paths.execution_repo.unwrap_or(paths.workspace_root))
        }
    }
}

/// Start (or reuse) a Grok session for the project.
pub async fn start(
    project: Option<&str>,
    in_process: bool,
    infer_cwd: bool,
) -> Result<GrokHarnessStatus> {
    pool::start(project, in_process, infer_cwd).await
}

/// Start a new session with a phase-resolved binary, optional model, and ACP harness slug.
pub async fn start_with_bin(
    project: Option<&str>,
    in_process: bool,
    bin: PathBuf,
    model: Option<String>,
    infer_cwd: bool,
    harness: &str,
    epoch: Option<u64>,
) -> Result<GrokHarnessStatus> {
    pool::start_with_bin(project, in_process, bin, model, infer_cwd, harness, epoch).await
}

pub async fn prompt(
    project: Option<&str>,
    text: String,
    infer_cwd: bool,
) -> Result<HarnessPromptView> {
    pool::prompt(project, text, infer_cwd).await
}

/// Adapter-only prepared turn. Carries the frozen track target into the pool.
pub async fn prompt_prepared(
    project: Option<&str>,
    text: String,
    infer_cwd: bool,
    prepared: crate::workflow::execution_repo::PreparedInjection,
) -> Result<HarnessPromptView> {
    pool::prompt_prepared(project, text, infer_cwd, prepared).await
}

pub async fn compact(project: Option<&str>, infer_cwd: bool) -> Result<HarnessPromptView> {
    pool::compact(project, infer_cwd).await
}

pub async fn grok_status(project: Option<&str>, infer_cwd: bool) -> Result<GrokHarnessStatus> {
    pool::status(project, infer_cwd).await
}

pub async fn shutdown(project: Option<&str>, infer_cwd: bool) -> Result<GrokHarnessStatus> {
    pool::shutdown(project, infer_cwd).await
}

/// Detached ACP holder loop (hidden CLI).
pub async fn hold(project: Option<&str>, epoch: Option<u64>) -> Result<()> {
    pool::hold_loop(project, epoch).await
}
