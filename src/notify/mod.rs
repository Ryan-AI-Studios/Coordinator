//! Failure notify: artifact + toast + adapter trait (track 0009) + Hermes (0015).
//! Opt-in progress POSTs on phase advance (track 0033) use [`ProgressEvent`],
//! never toast / `FAILURE.md`, and never fail apply.
//! Opt-in fleet summaries (track 0068, digest 0105) use [`FleetSummaryEvent`] on
//! that same Hermes path. `notify fleet-summary` posts once. `serve` posts at
//! most once per hour, outside 00:00–05:59 local, and only when the fleet flag is
//! on. The digest carries one line per project, and `projects` stays the full
//! machine-readable payload.
//!
//! **0105 progress disposition:** `hermes.progress` stays **off** deliberately.
//! The 0033 stream is unbounded (one POST per canonical phase advance plus every
//! cross-model bounce), has no dedupe (the millis request id defeats the Hermes
//! cache), and has no quiet hours. The hourly fleet summary is the bounded owner
//! channel. The existing 0102 `Duty::FleetNotify` gate vetoes progress first, so
//! an operator opt-in still has a machine kill switch.
//!
//! Hook failure notify only after a successful Phase Outcome **failure** commit.
//! Operator `stop` is not a Failure Class and must not notify.

pub mod adapter;
pub mod artifact;
pub mod hermes;
pub mod recovery;
pub mod toast;

use chrono::{DateTime, Timelike, Utc};
use serde::{Deserialize, Serialize};

use crate::features::{Duty, evaluate_loaded};
use crate::outcome::FailureClass;
use crate::registry::ProjectRecord;
use crate::state::{RunStatus, StatusView};

pub use adapter::{
    ArtifactAdapter, Composite, HermesAdapter, LogAdapter, NotifyAdapter, RecordingAdapter,
};
pub use artifact::{
    AUTO_CLEAR_DETAIL, ArtifactMeta, FailureShow, LAST_EVENT_FAILURE_RESOLVED, SETTLED_DETAIL,
    START_CLEAR_DETAIL, SUPERSEDED_COMPLETED, SUPERSEDED_MISMATCH, clear as clear_artifact,
    compute_superseded, parse_metadata, settle_failure, track_ids_match,
};
pub use recovery::recommended_action;
pub use toast::{ENV_COORDINATOR_NOTIFY, ToastAdapter, notify_enabled};

/// Opt-in Hermes progress POSTs (`1` / `true` / `on`). `off` force-disables.
pub const ENV_COORDINATOR_NOTIFY_PROGRESS: &str = "COORDINATOR_NOTIFY_PROGRESS";

/// JSON `event_type` for [`ProgressEvent`]. Failure [`NotifyEvent`] has no such field.
pub const EVENT_TYPE_PROGRESS: &str = "progress";

/// Opt-in periodic fleet summaries (`1` / `true` / `on`). `off` force-disables.
pub const ENV_COORDINATOR_NOTIFY_FLEET: &str = "COORDINATOR_NOTIFY_FLEET";

/// JSON `event_type` and `X-Coordinator-Event` value for [`FleetSummaryEvent`].
pub const EVENT_TYPE_FLEET_SUMMARY: &str = "fleet_summary";

/// JSON `event_type` and `X-Coordinator-Event` value for [`StallEvent`] (0104).
pub const EVENT_TYPE_STALL: &str = "stall";

/// Minimum gap between serve-loop fleet posts.
pub const FLEET_SUMMARY_INTERVAL: chrono::Duration = chrono::Duration::seconds(3600);

/// 0105: Telegram `sendMessage` allows 1–4096 characters after entity parsing;
/// leave room for the route template prefix.
const FLEET_SUMMARY_MAX_CHARS: usize = 3500;

/// 0105: one project's `last_event` in the digest (the retired reporter used 120).
const FLEET_LAST_EVENT_MAX_CHARS: usize = 120;

/// 0105: the retired `track-progress-watch.py` kept Telegram silent 00:00–05:59
/// local. This is a constant, not a config key; a different window is a later
/// track.
const FLEET_SUMMARY_QUIET_UNTIL_HOUR: u32 = 6;

/// Payload fanned out to every [`NotifyAdapter`].
///
/// Hermes v1.x POSTs this JSON (unchanged schema) to an opt-in local inbound webhook.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NotifyEvent {
    pub project_id: String,
    pub track_id: Option<String>,
    pub phase: String,
    pub failure_class: FailureClass,
    pub message: Option<String>,
    pub last_event: String,
    pub artifact_path: std::path::PathBuf,
    pub written_at: DateTime<Utc>,
    pub run_epoch: u64,
}

/// Phase-transition payload for opt-in Hermes progress POSTs (track 0033).
///
/// Not a [`NotifyEvent`]: no `failure_class`, no artifact path, never toast.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProgressEvent {
    pub event_type: String,
    pub project_id: String,
    pub track_id: Option<String>,
    pub from_phase: String,
    pub to_phase: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_secs: Option<u64>,
    pub last_event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub written_at: DateTime<Utc>,
    pub run_epoch: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_track: Option<String>,
}

/// Fleet snapshot posted on the Hermes path (track 0068; digest 0105).
///
/// `projects` is [`StatusView`] from `api::status_all` and remains the full
/// machine-readable payload (never truncated). `message` is the human-readable
/// digest the existing Hermes route template reads as `{message}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FleetSummaryEvent {
    pub event_type: String,
    pub written_at: DateTime<Utc>,
    pub projects: Vec<StatusView>,
    /// 0105: human-readable digest. `projects` remains the full machine-readable payload.
    /// The existing Hermes route template reads `{message}`.
    #[serde(default)]
    pub message: String,
}

/// Stall page kind. `begin` opens one owner page per `(project, track, epoch)`;
/// `clear` is its single matching resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StallKind {
    Begin,
    Clear,
}

impl StallKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Begin => "begin",
            Self::Clear => "clear",
        }
    }
}

/// Why a stall page was resolved. Precedence: run_ended → track → phase → progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StallClearReason {
    Progress,
    Phase,
    Track,
    RunEnded,
}

impl StallClearReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Progress => "progress",
            Self::Phase => "phase",
            Self::Track => "track",
            Self::RunEnded => "run_ended",
        }
    }
}

/// Owner stall page (0104). Not a [`NotifyEvent`]: no `failure_class`, no artifact,
/// no toast, no retry. `track_id` is omitted when `None`; `stalled_at`/`idle_secs`
/// appear only on begin, `cleared_at`/`clear_reason` only on clear.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StallEvent {
    pub event_type: String,
    pub kind: StallKind,
    pub project_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_id: Option<String>,
    /// begin: current phase; clear: the paged phase (stored-key echo).
    pub phase: String,
    pub run_epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stalled_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleared_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clear_reason: Option<StallClearReason>,
    pub last_event: String,
    pub written_at: DateTime<Utc>,
}

/// Test-only delivery-mode recorder.
///
/// The in-process Hermes test slot delivers both transports synchronously, so a
/// request count alone cannot prove that `run::stop` used the blocking path. The
/// recorder is the only non-vacuous seam (mirrors `features::take_feature_warnings`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallDelivery {
    Detached,
    Blocking,
}

#[cfg(test)]
static RECORDED_STALL_DELIVERIES: std::sync::Mutex<Vec<StallDelivery>> =
    std::sync::Mutex::new(Vec::new());

/// Single notify entry. Never fails the caller (toast/adapter errors isolated).
pub fn on_hard_failure(record: &ProjectRecord, event: &NotifyEvent) {
    let event = {
        let mut e = event.clone();
        if let Ok(p) = artifact::path(record) {
            e.artifact_path = p;
        }
        e
    };
    let _ = Composite::default_stack().notify(&event);
}

fn env_flag_off(name: &str) -> bool {
    matches!(
        std::env::var(name),
        Ok(s) if s.eq_ignore_ascii_case("off")
    )
}

fn env_flag_on(name: &str) -> bool {
    matches!(
        std::env::var(name),
        Ok(s) if s.eq_ignore_ascii_case("1")
            || s.eq_ignore_ascii_case("true")
            || s.eq_ignore_ascii_case("on")
    )
}

/// Feature gate **veto** first (0102 `fleet_notify`, the notify-family key), then
/// env `off` wins; else env on **or** machine `hermes.progress` **or** project flag.
/// One config load per call (live re-read; no caching).
///
/// 0105 deliberately keeps the 0033 stream off: it is unbounded, has no dedupe
/// (the millis request id defeats the Hermes cache), and has no quiet hours. The
/// gate means a later opt-in still has a machine kill switch.
pub fn progress_enabled(record: &ProjectRecord) -> bool {
    let loaded = crate::config::load_machine_config();
    let gate = evaluate_loaded(&loaded);
    if !gate.enabled(Duty::FleetNotify) {
        return false;
    }
    if env_flag_off(ENV_COORDINATOR_NOTIFY_PROGRESS) {
        return false;
    }
    if env_flag_on(ENV_COORDINATOR_NOTIFY_PROGRESS) {
        return true;
    }
    let machine = loaded.map(|c| c.hermes.progress).unwrap_or(false);
    machine || record.notify_progress
}

