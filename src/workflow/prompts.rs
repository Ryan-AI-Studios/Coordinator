//! Short phase prompt constants (not a prompt DSL).

use crate::registry::ProjectRecord;

use super::graph::resolve_track_dir;

/// Injected into Grok-bound phases (plan / fold / implement / advance).
pub fn phase_prompt(record: &ProjectRecord, phase: &str, track_id: Option<&str>) -> String {
    let paths = crate::layout::resolve(record);
    let track = track_id.unwrap_or("(none)");
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
    format!(
        "Coordinator phase `{phase}` for track `{track}`.\n\
         Honor project skills (plan, review-track, foldin, implement) as applicable.\n\
         Workspace root: {workspace}\n\
         Execution repo: {execution}\n\
         Conductor directory: {conductor}\n\
         State directory: {state}\n\
         {track_hint}\
         Read spec.md + plan.md in the track folder when present.\n\
         Planning, conductor tracks, ADRs, and deferred.md stay outside the execution repo. \
         Never commit them into product git.\n\
         plan / fold / advance write under the workspace / conductor / track folder, \
         not inside the execution repo unless the track spec says the execution path is the workspace.\n\
         implement honors the track spec execution path.\n\
         When done, write a Phase Outcome via `coordinator outcome write` \
         (or the adapter-owned write).\n"
    )
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
            created_at: Utc::now(),
        }
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
    }

    #[test]
    fn unset_execution_repo_is_explicit() {
        let mut rec = nested_record();
        rec.execution_repo = None;
        let text = phase_prompt(&rec, "plan", None);
        assert!(text.contains("Execution repo: (unset)"));
    }
}
