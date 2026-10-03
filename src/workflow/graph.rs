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
/// Apply-path side loop after GateFail (0031). Not in [`canonical_phases`].
pub const PHASE_ADDRESS_FINDINGS: &str = "address-findings";
/// Max entries into `address-findings` this `run_epoch`. No operator env.
pub const ADDRESS_FINDINGS_CAP: u32 = 2;

pub const REVIEW_SLUG_AGY: &str = "agy";
pub const REVIEW_SLUG_OPENCODE: &str = "opencode";

pub const ROLE_PLANNER: &str = "planner";
pub const ROLE_IMPLEMENTOR: &str = "implementor";
pub const ROLE_REVIEWER_AGY: &str = "plan_reviewer_agy";
pub const ROLE_REVIEWER_OPENCODE: &str = "plan_reviewer_opencode";
pub const ROLE_CROSS_MODEL_PRIMARY: &str = "cross_model_primary";
pub const ROLE_CROSS_MODEL_SECONDARY: &str = "cross_model_secondary";
pub const ROLE_CROSS_MODEL_TERTIARY: &str = "cross_model_tertiary";

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

/// Happy-path ids plus the `address-findings` side loop.
pub fn all_phase_ids() -> &'static [&'static str] {
    &[
        PHASE_PLAN,
        PHASE_PLAN_REVIEW,
        PHASE_FOLD,
        PHASE_IMPLEMENT,
        PHASE_CROSS_MODEL,
        PHASE_CI_WAIT,
        PHASE_COMPACT,
        PHASE_ADVANCE,
        PHASE_ADDRESS_FINDINGS,
    ]
}

pub fn is_canonical(phase: &str) -> bool {
    all_phase_ids().contains(&phase)
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
        PHASE_ADDRESS_FINDINGS => Some(PHASE_CROSS_MODEL),
        _ => None,
    }
}

/// No skip slots remain after 0011. Kept as a hook for a future skip profile.
pub fn is_skip_phase(_phase: &str) -> bool {
    false
}

/// No skip slots remain after 0011.
pub fn skip_deferred_track(_phase: &str) -> Option<&'static str> {
    None
}

/// Ordered cross-model Role Binding keys (not BTreeMap iteration).
pub fn cross_model_roles() -> &'static [&'static str] {
    &[
        ROLE_CROSS_MODEL_PRIMARY,
        ROLE_CROSS_MODEL_SECONDARY,
        ROLE_CROSS_MODEL_TERTIARY,
    ]
}

pub fn is_grok_bound(phase: &str) -> bool {
    matches!(
        phase,
        PHASE_PLAN | PHASE_FOLD | PHASE_IMPLEMENT | PHASE_ADVANCE | PHASE_ADDRESS_FINDINGS
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
        ROLE_PLANNER
            | ROLE_IMPLEMENTOR
            | ROLE_REVIEWER_AGY
            | ROLE_REVIEWER_OPENCODE
            | ROLE_CROSS_MODEL_PRIMARY
            | ROLE_CROSS_MODEL_SECONDARY
            | ROLE_CROSS_MODEL_TERTIARY
    )
}

pub fn first_phase() -> &'static str {
    PHASE_PLAN
}

/// Digits-only id, or a case-insensitive `track` prefix plus digits (`track72` → `72`).
///
/// A slug (`0065-Name`, `track72-foo`) has no bare form. Callers still try the raw id.
fn bare_track_digits(track_id: &str) -> Option<&str> {
    let id = track_id.trim();
    let rest = if id.len() >= 5 && id[..5].eq_ignore_ascii_case("track") {
        &id[5..]
    } else {
        id
    };
    if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
        Some(rest)
    } else {
        None
    }
}

fn existing_child_dir(conductor: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    let path = conductor.join(name);
    path.is_dir().then_some(path)
}

