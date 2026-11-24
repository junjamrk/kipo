use std::path::PathBuf;

use kp_model::ContentHash;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error(transparent)]
    Model(#[from] kp_model::KpError),
    #[error("SQLite operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid stored JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("embedding operation failed: {0}")]
    Embedding(String),
    #[error("library is not initialized at {0}")]
    NotInitialized(PathBuf),
    #[error("library already exists at {0}")]
    AlreadyInitialized(PathBuf),
    #[error("unsupported or inconsistent schema")]
    IncompatibleSchema,
    #[error("record has been purged (tombstone {0})")]
    Tombstoned(ContentHash),
    #[error("conflicting immutable record")]
    Conflict,
    #[error("record not found")]
    NotFound,
    #[error("permission denied")]
    PermissionDenied,
}

pub type Result<T> = std::result::Result<T, StoreError>;
