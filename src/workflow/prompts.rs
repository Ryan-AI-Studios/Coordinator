//! Per-phase inject contracts (not a prompt DSL).

use std::path::{Path, PathBuf};

use crate::registry::ProjectRecord;
use crate::workflow::graph::{PHASE_ADVANCE, PHASE_FOLD, PHASE_IMPLEMENT, PHASE_PLAN};

use super::graph::resolve_track_dir;

const RESEARCH: &str = "Knowledge is stale. Verify pins, APIs, and hooks against primary sources \
(crates.io, docs.rs, official harness docs) before trusting training data or the track's \
plan-time snapshot.";

const END_TURN: &str = "After artifacts exist, end this turn. Coordinator applies the Phase Outcome. \
Do not run `coordinator outcome write` during this inject.";

/// 0012 / 0016 layout lines plus the pinned planning-outside-product sentence.
pub(crate) fn layout_block(record: &ProjectRecord, track_id: Option<&str>) -> String {
    let paths = crate::layout::resolve(record);
    let workspace = paths.workspace_root.display();
    let execution = paths
        .execution_repo
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(unset)".into());
    let conductor = paths.conductor_dir.display();
    let state = paths.state_dir.display();
    let track_hint = track_id
        .and_then(|id| resolve_track_dir(record, id))
        .map(|p| format!("Track folder: {}\n", p.display()))
        .unwrap_or_default();
    let repos_block = if paths.execution_repos.is_empty() {
        String::new()
    } else {
        let mut lines = String::from("Execution repos:\n");
        for (name, path) in &paths.execution_repos {
            lines.push_str(&format!("- {name} = {}\n", path.display()));
        }
        lines
    };
    format!(
        "Workspace root: {workspace}\n\
         Execution repo: {execution}\n\
         {repos_block}\
         Conductor directory: {conductor}\n\
         State directory: {state}\n\
         {track_hint}\
         Planning, conductor tracks, ADRs, and deferred.md stay outside the product git. \
         Never commit them into the execution repo.\n\
         plan / fold / advance write under the workspace / conductor / track folder, \
         not inside the execution repo unless the track spec says the execution path is the workspace.\n\
         implement honors the track spec execution path.\n"
    )
}

fn skill_md(root: &Path, name: &str) -> PathBuf {
    root.join(".agents")
        .join("skills")
        .join(name)
        .join("SKILL.md")
}

fn workspace_skill(record: &ProjectRecord, name: &str) -> String {
    let paths = crate::layout::resolve(record);
    skill_md(&paths.workspace_root, name).display().to_string()
}

fn execution_skill(record: &ProjectRecord, name: &str) -> String {
    let paths = crate::layout::resolve(record);
    let root = paths
        .execution_repo
        .as_ref()
        .unwrap_or(&paths.workspace_root);
    skill_md(root, name).display().to_string()
}

fn honor_skill(name: &str, path: &str) -> String {
    format!("Honor project skills. This phase loads the `{name}` skill from {path}.")
}

/// Injected into Grok-bound phases (plan / fold / implement / advance).
pub fn phase_prompt(record: &ProjectRecord, phase: &str, track_id: Option<&str>) -> String {
    let track = track_id.unwrap_or("(none)");
    let layout = layout_block(record, track_id);
    let body = match phase {
        PHASE_PLAN => {
            let path = workspace_skill(record, "plan");
            format!(
                "{}\n\
                 {RESEARCH}\n\
                 Write spec.md and plan.md in the track folder. Mark the track Ready.\n\
                 If spec.md and plan.md already exist, do not re-plan from scratch. \
                 Do not run cargo, ledgerful, or ai-brains unless the track spec \
                 execution path is the execution repo. Write evidence.md if the spec \
                 asks for it, then end this turn.\n\
                 {END_TURN}\n",
                honor_skill("plan", &path)
            )
        }
        PHASE_FOLD => {
            let path = workspace_skill(record, "foldin");
            format!(
                "{}\n\
                 Fold the track `*-review.md` files (agy-review / opencode-review) into spec and plan.\n\
                 {END_TURN}\n",
                honor_skill("foldin", &path)
            )
        }
        PHASE_IMPLEMENT => {
            let implement = execution_skill(record, "implement");
            let onboarding = execution_skill(record, "onboarding");
            format!(
                "Honor project skills. This phase loads the `implement` skill from {implement} \
                 and the `onboarding` skill from {onboarding}.\n\
                 Honor the track spec execution path.\n\
                 If the spec execution path is the workspace (planning-only), write \
                 evidence.md there. Do not edit the execution repo and do not run \
                 cargo, ledgerful, or ai-brains there.\n\
                 {RESEARCH}\n\
                 {END_TURN}\n"
            )
        }
        PHASE_ADVANCE => {
            let path = workspace_skill(record, "plan");
            format!(
                "{}\n\
                 Pick the next natural track, or none. The last line of your reply must be \
                 `next_track: <id>` or `next_track: null`.\n\
                 {END_TURN}\n",
                honor_skill("plan", &path)
            )
        }
        _ => "Unknown phase; end the turn.\n".into(),
    };
    format!("Coordinator phase `{phase}` for track `{track}`.\n{layout}{body}")
}

