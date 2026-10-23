//! Parse `conductor/conductor.md` and pick the first exact Ready track (0030).

use crate::error::{CoordinatorError, Result};
use crate::notify::artifact::{numeric_track_id, track_ids_match};
use crate::outcome::FailureClass;
use crate::registry::{AutoStartPolicy, ProjectRecord};
use crate::state::{RunState, RunStatus, StatusView};
use serde::{Deserialize, Serialize};

use super::LAST_EVENT_BACKLOG_CLEAR;
use super::graph::resolve_track_dir;
use super::shipped::MergedTrackProbe;

const READY_NORMALIZED: &str = "Ready - not started";

/// Longest-first; shared by [`unwrap_emphasis`] and [`status_token_matches`].
const EMPHASIS_MARKERS: [&str; 6] = ["**", "__", "~~", "`", "*", "_"];

/// Default conveyor phrase (em dash). Empty `ProjectRecord.ready_aliases` uses this only.
pub const DEFAULT_READY_PHRASE: &str = "Ready — not started";

/// Default alias list for omit-`--track` when the project has not opted in extras.
pub fn default_ready_aliases() -> &'static [&'static str] {
    &[DEFAULT_READY_PHRASE]
}

/// Effective aliases: default phrase plus stored extras (dedup after `status_clean`).
pub fn ready_aliases_for(record: &ProjectRecord) -> Vec<String> {
    let mut out: Vec<String> = default_ready_aliases()
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    extend_ready_aliases(&mut out, &record.ready_aliases);
    out
}

/// Append operator-typed aliases; skip empty and `status_clean` duplicates.
pub fn extend_ready_aliases(existing: &mut Vec<String>, incoming: &[String]) {
    for alias in incoming {
        let trimmed = alias.trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = status_clean(trimmed);
        if key == READY_NORMALIZED {
            continue;
        }
        if existing.iter().any(|e| status_clean(e) == key) {
            continue;
        }
        existing.push(trimmed.to_string());
    }
}

/// Fields shared by `cmd_run` pick and the Status Surface card label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyPickState {
    pub track_id: Option<String>,
    pub status: RunStatus,
    pub next_track: Option<String>,
    pub failure_class: Option<FailureClass>,
    pub last_event: String,
    pub auto_start: AutoStartPolicy,
}

/// Defaults `auto_start` to Hitl — `RunState` has no policy field.
/// Callers that enforce `never` must overlay `record.auto_start`
/// (`resolve_run_track` does). Prefer `From<&StatusView>` when the view
/// already copied the record.
impl From<&RunState> for ReadyPickState {
    fn from(s: &RunState) -> Self {
        Self {
            track_id: s.track_id.clone(),
            status: s.status,
            next_track: s.next_track.clone(),
            failure_class: s.failure_class,
            last_event: s.last_event.clone(),
            auto_start: AutoStartPolicy::Hitl,
        }
    }
}

impl From<&StatusView> for ReadyPickState {
    fn from(v: &StatusView) -> Self {
        Self {
            track_id: v.track_id.clone(),
            status: v.status,
            next_track: v.next_track.clone(),
            failure_class: v.failure_class,
            last_event: v.last_event.clone(),
            auto_start: v.auto_start,
        }
    }
}

/// True when omit-`--track` should pick the next exact Ready row.
///
/// Running/Paused: never pick (`run` stays `InvalidTransition`). Else unset/empty
/// `track_id`, **or** Idle + backlog-clear + no failure + no `next_track`.
/// Do not special-case `invalid next_track` last_event text.
pub fn should_pick_next_ready(s: &ReadyPickState) -> bool {
    if s.auto_start == AutoStartPolicy::Never {
        return false;
    }
    if matches!(s.status, RunStatus::Running | RunStatus::Paused) {
        return false;
    }
    let track_empty = s
        .track_id
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .is_none();
    if track_empty {
        return true;
    }
    s.status == RunStatus::Idle
        && s.next_track.is_none()
        && s.failure_class.is_none()
        && s.last_event == LAST_EVENT_BACKLOG_CLEAR
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackRow {
    pub id: String,
    pub slug: String,
    pub status_raw: String,
    pub summary: String,
    pub nostart: bool,
    /// Raw execution cell (`Execution`, `Execution path`, `Execution repo`).
    pub execution: Option<String>,
}

/// Trim → unwrap wrapping emphasis → strip leading emoji/punct (stop at alnum or
/// emphasis markers) until stable, then fold the dash and space families.
///
/// This is the **comparison** seam only: the registry text is never rewritten.
/// Eligible iff the result equals an alias after cleaning (`Ready - not started`
/// for the default alias). The dash set is a compatibility set, not the whole
/// Unicode `Dash` property (0115).
pub fn status_clean(raw: &str) -> String {
    let mut cur = raw.to_string();
    loop {
        let trimmed = cur.trim();
        let unwrapped = unwrap_emphasis(trimmed).unwrap_or(trimmed);
        let stripped = strip_leading_non_alnum(unwrapped);
        if stripped == cur {
            break;
        }
        cur = stripped.to_string();
    }
    collapse_unicode_space(&fold_dash_family(&cur))
}

/// Named supported dash set: `U+002D`, `U+2010`–`U+2015`, `U+2212`, `U+FE58`,
/// `U+FE63`, `U+FF0D` → `-`.
///
/// A compatibility set, not the Unicode `Dash` property. `NFKC` is deliberately
/// not used: it folds compatibility characters generally and does not fold
/// `Pd`/`U+2212` consistently (0115, spec §8).
fn fold_dash_family(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' | '\u{FE58}' | '\u{FE63}' | '\u{FF0D}' => '-',
            other => other,
        })
        .collect()
}

/// Collapse every run of Unicode `White_Space` (`char::is_whitespace`) to one
/// `U+0020`. Does not trim: the caller already did.
///
/// The 25-code-point property includes tabs, VT, FF, a lone CR, NEL, `Zs`, LS
/// and PS. LF cannot occur inside a `str::lines()` element and CRLF's CR is
/// stripped, so those two are the only exclusions and they are structural.
fn collapse_unicode_space(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !in_space {
                out.push(' ');
                in_space = true;
            }
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

fn unwrap_emphasis(s: &str) -> Option<&str> {
    for m in EMPHASIS_MARKERS {
        if s.len() >= m.len() * 2 && s.starts_with(m) && s.ends_with(m) {
            return Some(&s[m.len()..s.len() - m.len()]);
        }
    }
    None
}

/// Leading emoji / checkmark / punctuation, but stop before `*_`~` so the next
/// loop iteration can unwrap wrapping bold (agy m1: emoji outside **and** inside).
fn strip_leading_non_alnum(s: &str) -> &str {
    s.trim_start_matches(|c: char| {
        !c.is_ascii_alphanumeric() && !matches!(c, '*' | '_' | '`' | '~')
    })
}

pub fn is_eligible_ready(status_raw: &str) -> bool {
    status_clean(status_raw) == READY_NORMALIZED
}

/// Exact `status_clean` equality against any alias (no prefix).
pub fn is_eligible_ready_in<S: AsRef<str>>(status_raw: &str, aliases: &[S]) -> bool {
    let cleaned = status_clean(status_raw);
    aliases.iter().any(|a| status_clean(a.as_ref()) == cleaned)
}

/// House-style status token on a `status_clean` string (0062).
///
/// Peels at most one leading emphasis marker, requires the full `phrase`
/// (case-sensitive; `"In progress"` keeps its internal space), peels the
/// matching closer, then accepts end-of-string or a separator
/// (whitespace / `-` / `(` / `:` / `,`). Not a substring match.
fn status_token_matches(cleaned: &str, phrase: &str) -> bool {
    let mut s = cleaned;
    let mut opener: Option<&str> = None;
    for m in EMPHASIS_MARKERS {
        if let Some(rest) = s.strip_prefix(m) {
            opener = Some(m);
            s = rest;
            break;
        }
    }
    let Some(after) = s.strip_prefix(phrase) else {
        return false;
    };
    let after = match opener {
        Some(m) => after.strip_prefix(m).unwrap_or(after),
        None => after,
    };
    after.is_empty()
        || after
            .starts_with(|c: char| c.is_ascii_whitespace() || matches!(c, '-' | '(' | ':' | ','))
}

/// Registry `**Completed**` or house-style `**Completed** <detail>` (0062).
pub fn is_completed(status_raw: &str) -> bool {
    status_token_matches(&status_clean(status_raw), "Completed")
}

/// Registry `**In progress**` or `**In progress** <detail>` (0055 sticky-exempt).
pub fn is_in_progress(status_raw: &str) -> bool {
    status_token_matches(&status_clean(status_raw), "In progress")
}

/// Registry `**Absorbed**` or `**Absorbed** <detail>` (0063 retain refusal).
pub fn is_absorbed(status_raw: &str) -> bool {
    status_token_matches(&status_clean(status_raw), "Absorbed")
}

/// Registry `**Cancelled**` or `**Cancelled** <detail>` (0063 retain refusal).
pub fn is_cancelled(status_raw: &str) -> bool {
    status_token_matches(&status_clean(status_raw), "Cancelled")
}

/// Vocabulary `Proposed`, including house-style trailing detail (0074).
pub(crate) fn is_proposed(status_raw: &str) -> bool {
    status_token_matches(&status_clean(status_raw), "Proposed")
}

/// Vocabulary `Blocked`, including house-style trailing detail (0074).
pub(crate) fn is_blocked(status_raw: &str) -> bool {
    status_token_matches(&status_clean(status_raw), "Blocked")
}

/// Completed, Absorbed, or Cancelled. Does not change Ready aliases or the sticky overlay.
pub fn is_terminal(status_raw: &str) -> bool {
    is_completed(status_raw) || is_absorbed(status_raw) || is_cancelled(status_raw)
}

/// Canonical terminal token for a row status; `None` when the row is not terminal.
///
/// The 0120 guard uses one token for the receipt and the error text, so a house-style
/// `**Completed** <detail>` row never puts spaces inside a `key=value` field.
pub(crate) fn terminal_token(status_raw: &str) -> Option<&'static str> {
    let clean = status_clean(status_raw);
    ["Completed", "Absorbed", "Cancelled"]
        .into_iter()
        .find(|token| status_token_matches(&clean, token))
}

/// Canonical vocabulary token for a non-terminal row, used by the 0120 ambiguity message.
///
/// Terminal rows answer [`terminal_token`]; a house-style `**Blocked** - prerequisite` cell
/// answers `Blocked`, and an unrecognised cell falls back to its cleaned text.
pub(crate) fn status_token(status_raw: &str) -> String {
    if let Some(token) = terminal_token(status_raw) {
        return token.to_string();
    }
    let clean = status_clean(status_raw);
    for phrase in ["In progress", "Proposed", "Blocked"] {
        if status_token_matches(&clean, phrase) {
            return phrase.to_string();
        }
    }
    clean
}

/// A downgrade that carries a stated reason is a decision, not lost bookkeeping.
///
/// `Proposed` and `Blocked` are written two ways. Bare - as a marker that the next tick should
/// pick the row up again (0089: a plan left a row `Proposed` so it would be re-planned). Or with a
/// reason, when the phase is deliberately not advancing it (0098: "Proposed - spec section 7
/// prerequisites remain open"). Replaying the bare marker back to `Ready` is the intended safety
/// net; replaying a reasoned one overrides the decision the phase just made, and the next tick
/// starts work the plan says must not start. Keep this narrow: it only speaks about rows whose
/// status names `Proposed` or `Blocked`.
pub fn is_reasoned_downgrade(status_raw: &str) -> bool {
    let lower = status_raw.to_ascii_lowercase();
    let Some((word, at)) = ["proposed", "blocked"]
        .iter()
        .filter_map(|w| lower.find(w).map(|p| (*w, p)))
        .min_by_key(|(_, p)| *p)
    else {
        return false;
    };
    lower[at + word.len()..]
        .trim_matches(|c: char| c.is_whitespace() || c == '*' || c == '`')
        .trim_start_matches(['-', ':', '=', '.'])
        .chars()
        .any(|c| c.is_ascii_alphanumeric())
}

/// Best-effort `{conductor_dir}/conductor.md` rows. Missing / unreadable → `None`.
///
/// 0109: one seam. A present store is authoritative (`Some(rows)`); a drifted view is
/// reported once and still answers from the store; an absent store falls back to the
/// shipped markdown parse; a broken store never falls back to markdown.
pub fn load_track_rows(record: &ProjectRecord) -> Option<Vec<TrackRow>> {
    match crate::registry_store::snapshot(record) {
        Ok(snapshot) => {
            if let crate::registry_store::StoreView::Drifted { expected, actual } = &snapshot.view {
                crate::registry_store::report_drift_once(record, expected, actual);
            }
            if snapshot.view == crate::registry_store::StoreView::Absent {
                legacy_load_track_rows(record)
            } else {
                Some(snapshot.rows)
            }
        }
        Err(e) => {
            crate::registry_store::report_store_unreadable_once(record, &e);
            None
        }
    }
}

fn legacy_load_track_rows(record: &ProjectRecord) -> Option<Vec<TrackRow>> {
    let path = conductor_md_path(record);
    if !path.is_file() {
        return None;
    }
    let text = read_view_for_parse(&path).ok()?;
    parse_conductor_md(&text).ok()
}

#[cfg(test)]
thread_local! {
    static VIEW_PARSE_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The one helper for every production **parse** read of the generated view.
///
/// `#[cfg(test)]` counts calls so AC-5.3 can prove the decision legs read the store
/// (counter zero in store mode) instead of grepping for `read_to_string`. The counter is
/// thread-local so concurrent tests cannot pollute each other's proof.
pub(crate) fn read_view_for_parse(path: &std::path::Path) -> std::io::Result<String> {
    #[cfg(test)]
    VIEW_PARSE_READS.with(|c| c.set(c.get() + 1));
    std::fs::read_to_string(path)
}

#[cfg(test)]
pub(crate) fn view_parse_reads() -> usize {
    VIEW_PARSE_READS.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(crate) fn reset_view_parse_reads() {
    VIEW_PARSE_READS.with(|c| c.set(0));
}

pub(crate) fn row_matches_track(row: &TrackRow, key: &str) -> bool {
    let slug = if row.slug.is_empty() {
        row.id.clone()
    } else {
        format!("{}-{}", row.id, row.slug)
    };
    crate::notify::artifact::track_ids_match(&row.id, key)
        || crate::notify::artifact::track_ids_match(&slug, key)
}

/// True when a row matching `key` is `Proposed` (not yet Ready, not Blocked).
///
/// A `Proposed` row whose spec/plan already exist must still run the planning
/// skill. Those files may be a deliberate hold-boundary record that predates the
/// track's "Before Ready" prerequisites, so a fast-skip decided on file presence
/// alone leaves the required plan permanently un-writable and stalls every fold.
pub(crate) fn track_row_proposed(record: &ProjectRecord, track_id: Option<&str>) -> bool {
    let Some(key) = track_id.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    load_track_rows(record).is_some_and(|rows| {
        rows.iter()
            .any(|row| row_matches_track(row, key) && is_proposed(&row.status_raw))
    })
}

/// True when a row matching `key` has status Completed.
pub fn track_row_completed(rows: &[TrackRow], key: &str) -> bool {
    rows.iter()
        .any(|r| row_matches_track(r, key) && is_completed(&r.status_raw))
}

/// True when a row matching `key` is Completed, Absorbed, or Cancelled.
pub fn track_row_terminal(rows: &[TrackRow], key: &str) -> bool {
    rows.iter()
        .any(|r| row_matches_track(r, key) && is_terminal(&r.status_raw))
}

/// Read-time SUPERSEDED reason (0040). Missing conductor.md skips the Completed signal.
pub fn failure_superseded(
    record: &ProjectRecord,
    state: &RunState,
    body: Option<&str>,
) -> Option<String> {
    let meta = body.map(crate::notify::artifact::parse_metadata);
    // 0098: a Blocked failure is not superseded by a Completed conductor row. Only
    // `failure resolve` releases the hold.
    if state.failure_class == Some(crate::outcome::FailureClass::Blocked)
        || meta
            .as_ref()
            .and_then(|m| m.failure_class.as_deref())
            .is_some_and(|class| class == "blocked")
    {
        return None;
    }
    let key = meta
        .as_ref()
        .and_then(|m| m.track_id.as_deref())
        .or(state.track_id.as_deref());
    let completed = key
        .and_then(|k| {
            let rows = load_track_rows(record)?;
            Some(track_row_completed(&rows, k))
        })
        .unwrap_or(false);
    crate::notify::artifact::compute_superseded(
        body,
        state.track_id.as_deref(),
        state.run_epoch,
        completed,
    )
}

/// Skip even exact Ready when id, slug, cleaned status, or summary matches HITL.
pub fn is_hitl_marked(id: &str, slug: &str, status: &str, summary: &str) -> bool {
    hitl_field(id, slug, status, summary).is_some()
}

/// The first field (in `id`, `slug`, `status`, `summary` order) that carries the
/// HITL marker. Exposed so the 0115 report can name the cause (`detail:
/// "summary"` for Printer) without re-implementing the gate.
pub(crate) fn hitl_field(
    id: &str,
    slug: &str,
    status: &str,
    summary: &str,
) -> Option<&'static str> {
    [
        ("id", id),
        ("slug", slug),
        ("status", status),
        ("summary", summary),
    ]
    .into_iter()
    .find_map(|(name, value)| field_is_hitl(value).then_some(name))
}

fn field_is_hitl(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.contains("hitl") || contains_owner_only(&lower)
}

/// `(?i)owner[\s-]?only`
fn contains_owner_only(lower: &str) -> bool {
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i + 5 <= bytes.len() {
        if &bytes[i..i + 5] == b"owner" {
            let rest = &bytes[i + 5..];
            if rest.starts_with(b"only") {
                return true;
            }
            if let Some((&c, tail)) = rest.split_first()
                && (c == b'-' || c.is_ascii_whitespace())
                && tail.starts_with(b"only")
            {
                return true;
            }
        }
        i += 1;
    }
    false
}

const NOSTART_LITERAL: &str = "<!-- nostart -->";

/// Strip the exact `<!-- nostart -->` literal. No general HTML-comment parser.
///
/// **The marker only counts when it terminates a cell** — that is, when the next
/// non-space character after it is a `|` or the end of the line (0088). A line that
/// merely *mentions* the literal in prose — a registry summary describing this very
/// marker, or quoting a row that carries it — must not gate its own row forever.
/// Observed live 2026-10-07: 0088's own summary contained the literal twice (once as
/// prose describing the defect, once as the real marker), and 0087's summary quoted it
/// as `\`Proposed <!-- nostart -->\`` inside a code span.
///
/// All terminated occurrences are removed; prose mentions are preserved verbatim and
/// do not set the flag.
pub fn strip_nostart_literal(line: &str) -> (String, bool) {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    let mut active = false;
    while let Some(pos) = rest.find(NOSTART_LITERAL) {
        let after = &rest[pos + NOSTART_LITERAL.len()..];
        let tail = after.trim_start();
        let terminates = tail.is_empty() || tail.starts_with('|');
        out.push_str(&rest[..pos]);
        if terminates {
            active = true;
        } else {
            out.push_str(NOSTART_LITERAL);
        }
        rest = after;
    }
    out.push_str(rest);
    (out, active)
}

/// True when the matching conductor row carries `<!-- nostart -->`.
pub fn track_row_nostart(record: &ProjectRecord, id: &str) -> bool {
    load_track_rows(record)
        .and_then(|rows| rows.into_iter().find(|r| row_matches_track(r, id)))
        .map(|r| r.nostart)
        .unwrap_or(false)
}

#[derive(Debug, Clone)]
struct PipeRow {
    cells: Vec<String>,
    nostart: bool,
}

/// Path to this project's `conductor.md`.
pub(crate) fn conductor_md_path(record: &ProjectRecord) -> std::path::PathBuf {
    crate::layout::resolve(record)
        .conductor_dir
        .join("conductor.md")
}

/// One row whose cell count differs from its table header's (0088 DoD-3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WidthDeviation {
    /// 1-based line number in `conductor.md`.
    pub line: usize,
    /// Cell count found on the row.
    pub got: usize,
    /// Cell count of the owning table's header.
    pub want: usize,
    /// Leading text of the row, for the report.
    pub preview: String,
}

