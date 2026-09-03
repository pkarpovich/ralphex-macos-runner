//! The daemon's claim loop against a scripted fake farm and a fake ralphex.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nix::sys::signal::kill;
use nix::unistd::Pid;
use ralphex_macos_runner::agent::{Agent, AgentExit, AgentOptions, LocalStart, RunSlot};
use ralphex_macos_runner::config::Config;
use ralphex_macos_runner::ipc::RunRequest;
use ralphex_macos_runner::job::Worktree;
use ralphex_macos_runner::pr::PrTools;
use ralphex_macos_runner::protocol::client::FarmClient;
use ralphex_macos_runner::protocol::types::{
    Branch, CompleteRequest, CompleteStatus, CreatePr, HeartbeatAction, Job, RunId, RunnerName,
};
use support::fake_farm::{FakeFarm, Recorded, Reply};
use support::{
    Record, TestSleeper, fake_gh, fake_git, fake_ralphex_with, fixed_ticker, invocations,
};
use tempfile::TempDir;
use tokio::sync::watch;
use tokio::task::JoinHandle;

const MISMATCH: &str = r#"{"error":"the runner speaks 1, the farm speaks 2"}"#;

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

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn ralphex(&self, settings: &[(&str, &str)]) -> PathBuf {
        let mut settings = settings.to_vec();
        let record = self.record.display().to_string();
        settings.push(("FAKE_RALPHEX_RECORD", &record));
        fake_ralphex_with(self.dir.path(), &settings)
    }
}

fn job(ctx: &Path, plan: &Path, create_pr: CreatePr) -> Job {
    Job {
        run_id: RunId("FARM-12-1753180800000".to_string()),
        issue_id: "issue-uuid".to_string(),
        identifier: "FARM-12".to_string(),
        issue_url: "https://linear.app/example/issue/FARM-12".to_string(),
        title: "split farm and runner".to_string(),
        repo_slug: "owner/repo".to_string(),
        plan_path: plan.display().to_string(),
        branch: Branch("x".to_string()),
        mode: String::new(),
        lease_ttl_seconds: 180,
        runtime: "native".to_string(),
        ctx: ctx.display().to_string(),
        create_pr,
    }
}

async fn farm_with(job: Job) -> FakeFarm {
    let farm = FakeFarm::start().await;
    farm.push_claim(Reply::Job(Box::new(job)));
    farm.always_claim(Reply::Hold);
    farm
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

fn agent(farm: &FakeFarm, config: Config, options: AgentOptions) -> Agent {
    let client = Arc::new(
        FarmClient::new(farm.url(), "secret-token", Arc::new(TestSleeper::new())).unwrap(),
    );
    Agent::new(config, client, options)
}

struct Running {
    agent: Arc<Agent>,
    raise: watch::Sender<bool>,
    handle: JoinHandle<AgentExit>,
}

fn start(agent: Agent) -> Running {
    let agent = Arc::new(agent);
    let (raise, shutdown) = watch::channel(false);
    let claiming = Arc::clone(&agent);
    let handle = tokio::spawn(async move { claiming.run(shutdown).await });
    Running {
        agent,
        raise,
        handle,
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

async fn completion(farm: &FakeFarm) -> CompleteRequest {
    let recorded = wait_for(|| farm.requests_ending("/complete").first().cloned()).await;
    let Some(recorded) = recorded else {
        panic!("no completion arrived");
    };
    let Recorded {
        path: _,
        query: _,
        authorization: _,
        content_type: _,
        body,
    } = recorded;
    serde_json::from_slice(&body).unwrap()
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

fn alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

async fn dead(pid: i32) -> bool {
    let gone = wait_for(|| if alive(pid) { None } else { Some(()) }).await;
    gone.is_some()
}

fn delivered(farm: &FakeFarm) -> String {
    let mut bytes = Vec::new();
    for request in farm.requests_ending("/log") {
        bytes.extend(request.body);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn first_index(farm: &FakeFarm, suffix: &str) -> Option<usize> {
    for (index, request) in farm.requests().iter().enumerate() {
        if request.path.ends_with(suffix) {
            return Some(index);
        }
    }
    None
}

fn last_index(farm: &FakeFarm, suffix: &str) -> Option<usize> {
    let mut last = None;
    for (index, request) in farm.requests().iter().enumerate() {
        if request.path.ends_with(suffix) {
            last = Some(index);
        }
    }
    last
}

#[tokio::test]
async fn a_claimed_job_runs_to_done_and_its_output_reaches_the_farm() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "3")]);
    let farm = farm_with(job(checkout.path(), &checkout.plan, CreatePr::No)).await;
    let running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(&checkout.tools),
    ));

    let CompleteRequest {
        status,
        pr_url,
        fail_reason,
        message: _,
        log_tail,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Done);
    assert!(pr_url.is_empty());
    assert!(fail_reason.is_empty());
    assert!(log_tail.is_empty(), "a finished run carries no tail");
    let output = delivered(&farm);
    assert!(output.contains("out 1"), "{output}");
    assert!(output.contains("err 3"), "{output}");
    let record = Record::read(&checkout.record);
    assert_eq!(
        record.argv,
        vec![
            "--branch".to_string(),
            "x".to_string(),
            checkout.plan.display().to_string(),
        ]
    );
    let freed = wait_for(|| match running.agent.slot() {
        RunSlot::Running(_) => None,
        RunSlot::Free => Some(()),
        RunSlot::Polling => Some(()),
        RunSlot::Opening => None,
    })
    .await;
    assert!(freed.is_some(), "the run slot was never released");
    drop(running.raise);
}

