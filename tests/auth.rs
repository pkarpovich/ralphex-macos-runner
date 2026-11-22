//! The runner as a client of the authorization server, against the fake farm's token endpoint.

mod support;

use std::sync::Arc;
use std::time::Duration;

use ralphex_macos_runner::auth::{ASSERTION_TYPE, AuthError, GRANT_TYPE, SCOPE};
use ralphex_macos_runner::protocol::client::{FarmClient, FarmError, Sleeper};
use ralphex_macos_runner::protocol::types::{
    CompleteRequest, CompleteStatus, HeartbeatAction, HeartbeatRequest, HeartbeatResponse,
    RETRY_MAX_ATTEMPTS, RunId, Seq, TOKEN_RENEW_MARGIN,
};
use support::fake_farm::{FakeFarm, Grant, Reply, TOKEN_LIFETIME, TOKEN_PATH, job_reply};
use support::{
    CLIENT_ID, KEY_ID, RESOURCE, TestSleeper, credentials, farm_client, farm_client_as,
    token_source,
};

fn run_id() -> RunId {
    RunId("FARM-12-1753180800000".to_string())
}

fn completion() -> CompleteRequest {
    CompleteRequest {
        status: CompleteStatus::Done,
        pr_url: String::new(),
        fail_reason: String::new(),
        message: String::new(),
        log_tail: String::new(),
    }
}

fn refused_token() -> Grant {
    Grant::Status(503, r#"{"error":"temporarily_unavailable"}"#.to_string())
}

#[tokio::test]
async fn the_token_request_is_the_contracts_form_and_assertion() {
    let farm = FakeFarm::start().await;
    let sleeper = Arc::new(TestSleeper::new());
    let tokens = token_source(credentials(&farm), sleeper);

    let token = tokens.token().await.unwrap();

    assert_eq!(token.secret(), "token-1");
    let requests = farm.token_requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.refusal, "", "the endpoint refused the request");
    let mut fields: Vec<&str> = Vec::new();
    for key in request.form.keys() {
        fields.push(key);
    }
    fields.sort_unstable();
    assert_eq!(
        fields,
        vec![
            "client_assertion",
            "client_assertion_type",
            "client_id",
            "grant_type",
            "resource",
            "scope",
        ]
    );
    assert_eq!(request.form["grant_type"], GRANT_TYPE);
    assert_eq!(request.form["client_id"], CLIENT_ID);
    assert_eq!(request.form["client_assertion_type"], ASSERTION_TYPE);
    assert_eq!(request.form["resource"], RESOURCE);
    assert_eq!(request.form["scope"], SCOPE);
    assert_eq!(request.header["alg"], "RS256");
    assert_eq!(request.header["kid"], KEY_ID);
    assert_eq!(request.claims["iss"], CLIENT_ID);
    assert_eq!(request.claims["sub"], CLIENT_ID);
    assert_eq!(request.claims["aud"], farm.url());
    let iat = request.claims["iat"].as_u64().unwrap();
    let exp = request.claims["exp"].as_u64().unwrap();
    assert_eq!(exp - iat, 60);
}

#[tokio::test]
async fn every_token_request_signs_a_fresh_assertion() {
    let farm = FakeFarm::start().await;
    farm.push_grant(Grant::Token(1));
    farm.push_grant(Grant::Token(1));
    let tokens = token_source(credentials(&farm), Arc::new(TestSleeper::new()));

    let first = tokens.token().await.unwrap();
    let second = tokens.token().await.unwrap();

    assert_ne!(first, second);
    let requests = farm.token_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].refusal, "", "a replayed jti was refused");
    assert_ne!(requests[0].claims["jti"], requests[1].claims["jti"]);
    assert_ne!(
        requests[0].form["client_assertion"],
        requests[1].form["client_assertion"]
    );
}

