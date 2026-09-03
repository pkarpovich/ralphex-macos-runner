//! The client socket, the run slot it competes for and the `rxd` binary.

mod support;

use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use ralphex_macos_runner::agent::{Agent, AgentOptions, Shutdown};
use ralphex_macos_runner::config::Config;
use ralphex_macos_runner::ipc::{
    self, Command, DIRECTORY_MODE, IpcError, MAX_MESSAGE_BYTES, Response, RunRequest,
};
use ralphex_macos_runner::job::Worktree;
use ralphex_macos_runner::pr::PrTools;
use ralphex_macos_runner::protocol::client::FarmClient;
use ralphex_macos_runner::protocol::types::{
    Branch, CompleteRequest, CompleteStatus, CreatePr, Job, RunId, RunnerName,
};
use support::fake_farm::{FakeFarm, Reply};
use support::{
    Record, TestSleeper, fake_gh, fake_git, fake_ralphex_with, fixed_ticker, invocations,
};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::ChildStdout;
use tokio::sync::watch;

enum Claiming {
    Yes,
    No,
}

struct Checkout {
    dir: TempDir,
    plan: PathBuf,
    record: PathBuf,
    tools: PathBuf,
}

impl Checkout {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("plan.md");
        std::fs::write(&plan, "# plan\n").unwrap();
        let status = std::process::Command::new("git")
            .arg("init")
            .arg("--quiet")
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        let record = dir.path().join("ralphex-record");
        let tools = dir.path().join("tools-record");
        Checkout {
            dir,
            plan,
            record,
            tools,
        }
    }

    fn path(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn plan_path(&self) -> PathBuf {
        self.plan.canonicalize().unwrap()
    }

    fn ralphex(&self, settings: &[(&str, &str)]) -> PathBuf {
        let mut settings = settings.to_vec();
        let record = self.record.display().to_string();
        settings.push(("FAKE_RALPHEX_RECORD", &record));
        fake_ralphex_with(self.dir.path(), &settings)
    }
}

fn job(checkout: &Checkout, run_id: &str) -> Job {
    Job {
        run_id: RunId(run_id.to_string()),
        issue_id: String::new(),
        identifier: String::new(),
        issue_url: String::new(),
        title: "plan".to_string(),
        repo_slug: String::new(),
        plan_path: checkout.plan_path().display().to_string(),
        branch: Branch("plan".to_string()),
        mode: String::new(),
        lease_ttl_seconds: 180,
        runtime: "native".to_string(),
        ctx: checkout.path().display().to_string(),
        create_pr: CreatePr::No,
    }
}

fn config(farm: &FakeFarm, ralphex: &Path) -> Config {
    Config {
        farm_url: farm.url().to_string(),
        token: "secret-token".to_string(),
        name: RunnerName("mbp-native".to_string()),
        drain_timeout: Duration::from_millis(50),
        ralphex_bin: ralphex.display().to_string(),
    }
}

fn options(record: &Path) -> AgentOptions {
    AgentOptions {
        heartbeat_interval: Duration::from_millis(20),
        drain_timeout: Duration::from_millis(50),
        stop_grace: Duration::from_millis(200),
        claim_retry_delay: Duration::from_millis(10),
        ticker: fixed_ticker(Duration::from_millis(20)),
        pr_tools: PrTools {
            git: fake_git().display().to_string(),
            gh: fake_gh().display().to_string(),
            env: vec![("FAKE_RECORD".to_string(), record.display().to_string())],
            step_timeout: Duration::from_secs(30),
        },
    }
}

struct Daemon {
    agent: Arc<Agent>,
    socket: PathBuf,
    raise: watch::Sender<Shutdown>,
}

