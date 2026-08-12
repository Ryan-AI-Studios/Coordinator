//! Machine home, bind defaults, machine config.json, and path resolution.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CoordinatorError, Result};
use crate::persist::atomic_write_json;

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
///
/// Empty `COORDINATOR_STATE_DIR` is rejected (same class as empty `COORDINATOR_HOME`).
pub fn state_dir_override() -> Result<Option<PathBuf>> {
    match std::env::var(ENV_COORDINATOR_STATE_DIR) {
        Ok(s) if s.is_empty() => Err(CoordinatorError::Message(
            "COORDINATOR_STATE_DIR is set but empty".into(),
        )),
        Ok(s) => Ok(Some(PathBuf::from(s))),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(CoordinatorError::Message(format!(
            "COORDINATOR_STATE_DIR: {e}"
        ))),
    }
}

pub const MACHINE_CONFIG_VERSION: u32 = 1;

/// Machine-level prefs (`{COORDINATOR_HOME}/config.json`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MachineConfig {
    pub version: u32,
    #[serde(default)]
    pub scan_roots: Vec<PathBuf>,
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self {
            version: MACHINE_CONFIG_VERSION,
            scan_roots: default_scan_roots(),
        }
    }
}

/// Windows: `C:\dev` when it exists; otherwise empty (CI-safe).
pub fn default_scan_roots() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let p = PathBuf::from(r"C:\dev");
        if p.is_dir() { vec![p] } else { Vec::new() }
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// Path to `{home}/config.json`.
pub fn machine_config_path() -> Result<PathBuf> {
    Ok(ensure_machine_home()?.join("config.json"))
}

/// Load machine config; missing file → defaults (including default scan_roots).
pub fn load_machine_config() -> Result<MachineConfig> {
    let path = machine_config_path()?;
    load_machine_config_at(&path)
}

pub fn load_machine_config_at(path: &Path) -> Result<MachineConfig> {
    if !path.exists() {
        return Ok(MachineConfig::default());
    }
    let text = std::fs::read_to_string(path)?;
    let cfg: MachineConfig = serde_json::from_str(&text)?;
    if cfg.version != MACHINE_CONFIG_VERSION {
        return Err(CoordinatorError::Message(format!(
            "unsupported machine config version {}; expected {MACHINE_CONFIG_VERSION}",
            cfg.version
        )));
    }
    Ok(cfg)
}

pub fn save_machine_config(cfg: &MachineConfig) -> Result<()> {
    let path = machine_config_path()?;
    atomic_write_json(&path, cfg)
}

/// Resolve scan roots for one invocation: explicit `--root` flags win / extend;
/// otherwise use config `scan_roots`.
pub fn resolve_scan_roots(explicit: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if !explicit.is_empty() {
        return explicit
            .iter()
            .map(|p| {
                if p.is_absolute() {
                    Ok(p.clone())
                } else if p.exists() {
                    crate::registry::canonicalize_path(p)
                } else {
                    Err(CoordinatorError::Message(format!(
                        "scan root must be absolute (or existing): {}",
                        p.display()
                    )))
                }
            })
            .collect();
    }
    Ok(load_machine_config()?.scan_roots)
}

/// Process-wide lock for tests that mutate env vars affecting paths/timeouts.
///
/// All tests that set `COORDINATOR_HOME`, `COORDINATOR_STATE_DIR`,
/// `COORDINATOR_STUB_PHASE_TIMEOUT_SECS`, or `COORDINATOR_OUTCOME_POLL_MS` must
/// hold this (survives poison so one failure does not cascade).
#[cfg(test)]
pub fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let m = LOCK.get_or_init(|| Mutex::new(()));
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
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

    #[test]
    fn machine_config_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let cfg = MachineConfig {
            version: MACHINE_CONFIG_VERSION,
            scan_roots: vec![PathBuf::from(r"C:\dev")],
        };
        atomic_write_json(&path, &cfg).unwrap();
        let loaded = load_machine_config_at(&path).unwrap();
        assert_eq!(loaded.scan_roots.len(), 1);
    }

    #[test]
    fn machine_config_reject_bad_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"version":9,"scan_roots":[]}"#).unwrap();
        assert!(load_machine_config_at(&path).is_err());
    }

    #[test]
    fn empty_state_dir_env_errors() {
        let _guard = test_env_lock();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_STATE_DIR, "");
        }
        let err = state_dir_override().unwrap_err();
        assert!(err.to_string().contains("empty"));
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_STATE_DIR);
        }
    }
}
