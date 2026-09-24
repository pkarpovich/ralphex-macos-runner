//! The pull request a finished run leaves behind.
//!
//! [`open_pull_request`] runs the four-step sequence in the checkout: it asks
//! `gh` for an open pull request on the branch, pushes the branch, resolves the
//! base branch and calls `gh pr create`. A branch that already has a pull
//! request is only pushed, and the existing URL is reported. [`PrSpec::describe`]
//! builds the title and the body a run gets from the description finalize
//! wrote, or from [`prdesc::fallback_title`] without one, and appends
//! [`prdesc::footer`], for a ticket job and for a local run alike. Every step is bounded by [`PrTools::step_timeout`], because the
//! sequence runs after the terminal channel stops being read and while the run
//! slot is still held: a `git push` that hangs on the wire would otherwise wedge
//! the daemon until it is restarted by hand.

use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use tokio::process::Command;

use crate::prdesc::{self, Description};
use crate::protocol::types::{Branch, PR_STEP_TIMEOUT, RunId};

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
    Local {
        /// The name the farm gave the run.
        title: String,
    },
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
    /// The time one step may take before its process is killed and the step fails.
    pub step_timeout: Duration,
}

impl Default for PrTools {
    fn default() -> Self {
        PrTools {
            git: "git".to_string(),
            gh: "gh".to_string(),
            env: Vec::new(),
            step_timeout: PR_STEP_TIMEOUT,
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
    /// A description finalize `written` supplies both; without one the title is
    /// [`prdesc::fallback_title`] and the body is empty. The farm's
    /// [`prdesc::footer`] is appended to the body either way.
    ///
    /// # Examples
    ///
    /// A written description is used as it is:
    ///
    /// ```
    /// use ralphex_macos_runner::pr::{PrSpec, RunOrigin};
    /// use ralphex_macos_runner::prdesc::Description;
    /// use ralphex_macos_runner::protocol::types::{Branch, RunId};
    ///
    /// let origin = RunOrigin::Ticket {
    ///     identifier: "FARM-12".to_string(),
    ///     issue_url: "https://linear.app/example/issue/FARM-12".to_string(),
    ///     title: "split farm and runner".to_string(),
    /// };
    /// let written = Description {
    ///     title: "Split the farm from its runner".to_string(),
    ///     body: "The runner moves out.".to_string(),
    /// };
    /// let spec = PrSpec::describe(
    ///     Branch("x".to_string()),
    ///     &origin,
    ///     "/abs/checkout/docs/plans/x.md",
    ///     &RunId("FARM-12-1".to_string()),
    ///     Some(written),
    /// );
    /// assert_eq!(spec.title, "Split the farm from its runner");
    /// assert_eq!(
    ///     spec.body,
    ///     "The runner moves out.\n\n---\n\n[FARM-12](https://linear.app/example/issue/FARM-12) - plan: `/abs/checkout/docs/plans/x.md` - run `FARM-12-1`\n\nOpened automatically by ralphex-farm."
    /// );
    /// ```
    ///
    /// Without one a local run is titled after its name and carries only the footer:
    ///
    /// ```
    /// use ralphex_macos_runner::pr::{PrSpec, RunOrigin};
    /// use ralphex_macos_runner::protocol::types::{Branch, RunId};
    ///
    /// let origin = RunOrigin::Local {
    ///     title: "Require dials".to_string(),
    /// };
    /// let spec = PrSpec::describe(
    ///     Branch("x".to_string()),
    ///     &origin,
    ///     "/abs/checkout/docs/plans/20260902-x.md",
    ///     &RunId("local-1".to_string()),
    ///     None,
    /// );
    /// assert_eq!(spec.title, "Require dials");
    /// assert_eq!(
    ///     spec.body,
    ///     "\n\n---\n\nplan: `/abs/checkout/docs/plans/20260902-x.md` - run `local-1`\n\nOpened automatically by ralphex-farm."
    /// );
    /// ```
    #[must_use]
    pub fn describe(
        branch: Branch,
        origin: &RunOrigin,
        plan: &str,
        run_id: &RunId,
        written: Option<Description>,
    ) -> PrSpec {
        let (title, body) = match written {
            Some(Description { title, body }) => (title, body),
            None => (prdesc::fallback_title(origin), String::new()),
        };
        let footer = prdesc::footer(origin, plan, run_id);
        PrSpec {
            branch,
            title,
            body: format!("{body}{footer}"),
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
/// base branch is resolved and `gh pr create` opens the pull request. Both
/// pushes place the remote and the branch after `--`, because git parses an
/// option anywhere in its argument list and a branch named `--receive-pack=…`
/// would otherwise run a command of the ticket author's choosing on the remote.
///
/// # Errors
///
/// Returns [`PrError::List`] when the lookup fails, [`PrError::Push`] when the
/// push fails, [`PrError::Base`] when neither git nor the GitHub CLI names the
/// default branch, and [`PrError::Create`] when the creation fails or prints no
/// URL. A step that outlives [`PrTools::step_timeout`] is killed and fails as
/// the step it is.
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
    let PrTools {
        git,
        gh,
        env,
        step_timeout,
    } = tools;
    let step_timeout = *step_timeout;
    let branch = branch.as_str();

    let listed = match step(
        ctx,
        gh,
        &[
            "pr", "list", "--head", branch, "--state", "open", "--json", "url", "--jq", ".[0].url",
        ],
        env,
        step_timeout,
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
        match step(
            ctx,
            git,
            &["push", "--", "origin", branch],
            env,
            step_timeout,
        )
        .await
        {
            Ok(_pushed) => {}
            Err(message) => return Err(PrError::Push(message)),
        }
        return Ok(existing);
    }

    match step(
        ctx,
        git,
        &["push", "-u", "--", "origin", branch],
        env,
        step_timeout,
    )
    .await
    {
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
        step_timeout,
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
    let PrTools {
        git,
        gh,
        env,
        step_timeout,
    } = tools;
    let step_timeout = *step_timeout;
    let symbolic = step(
        ctx,
        git,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        env,
        step_timeout,
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
        step_timeout,
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
    step_timeout: Duration,
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
    command.kill_on_drop(true);
    let label = label(program, args);
    let output = match tokio::time::timeout(step_timeout, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => return Err(format!("{label} could not be run: {error}")),
        Err(_elapsed) => {
            let seconds = step_timeout.as_secs();
            return Err(format!("{label} was killed after {seconds} seconds"));
        }
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
    fn a_written_description_titles_the_pull_request_and_leads_its_body() {
        let written = Description {
            title: "Split the farm from its runner".to_string(),
            body: "The runner moves out.".to_string(),
        };
        let PrSpec {
            branch,
            title,
            body,
        } = PrSpec::describe(
            Branch("farm-runner".to_string()),
            &ticket(),
            "/abs/checkout/docs/plans/x.md",
            &RunId("FARM-12-1753180800000".to_string()),
            Some(written),
        );
        assert_eq!(branch, Branch("farm-runner".to_string()));
        assert_eq!(title, "Split the farm from its runner");
        assert_eq!(
            body,
            concat!(
                "The runner moves out.\n",
                "\n",
                "---\n",
                "\n",
                "[FARM-12](https://linear.app/example/issue/FARM-12) - plan: `/abs/checkout/docs/plans/x.md` - run `FARM-12-1753180800000`\n",
                "\n",
                "Opened automatically by ralphex-farm.",
            )
        );
    }

    #[test]
    fn a_ticket_job_without_a_description_is_titled_after_its_ticket() {
        let PrSpec {
            branch: _,
            title,
            body,
        } = PrSpec::describe(
            Branch("x".to_string()),
            &ticket(),
            "/abs/plan.md",
            &RunId("FARM-12-1".to_string()),
            None,
        );
        assert_eq!(title, "FARM-12: split farm and runner");
        assert_eq!(
            body,
            prdesc::footer(&ticket(), "/abs/plan.md", &RunId("FARM-12-1".to_string()))
        );
    }

    #[test]
    fn a_local_run_without_a_description_is_titled_after_its_name() {
        let origin = RunOrigin::Local {
            title: "Require dials".to_string(),
        };
        let PrSpec {
            branch: _,
            title,
            body,
        } = PrSpec::describe(
            Branch("x".to_string()),
            &origin,
            "/abs/checkout/docs/plans/20260902-ralphex-macos-runner.md",
            &RunId("local-1753180800000".to_string()),
            None,
        );
        assert_eq!(title, "Require dials");
        assert_eq!(
            body,
            concat!(
                "\n\n---\n\n",
                "plan: `/abs/checkout/docs/plans/20260902-ralphex-macos-runner.md` - run `local-1753180800000`",
                "\n\nOpened automatically by ralphex-farm.",
            )
        );
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
        let PrTools {
            git,
            gh,
            env,
            step_timeout,
        } = PrTools::default();
        assert_eq!(git, "git");
        assert_eq!(gh, "gh");
        assert!(env.is_empty());
        assert_eq!(step_timeout, PR_STEP_TIMEOUT);
    }
}
