//! Unified error type for the Tao chain.

use std::io;

/// Convenience alias used across the workspace.
pub type Result<T, E = TaoError> = std::result::Result<T, E>;

/// Top-level error type. Crate-specific errors should convert into this at the
/// node boundary; library crates may define their own and add a `From` impl.
#[derive(Debug, thiserror::Error)]
pub enum TaoError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("genesis error: {0}")]
    Genesis(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("consensus error: {0}")]
    Consensus(String),

    #[error("runtime/execution error: {0}")]
    Runtime(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("network error: {0}")]
    Network(String),

    #[error("{0}")]
    Other(String),
}

impl TaoError {
    /// Build a generic error from anything string-like.
    pub fn other(msg: impl Into<String>) -> Self {
        TaoError::Other(msg.into())
    }
}
