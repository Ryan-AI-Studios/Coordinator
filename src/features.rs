//! Machine feature gates for automation duties (track 0102).
//!
//! One machine-level kill switch per duty, resolved as **env > config > default**
//! and re-read on every call (a config edit is honoured on the next tick; no
//! restart). The gate is a veto: when a duty is on, its existing opt-in still
//! decides. Missing table / missing key / `{}` means **on**.
//!
//! Fail-closed: an unreadable machine config, a malformed/type-invalid
//! `features` table, or a table with an unknown duty key disables **all** duties
//! with source `config`. Environment tokens cannot rescue a broken table. An
//! unrecognised env token fails that one duty closed. Every distinct
//! fail-closed condition emits **one** stderr line per process (process-wide
//! [`WARNED`] set, not a `thread_local!`: tokio workers must not re-emit).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::config::{FeatureTable, MachineConfig};

/// Stderr prefix for feature diagnostics (matches the `coordinator: …` style).
pub const WARN_PREFIX: &str = "coordinator: features: ";

/// Stable duty identifiers. The `as_str` keys are the public config contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Duty {
    AutoStart,
    StallPaging,
    FleetNotify,
    SelfUpdate,
    SelfReap,
    ModelSwap,
}

impl Duty {
    /// Every duty, in contract order.
    pub const ALL: [Duty; 6] = [
        Duty::AutoStart,
        Duty::StallPaging,
        Duty::FleetNotify,
        Duty::SelfUpdate,
        Duty::SelfReap,
        Duty::ModelSwap,
    ];

    /// Stable machine-config key.
    pub fn as_str(self) -> &'static str {
        match self {
            Duty::AutoStart => "auto_start",
            Duty::StallPaging => "stall_paging",
            Duty::FleetNotify => "fleet_notify",
            Duty::SelfUpdate => "self_update",
            Duty::SelfReap => "self_reap",
            Duty::ModelSwap => "model_swap",
        }
    }

    /// Env override name (`COORDINATOR_FEATURE_*`).
    pub fn env_var(self) -> &'static str {
        match self {
            Duty::AutoStart => "COORDINATOR_FEATURE_AUTO_START",
            Duty::StallPaging => "COORDINATOR_FEATURE_STALL_PAGING",
            Duty::FleetNotify => "COORDINATOR_FEATURE_FLEET_NOTIFY",
            Duty::SelfUpdate => "COORDINATOR_FEATURE_SELF_UPDATE",
            Duty::SelfReap => "COORDINATOR_FEATURE_SELF_REAP",
            Duty::ModelSwap => "COORDINATOR_FEATURE_MODEL_SWAP",
        }
    }
}

/// Where one duty's effective value came from.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FeatureSource {
    Default,
    Config,
    Env,
}

/// Effective state of one duty.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DutyGateView {
    pub enabled: bool,
    pub source: FeatureSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Effective state of every duty (status surface + fleet payload).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeatureSnapshot {
    #[serde(flatten)]
    pub duties: BTreeMap<String, DutyGateView>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unknown_keys: Vec<String>,
}

impl FeatureSnapshot {
    /// Fail closed: an absent duty entry reads as disabled.
    pub fn enabled(&self, duty: Duty) -> bool {
        self.duties
            .get(duty.as_str())
            .is_some_and(|view| view.enabled)
    }
}

/// Process-wide warn-once set. **Not** `thread_local!`: `serve_poll_loop` and the
/// axum status handlers hop tokio workers, so per-thread state would re-emit.
static WARNED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// Test-only recorder, drained by [`take_feature_warnings`].
#[cfg(test)]
static RECORDED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Emit `line` once per process (dedupe keyed by the full line).
fn warn_once(line: &str) {
    let first = {
        let mut warned = WARNED.lock().unwrap_or_else(|e| e.into_inner());
        warned.insert(line.to_string())
    };
    if !first {
        return;
    }
    eprintln!("{line}");
    #[cfg(test)]
    {
        let mut recorded = RECORDED.lock().unwrap_or_else(|e| e.into_inner());
        recorded.push(line.to_string());
    }
}

/// Drain the test recorder (API shape mirrors `notify::toast::take_recorded_toasts`).
#[cfg(test)]
pub(crate) fn take_feature_warnings() -> Vec<String> {
    let mut recorded = RECORDED.lock().unwrap_or_else(|e| e.into_inner());
    std::mem::take(&mut *recorded)
}

