//! The daemon's claim loop against a scripted fake farm and a fake ralphex.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ralphex_macos_runner::agent::{Agent, AgentExit, AgentOptions, LocalStart, RunSlot, Shutdown};
use ralphex_macos_runner::config::Config;
use ralphex_macos_runner::ipc::RunRequest;
use ralphex_macos_runner::job::{LocalOptions, Worktree};
use ralphex_macos_runner::pr::RunOrigin;
use ralphex_macos_runner::prdesc::{self, PR_FILE_NAME, PR_FILE_VAR};
use ralphex_macos_runner::protocol::client::FarmClient;
use ralphex_macos_runner::protocol::types::{
    Branch, CompleteRequest, CompleteStatus, CreatePr, HeartbeatAction, Job, Phase,
    ProgressRequest, ProgressTask, RunId, RunnerName,
};
use support::fake_farm::{FakeFarm, Reply};
use support::{
    Checkout, Record, TestSleeper, completion, dead, invocations, options, snapshots, spawned,
    ticket_job, wait_for,
};
use tokio::sync::watch;
use tokio::task::JoinHandle;

const MISMATCH: &str = r#"{"error":"the runner speaks 1, the farm speaks 2"}"#;

const TIMELINE_PLAN: &str = "# Wire the timeline

### Task 1: Add it
- [ ] write it
- [ ] test it

### Task 2: Ship it
- [ ] ship it
";

const MARKERS: &str = "--- task iteration 1 ---\n--- claude review 0: all findings ---";

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

fn agent(farm: &FakeFarm, config: Config, options: AgentOptions) -> Agent {
    let client = Arc::new(
        FarmClient::new(farm.url(), "secret-token", Arc::new(TestSleeper::new())).unwrap(),
    );
    Agent::new(config, client, options)
}

struct Running {
    agent: Arc<Agent>,
    raise: watch::Sender<Shutdown>,
    handle: JoinHandle<AgentExit>,
}

fn start(agent: Agent) -> Running {
    let agent = Arc::new(agent);
    let (raise, shutdown) = watch::channel(Shutdown::Running);
    let claiming = Arc::clone(&agent);
    let handle = tokio::spawn(async move { claiming.run(shutdown).await });
    Running {
        agent,
        raise,
        handle,
    }
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

fn ticked(snapshot: &ProgressRequest) -> bool {
    let ProgressRequest {
        phase: _,
        failed: _,
        tasks,
    } = snapshot;
    let Some(tasks) = tasks else {
        return false;
    };
    if tasks.is_empty() {
        return false;
    }
    for ProgressTask {
        number: _,
        ord: _,
        title: _,
        status: _,
        checkboxes,
    } in tasks
    {
        for checkbox in checkboxes {
            if !checkbox.checked {
                return false;
            }
        }
    }
    true
}

fn first_in_phase(snapshots: &[ProgressRequest], wanted: Phase) -> Option<usize> {
    for (index, snapshot) in snapshots.iter().enumerate() {
        if snapshot.phase == wanted {
            return Some(index);
        }
    }
    None
}

fn any_ticked(snapshots: &[ProgressRequest]) -> bool {
    for snapshot in snapshots {
        if ticked(snapshot) {
            return true;
        }
    }
    false
}

async fn release_after(farm: &FakeFarm, release: &Path, seen: impl Fn(&[ProgressRequest]) -> bool) {
    let posted = wait_for(|| {
        let posted = snapshots(farm);
        match seen(&posted) {
            true => Some(()),
            false => None,
        }
    })
    .await;
    assert!(posted.is_some(), "{:?}", snapshots(farm));
    std::fs::write(release, "").unwrap();
}

fn output_dir(checkout: &Checkout, run_id: &str) -> PathBuf {
    checkout.tools().with_file_name("farm-out").join(run_id)
}

async fn removed(path: &Path) -> bool {
    let gone = wait_for(|| match path.exists() {
        true => None,
        false => Some(()),
    })
    .await;
    gone.is_some()
}

fn ticket_footer(checkout: &Checkout) -> String {
    let origin = RunOrigin::Ticket {
        identifier: "FARM-12".to_string(),
        issue_url: "https://linear.app/example/issue/FARM-12".to_string(),
        title: "split farm and runner".to_string(),
    };
    prdesc::footer(
        &origin,
        &checkout.plan().display().to_string(),
        &RunId("FARM-12-1753180800000".to_string()),
    )
}

fn as_recorded(argument: &str) -> String {
    argument.replace('\n', " ")
}

async fn opened_with(checkout: &Checkout, settings: &[(&str, &str)]) -> (String, String) {
    let ralphex = checkout.ralphex(settings);
    let farm = farm_with(ticket_job(
        &checkout.path(),
        &checkout.plan(),
        CreatePr::Yes,
    ))
    .await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(checkout.tools()),
    ));

    let CompleteRequest {
        status,
        pr_url,
        fail_reason,
        message: _,
        log_tail: _,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Done, "{fail_reason}");
    assert_eq!(pr_url, "https://github.com/owner/repo/pull/7");
    let runs = invocations(checkout.tools());
    let created = &runs[3];
    assert!(created.starts_with(&["pr", "create"]), "{created:?}");
    (created.args[7].clone(), created.args[9].clone())
}

