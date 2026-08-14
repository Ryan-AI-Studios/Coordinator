//! Fail-closed Verdict / Findings parser for the cross-model gate.

use super::backend::ReviewResult;

/// Parsed report contract (before fall-through classification).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParsedVerdict {
    Pass,
    PassWithLows,
    GateFail,
    Unparseable,
}

/// Backend / chain classification after parse + keywords + exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierClass {
    Pass,
    PassWithLows,
    GateFail,
    Exhaustion,
    Permission,
    Crash,
}

pub fn parse_report(last_message: &str, stdout: &str) -> ParsedVerdict {
    let primary = if !last_message.trim().is_empty() {
        last_message
    } else {
        stdout
    };
    let mut parsed = parse_json(last_message)
        .or_else(|| parse_json(stdout))
        .unwrap_or_else(|| parse_markdown(primary));
    if matches!(parsed, ParsedVerdict::Pass | ParsedVerdict::PassWithLows)
        && (findings_block(last_message) || findings_block(stdout))
    {
        parsed = ParsedVerdict::GateFail;
    }
    parsed
}

pub fn classify_result(result: &ReviewResult) -> TierClass {
    let parsed = parse_report(&result.last_message, &result.stdout);
    // A dirty Verdict is the gate (no fallback), even if the CLI exited nonzero.
    if parsed == ParsedVerdict::GateFail {
        return TierClass::GateFail;
    }
    // Nonzero / timeout-kill is crash-class fall-through. Do not accept PASS
    // from a failed process (leftover last-message or template dump).
    if result.exit != 0 {
        return classify_unusable(result.exit, &blob(result));
    }
    match parsed {
        ParsedVerdict::Pass => TierClass::Pass,
        ParsedVerdict::PassWithLows => TierClass::PassWithLows,
        ParsedVerdict::GateFail => TierClass::GateFail,
        ParsedVerdict::Unparseable => classify_unusable(result.exit, &blob(result)),
    }
}

pub fn classify_error(err: &str) -> TierClass {
    classify_unusable(1, err)
}

fn blob(result: &ReviewResult) -> String {
    format!(
        "{}\n{}\n{}",
        result.last_message, result.stdout, result.stderr
    )
}

fn classify_unusable(exit: i32, text: &str) -> TierClass {
    if looks_exhausted(text) {
        return TierClass::Exhaustion;
    }
    if looks_permission(text) {
        return TierClass::Permission;
    }
    let _ = exit;
    TierClass::Crash
}

fn looks_exhausted(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("quota")
        || t.contains("rate limit")
        || t.contains("rate-limit")
        || t.contains("resource_exhausted")
        || t.contains("usage limit")
        || t.contains("exhausted")
}

fn looks_permission(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("not logged in")
        || t.contains("not authenticated")
        || t.contains("auth required")
        || t.contains("authentication required")
        || t.contains("please log in")
        || t.contains("login required")
        || t.contains("command not found")
        || t.contains("not found on path")
        || t.contains("missing binary")
        || t.contains("refusing to spawn .ps1")
        || t.contains("refusing to spawn shim")
}

fn parse_json(text: &str) -> Option<ParsedVerdict> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let value: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return None,
    };
    let verdict = value.get("verdict")?.as_str()?;
    let mut parsed = match normalize_verdict_token(verdict) {
        Some(v) => v,
        None => return Some(ParsedVerdict::Unparseable),
    };
    if let Some(highest) = value.get("highest").and_then(|h| h.as_str())
        && is_blocking_severity_token(highest.trim())
    {
        parsed = ParsedVerdict::GateFail;
    }
    Some(parsed)
}

fn parse_markdown(text: &str) -> ParsedVerdict {
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(rest) = strip_verdict_heading(trimmed) else {
            continue;
        };
        if let Some(v) = normalize_verdict_token(rest) {
            return v;
        }
        return ParsedVerdict::Unparseable;
    }
    ParsedVerdict::Unparseable
}