/// Feature gate **veto** first (0102), then env `off` wins; else env on **or**
/// machine `hermes.fleet_summary`. No per-project flag. The CLI probe does not
/// consult this. One config load per call (live re-read; no caching).
pub fn fleet_summary_enabled() -> bool {
    let loaded = crate::config::load_machine_config();
    let gate = evaluate_loaded(&loaded);
    if !gate.enabled(Duty::FleetNotify) {
        return false;
    }
    if env_flag_off(ENV_COORDINATOR_NOTIFY_FLEET) {
        return false;
    }
    if env_flag_on(ENV_COORDINATOR_NOTIFY_FLEET) {
        return true;
    }
    loaded.map(|c| c.hermes.fleet_summary).unwrap_or(false)
}

/// `enabled` false is never due. A missing `last_sent` is due. Otherwise due
/// once `now` is at least [`FLEET_SUMMARY_INTERVAL`] after `last_sent`.
pub fn fleet_summary_due(
    last_sent: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    enabled: bool,
) -> bool {
    if !enabled {
        return false;
    }
    match last_sent {
        None => true,
        Some(last) => now.signed_duration_since(last) >= FLEET_SUMMARY_INTERVAL,
    }
}

/// 0105: the reporter's quiet window (00:00–05:59 local), fixed for now.
fn fleet_summary_quiet(local_hour: u32) -> bool {
    local_hour < FLEET_SUMMARY_QUIET_UNTIL_HOUR
}

fn project_label(view: &StatusView) -> String {
    view.display_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            view.path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| view.project_id.clone())
}

/// Flatten CR/LF to spaces, cap by characters (not bytes), append `...` only when cut.
fn one_line(text: &str, max_chars: usize) -> String {
    let flattened: String = text
        .chars()
        .map(|c| if c == '\r' || c == '\n' { ' ' } else { c })
        .collect();
    if flattened.chars().count() <= max_chars {
        return flattened;
    }
    let keep = max_chars.saturating_sub(3);
    let mut out: String = flattened.chars().take(keep).collect();
    out.push_str("...");
    out
}

fn cap_fleet_text(text: String) -> String {
    if text.chars().count() <= FLEET_SUMMARY_MAX_CHARS {
        return text;
    }
    let marker = "\n... (truncated)";
    let keep = FLEET_SUMMARY_MAX_CHARS.saturating_sub(marker.chars().count());
    let mut out: String = text.chars().take(keep).collect();
    out.push_str(marker);
    out
}

/// 0105: one full snapshot line per project, registry order preserved. The total
/// digest is capped; later project lines may be dropped by the cap, while the
/// caller's `projects` slice is never truncated.
fn fleet_summary_text(projects: &[StatusView]) -> String {
    if projects.is_empty() {
        return "fleet 0 projects".to_string();
    }
    let running = projects
        .iter()
        .filter(|v| v.status == RunStatus::Running)
        .count();
    let paused = projects
        .iter()
        .filter(|v| v.status == RunStatus::Paused)
        .count();
    let idle = projects
        .iter()
        .filter(|v| v.status == RunStatus::Idle)
        .count();
    let stopped = projects
        .iter()
        .filter(|v| v.status == RunStatus::Stopped)
        .count();
    let mut out = format!(
        "fleet {} projects: {} running, {} paused, {} idle, {} stopped",
        projects.len(),
        running,
        paused,
        idle,
        stopped
    );
    for view in projects {
        let track = view
            .track_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("-");
        let phase = if view.phase.trim().is_empty() {
            "-"
        } else {
            view.phase.trim()
        };
        let failure = view
            .failure_class
            .map(|c| format!(" (failure: {})", c.as_str()))
            .unwrap_or_default();
        let event = one_line(view.last_event.trim(), FLEET_LAST_EVENT_MAX_CHARS);
        let event = if event.is_empty() {
            "-".to_string()
        } else {
            event
        };
        out.push('\n');
        out.push_str(&format!(
            "- {} {}: {} / {}{} - {}",
            project_label(view),
            track,
            phase,
            view.status,
            failure,
            event
        ));
    }
    cap_fleet_text(out)
}

/// One fleet snapshot. `written_at` is the caller's clock, not a second read.
pub fn build_fleet_summary_event(now: DateTime<Utc>) -> crate::error::Result<FleetSummaryEvent> {
    let projects = crate::api::status_all()?;
    let message = fleet_summary_text(&projects);
    Ok(FleetSummaryEvent {
        event_type: EVENT_TYPE_FLEET_SUMMARY.to_string(),
        written_at: now,
        projects,
        message,
    })
}

