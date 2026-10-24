//! Read-only ready-pick preview and compatibility report (0115).
//!
//! `coordinator ready` answers "what would the machine start, and why not that
//! row?" without starting, writing, or mutating anything. The `decision` is the
//! **ticker's** ordered gate walk (`watch::try_auto_start`); `operator_omit_pick`
//! is what an operator `run` would choose from the same run state. They are
//! separate fields so a pick id stays visible while the machine gate is off.
//!
//! Read side-effects matter here: [`crate::workflow::conductor_md::load_track_rows`]
//! reports drift/unreadable through `progress_log::append`, so this module takes
//! `read_view_for_parse` plus the one `registry_store::snapshot` directly. It is
//! the command's reason to exist that it writes nothing at all.
//!
//! `rows.by_reason` tests HITL before terminal (the picker's predicate order), so
//! three Completed **and** HITL DeDupe rows count as `hitl` while all of them stay
//! Completed. The report says so rather than reordering the picker's gates.
//!
//! `pick_row_version` compares the report's view read with the picker's own
//! (unversioned) snapshot. A row that changes between them can surface as
//! `would_start` beside an ineligible listed row; `picked_but_ineligible`
//! discloses that window without claiming which view is stale (0109 row 179).

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{CoordinatorError, Result};
use crate::features::{Duty, FeatureSnapshot, FeatureSource};
use crate::registry::{AutoStartPolicy, ProjectRecord};
use crate::state::{ReadyStatusView, RunState, RunStatus, load_run_state};

use super::conductor_md::{
    self, DroppedRow, TrackRow, conductor_md_path, hitl_field, is_blocked, is_eligible_ready_in,
    is_hitl_marked, is_in_progress, is_proposed, is_terminal, parse_conductor_md,
    row_matches_track, row_overlay_eligible, scan_dropped_rows, status_clean,
};
use super::{ReadyPickState, pick_next_ready_excluding_sticky, should_pick_next_ready};

/// `true` on a gate-on `full` project — never the literal policy name (`full`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TickerEffective {
    Start,
    Hitl,
    Never,
    FeatureOff,
}

/// One machine feature gate, projected without the rest of the duty table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureGate {
    pub enabled: bool,
    pub source: FeatureSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistrySource {
    Store,
    View,
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryState {
    Ok,
    Drift,
    Absent,
    Invalid,
}

/// Registry evidence derived from the report's **one** `snapshot`.
///
/// Deliberately not a second `status_view_for` read: one JSON must not say
/// `state: "ok"` while the decision says `registry_drift`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryEvidence {
    pub source: RegistrySource,
    pub state: RegistryState,
    /// The candidate store path, whether or not the file exists.
    pub store_path: PathBuf,
    /// Operator recovery line for `drift` / `invalid`.
    pub recovery: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RowSource {
    View,
    None,
}

/// The view scan: what the parser can see, and what it silently drops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowsReport {
    pub source: RowSource,
    pub data: usize,
    pub parsed: usize,
    pub dropped: usize,
    /// Parsed pick rows whose id has no resolvable conductor directory.
    ///
    /// Additive (0115 DoD-3): the picker only checks a directory for rows that are
    /// otherwise eligible, so a dir-less `Proposed`/`Completed` row keeps its status
    /// code; this count keeps the fleet fact (292 AI-Brains rows) visible.
    #[serde(default)]
    pub without_dir: usize,
    #[serde(default)]
    pub samples: Vec<DroppedRow>,
    #[serde(default)]
    pub by_reason: BTreeMap<String, usize>,
    /// Set only when the diagnostic scan diverges from the parser, or on a read error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PickRowSource {
    Store,
    View,
    None,
}

/// The rows the picker actually reads (store first, view under `absent`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickRowsReport {
    pub source: PickRowSource,
    pub count: usize,
}

/// Parsed-id shape evidence: a digit-length histogram and unparsed samples.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdForm {
    pub parsed_lengths: BTreeMap<String, usize>,
    #[serde(default)]
    pub unparsed_samples: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    WouldStart,
    Skip,
}

/// The fourteen reasons the ticker would skip, plus `would_start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    FeatureOff,
    PolicyHitl,
    PolicyNever,
    AlreadyActive,
    DurableFailure,
    NotPickState,
    NoReadyRow,
    Nostart,
    RegistryDrift,
    RegistryMissing,
    RegistryInvalid,
    TransientRead,
    NoTrackDir,
    Unclassified,
}

/// The ticker's ordered gate walk (`watch::try_auto_start`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyDecision {
    pub kind: DecisionKind,
    pub track_id: Option<String>,
    pub reason: Option<SkipReason>,
    pub detail: Option<String>,
}

impl ReadyDecision {
    fn would_start(id: String) -> Self {
        Self {
            kind: DecisionKind::WouldStart,
            track_id: Some(id),
            reason: None,
            detail: None,
        }
    }

    fn skip(reason: SkipReason, detail: Option<String>) -> Self {
        Self {
            kind: DecisionKind::Skip,
            track_id: None,
            reason: Some(reason),
            detail,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorPickKind {
    Pick,
    RefusedNever,
    None,
    Error,
}

/// What an omit-`--track` operator `run` would choose from the same run state.
///
/// It applies the run path's run-state gate but deliberately does **not** model
/// `refused_retain_id` (which reads rows through `load_track_rows`): a retained
/// track reports `none` with a `detail`, never a different new Ready id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorPick {
    pub kind: OperatorPickKind,
    pub track_id: Option<String>,
    pub detail: Option<String>,
}

impl OperatorPick {
    fn pick(id: String) -> Self {
        Self {
            kind: OperatorPickKind::Pick,
            track_id: Some(id),
            detail: None,
        }
    }

    fn refused_never() -> Self {
        Self {
            kind: OperatorPickKind::RefusedNever,
            track_id: None,
            detail: Some("auto_start=never; pass --track".into()),
        }
    }

    fn none(detail: Option<String>) -> Self {
        Self {
            kind: OperatorPickKind::None,
            track_id: None,
            detail,
        }
    }

    fn error(detail: String) -> Self {
        Self {
            kind: OperatorPickKind::Error,
            track_id: None,
            detail: Some(detail),
        }
    }
}

/// Per-row eligibility code. `status_gate` is a boolean field, not a code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RowReasonCode {
    Eligible,
    EligibleNotChosen,
    StickyReady,
    Nostart,
    Hitl,
    Terminal,
    InProgress,
    Proposed,
    Blocked,
    StatusNotReady,
    CompletedElsewhere,
    NoTrackDir,
}

impl RowReasonCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Eligible => "eligible",
            Self::EligibleNotChosen => "eligible_not_chosen",
            Self::StickyReady => "sticky_ready",
            Self::Nostart => "nostart",
            Self::Hitl => "hitl",
            Self::Terminal => "terminal",
            Self::InProgress => "in_progress",
            Self::Proposed => "proposed",
            Self::Blocked => "blocked",
            Self::StatusNotReady => "status_not_ready",
            Self::CompletedElsewhere => "completed_elsewhere",
            Self::NoTrackDir => "no_track_dir",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowReason {
    pub id: String,
    pub slug: String,
    pub status_raw: String,
    pub status_clean: String,
    /// `is_eligible_ready_in` **before** the HITL/nostart gates.
    pub status_gate: bool,
    pub eligible: bool,
    pub code: RowReasonCode,
    pub detail: Option<String>,
}

/// The picked id's cleaned status in each listed block (0109 row 179 window).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickRowVersion {
    pub in_rows: Option<String>,
    pub in_pick_rows: Option<String>,
    /// `true` when **no** `pick_rows` copy of the picked id is overlay-eligible.
    pub picked_but_ineligible: bool,
}

/// One project's read-only ready report (Appendix A shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyReport {
    pub project_id: String,
    pub display_name: Option<String>,
    pub conductor_dir: PathBuf,
    pub registry: RegistryEvidence,
    pub tables: usize,
    pub rows: RowsReport,
    pub pick_rows: PickRowsReport,
    pub id_form: IdForm,
    pub ready_phrases: Vec<String>,
    pub ready_hits: BTreeMap<String, usize>,
    pub policy: AutoStartPolicy,
    pub feature: FeatureGate,
    pub ticker_effective: TickerEffective,
    pub operator_omit_pick: OperatorPick,
    /// Always the literal `"none"`: the preview runs the probeless pick.
    pub probe: String,
    pub pickable: usize,
    pub decision: ReadyDecision,
    pub pick_not_in_snapshot: bool,
    pub pick_row_version: Option<PickRowVersion>,
    pub row_reasons: Vec<RowReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineReady {
    pub home: PathBuf,
    pub feature_auto_start: FeatureGate,
}

