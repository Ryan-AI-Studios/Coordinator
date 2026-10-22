//! Clap CLI surface (tracks 0004–0006).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

use crate::api;
use crate::config::DEFAULT_SERVE_PORT;
use crate::error::CoordinatorError;
use crate::layout::LayoutProfile;
use crate::registry::{
    AutoStartPolicy, ProjectAddOptions, ProjectSetOptions, parse_skill_alias, parse_skill_alias_key,
};
use crate::server;
use crate::workflow::timeouts::parse_phase_timeout;

fn parse_auto_merge(s: &str) -> std::result::Result<bool, String> {
    match s.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!("expected true|false, got {other}")),
    }
}

fn parse_auto_start(s: &str) -> std::result::Result<AutoStartPolicy, String> {
    AutoStartPolicy::parse(s).map_err(|e| e.to_string())
}

#[derive(Debug, Parser)]
#[command(
    name = "coordinator",
    version,
    about = "Local Control Plane for multi-harness conductor-track workflows"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Manage the machine Project Registry
    Project {
        #[command(subcommand)]
        action: ProjectCommands,
    },
    /// Show run status for a project
    Status {
        #[arg(long)]
        project: Option<String>,
    },
    /// Track-registry store: import, render, verify, set-status
    Registry {
        #[command(subcommand)]
        action: RegistryCommands,
    },
    /// Probe bound harnesses (PATH + cheap auth). Pretty JSON; exit 1 if a required row is missing or logged out.
    Doctor {
        #[arg(long)]
        project: Option<String>,
    },
    /// Start the canonical workflow at `plan` and tick until Idle/Stopped
    ///
    /// Skips wait when serve answers (lease then 7420). `--serve-port N` probes N only.
    Run {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        track: Option<String>,
        /// adapter | file_wait | stub (default adapter, or COORDINATOR_WORKFLOW_DRIVER)
        #[arg(long)]
        driver: Option<String>,
        /// Write-only start. Use when `serve` already ticks.
        #[arg(long)]
        detach: bool,
        /// CLI poll budget (same contract as `wait`). `N>0`; omit to tick until terminal.
        #[arg(long, conflicts_with = "detach")]
        timeout_secs: Option<u64>,
        /// Probe this port only for an already-on serve (conflicts with --detach).
        #[arg(long, conflicts_with = "detach")]
        serve_port: Option<u16>,
        /// Skip harness preflight (adapter still starts). Does not conflict with --detach.
        #[arg(long)]
        skip_preflight: bool,
    },
    /// Pause a running workflow
    Pause {
        #[arg(long)]
        project: Option<String>,
    },
    /// Resume a paused workflow
    Resume {
        #[arg(long)]
        project: Option<String>,
    },
    /// Stop a run (no merge; sessions left for attach; not a failure)
    Stop {
        #[arg(long)]
        project: Option<String>,
    },
    /// Return the execution tree to this epoch's checkpoint ref
    Restore {
        #[arg(long)]
        project: Option<String>,
        /// Reset and `git clean -fd` even when the tree has committable changes
        #[arg(long)]
        discard: bool,
    },
    /// Phase Outcome File writers / inspectors
    Outcome {
        #[command(subcommand)]
        action: OutcomeCommands,
    },
    /// Failure Artifact inspectors
    Failure {
        #[command(subcommand)]
        action: FailureCommands,
    },
    /// Block until a Phase Outcome is applied (or wait budget expires)
    Wait {
        #[arg(long)]
        project: Option<String>,
        /// Max seconds to wait for an applied outcome (default 3600)
        #[arg(long, default_value_t = 3600)]
        timeout_secs: u64,
    },
    /// Serve localhost HTTP API (127.0.0.1 only)
    ///
    /// Always-on machine ticker. `--check` prints health JSON and exits 0/1 without binding.
    Serve {
        /// Bind port (default 7420). With --check, presence of this flag means probe N only.
        #[arg(long)]
        port: Option<u16>,
        /// One-shot health check. Does not bind and does not write a lease.
        #[arg(long)]
        check: bool,
    },
    /// Open the Local Ops Console (Status Surface).
    Ui {
        #[arg(long, default_value_t = DEFAULT_SERVE_PORT)]
        port: u16,
    },
    /// Build, smoke-test and swap the installed coordinator.exe (operator-invoked)
    ///
    /// Refuses while a build/test, a live coordinator.exe, an active run, a dirty
    /// tree, a recent tree write or a git write is present. Run the installed (or
    /// built) coordinator.exe directly: `cargo run -- self-update` self-blocks
    /// because the invoking cargo.exe is in the process table. Stop serve/UI/
    /// holders before a real swap; the product never restarts them.
    SelfUpdate {
        /// Product repo (default: git toplevel of cwd)
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Install target (default: the running executable)
        #[arg(long)]
        install: Option<PathBuf>,
        /// Prebuilt artefact; skips the build
        #[arg(long)]
        artifact: Option<PathBuf>,
        /// Report the decision only; build, rename and write nothing
        #[arg(long)]
        check: bool,
    },
    /// Drive a harness session (Grok ACP this track)
    Harness {
        #[command(subcommand)]
        action: HarnessCommands,
    },
    /// Show, toggle, or set machine Role Bindings (planner/implementor/reviewers)
    Roles {
        #[command(subcommand)]
        action: RolesCommands,
    },
    /// Notify probes (Hermes inbound webhook)
    Notify {
        #[command(subcommand)]
        action: NotifyCommands,
    },
    /// Record, show, or list an owner decision on a track.
    Decision {
        #[command(subcommand)]
        action: DecisionCommands,
    },
    /// Show, set, or approve a contextual state policy.
    Policy {
        #[command(subcommand)]
        action: PolicyCommands,
    },
}