/// Clear the recorder **and** the process-wide dedupe set.
#[cfg(test)]
pub(crate) fn reset_feature_warnings_for_tests() {
    {
        let mut recorded = RECORDED.lock().unwrap_or_else(|e| e.into_inner());
        recorded.clear();
    }
    {
        let mut warned = WARNED.lock().unwrap_or_else(|e| e.into_inner());
        warned.clear();
    }
}

/// Every duty env name (fixtures capture/restore all six).
#[cfg(test)]
pub(crate) fn test_duty_env_vars() -> [&'static str; 6] {
    Duty::ALL.map(|duty| duty.env_var())
}

/// Fixture env names: machine home + legacy fleet/Hermes + every duty gate.
#[cfg(test)]
pub(crate) fn test_feature_env_names() -> Vec<&'static str> {
    let mut names = vec![
        crate::config::ENV_COORDINATOR_HOME,
        "COORDINATOR_NOTIFY_FLEET",
        "COORDINATOR_HERMES",
    ];
    names.extend(test_duty_env_vars());
    names
}

/// Resolve one snapshot from a table, an optional config-load error, and an env
/// reader. Precedence per duty: env > config > default.
pub fn evaluate_with(
    table: &FeatureTable,
    config_error: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> FeatureSnapshot {
    let mut snapshot = FeatureSnapshot::default();

    if let Some(err) = config_error {
        let line = format!("{WARN_PREFIX}machine config unreadable; all duties off: {err}");
        warn_once(&line);
        fail_closed_all(&mut snapshot, format!("machine config unreadable: {err}"));
        return snapshot;
    }

    if let Some(invalid) = table.invalid() {
        let line = format!("{WARN_PREFIX}{invalid}; all duties off");
        warn_once(&line);
        fail_closed_all(&mut snapshot, invalid.to_string());
        return snapshot;
    }

    let known: BTreeSet<&str> = Duty::ALL.iter().map(|duty| duty.as_str()).collect();
    let unknown: Vec<String> = table
        .duties()
        .keys()
        .filter(|key| !known.contains(key.as_str()))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        let named = quoted_list(&unknown);
        let known_list = known_list_sorted();
        let line = format!("{WARN_PREFIX}unknown {named}; all duties off (known: {known_list})");
        warn_once(&line);
        snapshot.unknown_keys = unknown.clone();
        fail_closed_all(
            &mut snapshot,
            format!("unknown duty: {}", unknown.join(", ")),
        );
        return snapshot;
    }

    for duty in Duty::ALL {
        let view = if let Some(raw) = env(duty.env_var()) {
            let token = raw.trim();
            if on_family(token) {
                DutyGateView {
                    enabled: true,
                    source: FeatureSource::Env,
                    error: None,
                }
            } else if off_family(token) {
                DutyGateView {
                    enabled: false,
                    source: FeatureSource::Env,
                    error: None,
                }
            } else {
                let line = format!(
                    "{WARN_PREFIX}{}={token} is not on/off; treating {} as off",
                    duty.env_var(),
                    duty.as_str()
                );
                warn_once(&line);
                DutyGateView {
                    enabled: false,
                    source: FeatureSource::Env,
                    error: Some(format!("{}={token} is not on/off", duty.env_var())),
                }
            }
        } else if let Some(enabled) = table.get(duty.as_str()) {
            DutyGateView {
                enabled,
                source: FeatureSource::Config,
                error: None,
            }
        } else {
            DutyGateView {
                enabled: true,
                source: FeatureSource::Default,
                error: None,
            }
        };
        snapshot.duties.insert(duty.as_str().to_string(), view);
    }
    snapshot
}

/// Resolve from an already-loaded machine config (one load per caller).
pub fn evaluate_loaded(loaded: &crate::error::Result<MachineConfig>) -> FeatureSnapshot {
    let env = |name: &str| std::env::var(name).ok();
    match loaded {
        Ok(cfg) => evaluate_with(&cfg.features, None, &env),
        Err(err) => evaluate_with(&FeatureTable::default(), Some(&err.to_string()), &env),
    }
}

/// Load the machine config once and resolve every duty (live re-read).
pub fn resolve_all() -> FeatureSnapshot {
    #[cfg(test)]
    RESOLVE_ALL_CALLS.with(|c| c.set(c.get() + 1));
    evaluate_loaded(&crate::config::load_machine_config())
}

