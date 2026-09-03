//! The claim loop, the single run slot and the life of one run.
//!
//! [`Agent::run`] long-polls the farm from the only slot this runner has, hands
//! a claimed job to [`Agent::run_job`] and starts polling again when the run is
//! finalized. [`Agent::start_local`] takes the same slot for an `rxd`
//! invocation, waiting through a poll that is already in flight rather than
//! aborting it. A run's terminal conditions - a cancel, a lease the farm forgot,
//! a protocol mismatch, a shutdown that outlasted its drain - all arrive on one
//! channel, so every one of them is handled in a single place, next to the
//! process exit it competes with.

use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, mpsc, watch};

use crate::config::Config;
use crate::ipc::RunRequest;
use crate::job::{self, JobError, JobSpec, LocalOptions, Review};
use crate::logstream::{IntervalTicker, LogStream, Ticker};
use crate::pr::{PrSpec, PrTools, PrUrl, RunOrigin, open_pull_request};
use crate::protocol::client::{FarmClient, FarmError};
use crate::protocol::types::{
    ClaimRequest, CompleteRequest, CompleteStatus, CreatePr, HEARTBEAT_INTERVAL, HeartbeatAction,
    HeartbeatRequest, HeartbeatResponse, Job, OpenRunRequest, RETRY_BASE_DELAY, RUNTIME, RunId,
    RunnerName, STOP_GRACE,
};

const TERMINAL_CAPACITY: usize = 8;

/// What the agent is doing with its only run slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunSlot {
    /// Nothing holds the slot.
    Free,
    /// A claim long-poll holds the slot until it returns.
    Polling,
    /// A local request holds the slot while the farm mints its run.
    Opening,
    /// A run holds the slot.
    Running(RunId),
}

/// How far the run in the slot has got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunState {
    /// The run is in flight.
    Running,
    /// The run finished, with the completion it reported when it reported one.
    Finished(Option<CompleteRequest>),
}

/// The run an attached client follows.
pub struct CurrentRun {
    run_id: RunId,
    dashboard_url: String,
    log: Arc<LogStream>,
    state: watch::Sender<RunState>,
}

impl CurrentRun {
    /// Returns the identifier the farm gave this run.
    #[must_use]
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// Returns the dashboard page of this run.
    #[must_use]
    pub fn dashboard_url(&self) -> &str {
        &self.dashboard_url
    }

    /// Returns the log stream this run writes to.
    #[must_use]
    pub fn log(&self) -> Arc<LogStream> {
        Arc::clone(&self.log)
    }

    /// Returns the replay history and a receiver of the lines that follow it.
    #[must_use]
    pub fn subscribe(&self) -> (Vec<String>, broadcast::Receiver<String>) {
        self.log.subscribe()
    }

    /// Waits for the run to finish and returns the completion it reported.
    ///
    /// A run the farm has forgotten is finished without a completion and
    /// resolves to [`None`].
    pub async fn ended(&self) -> Option<CompleteRequest> {
        let mut state = self.state.subscribe();
        let finished = state
            .wait_for(|state| match state {
                RunState::Running => false,
                RunState::Finished(_) => true,
            })
            .await;
        let Ok(finished) = finished else {
            return None;
        };
        match finished.clone() {
            RunState::Running => None,
            RunState::Finished(completion) => completion,
        }
    }
}

/// What a local request got when it asked for the run slot.
pub enum LocalStart {
    /// The run is in flight and can be followed.
    Started(Arc<CurrentRun>),
    /// Another run holds the slot.
    Busy {
        /// The identifier of the run that holds the slot.
        run_id: RunId,
    },
    /// The farm refused to open the run.
    Refused {
        /// What the farm answered.
        message: String,
    },
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
    slot: watch::Sender<RunSlot>,
    permits: Arc<Semaphore>,
    current: Mutex<Option<Arc<CurrentRun>>>,
}

impl Agent {
    /// Returns an agent that claims for `config` through `client`.
    #[must_use]
    pub fn new(config: Config, client: Arc<FarmClient>, options: AgentOptions) -> Self {
        let (slot, _) = watch::channel(RunSlot::Free);
        Agent {
            config,
            client,
            options,
            slot,
            permits: Arc::new(Semaphore::new(1)),
            current: Mutex::new(None),
        }
    }

    /// Returns what the run slot currently holds.
    #[must_use]
    pub fn slot(&self) -> RunSlot {
        let slot = self.slot.borrow();
        slot.clone()
    }

    /// Returns the run an attaching client would follow.
    ///
    /// # Panics
    ///
    /// Panics when another holder of the run lock panicked.
    #[must_use]
    pub fn current(&self) -> Option<Arc<CurrentRun>> {
        let current = self.current.lock().unwrap();
        current.clone()
    }

