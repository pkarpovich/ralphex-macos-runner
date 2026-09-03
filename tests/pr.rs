//! The pull request a finished run leaves behind.

mod support;

use std::path::{Path, PathBuf};

use ralphex_macos_runner::pr::{PrError, PrSpec, PrTools, PrUrl, RunOrigin, open_pull_request};
use ralphex_macos_runner::protocol::types::{Branch, RunId};
use support::{Invocation, fake_gh, fake_git, invocations};
use tempfile::TempDir;

fn checkout() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("record");
    (dir, record)
}

fn tools(record: &Path) -> PrTools {
    PrTools {
        git: fake_git().display().to_string(),
        gh: fake_gh().display().to_string(),
        env: vec![("FAKE_RECORD".to_string(), record.display().to_string())],
    }
}

fn with(tools: &mut PrTools, key: &str, value: &str) {
    tools.env.push((key.to_string(), value.to_string()));
}

fn spec() -> PrSpec {
    PrSpec::describe(
        Branch("farm-runner".to_string()),
        &RunOrigin::Local,
        "/abs/checkout/docs/plans/farm-runner.md",
        &RunId("local-1".to_string()),
    )
}

fn programs(runs: &[Invocation]) -> Vec<String> {
    let mut named = Vec::new();
    for run in runs {
        let mut name = run.program.clone();
        for (taken, arg) in run.args.iter().enumerate() {
            if taken == 2 {
                break;
            }
            name.push(' ');
            name.push_str(arg);
        }
        named.push(name);
    }
    named
}

#[tokio::test]
async fn a_branch_without_a_pull_request_is_pushed_and_opened() {
    let (dir, record) = checkout();
    let tools = tools(&record);

    let url = open_pull_request(dir.path(), &spec(), &tools)
        .await
        .unwrap();

    assert_eq!(
        url,
        PrUrl("https://github.com/owner/repo/pull/7".to_string())
    );
    let runs = invocations(&record);
    assert_eq!(
        programs(&runs),
        vec![
            "gh pr list".to_string(),
            "git push -u".to_string(),
            "git symbolic-ref --short".to_string(),
            "gh pr create".to_string(),
        ]
    );
    for run in &runs {
        assert_eq!(
            run.cwd,
            dir.path().canonicalize().unwrap().display().to_string()
        );
    }
    let listed = &runs[0];
    assert_eq!(
        listed.args,
        vec![
            "pr".to_string(),
            "list".to_string(),
            "--head".to_string(),
            "farm-runner".to_string(),
            "--state".to_string(),
            "open".to_string(),
            "--json".to_string(),
            "url".to_string(),
            "--jq".to_string(),
            ".[0].url".to_string(),
        ]
    );
    let pushed = &runs[1];
    assert_eq!(
        pushed.args,
        vec![
            "push".to_string(),
            "-u".to_string(),
            "origin".to_string(),
            "farm-runner".to_string(),
        ]
    );
    let created = &runs[3];
    assert!(created.starts_with(&[
        "pr",
        "create",
        "--head",
        "farm-runner",
        "--base",
        "main",
        "--title",
        "farm-runner",
        "--body",
    ]));
    assert!(created.args[9].contains("Plan: /abs/checkout/docs/plans/farm-runner.md"));
    assert!(created.args[9].contains("Run: local-1"));
}

#[tokio::test]
async fn an_existing_pull_request_is_updated_and_reported() {
    let (dir, record) = checkout();
    let mut tools = tools(&record);
    with(
        &mut tools,
        "FAKE_EXISTING_PR",
        "https://github.com/owner/repo/pull/3",
    );

    let url = open_pull_request(dir.path(), &spec(), &tools)
        .await
        .unwrap();

    assert_eq!(
        url,
        PrUrl("https://github.com/owner/repo/pull/3".to_string())
    );
    let runs = invocations(&record);
    assert_eq!(
        programs(&runs),
        vec!["gh pr list".to_string(), "git push origin".to_string()]
    );
    let pushed = &runs[1];
    assert_eq!(
        pushed.args,
        vec![
            "push".to_string(),
            "origin".to_string(),
            "farm-runner".to_string(),
        ]
    );
}

#[tokio::test]
async fn the_base_falls_back_to_the_repository_view() {
    let (dir, record) = checkout();
    let mut tools = tools(&record);
    with(&mut tools, "FAKE_FAIL", "symbolic-ref");

    open_pull_request(dir.path(), &spec(), &tools)
        .await
        .unwrap();

    let runs = invocations(&record);
    assert_eq!(
        programs(&runs),
        vec![
            "gh pr list".to_string(),
            "git push -u".to_string(),
            "git symbolic-ref --short".to_string(),
            "gh repo view".to_string(),
            "gh pr create".to_string(),
        ]
    );
    let created = &runs[4];
    assert!(created.starts_with(&["pr", "create", "--head", "farm-runner", "--base", "main"]));
}

#[tokio::test]
async fn a_failed_lookup_is_a_list_error() {
    let (dir, record) = checkout();
    let mut tools = tools(&record);
    with(&mut tools, "FAKE_FAIL", "list");

    let error = open_pull_request(dir.path(), &spec(), &tools)
        .await
        .unwrap_err();

    match error {
        PrError::List(message) => assert!(message.contains("fake gh list failed")),
        other => panic!("{other} is not a list error"),
    }
    assert_eq!(
        programs(&invocations(&record)),
        vec!["gh pr list".to_string()]
    );
}

