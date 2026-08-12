//! Clap CLI surface (tracks 0004–0005).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::api;
use crate::config::DEFAULT_SERVE_PORT;
use crate::error::CoordinatorError;
use crate::server;

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
    /// Start (or re-start) a stub run
    Run {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        track: Option<String>,
    },
    /// Pause a running stub
    Pause {
        #[arg(long)]
        project: Option<String>,
    },
    /// Resume a paused stub
    Resume {
        #[arg(long)]
        project: Option<String>,
    },
    /// Stop a run (no merge; sessions-for-attach deferred)
    Stop {
        #[arg(long)]
        project: Option<String>,
    },
    /// Phase Outcome File writers / inspectors
    Outcome {
        #[command(subcommand)]
        action: OutcomeCommands,
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
}

#[derive(Debug, Subcommand)]
pub enum ProjectCommands {
    /// Register a project path
    Add { path: PathBuf },
    /// List registered projects
    List,
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
        /// file | http | cli | timeout | test (default cli)
        #[arg(long, default_value = "cli")]
        source: String,
    },
    /// Print current.json if present
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
            ProjectCommands::Add { path } => {
                let rec = api::project_add(&path)?;
                println!("{}", serde_json::to_string_pretty(&rec)?);
            }
            ProjectCommands::List => {
                let list = api::project_list()?;
                println!("{}", serde_json::to_string_pretty(&list)?);
            }
        },
        Commands::Status { project } => {
            let view = api::status(project.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&view)?);
        }
        Commands::Run { project, track } => {
            let view = api::cmd_run(project.as_deref(), track)?;
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
        Commands::Wait {
            project,
            timeout_secs,
        } => {
            let view = api::cmd_wait(project.as_deref(), timeout_secs)?;
            println!("{}", serde_json::to_string_pretty(&view)?);
        }
        Commands::Serve { port } => {
            let rt = tokio::runtime::Runtime::new().map_err(|e| {
                CoordinatorError::Message(format!("failed to start async runtime: {e}"))
            })?;
            rt.block_on(server::serve(port))?;
        }
    }
    Ok(())
}
