use std::collections::{BTreeMap, BTreeSet};

use kp_model::{
    format_opaque_id, is_bare_content_hash, verify_verbatim_quote, ActorId, AnchorSpec,
    AudienceSpec, CharSpan, CommunityConcept, ConceptId, ConceptKind, ConceptPolarity, ContentHash,
    DerivationView, EpochMs, Fact, FactAccess, FactId, FactKind, FactStatus, GovernedScope,
    JustificationId, KpError, MemberRole, MembershipRelation, QuoteLocationHint,
    RejectionEvidenceKey, RejectionId, RejectionRecord, RejectionSemanticKey, ReviewId, ReviewItem,
    ReviewKind, ReviewStatus, SourceId, SourceScopeId,
};
use rusqlite::{params, Connection, OptionalExtension, Row, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::facts::{read_fact, read_graph, write_fact_indexed};
use crate::maintenance::invalidate;
use crate::{Result, Store, StoreError};

/// Allocates an opaque prefixed `FactId` (`fact_<uuidv7>`), never exposing a content hash.
pub fn new_fact_id() -> Result<FactId> {
    Ok(format_opaque_id("fact", &Uuid::now_v7().to_string())?.parse()?)
}

/// Allocates an opaque prefixed `SourceId` (`src_<uuidv7>`), never exposing a content hash.
pub fn new_source_id() -> Result<SourceId> {
    Ok(format_opaque_id("src", &Uuid::now_v7().to_string())?.parse()?)
}

/// Allocates an opaque prefixed `ReviewId` (`rev_<uuidv7>`), never exposing a content hash.
pub fn new_review_id() -> Result<ReviewId> {
    Ok(format_opaque_id("rev", &Uuid::now_v7().to_string())?.parse()?)
}

/// Allocates an opaque prefixed `RejectionId` (`rej_<uuidv7>`), never exposing a content hash.
pub fn new_rejection_id() -> Result<RejectionId> {
    Ok(format_opaque_id("rej", &Uuid::now_v7().to_string())?.parse()?)
}

/// Allocates an opaque prefixed `ConceptId` (`cpt_<uuidv7>`), never exposing a content hash.
pub fn new_concept_id() -> Result<ConceptId> {
    Ok(format_opaque_id("cpt", &Uuid::now_v7().to_string())?.parse()?)
}

/// Allocates an opaque prefixed `JustificationId` (`jst_<uuidv7>`), never exposing a content hash.
pub fn new_justification_id() -> Result<JustificationId> {
    Ok(format_opaque_id("jst", &Uuid::now_v7().to_string())?.parse()?)
}

/// Allocates an opaque prefixed queue job identifier (`job_<uuidv7>`).
pub fn new_job_id() -> Result<String> {
    Ok(format_opaque_id("job", &Uuid::now_v7().to_string())?)
}

/// Normalizes a concept label across fullwidth/halfwidth ASCII, whitespace runs, and ASCII case.
pub fn normalize_concept_label(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut prev_space = true;
    for ch in label.trim().chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
            continue;
        }
        prev_space = false;
        let folded = if ('\u{FF01}'..='\u{FF5E}').contains(&ch) {
            char::from_u32((ch as u32) - 0xFEE0).unwrap_or(ch)
        } else {
            ch
        };
        for lower in folded.to_lowercase() {
            out.push(lower);
        }
    }
    out
}

fn reject_bare_hash(field: &'static str, id: &str) -> Result<()> {
    if is_bare_content_hash(id) {
        return Err(KpError::invalid(
            field,
            "external identifier must be a prefixed opaque ID, not a bare 64-hex digest",
        )
        .into());
    }
    Ok(())
}

/// Optional keys and replacement supplied when rejecting a wrong claim (`kp reject` / writeback).
#[derive(Clone, Debug, Default)]
pub struct RejectOptions<'a> {
    pub evidence_key: Option<RejectionEvidenceKey>,
    pub semantic_key: Option<RejectionSemanticKey>,
    pub replacement: Option<&'a Fact>,
}

/// Single-use time-bounded authorization grant recorded by `kp allow` (`allow_guard_once`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardAllowGrant {
    pub fact_id: FactId,
    pub kp_ref: String,
    pub allowed_by: ActorId,
    pub single_use: bool,
    pub created_at: EpochMs,
    pub expires_at: EpochMs,
}

/// Source visibility, scope, URI, media type, and ordered `(chunk_id, heading, body, base_anchor)` slices for queued extraction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExtractionSourceContext {
    pub readable_by: AudienceSpec,
    pub source_scope_id: Option<SourceScopeId>,
    pub uri: String,
    pub media_type: String,
    pub chunks: Vec<(String, String, String, AnchorSpec)>,
}

/// Atomic fact & audit updates applied when resolving a governance review item.
#[derive(Clone, Debug, Default)]
pub struct ScopeReviewFactUpdates<'a> {
    pub replacement_fact_id: Option<&'a FactId>,
    pub widened_audience: Option<&'a AudienceSpec>,
    pub copied_entities: Option<&'a BTreeSet<String>>,
    pub clear_broader_scope: Option<(&'a FactId, &'a ActorId)>,
    pub change_log_entry: Option<(&'a ReviewId, &'a ActorId, Value, EpochMs)>,
}

