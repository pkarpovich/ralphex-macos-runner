//! The run's timeline: the section markers ralphex prints, the phase they set
//! and the plan-progress snapshots posted to the farm.
//!
//! [`marker`] classifies one plain line of a run's output with the farm's
//! container runner's two expressions, and a [`PhaseTracker`] fed every line
//! through [`crate::logstream::LogStream::push_line`] turns the markers into the
//! phase the next plan-progress snapshot carries and the requests for one. A
//! [`PlanWatcher`] watches the plan file, requests a snapshot when it changes
//! and posts every snapshot from one task.

use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use notify::event::ModifyKind;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use regex::Regex;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};

use crate::job::Worktree;
use crate::planfile::{self, Checkbox, Plan, Task};
use crate::protocol::client::{FarmClient, FarmError};
use crate::protocol::types::{
    Branch, PROGRESS_POST_TIMEOUT, Phase, ProgressCheckbox, ProgressRequest, ProgressTask, RunId,
    WATCH_START_TIMEOUT,
};

/// The quiet period after the last change to the plan before a snapshot is requested.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(300);

/// The interval between attempts to watch plan directories that do not exist yet.
pub const DEFAULT_ATTACH_RETRY: Duration = Duration::from_secs(2);

const COMPLETED_DIR: &str = "completed";

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

/// The farm call a [`PlanWatcher`] posts its snapshots through.
///
/// The daemon uses [`FarmClient`]; a test supplies an implementation that
/// records what it is given.
pub trait ProgressSender: Send + Sync {
    /// Posts one plan-progress snapshot of the run `run_id`.
    ///
    /// # Errors
    ///
    /// Returns the [`FarmError`] the farm or the transport answered with.
    fn post<'a>(
        &'a self,
        run_id: &'a RunId,
        request: &'a ProgressRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), FarmError>> + Send + 'a>>;
}

impl ProgressSender for FarmClient {
    fn post<'a>(
        &'a self,
        run_id: &'a RunId,
        request: &'a ProgressRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), FarmError>> + Send + 'a>> {
        Box::pin(self.post_progress(run_id, request))
    }
}

/// Returns where the plan a run works through is expected on disk.
///
/// Without a worktree this is `plan` itself. With one it is the same path
/// relative to `top`, the checkout's top level, under
/// `<top>/.ralphex/worktrees/<branch>/`, the branch taken verbatim, so a branch
/// with `/` gives nested directories.
///
/// # Examples
///
/// ```
/// use std::path::Path;
///
/// use ralphex_macos_runner::job::Worktree;
/// use ralphex_macos_runner::progress::expected_plan;
/// use ralphex_macos_runner::protocol::types::Branch;
///
/// let top = Path::new("/src/nhop");
/// let plan = Path::new("/src/nhop/docs/plans/x.md");
/// let branch = Branch("feature/x".to_string());
/// assert_eq!(expected_plan(top, plan, &branch, Worktree::No), plan);
/// assert_eq!(
///     expected_plan(top, plan, &branch, Worktree::Yes),
///     Path::new("/src/nhop/.ralphex/worktrees/feature/x/docs/plans/x.md"),
/// );
/// ```
#[must_use]
pub fn expected_plan(top: &Path, plan: &Path, branch: &Branch, worktree: Worktree) -> PathBuf {
    match worktree {
        Worktree::No => plan.to_path_buf(),
        Worktree::Yes => {
            let Ok(relative) = plan.strip_prefix(top) else {
                return plan.to_path_buf();
            };
            let mut expected = top.join(".ralphex").join("worktrees");
            for segment in branch.as_str().split('/') {
                expected.push(segment);
            }
            expected.join(relative)
        }
    }
}

/// Returns the plan file as it is on disk right now, if it is anywhere.
///
/// The first of `expected` and `completed/<file name>` beside it that exists
/// and is not a directory wins.
#[must_use]
pub fn resolve(expected: &Path) -> Option<PathBuf> {
    if is_file(expected) {
        return Some(expected.to_path_buf());
    }
    let (Some(directory), Some(name)) = (expected.parent(), expected.file_name()) else {
        return None;
    };
    let completed = directory.join(COMPLETED_DIR).join(name);
    if is_file(&completed) {
        return Some(completed);
    }
    None
}

fn is_file(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(metadata) => !metadata.is_dir(),
        Err(_) => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Running,
    Failed,
}

