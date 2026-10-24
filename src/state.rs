//! Per-project run-state persistence under `.coordinator/`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::state_dir_override;
use crate::error::Result;
use crate::outcome::FailureClass;
use crate::persist::atomic_write_json;
use crate::registry::ProjectRecord;

/// In-phase self-check snap (track 0070). Absent on old run-state files.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SelfCheckState {
    #[serde(default)]
    pub steps: u32,
    #[serde(default)]
    pub pending_inject: bool,
    #[serde(default)]
    pub repairs_in_a_row: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_finding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_diff_fp: Option<String>,
    #[serde(default)]
    pub last_journal_lines: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_started_at: Option<DateTime<Utc>>,
}

/// Run lifecycle status (stub phases + outcome-driven completion in 0005).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum RunStatus {
    Idle,
    Running,
    Paused,
    Stopped,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Running => "Running",
            Self::Paused => "Paused",
            Self::Stopped => "Stopped",
        }
    }
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// ADR-0024 stop note (persisted in run-state / status JSON).
pub const STOP_LAST_EVENT: &str = "stopped: no merge; sessions left for attach";

/// Default stub phase while Running/Paused.
pub const STUB_PHASE_ACTIVE: &str = "stub:active";

/// Default phase when Idle (no active run).
pub const STUB_PHASE_IDLE: &str = "stub:idle";

/// Phase after operator Stop (not outcome failure).
pub const STUB_PHASE_STOPPED: &str = "stub:stopped";

/// Phase after successful outcome apply (single stub leftover).
pub const STUB_PHASE_COMPLETED: &str = "stub:completed";

/// Phase after failure outcome apply (including timeout).
pub const STUB_PHASE_FAILED: &str = "stub:failed";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunState {
    pub project_id: String,
    pub status: RunStatus,
    /// Stub phase string; advanced by Phase Outcome apply (0005), not by wall clock alone.
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_id: Option<String>,
    pub updated_at: DateTime<Utc>,
    pub last_event: String,
    /// Monotonic epoch; incremented each successful `run` into Running.
    #[serde(default)]
    pub run_epoch: u64,
    /// 0097: consecutive `fold -> plan-review` re-entries this run (capped). A fold that
    /// rewrites a reviewed artifact must be re-reviewed, but a fold that always rewrites
    /// must not spin: the run stops with `review_never_settles` at the cap.
    #[serde(default)]
    pub fold_review_reentries: u32,
    /// Option (b) part 2 (2026-10-20): the last spec/plan pair observed at a fold
    /// re-entry. A re-entry counts against [`MAX_FOLD_REVIEW_REENTRIES`] only when it
    /// reproduces this identical pair; a pair that changed since the previous re-entry
    /// is a converging rewrite and resets the count instead of marching toward the cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fold_last_pair: Option<(String, String)>,
    /// When the current phase clock started (Running entry / fresh run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_started_at: Option<DateTime<Utc>>,
    /// Accumulated pause duration for the current phase (timeout freeze).
    #[serde(default)]
    pub total_paused_ms: u64,
    /// When the current Pause began (None if not paused).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_started_at: Option<DateTime<Utc>>,
    /// Last applied failure class (cleared on success / fresh run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<FailureClass>,
    /// Planner handoff from outcome `metadata.next_track`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_track: Option<String>,
    /// Parked advance candidate (0044). Cleared on the next successful start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked_next: Option<String>,
    /// Content hash of last successfully applied outcome (consume marker).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_applied_outcome_hash: Option<String>,
    /// `"canonical_v1"` when started by public `run`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// Driver chosen at `run` (serde default `adapter`).
    #[serde(default)]
    pub driver: crate::workflow::WorkflowDriver,
    /// Slugs still open in `plan-review`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_roles: Vec<String>,
    /// Tick inject-once marker (`run_epoch` + phase).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_driven_phase: Option<String>,
    /// Token-idle CI watch (0010). Cleared on fresh `run`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci: Option<CiWatchState>,
    /// Cross-model review gate (0011). Cleared on fresh `run`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewWatchState>,
    /// First time the progress watchdog fired this inject (0026). Cleared on resume / fresh run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stalled_at: Option<DateTime<Utc>>,
    /// Completed pause intervals for this phase (stall idle window; 0026).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pause_spans: Vec<PauseSpan>,
    /// Stall recycles this phase (0027). Cap 1. Not cleared by the recycle itself.
    #[serde(default)]
    pub stall_recycles: u32,
    /// 0104: owner-page ownership for the current/most recent stall key.
    #[serde(default, skip_serializing_if = "StallPagingState::is_idle")]
    pub stall_paging: StallPagingState,
    /// Session id being recycled; apply skips fallout from this id only (0027).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aborted_session_id: Option<String>,
    /// ACP stdout-close restarts this phase (0064). Cap 2. Not a Status JSON key.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub acp_stdout_retries: u32,
    /// Plan-review one-shot slots already launched this phase (0017). Additive.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plan_review_spawned: Vec<String>,
    /// Join-level empty/dud re-arms this phase (0054). Cap 1. Not a Status JSON key.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub plan_review_join_retries: u32,
    /// Slugs whose child actually ran `backend.run` this phase (0054). Additive.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plan_review_slot_ran: Vec<String>,
    /// 0119: the persisted plan-review entry identity.
    ///
    /// `(run_epoch, entry_id)` is minted on every transition **into** `plan-review`, survives the
    /// phase exit (monotonicity), and is adopted - never re-minted - when a process resumes a
    /// phase already in flight. Absent on pre-0119 files (`None`), which adopt `{run_epoch, 1}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_review_entry: Option<PlanReviewEntry>,
    /// 0119: per-slug, entry-scoped degraded cause (`ran; dud: ...`, `failed: ...`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plan_review_verdicts: BTreeMap<String, String>,
    /// 0119: per-slug synchronous admission failures (no child ever started).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plan_review_admission_failures: BTreeMap<String, String>,
    /// Address-findings entries this `run_epoch` (0031). Cap 2. Always serialized on disk.
    #[serde(default)]
    pub address_findings_attempts: u32,
    /// Operator-Ready ids captured at `run` (0055). Overlay pick; not a Status JSON key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sticky_ready_ids: Vec<String>,
    /// Branch `HEAD` named when this epoch's checkpoint ref was first created (0067).
    /// `None` when that `HEAD` was detached. Not rewritten when the ref already exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_branch: Option<String>,
    /// Per-track committed failure count (0069). Fresh `run` does not clear it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub consecutive_failures: BTreeMap<String, u32>,
    /// Registry status cells captured before an `advance`/`fold` inject (0088 DoD-1).
    /// `id -> status cell`. `on_success` clears this in-memory map when the helper
    /// reports the snapshot consumed (0090). `begin_fresh_run` clears it on a new epoch.
    /// Used to undo a non-terminal `Ready` row downgraded by the session.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub registry_status_before: BTreeMap<String, String>,
    /// `run_epoch` captured by a successful operator restore (0069). Cleared on publish/merge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restored_epoch: Option<u64>,
    /// Fingerprints accepted by `policy approve` (0069). Fresh `run` does not clear them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub policy_approvals: Vec<crate::policy::PolicyApproval>,
    /// Latest state-policy alert (0069). Cleared on fresh `run` and on Allow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_gate: Option<crate::policy::StateGate>,
    /// Bounded self-check snap (0070). Cleared on fresh `run`. Omitted when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_check: Option<SelfCheckState>,
    /// CI-gate repair entries this `run_epoch` (0071). Cap 2. Always serialized on disk.
    #[serde(default)]
    pub ci_fix_attempts: u32,
    /// Payload for the current `address-ci` inject (0071). Omitted when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_fix_request: Option<CiFixRequest>,
    /// Review-comment repair entries this `run_epoch` (0078). Cap 2. Always serialized on disk.
    #[serde(default)]
    pub review_fix_attempts: u32,
    /// Payload for the current `address-review` inject (0078). Omitted when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_fix_request: Option<ReviewFixRequest>,
    /// Review route latch and resolution stamp (0078). Omitted when no poll has been examined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_route: Option<ReviewRouteState>,
    /// 0101: the frozen delivery target of the most recent delivery leg.
    ///
    /// Written at checkpoint/inject, compared before overwrite, and cleared on a fresh
    /// epoch. A legacy file loads with `None`; gates then re-resolve and validate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_target: Option<crate::workflow::execution_repo::ResolvedExecution>,
    /// 0101: one persisted memo for the bounded `gate-target:` journal line.
    /// Cleared with `delivery_target`. Never gates a mutation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_target_journal: Option<String>,
}

