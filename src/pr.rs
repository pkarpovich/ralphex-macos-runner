//! The pull request a finished run leaves behind.
//!
//! [`open_pull_request`] runs the four-step sequence in the checkout: it asks
//! `gh` for an open pull request on the branch, pushes the branch, resolves the
//! base branch and calls `gh pr create`. A branch that already has a pull
//! request is only pushed, and the existing URL is reported. [`PrSpec::describe`]
//! builds the title and the body a run gets, for a ticket job and for a local
//! run alike.

use std::path::Path;
use std::process::{Output, Stdio};

use tokio::process::Command;

use crate::protocol::types::{Branch, RunId};

/// The URL of a pull request.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PrUrl(pub String);

impl PrUrl {
    /// Returns the URL as a string slice.
    ///
    /// # Examples
    ///
    /// ```
    /// use ralphex_macos_runner::pr::PrUrl;
    ///
    /// let url = PrUrl("https://github.com/owner/repo/pull/7".to_string());
    /// assert_eq!(url.as_str(), "https://github.com/owner/repo/pull/7");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PrUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Who opened the run a pull request describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOrigin {
    /// A Linear ticket opened the run.
    Ticket {
        /// The ticket's human identifier, such as `FARM-12`.
        identifier: String,
        /// The ticket's URL, empty when the farm sent none.
        issue_url: String,
        /// The ticket's title.
        title: String,
    },
    /// A local `rxd` invocation opened the run.
    Local,
}

/// The programs the pull-request sequence runs and the environment they see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrTools {
    /// The git binary, resolved through the daemon's `PATH` by default.
    pub git: String,
    /// The GitHub CLI binary, resolved through the daemon's `PATH` by default.
    pub gh: String,
    /// Environment entries added to the daemon's own.
    pub env: Vec<(String, String)>,
}

impl Default for PrTools {
    fn default() -> Self {
        PrTools {
            git: "git".to_string(),
            gh: "gh".to_string(),
            env: Vec::new(),
        }
    }
}

/// The branch, the title and the body one pull request is opened with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrSpec {
    /// The branch the run worked on.
    pub branch: Branch,
    /// The title of the pull request.
    pub title: String,
    /// The body of the pull request.
    pub body: String,
}

impl PrSpec {
    /// Returns the title and the body a run of `origin` gets.
    ///
    /// # Examples
    ///
    /// A ticket job is titled after its ticket and resolves it:
    ///
    /// ```
    /// use ralphex_macos_runner::pr::{PrSpec, RunOrigin};
    /// use ralphex_macos_runner::protocol::types::{Branch, RunId};
    ///
    /// let origin = RunOrigin::Ticket {
    ///     identifier: "FARM-12".to_string(),
    ///     issue_url: "https://linear.app/example/issue/FARM-12".to_string(),
    ///     title: "split farm and runner".to_string(),
    /// };
    /// let spec = PrSpec::describe(
    ///     Branch("x".to_string()),
    ///     &origin,
    ///     "/abs/checkout/docs/plans/x.md",
    ///     &RunId("FARM-12-1".to_string()),
    /// );
    /// assert_eq!(spec.title, "FARM-12: split farm and runner");
    /// assert_eq!(
    ///     spec.body,
    ///     "Plan: /abs/checkout/docs/plans/x.md\n\nRun: FARM-12-1\n\nResolves FARM-12 (https://linear.app/example/issue/FARM-12)\n\nAutomated by ralphex-macos-runner."
    /// );
    /// ```
    ///
    /// A local run is titled after its plan and resolves nothing:
    ///
    /// ```
    /// use ralphex_macos_runner::pr::{PrSpec, RunOrigin};
    /// use ralphex_macos_runner::protocol::types::{Branch, RunId};
    ///
    /// let spec = PrSpec::describe(
    ///     Branch("x".to_string()),
    ///     &RunOrigin::Local,
    ///     "/abs/checkout/docs/plans/20260902-x.md",
    ///     &RunId("local-1".to_string()),
    /// );
    /// assert_eq!(spec.title, "20260902-x");
    /// assert_eq!(
    ///     spec.body,
    ///     "Plan: /abs/checkout/docs/plans/20260902-x.md\n\nRun: local-1\n\nAutomated by ralphex-macos-runner."
    /// );
    /// ```
    #[must_use]
    pub fn describe(branch: Branch, origin: &RunOrigin, plan: &str, run_id: &RunId) -> PrSpec {
        let (title, resolves) = match origin {
            RunOrigin::Ticket {
                identifier,
                issue_url,
                title,
            } => {
                let resolves = if issue_url.is_empty() {
                    format!("Resolves {identifier}")
                } else {
                    format!("Resolves {identifier} ({issue_url})")
                };
                (format!("{identifier}: {title}"), resolves)
            }
            RunOrigin::Local => (plan_stem(plan), String::new()),
        };
        let mut paragraphs = Vec::new();
        paragraphs.push(format!("Plan: {plan}"));
        paragraphs.push(format!("Run: {run_id}"));
        if !resolves.is_empty() {
            paragraphs.push(resolves);
        }
        paragraphs.push("Automated by ralphex-macos-runner.".to_string());
        PrSpec {
            branch,
            title,
            body: paragraphs.join("\n\n"),
        }
    }
}