#[tokio::test]
async fn the_first_heartbeat_arrives_before_the_first_log_chunk() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "2")]);
    let farm = farm_with(job(checkout.path(), &checkout.plan, CreatePr::No)).await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(&checkout.tools),
    ));

    completion(&farm).await;

    let beat = first_index(&farm, "/heartbeat");
    let chunk = first_index(&farm, "/log");
    assert!(beat.is_some(), "no heartbeat was sent");
    assert!(chunk.is_some(), "no log chunk was sent");
    assert!(beat < chunk, "the first flush beat the first heartbeat");
}

#[tokio::test]
async fn a_nonzero_exit_completes_as_a_failure_with_its_tail() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "2"), ("FAKE_RALPHEX_EXIT", "3")]);
    let farm = farm_with(job(checkout.path(), &checkout.plan, CreatePr::No)).await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(&checkout.tools),
    ));

    let CompleteRequest {
        status,
        pr_url: _,
        fail_reason,
        message,
        log_tail,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Error);
    assert_eq!(fail_reason, "nonzero_exit");
    assert_eq!(message, "ralphex exited with code 3");
    assert!(log_tail.contains("out 2"), "{log_tail}");
}

#[tokio::test]
async fn a_cancel_on_the_heartbeat_stops_the_run_and_completes_it_as_canceled() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_SLEEP", "120")]);
    let farm = farm_with(job(checkout.path(), &checkout.plan, CreatePr::No)).await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(&checkout.tools),
    ));

    let record = spawned(&checkout.record).await;
    farm.push_heartbeat(Reply::Beat(HeartbeatAction::Cancel));
    let CompleteRequest {
        status,
        pr_url: _,
        fail_reason,
        message: _,
        log_tail: _,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Error);
    assert_eq!(fail_reason, "canceled");
    assert!(dead(record.pid).await, "the run outlived its cancel");
}