/// 0104: owner-page ownership for the current/most recent stall key.
///
/// Durable dedupe marker for `(run_epoch, track_id)`. The paging module owns its
/// lifecycle; `run.rs` never touches it (a fresh epoch must be observable so the
/// old page can be closed). Omitted from JSON while idle.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StallPagingState {
    /// `run_epoch` whose begin was claimed (written before the POST).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paged_epoch: Option<u64>,
    /// `track_id` at begin (`None` = no track id). Part of the dedupe key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paged_track: Option<String>,
    /// Phase at begin. A later phase is a clear, not a re-begin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paged_phase: Option<String>,
    /// Claim time; also the "progress since the page" watermark.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paged_at: Option<DateTime<Utc>>,
    /// The matching resolution was claimed for this key (at most one).
    #[serde(default)]
    pub cleared: bool,
}

impl StallPagingState {
    /// No begin has ever been claimed for the current/most recent key.
    pub fn is_idle(&self) -> bool {
        self.paged_epoch.is_none()
    }
}

/// One completed pause interval (start inclusive, end exclusive-ish).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PauseSpan {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

/// Persisted `cross-model-review` watcher (additive; old run-state.json loads as None).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewWatchState {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempted: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    /// `PASS` | `PASS_WITH_LOWS` | `FAIL` | `DEGRADED_PUBLISH`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    /// Relative name e.g. `review.codex.md`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<String>,
    /// Execution-repo `HEAD` when `verdict` was persisted (0096). `ci-wait` labels only on a match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewed_oid: Option<String>,
    /// One-shot slugs killed by reviewer stall (0036). Never `RunState.stalled_at`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stalled: Vec<String>,
    /// Slot-quality duds (`file (bytes)`). Additive; empty omits from JSON.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub duds: Vec<String>,
}

/// Persisted `ci-wait` watcher (additive; old run-state.json loads as None).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CiWatchState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_poll_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_summary: Option<String>,
    /// `"done"` | `"skipped"` | `"queued"`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge: Option<String>,
    /// Fingerprint of last check/run set (interval reset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_key: Option<String>,
    /// Interval to wait before the next spawn (ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_interval_ms: Option<u64>,
    /// HEAD SHA of the last auto-publish spawn (0053). Not a Status JSON key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_attempted_sha: Option<String>,
    /// Transient publish misses for `publish_transient_sha` (0082). Old files load as 0.
    #[serde(default)]
    pub publish_transient_attempts: u32,
    /// HEAD the transient counter is counting. Absent on old files and after `Opened`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_transient_sha: Option<String>,
    /// Transient unresolved-merge-tip misses for `merge_tip_transient_sha`. Old files load as 0.
    /// A merge tip that has not arrived yet is a read that failed, not a policy violation (0092).
    #[serde(default)]
    pub merge_tip_transient_attempts: u32,
    /// HEAD the merge-tip counter is counting. Absent on old files and after `Opened`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_tip_transient_sha: Option<String>,
    /// 0101: the delivery repo latched on the first ci-wait tick. Never rewritten to
    /// follow a later row edit; publish authority only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_repo: Option<PathBuf>,
    /// 0078: the `address-review` commit that still has to be pushed by the engine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_push_pending: Option<String>,
    /// 0078: the actual pushed tip. Cleared when the PR head matches it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_pushed_sha: Option<String>,
    /// 0078: consecutive failed review-push attempts (cap 3).
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub review_push_attempts: u32,
    /// 0078: first tick that waited for the pushed head (10 minute bound).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_push_waiting_since: Option<DateTime<Utc>>,
    /// 0125: the `address-ci` commit that still has to be pushed by the engine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_fix_push_pending: Option<String>,
    /// 0125: the actual pushed tip. Cleared when the PR head matches it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_fix_pushed_sha: Option<String>,
    /// 0125: consecutive failed ci-fix push attempts (cap 3). Not `ci_fix_attempts`.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub ci_fix_push_attempts: u32,
    /// 0125: first tick that waited for the pushed CI-fix head (10 minute bound).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_fix_push_waiting_since: Option<DateTime<Utc>>,
}

/// One failing check from the judged slice handed to `address-ci` (0071).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CiFixCheck {
    pub name: String,
    pub bucket: String,
    pub description: String,
    pub link: String,
}

/// Durable `address-ci` payload (0071). `from_sha` is the PR head at route time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CiFixRequest {
    pub pr_number: u64,
    pub from_sha: String,
    pub checks: Vec<CiFixCheck>,
}

/// Durable `address-review` payload (0078). `from_sha` is the PR head at route time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewFixRequest {
    pub pr_number: u64,
    pub from_sha: String,
    pub comments: Vec<ReviewComment>,
}

/// One rendered/latched review item (0078). `body` is sanitized and bounded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewComment {
    pub key: String,
    pub kind: String,
    pub author: String,
    pub path: String,
    pub line: Option<u32>,
    pub body: String,
    pub url: String,
    pub updated_at: DateTime<Utc>,
}

/// 0078 route dedupe and poll stamp. `routed` is a route latch, not the merge gate;
/// `last_result == Clean` plus zero open items is the merge gate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewRouteState {
    pub pr_number: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routed: Vec<RoutedReviewItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_poll_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_result: Option<ReviewPollResult>,
    /// Consecutive `ReadFailed` polls only; every completed read resets to 0.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub read_failures: u32,
}

/// One latched review item. `source_*` is route-time evidence and is never refreshed
/// by a later observation; a later edit is a new round.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoutedReviewItem {
    pub key: String,
    pub round: u32,
    pub routed_at: DateTime<Utc>,
    pub source_comment_id: String,
    pub source_updated_at: DateTime<Utc>,
}

/// Outcome of one bounded review poll.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewPollResult {
    Clean,
    Open,
    Truncated,
    RateLow,
    ReadFailed,
    Foreign,
}

