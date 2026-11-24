//! Public JSONL Ingest Contract atomic `Store` primitives (`Rev 5 §0.1.2`).
//!
//! Provides SQL-free transactional primitives for `kp ingest`, MCP `kp_note`, `kp daemon` UDS
//! `IngestJsonl`, external connectors, and Hooks:
//! - [`Store::apply_public_container_roster`]: authorizes `Owner | Maintainer` callers, enforces
//!   strictly monotonic `synced_at` anti-rollback, scrubs `label` via [`SecretRedactor`], resolves
//!   library `actor_id`s and confirmed `external_identities`, records unconfirmed `namespace:id`
//!   entries as `status = 'proposed'`, and updates `source_scopes` live container ceilings;
//! - [`Store::apply_public_ingest_record`] & [`Store::ingest_public_session_subbatch`]: scrub
//!   content and speaker display names via [`SecretRedactor`] prior to hashing/storage, enforce
//!   caller membership, `ingested_by`, cross-speaker container membership, and target source
//!   ownership on `Supersede`/`Withdraw`, handle policy narrowing/conflicts on duplicate `Upsert`,
//!   and enforce `RawOnly` zero-extraction & zero-cloud-embedder guarantees.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use kp_model::{
    is_byte_identical_media_type, parse_public_jsonl_str, partition_chunks_by_token_budget,
    ActorId, AnchorSpec, AudienceSpec, ContentHash, EpochMs, ExtractionChunkSlice, FactId, KpError,
    LogicalKey, MemberRole, ModelRoutePolicy, PublicAudience, PublicContainerRoster,
    PublicIngestLine, PublicIngestOp, PublicIngestRecord, PublicSpeaker, SecretRedactor, SliceRole,
    SourceId, SourceScope, SourceScopeId, DEFAULT_MAX_BATCH_TOKENS,
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::embedding::{encode_vector_le_bytes, guard_and_lock_tx_embedding_dims, FtsOnlyEmbedder};
use crate::facts::is_source_marked_raw_only;
use crate::maintenance::invalidate;
use crate::ops::{embed_chunk_texts, ChunkInput, StoredSource};
use crate::queue::{enqueue_or_rearm_extraction_job_tx, ExtractionJobInput};
use crate::tombstone::guard;
use crate::{Embedder, FactInput, Remembered, Result, Store, StoreError};

/// Lifecycle write mode for [`Store::upsert_governed_source`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum GovernedUpsertMode {
    /// Inserts when new, returns `DuplicateIdempotent` when active SHA matches, or supersedes
    /// prior active version(s) automatically when `bytes` SHA changes on `external_source_key`.
    #[default]
    AutoSupersede,
    /// Strict Public Ingest Contract `op = "upsert"`: conflicts if `external_source_key` already
    /// exists with a different `content_sha256`.
    StrictUpsert,
    /// Explicit supersession retiring `previous_external_source_key`.
    Supersede {
        previous_external_source_key: String,
    },
    /// Withdraws all active versions matching `external_source_key`.
    Withdraw,
}

/// Unified governed source input accepted by [`Store::upsert_governed_source`].
pub struct GovernedSourceInput {
    pub external_source_key: String,
    pub uri: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
    pub chunks: Vec<ChunkInput>,
    pub speaker: ActorId,
    pub ingested_by: ActorId,
    pub readable_by: AudienceSpec,
    pub source_scope_id: Option<SourceScopeId>,
    pub effective_policy: ModelRoutePolicy,
    pub op_kind: GovernedUpsertMode,
    pub thread_key: Option<String>,
    pub reply_to_key: Option<String>,
    pub captured_at: EpochMs,
}

/// Structured result of [`Store::upsert_governed_source`].
#[derive(Clone, Debug, Serialize)]
pub struct GovernedSourceOutcome {
    pub kind: PublicRecordAppliedKind,
    pub external_source_key: String,
    pub source: Option<StoredSource>,
    pub superseded_source_id: Option<SourceId>,
    pub affected_facts: Vec<FactId>,
    pub enqueued_job_ids: Vec<String>,
    pub resolved_speaker: ActorId,
}

/// Outcome classification for [`Store::apply_public_ingest_record`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicRecordAppliedKind {
    Upserted,
    DuplicateIdempotent,
    Superseded,
    Withdrawn,
    NoopMissingTarget,
}

/// Structured outcome of applying one [`PublicIngestRecord`] atomically.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicRecordApplyOutcome {
    pub kind: PublicRecordAppliedKind,
    pub external_source_key: String,
    pub source_id: Option<SourceId>,
    pub superseded_source_id: Option<SourceId>,
    pub affected_facts: Vec<FactId>,
    pub enqueued_job_id: Option<String>,
    pub resolved_speaker: ActorId,
}

/// Aggregated report returned by [`Store::apply_public_ingest_jsonl`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicIngestStreamReport {
    pub rosters_applied: usize,
    pub records_upserted: usize,
    pub records_duplicate: usize,
    pub records_superseded: usize,
    pub records_withdrawn: usize,
    pub jobs_enqueued: usize,
    pub source_ids: Vec<SourceId>,
}

/// Parameters for [`Store::ingest_public_session_subbatch`].
#[derive(Clone, Debug)]
pub struct PublicSessionSubbatchSpec<'a> {
    pub caller: &'a ActorId,
    pub record: &'a PublicIngestRecord,
    pub global_policy: &'a ModelRoutePolicy,
    pub session_id: &'a str,
    pub start_byte: u64,
    pub end_byte: u64,
    pub job_priority: i32,
    pub self_echo_filtered: bool,
    pub at: EpochMs,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct StoredPublicSourceMeta {
    #[serde(default)]
    pub(crate) explicit_policy: Option<ModelRoutePolicy>,
    #[serde(default)]
    pub(crate) effective_policy: Option<ModelRoutePolicy>,
    #[serde(default)]
    pub(crate) speaker: Option<String>,
}

pub(crate) fn shared_redactor() -> Result<&'static SecretRedactor> {
    static REDACTOR: OnceLock<std::result::Result<SecretRedactor, String>> = OnceLock::new();
    let res = REDACTOR.get_or_init(|| SecretRedactor::new().map_err(|e| e.to_string()));
    match res {
        Ok(r) => Ok(r),
        Err(msg) => Err(StoreError::Embedding(format!(
            "secret redactor init failed: {msg}"
        ))),
    }
}

