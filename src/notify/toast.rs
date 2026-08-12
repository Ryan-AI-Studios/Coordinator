//! Windows WinRT toast. Non-fatal; skip when `COORDINATOR_NOTIFY=off` or non-Windows.
//!
//! `tauri_winrt_notification::Toast` is `!Send` + `!Sync`. Construct-show-drop
//! **per call** on a dedicated thread. Never store a `Toast` in a static.

/// Named so `cargo test` still type-checks the crate API (`Toast` itself is
/// only constructed on the non-test Windows toast thread).
#[cfg(windows)]
const _: &str = tauri_winrt_notification::Toast::POWERSHELL_APP_ID;

use crate::error::Result;
use crate::notify::NotifyEvent;

/// Env: `off` disables WinRT toasts (tests / CI / headless). Unset = on (Windows).
pub const ENV_COORDINATOR_NOTIFY: &str = "COORDINATOR_NOTIFY";

/// Max toast body characters (title is separate).
pub const TOAST_BODY_CAP: usize = 120;

pub struct ToastAdapter;

impl super::adapter::NotifyAdapter for ToastAdapter {
    fn notify(&self, event: &NotifyEvent) -> Result<()> {
        show(event)
    }
}

pub fn notify_enabled() -> bool {
    !matches!(
        std::env::var(ENV_COORDINATOR_NOTIFY),
        Ok(s) if s.eq_ignore_ascii_case("off")
    )
}

pub fn toast_title(event: &NotifyEvent) -> String {
    format!("Coordinator: {}", event.failure_class)
}

pub fn toast_body(event: &NotifyEvent) -> String {
    let track = event.track_id.as_deref().unwrap_or("null");
    let mut body = format!("{} · {} · {}", event.project_id, track, event.phase);
    if let Some(ref msg) = event.message {
        let t = truncate(msg, TOAST_BODY_CAP);
        body.push(' ');
        body.push_str(&t);
    }
    truncate(&body, TOAST_BODY_CAP + 80)
}

fn truncate(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s.to_string();
    }
    let t: String = s.chars().take(cap).collect();
    format!("{t}…")
}

/// Show a toast or record (tests). Errors are the caller's to isolate.
pub fn show(event: &NotifyEvent) -> Result<()> {
    if !notify_enabled() {
        return Ok(());
    }
    show_enabled(event)
}

#[cfg(test)]
fn show_enabled(event: &NotifyEvent) -> Result<()> {
    record_toast(event);
    Ok(())
}

#[cfg(all(windows, not(test)))]
fn show_enabled(event: &NotifyEvent) -> Result<()> {
    spawn_winrt_toast(event);
    Ok(())
}

#[cfg(all(not(windows), not(test)))]
fn show_enabled(_event: &NotifyEvent) -> Result<()> {
    Ok(())
}

#[cfg(all(windows, not(test)))]
fn spawn_winrt_toast(event: &NotifyEvent) {
    let title = toast_title(event);
    let body = toast_body(event);
    // Construct Toast on this thread only (`!Send`). Detach — never block serve poll.
    let _ = std::thread::Builder::new()
        .name("coordinator-toast".into())
        .spawn(move || {
            let shown = std::panic::catch_unwind(|| {
                let toast = tauri_winrt_notification::Toast::new(
                    tauri_winrt_notification::Toast::POWERSHELL_APP_ID,
                )
                .title(&title)
                .text1(&body);
                toast.show()
            });
            match shown {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("coordinator: toast failed (non-fatal): {e}"),
                Err(_) => eprintln!("coordinator: toast thread panicked (non-fatal)"),
            }
        });
}

#[cfg(test)]
thread_local! {
    static TOAST_SINK: std::cell::RefCell<Vec<NotifyEvent>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn record_toast(event: &NotifyEvent) {
    TOAST_SINK.with(|s| s.borrow_mut().push(event.clone()));
}

#[cfg(test)]
pub fn take_recorded_toasts() -> Vec<NotifyEvent> {
    TOAST_SINK.with(|s| s.take())
}

#[cfg(test)]
pub fn clear_recorded_toasts() {
    TOAST_SINK.with(|s| s.borrow_mut().clear());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcome::FailureClass;
    use chrono::Utc;
    use std::path::PathBuf;

    fn event() -> NotifyEvent {
        NotifyEvent {
            project_id: "proj".into(),
            track_id: Some("0009".into()),
            phase: "implement".into(),
            failure_class: FailureClass::Timeout,
            message: Some("budget".into()),
            last_event: "x".into(),
            artifact_path: PathBuf::from("FAILURE.md"),
            written_at: Utc::now(),
            run_epoch: 1,
        }
    }

    #[test]
    fn title_includes_class() {
        assert_eq!(toast_title(&event()), "Coordinator: timeout");
    }

    #[test]
    fn body_includes_ids() {
        let b = toast_body(&event());
        assert!(b.contains("proj"));
        assert!(b.contains("0009"));
        assert!(b.contains("implement"));
        assert!(b.contains("budget"));
    }
}
