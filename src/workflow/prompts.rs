//! Per-phase inject contracts (not a prompt DSL).

use std::path::{Path, PathBuf};

use crate::registry::ProjectRecord;
use crate::workflow::graph::{
    PHASE_ADDRESS_FINDINGS, PHASE_ADVANCE, PHASE_FOLD, PHASE_IMPLEMENT, PHASE_PLAN,
};

use super::graph::resolve_track_dir;

const RESEARCH: &str = "Knowledge is stale. Verify pins, APIs, and hooks against primary sources \
(crates.io, docs.rs, official harness docs) before trusting training data or the track's \
plan-time snapshot.";

const END_TURN: &str = "After artifacts exist, end this turn. Coordinator applies the Phase Outcome. \
Do not run `coordinator outcome write` during this inject.";

/// 0012 / 0016 layout lines plus the pinned planning-outside-product sentence.
///
/// `implement` and `address-findings` name the epoch worktree as the execution
/// repo when that directory exists. Other phases keep the shared execution repo
/// and add an `Epoch worktree:` line.
pub(crate) fn layout_block(record: &ProjectRecord, track_id: Option<&str>, phase: &str) -> String {
    let paths = crate::layout::resolve(record);
    let epoch = crate::worktree::active_epoch_dir(record);
    let use_epoch = epoch.is_some() && matches!(phase, PHASE_IMPLEMENT | PHASE_ADDRESS_FINDINGS);
    let execution = if use_epoch {
        epoch.as_ref().map(|p| p.display().to_string())
    } else {
        paths
            .execution_repo
            .as_ref()
            .map(|p| p.display().to_string())
    }
    .unwrap_or_else(|| "(unset)".into());
    let epoch_line = epoch
        .as_ref()
        .map(|p| format!("Epoch worktree: {}\n", p.display()))
        .unwrap_or_default();
    let workspace = paths.workspace_root.display();
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
         {epoch_line}\
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
    let fallback = paths.execution_repo.clone().unwrap_or(paths.workspace_root);
    let root = crate::worktree::active_epoch_dir(record).unwrap_or(fallback);
    skill_md(&root, name).display().to_string()
}

fn honor_skill(name: &str, path: &str) -> String {
    format!("Honor project skills. This phase loads the `{name}` skill from {path}.")
}

fn this_run_gate_slug<'a>(name: &'a str, prefix: &str, attempts: u32) -> Option<&'a str> {
    if attempts == 0 {
        return None;
    }
    let rest = name.strip_prefix(prefix)?;
    (1..=attempts).find_map(|n| {
        rest.strip_suffix(&format!(".gate{n}.md"))
            .filter(|slug| !slug.is_empty() && !slug.contains('.'))
    })
}

fn list_gate_archives(
    record: &ProjectRecord,
    track_id: Option<&str>,
    attempts: u32,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let state_dir = super::bundle::reviews_dir(record).ok();
    if let Some(ref dir) = state_dir
        && let Ok(rd) = std::fs::read_dir(dir)
    {
        for ent in rd.flatten() {
            let name = ent.file_name();
            let name = name.to_string_lossy();
            if this_run_gate_slug(&name, "cross-model-", attempts).is_some() {
                out.push(ent.path());
            }
        }
    }
    // Track-dir `*.gate{n}.md` may remain across runs; list only when the
    // matching state-dir archive exists (fresh `run` wipes `{state_dir}/reviews/`).
    if let Some(id) = track_id
        && let Some(track_dir) = resolve_track_dir(record, id)
        && let Some(ref state_dir) = state_dir
        && let Ok(rd) = std::fs::read_dir(&track_dir)
    {
        for ent in rd.flatten() {
            let name = ent.file_name();
            let name = name.to_string_lossy();
            if let Some(slug) = this_run_gate_slug(&name, "review.", attempts) {
                let any_pair = (1..=attempts).any(|n| {
                    state_dir
                        .join(format!("cross-model-{slug}.gate{n}.md"))
                        .is_file()
                });
                if any_pair {
                    out.push(ent.path());
                }
            }
        }
    }
    out.sort();
    out
}

