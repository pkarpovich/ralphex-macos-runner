//! The daemon's configuration file.
//!
//! [`Config`] is the whole of `config.toml`: the farm to talk to, who the runner
//! is at the authorization server, the key it signs with and two settings with
//! defaults. Every setting but the two is required, because a daemon without
//! one can do nothing but fail every call. The farm, the issuer and the token
//! endpoint are reached over `https` only; plain `http` is accepted for a
//! loopback host and nothing else, so no token or assertion can leave the
//! machine unencrypted. There is no slot count: this runner takes one job at a
//! time.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use url::{Host, Url};

use crate::auth::Credentials;
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
    /// Who the runner is at the authorization server.
    pub credentials: Credentials,
    /// The RSA private key the client assertions are signed with.
    pub key_file: PathBuf,
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
    /// A setting of the previous protocol is still there.
    #[error("{setting} is no longer a setting: {replacement}")]
    Removed {
        /// The setting that is no longer read.
        setting: &'static str,
        /// What took its place.
        replacement: &'static str,
    },
    /// A URL is neither `https` nor plain `http` on a loopback host.
    #[error("{setting} must be an https URL, or http on 127.0.0.1, [::1] or localhost: {value}")]
    Insecure {
        /// The setting that holds the URL.
        setting: &'static str,
        /// The value it holds.
        value: String,
    },
}

const TOKEN_REPLACED: &str = "the runner gets its access tokens from the authorization server now; set issuer, token_endpoint, client_id, key_id, key_file and resource instead";

const NAME_REPLACED: &str = "the runner's name is its client id now; set client_id instead";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    farm_url: Option<String>,
    issuer: Option<String>,
    token_endpoint: Option<String>,
    client_id: Option<String>,
    key_id: Option<String>,
    key_file: Option<String>,
    resource: Option<String>,
    drain_timeout: Option<String>,
    ralphex_bin: Option<String>,
    token: Option<toml::Value>,
    name: Option<toml::Value>,
}

impl Config {
    /// Reads and parses the configuration at `path`.
    ///
    /// The key file is not read here: only the daemon needs it, and
    /// [`crate::auth::SigningKey::load`] refuses one others can read.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Read`] when the file cannot be read and every
    /// error [`Config::parse`] returns.
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
        Config::parse(&contents)
    }

    /// Parses the configuration in `contents`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Parse`] when the text is not the expected TOML,
    /// [`ConfigError::Removed`] when it still holds `token` or `name`,
    /// [`ConfigError::Missing`] when a required setting is absent,
    /// [`ConfigError::Insecure`] when a URL is not `https` and not plain
    /// `http` on a loopback host and [`ConfigError::Duration`] when
    /// `drain_timeout` names no duration.
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
    ///     farm_url = "https://ralphex-farm.example.com"
    ///     issuer = "https://auth.example.com"
    ///     token_endpoint = "https://auth.example.com/api/oidc/token"
    ///     client_id = "farm-runner-native"
    ///     key_id = "farm-runner-native-1"
    ///     key_file = "/Users/me/.config/ralphex-macos-runner/runner.pem"
    ///     resource = "https://ralphex-farm.example.com/api"
    ///     "#,
    /// )
    /// .unwrap();
    /// assert_eq!(config.credentials.client_id.as_str(), "farm-runner-native");
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
            issuer,
            token_endpoint,
            client_id,
            key_id,
            key_file,
            resource,
            drain_timeout,
            ralphex_bin,
            token,
            name,
        } = file;
        if token.is_some() {
            return Err(ConfigError::Removed {
                setting: "token",
                replacement: TOKEN_REPLACED,
            });
        }
        if name.is_some() {
            return Err(ConfigError::Removed {
                setting: "name",
                replacement: NAME_REPLACED,
            });
        }
        let farm_url = secure(required(farm_url, "farm_url")?, "farm_url")?;
        let issuer = secure(required(issuer, "issuer")?, "issuer")?;
        let token_endpoint = secure(
            required(token_endpoint, "token_endpoint")?,
            "token_endpoint",
        )?;
        let client_id = required(client_id, "client_id")?;
        let key_id = required(key_id, "key_id")?;
        let key_file = required(key_file, "key_file")?;
        let resource = required(resource, "resource")?;
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
            credentials: Credentials {
                issuer,
                token_endpoint,
                client_id: RunnerName(client_id),
                key_id,
                resource,
            },
            key_file: PathBuf::from(key_file),
            drain_timeout,
            ralphex_bin,
        })
    }
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

