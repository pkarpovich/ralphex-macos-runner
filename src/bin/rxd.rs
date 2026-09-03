//! The rxd client for the ralphex-macos-runner daemon.
//!
//! `rxd <plan>` opens a ticketless run on the farm and streams its output;
//! `rxd attach` reconnects to a run in progress; `rxd install` and
//! `rxd uninstall` register and remove the daemon's launchd agent. Ctrl-C only
//! detaches the terminal: the run keeps going in the daemon. The handler is
//! installed before the first answer is waited for, because the daemon can hold
//! that answer for the length of a farm poll and the run it is about to open
//! must not die with the terminal that asked for it.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use ralphex_macos_runner::ipc::{self, IpcError, Response, RunRequest};
use ralphex_macos_runner::job::Worktree;
use ralphex_macos_runner::paths;
use ralphex_macos_runner::protocol::client::CLAIM_TIMEOUT;
use ralphex_macos_runner::protocol::types::{Branch, CompleteStatus, CreatePr, REQUEST_TIMEOUT};
use ralphex_macos_runner::service;
use tokio::net::UnixStream;

const FORWARDED: &str = "CLAUDE_CONFIG_DIR";

const WAIT_NOTICE: Duration = Duration::from_millis(250);

const HELD: Duration = Duration::from_secs(CLAIM_TIMEOUT.as_secs() + REQUEST_TIMEOUT.as_secs());

type Interrupt = Pin<Box<dyn Future<Output = ()> + Send>>;

enum Notice {
    Poll,
    Quiet,
}

#[derive(Debug, Parser)]
#[command(
    name = "rxd",
    about = "Runs a plan through the ralphex-macos-runner daemon",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path of the daemon's Unix socket.
    #[arg(long, value_name = "path", global = true)]
    socket: Option<PathBuf>,

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

#[derive(Debug)]
enum RunArgsGiven {
    Yes,
    No,
}

#[tokio::main]
async fn main() -> ExitCode {
    let Cli {
        command,
        socket,
        run,
    } = Cli::parse();

    match command {
        Some(command) => match given(&run) {
            RunArgsGiven::Yes => {
                eprintln!("rxd: run arguments do not belong with a subcommand; see rxd --help");
                ExitCode::FAILURE
            }
            RunArgsGiven::No => dispatch(command, socket).await,
        },
        None => run_plan(socket, run).await,
    }
}

async fn dispatch(command: Command, socket: Option<PathBuf>) -> ExitCode {
    match command {
        Command::Attach => session(socket, ipc::Command::Attach, Notice::Quiet).await,
        Command::Install => install().await,
        Command::Uninstall => uninstall().await,
    }
}

fn given(run: &RunArgs) -> RunArgsGiven {
    let RunArgs {
        plan,
        branch,
        no_pr,
        worktree,
    } = run;
    match (plan, branch, no_pr, worktree) {
        (None, None, false, false) => RunArgsGiven::No,
        (_, _, _, _) => RunArgsGiven::Yes,
    }
}

