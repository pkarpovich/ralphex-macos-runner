#![allow(dead_code)]

//! Shared doubles for the integration suite.

pub mod fake_farm;

use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::sys::signal::kill;
use nix::unistd::Pid;
use ralphex_macos_runner::agent::AgentOptions;
use ralphex_macos_runner::logstream::Ticker;
use ralphex_macos_runner::pr::PrTools;
use ralphex_macos_runner::protocol::client::Sleeper;
use ralphex_macos_runner::protocol::types::{Branch, CompleteRequest, CreatePr, Job, RunId};
use tempfile::TempDir;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc;

use fake_farm::FakeFarm;

/// A git checkout with a plan in it and the paths the fakes record to.
pub struct Checkout {
    dir: TempDir,
    plan: PathBuf,
    record: PathBuf,
    tools: PathBuf,
}

impl Checkout {
    /// Returns a fresh git checkout holding `plan.md`.
    ///
    /// # Panics
    ///
    /// Panics when the directory cannot be made or `git init` fails.
    #[must_use]
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("plan.md");
        std::fs::write(&plan, "# plan\n").unwrap();
        let status = std::process::Command::new("git")
            .arg("init")
            .arg("--quiet")
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        let record = dir.path().join("ralphex-record");
        let tools = dir.path().join("tools-record");
        Checkout {
            dir,
            plan,
            record,
            tools,
        }
    }

    /// Returns the checkout's directory as the temporary root named it.
    #[must_use]
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// Returns the checkout's directory with every symlink resolved.
    ///
    /// # Panics
    ///
    /// Panics when the directory does not resolve.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    /// Returns the plan's path with every symlink resolved.
    ///
    /// # Panics
    ///
    /// Panics when the plan does not resolve.
    #[must_use]
    pub fn plan(&self) -> PathBuf {
        self.plan.canonicalize().unwrap()
    }

    /// Returns the file the fake ralphex records its invocation to.
    #[must_use]
    pub fn record(&self) -> &Path {
        &self.record
    }

    /// Returns the file the git and GitHub CLI stand-ins record their runs to.
    #[must_use]
    pub fn tools(&self) -> &Path {
        &self.tools
    }

    /// Returns a fake ralphex in this checkout, scripted with `settings`.
    #[must_use]
    pub fn ralphex(&self, settings: &[(&str, &str)]) -> PathBuf {
        let mut settings = settings.to_vec();
        let record = self.record.display().to_string();
        settings.push(("FAKE_RALPHEX_RECORD", &record));
        fake_ralphex_with(self.dir.path(), &settings)
    }
}

impl Default for Checkout {
    fn default() -> Self {
        Checkout::new()
    }
}

/// Returns the job a Linear ticket dispatched for `ctx` and `plan`.
#[must_use]
pub fn ticket_job(ctx: &Path, plan: &Path, create_pr: CreatePr) -> Job {
    Job {
        run_id: RunId("FARM-12-1753180800000".to_string()),
        issue_id: "issue-uuid".to_string(),
        identifier: "FARM-12".to_string(),
        issue_url: "https://linear.app/example/issue/FARM-12".to_string(),
        title: "split farm and runner".to_string(),
        repo_slug: "owner/repo".to_string(),
        plan_path: plan.display().to_string(),
        branch: Branch("x".to_string()),
        mode: String::new(),
        lease_ttl_seconds: 180,
        runtime: "native".to_string(),
        ctx: ctx.display().to_string(),
        create_pr,
    }
}

/// Returns the job an `rxd` invocation opened in `checkout` under `run_id`.
#[must_use]
pub fn local_job(checkout: &Checkout, run_id: &str) -> Job {
    Job {
        run_id: RunId(run_id.to_string()),
        issue_id: String::new(),
        identifier: String::new(),
        issue_url: String::new(),
        title: "plan".to_string(),
        repo_slug: String::new(),
        plan_path: checkout.plan().display().to_string(),
        branch: Branch("plan".to_string()),
        mode: String::new(),
        lease_ttl_seconds: 180,
        runtime: "native".to_string(),
        ctx: checkout.path().display().to_string(),
        create_pr: CreatePr::No,
    }
}

/// Returns the agent options the suite runs with, recording tools to `record`.
#[must_use]
pub fn options(record: &Path) -> AgentOptions {
    AgentOptions {
        heartbeat_interval: Duration::from_millis(20),
        drain_timeout: Duration::from_millis(50),
        stop_grace: Duration::from_millis(200),
        claim_retry_delay: Duration::from_millis(10),
        ticker: fixed_ticker(Duration::from_millis(20)),
        pr_tools: PrTools {
            git: fake_git().display().to_string(),
            gh: fake_gh().display().to_string(),
            env: vec![("FAKE_RECORD".to_string(), record.display().to_string())],
            step_timeout: Duration::from_secs(30),
        },
    }
}