    /// Claims and runs jobs until `shutdown` is raised or the protocol drifts.
    ///
    /// An in-flight claim is never aborted: a job the farm dispatched in the
    /// instant before an abort would be lost on the wire and finalized as a lost
    /// run three minutes later. A claim that comes back empty hands the slot to
    /// a local request that is already waiting for it, because the queue behind
    /// the slot serves its waiters in order.
    pub async fn run(&self, shutdown: watch::Receiver<bool>) -> AgentExit {
        let request = ClaimRequest::native(self.config.name.clone());
        let mut shutdown = shutdown;
        loop {
            if *shutdown.borrow() {
                self.hold(RunSlot::Free);
                return AgentExit::Shutdown;
            }
            let permit = tokio::select! {
                permit = Arc::clone(&self.permits).acquire_owned() => permit,
                _raised = raised(&mut shutdown) => {
                    self.hold(RunSlot::Free);
                    return AgentExit::Shutdown;
                }
            };
            let Ok(permit) = permit else {
                self.hold(RunSlot::Free);
                return AgentExit::Shutdown;
            };
            self.hold(RunSlot::Polling);
            let claimed = self.client.claim(&request).await;
            let job = match claimed {
                Ok(Some(job)) => job,
                Ok(None) => {
                    self.hold(RunSlot::Free);
                    drop(permit);
                    continue;
                }
                Err(error) => {
                    self.hold(RunSlot::Free);
                    drop(permit);
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
            drop(permit);
            match outcome {
                RunOutcome::Continue => {}
                RunOutcome::VersionMismatch { message } => {
                    return AgentExit::VersionMismatch { message };
                }
            }
        }
    }

    /// Opens a ticketless run for `request` and starts it in the run slot.
    ///
    /// A request that arrives while a claim is in flight waits for that claim to
    /// return; a request that arrives while a run holds the slot is answered
    /// [`LocalStart::Busy`] at once. The run outlives the client that asked for
    /// it.
    ///
    /// # Panics
    ///
    /// Panics when called outside a tokio runtime.
    pub async fn start_local(
        self: &Arc<Self>,
        request: RunRequest,
        shutdown: watch::Receiver<bool>,
    ) -> LocalStart {
        let permit = match self.hold_slot().await {
            Ok(permit) => permit,
            Err(run_id) => return LocalStart::Busy { run_id },
        };
        self.hold(RunSlot::Opening);
        let RunRequest {
            ctx,
            plan,
            branch,
            create_pr,
            worktree,
            env,
        } = request;
        let opened = OpenRunRequest {
            runner: self.config.name.clone(),
            runtime: RUNTIME.to_string(),
            repo: repo_name(Path::new(&ctx)),
            ctx,
            plan,
            branch,
            create_pr,
        };
        let job = match self.client.open_run(&opened).await {
            Ok(job) => job,
            Err(error) => {
                self.hold(RunSlot::Free);
                drop(permit);
                return LocalStart::Refused {
                    message: error.to_string(),
                };
            }
        };
        self.hold(RunSlot::Running(job.run_id.clone()));
        let current = self.enter(&job.run_id);
        let started = Arc::clone(&current);
        let agent = Arc::clone(self);
        let local = LocalOptions { worktree, env };
        tokio::spawn(async move {
            let outcome = agent.serve_run(job, local, shutdown, &started).await;
            agent.hold(RunSlot::Free);
            drop(permit);
            match outcome {
                RunOutcome::Continue => {}
                RunOutcome::VersionMismatch { message } => {
                    tracing::error!("the farm speaks another protocol version: {message}");
                }
            }
        });
        LocalStart::Started(current)
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
        let current = self.enter(&job.run_id);
        self.serve_run(job, local, shutdown, &current).await
    }

    async fn serve_run(
        &self,
        job: Job,
        local: LocalOptions,
        shutdown: watch::Receiver<bool>,
        current: &CurrentRun,
    ) -> RunOutcome {
        let run_id = job.run_id.clone();
        let (completion, outcome) = self.execute(job, local, shutdown, current).await;
        if let Some(completion) = completion.clone() {
            self.report(&run_id, completion).await;
        }
        self.leave(current, completion);
        outcome
    }

    async fn execute(
        &self,
        job: Job,
        local: LocalOptions,
        shutdown: watch::Receiver<bool>,
        current: &CurrentRun,
    ) -> (Option<CompleteRequest>, RunOutcome) {
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
            return (
                Some(failed(
                    "runtime_mismatch",
                    format!("this runner serves {RUNTIME}, the job asks for {runtime}"),
                    String::new(),
                )),
                RunOutcome::Continue,
            );
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
            return (
                Some(failed(
                    error.fail_reason(),
                    error.to_string(),
                    String::new(),
                )),
                RunOutcome::Continue,
            );
        }

        let log = current.log();
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
                return (
                    Some(failed(error.fail_reason(), error.to_string(), log.tail())),
                    RunOutcome::Continue,
                );
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
            Ended::Exited(Err(error)) => (
                Some(failed(error.fail_reason(), error.to_string(), tail)),
                RunOutcome::Continue,
            ),
            Ended::Exited(Ok(status)) => {
                if !status.success() {
                    return (
                        Some(failed("nonzero_exit", exit_message(status), tail)),
                        RunOutcome::Continue,
                    );
                }
                match create_pr {
                    CreatePr::No => (Some(done(String::new())), RunOutcome::Continue),
                    CreatePr::Yes => {
                        let origin = origin(identifier, issue_url, title);
                        let spec = PrSpec::describe(branch, &origin, &plan_path, &run_id);
                        let opened =
                            open_pull_request(&PathBuf::from(&ctx), &spec, &self.options.pr_tools)
                                .await;
                        match opened {
                            Ok(PrUrl(url)) => (Some(done(url)), RunOutcome::Continue),
                            Err(error) => (
                                Some(failed(error.fail_reason(), error.to_string(), tail)),
                                RunOutcome::Continue,
                            ),
                        }
                    }
                }
            }
            Ended::Terminal(Terminal::Cancel) => (
                Some(failed("canceled", "the run was canceled".to_string(), tail)),
                RunOutcome::Continue,
            ),
            Ended::Terminal(Terminal::Drain) => (
                Some(failed(
                    "runner_shutdown",
                    "the runner shut down while the run was in flight".to_string(),
                    tail,
                )),
                RunOutcome::Continue,
            ),
            Ended::Terminal(Terminal::Gone) => {
                tracing::warn!("run {run_id} is unknown to the farm and was stopped");
                (None, RunOutcome::Continue)
            }
            Ended::Terminal(Terminal::VersionMismatch { message }) => {
                (None, RunOutcome::VersionMismatch { message })
            }
        }
    }