fn failures(snapshots: &[ProgressRequest]) -> usize {
    let mut failures = 0;
    for snapshot in snapshots {
        if snapshot.failed {
            failures += 1;
        }
    }
    failures
}

#[tokio::test]
async fn a_claimed_job_runs_to_done_and_its_output_reaches_the_farm() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "3")]);
    let farm = farm_with(ticket_job(&checkout.path(), &checkout.plan(), CreatePr::No)).await;
    let running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(checkout.tools()),
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
    let record = Record::read(checkout.record());
    assert_eq!(
        record.argv,
        vec![
            "--branch".to_string(),
            "x".to_string(),
            checkout.plan().display().to_string(),
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
    let farm = farm_with(ticket_job(&checkout.path(), &checkout.plan(), CreatePr::No)).await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(checkout.tools()),
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
    let farm = farm_with(ticket_job(&checkout.path(), &checkout.plan(), CreatePr::No)).await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(checkout.tools()),
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
    let farm = farm_with(ticket_job(&checkout.path(), &checkout.plan(), CreatePr::No)).await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(checkout.tools()),
    ));

    let record = spawned(checkout.record()).await;
    farm.push_heartbeat(Reply::Beat(HeartbeatAction::Cancel));
    let CompleteRequest {
        status,
        pr_url: _,
        fail_reason,
        message: _,
        log_tail,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Error);
    assert_eq!(fail_reason, "canceled");
    assert!(
        log_tail.is_empty(),
        "only a nonzero exit carries a tail: {log_tail}"
    );
    assert!(dead(record.pid).await, "the run outlived its cancel");
}

#[tokio::test]
async fn the_lease_is_still_beaten_while_a_canceled_run_is_stopped() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_IGNORE_TERM", "1")]);
    let farm = farm_with(ticket_job(&checkout.path(), &checkout.plan(), CreatePr::No)).await;
    let mut options = options(checkout.tools());
    options.stop_grace = Duration::from_secs(1);
    let _running = start(agent(&farm, config(&farm, &ralphex), options));

    let record = spawned(checkout.record()).await;
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
    let mut asked = ticket_job(&checkout.path(), &checkout.plan(), CreatePr::No);
    asked.runtime = "container".to_string();
    let farm = farm_with(asked).await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(checkout.tools()),
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
    assert!(!checkout.record().exists(), "ralphex was spawned anyway");
}

#[tokio::test]
async fn a_job_of_the_wrong_runtime_is_beaten_for_while_its_completion_is_in_flight() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[]);
    let mut asked = ticket_job(&checkout.path(), &checkout.plan(), CreatePr::No);
    asked.runtime = "container".to_string();
    let farm = farm_with(asked).await;
    farm.push_complete(Reply::Hold);
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(checkout.tools()),
    ));

    let beaten = wait_for(|| match farm.requests_ending("/heartbeat").is_empty() {
        true => None,
        false => Some(()),
    })
    .await;
    assert!(
        beaten.is_some(),
        "a refused job was completed under a lease nobody renewed"
    );
    farm.release_complete(Reply::Accepted);

    let CompleteRequest {
        status,
        pr_url: _,
        fail_reason,
        message: _,
        log_tail: _,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Error);
    assert_eq!(fail_reason, "runtime_mismatch");
    assert!(!checkout.record().exists(), "ralphex was spawned anyway");
}