async fn install() -> ExitCode {
    match service::install().await {
        Ok(installed) => {
            println!("{installed}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("rxd: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn uninstall() -> ExitCode {
    match service::uninstall().await {
        Ok(uninstalled) => {
            println!("{uninstalled}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("rxd: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run_plan(socket: Option<PathBuf>, run: RunArgs) -> ExitCode {
    let request = match describe(run) {
        Ok(request) => request,
        Err(message) => {
            eprintln!("rxd: {message}");
            return ExitCode::FAILURE;
        }
    };
    session(socket, ipc::Command::Run(request), Notice::Poll).await
}

async fn session(socket: Option<PathBuf>, command: ipc::Command, notice: Notice) -> ExitCode {
    let mut stream = match connect(socket).await {
        Ok(stream) => stream,
        Err(message) => {
            eprintln!("rxd: {message}");
            return ExitCode::FAILURE;
        }
    };
    let sent = ipc::send(&mut stream, &command).await;
    if let Err(error) = sent {
        eprintln!("rxd: {error}");
        return ExitCode::FAILURE;
    }

    let mut interrupted: Interrupt = Box::pin(async {
        let _signaled = tokio::signal::ctrl_c().await;
    });

    let first = {
        let receiving = ipc::receive::<Response, _>(&mut stream);
        tokio::pin!(receiving);
        let mut announced = false;
        loop {
            tokio::select! {
                received = &mut receiving => break received,
                () = &mut interrupted => {
                    println!(
                        "detached before the run id arrived; the daemon may still start it - use `rxd attach`"
                    );
                    return ExitCode::SUCCESS;
                }
                () = tokio::time::sleep(WAIT_NOTICE), if !announced => {
                    announce(&notice);
                    announced = true;
                }
            }
        }
    };
    let first = match first {
        Ok(response) => response,
        Err(error) => {
            eprintln!("rxd: {error}");
            return ExitCode::FAILURE;
        }
    };
    match show(first) {
        Some(code) => code,
        None => follow(&mut stream, &mut interrupted).await,
    }
}

fn announce(notice: &Notice) {
    match notice {
        Notice::Poll => println!(
            "waiting for the daemon to finish its farm poll (this can take up to {} s)",
            HELD.as_secs()
        ),
        Notice::Quiet => {}
    }
}

fn show(response: Response) -> Option<ExitCode> {
    match response {
        Response::Started {
            run_id,
            dashboard_url,
        } => {
            println!("run {run_id}");
            println!("{dashboard_url}");
            None
        }
        Response::Line { text } => {
            println!("{text}");
            None
        }
        Response::Ended {
            status,
            pr_url,
            fail_reason,
        } => Some(report(status, &pr_url, &fail_reason)),
        Response::Busy { run_id } => {
            eprintln!("rxd: the daemon is running {run_id}");
            Some(ExitCode::FAILURE)
        }
        Response::NoRun => {
            eprintln!("rxd: nothing is running");
            Some(ExitCode::FAILURE)
        }
        Response::Error { message } => {
            eprintln!("rxd: {message}");
            Some(ExitCode::FAILURE)
        }
    }
}

async fn follow(stream: &mut UnixStream, interrupted: &mut Interrupt) -> ExitCode {
    loop {
        let received = tokio::select! {
            received = ipc::receive::<Response, _>(stream) => received,
            () = &mut *interrupted => {
                println!("detached; the run continues");
                return ExitCode::SUCCESS;
            }
        };
        let response = match received {
            Ok(response) => response,
            Err(IpcError::Closed) => {
                eprintln!("rxd: the daemon closed the connection");
                return ExitCode::FAILURE;
            }
            Err(error) => {
                eprintln!("rxd: {error}");
                return ExitCode::FAILURE;
            }
        };
        if let Some(code) = show(response) {
            return code;
        }
    }
}

fn report(status: CompleteStatus, pr_url: &str, fail_reason: &str) -> ExitCode {
    if !pr_url.is_empty() {
        println!("{pr_url}");
    }
    match status {
        CompleteStatus::Done => {
            println!("done");
            ExitCode::SUCCESS
        }
        CompleteStatus::Error => {
            eprintln!("rxd: the run failed: {fail_reason}");
            ExitCode::FAILURE
        }
    }
}

async fn connect(socket: Option<PathBuf>) -> Result<UnixStream, String> {
    let path = match socket {
        Some(path) => path,
        None => match paths::socket_path() {
            Ok(path) => path,
            Err(error) => return Err(error.to_string()),
        },
    };
    match UnixStream::connect(&path).await {
        Ok(stream) => Ok(stream),
        Err(error) => Err(format!(
            "the daemon is not listening on {}: {error}",
            path.display()
        )),
    }
}

fn describe(run: RunArgs) -> Result<RunRequest, String> {
    let RunArgs {
        plan,
        branch,
        no_pr,
        worktree,
    } = run;
    let Some(plan) = plan else {
        return Err("a plan path is required; see rxd --help".to_string());
    };
    let ctx = match std::env::current_dir() {
        Ok(ctx) => ctx,
        Err(error) => return Err(format!("the current directory is unusable: {error}")),
    };
    let Ok(ctx) = ctx.canonicalize() else {
        return Err(format!("{} does not resolve", ctx.display()));
    };
    let plan = match plan.is_absolute() {
        true => plan,
        false => ctx.join(plan),
    };
    let plan = match plan.canonicalize() {
        Ok(plan) => plan,
        Err(error) => return Err(format!("{}: {error}", plan.display())),
    };
    let branch = match branch {
        Some(branch) => branch,
        None => plan_stem(&plan),
    };
    let create_pr = match no_pr {
        true => CreatePr::No,
        false => CreatePr::Yes,
    };
    let worktree = match worktree {
        true => Worktree::Yes,
        false => Worktree::No,
    };
    let mut env = Vec::new();
    if let Ok(forwarded) = std::env::var(FORWARDED) {
        env.push((FORWARDED.to_string(), forwarded));
    }
    Ok(RunRequest {
        ctx: ctx.display().to_string(),
        plan: plan.display().to_string(),
        branch: Branch(branch),
        create_pr,
        worktree,
        env,
    })
}

fn plan_stem(plan: &Path) -> String {
    let Some(stem) = plan.file_stem() else {
        return "ralphex".to_string();
    };
    stem.to_string_lossy().into_owned()
}
