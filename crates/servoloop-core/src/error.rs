use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("model error: {0}")]
    Model(String),
    #[error("tool error: {0}")]
    Tool(String),
    #[error("safety violation: {0}")]
    Safety(String),
    #[error("run stopped")]
    Stopped,
    #[error("maximum turn count reached ({0})")]
    MaxTurns(usize),
}

pub type Result<T> = std::result::Result<T, Error>;
