//! Terminal block declaration grammar (track 0098).
//!
//! A leg may declare that it cannot complete the phase by ending its reply with a
//! column-zero `Blocked: <reason>` line, or `<current-phase> blocked: <reason>`.
//! Classification reads **only** the current turn text. It never reads evidence,
//! receipts, owner markers or product files, and it never replays transcript history.
//!
//! The grammar is deliberately narrow so a successful turn cannot be mistaken for a
//! block by ordinary prose:
//!
//! * both prefixes are ASCII case-insensitive; the phase arm must be the canonical
//!   `graph.rs` phase id (no underscore alias, no wrong phase);
//! * the reason is trimmed, non-empty, at most [`REASON_MAX_SCALARS`] Unicode scalars,
//!   contains no control character (checked **before** trimming, so a tab is rejected),
//!   and contains no `<...>` placeholder pair;
//! * the last eligible declaration wins;
//! * everything after it must be blank or at most [`TAIL_MAX_PARAGRAPHS`] recovery
//!   paragraphs (starting `No files changed;`, `Resume when ` or
//!   `The [ai-brains skill](`) totalling at most [`TAIL_MAX_SCALARS`] scalars;
//! * phase control tails (`self-check:` / `next_track:`) are stripped with the shipped
//!   parsers before terminality is judged, but a block discards those directives.
//!
//! The known limitation is documented in the README: unmarked narration that happens
//! to contain a terminal line can stop the leg. There is no natural-language classifier.

use crate::workflow::graph::{
    PHASE_ADDRESS_CI, PHASE_ADDRESS_FINDINGS, PHASE_ADDRESS_REVIEW, PHASE_ADVANCE, PHASE_FOLD,
    PHASE_IMPLEMENT, PHASE_PLAN,
};

/// Longest accepted reason, in Unicode scalars.
pub const REASON_MAX_SCALARS: usize = 512;
/// Largest accepted post-declaration tail, in Unicode scalars (separators included).
pub const TAIL_MAX_SCALARS: usize = 4096;
/// Largest number of non-empty recovery paragraphs after a declaration.
pub const TAIL_MAX_PARAGRAPHS: usize = 3;

const DECISION_OPEN: &str = "<!-- coordinator:decision ";
const DECISION_CLOSE: &str = "<!-- /coordinator:decision -->";

const EXAMPLE_CUES: [&str; 4] = ["Example:", "Examples:", "For example:", "Sample output:"];

const RECOVERY_PREFIXES: [&str; 3] = [
    "No files changed;",
    "Resume when ",
    "The [ai-brains skill](",
];

/// Canonical phases that may declare a block.
pub fn phases() -> &'static [&'static str] {
    &[
        PHASE_PLAN,
        PHASE_FOLD,
        PHASE_IMPLEMENT,
        PHASE_ADDRESS_FINDINGS,
        PHASE_ADDRESS_CI,
        PHASE_ADDRESS_REVIEW,
        PHASE_ADVANCE,
    ]
}

/// True when a leg in `phase` is allowed to declare a block.
pub fn phase_allowed(phase: &str) -> bool {
    phases().contains(&phase)
}

/// Identity capture is block-only: Adapter driver, a six-phase `is_grok_bound` phase,
/// and a Running run. A manual prompt into any other state never acquires a block identity.
pub fn capture_allowed(state: &crate::state::RunState) -> bool {
    state.driver == crate::workflow::WorkflowDriver::Adapter
        && crate::workflow::graph::is_grok_bound(&state.phase)
        && state.status == crate::state::RunStatus::Running
}

/// One eligible terminal declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    pub reason: String,
}

/// Classify the current turn text. `Some(reason)` only when the last eligible
/// declaration is terminal for `phase`.
pub fn classify(text: &str, phase: &str) -> Option<String> {
    classify_declaration(text, phase).map(|d| d.reason)
}

