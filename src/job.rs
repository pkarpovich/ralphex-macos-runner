//! Spawning ralphex in a checkout and stopping the process group it leads.
//!
//! A [`JobSpec`] names everything the run needs; [`validate`] rejects a checkout
//! or a plan the lifecycle table refuses before anything is spawned. [`spawn`]
//! starts ralphex as the leader of its own process group and pumps both of its
//! pipes into a [`LogStream`] one assembled line at a time, capped at
//! [`MAX_LOG_CHUNK`]: the two pipes share the stream, and handing it raw reads
//! let a chunk of stderr land inside a line of stdout in the farm's copy while
//! the history, the tail and the attached clients still held that line whole.
//! [`RunningJob`] waits for the exit or takes the group down.

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

use crate::logstream::{LogStream, Terminator};
use crate::protocol::types::{Branch, MAX_LOG_CHUNK, VALIDATE_TIMEOUT};

/// Whether ralphex runs in review mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Review {
    /// The run passes `--review`.
    Yes,
    /// The run is an ordinary implementation run.
    No,
}

impl Review {
    /// Returns the review mode a job's `mode` field asks for.
    ///
    /// # Examples
    ///
    /// ```
    /// use ralphex_macos_runner::job::Review;
    ///
    /// assert_eq!(Review::from_mode("review"), Review::Yes);
    /// assert_eq!(Review::from_mode(""), Review::No);
    /// ```
    #[must_use]
    pub fn from_mode(mode: &str) -> Review {
        if mode == "review" {
            Review::Yes
        } else {
            Review::No
        }
    }
}

/// Whether ralphex works in a git worktree of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Worktree {
    /// The run passes `--worktree`.
    Yes,
    /// The run works in the checkout itself.
    No,
}

/// What only a local `rxd` run can ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalOptions {
    /// Whether ralphex works in a worktree.
    pub worktree: Worktree,
    /// Environment entries added to the daemon's own.
    pub env: Vec<(String, String)>,
}

impl Default for LocalOptions {
    fn default() -> Self {
        LocalOptions {
            worktree: Worktree::No,
            env: Vec::new(),
        }
    }
}

/// Everything one ralphex invocation needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSpec {
    /// The checkout the run executes in.
    pub ctx: PathBuf,
    /// The plan file ralphex works through.
    pub plan: PathBuf,
    /// The branch ralphex works on.
    pub branch: Branch,
    /// Whether the run is a review run.
    pub review: Review,
    /// What the local client asked for.
    pub local: LocalOptions,
    /// The ralphex binary, resolved through the daemon's `PATH`.
    pub ralphex_bin: String,
}

/// Why a run could not start or could not be waited for.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JobError {
    /// The checkout does not exist, is not a directory or is not a git checkout.
    #[error("the checkout is unusable: {0}")]
    CtxInvalid(String),
    /// The plan file does not exist or lies outside the checkout.
    #[error("the plan is unusable: {0}")]
    PlanNotFound(String),
    /// ralphex could not be started.
    #[error("ralphex could not be started: {0}")]
    SpawnFailed(String),
    /// The started process could not be waited for.
    #[error("the run could not be waited for: {0}")]
    Wait(String),
}

impl JobError {
    /// Returns the farm's machine-readable name for this failure.
    ///
    /// # Examples
    ///
    /// ```
    /// use ralphex_macos_runner::job::JobError;
    ///
    /// assert_eq!(
    ///     JobError::PlanNotFound("gone".to_string()).fail_reason(),
    ///     "plan_not_found"
    /// );
    /// ```
    #[must_use]
    pub fn fail_reason(&self) -> &'static str {
        match self {
            JobError::CtxInvalid(_) => "ctx_invalid",
            JobError::PlanNotFound(_) => "plan_not_found",
            JobError::SpawnFailed(_) => "spawn_failed",
            JobError::Wait(_) => "spawn_failed",
        }
    }
}

trait Files: Send + Sync + 'static {
    fn checkout(&self, ctx: &Path) -> Result<PathBuf, JobError>;

    fn plan(&self, ctx: &Path, plan: &Path) -> Result<PathBuf, JobError>;
}

struct HostFiles;

