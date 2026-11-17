//! Crate-level error type.

use crate::encoding::{DecodeError, EncodeError};

/// A specialized [`Result`](std::result::Result) for storage operations.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Errors returned by the storage layer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The underlying fjall engine reported an error.
    #[error("storage engine error: {0}")]
    Fjall(#[from] fjall::Error),

    /// The database directory was written by an incompatible build.
    #[error("unsupported on-disk format version {found} (this build supports {supported})")]
    UnsupportedFormatVersion {
        /// Version found in the `meta` keyspace.
        found: u32,
        /// Version this build reads and writes.
        supported: u32,
    },

    /// The `meta` keyspace is missing an entry or holds a malformed value.
    #[error("corrupt engine metadata: {0}")]
    CorruptMeta(&'static str),

    /// A key could not be encoded.
    #[error(transparent)]
    Encode(#[from] EncodeError),

    /// A key could not be decoded.
    #[error(transparent)]
    Decode(#[from] DecodeError),
}