fn snapshot(expected: &Path, phase: Phase, outcome: Outcome) -> ProgressRequest {
    let failed = match outcome {
        Outcome::Running => false,
        Outcome::Failed => true,
    };
    ProgressRequest {
        phase,
        failed,
        tasks: read_tasks(expected),
    }
}

fn read_tasks(expected: &Path) -> Option<Vec<ProgressTask>> {
    let path = resolve(expected)?;
    let content = match planfile::read(&path) {
        Ok(content) => content,
        Err(error) => {
            tracing::warn!("no tasks from {}: {error}", path.display());
            return None;
        }
    };
    let Plan { title: _, tasks } = planfile::parse(&content);
    if tasks.is_empty() {
        return None;
    }
    let mut progress = Vec::new();
    for Task {
        number,
        ord,
        title,
        status,
        checkboxes,
    } in tasks
    {
        let mut boxes = Vec::new();
        for Checkbox { text, checked } in checkboxes {
            boxes.push(ProgressCheckbox { text, checked });
        }
        progress.push(ProgressTask {
            number,
            ord,
            title,
            status,
            checkboxes: boxes,
        });
    }
    Some(progress)
}

/// The intervals a [`PlanWatcher`] waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchTimings {
    /// The quiet period after the last change to the plan before a snapshot is requested.
    pub debounce: Duration,
    /// The interval between attempts to watch directories that do not exist yet.
    pub attach_retry: Duration,
}

struct Poster {
    sender: Arc<dyn ProgressSender>,
    run_id: RunId,
    expected: PathBuf,
}

impl Poster {
    async fn post(&self, phase: Phase, outcome: Outcome) {
        let expected = self.expected.clone();
        let taken = tokio::task::spawn_blocking(move || snapshot(&expected, phase, outcome)).await;
        let request = match taken {
            Ok(request) => request,
            Err(error) => {
                tracing::warn!("the plan of run {} was not read: {error}", self.run_id);
                return;
            }
        };
        if let Err(error) = self.sender.post(&self.run_id, &request).await {
            tracing::warn!(
                "the progress of run {} was not posted: {error}",
                self.run_id
            );
        }
    }

    async fn post_within_deadline(&self, phase: Phase, outcome: Outcome) {
        let posted = tokio::time::timeout(PROGRESS_POST_TIMEOUT, self.post(phase, outcome)).await;
        if posted.is_err() {
            tracing::warn!(
                "the progress of run {} was not posted within {PROGRESS_POST_TIMEOUT:?}",
                self.run_id
            );
        }
    }
}

/// The plan watcher and the snapshot poster of one run.
///
/// Started before ralphex is spawned, it posts a [`Phase::Setup`] snapshot
/// first, then one whenever the plan file changes or its
/// [`PlanWatcher::tracker`] requests one. Every post comes from one task, so
/// two are never in flight at once, and requests made while one is pending
/// coalesce. A failed post is logged and forgotten. The plan is read on the
/// blocking pool, so a mount that stops answering holds a blocking thread, not
/// a runtime worker, and cannot outlast [`PROGRESS_POST_TIMEOUT`] after stop.
pub struct PlanWatcher {
    poster: Arc<Poster>,
    tracker: Arc<PhaseTracker>,
    events: JoinHandle<()>,
    posts: JoinHandle<()>,
    opened: oneshot::Receiver<()>,
}

impl PlanWatcher {
    /// Starts watching the plan expected at `expected` and posting its snapshots.
    ///
    /// The directory of `expected` and its `completed/` sibling are watched
    /// without recursion. A directory that does not exist yet is retried every
    /// `timings.attach_retry` and on every event; until the plan's own
    /// directory attaches, its nearest existing ancestor is watched instead. A
    /// change to a file named like the plan requests a snapshot once
    /// `timings.debounce` has passed without another. The first directories are
    /// handed to the watcher on the blocking pool, and this returns once they
    /// are, or after [`WATCH_START_TIMEOUT`] without a watcher: the snapshots
    /// the markers request are still posted.
    ///
    /// # Panics
    ///
    /// Panics when called outside a tokio runtime.
    pub async fn start(
        sender: Arc<dyn ProgressSender>,
        run_id: RunId,
        expected: PathBuf,
        timings: WatchTimings,
    ) -> PlanWatcher {
        let tracker = Arc::new(PhaseTracker::new());
        let (sink, events) = mpsc::unbounded_channel();
        let watched = expected.clone();
        let watches = start_within(WATCH_START_TIMEOUT, &expected, move || {
            Watches::start(&watched, sink)
        })
        .await;
        let name = match expected.file_name() {
            Some(name) => name.to_os_string(),
            None => OsString::new(),
        };
        let events = tokio::spawn(follow_events(
            watches,
            events,
            name,
            Arc::clone(&tracker),
            timings,
        ));
        let poster = Arc::new(Poster {
            sender,
            run_id,
            expected,
        });
        let (opening, opened) = oneshot::channel();
        let posts = tokio::spawn(post_requested(
            Arc::clone(&poster),
            Arc::clone(&tracker),
            opening,
        ));
        PlanWatcher {
            poster,
            tracker,
            events,
            posts,
            opened,
        }
    }