fn strip_verdict_heading(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    if bytes.len() < 10 {
        return None;
    }
    if !line.starts_with("##") {
        return None;
    }
    let after_hash = line[2..].trim_start();
    let (label, rest) = after_hash.split_once(':')?;
    if !label.eq_ignore_ascii_case("verdict") {
        return None;
    }
    Some(rest.trim())
}

fn normalize_verdict_token(raw: &str) -> Option<ParsedVerdict> {
    let t = raw
        .trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == '`');
    let upper = t.to_ascii_uppercase();
    if upper.starts_with("PASS WITH DEFERRED") || upper.starts_with("PASS WITH LOWS") {
        return Some(ParsedVerdict::PassWithLows);
    }
    if upper == "PASS_WITH_DEFERRED_P3" || upper == "PASS_WITH_LOWS" {
        return Some(ParsedVerdict::PassWithLows);
    }
    if upper == "PASS" {
        return Some(ParsedVerdict::Pass);
    }
    if upper.starts_with("FAIL") {
        return Some(ParsedVerdict::GateFail);
    }
    None
}

fn findings_block(text: &str) -> bool {
    let Some(slice) = findings_slice(text) else {
        return false;
    };
    for line in slice.lines() {
        if line_has_blocking_severity(line) {
            return true;
        }
    }
    false
}

