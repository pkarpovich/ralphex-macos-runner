//! Spawning, draining and stopping a ralphex run.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nix::sys::signal::kill;
use nix::unistd::Pid;
use ralphex_macos_runner::job::{JobSpec, LocalOptions, Review, Worktree, spawn, validate};
use ralphex_macos_runner::logstream::LogStream;
use ralphex_macos_runner::protocol::client::FarmClient;
use ralphex_macos_runner::protocol::types::{Branch, MAX_LOG_CHUNK, RunId};
use support::fake_farm::FakeFarm;
use support::{Record, TestSleeper, TickHandle, fake_ralphex, manual_ticker};
use tempfile::TempDir;

fn stream(farm: &FakeFarm) -> (Arc<LogStream>, TickHandle) {
    let client = Arc::new(
        FarmClient::new(farm.url(), "secret-token", Arc::new(TestSleeper::new())).unwrap(),
    );
    let (ticker, handle) = manual_ticker();
    let stream = LogStream::new(client, RunId("local-1".to_string()), ticker);
    (Arc::new(stream), handle)
}

fn checkout() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let plan = dir.path().join("plan.md");
    std::fs::write(&plan, "# plan\n").unwrap();
    (dir, plan)
}

fn git_init(dir: &Path) {
    let status = std::process::Command::new("git")
        .arg("init")
        .arg("--quiet")
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success());
}

fn spec(ctx: &Path, plan: &Path) -> JobSpec {
    JobSpec {
        ctx: ctx.to_path_buf(),
        plan: plan.to_path_buf(),
        branch: Branch("x".to_string()),
        review: Review::No,
        local: LocalOptions::default(),
        ralphex_bin: fake_ralphex().display().to_string(),
    }
}

fn with_env(spec: &mut JobSpec, key: &str, value: &str) {
    spec.local.env.push((key.to_string(), value.to_string()));
}

