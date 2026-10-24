//! Workspace `status.md` — append-only phase start/end log.
//!
//! Path is `{workspace}/status.md` (planning tree; never the execution repo).
//! Writes are best-effort and must not fail the run.
//!
//! 0109 bounds growth: before an append that would grow the active file past
//! [`ROTATE_BYTES`], the file is renamed into
//! `{state_dir}/status-archive/status-<UTC>-<NNN>.md` and only the newest
//! [`KEEP_ARCHIVES`] archives are kept. Rotation is best-effort — a failed rename
//! appends in place, so no line is ever lost. The active path is unchanged.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{SecondsFormat, Utc};

use crate::registry::ProjectRecord;

const HEADER: &str = "# Coordinator progress\n\n";

/// Rotate the active file once it is at least this large (checked before the append).
pub const ROTATE_BYTES: u64 = 1_048_576;
/// Newest archives kept; the rest are deleted oldest-first by parsed (timestamp, seq).
pub const KEEP_ARCHIVES: usize = 7;
pub const ARCHIVE_DIR: &str = "status-archive";
pub const ARCHIVE_PREFIX: &str = "status-";

#[cfg(test)]
thread_local! {
    static TEST_ROTATE_BYTES: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
    static TEST_FORCE_FAILURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static TEST_NOW: std::cell::RefCell<Option<chrono::DateTime<Utc>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_test_rotate_bytes(bytes: Option<u64>) {
    TEST_ROTATE_BYTES.with(|c| c.set(bytes));
}

#[cfg(test)]
pub(crate) fn set_test_force_rotation_failure(on: bool) {
    TEST_FORCE_FAILURE.with(|c| c.set(on));
}

#[cfg(test)]
pub(crate) fn set_test_now(now: Option<chrono::DateTime<Utc>>) {
    TEST_NOW.with(|c| *c.borrow_mut() = now);
}

fn rotate_bytes() -> u64 {
    #[cfg(test)]
    if let Some(n) = TEST_ROTATE_BYTES.with(std::cell::Cell::get) {
        return n;
    }
    ROTATE_BYTES
}

fn now_utc() -> chrono::DateTime<Utc> {
    #[cfg(test)]
    if let Some(now) = TEST_NOW.with(|c| *c.borrow()) {
        return now;
    }
    Utc::now()
}

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
    // O(1) size check before the append handle is opened: never rename an open file on
    // Windows. A rotation failure is tolerated and the append proceeds in place.
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) >= rotate_bytes() {
        let _ = rotate(record, &path);
    }
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    if file.metadata()?.len() == 0 {
        file.write_all(HEADER.as_bytes())?;
    }
    let ts = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    writeln!(file, "- {ts}  {kind}  {detail}")
}

/// `{state_dir}/status-archive`. The state dir is used instead of the workspace root so
/// rotation cannot add untracked dirt to a repo-root workspace.
pub fn archive_dir(record: &ProjectRecord) -> std::io::Result<PathBuf> {
    crate::state::resolve_state_dir(record)
        .map(|dir| dir.join(ARCHIVE_DIR))
        .map_err(std::io::Error::other)
}

fn rotate(record: &ProjectRecord, active: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if TEST_FORCE_FAILURE.with(std::cell::Cell::get) {
        return Err(std::io::Error::other("injected rotation failure"));
    }
    if !active.is_file() {
        return Ok(());
    }
    let dir = archive_dir(record)?;
    std::fs::create_dir_all(&dir)?;
    let stamp = now_utc().format("%Y%m%dT%H%M%SZ").to_string();
    let seq = next_sequence(&dir, &stamp);
    let dest = dir.join(format!("{ARCHIVE_PREFIX}{stamp}-{seq:03}.md"));
    std::fs::rename(active, &dest)?;
    let _ = prune(&dir);
    Ok(())
}

fn next_sequence(dir: &Path, stamp: &str) -> u32 {
    let mut max: Option<u32> = None;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if let Some((parsed_stamp, seq)) = parse_archive_name(&name)
                && parsed_stamp == stamp
            {
                max = Some(max.map_or(seq, |m: u32| m.max(seq)));
            }
        }
    }
    max.map(|m| (m + 1).min(999)).unwrap_or(0)
}

/// `status-YYYYMMDDTHHMMSSZ-NNN.md` (three-digit sequence) or the sequence-less form.
pub fn parse_archive_name(name: &str) -> Option<(String, u32)> {
    let rest = name.strip_prefix(ARCHIVE_PREFIX)?.strip_suffix(".md")?;
    let bytes = rest.as_bytes();
    if bytes.len() != 16 && bytes.len() != 20 {
        return None;
    }
    if !bytes[..8].iter().all(u8::is_ascii_digit)
        || bytes[8] != b'T'
        || !bytes[9..15].iter().all(u8::is_ascii_digit)
        || bytes[15] != b'Z'
    {
        return None;
    }
    let mut seq = 0u32;
    if bytes.len() == 20 {
        if bytes[16] != b'-' || !bytes[17..20].iter().all(u8::is_ascii_digit) {
            return None;
        }
        seq = rest[17..20].parse().ok()?;
    }
    Some((rest[..16].to_string(), seq))
}

