use std::sync::Arc;

use kp_model::KpError;
use rusqlite::Connection;

use crate::{Result, StoreError};

/// Minimum L2 norm required for a non-degenerate model embedding vector (GBrain `embedding-guard`).
pub const MIN_NON_DEGENERATE_NORM: f64 = 1e-6;

/// Implementations return finite, unit-length vectors (or an all-zero sentinel in custom tests).
pub trait Embedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>>;

    /// Stable model identifier persisted in `chunk_vectors.model_id` / `fact_vectors.model_id`.
    /// Custom unit-test `impl Embedder` types default to `"custom"`.
    fn model_id(&self) -> &str {
        "custom"
    }

    /// Expected output vector dimensionality when known statically (`None` when auto-detected).
    fn expected_dims(&self) -> Option<usize> {
        None
    }

    /// Returns `false` when vector indexing/search is disabled (`--no-embedding`) or a local ONNX
    /// model has not yet been downloaded, causing writes to skip vector rows and queries to fall
    /// back seamlessly to FTS5.
    fn is_enabled(&self) -> bool {
        true
    }

    /// Returns `true` ONLY for purely local / deterministic embedders (`local:*`, `hash:v1`, `none`).
    /// Cloud and external HTTP services MUST return `false` so `RawOnly` sources never leave the host.
    fn allows_raw_only(&self) -> bool {
        let id = self.model_id();
        matches!(id, "hash:v1" | "none") || id.starts_with("local:")
    }

    /// Batch embedding entry point; providers override this to send multi-item batches in one call.
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
}

impl<T: Embedder + ?Sized> Embedder for Box<T> {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        (**self).embed(text)
    }

    fn model_id(&self) -> &str {
        (**self).model_id()
    }

    fn expected_dims(&self) -> Option<usize> {
        (**self).expected_dims()
    }

    fn is_enabled(&self) -> bool {
        (**self).is_enabled()
    }

    fn allows_raw_only(&self) -> bool {
        (**self).allows_raw_only()
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        (**self).embed_batch(texts)
    }
}

impl<T: Embedder + ?Sized> Embedder for Arc<T> {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        (**self).embed(text)
    }

    fn model_id(&self) -> &str {
        (**self).model_id()
    }

    fn expected_dims(&self) -> Option<usize> {
        (**self).expected_dims()
    }

    fn is_enabled(&self) -> bool {
        (**self).is_enabled()
    }

    fn allows_raw_only(&self) -> bool {
        (**self).allows_raw_only()
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        (**self).embed_batch(texts)
    }
}

/// Deterministic zero-network 256-dimensional FNV-1a n-gram hash embedder (`hash:v1`) used by unit tests.
#[derive(Clone, Copy, Debug, Default)]
pub struct HashEmbedder;

impl Embedder for HashEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let text = text.to_lowercase();
        let mut vector = vec![0.0_f32; 256];
        let mut add = |feature: &str| {
            // Fixed FNV-1a, independent of process seeds and platform endianness.
            let hash = feature.bytes().fold(0xcbf29ce484222325_u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
            });
            vector[(hash & 255) as usize] += if hash & 256 == 0 { 1.0 } else { -1.0 };
        };
        for token in text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| !s.is_empty())
        {
            add(token);
            let characters: Vec<_> = token.chars().collect();
            for width in 2..=3 {
                for gram in characters.windows(width) {
                    add(&gram.iter().collect::<String>());
                }
            }
        }
        let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > 0.0 {
            for value in &mut vector {
                *value /= norm;
            }
        }
        Ok(vector)
    }

    fn model_id(&self) -> &str {
        "hash:v1"
    }

    fn expected_dims(&self) -> Option<usize> {
        Some(256)
    }

    fn is_enabled(&self) -> bool {
        true
    }

    fn allows_raw_only(&self) -> bool {
        true
    }
}

/// Pure FTS5 keyword-only mode (`--no-embedding`, `model_id = "none"`): zero vector rows, zero downloads.
#[derive(Clone, Copy, Debug, Default)]
pub struct FtsOnlyEmbedder;

impl Embedder for FtsOnlyEmbedder {
    fn embed(&self, _text: &str) -> Result<Vec<f32>> {
        Ok(Vec::new())
    }

    fn model_id(&self) -> &str {
        "none"
    }

    fn expected_dims(&self) -> Option<usize> {
        Some(0)
    }

    fn is_enabled(&self) -> bool {
        false
    }

    fn allows_raw_only(&self) -> bool {
        true
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(vec![Vec::new(); texts.len()])
    }
}

/// Placeholder attached by `Store::open` / `Store::init_with_embedding` when a library's configured
/// model (`local:*` or `<provider>:<model>`) has not yet been loaded into the current process.
/// Because `is_enabled() == false`, thin Hook/CLI callers write L0/L1 cleanly without writing wrong-dim
/// hash vectors and seamlessly fall back to FTS5 until `kp daemon` or `kp embed --stale` attaches the model.
#[derive(Clone, Debug)]
pub struct DisabledModelEmbedder {
    model_id: String,
    dims: usize,
}

impl DisabledModelEmbedder {
    pub fn new(model_id: impl Into<String>, dims: usize) -> Self {
        Self {
            model_id: model_id.into(),
            dims,
        }
    }
}

