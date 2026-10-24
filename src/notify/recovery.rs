//! Recovery policy table (ADR-0009). Advisory text. Automatic recovery has stopped
//! once `FAILURE.md` exists. An ACP stdout close or a transport drop restarts the
//! session only before that file is written (bounded). Opt-in `address-ci` (0071) may route the ci-wait gate's
//! failing checks before that file exists; opt-in `address-review` (0078) may route owned-PR review
//! comments, and the engine delivers its commit with three bounded push attempts. Nothing else
//! auto-retries, and `FAILURE.md` ends auto-retry.

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
            "For an ACP stdout close or a transport drop, bounded session restart already ran. Inspect exit status and stderr here. Other harness crashes are not auto-retried."
        }
        FailureClass::Timeout => "Increase the phase budget or split the work; then re-run.",
        FailureClass::CiFailed => {
            "Do not merge. Inspect CI; re-run after fix (0010). Opt-in address-ci is off or its cap is exhausted (0071). If `last_event` begins `ci-push:`, the CI fix is committed on the track branch but not delivered - fix the named push error and push it, or push the branch by hand, before re-running; a fresh run starts at `plan` and does not re-push. A `ci-push: PR #N merged|closed` stop means the PR ended before the fix landed - reopen or open a new PR for that branch (0125)."
        }
        FailureClass::SkillMissing => {
            "Create the missing skill file or set skill_aliases; do not blind-retry."
        }
        FailureClass::ReviewNeverSettles => {
            "Do not blind-retry: the fold keeps rewriting the artifacts the last review passed. Read the fold's changes and re-review the amended pair, or stop the fold rewriting spec/plan."
        }
        FailureClass::Gate => {
            "Do not re-drive the track: it reached the fold without a usable plan.md, or with a review still asking for a re-plan. Write or re-review the artifacts, and resolve any owner decision the leg was waiting on."
        }
        FailureClass::Blocked => {
            "Resolve the prerequisite the leg named, then `coordinator failure resolve` and start a new `coordinator run --track <id>`. No blocked repair was attempted, and the block never advances."
        }
        FailureClass::ReviewComments => {
            "Do not merge. Resolve or address the open review comments on the PR, then `failure resolve` and start a new `run --track <id>`. If `last_event` begins `review-push:`, the review fix is committed on the track branch but not delivered - fix the named push error and push it, or push the branch by hand, before re-running; a fresh run starts at `plan` and does not re-push. A `review-push: PR #N merged|closed` stop means the PR ended before the fix landed - reopen or open a new PR for that branch. Routing may also be off, capped, or the review surface could not be read (0078)."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_classes_have_stable_text() {
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
            "For an ACP stdout close or a transport drop, bounded session restart already ran. Inspect exit status and stderr here. Other harness crashes are not auto-retried."
        );
        assert_eq!(
            recommended_action(FailureClass::Timeout),
            "Increase the phase budget or split the work; then re-run."
        );
        assert_eq!(
            recommended_action(FailureClass::CiFailed),
            "Do not merge. Inspect CI; re-run after fix (0010). Opt-in address-ci is off or its cap is exhausted (0071). If `last_event` begins `ci-push:`, the CI fix is committed on the track branch but not delivered - fix the named push error and push it, or push the branch by hand, before re-running; a fresh run starts at `plan` and does not re-push. A `ci-push: PR #N merged|closed` stop means the PR ended before the fix landed - reopen or open a new PR for that branch (0125)."
        );
        assert_eq!(
            recommended_action(FailureClass::SkillMissing),
            "Create the missing skill file or set skill_aliases; do not blind-retry."
        );
        assert!(
            recommended_action(FailureClass::ReviewComments)
                .contains("Resolve or address the open review comments")
        );
        assert!(
            recommended_action(FailureClass::ReviewComments)
                .contains("a fresh run starts at `plan` and does not re-push")
        );
    }
}