impl RunState {
    pub fn idle(project_id: impl Into<String>) -> Self {
        Self {
            project_id: project_id.into(),
            status: RunStatus::Idle,
            phase: STUB_PHASE_IDLE.into(),
            track_id: None,
            updated_at: Utc::now(),
            last_event: "idle: no run".into(),
            run_epoch: 0,
            fold_review_reentries: 0,
            fold_last_pair: None,
            phase_started_at: None,
            total_paused_ms: 0,
            pause_started_at: None,
            failure_class: None,
            next_track: None,
            parked_next: None,
            last_applied_outcome_hash: None,
            workflow: None,
            driver: crate::workflow::WorkflowDriver::default(),
            pending_roles: Vec::new(),
            last_driven_phase: None,
            ci: None,
            review: None,
            stalled_at: None,
            registry_status_before: BTreeMap::new(),
            pause_spans: Vec::new(),
            stall_recycles: 0,
            stall_paging: StallPagingState::default(),
            aborted_session_id: None,
            acp_stdout_retries: 0,
            plan_review_spawned: Vec::new(),
            plan_review_join_retries: 0,
            plan_review_slot_ran: Vec::new(),
            plan_review_entry: None,
            plan_review_verdicts: BTreeMap::new(),
            plan_review_admission_failures: BTreeMap::new(),
            address_findings_attempts: 0,
            sticky_ready_ids: Vec::new(),
            checkpoint_branch: None,
            consecutive_failures: BTreeMap::new(),
            restored_epoch: None,
            policy_approvals: Vec::new(),
            state_gate: None,
            self_check: None,
            ci_fix_attempts: 0,
            ci_fix_request: None,
            review_fix_attempts: 0,
            review_fix_request: None,
            review_route: None,
            delivery_target: None,
            gate_target_journal: None,
        }
    }

    /// Effective running elapsed for timeout (excludes paused intervals).
    ///
    /// While currently Paused, the open pause interval is also excluded (timer frozen).
    pub fn effective_running_elapsed(&self, now: DateTime<Utc>) -> Duration {
        let Some(started) = self.phase_started_at else {
            return Duration::ZERO;
        };
        let wall = (now - started).num_milliseconds().max(0) as u64;
        let mut paused = self.total_paused_ms;
        if let Some(pstart) = self.pause_started_at {
            paused = paused.saturating_add((now - pstart).num_milliseconds().max(0) as u64);
        }
        Duration::from_millis(wall.saturating_sub(paused))
    }

    /// 0119: mint the entry for a transition **into** `plan-review` and clear its books.
    ///
    /// `entry_id` is previous same-epoch id + 1, else 1. Only a real transition calls this; an
    /// in-flight phase is adopted, never re-minted.
    pub fn begin_plan_review_entry(&mut self) {
        let entry_id = match self.plan_review_entry {
            Some(e) if e.run_epoch == self.run_epoch => e.entry_id.saturating_add(1),
            _ => 1,
        };
        self.plan_review_entry = Some(PlanReviewEntry {
            run_epoch: self.run_epoch,
            entry_id,
        });
        self.clear_plan_review_books();
        // A stall from an earlier entry must not suppress this entry's retry. `review.duds` stays
        // diagnostic (it is not a join cause).
        let slugs = crate::workflow::graph::review_slugs();
        if let Some(rv) = self.review.as_mut() {
            rv.stalled.retain(|s| !slugs.contains(&s.as_str()));
        }
    }

    /// 0119: adopt the in-flight entry for a phase that is already `plan-review`.
    ///
    /// Keeps a same-epoch entry; otherwise starts `{run_epoch, 1}`. **Never** clears the books,
    /// verdicts or admission failures: a mid-phase resume must not re-spawn live children.
    ///
    /// This assumes the kept entry is exactly the in-flight one. Production sets the phase only
    /// through `on_success`, so a same-epoch retained entry can only be that entry; a hand-edited
    /// state that keeps an older same-epoch id is not a supported state.
    pub fn adopt_plan_review_entry(&mut self) {
        if !matches!(self.plan_review_entry, Some(e) if e.run_epoch == self.run_epoch) {
            self.plan_review_entry = Some(PlanReviewEntry {
                run_epoch: self.run_epoch,
                entry_id: 1,
            });
        }
    }

    /// 0119: clear the entry's books on the way out, keeping the entry for monotonicity.
    ///
    /// The retained entry is inert because [`Self::plan_review_entry_current`] also requires the
    /// phase to be `plan-review`.
    pub fn leave_plan_review(&mut self) {
        self.clear_plan_review_books();
    }

    /// 0119: clear the plan-review books, verdicts and admission records only.
    pub fn clear_plan_review_books(&mut self) {
        self.plan_review_spawned.clear();
        self.plan_review_slot_ran.clear();
        self.plan_review_join_retries = 0;
        self.plan_review_verdicts.clear();
        self.plan_review_admission_failures.clear();
    }

    /// 0119: the read fence. `Some` only for the live epoch while the phase is `plan-review`.
    pub fn plan_review_entry_current(&self) -> Option<PlanReviewEntry> {
        self.plan_review_entry.filter(|e| {
            e.run_epoch == self.run_epoch && self.phase == crate::workflow::graph::PHASE_PLAN_REVIEW
        })
    }

    /// 0119: spawn ledger for the current entry (empty when no entry is live).
    pub fn plan_review_spawned_now(&self) -> &[String] {
        if self.plan_review_entry_current().is_some() {
            &self.plan_review_spawned
        } else {
            &[]
        }
    }

    /// 0119: ran ledger for the current entry (empty when no entry is live).
    pub fn plan_review_slot_ran_now(&self) -> &[String] {
        if self.plan_review_entry_current().is_some() {
            &self.plan_review_slot_ran
        } else {
            &[]
        }
    }

    /// 0119: join retry counter for the current entry (0 when no entry is live).
    pub fn plan_review_join_retries_now(&self) -> u32 {
        if self.plan_review_entry_current().is_some() {
            self.plan_review_join_retries
        } else {
            0
        }
    }

    /// 0119: this entry's recorded verdict for `slug`, if any.
    pub fn plan_review_verdict(&self, slug: &str) -> Option<&str> {
        self.plan_review_entry_current()?;
        self.plan_review_verdicts.get(slug).map(String::as_str)
    }

    /// 0119: this entry's synchronous admission failure for `slug`, if any.
    pub fn plan_review_admission_failure(&self, slug: &str) -> Option<&str> {
        self.plan_review_entry_current()?;
        self.plan_review_admission_failures
            .get(slug)
            .map(String::as_str)
    }
}

/// One adapter plan-review entry: the identity of the phase attempt that owns every book.
///
/// Persisted in `run-state.json`, minted on entry into `plan-review`, **never reset within an
/// epoch**, and adopted on resume. A worker from an earlier `entry_id` cannot stamp, write a
/// review, or write a role outcome into the live entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanReviewEntry {
    pub run_epoch: u64,
    pub entry_id: u64,
}