/// Polls `ready` every ten milliseconds until it answers or ten seconds pass.
pub async fn wait_for<T>(mut ready: impl FnMut() -> Option<T>) -> Option<T> {
    for _attempt in 0..1000 {
        if let Some(value) = ready() {
            return Some(value);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
}

/// Returns what the fake ralphex recorded, once it has started.
///
/// # Panics
///
/// Panics when it never starts.
pub async fn spawned(record: &Path) -> Record {
    let started = wait_for(|| {
        let Ok(contents) = std::fs::read_to_string(record) else {
            return None;
        };
        if contents.contains("pid: ") {
            return Some(());
        }
        None
    })
    .await;
    assert!(started.is_some(), "the fake ralphex never started");
    Record::read(record)
}

/// Returns the first completion the fake farm received.
///
/// # Panics
///
/// Panics when none arrives.
pub async fn completion(farm: &FakeFarm) -> CompleteRequest {
    let recorded = wait_for(|| farm.requests_ending("/complete").first().cloned()).await;
    let Some(recorded) = recorded else {
        panic!("no completion arrived");
    };
    serde_json::from_slice(&recorded.body).unwrap()
}

/// Returns whether the process `pid` still exists.
#[must_use]
pub fn alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

/// Waits up to `budget` for the process `pid` to be gone.
pub async fn gone_within(pid: i32, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while alive(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    true
}

/// Waits for the process `pid` to be gone and returns whether it went.
pub async fn dead(pid: i32) -> bool {
    gone_within(pid, Duration::from_secs(10)).await
}

/// Spawns the real `rxd` against `socket`, in `checkout`, with `args`.
///
/// # Panics
///
/// Panics when the binary cannot be started.
#[must_use]
pub fn rxd(
    socket: &Path,
    checkout: &Checkout,
    args: &[&str],
    env: &[(&str, &str)],
) -> tokio::process::Child {
    let mut argv = args.to_vec();
    let socket = socket.display().to_string();
    argv.push("--socket");
    argv.push(&socket);
    rxd_argv(checkout, &argv, env)
}

/// Spawns the real `rxd` in `checkout` with exactly `args`.
///
/// # Panics
///
/// Panics when the binary cannot be started.
#[must_use]
pub fn rxd_argv(checkout: &Checkout, args: &[&str], env: &[(&str, &str)]) -> tokio::process::Child {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_rxd"));
    for argument in args {
        command.arg(argument);
    }
    command.current_dir(checkout.dir());
    command.env_remove("CLAUDE_CONFIG_DIR");
    for (key, value) in env {
        command.env(key, value);
    }
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    command.spawn().unwrap()
}

/// A [`Sleeper`] that returns at once, records every delay and advances a clock
/// of its own by the delay it was asked for.
pub struct TestSleeper {
    state: Mutex<SleeperState>,
}

struct SleeperState {
    slept: Vec<Duration>,
    now: Instant,
}

impl TestSleeper {
    /// Returns a sleeper whose clock starts now and which has slept nothing.
    #[must_use]
    pub fn new() -> Self {
        TestSleeper {
            state: Mutex::new(SleeperState {
                slept: Vec::new(),
                now: Instant::now(),
            }),
        }
    }

    /// Returns every delay this sleeper was asked for, in order.
    #[must_use]
    pub fn slept(&self) -> Vec<Duration> {
        let state = self.state.lock().unwrap();
        state.slept.clone()
    }
}

impl Default for TestSleeper {
    fn default() -> Self {
        TestSleeper::new()
    }
}

impl Sleeper for TestSleeper {
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let mut state = self.state.lock().unwrap();
        state.slept.push(duration);
        state.now += duration;
        drop(state);
        Box::pin(std::future::ready(()))
    }

    fn now(&self) -> Instant {
        let state = self.state.lock().unwrap();
        state.now
    }
}

/// A [`Ticker`] that fires only when its [`TickHandle`] releases it.
pub struct ManualTicker {
    requests: AsyncMutex<mpsc::Receiver<()>>,
    acks: mpsc::Sender<()>,
    served: AtomicBool,
}

/// The test's end of a [`ManualTicker`].
pub struct TickHandle {
    requests: mpsc::Sender<()>,
    acks: AsyncMutex<mpsc::Receiver<()>>,
}

impl TickHandle {
    /// Releases one tick and returns once the work it triggered is finished.
    ///
    /// # Panics
    ///
    /// Panics when the ticker it belongs to was dropped.
    pub async fn drive(&self) {
        self.requests.send(()).await.unwrap();
        let mut acks = self.acks.lock().await;
        acks.recv().await.unwrap();
    }
}

/// Returns a ticker and the handle that drives it.
#[must_use]
pub fn manual_ticker() -> (Arc<ManualTicker>, TickHandle) {
    let (requests, incoming) = mpsc::channel(64);
    let (acks, finished) = mpsc::channel(64);
    let ticker = Arc::new(ManualTicker {
        requests: AsyncMutex::new(incoming),
        acks,
        served: AtomicBool::new(false),
    });
    let handle = TickHandle {
        requests,
        acks: AsyncMutex::new(finished),
    };
    (ticker, handle)
}

/// Returns the path of the stand-in for the ralphex binary.
#[must_use]
pub fn fake_ralphex() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake-ralphex.sh")
}

/// Writes a wrapper in `dir` that runs the fake ralphex under `settings`.
///
/// The daemon spawns a claimed job with no environment of its own, so a test
/// that has to script the fake gives the wrapper's path as the ralphex binary.
///
/// # Panics
///
/// Panics when the wrapper cannot be written or made executable.
#[must_use]
pub fn fake_ralphex_with(dir: &Path, settings: &[(&str, &str)]) -> PathBuf {
    let path = dir.join("ralphex");
    let mut script = String::from("#!/bin/sh\n");
    for (key, value) in settings {
        script.push_str(&format!("{key}='{value}'\nexport {key}\n"));
    }
    script.push_str(&format!("exec '{}' \"$@\"\n", fake_ralphex().display()));
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A [`Ticker`] that fires on a fixed interval.
pub struct FixedTicker {
    interval: Duration,
}

impl Ticker for FixedTicker {
    fn tick(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(tokio::time::sleep(self.interval))
    }
}

/// Returns a ticker that fires every `interval`.
#[must_use]
pub fn fixed_ticker(interval: Duration) -> Arc<FixedTicker> {
    Arc::new(FixedTicker { interval })
}

/// Returns the path of the stand-in for git.
#[must_use]
pub fn fake_git() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support/bin/git")
}

/// Returns the path of the stand-in for the GitHub CLI.
#[must_use]
pub fn fake_gh() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support/bin/gh")
}