    /// Returns the tracker whose phase the snapshots carry.
    #[must_use]
    pub fn tracker(&self) -> Arc<PhaseTracker> {
        Arc::clone(&self.tracker)
    }

    /// Stops watching and posting, and returns what can still post the run's last snapshots.
    ///
    /// The opening [`Phase::Setup`] snapshot is waited for, within its
    /// [`PROGRESS_POST_TIMEOUT`], so it can neither be lost to a run that ends
    /// at once nor land after the snapshots posted from here on; any later post
    /// in flight is abandoned. The file-system watcher is released on the
    /// blocking pool, since ending its thread waits on the file-system events
    /// daemon, and this returns without waiting for it.
    pub async fn stop(self) -> StoppedWatcher {
        let PlanWatcher {
            poster,
            tracker,
            events,
            posts,
            opened,
        } = self;
        events.abort();
        let _opened = opened.await;
        posts.abort();
        let _ended = events.await;
        let _ended = posts.await;
        StoppedWatcher { poster, tracker }
    }
}

/// A [`PlanWatcher`] after [`PlanWatcher::stop`]: nothing posts but its caller.
pub struct StoppedWatcher {
    poster: Arc<Poster>,
    tracker: Arc<PhaseTracker>,
}

impl StoppedWatcher {
    /// Fixes the run's phase at `phase` and posts a snapshot of it.
    ///
    /// The post is given [`PROGRESS_POST_TIMEOUT`]; a failure is logged.
    pub async fn post_phase(&self, phase: Phase) {
        self.tracker.freeze(phase);
        self.poster
            .post_within_deadline(phase, Outcome::Running)
            .await;
    }

    /// Posts a snapshot that marks the run as failed.
    ///
    /// The post is given [`PROGRESS_POST_TIMEOUT`]; a failure is logged.
    pub async fn post_failure(&self) {
        self.poster
            .post_within_deadline(self.tracker.phase(), Outcome::Failed)
            .await;
    }
}