/// First directory whose name starts with `prefix`, in sorted path order.
fn first_prefixed_dir(conductor: &std::path::Path, prefix: &str) -> Option<std::path::PathBuf> {
    let mut found = Vec::new();
    let rd = std::fs::read_dir(conductor).ok()?;
    for ent in rd.flatten() {
        let path = ent.path();
        if !path.is_dir() {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && name.starts_with(prefix)
        {
            found.push(path);
        }
    }
    found.sort();
    found.into_iter().next()
}

/// Resolve `track_id` to a conductor directory.
///
/// Order: exact name, bare digits, `track{digits}`, `{raw}-` prefix, `{digits}-`
/// prefix, then a 4-digit zero-pad when the digits are shorter than 4. Prefix
/// ties take the first sorted name. `None` means no directory.
pub fn resolve_track_dir(
    record: &crate::registry::ProjectRecord,
    track_id: &str,
) -> Option<std::path::PathBuf> {
    let conductor = crate::layout::resolve(record).conductor_dir;
    if !conductor.is_dir() {
        return None;
    }
    if let Some(exact) = existing_child_dir(&conductor, track_id) {
        return Some(exact);
    }
    let norm = bare_track_digits(track_id);
    if let Some(norm) = norm {
        if norm != track_id
            && let Some(bare) = existing_child_dir(&conductor, norm)
        {
            return Some(bare);
        }
        let track_name = format!("track{norm}");
        if track_name != track_id
            && let Some(prefixed) = existing_child_dir(&conductor, &track_name)
        {
            return Some(prefixed);
        }
    }
    let raw_prefix = format!("{track_id}-");
    if let Some(hit) = first_prefixed_dir(&conductor, &raw_prefix) {
        return Some(hit);
    }
    let norm = norm?;
    let norm_prefix = format!("{norm}-");
    if norm_prefix != raw_prefix
        && let Some(hit) = first_prefixed_dir(&conductor, &norm_prefix)
    {
        return Some(hit);
    }
    if norm.len() < 4 {
        let padded = format!("{norm:0>4}");
        if padded != norm {
            if let Some(exact) = existing_child_dir(&conductor, &padded) {
                return Some(exact);
            }
            if let Some(hit) = first_prefixed_dir(&conductor, &format!("{padded}-")) {
                return Some(hit);
            }
        }
    }
    None
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
    fn no_skip_slots_remain() {
        for phase in canonical_phases() {
            assert!(!is_skip_phase(phase), "skip leftover on {phase}");
            assert_eq!(skip_deferred_track(phase), None);
        }
        assert!(!is_grok_bound(PHASE_CI_WAIT));
        assert!(!is_grok_bound(PHASE_CROSS_MODEL));
        assert!(is_recognized_role(ROLE_CROSS_MODEL_PRIMARY));
        assert!(is_recognized_role(ROLE_CROSS_MODEL_SECONDARY));
        assert!(is_recognized_role(ROLE_CROSS_MODEL_TERTIARY));
        assert_eq!(
            cross_model_roles(),
            [
                ROLE_CROSS_MODEL_PRIMARY,
                ROLE_CROSS_MODEL_SECONDARY,
                ROLE_CROSS_MODEL_TERTIARY
            ]
        );
    }

    #[test]
    fn stub_vs_canonical() {
        assert!(is_canonical(PHASE_PLAN));
        assert!(!is_canonical("stub:active"));
        assert!(is_stub_phase("stub:failed"));
        assert!(!is_stub_phase(PHASE_PLAN));
    }

    #[test]
    fn address_findings_is_canonical_side_loop() {
        assert!(is_canonical(PHASE_ADDRESS_FINDINGS));
        assert_eq!(successor(PHASE_ADDRESS_FINDINGS), Some(PHASE_CROSS_MODEL));
        assert!(is_grok_bound(PHASE_ADDRESS_FINDINGS));
        assert!(!canonical_phases().contains(&PHASE_ADDRESS_FINDINGS));
        assert_eq!(ADDRESS_FINDINGS_CAP, 2);
        assert!(all_phase_ids().contains(&PHASE_ADDRESS_FINDINGS));
        assert_eq!(all_phase_ids().len(), canonical_phases().len() + 1);
    }

    fn record_at(path: &std::path::Path) -> crate::registry::ProjectRecord {
        crate::registry::ProjectRecord {
            id: "0065-test".into(),
            path: path.to_path_buf(),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: std::collections::BTreeMap::new(),
            state_dir: None,
            auto_merge: true,
            phase_timeouts_secs: std::collections::BTreeMap::new(),
            notify_progress: false,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            created_at: chrono::Utc::now(),
        }
    }

    fn mkdir(root: &std::path::Path, name: &str) {
        std::fs::create_dir_all(root.join("conductor").join(name)).unwrap();
    }

    fn assert_resolved(root: &std::path::Path, id: &str, expect: Option<&str>) {
        let got = resolve_track_dir(&record_at(root), id);
        match expect {
            None => assert!(got.is_none(), "id {id} resolved {}", got.unwrap().display()),
            Some(name) => {
                let got = got
                    .unwrap_or_else(|| panic!("id {id} resolved None"))
                    .canonicalize()
                    .unwrap();
                let expect = root.join("conductor").join(name).canonicalize().unwrap();
                assert_eq!(got, expect, "id {id}");
            }
        }
    }

    #[test]
    fn bare_track_digits_table() {
        assert_eq!(bare_track_digits("72"), Some("72"));
        assert_eq!(bare_track_digits("track72"), Some("72"));
        assert_eq!(bare_track_digits("Track72"), Some("72"));
        assert_eq!(bare_track_digits("TRACK72"), Some("72"));
        assert_eq!(bare_track_digits("0065"), Some("0065"));
        assert_eq!(bare_track_digits("  track72  "), Some("72"));
        assert_eq!(bare_track_digits("track72-foo"), None);
        assert_eq!(bare_track_digits("0065-Slug"), None);
        assert_eq!(bare_track_digits("track"), None);
        assert_eq!(bare_track_digits(""), None);
    }

    #[test]
    fn resolve_track_dir_notation_table() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        mkdir(root, "track72");
        assert_resolved(root, "72", Some("track72"));
        assert_resolved(root, "TRACK72", Some("track72"));
        assert_resolved(root, "track72", Some("track72"));

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        mkdir(root, "72");
        assert_resolved(root, "track72", Some("72"));

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        mkdir(root, "0065-PlanReviewAdoptionTrackId");
        assert_resolved(root, "0065", Some("0065-PlanReviewAdoptionTrackId"));
        assert_resolved(
            root,
            "0065-PlanReviewAdoptionTrackId",
            Some("0065-PlanReviewAdoptionTrackId"),
        );
        assert_resolved(root, "65", Some("0065-PlanReviewAdoptionTrackId"));

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        mkdir(root, "track72");
        mkdir(root, "72-Real");
        assert_resolved(root, "72", Some("track72"));
        assert_resolved(root, "track72", Some("track72"));

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        mkdir(root, "track72");
        mkdir(root, "0072-Other");
        assert_resolved(root, "72", Some("track72"));

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        mkdir(root, "65-Old");
        mkdir(root, "0065-New");
        assert_resolved(root, "65", Some("65-Old"));

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        mkdir(root, "track720");
        mkdir(root, "720-Foo");
        assert_resolved(root, "72", None);

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        mkdir(root, "track72-foo");
        assert_resolved(root, "track72-foo", Some("track72-foo"));
        assert_resolved(root, "72", None);
    }
}