/// Keep the newest [`KEEP_ARCHIVES`], deleting oldest-first by parsed (timestamp, seq).
/// Returns the deleted paths, oldest first.
fn prune(dir: &Path) -> Vec<PathBuf> {
    let mut archives: Vec<(String, u32, PathBuf)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if let Some((stamp, seq)) = parse_archive_name(name) {
                archives.push((stamp, seq, path));
            }
        }
    }
    archives.sort();
    let mut deleted = Vec::new();
    while archives.len() > KEEP_ARCHIVES {
        let (_, _, oldest) = archives.remove(0);
        if std::fs::remove_file(&oldest).is_ok() {
            deleted.push(oldest);
        }
    }
    deleted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::LayoutProfile;
    use chrono::TimeZone;
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
            notify_progress: false,
            worktree_isolation: false,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            ci_fix_routing: false,
            review_comment_routing: false,
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: Utc::now(),
        }
    }

    fn archives(record: &ProjectRecord) -> Vec<PathBuf> {
        let dir = archive_dir(record).unwrap();
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|rd| rd.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        paths.sort();
        paths
    }

    fn log_lines(path: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("- "))
            .map(str::to_string)
            .collect()
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

    /// AC-4.1/AC-4.5: crossing the threshold archives the exact pre-rotation bytes and
    /// recreates the active file with the header + the new line under the state dir.
    #[test]
    fn append_rotates_at_threshold() {
        let _guard = crate::config::test_env_lock();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        append(&r, "start", "first");
        let before = std::fs::read(path(&r)).unwrap();
        set_test_rotate_bytes(Some(before.len() as u64));
        append(&r, "start", "second");
        set_test_rotate_bytes(None);

        let archived = archives(&r);
        assert_eq!(archived.len(), 1, "{archived:?}");
        assert_eq!(std::fs::read(&archived[0]).unwrap(), before);
        let active = std::fs::read_to_string(path(&r)).unwrap();
        assert!(active.starts_with(HEADER), "{active}");
        assert!(active.contains("second"));
        assert!(!active.contains("first"));
        assert!(archived[0].starts_with(archive_dir(&r).unwrap()));
    }

    /// AC-4.2: nine rotations keep exactly seven; the two oldest are named and gone.
    #[test]
    fn append_retains_seven_archives() {
        let _guard = crate::config::test_env_lock();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        // Seed one pre-rotation body so every append below performs a rotation.
        std::fs::write(path(&r), "# Coordinator progress\n\n").unwrap();
        set_test_rotate_bytes(Some(1));
        for i in 0..9 {
            set_test_now(Some(Utc.with_ymd_and_hms(2026, 10, 21, 12, 0, i).unwrap()));
            append(&r, "start", &format!("line{i}"));
        }
        set_test_rotate_bytes(None);
        set_test_now(None);

        let archived = archives(&r);
        assert_eq!(archived.len(), KEEP_ARCHIVES, "{archived:?}");
        let names: Vec<String> = archived
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        // The two oldest (timestamp) archives are named and gone.
        assert!(
            !names
                .iter()
                .any(|n| n.starts_with("status-20261021T120000Z")),
            "{names:?}"
        );
        assert!(
            !names
                .iter()
                .any(|n| n.starts_with("status-20261021T120001Z")),
            "{names:?}"
        );
        assert!(
            names
                .iter()
                .any(|n| n.starts_with("status-20261021T120002Z")),
            "{names:?}"
        );
    }

    /// AC-4.3: archives (oldest→newest) + active contain every appended log line.
    #[test]
    fn append_no_line_lost_across_two_rotations() {
        let _guard = crate::config::test_env_lock();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        std::fs::write(path(&r), "# Coordinator progress\n\n").unwrap();
        set_test_rotate_bytes(Some(1));
        let mut expected = Vec::new();
        for i in 0..5 {
            set_test_now(Some(Utc.with_ymd_and_hms(2026, 10, 21, 13, 0, i).unwrap()));
            let detail = format!("line{i}");
            append(&r, "start", &detail);
            expected.push(detail);
        }
        set_test_rotate_bytes(None);
        set_test_now(None);

        let mut all: Vec<String> = Vec::new();
        for archive in archives(&r) {
            all.extend(log_lines(&archive));
        }
        all.extend(log_lines(&path(&r)));
        // Chronological: archives oldest→newest + the active file, in append order.
        let actual: Vec<String> = all
            .iter()
            .map(|l| l.rsplit("  ").next().unwrap_or_default().to_string())
            .collect();
        assert_eq!(actual, expected);
    }

    /// AC-4.4: a forced rotation failure still appends in place; the next append rotates.
    #[test]
    fn append_rotation_failure_still_appends() {
        let _guard = crate::config::test_env_lock();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        append(&r, "start", "first");
        let before = std::fs::read(path(&r)).unwrap();
        set_test_rotate_bytes(Some(before.len() as u64));
        set_test_force_rotation_failure(true);
        append(&r, "start", "second");
        let after = std::fs::read_to_string(path(&r)).unwrap();
        assert!(after.contains("second"), "{after}");
        assert!(after.contains("first"), "{after}");
        assert!(archives(&r).is_empty());
        set_test_force_rotation_failure(false);
        append(&r, "start", "third");
        set_test_rotate_bytes(None);
        assert_eq!(archives(&r).len(), 1);
        let active = std::fs::read_to_string(path(&r)).unwrap();
        assert!(active.contains("third"));
        assert!(!active.contains("second"));
    }

    /// AC-4.2: same-second rotations are ordered/pruned by (timestamp, sequence).
    #[test]
    fn append_same_second_rotations_order_and_prune() {
        let _guard = crate::config::test_env_lock();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        std::fs::write(path(&r), "# Coordinator progress\n\n").unwrap();
        set_test_rotate_bytes(Some(1));
        set_test_now(Some(Utc.with_ymd_and_hms(2026, 10, 21, 14, 0, 0).unwrap()));
        for i in 0..9 {
            append(&r, "start", &format!("line{i}"));
        }
        set_test_rotate_bytes(None);
        set_test_now(None);
        let archived = archives(&r);
        assert_eq!(archived.len(), KEEP_ARCHIVES);
        let names: Vec<String> = archived
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            names,
            vec![
                "status-20261021T140000Z-002.md",
                "status-20261021T140000Z-003.md",
                "status-20261021T140000Z-004.md",
                "status-20261021T140000Z-005.md",
                "status-20261021T140000Z-006.md",
                "status-20261021T140000Z-007.md",
                "status-20261021T140000Z-008.md",
            ]
        );
    }

    /// AC-4.5: archives live under the state dir; `progress_log::path` is unchanged.
    #[test]
    fn archive_path_is_under_state_dir() {
        let _guard = crate::config::test_env_lock();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        let archive = archive_dir(&r).unwrap();
        assert!(archive.starts_with(crate::state::resolve_state_dir(&r).unwrap()));
        assert_eq!(path(&r), dir.path().join("status.md"));
        assert_ne!(archive, dir.path());
    }

    /// Test 40: existing readers still resolve the active file after rotation.
    #[test]
    fn existing_status_readers_still_resolve_the_active_file() {
        let _guard = crate::config::test_env_lock();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        std::fs::write(path(&r), "# Coordinator progress\n\n").unwrap();
        set_test_rotate_bytes(Some(1));
        for i in 0..3 {
            set_test_now(Some(Utc.with_ymd_and_hms(2026, 10, 21, 15, 0, i).unwrap()));
            append(&r, "start", &format!("line{i}"));
        }
        set_test_rotate_bytes(None);
        set_test_now(None);
        assert_eq!(path(&r), dir.path().join("status.md"));
        let text = std::fs::read_to_string(path(&r)).unwrap();
        assert!(text.starts_with(HEADER), "{text}");
        assert!(text.contains("line2"), "{text}");
        assert!(archives(&r).len() >= 2);
    }

    /// AC-4.6: after nine rotations a repo-root workspace has no committable dirt from
    /// the archives (the `status-<UTC>-<NNN>.md` basenames are excluded).
    #[test]
    fn rotation_leaves_committable_dirt_clean() {
        let _guard = crate::config::test_env_lock();
        let dir = tempdir().unwrap();
        let ws = dir.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(ws)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"]);
        std::fs::write(ws.join(".gitignore"), "status.md\n").unwrap();
        git(&["add", ".gitignore"]);
        git(&[
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "user.name=test",
            "commit",
            "-q",
            "-m",
            "init",
        ]);
        let r = rec(ws);
        set_test_rotate_bytes(Some(1));
        for i in 0..9 {
            set_test_now(Some(Utc.with_ymd_and_hms(2026, 10, 21, 16, 0, i).unwrap()));
            append(&r, "start", &format!("line{i}"));
        }
        set_test_rotate_bytes(None);
        set_test_now(None);
        let dirt = crate::checkpoint::committable_dirt(ws).unwrap();
        assert!(
            dirt.is_empty(),
            "archives became committable dirt: {dirt:?}"
        );
        assert_eq!(archives(&r).len(), KEEP_ARCHIVES);
        assert!(archive_dir(&r).unwrap().starts_with(ws));
    }
}