#[tokio::test]
async fn a_job_whose_checkout_is_unusable_is_beaten_for_while_its_completion_is_in_flight() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[]);
    let absent = checkout.path().join("absent");
    let farm = farm_with(ticket_job(&absent, &checkout.plan(), CreatePr::No)).await;
    farm.push_complete(Reply::Hold);
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(checkout.tools()),
    ));

    let beaten = wait_for(|| match farm.requests_ending("/heartbeat").is_empty() {
        true => None,
        false => Some(()),
    })
    .await;
    assert!(
        beaten.is_some(),
        "a job that failed validation was completed under a lease nobody renewed"
    );
    farm.release_complete(Reply::Accepted);

    let CompleteRequest {
        status,
        pr_url: _,
        fail_reason,
        message: _,
        log_tail: _,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Error);
    assert_eq!(fail_reason, "ctx_invalid");
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
        options(checkout.tools()),
    ));

    let exit = running.handle.await.unwrap();

    assert_eq!(
        exit,
        AgentExit::VersionMismatch {
            message: "the runner speaks 1, the farm speaks 2".to_string()
        }
    );
    assert!(!checkout.record().exists(), "ralphex was spawned anyway");
}

#[tokio::test]
async fn a_version_mismatch_on_the_heartbeat_stops_the_run_and_ends_the_agent() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_SLEEP", "120")]);
    let farm = farm_with(ticket_job(&checkout.path(), &checkout.plan(), CreatePr::No)).await;
    let running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(checkout.tools()),
    ));

    let record = spawned(checkout.record()).await;
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
    let farm = farm_with(ticket_job(&checkout.path(), &checkout.plan(), CreatePr::No)).await;
    let running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(checkout.tools()),
    ));

    let record = spawned(checkout.record()).await;
    farm.always_heartbeat(Reply::Status(410, String::new()));

    assert!(dead(record.pid).await, "the run outlived its lease");
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
}

#[tokio::test]
async fn a_forgotten_log_stream_leaves_the_run_alone() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "2")]);
    let farm = farm_with(ticket_job(&checkout.path(), &checkout.plan(), CreatePr::No)).await;
    farm.always_log(Reply::Status(410, String::new()));
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(checkout.tools()),
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
    farm.push_claim(Reply::Job(Box::new(ticket_job(
        &checkout.path(),
        &checkout.plan(),
        CreatePr::No,
    ))));
    farm.push_claim(Reply::Job(Box::new(ticket_job(
        &checkout.path(),
        &checkout.plan(),
        CreatePr::No,
    ))));
    let running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(checkout.tools()),
    ));

    let record = spawned(checkout.record()).await;
    let held = wait_for(|| match running.agent.slot() {
        RunSlot::Running(run_id) => Some(run_id),
        RunSlot::Free => None,
        RunSlot::Polling => None,
        RunSlot::Opening => None,
    })
    .await;
    assert_eq!(held, Some(RunId("FARM-12-1753180800000".to_string())));
    assert_eq!(farm.requests_ending("/claim").len(), 1);

    running.raise.send_replace(Shutdown::Draining);
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
    let farm = farm_with(ticket_job(&checkout.path(), &checkout.plan(), CreatePr::No)).await;
    let running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(checkout.tools()),
    ));

    let record = spawned(checkout.record()).await;
    running.raise.send_replace(Shutdown::Draining);

    let CompleteRequest {
        status,
        pr_url: _,
        fail_reason,
        message,
        log_tail,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Error);
    assert_eq!(fail_reason, "runner_shutdown");
    assert!(message.contains("shut down"), "{message}");
    assert!(
        log_tail.is_empty(),
        "only a nonzero exit carries a tail: {log_tail}"
    );
    assert!(dead(record.pid).await, "the run outlived the shutdown");
    assert_eq!(running.handle.await.unwrap(), AgentExit::Shutdown);
}

#[tokio::test]
async fn a_second_signal_stops_the_run_without_waiting_the_drain_out() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_SLEEP", "120")]);
    let farm = farm_with(ticket_job(&checkout.path(), &checkout.plan(), CreatePr::No)).await;
    let mut options = options(checkout.tools());
    options.drain_timeout = Duration::from_secs(3600);
    let running = start(agent(&farm, config(&farm, &ralphex), options));

    let record = spawned(checkout.record()).await;
    running.raise.send_replace(Shutdown::Draining);
    running.raise.send_replace(Shutdown::Hurry);

    let CompleteRequest {
        status,
        pr_url: _,
        fail_reason,
        message: _,
        log_tail: _,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Error);
    assert_eq!(fail_reason, "runner_shutdown");
    assert!(dead(record.pid).await, "the run outlived the second signal");
    assert_eq!(running.handle.await.unwrap(), AgentExit::Shutdown);
}

