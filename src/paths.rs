//! Resolves the filesystem locations the daemon and the client share.
//!
//! Every location depends on the build profile: a release build uses the
//! application directory `ralphex-macos-runner`, a debug build uses
//! `ralphex-macos-runner-dev`, so a daemon under development never collides
//! with the installed one.

use std::path::{Path, PathBuf};

/// The application directory name a release build uses.
pub const APP_NAME: &str = "ralphex-macos-runner";

/// The prefix every launchd label of this daemon carries.
pub const LAUNCHD_PREFIX: &str = "dev.pkarpovich";

/// The build profile that decides which application directory is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// An optimized build installed through Homebrew and launchd.
    Release,
    /// A development build run from the target directory.
    Debug,
}

impl Profile {
    /// Returns the profile this crate was compiled with.
    ///
    /// # Examples
    ///
    /// ```
    /// use ralphex_macos_runner::paths::Profile;
    ///
    /// assert_eq!(Profile::current(), Profile::Debug);
    /// ```
    #[must_use]
    pub fn current() -> Self {
        if cfg!(debug_assertions) {
            Profile::Debug
        } else {
            Profile::Release
        }
    }

    /// Returns the application directory name this profile owns.
    ///
    /// # Examples
    ///
    /// ```
    /// use ralphex_macos_runner::paths::Profile;
    ///
    /// assert_eq!(Profile::Release.app_dir_name(), "ralphex-macos-runner");
    /// assert_eq!(Profile::Debug.app_dir_name(), "ralphex-macos-runner-dev");
    /// ```
    #[must_use]
    pub fn app_dir_name(self) -> &'static str {
        match self {
            Profile::Release => APP_NAME,
            Profile::Debug => "ralphex-macos-runner-dev",
        }
    }
}

/// The reason a filesystem location could not be resolved.
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    /// The user's data directory is unknown.
    #[error("the user data directory could not be determined")]
    DataDir,
    /// The user's home directory is unknown.
    #[error("the user home directory could not be determined")]
    HomeDir,
}

/// Returns the application directory holding the config, the socket and the installed binary.
///
/// # Errors
///
/// Returns [`PathError::DataDir`] when the user's data directory is unknown.
///
/// # Examples
///
/// ```
/// use ralphex_macos_runner::paths::{self, Profile};
///
/// let dir = paths::app_dir().unwrap();
/// assert!(dir.ends_with(Profile::current().app_dir_name()));
/// ```
pub fn app_dir() -> Result<PathBuf, PathError> {
    let Some(root) = dirs::data_dir() else {
        return Err(PathError::DataDir);
    };
    Ok(join_app_dir(&root, Profile::current()))
}

/// Returns the application directory under `root` for `profile`.
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// use ralphex_macos_runner::paths::{self, Profile};
///
/// let dir = paths::join_app_dir(Path::new("/tmp"), Profile::Release);
/// assert_eq!(dir, Path::new("/tmp/ralphex-macos-runner"));
/// ```
#[must_use]
pub fn join_app_dir(root: &Path, profile: Profile) -> PathBuf {
    root.join(profile.app_dir_name())
}

/// Returns the path of the daemon's `config.toml`.
///
/// # Errors
///
/// Returns [`PathError::DataDir`] when the user's data directory is unknown.
///
/// # Examples
///
/// ```
/// let path = ralphex_macos_runner::paths::config_path().unwrap();
/// assert!(path.ends_with("config.toml"));
/// ```
pub fn config_path() -> Result<PathBuf, PathError> {
    Ok(app_dir()?.join("config.toml"))
}

/// Returns the path of the Unix socket the daemon listens on.
///
/// # Errors
///
/// Returns [`PathError::DataDir`] when the user's data directory is unknown.
///
/// # Examples
///
/// ```
/// let path = ralphex_macos_runner::paths::socket_path().unwrap();
/// assert!(path.ends_with("daemon.sock"));
/// ```
pub fn socket_path() -> Result<PathBuf, PathError> {
    Ok(app_dir()?.join("daemon.sock"))
}

/// Returns the stable path of the daemon binary launchd runs.
///
/// # Errors
///
/// Returns [`PathError::DataDir`] when the user's data directory is unknown.
///
/// # Examples
///
/// ```
/// let path = ralphex_macos_runner::paths::daemon_binary_path().unwrap();
/// assert!(path.ends_with("bin/ralphex-macos-runner"));
/// ```
pub fn daemon_binary_path() -> Result<PathBuf, PathError> {
    Ok(app_dir()?.join("bin").join(APP_NAME))
}

/// Returns the directory holding the daemon's launchd log files.
///
/// # Errors
///
/// Returns [`PathError::HomeDir`] when the user's home directory is unknown.
///
/// # Examples
///
/// ```
/// use ralphex_macos_runner::paths::{self, Profile};
///
/// let dir = paths::log_dir().unwrap();
/// assert!(dir.ends_with(Profile::current().app_dir_name()));
/// ```
pub fn log_dir() -> Result<PathBuf, PathError> {
    let Some(home) = dirs::home_dir() else {
        return Err(PathError::HomeDir);
    };
    Ok(join_app_dir(
        &home.join("Library").join("Logs"),
        Profile::current(),
    ))
}