#[tokio::test]
async fn the_lease_is_still_beaten_while_a_canceled_run_is_stopped() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_IGNORE_TERM", "1")]);
    let farm = farm_with(job(checkout.path(), &checkout.plan, CreatePr::No)).await;
    let mut options = options(&checkout.tools);
    options.stop_grace = Duration::from_secs(1);
    let _running = start(agent(&farm, config(&farm, &ralphex), options));

    let record = spawned(&checkout.record).await;
    let before = farm.requests_ending("/heartbeat").len();
    farm.always_heartbeat(Reply::Beat(HeartbeatAction::Cancel));
    let CompleteRequest {
        status,
        pr_url: _,
        fail_reason,
        message: _,
        log_tail: _,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Error);
    assert_eq!(fail_reason, "canceled");
    assert!(dead(record.pid).await, "the run outlived its cancel");
    let beats = farm.requests_ending("/heartbeat").len() - before;
    assert!(
        beats >= 20,
        "the heartbeat stopped at the cancel: {beats} beats reached the farm while the canceled run was being stopped"
    );
}

#[tokio::test]
async fn a_container_job_is_refused_without_spawning_anything() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[]);
    let mut asked = job(checkout.path(), &checkout.plan, CreatePr::No);
    asked.runtime = "container".to_string();
    let farm = farm_with(asked).await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(&checkout.tools),
    ));

    let CompleteRequest {
        status,
        pr_url: _,
        fail_reason,
        message,
        log_tail: _,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Error);
    assert_eq!(fail_reason, "runtime_mismatch");
    assert!(message.contains("container"), "{message}");
    assert!(!checkout.record.exists(), "ralphex was spawned anyway");
}

#[tokio::test]
async fn a_version_mismatch_on_the_claim_ends_the_agent() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[]);
    let farm = FakeFarm::start().await;
    farm.push_claim(Reply::Status(409, MISMATCH.to_string()));
    farm.always_claim(Reply::Hold);
    let running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(&checkout.tools),
    ));

    let exit = running.handle.await.unwrap();

    assert_eq!(
        exit,
        AgentExit::VersionMismatch {
            message: "the runner speaks 1, the farm speaks 2".to_string()
        }
    );
    assert!(!checkout.record.exists(), "ralphex was spawned anyway");
}

#[tokio::test]
async fn a_version_mismatch_on_the_heartbeat_stops_the_run_and_ends_the_agent() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_SLEEP", "120")]);
    let farm = farm_with(job(checkout.path(), &checkout.plan, CreatePr::No)).await;
    let running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(&checkout.tools),
    ));

    let record = spawned(&checkout.record).await;
    farm.push_heartbeat(Reply::Status(409, MISMATCH.to_string()));
    let exit = running.handle.await.unwrap();

    assert_eq!(
        exit,
        AgentExit::VersionMismatch {
            message: "the runner speaks 1, the farm speaks 2".to_string()
        }
    );
    assert!(dead(record.pid).await, "the run outlived the mismatch");
    assert!(
        farm.requests_ending("/complete").is_empty(),
        "a mismatched run was completed"
    );
}

#[tokio::test]
async fn a_forgotten_run_is_killed_and_never_completed() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_SLEEP", "120")]);
    let farm = farm_with(job(checkout.path(), &checkout.plan, CreatePr::No)).await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(&checkout.tools),
    ));

    let record = spawned(&checkout.record).await;
    farm.always_heartbeat(Reply::Status(410, String::new()));

    assert!(dead(record.pid).await, "the run outlived its lease");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        farm.requests_ending("/complete").is_empty(),
        "a forgotten run was completed"
    );
}

#[tokio::test]
async fn a_forgotten_log_stream_leaves_the_run_alone() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "2")]);
    let farm = farm_with(job(checkout.path(), &checkout.plan, CreatePr::No)).await;
    farm.always_log(Reply::Status(410, String::new()));
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(&checkout.tools),
    ));

    let CompleteRequest {
        status,
        pr_url: _,
        fail_reason,
        message: _,
        log_tail: _,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Done);
    assert!(fail_reason.is_empty());
    let chunks = farm.requests_ending("/log");
    assert_eq!(chunks.len(), 1, "the stream kept posting after its 410");
}

