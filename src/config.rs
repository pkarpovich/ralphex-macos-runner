//! The daemon's configuration file.
//!
//! [`Config`] is the whole of `config.toml`: the farm to talk to, the token to
//! talk with, the name this runner registers under and two settings with
//! defaults. The three first are required, because a daemon without them can do
//! nothing but fail every call. There is no slot count: this runner takes one
//! job at a time.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::protocol::types::RunnerName;

/// The time a shutdown lets a running job finish before it is stopped.
pub const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(120);

/// The ralphex binary a configuration without `ralphex_bin` runs.
pub const DEFAULT_RALPHEX_BIN: &str = "ralphex";

/// Everything the daemon reads out of `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The base URL of the farm.
    pub farm_url: String,
    /// The bearer token every call carries.
    pub token: String,
    /// The name this runner registers under.
    pub name: RunnerName,
    /// The time a shutdown lets a running job finish.
    pub drain_timeout: Duration,
    /// The ralphex binary a run spawns.
    pub ralphex_bin: String,
}

/// Why a configuration could not be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("{path} could not be read: {message}")]
    Read {
        /// The path that was tried.
        path: String,
        /// The reason the read failed.
        message: String,
    },
    /// The file is not the TOML this daemon expects.
    #[error("the configuration is not valid TOML: {0}")]
    Parse(String),
    /// A required setting is missing or empty.
    #[error("the configuration has no {0}")]
    Missing(&'static str),
    /// A duration setting does not name a duration.
    #[error("{0} is not a duration such as 30s, 2m or 1h")]
    Duration(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    farm_url: Option<String>,
    token: Option<String>,
    name: Option<String>,
    drain_timeout: Option<String>,
    ralphex_bin: Option<String>,
}

impl Config {
    /// Reads and parses the configuration at `path`.
    ///
    /// A file other users can read is logged as a warning and used anyway.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Read`] when the file cannot be read,
    /// [`ConfigError::Parse`] when it is not the expected TOML,
    /// [`ConfigError::Missing`] when a required setting is absent and
    /// [`ConfigError::Duration`] when `drain_timeout` names no duration.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) => {
                return Err(ConfigError::Read {
                    path: path.display().to_string(),
                    message: error.to_string(),
                });
            }
        };
        if let Ok(metadata) = std::fs::metadata(path)
            && let Some(warning) = permissions_warning(metadata.permissions().mode())
        {
            tracing::warn!("{}: {warning}", path.display());
        }
        Config::parse(&contents)
    }

    /// Parses the configuration in `contents`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Parse`] when the text is not the expected TOML,
    /// [`ConfigError::Missing`] when a required setting is absent and
    /// [`ConfigError::Duration`] when `drain_timeout` names no duration.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use ralphex_macos_runner::config::Config;
    ///
    /// let config = Config::parse(
    ///     r#"
    ///     farm_url = "http://farm.example:7077"
    ///     token = "secret"
    ///     name = "mbp-native"
    ///     "#,
    /// )
    /// .unwrap();
    /// assert_eq!(config.drain_timeout, Duration::from_secs(120));
    /// assert_eq!(config.ralphex_bin, "ralphex");
    /// ```
    pub fn parse(contents: &str) -> Result<Config, ConfigError> {
        let file = match toml::from_str::<FileConfig>(contents) {
            Ok(file) => file,
            Err(error) => return Err(ConfigError::Parse(error.to_string())),
        };
        let FileConfig {
            farm_url,
            token,
            name,
            drain_timeout,
            ralphex_bin,
        } = file;
        let farm_url = required(farm_url, "farm_url")?;
        let token = required(token, "token")?;
        let name = required(name, "name")?;
        let drain_timeout = match drain_timeout {
            Some(drain_timeout) => parse_duration(&drain_timeout)?,
            None => DEFAULT_DRAIN_TIMEOUT,
        };
        let ralphex_bin = match ralphex_bin {
            Some(ralphex_bin) => ralphex_bin,
            None => DEFAULT_RALPHEX_BIN.to_string(),
        };
        Ok(Config {
            farm_url,
            token,
            name: RunnerName(name),
            drain_timeout,
            ralphex_bin,
        })
    }
}

/// Returns the warning a configuration file with mode `mode` deserves.
///
/// # Examples
///
/// ```
/// use ralphex_macos_runner::config::permissions_warning;
///
/// assert_eq!(permissions_warning(0o600), None);
/// assert!(permissions_warning(0o644).is_some());
/// ```
#[must_use]
pub fn permissions_warning(mode: u32) -> Option<String> {
    let shared = mode & 0o077;
    if shared == 0 {
        return None;
    }
    Some(format!(
        "the configuration holds a token and is readable by others (mode {:04o})",
        mode & 0o7777
    ))
}

fn required(value: Option<String>, field: &'static str) -> Result<String, ConfigError> {
    let Some(value) = value else {
        return Err(ConfigError::Missing(field));
    };
    if value.trim().is_empty() {
        return Err(ConfigError::Missing(field));
    }
    Ok(value)
}