/// `coordinator ready --all` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyFleet {
    pub generated_at: DateTime<Utc>,
    pub machine: MachineReady,
    pub projects: Vec<ReadyReport>,
}

/// Resolve the machine feature snapshot once and build the report.
pub fn report(record: &ProjectRecord, all_rows: bool) -> ReadyReport {
    report_with_features(record, all_rows, &crate::features::resolve_all())
}

/// Build the report from an already-resolved snapshot (one read feeds both
/// `status.features` and `status.ready.feature`, so the JSON cannot disagree).
pub fn report_with_features(
    record: &ProjectRecord,
    all_rows: bool,
    features: &FeatureSnapshot,
) -> ReadyReport {
    let feature = gate_of(features);
    let policy = record.auto_start;
    let aliases = conductor_md::ready_aliases_for(record);

    let layout = crate::layout::resolve(record);
    let conductor_dir = layout.conductor_dir.clone();
    let dir_names = crate::workflow::graph::conductor_dir_names(&conductor_dir).unwrap_or_default();
    let view_path = conductor_md_path(record);
    let (view_text, view_error) = match conductor_md::read_view_for_parse(&view_path) {
        Ok(text) => (Some(text), None),
        Err(e) => (None, Some(e)),
    };
    let parsed_view = view_text
        .as_deref()
        .and_then(|text| parse_conductor_md(text).ok());
    let scan = view_text
        .as_deref()
        .map(scan_dropped_rows)
        .unwrap_or_default();
    let view_rows = parsed_view.clone().unwrap_or_default();

    // ---- registry evidence + pick rows from ONE snapshot -------------------
    let (registry, pick_source, pick_rows, store_error) =
        snapshot_evidence(record, &conductor_dir, &view_rows, parsed_view.is_some());

    // ---- run state once, pick once ----------------------------------------
    let state_result = load_run_state(record);
    let pick_result = state_result
        .as_ref()
        .ok()
        .map(|state| pick_next_ready_excluding_sticky(record, &[], None, &state.sticky_ready_ids));

    // A scanner bug must degrade to a named decision, never abort `ready --all`.
    let scan_divergence = (view_text.is_some() && scan.parsed_rows != view_rows.len()).then(|| {
        format!(
            "scan/parser divergence: scan {} parsed, parser {}",
            scan.parsed_rows,
            view_rows.len()
        )
    });

    let decision = decide(
        record,
        feature.enabled,
        policy,
        &state_result,
        pick_result.as_ref(),
        &pick_rows,
        &aliases,
        store_error.as_ref(),
        scan_divergence.as_deref(),
    );
    let operator_omit_pick = operator_pick(policy, &state_result, pick_result.as_ref());

    let (reasons, without_dir) = build_row_reasons(
        &conductor_dir,
        &dir_names,
        &pick_rows,
        &aliases,
        state_result
            .as_ref()
            .map(|s| s.sticky_ready_ids.as_slice())
            .unwrap_or(&[]),
        decision.track_id.as_deref(),
    );
    let mut by_reason: BTreeMap<String, usize> = BTreeMap::new();
    for reason in &reasons {
        *by_reason
            .entry(reason.code.as_str().to_string())
            .or_insert(0) += 1;
    }
    let pickable = reasons
        .iter()
        .filter(|r| {
            matches!(
                r.code,
                RowReasonCode::Eligible | RowReasonCode::EligibleNotChosen
            )
        })
        .count();

    let row_reasons = if all_rows {
        reasons.clone()
    } else {
        reasons
            .iter()
            .filter(|r| is_ambiguous(r, decision.track_id.as_deref()))
            .cloned()
            .collect()
    };

    let id_form = id_form(&view_rows, &scan.dropped);
    let ready_hits: BTreeMap<String, usize> = scan
        .status_counts
        .iter()
        .filter(|(raw, _)| is_eligible_ready_in(raw, &aliases))
        .map(|(raw, count)| (raw.clone(), *count))
        .collect();

    let mut detail = view_error
        .as_ref()
        .map(|e| format!("read {}: {e}", view_path.display()));
    if detail.is_none() && scan_divergence.is_some() {
        detail = scan_divergence.clone();
    }

    let pick_not_in_snapshot = decision
        .track_id
        .as_deref()
        .is_some_and(|id| !pick_rows.iter().any(|r| row_matches_track(r, id)));
    let pick_row_version = decision.track_id.as_deref().and_then(|id| {
        pick_row_version_with(
            id,
            &view_rows,
            &pick_rows,
            &aliases,
            state_result
                .as_ref()
                .map(|s| s.sticky_ready_ids.as_slice())
                .unwrap_or(&[]),
        )
    });

    ReadyReport {
        project_id: record.id.clone(),
        display_name: record.display_name.clone(),
        conductor_dir,
        registry,
        tables: scan.tables,
        rows: RowsReport {
            source: if view_text.is_some() {
                RowSource::View
            } else {
                RowSource::None
            },
            data: scan.data_rows,
            parsed: scan.parsed_rows,
            dropped: scan.dropped_total,
            without_dir,
            samples: scan.dropped.iter().take(3).cloned().collect(),
            by_reason,
            detail,
        },
        pick_rows: PickRowsReport {
            source: pick_source,
            count: pick_rows.len(),
        },
        id_form,
        ready_phrases: aliases,
        ready_hits,
        policy,
        feature: feature.clone(),
        ticker_effective: ticker_effective(feature.enabled, policy),
        operator_omit_pick,
        probe: "none".into(),
        pickable,
        decision,
        pick_not_in_snapshot,
        pick_row_version,
        row_reasons,
    }
}

/// Trimmed `status --project` projection: no row list.
pub fn status_view(report: &ReadyReport) -> ReadyStatusView {
    ReadyStatusView {
        policy: report.policy,
        feature: report.feature.clone(),
        ticker_effective: report.ticker_effective,
        operator_omit_pick: report.operator_omit_pick.clone(),
        probe: report.probe.clone(),
        pickable: report.pickable,
        decision: report.decision.clone(),
    }
}

pub(crate) fn gate_of(features: &FeatureSnapshot) -> FeatureGate {
    match features.duties.get(Duty::AutoStart.as_str()) {
        Some(view) => FeatureGate {
            enabled: view.enabled,
            source: view.source,
        },
        None => FeatureGate {
            enabled: false,
            source: FeatureSource::Default,
        },
    }
}

pub(crate) fn ticker_effective(enabled: bool, policy: AutoStartPolicy) -> TickerEffective {
    if !enabled {
        return TickerEffective::FeatureOff;
    }
    match policy {
        AutoStartPolicy::Full => TickerEffective::Start,
        AutoStartPolicy::Hitl => TickerEffective::Hitl,
        AutoStartPolicy::Never => TickerEffective::Never,
    }
}

