use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{FactKind, KpError, Result};

macro_rules! string_id {
    ($name:ident, $validate:expr) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self> {
                let value = value.into();
                if !($validate)(&value) {
                    return Err(KpError::invalid(stringify!($name), "invalid identifier"));
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl TryFrom<String> for $name {
            type Error = KpError;
            fn try_from(value: String) -> Result<Self> {
                Self::new(value)
            }
        }

        impl FromStr for $name {
            type Err = KpError;
            fn from_str(value: &str) -> Result<Self> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
}

string_id!(LibraryId, identifier);
string_id!(ActorId, identifier);
string_id!(SourceId, identifier);
string_id!(FactId, identifier);
string_id!(JustificationId, identifier);
string_id!(ReviewId, identifier);
string_id!(RejectionId, identifier);
string_id!(ConceptId, identifier);
string_id!(ConnectorId, identifier);
string_id!(SourceScopeId, identifier);
string_id!(LogicalKey, |s: &str| !s.trim().is_empty()
    && !s.chars().any(char::is_control));

/// Formats a prefixed opaque external identifier, rejecting bare 64-hex content digests.
pub fn format_opaque_id(prefix: &str, token: &str) -> Result<String> {
    if prefix.is_empty()
        || !prefix
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    {
        return Err(KpError::invalid(
            "opaque_id",
            "prefix must be lowercase ASCII alphanumeric",
        ));
    }
    if token.is_empty()
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(KpError::invalid("opaque_id", "invalid opaque token"));
    }
    Ok(format!("{prefix}_{token}"))
}

/// Returns true if an external identifier looks like an unprefixed 64-hex SHA-256 digest.
pub fn is_bare_content_hash(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A normalized, validated SHA-256 digest. Deserialization enforces the same invariant.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ContentHash(String);

impl ContentHash {
    pub fn digest(bytes: impl AsRef<[u8]>) -> Self {
        Self(hex::encode(Sha256::digest(bytes.as_ref())))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ContentHash {
    type Error = KpError;

    fn try_from(value: String) -> Result<Self> {
        if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(KpError::invalid(
                "ContentHash",
                "expected 64 hexadecimal digits",
            ));
        }
        Ok(Self(value.to_ascii_lowercase()))
    }
}

impl FromStr for ContentHash {
    type Err = KpError;
    fn from_str(value: &str) -> Result<Self> {
        Self::try_from(value.to_owned())
    }
}

impl From<ContentHash> for String {
    fn from(value: ContentHash) -> Self {
        value.0
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct StrongRef {
    pub origin_library: LibraryId,
    pub logical_key: LogicalKey,
    pub content_sha256: ContentHash,
}

/// Hashes a JSON tuple with recursively sorted object keys; array order is significant.
/// Mutable access, validity, and lifecycle metadata are deliberately absent.
pub fn compute_fact_content_hash(
    subject: &str,
    predicate: &str,
    object_json: &Value,
    kind: FactKind,
) -> ContentHash {
    let value = serde_json::json!([subject, predicate, object_json, kind.as_str()]);
    ContentHash::digest(canonical_json(&value))
}

pub fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(object) => {
            let mut keys: Vec<_> = object.keys().collect();
            keys.sort_unstable();
            let fields: Vec<_> = keys
                .into_iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        Value::String(key.clone()),
                        canonical_json(&object[key])
                    )
                })
                .collect();
            format!("{{{}}}", fields.join(","))
        }
        Value::Array(values) => {
            let values: Vec<_> = values.iter().map(canonical_json).collect();
            format!("[{}]", values.join(","))
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => value.to_string(),
    }
}

#[cfg(test)]
#[path = "ids_tests.rs"]
mod tests;