#[cfg(test)]
thread_local! {
    /// Thread-local resolve counter (the `VIEW_PARSE_READS` precedent) so DoD-4's
    /// "one `resolve_all` per call" can fail when the code resolves twice.
    static RESOLVE_ALL_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn resolve_all_calls() -> usize {
    RESOLVE_ALL_CALLS.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(crate) fn reset_resolve_all_calls() {
    RESOLVE_ALL_CALLS.with(|c| c.set(0));
}

/// Convenience for one duty (always a fresh read; never cached).
pub fn is_enabled(duty: Duty) -> bool {
    resolve_all().enabled(duty)
}

fn fail_closed_all(snapshot: &mut FeatureSnapshot, error: String) {
    for duty in Duty::ALL {
        snapshot.duties.insert(
            duty.as_str().to_string(),
            DutyGateView {
                enabled: false,
                source: FeatureSource::Config,
                error: Some(error.clone()),
            },
        );
    }
}

fn on_family(token: &str) -> bool {
    matches!(
        token.to_ascii_lowercase().as_str(),
        "1" | "true" | "on" | "yes"
    )
}

fn off_family(token: &str) -> bool {
    matches!(
        token.to_ascii_lowercase().as_str(),
        "0" | "false" | "off" | "no"
    )
}

fn quoted_list(keys: &[String]) -> String {
    let quoted: Vec<String> = keys.iter().map(|key| format!("\"{key}\"")).collect();
    if keys.len() == 1 {
        format!("duty {}", quoted[0])
    } else {
        format!("duties {}", quoted.join(", "))
    }
}

fn known_list_sorted() -> String {
    let mut known: Vec<&str> = Duty::ALL.iter().map(|duty| duty.as_str()).collect();
    known.sort_unstable();
    known.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        MACHINE_CONFIG_VERSION, TestEnvGuard, load_machine_config_at, test_env_lock,
    };
    use std::path::Path;
    use tempfile::{TempDir, tempdir};

    fn guard() -> TestEnvGuard {
        TestEnvGuard::capture(&test_feature_env_names())
    }

    fn clear_feature_env() {
        for name in test_duty_env_vars() {
            unsafe {
                std::env::remove_var(name);
            }
        }
    }

    fn table(duties: &[(&str, bool)]) -> FeatureTable {
        FeatureTable::from_duties(
            duties
                .iter()
                .map(|(key, value)| ((*key).to_string(), *value))
                .collect(),
        )
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn env_with(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    fn write_config(home: &Path, body: serde_json::Value) {
        std::fs::write(home.join("config.json"), body.to_string()).unwrap();
    }

    fn config_with_features(features: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"version": MACHINE_CONFIG_VERSION, "scan_roots": [], "features": features})
    }

    fn load_in(home: &Path) -> crate::config::MachineConfig {
        load_machine_config_at(&home.join("config.json")).unwrap()
    }

    fn isolated_home() -> (TempDir, TestEnvGuard) {
        let guard = guard();
        clear_feature_env();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        (home, guard)
    }

    #[test]
    fn defaults_are_on_with_source_default() {
        let _lock = test_env_lock();
        let _guard = guard();
        reset_feature_warnings_for_tests();
        let snapshot = evaluate_with(&FeatureTable::default(), None, &no_env);
        assert_eq!(snapshot.duties.len(), 6);
        for duty in Duty::ALL {
            let view = &snapshot.duties[duty.as_str()];
            assert!(view.enabled, "{} should default on", duty.as_str());
            assert_eq!(view.source, FeatureSource::Default);
            assert!(view.error.is_none());
        }
        assert!(snapshot.unknown_keys.is_empty());
        assert!(take_feature_warnings().is_empty());
    }

    #[test]
    fn table_false_is_config_off_and_true_is_config_on() {
        let _lock = test_env_lock();
        let _guard = guard();
        reset_feature_warnings_for_tests();
        let table = table(&[("fleet_notify", false), ("auto_start", true)]);
        let snapshot = evaluate_with(&table, None, &no_env);
        let fleet = &snapshot.duties["fleet_notify"];
        assert!(!fleet.enabled);
        assert_eq!(fleet.source, FeatureSource::Config);
        assert!(fleet.error.is_none());
        let auto = &snapshot.duties["auto_start"];
        assert!(auto.enabled);
        assert_eq!(auto.source, FeatureSource::Config);
        assert_eq!(
            snapshot.duties["self_reap"].source,
            FeatureSource::Default,
            "absent key keeps the default"
        );
    }

    #[test]
    fn env_off_overrides_config_on() {
        let _lock = test_env_lock();
        let _guard = guard();
        reset_feature_warnings_for_tests();
        let table = table(&[("fleet_notify", true)]);
        let env = env_with(&[(Duty::FleetNotify.env_var(), "no")]);
        let snapshot = evaluate_with(&table, None, &env);
        let fleet = &snapshot.duties["fleet_notify"];
        assert!(!fleet.enabled);
        assert_eq!(fleet.source, FeatureSource::Env);
        assert!(fleet.error.is_none());
    }

    #[test]
    fn env_on_overrides_config_off() {
        let _lock = test_env_lock();
        let _guard = guard();
        reset_feature_warnings_for_tests();
        let table = table(&[("fleet_notify", false)]);
        let env = env_with(&[(Duty::FleetNotify.env_var(), " YES ")]);
        let snapshot = evaluate_with(&table, None, &env);
        let fleet = &snapshot.duties["fleet_notify"];
        assert!(fleet.enabled);
        assert_eq!(fleet.source, FeatureSource::Env);
    }

    #[test]
    fn invalid_env_token_fails_closed_and_warns_once() {
        let _lock = test_env_lock();
        let _guard = guard();
        for token in ["of", "maybe", ""] {
            reset_feature_warnings_for_tests();
            let env = env_with(&[(Duty::FleetNotify.env_var(), token)]);
            let snapshot = evaluate_with(&FeatureTable::default(), None, &env);
            let fleet = &snapshot.duties["fleet_notify"];
            assert!(!fleet.enabled, "token {token:?} must fail closed");
            assert_eq!(fleet.source, FeatureSource::Env);
            assert!(fleet.error.is_some());
            assert!(
                snapshot.duties["auto_start"].enabled,
                "one bad token does not disable the other duties"
            );
            let lines = take_feature_warnings();
            assert_eq!(lines.len(), 1, "one line per distinct condition");
            assert!(lines[0].contains("is not on/off"));
            // A second evaluation of the same condition is silent.
            let _ = evaluate_with(&FeatureTable::default(), None, &env);
            assert!(take_feature_warnings().is_empty());
        }
    }

    #[test]
    fn warn_once_is_process_wide_across_threads() {
        let _lock = test_env_lock();
        reset_feature_warnings_for_tests();
        let line = "coordinator: features: test line; all duties off";
        let handles: Vec<_> = (0..2)
            .map(|_| std::thread::spawn(move || warn_once(line)))
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(
            take_feature_warnings(),
            vec![line.to_string()],
            "two threads emit one recorded line (process-wide set, not thread_local)"
        );
        warn_once(line);
        assert!(
            take_feature_warnings().is_empty(),
            "third call on the test thread records nothing"
        );
    }

    #[test]
    fn malformed_table_disables_all_and_warns_once() {
        let _lock = test_env_lock();
        let (_home, _guard) = isolated_home();
        let cases = [
            serde_json::json!("nope"),
            serde_json::json!({"fleet_notify": "yes"}),
            serde_json::json!([true]),
            serde_json::json!({"auto_strat": true, "fleet_notify": "yes"}),
        ];
        for features in cases {
            reset_feature_warnings_for_tests();
            let home = tempdir().unwrap();
            write_config(home.path(), config_with_features(features.clone()));
            let cfg = load_in(home.path());
            assert!(
                !cfg.role_bindings.is_empty(),
                "malformed features must not fail MachineConfig load"
            );
            assert!(cfg.features.invalid().is_some());
            let snapshot = evaluate_with(&cfg.features, None, &no_env);
            for duty in Duty::ALL {
                let view = &snapshot.duties[duty.as_str()];
                assert!(!view.enabled, "{features} must disable all duties");
                assert_eq!(view.source, FeatureSource::Config);
                assert!(view.error.is_some());
            }
            assert!(
                snapshot.unknown_keys.is_empty(),
                "the invalid path never enumerates keys (combined case)"
            );
            assert_eq!(take_feature_warnings().len(), 1);
        }
    }

    #[test]
    fn unreadable_config_disables_all() {
        let _lock = test_env_lock();
        let (home, _guard) = isolated_home();
        reset_feature_warnings_for_tests();
        std::fs::write(home.path().join("config.json"), "{not-json").unwrap();
        let snapshot = resolve_all();
        for duty in Duty::ALL {
            let view = &snapshot.duties[duty.as_str()];
            assert!(!view.enabled);
            assert_eq!(view.source, FeatureSource::Config);
            assert!(view.error.is_some());
        }
        // An on env token cannot rescue a broken config.
        unsafe {
            std::env::set_var(Duty::FleetNotify.env_var(), "on");
        }
        let rescued = resolve_all();
        for duty in Duty::ALL {
            assert!(!rescued.enabled(duty), "no env rescue for a broken config");
            assert_eq!(rescued.duties[duty.as_str()].source, FeatureSource::Config);
        }
        assert_eq!(
            take_feature_warnings().len(),
            1,
            "one line for the distinct unreadable-config condition"
        );

        // Unsupported version takes the same fail-closed path.
        reset_feature_warnings_for_tests();
        std::fs::write(
            home.path().join("config.json"),
            r#"{"version":9,"scan_roots":[]}"#,
        )
        .unwrap();
        let versioned = resolve_all();
        for duty in Duty::ALL {
            let view = &versioned.duties[duty.as_str()];
            assert!(!view.enabled);
            assert_eq!(view.source, FeatureSource::Config);
        }
        assert_eq!(take_feature_warnings().len(), 1);
    }

    #[test]
    fn unknown_key_fails_closed_listed_and_warned_once() {
        let _lock = test_env_lock();
        let (home, _guard) = isolated_home();
        reset_feature_warnings_for_tests();
        write_config(
            home.path(),
            config_with_features(serde_json::json!({
                "auto_strat": true,
                "fleet_notify": false,
            })),
        );
        let snapshot = resolve_all();
        for duty in Duty::ALL {
            let view = &snapshot.duties[duty.as_str()];
            assert!(
                !view.enabled,
                "a known false does not rescue an unknown-key table"
            );
            assert_eq!(view.source, FeatureSource::Config);
            assert!(view.error.is_some());
        }
        assert_eq!(snapshot.unknown_keys, vec!["auto_strat".to_string()]);
        let lines = take_feature_warnings();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("auto_strat"));
        // Second read is silent (still fail-closed).
        let again = resolve_all();
        assert!(!again.enabled(Duty::AutoStart));
        assert!(take_feature_warnings().is_empty());
    }

    #[test]
    fn legacy_config_without_features_defaults_on() {
        let _lock = test_env_lock();
        let (home, _guard) = isolated_home();
        reset_feature_warnings_for_tests();
        std::fs::write(
            home.path().join("config.json"),
            r#"{"version":1,"scan_roots":[]}"#,
        )
        .unwrap();
        assert_eq!(MACHINE_CONFIG_VERSION, 1, "no version bump for 0102");
        let snapshot = resolve_all();
        for duty in Duty::ALL {
            let view = &snapshot.duties[duty.as_str()];
            assert!(view.enabled);
            assert_eq!(view.source, FeatureSource::Default);
        }
        assert!(take_feature_warnings().is_empty());
    }

    #[test]
    fn features_round_trip_and_empty_is_omitted() {
        let _lock = test_env_lock();
        let (home, _guard) = isolated_home();
        reset_feature_warnings_for_tests();

        // Default config omits the key entirely.
        let default_cfg = crate::config::MachineConfig::default();
        let text = serde_json::to_string(&default_cfg).unwrap();
        assert!(!text.contains("\"features\""), "empty table is omitted");

        // Valid bools survive save/load.
        write_config(
            home.path(),
            config_with_features(serde_json::json!({
                "fleet_notify": false,
                "auto_start": true,
            })),
        );
        crate::config::save_machine_config(&load_in(home.path())).unwrap();
        let reloaded = load_in(home.path());
        assert_eq!(reloaded.features.get("fleet_notify"), Some(false));
        assert_eq!(reloaded.features.get("auto_start"), Some(true));
        assert_eq!(reloaded.features.get("self_reap"), None);

        // A malformed table is re-emitted as the same JSON value on save.
        let malformed = serde_json::json!({"fleet_notify": "yes", "auto_strat": true});
        write_config(home.path(), config_with_features(malformed.clone()));
        let loaded = load_in(home.path());
        crate::config::save_machine_config(&loaded).unwrap();
        let text = std::fs::read_to_string(home.path().join("config.json")).unwrap();
        let saved: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            saved["features"], malformed,
            "semantic (reparse-equal) preservation keeps duties closed"
        );
        let after = resolve_all();
        for duty in Duty::ALL {
            assert!(!after.enabled(duty), "still fail-closed after save");
        }
    }

    #[test]
    fn all_duty_names_and_env_names_are_stable() {
        let names: Vec<&str> = Duty::ALL.iter().map(|duty| duty.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "auto_start",
                "stall_paging",
                "fleet_notify",
                "self_update",
                "self_reap",
                "model_swap"
            ]
        );
        let envs: Vec<&str> = Duty::ALL.iter().map(|duty| duty.env_var()).collect();
        assert_eq!(
            envs,
            vec![
                "COORDINATOR_FEATURE_AUTO_START",
                "COORDINATOR_FEATURE_STALL_PAGING",
                "COORDINATOR_FEATURE_FLEET_NOTIFY",
                "COORDINATOR_FEATURE_SELF_UPDATE",
                "COORDINATOR_FEATURE_SELF_REAP",
                "COORDINATOR_FEATURE_MODEL_SWAP"
            ]
        );
    }
}
