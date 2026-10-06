//! Recovery policy table (ADR-0009). Advisory text. Automatic recovery has stopped
//! once `FAILURE.md` exists. An ACP stdout close restarts the session only before
//! that file is written (bounded). Opt-in `address-ci` (0071) may route the ci-wait gate's
//! failing checks before that file exists. Nothing else auto-retries, and `FAILURE.md` ends auto-retry.

use crate::outcome::FailureClass;

/// Stable recommended-action string written onto the Failure Artifact.
pub fn recommended_action(class: FailureClass) -> &'static str {
    match class {
        FailureClass::Permission => "Fix auth or PATH; do not blind-retry.",
        FailureClass::ModelExhaustion => {
            "Wait for quota or credits, or switch Role Bindings. Do not spin."
        }
        FailureClass::Difficulty => {
            "Re-prompt Planner/Implementor with online research; adjust approach."
        }
        FailureClass::HarnessCrash => {
            "For an ACP stdout close, bounded session restart already ran. Inspect exit status and stderr here. Other harness crashes are not auto-retried."
        }
        FailureClass::Timeout => "Increase the phase budget or split the work; then re-run.",
        FailureClass::CiFailed => {
            "Do not merge. Inspect CI; re-run after fix (0010). Opt-in address-ci is off or its cap is exhausted (0071)."
        }
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
            "Wait for quota or credits, or switch Role Bindings. Do not spin."
        );
        assert_eq!(
            recommended_action(FailureClass::Difficulty),
            "Re-prompt Planner/Implementor with online research; adjust approach."
        );
        assert_eq!(
            recommended_action(FailureClass::HarnessCrash),
            "For an ACP stdout close, bounded session restart already ran. Inspect exit status and stderr here. Other harness crashes are not auto-retried."
        );
        assert_eq!(
            recommended_action(FailureClass::Timeout),
            "Increase the phase budget or split the work; then re-run."
        );
        assert_eq!(
            recommended_action(FailureClass::CiFailed),
            "Do not merge. Inspect CI; re-run after fix (0010). Opt-in address-ci is off or its cap is exhausted (0071)."
        );
    }
}