/// Rows that `parse_conductor_md` will **skip silently** because their width differs
/// from the header's (0088 DoD-3).
///
/// The parse contract drops such rows, which is how a raw `|` written into a summary
/// erased 0474's Completed row and 0111's row on the HelpMeMove registry. A deviation
/// is now *reportable* rather than invisible. Read-only; never writes.
pub fn width_deviations(text: &str) -> Vec<WidthDeviation> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut out = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        if !is_table_start(&lines, i) {
            i += 1;
            continue;
        }
        let (header_line, _) = strip_nostart_literal(lines[i]);
        let want = split_unescaped_pipes(&header_line).len();
        let header_line_no = i + 1;
        i += 2;
        while i < lines.len() {
            if lines[i].trim().is_empty() || is_thematic_break(lines[i]) {
                i += 1;
                continue;
            }
            if !looks_like_row(lines[i]) || is_table_start(&lines, i) {
                break;
            }
            let (stripped, _) = strip_nostart_literal(lines[i]);
            let got = split_unescaped_pipes(&stripped).len();
            if got != want {
                let preview: String = lines[i].trim().chars().take(96).collect();
                out.push(WidthDeviation {
                    line: i + 1,
                    got,
                    want,
                    preview,
                });
            }
            let _ = header_line_no;
            i += 1;
        }
    }
    out
}

/// Replace the content of the `idx`-th cell (0-based, counting only unescaped `|`
/// separators) in a single table row line. Returns `None` when the row has too few
/// cells. Leading/trailing single spaces around the new content are normalized.
pub(crate) fn replace_cell(line: &str, idx: usize, content: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut boundaries: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'|' {
            i += 2;
            continue;
        }
        if bytes[i] == b'|' {
            boundaries.push(i);
        }
        i += 1;
    }
    // Cell `idx` lives between boundaries[idx] and boundaries[idx + 1].
    if idx + 1 >= boundaries.len() {
        return None;
    }
    let start = boundaries[idx];
    let end = boundaries[idx + 1];
    let mut out = String::with_capacity(line.len() + content.len());
    out.push_str(&line[..=start]);
    out.push(' ');
    out.push_str(content);
    out.push(' ');
    out.push_str(&line[end..]);
    Some(out)
}

/// Snapshot every row's id → trimmed status cell, for later downgrade detection.
pub fn status_snapshot(text: &str) -> std::collections::BTreeMap<String, String> {
    match parse_conductor_md(text) {
        Ok(rows) => rows
            .into_iter()
            .map(|r| (r.id, r.status_raw.trim().to_string()))
            .collect(),
        Err(_) => std::collections::BTreeMap::new(),
    }
}

/// Snapshot for a record's live registry.
pub fn status_snapshot_for(record: &ProjectRecord) -> std::collections::BTreeMap<String, String> {
    match crate::registry_store::snapshot(record) {
        Ok(snapshot) if snapshot.view != crate::registry_store::StoreView::Absent => snapshot
            .rows
            .into_iter()
            .map(|r| (r.id, r.status_raw.trim().to_string()))
            .collect(),
        Ok(_) => read_view_for_parse(&conductor_md_path(record))
            .map(|t| status_snapshot(&t))
            .unwrap_or_default(),
        Err(_) => std::collections::BTreeMap::new(),
    }
}

/// Undo a non-terminal `Ready` downgrade an `advance`/`fold` session wrote into the
/// registry (0088 DoD-1, narrowed by 0090).
///
/// `before` is a [`status_snapshot`] taken **before** the phase ran. For every id that
/// was an eligible `Ready` row, is no longer one, and whose live cell is not
/// [`is_terminal`], the status cell is rewritten back to its previous text. A terminal
/// live cell is left as written. Returns the restored ids and the new file body (or
/// `None` when nothing changed, so the caller does not rewrite the file needlessly).
///
/// The caller parses once so a parse error stays distinct from nothing to restore.
/// This function parses again. That second parse is intentional. The return type stays
/// `(Vec<String>, Option<String>)`.
///
/// The banner is deliberately **not** consulted: the status cell is the state, and a
/// mint-time banner is not authority over it.
pub fn restore_ready_rows(
    text: &str,
    before: &std::collections::BTreeMap<String, String>,
    aliases: &[String],
) -> (Vec<String>, Option<String>) {
    let after = match parse_conductor_md(text) {
        Ok(rows) => rows,
        Err(_) => return (Vec::new(), None),
    };
    let mut downgraded: Vec<(String, String)> = Vec::new();
    for row in &after {
        let Some(prev) = before.get(&row.id) else {
            continue;
        };
        let was_ready = is_eligible_ready_in(prev, aliases);
        let now_ready = is_eligible_ready_in(&row.status_raw, aliases);
        if was_ready
            && !now_ready
            && !is_terminal(&row.status_raw)
            && !is_reasoned_downgrade(&row.status_raw)
        {
            downgraded.push((row.id.clone(), prev.clone()));
        }
    }
    if downgraded.is_empty() {
        return (Vec::new(), None);
    }
    let owned: Vec<String> = text.lines().map(str::to_string).collect();
    let mut lines: Vec<String> = owned.clone();
    let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
    let mut restored: Vec<String> = Vec::new();
    for (id, prev) in &downgraded {
        let mut i = 0;
        while i < lines.len() {
            if !is_table_start(&refs, i) {
                i += 1;
                continue;
            }
            let (header_line, _) = strip_nostart_literal(refs[i]);
            let headers: Vec<String> = split_unescaped_pipes(&header_line)
                .iter()
                .map(|c| c.trim().to_string())
                .collect();
            let status_col = headers.iter().position(|h| is_status_header(h));
            i += 2;
            while i < lines.len() {
                if lines[i].trim().is_empty() || is_thematic_break(&lines[i]) {
                    i += 1;
                    continue;
                }
                if !looks_like_row(&lines[i]) || is_table_start(&refs, i) {
                    break;
                }
                let (stripped, _) = strip_nostart_literal(&lines[i]);
                let cells = split_unescaped_pipes(&stripped);
                if let Some(sc) = status_col
                    && let Some(track_cell) = cells.first()
                    && let Some((rid, _)) = parse_leading_id(&unwrap_md_link(track_cell.trim()))
                    && track_ids_match(&rid, id)
                {
                    // The status cell index in the *original* line: header cell 0 is the
                    // text before the first pipe, so the cell index equals `status_col`.
                    // 0109: preserve a cell-terminating marker the row already carries.
                    if let Some(new_line) =
                        replace_status_cell_preserving_marker(&lines[i], sc, prev)
                    {
                        lines[i] = new_line;
                        if !restored.iter().any(|e| track_ids_match(e, id)) {
                            restored.push(id.clone());
                        }
                    }
                }
                i += 1;
            }
        }
    }
    if restored.is_empty() {
        return (Vec::new(), None);
    }
    let trailing_newline = text.ends_with('\n');
    let mut body = lines.join("\n");
    if trailing_newline {
        body.push('\n');
    }
    (restored, Some(body))
}

/// The status names the placeholder condition, e.g.
/// `Proposed - placeholder, needs full spec/plan pass`.
///
/// A deliberate `Proposed - gated on ...` hold does not match, so it keeps its withhold.
pub(crate) fn status_names_placeholder(status_raw: &str) -> bool {
    status_clean(status_raw)
        .to_ascii_lowercase()
        .contains("placeholder")
}

/// The placeholder condition is satisfied: the track has a real `plan.md`.
///
/// `plan_markdown_is_placeholder` already treats an empty file, an unfilled heading, the
/// `<!-- coordinator:placeholder-plan -->` comment, and the template blockquote as stubs,
/// so this reuses it rather than matching the marker string again.
fn placeholder_pass_complete(record: &ProjectRecord, key: &str) -> bool {
    let Some(dir) = crate::workflow::graph::resolve_track_dir(record, key) else {
        return false;
    };
    if !dir.join("spec.md").is_file() {
        return false;
    }
    match std::fs::read_to_string(dir.join("plan.md")) {
        Ok(body) => !crate::workflow::prompts::plan_markdown_is_placeholder(&body),
        Err(_) => false,
    }
}

/// Replace the content of the `status_col`-th cell, preserving a cell-terminating
/// `<!-- nostart -->` that the row already carried in that cell (0109 spec §3.2 rule 9).
///
/// Shared by the store mutation core and the legacy markdown writers so store mode and
/// markdown mode produce identical bytes for a marker-bearing row.
pub(crate) fn replace_status_cell_preserving_marker(
    line: &str,
    status_col: usize,
    content: &str,
) -> Option<String> {
    let (stripped, _) = strip_nostart_literal(content);
    let content = stripped.trim();
    let cells = split_unescaped_pipes(line);
    let keep_marker = cells
        .get(status_col)
        .map(|c| c.trim_end().ends_with(NOSTART_LITERAL))
        .unwrap_or(false);
    let cell = if keep_marker {
        format!("{content} {NOSTART_LITERAL}")
    } else {
        content.to_string()
    };
    replace_cell(line, status_col, &cell)
}

/// Set the Status cell of the row matching `key` to `ready`, returning the new registry.
///
/// Pure, mirroring [`restore_ready_rows`]: the caller writes the body. `None` when no row
/// matched, so a vanished row can never be reported as promoted. A status-cell
/// `<!-- nostart -->` is preserved (0109).
fn promote_placeholder_in(text: &str, key: &str, ready: &str) -> Option<String> {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let owned: Vec<String> = lines.clone();
    let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
    let mut i = 0;
    while i < lines.len() {
        if !is_table_start(&refs, i) {
            i += 1;
            continue;
        }
        let (header_line, _) = strip_nostart_literal(refs[i]);
        let headers: Vec<String> = split_unescaped_pipes(&header_line)
            .iter()
            .map(|c| c.trim().to_string())
            .collect();
        let status_col = headers.iter().position(|h| is_status_header(h));
        i += 2;
        while i < lines.len() {
            if lines[i].trim().is_empty() || is_thematic_break(&lines[i]) {
                i += 1;
                continue;
            }
            if !looks_like_row(&lines[i]) || is_table_start(&refs, i) {
                break;
            }
            let (stripped, _) = strip_nostart_literal(&lines[i]);
            let cells = split_unescaped_pipes(&stripped);
            let matched = status_col.is_some_and(|sc| {
                cells
                    .first()
                    .and_then(|c| parse_leading_id(&unwrap_md_link(c.trim())))
                    .is_some_and(|(rid, _)| track_ids_match(&rid, key))
                    .then_some(sc)
                    .is_some()
            });
            if matched
                && let Some(sc) = status_col
                && let Some(new_line) = replace_status_cell_preserving_marker(&lines[i], sc, ready)
            {
                let trailing_newline = text.ends_with('\n');
                lines[i] = new_line;
                let mut body = lines.join("\n");
                if trailing_newline {
                    body.push('\n');
                }
                return Some(body);
            }
            i += 1;
        }
    }
    None
}

/// The Ready cell this row should get when its placeholder condition is satisfied.
///
/// Shared by the fold gate (0106 refactor) and the no-run backstop. `None` when the row
/// is not a `Proposed` placeholder, is HITL/owner-only, or its spec/plan pass is
/// incomplete. The caller decides whether a row-local `<!-- nostart -->` applies: the
/// fold has already been explicitly started; the backstop has not.
pub(crate) fn placeholder_ready(
    record: &ProjectRecord,
    row: &TrackRow,
    key: &str,
    aliases: &[String],
) -> Option<String> {
    if !is_proposed(&row.status_raw) {
        return None;
    }
    let clean = status_clean(&row.status_raw);
    if is_hitl_marked(&row.id, &row.slug, &clean, &row.summary)
        || !status_names_placeholder(&row.status_raw)
        || !placeholder_pass_complete(record, key)
    {
        return None;
    }
    let ready = aliases
        .first()
        .cloned()
        .unwrap_or_else(|| DEFAULT_READY_PHRASE.to_string());
    Some(format!("**{ready}**"))
}

/// Apply every `(key, ready)` promotion to one registry body.
///
/// Pure. Returns the ids actually rewritten and the new body only when at least one row
/// matched. Sharing one body keeps a multi-row promotion to a single write.
pub(crate) fn promote_placeholders_in(
    text: &str,
    promotions: &[(String, String)],
) -> (Vec<String>, Option<String>) {
    let mut body = text.to_string();
    let mut promoted = Vec::new();
    for (key, ready) in promotions {
        if let Some(next) = promote_placeholder_in(&body, key, ready) {
            body = next;
            promoted.push(key.clone());
        }
    }
    if promoted.is_empty() {
        (promoted, None)
    } else {
        (promoted, Some(body))
    }
}

/// Promote `key`'s row to `ready` in the live registry. `Ok(false)` when no row matched.
///
/// 0109: one lock for both arms. Store mode promotes the first matching row and then
/// re-renders the view; `NoStore` keeps the shipped markdown writer under the same lock.
pub fn promote_placeholder(
    record: &ProjectRecord,
    key: &str,
    ready: &str,
) -> std::io::Result<bool> {
    promote_placeholder_inner(record, key, ready, None)
}

/// Promote only while the row still carries the exact status the fold gate observed.
///
/// Closes the read→lock race: a concurrent writer that changed the row wins the write and
/// this call reports `Ok(false)` (the fold then withholds instead of advancing).
pub fn promote_placeholder_observed(
    record: &ProjectRecord,
    key: &str,
    ready: &str,
    observed: &str,
) -> std::io::Result<bool> {
    promote_placeholder_inner(record, key, ready, Some(observed))
}

fn promote_placeholder_inner(
    record: &ProjectRecord,
    key: &str,
    ready: &str,
    observed: Option<&str>,
) -> std::io::Result<bool> {
    let dir = crate::layout::resolve(record).conductor_dir;
    let promotions = vec![(key.to_string(), ready.to_string())];
    crate::registry_store::with_registry_lock(&dir, || {
        let store_outcome = match observed {
            Some(observed) => crate::registry_store::promote_placeholders_observed_locked(
                &dir,
                &[(key.to_string(), observed.to_string(), ready.to_string())],
                "fold",
            )?,
            None => crate::registry_store::promote_placeholders_locked(&dir, &promotions, "fold")?,
        };
        match store_outcome {
            crate::registry_store::WriteOutcome::NoStore => {
                let path = dir.join("conductor.md");
                let text = read_view_for_parse(&path)?;
                if let Some(observed) = observed {
                    let current = parse_conductor_md(&text).ok().and_then(|rows| {
                        rows.into_iter()
                            .find(|r| row_matches_track(r, key))
                            .map(|r| r.status_raw)
                    });
                    if current.as_deref() != Some(observed) {
                        return Ok(false);
                    }
                }
                let (promoted, body) = promote_placeholders_in(&text, &promotions);
                if let Some(body) = body {
                    std::fs::write(&path, body)?;
                }
                Ok(!promoted.is_empty())
            }
            crate::registry_store::WriteOutcome::Changed(ids) => Ok(!ids.is_empty()),
            crate::registry_store::WriteOutcome::Unchanged => Ok(false),
        }
    })
    .map_err(std::io::Error::other)
}

/// [`width_deviations`] for a record's live registry.
pub fn width_deviations_for(record: &ProjectRecord) -> Vec<WidthDeviation> {
    read_view_for_parse(&conductor_md_path(record))
        .map(|t| width_deviations(&t))
        .unwrap_or_default()
}

/// Every GFM table with Track/Id + Status columns, concatenated, by header name.
pub fn parse_conductor_md(text: &str) -> Result<Vec<TrackRow>> {
    let (rows, _) = parse_conductor_md_parts(text);
    if rows.is_empty() {
        return Err(CoordinatorError::Message(
            "could not parse track registry in conductor.md; pass --track <id>".into(),
        ));
    }
    Ok(rows)
}

/// [`parse_conductor_md`] without the zero-row `Err`, plus whether any table looked like a
/// Track/Status registry at all.
///
/// 0120 needs to tell a readable but empty registry (a new project's `# tracks\n`, or a
/// header-only Tracks table) from a view that is not a registry. `parse_conductor_md` keeps
/// its shipped zero-row error for every existing caller; only the explicit-run guard uses
/// this parts form.
pub(crate) fn parse_conductor_md_parts(text: &str) -> (Vec<TrackRow>, bool) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let tables = collect_pipe_tables(text);
    let mut rows = Vec::new();
    let mut registry_shaped = false;
    for table in tables {
        if table.is_empty() {
            continue;
        }
        let headers: Vec<String> = table[0]
            .cells
            .iter()
            .map(|c| c.trim().to_string())
            .collect();
        let Some(track_col) = headers.iter().position(|h| is_track_or_id_header(h)) else {
            continue;
        };
        let Some(status_col) = headers.iter().position(|h| is_status_header(h)) else {
            continue;
        };
        registry_shaped = true;
        let summary_col = headers.iter().position(|h| is_summary_header(h));
        let execution_col = headers.iter().position(|h| is_execution_header(h));
        let ncols = headers.len();
        for row in table.iter().skip(1) {
            if row.cells.len() != ncols {
                continue;
            }
            let track_cell = unwrap_md_link(row.cells[track_col].trim());
            let Some((id, slug)) = parse_leading_id(&track_cell) else {
                continue;
            };
            let status_raw = row.cells[status_col].trim().to_string();
            let summary = summary_col
                .map(|i| row.cells[i].trim().to_string())
                .unwrap_or_default();
            let execution = execution_col
                .map(|i| row.cells[i].trim().to_string())
                .filter(|cell| !cell.is_empty());
            rows.push(TrackRow {
                id,
                slug,
                status_raw,
                summary,
                nostart: row.nostart,
                execution,
            });
        }
    }
    (rows, registry_shaped)
}

/// True when every non-blank line is an ATX heading, fence delimiter, thematic break, or
/// blockquote marker — a scaffold such as a new project's `# tracks\n`, not prose and not a
/// table.
///
/// Fence state is tracked: fenced **content** is prose and fails closed (including a line
/// that merely looks like a delimiter). Only a real opening/closing fence line is scaffolding.
/// 0120 uses this to keep a readable-but-empty registry on the explicit-wins path instead of
/// failing it closed as "no Track/Status table".
pub(crate) fn view_is_scaffolding_only(text: &str) -> bool {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut fence: Option<(char, usize)> = None;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if let Some((ch, len)) = fence {
            if closes_fence(line, ch, len) {
                fence = None;
                continue;
            }
            return false;
        }
        if is_indented_code(line) {
            return false;
        }
        if let Some(open) = opens_fence(line) {
            fence = Some(open);
            continue;
        }
        if is_atx_heading(t) || t.starts_with('>') || is_thematic_break_line(t) {
            continue;
        }
        return false;
    }
    true
}

/// `(fence char, run length)` when `line` opens a GFM code fence (≤3 leading spaces, 3+
/// backticks or tildes).
fn opens_fence(line: &str) -> Option<(char, usize)> {
    if line.len() - line.trim_start_matches(' ').len() > 3 {
        return None;
    }
    let t = line.trim_start();
    for ch in ['`', '~'] {
        let run = t.chars().take_while(|c| *c == ch).count();
        if run >= 3 {
            return Some((ch, run));
        }
    }
    None
}