impl Files for HostFiles {
    fn checkout(&self, ctx: &Path) -> Result<PathBuf, JobError> {
        let Ok(resolved) = ctx.canonicalize() else {
            return Err(JobError::CtxInvalid(format!(
                "{} does not exist",
                ctx.display()
            )));
        };
        if !resolved.is_dir() {
            return Err(JobError::CtxInvalid(format!(
                "{} is not a directory",
                resolved.display()
            )));
        }
        Ok(resolved)
    }

    fn plan(&self, ctx: &Path, plan: &Path) -> Result<PathBuf, JobError> {
        let Ok(resolved) = plan.canonicalize() else {
            return Err(JobError::PlanNotFound(format!(
                "{} does not exist",
                plan.display()
            )));
        };
        if !resolved.starts_with(ctx) {
            return Err(JobError::PlanNotFound(format!(
                "{} is outside {}",
                resolved.display(),
                ctx.display()
            )));
        }
        Ok(resolved)
    }
}

/// Checks that the checkout is a git checkout and the plan lies inside it.
///
/// [`VALIDATE_TIMEOUT`] is the budget for the whole inspection, the git child
/// and every filesystem call alike, and the filesystem calls run on a blocking
/// thread: the inspection holds the run slot before the terminal channel exists,
/// and a `canonicalize` on an unresponsive mount would otherwise wedge a runtime
/// worker forever, taking the slot and the shutdown behind it with no timeout
/// able to reach it.
///
/// # Errors
///
/// Returns [`JobError::CtxInvalid`] when `ctx` does not resolve to a directory
/// in which `git rev-parse --git-dir` succeeds, or when the inspection outlives
/// [`VALIDATE_TIMEOUT`], and [`JobError::PlanNotFound`] when the plan does not
/// resolve to a path under `ctx`.
pub async fn validate(spec: &JobSpec) -> Result<(), JobError> {
    inspect(spec, Arc::new(HostFiles), VALIDATE_TIMEOUT).await
}

async fn inspect(spec: &JobSpec, files: Arc<dyn Files>, budget: Duration) -> Result<(), JobError> {
    let JobSpec {
        ctx,
        plan,
        branch: _,
        review: _,
        local: _,
        ralphex_bin: _,
    } = spec;
    let deadline = tokio::time::Instant::now() + budget;

    let asked = ctx.clone();
    let probe = Arc::clone(&files);
    let resolved = tokio::task::spawn_blocking(move || probe.checkout(&asked));
    let ctx = match tokio::time::timeout_at(deadline, resolved).await {
        Err(_elapsed) => return Err(unanswered(ctx, budget)),
        Ok(Err(error)) => {
            return Err(JobError::CtxInvalid(format!(
                "{} could not be inspected: {error}",
                ctx.display()
            )));
        }
        Ok(Ok(resolved)) => resolved?,
    };

    let mut command = Command::new("git");
    command.arg("rev-parse");
    command.arg("--git-dir");
    command.current_dir(&ctx);
    command.stdin(Stdio::null());
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());
    command.kill_on_drop(true);
    let inspected = tokio::time::timeout_at(deadline, command.status()).await;
    let Ok(inspected) = inspected else {
        return Err(JobError::CtxInvalid(format!(
            "git was killed after {budget:?} in {}",
            ctx.display()
        )));
    };
    let Ok(inspected) = inspected else {
        return Err(JobError::CtxInvalid(format!(
            "git could not be run in {}",
            ctx.display()
        )));
    };
    if !inspected.success() {
        return Err(JobError::CtxInvalid(format!(
            "{} is not a git checkout",
            ctx.display()
        )));
    }

    let asked = plan.clone();
    let root = ctx.clone();
    let probe = Arc::clone(&files);
    let resolved = tokio::task::spawn_blocking(move || probe.plan(&root, &asked));
    match tokio::time::timeout_at(deadline, resolved).await {
        Err(_elapsed) => Err(unanswered(plan, budget)),
        Ok(Err(error)) => Err(JobError::PlanNotFound(format!(
            "{} could not be inspected: {error}",
            plan.display()
        ))),
        Ok(Ok(resolved)) => resolved.map(|_plan| ()),
    }
}

