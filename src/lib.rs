//! Coordinator Control Plane library (CLI + localhost API share these ops).
//!
//! Phase Outcome File (schema v1) is the portable done-contract; apply + wait/timeout
//! complete phases without chat vibes (track **0005**). Grok ACP adapter +
//! project-keyed session pool: track **0007**. Canonical workflow runner: track **0008**.
//! Failure Artifact + toast + Notify Adapter: track **0009**.

pub mod api;
pub mod cli;
pub mod config;
pub mod error;
pub mod harness;
pub mod layout;
pub mod notify;
pub mod outcome;
pub mod persist;
pub mod registry;
pub mod run;
pub mod scan;
pub mod server;
pub mod state;
pub mod watch;
pub mod workflow;

pub use error::{CoordinatorError, Result};