/// Why a run that finished could not be turned into a pull request.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PrError {
    /// The existing pull request could not be looked up.
    #[error("the pull request could not be looked up: {0}")]
    List(String),
    /// The branch could not be pushed.
    #[error("the branch could not be pushed: {0}")]
    Push(String),
    /// The base branch could not be determined.
    #[error("the base branch could not be determined: {0}")]
    Base(String),
    /// The pull request could not be created.
    #[error("the pull request could not be created: {0}")]
    Create(String),
}

impl PrError {
    /// Returns the farm's machine-readable name for this failure.
    ///
    /// # Examples
    ///
    /// ```
    /// use ralphex_macos_runner::pr::PrError;
    ///
    /// assert_eq!(PrError::Push("denied".to_string()).fail_reason(), "git_push");
    /// assert_eq!(PrError::Create("denied".to_string()).fail_reason(), "pr_create");
    /// ```
    #[must_use]
    pub fn fail_reason(&self) -> &'static str {
        match self {
            PrError::List(_) => "pr_create",
            PrError::Push(_) => "git_push",
            PrError::Base(_) => "pr_create",
            PrError::Create(_) => "pr_create",
        }
    }
}

/// Pushes the branch of a finished run and reports its pull request.
///
/// A branch that already has an open pull request is pushed to update it and
/// that URL is reported; otherwise the branch is pushed with an upstream, the
/// base branch is resolved and `gh pr create` opens the pull request.
///
/// # Errors
///
/// Returns [`PrError::List`] when the lookup fails, [`PrError::Push`] when the
/// push fails, [`PrError::Base`] when neither git nor the GitHub CLI names the
/// default branch, and [`PrError::Create`] when the creation fails or prints no
/// URL.
pub async fn open_pull_request(
    ctx: &Path,
    spec: &PrSpec,
    tools: &PrTools,
) -> Result<PrUrl, PrError> {
    let PrSpec {
        branch,
        title,
        body,
    } = spec;
    let PrTools { git, gh, env } = tools;
    let branch = branch.as_str();

    let listed = match step(
        ctx,
        gh,
        &[
            "pr", "list", "--head", branch, "--state", "open", "--json", "url", "--jq", ".[0].url",
        ],
        env,
    )
    .await
    {
        Ok(listed) => listed,
        Err(message) => return Err(PrError::List(message)),
    };
    let listed = listed.trim();
    let existing = match listed {
        "" | "null" => None,
        url => Some(PrUrl(url.to_string())),
    };

    if let Some(existing) = existing {
        match step(ctx, git, &["push", "origin", branch], env).await {
            Ok(_pushed) => {}
            Err(message) => return Err(PrError::Push(message)),
        }
        return Ok(existing);
    }

    match step(ctx, git, &["push", "-u", "origin", branch], env).await {
        Ok(_pushed) => {}
        Err(message) => return Err(PrError::Push(message)),
    }

    let base = resolve_base(ctx, tools).await?;

    let created = match step(
        ctx,
        gh,
        &[
            "pr", "create", "--head", branch, "--base", &base, "--title", title, "--body", body,
        ],
        env,
    )
    .await
    {
        Ok(created) => created,
        Err(message) => return Err(PrError::Create(message)),
    };
    let mut url = String::new();
    for line in created.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        url = line.to_string();
    }
    if url.is_empty() {
        return Err(PrError::Create(
            "gh pr create printed no pull request URL".to_string(),
        ));
    }
    Ok(PrUrl(url))
}

async fn resolve_base(ctx: &Path, tools: &PrTools) -> Result<String, PrError> {
    let PrTools { git, gh, env } = tools;
    let symbolic = step(
        ctx,
        git,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        env,
    )
    .await;
    if let Ok(base) = symbolic {
        let base = base.trim();
        let base = match base.strip_prefix("origin/") {
            Some(base) => base,
            None => base,
        };
        if !base.is_empty() {
            return Ok(base.to_string());
        }
    }
    let viewed = step(
        ctx,
        gh,
        &[
            "repo",
            "view",
            "--json",
            "defaultBranchRef",
            "--jq",
            ".defaultBranchRef.name",
        ],
        env,
    )
    .await;
    let base = match viewed {
        Ok(base) => base,
        Err(message) => return Err(PrError::Base(message)),
    };
    let base = base.trim();
    if base.is_empty() {
        return Err(PrError::Base(
            "neither git nor gh named the default branch".to_string(),
        ));
    }
    Ok(base.to_string())
}