fn unanswered(path: &Path, budget: Duration) -> JobError {
    JobError::CtxInvalid(format!(
        "{} did not answer within {budget:?}",
        path.display()
    ))
}

/// Starts ralphex for `spec` and tees its output into `log`.
///
/// The child leads its own process group, reads nothing from stdin and has both
/// of its pipes drained by tasks of their own. A [`RunningJob`] dropped without
/// being stopped kills the leader, so no path out of the agent can leave ralphex
/// running in a checkout the next job is about to take.
///
/// # Errors
///
/// Returns [`JobError::SpawnFailed`] when the binary cannot be started or the
/// started child reports no process id.
///
/// # Panics
///
/// Panics when called outside a tokio runtime.
pub fn spawn(spec: &JobSpec, log: Arc<LogStream>) -> Result<RunningJob, JobError> {
    let JobSpec {
        ctx,
        plan,
        branch,
        review,
        local,
        ralphex_bin,
    } = spec;
    let LocalOptions { worktree, env } = local;
    let mut command = Command::new(ralphex_bin);
    command.arg("--branch").arg(branch.as_str());
    match worktree {
        Worktree::Yes => {
            command.arg("--worktree");
        }
        Worktree::No => {}
    }
    match review {
        Review::Yes => {
            command.arg("--review");
        }
        Review::No => {}
    }
    command.arg(plan);
    command.current_dir(ctx);
    for (key, value) in env {
        command.env(key, value);
    }
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.process_group(0);
    command.kill_on_drop(true);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return Err(JobError::SpawnFailed(format!("{ralphex_bin}: {error}"))),
    };
    let Some(pid) = child.id() else {
        return Err(JobError::SpawnFailed(
            "the child reported no process id".to_string(),
        ));
    };
    let Ok(pid) = i32::try_from(pid) else {
        return Err(JobError::SpawnFailed(format!(
            "the child reported an unusable process id {pid}"
        )));
    };

    let mut readers = Vec::with_capacity(2);
    let stdout = child.stdout.take();
    if let Some(stdout) = stdout {
        readers.push(tokio::spawn(pump(stdout, Arc::clone(&log))));
    }
    let stderr = child.stderr.take();
    if let Some(stderr) = stderr {
        readers.push(tokio::spawn(pump(stderr, Arc::clone(&log))));
    }

    Ok(RunningJob {
        child,
        pgid: Pid::from_raw(pid),
        readers,
    })
}

/// A ralphex process and the tasks draining its pipes.
pub struct RunningJob {
    child: Child,
    pgid: Pid,
    readers: Vec<JoinHandle<()>>,
}

impl RunningJob {
    /// Returns the identifier of the process group the run leads.
    #[must_use]
    pub fn pgid(&self) -> i32 {
        self.pgid.as_raw()
    }

    /// Waits for the run to exit.
    ///
    /// The pipes are emptied separately by [`RunningJob::drain_output`]: a
    /// helper that reparented out of the process group holds them open after
    /// the leader is gone, and draining here would let a terminal event that
    /// arrives in that window discard a status the run already produced.
    ///
    /// # Errors
    ///
    /// Returns [`JobError::Wait`] when the child cannot be waited for.
    pub async fn wait(&mut self) -> Result<ExitStatus, JobError> {
        match self.child.wait().await {
            Ok(exited) => Ok(exited),
            Err(error) => Err(JobError::Wait(error.to_string())),
        }
    }

    /// Signals the process group, waits out `grace` and kills what is left.
    ///
    /// The `SIGKILL` follows whether or not the leader honoured the `SIGTERM`,
    /// because the members of its group are what the next run has to be safe
    /// from: `claude` and `xcodebuild` ignore a `SIGTERM`, and a leader that
    /// exits on its own within the grace would otherwise leave them editing the
    /// checkout while the next job is spawned into it.
    ///
    /// # Errors
    ///
    /// Returns [`JobError::Wait`] when the child cannot be waited for.
    pub async fn stop(&mut self, grace: Duration) -> Result<ExitStatus, JobError> {
        let _ = killpg(self.pgid, Signal::SIGTERM);
        let exited = tokio::time::timeout(grace, self.child.wait()).await;
        let _ = killpg(self.pgid, Signal::SIGKILL);
        let exited = match exited {
            Ok(exited) => exited,
            Err(_elapsed) => self.child.wait().await,
        };
        let exited = match exited {
            Ok(exited) => exited,
            Err(error) => return Err(JobError::Wait(error.to_string())),
        };
        Ok(exited)
    }

