//! Owner-decision comment blocks in a track `evidence.md`.
//!
//! The plan stamp writer copies the body. This writer copies stamp lines and
//! every byte outside a block it rewrites.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::error::{CoordinatorError, Result};
use crate::registry::ProjectRecord;

use super::evidence_stamp::{self, is_utc_line};
use super::graph::resolve_track_dir;

/// Honest limit shared by `decision record --help` and the fold inject.
pub const RECORD_LIMITATION: &str = "The marker records that an operator declared this decision \
and supplied the name. It is not proof the named person typed it, not a signature, and not a \
tamper-proof or compliance control.";

const CLOSER: &str = "<!-- /coordinator:decision -->";
const OPENER_PREFIX: &str = "<!-- coordinator:decision ";
const ACTIVE_PREFIX: &str = "<!-- coordinator:decision active by=\"";
const SUPERSEDED_PREFIX: &str = "<!-- coordinator:decision superseded by=\"";

const ERR_ALREADY: &str = "active decision already recorded; pass --supersede to replace it";
const ERR_BY: &str = "by must be non-empty and must not contain quotes or newlines";
const ERR_EMPTY: &str = "sentence is empty";
const ERR_MARKER: &str = "sentence contains a decision marker line";
const ERR_UTF8: &str = "evidence.md is not valid UTF-8";
const ERR_MANY: &str = "evidence.md has more than one active decision";
const ERR_MALFORMED: &str = "evidence.md decision block is malformed";

/// Whether `record` replaced bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordEffect {
    Wrote,
    Unchanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockStatus {
    Active,
    Superseded,
}

struct DecisionBlock {
    status: BlockStatus,
    by: String,
    recorded_at: String,
    superseded_at: Option<String>,
    sentence: String,
    start: usize,
    end: usize,
}

struct Opener {
    status: BlockStatus,
    by: String,
    recorded_at: String,
    superseded_at: Option<String>,
}

enum ReadFile {
    Missing,
    Text(String),
    InvalidUtf8,
}

enum Parsed {
    Blocks(Vec<DecisionBlock>),
    Malformed,
}

struct Line<'a> {
    text: &'a str,
    start: usize,
    end: usize,
}

/// Strip one trailing `\r\n` or `\n`. Do not trim spaces. Reject an empty
/// sentence and a sentence that contains a decision marker line.
pub(crate) fn normalize_sentence_input(raw: &str) -> Result<String> {
    let sentence = strip_one_newline(raw);
    validate_sentence(sentence)?;
    Ok(sentence.to_string())
}

/// Record into `path`. A missing file is created with a stamp. An existing
/// file is not given a stamp by this function.
pub(crate) fn record_evidence(
    path: &Path,
    by: &str,
    sentence: &str,
    supersede: bool,
    now: DateTime<Utc>,
) -> Result<RecordEffect> {
    validate_by(by)?;
    validate_sentence(sentence)?;
    let stamp = evidence_stamp::utc_stamp_line(now);
    match read_evidence(path)? {
        ReadFile::InvalidUtf8 => Err(CoordinatorError::Message(ERR_UTF8.into())),
        ReadFile::Missing => {
            let body = format!(
                "{}\n{}\n{}",
                evidence_stamp::version_line(),
                stamp,
                render_active(by, &stamp, sentence)
            );
            crate::persist::atomic_write(path, body.as_bytes())?;
            Ok(RecordEffect::Wrote)
        }
        ReadFile::Text(text) => write_into_existing(path, &text, by, sentence, supersede, &stamp),
    }
}

/// Resolve the track directory, then record into its `evidence.md`.
pub(crate) fn record_for(
    record: &ProjectRecord,
    track: &str,
    by: &str,
    sentence: &str,
    supersede: bool,
    now: DateTime<Utc>,
) -> Result<RecordEffect> {
    let path = evidence_path(record, track)?;
    record_evidence(&path, by, sentence, supersede, now)
}