pub fn classify_declaration(text: &str, phase: &str) -> Option<Declaration> {
    if !phase_allowed(phase) {
        return None;
    }
    let normalized = text.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    let excluded = excluded_lines(&lines);

    let mut last: Option<(usize, String)> = None;
    for (i, line) in lines.iter().enumerate() {
        if excluded[i] {
            continue;
        }
        if let Some(reason) = parse_declaration_line(line, phase) {
            last = Some((i, reason));
        }
    }
    let (index, reason) = last?;
    let tail: Vec<&str> = lines[index + 1..].to_vec();
    let tail = strip_control_tail(tail, phase);
    terminal_tail(&tail).then_some(Declaration { reason })
}

fn excluded_lines(lines: &[&str]) -> Vec<bool> {
    let mut out = vec![false; lines.len()];

    // Coordinator decision blocks are excluded (active and superseded). An unclosed
    // opener excludes the remaining text. This is exclusion only, never authentication.
    let mut in_decision = false;
    for (i, line) in lines.iter().enumerate() {
        if in_decision {
            out[i] = true;
            if line.contains(DECISION_CLOSE) {
                in_decision = false;
            }
            continue;
        }
        if line.contains(DECISION_OPEN) {
            out[i] = true;
            in_decision = !line.contains(DECISION_CLOSE);
        }
    }

    // Fenced code (backtick or tilde) is never a declaration, and an unclosed fence
    // consumes the rest of the message.
    let mut fence: Option<char> = None;
    for (i, line) in lines.iter().enumerate() {
        if out[i] {
            continue;
        }
        let trimmed = line.trim_start();
        let marker = fence_marker(trimmed);
        match fence {
            Some(c) => {
                out[i] = true;
                if marker == Some(c) && is_fence_line(trimmed, c) {
                    fence = None;
                }
            }
            None => {
                if let Some(c) = marker {
                    out[i] = true;
                    fence = Some(c);
                }
            }
        }
    }

    // Structurally marked examples are excluded from the cue line through the next
    // blank line, including a declaration after a single newline.
    for i in 0..lines.len() {
        if out[i] {
            continue;
        }
        let trimmed = lines[i].trim();
        if EXAMPLE_CUES
            .iter()
            .any(|cue| trimmed.eq_ignore_ascii_case(cue))
        {
            out[i] = true;
            let mut j = i + 1;
            while j < lines.len() && !lines[j].trim().is_empty() {
                out[j] = true;
                j += 1;
            }
        }
    }

    out
}

fn fence_marker(line: &str) -> Option<char> {
    if line.starts_with("```") {
        Some('`')
    } else if line.starts_with("~~~") {
        Some('~')
    } else {
        None
    }
}

fn is_fence_line(line: &str, c: char) -> bool {
    let trimmed = line.trim_end();
    trimmed.chars().count() >= 3 && trimmed.chars().all(|ch| ch == c)
}

fn strip_prefix_ascii_ci<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let head = line.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &line[prefix.len()..])
}

fn parse_declaration_line(line: &str, phase: &str) -> Option<String> {
    let raw = strip_prefix_ascii_ci(line, "Blocked: ")
        .or_else(|| strip_prefix_ascii_ci(line, &format!("{phase} blocked: ")))?;
    // Control characters (including horizontal tabs) are rejected before trimming.
    if raw.chars().any(char::is_control) {
        return None;
    }
    let reason = raw.trim();
    if reason.is_empty() || reason.chars().count() > REASON_MAX_SCALARS {
        return None;
    }
    if has_angle_pair(reason) {
        return None;
    }
    Some(reason.to_string())
}

fn has_angle_pair(text: &str) -> bool {
    match text.find('<') {
        Some(open) => text[open + 1..].contains('>'),
        None => false,
    }
}

fn strip_control_tail<'a>(mut lines: Vec<&'a str>, phase: &str) -> Vec<&'a str> {
    match phase {
        PHASE_IMPLEMENT | PHASE_ADDRESS_FINDINGS => {
            if let Some(pos) = lines.iter().rposition(|line| !line.trim().is_empty())
                && crate::workflow::self_check::parse_line(lines[pos]).is_some()
            {
                lines.remove(pos);
            }
        }
        PHASE_ADVANCE => {
            // Remove the maximal contiguous suffix of nonempty `next_track:` lines,
            // ignoring blank lines between them. A malformed control or prose stops it.
            let mut i = lines.len();
            while i > 0 {
                i -= 1;
                let line = lines[i];
                if line.trim().is_empty() {
                    continue;
                }
                if crate::workflow::prompts::parse_next_track_line(line).is_some() {
                    lines.remove(i);
                } else {
                    break;
                }
            }
        }
        _ => {}
    }
    lines
}

