//! Review Bundle assembly (ADR-0022).

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::persist::atomic_write;
use crate::registry::ProjectRecord;
use crate::state::{RunState, resolve_state_dir};

use super::graph::{REVIEW_SLUG_AGY, REVIEW_SLUG_OPENCODE, resolve_track_dir};

/// Normalize `\r\n` and lone `\r` to `\n`.
pub fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

pub fn reviews_dir(record: &ProjectRecord) -> Result<PathBuf> {
    Ok(resolve_state_dir(record)?.join("reviews"))
}

pub fn review_file(record: &ProjectRecord, slug: &str) -> Result<PathBuf> {
    Ok(reviews_dir(record)?.join(format!("{slug}-review.md")))
}

/// Assemble `{workspace_root}/AI-review.md` (+ track copy when resolvable).
pub fn assemble(record: &ProjectRecord, state: &RunState) -> Result<PathBuf> {
    let agy = read_review(record, REVIEW_SLUG_AGY)?;
    let oc = read_review(record, REVIEW_SLUG_OPENCODE)?;
    let body = assemble_body(agy.as_deref(), oc.as_deref());
    let paths = crate::layout::resolve(record);
    let dest = paths.workspace_root.join("AI-review.md");
    write_normalized(&dest, &body)?;
    if let Some(ref track_id) = state.track_id
        && let Some(track_dir) = resolve_track_dir(record, track_id)
    {
        let copy = track_dir.join("AI-review.md");
        write_normalized(&copy, &body)?;
    }
    Ok(dest)
}

pub fn assemble_body(agy: Option<&str>, opencode: Option<&str>) -> String {
    let mut out = String::from("# Review Bundle\n\n");
    append_section(&mut out, REVIEW_SLUG_AGY, agy);
    append_section(&mut out, REVIEW_SLUG_OPENCODE, opencode);
    normalize_newlines(&out)
}

fn append_section(out: &mut String, slug: &str, body: Option<&str>) {
    match body {
        Some(text) if !text.trim().is_empty() => {
            out.push_str(&format!("## {slug}\n\n"));
            out.push_str(normalize_newlines(text).trim_end());
            out.push_str("\n\n");
        }
        Some(_) | None => {
            out.push_str(&format!("## {slug} (degraded — not produced)\n\n"));
        }
    }
}

fn read_review(record: &ProjectRecord, slug: &str) -> Result<Option<String>> {
    let path = review_file(record, slug)?;
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(std::fs::read_to_string(path)?))
}

fn write_normalized(path: &Path, body: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write(path, normalize_newlines(body).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_line_endings_normalize() {
        let agy = "line1\r\n\r\nline2\r\n";
        let oc = "a\rb\r\nc";
        let body = assemble_body(Some(agy), Some(oc));
        assert!(!body.contains('\r'));
        assert!(body.contains("## agy\n\nline1\n\nline2\n"));
        assert!(body.contains("## opencode\n\na\nb\nc\n"));
    }

    #[test]
    fn degraded_heading_when_missing() {
        let body = assemble_body(Some("ok"), None);
        assert!(body.contains("## agy\n\nok\n"));
        assert!(body.contains("## opencode (degraded — not produced)"));
    }
}
