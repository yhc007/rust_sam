//! Top-level error type for Sam crates.

use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum SamError {
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("Failed to parse config at {path}: {source}")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("Missing iMessage handle — whitelist is empty")]
    MissingHandle,

    #[error("Permission denied: {0}")]
    Permission(String),

    #[error("External error: {0}")]
    External(String),

    #[error("Claude API error: {0}")]
    ClaudeApi(String),

    #[error("Token budget exceeded: used {used} of {limit}")]
    BudgetExceeded { used: u64, limit: u64 },

    #[error("API key not found: {0}")]
    ApiKeyMissing(String),
}

impl SamError {
    /// Helper: wrap an I/O error together with the path that produced it.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
