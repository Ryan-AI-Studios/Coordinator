//! Parse `conductor/conductor.md` and pick the first exact Ready track (0030).

use crate::error::{CoordinatorError, Result};
use crate::notify::artifact::{numeric_track_id, track_ids_match};
use crate::outcome::FailureClass;
use crate::registry::{AutoStartPolicy, ProjectRecord};
use crate::state::{RunState, RunStatus, StatusView};

use super::LAST_EVENT_BACKLOG_CLEAR;
use super::graph::resolve_track_dir;
use super::shipped::MergedTrackProbe;

const READY_NORMALIZED: &str = "Ready - not started";

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
}

/// Trim → unwrap wrapping emphasis → strip leading emoji/punct (stop at alnum or
/// emphasis markers) until stable, then dash-normalize. Eligible iff the result
/// equals `Ready - not started`.
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
    cur.replace(['\u{2014}', '\u{2013}', '\u{2212}'], "-")
}

fn unwrap_emphasis(s: &str) -> Option<&str> {
    const MARKERS: [&str; 6] = ["**", "__", "~~", "`", "*", "_"];
    for m in MARKERS {
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

/// Registry `**Completed**` (and the same after status_clean).
pub fn is_completed(status_raw: &str) -> bool {
    status_clean(status_raw) == "Completed"
}

/// Best-effort `{conductor_dir}/conductor.md` rows. Missing / unreadable → `None`.
pub fn load_track_rows(record: &ProjectRecord) -> Option<Vec<TrackRow>> {
    let path = crate::layout::resolve(record)
        .conductor_dir
        .join("conductor.md");
    if !path.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    parse_conductor_md(&text).ok()
}

fn row_matches_track(row: &TrackRow, key: &str) -> bool {
    let slug = if row.slug.is_empty() {
        row.id.clone()
    } else {
        format!("{}-{}", row.id, row.slug)
    };
    crate::notify::artifact::track_ids_match(&row.id, key)
        || crate::notify::artifact::track_ids_match(&slug, key)
}

/// True when a row matching `key` has status Completed.
pub fn track_row_completed(rows: &[TrackRow], key: &str) -> bool {
    rows.iter()
        .any(|r| row_matches_track(r, key) && is_completed(&r.status_raw))
}

/// Read-time SUPERSEDED reason (0040). Missing conductor.md skips the Completed signal.
pub fn failure_superseded(
    record: &ProjectRecord,
    state: &RunState,
    body: Option<&str>,
) -> Option<String> {
    let meta = body.map(crate::notify::artifact::parse_metadata);
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
    [id, slug, status, summary].iter().any(|p| field_is_hitl(p))
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
pub fn strip_nostart_literal(line: &str) -> (String, bool) {
    if line.contains(NOSTART_LITERAL) {
        (line.replace(NOSTART_LITERAL, ""), true)
    } else {
        (line.to_string(), false)
    }
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

/// Every GFM table with Track/Id + Status columns, concatenated, by header name.
pub fn parse_conductor_md(text: &str) -> Result<Vec<TrackRow>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let tables = collect_pipe_tables(text);
    let mut rows = Vec::new();
    for table in tables {
        if table.len() < 2 {
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
        let summary_col = headers.iter().position(|h| is_summary_header(h));
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
            rows.push(TrackRow {
                id,
                slug,
                status_raw,
                summary,
                nostart: row.nostart,
            });
        }
    }
    if rows.is_empty() {
        return Err(CoordinatorError::Message(
            "could not parse track registry in conductor.md; pass --track <id>".into(),
        ));
    }
    Ok(rows)
}

fn is_track_or_id_header(h: &str) -> bool {
    h.eq_ignore_ascii_case("track") || h.eq_ignore_ascii_case("id")
}

fn is_status_header(h: &str) -> bool {
    h.eq_ignore_ascii_case("status")
}

fn is_summary_header(h: &str) -> bool {
    h.eq_ignore_ascii_case("summary")
}

fn unwrap_md_link(cell: &str) -> String {
    let t = cell.trim();
    if let Some(rest) = t.strip_prefix('[')
        && let Some(end) = rest.find("](")
        && rest[end + 2..].ends_with(')')
    {
        return rest[..end].to_string();
    }
    t.to_string()
}

fn parse_leading_id(text: &str) -> Option<(String, String)> {
    let t = text.trim();
    let b = t.as_bytes();
    if b.len() < 4 || !b[..4].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let rest = &t[4..];
    if rest
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    let slug = rest.strip_prefix('-').unwrap_or(rest).to_string();
    Some((t[..4].to_string(), slug))
}

/// Split on unescaped `|`. Drop wrapping empty cells from leading/trailing pipes.
fn split_unescaped_pipes(line: &str) -> Vec<String> {
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

fn is_delimiter_cell(cell: &str) -> bool {
    let t = cell.trim();
    let t = t.strip_prefix(':').unwrap_or(t);
    let t = t.strip_suffix(':').unwrap_or(t);
    t.len() >= 3 && t.chars().all(|c| c == '-')
}

fn looks_like_row(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && t.contains('|') && !t.starts_with("```") && !t.starts_with("~~~")
}

fn is_table_start(lines: &[&str], i: usize) -> bool {
    if i + 1 >= lines.len() || !looks_like_row(lines[i]) {
        return false;
    }
    let (delim_line, _) = strip_nostart_literal(lines[i + 1]);
    let delim = split_unescaped_pipes(&delim_line);
    !delim.is_empty() && delim.iter().all(|c| is_delimiter_cell(c))
}

fn collect_pipe_tables(text: &str) -> Vec<Vec<PipeRow>> {
    let lines: Vec<&str> = text.lines().collect();
    let mut tables = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if !is_table_start(&lines, i) {
            i += 1;
            continue;
        }
        let (header_line, header_nostart) = strip_nostart_literal(lines[i]);
        let header = split_unescaped_pipes(&header_line);
        let mut rows = vec![PipeRow {
            cells: header,
            nostart: header_nostart,
        }];
        i += 2;
        while i < lines.len() {
            if lines[i].trim().is_empty() {
                i += 1;
                continue;
            }
            if !looks_like_row(lines[i]) || is_table_start(&lines, i) {
                break;
            }
            let (stripped, nostart) = strip_nostart_literal(lines[i]);
            rows.push(PipeRow {
                cells: split_unescaped_pipes(&stripped),
                nostart,
            });
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
    let path = crate::layout::resolve(record)
        .conductor_dir
        .join("conductor.md");
    if !path.is_file() {
        return Err(CoordinatorError::Message(format!(
            "no conductor.md at {}; pass --track <id>",
            path.display()
        )));
    }
    let text = std::fs::read_to_string(&path)?;
    let rows = parse_conductor_md(&text)?;
    let order = parse_execution_order(&text);
    let aliases = ready_aliases_for(record);
    let mut eligible: Vec<&TrackRow> = rows
        .iter()
        .filter(|r| {
            is_eligible_ready_in(&r.status_raw, &aliases)
                && !is_hitl_marked(&r.id, &r.slug, &r.status_raw, &r.summary)
                && !r.nostart
        })
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

/// Test helper: one exact Ready row + `{id}-Fixture/` dir under `ws/conductor/`.
#[cfg(test)]
pub fn write_ready_fixture(ws: &std::path::Path, id: &str) -> std::io::Result<()> {
    let cond = ws.join("conductor");
    std::fs::create_dir_all(cond.join(format!("{id}-Fixture")))?;
    let md = format!(
        "| Track | Execution path | Status | Summary |\n\
         | --- | --- | --- | --- |\n\
         | {id}-Fixture | `.` | **Ready — not started** | fixture |\n"
    );
    std::fs::write(cond.join("conductor.md"), md)
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
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
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
}