fn secure(value: String, setting: &'static str) -> Result<String, ConfigError> {
    let Ok(url) = Url::parse(&value) else {
        return Err(ConfigError::Insecure { setting, value });
    };
    let accepted = match url.scheme() {
        "https" => url.host().is_some(),
        "http" => match url.host() {
            Some(Host::Ipv4(address)) => address == Ipv4Addr::LOCALHOST,
            Some(Host::Ipv6(address)) => address == Ipv6Addr::LOCALHOST,
            Some(Host::Domain(domain)) => domain == "localhost",
            None => false,
        },
        _ => false,
    };
    if !accepted {
        return Err(ConfigError::Insecure { setting, value });
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
farm_url = "https://ralphex-farm.example.com"
issuer = "https://auth.example.com"
token_endpoint = "https://auth.example.com/api/oidc/token"
client_id = "farm-runner-native"
key_id = "farm-runner-native-1"
key_file = "/keys/runner.pem"
resource = "https://ralphex-farm.example.com/api"
drain_timeout = "5m"
ralphex_bin = "/opt/homebrew/bin/ralphex"
"#;

    const REQUIRED: [&str; 7] = [
        "farm_url",
        "issuer",
        "token_endpoint",
        "client_id",
        "key_id",
        "key_file",
        "resource",
    ];

    fn without(setting: &str) -> String {
        let mut kept = String::new();
        for line in COMPLETE.lines() {
            if line.starts_with(&format!("{setting} =")) {
                continue;
            }
            kept.push_str(line);
            kept.push('\n');
        }
        kept
    }

    fn with(setting: &str, value: &str) -> String {
        let mut text = without(setting);
        text.push_str(&format!("{setting} = \"{value}\"\n"));
        text
    }

    #[test]
    fn every_setting_is_read() {
        let Config {
            farm_url,
            credentials,
            key_file,
            drain_timeout,
            ralphex_bin,
        } = Config::parse(COMPLETE).unwrap();
        let Credentials {
            issuer,
            token_endpoint,
            client_id,
            key_id,
            resource,
        } = credentials;
        assert_eq!(farm_url, "https://ralphex-farm.example.com");
        assert_eq!(issuer, "https://auth.example.com");
        assert_eq!(token_endpoint, "https://auth.example.com/api/oidc/token");
        assert_eq!(client_id, RunnerName("farm-runner-native".to_string()));
        assert_eq!(key_id, "farm-runner-native-1");
        assert_eq!(key_file, PathBuf::from("/keys/runner.pem"));
        assert_eq!(resource, "https://ralphex-farm.example.com/api");
        assert_eq!(drain_timeout, Duration::from_secs(300));
        assert_eq!(ralphex_bin, "/opt/homebrew/bin/ralphex");
    }

    #[test]
    fn the_optional_settings_have_defaults() {
        let minimal = without("drain_timeout");
        let minimal = minimal.replace("ralphex_bin = \"/opt/homebrew/bin/ralphex\"\n", "");
        let Config {
            farm_url: _,
            credentials: _,
            key_file: _,
            drain_timeout,
            ralphex_bin,
        } = Config::parse(&minimal).unwrap();
        assert_eq!(drain_timeout, DEFAULT_DRAIN_TIMEOUT);
        assert_eq!(ralphex_bin, DEFAULT_RALPHEX_BIN);
    }

    #[test]
    fn each_required_setting_is_named_when_it_is_absent() {
        for setting in REQUIRED {
            let missing = Config::parse(&without(setting)).unwrap_err();
            assert_eq!(missing, ConfigError::Missing(setting));
        }
    }

    #[test]
    fn an_empty_required_setting_counts_as_absent() {
        for setting in REQUIRED {
            let missing = Config::parse(&with(setting, "   ")).unwrap_err();
            assert_eq!(missing, ConfigError::Missing(setting));
        }
    }

    #[test]
    fn a_configuration_with_a_token_says_what_replaced_it() {
        let mut text = COMPLETE.to_string();
        text.push_str("token = \"secret\"\n");
        let refused = Config::parse(&text).unwrap_err();
        assert_eq!(
            refused,
            ConfigError::Removed {
                setting: "token",
                replacement: TOKEN_REPLACED,
            }
        );
        let message = refused.to_string();
        assert!(!message.contains("secret"), "{message}");
        for setting in REQUIRED {
            if setting == "farm_url" {
                continue;
            }
            assert!(message.contains(setting), "{message}");
        }
    }

    #[test]
    fn a_configuration_of_the_previous_protocol_is_refused_for_its_token() {
        let refused = Config::parse(
            r#"
farm_url = "https://ralphex-farm.example.com"
token = "secret"
name = "mbp-native"
"#,
        )
        .unwrap_err();
        let ConfigError::Removed {
            setting,
            replacement: _,
        } = refused
        else {
            panic!("an old configuration is refused for what it still holds");
        };
        assert_eq!(setting, "token");
    }

    #[test]
    fn a_configuration_with_a_name_says_what_replaced_it() {
        let mut text = COMPLETE.to_string();
        text.push_str("name = \"mbp-native\"\n");
        let refused = Config::parse(&text).unwrap_err();
        assert_eq!(
            refused,
            ConfigError::Removed {
                setting: "name",
                replacement: NAME_REPLACED,
            }
        );
        assert!(refused.to_string().contains("client_id"), "{refused}");
    }

    #[test]
    fn https_urls_and_plain_http_on_loopback_are_accepted() {
        for setting in ["farm_url", "issuer", "token_endpoint"] {
            for value in [
                "https://example.com",
                "https://auth.example.com:8443/api/oidc/token",
                "http://127.0.0.1:7077",
                "http://[::1]:7077/api/oidc/token",
                "http://localhost:7077",
                "http://LOCALHOST",
            ] {
                assert!(
                    Config::parse(&with(setting, value)).is_ok(),
                    "{setting} = {value}"
                );
            }
        }
    }

    #[test]
    fn plain_http_anywhere_but_loopback_is_refused() {
        for setting in ["farm_url", "issuer", "token_endpoint"] {
            for value in [
                "http://example.com",
                "http://ralphex-farm.example.com:7077",
                "http://127.0.0.1.example.com",
                "http://localhost.example.com",
                "http://localhost@example.com",
                "http://127.0.0.1@example.com",
                "http://127.0.0.2",
                "http://192.0.2.10",
                "http://[::2]",
                "ftp://example.com",
                "file:///tmp/token",
                "https://",
                "example.com",
                "not a url",
            ] {
                let refused = Config::parse(&with(setting, value)).unwrap_err();
                assert_eq!(
                    refused,
                    ConfigError::Insecure {
                        setting,
                        value: value.to_string(),
                    },
                    "{setting} = {value}"
                );
            }
        }
    }

    #[test]
    fn a_slot_count_is_refused() {
        let mut text = COMPLETE.to_string();
        text.push_str("slots = 2\n");
        let refused = Config::parse(&text).unwrap_err();
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
        let refused = Config::parse(&with("drain_timeout", "soon")).unwrap_err();
        assert_eq!(refused, ConfigError::Duration("soon".to_string()));
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
        std::fs::write(&path, COMPLETE).unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.farm_url, "https://ralphex-farm.example.com");
    }
}
