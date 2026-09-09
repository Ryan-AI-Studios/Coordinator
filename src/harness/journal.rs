//! TerminalHub child-command JSONL journal (track **0034**).
//!
//! Best-effort: IO errors never fail `terminal/create`. No env, stdout, or
//! stderr. Host probe is not journaled (caller skips `HOST_PROBE_LINE`).

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{SecondsFormat, Utc};
use serde::Serialize;

use crate::config::load_machine_config;
use crate::registry::ProjectRecord;
use crate::state::resolve_state_dir;

pub const ENV_COORDINATOR_JOURNAL: &str = "COORDINATOR_JOURNAL";

const ARGV_HEAD_CAP: usize = 120;
const DEFAULT_KEEP: u64 = 20;

#[derive(Serialize)]
struct JournalLine<'a> {
    ts: String,
    phase: &'a str,
    harness: &'a str,
    argv_head: &'a str,
    exit: Option<i64>,
    dur_ms: u64,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    signal: Option<&'a str>,
}

#[derive(Clone)]
pub(crate) struct JournalSnap {
    pub record: ProjectRecord,
    pub phase: String,
    pub track: String,
    pub epoch: u64,
}

pub(crate) struct JournalEvent<'a> {
    pub snap: &'a JournalSnap,
    pub argv_head: &'a str,
    pub exit: Option<i64>,
    pub dur_ms: u64,
    pub ok: bool,
    pub signal: Option<&'static str>,
    pub spawn_fail: bool,
}

pub(crate) struct JournalHub {
    fail_counts: HashMap<(String, String, String), u32>,
}

impl JournalHub {
    pub(crate) fn new() -> Self {
        Self {
            fail_counts: HashMap::new(),
        }
    }

    pub(crate) fn reset(&mut self) {
        self.fail_counts.clear();
    }

    pub(crate) fn record(&mut self, ev: JournalEvent<'_>) {
        if !enabled() {
            return;
        }
        let line = JournalLine {
            ts: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            phase: &ev.snap.phase,
            harness: "grok",
            argv_head: ev.argv_head,
            exit: ev.exit,
            dur_ms: ev.dur_ms,
            ok: ev.ok,
            signal: ev.signal,
        };
        let Ok(json) = serde_json::to_string(&line) else {
            return;
        };
        let Some(path) = journal_file(&ev.snap.record, &ev.snap.track, ev.snap.epoch) else {
            return;
        };
        if append_line(&path, &json).is_err() {
            return;
        }
        gc_after_append(&path);
        if !ev.ok && ev.signal != Some("killed") {
            let exit_debug = if ev.spawn_fail {
                "spawn".to_string()
            } else {
                ev.exit
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "none".into())
            };
            let key = (
                ev.snap.phase.clone(),
                ev.argv_head.to_string(),
                exit_debug.clone(),
            );
            let n = self.fail_counts.entry(key).or_insert(0);
            *n += 1;
            if *n == 2 {
                crate::progress_log::append(
                    &ev.snap.record,
                    "loop_suspect",
                    &format!("{} exit={exit_debug}", ev.argv_head),
                );
            }
        }
    }
}

pub(crate) fn enabled() -> bool {
    !matches!(
        std::env::var(ENV_COORDINATOR_JOURNAL),
        Ok(s) if s.eq_ignore_ascii_case("off")
    )
}

/// One-shot reviewer stall line (0036). Caller-supplied `harness` slug.
/// Does **not** call [`JournalHub::record`] (that hardcodes `grok`) and does
/// not increment `loop_suspect`.
pub(crate) fn record_reviewer_stall(
    record: &ProjectRecord,
    harness: &str,
    argv_head: &str,
    dur_ms: u64,
) {
    if !enabled() {
        return;
    }
    let snap = snapshot(record);
    let line = JournalLine {
        ts: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        phase: &snap.phase,
        harness,
        argv_head,
        exit: Some(124),
        dur_ms,
        ok: false,
        signal: Some("reviewer_stall"),
    };
    let Ok(json) = serde_json::to_string(&line) else {
        return;
    };
    let Some(path) = journal_file(&snap.record, &snap.track, snap.epoch) else {
        return;
    };
    if append_line(&path, &json).is_err() {
        return;
    }
    gc_after_append(&path);
}

