//! Failure notify: artifact + toast + adapter trait (track 0009) + Hermes (0015).
//! Opt-in progress POSTs on phase advance (track 0033) use [`ProgressEvent`],
//! never toast / `FAILURE.md`, and never fail apply.
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
            ready_aliases: Vec::new(),
            auto_start: Default::default(),
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
            assert!(shown.body.contains("does **not** auto-retry"));
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
        std::fs::create_dir_all(dir.path().join("conductor").join("0040-Next")).unwrap();
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
}
