//! Per-phase inject contracts (not a prompt DSL).

use std::path::{Path, PathBuf};

use crate::registry::ProjectRecord;
use crate::workflow::graph::{
    PHASE_ADDRESS_CI, PHASE_ADDRESS_FINDINGS, PHASE_ADVANCE, PHASE_FOLD, PHASE_IMPLEMENT,
    PHASE_PLAN,
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
    let use_epoch = epoch.is_some()
        && matches!(
            phase,
            PHASE_IMPLEMENT | PHASE_ADDRESS_FINDINGS | PHASE_ADDRESS_CI
        );
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

/// On-disk `plan.md` for the plan-phase branch. A read error is `Unreadable`
/// (fast-skip when `spec.md` is a file). It is not a panic and not `Err`.
enum PlanOnDisk {
    Missing,
    Unreadable,
    Text(String),
}

/// Content predicate. Case-sensitive. No byte or line threshold.
/// One leading U+FEFF is ignored. Empty trim is a placeholder.
pub(crate) fn plan_markdown_is_placeholder(body: &str) -> bool {
    let body = body.strip_prefix('\u{FEFF}').unwrap_or(body);
    if body.trim().is_empty() {
        return true;
    }
    body.lines().any(|line| {
        let line = line.trim();
        is_unfilled_heading(line) || is_placeholder_comment(line) || is_template_blockquote(line)
    })
}

fn is_placeholder_dash(dash: char) -> bool {
    matches!(dash, '\u{2014}' | '-' | '\u{2013}')
}

/// Unfilled skeleton only: number `0000`, title token `<Track Title>`, same dash both sides.
fn is_unfilled_heading(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("# 0000 ") else {
        return false;
    };
    let mut chars = rest.chars();
    let Some(dash) = chars.next() else {
        return false;
    };
    if !is_placeholder_dash(dash) {
        return false;
    }
    let Some(rest) = chars.as_str().strip_prefix(" <Track Title> ") else {
        return false;
    };
    let mut chars = rest.chars();
    let Some(dash2) = chars.next() else {
        return false;
    };
    dash == dash2 && chars.as_str() == " Plan"
}

fn is_placeholder_comment(line: &str) -> bool {
    line == "<!-- coordinator:placeholder-plan -->" || line == "<!--coordinator:placeholder-plan-->"
}

fn is_template_blockquote(line: &str) -> bool {
    line.starts_with("> Template. Phased checklist")
        || line.starts_with("> Template. Replace placeholders")
}

fn plan_on_disk_is_fast_skip(plan: &PlanOnDisk) -> bool {
    match plan {
        PlanOnDisk::Unreadable => true,
        PlanOnDisk::Text(body) => !plan_markdown_is_placeholder(body),
        PlanOnDisk::Missing => false,
    }
}

fn read_plan_markdown(path: &Path) -> PlanOnDisk {
    if !path.is_file() {
        return PlanOnDisk::Missing;
    }
    match std::fs::read_to_string(path) {
        Ok(text) => PlanOnDisk::Text(text),
        Err(_) => PlanOnDisk::Unreadable,
    }
}

/// `(spec.md is a file, plan.md)`. No track id or no dir is spec-missing and plan `Missing`.
fn read_plan_inputs(record: &ProjectRecord, track_id: Option<&str>) -> (bool, PlanOnDisk) {
    let Some(id) = track_id else {
        return (false, PlanOnDisk::Missing);
    };
    let Some(dir) = resolve_track_dir(record, id) else {
        return (false, PlanOnDisk::Missing);
    };
    (
        dir.join("spec.md").is_file(),
        read_plan_markdown(&dir.join("plan.md")),
    )
}

fn plan_fast_skip_body() -> String {
    format!(
        "If spec.md and plan.md already exist in the track folder: Coordinator \
         has refreshed the evidence.md stamp. Do not write evidence.md. Do not \
         load the plan skill. Do not run cargo, ledgerful, or ai-brains. Do not \
         read Coordinator product source. Then end this turn.\n\
         {END_TURN}\n"
    )
}

fn plan_expand_write() -> String {
    format!(
        "{RESEARCH}\n\
         Write spec.md and plan.md in the track folder. Remove any placeholder-marker line \
         from plan.md. Mark the track Ready.\n\
         {END_TURN}\n"
    )
}

/// One branch. Fast-skip is `spec.md` present and a real or unreadable plan.
/// The stamp gate stays on file presence and does not call this.
fn plan_prompt_body(record: &ProjectRecord, track_id: Option<&str>) -> String {
    let path = workspace_skill(record, "plan");
    let (spec_is_file, plan) = read_plan_inputs(record, track_id);
    if spec_is_file && plan_on_disk_is_fast_skip(&plan) {
        return plan_fast_skip_body();
    }
    let skill_is_file = skill_md(&crate::layout::resolve(record).workspace_root, "plan").is_file();
    let placeholder =
        matches!(plan, PlanOnDisk::Text(ref body) if plan_markdown_is_placeholder(body));
    let write = plan_expand_write();
    if skill_is_file {
        format!("{}\n{write}", honor_skill("plan", &path))
    } else if placeholder {
        format!(
            "placeholder present, skill absent. The `plan` skill file at {path} was not \
             loaded. This is not a fast-skip. Do not claim the skill was loaded.\n\
             {write}"
        )
    } else {
        format!("Plan skill file is absent at {path}. This is not a fast-skip.\n{write}")
    }
}

/// Injected into Grok-bound phases (plan / fold / implement / advance).
pub fn phase_prompt(record: &ProjectRecord, phase: &str, track_id: Option<&str>) -> String {
    let track = track_id.unwrap_or("(none)");
    let layout = layout_block(record, track_id, phase);
    let clause = super::self_check::phase_clause(record);
    let body = match phase {
        PHASE_PLAN => plan_prompt_body(record, track_id),
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
        PHASE_ADDRESS_CI => address_ci_body(record),
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

const ADDRESS_CI_CONTEXT_CAP: usize = 4096;

fn truncate_scalars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        text.chars().take(max).collect()
    }
}

/// `address-ci` inject. Does not load the implement-track publish loop or the self-check clause.
fn address_ci_body(record: &ProjectRecord) -> String {
    let state = crate::state::load_run_state(record).ok();
    let attempts = state.as_ref().map(|s| s.ci_fix_attempts).unwrap_or(0);
    let request = state.as_ref().and_then(|s| s.ci_fix_request.clone());
    let pr = request
        .as_ref()
        .map(|req| req.pr_number.to_string())
        .unwrap_or_else(|| "(none)".into());
    let sha = request
        .as_ref()
        .map(|req| req.from_sha.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(none)".into());
    let mut checks = String::new();
    if let Some(req) = request {
        for check in req.checks {
            checks.push_str(&format!(
                "- {} bucket={} description={} link={}\n",
                check.name, check.bucket, check.description, check.link
            ));
        }
    }
    if checks.is_empty() {
        checks.push_str("- (no failing required checks stored)\n");
    }
    let head = format!(
        "This phase is `address-ci`, attempt {attempts} of {}.\n\
         PR #{pr}\n\
         from_sha: {sha}\n\
         Commit a real diff against `from_sha` and end the turn.\n\
         Do not run `git push`, `gh pr create`, or `gh pr merge`.\n\
         Do not load the implement-track publish loop.\n\
         Failing required checks:\n",
        crate::workflow::graph::CI_FIX_CAP
    );
    let room = ADDRESS_CI_CONTEXT_CAP.saturating_sub(head.chars().count());
    let checks = truncate_scalars(&checks, room);
    let body = format!("{head}{checks}");
    truncate_scalars(&body, ADDRESS_CI_CONTEXT_CAP)
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
            ci_fix_routing: false,
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
            ci_fix_routing: false,
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

    const LIVE_TEMPLATE_HEADING: &str = "# 0000 \u{2014} <Track Title> \u{2014} Plan";
    const ORCA_0099_HEADING: &str = "# 0099 \u{2014} Coordinator Dogfood Probe \u{2014} Plan";

    fn temp_project(root: &std::path::Path) -> ProjectRecord {
        ProjectRecord {
            id: "0085-temp".into(),
            path: root.to_path_buf(),
            display_name: None,
            layout_profile: LayoutProfile::Nested,
            conductor_dir: Some(root.join("conductor")),
            execution_repo: Some(root.join("exec")),
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
            ci_fix_routing: false,
            created_at: Utc::now(),
        }
    }

    fn write_track(root: &std::path::Path, spec: Option<&str>, plan: Option<&str>) {
        let track = root.join("conductor").join("0001-Minted");
        std::fs::create_dir_all(&track).unwrap();
        if let Some(body) = spec {
            std::fs::write(track.join("spec.md"), body).unwrap();
        }
        if let Some(body) = plan {
            std::fs::write(track.join("plan.md"), body).unwrap();
        }
    }

    fn write_plan_skill(root: &std::path::Path) {
        let skill = root.join(".agents").join("skills").join("plan");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "plan skill\n").unwrap();
    }

    fn assert_fast_skip(text: &str) {
        for needle in [
            "already exist",
            "Do not write evidence.md",
            "Do not load the plan skill",
            "Do not run cargo",
            "Do not read Coordinator product source",
            "end this turn",
            "outcome write",
        ] {
            assert!(text.contains(needle), "missing {needle}: {text}");
        }
        for needle in [
            "Otherwise:",
            "Write spec.md and plan.md",
            "Honor project skills",
            "Knowledge is stale",
            "placeholder present, skill absent",
        ] {
            assert!(!text.contains(needle), "unexpected {needle}: {text}");
        }
        assert!(!text.contains("timestamp only"), "{text}");
        assert!(!text.contains("coordinator --version"), "{text}");
    }

    fn assert_expand_with_skill(text: &str, skill_path: &str) {
        let honor = text.find("Honor project skills").expect(text);
        let research = text.find("Knowledge is stale").expect(text);
        let write = text
            .find("Write spec.md and plan.md in the track folder")
            .expect(text);
        let remove = text.find("Remove any placeholder-marker line").expect(text);
        let ready = text.find("Mark the track Ready").expect(text);
        let end = text.find("end this turn").expect(text);
        assert!(
            honor < research && research < write && write < remove && remove < ready && ready < end,
            "{text}"
        );
        assert!(text.contains(skill_path), "{text}");
        assert!(text.contains("outcome write"), "{text}");
        assert!(!text.contains("Do not load the plan skill"), "{text}");
        assert!(
            !text.contains("placeholder present, skill absent"),
            "{text}"
        );
        assert!(!text.contains("Otherwise:"), "{text}");
    }

    #[test]
    fn empty_whitespace_and_bom_comment_are_placeholders() {
        assert!(plan_markdown_is_placeholder(""));
        assert!(plan_markdown_is_placeholder(" \n\t \n"));
        assert!(plan_markdown_is_placeholder(
            "\u{FEFF}<!-- coordinator:placeholder-plan -->\n"
        ));
    }

    #[test]
    fn crlf_comment_is_placeholder() {
        assert!(plan_markdown_is_placeholder(
            "<!--coordinator:placeholder-plan-->\r\n"
        ));
    }

    #[test]
    fn live_template_heading_and_dash_skeletons_are_placeholders() {
        assert!(plan_markdown_is_placeholder(LIVE_TEMPLATE_HEADING));
        assert!(plan_markdown_is_placeholder(
            "# 0000 - <Track Title> - Plan"
        ));
        assert!(plan_markdown_is_placeholder(
            "# 0000 \u{2013} <Track Title> \u{2013} Plan"
        ));
        assert!(!plan_markdown_is_placeholder(
            "# 0000 \u{2014} <Track Title> - Plan"
        ));
        assert!(!plan_markdown_is_placeholder(
            "# 0001 \u{2014} <Track Title> \u{2014} Plan"
        ));
        assert!(!plan_markdown_is_placeholder(
            "# 0000 \u{2014} Product Repo Bootstrap \u{2014} Plan"
        ));
    }

    #[test]
    fn blockquote_prefixes_match_and_other_template_lines_do_not() {
        assert!(plan_markdown_is_placeholder(
            "> Template. Phased checklist; map each phase to the DoD items in spec.md\n"
        ));
        assert!(plan_markdown_is_placeholder(
            "> Template. Replace placeholders; map phases to spec section 7.\n"
        ));
        assert!(!plan_markdown_is_placeholder(
            "> Template. This sentence is a finished note.\n"
        ));
    }

    #[test]
    fn comment_spellings_match_and_other_spacing_does_not() {
        assert!(plan_markdown_is_placeholder(
            "<!-- coordinator:placeholder-plan -->"
        ));
        assert!(plan_markdown_is_placeholder(
            "<!--coordinator:placeholder-plan-->"
        ));
        assert!(!plan_markdown_is_placeholder(
            "<!--  coordinator:placeholder-plan -->"
        ));
        assert!(!plan_markdown_is_placeholder(
            "<!-- coordinator:placeholder-plan-->"
        ));
        assert!(!plan_markdown_is_placeholder(
            "<!--coordinator:placeholder-plan -->"
        ));
    }

    #[test]
    fn short_real_plan_and_filled_headings_are_not_placeholders() {
        assert!(!plan_markdown_is_placeholder("# plan\n"));
        assert!(!plan_markdown_is_placeholder(&format!(
            "{ORCA_0099_HEADING}\n"
        )));
        assert!(!plan_markdown_is_placeholder(
            "The words > Template. Phased checklist appear in this sentence.\n"
        ));
        assert!(!plan_markdown_is_placeholder(&format!(
            "See {LIVE_TEMPLATE_HEADING} in the template.\n"
        )));
    }

    #[test]
    fn plan_on_disk_fast_skip_matches_predicate() {
        assert!(plan_on_disk_is_fast_skip(&PlanOnDisk::Unreadable));
        assert!(!plan_on_disk_is_fast_skip(&PlanOnDisk::Missing));
        assert!(plan_on_disk_is_fast_skip(&PlanOnDisk::Text(
            "# plan\n".into()
        )));
        assert!(!plan_on_disk_is_fast_skip(&PlanOnDisk::Text(
            "> Template. Phased checklist\n".into()
        )));
    }

    #[test]
    fn real_plan_prompt_fast_skips() {
        let dir = tempfile::tempdir().unwrap();
        write_track(dir.path(), Some("# spec\n"), Some("# plan\n"));
        write_plan_skill(dir.path());
        let rec = temp_project(dir.path());
        let text = phase_prompt(&rec, "plan", Some("0001-Minted"));
        assert_fast_skip(&text);
        assert!(
            text.contains("spec.md") && text.contains("plan.md"),
            "{text}"
        );
    }

    #[test]
    fn placeholder_plan_prompt_loads_skill_when_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        write_track(
            dir.path(),
            Some("# spec\n"),
            Some(&format!("{LIVE_TEMPLATE_HEADING}\n")),
        );
        write_plan_skill(dir.path());
        let rec = temp_project(dir.path());
        let text = phase_prompt(&rec, "plan", Some("0001"));
        let skill = skill_md(dir.path(), "plan");
        assert_expand_with_skill(&text, &skill.display().to_string());
    }

    #[test]
    fn placeholder_plan_prompt_when_skill_file_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        write_track(
            dir.path(),
            Some("# spec\n"),
            Some("> Template. Replace placeholders\n"),
        );
        let rec = temp_project(dir.path());
        let text = phase_prompt(&rec, "plan", Some("0001"));
        let skill = skill_md(dir.path(), "plan").display().to_string();
        assert!(text.contains("placeholder present, skill absent"), "{text}");
        assert!(text.contains(&skill), "{text}");
        assert!(text.contains("This is not a fast-skip"), "{text}");
        assert!(text.contains("Do not claim the skill was loaded"), "{text}");
        assert!(text.contains("Write spec.md and plan.md"), "{text}");
        assert!(text.contains("Mark the track Ready"), "{text}");
        assert!(text.contains("Knowledge is stale"), "{text}");
        assert!(text.contains("end this turn"), "{text}");
        assert!(text.contains("outcome write"), "{text}");
        assert!(
            text.contains("Remove any placeholder-marker line"),
            "{text}"
        );
        assert!(!text.contains("Do not load the plan skill"), "{text}");
        assert!(!text.contains("Honor project skills"), "{text}");
        assert!(!text.contains("Otherwise:"), "{text}");
    }

    #[test]
    fn missing_plan_prompt_expands_when_skill_file_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        write_track(dir.path(), Some("# spec\n"), None);
        let rec = temp_project(dir.path());
        let text = phase_prompt(&rec, "plan", Some("0001"));
        let skill = skill_md(dir.path(), "plan").display().to_string();
        assert!(text.contains("Plan skill file is absent"), "{text}");
        assert!(text.contains(&skill), "{text}");
        assert!(text.contains("This is not a fast-skip"), "{text}");
        assert!(text.contains("Write spec.md and plan.md"), "{text}");
        assert!(text.contains("Knowledge is stale"), "{text}");
        assert!(text.contains("end this turn"), "{text}");
        assert!(text.contains("outcome write"), "{text}");
        assert!(
            !text.contains("placeholder present, skill absent"),
            "{text}"
        );
        assert!(!text.contains("Do not load the plan skill"), "{text}");
        assert!(!text.contains("Honor project skills"), "{text}");
    }

    #[test]
    fn missing_plan_prompt_loads_skill_when_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        write_track(dir.path(), Some("# spec\n"), None);
        write_plan_skill(dir.path());
        let rec = temp_project(dir.path());
        let text = phase_prompt(&rec, "plan", Some("0001"));
        let skill = skill_md(dir.path(), "plan");
        assert_expand_with_skill(&text, &skill.display().to_string());
    }

    #[test]
    fn missing_spec_forces_expand_even_when_real_plan_exists() {
        let dir = tempfile::tempdir().unwrap();
        write_track(dir.path(), None, Some("# plan\n"));
        write_plan_skill(dir.path());
        let rec = temp_project(dir.path());
        let text = phase_prompt(&rec, "plan", Some("0001"));
        let skill = skill_md(dir.path(), "plan");
        assert_expand_with_skill(&text, &skill.display().to_string());
        assert!(!text.contains("Do not load the plan skill"), "{text}");
    }

    #[test]
    fn no_track_id_or_dir_expands() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("conductor")).unwrap();
        write_plan_skill(dir.path());
        let rec = temp_project(dir.path());
        let missing_id = phase_prompt(&rec, "plan", None);
        let skill = skill_md(dir.path(), "plan");
        assert_expand_with_skill(&missing_id, &skill.display().to_string());
        let missing_dir = phase_prompt(&rec, "plan", Some("0099"));
        assert_expand_with_skill(&missing_dir, &skill.display().to_string());
        assert!(!missing_dir.contains("placeholder present, skill absent"));
    }

    #[cfg(windows)]
    #[test]
    fn unreadable_plan_file_fast_skips_without_panic() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        write_track(
            dir.path(),
            Some("# spec\n"),
            Some("<!-- coordinator:placeholder-plan -->\n"),
        );
        let plan = dir
            .path()
            .join("conductor")
            .join("0001-Minted")
            .join("plan.md");
        let _lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&plan)
            .unwrap();
        let rec = temp_project(dir.path());
        let text = phase_prompt(&rec, "plan", Some("0001"));
        assert_fast_skip(&text);
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
            ci_fix_routing: false,
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
            ci_fix_routing: false,
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