#[cfg(test)]
thread_local! {
    static RECORDED_FLEET_WARNINGS: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// One stderr line per failed fleet attempt; the test recorder proves the line
/// for the synchronous Hermes Test arm. The production `HermesKind::Http` arm
/// logs the same literal from its detached thread (inspection-only).
fn fleet_warn(line: &str) {
    eprintln!("{line}");
    #[cfg(test)]
    RECORDED_FLEET_WARNINGS.with(|r| r.borrow_mut().push(line.to_string()));
}

#[cfg(test)]
pub(crate) fn take_fleet_warnings_for_tests() -> Vec<String> {
    RECORDED_FLEET_WARNINGS.with(|r| std::mem::take(&mut *r.borrow_mut()))
}

/// Production entry: one chance per serve pass; derives the local hour from the
/// caller's `now` (no second clock sample), so tests can pass a local-noon instant.
pub fn fleet_tick(last_sent: &mut Option<DateTime<Utc>>, now: DateTime<Utc>) {
    let local_hour = chrono::DateTime::<chrono::Local>::from(now).hour();
    fleet_tick_at(last_sent, now, local_hour);
}

/// Test seam. Quiet hours return **before** `last_sent` is stamped, so the first
/// tick at 06:00 posts rather than waiting another hour.
fn fleet_tick_at(last_sent: &mut Option<DateTime<Utc>>, now: DateTime<Utc>, local_hour: u32) {
    if fleet_summary_quiet(local_hour) {
        return;
    }
    if !fleet_summary_due(*last_sent, now, fleet_summary_enabled()) {
        return;
    }
    *last_sent = Some(now);
    let event = match build_fleet_summary_event(now) {
        Ok(event) => event,
        Err(e) => {
            fleet_warn(&format!(
                "coordinator: fleet summary failed (non-fatal): {e}"
            ));
            return;
        }
    };
    if let Err(e) = hermes::HermesAdapter::for_default_stack().notify_fleet(&event) {
        fleet_warn(&format!(
            "coordinator: fleet summary failed (non-fatal): {e}"
        ));
    }
}

/// Hermes (+ one stderr line). Never artifact, never toast, never fails the caller.
pub fn on_progress(event: &ProgressEvent) {
    eprintln!(
        "coordinator: progress {} → {} track={} {}",
        event.from_phase,
        event.to_phase,
        event.track_id.as_deref().unwrap_or("-"),
        event.last_event
    );
    if let Err(e) = hermes::HermesAdapter::for_default_stack().notify_progress(event) {
        eprintln!("coordinator: progress hermes failed (non-fatal): {e}");
    }
}

/// One stderr line + one detached Hermes POST (0104).
///
/// Never artifact, never toast, never fails the caller. With an unconfigured or
/// `NoOp` Hermes the page is still claimed by the caller and only the stderr line
/// is written (no POST, no retry).
pub fn on_stall(event: &StallEvent) {
    record_stall_delivery(StallDelivery::Detached);
    eprintln!(
        "coordinator: stall page: {} track={} phase={} epoch={}",
        event.kind.as_str(),
        event.track_id.as_deref().unwrap_or("-"),
        event.phase,
        event.run_epoch
    );
    if let Err(e) = hermes::HermesAdapter::for_default_stack().notify_stall(event) {
        eprintln!("coordinator: stall page hermes failed (non-fatal): {e}");
    }
}

/// One stderr line + one **synchronous** Hermes POST, bounded by `POST_TIMEOUT`
/// (0104 operator stop only). No thread; a failure is logged and never fails the
/// caller (at-most-once; no retry).
pub fn on_stall_blocking(event: &StallEvent) {
    record_stall_delivery(StallDelivery::Blocking);
    eprintln!(
        "coordinator: stall page: {} track={} phase={} epoch={}",
        event.kind.as_str(),
        event.track_id.as_deref().unwrap_or("-"),
        event.phase,
        event.run_epoch
    );
    if let Err(e) = hermes::HermesAdapter::for_default_stack().notify_stall_blocking(event) {
        eprintln!("coordinator: stall page hermes failed (non-fatal): {e}");
    }
}

fn record_stall_delivery(mode: StallDelivery) {
    #[cfg(test)]
    {
        let mut recorded = RECORDED_STALL_DELIVERIES
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        recorded.push(mode);
    }
    #[cfg(not(test))]
    {
        let _ = mode;
    }
}

/// Drain the delivery-mode recorder (test-only).
#[cfg(test)]
pub(crate) fn take_stall_delivery_modes_for_tests() -> Vec<StallDelivery> {
    let mut recorded = RECORDED_STALL_DELIVERIES
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    std::mem::take(&mut *recorded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcome::{FailureClass, OutcomeSource, PhaseOutcome};
    use crate::run::{self, run_stub, run_with_driver};
    use crate::state::{STOP_LAST_EVENT, STUB_PHASE_ACTIVE};
    use crate::workflow::WorkflowDriver;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn rec(path: &std::path::Path) -> ProjectRecord {
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
            review_comment_routing: false,
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn each_class_writes_artifact_with_recommended_action() {
        use crate::config::test_env_lock;
        let _guard = test_env_lock();
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
        toast::clear_recorded_toasts();
        for class in FailureClass::ALL {
            let dir = tempdir().unwrap();
            let r = rec(dir.path());
            run_stub(&r, Some("0009".into())).unwrap();
            let o = PhaseOutcome::failure(
                STUB_PHASE_ACTIVE,
                class,
                OutcomeSource::Test,
                Some(format!("msg-{class}")),
                None,
            );
            let view = crate::outcome::apply(&r, o).unwrap();
            assert_eq!(view.failure_class, Some(class));
            let shown = artifact::read(&r).unwrap().expect("FAILURE.md");
            assert!(shown.body.contains(&format!("project_id: {}", r.id)));
            assert!(shown.body.contains("track_id: 0009"));
            assert!(shown.body.contains("phase: stub:failed"));
            assert!(shown.body.contains(&format!("failure_class: {class}")));
            assert!(shown.body.contains("run_epoch:"));
            assert!(shown.body.contains("written_at:"));
            assert!(shown.body.contains(recommended_action(class)));
            assert!(shown.body.contains("automatic recovery has stopped"));
            assert!(shown.body.contains(&format!("msg-{class}")));
            assert!(view.failure_artifact.is_some());
        }
        let toasts = toast::take_recorded_toasts();
        assert_eq!(toasts.len(), FailureClass::ALL.len());
    }

    #[test]
    fn apply_success_does_not_write_artifact() {
        use crate::config::test_env_lock;
        let _guard = test_env_lock();
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
        toast::clear_recorded_toasts();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_stub(&r, None).unwrap();
        let o = PhaseOutcome::success(STUB_PHASE_ACTIVE, OutcomeSource::Test, None, None, None);
        crate::outcome::apply(&r, o).unwrap();
        assert!(artifact::read(&r).unwrap().is_none());
        assert!(toast::take_recorded_toasts().is_empty());
    }

    #[test]
    fn run_clears_artifact() {
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_stub(&r, None).unwrap();
        let o = PhaseOutcome::failure(
            STUB_PHASE_ACTIVE,
            FailureClass::Timeout,
            OutcomeSource::Test,
            Some("budget".into()),
            None,
        );
        crate::outcome::apply(&r, o).unwrap();
        assert!(artifact::existing_path(&r).is_some());
        run_with_driver(&r, None, WorkflowDriver::FileWait).unwrap();
        assert!(
            artifact::existing_path(&r).is_none(),
            "fresh run must remove FAILURE.md"
        );
    }

    #[test]
    fn stop_does_not_notify() {
        use crate::config::test_env_lock;
        let _guard = test_env_lock();
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
        toast::clear_recorded_toasts();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0009".into()), WorkflowDriver::FileWait).unwrap();
        let s = run::stop(&r).unwrap();
        assert_eq!(s.last_event, STOP_LAST_EVENT);
        assert!(s.failure_class.is_none());
        assert!(artifact::existing_path(&r).is_none());
        assert!(toast::take_recorded_toasts().is_empty());
    }

    #[test]
    fn pause_does_not_notify() {
        use crate::config::test_env_lock;
        let _guard = test_env_lock();
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
        toast::clear_recorded_toasts();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, None, WorkflowDriver::FileWait).unwrap();
        run::pause(&r).unwrap();
        assert!(artifact::existing_path(&r).is_none());
        assert!(toast::take_recorded_toasts().is_empty());
    }

    #[test]
    fn notify_off_skips_toast_still_writes_artifact() {
        use crate::config::test_env_lock;
        let _guard = test_env_lock();
        toast::clear_recorded_toasts();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_NOTIFY, "off");
        }
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_stub(&r, None).unwrap();
        let o = PhaseOutcome::failure(
            STUB_PHASE_ACTIVE,
            FailureClass::Permission,
            OutcomeSource::Test,
            Some(r"denied C:\dev\secret".into()),
            None,
        );
        crate::outcome::apply(&r, o).unwrap();
        let shown = artifact::read(&r).unwrap().expect("artifact");
        assert!(shown.body.contains(r"C:\dev\secret"));
        assert!(toast::take_recorded_toasts().is_empty());
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    #[test]
    fn timeout_synthesizer_writes_artifact() {
        use crate::config::test_env_lock;
        use crate::workflow::ENV_PHASE_TIMEOUT_SECS;
        let _guard = test_env_lock();
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
        toast::clear_recorded_toasts();
        unsafe {
            std::env::set_var(ENV_PHASE_TIMEOUT_SECS, "1");
        }
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, None, WorkflowDriver::FileWait).unwrap();
        let mut state = crate::state::load_run_state(&r).unwrap();
        state.phase_started_at = Some(Utc::now() - chrono::Duration::seconds(5));
        crate::state::save_run_state(&r, &state).unwrap();
        let view = crate::outcome::try_timeout_under_lock(&r)
            .unwrap()
            .expect("timeout");
        assert_eq!(view.failure_class, Some(FailureClass::Timeout));
        let shown = artifact::read(&r).unwrap().expect("FAILURE.md");
        assert!(shown.body.contains("failure_class: timeout"));
        assert!(shown.body.contains("Increase the phase budget"));
        assert!(!toast::take_recorded_toasts().is_empty());
        unsafe {
            std::env::remove_var(ENV_PHASE_TIMEOUT_SECS);
        }
    }

    #[test]
    fn idempotent_reapply_does_not_double_toast() {
        use crate::config::test_env_lock;
        let _guard = test_env_lock();
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
        toast::clear_recorded_toasts();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_stub(&r, None).unwrap();
        let o = PhaseOutcome::failure(
            STUB_PHASE_ACTIVE,
            FailureClass::CiFailed,
            OutcomeSource::Test,
            Some("red".into()),
            None,
        );
        crate::outcome::apply(&r, o.clone()).unwrap();
        crate::outcome::apply(&r, o).unwrap();
        assert_eq!(toast::take_recorded_toasts().len(), 1);
    }

    #[test]
    fn apply_failure_survives_hermes_401() {
        use crate::config::test_env_lock;
        let _guard = test_env_lock();
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
        toast::clear_recorded_toasts();
        let _h = hermes::install_scripted(
            "http://127.0.0.1:8644/webhooks/coordinator-failure",
            "s",
            hermes::ScriptedOutcome::Status(401),
        );
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_stub(&r, Some("0015".into())).unwrap();
        let o = PhaseOutcome::failure(
            STUB_PHASE_ACTIVE,
            FailureClass::Timeout,
            OutcomeSource::Test,
            Some("budget".into()),
            None,
        );
        crate::outcome::apply(&r, o).unwrap();
        assert!(artifact::existing_path(&r).is_some());
        assert_eq!(toast::take_recorded_toasts().len(), 1);
        assert_eq!(_h.take().len(), 1);
    }

    #[test]
    fn stop_and_pause_do_not_hermes() {
        use crate::config::test_env_lock;
        let _guard = test_env_lock();
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
        let rec_h =
            hermes::install_recording("http://127.0.0.1:8644/webhooks/coordinator-failure", "s");
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0015".into()), WorkflowDriver::FileWait).unwrap();
        run::stop(&r).unwrap();
        assert!(rec_h.take().is_empty());
        drop(rec_h);

        let rec_h =
            hermes::install_recording("http://127.0.0.1:8644/webhooks/coordinator-failure", "s");
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, None, WorkflowDriver::FileWait).unwrap();
        run::pause(&r).unwrap();
        assert!(rec_h.take().is_empty());
    }

    #[test]
    fn notify_off_does_not_disable_hermes() {
        use crate::config::test_env_lock;
        let _guard = test_env_lock();
        toast::clear_recorded_toasts();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_NOTIFY, "off");
        }
        let rec_h =
            hermes::install_recording("http://127.0.0.1:8644/webhooks/coordinator-failure", "s");
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_stub(&r, None).unwrap();
        let o = PhaseOutcome::failure(
            STUB_PHASE_ACTIVE,
            FailureClass::Permission,
            OutcomeSource::Test,
            Some("denied".into()),
            None,
        );
        crate::outcome::apply(&r, o).unwrap();
        assert!(artifact::existing_path(&r).is_some());
        assert!(toast::take_recorded_toasts().is_empty());
        assert_eq!(rec_h.take().len(), 1);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
    }

    fn hdr(req: &hermes::CapturedRequest, name: &str) -> String {
        req.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    fn jump_phase(r: &ProjectRecord, phase: &str, track: &str) {
        run_with_driver(r, Some(track.into()), WorkflowDriver::FileWait).unwrap();
        let mut s = crate::state::load_run_state(r).unwrap();
        s.phase = phase.into();
        crate::state::save_run_state(r, &s).unwrap();
    }

    #[test]
    fn progress_default_off_success_posts_nothing() {
        use crate::config::{ENV_COORDINATOR_HOME, test_env_lock};
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::remove_var(ENV_COORDINATOR_NOTIFY_PROGRESS);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
        toast::clear_recorded_toasts();
        let rec_h =
            hermes::install_recording("http://127.0.0.1:8644/webhooks/coordinator-progress", "s");
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0033".into()), WorkflowDriver::FileWait).unwrap();
        let o = PhaseOutcome::success(
            crate::workflow::graph::PHASE_PLAN,
            OutcomeSource::Test,
            None,
            None,
            None,
        );
        crate::outcome::apply(&r, o).unwrap();
        assert!(rec_h.take().is_empty());
        assert!(artifact::existing_path(&r).is_none());
        assert!(toast::take_recorded_toasts().is_empty());
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[test]
    fn progress_env_on_canonical_success_posts_once() {
        use crate::config::{ENV_COORDINATOR_HOME, test_env_lock};
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(ENV_COORDINATOR_NOTIFY_PROGRESS, "1");
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
        toast::clear_recorded_toasts();
        let rec_h =
            hermes::install_recording("http://127.0.0.1:8644/webhooks/coordinator-progress", "s");
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0033".into()), WorkflowDriver::FileWait).unwrap();
        let before = crate::state::load_run_state(&r).unwrap();
        let o = PhaseOutcome::success(
            crate::workflow::graph::PHASE_PLAN,
            OutcomeSource::Test,
            None,
            None,
            None,
        );
        let view = crate::outcome::apply(&r, o.clone()).unwrap();
        assert_eq!(view.phase, crate::workflow::graph::PHASE_PLAN_REVIEW);
        assert!(artifact::existing_path(&r).is_none());
        assert!(toast::take_recorded_toasts().is_empty());
        let posts = rec_h.take();
        assert_eq!(posts.len(), 1);
        assert_eq!(hdr(&posts[0], "X-Coordinator-Event"), "progress");
        let body: serde_json::Value = serde_json::from_slice(&posts[0].body).unwrap();
        assert_eq!(body["event_type"], "progress");
        assert!(body.get("failure_class").is_none());
        let parsed: ProgressEvent = serde_json::from_slice(&posts[0].body).unwrap();
        assert_eq!(parsed.track_id.as_deref(), Some("0033"));
        assert_eq!(parsed.from_phase, crate::workflow::graph::PHASE_PLAN);
        assert_eq!(parsed.to_phase, crate::workflow::graph::PHASE_PLAN_REVIEW);
        assert_eq!(parsed.run_epoch, before.run_epoch);
        assert_eq!(
            hdr(&posts[0], "X-Request-ID"),
            hermes::progress_request_id(&parsed)
        );
        crate::outcome::apply(&r, o).unwrap();
        assert!(
            rec_h.take().is_empty(),
            "idempotent re-apply must not POST again"
        );
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_NOTIFY_PROGRESS);
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[test]
    fn progress_bounce_posts_without_artifact() {
        use crate::config::{ENV_COORDINATOR_HOME, test_env_lock};
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(ENV_COORDINATOR_NOTIFY_PROGRESS, "true");
        }
        toast::clear_recorded_toasts();
        let rec_h =
            hermes::install_recording("http://127.0.0.1:8644/webhooks/coordinator-progress", "s");
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("conductor").join("0031-Example")).unwrap();
        let r = rec(dir.path());
        jump_phase(&r, crate::workflow::graph::PHASE_CROSS_MODEL, "0031");
        let o = PhaseOutcome::failure(
            crate::workflow::graph::PHASE_CROSS_MODEL,
            FailureClass::Difficulty,
            OutcomeSource::File,
            Some("cross-model: gate failed (codex)".into()),
            None,
        );
        let view = crate::outcome::apply(&r, o).unwrap();
        assert_eq!(view.phase, crate::workflow::graph::PHASE_ADDRESS_FINDINGS);
        assert!(artifact::existing_path(&r).is_none());
        assert!(toast::take_recorded_toasts().is_empty());
        let posts = rec_h.take();
        assert_eq!(posts.len(), 1);
        assert_eq!(hdr(&posts[0], "X-Coordinator-Event"), "progress");
        let parsed: ProgressEvent = serde_json::from_slice(&posts[0].body).unwrap();
        assert_eq!(parsed.from_phase, crate::workflow::graph::PHASE_CROSS_MODEL);
        assert_eq!(
            parsed.to_phase,
            crate::workflow::graph::PHASE_ADDRESS_FINDINGS
        );
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_NOTIFY_PROGRESS);
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[test]
    fn progress_two_bounces_distinct_request_ids() {
        use crate::config::{ENV_COORDINATOR_HOME, test_env_lock};
        use crate::outcome::write_and_apply;
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(ENV_COORDINATOR_NOTIFY_PROGRESS, "on");
        }
        let rec_h =
            hermes::install_recording("http://127.0.0.1:8644/webhooks/coordinator-progress", "s");
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("conductor").join("0031-Example")).unwrap();
        let r = rec(dir.path());
        jump_phase(&r, crate::workflow::graph::PHASE_CROSS_MODEL, "0031");
        let mut first = PhaseOutcome::failure(
            crate::workflow::graph::PHASE_CROSS_MODEL,
            FailureClass::Difficulty,
            OutcomeSource::File,
            Some("gate 1".into()),
            None,
        );
        first.written_at = Utc::now();
        crate::outcome::apply(&r, first.clone()).unwrap();
        write_and_apply(
            &r,
            PhaseOutcome::success(
                crate::workflow::graph::PHASE_ADDRESS_FINDINGS,
                OutcomeSource::Test,
                None,
                None,
                None,
            ),
        )
        .unwrap();
        let mut second = PhaseOutcome::failure(
            crate::workflow::graph::PHASE_CROSS_MODEL,
            FailureClass::Difficulty,
            OutcomeSource::File,
            Some("gate 2".into()),
            None,
        );
        second.written_at = first.written_at + chrono::Duration::milliseconds(3);
        crate::outcome::apply(&r, second).unwrap();
        let posts = rec_h.take();
        let progress: Vec<_> = posts
            .iter()
            .filter(|p| hdr(p, "X-Coordinator-Event") == "progress")
            .collect();
        assert_eq!(progress.len(), 3, "two bounces + address-findings success");
        let bounce_ids: Vec<_> = progress
            .iter()
            .filter_map(|p| {
                let ev: ProgressEvent = serde_json::from_slice(&p.body).ok()?;
                if ev.to_phase == crate::workflow::graph::PHASE_ADDRESS_FINDINGS {
                    Some(hdr(p, "X-Request-ID"))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(bounce_ids.len(), 2);
        assert_ne!(bounce_ids[0], bounce_ids[1]);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_NOTIFY_PROGRESS);
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[test]
    fn progress_pause_then_bounce_still_posts() {
        use crate::config::{ENV_COORDINATOR_HOME, test_env_lock};
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(ENV_COORDINATOR_NOTIFY_PROGRESS, "1");
        }
        toast::clear_recorded_toasts();
        let rec_h =
            hermes::install_recording("http://127.0.0.1:8644/webhooks/coordinator-progress", "s");
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("conductor").join("0031-Example")).unwrap();
        let r = rec(dir.path());
        jump_phase(&r, crate::workflow::graph::PHASE_CROSS_MODEL, "0031");
        run::pause(&r).unwrap();
        assert!(rec_h.take().is_empty(), "pause command must not POST");
        let o = PhaseOutcome::failure(
            crate::workflow::graph::PHASE_CROSS_MODEL,
            FailureClass::Difficulty,
            OutcomeSource::File,
            Some("gate while paused".into()),
            None,
        );
        let view = crate::outcome::apply(&r, o).unwrap();
        assert_eq!(view.status, crate::state::RunStatus::Paused);
        assert_eq!(view.phase, crate::workflow::graph::PHASE_ADDRESS_FINDINGS);
        assert!(artifact::existing_path(&r).is_none());
        assert!(toast::take_recorded_toasts().is_empty());
        assert_eq!(rec_h.take().len(), 1);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_NOTIFY_PROGRESS);
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[test]
    fn progress_backlog_clear_to_phase_idle() {
        use crate::config::{ENV_COORDINATOR_HOME, test_env_lock};
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(ENV_COORDINATOR_NOTIFY_PROGRESS, "1");
        }
        let rec_h =
            hermes::install_recording("http://127.0.0.1:8644/webhooks/coordinator-progress", "s");
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        jump_phase(&r, crate::workflow::graph::PHASE_ADVANCE, "0033");
        let mut s = crate::state::load_run_state(&r).unwrap();
        s.next_track = None;
        crate::state::save_run_state(&r, &s).unwrap();
        let view = crate::outcome::apply(
            &r,
            PhaseOutcome::success(
                crate::workflow::graph::PHASE_ADVANCE,
                OutcomeSource::Test,
                None,
                None,
                None,
            ),
        )
        .unwrap();
        assert_eq!(view.status, crate::state::RunStatus::Idle);
        let posts = rec_h.take();
        assert_eq!(posts.len(), 1);
        let parsed: ProgressEvent = serde_json::from_slice(&posts[0].body).unwrap();
        assert_eq!(parsed.from_phase, crate::workflow::graph::PHASE_ADVANCE);
        assert_eq!(parsed.to_phase, "idle");
        assert_eq!(parsed.track_id.as_deref(), Some("0033"));
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_NOTIFY_PROGRESS);
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[test]
    fn progress_auto_start_keeps_pre_apply_track_and_epoch() {
        use crate::config::{ENV_COORDINATOR_HOME, test_env_lock};
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(ENV_COORDINATOR_NOTIFY_PROGRESS, "1");
        }
        let rec_h =
            hermes::install_recording("http://127.0.0.1:8644/webhooks/coordinator-progress", "s");
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("conductor").join("0033-Example")).unwrap();
        std::fs::create_dir_all(dir.path().join("conductor").join("0040-Next")).unwrap();
        std::fs::write(
            dir.path().join("conductor").join("conductor.md"),
            "| Track | Execution path | Status | Summary |\n\
             | --- | --- | --- | --- |\n\
             | [0033-Example](0033-Example/spec.md) | `.` | **Completed** | done |\n\
             | [0040-Next](0040-Next/spec.md) | `.` | **Ready — not started** | next |\n",
        )
        .unwrap();
        let mut r = rec(dir.path());
        r.auto_start = crate::registry::AutoStartPolicy::Full;
        jump_phase(&r, crate::workflow::graph::PHASE_ADVANCE, "0033");
        let mut s = crate::state::load_run_state(&r).unwrap();
        s.next_track = Some("0040".into());
        let snap_epoch = s.run_epoch;
        crate::state::save_run_state(&r, &s).unwrap();
        crate::outcome::apply(
            &r,
            PhaseOutcome::success(
                crate::workflow::graph::PHASE_ADVANCE,
                OutcomeSource::Test,
                None,
                None,
                None,
            ),
        )
        .unwrap();
        let posts = rec_h.take();
        assert_eq!(posts.len(), 1);
        let parsed: ProgressEvent = serde_json::from_slice(&posts[0].body).unwrap();
        assert_eq!(parsed.track_id.as_deref(), Some("0033"));
        assert_eq!(parsed.to_phase, crate::workflow::graph::PHASE_PLAN);
        assert_eq!(parsed.next_track.as_deref(), Some("0040"));
        assert_eq!(parsed.run_epoch, snap_epoch);
        let after = crate::state::load_run_state(&r).unwrap();
        assert_eq!(after.track_id.as_deref(), Some("0040"));
        assert_eq!(after.run_epoch, snap_epoch + 1);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_NOTIFY_PROGRESS);
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[test]
    fn progress_timeout_failure_is_hard_failure_not_progress() {
        use crate::config::{ENV_COORDINATOR_HOME, test_env_lock};
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(ENV_COORDINATOR_NOTIFY_PROGRESS, "1");
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
        }
        toast::clear_recorded_toasts();
        let rec_h =
            hermes::install_recording("http://127.0.0.1:8644/webhooks/coordinator-failure", "s");
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0033".into()), WorkflowDriver::FileWait).unwrap();
        let o = PhaseOutcome::failure(
            crate::workflow::graph::PHASE_PLAN,
            FailureClass::Timeout,
            OutcomeSource::Timeout,
            Some("phase budget exceeded".into()),
            None,
        );
        crate::outcome::apply(&r, o).unwrap();
        assert!(artifact::existing_path(&r).is_some());
        assert_eq!(toast::take_recorded_toasts().len(), 1);
        let posts = rec_h.take();
        assert_eq!(posts.len(), 1);
        assert_eq!(hdr(&posts[0], "X-Coordinator-Event"), "hard_failure");
        let body: serde_json::Value = serde_json::from_slice(&posts[0].body).unwrap();
        assert!(body.get("event_type").is_none());
        assert_eq!(body["failure_class"], "timeout");
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_NOTIFY_PROGRESS);
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[test]
    fn progress_scripted_401_does_not_fail_apply() {
        use crate::config::{ENV_COORDINATOR_HOME, test_env_lock};
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(ENV_COORDINATOR_NOTIFY_PROGRESS, "1");
        }
        let _h = hermes::install_scripted(
            "http://127.0.0.1:8644/webhooks/coordinator-progress",
            "s",
            hermes::ScriptedOutcome::Status(401),
        );
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0033".into()), WorkflowDriver::FileWait).unwrap();
        let view = crate::outcome::apply(
            &r,
            PhaseOutcome::success(
                crate::workflow::graph::PHASE_PLAN,
                OutcomeSource::Test,
                None,
                None,
                None,
            ),
        )
        .unwrap();
        assert_eq!(view.phase, crate::workflow::graph::PHASE_PLAN_REVIEW);
        assert_eq!(view.status, crate::state::RunStatus::Running);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_NOTIFY_PROGRESS);
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[test]
    fn progress_stop_command_posts_nothing() {
        use crate::config::{ENV_COORDINATOR_HOME, test_env_lock};
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(ENV_COORDINATOR_NOTIFY_PROGRESS, "1");
        }
        let rec_h =
            hermes::install_recording("http://127.0.0.1:8644/webhooks/coordinator-progress", "s");
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_with_driver(&r, Some("0033".into()), WorkflowDriver::FileWait).unwrap();
        run::stop(&r).unwrap();
        assert!(rec_h.take().is_empty());
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_NOTIFY_PROGRESS);
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    fn sorted_keys(value: &serde_json::Value) -> Vec<String> {
        let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    }

    fn forbid_secret_keys(value: &serde_json::Value) {
        const BANNED: &[&str] = &[
            "secret",
            "token",
            "password",
            "authorization",
            "api_key",
            "webhook_url",
        ];
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    let lower = key.to_ascii_lowercase();
                    assert!(
                        !BANNED.contains(&lower.as_str()),
                        "credential-shaped key {key}"
                    );
                    forbid_secret_keys(child);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    forbid_secret_keys(item);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn notify_and_progress_field_names_unchanged() {
        let failure = NotifyEvent {
            project_id: "p".into(),
            track_id: Some("t".into()),
            phase: "implement".into(),
            failure_class: FailureClass::Timeout,
            message: Some("m".into()),
            last_event: "x".into(),
            artifact_path: std::path::PathBuf::from("FAILURE.md"),
            written_at: Utc::now(),
            run_epoch: 1,
        };
        let progress = ProgressEvent {
            event_type: EVENT_TYPE_PROGRESS.into(),
            project_id: "p".into(),
            track_id: Some("t".into()),
            from_phase: "plan".into(),
            to_phase: "implement".into(),
            elapsed_secs: Some(1),
            last_event: "x".into(),
            message: Some("m".into()),
            written_at: Utc::now(),
            run_epoch: 1,
            next_track: Some("n".into()),
        };
        assert_eq!(
            sorted_keys(&serde_json::to_value(&failure).unwrap()),
            [
                "artifact_path",
                "failure_class",
                "last_event",
                "message",
                "phase",
                "project_id",
                "run_epoch",
                "track_id",
                "written_at",
            ]
        );
        assert_eq!(
            sorted_keys(&serde_json::to_value(&progress).unwrap()),
            [
                "elapsed_secs",
                "event_type",
                "from_phase",
                "last_event",
                "message",
                "next_track",
                "project_id",
                "run_epoch",
                "to_phase",
                "track_id",
                "written_at",
            ]
        );
    }

    #[test]
    fn fleet_summary_due_table() {
        let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        assert!(!fleet_summary_due(None, now, false));
        assert!(fleet_summary_due(None, now, true));
        let ready = now - FLEET_SUMMARY_INTERVAL;
        assert!(fleet_summary_due(Some(ready), now, true));
        let early = now - (FLEET_SUMMARY_INTERVAL - chrono::Duration::seconds(1));
        assert!(!fleet_summary_due(Some(early), now, true));
        assert!(!fleet_summary_due(Some(now), now, true));
        let backward = now + chrono::Duration::seconds(5);
        assert!(!fleet_summary_due(Some(backward), now, true));
    }

    #[test]
    fn fleet_summary_text_lists_every_project_status_phase_track_and_event() {
        use crate::state::RunStatus;
        let dir = tempdir().unwrap();
        let running = fleet_view(
            Some("Alpha"),
            dir.path(),
            RunStatus::Running,
            "implement",
            Some("0105"),
            "workflow: advance plan -> implement",
        );
        let idle = fleet_view(
            Some("Beta"),
            dir.path(),
            RunStatus::Idle,
            "idle",
            None,
            "idle: no run",
        );
        let text = fleet_summary_text(&[running, idle]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "fleet 2 projects: 1 running, 0 paused, 1 idle, 0 stopped"
        );
        assert_eq!(lines.len(), 3, "{text}");
        assert_eq!(
            lines[1],
            "- Alpha 0105: implement / Running - workflow: advance plan -> implement"
        );
        assert_eq!(lines[2], "- Beta -: idle / Idle - idle: no run");
    }

    #[test]
    fn fleet_summary_text_falls_back_to_path_then_project_id() {
        use crate::state::RunStatus;
        let dir = tempdir().unwrap();
        let mut no_display = fleet_view(
            None,
            dir.path(),
            RunStatus::Idle,
            "idle",
            None,
            "idle: no run",
        );
        no_display.path = std::path::PathBuf::from(r"C:\dev\coordinated");
        let mut root_path = fleet_view(
            None,
            dir.path(),
            RunStatus::Idle,
            "idle",
            None,
            "idle: no run",
        );
        root_path.path = std::path::PathBuf::from(r"C:\");
        let text = fleet_summary_text(&[no_display, root_path.clone()]);
        assert!(text.contains("- coordinated -: idle / Idle - idle: no run"));
        assert!(
            text.contains(&format!(
                "- {} -: idle / Idle - idle: no run",
                root_path.project_id
            )),
            "{text}"
        );
    }

    #[test]
    fn fleet_summary_text_flattens_last_event_newlines() {
        use crate::state::RunStatus;
        let dir = tempdir().unwrap();
        let view = fleet_view(
            Some("A"),
            dir.path(),
            RunStatus::Running,
            "implement",
            Some("0105"),
            "line one\r\nline two\nline three",
        );
        let text = fleet_summary_text(&[view]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "one header + one project line: {text}");
        assert!(!text.contains('\r'), "{text}");
        assert!(lines[1].contains("line one"), "{text}");
        assert!(lines[1].contains("line two line three"), "{text}");
    }

    #[test]
    fn fleet_summary_text_caps_last_event_and_total_length() {
        use crate::state::RunStatus;
        let dir = tempdir().unwrap();
        let long_event = "x".repeat(200);
        let views: Vec<_> = (0..100)
            .map(|i| {
                let label = format!("project-{i:03}");
                fleet_view(
                    Some(&label),
                    dir.path(),
                    RunStatus::Idle,
                    "idle",
                    None,
                    &long_event,
                )
            })
            .collect();
        let text = fleet_summary_text(&views);
        assert!(text.chars().count() <= FLEET_SUMMARY_MAX_CHARS);
        assert!(text.ends_with("... (truncated)"), "{text}");
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.len() > 2);
        for line in &lines[1..] {
            if let Some((_, event)) = line.rsplit_once(" - ") {
                assert!(
                    event.chars().count() <= FLEET_LAST_EVENT_MAX_CHARS,
                    "{line}"
                );
            }
        }
        assert!(
            !text.contains("project-099"),
            "the cap drops later project lines"
        );
        assert_eq!(views.len(), 100, "the projects slice is never truncated");
    }

    #[test]
    fn fleet_summary_text_is_utf8_safe_on_truncation() {
        use crate::state::RunStatus;
        let dir = tempdir().unwrap();
        let long_event = "é".repeat(200);
        let view = fleet_view(
            Some("A"),
            dir.path(),
            RunStatus::Running,
            "implement",
            Some("0105"),
            &long_event,
        );
        let text = fleet_summary_text(&[view]);
        assert!(text.chars().count() <= FLEET_SUMMARY_MAX_CHARS);
        let last = text.lines().last().unwrap();
        let event = last.rsplit_once(" - ").unwrap().1;
        assert!(event.chars().count() <= FLEET_LAST_EVENT_MAX_CHARS);
        assert!(event.ends_with("..."));
    }

    #[test]
    fn fleet_summary_text_zero_projects() {
        assert_eq!(fleet_summary_text(&[]), "fleet 0 projects");
    }

    #[test]
    fn build_fleet_summary_event_message_comes_from_status_all() {
        let _lock = crate::config::test_env_lock();
        let _guard = crate::config::TestEnvGuard::capture(&[crate::config::ENV_COORDINATOR_HOME]);
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        let project = tempdir().unwrap();
        let mut registry = crate::registry::Registry::default();
        let record = registry
            .add(
                project.path(),
                crate::registry::ProjectAddOptions {
                    display_name: Some("Fleet".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        registry
            .save(&crate::config::registry_path().unwrap())
            .unwrap();
        let mut state = crate::state::RunState::idle(&record.id);
        state.status = crate::state::RunStatus::Running;
        state.phase = "plan".into();
        state.track_id = Some("0105".into());
        state.last_event = "workflow: start track=0105".into();
        crate::state::save_run_state(&record, &state).unwrap();

        let now = Utc::now();
        let event = build_fleet_summary_event(now).unwrap();
        assert_eq!(event.written_at, now);
        assert_eq!(event.projects.len(), 1);
        assert!(
            event
                .message
                .contains("- Fleet 0105: plan / Running - workflow: start track=0105"),
            "{}",
            event.message
        );
    }

    #[test]
    fn fleet_summary_quiet_hours_are_midnight_to_six() {
        for hour in 0..=5 {
            assert!(fleet_summary_quiet(hour), "hour {hour} is quiet");
        }
        for hour in 6..=23 {
            assert!(!fleet_summary_quiet(hour), "hour {hour} is not quiet");
        }
    }

    #[test]
    fn fleet_tick_at_quiet_hour_posts_nothing_and_does_not_stamp() {
        let _lock = crate::config::test_env_lock();
        let _guard = fleet_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(ENV_COORDINATOR_NOTIFY_FLEET, "1");
            std::env::remove_var(hermes::ENV_COORDINATOR_HERMES);
        }
        write_machine_config(home.path(), true, serde_json::json!({}));
        assert!(fleet_summary_enabled());
        let sink = hermes::install_recording("http://127.0.0.1:9/hook", "s3cret-fleet");
        let now = local_noon_utc();
        let mut last = None;

        fleet_tick_at(&mut last, now, 3);
        assert!(last.is_none(), "quiet hours do not stamp");
        assert!(sink.take().is_empty());

        fleet_tick_at(&mut last, now, 6);
        assert_eq!(last, Some(now));
        assert_eq!(sink.take().len(), 1);
    }

    #[test]
    fn fleet_tick_at_quiet_hour_does_not_consume_due_after_midnight() {
        let _lock = crate::config::test_env_lock();
        let _guard = fleet_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(ENV_COORDINATOR_NOTIFY_FLEET, "1");
            std::env::remove_var(hermes::ENV_COORDINATOR_HERMES);
        }
        write_machine_config(home.path(), true, serde_json::json!({}));
        let sink = hermes::install_recording("http://127.0.0.1:9/hook", "s3cret-fleet");
        let t0 = Utc::now();
        let mut last = Some(t0);

        fleet_tick_at(&mut last, t0 + chrono::Duration::minutes(6 * 60 + 59), 5);
        assert_eq!(last, Some(t0), "a quiet tick must not stamp last_sent");
        assert!(sink.take().is_empty());

        let catch_up = t0 + chrono::Duration::hours(7);
        fleet_tick_at(&mut last, catch_up, 6);
        assert_eq!(last, Some(catch_up));
        assert_eq!(sink.take().len(), 1, "06:00 catches up one minute later");
    }

    #[test]
    fn fleet_tick_scripted_401_is_non_fatal_logged_and_stamped() {
        let _lock = crate::config::test_env_lock();
        let _guard = fleet_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
            std::env::remove_var(ENV_COORDINATOR_NOTIFY_FLEET);
            std::env::remove_var(hermes::ENV_COORDINATOR_HERMES);
        }
        write_machine_config(home.path(), true, serde_json::json!({}));
        assert!(fleet_summary_enabled());
        let sink = hermes::install_scripted(
            "http://127.0.0.1:9/hook",
            "s",
            hermes::ScriptedOutcome::Status(401),
        );
        let _ = take_fleet_warnings_for_tests();
        let now = local_noon_utc();
        let mut last = None;

        fleet_tick_at(&mut last, now, 12);

        assert_eq!(last, Some(now), "failure still consumes the due slot");
        let warnings = take_fleet_warnings_for_tests();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("non-fatal"), "{warnings:?}");
        assert!(warnings[0].contains("401"), "{warnings:?}");
        assert_eq!(sink.take().len(), 1, "one attempted POST");
    }

    #[test]
    fn fleet_tick_connect_fail_is_non_fatal_logged() {
        let _lock = crate::config::test_env_lock();
        let _guard = fleet_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
            std::env::remove_var(ENV_COORDINATOR_NOTIFY_FLEET);
            std::env::remove_var(hermes::ENV_COORDINATOR_HERMES);
        }
        write_machine_config(home.path(), true, serde_json::json!({}));
        let sink = hermes::install_scripted(
            "http://127.0.0.1:9/hook",
            "s",
            hermes::ScriptedOutcome::ConnectFail,
        );
        let _ = take_fleet_warnings_for_tests();
        let now = local_noon_utc();
        let mut last = None;

        fleet_tick_at(&mut last, now, 12);

        assert_eq!(last, Some(now));
        let warnings = take_fleet_warnings_for_tests();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("non-fatal"), "{warnings:?}");
        assert!(warnings[0].contains("connect refused"), "{warnings:?}");
        assert_eq!(sink.take().len(), 1);
    }

    #[test]
    fn progress_gate_off_blocks_env_on() {
        let _lock = crate::config::test_env_lock();
        let _guard = fleet_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(ENV_COORDINATOR_NOTIFY_PROGRESS, "1");
            std::env::remove_var(hermes::ENV_COORDINATOR_HERMES);
        }
        write_machine_config(
            home.path(),
            false,
            serde_json::json!({"fleet_notify": false}),
        );
        toast::clear_recorded_toasts();
        let dir = tempdir().unwrap();
        let record = rec(dir.path());
        assert!(
            !progress_enabled(&record),
            "the feature gate vetoes env-on progress"
        );
        let sink =
            hermes::install_recording("http://127.0.0.1:8644/webhooks/coordinator-progress", "s");
        run_with_driver(&record, Some("0033".into()), WorkflowDriver::FileWait).unwrap();
        let outcome = PhaseOutcome::success(
            crate::workflow::graph::PHASE_PLAN,
            OutcomeSource::Test,
            None,
            None,
            None,
        );
        crate::outcome::apply(&record, outcome).unwrap();
        assert!(sink.take().is_empty(), "no progress POST under a veto");
        assert!(artifact::existing_path(&record).is_none());
        assert!(toast::take_recorded_toasts().is_empty());
    }

    #[test]
    fn progress_gate_off_blocks_project_flag() {
        let _lock = crate::config::test_env_lock();
        let _guard = fleet_env_guard();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
            std::env::remove_var(ENV_COORDINATOR_NOTIFY_PROGRESS);
            std::env::remove_var(hermes::ENV_COORDINATOR_HERMES);
        }
        write_machine_config(
            home.path(),
            false,
            serde_json::json!({"fleet_notify": false}),
        );
        let dir = tempdir().unwrap();
        let mut record = rec(dir.path());
        record.notify_progress = true;
        assert!(
            !progress_enabled(&record),
            "the gate also vetoes the per-project opt-in"
        );
    }

    #[test]
    fn progress_gate_on_env_on_still_posts() {
        let _lock = crate::config::test_env_lock();
        let _guard = fleet_env_guard();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(ENV_COORDINATOR_NOTIFY_PROGRESS, "1");
            std::env::remove_var(hermes::ENV_COORDINATOR_HERMES);
        }
        write_machine_config(
            home.path(),
            false,
            serde_json::json!({"fleet_notify": true}),
        );
        let dir = tempdir().unwrap();
        let record = rec(dir.path());
        assert!(progress_enabled(&record));
        let sink =
            hermes::install_recording("http://127.0.0.1:8644/webhooks/coordinator-progress", "s");
        run_with_driver(&record, Some("0033".into()), WorkflowDriver::FileWait).unwrap();
        let outcome = PhaseOutcome::success(
            crate::workflow::graph::PHASE_PLAN,
            OutcomeSource::Test,
            None,
            None,
            None,
        );
        crate::outcome::apply(&record, outcome).unwrap();
        let posts = sink.take();
        assert_eq!(posts.len(), 1);
        assert_eq!(hdr(&posts[0], "X-Coordinator-Event"), "progress");
    }

    #[test]
    fn progress_gate_broken_config_fails_closed() {
        let _lock = crate::config::test_env_lock();
        let _guard = fleet_env_guard();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(ENV_COORDINATOR_NOTIFY_PROGRESS, "1");
            std::env::remove_var(hermes::ENV_COORDINATOR_HERMES);
        }
        let dir = tempdir().unwrap();
        let record = rec(dir.path());

        write_machine_config(
            home.path(),
            false,
            serde_json::json!({"fleet_notify": "yes"}),
        );
        crate::features::reset_feature_warnings_for_tests();
        assert!(!progress_enabled(&record));
        assert_eq!(
            crate::features::take_feature_warnings().len(),
            1,
            "one line for the type-invalid table"
        );

        std::fs::write(home.path().join("config.json"), "{not-json").unwrap();
        crate::features::reset_feature_warnings_for_tests();
        assert!(!progress_enabled(&record));
        assert_eq!(
            crate::features::take_feature_warnings().len(),
            1,
            "one line for the unreadable table"
        );

        let sink =
            hermes::install_recording("http://127.0.0.1:8644/webhooks/coordinator-progress", "s");
        run_with_driver(&record, Some("0033".into()), WorkflowDriver::FileWait).unwrap();
        let outcome = PhaseOutcome::success(
            crate::workflow::graph::PHASE_PLAN,
            OutcomeSource::Test,
            None,
            None,
            None,
        );
        crate::outcome::apply(&record, outcome).unwrap();
        assert!(
            sink.take().is_empty(),
            "a broken config fails progress closed"
        );
    }

    fn fleet_env_guard() -> crate::config::TestEnvGuard {
        let mut names = crate::features::test_feature_env_names();
        names.push(crate::config::ENV_COORDINATOR_HOME);
        names.push(ENV_COORDINATOR_NOTIFY_FLEET);
        names.push(ENV_COORDINATOR_NOTIFY_PROGRESS);
        names.push(hermes::ENV_COORDINATOR_HERMES);
        crate::config::TestEnvGuard::capture(&names)
    }

    fn fleet_view(
        display_name: Option<&str>,
        path: &std::path::Path,
        status: crate::state::RunStatus,
        phase: &str,
        track: Option<&str>,
        last_event: &str,
    ) -> crate::state::StatusView {
        let record = rec(path);
        let idle = crate::state::RunState::idle(&record.id);
        let mut view = crate::state::StatusView::from_record(&record, &idle);
        view.display_name = display_name.map(str::to_string);
        view.path = path.to_path_buf();
        view.status = status;
        view.phase = phase.to_string();
        view.track_id = track.map(str::to_string);
        view.last_event = last_event.to_string();
        view.failure_class = None;
        view
    }

    fn local_noon_utc() -> DateTime<Utc> {
        use chrono::TimeZone;
        chrono::Local
            .with_ymd_and_hms(2026, 10, 20, 12, 0, 0)
            .single()
            .expect("unambiguous local noon")
            .with_timezone(&Utc)
    }

    fn probe_env_guard() -> crate::config::TestEnvGuard {
        let mut names = crate::features::test_feature_env_names();
        names.push("COORDINATOR_HERMES_URL");
        names.push("COORDINATOR_HERMES_SECRET");
        crate::config::TestEnvGuard::capture(&names)
    }

    /// Write a real machine config at `home` (raw JSON = the load path under test).
    fn write_machine_config(
        home: &std::path::Path,
        fleet_summary: bool,
        features: serde_json::Value,
    ) {
        let body = serde_json::json!({
            "version": 1,
            "scan_roots": [],
            "hermes": {
                "enabled": true,
                "webhook_url": "http://127.0.0.1:9/hook",
                "progress": false,
                "fleet_summary": fleet_summary,
            },
            "features": features,
        });
        std::fs::write(home.join("config.json"), body.to_string()).unwrap();
    }

    fn assert_fleet_gate(enabled: bool, source: crate::features::FeatureSource) {
        let snapshot = crate::features::resolve_all();
        let view = &snapshot.duties[Duty::FleetNotify.as_str()];
        assert_eq!(view.enabled, enabled, "fleet_notify gate");
        assert_eq!(view.source, source, "fleet_notify source");
    }

    #[test]
    fn fleet_off_beats_machine_flag_and_tick_posts_nothing() {
        let _lock = crate::config::test_env_lock();
        let _guard = fleet_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(ENV_COORDINATOR_NOTIFY_FLEET, "off");
            std::env::remove_var("COORDINATOR_HERMES");
        }
        write_machine_config(home.path(), true, serde_json::json!({}));
        assert!(!fleet_summary_enabled());
        let sink = hermes::install_recording("http://127.0.0.1:9/hook", "s3cret-fleet");
        let mut last = None;
        fleet_tick_at(&mut last, Utc::now(), 12);
        assert!(last.is_none());
        assert!(sink.take().is_empty());
    }

    #[test]
    fn fleet_tick_due_posts_one_header_then_waits() {
        let _lock = crate::config::test_env_lock();
        let _guard = fleet_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(ENV_COORDINATOR_NOTIFY_FLEET, "1");
            std::env::remove_var("COORDINATOR_HERMES");
        }
        assert!(fleet_summary_enabled());
        let sink = hermes::install_recording("http://127.0.0.1:9/hook", "s3cret-fleet");
        let now = local_noon_utc();
        let mut last = None;
        fleet_tick(&mut last, now);
        assert_eq!(last, Some(now));
        let posts = sink.take();
        assert_eq!(posts.len(), 1);
        assert_eq!(
            hdr(&posts[0], "X-Coordinator-Event"),
            EVENT_TYPE_FLEET_SUMMARY
        );
        let body: serde_json::Value = serde_json::from_slice(&posts[0].body).unwrap();
        assert_eq!(body["event_type"], "fleet_summary");
        assert!(body["projects"].as_array().unwrap().is_empty());
        assert_eq!(body["message"], "fleet 0 projects");
        let text = String::from_utf8(posts[0].body.clone()).unwrap();
        assert!(!text.contains("s3cret-fleet"));
        fleet_tick(&mut last, now);
        assert!(sink.take().is_empty());
    }

    #[test]
    fn fleet_tick_honours_live_feature_edit_on_next_tick() {
        use crate::features::FeatureSource;
        let _lock = crate::config::test_env_lock();
        let _guard = fleet_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
            std::env::remove_var(ENV_COORDINATOR_NOTIFY_FLEET);
            std::env::remove_var("COORDINATOR_HERMES");
        }
        let t0 = Utc::now();
        let later = t0 + chrono::Duration::hours(2);
        write_machine_config(home.path(), true, serde_json::json!({"fleet_notify": true}));
        let sink = hermes::install_recording("http://127.0.0.1:9/hook", "s3cret-fleet");
        let mut last = None;

        fleet_tick_at(&mut last, t0, 12);
        assert_eq!(sink.take().len(), 1, "tick N posts while the gate is on");
        assert_eq!(last, Some(t0));
        assert_fleet_gate(true, FeatureSource::Config);

        write_machine_config(
            home.path(),
            true,
            serde_json::json!({"fleet_notify": false}),
        );
        fleet_tick_at(&mut last, later, 12);
        assert!(sink.take().is_empty(), "tick N+1 honours the config edit");
        assert_eq!(last, Some(t0), "a gated-off tick does not stamp last_sent");
        assert_fleet_gate(false, FeatureSource::Config);

        write_machine_config(home.path(), true, serde_json::json!({"fleet_notify": true}));
        fleet_tick_at(&mut last, later, 12);
        assert_eq!(sink.take().len(), 1, "tick N+2 posts again");
        assert_fleet_gate(true, FeatureSource::Config);
    }

    #[test]
    fn fleet_feature_off_is_a_noop_even_when_hermes_flag_on() {
        let _lock = crate::config::test_env_lock();
        let _guard = fleet_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
            std::env::remove_var(ENV_COORDINATOR_NOTIFY_FLEET);
            std::env::remove_var("COORDINATOR_HERMES");
        }
        write_machine_config(
            home.path(),
            true,
            serde_json::json!({"fleet_notify": false}),
        );
        assert!(!fleet_summary_enabled());
        let sink = hermes::install_recording("http://127.0.0.1:9/hook", "s3cret-fleet");
        let mut last = None;
        fleet_tick_at(&mut last, Utc::now(), 12);
        assert!(last.is_none());
        assert!(sink.take().is_empty());
        assert!(
            toast::take_recorded_toasts().is_empty(),
            "no toast side effect"
        );
    }

    #[test]
    fn legacy_env_on_cannot_bypass_feature_gate() {
        let _lock = crate::config::test_env_lock();
        let _guard = fleet_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(ENV_COORDINATOR_NOTIFY_FLEET, "1");
            std::env::remove_var("COORDINATOR_HERMES");
        }
        write_machine_config(
            home.path(),
            true,
            serde_json::json!({"fleet_notify": false}),
        );
        assert!(
            !fleet_summary_enabled(),
            "the gate is a veto above the legacy env-on arm"
        );
        let sink = hermes::install_recording("http://127.0.0.1:9/hook", "s3cret-fleet");
        let mut last = None;
        fleet_tick_at(&mut last, Utc::now(), 12);
        assert!(last.is_none());
        assert!(sink.take().is_empty());
    }

    #[test]
    fn malformed_features_table_keeps_fleet_dark_and_logs_one_line() {
        let _lock = crate::config::test_env_lock();
        let _guard = fleet_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
            std::env::remove_var(ENV_COORDINATOR_NOTIFY_FLEET);
            std::env::remove_var("COORDINATOR_HERMES");
        }
        write_machine_config(
            home.path(),
            true,
            serde_json::json!({"fleet_notify": "yes"}),
        );
        let sink = hermes::install_recording("http://127.0.0.1:9/hook", "s3cret-fleet");
        let now = Utc::now();
        let mut last = None;
        fleet_tick_at(&mut last, now, 12);
        fleet_tick_at(&mut last, now + chrono::Duration::hours(2), 12);
        assert!(last.is_none());
        assert!(sink.take().is_empty());
        assert_eq!(
            crate::features::take_feature_warnings().len(),
            1,
            "two ticks, one line"
        );
    }

    #[test]
    fn manual_fleet_summary_probe_is_not_gated() {
        let _lock = crate::config::test_env_lock();
        let _guard = probe_env_guard();
        crate::features::reset_feature_warnings_for_tests();
        let home = tempdir().unwrap();
        // A bound-then-dropped loopback port so the connect is refused at once.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
            std::env::remove_var("COORDINATOR_HERMES");
            std::env::set_var(
                "COORDINATOR_HERMES_URL",
                format!("http://127.0.0.1:{port}/hook"),
            );
            std::env::set_var("COORDINATOR_HERMES_SECRET", "dummy-secret");
        }
        write_machine_config(
            home.path(),
            true,
            serde_json::json!({"fleet_notify": false}),
        );
        assert!(!fleet_summary_enabled(), "gate off");
        let event = build_fleet_summary_event(Utc::now()).unwrap();
        match hermes::probe_fleet(&event) {
            hermes::ProbeOutcome::Failed(_) => {}
            other => panic!("manual probe must attempt the POST, got {other:?}"),
        }
    }

    #[test]
    fn fleet_summary_json_has_no_credential_keys() {
        let dir = tempdir().unwrap();
        let record = rec(dir.path());
        let mut state = crate::state::RunState::idle(&record.id);
        state.run_epoch = 4;
        state.phase = "implement".into();
        state.failure_class = Some(FailureClass::Timeout);
        let mut view = crate::state::StatusView::from_record(&record, &state);
        view.stall = Some(crate::state::StallView {
            since: Utc::now(),
            idle_secs: 9,
        });
        view.harness = Some(crate::harness::HarnessStatusBundle {
            grok: Some(crate::harness::GrokHarnessStatus {
                alive: true,
                session_id: Some("sess-1".into()),
                cwd: Some(dir.path().to_path_buf()),
                supports_compact: true,
                pid: Some(1),
                adapter: "grok".into(),
                context_usage: None,
                last_signal: None,
                last_tool_title: None,
                model: None,
                bin: None,
            }),
        });
        view.ci = Some(crate::state::CiStatusView {
            pr: Some(1),
            pr_url: Some("https://example.test/pr/1".into()),
            head_sha: Some("abc".into()),
            last_summary: None,
            interval_ms: 1000,
            auto_merge: false,
            merge: None,
        });
        let event = FleetSummaryEvent {
            event_type: EVENT_TYPE_FLEET_SUMMARY.into(),
            written_at: Utc::now(),
            projects: vec![view],
            message: "fleet 1 project".into(),
        };
        let value = serde_json::to_value(&event).unwrap();
        forbid_secret_keys(&value);
        assert_eq!(value["message"], "fleet 1 project");
        assert_eq!(value["projects"][0]["run_epoch"], 4);
        assert_eq!(value["projects"][0]["phase"], "implement");
        assert_eq!(value["projects"][0]["failure_class"], "timeout");
        assert_eq!(
            value["projects"][0]["harness"]["grok"]["session_id"],
            "sess-1"
        );
        assert!(value["projects"][0]["stall"]["idle_secs"].is_number());
        assert_eq!(
            value["projects"][0]["ci"]["pr_url"],
            "https://example.test/pr/1"
        );
    }

    #[test]
    fn blocked_failure_notifies_once_without_success_or_progress_and_dedupes() {
        use crate::config::test_env_lock;
        let _guard = test_env_lock();
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_NOTIFY);
            std::env::remove_var(ENV_COORDINATOR_NOTIFY_PROGRESS);
        }
        toast::clear_recorded_toasts();
        let dir = tempdir().unwrap();
        let r = rec(dir.path());
        run_stub(&r, Some("0098".into())).unwrap();
        let outcome = PhaseOutcome::failure(
            STUB_PHASE_ACTIVE,
            FailureClass::Blocked,
            OutcomeSource::Test,
            Some("prerequisite".into()),
            None,
        );
        let view = crate::outcome::apply(&r, outcome.clone()).unwrap();
        assert_eq!(view.failure_class, Some(FailureClass::Blocked));
        let toasts = toast::take_recorded_toasts();
        assert_eq!(toasts.len(), 1, "exactly one captured failure notification");
        assert_eq!(toasts[0].failure_class, FailureClass::Blocked);

        // Identical re-apply is idempotent: no second toast or progress event.
        crate::outcome::apply(&r, outcome).unwrap();
        assert!(toast::take_recorded_toasts().is_empty(), "dedupe");
        assert!(
            !std::fs::read_to_string(crate::progress_log::path(&r))
                .unwrap_or_default()
                .contains("kind=progress")
        );
    }
}
