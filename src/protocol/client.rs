//! The HTTP client that speaks the farm protocol.
//!
//! [`FarmClient`] owns the five calls a runner makes, the retry policy each one
//! carries and the mapping from an HTTP status to a [`FarmError`]. Delays and
//! the passage of time arrive through the [`Sleeper`] trait, so a test drives
//! the whole retry table without waiting on a clock.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::header::CONTENT_TYPE;

use crate::protocol::types::{
    CLAIM_WINDOW, COMPLETE_BUDGET, ClaimRequest, CompleteRequest, HeartbeatRequest,
    HeartbeatResponse, Job, OpenRunRequest, RETRY_BASE_DELAY, RETRY_MAX_ATTEMPTS, RETRY_MAX_DELAY,
    RunId, Seq,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

const CLAIM_TIMEOUT: Duration = Duration::from_secs(CLAIM_WINDOW.as_secs() + 30);

const MAX_BODY_BYTES: usize = 65536;

/// A source of delays and of the current instant.
///
/// The daemon uses [`TokioSleeper`]; a test supplies an implementation that
/// returns at once and advances a clock of its own.
pub trait Sleeper: Send + Sync {
    /// Returns a future that resolves once `duration` has passed.
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

    /// Returns the instant this sleeper considers current.
    fn now(&self) -> Instant;
}

/// The [`Sleeper`] the daemon runs with.
#[derive(Debug, Clone, Copy)]
pub struct TokioSleeper;

impl Sleeper for TokioSleeper {
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(tokio::time::sleep(duration))
    }

    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Why a call to the farm did not succeed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FarmError {
    /// The farm no longer knows the run.
    #[error("the farm no longer knows this run")]
    Gone,
    /// The farm speaks a different protocol version.
    #[error("protocol version mismatch: {message}")]
    VersionMismatch {
        /// The farm's message, naming both versions.
        message: String,
    },
    /// The farm refused the request as malformed.
    #[error("the farm refused the request: {0}")]
    BadRequest(String),
    /// The farm answered with a status the call does not accept.
    #[error("the farm answered {0}: {1}")]
    Rejected(u16, String),
    /// The farm could not be reached.
    #[error("the farm could not be reached: {0}")]
    Transport(String),
    /// The farm's answer was not the expected JSON.
    #[error("the farm's answer could not be decoded: {0}")]
    Decode(String),
}

enum RetryPolicy {
    Attempts(u32),
    Budget(Duration),
}

/// The runner's end of the farm protocol.
pub struct FarmClient {
    http: reqwest::Client,
    farm_url: String,
    token: String,
    sleeper: Arc<dyn Sleeper>,
}

impl FarmClient {
    /// Returns a client that talks to the farm at `farm_url` with `token`.
    ///
    /// # Errors
    ///
    /// Returns [`FarmError::Transport`] when the underlying HTTP client cannot
    /// be built.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// use ralphex_macos_runner::protocol::client::{FarmClient, TokioSleeper};
    ///
    /// let client = FarmClient::new("http://farm.example:7077/", "secret", Arc::new(TokioSleeper));
    /// assert!(client.is_ok());
    /// ```
    pub fn new(farm_url: &str, token: &str, sleeper: Arc<dyn Sleeper>) -> Result<Self, FarmError> {
        let http = match reqwest::Client::builder().build() {
            Ok(http) => http,
            Err(error) => return Err(FarmError::Transport(error.to_string())),
        };
        Ok(FarmClient {
            http,
            farm_url: farm_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            sleeper,
        })
    }

    /// Polls the farm for a job and returns [`None`] when the window expired.
    ///
    /// The call is never retried: the claim loop polls again on its own.
    ///
    /// # Errors
    ///
    /// Returns [`FarmError::VersionMismatch`] when the farm answers `409`,
    /// [`FarmError::Transport`] when it cannot be reached and
    /// [`FarmError::Decode`] when the job does not parse.
    pub async fn claim(&self, request: &ClaimRequest) -> Result<Option<Job>, FarmError> {
        let url = self.endpoint("api/runner/claim");
        let attempt = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .timeout(CLAIM_TIMEOUT)
            .json(request);
        let (status, body) = self.send(attempt).await?;
        let body = body.trim();
        if status == 204 || body.is_empty() {
            return Ok(None);
        }
        match serde_json::from_str::<Job>(body) {
            Ok(job) => Ok(Some(job)),
            Err(error) => Err(FarmError::Decode(error.to_string())),
        }
    }

    /// Opens a run that no Linear ticket asked for and returns its job.
    ///
    /// The call is never retried: it mints a run id and a lease on every
    /// attempt, so a retry after a lost answer would orphan a run.
    ///
    /// # Errors
    ///
    /// Returns [`FarmError::BadRequest`] when the farm refuses the request,
    /// [`FarmError::Rejected`] on any other refusal, [`FarmError::Transport`]
    /// when the farm cannot be reached and [`FarmError::Decode`] when the job
    /// does not parse.
    pub async fn open_run(&self, request: &OpenRunRequest) -> Result<Job, FarmError> {
        let url = self.endpoint("api/runner/runs");
        let attempt = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .timeout(REQUEST_TIMEOUT)
            .json(request);
        let (_status, body) = self.send(attempt).await?;
        match serde_json::from_str::<Job>(&body) {
            Ok(job) => Ok(job),
            Err(error) => Err(FarmError::Decode(error.to_string())),
        }
    }

    /// Sends one chunk of a run's output under the sequence number `seq`.
    ///
    /// Transport failures and `5xx` answers are retried with an exponential
    /// backoff for at most [`RETRY_MAX_ATTEMPTS`] attempts; any `4xx` is final.
    ///
    /// # Errors
    ///
    /// Returns [`FarmError::Gone`] when the farm has forgotten the run,
    /// [`FarmError::BadRequest`] when it rejects the sequence number and
    /// [`FarmError::Transport`] or [`FarmError::Rejected`] when the retries ran
    /// out.
    pub async fn append_log(
        &self,
        run_id: &RunId,
        seq: Seq,
        bytes: &[u8],
    ) -> Result<(), FarmError> {
        let url = self.endpoint(&format!(
            "api/runner/jobs/{}/log?seq={}",
            encode_segment(run_id.as_str()),
            seq.get()
        ));
        self.retrying(RetryPolicy::Attempts(RETRY_MAX_ATTEMPTS), || async {
            let attempt = self
                .http
                .post(&url)
                .bearer_auth(&self.token)
                .header(CONTENT_TYPE, "application/octet-stream")
                .timeout(REQUEST_TIMEOUT)
                .body(bytes.to_vec());
            self.send(attempt).await
        })
        .await?;
        Ok(())
    }

    /// Renews the lease of a running job and returns what the farm asks for.
    ///
    /// The retry policy is the one [`FarmClient::append_log`] uses.
    ///
    /// # Errors
    ///
    /// Returns [`FarmError::Gone`] when the farm has forgotten the run,
    /// [`FarmError::VersionMismatch`] when it answers `409` and
    /// [`FarmError::Decode`] when the answer does not parse.
    pub async fn heartbeat(
        &self,
        run_id: &RunId,
        request: &HeartbeatRequest,
    ) -> Result<HeartbeatResponse, FarmError> {
        let url = self.endpoint(&format!(
            "api/runner/jobs/{}/heartbeat",
            encode_segment(run_id.as_str())
        ));
        let (_status, body) = self
            .retrying(RetryPolicy::Attempts(RETRY_MAX_ATTEMPTS), || async {
                let attempt = self
                    .http
                    .post(&url)
                    .bearer_auth(&self.token)
                    .timeout(REQUEST_TIMEOUT)
                    .json(request);
                self.send(attempt).await
            })
            .await?;
        match serde_json::from_str::<HeartbeatResponse>(&body) {
            Ok(response) => Ok(response),
            Err(error) => Err(FarmError::Decode(error.to_string())),
        }
    }

    /// Finalizes a run at the farm.
    ///
    /// Transport failures and `5xx` answers are retried without an attempt
    /// limit until [`COMPLETE_BUDGET`] has elapsed since the first attempt,
    /// because giving up earlier lets the lease expire and the run be
    /// finalized as lost.
    ///
    /// # Errors
    ///
    /// Returns [`FarmError::Gone`] when the run was already completed and
    /// [`FarmError::Transport`] or [`FarmError::Rejected`] when the budget ran
    /// out.
    pub async fn complete(
        &self,
        run_id: &RunId,
        request: &CompleteRequest,
    ) -> Result<(), FarmError> {
        let url = self.endpoint(&format!(
            "api/runner/jobs/{}/complete",
            encode_segment(run_id.as_str())
        ));
        self.retrying(RetryPolicy::Budget(COMPLETE_BUDGET), || async {
            let attempt = self
                .http
                .post(&url)
                .bearer_auth(&self.token)
                .timeout(REQUEST_TIMEOUT)
                .json(request);
            self.send(attempt).await
        })
        .await?;
        Ok(())
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/{}", self.farm_url, path)
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<(u16, String), FarmError> {
        match request.send().await {
            Ok(response) => accept(response).await,
            Err(error) => Err(FarmError::Transport(error.to_string())),
        }
    }

    async fn retrying<T, F, Fut>(&self, policy: RetryPolicy, operation: F) -> Result<T, FarmError>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<T, FarmError>>,
    {
        let started = self.sleeper.now();
        let mut attempts = 0;
        let mut delay = RETRY_BASE_DELAY;
        loop {
            let error = match operation().await {
                Ok(value) => return Ok(value),
                Err(error) => error,
            };
            attempts += 1;
            if !is_retryable(&error) {
                return Err(error);
            }
            match policy {
                RetryPolicy::Attempts(limit) => {
                    if attempts >= limit {
                        return Err(error);
                    }
                }
                RetryPolicy::Budget(budget) => {
                    if self.sleeper.now().duration_since(started) >= budget {
                        return Err(error);
                    }
                }
            }
            self.sleeper.sleep(delay).await;
            delay = next_delay(delay);
        }
    }
}

fn next_delay(delay: Duration) -> Duration {
    let delay = delay * 2;
    if delay > RETRY_MAX_DELAY {
        return RETRY_MAX_DELAY;
    }
    delay
}

fn is_retryable(error: &FarmError) -> bool {
    match error {
        FarmError::Transport(_) => true,
        FarmError::Rejected(status, _) => *status >= 500,
        FarmError::Gone => false,
        FarmError::VersionMismatch { message: _ } => false,
        FarmError::BadRequest(_) => false,
        FarmError::Decode(_) => false,
    }
}

async fn accept(response: reqwest::Response) -> Result<(u16, String), FarmError> {
    let status = response.status().as_u16();
    let body = read_body(response).await?;
    if (200..300).contains(&status) {
        return Ok((status, body));
    }
    if status == 409 {
        return Err(FarmError::VersionMismatch {
            message: error_message(&body),
        });
    }
    if status == 410 {
        return Err(FarmError::Gone);
    }
    if status == 400 {
        return Err(FarmError::BadRequest(body));
    }
    Err(FarmError::Rejected(status, body))
}

async fn read_body(response: reqwest::Response) -> Result<String, FarmError> {
    let mut response = response;
    let mut collected: Vec<u8> = Vec::new();
    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => return Err(FarmError::Transport(error.to_string())),
        };
        let room = MAX_BODY_BYTES - collected.len();
        if chunk.len() >= room {
            collected.extend_from_slice(&chunk[..room]);
            break;
        }
        collected.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&collected).into_owned())
}

