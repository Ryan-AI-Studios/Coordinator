//! Coordinator Control Plane library (CLI + localhost API share these ops).
//!
//! Phase Outcome File (schema v1) is the portable done-contract; apply + wait/timeout
//! complete phases without chat vibes (track **0005**). Grok ACP adapter +
//! project-keyed session pool: track **0007**. Canonical workflow runner: track **0008**.
//! Failure Artifact + toast + Notify Adapter: track **0009**.
//! Token-idle CI wait + auto squash-merge: track **0010**.
//! Cross-model review gate (Codex→Claude→OpenCode): track **0011**.
//! Publish-only cross-model FAIL (no product blockers + evidence.md + clean tree) degrades to ci-wait: track **0051**.
//! Status Surface (Dioxus Desktop, default-on `ui`; `--no-default-features` escape): track **0014** / **0024**.
//! Hermes inbound webhook notify adapter (opt-in HMAC V2): track **0015**.
//! Coordinated multi-sibling dogfood (named-map prompt): track **0016**.
//! Harness progress watchdog (detect + surface stall): track **0026**.
//! Abort stuck Prompt + refuse wedged-session reuse: track **0027**.
//! Harness preflight (`doctor` + adapter `run` refuse missing/auth): track **0028**.
//! Infer `--project` from unique cwd containment else last-used: track **0029**.
//! Omit `--track` starts first exact Ready row (`status_clean` vs `ready_aliases`;
//! default `Ready — not started`) when unset or backlog-clear: track **0030** / **0042**.
//! Skip already-merged `track(NNNN):` at omit-pick / local shipped at advance: track **0046**.
//! Deterministic advance successor from Ready rows (planner `next_track` recorded, not authoritative): track **0047**.
//! `parse_conductor_md` keeps rows after intra-table blanks and later Track+Status tables: track **0050**.
//! Intra-table thematic breaks (`---`) are skipped like blanks; empty Ready-walk journals parsed row count: track **0052**.
//! ci-wait resolves immediately when the awaited PR is already GitHub MERGED (`--head` merged list + title probe): track **0048**.
//! ci-wait fail-set is required GitHub checks only (`--required` + `mergeStateStatus` backstop + all-checks fallback): track **0049**.
//! ci-wait auto-publishes a clean local `track(NNNN):` commit when no PR exists (resolved remote + `gh pr create`): track **0053**.
//! Slot-quality admission (plan-review + cross-model; schema-only JSON is dud): track **0043**.
//! Per-project `AutoStartPolicy` (full|hitl|never) + `<!-- nostart -->`: track **0044**.
//! Address-findings side loop after GateFail (cap 2 then difficulty Stop): track **0031**.
//! Windows Grok `terminal/create` pwsh→powershell.exe→cmd.exe fallback + spawn-fail `HarnessCrash`: track **0032**.
//! Opt-in Hermes progress POSTs on phase advance (failure JSON unchanged): track **0033**.
//! TerminalHub JSONL command journal + `loop_suspect` on repeated fail: track **0034**.
//! First stall recycles unless sidecar `tool_in_flight` (`session/update` is not mid-tool): track **0035**.
//! One-shot reviewer silence stall (tree-kill + 0011 fall-through; not ACP recycle): track **0036**.
//! Plan-review Antigravity one-shot (`agy --print`): track **0017**.
//! Plan-review OpenCode one-shot (`opencode run`): track **0018**.
//! OpenCode prompt on stdin (plan-review + 0011); degenerate `opencode-review.md` retry: track **0037**.
//! Role-bound plan/fold/implement/advance drive: track **0019**.
//! Adaptive ACP spawn (cursor `--yolo --trust acp`, `cursor_login`, `/summarize`, `roles use`): track **0038**.
//! Plan-review join wall 2400s + OpenCode `plan_review_slot` clock and pin-OOS prompt: track **0039**.

pub mod api;
pub mod ci;
pub mod cli;
pub mod config;
pub mod error;
pub mod harness;
pub mod layout;
pub mod notify;
pub mod outcome;
pub mod persist;
pub mod progress_log;
pub mod registry;
pub mod review;
pub mod run;
pub mod scan;
pub mod serve_lease;
pub mod server;
pub mod state;
pub mod ui;
pub mod watch;
pub mod workflow;

pub use error::{CoordinatorError, Result};