    /// Waits out `budget` for the tasks draining the run's pipes to finish.
    ///
    /// The budget covers the whole drain rather than each pipe in turn, and a
    /// task still reading when it runs out is aborted: a helper that reparented
    /// out of the process group holds the pipe open for as long as it lives, so
    /// a task merely dropped here would outlive the run forever, holding the
    /// log stream the flusher has already stopped serving.
    pub async fn drain_output(&mut self, budget: Duration) {
        let readers = std::mem::take(&mut self.readers);
        let deadline = tokio::time::Instant::now() + budget;
        for mut reader in readers {
            let drained = tokio::time::timeout_at(deadline, &mut reader).await;
            match drained {
                Ok(_joined) => {}
                Err(_elapsed) => reader.abort(),
            }
        }
    }
}

async fn pump<R>(mut pipe: R, log: Arc<LogStream>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut buffer = vec![0u8; MAX_LOG_CHUNK];
    let mut lines = LineAssembler::new();
    loop {
        let read = pipe.read(&mut buffer).await;
        let Ok(read) = read else {
            break;
        };
        if read == 0 {
            break;
        }
        lines.feed(&buffer[..read], &log);
    }
    lines.finish(&log);
}

struct LineAssembler {
    pending: Vec<u8>,
    cut: bool,
}

impl LineAssembler {
    fn new() -> Self {
        LineAssembler {
            pending: Vec::new(),
            cut: false,
        }
    }

    fn feed(&mut self, bytes: &[u8], log: &LogStream) {
        for byte in bytes {
            if *byte == b'\n' {
                if self.cut {
                    self.cut = false;
                    log.push_break();
                    continue;
                }
                self.emit(log, Terminator::Newline);
                continue;
            }
            self.cut = false;
            self.pending.push(*byte);
            if self.pending.len() >= MAX_LOG_CHUNK {
                self.force(log);
            }
        }
    }

    fn force(&mut self, log: &LogStream) {
        let boundary = floor_boundary(&self.pending);
        let carried = self.pending.split_off(boundary);
        let piece = std::mem::replace(&mut self.pending, carried);
        log.push_line(&piece, Terminator::Cut);
        self.cut = self.pending.is_empty();
    }

    fn emit(&mut self, log: &LogStream, terminator: Terminator) {
        let line = std::mem::take(&mut self.pending);
        log.push_line(&line, terminator);
    }

    fn finish(&mut self, log: &LogStream) {
        if self.pending.is_empty() {
            return;
        }
        self.emit(log, Terminator::Cut);
    }
}