#[tokio::test]
async fn a_token_is_kept_until_sixty_seconds_before_it_expires() {
    let farm = FakeFarm::start().await;
    let sleeper = Arc::new(TestSleeper::new());
    let tokens = token_source(credentials(&farm), sleeper.clone());
    let lifetime = Duration::from_secs(TOKEN_LIFETIME);

    let first = tokens.token().await.unwrap();
    sleeper
        .sleep(lifetime - TOKEN_RENEW_MARGIN - Duration::from_secs(1))
        .await;
    let kept = tokens.token().await.unwrap();
    sleeper.sleep(Duration::from_secs(1)).await;
    let renewed = tokens.token().await.unwrap();

    assert_eq!(first, kept);
    assert_ne!(kept, renewed);
    assert_eq!(farm.token_requests().len(), 2);
}

#[tokio::test]
async fn a_token_shorter_lived_than_the_margin_is_asked_for_again_each_time() {
    let farm = FakeFarm::start().await;
    farm.push_grant(Grant::Token(30));
    let tokens = token_source(credentials(&farm), Arc::new(TestSleeper::new()));

    let first = tokens.token().await.unwrap();
    let second = tokens.token().await.unwrap();

    assert_ne!(first, second);
    assert_eq!(farm.token_requests().len(), 2);
}

#[tokio::test]
async fn a_refusal_carries_the_status_and_the_oauth_code_only() {
    let farm = FakeFarm::start().await;
    farm.push_grant(Grant::Status(
        400,
        r#"{"error":"invalid_scope","error_description":"farm:run is not allowed"}"#.to_string(),
    ));
    let tokens = token_source(credentials(&farm), Arc::new(TestSleeper::new()));

    let refused = tokens.token().await.unwrap_err();

    assert_eq!(
        refused,
        AuthError::Refused {
            status: 400,
            code: Some("invalid_scope".to_string()),
        }
    );
    assert_eq!(
        refused.to_string(),
        "the token endpoint answered 400 (invalid_scope)"
    );
}

#[tokio::test]
async fn an_assertion_the_endpoint_cannot_verify_is_refused_as_invalid_client() {
    let farm = FakeFarm::start().await;
    let mut unknown = credentials(&farm);
    unknown.key_id = "another-key".to_string();
    let tokens = token_source(unknown, Arc::new(TestSleeper::new()));

    let refused = tokens.token().await.unwrap_err();

    assert_eq!(
        refused,
        AuthError::Refused {
            status: 401,
            code: Some("invalid_client".to_string()),
        }
    );
    assert_eq!(farm.token_requests()[0].refusal, "unknown kid");
}

#[tokio::test]
async fn a_farm_that_refuses_the_token_gets_one_repeat_with_a_new_token() {
    let farm = FakeFarm::start().await;
    let client = farm_client(&farm, Arc::new(TestSleeper::new()));
    client
        .heartbeat(&run_id(), &HeartbeatRequest::native())
        .await
        .unwrap();
    farm.revoke_tokens();

    let answer = client
        .heartbeat(&run_id(), &HeartbeatRequest::native())
        .await;

    assert_eq!(
        answer,
        Ok(HeartbeatResponse {
            action: HeartbeatAction::None
        })
    );
    let beats = farm.requests_ending("/heartbeat");
    assert_eq!(beats.len(), 3);
    assert_eq!(beats[1].authorization, "Bearer token-1");
    assert_eq!(beats[2].authorization, "Bearer token-2");
    assert_eq!(farm.token_requests().len(), 2);
}

#[tokio::test]
async fn a_second_401_is_final() {
    let farm = FakeFarm::start().await;
    farm.push_heartbeat(Reply::Status(
        401,
        r#"{"error":"invalid_token"}"#.to_string(),
    ));
    farm.push_heartbeat(Reply::Status(
        401,
        r#"{"error":"invalid_token"}"#.to_string(),
    ));
    let sleeper = Arc::new(TestSleeper::new());
    let client = farm_client(&farm, sleeper.clone());

    let refused = client
        .heartbeat(&run_id(), &HeartbeatRequest::native())
        .await
        .unwrap_err();

    let FarmError::Rejected(401, _) = refused else {
        panic!("a second 401 is final: {refused:?}");
    };
    assert_eq!(farm.requests_ending("/heartbeat").len(), 2);
    assert_eq!(farm.token_requests().len(), 2);
    assert!(
        sleeper.slept().is_empty(),
        "a 401 was retried with a backoff"
    );
}

