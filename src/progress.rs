//! The run's timeline: the section markers ralphex prints and the phase they set.
//!
//! [`marker`] classifies one plain line of a run's output with the farm's
//! container runner's two expressions, and a [`PhaseTracker`] fed every line
//! through [`crate::logstream::LogStream::push_line`] turns the markers into the
//! phase the next plan-progress snapshot carries and the requests for one.

use std::sync::{LazyLock, Mutex};

use regex::Regex;
use tokio::sync::Notify;

use crate::protocol::types::Phase;

const MARKER_MAX_BYTES: usize = 256;

static TASK_ITERATION: LazyLock<Regex> =
    LazyLock::new(|| compile(r"^--- task iteration [0-9]+ ---$"));
static REVIEW: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r"^--- (claude review [0-9]+.*|codex external review|codex iteration [0-9]+|claude evaluating codex findings) ---$",
    )
});

fn compile(pattern: &str) -> Regex {
    Regex::new(pattern).expect("marker patterns are valid")
}

/// A section marker ralphex prints between the stages of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    /// A `--- task iteration N ---` line: ralphex starts working on a task.
    TaskIteration,
    /// One of the review section lines: ralphex reviews the work.
    Review,
}

/// Returns the section marker `line` is, if it is one.
///
/// `line` is the plain text of one output line, without its escape sequences
/// and its trailing carriage return. The whole line has to match; a line
/// longer than 256 bytes is never a marker.
///
/// # Examples
///
/// ```
/// use ralphex_macos_runner::progress::{Marker, marker};
///
/// assert_eq!(marker("--- task iteration 3 ---"), Some(Marker::TaskIteration));
/// assert_eq!(marker("--- codex external review ---"), Some(Marker::Review));
/// assert_eq!(marker("--- finalize step ---"), None);
/// ```
#[must_use]
pub fn marker(line: &str) -> Option<Marker> {
    if line.len() > MARKER_MAX_BYTES {
        return None;
    }
    if TASK_ITERATION.is_match(line) {
        return Some(Marker::TaskIteration);
    }
    if REVIEW.is_match(line) {
        return Some(Marker::Review);
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Freeze {
    Open,
    Frozen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Request {
    Idle,
    Pending,
}

struct State {
    phase: Phase,
    freeze: Freeze,
    request: Request,
}

/// The phase of one run and the pending request for a snapshot of it.
///
/// The phase starts as [`Phase::Setup`] and moves on the markers
/// [`PhaseTracker::observe`] sees until [`PhaseTracker::freeze`] fixes it.
/// Requests coalesce: while one is pending, more add nothing.
pub struct PhaseTracker {
    state: Mutex<State>,
    wake: Notify,
}

impl Default for PhaseTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl PhaseTracker {
    /// Returns a tracker in [`Phase::Setup`] with no request pending.
    #[must_use]
    pub fn new() -> Self {
        PhaseTracker {
            state: Mutex::new(State {
                phase: Phase::Setup,
                freeze: Freeze::Open,
                request: Request::Idle,
            }),
            wake: Notify::new(),
        }
    }

    /// Moves the phase on the marker `line` is, if it is one.
    ///
    /// A task iteration sets [`Phase::Tasks`] and requests a snapshot even when
    /// the phase already was [`Phase::Tasks`]; a review marker sets
    /// [`Phase::Review`] and requests one when the phase changed. Any other line,
    /// and every line after [`PhaseTracker::freeze`], changes nothing.
    ///
    /// # Panics
    ///
    /// Panics when another holder of the state lock panicked.
    pub fn observe(&self, line: &str) {
        let Some(marker) = marker(line) else {
            return;
        };
        let mut state = self.state.lock().unwrap();
        match state.freeze {
            Freeze::Frozen => return,
            Freeze::Open => {}
        }
        let changed = match marker {
            Marker::TaskIteration => {
                state.phase = Phase::Tasks;
                true
            }
            Marker::Review => {
                let changed = state.phase != Phase::Review;
                state.phase = Phase::Review;
                changed
            }
        };
        if !changed {
            return;
        }
        state.request = Request::Pending;
        drop(state);
        self.wake.notify_one();
    }

    /// Returns the phase the next snapshot carries.
    ///
    /// # Panics
    ///
    /// Panics when another holder of the state lock panicked.
    #[must_use]
    pub fn phase(&self) -> Phase {
        let state = self.state.lock().unwrap();
        state.phase
    }

    /// Records `phase` and fixes it for the rest of the run.
    ///
    /// No snapshot is requested: the caller posts the phase itself.
    ///
    /// # Panics
    ///
    /// Panics when another holder of the state lock panicked.
    pub fn freeze(&self, phase: Phase) {
        let mut state = self.state.lock().unwrap();
        state.phase = phase;
        state.freeze = Freeze::Frozen;
    }

    /// Requests a snapshot, unless one is already pending.
    ///
    /// # Panics
    ///
    /// Panics when another holder of the state lock panicked.
    pub fn request(&self) {
        let mut state = self.state.lock().unwrap();
        state.request = Request::Pending;
        drop(state);
        self.wake.notify_one();
    }

    /// Resolves once a snapshot is requested and takes the request.
    ///
    /// # Panics
    ///
    /// Panics when another holder of the state lock panicked.
    pub async fn requested(&self) {
        loop {
            let woken = self.wake.notified();
            match self.take_request() {
                Request::Pending => return,
                Request::Idle => {}
            }
            woken.await;
        }
    }

    fn take_request(&self) -> Request {
        let mut state = self.state.lock().unwrap();
        let request = state.request;
        state.request = Request::Idle;
        request
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::time::Duration;

    use crate::ansi::plain;

    #[test]
    fn the_marker_expressions_match_their_examples() {
        let cases = [
            ("--- task iteration 12 ---", Some(Marker::TaskIteration)),
            ("--- task iteration 0 ---", Some(Marker::TaskIteration)),
            (
                "--- claude review 0: all findings ---",
                Some(Marker::Review),
            ),
            ("--- claude review 3 ---", Some(Marker::Review)),
            ("--- codex external review ---", Some(Marker::Review)),
            ("--- codex iteration 3 ---", Some(Marker::Review)),
            (
                "--- claude evaluating codex findings ---",
                Some(Marker::Review),
            ),
        ];
        for (line, expected) in cases {
            assert_eq!(marker(line), expected, "{line}");
        }
    }

    #[test]
    fn near_misses_are_not_markers() {
        let cases = [
            "--- review 1 ---",
            "--- finalize step ---",
            "--- custom review iteration 2 ---",
            "--- claude external review ---",
            "--- task iteration x ---",
            "--- task iteration ---",
            "--- task iteration 1 --- done",
            "--- codex external review --- extra",
            " --- task iteration 1 ---",
            "--- task iteration 1 --- ",
            "",
            "plain output",
        ];
        for line in cases {
            assert_eq!(marker(line), None, "{line:?}");
        }
    }

    #[test]
    fn a_coloured_marker_matches_once_stripped() {
        let line = "\u{1b}[1;36m--- task iteration 4 ---\u{1b}[0m";
        assert_eq!(marker(line), None);
        assert_eq!(marker(&plain(line)), Some(Marker::TaskIteration));
    }

    #[test]
    fn a_line_over_256_bytes_is_never_a_marker() {
        let at_limit = format!("--- claude review 1{} ---", "x".repeat(256 - 23));
        assert_eq!(at_limit.len(), 256);
        assert_eq!(marker(&at_limit), Some(Marker::Review));
        let over = format!("--- claude review 1{} ---", "x".repeat(257 - 23));
        assert_eq!(over.len(), 257);
        assert_eq!(marker(&over), None);
    }

    #[test]
    fn the_phase_starts_in_setup_with_nothing_requested() {
        let tracker = PhaseTracker::new();
        assert_eq!(tracker.phase(), Phase::Setup);
        assert_eq!(tracker.take_request(), Request::Idle);
    }

    #[test]
    fn a_line_that_is_not_a_marker_changes_nothing() {
        let tracker = PhaseTracker::new();
        tracker.observe("compiling the crate");
        assert_eq!(tracker.phase(), Phase::Setup);
        assert_eq!(tracker.take_request(), Request::Idle);
    }

    #[test]
    fn a_task_iteration_requests_a_snapshot_even_when_already_in_tasks() {
        let tracker = PhaseTracker::new();
        tracker.observe("--- task iteration 1 ---");
        assert_eq!(tracker.phase(), Phase::Tasks);
        assert_eq!(tracker.take_request(), Request::Pending);
        tracker.observe("--- task iteration 2 ---");
        assert_eq!(tracker.phase(), Phase::Tasks);
        assert_eq!(tracker.take_request(), Request::Pending);
    }

    #[test]
    fn a_review_marker_requests_a_snapshot_only_when_the_phase_changes() {
        let tracker = PhaseTracker::new();
        tracker.observe("--- claude review 0: all findings ---");
        assert_eq!(tracker.phase(), Phase::Review);
        assert_eq!(tracker.take_request(), Request::Pending);
        tracker.observe("--- codex external review ---");
        assert_eq!(tracker.phase(), Phase::Review);
        assert_eq!(tracker.take_request(), Request::Idle);
    }

    #[test]
    fn requests_coalesce_while_one_is_pending() {
        let tracker = PhaseTracker::new();
        tracker.request();
        tracker.observe("--- task iteration 1 ---");
        tracker.request();
        assert_eq!(tracker.take_request(), Request::Pending);
        assert_eq!(tracker.take_request(), Request::Idle);
    }

    #[test]
    fn freezing_records_the_phase_and_ignores_later_markers() {
        let tracker = PhaseTracker::new();
        tracker.observe("--- task iteration 1 ---");
        let _ = tracker.take_request();
        tracker.freeze(Phase::Pr);
        assert_eq!(tracker.phase(), Phase::Pr);
        assert_eq!(tracker.take_request(), Request::Idle);
        tracker.observe("--- task iteration 2 ---");
        tracker.observe("--- codex external review ---");
        assert_eq!(tracker.phase(), Phase::Pr);
        assert_eq!(tracker.take_request(), Request::Idle);
    }

    #[tokio::test]
    async fn a_waiter_wakes_on_a_request_and_takes_it() {
        let tracker = Arc::new(PhaseTracker::new());
        let waiter = tokio::spawn({
            let tracker = Arc::clone(&tracker);
            async move { tracker.requested().await }
        });
        tracker.observe("--- task iteration 1 ---");
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tracker.take_request(), Request::Idle);
    }

    #[tokio::test]
    async fn a_request_made_before_waiting_is_not_lost() {
        let tracker = PhaseTracker::new();
        tracker.request();
        tokio::time::timeout(Duration::from_secs(5), tracker.requested())
            .await
            .unwrap();
        assert_eq!(tracker.take_request(), Request::Idle);
    }
}