fn floor_boundary(bytes: &[u8]) -> usize {
    let Err(error) = std::str::from_utf8(bytes) else {
        return bytes.len();
    };
    match error.error_len() {
        Some(_invalid) => bytes.len(),
        None => error.valid_up_to(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    fn spec() -> JobSpec {
        JobSpec {
            ctx: PathBuf::from("/tmp"),
            plan: PathBuf::from("/tmp/plan.md"),
            branch: Branch("x".to_string()),
            review: Review::No,
            local: LocalOptions::default(),
            ralphex_bin: "ralphex".to_string(),
        }
    }

    #[test]
    fn local_options_default_to_no_worktree_and_no_extra_environment() {
        let LocalOptions { worktree, env } = LocalOptions::default();
        assert_eq!(worktree, Worktree::No);
        assert!(env.is_empty());
    }

    #[test]
    fn a_review_mode_becomes_a_review_run() {
        assert_eq!(Review::from_mode("review"), Review::Yes);
        assert_eq!(Review::from_mode("implement"), Review::No);
        assert_eq!(Review::from_mode(""), Review::No);
    }

    #[test]
    fn every_failure_carries_the_farms_name_for_it() {
        assert_eq!(
            JobError::CtxInvalid(String::new()).fail_reason(),
            "ctx_invalid"
        );
        assert_eq!(
            JobError::PlanNotFound(String::new()).fail_reason(),
            "plan_not_found"
        );
        assert_eq!(
            JobError::SpawnFailed(String::new()).fail_reason(),
            "spawn_failed"
        );
        assert_eq!(JobError::Wait(String::new()).fail_reason(), "spawn_failed");
    }

    fn detached_log() -> LogStream {
        let client = Arc::new(
            crate::protocol::client::FarmClient::new(
                "http://127.0.0.1:1",
                "t",
                Arc::new(crate::protocol::client::TokioSleeper),
            )
            .unwrap(),
        );
        LogStream::new(
            client,
            crate::protocol::types::RunId("local-1".to_string()),
            Arc::new(crate::logstream::IntervalTicker),
        )
    }

    #[tokio::test]
    async fn a_line_cut_at_the_cap_keeps_its_characters_whole() {
        let log = detached_log();
        let mut printed = vec![b'a'; MAX_LOG_CHUNK - 1];
        printed.extend("\u{20ac}".as_bytes());
        printed.push(b'\n');

        let mut lines = LineAssembler::new();
        lines.feed(&printed, &log);
        lines.finish(&log);

        let (replay, _live) = log.subscribe();
        assert_eq!(replay.len(), 2);
        assert_eq!(replay[0].len(), MAX_LOG_CHUNK - 1);
        assert_eq!(replay[1], "\u{20ac}");
        for line in &replay {
            assert!(
                !line.contains('\u{fffd}'),
                "a character was cut in half at the chunk cap"
            );
        }
    }

    #[tokio::test]
    async fn a_newline_right_after_a_forced_cut_adds_no_empty_line() {
        let log = detached_log();
        let mut printed = vec![b'a'; MAX_LOG_CHUNK];
        printed.push(b'\n');
        printed.extend(b"after\n");

        let mut lines = LineAssembler::new();
        lines.feed(&printed, &log);
        lines.finish(&log);

        let (replay, _live) = log.subscribe();
        assert_eq!(replay.len(), 2);
        assert_eq!(replay[0].len(), MAX_LOG_CHUNK);
        assert_eq!(replay[1], "after");
    }

    #[test]
    fn a_boundary_falls_before_a_character_the_cap_would_split() {
        assert_eq!(floor_boundary(b"abc"), 3);
        assert_eq!(floor_boundary("a\u{20ac}".as_bytes()), 4);
        assert_eq!(floor_boundary(&[b'a', 0xe2, 0x82]), 1);
        assert_eq!(floor_boundary(&[b'a', 0xe2]), 1);
        assert_eq!(floor_boundary(&[0xff, 0xfe]), 2);
    }

    #[tokio::test]
    async fn a_missing_checkout_is_refused() {
        let mut spec = spec();
        spec.ctx = PathBuf::from("/tmp/ralphex-macos-runner-does-not-exist");
        let error = validate(&spec).await.unwrap_err();
        assert_eq!(error.fail_reason(), "ctx_invalid");
    }

    struct StalledFiles {
        released: Arc<AtomicBool>,
    }

    impl StalledFiles {
        fn wait(&self) {
            while !self.released.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    impl Files for StalledFiles {
        fn checkout(&self, ctx: &Path) -> Result<PathBuf, JobError> {
            self.wait();
            Ok(ctx.to_path_buf())
        }

        fn plan(&self, _ctx: &Path, plan: &Path) -> Result<PathBuf, JobError> {
            self.wait();
            Ok(plan.to_path_buf())
        }
    }

    #[tokio::test]
    async fn a_checkout_whose_filesystem_never_answers_is_refused_at_the_deadline() {
        let released = Arc::new(AtomicBool::new(false));
        let files = Arc::new(StalledFiles {
            released: Arc::clone(&released),
        });
        let budget = Duration::from_millis(50);

        let error = inspect(&spec(), files, budget).await.unwrap_err();
        released.store(true, Ordering::SeqCst);

        assert_eq!(error.fail_reason(), "ctx_invalid");
        let JobError::CtxInvalid(message) = error else {
            panic!("a filesystem that does not answer is an invalid context");
        };
        assert!(message.contains("did not answer"), "{message}");
        assert!(message.contains("50ms"), "{message}");
    }
}