/// Status JSON fields (CLI + GET /v1/status). Additive layout fields from 0006.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusView {
    pub project_id: String,
    pub path: PathBuf,
    /// Copied from `ProjectRecord` so cards do not join the registry again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub status: RunStatus,
    pub phase: String,
    pub track_id: Option<String>,
    pub last_event: String,
    #[serde(default)]
    pub run_epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<FailureClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_track: Option<String>,
    /// Copied from `ProjectRecord` (0044). Missing JSON = hitl. Always serialized.
    #[serde(default)]
    pub auto_start: crate::registry::AutoStartPolicy,
    /// Parked advance candidate (0044). Omitted when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked_next: Option<String>,
    /// Layout profile (nested | multi_sibling | single_root).
    #[serde(default)]
    pub layout_profile: crate::layout::LayoutProfile,
    /// Resolved primary execution repo (`null` when nested and unknown).
    #[serde(default)]
    pub execution_repo: Option<PathBuf>,
    /// Resolved conductor directory.
    #[serde(default)]
    pub conductor_dir: Option<PathBuf>,
    /// 0101: the frozen delivery target copied from `RunState` (no I/O).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_target: Option<crate::workflow::execution_repo::ResolvedExecution>,
    /// Additive harness summary (`harness.grok`) when a session exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<crate::harness::HarnessStatusBundle>,
    /// Additive workflow object (`id`, `driver`, `pending_roles`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<WorkflowView>,
    /// Path to `{state_dir}/FAILURE.md` when present; `null` when absent.
    #[serde(default)]
    pub failure_artifact: Option<PathBuf>,
    /// Read-time SUPERSEDED reason (0040). Omitted when the failure is live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_superseded: Option<String>,
    /// Token-idle CI watch; `null` when phase ≠ `ci-wait` and no persisted state.
    #[serde(default)]
    pub ci: Option<CiStatusView>,
    /// Cross-model review; `null` when phase ≠ `cross-model-review` and no persisted state.
    #[serde(default)]
    pub review: Option<ReviewStatusView>,
    /// Last adapter heartbeat (`harness-progress.json`); omitted when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_progress_at: Option<DateTime<Utc>>,
    /// Progress stall (0026). `null` when not stalled. Run stays Running.
    #[serde(default)]
    pub stall: Option<StallView>,
    /// Who ticks the machine. Filled only at `api::status` / `status_all` / `cmd_run_cli`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticker: Option<TickerView>,
    /// Machine feature gates (0102). Omitted on the poll path, like `ticker`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub features: Option<crate::features::FeatureSnapshot>,
    /// Contextual state-policy alert (0069). Omitted when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_gate: Option<crate::policy::StateGate>,
    /// Bounded self-check snap (0070). Omitted when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_check: Option<SelfCheckState>,
    /// 0109: track store vs generated view state (`ok|drift|invalid|absent`). Filled by
    /// `api::status` (HTTP single-project route and UI Run precheck) and by the CLI
    /// `status` path (`api::status_with_ready`); the poll path leaves it `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<RegistryView>,
    /// 0115: effective ready policy, duty gate and pick decision. CLI `status
    /// --project` only (`api::status_with_ready`); `api::status` (HTTP/UI),
    /// `status_all`, and the poll path omit it (0102/0109 precedent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready: Option<ReadyStatusView>,
    /// 0117: derived, read-only holder lifecycle (files only, no RPC, no lock).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<crate::harness::reap::HolderLifecycle>,
}

/// 0109 status evidence for a project whose registry may have a structured store.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegistryView {
    /// `ok` | `drift` | `invalid` | `absent`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<String>,
}

/// 0115: trimmed ready projection for CLI `status --project`.
///
/// Carries no row list; `coordinator ready` is the full report. `policy`,
/// `feature` and `ticker_effective` come from the same `FeatureSnapshot` the
/// view already carries, so one JSON cannot disagree with itself.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadyStatusView {
    pub policy: crate::registry::AutoStartPolicy,
    pub feature: crate::workflow::readiness::FeatureGate,
    pub ticker_effective: crate::workflow::readiness::TickerEffective,
    pub operator_omit_pick: crate::workflow::readiness::OperatorPick,
    /// Always `"none"` (the preview is probeless).
    pub probe: String,
    pub pickable: usize,
    pub decision: crate::workflow::readiness::ReadyDecision,
}

/// Status JSON `ticker` object (0023). `owner` is `serve` or `none` — never `cli`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TickerView {
    pub owner: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

/// Status JSON `stall` object (0026).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StallView {
    pub since: DateTime<Utc>,
    pub idle_secs: u64,
}

/// Status JSON `review` object (0011).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewStatusView {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempted: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stalled: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub duds: Vec<String>,
}

/// Status JSON `ci` object (0010).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CiStatusView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_summary: Option<String>,
    pub interval_ms: u64,
    pub auto_merge: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge: Option<String>,
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

/// Status JSON `workflow` object (0008).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub driver: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_roles: Vec<String>,
    /// Omitted on happy-path JSON when 0.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub address_findings_attempts: u32,
    /// Omitted on happy-path JSON when 0.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub ci_fix_attempts: u32,
    /// Review-comment repair entries this epoch. Omitted when 0.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub review_fix_attempts: u32,
}

impl StatusView {
    pub fn from_record(record: &ProjectRecord, state: &RunState) -> Self {
        let paths = crate::layout::resolve(record);
        Self {
            project_id: record.id.clone(),
            path: record.path.clone(),
            display_name: record.display_name.clone(),
            status: state.status,
            phase: state.phase.clone(),
            track_id: state.track_id.clone(),
            last_event: state.last_event.clone(),
            run_epoch: state.run_epoch,
            phase_started_at: state.phase_started_at,
            failure_class: state.failure_class,
            next_track: state.next_track.clone(),
            auto_start: record.auto_start,
            parked_next: state.parked_next.clone(),
            layout_profile: record.layout_profile,
            execution_repo: paths.execution_repo,
            conductor_dir: Some(paths.conductor_dir),
            delivery_target: state.delivery_target.clone(),
            harness: crate::harness::status_bundle_sync(record),
            workflow: Some(WorkflowView {
                id: state.workflow.clone(),
                driver: state.driver.as_str().to_string(),
                pending_roles: state.pending_roles.clone(),
                address_findings_attempts: state.address_findings_attempts,
                ci_fix_attempts: state.ci_fix_attempts,
                review_fix_attempts: state.review_fix_attempts,
            }),
            failure_artifact: crate::notify::artifact::existing_path(record),
            failure_superseded: failure_superseded_for(record, state),
            ci: ci_status_view(record, state),
            review: review_status_view(state),
            last_progress_at: if state.status == RunStatus::Running {
                crate::workflow::watchdog::last_progress_at(record)
            } else {
                None
            },
            stall: stall_status_view(record, state),
            ticker: None,
            features: None,
            state_gate: state.state_gate.clone(),
            self_check: state.self_check.clone(),
            registry: None,
            ready: None,
            holder: Some(holder_lifecycle(record, state)),
        }
    }
}

/// 0117 `status.holder`: the one file-only gate list, computed read-only. No lock and no
/// socket I/O — `candidate` is pre-RPC eligibility, never an identity verdict.
fn holder_lifecycle(
    record: &ProjectRecord,
    state: &RunState,
) -> crate::harness::reap::HolderLifecycle {
    let state_present = crate::state::run_state_path(record).is_ok_and(|path| path.exists());
    // `gate_direct`, not `is_enabled`: a status render may already have resolved (or
    // will resolve) the machine snapshot, and 0115's one-snapshot-per-status budget
    // must not be consumed twice.
    let gate_on = crate::features::gate_direct(crate::features::Duty::SelfReap);
    crate::harness::reap::decide(record, Utc::now(), state, state_present, gate_on).lifecycle
}

fn failure_superseded_for(record: &ProjectRecord, state: &RunState) -> Option<String> {
    if state.failure_class.is_none() && crate::notify::artifact::existing_path(record).is_none() {
        return None;
    }
    let body = crate::notify::artifact::read(record)
        .ok()
        .flatten()
        .map(|s| s.body);
    crate::workflow::conductor_md::failure_superseded(record, state, body.as_deref())
}

fn stall_status_view(record: &ProjectRecord, state: &RunState) -> Option<StallView> {
    if state.status != RunStatus::Running {
        return None;
    }
    let since = state.stalled_at?;
    let last = crate::workflow::watchdog::last_progress_at(record)
        .or(state.phase_started_at)
        .unwrap_or(since);
    let idle = crate::workflow::watchdog::idle_since(last, state, Utc::now());
    Some(StallView {
        since,
        idle_secs: idle.as_secs(),
    })
}

