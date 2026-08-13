//! Notify Adapter trait + composite + log / Hermes / recording.

use crate::error::Result;

use super::NotifyEvent;

/// Fan-out slot for Failure Artifact, toast, log, and Hermes.
pub trait NotifyAdapter: Send + Sync {
    fn notify(&self, event: &NotifyEvent) -> Result<()>;
}

/// Writes `{state_dir}/FAILURE.md` (durable signal).
pub struct ArtifactAdapter;

impl NotifyAdapter for ArtifactAdapter {
    fn notify(&self, event: &NotifyEvent) -> Result<()> {
        super::artifact::write_event(event)?;
        Ok(())
    }
}

/// One stderr line (useful in `serve`).
pub struct LogAdapter;

impl NotifyAdapter for LogAdapter {
    fn notify(&self, event: &NotifyEvent) -> Result<()> {
        eprintln!(
            "coordinator: failure class={} project={} phase={} artifact={}",
            event.failure_class,
            event.project_id,
            event.phase,
            event.artifact_path.display()
        );
        Ok(())
    }
}

pub use super::hermes::HermesAdapter;

/// Isolated fan-out. Artifact is first; later adapter errors do not undo it.
pub struct Composite {
    adapters: Vec<Box<dyn NotifyAdapter>>,
}

impl Composite {
    pub fn new(adapters: Vec<Box<dyn NotifyAdapter>>) -> Self {
        Self { adapters }
    }

    /// Artifact + Toast + Log + Hermes. Tests never read machine-home Hermes config.
    pub fn default_stack() -> Self {
        Self::new(vec![
            Box::new(ArtifactAdapter),
            Box::new(super::toast::ToastAdapter),
            Box::new(LogAdapter),
            Box::new(HermesAdapter::for_default_stack()),
        ])
    }
}

impl NotifyAdapter for Composite {
    fn notify(&self, event: &NotifyEvent) -> Result<()> {
        for adapter in &self.adapters {
            if let Err(e) = adapter.notify(event) {
                eprintln!(
                    "coordinator: notify adapter error (non-fatal): {e} (class={})",
                    event.failure_class
                );
            }
        }
        Ok(())
    }
}

/// Test sink that records every event.
#[derive(Default)]
pub struct RecordingAdapter {
    pub events: std::sync::Mutex<Vec<NotifyEvent>>,
}

impl RecordingAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn take(&self) -> Vec<NotifyEvent> {
        self.events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain(..)
            .collect()
    }
}

impl NotifyAdapter for RecordingAdapter {
    fn notify(&self, event: &NotifyEvent) -> Result<()> {
        self.events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(event.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcome::FailureClass;
    use chrono::Utc;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn event() -> NotifyEvent {
        NotifyEvent {
            project_id: "p".into(),
            track_id: None,
            phase: "plan".into(),
            failure_class: FailureClass::Timeout,
            message: None,
            last_event: "x".into(),
            artifact_path: PathBuf::from("FAILURE.md"),
            written_at: Utc::now(),
            run_epoch: 1,
        }
    }

    #[test]
    fn hermes_disabled_is_noop() {
        HermesAdapter::noop().notify(&event()).unwrap();
    }

    #[test]
    fn recording_adapter_captures() {
        let rec = RecordingAdapter::new();
        rec.notify(&event()).unwrap();
        assert_eq!(rec.take().len(), 1);
        assert!(rec.take().is_empty());
    }

    #[test]
    fn composite_runs_later_adapters_after_error() {
        struct Boom;
        impl NotifyAdapter for Boom {
            fn notify(&self, _event: &NotifyEvent) -> Result<()> {
                Err(crate::error::CoordinatorError::Message("boom".into()))
            }
        }
        struct Flag(Arc<AtomicBool>);
        impl NotifyAdapter for Flag {
            fn notify(&self, _event: &NotifyEvent) -> Result<()> {
                self.0.store(true, Ordering::SeqCst);
                Ok(())
            }
        }
        let seen = Arc::new(AtomicBool::new(false));
        let composite = Composite::new(vec![Box::new(Boom), Box::new(Flag(seen.clone()))]);
        composite.notify(&event()).unwrap();
        assert!(seen.load(Ordering::SeqCst));
    }
}
