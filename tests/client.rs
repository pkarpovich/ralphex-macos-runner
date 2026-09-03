//! The farm client against a scripted fake farm.

mod support;

use std::sync::Arc;
use std::time::Duration;

use ralphex_macos_runner::protocol::client::{FarmClient, FarmError};
use ralphex_macos_runner::protocol::types::{
    Branch, COMPLETE_BUDGET, ClaimRequest, CompleteRequest, CompleteStatus, CreatePr,
    HeartbeatAction, HeartbeatRequest, HeartbeatResponse, Job, OpenRunRequest, RunId, RunnerName,
    Seq,
};
use support::TestSleeper;
use support::fake_farm::{FakeFarm, Reply, job_reply};

fn client(farm: &FakeFarm, sleeper: Arc<TestSleeper>) -> FarmClient {
    FarmClient::new(farm.url(), "secret-token", sleeper).unwrap()
}

fn claim_request() -> ClaimRequest {
    ClaimRequest::native(RunnerName("mbp-native".to_string()))
}

fn heartbeat_request() -> HeartbeatRequest {
    HeartbeatRequest::native(RunnerName("mbp-native".to_string()))
}

fn open_run_request() -> OpenRunRequest {
    OpenRunRequest {
        runner: RunnerName("mbp-native".to_string()),
        runtime: "native".to_string(),
        repo: "ralphex-farm".to_string(),
        ctx: "/abs/checkout".to_string(),
        plan: "/abs/checkout/docs/plans/x.md".to_string(),
        branch: Branch("x".to_string()),
        create_pr: CreatePr::Yes,
    }
}

fn complete_request() -> CompleteRequest {
    CompleteRequest {
        status: CompleteStatus::Done,
        pr_url: String::new(),
        fail_reason: String::new(),
        message: String::new(),
        log_tail: String::new(),
    }
}

fn server_error() -> Reply {
    Reply::Status(500, "boom".to_string())
}

#[tokio::test]
async fn every_call_carries_the_bearer_token() {
    let farm = FakeFarm::start().await;
    farm.push_claim(Reply::NoJob);
    farm.push_runs(job_reply("local-1"));
    let sleeper = Arc::new(TestSleeper::new());
    let client = client(&farm, Arc::clone(&sleeper));
    let run_id = RunId("local-1".to_string());

    client.claim(&claim_request()).await.unwrap();
    client.open_run(&open_run_request()).await.unwrap();
    client
        .append_log(&run_id, Seq::FIRST, b"hello")
        .await
        .unwrap();
    client
        .heartbeat(&run_id, &heartbeat_request())
        .await
        .unwrap();
    client.complete(&run_id, &complete_request()).await.unwrap();

    let requests = farm.requests();
    assert_eq!(requests.len(), 5);
    for request in &requests {
        assert_eq!(request.authorization, "Bearer secret-token");
    }
}

#[tokio::test]
async fn an_expired_claim_window_yields_no_job() {
    let farm = FakeFarm::start().await;
    farm.push_claim(Reply::NoJob);
    let client = client(&farm, Arc::new(TestSleeper::new()));

    let job = client.claim(&claim_request()).await.unwrap();

    assert_eq!(job, None);
    assert_eq!(farm.requests_ending("/claim").len(), 1);
}