/// Returns the path launchd writes the daemon's standard output to.
///
/// # Errors
///
/// Returns [`PathError::HomeDir`] when the user's home directory is unknown.
///
/// # Examples
///
/// ```
/// let path = ralphex_macos_runner::paths::daemon_stdout_path().unwrap();
/// assert!(path.ends_with("daemon.out.log"));
/// ```
pub fn daemon_stdout_path() -> Result<PathBuf, PathError> {
    Ok(log_dir()?.join("daemon.out.log"))
}

/// Returns the path launchd writes the daemon's standard error to.
///
/// # Errors
///
/// Returns [`PathError::HomeDir`] when the user's home directory is unknown.
///
/// # Examples
///
/// ```
/// let path = ralphex_macos_runner::paths::daemon_stderr_path().unwrap();
/// assert!(path.ends_with("daemon.err.log"));
/// ```
pub fn daemon_stderr_path() -> Result<PathBuf, PathError> {
    Ok(log_dir()?.join("daemon.err.log"))
}

/// Returns the launchd label of the daemon's user agent for this profile.
///
/// A development build registers under a label of its own, so `rxd install`
/// from the target directory never boots the installed daemon out.
///
/// # Examples
///
/// ```
/// use ralphex_macos_runner::paths::{self, Profile};
///
/// assert_eq!(
///     paths::launchd_label(),
///     format!("dev.pkarpovich.{}", Profile::current().app_dir_name())
/// );
/// ```
#[must_use]
pub fn launchd_label() -> String {
    format!("{LAUNCHD_PREFIX}.{}", Profile::current().app_dir_name())
}

/// Returns the path of the daemon's launchd property list.
///
/// # Errors
///
/// Returns [`PathError::HomeDir`] when the user's home directory is unknown.
///
/// # Examples
///
/// ```
/// let path = ralphex_macos_runner::paths::launch_agent_path().unwrap();
/// assert!(path.ends_with("dev.pkarpovich.ralphex-macos-runner-dev.plist"));
/// ```
pub fn launch_agent_path() -> Result<PathBuf, PathError> {
    let Some(home) = dirs::home_dir() else {
        return Err(PathError::HomeDir);
    };
    Ok(home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{}.plist", launchd_label())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_have_different_app_dir_names() {
        assert_ne!(
            Profile::Release.app_dir_name(),
            Profile::Debug.app_dir_name()
        );
    }

    #[test]
    fn debug_app_dir_name_carries_the_dev_suffix() {
        assert_eq!(Profile::Debug.app_dir_name(), format!("{APP_NAME}-dev"));
    }

    #[test]
    fn current_profile_is_debug_under_test() {
        assert_eq!(Profile::current(), Profile::Debug);
    }

    #[test]
    fn join_app_dir_differs_per_profile() {
        let root = Path::new("/tmp/root");
        assert_ne!(
            join_app_dir(root, Profile::Release),
            join_app_dir(root, Profile::Debug)
        );
    }

    #[test]
    fn config_socket_and_binary_live_in_the_app_dir() {
        let dir = app_dir().unwrap();
        assert_eq!(config_path().unwrap(), dir.join("config.toml"));
        assert_eq!(socket_path().unwrap(), dir.join("daemon.sock"));
        assert_eq!(
            daemon_binary_path().unwrap(),
            dir.join("bin").join(APP_NAME)
        );
    }

    #[test]
    fn app_dir_lives_under_the_data_dir() {
        let root = dirs::data_dir().unwrap();
        assert!(app_dir().unwrap().starts_with(&root));
    }

    #[test]
    fn log_paths_live_under_library_logs() {
        let home = dirs::home_dir().unwrap();
        let root = home.join("Library").join("Logs");
        let dir = log_dir().unwrap();
        assert!(dir.starts_with(&root));
        assert_eq!(daemon_stdout_path().unwrap(), dir.join("daemon.out.log"));
        assert_eq!(daemon_stderr_path().unwrap(), dir.join("daemon.err.log"));
    }

    #[test]
    fn launch_agent_lives_under_library_launch_agents() {
        let home = dirs::home_dir().unwrap();
        let path = launch_agent_path().unwrap();
        assert!(path.starts_with(home.join("Library").join("LaunchAgents")));
        assert_eq!(
            path.file_name().unwrap(),
            format!("{}.plist", launchd_label()).as_str()
        );
    }

    #[test]
    fn the_launchd_label_carries_the_profile_of_the_build() {
        assert_eq!(launchd_label(), "dev.pkarpovich.ralphex-macos-runner-dev");
        assert_ne!(
            launchd_label(),
            format!("{LAUNCHD_PREFIX}.{}", Profile::Release.app_dir_name())
        );
    }
}
