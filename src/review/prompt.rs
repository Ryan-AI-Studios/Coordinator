//! Shipped read-only completion-audit prompt for the cross-model gate.

use std::path::Path;

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
{track_dir}You are the independent completion reviewer for a Coordinator conductor track.

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
        track_dir = track_dir_line,
        deferred = deferred.display(),
    )
}

/// JSON schema shipped to Codex `--output-schema` (file) and Claude `--json-schema`
/// (inline JSON string). OpenAI structured outputs require
/// `additionalProperties: false` and every `properties` key in `required`.
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
    use super::{VERDICT_SCHEMA_JSON, audit_prompt};
    use std::path::Path;

    #[test]
    fn audit_prompt_includes_track_line_with_resolved_path() {
        let track_dir = Path::new(r"C:\dev\Helping-Hands\conductor\0099-AutonomousPipelineProbe");
        let prompt = audit_prompt(
            Path::new(r"C:\dev\Helping-Hands"),
            Path::new(r"C:\dev\Helping-Hands\hands"),
            Some(track_dir),
            Some("0099"),
            Path::new(r"C:\dev\Helping-Hands\conductor\deferred.md"),
        );
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
        let prompt = audit_prompt(
            Path::new(r"C:\dev\Helping-Hands"),
            Path::new(r"C:\dev\Helping-Hands\hands"),
            None,
            Some("0099"),
            Path::new(r"C:\dev\Helping-Hands\conductor\deferred.md"),
        );
        assert!(
            prompt.starts_with("TRACK: 0099\n"),
            "missing first-line track-id TRACK:\n{prompt}"
        );
    }

    #[test]
    fn audit_prompt_falls_back_when_unresolved() {
        let prompt = audit_prompt(
            Path::new(r"C:\dev\Helping-Hands"),
            Path::new(r"C:\dev\Helping-Hands\hands"),
            None,
            None,
            Path::new(r"C:\dev\Helping-Hands\conductor\deferred.md"),
        );
        assert!(
            prompt.contains("TRACK: (no track directory resolved)"),
            "missing unresolved TRACK:\n{prompt}"
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
