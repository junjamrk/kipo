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
    parse_public_jsonl_str, ActorId, AnchorSpec, AudienceSpec, ContentHash, EpochMs, FactId,
    KpError, LogicalKey, MemberRole, ModelRoutePolicy, PublicAudience, PublicContainerRoster,
    PublicIngestLine, PublicIngestOp, PublicIngestRecord, PublicSpeaker, SecretRedactor, SourceId,
    SourceScope, SourceScopeId,
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
use crate::{Embedder, Result, Store, StoreError};

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
             WHERE json_extract(source_anchor_json, '$.source_id') = ?1
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
    content_sha256: &'a ContentHash,
    resolved_speaker: &'a ActorId,
    explicit_policy: Option<&'a ModelRoutePolicy>,
    queue_policy_opt: Option<&'a str>,
    job_priority: i32,
    occurred_at: EpochMs,
    embedder_allows_raw_only: bool,
}

fn handle_duplicate_upsert_policy_tx(
    tx: &Connection,
    spec: &DuplicateUpsertPolicySpec<'_>,
) -> Result<Option<String>> {
    let DuplicateUpsertPolicySpec {
        existing_sid,
        external_source_key,
        content_sha256,
        resolved_speaker,
        explicit_policy,
        queue_policy_opt,
        job_priority,
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
        return Ok(None);
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
        return Ok(None);
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
        return Ok(None);
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
    let chunk_id = format!("{existing_sid}-0");
    let job = enqueue_or_rearm_extraction_job_tx(
        tx,
        existing_sid,
        &ExtractionJobInput {
            source_id: existing_sid.clone(),
            chunk_ids: vec![chunk_id],
            chunk_batch_sha256: content_sha256.clone(),
            author: resolved_speaker.clone(),
            model_policy: effective_job_policy,
            priority: job_priority,
            reference_time: occurred_at,
            created_at: occurred_at,
        },
    )?;
    Ok(Some(job.job_id))
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

        // Pre-check caller authentication and explicit `ingested_by` before any external embedder call.
        let pre_role = require_active_member_role_tx(&self.db, caller)?;
        let pre_is_admin = matches!(pre_role, MemberRole::Owner | MemberRole::Maintainer);
        let _ = resolve_effective_ingester_tx(
            &self.db,
            caller,
            pre_is_admin,
            record.ingested_by.as_ref(),
        )?;

        let external_source_key = format!(
            "{}:{}",
            record.source.kind.trim(),
            record.source.external_id.trim()
        );
        let effective_policy = record.model_policy.as_ref().unwrap_or(global_policy);
        let queue_policy_opt = route_policy_to_queue_str(effective_policy)?;
        let is_raw_only = queue_policy_opt.is_none();

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

        if matches!(record.op, PublicIngestOp::Withdraw) {
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let caller_role = require_active_member_role_tx(&tx, caller)?;
            let is_admin = matches!(caller_role, MemberRole::Owner | MemberRole::Maintainer);
            let ingester =
                resolve_effective_ingester_tx(&tx, caller, is_admin, record.ingested_by.as_ref())?;
            let resolved_speaker =
                resolve_speaker_tx(&tx, &record.speaker, redactor, record.occurred_at)?;

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
                    params![
                        &external_source_key,
                        ingester.as_str(),
                        record.occurred_at.0
                    ],
                )?;
                tx.commit()?;
                return Ok((
                    PublicRecordApplyOutcome {
                        kind: PublicRecordAppliedKind::NoopMissingTarget,
                        external_source_key,
                        source_id: None,
                        superseded_source_id: None,
                        affected_facts: Vec::new(),
                        enqueued_job_id: None,
                        resolved_speaker,
                    },
                    None,
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
                params![
                    &external_source_key,
                    ingester.as_str(),
                    record.occurred_at.0
                ],
            )?;

            let mut all_affected = Vec::new();
            let mut primary_source_id: Option<SourceId> = None;
            for (sid_str, _, _) in existing_sources {
                if primary_source_id.is_none() {
                    primary_source_id = Some(sid_str.parse()?);
                }
                let affected = retire_source_and_facts_tx(
                    &tx,
                    &sid_str,
                    "__withdrawn__",
                    "withdrawn",
                    "source_withdrawn",
                    &ingester,
                    record.occurred_at,
                )?;
                all_affected.extend(affected);
            }
            crate::exchange::mark_alignment_dirty_tx(&tx, record.occurred_at)?;
            tx.commit()?;
            if !all_affected.is_empty() {
                self.sync_skills_now()?;
            }
            return Ok((
                PublicRecordApplyOutcome {
                    kind: PublicRecordAppliedKind::Withdrawn,
                    external_source_key,
                    source_id: primary_source_id,
                    superseded_source_id: None,
                    affected_facts: all_affected,
                    enqueued_job_id: None,
                    resolved_speaker,
                },
                None,
                false,
            ));
        }

        // Pre-validate caller authorization before embedding so unauthorized callers never hit cloud APIs.
        if !pre_is_admin {
            if !audience_includes_actor(&readable_by, caller) {
                return Err(StoreError::PermissionDenied);
            }
            if let Some(ref scope_id) = source_scope_id {
                if !scope_currently_includes_actor_tx(&self.db, scope_id, caller)? {
                    return Err(StoreError::PermissionDenied);
                }
            } else {
                let raw_spk = record.speaker.id.trim();
                let pre_same_speaker = if let Some((ns, ext)) = raw_spk.split_once(':') {
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
                };
                if !pre_same_speaker {
                    return Err(StoreError::PermissionDenied);
                }
            }
        }

        let logical_key: LogicalKey = format!("ingest:{external_source_key}").parse()?;
        let opaque = self.tombstone_id(&self.library_id, &logical_key)?;
        let source_id = SourceId::new(opaque.as_str())?;
        guard(&self.db, &opaque)?;

        let redacted_content = redactor.redact(&record.content);
        let content_bytes = redacted_content.as_bytes().to_vec();
        let byte_len = i64::try_from(content_bytes.len())
            .map_err(|_| KpError::invalid("content", "content exceeds SQLite length range"))?;
        let content_sha256 = ContentHash::digest(&content_bytes);
        let blob_id = content_sha256.to_string();
        let media_type = record
            .media_type
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .unwrap_or("text/plain")
            .to_owned();

        let redacted_uri = redactor.redact(record.source.uri.trim());
        let char_count = u32::try_from(redacted_content.chars().count()).ok();
        let chunk_input = ChunkInput {
            heading: redacted_uri.clone(),
            body: redacted_content,
            anchor: AnchorSpec {
                source_id: Some(source_id.clone()),
                start_byte: Some(0),
                end_byte: u64::try_from(content_bytes.len()).ok(),
                char_start: Some(0),
                char_end: char_count,
                body_hash: Some(content_sha256.clone()),
                ..AnchorSpec::default()
            },
        };

        let store_embedder = self.embedder.clone();
        let embedder_allows_raw_only = store_embedder.allows_raw_only();
        let active_embedder: &dyn Embedder = if is_raw_only && !embedder_allows_raw_only {
            &FtsOnlyEmbedder
        } else {
            store_embedder.as_ref()
        };

        // Pre-validate `Supersede` target source ownership BEFORE invoking any external `Embedder`.
        if let PublicIngestOp::Supersede {
            previous_external_id,
        } = &record.op
        {
            let prev_ext_key = format!(
                "{}:{}",
                record.source.kind.trim(),
                previous_external_id.trim()
            );
            let pre_prev_rows: Vec<(String, Option<String>, Option<String>)> = {
                let mut stmt = self.db.prepare(
                    "SELECT source_id, ingested_by_actor, source_scope_id
                     FROM raw_sources
                     WHERE external_source_key = ?1
                     ORDER BY captured_at ASC, source_id ASC",
                )?;
                let rows =
                    stmt.query_map([&prev_ext_key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
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

        let pre_existing_sha: Option<String> = self
            .db
            .query_row(
                "SELECT content_sha256 FROM raw_sources WHERE external_source_key = ?1 ORDER BY captured_at DESC LIMIT 1",
                [&external_source_key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(ref existing_sha) = pre_existing_sha {
            if existing_sha != content_sha256.as_str() {
                return Err(StoreError::Conflict);
            }
        }

        let prepared_vectors = if pre_existing_sha.is_none() {
            embed_chunk_texts(std::slice::from_ref(&chunk_input), active_embedder)?
        } else {
            None
        };

        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        guard(&tx, &opaque)?;
        let caller_role = require_active_member_role_tx(&tx, caller)?;
        let is_admin = matches!(caller_role, MemberRole::Owner | MemberRole::Maintainer);
        let ingester =
            resolve_effective_ingester_tx(&tx, caller, is_admin, record.ingested_by.as_ref())?;
        let resolved_speaker =
            resolve_speaker_tx(&tx, &record.speaker, redactor, record.occurred_at)?;
        enforce_caller_write_authorization_tx(
            &tx,
            caller,
            is_admin,
            &resolved_speaker,
            &readable_by,
            source_scope_id.as_ref(),
        )?;

        if let PublicIngestOp::Supersede {
            previous_external_id,
        } = &record.op
        {
            let prev_ext_key = format!(
                "{}:{}",
                record.source.kind.trim(),
                previous_external_id.trim()
            );
            let prev_rows: Vec<(String, Option<String>, Option<String>)> = {
                let mut stmt = tx.prepare(
                    "SELECT source_id, ingested_by_actor, source_scope_id
                     FROM raw_sources
                     WHERE external_source_key = ?1
                     ORDER BY captured_at ASC, source_id ASC",
                )?;
                let rows =
                    stmt.query_map([&prev_ext_key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
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

        let already_withdrawn: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM change_log WHERE op = 'public_source_withdrawn' AND logical_key = ?1)",
            [&external_source_key],
            |r| r.get(0),
        )?;
        if already_withdrawn {
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
                PublicRecordApplyOutcome {
                    kind: PublicRecordAppliedKind::DuplicateIdempotent,
                    external_source_key,
                    source_id: None,
                    superseded_source_id: None,
                    affected_facts: Vec::new(),
                    enqueued_job_id: None,
                    resolved_speaker,
                },
                Some(stored),
                false,
            ));
        }

        let existing_in_tx: Option<ExistingPublicSourceRow> = tx
            .query_row(
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
            .optional()?;

        if let Some(existing_row) = existing_in_tx {
            let existing_sid_str = existing_row.source_id;
            let existing_sha = existing_row.content_sha256;
            let existing_git_blob = existing_row.git_blob;
            let existing_ingested_by = existing_row.ingested_by;
            let existing_scope_id = existing_row.scope_id;
            if existing_sha != content_sha256.as_str() {
                return Err(StoreError::Conflict);
            }
            if source_scope_id.as_ref().map(SourceScopeId::as_str) != existing_scope_id.as_deref() {
                return Err(StoreError::Conflict);
            }
            if let Some(orig_meta) = load_initial_public_source_meta_tx(&tx, &existing_sid_str)? {
                if let Some(ref orig_spk) = orig_meta.speaker {
                    if resolved_speaker.as_str() != orig_spk.as_str() {
                        return Err(StoreError::Conflict);
                    }
                }
            }
            let existing_sid: SourceId = existing_sid_str.parse()?;
            let caller_owns_target = verify_target_source_mutation_allowed_tx(
                &tx,
                caller,
                is_admin,
                &existing_sid_str,
                existing_ingested_by.as_deref(),
                existing_scope_id.as_deref(),
            )
            .is_ok();
            let enqueued_job_id = if caller_owns_target {
                handle_duplicate_upsert_policy_tx(
                    &tx,
                    &DuplicateUpsertPolicySpec {
                        existing_sid: &existing_sid,
                        external_source_key: &external_source_key,
                        content_sha256: &content_sha256,
                        resolved_speaker: &resolved_speaker,
                        explicit_policy: record.model_policy.as_ref(),
                        queue_policy_opt: queue_policy_opt.as_deref(),
                        job_priority,
                        occurred_at: record.occurred_at,
                        embedder_allows_raw_only,
                    },
                )?
            } else {
                // Non-owner peer in the same container may not mutate `model_policy` or forge jobs.
                let was_raw_only = is_source_marked_raw_only(&tx, &existing_sid_str)?;
                if record.model_policy.is_some()
                    || (is_raw_only && !was_raw_only)
                    || matches!(record.op, PublicIngestOp::Supersede { .. })
                {
                    return Err(StoreError::PermissionDenied);
                }
                None
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
                source_id: existing_sid.clone(),
                content_sha256,
                git_blob: existing_git_blob,
            };
            return Ok((
                PublicRecordApplyOutcome {
                    kind: PublicRecordAppliedKind::DuplicateIdempotent,
                    external_source_key,
                    source_id: Some(existing_sid),
                    superseded_source_id: None,
                    affected_facts: Vec::new(),
                    enqueued_job_id,
                    resolved_speaker,
                },
                Some(stored),
                false,
            ));
        }

        let vectors = match prepared_vectors {
            Some(vs) => Some(vs),
            None => embed_chunk_texts(std::slice::from_ref(&chunk_input), active_embedder)?,
        };

        tx.execute(
            "INSERT OR IGNORE INTO raw_blobs (content_sha256, byte_len, content_blob, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                content_sha256.as_str(),
                byte_len,
                &content_bytes,
                record.occurred_at.0
            ],
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
                &media_type,
                content_sha256.as_str(),
                &blob_id,
                byte_len,
                &audience_json,
                record.occurred_at.0,
                self_echo_flag,
                ingester.as_str(),
                source_scope_id.as_ref().map(SourceScopeId::as_str),
                &external_source_key,
            ],
        )?;

        let chunk_id = format!("{source_id}-0");
        let mut anchor = chunk_input.anchor.clone();
        anchor.chunk_id = Some(chunk_id.clone());
        let segmented = self.tokenizer.segment_text(&chunk_input.body);
        let fts_rowid: i64 = tx.query_row(
            "INSERT INTO chunks
                (chunk_id, source_id, source_sha256, ordinal, heading, body, segmented_body,
                 anchor_json, readable_by_json, created_at)
             VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6, ?7, ?8, ?9) RETURNING fts_rowid",
            params![
                &chunk_id,
                source_id.as_str(),
                content_sha256.as_str(),
                &chunk_input.heading,
                &chunk_input.body,
                &segmented,
                serde_json::to_string(&anchor)?,
                &audience_json,
                record.occurred_at.0,
            ],
            |row| row.get(0),
        )?;

        let model_id = active_embedder.model_id();
        if let Some(Some(vector)) = vectors.as_ref().and_then(|vs| vs.first()) {
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
                self.tokenizer.segment_text(&redacted_uri),
                self.tokenizer.segment_text(&chunk_input.heading),
                segmented,
            ],
        )?;

        tx.execute(
            "INSERT INTO change_log (op, target_id, origin_library_id, logical_key, content_sha256, actor_id, created_at)
             VALUES ('ingest_source', ?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                source_id.as_str(),
                self.library_id.as_str(),
                logical_key.as_str(),
                content_sha256.as_str(),
                ingester.as_str(),
                record.occurred_at.0,
            ],
        )?;

        let meta_payload = StoredPublicSourceMeta {
            explicit_policy: record.model_policy.clone(),
            effective_policy: Some(effective_policy.clone()),
            speaker: Some(resolved_speaker.to_string()),
        };
        tx.execute(
            "INSERT INTO change_log (op, target_id, logical_key, payload_json, created_at)
             VALUES ('public_source_meta', ?1, ?2, ?3, ?4)",
            params![
                source_id.as_str(),
                &external_source_key,
                serde_json::to_string(&meta_payload)?,
                record.occurred_at.0,
            ],
        )?;

        if is_raw_only {
            tx.execute(
                "INSERT INTO change_log (op, target_id, created_at) VALUES ('raw_only_source', ?1, ?2)",
                params![source_id.as_str(), record.occurred_at.0],
            )?;
        }

        let enqueued_job_id = if let Some(policy_str) = queue_policy_opt {
            let job = enqueue_or_rearm_extraction_job_tx(
                &tx,
                &source_id,
                &ExtractionJobInput {
                    source_id: source_id.clone(),
                    chunk_ids: vec![chunk_id],
                    chunk_batch_sha256: content_sha256.clone(),
                    author: resolved_speaker.clone(),
                    model_policy: policy_str,
                    priority: job_priority,
                    reference_time: record.occurred_at,
                    created_at: record.occurred_at,
                },
            )?;
            Some(job.job_id)
        } else {
            None
        };

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
        let final_kind = match &record.op {
            PublicIngestOp::Upsert => PublicRecordAppliedKind::Upserted,
            PublicIngestOp::Supersede {
                previous_external_id,
            } => {
                let prev_ext_key = format!(
                    "{}:{}",
                    record.source.kind.trim(),
                    previous_external_id.trim()
                );
                let prev_sources: Vec<String> = {
                    let mut stmt = tx.prepare(
                        "SELECT source_id FROM raw_sources
                         WHERE external_source_key = ?1
                         ORDER BY captured_at ASC, source_id ASC",
                    )?;
                    let rows = stmt.query_map([&prev_ext_key], |r| r.get::<_, String>(0))?;
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
                        record.occurred_at,
                    )?;
                    affected_facts.extend(retired);
                }
                crate::exchange::mark_alignment_dirty_tx(&tx, record.occurred_at)?;
                PublicRecordAppliedKind::Superseded
            }
            PublicIngestOp::Withdraw => PublicRecordAppliedKind::Withdrawn,
        };

        tx.commit()?;
        if !affected_facts.is_empty() {
            self.sync_skills_now()?;
        }

        let stored = StoredSource {
            source_id: source_id.clone(),
            content_sha256,
            git_blob: blob_id,
        };
        Ok((
            PublicRecordApplyOutcome {
                kind: final_kind,
                external_source_key,
                source_id: Some(source_id),
                superseded_source_id,
                affected_facts,
                enqueued_job_id,
                resolved_speaker,
            },
            Some(stored),
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
