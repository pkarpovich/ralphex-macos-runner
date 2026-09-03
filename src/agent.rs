//! The claim loop, the single run slot and the life of one run.
//!
//! [`Agent::run`] long-polls the farm from the only slot this runner has, hands
//! a claimed job to [`Agent::run_job`] and starts polling again when the run is
//! finalized. A run's terminal conditions - a cancel, a lease the farm forgot, a
//! protocol mismatch, a shutdown that outlasted its drain - all arrive on one
//! channel, so every one of them is handled in a single place, next to the
//! process exit it competes with.

use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use crate::config::Config;
use crate::job::{self, JobError, JobSpec, LocalOptions, Review};
use crate::logstream::{IntervalTicker, LogStream, Ticker};
use crate::pr::{PrSpec, PrTools, PrUrl, RunOrigin, open_pull_request};
use crate::protocol::client::{FarmClient, FarmError};
use crate::protocol::types::{
    ClaimRequest, CompleteRequest, CompleteStatus, CreatePr, HEARTBEAT_INTERVAL, HeartbeatAction,
    HeartbeatRequest, HeartbeatResponse, Job, RETRY_BASE_DELAY, RUNTIME, RunId, RunnerName,
    STOP_GRACE,
};

const TERMINAL_CAPACITY: usize = 8;

/// What the agent is doing with its only run slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunSlot {
    /// Nothing holds the slot.
    Free,
    /// A claim long-poll holds the slot until it returns.
    Polling,
    /// A run holds the slot.
    Running(RunId),
}

/// How the agent's loop ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentExit {
    /// A shutdown was asked for and any running job was finalized.
    Shutdown,
    /// The farm speaks a different protocol version.
    VersionMismatch {
        /// The farm's message, naming both versions.
        message: String,
    },
}

/// What a finished run leaves the agent to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// The agent keeps claiming.
    Continue,
    /// The agent stops, because the farm speaks a different protocol version.
    VersionMismatch {
        /// The farm's message, naming both versions.
        message: String,
    },
}

/// The intervals and the tools the agent runs with.
pub struct AgentOptions {
    /// The interval between two heartbeats of a running job.
    pub heartbeat_interval: Duration,
    /// The time a shutdown lets a running job finish.
    pub drain_timeout: Duration,
    /// The grace between the `SIGTERM` and the `SIGKILL` of a stopped run.
    pub stop_grace: Duration,
    /// The delay before the claim loop polls again after a failed claim.
    pub claim_retry_delay: Duration,
    /// The flush cadence of a run's log stream.
    pub ticker: Arc<dyn Ticker>,
    /// The programs the pull-request sequence runs.
    pub pr_tools: PrTools,
}

impl Default for AgentOptions {
    fn default() -> Self {
        AgentOptions {
            heartbeat_interval: HEARTBEAT_INTERVAL,
            drain_timeout: crate::config::DEFAULT_DRAIN_TIMEOUT,
            stop_grace: STOP_GRACE,
            claim_retry_delay: RETRY_BASE_DELAY,
            ticker: Arc::new(IntervalTicker),
            pr_tools: PrTools::default(),
        }
    }
}

enum Terminal {
    Cancel,
    Drain,
    Gone,
    VersionMismatch { message: String },
}

enum Ended {
    Exited(Result<ExitStatus, JobError>),
    Terminal(Terminal),
}

/// The runner's claim loop and its single run slot.
pub struct Agent {
    config: Config,
    client: Arc<FarmClient>,
    options: AgentOptions,
    slot: Mutex<RunSlot>,
}

impl Agent {
    /// Returns an agent that claims for `config` through `client`.
    #[must_use]
    pub fn new(config: Config, client: Arc<FarmClient>, options: AgentOptions) -> Self {
        Agent {
            config,
            client,
            options,
            slot: Mutex::new(RunSlot::Free),
        }
    }

    /// Returns what the run slot currently holds.
    ///
    /// # Panics
    ///
    /// Panics when another holder of the slot lock panicked.
    #[must_use]
    pub fn slot(&self) -> RunSlot {
        let slot = self.slot.lock().unwrap();
        slot.clone()
    }

