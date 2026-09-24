use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ralphex_macos_runner::progress::{PlanWatcher, ProgressSender, WatchTimings};
use ralphex_macos_runner::protocol::client::FarmError;
use ralphex_macos_runner::protocol::types::{
    Phase, ProgressCheckbox, ProgressRequest, ProgressTask, RunId, TaskStatus,
};

const FAST: WatchTimings = WatchTimings {
    debounce: Duration::from_millis(20),
    attach_retry: Duration::from_millis(50),
};

const WAIT: Duration = Duration::from_secs(30);

const PLAN: &str = "# Require dials\n\n### Task 1: Add it\n- [ ] write it\n- [ ] test it\n";

const TICKED: &str = "# Require dials\n\n### Task 1: Add it\n- [x] write it\n- [x] test it\n";

#[derive(Default)]
struct Recorder {
    posts: Mutex<Vec<(RunId, ProgressRequest)>>,
    failures: Mutex<u32>,
    latency: Duration,
}

impl Recorder {
    fn failing(failures: u32) -> Recorder {
        Recorder {
            posts: Mutex::new(Vec::new()),
            failures: Mutex::new(failures),
            latency: Duration::ZERO,
        }
    }

    fn slow(latency: Duration) -> Recorder {
        Recorder {
            posts: Mutex::new(Vec::new()),
            failures: Mutex::new(0),
            latency,
        }
    }

    fn posts(&self) -> Vec<ProgressRequest> {
        let posts = self.posts.lock().unwrap();
        let mut requests = Vec::new();
        for (_, request) in posts.iter() {
            requests.push(request.clone());
        }
        requests
    }