/// A closing fence uses the same char, is at least as long as the opener, and carries only
/// trailing whitespace.
fn closes_fence(line: &str, ch: char, open_len: usize) -> bool {
    if line.len() - line.trim_start_matches(' ').len() > 3 {
        return false;
    }
    let t = line.trim_start();
    let run = t.chars().take_while(|c| *c == ch).count();
    run >= open_len && t[run..].trim().is_empty()
}

/// GFM indented code block: four or more leading whitespace columns (a tab advances to the
/// next multiple of 4). A table indented this far is code, not registry text.
pub(crate) fn is_indented_code(line: &str) -> bool {
    let mut cols = 0usize;
    for c in line.chars() {
        match c {
            ' ' => cols += 1,
            '\t' => cols += 4 - (cols % 4),
            _ => break,
        }
        if cols >= 4 {
            return true;
        }
    }
    false
}

fn is_atx_heading(line: &str) -> bool {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    (1..=6).contains(&hashes)
        && line[hashes..]
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace())
}

fn is_thematic_break_line(line: &str) -> bool {
    let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    let mut chars = compact.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    matches!(first, '-' | '*' | '_') && compact.len() >= 3 && chars.all(|c| c == first)
}

/// Why a row never reached [`TrackRow`] (0115).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DropReason {
    /// Cell count differs from the table header (GFM allows either; the product drops).
    CellCount,
    /// `parse_leading_id` rejected the (link-unwrapped) track cell.
    IdUnparseable,
}

/// One row `parse_conductor_md` silently drops, with its physical source line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroppedRow {
    /// 1-based physical line in `conductor.md`.
    pub line: usize,
    pub line_cells: usize,
    pub header_cells: usize,
    pub track_cell: String,
    pub status_cell: String,
    pub reason: DropReason,
}

/// Diagnostic mirror of the parser's drop decisions (0115).
#[derive(Debug, Clone, Default)]
pub(crate) struct DroppedScan {
    /// Tables that carry both a Track/Id and a Status column.
    pub tables: usize,
    /// Rows in those tables (header excluded).
    pub data_rows: usize,
    /// Rows `parse_conductor_md` returns.
    pub parsed_rows: usize,
    /// `data_rows - parsed_rows`; always exact.
    pub dropped_total: usize,
    /// Up to [`DROPPED_SAMPLE_CAP`] samples; totals above stay exact.
    pub dropped: Vec<DroppedRow>,
    /// Raw trimmed status text → count over **all** data rows (parsed and dropped).
    ///
    /// Present so `ready_hits` can stay exact when `dropped` is capped.
    pub status_counts: std::collections::BTreeMap<String, usize>,
}

/// Keep the sample list bounded; `dropped_total` is the exact count.
pub(crate) const DROPPED_SAMPLE_CAP: usize = 50;

/// Walk the same tables and rows as [`parse_conductor_md`] and record what it drops.
///
/// Mirrors the parser **exactly**, in this order: BOM strip, the same header
/// rules, the same cell-count check, then `unwrap_md_link` before
/// `parse_leading_id`. Every markdown-linked track cell (the dominant live form)
/// must be unwrapped first or it would be misclassified `id_unparseable`.
///
/// Read-only: it names the live drops the picker cannot see (0115).
pub(crate) fn scan_dropped_rows(text: &str) -> DroppedScan {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut scan = DroppedScan::default();
    for table in collect_pipe_tables_lines(text) {
        if table.len() < 2 {
            continue;
        }
        let headers: Vec<String> = table[0]
            .0
            .cells
            .iter()
            .map(|c| c.trim().to_string())
            .collect();
        let Some(track_col) = headers.iter().position(|h| is_track_or_id_header(h)) else {
            continue;
        };
        let Some(status_col) = headers.iter().position(|h| is_status_header(h)) else {
            continue;
        };
        scan.tables += 1;
        let ncols = headers.len();
        for (row, line) in table.iter().skip(1) {
            scan.data_rows += 1;
            let status_cell = row
                .cells
                .get(status_col)
                .map(|c| c.trim())
                .unwrap_or_default();
            if !status_cell.is_empty() {
                *scan
                    .status_counts
                    .entry(status_cell.to_string())
                    .or_insert(0) += 1;
            }
            if row.cells.len() != ncols {
                scan.dropped_total += 1;
                push_dropped(
                    &mut scan,
                    *line,
                    row,
                    track_col,
                    status_col,
                    ncols,
                    DropReason::CellCount,
                );
                continue;
            }
            // `track_col < ncols` here because the cell count matches the header.
            let track_cell = unwrap_md_link(row.cells[track_col].trim());
            if parse_leading_id(&track_cell).is_none() {
                scan.dropped_total += 1;
                push_dropped(
                    &mut scan,
                    *line,
                    row,
                    track_col,
                    status_col,
                    ncols,
                    DropReason::IdUnparseable,
                );
                continue;
            }
            scan.parsed_rows += 1;
        }
    }
    scan
}

fn push_dropped(
    scan: &mut DroppedScan,
    line: usize,
    row: &PipeRow,
    track_col: usize,
    status_col: usize,
    header_cells: usize,
    reason: DropReason,
) {
    if scan.dropped.len() >= DROPPED_SAMPLE_CAP {
        return;
    }
    // `get` never indexes: GFM allows a short data row, and two live coordinated
    // rows are one cell short of the header.
    let track_cell = row
        .cells
        .get(track_col)
        .map(|c| unwrap_md_link(c.trim()))
        .unwrap_or_default();
    let status_cell = row
        .cells
        .get(status_col)
        .map(|c| c.trim().to_string())
        .unwrap_or_default();
    scan.dropped.push(DroppedRow {
        line,
        line_cells: row.cells.len(),
        header_cells,
        track_cell,
        status_cell,
        reason,
    });
}

pub(crate) fn is_track_or_id_header(h: &str) -> bool {
    h.eq_ignore_ascii_case("track") || h.eq_ignore_ascii_case("id")
}

pub(crate) fn is_status_header(h: &str) -> bool {
    h.eq_ignore_ascii_case("status")
}

pub(crate) fn is_summary_header(h: &str) -> bool {
    h.eq_ignore_ascii_case("summary")
}

/// Case/punctuation/whitespace-insensitive execution header match.
pub(crate) fn is_execution_header(h: &str) -> bool {
    let norm: String = h
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    matches!(
        norm.as_str(),
        "execution" | "executionpath" | "executionrepo"
    )
}

pub(crate) fn unwrap_md_link(cell: &str) -> String {
    let t = cell.trim();
    if let Some(rest) = t.strip_prefix('[')
        && let Some(end) = rest.find("](")
        && rest[end + 2..].ends_with(')')
    {
        return rest[..end].to_string();
    }
    t.to_string()
}

pub(crate) fn parse_leading_id(text: &str) -> Option<(String, String)> {
    let t = text.trim();
    let b = t.as_bytes();
    // Registries in the wild prefix the numeric id with an alpha marker (`| T374 | ... |`)
    // while the engine canonicalises on the digits. Requiring the cell to START with four
    // digits made every row of such a registry unreadable, which stops the fold -> implement
    // gate on every track in that project (0075). Accept an optional alpha prefix and keep the
    // digit run as the id, so `T374` parses as `374` and matches a run launched with that id.
    let (digits, off) = if b.first().is_some_and(u8::is_ascii_alphabetic) {
        let n = b[1..].iter().take_while(|c| c.is_ascii_digit()).count();
        if n == 0 {
            return None;
        }
        (&t[1..1 + n], 1 + n)
    } else {
        let n = b.iter().take_while(|c| c.is_ascii_digit()).count();
        if n < 4 {
            return None;
        }
        (&t[..n], n)
    };
    let rest = &t[off..];
    if rest
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    let slug = rest.strip_prefix('-').unwrap_or(rest).to_string();
    Some((digits.to_string(), slug))
}

/// Split on unescaped `|`. Drop wrapping empty cells from leading/trailing pipes.
pub(crate) fn split_unescaped_pipes(line: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&'|') {
            chars.next();
            cur.push('|');
        } else if c == '|' {
            cells.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    cells.push(cur);
    if cells.first().is_some_and(|c| c.is_empty()) {
        cells.remove(0);
    }
    if cells.last().is_some_and(|c| c.is_empty()) {
        cells.pop();
    }
    cells
}

pub(crate) fn is_delimiter_cell(cell: &str) -> bool {
    let t = cell.trim();
    let t = t.strip_prefix(':').unwrap_or(t);
    let t = t.strip_suffix(':').unwrap_or(t);
    t.len() >= 3 && t.chars().all(|c| c == '-')
}

pub(crate) fn looks_like_row(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && t.contains('|') && !t.starts_with("```") && !t.starts_with("~~~")
}

/// CommonMark 0.31.2 / GFM §4.1 thematic break, and not a pipe row.
/// Indent is at most three spaces (four-space / tab-indented lines stay
/// non-separators so a code-block `---` still ends the table).
pub(crate) fn is_thematic_break(line: &str) -> bool {
    if line.contains('|') {
        return false;
    }
    let mut rest = line.trim_end();
    for _ in 0..3 {
        if let Some(stripped) = rest.strip_prefix(' ') {
            rest = stripped;
        } else {
            break;
        }
    }
    if rest.starts_with(' ') || rest.starts_with('\t') {
        return false;
    }
    let mut chars = rest.chars().filter(|c| *c != ' ' && *c != '\t');
    let Some(first) = chars.next() else {
        return false;
    };
    if first != '-' && first != '*' && first != '_' {
        return false;
    }
    let mut n = 1;
    for c in chars {
        if c != first {
            return false;
        }
        n += 1;
    }
    n >= 3
}

pub(crate) fn is_table_start(lines: &[&str], i: usize) -> bool {
    if i + 1 >= lines.len() || !looks_like_row(lines[i]) || !looks_like_row(lines[i + 1]) {
        return false;
    }
    let (delim_line, _) = strip_nostart_literal(lines[i + 1]);
    let delim = split_unescaped_pipes(&delim_line);
    !delim.is_empty() && delim.iter().all(|c| is_delimiter_cell(c))
}

fn collect_pipe_tables(text: &str) -> Vec<Vec<PipeRow>> {
    collect_pipe_tables_lines(text)
        .into_iter()
        .map(|table| table.into_iter().map(|(row, _)| row).collect())
        .collect()
}

/// [`collect_pipe_tables`] with each row's **1-based physical source line**.
///
/// The line number comes from this one walk (never a second scan), so a dropped
/// row can name its source line without risking a different view of the text.
fn collect_pipe_tables_lines(text: &str) -> Vec<Vec<(PipeRow, usize)>> {
    let lines: Vec<&str> = text.lines().collect();
    let mut tables = Vec::new();
    let mut i = 0;
    let mut fence: Option<(char, usize)> = None;
    while i < lines.len() {
        // GFM fences are code, not tables: a fenced fake Track/Status table is not a
        // registry. The walk is the one place rows are collected, so the state lives here.
        if let Some((ch, len)) = fence {
            if closes_fence(lines[i], ch, len) {
                fence = None;
            }
            i += 1;
            continue;
        }
        if let Some(open) = opens_fence(lines[i]) {
            fence = Some(open);
            i += 1;
            continue;
        }
        if is_indented_code(lines[i]) {
            i += 1;
            continue;
        }
        if !is_table_start(&lines, i) {
            i += 1;
            continue;
        }
        let (header_line, header_nostart) = strip_nostart_literal(lines[i]);
        let header = split_unescaped_pipes(&header_line);
        let mut rows = vec![(
            PipeRow {
                cells: header,
                nostart: header_nostart,
            },
            i + 1,
        )];
        i += 2;
        while i < lines.len() {
            if lines[i].trim().is_empty() || is_thematic_break(lines[i]) {
                i += 1;
                continue;
            }
            if is_indented_code(lines[i]) || !looks_like_row(lines[i]) || is_table_start(&lines, i)
            {
                break;
            }
            let (stripped, nostart) = strip_nostart_literal(lines[i]);
            rows.push((
                PipeRow {
                    cells: split_unescaped_pipes(&stripped),
                    nostart,
                },
                i + 1,
            ));
            i += 1;
        }
        tables.push(rows);
    }
    tables
}

/// Suggested-execution-order fence: 4-digit ids, first appearance, skip ISO dates.
pub fn parse_execution_order(text: &str) -> Vec<String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines: Vec<&str> = text.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if !trimmed.starts_with('#') {
            continue;
        }
        let title = trimmed.trim_start_matches('#').trim();
        if !title
            .to_ascii_lowercase()
            .contains("suggested execution order")
        {
            continue;
        }
        if let Some(body) = next_fence_body(&lines, i + 1) {
            return extract_order_ids(&body);
        }
    }
    Vec::new()
}

fn next_fence_body(lines: &[&str], from: usize) -> Option<String> {
    let mut j = from;
    while j < lines.len() {
        let t = lines[j].trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            let mut body = String::new();
            j += 1;
            while j < lines.len() {
                let close = lines[j].trim_start();
                if close.starts_with("```") || close.starts_with("~~~") {
                    return Some(body);
                }
                body.push_str(lines[j]);
                body.push('\n');
                j += 1;
            }
            return Some(body);
        }
        j += 1;
    }
    None
}

fn extract_order_ids(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i - start != 4 {
                continue;
            }
            let left_ok = start == 0 || !is_word_byte(bytes[start - 1]);
            let rest = &body[i..];
            if is_iso_date_tail(rest) {
                continue;
            }
            if left_ok && is_id_lookahead(rest) {
                let id = &body[start..i];
                if !out.iter().any(|s| s == id) {
                    out.push(id.to_string());
                }
            }
            continue;
        }
        i += 1;
    }
    out
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_iso_date_tail(rest: &str) -> bool {
    let b = rest.as_bytes();
    b.len() >= 6
        && b[0] == b'-'
        && b[1].is_ascii_digit()
        && b[2].is_ascii_digit()
        && b[3] == b'-'
        && b[4].is_ascii_digit()
        && b[5].is_ascii_digit()
}

/// `\b(\d{4})(?=-\w|\s+[A-Za-z]|$)`
fn is_id_lookahead(rest: &str) -> bool {
    if rest.is_empty() {
        return true;
    }
    let b = rest.as_bytes();
    if b[0] == b'-' && b.len() > 1 && is_word_byte(b[1]) {
        return true;
    }
    let stripped = rest.trim_start_matches(|c: char| c.is_whitespace());
    if stripped.len() < rest.len()
        && stripped
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
    {
        return true;
    }
    false
}

/// Track+Status rows `parse_conductor_md` can see. `None` if the file is
/// missing or not a parseable registry (honest empty vs parser-blind).
pub fn parsed_track_row_count(record: &ProjectRecord) -> Option<usize> {
    load_track_rows(record).map(|rows| rows.len())
}

fn empty_ready_pick_error(record: &ProjectRecord) -> String {
    if record.ready_aliases.is_empty() {
        "no Ready — not started track in conductor.md; pass --track <id>".into()
    } else {
        format!(
            "no eligible Ready track (aliases: {}); pass --track <id>",
            ready_aliases_for(record).join(", ")
        )
    }
}

/// True only when a pick error means "no eligible Ready row exists" — an empty backlog.
///
/// A missing `conductor.md` is NOT benign: for a `full` project it is a misconfigured
/// workspace/layout, and it must record like an unparseable table. Additive classifier for
/// 0103 — it must not change pick behaviour.
pub fn ready_pick_miss_is_benign(err: &crate::error::CoordinatorError) -> bool {
    let crate::error::CoordinatorError::Message(msg) = err else {
        return false;
    };
    msg.starts_with("no Ready — not started track")
        || msg.starts_with("no eligible Ready track (aliases: ")
}

/// Read `{conductor_dir}/conductor.md` and return the first eligible Ready id
/// whose track directory exists.
pub fn pick_next_ready(record: &ProjectRecord) -> Result<String> {
    pick_next_ready_excluding(record, &[], None)
}

/// `pick_next_ready` plus skip list / optional merged-PR probe (0046).
pub fn pick_next_ready_excluding(
    record: &ProjectRecord,
    skip: &[String],
    probe: Option<&dyn MergedTrackProbe>,
) -> Result<String> {
    pick_next_ready_excluding_sticky(record, skip, probe, &[])
}

/// Like [`pick_next_ready_excluding`], plus a read-time Ready overlay (0055).
///
/// Sticky ids stay eligible when the live cell is no longer a Ready alias,
/// unless HITL, `<!-- nostart -->`, Completed, or `In progress`. Merged-PR
/// probe still wins. Does not write `conductor.md`.
pub fn pick_next_ready_excluding_sticky(
    record: &ProjectRecord,
    skip: &[String],
    probe: Option<&dyn MergedTrackProbe>,
    sticky: &[String],
) -> Result<String> {
    let path = crate::layout::resolve(record)
        .conductor_dir
        .join("conductor.md");
    let snapshot = crate::registry_store::snapshot(record)?;
    let (rows, order) = match &snapshot.view {
        crate::registry_store::StoreView::Absent => {
            if !path.is_file() {
                return Err(CoordinatorError::Message(format!(
                    "no conductor.md at {}; pass --track <id>",
                    path.display()
                )));
            }
            let text = read_view_for_parse(&path)?;
            (parse_conductor_md(&text)?, parse_execution_order(&text))
        }
        crate::registry_store::StoreView::Drifted { expected, actual } => {
            let dir = crate::registry_store::conductor_dir_for(record);
            return Err(crate::registry_store::view_drift_error(
                &dir, expected, actual,
            ));
        }
        crate::registry_store::StoreView::Ok => {
            (snapshot.rows.clone(), snapshot.execution_order.clone())
        }
    };
    let aliases = ready_aliases_for(record);
    let mut eligible: Vec<&TrackRow> = rows
        .iter()
        .filter(|r| row_overlay_eligible(r, &aliases, sticky))
        .collect();
    if eligible.is_empty() {
        return Err(CoordinatorError::Message(empty_ready_pick_error(record)));
    }
    if !order.is_empty() {
        eligible.sort_by_key(|r| {
            order
                .iter()
                .position(|id| id == &r.id)
                .unwrap_or(usize::MAX)
        });
    }
    let exec = crate::layout::resolve(record).execution_repo;
    let mut missing = Vec::new();
    for row in eligible {
        if skip.iter().any(|s| track_ids_match(&row.id, s)) {
            continue;
        }
        if track_row_completed(&rows, &row.id) {
            continue;
        }
        if resolve_track_dir(record, &row.id).is_none() {
            missing.push(row.id.clone());
            continue;
        }
        if let (Some(probe), Some(cwd)) = (probe, exec.as_ref())
            && let Some(nid) = numeric_track_id(&row.id)
            && probe.merged_pr_for_track(cwd, nid).ok().flatten().is_some()
        {
            continue;
        }
        return Ok(row.id.clone());
    }
    if missing.is_empty() {
        return Err(CoordinatorError::Message(empty_ready_pick_error(record)));
    }
    Err(CoordinatorError::Message(format!(
        "Ready track(s) {} have no matching conductor directory; pass --track <id>",
        missing.join(", ")
    )))
}

pub(crate) fn row_overlay_eligible(row: &TrackRow, aliases: &[String], sticky: &[String]) -> bool {
    if is_hitl_marked(&row.id, &row.slug, &row.status_raw, &row.summary) || row.nostart {
        return false;
    }
    if is_eligible_ready_in(&row.status_raw, aliases) {
        return true;
    }
    if explicit_non_ready(&row.status_raw) {
        return false;
    }
    sticky.iter().any(|s| track_ids_match(&row.id, s))
}

/// Statuses sticky must not override or retain (0074).
///
/// `In progress` and `is_terminal` (Completed, Absorbed, Cancelled) were
/// already exempt. `Proposed` and `Blocked` are the explicit withholds.
pub(crate) fn explicit_non_ready(status_raw: &str) -> bool {
    is_in_progress(status_raw)
        || is_terminal(status_raw)
        || is_proposed(status_raw)
        || is_blocked(status_raw)
}