/// Text or JSON for the one active block. Zero active blocks is success.
pub(crate) fn show_for(record: &ProjectRecord, track: &str, json: bool) -> Result<String> {
    let blocks = load_blocks(record, track)?;
    let active = active_refs(&blocks);
    match active.len() {
        0 => Ok(show_none(json)?),
        1 => Ok(show_one(active[0], json)?),
        _ => Err(CoordinatorError::Message(ERR_MANY.into())),
    }
}

/// Text or JSON for every well-formed block, including superseded.
pub(crate) fn list_for(record: &ProjectRecord, track: &str, json: bool) -> Result<String> {
    let blocks = load_blocks(record, track)?;
    if json {
        json_line(&ListOut {
            blocks: blocks.iter().map(BlockOut::from).collect(),
        })
    } else {
        Ok(render_list_text(&blocks))
    }
}

/// Fold inject. Missing directory, missing file, or no active block is
/// `active decision: none`. This function does not write.
pub(crate) fn inject_for(record: &ProjectRecord, track_id: Option<&str>) -> String {
    let Some(track) = track_id.map(str::trim).filter(|id| !id.is_empty()) else {
        return "active decision: none\n".into();
    };
    let Some(dir) = resolve_track_dir(record, track) else {
        return "active decision: none\n".into();
    };
    let text = match read_evidence(&dir.join("evidence.md")) {
        Ok(ReadFile::Missing) => return "active decision: none\n".into(),
        Ok(ReadFile::Text(text)) => text,
        _ => return "active decision: invalid\n".into(),
    };
    match parse(&text) {
        Parsed::Malformed => "active decision: invalid\n".into(),
        Parsed::Blocks(blocks) => match active_refs(&blocks).as_slice() {
            [] => "active decision: none\n".into(),
            [block] => active_decision_text(block),
            _ => "active decision: invalid\n".into(),
        },
    }
}

fn write_into_existing(
    path: &Path,
    text: &str,
    by: &str,
    sentence: &str,
    supersede: bool,
    stamp: &str,
) -> Result<RecordEffect> {
    let blocks = match parse(text) {
        Parsed::Malformed => return Err(CoordinatorError::Message(ERR_MALFORMED.into())),
        Parsed::Blocks(blocks) => blocks,
    };
    let active = active_refs(&blocks);
    if active.len() > 1 {
        return Err(CoordinatorError::Message(ERR_MANY.into()));
    }
    if let Some(current) = active.first() {
        if current.by == by && current.sentence == sentence {
            return Ok(RecordEffect::Unchanged);
        }
        if !supersede {
            return Err(CoordinatorError::Message(ERR_ALREADY.into()));
        }
        let replaced = supersede_text(text, current, by, sentence, stamp);
        crate::persist::atomic_write(path, replaced.as_bytes())?;
        return Ok(RecordEffect::Wrote);
    }
    let mut out = text.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&render_active(by, stamp, sentence));
    crate::persist::atomic_write(path, out.as_bytes())?;
    Ok(RecordEffect::Wrote)
}

fn supersede_text(
    text: &str,
    current: &DecisionBlock,
    by: &str,
    sentence: &str,
    stamp: &str,
) -> String {
    let mut out = String::new();
    out.push_str(&text[..current.start]);
    out.push_str(&render_superseded(
        &current.by,
        &current.recorded_at,
        stamp,
        &current.sentence,
    ));
    out.push_str(&text[current.end..]);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&render_active(by, stamp, sentence));
    out
}

fn evidence_path(record: &ProjectRecord, track: &str) -> Result<PathBuf> {
    let dir = resolve_track_dir(record, track)
        .ok_or_else(|| CoordinatorError::Message(format!("no track directory for {track}")))?;
    Ok(dir.join("evidence.md"))
}

fn load_blocks(record: &ProjectRecord, track: &str) -> Result<Vec<DecisionBlock>> {
    let path = evidence_path(record, track)?;
    match read_evidence(&path)? {
        ReadFile::Missing => Ok(Vec::new()),
        ReadFile::InvalidUtf8 => Err(CoordinatorError::Message(ERR_UTF8.into())),
        ReadFile::Text(text) => match parse(&text) {
            Parsed::Malformed => Err(CoordinatorError::Message(ERR_MALFORMED.into())),
            Parsed::Blocks(blocks) => Ok(blocks),
        },
    }
}