    /// Claims and runs jobs until `shutdown` is raised or the protocol drifts.
    ///
    /// An in-flight claim is never aborted: a job the farm dispatched in the
    /// instant before an abort would be lost on the wire and finalized as a lost
    /// run three minutes later.
    ///
    /// # Panics
    ///
    /// Panics when another holder of the slot lock panicked.
    pub async fn run(&self, shutdown: watch::Receiver<bool>) -> AgentExit {
        let request = ClaimRequest::native(self.config.name.clone());
        loop {
            if *shutdown.borrow() {
                self.hold(RunSlot::Free);
                return AgentExit::Shutdown;
            }
            self.hold(RunSlot::Polling);
            let claimed = self.client.claim(&request).await;
            let job = match claimed {
                Ok(Some(job)) => job,
                Ok(None) => {
                    self.hold(RunSlot::Free);
                    continue;
                }
                Err(error) => {
                    self.hold(RunSlot::Free);
                    let Some(message) = version_mismatch(&error) else {
                        tracing::warn!("the claim failed: {error}");
                        tokio::time::sleep(self.options.claim_retry_delay).await;
                        continue;
                    };
                    return AgentExit::VersionMismatch { message };
                }
            };
            self.hold(RunSlot::Running(job.run_id.clone()));
            let outcome = self
                .run_job(job, LocalOptions::default(), shutdown.clone())
                .await;
            self.hold(RunSlot::Free);
            match outcome {
                RunOutcome::Continue => {}
                RunOutcome::VersionMismatch { message } => {
                    return AgentExit::VersionMismatch { message };
                }
            }
        }
    }

    /// Runs `job` to its completion and reports it to the farm.
    ///
    /// `local` carries what only an `rxd` invocation can ask for; a claimed job
    /// passes [`LocalOptions::default`].
    ///
    /// # Panics
    ///
    /// Panics when called outside a tokio runtime.
    pub async fn run_job(
        &self,
        job: Job,
        local: LocalOptions,
        shutdown: watch::Receiver<bool>,
    ) -> RunOutcome {
        let Job {
            run_id,
            issue_id: _,
            identifier,
            issue_url,
            title,
            repo_slug: _,
            plan_path,
            branch,
            mode,
            lease_ttl_seconds: _,
            runtime,
            ctx,
            create_pr,
        } = job;

        if runtime != RUNTIME {
            self.report(
                &run_id,
                failed(
                    "runtime_mismatch",
                    format!("this runner serves {RUNTIME}, the job asks for {runtime}"),
                    String::new(),
                ),
            )
            .await;
            return RunOutcome::Continue;
        }

        let spec = JobSpec {
            ctx: PathBuf::from(&ctx),
            plan: PathBuf::from(&plan_path),
            branch: branch.clone(),
            review: Review::from_mode(&mode),
            local,
            ralphex_bin: self.config.ralphex_bin.clone(),
        };
        if let Err(error) = job::validate(&spec).await {
            self.report(
                &run_id,
                failed(error.fail_reason(), error.to_string(), String::new()),
            )
            .await;
            return RunOutcome::Continue;
        }

        let log = Arc::new(LogStream::new(
            Arc::clone(&self.client),
            run_id.clone(),
            Arc::clone(&self.options.ticker),
        ));
        let (terminals, mut terminal) = mpsc::channel(TERMINAL_CAPACITY);
        let beats = tokio::spawn(beat(
            Arc::clone(&self.client),
            run_id.clone(),
            self.config.name.clone(),
            self.options.heartbeat_interval,
            terminals.clone(),
        ));
        let drain = tokio::spawn(drain_after(
            shutdown,
            self.options.drain_timeout,
            terminals.clone(),
        ));

        let mut running = match job::spawn(&spec, Arc::clone(&log)) {
            Ok(running) => running,
            Err(error) => {
                beats.abort();
                drain.abort();
                log.close().await;
                self.report(
                    &run_id,
                    failed(error.fail_reason(), error.to_string(), log.tail()),
                )
                .await;
                return RunOutcome::Continue;
            }
        };
        tracing::info!("run {run_id} started in {ctx}");

        let ended = tokio::select! {
            exited = running.wait() => Ended::Exited(exited),
            Some(terminal) = terminal.recv() => Ended::Terminal(terminal),
        };

        match &ended {
            Ended::Exited(_) => {}
            Ended::Terminal(_) => {
                if let Err(error) = running.stop(self.options.stop_grace).await {
                    tracing::warn!("run {run_id} could not be stopped: {error}");
                }
            }
        }
        beats.abort();
        drain.abort();
        log.close().await;
        let tail = log.tail();

        match ended {
            Ended::Exited(Err(error)) => {
                self.report(
                    &run_id,
                    failed(error.fail_reason(), error.to_string(), tail),
                )
                .await;
                RunOutcome::Continue
            }
            Ended::Exited(Ok(status)) => {
                if !status.success() {
                    self.report(&run_id, failed("nonzero_exit", exit_message(status), tail))
                        .await;
                    return RunOutcome::Continue;
                }
                match create_pr {
                    CreatePr::No => {
                        self.report(&run_id, done(String::new())).await;
                    }
                    CreatePr::Yes => {
                        let origin = origin(identifier, issue_url, title);
                        let spec = PrSpec::describe(branch, &origin, &plan_path, &run_id);
                        match open_pull_request(&PathBuf::from(&ctx), &spec, &self.options.pr_tools)
                            .await
                        {
                            Ok(PrUrl(url)) => self.report(&run_id, done(url)).await,
                            Err(error) => {
                                self.report(
                                    &run_id,
                                    failed(error.fail_reason(), error.to_string(), tail),
                                )
                                .await;
                            }
                        }
                    }
                }
                RunOutcome::Continue
            }
            Ended::Terminal(Terminal::Cancel) => {
                self.report(
                    &run_id,
                    failed("canceled", "the run was canceled".to_string(), tail),
                )
                .await;
                RunOutcome::Continue
            }
            Ended::Terminal(Terminal::Drain) => {
                self.report(
                    &run_id,
                    failed(
                        "runner_shutdown",
                        "the runner shut down while the run was in flight".to_string(),
                        tail,
                    ),
                )
                .await;
                RunOutcome::Continue
            }
            Ended::Terminal(Terminal::Gone) => {
                tracing::warn!("run {run_id} is unknown to the farm and was stopped");
                RunOutcome::Continue
            }
            Ended::Terminal(Terminal::VersionMismatch { message }) => {
                RunOutcome::VersionMismatch { message }
            }
        }
    }

