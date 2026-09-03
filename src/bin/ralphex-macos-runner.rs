//! The ralphex-macos-runner daemon.
//!
//! Long-polls the farm for jobs, runs ralphex in the checkout a job names,
//! streams the output back and opens the pull request.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use ralphex_macos_runner::paths;

#[derive(Debug, Parser)]
#[command(
    name = "ralphex-macos-runner",
    about = "Runs ralphex natively for ralphex-farm",
    version
)]
struct Cli {
    /// Path of the configuration file to load.
    #[arg(long, value_name = "path")]
    config: Option<PathBuf>,
}

fn main() -> ExitCode {
    let Cli { config } = Cli::parse();

    let config = match config {
        Some(path) => path,
        None => match paths::config_path() {
            Ok(path) => path,
            Err(error) => {
                eprintln!("ralphex-macos-runner: {error}");
                return ExitCode::FAILURE;
            }
        },
    };

    println!("ralphex-macos-runner would load {}", config.display());
    ExitCode::SUCCESS
}
