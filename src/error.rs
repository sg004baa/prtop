use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("Transient HTTP error: {0}")]
    Transient(#[from] reqwest::Error),

    #[error("GitHub rate limited, retry after {retry_after_secs}s ({details})")]
    RateLimited {
        retry_after_secs: u64,
        details: String,
    },

    #[error("GitHub temporarily unavailable, retry after {retry_after_secs}s ({details})")]
    ServiceUnavailable {
        retry_after_secs: u64,
        details: String,
    },

    #[error("Authentication error: {0}")]
    Auth(String),

    #[error("GraphQL error: {0}")]
    GraphQl(String),

    #[error("Config error: {0}")]
    Config(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
