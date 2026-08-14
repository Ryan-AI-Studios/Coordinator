//! Grok harness adapter, session pool, and role-binding helpers (track **0007**).
//! Abort/recycle of a wedged ACP Prompt: track **0027**.

pub mod abort;
pub mod grok;
pub mod pool;
pub mod roles;

pub use grok::{
    CancelHandle, ENV_GROK_BIN, ENV_GROK_LIVE, GrokSession, PromptResult, map_failure_class,
    resolve_command, resolve_grok_binary,
};
pub use pool::{
    GrokHarnessStatus, HarnessPromptView, HarnessStatusBundle, SessionPool, global_pool,
    persist_path, status_bundle_sync,
};
pub use roles::{load_role_bindings, resolve_grok_command};

use std::path::PathBuf;

use crate::error::Result;
use crate::layout;
use crate::registry::ProjectRecord;

/// Cwd for Grok: `execution_repo` if set, else `workspace_root`.
pub fn grok_cwd(record: &ProjectRecord) -> PathBuf {
    let paths = layout::resolve(record);
    paths.execution_repo.unwrap_or(paths.workspace_root)
}

/// Start (or reuse) a Grok session for the project.
pub async fn start(project: Option<&str>, in_process: bool) -> Result<GrokHarnessStatus> {
    pool::start(project, in_process).await
}

pub async fn prompt(project: Option<&str>, text: String) -> Result<HarnessPromptView> {
    pool::prompt(project, text).await
}

pub async fn compact(project: Option<&str>) -> Result<HarnessPromptView> {
    pool::compact(project).await
}

pub async fn grok_status(project: Option<&str>) -> Result<GrokHarnessStatus> {
    pool::status(project).await
}

pub async fn shutdown(project: Option<&str>) -> Result<GrokHarnessStatus> {
    pool::shutdown(project).await
}

/// Detached ACP holder loop (hidden CLI).
pub async fn hold(project: Option<&str>) -> Result<()> {
    pool::hold_loop(project).await
}