#[tokio::test]
async fn a_shutdown_before_the_first_claim_stops_the_agent_at_once() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[]);
    let farm = FakeFarm::start().await;
    farm.always_claim(Reply::Hold);
    let agent = agent(&farm, config(&farm, &ralphex), options(checkout.tools()));
    let (raise, shutdown) = watch::channel(Shutdown::Draining);

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
    let mut options = options(checkout.tools());
    options.claim_retry_delay = Duration::from_secs(120);
    let running = start(agent(&farm, config(&farm, &ralphex), options));

    let polled = wait_for(|| match farm.requests_ending("/claim").is_empty() {
        true => None,
        false => Some(()),
    })
    .await;
    assert!(polled.is_some(), "the loop never polled");

    running.raise.send_replace(Shutdown::Draining);
    let exit = tokio::time::timeout(Duration::from_secs(5), running.handle).await;

    let Ok(exit) = exit else {
        panic!("the shutdown waited the pause out");
    };
    assert_eq!(exit.unwrap(), AgentExit::Shutdown);
    assert_eq!(
        farm.requests_ending("/claim").len(),
        1,
        "an empty claim was not paced"
    );
}

#[tokio::test]
async fn a_claim_the_farm_refuses_is_paced_on_a_growing_delay() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[]);
    let farm = FakeFarm::start().await;
    farm.always_claim(Reply::Status(401, "bad token".to_string()));
    let mut options = options(checkout.tools());
    options.claim_retry_delay = Duration::from_millis(50);
    let running = start(agent(&farm, config(&farm, &ralphex), options));

    let started = Instant::now();
    let polled = wait_for(|| match farm.requests_ending("/claim").len() >= 4 {
        true => Some(()),
        false => None,
    })
    .await;
    let elapsed = started.elapsed();

    assert!(polled.is_some(), "the loop stopped polling");
    assert!(
        elapsed >= Duration::from_millis(300),
        "a refused claim was retried on a flat delay: four polls in {elapsed:?}"
    );
    running.raise.send_replace(Shutdown::Draining);
    assert_eq!(running.handle.await.unwrap(), AgentExit::Shutdown);
}