#[tokio::test]
async fn a_server_error_after_the_repeat_goes_back_to_the_backoff() {
    let farm = FakeFarm::start().await;
    farm.push_log(Reply::Status(
        401,
        r#"{"error":"invalid_token"}"#.to_string(),
    ));
    farm.push_log(Reply::Status(503, "busy".to_string()));
    let sleeper = Arc::new(TestSleeper::new());
    let client = farm_client(&farm, sleeper.clone());

    client
        .append_log(&run_id(), Seq::FIRST, b"line\n")
        .await
        .unwrap();

    assert_eq!(farm.requests_ending("/log").len(), 3);
    assert_eq!(sleeper.slept(), vec![Duration::from_secs(1)]);
}

#[tokio::test]
async fn a_claim_and_an_opening_also_get_the_single_repeat() {
    let farm = FakeFarm::start().await;
    farm.push_claim(Reply::Status(
        401,
        r#"{"error":"invalid_token"}"#.to_string(),
    ));
    farm.push_claim(job_reply("FARM-12-1"));
    farm.push_runs(Reply::Status(
        401,
        r#"{"error":"invalid_token"}"#.to_string(),
    ));
    farm.push_runs(job_reply("local-1"));
    let client = farm_client(&farm, Arc::new(TestSleeper::new()));

    let claimed = client
        .claim(&ralphex_macos_runner::protocol::types::ClaimRequest::native())
        .await
        .unwrap();
    let opened = client
        .open_run(&ralphex_macos_runner::protocol::types::OpenRunRequest {
            version: ralphex_macos_runner::protocol::types::VERSION.to_string(),
            runtime: "native".to_string(),
            repo: "ralphex-farm".to_string(),
            ctx: "/abs/checkout".to_string(),
            plan: "/abs/checkout/docs/plans/x.md".to_string(),
            branch: ralphex_macos_runner::protocol::types::Branch("x".to_string()),
            create_pr: ralphex_macos_runner::protocol::types::CreatePr::No,
            title: None,
        })
        .await
        .unwrap();

    assert_eq!(claimed.unwrap().run_id, RunId("FARM-12-1".to_string()));
    assert_eq!(opened.run_id, RunId("local-1".to_string()));
    assert_eq!(farm.requests_ending("/claim").len(), 2);
    assert_eq!(farm.requests_ending("/runs").len(), 2);
}

#[tokio::test]
async fn a_failed_token_request_sends_nothing_and_is_retried_like_a_transport_failure() {
    let farm = FakeFarm::start().await;
    farm.push_grant(refused_token());
    farm.push_grant(refused_token());
    let sleeper = Arc::new(TestSleeper::new());
    let client = farm_client(&farm, sleeper.clone());

    client
        .heartbeat(&run_id(), &HeartbeatRequest::native())
        .await
        .unwrap();

    assert_eq!(farm.requests_ending("/heartbeat").len(), 1);
    assert_eq!(farm.token_requests().len(), 3);
    assert_eq!(
        sleeper.slept(),
        vec![Duration::from_secs(1), Duration::from_secs(2)]
    );
}

