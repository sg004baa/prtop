use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("Transient HTTP error: {0}")]
    Transient(#[from] reqwest::Error),

    #[error("GitHub rate limited, retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },

    #[error("Authentication error: {0}")]
    Auth(String),

    #[error("GraphQL error: {0}")]
    GraphQl(String),

    #[error("Config error: {0}")]
    Config(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