fn read_evidence(path: &Path) -> Result<ReadFile> {
    match std::fs::read(path) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => Ok(ReadFile::Text(text)),
            Err(_) => Ok(ReadFile::InvalidUtf8),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(ReadFile::Missing),
        Err(err) => Err(err.into()),
    }
}

fn validate_by(by: &str) -> Result<()> {
    if by.is_empty() || by.contains('"') || by.contains('\n') || by.contains('\r') {
        Err(CoordinatorError::Message(ERR_BY.into()))
    } else {
        Ok(())
    }
}

fn validate_sentence(sentence: &str) -> Result<()> {
    if sentence.is_empty() {
        return Err(CoordinatorError::Message(ERR_EMPTY.into()));
    }
    if sentence_has_marker(sentence) {
        return Err(CoordinatorError::Message(ERR_MARKER.into()));
    }
    Ok(())
}

fn sentence_has_marker(sentence: &str) -> bool {
    sentence.split('\n').any(|line| {
        let line = line.strip_suffix('\r').unwrap_or(line);
        line == CLOSER || line.starts_with(OPENER_PREFIX)
    })
}

fn strip_one_newline(raw: &str) -> &str {
    if let Some(stripped) = raw.strip_suffix("\r\n") {
        stripped
    } else {
        raw.strip_suffix('\n').unwrap_or(raw)
    }
}

fn parse(text: &str) -> Parsed {
    let lines = split_lines(text);
    let mut blocks = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = &lines[index];
        if let Some(opener) = match_opener(line.text) {
            let Some(closer) = find_closer(&lines, index + 1) else {
                return Parsed::Malformed;
            };
            let raw = &text[line.end..lines[closer].start];
            let sentence = strip_one_newline(raw);
            if sentence.is_empty() || sentence_has_marker(sentence) {
                return Parsed::Malformed;
            }
            blocks.push(DecisionBlock {
                status: opener.status,
                by: opener.by,
                recorded_at: opener.recorded_at,
                superseded_at: opener.superseded_at,
                sentence: sentence.to_string(),
                start: line.start,
                end: lines[closer].end,
            });
            index = closer + 1;
            continue;
        }
        if line.text.starts_with(OPENER_PREFIX) {
            return Parsed::Malformed;
        }
        index += 1;
    }
    Parsed::Blocks(blocks)
}

fn find_closer(lines: &[Line<'_>], from: usize) -> Option<usize> {
    let mut index = from;
    while index < lines.len() {
        if lines[index].text == CLOSER {
            return Some(index);
        }
        if lines[index].text.starts_with(OPENER_PREFIX) {
            return None;
        }
        index += 1;
    }
    None
}

fn match_opener(line: &str) -> Option<Opener> {
    let (status, rest) = if let Some(rest) = line.strip_prefix(ACTIVE_PREFIX) {
        (BlockStatus::Active, rest)
    } else {
        let rest = line.strip_prefix(SUPERSEDED_PREFIX)?;
        (BlockStatus::Superseded, rest)
    };
    let (by, rest) = cut_at_quote(rest)?;
    if by.is_empty() {
        return None;
    }
    let rest = rest.strip_prefix(" recorded_at=\"")?;
    let (recorded_at, rest) = cut_at_quote(rest)?;
    if !is_utc_line(recorded_at) {
        return None;
    }
    let (superseded_at, rest) = if status == BlockStatus::Superseded {
        let rest = rest.strip_prefix(" superseded_at=\"")?;
        let (stamp, rest) = cut_at_quote(rest)?;
        if !is_utc_line(stamp) {
            return None;
        }
        (Some(stamp.to_string()), rest)
    } else {
        (None, rest)
    };
    if rest != " -->" {
        return None;
    }
    Some(Opener {
        status,
        by: by.to_string(),
        recorded_at: recorded_at.to_string(),
        superseded_at,
    })
}

fn cut_at_quote(text: &str) -> Option<(&str, &str)> {
    let index = text.find('"')?;
    Some((&text[..index], &text[index + 1..]))
}

fn split_lines(text: &str) -> Vec<Line<'_>> {
    let mut lines = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0;
    while start < text.len() {
        let rest = &text[start..];
        if let Some(rel) = rest.find('\n') {
            let mut text_end = start + rel;
            let end = text_end + 1;
            if text_end > start && bytes[text_end - 1] == b'\r' {
                text_end -= 1;
            }
            lines.push(Line {
                text: &text[start..text_end],
                start,
                end,
            });
            start = end;
        } else {
            let mut text_end = text.len();
            if text_end > start && bytes[text_end - 1] == b'\r' {
                text_end -= 1;
            }
            lines.push(Line {
                text: &text[start..text_end],
                start,
                end: text.len(),
            });
            break;
        }
    }
    lines
}