async fn post_requested(
    poster: Arc<Poster>,
    tracker: Arc<PhaseTracker>,
    opening: oneshot::Sender<()>,
) {
    poster
        .post_within_deadline(Phase::Setup, Outcome::Running)
        .await;
    let _sent = opening.send(());
    loop {
        tracker.requested().await;
        poster.post(tracker.phase(), Outcome::Running).await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attachment {
    Attached,
    Detached,
}

type EventSink = mpsc::UnboundedSender<notify::Result<Event>>;

struct Watches {
    watcher: RecommendedWatcher,
    directory: PathBuf,
    directory_state: Attachment,
    completed: PathBuf,
    completed_state: Attachment,
    ancestor: Option<PathBuf>,
}

impl Watches {
    fn start(expected: &Path, sink: EventSink) -> Option<Watches> {
        let Some(directory) = expected.parent() else {
            tracing::warn!("the plan {} has no directory to watch", expected.display());
            return None;
        };
        let handler = move |event| {
            let _sent = sink.send(event);
        };
        let watcher = match notify::recommended_watcher(handler) {
            Ok(watcher) => watcher,
            Err(error) => {
                tracing::warn!("the plan {} cannot be watched: {error}", expected.display());
                return None;
            }
        };
        let mut watches = Watches {
            watcher,
            directory: directory.to_path_buf(),
            directory_state: Attachment::Detached,
            completed: directory.join(COMPLETED_DIR),
            completed_state: Attachment::Detached,
            ancestor: None,
        };
        let _attached = watches.attach();
        Some(watches)
    }

    fn attachment(&self) -> Attachment {
        match (self.directory_state, self.completed_state) {
            (Attachment::Attached, Attachment::Attached) => Attachment::Attached,
            (Attachment::Attached, Attachment::Detached) => Attachment::Detached,
            (Attachment::Detached, Attachment::Attached) => Attachment::Detached,
            (Attachment::Detached, Attachment::Detached) => Attachment::Detached,
        }
    }

    fn attach(&mut self) -> Attachment {
        let mut attached = Attachment::Detached;
        match self.directory_state {
            Attachment::Attached => {}
            Attachment::Detached => match watch(&mut self.watcher, &self.directory) {
                Attachment::Attached => {
                    self.directory_state = Attachment::Attached;
                    attached = Attachment::Attached;
                    if let Some(ancestor) = self.ancestor.take() {
                        let _unwatched = self.watcher.unwatch(&ancestor);
                    }
                }
                Attachment::Detached => self.fall_back(),
            },
        }
        match self.completed_state {
            Attachment::Attached => {}
            Attachment::Detached => match watch(&mut self.watcher, &self.completed) {
                Attachment::Attached => {
                    self.completed_state = Attachment::Attached;
                    attached = Attachment::Attached;
                }
                Attachment::Detached => {}
            },
        }
        attached
    }

    fn fall_back(&mut self) {
        let nearest = nearest_ancestor(&self.directory);
        if nearest == self.ancestor {
            return;
        }
        if let Some(ancestor) = self.ancestor.take() {
            let _unwatched = self.watcher.unwatch(&ancestor);
        }
        let Some(nearest) = nearest else {
            return;
        };
        match watch(&mut self.watcher, &nearest) {
            Attachment::Attached => self.ancestor = Some(nearest),
            Attachment::Detached => {}
        }
    }
}

fn watch(watcher: &mut RecommendedWatcher, directory: &Path) -> Attachment {
    if !directory.is_dir() {
        return Attachment::Detached;
    }
    match watcher.watch(directory, RecursiveMode::NonRecursive) {
        Ok(()) => Attachment::Attached,
        Err(error) => {
            tracing::debug!("{} cannot be watched: {error}", directory.display());
            Attachment::Detached
        }
    }
}

fn nearest_ancestor(directory: &Path) -> Option<PathBuf> {
    let mut candidate = directory.parent();
    while let Some(path) = candidate {
        if path.is_dir() {
            return Some(path.to_path_buf());
        }
        candidate = path.parent();
    }
    None
}

fn names_plan(event: &Event, name: &OsStr) -> bool {
    let Event {
        kind,
        paths,
        attrs: _,
    } = event;
    let changes = match kind {
        EventKind::Create(_) => true,
        EventKind::Modify(ModifyKind::Data(_)) => true,
        EventKind::Modify(ModifyKind::Name(_)) => true,
        EventKind::Modify(ModifyKind::Any) => false,
        EventKind::Modify(ModifyKind::Metadata(_)) => false,
        EventKind::Modify(ModifyKind::Other) => false,
        EventKind::Remove(_) => true,
        EventKind::Any => false,
        EventKind::Access(_) => false,
        EventKind::Other => false,
    };
    if !changes {
        return false;
    }
    for path in paths {
        if path.file_name() == Some(name) {
            return true;
        }
    }
    false
}

async fn quiet(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn follow_events(
    watches: Option<Watches>,
    mut events: mpsc::UnboundedReceiver<notify::Result<Event>>,
    name: OsString,
    tracker: Arc<PhaseTracker>,
    timings: WatchTimings,
) {
    let Some(watches) = watches else {
        return;
    };
    let mut watches = Offloaded(Some(watches));
    let WatchTimings {
        debounce,
        attach_retry,
    } = timings;
    let mut retry = tokio::time::interval_at(Instant::now() + attach_retry, attach_retry);
    retry.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut deadline = None;
    loop {
        let Offloaded(Some(held)) = &watches else {
            return;
        };
        let detached = match held.attachment() {
            Attachment::Attached => false,
            Attachment::Detached => true,
        };
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else {
                    return;
                };
                match event {
                    Ok(event) => {
                        if names_plan(&event, &name) {
                            deadline = Some(Instant::now() + debounce);
                        }
                    }
                    Err(error) => tracing::debug!("the plan watcher reported: {error}"),
                }
                if detached {
                    let Some(attached) = reattach(watches.0.take(), &tracker).await else {
                        return;
                    };
                    watches.0 = Some(attached);
                }
            }
            () = quiet(deadline) => {
                deadline = None;
                tracker.request();
            }
            _ = retry.tick(), if detached => {
                let Some(attached) = reattach(watches.0.take(), &tracker).await else {
                    return;
                };
                watches.0 = Some(attached);
            }
        }
    }
}

async fn start_within<T: Send + 'static>(
    budget: Duration,
    expected: &Path,
    start: impl FnOnce() -> Option<T> + Send + 'static,
) -> Option<T> {
    let starting = tokio::task::spawn_blocking(start);
    match tokio::time::timeout(budget, starting).await {
        Err(_elapsed) => {
            tracing::warn!(
                "the plan {} cannot be watched: its directories did not answer within {budget:?}",
                expected.display()
            );
            None
        }
        Ok(Err(error)) => {
            tracing::warn!("the plan {} cannot be watched: {error}", expected.display());
            None
        }
        Ok(Ok(watches)) => watches,
    }
}

