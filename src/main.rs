//! Coordinator binary — local Control Plane CLI entrypoint.

use std::process::ExitCode;

fn main() -> ExitCode {
    coordinator::config::load_dotenv();
    coordinator::cli::run()
}