fn sticky_keep_live(row: &TrackRow) -> bool {
    !is_hitl_marked(&row.id, &row.slug, &row.status_raw, &row.summary)
        && !row.nostart
        && !explicit_non_ready(&row.status_raw)
}

/// Union-capture operator-Ready ids (0055). Missing/unparseable file → `prev`.
pub fn capture_sticky_ready_ids(record: &ProjectRecord, prev: &[String]) -> Vec<String> {
    let Some(rows) = load_track_rows(record) else {
        return prev.to_vec();
    };
    let aliases = ready_aliases_for(record);
    let mut out: Vec<String> = Vec::new();
    let push = |out: &mut Vec<String>, id: &str| {
        if !out.iter().any(|e| track_ids_match(e, id)) {
            out.push(id.to_string());
        }
    };
    for id in prev {
        if let Some(row) = rows.iter().find(|r| track_ids_match(&r.id, id))
            && sticky_keep_live(row)
        {
            push(&mut out, &row.id);
        }
    }
    for row in &rows {
        if is_eligible_ready_in(&row.status_raw, &aliases) && sticky_keep_live(row) {
            push(&mut out, &row.id);
        }
    }
    out
}

/// Whether a finished `fold` may enter `implement` (0075).
///
/// Ready aliases win. `Proposed`, `Blocked`, and [`is_terminal`] withhold.
/// `In progress` and any other non-token cell allow. This reader does not
/// call `explicit_non_ready`, which also treats `In progress` as excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoldAdvance {
    Allow,
    Withhold {
        status_clean: String,
    },
    /// Nothing could be read: no id given, or the registry is missing/unparseable.
    Unreadable,
    /// The registry parsed cleanly, but no row matched `key`.
    ///
    /// Distinct from [`FoldAdvance::Unreadable`] on purpose. A row can sit OUTSIDE
    /// the track table -- a heading or a `---` ends the table, so rows below it belong
    /// to no table and are never parsed. Reporting the parsed-row count and the key
    /// makes that self-evident instead of reading as a corrupt registry.
    RowMissing {
        key: String,
        parsed: usize,
    },
    /// A `Proposed` row that names the placeholder condition and whose `plan.md` is no
    /// longer the stub. The condition the status names is satisfied, so the caller
    /// promotes the row and then advances exactly as for [`FoldAdvance::Allow`].
    PromotePlaceholder {
        key: String,
        ready: String,
        /// The raw status this gate observed. The writer promotes only while the row still
        /// carries exactly this status (a concurrent writer wins; no silent overwrite).
        observed: String,
    },
}

/// Read the registry row for `track_id`. Missing id, file, or row is [`FoldAdvance::Unreadable`].
pub fn fold_advance(record: &ProjectRecord, track_id: Option<&str>) -> FoldAdvance {
    let Some(key) = track_id.map(str::trim).filter(|s| !s.is_empty()) else {
        return FoldAdvance::Unreadable;
    };
    let Some(rows) = load_track_rows(record) else {
        return FoldAdvance::Unreadable;
    };
    let Some(row) = rows.iter().find(|r| row_matches_track(r, key)) else {
        return FoldAdvance::RowMissing {
            key: key.to_string(),
            parsed: rows.len(),
        };
    };
    let aliases = ready_aliases_for(record);
    if is_eligible_ready_in(&row.status_raw, &aliases) {
        return FoldAdvance::Allow;
    }
    if is_proposed(&row.status_raw) {
        // A placeholder row whose spec/plan pass has actually run has met the condition
        // its own status names. Promote it rather than withholding forever: the fold will
        // not advance a `Proposed` row, and the planner re-plans one on every tick, so a
        // completed placeholder deadlocks (one track ran six cycles and never reached
        // implement; the row has to be promoted from outside the fold either way).
        // HITL and owner-only marks still win anywhere in the row, and a bare
        // `Proposed - gated on ...` hold does not name the placeholder condition.
        let clean = status_clean(&row.status_raw);
        if let Some(ready) = placeholder_ready(record, row, key, &aliases) {
            return FoldAdvance::PromotePlaceholder {
                key: key.to_string(),
                ready,
                observed: row.status_raw.clone(),
            };
        }
        return FoldAdvance::Withhold {
            status_clean: clean,
        };
    }
    if is_blocked(&row.status_raw) || is_terminal(&row.status_raw) {
        return FoldAdvance::Withhold {
            status_clean: status_clean(&row.status_raw),
        };
    }
    FoldAdvance::Allow
}

/// `Some(status_clean)` when `id` was picked but the live cell is not Ready.
pub fn overlay_file_status(record: &ProjectRecord, id: &str) -> Option<String> {
    let rows = load_track_rows(record)?;
    let aliases = ready_aliases_for(record);
    let row = rows.iter().find(|r| track_ids_match(&r.id, id))?;
    if is_eligible_ready_in(&row.status_raw, &aliases) {
        None
    } else {
        Some(status_clean(&row.status_raw))
    }
}

/// Test helper: one exact Ready row + `{id}-Fixture/` dir under `ws/conductor/`.
#[cfg(test)]
pub fn write_ready_fixture(ws: &std::path::Path, id: &str) -> std::io::Result<()> {
    let cond = ws.join("conductor");
    let track = cond.join(format!("{id}-Fixture"));
    std::fs::create_dir_all(&track)?;
    // A Ready row means spec/plan already exist. The fold gate refuses to advance to
    // implement without a usable plan.md, so the fixture has to carry one or it models a
    // state the real registry never produces.
    std::fs::write(track.join("spec.md"), b"fixture spec\n")?;
    std::fs::write(track.join("plan.md"), b"fixture plan\n")?;
    let md = format!(
        "| Track | Execution path | Status | Summary |\n\
         | --- | --- | --- | --- |\n\
         | {id}-Fixture | `.` | **Ready — not started** | fixture |\n"
    );
    std::fs::write(cond.join("conductor.md"), md)
}

#[cfg(test)]
mod tests_0088 {
    use super::*;
    use std::path::Path;

    fn rec(path: &Path) -> ProjectRecord {
        ProjectRecord {
            id: uuid::Uuid::new_v4().to_string(),
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
            auto_start: AutoStartPolicy::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            ci_fix_routing: false,
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now(),
        }
    }

    fn write_md(ws: &Path, md: &str) {
        let cond = ws.join("conductor");
        std::fs::create_dir_all(&cond).unwrap();
        std::fs::write(cond.join("conductor.md"), md).unwrap();
    }

    /// DoD-4: `status=Ready` + a banner saying "do not auto-start" ⇒ the row stays Ready,
    /// and a downgrade written into the cell is undone.
    #[test]
    fn ready_row_downgraded_by_advance_is_restored() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        let before_md = "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0475-V1](0475-V1/spec.md) | `.` | **Ready — not started** | go |\n\
             | [0476-V2](0476-V2/spec.md) | `.` | **Ready — not started** | go |\n";
        write_md(ws, before_md);
        let r = rec(ws);
        let before = status_snapshot(before_md);
        assert_eq!(
            before.get("0475").map(String::as_str),
            Some("**Ready — not started**")
        );

