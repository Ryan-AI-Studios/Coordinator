//! Family predicates for product-owned review slot bodies (track 0043).
//!
//! Production floor is 512 bytes. Tests default to 1 so existing tiny fixtures
//! stay valid unless a test opts into the production floor.

use crate::review::parse::is_schema_only_json;
use crate::workflow::bundle::normalize_newlines;

pub const PROD_MIN_BODY_BYTES: usize = 512;

#[cfg(test)]
thread_local! {
    static TEST_MIN_BODY_BYTES: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotFamily {
    PlanReview,
    CrossModel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dud {
    pub bytes: usize,
    pub reason: &'static str,
}

pub fn min_body_bytes() -> usize {
    #[cfg(test)]
    {
        if let Some(n) = TEST_MIN_BODY_BYTES.with(|c| c.get()) {
            return n;
        }
        1
    }
    #[cfg(not(test))]
    {
        PROD_MIN_BODY_BYTES
    }
}

#[cfg(test)]
pub fn set_test_min_body_bytes(n: Option<usize>) {
    TEST_MIN_BODY_BYTES.with(|c| c.set(n));
}

pub fn dud_message(file: &str, bytes: usize) -> String {
    format!("slot: dud {file} ({bytes})")
}

pub fn check(family: SlotFamily, body: &str, track_id: Option<&str>) -> Result<(), Dud> {
    let text = normalize_newlines(body);
    let bytes = text.len();
    match family {
        SlotFamily::PlanReview => check_plan_review(&text, track_id, bytes),
        SlotFamily::CrossModel => check_cross_model(&text, bytes),
    }
}

fn check_plan_review(text: &str, track_id: Option<&str>, bytes: usize) -> Result<(), Dud> {
    if bytes < min_body_bytes() {
        return Err(Dud {
            bytes,
            reason: "too small",
        });
    }
    if !text.to_ascii_lowercase().contains("# track review:") {
        return Err(Dud {
            bytes,
            reason: "missing # Track review:",
        });
    }
    if let Some(id) = track_id
        && !id.is_empty()
        && !text.contains(id)
    {
        return Err(Dud {
            bytes,
            reason: "missing track id",
        });
    }
    Ok(())
}

fn check_cross_model(text: &str, bytes: usize) -> Result<(), Dud> {
    if is_schema_only_json(text) {
        return Err(Dud {
            bytes,
            reason: "schema-only verdict JSON",
        });
    }
    if bytes < min_body_bytes() {
        return Err(Dud {
            bytes,
            reason: "too small",
        });
    }
    if !has_heading(text, "verdict") {
        return Err(Dud {
            bytes,
            reason: "missing ## Verdict:",
        });
    }
    if !has_heading(text, "findings")
        && !has_heading(text, "scope reviewed")
        && !heading_starts_with(text, "requirement")
    {
        return Err(Dud {
            bytes,
            reason: "missing required section",
        });
    }
    Ok(())
}

fn has_heading(text: &str, label: &str) -> bool {
    heading_starts_with(text, label)
}

fn heading_starts_with(text: &str, label: &str) -> bool {
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("##") {
            continue;
        }
        let after = trimmed[2..].trim_start();
        let name = after.split_once(':').map(|(n, _)| n).unwrap_or(after);
        if name.trim().to_ascii_lowercase().starts_with(label) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MinGuard;
    impl Drop for MinGuard {
        fn drop(&mut self) {
            set_test_min_body_bytes(None);
        }
    }

    #[test]
    fn plan_review_heading_and_id() {
        assert!(
            check(
                SlotFamily::PlanReview,
                "# Track review: 0001-Example\n",
                Some("0001")
            )
            .is_ok()
        );
        assert!(
            check(
                SlotFamily::PlanReview,
                "# Track Review: 0001-Example\n",
                Some("0001")
            )
            .is_ok()
        );
        assert!(
            check(
                SlotFamily::PlanReview,
                "# Track review: 0001-Example\n",
                Some("0037")
            )
            .is_err()
        );
        assert!(
            check(
                SlotFamily::PlanReview,
                "Which track should I review?\n",
                Some("0001")
            )
            .is_err()
        );
    }

    #[test]
    fn cross_model_requires_verdict_and_section() {
        assert!(
            check(
                SlotFamily::CrossModel,
                "## Verdict: PASS\n\n## Findings\n\nnone\n",
                None
            )
            .is_ok()
        );
        assert!(
            check(
                SlotFamily::CrossModel,
                "## Verdict: PASS\n\n## Scope Reviewed\n\ntrack\n",
                None
            )
            .is_ok()
        );
        assert!(
            check(
                SlotFamily::CrossModel,
                "## Verdict: PASS\n\n## Requirement and DoD Matrix\n\nx\n",
                None
            )
            .is_ok()
        );
        assert!(check(SlotFamily::CrossModel, "## Verdict: PASS\n", None).is_err());
    }

    #[test]
    fn schema_only_json_is_always_dud() {
        let pass = r#"{"verdict":"PASS","highest":"None"}"#;
        let deferred = r#"{"verdict":"PASS_WITH_DEFERRED_P3","highest":"low"}"#;
        let fail = r#"{"verdict":"FAIL","highest":"P1"}"#;
        for body in [pass, deferred, fail] {
            let err = check(SlotFamily::CrossModel, body, None).unwrap_err();
            assert_eq!(err.reason, "schema-only verdict JSON");
            assert_eq!(err.bytes, body.len());
        }
    }

    #[test]
    fn production_floor_rejects_tiny_good_shape() {
        let _g = MinGuard;
        set_test_min_body_bytes(Some(PROD_MIN_BODY_BYTES));
        let tiny = "## Verdict: PASS\n\n## Findings\n\nnone\n";
        assert!(tiny.len() < PROD_MIN_BODY_BYTES);
        assert!(check(SlotFamily::CrossModel, tiny, None).is_err());
        let mut padded = String::from("## Verdict: PASS\n\n## Findings\n\n");
        padded.push_str(&"x".repeat(PROD_MIN_BODY_BYTES));
        assert!(check(SlotFamily::CrossModel, &padded, None).is_ok());
    }

    #[test]
    fn dud_message_shape() {
        assert_eq!(
            dud_message("review.claude.md", 32),
            "slot: dud review.claude.md (32)"
        );
    }
}
