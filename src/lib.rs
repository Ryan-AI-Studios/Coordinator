//! Coordinator Control Plane library (CLI + localhost API share these ops).
//!
//! Stub-phase runs never auto-advance; timeouts and Phase Outcomes are track **0005+**.

pub mod api;
pub mod cli;
pub mod config;
pub mod error;
pub mod persist;
pub mod registry;
pub mod run;
pub mod server;
pub mod state;

pub use error::{CoordinatorError, Result};
