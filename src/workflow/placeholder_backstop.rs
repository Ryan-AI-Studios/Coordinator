//! 0106: promote a completed placeholder when no run will re-enter the fold.
//!
//! The fold promotes only inside a live fold leg (`workflow::on_success`). A stopped or
//! idle project with a real spec/plan never gets there. This tick is the no-run half:
//! it runs on the serve tick, only behind the shipped 0102 `Duty::AutoStart` veto, and
//! only after a locked re-read shows the project is not `Running`/`Paused`. It writes the
//! Status cell and nothing else; it never starts a run. Promotion is independent of the
//! per-project `auto_start` policy (0103 still starts `full` only).

use crate::features::{Duty, is_enabled};
use crate::registry::ProjectRecord;
use crate::state::{RunStatus, load_run_state, with_run_state_lock};
use crate::workflow::conductor_md::{
    conductor_md_path, parse_conductor_md, placeholder_ready, promote_placeholders_in,
    ready_aliases_for,
};

pub const PROMOTED_KIND: &str = "placeholder-promoted";

/// One stderr line per project+condition per process; the serve tick can be frequent.
static WARNED: std::sync::Mutex<std::collections::BTreeSet<String>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// Returns `true` when this call emitted the line (dedupe key seen for the first time).
fn warn_once(key: String, line: &str) -> bool {
    let first = {
        let mut warned = WARNED.lock().unwrap_or_else(|e| e.into_inner());
        warned.insert(key)
    };
    if first {
        eprintln!("{line}");
    }
    first
}

