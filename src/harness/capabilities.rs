//! Static map of signals this client keeps for each bound harness (track 0072).
//!
//! Cell labels are closed: `native`, `approximated`, `unavailable`.
//! `used` and `size` are tokens in the context window, not an account balance.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `last_signal` after a non-cancel `session/request_permission` allow.
pub const APPROVALS_AUTO_ALLOWED: &str = "approvals: auto-allowed (approximated)";

/// `last_signal` after `cursor/ask_question` is answered `skipped`.
pub const QUESTIONS_AUTO_SKIPPED: &str = "questions: auto-skipped (approximated)";

/// How faithfully a cell matches the harness payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SignalClass {
    Native,
    Approximated,
    Unavailable,
}

impl SignalClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Approximated => "approximated",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Interactive ACP sessions share one client. One-shot reviewers are spawned once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessTier {
    Interactive,
    OneShot,
}

/// One signal on one harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MapCell {
    pub signal: &'static str,
    pub class: SignalClass,
    pub note: &'static str,
}

/// One harness row in the map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessRow {
    pub harness: &'static str,
    pub tier: HarnessTier,
    pub acp_session: bool,
    pub cells: &'static [MapCell],
}

/// ACP `usage_update` kept on the session. `cost` is `{amount} {currency}` text so status stays `Eq`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUsage {
    pub used: u64,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<String>,
}

const INTERACTIVE_CELLS: &[MapCell] = &[
    MapCell {
        signal: "streaming",
        class: SignalClass::Native,
        note: "agent_message_chunk text is the assistant message.",
    },
    MapCell {
        signal: "tools",
        class: SignalClass::Approximated,
        note: "toolCallId and status drive tool_in_flight. A non-empty title is stored on last_tool_title. Input and output are not stored.",
    },
    MapCell {
        signal: "diffs",
        class: SignalClass::Unavailable,
        note: "Harness diffs are not captured. Git is the product diff.",
    },
    MapCell {
        signal: "approvals",
        class: SignalClass::Approximated,
        note: "permission_allow_reply selects allow_once or allow_always. last_signal is approvals: auto-allowed (approximated). Cancel does not set it.",
    },
    MapCell {
        signal: "questions",
        class: SignalClass::Approximated,
        note: "cursor/ask_question replies skipped. last_signal is questions: auto-skipped (approximated). cursor/create_plan stays accepted and does not change last_signal.",
    },
    MapCell {
        signal: "context_usage",
        class: SignalClass::Native,
        note: "usage_update used and size are tokens in context and the window size. Cost is optional. An absent field means no update yet.",
    },
    MapCell {
        signal: "account_quota",
        class: SignalClass::Unavailable,
        note: "No remaining-balance command. Do not infer a balance from ModelExhaustion.",
    },
    MapCell {
        signal: "compaction",
        class: SignalClass::Native,
        note: "context_reduce_command reads availableCommands once at initialize.",
    },
    MapCell {
        signal: "thought",
        class: SignalClass::Unavailable,
        note: "agent_thought_chunk is not copied into collected_text.",
    },
    MapCell {
        signal: "available_commands",
        class: SignalClass::Approximated,
        note: "Post-initialize available_commands_update is ignored. The initialize snapshot is the only read.",
    },
    MapCell {
        signal: "resume_fork",
        class: SignalClass::Unavailable,
        note: "session/load and session/resume are not called. That is a second session lifecycle.",
    },
    MapCell {
        signal: "session_listing",
        class: SignalClass::Unavailable,
        note: "session/list is not called. That is a second session lifecycle.",
    },
];

