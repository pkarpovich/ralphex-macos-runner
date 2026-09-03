//! The rxd client for the ralphex-macos-runner daemon.
//!
//! `rxd <plan>` opens a ticketless run on the farm and streams its output;
//! `rxd attach` reconnects to a run in progress; `rxd install` and
//! `rxd uninstall` register and remove the daemon's launchd agent.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "rxd",
    about = "Runs a plan through the ralphex-macos-runner daemon",
    version,
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    run: RunArgs,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Reconnects to the run in progress and streams its output.
    Attach,
    /// Installs the daemon as a launchd user agent.
    Install,
    /// Removes the daemon's launchd user agent.
    Uninstall,
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Path of the plan to run.
    #[arg(value_name = "plan")]
    plan: Option<PathBuf>,

    /// Branch ralphex works on; defaults to the plan file's stem.
    #[arg(long, value_name = "name")]
    branch: Option<String>,

    /// Finishes the run without opening a pull request.
    #[arg(long)]
    no_pr: bool,

    /// Runs ralphex in a worktree instead of the checkout itself.
    #[arg(long)]
    worktree: bool,
}

fn main() -> ExitCode {
    let Cli { command, run } = Cli::parse();

    match command {
        Some(Command::Attach) => {
            println!("rxd would attach to the run in progress");
            ExitCode::SUCCESS
        }
        Some(Command::Install) => {
            println!("rxd would install the launchd agent");
            ExitCode::SUCCESS
        }
        Some(Command::Uninstall) => {
            println!("rxd would remove the launchd agent");
            ExitCode::SUCCESS
        }
        None => start_run(run),
    }
}

fn start_run(run: RunArgs) -> ExitCode {
    let RunArgs {
        plan,
        branch,
        no_pr,
        worktree,
    } = run;

    let Some(plan) = plan else {
        eprintln!("rxd: a plan path is required; see rxd --help");
        return ExitCode::FAILURE;
    };

    let branch = branch.unwrap_or_else(|| String::from("<plan stem>"));
    let create_pr = match no_pr {
        true => "no",
        false => "yes",
    };
    let worktree = match worktree {
        true => "yes",
        false => "no",
    };

    println!(
        "rxd would run {} on branch {branch} (pull request: {create_pr}, worktree: {worktree})",
        plan.display()
    );
    ExitCode::SUCCESS
}
