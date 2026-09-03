//! The launchd user agent the daemon runs under.
//!
//! [`install`] copies the daemon binary that sits next to the running `rxd` to
//! the stable path under the application directory, writes the property list
//! and hands it to launchd; [`uninstall`] takes it out again. The stable path
//! never changes across a `brew upgrade`, so re-running `rxd install` after an
//! upgrade is the whole update procedure. The property list carries the
//! installing shell's `PATH`, without which the daemon would find neither
//! `ralphex`, `claude`, `codex`, `gh` nor `xcodebuild`.

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use nix::unistd::Uid;
use tokio::process::Command;

use crate::config::{Config, DEFAULT_DRAIN_TIMEOUT};
use crate::paths::{self, APP_NAME, PathError};
use crate::protocol::types::{
    COMPLETE_BUDGET, LOG_CLOSE_TIMEOUT, REQUEST_TIMEOUT, RETRY_MAX_DELAY, STOP_GRACE,
};

/// The file launchd writes the daemon's standard output to.
pub const STDOUT_FILE: &str = "daemon.out.log";

/// The file launchd writes the daemon's standard error to.
pub const STDERR_FILE: &str = "daemon.err.log";

/// Why the launchd agent could not be installed or removed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServiceError {
    /// A filesystem location could not be resolved.
    #[error("{0}")]
    Path(String),
    /// The running binary does not name a directory to take the daemon from.
    #[error("the running binary could not be located: {0}")]
    Exe(String),
    /// The daemon binary does not sit next to the running `rxd`.
    #[error("{path} does not exist; rxd and the daemon are installed side by side")]
    DaemonMissing {
        /// The path that was looked for.
        path: String,
    },
    /// A directory or a file could not be written.
    #[error("{path} could not be written: {message}")]
    Write {
        /// The path that was written to.
        path: String,
        /// The reason the write failed.
        message: String,
    },
    /// launchd refused to take the agent.
    #[error("launchctl {action} failed: {message}")]
    Launchctl {
        /// The launchctl subcommand that failed.
        action: &'static str,
        /// What launchctl said.
        message: String,
    },
}

impl From<PathError> for ServiceError {
    fn from(error: PathError) -> Self {
        ServiceError::Path(error.to_string())
    }
}

/// What an [`install`] left on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// The stable path launchd runs the daemon from.
    pub daemon_path: PathBuf,
    /// The property list handed to launchd.
    pub plist_path: PathBuf,
    /// The directory launchd writes the daemon's output to.
    pub log_dir: PathBuf,
    /// The user whose launchd domain holds the agent.
    pub uid: u32,
}

impl std::fmt::Display for Installed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Installed {
            daemon_path,
            plist_path,
            log_dir,
            uid,
        } = self;
        writeln!(f, "daemon  {}", daemon_path.display())?;
        writeln!(f, "agent   {}", plist_path.display())?;
        writeln!(f, "logs    {}", log_dir.display())?;
        write!(f, "{}", by_hand(*uid, plist_path))
    }
}

/// What an [`uninstall`] took off disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uninstalled {
    /// The property list that was removed.
    pub plist_path: PathBuf,
    /// The user whose launchd domain held the agent.
    pub uid: u32,
}

impl std::fmt::Display for Uninstalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Uninstalled { plist_path, uid } = self;
        writeln!(f, "removed {}", plist_path.display())?;
        write!(f, "{}", by_hand(*uid, plist_path))
    }
}

/// Returns how long launchd must wait for the daemon after it sends `SIGTERM`.
///
/// launchd's default `ExitTimeOut` is 20 seconds, which is far shorter than the
/// shutdown sequence, and a `SIGKILL` before that sequence ends leaves the farm
/// to finalise a finished run `runner_lost`. The sum walks every await the
/// daemon makes after the signal: `drain_timeout` for the run to finish,
/// [`STOP_GRACE`] to stop the process group, another [`STOP_GRACE`] for the
/// pipe drain, [`LOG_CLOSE_TIMEOUT`] for the log stream's last flush, and
/// [`COMPLETE_BUDGET`] for the completion - which overruns its budget by a
/// backoff and a request, because the budget is checked before the sleep rather
/// than after the attempt.
///
/// # Examples
///
/// ```
/// use std::time::Duration;
///
/// use ralphex_macos_runner::service;
///
/// assert_eq!(
///     service::exit_timeout(Duration::from_secs(120)),
///     Duration::from_secs(120 + 10 + 10 + 30 + 180 + 30 + 30)
/// );
/// ```
#[must_use]
pub fn exit_timeout(drain_timeout: Duration) -> Duration {
    drain_timeout
        + STOP_GRACE
        + STOP_GRACE
        + LOG_CLOSE_TIMEOUT
        + COMPLETE_BUDGET
        + RETRY_MAX_DELAY
        + REQUEST_TIMEOUT
}

