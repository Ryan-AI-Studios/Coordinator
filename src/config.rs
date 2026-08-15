//! Machine home, bind defaults, machine config.json, and path resolution.

use std::collections::BTreeMap;
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

/// Env: override `gh` binary (absolute path). Default `gh` / `gh.exe` on PATH.
pub const ENV_COORDINATOR_GH_BIN: &str = "COORDINATOR_GH_BIN";

/// Env: fixed `ci-wait` poll interval in milliseconds (tests). Unset → adaptive 15/30/60s.
pub const ENV_COORDINATOR_CI_POLL_MS: &str = "COORDINATOR_CI_POLL_MS";

/// Env: set to `1` to enable ignored live `gh` smoke tests.
pub const ENV_COORDINATOR_GH_LIVE: &str = "COORDINATOR_GH_LIVE";

/// Env: override `codex` binary for the cross-model gate.
pub const ENV_COORDINATOR_CODEX_BIN: &str = "COORDINATOR_CODEX_BIN";

/// Env: override `claude` binary for the cross-model gate.
pub const ENV_COORDINATOR_CLAUDE_BIN: &str = "COORDINATOR_CLAUDE_BIN";

/// Env: override `opencode` binary for the cross-model gate and plan-review slot.
pub const ENV_COORDINATOR_OPENCODE_BIN: &str = "COORDINATOR_OPENCODE_BIN";

/// Env: set to `1` to enable ignored live `opencode run` plan-review smoke tests.
pub const ENV_COORDINATOR_OPENCODE_LIVE: &str = "COORDINATOR_OPENCODE_LIVE";

/// Env: override `agy` binary for the plan-review Antigravity slot (0017).
pub const ENV_COORDINATOR_AGY_BIN: &str = "COORDINATOR_AGY_BIN";

/// Env: set to `1` to enable ignored live `agy --print` smoke tests.
pub const ENV_COORDINATOR_AGY_LIVE: &str = "COORDINATOR_AGY_LIVE";

/// Env: set to `1` to enable ignored live review-CLI smoke tests.
pub const ENV_COORDINATOR_REVIEW_LIVE: &str = "COORDINATOR_REVIEW_LIVE";

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

/// Role → harness binding (ADR-0012). Stored on machine `config.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoleBinding {
    pub harness: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Default Planner + Implementor → Grok; plan reviewers → agy / opencode.
pub fn default_role_bindings() -> BTreeMap<String, RoleBinding> {
    let grok = RoleBinding {
        harness: "grok".into(),
        command: "grok".into(),
        model: None,
    };
    let mut map = BTreeMap::new();
    map.insert("planner".into(), grok.clone());
    map.insert("implementor".into(), grok);
    map.insert(
        "plan_reviewer_agy".into(),
        RoleBinding {
            harness: "antigravity".into(),
            command: "agy".into(),
            model: None,
        },
    );
    map.insert(
        "plan_reviewer_opencode".into(),
        RoleBinding {
            harness: "opencode".into(),
            command: "opencode".into(),
            model: None,
        },
    );
    map.insert(
        "cross_model_primary".into(),
        RoleBinding {
            harness: "codex".into(),
            command: "codex".into(),
            model: None,
        },
    );
    map.insert(
        "cross_model_secondary".into(),
        RoleBinding {
            harness: "claude".into(),
            command: "claude".into(),
            model: None,
        },
    );
    map.insert(
        "cross_model_tertiary".into(),
        RoleBinding {
            harness: "opencode".into(),
            command: "opencode".into(),
            model: None,
        },
    );
    map
}

/// Opt-in local Hermes inbound webhook (track 0015). Secret is env-only.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HermesNotifyConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,
}

/// Machine-level prefs (`{COORDINATOR_HOME}/config.json`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MachineConfig {
    pub version: u32,
    #[serde(default)]
    pub scan_roots: Vec<PathBuf>,
    #[serde(default = "default_role_bindings")]
    pub role_bindings: BTreeMap<String, RoleBinding>,
    /// Optional per-phase timeout overrides (seconds). Missing keys use table defaults.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub phase_timeouts_secs: BTreeMap<String, u64>,
    /// Hermes notify adapter. Missing key → disabled defaults (no version bump).
    #[serde(default)]
    pub hermes: HermesNotifyConfig,
    /// Adapter progress stall interval (seconds). Missing → 600. `0` disables. No version bump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_stall_secs: Option<u64>,
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self {
            version: MACHINE_CONFIG_VERSION,
            scan_roots: default_scan_roots(),
            role_bindings: default_role_bindings(),
            phase_timeouts_secs: BTreeMap::new(),
            hermes: HermesNotifyConfig::default(),
            progress_stall_secs: None,
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
    let mut cfg: MachineConfig = serde_json::from_str(&text)?;
    if cfg.version != MACHINE_CONFIG_VERSION {
        return Err(CoordinatorError::Message(format!(
            "unsupported machine config version {}; expected {MACHINE_CONFIG_VERSION}",
            cfg.version
        )));
    }
    cfg.scan_roots = normalize_scan_roots(&cfg.scan_roots)?;
    merge_missing_role_bindings(&mut cfg);
    Ok(cfg)
}

