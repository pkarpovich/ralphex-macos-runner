#![allow(dead_code)]

//! Shared doubles for the integration suite.

pub mod fake_farm;

use std::future::Future;
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
