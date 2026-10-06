//! Plan-phase stamp lines on `evidence.md`. The body after the stamp is preserved.

use std::path::Path;

use crate::error::Result;

/// `coordinator {CARGO_PKG_VERSION}`, the same text `coordinator --version` prints.
pub(crate) fn version_line() -> String {
    format!("coordinator {}", env!("CARGO_PKG_VERSION"))
}

/// UTC stamp `YYYY-MM-DDTHH:MM:SSZ` (chrono 0.4 `to_rfc3339_opts`, seconds, `Z`).
pub(crate) fn utc_stamp_line(now: chrono::DateTime<chrono::Utc>) -> String {
    now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Refresh `path`. A missing file is created. A non-UTF-8 file fails and is left as it was.
pub(crate) fn refresh_evidence_file(path: &Path, version_line: &str, utc_line: &str) -> Result<()> {
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err.into()),
    };
    let merged = merge_evidence_stamp(&existing, version_line, utc_line);
    crate::persist::atomic_write(path, merged.as_bytes())
}

/// Replace a recognized two-line stamp. Anything else is kept after a new stamp.
pub(crate) fn merge_evidence_stamp(existing: &str, version_line: &str, utc_line: &str) -> String {
    if existing.is_empty() {
        return format!("{version_line}\n{utc_line}\n");
    }
    if let Some(body) = recognized_stamp_body(existing) {
        return format!("{version_line}\n{utc_line}\n{body}");
    }
    format!("{version_line}\n{utc_line}\n\n{existing}")
}

/// Bytes after the second stamp line's ending, when lines 1–2 match the stamp grammar.
fn recognized_stamp_body(existing: &str) -> Option<&str> {
    let (line1, rest) = split_line(existing)?;
    let (line2, body) = split_line(rest)?;
    if is_version_line(line1) && is_utc_line(line2) {
        Some(body)
    } else {
        None
    }
}

/// Split one line. The line text omits an optional CR. The remainder starts after `\n`.
fn split_line(text: &str) -> Option<(&str, &str)> {
    if text.is_empty() {
        return None;
    }
    match text.find('\n') {
        Some(index) => {
            let mut line = &text[..index];
            if let Some(stripped) = line.strip_suffix('\r') {
                line = stripped;
            }
            Some((line, &text[index + 1..]))
        }
        None => Some((text.strip_suffix('\r').unwrap_or(text), "")),
    }
}

fn is_version_line(line: &str) -> bool {
    let Some(token) = line.strip_prefix("coordinator ") else {
        return false;
    };
    !token.is_empty() && !token.chars().any(char::is_whitespace)
}

fn is_utc_line(line: &str) -> bool {
    let bytes = line.as_bytes();
    if bytes.len() != 20 {
        return false;
    }
    let digit = |index: usize| bytes[index].is_ascii_digit();
    digit(0)
        && digit(1)
        && digit(2)
        && digit(3)
        && bytes[4] == b'-'
        && digit(5)
        && digit(6)
        && bytes[7] == b'-'
        && digit(8)
        && digit(9)
        && bytes[10] == b'T'
        && digit(11)
        && digit(12)
        && bytes[13] == b':'
        && digit(14)
        && digit(15)
        && bytes[16] == b':'
        && digit(17)
        && digit(18)
        && bytes[19] == b'Z'
}

#[cfg(test)]
mod tests {
    use super::*;

    const VERSION: &str = "coordinator 0.1.0";
    const EARLY: &str = "2020-01-01T00:00:00Z";
    const LATER: &str = "2026-10-06T03:18:02Z";

    #[test]
    fn empty_input_is_two_stamp_lines() {
        assert_eq!(
            merge_evidence_stamp("", VERSION, LATER),
            format!("{VERSION}\n{LATER}\n")
        );
    }

    #[test]
    fn two_refreshes_keep_the_body_marker() {
        let once = merge_evidence_stamp(
            &format!("{VERSION}\n{EARLY}\nOWNER-MARKER\n"),
            VERSION,
            "2026-10-06T01:00:00Z",
        );
        let twice = merge_evidence_stamp(&once, VERSION, LATER);
        assert_eq!(twice, format!("{VERSION}\n{LATER}\nOWNER-MARKER\n"));
        assert!(!twice.contains(EARLY));
        assert!(!twice.contains("2026-10-06T01:00:00Z"));
    }

    #[test]
    fn stamp_only_file_gains_no_owner_sentence() {
        let got = merge_evidence_stamp(&format!("{VERSION}\n{EARLY}\n"), VERSION, LATER);
        assert_eq!(got, format!("{VERSION}\n{LATER}\n"));
        assert!(!got.contains("Owner"));
        assert!(!got.contains("OWNER-MARKER"));
    }

    #[test]
    fn unrecognized_content_is_prepended_not_dropped() {
        let existing = "not a stamp\nOWNER-MARKER";
        let got = merge_evidence_stamp(existing, VERSION, LATER);
        assert_eq!(got, format!("{VERSION}\n{LATER}\n\n{existing}"));
    }

    #[test]
    fn crlf_stamp_lines_keep_the_body_bytes() {
        let existing = "coordinator 0.1.0\r\n2026-10-06T00:00:00Z\r\nOWNER-MARKER\r\nkept";
        let got = merge_evidence_stamp(existing, VERSION, LATER);
        assert_eq!(got, format!("{VERSION}\n{LATER}\nOWNER-MARKER\r\nkept"));
    }

    #[test]
    fn version_line_matches_package_version() {
        assert_eq!(
            version_line(),
            format!("coordinator {}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn utc_stamp_line_is_seconds_with_zulu() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-06T03:18:02Z")
            .unwrap()
            .to_utc();
        assert_eq!(utc_stamp_line(now), LATER);
    }

    #[test]
    fn file_refresh_preserves_marker_across_two_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("evidence.md");
        std::fs::write(&path, format!("{VERSION}\n{EARLY}\nOWNER-MARKER\n")).unwrap();
        refresh_evidence_file(&path, VERSION, "2026-10-06T01:00:00Z").unwrap();
        refresh_evidence_file(&path, VERSION, LATER).unwrap();
        let got = std::fs::read(&path).unwrap();
        assert_eq!(
            got,
            format!("{VERSION}\n{LATER}\nOWNER-MARKER\n").into_bytes()
        );
    }

    #[test]
    fn missing_file_is_created_as_stamp_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("evidence.md");
        refresh_evidence_file(&path, VERSION, LATER).unwrap();
        let got = std::fs::read(&path).unwrap();
        assert_eq!(got, format!("{VERSION}\n{LATER}\n").into_bytes());
        assert!(!got.starts_with(&[0xEF, 0xBB, 0xBF]));
    }
}