/// Serde `default` on the whole map does not insert keys missing from a saved file.
fn merge_missing_role_bindings(cfg: &mut MachineConfig) {
    for (k, v) in default_role_bindings() {
        cfg.role_bindings.entry(k).or_insert(v);
    }
}

pub fn save_machine_config(cfg: &MachineConfig) -> Result<()> {
    let mut cfg = cfg.clone();
    cfg.scan_roots = normalize_scan_roots(&cfg.scan_roots)?;
    let path = machine_config_path()?;
    atomic_write_json(&path, &cfg)
}

/// Require **absolute** scan roots (schema contract). Canonicalize when the path exists.
///
/// Relative paths are always rejected — even if they exist under the current cwd —
/// so persisted `scan_roots` never depend on process working directory.
pub fn normalize_scan_root(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(CoordinatorError::Message(
            "scan root must not be empty".into(),
        ));
    }
    if !path.is_absolute() {
        return Err(CoordinatorError::Message(format!(
            "scan root must be absolute: {}",
            path.display()
        )));
    }
    if path.exists() {
        return crate::registry::canonicalize_path(path);
    }
    Ok(path.to_path_buf())
}

fn normalize_scan_roots(roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    roots.iter().map(|p| normalize_scan_root(p)).collect()
}

/// Resolve scan roots for one invocation: explicit `--root` flags win / extend;
/// otherwise use config `scan_roots`.
pub fn resolve_scan_roots(explicit: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if !explicit.is_empty() {
        return normalize_scan_roots(explicit);
    }
    Ok(load_machine_config()?.scan_roots)
}

