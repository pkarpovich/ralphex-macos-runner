//! The rxd client for the ralphex-macos-runner daemon.
//!
//! `rxd <plan>` opens a ticketless run on the farm and streams its output;
//! `rxd attach` reconnects to a run in progress; `rxd install` and
//! `rxd uninstall` register and remove the daemon's launchd agent. Ctrl-C only
//! detaches the terminal: the run keeps going in the daemon. The handler is
//! installed before the first answer is waited for, because the daemon can hold
//! that answer for the length of a farm poll and the run it is about to open
//! must not die with the terminal that asked for it. A run's lines carry
//! ralphex's escape sequences as it wrote them: they are printed unchanged when
//! this client's stdout is a terminal and stripped when it is anything else.
//! The run inherits this client's `CLAUDE_CONFIG_DIR` and every `AGTERM_*`
//! variable, so the Claude Code hooks inside it report their status to the
//! agterm session `rxd` was started from, as they do for a run started by hand.

use std::ffi::OsString;
use std::future::Future;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use ralphex_macos_runner::ansi;
use ralphex_macos_runner::ipc::{self, IpcError, Response, RunRequest};
use ralphex_macos_runner::job::Worktree;
use ralphex_macos_runner::paths;
use ralphex_macos_runner::protocol::client::CLAIM_TIMEOUT;
use ralphex_macos_runner::protocol::types::{Branch, CompleteStatus, CreatePr, REQUEST_TIMEOUT};
use ralphex_macos_runner::service;
use tokio::net::UnixStream;

const FORWARDED: &str = "CLAUDE_CONFIG_DIR";

const FORWARDED_PREFIX: &str = "AGTERM_";

const WAIT_NOTICE: Duration = Duration::from_millis(250);

const HELD: Duration = Duration::from_secs(CLAIM_TIMEOUT.as_secs() + REQUEST_TIMEOUT.as_secs());

type Interrupt = Pin<Box<dyn Future<Output = ()> + Send>>;

enum Notice {
    Poll,
    Quiet,
}

#[derive(Debug, Clone, Copy)]
enum Palette {
    Keep,
    Strip,
}

impl Palette {
    fn of_stdout() -> Palette {
        match std::io::stdout().is_terminal() {
            true => Palette::Keep,
            false => Palette::Strip,
        }
    }
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
    Install {
        /// Replaces the daemon even when a run is in progress.
        #[arg(long)]
        force: bool,
    },
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
        Command::Install { force } => install(force).await,
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

async fn install(force: bool) -> ExitCode {
    let force = match force {
        true => service::Force::Yes,
        false => service::Force::No,
    };
    match service::install(force).await {
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
    let palette = Palette::of_stdout();
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
    match show(first, palette) {
        Some(code) => code,
        None => follow(&mut stream, &mut interrupted, palette).await,
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

fn show(response: Response, palette: Palette) -> Option<ExitCode> {
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
            match palette {
                Palette::Keep => println!("{text}"),
                Palette::Strip => println!("{}", ansi::plain(&text)),
            }
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

async fn follow(
    stream: &mut UnixStream,
    interrupted: &mut Interrupt,
    palette: Palette,
) -> ExitCode {
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
        if let Some(code) = show(response, palette) {
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
    let env = forwarded(std::env::vars_os());
    Ok(RunRequest {
        ctx: ctx.display().to_string(),
        plan: plan.display().to_string(),
        branch: Branch(branch),
        create_pr,
        worktree,
        env,
    })
}

fn forwarded(vars: impl IntoIterator<Item = (OsString, OsString)>) -> Vec<(String, String)> {
    let mut env = Vec::new();
    for (key, value) in vars {
        let (Some(key), Some(value)) = (key.to_str(), value.to_str()) else {
            continue;
        };
        if key == FORWARDED || key.starts_with(FORWARDED_PREFIX) {
            env.push((key.to_string(), value.to_string()));
        }
    }
    env
}

fn plan_stem(plan: &Path) -> String {
    let Some(stem) = plan.file_stem() else {
        return "ralphex".to_string();
    };
    stem.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    use super::forwarded;

    fn var(key: &str, value: &str) -> (OsString, OsString) {
        (OsString::from(key), OsString::from(value))
    }

    #[test]
    fn the_claude_profile_and_the_agterm_session_reach_the_run() {
        let vars = vec![
            var("CLAUDE_CONFIG_DIR", "/work/claude"),
            var("AGTERM_SESSION_ID", "session-1"),
            var("AGTERM_SOCKET", "/run/agterm.sock"),
            var("AGTERM_PANE_ID", "pane-1"),
            var("HOME", "/home/op"),
            var("PATH", "/usr/bin"),
            var("MY_AGTERM_SESSION_ID", "not-a-prefix-match"),
        ];

        let env = forwarded(vars);

        assert_eq!(
            env,
            vec![
                ("CLAUDE_CONFIG_DIR".to_string(), "/work/claude".to_string()),
                ("AGTERM_SESSION_ID".to_string(), "session-1".to_string()),
                ("AGTERM_SOCKET".to_string(), "/run/agterm.sock".to_string()),
                ("AGTERM_PANE_ID".to_string(), "pane-1".to_string()),
            ]
        );
    }

    #[test]
    fn a_shell_outside_agterm_forwards_nothing_of_it() {
        let env = forwarded(vec![var("HOME", "/home/op"), var("TERM", "xterm")]);

        assert!(env.is_empty());
    }

    #[test]
    fn a_variable_that_is_not_utf8_is_skipped() {
        let vars = vec![
            (
                OsString::from("AGTERM_SESSION_ID"),
                OsString::from_vec(vec![0xff, 0xfe]),
            ),
            var("AGTERM_SOCKET", "/run/agterm.sock"),
        ];

        let env = forwarded(vars);

        assert_eq!(
            env,
            vec![("AGTERM_SOCKET".to_string(), "/run/agterm.sock".to_string())]
        );
    }
}
