//! Epoch-start snapshot writer. Two callers, one lock proof.

#![cfg_attr(not(windows), allow(dead_code, unused_imports))]

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::registry::ProjectRecord;
use crate::state::RunState;
use crate::workflow::WorkflowDriver;

use super::{
    ARCHIVE_KEEP, ProducerHook, RequiredPair, SCHEMA, WRITER_ID, bounded_copy, capture_pairs,
    digest_of, object_from, path_shape,
};

#[cfg(windows)]
use super::{
    MAX_PATH_UTF16, NoEvidenceReason, PathShape, SKILL_MAX_BYTES, SNAPSHOT_MAX_BYTES, digest_bytes,
    metadata_is_reparse, path_allowed, utf16_len,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteOutcome {
    Written,
    Skipped,
    NotLocked,
    Failed,
}

pub(crate) fn write_for_epoch(
    record: &ProjectRecord,
    state: &RunState,
    hook: ProducerHook,
) -> WriteOutcome {
    if !crate::state::run_state_lock_held_for(&record.id) {
        journal(record, "not-locked", state.run_epoch);
        return WriteOutcome::NotLocked;
    }
    if state.driver != WorkflowDriver::Adapter {
        return WriteOutcome::Skipped;
    }
    #[cfg(not(windows))]
    {
        let _ = hook;
        return WriteOutcome::Skipped;
    }
    #[cfg(windows)]
    write_windows(record, state, hook)
}

/// While another entry is Running or Paused, the leaf is `{epoch}.json`.
/// On the 1→2 write, park `current.json` on the first entry's epoch first.
fn evidence_leaf(record: &ProjectRecord, root: &Path, epoch: u64) -> PathBuf {
    let current = root.join("current.json");
    let own = root.join(format!("{epoch}.json"));
    match crate::state::load_run_book(record) {
        Ok(crate::state::RunBook::Many(envelope)) => {
            let sibling_live = envelope.runs.iter().any(|entry| {
                entry.run_epoch != epoch
                    && matches!(
                        entry.status,
                        crate::state::RunStatus::Running | crate::state::RunStatus::Paused
                    )
            });
            if sibling_live { own } else { current }
        }
        _ => current,
    }
}

/// Collapse puts the remaining epoch's leaf back at `current.json`.
pub(crate) fn restore_current_leaf(record: &ProjectRecord, epoch: u64) {
    let Ok(dir) = crate::state::resolve_state_dir(record) else {
        return;
    };
    let root = dir.join("trusted-evidence");
    let specific = root.join(format!("{epoch}.json"));
    let current = root.join("current.json");
    if specific.is_file() && !current.exists() {
        let _ = std::fs::rename(specific, current);
    }
}

#[cfg(windows)]
fn write_windows(record: &ProjectRecord, state: &RunState, hook: ProducerHook) -> WriteOutcome {
    let epoch = state.run_epoch;
    let state_dir = match crate::state::resolve_state_dir(record) {
        Ok(dir) => dir,
        Err(_) => {
            journal(record, "io", epoch);
            return WriteOutcome::Failed;
        }
    };
    if !matches!(path_shape(&state_dir), PathShape::Ok) {
        journal(record, "io", epoch);
        return WriteOutcome::Failed;
    }
    let root = state_dir.join("trusted-evidence");
    let leaf = evidence_leaf(record, &root, epoch);
    if let Err(reason) = guard_dir(&root) {
        journal(record, reason, epoch);
        return WriteOutcome::Failed;
    }
    if let Err(err) = std::fs::create_dir_all(&root) {
        let _ = err;
        journal(record, "io", epoch);
        return WriteOutcome::Failed;
    }
    if let Err(reason) = guard_dir(&root) {
        journal(record, reason, epoch);
        return WriteOutcome::Failed;
    }
    sweep_temps(&root);
    if temp_leaf_too_long(&leaf) {
        journal(record, "io", epoch);
        return WriteOutcome::Failed;
    }
    let sequence = previous_sequence(&leaf).saturating_add(1).max(1);
    let value = match build_snapshot(record, state, hook, sequence) {
        Ok(value) => value,
        Err(()) => {
            journal(record, "io", epoch);
            return WriteOutcome::Failed;
        }
    };
    let bytes = match serde_json::to_vec_pretty(&value) {
        Ok(bytes) if bytes.len() as u64 <= SNAPSHOT_MAX_BYTES => bytes,
        _ => {
            journal(record, "io", epoch);
            return WriteOutcome::Failed;
        }
    };
    if crate::persist::atomic_write(&leaf, &bytes).is_err() {
        journal(record, "io", epoch);
        return WriteOutcome::Failed;
    }
    let archive = root.join("archive");
    if let Err(reason) = guard_dir(&archive) {
        journal(record, reason, epoch);
        return WriteOutcome::Failed;
    }
    if std::fs::create_dir_all(&archive).is_err() {
        journal(record, "archive", epoch);
        return WriteOutcome::Failed;
    }
    if let Err(reason) = guard_dir(&archive) {
        journal(record, reason, epoch);
        return WriteOutcome::Failed;
    }
    let archive_leaf = archive.join(format!("{epoch:020}.json"));
    if temp_leaf_too_long(&archive_leaf)
        || crate::persist::atomic_write(&archive_leaf, &bytes).is_err()
    {
        journal(record, "archive", epoch);
        prune_archive(&archive);
        return WriteOutcome::Failed;
    }
    prune_archive(&archive);
    WriteOutcome::Written
}

fn journal(record: &ProjectRecord, reason: &str, epoch: u64) {
    crate::progress_log::append(
        record,
        "evidence",
        &format!("write-failed reason={reason} epoch={epoch}"),
    );
}

#[cfg(windows)]
fn guard_dir(path: &Path) -> Result<(), &'static str> {
    let chain = match super::ancestor_chain(path) {
        Some(chain) => chain,
        None => return Err("reparse"),
    };
    for ancestor in &chain {
        match std::fs::symlink_metadata(ancestor) {
            Ok(meta) if metadata_is_reparse(&meta) => return Err("reparse"),
            Ok(meta) if !meta.is_dir() => return Err("io"),
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("io"),
        }
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) if metadata_is_reparse(&meta) => Err("reparse"),
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err("io"),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("io"),
    }
}

