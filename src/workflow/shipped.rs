//! Skip already-shipped track ids at omit-pick / `finish_advance` (track 0046).

use std::path::Path;

use crate::error::{CoordinatorError, Result};
use crate::notify::artifact::track_ids_match;
use crate::registry::ProjectRecord;
use crate::state::RunState;

use super::conductor_md::{load_track_rows, track_row_completed};

/// Quoted GitHub search for `gh pr list --search` (`"track(NNNN)" in:title`).
pub fn merged_search_query(numeric_id: &str) -> String {
    format!("\"track({numeric_id})\" in:title")
}

/// After trim, case-insensitive prefix `track(NNNN):` (colon required).
pub fn pr_title_is_track(title: &str, numeric_id: &str) -> bool {
    let title = title.trim();
    let id = numeric_id.trim();
    if id.len() != 4 || !id.as_bytes().iter().all(u8::is_ascii_digit) {
        return false;
    }
    let prefix = format!("track({id}):");
    let Some(head) = title.get(..prefix.len()) else {
        return false;
    };
    head.eq_ignore_ascii_case(&prefix)
}

/// Local shipped proof: conductor Completed, or this-run `ci.merge` done/queued on the same id.
pub fn track_is_shipped_local(record: &ProjectRecord, state: &RunState, id: &str) -> bool {
    if let Some(rows) = load_track_rows(record)
        && track_row_completed(&rows, id)
    {
        return true;
    }
    let merge = state
        .ci
        .as_ref()
        .and_then(|c| c.merge.as_deref())
        .map(str::trim);
    let merge_shipped = matches!(merge, Some("done" | "queued"));
    matches!(
        state.track_id.as_deref(),
        Some(cur) if track_ids_match(id, cur) && merge_shipped
    )
}

/// Adapter omit-pick: `Some(n)` skip, `Ok(None)` not found, `Err` fail-open at caller.
pub trait MergedTrackProbe {
    fn merged_pr_for_track(&self, cwd: &Path, numeric_id: &str) -> Result<Option<u64>>;
}

/// First `track(NNNN):` title in a `gh pr list --json` array. No process.
pub fn first_merged_pr_for_track(
    json: &str,
    numeric_id: &str,
    default_branch: Option<&str>,
) -> Result<Option<u64>> {
    let rows: Vec<serde_json::Value> = serde_json::from_str(json)
        .map_err(|e| CoordinatorError::Message(format!("gh pr list json: {e}")))?;
    for row in rows {
        let Some(number) = row.get("number").and_then(|n| n.as_u64()) else {
            continue;
        };
        let Some(title) = row.get("title").and_then(|t| t.as_str()) else {
            continue;
        };
        if row.get("mergedAt").is_none()
            || matches!(row.get("mergedAt"), Some(serde_json::Value::Null))
        {
            continue;
        }
        if let Some(serde_json::Value::String(s)) = row.get("mergedAt")
            && s.is_empty()
        {
            continue;
        }
        if let Some(def) = default_branch
            && let Some(base) = row.get("baseRefName").and_then(|b| b.as_str())
            && !base.is_empty()
            && base != def
        {
            continue;
        }
        if pr_title_is_track(title, numeric_id) {
            return Ok(Some(number));
        }
    }
    Ok(None)
}

#[cfg(test)]
use std::cell::RefCell;
#[cfg(test)]
use std::sync::Arc;

