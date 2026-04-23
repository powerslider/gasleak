//! Library error type.
//!
//! Each variant carries the data it needs to format a good message. Paths
//! stay as `PathBuf`, causes stay as `#[source]` so `anyhow` prints a full
//! chain, and one-off strings (`InvalidTimestamp`) only appear where the
//! underlying type lacks its own `Error` impl.

use std::path::PathBuf;
use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{context}: {source}")]
    Aws {
        context: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("invalid timestamp from AWS SDK: {0}")]
    InvalidTimestamp(String),

    #[error("config file not found: {}", .path.display())]
    ConfigMissing { path: PathBuf },

    #[error("failed to read config file {}", .path.display())]
    ConfigRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse config file {}", .path.display())]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("instance '{id}' not found in region '{region}'")]
    InstanceNotFound { id: String, region: String },

    #[error(
        "no AWS region configured. Set AWS_REGION, export an AWS_PROFILE \
         whose config defines a region, or pass one in a gasleak config file."
    )]
    RegionNotConfigured,

    #[error(
        "Slack is enabled but no webhook URL was resolved. Set `[slack] \
         webhook_url` in gasleak.toml, or export $GASLEAK_SLACK_WEBHOOK. \
         Never pass the URL as a CLI flag."
    )]
    SlackWebhookMissing,

    #[error("invalid Slack config: {0}")]
    SlackConfigInvalid(String),
}

impl Error {
    pub fn aws(
        context: &'static str,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Error::Aws {
            context,
            source: Box::new(source),
        }
    }
}