#[tokio::test]
async fn a_second_job_is_not_claimed_while_one_runs() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_SLEEP", "120")]);
    let farm = FakeFarm::start().await;
    farm.push_claim(Reply::Job(Box::new(job(
        checkout.path(),
        &checkout.plan,
        CreatePr::No,
    ))));
    farm.push_claim(Reply::Job(Box::new(job(
        checkout.path(),
        &checkout.plan,
        CreatePr::No,
    ))));
    let running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(&checkout.tools),
    ));

    let record = spawned(&checkout.record).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(farm.requests_ending("/claim").len(), 1);
    assert_eq!(
        running.agent.slot(),
        RunSlot::Running(RunId("FARM-12-1753180800000".to_string()))
    );

    running.raise.send_replace(true);
    let CompleteRequest {
        status: _,
        pr_url: _,
        fail_reason,
        message: _,
        log_tail: _,
    } = completion(&farm).await;

    assert_eq!(fail_reason, "runner_shutdown");
    assert!(dead(record.pid).await, "the run outlived the shutdown");
    assert_eq!(running.handle.await.unwrap(), AgentExit::Shutdown);
    assert_eq!(farm.requests_ending("/claim").len(), 1);
}

#[tokio::test]
async fn a_run_that_outlasts_its_drain_completes_as_a_shutdown() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_SLEEP", "120")]);
    let farm = farm_with(job(checkout.path(), &checkout.plan, CreatePr::No)).await;
    let running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(&checkout.tools),
    ));

    let record = spawned(&checkout.record).await;
    running.raise.send_replace(true);

    let CompleteRequest {
        status,
        pr_url: _,
        fail_reason,
        message,
        log_tail: _,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Error);
    assert_eq!(fail_reason, "runner_shutdown");
    assert!(message.contains("shut down"), "{message}");
    assert!(dead(record.pid).await, "the run outlived the shutdown");
    assert_eq!(running.handle.await.unwrap(), AgentExit::Shutdown);
}

#[tokio::test]
async fn a_shutdown_before_the_first_claim_stops_the_agent_at_once() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[]);
    let farm = FakeFarm::start().await;
    farm.always_claim(Reply::Hold);
    let agent = agent(&farm, config(&farm, &ralphex), options(&checkout.tools));
    let (raise, shutdown) = watch::channel(true);

    let exit = agent.run(shutdown).await;

    assert_eq!(exit, AgentExit::Shutdown);
    assert!(farm.requests_ending("/claim").is_empty());
    drop(raise);
}

#[tokio::test]
async fn an_empty_claim_paces_the_loop_and_the_pause_ends_on_a_shutdown() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[]);
    let farm = FakeFarm::start().await;
    farm.always_claim(Reply::NoJob);
    let mut options = options(&checkout.tools);
    options.claim_retry_delay = Duration::from_secs(120);
    let running = start(agent(&farm, config(&farm, &ralphex), options));

    let polled = wait_for(|| match farm.requests_ending("/claim").is_empty() {
        true => None,
        false => Some(()),
    })
    .await;
    assert!(polled.is_some(), "the loop never polled");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        farm.requests_ending("/claim").len(),
        1,
        "an empty claim was not paced"
    );

    running.raise.send_replace(true);
    let exit = tokio::time::timeout(Duration::from_secs(5), running.handle).await;

    let Ok(exit) = exit else {
        panic!("the shutdown waited the pause out");
    };
    assert_eq!(exit.unwrap(), AgentExit::Shutdown);
}

#[tokio::test]
async fn a_job_claimed_during_a_shutdown_is_completed_without_being_started() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_SLEEP", "120")]);
    let farm = FakeFarm::start().await;
    farm.always_claim(Reply::Hold);
    let running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(&checkout.tools),
    ));

    let polling = wait_for(|| match farm.requests_ending("/claim").is_empty() {
        true => None,
        false => Some(()),
    })
    .await;
    assert!(polling.is_some(), "the claim never reached the farm");
    running.raise.send_replace(true);
    farm.release_claim(Reply::Job(Box::new(job(
        checkout.path(),
        &checkout.plan,
        CreatePr::No,
    ))));

    let CompleteRequest {
        status,
        pr_url: _,
        fail_reason,
        message,
        log_tail: _,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Error);
    assert_eq!(fail_reason, "runner_shutdown");
    assert!(message.contains("before the run started"), "{message}");
    assert_eq!(running.handle.await.unwrap(), AgentExit::Shutdown);
    assert!(
        !checkout.record.exists(),
        "ralphex was started for a job claimed during a shutdown"
    );
}

