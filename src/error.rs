//! Typed errors for CLI exit codes and HTTP status mapping.

use thiserror::Error;

/// Control Plane error kinds shared by CLI and HTTP.
#[derive(Debug, Error)]
pub enum CoordinatorError {
    #[error("{0}")]
    Message(String),

    #[error("project not found: {0}")]
    ProjectNotFound(String),

    #[error("invalid transition: cannot {action} from status {from}")]
    InvalidTransition { action: &'static str, from: String },

    #[error("bind address must be loopback (127.0.0.1); refused: {0}")]
    NonLoopbackBind(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

impl CoordinatorError {
    /// Suggested process exit code (non-zero on failure).
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::InvalidTransition { .. } => 2,
            Self::ProjectNotFound(_) => 3,
            Self::NonLoopbackBind(_) => 4,
            Self::Message(_) | Self::Io(_) | Self::Json(_) => 1,
        }
    }

    /// HTTP status for API responses.
    pub fn http_status(&self) -> u16 {
        match self {
            Self::InvalidTransition { .. } => 409,
            Self::ProjectNotFound(_) => 404,
            Self::NonLoopbackBind(_) => 400,
            Self::Message(_) => 400,
            Self::Io(_) | Self::Json(_) => 500,
        }
    }
}

pub type Result<T> = std::result::Result<T, CoordinatorError>;