fn ci_status_view(record: &ProjectRecord, state: &RunState) -> Option<CiStatusView> {
    if state.phase != crate::workflow::graph::PHASE_CI_WAIT && state.ci.is_none() {
        return None;
    }
    let interval_ms = state
        .ci
        .as_ref()
        .and_then(|c| c.next_interval_ms)
        .unwrap_or_else(crate::ci::initial_interval_ms);
    let (pr, pr_url, head_sha, last_summary, merge) = match state.ci.as_ref() {
        Some(c) => (
            c.pr_number,
            c.pr_url.clone(),
            c.head_sha.clone(),
            c.last_summary.clone(),
            c.merge.clone(),
        ),
        None => (None, None, None, None, None),
    };
    Some(CiStatusView {
        pr,
        pr_url,
        head_sha,
        last_summary,
        interval_ms,
        auto_merge: record.auto_merge,
        merge,
    })
}

fn review_status_view(state: &RunState) -> Option<ReviewStatusView> {
    if state.phase != crate::workflow::graph::PHASE_CROSS_MODEL && state.review.is_none() {
        return None;
    }
    let (attempted, active, verdict, report, stalled, duds) = match state.review.as_ref() {
        Some(r) => (
            r.attempted.clone(),
            r.active.clone(),
            r.verdict.clone(),
            r.report.clone(),
            r.stalled.clone(),
            r.duds.clone(),
        ),
        None => (Vec::new(), None, None, None, Vec::new(), Vec::new()),
    };
    Some(ReviewStatusView {
        attempted,
        active,
        verdict,
        report,
        stalled,
        duds,
    })
}

/// Resolve state directory for a project.
///
/// Priority:
/// 1. `COORDINATOR_STATE_DIR` env → `{override}/{project_id}/` (keyed per project;
///    avoids multi-project collisions when the env override is shared)
/// 2. `record.state_dir` (already project-specific)
/// 3. `{workspace_path}/.coordinator`
///
/// Empty `COORDINATOR_STATE_DIR` is an error.
pub fn resolve_state_dir(record: &ProjectRecord) -> Result<PathBuf> {
    if let Some(over) = state_dir_override()? {
        return Ok(over.join(&record.id));
    }
    if let Some(ref sd) = record.state_dir {
        return Ok(sd.clone());
    }
    Ok(record.path.join(".coordinator"))
}

pub fn run_state_path(record: &ProjectRecord) -> Result<PathBuf> {
    Ok(resolve_state_dir(record)?.join("run-state.json"))
}

pub fn load_run_state(record: &ProjectRecord) -> Result<RunState> {
    let path = run_state_path(record)?;
    if !path.exists() {
        return Ok(RunState::idle(&record.id));
    }
    let text = std::fs::read_to_string(&path)?;
    let state: RunState = serde_json::from_str(&text)?;
    Ok(state)
}

pub fn save_run_state(record: &ProjectRecord, state: &RunState) -> Result<()> {
    let path = run_state_path(record)?;
    atomic_write_json(&path, state)
}