fn terminal_tail(lines: &[&str]) -> bool {
    if lines.join("\n").chars().count() > TAIL_MAX_SCALARS {
        return false;
    }

    let mut paragraphs: Vec<Vec<&str>> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            if !current.is_empty() {
                paragraphs.push(std::mem::take(&mut current));
            }
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        paragraphs.push(current);
    }
    if paragraphs.len() > TAIL_MAX_PARAGRAPHS {
        return false;
    }

    for paragraph in &paragraphs {
        let first = paragraph[0];
        if !RECOVERY_PREFIXES
            .iter()
            .any(|prefix| first.starts_with(prefix))
        {
            return false;
        }
        for line in paragraph {
            if block_level_construct(line) || declaration_shaped(line) {
                return false;
            }
        }
    }
    true
}

fn block_level_construct(line: &str) -> bool {
    // Indented code: a tab or >= 4 leading spaces on any line of the paragraph.
    if line.starts_with('\t') {
        return true;
    }
    let leading = line.chars().take_while(|c| *c == ' ').count();
    if leading >= 4 {
        return true;
    }
    let trimmed = line.trim_start();
    if fence_marker(trimmed).is_some() {
        return true;
    }
    if trimmed.starts_with('>') {
        return true;
    }
    if atx_heading(trimmed) {
        return true;
    }
    line.contains(DECISION_OPEN) || line.contains(DECISION_CLOSE)
}

fn atx_heading(line: &str) -> bool {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return false;
    }
    let rest = &line[hashes..];
    rest.is_empty() || rest.starts_with(' ')
}