/// Slice from line-start `## Findings` to the next line-start `## ` (not `###`).
/// Fenced code is omitted (headings and severity tokens inside fences are ignored).
fn findings_slice(text: &str) -> Option<String> {
    let mut in_fence = false;
    let mut collecting = false;
    let mut out = String::new();
    for line in text.lines() {
        if fence_toggle(line) {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if !collecting {
            if is_findings_heading(line) {
                collecting = true;
            }
            continue;
        }
        if is_h2_heading(line) {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    collecting.then_some(out)
}

fn fence_toggle(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("```") || t.starts_with("~~~")
}

fn is_findings_heading(line: &str) -> bool {
    let t = line.trim_start();
    let Some(rest) = t.strip_prefix("##") else {
        return false;
    };
    if rest.starts_with('#') {
        return false;
    }
    rest.trim_start()
        .to_ascii_lowercase()
        .starts_with("findings")
}

fn is_h2_heading(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("##") && !t.starts_with("###")
}

fn line_has_blocking_severity(line: &str) -> bool {
    if let Some(rest) = split_severity_label(line)
        && is_blocking_severity_token(rest.trim())
    {
        return true;
    }
    for cell in line.split('|').skip(1) {
        if is_blocking_severity_token(cell.trim()) {
            return true;
        }
    }
    for token in bold_tokens(line) {
        if is_blocking_severity_token(&token) {
            return true;
        }
    }
    false
}

fn split_severity_label(line: &str) -> Option<&str> {
    let lower = line.to_ascii_lowercase();
    let idx = lower.find("severity:")?;
    Some(&line[idx + "severity:".len()..])
}

fn bold_tokens(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == b'*'
            && bytes[i + 1] == b'*'
            && let Some(end) = line[i + 2..].find("**")
        {
            out.push(line[i + 2..i + 2 + end].to_string());
            i += 4 + end;
            continue;
        }
        i += 1;
    }
    out
}

fn is_blocking_severity_token(raw: &str) -> bool {
    let t = raw
        .trim()
        .trim_matches(|c: char| c == '*' || c == '`' || c == '"' || c == '\'')
        .to_ascii_lowercase();
    matches!(
        t.as_str(),
        "p0" | "p1" | "p2" | "critical" | "high" | "medium"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::backend::ReviewResult;

    fn res(last: &str) -> ReviewResult {
        ReviewResult {
            exit: 0,
            stdout: last.into(),
            stderr: String::new(),
            last_message: last.into(),
        }
    }

    #[test]
    fn markdown_pass() {
        assert_eq!(parse_report("## Verdict: PASS\n", ""), ParsedVerdict::Pass);
    }

    #[test]
    fn markdown_pass_with_deferred() {
        assert_eq!(
            parse_report("## Verdict: PASS WITH DEFERRED P3\n", ""),
            ParsedVerdict::PassWithLows
        );
    }

    #[test]
    fn markdown_pass_with_lows() {
        assert_eq!(
            parse_report("  ## verdict: PASS WITH LOWS\n", ""),
            ParsedVerdict::PassWithLows
        );
    }

    #[test]
    fn markdown_fail() {
        assert_eq!(
            parse_report("## Verdict: FAIL\n", ""),
            ParsedVerdict::GateFail
        );
    }

    #[test]
    fn json_verdict_pass() {
        let j = r#"{"verdict":"PASS"}"#;
        assert_eq!(parse_report(j, ""), ParsedVerdict::Pass);
    }

    #[test]
    fn json_verdict_deferred() {
        let j = r#"{"verdict":"PASS_WITH_DEFERRED_P3","highest":"low"}"#;
        assert_eq!(parse_report(j, ""), ParsedVerdict::PassWithLows);
    }

    #[test]
    fn json_highest_medium_overrides_pass() {
        let j = r#"{"verdict":"PASS","highest":"medium"}"#;
        assert_eq!(parse_report(j, ""), ParsedVerdict::GateFail);
    }

    #[test]
    fn empty_is_unparseable() {
        assert_eq!(parse_report("", ""), ParsedVerdict::Unparseable);
        assert_eq!(
            parse_report("no heading here", ""),
            ParsedVerdict::Unparseable
        );
    }

    #[test]
    fn findings_p1_overrides_pass() {
        let text = r#"
## Verdict: PASS

## Findings

| Sev | Item |
| P1 | missing lock |
"#;
        assert_eq!(parse_report(text, ""), ParsedVerdict::GateFail);
    }

    #[test]
    fn findings_p1_in_fence_does_not_override() {
        let text = r#"
## Verdict: PASS

## Findings

```
| P1 | example only |
```

- leftover low
"#;
        assert_eq!(parse_report(text, ""), ParsedVerdict::Pass);
    }

    #[test]
    fn severity_legend_outside_findings_does_not_override() {
        let text = r#"
## Verdict: PASS

## Severity

| P0 | critical |
| P1 | high |
| P2 | medium |

## Findings

- none
"#;
        assert_eq!(parse_report(text, ""), ParsedVerdict::Pass);
    }

    #[test]
    fn findings_bold_medium_overrides() {
        let text = "## Verdict: PASS\n\n## Findings\n\n- **medium** leftover lock\n";
        assert_eq!(parse_report(text, ""), ParsedVerdict::GateFail);
    }

    #[test]
    fn unparseable_exit_zero_is_crash() {
        assert_eq!(classify_result(&res("garbage")), TierClass::Crash);
    }

    #[test]
    fn exhaustion_keywords() {
        let mut r = res("");
        r.stderr = "API quota exceeded".into();
        assert_eq!(classify_result(&r), TierClass::Exhaustion);
    }

    #[test]
    fn permission_keywords() {
        let mut r = res("");
        r.stderr = "Error: not logged in".into();
        assert_eq!(classify_result(&r), TierClass::Permission);
    }

    #[test]
    fn nonzero_exit_with_pass_is_crash() {
        let mut r = res("## Verdict: PASS\n");
        r.exit = 1;
        assert_eq!(classify_result(&r), TierClass::Crash);
    }

    #[test]
    fn timeout_kill_with_pass_is_crash() {
        let mut r = res("## Verdict: PASS\n");
        r.exit = 124;
        r.stderr = "review CLI timed out after 60s".into();
        assert_eq!(classify_result(&r), TierClass::Crash);
    }

    #[test]
    fn gate_fail_still_wins_on_nonzero_exit() {
        let mut r = res("## Verdict: FAIL\n");
        r.exit = 1;
        assert_eq!(classify_result(&r), TierClass::GateFail);
    }
}
