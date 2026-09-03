//! Golden JSON round-trips for every body of the farm protocol.
//!
//! Each vector is transcribed verbatim from the plan's "Conformance vectors"
//! section, deserialized into this crate's type and re-serialized; the two JSON
//! values must be equal.

use ralphex_macos_runner::protocol::types::{
    ClaimRequest, CompleteRequest, HeartbeatRequest, HeartbeatResponse, Job, OpenRunRequest,
    RepoCapability,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

fn round_trip<T>(vector: &str)
where
    T: DeserializeOwned + Serialize,
{
    let expected: Value = serde_json::from_str(vector).unwrap();
    let decoded: T = serde_json::from_str(vector).unwrap();
    let encoded = serde_json::to_value(&decoded).unwrap();
    assert_eq!(encoded, expected);
}

#[test]
fn repo_capability() {
    round_trip::<RepoCapability>(r#"{"slug":"pkarpovich/ralphex-farm","default_branch":"master"}"#);
}

#[test]
fn claim_request_full() {
    round_trip::<ClaimRequest>(
        r#"{"runner":"mac-1","version":"1","runtime":"native","repos":[{"slug":"owner/one","default_branch":"main"},{"slug":"owner/two","default_branch":"master"}],"ready":["owner/two"],"slots":2}"#,
    );
}

#[test]
fn claim_request_empty() {
    round_trip::<ClaimRequest>(
        r#"{"runner":"","version":"","runtime":"","repos":null,"ready":null,"slots":0}"#,
    );
}

#[test]
fn job_full() {
    round_trip::<Job>(
        r#"{"run_id":"FARM-12-1753180800000","issue_id":"issue-uuid","identifier":"FARM-12","issue_url":"https://linear.app/example/issue/FARM-12","title":"split farm and runner","repo_slug":"owner/repo","plan_path":"/abs/checkout/docs/plans/20260722-farm-runner-architecture.md","branch":"farm-runner-architecture","mode":"review","lease_ttl_seconds":180,"runtime":"native","ctx":"/abs/checkout","create_pr":true}"#,
    );
}

#[test]
fn job_empty() {
    round_trip::<Job>(
        r#"{"run_id":"","issue_id":"","identifier":"","issue_url":"","title":"","repo_slug":"","plan_path":"","branch":"","mode":"","lease_ttl_seconds":0,"runtime":"","ctx":"","create_pr":false}"#,
    );
}

#[test]
fn heartbeat_request_full() {
    round_trip::<HeartbeatRequest>(
        r#"{"runner":"mac-1","image":"ghcr.io/pkarpovich/ralphex:latest","version":"1","runtime":"native","repos":[{"slug":"owner/one","default_branch":"main"}],"slots":2}"#,
    );
}

#[test]
fn heartbeat_request_empty() {
    round_trip::<HeartbeatRequest>(
        r#"{"runner":"","image":"","version":"","runtime":"","repos":null,"slots":0}"#,
    );
}

#[test]
fn heartbeat_response_cancel() {
    round_trip::<HeartbeatResponse>(r#"{"action":"cancel"}"#);
}

#[test]
fn heartbeat_response_none() {
    round_trip::<HeartbeatResponse>(r#"{"action":"none"}"#);
}

#[test]
fn open_run_request_full() {
    round_trip::<OpenRunRequest>(
        r#"{"runner":"mbp","version":"1","runtime":"native","repo":"ralphex-farm","ctx":"/abs/checkout","plan":"/abs/checkout/docs/plans/x.md","branch":"x","create_pr":true}"#,
    );
}

#[test]
fn open_run_request_empty() {
    round_trip::<OpenRunRequest>(
        r#"{"runner":"","version":"","runtime":"","repo":"","ctx":"","plan":"","branch":"","create_pr":false}"#,
    );
}

#[test]
fn complete_request_done() {
    round_trip::<CompleteRequest>(
        r#"{"status":"done","pr_url":"https://github.com/owner/repo/pull/7","fail_reason":"","message":"","log_tail":""}"#,
    );
}

#[test]
fn complete_request_error() {
    round_trip::<CompleteRequest>(
        r#"{"status":"error","pr_url":"","fail_reason":"nonzero_exit","message":"ralphex exited with code 2","log_tail":"line one\nline two"}"#,
    );
}