impl Embedder for DisabledModelEmbedder {
    fn embed(&self, _text: &str) -> Result<Vec<f32>> {
        Ok(Vec::new())
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn expected_dims(&self) -> Option<usize> {
        (self.dims > 0).then_some(self.dims)
    }

    fn is_enabled(&self) -> bool {
        false
    }

    fn allows_raw_only(&self) -> bool {
        let id = self.model_id.as_str();
        matches!(id, "hash:v1" | "none") || id.starts_with("local:")
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(vec![Vec::new(); texts.len()])
    }
}

pub(crate) fn default_embedder_for_meta(
    embedding_model: &str,
    embedding_dims: usize,
) -> Arc<dyn Embedder + Send + Sync> {
    match embedding_model {
        "hash:v1" => Arc::new(HashEmbedder),
        "none" => Arc::new(FtsOnlyEmbedder),
        other => Arc::new(DisabledModelEmbedder::new(other.to_owned(), embedding_dims)),
    }
}

/// Validates a finite, nonempty, unit-length embedding or an all-zero sentinel.
///
/// # Errors
/// Returns an embedding validation error for empty, nonfinite, or unnormalized vectors.
pub fn validate_vector(vector: &[f32]) -> Result<()> {
    let norm = vector.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>();
    if vector.is_empty() || !norm.is_finite() || (norm != 0.0 && (norm - 1.0).abs() > 1e-4) {
        return Err(KpError::invalid("embedding", "expected a finite normalized vector").into());
    }
    Ok(())
}

/// Screens real ONNX / HTTP provider vectors for `NaN`, `±Inf`, empty slices, or degenerate near-zero
/// L2 norm (`< 1e-6`), and L2-normalizes the vector to unit length (`GBrain` `embedding-guard.ts`).
pub fn sanitize_and_validate_embedding(raw: &[f32]) -> Result<Vec<f32>> {
    if raw.is_empty() {
        return Err(KpError::invalid("embedding", "embedding vector must not be empty").into());
    }
    if raw.iter().any(|v| !v.is_finite()) {
        return Err(
            KpError::invalid("embedding", "embedding vector contains NaN or Inf values").into(),
        );
    }
    let norm_sq: f64 = raw.iter().map(|v| f64::from(*v) * f64::from(*v)).sum();
    let norm = norm_sq.sqrt();
    if !norm.is_finite() || norm < MIN_NON_DEGENERATE_NORM {
        return Err(KpError::invalid(
            "embedding",
            "degenerate zero-norm embedding vector (< 1e-6)",
        )
        .into());
    }
    let scale = norm as f32;
    let normalized: Vec<f32> = if (norm - 1.0).abs() <= 1e-5 {
        raw.to_vec()
    } else {
        raw.iter().map(|v| *v / scale).collect()
    };
    validate_vector(&normalized)?;
    Ok(normalized)
}

/// Enforces `library_meta.embedding_dims` before persisting any vector row (`GBrain` `dims.ts`).
///
/// - When `library_meta.embedding_dims == 0` and `embedding_model != "none"`, auto-locks the library's
///   `embedding_dims` to `actual_dims` on the first real vector persisted.
/// - When `actual_dims` disagrees with the locked `embedding_dims` (or `embedding_model == "none"`),
///   fails immediately with the exact `kp embed --stale --to <provider>:<model> --dim <N>` command.
pub(crate) fn guard_and_lock_tx_embedding_dims(
    db: &Connection,
    embedder_model: &str,
    actual_dims: usize,
) -> Result<()> {
    if actual_dims == 0 {
        return Err(KpError::invalid("embedding_dims", "vector dimension must be > 0").into());
    }
    if embedder_model == "custom" {
        return Ok(());
    }
    let (meta_model, meta_dims): (String, i64) = db.query_row(
        "SELECT embedding_model, embedding_dims FROM library_meta LIMIT 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let locked_dims = usize::try_from(meta_dims)
        .map_err(|_| KpError::invalid("embedding_dims", "negative stored embedding_dims"))?;

    if meta_model == "none" {
        return Err(StoreError::Embedding(format!(
            "library has embeddings disabled (`none`), but `{embedder_model}` returned {actual_dims}d; \
             run `kp embed --stale --to {embedder_model} --dim {actual_dims}`"
        )));
    }

    if meta_model != embedder_model {
        return Err(StoreError::Embedding(format!(
            "embedding model mismatch: library locks `{meta_model}` ({locked_dims}d), \
             but active embedder is `{embedder_model}` ({actual_dims}d); \
             run `kp embed --stale --to {embedder_model} --dim {actual_dims}`"
        )));
    }

    if locked_dims == 0 {
        let dims_i64 = i64::try_from(actual_dims)
            .map_err(|_| KpError::invalid("embedding_dims", "embedding dimension overflow"))?;
        db.execute("UPDATE library_meta SET embedding_dims = ?1", [dims_i64])?;
        db.execute(
            "INSERT INTO change_log (op, target_id, payload_json, created_at)
             VALUES ('embedding_migration', 'library_meta', ?1, 0)",
            [serde_json::to_string(&serde_json::json!({
                "model": embedder_model,
                "dims": actual_dims,
                "auto_locked": true,
            }))?],
        )?;
        return Ok(());
    }

    if locked_dims != actual_dims {
        return Err(StoreError::Embedding(format!(
            "embedding dimension mismatch: library locks {locked_dims}d (`{meta_model}`), \
             but `{embedder_model}` returned {actual_dims}d; \
             run `kp embed --stale --to {embedder_model} --dim {actual_dims}`"
        )));
    }

    Ok(())
}

pub(crate) fn encode_vector_le_bytes(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|v| v.to_le_bytes()).collect()
}

#[cfg(test)]
#[path = "embedding_tests.rs"]
mod tests;
