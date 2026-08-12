//! Short phase prompt constants (not a prompt DSL).

use crate::registry::ProjectRecord;

use super::graph::resolve_track_dir;

/// Injected into Grok-bound phases (plan / fold / implement / advance).
pub fn phase_prompt(record: &ProjectRecord, phase: &str, track_id: Option<&str>) -> String {
    let paths = crate::layout::resolve(record);
    let track = track_id.unwrap_or("(none)");
    let conductor = paths.conductor_dir.display();
    let track_hint = track_id
        .and_then(|id| resolve_track_dir(record, id))
        .map(|p| format!("Track folder: {}\n", p.display()))
        .unwrap_or_default();
    format!(
        "Coordinator phase `{phase}` for track `{track}`.\n\
         Honor project skills (plan, review-track, foldin, implement) as applicable.\n\
         Conductor directory: {conductor}\n\
         {track_hint}\
         Read spec.md + plan.md in the track folder when present.\n\
         When done, write a Phase Outcome via `coordinator outcome write` \
         (or the adapter-owned write).\n"
    )
}