#[tokio::test]
async fn a_shutdown_ends_the_pause_after_a_refused_claim() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[]);
    let farm = FakeFarm::start().await;
    farm.always_claim(Reply::Status(401, "bad token".to_string()));
    let mut options = options(checkout.tools());
    options.claim_retry_delay = Duration::from_secs(120);
    let running = start(agent(&farm, config(&farm, &ralphex), options));

    let polled = wait_for(|| match farm.requests_ending("/claim").is_empty() {
        true => None,
        false => Some(()),
    })
    .await;
    assert!(polled.is_some(), "the loop never polled");

    running.raise.send_replace(Shutdown::Draining);
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
        options(checkout.tools()),
    ));

    let polling = wait_for(|| match farm.requests_ending("/claim").is_empty() {
        true => None,
        false => Some(()),
    })
    .await;
    assert!(polling.is_some(), "the claim never reached the farm");
    running.raise.send_replace(Shutdown::Draining);
    farm.release_claim(Reply::Job(Box::new(ticket_job(
        &checkout.path(),
        &checkout.plan(),
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
        !checkout.record().exists(),
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
        options(checkout.tools()),
    ));
    let (raise, shutdown) = watch::channel(Shutdown::Draining);

    let started = agent
        .start_local(
            RunRequest {
                ctx: checkout.path().display().to_string(),
                plan: checkout.plan().display().to_string(),
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
    assert!(!checkout.record().exists(), "ralphex was started anyway");
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
        options(checkout.tools()),
    ));
    let (raise, shutdown) = watch::channel(Shutdown::Running);
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
        plan: checkout.plan().display().to_string(),
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
    assert!(!checkout.record().exists(), "ralphex was started anyway");
    drop(raise);
}

#[tokio::test]
async fn a_local_run_asked_for_after_a_mismatched_heartbeat_is_refused() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_SLEEP", "120")]);
    let farm = FakeFarm::start().await;
    farm.push_claim(Reply::Job(Box::new(ticket_job(
        &checkout.path(),
        &checkout.plan(),
        CreatePr::No,
    ))));
    farm.always_claim(Reply::Hold);
    let agent = Arc::new(agent(
        &farm,
        config(&farm, &ralphex),
        options(checkout.tools()),
    ));
    let (raise, shutdown) = watch::channel(Shutdown::Running);
    let claiming = Arc::clone(&agent);
    let claims = tokio::spawn({
        let shutdown = shutdown.clone();
        async move { claiming.run(shutdown).await }
    });
    spawned(checkout.record()).await;
    farm.always_heartbeat(Reply::Status(409, MISMATCH.to_string()));
    let exit = claims.await.unwrap();

    let started = agent
        .start_local(
            RunRequest {
                ctx: checkout.path().display().to_string(),
                plan: checkout.plan().display().to_string(),
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
        options(checkout.tools()),
    ));
    let (raise, shutdown) = watch::channel(Shutdown::Running);
    let opening = Arc::clone(&agent);
    let request = RunRequest {
        ctx: checkout.path().display().to_string(),
        plan: checkout.plan().display().to_string(),
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
    raise.send_replace(Shutdown::Draining);
    farm.release_runs(Reply::Job(Box::new(ticket_job(
        &checkout.path(),
        &checkout.plan(),
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
        !checkout.record().exists(),
        "ralphex was started for a run the farm minted during a shutdown"
    );
    drop(raise);
}

#[tokio::test]
async fn a_finished_run_opens_a_pull_request() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "1")]);
    let farm = farm_with(ticket_job(
        &checkout.path(),
        &checkout.plan(),
        CreatePr::Yes,
    ))
    .await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(checkout.tools()),
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
    let runs = invocations(checkout.tools());
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
    let farm = farm_with(ticket_job(
        &checkout.path(),
        &checkout.plan(),
        CreatePr::Yes,
    ))
    .await;
    let mut options = options(checkout.tools());
    options
        .pr_tools
        .env
        .push(("FAKE_GH_SLEEP".to_string(), "30".to_string()));
    let _running = start(agent(&farm, config(&farm, &ralphex), options));

    let listing = wait_for(|| match invocations(checkout.tools()).is_empty() {
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
    for run in invocations(checkout.tools()) {
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
    let farm = farm_with(ticket_job(
        &checkout.path(),
        &checkout.plan(),
        CreatePr::Yes,
    ))
    .await;
    let mut options = options(checkout.tools());
    options
        .pr_tools
        .env
        .push(("FAKE_GH_SLEEP".to_string(), "30".to_string()));
    let running = start(agent(&farm, config(&farm, &ralphex), options));

    let listing = wait_for(|| match invocations(checkout.tools()).is_empty() {
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
    for run in invocations(checkout.tools()) {
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
    let mut asked = ticket_job(&checkout.path(), &checkout.plan(), CreatePr::No);
    asked.mode = "review".to_string();
    let farm = farm_with(asked).await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(checkout.tools()),
    ));

    let CompleteRequest {
        status,
        pr_url: _,
        fail_reason: _,
        message: _,
        log_tail: _,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Done);
    let record = Record::read(checkout.record());
    assert_eq!(
        record.argv,
        vec![
            "--branch".to_string(),
            "x".to_string(),
            "--review".to_string(),
            checkout.plan().display().to_string(),
        ]
    );
}

#[tokio::test]
async fn the_lease_is_still_beaten_while_the_pull_request_is_opened() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "1")]);
    let farm = farm_with(ticket_job(
        &checkout.path(),
        &checkout.plan(),
        CreatePr::Yes,
    ))
    .await;
    let mut options = options(checkout.tools());
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
    let farm = farm_with(ticket_job(
        &checkout.path(),
        &checkout.plan(),
        CreatePr::Yes,
    ))
    .await;
    let mut options = options(checkout.tools());
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
    let farm = farm_with(ticket_job(
        &checkout.path(),
        &checkout.plan(),
        CreatePr::Yes,
    ))
    .await;
    let mut options = options(checkout.tools());
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
    let farm = farm_with(ticket_job(&checkout.path(), &checkout.plan(), CreatePr::No)).await;
    let _running = start(agent(
        &farm,
        config(&farm, &absent),
        options(checkout.tools()),
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
    let farm = farm_with(ticket_job(&absent, &checkout.plan(), CreatePr::No)).await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(checkout.tools()),
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
    assert!(!checkout.record().exists(), "ralphex was spawned anyway");
}

#[tokio::test]
async fn a_plan_outside_the_checkout_completes_as_a_missing_plan() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[]);
    let outside = tempfile::tempdir().unwrap();
    let plan = outside.path().join("plan.md");
    std::fs::write(&plan, "# plan\n").unwrap();
    let farm = farm_with(ticket_job(&checkout.path(), &plan, CreatePr::No)).await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(checkout.tools()),
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
    assert!(!checkout.record().exists(), "ralphex was spawned anyway");
}

