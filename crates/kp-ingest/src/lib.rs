//! Local daytime capture and bounded ingestion, with optional sleep-time attribution.

pub mod code_intel;
pub mod connect;
pub mod doc_router;
pub mod dream;
mod hook_cursor;
pub mod hooks;
mod office;
pub mod session_miner;
pub mod signals;
pub mod skill_pkg;
pub mod source_miner;
mod subtitles;
mod text;
mod web_and_media;

use std::io::Read;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error(transparent)]
    Model(#[from] kp_model::KpError),
    #[error(transparent)]
    Store(#[from] kp_store::StoreError),
    #[error(transparent)]
    Retrieve(#[from] kp_retrieve::RetrieveError),
    #[error("filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid pattern: {0}")]
    Pattern(#[from] regex::Error),
    #[error("invalid ZIP: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("invalid XML: {0}")]
    Xml(#[from] quick_xml::Error),
    #[error("invalid PDF: {0}")]
    Pdf(#[from] lopdf::Error),
    #[error("{0}")]
    Invalid(&'static str),
    #[error("input exceeds {0} byte limit")]
    Limit(usize),
}

pub type Result<T> = std::result::Result<T, IngestError>;
pub const MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;

pub fn read_bounded(reader: impl Read, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(IngestError::Limit(limit));
    }
    Ok(bytes)
}

pub(crate) fn read_file(path: &Path) -> Result<Vec<u8>> {
    read_bounded(std::fs::File::open(path)?, MAX_INPUT_BYTES)
}
