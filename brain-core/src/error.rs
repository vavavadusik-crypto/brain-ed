//! Error types returned by storage implementations.

use thiserror::Error;

/// Result type used by the storage abstraction.
pub type StorageResult<T> = Result<T, StorageError>;

/// Errors produced while validating or storing memory records.
#[derive(Debug, Error)]
pub enum StorageError {
    /// An operation violated a domain invariant.
    #[error("invalid data: {0}")]
    InvalidData(String),

    /// A requested logical record does not exist at the requested time.
    #[error("record not found: {0}")]
    NotFound(String),

    /// The database was created by a newer engine version.
    #[error("database schema version {found} is newer than supported version {supported}")]
    SchemaTooNew {
        /// Version found in the database.
        found: u32,
        /// Highest version understood by this build.
        supported: u32,
    },

    /// A stored enum or JSON value passed SQL checks but could not be decoded.
    #[error("database contains invalid {field}: {value}")]
    CorruptData {
        /// Name of the invalid field.
        field: &'static str,
        /// Invalid stored value.
        value: String,
    },

    /// SQLite rejected registration of the statically linked vector extension.
    #[error("could not register sqlite-vec (SQLite error code {0})")]
    VectorExtension(i32),

    /// The single-writer actor stopped before completing an operation.
    #[error("storage actor is unavailable: {0}")]
    ActorUnavailable(String),

    /// An embedding has a width unsupported by the statically compiled indexes.
    #[error("unsupported embedding dimensions {found}; expected 256 or 384")]
    UnsupportedEmbeddingDimensions {
        /// Width supplied by the caller.
        found: usize,
    },

    /// Error returned by SQLite.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}