/// Returns the property list launchd loads the daemon from.
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// use std::time::Duration;
///
/// use ralphex_macos_runner::service;
///
/// let plist = service::generate_plist(
///     "dev.pkarpovich.ralphex-macos-runner",
///     Path::new("/data/ralphex-macos-runner/bin/ralphex-macos-runner"),
///     "/opt/homebrew/bin:/usr/bin",
///     Path::new("/logs/ralphex-macos-runner"),
///     Duration::from_secs(310),
/// );
/// assert!(plist.contains("<string>dev.pkarpovich.ralphex-macos-runner</string>"));
/// assert!(plist.contains("<string>/opt/homebrew/bin:/usr/bin</string>"));
/// assert!(plist.contains("<string>/logs/ralphex-macos-runner/daemon.out.log</string>"));
/// assert!(plist.contains("<integer>310</integer>"));
/// ```
#[must_use]
pub fn generate_plist(
    label: &str,
    daemon_path: &Path,
    path_env: &str,
    log_dir: &Path,
    exit_timeout: Duration,
) -> String {
    let label = escape(label);
    let daemon_path = escape(&daemon_path.display().to_string());
    let path_env = escape(path_env);
    let stdout_path = escape(&log_dir.join(STDOUT_FILE).display().to_string());
    let stderr_path = escape(&log_dir.join(STDERR_FILE).display().to_string());
    let exit_timeout = exit_timeout.as_secs();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{daemon_path}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>ExitTimeOut</key>
	<integer>{exit_timeout}</integer>
	<key>StandardOutPath</key>
	<string>{stdout_path}</string>
	<key>StandardErrorPath</key>
	<string>{stderr_path}</string>
	<key>EnvironmentVariables</key>
	<dict>
		<key>PATH</key>
		<string>{path_env}</string>
	</dict>
</dict>
</plist>
"#
    )
}

/// Installs the daemon as a launchd user agent and starts it.
///
/// The daemon binary is taken from the directory of the running executable,
/// copied to the stable path under the application directory and registered
/// with the installing shell's `PATH`.
///
/// # Errors
///
/// Returns [`ServiceError::Exe`] when the running binary cannot be located,
/// [`ServiceError::DaemonMissing`] when the daemon does not sit next to it,
/// [`ServiceError::Path`] when a location cannot be resolved,
/// [`ServiceError::Write`] when a directory or a file cannot be written and
/// [`ServiceError::Launchctl`] when launchd refuses the agent.
pub async fn install() -> Result<Installed, ServiceError> {
    let source = daemon_source()?;
    let daemon_path = paths::daemon_binary_path()?;
    let plist_path = paths::launch_agent_path()?;
    let log_dir = paths::log_dir()?;

    if let Some(parent) = daemon_path.parent() {
        create_dir(parent)?;
    }
    if let Some(parent) = plist_path.parent() {
        create_dir(parent)?;
    }
    create_dir(&log_dir)?;

    place_daemon(&source, &daemon_path)?;

    let path_env = std::env::var("PATH").unwrap_or_default();
    let plist = generate_plist(
        &paths::launchd_label(),
        &daemon_path,
        &path_env,
        &log_dir,
        exit_timeout(configured_drain_timeout()),
    );
    if let Err(error) = std::fs::write(&plist_path, plist) {
        return Err(ServiceError::Write {
            path: plist_path.display().to_string(),
            message: error.to_string(),
        });
    }

    let uid = Uid::current().as_raw();
    let _booted_out = launchctl("bootout", uid, &plist_path).await;
    launchctl("bootstrap", uid, &plist_path).await?;

    Ok(Installed {
        daemon_path,
        plist_path,
        log_dir,
        uid,
    })
}