async fn daemon(
    farm: &FakeFarm,
    checkout: &Checkout,
    ralphex: &Path,
    claiming: Claiming,
) -> Daemon {
    let client = Arc::new(
        FarmClient::new(farm.url(), "secret-token", Arc::new(TestSleeper::new())).unwrap(),
    );
    let agent = Arc::new(Agent::new(
        config(farm, ralphex),
        client,
        options(&checkout.tools),
    ));
    let (raise, shutdown) = watch::channel(Shutdown::Running);
    let socket = checkout.dir.path().join("daemon.sock");
    tokio::spawn(ipc::serve(
        socket.clone(),
        Arc::clone(&agent),
        shutdown.clone(),
    ));
    match claiming {
        Claiming::Yes => {
            let polling = Arc::clone(&agent);
            tokio::spawn(async move { polling.run(shutdown).await });
        }
        Claiming::No => {}
    }
    let bound = wait_for(|| {
        let Ok(metadata) = std::fs::metadata(&socket) else {
            return None;
        };
        match metadata.file_type().is_socket() {
            true => Some(()),
            false => None,
        }
    })
    .await;
    assert!(bound.is_some(), "the socket was never bound");
    Daemon {
        agent,
        socket,
        raise,
    }
}

async fn wait_for<T>(mut ready: impl FnMut() -> Option<T>) -> Option<T> {
    for _attempt in 0..1000 {
        if let Some(value) = ready() {
            return Some(value);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
}

fn rxd(
    socket: &Path,
    checkout: &Checkout,
    args: &[&str],
    env: &[(&str, &str)],
) -> tokio::process::Child {
    let mut argv = args.to_vec();
    let socket = socket.display().to_string();
    argv.push("--socket");
    argv.push(&socket);
    rxd_argv(checkout, &argv, env)
}

fn rxd_argv(checkout: &Checkout, args: &[&str], env: &[(&str, &str)]) -> tokio::process::Child {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_rxd"));
    for argument in args {
        command.arg(argument);
    }
    command.current_dir(checkout.dir.path());
    command.env_remove("CLAUDE_CONFIG_DIR");
    for (key, value) in env {
        command.env(key, value);
    }
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    command.spawn().unwrap()
}

fn lines_of(client: &mut tokio::process::Child) -> tokio::io::Lines<BufReader<ChildStdout>> {
    let Some(stdout) = client.stdout.take() else {
        panic!("the client's output was already taken");
    };
    BufReader::new(stdout).lines()
}

fn text(output: &Output) -> String {
    let mut both = String::from_utf8_lossy(&output.stdout).into_owned();
    both.push_str(&String::from_utf8_lossy(&output.stderr));
    both
}

async fn spawned(record: &Path) -> Record {
    let started = wait_for(|| {
        let Ok(contents) = std::fs::read_to_string(record) else {
            return None;
        };
        if contents.contains("pid: ") {
            return Some(());
        }
        None
    })
    .await;
    assert!(started.is_some(), "the fake ralphex never started");
    Record::read(record)
}

async fn completion(farm: &FakeFarm) -> CompleteRequest {
    let recorded = wait_for(|| farm.requests_ending("/complete").first().cloned()).await;
    let Some(recorded) = recorded else {
        panic!("no completion arrived");
    };
    serde_json::from_slice(&recorded.body).unwrap()
}

fn alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

async fn dead(pid: i32) -> bool {
    let gone = wait_for(|| if alive(pid) { None } else { Some(()) }).await;
    gone.is_some()
}

#[tokio::test]
async fn a_command_and_its_answer_cross_a_socket() {
    let (mut client, mut server) = UnixStream::pair().unwrap();
    let request = RunRequest {
        ctx: "/abs/checkout".to_string(),
        plan: "/abs/checkout/plan.md".to_string(),
        branch: Branch("plan".to_string()),
        create_pr: CreatePr::No,
        worktree: Worktree::Yes,
        env: Vec::new(),
    };
    ipc::send(&mut client, &Command::Run(request.clone()))
        .await
        .unwrap();
    let received: Command = ipc::receive(&mut server).await.unwrap();
    assert_eq!(received, Command::Run(request));

    ipc::send(&mut server, &Response::NoRun).await.unwrap();
    let answered: Response = ipc::receive(&mut client).await.unwrap();
    assert_eq!(answered, Response::NoRun);
}

#[tokio::test]
async fn a_message_over_the_cap_never_reaches_the_socket() {
    let (mut client, _server) = UnixStream::pair().unwrap();
    let text = "x".repeat(MAX_MESSAGE_BYTES + 1);
    let refused = ipc::send(&mut client, &Response::Line { text })
        .await
        .unwrap_err();
    let IpcError::TooLarge(size) = refused else {
        panic!("an oversized message is refused for its size");
    };
    assert!(size > MAX_MESSAGE_BYTES);
}

#[tokio::test]
async fn the_socket_is_readable_by_its_owner_alone() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[]);
    let farm = FakeFarm::start().await;
    let daemon = daemon(&farm, &checkout, &ralphex, Claiming::No).await;

    let mode = std::fs::metadata(&daemon.socket)
        .unwrap()
        .permissions()
        .mode();

    assert_eq!(mode & 0o777, 0o600);
    drop(daemon.raise);
}