#[tokio::test]
async fn a_held_claim_answers_only_once_it_is_released() {
    let farm = FakeFarm::start().await;
    farm.push_claim(Reply::Hold);
    let client = client(&farm, Arc::new(TestSleeper::new()));

    let request = claim_request();
    let polling = client.claim(&request);
    let releasing = async {
        loop {
            if !farm.requests_ending("/claim").is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        farm.release_claim(job_reply("local-1"));
    };
    let (job, ()) = tokio::join!(polling, releasing);

    let job = job.unwrap().unwrap();
    assert_eq!(job.run_id, RunId("local-1".to_string()));
}

#[tokio::test]
async fn a_claimed_job_decodes() {
    let farm = FakeFarm::start().await;
    farm.push_claim(job_reply("FARM-12-1753180800000"));
    let client = client(&farm, Arc::new(TestSleeper::new()));

    let job = client.claim(&claim_request()).await.unwrap().unwrap();

    let Job {
        run_id,
        issue_id: _,
        identifier,
        issue_url: _,
        title: _,
        repo_slug: _,
        plan_path: _,
        branch,
        mode: _,
        lease_ttl_seconds,
        runtime,
        ctx,
        create_pr,
    } = job;
    assert_eq!(run_id, RunId("FARM-12-1753180800000".to_string()));
    assert_eq!(identifier, "FARM-12");
    assert_eq!(branch, Branch("x".to_string()));
    assert_eq!(lease_ttl_seconds, 180);
    assert_eq!(runtime, "native");
    assert_eq!(ctx, "/abs/checkout");
    assert_eq!(create_pr, CreatePr::Yes);
}

#[tokio::test]
async fn a_claim_answered_409_reports_the_farms_message() {
    let farm = FakeFarm::start().await;
    farm.push_claim(Reply::Status(
        409,
        r#"{"error":"runner protocol 1, farm protocol 2"}"#.to_string(),
    ));
    let client = client(&farm, Arc::new(TestSleeper::new()));

    let error = client.claim(&claim_request()).await.unwrap_err();

    assert_eq!(
        error,
        FarmError::VersionMismatch {
            message: "runner protocol 1, farm protocol 2".to_string()
        }
    );
}

#[tokio::test]
async fn a_heartbeat_answered_409_reports_the_farms_message_and_does_not_retry() {
    let farm = FakeFarm::start().await;
    farm.always_heartbeat(Reply::Status(
        409,
        r#"{"error":"runner protocol 1, farm protocol 2"}"#.to_string(),
    ));
    let sleeper = Arc::new(TestSleeper::new());
    let client = client(&farm, Arc::clone(&sleeper));

    let error = client
        .heartbeat(&RunId("local-1".to_string()), &heartbeat_request())
        .await
        .unwrap_err();

    assert_eq!(
        error,
        FarmError::VersionMismatch {
            message: "runner protocol 1, farm protocol 2".to_string()
        }
    );
    assert_eq!(farm.requests_ending("/heartbeat").len(), 1);
    assert!(sleeper.slept().is_empty());
}

#[tokio::test]
async fn a_heartbeat_answered_410_is_gone() {
    let farm = FakeFarm::start().await;
    farm.push_heartbeat(Reply::Status(410, String::new()));
    let client = client(&farm, Arc::new(TestSleeper::new()));

    let error = client
        .heartbeat(&RunId("local-1".to_string()), &heartbeat_request())
        .await
        .unwrap_err();

    assert_eq!(error, FarmError::Gone);
}

#[tokio::test]
async fn a_heartbeat_answer_decodes_a_cancel() {
    let farm = FakeFarm::start().await;
    farm.push_heartbeat(Reply::Beat(HeartbeatAction::Cancel));
    let client = client(&farm, Arc::new(TestSleeper::new()));

    let response = client
        .heartbeat(&RunId("local-1".to_string()), &heartbeat_request())
        .await
        .unwrap();

    assert_eq!(
        response,
        HeartbeatResponse {
            action: HeartbeatAction::Cancel
        }
    );
}

#[tokio::test]
async fn a_log_chunk_survives_two_server_errors() {
    let farm = FakeFarm::start().await;
    farm.push_log(server_error());
    farm.push_log(server_error());
    farm.push_log(Reply::Accepted);
    let sleeper = Arc::new(TestSleeper::new());
    let client = client(&farm, Arc::clone(&sleeper));

    client
        .append_log(&RunId("local-1".to_string()), Seq::FIRST, b"hello")
        .await
        .unwrap();

    assert_eq!(farm.requests_ending("/log").len(), 3);
    assert_eq!(
        sleeper.slept(),
        vec![Duration::from_secs(1), Duration::from_secs(2)]
    );
    let requests = farm.requests_ending("/log");
    for request in &requests {
        assert_eq!(request.seq(), Some(1));
        assert_eq!(request.content_type, "application/octet-stream");
        assert_eq!(request.text(), "hello");
    }
}

#[tokio::test]
async fn a_log_chunk_gives_up_after_six_attempts() {
    let farm = FakeFarm::start().await;
    farm.always_log(server_error());
    let sleeper = Arc::new(TestSleeper::new());
    let client = client(&farm, Arc::clone(&sleeper));

    let error = client
        .append_log(&RunId("local-1".to_string()), Seq::FIRST, b"hello")
        .await
        .unwrap_err();

    assert_eq!(error, FarmError::Rejected(500, "boom".to_string()));
    assert_eq!(farm.requests_ending("/log").len(), 6);
    assert_eq!(sleeper.slept().len(), 5);
}

#[tokio::test]
async fn a_log_chunk_refused_with_a_client_error_is_not_retried() {
    let farm = FakeFarm::start().await;
    farm.always_log(Reply::Status(400, "bad chunk".to_string()));
    let sleeper = Arc::new(TestSleeper::new());
    let client = client(&farm, Arc::clone(&sleeper));

    let error = client
        .append_log(&RunId("local-1".to_string()), Seq::FIRST, b"hello")
        .await
        .unwrap_err();

    assert_eq!(error, FarmError::BadRequest("bad chunk".to_string()));
    assert_eq!(farm.requests_ending("/log").len(), 1);
    assert!(sleeper.slept().is_empty());
}

#[tokio::test]
async fn a_log_chunk_for_a_forgotten_run_is_gone() {
    let farm = FakeFarm::start().await;
    farm.always_log(Reply::Status(410, String::new()));
    let client = client(&farm, Arc::new(TestSleeper::new()));

    let error = client
        .append_log(&RunId("local-1".to_string()), Seq::FIRST, b"hello")
        .await
        .unwrap_err();

    assert_eq!(error, FarmError::Gone);
    assert_eq!(farm.requests_ending("/log").len(), 1);
}

#[tokio::test]
async fn a_completion_keeps_retrying_until_the_budget_runs_out() {
    let farm = FakeFarm::start().await;
    farm.always_complete(server_error());
    let sleeper = Arc::new(TestSleeper::new());
    let client = client(&farm, Arc::clone(&sleeper));

    let error = client
        .complete(&RunId("local-1".to_string()), &complete_request())
        .await
        .unwrap_err();

    assert_eq!(error, FarmError::Rejected(500, "boom".to_string()));
    let attempts = farm.requests_ending("/complete").len();
    assert!(attempts > 10, "gave up after {attempts} attempts");
    let mut elapsed = Duration::ZERO;
    for delay in sleeper.slept() {
        elapsed += delay;
    }
    assert!(elapsed >= COMPLETE_BUDGET, "stopped after {elapsed:?}");
}

#[tokio::test]
async fn a_completion_stops_when_the_run_is_already_gone() {
    let farm = FakeFarm::start().await;
    farm.push_complete(server_error());
    farm.always_complete(Reply::Status(410, String::new()));
    let sleeper = Arc::new(TestSleeper::new());
    let client = client(&farm, Arc::clone(&sleeper));

    let error = client
        .complete(&RunId("local-1".to_string()), &complete_request())
        .await
        .unwrap_err();

    assert_eq!(error, FarmError::Gone);
    assert_eq!(farm.requests_ending("/complete").len(), 2);
    assert_eq!(sleeper.slept(), vec![Duration::from_secs(1)]);
}

#[tokio::test]
async fn opening_a_run_is_never_retried() {
    let farm = FakeFarm::start().await;
    farm.always_complete(Reply::Accepted);
    farm.push_runs(server_error());
    let sleeper = Arc::new(TestSleeper::new());
    let client = client(&farm, Arc::clone(&sleeper));

    let error = client.open_run(&open_run_request()).await.unwrap_err();

    assert_eq!(error, FarmError::Rejected(500, "boom".to_string()));
    assert_eq!(farm.requests_ending("/runs").len(), 1);
    assert!(sleeper.slept().is_empty());
}

#[tokio::test]
async fn an_opened_run_decodes_and_carries_the_request_body() {
    let farm = FakeFarm::start().await;
    farm.push_runs(job_reply("local-1753180800000"));
    let client = client(&farm, Arc::new(TestSleeper::new()));

    let job = client.open_run(&open_run_request()).await.unwrap();

    assert_eq!(job.run_id, RunId("local-1753180800000".to_string()));
    let requests = farm.requests_ending("/runs");
    let body: serde_json::Value = serde_json::from_str(&requests[0].text()).unwrap();
    assert_eq!(body["runtime"], "native");
    assert_eq!(body["create_pr"], true);
    assert_eq!(requests[0].content_type, "application/json");
}

#[tokio::test]
async fn a_claim_that_cannot_reach_the_farm_reports_transport() {
    let client = FarmClient::new(
        "http://127.0.0.1:1",
        "secret-token",
        Arc::new(TestSleeper::new()),
    )
    .unwrap();

    let error = client.claim(&claim_request()).await.unwrap_err();

    let FarmError::Transport(message) = error else {
        panic!("expected a transport error");
    };
    assert!(!message.is_empty());
}