#[derive(Debug, Subcommand)]
pub enum RolesCommands {
    /// Pretty JSON of machine `role_bindings`
    Show,
    /// Set planner+implementor (and present fold/next) to grok or cursor
    Use {
        /// grok | cursor
        target: RolesTarget,
    },
    /// Set harness/model on ANY role binding (partial update; unsupplied fields are kept)
    Set {
        /// Role key, e.g. planner | implementor | fold | next | cross_model_primary
        role: String,
        /// Harness adapter name to write into the binding
        #[arg(long)]
        harness: Option<String>,
        /// Command to spawn for this role
        #[arg(long)]
        command: Option<String>,
        /// Model id passed to the harness verbatim (cursor base ids accept [effort=...])
        #[arg(long)]
        model: Option<String>,
        /// Remove this role's model pin (harness default applies)
        #[arg(long)]
        clear_model: bool,
    },
    /// Validated, reversible swap with a truthful probe and a unique snapshot
    Swap {
        /// Role key; required for apply and --cancel, optional for --status
        role: Option<String>,
        /// Role filter for --status (the positional form is also accepted)
        #[arg(long = "role")]
        role_flag: Option<String>,
        /// New harness (uses that harness's command for the role class)
        #[arg(long)]
        harness: Option<String>,
        /// New command (partial update; must match the role class)
        #[arg(long)]
        command: Option<String>,
        /// New model id (verbatim; an OpenCode `#variant` stays catalog-defined)
        #[arg(long)]
        model: Option<String>,
        /// Remove the model pin
        #[arg(long)]
        clear_model: bool,
        /// Drop the role's deferred entry (no config write)
        #[arg(long)]
        cancel: bool,
        /// Print the machine ledger (pending/last/errors) and the lock holder
        #[arg(long)]
        status: bool,
        /// Explicit recovery for a crash-left role-swap.lock
        #[arg(long)]
        unlock: bool,
        /// JSON output
        #[arg(long)]
        json: bool,
    },
    /// Restore the exact previous RoleBinding from a snapshot
    Rollback {
        /// Role key
        role: String,
        /// Snapshot path (default: last[role].snapshot)
        #[arg(long)]
        from: Option<PathBuf>,
        /// JSON output
        #[arg(long)]
        json: bool,
    },
    /// Prove the live holder identity for an adapter role (read-only)
    Prove {
        /// Adapter role key (planner | implementor | fold | next)
        role: String,
        /// Project id (read-only resolve; never writes last-used.json)
        #[arg(long)]
        project: Option<String>,
        /// JSON output
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RolesTarget {
    Grok,
    Cursor,
}

#[derive(Debug, Subcommand)]
pub enum NotifyCommands {
    /// Probe the opt-in Hermes inbound webhook (no artifact, no toast)
    HermesTest {
        #[arg(long)]
        project: Option<String>,
        /// Synthetic progress POST (no artifact / toast). Default is the 0015 failure probe.
        #[arg(long)]
        progress: bool,
    },
    /// Post one fleet status summary (no artifact, no toast).
    FleetSummary,
}

#[derive(Debug, Subcommand)]
pub enum PolicyCommands {
    /// Print the four builtin rules and where each action came from.
    Show {
        #[arg(long)]
        project: Option<String>,
    },
    /// Upsert one builtin on the project record.
    Set {
        #[arg(long)]
        project: Option<String>,
        /// dependency-manifest | ci-workflow-path | consecutive-failures | restore-before-publish
        #[arg(long)]
        name: String,
        /// allow | report | require-approval | block
        #[arg(long)]
        action: String,
        /// Rule threshold. Consecutive-failures defaults to 3 when omitted.
        #[arg(long)]
        threshold: Option<u32>,
    },
    /// Approve the current fingerprint for one builtin.
    Approve {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        name: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum DecisionCommands {
    /// Record one owner decision into the track evidence file.
    ///
    /// The marker records that an operator declared this decision and supplied the name. It is not proof the named person typed it, not a signature, and not a tamper-proof or compliance control.
    #[command(long_about = crate::workflow::decision::RECORD_LIMITATION)]
    Record {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        track: String,
        #[arg(long)]
        by: String,
        /// Mark the current active block superseded and append a new one.
        #[arg(long)]
        supersede: bool,
        /// Read the sentence from this file instead of stdin.
        #[arg(long = "sentence-file")]
        sentence_file: Option<PathBuf>,
    },
    /// Show the one active owner decision.
    Show {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        track: String,
        #[arg(long)]
        json: bool,
    },
    /// List owner-decision blocks in file order.
    List {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        track: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum HarnessCommands {
    /// Grok Build ACP adapter
    Grok {
        #[command(subcommand)]
        action: GrokCommands,
    },
}

#[derive(Debug, Subcommand)]
pub enum GrokCommands {
    /// Start (or reuse) a long-lived Grok ACP session
    Start {
        #[arg(long)]
        project: Option<String>,
    },
    /// Inject a prompt; applies Phase Outcome when the run is Running
    Prompt {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        text: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Inject `/compact` (skip if capability is false)
    Compact {
        #[arg(long)]
        project: Option<String>,
    },
    /// Show Grok session summary
    Status {
        #[arg(long)]
        project: Option<String>,
    },
    /// Kill the Grok ACP child (explicit teardown)
    Shutdown {
        #[arg(long)]
        project: Option<String>,
    },
    /// Internal detached holder (not for operators)
    #[command(hide = true)]
    Hold {
        #[arg(long)]
        project: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ProjectCommands {
    /// Register a project path
    Add {
        path: PathBuf,
        /// nested | multi_sibling | single_root (default nested)
        #[arg(long = "profile", default_value = "nested")]
        profile: String,
        #[arg(long = "execution-repo")]
        execution_repo: Option<PathBuf>,
        #[arg(long = "conductor-dir")]
        conductor_dir: Option<PathBuf>,
        #[arg(long = "state-dir")]
        state_dir: Option<PathBuf>,
        #[arg(long = "display-name")]
        display_name: Option<String>,
        /// multi_sibling: name for primary map entry when --execution-repo set
        #[arg(long = "execution-repo-name")]
        execution_repo_name: Option<String>,
        /// true | false (omit = default on)
        #[arg(long = "auto-merge", value_parser = parse_auto_merge)]
        auto_merge: Option<bool>,
        /// full | hitl | never (omit = hitl)
        #[arg(long = "auto-start", value_parser = parse_auto_start)]
        auto_start: Option<AutoStartPolicy>,
        /// Repeatable. Canonical phase id or `plan_review_slot` = seconds (>0).
        #[arg(long = "phase-timeout", value_name = "PHASE=SECS", value_parser = parse_phase_timeout)]
        phase_timeouts: Vec<(String, u64)>,
        /// Repeatable. Canonical skill name = directory segment (`plan=plan-track`).
        #[arg(long = "skill-alias", value_name = "KEY=NAME", value_parser = parse_skill_alias)]
        skill_aliases: Vec<(String, String)>,
        /// Repeatable. Drop one skill-alias key. A no-op on add because the map starts empty.
        #[arg(long = "clear-skill-alias", value_name = "KEY", value_parser = parse_skill_alias_key)]
        clear_skill_alias: Vec<String>,
        /// Wipe skill aliases before the overlay. A no-op on add.
        #[arg(long = "clear-skill-aliases")]
        clear_skill_aliases: bool,
    },
    /// List registered projects (JSON; includes profile + execution summary)
    List,
    /// Show raw record + resolved layout paths
    Show {
        #[arg(long)]
        project: Option<String>,
        /// Read-only resolved execution-target preview for this track.
        #[arg(long)]
        track: Option<String>,
    },
    /// Update profile / path bindings (workspace path is immutable)
    Set {
        #[arg(long)]
        project: Option<String>,
        #[arg(long = "profile")]
        profile: Option<String>,
        #[arg(long = "execution-repo")]
        execution_repo: Option<PathBuf>,
        #[arg(long = "conductor-dir")]
        conductor_dir: Option<PathBuf>,
        #[arg(long = "state-dir")]
        state_dir: Option<PathBuf>,
        #[arg(long = "display-name")]
        display_name: Option<String>,
        /// JSON object map name → path for multi_sibling
        #[arg(long = "execution-repos-json")]
        execution_repos_json: Option<String>,
        #[arg(long = "execution-repo-name")]
        execution_repo_name: Option<String>,
        /// true | false (omit = leave unchanged)
        #[arg(long = "auto-merge", value_parser = parse_auto_merge)]
        auto_merge: Option<bool>,
        /// true | false (omit = leave unchanged). Opt-in Hermes progress POSTs.
        #[arg(long = "notify-progress", value_parser = parse_auto_merge)]
        notify_progress: Option<bool>,
        /// true | false (omit = leave unchanged). Per-run_epoch detached worktree.
        #[arg(long = "worktree-isolation", value_parser = parse_auto_merge)]
        worktree_isolation: Option<bool>,
        /// true | false (omit = leave unchanged). Bounded self-check on implement and address-findings.
        #[arg(long = "self-continuation", value_parser = parse_auto_merge)]
        self_continuation: Option<bool>,
        /// true | false (omit = leave unchanged). Route the ci-wait gate's failing checks into address-ci.
        #[arg(long = "ci-fix-routing", value_parser = parse_auto_merge)]
        ci_fix_routing: Option<bool>,
        /// Repeatable. Canonical phase id or `plan_review_slot` = seconds (>0).
        #[arg(long = "phase-timeout", value_name = "PHASE=SECS", value_parser = parse_phase_timeout)]
        phase_timeouts: Vec<(String, u64)>,
        /// Repeatable. Drop one stored project override (canonical phase id or `plan_review_slot`).
        #[arg(long = "clear-phase-timeout", value_name = "PHASE")]
        clear_phase_timeout: Vec<String>,
        /// Wipe the project phase-timeout map.
        #[arg(long = "clear-phase-timeouts")]
        clear_phase_timeouts: bool,
        /// Repeatable. Extra exact Ready phrases for omit-`--track` pick.
        #[arg(long = "status-word-ready", value_name = "PHRASE")]
        status_words_ready: Vec<String>,
        /// Clear stored ready aliases (back to `Ready — not started`).
        #[arg(long = "clear-status-words-ready")]
        clear_status_words_ready: bool,
        /// full | hitl | never (omit = leave unchanged)
        #[arg(long = "auto-start", value_parser = parse_auto_start)]
        auto_start: Option<AutoStartPolicy>,
        /// Repeatable. Canonical skill name = directory segment (`plan=plan-track`). Last duplicate wins.
        #[arg(long = "skill-alias", value_name = "KEY=NAME", value_parser = parse_skill_alias)]
        skill_aliases: Vec<(String, String)>,
        /// Repeatable. Drop one stored skill-alias key before the overlay.
        #[arg(long = "clear-skill-alias", value_name = "KEY", value_parser = parse_skill_alias_key)]
        clear_skill_alias: Vec<String>,
        /// Wipe stored skill aliases before the per-key clears and the overlay.
        #[arg(long = "clear-skill-aliases")]
        clear_skill_aliases: bool,
    },
    /// Scan roots for conductor/conductor.md markers
    Scan {
        /// One-shot root (repeatable). When omitted, uses config.json scan_roots.
        #[arg(long = "root")]
        roots: Vec<PathBuf>,
        /// Register new candidates (default is dry-run list only)
        #[arg(long)]
        add: bool,
        /// Explicit dry-run (default when --add absent)
        #[arg(long = "dry-run", default_value_t = false)]
        dry_run: bool,
        /// Persist --root into machine config.json scan_roots
        #[arg(long = "save-root", default_value_t = false)]
        save_root: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum RegistryCommands {
    /// Import conductor.md into track-registry.json (lossless; view bytes unchanged)
    Import {
        #[arg(long)]
        project: Option<String>,
        #[arg(long = "conductor-dir")]
        conductor_dir: Option<PathBuf>,
        #[arg(long = "dry-run", default_value_t = false)]
        dry_run: bool,
        /// Adopt a hand-edited view / refresh an existing store
        #[arg(long, default_value_t = false)]
        force: bool,
    },
    /// Regenerate conductor.md from the store
    Render {
        #[arg(long)]
        project: Option<String>,
        #[arg(long = "conductor-dir")]
        conductor_dir: Option<PathBuf>,
        /// Compare only
        #[arg(long, default_value_t = false)]
        check: bool,
        /// Archive a drifted view, then rewrite it
        #[arg(long, default_value_t = false)]
        force: bool,
    },
    /// Verify store/view agreement and row-line consistency
    Verify {
        #[arg(long)]
        project: Option<String>,
        #[arg(long = "conductor-dir")]
        conductor_dir: Option<PathBuf>,
    },
    /// Change one row's status cell in the store and regenerate the view
    SetStatus {
        #[arg(long)]
        project: Option<String>,
        #[arg(long = "conductor-dir")]
        conductor_dir: Option<PathBuf>,
        #[arg(long)]
        track: String,
        #[arg(long)]
        status: String,
        /// Remove a `<!-- nostart -->` marker from the row
        #[arg(long = "clear-nostart", default_value_t = false)]
        clear_nostart: bool,
        #[arg(long = "dry-run", default_value_t = false)]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum OutcomeCommands {
    /// Write a Phase Outcome and apply it (single apply path)
    Write {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        phase: String,
        /// success | failure
        #[arg(long)]
        status: String,
        #[arg(long = "failure-class")]
        failure_class: Option<String>,
        #[arg(long)]
        message: Option<String>,
        #[arg(long = "next-track")]
        next_track: Option<String>,
        /// file | http | cli | timeout | test | adapter (default cli)
        #[arg(long, default_value = "cli")]
        source: String,
    },
    /// Print current.json if present
    Show {
        #[arg(long)]
        project: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum FailureCommands {
    /// Print `{state_dir}/FAILURE.md` if present
    Show {
        #[arg(long)]
        project: Option<String>,
    },
    /// Acknowledge and remove a leftover Failure Artifact (Idle/Stopped only)
    Resolve {
        #[arg(long)]
        project: Option<String>,
    },
}

/// Parse args and dispatch; returns process exit code.
pub fn run() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if let CoordinatorError::Preflight { report } = &e
                && let Ok(json) = serde_json::to_string_pretty(report.as_ref())
            {
                println!("{json}");
            }
            eprintln!("error: {e}");
            ExitCode::from(e.exit_code() as u8)
        }
    }
}

fn dispatch(cli: Cli) -> Result<(), CoordinatorError> {
    match cli.command {
        Commands::Project { action } => match action {
            ProjectCommands::Add {
                path,
                profile,
                execution_repo,
                conductor_dir,
                state_dir,
                display_name,
                execution_repo_name,
                auto_merge,
                auto_start,
                phase_timeouts,
                skill_aliases,
                clear_skill_alias,
                clear_skill_aliases,
            } => {
                let layout_profile = LayoutProfile::parse(&profile)?;
                let mut alias_map = BTreeMap::new();
                for (key, value) in skill_aliases {
                    alias_map.insert(key, value);
                }
                let opts = ProjectAddOptions {
                    layout_profile,
                    execution_repo,
                    conductor_dir,
                    state_dir,
                    display_name,
                    execution_repo_name,
                    execution_repos: BTreeMap::new(),
                    auto_merge,
                    phase_timeouts_secs: phase_timeouts.into_iter().collect(),
                    auto_start,
                    skill_aliases: alias_map,
                    clear_skill_aliases,
                    clear_skill_alias,
                };
                let rec = api::project_add(&path, opts)?;
                println!("{}", serde_json::to_string_pretty(&rec)?);
            }
            ProjectCommands::List => {
                let list = api::project_list()?;
                println!("{}", serde_json::to_string_pretty(&list)?);
            }
            ProjectCommands::Show { project, track } => {
                let view =
                    api::project_show_with_track(project.as_deref(), track.as_deref(), true)?;
                println!("{}", serde_json::to_string_pretty(&view)?);
            }
            ProjectCommands::Set {
                project,
                profile,
                execution_repo,
                conductor_dir,
                state_dir,
                display_name,
                execution_repos_json,
                execution_repo_name,
                auto_merge,
                notify_progress,
                worktree_isolation,
                self_continuation,
                ci_fix_routing,
                phase_timeouts,
                clear_phase_timeout,
                clear_phase_timeouts,
                status_words_ready,
                clear_status_words_ready,
                auto_start,
                skill_aliases,
                clear_skill_alias,
                clear_skill_aliases,
            } => {
                let layout_profile = match profile {
                    Some(s) => Some(LayoutProfile::parse(&s)?),
                    None => None,
                };
                let execution_repos = match execution_repos_json {
                    Some(j) => Some(serde_json::from_str::<BTreeMap<String, PathBuf>>(&j)?),
                    None => None,
                };
                let phase_timeouts_secs = if phase_timeouts.is_empty() {
                    None
                } else {
                    Some(phase_timeouts.into_iter().collect())
                };
                let ready_aliases = if status_words_ready.is_empty() {
                    None
                } else {
                    Some(status_words_ready)
                };
                let skill_alias_map = if skill_aliases.is_empty() {
                    None
                } else {
                    let mut map = BTreeMap::new();
                    for (key, value) in skill_aliases {
                        map.insert(key, value);
                    }
                    Some(map)
                };
                let opts = ProjectSetOptions {
                    layout_profile,
                    execution_repo,
                    clear_execution_repo: false,
                    conductor_dir,
                    clear_conductor_dir: false,
                    state_dir,
                    clear_state_dir: false,
                    display_name,
                    execution_repos,
                    execution_repo_name,
                    auto_merge,
                    notify_progress,
                    worktree_isolation,
                    self_continuation,
                    ci_fix_routing,
                    phase_timeouts_secs,
                    clear_phase_timeouts,
                    clear_phase_timeout,
                    ready_aliases,
                    clear_ready_aliases: clear_status_words_ready,
                    auto_start,
                    skill_aliases: skill_alias_map,
                    clear_skill_aliases,
                    clear_skill_alias,
                };
                let rec = api::project_set(project.as_deref(), opts, true)?;
                println!("{}", serde_json::to_string_pretty(&rec)?);
            }
            ProjectCommands::Scan {
                roots,
                add,
                dry_run,
                save_root,
            } => {
                if save_root {
                    for r in &roots {
                        api::save_scan_root(r)?;
                    }
                }
                // Dry-run is default when --add is absent. --dry-run forces list-only.
                let do_add = add && !dry_run;
                let (candidates, added) = api::project_scan(&roots, do_add)?;
                let out = serde_json::json!({
                    "candidates": candidates,
                    "added": added,
                    "mode": if do_add { "add" } else { "dry-run" },
                });
                println!("{}", serde_json::to_string_pretty(&out)?);
            }
        },
        Commands::Status { project } => {
            let view = api::status(project.as_deref(), true)?;
            println!("{}", serde_json::to_string_pretty(&view)?);
        }
        Commands::Registry { action } => match action {
            RegistryCommands::Import {
                project,
                conductor_dir,
                dry_run,
                force,
            } => cmd_registry_import(project.as_deref(), conductor_dir.as_deref(), dry_run, force)?,
            RegistryCommands::Render {
                project,
                conductor_dir,
                check,
                force,
            } => {
                let target = resolve_registry_target(project.as_deref(), conductor_dir.as_deref())?;
                let report = crate::registry_store::render_at(&target.conductor_dir, check, force)?;
                println!("{}", serde_json::to_string_pretty(&report)?);
                if check && report.drift {
                    return Err(CoordinatorError::Message(
                        "conductor.md differs from the store; run `registry render --force` to rewrite it or `registry import --force` to adopt it".into(),
                    ));
                }
            }
            RegistryCommands::Verify {
                project,
                conductor_dir,
            } => {
                let target = resolve_registry_target(project.as_deref(), conductor_dir.as_deref())?;
                let report = crate::registry_store::verify_at(&target.conductor_dir)?;
                println!("{}", serde_json::to_string_pretty(&report)?);
                if report.status == "drift" || report.status == "invalid" {
                    return Err(CoordinatorError::Message(format!(
                        "track registry verify: {}",
                        report.status
                    )));
                }
            }
            RegistryCommands::SetStatus {
                project,
                conductor_dir,
                track,
                status,
                clear_nostart,
                dry_run,
            } => cmd_registry_set_status(
                project.as_deref(),
                conductor_dir.as_deref(),
                &track,
                &status,
                clear_nostart,
                dry_run,
            )?,
        },
        Commands::Doctor { project } => {
            let report = api::cmd_doctor(project.as_deref())?;
            if report.ok {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                return Err(CoordinatorError::Preflight {
                    report: Box::new(report),
                });
            }
        }
        Commands::Run {
            project,
            track,
            driver,
            detach,
            timeout_secs,
            serve_port,
            skip_preflight,
        } => {
            let probe = if detach {
                api::ServeProbe::Skip
            } else if let Some(n) = serve_port {
                api::ServeProbe::Port(n)
            } else {
                api::ServeProbe::Auto
            };
            let view = api::cmd_run_cli(
                project.as_deref(),
                track,
                driver.as_deref(),
                api::RunCliOpts {
                    detach,
                    timeout_secs,
                    probe,
                    skip_preflight,
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&view)?);
        }
        Commands::Pause { project } => {
            let view = api::cmd_pause(project.as_deref(), true)?;
            println!("{}", serde_json::to_string_pretty(&view)?);
        }
        Commands::Resume { project } => {
            let view = api::cmd_resume(project.as_deref(), true)?;
            println!("{}", serde_json::to_string_pretty(&view)?);
        }
        Commands::Stop { project } => {
            let view = api::cmd_stop(project.as_deref(), true)?;
            println!("{}", serde_json::to_string_pretty(&view)?);
        }
        Commands::Restore { project, discard } => {
            let view = api::cmd_restore(project.as_deref(), discard, true)?;
            println!("{}", serde_json::to_string_pretty(&view)?);
        }
        Commands::Outcome { action } => match action {
            OutcomeCommands::Write {
                project,
                phase,
                status,
                failure_class,
                message,
                next_track,
                source,
            } => {
                let view = api::cmd_outcome_write(
                    project.as_deref(),
                    phase,
                    &status,
                    failure_class.as_deref(),
                    message,
                    next_track,
                    Some(&source),
                    true,
                )?;
                println!("{}", serde_json::to_string_pretty(&view)?);
            }
            OutcomeCommands::Show { project } => {
                match api::cmd_outcome_show(project.as_deref(), true)? {
                    Some(o) => println!("{}", serde_json::to_string_pretty(&o)?),
                    None => {
                        println!("null");
                    }
                }
            }
        },
        Commands::Failure { action } => match action {
            FailureCommands::Show { project } => {
                match api::cmd_failure_show(project.as_deref(), true)? {
                    Some(v) => print!("{}", v.body),
                    None => {
                        return Err(CoordinatorError::Message("no failure artifact".into()));
                    }
                }
            }
            FailureCommands::Resolve { project } => {
                let view = api::cmd_failure_resolve(project.as_deref(), true)?;
                println!("{}", serde_json::to_string_pretty(&view)?);
            }
        },
        Commands::Wait {
            project,
            timeout_secs,
        } => {
            let view = api::cmd_wait(project.as_deref(), timeout_secs, true)?;
            println!("{}", serde_json::to_string_pretty(&view)?);
        }
        Commands::Serve { port, check } => {
            if check {
                serve_check(port)?;
            } else {
                block_on(server::serve(port.unwrap_or(DEFAULT_SERVE_PORT)))?;
            }
        }
        Commands::Ui { port } => {
            crate::ui::run_cli(port)?;
        }
        Commands::SelfUpdate {
            repo,
            install,
            artifact,
            check,
        } => {
            let report = crate::selfupdate::cmd_self_update(repo, install, artifact, check)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            if report.outcome == "deployed" {
                eprintln!("{}", report.summary());
            }
            if !report.ok() {
                return Err(CoordinatorError::Message(report.summary()));
            }
        }
        Commands::Harness { action } => match action {
            HarnessCommands::Grok { action } => match action {
                GrokCommands::Start { project } => {
                    let view =
                        block_on(api::cmd_harness_grok_start(project.as_deref(), false, true))?;
                    println!("{}", serde_json::to_string_pretty(&view)?);
                }
                GrokCommands::Prompt {
                    project,
                    text,
                    file,
                } => {
                    let body = read_prompt_text(text, file)?;
                    let view =
                        block_on(api::cmd_harness_grok_prompt(project.as_deref(), body, true))?;
                    println!("{}", serde_json::to_string_pretty(&view)?);
                }
                GrokCommands::Compact { project } => {
                    let view = block_on(api::cmd_harness_grok_compact(project.as_deref(), true))?;
                    println!("{}", serde_json::to_string_pretty(&view)?);
                }
                GrokCommands::Status { project } => {
                    let view = block_on(api::cmd_harness_grok_status(project.as_deref(), true))?;
                    println!("{}", serde_json::to_string_pretty(&view)?);
                }
                GrokCommands::Shutdown { project } => {
                    let view = block_on(api::cmd_harness_grok_shutdown(project.as_deref(), true))?;
                    println!("{}", serde_json::to_string_pretty(&view)?);
                }
                GrokCommands::Hold { project } => {
                    block_on(api::cmd_harness_grok_hold(project.as_deref()))?;
                }
            },
        },
        Commands::Roles { action } => match action {
            RolesCommands::Show => {
                let bindings = crate::harness::roles::roles_show()?;
                println!("{}", serde_json::to_string_pretty(&bindings)?);
            }
            RolesCommands::Use { target } => {
                let slug = match target {
                    RolesTarget::Grok => "grok",
                    RolesTarget::Cursor => "cursor",
                };
                let bindings = crate::harness::roles::roles_use(slug)?;
                println!("{}", serde_json::to_string_pretty(&bindings)?);
            }
            RolesCommands::Set {
                role,
                harness,
                command,
                model,
                clear_model,
            } => {
                let bindings = crate::harness::roles::roles_set(
                    &role,
                    harness.as_deref(),
                    command.as_deref(),
                    model.as_deref(),
                    clear_model,
                )?;
                println!("{}", serde_json::to_string_pretty(&bindings)?);
            }
            RolesCommands::Swap {
                role,
                role_flag,
                harness,
                command,
                model,
                clear_model,
                cancel,
                status,
                unlock,
                json,
            } => {
                let role = role.or(role_flag);
                let modes = [cancel, status, unlock].iter().filter(|b| **b).count();
                if modes > 1 {
                    return Err(CoordinatorError::Message(
                        "--cancel, --status and --unlock are mutually exclusive".into(),
                    ));
                }
                if unlock {
                    let info = crate::harness::roles_swap::unlock()?;
                    print_swap_json(json, &info)?;
                    return Ok(());
                }
                if status {
                    let view = crate::harness::roles_swap::status()?;
                    if let Some(role) = role.as_deref() {
                        let mut one = view.clone();
                        one.ledger.pending.retain(|k, _| k == role);
                        one.ledger.last.retain(|k, _| k == role);
                        one.ledger.errors.retain(|k, _| k == role);
                        print_swap_json(json, &one)?;
                    } else {
                        print_swap_json(json, &view)?;
                    }
                    return Ok(());
                }
                let Some(role) = role.as_deref() else {
                    return Err(CoordinatorError::Message(
                        "roles swap requires a role (or --status/--unlock)".into(),
                    ));
                };
                if cancel {
                    let ledger = crate::harness::roles_swap::cancel(role)?;
                    print_swap_json(json, &ledger)?;
                    return Ok(());
                }
                let partial = crate::harness::roles_swap::PartialBinding {
                    harness,
                    command,
                    model,
                    clear_model,
                };
                let outcome = match crate::harness::roles_swap::swap(role, partial) {
                    Ok(outcome) => outcome,
                    Err(e) => {
                        if json {
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&serde_json::json!({
                                    "role": role,
                                    "applied": false,
                                    "deferred": false,
                                    "refused": true,
                                    "message": e.to_string(),
                                }))?
                            );
                        }
                        return Err(e);
                    }
                };
                if json {
                    println!("{}", serde_json::to_string_pretty(&outcome)?);
                } else {
                    println!("{}", outcome.message);
                }
            }
            RolesCommands::Rollback { role, from, json } => {
                let outcome = match crate::harness::roles_swap::rollback(&role, from.as_deref()) {
                    Ok(outcome) => outcome,
                    Err(e) => {
                        if json {
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&serde_json::json!({
                                    "role": role,
                                    "applied": false,
                                    "deferred": false,
                                    "refused": true,
                                    "message": e.to_string(),
                                }))?
                            );
                        }
                        return Err(e);
                    }
                };
                if json {
                    println!("{}", serde_json::to_string_pretty(&outcome)?);
                } else {
                    println!("{}", outcome.message);
                }
            }
            RolesCommands::Prove {
                role,
                project,
                json,
            } => {
                let view = crate::harness::roles_swap::prove(&role, project.as_deref())?;
                print_swap_json(json, &view)?;
            }
        },
        Commands::Notify { action } => match action {
            NotifyCommands::HermesTest { project, progress } => {
                let project_id = match project {
                    Some(p) => api::resolve_selected(Some(&p), true)?.id,
                    None => "hermes-test".into(),
                };
                let outcome = if progress {
                    let event = crate::notify::hermes::synthetic_progress_event(project_id);
                    crate::notify::hermes::probe_progress(&event)
                } else {
                    let event = crate::notify::hermes::synthetic_event(project_id);
                    crate::notify::hermes::probe(&event)
                };
                match outcome {
                    crate::notify::hermes::ProbeOutcome::Skipped(reason) => {
                        println!("hermes skipped: {reason}");
                    }
                    crate::notify::hermes::ProbeOutcome::Delivered { status } => {
                        println!("hermes delivered HTTP {status}");
                    }
                    crate::notify::hermes::ProbeOutcome::Failed(e) => {
                        return Err(e);
                    }
                }
            }
            NotifyCommands::FleetSummary => {
                let event = crate::notify::build_fleet_summary_event(chrono::Utc::now())?;
                match crate::notify::hermes::probe_fleet(&event) {
                    crate::notify::hermes::ProbeOutcome::Skipped(reason) => {
                        println!("hermes skipped: {reason}");
                    }
                    crate::notify::hermes::ProbeOutcome::Delivered { status } => {
                        println!("hermes delivered HTTP {status}");
                    }
                    crate::notify::hermes::ProbeOutcome::Failed(e) => {
                        return Err(e);
                    }
                }
            }
        },
        Commands::Decision { action } => match action {
            DecisionCommands::Record {
                project,
                track,
                by,
                supersede,
                sentence_file,
            } => {
                let sentence = read_decision_sentence(sentence_file.as_deref())?;
                match api::cmd_decision_record(
                    project.as_deref(),
                    &track,
                    &by,
                    &sentence,
                    supersede,
                    true,
                )? {
                    crate::workflow::decision::RecordEffect::Wrote => {
                        println!("decision recorded");
                    }
                    crate::workflow::decision::RecordEffect::Unchanged => {
                        println!("decision unchanged");
                    }
                }
            }
            DecisionCommands::Show {
                project,
                track,
                json,
            } => {
                let text = api::cmd_decision_show(project.as_deref(), &track, json, true)?;
                print!("{text}");
            }
            DecisionCommands::List {
                project,
                track,
                json,
            } => {
                let text = api::cmd_decision_list(project.as_deref(), &track, json, true)?;
                print!("{text}");
            }
        },
        Commands::Policy { action } => match action {
            PolicyCommands::Show { project } => {
                let text = api::cmd_policy_show(project.as_deref(), true)?;
                print!("{text}");
            }
            PolicyCommands::Set {
                project,
                name,
                action,
                threshold,
            } => {
                api::cmd_policy_set(project.as_deref(), &name, &action, threshold, true)?;
                println!("policy set {name}");
            }
            PolicyCommands::Approve { project, name } => {
                let fingerprint = api::cmd_policy_approve(project.as_deref(), &name, true)?;
                println!("policy approved {name} {fingerprint}");
            }
        },
    }
    Ok(())
}

fn read_decision_sentence(
    sentence_file: Option<&std::path::Path>,
) -> Result<String, CoordinatorError> {
    let raw = if let Some(path) = sentence_file {
        std::fs::read_to_string(path)?
    } else {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
        buf
    };
    crate::workflow::decision::normalize_sentence_input(&raw)
}

/// `serve --check`: print JSON, exit 0 if coordinator health, 1 otherwise.
/// Does not bind and does not write a lease.
fn serve_check(port: Option<u16>) -> Result<(), CoordinatorError> {
    let report = crate::watch::serve_check_report(port);
    let body = if report.ok {
        serde_json::json!({
            "ok": true,
            "service": "coordinator",
            "port": report.port,
            "source": report.source,
        })
    } else {
        serde_json::json!({
            "ok": false,
            "port": report.port,
            "source": report.source,
        })
    };
    println!("{}", serde_json::to_string(&body)?);
    if report.ok {
        Ok(())
    } else {
        Err(CoordinatorError::Message(format!(
            "no coordinator serve on 127.0.0.1:{}",
            report.port
        )))
    }
}

fn block_on<T>(
    fut: impl std::future::Future<Output = Result<T, CoordinatorError>>,
) -> Result<T, CoordinatorError> {
    tokio::runtime::Runtime::new()
        .map_err(|e| CoordinatorError::Message(format!("failed to start async runtime: {e}")))?
        .block_on(fut)
}

fn read_prompt_text(
    text: Option<String>,
    file: Option<PathBuf>,
) -> Result<String, CoordinatorError> {
    match (text, file) {
        (Some(t), None) => Ok(t),
        (None, Some(p)) => std::fs::read_to_string(p).map_err(CoordinatorError::from),
        (Some(_), Some(_)) => Err(CoordinatorError::Message(
            "pass only one of --text or --file".into(),
        )),
        (None, None) => Err(CoordinatorError::Message(
            "harness grok prompt requires --text or --file".into(),
        )),
    }
}

/// 0109 registry target: an explicit conductor dir plus the registered project (if any).
struct RegistryTarget {
    conductor_dir: PathBuf,
    record: Option<crate::registry::ProjectRecord>,
}

fn canonical(path: &std::path::Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Exactly one of `--project` / `--conductor-dir`; neither infers from cwd.
fn resolve_registry_target(
    project: Option<&str>,
    conductor_dir: Option<&std::path::Path>,
) -> std::result::Result<RegistryTarget, CoordinatorError> {
    if project.is_some() && conductor_dir.is_some() {
        return Err(CoordinatorError::Message(
            "pass exactly one of --project or --conductor-dir".into(),
        ));
    }
    if let Some(id) = project {
        let rec = api::resolve_selected(Some(id), true)?;
        let dir = crate::layout::resolve(&rec).conductor_dir;
        return Ok(RegistryTarget {
            conductor_dir: dir,
            record: Some(rec),
        });
    }
    if let Some(dir) = conductor_dir {
        let want = canonical(dir);
        let rec = api::project_list()?
            .into_iter()
            .find(|r| canonical(&crate::layout::resolve(r).conductor_dir) == want);
        return Ok(RegistryTarget {
            conductor_dir: dir.to_path_buf(),
            record: rec,
        });
    }
    let rec = api::resolve_selected(None, true)?;
    let dir = crate::layout::resolve(&rec).conductor_dir;
    Ok(RegistryTarget {
        conductor_dir: dir,
        record: Some(rec),
    })
}

/// Hold the run-state lock and refuse `Running`/`Paused` (import/set-status posture).
fn with_idle_project<T>(
    target: &RegistryTarget,
    f: impl FnOnce() -> std::result::Result<T, CoordinatorError>,
) -> std::result::Result<T, CoordinatorError> {
    match &target.record {
        None => f(),
        Some(rec) => crate::state::with_run_state_lock(rec, || {
            let state = crate::state::load_run_state(rec)?;
            if matches!(
                state.status,
                crate::state::RunStatus::Running | crate::state::RunStatus::Paused
            ) {
                return Err(CoordinatorError::Message(format!(
                    "project is {}; refusing a registry mutation while a run is live (stop first)",
                    state.status
                )));
            }
            f()
        }),
    }
}

fn cmd_registry_import(
    project: Option<&str>,
    conductor_dir: Option<&Path>,
    dry_run: bool,
    force: bool,
) -> std::result::Result<(), CoordinatorError> {
    let target = resolve_registry_target(project, conductor_dir)?;
    let dir = target.conductor_dir.clone();
    with_idle_project(&target, || {
        let report = crate::registry_store::import_at(
            &dir,
            crate::registry_store::ImportOpts { dry_run, force },
        )?;
        if let Some(rec) = &target.record
            && let Ok(state_dir) = crate::state::resolve_state_dir(rec)
        {
            let path = state_dir.join("registry").join(format!(
                "import-{}.json",
                chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
            ));
            let _ = crate::persist::atomic_write_json(&path, &report);
        }
        println!("{}", serde_json::to_string_pretty(&report)?);
        if report.status != "ok" {
            return Err(CoordinatorError::Message(format!(
                "registry import refused: {}",
                report.reason.unwrap_or_else(|| report.status.clone())
            )));
        }
        Ok(())
    })
}

fn cmd_registry_set_status(
    project: Option<&str>,
    conductor_dir: Option<&Path>,
    track: &str,
    status: &str,
    clear_nostart: bool,
    dry_run: bool,
) -> std::result::Result<(), CoordinatorError> {
    let target = resolve_registry_target(project, conductor_dir)?;
    let dir = target.conductor_dir.clone();
    with_idle_project(&target, || {
        if dry_run {
            let ids =
                crate::registry_store::preview_set_statuses_at(&dir, track, status, clear_nostart)?;
            let out = serde_json::json!({
                "status": "ok",
                "dry_run": true,
                "conductor_dir": dir.display().to_string(),
                "track": track,
                "changed": ids,
            });
            println!("{}", serde_json::to_string_pretty(&out)?);
            return Ok(());
        }
        let changes = vec![(track.to_string(), status.to_string())];
        let outcome =
            crate::registry_store::set_statuses_at(&dir, &changes, "operator", clear_nostart)?;
        let changed = match outcome {
            crate::registry_store::WriteOutcome::NoStore => {
                return Err(CoordinatorError::Message(format!(
                    "no track store at {}; set-status has no markdown fallback",
                    crate::registry_store::store_path_at(&dir).display()
                )));
            }
            crate::registry_store::WriteOutcome::Unchanged => Vec::new(),
            crate::registry_store::WriteOutcome::Changed(ids) => ids,
        };
        if changed.is_empty() {
            return Err(CoordinatorError::Message(format!(
                "no row matched --track {track}"
            )));
        }
        let out = serde_json::json!({
            "status": "ok",
            "conductor_dir": dir.display().to_string(),
            "track": track,
            "changed": changed,
            "store_path": crate::registry_store::store_path_at(&dir).display().to_string(),
            "view_path": dir.join("conductor.md").display().to_string(),
            "clear_nostart": clear_nostart,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        Ok(())
    })
}

/// One shape for every `roles swap|rollback|prove` JSON payload.
fn print_swap_json<T: serde::Serialize>(_json: bool, value: &T) -> Result<(), CoordinatorError> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Test seam: the real `Cli::parse` + dispatch, so exit codes are the shipped ones.
#[cfg(test)]
pub(crate) fn exit_code_for_test(args: &[&str]) -> i32 {
    let mut argv: Vec<&str> = vec!["coordinator"];
    argv.extend_from_slice(args);
    match Cli::try_parse_from(argv) {
        Ok(cli) => match dispatch(cli) {
            Ok(()) => 0,
            Err(e) => e.exit_code(),
        },
        Err(_) => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[test]
    fn run_has_driver_flag() {
        let cmd = Cli::command();
        let run = cmd.find_subcommand("run").expect("run");
        assert!(
            run.get_arguments().any(|a| a.get_id() == "driver"),
            "run --driver"
        );
        assert!(
            run.get_arguments().any(|a| a.get_id() == "detach"),
            "run --detach"
        );
        assert!(
            run.get_arguments().any(|a| a.get_id() == "timeout_secs"),
            "run --timeout-secs"
        );
        assert!(
            run.get_arguments().any(|a| a.get_id() == "serve_port"),
            "run --serve-port"
        );
        assert!(
            run.get_arguments().any(|a| a.get_id() == "skip_preflight"),
            "run --skip-preflight"
        );
        let about = run.get_about().map(|s| s.to_string()).unwrap_or_default();
        assert!(
            about.contains("Idle/Stopped"),
            "run about should say it ticks until Idle/Stopped: {about}"
        );
        let mut run_help = run.clone();
        let help = run_help.render_long_help().to_string();
        assert!(
            help.contains("--serve-port"),
            "run --help should mention --serve-port: {help}"
        );
    }

    #[test]
    fn policy_set_parses_builtin_name_and_action() {
        let cmd = Cli::try_parse_from([
            "coordinator",
            "policy",
            "set",
            "--name",
            "dependency-manifest",
            "--action",
            "require-approval",
            "--threshold",
            "3",
            "--project",
            "p",
        ])
        .unwrap();
        match cmd.command {
            Commands::Policy { action } => match action {
                PolicyCommands::Set {
                    project,
                    name,
                    action,
                    threshold,
                } => {
                    assert_eq!(project.as_deref(), Some("p"));
                    assert_eq!(name, "dependency-manifest");
                    assert_eq!(action, "require-approval");
                    assert_eq!(threshold, Some(3));
                }
                other => panic!("expected policy set, got {other:?}"),
            },
            other => panic!("expected policy, got {other:?}"),
        }
        let show = Cli::try_parse_from(["coordinator", "policy", "show"]).unwrap();
        assert!(matches!(
            show.command,
            Commands::Policy {
                action: PolicyCommands::Show { project: None }
            }
        ));
        let approve = Cli::try_parse_from([
            "coordinator",
            "policy",
            "approve",
            "--name",
            "restore-before-publish",
        ])
        .unwrap();
        assert!(matches!(
            approve.command,
            Commands::Policy {
                action: PolicyCommands::Approve { .. }
            }
        ));
    }

    #[test]
    fn restore_subcommand_parses_project_and_discard() {
        let cmd = Cli::command();
        let restore = cmd.find_subcommand("restore").expect("restore");
        assert!(restore.get_arguments().any(|a| a.get_id() == "project"));
        assert!(restore.get_arguments().any(|a| a.get_id() == "discard"));
        let bare = Cli::try_parse_from(["coordinator", "restore"]).unwrap();
        match bare.command {
            Commands::Restore { project, discard } => {
                assert!(project.is_none());
                assert!(!discard);
            }
            other => panic!("expected restore, got {other:?}"),
        }
        let flagged =
            Cli::try_parse_from(["coordinator", "restore", "--project", "p", "--discard"]).unwrap();
        match flagged.command {
            Commands::Restore { project, discard } => {
                assert_eq!(project.as_deref(), Some("p"));
                assert!(discard);
            }
            other => panic!("expected restore, got {other:?}"),
        }
    }

    #[test]
    fn self_update_subcommand_in_help() {
        let cmd = Cli::command();
        let sub = cmd.find_subcommand("self-update").expect("self-update");
        for flag in ["repo", "install", "artifact", "check"] {
            assert!(
                sub.get_arguments().any(|arg| arg.get_id() == flag),
                "self-update --{flag}"
            );
        }
        let mut help = sub.clone();
        let help = help.render_long_help().to_string();
        assert!(
            help.contains("--install"),
            "self-update --help should document --install: {help}"
        );
        assert!(
            help.contains("cargo run -- self-update") || help.contains("self-blocks"),
            "help should warn that cargo run self-blocks: {help}"
        );
        let parsed = Cli::try_parse_from(["coordinator", "self-update", "--check"]).expect("parse");
        assert!(matches!(
            parsed.command,
            Commands::SelfUpdate { check: true, .. }
        ));
    }

    #[test]
    fn doctor_subcommand_in_help() {
        let cmd = Cli::command();
        let doctor = cmd.find_subcommand("doctor").expect("doctor");
        assert!(
            doctor.get_arguments().any(|a| a.get_id() == "project"),
            "doctor --project"
        );
        let parsed = Cli::try_parse_from(["coordinator", "doctor"]).unwrap();
        match parsed.command {
            Commands::Doctor { project } => assert!(project.is_none()),
            other => panic!("expected doctor, got {other:?}"),
        }
    }

    #[test]
    fn run_skip_preflight_does_not_conflict_with_detach() {
        let parsed = Cli::try_parse_from(["coordinator", "run", "--detach", "--skip-preflight"]);
        assert!(parsed.is_ok(), "{parsed:?}");
        match parsed.unwrap().command {
            Commands::Run {
                detach,
                skip_preflight,
                ..
            } => {
                assert!(detach);
                assert!(skip_preflight);
            }
            other => panic!("expected run, got {other:?}"),
        }
        let mut run = Cli::command().find_subcommand("run").unwrap().clone();
        let help = run.render_long_help().to_string();
        assert!(
            help.contains("--skip-preflight"),
            "run --help should mention --skip-preflight: {help}"
        );
    }

    #[test]
    fn run_detach_conflicts_with_timeout_secs() {
        let err = Cli::try_parse_from(["coordinator", "run", "--detach", "--timeout-secs", "1"]);
        assert!(err.is_err(), "detach + timeout-secs must conflict");
    }

    #[test]
    fn run_detach_conflicts_with_serve_port() {
        let err = Cli::try_parse_from(["coordinator", "run", "--detach", "--serve-port", "7500"]);
        assert!(err.is_err(), "detach + serve-port must conflict");
    }

    #[test]
    fn serve_has_check_and_optional_port() {
        let cmd = Cli::command();
        let serve = cmd.find_subcommand("serve").expect("serve");
        assert!(
            serve.get_arguments().any(|a| a.get_id() == "check"),
            "serve --check"
        );
        assert!(
            serve.get_arguments().any(|a| a.get_id() == "port"),
            "serve --port"
        );
        let mut serve_help = serve.clone();
        let help = serve_help.render_long_help().to_string();
        assert!(
            help.contains("--check"),
            "serve --help should mention --check: {help}"
        );
        let bare = Cli::try_parse_from(["coordinator", "serve", "--check"]).unwrap();
        match bare.command {
            Commands::Serve { port, check } => {
                assert!(check);
                assert!(port.is_none(), "default --check must not pin port 7420");
            }
            other => panic!("expected serve, got {other:?}"),
        }
        let flagged =
            Cli::try_parse_from(["coordinator", "serve", "--check", "--port", "7500"]).unwrap();
        match flagged.command {
            Commands::Serve { port, check } => {
                assert!(check);
                assert_eq!(port, Some(7500));
            }
            other => panic!("expected serve, got {other:?}"),
        }
    }

    #[test]
    fn failure_show_in_help() {
        let cmd = Cli::command();
        let failure = cmd.find_subcommand("failure").expect("failure");
        assert!(failure.find_subcommand("show").is_some());
    }

    #[test]
    fn ui_subcommand_in_help() {
        let cmd = Cli::command();
        let ui = cmd.find_subcommand("ui").expect("ui");
        assert!(
            ui.get_arguments().any(|a| a.get_id() == "port"),
            "ui --port"
        );
        let about = ui.get_about().map(|s| s.to_string()).unwrap_or_default();
        let mut ui_help = ui.clone();
        let help = ui_help.render_long_help().to_string();
        let combined = format!("{about}\n{help}");
        assert!(
            !combined.contains("Requires `--features ui`")
                && !combined.contains("Requires --features ui"),
            "ui help must not require --features ui: about={about} help={help}"
        );
    }

    #[test]
    fn notify_hermes_test_in_help() {
        let cmd = Cli::command();
        let notify = cmd.find_subcommand("notify").expect("notify");
        let hermes = notify.find_subcommand("hermes-test").expect("hermes-test");
        assert!(
            hermes.get_arguments().any(|a| a.get_id() == "progress"),
            "hermes-test --progress"
        );
        let fleet = notify
            .find_subcommand("fleet-summary")
            .expect("fleet-summary");
        assert!(
            fleet
                .get_arguments()
                .all(|a| a.get_id().as_str() != "project"),
            "fleet-summary has no --project"
        );
    }

    #[test]
    fn project_set_has_notify_progress_flag() {
        let cmd = Cli::command();
        let project = cmd.find_subcommand("project").expect("project");
        let set = project.find_subcommand("set").expect("set");
        assert!(
            set.get_arguments().any(|a| a.get_id() == "notify_progress"),
            "set --notify-progress"
        );
        assert!(
            set.get_arguments()
                .any(|a| a.get_id() == "worktree_isolation"),
            "set --worktree-isolation"
        );
        assert!(
            set.get_arguments()
                .any(|a| a.get_id() == "self_continuation"),
            "set --self-continuation"
        );
        assert!(
            set.get_arguments().any(|a| a.get_id() == "ci_fix_routing"),
            "set --ci-fix-routing"
        );
        let add = project.find_subcommand("add").expect("add");
        assert!(
            add.get_arguments()
                .all(|a| a.get_id().as_str() != "self_continuation"),
            "add must not grow --self-continuation"
        );
        assert!(
            add.get_arguments()
                .all(|a| a.get_id().as_str() != "ci_fix_routing"),
            "add must not grow --ci-fix-routing"
        );
    }

    #[test]
    fn project_add_and_set_have_auto_start_flag() {
        let cmd = Cli::command();
        let project = cmd.find_subcommand("project").expect("project");
        let set = project.find_subcommand("set").expect("set");
        let add = project.find_subcommand("add").expect("add");
        assert!(
            set.get_arguments().any(|a| a.get_id() == "auto_start"),
            "set --auto-start"
        );
        assert!(
            add.get_arguments().any(|a| a.get_id() == "auto_start"),
            "add --auto-start"
        );
    }

    #[test]
    fn project_set_and_add_have_skill_alias_flags() {
        let mut cmd = Cli::command();
        let project = cmd.find_subcommand_mut("project").unwrap();
        let set = project.find_subcommand_mut("set").unwrap();
        assert!(set.get_arguments().any(|a| a.get_id() == "skill_aliases"));
        assert!(
            set.get_arguments()
                .any(|a| a.get_id() == "clear_skill_alias")
        );
        assert!(
            set.get_arguments()
                .any(|a| a.get_id() == "clear_skill_aliases")
        );
        let add = project.find_subcommand_mut("add").unwrap();
        assert!(add.get_arguments().any(|a| a.get_id() == "skill_aliases"));
        assert!(
            add.get_arguments()
                .any(|a| a.get_id() == "clear_skill_alias")
        );
        assert!(
            add.get_arguments()
                .any(|a| a.get_id() == "clear_skill_aliases")
        );
    }

    #[test]
    fn project_set_skill_alias_last_flag_wins_and_rejects() {
        let cli = Cli::try_parse_from([
            "coordinator",
            "project",
            "set",
            "--skill-alias",
            "plan=plan-track",
            "--skill-alias",
            "plan=other-plan",
        ])
        .unwrap();
        match cli.command {
            Commands::Project {
                action: ProjectCommands::Set { skill_aliases, .. },
            } => {
                assert_eq!(
                    skill_aliases,
                    vec![
                        ("plan".into(), "plan-track".into()),
                        ("plan".into(), "other-plan".into()),
                    ]
                );
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(
            Cli::try_parse_from(["coordinator", "project", "set", "--skill-alias", "nope=x"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "coordinator",
                "project",
                "set",
                "--skill-alias",
                "plan=../x"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "coordinator",
                "project",
                "set",
                "--clear-skill-alias",
                "nope"
            ])
            .is_err()
        );
    }

    #[test]
    fn project_set_and_add_have_phase_timeout_flags() {
        let cmd = Cli::command();
        let project = cmd.find_subcommand("project").expect("project");
        let set = project.find_subcommand("set").expect("set");
        let add = project.find_subcommand("add").expect("add");
        assert!(
            set.get_arguments().any(|a| a.get_id() == "phase_timeouts"),
            "set --phase-timeout"
        );
        assert!(
            set.get_arguments()
                .any(|a| a.get_id() == "clear_phase_timeout"),
            "set --clear-phase-timeout"
        );
        assert!(
            set.get_arguments()
                .any(|a| a.get_id() == "clear_phase_timeouts"),
            "set --clear-phase-timeouts"
        );
        assert!(
            add.get_arguments().any(|a| a.get_id() == "phase_timeouts"),
            "add --phase-timeout"
        );
        assert!(
            set.get_arguments()
                .any(|a| a.get_id() == "status_words_ready"),
            "set --status-word-ready"
        );
        assert!(
            set.get_arguments()
                .any(|a| a.get_id() == "clear_status_words_ready"),
            "set --clear-status-words-ready"
        );
    }

    #[test]
    fn project_set_status_word_ready_is_repeatable() {
        let cli = Cli::try_parse_from([
            "coordinator",
            "project",
            "set",
            "--status-word-ready",
            "Ready — full plan @ 072399b6",
            "--status-word-ready",
            "Ready — implement on GO",
        ])
        .unwrap();
        match cli.command {
            Commands::Project {
                action:
                    ProjectCommands::Set {
                        status_words_ready, ..
                    },
            } => {
                assert_eq!(
                    status_words_ready,
                    vec![
                        "Ready — full plan @ 072399b6".to_string(),
                        "Ready — implement on GO".to_string(),
                    ]
                );
            }
            other => panic!("expected project set, got {other:?}"),
        }
    }

    #[test]
    fn project_set_phase_timeout_last_flag_wins() {
        let cli = Cli::try_parse_from([
            "coordinator",
            "project",
            "set",
            "--phase-timeout",
            "plan=1",
            "--phase-timeout",
            "plan=3600",
        ])
        .unwrap();
        match cli.command {
            Commands::Project {
                action: ProjectCommands::Set { phase_timeouts, .. },
            } => {
                let map: BTreeMap<_, _> = phase_timeouts.into_iter().collect();
                assert_eq!(map.get("plan"), Some(&3600));
            }
            other => panic!("expected project set, got {other:?}"),
        }
    }

    #[test]
    fn project_set_phase_timeout_accepts_plan_review_slot() {
        let cli = Cli::try_parse_from([
            "coordinator",
            "project",
            "set",
            "--phase-timeout",
            "plan_review_slot=600",
        ])
        .unwrap();
        match cli.command {
            Commands::Project {
                action: ProjectCommands::Set { phase_timeouts, .. },
            } => {
                assert_eq!(phase_timeouts, vec![("plan_review_slot".into(), 600)]);
            }
            other => panic!("expected project set, got {other:?}"),
        }
    }

    #[test]
    fn project_set_phase_timeout_zero_and_unknown_fail_parse() {
        assert!(
            Cli::try_parse_from(["coordinator", "project", "set", "--phase-timeout", "plan=0"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from(["coordinator", "project", "set", "--phase-timeout", "nope=1"])
                .is_err()
        );
    }

    #[test]
    fn harness_grok_commands_in_help() {
        let cmd = Cli::command();
        let harness = cmd.find_subcommand("harness").expect("harness");
        let grok = harness.find_subcommand("grok").expect("grok");
        for expected in ["start", "prompt", "compact", "status", "shutdown"] {
            assert!(
                grok.find_subcommand(expected).is_some(),
                "missing {expected}"
            );
        }
    }

    #[test]
    fn roles_commands_in_help() {
        let cmd = Cli::command();
        let roles = cmd.find_subcommand("roles").expect("roles");
        assert!(roles.find_subcommand("show").is_some(), "missing show");
        assert!(roles.find_subcommand("use").is_some(), "missing use");
        assert!(
            roles.find_subcommand("set").is_some(),
            "missing set (per-role harness/model editor)"
        );
        let parsed = Cli::try_parse_from([
            "coordinator",
            "roles",
            "set",
            "planner",
            "--harness",
            "cursor",
            "--model",
            "claude-sonnet-5-5[effort=high]",
        ])
        .unwrap();
        match parsed.command {
            Commands::Roles {
                action:
                    RolesCommands::Set {
                        role,
                        harness,
                        model,
                        clear_model,
                        ..
                    },
            } => {
                assert_eq!(role, "planner");
                assert_eq!(harness.as_deref(), Some("cursor"));
                assert_eq!(model.as_deref(), Some("claude-sonnet-5-5[effort=high]"));
                assert!(!clear_model, "clear_model must default false");
            }
            other => panic!("expected roles set, got {other:?}"),
        }
        let parsed = Cli::try_parse_from(["coordinator", "roles", "use", "cursor"]).unwrap();
        match parsed.command {
            Commands::Roles {
                action:
                    RolesCommands::Use {
                        target: RolesTarget::Cursor,
                    },
            } => {}
            other => panic!("expected roles use cursor, got {other:?}"),
        }
        assert!(
            Cli::try_parse_from(["coordinator", "roles", "use", "opencode"]).is_err(),
            "unknown token must be clap error"
        );
    }

    #[test]
    fn failure_resolve_is_subcommand_not_flag() {
        let parsed = Cli::try_parse_from(["coordinator", "failure", "resolve"]).unwrap();
        match parsed.command {
            Commands::Failure {
                action: FailureCommands::Resolve { project },
            } => assert!(project.is_none()),
            other => panic!("expected failure resolve, got {other:?}"),
        }
        assert!(
            Cli::try_parse_from(["coordinator", "failure", "--resolve"]).is_err(),
            "legacy --resolve flag must not parse"
        );
        assert!(
            Cli::try_parse_from(["coordinator", "failure"]).is_err(),
            "bare failure still requires a subcommand"
        );
        let cmd = Cli::command();
        let failure = cmd.find_subcommand("failure").expect("failure");
        assert!(failure.find_subcommand("show").is_some());
        assert!(failure.find_subcommand("resolve").is_some());
    }

    #[test]
    fn decision_record_help_states_the_limitation() {
        let cmd = Cli::command();
        let decision = cmd.find_subcommand("decision").expect("decision");
        let record = decision.find_subcommand("record").expect("record");
        assert!(record.get_arguments().any(|arg| arg.get_id() == "by"));
        assert!(record.get_arguments().any(|arg| arg.get_id() == "track"));
        assert!(
            record
                .get_arguments()
                .any(|arg| arg.get_id() == "supersede")
        );
        assert!(
            record
                .get_arguments()
                .any(|arg| arg.get_id() == "sentence_file")
        );
        assert!(
            record.get_arguments().all(|arg| arg.get_id() != "sentence"),
            "record must not take --sentence"
        );
        let mut help_cmd = record.clone();
        let help = help_cmd.render_long_help().to_string();
        assert!(
            help.contains(crate::workflow::decision::RECORD_LIMITATION),
            "{help}"
        );
        assert!(!help.contains("cryptographically verified"), "{help}");
        let show = decision.find_subcommand("show").expect("show");
        assert!(show.get_arguments().any(|arg| arg.get_id() == "json"));
        let list = decision.find_subcommand("list").expect("list");
        assert!(list.get_arguments().any(|arg| arg.get_id() == "json"));
    }

    #[test]
    fn roles_swap_rollback_and_prove_parse() {
        assert!(
            Cli::try_parse_from(["coordinator", "roles", "swap", "planner", "--model", "m"])
                .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "coordinator",
                "roles",
                "swap",
                "--status",
                "--role",
                "planner",
                "--json"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from(["coordinator", "roles", "swap", "planner", "--cancel"]).is_ok()
        );
        assert!(Cli::try_parse_from(["coordinator", "roles", "swap", "--unlock"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "coordinator",
                "roles",
                "rollback",
                "planner",
                "--from",
                "snap.json"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "coordinator",
                "roles",
                "prove",
                "planner",
                "--project",
                "p",
                "--json"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from(["coordinator", "roles", "swap", "--help"]).is_err(),
            "clap prints help via Err(DisplayHelp)"
        );
    }
}
// 0109 CLI dispatch tests: the real `Cli::parse` + `dispatch`, so exit codes are shipped.

#[cfg(test)]
mod tests_0109_registry {
    use super::*;
    use clap::CommandFactory;
    use std::path::Path;
    use tempfile::{TempDir, tempdir};

    const HEAD: &str = "| Track | Execution path | Status | Summary |\n\
                        | --- | --- | --- | --- |\n";

    struct CliFx {
        _home: TempDir,
        _ws: TempDir,
        _guard: crate::config::TestEnvGuard,
        record: crate::registry::ProjectRecord,
    }

    impl CliFx {
        fn dir(&self) -> std::path::PathBuf {
            self.record.path.join("conductor")
        }
        fn dir_arg(&self) -> String {
            self.dir().display().to_string()
        }
        fn view(&self) -> std::path::PathBuf {
            self.dir().join("conductor.md")
        }
        fn write_view(&self, body: &str) {
            std::fs::create_dir_all(self.dir()).unwrap();
            std::fs::write(self.view(), body).unwrap();
        }
        fn read_view(&self) -> String {
            std::fs::read_to_string(self.view()).unwrap()
        }
        fn store(&self) -> serde_json::Value {
            let text = std::fs::read_to_string(self.dir().join("track-registry.json")).unwrap();
            serde_json::from_str(&text).unwrap()
        }
        fn store_exists(&self) -> bool {
            self.dir().join("track-registry.json").is_file()
        }
    }

    fn isolate() -> CliFx {
        let names = crate::features::test_feature_env_names();
        let guard = crate::config::TestEnvGuard::capture(&names);
        let home = tempdir().unwrap();
        for name in &names {
            unsafe {
                std::env::remove_var(name);
            }
        }
        unsafe {
            std::env::set_var(crate::config::ENV_COORDINATOR_HOME, home.path());
        }
        let ws = tempdir().unwrap();
        let record =
            crate::api::project_add(ws.path(), crate::registry::ProjectAddOptions::default())
                .unwrap();
        CliFx {
            _home: home,
            _ws: ws,
            _guard: guard,
            record,
        }
    }

    fn ready_row(id: &str) -> String {
        format!("| [{id}-T]({id}-T/spec.md) | `.` | **Ready — not started** | go |\n")
    }

    fn exit(args: &[&str]) -> i32 {
        exit_code_for_test(args)
    }

    #[test]
    fn registry_subcommand_in_help() {
        let mut cmd = Cli::command();
        let mut registry = cmd
            .find_subcommand_mut("registry")
            .expect("registry subcommand")
            .clone();
        let help = registry.render_long_help().to_string();
        for name in ["import", "render", "verify", "set-status"] {
            assert!(
                help.contains(name),
                "registry --help missing {name}: {help}"
            );
        }
        let import_help = registry
            .find_subcommand_mut("import")
            .expect("import")
            .render_long_help()
            .to_string();
        assert!(import_help.contains("--conductor-dir"), "{import_help}");
        assert!(Cli::try_parse_from(["coordinator", "registry", "--help"]).is_err());
    }

    #[test]
    fn registry_verify_cli_exits_zero_after_import() {
        let _lock = crate::config::test_env_lock();
        let fx = isolate();
        fx.write_view(&format!("{HEAD}{}", ready_row("0001")));
        let dir = fx.dir_arg();
        assert_eq!(exit(&["registry", "import", "--conductor-dir", &dir]), 0);
        assert_eq!(exit(&["registry", "verify", "--conductor-dir", &dir]), 0);
        let report = crate::registry_store::verify_at(&fx.dir()).unwrap();
        assert_eq!(report.status, "ok");
        assert!(report.row_diff.missing.is_empty());
        assert!(report.row_diff.extra.is_empty());
    }

    #[test]
    fn registry_cli_exit_codes_at_dispatch() {
        let _lock = crate::config::test_env_lock();
        let fx = isolate();
        // A refused import (cell count) is exit 1.
        fx.write_view(&format!(
            "{HEAD}| [0001-T](0001-T/spec.md) | `.` | **Ready — not started** | a | b | c |\n"
        ));
        let dir = fx.dir_arg();
        assert_eq!(exit(&["registry", "import", "--conductor-dir", &dir]), 1);
        assert!(!fx.store_exists());

        // A clean import is exit 0.
        fx.write_view(&format!("{HEAD}{}", ready_row("0001")));
        assert_eq!(exit(&["registry", "import", "--conductor-dir", &dir]), 0);

        // A drifted view makes verify exit 1.
        fx.write_view(&fx.read_view().replace("| go |", "| drift |"));
        assert_eq!(exit(&["registry", "verify", "--conductor-dir", &dir]), 1);
        assert_eq!(
            exit(&["registry", "render", "--conductor-dir", &dir, "--check"]),
            1
        );

        // set-status on an absent store has no markdown fallback: exit 1.
        let other = tempdir().unwrap();
        std::fs::create_dir_all(other.path()).unwrap();
        let other_dir = other.path().display().to_string();
        assert_eq!(
            exit(&[
                "registry",
                "set-status",
                "--conductor-dir",
                &other_dir,
                "--track",
                "0001",
                "--status",
                "**Completed**",
            ]),
            1
        );
    }

    #[test]
    fn registry_set_status_cli_updates_store_and_view() {
        let _lock = crate::config::test_env_lock();
        let fx = isolate();
        fx.write_view(&format!("{HEAD}{}", ready_row("0001")));
        let dir = fx.dir_arg();
        assert_eq!(exit(&["registry", "import", "--conductor-dir", &dir]), 0);
        assert_eq!(
            exit(&[
                "registry",
                "set-status",
                "--conductor-dir",
                &dir,
                "--track",
                "0001",
                "--status",
                "**Completed** 2026-10-21 — shipped",
            ]),
            0
        );
        let store = fx.store();
        assert_eq!(
            store["rows"][0]["status"],
            "**Completed** 2026-10-21 — shipped"
        );
        assert_eq!(store["rows"][0]["status_source"], "operator");
        assert!(
            fx.read_view()
                .contains("**Completed** 2026-10-21 — shipped")
        );
        assert_eq!(exit(&["registry", "verify", "--conductor-dir", &dir]), 0);
    }

    #[test]
    fn registry_set_status_clear_nostart_and_guards() {
        let _lock = crate::config::test_env_lock();
        let fx = isolate();
        fx.write_view(&format!(
            "{HEAD}| [0109-T](0109-T/spec.md) | `.` | **Ready - not started** <!-- nostart --> | go |\n"
        ));
        let dir = fx.dir_arg();
        assert_eq!(exit(&["registry", "import", "--conductor-dir", &dir]), 0);
        assert_eq!(
            exit(&[
                "registry",
                "set-status",
                "--conductor-dir",
                &dir,
                "--track",
                "0109",
                "--status",
                "**Completed** 2026-10-21 — done",
                "--clear-nostart",
            ]),
            0
        );
        let store = fx.store();
        assert_eq!(
            store["rows"][0]["status"],
            "**Completed** 2026-10-21 — done"
        );
        assert_eq!(store["rows"][0]["nostart"], false);
        assert!(!fx.read_view().contains("<!-- nostart -->"));

        // A drifted view refuses and writes nothing.
        fx.write_view(&format!("{}\nhand edit\n", fx.read_view()));
        let status_before = fx.store()["rows"][0]["status"].clone();
        assert_eq!(
            exit(&[
                "registry",
                "set-status",
                "--conductor-dir",
                &dir,
                "--track",
                "0109",
                "--status",
                "**Blocked**",
            ]),
            1
        );
        assert_eq!(fx.store()["rows"][0]["status"], status_before);

        // A Running project refuses the operator flip under the run-state lock.
        fx.write_view(&format!(
            "{HEAD}| [0109-T](0109-T/spec.md) | `.` | **Ready - not started** | go |\n"
        ));
        assert_eq!(
            exit(&["registry", "import", "--conductor-dir", &dir, "--force"]),
            0
        );
        let mut state = crate::state::RunState::idle(&fx.record.id);
        state.status = crate::state::RunStatus::Running;
        state.track_id = Some("0109".into());
        crate::state::save_run_state(&fx.record, &state).unwrap();
        let status_before = fx.store()["rows"][0]["status"].clone();
        assert_eq!(
            exit(&[
                "registry",
                "set-status",
                "--project",
                &fx.record.id,
                "--track",
                "0109",
                "--status",
                "**Blocked**",
            ]),
            1
        );
        assert_eq!(fx.store()["rows"][0]["status"], status_before);
    }

    #[test]
    fn import_refuses_registered_conductor_dir_while_running() {
        let _lock = crate::config::test_env_lock();
        let fx = isolate();
        fx.write_view(&format!("{HEAD}{}", ready_row("0001")));
        let mut state = crate::state::RunState::idle(&fx.record.id);
        state.status = crate::state::RunStatus::Running;
        state.track_id = Some("0001".into());
        crate::state::save_run_state(&fx.record, &state).unwrap();

        let dir = fx.dir_arg();
        assert_eq!(exit(&["registry", "import", "--conductor-dir", &dir]), 1);
        assert!(!fx.store_exists());
        assert_eq!(exit(&["registry", "import", "--project", &fx.record.id]), 1);
        assert!(!fx.store_exists());

        // Paused refuses too.
        state.status = crate::state::RunStatus::Paused;
        crate::state::save_run_state(&fx.record, &state).unwrap();
        assert_eq!(exit(&["registry", "import", "--conductor-dir", &dir]), 1);
        assert!(!fx.store_exists());
    }

    #[allow(dead_code)]
    fn _unused(path: &Path) {
        let _ = path;
    }
}
