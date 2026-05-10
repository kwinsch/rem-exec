use thiserror::Error;

#[derive(Error, Debug)]
pub enum RemExecError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("process not found: {id}")]
    ProcessNotFound { id: String },

    #[error("process already exited: {id}")]
    ProcessAlreadyExited { id: String },

    #[error("invalid process ID: {id}")]
    InvalidProcessId { id: String },

    #[error("SSH error: {0}")]
    Ssh(String),

    #[error("daemon not running")]
    DaemonNotRunning,

    #[error("daemon already running")]
    DaemonAlreadyRunning,

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("fork failed: {0}")]
    ForkFailed(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, RemExecError>;