#[tokio::test]
async fn a_local_run_asked_for_during_a_shutdown_is_refused_without_opening_one() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[]);
    let farm = FakeFarm::start().await;
    farm.always_claim(Reply::Hold);
    let agent = Arc::new(agent(
        &farm,
        config(&farm, &ralphex),
        options(&checkout.tools),
    ));
    let (raise, shutdown) = watch::channel(true);

    let started = agent
        .start_local(
            RunRequest {
                ctx: checkout.path().display().to_string(),
                plan: checkout.plan.display().to_string(),
                branch: Branch("x".to_string()),
                create_pr: CreatePr::No,
                worktree: Worktree::No,
                env: Vec::new(),
            },
            shutdown,
        )
        .await;

    let LocalStart::Refused { message } = started else {
        panic!("the run was not refused");
    };
    assert!(message.contains("shutting down"), "{message}");
    assert!(farm.requests_ending("/runs").is_empty());
    assert!(!checkout.record.exists(), "ralphex was started anyway");
    drop(raise);
}

#[tokio::test]
async fn a_local_run_queued_behind_a_mismatched_claim_is_refused_without_opening_one() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[]);
    let farm = FakeFarm::start().await;
    farm.always_claim(Reply::Hold);
    let agent = Arc::new(agent(
        &farm,
        config(&farm, &ralphex),
        options(&checkout.tools),
    ));
    let (raise, shutdown) = watch::channel(false);
    let claiming = Arc::clone(&agent);
    let claims = tokio::spawn({
        let shutdown = shutdown.clone();
        async move { claiming.run(shutdown).await }
    });
    let polling = wait_for(|| match agent.slot() {
        RunSlot::Polling => Some(()),
        RunSlot::Free => None,
        RunSlot::Opening => None,
        RunSlot::Running(_) => None,
    })
    .await;
    assert!(polling.is_some(), "the claim loop never polled");

    let asking = Arc::clone(&agent);
    let request = RunRequest {
        ctx: checkout.path().display().to_string(),
        plan: checkout.plan.display().to_string(),
        branch: Branch("x".to_string()),
        create_pr: CreatePr::No,
        worktree: Worktree::No,
        env: Vec::new(),
    };
    let started = tokio::spawn(async move { asking.start_local(request, shutdown).await });
    farm.release_claim(Reply::Status(409, MISMATCH.to_string()));

    let exit = claims.await.unwrap();
    let LocalStart::Refused { message } = started.await.unwrap() else {
        panic!("the queued request was not refused");
    };

    assert_eq!(
        exit,
        AgentExit::VersionMismatch {
            message: "the runner speaks 1, the farm speaks 2".to_string()
        }
    );
    assert!(message.contains("the daemon is exiting"), "{message}");
    assert!(farm.requests_ending("/runs").is_empty());
    assert!(!checkout.record.exists(), "ralphex was started anyway");
    drop(raise);
}

