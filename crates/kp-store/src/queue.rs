use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

use kp_model::{
    estimate_batch_tokens, has_tagged_turns, is_bare_content_hash, ActorId, AnchorSpec,
    AudienceSpec, CommunityConcept, ConceptId, ContentHash, EpochMs, ExtractionChunkSlice, Fact,
    FactId, Justification, KpError, SliceRole, SourceId, SourceScopeId, DEFAULT_MAX_BATCH_TOKENS,
};
use rusqlite::{params, OptionalExtension, Row, TransactionBehavior};
use serde::{Deserialize, Serialize};

use crate::governance::new_job_id;
use crate::{Embedder, FactInput, Result, Store, StoreError};

/// A single base-fact mutation staged by `kp-derive` before atomic persistence.
#[derive(Clone, Debug)]
pub enum StagedAtomWrite {
    Remember(FactInput),
    Supersede {
        target_fact_id: FactId,
        replacement: Fact,
    },
}

/// Pure in-memory write set produced by extraction atom validation so all fact, supersession,
/// assumed-premise derivation, and concept writes commit atomically under the job's lease fence.
#[derive(Clone, Debug, Default)]
pub struct StagedExtractionBatch {
    pub writes: Vec<StagedAtomWrite>,
    pub derivations: Vec<(Fact, Justification)>,
    pub concepts: Vec<CommunityConcept>,
    pub created_concepts: Vec<CommunityConcept>,
    pub fact_concept_links: Vec<(FactId, Vec<ConceptId>, EpochMs)>,
}

/// Incremental byte/line offset cursor for an active host CLI transcript (`session_cursors`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCursor {
    pub session_id: String,
    pub client_kind: String,
    pub transcript_path: Option<String>,
    pub last_byte_offset: u64,
    pub last_line_count: u64,
    pub prefix_sha256: String,
    pub last_activity_at: EpochMs,
    pub last_extracted_offset: u64,
}

/// Lifecycle state of an asynchronous batch extraction job in `extraction_queue`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionJobStatus {
    Pending,
    Leased,
    PausedQuota,
    Done,
    FailedPermanent,
}

impl ExtractionJobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Leased => "leased",
            Self::PausedQuota => "paused_quota",
            Self::Done => "done",
            Self::FailedPermanent => "failed_permanent",
        }
    }
}

impl fmt::Display for ExtractionJobStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ExtractionJobStatus {
    type Err = KpError;

    fn from_str(value: &str) -> kp_model::Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "leased" => Ok(Self::Leased),
            "paused_quota" => Ok(Self::PausedQuota),
            "done" => Ok(Self::Done),
            "failed_permanent" => Ok(Self::FailedPermanent),
            _ => Err(KpError::invalid(
                "extraction_job_status",
                "unknown extraction job status",
            )),
        }
    }
}

/// Parameters for [`Store::commit_staged_extraction_job_fenced`].
#[derive(Clone, Debug)]
pub struct FencedJobCommitSpec<'a> {
    pub jobs: &'a [(String, u32)],
    pub lease_now: EpochMs,
    pub batch: &'a StagedExtractionBatch,
    pub chunk_hashes: &'a [(ContentHash, ActorId)],
    pub schema_ver: &'a str,
    pub at: EpochMs,
}

/// Multi-job extraction window leased atomically via [`Store::lease_extraction_window`].
#[derive(Clone, Debug)]
pub struct LeasedExtractionWindow {
    pub jobs: Vec<ExtractionJob>,
    pub readable_by: AudienceSpec,
    pub source_scope_id: Option<SourceScopeId>,
    pub ingested_by: Option<ActorId>,
    pub primary_uri: String,
    pub has_http_uri: bool,
    pub slices: Vec<ExtractionChunkSlice>,
}

/// Parameters to enqueue an out-of-session batch extraction job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractionJobInput {
    pub source_id: SourceId,
    pub chunk_ids: Vec<String>,
    pub chunk_batch_sha256: ContentHash,
    pub author: ActorId,
    pub model_policy: String,
    pub priority: i32,
    pub reference_time: EpochMs,
    pub created_at: EpochMs,
}

/// Persisted batch extraction job row in `extraction_queue`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractionJob {
    pub job_id: String,
    pub source_id: SourceId,
    pub chunk_ids: Vec<String>,
    pub chunk_batch_sha256: ContentHash,
    pub author: ActorId,
    pub model_policy: String,
    pub priority: i32,
    pub reference_time: EpochMs,
    pub status: ExtractionJobStatus,
    pub lease_until: Option<EpochMs>,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub created_at: EpochMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_key: Option<String>,
}

