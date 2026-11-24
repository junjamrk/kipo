use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum KpError {
    #[error("invalid {field}: {reason}")]
    InvalidInput {
        field: &'static str,
        reason: &'static str,
    },
    #[error("invalid input JSON in {uri}: {detail}")]
    InvalidInputJson { uri: String, detail: String },
}

pub type Result<T> = std::result::Result<T, KpError>;

impl KpError {
    pub fn invalid(field: &'static str, reason: &'static str) -> Self {
        Self::InvalidInput { field, reason }
    }

    pub fn invalid_input_json(uri: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::InvalidInputJson {
            uri: uri.into(),
            detail: detail.into(),
        }
    }
}
