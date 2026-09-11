//! Failure Artifact path, atomic markdown write, clear, and read.

use std::path::PathBuf;

use crate::error::Result;
use crate::persist::atomic_write;
use crate::registry::ProjectRecord;
use crate::state::resolve_state_dir;

use super::NotifyEvent;
use super::recovery::recommended_action;

/// Cap for `message` in the artifact (debug enough; not a transcript dump).
pub const MESSAGE_CAP: usize = 4096;

/// `{state_dir}/FAILURE.md`
pub fn path(record: &ProjectRecord) -> Result<PathBuf> {
    Ok(resolve_state_dir(record)?.join("FAILURE.md"))
}

/// Path if the artifact file currently exists.
pub fn existing_path(record: &ProjectRecord) -> Option<PathBuf> {
    let p = path(record).ok()?;
    p.is_file().then_some(p)
}

/// Best-effort remove of a leftover Failure Artifact (fresh `run`).
pub fn clear(record: &ProjectRecord) {
    if let Ok(p) = path(record)
        && p.exists()
    {
        let _ = std::fs::remove_file(&p);
    }
}

/// Operator ack `last_event` / progress_log detail (0040).
pub const LAST_EVENT_FAILURE_RESOLVED: &str = "failure: resolved by operator";
/// Display reason when the conductor row is Completed.
pub const SUPERSEDED_COMPLETED: &str = "conductor row Completed";
/// Display reason when artifact track/epoch disagrees with run-state.
pub const SUPERSEDED_MISMATCH: &str = "track/epoch mismatch";
/// progress_log detail for backlog-clear same-track auto-clear (last_event unchanged).
pub const AUTO_CLEAR_DETAIL: &str = "failure: auto-cleared on backlog clear";
/// Auto-settle when the conductor row is Completed (0045).
pub const SETTLED_DETAIL: &str = "failure: settled (conductor row Completed)";
/// Fresh `run` cleared a leftover that was not conductor-Completed.
pub const START_CLEAR_DETAIL: &str = "failure: cleared on run start";

/// Clear a stored failure and journal `detail`. Returns whether a record existed.
///
/// When `touch_last_event` is false, `last_event` is left as-is (backlog-clear).
/// Does not change `track_id`, `status`, or `run_epoch`.
pub fn settle_failure(
    record: &ProjectRecord,
    state: &mut crate::state::RunState,
    detail: &str,
    touch_last_event: bool,
) -> bool {
    let had = existing_path(record).is_some() || state.failure_class.is_some();
    if !had {
        return false;
    }
    clear(record);
    state.failure_class = None;
    if touch_last_event {
        state.last_event = detail.to_string();
    }
    crate::progress_log::append(record, "resolve", detail);
    true
}

/// Parsed FAILURE.md bullets (`- track_id:` / `- run_epoch:`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArtifactMeta {
    pub track_id: Option<String>,
    pub run_epoch: Option<u64>,
}