    async fn wait_for(&self, description: &str, done: impl Fn(&[ProgressRequest]) -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let posts = self.posts();
            if done(&posts) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{description}: {posts:#?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl ProgressSender for Recorder {
    fn post<'a>(
        &'a self,
        run_id: &'a RunId,
        request: &'a ProgressRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), FarmError>> + Send + 'a>> {
        Box::pin(async move {
            tokio::time::sleep(self.latency).await;
            let mut posts = self.posts.lock().unwrap();
            posts.push((run_id.clone(), request.clone()));
            drop(posts);
            let mut failures = self.failures.lock().unwrap();
            if *failures == 0 {
                return Ok(());
            }
            *failures -= 1;
            Err(FarmError::Transport("scripted failure".to_string()))
        })
    }
}

async fn start(recorder: &Arc<Recorder>, expected: &Path, timings: WatchTimings) -> PlanWatcher {
    let sender: Arc<dyn ProgressSender> = Arc::clone(recorder) as Arc<dyn ProgressSender>;
    PlanWatcher::start(
        sender,
        RunId("local-1".to_string()),
        expected.to_path_buf(),
        timings,
    )
    .await
}

fn plan_dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let plans = dir.path().join("docs").join("plans");
    std::fs::create_dir_all(&plans).unwrap();
    let expected = plans.join("20260907-require-dials.md");
    (dir, expected)
}

fn statuses(request: &ProgressRequest) -> Vec<TaskStatus> {
    let mut statuses = Vec::new();
    let Some(tasks) = &request.tasks else {
        return statuses;
    };
    for ProgressTask {
        number: _,
        ord: _,
        title: _,
        status,
        checkboxes: _,
    } in tasks
    {
        statuses.push(*status);
    }
    statuses
}

fn last_statuses(posts: &[ProgressRequest]) -> Vec<TaskStatus> {
    let Some(last) = posts.last() else {
        return Vec::new();
    };
    statuses(last)
}

#[tokio::test]
async fn the_first_post_is_setup_without_tasks_when_the_plan_is_absent() {
    let (_dir, expected) = plan_dir();
    let recorder = Arc::new(Recorder::default());

    let watcher = start(&recorder, &expected, FAST).await;
    recorder
        .wait_for("a setup post", |posts| !posts.is_empty())
        .await;
    let _stopped = watcher.stop().await;

    let posts = recorder.posts.lock().unwrap();
    assert_eq!(
        posts[0],
        (
            RunId("local-1".to_string()),
            ProgressRequest {
                phase: Phase::Setup,
                failed: false,
                tasks: None,
            }
        )
    );
}

#[tokio::test]
async fn creating_the_plan_posts_its_tasks_and_ticking_it_posts_the_new_status() {
    let (_dir, expected) = plan_dir();
    let recorder = Arc::new(Recorder::default());
    let watcher = start(&recorder, &expected, FAST).await;
    recorder
        .wait_for("a setup post", |posts| !posts.is_empty())
        .await;

    std::fs::write(&expected, PLAN).unwrap();
    recorder
        .wait_for("the created plan", |posts| {
            last_statuses(posts) == [TaskStatus::Pending]
        })
        .await;
    assert_eq!(
        recorder.posts().last().unwrap().tasks,
        Some(vec![ProgressTask {
            number: "1".to_string(),
            ord: 0,
            title: "Add it".to_string(),
            status: TaskStatus::Pending,
            checkboxes: vec![
                ProgressCheckbox {
                    text: "write it".to_string(),
                    checked: false,
                },
                ProgressCheckbox {
                    text: "test it".to_string(),
                    checked: false,
                },
            ],
        }])
    );

    std::fs::write(&expected, TICKED).unwrap();
    recorder
        .wait_for("the ticked plan", |posts| {
            last_statuses(posts) == [TaskStatus::Done]
        })
        .await;
    let _stopped = watcher.stop().await;
}

#[tokio::test]
async fn a_plan_moved_into_completed_keeps_posting_from_there() {
    let (_dir, expected) = plan_dir();
    std::fs::write(&expected, PLAN).unwrap();
    let recorder = Arc::new(Recorder::default());
    let watcher = start(&recorder, &expected, FAST).await;
    recorder
        .wait_for("a setup post", |posts| {
            last_statuses(posts) == [TaskStatus::Pending]
        })
        .await;

    let completed = expected.parent().unwrap().join("completed");
    std::fs::create_dir(&completed).unwrap();
    recorder
        .wait_for("a post once completed/ is watched", |posts| {
            posts.len() >= 2
        })
        .await;
    let before = recorder.posts().len();
    let moved = completed.join(expected.file_name().unwrap());
    std::fs::rename(&expected, &moved).unwrap();
    recorder
        .wait_for("a post after the move", |posts| posts.len() > before)
        .await;
    assert_eq!(
        last_statuses(&recorder.posts()),
        [TaskStatus::Pending],
        "the moved plan still resolves"
    );

    std::fs::write(&moved, TICKED).unwrap();
    recorder
        .wait_for("the ticked completed plan", |posts| {
            last_statuses(posts) == [TaskStatus::Done]
        })
        .await;
    let _stopped = watcher.stop().await;
}

#[tokio::test]
async fn a_plan_directory_created_after_start_attaches_and_posts() {
    let dir = tempfile::tempdir().unwrap();
    let expected = dir
        .path()
        .join(".ralphex")
        .join("worktrees")
        .join("feature")
        .join("x")
        .join("docs")
        .join("plans")
        .join("x.md");
    let recorder = Arc::new(Recorder::default());
    let watcher = start(&recorder, &expected, FAST).await;
    recorder
        .wait_for("a setup post", |posts| !posts.is_empty())
        .await;

    std::fs::create_dir_all(expected.parent().unwrap()).unwrap();
    std::fs::write(&expected, PLAN).unwrap();
    recorder
        .wait_for("the worktree plan", |posts| {
            last_statuses(posts) == [TaskStatus::Pending]
        })
        .await;

    std::fs::write(&expected, TICKED).unwrap();
    recorder
        .wait_for("the ticked worktree plan", |posts| {
            last_statuses(posts) == [TaskStatus::Done]
        })
        .await;
    let _stopped = watcher.stop().await;
}

#[tokio::test]
async fn a_burst_of_writes_inside_the_debounce_posts_once() {
    let (_dir, expected) = plan_dir();
    let recorder = Arc::new(Recorder::default());
    let timings = WatchTimings {
        debounce: Duration::from_millis(400),
        attach_retry: Duration::from_millis(50),
    };
    let watcher = start(&recorder, &expected, timings).await;
    recorder
        .wait_for("a setup post", |posts| !posts.is_empty())
        .await;

    for _ in 0..5 {
        std::fs::write(&expected, PLAN).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    std::fs::write(&expected, TICKED).unwrap();
    recorder
        .wait_for("the burst", |posts| posts.len() >= 2)
        .await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let _stopped = watcher.stop().await;

    let posts = recorder.posts();
    assert_eq!(posts.len(), 2, "{posts:#?}");
    assert_eq!(statuses(&posts[1]), [TaskStatus::Done]);
}

#[tokio::test]
async fn an_unreadable_or_task_less_plan_posts_no_tasks() {
    let cases: [&[u8]; 2] = [&[0xff, 0xfe, 0x00], b"# Only a title\n\nNo tasks here.\n"];
    for content in cases {
        let (_dir, expected) = plan_dir();
        std::fs::write(&expected, content).unwrap();
        let recorder = Arc::new(Recorder::default());

        let watcher = start(&recorder, &expected, FAST).await;
        recorder
            .wait_for("a setup post", |posts| !posts.is_empty())
            .await;
        watcher.tracker().request();
        recorder
            .wait_for("a requested post", |posts| posts.len() >= 2)
            .await;
        let _stopped = watcher.stop().await;

        for request in recorder.posts() {
            assert_eq!(request.tasks, None);
        }
    }
}

#[tokio::test]
async fn a_sender_error_does_not_stop_later_posts() {
    let (_dir, expected) = plan_dir();
    std::fs::write(&expected, PLAN).unwrap();
    let recorder = Arc::new(Recorder::failing(1));

    let watcher = start(&recorder, &expected, FAST).await;
    recorder
        .wait_for("a failed setup post", |posts| !posts.is_empty())
        .await;
    watcher.tracker().observe("--- task iteration 1 ---");
    recorder
        .wait_for("a post after the failure", |posts| posts.len() >= 2)
        .await;
    let _stopped = watcher.stop().await;

    let posts = recorder.posts();
    assert_eq!(posts[1].phase, Phase::Tasks);
    assert_eq!(statuses(&posts[1]), [TaskStatus::Pending]);
}

#[tokio::test]
async fn the_markers_move_the_phase_of_the_posted_snapshots() {
    let (_dir, expected) = plan_dir();
    std::fs::write(&expected, PLAN).unwrap();
    let recorder = Arc::new(Recorder::default());
    let watcher = start(&recorder, &expected, FAST).await;
    recorder
        .wait_for("a setup post", |posts| !posts.is_empty())
        .await;

    let tracker = watcher.tracker();
    tracker.observe("--- task iteration 1 ---");
    recorder
        .wait_for("a tasks post", |posts| {
            posts.last().map(|post| post.phase) == Some(Phase::Tasks)
        })
        .await;
    tracker.observe("--- codex external review ---");
    recorder
        .wait_for("a review post", |posts| {
            posts.last().map(|post| post.phase) == Some(Phase::Review)
        })
        .await;
    let _stopped = watcher.stop().await;
}

#[tokio::test]
async fn post_phase_freezes_the_phase_and_is_the_last_post_after_stop() {
    let (_dir, expected) = plan_dir();
    std::fs::write(&expected, PLAN).unwrap();
    let recorder = Arc::new(Recorder::default());
    let watcher = start(&recorder, &expected, FAST).await;
    recorder
        .wait_for("a setup post", |posts| !posts.is_empty())
        .await;
    let tracker = watcher.tracker();

    let stopped = watcher.stop().await;
    stopped.post_phase(Phase::Pr).await;
    tracker.observe("--- task iteration 9 ---");
    tracker.observe("--- claude review 1 ---");
    std::fs::write(&expected, TICKED).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(tracker.phase(), Phase::Pr);
    let posts = recorder.posts();
    assert_eq!(posts.len(), 2, "{posts:#?}");
    assert_eq!(posts[1].phase, Phase::Pr);
    assert!(!posts[1].failed);
    assert_eq!(statuses(&posts[1]), [TaskStatus::Pending]);
}

#[tokio::test]
async fn a_stop_right_after_start_still_delivers_setup_before_the_last_post() {
    let (_dir, expected) = plan_dir();
    let recorder = Arc::new(Recorder::slow(Duration::from_millis(200)));
    let watcher = start(&recorder, &expected, FAST).await;

    let stopped = watcher.stop().await;
    stopped.post_phase(Phase::Pr).await;

    let posts = recorder.posts();
    assert_eq!(posts.len(), 2, "{posts:#?}");
    assert_eq!(posts[0].phase, Phase::Setup);
    assert_eq!(posts[1].phase, Phase::Pr);
}

#[tokio::test]
async fn post_failure_marks_the_run_failed_with_the_re_read_tasks() {
    let (_dir, expected) = plan_dir();
    std::fs::write(&expected, PLAN).unwrap();
    let recorder = Arc::new(Recorder::default());
    let watcher = start(&recorder, &expected, FAST).await;
    recorder
        .wait_for("a setup post", |posts| !posts.is_empty())
        .await;
    watcher.tracker().observe("--- task iteration 1 ---");
    recorder
        .wait_for("a tasks post", |posts| posts.len() >= 2)
        .await;

    let stopped = watcher.stop().await;
    std::fs::write(&expected, TICKED).unwrap();
    let before = recorder.posts().len();
    stopped.post_failure().await;

    let posts = recorder.posts();
    assert_eq!(posts.len(), before + 1, "{posts:#?}");
    let last = posts.last().unwrap();
    assert_eq!(last.phase, Phase::Tasks);
    assert!(last.failed);
    assert_eq!(statuses(last), [TaskStatus::Done]);
}
