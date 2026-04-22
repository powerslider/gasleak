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

    #[error("config: {0}")]
    Config(String),

    #[error("{0}")]
    NotFound(String),
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