const ONE_SHOT_CELLS: &[MapCell] = &[
    MapCell {
        signal: "spawn_argv",
        class: SignalClass::Native,
        note: "The bound command is the spawn argv.",
    },
    MapCell {
        signal: "exit_code",
        class: SignalClass::Native,
        note: "The child exit code is the result.",
    },
    MapCell {
        signal: "captured_output",
        class: SignalClass::Native,
        note: "Stdout and stderr of the one-shot are the captured output.",
    },
    MapCell {
        signal: "streaming",
        class: SignalClass::Unavailable,
        note: "Not an ACP session.",
    },
    MapCell {
        signal: "tools",
        class: SignalClass::Unavailable,
        note: "Not an ACP session.",
    },
    MapCell {
        signal: "diffs",
        class: SignalClass::Unavailable,
        note: "Not an ACP session. Git is the product diff.",
    },
    MapCell {
        signal: "approvals",
        class: SignalClass::Unavailable,
        note: "Not an ACP session.",
    },
    MapCell {
        signal: "questions",
        class: SignalClass::Unavailable,
        note: "Not an ACP session.",
    },
    MapCell {
        signal: "context_usage",
        class: SignalClass::Unavailable,
        note: "Not an ACP session. No usage_update.",
    },
    MapCell {
        signal: "account_quota",
        class: SignalClass::Unavailable,
        note: "No remaining-balance command.",
    },
    MapCell {
        signal: "compaction",
        class: SignalClass::Unavailable,
        note: "Not an ACP session.",
    },
    MapCell {
        signal: "thought",
        class: SignalClass::Unavailable,
        note: "Not an ACP session.",
    },
    MapCell {
        signal: "available_commands",
        class: SignalClass::Unavailable,
        note: "Not an ACP session.",
    },
    MapCell {
        signal: "resume_fork",
        class: SignalClass::Unavailable,
        note: "session/load and session/resume are not called.",
    },
    MapCell {
        signal: "session_listing",
        class: SignalClass::Unavailable,
        note: "session/list is not called.",
    },
];

const MAP: &[HarnessRow] = &[
    HarnessRow {
        harness: "grok",
        tier: HarnessTier::Interactive,
        acp_session: true,
        cells: INTERACTIVE_CELLS,
    },
    HarnessRow {
        harness: "cursor",
        tier: HarnessTier::Interactive,
        acp_session: true,
        cells: INTERACTIVE_CELLS,
    },
    HarnessRow {
        harness: "antigravity",
        tier: HarnessTier::OneShot,
        acp_session: false,
        cells: ONE_SHOT_CELLS,
    },
    HarnessRow {
        harness: "opencode",
        tier: HarnessTier::OneShot,
        acp_session: false,
        cells: ONE_SHOT_CELLS,
    },
    HarnessRow {
        harness: "codex",
        tier: HarnessTier::OneShot,
        acp_session: false,
        cells: ONE_SHOT_CELLS,
    },
    HarnessRow {
        harness: "claude",
        tier: HarnessTier::OneShot,
        acp_session: false,
        cells: ONE_SHOT_CELLS,
    },
];

const DECISION_RULE: &str = "\
## Decision rule

Safety and cost signals are context usage, approvals, and questions.

- Context usage records `used` and `size` from `usage_update` (tokens currently in context, and the window size). This is not an account balance. An absent `context_usage` field means the session has not emitted `usage_update`. Cost is stored only when `amount` is a finite number and `currency` is a non-empty code, as `{amount} {currency}`.
- Approvals stay on `permission_allow_reply` (`allow_once` or `allow_always`, else the first option, else `allow-once`). `last_signal` is `approvals: auto-allowed (approximated)`. Cancel replies `cancelled` and does not set that string. The run does not pause for a person (ADR-0003).
- Questions: `cursor/ask_question` replies `skipped` and sets `last_signal` to `questions: auto-skipped (approximated)`. `cursor/create_plan` stays `accepted` and does not change `last_signal`.
- Account quota is `unavailable` on every row. `map_failure_class` still reports `ModelExhaustion` after the fact. That class is not a balance.
- `agent_thought_chunk` is not copied into `collected_text`.
- Post-initialize `available_commands_update` is ignored. Compaction reads `availableCommands` once at initialize (`context_reduce_command`).
- Harness diffs are not captured. Git (`git diff`) is the product diff.
- `session/list`, `session/load`, and `session/resume` are not called. Calling them would be a second session lifecycle.
";

/// Rows for grok, cursor, and the one-shot reviewers in `default_role_bindings`.
pub fn capability_map() -> &'static [HarnessRow] {
    MAP
}

/// Markdown for the planning-tree map. The product test asserts this text; it does not write the file.
pub fn capability_map_markdown() -> String {
    let mut out = String::new();
    out.push_str("# Capability map\n\n");
    out.push_str(
        "Coordinator drives grok and cursor on one ACP client (`is_acp_session_harness`). \
One-shot reviewers from `default_role_bindings` are separate rows. \
`gh` is the CI client and is not a row.\n\n",
    );
    out.push_str("Each cell is `native`, `approximated`, or `unavailable`.\n\n");
    out.push_str(DECISION_RULE);
    out.push_str("\n## Interactive ACP\n\n");
    out.push_str("grok and cursor share this client.\n\n");
    push_table(&mut out, HarnessTier::Interactive);
    out.push_str("\n## One-shot reviewers\n\n");
    out.push_str("These harnesses are not ACP sessions.\n\n");
    push_table(&mut out, HarnessTier::OneShot);
    out
}

