//! Per-phase wall budgets (ADR-0016).

use std::time::Duration;

use crate::config::stub_phase_timeout;

use super::graph::{self, is_canonical, is_stub_phase};

/// Uniform override for every canonical phase (tests).
pub const ENV_PHASE_TIMEOUT_SECS: &str = "COORDINATOR_PHASE_TIMEOUT_SECS";

/// Table defaults from spec (seconds).
pub fn default_timeout_secs(phase: &str) -> u64 {
    match phase {
        graph::PHASE_PLAN => 1800,
        graph::PHASE_PLAN_REVIEW => 1200,
        graph::PHASE_FOLD => 1200,
        graph::PHASE_IMPLEMENT => 7200,
        graph::PHASE_CROSS_MODEL => 2700,
        graph::PHASE_CI_WAIT => 3600,
        graph::PHASE_COMPACT => 600,
        graph::PHASE_ADVANCE => 900,
        _ => 300,
    }
}

/// Resolve the wall budget for `phase`.
///
/// 1. `stub:*` → `COORDINATOR_STUB_PHASE_TIMEOUT_SECS` / 300s
/// 2. `COORDINATOR_PHASE_TIMEOUT_SECS` uniform override (canonical only)
/// 3. Machine `phase_timeouts_secs` map, else table default
pub fn timeout_for_phase(phase: &str) -> Duration {
    if is_stub_phase(phase) {
        return stub_phase_timeout();
    }
    if is_canonical(phase)
        && let Ok(s) = std::env::var(ENV_PHASE_TIMEOUT_SECS)
        && let Ok(secs) = s.parse::<u64>()
    {
        return Duration::from_secs(secs);
    }
    let mut secs = default_timeout_secs(phase);
    if let Ok(cfg) = crate::config::load_machine_config()
        && let Some(&override_secs) = cfg.phase_timeouts_secs.get(phase)
    {
        secs = override_secs;
    }
    Duration::from_secs(secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_env_lock;

    #[test]
    fn plan_default_is_not_300s() {
        assert_eq!(default_timeout_secs(graph::PHASE_PLAN), 1800);
        assert_ne!(default_timeout_secs(graph::PHASE_PLAN), 300);
        assert_eq!(default_timeout_secs(graph::PHASE_IMPLEMENT), 7200);
        assert_eq!(default_timeout_secs(graph::PHASE_COMPACT), 600);
    }

    #[test]
    fn uniform_env_overrides_canonical() {
        let _guard = test_env_lock();
        unsafe {
            std::env::set_var(ENV_PHASE_TIMEOUT_SECS, "7");
        }
        assert_eq!(timeout_for_phase(graph::PHASE_PLAN), Duration::from_secs(7));
        assert_eq!(
            timeout_for_phase(graph::PHASE_IMPLEMENT),
            Duration::from_secs(7)
        );
        unsafe {
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
        }
    }

    #[test]
    fn stub_env_does_not_apply_to_plan() {
        let _guard = test_env_lock();
        unsafe {
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
            std::env::set_var(crate::config::ENV_STUB_PHASE_TIMEOUT_SECS, "11");
        }
        assert_eq!(timeout_for_phase("stub:active"), Duration::from_secs(11));
        assert_eq!(
            timeout_for_phase(graph::PHASE_PLAN),
            Duration::from_secs(1800)
        );
        unsafe {
            std::env::remove_var(crate::config::ENV_STUB_PHASE_TIMEOUT_SECS);
        }
    }
}
