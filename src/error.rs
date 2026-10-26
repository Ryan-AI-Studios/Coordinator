//! Typed errors for CLI exit codes and HTTP status mapping.

use thiserror::Error;

use crate::harness::preflight::DoctorReport;

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

    /// CLI `run` / `wait` poll budget expired without an applied outcome (exit 2).
    #[error("wait budget expired without an applied outcome")]
    WaitBudgetExpired,

    /// Adapter `run` / `doctor` required harness missing or logged out (exit 1, HTTP 409).
    #[error("{}", report.preflight_message())]
    Preflight { report: Box<DoctorReport> },

    /// 0098: a Blocked failure holds the whole project. Only `failure resolve` releases it.
    #[error(
        "project is blocked at track {track} (conductor row: {row_status}); \
         resolve the prerequisite, then `coordinator failure resolve` and start a new \
         `coordinator run --track {track}`"
    )]
    BlockedHold { track: String, row_status: String },

    /// 0120: an explicit `--track` whose registry row is terminal, refused before any write.
    /// `status` is the canonical terminal token (`Completed`/`Absorbed`/`Cancelled`).
    #[error(
        "track {track} is terminal ({status}) in the conductor registry; nothing was started. \
         Recovery: mint a new track id, or have the owner change the row status \
         (`coordinator registry set-status` on a migrated project). \
         Deliberate override: `coordinator run --replay --track {track}`."
    )]
    TerminalRunTrack { track: String, status: String },

    /// 0120: an explicit `--track` matching more than one parsed row; never pick silently.
    /// `rows` is `"<id>=<token>, <id>=<token>"`.
    #[error(
        "track id {track} matches more than one conductor row ({rows}); refusing an ambiguous \
         id — fix the duplicate rows in the registry"
    )]
    AmbiguousRunTrack { track: String, rows: String },

    /// 0128: pause, resume, or stop matched more than one run, or the selectors disagree.
    /// Not [`Self::AmbiguousRunTrack`] and not `state::ambiguous_run_error`.
    #[error("ambiguous {action} target: {matches}")]
    AmbiguousControlTarget {
        action: &'static str,
        matches: String,
    },
}

impl CoordinatorError {
    /// Suggested process exit code (non-zero on failure).
    ///
    /// Note: `wait` exit **2** means budget expired (not "failure outcome"). Scripts that
    /// care about success-only must inspect `status` / `failure_class` after exit 0.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::WaitBudgetExpired => 2,
            Self::InvalidTransition { .. }
            | Self::BlockedHold { .. }
            | Self::TerminalRunTrack { .. }
            | Self::AmbiguousRunTrack { .. }
            | Self::AmbiguousControlTarget { .. } => 2,
            Self::ProjectNotFound(_) => 3,
            Self::NonLoopbackBind(_) => 4,
            Self::Message(_) | Self::Io(_) | Self::Json(_) | Self::Preflight { .. } => 1,
        }
    }

    /// HTTP status for API responses.
    pub fn http_status(&self) -> u16 {
        match self {
            Self::WaitBudgetExpired => 408,
            Self::InvalidTransition { .. }
            | Self::Preflight { .. }
            | Self::BlockedHold { .. }
            | Self::TerminalRunTrack { .. }
            | Self::AmbiguousRunTrack { .. }
            | Self::AmbiguousControlTarget { .. } => 409,
            Self::ProjectNotFound(_) => 404,
            Self::NonLoopbackBind(_) => 400,
            Self::Message(_) => 400,
            Self::Io(_) | Self::Json(_) => 500,
        }
    }
}

pub type Result<T> = std::result::Result<T, CoordinatorError>;

#[cfg(test)]
mod tests {
    use super::*;

    use crate::harness::preflight::DoctorReport;

    #[test]
    fn terminal_run_track_exit_and_status() {
        let err = CoordinatorError::TerminalRunTrack {
            track: "0099".into(),
            status: "Completed".into(),
        };
        assert_eq!(err.exit_code(), 2);
        assert_eq!(err.http_status(), 409);
        let msg = err.to_string();
        assert!(msg.contains("0099"), "{msg}");
        assert!(msg.contains("terminal (Completed)"), "{msg}");
        assert!(msg.contains("mint a new track id"), "{msg}");
        assert!(msg.contains("--replay --track 0099"), "{msg}");
    }

    #[test]
    fn ambiguous_run_track_exit_and_status() {
        let err = CoordinatorError::AmbiguousRunTrack {
            track: "0099".into(),
            rows: "0099=Ready - not started, 0099=Proposed".into(),
        };
        assert_eq!(err.exit_code(), 2);
        assert_eq!(err.http_status(), 409);
        let msg = err.to_string();
        assert!(msg.contains("more than one conductor row"), "{msg}");
        assert!(msg.contains("0099=Ready - not started"), "{msg}");
    }

    #[test]
    fn existing_mappings_unchanged() {
        assert_eq!(
            CoordinatorError::InvalidTransition {
                action: "run",
                from: "Running".into()
            }
            .exit_code(),
            2
        );
        assert_eq!(
            CoordinatorError::InvalidTransition {
                action: "run",
                from: "Running".into()
            }
            .http_status(),
            409
        );
        assert_eq!(CoordinatorError::Message("x".into()).exit_code(), 1);
        assert_eq!(CoordinatorError::Message("x".into()).http_status(), 400);
        let preflight = CoordinatorError::Preflight {
            report: Box::new(DoctorReport {
                ok: false,
                rows: Vec::new(),
            }),
        };
        assert_eq!(preflight.exit_code(), 1);
        assert_eq!(preflight.http_status(), 409);
        assert_eq!(
            CoordinatorError::Io(std::io::Error::other("x")).http_status(),
            500
        );
    }
}