fn push_table(out: &mut String, tier: HarnessTier) {
    out.push_str("| Harness | Signal | Class | Note |\n| --- | --- | --- | --- |\n");
    for row in MAP.iter().filter(|r| r.tier == tier) {
        for cell in row.cells {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} |",
                row.harness,
                cell.signal,
                cell.class.as_str(),
                cell.note
            );
        }
    }
}

/// `session/update` whose `sessionUpdate` is `usage_update` with integer `used` and `size`.
pub fn context_usage_from_message(msg: &Value) -> Option<ContextUsage> {
    let update = msg.get("params")?.get("update")?;
    context_usage_from_update(update)
}

/// Parse one ACP update object. `None` when the variant is not `usage_update` or `used`/`size` are not integers.
pub fn context_usage_from_update(update: &Value) -> Option<ContextUsage> {
    if update.get("sessionUpdate").and_then(|v| v.as_str()) != Some("usage_update") {
        return None;
    }
    let used = update.get("used").and_then(Value::as_u64)?;
    let size = update.get("size").and_then(Value::as_u64)?;
    let cost = update.get("cost").and_then(cost_text);
    Some(ContextUsage { used, size, cost })
}

fn cost_text(cost: &Value) -> Option<String> {
    let obj = cost.as_object()?;
    let amount = obj.get("amount")?.as_number()?;
    if !amount.as_f64().is_some_and(f64::is_finite) {
        return None;
    }
    let currency = obj.get("currency")?.as_str()?.trim();
    if currency.is_empty() || currency.chars().any(char::is_whitespace) {
        return None;
    }
    Some(format!("{amount} {currency}"))
}

