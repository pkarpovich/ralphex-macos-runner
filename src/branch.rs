//! The branch a local run works on, decided the way ralphex decides it.
//!
//! ralphex creates the branch it is given only when the checkout is on its
//! default branch; on any other branch it keeps working where it is and ignores
//! `--branch`. The daemon pushes the branch it asked for, so a name that differs
//! from the one ralphex actually committed to fails the run at the push, hours
//! after the choice was made. [`choose`] makes the same decision up front: the
//! checked-out branch when the checkout is off its default branch, the requested
//! name or the plan's stem when it is on it, and a refusal when the two cannot
//! agree.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use crate::job::Worktree;

const COMMON_DEFAULTS: [&str; 4] = ["main", "master", "trunk", "develop"];

/// What the checkout's `HEAD` points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    /// `HEAD` is the named local branch.
    Branch(String),
    /// `HEAD` is a commit, not a branch.
    Detached,
}

/// Why no branch can be chosen for a run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BranchError {
    /// The checkout has no branch checked out.
    #[error(
        "the checkout is on a detached HEAD: ralphex would commit to no branch, so nothing could be pushed; check out a branch first"
    )]
    Detached,
    /// A worktree run was asked for off the default branch, which ralphex refuses.
    #[error("--worktree needs the checkout on {default}, but it is on {current}")]
    WorktreeOffDefault {
        /// The branch the checkout is on.
        current: String,
        /// The checkout's default branch.
        default: String,
    },
    /// A branch was named while the checkout is on another feature branch.
    #[error(
        "the checkout is on {current}: ralphex keeps working there and would ignore --branch {requested}; drop --branch, or check out {default} first"
    )]
    Ignored {
        /// The branch the checkout is on.
        current: String,
        /// The branch that was asked for.
        requested: String,
        /// The checkout's default branch.
        default: String,
    },
    /// The branch a pull request would target does not exist on `origin`.
    #[error(
        "the base branch {base} is not on origin, so no pull request could be opened into it; push it first (git push -u origin {base}) and run again"
    )]
    BaseMissing {
        /// The checkout's default branch, the pull request's base.
        base: String,
    },
    /// The checkout could not be inspected.
    #[error("{0}")]
    Git(String),
}

/// Checks that `origin` of the checkout at `ctx` has the branch `base`.
///
/// A pull request is opened into the checkout's default branch; when that
/// branch was never pushed, the feature branch the run pushes becomes the
/// repository's only branch, GitHub makes it the default, and the pull request
/// into itself fails after the whole run. `base` may be given as `origin/<name>`.
///
/// # Errors
///
/// Returns [`BranchError::BaseMissing`] when `origin` has no such branch and
/// [`BranchError::Git`] when `origin` cannot be asked.
pub fn require_remote_base(ctx: &Path, base: &str) -> Result<(), BranchError> {
    let base = match base.strip_prefix("origin/") {
        Some(base) => base,
        None => base,
    };
    let reference = format!("refs/heads/{base}");
    let Output {
        status,
        stdout,
        stderr,
    } = git(ctx, &["ls-remote", "--heads", "origin", &reference])?;
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        return Err(BranchError::Git(format!(
            "origin could not be asked for {base}: {}",
            stderr.trim()
        )));
    }
    match String::from_utf8_lossy(&stdout).trim().is_empty() {
        true => Err(BranchError::BaseMissing {
            base: base.to_string(),
        }),
        false => Ok(()),
    }
}