    fn hold(&self, state: RunSlot) {
        let mut slot = self.slot.lock().unwrap();
        *slot = state;
    }

    async fn report(&self, run_id: &RunId, request: CompleteRequest) {
        match self.client.complete(run_id, &request).await {
            Ok(()) => {}
            Err(error) => tracing::warn!("run {run_id} could not be completed: {error}"),
        }
    }
}

fn origin(identifier: String, issue_url: String, title: String) -> RunOrigin {
    if identifier.is_empty() {
        return RunOrigin::Local;
    }
    RunOrigin::Ticket {
        identifier,
        issue_url,
        title,
    }
}

fn exit_message(status: ExitStatus) -> String {
    let Some(code) = status.code() else {
        return "ralphex was killed by a signal".to_string();
    };
    format!("ralphex exited with code {code}")
}

fn done(pr_url: String) -> CompleteRequest {
    CompleteRequest {
        status: CompleteStatus::Done,
        pr_url,
        fail_reason: String::new(),
        message: String::new(),
        log_tail: String::new(),
    }
}

fn failed(fail_reason: &str, message: String, log_tail: String) -> CompleteRequest {
    CompleteRequest {
        status: CompleteStatus::Error,
        pr_url: String::new(),
        fail_reason: fail_reason.to_string(),
        message,
        log_tail,
    }
}

fn version_mismatch(error: &FarmError) -> Option<String> {
    match error {
        FarmError::VersionMismatch { message } => Some(message.clone()),
        FarmError::Gone => None,
        FarmError::BadRequest(_) => None,
        FarmError::Rejected(_, _) => None,
        FarmError::Transport(_) => None,
        FarmError::Decode(_) => None,
    }
}

async fn beat(
    client: Arc<FarmClient>,
    run_id: RunId,
    runner: RunnerName,
    interval: Duration,
    terminals: mpsc::Sender<Terminal>,
) {
    let request = HeartbeatRequest::native(runner);
    loop {
        match client.heartbeat(&run_id, &request).await {
            Ok(HeartbeatResponse { action }) => match action {
                HeartbeatAction::None => {}
                HeartbeatAction::Cancel => {
                    let _ = terminals.send(Terminal::Cancel).await;
                    return;
                }
            },
            Err(FarmError::Gone) => {
                let _ = terminals.send(Terminal::Gone).await;
                return;
            }
            Err(FarmError::VersionMismatch { message }) => {
                let _ = terminals.send(Terminal::VersionMismatch { message }).await;
                return;
            }
            Err(FarmError::BadRequest(message)) => {
                tracing::warn!("the heartbeat of run {run_id} was refused: {message}");
            }
            Err(FarmError::Rejected(status, body)) => {
                tracing::warn!("the heartbeat of run {run_id} answered {status}: {body}");
            }
            Err(FarmError::Transport(message)) => {
                tracing::warn!("the heartbeat of run {run_id} did not reach the farm: {message}");
            }
            Err(FarmError::Decode(message)) => {
                tracing::warn!("the heartbeat of run {run_id} could not be read: {message}");
            }
        }
        tokio::time::sleep(interval).await;
    }
}