/// Scan markdown bullets. `"null"`, absent, or a non-integer epoch → `None`.
pub fn parse_metadata(body: &str) -> ArtifactMeta {
    let mut meta = ArtifactMeta::default();
    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("- track_id:") {
            let v = rest.trim();
            if !v.is_empty() && v != "null" {
                meta.track_id = Some(v.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("- run_epoch:") {
            let v = rest.trim();
            if let Ok(n) = v.parse::<u64>() {
                meta.run_epoch = Some(n);
            }
        }
    }
    meta
}

/// Equal, or same 4-digit leading id (`0040` ↔ `0040-Slug`). Empty / `null` never match.
pub fn track_ids_match(a: &str, b: &str) -> bool {
    let a = a.trim();
    let b = b.trim();
    if a.is_empty() || b.is_empty() || a == "null" || b == "null" {
        return false;
    }
    if a == b {
        return true;
    }
    match (numeric_track_id(a), numeric_track_id(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

pub(crate) fn numeric_track_id(s: &str) -> Option<&str> {
    if s.len() >= 4 && s.as_bytes()[..4].iter().all(u8::is_ascii_digit) {
        let rest = &s[4..];
        if rest.is_empty() || rest.starts_with('-') {
            return Some(&s[..4]);
        }
    }
    None
}

/// Read-time SUPERSEDED reason. `completed` is the conductor-row signal (false if unknown).
pub fn compute_superseded(
    body: Option<&str>,
    state_track: Option<&str>,
    state_epoch: u64,
    completed: bool,
) -> Option<String> {
    if completed {
        return Some(SUPERSEDED_COMPLETED.into());
    }
    let body = body?;
    let meta = parse_metadata(body);
    if let (Some(a), Some(b)) = (meta.track_id.as_deref(), state_track)
        && !track_ids_match(a, b)
    {
        return Some(SUPERSEDED_MISMATCH.into());
    }
    if let Some(ep) = meta.run_epoch
        && ep != state_epoch
    {
        return Some(SUPERSEDED_MISMATCH.into());
    }
    None
}

/// Read artifact body when present.
pub fn read(record: &ProjectRecord) -> Result<Option<FailureShow>> {
    let p = path(record)?;
    if !p.is_file() {
        return Ok(None);
    }
    let body = std::fs::read_to_string(&p)?;
    Ok(Some(FailureShow {
        path: p,
        body,
        superseded: None,
    }))
}

/// CLI / HTTP payload for `failure show`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct FailureShow {
    pub path: PathBuf,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded: Option<String>,
}

/// Atomic UTF-8 markdown write of `FAILURE.md`.
pub fn write(record: &ProjectRecord, event: &NotifyEvent) -> Result<PathBuf> {
    let mut event = event.clone();
    event.artifact_path = path(record)?;
    write_event(&event)
}

/// Write to `event.artifact_path` (used by [`super::adapter::ArtifactAdapter`]).
pub fn write_event(event: &NotifyEvent) -> Result<PathBuf> {
    let dest = event.artifact_path.clone();
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = render(event);
    atomic_write(&dest, body.as_bytes())?;
    Ok(dest)
}

fn render(event: &NotifyEvent) -> String {
    let track = event.track_id.as_deref().unwrap_or("null");
    let written = event.written_at.to_rfc3339();
    let last_event = event.last_event.as_str();
    let message = truncate_message(event.message.as_deref().unwrap_or("(none)"));
    let action = recommended_action(event.failure_class);
    format!(
        "# Coordinator failure\n\
         \n\
         Recommended action is **advisory** — Coordinator does **not** auto-retry in this track.\n\
         \n\
         - project_id: {project}\n\
         - track_id: {track}\n\
         - phase: {phase}\n\
         - failure_class: {class}\n\
         - run_epoch: {epoch}\n\
         - written_at: {written}\n\
         \n\
         ## last_event\n\
         \n\
         ```\n\
         {last_event}\n\
         ```\n\
         \n\
         ## recommended_action\n\
         \n\
         {action}\n\
         \n\
         ## Message\n\
         \n\
         ```\n\
         {message}\n\
         ```\n",
        project = event.project_id,
        phase = event.phase,
        class = event.failure_class,
        epoch = event.run_epoch,
    )
}

fn truncate_message(msg: &str) -> String {
    if msg.chars().count() <= MESSAGE_CAP {
        return msg.to_string();
    }
    let t: String = msg.chars().take(MESSAGE_CAP).collect();
    format!("{t}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcome::FailureClass;
    use chrono::Utc;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn rec(path: &std::path::Path) -> ProjectRecord {
        ProjectRecord {
            id: Uuid::new_v4().to_string(),
            path: path.to_path_buf(),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: std::collections::BTreeMap::new(),
            state_dir: None,
            auto_merge: true,
            phase_timeouts_secs: std::collections::BTreeMap::new(),
            notify_progress: false,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn path_is_state_dir_failure_md() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        assert_eq!(
            path(&r).unwrap(),
            dir.path().join(".coordinator").join("FAILURE.md")
        );
    }

    #[test]
    fn write_read_clear_round_trip() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        let event = NotifyEvent {
            project_id: r.id.clone(),
            track_id: Some("0009".into()),
            phase: "plan".into(),
            failure_class: FailureClass::Timeout,
            message: Some(r"budget at C:\dev\work".into()),
            last_event: r"outcome: failure class=timeout phase=plan source=cli — C:\dev\work"
                .into(),
            artifact_path: path(&r).unwrap(),
            written_at: Utc::now(),
            run_epoch: 3,
        };
        write(&r, &event).unwrap();
        let shown = read(&r).unwrap().expect("written");
        assert!(shown.body.contains("failure_class: timeout"));
        assert!(shown.body.contains("recommended_action"));
        assert!(shown.body.contains("Increase the phase budget"));
        assert!(shown.body.contains("does **not** auto-retry"));
        assert!(shown.body.contains("```\noutcome: failure"));
        assert!(shown.body.contains(r"C:\dev\work"));
        assert!(existing_path(&r).is_some());
        clear(&r);
        assert!(read(&r).unwrap().is_none());
        assert!(existing_path(&r).is_none());
    }

    #[test]
    fn parse_metadata_bullets_and_nulls() {
        let meta =
            parse_metadata("# Coordinator failure\n\n- track_id: 0040-Slug\n- run_epoch: 2\n");
        assert_eq!(meta.track_id.as_deref(), Some("0040-Slug"));
        assert_eq!(meta.run_epoch, Some(2));
        let missing = parse_metadata("# Coordinator failure\n");
        assert_eq!(missing, ArtifactMeta::default());
        let nulls = parse_metadata("- track_id: null\n- run_epoch: nope\n");
        assert_eq!(nulls, ArtifactMeta::default());
    }

    #[test]
    fn track_ids_match_numeric_prefix() {
        assert!(track_ids_match("0040", "0040-FailureArtifactLifecycle"));
        assert!(track_ids_match("0040-FailureArtifactLifecycle", "0040"));
        assert!(track_ids_match("0040", "0040"));
        assert!(!track_ids_match("0040", "0041"));
        assert!(!track_ids_match("null", "0040"));
        assert!(!track_ids_match("", "0040"));
    }

    #[test]
    fn compute_superseded_reasons() {
        let body = "- track_id: 0038\n- run_epoch: 2\n";
        assert_eq!(
            compute_superseded(Some(body), Some("0038"), 2, true).as_deref(),
            Some(SUPERSEDED_COMPLETED)
        );
        assert_eq!(compute_superseded(Some(body), Some("0038"), 2, false), None);
        assert_eq!(
            compute_superseded(Some(body), Some("0038"), 9, false).as_deref(),
            Some(SUPERSEDED_MISMATCH)
        );
        assert_eq!(
            compute_superseded(Some(body), Some("0040"), 2, false).as_deref(),
            Some(SUPERSEDED_MISMATCH)
        );
        assert_eq!(compute_superseded(None, Some("0038"), 2, false), None);
    }

    #[test]
    fn settle_failure_absent_is_false() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        crate::state::ensure_state_dir(&r).unwrap();
        let mut state = crate::state::load_run_state(&r).unwrap();
        assert!(!settle_failure(&r, &mut state, SETTLED_DETAIL, true));
        assert!(state.failure_class.is_none());
    }

    #[test]
    fn settle_failure_clears_and_second_call_is_noop() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        crate::state::ensure_state_dir(&r).unwrap();
        let mut state = crate::state::load_run_state(&r).unwrap();
        state.failure_class = Some(FailureClass::Timeout);
        state.track_id = Some("0045".into());
        state.run_epoch = 3;
        let event = NotifyEvent {
            project_id: r.id.clone(),
            track_id: Some("0045".into()),
            phase: "ci-wait".into(),
            failure_class: FailureClass::Timeout,
            message: Some("stale".into()),
            last_event: "timeout".into(),
            artifact_path: path(&r).unwrap(),
            written_at: Utc::now(),
            run_epoch: 3,
        };
        write(&r, &event).unwrap();
        assert!(settle_failure(&r, &mut state, SETTLED_DETAIL, true));
        assert!(existing_path(&r).is_none());
        assert!(state.failure_class.is_none());
        assert_eq!(state.last_event, SETTLED_DETAIL);
        assert_eq!(state.track_id.as_deref(), Some("0045"));
        assert_eq!(state.run_epoch, 3);
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap();
        assert_eq!(log.matches(SETTLED_DETAIL).count(), 1);
        assert!(!settle_failure(&r, &mut state, SETTLED_DETAIL, true));
        let log = std::fs::read_to_string(crate::progress_log::path(&r)).unwrap();
        assert_eq!(log.matches(SETTLED_DETAIL).count(), 1);
    }
}