#[cfg(windows)]
fn sweep_temps(root: &Path) {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    let mut removed = 0u32;
    for entry in entries {
        if removed >= 16 {
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
        let name_owned = entry.file_name();
        let Some(name) = name_owned.to_str() else {
            continue;
        };
        if !(name.starts_with(".current.json.") && name.ends_with(".tmp")) {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata_is_reparse(&meta) || !meta.is_file() {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let stale = match modified.elapsed() {
            Ok(age) => age > std::time::Duration::from_secs(24 * 60 * 60),
            Err(_) => false,
        };
        if stale && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
}

#[cfg(windows)]
fn temp_leaf_too_long(leaf: &Path) -> bool {
    let Some(parent) = leaf.parent() else {
        return true;
    };
    let Some(name) = leaf.file_name() else {
        return true;
    };
    let extra = 1 + 1 + 10 + 1 + 32 + 4;
    utf16_len(parent)
        .saturating_add(1)
        .saturating_add(utf16_len(Path::new(name)))
        + extra
        > MAX_PATH_UTF16
}

#[cfg(windows)]
#[allow(clippy::manual_unwrap_or, clippy::manual_unwrap_or_default)]
fn previous_sequence(leaf: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(leaf) else {
        return 0;
    };
    if metadata_is_reparse(&meta) || !meta.is_file() || meta.len() > SNAPSHOT_MAX_BYTES {
        return 0;
    }
    let Ok(bytes) = std::fs::read(leaf) else {
        return 0;
    };
    if bytes.len() as u64 > SNAPSHOT_MAX_BYTES {
        return 0;
    }
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return 0;
    };
    match value.get("sequence").and_then(Value::as_u64) {
        Some(sequence) => sequence,
        None => 0,
    }
}

#[cfg(windows)]
fn build_snapshot(
    record: &ProjectRecord,
    state: &RunState,
    hook: ProducerHook,
    sequence: u64,
) -> Result<Value, ()> {
    let mut skills = Vec::new();
    let mut observations = Vec::new();
    for pair in capture_pairs() {
        let captured = capture_pair(record, state.run_epoch, pair);
        observations.push(captured.observation());
        skills.push(captured.skill());
    }
    skills.sort_by_key(skill_key);
    observations.sort_by_key(observation_key);
    let mut value = object_from(vec![
        (
            "captured_at",
            Value::String(crate::workflow::evidence_stamp::utc_stamp_line(
                chrono::Utc::now(),
            )),
        ),
        ("digest", Value::Null),
        ("observations", Value::Array(observations)),
        ("producer", producer_value(state, hook)),
        ("project_id", Value::String(record.id.clone())),
        ("run_epoch", Value::from(state.run_epoch)),
        ("schema", Value::String(SCHEMA.to_string())),
        ("schema_version", Value::from(1u64)),
        ("sequence", Value::from(sequence)),
        ("skills", Value::Array(skills)),
        (
            "track_id",
            match state.track_id.as_deref() {
                Some(id) if !id.is_empty() => Value::String(id.to_string()),
                _ => Value::Null,
            },
        ),
    ]);
    let digest = digest_of(&value).ok_or(())?;
    if let Some(obj) = value.as_object_mut() {
        obj.insert("digest".to_string(), Value::String(digest));
    }
    Ok(value)
}

fn json_text(value: &Value, key: &str) -> String {
    match value.get(key).and_then(Value::as_str) {
        Some(text) => text.to_string(),
        None => String::new(),
    }
}

fn skill_key(value: &Value) -> (String, String, String) {
    (
        json_text(value, "phase"),
        json_text(value, "name"),
        json_text(value, "scope"),
    )
}

fn observation_key(value: &Value) -> (String, String) {
    (json_text(value, "phase"), json_text(value, "name"))
}

#[cfg(windows)]
fn producer_value(state: &RunState, hook: ProducerHook) -> Value {
    let bindings = crate::harness::roles::load_role_bindings().ok();
    let role_key = bindings
        .as_ref()
        .and_then(|map| crate::harness::roles::resolve_phase_role_key(state.phase.as_str(), map));
    let binding = match (&bindings, &role_key) {
        (Some(map), Some(key)) => map.get(key),
        _ => None,
    };
    let phase = if state.phase.len() <= 32 && !state.phase.chars().any(char::is_control) {
        state.phase.clone()
    } else {
        "plan".to_string()
    };
    let pid = std::process::id();
    object_from(vec![
        (
            "command",
            bounded_copy(binding.map(|item| item.command.as_str()), 256),
        ),
        (
            "harness",
            bounded_copy(binding.map(|item| item.harness.as_str()), 256),
        ),
        ("hook", Value::String(hook.as_str().to_string())),
        (
            "model",
            bounded_copy(binding.and_then(|item| item.model.as_deref()), 256),
        ),
        ("phase", Value::String(phase)),
        (
            "pid",
            if pid >= 1 {
                Value::from(u64::from(pid))
            } else {
                Value::Null
            },
        ),
        (
            "product_version",
            Value::String(env!("CARGO_PKG_VERSION").to_string()),
        ),
        ("role_key", bounded_copy(role_key.as_deref(), 64)),
        ("writer", Value::String(WRITER_ID.to_string())),
    ])
}

struct Captured {
    phase: &'static str,
    name: &'static str,
    scope: &'static str,
    path: Option<String>,
    state: &'static str,
    regular: bool,
    reparse: bool,
    size: Option<u64>,
    digest: Option<String>,
    observed: &'static str,
    outcome: &'static str,
    reason: Option<&'static str>,
}

impl Captured {
    fn skill(&self) -> Value {
        object_from(vec![
            (
                "digest",
                match &self.digest {
                    Some(digest) => Value::String(digest.clone()),
                    None => Value::Null,
                },
            ),
            ("name", Value::String(self.name.to_string())),
            (
                "path",
                match &self.path {
                    Some(path) => Value::String(path.clone()),
                    None => Value::Null,
                },
            ),
            ("phase", Value::String(self.phase.to_string())),
            ("regular", Value::Bool(self.regular)),
            ("reparse", Value::Bool(self.reparse)),
            ("scope", Value::String(self.scope.to_string())),
            (
                "size",
                match self.size {
                    Some(size) => Value::from(size),
                    None => Value::Null,
                },
            ),
            ("state", Value::String(self.state.to_string())),
        ])
    }

    fn observation(&self) -> Value {
        object_from(vec![
            ("fact", Value::String("skill_material".to_string())),
            ("name", Value::String(self.name.to_string())),
            ("observed", Value::String(self.observed.to_string())),
            ("outcome", Value::String(self.outcome.to_string())),
            ("phase", Value::String(self.phase.to_string())),
            (
                "reason",
                match self.reason {
                    Some(reason) => Value::String(reason.to_string()),
                    None => Value::Null,
                },
            ),
        ])
    }
}

#[cfg(windows)]
fn capture_pair(record: &ProjectRecord, epoch: u64, pair: &RequiredPair) -> Captured {
    let (path, roots) = resolve_pair(record, epoch, pair);
    let Some(path_text) = path.to_str().map(str::to_string) else {
        return captured(
            pair,
            None,
            "non_utf8_path",
            false,
            false,
            None,
            None,
            "metadata",
            "non_utf8_path",
            Some("non-utf8-path"),
        );
    };
    match path_allowed(&path, &roots) {
        Err(NoEvidenceReason::ReparseAncestor) => {
            return captured(
                pair,
                Some(path_text),
                "reparse",
                false,
                true,
                None,
                None,
                "metadata",
                "reparse",
                Some("reparse"),
            );
        }
        Err(_) => {
            return captured(
                pair,
                Some(path_text),
                "unreadable",
                false,
                false,
                None,
                None,
                "metadata",
                "unreadable",
                Some("io"),
            );
        }
        Ok(()) => {}
    }
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return captured(
                pair,
                Some(path_text),
                "absent",
                false,
                false,
                None,
                None,
                "metadata",
                "absent",
                Some("not-found"),
            );
        }
        Err(_) => {
            return captured(
                pair,
                Some(path_text),
                "unreadable",
                false,
                false,
                None,
                None,
                "metadata",
                "unreadable",
                Some("io"),
            );
        }
    };
    if metadata_is_reparse(&meta) {
        return captured(
            pair,
            Some(path_text),
            "reparse",
            false,
            true,
            None,
            None,
            "metadata",
            "reparse",
            Some("reparse"),
        );
    }
    if !meta.is_file() {
        return captured(
            pair,
            Some(path_text),
            "not_regular",
            false,
            false,
            None,
            None,
            "metadata",
            "not_regular",
            Some("not-regular"),
        );
    }
    if meta.len() > SKILL_MAX_BYTES {
        return captured(
            pair,
            Some(path_text),
            "oversize",
            true,
            false,
            None,
            None,
            "metadata",
            "oversize",
            Some("oversize"),
        );
    }
    let pre = witness(&meta);
    #[cfg(test)]
    super::note_skill_open();
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return captured(
                pair,
                Some(path_text),
                "absent",
                false,
                false,
                None,
                None,
                "metadata",
                "absent",
                Some("not-found"),
            );
        }
        Err(_) => {
            return captured(
                pair,
                Some(path_text),
                "unreadable",
                true,
                false,
                None,
                None,
                "metadata",
                "unreadable",
                Some("open-error"),
            );
        }
    };
    let handle = match file.metadata() {
        Ok(meta) => meta,
        Err(_) => {
            return captured(
                pair,
                Some(path_text),
                "unreadable",
                true,
                false,
                None,
                None,
                "metadata",
                "unreadable",
                Some("io"),
            );
        }
    };
    use std::io::Read;
    let mut limited = file.take(SKILL_MAX_BYTES.saturating_add(1));
    let mut buf = Vec::new();
    if Read::read_to_end(&mut limited, &mut buf).is_err() {
        return captured(
            pair,
            Some(path_text),
            "unreadable",
            true,
            false,
            None,
            None,
            "metadata",
            "unreadable",
            Some("io"),
        );
    }
    if buf.len() as u64 > SKILL_MAX_BYTES {
        return captured(
            pair,
            Some(path_text),
            "oversize",
            true,
            false,
            None,
            None,
            "metadata",
            "oversize",
            Some("oversize"),
        );
    }
    let re = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(_) => {
            return captured(
                pair,
                Some(path_text),
                "unreadable",
                true,
                false,
                None,
                None,
                "metadata",
                "unreadable",
                Some("io"),
            );
        }
    };
    if metadata_is_reparse(&re)
        || witness(&handle) != pre
        || witness(&re) != witness(&handle)
        || !handle.is_file()
        || handle.len() != buf.len() as u64
    {
        return captured(
            pair,
            Some(path_text),
            "unreadable",
            true,
            false,
            None,
            None,
            "metadata",
            "unreadable",
            Some("io"),
        );
    }
    captured(
        pair,
        Some(path_text),
        "present",
        true,
        false,
        Some(buf.len() as u64),
        Some(digest_bytes(&buf)),
        "open_read",
        "readable",
        None,
    )
}