#[tokio::test]
async fn a_stale_socket_is_replaced_and_the_new_one_is_removed_on_shutdown() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[]);
    let stale = checkout.dir.path().join("daemon.sock");
    std::fs::write(&stale, "an old daemon left this behind").unwrap();
    let farm = FakeFarm::start().await;
    let daemon = daemon(&farm, &checkout, &ralphex, Claiming::No).await;
    let client = UnixStream::connect(&daemon.socket).await;
    assert!(client.is_ok(), "the stale file was not replaced");
    drop(client);

    daemon.raise.send_replace(Shutdown::Draining);
    let removed = wait_for(|| match daemon.socket.exists() {
        true => None,
        false => Some(()),
    })
    .await;

    assert!(removed.is_some(), "the socket outlived the daemon");
}

#[tokio::test]
async fn a_local_run_streams_its_output_and_ends_as_done() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "3")]);
    let farm = FakeFarm::start().await;
    farm.push_runs(Reply::Job(Box::new(job(&checkout, "local-1"))));
    let daemon = daemon(&farm, &checkout, &ralphex, Claiming::No).await;

    let client = rxd(&daemon.socket, &checkout, &["plan.md", "--no-pr"], &[]);
    let output = client.wait_with_output().await.unwrap();

    let printed = text(&output);
    assert!(output.status.success(), "{printed}");
    let lines: Vec<&str> = printed.lines().collect();
    assert_eq!(lines[0], "run local-1");
    assert_eq!(lines[1], format!("{}/#/run/local-1", farm.url()));
    assert!(printed.contains("out 1"), "{printed}");
    assert!(printed.contains("err 3"), "{printed}");
    assert!(printed.contains("done"), "{printed}");
    let opened = farm.requests_ending("/runs");
    assert_eq!(opened.len(), 1);
    let body = opened[0].text();
    assert!(body.contains(r#""runtime":"native""#), "{body}");
    assert!(body.contains(r#""create_pr":false"#), "{body}");
    assert!(body.contains(r#""branch":"plan""#), "{body}");
    drop(daemon.raise);
}

#[tokio::test]
async fn a_client_the_run_outran_is_told_how_many_lines_it_missed() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_BURST", "5000")]);
    let farm = FakeFarm::start().await;
    farm.push_runs(Reply::Job(Box::new(job(&checkout, "local-14"))));
    let daemon = daemon(&farm, &checkout, &ralphex, Claiming::No).await;

    let client = rxd(&daemon.socket, &checkout, &["plan.md", "--no-pr"], &[]);
    let output = client.wait_with_output().await.unwrap();

    let printed = text(&output);
    assert!(output.status.success(), "{printed}");
    assert!(printed.contains("lines skipped"), "{printed}");
    assert!(printed.contains("burst 5000"), "{printed}");
    assert!(printed.contains("done"), "{printed}");
    drop(daemon.raise);
}

#[tokio::test]
async fn a_failed_local_run_ends_the_client_with_a_failure() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "1"), ("FAKE_RALPHEX_EXIT", "3")]);
    let farm = FakeFarm::start().await;
    farm.push_runs(Reply::Job(Box::new(job(&checkout, "local-2"))));
    let daemon = daemon(&farm, &checkout, &ralphex, Claiming::No).await;

    let client = rxd(&daemon.socket, &checkout, &["plan.md", "--no-pr"], &[]);
    let output = client.wait_with_output().await.unwrap();

    let printed = text(&output);
    assert!(!output.status.success(), "{printed}");
    assert!(printed.contains("nonzero_exit"), "{printed}");
    drop(daemon.raise);
}

