//! Frozen `canonical_v1` successor table and skip flags (track 0008).

pub const WORKFLOW_ID: &str = "canonical_v1";

pub const PHASE_PLAN: &str = "plan";
pub const PHASE_PLAN_REVIEW: &str = "plan-review";
pub const PHASE_FOLD: &str = "fold";
pub const PHASE_IMPLEMENT: &str = "implement";
pub const PHASE_CROSS_MODEL: &str = "cross-model-review";
pub const PHASE_CI_WAIT: &str = "ci-wait";
pub const PHASE_COMPACT: &str = "compact";
pub const PHASE_ADVANCE: &str = "advance";

pub const REVIEW_SLUG_AGY: &str = "agy";
pub const REVIEW_SLUG_OPENCODE: &str = "opencode";

pub const ROLE_PLANNER: &str = "planner";
pub const ROLE_IMPLEMENTOR: &str = "implementor";
pub const ROLE_REVIEWER_AGY: &str = "plan_reviewer_agy";
pub const ROLE_REVIEWER_OPENCODE: &str = "plan_reviewer_opencode";

/// Ordered canonical phase ids.
pub fn canonical_phases() -> &'static [&'static str] {
    &[
        PHASE_PLAN,
        PHASE_PLAN_REVIEW,
        PHASE_FOLD,
        PHASE_IMPLEMENT,
        PHASE_CROSS_MODEL,
        PHASE_CI_WAIT,
        PHASE_COMPACT,
        PHASE_ADVANCE,
    ]
}

pub fn is_canonical(phase: &str) -> bool {
    canonical_phases().contains(&phase)
}

pub fn is_stub_phase(phase: &str) -> bool {
    phase.starts_with("stub:")
}

pub fn successor(phase: &str) -> Option<&'static str> {
    match phase {
        PHASE_PLAN => Some(PHASE_PLAN_REVIEW),
        PHASE_PLAN_REVIEW => Some(PHASE_FOLD),
        PHASE_FOLD => Some(PHASE_IMPLEMENT),
        PHASE_IMPLEMENT => Some(PHASE_CROSS_MODEL),
        PHASE_CROSS_MODEL => Some(PHASE_CI_WAIT),
        PHASE_CI_WAIT => Some(PHASE_COMPACT),
        PHASE_COMPACT => Some(PHASE_ADVANCE),
        PHASE_ADVANCE => None,
        _ => None,
    }
}

pub fn is_skip_phase(phase: &str) -> bool {
    matches!(phase, PHASE_CROSS_MODEL)
}

pub fn skip_deferred_track(phase: &str) -> Option<&'static str> {
    match phase {
        PHASE_CROSS_MODEL => Some("0011"),
        _ => None,
    }
}

pub fn is_grok_bound(phase: &str) -> bool {
    matches!(
        phase,
        PHASE_PLAN | PHASE_FOLD | PHASE_IMPLEMENT | PHASE_ADVANCE
    )
}

pub fn review_slugs() -> &'static [&'static str] {
    &[REVIEW_SLUG_AGY, REVIEW_SLUG_OPENCODE]
}

pub fn role_phase(slug: &str) -> String {
    format!("{PHASE_PLAN_REVIEW}:{slug}")
}

pub fn is_recognized_role(role: &str) -> bool {
    matches!(
        role,
        ROLE_PLANNER | ROLE_IMPLEMENTOR | ROLE_REVIEWER_AGY | ROLE_REVIEWER_OPENCODE
    )
}

pub fn first_phase() -> &'static str {
    PHASE_PLAN
}

/// `{conductor_dir}/{track_id}` if that dir exists; else first `{track_id}-*` directory.
pub fn resolve_track_dir(
    record: &crate::registry::ProjectRecord,
    track_id: &str,
) -> Option<std::path::PathBuf> {
    let conductor = crate::layout::resolve(record).conductor_dir;
    if !conductor.is_dir() {
        return None;
    }
    let exact = conductor.join(track_id);
    if exact.is_dir() {
        return Some(exact);
    }
    let prefix = format!("{track_id}-");
    let mut found = Vec::new();
    let Ok(rd) = std::fs::read_dir(&conductor) else {
        return None;
    };
    for ent in rd.flatten() {
        let p = ent.path();
        if !p.is_dir() {
            continue;
        }
        if let Some(name) = p.file_name().and_then(|n| n.to_str())
            && name.starts_with(&prefix)
        {
            found.push(p);
        }
    }
    found.sort();
    found.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successor_table_walks_full_graph() {
        let mut phase = first_phase();
        let mut seen = vec![phase];
        while let Some(next) = successor(phase) {
            seen.push(next);
            phase = next;
        }
        assert_eq!(seen, canonical_phases());
        assert_eq!(successor(PHASE_ADVANCE), None);
    }

    #[test]
    fn skip_slot_is_0011_only() {
        assert!(is_skip_phase(PHASE_CROSS_MODEL));
        assert!(!is_skip_phase(PHASE_CI_WAIT));
        assert_eq!(skip_deferred_track(PHASE_CROSS_MODEL), Some("0011"));
        assert_eq!(skip_deferred_track(PHASE_CI_WAIT), None);
        assert!(!is_skip_phase(PHASE_PLAN));
        assert!(!is_skip_phase(PHASE_COMPACT));
        assert!(!is_grok_bound(PHASE_CI_WAIT));
    }

    #[test]
    fn stub_vs_canonical() {
        assert!(is_canonical(PHASE_PLAN));
        assert!(!is_canonical("stub:active"));
        assert!(is_stub_phase("stub:failed"));
        assert!(!is_stub_phase(PHASE_PLAN));
    }
}