pub(crate) fn snapshot(record: &ProjectRecord) -> JournalSnap {
    let missing = crate::state::run_state_path(record)
        .map(|p| !p.exists())
        .unwrap_or(true);
    if missing {
        return JournalSnap {
            record: record.clone(),
            phase: "-".into(),
            track: "-".into(),
            epoch: 0,
        };
    }
    match crate::state::load_run_state(record) {
        Ok(s) => JournalSnap {
            record: record.clone(),
            phase: if s.phase.is_empty() {
                "-".into()
            } else {
                s.phase
            },
            track: sanitize_track(s.track_id.as_deref().unwrap_or("-")),
            epoch: s.run_epoch,
        },
        Err(_) => JournalSnap {
            record: record.clone(),
            phase: "-".into(),
            track: "-".into(),
            epoch: 0,
        },
    }
}

pub(crate) fn argv_head(command: &str, args: &[impl AsRef<str>]) -> String {
    let mut s = command.to_string();
    for a in args {
        s.push(' ');
        s.push_str(a.as_ref());
    }
    super::preflight::redact_secrets(&s)
        .chars()
        .take(ARGV_HEAD_CAP)
        .collect()
}

pub(crate) fn sanitize_track(track: &str) -> String {
    let t = if track.is_empty() { "-" } else { track };
    t.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect()
}

fn journal_file(record: &ProjectRecord, track: &str, epoch: u64) -> Option<PathBuf> {
    let dir = resolve_state_dir(record).ok()?;
    Some(dir.join("journal").join(format!("{track}-{epoch}.jsonl")))
}

fn append_line(path: &Path, json: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{json}")
}

/// Keep N newest `*.jsonl` (mtime desc, then filename desc). `None` skips GC.
fn retention_keep() -> Option<u64> {
    match load_machine_config().ok().and_then(|c| c.journal_keep) {
        None => Some(DEFAULT_KEEP),
        Some(0) => None,
        Some(n) => Some(n),
    }
}

fn gc_after_append(just_written: &Path) {
    let Some(keep) = retention_keep() else {
        return;
    };
    let Some(dir) = just_written.parent() else {
        return;
    };
    let _ = gc_journal_dir(dir, keep, just_written);
}