#[tokio::test]
async fn a_farm_that_refuses_the_run_is_reported_to_the_client() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[]);
    let farm = FakeFarm::start().await;
    farm.push_runs(Reply::Status(400, "no such repository".to_string()));
    let daemon = daemon(&farm, &checkout, &ralphex, Claiming::No).await;

    let client = rxd(&daemon.socket, &checkout, &["plan.md"], &[]);
    let output = client.wait_with_output().await.unwrap();

    let printed = text(&output);
    assert!(!output.status.success(), "{printed}");
    assert!(printed.contains("no such repository"), "{printed}");
    assert_eq!(farm.requests_ending("/runs").len(), 1);
    drop(daemon.raise);
}

#[tokio::test]
async fn a_worktree_run_passes_the_flag_to_ralphex() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "1")]);
    let farm = FakeFarm::start().await;
    farm.push_runs(Reply::Job(Box::new(job(&checkout, "local-3"))));
    let daemon = daemon(&farm, &checkout, &ralphex, Claiming::No).await;

    let client = rxd(
        &daemon.socket,
        &checkout,
        &["plan.md", "--no-pr", "--worktree"],
        &[],
    );
    let output = client.wait_with_output().await.unwrap();

    assert!(output.status.success(), "{}", text(&output));
    let record = Record::read(&checkout.record);
    assert_eq!(
        record.argv,
        vec![
            "--branch".to_string(),
            "plan".to_string(),
            "--worktree".to_string(),
            checkout.plan_path().display().to_string(),
        ]
    );
    assert_eq!(record.cwd, checkout.path().display().to_string());
    drop(daemon.raise);
}

#[tokio::test]
async fn the_claude_profile_of_the_client_reaches_ralphex() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "1")]);
    let farm = FakeFarm::start().await;
    farm.push_runs(Reply::Job(Box::new(job(&checkout, "local-4"))));
    let daemon = daemon(&farm, &checkout, &ralphex, Claiming::No).await;

    let client = rxd(
        &daemon.socket,
        &checkout,
        &["plan.md", "--no-pr"],
        &[("CLAUDE_CONFIG_DIR", "/work/claude")],
    );
    let output = client.wait_with_output().await.unwrap();

    assert!(output.status.success(), "{}", text(&output));
    let record = Record::read(&checkout.record);
    assert_eq!(
        record.env_value("CLAUDE_CONFIG_DIR"),
        Some("/work/claude".to_string())
    );
    drop(daemon.raise);
}

#[tokio::test]
async fn a_client_waits_through_a_poll_and_starts_when_it_comes_back_empty() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "1")]);
    let farm = FakeFarm::start().await;
    farm.always_claim(Reply::Hold);
    farm.push_runs(Reply::Job(Box::new(job(&checkout, "local-5"))));
    let daemon = daemon(&farm, &checkout, &ralphex, Claiming::Yes).await;
    let polling = wait_for(|| match farm.requests_ending("/claim").is_empty() {
        true => None,
        false => Some(()),
    })
    .await;
    assert!(polling.is_some(), "the claim loop never polled");

    let mut client = rxd(&daemon.socket, &checkout, &["plan.md", "--no-pr"], &[]);
    let mut printed = lines_of(&mut client);
    let notice = printed.next_line().await.unwrap().unwrap();
    assert!(notice.contains("waiting for the daemon"), "{notice}");
    assert!(
        farm.requests_ending("/runs").is_empty(),
        "the client did not wait for the poll"
    );

    farm.release_claim(Reply::NoJob);
    let started = printed.next_line().await.unwrap().unwrap();
    let status = client.wait().await.unwrap();

    assert_eq!(started, "run local-5");
    assert!(status.success(), "{status}");
    drop(daemon.raise);
}

