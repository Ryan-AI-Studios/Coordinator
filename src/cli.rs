//! Clap CLI surface (frozen command names from track 0004).

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
        Commands::Serve { port } => {
            let rt = tokio::runtime::Runtime::new().map_err(|e| {
                CoordinatorError::Message(format!("failed to start async runtime: {e}"))
            })?;
            rt.block_on(server::serve(port))?;
        }
    }
    Ok(())
}
