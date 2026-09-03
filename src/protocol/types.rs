//! The wire types, the JSON encoding and the tuning constants of the farm protocol.
//!
//! Every struct in this module is a body the daemon sends to the farm or a body
//! the farm sends back. The JSON keys are the farm's; `repos` and `ready` are
//! optional because the farm's goldens encode an absent slice as `null`, while
//! this crate always sends an empty array.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The protocol version this runner speaks.
pub const VERSION: &str = "1";

/// The runtime class this runner serves.
pub const RUNTIME: &str = "native";

/// The number of jobs this runner takes at once.
pub const SLOTS: u32 = 1;

/// The lease the farm grants a claimed job.
pub const LEASE_TTL: Duration = Duration::from_secs(180);

/// The interval between two heartbeats of a running job.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// The window the farm holds a claim open for.
pub const CLAIM_WINDOW: Duration = Duration::from_secs(25);

/// The interval between two flushes of the outgoing log buffer.
pub const LOG_FLUSH_INTERVAL: Duration = Duration::from_secs(2);

/// The largest log chunk the farm accepts in one request.
pub const MAX_LOG_CHUNK: usize = 65536;

/// The number of trailing lines kept for a completion's log tail.
pub const LOG_TAIL_LINES: usize = 100;

/// The byte ceiling of a completion's log tail.
pub const LOG_TAIL_BYTES: usize = 65536;

/// The byte ceiling of the outgoing log buffer before the oldest bytes are dropped.
pub const LOG_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// The number of lines an attaching client replays.
pub const HISTORY_LINES: usize = 2000;

/// The byte ceiling of the replay history.
pub const HISTORY_BYTES: usize = 4 * 1024 * 1024;

/// The delay before the first retry of a failed call.
pub const RETRY_BASE_DELAY: Duration = Duration::from_secs(1);

/// The ceiling the retry delay doubles up to.
pub const RETRY_MAX_DELAY: Duration = Duration::from_secs(30);

/// The number of attempts a retried call makes before it gives up.
pub const RETRY_MAX_ATTEMPTS: u32 = 6;

/// The time a single request to the farm may take, the claim poll aside.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The time a completion keeps retrying before the lease would expire.
pub const COMPLETE_BUDGET: Duration = LEASE_TTL;

/// The budget a log stream has to flush its remainder while closing.
pub const LOG_CLOSE_TIMEOUT: Duration = Duration::from_secs(30);

/// The grace between the `SIGTERM` and the `SIGKILL` of a stopped process group.
pub const STOP_GRACE: Duration = Duration::from_secs(10);

/// The identifier the farm gives a run.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(pub String);

impl RunId {
    /// Returns the identifier as a string slice.
    ///
    /// # Examples
    ///
    /// ```
    /// use ralphex_macos_runner::protocol::types::RunId;
    ///
    /// assert_eq!(RunId("local-1".to_string()).as_str(), "local-1");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The name this runner registers under.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunnerName(pub String);

impl RunnerName {
    /// Returns the name as a string slice.
    ///
    /// # Examples
    ///
    /// ```
    /// use ralphex_macos_runner::protocol::types::RunnerName;
    ///
    /// assert_eq!(RunnerName("mbp-native".to_string()).as_str(), "mbp-native");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RunnerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The git branch a run works on.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Branch(pub String);

impl Branch {
    /// Returns the branch name as a string slice.
    ///
    /// # Examples
    ///
    /// ```
    /// use ralphex_macos_runner::protocol::types::Branch;
    ///
    /// assert_eq!(Branch("farm-runner".to_string()).as_str(), "farm-runner");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Branch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The 1-based sequence number of a log chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Seq(pub u64);

impl Seq {
    /// The sequence number of a run's first log chunk.
    pub const FIRST: Seq = Seq(1);

    /// Returns the sequence number that follows this one.
    ///
    /// # Examples
    ///
    /// ```
    /// use ralphex_macos_runner::protocol::types::Seq;
    ///
    /// assert_eq!(Seq::FIRST.increment(), Seq(2));
    /// ```
    #[must_use]
    pub fn increment(self) -> Seq {
        let Seq(value) = self;
        Seq(value + 1)
    }

    /// Returns the sequence number as an integer.
    ///
    /// # Examples
    ///
    /// ```
    /// use ralphex_macos_runner::protocol::types::Seq;
    ///
    /// assert_eq!(Seq::FIRST.get(), 1);
    /// ```
    #[must_use]
    pub fn get(self) -> u64 {
        let Seq(value) = self;
        value
    }
}

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Seq(value) = self;
        write!(f, "{value}")
    }
}

/// Whether a run ends with a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "bool", into = "bool")]
pub enum CreatePr {
    /// The runner pushes the branch and opens a pull request.
    Yes,
    /// The runner leaves the branch alone.
    No,
}

impl From<bool> for CreatePr {
    fn from(value: bool) -> Self {
        if value { CreatePr::Yes } else { CreatePr::No }
    }
}

impl From<CreatePr> for bool {
    fn from(value: CreatePr) -> Self {
        match value {
            CreatePr::Yes => true,
            CreatePr::No => false,
        }
    }
}

/// What the farm asks a heartbeating runner to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HeartbeatAction {
    /// The run continues.
    None,
    /// The run is canceled and its process group is stopped.
    Cancel,
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompleteStatus {
    /// ralphex finished and, when asked, a pull request exists.
    Done,
    /// The run failed, was canceled or was cut short by a shutdown.
    Error,
}

/// A repository this runner can serve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoCapability {
    /// The `owner/name` slug of the repository.
    pub slug: String,
    /// The repository's default branch.
    pub default_branch: String,
}

