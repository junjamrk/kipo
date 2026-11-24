use std::fmt;
use std::str::FromStr;

use kp_model::{
    is_bare_content_hash, ActorId, CommunityConcept, ConceptId, ContentHash, EpochMs, Fact, FactId,
    Justification, KpError, SourceId,
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
    pub job_id: &'a str,
    pub expected_attempts: u32,
    pub lease_now: EpochMs,
    pub batch: &'a StagedExtractionBatch,
    pub chunk_hashes: &'a [ContentHash],
    pub author: Option<&'a ActorId>,
    pub schema_ver: &'a str,
    pub at: EpochMs,
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
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<ExtractionJob> = tx
            .query_row(
                "SELECT job_id, source_id, chunk_ids_json, chunk_batch_sha256, author_actor,
                        model_policy, priority, reference_time, status, lease_until, attempts,
                        last_error, created_at
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
                 priority, reference_time, status, lease_until, attempts, last_error, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', NULL, 0, NULL, ?9)",
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
                    last_error, created_at
             FROM extraction_queue
             WHERE job_id = ?1",
            [job_id],
            read_job_row,
        )??;
        tx.commit()?;
        Ok(Some(leased))
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

    /// Atomically verifies that `job_id` is still actively leased under `expected_attempts` at `lease_now`
    /// (`status = 'leased' AND attempts = expected_attempts AND (lease_until IS NULL OR lease_until > lease_now)`),
    /// applies all staged fact/supersede/derivation/concept writes, marks `extracted_chunks`, transitions
    /// the job to `'done'`, and advances any `session_source_span` in ONE `TransactionBehavior::Immediate`
    /// transaction — rolling back 100% of writes on `Err(StoreError::Conflict)`.
    pub fn commit_staged_extraction_job_fenced(
        &mut self,
        spec: &FencedJobCommitSpec<'_>,
    ) -> Result<()> {
        let FencedJobCommitSpec {
            job_id,
            expected_attempts,
            lease_now,
            batch,
            chunk_hashes,
            author,
            schema_ver,
            at,
        } = *spec;
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
        if crate::facts::is_source_marked_raw_only(&tx, &source_id_str)? {
            tx.execute(
                "UPDATE extraction_queue
                 SET status = 'done', lease_until = NULL, last_error = 'cancelled_raw_only'
                 WHERE job_id = ?1",
                [job_id],
            )?;
            let source_id: SourceId = source_id_str.parse()?;
            let _ = complete_session_source_span_tx(&tx, &source_id)?;
            tx.commit()?;
            return Err(StoreError::Conflict);
        }
        let changed = tx.execute(
            "UPDATE extraction_queue
             SET status = 'done', lease_until = NULL, last_error = NULL
             WHERE job_id = ?1
               AND status = 'leased'
               AND attempts = ?2
               AND (lease_until IS NULL OR lease_until > ?3)",
            params![job_id, expected_attempts, lease_now.0],
        )?;
        if changed == 0 {
            return Err(StoreError::Conflict);
        }
        apply_staged_extraction_batch_tx(&tx, batch, &self.tokenizer, self.embedder.as_ref())?;
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
        post_commit_staged_extraction_effects(self, batch)?;
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
                        last_error, created_at
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
    let existing: Option<ExtractionJob> = tx
        .query_row(
            "SELECT job_id, source_id, chunk_ids_json, chunk_batch_sha256, author_actor,
                    model_policy, priority, reference_time, status, lease_until, attempts,
                    last_error, created_at
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
                     priority = ?1
                 WHERE job_id = ?2",
                params![input.priority, &job.job_id],
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
             priority, reference_time, status, lease_until, attempts, last_error, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', NULL, 0, NULL, ?9)",
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
    })
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
        })
    })())
}

#[cfg(test)]
#[path = "queue_tests.rs"]
mod tests;