#[tokio::test]
async fn a_local_run_asked_for_after_a_mismatched_heartbeat_is_refused() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_SLEEP", "120")]);
    let farm = FakeFarm::start().await;
    farm.push_claim(Reply::Job(Box::new(job(
        checkout.path(),
        &checkout.plan,
        CreatePr::No,
    ))));
    farm.always_claim(Reply::Hold);
    let agent = Arc::new(agent(
        &farm,
        config(&farm, &ralphex),
        options(&checkout.tools),
    ));
    let (raise, shutdown) = watch::channel(false);
    let claiming = Arc::clone(&agent);
    let claims = tokio::spawn({
        let shutdown = shutdown.clone();
        async move { claiming.run(shutdown).await }
    });
    spawned(&checkout.record).await;
    farm.always_heartbeat(Reply::Status(409, MISMATCH.to_string()));
    let exit = claims.await.unwrap();

    let started = agent
        .start_local(
            RunRequest {
                ctx: checkout.path().display().to_string(),
                plan: checkout.plan.display().to_string(),
                branch: Branch("x".to_string()),
                create_pr: CreatePr::No,
                worktree: Worktree::No,
                env: Vec::new(),
            },
            shutdown,
        )
        .await;

    assert_eq!(
        exit,
        AgentExit::VersionMismatch {
            message: "the runner speaks 1, the farm speaks 2".to_string()
        }
    );
    let LocalStart::Refused { message } = started else {
        panic!("the request was not refused");
    };
    assert!(message.contains("the daemon is exiting"), "{message}");
    assert!(farm.requests_ending("/runs").is_empty());
    drop(raise);
}

#[tokio::test]
async fn a_shutdown_that_lands_while_the_farm_mints_a_local_run_completes_it_unstarted() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_SLEEP", "120")]);
    let farm = FakeFarm::start().await;
    farm.push_runs(Reply::Hold);
    let agent = Arc::new(agent(
        &farm,
        config(&farm, &ralphex),
        options(&checkout.tools),
    ));
    let (raise, shutdown) = watch::channel(false);
    let opening = Arc::clone(&agent);
    let request = RunRequest {
        ctx: checkout.path().display().to_string(),
        plan: checkout.plan.display().to_string(),
        branch: Branch("x".to_string()),
        create_pr: CreatePr::No,
        worktree: Worktree::No,
        env: Vec::new(),
    };
    let started = tokio::spawn(async move { opening.start_local(request, shutdown).await });

    let opened = wait_for(|| match farm.requests_ending("/runs").is_empty() {
        true => None,
        false => Some(()),
    })
    .await;
    assert!(opened.is_some(), "the opening never reached the farm");
    raise.send_replace(true);
    farm.release_runs(Reply::Job(Box::new(job(
        checkout.path(),
        &checkout.plan,
        CreatePr::No,
    ))));

    let LocalStart::Refused { message } = started.await.unwrap() else {
        panic!("the run was not refused");
    };
    assert!(message.contains("shutting down"), "{message}");
    let CompleteRequest {
        status,
        pr_url: _,
        fail_reason,
        message: _,
        log_tail: _,
    } = completion(&farm).await;
    assert_eq!(status, CompleteStatus::Error);
    assert_eq!(fail_reason, "runner_shutdown");
    assert!(
        !checkout.record.exists(),
        "ralphex was started for a run the farm minted during a shutdown"
    );
    drop(raise);
}

#[tokio::test]
async fn a_finished_run_opens_a_pull_request() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "1")]);
    let farm = farm_with(job(checkout.path(), &checkout.plan, CreatePr::Yes)).await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(&checkout.tools),
    ));

    let CompleteRequest {
        status,
        pr_url,
        fail_reason,
        message: _,
        log_tail: _,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Done);
    assert!(fail_reason.is_empty());
    assert_eq!(pr_url, "https://github.com/owner/repo/pull/7");
    let runs = invocations(&checkout.tools);
    assert!(runs[0].starts_with(&["pr", "list", "--head", "x"]));
    assert!(runs[1].starts_with(&["push", "-u", "--", "origin", "x"]));
    assert!(runs[2].starts_with(&["symbolic-ref"]));
    assert!(runs[3].starts_with(&["pr", "create", "--head", "x", "--base", "main"]));
    assert!(
        runs[3]
            .args
            .contains(&"FARM-12: split farm and runner".to_string())
    );
}