/// The body of a claim long-poll.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimRequest {
    /// The name this runner registers under.
    pub runner: RunnerName,
    /// The protocol version the runner speaks.
    pub version: String,
    /// The runtime class the runner serves.
    pub runtime: String,
    /// The repositories the runner can serve.
    #[serde(default)]
    pub repos: Option<Vec<RepoCapability>>,
    /// The slugs the runner is ready to take a job for.
    #[serde(default)]
    pub ready: Option<Vec<String>>,
    /// The number of jobs the runner takes at once.
    pub slots: u32,
}

impl ClaimRequest {
    /// Returns the claim a native runner named `runner` sends when it is idle.
    ///
    /// # Examples
    ///
    /// ```
    /// use ralphex_macos_runner::protocol::types::{ClaimRequest, RunnerName};
    ///
    /// let request = ClaimRequest::native(RunnerName("mbp-native".to_string()));
    /// let encoded = serde_json::to_string(&request).unwrap();
    /// assert!(encoded.contains(r#""repos":[]"#));
    /// assert!(encoded.contains(r#""runtime":"native""#));
    /// ```
    #[must_use]
    pub fn native(runner: RunnerName) -> Self {
        ClaimRequest {
            runner,
            version: VERSION.to_string(),
            runtime: RUNTIME.to_string(),
            repos: Some(Vec::new()),
            ready: Some(Vec::new()),
            slots: SLOTS,
        }
    }
}

/// The body of a heartbeat for a running job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    /// The name this runner registers under.
    pub runner: RunnerName,
    /// The container image the run uses, empty for a native run.
    pub image: String,
    /// The protocol version the runner speaks.
    pub version: String,
    /// The runtime class the runner serves.
    pub runtime: String,
    /// The repositories the runner can serve.
    #[serde(default)]
    pub repos: Option<Vec<RepoCapability>>,
    /// The number of jobs the runner takes at once.
    pub slots: u32,
}

impl HeartbeatRequest {
    /// Returns the heartbeat a native runner named `runner` sends for a running job.
    ///
    /// # Examples
    ///
    /// ```
    /// use ralphex_macos_runner::protocol::types::{HeartbeatRequest, RunnerName};
    ///
    /// let request = HeartbeatRequest::native(RunnerName("mbp-native".to_string()));
    /// let encoded = serde_json::to_string(&request).unwrap();
    /// assert!(encoded.contains(r#""image":"""#));
    /// assert!(encoded.contains(r#""slots":1"#));
    /// ```
    #[must_use]
    pub fn native(runner: RunnerName) -> Self {
        HeartbeatRequest {
            runner,
            image: String::new(),
            version: VERSION.to_string(),
            runtime: RUNTIME.to_string(),
            repos: Some(Vec::new()),
            slots: SLOTS,
        }
    }
}

/// The farm's answer to a heartbeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    /// What the farm asks the runner to do.
    pub action: HeartbeatAction,
}

/// The body that opens a run without a Linear ticket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenRunRequest {
    /// The name of the runner that will execute the run.
    pub runner: RunnerName,
    /// The runtime class the run needs.
    pub runtime: String,
    /// The repository name the dashboard shows.
    pub repo: String,
    /// The checkout the run executes in.
    pub ctx: String,
    /// The absolute path of the plan file.
    pub plan: String,
    /// The branch ralphex works on.
    pub branch: Branch,
    /// Whether the run ends with a pull request.
    pub create_pr: CreatePr,
}

/// A run the farm handed to this runner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    /// The identifier of the run.
    pub run_id: RunId,
    /// The Linear issue's identifier, empty for a local run.
    pub issue_id: String,
    /// The Linear issue's human identifier, empty for a local run.
    pub identifier: String,
    /// The Linear issue's URL, empty for a local run.
    pub issue_url: String,
    /// The title of the run.
    pub title: String,
    /// The `owner/name` slug of the repository.
    pub repo_slug: String,
    /// The absolute path of the plan file.
    pub plan_path: String,
    /// The branch ralphex works on.
    pub branch: Branch,
    /// The ralphex mode, `review` for a review run.
    pub mode: String,
    /// The lease the farm granted, in seconds.
    pub lease_ttl_seconds: u64,
    /// The runtime class the run needs.
    pub runtime: String,
    /// The checkout the run executes in.
    pub ctx: String,
    /// Whether the run ends with a pull request.
    pub create_pr: CreatePr,
}