impl Store {
    /// Upserts an incremental transcript cursor in `session_cursors` (`< 15ms` Stop hook path).
    pub fn upsert_session_cursor(&mut self, cursor: &SessionCursor) -> Result<()> {
        if cursor.session_id.trim().is_empty() {
            return Err(KpError::invalid("session_id", "session_id must be nonempty").into());
        }
        if cursor.client_kind.trim().is_empty() {
            return Err(KpError::invalid("client_kind", "client_kind must be nonempty").into());
        }
        let last_byte = i64::try_from(cursor.last_byte_offset)
            .map_err(|_| KpError::invalid("last_byte_offset", "offset exceeds i64"))?;
        let last_line = i64::try_from(cursor.last_line_count)
            .map_err(|_| KpError::invalid("last_line_count", "line count exceeds i64"))?;
        let last_extracted = i64::try_from(cursor.last_extracted_offset)
            .map_err(|_| KpError::invalid("last_extracted_offset", "offset exceeds i64"))?;

        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO session_cursors
                (session_id, client_kind, transcript_path, last_byte_offset, last_line_count,
                 prefix_sha256, last_activity_at, last_extracted_offset)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(session_id) DO UPDATE SET
                client_kind = excluded.client_kind,
                transcript_path = excluded.transcript_path,
                last_byte_offset = excluded.last_byte_offset,
                last_line_count = excluded.last_line_count,
                prefix_sha256 = excluded.prefix_sha256,
                last_activity_at = excluded.last_activity_at,
                last_extracted_offset = excluded.last_extracted_offset",
            params![
                cursor.session_id.trim(),
                cursor.client_kind.trim(),
                cursor.transcript_path.as_deref(),
                last_byte,
                last_line,
                cursor.prefix_sha256,
                cursor.last_activity_at.0,
                last_extracted,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Reads the stored `SessionCursor` for `session_id`.
    pub fn get_session_cursor(&self, session_id: &str) -> Result<Option<SessionCursor>> {
        self.db
            .query_row(
                "SELECT session_id, client_kind, transcript_path, last_byte_offset, last_line_count,
                        prefix_sha256, last_activity_at, last_extracted_offset
                 FROM session_cursors
                 WHERE session_id = ?1",
                [session_id],
                read_cursor_row,
            )
            .optional()?
            .transpose()
    }

    /// Advances `last_extracted_offset` after a session's newly appended slice has been queued/extracted.
    pub fn mark_session_extracted(
        &mut self,
        session_id: &str,
        extracted_offset: u64,
    ) -> Result<bool> {
        let offset = i64::try_from(extracted_offset)
            .map_err(|_| KpError::invalid("last_extracted_offset", "offset exceeds i64"))?;
        let changed = self.db.execute(
            "UPDATE session_cursors SET last_extracted_offset = ?1 WHERE session_id = ?2",
            params![offset, session_id],
        )?;
        Ok(changed > 0)
    }

    /// Lists sessions with unextracted bytes whose last activity occurred at or before `idle_before`.
    pub fn idle_unextracted_sessions(&self, idle_before: EpochMs) -> Result<Vec<SessionCursor>> {
        let mut stmt = self.db.prepare(
            "SELECT session_id, client_kind, transcript_path, last_byte_offset, last_line_count,
                    prefix_sha256, last_activity_at, last_extracted_offset
             FROM session_cursors
             WHERE last_byte_offset > last_extracted_offset
               AND last_activity_at <= ?1
             ORDER BY last_activity_at ASC, session_id ASC",
        )?;
        let rows = stmt.query_map([idle_before.0], read_cursor_row)?;
        rows.map(|r| r?).collect()
    }

    /// Idempotently enqueues a batch extraction job into `extraction_queue` (`job_<uuidv7>`).
    /// Rejects `raw_only` policies so compliance-locked connectors never reach an LLM runner.
    pub fn enqueue_extraction_job(&mut self, input: &ExtractionJobInput) -> Result<ExtractionJob> {
        self.enqueue_extraction_job_with_thread(input, None, None)
    }

    pub fn enqueue_extraction_job_with_thread(
        &mut self,
        input: &ExtractionJobInput,
        thread_key: Option<&str>,
        reply_to_key: Option<&str>,
    ) -> Result<ExtractionJob> {
        if input.chunk_ids.is_empty() || input.chunk_ids.iter().any(|c| c.trim().is_empty()) {
            return Err(KpError::invalid(
                "chunk_ids",
                "extraction job requires at least one nonempty chunk_id",
            )
            .into());
        }
        let policy = input.model_policy.trim();
        if policy.is_empty() || policy == "raw_only" {
            return Err(KpError::invalid(
                "model_policy",
                "model_policy must be a valid non-raw_only extraction route",
            )
            .into());
        }
        let clean_thread = thread_key.map(str::trim).filter(|s| !s.is_empty());
        let clean_reply = reply_to_key.map(str::trim).filter(|s| !s.is_empty());
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<ExtractionJob> = tx
            .query_row(
                "SELECT job_id, source_id, chunk_ids_json, chunk_batch_sha256, author_actor,
                        model_policy, priority, reference_time, status, lease_until, attempts,
                        last_error, created_at, thread_key, reply_to_key
                 FROM extraction_queue
                 WHERE source_id = ?1
                   AND chunk_batch_sha256 = ?2
                   AND author_actor = ?3
                   AND status IN ('pending', 'leased', 'paused_quota', 'done')
                 ORDER BY created_at ASC, job_id ASC
                 LIMIT 1",
                params![
                    input.source_id.as_str(),
                    input.chunk_batch_sha256.as_str(),
                    input.author.as_str(),
                ],
                read_job_row,
            )
            .optional()?
            .transpose()?;

        if let Some(job) = existing {
            tx.commit()?;
            return Ok(job);
        }

        let job_id = new_job_id()?;
        if is_bare_content_hash(&job_id) {
            return Err(KpError::invalid("job_id", "unexpected bare digest").into());
        }
        let chunk_ids_json = serde_json::to_string(&input.chunk_ids)?;
        tx.execute(
            "INSERT INTO extraction_queue
                (job_id, source_id, chunk_ids_json, chunk_batch_sha256, author_actor, model_policy,
                 priority, reference_time, status, lease_until, attempts, last_error, created_at,
                 thread_key, reply_to_key)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', NULL, 0, NULL, ?9, ?10, ?11)",
            params![
                job_id,
                input.source_id.as_str(),
                chunk_ids_json,
                input.chunk_batch_sha256.as_str(),
                input.author.as_str(),
                policy,
                input.priority,
                input.reference_time.0,
                input.created_at.0,
                clean_thread,
                clean_reply,
            ],
        )?;
        tx.commit()?;

        Ok(ExtractionJob {
            job_id,
            source_id: input.source_id.clone(),
            chunk_ids: input.chunk_ids.clone(),
            chunk_batch_sha256: input.chunk_batch_sha256.clone(),
            author: input.author.clone(),
            model_policy: policy.to_owned(),
            priority: input.priority,
            reference_time: input.reference_time,
            status: ExtractionJobStatus::Pending,
            lease_until: None,
            attempts: 0,
            last_error: None,
            created_at: input.created_at,
            thread_key: clean_thread.map(str::to_owned),
            reply_to_key: clean_reply.map(str::to_owned),
        })
    }

    /// Recovers expired `leased` jobs (`lease_until <= now`) back to `pending` and atomically leases
    /// the highest-priority pending job until `now + lease_duration_ms`.
    pub fn lease_next_extraction_job(
        &mut self,
        now: EpochMs,
        lease_duration_ms: i64,
    ) -> Result<Option<ExtractionJob>> {
        if lease_duration_ms <= 0 {
            return Err(
                KpError::invalid("lease_duration_ms", "lease duration must be positive").into(),
            );
        }
        let lease_until = EpochMs(
            now.0
                .checked_add(lease_duration_ms)
                .ok_or_else(|| KpError::invalid("lease_until", "timestamp overflow"))?,
        );
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        reconcile_queue_leases_tx(&tx, now)?;

        let next_id: Option<String> = tx
            .query_row(
                "SELECT job_id FROM extraction_queue
                 WHERE status = 'pending'
                 ORDER BY priority DESC, created_at ASC, reference_time ASC, job_id ASC
                 LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;

        let Some(job_id) = next_id else {
            tx.commit()?;
            return Ok(None);
        };

        tx.execute(
            "UPDATE extraction_queue
             SET status = 'leased', lease_until = ?1, attempts = attempts + 1
             WHERE job_id = ?2",
            params![lease_until.0, job_id],
        )?;
        let leased = tx.query_row(
            "SELECT job_id, source_id, chunk_ids_json, chunk_batch_sha256, author_actor,
                    model_policy, priority, reference_time, status, lease_until, attempts,
                    last_error, created_at, thread_key, reply_to_key
             FROM extraction_queue
             WHERE job_id = ?1",
            [job_id],
            read_job_row,
        )??;
        tx.commit()?;
        Ok(Some(leased))
    }

    /// Leases an extraction window: a single job for documents and `AgentSession` tagged transcripts,
    /// or a bounded multi-job window (`<= max_batch_tokens`) plus up to `5` preceding `SliceRole::Context`
    /// slices when `thread_key.is_some()` and all grouped jobs share identical
    /// `(source_scope_id, readable_by, model_policy, ingested_by, thread_key)`.
    pub fn lease_extraction_window(
        &mut self,
        now: EpochMs,
        lease_duration_ms: i64,
        max_batch_tokens: usize,
    ) -> Result<Option<LeasedExtractionWindow>> {
        if lease_duration_ms <= 0 {
            return Err(
                KpError::invalid("lease_duration_ms", "lease duration must be positive").into(),
            );
        }
        let lease_until = EpochMs(
            now.0
                .checked_add(lease_duration_ms)
                .ok_or_else(|| KpError::invalid("lease_until", "timestamp overflow"))?,
        );
        let budget = if max_batch_tokens == 0 {
            DEFAULT_MAX_BATCH_TOKENS
        } else {
            max_batch_tokens
        }
        .max(1);

        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        reconcile_queue_leases_tx(&tx, now)?;

        loop {
            let head: Option<ExtractionJob> = tx
                .query_row(
                    "SELECT job_id, source_id, chunk_ids_json, chunk_batch_sha256, author_actor,
                            model_policy, priority, reference_time, status, lease_until, attempts,
                            last_error, created_at, thread_key, reply_to_key
                     FROM extraction_queue
                     WHERE status = 'pending'
                     ORDER BY priority DESC, created_at ASC, reference_time ASC, job_id ASC
                     LIMIT 1",
                    [],
                    read_job_row,
                )
                .optional()?
                .transpose()?;
            let Some(head_job) = head else {
                tx.commit()?;
                return Ok(None);
            };

            let Some(head_meta) = load_window_source_meta_tx(&tx, &head_job.source_id)? else {
                let leased = lease_single_job_in_tx(&tx, &head_job.job_id, lease_until)?;
                let fallback_aud = AudienceSpec::Explicit(BTreeSet::from([leased.author.clone()]));
                tx.commit()?;
                return Ok(Some(LeasedExtractionWindow {
                    jobs: vec![leased],
                    readable_by: fallback_aud,
                    source_scope_id: None,
                    ingested_by: None,
                    primary_uri: String::new(),
                    has_http_uri: false,
                    slices: Vec::new(),
                }));
            };

            if head_meta.superseded_by_source.is_some() {
                tx.execute(
                    "UPDATE extraction_queue
                     SET status = 'done', lease_until = NULL, last_error = 'cancelled_superseded'
                     WHERE job_id = ?1",
                    [&head_job.job_id],
                )?;
                let _ = complete_session_source_span_tx(&tx, &head_job.source_id)?;
                continue;
            }

            let head_chunks = load_job_chunks_tx(&tx, &head_job.source_id, &head_job.chunk_ids)?;
            let head_has_tagged = head_chunks.iter().any(|c| has_tagged_turns(&c.body));
            let clean_tkey = head_job
                .thread_key
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());

            if clean_tkey.is_none() || head_has_tagged {
                let leased = lease_single_job_in_tx(&tx, &head_job.job_id, lease_until)?;
                let slices = head_chunks
                    .into_iter()
                    .map(|c| ExtractionChunkSlice {
                        chunk_id: c.chunk_id,
                        source_id: leased.source_id.clone(),
                        author: leased.author.clone(),
                        base_anchor: c.base_anchor,
                        role: SliceRole::Target,
                        reply_to_chunk_id: None,
                        heading: c.heading,
                        body: c.body,
                    })
                    .collect();
                let has_http = uri_is_http(&head_meta.uri);
                tx.commit()?;
                return Ok(Some(LeasedExtractionWindow {
                    jobs: vec![leased],
                    readable_by: head_meta.readable_by,
                    source_scope_id: head_meta.source_scope_id,
                    ingested_by: head_meta.ingested_by,
                    primary_uri: head_meta.uri,
                    has_http_uri: has_http,
                    slices,
                }));
            }

            let tkey = clean_tkey.unwrap_or("");
            let mut selected: Vec<(ExtractionJob, WindowSourceMeta, Vec<LoadedChunkRow>)> =
                Vec::new();
            let head_tokens = estimate_chunks_tokens(&head_chunks);
            let mut running_tokens = head_tokens;
            selected.push((head_job.clone(), head_meta.clone(), head_chunks));

            let candidates: Vec<ExtractionJob> = {
                let mut stmt = tx.prepare(
                    "SELECT job_id, source_id, chunk_ids_json, chunk_batch_sha256, author_actor,
                            model_policy, priority, reference_time, status, lease_until, attempts,
                            last_error, created_at, thread_key, reply_to_key
                     FROM extraction_queue
                     WHERE status = 'pending'
                       AND thread_key = ?1
                       AND model_policy = ?2
                       AND job_id != ?3
                     ORDER BY reference_time ASC, created_at ASC, job_id ASC",
                )?;
                let rows = stmt.query_map(
                    params![tkey, &head_job.model_policy, &head_job.job_id],
                    read_job_row,
                )?;
                rows.map(|r| r?).collect::<Result<Vec<_>>>()?
            };

            for cand in candidates {
                if crate::facts::is_source_marked_raw_only(&tx, cand.source_id.as_str())? {
                    continue;
                }
                let Some(cand_meta) = load_window_source_meta_tx(&tx, &cand.source_id)? else {
                    continue;
                };
                if cand_meta.superseded_by_source.is_some()
                    || cand_meta.source_scope_id != head_meta.source_scope_id
                    || cand_meta.readable_by != head_meta.readable_by
                    || cand_meta.ingested_by != head_meta.ingested_by
                {
                    continue;
                }
                let cand_chunks = load_job_chunks_tx(&tx, &cand.source_id, &cand.chunk_ids)?;
                if cand_chunks.iter().any(|c| has_tagged_turns(&c.body)) {
                    continue;
                }
                let cand_tokens = estimate_chunks_tokens(&cand_chunks);
                if running_tokens.saturating_add(cand_tokens) > budget {
                    break;
                }
                running_tokens = running_tokens.saturating_add(cand_tokens);
                selected.push((cand, cand_meta, cand_chunks));
            }

            selected.sort_by(|a, b| {
                a.0.reference_time
                    .cmp(&b.0.reference_time)
                    .then_with(|| a.0.created_at.cmp(&b.0.created_at))
                    .then_with(|| a.0.job_id.cmp(&b.0.job_id))
            });

            let mut leased_jobs = Vec::with_capacity(selected.len());
            for (job, _, _) in &selected {
                leased_jobs.push(lease_single_job_in_tx(&tx, &job.job_id, lease_until)?);
            }

            let earliest_ref = selected
                .first()
                .map_or(head_job.reference_time, |(j, _, _)| j.reference_time);
            let target_source_ids: BTreeSet<String> = selected
                .iter()
                .map(|(j, _, _)| j.source_id.as_str().to_owned())
                .collect();

            let context_entries = load_thread_context_entries_tx(
                &tx,
                &ThreadContextQuerySpec {
                    thread_key: tkey,
                    model_policy: &head_job.model_policy,
                    earliest_ref,
                    head_meta: &head_meta,
                    target_source_ids: &target_source_ids,
                    target_reply_keys: &selected
                        .iter()
                        .filter_map(|(j, _, _)| j.reply_to_key.clone())
                        .collect::<Vec<_>>(),
                },
            )?;

            let mut ext_key_to_chunk_id: BTreeMap<String, String> = BTreeMap::new();
            for ctx_entry in &context_entries {
                if let Some(ref ext_key) = ctx_entry.external_source_key {
                    register_ext_key_mapping(
                        &mut ext_key_to_chunk_id,
                        ext_key,
                        &ctx_entry.chunk.chunk_id,
                    );
                }
            }
            for (_, meta, chunks) in &selected {
                if let (Some(ext_key), Some(first_c)) = (&meta.external_source_key, chunks.first())
                {
                    register_ext_key_mapping(&mut ext_key_to_chunk_id, ext_key, &first_c.chunk_id);
                }
            }

            let mut slices = Vec::with_capacity(context_entries.len() + selected.len());
            for ctx_entry in context_entries {
                let reply_to_chunk_id = ctx_entry
                    .reply_to_key
                    .as_deref()
                    .and_then(|rk| ext_key_to_chunk_id.get(rk).cloned());
                slices.push(ExtractionChunkSlice {
                    chunk_id: ctx_entry.chunk.chunk_id,
                    source_id: ctx_entry.source_id,
                    author: ctx_entry.author,
                    base_anchor: ctx_entry.chunk.base_anchor,
                    role: SliceRole::Context,
                    reply_to_chunk_id,
                    heading: ctx_entry.chunk.heading,
                    body: ctx_entry.chunk.body,
                });
            }

            let mut has_http_uri = false;
            for ((_, meta, chunks), leased_job) in selected.into_iter().zip(leased_jobs.iter()) {
                has_http_uri |= uri_is_http(&meta.uri);
                let reply_to_chunk_id = leased_job
                    .reply_to_key
                    .as_deref()
                    .and_then(|rk| ext_key_to_chunk_id.get(rk).cloned());
                for (idx, c) in chunks.into_iter().enumerate() {
                    slices.push(ExtractionChunkSlice {
                        chunk_id: c.chunk_id,
                        source_id: leased_job.source_id.clone(),
                        author: leased_job.author.clone(),
                        base_anchor: c.base_anchor,
                        role: SliceRole::Target,
                        reply_to_chunk_id: if idx == 0 {
                            reply_to_chunk_id.clone()
                        } else {
                            None
                        },
                        heading: c.heading,
                        body: c.body,
                    });
                }
            }

            let primary_uri = head_meta.uri;
            tx.commit()?;
            return Ok(Some(LeasedExtractionWindow {
                jobs: leased_jobs,
                readable_by: head_meta.readable_by,
                source_scope_id: head_meta.source_scope_id,
                ingested_by: head_meta.ingested_by,
                primary_uri,
                has_http_uri,
                slices,
            }));
        }
    }

    /// Transitions a job to `paused_quota` without losing its cursor or counting as permanent failure.
    pub fn pause_extraction_job_quota(&mut self, job_id: &str, reason: &str) -> Result<()> {
        let changed = self.db.execute(
            "UPDATE extraction_queue
             SET status = 'paused_quota', lease_until = NULL, last_error = ?1
             WHERE job_id = ?2 AND status IN ('pending', 'leased', 'paused_quota')",
            params![reason, job_id],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Resumes all `paused_quota` jobs back to `pending` once host rate-limit headroom recovers.
    pub fn resume_paused_quota_jobs(&mut self) -> Result<usize> {
        Ok(self.db.execute(
            "UPDATE extraction_queue
             SET status = 'pending', lease_until = NULL
             WHERE status = 'paused_quota'",
            [],
        )?)
    }

    /// Advances `start_byte_offset` past any contiguous prefix of `session_source_span` intervals
    /// already persisted in `raw_sources` (re-arming any `'failed_permanent'` job on those spans to
    /// `'pending'`) so concurrent transcript appends (`[s, e)` -> `[s, e2)`) never re-ingest `[s, e)`
    /// into an overlapping `[s, e2)` job!
    pub fn advance_past_ingested_session_spans(
        &mut self,
        session_id: &str,
        start_byte_offset: u64,
        max_end_byte_offset: u64,
    ) -> Result<u64> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut spans: Vec<(String, u64, u64)> = {
            let mut stmt = tx.prepare(
                "SELECT c.target_id, c.payload_json
                 FROM change_log c
                 WHERE c.op = 'session_source_span'
                   AND c.logical_key = ?1
                   AND EXISTS(SELECT 1 FROM raw_sources r WHERE r.source_id = c.target_id)
                 ORDER BY c.seq ASC",
            )?;
            let mut rows = stmt.query([session_id])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                let target_id: String = row.get(0)?;
                let raw: String = row.get(1)?;
                let (s, e) = parse_span_payload(&raw)?;
                out.push((target_id, s, e));
            }
            out
        };
        spans.sort_by_key(|&(_, s, e)| (s, e));
        let mut frontier = start_byte_offset;
        for (source_id, s, e) in spans {
            if s <= frontier && e > frontier && e <= max_end_byte_offset {
                tx.execute(
                    "UPDATE extraction_queue
                     SET status = 'pending', lease_until = NULL, attempts = 0, last_error = NULL
                     WHERE source_id = ?1 AND status = 'failed_permanent'",
                    [&source_id],
                )?;
                frontier = e;
            }
        }
        tx.commit()?;
        Ok(frontier)
    }

    /// Returns `(has_any_incomplete, has_paused_quota)` across all persisted `session_source_span`
    /// intervals for `session_id` with `span_start < upto_end_byte_offset` whose `raw_sources` row
    /// still exists and whose `extraction_queue` job is not yet `'done'`.
    pub fn inspect_incomplete_session_spans_up_to(
        &self,
        session_id: &str,
        upto_end_byte_offset: u64,
    ) -> Result<(bool, bool)> {
        let mut stmt = self.db.prepare(
            "SELECT c.payload_json,
                    EXISTS(
                        SELECT 1 FROM extraction_queue q
                        WHERE q.source_id = c.target_id
                          AND q.status IN ('pending', 'leased', 'paused_quota', 'failed_permanent')
                    ),
                    EXISTS(
                        SELECT 1 FROM extraction_queue q
                        WHERE q.source_id = c.target_id
                          AND q.status = 'paused_quota'
                    )
             FROM change_log c
             WHERE c.op = 'session_source_span'
               AND c.logical_key = ?1
               AND EXISTS(SELECT 1 FROM raw_sources r WHERE r.source_id = c.target_id)",
        )?;
        let mut rows = stmt.query([session_id])?;
        let mut any_incomplete = false;
        let mut any_paused_quota = false;
        while let Some(row) = rows.next()? {
            let raw: String = row.get(0)?;
            let inc: bool = row.get(1)?;
            let paused: bool = row.get(2)?;
            let (s, _e) = parse_span_payload(&raw)?;
            if s < upto_end_byte_offset && inc {
                any_incomplete = true;
                any_paused_quota |= paused;
            }
        }
        Ok((any_incomplete, any_paused_quota))
    }

    /// Marks an extraction job `done`, clears its lease, and atomically advances any associated
    /// `session_source_span` cursor in the same transaction.
    pub fn complete_extraction_job(&mut self, job_id: &str) -> Result<()> {
        self.complete_extraction_job_with_chunks(job_id, &[], None, "", EpochMs(0))
    }

    /// Atomically marks `job_id` as `done`, records `(chunk_sha256, author, schema_ver)` in
    /// `extracted_chunks`, and advances any associated `session_source_span` in the same transaction.
    pub fn complete_extraction_job_with_chunks(
        &mut self,
        job_id: &str,
        chunk_hashes: &[ContentHash],
        author: Option<&ActorId>,
        schema_ver: &str,
        at: EpochMs,
    ) -> Result<()> {
        if !chunk_hashes.is_empty() && (author.is_none() || schema_ver.trim().is_empty()) {
            return Err(KpError::invalid(
                "extracted_chunks",
                "recording extracted chunks requires author and nonempty schema_ver",
            )
            .into());
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let source_id_str: Option<String> = tx
            .query_row(
                "SELECT source_id FROM extraction_queue WHERE job_id = ?1",
                [job_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(source_id_str) = source_id_str else {
            return Err(StoreError::NotFound);
        };
        tx.execute(
            "UPDATE extraction_queue
             SET status = 'done', lease_until = NULL, last_error = NULL
             WHERE job_id = ?1",
            [job_id],
        )?;
        if let Some(actor) = author {
            for hash in chunk_hashes {
                tx.execute(
                    "INSERT INTO extracted_chunks (chunk_sha256, author_actor, schema_ver, extracted_at)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(chunk_sha256, author_actor, schema_ver) DO NOTHING",
                    params![hash.as_str(), actor.as_str(), schema_ver.trim(), at.0],
                )?;
            }
        }
        let source_id: SourceId = source_id_str.parse()?;
        let _ = complete_session_source_span_tx(&tx, &source_id)?;
        tx.commit()?;
        Ok(())
    }

    /// Commits a [`StagedExtractionBatch`] atomically in one `TransactionBehavior::Immediate` transaction
    /// (for direct in-process `apply_extracted_atoms` callers outside `extraction_queue`).
    pub fn commit_staged_extraction_batch(&mut self, batch: &StagedExtractionBatch) -> Result<()> {
        let embedder = self.embedder.clone();
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        apply_staged_extraction_batch_tx(&tx, batch, &self.tokenizer, embedder.as_ref())?;
        tx.commit()?;
        post_commit_staged_extraction_effects(self, batch)?;
        Ok(())
    }

    /// Atomically verifies that all `(job_id, expected_attempts)` in `spec.jobs` are still actively
    /// leased at `lease_now` (`changed == spec.jobs.len()`), applies all staged writes, marks
    /// per-slice `(chunk_sha256, author_actor)` in `extracted_chunks`, transitions all jobs to `'done'`,
    /// and advances any `session_source_span` in ONE `TransactionBehavior::Immediate` transaction —
    /// rolling back 100% of writes on `Err(StoreError::Conflict)`.
    pub fn commit_staged_extraction_job_fenced(
        &mut self,
        spec: &FencedJobCommitSpec<'_>,
    ) -> Result<()> {
        let FencedJobCommitSpec {
            jobs,
            lease_now,
            batch,
            chunk_hashes,
            schema_ver,
            at,
        } = *spec;
        if jobs.is_empty() {
            return Err(KpError::invalid("jobs", "fenced commit requires at least one job").into());
        }
        if !chunk_hashes.is_empty() && schema_ver.trim().is_empty() {
            return Err(KpError::invalid(
                "extracted_chunks",
                "recording extracted chunks requires nonempty schema_ver",
            )
            .into());
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut source_ids = Vec::with_capacity(jobs.len());
        let mut raw_only_jobs = Vec::new();
        for (job_id, _) in jobs {
            let sid_str: Option<String> = tx
                .query_row(
                    "SELECT source_id FROM extraction_queue WHERE job_id = ?1",
                    [job_id.as_str()],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(sid_str) = sid_str else {
                return Err(StoreError::NotFound);
            };
            if crate::facts::is_source_marked_raw_only(&tx, &sid_str)? {
                raw_only_jobs.push((job_id.as_str(), sid_str.clone()));
            }
            source_ids.push(sid_str);
        }
        if !raw_only_jobs.is_empty() {
            for (job_id, sid_str) in raw_only_jobs {
                tx.execute(
                    "UPDATE extraction_queue
                     SET status = 'done', lease_until = NULL, last_error = 'cancelled_raw_only'
                     WHERE job_id = ?1",
                    [job_id],
                )?;
                let sid: SourceId = sid_str.parse()?;
                let _ = complete_session_source_span_tx(&tx, &sid)?;
            }
            tx.commit()?;
            return Err(StoreError::Conflict);
        }

        let mut changed_total = 0usize;
        for (job_id, expected_attempts) in jobs {
            changed_total += tx.execute(
                "UPDATE extraction_queue
                 SET status = 'done', lease_until = NULL, last_error = NULL
                 WHERE job_id = ?1
                   AND status = 'leased'
                   AND attempts = ?2
                   AND (lease_until IS NULL OR lease_until > ?3)",
                params![job_id, expected_attempts, lease_now.0],
            )?;
        }
        if changed_total != jobs.len() {
            return Err(StoreError::Conflict);
        }

        apply_staged_extraction_batch_tx(&tx, batch, &self.tokenizer, self.embedder.as_ref())?;
        for (hash, actor) in chunk_hashes {
            tx.execute(
                "INSERT INTO extracted_chunks (chunk_sha256, author_actor, schema_ver, extracted_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(chunk_sha256, author_actor, schema_ver) DO NOTHING",
                params![hash.as_str(), actor.as_str(), schema_ver.trim(), at.0],
            )?;
        }
        let mut completed_sources = BTreeSet::new();
        for sid_str in source_ids {
            if completed_sources.insert(sid_str.clone()) {
                let sid: SourceId = sid_str.parse()?;
                let _ = complete_session_source_span_tx(&tx, &sid)?;
            }
        }
        tx.commit()?;
        post_commit_staged_extraction_effects(self, batch)?;
        Ok(())
    }

    /// Checks whether all `(job_id, expected_attempts)` in `jobs` are still actively leased at `now`.
    pub fn verify_extraction_window_leases(
        &self,
        jobs: &[(String, u32)],
        now: EpochMs,
    ) -> Result<bool> {
        for (job_id, expected_attempts) in jobs {
            if !self.verify_extraction_job_lease(job_id, *expected_attempts, now)? {
                return Ok(false);
            }
        }
        Ok(!jobs.is_empty())
    }

    /// Multi-job lease-fenced completion without staged facts (`SkippedEmpty` / pre-LLM ack skip),
    /// rolling back completely if any job in `jobs` lost its lease.
    pub fn complete_extraction_window_fenced(
        &mut self,
        jobs: &[(String, u32)],
        chunk_hashes: &[(ContentHash, ActorId)],
        schema_ver: &str,
        at: EpochMs,
    ) -> Result<()> {
        if jobs.is_empty() {
            return Err(
                KpError::invalid("jobs", "fenced completion requires at least one job").into(),
            );
        }
        if !chunk_hashes.is_empty() && schema_ver.trim().is_empty() {
            return Err(KpError::invalid(
                "extracted_chunks",
                "recording extracted chunks requires nonempty schema_ver",
            )
            .into());
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut source_ids = Vec::with_capacity(jobs.len());
        let mut changed_total = 0usize;
        for (job_id, expected_attempts) in jobs {
            let sid_str: Option<String> = tx
                .query_row(
                    "SELECT source_id FROM extraction_queue WHERE job_id = ?1",
                    [job_id.as_str()],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(sid_str) = sid_str else {
                return Err(StoreError::NotFound);
            };
            source_ids.push(sid_str);
            changed_total += tx.execute(
                "UPDATE extraction_queue
                 SET status = 'done', lease_until = NULL, last_error = NULL
                 WHERE job_id = ?1 AND status = 'leased' AND attempts = ?2",
                params![job_id, expected_attempts],
            )?;
        }
        if changed_total != jobs.len() {
            return Err(StoreError::Conflict);
        }
        for (hash, actor) in chunk_hashes {
            tx.execute(
                "INSERT INTO extracted_chunks (chunk_sha256, author_actor, schema_ver, extracted_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(chunk_sha256, author_actor, schema_ver) DO NOTHING",
                params![hash.as_str(), actor.as_str(), schema_ver.trim(), at.0],
            )?;
        }
        let mut seen = BTreeSet::new();
        for sid_str in source_ids {
            if seen.insert(sid_str.clone()) {
                let sid: SourceId = sid_str.parse()?;
                let _ = complete_session_source_span_tx(&tx, &sid)?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Transitions still-leased jobs in `jobs` to `paused_quota`.
    pub fn pause_extraction_window_quota_fenced(
        &mut self,
        jobs: &[(String, u32)],
        reason: &str,
    ) -> Result<usize> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut paused = 0usize;
        for (job_id, expected_attempts) in jobs {
            paused += tx.execute(
                "UPDATE extraction_queue
                 SET status = 'paused_quota', lease_until = NULL, last_error = ?1
                 WHERE job_id = ?2 AND status = 'leased' AND attempts = ?3",
                params![reason, job_id, expected_attempts],
            )?;
        }
        tx.commit()?;
        Ok(paused)
    }

    /// Releases still-leased jobs in `jobs` for transient retry (or `failed_permanent` at `max_attempts`).
    pub fn release_extraction_window_for_retry(
        &mut self,
        jobs: &[(String, u32)],
        error: &str,
        retry_after: EpochMs,
        max_attempts: u32,
    ) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (job_id, expected_attempts) in jobs {
            if *expected_attempts < max_attempts {
                tx.execute(
                    "UPDATE extraction_queue
                     SET status = 'leased', lease_until = ?1, last_error = ?2
                     WHERE job_id = ?3 AND status = 'leased' AND attempts = ?4",
                    params![retry_after.0, error, job_id, expected_attempts],
                )?;
            } else {
                tx.execute(
                    "UPDATE extraction_queue
                     SET status = 'failed_permanent', lease_until = NULL, last_error = ?1
                     WHERE job_id = ?2 AND status = 'leased' AND attempts = ?3",
                    params![error, job_id, expected_attempts],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Checks whether `job_id` is still actively leased under `expected_attempts` at `now`.
    pub fn verify_extraction_job_lease(
        &self,
        job_id: &str,
        expected_attempts: u32,
        now: EpochMs,
    ) -> Result<bool> {
        Ok(self.db.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM extraction_queue
                 WHERE job_id = ?1
                   AND status = 'leased'
                   AND attempts = ?2
                   AND (lease_until IS NULL OR lease_until > ?3)
             )",
            params![job_id, expected_attempts, now.0],
            |r| r.get(0),
        )?)
    }

    /// Lease-fenced completion (`WHERE job_id = ?1 AND status = 'leased' AND attempts = ?2`),
    /// returning `Err(StoreError::Conflict)` on a stale lease and atomically advancing any
    /// associated `session_source_span` in the same `IMMEDIATE` transaction.
    pub fn complete_extraction_job_fenced(
        &mut self,
        job_id: &str,
        expected_attempts: u32,
        chunk_hashes: &[ContentHash],
        author: Option<&ActorId>,
        schema_ver: &str,
        at: EpochMs,
    ) -> Result<()> {
        if !chunk_hashes.is_empty() && (author.is_none() || schema_ver.trim().is_empty()) {
            return Err(KpError::invalid(
                "extracted_chunks",
                "recording extracted chunks requires author and nonempty schema_ver",
            )
            .into());
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let source_id_str: Option<String> = tx
            .query_row(
                "SELECT source_id FROM extraction_queue WHERE job_id = ?1",
                [job_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(source_id_str) = source_id_str else {
            return Err(StoreError::NotFound);
        };
        let changed = tx.execute(
            "UPDATE extraction_queue
             SET status = 'done', lease_until = NULL, last_error = NULL
             WHERE job_id = ?1 AND status = 'leased' AND attempts = ?2",
            params![job_id, expected_attempts],
        )?;
        if changed == 0 {
            return Err(StoreError::Conflict);
        }
        if let Some(actor) = author {
            for hash in chunk_hashes {
                tx.execute(
                    "INSERT INTO extracted_chunks (chunk_sha256, author_actor, schema_ver, extracted_at)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(chunk_sha256, author_actor, schema_ver) DO NOTHING",
                    params![hash.as_str(), actor.as_str(), schema_ver.trim(), at.0],
                )?;
            }
        }
        let source_id: SourceId = source_id_str.parse()?;
        let _ = complete_session_source_span_tx(&tx, &source_id)?;
        tx.commit()?;
        Ok(())
    }

    /// Lease-fenced quota pause (`WHERE job_id = ?1 AND status = 'leased' AND attempts = ?2`),
    /// returning `Err(StoreError::Conflict)` on a stale lease.
    pub fn pause_extraction_job_quota_fenced(
        &mut self,
        job_id: &str,
        expected_attempts: u32,
        reason: &str,
    ) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM extraction_queue WHERE job_id = ?1)",
            [job_id],
            |r| r.get(0),
        )?;
        if !exists {
            return Err(StoreError::NotFound);
        }
        let changed = tx.execute(
            "UPDATE extraction_queue
             SET status = 'paused_quota', lease_until = NULL, last_error = ?1
             WHERE job_id = ?2 AND status = 'leased' AND attempts = ?3",
            params![reason, job_id, expected_attempts],
        )?;
        if changed == 0 {
            return Err(StoreError::Conflict);
        }
        tx.commit()?;
        Ok(())
    }

    /// Lease-fenced transient retry / max-attempts termination:
    /// - When `expected_attempts < max_attempts`, sets `status = 'leased', lease_until = retry_after, last_error = error`
    ///   so [`Store::lease_next_extraction_job`] automatically recovers it back to `'pending'` at `now >= retry_after`.
    /// - When `expected_attempts >= max_attempts`, transitions to `'failed_permanent'`.
    pub fn release_extraction_job_for_retry(
        &mut self,
        job_id: &str,
        expected_attempts: u32,
        error: &str,
        retry_after: EpochMs,
        max_attempts: u32,
    ) -> Result<ExtractionJobStatus> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM extraction_queue WHERE job_id = ?1)",
            [job_id],
            |r| r.get(0),
        )?;
        if !exists {
            return Err(StoreError::NotFound);
        }
        if expected_attempts < max_attempts {
            let changed = tx.execute(
                "UPDATE extraction_queue
                 SET status = 'leased', lease_until = ?1, last_error = ?2
                 WHERE job_id = ?3 AND status = 'leased' AND attempts = ?4",
                params![retry_after.0, error, job_id, expected_attempts],
            )?;
            if changed == 0 {
                return Err(StoreError::Conflict);
            }
            tx.commit()?;
            Ok(ExtractionJobStatus::Leased)
        } else {
            let changed = tx.execute(
                "UPDATE extraction_queue
                 SET status = 'failed_permanent', lease_until = NULL, last_error = ?1
                 WHERE job_id = ?2 AND status = 'leased' AND attempts = ?3",
                params![error, job_id, expected_attempts],
            )?;
            if changed == 0 {
                return Err(StoreError::Conflict);
            }
            tx.commit()?;
            Ok(ExtractionJobStatus::FailedPermanent)
        }
    }

    /// Marks an extraction job `failed_permanent` with diagnostic context.
    pub fn fail_extraction_job(&mut self, job_id: &str, error: &str) -> Result<()> {
        let changed = self.db.execute(
            "UPDATE extraction_queue
             SET status = 'failed_permanent', lease_until = NULL, last_error = ?1
             WHERE job_id = ?2",
            params![error, job_id],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Reads an `ExtractionJob` by `job_id`.
    pub fn get_extraction_job(&self, job_id: &str) -> Result<Option<ExtractionJob>> {
        self.db
            .query_row(
                "SELECT job_id, source_id, chunk_ids_json, chunk_batch_sha256, author_actor,
                        model_policy, priority, reference_time, status, lease_until, attempts,
                        last_error, created_at, thread_key, reply_to_key
                 FROM extraction_queue
                 WHERE job_id = ?1",
                [job_id],
                read_job_row,
            )
            .optional()?
            .transpose()
    }

    /// Checks `(chunk_sha256, author_actor, schema_ver)` idempotency in `extracted_chunks`.
    pub fn is_chunk_extracted(
        &self,
        chunk_sha256: &ContentHash,
        author: &ActorId,
        schema_ver: &str,
    ) -> Result<bool> {
        Ok(self.db.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM extracted_chunks
                WHERE chunk_sha256 = ?1 AND author_actor = ?2 AND schema_ver = ?3
            )",
            params![chunk_sha256.as_str(), author.as_str(), schema_ver.trim()],
            |row| row.get(0),
        )?)
    }

    /// Records `(chunk_sha256, author_actor, schema_ver)` in `extracted_chunks`; returns `true` if newly inserted.
    pub fn mark_chunk_extracted(
        &mut self,
        chunk_sha256: &ContentHash,
        author: &ActorId,
        schema_ver: &str,
        at: EpochMs,
    ) -> Result<bool> {
        let ver = schema_ver.trim();
        if ver.is_empty() {
            return Err(KpError::invalid("schema_ver", "schema_ver must be nonempty").into());
        }
        let changed = self.db.execute(
            "INSERT INTO extracted_chunks (chunk_sha256, author_actor, schema_ver, extracted_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(chunk_sha256, author_actor, schema_ver) DO NOTHING",
            params![chunk_sha256.as_str(), author.as_str(), ver, at.0],
        )?;
        Ok(changed > 0)
    }

    /// Idempotently binds a sub-batch `source_id` to `(session_id, start_byte_offset, end_byte_offset)`
    /// in `change_log(op='session_source_span')` so multi-day / multi-batch extraction advances
    /// `session_cursors.last_extracted_offset` strictly through contiguous completed spans.
    pub fn record_session_source_span(
        &mut self,
        source_id: &SourceId,
        session_id: &str,
        start_byte_offset: u64,
        end_byte_offset: u64,
        at: EpochMs,
    ) -> Result<()> {
        validate_session_span_args(session_id, start_byte_offset, end_byte_offset)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        record_session_source_span_tx(
            &tx,
            source_id,
            session_id,
            start_byte_offset,
            end_byte_offset,
            at,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Computes the highest contiguous completed `end_byte_offset` for `source_id`'s session
    /// such that no span with `span.end <= candidate_end` (or overlapping `< candidate_end`)
    /// has an incomplete job in `extraction_queue`, and atomically updates
    /// `session_cursors.last_extracted_offset = MAX(last_extracted_offset, MIN(candidate_end, last_byte_offset))`
    /// without ever jumping to `last_byte_offset`.
    pub fn complete_session_source_span(
        &mut self,
        source_id: &SourceId,
    ) -> Result<Option<(String, u64)>> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let out = complete_session_source_span_tx(&tx, source_id)?;
        tx.commit()?;
        Ok(out)
    }
}

fn apply_staged_extraction_batch_tx(
    tx: &rusqlite::Connection,
    batch: &StagedExtractionBatch,
    tokenizer: &crate::Tokenizer,
    embedder: &(impl Embedder + ?Sized),
) -> Result<()> {
    for write in &batch.writes {
        match write {
            StagedAtomWrite::Remember(input) => {
                crate::slots::remember_facts_indexed(
                    tx,
                    std::slice::from_ref(input),
                    tokenizer,
                    embedder,
                )?;
            }
            StagedAtomWrite::Supersede {
                target_fact_id,
                replacement,
            } => {
                crate::maintenance::supersede_tx(tx, target_fact_id, replacement)?;
            }
        }
    }
    for (dfact, proof) in &batch.derivations {
        crate::maintenance::save_derivations_tx(
            tx,
            std::slice::from_ref(dfact),
            std::slice::from_ref(proof),
        )?;
    }
    for concept in &batch.concepts {
        crate::exchange::upsert_concept_tx(tx, concept, false)?;
    }
    for created in &batch.created_concepts {
        crate::governance::upsert_community_concept_tx(tx, created)?;
    }
    for (fid, cids, link_at) in &batch.fact_concept_links {
        for cid in cids {
            crate::exchange::link_fact_concept_tx(tx, fid, cid, *link_at)?;
        }
    }
    Ok(())
}

fn post_commit_staged_extraction_effects(
    store: &mut Store,
    batch: &StagedExtractionBatch,
) -> Result<()> {
    let mut had_writes = false;
    for write in &batch.writes {
        match write {
            StagedAtomWrite::Remember(input) => {
                store.register_entities(std::slice::from_ref(input));
                had_writes = true;
            }
            StagedAtomWrite::Supersede { .. } => {
                had_writes = true;
            }
        }
    }
    for c in &batch.created_concepts {
        store.tokenizer.add_entity_word(&c.canonical_label);
    }
    if had_writes {
        store.sync_skills_now()?;
    }
    Ok(())
}

pub(crate) fn validate_session_span_args(
    session_id: &str,
    start_byte_offset: u64,
    end_byte_offset: u64,
) -> Result<()> {
    if session_id.trim().is_empty() {
        return Err(KpError::invalid("session_id", "session_id must be nonempty").into());
    }
    if end_byte_offset < start_byte_offset {
        return Err(KpError::invalid(
            "end_byte_offset",
            "end_byte_offset must be >= start_byte_offset",
        )
        .into());
    }
    let _ = i64::try_from(end_byte_offset)
        .map_err(|_| KpError::invalid("end_byte_offset", "offset exceeds i64"))?;
    Ok(())
}

pub(crate) fn record_session_source_span_tx(
    tx: &rusqlite::Connection,
    source_id: &SourceId,
    session_id: &str,
    start_byte_offset: u64,
    end_byte_offset: u64,
    at: EpochMs,
) -> Result<()> {
    let session = session_id.trim();
    let payload = serde_json::to_string(&serde_json::json!({
        "start": start_byte_offset,
        "end": end_byte_offset,
    }))?;
    let existing_seq: Option<i64> = tx
        .query_row(
            "SELECT seq FROM change_log
             WHERE op = 'session_source_span' AND target_id = ?1
             ORDER BY seq DESC LIMIT 1",
            [source_id.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(seq) = existing_seq {
        tx.execute(
            "UPDATE change_log SET logical_key = ?1, payload_json = ?2, created_at = ?3 WHERE seq = ?4",
            params![session, payload, at.0, seq],
        )?;
    } else {
        tx.execute(
            "INSERT INTO change_log (op, target_id, logical_key, payload_json, created_at)
             VALUES ('session_source_span', ?1, ?2, ?3, ?4)",
            params![source_id.as_str(), session, payload, at.0],
        )?;
    }
    Ok(())
}

pub(crate) fn enqueue_or_rearm_extraction_job_tx(
    tx: &rusqlite::Connection,
    source_id: &SourceId,
    input: &ExtractionJobInput,
    thread_key: Option<&str>,
    reply_to_key: Option<&str>,
) -> Result<ExtractionJob> {
    if input.chunk_ids.is_empty() || input.chunk_ids.iter().any(|c| c.trim().is_empty()) {
        return Err(KpError::invalid(
            "chunk_ids",
            "extraction job requires at least one nonempty chunk_id",
        )
        .into());
    }
    let policy = input.model_policy.trim();
    if policy.is_empty() || policy == "raw_only" {
        return Err(KpError::invalid(
            "model_policy",
            "model_policy must be a valid non-raw_only extraction route",
        )
        .into());
    }
    let clean_thread = thread_key.map(str::trim).filter(|s| !s.is_empty());
    let clean_reply = reply_to_key.map(str::trim).filter(|s| !s.is_empty());
    let existing: Option<ExtractionJob> = tx
        .query_row(
            "SELECT job_id, source_id, chunk_ids_json, chunk_batch_sha256, author_actor,
                    model_policy, priority, reference_time, status, lease_until, attempts,
                    last_error, created_at, thread_key, reply_to_key
             FROM extraction_queue
             WHERE source_id = ?1
               AND chunk_batch_sha256 = ?2
               AND author_actor = ?3
             ORDER BY (status IN ('pending', 'leased', 'paused_quota', 'done')) DESC,
                      created_at ASC, job_id ASC
             LIMIT 1",
            params![
                source_id.as_str(),
                input.chunk_batch_sha256.as_str(),
                input.author.as_str(),
            ],
            read_job_row,
        )
        .optional()?
        .transpose()?;

    if let Some(mut job) = existing {
        if job.status == ExtractionJobStatus::FailedPermanent {
            tx.execute(
                "UPDATE extraction_queue
                 SET status = 'pending', lease_until = NULL, attempts = 0, last_error = NULL,
                     priority = ?1,
                     thread_key = COALESCE(thread_key, ?2),
                     reply_to_key = COALESCE(reply_to_key, ?3)
                 WHERE job_id = ?4",
                params![input.priority, clean_thread, clean_reply, &job.job_id],
            )?;
            job.status = ExtractionJobStatus::Pending;
            job.lease_until = None;
            job.attempts = 0;
            job.last_error = None;
            job.priority = input.priority;
        }
        return Ok(job);
    }

    let job_id = new_job_id()?;
    if is_bare_content_hash(&job_id) {
        return Err(KpError::invalid("job_id", "unexpected bare digest").into());
    }
    let chunk_ids_json = serde_json::to_string(&input.chunk_ids)?;
    tx.execute(
        "INSERT INTO extraction_queue
            (job_id, source_id, chunk_ids_json, chunk_batch_sha256, author_actor, model_policy,
             priority, reference_time, status, lease_until, attempts, last_error, created_at,
             thread_key, reply_to_key)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', NULL, 0, NULL, ?9, ?10, ?11)",
        params![
            &job_id,
            source_id.as_str(),
            chunk_ids_json,
            input.chunk_batch_sha256.as_str(),
            input.author.as_str(),
            policy,
            input.priority,
            input.reference_time.0,
            input.created_at.0,
            clean_thread,
            clean_reply,
        ],
    )?;
    Ok(ExtractionJob {
        job_id,
        source_id: source_id.clone(),
        chunk_ids: input.chunk_ids.clone(),
        chunk_batch_sha256: input.chunk_batch_sha256.clone(),
        author: input.author.clone(),
        model_policy: policy.to_owned(),
        priority: input.priority,
        reference_time: input.reference_time,
        status: ExtractionJobStatus::Pending,
        lease_until: None,
        attempts: 0,
        last_error: None,
        created_at: input.created_at,
        thread_key: clean_thread.map(str::to_owned),
        reply_to_key: clean_reply.map(str::to_owned),
    })
}

#[derive(Clone)]
struct WindowSourceMeta {
    readable_by: AudienceSpec,
    source_scope_id: Option<SourceScopeId>,
    ingested_by: Option<ActorId>,
    uri: String,
    external_source_key: Option<String>,
    superseded_by_source: Option<String>,
}

struct LoadedChunkRow {
    chunk_id: String,
    heading: String,
    body: String,
    base_anchor: AnchorSpec,
}

struct ThreadContextEntry {
    source_id: SourceId,
    author: ActorId,
    external_source_key: Option<String>,
    reply_to_key: Option<String>,
    chunk: LoadedChunkRow,
}

struct ThreadContextQuerySpec<'a> {
    thread_key: &'a str,
    model_policy: &'a str,
    earliest_ref: EpochMs,
    head_meta: &'a WindowSourceMeta,
    target_source_ids: &'a BTreeSet<String>,
    target_reply_keys: &'a [String],
}

fn reconcile_queue_leases_tx(tx: &rusqlite::Connection, now: EpochMs) -> Result<()> {
    tx.execute(
        "UPDATE extraction_queue
         SET status = 'done', lease_until = NULL, last_error = 'cancelled_raw_only'
         WHERE status IN ('pending', 'leased', 'paused_quota', 'failed_permanent')
           AND EXISTS (
               SELECT 1 FROM change_log cl
               WHERE cl.op = 'raw_only_source' AND cl.target_id = extraction_queue.source_id
           )",
        [],
    )?;
    tx.execute(
        "UPDATE extraction_queue
         SET status = 'pending', lease_until = NULL
         WHERE status = 'leased' AND lease_until IS NOT NULL AND lease_until <= ?1",
        [now.0],
    )?;
    Ok(())
}

fn lease_single_job_in_tx(
    tx: &rusqlite::Connection,
    job_id: &str,
    lease_until: EpochMs,
) -> Result<ExtractionJob> {
    tx.execute(
        "UPDATE extraction_queue
         SET status = 'leased', lease_until = ?1, attempts = attempts + 1
         WHERE job_id = ?2",
        params![lease_until.0, job_id],
    )?;
    tx.query_row(
        "SELECT job_id, source_id, chunk_ids_json, chunk_batch_sha256, author_actor,
                model_policy, priority, reference_time, status, lease_until, attempts,
                last_error, created_at, thread_key, reply_to_key
         FROM extraction_queue
         WHERE job_id = ?1",
        [job_id],
        read_job_row,
    )?
}

struct RawWindowSourceRow {
    aud_json: String,
    scope_opt: Option<String>,
    ing_opt: Option<String>,
    uri: String,
    ext_key: Option<String>,
    sup_opt: Option<String>,
}

fn load_window_source_meta_tx(
    tx: &rusqlite::Connection,
    source_id: &SourceId,
) -> Result<Option<WindowSourceMeta>> {
    let row: Option<RawWindowSourceRow> = tx
        .query_row(
            "SELECT COALESCE(
                (SELECT c.readable_by_json FROM chunks c WHERE c.source_id = s.source_id ORDER BY c.ordinal ASC LIMIT 1),
                s.visibility_scope
             ), s.source_scope_id, s.ingested_by_actor, s.uri, s.external_source_key, s.superseded_by_source
             FROM raw_sources s
             WHERE s.source_id = ?1",
            [source_id.as_str()],
            |r| {
                Ok(RawWindowSourceRow {
                    aud_json: r.get(0)?,
                    scope_opt: r.get(1)?,
                    ing_opt: r.get(2)?,
                    uri: r.get(3)?,
                    ext_key: r.get(4)?,
                    sup_opt: r.get(5)?,
                })
            },
        )
        .optional()?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(WindowSourceMeta {
        readable_by: serde_json::from_str(&row.aud_json)?,
        source_scope_id: row
            .scope_opt
            .map(|s| s.parse::<SourceScopeId>())
            .transpose()?,
        ingested_by: row.ing_opt.map(|s| s.parse::<ActorId>()).transpose()?,
        uri: row.uri,
        external_source_key: row.ext_key,
        superseded_by_source: row.sup_opt,
    }))
}

fn load_job_chunks_tx(
    tx: &rusqlite::Connection,
    source_id: &SourceId,
    chunk_ids: &[String],
) -> Result<Vec<LoadedChunkRow>> {
    if chunk_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = tx.prepare(
        "SELECT chunk_id, heading, body, anchor_json
         FROM chunks
         WHERE source_id = ?1 AND chunk_id = ?2",
    )?;
    let mut out = Vec::with_capacity(chunk_ids.len());
    for cid in chunk_ids {
        if let Some((chunk_id, heading, body, anchor_raw)) = stmt
            .query_row(params![source_id.as_str(), cid.as_str()], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .optional()?
        {
            out.push(LoadedChunkRow {
                chunk_id,
                heading,
                body,
                base_anchor: serde_json::from_str(&anchor_raw)?,
            });
        }
    }
    Ok(out)
}

fn estimate_chunks_tokens(chunks: &[LoadedChunkRow]) -> usize {
    chunks
        .iter()
        .map(|c| {
            estimate_batch_tokens(&c.heading)
                .saturating_add(estimate_batch_tokens(&c.body))
                .saturating_add(4)
                .max(1)
        })
        .sum::<usize>()
        .max(1)
}

fn uri_is_http(uri: &str) -> bool {
    let trimmed = uri.trim();
    trimmed
        .get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case("http://"))
        || trimmed
            .get(..8)
            .is_some_and(|p| p.eq_ignore_ascii_case("https://"))
}

fn register_ext_key_mapping(map: &mut BTreeMap<String, String>, ext_key: &str, chunk_id: &str) {
    map.insert(ext_key.to_owned(), chunk_id.to_owned());
    if let Some((base, _)) = ext_key.rsplit_once("@rev") {
        map.entry(base.to_owned())
            .or_insert_with(|| chunk_id.to_owned());
    }
}

fn load_thread_context_entries_tx(
    tx: &rusqlite::Connection,
    spec: &ThreadContextQuerySpec<'_>,
) -> Result<Vec<ThreadContextEntry>> {
    const MAX_CONTEXT_CHUNKS: usize = 5;
    let mut stmt = tx.prepare(
        "SELECT job_id, source_id, chunk_ids_json, chunk_batch_sha256, author_actor,
                model_policy, priority, reference_time, status, lease_until, attempts,
                last_error, created_at, thread_key, reply_to_key
         FROM extraction_queue
         WHERE status = 'done'
           AND thread_key = ?1
           AND model_policy = ?2
           AND reference_time <= ?3
           AND last_error IS NULL
         ORDER BY reference_time DESC, created_at DESC, job_id DESC
         LIMIT 20",
    )?;
    let prior_jobs: Vec<ExtractionJob> = stmt
        .query_map(
            params![spec.thread_key, spec.model_policy, spec.earliest_ref.0],
            read_job_row,
        )?
        .map(|r| r?)
        .collect::<Result<Vec<_>>>()?;

    let mut seen_sources = spec.target_source_ids.clone();
    let mut collected_rev = Vec::new();
    for pjob in prior_jobs {
        if collected_rev.len() >= MAX_CONTEXT_CHUNKS {
            break;
        }
        if !seen_sources.insert(pjob.source_id.as_str().to_owned()) {
            continue;
        }
        if crate::facts::is_source_marked_raw_only(tx, pjob.source_id.as_str())? {
            continue;
        }
        let Some(pmeta) = load_window_source_meta_tx(tx, &pjob.source_id)? else {
            continue;
        };
        if pmeta.superseded_by_source.is_some()
            || pmeta.source_scope_id != spec.head_meta.source_scope_id
            || pmeta.readable_by != spec.head_meta.readable_by
            || pmeta.ingested_by != spec.head_meta.ingested_by
        {
            continue;
        }
        let pchunks = load_job_chunks_tx(tx, &pjob.source_id, &pjob.chunk_ids)?;
        if pchunks.iter().any(|c| has_tagged_turns(&c.body)) {
            continue;
        }
        for chunk in pchunks.into_iter().rev() {
            if collected_rev.len() >= MAX_CONTEXT_CHUNKS {
                break;
            }
            collected_rev.push(ThreadContextEntry {
                source_id: pjob.source_id.clone(),
                author: pjob.author.clone(),
                external_source_key: pmeta.external_source_key.clone(),
                reply_to_key: pjob.reply_to_key.clone(),
                chunk,
            });
        }
    }
    collected_rev.reverse();

    for rk in spec.target_reply_keys {
        let already_present = collected_rev.iter().any(|e| {
            e.external_source_key.as_deref().is_some_and(|ek| {
                ek == rk || ek.strip_prefix(rk).is_some_and(|s| s.starts_with("@rev"))
            })
        });
        if already_present {
            continue;
        }
        let rev_pattern = format!("{rk}@rev%");
        let parent_job: Option<ExtractionJob> = tx
            .query_row(
                "SELECT q.job_id, q.source_id, q.chunk_ids_json, q.chunk_batch_sha256, q.author_actor,
                        q.model_policy, q.priority, q.reference_time, q.status, q.lease_until, q.attempts,
                        q.last_error, q.created_at, q.thread_key, q.reply_to_key
                 FROM extraction_queue q
                 JOIN raw_sources s ON s.source_id = q.source_id
                 WHERE s.superseded_by_source IS NULL
                   AND (s.external_source_key = ?1 OR s.external_source_key LIKE ?2)
                   AND q.model_policy = ?3
                 ORDER BY q.reference_time DESC, q.created_at DESC
                 LIMIT 1",
                params![rk, rev_pattern, spec.model_policy],
                read_job_row,
            )
            .optional()?
            .transpose()?;
        if let Some(pjob) = parent_job {
            if !seen_sources.insert(pjob.source_id.as_str().to_owned()) {
                continue;
            }
            if crate::facts::is_source_marked_raw_only(tx, pjob.source_id.as_str())? {
                continue;
            }
            if let Some(pmeta) = load_window_source_meta_tx(tx, &pjob.source_id)? {
                if pmeta.superseded_by_source.is_none()
                    && pmeta.source_scope_id == spec.head_meta.source_scope_id
                    && pmeta.readable_by == spec.head_meta.readable_by
                    && pmeta.ingested_by == spec.head_meta.ingested_by
                {
                    let pchunks = load_job_chunks_tx(tx, &pjob.source_id, &pjob.chunk_ids)?;
                    if !pchunks.iter().any(|c| has_tagged_turns(&c.body)) {
                        if let Some(first_chunk) = pchunks.into_iter().next() {
                            collected_rev.insert(
                                0,
                                ThreadContextEntry {
                                    source_id: pjob.source_id,
                                    author: pjob.author,
                                    external_source_key: pmeta.external_source_key,
                                    reply_to_key: pjob.reply_to_key,
                                    chunk: first_chunk,
                                },
                            );
                        }
                    }
                }
            }
        }
    }

    Ok(collected_rev)
}

pub(crate) fn complete_session_source_span_tx(
    tx: &rusqlite::Connection,
    source_id: &SourceId,
) -> Result<Option<(String, u64)>> {
    let this_row: Option<(String, String)> = tx
        .query_row(
            "SELECT logical_key, payload_json FROM change_log
             WHERE op = 'session_source_span' AND target_id = ?1
             ORDER BY seq DESC LIMIT 1",
            [source_id.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((session_id, this_payload_str)) = this_row else {
        return Ok(None);
    };
    let (_, this_end) = parse_span_payload(&this_payload_str)?;

    let cursor_row: Option<(i64, i64, i64)> = tx
        .query_row(
            "SELECT last_byte_offset, last_extracted_offset, last_activity_at
             FROM session_cursors WHERE session_id = ?1",
            [&session_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((cur_byte_i64, cur_extracted_i64, activity_ms)) = cursor_row else {
        return Ok(None);
    };
    let cur_byte = u64::try_from(cur_byte_i64).unwrap_or(0);
    let cur_extracted = u64::try_from(cur_extracted_i64).unwrap_or(0);

    let mut spans = {
        let mut stmt = tx.prepare(
            "SELECT c.payload_json,
                    EXISTS(
                        SELECT 1 FROM extraction_queue q
                        WHERE q.source_id = c.target_id
                          AND q.status IN ('pending', 'leased', 'paused_quota', 'failed_permanent')
                    ),
                    EXISTS(
                        SELECT 1 FROM extraction_queue q
                        WHERE q.source_id = c.target_id
                          AND q.status = 'done'
                    )
             FROM change_log c
             WHERE c.op = 'session_source_span' AND c.logical_key = ?1
             ORDER BY c.seq ASC",
        )?;
        let mut rows = stmt.query([&session_id])?;
        let mut parsed = Vec::new();
        while let Some(row) = rows.next()? {
            let raw: String = row.get(0)?;
            let incomplete: bool = row.get(1)?;
            let has_done: bool = row.get(2)?;
            let (start, end) = parse_span_payload(&raw)?;
            parsed.push((start, end, incomplete, has_done));
        }
        parsed
    };
    spans.sort_by_key(|&(s, e, _, _)| (s, e));

    let mut best_candidate: Option<u64> = None;
    for &(start, end, incomplete, has_done) in &spans {
        if incomplete {
            continue;
        }
        // Spans beyond `this_end` may advance the frontier only if they already have a completed
        // `'done'` extraction job and connect contiguously to `best_candidate`.
        if end > this_end && (!has_done || best_candidate.is_none_or(|prev| start > prev)) {
            continue;
        }
        let blocked = spans
            .iter()
            .any(|&(s_start, s_end, s_inc, _)| s_inc && (s_end <= end || s_start < end));
        if !blocked {
            best_candidate = Some(best_candidate.map_or(end, |prev| prev.max(end)));
        }
    }

    let updated_extracted = match best_candidate {
        Some(cand) => cur_extracted.max(cand.min(cur_byte)),
        None => cur_extracted,
    };
    let updated_i64 = i64::try_from(updated_extracted)
        .map_err(|_| KpError::invalid("last_extracted_offset", "offset exceeds i64"))?;
    if updated_i64 != cur_extracted_i64 {
        tx.execute(
            "UPDATE session_cursors SET last_extracted_offset = ?1 WHERE session_id = ?2",
            params![updated_i64, session_id],
        )?;
    }
    let more = cur_byte > updated_extracted;
    let any_incomplete_remaining = spans.iter().any(|&(_, _, inc, _)| inc);
    if !any_incomplete_remaining {
        crate::exchange::sync_session_cursor_meta_after_advance_tx(
            tx,
            &session_id,
            more,
            activity_ms,
        )?;
    }
    Ok(Some((session_id, updated_extracted)))
}

fn parse_span_payload(raw: &str) -> Result<(u64, u64)> {
    let val: serde_json::Value = serde_json::from_str(raw)?;
    let start = val
        .get("start")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| KpError::invalid("session_source_span", "missing start offset"))?;
    let end = val
        .get("end")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| KpError::invalid("session_source_span", "missing end offset"))?;
    Ok((start, end))
}

fn read_cursor_row(row: &Row<'_>) -> rusqlite::Result<Result<SessionCursor>> {
    let session_id: String = row.get(0)?;
    let client_kind: String = row.get(1)?;
    let transcript_path: Option<String> = row.get(2)?;
    let last_byte: i64 = row.get(3)?;
    let last_line: i64 = row.get(4)?;
    let prefix_sha256: String = row.get(5)?;
    let last_activity: i64 = row.get(6)?;
    let last_extracted: i64 = row.get(7)?;
    Ok((|| {
        Ok(SessionCursor {
            session_id,
            client_kind,
            transcript_path,
            last_byte_offset: u64::try_from(last_byte)
                .map_err(|_| KpError::invalid("last_byte_offset", "negative cursor offset"))?,
            last_line_count: u64::try_from(last_line)
                .map_err(|_| KpError::invalid("last_line_count", "negative line count"))?,
            prefix_sha256,
            last_activity_at: EpochMs(last_activity),
            last_extracted_offset: u64::try_from(last_extracted)
                .map_err(|_| KpError::invalid("last_extracted_offset", "negative cursor offset"))?,
        })
    })())
}

fn read_job_row(row: &Row<'_>) -> rusqlite::Result<Result<ExtractionJob>> {
    let job_id: String = row.get(0)?;
    let source_id: String = row.get(1)?;
    let chunk_ids_json: String = row.get(2)?;
    let batch_hash: String = row.get(3)?;
    let author: String = row.get(4)?;
    let model_policy: String = row.get(5)?;
    let priority: i32 = row.get(6)?;
    let reference_time: i64 = row.get(7)?;
    let status: String = row.get(8)?;
    let lease_until: Option<i64> = row.get(9)?;
    let attempts: u32 = row.get(10)?;
    let last_error: Option<String> = row.get(11)?;
    let created_at: i64 = row.get(12)?;
    let thread_key: Option<String> = row.get(13)?;
    let reply_to_key: Option<String> = row.get(14)?;
    Ok((|| {
        Ok(ExtractionJob {
            job_id,
            source_id: source_id.parse()?,
            chunk_ids: serde_json::from_str(&chunk_ids_json)?,
            chunk_batch_sha256: batch_hash.parse()?,
            author: author.parse()?,
            model_policy,
            priority,
            reference_time: EpochMs(reference_time),
            status: status.parse()?,
            lease_until: lease_until.map(EpochMs),
            attempts,
            last_error,
            created_at: EpochMs(created_at),
            thread_key,
            reply_to_key,
        })
    })())
}

#[cfg(test)]
#[path = "queue_tests.rs"]
mod tests;
