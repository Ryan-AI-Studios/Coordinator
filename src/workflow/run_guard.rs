//! 0120: refuse an explicit `run --track` whose registry row is terminal.
//!
//! This is the first check in `api::cmd_run` after `resolve_selected`, i.e. before
//! `settle_if_conductor_completed`, `blocked_hold`, driver resolution, preflight, or any
//! state/spawn write. It is read-only: `registry_store::snapshot` (never `load_track_rows`,
//! which appends drift/unreadable progress lines) and one receipt line in `status.md`.
//!
//! `--replay` is the single deliberate override, reachable only from the CLI `run` verb. It
//! authorizes the terminal override but does not release a 0098 Blocked hold, does not
//! override registry drift/invalid/ambiguity, and never rewrites the registry.

use crate::error::{CoordinatorError, Result};
use crate::registry::ProjectRecord;
use crate::registry_store::{StoreView, conductor_dir_for, store_path_for};
use crate::workflow::conductor_md::{self, TrackRow};
use std::path::Path;

/// Which surface asked. Only the CLI can carry `--replay`; HTTP/UI are the control plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunActor {
    Cli,
    Http,
}

impl RunActor {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Http => "http",
        }
    }
}

/// OD-120-7: the guard at the top of the run command.
///
/// Reads the registry read-only and returns:
/// - `Ok(())` for an omitted/empty track, an absent row, a readable but empty registry, and
///   every non-terminal row (explicit wins, unchanged);
/// - `TerminalRunTrack` for a terminal row (`--replay` authorizes it instead);
/// - `AmbiguousRunTrack` when more than one row matches and none is terminal;
/// - `Message` for registry integrity refusals (store invalid, view drift, unreadable or
///   unrecognized legacy view) and for `--replay` with an empty track.
pub(crate) fn check_explicit_run(
    record: &ProjectRecord,
    explicit: Option<&str>,
    replay: bool,
    actor: RunActor,
) -> Result<()> {
    let id = explicit.unwrap_or("").trim();
    // Single home for the check: `cmd_run_request` does not duplicate it.
    if replay && id.is_empty() {
        return Err(CoordinatorError::Message(
            "--replay requires a non-empty --track".into(),
        ));
    }
    if id.is_empty() {
        return Ok(());
    }

    let rows = explicit_run_rows(record)?;
    let matches: Vec<&TrackRow> = rows
        .iter()
        .filter(|row| conductor_md::row_matches_track(row, id))
        .collect();
    if matches.is_empty() {
        return Ok(());
    }

    if let Some((_row, token)) = matches
        .iter()
        .find_map(|row| conductor_md::terminal_token(&row.status_raw).map(|token| (*row, token)))
    {
        if replay {
            crate::progress_log::append(
                record,
                "run-replay",
                &format!(
                    "actor=cli track={id} row={token} override=--replay result=override-authorized"
                ),
            );
            return Ok(());
        }
        crate::progress_log::append(
            record,
            "run-refused",
            &format!(
                "actor={} track={id} row={token} reason=terminal",
                actor.as_str()
            ),
        );
        return Err(CoordinatorError::TerminalRunTrack {
            track: id.to_string(),
            status: token.to_string(),
        });
    }

    if matches.len() > 1 {
        let rows_desc = matches
            .iter()
            .map(|row| format!("{}={}", row.id, conductor_md::status_token(&row.status_raw)))
            .collect::<Vec<_>>()
            .join(", ");
        crate::progress_log::append(
            record,
            "run-refused",
            &format!(
                "actor={} track={id} reason=ambiguous rows={}",
                actor.as_str(),
                matches.len()
            ),
        );
        return Err(CoordinatorError::AmbiguousRunTrack {
            track: id.to_string(),
            rows: rows_desc,
        });
    }

    Ok(())
}