#[tokio::test]
async fn a_client_that_waited_through_a_poll_is_busy_when_the_poll_brought_a_job() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_SLEEP", "120")]);
    let farm = FakeFarm::start().await;
    farm.always_claim(Reply::Hold);
    let daemon = daemon(&farm, &checkout, &ralphex, Claiming::Yes).await;
    let polling = wait_for(|| match farm.requests_ending("/claim").is_empty() {
        true => None,
        false => Some(()),
    })
    .await;
    assert!(polling.is_some(), "the claim loop never polled");

    let client = rxd(&daemon.socket, &checkout, &["plan.md", "--no-pr"], &[]);
    tokio::time::sleep(Duration::from_millis(200)).await;
    farm.release_claim(Reply::Job(Box::new(job(&checkout, "FARM-12-1"))));
    let output = client.wait_with_output().await.unwrap();

    let printed = text(&output);
    assert!(!output.status.success(), "{printed}");
    assert!(printed.contains("FARM-12-1"), "{printed}");
    assert!(farm.requests_ending("/runs").is_empty());
    let record = spawned(&checkout.record).await;
    assert!(record.pid > 0);
    daemon.raise.send_replace(Shutdown::Draining);
}

#[tokio::test]
async fn a_client_is_busy_while_a_run_holds_the_slot() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_SLEEP", "120")]);
    let farm = FakeFarm::start().await;
    farm.push_claim(Reply::Job(Box::new(job(&checkout, "FARM-13-1"))));
    farm.always_claim(Reply::Hold);
    let daemon = daemon(&farm, &checkout, &ralphex, Claiming::Yes).await;
    spawned(&checkout.record).await;

    let client = rxd(&daemon.socket, &checkout, &["plan.md", "--no-pr"], &[]);
    let output = client.wait_with_output().await.unwrap();

    let printed = text(&output);
    assert!(!output.status.success(), "{printed}");
    assert!(printed.contains("FARM-13-1"), "{printed}");
    assert!(farm.requests_ending("/runs").is_empty());
    daemon.raise.send_replace(Shutdown::Draining);
}

#[tokio::test]
async fn an_attach_without_a_run_says_so() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[]);
    let farm = FakeFarm::start().await;
    let daemon = daemon(&farm, &checkout, &ralphex, Claiming::No).await;

    let client = rxd(&daemon.socket, &checkout, &["attach"], &[]);
    let output = client.wait_with_output().await.unwrap();

    let printed = text(&output);
    assert!(!output.status.success(), "{printed}");
    assert!(printed.contains("nothing is running"), "{printed}");
    drop(daemon.raise);
}

#[tokio::test]
async fn a_local_run_that_asks_for_one_opens_a_pull_request() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "1")]);
    let farm = FakeFarm::start().await;
    let mut asked = job(&checkout, "local-7");
    asked.create_pr = CreatePr::Yes;
    farm.push_runs(Reply::Job(Box::new(asked)));
    let daemon = daemon(&farm, &checkout, &ralphex, Claiming::No).await;

    let client = rxd(&daemon.socket, &checkout, &["plan.md"], &[]);
    let output = client.wait_with_output().await.unwrap();

    let printed = text(&output);
    assert!(output.status.success(), "{printed}");
    assert!(
        printed.contains("https://github.com/owner/repo/pull/7"),
        "{printed}"
    );
    let opened = farm.requests_ending("/runs");
    assert!(opened[0].text().contains(r#""create_pr":true"#));
    let runs = invocations(&checkout.tools);
    assert!(runs[0].starts_with(&["pr", "list", "--head", "plan"]));
    assert!(runs[1].starts_with(&["push", "-u", "--", "origin", "plan"]));
    assert!(runs[3].starts_with(&["pr", "create", "--head", "plan", "--base", "main"]));
    assert!(runs[3].args.contains(&"plan".to_string()));
    let CompleteRequest {
        status,
        pr_url,
        fail_reason: _,
        message: _,
        log_tail: _,
    } = completion(&farm).await;
    assert_eq!(status, CompleteStatus::Done);
    assert_eq!(pr_url, "https://github.com/owner/repo/pull/7");
    drop(daemon.raise);
}

#[tokio::test]
async fn a_branch_the_client_names_reaches_the_farm() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "1")]);
    let farm = FakeFarm::start().await;
    farm.push_runs(Reply::Job(Box::new(job(&checkout, "local-8"))));
    let daemon = daemon(&farm, &checkout, &ralphex, Claiming::No).await;

    let client = rxd(
        &daemon.socket,
        &checkout,
        &["plan.md", "--no-pr", "--branch", "other"],
        &[],
    );
    let output = client.wait_with_output().await.unwrap();

    assert!(output.status.success(), "{}", text(&output));
    let opened = farm.requests_ending("/runs");
    assert!(opened[0].text().contains(r#""branch":"other""#));
    drop(daemon.raise);
}

#[tokio::test]
async fn a_local_run_the_farm_forgets_is_reported_to_the_client() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_SLEEP", "120")]);
    let farm = FakeFarm::start().await;
    farm.push_runs(Reply::Job(Box::new(job(&checkout, "local-10"))));
    let daemon = daemon(&farm, &checkout, &ralphex, Claiming::No).await;

    let client = rxd(&daemon.socket, &checkout, &["plan.md", "--no-pr"], &[]);
    let record = spawned(&checkout.record).await;
    farm.always_heartbeat(Reply::Status(410, String::new()));
    let output = client.wait_with_output().await.unwrap();

    let printed = text(&output);
    assert!(!output.status.success(), "{printed}");
    assert!(
        printed.contains("the farm no longer knows this run"),
        "{printed}"
    );
    assert!(dead(record.pid).await, "the run outlived its lease");
    assert!(
        farm.requests_ending("/complete").is_empty(),
        "a forgotten run was completed"
    );
    drop(daemon.raise);
}