#[tokio::test]
async fn every_call_carries_the_bearer_token_and_the_runner_name() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_LINES", "1")]);
    let farm = farm_with(ticket_job(&checkout.path(), &checkout.plan(), CreatePr::No)).await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(checkout.tools()),
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

#[tokio::test]
async fn a_finished_run_posts_its_timeline_and_the_pr_phase_last_before_the_push() {
    let checkout = Checkout::new();
    checkout.write_plan(TIMELINE_PLAN);
    let release = checkout.dir().join("release").display().to_string();
    let ralphex = checkout.ralphex(&[
        ("FAKE_RALPHEX_MARKERS", MARKERS),
        ("FAKE_RALPHEX_TICK", "1"),
        ("FAKE_RALPHEX_COMPLETE", "1"),
        ("FAKE_RALPHEX_WAIT_FOR", &release),
    ]);
    let farm = farm_with(ticket_job(
        &checkout.path(),
        &checkout.plan(),
        CreatePr::Yes,
    ))
    .await;
    let mut options = options(checkout.tools());
    options
        .pr_tools
        .env
        .push(("FAKE_DELAY".to_string(), "0.3".to_string()));
    let _running = start(agent(&farm, config(&farm, &ralphex), options));
    release_after(&farm, Path::new(&release), |posted| {
        first_in_phase(posted, Phase::Review).is_some() && any_ticked(posted)
    })
    .await;

    let pushing = wait_for(|| {
        for run in invocations(checkout.tools()) {
            if run.starts_with(&["push"]) {
                return Some(snapshots(&farm));
            }
        }
        None
    })
    .await;
    let Some(before_push) = pushing else {
        panic!("the branch was never pushed");
    };
    let Some(last) = before_push.last() else {
        panic!("nothing was posted before the push");
    };
    assert_eq!(last.phase, Phase::Pr, "{before_push:?}");

    let CompleteRequest {
        status,
        pr_url: _,
        fail_reason: _,
        message: _,
        log_tail: _,
    } = completion(&farm).await;
    assert_eq!(status, CompleteStatus::Done);

    let posted = snapshots(&farm);
    assert_eq!(
        posted.len(),
        before_push.len(),
        "a snapshot followed the pr phase"
    );
    assert_eq!(posted[0].phase, Phase::Setup, "{posted:?}");
    let tasks = first_in_phase(&posted, Phase::Tasks);
    let review = first_in_phase(&posted, Phase::Review);
    assert!(tasks.is_some(), "no tasks phase: {posted:?}");
    assert!(review.is_some(), "no review phase: {posted:?}");
    assert!(tasks < review, "{posted:?}");
    assert!(
        any_ticked(&posted[..posted.len() - 1]),
        "the ticks were never posted: {posted:?}"
    );
    let Some(last) = posted.last() else {
        panic!("nothing was posted");
    };
    assert_eq!(last.phase, Phase::Pr);
    assert!(!last.failed);
    assert!(
        ticked(last),
        "the pr snapshot lost the completed plan: {last:?}"
    );
    assert_eq!(failures(&posted), 0, "{posted:?}");
    assert!(last_index(&farm, "/progress") < first_index(&farm, "/complete"));
}

#[tokio::test]
async fn a_run_without_a_pull_request_posts_no_pr_phase() {
    let checkout = Checkout::new();
    checkout.write_plan(TIMELINE_PLAN);
    let release = checkout.dir().join("release").display().to_string();
    let ralphex = checkout.ralphex(&[
        ("FAKE_RALPHEX_MARKERS", MARKERS),
        ("FAKE_RALPHEX_TICK", "1"),
        ("FAKE_RALPHEX_WAIT_FOR", &release),
    ]);
    let farm = farm_with(ticket_job(&checkout.path(), &checkout.plan(), CreatePr::No)).await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(checkout.tools()),
    ));
    release_after(&farm, Path::new(&release), |posted| {
        first_in_phase(posted, Phase::Review).is_some() && any_ticked(posted)
    })
    .await;

    let CompleteRequest {
        status,
        pr_url: _,
        fail_reason: _,
        message: _,
        log_tail: _,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Done);
    let posted = snapshots(&farm);
    assert_eq!(posted[0].phase, Phase::Setup, "{posted:?}");
    assert!(
        first_in_phase(&posted, Phase::Review).is_some(),
        "{posted:?}"
    );
    assert_eq!(first_in_phase(&posted, Phase::Pr), None, "{posted:?}");
    assert_eq!(failures(&posted), 0, "{posted:?}");
}