/// The registry rows an explicit `--track` must be checked against (spec §3.3).
///
/// Store-first and lock-free; a present store is authoritative. Integrity refusals are
/// stderr-only: this helper never calls `load_track_rows` / `report_drift_once`, so it adds
/// no `status.md` line of its own.
fn explicit_run_rows(record: &ProjectRecord) -> Result<Vec<TrackRow>> {
    let snapshot = match crate::registry_store::snapshot(record) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            return Err(CoordinatorError::Message(format!(
                "track registry store unreadable at {}: {err}; repair or delete the store \
                 before running an explicit --track",
                store_path_for(record).display()
            )));
        }
    };
    match snapshot.view {
        StoreView::Drifted { expected, actual } => Err(crate::registry_store::view_drift_error(
            &conductor_dir_for(record),
            &expected,
            &actual,
        )),
        StoreView::Ok => Ok(snapshot.rows),
        StoreView::Absent => legacy_view_rows(record),
    }
}

fn legacy_view_rows(record: &ProjectRecord) -> Result<Vec<TrackRow>> {
    let view = conductor_md::conductor_md_path(record);
    match std::fs::metadata(&view) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // No registry can contradict the id: explicit wins (R6).
            return Ok(Vec::new());
        }
        Err(err) => return Err(view_unreadable(&view, &err)),
        Ok(meta) if !meta.is_file() => {
            // A directory (or other non-regular file) named conductor.md is unreadable,
            // not "absent": do not fail open (codex P1, pass 4).
            return Err(view_unreadable(
                &view,
                &std::io::Error::other("not a regular file"),
            ));
        }
        Ok(_) => {}
    }
    let text =
        conductor_md::read_view_for_parse(&view).map_err(|err| view_unreadable(&view, &err))?;
    let (rows, registry_shaped) = conductor_md::parse_conductor_md_parts(&text);
    if !rows.is_empty() || registry_shaped || conductor_md::view_is_scaffolding_only(&text) {
        // Readable with no row for the id (including a zero-row scaffold) is the
        // absent-row case: explicit wins (AC-3.2, M-4).
        return Ok(rows);
    }
    Err(CoordinatorError::Message(format!(
        "track registry view has no Track/Status table at {}; repair conductor.md before \
         running an explicit --track",
        view.display()
    )))
}