#[tokio::test]
async fn a_cancel_that_lands_while_the_pull_request_is_opened_abandons_it() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "1")]);
    let farm = farm_with(job(checkout.path(), &checkout.plan, CreatePr::Yes)).await;
    let mut options = options(&checkout.tools);
    options
        .pr_tools
        .env
        .push(("FAKE_GH_SLEEP".to_string(), "30".to_string()));
    let _running = start(agent(&farm, config(&farm, &ralphex), options));

    let listing = wait_for(|| match invocations(&checkout.tools).is_empty() {
        true => None,
        false => Some(()),
    })
    .await;
    assert!(listing.is_some(), "the pull-request sequence never started");
    farm.always_heartbeat(Reply::Beat(HeartbeatAction::Cancel));

    let CompleteRequest {
        status,
        pr_url,
        fail_reason,
        message: _,
        log_tail: _,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Error);
    assert_eq!(fail_reason, "canceled");
    assert!(pr_url.is_empty());
    for run in invocations(&checkout.tools) {
        assert!(
            !run.starts_with(&["pr", "create"]),
            "a canceled run opened a pull request anyway"
        );
    }
}

#[tokio::test]
async fn a_forgotten_lease_while_the_pull_request_is_opened_leaves_no_completion() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "1")]);
    let farm = farm_with(job(checkout.path(), &checkout.plan, CreatePr::Yes)).await;
    let mut options = options(&checkout.tools);
    options
        .pr_tools
        .env
        .push(("FAKE_GH_SLEEP".to_string(), "30".to_string()));
    let running = start(agent(&farm, config(&farm, &ralphex), options));

    let listing = wait_for(|| match invocations(&checkout.tools).is_empty() {
        true => None,
        false => Some(()),
    })
    .await;
    assert!(listing.is_some(), "the pull-request sequence never started");
    farm.always_heartbeat(Reply::Status(410, String::new()));

    let freed = wait_for(|| match running.agent.slot() {
        RunSlot::Running(_) => None,
        RunSlot::Opening => None,
        RunSlot::Free => Some(()),
        RunSlot::Polling => Some(()),
    })
    .await;

    assert!(freed.is_some(), "the run slot was never released");
    assert!(
        farm.requests_ending("/complete").is_empty(),
        "a forgotten run was completed"
    );
    for run in invocations(&checkout.tools) {
        assert!(
            !run.starts_with(&["pr", "create"]),
            "a forgotten run opened a pull request anyway"
        );
    }
}

#[tokio::test]
async fn a_review_job_runs_ralphex_in_review_mode() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "1")]);
    let mut asked = job(checkout.path(), &checkout.plan, CreatePr::No);
    asked.mode = "review".to_string();
    let farm = farm_with(asked).await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(&checkout.tools),
    ));

    let CompleteRequest {
        status,
        pr_url: _,
        fail_reason: _,
        message: _,
        log_tail: _,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Done);
    let record = Record::read(&checkout.record);
    assert_eq!(
        record.argv,
        vec![
            "--branch".to_string(),
            "x".to_string(),
            "--review".to_string(),
            checkout.plan.display().to_string(),
        ]
    );
}

#[tokio::test]
async fn the_lease_is_still_beaten_while_the_pull_request_is_opened() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "1")]);
    let farm = farm_with(job(checkout.path(), &checkout.plan, CreatePr::Yes)).await;
    let mut options = options(&checkout.tools);
    options
        .pr_tools
        .env
        .push(("FAKE_DELAY".to_string(), "0.3".to_string()));
    let _running = start(agent(&farm, config(&farm, &ralphex), options));

    let CompleteRequest {
        status,
        pr_url: _,
        fail_reason: _,
        message: _,
        log_tail: _,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Done);
    let Some(last_chunk) = last_index(&farm, "/log") else {
        panic!("no log chunk was sent");
    };
    let mut beats = 0;
    for request in farm.requests().iter().skip(last_chunk + 1) {
        if request.path.ends_with("/heartbeat") {
            beats += 1;
        }
    }
    assert!(
        beats >= 5,
        "the heartbeat stopped at the process exit: {beats} beats reached the farm while the pull request was opened"
    );
}