/// Last matching `next_track:` line.
///
/// `Some(Some(id))` = id; `Some(None)` = explicit clear (`null` / `none` / empty);
/// `None` = no matching line.
pub fn parse_next_track_line(text: &str) -> Option<Option<String>> {
    let mut last = None;
    for raw in text.lines() {
        let line = raw.trim();
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("next_track") {
            continue;
        }
        let value = rest.trim();
        if value.is_empty()
            || value.eq_ignore_ascii_case("null")
            || value.eq_ignore_ascii_case("none")
        {
            last = Some(None);
        } else if !value.chars().any(char::is_whitespace) {
            last = Some(Some(value.to_string()));
        }
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::LayoutProfile;
    use chrono::Utc;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn nested_record() -> ProjectRecord {
        let ws = PathBuf::from(r"C:\dev\Orca");
        ProjectRecord {
            id: "orca".into(),
            path: ws.clone(),
            display_name: Some("Orca".into()),
            layout_profile: LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: Some(ws.join("OrcaSlicer-ZR")),
            execution_repos: BTreeMap::new(),
            state_dir: None,
            auto_merge: false,
            phase_timeouts_secs: BTreeMap::new(),
            created_at: Utc::now(),
        }
    }

    fn contains_skill(text: &str, name: &str) -> bool {
        let n = text.replace('\\', "/");
        n.contains(&format!(".agents/skills/{name}/SKILL.md"))
    }

    #[test]
    fn nested_prompt_includes_layout_paths_and_split_rule() {
        let rec = nested_record();
        let text = phase_prompt(&rec, "plan", Some("0099-CoordinatorDogfoodProbe"));
        assert!(
            text.contains(r"Workspace root: C:\dev\Orca"),
            "workspace root missing: {text}"
        );
        assert!(
            text.contains(r"Execution repo: C:\dev\Orca\OrcaSlicer-ZR"),
            "execution repo missing: {text}"
        );
        assert!(
            text.contains(r"State directory: C:\dev\Orca\.coordinator"),
            "state dir missing: {text}"
        );
        assert!(
            text.contains("outside"),
            "planning-outside-product rule missing: {text}"
        );
        assert!(text.contains("Honor project skills"));
        assert!(text.contains("Execution repo:"));
        assert!(!text.contains("Execution repo: (unset)"));
        assert!(
            !text.contains("Execution repos:"),
            "nested empty map must omit named-map block: {text}"
        );
    }

    fn multi_sibling_record() -> ProjectRecord {
        let ws = PathBuf::from(r"C:\dev\coordinated");
        let mut execution_repos = BTreeMap::new();
        execution_repos.insert("ledgerful".into(), PathBuf::from(r"C:\dev\ledgerful"));
        execution_repos.insert(
            "ledgerful-action".into(),
            PathBuf::from(r"C:\dev\ledgerful-action"),
        );
        execution_repos.insert(
            "ledgerful-frontend".into(),
            PathBuf::from(r"C:\dev\ledgerful-frontend"),
        );
        execution_repos.insert(
            "ledgerful-web".into(),
            PathBuf::from(r"C:\dev\ledgerful-web"),
        );
        ProjectRecord {
            id: "coordinated".into(),
            path: ws,
            display_name: Some("coordinated".into()),
            layout_profile: LayoutProfile::MultiSibling,
            conductor_dir: None,
            execution_repo: Some(PathBuf::from(r"C:\dev\ledgerful")),
            execution_repos,
            state_dir: None,
            auto_merge: false,
            phase_timeouts_secs: BTreeMap::new(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn multi_sibling_prompt_lists_named_map() {
        let rec = multi_sibling_record();
        let text = phase_prompt(&rec, "plan", Some("0899-CoordinatorDogfoodProbe"));
        assert!(
            text.contains(r"Workspace root: C:\dev\coordinated"),
            "workspace root missing: {text}"
        );
        assert!(
            text.contains(r"Execution repo: C:\dev\ledgerful"),
            "primary execution repo missing: {text}"
        );
        assert!(
            text.contains("- ledgerful = C:\\dev\\ledgerful"),
            "ledgerful map line missing: {text}"
        );
        assert!(
            text.contains("- ledgerful-action = C:\\dev\\ledgerful-action"),
            "ledgerful-action map line missing: {text}"
        );
        assert!(
            text.contains("- ledgerful-frontend = C:\\dev\\ledgerful-frontend"),
            "ledgerful-frontend map line missing: {text}"
        );
        assert!(
            text.contains("- ledgerful-web = C:\\dev\\ledgerful-web"),
            "ledgerful-web map line missing: {text}"
        );
        assert!(
            text.contains("outside"),
            "planning-outside-product rule missing: {text}"
        );
        assert!(
            text.contains("Execution repos:"),
            "named-map heading missing: {text}"
        );
        let heading = text.find("Execution repos:").expect("heading");
        let ledgerful = text.find("- ledgerful = ").expect("ledgerful");
        let action = text.find("- ledgerful-action = ").expect("action");
        let frontend = text.find("- ledgerful-frontend = ").expect("frontend");
        let web = text.find("- ledgerful-web = ").expect("web");
        assert!(
            heading < ledgerful && ledgerful < action && action < frontend && frontend < web,
            "named map must follow BTreeMap key order: {text}"
        );
    }

    #[test]
    fn unset_execution_repo_is_explicit() {
        let mut rec = nested_record();
        rec.execution_repo = None;
        let text = phase_prompt(&rec, "plan", None);
        assert!(text.contains("Execution repo: (unset)"));
    }

    #[test]
    fn shared_outside_sentence_is_product_git() {
        let rec = nested_record();
        let text = phase_prompt(&rec, "plan", Some("0099"));
        assert!(
            text.contains("outside the product git"),
            "layout must pin slot_prompt wording: {text}"
        );
        assert!(
            !text.contains("Honor project skills (plan, review-track, foldin, implement)"),
            "four-name bag must be gone: {text}"
        );
    }

    #[test]
    fn plan_contract_names_skill_research_and_forbids_cli_write() {
        let rec = nested_record();
        let text = phase_prompt(&rec, "plan", Some("0099-CoordinatorDogfoodProbe"));
        assert!(contains_skill(&text, "plan"), "plan skill path: {text}");
        assert!(text.contains("Honor project skills"));
        assert!(text.contains("stale") && text.contains("primary sources"));
        assert!(text.contains("spec.md"));
        assert!(text.contains("plan.md"));
        assert!(text.contains("already exist"));
        assert!(text.contains("evidence.md"));
        assert!(text.contains("Do not run cargo"));
        assert!(text.contains("end this turn") || text.contains("end the turn"));
        assert!(text.contains("Do not") && text.contains("outcome write"));
        assert!(!contains_skill(&text, "foldin"));
        assert!(!contains_skill(&text, "implement"));
        let n = text.replace('\\', "/");
        assert!(
            n.contains("C:/dev/Orca/.agents/skills/plan/SKILL.md"),
            "plan skill under workspace: {text}"
        );
        assert!(
            !n.contains("OrcaSlicer-ZR/.agents/skills/plan/SKILL.md"),
            "plan skill must not be under execution_repo: {text}"
        );
    }

    #[test]
    fn fold_contract_names_foldin_and_reviews() {
        let rec = nested_record();
        let text = phase_prompt(&rec, "fold", Some("0099"));
        assert!(contains_skill(&text, "foldin"), "foldin skill: {text}");
        assert!(text.contains("foldin"));
        assert!(
            text.contains("*-review.md")
                || (text.contains("agy-review") && text.contains("opencode-review"))
        );
        assert!(text.contains("end this turn") || text.contains("end the turn"));
        assert!(text.contains("Do not") && text.contains("outcome write"));
        assert!(text.contains("Honor project skills"));
        let n = text.replace('\\', "/");
        assert!(
            n.contains("C:/dev/Orca/.agents/skills/foldin/SKILL.md"),
            "foldin skill under workspace: {text}"
        );
        assert!(!n.contains("OrcaSlicer-ZR/.agents/skills/foldin/SKILL.md"));
    }

    #[test]
    fn implement_contract_names_product_skills_under_exec() {
        let rec = nested_record();
        let text = phase_prompt(&rec, "implement", Some("0099"));
        assert!(
            contains_skill(&text, "implement"),
            "implement skill: {text}"
        );
        assert!(
            contains_skill(&text, "onboarding"),
            "onboarding skill: {text}"
        );
        let n = text.replace('\\', "/");
        assert!(
            n.contains("OrcaSlicer-ZR/.agents/skills/implement/SKILL.md"),
            "implement under execution_repo: {text}"
        );
        assert!(
            n.contains("OrcaSlicer-ZR/.agents/skills/onboarding/SKILL.md"),
            "onboarding under execution_repo: {text}"
        );
        assert!(text.contains("stale") && text.contains("primary sources"));
        assert!(text.contains("execution path"));
        assert!(text.contains("planning-only"));
        assert!(text.contains("evidence.md"));
        assert!(text.contains("end this turn") || text.contains("end the turn"));
        assert!(text.contains("Do not") && text.contains("outcome write"));
        assert!(text.contains("Honor project skills"));
    }

    #[test]
    fn implement_unset_exec_falls_back_to_workspace_skills() {
        let mut rec = nested_record();
        rec.execution_repo = None;
        let text = phase_prompt(&rec, "implement", Some("0099"));
        let n = text.replace('\\', "/");
        assert!(n.contains("C:/dev/Orca/.agents/skills/implement/SKILL.md"));
        assert!(n.contains("C:/dev/Orca/.agents/skills/onboarding/SKILL.md"));
        assert!(!n.contains("OrcaSlicer-ZR/.agents/skills/"));
    }

    #[test]
    fn advance_contract_names_plan_and_next_track_line() {
        let rec = nested_record();
        let text = phase_prompt(&rec, "advance", Some("0099"));
        assert!(
            contains_skill(&text, "plan"),
            "advance uses plan skill: {text}"
        );
        assert!(text.contains("Honor project skills"));
        assert!(text.contains("next_track:"));
        assert!(text.contains("null"));
        assert!(text.contains("end this turn") || text.contains("end the turn"));
        assert!(text.contains("Do not") && text.contains("outcome write"));
        let n = text.replace('\\', "/");
        assert!(n.contains("C:/dev/Orca/.agents/skills/plan/SKILL.md"));
        assert!(!n.contains("OrcaSlicer-ZR/.agents/skills/plan/SKILL.md"));
    }

    #[test]
    fn unknown_phase_keeps_layout_and_does_not_panic() {
        let rec = nested_record();
        let text = phase_prompt(&rec, "ci-wait", Some("0099"));
        assert!(text.contains("Workspace root:"));
        assert!(text.contains("Unknown phase"));
        assert!(text.contains("end the turn"));
    }

    #[test]
    fn parse_next_track_line_last_match_wins() {
        assert_eq!(
            parse_next_track_line("next_track: 0028-Foo"),
            Some(Some("0028-Foo".into()))
        );
        assert_eq!(parse_next_track_line("next_track: null"), Some(None));
        assert_eq!(parse_next_track_line("next_track: None"), Some(None));
        assert_eq!(parse_next_track_line("next_track:"), Some(None));
        assert_eq!(parse_next_track_line("no line here"), None);
        let mixed = "noise\nNEXT_TRACK: 0001\nnext_track: 0002\ntrailing prose";
        assert_eq!(parse_next_track_line(mixed), Some(Some("0002".into())));
        let then_clear = "next_track: 0002\nnext_track: null";
        assert_eq!(parse_next_track_line(then_clear), Some(None));
        assert_eq!(
            parse_next_track_line("  next_track:   0030-Start  "),
            Some(Some("0030-Start".into()))
        );
        assert_eq!(
            parse_next_track_line("next_track: has spaces"),
            None,
            "id must not contain spaces"
        );
    }
}
