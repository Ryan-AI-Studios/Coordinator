//! Shipped read-only completion-audit prompt for the cross-model gate.

use std::path::Path;

/// Build the one-shot audit prompt. Paths are absolute. The model must not write
/// `review.md` or planning `deferred.md` — Coordinator persists the report.
pub fn audit_prompt(
    workspace_root: &Path,
    exec_repo: &Path,
    track_dir: Option<&Path>,
    deferred: &Path,
) -> String {
    let track = track_dir
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(no track directory resolved)".into());
    format!(
        r#"You are the independent completion reviewer for a Coordinator conductor track.

READ-ONLY. Do not modify files, Git state, or write review.md / deferred.md.
Do not spawn further agents. Do not approve writes.

Planning root (read-only): {workspace}
Product / execution repo (cwd): {exec}
Track directory: {track}
Deferred register: {deferred}

Audit every Definition of Done item in the track spec against the product tree.
Flag planning docs committed into the product repo.
Ignore training-data guesses — verify against the files.

Required output heading (exact label, one of the three verdicts):

## Verdict: PASS | PASS WITH DEFERRED P3 | FAIL

Also include:

## Scope Reviewed
## Requirement and DoD Matrix
## Findings
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
        deferred = deferred.display(),
    )
}

/// JSON schema shipped to Codex `--output-schema` (and Claude `--json-schema` when used).
pub const VERDICT_SCHEMA_JSON: &str = r#"{
  "type": "object",
  "additionalProperties": true,
  "required": ["verdict"],
  "properties": {
    "verdict": {
      "type": "string",
      "enum": ["PASS", "PASS_WITH_DEFERRED_P3", "FAIL"]
    },
    "highest": { "type": "string" }
  }
}
"#;