/// The body that finalizes a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompleteRequest {
    /// How the run ended.
    pub status: CompleteStatus,
    /// The pull request the run opened, empty when it opened none.
    pub pr_url: String,
    /// The machine-readable reason a failed run failed, empty on success.
    pub fail_reason: String,
    /// The human-readable message shown on the dashboard.
    pub message: String,
    /// The trailing output of the run.
    pub log_tail: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_claim_sends_empty_slices() {
        let request = ClaimRequest::native(RunnerName("mbp-native".to_string()));
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(
            encoded,
            json!({
                "runner": "mbp-native",
                "version": "1",
                "runtime": "native",
                "repos": [],
                "ready": [],
                "slots": 1,
            })
        );
    }

    #[test]
    fn native_heartbeat_sends_empty_repos_and_no_image() {
        let request = HeartbeatRequest::native(RunnerName("mbp-native".to_string()));
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(
            encoded,
            json!({
                "runner": "mbp-native",
                "image": "",
                "version": "1",
                "runtime": "native",
                "repos": [],
                "slots": 1,
            })
        );
    }

    #[test]
    fn null_slices_decode_to_none() {
        let request: ClaimRequest = serde_json::from_value(json!({
            "runner": "mbp-native",
            "version": "1",
            "runtime": "native",
            "repos": null,
            "ready": null,
            "slots": 1,
        }))
        .unwrap();
        let ClaimRequest {
            runner: _,
            version: _,
            runtime: _,
            repos,
            ready,
            slots: _,
        } = request;
        assert_eq!(repos, None);
        assert_eq!(ready, None);
    }

    #[test]
    fn missing_slices_decode_to_none() {
        let request: ClaimRequest = serde_json::from_value(json!({
            "runner": "mbp-native",
            "version": "1",
            "runtime": "native",
            "slots": 1,
        }))
        .unwrap();
        let ClaimRequest {
            runner: _,
            version: _,
            runtime: _,
            repos,
            ready,
            slots: _,
        } = request;
        assert_eq!(repos, None);
        assert_eq!(ready, None);
    }

    #[test]
    fn empty_slices_decode_to_an_empty_vector() {
        let request: HeartbeatRequest = serde_json::from_value(json!({
            "runner": "mbp-native",
            "image": "",
            "version": "1",
            "runtime": "native",
            "repos": [],
            "slots": 1,
        }))
        .unwrap();
        let HeartbeatRequest {
            runner: _,
            image: _,
            version: _,
            runtime: _,
            repos,
            slots: _,
        } = request;
        assert_eq!(repos, Some(Vec::new()));
    }

    #[test]
    fn create_pr_encodes_as_a_boolean() {
        assert_eq!(serde_json::to_value(CreatePr::Yes).unwrap(), json!(true));
        assert_eq!(serde_json::to_value(CreatePr::No).unwrap(), json!(false));
        assert_eq!(
            serde_json::from_value::<CreatePr>(json!(true)).unwrap(),
            CreatePr::Yes
        );
        assert_eq!(
            serde_json::from_value::<CreatePr>(json!(false)).unwrap(),
            CreatePr::No
        );
    }

    #[test]
    fn heartbeat_actions_and_complete_statuses_are_lowercase() {
        assert_eq!(
            serde_json::to_value(HeartbeatAction::Cancel).unwrap(),
            json!("cancel")
        );
        assert_eq!(
            serde_json::to_value(HeartbeatAction::None).unwrap(),
            json!("none")
        );
        assert_eq!(
            serde_json::to_value(CompleteStatus::Done).unwrap(),
            json!("done")
        );
        assert_eq!(
            serde_json::to_value(CompleteStatus::Error).unwrap(),
            json!("error")
        );
    }

    #[test]
    fn sequence_numbers_start_at_one_and_advance() {
        assert_eq!(Seq::FIRST.get(), 1);
        assert_eq!(Seq::FIRST.increment().get(), 2);
        assert_eq!(Seq::FIRST.increment().to_string(), "2");
    }

    #[test]
    fn newtypes_encode_transparently() {
        assert_eq!(
            serde_json::to_value(RunId("local-1".to_string())).unwrap(),
            json!("local-1")
        );
        assert_eq!(
            serde_json::to_value(Branch("x".to_string())).unwrap(),
            json!("x")
        );
        assert_eq!(serde_json::to_value(Seq(7)).unwrap(), json!(7));
    }

    #[test]
    fn constants_match_the_wire_contract() {
        assert_eq!(VERSION, "1");
        assert_eq!(RUNTIME, "native");
        assert_eq!(SLOTS, 1);
        assert_eq!(COMPLETE_BUDGET, LEASE_TTL);
        assert_eq!(MAX_LOG_CHUNK, 65536);
        assert_eq!(LOG_BUFFER_BYTES, 4 * 1024 * 1024);
    }
}
