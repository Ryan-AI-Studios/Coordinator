//! Recovery policy table (ADR-0009). Advisory text only — no auto-retry this track.

use crate::outcome::FailureClass;

/// Stable recommended-action string written onto the Failure Artifact.
pub fn recommended_action(class: FailureClass) -> &'static str {
    match class {
        FailureClass::Permission => "Fix auth or PATH; do not blind-retry.",
        FailureClass::ModelExhaustion => {
            "All cross-model tiers exhausted. Wait for quota or fix Role Bindings. Do not spin."
        }
        FailureClass::Difficulty => {
            "Re-prompt Planner/Implementor with online research; adjust approach."
        }
        FailureClass::HarnessCrash => {
            "Restart the Project session (limited retries later); inspect harness logs."
        }
        FailureClass::Timeout => "Increase the phase budget or split the work; then re-run.",
        FailureClass::CiFailed => "Do not merge. Inspect CI; re-run after fix (0010).",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_six_classes_have_stable_text() {
        assert_eq!(
            recommended_action(FailureClass::Permission),
            "Fix auth or PATH; do not blind-retry."
        );
        assert_eq!(
            recommended_action(FailureClass::ModelExhaustion),
            "All cross-model tiers exhausted. Wait for quota or fix Role Bindings. Do not spin."
        );
        assert_eq!(
            recommended_action(FailureClass::Difficulty),
            "Re-prompt Planner/Implementor with online research; adjust approach."
        );
        assert_eq!(
            recommended_action(FailureClass::HarnessCrash),
            "Restart the Project session (limited retries later); inspect harness logs."
        );
        assert_eq!(
            recommended_action(FailureClass::Timeout),
            "Increase the phase budget or split the work; then re-run."
        );
        assert_eq!(
            recommended_action(FailureClass::CiFailed),
            "Do not merge. Inspect CI; re-run after fix (0010)."
        );
    }
}