/// Returns the branch a run of `plan_stem` works on, given where the checkout is.
///
/// `default` is the checkout's default branch as [`default_branch`] reports it,
/// possibly as `origin/<name>`. On the default branch the run gets `requested`,
/// or `plan_stem` without one, because ralphex creates exactly that branch. Off
/// it the run gets the checked-out branch, because ralphex keeps working there.
///
/// # Errors
///
/// Returns [`BranchError::Detached`] for a detached `HEAD`,
/// [`BranchError::WorktreeOffDefault`] for a worktree run off the default
/// branch, and [`BranchError::Ignored`] when `requested` names a branch other
/// than the feature branch the checkout is on.
///
/// # Examples
///
/// ```
/// use ralphex_macos_runner::branch::{Head, choose};
/// use ralphex_macos_runner::job::Worktree;
///
/// let on_main = Head::Branch("main".to_string());
/// assert_eq!(choose(&on_main, "main", None, "20260927-heap", Worktree::No).unwrap(), "20260927-heap");
///
/// let on_feature = Head::Branch("feat/heap-app".to_string());
/// assert_eq!(choose(&on_feature, "main", None, "20260927-heap", Worktree::No).unwrap(), "feat/heap-app");
/// assert!(choose(&on_feature, "main", Some("other"), "20260927-heap", Worktree::No).is_err());
/// ```
pub fn choose(
    head: &Head,
    default: &str,
    requested: Option<&str>,
    plan_stem: &str,
    worktree: Worktree,
) -> Result<String, BranchError> {
    let current = match head {
        Head::Branch(current) => current,
        Head::Detached => return Err(BranchError::Detached),
    };
    let default = match default.strip_prefix("origin/") {
        Some(default) => default,
        None => default,
    };
    if current == default {
        return match requested {
            Some(requested) => Ok(requested.to_string()),
            None => Ok(plan_stem.to_string()),
        };
    }
    match worktree {
        Worktree::Yes => {
            return Err(BranchError::WorktreeOffDefault {
                current: current.clone(),
                default: default.to_string(),
            });
        }
        Worktree::No => {}
    }
    match requested {
        None => Ok(current.clone()),
        Some(requested) if requested == current => Ok(current.clone()),
        Some(requested) => Err(BranchError::Ignored {
            current: current.clone(),
            requested: requested.to_string(),
            default: default.to_string(),
        }),
    }
}

/// Returns what the checkout at `ctx` has checked out.
///
/// # Errors
///
/// Returns [`BranchError::Git`] when `git` cannot be run or fails for any
/// reason other than a detached `HEAD`, such as `ctx` not being a checkout.
pub fn head(ctx: &Path) -> Result<Head, BranchError> {
    let output = git(ctx, &["symbolic-ref", "--short", "HEAD"])?;
    let Output {
        status,
        stdout,
        stderr,
    } = output;
    if status.success() {
        let branch = String::from_utf8_lossy(&stdout).trim().to_string();
        return Ok(Head::Branch(branch));
    }
    let stderr = String::from_utf8_lossy(&stderr).to_lowercase();
    if stderr.contains("not a symbolic ref") {
        return Ok(Head::Detached);
    }
    Err(BranchError::Git(format!(
        "{} is not a usable git checkout: {}",
        ctx.display(),
        stderr.trim()
    )))
}

/// Returns the default branch of the checkout at `ctx`, as ralphex resolves it.
///
/// It is the branch `origin/HEAD` names, as a local name when that branch
/// exists locally and as `origin/<name>` when it does not; without `origin/HEAD`
/// it is the first of `main`, `master`, `trunk` and `develop` that exists
/// locally, and `master` when none does.
#[must_use]
pub fn default_branch(ctx: &Path) -> String {
    let symbolic = git(ctx, &["symbolic-ref", "refs/remotes/origin/HEAD"]);
    if let Ok(Output {
        status,
        stdout,
        stderr: _,
    }) = symbolic
        && status.success()
    {
        let reference = String::from_utf8_lossy(&stdout).trim().to_string();
        if let Some(name) = reference.strip_prefix("refs/remotes/origin/") {
            return match local_branch(ctx, name) {
                true => name.to_string(),
                false => format!("origin/{name}"),
            };
        }
    }
    for name in COMMON_DEFAULTS {
        if local_branch(ctx, name) {
            return name.to_string();
        }
    }
    "master".to_string()
}

fn local_branch(ctx: &Path, name: &str) -> bool {
    let reference = format!("refs/heads/{name}");
    let Ok(Output {
        status,
        stdout: _,
        stderr: _,
    }) = git(ctx, &["show-ref", "--verify", "--quiet", &reference])
    else {
        return false;
    };
    status.success()
}