#[tokio::test]
async fn a_completion_the_farm_refuses_is_reported_to_the_client() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "1")]);
    let farm = FakeFarm::start().await;
    farm.push_runs(Reply::Job(Box::new(job(&checkout, "local-12"))));
    farm.always_complete(Reply::Status(400, "the lease expired".to_string()));
    let daemon = daemon(&farm, &checkout, &ralphex, Claiming::No).await;

    let client = rxd(&daemon.socket, &checkout, &["plan.md", "--no-pr"], &[]);
    let output = client.wait_with_output().await.unwrap();

    let printed = text(&output);
    assert!(!output.status.success(), "{printed}");
    assert!(!printed.contains("\ndone"), "{printed}");
    assert!(printed.contains("the run ended done"), "{printed}");
    assert!(printed.contains("the farm did not record it"), "{printed}");
    assert!(printed.contains("the lease expired"), "{printed}");
    assert_eq!(farm.requests_ending("/complete").len(), 1);
    drop(daemon.raise);
}

#[tokio::test]
async fn a_completion_the_farm_finalized_first_is_reported_as_a_forgotten_run() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "1")]);
    let farm = FakeFarm::start().await;
    farm.push_runs(Reply::Job(Box::new(job(&checkout, "local-13"))));
    farm.always_complete(Reply::Status(410, String::new()));
    let daemon = daemon(&farm, &checkout, &ralphex, Claiming::No).await;

    let client = rxd(&daemon.socket, &checkout, &["plan.md", "--no-pr"], &[]);
    let output = client.wait_with_output().await.unwrap();

    let printed = text(&output);
    assert!(!output.status.success(), "{printed}");
    assert!(
        printed.contains("the farm no longer knows this run"),
        "{printed}"
    );
    assert!(!printed.contains("\ndone"), "{printed}");
    assert_eq!(farm.requests_ending("/complete").len(), 1);
    drop(daemon.raise);
}

#[tokio::test]
async fn a_socket_named_before_the_subcommand_still_reaches_the_daemon() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[]);
    let farm = FakeFarm::start().await;
    let daemon = daemon(&farm, &checkout, &ralphex, Claiming::No).await;

    let socket = daemon.socket.display().to_string();
    let client = rxd_argv(&checkout, &["--socket", &socket, "attach"], &[]);
    let output = client.wait_with_output().await.unwrap();

    let printed = text(&output);
    assert!(!output.status.success(), "{printed}");
    assert!(printed.contains("nothing is running"), "{printed}");
    drop(daemon.raise);
}