        // The "advance" session downgraded 0475 on the strength of a stale banner.
        let after_md = "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0475-V1](0475-V1/spec.md) | `.` | **Proposed** <!-- nostart --> | go |\n\
             | [0476-V2](0476-V2/spec.md) | `.` | **Ready — not started** | go |\n";
        let aliases = ready_aliases_for(&r);
        let (restored, body) = restore_ready_rows(after_md, &before, &aliases);
        assert_eq!(restored, vec!["0475".to_string()]);
        let body = body.expect("body rewritten");
        let reparsed = parse_conductor_md(&body).unwrap();
        let row = reparsed.iter().find(|x| x.id == "0475").unwrap();
        assert!(
            is_eligible_ready_in(&row.status_raw, &aliases),
            "restored cell must be Ready again, got {:?}",
            row.status_raw
        );
    }

    /// A row already non-Ready before the phase is left alone — no false restore.
    #[test]
    fn non_ready_row_is_not_touched() {
        let ws_md = "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0475-V1](0475-V1/spec.md) | `.` | **Proposed** | go |\n";
        let dir = tempfile::tempdir().unwrap();
        write_md(dir.path(), ws_md);
        let r = rec(dir.path());
        let before = status_snapshot(ws_md);
        let aliases = ready_aliases_for(&r);
        let (restored, body) = restore_ready_rows(ws_md, &before, &aliases);
        assert!(restored.is_empty(), "{restored:?}");
        assert!(body.is_none());
    }

    fn registry(rows: &str) -> String {
        format!(
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             {rows}"
        )
    }

    fn linked(id: &str, status: &str) -> String {
        format!("| [{id}-T]({id}-T/spec.md) | `.` | {status} | go |\n")
    }

    fn ready_before(ids: &[&str]) -> std::collections::BTreeMap<String, String> {
        ids.iter()
            .map(|id| ((*id).to_string(), "**Ready — not started**".to_string()))
            .collect()
    }

    /// DoD-1: a terminal live cell is left, and a terminal-only table returns no body.
    #[test]
    fn terminal_cells_are_not_restored() {
        let completed = "**Completed** 2026-10-12 — PR **#87** squash `b9dc76e`";
        let absorbed = "**Absorbed — 0058 `204a258`**";
        let cancelled = "**Cancelled** - note";
        let text = registry(&format!(
            "{}{}{}",
            linked("0001", completed),
            linked("0002", absorbed),
            linked("0003", cancelled),
        ));
        let before = ready_before(&["0001", "0002", "0003"]);
        let aliases = ready_aliases_for(&rec(std::path::Path::new(".")));
        let (restored, body) = restore_ready_rows(&text, &before, &aliases);
        assert!(restored.is_empty(), "{restored:?}");
        assert!(body.is_none());
    }

    /// DoD-2: Proposed stays covered by the 0088 test. These four stay restored too.
    #[test]
    fn non_terminal_downgrades_are_still_restored() {
        let text = registry(&format!(
            "{}{}{}{}",
            linked("0001", "**Blocked**"),
            linked("0002", ""),
            linked("0003", "garbage"),
            linked("0004", "**In progress**"),
        ));
        let before = ready_before(&["0001", "0002", "0003", "0004"]);
        let aliases = ready_aliases_for(&rec(std::path::Path::new(".")));
        let (restored, body) = restore_ready_rows(&text, &before, &aliases);
        assert_eq!(
            restored,
            vec![
                "0001".to_string(),
                "0002".to_string(),
                "0003".to_string(),
                "0004".to_string()
            ]
        );
        let body = body.expect("rewritten");
        let rows = parse_conductor_md(&body).unwrap();
        assert_eq!(rows.len(), 4);
        for row in rows {
            assert!(
                is_eligible_ready_in(&row.status_raw, &aliases),
                "{} stayed {:?}",
                row.id,
                row.status_raw
            );
        }
    }

    /// DoD-3: a Completed sibling stays. Only the Proposed id is restored.
    #[test]
    fn mixed_terminal_and_proposed_restores_only_the_proposed_row() {
        let completed = "**Completed** 2026-10-12 — PR **#87** squash `b9dc76e`";
        let text = registry(&format!(
            "{}{}",
            linked("0001", completed),
            "| [0002-T](0002-T/spec.md) | `.` | **Proposed** <!-- nostart --> | go |\n",
        ));
        let before = ready_before(&["0001", "0002"]);
        let aliases = ready_aliases_for(&rec(std::path::Path::new(".")));
        let (restored, body) = restore_ready_rows(&text, &before, &aliases);
        assert_eq!(restored, vec!["0002".to_string()]);
        let rows = parse_conductor_md(&body.expect("rewritten")).unwrap();
        let done = rows.iter().find(|row| row.id == "0001").unwrap();
        assert_eq!(done.status_raw, completed);
        let proposed = rows.iter().find(|row| row.id == "0002").unwrap();
        assert!(is_eligible_ready_in(&proposed.status_raw, &aliases));
    }

    /// 0088 self-reference trap: a summary that *mentions* `<!-- nostart -->` in prose
    /// (or quotes it) must not set the flag; only a marker that terminates a cell does.
    #[test]
    fn prose_mention_of_marker_does_not_gate_the_row() {
        let prose = "| [0088-X](0088-X/spec.md) | `C:\\dev` | **Proposed** | Describes the fix: write `Proposed <!-- nostart -->` back to the cell |";
        let (stripped, flagged) = strip_nostart_literal(prose);
        assert!(!flagged, "a prose mention must not set nostart");
        assert!(
            stripped.contains("<!-- nostart -->"),
            "prose is preserved verbatim"
        );

        let real = "| [0088-X](0088-X/spec.md) | `C:\\dev` | **Proposed** | do not start <!-- nostart --> |";
        let (stripped2, flagged2) = strip_nostart_literal(real);
        assert!(flagged2, "a terminating marker must set nostart");
        assert!(!stripped2.contains("<!-- nostart -->"), "marker removed");

        // End-of-line form also counts.
        let eol = "| [0088-X](0088-X/spec.md) | **Proposed** | x <!-- nostart -->";
        assert!(strip_nostart_literal(eol).1);
    }

    /// DoD-3: a row whose width differs from the header is REPORTED, not silently dropped.
    #[test]
    fn width_deviation_is_reported_not_silent() {
        let md = "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0474-G](0474-G/spec.md) | `.` | **Completed** | uses `graphml | cypher` |\n\
             | [0475-V](0475-V/spec.md) | `.` | **Ready — not started** | ok |\n";
        let devs = width_deviations(md);
        assert_eq!(devs.len(), 1, "{devs:?}");
        // Header is `| Track | Execution path | Status | Summary |` => 4 cells (5 pipes).
        // The stray `|` makes this row 5 cells.
        assert_eq!(devs[0].want, 4);
        assert_eq!(devs[0].got, 5);
        assert!(devs[0].preview.contains("0474"), "{}", devs[0].preview);
        // The parse still drops it — but now the drop is visible.
        let rows = parse_conductor_md(md).unwrap();
        assert!(!rows.iter().any(|r| r.id == "0474"));
    }

    /// The escaped form is legitimate and must not be reported as a deviation.
    #[test]
    fn escaped_pipe_is_not_a_deviation() {
        let md = "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0474-G](0474-G/spec.md) | `.` | **Completed** | uses graphml \\| cypher |\n";
        assert!(width_deviations(md).is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcome::FailureClass;
    use crate::state::{RunState, RunStatus};
    use std::path::Path;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn rec(path: &Path) -> ProjectRecord {
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
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now(),
        }
    }

    fn pick_state(
        track_id: Option<&str>,
        status: RunStatus,
        next_track: Option<&str>,
        failure: Option<FailureClass>,
        last_event: &str,
    ) -> ReadyPickState {
        ReadyPickState {
            track_id: track_id.map(str::to_string),
            status,
            next_track: next_track.map(str::to_string),
            failure_class: failure,
            last_event: last_event.into(),
            auto_start: AutoStartPolicy::Hitl,
        }
    }

    fn write_md(ws: &Path, md: &str) {
        let cond = ws.join("conductor");
        std::fs::create_dir_all(&cond).unwrap();
        std::fs::write(cond.join("conductor.md"), md).unwrap();
    }

    fn mkdir_track(ws: &Path, name: &str) {
        std::fs::create_dir_all(ws.join("conductor").join(name)).unwrap();
    }

    fn one_row(status: &str, summary: &str, nostart: bool) -> String {
        let summary = if nostart {
            format!("{summary} <!-- nostart -->")
        } else {
            summary.to_string()
        };
        format!(
            "| Track | Status | Summary |\n\
             | --- | --- | --- |\n\
             | [0001-One](0001-One/spec.md) | {status} | {summary} |\n"
        )
    }

    #[test]
    fn fold_advance_matrix() {
        let dir = tempdir().unwrap();
        let mut record = rec(dir.path());

        assert_eq!(fold_advance(&record, None), FoldAdvance::Unreadable);
        assert_eq!(fold_advance(&record, Some("   ")), FoldAdvance::Unreadable);
        assert_eq!(
            fold_advance(&record, Some("0001")),
            FoldAdvance::Unreadable,
            "missing conductor.md"
        );

        write_md(dir.path(), &one_row("**Ready — not started**", "ok", false));
        // File parses, asked id is absent.
        write_md(
            dir.path(),
            "| Track | Status | Summary |\n\
             | --- | --- | --- |\n\
             | [0002-Two](0002-Two/spec.md) | **Ready — not started** | ok |\n",
        );
        // Not `Unreadable`: the registry parsed. The row is simply not in it.
        assert_eq!(
            fold_advance(&record, Some("0001")),
            FoldAdvance::RowMissing {
                key: "0001".into(),
                parsed: 1
            }
        );
        // `key` is echoed verbatim, so a T-prefixed id is reported as asked.
        assert_eq!(
            fold_advance(&record, Some("T374")),
            FoldAdvance::RowMissing {
                key: "T374".into(),
                parsed: 1
            }
        );

        let withhold = |record: &ProjectRecord, status: &str| {
            write_md(record.path.as_path(), &one_row(status, "ok", false));
            match fold_advance(record, Some("0001")) {
                FoldAdvance::Withhold { status_clean: got } => {
                    assert_eq!(got, status_clean(status), "{status}");
                    got
                }
                other => panic!("{status} -> {other:?}"),
            }
        };
        let proposed = withhold(
            &record,
            "**Proposed — placeholder, needs full spec/plan pass**",
        );
        assert!(proposed.starts_with("Proposed - placeholder"), "{proposed}");
        withhold(&record, "**Blocked — owner review pending**");
        withhold(&record, "**Cancelled** - note");
        withhold(&record, "**Absorbed — 0058 `204a258`**");
        withhold(
            &record,
            "**Completed** 2026-10-06 - PR **#69** squash 2f3a1ea",
        );

        let allow = |record: &ProjectRecord, status: &str, summary: &str, nostart: bool| {
            write_md(record.path.as_path(), &one_row(status, summary, nostart));
            assert_eq!(
                fold_advance(record, Some("0001")),
                FoldAdvance::Allow,
                "{status} {summary} nostart={nostart}"
            );
        };
        allow(&record, "**Ready — not started**", "ok", false);
        allow(&record, "**In progress**", "ok", false);
        allow(&record, "**Ready — folded @ sha1234**", "ok", false);
        allow(
            &record,
            "**Ready — not started**",
            "needs hitl review",
            false,
        );
        allow(&record, "**Ready — not started**", "ok", true);
        allow(&record, "   ", "ok", false);

        write_md(
            dir.path(),
            &one_row(
                "**Proposed — placeholder, needs full spec/plan pass**",
                "ok",
                true,
            ),
        );
        assert!(matches!(
            fold_advance(&record, Some("0001")),
            FoldAdvance::Withhold { .. }
        ));

        record.ready_aliases = vec!["Proposed".into()];
        write_md(dir.path(), &one_row("**Proposed**", "ok", false));
        assert_eq!(fold_advance(&record, Some("0001")), FoldAdvance::Allow);
        record.ready_aliases = vec!["Ready — folded @ sha1234".into()];
        allow(&record, "**Ready — folded @ sha1234**", "ok", false);
    }

    /// A completed placeholder promotes instead of withholding forever. Proven strings
    /// from `plan_markdown_is_placeholder`'s own tests: `# plan` is real, the
    /// `<!-- coordinator:placeholder-plan -->` comment is the stub.
    fn write_track_files(ws: &Path, name: &str, spec: &str, plan: &str) {
        let dir = ws.join("conductor").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("spec.md"), spec).unwrap();
        std::fs::write(dir.join("plan.md"), plan).unwrap();
    }

    /// 0106 test hygiene: hold the env lock, capture/clear every feature token, and
    /// point `COORDINATOR_HOME` at a temp dir before the caller builds a fixture.
    fn isolated_test_env() -> (crate::config::TestEnvGuard, tempfile::TempDir) {
        let guard =
            crate::config::TestEnvGuard::capture(&crate::features::test_feature_env_names());
        for name in crate::features::test_feature_env_names() {
            unsafe {
                std::env::remove_var(name);
            }
        }
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        (guard, home)
    }

    const PLACEHOLDER_STATUS: &str = "**Proposed — placeholder, needs full spec/plan pass**";

    #[test]
    fn completed_placeholder_promotes() {
        let dir = tempdir().unwrap();
        let record = rec(dir.path());
        write_md(dir.path(), &one_row(PLACEHOLDER_STATUS, "ok", false));
        write_track_files(dir.path(), "0001-One", "# spec\n", "# plan\n");
        assert_eq!(
            fold_advance(&record, Some("0001")),
            FoldAdvance::PromotePlaceholder {
                key: "0001".into(),
                ready: "**Ready — not started**".into(),
                observed: PLACEHOLDER_STATUS.into(),
            }
        );
    }

    #[test]
    fn placeholder_with_stub_plan_still_withholds() {
        let dir = tempdir().unwrap();
        let record = rec(dir.path());
        write_md(dir.path(), &one_row(PLACEHOLDER_STATUS, "ok", false));
        // A plan that still carries the stub marker means the pass has NOT run.
        write_track_files(
            dir.path(),
            "0001-One",
            "# spec\n",
            "<!-- coordinator:placeholder-plan -->\n",
        );
        assert!(matches!(
            fold_advance(&record, Some("0001")),
            FoldAdvance::Withhold { .. }
        ));
    }

    #[test]
    fn placeholder_without_spec_still_withholds() {
        let dir = tempdir().unwrap();
        let record = rec(dir.path());
        write_md(dir.path(), &one_row(PLACEHOLDER_STATUS, "ok", false));
        let d = dir.path().join("conductor").join("0001-One");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("plan.md"), "# plan\n").unwrap();
        assert!(matches!(
            fold_advance(&record, Some("0001")),
            FoldAdvance::Withhold { .. }
        ));
    }

    /// A deliberate owner hold does not name the placeholder condition, so a real plan on
    /// disk must NOT promote it.
    #[test]
    fn proposed_owner_hold_never_promotes() {
        let dir = tempdir().unwrap();
        let record = rec(dir.path());
        write_md(
            dir.path(),
            &one_row(
                "**Proposed** — gated on first paid interest + owner GO",
                "ok",
                false,
            ),
        );
        write_track_files(dir.path(), "0001-One", "# spec\n", "# plan\n");
        assert!(matches!(
            fold_advance(&record, Some("0001")),
            FoldAdvance::Withhold { .. }
        ));
    }

    /// An HITL / owner-only mark wins anywhere in the row, even on a completed placeholder.
    #[test]
    fn hitl_mark_blocks_promotion_of_completed_placeholder() {
        let dir = tempdir().unwrap();
        let record = rec(dir.path());
        write_md(
            dir.path(),
            &one_row(PLACEHOLDER_STATUS, "needs owner-only sign off", false),
        );
        write_track_files(dir.path(), "0001-One", "# spec\n", "# plan\n");
        assert!(matches!(
            fold_advance(&record, Some("0001")),
            FoldAdvance::Withhold { .. }
        ));
    }

    #[test]
    fn promote_placeholder_in_touches_only_the_status_cell() {
        // An escaped pipe inside another cell must survive, and so must the trailing LF.
        let text = "| Track | Status | Summary |\n\
                    | --- | --- | --- |\n\
                    | [0001-One](0001-One/spec.md) | **Proposed — placeholder, needs full spec/plan pass** | a \\| b |\n";
        let out = promote_placeholder_in(text, "0001", "**Ready — not started**").unwrap();
        assert!(
            out.contains("| **Ready — not started** | a \\| b |"),
            "{out}"
        );
        assert!(!out.contains("needs full spec/plan pass"), "{out}");
        assert!(out.ends_with('\n'));
        assert_eq!(out.lines().count(), text.lines().count());
    }

    #[test]
    fn promote_placeholder_in_unknown_key_is_none() {
        let text = "| Track | Status | Summary |\n\
                    | --- | --- | --- |\n\
                    | [0001-One](0001-One/spec.md) | **Proposed — placeholder, needs full spec/plan pass** | ok |\n";
        assert!(promote_placeholder_in(text, "9999", "**Ready — not started**").is_none());
    }

    /// Delegation-seam guard only (0106). After the refactor `fold_advance` calls the
    /// helper, so this is near-tautological by construction. The load-bearing fold
    /// evidence is the six unchanged fold tests plus
    /// `promote_placeholder_matches_single_row_rewrite`.
    #[test]
    fn placeholder_ready_matches_fold_advance_matrix() {
        let _lock = crate::config::test_env_lock();
        let (_env, _home) = isolated_test_env();
        let dir = tempdir().unwrap();
        let record = rec(dir.path());
        let cases = [
            ("**Ready — not started**", true, "ok", false),
            ("   ", true, "ok", false),
            (PLACEHOLDER_STATUS, true, "ok", true),
            (PLACEHOLDER_STATUS, false, "ok", false),
            ("**Proposed** — gated on owner GO", true, "ok", false),
            (PLACEHOLDER_STATUS, true, "needs owner-only sign off", false),
            ("**Blocked** — waiting", true, "ok", false),
            ("**Completed**", true, "ok", false),
            ("**In progress**", true, "ok", false),
        ];
        for (status, files, summary, expect) in cases {
            write_md(dir.path(), &one_row(status, summary, false));
            let track = dir.path().join("conductor").join("0001-One");
            if files {
                write_track_files(dir.path(), "0001-One", "# spec\n", "# plan\n");
            } else {
                let _ = std::fs::remove_dir_all(&track);
            }
            let text =
                std::fs::read_to_string(dir.path().join("conductor").join("conductor.md")).unwrap();
            let rows = parse_conductor_md(&text).unwrap();
            let row = rows.iter().find(|r| r.id == "0001").unwrap();
            let aliases = ready_aliases_for(&record);
            let helper = placeholder_ready(&record, row, &row.id, &aliases);
            assert_eq!(helper.is_some(), expect, "{status}");
            assert_eq!(
                helper.is_some(),
                matches!(
                    fold_advance(&record, Some("0001")),
                    FoldAdvance::PromotePlaceholder { .. }
                ),
                "{status}"
            );
        }
    }

    /// R4 placement: the helper has no `nostart` opinion; the no-run caller filters it.
    #[test]
    fn placeholder_ready_ignores_nostart_the_caller_filters_it() {
        let _lock = crate::config::test_env_lock();
        let (_env, _home) = isolated_test_env();
        crate::features::reset_feature_warnings_for_tests();
        crate::workflow::placeholder_backstop::reset_backstop_warnings_for_tests();

        let dir = tempdir().unwrap();
        let record = rec(dir.path());
        write_md(dir.path(), &one_row(PLACEHOLDER_STATUS, "ok", true));
        write_track_files(dir.path(), "0001-One", "# spec\n", "# plan\n");
        let text =
            std::fs::read_to_string(dir.path().join("conductor").join("conductor.md")).unwrap();
        let rows = parse_conductor_md(&text).unwrap();
        let row = rows.iter().find(|r| r.id == "0001").unwrap();
        assert!(row.nostart);
        let aliases = ready_aliases_for(&record);
        assert_eq!(
            placeholder_ready(&record, row, &row.id, &aliases),
            Some("**Ready — not started**".to_string())
        );
        assert_eq!(
            crate::workflow::placeholder_backstop::tick(&record),
            crate::workflow::placeholder_backstop::BackstopOutcome::Skipped(
                crate::workflow::placeholder_backstop::BackstopSkip::NoCandidate
            )
        );
        assert!(text.contains("**Proposed — placeholder"), "{text}");
    }

    /// Bytes equal the unchanged pure single-row rewrite: only the Status cell changes,
    /// escaped pipes / other rows / the trailing newline survive.
    #[test]
    fn promote_placeholder_matches_single_row_rewrite() {
        let _lock = crate::config::test_env_lock();
        let (_env, _home) = isolated_test_env();
        let dir = tempdir().unwrap();
        let record = rec(dir.path());
        let before = "| Track | Status | Summary |\n\
                      | --- | --- | --- |\n\
                      | [0001-One](0001-One/spec.md) | **Proposed — placeholder, needs full spec/plan pass** | a \\| b |\n\
                      | [0002-Two](0002-Two/spec.md) | **Ready — not started** | untouched |\n";
        write_md(dir.path(), before);
        let expected = promote_placeholder_in(before, "0001", "**Ready — not started**").unwrap();
        assert!(promote_placeholder(&record, "0001", "**Ready — not started**").unwrap());
        let after =
            std::fs::read_to_string(dir.path().join("conductor").join("conductor.md")).unwrap();
        assert_eq!(after, expected);
        assert!(
            after
                .contains("| [0002-Two](0002-Two/spec.md) | **Ready — not started** | untouched |")
        );
    }

    #[test]
    fn status_clean_emoji_outside_and_inside_bold() {
        assert!(is_eligible_ready("**Ready — not started**"));
        assert!(is_eligible_ready("✅ **Ready — not started**"));
        assert!(is_eligible_ready("**✅ Ready — not started**"));
        assert!(!is_eligible_ready(
            "✅ **Ready — not started** (planned 2026-07-29; **revised…**)"
        ));
        assert!(!is_eligible_ready(
            "**Proposed — placeholder, needs full spec/plan pass**"
        ));
        assert!(!is_eligible_ready("**Completed**"));
        assert!(!is_eligible_ready("**Ready — not started** (owner only)"));
        assert!(is_completed("**Completed**"));
        assert!(is_completed("Completed"));
        assert!(!is_completed("**Ready — not started**"));
        assert!(!is_completed("**Cancelled**"));
        assert!(is_in_progress("**In progress**"));
        assert!(is_in_progress("In progress"));
        assert!(!is_in_progress("**Completed**"));
        assert!(!is_in_progress("**Ready — not started**"));
    }

    const CELL_0053: &str = "**Completed** 2026-09-12 — PR **#54** squash `226f4ba`";
    const CELL_0054: &str = "**Completed** 2026-09-15 — PR **#55** squash `cacb9cc`";
    const CELL_0055: &str = "**Completed** 2026-09-16 — PR **#56** squash `8ab5bdb`";
    const CELL_0056: &str = "**Completed** 2026-09-18 — PR **#57** squash `aa5f142`";
    const CELL_0057: &str =
        "**Completed** 2026-09-22 — PR **#58** squash `075f239b32f574301382dbcffee6588853807977`";
    const CELL_0060_IN_PROGRESS: &str = "**In progress** PR #60";

    #[test]
    fn house_style_completed_cells_match_status_token() {
        assert_eq!(
            status_clean(CELL_0056),
            "**Completed** 2026-09-18 - PR **#57** squash `aa5f142`"
        );
        assert_eq!(
            status_clean(CELL_0057),
            "**Completed** 2026-09-22 - PR **#58** squash `075f239b32f574301382dbcffee6588853807977`"
        );
        for cell in [CELL_0053, CELL_0054, CELL_0055, CELL_0056, CELL_0057] {
            assert!(is_completed(cell), "{cell}");
        }
        assert!(is_completed("**Completed**"));
        assert!(is_completed("Completed"));
        assert!(is_completed("Completed - pending"));
        assert!(is_in_progress(CELL_0060_IN_PROGRESS));
        assert!(is_in_progress("**In progress**"));
        assert!(!is_completed("Completedish"));
        assert!(!is_completed("Not Completed"));
        assert!(!is_completed("Complete"));
        assert!(!is_in_progress("In Progressed"));
        assert!(!is_completed("**Ready — not started**"));
        assert!(!is_completed("**Cancelled**"));
        assert!(!is_completed("**Absorbed — 0058 `204a258`**"));
        assert!(is_absorbed("**Absorbed**"));
        assert!(is_absorbed("**Absorbed — 0058 `204a258`**"));
        assert!(is_cancelled("**Cancelled**"));
        assert!(is_cancelled("**Cancelled** - note"));
        assert!(!is_absorbed("Absorbedish"));
        assert!(!is_cancelled("**In progress**"));
        assert!(is_terminal(
            "**Completed** 2026-09-26 — PR **#60** squash `0d8244a`"
        ));
        assert!(is_terminal("**Absorbed**"));
        assert!(is_terminal("**Cancelled** - note"));
        assert!(!is_terminal("**In progress** PR #60"));
        assert!(!is_terminal("**Ready — not started**"));
        assert!(!is_eligible_ready("**Ready — not started** (owner only)"));
        assert!(is_eligible_ready("**Ready — not started**"));
    }

    #[test]
    fn house_style_completed_sticky_row_dropped_picker_returns_ready() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(
            ws,
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0056-Done | `.` | **Completed** 2026-09-18 — PR **#57** squash `aa5f142` | shipped |\n\
             | 0062-Go | `.` | **Ready — not started** | go |\n",
        );
        mkdir_track(ws, "0056-Done");
        mkdir_track(ws, "0062-Go");
        let r = rec(ws);
        let rows = load_track_rows(&r).expect("rows");
        assert!(track_row_completed(&rows, "0056"));
        assert!(!track_row_completed(&rows, "0062"));
        let out = capture_sticky_ready_ids(&r, &["0056".into()]);
        assert_eq!(out, vec!["0062".to_string()]);
        assert_eq!(
            pick_next_ready_excluding_sticky(&r, &[], None, &["0056".into()]).unwrap(),
            "0062"
        );
    }

    #[test]
    fn detailed_in_progress_sticky_row_is_dropped() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(
            ws,
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0060-Dup | `.` | **In progress** PR #60 | mid |\n\
             | 0062-Go | `.` | **Ready — not started** | go |\n",
        );
        mkdir_track(ws, "0060-Dup");
        mkdir_track(ws, "0062-Go");
        let r = rec(ws);
        let out = capture_sticky_ready_ids(&r, &["0060".into()]);
        assert_eq!(out, vec!["0062".to_string()]);
        assert_eq!(
            pick_next_ready_excluding_sticky(&r, &[], None, &["0060".into()]).unwrap(),
            "0062"
        );
    }

    #[test]
    fn house_style_completed_sticky_only_empty_walk() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(
            ws,
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0056-Done | `.` | **Completed** 2026-09-18 — PR **#57** squash `aa5f142` | shipped |\n",
        );
        mkdir_track(ws, "0056-Done");
        let r = rec(ws);
        let rows = load_track_rows(&r).expect("rows");
        assert!(track_row_completed(&rows, "0056"));
        let out = capture_sticky_ready_ids(&r, &["0056".into()]);
        assert!(out.is_empty(), "{out:?}");
        let err = pick_next_ready_excluding_sticky(&r, &[], None, &["0056".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("no Ready"), "{err}");
        assert!(err.contains("pass --track"), "{err}");
        assert!(!err.contains("0056"), "{err}");
    }

    #[test]
    fn should_pick_unset_true_stopped_and_failure_false() {
        assert!(should_pick_next_ready(&pick_state(
            None,
            RunStatus::Idle,
            None,
            None,
            "idle: no run"
        )));
        assert!(should_pick_next_ready(&pick_state(
            Some(""),
            RunStatus::Idle,
            None,
            None,
            "idle: no run"
        )));
        assert!(!should_pick_next_ready(&pick_state(
            Some("0001"),
            RunStatus::Stopped,
            None,
            None,
            crate::state::STOP_LAST_EVENT
        )));
        assert!(!should_pick_next_ready(&pick_state(
            Some("0001"),
            RunStatus::Idle,
            None,
            Some(FailureClass::Timeout),
            "failure"
        )));
        assert!(should_pick_next_ready(&pick_state(
            Some("0001"),
            RunStatus::Idle,
            None,
            None,
            LAST_EVENT_BACKLOG_CLEAR
        )));
        assert!(!should_pick_next_ready(&pick_state(
            None,
            RunStatus::Running,
            None,
            None,
            "run: started canonical_v1"
        )));
        assert!(!should_pick_next_ready(&pick_state(
            None,
            RunStatus::Paused,
            None,
            None,
            "pause: hold"
        )));
    }

    #[test]
    fn idle_after_invalid_next_track_retains() {
        assert!(!should_pick_next_ready(&pick_state(
            Some("0001"),
            RunStatus::Idle,
            None,
            None,
            "workflow: invalid next_track 9999"
        )));
    }

    #[test]
    fn parse_4col_coordinator_skips_vocab_picks_ready() {
        let md = "\
### Status vocabulary\n\
\n\
| Status | Meaning |\n\
|--------|--------|\n\
| Ready — not started | Full spec |\n\
| Completed | Done |\n\
\n\
## Tracks\n\
\n\
| Track | Execution path | Status | Summary |\n\
| --- | --- | --- | --- |\n\
| [0001-Done](0001-Done/spec.md) | `.` | **Completed** | shipped |\n\
| [0030-Start](0030-Start/spec.md) | `.` | **Ready — not started** | pick me |\n\
| [0031-Later](0031-Later/spec.md) | `.` | **Proposed — placeholder, needs full spec/plan pass** | no |\n\
";
        let rows = parse_conductor_md(md).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].id, "0030");
        assert!(is_eligible_ready(&rows[1].status_raw));
        assert!(!is_eligible_ready(&rows[0].status_raw));
        assert!(!is_eligible_ready(&rows[2].status_raw));
    }

    #[test]
    fn parse_blank_line_between_rows_keeps_later_ready() {
        let md = "\
| Track | Execution path | Status | Summary |\n\
| --- | --- | --- | --- |\n\
| [0001-Done](0001-Done/spec.md) | `.` | **Completed** | shipped |\n\
\n\
| [0002-Next](0002-Next/spec.md) | `.` | **Ready — not started** | pick me |\n\
";
        let rows = parse_conductor_md(md).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].id, "0002");
        assert!(is_eligible_ready(&rows[1].status_raw));
    }

    #[test]
    fn parse_thematic_break_between_rows_keeps_later_ready() {
        for hr in ["---", "***", "___", "- - -", "   ---"] {
            let md = format!(
                "\
| Track | Execution path | Status | Summary |\n\
| --- | --- | --- | --- |\n\
| [0001-Done](0001-Done/spec.md) | `.` | **Completed** | shipped |\n\
{hr}\n\
| [0002-Next](0002-Next/spec.md) | `.` | **Ready — not started** | pick me |\n\
"
            );
            let rows = parse_conductor_md(&md).unwrap();
            assert_eq!(rows.len(), 2, "{hr}");
            assert_eq!(rows[1].id, "0002", "{hr}");
            assert!(is_eligible_ready(&rows[1].status_raw), "{hr}");
        }
    }

    #[test]
    fn parse_four_space_indented_hr_closes_table() {
        let md = format!(
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0001-Done](0001-Done/spec.md) | `.` | **Completed** | shipped |\n\
             {}\n\
             | [0002-Next](0002-Next/spec.md) | `.` | **Ready — not started** | hidden |\n",
            "    ---"
        );
        let rows = parse_conductor_md(&md).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].id, "0001");
    }

    #[test]
    fn parse_concatenates_later_track_status_table() {
        let md = "\
| Track | Execution path | Status | Summary |\n\
| --- | --- | --- | --- |\n\
| [0001-Done](0001-Done/spec.md) | `.` | **Completed** | shipped |\n\
\n\
## Later cluster\n\
\n\
| Track | Execution path | Status | Summary |\n\
| --- | --- | --- | --- |\n\
| [0009-Ready](0009-Ready/spec.md) | `.` | **Ready — not started** | pick me |\n\
";
        let rows = parse_conductor_md(md).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].id, "0009");
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(ws, md);
        mkdir_track(ws, "0001-Done");
        mkdir_track(ws, "0009-Ready");
        let r = rec(ws);
        assert_eq!(pick_next_ready(&r).unwrap(), "0009");
    }

    #[test]
    fn parse_new_table_after_blank_not_swallowed() {
        let md = "\
| Track | Execution path | Status | Summary |\n\
| --- | --- | --- | --- |\n\
| [0001-Done](0001-Done/spec.md) | `.` | **Completed** | shipped |\n\
\n\
| Track | Execution path | Status | Summary |\n\
| --- | --- | --- | --- |\n\
| [0008-Ready](0008-Ready/spec.md) | `.` | **Ready — not started** | pick me |\n\
";
        let rows = parse_conductor_md(md).unwrap();
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0].id, "0001");
        assert_eq!(rows[1].id, "0008");
        assert!(rows.iter().all(|r| r.id != "Track"));
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(ws, md);
        mkdir_track(ws, "0001-Done");
        mkdir_track(ws, "0008-Ready");
        let r = rec(ws);
        assert_eq!(pick_next_ready(&r).unwrap(), "0008");
    }

    #[test]
    fn parse_new_table_after_thematic_break_not_swallowed() {
        let md = "\
| Track | Execution path | Status | Summary |\n\
| --- | --- | --- | --- |\n\
| [0001-Done](0001-Done/spec.md) | `.` | **Completed** | shipped |\n\
---\n\
| Track | Execution path | Status | Summary |\n\
| --- | --- | --- | --- |\n\
| [0008-Ready](0008-Ready/spec.md) | `.` | **Ready — not started** | pick me |\n\
";
        let rows = parse_conductor_md(md).unwrap();
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0].id, "0001");
        assert_eq!(rows[1].id, "0008");
        assert!(rows.iter().all(|r| r.id != "Track"));
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(ws, md);
        mkdir_track(ws, "0001-Done");
        mkdir_track(ws, "0008-Ready");
        let r = rec(ws);
        assert_eq!(pick_next_ready(&r).unwrap(), "0008");
    }

    #[test]
    fn parse_5col_orca_status_by_header_name() {
        let md = "\
| Track | Kind | Execution | Status | Summary |\n\
| --- | --- | --- | --- | --- |\n\
| 0001-Orca | product | `.` | **Completed** | done |\n\
| 0099-Probe | probe | `.` | **Ready — not started** | probe |\n\
";
        let rows = parse_conductor_md(md).unwrap();
        assert_eq!(rows[1].id, "0099");
        assert!(is_eligible_ready(&rows[1].status_raw));
        assert_eq!(rows[0].status_raw, "**Completed**");
        assert!(is_completed(&rows[0].status_raw));
        assert!(track_row_completed(&rows, "0001"));
        assert!(track_row_completed(&rows, "0001-Orca"));
    }

    #[test]
    fn failure_superseded_completed_then_mismatch() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        write_md(
            dir.path(),
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0038-Done | `.` | **Completed** | shipped |\n",
        );
        let body = "- track_id: 0038\n- run_epoch: 2\n";
        let mut state = RunState::idle(&r.id);
        state.track_id = Some("0038".into());
        state.run_epoch = 2;
        assert_eq!(
            failure_superseded(&r, &state, Some(body)).as_deref(),
            Some(crate::notify::SUPERSEDED_COMPLETED)
        );
        write_md(
            dir.path(),
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0038-Done | `.` | **Ready — not started** | open |\n",
        );
        assert_eq!(failure_superseded(&r, &state, Some(body)), None);
        state.run_epoch = 9;
        assert_eq!(
            failure_superseded(&r, &state, Some(body)).as_deref(),
            Some(crate::notify::SUPERSEDED_MISMATCH)
        );
    }

    #[test]
    fn trailing_notes_skipped_later_exact_ready_wins() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(
            ws,
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0101-Notes | `.` | ✅ **Ready — not started** (planned 2026-07-29; **revised…**) | skip |\n\
             | 0899-Probe | `.` | **Ready — not started** | probe |\n\
             | 0001-Done | `.` | **Completed** | done |\n",
        );
        mkdir_track(ws, "0101-Notes");
        mkdir_track(ws, "0899-Probe");
        mkdir_track(ws, "0001-Done");
        let r = rec(ws);
        assert_eq!(pick_next_ready(&r).unwrap(), "0899");
    }

    #[test]
    fn hitl_slug_and_owner_only_summary_skipped() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(
            ws,
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0011-OwnerHitl | `.` | **Ready — not started** | skip |\n\
             | 0012-Manual | `.` | **Ready — not started** | owner only |\n\
             | 0030-Ok | `.` | **Ready — not started** | go |\n",
        );
        mkdir_track(ws, "0011-OwnerHitl");
        mkdir_track(ws, "0012-Manual");
        mkdir_track(ws, "0030-Ok");
        let r = rec(ws);
        assert_eq!(pick_next_ready(&r).unwrap(), "0030");
    }

    #[test]
    fn escaped_pipe_in_summary_does_not_shift_status() {
        let md = "\
| Track | Execution path | Status | Summary |\n\
| --- | --- | --- | --- |\n\
| 0030-Ready | `.` | **Ready — not started** | schema 1\\|2 block\\|warn\\|info |\n\
";
        let rows = parse_conductor_md(md).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "0030");
        assert!(is_eligible_ready(&rows[0].status_raw));
        assert!(rows[0].summary.contains("schema 1|2"));
        assert!(rows[0].summary.contains("block|warn|info"));
    }

    #[test]
    fn suggested_order_before_table_order_same_fixture() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(
            ws,
            "### Suggested execution order (not numeric)\n\
             \n\
             ```\n\
             0009 LaterFirst\n\
             0006 TableFirst\n\
             ```\n\
             \n\
             | Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0006-TableFirst | `.` | **Ready — not started** | table first |\n\
             | 0009-LaterFirst | `.` | **Ready — not started** | listed first |\n",
        );
        mkdir_track(ws, "0006-TableFirst");
        mkdir_track(ws, "0009-LaterFirst");
        let r = rec(ws);
        assert_eq!(pick_next_ready(&r).unwrap(), "0009");
    }

    #[test]
    fn no_suggested_order_uses_table_order() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(
            ws,
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0006-First | `.` | **Ready — not started** | a |\n\
             | 0009-Second | `.` | **Ready — not started** | b |\n",
        );
        mkdir_track(ws, "0006-First");
        mkdir_track(ws, "0009-Second");
        let r = rec(ws);
        assert_eq!(pick_next_ready(&r).unwrap(), "0006");
    }

    #[test]
    fn parallel_first_appearance_left_to_right() {
        let md = "\
### Suggested execution order (not numeric)\n\
\n\
```\n\
0028 Name ∥ 0029 Name\n\
```\n\
";
        assert_eq!(parse_execution_order(md), vec!["0028", "0029"]);
    }

    #[test]
    fn question_chain_left_to_right() {
        let md = "\
### Suggested execution order (not numeric)\n\
\n\
```\n\
0028 Name ? 0029 Name ? 0030 Name\n\
```\n\
";
        assert_eq!(parse_execution_order(md), vec!["0028", "0029", "0030"]);
    }

    #[test]
    fn iso_date_does_not_inject_id_2026() {
        let md = "\
### Suggested execution order (not numeric)\n\
\n\
```\n\
0001 Foo\n\
# planned 2026-08-22\n\
0002 Bar\n\
```\n\
";
        assert_eq!(parse_execution_order(md), vec!["0001", "0002"]);
    }

    #[test]
    fn ready_without_dir_skips_to_next() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(
            ws,
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0002-Missing | `.` | **Ready — not started** | no dir |\n\
             | 0003-Present | `.` | **Ready — not started** | has dir |\n",
        );
        mkdir_track(ws, "0003-Present");
        let r = rec(ws);
        assert_eq!(pick_next_ready(&r).unwrap(), "0003");
    }

    #[test]
    fn proposed_only_errors() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(
            ws,
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0044-Later | `.` | **Proposed — placeholder, needs full spec/plan pass** | no |\n",
        );
        mkdir_track(ws, "0044-Later");
        let r = rec(ws);
        let err = pick_next_ready(&r).unwrap_err().to_string();
        assert!(err.contains("no Ready"), "{err}");
        assert!(err.contains("pass --track"), "{err}");
    }

    #[test]
    fn missing_file_errors() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        let err = pick_next_ready(&r).unwrap_err().to_string();
        assert!(err.contains("no conductor.md"), "{err}");
        assert!(err.contains("pass --track"), "{err}");
    }

    #[test]
    fn ready_without_any_dir_errors() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(
            ws,
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0002-A | `.` | **Ready — not started** | a |\n\
             | 0003-B | `.` | **Ready — not started** | b |\n",
        );
        let r = rec(ws);
        let err = pick_next_ready(&r).unwrap_err().to_string();
        assert!(err.contains("0002"), "{err}");
        assert!(err.contains("0003"), "{err}");
        assert!(err.contains("no matching conductor directory"), "{err}");
    }

    #[test]
    fn first_qualifying_table_zero_valid_rows_continues() {
        let md = "\
| Track | Execution path | Status | Summary |\n\
| --- | --- | --- | --- |\n\
| not-an-id | `.` | **Ready — not started** | skip |\n\
| a\\|b\\|c | `.` |\n\
\n\
| Track | Execution path | Status | Summary |\n\
| --- | --- | --- | --- |\n\
| 0030-Later | `.` | **Ready — not started** | later table wins |\n\
";
        let rows = parse_conductor_md(md).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "0030");
    }

    #[test]
    fn all_track_tables_zero_valid_rows_is_parse_error() {
        let md = "\
| Track | Execution path | Status | Summary |\n\
| --- | --- | --- | --- |\n\
| not-an-id | `.` | **Ready — not started** | skip |\n\
\n\
| Status | Meaning |\n\
|--------|--------|\n\
| Ready — not started | Full spec |\n\
";
        let err = parse_conductor_md(md).unwrap_err().to_string();
        assert!(err.contains("could not parse"), "{err}");
    }

    #[test]
    fn terminal_token_is_canonical() {
        assert_eq!(
            terminal_token("**Completed** 2026-10-23 - PR **#135** squash `018a8ef`"),
            Some("Completed")
        );
        assert_eq!(terminal_token("**Absorbed** into 0107"), Some("Absorbed"));
        assert_eq!(
            terminal_token("**Cancelled** - replaced"),
            Some("Cancelled")
        );
        assert_eq!(terminal_token("**Ready - not started**"), None);
        assert_eq!(terminal_token("**Proposed**"), None);
        assert_eq!(terminal_token("**Blocked** - prerequisite"), None);
    }

    #[test]
    fn parser_parts_empty_is_zero_rows_not_an_error() {
        let (rows, shaped) = parse_conductor_md_parts("# tracks\n");
        assert!(rows.is_empty());
        assert!(!shaped);
        assert!(view_is_scaffolding_only("# tracks\n"));

        let header_only = "| Track | Execution path | Status | Summary |\n\
                           | --- | --- | --- | --- |\n";
        let (rows, shaped) = parse_conductor_md_parts(header_only);
        assert!(rows.is_empty());
        assert!(shaped);
        assert!(!view_is_scaffolding_only(header_only));

        let prose = "# Tracks\n\nThis project has no track registry yet.\n";
        let (rows, shaped) = parse_conductor_md_parts(prose);
        assert!(rows.is_empty());
        assert!(!shaped);
        assert!(!view_is_scaffolding_only(prose));

        assert!(view_is_scaffolding_only(""));
        assert!(view_is_scaffolding_only("# Tracks\n\n---\n\n> note\n"));
        assert!(!view_is_scaffolding_only("# Tracks\nplain prose\n"));
        // Fenced content is prose: only the delimiter is scaffolding.
        assert!(!view_is_scaffolding_only(
            "# Tracks\n```\nnot a registry\n```\n"
        ));
        assert!(view_is_scaffolding_only("# Tracks\n```\n```\n"));
        // A fence-like line inside a fence is content, not a delimiter.
        assert!(!view_is_scaffolding_only(
            "# Tracks\n```\n```not a delimiter\n```\n"
        ));
        // A fenced fake Track/Status table is code, not a registry (codex P2, pass 3).
        let fenced_table =
            "# tracks\n```\n| Track | Status |\n| --- | --- |\n| 0099 | **Completed** |\n```\n";
        let (rows, shaped) = parse_conductor_md_parts(fenced_table);
        assert!(rows.is_empty(), "{rows:?}");
        assert!(!shaped, "a fenced table is not registry-shaped");
        assert!(!view_is_scaffolding_only(fenced_table));
        // A real table after a fence still parses.
        let after_fence = "# notes\n```\nnot a registry\n```\n\
                           | Track | Status |\n| --- | --- |\n\
                           | 0099 | **Ready - not started** |\n";
        let (rows, shaped) = parse_conductor_md_parts(after_fence);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(shaped);
        // Indented code (4+ columns) is code, not registry text (codex P2, pass 4).
        let indented_table =
            "    | Track | Status |\n    | --- | --- |\n    | 0099 | **Completed** |\n";
        let (rows, shaped) = parse_conductor_md_parts(indented_table);
        assert!(rows.is_empty(), "{rows:?}");
        assert!(!shaped);
        assert!(!view_is_scaffolding_only(indented_table));
    }

    #[test]
    fn parse_conductor_md_keeps_zero_row_error() {
        let err = parse_conductor_md("# tracks\n").unwrap_err().to_string();
        assert!(err.contains("could not parse"), "{err}");
        let err = parse_conductor_md("").unwrap_err().to_string();
        assert!(err.contains("could not parse"), "{err}");
    }

    #[test]
    fn pick_ready_without_plan_md_still_starts() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(
            ws,
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0050-EmptyDir | `.` | **Ready — not started** | no plan.md |\n",
        );
        mkdir_track(ws, "0050-EmptyDir");
        assert!(
            !ws.join("conductor")
                .join("0050-EmptyDir")
                .join("plan.md")
                .exists()
        );
        let r = rec(ws);
        assert_eq!(pick_next_ready(&r).unwrap(), "0050");
    }

    #[test]
    fn write_ready_fixture_picks_id() {
        let dir = tempdir().unwrap();
        write_ready_fixture(dir.path(), "0030").unwrap();
        let r = rec(dir.path());
        assert_eq!(pick_next_ready(&r).unwrap(), "0030");
    }

    #[test]
    fn custom_ready_alias_picks_exact_row() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(
            ws,
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0042-Custom | `.` | **Ready — full plan @ 072399b6** | go |\n\
             | 0030-Default | `.` | **Ready — not started** | default still eligible |\n",
        );
        mkdir_track(ws, "0042-Custom");
        mkdir_track(ws, "0030-Default");
        let mut r = rec(ws);
        r.ready_aliases = vec!["Ready — full plan @ 072399b6".into()];
        assert_eq!(pick_next_ready(&r).unwrap(), "0042");
    }

    #[test]
    fn unknown_status_is_ignored_not_parse_error() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(
            ws,
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0007-Draft | `.` | **Draft** | ignore |\n\
             | 0030-Ok | `.` | **Ready — not started** | go |\n",
        );
        mkdir_track(ws, "0007-Draft");
        mkdir_track(ws, "0030-Ok");
        let r = rec(ws);
        assert_eq!(pick_next_ready(&r).unwrap(), "0030");
    }

    #[test]
    fn configured_aliases_empty_pick_mentions_list() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(
            ws,
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0001-Done | `.` | **Completed** | done |\n",
        );
        mkdir_track(ws, "0001-Done");
        let mut r = rec(ws);
        r.ready_aliases = vec!["Ready — full plan @ 072399b6".into()];
        let err = pick_next_ready(&r).unwrap_err().to_string();
        assert!(err.contains("no eligible Ready track"), "{err}");
        assert!(err.contains("Ready — full plan @ 072399b6"), "{err}");
    }

    #[test]
    fn default_empty_pick_keeps_legacy_wording() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(
            ws,
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0001-Done | `.` | **Completed** | done |\n",
        );
        mkdir_track(ws, "0001-Done");
        let err = pick_next_ready(&rec(ws)).unwrap_err().to_string();
        assert_eq!(
            err,
            "no Ready — not started track in conductor.md; pass --track <id>"
        );
    }

    #[test]
    fn hitl_skipped_even_when_alias_matches() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(
            ws,
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0042-Hitl | `.` | **Ready — full plan @ 072399b6** | owner only |\n\
             | 0030-Ok | `.` | **Ready — not started** | go |\n",
        );
        mkdir_track(ws, "0042-Hitl");
        mkdir_track(ws, "0030-Ok");
        let mut r = rec(ws);
        r.ready_aliases = vec!["Ready — full plan @ 072399b6".into()];
        assert_eq!(pick_next_ready(&r).unwrap(), "0030");
    }

    #[test]
    fn prefix_does_not_match_alias() {
        assert!(!is_eligible_ready_in(
            "Ready — full plan @ 072399b6",
            &["Ready — full plan"]
        ));
        assert!(is_eligible_ready_in(
            "Ready — full plan @ 072399b6",
            &["Ready — full plan @ 072399b6"]
        ));
    }

    #[test]
    fn nostart_comment_in_each_column_parses_ready() {
        let cases = [
            "| [0030-Ok](0030-Ok/spec.md) <!-- nostart --> | `.` | **Ready — not started** | go |",
            "| [0030-Ok](0030-Ok/spec.md) | `.` | **Ready — not started** <!-- nostart --> | go |",
            "| [0030-Ok](0030-Ok/spec.md) | `.` | **Ready — not started** | go <!-- nostart --> |",
            "| [0030-Ok](0030-Ok/spec.md) | `.` <!-- nostart --> | **Ready — not started** | go |",
        ];
        for row in cases {
            let md = format!(
                "| Track | Execution path | Status | Summary |\n\
                 | --- | --- | --- | --- |\n\
                 {row}\n"
            );
            let rows = parse_conductor_md(&md).unwrap();
            assert_eq!(rows.len(), 1, "{row}");
            assert_eq!(rows[0].id, "0030");
            assert!(rows[0].nostart, "{row}");
            assert!(is_eligible_ready(&rows[0].status_raw), "{row}");
        }
        let plain = "\
| Track | Execution path | Status | Summary |\n\
| --- | --- | --- | --- |\n\
| [0030-Ok](0030-Ok/spec.md) | `.` | **Ready — not started** | go |\n";
        let rows = parse_conductor_md(plain).unwrap();
        assert!(!rows[0].nostart);
    }

    #[test]
    fn nostart_own_line_ends_table() {
        let md = "\
| Track | Execution path | Status | Summary |\n\
| --- | --- | --- | --- |\n\
| [0030-Ok](0030-Ok/spec.md) | `.` | **Ready — not started** | go |\n\
<!-- nostart -->\n\
| [0031-Later](0031-Later/spec.md) | `.` | **Ready — not started** | after |\n";
        let rows = parse_conductor_md(md).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "0030");
        assert!(!rows[0].nostart);
    }

    #[test]
    fn nostart_row_skipped_by_pick_completed_ignores_flag() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(
            ws,
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0029-Lock](0029-Lock/spec.md) | `.` | **Ready — not started** <!-- nostart --> | skip |\n\
             | [0030-Ok](0030-Ok/spec.md) | `.` | **Ready — not started** | go |\n\
             | [0028-Done](0028-Done/spec.md) | `.` | **Completed** <!-- nostart --> | done |\n",
        );
        mkdir_track(ws, "0029-Lock");
        mkdir_track(ws, "0030-Ok");
        mkdir_track(ws, "0028-Done");
        let r = rec(ws);
        assert_eq!(pick_next_ready(&r).unwrap(), "0030");
        let rows = parse_conductor_md(
            &std::fs::read_to_string(ws.join("conductor").join("conductor.md")).unwrap(),
        )
        .unwrap();
        assert!(track_row_completed(&rows, "0028"));
        assert!(track_row_nostart(&r, "0029"));
        assert!(!track_row_nostart(&r, "0030"));
    }

    #[test]
    fn should_pick_never_is_false() {
        let mut s = pick_state(
            Some("0001"),
            RunStatus::Idle,
            None,
            None,
            LAST_EVENT_BACKLOG_CLEAR,
        );
        s.auto_start = AutoStartPolicy::Never;
        assert!(!should_pick_next_ready(&s));
    }

    fn two_ready_ws(ws: &Path) -> ProjectRecord {
        write_md(
            ws,
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0001-One | `.` | **Ready — not started** | a |\n\
             | 0002-Two | `.` | **Ready — not started** | b |\n",
        );
        mkdir_track(ws, "0001-One");
        mkdir_track(ws, "0002-Two");
        let mut r = rec(ws);
        r.execution_repo = Some(ws.to_path_buf());
        r
    }

    #[test]
    fn pick_excluding_probe_skips_merged_title() {
        let dir = tempdir().unwrap();
        let r = two_ready_ws(dir.path());
        let probe = crate::workflow::shipped::ScriptedMergedProbe::found("0001", 56);
        assert_eq!(
            pick_next_ready_excluding(&r, &[], Some(&probe)).unwrap(),
            "0002"
        );
    }

    #[test]
    fn pick_excluding_probe_err_fail_opens() {
        let dir = tempdir().unwrap();
        let r = two_ready_ws(dir.path());
        let probe = crate::workflow::shipped::ScriptedMergedProbe::err("0001", "gh auth required");
        assert_eq!(
            pick_next_ready_excluding(&r, &[], Some(&probe)).unwrap(),
            "0001"
        );
    }

    #[test]
    fn pick_excluding_skip_list() {
        let dir = tempdir().unwrap();
        let r = two_ready_ws(dir.path());
        assert_eq!(
            pick_next_ready_excluding(&r, &["0001".into()], None).unwrap(),
            "0002"
        );
    }

    #[test]
    fn pick_excluding_only_merged_ready_is_empty() {
        let dir = tempdir().unwrap();
        write_md(
            dir.path(),
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0001-One | `.` | **Ready — not started** | a |\n",
        );
        mkdir_track(dir.path(), "0001-One");
        let mut r = rec(dir.path());
        r.execution_repo = Some(dir.path().to_path_buf());
        let probe = crate::workflow::shipped::ScriptedMergedProbe::found("0001", 56);
        let err = pick_next_ready_excluding(&r, &[], Some(&probe))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no Ready"), "{err}");
    }

    fn proposed_md(id: &str, slug: &str) -> String {
        format!(
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | {id}-Done | `.` | **Completed** | done |\n\
             | {id2}-{slug} | `.` | **Proposed — placeholder, needs full spec/plan pass** | later |\n",
            id = "0001",
            id2 = id,
            slug = slug
        )
    }

    #[test]
    fn sticky_skips_explicit_non_ready_and_picks_later_ready() {
        let cells = [
            "**Proposed — placeholder, needs full spec/plan pass**",
            "**Blocked — owner review pending**",
            "**Cancelled** - note",
            "**Absorbed — 0058 `204a258`**",
        ];
        for cell in cells {
            let dir = tempdir().unwrap();
            let ws = dir.path();
            write_md(
                ws,
                &format!(
                    "| Track | Execution path | Status | Summary |\n\
                     | --- | --- | --- | --- |\n\
                     | 0002-Held | `.` | {cell} | held |\n\
                     | 0003-Go | `.` | **Ready — not started** | go |\n"
                ),
            );
            mkdir_track(ws, "0002-Held");
            mkdir_track(ws, "0003-Go");
            let r = rec(ws);
            assert_eq!(
                pick_next_ready_excluding_sticky(&r, &[], None, &["0002".into()]).unwrap(),
                "0003",
                "{cell}"
            );
        }
    }

    #[test]
    fn sticky_does_not_pick_never_ready_proposed() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(ws, &proposed_md("0002", "Next"));
        mkdir_track(ws, "0001-Done");
        mkdir_track(ws, "0002-Next");
        let r = rec(ws);
        let err = pick_next_ready_excluding_sticky(&r, &[], None, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("Ready"), "{err}");
        assert!(err.contains("--track"), "{err}");
    }

    #[test]
    fn sticky_skips_hitl_even_if_listed() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(
            ws,
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0011-OwnerHitl | `.` | **Proposed — placeholder, needs full spec/plan pass** | skip |\n\
             | 0030-Ok | `.` | **Ready — not started** | go |\n",
        );
        mkdir_track(ws, "0011-OwnerHitl");
        mkdir_track(ws, "0030-Ok");
        let r = rec(ws);
        assert_eq!(
            pick_next_ready_excluding_sticky(&r, &[], None, &["0011".into()]).unwrap(),
            "0030"
        );
    }

    #[test]
    fn sticky_skips_nostart_even_if_listed() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(
            ws,
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0029-Lock](0029-Lock/spec.md) | `.` | **Proposed — placeholder, needs full spec/plan pass** <!-- nostart --> | skip |\n\
             | [0030-Ok](0030-Ok/spec.md) | `.` | **Ready — not started** | go |\n",
        );
        mkdir_track(ws, "0029-Lock");
        mkdir_track(ws, "0030-Ok");
        let r = rec(ws);
        assert_eq!(
            pick_next_ready_excluding_sticky(&r, &[], None, &["0029".into()]).unwrap(),
            "0030"
        );
    }

    #[test]
    fn sticky_skips_completed_even_if_listed() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(
            ws,
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0001-Done | `.` | **Completed** | done |\n\
             | 0002-Next | `.` | **Ready — not started** | go |\n",
        );
        mkdir_track(ws, "0001-Done");
        mkdir_track(ws, "0002-Next");
        let r = rec(ws);
        assert_eq!(
            pick_next_ready_excluding_sticky(&r, &[], None, &["0001".into()]).unwrap(),
            "0002"
        );
    }

    #[test]
    fn sticky_skips_in_progress_even_if_listed() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(
            ws,
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0001-Live | `.` | **In progress** | wip |\n\
             | 0002-Next | `.` | **Ready — not started** | go |\n",
        );
        mkdir_track(ws, "0001-Live");
        mkdir_track(ws, "0002-Next");
        let r = rec(ws);
        assert_eq!(
            pick_next_ready_excluding_sticky(&r, &[], None, &["0001".into()]).unwrap(),
            "0002"
        );
    }

    #[test]
    fn sticky_ready_aliases_capture_and_pick() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(
            ws,
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0042-Queued | `.` | **Ready — full plan @ 072399b6** | go |\n",
        );
        mkdir_track(ws, "0042-Queued");
        let mut r = rec(ws);
        r.ready_aliases = vec!["Ready — full plan @ 072399b6".into()];
        let sticky = capture_sticky_ready_ids(&r, &[]);
        assert_eq!(sticky, vec!["0042".to_string()]);
        write_md(
            ws,
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0042-Queued | `.` | **Ready — folded @ sha1234** | later |\n",
        );
        assert_eq!(
            pick_next_ready_excluding_sticky(&r, &[], None, &sticky).unwrap(),
            "0042"
        );
    }

    #[test]
    fn sticky_merged_probe_still_skips() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(
            ws,
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0001-One | `.` | **Ready — folded @ sha1234** | a |\n\
             | 0002-Two | `.` | **Ready — not started** | b |\n",
        );
        mkdir_track(ws, "0001-One");
        mkdir_track(ws, "0002-Two");
        let mut r = rec(ws);
        r.execution_repo = Some(ws.to_path_buf());
        let probe = crate::workflow::shipped::ScriptedMergedProbe::found("0001", 56);
        assert_eq!(
            pick_next_ready_excluding_sticky(&r, &[], Some(&probe), &["0001".into()]).unwrap(),
            "0002"
        );
    }

    #[test]
    fn sticky_capture_union_drops_prev_proposed_adds_new_ready_drops_completed() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(
            ws,
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | 0001-Done | `.` | **Completed** | done |\n\
             | 0002-Later | `.` | **Proposed — placeholder, needs full spec/plan pass** | later |\n\
             | 0003-Go | `.` | **Ready — not started** | go |\n",
        );
        let r = rec(ws);
        let out = capture_sticky_ready_ids(&r, &["0001".into(), "0002".into()]);
        assert_eq!(out, vec!["0003".to_string()]);
    }

    #[test]
    fn sticky_capture_parse_error_keeps_prev() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        write_md(ws, "# not a registry\n");
        let r = rec(ws);
        let prev = vec!["0002".to_string()];
        assert_eq!(capture_sticky_ready_ids(&r, &prev), prev);
        let missing_dir = tempdir().unwrap();
        let missing = rec(missing_dir.path());
        assert_eq!(capture_sticky_ready_ids(&missing, &prev), prev);
    }

    #[test]
    fn parse_leading_id_accepts_an_alpha_prefixed_id() {
        // A registry may write `| T374 | ... |`. Requiring the cell to start with four digits
        // made every row of such a registry unreadable, which stops the fold -> implement gate on
        // every track in that project (0075). The digit run is the id; the engine launches `374`.
        assert_eq!(
            parse_leading_id("T374"),
            Some(("374".to_string(), String::new()))
        );
        assert_eq!(
            parse_leading_id("T374-nightly-symbol-backlog"),
            Some(("374".to_string(), "nightly-symbol-backlog".to_string()))
        );
        // The bare four-digit form is unchanged, including its four-digit floor.
        assert_eq!(
            parse_leading_id("0374"),
            Some(("0374".to_string(), String::new()))
        );
        assert_eq!(
            parse_leading_id("374"),
            None,
            "bare form still needs four digits"
        );
        assert_eq!(parse_leading_id("T"), None, "prefix with no digits");
        assert_eq!(parse_leading_id("Tabc"), None, "prefix with no digits");
        assert_eq!(
            parse_leading_id("0040-Slug-With-Dashes"),
            Some(("0040".to_string(), "Slug-With-Dashes".to_string()))
        );
    }

    #[test]
    fn parse_conductor_md_reads_an_alpha_prefixed_registry() {
        // End-to-end on the exact shape found in AI-Brains/conductor/conductor.md. Before this
        // fix it returned Err("could not parse track registry ..."), which the fold gate
        // surfaces as `fold withheld (cannot read track row)` and Stops the run.
        let text = "| Track | Name | Status | Owner | Spec | Description |\n\
                    | --- | --- | --- | --- | --- | --- |\n\
                    | T374 | AGY stop-hook quoting | **Ready** | Grok | [T374](tracks/trackT374-x/spec.md) | desc |\n";
        let rows = parse_conductor_md(text).expect("an alpha-prefixed registry must parse");
        assert_eq!(rows.len(), 1, "the T-form row must be read");
        assert_eq!(rows[0].id, "374");
        assert!(
            rows[0].status_raw.contains("Ready"),
            "status cell was {}",
            rows[0].status_raw
        );
    }
}