fn error_message(body: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return body.to_string();
    };
    let Some(field) = value.get("error") else {
        return body.to_string();
    };
    let Some(message) = field.as_str() else {
        return body.to_string();
    };
    message.to_string()
}

fn encode_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        let unreserved = byte.is_ascii_alphanumeric()
            || byte == b'-'
            || byte == b'.'
            || byte == b'_'
            || byte == b'~';
        if unreserved {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_ids_are_percent_encoded_in_a_path() {
        assert_eq!(
            encode_segment("FARM-12-1753180800000"),
            "FARM-12-1753180800000"
        );
        assert_eq!(encode_segment("local-1/../x"), "local-1%2F..%2Fx");
        assert_eq!(encode_segment("a b"), "a%20b");
    }

    #[test]
    fn a_version_mismatch_body_yields_its_error_field() {
        assert_eq!(
            error_message(r#"{"error":"runner speaks 1, farm speaks 2"}"#),
            "runner speaks 1, farm speaks 2"
        );
        assert_eq!(error_message("plain text"), "plain text");
        assert_eq!(error_message(r#"{"other":1}"#), r#"{"other":1}"#);
    }

    #[test]
    fn the_backoff_doubles_up_to_the_ceiling() {
        let mut delay = RETRY_BASE_DELAY;
        let mut seen = Vec::new();
        for _ in 0..7 {
            seen.push(delay);
            delay = next_delay(delay);
        }
        assert_eq!(
            seen,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(16),
                Duration::from_secs(30),
                Duration::from_secs(30),
            ]
        );
    }

    #[test]
    fn only_transport_failures_and_server_errors_are_retried() {
        assert!(is_retryable(&FarmError::Transport("reset".to_string())));
        assert!(is_retryable(&FarmError::Rejected(500, String::new())));
        assert!(is_retryable(&FarmError::Rejected(503, String::new())));
        assert!(!is_retryable(&FarmError::Rejected(404, String::new())));
        assert!(!is_retryable(&FarmError::BadRequest(String::new())));
        assert!(!is_retryable(&FarmError::Gone));
        assert!(!is_retryable(&FarmError::VersionMismatch {
            message: String::new()
        }));
        assert!(!is_retryable(&FarmError::Decode(String::new())));
    }

    #[test]
    fn a_trailing_slash_in_the_farm_url_is_dropped() {
        let client =
            FarmClient::new("http://farm.example:7077/", "t", Arc::new(TokioSleeper)).unwrap();
        assert_eq!(
            client.endpoint("api/runner/claim"),
            "http://farm.example:7077/api/runner/claim"
        );
    }
}