/// Any column-zero declaration-shaped line in a recovery paragraph invalidates
/// terminality: only the final named skill paragraph may carry a quotation, never a
/// declaration.
fn declaration_shaped(line: &str) -> bool {
    if strip_prefix_ascii_ci(line, "Blocked: ").is_some() {
        return true;
    }
    phases()
        .iter()
        .any(|p| strip_prefix_ascii_ci(line, &format!("{p} blocked: ")).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phase_arm(phase: &str, reason: &str) -> String {
        format!("{phase} blocked: {reason}")
    }

    #[test]
    fn accepts_both_arms_and_is_ascii_case_insensitive() {
        assert_eq!(
            classify("work\nBlocked: a real reason", PHASE_PLAN),
            Some("a real reason".into())
        );
        assert_eq!(
            classify("work\nBLOCKED: a real reason", PHASE_PLAN),
            Some("a real reason".into())
        );
        assert_eq!(
            classify("work\nFold blocked: a real reason", PHASE_FOLD),
            Some("a real reason".into())
        );
        assert_eq!(
            classify("work\nFOLD BLOCKED: a real reason", PHASE_FOLD),
            Some("a real reason".into())
        );
        for phase in phases() {
            let line = phase_arm(phase, "prerequisite missing");
            assert_eq!(
                classify(&format!("work\n{line}"), phase),
                Some("prerequisite missing".into()),
                "{phase}"
            );
        }
    }

    #[test]
    fn rejects_wrong_phase_and_underscore_alias() {
        assert_eq!(classify("Fold blocked: x", PHASE_IMPLEMENT), None);
        assert_eq!(
            classify("address_findings blocked: x", PHASE_ADDRESS_FINDINGS),
            None
        );
        assert_eq!(classify("Blocked: x", "plan-review"), None);
        assert_eq!(classify("Blocked: x", "cross-model-review"), None);
    }

    #[test]
    fn rejects_empty_control_placeholder_and_long_reasons() {
        assert_eq!(classify("Blocked: ", PHASE_PLAN), None);
        assert_eq!(classify("Blocked:", PHASE_PLAN), None);
        assert_eq!(classify("Blocked:   ", PHASE_PLAN), None);
        assert_eq!(classify("Blocked: <reason>", PHASE_PLAN), None);
        assert_eq!(classify("Blocked: a <b> c", PHASE_PLAN), None);
        assert_eq!(classify("Blocked: bad\treason", PHASE_PLAN), None);
        assert_eq!(classify("Blocked: \tbad", PHASE_PLAN), None);
        let long = "x".repeat(REASON_MAX_SCALARS + 1);
        assert_eq!(classify(&format!("Blocked: {long}"), PHASE_PLAN), None);
        let edge = "x".repeat(REASON_MAX_SCALARS);
        assert_eq!(
            classify(&format!("Blocked: {edge}"), PHASE_PLAN),
            Some(edge)
        );
        // Inline code in a real reason remains allowed.
        assert_eq!(
            classify(
                "Fold blocked: preflight returned `VAULT_KEY_MISSING`.",
                PHASE_FOLD
            ),
            Some("preflight returned `VAULT_KEY_MISSING`.".into())
        );
    }

    #[test]
    fn rejects_indented_code_quote_fence_and_inline_prefix_code() {
        assert_eq!(classify("  Blocked: x", PHASE_PLAN), None);
        assert_eq!(classify("> Blocked: x", PHASE_PLAN), None);
        assert_eq!(classify("`Blocked: x`", PHASE_PLAN), None);
        assert_eq!(
            classify("```\nBlocked: x\n```", PHASE_PLAN),
            None,
            "fenced declaration"
        );
        assert_eq!(
            classify("~~~\nBlocked: x\n~~~", PHASE_PLAN),
            None,
            "tilde fence"
        );
        assert_eq!(
            classify("```text\nBlocked: x\n", PHASE_PLAN),
            None,
            "unclosed fence"
        );
    }

    #[test]
    fn excludes_decision_blocks_and_example_cues() {
        assert_eq!(
            classify(
                "<!-- coordinator:decision active -->\nBlocked: owner said so\n<!-- /coordinator:decision -->",
                PHASE_PLAN
            ),
            None
        );
        assert_eq!(
            classify(
                "<!-- coordinator:decision superseded -->\nBlocked: no",
                PHASE_PLAN
            ),
            None,
            "unclosed opener excludes the remaining text"
        );
        assert_eq!(classify("Example:\nBlocked: sample", PHASE_PLAN), None);
        assert_eq!(classify("for example:\nBlocked: sample", PHASE_PLAN), None);
        assert_eq!(
            classify(
                "Sample output:\nBLOCKED: sample\nStill example\n\nBlocked: real",
                PHASE_PLAN
            ),
            Some("real".into())
        );
    }

    #[test]
    fn last_eligible_declaration_wins_and_invalid_later_text_prevents_terminality() {
        assert_eq!(
            classify(
                "Blocked: first\nNo files changed; nothing\n\nBlocked: second",
                PHASE_PLAN
            ),
            Some("second".into())
        );
        // A bare prefix later is not eligible and, as later non-recovery text, kills
        // the earlier declaration.
        assert_eq!(classify("Blocked: first\nBlocked:", PHASE_PLAN), None);
        assert_eq!(
            classify("Blocked: first\nplain later prose", PHASE_PLAN),
            None
        );
        // A later valid block wins (last eligible declaration).
        assert_eq!(
            classify(
                "Blocked: first\nNo files changed; x\nBlocked: hidden",
                PHASE_PLAN
            ),
            Some("hidden".into())
        );
        // A declaration-shaped but ineligible line in a recovery paragraph is not a
        // declaration and invalidates terminality.
        assert_eq!(
            classify(
                "Blocked: first\nNo files changed; x\nBlocked: <placeholder>",
                PHASE_PLAN
            ),
            None
        );
    }

    #[test]
    fn recovery_rules_single_newline_blank_newline_budget_and_constructs() {
        // The first recovery paragraph may start immediately after the declaration.
        assert_eq!(
            classify(
                "Blocked: precondition\nNo files changed; nothing written.",
                PHASE_PLAN
            ),
            Some("precondition".into())
        );
        // Blank-separated paragraphs within the cap.
        assert_eq!(
            classify(
                "Blocked: precondition\n\nNo files changed; x\n\nResume when ready.",
                PHASE_PLAN
            ),
            Some("precondition".into())
        );
        // Four non-empty paragraphs exceed the cap.
        assert_eq!(
            classify(
                "Blocked: x\nNo files changed; a\n\nNo files changed; b\n\nNo files changed; c\n\nNo files changed; d",
                PHASE_PLAN
            ),
            None
        );
        // An unrelated prefix invalidates.
        assert_eq!(
            classify("Blocked: x\n\nNext steps: retry.", PHASE_PLAN),
            None
        );
        // Block-level constructs invalidate on a continuation line.
        for bad in [
            "No files changed; x\n> quote",
            "No files changed; x\n# heading",
            "No files changed; x\n```\ncode\n```",
            "No files changed; x\n    indented code",
            "No files changed; x\n<!-- coordinator:decision active -->",
        ] {
            assert_eq!(
                classify(&format!("Blocked: x\n\n{bad}"), PHASE_PLAN),
                None,
                "{bad}"
            );
        }
        // Inline code and quotation marks stay valid.
        assert_eq!(
            classify(
                "Blocked: x\n\nNo files changed; see `foo` and \"a quote\".",
                PHASE_PLAN
            ),
            Some("x".into())
        );
    }

    #[test]
    fn tail_budget_counts_separators_and_boundaries() {
        let head = "Blocked: x\n\n";
        let prefix = "No files changed; ";
        // The tail starts with a leading blank separator, which counts in the budget.
        let pad = TAIL_MAX_SCALARS - 1 - prefix.chars().count();
        let exact = format!("{head}{prefix}{}", "a".repeat(pad));
        assert_eq!(
            classify(&exact, PHASE_PLAN),
            Some("x".into()),
            "exactly {TAIL_MAX_SCALARS} scalars"
        );
        let over = format!("{head}{prefix}{}", "a".repeat(pad + 1));
        assert_eq!(classify(&over, PHASE_PLAN), None);
    }

    #[test]
    fn capture_requires_adapter_six_phase_running() {
        let mut state = crate::state::RunState::idle("p");
        state.driver = crate::workflow::WorkflowDriver::Adapter;
        state.phase = PHASE_PLAN.into();
        state.status = crate::state::RunStatus::Running;
        assert!(capture_allowed(&state));
        for phase in phases() {
            state.phase = (*phase).into();
            assert!(capture_allowed(&state), "{phase}");
        }
        state.phase = crate::workflow::graph::PHASE_CI_WAIT.into();
        assert!(!capture_allowed(&state), "ci-wait is not a six-phase leg");
        state.phase = PHASE_PLAN.into();
        state.status = crate::state::RunStatus::Stopped;
        assert!(!capture_allowed(&state));
        state.status = crate::state::RunStatus::Running;
        state.driver = crate::workflow::WorkflowDriver::Stub;
        assert!(!capture_allowed(&state));
    }

    #[test]
    fn crlf_is_normalized() {
        let text = "narrative\r\nFold blocked: reason\r\n\r\nNo files changed; x\r\n";
        assert_eq!(classify(text, PHASE_FOLD), Some("reason".into()));
    }

    #[test]
    fn self_check_control_tail_is_stripped_for_implement_and_address_findings() {
        for phase in [PHASE_IMPLEMENT, PHASE_ADDRESS_FINDINGS] {
            for control in [
                "self-check: done",
                "self-check: continue finding=f1 action=repair",
            ] {
                let text = format!("Blocked: needs key\n{control}");
                assert_eq!(
                    classify(&text, phase),
                    Some("needs key".into()),
                    "{phase} / {control}"
                );
            }
            // An invalid tail is not stripped and prevents terminality.
            assert_eq!(
                classify("Blocked: needs key\nself-check: continue", phase),
                None
            );
            assert_eq!(classify("Blocked: needs key\nplain prose", phase), None);
        }
    }

    #[test]
    fn advance_control_suffix_is_stripped_and_blocks_discard_directives() {
        for control in [
            "next_track: 0112",
            "next_track: null",
            "next_track: none",
            "next_track:",
            "  next_track:   0112  ",
            "NEXT_TRACK: 0112",
        ] {
            let text = format!("Blocked: ship blocked\n{control}");
            assert_eq!(
                classify(&text, PHASE_ADVANCE),
                Some("ship blocked".into()),
                "{control}"
            );
        }
        assert_eq!(
            classify(
                "Blocked: x\nnext_track: 0112\nnext_track: null",
                PHASE_ADVANCE
            ),
            Some("x".into())
        );
        assert_eq!(
            classify("Blocked: x\nnext_track: has spaces", PHASE_ADVANCE),
            None,
            "malformed trailing control is not stripped"
        );
        assert_eq!(
            classify("Blocked: x\nnext_track: 1\nprose", PHASE_ADVANCE),
            None,
            "prose after a control prevents suffix stripping"
        );
        assert_eq!(
            classify(
                "Blocked: first\nnext_track: 1\nBlocked: second",
                PHASE_ADVANCE
            ),
            Some("second".into())
        );
        // A control before the declaration is not a suffix and is just narrative.
        assert_eq!(
            classify("next_track: 1\nBlocked: x", PHASE_ADVANCE),
            Some("x".into())
        );
    }

    #[test]
    fn no_control_tail_parity_for_other_phases() {
        assert_eq!(
            classify("Blocked: x\nself-check: done", PHASE_PLAN),
            None,
            "plan does not own a self-check tail"
        );
        assert_eq!(
            classify("Blocked: x\nnext_track: 0112", PHASE_FOLD),
            None,
            "fold does not own a next_track tail"
        );
    }

    #[test]
    fn historical_fold_message_verbatim() {
        // Copied verbatim from
        // `20261019T182439Z_fold.json` (2026-10-19). The declaration follows a single
        // newline after narrative; the reason is that line's remainder, not the
        // trailing skill quotation.
        const MESSAGE: &str = "I'll load the foldin skill, read track 0551's reviews and planning artifacts, and fold in supported findings. The injected block authenticates no owner decision.
Both reviews identify a stale bootstrap plan: `ledgerful-bench` reportedly already contains partial implementation. They also flag gaps in nextest profile pinning, status parsing, selection recovery, and test coverage. I'll verify those claims before updating the plan, and keep readiness blocked on unmet prerequisites.
Fold blocked: the required Windows AI-Brains preflight returned `VAULT_KEY_MISSING`. Ledger status is clean: 0 pending, 0 drift.

No files changed; `evidence.md` was not read. Resume when preflight succeeds.

The [ai-brains skill](/C:/dev/ledgerful/.agents/skills/ai-brains/SKILL.md) requires: \"If the preflight still fails with the Windows build, record the blocker and stop.\"";
        assert_eq!(
            classify(MESSAGE, PHASE_FOLD),
            Some(
                "the required Windows AI-Brains preflight returned `VAULT_KEY_MISSING`. \
                 Ledger status is clean: 0 pending, 0 drift."
                    .into()
            )
        );
    }

    #[test]
    fn single_newline_recovery_start_fixture() {
        const MESSAGE: &str = "Narrative before.\nFold blocked: a prerequisite is missing.\nNo files changed; nothing was written.\nResume when the prerequisite is resolved.";
        assert_eq!(
            classify(MESSAGE, PHASE_FOLD),
            Some("a prerequisite is missing.".into())
        );
    }

    #[test]
    fn unmarked_narration_can_stop_the_leg_disclosed_limitation() {
        // No natural-language classifier: a terminal line in unmarked narration is
        // accepted. This is the documented conservative false-block risk.
        assert_eq!(
            classify(
                "Earlier I thought this was blocked: maybe not.\nBlocked: still blocked",
                PHASE_PLAN
            ),
            Some("still blocked".into())
        );
    }
}
