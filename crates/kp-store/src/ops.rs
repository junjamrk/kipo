use kp_model::{
    ActorId, AnchorSpec, AudienceSpec, ContentHash, EpochMs, KpError, LibraryId, LogicalKey,
    ModelRoutePolicy, SourceId,
};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::Serialize;

use crate::embedding::{
    encode_vector_le_bytes, guard_and_lock_tx_embedding_dims, sanitize_and_validate_embedding,
    validate_vector, FtsOnlyEmbedder,
};
use crate::tombstone::guard;
use crate::{Embedder, Result, Store, StoreError};

pub struct RawSourceInput {
    pub self_echo_filtered: bool,
    pub uri: String,
    pub media_type: String,
    pub content: Vec<u8>,
    pub readable_by: AudienceSpec,
    pub captured_at: EpochMs,
    pub chunks: Vec<ChunkInput>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChunkInput {
    pub heading: String,
    pub body: String,
    pub anchor: AnchorSpec,
}

#[derive(Clone, Debug, Serialize)]
pub struct StoredSource {
    pub source_id: SourceId,
    pub content_sha256: ContentHash,
    pub git_blob: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ChunkHit {
    pub chunk_id: String,
    pub source_id: SourceId,
    pub heading: String,
    pub body: String,
    pub anchor: AnchorSpec,
}

pub(crate) fn embed_chunk_texts(
    chunks: &[ChunkInput],
    embedder: &(impl Embedder + ?Sized),
) -> Result<Option<Vec<Option<Vec<f32>>>>> {
    if !embedder.is_enabled() || chunks.is_empty() {
        return Ok(None);
    }
    let model_id = embedder.model_id();
    let is_real_model = model_id != "hash:v1" && model_id != "custom";
    let mut texts = Vec::with_capacity(chunks.len());
    let mut non_empty_indices = Vec::with_capacity(chunks.len());

    if is_real_model {
        for (idx, chunk) in chunks.iter().enumerate() {
            let combined = format!("{}\n{}", chunk.heading, chunk.body);
            if !combined.trim().is_empty() {
                texts.push(combined);
                non_empty_indices.push(idx);
            }
        }
        if texts.is_empty() {
            return Ok(Some(vec![None; chunks.len()]));
        }
        let batch = embedder.embed_batch(&texts)?;
        if batch.len() != texts.len() {
            return Err(KpError::invalid(
                "embedding",
                "embed_batch returned mismatched vector count",
            )
            .into());
        }
        let mut out = vec![None; chunks.len()];
        for (idx, raw_vec) in non_empty_indices.into_iter().zip(batch) {
            let clean = sanitize_and_validate_embedding(&raw_vec)?;
            out[idx] = Some(clean);
        }
        Ok(Some(out))
    } else {
        let mut out = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            let vec = embedder.embed(&format!("{}\n{}", chunk.heading, chunk.body))?;
            validate_vector(&vec)?;
            out.push(Some(vec));
        }
        Ok(Some(out))
    }
}

impl Store {
    /// Reads the raw source payload from `raw_blobs` for `source_id` (falling back to legacy
    /// `raw/{source_id}.md` / `raw/.git` on unmigrated `v1` read-only handles), returning `Ok(None)`
    /// if missing or tombstoned.
    pub fn raw_source_bytes(&self, source_id: &SourceId) -> Result<Option<Vec<u8>>> {
        let in_blobs: Option<Vec<u8>> = self
            .db
            .query_row(
                "SELECT b.content_blob
                 FROM raw_sources s
                 JOIN raw_blobs b ON b.content_sha256 = s.content_sha256
                 WHERE s.source_id = ?1
                   AND NOT EXISTS (SELECT 1 FROM tombstones t WHERE t.tombstone_id = s.source_id)",
                [source_id.as_str()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        if in_blobs.is_some() {
            return Ok(in_blobs);
        }
        let legacy_meta: Option<(String, String)> = self
            .db
            .query_row(
                "SELECT s.content_sha256, s.git_blob_or_commit
                 FROM raw_sources s
                 WHERE s.source_id = ?1
                   AND NOT EXISTS (SELECT 1 FROM tombstones t WHERE t.tombstone_id = s.source_id)",
                [source_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((expected_sha, git_ref)) = legacy_meta {
            if let Some(bytes) = crate::migrations::read_legacy_raw_source_bytes(
                &self.path,
                source_id.as_str(),
                &git_ref,
            ) {
                if ContentHash::digest(&bytes).as_str() == expected_sha {
                    return Ok(Some(bytes));
                }
            }
        }
        Ok(None)
    }

    /// Reads the UTF-8 raw source text from `raw_blobs` for `source_id`, returning `Ok(None)` if
    /// missing or tombstoned.
    pub fn raw_source_text(&self, source_id: &SourceId) -> Result<Option<String>> {
        let Some(bytes) = self.raw_source_bytes(source_id)? else {
            return Ok(None);
        };
        let text = String::from_utf8(bytes)
            .map_err(|_| KpError::invalid("raw_source", "raw source bytes are not valid UTF-8"))?;
        Ok(Some(text))
    }

    /// The stable source ID is the opaque identity digest, so sources need no parallel key registry.
    /// Raw content is immutable; a different version must use a different logical key.
    pub fn ingest_source(
        &mut self,
        origin: &LibraryId,
        key: &LogicalKey,
        input: &RawSourceInput,
    ) -> Result<StoredSource> {
        let embedder = self.embedder.clone();
        self.ingest_source_with_embedder(origin, key, input, embedder.as_ref())
    }

    pub fn ingest_source_with_embedder(
        &mut self,
        origin: &LibraryId,
        key: &LogicalKey,
        input: &RawSourceInput,
        embedder: &(impl crate::Embedder + ?Sized),
    ) -> Result<StoredSource> {
        self.ingest_raw_source_inner(origin, key, input, &ModelRoutePolicy::HostCli, embedder)
    }

    /// Ingests a raw source honoring `policy`: when `policy == ModelRoutePolicy::RawOnly`, marks
    /// the source `raw_only_source` and never sends text to a cloud/external [`Embedder`].
    pub fn ingest_source_with_policy(
        &mut self,
        origin: &LibraryId,
        key: &LogicalKey,
        input: &RawSourceInput,
        policy: &ModelRoutePolicy,
    ) -> Result<StoredSource> {
        let embedder = self.embedder.clone();
        self.ingest_raw_source_inner(origin, key, input, policy, embedder.as_ref())
    }

    fn ingest_raw_source_inner(
        &mut self,
        origin: &LibraryId,
        key: &LogicalKey,
        input: &RawSourceInput,
        policy: &ModelRoutePolicy,
        embedder: &(impl crate::Embedder + ?Sized),
    ) -> Result<StoredSource> {
        let is_raw_only = matches!(policy, ModelRoutePolicy::RawOnly);
        let skip_for_raw_only = is_raw_only && !embedder.allows_raw_only();
        if matches!(input.readable_by, AudienceSpec::SemiringDerived) {
            return Err(KpError::invalid(
                "readable_by",
                "raw sources require an explicit audience or all members",
            )
            .into());
        }
        let redactor = crate::public_ingest::shared_redactor()?;
        let (redacted_uri, redacted_content, redacted_chunks) =
            crate::public_ingest::redact_source_bytes_and_chunks(
                &input.uri,
                input.content.clone(),
                input.chunks.clone(),
                redactor,
            );
        let byte_len = i64::try_from(redacted_content.len())
            .map_err(|_| KpError::invalid("raw source", "content exceeds SQLite length range"))?;
        let _ = i64::try_from(redacted_chunks.len())
            .map_err(|_| KpError::invalid("chunks", "too many chunks"))?;

        let opaque = self.tombstone_id(origin, key)?;
        let source_id = SourceId::new(opaque.as_str())?;

        guard(&self.db, &opaque)?;
        let already_exists: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM raw_sources WHERE source_id=?1)",
            [source_id.as_str()],
            |row| row.get(0),
        )?;
        if already_exists {
            return Err(StoreError::Conflict);
        }

        let vectors = if skip_for_raw_only {
            embed_chunk_texts(&redacted_chunks, &FtsOnlyEmbedder)?
        } else {
            embed_chunk_texts(&redacted_chunks, embedder)?
        };

        let content_sha256 = ContentHash::digest(&redacted_content);
        let blob_id = content_sha256.to_string();

        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        guard(&tx, &opaque)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM raw_sources WHERE source_id=?1)",
            [source_id.as_str()],
            |row| row.get(0),
        )?;
        if exists {
            return Err(StoreError::Conflict);
        }

        tx.execute(
            "INSERT OR IGNORE INTO raw_blobs (content_sha256, byte_len, content_blob, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                content_sha256.as_str(),
                byte_len,
                &redacted_content,
                input.captured_at.0
            ],
        )?;
        let audience = serde_json::to_string(&input.readable_by)?;
        tx.execute(
            "INSERT INTO raw_sources
                (source_id,uri,media_type,content_sha256,git_blob_or_commit,byte_len,visibility_scope,captured_at,self_echo_filtered)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                source_id.as_str(),
                &redacted_uri,
                input.media_type,
                content_sha256.as_str(),
                &blob_id,
                byte_len,
                audience,
                input.captured_at.0,
                input.self_echo_filtered,
            ],
        )?;

