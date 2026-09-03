#![allow(dead_code)]

//! Shared doubles for the integration suite.

pub mod fake_farm;

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ralphex_macos_runner::protocol::client::Sleeper;

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
