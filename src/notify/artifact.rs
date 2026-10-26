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

/// `{state_dir}/FAILURE.md` — the project summary, not one run's record.
pub fn path(record: &ProjectRecord) -> Result<PathBuf> {
    Ok(resolve_state_dir(record)?.join("FAILURE.md"))
}

/// `{state_dir}/failures/{epoch}/FAILURE.md`
pub fn run_path(record: &ProjectRecord, epoch: u64) -> Result<PathBuf> {
    Ok(resolve_state_dir(record)?
        .join("failures")
        .join(epoch.to_string())
        .join("FAILURE.md"))
}

/// The summary path when that file exists. This is not a per-run file.
pub fn summary_path_if_present(record: &ProjectRecord) -> Option<PathBuf> {
    let p = path(record).ok()?;
    p.is_file().then_some(p)
}

/// Scoped epoch's per-run file, or the singular entry's file. Never the summary.
///
/// A book with no scope returns `None`.
pub fn existing_path(record: &ProjectRecord) -> Option<PathBuf> {
    let epoch = match crate::state::load_run_book(record).ok()? {
        crate::state::RunBook::Many(_) => crate::state::active_run_epoch()?,
        crate::state::RunBook::One(state) => {
            if let Some(scope) = crate::state::active_run_epoch()
                && scope != state.run_epoch
            {
                return None;
            }
            state.run_epoch
        }
    };
    let p = run_path(record, epoch).ok()?;
    p.is_file().then_some(p)
}

/// True when any per-run file exists, or the summary path is still a legacy body.
pub fn any_failure_record(record: &ProjectRecord) -> bool {
    let Ok(state_dir) = resolve_state_dir(record) else {
        return false;
    };
    if legacy_epoch(&state_dir).is_some() {
        return true;
    }
    !scan_per_run(&state_dir).is_empty()
}

/// One failure epoch: a per-run file, a book entry whose class is set, or both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureRecord {
    pub epoch: u64,
    pub track_id: Option<String>,
    pub has_file: bool,
    pub has_entry: bool,
}

/// Union of per-run files and entries with `failure_class`, one row per epoch.
pub fn failure_records(record: &ProjectRecord) -> Result<Vec<FailureRecord>> {
    let mut map = std::collections::BTreeMap::<u64, FailureRecord>::new();
    if let Ok(state_dir) = resolve_state_dir(record) {
        for item in scan_per_run(&state_dir) {
            map.insert(
                item.epoch,
                FailureRecord {
                    epoch: item.epoch,
                    track_id: item.track,
                    has_file: true,
                    has_entry: false,
                },
            );
        }
    }
    if let Some(meta) = legacy_meta(record)
        && let Some(epoch) = meta.run_epoch
    {
        map.entry(epoch).or_insert(FailureRecord {
            epoch,
            track_id: meta.track_id,
            has_file: true,
            has_entry: false,
        });
    }
    let book = crate::state::load_run_book(record)?;
    for entry in book.entries() {
        if let Some(slot) = map.get_mut(&entry.run_epoch) {
            slot.has_entry = true;
            if entry.track_id.is_some() {
                slot.track_id = entry.track_id.clone();
            }
        } else if entry.failure_class.is_some() {
            map.insert(
                entry.run_epoch,
                FailureRecord {
                    epoch: entry.run_epoch,
                    track_id: entry.track_id.clone(),
                    has_file: false,
                    has_entry: true,
                },
            );
        }
    }
    Ok(map.into_values().collect())
}