#[cfg(test)]
mod tests_0103 {
    use super::*;
    use crate::registry::ProjectRecord;
    use tempfile::tempdir;

    fn rec(path: &std::path::Path) -> ProjectRecord {
        ProjectRecord {
            id: uuid::Uuid::new_v4().to_string(),
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
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now(),
        }
    }

    fn write_conductor(ws: &std::path::Path, row: &str) {
        let cond = ws.join("conductor");
        std::fs::create_dir_all(&cond).unwrap();
        let md = format!(
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             {row}\n"
        );
        std::fs::write(cond.join("conductor.md"), md).unwrap();
    }

    #[test]
    fn ready_pick_miss_classification() {
        // (1) Missing conductor.md is a setup failure, not an empty backlog.
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        let err = pick_next_ready(&r).unwrap_err();
        assert!(err.to_string().contains("no conductor.md at"), "{err}");
        assert!(!ready_pick_miss_is_benign(&err));

        // (2) Only a Completed row is a benign empty backlog.
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        write_conductor(dir.path(), "| 0029-Done | `.` | **Completed** | shipped |");
        let err = pick_next_ready(&r).unwrap_err();
        assert!(
            err.to_string().contains("no Ready — not started track"),
            "{err}"
        );
        assert!(ready_pick_miss_is_benign(&err));

        // (3) Custom aliases with no match take the alias error branch.
        let dir = tempdir().unwrap();
        let mut r = rec(dir.path());
        r.ready_aliases = vec!["Ready - full plan @ 0020".into()];
        write_conductor(dir.path(), "| 0029-Done | `.` | **Completed** | shipped |");
        let err = pick_next_ready(&r).unwrap_err();
        assert!(
            err.to_string()
                .contains("no eligible Ready track (aliases: "),
            "{err}"
        );
        assert!(ready_pick_miss_is_benign(&err));

        // (4) A Ready row without its conductor directory is a recorded setup failure.
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        write_conductor(
            dir.path(),
            "| 0021-Missing | `.` | **Ready — not started** | fixture |",
        );
        let err = pick_next_ready(&r).unwrap_err();
        assert!(
            err.to_string().contains("no matching conductor directory"),
            "{err}"
        );
        assert!(!ready_pick_miss_is_benign(&err));

        // (5) An Io read error is never benign by construction.
        let io = crate::error::CoordinatorError::Io(std::io::Error::other("share"));
        assert!(!ready_pick_miss_is_benign(&io));
    }
}
// 0109 read-seam tests: the decision legs read the store, never a stale view.

