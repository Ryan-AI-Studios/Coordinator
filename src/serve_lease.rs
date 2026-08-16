//! `{COORDINATOR_HOME}/serve.json` lease (always-on ticker discovery).
//!
//! Written after a successful bind; deleted on graceful shutdown. A leftover
//! file after a crash is OK — health JSON is the truth, not this file.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{ensure_machine_home, machine_home};
use crate::error::Result;
use crate::persist::atomic_write_json;

pub const SERVE_LEASE_VERSION: u32 = 1;
pub const SERVE_LEASE_BIND: &str = "127.0.0.1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServeLease {
    pub version: u32,
    pub port: u16,
    pub pid: u32,
    pub bind: String,
    pub started_at: DateTime<Utc>,
}

/// `{COORDINATOR_HOME}/serve.json`.
pub fn serve_lease_path() -> Result<std::path::PathBuf> {
    Ok(machine_home()?.join("serve.json"))
}

/// Atomic write after a successful bind. `pid` is this process.
pub fn write_serve_lease(port: u16) -> Result<()> {
    let home = ensure_machine_home()?;
    let lease = ServeLease {
        version: SERVE_LEASE_VERSION,
        port,
        pid: std::process::id(),
        bind: SERVE_LEASE_BIND.into(),
        started_at: Utc::now(),
    };
    atomic_write_json(&home.join("serve.json"), &lease)
}

/// Best-effort delete. Missing file is Ok.
pub fn clear_serve_lease() {
    if let Ok(path) = serve_lease_path() {
        let _ = std::fs::remove_file(path);
    }
}

/// Missing file, torn JSON, or unknown `version` → `None`.
pub fn read_serve_lease() -> Option<ServeLease> {
    let path = serve_lease_path().ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    let lease: ServeLease = serde_json::from_str(&text).ok()?;
    if lease.version != SERVE_LEASE_VERSION {
        return None;
    }
    Some(lease)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ENV_COORDINATOR_HOME, test_env_lock};
    use tempfile::tempdir;

    fn isolate_home() -> tempfile::TempDir {
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        home
    }

    fn clear_home() {
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[test]
    fn write_read_clear_round_trip() {
        let _guard = test_env_lock();
        let home = isolate_home();
        assert!(read_serve_lease().is_none());
        write_serve_lease(7500).unwrap();
        let lease = read_serve_lease().expect("lease");
        assert_eq!(lease.version, SERVE_LEASE_VERSION);
        assert_eq!(lease.port, 7500);
        assert_eq!(lease.pid, std::process::id());
        assert_eq!(lease.bind, SERVE_LEASE_BIND);
        assert!(home.path().join("serve.json").exists());
        clear_serve_lease();
        assert!(read_serve_lease().is_none());
        assert!(!home.path().join("serve.json").exists());
        clear_serve_lease();
        clear_home();
    }

    #[test]
    fn unknown_version_and_torn_json_are_none() {
        let _guard = test_env_lock();
        let home = isolate_home();
        std::fs::write(
            home.path().join("serve.json"),
            r#"{
                "version": 99,
                "port": 7420,
                "pid": 1,
                "bind": "127.0.0.1",
                "started_at": "2026-08-15T12:00:00Z"
            }"#,
        )
        .unwrap();
        assert!(read_serve_lease().is_none());
        std::fs::write(home.path().join("serve.json"), b"{not valid json").unwrap();
        assert!(read_serve_lease().is_none());
        clear_home();
    }
}