async fn step(
    ctx: &Path,
    program: &str,
    args: &[&str],
    env: &[(String, String)],
) -> Result<String, String> {
    let mut command = Command::new(program);
    for arg in args {
        command.arg(arg);
    }
    command.current_dir(ctx);
    for (key, value) in env {
        command.env(key, value);
    }
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let label = label(program, args);
    let output = match command.output().await {
        Ok(output) => output,
        Err(error) => return Err(format!("{label} could not be run: {error}")),
    };
    let Output {
        status,
        stdout,
        stderr,
    } = output;
    let stdout = String::from_utf8_lossy(&stdout).into_owned();
    if status.success() {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&stderr).into_owned();
    let stderr = stderr.trim();
    let code = match status.code() {
        Some(code) => code.to_string(),
        None => "a signal".to_string(),
    };
    Err(format!("{label} exited with {code}: {stderr}"))
}

fn label(program: &str, args: &[&str]) -> String {
    let mut label = program.to_string();
    for (taken, arg) in args.iter().enumerate() {
        if taken == 2 || arg.starts_with('-') {
            break;
        }
        label.push(' ');
        label.push_str(arg);
    }
    label
}

fn plan_stem(plan: &str) -> String {
    let Some(stem) = Path::new(plan).file_stem() else {
        return plan.to_string();
    };
    stem.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket() -> RunOrigin {
        RunOrigin::Ticket {
            identifier: "FARM-12".to_string(),
            issue_url: "https://linear.app/example/issue/FARM-12".to_string(),
            title: "split farm and runner".to_string(),
        }
    }

    #[test]
    fn a_ticket_job_is_titled_after_its_ticket() {
        let PrSpec {
            branch,
            title,
            body,
        } = PrSpec::describe(
            Branch("farm-runner".to_string()),
            &ticket(),
            "/abs/checkout/docs/plans/x.md",
            &RunId("FARM-12-1753180800000".to_string()),
        );
        assert_eq!(branch, Branch("farm-runner".to_string()));
        assert_eq!(title, "FARM-12: split farm and runner");
        assert_eq!(
            body,
            concat!(
                "Plan: /abs/checkout/docs/plans/x.md\n",
                "\n",
                "Run: FARM-12-1753180800000\n",
                "\n",
                "Resolves FARM-12 (https://linear.app/example/issue/FARM-12)\n",
                "\n",
                "Automated by ralphex-macos-runner.",
            )
        );
    }

    #[test]
    fn a_ticket_without_a_url_resolves_the_bare_identifier() {
        let origin = RunOrigin::Ticket {
            identifier: "FARM-12".to_string(),
            issue_url: String::new(),
            title: "split farm and runner".to_string(),
        };
        let PrSpec {
            branch: _,
            title: _,
            body,
        } = PrSpec::describe(
            Branch("x".to_string()),
            &origin,
            "/abs/plan.md",
            &RunId("FARM-12-1".to_string()),
        );
        assert!(body.contains("\nResolves FARM-12\n"));
    }

    #[test]
    fn a_local_run_is_titled_after_its_plan_and_resolves_nothing() {
        let PrSpec {
            branch: _,
            title,
            body,
        } = PrSpec::describe(
            Branch("x".to_string()),
            &RunOrigin::Local,
            "/abs/checkout/docs/plans/20260902-ralphex-macos-runner.md",
            &RunId("local-1753180800000".to_string()),
        );
        assert_eq!(title, "20260902-ralphex-macos-runner");
        assert_eq!(
            body,
            concat!(
                "Plan: /abs/checkout/docs/plans/20260902-ralphex-macos-runner.md\n",
                "\n",
                "Run: local-1753180800000\n",
                "\n",
                "Automated by ralphex-macos-runner.",
            )
        );
    }

    #[test]
    fn a_plan_without_a_stem_titles_the_run_with_the_path() {
        assert_eq!(plan_stem("/"), "/");
        assert_eq!(plan_stem("plan.md"), "plan");
    }

    #[test]
    fn every_failure_carries_the_farms_name_for_it() {
        assert_eq!(PrError::List(String::new()).fail_reason(), "pr_create");
        assert_eq!(PrError::Push(String::new()).fail_reason(), "git_push");
        assert_eq!(PrError::Base(String::new()).fail_reason(), "pr_create");
        assert_eq!(PrError::Create(String::new()).fail_reason(), "pr_create");
    }

    #[test]
    fn a_label_names_the_command_without_its_flags() {
        assert_eq!(
            label("gh", &["pr", "create", "--head", "x"]),
            "gh pr create"
        );
        assert_eq!(label("git", &["push", "-u", "origin", "x"]), "git push");
        assert_eq!(
            label(
                "git",
                &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]
            ),
            "git symbolic-ref"
        );
    }

    #[test]
    fn the_default_tools_are_the_ones_on_the_path() {
        let PrTools { git, gh, env } = PrTools::default();
        assert_eq!(git, "git");
        assert_eq!(gh, "gh");
        assert!(env.is_empty());
    }
}