fn gc_journal_dir(dir: &Path, keep: u64, just_written: &Path) -> std::io::Result<()> {
    if keep == 0 {
        return Ok(());
    }
    let mut files: Vec<(SystemTime, String, PathBuf)> = Vec::new();
    for ent in std::fs::read_dir(dir)? {
        let ent = ent?;
        let path = ent.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let mtime = ent.metadata()?.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        files.push((mtime, name, path));
    }
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    for (i, (_, _, p)) in files.iter().enumerate() {
        if (i as u64) < keep {
            continue;
        }
        if p == just_written {
            continue;
        }
        let _ = std::fs::remove_file(p);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ENV_COORDINATOR_HOME, ENV_COORDINATOR_STATE_DIR, MACHINE_CONFIG_VERSION, MachineConfig,
        default_role_bindings, save_machine_config, test_env_lock,
    };
    use crate::layout::LayoutProfile;
    use crate::state::{RunState, save_run_state};
    use chrono::Utc;
    use serde_json::Value;
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;
    use tempfile::TempDir;

    struct Isolated {
        prev_home: Option<OsString>,
        prev_state: Option<OsString>,
        prev_journal: Option<OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
        _home: TempDir,
        ws: TempDir,
    }

    impl Isolated {
        fn enter() -> Self {
            let lock = test_env_lock();
            let home = tempfile::tempdir().unwrap();
            let ws = tempfile::tempdir().unwrap();
            let prev_home = std::env::var_os(ENV_COORDINATOR_HOME);
            let prev_state = std::env::var_os(ENV_COORDINATOR_STATE_DIR);
            let prev_journal = std::env::var_os(ENV_COORDINATOR_JOURNAL);
            unsafe {
                std::env::set_var(ENV_COORDINATOR_HOME, home.path());
                std::env::remove_var(ENV_COORDINATOR_STATE_DIR);
                std::env::remove_var(ENV_COORDINATOR_JOURNAL);
            }
            Self {
                prev_home,
                prev_state,
                prev_journal,
                _lock: lock,
                _home: home,
                ws,
            }
        }

        fn rec(&self) -> ProjectRecord {
            ProjectRecord {
                id: "j34".into(),
                path: self.ws.path().to_path_buf(),
                display_name: None,
                layout_profile: LayoutProfile::Nested,
                conductor_dir: None,
                execution_repo: None,
                execution_repos: BTreeMap::new(),
                state_dir: Some(self.ws.path().join("state")),
                auto_merge: true,
                phase_timeouts_secs: BTreeMap::new(),
                notify_progress: false,
                created_at: Utc::now(),
            }
        }
    }

    impl Drop for Isolated {
        fn drop(&mut self) {
            unsafe {
                match &self.prev_home {
                    Some(v) => std::env::set_var(ENV_COORDINATOR_HOME, v),
                    None => std::env::remove_var(ENV_COORDINATOR_HOME),
                }
                match &self.prev_state {
                    Some(v) => std::env::set_var(ENV_COORDINATOR_STATE_DIR, v),
                    None => std::env::remove_var(ENV_COORDINATOR_STATE_DIR),
                }
                match &self.prev_journal {
                    Some(v) => std::env::set_var(ENV_COORDINATOR_JOURNAL, v),
                    None => std::env::remove_var(ENV_COORDINATOR_JOURNAL),
                }
            }
        }
    }

    fn ev<'a>(
        snap: &'a JournalSnap,
        argv: &'a str,
        exit: Option<i64>,
        dur_ms: u64,
        ok: bool,
        signal: Option<&'static str>,
        spawn_fail: bool,
    ) -> JournalEvent<'a> {
        JournalEvent {
            snap,
            argv_head: argv,
            exit,
            dur_ms,
            ok,
            signal,
            spawn_fail,
        }
    }

    fn seed_run(rec: &ProjectRecord, phase: &str, track: &str, epoch: u64) {
        let mut s = RunState::idle(&rec.id);
        s.phase = phase.into();
        s.track_id = Some(track.into());
        s.run_epoch = epoch;
        save_run_state(rec, &s).unwrap();
    }

    fn read_lines(path: &Path) -> Vec<Value> {
        let text = fs::read_to_string(path).unwrap();
        text.lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str::<Value>(l).expect(l))
            .collect()
    }

    #[test]
    fn argv_head_redacts_then_char_truncates() {
        let long = "é".repeat(200);
        let head = argv_head(&long, &[] as &[String]);
        assert_eq!(head.chars().count(), 120);
        assert!(!head.contains('\u{FFFD}'));

        let prefix = "a".repeat(115);
        let s = format!("{prefix}sk-SECRETVALUE");
        let head = argv_head(&s, &[] as &[String]);
        // Redact first (`sk-***`), then char-truncate. Truncate-first would leak `sk-SE`.
        assert!(head.contains("sk-"), "head={head}");
        assert!(!head.contains("SECRET"), "head={head}");
        assert!(!head.contains("sk-SE"), "head={head}");
        assert!(head.chars().count() <= 120);
    }

    #[test]
    fn argv_head_joins_command_and_args() {
        let head = argv_head("cmd.exe", &["/C".to_string(), "echo hi".into()]);
        assert_eq!(head, "cmd.exe /C echo hi");
    }

    #[test]
    fn sanitize_track_replaces_forbidden() {
        assert_eq!(sanitize_track(r#"a/b:c*d?e"f<g>h|i"#), "a_b_c_d_e_f_g_h_i");
        assert_eq!(sanitize_track(""), "-");
        assert_eq!(sanitize_track("0034"), "0034");
    }

    #[test]
    fn missing_run_state_snapshots_dash_not_stub_idle() {
        let iso = Isolated::enter();
        let rec = iso.rec();
        let snap = snapshot(&rec);
        assert_eq!(snap.phase, "-");
        assert_eq!(snap.track, "-");
        assert_eq!(snap.epoch, 0);
        let mut hub = JournalHub::new();
        hub.record(ev(&snap, "echo", Some(0), 1, true, None, false));
        let path = journal_file(&rec, "-", 0).unwrap();
        let v = &read_lines(&path)[0];
        assert_eq!(v["phase"], "-");
    }

    #[test]
    fn bound_record_writes_parseable_line_without_env() {
        let iso = Isolated::enter();
        let rec = iso.rec();
        seed_run(&rec, "implement", "0034", 7);
        let snap = snapshot(&rec);
        let mut hub = JournalHub::new();
        hub.record(ev(
            &snap,
            "cmd.exe /C echo hi",
            Some(0),
            12,
            true,
            None,
            false,
        ));
        let path = journal_file(&rec, "0034", 7).unwrap();
        let lines = read_lines(&path);
        assert_eq!(lines.len(), 1);
        let v = &lines[0];
        assert!(v.get("env").is_none());
        assert_eq!(v["harness"], "grok");
        assert_eq!(v["phase"], "implement");
        assert_eq!(v["argv_head"], "cmd.exe /C echo hi");
        assert_eq!(v["exit"], 0);
        assert_eq!(v["dur_ms"], 12);
        assert_eq!(v["ok"], true);
        assert!(v.get("signal").is_none());
        assert!(v["ts"].as_str().unwrap().contains('T'));
    }

    #[test]
    fn reviewer_stall_line_uses_caller_harness() {
        let iso = Isolated::enter();
        let rec = iso.rec();
        seed_run(&rec, "cross-model-review", "0036", 3);
        record_reviewer_stall(&rec, "codex", "codex exec", 600_000);
        let path = journal_file(&rec, "0036", 3).unwrap();
        let lines = read_lines(&path);
        assert_eq!(lines.len(), 1);
        let v = &lines[0];
        assert_eq!(v["harness"], "codex");
        assert_eq!(v["signal"], "reviewer_stall");
        assert_eq!(v["ok"], false);
        assert_eq!(v["exit"], 124);
        assert_eq!(v["phase"], "cross-model-review");
        assert_eq!(v["argv_head"], "codex exec");
        assert!(v.get("env").is_none());
    }

    #[test]
    fn reviewer_stall_journal_off_writes_nothing() {
        let iso = Isolated::enter();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_JOURNAL, "off");
        }
        let rec = iso.rec();
        seed_run(&rec, "cross-model-review", "0036", 3);
        record_reviewer_stall(&rec, "codex", "codex exec", 1);
        let path = journal_file(&rec, "0036", 3).unwrap();
        assert!(!path.exists() || fs::read_to_string(&path).unwrap().trim().is_empty());
    }

    #[test]
    fn journal_off_skips_write_and_loop_suspect() {
        let iso = Isolated::enter();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_JOURNAL, "off");
        }
        let rec = iso.rec();
        seed_run(&rec, "implement", "0034", 1);
        let snap = snapshot(&rec);
        let mut hub = JournalHub::new();
        hub.record(ev(
            &snap,
            "cmd.exe /C exit 1",
            Some(1),
            1,
            false,
            None,
            false,
        ));
        hub.record(ev(
            &snap,
            "cmd.exe /C exit 1",
            Some(1),
            1,
            false,
            None,
            false,
        ));
        let path = journal_file(&rec, "0034", 1).unwrap();
        assert!(!path.exists());
        assert!(!crate::progress_log::path(&rec).exists());
    }

    #[test]
    fn two_identical_fails_emit_one_loop_suspect_third_silent() {
        let iso = Isolated::enter();
        let rec = iso.rec();
        seed_run(&rec, "implement", "0034", 1);
        let snap = snapshot(&rec);
        let mut hub = JournalHub::new();
        let argv = "cmd.exe /C exit 1";
        hub.record(ev(&snap, argv, Some(1), 1, false, None, false));
        hub.record(ev(&snap, argv, Some(1), 2, false, None, false));
        hub.record(ev(&snap, argv, Some(1), 3, false, None, false));
        let path = journal_file(&rec, "0034", 1).unwrap();
        assert_eq!(read_lines(&path).len(), 3);
        let status = fs::read_to_string(crate::progress_log::path(&rec)).unwrap();
        let n = status.matches("loop_suspect").count();
        assert_eq!(n, 1, "status={status}");
    }

    #[test]
    fn success_repeats_do_not_loop_suspect() {
        let iso = Isolated::enter();
        let rec = iso.rec();
        seed_run(&rec, "implement", "0034", 1);
        let snap = snapshot(&rec);
        let mut hub = JournalHub::new();
        hub.record(ev(
            &snap,
            "cmd.exe /C echo hi",
            Some(0),
            1,
            true,
            None,
            false,
        ));
        hub.record(ev(
            &snap,
            "cmd.exe /C echo hi",
            Some(0),
            1,
            true,
            None,
            false,
        ));
        assert!(
            !crate::progress_log::path(&rec).exists() || {
                let t = fs::read_to_string(crate::progress_log::path(&rec)).unwrap();
                !t.contains("loop_suspect")
            }
        );
    }

    #[test]
    fn killed_signal_never_increments_loop_suspect() {
        let iso = Isolated::enter();
        let rec = iso.rec();
        seed_run(&rec, "implement", "0034", 1);
        let snap = snapshot(&rec);
        let mut hub = JournalHub::new();
        hub.record(ev(
            &snap,
            "cmd.exe /C ping",
            Some(1),
            5,
            false,
            Some("killed"),
            false,
        ));
        hub.record(ev(
            &snap,
            "cmd.exe /C ping",
            Some(1),
            5,
            false,
            Some("killed"),
            false,
        ));
        let path = journal_file(&rec, "0034", 1).unwrap();
        let lines = read_lines(&path);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["signal"], "killed");
        assert_eq!(lines[0]["ok"], false);
        let status_path = crate::progress_log::path(&rec);
        assert!(
            !status_path.exists()
                || !fs::read_to_string(&status_path)
                    .unwrap()
                    .contains("loop_suspect")
        );
    }

    #[test]
    fn reset_clears_fail_map() {
        let iso = Isolated::enter();
        let rec = iso.rec();
        seed_run(&rec, "implement", "0034", 1);
        let snap = snapshot(&rec);
        let mut hub = JournalHub::new();
        hub.record(ev(&snap, "x", Some(1), 1, false, None, false));
        hub.reset();
        hub.record(ev(&snap, "x", Some(1), 1, false, None, false));
        let status_path = crate::progress_log::path(&rec);
        assert!(
            !status_path.exists()
                || !fs::read_to_string(&status_path)
                    .unwrap()
                    .contains("loop_suspect")
        );
    }

    #[test]
    fn gc_keep_2_deletes_oldest_same_mtime_filename_desc() {
        let iso = Isolated::enter();
        let dir = iso.ws.path().join("journal");
        fs::create_dir_all(&dir).unwrap();
        let t = SystemTime::now();
        for name in ["a-1.jsonl", "b-1.jsonl", "c-1.jsonl"] {
            let p = dir.join(name);
            fs::write(&p, "{}\n").unwrap();
            OpenOptions::new()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
        gc_journal_dir(&dir, 2, &dir.join("c-1.jsonl")).unwrap();
        assert!(!dir.join("a-1.jsonl").exists());
        assert!(dir.join("b-1.jsonl").exists());
        assert!(dir.join("c-1.jsonl").exists());
    }

    #[test]
    fn gc_never_deletes_just_written() {
        let iso = Isolated::enter();
        let dir = iso.ws.path().join("journal");
        fs::create_dir_all(&dir).unwrap();
        let t = SystemTime::now();
        for name in ["a-1.jsonl", "b-1.jsonl", "c-1.jsonl"] {
            let p = dir.join(name);
            fs::write(&p, "{}\n").unwrap();
            OpenOptions::new()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
        let just = dir.join("a-1.jsonl");
        gc_journal_dir(&dir, 2, &just).unwrap();
        assert!(just.exists());
        assert!(dir.join("c-1.jsonl").exists());
        assert!(dir.join("b-1.jsonl").exists());
    }

    #[test]
    fn journal_keep_2_from_machine_config_gcs_on_append() {
        let iso = Isolated::enter();
        let cfg = MachineConfig {
            version: MACHINE_CONFIG_VERSION,
            scan_roots: Vec::new(),
            role_bindings: default_role_bindings(),
            phase_timeouts_secs: BTreeMap::new(),
            hermes: crate::config::HermesNotifyConfig::default(),
            progress_stall_secs: None,
            journal_keep: Some(2),
        };
        save_machine_config(&cfg).unwrap();
        let rec = iso.rec();
        let mut hub = JournalHub::new();
        for epoch in [1_u64, 2, 3] {
            seed_run(&rec, "implement", "0034", epoch);
            let snap = snapshot(&rec);
            hub.record(ev(&snap, "echo", Some(0), 1, true, None, false));
        }
        let journal_dir = rec.state_dir.as_ref().unwrap().join("journal");
        let mut names: Vec<_> = fs::read_dir(&journal_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.len(), 2, "names={names:?}");
        assert!(!journal_dir.join("0034-1.jsonl").exists());
        assert!(journal_dir.join("0034-3.jsonl").exists());
    }

    #[test]
    fn journal_keep_zero_skips_gc() {
        let iso = Isolated::enter();
        let cfg = MachineConfig {
            journal_keep: Some(0),
            ..Default::default()
        };
        save_machine_config(&cfg).unwrap();
        let rec = iso.rec();
        let mut hub = JournalHub::new();
        for epoch in [1_u64, 2, 3] {
            seed_run(&rec, "implement", "0034", epoch);
            let snap = snapshot(&rec);
            hub.record(ev(&snap, "echo", Some(0), 1, true, None, false));
        }
        let journal_dir = rec.state_dir.as_ref().unwrap().join("journal");
        let n = fs::read_dir(&journal_dir).unwrap().count();
        assert_eq!(n, 3);
    }

    #[test]
    fn missing_journal_keep_loads_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, r#"{"version":1,"scan_roots":[],"role_bindings":{}}"#).unwrap();
        let loaded = crate::config::load_machine_config_at(&path).unwrap();
        assert_eq!(loaded.journal_keep, None);
        assert_eq!(retention_keep_from(loaded.journal_keep), Some(20));
    }

    fn retention_keep_from(v: Option<u64>) -> Option<u64> {
        match v {
            None => Some(DEFAULT_KEEP),
            Some(0) => None,
            Some(n) => Some(n),
        }
    }

    #[test]
    fn spawn_fail_line_null_exit_dur_zero() {
        let iso = Isolated::enter();
        let rec = iso.rec();
        seed_run(&rec, "implement", "0034", 1);
        let snap = snapshot(&rec);
        let mut hub = JournalHub::new();
        hub.record(ev(&snap, "missing-bin x", None, 0, false, None, true));
        let v = &read_lines(&journal_file(&rec, "0034", 1).unwrap())[0];
        assert!(v["exit"].is_null());
        assert_eq!(v["dur_ms"], 0);
        assert_eq!(v["ok"], false);
        assert!(v.get("env").is_none());
    }
}