/// Stops the launchd user agent and removes its property list.
///
/// # Errors
///
/// Returns [`ServiceError::Path`] when the property list's location cannot be
/// resolved and [`ServiceError::Write`] when it cannot be removed.
pub async fn uninstall() -> Result<Uninstalled, ServiceError> {
    let plist_path = paths::launch_agent_path()?;
    let uid = Uid::current().as_raw();
    let _booted_out = launchctl("bootout", uid, &plist_path).await;
    match std::fs::remove_file(&plist_path) {
        Ok(()) => {}
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(ServiceError::Write {
                    path: plist_path.display().to_string(),
                    message: error.to_string(),
                });
            }
        }
    }
    Ok(Uninstalled { plist_path, uid })
}

/// Returns the launchctl commands that stop and start the agent by hand.
///
/// # Examples
///
/// ```
/// use std::path::Path;
///
/// use ralphex_macos_runner::service;
///
/// let hint = service::by_hand(501, Path::new("/agents/dev.pkarpovich.plist"));
/// assert!(hint.contains("launchctl bootout gui/501 /agents/dev.pkarpovich.plist"));
/// assert!(hint.contains("launchctl bootstrap gui/501 /agents/dev.pkarpovich.plist"));
/// ```
#[must_use]
pub fn by_hand(uid: u32, plist_path: &Path) -> String {
    let plist_path = plist_path.display();
    format!(
        "stop    launchctl bootout gui/{uid} {plist_path}\nstart   launchctl bootstrap gui/{uid} {plist_path}"
    )
}

fn configured_drain_timeout() -> Duration {
    let Ok(path) = paths::config_path() else {
        return DEFAULT_DRAIN_TIMEOUT;
    };
    let Ok(config) = Config::load(&path) else {
        return DEFAULT_DRAIN_TIMEOUT;
    };
    config.drain_timeout
}

/// Copies the daemon binary from `source` to `daemon_path`.
///
/// The old file is removed before the copy: overwriting the binary launchd is
/// running fails with `ETXTBSY`, and unlinking it leaves the running daemon on
/// the inode it already opened.
///
/// # Errors
///
/// Returns [`ServiceError::Write`] when the old binary cannot be removed or the
/// new one cannot be copied into place.
pub fn place_daemon(source: &Path, daemon_path: &Path) -> Result<(), ServiceError> {
    match std::fs::remove_file(daemon_path) {
        Ok(()) => {}
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(ServiceError::Write {
                    path: daemon_path.display().to_string(),
                    message: error.to_string(),
                });
            }
        }
    }
    if let Err(error) = std::fs::copy(source, daemon_path) {
        return Err(ServiceError::Write {
            path: daemon_path.display().to_string(),
            message: format!("{} could not be copied: {error}", source.display()),
        });
    }
    Ok(())
}

fn daemon_source() -> Result<PathBuf, ServiceError> {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => return Err(ServiceError::Exe(error.to_string())),
    };
    let exe = match exe.canonicalize() {
        Ok(exe) => exe,
        Err(error) => {
            return Err(ServiceError::Exe(format!(
                "{} does not resolve: {error}",
                exe.display()
            )));
        }
    };
    let Some(dir) = exe.parent() else {
        return Err(ServiceError::Exe(format!(
            "{} has no directory",
            exe.display()
        )));
    };
    let source = dir.join(APP_NAME);
    if !source.is_file() {
        return Err(ServiceError::DaemonMissing {
            path: source.display().to_string(),
        });
    }
    Ok(source)
}

fn create_dir(path: &Path) -> Result<(), ServiceError> {
    match std::fs::create_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) => Err(ServiceError::Write {
            path: path.display().to_string(),
            message: error.to_string(),
        }),
    }
}

