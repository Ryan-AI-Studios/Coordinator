//! Per-phase wall budgets (ADR-0016).

use std::collections::BTreeMap;
use std::time::Duration;

use crate::config::stub_phase_timeout;
use crate::error::{CoordinatorError, Result};
use crate::registry::ProjectRecord;

use super::graph::{self, is_canonical, is_stub_phase};

/// Uniform override for every canonical phase (tests).
pub const ENV_PHASE_TIMEOUT_SECS: &str = "COORDINATOR_PHASE_TIMEOUT_SECS";

/// Extra spawn clock for the plan-review OpenCode slot (not a DAG phase).
pub const TIMEOUT_KEY_PLAN_REVIEW_SLOT: &str = "plan_review_slot";

/// Where `timeout_for_phase` took the budget from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeoutSource {
    Env,
    Project,
    Machine,
    Table,
    Stub,
}

impl TimeoutSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::Project => "project",
            Self::Machine => "machine",
            Self::Table => "table",
            Self::Stub => "stub",
        }
    }
}

/// Table defaults from spec (seconds).
pub fn default_timeout_secs(phase: &str) -> u64 {
    match phase {
        graph::PHASE_PLAN => 1800,
        graph::PHASE_PLAN_REVIEW => 2400,
        TIMEOUT_KEY_PLAN_REVIEW_SLOT => 2400,
        graph::PHASE_FOLD => 1200,
        graph::PHASE_IMPLEMENT => 7200,
        graph::PHASE_CROSS_MODEL => 2700,
        graph::PHASE_CI_WAIT => 3600,
        graph::PHASE_COMPACT => 600,
        graph::PHASE_ADVANCE => 900,
        graph::PHASE_ADDRESS_FINDINGS => 3600,
        _ => 300,
    }
}

/// Parse CLI `PHASE=SECS` (trim key + value; first `=`; canonical key; secs > 0).
pub fn parse_phase_timeout(s: &str) -> Result<(String, u64)> {
    let Some((key_raw, val_raw)) = s.split_once('=') else {
        return Err(CoordinatorError::Message(format!(
            "phase timeout must be PHASE=SECS (got '{s}')"
        )));
    };
    let key = key_raw.trim();
    let val = val_raw.trim();
    if key.is_empty() {
        return Err(CoordinatorError::Message(
            "phase timeout key must not be empty".into(),
        ));
    }
    if !is_timeout_key(key) {
        return Err(CoordinatorError::Message(unknown_timeout_key_msg(key)));
    }
    let secs = val.parse::<u64>().map_err(|_| {
        CoordinatorError::Message(format!(
            "phase timeout seconds must be a positive integer (got '{val}')"
        ))
    })?;
    if secs == 0 {
        return Err(CoordinatorError::Message(
            "phase timeout seconds must be > 0 (0 is not a clear; use --clear-phase-timeout)"
                .into(),
        ));
    }
    Ok((key.to_string(), secs))
}

/// Same rules as `parse_phase_timeout` for HTTP / Registry maps.
pub fn validate_phase_timeout_map(map: &BTreeMap<String, u64>) -> Result<()> {
    for (key, secs) in map {
        validate_phase_timeout_key(key)?;
        if *secs == 0 {
            return Err(CoordinatorError::Message(format!(
                "phase timeout seconds must be > 0 for '{key}' (0 is not a clear)"
            )));
        }
    }
    Ok(())
}

/// Canonical phase id or [`TIMEOUT_KEY_PLAN_REVIEW_SLOT`] (clear flags / map keys).
pub fn validate_phase_timeout_key(phase: &str) -> Result<()> {
    if phase.is_empty() {
        return Err(CoordinatorError::Message(
            "phase timeout key must not be empty".into(),
        ));
    }
    if !is_timeout_key(phase) {
        return Err(CoordinatorError::Message(unknown_timeout_key_msg(phase)));
    }
    Ok(())
}