fn snapshot_evidence(
    record: &ProjectRecord,
    conductor_dir: &std::path::Path,
    view_rows: &[TrackRow],
    view_parsed: bool,
) -> (
    RegistryEvidence,
    PickRowSource,
    Vec<TrackRow>,
    Option<CoordinatorError>,
) {
    let store_path = crate::registry_store::store_path_for(record);
    let recovery = |text: String| Some(text);
    match crate::registry_store::snapshot(record) {
        Ok(snapshot) => match &snapshot.view {
            crate::registry_store::StoreView::Ok => (
                RegistryEvidence {
                    source: RegistrySource::Store,
                    state: RegistryState::Ok,
                    store_path,
                    recovery: None,
                },
                PickRowSource::Store,
                snapshot.rows.clone(),
                None,
            ),
            crate::registry_store::StoreView::Drifted { expected, actual } => (
                RegistryEvidence {
                    source: RegistrySource::Store,
                    state: RegistryState::Drift,
                    store_path,
                    recovery: recovery(format!(
                        "view {actual} != store {expected}; run `coordinator registry import --conductor-dir {} --force` to adopt it or `coordinator registry render --conductor-dir {} --force` to discard it",
                        conductor_dir.display(),
                        conductor_dir.display()
                    )),
                },
                PickRowSource::Store,
                snapshot.rows.clone(),
                None,
            ),
            crate::registry_store::StoreView::Absent => {
                let (source, pick_source) = if view_parsed {
                    (RegistrySource::View, PickRowSource::View)
                } else {
                    (RegistrySource::Missing, PickRowSource::None)
                };
                (
                    RegistryEvidence {
                        source,
                        state: RegistryState::Absent,
                        store_path,
                        recovery: None,
                    },
                    pick_source,
                    view_rows.to_vec(),
                    None,
                )
            }
        },
        Err(e) => (
            RegistryEvidence {
                source: RegistrySource::Store,
                state: RegistryState::Invalid,
                store_path,
                recovery: recovery(format!("{e}; repair or delete the store")),
            },
            PickRowSource::None,
            Vec::new(),
            Some(e),
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn decide(
    record: &ProjectRecord,
    feature_enabled: bool,
    policy: AutoStartPolicy,
    state_result: &Result<RunState>,
    pick_result: Option<&Result<String>>,
    pick_rows: &[TrackRow],
    aliases: &[String],
    store_error: Option<&CoordinatorError>,
    scan_divergence: Option<&str>,
) -> ReadyDecision {
    // (a) feature veto.
    if !feature_enabled {
        return ReadyDecision::skip(SkipReason::FeatureOff, None);
    }
    // (b) only `full` may auto-start.
    match policy {
        AutoStartPolicy::Full => {}
        AutoStartPolicy::Hitl => return ReadyDecision::skip(SkipReason::PolicyHitl, None),
        AutoStartPolicy::Never => return ReadyDecision::skip(SkipReason::PolicyNever, None),
    }
    // (c) fresh run state.
    let state = match state_result {
        Ok(state) => state,
        Err(e) => return ReadyDecision::skip(SkipReason::NotPickState, Some(e.to_string())),
    };
    // (d) active run.
    if matches!(state.status, RunStatus::Running | RunStatus::Paused) {
        return ReadyDecision::skip(SkipReason::AlreadyActive, None);
    }
    // (e) durable failure must precede `should_pick_next_ready`.
    if state.failure_class.is_some() || crate::notify::artifact::existing_path(record).is_some() {
        return ReadyDecision::skip(SkipReason::DurableFailure, None);
    }
    // (f) pick state.
    let mut pick = ReadyPickState::from(state);
    pick.auto_start = policy;
    if !should_pick_next_ready(&pick) {
        return ReadyDecision::skip(SkipReason::NotPickState, Some(run_state_detail(state)));
    }
    // (g) the picker's own result — never re-implemented. A scan/parser
    // divergence degrades to `unclassified` rather than trusting either count.
    if let Some(detail) = scan_divergence {
        return ReadyDecision::skip(SkipReason::Unclassified, Some(detail.to_string()));
    }
    match pick_result {
        Some(Ok(id)) => ReadyDecision::would_start(id.clone()),
        Some(Err(e)) => {
            let (reason, detail) = classify_pick_error(e, store_error, pick_rows, aliases);
            ReadyDecision::skip(reason, detail)
        }
        None => ReadyDecision::skip(
            SkipReason::Unclassified,
            Some("pick not evaluated (run state unavailable)".into()),
        ),
    }
}

fn operator_pick(
    policy: AutoStartPolicy,
    state_result: &Result<RunState>,
    pick_result: Option<&Result<String>>,
) -> OperatorPick {
    let state = match state_result {
        Ok(state) => state,
        Err(e) => return OperatorPick::error(e.to_string()),
    };
    let mut pick = ReadyPickState::from(state);
    pick.auto_start = policy;
    let mut no_policy = pick.clone();
    no_policy.auto_start = AutoStartPolicy::Hitl;
    if policy == AutoStartPolicy::Never && should_pick_next_ready(&no_policy) {
        return OperatorPick::refused_never();
    }
    if !should_pick_next_ready(&pick) {
        return OperatorPick::none(Some(run_state_detail(state)));
    }
    match pick_result {
        Some(Ok(id)) => OperatorPick::pick(id.clone()),
        Some(Err(e)) if conductor_md::ready_pick_miss_is_benign(e) => {
            OperatorPick::none(Some(e.to_string()))
        }
        Some(Err(e)) => OperatorPick::error(e.to_string()),
        None => OperatorPick::error("pick not evaluated (run state unavailable)".into()),
    }
}

fn run_state_detail(state: &RunState) -> String {
    match state.status {
        RunStatus::Running => return "run-status: running".into(),
        RunStatus::Paused => return "run-status: paused".into(),
        _ => {}
    }
    if let Some(track) = state
        .track_id
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        return format!("retained track: {track}");
    }
    if state.failure_class.is_some() {
        return "failure_class set".into();
    }
    "run-status: idle".into()
}

/// Map the picker's error to the report's reason, mirroring `try_auto_start`.
///
/// The four fall-through arms (`registry_missing`, `registry_invalid`,
/// `no_track_dir`, `unclassified`) are where the live ticker would call
/// `record_auto_start_failure` — which persists `RunState.failure_class` /
/// `last_event` and creates **no `FAILURE.md` and no toast**. The detail names
/// that; the preview performs neither.
fn classify_pick_error(
    err: &CoordinatorError,
    store_error: Option<&CoordinatorError>,
    pick_rows: &[TrackRow],
    aliases: &[String],
) -> (SkipReason, Option<String>) {
    if crate::registry_store::is_view_drift_error(err) {
        return (SkipReason::RegistryDrift, Some(err.to_string()));
    }
    if let Some(store_err) = store_error.filter(|e| !matches!(e, CoordinatorError::Io(_)))
        && store_err.to_string() == err.to_string()
    {
        return (
            SkipReason::RegistryInvalid,
            Some(format!(
                "{err}; live: record_auto_start_failure (RunState only)"
            )),
        );
    }
    if conductor_md::ready_pick_miss_is_benign(err) {
        let withheld = nostart_withheld_ids(pick_rows, aliases);
        if withheld.is_empty() {
            return (SkipReason::NoReadyRow, Some(err.to_string()));
        }
        return (SkipReason::Nostart, Some(withheld.join(", ")));
    }
    if matches!(err, CoordinatorError::Io(_)) {
        return (SkipReason::TransientRead, Some(err.to_string()));
    }
    let message = err.to_string();
    if message.starts_with("no conductor.md at ") {
        return (
            SkipReason::RegistryMissing,
            Some(format!(
                "{message}; live: record_auto_start_failure (RunState only)"
            )),
        );
    }
    if message.starts_with("Ready track(s) ") {
        return (
            SkipReason::NoTrackDir,
            Some(format!(
                "{message}; live: record_auto_start_failure (RunState only)"
            )),
        );
    }
    (
        SkipReason::Unclassified,
        Some(format!(
            "{message}; live: record_auto_start_failure (RunState only)"
        )),
    )
}

/// Ready rows withheld *only* by `<!-- nostart -->`.
fn nostart_withheld_ids(pick_rows: &[TrackRow], aliases: &[String]) -> Vec<String> {
    pick_rows
        .iter()
        .filter(|row| {
            row.nostart
                && is_eligible_ready_in(&row.status_raw, aliases)
                && !is_hitl_marked(&row.id, &row.slug, &row.status_raw, &row.summary)
        })
        .map(|row| row.id.clone())
        .collect()
}

fn build_row_reasons(
    conductor_dir: &std::path::Path,
    dir_names: &[String],
    pick_rows: &[TrackRow],
    aliases: &[String],
    sticky: &[String],
    chosen: Option<&str>,
) -> (Vec<RowReason>, usize) {
    let mut without_dir = 0;
    let reasons = pick_rows
        .iter()
        .map(|row| {
            let status_gate = is_eligible_ready_in(&row.status_raw, aliases);
            let dir_missing =
                crate::workflow::graph::resolve_track_dir_in(conductor_dir, dir_names, &row.id)
                    .is_none();
            if dir_missing {
                without_dir += 1;
            }
            let (code, detail) = if let Some(field) =
                hitl_field(&row.id, &row.slug, &row.status_raw, &row.summary)
            {
                (RowReasonCode::Hitl, Some(field.to_string()))
            } else if row.nostart {
                (RowReasonCode::Nostart, None)
            } else if status_gate {
                if conductor_md::track_row_completed(pick_rows, &row.id) {
                    (RowReasonCode::CompletedElsewhere, None)
                } else if dir_missing {
                    (RowReasonCode::NoTrackDir, None)
                } else if chosen.is_some_and(|id| row_matches_track(row, id)) {
                    (RowReasonCode::Eligible, None)
                } else {
                    (RowReasonCode::EligibleNotChosen, None)
                }
            } else if is_in_progress(&row.status_raw) {
                (RowReasonCode::InProgress, None)
            } else if is_terminal(&row.status_raw) {
                (RowReasonCode::Terminal, None)
            } else if is_proposed(&row.status_raw) {
                (RowReasonCode::Proposed, None)
            } else if is_blocked(&row.status_raw) {
                (RowReasonCode::Blocked, None)
            } else if sticky
                .iter()
                .any(|s| crate::notify::artifact::track_ids_match(&row.id, s))
            {
                (RowReasonCode::StickyReady, None)
            } else {
                (RowReasonCode::StatusNotReady, None)
            };
            RowReason {
                id: row.id.clone(),
                slug: row.slug.clone(),
                status_raw: row.status_raw.clone(),
                status_clean: status_clean(&row.status_raw),
                status_gate,
                eligible: code == RowReasonCode::Eligible,
                code,
                detail,
            }
        })
        .collect();
    (reasons, without_dir)
}

/// Default (ambiguous-only) row list: every alias hit, nostart, HITL, a
/// `status_not_ready` that mentions Ready/Pending, and the chosen row.
fn is_ambiguous(reason: &RowReason, chosen: Option<&str>) -> bool {
    if reason.status_gate
        || matches!(
            reason.code,
            RowReasonCode::Nostart | RowReasonCode::Hitl | RowReasonCode::Eligible
        )
    {
        return true;
    }
    if chosen.is_some_and(|id| crate::notify::artifact::track_ids_match(&reason.id, id)) {
        return true;
    }
    reason.code == RowReasonCode::StatusNotReady && mentions_ready_or_pending(&reason.status_raw)
}

fn mentions_ready_or_pending(raw: &str) -> bool {
    let lower = raw.to_ascii_lowercase();
    lower.contains("ready") || lower.contains("pending")
}

fn id_form(view_rows: &[TrackRow], dropped: &[DroppedRow]) -> IdForm {
    let mut parsed_lengths: BTreeMap<String, usize> = BTreeMap::new();
    for row in view_rows {
        *parsed_lengths.entry(row.id.len().to_string()).or_insert(0) += 1;
    }
    let mut unparsed_samples: Vec<String> = Vec::new();
    for drop in dropped {
        if drop.reason != conductor_md::DropReason::IdUnparseable {
            continue;
        }
        if drop.track_cell.is_empty() || unparsed_samples.contains(&drop.track_cell) {
            continue;
        }
        unparsed_samples.push(drop.track_cell.clone());
        if unparsed_samples.len() >= 5 {
            break;
        }
    }
    IdForm {
        parsed_lengths,
        unparsed_samples,
    }
}

/// Pure comparison of the picked id's cleaned status across the two listed blocks.
///
/// A duplicate id where one copy is Ready must not set the flag: match **any**
/// eligible copy, never the first one.
#[cfg(test)]
pub(crate) fn pick_row_version(
    picked: &str,
    rows: &[TrackRow],
    pick_rows: &[TrackRow],
) -> Option<PickRowVersion> {
    let aliases: Vec<String> = conductor_md::default_ready_aliases()
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    pick_row_version_with(picked, rows, pick_rows, &aliases, &[])
}

fn pick_row_version_with(
    picked: &str,
    rows: &[TrackRow],
    pick_rows: &[TrackRow],
    aliases: &[String],
    sticky: &[String],
) -> Option<PickRowVersion> {
    let matched_rows: Vec<&TrackRow> = rows
        .iter()
        .filter(|r| row_matches_track(r, picked))
        .collect();
    let matched_pick: Vec<&TrackRow> = pick_rows
        .iter()
        .filter(|r| row_matches_track(r, picked))
        .collect();
    if matched_rows.is_empty() && matched_pick.is_empty() {
        return None;
    }
    let pick_eligible = matched_pick
        .iter()
        .any(|r| row_overlay_eligible(r, aliases, sticky));
    let row_copy = matched_rows
        .iter()
        .find(|r| row_overlay_eligible(r, aliases, sticky))
        .or_else(|| matched_rows.first());
    let pick_copy = matched_pick
        .iter()
        .find(|r| row_overlay_eligible(r, aliases, sticky))
        .or_else(|| matched_pick.first());
    Some(PickRowVersion {
        in_rows: row_copy.map(|r| status_clean(&r.status_raw)),
        in_pick_rows: pick_copy.map(|r| status_clean(&r.status_raw)),
        picked_but_ineligible: !pick_eligible,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{TestEnvGuard, test_env_lock};
    use crate::outcome::FailureClass;
    use crate::registry::{AutoStartPolicy, ProjectRecord, Registry};
    use crate::state::{RunState, RunStatus, save_run_state};
    use crate::watch::{AutoStartAttempt, AutoStartSkip, try_auto_start};
    use crate::workflow::MergedTrackProbe;
    use sha2::{Digest, Sha256};
    use std::path::{Path, PathBuf};
    use tempfile::{TempDir, tempdir};

    const READY_EM: &str = "**Ready — not started**";
    const READY_ASCII: &str = "**Ready - not started**";

    fn env_guard() -> TestEnvGuard {
        let guard = TestEnvGuard::capture(&crate::features::test_feature_env_names());
        for name in crate::features::test_duty_env_vars() {
            unsafe { std::env::remove_var(name) };
        }
        guard
    }

    fn write_gate(home: &Path, enabled: bool) {
        let body = serde_json::json!({
            "version": 1,
            "scan_roots": [],
            "features": {"auto_start": enabled},
        });
        std::fs::write(home.join("config.json"), body.to_string()).unwrap();
    }

    fn base_record(ws: &Path) -> ProjectRecord {
        ProjectRecord {
            id: uuid::Uuid::new_v4().to_string(),
            path: ws.to_path_buf(),
            display_name: Some("fixture".into()),
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
            auto_start: AutoStartPolicy::Full,
            state_policies: Vec::new(),
            self_continuation: false,
            ci_fix_routing: false,

            review_comment_routing: false,
            skill_aliases: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now(),
        }
    }

    fn conductor(ws: &Path) -> PathBuf {
        ws.join("conductor")
    }

    fn write_md(ws: &Path, md: &str) {
        let dir = conductor(ws);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("conductor.md"), md).unwrap();
    }

    fn track_dir(ws: &Path, name: &str) {
        let dir = conductor(ws).join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("spec.md"), b"spec\n").unwrap();
        std::fs::write(dir.join("plan.md"), b"plan\n").unwrap();
    }

    fn ready_md(id: &str) -> String {
        format!(
            "| Track | Status | Summary |\n\
             | --- | --- | --- |\n\
             | {id}-Fixture | {READY_EM} | fixture |\n"
        )
    }

    fn save_state(rec: &ProjectRecord, state: &RunState) {
        save_run_state(rec, state).unwrap();
    }

    fn running(rec: &ProjectRecord) {
        let mut state = RunState::idle(&rec.id);
        state.status = RunStatus::Running;
        save_state(rec, &state);
    }

    fn register(records: &[ProjectRecord]) {
        let path = crate::config::registry_path().unwrap();
        let reg = Registry {
            projects: records.to_vec(),
            ..Registry::default()
        };
        reg.save(&path).unwrap();
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn tree(root: &Path) -> std::collections::BTreeMap<String, String> {
        fn walk(dir: &Path, base: &Path, out: &mut std::collections::BTreeMap<String, String>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let rel = path
                    .strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                if path.is_dir() {
                    out.insert(format!("d:{rel}"), "dir".into());
                    walk(&path, base, out);
                } else {
                    let bytes = std::fs::read(&path).unwrap_or_default();
                    out.insert(
                        format!("f:{rel}"),
                        format!("{}:{}", bytes.len(), sha256_hex(&bytes)),
                    );
                }
            }
        }
        let mut out = std::collections::BTreeMap::new();
        walk(root, root, &mut out);
        out
    }

    fn assert_writes_nothing(home: &Path, ws: &Path, rec: &ProjectRecord, all_rows: bool) {
        let before_home = tree(home);
        let before_ws = tree(ws);
        let _ = report(rec, all_rows);
        assert_eq!(before_home, tree(home), "home tree changed");
        assert_eq!(before_ws, tree(ws), "project tree changed");
    }

    fn build_would_start(home: &Path) -> (TempDir, ProjectRecord) {
        write_gate(home, true);
        let ws = tempdir().unwrap();
        let rec = base_record(ws.path());
        write_md(ws.path(), &ready_md("0020"));
        track_dir(ws.path(), "0020-Fixture");
        (ws, rec)
    }

    fn build_arm(home: &Path, reason: SkipReason) -> (TempDir, ProjectRecord) {
        let ws = tempdir().unwrap();
        let mut rec = base_record(ws.path());
        match reason {
            SkipReason::FeatureOff => {
                write_gate(home, false);
                write_md(ws.path(), &ready_md("0020"));
                track_dir(ws.path(), "0020-Fixture");
            }
            SkipReason::PolicyHitl => {
                write_gate(home, true);
                rec.auto_start = AutoStartPolicy::Hitl;
                write_md(ws.path(), &ready_md("0020"));
                track_dir(ws.path(), "0020-Fixture");
            }
            SkipReason::PolicyNever => {
                write_gate(home, true);
                rec.auto_start = AutoStartPolicy::Never;
                write_md(ws.path(), &ready_md("0020"));
                track_dir(ws.path(), "0020-Fixture");
            }
            SkipReason::AlreadyActive => {
                write_gate(home, true);
                write_md(ws.path(), &ready_md("0020"));
                track_dir(ws.path(), "0020-Fixture");
                running(&rec);
            }
            SkipReason::DurableFailure => {
                write_gate(home, true);
                write_md(ws.path(), &ready_md("0020"));
                track_dir(ws.path(), "0020-Fixture");
                let mut state = RunState::idle(&rec.id);
                state.failure_class = Some(FailureClass::HarnessCrash);
                save_state(&rec, &state);
            }
            SkipReason::NotPickState => {
                write_gate(home, true);
                write_md(ws.path(), &ready_md("0020"));
                track_dir(ws.path(), "0020-Fixture");
                let mut state = RunState::idle(&rec.id);
                state.status = RunStatus::Stopped;
                state.track_id = Some("0019".into());
                save_state(&rec, &state);
            }
            SkipReason::NoReadyRow => {
                write_gate(home, true);
                write_md(
                    ws.path(),
                    "| Track | Status | Summary |\n\
                     | --- | --- | --- |\n\
                     | 0020-Done | **Completed** | done |\n",
                );
            }
            SkipReason::Nostart => {
                write_gate(home, true);
                write_md(
                    ws.path(),
                    &format!(
                        "| Track | Status | Summary |\n\
                         | --- | --- | --- |\n\
                         | 0020-Held | {READY_EM} | hold <!-- nostart --> |\n"
                    ),
                );
                track_dir(ws.path(), "0020-Held");
            }
            SkipReason::RegistryDrift => {
                write_gate(home, true);
                write_md(ws.path(), &ready_md("0020"));
                track_dir(ws.path(), "0020-Fixture");
                let dir = conductor(ws.path());
                crate::registry_store::import_at(
                    &dir,
                    crate::registry_store::ImportOpts {
                        dry_run: false,
                        force: false,
                    },
                )
                .unwrap();
                let path = dir.join("conductor.md");
                let mut text = std::fs::read_to_string(&path).unwrap();
                text.push_str("\n<!-- edited outside the store -->\n");
                std::fs::write(&path, text).unwrap();
                crate::registry_store::reset_drift_warnings_for_tests();
            }
            SkipReason::RegistryMissing => {
                write_gate(home, true);
            }
            SkipReason::RegistryInvalid => {
                write_gate(home, true);
                let dir = conductor(ws.path());
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("track-registry.json"), b"{ not a store").unwrap();
            }
            SkipReason::TransientRead => {
                write_gate(home, true);
                let dir = conductor(ws.path());
                std::fs::create_dir_all(&dir).unwrap();
                // A directory at the store path makes the snapshot read fail with `Io`.
                std::fs::create_dir(dir.join("track-registry.json")).unwrap();
            }
            SkipReason::NoTrackDir => {
                write_gate(home, true);
                write_md(
                    ws.path(),
                    &format!(
                        "| Track | Status | Summary |\n\
                         | --- | --- | --- |\n\
                         | T374-Orphan | {READY_EM} | no dir |\n"
                    ),
                );
            }
            SkipReason::Unclassified => {
                write_gate(home, true);
                write_md(ws.path(), "# Registry\n\nNo table in this file.\n");
            }
        }
        (ws, rec)
    }

    #[test]
    fn report_matches_the_canonical_pick() {
        let _guard = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        write_gate(home.path(), true);
        let ws = tempdir().unwrap();
        let rec = base_record(ws.path());
        write_md(
            ws.path(),
            "# Registry\n\n\
             | Track | Status | Summary |\n\
             | --- | --- | --- |\n\
             | 0010-Alpha | **Ready — not started** | first |\n\
             | 0020-Beta | **Ready — not started** | second <!-- nostart --> |\n\
             | 0030-Gamma | Active | sticky |\n\
             | 0040-Delta | **Ready — not started** | owner-HITL gaps |\n\
             | 0050-Epsilon | **Completed** | done |\n\
             | 0050-Epsilon | **Ready — not started** | duplicate |\n\
             | 0060-Zeta | **Ready — not started** | no dir |\n\n\
             ## Suggested execution order\n\n\
             ```\n\
             0030 Gamma\n\
             0010 Alpha\n\
             0020 Beta\n\
             ```\n",
        );
        for name in [
            "0010-Alpha",
            "0020-Beta",
            "0030-Gamma",
            "0040-Delta",
            "0050-Epsilon",
        ] {
            track_dir(ws.path(), name);
        }
        let mut state = RunState::idle(&rec.id);
        state.sticky_ready_ids = vec!["0030".into()];
        save_state(&rec, &state);

        let report = report(&rec, true);
        let expected =
            pick_next_ready_excluding_sticky(&rec, &[], None, &state.sticky_ready_ids).unwrap();
        assert_eq!(report.decision.track_id.as_deref(), Some(expected.as_str()));
        assert_eq!(report.decision.kind, DecisionKind::WouldStart);
        assert_eq!(report.ticker_effective, TickerEffective::Start);

        let first_ready = pick_next_ready_excluding_sticky(&rec, &[], None, &[]).unwrap();
        assert_ne!(first_ready, expected, "sticky/order must change the pick");
        assert_eq!(first_ready, "0010");

        // One clean sub-fixture: the probeless pick equals the run path's pick.
        let ws2 = tempdir().unwrap();
        let rec2 = base_record(ws2.path());
        write_md(ws2.path(), &ready_md("0020"));
        track_dir(ws2.path(), "0020-Fixture");
        let clean = super::report(&rec2, true);
        let id = clean.decision.track_id.clone().expect("would_start id");
        struct NoopProbe;
        impl MergedTrackProbe for NoopProbe {
            fn merged_pr_for_track(&self, _cwd: &Path, _id: &str) -> Result<Option<u64>> {
                Ok(None)
            }
        }
        let (run_id, picked) =
            crate::api::resolve_run_track_with_probe(&rec2, None, Some(&NoopProbe)).unwrap();
        assert_eq!(run_id.as_deref(), Some(id.as_str()));
        assert!(picked);
        assert_eq!(clean.ticker_effective, TickerEffective::Start);
    }

    #[test]
    fn report_reasons_are_exhaustive_and_unique() {
        let _guard = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        write_gate(home.path(), true);
        let ws = tempdir().unwrap();
        let rec = base_record(ws.path());
        write_md(
            ws.path(),
            "| Track | Status | Summary |\n\
             | --- | --- | --- |\n\
             | 0010-Alpha | **Ready — not started** | a |\n\
             | 0020-Beta | **Ready — not started** | b |\n\
             | 0030-Gamma | **Proposed** | c |\n",
        );
        track_dir(ws.path(), "0010-Alpha");
        track_dir(ws.path(), "0020-Beta");
        track_dir(ws.path(), "0030-Gamma");
        let report = report(&rec, true);
        let eligible: Vec<&RowReason> = report
            .row_reasons
            .iter()
            .filter(|r| r.code == RowReasonCode::Eligible)
            .collect();
        assert_eq!(eligible.len(), 1);
        assert_eq!(
            Some(eligible[0].id.as_str()),
            report.decision.track_id.as_deref()
        );
        assert_eq!(
            report.rows.by_reason.values().sum::<usize>(),
            report.pick_rows.count
        );
        for reason in &report.row_reasons {
            assert_eq!(reason.eligible, reason.code == RowReasonCode::Eligible);
        }
        assert_eq!(report.pickable, 2);
    }

    #[test]
    fn report_skip_arms_match_try_auto_start() {
        let _guard = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        let cases = [
            (
                SkipReason::FeatureOff,
                AutoStartSkip::FeatureOff,
                TickerEffective::FeatureOff,
            ),
            (
                SkipReason::PolicyHitl,
                AutoStartSkip::Policy(AutoStartPolicy::Hitl),
                TickerEffective::Hitl,
            ),
            (
                SkipReason::PolicyNever,
                AutoStartSkip::Policy(AutoStartPolicy::Never),
                TickerEffective::Never,
            ),
            (
                SkipReason::AlreadyActive,
                AutoStartSkip::AlreadyActive,
                TickerEffective::Start,
            ),
            (
                SkipReason::DurableFailure,
                AutoStartSkip::DurableFailure,
                TickerEffective::Start,
            ),
            (
                SkipReason::NotPickState,
                AutoStartSkip::NotEligible,
                TickerEffective::Start,
            ),
            (
                SkipReason::NoReadyRow,
                AutoStartSkip::NoReadyRow,
                TickerEffective::Start,
            ),
            (
                SkipReason::RegistryDrift,
                AutoStartSkip::RegistryDrift,
                TickerEffective::Start,
            ),
            (
                SkipReason::TransientRead,
                AutoStartSkip::TransientRead,
                TickerEffective::Start,
            ),
        ];
        for (reason, skip, effective) in cases {
            let (_ws, rec) = build_arm(home.path(), reason);
            let report = report(&rec, true);
            assert_eq!(report.decision.kind, DecisionKind::Skip, "{reason:?}");
            assert_eq!(report.decision.reason, Some(reason), "{reason:?}");
            assert_eq!(report.ticker_effective, effective, "{reason:?}");
            assert_eq!(
                try_auto_start(&rec),
                AutoStartAttempt::Skipped(skip),
                "{reason:?}"
            );
        }

        // Paused is the second AlreadyActive arm.
        let ws = tempdir().unwrap();
        let rec = base_record(ws.path());
        write_gate(home.path(), true);
        write_md(ws.path(), &ready_md("0020"));
        track_dir(ws.path(), "0020-Fixture");
        let mut state = RunState::idle(&rec.id);
        state.status = RunStatus::Paused;
        save_state(&rec, &state);
        assert_eq!(
            report(&rec, true).decision.reason,
            Some(SkipReason::AlreadyActive)
        );
        assert_eq!(
            try_auto_start(&rec),
            AutoStartAttempt::Skipped(AutoStartSkip::AlreadyActive)
        );

        // A FAILURE.md (no failure_class) is durable too.
        let ws = tempdir().unwrap();
        let rec = base_record(ws.path());
        write_gate(home.path(), true);
        write_md(ws.path(), &ready_md("0020"));
        track_dir(ws.path(), "0020-Fixture");
        let artifact = crate::notify::artifact::path(&rec).unwrap();
        std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        std::fs::write(&artifact, b"failure\n").unwrap();
        assert_eq!(
            report(&rec, true).decision.reason,
            Some(SkipReason::DurableFailure)
        );
        assert_eq!(
            try_auto_start(&rec),
            AutoStartAttempt::Skipped(AutoStartSkip::DurableFailure)
        );
    }

    #[test]
    fn report_feature_off_keeps_the_operator_pick() {
        let _guard = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        let (_ws, rec) = build_arm(home.path(), SkipReason::FeatureOff);
        let report = report(&rec, true);
        assert_eq!(report.decision.reason, Some(SkipReason::FeatureOff));
        assert_eq!(report.ticker_effective, TickerEffective::FeatureOff);
        let expected = pick_next_ready_excluding_sticky(&rec, &[], None, &[]).unwrap();
        assert_eq!(
            report.operator_omit_pick.kind,
            OperatorPickKind::Pick,
            "the operator pick stays visible while the machine gate is off"
        );
        assert_eq!(
            report.operator_omit_pick.track_id.as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn report_feature_off_beats_invalid_store() {
        let _guard = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        // Gate off first, then a corrupt store: the precedence case.
        let ws = tempdir().unwrap();
        let rec = base_record(ws.path());
        write_gate(home.path(), false);
        let dir = conductor(ws.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("track-registry.json"), b"{ not a store").unwrap();
        let report = report(&rec, true);
        assert_eq!(report.decision.reason, Some(SkipReason::FeatureOff));
        assert_eq!(report.registry.state, RegistryState::Invalid);
    }

    #[test]
    fn report_hitl_running_prefers_policy() {
        let _guard = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        write_gate(home.path(), true);
        let ws = tempdir().unwrap();
        let mut rec = base_record(ws.path());
        rec.auto_start = AutoStartPolicy::Hitl;
        write_md(ws.path(), &ready_md("0020"));
        track_dir(ws.path(), "0020-Fixture");
        running(&rec);
        let report = report(&rec, true);
        assert_eq!(report.decision.reason, Some(SkipReason::PolicyHitl));
        assert_eq!(report.operator_omit_pick.kind, OperatorPickKind::None);
    }

    #[test]
    fn report_operator_pick_follows_run_state() {
        let _guard = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        write_gate(home.path(), true);

        // never + Idle + Ready → policy_never AND refused_never.
        let ws = tempdir().unwrap();
        let mut rec = base_record(ws.path());
        rec.auto_start = AutoStartPolicy::Never;
        write_md(ws.path(), &ready_md("0020"));
        track_dir(ws.path(), "0020-Fixture");
        let report = report(&rec, true);
        assert_eq!(report.decision.reason, Some(SkipReason::PolicyNever));
        assert_eq!(
            report.operator_omit_pick.kind,
            OperatorPickKind::RefusedNever
        );

        // The same project while Running with no retained track → none.
        running(&rec);
        let report = super::report(&rec, true);
        assert_eq!(report.operator_omit_pick.kind, OperatorPickKind::None);
        assert!(
            report
                .operator_omit_pick
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("running"))
        );

        // full + Running + a second Ready row → already_active, never the second id.
        let ws = tempdir().unwrap();
        let rec = base_record(ws.path());
        write_md(
            ws.path(),
            &format!(
                "| Track | Status | Summary |\n\
                 | --- | --- | --- |\n\
                 | 0020-First | {READY_EM} | a |\n\
                 | 0030-Second | {READY_EM} | b |\n"
            ),
        );
        track_dir(ws.path(), "0020-First");
        track_dir(ws.path(), "0030-Second");
        running(&rec);
        let report = super::report(&rec, true);
        assert_eq!(report.decision.reason, Some(SkipReason::AlreadyActive));
        assert_eq!(report.operator_omit_pick.kind, OperatorPickKind::None);
        assert_eq!(report.operator_omit_pick.track_id, None);

        // feature off + Idle + Ready → the picker's id.
        let (_ws, rec) = build_arm(home.path(), SkipReason::FeatureOff);
        let report = super::report(&rec, true);
        let expected = pick_next_ready_excluding_sticky(&rec, &[], None, &[]).unwrap();
        assert_eq!(report.operator_omit_pick.kind, OperatorPickKind::Pick);
        assert_eq!(
            report.operator_omit_pick.track_id.as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn report_writes_nothing() {
        let _guard = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        let arms = [
            SkipReason::FeatureOff,
            SkipReason::PolicyHitl,
            SkipReason::PolicyNever,
            SkipReason::AlreadyActive,
            SkipReason::DurableFailure,
            SkipReason::NotPickState,
            SkipReason::NoReadyRow,
            SkipReason::Nostart,
            SkipReason::RegistryDrift,
            SkipReason::RegistryMissing,
            SkipReason::RegistryInvalid,
            SkipReason::TransientRead,
            SkipReason::NoTrackDir,
            SkipReason::Unclassified,
        ];
        for arm in arms {
            crate::registry_store::reset_drift_warnings_for_tests();
            let (ws, rec) = build_arm(home.path(), arm);
            assert_writes_nothing(home.path(), ws.path(), &rec, true);
        }
        let (ws, rec) = build_would_start(home.path());
        assert_writes_nothing(home.path(), ws.path(), &rec, true);
    }

    #[test]
    fn report_dropped_rows_and_id_forms() {
        let _guard = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        write_gate(home.path(), true);
        let ws = tempdir().unwrap();
        let rec = base_record(ws.path());
        write_md(
            ws.path(),
            "| Track | Status | Summary |\n\
             | --- | --- | --- |\n\
             | [0001-Foo](0001-Foo/spec.md) | **Ready — not started** | ok |\n\
             | track12 | **Ready — not started** | x |\n\
             | AI-T572-SafetySyncHotspotDelta | **Ready — not started** | y |\n\
             | 0003-Gamma | **Ready — not started** | z | extra |\n\
             | T374 | **Ready — not started** | t |\n",
        );
        track_dir(ws.path(), "0001-Foo");
        let report = report(&rec, true);
        assert_eq!(report.rows.data, 5);
        assert_eq!(report.rows.parsed, 2);
        assert_eq!(report.rows.dropped, 3);
        assert_eq!(report.rows.data, report.rows.parsed + report.rows.dropped);
        let ai = report
            .rows
            .samples
            .iter()
            .find(|s| s.track_cell == "AI-T572-SafetySyncHotspotDelta")
            .expect("AI-T572 sample");
        assert_eq!(ai.line, 5);
        assert_eq!(ai.reason, conductor_md::DropReason::IdUnparseable);
        let extra = report
            .rows
            .samples
            .iter()
            .find(|s| s.line_cells == 4)
            .expect("extra pipe sample");
        assert_eq!(extra.line, 6);
        assert_eq!(extra.reason, conductor_md::DropReason::CellCount);
        assert_eq!(report.id_form.parsed_lengths.get("4"), Some(&1));
        assert_eq!(report.id_form.parsed_lengths.get("3"), Some(&1));
        assert!(
            report
                .id_form
                .unparsed_samples
                .iter()
                .any(|s| s == "track12")
        );
        assert!(
            report
                .id_form
                .unparsed_samples
                .iter()
                .any(|s| s == "AI-T572-SafetySyncHotspotDelta")
        );
    }

    #[test]
    fn report_nostart_and_hitl_prose() {
        let _guard = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        write_gate(home.path(), true);
        let ws = tempdir().unwrap();
        let rec = base_record(ws.path());
        write_md(
            ws.path(),
            &format!(
                "| Track | Status | Summary |\n\
                 | --- | --- | --- |\n\
                 | 0014-Printer | {READY_ASCII} | owner-HITL gaps |\n\
                 | 0020-Held | {READY_EM} | hold <!-- nostart --> |\n\
                 | 0030-Both | {READY_EM} | owner-HITL and <!-- nostart --> |\n"
            ),
        );
        for name in ["0014-Printer", "0020-Held", "0030-Both"] {
            track_dir(ws.path(), name);
        }
        let report = report(&rec, true);
        let printer = report.row_reasons.iter().find(|r| r.id == "0014").unwrap();
        assert!(printer.status_gate);
        assert_eq!(printer.code, RowReasonCode::Hitl);
        assert_eq!(printer.detail.as_deref(), Some("summary"));
        let held = report.row_reasons.iter().find(|r| r.id == "0020").unwrap();
        assert_eq!(held.code, RowReasonCode::Nostart);
        let both = report.row_reasons.iter().find(|r| r.id == "0030").unwrap();
        assert_eq!(both.code, RowReasonCode::Hitl);
    }

    #[test]
    fn report_alpha_prefixed_id_without_dir() {
        let _guard = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        let (ws, rec) = build_arm(home.path(), SkipReason::NoTrackDir);
        let report = report(&rec, true);
        assert_eq!(report.decision.reason, Some(SkipReason::NoTrackDir));
        assert_eq!(report.pick_rows.count, 1);
        assert_eq!(report.rows.without_dir, 1);
        assert_eq!(report.row_reasons[0].code, RowReasonCode::NoTrackDir);
        assert_writes_nothing(home.path(), ws.path(), &rec, true);
    }

    #[test]
    fn report_nostart_only_project_decision() {
        let _guard = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        write_gate(home.path(), true);
        let ws = tempdir().unwrap();
        let rec = base_record(ws.path());
        write_md(
            ws.path(),
            &format!(
                "| Track | Status | Summary |\n\
                 | --- | --- | --- |\n\
                 | 0115-Held | {READY_EM} | wait <!-- nostart --> |\n"
            ),
        );
        track_dir(ws.path(), "0115-Held");
        let report = report(&rec, true);
        assert_eq!(report.decision.reason, Some(SkipReason::Nostart));
        assert_eq!(report.operator_omit_pick.kind, OperatorPickKind::None);
        assert_eq!(report.decision.detail.as_deref(), Some("0115"));
    }

    #[test]
    fn report_missing_registry_is_named() {
        let _guard = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        let (ws, rec) = build_arm(home.path(), SkipReason::RegistryMissing);
        let report = report(&rec, true);
        assert_eq!(report.decision.reason, Some(SkipReason::RegistryMissing));
        assert_eq!(report.rows.source, RowSource::None);
        assert_eq!(report.pick_rows.source, PickRowSource::None);
        assert_eq!(report.rows.data, 0);
        assert_eq!(report.rows.parsed, 0);
        assert_eq!(report.registry.source, RegistrySource::Missing);
        assert_eq!(report.registry.state, RegistryState::Absent);
        assert_writes_nothing(home.path(), ws.path(), &rec, true);
    }

    #[test]
    fn report_unreadable_run_state_is_not_pick_state() {
        let _guard = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        write_gate(home.path(), true);
        let ws = tempdir().unwrap();
        let rec = base_record(ws.path());
        write_md(ws.path(), &ready_md("0020"));
        track_dir(ws.path(), "0020-Fixture");
        let state_dir = ws.path().join(".coordinator");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("run-state.json"), b"{ not run state").unwrap();

        let report = report(&rec, true);
        assert_eq!(report.decision.kind, DecisionKind::Skip);
        assert_eq!(report.decision.reason, Some(SkipReason::NotPickState));
        assert!(report.decision.detail.is_some());
        assert_eq!(
            try_auto_start(&rec),
            AutoStartAttempt::Skipped(AutoStartSkip::NotEligible)
        );
        assert_writes_nothing(home.path(), ws.path(), &rec, true);
    }

    #[test]
    fn report_prose_only_registry() {
        let _guard = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        let (ws, rec) = build_arm(home.path(), SkipReason::Unclassified);
        let report = report(&rec, true);
        assert_eq!(report.tables, 0);
        assert_eq!(report.rows.parsed, 0);
        assert_eq!(report.decision.reason, Some(SkipReason::Unclassified));
        assert!(
            report
                .decision
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("could not parse track registry"))
        );
        assert_writes_nothing(home.path(), ws.path(), &rec, true);
    }

    #[test]
    fn report_store_and_view_modes() {
        let _guard = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        write_gate(home.path(), true);
        let ws = tempdir().unwrap();
        let rec = base_record(ws.path());
        write_md(ws.path(), &ready_md("0020"));
        track_dir(ws.path(), "0020-Fixture");
        let dir = conductor(ws.path());
        crate::registry_store::import_at(
            &dir,
            crate::registry_store::ImportOpts {
                dry_run: false,
                force: false,
            },
        )
        .unwrap();
        let report = report(&rec, true);
        assert_eq!(report.registry.source, RegistrySource::Store);
        assert_eq!(report.registry.state, RegistryState::Ok);
        assert_eq!(report.rows.source, RowSource::View);
        assert_eq!(report.pick_rows.source, PickRowSource::Store);
        assert_eq!(report.rows.parsed, report.pick_rows.count);

        // Drift: edit the view after the import.
        let path = dir.join("conductor.md");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("\n<!-- edited -->\n");
        std::fs::write(&path, text).unwrap();
        crate::registry_store::reset_drift_warnings_for_tests();
        let report = super::report(&rec, true);
        assert_eq!(report.decision.reason, Some(SkipReason::RegistryDrift));
        assert_eq!(report.registry.state, RegistryState::Drift);
        assert_eq!(report.rows.source, RowSource::View);
        assert_eq!(report.pick_rows.source, PickRowSource::Store);
    }

    #[test]
    fn pick_row_version_prefers_an_eligible_copy() {
        let ready = TrackRow {
            id: "0050".into(),
            slug: "Epsilon".into(),
            status_raw: READY_EM.into(),
            summary: String::new(),
            nostart: false,
            execution: None,
        };
        let blocked = TrackRow {
            status_raw: "**Blocked**".into(),
            ..ready.clone()
        };
        let version = pick_row_version(
            "0050",
            std::slice::from_ref(&blocked),
            &[blocked.clone(), ready.clone()],
        )
        .unwrap();
        assert_eq!(version.in_pick_rows.as_deref(), Some("Ready - not started"));
        assert!(!version.picked_but_ineligible);

        let version = pick_row_version("0050", &[ready], &[blocked]).unwrap();
        assert_eq!(version.in_rows.as_deref(), Some("Ready - not started"));
        assert_eq!(version.in_pick_rows.as_deref(), Some("Blocked"));
        assert!(version.picked_but_ineligible);
    }

    #[test]
    fn classify_pick_error_round_trips() {
        let aliases = vec!["Ready — not started".to_string()];
        let benign = CoordinatorError::Message(
            "no Ready — not started track in conductor.md; pass --track <id>".into(),
        );
        assert_eq!(
            classify_pick_error(&benign, None, &[], &aliases).0,
            SkipReason::NoReadyRow
        );
        let benign_alias = CoordinatorError::Message(
            "no eligible Ready track (aliases: Ready — not started); pass --track <id>".into(),
        );
        assert_eq!(
            classify_pick_error(&benign_alias, None, &[], &aliases).0,
            SkipReason::NoReadyRow
        );
        let missing =
            CoordinatorError::Message("no conductor.md at C:\\x; pass --track <id>".into());
        assert_eq!(
            classify_pick_error(&missing, None, &[], &aliases).0,
            SkipReason::RegistryMissing
        );
        let no_dir = CoordinatorError::Message(
            "Ready track(s) 0001 have no matching conductor directory; pass --track <id>".into(),
        );
        assert_eq!(
            classify_pick_error(&no_dir, None, &[], &aliases).0,
            SkipReason::NoTrackDir
        );
        let drift = CoordinatorError::Message(format!(
            "{} (store a, view b); run `coordinator registry import`",
            crate::registry_store::VIEW_DRIFT_PREFIX
        ));
        assert_eq!(
            classify_pick_error(&drift, None, &[], &aliases).0,
            SkipReason::RegistryDrift
        );
        let io = CoordinatorError::Io(std::io::Error::other("transient"));
        assert_eq!(
            classify_pick_error(&io, None, &[], &aliases).0,
            SkipReason::TransientRead
        );
        let store = CoordinatorError::Message("bad store".into());
        let same = CoordinatorError::Message("bad store".into());
        assert_eq!(
            classify_pick_error(&same, Some(&store), &[], &aliases).0,
            SkipReason::RegistryInvalid
        );
    }

    // ---- CLI / status surfaces -------------------------------------------

    #[test]
    fn status_project_attaches_ready_and_status_all_omits_it() {
        let _guard = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        write_gate(home.path(), false);
        let ws = tempdir().unwrap();
        let rec = base_record(ws.path());
        write_md(ws.path(), &ready_md("0020"));
        track_dir(ws.path(), "0020-Fixture");
        register(std::slice::from_ref(&rec));

        let with_ready = crate::api::status_with_ready(Some(&rec.id), false).unwrap();
        assert!(with_ready.ready.is_some());
        let http_ui = crate::api::status(Some(&rec.id), false).unwrap();
        assert!(http_ui.ready.is_none());
        let all = crate::api::status_all().unwrap();
        assert!(all.iter().all(|view| view.ready.is_none()));
        let json = serde_json::to_string(&all).unwrap();
        assert!(!json.contains("\"ready\""));
    }

    #[test]
    fn status_ready_agrees_with_features_and_policy() {
        let _guard = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        write_gate(home.path(), false);
        let ws = tempdir().unwrap();
        let rec = base_record(ws.path());
        write_md(ws.path(), &ready_md("0020"));
        track_dir(ws.path(), "0020-Fixture");
        register(std::slice::from_ref(&rec));

        let view = crate::api::status_with_ready(Some(&rec.id), false).unwrap();
        let ready = view.ready.as_ref().expect("ready attached");
        assert_eq!(ready.policy, view.auto_start);
        let duty = view
            .features
            .as_ref()
            .unwrap()
            .duties
            .get("auto_start")
            .unwrap();
        assert_eq!(ready.feature.enabled, duty.enabled);
        assert_eq!(ready.ticker_effective, TickerEffective::FeatureOff);
        assert_eq!(ready.probe, "none");
    }

    #[test]
    fn status_with_ready_resolves_features_once() {
        let _guard = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        write_gate(home.path(), false);
        let ws = tempdir().unwrap();
        let rec = base_record(ws.path());
        write_md(ws.path(), &ready_md("0020"));
        track_dir(ws.path(), "0020-Fixture");
        register(std::slice::from_ref(&rec));

        crate::features::reset_resolve_all_calls();
        let _ = crate::api::status_with_ready(Some(&rec.id), false).unwrap();
        assert_eq!(crate::features::resolve_all_calls(), 1);
    }

    #[test]
    fn ready_all_resolves_features_once() {
        let _guard = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        write_gate(home.path(), false);
        let ws1 = tempdir().unwrap();
        let rec1 = base_record(ws1.path());
        write_md(ws1.path(), &ready_md("0020"));
        track_dir(ws1.path(), "0020-Fixture");
        let ws2 = tempdir().unwrap();
        let rec2 = base_record(ws2.path());
        write_md(ws2.path(), &ready_md("0030"));
        track_dir(ws2.path(), "0030-Fixture");
        register(&[rec1, rec2]);

        crate::features::reset_resolve_all_calls();
        let fleet = crate::api::ready_all(true).unwrap();
        assert_eq!(crate::features::resolve_all_calls(), 1);
        assert_eq!(fleet.projects.len(), 2);
    }

    #[test]
    fn ready_cli_prints_a_report_and_exits_zero() {
        let _guard = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        write_gate(home.path(), false);
        let ws = tempdir().unwrap();
        let rec = base_record(ws.path());
        write_md(ws.path(), &ready_md("0020"));
        track_dir(ws.path(), "0020-Fixture");
        register(std::slice::from_ref(&rec));
        let last_used = home.path().join("last-used.json");
        assert!(!last_used.exists());

        let report = crate::api::ready(Some(&rec.id), false, true).unwrap();
        let json = serde_json::to_string_pretty(&report).unwrap();
        let parsed: ReadyReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.decision.kind, report.decision.kind);
        assert!(
            !last_used.exists(),
            "read-only ready must not write last-used.json"
        );
    }

    #[test]
    fn ready_all_covers_every_registered_project() {
        let _guard = test_env_lock();
        let _env = env_guard();
        let home = tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        write_gate(home.path(), true);
        let ws1 = tempdir().unwrap();
        let rec1 = base_record(ws1.path());
        write_md(ws1.path(), &ready_md("0020"));
        track_dir(ws1.path(), "0020-Fixture");
        let ws2 = tempdir().unwrap();
        let rec2 = base_record(ws2.path());
        let dir2 = conductor(ws2.path());
        std::fs::create_dir_all(&dir2).unwrap();
        std::fs::write(dir2.join("track-registry.json"), b"{ not a store").unwrap();
        register(&[rec1, rec2]);

        let before = (tree(home.path()), tree(ws1.path()), tree(ws2.path()));
        let fleet = crate::api::ready_all(true).unwrap();
        assert_eq!(fleet.projects.len(), 2);
        let invalid = fleet
            .projects
            .iter()
            .find(|p| p.registry.state == RegistryState::Invalid)
            .expect("invalid project");
        assert_eq!(invalid.decision.reason, Some(SkipReason::RegistryInvalid));
        assert_eq!(
            before,
            (tree(home.path()), tree(ws1.path()), tree(ws2.path()))
        );
    }
}
