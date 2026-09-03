//! The ralphex-macos-runner daemon.
//!
//! Long-polls the farm for jobs, runs ralphex in the checkout a job names,
//! streams the output back and opens the pull request. A protocol version the
//! farm does not share ends the process with status 2, which launchd's
//! `KeepAlive` turns into a throttled restart.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use ralphex_macos_runner::agent::{Agent, AgentExit, AgentOptions};
use ralphex_macos_runner::config::Config;
use ralphex_macos_runner::paths;
use ralphex_macos_runner::protocol::client::{FarmClient, TokioSleeper};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

const VERSION_MISMATCH: u8 = 2;

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

#[tokio::main]
async fn main() -> ExitCode {
    let Cli { config } = Cli::parse();
    let filter = match EnvFilter::try_from_default_env() {
        Ok(filter) => filter,
        Err(_absent) => EnvFilter::new("info"),
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let config = match config {
        Some(path) => path,
        None => match paths::config_path() {
            Ok(path) => path,
            Err(error) => {
                tracing::error!("{error}");
                return ExitCode::FAILURE;
            }
        },
    };
    let config = match Config::load(&config) {
        Ok(config) => config,
        Err(error) => {
            tracing::error!("{error}");
            return ExitCode::FAILURE;
        }
    };

    let client = FarmClient::new(&config.farm_url, &config.token, Arc::new(TokioSleeper));
    let client = match client {
        Ok(client) => Arc::new(client),
        Err(error) => {
            tracing::error!("{error}");
            return ExitCode::FAILURE;
        }
    };

    let options = AgentOptions {
        drain_timeout: config.drain_timeout,
        ..AgentOptions::default()
    };
    tracing::info!(
        "runner {} serving {} for {}",
        config.name,
        config.ralphex_bin,
        config.farm_url
    );
    let agent = Agent::new(config, client, options);

    let (raise, shutdown) = watch::channel(false);
    tokio::spawn(raise_on_signal(raise));

    match agent.run(shutdown).await {
        AgentExit::Shutdown => ExitCode::SUCCESS,
        AgentExit::VersionMismatch { message } => {
            tracing::error!("the farm speaks another protocol version: {message}");
            ExitCode::from(VERSION_MISMATCH)
        }
    }
}

async fn raise_on_signal(raise: watch::Sender<bool>) {
    let terminate = signal(SignalKind::terminate());
    let interrupt = signal(SignalKind::interrupt());
    let (Ok(mut terminate), Ok(mut interrupt)) = (terminate, interrupt) else {
        tracing::error!("the shutdown signals could not be installed");
        return;
    };
    tokio::select! {
        _signaled = terminate.recv() => {}
        _signaled = interrupt.recv() => {}
    }
    tracing::info!("a shutdown was asked for; the claim loop stops");
    raise.send_replace(true);
}