#[cfg(test)]
mod tests_0109 {
    use super::*;
    use crate::error::CoordinatorError;
    use crate::layout::LayoutProfile;
    use crate::registry::AutoStartPolicy;
    use crate::state::RunState;
    use crate::workflow::shipped::MergedTrackProbe;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use tempfile::{TempDir, tempdir};

    const READY: &str = "**Ready — not started**";
    const PROPOSED_PLACEHOLDER: &str = "**Proposed — placeholder, needs full spec/plan pass**";

    fn rec(ws: &Path, id: &str) -> ProjectRecord {
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
        fn import(&self) -> crate::registry_store::ImportReport {
            crate::registry_store::import_at(
                &self.dir(),
                crate::registry_store::ImportOpts::default(),
            )
            .unwrap()
        }
        fn import_force(&self) -> crate::registry_store::ImportReport {
            crate::registry_store::import_at(
                &self.dir(),
                crate::registry_store::ImportOpts {
                    dry_run: false,
                    force: true,
                },
            )
            .unwrap()
        }
        fn track(&self, dir_name: &str) {
            let dir = self.dir().join(dir_name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("spec.md"), b"# spec\n").unwrap();
            std::fs::write(dir.join("plan.md"), b"# real plan\n").unwrap();
        }
        fn status_log(&self) -> String {
            std::fs::read_to_string(self.record.path.join("status.md")).unwrap_or_default()
        }
    }

    fn fixture(id: &str) -> Fx {
        let ws = tempdir().unwrap();
        let record = rec(ws.path(), id);
        std::fs::create_dir_all(ws.path().join("conductor")).unwrap();
        Fx { _ws: ws, record }
    }

    fn row(key: &str, status: &str, summary: &str) -> String {
        format!("| [{key}]({key}/spec.md) | `.` | {status} | {summary} |\n")
    }

    fn registry(rows: &[String]) -> String {
        let mut body = String::from(
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n",
        );
        for r in rows {
            body.push_str(r);
        }
        body
    }

    /// 0120 `m-2`: the picker's drift text is the byte guard for the shared helper.
    fn assert_drift_error_text(err: &CoordinatorError) {
        let msg = err.to_string();
        assert!(msg.starts_with("track registry view drifted"), "{msg}");
        assert!(msg.contains("(store "), "expected/actual shape: {msg}");
        assert!(msg.contains(", view "), "expected/actual shape: {msg}");
        assert!(
            msg.contains("registry import --conductor-dir")
                && msg.contains("--force` to adopt the edit"),
            "adopt recovery missing: {msg}"
        );
        assert!(
            msg.contains("registry render --conductor-dir")
                && msg.contains("--force` to discard it"),
            "discard recovery missing: {msg}"
        );
    }

    struct FixedProbe {
        merged: Vec<String>,
    }

    impl MergedTrackProbe for FixedProbe {
        fn merged_pr_for_track(
            &self,
            _cwd: &Path,
            numeric_id: &str,
        ) -> crate::error::Result<Option<u64>> {
            Ok(self
                .merged
                .iter()
                .any(|id| id == numeric_id)
                .then_some(1u64))
        }
    }

    struct FeatureHome {
        _home: TempDir,
        _guard: crate::config::TestEnvGuard,
    }

    fn isolate_features() -> FeatureHome {
        let names = crate::features::test_feature_env_names();
        let guard = crate::config::TestEnvGuard::capture(&names);
        let home = tempdir().unwrap();
        for name in &names {
            unsafe {
                std::env::remove_var(name);
            }
        }
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        crate::features::reset_feature_warnings_for_tests();
        crate::registry_store::reset_drift_warnings_for_tests();
        FeatureHome {
            _home: home,
            _guard: guard,
        }
    }

    #[test]
    fn pick_answers_from_store_with_a_synced_view() {
        let fx = fixture("p-17");
        fx.write_view(&registry(&[row("0001", READY, "go")]));
        fx.track("0001");
        fx.import();
        assert_eq!(
            pick_next_ready_excluding_sticky(&fx.record, &[], None, &[]).unwrap(),
            "0001"
        );
        let store = crate::registry_store::load(&fx.record).unwrap().unwrap();
        assert_eq!(crate::registry_store::render(&store), fx.read_view());
    }

    #[test]
    fn pick_succeeds_with_no_view() {
        let fx = fixture("p-18");
        fx.write_view(&registry(&[row("0001", READY, "go")]));
        fx.track("0001");
        fx.import();
        std::fs::remove_file(fx.view()).unwrap();
        assert_eq!(
            pick_next_ready_excluding_sticky(&fx.record, &[], None, &[]).unwrap(),
            "0001"
        );
    }