fn git(ctx: &Path, args: &[&str]) -> Result<Output, BranchError> {
    let mut command = Command::new("git");
    command.args(args);
    command.current_dir(ctx);
    command.env("LC_ALL", "C");
    command.stdin(Stdio::null());
    match command.output() {
        Ok(output) => Ok(output),
        Err(error) => Err(BranchError::Git(format!("git could not be run: {error}"))),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use super::{BranchError, Head, choose, require_remote_base};
    use crate::job::Worktree;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn pushed_checkout() -> (tempfile::TempDir, tempfile::TempDir) {
        let origin = tempfile::tempdir().unwrap();
        git(origin.path(), &["init", "--quiet", "--bare"]);
        let checkout = tempfile::tempdir().unwrap();
        git(
            checkout.path(),
            &["init", "--quiet", "--initial-branch", "main"],
        );
        git(
            checkout.path(),
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--quiet",
                "--allow-empty",
                "--message",
                "init",
            ],
        );
        let origin_path = origin.path().display().to_string();
        git(checkout.path(), &["remote", "add", "origin", &origin_path]);
        git(checkout.path(), &["push", "--quiet", "origin", "main"]);
        (checkout, origin)
    }

    #[test]
    fn a_base_origin_has_is_accepted_bare_or_as_a_remote_name() {
        let (checkout, _origin) = pushed_checkout();

        assert_eq!(require_remote_base(checkout.path(), "main"), Ok(()));
        assert_eq!(require_remote_base(checkout.path(), "origin/main"), Ok(()));
    }

    #[test]
    fn a_base_origin_lacks_is_refused() {
        let (checkout, _origin) = pushed_checkout();

        assert_eq!(
            require_remote_base(checkout.path(), "master"),
            Err(BranchError::BaseMissing {
                base: "master".to_string()
            })
        );
    }

    #[test]
    fn a_checkout_without_an_origin_is_a_git_error() {
        let (checkout, _origin) = pushed_checkout();
        git(checkout.path(), &["remote", "remove", "origin"]);

        let refused = require_remote_base(checkout.path(), "main");

        assert!(
            matches_git_error(&refused),
            "expected a git error, got {refused:?}"
        );
    }

    fn matches_git_error(result: &Result<(), BranchError>) -> bool {
        match result {
            Err(BranchError::Git(_)) => true,
            Ok(())
            | Err(BranchError::Detached)
            | Err(BranchError::WorktreeOffDefault { .. })
            | Err(BranchError::Ignored { .. })
            | Err(BranchError::BaseMissing { .. }) => false,
        }
    }

    fn on(branch: &str) -> Head {
        Head::Branch(branch.to_string())
    }

    #[test]
    fn a_run_follows_ralphex_on_every_kind_of_checkout() {
        let cases = [
            (
                on("main"),
                "main",
                None,
                Worktree::No,
                "20260927-heap-backend",
            ),
            (on("main"), "main", Some("named"), Worktree::No, "named"),
            (
                on("main"),
                "origin/main",
                None,
                Worktree::No,
                "20260927-heap-backend",
            ),
            (
                on("main"),
                "main",
                None,
                Worktree::Yes,
                "20260927-heap-backend",
            ),
            (on("main"), "main", Some("named"), Worktree::Yes, "named"),
            (
                on("feat/heap-app"),
                "main",
                None,
                Worktree::No,
                "feat/heap-app",
            ),
            (
                on("feat/heap-app"),
                "main",
                Some("feat/heap-app"),
                Worktree::No,
                "feat/heap-app",
            ),
            (
                on("develop"),
                "develop",
                None,
                Worktree::No,
                "20260927-heap-backend",
            ),
        ];
        for (head, default, requested, worktree, expected) in cases {
            let chosen = choose(&head, default, requested, "20260927-heap-backend", worktree);
            assert_eq!(
                chosen,
                Ok(expected.to_string()),
                "{head:?} default {default} requested {requested:?} {worktree:?}"
            );
        }
    }

    #[test]
    fn a_detached_checkout_is_refused() {
        let chosen = choose(&Head::Detached, "main", None, "plan", Worktree::No);

        assert_eq!(chosen, Err(BranchError::Detached));
    }

    #[test]
    fn a_named_branch_ralphex_would_ignore_is_refused() {
        let chosen = choose(
            &on("add-1password-extension"),
            "origin/main",
            Some("20260920-1password-extension"),
            "20260920-1password-extension",
            Worktree::No,
        );

        assert_eq!(
            chosen,
            Err(BranchError::Ignored {
                current: "add-1password-extension".to_string(),
                requested: "20260920-1password-extension".to_string(),
                default: "main".to_string(),
            })
        );
    }

    #[test]
    fn a_worktree_off_the_default_branch_is_refused() {
        let chosen = choose(&on("feat/heap-app"), "main", None, "plan", Worktree::Yes);

        assert_eq!(
            chosen,
            Err(BranchError::WorktreeOffDefault {
                current: "feat/heap-app".to_string(),
                default: "main".to_string(),
            })
        );
    }
}