#[tokio::test]
async fn a_token_that_never_comes_exhausts_the_calls_attempts_without_reaching_the_farm() {
    let farm = FakeFarm::start().await;
    for _ in 0..RETRY_MAX_ATTEMPTS {
        farm.push_grant(refused_token());
    }
    let client = farm_client(&farm, Arc::new(TestSleeper::new()));

    let error = client
        .append_log(&run_id(), Seq::FIRST, b"line\n")
        .await
        .unwrap_err();

    let FarmError::Transport(message) = error else {
        panic!("a failed token request is a transport failure: {error:?}");
    };
    assert_eq!(
        message,
        "no access token: the token endpoint answered 503 (temporarily_unavailable)"
    );
    assert!(farm.requests().is_empty(), "a request reached the farm");
    assert_eq!(
        farm.token_requests().len(),
        usize::try_from(RETRY_MAX_ATTEMPTS).unwrap()
    );
}

#[tokio::test]
async fn a_claim_whose_token_fails_is_a_transport_failure_and_is_not_retried() {
    let farm = FakeFarm::start().await;
    farm.push_grant(refused_token());
    let sleeper = Arc::new(TestSleeper::new());
    let client = farm_client(&farm, sleeper.clone());

    let error = client
        .claim(&ralphex_macos_runner::protocol::types::ClaimRequest::native())
        .await
        .unwrap_err();

    let FarmError::Transport(_) = error else {
        panic!("a failed token request is a transport failure: {error:?}");
    };
    assert!(farm.requests().is_empty());
    assert!(sleeper.slept().is_empty());
}

#[tokio::test]
async fn a_completion_whose_token_fails_keeps_retrying_within_its_budget() {
    let farm = FakeFarm::start().await;
    farm.push_grant(refused_token());
    let sleeper = Arc::new(TestSleeper::new());
    let client = farm_client(&farm, sleeper.clone());

    client.complete(&run_id(), &completion()).await.unwrap();

    assert_eq!(farm.requests_ending("/complete").len(), 1);
    assert_eq!(sleeper.slept(), vec![Duration::from_secs(1)]);
}

#[tokio::test]
async fn a_lease_another_client_holds_is_gone_for_every_run_route() {
    let farm = FakeFarm::start().await;
    farm.push_claim(job_reply("FARM-12-1"));
    let holder = farm_client(&farm, Arc::new(TestSleeper::new()));
    let other: FarmClient =
        farm_client_as(&farm, "farm-runner-other", Arc::new(TestSleeper::new()));
    let Some(job) = holder
        .claim(&ralphex_macos_runner::protocol::types::ClaimRequest::native())
        .await
        .unwrap()
    else {
        panic!("the claim handed out no job");
    };
    let run_id = job.run_id;

    let beat = other.heartbeat(&run_id, &HeartbeatRequest::native()).await;
    let log = other.append_log(&run_id, Seq::FIRST, b"line\n").await;
    let progress = other
        .post_progress(
            &run_id,
            &ralphex_macos_runner::protocol::types::ProgressRequest {
                phase: ralphex_macos_runner::protocol::types::Phase::Setup,
                failed: false,
                tasks: None,
            },
        )
        .await;
    let complete = other.complete(&run_id, &completion()).await;

    assert_eq!(beat, Err(FarmError::Gone));
    assert_eq!(log, Err(FarmError::Gone));
    assert_eq!(progress, Err(FarmError::Gone));
    assert_eq!(complete, Err(FarmError::Gone));
    assert_eq!(
        holder.heartbeat(&run_id, &HeartbeatRequest::native()).await,
        Ok(HeartbeatResponse {
            action: HeartbeatAction::None
        })
    );
    assert_eq!(holder.complete(&run_id, &completion()).await, Ok(()));
}

#[tokio::test]
async fn a_farm_request_never_reaches_the_token_endpoint_path() {
    let farm = FakeFarm::start().await;
    let client = farm_client(&farm, Arc::new(TestSleeper::new()));

    client
        .heartbeat(&run_id(), &HeartbeatRequest::native())
        .await
        .unwrap();

    for request in farm.requests() {
        assert_ne!(request.path, TOKEN_PATH);
        assert!(
            request.path.starts_with("/external/runner/"),
            "{}",
            request.path
        );
    }
}
