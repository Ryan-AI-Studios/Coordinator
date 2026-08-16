//! Status Surface view-model (no dioxus). Maps `StatusView` → mock `data-state`s + actions.

use std::path::{Path, PathBuf};

use crate::api;
use crate::error::{CoordinatorError, Result};
use crate::layout::LayoutProfile;
use crate::registry::{ProjectAddOptions, ProjectRecord};
use crate::state::{RunStatus, STOP_LAST_EVENT, StatusView, TickerView};
use crate::workflow::graph::{
    PHASE_CI_WAIT, PHASE_PLAN_REVIEW, REVIEW_SLUG_AGY, REVIEW_SLUG_OPENCODE, canonical_phases,
    is_canonical, is_stub_phase,
};

/// Mock `article[data-state]` values (0003 visual contract).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardState {
    ParallelPlanReview,
    TokenIdleCi,
    Running,
    Paused,
    HardFailure,
    Idle,
}

impl CardState {
    pub fn as_data_state(self) -> &'static str {
        match self {
            Self::ParallelPlanReview => "parallel-plan-review",
            Self::TokenIdleCi => "token-idle-ci",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::HardFailure => "hard-failure",
            Self::Idle => "idle",
        }
    }

    pub fn needs_attention(self) -> bool {
        matches!(self, Self::Paused | Self::HardFailure)
    }
}