#[cfg(windows)]
fn witness(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::windows::fs::MetadataExt;
    (meta.len(), meta.last_write_time())
}

#[allow(clippy::too_many_arguments)]
fn captured(
    pair: &RequiredPair,
    path: Option<String>,
    state: &'static str,
    regular: bool,
    reparse: bool,
    size: Option<u64>,
    digest: Option<String>,
    observed: &'static str,
    outcome: &'static str,
    reason: Option<&'static str>,
) -> Captured {
    Captured {
        phase: pair.phase,
        name: pair.name,
        scope: pair.scope,
        path,
        state,
        regular,
        reparse,
        size,
        digest,
        observed,
        outcome,
        reason,
    }
}

#[cfg(windows)]
fn resolve_pair(
    record: &ProjectRecord,
    epoch: u64,
    pair: &RequiredPair,
) -> (PathBuf, Vec<PathBuf>) {
    let paths = crate::layout::resolve(record);
    if pair.scope == "workspace" {
        let (path, _) = crate::workflow::reuse::chosen_workspace_skill(record, pair.name);
        let mut roots = vec![paths.workspace_root];
        if let Some(parent) = paths.conductor_dir.parent() {
            roots.push(parent.to_path_buf());
        }
        (path, roots)
    } else {
        let path = crate::workflow::reuse::execution_skill_path_for_epoch(record, epoch, pair.name);
        let mut roots = Vec::new();
        if let Some(repo) = paths.execution_repo.clone() {
            roots.push(repo);
        }
        roots.push(paths.workspace_root);
        if crate::worktree::isolation_enabled(record).ok() == Some(true)
            && let Ok(dir) = crate::worktree::epoch_dir(record, epoch)
        {
            roots.push(dir);
        }
        (path, roots)
    }
}

#[cfg(windows)]
fn prune_archive(archive: &Path) {
    if guard_dir(archive).is_err() {
        return;
    }
    let entries = match std::fs::read_dir(archive) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    let mut numeric: Vec<(u128, PathBuf)> = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let name_owned = entry.file_name();
        let Some(name) = name_owned.to_str() else {
            continue;
        };
        if !is_epoch_leaf(name) {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata_is_reparse(&meta) || !meta.is_file() {
            continue;
        }
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        let Ok(epoch) = stem.parse::<u128>() else {
            continue;
        };
        numeric.push((epoch, path));
    }
    numeric.sort_by_key(|item| std::cmp::Reverse(item.0));
    for (_, path) in numeric.into_iter().skip(ARCHIVE_KEEP) {
        let _ = std::fs::remove_file(path);
    }
}

fn is_epoch_leaf(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".json") else {
        return false;
    };
    stem.len() == 20 && stem.bytes().all(|byte| byte.is_ascii_digit())
}