async fn drain_after(
    shutdown: watch::Receiver<bool>,
    timeout: Duration,
    terminals: mpsc::Sender<Terminal>,
) {
    let mut shutdown = shutdown;
    loop {
        if *shutdown.borrow() {
            break;
        }
        let Ok(()) = shutdown.changed().await else {
            return;
        };
    }
    tokio::time::sleep(timeout).await;
    let _ = terminals.send(Terminal::Drain).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(code: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
    }

    #[test]
    fn a_nonzero_exit_names_its_code() {
        assert_eq!(exit_message(status(3)), "ralphex exited with code 3");
    }

    #[test]
    fn a_done_completion_carries_nothing_but_its_url() {
        let CompleteRequest {
            status,
            pr_url,
            fail_reason,
            message,
            log_tail,
        } = done("https://github.com/owner/repo/pull/7".to_string());
        assert_eq!(status, CompleteStatus::Done);
        assert_eq!(pr_url, "https://github.com/owner/repo/pull/7");
        assert!(fail_reason.is_empty());
        assert!(message.is_empty());
        assert!(log_tail.is_empty());
    }

    #[test]
    fn a_failed_completion_carries_its_reason_and_tail() {
        let CompleteRequest {
            status,
            pr_url,
            fail_reason,
            message,
            log_tail,
        } = failed("canceled", "stopped".to_string(), "line".to_string());
        assert_eq!(status, CompleteStatus::Error);
        assert!(pr_url.is_empty());
        assert_eq!(fail_reason, "canceled");
        assert_eq!(message, "stopped");
        assert_eq!(log_tail, "line");
    }

    #[test]
    fn only_a_mismatch_is_fatal_to_the_claim_loop() {
        assert_eq!(
            version_mismatch(&FarmError::VersionMismatch {
                message: "1 against 2".to_string()
            }),
            Some("1 against 2".to_string())
        );
        assert_eq!(version_mismatch(&FarmError::Gone), None);
        assert_eq!(
            version_mismatch(&FarmError::Transport("reset".to_string())),
            None
        );
        assert_eq!(
            version_mismatch(&FarmError::Rejected(500, String::new())),
            None
        );
        assert_eq!(
            version_mismatch(&FarmError::BadRequest(String::new())),
            None
        );
        assert_eq!(version_mismatch(&FarmError::Decode(String::new())), None);
    }

    #[test]
    fn a_job_without_an_identifier_is_a_local_run() {
        assert_eq!(
            origin(String::new(), String::new(), String::new()),
            RunOrigin::Local
        );
        assert_eq!(
            origin(
                "FARM-12".to_string(),
                "https://linear.app/x".to_string(),
                "split".to_string()
            ),
            RunOrigin::Ticket {
                identifier: "FARM-12".to_string(),
                issue_url: "https://linear.app/x".to_string(),
                title: "split".to_string(),
            }
        );
    }

    #[test]
    fn the_default_options_are_the_protocol_constants() {
        let AgentOptions {
            heartbeat_interval,
            drain_timeout,
            stop_grace,
            claim_retry_delay,
            ticker: _,
            pr_tools,
        } = AgentOptions::default();
        assert_eq!(heartbeat_interval, HEARTBEAT_INTERVAL);
        assert_eq!(drain_timeout, crate::config::DEFAULT_DRAIN_TIMEOUT);
        assert_eq!(stop_grace, STOP_GRACE);
        assert_eq!(claim_retry_delay, RETRY_BASE_DELAY);
        assert_eq!(pr_tools, PrTools::default());
    }

    #[test]
    fn a_fresh_agent_holds_nothing() {
        let config = Config {
            farm_url: "http://farm.example".to_string(),
            token: "t".to_string(),
            name: RunnerName("mbp-native".to_string()),
            drain_timeout: Duration::from_secs(1),
            ralphex_bin: "ralphex".to_string(),
        };
        let client = Arc::new(
            FarmClient::new(
                &config.farm_url,
                &config.token,
                Arc::new(crate::protocol::client::TokioSleeper),
            )
            .unwrap(),
        );
        let agent = Agent::new(config, client, AgentOptions::default());
        assert_eq!(agent.slot(), RunSlot::Free);
        agent.hold(RunSlot::Running(RunId("local-1".to_string())));
        assert_eq!(agent.slot(), RunSlot::Running(RunId("local-1".to_string())));
    }
}
