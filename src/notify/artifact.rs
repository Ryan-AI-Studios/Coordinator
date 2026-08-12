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

/// Read artifact body when present.
pub fn read(record: &ProjectRecord) -> Result<Option<FailureShow>> {
    let p = path(record)?;
    if !p.is_file() {
        return Ok(None);
    }
    let body = std::fs::read_to_string(&p)?;
    Ok(Some(FailureShow { path: p, body }))
}

/// CLI / HTTP payload for `failure show`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct FailureShow {
    pub path: PathBuf,
    pub body: String,
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
}
