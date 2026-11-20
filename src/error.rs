//! Error types for `mount-fs`.
//!
//! A single [`AppError`] enum is used across the crate so that command
//! handlers can return a uniform [`Result`].

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, AppError>;

/// Top-level application error.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// A configuration file could not be read or was invalid.
    #[error("configuration error: {0}")]
    Config(String),

    /// An I/O operation failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A requested item (engine, profile, key) was not found.
    #[error("not found: {0}")]
    NotFound(String),

    /// A credential could not be obtained, written, or validated.
    #[error("credential error: {0}")]
    Credential(String),

    /// A download failed.
    #[error("network error: {0}")]
    Network(String),

    /// An archive could not be read or extracted.
    #[error("archive error: {0}")]
    Archive(String),

    /// An integrity/checksum check failed.
    #[error("integrity error: {0}")]
    Integrity(String),

    /// A requested feature or engine is not supported.
    #[error("unsupported: {0}")]
    Unsupported(String),
}
