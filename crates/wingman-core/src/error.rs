use thiserror::Error;

#[derive(Debug, Error)]
pub enum WingmanError {
    #[error("config error: {0}")]
    Config(#[from] wingman_config::ConfigError),

    #[error("provider error: {0}")]
    Provider(String),

    #[error("tool error: {0}")]
    Tool(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("{0}")]
    Other(String),
}

pub type Result<T, E = WingmanError> = std::result::Result<T, E>;