        let model_id = if skip_for_raw_only {
            FtsOnlyEmbedder.model_id()
        } else {
            embedder.model_id()
        };
        for (ordinal, chunk) in redacted_chunks.iter().enumerate() {
            let ordinal_i64 = i64::try_from(ordinal)
                .map_err(|_| KpError::invalid("chunks", "too many chunks"))?;
            let chunk_id = format!("{source_id}-{ordinal_i64}");
            let mut anchor = chunk.anchor.clone();
            anchor.source_id = Some(source_id.clone());
            let segmented = self.tokenizer.segment_text(&chunk.body);
            let fts_rowid: i64 = tx.query_row(
                "INSERT INTO chunks
                    (chunk_id,source_id,source_sha256,ordinal,heading,body,segmented_body,anchor_json,readable_by_json,created_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) RETURNING fts_rowid",
                params![
                    chunk_id,
                    source_id.as_str(),
                    content_sha256.as_str(),
                    ordinal_i64,
                    chunk.heading,
                    chunk.body,
                    segmented,
                    serde_json::to_string(&anchor)?,
                    audience,
                    input.captured_at.0,
                ],
                |row| row.get(0),
            )?;
            if let Some(Some(vector)) = vectors.as_ref().and_then(|vs| vs.get(ordinal)) {
                guard_and_lock_tx_embedding_dims(&tx, model_id, vector.len())?;
                let blob = encode_vector_le_bytes(vector);
                tx.execute(
                    "INSERT INTO chunk_vectors (chunk_id, dims, vec_blob, model_id) VALUES (?1, ?2, ?3, ?4)",
                    params![chunk_id, vector.len(), blob, model_id],
                )?;
            }
            tx.execute(
                "INSERT INTO chunks_fts (rowid,title,heading,segmented_body) VALUES (?1,?2,?3,?4)",
                params![
                    fts_rowid,
                    self.tokenizer.segment_text(&redacted_uri),
                    self.tokenizer.segment_text(&chunk.heading),
                    segmented
                ],
            )?;
        }
        tx.execute(
            "INSERT INTO change_log (op,target_id,origin_library_id,logical_key,content_sha256,created_at)
             VALUES ('ingest_source',?1,?2,?3,?4,?5)",
            params![
                source_id.as_str(),
                origin.as_str(),
                key.as_str(),
                content_sha256.as_str(),
                input.captured_at.0
            ],
        )?;
        if is_raw_only {
            tx.execute(
                "INSERT INTO change_log (op, target_id, created_at) VALUES ('raw_only_source', ?1, ?2)",
                params![source_id.as_str(), input.captured_at.0],
            )?;
        }
        tx.commit()?;
        Ok(StoredSource {
            source_id,
            content_sha256,
            git_blob: blob_id,
        })
    }
}