/// One run of a git or GitHub CLI stand-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// The name the stand-in records itself under.
    pub program: String,
    /// The working directory the run had, with every symlink resolved.
    pub cwd: String,
    /// The arguments the run was given, without the program name.
    pub args: Vec<String>,
}

impl Invocation {
    /// Returns whether the run started with `args`.
    #[must_use]
    pub fn starts_with(&self, args: &[&str]) -> bool {
        if self.args.len() < args.len() {
            return false;
        }
        for (index, expected) in args.iter().enumerate() {
            if self.args[index] != *expected {
                return false;
            }
        }
        true
    }
}

/// Returns every run the stand-ins recorded in `path`, in order.
///
/// # Panics
///
/// Panics when the file carries a key no stand-in writes.
#[must_use]
pub fn invocations(path: &Path) -> Vec<Invocation> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut runs: Vec<Invocation> = Vec::new();
    for line in contents.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = match value.strip_prefix(' ') {
            Some(value) => value,
            None => value,
        };
        match key {
            "cmd" => runs.push(Invocation {
                program: value.to_string(),
                cwd: String::new(),
                args: Vec::new(),
            }),
            "cwd" => {
                let Some(run) = runs.last_mut() else {
                    continue;
                };
                run.cwd = value.to_string();
            }
            "arg" => {
                let Some(run) = runs.last_mut() else {
                    continue;
                };
                run.args.push(value.to_string());
            }
            other => panic!("unexpected record key {other}"),
        }
    }
    runs
}

/// What one run of the fake ralphex saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The arguments the run was given, without the program name.
    pub argv: Vec<String>,
    /// The working directory the run had, with every symlink resolved.
    pub cwd: String,
    /// The process id of the run.
    pub pid: i32,
    /// The process id of the child the run left behind, when it left one.
    pub child: Option<i32>,
    /// The environment the run saw.
    pub env: Vec<(String, String)>,
}

impl Record {
    /// Reads the record the fake ralphex wrote to `path`.
    ///
    /// # Panics
    ///
    /// Panics when the file is missing, unreadable or does not name a process id.
    #[must_use]
    pub fn read(path: &Path) -> Self {
        let contents = std::fs::read_to_string(path).unwrap();
        let mut argv = Vec::new();
        let mut cwd = String::new();
        let mut pid = None;
        let mut child = None;
        let mut env = Vec::new();
        for line in contents.lines() {
            let Some((key, value)) = line.split_once(": ") else {
                continue;
            };
            match key {
                "argv" => argv.push(value.to_string()),
                "cwd" => cwd = value.to_string(),
                "pid" => pid = Some(value.parse().unwrap()),
                "child" => child = Some(value.parse().unwrap()),
                "env" => {
                    let Some((name, setting)) = value.split_once('=') else {
                        continue;
                    };
                    env.push((name.to_string(), setting.to_string()));
                }
                other => panic!("unexpected record key {other}"),
            }
        }
        let Some(pid) = pid else {
            panic!("the record names no process id");
        };
        Record {
            argv,
            cwd,
            pid,
            child,
            env,
        }
    }

    /// Returns the value the environment gave `name`.
    #[must_use]
    pub fn env_value(&self, name: &str) -> Option<String> {
        for (key, value) in &self.env {
            if key == name {
                return Some(value.clone());
            }
        }
        None
    }
}

impl Ticker for ManualTicker {
    fn tick(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            if self.served.swap(false, Ordering::SeqCst) {
                let _ = self.acks.send(()).await;
            }
            let mut requests = self.requests.lock().await;
            match requests.recv().await {
                Some(()) => {}
                None => std::future::pending::<()>().await,
            }
            drop(requests);
            self.served.store(true, Ordering::SeqCst);
        })
    }
}