#[cfg(test)]
thread_local! {
    static TEST_PROBE: RefCell<Option<Arc<dyn MergedTrackProbe>>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub struct TestMergedProbeGuard;

#[cfg(test)]
impl Drop for TestMergedProbeGuard {
    fn drop(&mut self) {
        TEST_PROBE.with(|c| *c.borrow_mut() = None);
    }
}

#[cfg(test)]
pub fn install_test_merged_probe(probe: Arc<dyn MergedTrackProbe>) -> TestMergedProbeGuard {
    TEST_PROBE.with(|c| *c.borrow_mut() = Some(probe));
    TestMergedProbeGuard
}

/// `#[cfg(test)]` unset → `None` (never a live `GhMergedTrackProbe`).
#[cfg(test)]
pub fn installed_merged_probe() -> Option<Arc<dyn MergedTrackProbe>> {
    TEST_PROBE.with(|c| c.borrow().clone())
}

#[cfg(test)]
#[derive(Debug, Default)]
pub struct ScriptedMergedProbe {
    pub replies: std::collections::HashMap<String, std::result::Result<Option<u64>, String>>,
}

#[cfg(test)]
impl ScriptedMergedProbe {
    pub fn found(id: &str, n: u64) -> Self {
        let mut p = Self::default();
        p.replies.insert(id.to_string(), Ok(Some(n)));
        p
    }

    pub fn err(id: &str, msg: &str) -> Self {
        let mut p = Self::default();
        p.replies.insert(id.to_string(), Err(msg.to_string()));
        p
    }
}

#[cfg(test)]
impl MergedTrackProbe for ScriptedMergedProbe {
    fn merged_pr_for_track(&self, _cwd: &Path, numeric_id: &str) -> Result<Option<u64>> {
        match self.replies.get(numeric_id) {
            Some(Ok(v)) => Ok(*v),
            Some(Err(e)) => Err(CoordinatorError::Message(e.clone())),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::LayoutProfile;
    use crate::notify::artifact::numeric_track_id;
    use crate::state::{CiWatchState, RunState};
    use tempfile::tempdir;
    use uuid::Uuid;

    fn rec(path: &std::path::Path) -> ProjectRecord {
        ProjectRecord {
            id: Uuid::new_v4().to_string(),
            path: path.to_path_buf(),
            display_name: None,
            layout_profile: LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: std::collections::BTreeMap::new(),
            state_dir: None,
            auto_merge: true,
            phase_timeouts_secs: std::collections::BTreeMap::new(),
            notify_progress: false,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            created_at: chrono::Utc::now(),
        }
    }

    fn write_status(ws: &std::path::Path, id: &str, status: &str) {
        let cond = ws.join("conductor");
        std::fs::create_dir_all(cond.join(format!("{id}-Fixture"))).unwrap();
        let md = format!(
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | {id}-Fixture | `.` | **{status}** | fixture |\n"
        );
        std::fs::write(cond.join("conductor.md"), md).unwrap();
    }

    #[test]
    fn pr_title_is_track_table() {
        assert!(pr_title_is_track("track(0047): foo", "0047"));
        assert!(pr_title_is_track("TRACK(0047): x", "0047"));
        assert!(pr_title_is_track("  track(0047): spaced", "0047"));
        assert!(!pr_title_is_track("feat: track(0047):", "0047"));
        assert!(!pr_title_is_track("WIP: track(0047):", "0047"));
        assert!(!pr_title_is_track("track(0047)", "0047"));
        assert!(!pr_title_is_track("track(0048):", "0047"));
        assert!(!pr_title_is_track("track(0047):", "47"));
        // UTF-8: prefix-byte slice must not panic on a mid-char boundary.
        assert!(!pr_title_is_track("0123456789🎉 more", "0047"));
    }

    #[test]
    fn merged_search_query_includes_quotes() {
        assert_eq!(merged_search_query("0045"), "\"track(0045)\" in:title");
    }

    #[test]
    fn first_merged_pr_filters_title_and_base() {
        let json = r#"[
            {"number":1,"title":"WIP: track(0045): no","mergedAt":"2026-01-01T00:00:00Z","baseRefName":"main"},
            {"number":46,"title":"track(0045): auto-settle","mergedAt":"2026-09-10T15:28:08Z","baseRefName":"main"}
        ]"#;
        assert_eq!(
            first_merged_pr_for_track(json, "0045", Some("main"))
                .unwrap()
                .unwrap(),
            46
        );
        let other_base = r#"[{"number":9,"title":"track(0045): x","mergedAt":"2026-01-01T00:00:00Z","baseRefName":"dev"}]"#;
        assert_eq!(
            first_merged_pr_for_track(other_base, "0045", Some("main")).unwrap(),
            None
        );
        let null_merged =
            r#"[{"number":3,"title":"track(0045): x","mergedAt":null,"baseRefName":"main"}]"#;
        assert_eq!(
            first_merged_pr_for_track(null_merged, "0045", Some("main")).unwrap(),
            None
        );
        let missing_merged = r#"[{"number":4,"title":"track(0045): x","baseRefName":"main"}]"#;
        assert_eq!(
            first_merged_pr_for_track(missing_merged, "0045", Some("main")).unwrap(),
            None
        );
    }

    #[test]
    fn track_is_shipped_local_merge_and_completed() {
        let dir = tempdir().unwrap();
        write_status(dir.path(), "0001", "Ready — not started");
        let r = rec(dir.path());
        let mut state = RunState::idle(&r.id);
        state.track_id = Some("0001".into());
        state.ci = Some(CiWatchState {
            merge: Some("done".into()),
            ..Default::default()
        });
        assert!(track_is_shipped_local(&r, &state, "0001"));
        state.ci.as_mut().unwrap().merge = Some("queued".into());
        assert!(track_is_shipped_local(&r, &state, "0001"));
        state.ci.as_mut().unwrap().merge = Some("skipped".into());
        assert!(!track_is_shipped_local(&r, &state, "0001"));
        assert!(!track_is_shipped_local(&r, &state, "0002"));

        write_status(dir.path(), "0001", "Completed");
        let mut idle = RunState::idle(&r.id);
        idle.ci = None;
        assert!(track_is_shipped_local(&r, &idle, "0001"));
    }

    #[test]
    fn numeric_track_id_is_crate_visible() {
        assert_eq!(numeric_track_id("0045-Skip"), Some("0045"));
        assert_eq!(numeric_track_id("0045"), Some("0045"));
    }
}