fn active_refs(blocks: &[DecisionBlock]) -> Vec<&DecisionBlock> {
    blocks
        .iter()
        .filter(|block| block.status == BlockStatus::Active)
        .collect()
}

fn render_active(by: &str, recorded_at: &str, sentence: &str) -> String {
    format!(
        "<!-- coordinator:decision active by=\"{by}\" recorded_at=\"{recorded_at}\" -->\n\
         {sentence}\n\
         {CLOSER}\n"
    )
}

fn render_superseded(by: &str, recorded_at: &str, superseded_at: &str, sentence: &str) -> String {
    format!(
        "<!-- coordinator:decision superseded by=\"{by}\" recorded_at=\"{recorded_at}\" \
         superseded_at=\"{superseded_at}\" -->\n\
         {sentence}\n\
         {CLOSER}\n"
    )
}

fn active_decision_text(block: &DecisionBlock) -> String {
    format!(
        "active decision by=\"{}\" recorded_at=\"{}\":\n{}\n",
        block.by, block.recorded_at, block.sentence
    )
}

fn show_none(json: bool) -> Result<String> {
    if json {
        json_line(&ShowOut { active: None })
    } else {
        Ok("active decision: none\n".into())
    }
}

fn show_one(block: &DecisionBlock, json: bool) -> Result<String> {
    if json {
        json_line(&ShowOut {
            active: Some(ActiveOut {
                by: &block.by,
                recorded_at: &block.recorded_at,
                sentence: &block.sentence,
            }),
        })
    } else {
        Ok(active_decision_text(block))
    }
}

fn render_list_text(blocks: &[DecisionBlock]) -> String {
    if blocks.is_empty() {
        return "decisions: none\n".into();
    }
    let mut out = String::new();
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        match block.status {
            BlockStatus::Active => out.push_str(&format!(
                "decision status=\"active\" by=\"{}\" recorded_at=\"{}\":\n{}\n",
                block.by, block.recorded_at, block.sentence
            )),
            BlockStatus::Superseded => out.push_str(&format!(
                "decision status=\"superseded\" by=\"{}\" recorded_at=\"{}\" superseded_at=\"{}\":\n{}\n",
                block.by,
                block.recorded_at,
                block.superseded_at.as_deref().unwrap_or(""),
                block.sentence
            )),
        }
    }
    out
}

fn json_line(value: &impl serde::Serialize) -> Result<String> {
    Ok(format!("{}\n", serde_json::to_string(value)?))
}

#[derive(serde::Serialize)]
struct ShowOut<'a> {
    active: Option<ActiveOut<'a>>,
}

#[derive(serde::Serialize)]
struct ActiveOut<'a> {
    by: &'a str,
    recorded_at: &'a str,
    sentence: &'a str,
}

#[derive(serde::Serialize)]
struct ListOut<'a> {
    blocks: Vec<BlockOut<'a>>,
}

#[derive(serde::Serialize)]
struct BlockOut<'a> {
    status: &'a str,
    by: &'a str,
    recorded_at: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_at: Option<&'a str>,
    sentence: &'a str,
}

