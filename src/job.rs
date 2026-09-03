//! Spawning ralphex in a checkout and stopping the process group it leads.
//!
//! A [`JobSpec`] names everything the run needs; [`validate`] rejects a checkout
//! or a plan the lifecycle table refuses before anything is spawned. [`spawn`]
//! starts ralphex as the leader of its own process group and pumps both of its
//! pipes into a [`LogStream`], as raw chunks for the farm and as capped lines
//! for the history and the attached clients. [`RunningJob`] waits for the exit
//! or takes the group down.

use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

use crate::logstream::LogStream;
use crate::protocol::types::{Branch, MAX_LOG_CHUNK, STOP_GRACE};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Checks that the checkout is a git checkout and the plan lies inside it.
///
/// # Errors
///
/// Returns [`JobError::CtxInvalid`] when `ctx` does not resolve to a directory
/// in which `git rev-parse --git-dir` succeeds, and
/// [`JobError::PlanNotFound`] when the plan does not resolve to a path under
/// `ctx`.
pub async fn validate(spec: &JobSpec) -> Result<(), JobError> {
    let JobSpec {
        ctx,
        plan,
        branch: _,
        review: _,
        local: _,
        ralphex_bin: _,
    } = spec;
    let Ok(ctx) = ctx.canonicalize() else {
        return Err(JobError::CtxInvalid(format!(
            "{} does not exist",
            ctx.display()
        )));
    };
    if !ctx.is_dir() {
        return Err(JobError::CtxInvalid(format!(
            "{} is not a directory",
            ctx.display()
        )));
    }
    let inspected = Command::new("git")
        .arg("rev-parse")
        .arg("--git-dir")
        .current_dir(&ctx)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
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
    let Ok(plan) = plan.canonicalize() else {
        return Err(JobError::PlanNotFound(format!(
            "{} does not exist",
            plan.display()
        )));
    };
    if !plan.starts_with(&ctx) {
        return Err(JobError::PlanNotFound(format!(
            "{} is outside {}",
            plan.display(),
            ctx.display()
        )));
    }
    Ok(())
}

/// Starts ralphex for `spec` and tees its output into `log`.
///
/// The child leads its own process group, reads nothing from stdin and has both
/// of its pipes drained by tasks of their own.
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

    /// Waits for the run to exit and for its output to be drained.
    ///
    /// # Errors
    ///
    /// Returns [`JobError::Wait`] when the child cannot be waited for.
    pub async fn wait(&mut self) -> Result<ExitStatus, JobError> {
        let exited = match self.child.wait().await {
            Ok(exited) => exited,
            Err(error) => return Err(JobError::Wait(error.to_string())),
        };
        self.drain(STOP_GRACE).await;
        Ok(exited)
    }

    /// Signals the process group, waits out `grace` and kills what is left.
    ///
    /// # Errors
    ///
    /// Returns [`JobError::Wait`] when the child cannot be waited for.
    pub async fn stop(&mut self, grace: Duration) -> Result<ExitStatus, JobError> {
        let _ = killpg(self.pgid, Signal::SIGTERM);
        let exited = tokio::time::timeout(grace, self.child.wait()).await;
        let exited = match exited {
            Ok(exited) => exited,
            Err(_elapsed) => {
                let _ = killpg(self.pgid, Signal::SIGKILL);
                self.child.wait().await
            }
        };
        let exited = match exited {
            Ok(exited) => exited,
            Err(error) => return Err(JobError::Wait(error.to_string())),
        };
        self.drain(grace).await;
        Ok(exited)
    }

    async fn drain(&mut self, budget: Duration) {
        let readers = std::mem::take(&mut self.readers);
        for reader in readers {
            let _ = tokio::time::timeout(budget, reader).await;
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
        let chunk = &buffer[..read];
        log.write(chunk);
        lines.feed(chunk, &log);
    }
    lines.finish(&log);
}

struct LineAssembler {
    pending: Vec<u8>,
}

impl LineAssembler {
    fn new() -> Self {
        LineAssembler {
            pending: Vec::new(),
        }
    }

    fn feed(&mut self, bytes: &[u8], log: &LogStream) {
        for byte in bytes {
            if *byte == b'\n' {
                self.emit(log);
                continue;
            }
            self.pending.push(*byte);
            if self.pending.len() == MAX_LOG_CHUNK {
                self.emit(log);
            }
        }
    }

    fn emit(&mut self, log: &LogStream) {
        let line = std::mem::take(&mut self.pending);
        let line = String::from_utf8_lossy(&line).into_owned();
        let line = match line.strip_suffix('\r') {
            Some(line) => line.to_string(),
            None => line,
        };
        log.push_line(line);
    }

    fn finish(&mut self, log: &LogStream) {
        if self.pending.is_empty() {
            return;
        }
        self.emit(log);
    }
}

#[cfg(test)]
mod tests {
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

    #[tokio::test]
    async fn a_missing_checkout_is_refused() {
        let mut spec = spec();
        spec.ctx = PathBuf::from("/tmp/ralphex-macos-runner-does-not-exist");
        let error = validate(&spec).await.unwrap_err();
        assert_eq!(error.fail_reason(), "ctx_invalid");
    }
}
