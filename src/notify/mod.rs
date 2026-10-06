//! Failure notify: artifact + toast + adapter trait (track 0009) + Hermes (0015).
//! Opt-in progress POSTs on phase advance (track 0033) use [`ProgressEvent`],
//! never toast / `FAILURE.md`, and never fail apply.
//! Opt-in fleet summaries (track 0068) use [`FleetSummaryEvent`] on that same
//! Hermes path. `notify fleet-summary` posts once. `serve` posts at most once
//! per hour, and only when the fleet flag is on.
//!
//! Hook failure notify only after a successful Phase Outcome **failure** commit.
//! Operator `stop` is not a Failure Class and must not notify.

pub mod adapter;
pub mod artifact;
pub mod hermes;
pub mod recovery;
pub mod toast;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::outcome::FailureClass;
use crate::registry::ProjectRecord;

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

/// Minimum gap between serve-loop fleet posts.
pub const FLEET_SUMMARY_INTERVAL: chrono::Duration = chrono::Duration::seconds(3600);

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

/// Fleet snapshot posted on the Hermes path (track 0068).
///
/// `projects` is [`crate::state::StatusView`] from `api::status_all`. Not a
/// [`NotifyEvent`] and not a [`ProgressEvent`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FleetSummaryEvent {
    pub event_type: String,
    pub written_at: DateTime<Utc>,
    pub projects: Vec<crate::state::StatusView>,
}

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

/// Env `off` wins; else env on **or** machine `hermes.progress` **or** project flag.
/// Actual POST still requires 0015 Hermes resolve (or a test recording backend).
pub fn progress_enabled(record: &ProjectRecord) -> bool {
    if env_flag_off(ENV_COORDINATOR_NOTIFY_PROGRESS) {
        return false;
    }
    if env_flag_on(ENV_COORDINATOR_NOTIFY_PROGRESS) {
        return true;
    }
    let machine = crate::config::load_machine_config()
        .map(|c| c.hermes.progress)
        .unwrap_or(false);
    machine || record.notify_progress
}

/// Env `off` wins; else env on **or** machine `hermes.fleet_summary`.
/// No per-project flag. The CLI probe does not consult this.
pub fn fleet_summary_enabled() -> bool {
    if env_flag_off(ENV_COORDINATOR_NOTIFY_FLEET) {
        return false;
    }
    if env_flag_on(ENV_COORDINATOR_NOTIFY_FLEET) {
        return true;
    }
    crate::config::load_machine_config()
        .map(|c| c.hermes.fleet_summary)
        .unwrap_or(false)
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

/// One fleet snapshot. `written_at` is the caller's clock, not a second read.
pub fn build_fleet_summary_event(now: DateTime<Utc>) -> crate::error::Result<FleetSummaryEvent> {
    Ok(FleetSummaryEvent {
        event_type: EVENT_TYPE_FLEET_SUMMARY.to_string(),
        written_at: now,
        projects: crate::api::status_all()?,
    })
}

/// One serve-loop chance to post. Sets `last_sent` when the post is due, including
/// when build or POST fails, so a failure cannot retry on every poll tick.
/// Never fails the caller.
pub fn fleet_tick(last_sent: &mut Option<DateTime<Utc>>, now: DateTime<Utc>) {
    if !fleet_summary_due(*last_sent, now, fleet_summary_enabled()) {
        return;
    }
    *last_sent = Some(now);
    let event = match build_fleet_summary_event(now) {
        Ok(event) => event,
        Err(e) => {
            eprintln!("coordinator: fleet summary failed (non-fatal): {e}");
            return;
        }
    };
    if let Err(e) = hermes::HermesAdapter::for_default_stack().notify_fleet(&event) {
        eprintln!("coordinator: fleet summary failed (non-fatal): {e}");
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
    fn fleet_off_beats_machine_flag_and_tick_posts_nothing() {
        use crate::config::{
            ENV_COORDINATOR_HOME, HermesNotifyConfig, MACHINE_CONFIG_VERSION, MachineConfig,
            default_role_bindings, save_machine_config, test_env_lock,
        };
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(ENV_COORDINATOR_NOTIFY_FLEET, "off");
        }
        let cfg = MachineConfig {
            version: MACHINE_CONFIG_VERSION,
            scan_roots: Vec::new(),
            role_bindings: default_role_bindings(),
            phase_timeouts_secs: std::collections::BTreeMap::new(),
            hermes: HermesNotifyConfig {
                enabled: true,
                webhook_url: Some("http://127.0.0.1:9/hook".into()),
                progress: false,
                fleet_summary: true,
            },
            progress_stall_secs: None,
            journal_keep: None,
            state_policies: Vec::new(),
        };
        save_machine_config(&cfg).unwrap();
        assert!(!fleet_summary_enabled());
        let sink = hermes::install_recording("http://127.0.0.1:9/hook", "s3cret-fleet");
        let mut last = None;
        fleet_tick(&mut last, Utc::now());
        assert!(last.is_none());
        assert!(sink.take().is_empty());
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_NOTIFY_FLEET);
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[test]
    fn fleet_tick_due_posts_one_header_then_waits() {
        use crate::config::{ENV_COORDINATOR_HOME, test_env_lock};
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(ENV_COORDINATOR_NOTIFY_FLEET, "1");
            std::env::remove_var("COORDINATOR_HERMES");
        }
        assert!(fleet_summary_enabled());
        let sink = hermes::install_recording("http://127.0.0.1:9/hook", "s3cret-fleet");
        let now = Utc::now();
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
        let text = String::from_utf8(posts[0].body.clone()).unwrap();
        assert!(!text.contains("s3cret-fleet"));
        fleet_tick(&mut last, now);
        assert!(sink.take().is_empty());
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_NOTIFY_FLEET);
            std::env::remove_var(ENV_COORDINATOR_HOME);
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
        };
        let value = serde_json::to_value(&event).unwrap();
        forbid_secret_keys(&value);
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
}