async fn launchctl(action: &'static str, uid: u32, plist_path: &Path) -> Result<(), ServiceError> {
    let mut command = Command::new("launchctl");
    command.arg(action);
    command.arg(format!("gui/{uid}"));
    command.arg(plist_path);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let output = match command.output().await {
        Ok(output) => output,
        Err(error) => {
            return Err(ServiceError::Launchctl {
                action,
                message: format!("launchctl could not be run: {error}"),
            });
        }
    };
    let Output {
        status,
        stdout: _,
        stderr,
    } = output;
    if status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&stderr).into_owned();
    let stderr = stderr.trim();
    let code = match status.code() {
        Some(code) => code.to_string(),
        None => "a signal".to_string(),
    };
    Err(ServiceError::Launchctl {
        action,
        message: format!("exited with {code}: {stderr}"),
    })
}

fn escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            character => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plist() -> String {
        generate_plist(
            &paths::launchd_label(),
            Path::new("/data/ralphex-macos-runner/bin/ralphex-macos-runner"),
            "/opt/homebrew/bin:/usr/bin:/bin",
            Path::new("/logs/ralphex-macos-runner"),
            exit_timeout(DEFAULT_DRAIN_TIMEOUT),
        )
    }

    #[test]
    fn the_plist_carries_the_label_and_the_program() {
        let plist = plist();
        assert!(plist.contains("<key>Label</key>"), "{plist}");
        assert!(
            plist.contains(&format!("<string>{}</string>", paths::launchd_label())),
            "{plist}"
        );
        assert!(
            plist.contains(
                "<array>\n\t\t<string>/data/ralphex-macos-runner/bin/ralphex-macos-runner</string>\n\t</array>"
            ),
            "{plist}"
        );
    }

    #[test]
    fn the_agent_runs_at_load_and_is_kept_alive() {
        let plist = plist();
        assert!(plist.contains("<key>RunAtLoad</key>\n\t<true/>"), "{plist}");
        assert!(plist.contains("<key>KeepAlive</key>\n\t<true/>"), "{plist}");
    }

    #[test]
    fn launchd_waits_out_the_whole_shutdown_before_it_kills_the_daemon() {
        let plist = plist();
        let seconds = exit_timeout(DEFAULT_DRAIN_TIMEOUT).as_secs();
        assert!(
            plist.contains(&format!(
                "<key>ExitTimeOut</key>\n\t<integer>{seconds}</integer>"
            )),
            "{plist}"
        );
        assert!(seconds > DEFAULT_DRAIN_TIMEOUT.as_secs(), "{seconds}");
    }

    #[test]
    fn the_exit_timeout_covers_every_await_the_shutdown_makes() {
        let drain_timeout = DEFAULT_DRAIN_TIMEOUT;
        let stop = STOP_GRACE;
        let pipes = STOP_GRACE;
        let logs = LOG_CLOSE_TIMEOUT;
        let completion = COMPLETE_BUDGET + RETRY_MAX_DELAY + REQUEST_TIMEOUT;
        assert_eq!(
            exit_timeout(drain_timeout),
            drain_timeout + stop + pipes + logs + completion
        );
    }

    #[test]
    fn a_longer_drain_timeout_buys_a_longer_exit_timeout() {
        let drain_timeout = Duration::from_secs(600);
        let plist = generate_plist(
            &paths::launchd_label(),
            Path::new("/bin/daemon"),
            "/usr/bin",
            Path::new("/logs"),
            exit_timeout(drain_timeout),
        );
        assert!(plist.contains("<integer>890</integer>"), "{plist}");
    }

    #[test]
    fn both_log_paths_are_named() {
        let plist = plist();
        assert!(
            plist.contains(
                "<key>StandardOutPath</key>\n\t<string>/logs/ralphex-macos-runner/daemon.out.log</string>"
            ),
            "{plist}"
        );
        assert!(
            plist.contains(
                "<key>StandardErrorPath</key>\n\t<string>/logs/ralphex-macos-runner/daemon.err.log</string>"
            ),
            "{plist}"
        );
    }

    #[test]
    fn the_path_of_the_installing_shell_is_passed_through() {
        let plist = plist();
        assert!(
            plist.contains("<key>PATH</key>\n\t\t<string>/opt/homebrew/bin:/usr/bin:/bin</string>"),
            "{plist}"
        );
    }

    #[test]
    fn the_log_paths_are_the_ones_paths_names() {
        let log_dir = paths::log_dir().unwrap();
        let plist = generate_plist(
            &paths::launchd_label(),
            Path::new("/bin/daemon"),
            "/usr/bin",
            &log_dir,
            exit_timeout(DEFAULT_DRAIN_TIMEOUT),
        );
        let stdout_path = paths::daemon_stdout_path().unwrap();
        let stderr_path = paths::daemon_stderr_path().unwrap();
        assert!(
            plist.contains(&format!("<string>{}</string>", stdout_path.display())),
            "{plist}"
        );
        assert!(
            plist.contains(&format!("<string>{}</string>", stderr_path.display())),
            "{plist}"
        );
    }

    #[test]
    fn a_development_build_registers_under_a_label_of_its_own() {
        let plist = plist();
        assert!(
            plist.contains("<string>dev.pkarpovich.ralphex-macos-runner-dev</string>"),
            "{plist}"
        );
        assert!(
            !plist.contains("<string>dev.pkarpovich.ralphex-macos-runner</string>"),
            "{plist}"
        );
    }

    #[test]
    fn a_daemon_is_placed_over_the_binary_that_was_there() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("new-daemon");
        let installed = dir.path().join("ralphex-macos-runner");
        std::fs::write(&source, "new").unwrap();
        std::fs::write(&installed, "old").unwrap();

        place_daemon(&source, &installed).unwrap();

        assert_eq!(std::fs::read_to_string(&installed).unwrap(), "new");
    }

    #[test]
    fn a_daemon_is_placed_where_none_was() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("new-daemon");
        let installed = dir.path().join("bin").join("ralphex-macos-runner");
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        std::fs::write(&source, "new").unwrap();

        place_daemon(&source, &installed).unwrap();

        assert_eq!(std::fs::read_to_string(&installed).unwrap(), "new");
    }

    #[test]
    fn a_source_that_is_not_there_is_reported_with_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("absent-daemon");
        let installed = dir.path().join("ralphex-macos-runner");

        let error = place_daemon(&source, &installed).unwrap_err();

        let ServiceError::Write { path, message } = error else {
            panic!("a copy that fails is a write failure");
        };
        assert_eq!(path, installed.display().to_string());
        assert!(message.contains("absent-daemon"), "{message}");
        assert!(!installed.exists());
    }

    #[test]
    fn markup_in_a_path_is_escaped() {
        let plist = generate_plist(
            "dev.pkarpovich.ralphex-macos-runner",
            Path::new("/data/a&b/daemon"),
            "/usr/bin:/opt/<x>",
            Path::new("/logs"),
            exit_timeout(DEFAULT_DRAIN_TIMEOUT),
        );
        assert!(
            plist.contains("<string>/data/a&amp;b/daemon</string>"),
            "{plist}"
        );
        assert!(
            plist.contains("<string>/usr/bin:/opt/&lt;x&gt;</string>"),
            "{plist}"
        );
    }

    #[test]
    fn a_report_names_the_paths_and_the_launchctl_commands() {
        let installed = Installed {
            daemon_path: PathBuf::from("/data/bin/ralphex-macos-runner"),
            plist_path: PathBuf::from("/agents/dev.pkarpovich.plist"),
            log_dir: PathBuf::from("/logs"),
            uid: 501,
        };
        let report = installed.to_string();
        assert!(
            report.contains("/data/bin/ralphex-macos-runner"),
            "{report}"
        );
        assert!(report.contains("/agents/dev.pkarpovich.plist"), "{report}");
        assert!(report.contains("/logs"), "{report}");
        assert!(
            report.contains("launchctl bootstrap gui/501 /agents/dev.pkarpovich.plist"),
            "{report}"
        );

        let uninstalled = Uninstalled {
            plist_path: PathBuf::from("/agents/dev.pkarpovich.plist"),
            uid: 501,
        };
        let report = uninstalled.to_string();
        assert!(
            report.contains("removed /agents/dev.pkarpovich.plist"),
            "{report}"
        );
        assert!(
            report.contains("launchctl bootout gui/501 /agents/dev.pkarpovich.plist"),
            "{report}"
        );
    }

    #[test]
    fn a_path_error_becomes_a_service_error() {
        let error = ServiceError::from(PathError::HomeDir);
        assert_eq!(error, ServiceError::Path(PathError::HomeDir.to_string()));
    }
}