#[tokio::test]
async fn a_push_that_fails_completes_as_a_push_failure() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "1")]);
    let farm = farm_with(job(checkout.path(), &checkout.plan, CreatePr::Yes)).await;
    let mut options = options(&checkout.tools);
    options
        .pr_tools
        .env
        .push(("FAKE_FAIL".to_string(), "push".to_string()));
    let _running = start(agent(&farm, config(&farm, &ralphex), options));

    let CompleteRequest {
        status,
        pr_url,
        fail_reason,
        message,
        log_tail: _,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Error);
    assert!(pr_url.is_empty());
    assert_eq!(fail_reason, "git_push");
    assert!(message.contains("git push"), "{message}");
}

#[tokio::test]
async fn a_pull_request_that_fails_completes_as_a_creation_failure() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "1")]);
    let farm = farm_with(job(checkout.path(), &checkout.plan, CreatePr::Yes)).await;
    let mut options = options(&checkout.tools);
    options
        .pr_tools
        .env
        .push(("FAKE_FAIL".to_string(), "create".to_string()));
    let _running = start(agent(&farm, config(&farm, &ralphex), options));

    let CompleteRequest {
        status,
        pr_url: _,
        fail_reason,
        message,
        log_tail: _,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Error);
    assert_eq!(fail_reason, "pr_create");
    assert!(message.contains("gh pr create"), "{message}");
}

#[tokio::test]
async fn a_ralphex_that_cannot_be_started_completes_as_a_spawn_failure() {
    let checkout = Checkout::new();
    let absent = checkout.path().join("absent-ralphex");
    let farm = farm_with(job(checkout.path(), &checkout.plan, CreatePr::No)).await;
    let _running = start(agent(
        &farm,
        config(&farm, &absent),
        options(&checkout.tools),
    ));

    let CompleteRequest {
        status,
        pr_url: _,
        fail_reason,
        message,
        log_tail: _,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Error);
    assert_eq!(fail_reason, "spawn_failed");
    assert!(message.contains("absent-ralphex"), "{message}");
}

#[tokio::test]
async fn a_checkout_that_is_not_a_repository_completes_as_an_invalid_context() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[]);
    let absent = checkout.path().join("absent");
    let farm = farm_with(job(&absent, &checkout.plan, CreatePr::No)).await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(&checkout.tools),
    ));

    let CompleteRequest {
        status,
        pr_url: _,
        fail_reason,
        message: _,
        log_tail: _,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Error);
    assert_eq!(fail_reason, "ctx_invalid");
    assert!(!checkout.record.exists(), "ralphex was spawned anyway");
}

#[tokio::test]
async fn a_plan_outside_the_checkout_completes_as_a_missing_plan() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[]);
    let outside = tempfile::tempdir().unwrap();
    let plan = outside.path().join("plan.md");
    std::fs::write(&plan, "# plan\n").unwrap();
    let farm = farm_with(job(checkout.path(), &plan, CreatePr::No)).await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(&checkout.tools),
    ));

    let CompleteRequest {
        status,
        pr_url: _,
        fail_reason,
        message: _,
        log_tail: _,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Error);
    assert_eq!(fail_reason, "plan_not_found");
    assert!(!checkout.record.exists(), "ralphex was spawned anyway");
}

#[tokio::test]
async fn every_call_carries_the_bearer_token_and_the_runner_name() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "1")]);
    let farm = farm_with(job(checkout.path(), &checkout.plan, CreatePr::No)).await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(&checkout.tools),
    ));

    completion(&farm).await;

    for request in farm.requests() {
        assert_eq!(request.authorization, "Bearer secret-token");
    }
    let claims = farm.requests_ending("/claim");
    assert!(claims[0].text().contains(r#""runner":"mbp-native""#));
    assert!(claims[0].text().contains(r#""runtime":"native""#));
    assert!(claims[0].text().contains(r#""slots":1"#));
    let beats = farm.requests_ending("/heartbeat");
    assert!(beats[0].text().contains(r#""runner":"mbp-native""#));
    assert!(beats[0].text().contains(r#""runtime":"native""#));
}