/// Clear the process-wide warn-once set. Mirrors
/// `features::reset_feature_warnings_for_tests` so a later test cannot be silenced by an
/// earlier one.
#[cfg(test)]
pub(crate) fn reset_backstop_warnings_for_tests() {
    let mut warned = WARNED.lock().unwrap_or_else(|e| e.into_inner());
    warned.clear();
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackstopSkip {
    FeatureOff,
    Active,
    NoCandidate,
    Unreadable,
    Crlf,
    LockFailed,
    WriteFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackstopOutcome {
    Skipped(BackstopSkip),
    Promoted(Vec<String>),
}

pub fn tick(record: &ProjectRecord) -> BackstopOutcome {
    if !is_enabled(Duty::AutoStart) {
        return BackstopOutcome::Skipped(BackstopSkip::FeatureOff);
    }
    // `with_run_state_lock` is `fn(record, f) -> Result<T> where F: FnOnce() -> Result<T>`
    // (src/state.rs:734-737) and returns `f()` directly — one `Result` level, not nested.
    // `tick_locked` therefore returns `BackstopOutcome` and maps its own read failures, so
    // `Unreadable` (inner) and `LockFailed` (outer) stay distinct.
    match with_run_state_lock(record, || Ok(tick_locked(record))) {
        Ok(outcome) => {
            if let BackstopOutcome::Promoted(ids) = &outcome {
                for id in ids {
                    crate::progress_log::append(
                        record,
                        PROMOTED_KIND,
                        &format!(
                            "track={id} placeholder -> ready (spec/plan pass complete; no live run)"
                        ),
                    );
                }
            }
            outcome
        }
        Err(e) => {
            warn_once(
                format!("lock:{}", record.id),
                &format!(
                    "coordinator: placeholder backstop: run-state lock failed for {}: {e}",
                    record.id
                ),
            );
            BackstopOutcome::Skipped(BackstopSkip::LockFailed)
        }
    }
}

/// `spawn_blocking` wrapper, mirroring [`crate::watch::try_auto_start_async`].
pub async fn tick_async(record: &ProjectRecord) -> crate::error::Result<BackstopOutcome> {
    let rec = record.clone();
    tokio::task::spawn_blocking(move || tick(&rec))
        .await
        .map_err(|e| {
            crate::error::CoordinatorError::Message(format!("placeholder backstop task join: {e}"))
        })
}

fn tick_locked(record: &ProjectRecord) -> BackstopOutcome {
    // DoD-2: the refusal is this locked re-read, not a caller-side pre-check.
    let state = match load_run_state(record) {
        Ok(state) => state,
        Err(_) => return BackstopOutcome::Skipped(BackstopSkip::Unreadable),
    };
    if matches!(state.status, RunStatus::Running | RunStatus::Paused) {
        return BackstopOutcome::Skipped(BackstopSkip::Active);
    }
    // D5: exactly ONE registry read. Parse and rewrite the same `text` — do not call
    // `load_track_rows` here (it would read the file a second time and reopen the
    // check-then-write gap the lock does not cover).
    let path = conductor_md_path(record);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) => return BackstopOutcome::Skipped(BackstopSkip::Unreadable),
    };
    if text.contains('\r') {
        return BackstopOutcome::Skipped(BackstopSkip::Crlf);
    }
    let rows = match parse_conductor_md(&text) {
        Ok(rows) => rows,
        Err(_) => return BackstopOutcome::Skipped(BackstopSkip::Unreadable),
    };
    let aliases = ready_aliases_for(record);
    let promotions: Vec<(String, String)> = rows
        .iter()
        .filter(|row| !row.nostart)
        .filter_map(|row| {
            placeholder_ready(record, row, &row.id, &aliases).map(|ready| (row.id.clone(), ready))
        })
        .collect();
    if promotions.is_empty() {
        return BackstopOutcome::Skipped(BackstopSkip::NoCandidate);
    }
    let (promoted, body) = promote_placeholders_in(&text, &promotions);
    let Some(body) = body else {
        return BackstopOutcome::Skipped(BackstopSkip::NoCandidate);
    };
    match crate::persist::atomic_write(&path, body.as_bytes()) {
        Ok(()) => BackstopOutcome::Promoted(promoted),
        Err(e) => {
            warn_once(
                format!("write:{}", record.id),
                &format!(
                    "coordinator: placeholder backstop: write failed for {}: {e}",
                    record.id
                ),
            );
            BackstopOutcome::Skipped(BackstopSkip::WriteFailed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ENV_COORDINATOR_HOME, TestEnvGuard, test_env_lock};
    use crate::registry::ProjectRecord;
    use crate::state::{RunState, RunStatus, load_run_state, save_run_state};
    use std::path::Path;
    use tempfile::{TempDir, tempdir};
    use uuid::Uuid;

    const PROPOSED: &str = "**Proposed — placeholder, needs full spec/plan pass**";
    const READY: &str = "**Ready — not started**";

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

    /// Isolate the machine config and every 0102 env token, then reset warn-once sets.
    fn isolate(home: &Path) {
        for name in crate::features::test_feature_env_names() {
            unsafe {
                std::env::remove_var(name);
            }
        }
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home);
        }
        crate::features::reset_feature_warnings_for_tests();
        crate::workflow::placeholder_backstop::reset_backstop_warnings_for_tests();
    }

    fn write_config(home: &Path, features: serde_json::Value) {
        let body = serde_json::json!({
            "version": 1,
            "scan_roots": [],
            "features": features,
        });
        std::fs::write(home.join("config.json"), body.to_string()).unwrap();
    }

    fn row(key: &str, status: &str, summary: &str) -> String {
        format!("| [{key}]({key}/spec.md) | `.` | {status} | {summary} |\n")
    }

    fn registry(rows: &[String]) -> String {
        let mut body = String::from(
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n",
        );
        for line in rows {
            body.push_str(line);
        }
        body
    }

    fn write_registry(ws: &Path, body: &str) {
        let cond = ws.join("conductor");
        std::fs::create_dir_all(&cond).unwrap();
        std::fs::write(cond.join("conductor.md"), body).unwrap();
    }

    fn write_track(ws: &Path, name: &str, plan: &str) {
        let dir = ws.join("conductor").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("spec.md"), b"# spec\n").unwrap();
        std::fs::write(dir.join("plan.md"), plan).unwrap();
    }

    fn write_state(record: &ProjectRecord, status: RunStatus, track: Option<&str>) {
        let mut state = RunState::idle(&record.id);
        state.status = status;
        state.track_id = track.map(str::to_string);
        state.run_epoch = 3;
        save_run_state(record, &state).unwrap();
    }

    fn md(ws: &Path) -> std::path::PathBuf {
        ws.join("conductor").join("conductor.md")
    }

    fn log(record: &ProjectRecord) -> String {
        std::fs::read_to_string(crate::progress_log::path(record)).unwrap_or_default()
    }

    struct Fx {
        _home: TempDir,
        _ws: TempDir,
        record: ProjectRecord,
    }

    /// Home + workspace + a default-on machine config with no `config.json` (0102 default).
    fn fixture() -> Fx {
        let home = tempdir().unwrap();
        isolate(home.path());
        let ws = tempdir().unwrap();
        let record = rec(ws.path());
        Fx {
            _home: home,
            _ws: ws,
            record,
        }
    }

    fn promoted_registry(before: &str, key: &str) -> String {
        let (promoted, body) = crate::workflow::conductor_md::promote_placeholders_in(
            before,
            &[(key.to_string(), READY.to_string())],
        );
        assert_eq!(promoted, vec![key.to_string()]);
        body.unwrap()
    }

    #[test]
    fn promotes_stopped_planned_placeholder_without_a_run() {
        let _lock = test_env_lock();
        let _env = TestEnvGuard::capture(&crate::features::test_feature_env_names());
        let fx = fixture();
        let record = &fx.record;
        write_registry(
            &record.path,
            &registry(&[row("0020-Fixture", PROPOSED, "fixture")]),
        );
        write_track(&record.path, "0020-Fixture", "# 0020 — Fixture Plan\n");
        write_state(record, RunStatus::Stopped, Some("0099"));
        let state_before = load_run_state(record).unwrap();
        let before = std::fs::read_to_string(md(&record.path)).unwrap();
        // DoD-1: the row is eligible for the fold's own promotion, so this fixture proves
        // both halves on the same bytes before the no-run tick writes.
        assert_eq!(
            crate::workflow::conductor_md::fold_advance(record, Some("0020")),
            crate::workflow::conductor_md::FoldAdvance::PromotePlaceholder {
                key: "0020".into(),
                ready: READY.into(),
            }
        );

        assert_eq!(
            tick(record),
            BackstopOutcome::Promoted(vec!["0020".to_string()])
        );

        let after = std::fs::read_to_string(md(&record.path)).unwrap();
        assert_eq!(after, promoted_registry(&before, "0020"));
        assert!(after.contains(READY), "{after}");
        let status = log(record);
        assert!(status.contains(PROMOTED_KIND), "status.md was: {status}");
        assert!(!status.contains("fold-promoted"), "status.md was: {status}");
        assert!(!status.contains("start  track="), "status.md was: {status}");
        let state_after = load_run_state(record).unwrap();
        assert_eq!(state_after.status, state_before.status);
        assert_eq!(state_after.track_id, state_before.track_id);
        assert_eq!(state_after.run_epoch, state_before.run_epoch);
        assert_eq!(state_after.last_event, state_before.last_event);
    }

    #[test]
    fn promotes_idle_planned_placeholder() {
        let _lock = test_env_lock();
        let _env = TestEnvGuard::capture(&crate::features::test_feature_env_names());
        let fx = fixture();
        let record = &fx.record;
        write_registry(
            &record.path,
            &registry(&[row("0020-Fixture", PROPOSED, "fixture")]),
        );
        write_track(&record.path, "0020-Fixture", "# 0020 — Fixture Plan\n");

        assert_eq!(
            tick(record),
            BackstopOutcome::Promoted(vec!["0020".to_string()])
        );
        assert!(load_run_state(record).unwrap().status == RunStatus::Idle);
    }

    #[test]
    fn promotes_under_durable_failure() {
        let _lock = test_env_lock();
        let _env = TestEnvGuard::capture(&crate::features::test_feature_env_names());
        let fx = fixture();
        let record = &fx.record;
        write_registry(
            &record.path,
            &registry(&[row("0020-Fixture", PROPOSED, "fixture")]),
        );
        write_track(&record.path, "0020-Fixture", "# 0020 — Fixture Plan\n");
        let mut state = RunState::idle(&record.id);
        state.status = RunStatus::Stopped;
        state.track_id = Some("0099".into());
        state.failure_class = Some(crate::outcome::FailureClass::HarnessCrash);
        save_run_state(record, &state).unwrap();

        assert!(matches!(tick(record), BackstopOutcome::Promoted(_)));

        let after = load_run_state(record).unwrap();
        assert_eq!(
            after.failure_class,
            Some(crate::outcome::FailureClass::HarnessCrash),
            "promotion must not clear the durable failure"
        );
    }

    #[test]
    fn skips_running_run() {
        let _lock = test_env_lock();
        let _env = TestEnvGuard::capture(&crate::features::test_feature_env_names());
        let fx = fixture();
        let record = &fx.record;
        write_registry(
            &record.path,
            &registry(&[row("0020-Fixture", PROPOSED, "fixture")]),
        );
        write_track(&record.path, "0020-Fixture", "# 0020 — Fixture Plan\n");
        write_state(record, RunStatus::Running, Some("0020"));
        let before = std::fs::read(md(&record.path)).unwrap();

        assert_eq!(tick(record), BackstopOutcome::Skipped(BackstopSkip::Active));
        assert_eq!(std::fs::read(md(&record.path)).unwrap(), before);
        assert!(!log(record).contains(PROMOTED_KIND));
    }

    #[test]
    fn skips_paused_run() {
        let _lock = test_env_lock();
        let _env = TestEnvGuard::capture(&crate::features::test_feature_env_names());
        let fx = fixture();
        let record = &fx.record;
        write_registry(
            &record.path,
            &registry(&[row("0020-Fixture", PROPOSED, "fixture")]),
        );
        write_track(&record.path, "0020-Fixture", "# 0020 — Fixture Plan\n");
        write_state(record, RunStatus::Paused, Some("0020"));
        let before = std::fs::read(md(&record.path)).unwrap();

        assert_eq!(tick(record), BackstopOutcome::Skipped(BackstopSkip::Active));
        assert_eq!(std::fs::read(md(&record.path)).unwrap(), before);
        assert!(!log(record).contains(PROMOTED_KIND));
    }

    /// DoD-2: an implementation that reads the state before the lock sees `Stopped` and
    /// promotes; this test would turn red.
    #[test]
    fn locked_reread_refuses_a_run_that_started_while_waiting_for_the_lock() {
        use std::time::Duration;
        let _lock = test_env_lock();
        let _env = TestEnvGuard::capture(&crate::features::test_feature_env_names());
        let fx = fixture();
        let record = &fx.record;
        write_registry(
            &record.path,
            &registry(&[row("0020-Fixture", PROPOSED, "fixture")]),
        );
        write_track(&record.path, "0020-Fixture", "# 0020 — Fixture Plan\n");
        write_state(record, RunStatus::Stopped, Some("0099"));
        let path = md(&record.path);
        let before = std::fs::read(&path).unwrap();
        let lock = crate::state::resolve_state_dir(record)
            .unwrap()
            .join(".run-state.lock");
        std::fs::create_dir(&lock).unwrap(); // simulate another holder in the critical section

        let rec2 = record.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let out = crate::workflow::placeholder_backstop::tick(&rec2);
            done_tx.send(out).unwrap();
        });
        // (a) it must WAIT while the lock is held
        assert!(done_rx.recv_timeout(Duration::from_millis(150)).is_err());
        // (b) a run starts while it waits; only a re-read INSIDE the lock sees it
        let mut running = load_run_state(record).unwrap();
        running.status = RunStatus::Running;
        save_run_state(record, &running).unwrap();
        std::fs::remove_dir(&lock).unwrap(); // release

        let outcome = done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        handle.join().unwrap();
        assert_eq!(
            outcome,
            BackstopOutcome::Skipped(BackstopSkip::Active),
            "a pre-lock read would have seen Stopped and promoted"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn skips_nostart_row() {
        let _lock = test_env_lock();
        let _env = TestEnvGuard::capture(&crate::features::test_feature_env_names());
        let fx = fixture();
        let record = &fx.record;
        write_registry(
            &record.path,
            &registry(&[row("0020-Fixture", PROPOSED, "fixture <!-- nostart -->")]),
        );
        write_track(&record.path, "0020-Fixture", "# 0020 — Fixture Plan\n");
        let before = std::fs::read(md(&record.path)).unwrap();

        assert_eq!(
            tick(record),
            BackstopOutcome::Skipped(BackstopSkip::NoCandidate)
        );
        assert_eq!(std::fs::read(md(&record.path)).unwrap(), before);
    }

    #[test]
    fn skips_hitl_row() {
        let _lock = test_env_lock();
        let _env = TestEnvGuard::capture(&crate::features::test_feature_env_names());
        let fx = fixture();
        let record = &fx.record;
        write_registry(
            &record.path,
            &registry(&[row("0020-Fixture", PROPOSED, "needs HITL sign off")]),
        );
        write_track(&record.path, "0020-Fixture", "# 0020 — Fixture Plan\n");
        let before = std::fs::read(md(&record.path)).unwrap();

        assert_eq!(
            tick(record),
            BackstopOutcome::Skipped(BackstopSkip::NoCandidate)
        );
        assert_eq!(std::fs::read(md(&record.path)).unwrap(), before);
    }

    #[test]
    fn skips_stub_plan() {
        let _lock = test_env_lock();
        let _env = TestEnvGuard::capture(&crate::features::test_feature_env_names());
        let fx = fixture();
        let record = &fx.record;
        write_registry(
            &record.path,
            &registry(&[row("0020-Fixture", PROPOSED, "fixture")]),
        );
        write_track(
            &record.path,
            "0020-Fixture",
            "<!-- coordinator:placeholder-plan -->\n",
        );

        assert_eq!(
            tick(record),
            BackstopOutcome::Skipped(BackstopSkip::NoCandidate)
        );
    }

    #[test]
    fn skips_missing_spec() {
        let _lock = test_env_lock();
        let _env = TestEnvGuard::capture(&crate::features::test_feature_env_names());
        let fx = fixture();
        let record = &fx.record;
        write_registry(
            &record.path,
            &registry(&[row("0020-Fixture", PROPOSED, "fixture")]),
        );
        let dir = record.path.join("conductor").join("0020-Fixture");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plan.md"), "# 0020 — Fixture Plan\n").unwrap();

        assert_eq!(
            tick(record),
            BackstopOutcome::Skipped(BackstopSkip::NoCandidate)
        );
    }

    #[test]
    fn promotes_to_the_default_ready_phrase_with_a_custom_alias() {
        let _lock = test_env_lock();
        let _env = TestEnvGuard::capture(&crate::features::test_feature_env_names());
        let mut fx = fixture();
        fx.record.ready_aliases = vec!["Pending".into()];
        let record = &fx.record;
        write_registry(
            &record.path,
            &registry(&[row("0020-Fixture", PROPOSED, "fixture")]),
        );
        write_track(&record.path, "0020-Fixture", "# 0020 — Fixture Plan\n");

        assert!(matches!(tick(record), BackstopOutcome::Promoted(_)));
        let after = std::fs::read_to_string(md(&record.path)).unwrap();
        assert!(after.contains(READY), "{after}");
        assert!(!after.contains("**Pending**"), "{after}");
    }

    #[test]
    fn promotes_two_rows_in_one_write() {
        let _lock = test_env_lock();
        let _env = TestEnvGuard::capture(&crate::features::test_feature_env_names());
        let fx = fixture();
        let record = &fx.record;
        write_registry(
            &record.path,
            &registry(&[
                row("0020-One", PROPOSED, "fixture"),
                row("0021-Two", "**Ready — not started**", "already ready"),
                row("0022-Three", PROPOSED, "a \\| b"),
            ]),
        );
        write_track(&record.path, "0020-One", "# 0020 — One\n");
        write_track(&record.path, "0022-Three", "# 0022 — Three\n");

        assert_eq!(
            tick(record),
            BackstopOutcome::Promoted(vec!["0020".to_string(), "0022".to_string()])
        );
        let after = std::fs::read_to_string(md(&record.path)).unwrap();
        assert!(
            after.contains("| [0020-One](0020-One/spec.md) | `.` | **Ready — not started** |"),
            "{after}"
        );
        assert!(
            after.contains(
                "| [0022-Three](0022-Three/spec.md) | `.` | **Ready — not started** | a \\| b |"
            ),
            "{after}"
        );
        assert!(after.ends_with('\n'), "{after}");
    }

    #[test]
    fn feature_off_is_silent() {
        let _lock = test_env_lock();
        let _env = TestEnvGuard::capture(&crate::features::test_feature_env_names());
        let fx = fixture();
        let record = &fx.record;
        write_config(fx._home.path(), serde_json::json!({ "auto_start": false }));
        unsafe {
            std::env::set_var("COORDINATOR_FEATURE_AUTO_START", "off");
        }
        write_registry(
            &record.path,
            &registry(&[row("0020-Fixture", PROPOSED, "fixture")]),
        );
        write_track(&record.path, "0020-Fixture", "# 0020 — Fixture Plan\n");
        let before = std::fs::read(md(&record.path)).unwrap();

        assert_eq!(
            tick(record),
            BackstopOutcome::Skipped(BackstopSkip::FeatureOff)
        );
        assert_eq!(std::fs::read(md(&record.path)).unwrap(), before);
    }

    #[test]
    fn env_on_overrides_config_off() {
        let _lock = test_env_lock();
        let _env = TestEnvGuard::capture(&crate::features::test_feature_env_names());
        let fx = fixture();
        let record = &fx.record;
        write_config(fx._home.path(), serde_json::json!({ "auto_start": false }));
        unsafe {
            std::env::set_var("COORDINATOR_FEATURE_AUTO_START", "1");
        }
        write_registry(
            &record.path,
            &registry(&[row("0020-Fixture", PROPOSED, "fixture")]),
        );
        write_track(&record.path, "0020-Fixture", "# 0020 — Fixture Plan\n");

        assert!(matches!(tick(record), BackstopOutcome::Promoted(_)));
    }

    #[test]
    fn promotes_under_hitl_and_never_policies_without_starting() {
        let _lock = test_env_lock();
        let _env = TestEnvGuard::capture(&crate::features::test_feature_env_names());
        for policy in [
            crate::registry::AutoStartPolicy::Hitl,
            crate::registry::AutoStartPolicy::Never,
        ] {
            let fx = fixture();
            let mut record = fx.record.clone();
            record.auto_start = policy;
            write_registry(
                &record.path,
                &registry(&[row("0020-Fixture", PROPOSED, "fixture")]),
            );
            write_track(&record.path, "0020-Fixture", "# 0020 — Fixture Plan\n");
            write_state(&record, RunStatus::Stopped, Some("0099"));

            assert!(
                matches!(tick(&record), BackstopOutcome::Promoted(_)),
                "{policy:?} must still be promoted (OD-3)"
            );
            assert_eq!(load_run_state(&record).unwrap().status, RunStatus::Stopped);
            assert!(
                !log(&record).contains("start  track="),
                "{policy:?} tick must not start: {}",
                log(&record)
            );
        }
    }

    #[test]
    fn warn_once_dedupes_per_key_and_resets() {
        let _lock = test_env_lock();
        let _env = TestEnvGuard::capture(&crate::features::test_feature_env_names());
        let home = tempdir().unwrap();
        isolate(home.path());
        reset_backstop_warnings_for_tests();
        assert!(warn_once("lock:a".into(), "coordinator: test lock-a"));
        assert!(!warn_once("lock:a".into(), "coordinator: test lock-a"));
        assert!(warn_once("write:a".into(), "coordinator: test write-a"));
        reset_backstop_warnings_for_tests();
        assert!(warn_once("lock:a".into(), "coordinator: test lock-a"));
    }

    #[test]
    fn missing_registry_is_noop() {
        let _lock = test_env_lock();
        let _env = TestEnvGuard::capture(&crate::features::test_feature_env_names());
        let fx = fixture();
        assert_eq!(
            tick(&fx.record),
            BackstopOutcome::Skipped(BackstopSkip::Unreadable)
        );
    }

    #[test]
    fn malformed_registry_is_noop() {
        let _lock = test_env_lock();
        let _env = TestEnvGuard::capture(&crate::features::test_feature_env_names());
        let fx = fixture();
        write_registry(&fx.record.path, "| Foo | Bar |\n| --- | --- |\n| a | b |\n");

        assert_eq!(
            tick(&fx.record),
            BackstopOutcome::Skipped(BackstopSkip::Unreadable)
        );
    }

    #[test]
    fn skips_crlf_registry() {
        let _lock = test_env_lock();
        let _env = TestEnvGuard::capture(&crate::features::test_feature_env_names());
        let fx = fixture();
        let record = &fx.record;
        let crlf = registry(&[row("0020-Fixture", PROPOSED, "fixture")]).replace('\n', "\r\n");
        write_registry(&record.path, &crlf);
        write_track(&record.path, "0020-Fixture", "# 0020 — Fixture Plan\n");
        let before = std::fs::read(md(&record.path)).unwrap();

        assert_eq!(tick(record), BackstopOutcome::Skipped(BackstopSkip::Crlf));
        assert_eq!(std::fs::read(md(&record.path)).unwrap(), before);
    }
}