#[tokio::test]
async fn a_plan_named_beside_a_subcommand_is_refused() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[]);
    let farm = FakeFarm::start().await;
    let daemon = daemon(&farm, &checkout, &ralphex, Claiming::No).await;

    let client = rxd(&daemon.socket, &checkout, &["plan.md", "attach"], &[]);
    let output = client.wait_with_output().await.unwrap();

    let printed = text(&output);
    assert!(!output.status.success(), "{printed}");
    assert!(
        printed.contains("run arguments do not belong with a subcommand"),
        "{printed}"
    );
    assert!(!printed.contains("nothing is running"), "{printed}");
    drop(daemon.raise);
}

#[tokio::test]
async fn an_interrupted_client_detaches_and_leaves_the_run_going() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "1"), ("FAKE_RALPHEX_SLEEP", "120")]);
    let farm = FakeFarm::start().await;
    farm.push_runs(Reply::Job(Box::new(job(&checkout, "local-11"))));
    let daemon = daemon(&farm, &checkout, &ralphex, Claiming::No).await;

    let mut client = rxd(&daemon.socket, &checkout, &["plan.md", "--no-pr"], &[]);
    let mut printed = lines_of(&mut client);
    let started = printed.next_line().await.unwrap().unwrap();
    assert_eq!(started, "run local-11");
    let record = spawned(&checkout.record).await;
    let Some(client_pid) = client.id() else {
        panic!("the client reported no process id");
    };
    let client_pid = Pid::from_raw(i32::try_from(client_pid).unwrap());
    kill(client_pid, Signal::SIGINT).unwrap();

    let left = tokio::time::timeout(Duration::from_secs(10), client.wait()).await;
    let Ok(status) = left else {
        panic!("the client never left");
    };
    let status = status.unwrap();

    assert!(status.success(), "{status}");
    let mut rest = String::new();
    while let Ok(Some(line)) = printed.next_line().await {
        rest.push_str(&line);
        rest.push('\n');
    }
    assert!(rest.contains("detached; the run continues"), "{rest}");
    assert!(alive(record.pid), "the run died with its client");
    let mut attached = rxd(&daemon.socket, &checkout, &["attach"], &[]);
    let mut followed = lines_of(&mut attached);
    let reattached = followed.next_line().await.unwrap().unwrap();
    assert_eq!(reattached, "run local-11");
    attached.start_kill().unwrap();
    daemon.raise.send_replace(Shutdown::Draining);
}

#[tokio::test]
async fn a_client_interrupted_before_the_run_id_arrives_leaves_with_a_note() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[]);
    let farm = FakeFarm::start().await;
    farm.always_claim(Reply::Hold);
    let daemon = daemon(&farm, &checkout, &ralphex, Claiming::Yes).await;
    let polling = wait_for(|| match farm.requests_ending("/claim").is_empty() {
        true => None,
        false => Some(()),
    })
    .await;
    assert!(polling.is_some(), "the claim loop never polled");

    let mut client = rxd(&daemon.socket, &checkout, &["plan.md", "--no-pr"], &[]);
    let mut printed = lines_of(&mut client);
    let notice = printed.next_line().await.unwrap().unwrap();
    assert!(notice.contains("waiting for the daemon"), "{notice}");
    assert!(notice.contains("85 s"), "{notice}");
    let Some(client_pid) = client.id() else {
        panic!("the client reported no process id");
    };
    kill(
        Pid::from_raw(i32::try_from(client_pid).unwrap()),
        Signal::SIGINT,
    )
    .unwrap();

    let left = tokio::time::timeout(Duration::from_secs(10), client.wait()).await;
    let Ok(status) = left else {
        panic!("the client never left");
    };
    let status = status.unwrap();

    assert!(status.success(), "{status}");
    let mut rest = String::new();
    while let Ok(Some(line)) = printed.next_line().await {
        rest.push_str(&line);
        rest.push('\n');
    }
    assert!(
        rest.contains("detached before the run id arrived"),
        "{rest}"
    );
    assert!(rest.contains("rxd attach"), "{rest}");
    assert!(farm.requests_ending("/runs").is_empty());
    daemon.raise.send_replace(Shutdown::Draining);
}

