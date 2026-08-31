use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;
use thiserror::Error;

const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Errors raised while loading Moh's backend configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The configuration file could not be read.
    #[error("could not read configuration {path}: {source}")]
    Read {
        /// The configuration file path.
        path: PathBuf,
        /// The operating-system error.
        #[source]
        source: std::io::Error,
    },
    /// The configuration contents were not valid strict TOML.
    #[error("could not parse configuration {path} at {field}: {message}")]
    Parse {
        /// The configuration file path.
        path: PathBuf,
        /// The configuration field identified without exposing its value.
        field: String,
        /// A sanitized description of the invalid configuration.
        message: &'static str,
    },
    /// The configured idle timeout was not positive.
    #[error(
        "configuration {path} has an invalid server.idle_timeout; it must be greater than zero"
    )]
    ZeroIdleTimeout {
        /// The configuration file path.
        path: PathBuf,
    },
}

/// Configuration read once by the backend during startup.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MohConfig {
    /// Backend server configuration.
    pub server: ServerConfig,
}

impl MohConfig {
    /// Loads configuration from `path`, returning defaults when the file is absent.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match fs::read_to_string(path) {
            Ok(text) => Self::parse(&text, path),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Parses strict TOML configuration text associated with `path`.
    pub fn parse(text: &str, path: &Path) -> Result<Self, ConfigError> {
        let config =
            toml::from_str::<Self>(text).map_err(|source| Self::parse_error(path, &source))?;
        if config.server.idle_timeout.is_zero() {
            return Err(ConfigError::ZeroIdleTimeout {
                path: path.to_path_buf(),
            });
        }
        Ok(config)
    }

    fn parse_error(path: &Path, source: &toml::de::Error) -> ConfigError {
        let field = source
            .message()
            .strip_prefix("unknown field `")
            .and_then(|message| message.split_once('`'))
            .map(|(field, _)| field.to_owned())
            .unwrap_or_else(|| "configuration".to_owned());
        let message = if field == "configuration" {
            "invalid TOML configuration"
        } else {
            "unknown field"
        };
        ConfigError::Parse {
            path: path.to_path_buf(),
            field,
            message,
        }
    }
}

/// Settings that control the background backend server.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Time with no connected clients or active work before automatic shutdown.
    #[serde(with = "humantime_serde")]
    pub idle_timeout: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
        }
    }
}