#[tokio::test]
async fn a_worktree_run_posts_the_tasks_of_the_worktree_copy() {
    let checkout = Checkout::new();
    checkout.write_plan(TIMELINE_PLAN);
    let release = checkout.dir().join("release").display().to_string();
    let ralphex = checkout.ralphex(&[
        ("FAKE_RALPHEX_TICK", "1"),
        ("FAKE_RALPHEX_WAIT_FOR", &release),
    ]);
    let farm = FakeFarm::start().await;
    let agent = agent(&farm, config(&farm, &ralphex), options(checkout.tools()));
    let (_raise, shutdown) = watch::channel(Shutdown::Running);
    let local = LocalOptions {
        worktree: Worktree::Yes,
        env: Vec::new(),
    };
    let job = ticket_job(&checkout.path(), &checkout.plan(), CreatePr::No);
    let running = tokio::spawn(async move { agent.run_job(job, local, shutdown).await });

    release_after(&farm, Path::new(&release), any_ticked).await;
    running.await.unwrap();

    let copy = checkout
        .path()
        .join(".ralphex/worktrees/x")
        .join(checkout.plan().file_name().unwrap());
    assert!(copy.is_file(), "the fake never wrote the worktree copy");
    let original = std::fs::read_to_string(checkout.plan()).unwrap();
    assert!(original.contains("- [ ]"), "{original}");
}

#[tokio::test]
async fn a_nonzero_exit_posts_a_failed_snapshot_before_its_completion() {
    let checkout = Checkout::new();
    checkout.write_plan(TIMELINE_PLAN);
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_TICK", "1"), ("FAKE_RALPHEX_EXIT", "3")]);
    let farm = farm_with(ticket_job(
        &checkout.path(),
        &checkout.plan(),
        CreatePr::Yes,
    ))
    .await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(checkout.tools()),
    ));

    let CompleteRequest {
        status,
        pr_url: _,
        fail_reason,
        message: _,
        log_tail: _,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Error);
    assert_eq!(fail_reason, "nonzero_exit");
    let posted = snapshots(&farm);
    let Some(last) = posted.last() else {
        panic!("nothing was posted");
    };
    assert!(last.failed, "{posted:?}");
    assert!(
        ticked(last),
        "the failed snapshot did not re-read the plan: {last:?}"
    );
    assert_eq!(failures(&posted), 1, "{posted:?}");
    assert_eq!(first_in_phase(&posted, Phase::Pr), None, "{posted:?}");
    assert!(last_index(&farm, "/progress") < first_index(&farm, "/complete"));
}

#[tokio::test]
async fn a_canceled_run_posts_a_failed_snapshot_before_its_completion() {
    let checkout = Checkout::new();
    checkout.write_plan(TIMELINE_PLAN);
    let ralphex = checkout.ralphex(&[
        ("FAKE_RALPHEX_MARKERS", "--- task iteration 1 ---"),
        ("FAKE_RALPHEX_SLEEP", "120"),
    ]);
    let farm = farm_with(ticket_job(&checkout.path(), &checkout.plan(), CreatePr::No)).await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(checkout.tools()),
    ));

    spawned(checkout.record()).await;
    let started = wait_for(|| first_in_phase(&snapshots(&farm), Phase::Tasks)).await;
    assert!(started.is_some(), "the task iteration was never posted");
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
    let posted = snapshots(&farm);
    let Some(last) = posted.last() else {
        panic!("nothing was posted");
    };
    assert!(last.failed, "{posted:?}");
    assert_eq!(last.phase, Phase::Tasks, "{posted:?}");
    assert_eq!(failures(&posted), 1, "{posted:?}");
    assert!(last_index(&farm, "/progress") < first_index(&farm, "/complete"));
}

#[tokio::test]
async fn a_run_refused_at_validation_posts_no_snapshot() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[]);
    let absent = checkout.path().join("absent");
    let farm = farm_with(ticket_job(&absent, &checkout.plan(), CreatePr::No)).await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(checkout.tools()),
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
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(snapshots(&farm).is_empty(), "{:?}", snapshots(&farm));
}