/// Delete `failures/{epoch}/FAILURE.md` only, then rebuild the summary.
///
/// Another epoch's file stays. When any other file remains, the progress line
/// names those epochs.
pub fn clear(record: &ProjectRecord, epoch: u64) {
    let Ok(state_dir) = resolve_state_dir(record) else {
        return;
    };
    let Ok(entries) = with_summary_lock(&state_dir, || {
        let file = state_dir
            .join("failures")
            .join(epoch.to_string())
            .join("FAILURE.md");
        if file.is_file() {
            let _ = std::fs::remove_file(&file);
        }
        if legacy_epoch_at(&state_dir) == Some(epoch) {
            let _ = std::fs::remove_file(state_dir.join("FAILURE.md"));
        }
        rebuild_summary(&state_dir, false)
    }) else {
        return;
    };
    if entries.is_empty() {
        return;
    }
    let names = entries
        .iter()
        .map(|item| item.epoch.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    crate::progress_log::append(
        record,
        "failure",
        &format!("failure: left sibling artifact epoch {names}"),
    );
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
    let had = per_run_exists(record, state.run_epoch)
        || state.failure_class.is_some()
        || legacy_meta(record).and_then(|meta| meta.run_epoch) == Some(state.run_epoch);
    if !had {
        return false;
    }
    clear(record, state.run_epoch);
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
    /// Failure class as written on the artifact (`blocked` holds the run).
    pub failure_class: Option<String>,
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
        } else if let Some(rest) = line.strip_prefix("- failure_class:") {
            let v = rest.trim();
            if !v.is_empty() && v != "null" {
                meta.failure_class = Some(v.to_string());
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
    let s = s.trim();
    // Tolerate an alpha prefix: a registry may write `T374` where the engine canonicalises on
    // `374` (see `conductor_md::parse_leading_id`). The digit run is what identifies the track,
    // so compare on that. The bare form keeps its four-digit floor -- it is the canonical shape.
    if s.as_bytes().first().is_some_and(u8::is_ascii_alphabetic) {
        let n = s[1..].bytes().take_while(u8::is_ascii_digit).count();
        if n == 0 {
            return None;
        }
        let digits = &s[1..1 + n];
        let rest = &s[1 + n..];
        if rest.is_empty() || rest.starts_with('-') {
            return Some(digits);
        }
        return None;
    }
    // The bare form must accept a digit run of ANY length, not only four. The engine launches
    // AI-Brains as `374` while the registry prefixes the same id `T374`; comparing them requires
    // both sides to yield the same digit run. A four-digit floor here left `("T374","374")`
    // unmatchable -- the second half of the same defect (0075).
    let n = s.bytes().take_while(u8::is_ascii_digit).count();
    if n == 0 {
        return None;
    }
    let rest = &s[n..];
    if rest.is_empty() || rest.starts_with('-') {
        return Some(&s[..n]);
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

/// Read one run's file. The summary is not a run record.
pub fn read(record: &ProjectRecord, epoch: u64) -> Result<Option<FailureShow>> {
    let p = run_path(record, epoch)?;
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

fn legacy_meta(record: &ProjectRecord) -> Option<ArtifactMeta> {
    let path = path(record).ok()?;
    let body = std::fs::read_to_string(path).ok()?;
    let meta = parse_metadata(&body);
    meta.run_epoch?;
    Some(meta)
}

fn legacy_epoch_at(state_dir: &std::path::Path) -> Option<u64> {
    let body = std::fs::read_to_string(state_dir.join("FAILURE.md")).ok()?;
    parse_metadata(&body).run_epoch
}

/// Bare show of the only unmigrated legacy body still at the summary path.
pub fn read_legacy(record: &ProjectRecord) -> Result<Option<FailureShow>> {
    let p = path(record)?;
    if !p.is_file() {
        return Ok(None);
    }
    let body = std::fs::read_to_string(&p)?;
    if parse_metadata(&body).run_epoch.is_none() {
        return Ok(None);
    }
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

/// Atomic UTF-8 markdown write of one run's file, then the project summary.
pub fn write(record: &ProjectRecord, event: &NotifyEvent) -> Result<PathBuf> {
    let mut event = event.clone();
    event.artifact_path = run_path(record, event.run_epoch)?;
    write_event(&event)
}

/// Write the render body to the per-run file, then rebuild the summary.
///
/// A summary-shaped `artifact_path` does not receive the render body. The
/// per-run file is written before the summary lock. Used by
/// [`super::adapter::ArtifactAdapter`].
pub fn write_event(event: &NotifyEvent) -> Result<PathBuf> {
    let dest = per_run_destination(event)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write(&dest, render(event).as_bytes())?;
    let state_dir = state_dir_of_per_run(&dest)?;
    refresh_summary(&state_dir)?;
    Ok(dest)
}

fn per_run_exists(record: &ProjectRecord, epoch: u64) -> bool {
    run_path(record, epoch).is_ok_and(|path| path.is_file())
}

fn per_run_destination(event: &NotifyEvent) -> Result<PathBuf> {
    let state_dir = infer_state_dir(&event.artifact_path).ok_or_else(|| {
        crate::error::CoordinatorError::Message(
            "failure artifact path is not under a state dir".into(),
        )
    })?;
    Ok(state_dir
        .join("failures")
        .join(event.run_epoch.to_string())
        .join("FAILURE.md"))
}

fn infer_state_dir(path: &std::path::Path) -> Option<PathBuf> {
    if path.file_name()? != "FAILURE.md" {
        return path.parent().map(|parent| parent.to_path_buf());
    }
    let parent = path.parent()?;
    let grand = parent.parent();
    let epoch_dir = parent
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|text| text.parse::<u64>().ok())
        .is_some();
    let failures_dir = grand
        .and_then(|dir| dir.file_name())
        .is_some_and(|name| name == "failures");
    if epoch_dir && failures_dir {
        return grand
            .and_then(|dir| dir.parent())
            .map(|dir| dir.to_path_buf());
    }
    Some(parent.to_path_buf())
}

fn state_dir_of_per_run(dest: &std::path::Path) -> Result<PathBuf> {
    infer_state_dir(dest).ok_or_else(|| {
        crate::error::CoordinatorError::Message("failure path has no state dir".into())
    })
}

struct Scanned {
    epoch: u64,
    track: Option<String>,
    class: Option<String>,
    path: PathBuf,
}

fn legacy_epoch(state_dir: &std::path::Path) -> Option<u64> {
    let summary = state_dir.join("FAILURE.md");
    let body = std::fs::read_to_string(summary).ok()?;
    parse_metadata(&body).run_epoch
}

fn scan_per_run(state_dir: &std::path::Path) -> Vec<Scanned> {
    let Ok(entries) = std::fs::read_dir(state_dir.join("failures")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Some(text) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(epoch) = text.parse::<u64>() else {
            continue;
        };
        let path = entry.path().join("FAILURE.md");
        if !path.is_file() {
            continue;
        }
        let body = std::fs::read_to_string(&path).unwrap_or_default();
        let meta = parse_metadata(&body);
        out.push(Scanned {
            epoch,
            track: meta.track_id,
            class: meta.failure_class,
            path,
        });
    }
    out.sort_by_key(|item| item.epoch);
    out
}

fn adopt_legacy(state_dir: &std::path::Path) -> Result<()> {
    let summary = state_dir.join("FAILURE.md");
    if !summary.is_file() {
        return Ok(());
    }
    let bytes = std::fs::read(&summary)?;
    let body = String::from_utf8_lossy(&bytes);
    let Some(epoch) = parse_metadata(&body).run_epoch else {
        return Ok(());
    };
    let dest = state_dir
        .join("failures")
        .join(epoch.to_string())
        .join("FAILURE.md");
    if !dest.exists() {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&summary, &dest)?;
        return Ok(());
    }
    if std::fs::read(&dest)? == bytes {
        std::fs::remove_file(&summary)?;
        return Ok(());
    }
    Err(crate::error::CoordinatorError::Message(
        "legacy FAILURE.md differs from the per-run file; summary not written".into(),
    ))
}

fn write_summary(state_dir: &std::path::Path, entries: &[Scanned]) -> Result<()> {
    let summary = state_dir.join("FAILURE.md");
    if entries.is_empty() {
        if summary.exists() {
            let _ = std::fs::remove_file(&summary);
        }
        return Ok(());
    }
    let mut body = String::from("# Coordinator failures\n\n");
    for item in entries {
        let track = item.track.as_deref().unwrap_or("null");
        let class = item.class.as_deref().unwrap_or("null");
        body.push_str(&format!(
            "epoch {}: track {track} class {class} path {}\n",
            item.epoch,
            item.path.display()
        ));
    }
    atomic_write(&summary, body.as_bytes())
}

fn rebuild_summary(state_dir: &std::path::Path, adopt: bool) -> Result<Vec<Scanned>> {
    if adopt {
        adopt_legacy(state_dir)?;
    } else if legacy_epoch_at(state_dir).is_some() {
        return Ok(scan_per_run(state_dir));
    }
    let _first = scan_per_run(state_dir);
    pause_summary_rebuild();
    let entries = scan_per_run(state_dir);
    write_summary(state_dir, &entries)?;
    Ok(entries)
}

fn refresh_summary(state_dir: &std::path::Path) -> Result<Vec<Scanned>> {
    with_summary_lock(state_dir, || rebuild_summary(state_dir, true))
}

fn with_summary_lock<T>(state_dir: &std::path::Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    std::fs::create_dir_all(state_dir)?;
    let lock_path = state_dir.join(".failure-summary.lock");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match std::fs::create_dir(&lock_path) {
            Ok(()) => {
                let result = f();
                let _ = std::fs::remove_dir(&lock_path);
                return result;
            }
            Err(err)
                if err.kind() == std::io::ErrorKind::AlreadyExists
                    || err.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                if let Ok(meta) = std::fs::metadata(&lock_path)
                    && let Ok(modified) = meta.modified()
                    && let Ok(age) = std::time::SystemTime::now().duration_since(modified)
                    && age > std::time::Duration::from_secs(60)
                {
                    let _ = std::fs::remove_dir(&lock_path);
                    continue;
                }
                if std::time::Instant::now() >= deadline {
                    return Err(crate::error::CoordinatorError::Message(
                        "timed out waiting for failure-summary lock".into(),
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(err) => return Err(err.into()),
        }
    }
}

#[cfg(test)]
static SUMMARY_PAUSE: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn arm_summary_pause(hook: impl FnOnce() + Send + 'static) {
    *SUMMARY_PAUSE.lock().unwrap_or_else(|err| err.into_inner()) = Some(Box::new(hook));
}

#[cfg(test)]
fn pause_summary_rebuild() {
    let hook = SUMMARY_PAUSE
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .take();
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(not(test))]
fn pause_summary_rebuild() {}

fn render(event: &NotifyEvent) -> String {
    let track = event.track_id.as_deref().unwrap_or("null");
    let written = event.written_at.to_rfc3339();
    let last_event = event.last_event.as_str();
    let message = truncate_message(event.message.as_deref().unwrap_or("(none)"));
    let action = recommended_action(event.failure_class);
    // 0098: class-aware preamble. Empty for every pre-existing class, so their
    // artifact bytes are unchanged.
    let blocked_preamble = if event.failure_class == crate::outcome::FailureClass::Blocked {
        "The leg declared a terminal prerequisite block. No blocked repair was attempted; the project is held until `failure resolve` and a new `run --track`.\n\
         \n"
    } else {
        ""
    };
    format!(
        "# Coordinator failure\n\
         \n\
         Recommended action is **advisory**. This file means automatic recovery has stopped. An ACP stdout close or a transport drop restarts the session only before this file is written (bounded). Nothing else auto-retries.\n\
         \n\
         {blocked_preamble}\
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
        crate::state::ensure_state_dir(&r).unwrap();
        let mut state = crate::state::load_run_state(&r).unwrap();
        state.run_epoch = 3;
        crate::state::save_run_state(&r, &state).unwrap();
        write(&r, &event).unwrap();
        let shown = read(&r, 3).unwrap().expect("written");
        assert!(shown.body.contains("failure_class: timeout"));
        assert!(shown.body.contains("recommended_action"));
        assert!(shown.body.contains("Increase the phase budget"));
        assert!(shown.body.contains("automatic recovery has stopped"));
        assert!(shown.body.contains("```\noutcome: failure"));
        assert!(shown.body.contains(r"C:\dev\work"));
        assert!(existing_path(&r).is_some());
        clear(&r, 3);
        assert!(read(&r, 3).unwrap().is_none());
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
    fn track_ids_match_tolerates_an_alpha_prefix() {
        // A registry may write `| T374 | ... |` while the engine launches with `374`. Both must
        // identify the same track, in either argument order, or the fold -> implement gate can
        // never read such a project's row (0075).
        assert!(track_ids_match("T374", "374"));
        assert!(track_ids_match("374", "T374"));
        assert!(track_ids_match("T374", "T374"));
        assert!(!track_ids_match("T374", "T375"));
        // A trailing slug is still tolerated, exactly as the bare form tolerates `0040-Slug`.
        assert!(track_ids_match("T374-agy-stop-hook-quoting", "374"));
        // A prefix with no digits is not an id.
        assert!(!track_ids_match("Tabc", "374"));
        assert!(!track_ids_match("T", "374"));
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
    fn blocked_artifact_has_a_class_aware_preamble_and_recovery() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        crate::state::ensure_state_dir(&r).unwrap();
        let event = NotifyEvent {
            project_id: r.id.clone(),
            track_id: Some("0098".into()),
            phase: "plan".into(),
            failure_class: FailureClass::Blocked,
            message: Some("prerequisite missing".into()),
            last_event: "outcome: failure class=blocked".into(),
            artifact_path: path(&r).unwrap(),
            written_at: Utc::now(),
            run_epoch: 2,
        };
        let p = write(&r, &event).unwrap();
        let body = std::fs::read_to_string(p).unwrap();
        assert!(body.contains("terminal prerequisite block"));
        assert!(body.contains("No blocked repair was attempted"));
        assert!(body.contains(
            "Resolve the prerequisite the leg named, then `coordinator failure resolve`"
        ));
        assert!(body.contains("- failure_class: blocked"));
        assert_eq!(
            parse_metadata(&body).failure_class.as_deref(),
            Some("blocked")
        );
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

    fn event_for(r: &ProjectRecord, track: &str, epoch: u64, class: FailureClass) -> NotifyEvent {
        NotifyEvent {
            project_id: r.id.clone(),
            track_id: Some(track.into()),
            phase: "plan".into(),
            failure_class: class,
            message: Some("boom".into()),
            last_event: "boom".into(),
            artifact_path: path(r).unwrap(),
            written_at: Utc::now(),
            run_epoch: epoch,
        }
    }

    #[test]
    fn hard_failure_keeps_both_epochs_and_points_at_the_run_file() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        crate::state::ensure_state_dir(&r).unwrap();
        let first =
            crate::notify::on_hard_failure(&r, &event_for(&r, "0128", 4, FailureClass::Timeout));
        let epoch4 = std::fs::read(run_path(&r, 4).unwrap()).unwrap();
        assert_eq!(first.artifact_path, run_path(&r, 4).unwrap());
        let second = crate::notify::on_hard_failure(
            &r,
            &event_for(&r, "0129", 5, FailureClass::HarnessCrash),
        );
        assert_eq!(second.artifact_path, run_path(&r, 5).unwrap());
        assert_eq!(std::fs::read(run_path(&r, 4).unwrap()).unwrap(), epoch4);
        assert!(run_path(&r, 5).unwrap().is_file());
        let summary = std::fs::read_to_string(path(&r).unwrap()).unwrap();
        assert!(summary.starts_with("# Coordinator failures\n"), "{summary}");
        assert!(summary.contains("epoch 4:"), "{summary}");
        assert!(summary.contains("epoch 5:"), "{summary}");
        assert!(summary.contains(&run_path(&r, 4).unwrap().display().to_string()));
        assert!(summary.contains(&run_path(&r, 5).unwrap().display().to_string()));
        assert_eq!(parse_metadata(&summary), ArtifactMeta::default());
        assert!(!summary.contains("- run_epoch:"), "{summary}");
    }

    #[test]
    fn summary_rebuild_rescans_a_sibling_written_during_the_hold() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        crate::state::ensure_state_dir(&r).unwrap();
        let sibling = run_path(&r, 5).unwrap();
        arm_summary_pause(move || {
            std::fs::create_dir_all(sibling.parent().unwrap()).unwrap();
            std::fs::write(
                &sibling,
                "- track_id: 0129\n- run_epoch: 5\n- failure_class: timeout\n",
            )
            .unwrap();
        });
        crate::notify::on_hard_failure(&r, &event_for(&r, "0128", 4, FailureClass::Timeout));
        let summary = std::fs::read_to_string(path(&r).unwrap()).unwrap();
        assert!(summary.contains("epoch 4:"), "{summary}");
        assert!(summary.contains("epoch 5:"), "{summary}");
    }

    #[test]
    fn clear_of_one_epoch_leaves_the_other_files_bytes() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        crate::state::ensure_state_dir(&r).unwrap();
        crate::notify::on_hard_failure(&r, &event_for(&r, "0128", 4, FailureClass::Timeout));
        crate::notify::on_hard_failure(&r, &event_for(&r, "0129", 5, FailureClass::HarnessCrash));
        let kept = std::fs::read(run_path(&r, 5).unwrap()).unwrap();
        clear(&r, 4);
        assert!(!run_path(&r, 4).unwrap().is_file());
        assert_eq!(std::fs::read(run_path(&r, 5).unwrap()).unwrap(), kept);
        let summary = std::fs::read_to_string(path(&r).unwrap()).unwrap();
        assert!(!summary.contains("epoch 4:"), "{summary}");
        assert!(summary.contains("epoch 5:"), "{summary}");
    }

    #[test]
    fn clearing_the_last_file_deletes_the_summary() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        crate::state::ensure_state_dir(&r).unwrap();
        crate::notify::on_hard_failure(&r, &event_for(&r, "0128", 4, FailureClass::Timeout));
        assert!(path(&r).unwrap().is_file());
        clear(&r, 4);
        assert!(!path(&r).unwrap().exists());
        assert!(!any_failure_record(&r));
    }

    #[test]
    fn unscoped_book_hides_existing_path_but_any_file_still_counts() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        crate::state::ensure_state_dir(&r).unwrap();
        let mut older = crate::state::RunState::idle(&r.id);
        older.run_epoch = 4;
        older.track_id = Some("0128".into());
        older.status = crate::state::RunStatus::Idle;
        let mut newer = crate::state::RunState::idle(&r.id);
        newer.run_epoch = 5;
        newer.track_id = Some("0129".into());
        newer.status = crate::state::RunStatus::Idle;
        crate::state::write_run_book(&r, &crate::state::RunEnvelope::new(0, vec![older, newer]))
            .unwrap();
        crate::notify::on_hard_failure(&r, &event_for(&r, "0128", 4, FailureClass::Timeout));
        assert!(existing_path(&r).is_none());
        assert!(any_failure_record(&r));
    }

    #[test]
    fn legacy_body_is_renamed_byte_for_byte_before_the_summary() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        crate::state::ensure_state_dir(&r).unwrap();
        let legacy = b"# Coordinator failure\n\n- track_id: 0040\n- run_epoch: 2\n- failure_class: timeout\n";
        std::fs::write(path(&r).unwrap(), legacy).unwrap();
        crate::notify::on_hard_failure(&r, &event_for(&r, "0129", 5, FailureClass::HarnessCrash));
        assert_eq!(std::fs::read(run_path(&r, 2).unwrap()).unwrap(), legacy);
        let summary = std::fs::read_to_string(path(&r).unwrap()).unwrap();
        assert_eq!(parse_metadata(&summary), ArtifactMeta::default());
        assert!(summary.contains("epoch 2:"), "{summary}");
        assert!(summary.contains("epoch 5:"), "{summary}");
        assert_ne!(std::fs::read(path(&r).unwrap()).unwrap(), legacy);
    }

    #[test]
    fn legacy_conflict_leaves_the_old_bytes_and_keeps_the_new_file() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        crate::state::ensure_state_dir(&r).unwrap();
        let legacy = b"# Coordinator failure\n\n- track_id: 0040\n- run_epoch: 2\n- failure_class: timeout\n";
        let dest = run_path(&r, 2).unwrap();
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(
            &dest,
            b"- track_id: 0040\n- run_epoch: 2\n- failure_class: crash\n",
        )
        .unwrap();
        std::fs::write(path(&r).unwrap(), legacy).unwrap();
        let err = write(&r, &event_for(&r, "0129", 5, FailureClass::HarnessCrash)).unwrap_err();
        assert!(err.to_string().contains("summary not written"), "{err}");
        assert_eq!(std::fs::read(path(&r).unwrap()).unwrap(), legacy);
        assert!(run_path(&r, 5).unwrap().is_file());
    }

    #[test]
    fn clearing_the_legacy_epoch_removes_the_body() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        crate::state::ensure_state_dir(&r).unwrap();
        let legacy =
            b"# Coordinator failure\n\n- track_id: 0040\n- run_epoch: 2\n- failure_class: timeout\n";
        std::fs::write(path(&r).unwrap(), legacy).unwrap();
        clear(&r, 2);
        assert!(!path(&r).unwrap().exists());
        assert!(!run_path(&r, 2).unwrap().exists());
    }

    #[test]
    fn clearing_another_epoch_leaves_the_legacy_body() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        crate::state::ensure_state_dir(&r).unwrap();
        let legacy =
            b"# Coordinator failure\n\n- track_id: 0040\n- run_epoch: 2\n- failure_class: timeout\n";
        std::fs::write(path(&r).unwrap(), legacy).unwrap();
        clear(&r, 5);
        assert_eq!(std::fs::read(path(&r).unwrap()).unwrap(), legacy);
        assert!(!run_path(&r, 2).unwrap().exists());
    }

    #[test]
    fn settling_one_epoch_leaves_the_other_class_and_file() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        crate::state::ensure_state_dir(&r).unwrap();
        crate::notify::on_hard_failure(&r, &event_for(&r, "0128", 4, FailureClass::Timeout));
        crate::notify::on_hard_failure(&r, &event_for(&r, "0129", 5, FailureClass::HarnessCrash));
        let kept = std::fs::read(run_path(&r, 5).unwrap()).unwrap();
        let mut state = crate::state::RunState::idle(&r.id);
        state.run_epoch = 4;
        state.failure_class = Some(FailureClass::Timeout);
        assert!(settle_failure(&r, &mut state, SETTLED_DETAIL, true));
        assert!(state.failure_class.is_none());
        assert!(!run_path(&r, 4).unwrap().is_file());
        assert_eq!(std::fs::read(run_path(&r, 5).unwrap()).unwrap(), kept);
    }
}