    fn hold(&self, state: RunSlot) {
        self.slot.send_replace(state);
    }

    async fn hold_slot(&self) -> Result<OwnedSemaphorePermit, RunId> {
        if let Ok(permit) = Arc::clone(&self.permits).try_acquire_owned() {
            return Ok(permit);
        }
        let mut slot = self.slot.subscribe();
        if let RunSlot::Running(run_id) = self.slot() {
            return Err(run_id);
        }
        tokio::select! {
            permit = Arc::clone(&self.permits).acquire_owned() => match permit {
                Ok(permit) => Ok(permit),
                Err(_closed) => Err(RunId(String::new())),
            },
            run_id = running(&mut slot) => Err(run_id),
        }
    }

    fn enter(&self, run_id: &RunId) -> Arc<CurrentRun> {
        let log = Arc::new(LogStream::new(
            Arc::clone(&self.client),
            run_id.clone(),
            Arc::clone(&self.options.ticker),
        ));
        let (state, _) = watch::channel(RunState::Running);
        let current = Arc::new(CurrentRun {
            run_id: run_id.clone(),
            dashboard_url: dashboard_url(&self.config.farm_url, run_id),
            log,
            state,
        });
        let mut held = self.current.lock().unwrap();
        *held = Some(Arc::clone(&current));
        drop(held);
        current
    }

    fn leave(&self, current: &CurrentRun, completion: Option<CompleteRequest>) {
        current.state.send_replace(RunState::Finished(completion));
        let mut held = self.current.lock().unwrap();
        *held = None;
        drop(held);
    }

    async fn report(&self, run_id: &RunId, request: CompleteRequest) {
        match self.client.complete(run_id, &request).await {
            Ok(()) => {}
            Err(error) => tracing::warn!("run {run_id} could not be completed: {error}"),
        }
    }
}

/// Returns the dashboard page of the run `run_id` at the farm `farm_url`.
///
/// # Examples
///
/// ```
/// use ralphex_macos_runner::agent::dashboard_url;
/// use ralphex_macos_runner::protocol::types::RunId;
///
/// assert_eq!(
///     dashboard_url("http://farm.example:7077/", &RunId("local-1".to_string())),
///     "http://farm.example:7077/#/run/local-1"
/// );
/// ```
#[must_use]
pub fn dashboard_url(farm_url: &str, run_id: &RunId) -> String {
    format!("{}/#/run/{run_id}", farm_url.trim_end_matches('/'))
}

/// Returns the repository name the dashboard shows for the checkout `ctx`.
///
/// # Examples
///
/// ```
/// use std::path::Path;
///
/// use ralphex_macos_runner::agent::repo_name;
///
/// assert_eq!(repo_name(Path::new("/abs/Projects/ralphex-farm/")), "ralphex-farm");
/// assert_eq!(repo_name(Path::new("/")), "/");
/// ```
#[must_use]
pub fn repo_name(ctx: &Path) -> String {
    let Some(name) = ctx.file_name() else {
        return ctx.display().to_string();
    };
    name.to_string_lossy().into_owned()
}

async fn running(slot: &mut watch::Receiver<RunSlot>) -> RunId {
    loop {
        let held = slot.borrow_and_update().clone();
        match held {
            RunSlot::Running(run_id) => return run_id,
            RunSlot::Free => {}
            RunSlot::Polling => {}
            RunSlot::Opening => {}
        }
        let Ok(()) = slot.changed().await else {
            return RunId(String::new());
        };
    }
}

enum Raised {
    Shutdown,
    Detached,
}

async fn raised(shutdown: &mut watch::Receiver<bool>) -> Raised {
    loop {
        if *shutdown.borrow() {
            return Raised::Shutdown;
        }
        let Ok(()) = shutdown.changed().await else {
            return Raised::Detached;
        };
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
    match raised(&mut shutdown).await {
        Raised::Shutdown => {}
        Raised::Detached => return,
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