/// Ensure state dir exists (creates `.coordinator` as needed).
pub fn ensure_state_dir(record: &ProjectRecord) -> Result<PathBuf> {
    let dir = resolve_state_dir(record)?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
thread_local! {
    /// Test-only lock-wait override so `LockFailed` is provable without a 10 s wait.
    static TEST_LOCK_WAIT: std::cell::Cell<Option<std::time::Duration>> =
        const { std::cell::Cell::new(None) };
}

/// RAII test override for [`run_state_lock_wait`]. Resets the thread-local on drop.
#[cfg(test)]
pub(crate) struct TestLockWaitGuard;

#[cfg(test)]
pub(crate) fn test_run_state_lock_wait(duration: std::time::Duration) -> TestLockWaitGuard {
    TEST_LOCK_WAIT.with(|cell| cell.set(Some(duration)));
    TestLockWaitGuard
}

#[cfg(test)]
impl Drop for TestLockWaitGuard {
    fn drop(&mut self) {
        TEST_LOCK_WAIT.with(|cell| cell.set(None));
    }
}

fn run_state_lock_wait() -> std::time::Duration {
    #[cfg(test)]
    if let Some(wait) = TEST_LOCK_WAIT.with(std::cell::Cell::get) {
        return wait;
    }
    std::time::Duration::from_secs(10)
}

thread_local! {
    /// 0121: how many holder-persist fences this thread currently holds. The persist
    /// lock is a synchronous leaf, so the only legal nesting is
    /// `run_state.lock -> pool mutex -> persist lock`.
    static PERSIST_LOCK_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Proof that the persist fence is the *inner* lock on this thread. The private field
/// keeps the token unforgeable outside `state.rs`; dropping it releases the depth, so a
/// panic inside the fenced section cannot leak the ordering state.
pub(crate) struct PersistLockScope(());

impl Drop for PersistLockScope {
    fn drop(&mut self) {
        PERSIST_LOCK_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Enter the persist-lock depth. Fails closed on a recursive acquire; the caller has
/// already succeeded in holding the OS lock when this runs.
pub(crate) fn persist_lock_scope() -> Result<PersistLockScope> {
    if persist_lock_held() {
        return Err(crate::error::CoordinatorError::Message(
            "persist-lock-order: recursive persist lock".into(),
        ));
    }
    PERSIST_LOCK_DEPTH.with(|d| d.set(1));
    Ok(PersistLockScope(()))
}

/// Whether this thread currently holds the holder-persist fence.
pub(crate) fn persist_lock_held() -> bool {
    PERSIST_LOCK_DEPTH.with(std::cell::Cell::get) > 0
}

/// Cross-process exclusive lock for run-state mutation (mkdir is atomic).
///
/// Held across load → decide → save so concurrent CLI vs serve first-commit-wins.
/// Stale locks older than 60s are broken (process crash recovery).
///
/// 0121 order: `run_state.lock -> pool mutex -> persist lock`. The persist lock is a
/// synchronous leaf; taking the run-state lock while it is held is a defect (a grown
/// critical section would park the 10 s run-state budget behind it). The bounded persist
/// acquire already prevents a permanent hang, so this check is about making the
/// violation immediate, named and testable rather than about avoiding the hang.
pub fn with_run_state_lock<T, F>(record: &ProjectRecord, f: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    if persist_lock_held() {
        eprintln!(
            "coordinator: persist-lock-order: run-state lock requested while the persist \
             lock is held (order is run-state -> persist)"
        );
        return Err(crate::error::CoordinatorError::Message(
            "persist-lock-order: run-state lock requested while the persist lock is held \
             (order is run-state -> persist)"
                .into(),
        ));
    }
    ensure_state_dir(record)?;
    let lock_path = resolve_state_dir(record)?.join(".run-state.lock");
    let deadline = std::time::Instant::now() + run_state_lock_wait();
    loop {
        match std::fs::create_dir(&lock_path) {
            Ok(()) => {
                let result = f();
                let _ = std::fs::remove_dir(&lock_path);
                return result;
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::AlreadyExists
                    || e.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                // Break stale lock if older than 60s.
                // Windows surfaces PermissionDenied instead of AlreadyExists
                // while another thread is creating or removing the lock dir,
                // including when exists() is already false (delete in flight).
                if let Ok(meta) = std::fs::metadata(&lock_path)
                    && let Ok(modified) = meta.modified()
                    && let Ok(age) = std::time::SystemTime::now().duration_since(modified)
                    && age > std::time::Duration::from_secs(60)
                {
                    let _ = std::fs::remove_dir(&lock_path);
                    continue;
                }
                if std::time::Instant::now() >= deadline {
                    return Err(crate::error::CoordinatorError::Message(
                        "timed out waiting for run-state lock".into(),
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_review_entry_is_additive_and_defaults_to_none() {
        let mut s = RunState::idle("p");
        s.run_epoch = 4;
        let text = serde_json::to_string(&s).unwrap();
        assert!(!text.contains("plan_review_entry"), "{text}");
        s.plan_review_entry = Some(PlanReviewEntry {
            run_epoch: 4,
            entry_id: 2,
        });
        let back: RunState = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(
            back.plan_review_entry,
            Some(PlanReviewEntry {
                run_epoch: 4,
                entry_id: 2
            })
        );
        let mut v = serde_json::to_value(RunState::idle("p")).unwrap();
        v["run_epoch"] = serde_json::json!(4);
        v.as_object_mut().unwrap().remove("plan_review_entry");
        let legacy: RunState = serde_json::from_value(v).unwrap();
        assert!(legacy.plan_review_entry.is_none());
    }

    #[test]
    fn new_entry_clears_a_plan_review_stall() {
        let mut s = RunState::idle("p");
        s.run_epoch = 7;
        s.phase = crate::workflow::graph::PHASE_PLAN_REVIEW.into();
        s.review = Some(ReviewWatchState {
            stalled: vec!["agy".into(), "codex".into()],
            ..Default::default()
        });
        s.begin_plan_review_entry();
        let rv = s.review.as_ref().unwrap();
        assert!(
            !rv.stalled.iter().any(|x| x == "agy"),
            "a plan-review slug must not carry across entries: {:?}",
            rv.stalled
        );
        assert!(
            rv.stalled.iter().any(|x| x == "codex"),
            "only plan-review slugs are entry-scoped: {:?}",
            rv.stalled
        );
        assert_eq!(
            s.plan_review_entry,
            Some(PlanReviewEntry {
                run_epoch: 7,
                entry_id: 1
            })
        );
    }

    #[test]
    fn entry_books_are_inert_for_another_entry() {
        let mut s = RunState::idle("p");
        s.run_epoch = 3;
        s.phase = crate::workflow::graph::PHASE_PLAN_REVIEW.into();
        s.begin_plan_review_entry();
        s.plan_review_spawned = vec!["agy".into()];
        s.plan_review_slot_ran = vec!["agy".into()];
        s.plan_review_join_retries = 1;
        s.plan_review_verdicts
            .insert("opencode".into(), "ran; produced no review".into());
        s.plan_review_admission_failures
            .insert("opencode".into(), "empty command".into());
        assert_eq!(s.plan_review_spawned_now(), ["agy".to_string()]);
        assert_eq!(s.plan_review_slot_ran_now(), ["agy".to_string()]);
        assert_eq!(s.plan_review_join_retries_now(), 1);
        assert_eq!(
            s.plan_review_verdict("opencode"),
            Some("ran; produced no review")
        );
        assert_eq!(
            s.plan_review_admission_failure("opencode"),
            Some("empty command")
        );
        // The read fence is keyed by `(run_epoch, entry_id)`: the only stored entry owns the
        // books, and a different **epoch** cannot observe them even if a writer forgot to clear.
        s.run_epoch = 4;
        assert!(s.plan_review_entry_current().is_none());
        assert!(s.plan_review_spawned_now().is_empty());
        assert!(s.plan_review_slot_ran_now().is_empty());
        assert_eq!(s.plan_review_join_retries_now(), 0);
        assert!(s.plan_review_verdict("opencode").is_none());
        assert!(s.plan_review_admission_failure("opencode").is_none());
        // Restoring the live epoch restores the values (they were never cleared).
        s.run_epoch = 3;
        assert_eq!(s.plan_review_spawned_now(), ["agy".to_string()]);
        assert_eq!(s.plan_review_slot_ran_now(), ["agy".to_string()]);
        assert_eq!(s.plan_review_join_retries_now(), 1);
        assert_eq!(
            s.plan_review_verdict("opencode"),
            Some("ran; produced no review")
        );
    }

    #[test]
    fn review_reviewed_oid_is_additive() {
        let old: ReviewWatchState = serde_json::from_str(r#"{"verdict":"PASS"}"#).unwrap();
        assert!(old.reviewed_oid.is_none());
        assert!(
            !serde_json::to_string(&old)
                .unwrap()
                .contains("reviewed_oid")
        );
        let rv = ReviewWatchState {
            reviewed_oid: Some("abc".into()),
            ..Default::default()
        };
        let back: ReviewWatchState =
            serde_json::from_str(&serde_json::to_string(&rv).unwrap()).unwrap();
        assert_eq!(back.reviewed_oid.as_deref(), Some("abc"));
    }

    use chrono::Utc;
    use std::path::Path;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn sample_record(path: &Path) -> ProjectRecord {
        ProjectRecord {
            id: Uuid::new_v4().to_string(),
            path: path.to_path_buf(),
            display_name: Some("test".into()),
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
    fn default_state_dir_is_dot_coordinator() {
        let dir = tempdir().unwrap();
        let rec = sample_record(dir.path());
        assert_eq!(
            resolve_state_dir(&rec).unwrap(),
            dir.path().join(".coordinator")
        );
    }

    // ---- 0117: status.holder (DoD-5) ----------------------------------------

    fn holder_record(dir: &Path) -> ProjectRecord {
        let mut rec = sample_record(dir);
        rec.state_dir = Some(dir.join("holder-state"));
        rec
    }

    fn holder_persist(rec: &ProjectRecord, body: serde_json::Value) -> std::path::PathBuf {
        let path = crate::harness::pool::persist_path(rec).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body.to_string()).unwrap();
        let at = std::time::SystemTime::now() - std::time::Duration::from_secs(7 * 60 * 60);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(at)
            .unwrap();
        path
    }

    fn holder_body(
        rec: &ProjectRecord,
        session: Option<&str>,
        alive: bool,
        addr: Option<&str>,
        holder_pid: Option<u32>,
        error: Option<&str>,
    ) -> serde_json::Value {
        serde_json::json!({
            "version": 1,
            "project_id": rec.id,
            "session_id": session,
            "cwd": rec.path.display().to_string(),
            "pid": 4242,
            "holder_pid": holder_pid,
            "control_addr": addr,
            "adapter": "grok",
            "supports_compact": true,
            "alive": alive,
            "prompt_in_flight": false,
            "error": error,
        })
    }

    fn stale_state(rec: &ProjectRecord) -> RunState {
        let mut state = RunState::idle(&rec.id);
        state.status = RunStatus::Stopped;
        state.updated_at = Utc::now() - chrono::Duration::hours(7);
        state
    }

    #[test]
    fn status_project_renders_holder_candidate() {
        let _env = crate::config::test_env_lock();
        let dir = tempdir().unwrap();
        let rec = holder_record(dir.path());
        let path = holder_persist(
            &rec,
            holder_body(
                &rec,
                Some("sess-live"),
                true,
                Some("127.0.0.1:9"),
                Some(4242),
                None,
            ),
        );
        let state = stale_state(&rec);
        save_run_state(&rec, &state).unwrap();

        let view = StatusView::from_record(&rec, &state);
        let holder = view.holder.expect("holder object is always present");
        assert_eq!(holder.state, "candidate");
        assert_eq!(holder.reason, "pre-rpc-eligible");
        assert!(
            holder.candidate,
            "pre-RPC eligibility, not an identity verdict"
        );
        assert!(holder.run_terminal);
        assert_eq!(holder.persist_alive, Some(true));
        assert_eq!(holder.holder_pid, Some(4242));
        assert_eq!(holder.session_id.as_deref(), Some("sess-live"));
        assert!(holder.age_secs >= 6 * 60 * 60);
        assert_eq!(holder.persist_path.as_deref(), Some(path.as_path()));
    }

    #[test]
    fn status_project_renders_holder_kept_dead_persist_no_address() {
        let _env = crate::config::test_env_lock();
        let dir = tempdir().unwrap();
        let rec = holder_record(dir.path());
        let state = stale_state(&rec);
        save_run_state(&rec, &state).unwrap();

        // All three evidence-free producers share one reason; `persist_alive:false`
        // plus the age are the only discriminators (no `start-pending` token).
        for body in [
            holder_body(&rec, Some("sess-exit"), false, None, Some(4242), None),
            holder_body(&rec, None, false, None, None, Some("handshake failed")),
            holder_body(&rec, None, false, None, None, None),
        ] {
            holder_persist(&rec, body);
            let holder = StatusView::from_record(&rec, &state).holder.unwrap();
            assert_eq!(holder.state, "kept");
            assert_eq!(holder.reason, "dead-persist-no-address");
            assert_eq!(holder.persist_alive, Some(false));
            assert!(!holder.candidate);
        }
    }

    #[test]
    fn status_project_renders_holder_gate_off() {
        use crate::config::test_env_lock;

        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let rec = holder_record(dir.path());
        let path = holder_persist(
            &rec,
            holder_body(
                &rec,
                Some("sess-live"),
                true,
                Some("127.0.0.1:9"),
                Some(4242),
                None,
            ),
        );
        let state = stale_state(&rec);
        save_run_state(&rec, &state).unwrap();
        let key = crate::features::Duty::SelfReap.env_var();
        let previous = std::env::var_os(key);
        // SAFETY: serialized by test_env_lock; restored below.
        unsafe { std::env::set_var(key, "off") };
        let holder = StatusView::from_record(&rec, &state).holder.unwrap();
        unsafe {
            match previous {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        assert_eq!(holder.state, "gate-off");
        assert_eq!(holder.reason, "gate-off");
        assert_eq!(
            holder.persist_path.as_deref(),
            Some(path.as_path()),
            "§7.5: the recovery path is visible even with the duty off"
        );
    }

    #[test]
    fn status_project_renders_holder_absent() {
        let _env = crate::config::test_env_lock();
        let dir = tempdir().unwrap();
        let rec = holder_record(dir.path());
        let state = stale_state(&rec);
        // No run-state file on disk.
        let holder = StatusView::from_record(&rec, &state).holder.unwrap();
        assert_eq!(holder.state, "absent");
        assert_eq!(holder.reason, "missing-run-state");
        assert!(!holder.run_terminal, "never inferred from the Idle stub");
    }

    #[test]
    fn status_project_renders_holder_names_the_persist_path() {
        let _env = crate::config::test_env_lock();
        let dir = tempdir().unwrap();
        let rec = holder_record(dir.path());
        let path = holder_persist(&rec, holder_body(&rec, None, false, None, None, None));
        let state = stale_state(&rec);
        save_run_state(&rec, &state).unwrap();
        let holder = StatusView::from_record(&rec, &state).holder.unwrap();
        assert_eq!(holder.persist_path.as_deref(), Some(path.as_path()));
    }

    #[test]
    fn status_view_additive_holder_field() {
        let _env = crate::config::test_env_lock();
        let dir = tempdir().unwrap();
        let rec = holder_record(dir.path());
        let state = stale_state(&rec);
        save_run_state(&rec, &state).unwrap();
        let json = serde_json::to_value(StatusView::from_record(&rec, &state)).unwrap();
        assert!(
            json.get("holder").is_some(),
            "the holder object is additive"
        );

        // A pre-track view (no `holder` key) still deserializes.
        let mut legacy = json.clone();
        legacy.as_object_mut().unwrap().remove("holder");
        let back: StatusView = serde_json::from_value(legacy).unwrap();
        assert!(back.holder.is_none());
        assert_eq!(back.project_id, rec.id);
    }

    #[test]
    fn save_load_run_state() {
        let dir = tempdir().unwrap();
        // Explicit state_dir so parallel tests that set COORDINATOR_STATE_DIR cannot redirect us.
        let mut rec = sample_record(dir.path());
        rec.state_dir = Some(dir.path().join("explicit-state"));
        let mut state = RunState::idle(&rec.id);
        state.status = RunStatus::Running;
        state.phase = STUB_PHASE_ACTIVE.into();
        state.last_event = "run: started stub".into();
        state.run_epoch = 1;
        save_run_state(&rec, &state).unwrap();
        assert!(run_state_path(&rec).unwrap().exists());
        let loaded = load_run_state(&rec).unwrap();
        assert_eq!(loaded.status, RunStatus::Running);
        assert_eq!(loaded.phase, STUB_PHASE_ACTIVE);
        assert_eq!(loaded.run_epoch, 1);
    }

    #[test]
    fn loads_legacy_run_state_without_0005_fields() {
        let dir = tempdir().unwrap();
        let mut rec = sample_record(dir.path());
        rec.state_dir = Some(dir.path().join("explicit-state"));
        let path = run_state_path(&rec).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Minimal 0004-shaped JSON (no run_epoch / failure_class / etc.).
        std::fs::write(
            &path,
            r#"{
                "project_id": "legacy",
                "status": "Idle",
                "phase": "stub:idle",
                "updated_at": "2026-08-12T00:00:00Z",
                "last_event": "idle: no run"
            }"#,
        )
        .unwrap();
        let loaded = load_run_state(&rec).unwrap();
        assert_eq!(loaded.run_epoch, 0);
        assert!(loaded.failure_class.is_none());
        assert!(loaded.next_track.is_none());
    }

    #[test]
    fn state_dir_env_override() {
        use crate::config::{ENV_COORDINATOR_STATE_DIR, test_env_lock};

        let _guard = test_env_lock();

        let proj = tempdir().unwrap();
        let over = tempdir().unwrap();
        let rec = sample_record(proj.path());

        // SAFETY: serialized by test_env_lock; restored before drop.
        unsafe {
            std::env::set_var(ENV_COORDINATOR_STATE_DIR, over.path());
        }
        let resolved = resolve_state_dir(&rec).unwrap();
        assert_eq!(resolved, over.path().join(&rec.id));
        assert_eq!(
            run_state_path(&rec).unwrap(),
            over.path().join(&rec.id).join("run-state.json")
        );
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_STATE_DIR);
        }
    }

    #[test]
    fn state_dir_env_keys_by_project_id() {
        use crate::config::{ENV_COORDINATOR_STATE_DIR, test_env_lock};

        let _guard = test_env_lock();

        let over = tempdir().unwrap();
        let p1 = tempdir().unwrap();
        let p2 = tempdir().unwrap();
        let r1 = sample_record(p1.path());
        let r2 = sample_record(p2.path());

        unsafe {
            std::env::set_var(ENV_COORDINATOR_STATE_DIR, over.path());
        }
        assert_ne!(
            resolve_state_dir(&r1).unwrap(),
            resolve_state_dir(&r2).unwrap()
        );
        assert!(
            resolve_state_dir(&r1)
                .unwrap()
                .ends_with(std::path::Path::new(&r1.id))
        );
        assert!(
            resolve_state_dir(&r2)
                .unwrap()
                .ends_with(std::path::Path::new(&r2.id))
        );
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_STATE_DIR);
        }
    }

    #[test]
    fn state_dir_record_override() {
        let _env = crate::config::test_env_lock();
        let proj = tempdir().unwrap();
        let over = tempdir().unwrap();
        let mut rec = sample_record(proj.path());
        rec.state_dir = Some(over.path().to_path_buf());
        assert_eq!(resolve_state_dir(&rec).unwrap(), over.path());
    }

    #[test]
    fn empty_state_dir_env_rejected() {
        use crate::config::{ENV_COORDINATOR_STATE_DIR, test_env_lock};

        let _guard = test_env_lock();
        let proj = tempdir().unwrap();
        let rec = sample_record(proj.path());
        unsafe {
            std::env::set_var(ENV_COORDINATOR_STATE_DIR, "");
        }
        let err = resolve_state_dir(&rec).unwrap_err();
        assert!(err.to_string().contains("empty"));
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_STATE_DIR);
        }
    }

    #[test]
    fn run_state_lock_contention_succeeds() {
        use crate::config::test_env_lock;

        let _guard = test_env_lock();

        let dir = tempdir().unwrap();
        let mut rec = sample_record(dir.path());
        rec.state_dir = Some(dir.path().join("explicit-state"));
        save_run_state(&rec, &RunState::idle(&rec.id)).unwrap();
        let rec = std::sync::Arc::new(rec);
        let mut handles = Vec::new();
        for _ in 0..8 {
            let rec = rec.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..20 {
                    with_run_state_lock(&rec, || {
                        let mut s = load_run_state(&rec)?;
                        s.last_event = "lock".into();
                        save_run_state(&rec, &s)
                    })
                    .unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert!(run_state_path(&rec).unwrap().exists());
    }

    #[test]
    fn status_serializes_null_execution_repo() {
        let dir = tempdir().unwrap();
        let rec = sample_record(dir.path());
        let state = RunState::idle(&rec.id);
        let view = StatusView::from_record(&rec, &state);
        let json = serde_json::to_value(&view).unwrap();
        assert!(json.as_object().unwrap().contains_key("execution_repo"));
        assert!(json["execution_repo"].is_null());
        assert!(json.as_object().unwrap().contains_key("conductor_dir"));
        assert_eq!(json["layout_profile"], "nested");
        assert!(json.as_object().unwrap().contains_key("failure_artifact"));
        assert!(json["failure_artifact"].is_null());
        assert!(json.as_object().unwrap().contains_key("ci"));
        assert!(json["ci"].is_null());
        assert!(json.as_object().unwrap().contains_key("review"));
        assert!(json["review"].is_null());
        assert!(json.as_object().unwrap().contains_key("stall"));
        assert!(json["stall"].is_null());
        assert!(
            !json.as_object().unwrap().contains_key("ticker"),
            "from_record must omit ticker"
        );
        let last = json.get("last_progress_at");
        assert!(
            last.is_none() || last.is_some_and(|v| v.is_null()),
            "idle last_progress_at must be omitted or null"
        );
    }

    #[test]
    fn status_view_hitl_serializes_auto_start() {
        let dir = tempdir().unwrap();
        let rec = sample_record(dir.path());
        assert_eq!(rec.auto_start, crate::registry::AutoStartPolicy::Hitl);
        let view = StatusView::from_record(&rec, &RunState::idle(&rec.id));
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["auto_start"], "hitl");
    }

    #[test]
    fn status_serializes_display_name_when_some_omits_when_none() {
        let dir = tempdir().unwrap();
        let mut rec = sample_record(dir.path());
        rec.display_name = Some("Ops".into());
        let view = StatusView::from_record(&rec, &RunState::idle(&rec.id));
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["display_name"], "Ops");

        rec.display_name = None;
        let view = StatusView::from_record(&rec, &RunState::idle(&rec.id));
        let json = serde_json::to_value(&view).unwrap();
        assert!(
            !json.as_object().unwrap().contains_key("display_name"),
            "None must omit display_name"
        );
    }

    #[test]
    fn effective_elapsed_excludes_pause() {
        let mut state = RunState::idle("p");
        let start = Utc::now() - chrono::Duration::seconds(10);
        state.phase_started_at = Some(start);
        state.total_paused_ms = 3000;
        state.pause_started_at = None;
        let elapsed = state.effective_running_elapsed(Utc::now());
        // ~7s wall-pause; allow clock skew in CI
        assert!(elapsed.as_secs() >= 6 && elapsed.as_secs() <= 8);
    }

    #[test]
    fn stall_paging_state_is_additive_and_round_trips() {
        let mut idle = RunState::idle("p");
        let idle_json = serde_json::to_string(&idle).unwrap();
        assert!(
            !idle_json.contains("stall_paging"),
            "an idle marker must not add a JSON key"
        );

        // A legacy file with no key loads as default.
        let mut legacy: serde_json::Value = serde_json::from_str(&idle_json).unwrap();
        legacy.as_object_mut().unwrap().remove("stall_paging");
        let loaded: RunState = serde_json::from_value(legacy).unwrap();
        assert_eq!(loaded.stall_paging, StallPagingState::default());
        assert!(loaded.stall_paging.is_idle());

        // A claimed marker round-trips field-for-field.
        let paged_at = Utc::now();
        idle.stall_paging = StallPagingState {
            paged_epoch: Some(7),
            paged_track: Some("0104".into()),
            paged_phase: Some("implement".into()),
            paged_at: Some(paged_at),
            cleared: true,
        };
        let text = serde_json::to_string(&idle).unwrap();
        assert!(text.contains("stall_paging"));
        let back: RunState = serde_json::from_str(&text).unwrap();
        assert_eq!(back.stall_paging, idle.stall_paging);
        assert_eq!(back, idle);
        assert!(!back.stall_paging.is_idle());

        // Equality includes the new field.
        let mut other = idle.clone();
        other.stall_paging.cleared = false;
        assert_ne!(other, idle);
    }

    #[test]
    fn run_state_idle_has_idle_paging_marker() {
        let state = RunState::idle("p");
        assert_eq!(state.stall_paging, StallPagingState::default());
        assert!(state.stall_paging.is_idle());
        assert!(state.stall_paging.paged_epoch.is_none());
        assert!(state.stall_paging.paged_at.is_none());
        assert!(!state.stall_paging.cleared);
    }

    // ---- 0121: the persist lock order ---------------------------------------

    #[test]
    fn persist_lock_order_rejects_run_state_inside_the_fence() {
        let dir = tempdir().unwrap();
        let rec = holder_record(dir.path());
        let scope = persist_lock_scope().unwrap();
        let err = with_run_state_lock(&rec, || Ok(())).unwrap_err();
        assert!(err.to_string().contains("persist-lock-order"), "{err}");
        assert!(
            !resolve_state_dir(&rec)
                .unwrap()
                .join(".run-state.lock")
                .exists(),
            "the refused acquisition creates nothing"
        );
        drop(scope);
        with_run_state_lock(&rec, || Ok(())).expect("the order is legal once the fence drops");
    }

    #[test]
    fn persist_lock_order_is_per_thread() {
        let dir = tempdir().unwrap();
        let rec = holder_record(dir.path());
        let scope = persist_lock_scope().unwrap();
        let other = {
            let rec = rec.clone();
            std::thread::spawn(move || with_run_state_lock(&rec, || Ok(())).is_ok())
        };
        assert!(
            other.join().unwrap(),
            "the depth is thread-local, so another thread still takes the run-state lock"
        );
        drop(scope);
    }
}