/// Parameters for [`Store::ingest_session_subbatch`].
#[derive(Clone)]
pub struct SessionSubbatchSpec<'a> {
    pub origin: &'a LibraryId,
    pub key: &'a LogicalKey,
    pub source_input: &'a RawSourceInput,
    pub job_input: &'a crate::ExtractionJobInput,
    pub session_id: &'a str,
    pub start_byte: u64,
    pub end_byte: u64,
    pub at: EpochMs,
}

impl Store {
    /// Atomically persists (or recovers if `(origin, key)` already exists with identical `content_sha256`)
    /// a session sub-batch `raw_blobs` + `raw_sources` + `chunks`, idempotently enqueues `job_input`
    /// (re-arming `'failed_permanent'` back to `'pending'`), and records `session_source_span` in ONE
    /// `TransactionBehavior::Immediate` transaction. Returns `(stored_source, newly_ingested)`.
    pub fn ingest_session_subbatch(
        &mut self,
        spec: &SessionSubbatchSpec<'_>,
    ) -> Result<(StoredSource, bool)> {
        let SessionSubbatchSpec {
            origin,
            key,
            source_input,
            job_input,
            session_id,
            start_byte,
            end_byte,
            at,
        } = *spec;
        if matches!(source_input.readable_by, AudienceSpec::SemiringDerived) {
            return Err(KpError::invalid(
                "readable_by",
                "raw sources require an explicit audience or all members",
            )
            .into());
        }
        crate::queue::validate_session_span_args(session_id, start_byte, end_byte)?;
        let byte_len = i64::try_from(source_input.content.len())
            .map_err(|_| KpError::invalid("raw source", "content exceeds SQLite length range"))?;
        let _ = i64::try_from(source_input.chunks.len())
            .map_err(|_| KpError::invalid("chunks", "too many chunks"))?;

        let opaque = self.tombstone_id(origin, key)?;
        let source_id = SourceId::new(opaque.as_str())?;
        let content_sha256 = ContentHash::digest(&source_input.content);
        let blob_id = content_sha256.to_string();
        guard(&self.db, &opaque)?;

        let pre_existing: Option<(String, String)> = self
            .db
            .query_row(
                "SELECT content_sha256, git_blob_or_commit FROM raw_sources WHERE source_id=?1",
                [source_id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((ref existing_sha, _)) = pre_existing {
            if existing_sha != content_sha256.as_str() {
                return Err(StoreError::Conflict);
            }
        }

        let is_raw_only = job_input.model_policy.trim() == "raw_only";
        let store_embedder = self.embedder.clone();
        let active_embedder: &dyn Embedder = if is_raw_only && !store_embedder.allows_raw_only() {
            &FtsOnlyEmbedder
        } else {
            store_embedder.as_ref()
        };

        let prepared_vectors = if pre_existing.is_none() {
            embed_chunk_texts(&source_input.chunks, active_embedder)?
        } else {
            None
        };

        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        guard(&tx, &opaque)?;
        let existing_in_tx: Option<(String, String)> = tx
            .query_row(
                "SELECT content_sha256, git_blob_or_commit FROM raw_sources WHERE source_id=?1",
                [source_id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;

        let (stored, newly_ingested) = match existing_in_tx {
            Some((existing_sha, existing_git_blob)) => {
                if existing_sha != content_sha256.as_str() {
                    return Err(StoreError::Conflict);
                }
                (
                    StoredSource {
                        source_id: source_id.clone(),
                        content_sha256: content_sha256.clone(),
                        git_blob: existing_git_blob,
                    },
                    false,
                )
            }
            None => {
                let vectors = match prepared_vectors {
                    Some(vs) => Some(vs),
                    None => embed_chunk_texts(&source_input.chunks, active_embedder)?,
                };
                tx.execute(
                    "INSERT OR IGNORE INTO raw_blobs (content_sha256, byte_len, content_blob, created_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        content_sha256.as_str(),
                        byte_len,
                        &source_input.content,
                        source_input.captured_at.0,
                    ],
                )?;
                let audience = serde_json::to_string(&source_input.readable_by)?;
                tx.execute(
                    "INSERT INTO raw_sources
                        (source_id,uri,media_type,content_sha256,git_blob_or_commit,byte_len,visibility_scope,captured_at,self_echo_filtered)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![
                        source_id.as_str(),
                        &source_input.uri,
                        &source_input.media_type,
                        content_sha256.as_str(),
                        &blob_id,
                        byte_len,
                        &audience,
                        source_input.captured_at.0,
                        source_input.self_echo_filtered,
                    ],
                )?;
                let model_id = active_embedder.model_id();
                for (ordinal, chunk) in source_input.chunks.iter().enumerate() {
                    let ordinal_i64 = i64::try_from(ordinal)
                        .map_err(|_| KpError::invalid("chunks", "too many chunks"))?;
                    let chunk_id = format!("{source_id}-{ordinal_i64}");
                    let mut anchor = chunk.anchor.clone();
                    anchor.source_id = Some(source_id.clone());
                    let segmented = self.tokenizer.segment_text(&chunk.body);
                    let fts_rowid: i64 = tx.query_row(
                        "INSERT INTO chunks
                            (chunk_id,source_id,source_sha256,ordinal,heading,body,segmented_body,anchor_json,readable_by_json,created_at)
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) RETURNING fts_rowid",
                        params![
                            &chunk_id,
                            source_id.as_str(),
                            content_sha256.as_str(),
                            ordinal_i64,
                            &chunk.heading,
                            &chunk.body,
                            &segmented,
                            serde_json::to_string(&anchor)?,
                            &audience,
                            source_input.captured_at.0,
                        ],
                        |row| row.get(0),
                    )?;
                    if let Some(Some(vector)) = vectors.as_ref().and_then(|vs| vs.get(ordinal)) {
                        guard_and_lock_tx_embedding_dims(&tx, model_id, vector.len())?;
                        let blob = encode_vector_le_bytes(vector);
                        tx.execute(
                            "INSERT INTO chunk_vectors (chunk_id, dims, vec_blob, model_id) VALUES (?1, ?2, ?3, ?4)",
                            params![&chunk_id, vector.len(), blob, model_id],
                        )?;
                    }
                    tx.execute(
                        "INSERT INTO chunks_fts (rowid,title,heading,segmented_body) VALUES (?1,?2,?3,?4)",
                        params![
                            fts_rowid,
                            self.tokenizer.segment_text(&source_input.uri),
                            self.tokenizer.segment_text(&chunk.heading),
                            segmented,
                        ],
                    )?;
                }
                tx.execute(
                    "INSERT INTO change_log (op,target_id,origin_library_id,logical_key,content_sha256,created_at)
                     VALUES ('ingest_source',?1,?2,?3,?4,?5)",
                    params![
                        source_id.as_str(),
                        origin.as_str(),
                        key.as_str(),
                        content_sha256.as_str(),
                        source_input.captured_at.0,
                    ],
                )?;
                if is_raw_only {
                    tx.execute(
                        "INSERT INTO change_log (op, target_id, created_at) VALUES ('raw_only_source', ?1, ?2)",
                        params![source_id.as_str(), source_input.captured_at.0],
                    )?;
                }
                (
                    StoredSource {
                        source_id: source_id.clone(),
                        content_sha256,
                        git_blob: blob_id,
                    },
                    true,
                )
            }
        };

        let _ = crate::queue::enqueue_or_rearm_extraction_job_tx(
            &tx, &source_id, job_input, None, None,
        )?;
        crate::queue::record_session_source_span_tx(
            &tx, &source_id, session_id, start_byte, end_byte, at,
        )?;
        let _ = crate::queue::complete_session_source_span_tx(&tx, &source_id)?;
        tx.commit()?;
        Ok((stored, newly_ingested))
    }

    /// Literal token search with visibility filtering and stable ordering, without corpus-wide rank statistics.
    pub fn search_chunks(&self, query: &str, viewer: &ActorId) -> Result<Vec<ChunkHit>> {
        let terms = self.tokenizer.tokenize(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let query = terms
            .iter()
            .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR ");
        let mut stmt = self.db.prepare(
            "SELECT c.chunk_id,c.source_id,c.heading,c.body,c.anchor_json FROM chunks_fts
             JOIN chunks c ON c.fts_rowid=chunks_fts.rowid
             JOIN raw_sources s ON s.source_id=c.source_id AND s.content_sha256=c.source_sha256
             WHERE chunks_fts MATCH ?1
             AND s.superseded_by_source IS NULL
             AND NOT EXISTS (SELECT 1 FROM tombstones t WHERE t.tombstone_id=c.source_id)
             AND EXISTS (SELECT 1 FROM json_each(c.readable_by_json) a WHERE a.value=?2 OR
                 (a.value='*' AND EXISTS (SELECT 1 FROM members_tokens m WHERE m.actor_id=?2 AND m.revoked_at IS NULL)))
             AND (s.source_scope_id IS NULL OR EXISTS (
                 SELECT 1 FROM source_scopes sc, json_each(sc.current_audience_json) sa
                 WHERE sc.scope_id=s.source_scope_id AND (sa.value=?2 OR
                     (sa.value='*' AND EXISTS (SELECT 1 FROM members_tokens m WHERE m.actor_id=?2 AND m.revoked_at IS NULL)))
             ))
             ORDER BY c.chunk_id",
        )?;
        let mut rows = stmt.query(params![query, viewer.as_str()])?;
        let mut hits = Vec::new();
        while let Some(row) = rows.next()? {
            hits.push(ChunkHit {
                chunk_id: row.get(0)?,
                source_id: row.get::<_, String>(1)?.parse()?,
                heading: row.get(2)?,
                body: row.get(3)?,
                anchor: serde_json::from_str(&row.get::<_, String>(4)?)?,
            });
        }
        Ok(hits)
    }
}

#[cfg(test)]
#[path = "ops_tests.rs"]
mod tests;