fn wait_for(deadline: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    loop {
        if ready() {
            return true;
        }
        if started.elapsed() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

#[tokio::test]
async fn the_argv_carries_the_branch_the_flags_and_the_plan() {
    let farm = FakeFarm::start().await;
    let (log, _handle) = stream(&farm);
    let (dir, plan) = checkout();
    let record = dir.path().join("record");
    let mut spec = spec(dir.path(), &plan);
    spec.review = Review::Yes;
    spec.local.worktree = Worktree::Yes;
    with_env(
        &mut spec,
        "FAKE_RALPHEX_RECORD",
        &record.display().to_string(),
    );

    let mut job = spawn(&spec, Arc::clone(&log)).unwrap();
    let exited = job.wait().await.unwrap();

    assert!(exited.success());
    let record = Record::read(&record);
    assert_eq!(
        record.argv,
        vec![
            "--branch".to_string(),
            "x".to_string(),
            "--worktree".to_string(),
            "--review".to_string(),
            plan.display().to_string(),
        ]
    );
    assert_eq!(
        record.cwd,
        dir.path().canonicalize().unwrap().display().to_string()
    );
    log.close().await;
}

#[tokio::test]
async fn an_ordinary_run_asks_for_neither_a_worktree_nor_a_review() {
    let farm = FakeFarm::start().await;
    let (log, _handle) = stream(&farm);
    let (dir, plan) = checkout();
    let record = dir.path().join("record");
    let mut spec = spec(dir.path(), &plan);
    with_env(
        &mut spec,
        "FAKE_RALPHEX_RECORD",
        &record.display().to_string(),
    );

    let mut job = spawn(&spec, Arc::clone(&log)).unwrap();
    job.wait().await.unwrap();

    let record = Record::read(&record);
    assert_eq!(
        record.argv,
        vec![
            "--branch".to_string(),
            "x".to_string(),
            plan.display().to_string(),
        ]
    );
    log.close().await;
}

#[tokio::test]
async fn a_local_environment_entry_reaches_the_run() {
    let farm = FakeFarm::start().await;
    let (log, _handle) = stream(&farm);
    let (dir, plan) = checkout();
    let record = dir.path().join("record");
    let mut spec = spec(dir.path(), &plan);
    with_env(
        &mut spec,
        "FAKE_RALPHEX_RECORD",
        &record.display().to_string(),
    );
    with_env(&mut spec, "CLAUDE_CONFIG_DIR", "/tmp/work-profile");

    let mut job = spawn(&spec, Arc::clone(&log)).unwrap();
    job.wait().await.unwrap();

    let record = Record::read(&record);
    assert_eq!(
        record.env_value("CLAUDE_CONFIG_DIR"),
        Some("/tmp/work-profile".to_string())
    );
    assert_eq!(record.env_value("RALPHEX_CONFIG_DIR"), None);
    log.close().await;
}

#[tokio::test]
async fn both_pipes_reach_the_log_stream() {
    let farm = FakeFarm::start().await;
    let (log, _handle) = stream(&farm);
    let (dir, plan) = checkout();
    let mut spec = spec(dir.path(), &plan);
    with_env(&mut spec, "FAKE_RALPHEX_LINES", "2");

    let mut job = spawn(&spec, Arc::clone(&log)).unwrap();
    job.wait().await.unwrap();

    let tail = log.tail();
    for expected in ["out 1", "err 1", "out 2", "err 2"] {
        assert!(tail.contains(expected), "{tail} is missing {expected}");
    }
    log.close().await;

    let mut delivered = Vec::new();
    for request in farm.requests_ending("/log") {
        delivered.extend(request.body);
    }
    let delivered = String::from_utf8(delivered).unwrap();
    assert!(delivered.contains("out 2"));
    assert!(delivered.contains("err 2"));
}

#[tokio::test]
async fn a_nonzero_exit_code_propagates() {
    let farm = FakeFarm::start().await;
    let (log, _handle) = stream(&farm);
    let (dir, plan) = checkout();
    let mut spec = spec(dir.path(), &plan);
    with_env(&mut spec, "FAKE_RALPHEX_EXIT", "3");

    let mut job = spawn(&spec, Arc::clone(&log)).unwrap();
    let exited = job.wait().await.unwrap();

    assert_eq!(exited.code(), Some(3));
    log.close().await;
}

#[tokio::test]
async fn a_megabyte_long_line_is_chunked_for_the_farm_and_split_for_subscribers() {
    let farm = FakeFarm::start().await;
    let (log, _handle) = stream(&farm);
    let (dir, plan) = checkout();
    let mut spec = spec(dir.path(), &plan);
    let length = 1024 * 1024;
    with_env(&mut spec, "FAKE_RALPHEX_LONG_LINE", &length.to_string());

    let mut job = spawn(&spec, Arc::clone(&log)).unwrap();
    job.wait().await.unwrap();

    let (replay, _live) = log.subscribe();
    let mut printed = 0;
    for line in &replay {
        assert!(line.len() <= MAX_LOG_CHUNK, "a line grew past the cap");
        printed += line.len();
    }
    assert_eq!(printed, length);
    assert_eq!(replay.len(), length / MAX_LOG_CHUNK + 1);

    log.close().await;
    let mut delivered = 0;
    for request in farm.requests_ending("/log") {
        assert!(
            request.body.len() <= MAX_LOG_CHUNK,
            "a chunk grew past the cap"
        );
        delivered += request.body.len();
    }
    assert_eq!(delivered, length + 1);
}

#[tokio::test]
async fn stopping_takes_the_whole_process_group_down() {
    let farm = FakeFarm::start().await;
    let (log, _handle) = stream(&farm);
    let (dir, plan) = checkout();
    let record = dir.path().join("record");
    let mut spec = spec(dir.path(), &plan);
    with_env(
        &mut spec,
        "FAKE_RALPHEX_RECORD",
        &record.display().to_string(),
    );
    with_env(&mut spec, "FAKE_RALPHEX_CHILD", "120");
    with_env(&mut spec, "FAKE_RALPHEX_SLEEP", "120");

    let mut job = spawn(&spec, Arc::clone(&log)).unwrap();
    let written = wait_for(Duration::from_secs(10), || {
        let Ok(contents) = std::fs::read_to_string(&record) else {
            return false;
        };
        contents.contains("child: ")
    });
    assert!(written, "the fake never recorded its child");

    let record = Record::read(&record);
    let Some(child) = record.child else {
        panic!("the fake recorded no child");
    };
    assert_eq!(job.pgid(), record.pid);

    job.stop(Duration::from_secs(5)).await.unwrap();

    let gone = wait_for(Duration::from_secs(10), || {
        !alive(record.pid) && !alive(child)
    });
    assert!(gone, "the process group outlived the stop");
    log.close().await;
}

#[tokio::test]
async fn a_missing_checkout_fails_validation() {
    let (dir, plan) = checkout();
    let mut spec = spec(dir.path(), &plan);
    spec.ctx = dir.path().join("absent");

    let error = validate(&spec).await.unwrap_err();

    assert_eq!(error.fail_reason(), "ctx_invalid");
}

#[tokio::test]
async fn a_checkout_without_a_repository_fails_validation() {
    let (dir, plan) = checkout();
    let spec = spec(dir.path(), &plan);

    let error = validate(&spec).await.unwrap_err();

    assert_eq!(error.fail_reason(), "ctx_invalid");
}

#[tokio::test]
async fn a_plan_outside_the_checkout_fails_validation() {
    let (dir, _plan) = checkout();
    git_init(dir.path());
    let outside = tempfile::tempdir().unwrap();
    let plan = outside.path().join("plan.md");
    std::fs::write(&plan, "# plan\n").unwrap();
    let spec = spec(dir.path(), &plan);

    let error = validate(&spec).await.unwrap_err();

    assert_eq!(error.fail_reason(), "plan_not_found");
}

#[tokio::test]
async fn a_missing_plan_fails_validation() {
    let (dir, _plan) = checkout();
    git_init(dir.path());
    let spec = spec(dir.path(), &dir.path().join("absent.md"));

    let error = validate(&spec).await.unwrap_err();

    assert_eq!(error.fail_reason(), "plan_not_found");
}

#[tokio::test]
async fn a_plan_inside_a_git_checkout_passes_validation() {
    let (dir, plan) = checkout();
    git_init(dir.path());
    let spec = spec(dir.path(), &plan);

    validate(&spec).await.unwrap();
}