impl<'a> From<&'a DecisionBlock> for BlockOut<'a> {
    fn from(block: &'a DecisionBlock) -> Self {
        let status = match block.status {
            BlockStatus::Active => "active",
            BlockStatus::Superseded => "superseded",
        };
        Self {
            status,
            by: &block.by,
            recorded_at: &block.recorded_at,
            superseded_at: block.superseded_at.as_deref(),
            sentence: &block.sentence,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::LayoutProfile;
    use std::collections::BTreeMap;

    const T1: &str = "2026-10-06T12:00:00Z";
    const T2: &str = "2026-10-06T13:00:00Z";
    const EARLY: &str = "2020-01-01T00:00:00Z";

    fn at(stamp: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(stamp).unwrap().to_utc()
    }

    fn active_block(by: &str, stamp: &str, sentence: &str) -> String {
        render_active(by, stamp, sentence)
    }

    fn project(root: &Path, conductor: &Path) -> ProjectRecord {
        ProjectRecord {
            id: "decision-test".into(),
            path: root.to_path_buf(),
            display_name: None,
            layout_profile: LayoutProfile::Nested,
            conductor_dir: Some(conductor.to_path_buf()),
            execution_repo: Some(root.join("exec")),
            execution_repos: BTreeMap::new(),
            state_dir: Some(root.join("state")),
            auto_merge: false,
            phase_timeouts_secs: BTreeMap::new(),
            notify_progress: false,
            worktree_isolation: false,
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
            state_policies: Vec::new(),
            self_continuation: false,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn parser_accepts_active_superseded_and_keeps_internal_newlines() {
        let text = format!(
            "note\r\n{}\nkeep\n{CLOSER}\n",
            "<!-- coordinator:decision superseded by=\"Ada\" recorded_at=\"2026-10-06T12:00:00Z\" \
             superseded_at=\"2026-10-06T13:00:00Z\" -->"
        );
        let text = format!("{text}{}", active_block("Bea", T2, "one\ntwo"));
        match parse(&text) {
            Parsed::Blocks(blocks) => {
                assert_eq!(blocks.len(), 2);
                assert_eq!(blocks[0].status, BlockStatus::Superseded);
                assert_eq!(blocks[0].by, "Ada");
                assert_eq!(blocks[0].sentence, "keep");
                assert_eq!(blocks[0].superseded_at.as_deref(), Some(T2));
                assert_eq!(blocks[1].status, BlockStatus::Active);
                assert_eq!(blocks[1].sentence, "one\ntwo");
            }
            Parsed::Malformed => panic!("well-formed blocks must parse"),
        }
    }

    #[test]
    fn parser_rejects_malformed_opener_missing_closer_and_empty_sentence() {
        assert!(matches!(
            parse(
                "<!-- coordinator:decision active by=\"\" recorded_at=\"2026-10-06T12:00:00Z\" -->\nno\n<!-- /coordinator:decision -->\n"
            ),
            Parsed::Malformed
        ));
        assert!(matches!(
            parse(&format!(
                "<!-- coordinator:decision active by=\"Ada\" recorded_at=\"{T1}\" -->\nno closer\n"
            )),
            Parsed::Malformed
        ));
        assert!(matches!(
            parse(&format!(
                "<!-- coordinator:decision active by=\"Ada\" recorded_at=\"{T1}\" -->\n{CLOSER}\n"
            )),
            Parsed::Malformed
        ));
        assert!(matches!(
            parse("<!-- coordinator:decision paused by=\"Ada\" -->\n"),
            Parsed::Malformed
        ));
    }

    #[test]
    fn normalize_strips_one_newline_and_rejects_markers() {
        assert_eq!(normalize_sentence_input("hello\n").unwrap(), "hello");
        assert_eq!(normalize_sentence_input("hello\r\n").unwrap(), "hello");
        assert_eq!(normalize_sentence_input("hello\n\n").unwrap(), "hello\n");
        assert_eq!(
            normalize_sentence_input("  hello  \n").unwrap(),
            "  hello  "
        );
        assert!(
            normalize_sentence_input("\n")
                .unwrap_err()
                .to_string()
                .contains(ERR_EMPTY)
        );
        assert!(
            normalize_sentence_input("")
                .unwrap_err()
                .to_string()
                .contains(ERR_EMPTY)
        );
        let marked = format!("hello\n{OPENER_PREFIX}\n");
        assert!(
            normalize_sentence_input(&marked)
                .unwrap_err()
                .to_string()
                .contains(ERR_MARKER)
        );
        assert!(
            normalize_sentence_input(&format!("{CLOSER}\n"))
                .unwrap_err()
                .to_string()
                .contains(ERR_MARKER)
        );
    }

    #[test]
    fn identical_rerecord_is_bit_identical_even_with_supersede() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("evidence.md");
        let body = format!("note\r\n{}", active_block("Ada", T1, "Ship it."));
        std::fs::write(&path, &body).unwrap();
        let effect = record_evidence(&path, "Ada", "Ship it.", true, at(T2)).unwrap();
        assert_eq!(effect, RecordEffect::Unchanged);
        assert_eq!(std::fs::read(&path).unwrap(), body.as_bytes());
        let again = record_evidence(&path, "Ada", "Ship it.", false, at(T2)).unwrap();
        assert_eq!(again, RecordEffect::Unchanged);
        assert_eq!(std::fs::read(&path).unwrap(), body.as_bytes());
    }

    #[test]
    fn different_sentence_without_supersede_leaves_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("evidence.md");
        let body = active_block("Ada", T1, "Ship it.");
        std::fs::write(&path, &body).unwrap();
        let err = record_evidence(&path, "Ada", "Wait.", false, at(T2)).unwrap_err();
        assert!(err.to_string().contains(ERR_ALREADY), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), body.into_bytes());
    }

    #[test]
    fn supersede_keeps_old_sentence_notes_and_crlf_outside_the_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("evidence.md");
        let opener =
            format!("<!-- coordinator:decision active by=\"Ada\" recorded_at=\"{T1}\" -->\r\n");
        let body = format!("before\r\n{opener}keep me\r\n{CLOSER}\r\nafter\r\n");
        std::fs::write(&path, &body).unwrap();
        let effect = record_evidence(&path, "Bea", "next", true, at(T2)).unwrap();
        assert_eq!(effect, RecordEffect::Wrote);
        let got = String::from_utf8(std::fs::read(&path).unwrap()).unwrap();
        assert!(got.starts_with("before\r\n"), "{got}");
        assert!(got.contains("after\r\n"), "{got}");
        assert!(got.contains(&format!(
            "<!-- coordinator:decision superseded by=\"Ada\" recorded_at=\"{T1}\" superseded_at=\"{T2}\" -->\nkeep me\n{CLOSER}\n"
        )), "{got}");
        assert!(got.contains(&active_block("Bea", T2, "next")), "{got}");
        assert!(!got.contains("active by=\"Ada\""), "{got}");
    }

    #[test]
    fn supersede_with_no_active_block_records_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("evidence.md");
        std::fs::write(&path, "only a note\r\n").unwrap();
        record_evidence(&path, "Ada", "first", true, at(T1)).unwrap();
        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            got,
            format!("only a note\r\n{}", active_block("Ada", T1, "first"))
        );
    }

    #[test]
    fn two_active_blocks_do_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("evidence.md");
        let body = format!(
            "{}{}",
            active_block("Ada", T1, "one"),
            active_block("Bea", T1, "two")
        );
        std::fs::write(&path, &body).unwrap();
        let err = record_evidence(&path, "Cy", "three", true, at(T2)).unwrap_err();
        assert!(err.to_string().contains(ERR_MANY), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), body.into_bytes());
    }

    #[test]
    fn missing_file_gets_stamp_then_refresh_keeps_block_and_note() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("evidence.md");
        record_evidence(&path, "Ada", "Ship it.", false, at(T1)).unwrap();
        let created = std::fs::read_to_string(&path).unwrap();
        let block = active_block("Ada", T1, "Ship it.");
        let version = evidence_stamp::version_line();
        assert_eq!(created, format!("{version}\n{T1}\n{block}"));
        let noted = format!("{version}\n{T1}\nNOTE\n{block}");
        std::fs::write(&path, &noted).unwrap();
        evidence_stamp::refresh_evidence_file(&path, &version, T2).unwrap();
        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, format!("{version}\n{T2}\nNOTE\n{block}"));
        assert!(got.contains("Ship it."));
    }

    #[test]
    fn unrecognized_body_is_appended_then_refresh_prepends_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("evidence.md");
        let original = "operator note\r\nkept";
        std::fs::write(&path, original).unwrap();
        record_evidence(&path, "Ada", "Ship it.", false, at(T1)).unwrap();
        let recorded = std::fs::read_to_string(&path).unwrap();
        let block = active_block("Ada", T1, "Ship it.");
        assert_eq!(recorded, format!("{original}\n{block}"));
        assert!(!recorded.starts_with("coordinator "));
        let version = evidence_stamp::version_line();
        evidence_stamp::refresh_evidence_file(&path, &version, T2).unwrap();
        let got = std::fs::read_to_string(&path).unwrap();
        assert!(got.starts_with(&format!("{version}\n{T2}\n\n")), "{got}");
        assert!(got.contains(original), "{got}");
        assert!(got.contains("Ship it."), "{got}");
        assert!(got.contains(CLOSER), "{got}");
    }

    #[test]
    fn recognized_stamp_bytes_and_clock_stay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("evidence.md");
        let stamp = format!("coordinator 9.9.9\r\n{EARLY}\r\n");
        let original = format!("{stamp}note\r\n");
        std::fs::write(&path, &original).unwrap();
        record_evidence(&path, "Ada", "Ship it.", false, at(T1)).unwrap();
        let got = std::fs::read_to_string(&path).unwrap();
        assert!(got.starts_with(&stamp), "{got}");
        assert!(got.contains("note\r\n"), "{got}");
        assert!(got.contains(&format!("recorded_at=\"{T1}\"")), "{got}");
        assert!(
            !got.contains(&format!("\n{T1}\n")) || got.contains("recorded_at"),
            "{got}"
        );
        assert!(got.lines().nth(1).unwrap().trim_end_matches('\r') == EARLY);
    }

    #[test]
    fn empty_existing_file_is_not_the_stamp_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("evidence.md");
        std::fs::write(&path, "").unwrap();
        record_evidence(&path, "Ada", "Ship it.", false, at(T1)).unwrap();
        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, format!("\n{}", active_block("Ada", T1, "Ship it.")));
        assert!(!got.contains("coordinator "));
    }

    #[test]
    fn notes_before_and_after_survive_record_and_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("evidence.md");
        let version = evidence_stamp::version_line();
        let old = active_block("Ada", EARLY, "old sentence");
        let original = format!("{version}\n{EARLY}\nBEFORE\n{old}AFTER\n");
        std::fs::write(&path, &original).unwrap();
        record_evidence(&path, "Bea", "new sentence", true, at(T1)).unwrap();
        evidence_stamp::refresh_evidence_file(&path, &version, T2).unwrap();
        let got = std::fs::read_to_string(&path).unwrap();
        assert!(got.starts_with(&format!("{version}\n{T2}\n")), "{got}");
        assert!(got.contains("BEFORE\n"), "{got}");
        assert!(got.contains("AFTER\n"), "{got}");
        assert!(got.contains("old sentence"), "{got}");
        assert!(got.contains("new sentence"), "{got}");
        assert!(
            got.contains(&format!(
                "superseded by=\"Ada\" recorded_at=\"{EARLY}\" superseded_at=\"{T1}\""
            )),
            "{got}"
        );
        assert!(got.contains("active by=\"Bea\""), "{got}");
        assert_eq!(got.matches("old sentence").count(), 1, "{got}");
    }

    #[test]
    fn non_utf8_file_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("evidence.md");
        let bytes = [0xFFu8, 0xFE, 0x00];
        std::fs::write(&path, bytes).unwrap();
        let err = record_evidence(&path, "Ada", "Ship it.", false, at(T1)).unwrap_err();
        assert!(err.to_string().contains(ERR_UTF8), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn inject_show_and_list_cover_none_one_many_and_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let conductor = dir.path().join("conductor");
        let track = conductor.join("0077-Example");
        std::fs::create_dir_all(&track).unwrap();
        let rec = project(dir.path(), &conductor);
        let path = track.join("evidence.md");

        assert_eq!(inject_for(&rec, Some("0077")), "active decision: none\n");
        assert_eq!(
            show_for(&rec, "0077", false).unwrap(),
            "active decision: none\n"
        );
        assert_eq!(show_for(&rec, "0077", true).unwrap(), "{\"active\":null}\n");
        assert_eq!(list_for(&rec, "0077", false).unwrap(), "decisions: none\n");
        assert_eq!(list_for(&rec, "0077", true).unwrap(), "{\"blocks\":[]}\n");

        std::fs::write(&path, "prose only\n").unwrap();
        assert_eq!(inject_for(&rec, Some("0077")), "active decision: none\n");
        let before = std::fs::read(&path).unwrap();
        assert_eq!(inject_for(&rec, Some("0077")), "active decision: none\n");
        assert_eq!(std::fs::read(&path).unwrap(), before);

        std::fs::write(&path, active_block("Ada", T1, "Ship it.")).unwrap();
        let shown = show_for(&rec, "0077", false).unwrap();
        assert_eq!(
            shown,
            format!("active decision by=\"Ada\" recorded_at=\"{T1}\":\nShip it.\n")
        );
        assert_eq!(inject_for(&rec, Some("0077")), shown);
        let json = show_for(&rec, "0077", true).unwrap();
        assert!(json.contains("\"by\":\"Ada\""), "{json}");
        assert!(json.contains("\"sentence\":\"Ship it.\""), "{json}");

        let many = format!(
            "{}{}",
            active_block("Ada", T1, "one"),
            active_block("Bea", T1, "two")
        );
        std::fs::write(&path, &many).unwrap();
        assert_eq!(inject_for(&rec, Some("0077")), "active decision: invalid\n");
        assert!(
            show_for(&rec, "0077", false)
                .unwrap_err()
                .to_string()
                .contains(ERR_MANY)
        );
        let listed = list_for(&rec, "0077", false).unwrap();
        assert!(listed.contains("status=\"active\" by=\"Ada\""), "{listed}");
        assert!(listed.contains("status=\"active\" by=\"Bea\""), "{listed}");
        let listed_json = list_for(&rec, "0077", true).unwrap();
        assert!(listed_json.contains("\"blocks\":["), "{listed_json}");
        assert!(
            listed_json.contains("\"sentence\":\"one\""),
            "{listed_json}"
        );
        assert!(
            listed_json.contains("\"sentence\":\"two\""),
            "{listed_json}"
        );

        std::fs::write(&path, "<!-- coordinator:decision nope -->\n").unwrap();
        assert_eq!(inject_for(&rec, Some("0077")), "active decision: invalid\n");
        assert!(
            list_for(&rec, "0077", false)
                .unwrap_err()
                .to_string()
                .contains(ERR_MALFORMED)
        );
    }

    #[test]
    fn missing_track_directory_is_an_error_and_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let conductor = dir.path().join("conductor");
        std::fs::create_dir_all(&conductor).unwrap();
        let rec = project(dir.path(), &conductor);
        let err = record_for(&rec, "0077", "Ada", "Ship it.", false, at(T1)).unwrap_err();
        assert!(
            err.to_string().contains("no track directory for 0077"),
            "{err}"
        );
        assert!(!conductor.join("0077").exists());
        assert!(!conductor.join("0077-Example").exists());
    }

    #[test]
    fn by_rejects_empty_quotes_and_newlines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("evidence.md");
        for by in ["", "a\"b", "a\nb", "a\rb"] {
            let err = record_evidence(&path, by, "Ship it.", false, at(T1)).unwrap_err();
            assert!(err.to_string().contains(ERR_BY), "{by}: {err}");
            assert!(!path.exists());
        }
    }
}