/// Canonical phase id **or** [`TIMEOUT_KEY_PLAN_REVIEW_SLOT`].
pub fn is_timeout_key(key: &str) -> bool {
    is_canonical(key) || key == TIMEOUT_KEY_PLAN_REVIEW_SLOT
}

fn unknown_timeout_key_msg(key: &str) -> String {
    format!(
        "unknown phase '{key}'; expected a canonical phase id (plan, plan-review, fold, implement, cross-model-review, ci-wait, compact, advance, address-findings) or plan_review_slot"
    )
}

/// Resolve the wall budget for `phase`.
///
/// 1. `stub:*` → `COORDINATOR_STUB_PHASE_TIMEOUT_SECS` / 300s
/// 2. `COORDINATOR_PHASE_TIMEOUT_SECS` uniform override (timeout keys)
/// 3. project `phase_timeouts_secs`
/// 4. machine `phase_timeouts_secs`
/// 5. table `default_timeout_secs`
pub fn timeout_for_phase(record: &ProjectRecord, phase: &str) -> Duration {
    Duration::from_secs(resolve_timeout(record, phase).0)
}

/// Source that `timeout_for_phase` would use (same resolve order).
pub fn timeout_source(record: &ProjectRecord, phase: &str) -> TimeoutSource {
    resolve_timeout(record, phase).1
}

