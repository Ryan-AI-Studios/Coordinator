//! Failure notify: artifact + toast + adapter trait (track 0009).
//!
//! Hook only after a successful Phase Outcome **failure** commit. Operator
//! `stop` is not a Failure Class and must not notify.

pub mod adapter;
pub mod artifact;
pub mod recovery;
pub mod toast;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::outcome::FailureClass;
use crate::registry::ProjectRecord;

pub use adapter::{
    ArtifactAdapter, Composite, HermesAdapter, LogAdapter, NotifyAdapter, RecordingAdapter,
};
pub use artifact::{FailureShow, clear as clear_artifact};
pub use recovery::recommended_action;
pub use toast::{ENV_COORDINATOR_NOTIFY, ToastAdapter, notify_enabled};

/// Payload fanned out to every [`NotifyAdapter`].
///
/// Hermes v1.x will POST this JSON to a configured local inbound webhook.
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
}
