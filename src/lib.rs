//! Coordinator Control Plane library (CLI + localhost API share these ops).
//!
//! Phase Outcome File (schema v1) is the portable done-contract; apply + wait/timeout
//! complete stub phases without chat vibes (track **0005**).

pub mod api;
pub mod cli;
pub mod config;
pub mod error;
pub mod outcome;
pub mod persist;
pub mod registry;
pub mod run;
pub mod server;
pub mod state;
pub mod watch;

pub use error::{CoordinatorError, Result};
