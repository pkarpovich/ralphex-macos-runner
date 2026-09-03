#![allow(dead_code)]

//! Shared doubles for the integration suite.

pub mod fake_farm;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ralphex_macos_runner::logstream::Ticker;
use ralphex_macos_runner::protocol::client::Sleeper;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc;

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