#[tokio::test]
async fn a_run_is_told_where_finalize_writes_its_description() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[("FAKE_RALPHEX_SLEEP", "120")]);
    let farm = farm_with(ticket_job(&checkout.path(), &checkout.plan(), CreatePr::No)).await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(checkout.tools()),
    ));

    spawned(checkout.record()).await;
    let output = output_dir(&checkout, "FARM-12-1753180800000");
    let seen = wait_for(|| Record::read(checkout.record()).env_value(PR_FILE_VAR)).await;

    assert_eq!(seen, Some(output.join(PR_FILE_NAME).display().to_string()));
    assert!(output.is_dir(), "{} was not made", output.display());
    farm.push_heartbeat(Reply::Beat(HeartbeatAction::Cancel));
    let _completed = completion(&farm).await;
}

#[tokio::test]
async fn a_written_description_opens_the_pull_request_with_the_footer_appended() {
    let checkout = Checkout::new();

    let (title, body) = opened_with(
        &checkout,
        &[(
            "FAKE_RALPHEX_PR_DESCRIPTION",
            "# Split the farm from its runner\n\nThe runner moves out.\n\n- one\n- two",
        )],
    )
    .await;

    assert_eq!(title, "Split the farm from its runner");
    let expected = format!(
        "The runner moves out.\n\n- one\n- two{}",
        ticket_footer(&checkout)
    );
    assert_eq!(body, as_recorded(&expected));
    assert!(
        removed(&output_dir(&checkout, "FARM-12-1753180800000")).await,
        "the output directory outlived a done run"
    );
}

#[tokio::test]
async fn a_run_without_a_description_opens_with_the_fallback_title_and_the_footer() {
    let checkout = Checkout::new();

    let (title, body) = opened_with(&checkout, &[]).await;

    assert_eq!(title, "FARM-12: split farm and runner");
    assert_eq!(body, as_recorded(&ticket_footer(&checkout)));
}

#[tokio::test]
async fn an_invalid_description_opens_with_the_fallback_title_and_the_footer() {
    let checkout = Checkout::new();

    let (title, body) = opened_with(
        &checkout,
        &[("FAKE_RALPHEX_PR_DESCRIPTION", "# Only a title")],
    )
    .await;

    assert_eq!(title, "FARM-12: split farm and runner");
    assert_eq!(body, as_recorded(&ticket_footer(&checkout)));
}

#[tokio::test]
async fn the_output_directory_is_gone_after_a_failed_run() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[
        ("FAKE_RALPHEX_PR_DESCRIPTION", "# Title\n\nBody"),
        ("FAKE_RALPHEX_EXIT", "3"),
    ]);
    let farm = farm_with(ticket_job(
        &checkout.path(),
        &checkout.plan(),
        CreatePr::Yes,
    ))
    .await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(checkout.tools()),
    ));

    let CompleteRequest {
        status,
        pr_url: _,
        fail_reason,
        message: _,
        log_tail: _,
    } = completion(&farm).await;

    assert_eq!(status, CompleteStatus::Error);
    assert_eq!(fail_reason, "nonzero_exit");
    assert!(
        removed(&output_dir(&checkout, "FARM-12-1753180800000")).await,
        "the output directory outlived a failed run"
    );
}

#[tokio::test]
async fn the_output_directory_is_gone_after_a_canceled_run() {
    let checkout = Checkout::new();
    let ralphex = checkout.ralphex(&[
        ("FAKE_RALPHEX_PR_DESCRIPTION", "# Title\n\nBody"),
        ("FAKE_RALPHEX_SLEEP", "120"),
    ]);
    let farm = farm_with(ticket_job(
        &checkout.path(),
        &checkout.plan(),
        CreatePr::Yes,
    ))
    .await;
    let _running = start(agent(
        &farm,
        config(&farm, &ralphex),
        options(checkout.tools()),
    ));

    spawned(checkout.record()).await;
    let output = output_dir(&checkout, "FARM-12-1753180800000");
    let written = wait_for(|| match output.join(PR_FILE_NAME).is_file() {
        true => Some(()),
        false => None,
    })
    .await;
    assert!(written.is_some(), "the description was never written");
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
    assert!(
        removed(&output).await,
        "the output directory outlived a canceled run"
    );
    assert!(invocations(checkout.tools()).is_empty());
}