fn view_unreadable(view: &Path, err: &std::io::Error) -> CoordinatorError {
    CoordinatorError::Message(format!(
        "track registry view unreadable at {}: {err}; repair conductor.md before \
         running an explicit --track",
        view.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ENV_COORDINATOR_STATE_DIR, TestEnvGuard, test_env_lock};
    use crate::registry_store::{ImportOpts, import_at, reset_drift_warnings_for_tests};
    use std::collections::BTreeMap;
    use std::path::Path;
    use tempfile::{TempDir, tempdir};

    const HEAD: &str = "| Track | Execution path | Status | Summary |\n\
                        | --- | --- | --- | --- |\n";

    fn rec(ws: &Path, id: &str) -> ProjectRecord {
        ProjectRecord {
            id: id.to_string(),
            path: ws.to_path_buf(),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: BTreeMap::new(),
            state_dir: None,
            auto_merge: true,
            phase_timeouts_secs: BTreeMap::new(),
            notify_progress: false,
            worktree_isolation: false,
            ready_aliases: Vec::new(),
            auto_start: crate::registry::AutoStartPolicy::default(),
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
        fn conductor(&self) -> std::path::PathBuf {
            self.record.path.join("conductor")
        }

        fn write_view(&self, body: &str) {
            std::fs::create_dir_all(self.conductor()).unwrap();
            std::fs::write(self.conductor().join("conductor.md"), body).unwrap();
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
        let mut body = String::from(HEAD);
        for r in rows {
            body.push_str(r);
        }
        body
    }

    /// Remove ambient `COORDINATOR_STATE_DIR` so receipts land under the fixture.
    fn isolated() -> (std::sync::MutexGuard<'static, ()>, TestEnvGuard) {
        let lock = test_env_lock();
        let env = TestEnvGuard::capture(&[ENV_COORDINATOR_STATE_DIR]);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_STATE_DIR);
        }
        (lock, env)
    }

    fn assert_terminal(err: CoordinatorError, id: &str, token: &str) {
        match err {
            CoordinatorError::TerminalRunTrack { track, status } => {
                assert_eq!(track, id);
                assert_eq!(status, token, "canonical terminal token");
            }
            other => panic!("expected TerminalRunTrack, got {other:?}"),
        }
    }

    #[test]
    fn terminal_rows_refuse_and_write_one_line() {
        let (_lock, _env) = isolated();
        let fx = fixture("p-1200");
        fx.write_view(&registry(&[row(
            "0099",
            "**Completed** 2026-10-23 - PR **#135** squash `018a8ef`",
            "shipped",
        )]));
        let err = check_explicit_run(&fx.record, Some("0099"), false, RunActor::Cli).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert_eq!(err.http_status(), 409);
        assert_terminal(err, "0099", "Completed");
        let log = fx.status_log();
        assert_eq!(log.matches("run-refused").count(), 1, "{log}");
        assert!(
            log.contains("actor=cli track=0099 row=Completed reason=terminal"),
            "{log}"
        );
        assert!(crate::notify::artifact::existing_path(&fx.record).is_none());
    }

    #[test]
    fn absorbed_and_cancelled_refuse() {
        let (_lock, _env) = isolated();
        for (status, token) in [
            ("**Absorbed** into 0107", "Absorbed"),
            ("**Cancelled** - replaced", "Cancelled"),
        ] {
            let fx = fixture("p-1201");
            fx.write_view(&registry(&[row("0099", status, "x")]));
            let err =
                check_explicit_run(&fx.record, Some("0099"), false, RunActor::Http).unwrap_err();
            assert_terminal(err, "0099", token);
            assert!(
                fx.status_log().contains("actor=http"),
                "{}",
                fx.status_log()
            );
        }
    }

    #[test]
    fn non_terminal_rows_still_pass() {
        let (_lock, _env) = isolated();
        for status in [
            "**Ready - not started**",
            "**Proposed - placeholder**",
            "**Blocked** - prerequisite",
            "**In progress**",
        ] {
            let fx = fixture("p-1202");
            fx.write_view(&registry(&[row("0099", status, "x")]));
            check_explicit_run(&fx.record, Some("0099"), false, RunActor::Cli)
                .unwrap_or_else(|e| panic!("{status} must pass, got {e}"));
            assert!(fx.status_log().is_empty(), "{status}: {}", fx.status_log());
        }
        // Control: the same row with a terminal status refuses.
        let fx = fixture("p-1203");
        fx.write_view(&registry(&[row("0099", "**Completed** - done", "x")]));
        assert_terminal(
            check_explicit_run(&fx.record, Some("0099"), false, RunActor::Cli).unwrap_err(),
            "0099",
            "Completed",
        );
    }

    #[test]
    fn absent_row_and_absent_view_pass() {
        let (_lock, _env) = isolated();
        let fx = fixture("p-1204");
        fx.write_view(&registry(&[row("0001", "**Completed**", "other")]));
        check_explicit_run(&fx.record, Some("0099"), false, RunActor::Cli).unwrap();

        let bare = fixture("p-1205");
        std::fs::remove_dir_all(bare.conductor()).unwrap();
        check_explicit_run(&bare.record, Some("0099"), false, RunActor::Cli).unwrap();
        assert!(bare.status_log().is_empty());
    }

    #[test]
    fn readable_zero_row_registry_passes() {
        let (_lock, _env) = isolated();
        let scaffold = fixture("p-1206");
        scaffold.write_view("# tracks\n");
        check_explicit_run(&scaffold.record, Some("0120"), false, RunActor::Cli).unwrap();

        let header_only = fixture("p-1207");
        header_only.write_view(HEAD);
        check_explicit_run(&header_only.record, Some("0120"), false, RunActor::Cli).unwrap();
        assert!(header_only.status_log().is_empty());
    }

    #[test]
    fn prose_only_view_refuses() {
        let (_lock, _env) = isolated();
        let fx = fixture("p-1208");
        fx.write_view("# Tracks\n\nThis registry does not use a table yet.\n");
        let err = check_explicit_run(&fx.record, Some("0120"), false, RunActor::Cli).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no Track/Status table"), "{msg}");
        assert!(
            msg.contains(&fx.conductor().join("conductor.md").display().to_string()),
            "{msg}"
        );
        assert_eq!(err.exit_code(), 1);
        assert_eq!(err.http_status(), 400);

        // Prose hidden in a fenced block is still prose (codex P2, pass 2): fail closed,
        // never fail open as a zero-row scaffold.
        let fenced = fixture("p-1208f");
        fenced.write_view("# tracks\n```\nnot a registry\n```\n");
        let err =
            check_explicit_run(&fenced.record, Some("0120"), false, RunActor::Cli).unwrap_err();
        assert!(err.to_string().contains("no Track/Status table"), "{err}");
        assert!(fenced.status_log().is_empty(), "{}", fenced.status_log());

        // A fenced fake registry table is code, not rows (codex P2, pass 3): no fake
        // terminal row can refuse a run, and no fake Ready row can authorize one.
        let fenced_table = fixture("p-1208t");
        fenced_table.write_view(
            "# tracks\n```\n| Track | Status |\n| --- | --- |\n| 0120 | **Completed** |\n```\n",
        );
        let err = check_explicit_run(&fenced_table.record, Some("0120"), false, RunActor::Cli)
            .unwrap_err();
        assert!(err.to_string().contains("no Track/Status table"), "{err}");
        assert!(
            !fenced_table.status_log().contains("run-refused"),
            "{}",
            fenced_table.status_log()
        );

        // Indented code is code too: a 4-space fake registry table cannot refuse a run
        // (codex P2, pass 4).
        let indented = fixture("p-1208i");
        indented.write_view(
            "    | Track | Status |\n    | --- | --- |\n    | 0120 | **Completed** |\n",
        );
        let err =
            check_explicit_run(&indented.record, Some("0120"), false, RunActor::Cli).unwrap_err();
        assert!(err.to_string().contains("no Track/Status table"), "{err}");
        assert!(
            !indented.status_log().contains("run-refused"),
            "{}",
            indented.status_log()
        );
    }

    #[test]
    fn legacy_view_is_directory_refuses_named_path() {
        let (_lock, _env) = isolated();
        let fx = fixture("p-1208d");
        // A directory named conductor.md is unreadable, not absent: never fail open
        // (codex P1, pass 4).
        std::fs::create_dir_all(fx.conductor().join("conductor.md")).unwrap();
        let err = check_explicit_run(&fx.record, Some("0120"), false, RunActor::Cli).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("track registry view unreadable at"), "{msg}");
        assert!(
            msg.contains(&fx.conductor().join("conductor.md").display().to_string()),
            "{msg}"
        );
        assert!(msg.contains("repair conductor.md"), "{msg}");
        assert_eq!(err.exit_code(), 1);
        assert_eq!(err.http_status(), 400);
        assert!(fx.status_log().is_empty(), "{}", fx.status_log());
    }

    #[test]
    fn legacy_view_unreadable_refuses_named_path() {
        let (_lock, _env) = isolated();
        let fx = fixture("p-1208u");
        // Invalid UTF-8 makes `read_to_string` fail while `is_file()` stays true: the
        // legacy-view I/O arm (not the missing / unrecognized arms).
        std::fs::write(fx.conductor().join("conductor.md"), [0xFF, 0xFE, 0x00]).unwrap();
        let err = check_explicit_run(&fx.record, Some("0120"), false, RunActor::Cli).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("track registry view unreadable at"), "{msg}");
        assert!(
            msg.contains(&fx.conductor().join("conductor.md").display().to_string()),
            "{msg}"
        );
        assert!(msg.contains("repair conductor.md"), "{msg}");
        assert_eq!(err.exit_code(), 1);
        assert_eq!(err.http_status(), 400);
        assert!(fx.status_log().is_empty(), "{}", fx.status_log());
    }

    #[test]
    fn drifted_view_refuses_with_0109_text() {
        let (_lock, _env) = isolated();
        let fx = fixture("p-1209");
        fx.write_view(&registry(&[row("0099", "**Ready - not started**", "x")]));
        import_at(
            &fx.conductor(),
            ImportOpts {
                dry_run: false,
                force: false,
            },
        )
        .unwrap();
        reset_drift_warnings_for_tests();
        let edited = std::fs::read_to_string(fx.conductor().join("conductor.md"))
            .unwrap()
            .replace("| x |", "| x hand edit |");
        fx.write_view(&edited);
        let err = check_explicit_run(&fx.record, Some("0099"), false, RunActor::Cli).unwrap_err();
        let msg = err.to_string();
        assert!(msg.starts_with("track registry view drifted"), "{msg}");
        assert!(msg.contains("(store "), "{msg}");
        assert!(msg.contains(", view "), "{msg}");
        assert!(msg.contains("registry import --conductor-dir"), "{msg}");
        assert!(msg.contains("registry render --conductor-dir"), "{msg}");
        // M4 detector: the guard must not call load_track_rows/report_drift_once.
        assert!(
            !fx.status_log().contains("registry-view-drift"),
            "{}",
            fx.status_log()
        );
    }

    #[test]
    fn invalid_store_refuses_and_names_path() {
        let (_lock, _env) = isolated();
        let fx = fixture("p-1210");
        std::fs::write(fx.conductor().join("track-registry.json"), b"").unwrap();
        let err = check_explicit_run(&fx.record, Some("0099"), false, RunActor::Cli).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("track registry store unreadable at"), "{msg}");
        assert!(msg.contains("track-registry.json"), "{msg}");
        assert!(msg.contains("repair or delete"), "{msg}");
        assert_eq!(err.http_status(), 400);
        assert!(
            !fx.status_log().contains("registry-store-unreadable"),
            "{}",
            fx.status_log()
        );
    }

    #[test]
    fn duplicate_rows_without_terminal_refuse_ambiguous() {
        let (_lock, _env) = isolated();
        let fx = fixture("p-1211");
        fx.write_view(&registry(&[
            row("0099", "**Ready - not started**", "one"),
            row("0099", "**Proposed** - duplicate", "two"),
        ]));
        let err = check_explicit_run(&fx.record, Some("0099"), false, RunActor::Cli).unwrap_err();
        match err {
            CoordinatorError::AmbiguousRunTrack { track, rows } => {
                assert_eq!(track, "0099");
                assert!(rows.contains("0099=Ready - not started"), "{rows}");
                assert!(rows.contains("0099=Proposed"), "{rows}");
            }
            other => panic!("expected AmbiguousRunTrack, got {other:?}"),
        }
        let log = fx.status_log();
        assert_eq!(log.matches("run-refused").count(), 1, "{log}");
        assert!(log.contains("reason=ambiguous rows=2"), "{log}");
    }

    #[test]
    fn terminal_row_in_second_table_refuses() {
        let (_lock, _env) = isolated();
        let fx = fixture("p-1212");
        let mut body = registry(&[row("0099", "**Ready - not started**", "one")]);
        body.push_str("\n| Track | Status |\n| --- | --- |\n");
        body.push_str("| [0099](0099/spec.md) | **Completed** - shipped |\n");
        fx.write_view(&body);
        assert_terminal(
            check_explicit_run(&fx.record, Some("0099"), false, RunActor::Cli).unwrap_err(),
            "0099",
            "Completed",
        );
    }

    #[test]
    fn store_mode_does_not_parse_view() {
        let (_lock, _env) = isolated();
        let fx = fixture("p-1213");
        fx.write_view(&registry(&[row("0099", "**Completed**", "x")]));
        import_at(
            &fx.conductor(),
            ImportOpts {
                dry_run: false,
                force: false,
            },
        )
        .unwrap();
        let snapshot = crate::registry_store::snapshot(&fx.record).unwrap();
        assert_eq!(snapshot.view, StoreView::Ok, "fixture must be store-ok");
        conductor_md::reset_view_parse_reads();
        assert_terminal(
            check_explicit_run(&fx.record, Some("0099"), false, RunActor::Cli).unwrap_err(),
            "0099",
            "Completed",
        );
        assert_eq!(
            conductor_md::view_parse_reads(),
            0,
            "store mode must not parse the generated view"
        );
    }

    #[test]
    fn replay_writes_one_receipt() {
        let (_lock, _env) = isolated();
        let fx = fixture("p-1214");
        fx.write_view(&registry(&[row(
            "0099",
            "**Completed** 2026-10-23 - PR **#135** squash `018a8ef`",
            "x",
        )]));
        check_explicit_run(&fx.record, Some("0099"), true, RunActor::Cli).unwrap();
        let log = fx.status_log();
        assert_eq!(log.matches("run-replay").count(), 1, "{log}");
        assert!(
            log.contains(
                "actor=cli track=0099 row=Completed override=--replay result=override-authorized"
            ),
            "{log}"
        );
        assert!(!log.contains("run-refused"), "{log}");
    }

    #[test]
    fn replay_on_non_terminal_writes_no_receipt() {
        let (_lock, _env) = isolated();
        let fx = fixture("p-1215");
        fx.write_view(&registry(&[row("0099", "**Ready - not started**", "x")]));
        check_explicit_run(&fx.record, Some("0099"), true, RunActor::Cli).unwrap();
        assert!(fx.status_log().is_empty(), "{}", fx.status_log());

        let absent = fixture("p-1216");
        absent.write_view(&registry(&[row("0001", "**Completed**", "other")]));
        check_explicit_run(&absent.record, Some("0099"), true, RunActor::Cli).unwrap();
        assert!(absent.status_log().is_empty(), "{}", absent.status_log());
    }

    #[test]
    fn replay_empty_track_refused() {
        let (_lock, _env) = isolated();
        let fx = fixture("p-1217");
        for empty in [Some(""), Some("   "), None] {
            let err = check_explicit_run(&fx.record, empty, true, RunActor::Cli).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("--replay requires"), "{msg}");
            assert_eq!(err.exit_code(), 1);
            assert_eq!(err.http_status(), 400);
        }
        assert!(fx.status_log().is_empty());
    }

    #[test]
    fn replay_does_not_override_invalid_store() {
        let (_lock, _env) = isolated();
        let fx = fixture("p-1218");
        std::fs::write(fx.conductor().join("track-registry.json"), b"{not json").unwrap();
        let err = check_explicit_run(&fx.record, Some("0099"), true, RunActor::Cli).unwrap_err();
        assert!(err.to_string().contains("repair or delete"), "{err}");
        assert!(
            !fx.status_log().contains("run-replay"),
            "{}",
            fx.status_log()
        );
    }

    #[test]
    fn replay_does_not_override_ambiguous() {
        let (_lock, _env) = isolated();
        let fx = fixture("p-1219");
        fx.write_view(&registry(&[
            row("0099", "**Ready - not started**", "one"),
            row("0099", "**Proposed**", "two"),
        ]));
        let err = check_explicit_run(&fx.record, Some("0099"), true, RunActor::Cli).unwrap_err();
        assert!(matches!(err, CoordinatorError::AmbiguousRunTrack { .. }));
        assert!(
            !fx.status_log().contains("run-replay"),
            "{}",
            fx.status_log()
        );
    }

    #[test]
    fn full_slug_and_whitespace_match_terminal() {
        let (_lock, _env) = isolated();
        let fx = fixture("p-1220");
        fx.write_view(&registry(&[row(
            "0099-AutonomousPipelineProbe",
            "**Completed**",
            "x",
        )]));
        assert_terminal(
            check_explicit_run(
                &fx.record,
                Some("0099-AutonomousPipelineProbe"),
                false,
                RunActor::Cli,
            )
            .unwrap_err(),
            "0099-AutonomousPipelineProbe",
            "Completed",
        );
        assert_terminal(
            check_explicit_run(&fx.record, Some(" 0099 "), false, RunActor::Cli).unwrap_err(),
            "0099",
            "Completed",
        );
    }

    #[test]
    fn refusal_writes_exactly_one_line() {
        let (_lock, _env) = isolated();
        let fx = fixture("p-1221");
        fx.write_view(&registry(&[row("0099", "**Completed**", "x")]));
        let _ = check_explicit_run(&fx.record, Some("0099"), false, RunActor::Cli);
        let log = fx.status_log();
        assert_eq!(log.matches(" run-refused ").count(), 1, "{log}");
        assert_eq!(
            log.lines().filter(|line| line.starts_with("- ")).count(),
            1,
            "no second appender: {log}"
        );
    }
}
