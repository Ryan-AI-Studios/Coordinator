//! Workspace `status.md` — append-only phase start/end log.
//!
//! Path is `{workspace}/status.md` (planning tree; never the execution repo).
//! Writes are best-effort and must not fail the run.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use chrono::{SecondsFormat, Utc};

use crate::registry::ProjectRecord;

const HEADER: &str = "# Coordinator progress\n\n";

pub fn path(record: &ProjectRecord) -> PathBuf {
    crate::layout::resolve(record)
        .workspace_root
        .join("status.md")
}

/// Append one timestamped line. Errors are ignored.
pub fn append(record: &ProjectRecord, kind: &str, detail: &str) {
    let _ = append_inner(record, kind, detail);
}

fn append_inner(record: &ProjectRecord, kind: &str, detail: &str) -> std::io::Result<()> {
    let path = path(record);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    if file.metadata()?.len() == 0 {
        file.write_all(HEADER.as_bytes())?;
    }
    let ts = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    writeln!(file, "- {ts}  {kind}  {detail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::LayoutProfile;
    use chrono::Utc as ChronoUtc;
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    fn rec(ws: &std::path::Path) -> ProjectRecord {
        ProjectRecord {
            id: "hh".into(),
            path: ws.to_path_buf(),
            display_name: None,
            layout_profile: LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: Some(ws.join("hands")),
            execution_repos: BTreeMap::new(),
            state_dir: None,
            auto_merge: false,
            phase_timeouts_secs: BTreeMap::new(),
            created_at: ChronoUtc::now(),
        }
    }

    #[test]
    fn writes_header_and_line_under_workspace_not_exec() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        append(&r, "start", "track=0099 phase=plan");
        append(&r, "end", "track=0099 plan → plan-review");
        let text = std::fs::read_to_string(path(&r)).unwrap();
        assert!(text.starts_with(HEADER));
        assert!(text.contains("start  track=0099 phase=plan"));
        assert!(text.contains("end  track=0099 plan → plan-review"));
        assert!(text.contains('T') && text.contains('Z'));
        assert!(!dir.path().join("hands").join("status.md").exists());
    }
}
