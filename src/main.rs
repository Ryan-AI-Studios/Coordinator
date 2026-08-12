//! Coordinator binary — local Control Plane CLI entrypoint.

use std::process::ExitCode;

fn main() -> ExitCode {
    coordinator::cli::run()
}