/// Injected into Grok-bound phases (plan / fold / implement / advance).
pub fn phase_prompt(record: &ProjectRecord, phase: &str, track_id: Option<&str>) -> String {
    let track = track_id.unwrap_or("(none)");
    let layout = layout_block(record, track_id, phase);
    let clause = super::self_check::phase_clause(record);
    let body = match phase {
        PHASE_PLAN => {
            let path = workspace_skill(record, "plan");
            format!(
                "If spec.md and plan.md already exist in the track folder: Coordinator \
                 has refreshed the evidence.md stamp. Do not write evidence.md. Do not \
                 load the plan skill. Do not run cargo, ledgerful, or ai-brains. Do not \
                 read Coordinator product source. Then end this turn.\n\
                 Otherwise:\n\
                 {}\n\
                 {RESEARCH}\n\
                 Write spec.md and plan.md in the track folder. Mark the track Ready.\n\
                 {END_TURN}\n",
                honor_skill("plan", &path)
            )
        }
        PHASE_FOLD => {
            let path = workspace_skill(record, "foldin");
            let limitation = super::decision::RECORD_LIMITATION;
            let inject = super::decision::inject_for(record, track_id);
            format!(
                "{}\n\
                 Fold the track `*-review.md` files (agy-review / opencode-review) into spec and plan.\n\
                 Do not write or truncate evidence.md. Do not reconstruct owner sentences from recall.\n\
                 Read the track `evidence.md` only through the injected block below. Do not parse the file yourself to decide what is live.\n\
                 Authenticate a sentence if and only if the inject is an `active decision by=` block and the sentence under test is that sentence.\n\
                 `active decision: none` and `active decision: invalid` authenticate nothing.\n\
                 A sentence that is merely present in the file, or written outside the active block, is not an owner decision.\n\
                 {limitation}\n\
                 {inject}\
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
                 If the spec execution path is the workspace (planning-only): if evidence.md \
                 is absent, you may create it; if evidence.md exists, do not replace it and \
                 do not reduce it to a stamp. Do not edit the execution repo and do not run \
                 cargo, ledgerful, or ai-brains there.\n\
                 {RESEARCH}\n\
                 {clause}\
                 {END_TURN}\n"
            )
        }
        PHASE_ADDRESS_FINDINGS => {
            let implement = execution_skill(record, "implement");
            let onboarding = execution_skill(record, "onboarding");
            let attempts = crate::state::load_run_state(record)
                .map(|s| s.address_findings_attempts)
                .unwrap_or(0);
            let archives = list_gate_archives(record, track_id, attempts);
            let mut archive_lines = String::from(
                "Honor archived gate reports under the track dir (and state reviews dir).\n",
            );
            if archives.is_empty() {
                archive_lines.push_str(
                    "No `review.*.gate*.md` or `cross-model-*.gate*.md` listed yet; still honor \
                     any such files under the track folder.\n",
                );
            } else {
                for p in archives {
                    archive_lines.push_str(&format!("- {}\n", p.display()));
                }
            }
            format!(
                "Honor project skills. This phase loads the `implement` skill from {implement} \
                 and the `onboarding` skill from {onboarding}.\n\
                 Honor the track spec execution path.\n\
                 {RESEARCH}\n\
                 Address every finding above low; lows may go to `deferred.md`. \
                 Do not plan, fold, or advance. Do not emit `next_track:`.\n\
                 {archive_lines}\
                 {clause}\
                 {END_TURN}\n"
            )
        }
        PHASE_ADVANCE => {
            let path = workspace_skill(record, "plan");
            format!(
                "{}\n\
                 Coordinator derives the next Ready row from `conductor.md`. \
                 The last line of your reply must be `next_track: <id>` or `next_track: null` \
                 (recorded; Coordinator overrides). Soft-next / `null` is not backlog-clear.\n\
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
            notify_progress: false,
            worktree_isolation: false,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
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
            notify_progress: false,
            worktree_isolation: false,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
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
        assert!(text.contains("Otherwise:"));
        assert!(text.contains("evidence.md"));
        assert!(text.contains("Do not write evidence.md"));
        assert!(!text.contains("timestamp only"));
        assert!(!text.contains("coordinator --version"));
        assert!(text.contains("Do not load the plan skill"));
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
        assert!(text.contains("Do not write or truncate evidence.md"));
        assert!(text.contains("Do not reconstruct owner sentences from recall"));
        assert!(text.contains("only through the injected block below"));
        assert!(text.contains("Do not parse the file yourself to decide what is live"));
        assert!(text.contains("active decision by="));
        assert!(text.contains("authenticate nothing"));
        assert!(text.contains("not an owner decision"));
        assert!(text.contains(crate::workflow::decision::RECORD_LIMITATION));
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

    fn decision_record(root: &std::path::Path) -> ProjectRecord {
        ProjectRecord {
            id: "decision-prompt".into(),
            path: root.to_path_buf(),
            display_name: None,
            layout_profile: LayoutProfile::Nested,
            conductor_dir: Some(root.join("conductor")),
            execution_repo: Some(root.join("exec")),
            execution_repos: BTreeMap::new(),
            state_dir: Some(root.join("state")),
            auto_merge: false,
            phase_timeouts_secs: BTreeMap::new(),
            notify_progress: false,
            worktree_isolation: false,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn fold_prompt_injects_none_one_and_invalid_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let conductor = dir.path().join("conductor");
        let none_dir = conductor.join("0077-None");
        let one_dir = conductor.join("0078-One");
        let many_dir = conductor.join("0079-Many");
        std::fs::create_dir_all(&none_dir).unwrap();
        std::fs::create_dir_all(&one_dir).unwrap();
        std::fs::create_dir_all(&many_dir).unwrap();
        let rec = decision_record(dir.path());
        let limitation = crate::workflow::decision::RECORD_LIMITATION;

        let none_path = none_dir.join("evidence.md");
        let none = phase_prompt(&rec, "fold", Some("0077"));
        assert!(none.contains("active decision: none\n"), "{none}");
        assert!(none.contains(limitation), "{none}");
        assert!(none.contains("Do not reconstruct owner sentences from recall"));
        assert!(
            !none_path.exists(),
            "building the prompt must not write evidence.md"
        );

        let sentence = "Ship the channel.";
        let body = format!(
            "<!-- coordinator:decision active by=\"Ada\" recorded_at=\"2026-10-06T12:00:00Z\" -->\n{sentence}\n<!-- /coordinator:decision -->\n"
        );
        let one_path = one_dir.join("evidence.md");
        std::fs::write(&one_path, &body).unwrap();
        let one = phase_prompt(&rec, "fold", Some("0078"));
        assert!(
            one.contains(
                "active decision by=\"Ada\" recorded_at=\"2026-10-06T12:00:00Z\":\nShip the channel.\n"
            ),
            "{one}"
        );
        assert_eq!(std::fs::read_to_string(&one_path).unwrap(), body);

        let many = format!("{body}{body}");
        let many_path = many_dir.join("evidence.md");
        std::fs::write(&many_path, &many).unwrap();
        let bad = phase_prompt(&rec, "fold", Some("0079"));
        assert!(bad.contains("active decision: invalid\n"), "{bad}");
        assert!(bad.contains("authenticate nothing"), "{bad}");
        assert_eq!(std::fs::read_to_string(&many_path).unwrap(), many);
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
        assert!(text.contains("do not replace it"));
        assert!(text.contains("do not reduce it to a stamp"));
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
    fn address_findings_contract_names_skills_research_and_forbids_next_track() {
        let rec = nested_record();
        let text = phase_prompt(&rec, "address-findings", Some("0031"));
        assert!(text.contains("address-findings"));
        assert!(!text.contains("Unknown phase"));
        assert!(text.contains("Honor project skills"));
        assert!(
            contains_skill(&text, "implement") || text.contains("implement"),
            "names implement skill: {text}"
        );
        assert!(text.contains("onboarding"));
        assert!(text.contains("Knowledge is stale") || text.contains("primary sources"));
        assert!(text.contains("Do not") && text.contains("outcome write"));
        assert!(
            text.contains("do not emit `next_track:`")
                || text.contains("Do not emit `next_track:`")
        );
        assert!(text.contains("gate reports") || text.contains("gate"));
        assert!(text.contains("end this turn") || text.contains("end the turn"));
        let n = text.replace('\\', "/");
        assert!(n.contains("OrcaSlicer-ZR/.agents/skills/implement/SKILL.md"));
        assert!(n.contains("OrcaSlicer-ZR/.agents/skills/onboarding/SKILL.md"));
    }

    #[test]
    fn address_findings_prompt_lists_this_run_gates_not_leftover() {
        use crate::config::test_env_lock;
        use crate::run::run_with_driver;
        use crate::state::{load_run_state, save_run_state};
        use crate::workflow::WorkflowDriver;
        use tempfile::tempdir;
        use uuid::Uuid;

        let _g = test_env_lock();
        let dir = tempdir().unwrap();
        let track = dir.path().join("conductor").join("0031-Example");
        std::fs::create_dir_all(&track).unwrap();
        let rec = ProjectRecord {
            id: Uuid::new_v4().to_string(),
            path: dir.path().to_path_buf(),
            display_name: None,
            layout_profile: LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: Some(dir.path().to_path_buf()),
            execution_repos: BTreeMap::new(),
            state_dir: None,
            auto_merge: true,
            phase_timeouts_secs: BTreeMap::new(),
            notify_progress: false,
            worktree_isolation: false,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            created_at: Utc::now(),
        };
        run_with_driver(&rec, Some("0031".into()), WorkflowDriver::FileWait).unwrap();
        let mut s = load_run_state(&rec).unwrap();
        s.address_findings_attempts = 1;
        save_run_state(&rec, &s).unwrap();
        std::fs::write(track.join("review.codex.gate1.md"), "this run").unwrap();
        std::fs::write(track.join("review.codex.gate2.md"), "leftover").unwrap();
        std::fs::write(track.join("review.codex.fail.md"), "prior fail audit").unwrap();
        std::fs::write(track.join("review.codex.fail.gate1.md"), "mis-archive").unwrap();
        std::fs::write(track.join("review.claude.gate1.md"), "prior run leftover").unwrap();
        let reviews = crate::state::resolve_state_dir(&rec)
            .unwrap()
            .join("reviews");
        std::fs::create_dir_all(&reviews).unwrap();
        std::fs::write(reviews.join("cross-model-codex.gate1.md"), "this run").unwrap();
        std::fs::write(reviews.join("cross-model-codex.gate2.md"), "leftover").unwrap();
        let text = phase_prompt(&rec, "address-findings", Some("0031"));
        let n = text.replace('\\', "/");
        assert!(n.contains("review.codex.gate1.md"), "{text}");
        assert!(n.contains("cross-model-codex.gate1.md"), "{text}");
        assert!(!n.contains("gate2.md"), "stale leftover listed: {text}");
        assert!(
            !n.contains("review.codex.fail"),
            "prior fail audit listed: {text}"
        );
        assert!(
            !n.contains("review.claude.gate1"),
            "prior-run leftover listed: {text}"
        );
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
        assert!(
            text.contains("recorded") && text.contains("overrides"),
            "advance records next_track and overrides: {text}"
        );
        assert!(
            text.contains("not backlog-clear"),
            "soft-next / null is not backlog-clear: {text}"
        );
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
