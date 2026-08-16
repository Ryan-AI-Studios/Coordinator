//! Clap CLI surface (tracks 0004–0006).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::api;
use crate::config::DEFAULT_SERVE_PORT;
use crate::error::CoordinatorError;
use crate::layout::LayoutProfile;
use crate::registry::{ProjectAddOptions, ProjectSetOptions};
use crate::server;
use crate::workflow::timeouts::parse_phase_timeout;

fn parse_auto_merge(s: &str) -> std::result::Result<bool, String> {
    match s.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!("expected true|false, got {other}")),
    }
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
    /// Start the canonical workflow at `plan` and tick until Idle/Stopped
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
    Serve {
        #[arg(long, default_value_t = DEFAULT_SERVE_PORT)]
        port: u16,
    },
    /// Open the Local Ops Console (Status Surface). Requires `--features ui`.
    Ui {
        #[arg(long, default_value_t = DEFAULT_SERVE_PORT)]
        port: u16,
    },
    /// Drive a harness session (Grok ACP this track)
    Harness {
        #[command(subcommand)]
        action: HarnessCommands,
    },
    /// Notify probes (Hermes inbound webhook)
    Notify {
        #[command(subcommand)]
        action: NotifyCommands,
    },
}

#[derive(Debug, Subcommand)]
pub enum NotifyCommands {
    /// Probe the opt-in Hermes inbound webhook (no artifact, no toast)
    HermesTest {
        #[arg(long)]
        project: Option<String>,
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
        /// Repeatable. Canonical phase id = seconds (>0).
        #[arg(long = "phase-timeout", value_name = "PHASE=SECS", value_parser = parse_phase_timeout)]
        phase_timeouts: Vec<(String, u64)>,
    },
    /// List registered projects (JSON; includes profile + execution summary)
    List,
    /// Show raw record + resolved layout paths
    Show {
        #[arg(long)]
        project: Option<String>,
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
        /// Repeatable. Canonical phase id = seconds (>0).
        #[arg(long = "phase-timeout", value_name = "PHASE=SECS", value_parser = parse_phase_timeout)]
        phase_timeouts: Vec<(String, u64)>,
        /// Repeatable. Drop one stored project override.
        #[arg(long = "clear-phase-timeout", value_name = "PHASE")]
        clear_phase_timeout: Vec<String>,
        /// Wipe the project phase-timeout map.
        #[arg(long = "clear-phase-timeouts")]
        clear_phase_timeouts: bool,
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
}

/// Parse args and dispatch; returns process exit code.
pub fn run() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
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
                phase_timeouts,
            } => {
                let layout_profile = LayoutProfile::parse(&profile)?;
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
                };
                let rec = api::project_add(&path, opts)?;
                println!("{}", serde_json::to_string_pretty(&rec)?);
            }
            ProjectCommands::List => {
                let list = api::project_list()?;
                println!("{}", serde_json::to_string_pretty(&list)?);
            }
            ProjectCommands::Show { project } => {
                let view = api::project_show(project.as_deref())?;
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
                phase_timeouts,
                clear_phase_timeout,
                clear_phase_timeouts,
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
                    phase_timeouts_secs,
                    clear_phase_timeouts,
                    clear_phase_timeout,
                };
                let rec = api::project_set(project.as_deref(), opts)?;
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
            let view = api::status(project.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&view)?);
        }
        Commands::Run {
            project,
            track,
            driver,
            detach,
            timeout_secs,
        } => {
            let view = api::cmd_run_cli(
                project.as_deref(),
                track,
                driver.as_deref(),
                api::RunCliOpts {
                    detach,
                    timeout_secs,
                    detect_serve_port: if detach {
                        None
                    } else {
                        Some(DEFAULT_SERVE_PORT)
                    },
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&view)?);
        }
        Commands::Pause { project } => {
            let view = api::cmd_pause(project.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&view)?);
        }
        Commands::Resume { project } => {
            let view = api::cmd_resume(project.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&view)?);
        }
        Commands::Stop { project } => {
            let view = api::cmd_stop(project.as_deref())?;
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
                )?;
                println!("{}", serde_json::to_string_pretty(&view)?);
            }
            OutcomeCommands::Show { project } => match api::cmd_outcome_show(project.as_deref())? {
                Some(o) => println!("{}", serde_json::to_string_pretty(&o)?),
                None => {
                    println!("null");
                }
            },
        },
        Commands::Failure { action } => match action {
            FailureCommands::Show { project } => match api::cmd_failure_show(project.as_deref())? {
                Some(v) => print!("{}", v.body),
                None => {
                    return Err(CoordinatorError::Message("no failure artifact".into()));
                }
            },
        },
        Commands::Wait {
            project,
            timeout_secs,
        } => {
            let view = api::cmd_wait(project.as_deref(), timeout_secs)?;
            println!("{}", serde_json::to_string_pretty(&view)?);
        }
        Commands::Serve { port } => {
            block_on(server::serve(port))?;
        }
        Commands::Ui { port } => {
            crate::ui::run_cli(port)?;
        }
        Commands::Harness { action } => match action {
            HarnessCommands::Grok { action } => match action {
                GrokCommands::Start { project } => {
                    let view = block_on(api::cmd_harness_grok_start(project.as_deref(), false))?;
                    println!("{}", serde_json::to_string_pretty(&view)?);
                }
                GrokCommands::Prompt {
                    project,
                    text,
                    file,
                } => {
                    let body = read_prompt_text(text, file)?;
                    let view = block_on(api::cmd_harness_grok_prompt(project.as_deref(), body))?;
                    println!("{}", serde_json::to_string_pretty(&view)?);
                }
                GrokCommands::Compact { project } => {
                    let view = block_on(api::cmd_harness_grok_compact(project.as_deref()))?;
                    println!("{}", serde_json::to_string_pretty(&view)?);
                }
                GrokCommands::Status { project } => {
                    let view = block_on(api::cmd_harness_grok_status(project.as_deref()))?;
                    println!("{}", serde_json::to_string_pretty(&view)?);
                }
                GrokCommands::Shutdown { project } => {
                    let view = block_on(api::cmd_harness_grok_shutdown(project.as_deref()))?;
                    println!("{}", serde_json::to_string_pretty(&view)?);
                }
                GrokCommands::Hold { project } => {
                    block_on(api::cmd_harness_grok_hold(project.as_deref()))?;
                }
            },
        },
        Commands::Notify { action } => match action {
            NotifyCommands::HermesTest { project } => {
                let project_id = match project {
                    Some(p) => api::load_registry()?.resolve_project(Some(&p))?.id.clone(),
                    None => "hermes-test".into(),
                };
                let event = crate::notify::hermes::synthetic_event(project_id);
                match crate::notify::hermes::probe(&event) {
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
    }
    Ok(())
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
        let about = run.get_about().map(|s| s.to_string()).unwrap_or_default();
        assert!(
            about.contains("Idle/Stopped"),
            "run about should say it ticks until Idle/Stopped: {about}"
        );
    }

    #[test]
    fn run_detach_conflicts_with_timeout_secs() {
        let err = Cli::try_parse_from(["coordinator", "run", "--detach", "--timeout-secs", "1"]);
        assert!(err.is_err(), "detach + timeout-secs must conflict");
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
    }

    #[test]
    fn notify_hermes_test_in_help() {
        let cmd = Cli::command();
        let notify = cmd.find_subcommand("notify").expect("notify");
        assert!(notify.find_subcommand("hermes-test").is_some());
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
}