#[tokio::test]
async fn a_client_without_a_plan_says_what_it_needs() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[]);
    let farm = FakeFarm::start().await;
    let daemon = daemon(&farm, &checkout, &ralphex, Claiming::No).await;

    let missing = rxd(&daemon.socket, &checkout, &[], &[]);
    let missing = missing.wait_with_output().await.unwrap();
    let absent = rxd(&daemon.socket, &checkout, &["absent.md"], &[]);
    let absent = absent.wait_with_output().await.unwrap();

    let printed = text(&missing);
    assert!(!missing.status.success(), "{printed}");
    assert!(printed.contains("a plan path is required"), "{printed}");
    let printed = text(&absent);
    assert!(!absent.status.success(), "{printed}");
    assert!(printed.contains("absent.md"), "{printed}");
    assert!(farm.requests_ending("/runs").is_empty());
    drop(daemon.raise);
}

#[tokio::test]
async fn a_socket_directory_the_daemon_creates_is_its_own() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[]);
    let farm = FakeFarm::start().await;
    let client = Arc::new(
        FarmClient::new(farm.url(), "secret-token", Arc::new(TestSleeper::new())).unwrap(),
    );
    let agent = Arc::new(Agent::new(
        config(&farm, &ralphex),
        client,
        options(&checkout.tools),
    ));
    let (raise, shutdown) = watch::channel(Shutdown::Running);
    let state = checkout.dir.path().join("state");
    let socket = state.join("daemon.sock");
    tokio::spawn(ipc::serve(socket.clone(), agent, shutdown));

    let bound = wait_for(|| match socket.exists() {
        true => Some(()),
        false => None,
    })
    .await;

    assert!(bound.is_some(), "the socket was never bound");
    let mode = std::fs::metadata(&state).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, DIRECTORY_MODE);
    let mut left = Vec::new();
    for entry in std::fs::read_dir(&state).unwrap() {
        left.push(entry.unwrap().file_name());
    }
    assert_eq!(
        left,
        vec![std::ffi::OsString::from("daemon.sock")],
        "the daemon left more than its socket behind"
    );
    drop(raise);
}

#[tokio::test]
async fn a_socket_that_cannot_be_bound_is_an_error_the_daemon_can_report() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[]);
    let farm = FakeFarm::start().await;
    let client = Arc::new(
        FarmClient::new(farm.url(), "secret-token", Arc::new(TestSleeper::new())).unwrap(),
    );
    let agent = Arc::new(Agent::new(
        config(&farm, &ralphex),
        client,
        options(&checkout.tools),
    ));
    let (raise, shutdown) = watch::channel(Shutdown::Running);
    let blocked = checkout.dir.path().join("not-a-directory");
    std::fs::write(&blocked, "a file sits where the directory should be").unwrap();

    let served = ipc::serve(blocked.join("daemon.sock"), agent, shutdown).await;

    let Err(IpcError::Io(message)) = served else {
        panic!("a socket that cannot be bound is an io failure");
    };
    assert!(!message.is_empty());
    drop(raise);
}

#[tokio::test]
async fn two_attached_clients_replay_the_history_and_follow_the_run() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "2"), ("FAKE_RALPHEX_SLEEP", "1")]);
    let farm = FakeFarm::start().await;
    farm.push_claim(Reply::Job(Box::new(job(&checkout, "FARM-14-1"))));
    farm.always_claim(Reply::Hold);
    let daemon = daemon(&farm, &checkout, &ralphex, Claiming::Yes).await;
    let history = wait_for(|| {
        let current = daemon.agent.current()?;
        let (replay, _live) = current.log().subscribe();
        match replay.len() >= 4 {
            true => Some(replay),
            false => None,
        }
    })
    .await;
    assert!(history.is_some(), "the run printed nothing to replay");

    let first = rxd(&daemon.socket, &checkout, &["attach"], &[]);
    let second = rxd(&daemon.socket, &checkout, &["attach"], &[]);
    let first = first.wait_with_output().await.unwrap();
    let second = second.wait_with_output().await.unwrap();

    for output in [&first, &second] {
        let printed = text(output);
        assert!(output.status.success(), "{printed}");
        assert!(printed.contains("run FARM-14-1"), "{printed}");
        assert!(printed.contains("out 1"), "{printed}");
        assert!(printed.contains("err 2"), "{printed}");
        assert!(printed.contains("done"), "{printed}");
    }
    drop(daemon.raise);
}
