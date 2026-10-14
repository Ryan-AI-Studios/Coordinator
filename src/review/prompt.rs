//! Shipped read-only completion-audit prompt for the cross-model gate.

use std::path::Path;

/// Phase, review instant, and address-findings count for one audit prompt.
///
/// Bundled so `audit_prompt` stays at or under Clippy's default
/// `too_many_arguments` threshold (7). Do not add an allow attribute.
pub struct AuditPromptMeta<'a> {
    pub phase: &'a str,
    pub reviewed_at: &'a str,
    pub address_findings_attempts: u32,
}

/// Build the one-shot audit prompt. Paths are absolute. The model must not write
/// `review.md` or planning `deferred.md` — Coordinator persists the report.
///
/// `TRACK:` is the required Codex review-skill handoff: resolved track directory
/// if present, else the track id, else an explicit unresolved placeholder.
pub fn audit_prompt(
    workspace_root: &Path,
    exec_repo: &Path,
    track_dir: Option<&Path>,
    track_id: Option<&str>,
    deferred: &Path,
    meta: &AuditPromptMeta<'_>,
) -> String {
    // Project `codex-review` skills require this as a first-line handoff
    // (`####-Name` or absolute conductor track dir). Prefer the folder name.
    let track = track_dir
        .and_then(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .or_else(|| {
            track_id
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .or_else(|| track_dir.map(|p| p.display().to_string()))
        .unwrap_or_else(|| "(no track directory resolved)".into());
    let track_dir_line = track_dir
        .map(|p| format!("TRACK_DIR: {}\n", p.display()))
        .unwrap_or_default();
    format!(
        r#"TRACK: {track}
{track_dir}PHASE: {phase}
REVIEWED_AT: {reviewed_at}
ADDRESS_FINDINGS_ATTEMPTS: {attempts}
You are the independent completion reviewer for a Coordinator conductor track.

READ-ONLY. Do not modify files, Git state, or write review.md / deferred.md.
Do not spawn further agents. Do not approve writes.

Planning root (read-only): {workspace}
Product / execution repo (cwd): {exec}
Deferred register: {deferred}

Scope is this TRACK only. Audit that track spec's Definition of Done
(planning-only probe: evidence.md in the track directory and a clean
execution-repo git tree). Do not treat missing AI_BRAINS_KEY as a gate fail.
Do not select or review sibling tracks (do not pick Helping Hands 0001–0013).

Audit every Definition of Done item in the track spec against the product tree.
Downstream lifecycle is not this gate. Do NOT fail the gate and do NOT log
P0/P1/P2 findings for steps owned by later phases (ci-wait, auto-merge,
advance): PR squash-merge not landed; conductor.md / cluster registry not
Completed; required CI pending or in progress. In a read-only session, do
not fail solely because gh or cargo was denied if local product code, tests,
and evidence.md are present. Shipping artifacts are a non-empty
evidence.md, a clean execution-repo worktree, and an EXEC commit.
In flight only when ADDRESS_FINDINGS_ATTEMPTS is 0 and the track spec
forbids product edits until a measurement or other prerequisite exists.
Do not treat PHASE values implement, plan, fold, or plan-review as live
switches. Do not treat a stop-before-commit sentence as a trigger. An
ordinary spec that required this turn to ship is not in flight, even when
evidence.md is missing. In flight, do not emit P0, P1, or P2 for a missing
or empty evidence.md, a dirty execution-repo worktree, or no EXEC commit.
Record each gap under ## Verification Evidence using REVIEWED_AT, for
example `evidence.md` not present as of {reviewed_at}. That observation
does not change the verdict. Shipping is due when ADDRESS_FINDINGS_ATTEMPTS
is >= 1, or when the count is 0 and the spec required this turn to ship.
In that branch, still FAIL for missing or empty evidence.md, a dirty
execution-repo worktree, or no EXEC commit. A product defect FAILs in both
branches. A lockfile that retargets packages outside the allowed set FAILs
even when nothing has been committed. Do not shorten or down-rank any
finding outside the shipping-absence class. Live spawns pass
PHASE: cross-model-review, so the phase string alone is not proof that
shipping artifacts are due. Put merge, CI, and registry state only under ## Publish Status
(never scored, never P0–P2). If the only remaining gaps are
publish/lifecycle, the verdict is PASS or PASS WITH DEFERRED P3 — never FAIL.
Do not treat conductor.md or cluster registry Completed as a review-gate
blocker.
Flag planning docs committed into the product repo.
Ignore training-data guesses — verify against the files.

Required output heading (exact label, one of the three verdicts):

## Verdict: PASS | PASS WITH DEFERRED P3 | FAIL

Also include:

## Scope Reviewed
## Requirement and DoD Matrix
## Findings
## Publish Status
## Completeness Sweep
## Wiring and Regression Review
## Verification Evidence
## Deferred Candidates
## Completion Decision

Severity: P0/P1/P2 (critical/high/medium) fail the gate. P3/low may be listed
under Findings when the verdict is PASS WITH DEFERRED P3.
Do not put a skill legend under ## Findings.
"#,
        workspace = workspace_root.display(),
        exec = exec_repo.display(),
        track = track,
        track_dir = track_dir_line,
        deferred = deferred.display(),
        phase = meta.phase,
        reviewed_at = meta.reviewed_at,
        attempts = meta.address_findings_attempts,
    )
}

/// JSON schema shipped to Codex `--output-schema` (file). Claude no longer
/// receives `--json-schema` (0043); the markdown prompt is the contract.
/// OpenAI structured outputs require `additionalProperties: false` and every
/// `properties` key in `required`.
pub const VERDICT_SCHEMA_JSON: &str = r#"{
  "type": "object",
  "additionalProperties": false,
  "required": ["verdict", "highest"],
  "properties": {
    "verdict": {
      "type": "string",
      "enum": ["PASS", "PASS_WITH_DEFERRED_P3", "FAIL"]
    },
    "highest": { "type": "string" }
  }
}
"#;

#[cfg(test)]
mod tests {
    use super::{AuditPromptMeta, VERDICT_SCHEMA_JSON, audit_prompt};
    use std::path::Path;

    const REVIEWED_AT: &str = "2026-10-14T01:13:36Z";

    fn sample(
        track_dir: Option<&Path>,
        track_id: Option<&str>,
        phase: &str,
        attempts: u32,
    ) -> String {
        let m = AuditPromptMeta {
            phase,
            reviewed_at: REVIEWED_AT,
            address_findings_attempts: attempts,
        };
        audit_prompt(
            Path::new(r"C:\dev\Helping-Hands"),
            Path::new(r"C:\dev\Helping-Hands\hands"),
            track_dir,
            track_id,
            Path::new(r"C:\dev\Helping-Hands\conductor\deferred.md"),
            &m,
        )
    }

    #[test]
    fn audit_prompt_includes_track_line_with_resolved_path() {
        let track_dir = Path::new(r"C:\dev\Helping-Hands\conductor\0099-AutonomousPipelineProbe");
        let prompt = sample(Some(track_dir), Some("0099"), "cross-model-review", 0);
        assert!(
            prompt.starts_with("TRACK: 0099-AutonomousPipelineProbe\n"),
            "TRACK ####-Name must be first line:\n{prompt}"
        );
        assert!(
            prompt.contains(&format!("TRACK_DIR: {}", track_dir.display())),
            "missing TRACK_DIR:\n{prompt}"
        );
        assert!(prompt.contains("## Verdict: PASS | PASS WITH DEFERRED P3 | FAIL"));
        assert!(prompt.contains("Do not treat missing AI_BRAINS_KEY as a gate fail"));
        assert!(prompt.contains("do not pick Helping Hands 0001–0013"));
    }

    #[test]
    fn audit_prompt_falls_back_to_track_id() {
        let prompt = sample(None, Some("0099"), "cross-model-review", 0);
        assert!(
            prompt.starts_with("TRACK: 0099\nPHASE: cross-model-review\n"),
            "PHASE must follow TRACK when track_dir is None:\n{prompt}"
        );
    }

    #[test]
    fn audit_prompt_falls_back_when_unresolved() {
        let prompt = sample(None, None, "cross-model-review", 0);
        assert!(
            prompt.contains("TRACK: (no track directory resolved)"),
            "missing unresolved TRACK:\n{prompt}"
        );
    }

    #[test]
    fn audit_prompt_forbids_publish_dod_as_gate_fail() {
        let prompt = sample(
            Some(Path::new(
                r"C:\dev\Helping-Hands\conductor\0099-AutonomousPipelineProbe",
            )),
            Some("0099"),
            "cross-model-review",
            0,
        );
        assert!(prompt.contains("## Verdict: PASS | PASS WITH DEFERRED P3 | FAIL"));
        assert!(prompt.contains("Do not treat missing AI_BRAINS_KEY as a gate fail"));
        assert!(prompt.contains("do not pick Helping Hands 0001–0013"));
        assert!(prompt.contains("## Findings"));
        assert!(prompt.contains("## Publish Status"));
        assert!(prompt.contains("squash-merge"));
        assert!(prompt.contains("Completed"));
        assert!(prompt.contains("required CI"));
        assert!(prompt.contains("never FAIL"));
        assert!(prompt.contains("dirty execution-repo worktree"));
        assert!(prompt.contains("Do not treat conductor.md or cluster registry Completed"));
    }

    #[test]
    fn audit_prompt_records_phase_timestamp_and_attempts() {
        let track_dir = Path::new(r"C:\dev\Helping-Hands\conductor\0099-AutonomousPipelineProbe");
        let live = sample(Some(track_dir), Some("0099"), "cross-model-review", 0);
        assert!(live.starts_with("TRACK:"));
        assert!(live.contains("PHASE: cross-model-review\n"));
        assert!(live.contains("REVIEWED_AT: 2026-10-14T01:13:36Z\n"));
        assert!(live.contains("ADDRESS_FINDINGS_ATTEMPTS: 0\n"));
        // Header format only. Live `tick` does not call this with `implement`.
        let header = sample(Some(track_dir), Some("0099"), "implement", 0);
        assert!(header.starts_with("TRACK:"));
        assert!(header.contains("PHASE: implement\n"));
    }

    #[test]
    fn audit_prompt_in_flight_observation_uses_reviewed_at() {
        let prompt = sample(None, Some("0092"), "cross-model-review", 0);
        assert!(prompt.contains("## Verification Evidence"));
        assert!(prompt.contains("`evidence.md` not present as of 2026-10-14T01:13:36Z"));
        assert!(
            prompt
                .contains("forbids product edits until a measurement or other prerequisite exists")
        );
        assert!(
            prompt.contains(
                "Do not treat PHASE values implement, plan, fold, or plan-review as live"
            )
        );
        assert!(prompt.contains("Do not treat a stop-before-commit sentence as a trigger."));
        assert!(prompt.contains("not in flight"));
        assert!(!prompt.contains("Still FAIL when implement has not shipped"));
    }

    #[test]
    fn audit_prompt_after_address_findings_keeps_gate2() {
        let prompt = sample(None, Some("0092"), "cross-model-review", 1);
        assert!(prompt.contains("ADDRESS_FINDINGS_ATTEMPTS: 1\n"));
        assert!(prompt.contains("still FAIL for missing or empty evidence.md"));
        assert!(prompt.contains("dirty execution-repo worktree"));
        assert!(prompt.contains("EXEC commit"));
    }

    /// `24_000` is a proxy. Claude also passes fixed short flags. The
    /// CreateProcess limit is about 32_767 characters for the whole command
    /// line. This test does not assemble `claude_argv`.
    #[test]
    fn audit_prompt_stays_under_windows_argv_budget() {
        let track_dir = Path::new(r"C:\dev\Helping-Hands\conductor\0099-AutonomousPipelineProbe");
        let prompt = sample(Some(track_dir), Some("0099"), "cross-model-review", 0);
        assert!(
            prompt.len() < 24_000,
            "prompt is {} bytes; 24_000 is a proxy for the Windows command line",
            prompt.len()
        );
    }

    #[test]
    fn verdict_schema_meets_openai_structured_output_rules() {
        let v: serde_json::Value = serde_json::from_str(VERDICT_SCHEMA_JSON).unwrap();
        assert_eq!(v["additionalProperties"], false);
        let props: Vec<&str> = v["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let required: Vec<&str> = v["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|x| x.as_str())
            .collect();
        for key in props {
            assert!(
                required.contains(&key),
                "property {key} must be in required for Codex --output-schema"
            );
        }
    }
}