/// Header fleet counts (derived here — no fleet RPC).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeaderCounts {
    pub projects: usize,
    pub active: usize,
    pub attention: usize,
    pub idle: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipKind {
    Done,
    Current,
    Next,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseChip {
    pub label: String,
    pub kind: ChipKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub role: String,
    pub harness: String,
    pub state: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailurePanel {
    pub path: PathBuf,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCard {
    pub view: StatusView,
    pub card_state: CardState,
    pub sessions: Vec<SessionRow>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FleetSnapshot {
    pub cards: Vec<ProjectCard>,
    pub selected: Option<String>,
    pub counts: HeaderCounts,
    pub phase_chips: Vec<PhaseChip>,
    pub phase_caption: String,
    pub failure: Option<FailurePanel>,
    pub last_error: Option<String>,
    /// Shared machine ticker from `status_all` (same object on every view).
    pub ticker: Option<TickerView>,
}

/// Map live status → mock card `data-state` (spec §2 table).
pub fn card_state(view: &StatusView) -> CardState {
    match view.status {
        RunStatus::Running if view.phase == PHASE_PLAN_REVIEW => CardState::ParallelPlanReview,
        RunStatus::Running if view.phase == PHASE_CI_WAIT => CardState::TokenIdleCi,
        RunStatus::Running => CardState::Running,
        RunStatus::Paused => CardState::Paused,
        RunStatus::Stopped | RunStatus::Idle
            if view.failure_class.is_some() || view.failure_artifact.is_some() =>
        {
            CardState::HardFailure
        }
        RunStatus::Idle | RunStatus::Stopped => CardState::Idle,
    }
}

pub fn header_counts(views: &[StatusView]) -> HeaderCounts {
    let mut counts = HeaderCounts {
        projects: views.len(),
        ..HeaderCounts::default()
    };
    for v in views {
        match card_state(v) {
            CardState::ParallelPlanReview | CardState::TokenIdleCi | CardState::Running => {
                counts.active += 1;
            }
            CardState::Paused | CardState::HardFailure => counts.attention += 1,
            CardState::Idle => counts.idle += 1,
        }
    }
    counts
}

/// Phase-strip chips for the selected project.
///
/// Canonical DAG uses `canonical_phases()`. Stub / unknown leftover phases are a
/// single current chip — do not invent fake DAG progress.
pub fn phase_chips(view: &StatusView) -> Vec<PhaseChip> {
    if !is_canonical(&view.phase) {
        return vec![PhaseChip {
            label: view.phase.clone(),
            kind: ChipKind::Current,
        }];
    }
    let current = view.phase.as_str();
    let idx = canonical_phases()
        .iter()
        .position(|p| *p == current)
        .unwrap_or(0);
    canonical_phases()
        .iter()
        .enumerate()
        .map(|(i, phase)| {
            let mut label = (*phase).to_string();
            if *phase == PHASE_PLAN_REVIEW && current == PHASE_PLAN_REVIEW {
                let n = view
                    .workflow
                    .as_ref()
                    .map(|w| w.pending_roles.len())
                    .unwrap_or(0);
                if n > 0 {
                    label = format!("{phase} ×{n}");
                }
            }
            let kind = if i < idx {
                ChipKind::Done
            } else if i == idx {
                ChipKind::Current
            } else {
                ChipKind::Next
            };
            PhaseChip { label, kind }
        })
        .collect()
}

pub fn phase_caption(view: &StatusView) -> String {
    let name = view
        .display_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&view.project_id);
    if is_stub_phase(&view.phase) || !is_canonical(&view.phase) {
        format!("Selected · {name} · leftover phase (not a live DAG)")
    } else {
        format!("Selected · {name} · canonical pipeline")
    }
}

/// Session table: Grok from `harness.grok`; plan-review pending roles as agy/opencode.
/// Do not invent live ACP rows for Claude/Codex/OpenCode.
pub fn session_rows(view: &StatusView) -> Vec<SessionRow> {
    let mut rows = Vec::new();
    match view.harness.as_ref().and_then(|h| h.grok.as_ref()) {
        Some(g) => {
            let mut bits = Vec::new();
            if let Some(pid) = g.pid {
                bits.push(format!("pid {pid}"));
            }
            if let Some(cwd) = &g.cwd {
                bits.push(cwd.display().to_string());
            }
            bits.push(if g.supports_compact {
                "compact: yes".into()
            } else {
                "compact: no".into()
            });
            rows.push(SessionRow {
                role: "Grok".into(),
                harness: "grok".into(),
                state: if g.alive {
                    "alive".into()
                } else {
                    "dead".into()
                },
                detail: bits.join(" · "),
            });
        }
        None => rows.push(SessionRow {
            role: "Grok".into(),
            harness: "grok".into(),
            state: "no grok session".into(),
            detail: String::new(),
        }),
    }

    if view.phase == PHASE_PLAN_REVIEW {
        let pending = view
            .workflow
            .as_ref()
            .map(|w| w.pending_roles.as_slice())
            .unwrap_or(&[]);
        for slug in [REVIEW_SLUG_AGY, REVIEW_SLUG_OPENCODE] {
            let active = pending.iter().any(|r| r == slug);
            rows.push(SessionRow {
                role: format!("Plan review ({slug})"),
                harness: slug.to_string(),
                state: if active {
                    "active".into()
                } else {
                    "done".into()
                },
                detail: String::new(),
            });
        }
    }

    rows
}

pub fn failure_panel(view: &StatusView) -> Option<FailurePanel> {
    if let Ok(Some(shown)) = api::cmd_failure_show(Some(&view.project_id)) {
        return Some(FailurePanel {
            path: shown.path,
            body: shown.body,
        });
    }
    let path = view.failure_artifact.clone()?;
    let body = std::fs::read_to_string(&path).unwrap_or_default();
    Some(FailurePanel { path, body })
}

pub fn build_fleet(views: Vec<StatusView>, selected: Option<&str>) -> FleetSnapshot {
    let counts = header_counts(&views);
    let selected = match selected {
        Some(id) if views.iter().any(|v| v.project_id == id) => Some(id.to_string()),
        _ => views.first().map(|v| v.project_id.clone()),
    };
    let selected_view = selected
        .as_ref()
        .and_then(|id| views.iter().find(|v| v.project_id == *id));
    let phase_chips = selected_view.map(phase_chips).unwrap_or_default();
    let phase_caption = selected_view
        .map(phase_caption)
        .unwrap_or_else(|| "No project selected".into());
    let failure = selected_view.and_then(failure_panel);
    let ticker = views.first().and_then(|v| v.ticker.clone());
    let cards = views
        .into_iter()
        .map(|view| {
            let card_state = card_state(&view);
            let sessions = session_rows(&view);
            ProjectCard {
                view,
                card_state,
                sessions,
            }
        })
        .collect();
    FleetSnapshot {
        cards,
        selected,
        counts,
        phase_chips,
        phase_caption,
        failure,
        last_error: None,
        ticker,
    }
}

/// Header `.stat` copy: `Ticker serve :N` or `Ticker none`.
pub fn ticker_label(ticker: Option<&TickerView>) -> String {
    match ticker {
        Some(t) if t.owner == "serve" => match t.port {
            Some(p) => format!("Ticker serve :{p}"),
            None => "Ticker serve".into(),
        },
        _ => "Ticker none".into(),
    }
}

/// Poll Control Plane (same functions as CLI). Never a WebView `fetch`.
pub fn load_fleet(selected: Option<&str>) -> Result<FleetSnapshot> {
    let views = api::status_all()?;
    Ok(build_fleet(views, selected))
}

/// Pause every Running project. `InvalidTransition` is skip, not a hard UI error.
pub fn pause_all() -> Result<Vec<StatusView>> {
    pause_listed(api::status_all()?, |id| api::cmd_pause(Some(id)))
}

/// Testable pause-all core (no process-wide registry).
pub fn pause_listed(
    views: impl IntoIterator<Item = StatusView>,
    mut pause: impl FnMut(&str) -> Result<StatusView>,
) -> Result<Vec<StatusView>> {
    let mut out = Vec::new();
    for view in views {
        if view.status != RunStatus::Running {
            continue;
        }
        match pause(&view.project_id) {
            Ok(next) => out.push(next),
            Err(CoordinatorError::InvalidTransition { .. }) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// Stop selected. Does not call harness shutdown and does not write `FAILURE.md`.
pub fn stop_selected(project_id: &str) -> Result<StatusView> {
    api::cmd_stop(Some(project_id))
}

pub fn resume_selected(project_id: &str) -> Result<StatusView> {
    api::cmd_resume(Some(project_id))
}

/// Explicit absolute path only. Never `project scan --add`.
pub fn add_project(path: &Path, display_name: Option<String>) -> Result<ProjectRecord> {
    if !path.is_absolute() {
        return Err(CoordinatorError::Message(
            "add project requires an absolute path".into(),
        ));
    }
    api::project_add(
        path,
        ProjectAddOptions {
            layout_profile: LayoutProfile::Nested,
            display_name,
            ..ProjectAddOptions::default()
        },
    )
}

/// Convenience run. CLI remains the automation entry. Idle/Stopped only.
pub fn run_allowed(status: RunStatus) -> bool {
    matches!(status, RunStatus::Idle | RunStatus::Stopped)
}

pub fn run_selected(project_id: &str, track: Option<String>) -> Result<StatusView> {
    let view = api::status(Some(project_id))?;
    if !run_allowed(view.status) {
        return Err(CoordinatorError::InvalidTransition {
            action: "run",
            from: view.status.to_string(),
        });
    }
    api::cmd_run(Some(project_id), track, None)
}

pub fn show_failure(project_id: &str) -> Result<Option<crate::notify::FailureShow>> {
    api::cmd_failure_show(Some(project_id))
}

/// Display title for a card (display_name, else last path component, else id).
pub fn card_title(view: &StatusView) -> String {
    if let Some(name) = view
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return name.to_string();
    }
    view.path
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| view.project_id.clone())
}

pub fn stop_copy() -> &'static str {
    STOP_LAST_EVENT
}

/// Primary card button. Idle/Stopped (including hard-fail) → Run, never Pause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardPrimaryAction {
    Pause,
    Resume,
    Run,
}

pub fn card_primary_action(view: &StatusView) -> CardPrimaryAction {
    match view.status {
        RunStatus::Paused => CardPrimaryAction::Resume,
        RunStatus::Idle | RunStatus::Stopped => CardPrimaryAction::Run,
        RunStatus::Running => CardPrimaryAction::Pause,
    }
}

pub fn selected_is_paused(snap: &FleetSnapshot) -> bool {
    snap.cards
        .iter()
        .find(|c| Some(&c.view.project_id) == snap.selected.as_ref())
        .is_some_and(|c| c.view.status == RunStatus::Paused)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{GrokHarnessStatus, HarnessStatusBundle};
    use crate::notify::artifact;
    use crate::outcome::FailureClass;
    use crate::run;
    use crate::state::{STUB_PHASE_IDLE, WorkflowView};
    use crate::workflow::WorkflowDriver;
    use std::path::PathBuf;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn view(
        status: RunStatus,
        phase: &str,
        failure_class: Option<FailureClass>,
        failure_artifact: Option<PathBuf>,
    ) -> StatusView {
        StatusView {
            project_id: "p1".into(),
            path: PathBuf::from(r"C:\dev\demo"),
            display_name: Some("Demo".into()),
            status,
            phase: phase.into(),
            track_id: Some("0014".into()),
            last_event: "test".into(),
            run_epoch: 1,
            phase_started_at: None,
            failure_class,
            next_track: None,
            layout_profile: LayoutProfile::Nested,
            execution_repo: None,
            conductor_dir: None,
            harness: None,
            workflow: None,
            failure_artifact,
            ci: None,
            review: None,
            last_progress_at: None,
            stall: None,
            ticker: None,
        }
    }

    #[test]
    fn card_state_mapping_table() {
        assert_eq!(
            card_state(&view(RunStatus::Running, PHASE_PLAN_REVIEW, None, None)).as_data_state(),
            "parallel-plan-review"
        );
        assert_eq!(
            card_state(&view(RunStatus::Running, PHASE_CI_WAIT, None, None)).as_data_state(),
            "token-idle-ci"
        );
        assert_eq!(
            card_state(&view(RunStatus::Running, "plan", None, None)).as_data_state(),
            "running"
        );
        assert_eq!(
            card_state(&view(RunStatus::Running, "implement", None, None)).as_data_state(),
            "running"
        );
        assert_eq!(
            card_state(&view(RunStatus::Paused, "implement", None, None)).as_data_state(),
            "paused"
        );
        assert_eq!(
            card_state(&view(
                RunStatus::Stopped,
                "stub:stopped",
                Some(FailureClass::Timeout),
                None
            ))
            .as_data_state(),
            "hard-failure"
        );
        assert_eq!(
            card_state(&view(
                RunStatus::Idle,
                STUB_PHASE_IDLE,
                None,
                Some(PathBuf::from(r"C:\tmp\FAILURE.md"))
            ))
            .as_data_state(),
            "hard-failure"
        );
        assert_eq!(
            card_state(&view(RunStatus::Idle, STUB_PHASE_IDLE, None, None)).as_data_state(),
            "idle"
        );
        assert_eq!(
            card_state(&view(RunStatus::Stopped, "stub:stopped", None, None)).as_data_state(),
            "idle"
        );
    }

    #[test]
    fn header_counts_and_empty_registry() {
        let empty = build_fleet(vec![], None);
        assert!(empty.cards.is_empty());
        assert_eq!(empty.counts, HeaderCounts::default());
        assert!(empty.phase_chips.is_empty());
        assert!(empty.failure.is_none());

        let mixed = vec![
            view(RunStatus::Running, "plan", None, None),
            {
                let mut v = view(RunStatus::Paused, "implement", None, None);
                v.project_id = "p2".into();
                v
            },
            {
                let mut v = view(RunStatus::Idle, STUB_PHASE_IDLE, None, None);
                v.project_id = "p3".into();
                v
            },
            {
                let mut v = view(
                    RunStatus::Stopped,
                    "stub:failed",
                    Some(FailureClass::CiFailed),
                    None,
                );
                v.project_id = "p4".into();
                v
            },
        ];
        let counts = header_counts(&mixed);
        assert_eq!(counts.projects, 4);
        assert_eq!(counts.active, 1);
        assert_eq!(counts.attention, 2);
        assert_eq!(counts.idle, 1);
    }

    #[test]
    fn stub_phase_strip_is_single_current_chip() {
        let chips = phase_chips(&view(RunStatus::Idle, STUB_PHASE_IDLE, None, None));
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].label, STUB_PHASE_IDLE);
        assert_eq!(chips[0].kind, ChipKind::Current);
        assert!(
            !phase_caption(&view(RunStatus::Idle, STUB_PHASE_IDLE, None, None))
                .contains("canonical")
        );
    }

    #[test]
    fn canonical_phase_strip_marks_done_current_next() {
        let chips = phase_chips(&view(RunStatus::Running, PHASE_PLAN_REVIEW, None, None));
        assert_eq!(chips.len(), canonical_phases().len());
        assert_eq!(chips[0].kind, ChipKind::Done);
        assert_eq!(chips[1].kind, ChipKind::Current);
        assert_eq!(chips[2].kind, ChipKind::Next);
        assert!(chips[1].label.starts_with(PHASE_PLAN_REVIEW));
    }

    #[test]
    fn missing_grok_is_no_session_row() {
        let rows = session_rows(&view(RunStatus::Idle, STUB_PHASE_IDLE, None, None));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].harness, "grok");
        assert_eq!(rows[0].state, "no grok session");
    }

    #[test]
    fn plan_review_pending_roles_are_agy_opencode() {
        let mut v = view(RunStatus::Running, PHASE_PLAN_REVIEW, None, None);
        v.workflow = Some(WorkflowView {
            id: Some("canonical_v1".into()),
            driver: "file_wait".into(),
            pending_roles: vec![REVIEW_SLUG_AGY.into()],
        });
        v.harness = Some(HarnessStatusBundle {
            grok: Some(GrokHarnessStatus {
                alive: true,
                session_id: Some("s".into()),
                cwd: Some(PathBuf::from(r"C:\dev\demo")),
                supports_compact: true,
                pid: Some(42),
            }),
        });
        let rows = session_rows(&v);
        assert_eq!(rows[0].state, "alive");
        assert!(rows[0].detail.contains("pid 42"));
        assert!(rows[0].detail.contains("compact: yes"));
        let agy = rows.iter().find(|r| r.harness == REVIEW_SLUG_AGY).unwrap();
        let oc = rows
            .iter()
            .find(|r| r.harness == REVIEW_SLUG_OPENCODE)
            .unwrap();
        assert_eq!(agy.state, "active");
        assert_eq!(oc.state, "done");
        assert!(
            !rows
                .iter()
                .any(|r| r.harness == "claude" || r.harness == "codex"),
            "must not invent live ACP rows"
        );
    }

    #[test]
    fn failure_panel_absent_when_no_artifact() {
        let snap = build_fleet(
            vec![view(RunStatus::Idle, STUB_PHASE_IDLE, None, None)],
            None,
        );
        assert!(snap.failure.is_none());
    }

    #[test]
    fn failure_panel_reads_body_when_present() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("FAILURE.md");
        std::fs::write(&path, "quota exhausted\n").unwrap();
        let snap = build_fleet(
            vec![view(
                RunStatus::Stopped,
                "stub:failed",
                Some(FailureClass::ModelExhaustion),
                Some(path.clone()),
            )],
            None,
        );
        let panel = snap.failure.expect("panel");
        assert_eq!(panel.path, path);
        assert!(panel.body.contains("quota exhausted"));
        assert_eq!(snap.cards[0].card_state, CardState::HardFailure);
    }

    #[test]
    fn add_project_rejects_relative_path() {
        let err = add_project(Path::new("relative\\proj"), None).unwrap_err();
        assert!(err.to_string().contains("absolute"));
    }

    #[test]
    fn pause_listed_skips_invalid_transition() {
        let running = view(RunStatus::Running, "plan", None, None);
        let mut other = running.clone();
        other.project_id = "p2".into();
        let mut idle = view(RunStatus::Idle, STUB_PHASE_IDLE, None, None);
        idle.project_id = "idle".into();
        let mut paused = running.clone();
        paused.status = RunStatus::Paused;
        let mut calls = Vec::new();
        let out = pause_listed(vec![running, other, idle], |id| {
            calls.push(id.to_string());
            if id == "p2" {
                Err(CoordinatorError::InvalidTransition {
                    action: "pause",
                    from: "Running".into(),
                })
            } else {
                Ok(paused.clone())
            }
        })
        .unwrap();
        assert_eq!(calls, vec!["p1", "p2"]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].status, RunStatus::Paused);
    }

    #[test]
    fn stop_leaves_attach_copy_and_no_artifact() {
        let dir = tempdir().unwrap();
        let rec = crate::registry::ProjectRecord {
            id: Uuid::new_v4().to_string(),
            path: dir.path().to_path_buf(),
            display_name: Some("RunMe".into()),
            layout_profile: LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: std::collections::BTreeMap::new(),
            state_dir: None,
            auto_merge: true,
            phase_timeouts_secs: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now(),
        };
        run::run_with_driver(&rec, Some("0014".into()), WorkflowDriver::FileWait).unwrap();
        let stopped = run::stop(&rec).unwrap();
        assert_eq!(stopped.status, RunStatus::Stopped);
        assert_eq!(stopped.last_event, STOP_LAST_EVENT);
        assert!(stopped.last_event.contains("sessions left for attach"));
        assert!(stopped.failure_class.is_none());
        assert!(
            artifact::existing_path(&rec).is_none(),
            "Stop must not write FAILURE.md"
        );
        assert_eq!(stop_copy(), STOP_LAST_EVENT);
        assert_eq!(card_title(&stopped), "RunMe");
    }

    #[test]
    fn run_selected_refuses_non_idle_stopped() {
        assert!(run_allowed(RunStatus::Idle));
        assert!(run_allowed(RunStatus::Stopped));
        assert!(!run_allowed(RunStatus::Running));
        assert!(!run_allowed(RunStatus::Paused));
    }

    #[test]
    fn hard_failure_cards_offer_run_not_pause() {
        assert_eq!(
            card_primary_action(&view(
                RunStatus::Stopped,
                "stub:failed",
                Some(FailureClass::Timeout),
                None
            )),
            CardPrimaryAction::Run
        );
        assert_eq!(
            card_primary_action(&view(
                RunStatus::Idle,
                STUB_PHASE_IDLE,
                None,
                Some(PathBuf::from(r"C:\tmp\FAILURE.md"))
            )),
            CardPrimaryAction::Run
        );
        assert_eq!(
            card_primary_action(&view(RunStatus::Running, "plan", None, None)),
            CardPrimaryAction::Pause
        );
        assert_eq!(
            card_primary_action(&view(RunStatus::Paused, "implement", None, None)),
            CardPrimaryAction::Resume
        );
        assert_eq!(
            card_primary_action(&view(RunStatus::Idle, STUB_PHASE_IDLE, None, None)),
            CardPrimaryAction::Run
        );
    }

    #[test]
    fn ticker_label_serve_and_none() {
        assert_eq!(ticker_label(None), "Ticker none");
        let none = TickerView {
            owner: "none".into(),
            port: None,
        };
        assert_eq!(ticker_label(Some(&none)), "Ticker none");
        let serve = TickerView {
            owner: "serve".into(),
            port: Some(7420),
        };
        assert_eq!(ticker_label(Some(&serve)), "Ticker serve :7420");
    }
}