struct Offloaded<T: Send + 'static>(Option<T>);

impl<T: Send + 'static> Drop for Offloaded<T> {
    fn drop(&mut self) {
        let Some(held) = self.0.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let _released = runtime.spawn_blocking(move || drop(held));
    }
}

async fn reattach(watches: Option<Watches>, tracker: &PhaseTracker) -> Option<Watches> {
    let watches = watches?;
    let attaching = tokio::task::spawn_blocking(move || {
        let mut watches = watches;
        let attached = watches.attach();
        (watches, attached)
    });
    let (watches, attached) = match attaching.await {
        Ok(attaching) => attaching,
        Err(error) => {
            tracing::warn!("the plan watcher stopped attaching directories: {error}");
            return None;
        }
    };
    match attached {
        Attachment::Attached => tracker.request(),
        Attachment::Detached => {}
    }
    Some(watches)
}

#[cfg(test)]
mod tests {
    use super::*;

    use notify::event::{CreateKind, DataChange, MetadataKind, RemoveKind, RenameMode};

    use crate::ansi::plain;
    use crate::protocol::types::TaskStatus;

    fn event(kind: EventKind, path: &str) -> Event {
        Event::new(kind).add_path(PathBuf::from(path))
    }

    #[tokio::test]
    async fn a_plan_directory_that_does_not_answer_in_time_is_not_watched() {
        let (release, stalled) = std::sync::mpsc::channel::<()>();
        let started = std::time::Instant::now();

        let watches = start_within(Duration::from_millis(50), Path::new("x.md"), move || {
            stalled.recv().ok()
        })
        .await;

        assert_eq!(watches, None);
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(release);
    }

    struct Stalling(Option<std::sync::mpsc::Receiver<()>>);

    impl Drop for Stalling {
        fn drop(&mut self) {
            if let Some(release) = self.0.take() {
                let _released = release.recv();
            }
        }
    }

    #[tokio::test]
    async fn a_watcher_that_stalls_on_release_does_not_hold_the_runtime() {
        let (release, stalled) = std::sync::mpsc::channel::<()>();
        let started = std::time::Instant::now();

        drop(Offloaded(Some(Stalling(Some(stalled)))));

        assert!(started.elapsed() < Duration::from_secs(5));
        drop(release);
    }

    #[tokio::test]
    async fn a_plan_directory_that_answers_in_time_is_watched() {
        let watches = start_within(Duration::from_secs(5), Path::new("x.md"), || Some(())).await;

        assert_eq!(watches, Some(()));
    }

    #[test]
    fn the_expected_plan_is_the_plan_itself_without_a_worktree() {
        let branch = Branch("x".to_string());
        let expected = expected_plan(
            Path::new("/src/nhop"),
            Path::new("/src/nhop/docs/plans/x.md"),
            &branch,
            Worktree::No,
        );
        assert_eq!(expected, Path::new("/src/nhop/docs/plans/x.md"));
    }

    #[test]
    fn the_expected_plan_of_a_worktree_run_sits_under_its_branch() {
        let cases = [
            (
                "20260907-x",
                "/src/nhop/.ralphex/worktrees/20260907-x/docs/plans/x.md",
            ),
            (
                "feature/x",
                "/src/nhop/.ralphex/worktrees/feature/x/docs/plans/x.md",
            ),
        ];
        for (branch, expected) in cases {
            let branch = Branch(branch.to_string());
            let path = expected_plan(
                Path::new("/src/nhop"),
                Path::new("/src/nhop/docs/plans/x.md"),
                &branch,
                Worktree::Yes,
            );
            assert_eq!(path, Path::new(expected));
        }
    }

