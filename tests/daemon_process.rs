//! The daemon as launchd runs it: a real process, its signals and its statuses.

mod support;

use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nix::sys::signal::Signal;
use nix::unistd::Pid;
use ralphex_macos_runner::protocol::types::{CompleteRequest, CompleteStatus};
use support::fake_farm::{FakeFarm, Reply};
use support::{Checkout, completion, dead, local_job, spawned, wait_for};
use tokio::io::{AsyncBufReadExt, BufReader};

const MISMATCH: &str = r#"{"error":"the runner speaks 1, the farm speaks 2"}"#;

fn config_file(checkout: &Checkout, farm: &FakeFarm, ralphex: &Path) -> PathBuf {
    let path = checkout.dir().join("config.toml");
    let contents = format!(
        "farm_url = \"{}\"\ntoken = \"secret-token\"\nname = \"mbp-native\"\ndrain_timeout = \"1s\"\nralphex_bin = \"{}\"\n",
        farm.url(),
        ralphex.display()
    );
    std::fs::write(&path, contents).unwrap();
    path
}

fn daemon(config: &Path, socket: &Path) -> tokio::process::Child {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_ralphex-macos-runner"));
    command.arg("--config").arg(config);
    command.arg("--socket").arg(socket);
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    command.spawn().unwrap()
}

fn rxd(socket: &Path, checkout: &Checkout, args: &[&str]) -> tokio::process::Child {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_rxd"));
    for argument in args {
        command.arg(argument);
    }
    command.arg("--socket").arg(socket);
    command.current_dir(checkout.dir());
    command.env_remove("CLAUDE_CONFIG_DIR");
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    command.spawn().unwrap()
}

async fn listening(socket: &Path) {
    let bound = wait_for(|| {
        let Ok(metadata) = std::fs::metadata(socket) else {
            return None;
        };
        match metadata.file_type().is_socket() {
            true => Some(()),
            false => None,
        }
    })
    .await;
    assert!(bound.is_some(), "the daemon never bound its socket");
}

fn logged(output: &std::process::Output) -> String {
    let mut both = String::from_utf8_lossy(&output.stdout).into_owned();
    both.push_str(&String::from_utf8_lossy(&output.stderr));
    both
}

fn signal(child: &tokio::process::Child, signal: Signal) {
    let Some(pid) = child.id() else {
        panic!("the daemon reported no process id");
    };
    nix::sys::signal::kill(Pid::from_raw(i32::try_from(pid).unwrap()), signal).unwrap();
}

#[tokio::test]
async fn a_protocol_mismatch_exits_with_the_status_launchd_restarts_on() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[]);
    let farm = FakeFarm::start().await;
    farm.always_claim(Reply::Status(409, MISMATCH.to_string()));
    let config = config_file(&checkout, &farm, &ralphex);
    let socket = checkout.dir().join("daemon.sock");

    let daemon = daemon(&config, &socket);
    let output = daemon.wait_with_output().await.unwrap();

    assert_eq!(output.status.code(), Some(2));
    let printed = logged(&output);
    assert!(printed.contains("the farm speaks another"), "{printed}");
    assert!(!socket.exists(), "the socket outlived the daemon");
}

#[tokio::test]
async fn a_configuration_that_is_not_there_exits_with_a_failure() {
    let checkout = Checkout::new();
    let absent = checkout.dir().join("absent.toml");
    let socket = checkout.dir().join("daemon.sock");

    let daemon = daemon(&absent, &socket);
    let output = daemon.wait_with_output().await.unwrap();

    assert_eq!(output.status.code(), Some(1));
    let printed = logged(&output);
    assert!(printed.contains("absent.toml"), "{printed}");
}

#[tokio::test]
async fn a_signalled_daemon_drains_the_run_a_client_started() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_SLEEP", "120")]);
    let farm = FakeFarm::start().await;
    farm.always_claim(Reply::Hold);
    farm.push_runs(Reply::Job(Box::new(local_job(&checkout, "local-1"))));
    let config = config_file(&checkout, &farm, &ralphex);
    let socket = checkout.dir().join("daemon.sock");
    let mut daemon = daemon(&config, &socket);
    listening(&socket).await;
    let polling = wait_for(|| match farm.requests_ending("/claim").is_empty() {
        true => None,
        false => Some(()),
    })
    .await;
    assert!(polling.is_some(), "the claim loop never polled");

    let mut client = rxd(&socket, &checkout, &["plan.md", "--no-pr"]);
    let Some(stdout) = client.stdout.take() else {
        panic!("the client's output was already taken");
    };
    let mut printed = BufReader::new(stdout).lines();
    let waiting = printed.next_line().await.unwrap().unwrap();
    assert!(waiting.contains("waiting for the daemon"), "{waiting}");
    farm.release_claim(Reply::NoJob);
    let record = spawned(checkout.record()).await;
    signal(&daemon, Signal::SIGTERM);

    let CompleteRequest {
        status,
        pr_url: _,
        fail_reason,
        message: _,
        log_tail: _,
    } = completion(&farm).await;
    let exited = tokio::time::timeout(Duration::from_secs(30), daemon.wait()).await;

    assert_eq!(status, CompleteStatus::Error);
    assert_eq!(fail_reason, "runner_shutdown");
    assert!(dead(record.pid).await, "the run outlived the daemon");
    let Ok(exited) = exited else {
        panic!("the daemon never exited");
    };
    assert_eq!(exited.unwrap().code(), Some(0));
    let mut rest = String::new();
    while let Ok(Some(line)) = printed.next_line().await {
        rest.push_str(&line);
        rest.push('\n');
    }
    assert!(rest.contains("run local-1"), "{rest}");
    let _left = client.wait().await.unwrap();
}