pub(crate) fn require_active_member_role_tx(
    db: &Connection,
    actor: &ActorId,
) -> Result<MemberRole> {
    let role_str: Option<String> = db
        .query_row(
            "SELECT role FROM members_tokens WHERE actor_id = ?1 AND revoked_at IS NULL",
            [actor.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    let Some(raw) = role_str else {
        return Err(StoreError::PermissionDenied);
    };
    Ok(raw.parse()?)
}

fn resolve_effective_ingester_tx(
    db: &Connection,
    caller: &ActorId,
    is_admin: bool,
    explicit_ingested_by: Option<&ActorId>,
) -> Result<ActorId> {
    match explicit_ingested_by {
        Some(ing) if ing != caller => {
            if !is_admin {
                return Err(StoreError::PermissionDenied);
            }
            let _ = require_active_member_role_tx(db, ing)?;
            Ok(ing.clone())
        }
        _ => Ok(caller.clone()),
    }
}

fn audience_includes_actor(audience: &AudienceSpec, actor: &ActorId) -> bool {
    match audience {
        AudienceSpec::AllMembers => true,
        AudienceSpec::Explicit(set) => set.contains(actor),
        AudienceSpec::SemiringDerived => false,
    }
}

fn scope_currently_includes_actor_tx(
    db: &Connection,
    scope_id: &SourceScopeId,
    actor: &ActorId,
) -> Result<bool> {
    let raw_aud: Option<String> = db
        .query_row(
            "SELECT current_audience_json FROM source_scopes WHERE scope_id = ?1",
            [scope_id.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    let Some(aud_str) = raw_aud else {
        return Ok(false);
    };
    let aud: AudienceSpec = serde_json::from_str(&aud_str)?;
    Ok(audience_includes_actor(&aud, actor))
}

fn enforce_caller_write_authorization_tx(
    db: &Connection,
    caller: &ActorId,
    is_admin: bool,
    resolved_speaker: &ActorId,
    readable_by: &AudienceSpec,
    source_scope_id: Option<&SourceScopeId>,
) -> Result<()> {
    if is_admin {
        return Ok(());
    }
    if !audience_includes_actor(readable_by, caller) {
        return Err(StoreError::PermissionDenied);
    }
    if let Some(scope_id) = source_scope_id {
        if !scope_currently_includes_actor_tx(db, scope_id, caller)? {
            return Err(StoreError::PermissionDenied);
        }
    } else if resolved_speaker != caller {
        // Non-admin members cannot attribute claims to other speakers without a shared live container.
        return Err(StoreError::PermissionDenied);
    }
    Ok(())
}

pub(crate) fn load_public_source_meta_tx(
    db: &Connection,
    source_id: &str,
) -> Result<Option<StoredPublicSourceMeta>> {
    let raw: Option<String> = db
        .query_row(
            "SELECT payload_json FROM change_log
             WHERE op = 'public_source_meta' AND target_id = ?1
             ORDER BY seq DESC LIMIT 1",
            [source_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    match raw {
        Some(s) => Ok(Some(serde_json::from_str(&s)?)),
        None => Ok(None),
    }
}

fn load_initial_public_source_meta_tx(
    db: &Connection,
    source_id: &str,
) -> Result<Option<StoredPublicSourceMeta>> {
    let raw: Option<String> = db
        .query_row(
            "SELECT payload_json FROM change_log
             WHERE op = 'public_source_meta' AND target_id = ?1
             ORDER BY seq ASC LIMIT 1",
            [source_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    match raw {
        Some(s) => Ok(Some(serde_json::from_str(&s)?)),
        None => Ok(None),
    }
}

fn verify_target_source_mutation_allowed_tx(
    db: &Connection,
    caller: &ActorId,
    is_admin: bool,
    source_id: &str,
    ingested_by_actor: Option<&str>,
    source_scope_id: Option<&str>,
) -> Result<()> {
    if is_admin {
        return Ok(());
    }
    if ingested_by_actor == Some(caller.as_str()) {
        return Ok(());
    }
    if let Some(scope_str) = source_scope_id {
        let scope_id: SourceScopeId = scope_str.parse()?;
        if scope_currently_includes_actor_tx(db, &scope_id, caller)? {
            match load_initial_public_source_meta_tx(db, source_id)? {
                Some(meta) => {
                    if meta.speaker.as_deref() == Some(caller.as_str()) {
                        return Ok(());
                    }
                }
                None => {
                    // Legacy pre-M2 fallback: check ONLY the earliest extraction_queue row.
                    let first_author: Option<String> = db
                        .query_row(
                            "SELECT author_actor FROM extraction_queue
                             WHERE source_id = ?1
                             ORDER BY created_at ASC, rowid ASC
                             LIMIT 1",
                            [source_id],
                            |r| r.get(0),
                        )
                        .optional()?;
                    if first_author.as_deref() == Some(caller.as_str()) {
                        return Ok(());
                    }
                }
            }
        }
    }
    Err(StoreError::PermissionDenied)
}

fn sanitize_actor_component(raw: &str) -> String {
    let mut clean = String::with_capacity(raw.len());
    let mut lossless = true;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.') {
            clean.push(ch);
        } else {
            lossless = false;
            clean.push('_');
        }
    }
    if clean.is_empty() || !lossless || clean.len() > 48 {
        let full_digest = ContentHash::digest(raw.as_bytes());
        let short_hash = &full_digest.as_str()[..12];
        let prefix: String = clean.chars().take(24).collect();
        if prefix.is_empty() {
            format!("id_{short_hash}")
        } else {
            format!("{prefix}_{short_hash}")
        }
    } else {
        clean
    }
}

fn resolve_or_propose_external_identity_tx(
    tx: &Connection,
    namespace: &str,
    external_id: &str,
    redacted_display_name: Option<&str>,
    at: EpochMs,
) -> Result<Option<ActorId>> {
    let ns = namespace.trim();
    let ext = external_id.trim();
    let existing: Option<(String, Option<String>, String)> = tx
        .query_row(
            "SELECT status, confirmed_actor_id, display_name
             FROM external_identities
             WHERE provider = ?1 AND external_id = ?2",
            params![ns, ext],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;

    let clean_display = redacted_display_name
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .unwrap_or("");

    match existing {
        Some((status, confirmed_opt, prev_display)) => {
            if !clean_display.is_empty() && clean_display != prev_display {
                tx.execute(
                    "UPDATE external_identities SET display_name = ?1, updated_at = MAX(updated_at, ?2)
                     WHERE provider = ?3 AND external_id = ?4",
                    params![clean_display, at.0, ns, ext],
                )?;
            }
            if status == "confirmed" {
                if let Some(actor_str) = confirmed_opt {
                    return Ok(Some(actor_str.parse()?));
                }
            }
            Ok(None)
        }
        None => {
            tx.execute(
                "INSERT INTO external_identities
                    (provider, external_id, display_name, proposed_actor_id, confirmed_actor_id, status, updated_at)
                 VALUES (?1, ?2, ?3, NULL, NULL, 'proposed', ?4)",
                params![ns, ext, clean_display, at.0],
            )?;
            Ok(None)
        }
    }
}

fn resolve_speaker_tx(
    tx: &Connection,
    speaker: &PublicSpeaker,
    redactor: &SecretRedactor,
    at: EpochMs,
) -> Result<ActorId> {
    let raw = speaker.id.trim();
    let redacted_display = speaker
        .display_name
        .as_deref()
        .map(|d| redactor.redact(d.trim()));
    if let Some((ns, ext)) = raw.split_once(':') {
        if let Some(confirmed) =
            resolve_or_propose_external_identity_tx(tx, ns, ext, redacted_display.as_deref(), at)?
        {
            return Ok(confirmed);
        }
        let synth = format!(
            "ext.{}.{}",
            sanitize_actor_component(ns.trim()),
            sanitize_actor_component(ext.trim())
        );
        Ok(synth.parse()?)
    } else {
        Ok(raw.parse()?)
    }
}

fn route_policy_to_queue_str(policy: &ModelRoutePolicy) -> Result<Option<String>> {
    match policy {
        ModelRoutePolicy::RawOnly => Ok(None),
        ModelRoutePolicy::HostCli => Ok(Some("host".to_owned())),
        ModelRoutePolicy::NamedEndpoint(ep) => {
            let trimmed = ep.trim();
            if trimmed.is_empty() || trimmed == "raw_only" || trimmed.chars().any(char::is_control)
            {
                return Err(KpError::invalid(
                    "model_policy",
                    "named endpoint must be nonempty and not `raw_only`",
                )
                .into());
            }
            Ok(Some(format!("named_endpoint_v2:{trimmed}")))
        }
    }
}

/// Decodes an `extraction_queue.model_policy` string (`"host"`, `"raw_only"`, or
/// `"named_endpoint_v2:<id>"` / legacy `"named_endpoint:<id>"`) into [`ModelRoutePolicy`]
/// without `"host"` or self-prefixed `"named_endpoint:..."` collision ambiguity.
pub fn decode_queue_model_policy(raw: &str) -> Result<ModelRoutePolicy> {
    let trimmed = raw.trim();
    match trimmed {
        "" | "host" | "host_cli" => Ok(ModelRoutePolicy::HostCli),
        "raw_only" | "raw-only" | "none" => Ok(ModelRoutePolicy::RawOnly),
        other => {
            let ep = if let Some(v2) = other.strip_prefix("named_endpoint_v2:") {
                v2.trim()
            } else {
                other
                    .strip_prefix("named_endpoint:")
                    .unwrap_or(other)
                    .trim()
            };
            if ep.is_empty() || ep.chars().any(char::is_control) {
                return Err(KpError::invalid(
                    "model_policy",
                    "named endpoint identifier must be nonempty",
                )
                .into());
            }
            Ok(ModelRoutePolicy::NamedEndpoint(ep.to_owned()))
        }
    }
}

/// Repairs legacy `extraction_queue` rows (including pre-prefix `NamedEndpoint("host")` stored as
/// bare `"host"` and self-prefixed `NamedEndpoint("named_endpoint:...")` stored without `v2` tag)
/// to canonical `"named_endpoint_v2:<ep>"` using `public_source_meta`.
pub(crate) fn repair_legacy_named_endpoint_queue_rows(db: &Connection) -> Result<()> {
    let candidates: Vec<(String, String, String)> = {
        let mut stmt = db.prepare(
            "SELECT job_id, source_id, model_policy
             FROM extraction_queue
             WHERE status IN ('pending', 'leased', 'paused_quota')",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    for (job_id, source_id, cur_pol) in candidates {
        if is_source_marked_raw_only(db, &source_id)? {
            db.execute(
                "UPDATE extraction_queue
                 SET status = 'done', lease_until = NULL, last_error = 'cancelled_raw_only_narrowed'
                 WHERE job_id = ?1",
                [&job_id],
            )?;
            continue;
        }
        if let Some(meta) = load_public_source_meta_tx(db, &source_id)? {
            if let Some(ref eff_pol) = meta.effective_policy {
                if let Some(canonical) = route_policy_to_queue_str(eff_pol)? {
                    if cur_pol != canonical {
                        db.execute(
                            "UPDATE extraction_queue SET model_policy = ?1 WHERE job_id = ?2",
                            params![canonical, job_id],
                        )?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn retire_source_and_facts_tx(
    tx: &Connection,
    target_source_id: &str,
    marker: &str,
    fact_status: &str,
    invalidation_reason: &str,
    actor: &ActorId,
    at: EpochMs,
) -> Result<Vec<FactId>> {
    tx.execute(
        "UPDATE raw_sources
         SET superseded_by_source = ?1
         WHERE source_id = ?2 AND superseded_by_source IS NULL",
        params![marker, target_source_id],
    )?;

    tx.execute(
        "UPDATE extraction_queue
         SET status = 'done', lease_until = NULL, last_error = ?1
         WHERE source_id = ?2 AND status IN ('pending', 'leased', 'paused_quota', 'failed_permanent')",
        params![format!("cancelled_{invalidation_reason}"), target_source_id],
    )?;

    let active_facts: Vec<(String, String, String, String)> = {
        let mut stmt = tx.prepare(
            "SELECT fact_id, origin_library_id, logical_key, content_sha256
             FROM facts
             WHERE (
                 json_extract(source_anchor_json, '$.source_id') = ?1
                 OR json_extract(object_json, '$.evidence_anchor.source_id') = ?1
             )
               AND expired_at IS NULL
               AND status IN ('active', 'superseded')
             ORDER BY fact_id",
        )?;
        let rows = stmt.query_map([target_source_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        rows.collect::<std::result::Result<_, _>>()?
    };

    let mut affected = Vec::with_capacity(active_facts.len());
    for (fid_str, orig_str, key_str, sha_str) in active_facts {
        let fid: FactId = fid_str.parse()?;
        let changed = tx.execute(
            "UPDATE facts
             SET status = ?1, expired_at = ?2
             WHERE fact_id = ?3 AND expired_at IS NULL",
            params![fact_status, at.0, fid.as_str()],
        )?;
        if changed > 0 {
            invalidate(tx, &fid, at, invalidation_reason)?;
            crate::exchange::cancel_publishes_and_queue_withdrawals(tx, &fid)?;
            crate::exchange::record_withdrawn_version_tx(
                tx,
                &orig_str.parse()?,
                &key_str.parse()?,
                &sha_str.parse()?,
                actor,
                at,
            )?;
            tx.execute(
                "INSERT INTO change_log (op, target_id, origin_library_id, logical_key, actor_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    invalidation_reason,
                    fid.as_str(),
                    &orig_str,
                    &key_str,
                    actor.as_str(),
                    at.0
                ],
            )?;
            affected.push(fid);
        }
    }
    Ok(affected)
}

struct DuplicateUpsertPolicySpec<'a> {
    existing_sid: &'a SourceId,
    external_source_key: &'a str,
    partitioned_batches: &'a [(Vec<String>, ContentHash)],
    resolved_speaker: &'a ActorId,
    explicit_policy: Option<&'a ModelRoutePolicy>,
    queue_policy_opt: Option<&'a str>,
    job_priority: i32,
    thread_key: Option<&'a str>,
    reply_to_key: Option<&'a str>,
    occurred_at: EpochMs,
    embedder_allows_raw_only: bool,
}

fn handle_duplicate_upsert_policy_tx(
    tx: &Connection,
    spec: &DuplicateUpsertPolicySpec<'_>,
) -> Result<Vec<String>> {
    let DuplicateUpsertPolicySpec {
        existing_sid,
        external_source_key,
        partitioned_batches,
        resolved_speaker,
        explicit_policy,
        queue_policy_opt,
        job_priority,
        thread_key,
        reply_to_key,
        occurred_at,
        embedder_allows_raw_only,
    } = *spec;
    let was_raw_only = is_source_marked_raw_only(tx, existing_sid.as_str())?;
    let prev_meta = load_public_source_meta_tx(tx, existing_sid.as_str())?;
    let is_raw_only = queue_policy_opt.is_none();

    if was_raw_only {
        if explicit_policy.is_some_and(|p| !matches!(p, ModelRoutePolicy::RawOnly)) {
            return Err(StoreError::Conflict);
        }
        return Ok(Vec::new());
    }

    if is_raw_only {
        tx.execute(
            "INSERT INTO change_log (op, target_id, created_at) VALUES ('raw_only_source', ?1, ?2)",
            params![existing_sid.as_str(), occurred_at.0],
        )?;
        tx.execute(
            "UPDATE extraction_queue
             SET status = 'done', lease_until = NULL, last_error = 'cancelled_raw_only_narrowed'
             WHERE source_id = ?1 AND status IN ('pending', 'leased', 'paused_quota', 'failed_permanent')",
            [existing_sid.as_str()],
        )?;
        let force_purge_all: i64 = i64::from(!embedder_allows_raw_only);
        tx.execute(
            "DELETE FROM chunk_vectors
             WHERE chunk_id IN (SELECT chunk_id FROM chunks WHERE source_id = ?1)
               AND (?2 = 1 OR (model_id != 'hash:v1' AND model_id NOT LIKE 'local:%'))",
            params![existing_sid.as_str(), force_purge_all],
        )?;
        tx.execute(
            "DELETE FROM fact_vectors
             WHERE fact_id IN (
                 WITH RECURSIVE derived_from_src(fid) AS (
                     SELECT fact_id FROM facts
                     WHERE json_extract(source_anchor_json, '$.source_id') = ?1
                        OR json_extract(object_json, '$.evidence_anchor.source_id') = ?1
                     UNION
                     SELECT j.conclusion_fact_id
                     FROM derived_from_src d
                     JOIN justification_premises p ON p.premise_fact_id = d.fid
                     JOIN justifications j ON j.just_id = p.just_id
                 )
                 SELECT fid FROM derived_from_src
             )
               AND (?2 = 1 OR (model_id != 'hash:v1' AND model_id NOT LIKE 'local:%'))",
            params![existing_sid.as_str(), force_purge_all],
        )?;
        let updated_meta = StoredPublicSourceMeta {
            explicit_policy: explicit_policy.cloned(),
            effective_policy: Some(ModelRoutePolicy::RawOnly),
            speaker: Some(resolved_speaker.to_string()),
        };
        tx.execute(
            "INSERT INTO change_log (op, target_id, logical_key, payload_json, created_at)
             VALUES ('public_source_meta', ?1, ?2, ?3, ?4)",
            params![
                existing_sid.as_str(),
                external_source_key,
                serde_json::to_string(&updated_meta)?,
                occurred_at.0,
            ],
        )?;
        return Ok(Vec::new());
    }

    if let Some(ref meta) = prev_meta {
        if let (Some(prev_exp), Some(cur_exp)) = (&meta.explicit_policy, explicit_policy) {
            if prev_exp != cur_exp {
                return Err(StoreError::Conflict);
            }
        } else if meta.explicit_policy.is_none() {
            if let (Some(prev_eff), Some(cur_exp)) = (&meta.effective_policy, explicit_policy) {
                if prev_eff != cur_exp {
                    return Err(StoreError::Conflict);
                }
            }
        }
    }

    let Some(policy_str) = queue_policy_opt else {
        return Ok(Vec::new());
    };
    let prev_job_policy: Option<String> = tx
        .query_row(
            "SELECT model_policy FROM extraction_queue WHERE source_id = ?1 ORDER BY created_at DESC LIMIT 1",
            [existing_sid.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(ref existing_pol) = prev_job_policy {
        let decoded_existing = match prev_meta.as_ref().and_then(|m| m.effective_policy.clone()) {
            Some(pol) => pol,
            None => decode_queue_model_policy(existing_pol)?,
        };
        let decoded_new = decode_queue_model_policy(policy_str)?;
        if explicit_policy.is_some() && decoded_existing != decoded_new {
            return Err(StoreError::Conflict);
        }
        if existing_pol != policy_str && decoded_existing == decoded_new {
            tx.execute(
                "UPDATE extraction_queue
                 SET model_policy = ?1
                 WHERE source_id = ?2 AND status IN ('pending', 'leased', 'paused_quota')",
                params![policy_str, existing_sid.as_str()],
            )?;
        }
    }
    let effective_job_policy = match prev_meta.as_ref().and_then(|m| m.effective_policy.as_ref()) {
        Some(prev_eff) => {
            route_policy_to_queue_str(prev_eff)?.unwrap_or_else(|| policy_str.to_owned())
        }
        None => match prev_job_policy {
            Some(ref old_pol) => {
                let dec = decode_queue_model_policy(old_pol)?;
                route_policy_to_queue_str(&dec)?.unwrap_or_else(|| policy_str.to_owned())
            }
            None => policy_str.to_owned(),
        },
    };
    let mut job_ids = Vec::with_capacity(partitioned_batches.len());
    for (chunk_ids, batch_sha) in partitioned_batches {
        let job = enqueue_or_rearm_extraction_job_tx(
            tx,
            existing_sid,
            &ExtractionJobInput {
                source_id: existing_sid.clone(),
                chunk_ids: chunk_ids.clone(),
                chunk_batch_sha256: batch_sha.clone(),
                author: resolved_speaker.clone(),
                model_policy: effective_job_policy.clone(),
                priority: job_priority,
                reference_time: occurred_at,
                created_at: occurred_at,
            },
            thread_key,
            reply_to_key,
        )?;
        job_ids.push(job.job_id);
    }
    Ok(job_ids)
}

fn partition_source_chunks_for_enqueue(
    source_id: &SourceId,
    author: &ActorId,
    chunks: &[ChunkInput],
    content_sha256: &ContentHash,
) -> Vec<(Vec<String>, ContentHash)> {
    if chunks.is_empty() {
        return Vec::new();
    }
    let slices: Vec<ExtractionChunkSlice> = chunks
        .iter()
        .enumerate()
        .map(|(ordinal, c)| ExtractionChunkSlice {
            chunk_id: format!("{source_id}-{ordinal}"),
            source_id: source_id.clone(),
            author: author.clone(),
            base_anchor: c.anchor.clone(),
            role: SliceRole::Target,
            reply_to_chunk_id: None,
            heading: c.heading.clone(),
            body: c.body.clone(),
        })
        .collect();
    let batches = partition_chunks_by_token_budget(&slices, DEFAULT_MAX_BATCH_TOKENS);
    if chunks.len() == 1 && batches.len() == 1 {
        return vec![(
            batches[0].iter().map(|s| s.chunk_id.clone()).collect(),
            content_sha256.clone(),
        )];
    }
    batches
        .into_iter()
        .enumerate()
        .map(|(batch_idx, batch)| {
            let chunk_ids: Vec<String> = batch.iter().map(|s| s.chunk_id.clone()).collect();
            let mut canonical = format!("{}:{batch_idx}", content_sha256.as_str());
            for s in &batch {
                canonical.push('\n');
                canonical.push_str(&s.chunk_id);
                canonical.push('\t');
                canonical.push_str(&s.heading);
                canonical.push('\n');
                canonical.push_str(&s.body);
            }
            let batch_sha = ContentHash::digest(canonical.as_bytes());
            (chunk_ids, batch_sha)
        })
        .collect()
}

enum SpeakerResolveSpec<'a> {
    Direct(&'a ActorId),
    Public(&'a PublicSpeaker),
}

struct GovernedWriteCall<'a> {
    caller: &'a ActorId,
    explicit_ingested_by: Option<&'a ActorId>,
    speaker_spec: SpeakerResolveSpec<'a>,
    external_source_key: String,
    uri: String,
    media_type: String,
    bytes: Vec<u8>,
    chunks: Vec<ChunkInput>,
    readable_by: AudienceSpec,
    source_scope_id: Option<SourceScopeId>,
    explicit_policy: Option<ModelRoutePolicy>,
    effective_policy: ModelRoutePolicy,
    op_kind: GovernedUpsertMode,
    thread_key: Option<String>,
    reply_to_key: Option<String>,
    captured_at: EpochMs,
    job_priority: i32,
    session_span: Option<SessionSpanSpec<'a>>,
}

struct ExistingPublicSourceRow {
    source_id: String,
    content_sha256: String,
    git_blob: String,
    ingested_by: Option<String>,
    scope_id: Option<String>,
}

struct SessionSpanSpec<'a> {
    session_id: &'a str,
    start_byte: u64,
    end_byte: u64,
    at: EpochMs,
    self_echo_filtered: bool,
}

impl Store {
    /// Applies a [`PublicContainerRoster`] atomically on behalf of `caller`:
    /// - Requires `caller` to hold `Owner` or `Maintainer` role (`StoreError::PermissionDenied` otherwise);
    /// - Enforces strictly monotonic `roster.synced_at` (`<` returns `StoreError::Conflict`; `==`
    ///   succeeds idempotently only if `current_audience` and redacted `label` match identically);
    /// - Redacts `roster.label` via [`SecretRedactor`] before persistence;
    /// - Resolves direct library `actor_id`s and confirmed `external_identities`, proposing unknown
    ///   `namespace:id` entries;
    /// - Updates `source_scopes` and reconciles pending community publishes.
    pub fn apply_public_container_roster(
        &mut self,
        caller: &ActorId,
        roster: &PublicContainerRoster,
    ) -> Result<SourceScope> {
        roster.validate()?;
        let redactor = shared_redactor()?;
        let clean_label = redactor.redact(roster.label.trim());

        let scope_id: SourceScopeId = roster.container_id.trim().parse()?;
        let provider = roster
            .provider
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                roster
                    .container_id
                    .trim()
                    .split(['.', ':'])
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("external")
                    .to_owned()
            });
        let external_container_id = roster
            .upstream_container_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| roster.container_id.trim())
            .to_owned();

        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let caller_role = require_active_member_role_tx(&tx, caller)?;
        if !matches!(caller_role, MemberRole::Owner | MemberRole::Maintainer) {
            return Err(StoreError::PermissionDenied);
        }

        let existing_scope: Option<(String, String, i64)> = tx
            .query_row(
                "SELECT label, current_audience_json, synced_at
                 FROM source_scopes WHERE scope_id = ?1",
                [scope_id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;

        if let Some((_, _, existing_synced_at)) = &existing_scope {
            if roster.synced_at.0 < *existing_synced_at {
                return Err(StoreError::Conflict);
            }
        }

        let mut resolved_actors = BTreeSet::<ActorId>::new();
        for raw_member in &roster.members {
            let trimmed = raw_member.trim();
            if let Some((ns, ext)) = trimmed.split_once(':') {
                if let Some(actor) =
                    resolve_or_propose_external_identity_tx(&tx, ns, ext, None, roster.synced_at)?
                {
                    resolved_actors.insert(actor);
                }
            } else {
                resolved_actors.insert(trimmed.parse()?);
            }
        }

        let current_audience = if roster.all_library_members {
            AudienceSpec::AllMembers
        } else {
            AudienceSpec::Explicit(resolved_actors)
        };
        let new_audience_json = serde_json::to_string(&current_audience)?;

        let scope = SourceScope {
            scope_id: scope_id.clone(),
            provider: provider.clone(),
            external_container_id: external_container_id.clone(),
            label: clean_label,
            current_audience,
            synced_at: roster.synced_at,
        };

        if let Some((existing_label, existing_aud_raw, existing_synced_at)) = existing_scope {
            if roster.synced_at.0 == existing_synced_at {
                let normalized_existing_aud =
                    serde_json::to_string(&serde_json::from_str::<AudienceSpec>(
                        &existing_aud_raw,
                    )?)?;
                if new_audience_json == normalized_existing_aud && scope.label == existing_label {
                    tx.commit()?;
                    return Ok(scope);
                }
                return Err(StoreError::Conflict);
            }
        }

        tx.execute(
            "INSERT INTO source_scopes (scope_id, provider, external_container_id, label, current_audience_json, synced_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(scope_id) DO UPDATE SET
                provider = excluded.provider,
                external_container_id = excluded.external_container_id,
                label = excluded.label,
                current_audience_json = excluded.current_audience_json,
                synced_at = excluded.synced_at",
            params![
                scope.scope_id.as_str(),
                &scope.provider,
                &scope.external_container_id,
                &scope.label,
                &new_audience_json,
                scope.synced_at.0,
            ],
        )?;

        tx.execute(
            "INSERT INTO change_log (op, target_id, actor_id, payload_json, created_at)
             VALUES ('container_roster_sync', ?1, ?2, ?3, ?4)",
            params![
                scope.scope_id.as_str(),
                caller.as_str(),
                serde_json::to_string(&json!({
                    "members": &roster.members,
                    "all_library_members": roster.all_library_members,
                }))?,
                roster.synced_at.0,
            ],
        )?;

        crate::exchange::reconcile_pending_publishes_for_scope_tx(&tx, self.mode, &scope.scope_id)?;
        crate::exchange::mark_alignment_dirty_tx(&tx, roster.synced_at)?;
        tx.commit()?;
        self.sync_skills_now()?;
        Ok(scope)
    }

    /// Applies one [`PublicIngestRecord`] (`Upsert` | `Supersede` | `Withdraw`) atomically.
    pub fn apply_public_ingest_record(
        &mut self,
        record: &PublicIngestRecord,
        default_ingester: &ActorId,
        global_policy: &ModelRoutePolicy,
    ) -> Result<PublicRecordApplyOutcome> {
        let (outcome, _, _) = self.apply_public_ingest_record_inner(
            record,
            default_ingester,
            global_policy,
            0,
            None,
        )?;
        Ok(outcome)
    }

    /// Atomically ingests a Hook session sub-batch through the exact same Public Ingest Contract
    /// identity (`LogicalKey("ingest:{kind}:{external_id}")`, `external_source_key = "{kind}:{external_id}"`),
    /// [`SecretRedactor`] scrubbing, caller authorization, `RawOnly` zero-job / zero-cloud-embedder
    /// gate, and `session_source_span` registration in one `Immediate` SQLite transaction.
    pub fn ingest_public_session_subbatch(
        &mut self,
        spec: &PublicSessionSubbatchSpec<'_>,
    ) -> Result<(StoredSource, bool)> {
        let PublicSessionSubbatchSpec {
            caller,
            record,
            global_policy,
            session_id,
            start_byte,
            end_byte,
            job_priority,
            self_echo_filtered,
            at,
        } = *spec;
        if !matches!(record.op, PublicIngestOp::Upsert) {
            return Err(KpError::invalid(
                "op",
                "session sub-batch ingestion requires `op = \"upsert\"`",
            )
            .into());
        }
        crate::queue::validate_session_span_args(session_id, start_byte, end_byte)?;
        let span = SessionSpanSpec {
            session_id,
            start_byte,
            end_byte,
            at,
            self_echo_filtered,
        };
        let (_, stored_opt, newly_ingested) = self.apply_public_ingest_record_inner(
            record,
            caller,
            global_policy,
            job_priority,
            Some(span),
        )?;
        let stored = stored_opt.ok_or(StoreError::NotFound)?;
        Ok((stored, newly_ingested))
    }

    /// Unified governed single-writer entrypoint for files, web pages, and connector streams:
    /// - Reuses `ingest:{external_source_key}` identity, duplicate-upsert policy narrowing, and
    ///   automatic or explicit `Supersede` (`retire_source_and_facts_tx` retiring prior active facts
    ///   and cancelling pending queue jobs when SHA changes under `GovernedUpsertMode::AutoSupersede`);
    /// - Partitions `chunks` at enqueue time via `partition_chunks_by_token_budget` (`<= 2,400` tokens)
    ///   so multi-chunk documents enqueue bounded `extraction_queue` jobs keyed by `chunk_batch_sha256`.
    pub fn upsert_governed_source(
        &mut self,
        input: GovernedSourceInput,
    ) -> Result<GovernedSourceOutcome> {
        let (outcome, _) = self.upsert_governed_source_with_facts(input, &mut [])?;
        Ok(outcome)
    }

    /// Variant of [`Store::upsert_governed_source`] that also persists deterministic `FactInput`s
    /// (stamped with `source_id`) in the same `Immediate` transaction when newly ingested or superseded.
    pub fn upsert_governed_source_with_facts(
        &mut self,
        input: GovernedSourceInput,
        deterministic_facts: &mut [FactInput],
    ) -> Result<(GovernedSourceOutcome, Remembered)> {
        let GovernedSourceInput {
            external_source_key,
            uri,
            media_type,
            bytes,
            chunks,
            speaker,
            ingested_by,
            readable_by,
            source_scope_id,
            effective_policy,
            op_kind,
            thread_key,
            reply_to_key,
            captured_at,
        } = input;
        let caller = ingested_by.clone();
        let spk = speaker;
        let (outcome, remembered, _) = self.execute_governed_source_write(
            GovernedWriteCall {
                caller: &caller,
                explicit_ingested_by: Some(&ingested_by),
                speaker_spec: SpeakerResolveSpec::Direct(&spk),
                external_source_key,
                uri,
                media_type,
                bytes,
                chunks,
                readable_by,
                source_scope_id,
                explicit_policy: None,
                effective_policy,
                op_kind,
                thread_key,
                reply_to_key,
                captured_at,
                job_priority: 0,
                session_span: None,
            },
            deterministic_facts,
        )?;
        Ok((outcome, remembered))
    }

    fn apply_public_ingest_record_inner(
        &mut self,
        record: &PublicIngestRecord,
        caller: &ActorId,
        global_policy: &ModelRoutePolicy,
        job_priority: i32,
        session_span: Option<SessionSpanSpec<'_>>,
    ) -> Result<(PublicRecordApplyOutcome, Option<StoredSource>, bool)> {
        record.validate()?;
        let redactor = shared_redactor()?;
        let kind_trimmed = record.source.kind.trim();
        let external_source_key = format!("{kind_trimmed}:{}", record.source.external_id.trim());
        let effective_policy = record
            .model_policy
            .as_ref()
            .unwrap_or(global_policy)
            .clone();
        let source_scope_id: Option<SourceScopeId> = record
            .source
            .container_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.parse::<SourceScopeId>())
            .transpose()?;
        let readable_by = match &record.audience {
            PublicAudience::ContainerMembers => AudienceSpec::AllMembers,
            PublicAudience::Explicit(actors) => {
                AudienceSpec::Explicit(actors.iter().cloned().collect())
            }
        };
        let op_kind = match &record.op {
            PublicIngestOp::Upsert => GovernedUpsertMode::StrictUpsert,
            PublicIngestOp::Supersede {
                previous_external_id,
            } => GovernedUpsertMode::Supersede {
                previous_external_source_key: format!(
                    "{kind_trimmed}:{}",
                    previous_external_id.trim()
                ),
            },
            PublicIngestOp::Withdraw => GovernedUpsertMode::Withdraw,
        };
        let redacted_content = redactor.redact(&record.content);
        let content_bytes = redacted_content.as_bytes().to_vec();
        let content_sha256 = ContentHash::digest(&content_bytes);
        let media_type = record
            .media_type
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .unwrap_or("text/plain")
            .to_owned();
        let redacted_uri = redactor.redact(record.source.uri.trim());
        let char_count = u32::try_from(redacted_content.chars().count()).ok();
        let chunks = if matches!(op_kind, GovernedUpsertMode::Withdraw) {
            Vec::new()
        } else {
            let byte_identical = is_byte_identical_media_type(&media_type);
            vec![ChunkInput {
                heading: redacted_uri.clone(),
                body: redacted_content,
                anchor: AnchorSpec {
                    start_byte: byte_identical.then_some(0),
                    end_byte: byte_identical
                        .then(|| u64::try_from(content_bytes.len()).ok())
                        .flatten(),
                    char_start: Some(0),
                    char_end: char_count,
                    body_hash: Some(content_sha256),
                    ..AnchorSpec::default()
                },
            }]
        };
        let reply_to_key = record
            .reply_to
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|rt| {
                let prefix = format!("{kind_trimmed}:");
                if rt.starts_with(&prefix) {
                    rt.to_owned()
                } else {
                    format!("{prefix}{rt}")
                }
            });
        let thread_key = record
            .thread_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);

        let (gov_out, _, newly_ingested) = self.execute_governed_source_write(
            GovernedWriteCall {
                caller,
                explicit_ingested_by: record.ingested_by.as_ref(),
                speaker_spec: SpeakerResolveSpec::Public(&record.speaker),
                external_source_key,
                uri: redacted_uri,
                media_type,
                bytes: content_bytes,
                chunks,
                readable_by,
                source_scope_id,
                explicit_policy: record.model_policy.clone(),
                effective_policy,
                op_kind,
                thread_key,
                reply_to_key,
                captured_at: record.occurred_at,
                job_priority,
                session_span,
            },
            &mut [],
        )?;
        let stored_opt = gov_out.source.clone();
        let source_id_for_outcome = match gov_out.kind {
            PublicRecordAppliedKind::DuplicateIdempotent if !newly_ingested => {
                // If already_withdrawn returned synthetic `stored`, `PublicRecordApplyOutcome.source_id` was `None`.
                if let Some(ref st) = stored_opt {
                    let exists: bool = self.db.query_row(
                        "SELECT EXISTS(SELECT 1 FROM raw_sources WHERE source_id = ?1)",
                        [st.source_id.as_str()],
                        |r| r.get(0),
                    )?;
                    exists.then(|| st.source_id.clone())
                } else {
                    None
                }
            }
            _ => stored_opt.as_ref().map(|s| s.source_id.clone()),
        };
        let pub_outcome = PublicRecordApplyOutcome {
            kind: gov_out.kind,
            external_source_key: gov_out.external_source_key,
            source_id: source_id_for_outcome,
            superseded_source_id: gov_out.superseded_source_id,
            affected_facts: gov_out.affected_facts,
            enqueued_job_id: gov_out.enqueued_job_ids.first().cloned(),
            resolved_speaker: gov_out.resolved_speaker,
        };
        Ok((pub_outcome, stored_opt, newly_ingested))
    }

    fn execute_governed_source_write(
        &mut self,
        call: GovernedWriteCall<'_>,
        deterministic_facts: &mut [FactInput],
    ) -> Result<(GovernedSourceOutcome, Remembered, bool)> {
        let GovernedWriteCall {
            caller,
            explicit_ingested_by,
            speaker_spec,
            external_source_key,
            uri,
            media_type,
            bytes,
            chunks,
            readable_by,
            source_scope_id,
            explicit_policy,
            effective_policy,
            op_kind,
            thread_key,
            reply_to_key,
            captured_at,
            job_priority,
            session_span,
        } = call;

        let external_source_key = external_source_key.trim().to_owned();
        if external_source_key.is_empty() {
            return Err(KpError::invalid(
                "external_source_key",
                "external_source_key must be nonempty",
            )
            .into());
        }
        if matches!(readable_by, AudienceSpec::SemiringDerived) {
            return Err(KpError::invalid(
                "readable_by",
                "governed sources require an explicit audience or all members",
            )
            .into());
        }
        let redactor = shared_redactor()?;
        let redacted_uri = redactor.redact(uri.trim());
        let clean_media_type = if media_type.trim().is_empty() {
            "text/plain".to_owned()
        } else {
            media_type.trim().to_owned()
        };
        let clean_thread_key = thread_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let clean_reply_key = reply_to_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);

        let pre_role = require_active_member_role_tx(&self.db, caller)?;
        let pre_is_admin = matches!(pre_role, MemberRole::Owner | MemberRole::Maintainer);
        let _ =
            resolve_effective_ingester_tx(&self.db, caller, pre_is_admin, explicit_ingested_by)?;

        let queue_policy_opt = route_policy_to_queue_str(&effective_policy)?;
        let is_raw_only = queue_policy_opt.is_none();

        if matches!(op_kind, GovernedUpsertMode::Withdraw) {
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let caller_role = require_active_member_role_tx(&tx, caller)?;
            let is_admin = matches!(caller_role, MemberRole::Owner | MemberRole::Maintainer);
            let ingester =
                resolve_effective_ingester_tx(&tx, caller, is_admin, explicit_ingested_by)?;
            let resolved_speaker = match speaker_spec {
                SpeakerResolveSpec::Direct(actor) => actor.clone(),
                SpeakerResolveSpec::Public(pub_spk) => {
                    resolve_speaker_tx(&tx, pub_spk, redactor, captured_at)?
                }
            };

            let existing_sources: Vec<(String, Option<String>, Option<String>)> = {
                let mut stmt = tx.prepare(
                    "SELECT source_id, ingested_by_actor, source_scope_id
                     FROM raw_sources
                     WHERE external_source_key = ?1
                     ORDER BY captured_at ASC, source_id ASC",
                )?;
                let rows = stmt.query_map([&external_source_key], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?;
                rows.collect::<std::result::Result<_, _>>()?
            };

            if existing_sources.is_empty() {
                enforce_caller_write_authorization_tx(
                    &tx,
                    caller,
                    is_admin,
                    &resolved_speaker,
                    &readable_by,
                    source_scope_id.as_ref(),
                )?;
                tx.execute(
                    "INSERT INTO change_log (op, logical_key, actor_id, created_at)
                     VALUES ('public_source_withdrawn', ?1, ?2, ?3)",
                    params![&external_source_key, ingester.as_str(), captured_at.0],
                )?;
                tx.commit()?;
                return Ok((
                    GovernedSourceOutcome {
                        kind: PublicRecordAppliedKind::NoopMissingTarget,
                        external_source_key,
                        source: None,
                        superseded_source_id: None,
                        affected_facts: Vec::new(),
                        enqueued_job_ids: Vec::new(),
                        resolved_speaker,
                    },
                    Remembered::default(),
                    false,
                ));
            }

            for (sid_str, ing_opt, scope_opt) in &existing_sources {
                verify_target_source_mutation_allowed_tx(
                    &tx,
                    caller,
                    is_admin,
                    sid_str,
                    ing_opt.as_deref(),
                    scope_opt.as_deref(),
                )?;
            }

            tx.execute(
                "INSERT INTO change_log (op, logical_key, actor_id, created_at)
                 VALUES ('public_source_withdrawn', ?1, ?2, ?3)",
                params![&external_source_key, ingester.as_str(), captured_at.0],
            )?;

            let mut all_affected = Vec::new();
            let mut primary_stored: Option<StoredSource> = None;
            for (sid_str, _, _) in existing_sources {
                if primary_stored.is_none() {
                    let sha_blob: Option<(String, String)> = tx
                        .query_row(
                            "SELECT content_sha256, git_blob_or_commit FROM raw_sources WHERE source_id = ?1",
                            [&sid_str],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .optional()?;
                    if let Some((sha, blob)) = sha_blob {
                        primary_stored = Some(StoredSource {
                            source_id: sid_str.parse()?,
                            content_sha256: sha.parse()?,
                            git_blob: blob,
                        });
                    }
                }
                let affected = retire_source_and_facts_tx(
                    &tx,
                    &sid_str,
                    "__withdrawn__",
                    "withdrawn",
                    "source_withdrawn",
                    &ingester,
                    captured_at,
                )?;
                all_affected.extend(affected);
            }
            crate::exchange::mark_alignment_dirty_tx(&tx, captured_at)?;
            tx.commit()?;
            if !all_affected.is_empty() {
                self.sync_skills_now()?;
            }
            return Ok((
                GovernedSourceOutcome {
                    kind: PublicRecordAppliedKind::Withdrawn,
                    external_source_key,
                    source: primary_stored,
                    superseded_source_id: None,
                    affected_facts: all_affected,
                    enqueued_job_ids: Vec::new(),
                    resolved_speaker,
                },
                Remembered::default(),
                false,
            ));
        }

        if !pre_is_admin {
            if !audience_includes_actor(&readable_by, caller) {
                return Err(StoreError::PermissionDenied);
            }
            if let Some(ref scope_id) = source_scope_id {
                if !scope_currently_includes_actor_tx(&self.db, scope_id, caller)? {
                    return Err(StoreError::PermissionDenied);
                }
            } else {
                let pre_same_speaker = match speaker_spec {
                    SpeakerResolveSpec::Direct(actor) => actor == caller,
                    SpeakerResolveSpec::Public(pub_spk) => {
                        let raw_spk = pub_spk.id.trim();
                        if let Some((ns, ext)) = raw_spk.split_once(':') {
                            let confirmed: Option<String> = self
                                .db
                                .query_row(
                                    "SELECT confirmed_actor_id FROM external_identities
                                     WHERE provider = ?1 AND external_id = ?2 AND status = 'confirmed'",
                                    params![ns.trim(), ext.trim()],
                                    |r| r.get::<_, Option<String>>(0),
                                )
                                .optional()?
                                .flatten();
                            confirmed.as_deref() == Some(caller.as_str())
                        } else {
                            raw_spk == caller.as_str()
                        }
                    }
                };
                if !pre_same_speaker {
                    return Err(StoreError::PermissionDenied);
                }
            }
        }

        let base_logical_key: LogicalKey = format!("ingest:{external_source_key}").parse()?;
        let base_opaque = self.tombstone_id(&self.library_id, &base_logical_key)?;
        guard(&self.db, &base_opaque)?;

        let byte_len = i64::try_from(bytes.len())
            .map_err(|_| KpError::invalid("content", "content exceeds SQLite length range"))?;
        let _ = i64::try_from(chunks.len())
            .map_err(|_| KpError::invalid("chunks", "too many chunks"))?;
        let content_sha256 = ContentHash::digest(&bytes);
        let blob_id = content_sha256.to_string();

        let store_embedder = self.embedder.clone();
        let embedder_allows_raw_only = store_embedder.allows_raw_only();
        let active_embedder: &dyn Embedder = if is_raw_only && !embedder_allows_raw_only {
            &FtsOnlyEmbedder
        } else {
            store_embedder.as_ref()
        };

        if let GovernedUpsertMode::Supersede {
            ref previous_external_source_key,
        } = op_kind
        {
            let pre_prev_rows: Vec<(String, Option<String>, Option<String>)> = {
                let mut stmt = self.db.prepare(
                    "SELECT source_id, ingested_by_actor, source_scope_id
                     FROM raw_sources
                     WHERE external_source_key = ?1
                     ORDER BY captured_at ASC, source_id ASC",
                )?;
                let rows = stmt.query_map([previous_external_source_key], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?;
                rows.collect::<std::result::Result<_, _>>()?
            };
            for (prev_sid, prev_ing, prev_scope) in &pre_prev_rows {
                verify_target_source_mutation_allowed_tx(
                    &self.db,
                    caller,
                    pre_is_admin,
                    prev_sid,
                    prev_ing.as_deref(),
                    prev_scope.as_deref(),
                )?;
            }
        }

        let is_auto_supersede = matches!(op_kind, GovernedUpsertMode::AutoSupersede);
        let pre_existing_sha: Option<String> = if is_auto_supersede {
            self.db
                .query_row(
                    "SELECT content_sha256 FROM raw_sources
                     WHERE external_source_key = ?1 AND superseded_by_source IS NULL
                     ORDER BY captured_at DESC LIMIT 1",
                    [&external_source_key],
                    |r| r.get(0),
                )
                .optional()?
        } else {
            self.db
                .query_row(
                    "SELECT content_sha256 FROM raw_sources
                     WHERE external_source_key = ?1
                     ORDER BY captured_at DESC LIMIT 1",
                    [&external_source_key],
                    |r| r.get(0),
                )
                .optional()?
        };

        if !is_auto_supersede {
            if let Some(ref existing_sha) = pre_existing_sha {
                if existing_sha != content_sha256.as_str() {
                    return Err(StoreError::Conflict);
                }
            }
        }

        let needs_new_write = match &pre_existing_sha {
            None => true,
            Some(sha) => is_auto_supersede && sha != content_sha256.as_str(),
        };
        let prepared_vectors = if needs_new_write {
            embed_chunk_texts(&chunks, active_embedder)?
        } else {
            None
        };

        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        guard(&tx, &base_opaque)?;
        let caller_role = require_active_member_role_tx(&tx, caller)?;
        let is_admin = matches!(caller_role, MemberRole::Owner | MemberRole::Maintainer);
        let ingester = resolve_effective_ingester_tx(&tx, caller, is_admin, explicit_ingested_by)?;
        let resolved_speaker = match speaker_spec {
            SpeakerResolveSpec::Direct(actor) => actor.clone(),
            SpeakerResolveSpec::Public(pub_spk) => {
                resolve_speaker_tx(&tx, pub_spk, redactor, captured_at)?
            }
        };
        enforce_caller_write_authorization_tx(
            &tx,
            caller,
            is_admin,
            &resolved_speaker,
            &readable_by,
            source_scope_id.as_ref(),
        )?;

        if let GovernedUpsertMode::Supersede {
            ref previous_external_source_key,
        } = op_kind
        {
            let prev_rows: Vec<(String, Option<String>, Option<String>)> = {
                let mut stmt = tx.prepare(
                    "SELECT source_id, ingested_by_actor, source_scope_id
                     FROM raw_sources
                     WHERE external_source_key = ?1
                     ORDER BY captured_at ASC, source_id ASC",
                )?;
                let rows = stmt.query_map([previous_external_source_key], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?;
                rows.collect::<std::result::Result<_, _>>()?
            };
            for (prev_sid, prev_ing, prev_scope) in &prev_rows {
                verify_target_source_mutation_allowed_tx(
                    &tx,
                    caller,
                    is_admin,
                    prev_sid,
                    prev_ing.as_deref(),
                    prev_scope.as_deref(),
                )?;
            }
        }

        if !is_auto_supersede {
            let already_withdrawn: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM change_log WHERE op = 'public_source_withdrawn' AND logical_key = ?1)",
                [&external_source_key],
                |r| r.get(0),
            )?;
            if already_withdrawn {
                let source_id = SourceId::new(base_opaque.as_str())?;
                if let Some(ref span) = session_span {
                    crate::queue::record_session_source_span_tx(
                        &tx,
                        &source_id,
                        span.session_id,
                        span.start_byte,
                        span.end_byte,
                        span.at,
                    )?;
                    let _ = crate::queue::complete_session_source_span_tx(&tx, &source_id)?;
                }
                tx.commit()?;
                let stored = StoredSource {
                    source_id,
                    content_sha256,
                    git_blob: blob_id,
                };
                return Ok((
                    GovernedSourceOutcome {
                        kind: PublicRecordAppliedKind::DuplicateIdempotent,
                        external_source_key,
                        source: Some(stored),
                        superseded_source_id: None,
                        affected_facts: Vec::new(),
                        enqueued_job_ids: Vec::new(),
                        resolved_speaker,
                    },
                    Remembered::default(),
                    false,
                ));
            }
        }

        let existing_in_tx: Option<ExistingPublicSourceRow> = if is_auto_supersede {
            tx.query_row(
                "SELECT source_id, content_sha256, git_blob_or_commit, ingested_by_actor, source_scope_id
                 FROM raw_sources
                 WHERE external_source_key = ?1 AND superseded_by_source IS NULL
                 ORDER BY captured_at DESC LIMIT 1",
                [&external_source_key],
                |r| {
                    Ok(ExistingPublicSourceRow {
                        source_id: r.get(0)?,
                        content_sha256: r.get(1)?,
                        git_blob: r.get(2)?,
                        ingested_by: r.get(3)?,
                        scope_id: r.get(4)?,
                    })
                },
            )
            .optional()?
        } else {
            tx.query_row(
                "SELECT source_id, content_sha256, git_blob_or_commit, ingested_by_actor, source_scope_id
                 FROM raw_sources
                 WHERE external_source_key = ?1
                 ORDER BY captured_at DESC LIMIT 1",
                [&external_source_key],
                |r| {
                    Ok(ExistingPublicSourceRow {
                        source_id: r.get(0)?,
                        content_sha256: r.get(1)?,
                        git_blob: r.get(2)?,
                        ingested_by: r.get(3)?,
                        scope_id: r.get(4)?,
                    })
                },
            )
            .optional()?
        };

        let mut auto_retire_targets: Vec<String> = Vec::new();
        if let Some(existing_row) = existing_in_tx {
            let existing_sid_str = existing_row.source_id;
            let existing_sha = existing_row.content_sha256;
            let existing_git_blob = existing_row.git_blob;
            let existing_ingested_by = existing_row.ingested_by;
            let existing_scope_id = existing_row.scope_id;

            if existing_sha == content_sha256.as_str() {
                if source_scope_id.as_ref().map(SourceScopeId::as_str)
                    != existing_scope_id.as_deref()
                {
                    return Err(StoreError::Conflict);
                }
                if let Some(orig_meta) = load_initial_public_source_meta_tx(&tx, &existing_sid_str)?
                {
                    if let Some(ref orig_spk) = orig_meta.speaker {
                        if resolved_speaker.as_str() != orig_spk.as_str() {
                            return Err(StoreError::Conflict);
                        }
                    }
                }
                let existing_sid: SourceId = existing_sid_str.parse()?;
                let partitioned = partition_source_chunks_for_enqueue(
                    &existing_sid,
                    &resolved_speaker,
                    &chunks,
                    &content_sha256,
                );
                let caller_owns_target = verify_target_source_mutation_allowed_tx(
                    &tx,
                    caller,
                    is_admin,
                    &existing_sid_str,
                    existing_ingested_by.as_deref(),
                    existing_scope_id.as_deref(),
                )
                .is_ok();
                let enqueued_job_ids = if caller_owns_target {
                    handle_duplicate_upsert_policy_tx(
                        &tx,
                        &DuplicateUpsertPolicySpec {
                            existing_sid: &existing_sid,
                            external_source_key: &external_source_key,
                            partitioned_batches: &partitioned,
                            resolved_speaker: &resolved_speaker,
                            explicit_policy: explicit_policy.as_ref(),
                            queue_policy_opt: queue_policy_opt.as_deref(),
                            job_priority,
                            thread_key: clean_thread_key.as_deref(),
                            reply_to_key: clean_reply_key.as_deref(),
                            occurred_at: captured_at,
                            embedder_allows_raw_only,
                        },
                    )?
                } else {
                    let was_raw_only = is_source_marked_raw_only(&tx, &existing_sid_str)?;
                    if explicit_policy.is_some()
                        || (is_raw_only && !was_raw_only)
                        || matches!(op_kind, GovernedUpsertMode::Supersede { .. })
                    {
                        return Err(StoreError::PermissionDenied);
                    }
                    Vec::new()
                };
                if let Some(ref span) = session_span {
                    crate::queue::record_session_source_span_tx(
                        &tx,
                        &existing_sid,
                        span.session_id,
                        span.start_byte,
                        span.end_byte,
                        span.at,
                    )?;
                    let _ = crate::queue::complete_session_source_span_tx(&tx, &existing_sid)?;
                }
                tx.commit()?;
                let stored = StoredSource {
                    source_id: existing_sid,
                    content_sha256,
                    git_blob: existing_git_blob,
                };
                return Ok((
                    GovernedSourceOutcome {
                        kind: PublicRecordAppliedKind::DuplicateIdempotent,
                        external_source_key,
                        source: Some(stored),
                        superseded_source_id: None,
                        affected_facts: Vec::new(),
                        enqueued_job_ids,
                        resolved_speaker,
                    },
                    Remembered::default(),
                    false,
                ));
            } else if !is_auto_supersede {
                return Err(StoreError::Conflict);
            } else {
                let active_rows: Vec<(String, Option<String>, Option<String>)> = {
                    let mut stmt = tx.prepare(
                        "SELECT source_id, ingested_by_actor, source_scope_id
                         FROM raw_sources
                         WHERE external_source_key = ?1 AND superseded_by_source IS NULL
                         ORDER BY captured_at ASC, source_id ASC",
                    )?;
                    let rows = stmt.query_map([&external_source_key], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                    })?;
                    rows.collect::<std::result::Result<_, _>>()?
                };
                for (prev_sid, prev_ing, prev_scope) in active_rows {
                    verify_target_source_mutation_allowed_tx(
                        &tx,
                        caller,
                        is_admin,
                        &prev_sid,
                        prev_ing.as_deref(),
                        prev_scope.as_deref(),
                    )?;
                    auto_retire_targets.push(prev_sid);
                }
            }
        }

        let prior_row_count: usize = tx.query_row(
            "SELECT COUNT(*) FROM raw_sources WHERE external_source_key = ?1",
            [&external_source_key],
            |r| r.get(0),
        )?;
        let logical_key: LogicalKey = if prior_row_count == 0 {
            base_logical_key
        } else if is_auto_supersede {
            format!("ingest:{external_source_key}@rev{prior_row_count}").parse()?
        } else {
            return Err(StoreError::Conflict);
        };
        let opaque = crate::tombstone::tombstone_id(
            self.library_id.as_str().as_bytes(),
            &self.library_id,
            &logical_key,
        )?;
        guard(&tx, &opaque)?;
        let source_id = SourceId::new(opaque.as_str())?;

        for item in deterministic_facts.iter_mut() {
            item.fact
                .source_anchor
                .get_or_insert_with(AnchorSpec::default)
                .source_id = Some(source_id.clone());
        }
        crate::slots::prevalidate_fact_inputs(&tx, deterministic_facts, active_embedder)?;

        let vectors = match prepared_vectors {
            Some(vs) => Some(vs),
            None => embed_chunk_texts(&chunks, active_embedder)?,
        };

        tx.execute(
            "INSERT OR IGNORE INTO raw_blobs (content_sha256, byte_len, content_blob, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![content_sha256.as_str(), byte_len, &bytes, captured_at.0],
        )?;

        let audience_json = serde_json::to_string(&readable_by)?;
        let self_echo_flag = session_span.as_ref().is_some_and(|s| s.self_echo_filtered);
        tx.execute(
            "INSERT INTO raw_sources
                (source_id, uri, media_type, content_sha256, git_blob_or_commit, byte_len,
                 visibility_scope, captured_at, self_echo_filtered, ingested_by_actor,
                 source_scope_id, external_source_key, superseded_by_source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, NULL)",
            params![
                source_id.as_str(),
                &redacted_uri,
                &clean_media_type,
                content_sha256.as_str(),
                &blob_id,
                byte_len,
                &audience_json,
                captured_at.0,
                self_echo_flag,
                ingester.as_str(),
                source_scope_id.as_ref().map(SourceScopeId::as_str),
                &external_source_key,
            ],
        )?;

        let model_id = active_embedder.model_id();
        let segmented_title = self.tokenizer.segment_text(&redacted_uri);
        for (ordinal, chunk_input) in chunks.iter().enumerate() {
            let ordinal_i64 = i64::try_from(ordinal)
                .map_err(|_| KpError::invalid("chunks", "too many chunks"))?;
            let chunk_id = format!("{source_id}-{ordinal_i64}");
            let mut anchor = chunk_input.anchor.clone();
            anchor.source_id = Some(source_id.clone());
            anchor.chunk_id = Some(chunk_id.clone());
            if anchor.body_hash.is_none() {
                anchor.body_hash = Some(ContentHash::digest(&chunk_input.body));
            }
            let segmented = self.tokenizer.segment_text(&chunk_input.body);
            let fts_rowid: i64 = tx.query_row(
                "INSERT INTO chunks
                    (chunk_id, source_id, source_sha256, ordinal, heading, body, segmented_body,
                     anchor_json, readable_by_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) RETURNING fts_rowid",
                params![
                    &chunk_id,
                    source_id.as_str(),
                    content_sha256.as_str(),
                    ordinal_i64,
                    &chunk_input.heading,
                    &chunk_input.body,
                    &segmented,
                    serde_json::to_string(&anchor)?,
                    &audience_json,
                    captured_at.0,
                ],
                |row| row.get(0),
            )?;

            if let Some(Some(vector)) = vectors.as_ref().and_then(|vs| vs.get(ordinal)) {
                guard_and_lock_tx_embedding_dims(&tx, model_id, vector.len())?;
                let blob = encode_vector_le_bytes(vector);
                tx.execute(
                    "INSERT INTO chunk_vectors (chunk_id, dims, vec_blob, model_id)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![&chunk_id, vector.len(), blob, model_id],
                )?;
            }

            tx.execute(
                "INSERT INTO chunks_fts (rowid, title, heading, segmented_body) VALUES (?1, ?2, ?3, ?4)",
                params![
                    fts_rowid,
                    &segmented_title,
                    self.tokenizer.segment_text(&chunk_input.heading),
                    segmented,
                ],
            )?;
        }

        let remembered = if deterministic_facts.is_empty() {
            Remembered::default()
        } else {
            crate::slots::remember_facts_indexed(
                &tx,
                deterministic_facts,
                &self.tokenizer,
                active_embedder,
            )?
        };

        tx.execute(
            "INSERT INTO change_log (op, target_id, origin_library_id, logical_key, content_sha256, actor_id, created_at)
             VALUES ('ingest_source', ?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                source_id.as_str(),
                self.library_id.as_str(),
                logical_key.as_str(),
                content_sha256.as_str(),
                ingester.as_str(),
                captured_at.0,
            ],
        )?;

        let meta_payload = StoredPublicSourceMeta {
            explicit_policy: explicit_policy.clone(),
            effective_policy: Some(effective_policy),
            speaker: Some(resolved_speaker.to_string()),
        };
        tx.execute(
            "INSERT INTO change_log (op, target_id, logical_key, payload_json, created_at)
             VALUES ('public_source_meta', ?1, ?2, ?3, ?4)",
            params![
                source_id.as_str(),
                &external_source_key,
                serde_json::to_string(&meta_payload)?,
                captured_at.0,
            ],
        )?;

        if is_raw_only {
            tx.execute(
                "INSERT INTO change_log (op, target_id, created_at) VALUES ('raw_only_source', ?1, ?2)",
                params![source_id.as_str(), captured_at.0],
            )?;
        }

        let mut enqueued_job_ids = Vec::new();
        if let Some(policy_str) = queue_policy_opt {
            let partitioned = partition_source_chunks_for_enqueue(
                &source_id,
                &resolved_speaker,
                &chunks,
                &content_sha256,
            );
            for (batch_chunk_ids, batch_sha) in partitioned {
                let job = enqueue_or_rearm_extraction_job_tx(
                    &tx,
                    &source_id,
                    &ExtractionJobInput {
                        source_id: source_id.clone(),
                        chunk_ids: batch_chunk_ids,
                        chunk_batch_sha256: batch_sha,
                        author: resolved_speaker.clone(),
                        model_policy: policy_str.clone(),
                        priority: job_priority,
                        reference_time: captured_at,
                        created_at: captured_at,
                    },
                    clean_thread_key.as_deref(),
                    clean_reply_key.as_deref(),
                )?;
                enqueued_job_ids.push(job.job_id);
            }
        }

        if let Some(ref span) = session_span {
            crate::queue::record_session_source_span_tx(
                &tx,
                &source_id,
                span.session_id,
                span.start_byte,
                span.end_byte,
                span.at,
            )?;
            let _ = crate::queue::complete_session_source_span_tx(&tx, &source_id)?;
        }

        let mut superseded_source_id: Option<SourceId> = None;
        let mut affected_facts = Vec::new();
        let final_kind = match op_kind {
            GovernedUpsertMode::StrictUpsert => PublicRecordAppliedKind::Upserted,
            GovernedUpsertMode::AutoSupersede => {
                if auto_retire_targets.is_empty() {
                    PublicRecordAppliedKind::Upserted
                } else {
                    for prev_sid in auto_retire_targets {
                        if superseded_source_id.is_none() {
                            superseded_source_id = Some(prev_sid.parse()?);
                        }
                        let retired = retire_source_and_facts_tx(
                            &tx,
                            &prev_sid,
                            source_id.as_str(),
                            "superseded",
                            "source_superseded",
                            &ingester,
                            captured_at,
                        )?;
                        affected_facts.extend(retired);
                    }
                    crate::exchange::mark_alignment_dirty_tx(&tx, captured_at)?;
                    PublicRecordAppliedKind::Superseded
                }
            }
            GovernedUpsertMode::Supersede {
                previous_external_source_key,
            } => {
                let prev_sources: Vec<String> = {
                    let mut stmt = tx.prepare(
                        "SELECT source_id FROM raw_sources
                         WHERE external_source_key = ?1
                         ORDER BY captured_at ASC, source_id ASC",
                    )?;
                    let rows =
                        stmt.query_map([&previous_external_source_key], |r| r.get::<_, String>(0))?;
                    rows.collect::<std::result::Result<_, _>>()?
                };
                for prev_sid in prev_sources {
                    if superseded_source_id.is_none() {
                        superseded_source_id = Some(prev_sid.parse()?);
                    }
                    let retired = retire_source_and_facts_tx(
                        &tx,
                        &prev_sid,
                        source_id.as_str(),
                        "superseded",
                        "source_superseded",
                        &ingester,
                        captured_at,
                    )?;
                    affected_facts.extend(retired);
                }
                crate::exchange::mark_alignment_dirty_tx(&tx, captured_at)?;
                PublicRecordAppliedKind::Superseded
            }
            GovernedUpsertMode::Withdraw => PublicRecordAppliedKind::Withdrawn,
        };

        tx.commit()?;
        if !deterministic_facts.is_empty() {
            self.register_entities(deterministic_facts);
        }
        if !affected_facts.is_empty()
            || !remembered.facts.is_empty()
            || !remembered.superseded.is_empty()
        {
            self.sync_skills_now()?;
        }

        let stored = StoredSource {
            source_id,
            content_sha256,
            git_blob: blob_id,
        };
        Ok((
            GovernedSourceOutcome {
                kind: final_kind,
                external_source_key,
                source: Some(stored),
                superseded_source_id,
                affected_facts,
                enqueued_job_ids,
                resolved_speaker,
            },
            remembered,
            true,
        ))
    }

    /// Parses and applies an entire multi-line public JSONL stream sequentially.
    pub fn apply_public_ingest_jsonl(
        &mut self,
        jsonl: &str,
        default_ingester: &ActorId,
        global_policy: &ModelRoutePolicy,
    ) -> Result<PublicIngestStreamReport> {
        let lines = parse_public_jsonl_str(jsonl)?;
        let mut report = PublicIngestStreamReport::default();
        for item in lines {
            match item {
                PublicIngestLine::ContainerRoster(roster) => {
                    let _ = self.apply_public_container_roster(default_ingester, &roster)?;
                    report.rosters_applied += 1;
                }
                PublicIngestLine::Record(rec) => {
                    let outcome =
                        self.apply_public_ingest_record(&rec, default_ingester, global_policy)?;
                    if outcome.enqueued_job_id.is_some()
                        && matches!(
                            outcome.kind,
                            PublicRecordAppliedKind::Upserted | PublicRecordAppliedKind::Superseded
                        )
                    {
                        report.jobs_enqueued += 1;
                    }
                    if let Some(sid) = outcome.source_id {
                        if matches!(
                            outcome.kind,
                            PublicRecordAppliedKind::Upserted | PublicRecordAppliedKind::Superseded
                        ) {
                            report.source_ids.push(sid);
                        }
                    }
                    match outcome.kind {
                        PublicRecordAppliedKind::Upserted => report.records_upserted += 1,
                        PublicRecordAppliedKind::DuplicateIdempotent => {
                            report.records_duplicate += 1
                        }
                        PublicRecordAppliedKind::Superseded => report.records_superseded += 1,
                        PublicRecordAppliedKind::Withdrawn
                        | PublicRecordAppliedKind::NoopMissingTarget => {
                            report.records_withdrawn += 1
                        }
                    }
                }
            }
        }
        Ok(report)
    }
}