/// Process-wide lock for tests that mutate env vars affecting paths/timeouts.
///
/// All tests that set `COORDINATOR_HOME`, `COORDINATOR_STATE_DIR`,
/// `COORDINATOR_STUB_PHASE_TIMEOUT_SECS`, `COORDINATOR_PHASE_TIMEOUT_SECS`,
/// `COORDINATOR_WORKFLOW_DRIVER`, `COORDINATOR_OUTCOME_POLL_MS`,
/// `COORDINATOR_NOTIFY`, `COORDINATOR_HERMES`, `COORDINATOR_HERMES_URL`,
/// `COORDINATOR_HERMES_SECRET`, `COORDINATOR_HERMES_LIVE`,
/// `COORDINATOR_PROGRESS_STALL_SECS`, `COORDINATOR_CANCEL_WAIT_SECS`,
/// `COORDINATOR_AGY_BIN`, `COORDINATOR_OPENCODE_BIN`, or
/// `COORDINATOR_GROK_BIN` must
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
            role_bindings: default_role_bindings(),
            phase_timeouts_secs: BTreeMap::new(),
            hermes: HermesNotifyConfig::default(),
            progress_stall_secs: None,
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

    #[test]
    fn reject_relative_scan_root_in_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"version":1,"scan_roots":["relative\\scan"]}"#).unwrap();
        let err = load_machine_config_at(&path).unwrap_err();
        assert!(err.to_string().contains("absolute"));
    }

    #[test]
    fn reject_existing_relative_scan_root() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = test_env_lock();
        let abs = dir.path().to_path_buf();
        let name = abs.file_name().unwrap().to_os_string();
        let parent = abs.parent().unwrap().to_path_buf();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&parent).unwrap();
        let rel = PathBuf::from(&name);
        assert!(
            rel.exists(),
            "fixture relative dir should exist after chdir"
        );
        let err = normalize_scan_root(&rel).unwrap_err();
        assert!(err.to_string().contains("absolute"));
        std::env::set_current_dir(prev).unwrap();
    }

    #[test]
    fn normalize_scan_root_accepts_absolute() {
        let p = PathBuf::from(r"C:\dev");
        let n = normalize_scan_root(&p).unwrap();
        assert!(n.is_absolute());
    }

    #[test]
    fn old_config_without_role_bindings_gets_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"version":1,"scan_roots":[]}"#).unwrap();
        let loaded = load_machine_config_at(&path).unwrap();
        assert_eq!(loaded.role_bindings["planner"].harness, "grok");
        assert_eq!(loaded.role_bindings["implementor"].command, "grok");
        assert!(loaded.role_bindings["planner"].model.is_none());
        assert_eq!(loaded.role_bindings["plan_reviewer_agy"].command, "agy");
        assert_eq!(
            loaded.role_bindings["plan_reviewer_opencode"].harness,
            "opencode"
        );
        assert_eq!(loaded.role_bindings["cross_model_primary"].harness, "codex");
        assert_eq!(
            loaded.role_bindings["cross_model_secondary"].command,
            "claude"
        );
        assert_eq!(
            loaded.role_bindings["cross_model_tertiary"].harness,
            "opencode"
        );
        assert!(loaded.role_bindings["cross_model_primary"].model.is_none());
        assert!(
            loaded.role_bindings["cross_model_secondary"]
                .model
                .is_none()
        );
        assert!(loaded.role_bindings["cross_model_tertiary"].model.is_none());
        assert!(!loaded.role_bindings.contains_key("fold"));
        assert!(!loaded.role_bindings.contains_key("next"));
    }

    #[test]
    fn merge_missing_reviewer_keys_on_partial_saved_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "version": 1,
                "scan_roots": [],
                "role_bindings": {
                    "planner": { "harness": "grok", "command": "grok" },
                    "implementor": { "harness": "grok", "command": "grok" }
                }
            }"#,
        )
        .unwrap();
        let loaded = load_machine_config_at(&path).unwrap();
        assert_eq!(loaded.role_bindings["planner"].command, "grok");
        assert_eq!(loaded.role_bindings["plan_reviewer_agy"].command, "agy");
        assert_eq!(
            loaded.role_bindings["plan_reviewer_opencode"].command,
            "opencode"
        );
        assert_eq!(loaded.role_bindings["cross_model_primary"].command, "codex");
        assert_eq!(
            loaded.role_bindings["cross_model_secondary"].harness,
            "claude"
        );
        assert_eq!(
            loaded.role_bindings["cross_model_tertiary"].command,
            "opencode"
        );
    }

    #[test]
    fn merge_missing_cross_model_keys_on_old_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "version": 1,
                "scan_roots": [],
                "role_bindings": {
                    "planner": { "harness": "grok", "command": "grok" },
                    "implementor": { "harness": "grok", "command": "grok" },
                    "plan_reviewer_agy": { "harness": "antigravity", "command": "agy" },
                    "plan_reviewer_opencode": { "harness": "opencode", "command": "opencode" }
                }
            }"#,
        )
        .unwrap();
        let loaded = load_machine_config_at(&path).unwrap();
        assert_eq!(loaded.role_bindings["cross_model_primary"].harness, "codex");
        assert_eq!(
            loaded.role_bindings["cross_model_secondary"].command,
            "claude"
        );
        assert_eq!(
            loaded.role_bindings["cross_model_tertiary"].harness,
            "opencode"
        );
        assert!(loaded.role_bindings["cross_model_primary"].model.is_none());
    }

    #[test]
    fn old_config_without_hermes_loads_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"version":1,"scan_roots":[]}"#).unwrap();
        let loaded = load_machine_config_at(&path).unwrap();
        assert!(!loaded.hermes.enabled);
        assert!(loaded.hermes.webhook_url.is_none());
    }

    #[test]
    fn hermes_enabled_url_round_trip_never_persists_secret() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let cfg = MachineConfig {
            version: MACHINE_CONFIG_VERSION,
            scan_roots: Vec::new(),
            role_bindings: default_role_bindings(),
            phase_timeouts_secs: BTreeMap::new(),
            hermes: HermesNotifyConfig {
                enabled: true,
                webhook_url: Some("http://127.0.0.1:8644/webhooks/coordinator-failure".into()),
            },
            progress_stall_secs: None,
        };
        atomic_write_json(&path, &cfg).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("secret"),
            "HMAC secret must never appear in config.json"
        );
        let loaded = load_machine_config_at(&path).unwrap();
        assert!(loaded.hermes.enabled);
        assert_eq!(
            loaded.hermes.webhook_url.as_deref(),
            Some("http://127.0.0.1:8644/webhooks/coordinator-failure")
        );
    }
}