/// Non-empty trimmed `title` on `tool_call` or `tool_call_update`. Other updates return `None`.
pub fn tool_title_from_message(msg: &Value) -> Option<String> {
    let update = msg.get("params")?.get("update")?;
    let kind = update.get("sessionUpdate").and_then(|v| v.as_str())?;
    if kind != "tool_call" && kind != "tool_call_update" {
        return None;
    }
    let title = update.get("title")?.as_str()?.trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::json;

    use super::*;

    #[test]
    fn capabilities_map_labels_are_closed() {
        for class in [
            SignalClass::Native,
            SignalClass::Approximated,
            SignalClass::Unavailable,
        ] {
            let wire = serde_json::to_value(class).unwrap();
            assert_eq!(wire, json!(class.as_str()));
        }
        assert!(serde_json::from_str::<SignalClass>("\"partial\"").is_err());
        assert!(serde_json::from_str::<SignalClass>("\"native\"").is_ok());

        let map = capability_map();
        let interactive: Vec<_> = map
            .iter()
            .filter(|r| r.tier == HarnessTier::Interactive)
            .map(|r| r.harness)
            .collect();
        assert_eq!(interactive, ["grok", "cursor"]);
        assert!(map.iter().all(|r| r.harness != "gh"));

        let bound: BTreeSet<String> = crate::config::default_role_bindings()
            .values()
            .map(|b| b.harness.clone())
            .filter(|h| h != "grok" && h != "cursor")
            .collect();
        let one_shot: BTreeSet<String> = map
            .iter()
            .filter(|r| r.tier == HarnessTier::OneShot)
            .map(|r| r.harness.to_string())
            .collect();
        assert_eq!(one_shot, bound);

        for row in map {
            let quota = row
                .cells
                .iter()
                .find(|c| c.signal == "account_quota")
                .unwrap_or_else(|| panic!("{} missing account_quota", row.harness));
            assert_eq!(quota.class, SignalClass::Unavailable);
            let mut seen = BTreeSet::new();
            for cell in row.cells {
                assert!(seen.insert(cell.signal), "duplicate {}", cell.signal);
                assert!(matches!(
                    cell.class,
                    SignalClass::Native | SignalClass::Approximated | SignalClass::Unavailable
                ));
            }
            match row.tier {
                HarnessTier::Interactive => {
                    assert!(row.acp_session);
                    assert_eq!(cell(row, "context_usage").class, SignalClass::Native);
                    assert_eq!(cell(row, "approvals").class, SignalClass::Approximated);
                    assert!(cell(row, "approvals").note.contains(APPROVALS_AUTO_ALLOWED));
                    assert!(cell(row, "questions").note.contains(QUESTIONS_AUTO_SKIPPED));
                    assert_eq!(cell(row, "thought").class, SignalClass::Unavailable);
                    assert_eq!(
                        cell(row, "available_commands").class,
                        SignalClass::Approximated
                    );
                    assert_eq!(cell(row, "diffs").class, SignalClass::Unavailable);
                    assert_eq!(cell(row, "resume_fork").class, SignalClass::Unavailable);
                    assert_eq!(cell(row, "session_listing").class, SignalClass::Unavailable);
                }
                HarnessTier::OneShot => {
                    assert!(!row.acp_session, "{}", row.harness);
                    assert_eq!(cell(row, "spawn_argv").class, SignalClass::Native);
                    assert_eq!(cell(row, "exit_code").class, SignalClass::Native);
                    assert_eq!(cell(row, "captured_output").class, SignalClass::Native);
                    assert_eq!(cell(row, "streaming").class, SignalClass::Unavailable);
                    assert_eq!(cell(row, "context_usage").class, SignalClass::Unavailable);
                }
            }
        }

        let md = capability_map_markdown();
        assert!(md.contains(APPROVALS_AUTO_ALLOWED));
        assert!(md.contains(QUESTIONS_AUTO_SKIPPED));
        assert!(md.contains("not ACP sessions"));
        assert!(md.contains("session/list"));
        assert!(md.contains("agent_thought_chunk"));
        assert!(md.contains("available_commands_update"));
        assert!(md.contains("account balance"));
        assert!(!md.contains("| gh |"));
        assert!(!md.contains("partial"));
        for row in map {
            assert!(md.contains(row.harness));
            for cell in row.cells {
                assert!(md.contains(cell.class.as_str()));
                assert!(md.contains(cell.signal));
            }
        }
    }

    fn cell<'a>(row: &'a HarnessRow, signal: &str) -> &'a MapCell {
        row.cells
            .iter()
            .find(|c| c.signal == signal)
            .unwrap_or_else(|| panic!("{} missing {signal}", row.harness))
    }

    #[test]
    fn usage_object_keeps_json_cost_text() {
        let msg: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"usage_update","used":12,"size":200,"cost":{"amount":0.01,"currency":" USD "}}}}"#,
        )
        .unwrap();
        let usage = context_usage_from_message(&msg).unwrap();
        assert_eq!(usage.used, 12);
        assert_eq!(usage.size, 200);
        assert_eq!(usage.cost.as_deref(), Some("0.01 USD"));
    }

    #[test]
    fn usage_omits_cost_when_missing_or_malformed_and_rejects_fractional_tokens() {
        let bare = json!({"sessionUpdate":"usage_update","used":1,"size":2});
        let usage = context_usage_from_update(&bare).unwrap();
        assert_eq!(usage.used, 1);
        assert!(usage.cost.is_none());

        let bad = json!({"sessionUpdate":"usage_update","used":3,"size":4,"cost":"nope"});
        let usage = context_usage_from_update(&bad).unwrap();
        assert_eq!(usage.used, 3);
        assert_eq!(usage.size, 4);
        assert!(usage.cost.is_none());

        let spaced = json!({"sessionUpdate":"usage_update","used":3,"size":4,"cost":{"amount":1,"currency":"US D"}});
        assert!(context_usage_from_update(&spaced).unwrap().cost.is_none());

        assert!(
            context_usage_from_update(&json!({"sessionUpdate":"usage_update","used":1.5,"size":4}))
                .is_none()
        );
        assert!(
            context_usage_from_update(
                &json!({"sessionUpdate":"agent_thought_chunk","content":{"text":"x"}})
            )
            .is_none()
        );
    }

    #[test]
    fn tool_title_trims_call_and_update_only() {
        let call =
            json!({"params":{"update":{"sessionUpdate":"tool_call","title":"  read file  "}}});
        assert_eq!(tool_title_from_message(&call).as_deref(), Some("read file"));
        let update =
            json!({"params":{"update":{"sessionUpdate":"tool_call_update","title":"   "}}});
        assert!(tool_title_from_message(&update).is_none());
        let chunk =
            json!({"params":{"update":{"sessionUpdate":"agent_message_chunk","title":"nope"}}});
        assert!(tool_title_from_message(&chunk).is_none());
    }
}