    #[test]
    fn pick_follows_store_execution_order() {
        let fx = fixture("p-19");
        fx.write_view(
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0001-One](0001-One/spec.md) | `.` | **Ready — not started** | a |\n\
             | [0002-Two](0002-Two/spec.md) | `.` | **Ready — not started** | b |\n\
             \n## Suggested execution order\n\n```\n0002 Two\n0001 One\n```\n",
        );
        fx.track("0001-One");
        fx.track("0002-Two");
        fx.import();
        assert_eq!(
            pick_next_ready_excluding_sticky(&fx.record, &[], None, &[]).unwrap(),
            "0002"
        );
        // Mutating only the store's order changes the answer, proving the store is read.
        let mut store = crate::registry_store::load(&fx.record).unwrap().unwrap();
        store.execution_order = vec!["0001".into(), "0002".into()];
        std::fs::write(
            crate::registry_store::store_path_for(&fx.record),
            serde_json::to_vec_pretty(&store).unwrap(),
        )
        .unwrap();
        assert_eq!(
            pick_next_ready_excluding_sticky(&fx.record, &[], None, &[]).unwrap(),
            "0001"
        );
    }

    fn pick_case(body: &str, configure: impl Fn(&mut Fx)) -> (String, String) {
        let md = fixture("p-20-md");
        md.write_view(body);
        let mut md = md;
        configure(&mut md);
        let md_result = pick_next_ready_excluding_sticky(&md.record, &[], None, &[]);
        let md_text = match &md_result {
            Ok(id) => format!("ok:{id}"),
            Err(e) => format!("err:{e}"),
        };

        let mut stored = fixture("p-20-store");
        stored.write_view(body);
        configure(&mut stored);
        let report = stored.import();
        assert_eq!(report.status, "ok", "{report:?}");
        let store_result = pick_next_ready_excluding_sticky(&stored.record, &[], None, &[]);
        let store_text = match &store_result {
            Ok(id) => format!("ok:{id}"),
            Err(e) => format!("err:{e}"),
        };
        (md_text, store_text)
    }

    #[test]
    fn pick_parity_matrix_markdown_vs_store() {
        struct Case {
            name: &'static str,
            body: String,
            configure: fn(&mut Fx),
        }
        fn none(_: &mut Fx) {}
        fn ready_dir(fx: &mut Fx) {
            fx.track("0001");
        }
        let cases: Vec<Case> = vec![
            Case {
                name: "exact_ready",
                body: registry(&[row("0001", READY, "go")]),
                configure: ready_dir,
            },
            Case {
                name: "custom_alias",
                body: registry(&[row("0001", "**Pending**", "go")]),
                configure: |fx| {
                    fx.record.ready_aliases = vec!["Pending".into()];
                    fx.track("0001");
                },
            },
            Case {
                name: "proposed",
                body: registry(&[row("0001", "**Proposed**", "go")]),
                configure: none,
            },
            Case {
                name: "blocked",
                body: registry(&[row("0001", "**Blocked**", "go")]),
                configure: none,
            },
            Case {
                name: "completed",
                body: registry(&[row("0001", "**Completed**", "go")]),
                configure: none,
            },
            Case {
                name: "absorbed",
                body: registry(&[row("0001", "**Absorbed — 0058**", "go")]),
                configure: none,
            },
            Case {
                name: "cancelled",
                body: registry(&[row("0001", "**Cancelled**", "go")]),
                configure: none,
            },
            Case {
                name: "in_progress",
                body: registry(&[row("0001", "**In progress**", "go")]),
                configure: none,
            },
            Case {
                name: "nostart",
                body: registry(&[row("0001", READY, "go <!-- nostart -->")]),
                configure: ready_dir,
            },
            Case {
                name: "hitl",
                body: registry(&[row("0001", READY, "needs HITL")]),
                configure: ready_dir,
            },
            Case {
                name: "skip_list",
                body: registry(&[row("0001", READY, "a"), row("0002", READY, "b")]),
                configure: none,
            },
            Case {
                name: "missing_dir",
                body: registry(&[row("0001", READY, "a")]),
                configure: none,
            },
            Case {
                name: "duplicate",
                body: registry(&[row("0040", READY, "a"), row("0040", READY, "b")]),
                configure: |fx| fx.track("0040"),
            },
            Case {
                name: "alpha_prefixed",
                body: registry(&[row("T0374", READY, "a")]),
                configure: |fx| fx.track("0374-T"),
            },
        ];
        for case in cases {
            let (md, stored) = pick_case(&case.body, case.configure);
            assert_eq!(md, stored, "case {}", case.name);
        }

        // Sticky overlay: a non-token status kept live by the sticky list.
        let md = fixture("p-20-sticky-md");
        md.write_view(&registry(&[row("0001", "**Queued**", "a")]));
        md.track("0001");
        let md_text =
            pick_next_ready_excluding_sticky(&md.record, &[], None, &["0001".into()]).unwrap();
        let stored = fixture("p-20-sticky-store");
        stored.write_view(&registry(&[row("0001", "**Queued**", "a")]));
        stored.track("0001");
        stored.import();
        let store_text =
            pick_next_ready_excluding_sticky(&stored.record, &[], None, &["0001".into()]).unwrap();
        assert_eq!(md_text, store_text);
        assert_eq!(store_text, "0001");

        // Skip list.
        let body = registry(&[row("0001", READY, "a"), row("0002", READY, "b")]);
        let md = fixture("p-20-skip-md");
        md.write_view(&body);
        md.track("0001");
        md.track("0002");
        let md_text =
            pick_next_ready_excluding_sticky(&md.record, &["0001".into()], None, &[]).unwrap();
        let stored = fixture("p-20-skip-store");
        stored.write_view(&body);
        stored.track("0001");
        stored.track("0002");
        stored.import();
        let store_text =
            pick_next_ready_excluding_sticky(&stored.record, &["0001".into()], None, &[]).unwrap();
        assert_eq!(md_text, store_text);
        assert_eq!(store_text, "0002");

        // Merged-PR probe skip, then the next Ready row.
        let md = fixture("p-20-probe-md");
        md.write_view(&body);
        md.track("0001");
        md.track("0002");
        let mut md = md;
        md.record.execution_repo = Some(md.record.path.join("exec"));
        let probe = FixedProbe {
            merged: vec!["0001".into()],
        };
        let md_text = pick_next_ready_excluding_sticky(&md.record, &[], Some(&probe), &[]).unwrap();
        let mut stored = fixture("p-20-probe-store");
        stored.write_view(&body);
        stored.track("0001");
        stored.track("0002");
        stored.record.execution_repo = Some(stored.record.path.join("exec"));
        stored.import();
        let store_text =
            pick_next_ready_excluding_sticky(&stored.record, &[], Some(&probe), &[]).unwrap();
        assert_eq!(md_text, store_text);
        assert_eq!(store_text, "0002");
    }

    #[test]
    fn fold_advance_parity_matrix_markdown_vs_store() {
        fn fold_case(body: &str, track: Option<&str>, configure: impl Fn(&Fx)) -> (String, String) {
            let md = fixture("p-21-md");
            md.write_view(body);
            configure(&md);
            let md_result = format!("{:?}", fold_advance(&md.record, track));

            let stored = fixture("p-21-store");
            stored.write_view(body);
            configure(&stored);
            assert_eq!(stored.import().status, "ok");
            let store_result = format!("{:?}", fold_advance(&stored.record, track));
            (md_result, store_result)
        }
        let none = |_: &Fx| {};
        let cases = [
            ("allow", registry(&[row("0001", READY, "a")]), Some("0001")),
            (
                "proposed",
                registry(&[row("0001", "**Proposed**", "a")]),
                Some("0001"),
            ),
            (
                "blocked",
                registry(&[row("0001", "**Blocked**", "a")]),
                Some("0001"),
            ),
            (
                "completed",
                registry(&[row("0001", "**Completed**", "a")]),
                Some("0001"),
            ),
            (
                "row_missing",
                registry(&[row("0002", READY, "a")]),
                Some("0001"),
            ),
        ];
        for (name, body, track) in cases {
            let (md, stored) = fold_case(&body, track, none);
            assert_eq!(md, stored, "case {name}");
        }
        let body = registry(&[row("0020", PROPOSED_PLACEHOLDER, "a")]);
        let (md, stored) = fold_case(&body, Some("0020"), |fx| fx.track("0020"));
        assert_eq!(md, stored);
        assert!(md.contains("PromotePlaceholder"), "{md}");
    }

    #[test]
    fn store_error_is_not_a_benign_pick_miss() {
        let _lock = crate::config::test_env_lock();
        let _home = isolate_features();
        let fx = fixture("p-22");
        fx.write_view(&registry(&[row("0001", READY, "a")]));
        fx.track("0001");
        fx.import();
        std::fs::write(
            crate::registry_store::store_path_for(&fx.record),
            b"{not json",
        )
        .unwrap();
        let err = pick_next_ready_excluding_sticky(&fx.record, &[], None, &[]).unwrap_err();
        assert!(!ready_pick_miss_is_benign(&err), "{err}");
        assert!(matches!(err, CoordinatorError::Message(_)));

        let mut record = fx.record.clone();
        record.auto_start = AutoStartPolicy::Full;
        let attempt = crate::watch::try_auto_start(&record);
        assert!(
            matches!(attempt, crate::watch::AutoStartAttempt::Failed { .. }),
            "{attempt:?}"
        );
        assert_eq!(
            crate::state::load_run_state(&record).unwrap().failure_class,
            Some(crate::outcome::FailureClass::HarnessCrash),
            "a broken store is recorded as a durable failure, not a silent no-op"
        );
    }

    #[test]
    fn view_drift_reports_once_and_the_pick_fails_closed() {
        let fx = fixture("p-23");
        fx.write_view(&registry(&[row("0001", READY, "a")]));
        fx.track("0001");
        fx.import();
        crate::registry_store::reset_drift_warnings_for_tests();
        fx.write_view(&fx.read_view().replace("| a |", "| a hand edit |"));

        let rows = load_track_rows(&fx.record).unwrap();
        assert_eq!(rows.len(), 1, "the store still answers the display legs");
        let _ = load_track_rows(&fx.record).unwrap();
        let log = fx.status_log();
        assert_eq!(
            log.matches("registry-view-drift").count(),
            1,
            "one line per process/project: {log}"
        );
        let err = pick_next_ready_excluding_sticky(&fx.record, &[], None, &[]).unwrap_err();
        assert_drift_error_text(&err);
        assert!(!ready_pick_miss_is_benign(&err));
    }

    #[test]
    fn hand_added_ready_row_is_not_picked_until_import() {
        let _lock = crate::config::test_env_lock();
        let _home = isolate_features();
        let fx = fixture("p-49");
        fx.write_view(&registry(&[row("0002", "**In progress**", "a")]));
        fx.import();
        fx.track("0003");
        fx.write_view(&format!(
            "{}{}",
            fx.read_view(),
            row("0003", READY, "hand added")
        ));
        let view = crate::registry_store::status_view_for(&fx.record);
        assert_eq!(view.state, "drift");
        assert!(view.recovery.unwrap().contains("import"));

        let err = pick_next_ready_excluding_sticky(&fx.record, &[], None, &[]).unwrap_err();
        assert_drift_error_text(&err);

        let mut record = fx.record.clone();
        record.auto_start = AutoStartPolicy::Full;
        let attempt = crate::watch::try_auto_start(&record);
        assert_eq!(
            attempt,
            crate::watch::AutoStartAttempt::Skipped(crate::watch::AutoStartSkip::RegistryDrift)
        );
        assert!(
            crate::notify::artifact::existing_path(&record).is_none(),
            "a drift is not a harness crash"
        );

        fx.import_force();
        assert_eq!(
            pick_next_ready_excluding_sticky(&fx.record, &[], None, &[]).unwrap(),
            "0003"
        );
        assert_eq!(
            crate::registry_store::status_view_for(&fx.record).state,
            "ok"
        );
    }

    #[test]
    fn status_reports_registry_view_state() {
        let fx = fixture("p-50");
        fx.write_view(&registry(&[row("0001", READY, "a")]));
        assert_eq!(
            crate::registry_store::status_view_for(&fx.record).state,
            "absent"
        );
        fx.import();
        assert_eq!(
            crate::registry_store::status_view_for(&fx.record).state,
            "ok"
        );
        fx.write_view(&fx.read_view().replace("| a |", "| b |"));
        let drift = crate::registry_store::status_view_for(&fx.record);
        assert_eq!(drift.state, "drift");
        assert!(drift.recovery.unwrap().contains("render"));
        std::fs::write(crate::registry_store::store_path_for(&fx.record), b"[]").unwrap();
        let invalid = crate::registry_store::status_view_for(&fx.record);
        assert_eq!(invalid.state, "invalid");
        assert!(invalid.recovery.is_some());
    }

    #[test]
    fn retain_refuses_when_store_present_but_unreadable() {
        let fx = fixture("p-51");
        fx.write_view(&registry(&[row("0001", READY, "a")]));
        fx.import();
        std::fs::write(
            crate::registry_store::store_path_for(&fx.record),
            b"{not json",
        )
        .unwrap();
        let mut state = RunState::idle(&fx.record.id);
        state.track_id = Some("0001".into());
        assert_eq!(
            crate::api::refused_retain_id(&fx.record, &state, None),
            Some("0001".to_string())
        );

        std::fs::remove_file(crate::registry_store::store_path_for(&fx.record)).unwrap();
        fx.write_view("not a registry\n");
        assert_eq!(
            crate::api::refused_retain_id(&fx.record, &state, None),
            None
        );
    }

    #[test]
    fn view_parse_reads_zero_in_store_mode() {
        let fx = fixture("p-52");
        fx.write_view(&registry(&[row("0001", READY, "a")]));
        fx.track("0001");
        fx.import();
        let mut state = RunState::idle(&fx.record.id);
        state.track_id = Some("0001".into());
        reset_view_parse_reads();

        let rows = load_track_rows(&fx.record).unwrap();
        let _ = pick_next_ready_excluding_sticky(&fx.record, &[], None, &[]);
        let _ = status_snapshot_for(&fx.record);
        let _ = parsed_track_row_count(&fx.record);
        let _ = fold_advance(&fx.record, Some("0001"));
        let _ = track_row_proposed(&fx.record, Some("0001"));
        let _ = track_row_nostart(&fx.record, "0001");
        let _ = track_row_completed(&rows, "0001");
        let _ = failure_superseded(&fx.record, &state, None);
        let _ = capture_sticky_ready_ids(&fx.record, &[]);
        let _ = overlay_file_status(&fx.record, "0001");
        let _ = crate::api::refused_retain_id(&fx.record, &state, None);
        let _ = crate::workflow::track_is_shipped_local(&fx.record, &state, "0001");
        let _ =
            crate::workflow::execution_repo::resolve_track_target(&fx.record, &state, "implement");
        assert_eq!(
            view_parse_reads(),
            0,
            "decision legs must not parse the view in store mode"
        );

        std::fs::remove_file(crate::registry_store::store_path_for(&fx.record)).unwrap();
        reset_view_parse_reads();
        let _ = load_track_rows(&fx.record);
        let _ = width_deviations_for(&fx.record);
        assert!(view_parse_reads() >= 2, "{}", view_parse_reads());
    }

    #[test]
    fn repair_keeps_both_rows_invisible_until_escaped() {
        let broken = "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0102-X](0102-X/spec.md) | `.` | **Completed** | `default|config|env` |\n\
             | [0115-Y](0115-Y/spec.md) | `.` | **Proposed** | /(\\w|\\s)/ |\n\
             | [0003-Z](0003-Z/spec.md) | `.` | **Ready — not started** | visible |\n";
        let rows = parse_conductor_md(broken).unwrap();
        assert_eq!(rows.len(), 1, "6-cell rows are dropped: {rows:?}");
        assert_eq!(rows[0].id, "0003");

        let escaped = broken
            .replace("`default|config|env`", "`default\\|config\\|env`")
            .replace("/(\\w|\\s)/", "/(\\w\\|\\s)/");
        let rows = parse_conductor_md(&escaped).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].summary, "`default|config|env`");
        assert_eq!(rows[1].summary, "/(\\w|\\s)/");
    }

    // ---- 0115 comparison seam + dropped-row diagnostics -------------------

    #[test]
    fn status_clean_folds_every_dash_form() {
        for dash in [
            '-', '\u{2010}', '\u{2011}', '\u{2012}', '\u{2013}', '\u{2014}', '\u{2015}',
            '\u{2212}', '\u{FE58}', '\u{FE63}', '\u{FF0D}',
        ] {
            let raw = format!("**Ready {dash} not started**");
            assert_eq!(status_clean(&raw), READY_NORMALIZED, "{dash:?}");
            assert!(is_eligible_ready(&raw), "{dash:?}");
        }
    }

    #[test]
    fn status_clean_collapses_space_forms() {
        let mut forms: Vec<char> = vec![
            '\t', '\u{000B}', '\u{000C}', '\u{000D}', ' ', '\u{0085}', '\u{00A0}', '\u{1680}',
            '\u{2028}', '\u{2029}', '\u{202F}', '\u{205F}', '\u{3000}',
        ];
        forms.extend('\u{2000}'..='\u{200A}');
        for w in forms {
            let raw = format!("Ready{w}-{w}not started");
            assert_eq!(status_clean(&raw), READY_NORMALIZED, "{w:?}");
            assert!(is_eligible_ready(&raw), "{w:?}");
        }
        for raw in [
            "Ready  —  not started",
            "Ready  - not started",
            "Ready \u{00A0}\u{2009}- \t not\u{3000}started",
        ] {
            assert_eq!(status_clean(raw), READY_NORMALIZED, "{raw:?}");
            assert!(is_eligible_ready(raw), "{raw:?}");
        }
    }

    #[test]
    fn ready_gate_still_rejects_the_vocabulary() {
        for raw in [
            "**Proposed**",
            "**Proposed - placeholder**",
            "**Blocked**",
            "**Pending**",
            "**In progress**",
            "**Completed**",
            "Ready - not started — note",
            "Ready - not start",
            "Ready - not started extra",
        ] {
            assert!(!is_eligible_ready(raw), "{raw:?}");
        }
        assert!(is_eligible_ready_in("**Pending**", &["Pending"]));
    }

    #[test]
    fn status_clean_is_idempotent() {
        for raw in [
            "**Ready — not started**",
            "Ready\u{00A0}-\u{00A0}not started",
            "Ready  -  not started",
            "**Proposed - placeholder**",
            "Ready - not started — note",
        ] {
            let once = status_clean(raw);
            assert_eq!(status_clean(&once), once, "{raw:?}");
        }
    }

    #[test]
    fn scan_dropped_rows_counts_and_lines() {
        let text = "| Track | Execution | Status | Summary |\n\
                    | --- | --- | --- | --- |\n\
                    | [0001-Foo](0001-Foo/spec.md) | `.` | **Ready — not started** | ok |\n\
                    | track12 | `.` | **Ready — not started** | x |\n\
                    | AI-T572-SafetySyncHotspotDelta | `.` | **Ready — not started** | y |\n\
                    | 0002-Extra | `.` | **Completed** | z | extra |\n\
                    | 0003-Short | `.` | **Ready — not started** |\n\
                    | 0004-Short2 | `.` |\n";
        let scan = scan_dropped_rows(text);
        assert_eq!(scan.tables, 1);
        assert_eq!(scan.data_rows, 6);
        assert_eq!(scan.parsed_rows, 1);
        assert_eq!(scan.dropped_total, 5);
        assert_eq!(scan.data_rows, scan.parsed_rows + scan.dropped_total);
        assert_eq!(scan.dropped.len(), 5);
        let ai = scan
            .dropped
            .iter()
            .find(|d| d.track_cell == "AI-T572-SafetySyncHotspotDelta")
            .expect("AI-T572 dropped");
        assert_eq!(ai.line, 5);
        assert_eq!(ai.reason, DropReason::IdUnparseable);
        let extra = scan
            .dropped
            .iter()
            .find(|d| d.track_cell == "0002-Extra")
            .expect("extra pipe dropped");
        assert_eq!(extra.line, 6);
        assert_eq!(extra.reason, DropReason::CellCount);
        let short = scan
            .dropped
            .iter()
            .find(|d| d.track_cell == "0003-Short")
            .expect("short row dropped");
        assert_eq!(short.line, 7);
        assert_eq!(short.reason, DropReason::CellCount);
        assert_eq!(short.status_cell, "**Ready — not started**");
        let short2 = scan
            .dropped
            .iter()
            .find(|d| d.track_cell == "0004-Short2")
            .expect("second short row dropped");
        assert_eq!(short2.line, 8);
        assert_eq!(short2.status_cell, "", "omitted Status must not index");
        // The parser agrees on the parsed count.
        assert_eq!(parse_conductor_md(text).unwrap().len(), scan.parsed_rows);
    }

    #[test]
    fn scan_dropped_rows_cap_keeps_totals() {
        let mut text = String::from(
            "| Track | Status | Summary |\n\
             | --- | --- | --- |\n\
             | [0001-Foo](0001-Foo/spec.md) | **Ready — not started** | ok |\n",
        );
        for n in 0..60 {
            text.push_str(&format!("| trackx{n} | Active | row {n} |\n"));
        }
        let scan = scan_dropped_rows(&text);
        assert_eq!(scan.data_rows, 61);
        assert_eq!(scan.parsed_rows, 1);
        assert_eq!(scan.dropped_total, 60);
        assert_eq!(scan.dropped.len(), DROPPED_SAMPLE_CAP);
        assert_eq!(scan.data_rows, scan.parsed_rows + scan.dropped_total);
    }
}