fn resolve_timeout(record: &ProjectRecord, phase: &str) -> (u64, TimeoutSource) {
    if is_stub_phase(phase) {
        return (stub_phase_timeout().as_secs(), TimeoutSource::Stub);
    }
    if is_timeout_key(phase)
        && let Ok(s) = std::env::var(ENV_PHASE_TIMEOUT_SECS)
        && let Ok(secs) = s.parse::<u64>()
    {
        return (secs, TimeoutSource::Env);
    }
    if let Some(&secs) = record.phase_timeouts_secs.get(phase) {
        return (secs, TimeoutSource::Project);
    }
    if let Ok(cfg) = crate::config::load_machine_config()
        && let Some(&secs) = cfg.phase_timeouts_secs.get(phase)
    {
        return (secs, TimeoutSource::Machine);
    }
    (default_timeout_secs(phase), TimeoutSource::Table)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ENV_COORDINATOR_HOME, MachineConfig, save_machine_config, test_env_lock};
    use chrono::Utc;
    use uuid::Uuid;

    fn empty_record() -> ProjectRecord {
        ProjectRecord {
            id: Uuid::new_v4().to_string(),
            path: std::path::PathBuf::from(r"C:\dev\empty-timeout-fixture"),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: BTreeMap::new(),
            state_dir: None,
            auto_merge: true,
            phase_timeouts_secs: BTreeMap::new(),
            notify_progress: false,
            ready_aliases: Vec::new(),
            created_at: Utc::now(),
        }
    }

    fn isolate_home() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        unsafe {
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }
        home
    }

    fn clear_home() {
        unsafe {
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[test]
    fn plan_default_is_not_300s() {
        assert_eq!(default_timeout_secs(graph::PHASE_PLAN), 1800);
        assert_ne!(default_timeout_secs(graph::PHASE_PLAN), 300);
        assert_eq!(default_timeout_secs(graph::PHASE_IMPLEMENT), 7200);
        assert_eq!(default_timeout_secs(graph::PHASE_COMPACT), 600);
        assert_eq!(default_timeout_secs(graph::PHASE_ADDRESS_FINDINGS), 3600);
        assert_ne!(default_timeout_secs(graph::PHASE_ADDRESS_FINDINGS), 300);
    }

    #[test]
    fn uniform_env_overrides_canonical() {
        let _guard = test_env_lock();
        let _home = isolate_home();
        unsafe {
            std::env::set_var(ENV_PHASE_TIMEOUT_SECS, "7");
        }
        let rec = empty_record();
        assert_eq!(
            timeout_for_phase(&rec, graph::PHASE_PLAN),
            Duration::from_secs(7)
        );
        assert_eq!(
            timeout_for_phase(&rec, graph::PHASE_IMPLEMENT),
            Duration::from_secs(7)
        );
        assert_eq!(timeout_source(&rec, graph::PHASE_PLAN), TimeoutSource::Env);
        clear_home();
    }

    #[test]
    fn stub_env_does_not_apply_to_plan() {
        use crate::config::ENV_STUB_PHASE_TIMEOUT_SECS;

        let _guard = test_env_lock();
        let _home = isolate_home();
        unsafe {
            std::env::set_var(ENV_STUB_PHASE_TIMEOUT_SECS, "11");
        }
        let rec = empty_record();
        assert_eq!(
            timeout_for_phase(&rec, "stub:active"),
            Duration::from_secs(11)
        );
        assert_eq!(timeout_source(&rec, "stub:active"), TimeoutSource::Stub);
        assert_eq!(
            timeout_for_phase(&rec, graph::PHASE_PLAN),
            Duration::from_secs(1800)
        );
        assert_eq!(
            timeout_source(&rec, graph::PHASE_PLAN),
            TimeoutSource::Table
        );
        unsafe {
            std::env::remove_var(ENV_STUB_PHASE_TIMEOUT_SECS);
        }
        clear_home();
    }

    #[test]
    fn env_wins_over_project_for_all_canonical() {
        let _guard = test_env_lock();
        let _home = isolate_home();
        let mut rec = empty_record();
        rec.phase_timeouts_secs.insert(graph::PHASE_PLAN.into(), 9);
        unsafe {
            std::env::set_var(ENV_PHASE_TIMEOUT_SECS, "7");
        }
        rec.phase_timeouts_secs
            .insert(TIMEOUT_KEY_PLAN_REVIEW_SLOT.into(), 600);
        for phase in graph::canonical_phases() {
            assert_eq!(timeout_for_phase(&rec, phase), Duration::from_secs(7));
            assert_eq!(timeout_source(&rec, phase), TimeoutSource::Env);
        }
        assert_eq!(
            timeout_for_phase(&rec, TIMEOUT_KEY_PLAN_REVIEW_SLOT),
            Duration::from_secs(7)
        );
        assert_eq!(
            timeout_source(&rec, TIMEOUT_KEY_PLAN_REVIEW_SLOT),
            TimeoutSource::Env
        );
        clear_home();
    }

    #[test]
    fn project_wins_machine() {
        let _guard = test_env_lock();
        let _home = isolate_home();
        let mut cfg = MachineConfig::default();
        cfg.phase_timeouts_secs.insert(graph::PHASE_PLAN.into(), 99);
        save_machine_config(&cfg).unwrap();
        let mut rec = empty_record();
        rec.phase_timeouts_secs.insert(graph::PHASE_PLAN.into(), 11);
        assert_eq!(
            timeout_for_phase(&rec, graph::PHASE_PLAN),
            Duration::from_secs(11)
        );
        assert_eq!(
            timeout_source(&rec, graph::PHASE_PLAN),
            TimeoutSource::Project
        );
        assert_eq!(
            timeout_for_phase(&rec, graph::PHASE_IMPLEMENT),
            Duration::from_secs(7200)
        );
        assert_eq!(
            timeout_source(&rec, graph::PHASE_IMPLEMENT),
            TimeoutSource::Table
        );
        clear_home();
    }

    #[test]
    fn machine_wins_table() {
        let _guard = test_env_lock();
        let _home = isolate_home();
        let mut cfg = MachineConfig::default();
        cfg.phase_timeouts_secs
            .insert(graph::PHASE_IMPLEMENT.into(), 99);
        save_machine_config(&cfg).unwrap();
        let rec = empty_record();
        assert_eq!(
            timeout_for_phase(&rec, graph::PHASE_IMPLEMENT),
            Duration::from_secs(99)
        );
        assert_eq!(
            timeout_source(&rec, graph::PHASE_IMPLEMENT),
            TimeoutSource::Machine
        );
        assert_eq!(
            timeout_for_phase(&rec, graph::PHASE_PLAN),
            Duration::from_secs(1800)
        );
        assert_eq!(
            timeout_source(&rec, graph::PHASE_PLAN),
            TimeoutSource::Table
        );
        clear_home();
    }

    #[test]
    fn parse_phase_timeout_accepts_canonical() {
        assert_eq!(
            parse_phase_timeout(" plan = 3600 ").unwrap(),
            ("plan".into(), 3600)
        );
        assert_eq!(
            parse_phase_timeout("address-findings=3600").unwrap(),
            ("address-findings".into(), 3600)
        );
        assert_eq!(
            parse_phase_timeout("plan_review_slot=600").unwrap(),
            (TIMEOUT_KEY_PLAN_REVIEW_SLOT.into(), 600)
        );
    }

    #[test]
    fn parse_phase_timeout_rejects_zero_unknown_and_malformed() {
        assert!(parse_phase_timeout("plan=0").is_err());
        assert!(parse_phase_timeout("plan_review_slot=0").is_err());
        let unknown = parse_phase_timeout("nope=1").unwrap_err().to_string();
        assert!(
            unknown.contains("address-findings"),
            "parse error={unknown}"
        );
        assert!(
            unknown.contains("plan_review_slot"),
            "parse error={unknown}"
        );
        let unknown_key = validate_phase_timeout_key("nope").unwrap_err().to_string();
        assert!(
            unknown_key.contains("address-findings"),
            "validate error={unknown_key}"
        );
        assert!(
            unknown_key.contains("plan_review_slot"),
            "validate error={unknown_key}"
        );
        assert!(parse_phase_timeout("planner=1").is_err());
        assert!(parse_phase_timeout("xmodel=1").is_err());
        assert!(parse_phase_timeout("plan").is_err());
        assert!(parse_phase_timeout("=1").is_err());
        assert!(validate_phase_timeout_map(&BTreeMap::from([("plan".into(), 0)])).is_err());
        assert!(validate_phase_timeout_map(&BTreeMap::from([("planner".into(), 1)])).is_err());
    }

    #[test]
    fn plan_review_and_slot_table_defaults_are_2400() {
        assert_eq!(default_timeout_secs(graph::PHASE_PLAN_REVIEW), 2400);
        assert_eq!(default_timeout_secs(TIMEOUT_KEY_PLAN_REVIEW_SLOT), 2400);
        assert!(!graph::is_canonical(TIMEOUT_KEY_PLAN_REVIEW_SLOT));
        assert!(is_timeout_key(TIMEOUT_KEY_PLAN_REVIEW_SLOT));
        assert!(is_timeout_key(graph::PHASE_PLAN_REVIEW));
    }

    #[test]
    fn project_slot_overlay_wins_table_without_env() {
        let _guard = test_env_lock();
        let _home = isolate_home();
        let mut rec = empty_record();
        rec.phase_timeouts_secs
            .insert(TIMEOUT_KEY_PLAN_REVIEW_SLOT.into(), 600);
        assert_eq!(
            timeout_for_phase(&rec, TIMEOUT_KEY_PLAN_REVIEW_SLOT),
            Duration::from_secs(600)
        );
        assert_eq!(
            timeout_source(&rec, TIMEOUT_KEY_PLAN_REVIEW_SLOT),
            TimeoutSource::Project
        );
        assert_eq!(
            timeout_for_phase(&rec, graph::PHASE_PLAN_REVIEW),
            Duration::from_secs(2400)
        );
        assert_eq!(
            timeout_source(&rec, graph::PHASE_PLAN_REVIEW),
            TimeoutSource::Table
        );
        clear_home();
    }
}
