//! 0109: `{conductor_dir}/track-registry.json` is the track-row source of truth.
//!
//! `conductor.md` stays the human view and is generated from the store. Import is
//! lossless and byte-identical (`render(import(x)) == x`), writers update the store and
//! then the view under one `create_dir` registry lock, and a hand-edited view is adopted
//! (`import --force`) or discarded (`render --force`) — never silently overwritten and
//! never silently authoritative. The pick refuses while the view and the store disagree.
//!
//! Only `serde_json` + `sha2` (both already dependencies) are used. The markdown parser
//! in [`crate::workflow::conductor_md`] survives as this module's importer, as the legacy
//! path for un-migrated projects, and as the view lint.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CoordinatorError, Result};
use crate::notify::artifact::track_ids_match;
use crate::registry::ProjectRecord;
use crate::state::RegistryView;
use crate::workflow::conductor_md::{
    TrackRow, is_execution_header, is_proposed, is_status_header, is_summary_header,
    is_table_start, is_thematic_break, is_track_or_id_header, looks_like_row, parse_conductor_md,
    parse_execution_order, parse_leading_id, replace_cell, split_unescaped_pipes,
    status_names_placeholder, strip_nostart_literal, unwrap_md_link,
};

pub const STORE_FILE: &str = "track-registry.json";
pub const SCHEMA: u32 = 1;
pub(crate) const VIEW_DRIFT_PREFIX: &str = "track registry view drifted";