#[tokio::test]
async fn a_failed_push_is_a_push_error_with_the_farms_name() {
    let (dir, record) = checkout();
    let mut tools = tools(&record);
    with(&mut tools, "FAKE_FAIL", "push");

    let error = open_pull_request(dir.path(), &spec(), &tools)
        .await
        .unwrap_err();

    assert_eq!(error.fail_reason(), "git_push");
    match error {
        PrError::Push(message) => assert!(message.contains("fake git push failed")),
        other => panic!("{other} is not a push error"),
    }
}

#[tokio::test]
async fn a_base_neither_command_names_is_a_base_error() {
    let (dir, record) = checkout();
    let mut tools = tools(&record);
    with(&mut tools, "FAKE_FAIL", "symbolic-ref repo-view");

    let error = open_pull_request(dir.path(), &spec(), &tools)
        .await
        .unwrap_err();

    assert_eq!(error.fail_reason(), "pr_create");
    match error {
        PrError::Base(message) => assert!(message.contains("fake gh repo-view failed")),
        other => panic!("{other} is not a base error"),
    }
}

#[tokio::test]
async fn a_failed_creation_is_a_create_error() {
    let (dir, record) = checkout();
    let mut tools = tools(&record);
    with(&mut tools, "FAKE_FAIL", "create");

    let error = open_pull_request(dir.path(), &spec(), &tools)
        .await
        .unwrap_err();

    match error {
        PrError::Create(message) => assert!(message.contains("fake gh create failed")),
        other => panic!("{other} is not a create error"),
    }
}

#[tokio::test]
async fn an_empty_lookup_is_no_pull_request_whether_it_prints_null_or_nothing() {
    for printed in ["null", ""] {
        let (dir, record) = checkout();
        let mut tools = tools(&record);
        with(&mut tools, "FAKE_NO_PR_OUTPUT", printed);

        let url = open_pull_request(dir.path(), &spec(), &tools)
            .await
            .unwrap();

        assert_eq!(
            url,
            PrUrl("https://github.com/owner/repo/pull/7".to_string()),
            "{printed} was taken for an existing pull request"
        );
        assert_eq!(
            programs(&invocations(&record)),
            vec![
                "gh pr list".to_string(),
                "git push -u".to_string(),
                "git symbolic-ref --short".to_string(),
                "gh pr create".to_string(),
            ]
        );
    }
}

#[tokio::test]
async fn a_creation_that_prints_no_url_is_a_create_error() {
    let (dir, record) = checkout();
    let mut tools = tools(&record);
    with(&mut tools, "FAKE_SILENT_CREATE", "1");

    let error = open_pull_request(dir.path(), &spec(), &tools)
        .await
        .unwrap_err();

    assert_eq!(error.fail_reason(), "pr_create");
    match error {
        PrError::Create(message) => assert_eq!(message, "gh pr create printed no pull request URL"),
        other => panic!("{other} is not a create error"),
    }
}

#[tokio::test]
async fn a_blank_origin_head_falls_back_to_the_repository_view() {
    let (dir, record) = checkout();
    let mut tools = tools(&record);
    with(&mut tools, "FAKE_ORIGIN_HEAD", "");
    with(&mut tools, "FAKE_DEFAULT_BRANCH", "trunk");

    open_pull_request(dir.path(), &spec(), &tools)
        .await
        .unwrap();

    let runs = invocations(&record);
    assert_eq!(
        programs(&runs),
        vec![
            "gh pr list".to_string(),
            "git push -u".to_string(),
            "git symbolic-ref --short".to_string(),
            "gh repo view".to_string(),
            "gh pr create".to_string(),
        ]
    );
    let created = &runs[4];
    assert!(created.starts_with(&["pr", "create", "--head", "farm-runner", "--base", "trunk"]));
}

#[tokio::test]
async fn a_base_the_repository_view_leaves_blank_is_a_base_error() {
    let (dir, record) = checkout();
    let mut tools = tools(&record);
    with(&mut tools, "FAKE_ORIGIN_HEAD", "");
    with(&mut tools, "FAKE_DEFAULT_BRANCH", "");

    let error = open_pull_request(dir.path(), &spec(), &tools)
        .await
        .unwrap_err();

    assert_eq!(error.fail_reason(), "pr_create");
    match error {
        PrError::Base(message) => {
            assert_eq!(message, "neither git nor gh named the default branch");
        }
        other => panic!("{other} is not a base error"),
    }
}

#[tokio::test]
async fn a_missing_binary_is_reported_rather_than_panicking() {
    let (dir, record) = checkout();
    let mut tools = tools(&record);
    tools.gh = dir.path().join("absent-gh").display().to_string();

    let error = open_pull_request(dir.path(), &spec(), &tools)
        .await
        .unwrap_err();

    match error {
        PrError::List(message) => assert!(message.contains("could not be run")),
        other => panic!("{other} is not a list error"),
    }
}

#[tokio::test]
async fn a_ticket_run_carries_its_ticket_into_the_title_and_the_body() {
    let (dir, record) = checkout();
    let tools = tools(&record);
    let origin = RunOrigin::Ticket {
        identifier: "FARM-12".to_string(),
        issue_url: "https://linear.app/example/issue/FARM-12".to_string(),
        title: "split farm and runner".to_string(),
    };
    let spec = PrSpec::describe(
        Branch("farm-runner".to_string()),
        &origin,
        "/abs/checkout/docs/plans/x.md",
        &RunId("FARM-12-1".to_string()),
    );

    open_pull_request(dir.path(), &spec, &tools).await.unwrap();

    let runs = invocations(&record);
    let created = &runs[3];
    assert!(created.starts_with(&["pr", "create"]));
    assert_eq!(created.args[7], "FARM-12: split farm and runner");
    assert!(
        created.args[9].contains("Resolves FARM-12 (https://linear.app/example/issue/FARM-12)")
    );
    assert!(created.args[9].contains("Automated by ralphex-macos-runner."));
}
