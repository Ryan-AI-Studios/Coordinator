//! Machine home, bind defaults, and path resolution.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use crate::error::{CoordinatorError, Result};

/// Default localhost API port (avoids Impeccable live 5500/8400).
pub const DEFAULT_SERVE_PORT: u16 = 7420;

/// Loopback bind address only (ADR-0002).
pub const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// Env: override machine home (registry + global config).
pub const ENV_COORDINATOR_HOME: &str = "COORDINATOR_HOME";

/// Env: override base directory for per-project state (tests / advanced ops).
/// When set, each project uses `{COORDINATOR_STATE_DIR}/{project_id}/run-state.json`.
pub const ENV_COORDINATOR_STATE_DIR: &str = "COORDINATOR_STATE_DIR";

/// Env: stub phase wall budget in seconds (default 300). Timeout → failure_class=timeout.
pub const ENV_STUB_PHASE_TIMEOUT_SECS: &str = "COORDINATOR_STUB_PHASE_TIMEOUT_SECS";

/// Env: outcome file poll interval in milliseconds (default 500).
pub const ENV_OUTCOME_POLL_MS: &str = "COORDINATOR_OUTCOME_POLL_MS";

/// Default stub phase budget (seconds) for `stub:active`.
pub const DEFAULT_STUB_PHASE_TIMEOUT_SECS: u64 = 300;

/// Default poll interval for outcome wait / serve (milliseconds).
pub const DEFAULT_OUTCOME_POLL_MS: u64 = 500;

/// Resolve stub phase timeout duration.
pub fn stub_phase_timeout() -> std::time::Duration {
    let secs = std::env::var(ENV_STUB_PHASE_TIMEOUT_SECS)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_STUB_PHASE_TIMEOUT_SECS);
    std::time::Duration::from_secs(secs)
}

/// Resolve outcome poll interval.
pub fn outcome_poll_interval() -> std::time::Duration {
    let ms = std::env::var(ENV_OUTCOME_POLL_MS)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_OUTCOME_POLL_MS)
        .max(1);
    std::time::Duration::from_millis(ms)
}

/// Resolve machine home: `COORDINATOR_HOME` or `%LOCALAPPDATA%\coordinator` (Windows)
/// / `~/.local/share/coordinator` (other).
pub fn machine_home() -> Result<PathBuf> {
    if let Ok(override_home) = std::env::var(ENV_COORDINATOR_HOME) {
        let path = PathBuf::from(override_home);
        if path.as_os_str().is_empty() {
            return Err(CoordinatorError::Message(
                "COORDINATOR_HOME is set but empty".into(),
            ));
        }
        return Ok(path);
    }

    #[cfg(windows)]
    {
        let local = std::env::var_os("LOCALAPPDATA").ok_or_else(|| {
            CoordinatorError::Message(
                "LOCALAPPDATA is not set and COORDINATOR_HOME was not provided".into(),
            )
        })?;
        Ok(PathBuf::from(local).join("coordinator"))
    }

    #[cfg(not(windows))]
    {
        let home = std::env::var_os("HOME").ok_or_else(|| {
            CoordinatorError::Message(
                "HOME is not set and COORDINATOR_HOME was not provided".into(),
            )
        })?;
        Ok(PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("coordinator"))
    }
}

/// Ensure machine home exists; return its path.
pub fn ensure_machine_home() -> Result<PathBuf> {
    let home = machine_home()?;
    std::fs::create_dir_all(&home)?;
    Ok(home)
}

/// Path to `{home}/registry.json`.
pub fn registry_path() -> Result<PathBuf> {
    Ok(ensure_machine_home()?.join("registry.json"))
}

/// Build loopback socket address; reject non-loopback IPs.
pub fn loopback_addr(port: u16) -> SocketAddr {
    SocketAddr::new(LOOPBACK, port)
}

/// Validate that a configured bind IP is loopback-only.
pub fn require_loopback(ip: IpAddr) -> Result<()> {
    if ip.is_loopback() {
        Ok(())
    } else {
        Err(CoordinatorError::NonLoopbackBind(ip.to_string()))
    }
}

/// Optional global state-dir override from env.
pub fn state_dir_override() -> Option<PathBuf> {
    std::env::var_os(ENV_COORDINATOR_STATE_DIR).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn rejects_non_loopback_bind() {
        let err = require_loopback(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0))).unwrap_err();
        assert!(matches!(err, CoordinatorError::NonLoopbackBind(_)));
    }

    #[test]
    fn accepts_loopback() {
        require_loopback(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        require_loopback(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2))).unwrap();
    }

    #[test]
    fn default_port_is_7420() {
        assert_eq!(DEFAULT_SERVE_PORT, 7420);
    }
}