const VIEW_FILE: &str = "conductor.md";
const LOCK_DIR: &str = ".track-registry.lock";
const BACKUP_KEEP: usize = 5;

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredRow {
    pub id: String,
    pub slug: String,
    pub track_cell: String,
    /// Raw status cell, trimmed, with a cell-terminating `<!-- nostart -->` stripped.
    pub status: String,
    pub summary: String,
    pub execution: Option<String>,
    pub nostart: bool,
    /// The rendered row line, no trailing newline. Never parsed on read.
    pub line: String,
    pub status_source: Option<String>,
    pub status_updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Block {
    Text {
        lines: Vec<String>,
    },
    Table {
        header: String,
        delimiter: String,
        columns: usize,
        track_col: usize,
        status_col: usize,
        summary_col: Option<usize>,
        execution_col: Option<usize>,
        row_indices: Vec<usize>,
    },
}

impl Block {
    fn status_col(&self) -> Option<usize> {
        match self {
            Self::Table { status_col, .. } => Some(*status_col),
            Self::Text { .. } => None,
        }
    }

    fn track_col(&self) -> Option<usize> {
        match self {
            Self::Table { track_col, .. } => Some(*track_col),
            Self::Text { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportSource {
    pub bytes: u64,
    pub sha256: String,
    pub lines: usize,
    pub at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegistryStore {
    pub schema: u32,
    pub created_at: String,
    pub updated_at: String,
    pub view_sha256: String,
    pub view_bytes: u64,
    pub trailing_newline: bool,
    pub imported_from: ImportSource,
    pub execution_order: Vec<String>,
    pub blocks: Vec<Block>,
    pub rows: Vec<StoredRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreView {
    Ok,
    Drifted { expected: String, actual: String },
    Absent,
}

#[derive(Debug, Clone)]
pub struct StoreSnapshot {
    pub rows: Vec<TrackRow>,
    pub execution_order: Vec<String>,
    pub view: StoreView,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// No store: the caller runs the shipped markdown writer under the same lock.
    NoStore,
    /// Deduped ids actually rewritten.
    Changed(Vec<String>),
    Unchanged,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Reject {
    pub line: usize,
    pub reason: String,
    pub preview: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub got: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub want: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RejectClasses {
    pub cell_count: usize,
    pub struck_through: usize,
    pub alpha_prefixed: usize,
    pub other: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ImportBefore {
    pub bytes: u64,
    pub sha256: String,
    pub lines: usize,
    pub parser_rows: usize,
    pub tables: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ImportAfter {
    pub rows: usize,
    pub order: usize,
    pub view_bytes: u64,
    pub view_sha256: String,
    pub byte_identical: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusChange {
    pub id: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackupInfo {
    pub path: String,
    pub sha256: String,
    pub verified: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportReport {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub conductor_dir: String,
    pub before: ImportBefore,
    pub after: ImportAfter,
    pub missing_ids: Vec<String>,
    pub extra_ids: Vec<String>,
    pub status_changes: Vec<StatusChange>,
    pub order_changes: Vec<String>,
    pub rejects: Vec<Reject>,
    pub interior_gaps: Vec<Vec<usize>>,
    pub reject_classes: RejectClasses,
    pub backup: Option<BackupInfo>,
    pub store_written: bool,
    pub view_written: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RowDiff {
    pub missing: Vec<String>,
    pub extra: Vec<String>,
    pub status: Vec<StatusChange>,
    pub order: Vec<String>,
    pub line_mismatch: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WidthDeviationView {
    pub line: usize,
    pub got: usize,
    pub want: usize,
    pub preview: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ViewReport {
    pub actual_sha256: Option<String>,
    pub expected_sha256: Option<String>,
    pub rendered_sha256: Option<String>,
    pub bytes: Option<u64>,
    pub expected_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LayoutReport {
    pub width_deviations: Vec<WidthDeviationView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifyReport {
    /// `ok` | `drift` | `invalid` | `absent`.
    pub status: String,
    pub conductor_dir: String,
    pub view: ViewReport,
    pub row_diff: RowDiff,
    pub layout: LayoutReport,
    /// Row ids whose `nostart` flag disagrees between the store and the parsed view.
    pub nostart_parity: Vec<String>,
    pub rejects: Vec<Reject>,
    pub interior_gaps: Vec<Vec<usize>>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ImportOpts {
    pub dry_run: bool,
    pub force: bool,
}

// ---------------------------------------------------------------------------
// Seams
// ---------------------------------------------------------------------------

#[cfg(test)]
thread_local! {
    static LOCK_WAIT_MS: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    static LOCK_AGE_MS: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    static IMPORT_CHANGED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FORCE_VIEW_FAILURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn set_lock_seams(wait_ms: Option<u64>, age_ms: Option<u64>) {
    LOCK_WAIT_MS.with(|c| c.set(wait_ms));
    LOCK_AGE_MS.with(|c| c.set(age_ms));
}

#[cfg(test)]
pub(crate) fn set_import_changed_during_test(changed: bool) {
    IMPORT_CHANGED.with(|c| c.set(changed));
}

#[cfg(test)]
pub(crate) fn force_view_write_failure(on: bool) {
    FORCE_VIEW_FAILURE.with(|c| c.set(on));
}

#[cfg(test)]
pub(crate) struct SeamReset;

#[cfg(test)]
impl Drop for SeamReset {
    fn drop(&mut self) {
        set_lock_seams(None, None);
        set_import_changed_during_test(false);
        force_view_write_failure(false);
    }
}

#[cfg(test)]
pub(crate) fn reset_seams() -> SeamReset {
    set_lock_seams(None, None);
    set_import_changed_during_test(false);
    force_view_write_failure(false);
    SeamReset
}

fn lock_wait() -> std::time::Duration {
    #[cfg(test)]
    if let Some(ms) = LOCK_WAIT_MS.with(std::cell::Cell::get) {
        return std::time::Duration::from_millis(ms);
    }
    std::time::Duration::from_secs(10)
}

fn lock_stale_age() -> std::time::Duration {
    #[cfg(test)]
    if let Some(ms) = LOCK_AGE_MS.with(std::cell::Cell::get) {
        return std::time::Duration::from_millis(ms);
    }
    std::time::Duration::from_secs(60)
}

fn reread_view_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    let bytes = std::fs::read(path)?;
    #[cfg(test)]
    if IMPORT_CHANGED.with(std::cell::Cell::get) {
        let mut changed = bytes;
        changed.push(b' ');
        return Ok(changed);
    }
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let out = hasher.finalize();
    let mut hex = String::with_capacity(out.len() * 2);
    for b in out.iter() {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

fn now_rfc3339() -> String {
    crate::workflow::evidence_stamp::utc_stamp_line(chrono::Utc::now())
}

fn compact_stamp() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

pub fn store_path_at(conductor_dir: &Path) -> PathBuf {
    conductor_dir.join(STORE_FILE)
}

pub fn store_path_for(record: &ProjectRecord) -> PathBuf {
    store_path_at(&crate::layout::resolve(record).conductor_dir)
}

fn view_path_at(conductor_dir: &Path) -> PathBuf {
    conductor_dir.join(VIEW_FILE)
}

/// `{conductor_dir}` for a record; shared with the 0120 guard so drift text and store
/// lookups name the same directory the picker does.
pub(crate) fn conductor_dir_for(record: &ProjectRecord) -> PathBuf {
    crate::layout::resolve(record).conductor_dir
}

fn stored_to_row(row: &StoredRow) -> TrackRow {
    TrackRow {
        id: row.id.clone(),
        slug: row.slug.clone(),
        status_raw: row.status.clone(),
        summary: row.summary.clone(),
        nostart: row.nostart,
        execution: row.execution.clone(),
    }
}

fn block_for_row(store: &RegistryStore, idx: usize) -> Option<&Block> {
    store.blocks.iter().find(|block| match block {
        Block::Table { row_indices, .. } => row_indices.contains(&idx),
        Block::Text { .. } => false,
    })
}

fn row_matches_target(row: &StoredRow, key: &str) -> bool {
    let slug = if row.slug.is_empty() {
        row.id.clone()
    } else {
        format!("{}-{}", row.id, row.slug)
    };
    track_ids_match(&row.id, key) || track_ids_match(&slug, key)
}

fn classify_track_cell(raw: &str) -> String {
    if raw.starts_with("~~") {
        return "struck_through".into();
    }
    let t = raw.trim_start_matches('[');
    let letters: String = t.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    if letters.len() >= 2 && t[letters.len()..].starts_with('-') {
        return "alpha_prefixed".into();
    }
    "other".into()
}

// ---------------------------------------------------------------------------
// Lock
// ---------------------------------------------------------------------------

/// Cross-process registry lock (`create_dir`, 10 s wait, 60 s stale break).
///
/// Held across both the store arm and the legacy markdown arm of every writer, and from
/// the first read of an import through both writes. Order is `run-state -> registry`.
pub fn with_registry_lock<T>(conductor_dir: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let _ = std::fs::create_dir_all(conductor_dir);
    let lock_path = conductor_dir.join(LOCK_DIR);
    let deadline = std::time::Instant::now() + lock_wait();
    loop {
        match std::fs::create_dir(&lock_path) {
            Ok(()) => {
                let result = f();
                let _ = std::fs::remove_dir(&lock_path);
                return result;
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::AlreadyExists
                    || e.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                let stale = std::fs::metadata(&lock_path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > lock_stale_age());
                if stale {
                    let _ = std::fs::remove_dir(&lock_path);
                    continue;
                }
                if std::time::Instant::now() >= deadline {
                    return Err(CoordinatorError::Message(format!(
                        "track registry lock {} is held; retry when the other writer finishes",
                        lock_path.display()
                    )));
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

// ---------------------------------------------------------------------------
// Load / snapshot / status
// ---------------------------------------------------------------------------

pub fn load_at(conductor_dir: &Path) -> Result<Option<RegistryStore>> {
    let path = store_path_at(conductor_dir);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if bytes.is_empty() {
        return Err(CoordinatorError::Message(format!(
            "track store {} is empty",
            path.display()
        )));
    }
    let store: RegistryStore = serde_json::from_slice(&bytes).map_err(|e| {
        CoordinatorError::Message(format!(
            "track store {} is not valid JSON: {e}",
            path.display()
        ))
    })?;
    if store.schema != SCHEMA {
        return Err(CoordinatorError::Message(format!(
            "track store {} has unknown schema {} (this binary supports {SCHEMA})",
            path.display(),
            store.schema
        )));
    }
    Ok(Some(store))
}

pub fn load(record: &ProjectRecord) -> Result<Option<RegistryStore>> {
    load_at(&conductor_dir_for(record))
}

fn view_state(conductor_dir: &Path, store: &RegistryStore) -> StoreView {
    let path = view_path_at(conductor_dir);
    match std::fs::read(&path) {
        // A missing view is not a hand edit: the store is authoritative and `render`
        // recreates the projection. A drift needs two disagreeing documents.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => StoreView::Ok,
        Err(_) => StoreView::Drifted {
            expected: store.view_sha256.clone(),
            actual: "<unreadable>".into(),
        },
        Ok(bytes) => {
            let actual = sha256_hex(&bytes);
            if bytes.len() as u64 == store.view_bytes && actual == store.view_sha256 {
                StoreView::Ok
            } else {
                StoreView::Drifted {
                    expected: store.view_sha256.clone(),
                    actual,
                }
            }
        }
    }
}

pub fn snapshot_at(conductor_dir: &Path) -> Result<StoreSnapshot> {
    match load_at(conductor_dir)? {
        None => Ok(StoreSnapshot {
            rows: Vec::new(),
            execution_order: Vec::new(),
            view: StoreView::Absent,
        }),
        Some(store) => Ok(StoreSnapshot {
            rows: store.rows.iter().map(stored_to_row).collect(),
            execution_order: store.execution_order.clone(),
            view: view_state(conductor_dir, &store),
        }),
    }
}

pub fn snapshot(record: &ProjectRecord) -> Result<StoreSnapshot> {
    snapshot_at(&conductor_dir_for(record))
}

/// `status --project` registry evidence. Never fails: an unreadable store is
/// `invalid`, which is exactly what the operator needs to see.
pub fn status_view_for(record: &ProjectRecord) -> RegistryView {
    let dir = conductor_dir_for(record);
    let store_path = store_path_at(&dir);
    match load_at(&dir) {
        Err(e) => RegistryView {
            state: "invalid".into(),
            store_path: Some(store_path),
            recovery: Some(format!("{e}; repair or delete the store")),
        },
        Ok(None) => RegistryView {
            state: "absent".into(),
            store_path: Some(store_path),
            recovery: None,
        },
        Ok(Some(store)) => match view_state(&dir, &store) {
            StoreView::Ok => RegistryView {
                state: "ok".into(),
                store_path: Some(store_path),
                recovery: None,
            },
            StoreView::Drifted { expected, actual } => RegistryView {
                state: "drift".into(),
                store_path: Some(store_path),
                recovery: Some(format!(
                    "view {actual} != store {expected}; run `coordinator registry import --conductor-dir {} --force` to adopt it or `coordinator registry render --conductor-dir {} --force` to discard it",
                    dir.display(),
                    dir.display()
                )),
            },
            StoreView::Absent => RegistryView {
                state: "absent".into(),
                store_path: Some(store_path),
                recovery: None,
            },
        },
    }
}

/// One `registry-view-drift` line and one stderr line per process/project/hash pair.
static DRIFT_WARNED: std::sync::Mutex<BTreeSet<String>> = std::sync::Mutex::new(BTreeSet::new());
static UNREADABLE_WARNED: std::sync::Mutex<BTreeSet<String>> =
    std::sync::Mutex::new(BTreeSet::new());

pub(crate) fn report_drift_once(record: &ProjectRecord, expected: &str, actual: &str) {
    let key = format!("{}:{expected}:{actual}", record.id);
    let first = {
        let mut warned = DRIFT_WARNED.lock().unwrap_or_else(|e| e.into_inner());
        warned.insert(key)
    };
    if !first {
        return;
    }
    let dir = conductor_dir_for(record);
    let line = format!(
        "coordinator: {} drifted from the track store (store {expected}, view {actual}); \
         run `coordinator registry import --conductor-dir {} --force` to adopt it or \
         `coordinator registry render --conductor-dir {} --force` to discard it",
        view_path_at(&dir).display(),
        dir.display(),
        dir.display()
    );
    eprintln!("{line}");
    crate::progress_log::append(
        record,
        "registry-view-drift",
        &format!("expected={expected} actual={actual}"),
    );
}

pub(crate) fn report_store_unreadable_once(record: &ProjectRecord, err: &CoordinatorError) {
    let key = format!("{}:unreadable:{err}", record.id);
    let first = {
        let mut warned = UNREADABLE_WARNED.lock().unwrap_or_else(|e| e.into_inner());
        warned.insert(key)
    };
    if !first {
        return;
    }
    eprintln!(
        "coordinator: track store unreadable for {}: {err}",
        record.id
    );
    crate::progress_log::append(record, "registry-store-unreadable", &format!("{err}"));
}

#[cfg(test)]
pub(crate) fn reset_drift_warnings_for_tests() {
    DRIFT_WARNED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    UNREADABLE_WARNED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

pub(crate) fn is_view_drift_error(err: &CoordinatorError) -> bool {
    matches!(err, CoordinatorError::Message(m) if m.starts_with(VIEW_DRIFT_PREFIX))
}

/// The one 0109 drift refusal text; the picker and the 0120 explicit-run guard must agree
/// byte-for-byte, so both build the error here.
pub(crate) fn view_drift_error(
    conductor_dir: &Path,
    expected: &str,
    actual: &str,
) -> CoordinatorError {
    CoordinatorError::Message(format!(
        "{} (store {expected}, view {actual}); run \
         `coordinator registry import --conductor-dir {} --force` to adopt the edit \
         or `coordinator registry render --conductor-dir {} --force` to discard it",
        VIEW_DRIFT_PREFIX,
        conductor_dir.display(),
        conductor_dir.display(),
    ))
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

pub fn render(store: &RegistryStore) -> String {
    let mut out = String::new();
    for block in &store.blocks {
        match block {
            Block::Text { lines } => {
                for line in lines {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            Block::Table {
                header,
                delimiter,
                row_indices,
                ..
            } => {
                out.push_str(header);
                out.push('\n');
                out.push_str(delimiter);
                out.push('\n');
                for idx in row_indices {
                    if let Some(row) = store.rows.get(*idx) {
                        out.push_str(&row.line);
                        out.push('\n');
                    }
                }
            }
        }
    }
    if !store.trailing_newline && out.ends_with('\n') {
        out.pop();
    }
    out
}

fn store_bytes(store: &RegistryStore) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(store)?;
    bytes.push(b'\n');
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

struct Scanned {
    blocks: Vec<Block>,
    rows: Vec<StoredRow>,
    rejects: Vec<Reject>,
    interior_gaps: Vec<Vec<usize>>,
    tables: usize,
}

fn split_lines(text: &str) -> (Vec<String>, bool) {
    let trailing_newline = text.ends_with('\n');
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    if trailing_newline {
        lines.pop();
    }
    (lines, trailing_newline)
}

fn scan(lines: &[String]) -> Scanned {
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let mut out = Scanned {
        blocks: Vec::new(),
        rows: Vec::new(),
        rejects: Vec::new(),
        interior_gaps: Vec::new(),
        tables: 0,
    };
    let mut text_lines: Vec<String> = Vec::new();
    let mut i = 0;
    while i < refs.len() {
        if is_table_start(&refs, i) {
            out.tables += 1;
            let (header_line, _) = strip_nostart_literal(refs[i]);
            let headers: Vec<String> = split_unescaped_pipes(&header_line)
                .iter()
                .map(|c| c.trim().to_string())
                .collect();
            let track_col = headers.iter().position(|h| is_track_or_id_header(h));
            let status_col = headers.iter().position(|h| is_status_header(h));
            if let (Some(track_col), Some(status_col)) = (track_col, status_col) {
                if !text_lines.is_empty() {
                    out.blocks.push(Block::Text {
                        lines: std::mem::take(&mut text_lines),
                    });
                }
                let columns = headers.len();
                let summary_col = headers.iter().position(|h| is_summary_header(h));
                let execution_col = headers.iter().position(|h| is_execution_header(h));
                let mut row_indices: Vec<usize> = Vec::new();
                let mut gap: Vec<usize> = Vec::new();
                let mut j = i + 2;
                loop {
                    while j < refs.len()
                        && (refs[j].trim().is_empty() || is_thematic_break(refs[j]))
                    {
                        gap.push(j);
                        j += 1;
                    }
                    if j >= refs.len() {
                        break;
                    }
                    if !looks_like_row(refs[j]) || is_table_start(&refs, j) {
                        break;
                    }
                    if !gap.is_empty() {
                        out.interior_gaps
                            .push(gap.iter().map(|idx| idx + 1).collect());
                        gap.clear();
                    }
                    let (stripped, nostart) = strip_nostart_literal(refs[j]);
                    let cells = split_unescaped_pipes(&stripped);
                    if cells.len() != columns {
                        out.rejects.push(Reject {
                            line: j + 1,
                            reason: "cell_count".into(),
                            preview: refs[j].trim().chars().take(96).collect(),
                            class: None,
                            got: Some(cells.len()),
                            want: Some(columns),
                        });
                    } else {
                        let track_cell = unwrap_md_link(cells[track_col].trim());
                        match parse_leading_id(&track_cell) {
                            Some((id, slug)) => {
                                let status = cells[status_col].trim().to_string();
                                let summary = summary_col
                                    .map(|k| cells[k].trim().to_string())
                                    .unwrap_or_default();
                                let execution = execution_col
                                    .map(|k| cells[k].trim().to_string())
                                    .filter(|c| !c.is_empty());
                                out.rows.push(StoredRow {
                                    id,
                                    slug,
                                    track_cell,
                                    status,
                                    summary,
                                    execution,
                                    nostart,
                                    line: refs[j].to_string(),
                                    status_source: None,
                                    status_updated_at: None,
                                });
                                row_indices.push(out.rows.len() - 1);
                            }
                            None => {
                                let raw = cells[track_col].trim().to_string();
                                out.rejects.push(Reject {
                                    line: j + 1,
                                    reason: "track_cell".into(),
                                    preview: raw.chars().take(96).collect(),
                                    class: Some(classify_track_cell(&raw)),
                                    got: None,
                                    want: None,
                                });
                            }
                        }
                    }
                    j += 1;
                }
                out.blocks.push(Block::Table {
                    header: refs[i].to_string(),
                    delimiter: refs[i + 1].to_string(),
                    columns,
                    track_col,
                    status_col,
                    summary_col,
                    execution_col,
                    row_indices,
                });
                i = if let Some(first) = gap.first() {
                    *first
                } else {
                    j
                };
                gap.clear();
                continue;
            }
        }
        text_lines.push(refs[i].to_string());
        i += 1;
    }
    if !text_lines.is_empty() {
        out.blocks.push(Block::Text { lines: text_lines });
    }
    out
}

fn rows_match(parsed: &[TrackRow], rows: &[StoredRow]) -> Option<String> {
    if parsed.len() != rows.len() {
        return Some(format!(
            "parser rows {} != imported rows {}",
            parsed.len(),
            rows.len()
        ));
    }
    for (i, (p, s)) in parsed.iter().zip(rows.iter()).enumerate() {
        let same = p.id == s.id
            && p.slug == s.slug
            && p.status_raw == s.status
            && p.summary == s.summary
            && p.execution == s.execution
            && p.nostart == s.nostart;
        if !same {
            return Some(format!(
                "row {} id={}: parser (status={:?} summary={:?} execution={:?} nostart={}) \
                 store (status={:?} summary={:?} execution={:?} nostart={})",
                i + 1,
                p.id,
                p.status_raw,
                p.summary,
                p.execution,
                p.nostart,
                s.status,
                s.summary,
                s.execution,
                s.nostart
            ));
        }
    }
    None
}

fn first_differing_line(a: &str, b: &str) -> Option<usize> {
    for (i, (la, lb)) in a.lines().zip(b.lines()).enumerate() {
        if la != lb {
            return Some(i + 1);
        }
    }
    if a.lines().count() != b.lines().count() {
        Some(a.lines().count().min(b.lines().count()) + 1)
    } else {
        None
    }
}

/// Import `{conductor_dir}/conductor.md` into the store. Takes the registry lock before
/// its first read and holds it through both writes.
pub fn import_at(conductor_dir: &Path, opts: ImportOpts) -> Result<ImportReport> {
    with_registry_lock(conductor_dir, || import_locked(conductor_dir, opts))
}

fn refused_report(
    conductor_dir: &Path,
    before: ImportBefore,
    scanned_rows: usize,
    reason: String,
    rejects: Vec<Reject>,
    gaps: Vec<Vec<usize>>,
) -> ImportReport {
    let mut classes = RejectClasses::default();
    for r in &rejects {
        match r.reason.as_str() {
            "cell_count" => classes.cell_count += 1,
            "track_cell" => match r.class.as_deref() {
                Some("struck_through") => classes.struck_through += 1,
                Some("alpha_prefixed") => classes.alpha_prefixed += 1,
                _ => classes.other += 1,
            },
            _ => {}
        }
    }
    ImportReport {
        status: "refused".into(),
        reason: Some(reason),
        conductor_dir: conductor_dir.display().to_string(),
        before,
        after: ImportAfter {
            rows: scanned_rows,
            ..Default::default()
        },
        missing_ids: Vec::new(),
        extra_ids: Vec::new(),
        status_changes: Vec::new(),
        order_changes: Vec::new(),
        rejects,
        interior_gaps: gaps,
        reject_classes: classes,
        backup: None,
        store_written: false,
        view_written: false,
    }
}

fn import_locked(conductor_dir: &Path, opts: ImportOpts) -> Result<ImportReport> {
    let view_path = view_path_at(conductor_dir);
    let bytes = std::fs::read(&view_path).map_err(|e| {
        CoordinatorError::Message(format!(
            "cannot read {}: {e}; nothing imported",
            view_path.display()
        ))
    })?;
    let original_sha = sha256_hex(&bytes);
    let mut before = ImportBefore {
        bytes: bytes.len() as u64,
        sha256: original_sha.clone(),
        lines: 0,
        parser_rows: 0,
        tables: 0,
    };
    let text = match String::from_utf8(bytes.clone()) {
        Ok(text) => text,
        Err(_) => {
            return Ok(refused_report(
                conductor_dir,
                before,
                0,
                format!(
                    "{} is not valid UTF-8; refusing to import",
                    view_path.display()
                ),
                Vec::new(),
                Vec::new(),
            ));
        }
    };
    before.lines = text.lines().count();
    before.parser_rows = parse_conductor_md(&text).map(|r| r.len()).unwrap_or(0);
    if text.is_empty() {
        return Ok(refused_report(
            conductor_dir,
            before,
            0,
            "conductor.md is empty".into(),
            Vec::new(),
            Vec::new(),
        ));
    }
    if text.starts_with('\u{feff}') {
        return Ok(refused_report(
            conductor_dir,
            before,
            0,
            "conductor.md starts with a UTF-8 BOM; refusing to import".into(),
            Vec::new(),
            Vec::new(),
        ));
    }
    if text.contains('\r') {
        return Ok(refused_report(
            conductor_dir,
            before,
            0,
            "conductor.md contains CRLF line endings; refusing to import".into(),
            Vec::new(),
            Vec::new(),
        ));
    }

    let (lines, trailing_newline) = split_lines(&text);
    let mut scanned = scan(&lines);
    let before = ImportBefore {
        tables: scanned.tables,
        ..before
    };
    if !scanned.rejects.is_empty() {
        return Ok(refused_report(
            conductor_dir,
            before,
            scanned.rows.len(),
            "unattributable row(s); nothing written".into(),
            scanned.rejects,
            scanned.interior_gaps,
        ));
    }
    if !scanned.interior_gaps.is_empty() {
        return Ok(refused_report(
            conductor_dir,
            before,
            scanned.rows.len(),
            "interior table gap(s); nothing written".into(),
            Vec::new(),
            scanned.interior_gaps,
        ));
    }
    let parsed = match parse_conductor_md(&text) {
        Ok(rows) => rows,
        Err(e) => {
            return Ok(refused_report(
                conductor_dir,
                before,
                scanned.rows.len(),
                format!("parser/import divergence: {e}"),
                Vec::new(),
                Vec::new(),
            ));
        }
    };
    if let Some(diff) = rows_match(&parsed, &scanned.rows) {
        return Ok(refused_report(
            conductor_dir,
            before,
            scanned.rows.len(),
            format!("parser/import divergence: {diff}"),
            Vec::new(),
            Vec::new(),
        ));
    }

    let now = now_rfc3339();
    for row in &mut scanned.rows {
        row.status_source = Some("import".into());
        row.status_updated_at = Some(now.clone());
    }
    let store = RegistryStore {
        schema: SCHEMA,
        created_at: now.clone(),
        updated_at: now.clone(),
        view_sha256: original_sha.clone(),
        view_bytes: bytes.len() as u64,
        trailing_newline,
        imported_from: ImportSource {
            bytes: bytes.len() as u64,
            sha256: original_sha.clone(),
            lines: text.lines().count(),
            at: now.clone(),
        },
        execution_order: parse_execution_order(&text),
        blocks: scanned.blocks,
        rows: scanned.rows,
    };
    let rendered = render(&store);
    if rendered != text {
        let line = first_differing_line(&rendered, &text)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "<no line>".into());
        return Ok(refused_report(
            conductor_dir,
            before,
            store.rows.len(),
            format!("render not byte-identical; first differing line {line}"),
            Vec::new(),
            Vec::new(),
        ));
    }

    // Cross-check provenance/order comparisons against an existing store before writing.
    let store_exists = store_path_at(conductor_dir).is_file();
    let old = load_at(conductor_dir).ok().flatten();
    let mut status_changes = Vec::new();
    let mut order_changes = Vec::new();
    if let Some(old) = &old {
        for row in &store.rows {
            if let Some(prev) = old.rows.iter().find(|r| r.id == row.id)
                && prev.status != row.status
            {
                status_changes.push(StatusChange {
                    id: row.id.clone(),
                    from: prev.status.clone(),
                    to: row.status.clone(),
                });
            }
        }
        let old_ids: Vec<&str> = old.rows.iter().map(|r| r.id.as_str()).collect();
        let new_ids: Vec<&str> = store.rows.iter().map(|r| r.id.as_str()).collect();
        if old_ids != new_ids {
            order_changes.push(format!("{} -> {}", old_ids.join(","), new_ids.join(",")));
        }
    }

    let after = ImportAfter {
        rows: store.rows.len(),
        order: store.execution_order.len(),
        view_bytes: bytes.len() as u64,
        view_sha256: original_sha.clone(),
        byte_identical: true,
    };
    let ok_report = |backup: Option<BackupInfo>| ImportReport {
        status: "ok".into(),
        reason: None,
        conductor_dir: conductor_dir.display().to_string(),
        before: before.clone(),
        after: after.clone(),
        missing_ids: Vec::new(),
        extra_ids: Vec::new(),
        status_changes: status_changes.clone(),
        order_changes: order_changes.clone(),
        rejects: Vec::new(),
        interior_gaps: Vec::new(),
        reject_classes: RejectClasses::default(),
        backup,
        store_written: false,
        view_written: false,
    };

    // A hand edit that landed while the lock was held is caught here.
    let reread = reread_view_bytes(&view_path)?;
    if sha256_hex(&reread) != original_sha {
        return Ok(ImportReport {
            status: "refused".into(),
            reason: Some("registry changed during import; retry".into()),
            ..ok_report(None)
        });
    }
    if opts.dry_run {
        return Ok(ok_report(None));
    }
    if store_exists && !opts.force {
        return Ok(ImportReport {
            status: "refused".into(),
            reason: Some("store exists; use registry verify or import --force".into()),
            ..ok_report(None)
        });
    }
    // Second guard immediately before the backup.
    let reread = reread_view_bytes(&view_path)?;
    if sha256_hex(&reread) != original_sha {
        return Ok(ImportReport {
            status: "refused".into(),
            reason: Some("registry changed during import; retry".into()),
            ..ok_report(None)
        });
    }

    if let Some(old) = &old {
        let _ = crate::persist::atomic_write(
            &conductor_dir.join(format!("{STORE_FILE}.bak")),
            &store_bytes(old)?,
        );
    }

    let mut backup_path = conductor_dir.join(format!("conductor.md.pre-store-{}", compact_stamp()));
    let mut n = 1;
    while backup_path.exists() {
        backup_path = conductor_dir.join(format!("conductor.md.pre-store-{}-{n}", compact_stamp()));
        n += 1;
    }
    crate::persist::atomic_write(&backup_path, &bytes)?;
    let backup_bytes = std::fs::read(&backup_path)?;
    let backup = BackupInfo {
        path: backup_path.display().to_string(),
        sha256: sha256_hex(&backup_bytes),
        verified: sha256_hex(&backup_bytes) == original_sha,
    };
    if !backup.verified {
        return Err(CoordinatorError::Message(format!(
            "backup {} does not match conductor.md; nothing imported",
            backup_path.display()
        )));
    }

    crate::persist::atomic_write(&store_path_at(conductor_dir), &store_bytes(&store)?)?;
    let mut view_written = false;
    let current = std::fs::read(&view_path).ok();
    if current.as_deref() != Some(text.as_bytes()) {
        crate::persist::atomic_write(&view_path, text.as_bytes())?;
        view_written = true;
    }
    Ok(ImportReport {
        store_written: true,
        view_written,
        ..ok_report(Some(backup))
    })
}

// ---------------------------------------------------------------------------
// Verify / render CLI
// ---------------------------------------------------------------------------

pub fn verify_at(conductor_dir: &Path) -> Result<VerifyReport> {
    let Some(store) = load_at(conductor_dir)? else {
        return Ok(VerifyReport {
            status: "absent".into(),
            conductor_dir: conductor_dir.display().to_string(),
            view: ViewReport::default(),
            row_diff: RowDiff::default(),
            layout: LayoutReport {
                width_deviations: Vec::new(),
            },
            nostart_parity: Vec::new(),
            rejects: Vec::new(),
            interior_gaps: Vec::new(),
        });
    };
    let view_path = view_path_at(conductor_dir);
    let bytes = std::fs::read(&view_path).ok();
    let rendered = render(&store);
    let mut report = VerifyReport {
        status: "ok".into(),
        conductor_dir: conductor_dir.display().to_string(),
        view: ViewReport {
            actual_sha256: bytes.as_deref().map(sha256_hex),
            expected_sha256: Some(store.view_sha256.clone()),
            rendered_sha256: Some(sha256_hex(rendered.as_bytes())),
            bytes: bytes.as_ref().map(|b| b.len() as u64),
            expected_bytes: Some(store.view_bytes),
        },
        row_diff: RowDiff::default(),
        layout: LayoutReport {
            width_deviations: Vec::new(),
        },
        nostart_parity: Vec::new(),
        rejects: Vec::new(),
        interior_gaps: Vec::new(),
    };

    // Internal consistency: each row's `line` must re-split to `status`/id/nostart.
    for (idx, row) in store.rows.iter().enumerate() {
        let Some(block) = block_for_row(&store, idx) else {
            report.row_diff.line_mismatch.push(row.id.clone());
            continue;
        };
        let (Some(status_col), Some(track_col)) = (block.status_col(), block.track_col()) else {
            report.row_diff.line_mismatch.push(row.id.clone());
            continue;
        };
        let (stripped, nostart) = strip_nostart_literal(&row.line);
        let cells = split_unescaped_pipes(&stripped);
        let status_ok = cells
            .get(status_col)
            .map(|c| c.trim() == row.status.trim())
            .unwrap_or(false);
        let id_ok = cells
            .get(track_col)
            .and_then(|c| parse_leading_id(&unwrap_md_link(c.trim())))
            .map(|(id, _)| id == row.id)
            .unwrap_or(false);
        if !status_ok || !id_ok || nostart != row.nostart {
            report.row_diff.line_mismatch.push(row.id.clone());
        }
    }

    if let Some(bytes) = &bytes {
        let actual = sha256_hex(bytes);
        if bytes.len() as u64 != store.view_bytes || actual != store.view_sha256 {
            report.status = "drift".into();
        }
        if let Ok(text) = std::str::from_utf8(bytes) {
            let (lines, _) = split_lines(text);
            let scanned = scan(&lines);
            report.rejects = scanned.rejects;
            report.interior_gaps = scanned.interior_gaps;
            report.layout.width_deviations = crate::workflow::conductor_md::width_deviations(text)
                .into_iter()
                .map(|d| WidthDeviationView {
                    line: d.line,
                    got: d.got,
                    want: d.want,
                    preview: d.preview,
                })
                .collect();
            match parse_conductor_md(text) {
                Ok(parsed) => {
                    let store_ids: Vec<&str> = store.rows.iter().map(|r| r.id.as_str()).collect();
                    let view_ids: Vec<&str> = parsed.iter().map(|r| r.id.as_str()).collect();
                    for id in &store_ids {
                        if !view_ids.contains(id) {
                            report.row_diff.missing.push((*id).to_string());
                        }
                    }
                    for id in &view_ids {
                        if !store_ids.contains(id) {
                            report.row_diff.extra.push((*id).to_string());
                        }
                    }
                    if store_ids != view_ids {
                        report.row_diff.order = view_ids.iter().map(|s| (*s).to_string()).collect();
                    }
                    for row in &store.rows {
                        if let Some(view) = parsed.iter().find(|r| r.id == row.id) {
                            if view.status_raw != row.status {
                                report.row_diff.status.push(StatusChange {
                                    id: row.id.clone(),
                                    from: row.status.clone(),
                                    to: view.status_raw.clone(),
                                });
                            }
                            if view.nostart != row.nostart {
                                report.nostart_parity.push(row.id.clone());
                            }
                        }
                    }
                }
                Err(_) => report.status = "drift".into(),
            }
        } else {
            report.status = "drift".into();
        }
    } else {
        report.status = "drift".into();
    }
    if !report.row_diff.line_mismatch.is_empty()
        || !report.row_diff.missing.is_empty()
        || !report.row_diff.extra.is_empty()
        || !report.row_diff.status.is_empty()
        || !report.row_diff.order.is_empty()
        || !report.nostart_parity.is_empty()
    {
        report.status = "drift".into();
    }
    Ok(report)
}

#[derive(Debug, Clone, Serialize)]
pub struct RenderReport {
    pub status: String,
    pub conductor_dir: String,
    pub written: bool,
    pub check: bool,
    pub force: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived: Option<String>,
    pub drift: bool,
    pub view_sha256: Option<String>,
}

/// `registry render [--check] [--force]`.
pub fn render_at(conductor_dir: &Path, check: bool, force: bool) -> Result<RenderReport> {
    with_registry_lock(conductor_dir, || {
        let Some(store) = load_at(conductor_dir)? else {
            return Err(CoordinatorError::Message(format!(
                "no track store at {}; nothing to render",
                store_path_at(conductor_dir).display()
            )));
        };
        let view_path = view_path_at(conductor_dir);
        let body = render(&store);
        let current = std::fs::read(&view_path).ok();
        // A missing view is a projection to recreate, not a human edit to discard.
        let drift = current
            .as_deref()
            .is_some_and(|bytes| bytes != body.as_bytes());
        if check {
            return Ok(RenderReport {
                status: if drift { "drift".into() } else { "ok".into() },
                conductor_dir: conductor_dir.display().to_string(),
                written: false,
                check,
                force,
                archived: None,
                drift,
                view_sha256: Some(sha256_hex(body.as_bytes())),
            });
        }
        if !drift {
            let mut written = false;
            if current.is_none() {
                crate::persist::atomic_write(&view_path, body.as_bytes())?;
                written = true;
            }
            return Ok(RenderReport {
                status: "ok".into(),
                conductor_dir: conductor_dir.display().to_string(),
                written,
                check,
                force,
                archived: None,
                drift: false,
                view_sha256: Some(sha256_hex(body.as_bytes())),
            });
        }
        if !force {
            return Err(CoordinatorError::Message(format!(
                "{} was edited outside the track store; run `coordinator registry import --conductor-dir {} --force` to adopt it or `coordinator registry render --conductor-dir {} --force` to discard it",
                view_path.display(),
                conductor_dir.display(),
                conductor_dir.display()
            )));
        }
        let archived = if let Some(bytes) = current {
            archive_view(conductor_dir, &bytes)?
        } else {
            None
        };
        crate::persist::atomic_write(&view_path, body.as_bytes())?;
        Ok(RenderReport {
            status: "ok".into(),
            conductor_dir: conductor_dir.display().to_string(),
            written: true,
            check,
            force,
            archived,
            drift,
            view_sha256: Some(sha256_hex(body.as_bytes())),
        })
    })
}

fn archive_view(conductor_dir: &Path, bytes: &[u8]) -> Result<Option<String>> {
    let stamp = compact_stamp();
    let mut path = conductor_dir.join(format!("conductor.md.bak-{stamp}-000"));
    let mut seq = 0u32;
    while path.exists() && seq < 999 {
        seq += 1;
        path = conductor_dir.join(format!("conductor.md.bak-{stamp}-{seq:03}"));
    }
    crate::persist::atomic_write(&path, bytes)?;
    // Keep the 5 newest fixed-width sequence archives overall.
    let mut archived: Vec<PathBuf> = std::fs::read_dir(conductor_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("conductor.md.bak-"))
                })
                .collect()
        })
        .unwrap_or_default();
    archived.sort();
    while archived.len() > BACKUP_KEEP {
        let oldest = archived.remove(0);
        let _ = std::fs::remove_file(oldest);
    }
    Ok(Some(path.display().to_string()))
}

// ---------------------------------------------------------------------------
// Mutation core
// ---------------------------------------------------------------------------

enum MatchMode {
    First,
    All,
}

/// Public wrapper for the CLI (`--project`). Acquires and releases the lock.
pub fn set_statuses(
    record: &ProjectRecord,
    changes: &[(String, String)],
    source: &str,
    clear_nostart: bool,
) -> Result<WriteOutcome> {
    set_statuses_at(&conductor_dir_for(record), changes, source, clear_nostart)
}

pub fn set_statuses_at(
    conductor_dir: &Path,
    changes: &[(String, String)],
    source: &str,
    clear_nostart: bool,
) -> Result<WriteOutcome> {
    with_registry_lock(conductor_dir, || {
        set_statuses_locked(conductor_dir, changes, source, clear_nostart)
    })
}

pub fn promote_placeholders(
    record: &ProjectRecord,
    promotions: &[(String, String)],
    source: &str,
) -> Result<WriteOutcome> {
    with_registry_lock(&conductor_dir_for(record), || {
        promote_placeholders_locked(&conductor_dir_for(record), promotions, source)
    })
}

pub(crate) fn set_statuses_locked(
    conductor_dir: &Path,
    changes: &[(String, String)],
    source: &str,
    clear_nostart: bool,
) -> Result<WriteOutcome> {
    let owned: Vec<(String, Option<String>, String)> = changes
        .iter()
        .map(|(target, status)| (target.clone(), None, status.clone()))
        .collect();
    mutate_locked(
        conductor_dir,
        &owned,
        source,
        clear_nostart,
        MatchMode::All,
        false,
    )
}

pub(crate) fn promote_placeholders_locked(
    conductor_dir: &Path,
    promotions: &[(String, String)],
    source: &str,
) -> Result<WriteOutcome> {
    let owned: Vec<(String, Option<String>, String)> = promotions
        .iter()
        .map(|(target, status)| (target.clone(), None, status.clone()))
        .collect();
    mutate_locked(conductor_dir, &owned, source, false, MatchMode::First, true)
}

/// Promote only while each row still carries the exact observed status.
pub(crate) fn promote_placeholders_observed_locked(
    conductor_dir: &Path,
    promotions: &[(String, String, String)],
    source: &str,
) -> Result<WriteOutcome> {
    let owned: Vec<(String, Option<String>, String)> = promotions
        .iter()
        .map(|(target, observed, status)| (target.clone(), Some(observed.clone()), status.clone()))
        .collect();
    mutate_locked(conductor_dir, &owned, source, false, MatchMode::First, true)
}

fn mutate_locked(
    conductor_dir: &Path,
    changes: &[(String, Option<String>, String)],
    source: &str,
    clear_nostart: bool,
    mode: MatchMode,
    require_placeholder: bool,
) -> Result<WriteOutcome> {
    let Some(mut store) = load_at(conductor_dir)? else {
        return Ok(WriteOutcome::NoStore);
    };
    let view_path = view_path_at(conductor_dir);
    match std::fs::read(&view_path) {
        Ok(view) => {
            if view.len() as u64 != store.view_bytes || sha256_hex(&view) != store.view_sha256 {
                return Err(CoordinatorError::Message(format!(
                    "conductor.md was edited outside the track store ({}); run `coordinator registry import --conductor-dir {} --force` to adopt it, or `coordinator registry render --conductor-dir {} --force` to discard it",
                    view_path.display(),
                    conductor_dir.display(),
                    conductor_dir.display()
                )));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }

    let mut changed: Vec<String> = Vec::new();
    for (target, observed, new_status) in changes {
        let (content, _) = strip_nostart_literal(new_status);
        let content = content.trim().to_string();
        let limit = if matches!(mode, MatchMode::First) {
            1
        } else {
            usize::MAX
        };
        let matches: Vec<usize> = store
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| row_matches_target(r, target))
            .map(|(i, _)| i)
            .take(limit)
            .collect();
        for idx in matches {
            let (status_col, track_col) = {
                let block = block_for_row(&store, idx).ok_or_else(|| {
                    CoordinatorError::Message(format!(
                        "store row {} has no table block",
                        store.rows[idx].id
                    ))
                })?;
                match (block.status_col(), block.track_col()) {
                    (Some(s), Some(t)) => (s, t),
                    _ => {
                        return Err(CoordinatorError::Message(format!(
                            "store row {} is not in a track table",
                            store.rows[idx].id
                        )));
                    }
                }
            };
            if require_placeholder {
                // The row must still be the placeholder the caller observed, and — when the
                // caller passed the exact status — still carry it verbatim. A concurrent
                // writer that won between the caller's read and this lock is skipped.
                let current = &store.rows[idx].status;
                if !is_proposed(current) || !status_names_placeholder(current) {
                    continue;
                }
                if let Some(expected) = observed
                    && current != expected
                {
                    continue;
                }
            }
            let row = &mut store.rows[idx];
            let base_line = if clear_nostart {
                strip_nostart_literal(&row.line).0
            } else {
                row.line.clone()
            };
            let line = if clear_nostart {
                match replace_cell(&base_line, status_col, &content) {
                    Some(line) => line,
                    None => {
                        return Err(CoordinatorError::Message(format!(
                            "store row {}: status cell {} is missing from the stored line",
                            row.id, status_col
                        )));
                    }
                }
            } else {
                // Same helper as the legacy markdown writers: writer parity for a marker row.
                match crate::workflow::conductor_md::replace_status_cell_preserving_marker(
                    &base_line, status_col, &content,
                ) {
                    Some(line) => line,
                    None => {
                        return Err(CoordinatorError::Message(format!(
                            "store row {}: status cell {} is missing from the stored line",
                            row.id, status_col
                        )));
                    }
                }
            };
            // Re-split post-condition before any write.
            let (stripped, new_nostart) = strip_nostart_literal(&line);
            let cells = split_unescaped_pipes(&stripped);
            let status_ok = cells
                .get(status_col)
                .map(|c| c.trim() == content)
                .unwrap_or(false);
            let id_ok = cells
                .get(track_col)
                .and_then(|c| parse_leading_id(&unwrap_md_link(c.trim())))
                .map(|(id, _)| id == row.id)
                .unwrap_or(false);
            if !status_ok || !id_ok {
                return Err(CoordinatorError::Message(format!(
                    "store row {}: post-mutation line mismatch; nothing written",
                    row.id
                )));
            }
            // A marker in another cell stays, and `nostart` follows the line.
            row.line = line;
            row.status = content.clone();
            row.nostart = new_nostart;
            row.status_source = Some(source.to_string());
            row.status_updated_at = Some(now_rfc3339());
            if !changed.contains(&row.id) {
                changed.push(row.id.clone());
            }
        }
    }
    if changed.is_empty() {
        return Ok(WriteOutcome::Unchanged);
    }
    let body = render(&store);
    store.view_sha256 = sha256_hex(body.as_bytes());
    store.view_bytes = body.len() as u64;
    store.updated_at = now_rfc3339();
    crate::persist::atomic_write(&store_path_at(conductor_dir), &store_bytes(&store)?)?;
    #[cfg(test)]
    if FORCE_VIEW_FAILURE.with(std::cell::Cell::get) {
        return Err(CoordinatorError::Message(format!(
            "track store committed but the conductor.md view is stale (injected failure); run `coordinator registry render --conductor-dir {} --force`",
            conductor_dir.display()
        )));
    }
    if let Err(e) = crate::persist::atomic_write(&view_path, body.as_bytes()) {
        return Err(CoordinatorError::Message(format!(
            "track store committed but the conductor.md view is stale ({e}); run `coordinator registry render --conductor-dir {} --force`",
            conductor_dir.display()
        )));
    }
    Ok(WriteOutcome::Changed(changed))
}

/// Dry-run support for the CLI: what `set_statuses_at` would change, without writing.
pub fn preview_set_statuses_at(
    conductor_dir: &Path,
    track: &str,
    status: &str,
    clear_nostart: bool,
) -> Result<Vec<String>> {
    let Some(store) = load_at(conductor_dir)? else {
        return Err(CoordinatorError::Message(format!(
            "no track store at {}; set-status has no markdown fallback",
            store_path_at(conductor_dir).display()
        )));
    };
    let view = std::fs::read(view_path_at(conductor_dir)).ok();
    if let Some(view) = view.as_ref()
        && (view.len() as u64 != store.view_bytes || sha256_hex(view) != store.view_sha256)
    {
        return Err(CoordinatorError::Message(format!(
            "conductor.md was edited outside the track store ({}); run `coordinator registry import --conductor-dir {} --force` to adopt it first",
            view_path_at(conductor_dir).display(),
            conductor_dir.display()
        )));
    }
    let _ = (status, clear_nostart);
    Ok(store
        .rows
        .iter()
        .filter(|r| row_matches_target(r, track))
        .map(|r| r.id.clone())
        .collect())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::LayoutProfile;
    use crate::registry::AutoStartPolicy;
    use crate::state::{RunState, save_run_state};
    use std::collections::BTreeMap;
    use tempfile::{TempDir, tempdir};

    const READY: &str = "**Ready — not started**";
    const PROPOSED: &str = "**Proposed — placeholder, needs full spec/plan pass**";
    const HEAD: &str = "| Track | Execution path | Status | Summary |\n\
                        | --- | --- | --- | --- |\n";

    fn rec(ws: &std::path::Path, id: &str) -> ProjectRecord {
        ProjectRecord {
            id: id.to_string(),
            path: ws.to_path_buf(),
            display_name: None,
            layout_profile: LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: BTreeMap::new(),
            state_dir: None,
            auto_merge: true,
            phase_timeouts_secs: BTreeMap::new(),
            notify_progress: false,
            worktree_isolation: false,
            ready_aliases: Vec::new(),
            auto_start: AutoStartPolicy::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            ci_fix_routing: false,
            skill_aliases: BTreeMap::new(),
            created_at: chrono::Utc::now(),
        }
    }

    struct Fx {
        _ws: TempDir,
        record: ProjectRecord,
    }

    impl Fx {
        fn dir(&self) -> PathBuf {
            self.record.path.join("conductor")
        }
        fn view(&self) -> PathBuf {
            self.dir().join("conductor.md")
        }
        fn write_view(&self, body: &str) {
            std::fs::create_dir_all(self.dir()).unwrap();
            std::fs::write(self.view(), body).unwrap();
        }
        fn read_view(&self) -> String {
            std::fs::read_to_string(self.view()).unwrap()
        }
        fn import(&self) -> ImportReport {
            import_at(&self.dir(), ImportOpts::default()).unwrap()
        }
        fn import_force(&self) -> ImportReport {
            import_at(
                &self.dir(),
                ImportOpts {
                    dry_run: false,
                    force: true,
                },
            )
            .unwrap()
        }
        fn store(&self) -> RegistryStore {
            load_at(&self.dir()).unwrap().expect("store")
        }
        fn write_store(&self, store: &RegistryStore) {
            std::fs::write(store_path_at(&self.dir()), store_bytes(store).unwrap()).unwrap();
        }
        fn edit_store(&self, f: impl FnOnce(&mut RegistryStore)) {
            let mut store = self.store();
            f(&mut store);
            self.write_store(&store);
        }
        fn track(&self, dir_name: &str, plan: &str) {
            let dir = self.dir().join(dir_name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("spec.md"), b"# spec\n").unwrap();
            std::fs::write(dir.join("plan.md"), plan.as_bytes()).unwrap();
        }
        fn state_before(&self, before: std::collections::BTreeMap<String, String>) {
            let mut state = RunState::idle(&self.record.id);
            state.registry_status_before = before;
            save_run_state(&self.record, &state).unwrap();
        }
        fn status_log(&self) -> String {
            std::fs::read_to_string(self.record.path.join("status.md")).unwrap_or_default()
        }
    }

    fn fixture() -> Fx {
        let ws = tempdir().unwrap();
        let record = rec(ws.path(), &format!("p-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(ws.path().join("conductor")).unwrap();
        Fx { _ws: ws, record }
    }

    fn row(id: &str, status: &str, summary: &str) -> String {
        format!("| [{id}-T]({id}-T/spec.md) | `.` | {status} | {summary} |\n")
    }

    fn registry(rows: &[String]) -> String {
        let mut body = String::from(HEAD);
        for r in rows {
            body.push_str(r);
        }
        body
    }

    fn rich_registry() -> String {
        format!(
            "intro prose\n\n{HEAD}\
             | [0001-One](0001-One/spec.md) | `.` | {READY} | go |\n\
             | [0002-Two](0002-Two/spec.md) | `.` | **Proposed** | a \\| b |\n\
             \n## Suggested execution order\n\n```\n0002 Two\n0001 One\n```\n\n\
             | Id | Status | Summary |\n| --- | --- | --- |\n\
             | T374 | **Completed** | short id |\n\n## Adding a track\n\nprose tail\n"
        )
    }

    fn single(status: &str, summary: &str) -> Fx {
        let fx = fixture();
        fx.write_view(&registry(&[row("0001", status, summary)]));
        fx
    }

    // -- Phase 1: import / render / verify ---------------------------------

    #[test]
    fn import_multi_table_registry_round_trips_bytes() {
        let fx = fixture();
        let body = rich_registry();
        fx.write_view(&body);
        let report = fx.import();
        assert_eq!(report.status, "ok", "{report:?}");
        assert!(report.after.byte_identical);
        assert!(report.store_written);
        assert!(!report.view_written, "{report:?}");
        assert_eq!(report.after.rows, 3);
        assert_eq!(report.after.order, 2);
        let store = fx.store();
        assert_eq!(store.rows[0].id, "0001");
        assert_eq!(store.rows[1].summary, "a | b");
        assert_eq!(store.rows[2].id, "374");
        assert_eq!(render(&store), body);
        assert_eq!(fx.read_view(), body);
    }

    #[test]
    fn import_is_idempotent() {
        let fx = fixture();
        fx.write_view(&rich_registry());
        let first = fx.import();
        let second = fx.import_force();
        assert_eq!(second.status, "ok");
        assert!(second.status_changes.is_empty(), "{second:?}");
        assert!(second.order_changes.is_empty(), "{second:?}");
        assert_eq!(second.after.view_sha256, first.after.view_sha256);
        for row in &fx.store().rows {
            assert_eq!(row.status_source.as_deref(), Some("import"));
            assert!(row.status_updated_at.is_some());
        }
        assert!(fx.dir().join("track-registry.json.bak").is_file());
    }

    #[test]
    fn import_refuses_cell_count_mismatch() {
        let fx = fixture();
        fx.write_view(&registry(&[
            row("0001", "**Ready — not started**", "a | b | c"),
            row("0002", "**Ready — not started**", "c | d | e"),
        ]));
        let before: Vec<_> = std::fs::read_dir(fx.dir()).unwrap().collect();
        let report = fx.import();
        assert_eq!(report.status, "refused");
        assert_eq!(report.reject_classes.cell_count, 2);
        assert_eq!(report.rejects.len(), 2);
        assert_eq!(report.rejects[0].got, Some(6));
        assert!(!store_path_at(&fx.dir()).exists());
        assert!(!fx.dir().join("track-registry.json.bak").exists());
        assert_eq!(std::fs::read_dir(fx.dir()).unwrap().count(), before.len());
    }

    #[test]
    fn import_refuses_unparseable_track_cell() {
        let fx = fixture();
        let body = format!(
            "{HEAD}\
             | ~~[0099-Gone](0099-Gone/spec.md)~~ | `.` | **Completed** | archive |\n\
             | [AI-T01](AI-T01/spec.md) | `.` | **Completed** | alpha |\n"
        );
        fx.write_view(&body);
        let report = fx.import();
        assert_eq!(report.status, "refused");
        assert_eq!(report.reject_classes.struck_through, 1);
        assert_eq!(report.reject_classes.alpha_prefixed, 1);
        assert!(!store_path_at(&fx.dir()).exists());
    }

    #[test]
    fn import_refuses_undecodable_inputs() {
        let fx = fixture();
        fx.write_view(&registry(&[row("0001", READY, "x")]).replace('\n', "\r\n"));
        let report = fx.import();
        assert_eq!(report.status, "refused");
        let reason = report.reason.clone().unwrap_or_default();
        assert!(reason.contains("CRLF"), "{report:?}");

        let fx = fixture();
        let mut body = registry(&[row("0001", READY, "x")]);
        body.insert(0, '\u{feff}');
        fx.write_view(&body);
        let report = fx.import();
        let reason = report.reason.clone().unwrap_or_default();
        assert!(reason.contains("BOM"), "{report:?}");

        let fx = fixture();
        std::fs::write(fx.view(), [0x23, 0xff, 0xfe, 0x0a]).unwrap();
        let report = fx.import();
        let reason = report.reason.clone().unwrap_or_default();
        assert!(reason.contains("UTF-8"), "{report:?}");
        assert!(!store_path_at(&fx.dir()).exists());
    }

    #[test]
    fn import_refuses_when_store_exists_without_force() {
        let fx = fixture();
        fx.write_view(&registry(&[row("0001", READY, "x")]));
        fx.import();
        let store_before = std::fs::read(store_path_at(&fx.dir())).unwrap();
        let report = fx.import();
        assert_eq!(report.status, "refused");
        assert!(report.reason.unwrap().contains("store exists"));
        assert_eq!(
            std::fs::read(store_path_at(&fx.dir())).unwrap(),
            store_before
        );

        // A corrupt store still counts as an existing store: no implicit overwrite.
        std::fs::write(store_path_at(&fx.dir()), b"{not json").unwrap();
        let report = fx.import();
        assert_eq!(report.status, "refused");
        let reason = report.reason.clone().unwrap_or_default();
        assert!(reason.contains("store exists"), "{reason}");
        assert_eq!(
            std::fs::read(store_path_at(&fx.dir())).unwrap(),
            b"{not json"
        );
    }

    #[test]
    fn import_force_keeps_previous_store_bak() {
        let fx = fixture();
        fx.write_view(&registry(&[row("0001", READY, "x")]));
        fx.import();
        let old = std::fs::read(store_path_at(&fx.dir())).unwrap();
        fx.write_view(&registry(&[row("0001", "**Completed**", "x")]));
        fx.import_force();
        let bak = std::fs::read(fx.dir().join("track-registry.json.bak")).unwrap();
        assert_eq!(bak, old);
    }

    #[test]
    fn import_writes_verified_backup() {
        let fx = fixture();
        let body = registry(&[row("0001", READY, "x")]);
        fx.write_view(&body);
        let report = fx.import();
        let backup = report.backup.expect("backup");
        assert!(backup.verified);
        assert_eq!(backup.sha256, sha256_hex(body.as_bytes()));
        assert_eq!(std::fs::read(&backup.path).unwrap(), body.as_bytes());
    }

    #[test]
    fn import_refuses_if_view_changed_during_import() {
        let _seams = reset_seams();
        let fx = fixture();
        fx.write_view(&registry(&[row("0001", READY, "x")]));
        set_import_changed_during_test(true);
        let report = fx.import();
        assert_eq!(report.status, "refused");
        assert!(report.reason.unwrap().contains("changed during import"));
        assert!(!store_path_at(&fx.dir()).exists());
        assert!(!fx.dir().join("track-registry.json.bak").exists());
    }

    #[test]
    fn import_dry_run_writes_nothing() {
        let fx = fixture();
        fx.write_view(&registry(&[row("0001", READY, "x")]));
        let before: Vec<_> = std::fs::read_dir(fx.dir())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        let report = import_at(
            &fx.dir(),
            ImportOpts {
                dry_run: true,
                force: false,
            },
        )
        .unwrap();
        assert_eq!(report.status, "ok");
        assert_eq!(report.after.rows, 1);
        assert!(!report.store_written);
        assert!(!report.view_written);
        let after: Vec<_> = std::fs::read_dir(fx.dir())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        assert_eq!(before, after);
    }

    #[test]
    fn cross_check_detects_field_divergence() {
        let fx = fixture();
        fx.write_view(&rich_registry());
        fx.import();
        let store = fx.store();
        let parsed = parse_conductor_md(&render(&store)).unwrap();
        assert!(rows_match(&parsed, &store.rows).is_none());
        let mut mutated = store.rows.clone();
        mutated[0].summary.push('!');
        assert!(rows_match(&parsed, &mutated).is_some());
        let mut mutated = store.rows.clone();
        mutated[2].nostart = !mutated[2].nostart;
        assert!(rows_match(&parsed, &mutated).is_some());
    }

    #[test]
    fn render_is_byte_stable_across_two_calls() {
        let fx = fixture();
        fx.write_view(&rich_registry());
        fx.import();
        let store = fx.store();
        assert_eq!(render(&store), render(&store));
        let file = std::fs::read(store_path_at(&fx.dir())).unwrap();
        assert_eq!(
            file,
            store_bytes(&load_at(&fx.dir()).unwrap().unwrap()).unwrap()
        );
    }

    #[test]
    fn render_changes_exactly_one_status_cell() {
        let fx = fixture();
        fx.write_view(&registry(&[
            row("0001", READY, "a \\| b"),
            row("0002", READY, "c"),
        ]));
        fx.import();
        let before = fx.store();
        let outcome = set_statuses(
            &fx.record,
            &[("0001".into(), "**Completed**".into())],
            "operator",
            false,
        )
        .unwrap();
        assert_eq!(outcome, WriteOutcome::Changed(vec!["0001".to_string()]));
        let after = fx.store();
        assert_eq!(before.rows[1].line, after.rows[1].line);
        let old = split_unescaped_pipes(&before.rows[0].line);
        let new = split_unescaped_pipes(&after.rows[0].line);
        assert_eq!(old.len(), new.len());
        for (i, (a, b)) in old.iter().zip(new.iter()).enumerate() {
            if i == 2 {
                assert_ne!(a, b);
                assert_eq!(b.trim(), "**Completed**");
            } else {
                assert_eq!(a, b, "cell {i} changed");
            }
        }
        assert!(after.rows[0].line.contains("a \\| b"));
        assert!(fx.read_view().ends_with('\n'));
    }

    #[test]
    fn verify_reports_status_row_order_and_byte_drift() {
        // (a) one status byte in the view
        let fx = single(READY, "x");
        assert_eq!(fx.import().status, "ok");
        fx.write_view(&fx.read_view().replace("Ready", "Ready!"));
        let report = verify_at(&fx.dir()).unwrap();
        assert_eq!(report.status, "drift");
        assert_ne!(report.view.actual_sha256, report.view.expected_sha256);
        assert!(!report.row_diff.status.is_empty());

        // (b) one row line changed
        let fx = single(READY, "x");
        fx.import();
        fx.write_view(&fx.read_view().replace("| x |", "| y |"));
        let report = verify_at(&fx.dir()).unwrap();
        assert_eq!(report.status, "drift");

        // (c) order changed
        let fx = fixture();
        fx.write_view(&registry(&[
            row("0001", READY, "a"),
            row("0002", READY, "b"),
        ]));
        fx.import();
        let swapped = registry(&[row("0002", READY, "b"), row("0001", READY, "a")]);
        fx.write_view(&swapped);
        let report = verify_at(&fx.dir()).unwrap();
        assert_eq!(report.status, "drift");
        assert_eq!(report.row_diff.order, vec!["0002", "0001"]);

        // (d) truncated view
        let fx = single(READY, "x");
        fx.import();
        let text = fx.read_view();
        fx.write_view(&text[..text.len() / 2]);
        let report = verify_at(&fx.dir()).unwrap();
        assert_eq!(report.status, "drift");
        assert_ne!(report.view.bytes, report.view.expected_bytes);

        // (e) store status without its line
        let fx = single(READY, "x");
        fx.import();
        fx.edit_store(|s| s.rows[0].status = "**Blocked**".into());
        let report = verify_at(&fx.dir()).unwrap();
        assert_eq!(report.status, "drift");
        assert_eq!(report.row_diff.line_mismatch, vec!["0001"]);

        // (f) nostart flag without its line
        let fx = single(READY, "x");
        fx.import();
        fx.edit_store(|s| s.rows[0].nostart = true);
        let report = verify_at(&fx.dir()).unwrap();
        assert_eq!(report.status, "drift");
        assert_eq!(report.row_diff.line_mismatch, vec!["0001"]);
    }

    #[test]
    fn render_check_and_force_archive() {
        let fx = fixture();
        fx.write_view(&registry(&[row("0001", READY, "x")]));
        fx.import();
        let ok = render_at(&fx.dir(), true, false).unwrap();
        assert_eq!(ok.status, "ok");
        assert!(!ok.drift);

        // A missing view is recreated by plain `render` (no human edit to discard).
        std::fs::remove_file(fx.view()).unwrap();
        let healed = render_at(&fx.dir(), false, false).unwrap();
        assert!(healed.written);
        assert_eq!(fx.read_view(), render(&fx.store()));

        fx.write_view(&fx.read_view().replace("| Track |", "|  Track |"));
        let check = render_at(&fx.dir(), true, false).unwrap();
        assert_eq!(check.status, "drift");
        assert!(check.drift);
        let dry = render_at(&fx.dir(), false, false);
        assert!(dry.is_err(), "plain render must refuse a drifted view");

        let drifted = fx.read_view();
        let forced = render_at(&fx.dir(), false, true).unwrap();
        assert!(forced.written);
        let archived = forced.archived.expect("archived");
        assert_eq!(std::fs::read_to_string(&archived).unwrap(), drifted);
        assert_eq!(fx.read_view(), render(&fx.store()));

        for _ in 0..6 {
            fx.write_view(&fx.read_view().replace("| Track |", "|   Track |"));
            render_at(&fx.dir(), false, true).unwrap();
        }
        let kept = std::fs::read_dir(fx.dir())
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("conductor.md.bak-"))
            })
            .count();
        assert_eq!(kept, BACKUP_KEEP);
    }

    #[test]
    fn store_load_is_typed_on_unknown_schema_and_bad_json() {
        let fx = single(READY, "x");
        fx.import();
        std::fs::write(store_path_at(&fx.dir()), b"{not json").unwrap();
        let err = load_at(&fx.dir()).unwrap_err();
        assert!(err.to_string().contains("track-registry.json"), "{err}");

        let mut store = {
            // rebuild a valid store, then change only the schema
            fx.write_view(&registry(&[row("0001", READY, "x")]));
            import_at(
                &fx.dir(),
                ImportOpts {
                    dry_run: false,
                    force: true,
                },
            )
            .unwrap();
            fx.store()
        };
        store.schema = 99;
        fx.write_store(&store);
        let err = load_at(&fx.dir()).unwrap_err();
        assert!(err.to_string().contains("unknown schema 99"), "{err}");
    }

    #[test]
    fn import_refuses_interior_table_gap() {
        let fx = fixture();
        let body = format!(
            "{HEAD}| [0001-A](0001-A/spec.md) | `.` | {READY} | a |\n\
             \n---\n\
             | [0002-B](0002-B/spec.md) | `.` | {READY} | b |\n"
        );
        fx.write_view(&body);
        let report = fx.import();
        assert_eq!(report.status, "refused");
        let reason = report.reason.clone().unwrap_or_default();
        assert!(reason.contains("interior"));
        assert_eq!(report.interior_gaps.len(), 1);
        assert!(!store_path_at(&fx.dir()).exists());
    }

    #[test]
    fn import_trailing_gap_round_trips() {
        let fx = fixture();
        let body = format!(
            "{HEAD}| [0001-A](0001-A/spec.md) | `.` | {READY} | a |\n\
             \n---\n\n## Adding a track\n\nprose\n"
        );
        fx.write_view(&body);
        let report = fx.import();
        assert_eq!(report.status, "ok", "{report:?}");
        assert!(report.after.byte_identical);
        assert_eq!(render(&fx.store()), body);
    }

    #[test]
    fn import_holds_registry_lock_across_read_and_write() {
        let _seams = reset_seams();
        let fx = fixture();
        fx.write_view(&registry(&[row("0001", READY, "a")]));
        let dir = fx.dir();
        let view = fx.view();
        let holder_dir = dir.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            with_registry_lock(&holder_dir, || {
                // The lock is held before the signal, so the import below cannot win it.
                tx.send(()).unwrap();
                let body = std::fs::read_to_string(&view).unwrap();
                std::fs::write(&view, format!("{body}{}", row("0002", READY, "b"))).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(150));
                Ok(())
            })
            .unwrap();
        });
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("holder signalled");
        assert!(dir.join(LOCK_DIR).exists(), "holder must hold the lock");
        let report = import_at(&dir, ImportOpts::default()).unwrap();
        holder.join().unwrap();
        assert_eq!(report.status, "ok");
        let store = load_at(&dir).unwrap().unwrap();
        assert_eq!(store.rows.len(), 2);
        assert_eq!(store.rows[1].id, "0002");
    }

    #[test]
    fn verify_strips_nostart_before_cell_compare() {
        let fx = fixture();
        fx.write_view(&registry(&[row(
            "0115",
            "**Proposed - placeholder** <!-- nostart -->",
            "x",
        )]));
        assert_eq!(fx.import().status, "ok");
        let report = verify_at(&fx.dir()).unwrap();
        assert_eq!(report.status, "ok", "{report:?}");
        let store = fx.store();
        assert_eq!(store.rows[0].status, "**Proposed - placeholder**");
        assert!(store.rows[0].nostart);
        let cells = split_unescaped_pipes(&store.rows[0].line);
        let raw = cells[2].trim();
        assert!(raw.ends_with("<!-- nostart -->"));
        assert_ne!(raw, store.rows[0].status, "un-stripped compare would fail");
    }

    #[test]
    fn import_render_reparse_parity_including_nostart() {
        let fx = fixture();
        fx.write_view(&registry(&[
            row("0001", READY, "a"),
            row("0116", "**Proposed** <!-- nostart -->", "b"),
        ]));
        fx.import();
        let store = fx.store();
        let parsed = parse_conductor_md(&render(&store)).unwrap();
        assert_eq!(parsed.len(), store.rows.len());
        for (p, s) in parsed.iter().zip(store.rows.iter()) {
            assert_eq!(p.id, s.id);
            assert_eq!(p.slug, s.slug);
            assert_eq!(p.status_raw, s.status);
            assert_eq!(p.summary, s.summary);
            assert_eq!(p.execution, s.execution);
            assert_eq!(p.nostart, s.nostart);
        }
        assert!(parsed[1].nostart);
    }

    #[test]
    fn import_reports_reject_classes() {
        let fx = fixture();
        let body = format!(
            "{HEAD}\
             | [0001-A](0001-A/spec.md) | `.` | {READY} | a | b |\n\
             | ~~[0002-B](0002-B/spec.md)~~ | `.` | **Completed** | b |\n\
             | [AI-T03](AI-T03/spec.md) | `.` | **Completed** | c |\n\
             \n| [0004-D](0004-D/spec.md) | `.` | {READY} | d |\n"
        );
        fx.write_view(&body);
        let report = fx.import();
        assert_eq!(report.status, "refused");
        assert_eq!(report.reject_classes.cell_count, 1);
        assert_eq!(report.reject_classes.struck_through, 1);
        assert_eq!(report.reject_classes.alpha_prefixed, 1);
        assert_eq!(report.interior_gaps.len(), 1);
    }

    #[test]
    fn store_lock_times_out_and_breaks_stale() {
        let _seams = reset_seams();
        let fx = fixture();
        let lock = fx.dir().join(LOCK_DIR);
        std::fs::create_dir(&lock).unwrap();
        set_lock_seams(Some(60), Some(10_000));
        let err = with_registry_lock(&fx.dir(), || Ok(())).unwrap_err();
        assert!(err.to_string().contains("held"), "{err}");
        std::fs::remove_dir(&lock).unwrap();

        set_lock_seams(Some(60), Some(0));
        std::fs::create_dir(&lock).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        with_registry_lock(&fx.dir(), || Ok(())).expect("a stale lock is broken");
        assert!(!lock.exists());
    }

    // -- Phase 3: writer seam ------------------------------------------------

    #[test]
    fn set_statuses_updates_store_and_only_the_status_cell() {
        let fx = single(READY, "x");
        fx.import();
        set_statuses(
            &fx.record,
            &[("0001".into(), "**In progress**".into())],
            "operator",
            false,
        )
        .unwrap();
        let store = fx.store();
        assert_eq!(store.rows[0].status, "**In progress**");
        assert_eq!(store.rows[0].status_source.as_deref(), Some("operator"));
        assert!(store.rows[0].status_updated_at.is_some());
        assert!(fx.read_view().contains("**In progress**"));
    }

    #[test]
    fn set_statuses_refuses_when_view_drifted() {
        let fx = single(READY, "x");
        fx.import();
        fx.write_view(&fx.read_view().replace("| x |", "| hand edit |"));
        let store_before = std::fs::read(store_path_at(&fx.dir())).unwrap();
        let view_before = fx.read_view();
        let err = set_statuses(
            &fx.record,
            &[("0001".into(), "**Completed**".into())],
            "operator",
            false,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("import"), "{msg}");
        assert!(msg.contains("registry render"), "{msg}");
        assert!(msg.contains("--force"), "{msg}");
        assert_eq!(
            std::fs::read(store_path_at(&fx.dir())).unwrap(),
            store_before
        );
        assert_eq!(fx.read_view(), view_before);
    }

    #[test]
    fn set_statuses_writes_store_before_view() {
        let _seams = reset_seams();
        let fx = single(READY, "x");
        fx.import();
        force_view_write_failure(true);
        let err = set_statuses(
            &fx.record,
            &[("0001".into(), "**Completed**".into())],
            "operator",
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("registry render"), "{err}");
        assert_eq!(fx.store().rows[0].status, "**Completed**");
        assert!(!fx.read_view().contains("**Completed**"));
        let healed = render_at(&fx.dir(), false, true).unwrap();
        assert!(healed.written);
        assert_eq!(verify_at(&fx.dir()).unwrap().status, "ok");
    }

    #[test]
    fn registry_lock_serializes_two_writers() {
        let _seams = reset_seams();
        let fx = fixture();
        fx.write_view(&registry(&[
            row("0001", READY, "a"),
            row("0002", READY, "b"),
        ]));
        fx.import();
        let dir = fx.dir();
        let mut handles = Vec::new();
        for track in ["0001", "0002"] {
            let dir = dir.clone();
            handles.push(std::thread::spawn(move || {
                set_statuses_at(
                    &dir,
                    &[(track.to_string(), "**Completed**".into())],
                    "operator",
                    false,
                )
                .unwrap()
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let store = load_at(&dir).unwrap().unwrap();
        assert_eq!(store.rows[0].status, "**Completed**");
        assert_eq!(store.rows[1].status, "**Completed**");
    }

    #[test]
    fn registry_lock_breaks_a_stale_lock() {
        let _seams = reset_seams();
        let fx = single(READY, "x");
        fx.import();
        let lock = fx.dir().join(LOCK_DIR);
        std::fs::create_dir(&lock).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        set_lock_seams(Some(200), Some(0));
        let outcome = set_statuses(
            &fx.record,
            &[("0001".into(), "**Completed**".into())],
            "operator",
            false,
        )
        .unwrap();
        assert!(matches!(outcome, WriteOutcome::Changed(_)));
        assert_eq!(fx.store().rows[0].status, "**Completed**");
    }

    #[test]
    fn promotion_skips_when_the_observed_status_changed() {
        let fx = single(PROPOSED, "fixture");
        fx.track("0001-T", "# 0001 — real plan\n");
        fx.import();
        // A stale observation (what the fold gate saw) must not overwrite a newer status.
        let stale = promote_placeholders_observed_locked(
            &fx.dir(),
            &[(
                "0001".to_string(),
                "**Proposed** — stale".to_string(),
                READY.to_string(),
            )],
            "fold",
        )
        .unwrap();
        assert_eq!(stale, WriteOutcome::Unchanged);
        assert_eq!(fx.store().rows[0].status, PROPOSED);

        // The exact observed status promotes.
        let exact = promote_placeholders_observed_locked(
            &fx.dir(),
            &[("0001".to_string(), PROPOSED.to_string(), READY.to_string())],
            "fold",
        )
        .unwrap();
        assert_eq!(exact, WriteOutcome::Changed(vec!["0001".to_string()]));
        assert_eq!(fx.store().rows[0].status, READY);
    }

    #[test]
    fn promote_and_restore_marker_parity_markdown_vs_store() {
        // Promote a marker-bearing placeholder: both modes keep the marker and match bytes.
        let marker_proposed = "**Proposed - placeholder** <!-- nostart -->";
        let legacy = fixture();
        legacy.write_view(&registry(&[row("0020", marker_proposed, "p")]));
        legacy.track("0020-T", "# 0020 — real plan\n");
        assert!(
            crate::workflow::conductor_md::promote_placeholder(&legacy.record, "0020", READY)
                .unwrap()
        );

        let stored = fixture();
        stored.write_view(&registry(&[row("0020", marker_proposed, "p")]));
        stored.track("0020-T", "# 0020 — real plan\n");
        stored.import();
        assert!(
            crate::workflow::conductor_md::promote_placeholder(&stored.record, "0020", READY)
                .unwrap()
        );
        assert_eq!(stored.read_view(), legacy.read_view());
        for fx in [&legacy, &stored] {
            let rows = crate::workflow::conductor_md::load_track_rows(&fx.record).unwrap();
            assert!(rows[0].nostart, "{}", fx.read_view());
            assert_eq!(rows[0].status_raw, READY);
        }

        // Restore a marker-bearing downgrade: both modes match bytes and keep the marker.
        let original = registry(&[row("0030", &format!("{READY} <!-- nostart -->"), "a")]);
        let downgraded = registry(&[row("0030", "**Proposed** <!-- nostart -->", "a")]);
        let before = crate::workflow::conductor_md::status_snapshot(&original);

        let legacy = fixture();
        legacy.write_view(&downgraded);
        legacy.state_before(before.clone());
        let legacy_restore = crate::workflow::drive::restore_ready_rows_after_phase(&legacy.record);
        assert_eq!(legacy_restore.restored, vec!["0030".to_string()]);

        let stored = fixture();
        stored.write_view(&downgraded);
        stored.import();
        stored.state_before(before);
        let stored_restore = crate::workflow::drive::restore_ready_rows_after_phase(&stored.record);
        assert_eq!(stored_restore.restored, vec!["0030".to_string()]);
        assert_eq!(stored.read_view(), legacy.read_view());
        assert!(
            stored.read_view().contains("<!-- nostart -->"),
            "{}",
            stored.read_view()
        );
        assert_eq!(stored.store().rows[0].status, READY);
        assert!(stored.store().rows[0].nostart);
    }

    #[test]
    fn promote_placeholder_store_mode_matches_legacy_bytes() {
        // Legacy fixture (no store) and store fixture (imported) start from identical bytes.
        let legacy = fixture();
        let body = registry(&[row("0020", PROPOSED, "fixture")]);
        legacy.write_view(&body);
        legacy.track("0020-T", "# 0020 — real plan\n");
        assert!(
            crate::workflow::conductor_md::promote_placeholder(&legacy.record, "0020", READY)
                .unwrap()
        );

        let stored = fixture();
        stored.write_view(&body);
        stored.track("0020-T", "# 0020 — real plan\n");
        stored.import();
        assert!(
            crate::workflow::conductor_md::promote_placeholder(&stored.record, "0020", READY)
                .unwrap()
        );
        assert_eq!(stored.read_view(), legacy.read_view());
        assert_eq!(
            stored.store().rows[0].status_source.as_deref(),
            Some("fold")
        );
    }

    #[test]
    fn restore_ready_rows_store_mode_matches_legacy() {
        let original = registry(&[
            row("0030", READY, "a"),
            row("0040", READY, "b"),
            row("0050", "**Completed**", "c"),
        ]);
        let downgraded = registry(&[
            row("0030", "**Proposed**", "a"),
            row("0040", "**Proposed** — reasoned hold", "b"),
            row("0050", "**Completed**", "c"),
        ]);
        let before = crate::workflow::conductor_md::status_snapshot(&original);

        let legacy = fixture();
        legacy.write_view(&downgraded);
        legacy.state_before(before.clone());
        let legacy_restore = crate::workflow::drive::restore_ready_rows_after_phase(&legacy.record);
        assert_eq!(legacy_restore.restored, vec!["0030".to_string()]);
        assert!(legacy_restore.consumed);
        let legacy_text = legacy.read_view();

        let stored = fixture();
        stored.write_view(&downgraded);
        stored.import();
        stored.state_before(before);
        let stored_restore = crate::workflow::drive::restore_ready_rows_after_phase(&stored.record);
        assert_eq!(stored_restore.restored, vec!["0030".to_string()]);
        assert!(stored_restore.consumed);
        assert_eq!(stored.read_view(), legacy_text);
        assert_eq!(stored.store().rows[0].status, READY);
        assert_eq!(
            stored.store().rows[1].status,
            "**Proposed** — reasoned hold"
        );
    }

    #[test]
    fn writers_fall_back_to_markdown_without_a_store() {
        let fx = fixture();
        let body = registry(&[row("0020", PROPOSED, "fixture")]);
        fx.write_view(&body);
        fx.track("0020-T", "# 0020 — real plan\n");
        assert!(
            crate::workflow::conductor_md::promote_placeholder(&fx.record, "0020", READY).unwrap()
        );
        let (_, expected) = crate::workflow::conductor_md::promote_placeholders_in(
            &body,
            &[("0020".to_string(), READY.to_string())],
        );
        assert_eq!(fx.read_view(), expected.unwrap());
        assert!(!store_path_at(&fx.dir()).exists());

        // A store is created, then deleted: the project is back on markdown.
        fx.write_view(&registry(&[row("0002", READY, "x")]));
        fx.track("0002-T", "# 0002 — plan\n");
        fx.import();
        std::fs::remove_file(store_path_at(&fx.dir())).unwrap();
        let picked = crate::workflow::conductor_md::pick_next_ready_excluding_sticky(
            &fx.record,
            &[],
            None,
            &[],
        );
        assert_eq!(picked.unwrap(), "0002");
    }

    #[test]
    fn status_mutation_preserves_nostart_marker_and_reparses() {
        let fx = fixture();
        fx.write_view(&registry(&[row(
            "0115",
            "**Proposed - placeholder** <!-- nostart -->",
            "x",
        )]));
        fx.import();
        set_statuses(
            &fx.record,
            &[("0115".into(), READY.into())],
            "operator",
            false,
        )
        .unwrap();
        let store = fx.store();
        assert_eq!(store.rows[0].status, READY);
        assert!(store.rows[0].nostart);
        assert!(store.rows[0].line.contains("<!-- nostart -->"));
        let parsed = parse_conductor_md(&render(&store)).unwrap();
        assert!(parsed[0].nostart);
        assert_eq!(parsed[0].status_raw, READY);
        assert_eq!(verify_at(&fx.dir()).unwrap().status, "ok");
    }

    #[test]
    fn promote_and_restore_duplicate_and_short_id_parity() {
        // Promote updates the first duplicate only.
        let fx = fixture();
        let body = registry(&[
            row("0040", PROPOSED, "first"),
            row("0040", PROPOSED, "second"),
        ]);
        fx.write_view(&body);
        fx.track("0040-T", "# 0040 — real plan\n");
        fx.import();
        let outcome = promote_placeholders(
            &fx.record,
            &[("0040".to_string(), READY.to_string())],
            "fold",
        )
        .unwrap();
        assert_eq!(outcome, WriteOutcome::Changed(vec!["0040".to_string()]));
        let store = fx.store();
        assert_eq!(store.rows[0].status, READY);
        assert_eq!(store.rows[1].status, PROPOSED);

        // Restore updates every duplicate.
        let fx = fixture();
        let original = registry(&[row("0040", READY, "first"), row("0040", READY, "second")]);
        fx.write_view(&registry(&[
            row("0040", "**Proposed**", "first"),
            row("0040", "**Proposed**", "second"),
        ]));
        fx.import();
        fx.state_before(crate::workflow::conductor_md::status_snapshot(&original));
        let restored = crate::workflow::drive::restore_ready_rows_after_phase(&fx.record);
        assert_eq!(restored.restored, vec!["0040".to_string()]);
        let store = fx.store();
        assert_eq!(store.rows[0].status, READY);
        assert_eq!(store.rows[1].status, READY);

        // A short `374` key targets a `T374` row.
        let fx = fixture();
        fx.write_view(&format!("{HEAD}| T374 | `.` | {READY} | short |\n"));
        fx.import();
        set_statuses(
            &fx.record,
            &[("374".into(), "**Completed**".into())],
            "operator",
            false,
        )
        .unwrap();
        assert_eq!(fx.store().rows[0].status, "**Completed**");
    }

    #[test]
    fn restore_refuses_on_midphase_view_edit_and_does_not_consume() {
        let fx = fixture();
        let original = registry(&[row("0030", READY, "a")]);
        fx.write_view(&registry(&[row("0030", "**Proposed**", "a")]));
        fx.import();
        fx.state_before(crate::workflow::conductor_md::status_snapshot(&original));
        // A human edits the view after the snapshot was taken.
        fx.write_view(&fx.read_view().replace("| a |", "| a hand edit |"));
        let restore = crate::workflow::drive::restore_ready_rows_after_phase(&fx.record);
        assert!(restore.restored.is_empty());
        assert!(!restore.consumed);
        assert!(fx.status_log().contains("registry-view-drift"));
        assert_eq!(fx.store().rows[0].status, "**Proposed**");
    }

    #[test]
    fn writer_refuses_when_replace_cell_is_none() {
        let fx = single(READY, "x");
        fx.import();
        // Corrupt the stored line: no status cell left.
        fx.edit_store(|s| s.rows[0].line = "| broken |".into());
        let store_before = std::fs::read(store_path_at(&fx.dir())).unwrap();
        let view_before = fx.read_view();
        let err = set_statuses(
            &fx.record,
            &[("0001".into(), "**Completed**".into())],
            "operator",
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("missing"), "{err}");
        assert_eq!(
            std::fs::read(store_path_at(&fx.dir())).unwrap(),
            store_before
        );
        assert_eq!(fx.read_view(), view_before);
    }

    #[test]
    fn backstop_maps_store_drift_to_skip() {
        use crate::workflow::placeholder_backstop::{BackstopOutcome, BackstopSkip, tick};
        let _lock = crate::config::test_env_lock();
        let _env = crate::config::TestEnvGuard::capture(&[crate::config::ENV_COORDINATOR_HOME]);
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        crate::features::reset_feature_warnings_for_tests();
        crate::workflow::placeholder_backstop::reset_backstop_warnings_for_tests();

        let fx = fixture();
        fx.write_view(&registry(&[row("0020", PROPOSED, "fixture")]));
        fx.track("0020-T", "# 0020 — real plan\n");
        fx.import();

        fx.write_view(&fx.read_view().replace("| fixture |", "| drift |"));
        assert_eq!(
            tick(&fx.record),
            BackstopOutcome::Skipped(BackstopSkip::DriftRefused)
        );
        assert!(fx.status_log().contains("registry-view-drift"));

        std::fs::write(store_path_at(&fx.dir()), b"{not json").unwrap();
        assert_eq!(
            tick(&fx.record),
            BackstopOutcome::Skipped(BackstopSkip::Unreadable)
        );

        // Rebuild a clean store/view, then fail the view write.
        let fx = fixture();
        fx.write_view(&registry(&[row("0021", PROPOSED, "fixture")]));
        fx.track("0021-T", "# 0021 — real plan\n");
        fx.import();
        force_view_write_failure(true);
        assert_eq!(
            tick(&fx.record),
            BackstopOutcome::Skipped(BackstopSkip::WriteFailed)
        );
        assert!(!fx.status_log().contains("placeholder-promoted"));
    }

    #[test]
    fn legacy_writer_holds_registry_lock() {
        let _seams = reset_seams();
        let fx = fixture();
        fx.write_view(&registry(&[row("0001", READY, "a")]));
        let dir = fx.dir();
        let view = fx.view();
        let holder_dir = dir.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            with_registry_lock(&holder_dir, || {
                tx.send(()).unwrap();
                let body = std::fs::read_to_string(&view).unwrap();
                std::fs::write(&view, format!("{body}{}", row("0020", PROPOSED, "p"))).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(150));
                Ok(())
            })
            .unwrap();
        });
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("holder signalled");
        let report = import_at(&dir, ImportOpts::default()).unwrap();
        holder.join().unwrap();
        assert_eq!(report.status, "ok");
        assert!(
            load_at(&dir)
                .unwrap()
                .unwrap()
                .rows
                .iter()
                .any(|r| r.id == "0020")
        );
        // After the store exists, a promotion is store-native, not a markdown write.
        fx.track("0020-T", "# 0020 — real plan\n");
        assert!(
            crate::workflow::conductor_md::promote_placeholder(&fx.record, "0020", READY).unwrap()
        );
        let store = load_at(&dir).unwrap().unwrap();
        assert_eq!(store.rows[1].status_source.as_deref(), Some("fold"));
        assert_eq!(render(&store), fx.read_view());
    }

    #[test]
    fn set_status_clear_nostart_removes_marker() {
        let fx = fixture();
        fx.write_view(&registry(&[row(
            "0115",
            "**Proposed - placeholder** <!-- nostart -->",
            "x",
        )]));
        fx.import();
        set_statuses(
            &fx.record,
            &[("0115".into(), "**Completed**".into())],
            "operator",
            true,
        )
        .unwrap();
        let store = fx.store();
        assert_eq!(store.rows[0].status, "**Completed**");
        assert!(!store.rows[0].nostart);
        assert!(!store.rows[0].line.contains("<!-- nostart -->"));
        let parsed = parse_conductor_md(&render(&store)).unwrap();
        assert!(!parsed[0].nostart);
    }

    // -- Phase 4: repair helpers -------------------------------------------

    #[test]
    fn repair_bytes_preserve_parsed_summary_text() {
        let repaired_0102 = "`default\\|config\\|env`";
        let repaired_0115 = "/\\b(\\d{4})(?=-\\w\\|\\s+[A-Za-z]\\|$)/";
        assert_eq!(repaired_0102.matches("\\|").count(), 2);
        assert_eq!(repaired_0115.matches("\\|").count(), 2);
        assert_eq!(
            format!("{repaired_0102}{repaired_0115}")
                .matches("\\|")
                .count(),
            4
        );
        assert!(!repaired_0102.contains("\\\\|"));
        assert!(!repaired_0115.contains("\\\\|"));
        // The parsed cell text is exactly today's plain-pipe text.
        let line = format!("| [0102-X](0102-X/spec.md) | `.` | **Completed** | {repaired_0102} |");
        let cells = split_unescaped_pipes(&line);
        assert_eq!(cells.len(), 4);
        assert_eq!(cells[3].trim(), "`default|config|env`");
        let line = format!("| [0115-X](0115-X/spec.md) | `.` | **Proposed** | {repaired_0115} |");
        let cells = split_unescaped_pipes(&line);
        assert_eq!(cells.len(), 4);
        assert_eq!(cells[3].trim(), "/\\b(\\d{4})(?=-\\w|\\s+[A-Za-z]|$)/");
    }

    #[test]
    fn repair_visibility_consumer_matrix() {
        let broken = format!(
            "{HEAD}\
             | [0102-X](0102-X/spec.md) | `.` | **Completed** | `default|config|env` |\n\
             | [0115-X](0115-X/spec.md) | `.` | **Proposed** — placeholder <!-- nostart --> | /\\b(\\d{{4}})(?=-\\w|\\s+[A-Za-z]|$)/ |\n\
             | [0116-Y](0116-Y/spec.md) | `.` | **Proposed** | clean |\n"
        );
        let repaired = format!(
            "{HEAD}\
             | [0102-X](0102-X/spec.md) | `.` | **Completed** | `default\\|config\\|env` |\n\
             | [0115-X](0115-X/spec.md) | `.` | **Proposed** — placeholder <!-- nostart --> | /\\b(\\d{{4}})(?=-\\w\\|\\s+[A-Za-z]\\|$)/ |\n\
             | [0116-Y](0116-Y/spec.md) | `.` | **Proposed** | clean |\n"
        );
        let broken_dir = fixture();
        broken_dir.write_view(&broken);
        let broken_rows =
            crate::workflow::conductor_md::load_track_rows(&broken_dir.record).unwrap();
        assert_eq!(broken_rows.len(), 1, "{broken_rows:?}");
        assert!(!crate::workflow::conductor_md::track_row_terminal(
            &broken_rows,
            "0102"
        ));
        assert_eq!(
            crate::workflow::conductor_md::pick_next_ready_excluding_sticky(
                &broken_dir.record,
                &[],
                None,
                &[]
            )
            .unwrap_err()
            .to_string(),
            "no Ready — not started track in conductor.md; pass --track <id>"
        );

        let repaired_dir = fixture();
        repaired_dir.write_view(&repaired);
        repaired_dir.track("0116-Y", "# 0116 — plan\n");
        repaired_dir.track("0102-X", "# 0102 — plan\n");
        let rows = crate::workflow::conductor_md::load_track_rows(&repaired_dir.record).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(crate::workflow::conductor_md::track_row_terminal(
            &rows, "0102"
        ));
        let row = rows.iter().find(|r| r.id == "0115").unwrap();
        assert!(row.nostart);
        assert_eq!(row.summary, "/\\b(\\d{4})(?=-\\w|\\s+[A-Za-z]|$)/");
        // The pick is unchanged: no row became Ready.
        assert!(
            crate::workflow::conductor_md::pick_next_ready_excluding_sticky(
                &repaired_dir.record,
                &[],
                None,
                &[]
            )
            .is_err()
        );
        // The backstop excludes the nostart row.
        assert!(!crate::workflow::conductor_md::track_row_nostart(
            &repaired_dir.record,
            "0116"
        ));
        assert!(crate::workflow::conductor_md::track_row_nostart(
            &repaired_dir.record,
            "0115"
        ));
    }

    #[test]
    fn view_drift_error_names_both_recoveries() {
        let dir = std::path::Path::new(r"C:\dev\example\conductor");
        let err = view_drift_error(dir, "storeabc", "viewdef");
        let msg = err.to_string();
        assert!(msg.starts_with(VIEW_DRIFT_PREFIX), "{msg}");
        assert!(msg.contains("(store storeabc, view viewdef)"), "{msg}");
        assert!(
            msg.contains(
                "coordinator registry import --conductor-dir C:\\dev\\example\\conductor --force"
            ),
            "{msg}"
        );
        assert!(
            msg.contains(
                "coordinator registry render --conductor-dir C:\\dev\\example\\conductor --force"
            ),
            "{msg}"
        );
        assert!(is_view_drift_error(&err), "{msg}");
    }

    #[test]
    #[ignore]
    fn registry_live_reference_dry_run() {
        let path = std::env::var("COORDINATOR_HUB_REGISTRY")
            .unwrap_or_else(|_| r"C:\dev\coordinated\conductor".into());
        let dir = std::path::Path::new(&path);
        if !dir.join("conductor.md").is_file() {
            eprintln!("skipping: no reference registry at {path}");
            return;
        }
        let report = import_at(
            dir,
            ImportOpts {
                dry_run: true,
                force: false,
            },
        )
        .unwrap();
        assert_eq!(report.status, "refused");
        assert_eq!(report.rejects.len(), 77, "{}", report.rejects.len());
        assert_eq!(report.reject_classes.cell_count, 12);
        assert_eq!(report.reject_classes.struck_through, 34);
        assert_eq!(report.reject_classes.alpha_prefixed, 31);
        assert_eq!(report.interior_gaps.len(), 8);
        assert!(!store_path_at(dir).exists());
    }
}