    #[test]
    fn a_plan_outside_the_checkout_is_expected_where_it_is() {
        let branch = Branch("x".to_string());
        let expected = expected_plan(
            Path::new("/src/nhop"),
            Path::new("/elsewhere/x.md"),
            &branch,
            Worktree::Yes,
        );
        assert_eq!(expected, Path::new("/elsewhere/x.md"));
    }

    #[test]
    fn resolve_prefers_the_expected_plan_then_its_completed_copy() {
        let dir = tempfile::tempdir().unwrap();
        let expected = dir.path().join("x.md");
        let completed = dir.path().join("completed").join("x.md");
        assert_eq!(resolve(&expected), None);

        std::fs::create_dir(dir.path().join("completed")).unwrap();
        std::fs::write(&completed, "# x\n").unwrap();
        assert_eq!(resolve(&expected), Some(completed.clone()));

        std::fs::write(&expected, "# x\n").unwrap();
        assert_eq!(resolve(&expected), Some(expected.clone()));
    }

    #[test]
    fn resolve_skips_a_directory_named_like_the_plan() {
        let dir = tempfile::tempdir().unwrap();
        let expected = dir.path().join("x.md");
        std::fs::create_dir(&expected).unwrap();
        assert_eq!(resolve(&expected), None);
    }

    #[test]
    fn a_snapshot_maps_the_parsed_tasks_one_to_one() {
        let dir = tempfile::tempdir().unwrap();
        let expected = dir.path().join("x.md");
        std::fs::write(
            &expected,
            "# x\n### Task 1: Add it\n- [x] write it\n### Task 2: Wire it\n",
        )
        .unwrap();

        let request = snapshot(&expected, Phase::Tasks, Outcome::Failed);

        assert_eq!(
            request,
            ProgressRequest {
                phase: Phase::Tasks,
                failed: true,
                tasks: Some(vec![
                    ProgressTask {
                        number: "1".to_string(),
                        ord: 0,
                        title: "Add it".to_string(),
                        status: TaskStatus::Done,
                        checkboxes: vec![ProgressCheckbox {
                            text: "write it".to_string(),
                            checked: true,
                        }],
                    },
                    ProgressTask {
                        number: "2".to_string(),
                        ord: 1,
                        title: "Wire it".to_string(),
                        status: TaskStatus::Pending,
                        checkboxes: Vec::new(),
                    },
                ]),
            }
        );
    }

    #[test]
    fn a_snapshot_has_no_tasks_when_the_plan_is_missing_unreadable_or_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.md");
        let unreadable = dir.path().join("unreadable.md");
        std::fs::write(&unreadable, [0xff, 0xfe, 0x00]).unwrap();
        let empty = dir.path().join("empty.md");
        std::fs::write(&empty, "# only a title\n").unwrap();

        for path in [missing, unreadable, empty] {
            let request = snapshot(&path, Phase::Setup, Outcome::Running);
            assert_eq!(
                request,
                ProgressRequest {
                    phase: Phase::Setup,
                    failed: false,
                    tasks: None,
                },
                "{}",
                path.display()
            );
        }
    }

    #[test]
    fn changes_to_a_file_named_like_the_plan_are_interesting() {
        let name = OsStr::new("x.md");
        let cases = [
            EventKind::Create(CreateKind::File),
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
            EventKind::Remove(RemoveKind::File),
        ];
        for kind in cases {
            assert!(
                names_plan(&event(kind, "/private/var/t/docs/plans/x.md"), name),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn other_kinds_and_other_files_are_not_interesting() {
        let name = OsStr::new("x.md");
        let kinds = [
            EventKind::Any,
            EventKind::Other,
            EventKind::Modify(ModifyKind::Any),
            EventKind::Modify(ModifyKind::Other),
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
        ];
        for kind in kinds {
            assert!(!names_plan(&event(kind, "/t/x.md"), name), "{kind:?}");
        }
        let other = event(EventKind::Create(CreateKind::File), "/t/x.md.tmp");
        assert!(!names_plan(&other, name));
        let directory = event(EventKind::Create(CreateKind::Folder), "/t/completed");
        assert!(!names_plan(&directory, name));
    }

    #[test]
    fn the_nearest_ancestor_is_the_deepest_existing_directory_above() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("a");
        std::fs::create_dir(&existing).unwrap();
        let directory = existing.join("b").join("c");
        assert_eq!(nearest_ancestor(&directory), Some(existing.clone()));
        std::fs::create_dir_all(&directory).unwrap();
        assert_eq!(nearest_ancestor(&directory), Some(existing.join("b")));
    }

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