impl Store {
    /// Inserts or updates an active `CommunityConcept` in `community_concepts` and marks it as an
    /// explicitly seeded vocabulary concept (`op = 'concept_seed'`).
    pub fn upsert_community_concept(&mut self, concept: &CommunityConcept) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        upsert_community_concept_tx(&tx, concept)?;
        tx.execute(
            "INSERT INTO change_log (op, target_id, created_at)
             SELECT 'concept_seed', ?1, ?2
             WHERE NOT EXISTS (
                 SELECT 1 FROM change_log WHERE op = 'concept_seed' AND target_id = ?1
             )",
            params![concept.id.as_str(), concept.created_at.0],
        )?;
        tx.commit()?;
        self.tokenizer.add_entity_word(&concept.canonical_label);
        Ok(())
    }

    /// Returns the set of [`ConceptId`]s explicitly registered via [`Store::upsert_community_concept`]
    /// (as opposed to model-extracted concepts whose provenance derives solely from anchoring facts).
    pub fn explicitly_seeded_concept_ids(&self) -> Result<BTreeSet<ConceptId>> {
        let mut stmt = self
            .db
            .prepare("SELECT DISTINCT target_id FROM change_log WHERE op = 'concept_seed' AND target_id IS NOT NULL")?;
        let mut rows = stmt.query([])?;
        let mut out = BTreeSet::new();
        while let Some(row) = rows.next()? {
            if let Ok(cid) = row.get::<_, String>(0)?.parse::<ConceptId>() {
                out.insert(cid);
            }
        }
        Ok(out)
    }

    /// Looks up an active `CommunityConcept` by normalized `(kind, label)` or allocates a new `cpt_<uuidv7>` entry.
    pub fn ensure_community_concept(
        &mut self,
        kind: ConceptKind,
        canonical_label: &str,
        domain: &str,
        polarity: ConceptPolarity,
        proposed_by: &ActorId,
        at: EpochMs,
    ) -> Result<CommunityConcept> {
        let normalized = normalize_concept_label(canonical_label);
        if normalized.is_empty() {
            return Err(
                KpError::invalid("canonical_label", "concept label must be nonempty").into(),
            );
        }
        if let Some(existing) = self.find_concept_by_label(kind, canonical_label)? {
            return Ok(existing);
        }
        let concept = CommunityConcept {
            id: new_concept_id()?,
            kind,
            canonical_label: canonical_label.trim().to_owned(),
            domain: domain.trim().to_owned(),
            polarity,
            proposed_by: proposed_by.clone(),
            created_at: at,
        };
        self.upsert_community_concept(&concept)?;
        Ok(concept)
    }

    /// Fetches an active `CommunityConcept` by its `ConceptId`.
    pub fn get_community_concept(&self, id: &ConceptId) -> Result<Option<CommunityConcept>> {
        self.db
            .query_row(
                "SELECT concept_id, concept_kind, canonical_label, domain, polarity, proposed_by, created_at
                 FROM community_concepts
                 WHERE concept_id = ?1 AND status = 'active'",
                [id.as_str()],
                read_concept_row,
            )
            .optional()?
            .transpose()
    }

    /// Lists all active `CommunityConcept` entries ordered deterministically by `concept_id`.
    pub fn list_active_concepts(&self) -> Result<Vec<CommunityConcept>> {
        let mut stmt = self.db.prepare(
            "SELECT concept_id, concept_kind, canonical_label, domain, polarity, proposed_by, created_at
             FROM community_concepts
             WHERE status = 'active'
             ORDER BY concept_id",
        )?;
        let rows = stmt.query_map([], read_concept_row)?;
        rows.map(|row| row?)
            .collect::<Result<Vec<CommunityConcept>>>()
    }

    /// Looks up an active `CommunityConcept` matching normalized `(kind, canonical_label)`.
    pub fn find_concept_by_label(
        &self,
        kind: ConceptKind,
        label: &str,
    ) -> Result<Option<CommunityConcept>> {
        let target = normalize_concept_label(label);
        if target.is_empty() {
            return Ok(None);
        }
        let mut stmt = self.db.prepare(
            "SELECT concept_id, concept_kind, canonical_label, domain, polarity, proposed_by, created_at
             FROM community_concepts
             WHERE concept_kind = ?1 AND status = 'active'
             ORDER BY created_at ASC, concept_id ASC",
        )?;
        let mut rows = stmt.query([kind.as_str()])?;
        while let Some(row) = rows.next()? {
            let concept = read_concept_row(row)??;
            if normalize_concept_label(&concept.canonical_label) == target {
                return Ok(Some(concept));
            }
        }
        Ok(None)
    }

    /// Marks an existing `CommunityConcept` as `deprecated`.
    pub fn deprecate_community_concept(&mut self, id: &ConceptId) -> Result<bool> {
        let changed = self.db.execute(
            "UPDATE community_concepts SET status = 'deprecated'
             WHERE concept_id = ?1 AND status = 'active'",
            [id.as_str()],
        )?;
        Ok(changed > 0)
    }

    /// Atomically withdraws `target` and persists a dual-key `RejectionRecord` into `rejected_claims`.
    pub fn reject_fact(
        &mut self,
        target: &FactId,
        rejected_by: &ActorId,
        reason: &str,
        at: EpochMs,
    ) -> Result<RejectionRecord> {
        self.reject_fact_with_options(target, rejected_by, reason, at, RejectOptions::default())
    }

    /// Atomically withdraws (or supersedes when `options.replacement` is set) `target` AND records
    /// a durable dual-key `RejectionRecord` in `rejected_claims` so re-extraction never resurrects it.
    pub fn reject_fact_with_options(
        &mut self,
        target: &FactId,
        rejected_by: &ActorId,
        reason: &str,
        at: EpochMs,
        options: RejectOptions<'_>,
    ) -> Result<RejectionRecord> {
        let trimmed_reason = reason.trim();
        if trimmed_reason.is_empty() {
            return Err(KpError::invalid("reason", "rejection reason must be nonempty").into());
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let fact = tx
            .query_row(
                "SELECT * FROM facts WHERE fact_id = ?1",
                [target.as_str()],
                |row| Ok(read_fact(row)),
            )
            .optional()?
            .transpose()?
            .ok_or(StoreError::NotFound)?;

        let replacement_fact_id = if let Some(replacement) = options.replacement {
            reject_bare_hash("replacement_fact_id", replacement.id.as_str())?;
            let opaque = crate::tombstone_id(
                replacement.origin_library.as_str().as_bytes(),
                &replacement.origin_library,
                &replacement.logical_key,
            )?;
            write_fact_indexed(
                &tx,
                replacement,
                &opaque,
                &self.tokenizer,
                self.embedder.as_ref(),
            )?;
            let valid_from = match &replacement.access {
                FactAccess::Base { stamp, .. } => Some(stamp.valid.valid_from.0),
                FactAccess::Derived { .. } => None,
            };
            let changed = tx.execute(
                "UPDATE facts SET status = 'superseded', superseded_by = ?1,
                 valid_until = CASE WHEN valid_from < ?3 THEN ?3 ELSE valid_until END
                 WHERE fact_id = ?2 AND status = 'active'",
                params![replacement.id.as_str(), target.as_str(), valid_from],
            )?;
            if changed == 0 {
                return Err(StoreError::Conflict);
            }
            invalidate(&tx, target, replacement.access.recorded_at(), "superseded")?;
            crate::exchange::cancel_publishes_and_queue_withdrawals(&tx, target)?;
            Some(replacement.id.clone())
        } else {
            let changed = tx.execute(
                "UPDATE facts SET status = 'withdrawn', expired_at = ?1
                 WHERE fact_id = ?2 AND expired_at IS NULL",
                params![at.0, target.as_str()],
            )?;
            crate::exchange::cancel_publishes_and_queue_withdrawals(&tx, target)?;
            if changed > 0 {
                invalidate(&tx, target, at, "rejected")?;
                tx.execute(
                    "INSERT INTO change_log (op, target_id, origin_library_id, logical_key, actor_id, payload_json, created_at)
                     SELECT 'reject', fact_id, origin_library_id, logical_key, ?2, ?3, ?4
                     FROM facts WHERE fact_id = ?1",
                    params![
                        target.as_str(),
                        rejected_by.as_str(),
                        serde_json::to_string(&trimmed_reason)?,
                        at.0
                    ],
                )?;
            }
            None
        };

        let evidence_key = match options.evidence_key {
            Some(ev) => Some(ev),
            None => derive_fact_evidence_key(&tx, &fact)?,
        };
        let semantic_key = match options.semantic_key {
            Some(sem) => Some(sem),
            None => derive_fact_semantic_key(&fact),
        };

        let record = RejectionRecord {
            id: new_rejection_id()?,
            rejected_by: rejected_by.clone(),
            evidence_key,
            semantic_key,
            replacement_fact_id,
            reason: trimmed_reason.to_owned(),
            created_at: at,
        };

        if record.evidence_key.is_some() || record.semantic_key.is_some() {
            write_rejection_record(&tx, &record)?;
        } else {
            // Manual facts without source chunk spans or concept IDs still persist an exact digest
            // marker satisfying SQLite's non-null key CHECK without false-positiving unrelated claims.
            let fallback_fp = format!("fact_sha256:{}", fact.strong_ref().content_sha256);
            tx.execute(
                "INSERT INTO rejected_claims
                    (rejection_id, rejected_by_actor, speaker_actor, chunk_sha256, char_start, char_end,
                     semantic_fingerprint, semantic_key_json, replacement_fact_id, reason, created_at)
                 VALUES (?1, ?2, ?3, NULL, NULL, NULL, ?4, NULL, ?5, ?6, ?7)",
                params![
                    record.id.as_str(),
                    record.rejected_by.as_str(),
                    fact.author.as_str(),
                    fallback_fp,
                    record.replacement_fact_id.as_ref().map(FactId::as_str),
                    record.reason,
                    record.created_at.0,
                ],
            )?;
        }

        let fact_sref = fact.strong_ref();
        crate::exchange::record_withdrawn_version_tx(
            &tx,
            &fact_sref.origin_library,
            &fact_sref.logical_key,
            &fact_sref.content_sha256,
            rejected_by,
            at,
        )?;
        crate::exchange::mark_alignment_dirty_tx(&tx, at)?;
        tx.commit()?;
        self.sync_skills_now()?;
        Ok(record)
    }

    /// Persists a validated `RejectionRecord` directly into `rejected_claims`.
    pub fn insert_rejection(&mut self, record: &RejectionRecord) -> Result<()> {
        reject_bare_hash("rejection_id", record.id.as_str())?;
        record.validate()?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        write_rejection_record(&tx, record)?;
        tx.commit()?;
        Ok(())
    }

    /// Lists stored `RejectionRecord`s (optionally filtered by `rejected_by`).
    pub fn list_rejections(&self, rejected_by: Option<&ActorId>) -> Result<Vec<RejectionRecord>> {
        let mut records = Vec::new();
        let sql = if rejected_by.is_some() {
            "SELECT rejection_id, rejected_by_actor, speaker_actor, chunk_sha256, char_start, char_end,
                    semantic_key_json, replacement_fact_id, reason, created_at
             FROM rejected_claims
             WHERE rejected_by_actor = ?1
             ORDER BY created_at DESC, rejection_id ASC"
        } else {
            "SELECT rejection_id, rejected_by_actor, speaker_actor, chunk_sha256, char_start, char_end,
                    semantic_key_json, replacement_fact_id, reason, created_at
             FROM rejected_claims
             ORDER BY created_at DESC, rejection_id ASC"
        };
        let mut stmt = self.db.prepare(sql)?;
        let mut rows = if let Some(actor) = rejected_by {
            stmt.query([actor.as_str()])?
        } else {
            stmt.query([])?
        };
        while let Some(row) = rows.next()? {
            records.push(read_rejection_row(row)?);
        }
        Ok(records)
    }

    /// Dual-key sticky rejection filter: returns `true` if EITHER `(speaker, chunk_sha256)` has a
    /// rejected `[char_start, char_end)` span overlapping `span` OR `semantic_key` matches a
    /// rejected `semantic_fingerprint`.
    pub fn is_candidate_rejected(
        &self,
        speaker: &ActorId,
        chunk_sha256: &ContentHash,
        span: impl Into<Option<CharSpan>>,
        semantic_key: Option<&RejectionSemanticKey>,
    ) -> Result<bool> {
        if let Some(span) = span.into() {
            CharSpan::new(span.char_start, span.char_end)?;
            let evidence_hit: bool = self.db.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM rejected_claims
                    WHERE speaker_actor = ?1
                      AND chunk_sha256 = ?2
                      AND char_start IS NOT NULL
                      AND char_end IS NOT NULL
                      AND char_start < ?3
                      AND ?4 < char_end
                )",
                params![
                    speaker.as_str(),
                    chunk_sha256.as_str(),
                    i64::from(span.char_end),
                    i64::from(span.char_start),
                ],
                |row| row.get(0),
            )?;
            if evidence_hit {
                return Ok(true);
            }
        }

        if let Some(sem_key) = semantic_key {
            let fp = sem_key.fingerprint();
            let semantic_hit: bool = self.db.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM rejected_claims
                    WHERE semantic_fingerprint = ?1
                )",
                [fp.as_str()],
                |row| row.get(0),
            )?;
            if semantic_hit {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Idempotently upserts an `open` review item keyed by `(actor_id, kind, target_id)`.
    /// If an open review item already exists for that triple, updates its question/options/payload
    /// in place while preserving its existing `ReviewId` and `created_at`.
    pub fn upsert_open_review_item(&mut self, item: &ReviewItem) -> Result<ReviewItem> {
        validate_review_item_fields(item)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stored = upsert_open_review_item_tx(&tx, item)?;
        tx.commit()?;
        Ok(stored)
    }

    /// Atomically checks [`is_open_review_currently_valid`] inside a `TransactionBehavior::Immediate`
    /// transaction before inserting or updating `item`, returning `Ok(None)` without writing if any
    /// supporting premise was retracted, expired, or lost live visibility to `item.actor` mid-flight.
    pub fn upsert_open_review_item_if_valid(
        &mut self,
        item: &ReviewItem,
    ) -> Result<Option<ReviewItem>> {
        validate_review_item_fields(item)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let graph = read_graph(&tx)?;
        if !is_open_review_currently_valid(&graph, item) {
            tx.commit()?;
            return Ok(None);
        }
        let stored = upsert_open_review_item_tx(&tx, item)?;
        tx.commit()?;
        Ok(Some(stored))
    }

    /// Fetches a single `ReviewItem` by `ReviewId`, returning `Ok(None)` if an `Open` item is no
    /// longer live/visible under [`is_open_review_currently_valid`].
    pub fn get_review_item(&self, review_id: &ReviewId) -> Result<Option<ReviewItem>> {
        let item = self
            .db
            .query_row(
                "SELECT review_id, actor_id, kind, target_id, question, options_json, payload_json, status, created_at, resolved_at
                 FROM review_items
                 WHERE review_id = ?1",
                [review_id.as_str()],
                read_review_row,
            )
            .optional()?
            .transpose()?;
        let Some(item) = item else {
            return Ok(None);
        };
        if item.status == ReviewStatus::Open {
            let graph = read_graph(&self.db)?;
            if !is_open_review_currently_valid(&graph, &item) {
                return Ok(None);
            }
        }
        Ok(Some(item))
    }

    /// Lists review items belonging strictly to `actor`, optionally filtered by `status_filter`,
    /// suppressing `Open` items whose premises are no longer active and live-visible to `actor`.
    pub fn list_review_items(
        &self,
        actor: &ActorId,
        status_filter: Option<ReviewStatus>,
    ) -> Result<Vec<ReviewItem>> {
        let mut items = Vec::new();
        if let Some(status) = status_filter {
            let mut stmt = self.db.prepare(
                "SELECT review_id, actor_id, kind, target_id, question, options_json, payload_json, status, created_at, resolved_at
                 FROM review_items
                 WHERE actor_id = ?1 AND status = ?2
                 ORDER BY created_at DESC, review_id ASC",
            )?;
            let mut rows = stmt.query(params![actor.as_str(), status.as_str()])?;
            while let Some(row) = rows.next()? {
                items.push(read_review_row(row)??);
            }
        } else {
            let mut stmt = self.db.prepare(
                "SELECT review_id, actor_id, kind, target_id, question, options_json, payload_json, status, created_at, resolved_at
                 FROM review_items
                 WHERE actor_id = ?1
                 ORDER BY created_at DESC, review_id ASC",
            )?;
            let mut rows = stmt.query([actor.as_str()])?;
            while let Some(row) = rows.next()? {
                items.push(read_review_row(row)??);
            }
        }
        if items.iter().any(|it| it.status == ReviewStatus::Open) {
            let graph = read_graph(&self.db)?;
            items.retain(|it| is_open_review_currently_valid(&graph, it));
        }
        Ok(items)
    }

    /// Resolves a review item (`Confirmed` or `Dismissed`), enforcing that only its owning `actor`
    /// may resolve it and returning `NotFound` if an `Open` item's premises are retracted or hidden.
    pub fn resolve_review_item(
        &mut self,
        review_id: &ReviewId,
        actor: &ActorId,
        status: ReviewStatus,
        at: EpochMs,
    ) -> Result<ReviewItem> {
        if status == ReviewStatus::Open {
            return Err(KpError::invalid(
                "review_status",
                "resolving a review item requires Confirmed or Dismissed status",
            )
            .into());
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = tx
            .query_row(
                "SELECT review_id, actor_id, kind, target_id, question, options_json, payload_json, status, created_at, resolved_at
                 FROM review_items
                 WHERE review_id = ?1",
                [review_id.as_str()],
                read_review_row,
            )
            .optional()?
            .transpose()?
            .ok_or(StoreError::NotFound)?;

        if existing.actor != *actor {
            return Err(StoreError::PermissionDenied);
        }
        if existing.status == ReviewStatus::Open {
            let graph = read_graph(&tx)?;
            if !is_open_review_resolvable_in_tx(&tx, &graph, &existing)? {
                return Err(StoreError::NotFound);
            }
        }

        tx.execute(
            "UPDATE review_items SET status = ?1, resolved_at = ?2 WHERE review_id = ?3",
            params![status.as_str(), at.0, review_id.as_str()],
        )?;
        tx.execute(
            "INSERT INTO change_log (op, target_id, actor_id, payload_json, created_at)
             VALUES ('resolve_review', ?1, ?2, ?3, ?4)",
            params![
                review_id.as_str(),
                actor.as_str(),
                serde_json::to_string(&status)?,
                at.0
            ],
        )?;
        tx.commit()?;

        Ok(ReviewItem {
            status,
            resolved_at: Some(at),
            ..existing
        })
    }

    /// Issues a single-use time-bounded `allow_guard_once` grant for `target` in `change_log`.
    /// Requires `actor` to hold `Owner` or `Maintainer` role and `target` to be visible to `(actor, at)`.
    pub fn grant_guard_allow_once(
        &mut self,
        actor: &ActorId,
        target: &FactId,
        ttl_ms: i64,
        at: EpochMs,
    ) -> Result<GuardAllowGrant> {
        self.grant_guard_allow_once_many(actor, std::slice::from_ref(target), ttl_ms, at)?
            .into_iter()
            .next()
            .ok_or(StoreError::NotFound)
    }

    /// Atomically issues single-use time-bounded `allow_guard_once` grants for all `targets` in one
    /// `TransactionBehavior::Immediate` transaction after validating `ttl_ms > 0`, `targets` non-empty,
    /// `Owner | Maintainer` role, and visibility of EVERY target to `(actor, at)`.
    /// If any target is missing or invisible, zero grants are inserted.
    pub fn grant_guard_allow_once_many(
        &mut self,
        actor: &ActorId,
        targets: &[FactId],
        ttl_ms: i64,
        at: EpochMs,
    ) -> Result<Vec<GuardAllowGrant>> {
        if ttl_ms <= 0 {
            return Err(KpError::invalid("ttl_ms", "ttl_ms must be positive").into());
        }
        if targets.is_empty() {
            return Err(KpError::invalid("targets", "targets must be nonempty").into());
        }
        let expires_at =
            at.0.checked_add(ttl_ms)
                .ok_or_else(|| KpError::invalid("at", "timestamp overflow"))?;

        let mut unique_targets = Vec::with_capacity(targets.len());
        let mut seen = BTreeSet::new();
        for target in targets {
            if seen.insert(target.clone()) {
                unique_targets.push(target.clone());
            }
        }

        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let role_str: Option<String> = tx
            .query_row(
                "SELECT role FROM members_tokens WHERE actor_id = ?1 AND revoked_at IS NULL",
                [actor.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        let Some(role_str) = role_str else {
            return Err(StoreError::PermissionDenied);
        };
        let role: MemberRole = role_str.parse()?;
        if !matches!(role, MemberRole::Owner | MemberRole::Maintainer) {
            return Err(StoreError::PermissionDenied);
        }

        let visible =
            crate::snapshot::filter_visible_fact_ids_in_tx(&tx, actor, at, &unique_targets)?;
        if unique_targets.iter().any(|t| !visible.contains(t)) {
            return Err(StoreError::NotFound);
        }

        let payload_str = serde_json::to_string(&json!({
            "expires_at": expires_at,
            "consumed": false,
            "single_use": true,
        }))?;

        let mut grants = Vec::with_capacity(unique_targets.len());
        for target in unique_targets {
            tx.execute(
                "INSERT INTO change_log (op, target_id, actor_id, payload_json, created_at)
                 VALUES ('allow_guard_once', ?1, ?2, ?3, ?4)",
                params![target.as_str(), actor.as_str(), payload_str, at.0],
            )?;
            let kp_ref = if target.as_str().starts_with("fact_") {
                target.to_string()
            } else {
                format!("fact_{target}")
            };
            grants.push(GuardAllowGrant {
                fact_id: target,
                kp_ref,
                allowed_by: actor.clone(),
                single_use: true,
                created_at: at,
                expires_at: EpochMs(expires_at),
            });
        }
        tx.commit()?;
        Ok(grants)
    }

    /// Atomically consumes the earliest unconsumed, unexpired `allow_guard_once` grant for `target` at `at`.
    pub fn consume_guard_allow_once(&mut self, target: &FactId, at: EpochMs) -> Result<bool> {
        Ok(self
            .try_consume_all_guard_allows(std::slice::from_ref(target), at)?
            .is_empty())
    }

    /// All-or-nothing multi-guard consumption: in a single `TransactionBehavior::Immediate` transaction,
    /// checks deduplicated `targets` (preserving order) for an unconsumed, unexpired `allow_guard_once`
    /// grant (`created_at <= at.0 && at.0 < expires_at`).
    ///
    /// - If ANY target lacks a valid grant, **consumes ZERO grants** and returns `Ok(missing)`.
    /// - Only when every target has a valid grant, marks all matched `seq` rows consumed atomically
    ///   and returns `Ok(Vec::new())`.
    pub fn try_consume_all_guard_allows(
        &mut self,
        targets: &[FactId],
        at: EpochMs,
    ) -> Result<Vec<FactId>> {
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let mut unique_targets = Vec::with_capacity(targets.len());
        let mut seen = BTreeSet::new();
        for target in targets {
            if seen.insert(target.clone()) {
                unique_targets.push(target.clone());
            }
        }

        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut matched_grants = Vec::<(i64, Value)>::with_capacity(unique_targets.len());
        let mut used_seqs = BTreeSet::<i64>::new();
        let mut missing = Vec::<FactId>::new();

        {
            let mut stmt = tx.prepare(
                "SELECT seq, payload_json, created_at FROM change_log
                 WHERE op = 'allow_guard_once' AND (target_id = ?1 OR target_id = ?2)
                 ORDER BY seq ASC",
            )?;
            for target in unique_targets {
                let bare = target
                    .as_str()
                    .strip_prefix("fact_")
                    .unwrap_or(target.as_str());
                let prefixed = format!("fact_{bare}");
                let mut rows = stmt.query(params![bare, prefixed])?;
                let mut found: Option<(i64, Value)> = None;
                while let Some(row) = rows.next()? {
                    let seq: i64 = row.get(0)?;
                    if used_seqs.contains(&seq) {
                        continue;
                    }
                    let payload_str: String = row.get(1)?;
                    let created_at: i64 = row.get(2)?;
                    if created_at > at.0 {
                        continue;
                    }
                    let Ok(val) = serde_json::from_str::<Value>(&payload_str) else {
                        continue;
                    };
                    let consumed = val
                        .get("consumed")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let expires_at = val.get("expires_at").and_then(Value::as_i64).unwrap_or(0);
                    if !consumed && at.0 < expires_at {
                        found = Some((seq, val));
                        break;
                    }
                }
                if let Some((seq, val)) = found {
                    used_seqs.insert(seq);
                    matched_grants.push((seq, val));
                } else {
                    missing.push(target);
                }
            }
        }

        if !missing.is_empty() {
            tx.commit()?;
            return Ok(missing);
        }

        for (seq, mut payload) in matched_grants {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("consumed".to_owned(), Value::Bool(true));
                obj.insert("consumed_at".to_owned(), json!(at.0));
            }
            tx.execute(
                "UPDATE change_log SET payload_json = ?1 WHERE seq = ?2",
                params![serde_json::to_string(&payload)?, seq],
            )?;
        }
        tx.commit()?;
        Ok(Vec::new())
    }

    /// Lists all open `ReviewItem`s across actors ordered deterministically by `(created_at, review_id)`,
    /// filtered by [`is_open_review_currently_valid`].
    pub fn list_all_open_review_items(&self) -> Result<Vec<ReviewItem>> {
        let mut stmt = self.db.prepare(
            "SELECT review_id, actor_id, kind, target_id, question, options_json, payload_json, status, created_at, resolved_at
             FROM review_items
             WHERE status = 'open'
             ORDER BY created_at ASC, review_id ASC",
        )?;
        let mut rows = stmt.query([])?;
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            items.push(read_review_row(row)??);
        }
        if !items.is_empty() {
            let graph = read_graph(&self.db)?;
            items.retain(|it| is_open_review_currently_valid(&graph, it));
        }
        Ok(items)
    }

    /// Loads `(readable_by, source_scope_id, uri, media_type)` and requested `(chunk_id, heading, body, base_anchor)` slices scoped strictly to `source_id`.
    pub fn extraction_source_context(
        &self,
        source_id: &SourceId,
        chunk_ids: &[String],
    ) -> Result<Option<ExtractionSourceContext>> {
        let row: Option<(String, Option<String>, String, String)> = self
            .db
            .query_row(
                "SELECT COALESCE(
                    (SELECT c.readable_by_json FROM chunks c WHERE c.source_id = s.source_id ORDER BY c.ordinal ASC LIMIT 1),
                    s.visibility_scope
                 ), s.source_scope_id, s.uri, s.media_type
                 FROM raw_sources s
                 WHERE s.source_id = ?1",
                [source_id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((aud_json, scope_opt, uri, media_type)) = row else {
            return Ok(None);
        };
        let readable_by: AudienceSpec = serde_json::from_str(&aud_json)?;
        let source_scope_id = scope_opt.map(|s| s.parse::<SourceScopeId>()).transpose()?;

        let mut chunks = Vec::with_capacity(chunk_ids.len());
        if !chunk_ids.is_empty() {
            let mut stmt = self.db.prepare(
                "SELECT chunk_id, heading, body, anchor_json
                 FROM chunks
                 WHERE source_id = ?1 AND chunk_id = ?2",
            )?;
            for cid in chunk_ids {
                if let Some((cid_val, heading, body, anchor_raw)) = stmt
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
                    let base_anchor: AnchorSpec = serde_json::from_str(&anchor_raw)?;
                    chunks.push((cid_val, heading, body, base_anchor));
                }
            }
        }

        Ok(Some(ExtractionSourceContext {
            readable_by,
            source_scope_id,
            uri,
            media_type,
            chunks,
        }))
    }

    /// Returns `raw_sources.media_type` for `source_id` if present.
    pub fn source_media_type(&self, source_id: &SourceId) -> Result<Option<String>> {
        Ok(self
            .db
            .query_row(
                "SELECT media_type FROM raw_sources WHERE source_id = ?1",
                [source_id.as_str()],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Applies post-review fact updates (widening `facts.audience_json`, copying `fact_entities`,
    /// clearing `broader_scope_json`, and/or recording `resolve_review` in `change_log`) atomically.
    pub fn apply_scope_review_fact_updates(
        &mut self,
        updates: &ScopeReviewFactUpdates<'_>,
    ) -> Result<()> {
        if let Some(AudienceSpec::SemiringDerived) = updates.widened_audience {
            return Err(KpError::invalid("audience", "base fact cannot use derived access").into());
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(fact_id) = updates.replacement_fact_id {
            if let Some(audience) = updates.widened_audience {
                tx.execute(
                    "UPDATE facts SET audience_json = ?1
                     WHERE fact_id = ?2 AND audience_json != '\"__semiring__\"'",
                    params![serde_json::to_string(audience)?, fact_id.as_str()],
                )?;
            }
            if let Some(entities) = updates.copied_entities {
                for entity in entities {
                    let clean = entity.trim().to_lowercase();
                    if !clean.is_empty() {
                        tx.execute(
                            "INSERT OR IGNORE INTO fact_entities VALUES (?1, ?2, 'mention')",
                            params![fact_id.as_str(), clean],
                        )?;
                    }
                }
            }
        }

        if let Some((fact_id, author)) = updates.clear_broader_scope {
            tx.execute(
                "UPDATE facts SET broader_scope_json = NULL WHERE fact_id = ?1 AND author_actor = ?2",
                params![fact_id.as_str(), author.as_str()],
            )?;
        }

        if let Some((review_id, actor, ref payload, at)) = updates.change_log_entry {
            tx.execute(
                "INSERT INTO change_log (op, target_id, actor_id, payload_json, created_at)
                 VALUES ('resolve_review', ?1, ?2, ?3, ?4)",
                params![
                    review_id.as_str(),
                    actor.as_str(),
                    serde_json::to_string(payload)?,
                    at.0,
                ],
            )?;
        }

        let align_at = updates
            .change_log_entry
            .as_ref()
            .map_or(EpochMs(0), |(_, _, _, at)| *at);
        crate::exchange::mark_alignment_dirty_tx(&tx, align_at)?;
        tx.commit()?;
        if let Some(entities) = updates.copied_entities {
            for entity in entities {
                self.tokenizer.add_entity_word(entity);
            }
        }
        Ok(())
    }

    /// Confirms (`Some(actor)`) or rejects (`None`) an external identity mapping in `external_identities`.
    pub fn resolve_external_identity_mapping(
        &mut self,
        provider: &str,
        external_id: &str,
        confirmed_actor: Option<&ActorId>,
        at: EpochMs,
    ) -> Result<bool> {
        let provider = provider.trim();
        let external_id = external_id.trim();
        if provider.is_empty() || external_id.is_empty() {
            return Err(KpError::invalid(
                "external_identity",
                "provider and external_id must be nonempty",
            )
            .into());
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = match confirmed_actor {
            Some(actor) => tx.execute(
                "UPDATE external_identities
                 SET confirmed_actor_id = ?1, status = 'confirmed', updated_at = ?2
                 WHERE provider = ?3 AND external_id = ?4",
                params![actor.as_str(), at.0, provider, external_id],
            )?,
            None => tx.execute(
                "UPDATE external_identities
                 SET confirmed_actor_id = NULL, status = 'rejected', updated_at = ?1
                 WHERE provider = ?2 AND external_id = ?3",
                params![at.0, provider, external_id],
            )?,
        };
        tx.commit()?;
        Ok(changed > 0)
    }

    /// Returns all confirmed `(provider, external_id) -> ActorId` mappings in `external_identities`.
    pub fn confirmed_external_identities(&self) -> Result<BTreeMap<(String, String), ActorId>> {
        let mut stmt = self.db.prepare(
            "SELECT provider, external_id, confirmed_actor_id
             FROM external_identities
             WHERE status = 'confirmed' AND confirmed_actor_id IS NOT NULL
             ORDER BY provider, external_id",
        )?;
        let mut rows = stmt.query([])?;
        let mut out = BTreeMap::new();
        while let Some(row) = rows.next()? {
            let provider: String = row.get(0)?;
            let external_id: String = row.get(1)?;
            let actor_raw: String = row.get(2)?;
            if let Ok(actor) = actor_raw.parse::<ActorId>() {
                out.insert((provider, external_id), actor);
            }
        }
        Ok(out)
    }

    /// Explicitly binds `(provider, external_id)` to an active library `confirmed_actor`.
    ///
    /// Requires `caller` to hold `Owner` or `Maintainer` privileges so ordinary members cannot
    /// spoof another external participant's identity.
    pub fn bind_external_identity(
        &mut self,
        caller: &ActorId,
        provider: &str,
        external_id: &str,
        confirmed_actor: &ActorId,
        display_name: Option<&str>,
        at: EpochMs,
    ) -> Result<()> {
        let clean_provider = provider.trim();
        let clean_external = external_id.trim();
        if clean_provider.is_empty()
            || clean_external.is_empty()
            || clean_provider.chars().any(char::is_control)
            || clean_external.chars().any(char::is_control)
        {
            return Err(KpError::invalid(
                "external_identity",
                "provider and external_id must be nonempty without control characters",
            )
            .into());
        }
        let clean_display = display_name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(clean_external);

        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let caller_role = crate::public_ingest::require_active_member_role_tx(&tx, caller)?;
        if !matches!(caller_role, MemberRole::Owner | MemberRole::Maintainer) {
            return Err(StoreError::PermissionDenied);
        }
        let _ = crate::public_ingest::require_active_member_role_tx(&tx, confirmed_actor)?;

        tx.execute(
            "INSERT INTO external_identities
                (provider, external_id, display_name, proposed_actor_id, confirmed_actor_id, status, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4, 'confirmed', ?5)
             ON CONFLICT(provider, external_id) DO UPDATE SET
                display_name = excluded.display_name,
                confirmed_actor_id = excluded.confirmed_actor_id,
                status = 'confirmed',
                updated_at = excluded.updated_at",
            params![
                clean_provider,
                clean_external,
                clean_display,
                confirmed_actor.as_str(),
                at.0,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
}

fn read_concept_row(row: &Row<'_>) -> rusqlite::Result<Result<CommunityConcept>> {
    let id: String = row.get(0)?;
    let kind: String = row.get(1)?;
    let canonical_label: String = row.get(2)?;
    let domain: String = row.get(3)?;
    let polarity: String = row.get(4)?;
    let proposed_by: String = row.get(5)?;
    let created_at: i64 = row.get(6)?;
    Ok((|| {
        Ok(CommunityConcept {
            id: id.parse()?,
            kind: kind.parse()?,
            canonical_label,
            domain,
            polarity: polarity.parse()?,
            proposed_by: proposed_by.parse()?,
            created_at: EpochMs(created_at),
        })
    })())
}

pub(crate) fn read_review_row(row: &Row<'_>) -> rusqlite::Result<Result<ReviewItem>> {
    let id: String = row.get(0)?;
    let actor: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let target_id: String = row.get(3)?;
    let question: String = row.get(4)?;
    let options_json: String = row.get(5)?;
    let payload_json: String = row.get(6)?;
    let status: String = row.get(7)?;
    let created_at: i64 = row.get(8)?;
    let resolved_at: Option<i64> = row.get(9)?;
    Ok((|| {
        Ok(ReviewItem {
            id: id.parse()?,
            actor: actor.parse()?,
            kind: kind.parse::<ReviewKind>()?,
            target_id,
            question,
            options: serde_json::from_str(&options_json)?,
            payload: serde_json::from_str(&payload_json)?,
            status: status.parse()?,
            created_at: EpochMs(created_at),
            resolved_at: resolved_at.map(EpochMs),
        })
    })())
}

fn write_rejection_record(db: &Connection, record: &RejectionRecord) -> Result<()> {
    record.validate()?;
    let (speaker, chunk_sha256, char_start, char_end) = match &record.evidence_key {
        Some(ev) => (
            Some(ev.speaker.as_str()),
            Some(ev.chunk_sha256.as_str()),
            Some(i64::from(ev.quote_span.char_start)),
            Some(i64::from(ev.quote_span.char_end)),
        ),
        None => (None, None, None, None),
    };
    let (sem_fp, sem_json) = match &record.semantic_key {
        Some(sem) => (
            Some(sem.fingerprint().to_string()),
            Some(serde_json::to_string(sem)?),
        ),
        None => (None, None),
    };
    db.execute(
        "INSERT INTO rejected_claims
            (rejection_id, rejected_by_actor, speaker_actor, chunk_sha256, char_start, char_end,
             semantic_fingerprint, semantic_key_json, replacement_fact_id, reason, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            record.id.as_str(),
            record.rejected_by.as_str(),
            speaker,
            chunk_sha256,
            char_start,
            char_end,
            sem_fp,
            sem_json,
            record.replacement_fact_id.as_ref().map(FactId::as_str),
            record.reason,
            record.created_at.0,
        ],
    )?;
    Ok(())
}

fn read_rejection_row(row: &Row<'_>) -> Result<RejectionRecord> {
    let id: String = row.get(0)?;
    let rejected_by: String = row.get(1)?;
    let speaker: Option<String> = row.get(2)?;
    let chunk_sha256: Option<String> = row.get(3)?;
    let char_start: Option<u32> = row.get(4)?;
    let char_end: Option<u32> = row.get(5)?;
    let sem_json: Option<String> = row.get(6)?;
    let replacement: Option<String> = row.get(7)?;
    let reason: String = row.get(8)?;
    let created_at: i64 = row.get(9)?;

    let evidence_key = match (speaker, chunk_sha256, char_start, char_end) {
        (Some(sp), Some(hash), Some(start), Some(end)) => Some(RejectionEvidenceKey {
            speaker: sp.parse()?,
            chunk_sha256: hash.parse()?,
            quote_span: CharSpan::new(start, end)?,
        }),
        _ => None,
    };
    let semantic_key = sem_json
        .map(|json| serde_json::from_str::<RejectionSemanticKey>(&json))
        .transpose()?;

    Ok(RejectionRecord {
        id: id.parse()?,
        rejected_by: rejected_by.parse()?,
        evidence_key,
        semantic_key,
        replacement_fact_id: replacement.map(|r| r.parse()).transpose()?,
        reason,
        created_at: EpochMs(created_at),
    })
}

fn derive_fact_evidence_key(db: &Connection, fact: &Fact) -> Result<Option<RejectionEvidenceKey>> {
    let Some(anchor) = &fact.source_anchor else {
        return Ok(None);
    };
    let chunk_body: Option<String> = if let Some(chunk_id) = &anchor.chunk_id {
        db.query_row(
            "SELECT body FROM chunks WHERE chunk_id = ?1",
            [chunk_id.as_str()],
            |row| row.get(0),
        )
        .optional()?
    } else if let Some(source_id) = &anchor.source_id {
        let mut stmt =
            db.prepare("SELECT body FROM chunks WHERE source_id = ?1 ORDER BY ordinal LIMIT 2")?;
        let bodies = stmt
            .query_map([source_id.as_str()], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        (bodies.len() == 1)
            .then(|| bodies.into_iter().next())
            .flatten()
    } else {
        None
    };

    let Some(body) = chunk_body else {
        return Ok(None);
    };
    let span = match (anchor.char_start, anchor.char_end) {
        (Some(start), Some(end)) if end > start => Some(CharSpan::new(start, end)?),
        _ => anchor
            .verbatim_quote
            .as_deref()
            .and_then(|q| verify_verbatim_quote(&body, q, QuoteLocationHint::default()).ok())
            .map(|m| m.span),
    };
    let Some(quote_span) = span else {
        return Ok(None);
    };
    Ok(Some(RejectionEvidenceKey {
        speaker: fact.author.clone(),
        chunk_sha256: ContentHash::digest(&body),
        quote_span,
    }))
}

pub(crate) fn derive_fact_semantic_key(fact: &Fact) -> Option<RejectionSemanticKey> {
    let FactAccess::Base { stamp, .. } = &fact.access else {
        return None;
    };
    let mut concept_ids = BTreeSet::new();
    if let Some(GovernedScope::RelationGroup {
        group_concept_id, ..
    }) = &fact.governed_scope
    {
        concept_ids.insert(group_concept_id.clone());
    }
    if fact.kind == FactKind::Membership {
        if let Ok(rel) = serde_json::from_value::<MembershipRelation>(fact.object.clone()) {
            concept_ids.insert(rel.group_concept_id);
        }
    }
    if let Value::Object(map) = &fact.object {
        for key in [
            "condition_concept_id",
            "action_concept_id",
            "group_concept_id",
            "concept_id",
        ] {
            if let Some(id_str) = map.get(key).and_then(Value::as_str) {
                if let Ok(cid) = id_str.parse::<ConceptId>() {
                    concept_ids.insert(cid);
                }
            }
        }
        if let Some(arr) = map.get("concept_ids").and_then(Value::as_array) {
            for entry in arr {
                if let Some(id_str) = entry.as_str() {
                    if let Ok(cid) = id_str.parse::<ConceptId>() {
                        concept_ids.insert(cid);
                    }
                }
            }
        }
    }
    for tag in &fact.tags {
        if tag.starts_with("cpt_") {
            if let Ok(cid) = tag.parse::<ConceptId>() {
                concept_ids.insert(cid);
            }
        }
    }
    if concept_ids.is_empty() {
        return None;
    }
    Some(RejectionSemanticKey {
        kind: fact.kind,
        concept_ids,
        governed_scope: fact.governed_scope.clone(),
        valid_window: stamp.valid,
    })
}

#[cfg(test)]
#[path = "governance_tests.rs"]
mod tests;

/// Evaluates whether `item` is currently live and semiring-visible to `item.actor` against `graph`:
/// - Non-`Open` historical items always return `true`.
/// - `Open` items require `item.actor` to be an active member in `graph.members`.
/// - `ConceptAlignment`: when `payload` is non-empty (or carries fact ID keys), BOTH
///   `event_fact_id`/`left_fact_id` and `constraint_fact_id`/`right_fact_id` must exist in `graph.facts`,
///   be `Active` and non-expired, and allow `item.actor` under live base + `source_scopes` visibility.
/// - `AmbiguousScope`: if `payload.constraint_fact_id` is set or `target_id` resolves to a fact in
///   `graph.facts`, that constraint must be `Active`, non-expired, carry `broader_scope.is_some()`,
///   and allow `item.actor` under live base + `source_scopes` visibility.
pub fn is_open_review_currently_valid(graph: &DerivationView, item: &ReviewItem) -> bool {
    if item.status != ReviewStatus::Open {
        return true;
    }
    if !graph.members.contains(&item.actor) {
        return false;
    }
    match item.kind {
        ReviewKind::ConceptAlignment => {
            let payload_empty = match &item.payload {
                Value::Null => true,
                Value::Object(map) => map.is_empty(),
                _ => false,
            };
            if payload_empty {
                return true;
            }
            let left_fid = item
                .payload
                .get("event_fact_id")
                .or_else(|| item.payload.get("left_fact_id"))
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<FactId>().ok());
            let right_fid = item
                .payload
                .get("constraint_fact_id")
                .or_else(|| item.payload.get("right_fact_id"))
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<FactId>().ok());
            let (Some(lf_id), Some(rf_id)) = (left_fid, right_fid) else {
                return false;
            };
            let (Some(lf), Some(rf)) = (graph.facts.get(&lf_id), graph.facts.get(&rf_id)) else {
                return false;
            };
            is_fact_live_visible_to_actor(graph, lf, &item.actor)
                && is_fact_live_visible_to_actor(graph, rf, &item.actor)
        }
        ReviewKind::AmbiguousScope => {
            if !is_ambiguous_scope_review_supported(graph, item) {
                return false;
            }
            if let Some(explicit_cid_str) = item
                .payload
                .get("constraint_fact_id")
                .and_then(Value::as_str)
            {
                let Ok(cid) = explicit_cid_str.parse::<FactId>() else {
                    return false;
                };
                let Some(fact) = graph.facts.get(&cid) else {
                    return false;
                };
                return fact.broader_scope.is_some()
                    && is_fact_live_visible_to_actor(graph, fact, &item.actor);
            }
            if let Ok(cid) = item.target_id.trim().parse::<FactId>() {
                if let Some(fact) = graph.facts.get(&cid) {
                    return fact.broader_scope.is_some()
                        && is_fact_live_visible_to_actor(graph, fact, &item.actor);
                }
            }
            true
        }
        ReviewKind::IdentityMapping | ReviewKind::WritebackProposal => true,
    }
}

pub(crate) fn upsert_community_concept_tx(
    tx: &Connection,
    concept: &CommunityConcept,
) -> Result<()> {
    reject_bare_hash("concept_id", concept.id.as_str())?;
    if normalize_concept_label(&concept.canonical_label).is_empty() {
        return Err(KpError::invalid("canonical_label", "concept label must be nonempty").into());
    }
    tx.execute(
        "INSERT INTO community_concepts
            (concept_id, concept_kind, canonical_label, domain, polarity, proposed_by, status, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7)
         ON CONFLICT(concept_id) DO UPDATE SET
            concept_kind = excluded.concept_kind,
            canonical_label = excluded.canonical_label,
            domain = excluded.domain,
            polarity = excluded.polarity,
            proposed_by = excluded.proposed_by,
            status = 'active'",
        params![
            concept.id.as_str(),
            concept.kind.as_str(),
            concept.canonical_label.trim(),
            concept.domain.trim(),
            concept.polarity.as_str(),
            concept.proposed_by.as_str(),
            concept.created_at.0,
        ],
    )?;
    Ok(())
}

/// Checks that all supporting `Membership` premises and `impacted_actors` recorded on an
/// `AmbiguousScope` review remain active, unrevoked, and live-visible to `item.actor`
/// (including legacy `RelationGroup` reviews written without `supporting_membership_ids`).
pub fn is_ambiguous_scope_review_supported(graph: &DerivationView, item: &ReviewItem) -> bool {
    let mut impacted_set = BTreeSet::new();
    if let Some(imp_val) = item.payload.get("impacted_actors") {
        let Some(arr) = imp_val.as_array() else {
            return false;
        };
        if arr.is_empty() {
            return false;
        }
        for elem in arr {
            let Some(actor_str) = elem.as_str() else {
                return false;
            };
            let Ok(actor_id) = actor_str.parse::<ActorId>() else {
                return false;
            };
            if actor_id == item.actor || !graph.members.contains(&actor_id) {
                return false;
            }
            impacted_set.insert(actor_id);
        }
    }

    let broader_candidate: Option<kp_model::GovernedScope> = item
        .payload
        .get("broader_candidate")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .or_else(|| {
            let cid_str = item
                .payload
                .get("constraint_fact_id")
                .and_then(Value::as_str)
                .unwrap_or(item.target_id.as_str());
            cid_str
                .parse::<FactId>()
                .ok()
                .and_then(|fid| graph.facts.get(&fid))
                .and_then(|f| f.broader_scope.as_ref())
                .map(|b| b.broader_candidate.clone())
        });

    if let Some(mem_val) = item.payload.get("supporting_membership_ids") {
        let Some(arr) = mem_val.as_array() else {
            return false;
        };
        if arr.is_empty() {
            return false;
        }
        for elem in arr {
            let Some(mid_str) = elem.as_str() else {
                return false;
            };
            let Ok(mid) = mid_str.parse::<FactId>() else {
                return false;
            };
            let Some(mf) = graph.facts.get(&mid) else {
                return false;
            };
            if mf.kind != kp_model::FactKind::Membership
                || !is_fact_live_visible_to_actor(graph, mf, &item.actor)
            {
                return false;
            }
            let Ok(rel) = serde_json::from_value::<kp_model::MembershipRelation>(mf.object.clone())
            else {
                return false;
            };
            if rel.member == item.actor || !graph.members.contains(&rel.member) {
                return false;
            }
        }
    } else if let Some(kp_model::GovernedScope::RelationGroup {
        anchor,
        group_concept_id,
    }) = broader_candidate
    {
        // Legacy `RelationGroup` review lacking `supporting_membership_ids`: require live visible
        // `Membership` support (direct or entailed) covering at least one non-author member and all
        // `impacted_actors`.
        let entailed_left_to_target: BTreeSet<kp_model::ConceptId> = graph
            .facts
            .values()
            .filter(|f| {
                f.status == FactStatus::Active
                    && f.access.expired_at().is_none()
                    && f.predicate == "kp.core.concept_alignment"
                    && f.object.get("evidence_refs").is_none()
                    && f.object.get("binding").and_then(Value::as_str) == Some("concept_label_v2")
                    && f.object.get("verdict").and_then(Value::as_str) == Some("entails")
                    && f.object.get("right_concept_id").and_then(Value::as_str)
                        == Some(group_concept_id.as_str())
            })
            .filter_map(|f| {
                f.object
                    .get("left_concept_id")
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<kp_model::ConceptId>().ok())
            })
            .collect();

        let mut live_group_members = BTreeSet::new();
        for mf in graph.facts.values() {
            if mf.kind != kp_model::FactKind::Membership
                || !is_fact_live_visible_to_actor(graph, mf, &item.actor)
            {
                continue;
            }
            let Ok(rel) = serde_json::from_value::<kp_model::MembershipRelation>(mf.object.clone())
            else {
                continue;
            };
            if rel.anchor == anchor
                && rel.member != item.actor
                && graph.members.contains(&rel.member)
                && (rel.group_concept_id == group_concept_id
                    || entailed_left_to_target.contains(&rel.group_concept_id))
            {
                live_group_members.insert(rel.member);
            }
        }
        if live_group_members.is_empty()
            || (!impacted_set.is_empty() && !impacted_set.is_subset(&live_group_members))
        {
            return false;
        }
    }
    true
}

fn is_open_review_resolvable_in_tx(
    tx: &Connection,
    graph: &DerivationView,
    item: &ReviewItem,
) -> Result<bool> {
    if is_open_review_currently_valid(graph, item) {
        return Ok(true);
    }
    if item.status != ReviewStatus::Open
        || !graph.members.contains(&item.actor)
        || item.kind != ReviewKind::AmbiguousScope
        || !is_ambiguous_scope_review_supported(graph, item)
    {
        return Ok(false);
    }
    // Permit closing the `ReviewItem` row immediately after `resolve_ambiguous_scope_review`
    // superseded the constraint into an active non-rejected replacement or cleared `broader_scope`
    // on `Dismiss`, while rejecting withdrawn, rejected, expired, or hidden constraints.
    let target_fid = item
        .payload
        .get("constraint_fact_id")
        .and_then(Value::as_str)
        .unwrap_or_else(|| item.target_id.trim())
        .parse::<FactId>()
        .ok();
    let Some(cid) = target_fid else {
        return Ok(false);
    };
    let Some(old_fact) = graph.facts.get(&cid) else {
        return Ok(false);
    };
    if old_fact.status == FactStatus::Active {
        return Ok(old_fact.broader_scope.is_none()
            && is_fact_live_visible_to_actor(graph, old_fact, &item.actor));
    }
    if old_fact.status == FactStatus::Superseded {
        if let Some(rep_id) = &old_fact.superseded_by {
            let rejected: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM rejected_claims WHERE replacement_fact_id = ?1)",
                [rep_id.as_str()],
                |r| r.get(0),
            )?;
            if !rejected {
                if let Some(rep) = graph.facts.get(rep_id) {
                    return Ok(rep.author == item.actor
                        && is_fact_live_visible_to_actor(graph, rep, &item.actor));
                }
            }
        }
    }
    Ok(false)
}

fn is_fact_live_visible_to_actor(graph: &DerivationView, fact: &Fact, actor: &ActorId) -> bool {
    if fact.status != FactStatus::Active || fact.access.expired_at().is_some() {
        return false;
    }
    match &fact.access {
        FactAccess::Base { audience, .. } => {
            if !audience.allows(actor, &graph.members) {
                return false;
            }
            if let Some(sid) = &fact.source_scope_id {
                let Some(scope_aud) = graph.source_scopes.get(sid) else {
                    return false;
                };
                if !scope_aud.allows(actor, &graph.members) {
                    return false;
                }
            }
            true
        }
        FactAccess::Derived { recorded_at, .. } => {
            graph.evaluate(actor, *recorded_at).facts.contains(&fact.id)
        }
    }
}

fn validate_review_item_fields(item: &ReviewItem) -> Result<()> {
    reject_bare_hash("review_id", item.id.as_str())?;
    if item.target_id.trim().is_empty() {
        return Err(KpError::invalid("target_id", "review target_id must be nonempty").into());
    }
    if item.question.trim().is_empty() {
        return Err(KpError::invalid("question", "review question must be nonempty").into());
    }
    Ok(())
}

pub(crate) fn upsert_open_review_item_tx(tx: &Connection, item: &ReviewItem) -> Result<ReviewItem> {
    let existing_open: Option<(String, i64)> = tx
        .query_row(
            "SELECT review_id, created_at FROM review_items
             WHERE actor_id = ?1 AND kind = ?2 AND target_id = ?3 AND status = 'open'
             ORDER BY created_at ASC, review_id ASC
             LIMIT 1",
            params![
                item.actor.as_str(),
                item.kind.as_str(),
                item.target_id.trim()
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    let options_json = serde_json::to_string(&item.options)?;
    let payload_json = serde_json::to_string(&item.payload)?;

    if let Some((existing_id, existing_created)) = existing_open {
        tx.execute(
            "UPDATE review_items
             SET question = ?1, options_json = ?2, payload_json = ?3
             WHERE review_id = ?4",
            params![
                item.question.trim(),
                options_json,
                payload_json,
                existing_id
            ],
        )?;
        Ok(ReviewItem {
            id: existing_id.parse()?,
            actor: item.actor.clone(),
            kind: item.kind,
            target_id: item.target_id.trim().to_owned(),
            question: item.question.trim().to_owned(),
            options: item.options.clone(),
            payload: item.payload.clone(),
            status: ReviewStatus::Open,
            created_at: EpochMs(existing_created),
            resolved_at: None,
        })
    } else {
        tx.execute(
            "INSERT INTO review_items
                (review_id, actor_id, kind, target_id, question, options_json, payload_json, status, created_at, resolved_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'open', ?8, NULL)",
            params![
                item.id.as_str(),
                item.actor.as_str(),
                item.kind.as_str(),
                item.target_id.trim(),
                item.question.trim(),
                options_json,
                payload_json,
                item.created_at.0,
            ],
        )?;
        Ok(ReviewItem {
            id: item.id.clone(),
            actor: item.actor.clone(),
            kind: item.kind,
            target_id: item.target_id.trim().to_owned(),
            question: item.question.trim().to_owned(),
            options: item.options.clone(),
            payload: item.payload.clone(),
            status: ReviewStatus::Open,
            created_at: item.created_at,
            resolved_at: None,
        })
    }
}