fn parse_duration(value: &str) -> Result<Duration, ConfigError> {
    let text = value.trim();
    let mut digits = String::new();
    let mut unit = String::new();
    for character in text.chars() {
        if character.is_ascii_digit() && unit.is_empty() {
            digits.push(character);
            continue;
        }
        unit.push(character);
    }
    let Ok(amount) = digits.parse::<u64>() else {
        return Err(ConfigError::Duration(value.to_string()));
    };
    let factor = if unit == "s" {
        1
    } else if unit == "m" {
        60
    } else if unit == "h" {
        3600
    } else {
        return Err(ConfigError::Duration(value.to_string()));
    };
    let Some(seconds) = amount.checked_mul(factor) else {
        return Err(ConfigError::Duration(value.to_string()));
    };
    Ok(Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMPLETE: &str = r#"
farm_url = "http://farm.example:7077"
token = "secret"
name = "mbp-native"
drain_timeout = "5m"
ralphex_bin = "/opt/homebrew/bin/ralphex"
"#;

    const MINIMAL: &str = r#"
farm_url = "http://farm.example:7077"
token = "secret"
name = "mbp-native"
"#;

    #[test]
    fn every_setting_is_read() {
        let Config {
            farm_url,
            token,
            name,
            drain_timeout,
            ralphex_bin,
        } = Config::parse(COMPLETE).unwrap();
        assert_eq!(farm_url, "http://farm.example:7077");
        assert_eq!(token, "secret");
        assert_eq!(name, RunnerName("mbp-native".to_string()));
        assert_eq!(drain_timeout, Duration::from_secs(300));
        assert_eq!(ralphex_bin, "/opt/homebrew/bin/ralphex");
    }

    #[test]
    fn the_optional_settings_have_defaults() {
        let Config {
            farm_url: _,
            token: _,
            name: _,
            drain_timeout,
            ralphex_bin,
        } = Config::parse(MINIMAL).unwrap();
        assert_eq!(drain_timeout, DEFAULT_DRAIN_TIMEOUT);
        assert_eq!(ralphex_bin, DEFAULT_RALPHEX_BIN);
    }

    #[test]
    fn each_required_setting_is_named_when_it_is_absent() {
        let missing = Config::parse(r#"token = "t""#).unwrap_err();
        assert_eq!(missing, ConfigError::Missing("farm_url"));
        let missing = Config::parse(r#"farm_url = "u""#).unwrap_err();
        assert_eq!(missing, ConfigError::Missing("token"));
        let missing = Config::parse(
            r#"
farm_url = "u"
token = "t"
"#,
        )
        .unwrap_err();
        assert_eq!(missing, ConfigError::Missing("name"));
    }

    #[test]
    fn an_empty_required_setting_counts_as_absent() {
        let missing = Config::parse(
            r#"
farm_url = "u"
token = "   "
name = "n"
"#,
        )
        .unwrap_err();
        assert_eq!(missing, ConfigError::Missing("token"));
    }

    #[test]
    fn a_slot_count_is_refused() {
        let refused = Config::parse(
            r#"
farm_url = "u"
token = "t"
name = "n"
slots = 2
"#,
        )
        .unwrap_err();
        let ConfigError::Parse(message) = refused else {
            panic!("a stray key is a parse error");
        };
        assert!(message.contains("slots"), "{message}");
    }

    #[test]
    fn durations_carry_a_unit() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration(" 1h ").unwrap(), Duration::from_secs(3600));
    }

    #[test]
    fn a_duration_without_a_known_unit_is_refused() {
        for value in ["120", "2 minutes", "m", "2d", ""] {
            let refused = parse_duration(value).unwrap_err();
            assert_eq!(refused, ConfigError::Duration(value.to_string()));
        }
    }

    #[test]
    fn a_bad_duration_names_itself() {
        let refused = Config::parse(
            r#"
farm_url = "u"
token = "t"
name = "n"
drain_timeout = "soon"
"#,
        )
        .unwrap_err();
        assert_eq!(refused, ConfigError::Duration("soon".to_string()));
    }

    #[test]
    fn a_readable_configuration_is_warned_about() {
        assert_eq!(permissions_warning(0o600), None);
        assert_eq!(permissions_warning(0o400), None);
        assert!(permissions_warning(0o640).is_some());
        assert!(permissions_warning(0o604).is_some());
        assert!(permissions_warning(0o100_644).unwrap().contains("0644"));
    }

    #[test]
    fn a_missing_file_names_the_path_it_tried() {
        let error = Config::load(Path::new("/tmp/ralphex-macos-runner-absent.toml")).unwrap_err();
        let ConfigError::Read { path, message: _ } = error else {
            panic!("a missing file is a read error");
        };
        assert_eq!(path, "/tmp/ralphex-macos-runner-absent.toml");
    }

    #[test]
    fn a_file_on_disk_is_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, MINIMAL).unwrap();
        let Config {
            farm_url,
            token: _,
            name: _,
            drain_timeout: _,
            ralphex_bin: _,
        } = Config::load(&path).unwrap();
        assert_eq!(farm_url, "http://farm.example:7077");
    }
}
