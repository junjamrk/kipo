use std::cell::RefCell;
use std::collections::BTreeSet;

use kp_model::{
    ActorId, AnchorSpec, AudienceSpec, BitemporalStamp, CharSpan, ContentHash, DerivationView,
    EpochMs, Fact, FactAccess, FactId, FactKind, FactRank, FactStatus, KpError, LibraryId,
    LogicalKey, ModelRoutePolicy, ProjectionPageMeta, ReviewId, ReviewItem, ReviewKind,
    ReviewStatus, SourceId, SourceScopeId, TrustTier, ValidityWindow,
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use uuid::Uuid;

use super::{WritebackFactUpdate, WritebackProposalOp};
use crate::embedding::{
    encode_vector_le_bytes, guard_and_lock_tx_embedding_dims, sanitize_and_validate_embedding,
    validate_vector, FtsOnlyEmbedder,
};
use crate::export::{format_kp_ref, sanitize_md_inline};
use crate::facts::{fact_default_passage, write_fact_indexed};
use crate::governance::{new_fact_id, new_review_id, upsert_open_review_item_tx};
use crate::maintenance::supersede_indexed_tx;
use crate::public_ingest::StoredPublicSourceMeta;
use crate::tombstone::guard;
use crate::{Embedder, Result, Tokenizer};

pub(super) fn query_projection_page_tx(
    tx: &Connection,
    page_slug: &str,
    viewer: &ActorId,
) -> Result<Option<ProjectionPageMeta>> {
    tx.query_row(
        "SELECT page_slug, viewer_actor, base_seq, page_sha256, rendered_at
         FROM projection_pages
         WHERE page_slug = ?1 AND viewer_actor = ?2",
        params![page_slug.trim(), viewer.as_str()],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        },
    )
    .optional()?
    .map(|(slug, actor_str, base_seq, sha_str, rendered_at)| {
        Ok(ProjectionPageMeta {
            page_slug: slug,
            viewer: actor_str.parse()?,
            base_seq,
            page_sha256: sha_str.parse()?,
            rendered_at: EpochMs(rendered_at),
        })
    })
    .transpose()
}

pub(super) fn evaluate_baseline_view(
    graph: &DerivationView,
    viewer: &ActorId,
    at: EpochMs,
) -> BTreeSet<FactId> {
    let mut historical_graph = graph.clone();
    let historical_ids: BTreeSet<FactId> = historical_graph
        .facts
        .values()
        .filter(|fact| {
            fact.status == FactStatus::Superseded
                && fact.access.expired_at().is_none()
                && matches!(&fact.access, FactAccess::Base { stamp, .. } if stamp.valid.contains(at))
                && fact
                    .superseded_by
                    .as_ref()
                    .and_then(|id| historical_graph.facts.get(id))
                    .is_some_and(|next| {
                        matches!(&next.access, FactAccess::Base { stamp, .. } if at < stamp.valid.valid_from)
                    })
        })
        .map(|f| f.id.clone())
        .collect();
    for id in historical_ids {
        if let Some(fact) = historical_graph.facts.get_mut(&id) {
            fact.status = FactStatus::Active;
        }
    }
    historical_graph.evaluate(viewer, at).facts
}

pub(super) fn follow_superseded_head<'a>(graph: &'a DerivationView, start: &'a Fact) -> &'a Fact {
    let mut cur = start;
    let mut visited = BTreeSet::new();
    visited.insert(cur.id.clone());
    while let Some(next_id) = &cur.superseded_by {
        if !visited.insert(next_id.clone()) {
            break;
        }
        let Some(next) = graph.facts.get(next_id) else {
            break;
        };
        cur = next;
    }
    cur
}

pub(super) fn was_statically_permitted_for_viewer(
    graph: &DerivationView,
    fact: &Fact,
    viewer: &ActorId,
    visiting: &mut BTreeSet<FactId>,
) -> bool {
    match &fact.access {
        FactAccess::Base { audience, .. } => audience.allows(viewer, &graph.members),
        FactAccess::Derived { .. } => {
            if !visiting.insert(fact.id.clone()) {
                return false;
            }
            let ok = graph
                .justifications
                .values()
                .filter(|j| j.conclusion == fact.id && !j.premises.is_empty())
                .any(|j| {
                    j.premises.iter().all(|p| {
                        graph.facts.get(&p.fact_id).is_some_and(|pf| {
                            was_statically_permitted_for_viewer(graph, pf, viewer, visiting)
                        })
                    })
                });
            visiting.remove(&fact.id);
            ok
        }
    }
}

pub(super) enum PendingWritebackEmbed {
    Chunk { chunk_id: String, body: String },
    Fact { fact_id: FactId, text: String },
}

pub(super) struct WritebackTxEnv<'a> {
    pub(super) library_id: &'a LibraryId,
    pub(super) actor: &'a ActorId,
    pub(super) at: EpochMs,
    pub(super) tokenizer: &'a Tokenizer,
    pub(super) embedder_enabled: bool,
    pub(super) embedder_allows_raw_only: bool,
    pub(super) pending_embeds: &'a RefCell<Vec<PendingWritebackEmbed>>,
}

/// Computes writeback embeddings strictly outside any SQLite write transaction so remote HTTP
/// embedders never hold an `Immediate` write lock across network I/O, then persists the resulting
/// vectors in a short atomic transaction.
pub(super) fn flush_pending_writeback_embeds<E: Embedder + ?Sized>(
    db: &mut Connection,
    embedder: &E,
    pending: &[PendingWritebackEmbed],
) -> Result<()> {
    if pending.is_empty() || !embedder.is_enabled() {
        return Ok(());
    }
    let model_id = embedder.model_id();
    let is_builtin = model_id == "hash:v1" || model_id == "custom";
    let mut chunk_vecs: Vec<(&str, Vec<f32>)> = Vec::new();
    let mut fact_vecs: Vec<(&str, Vec<f32>)> = Vec::new();

    for item in pending {
        match item {
            PendingWritebackEmbed::Chunk { chunk_id, body } => {
                if !is_builtin && body.trim().is_empty() {
                    continue;
                }
                let raw = embedder.embed(body)?;
                let vec = if is_builtin {
                    validate_vector(&raw)?;
                    raw
                } else {
                    sanitize_and_validate_embedding(&raw)?
                };
                chunk_vecs.push((chunk_id.as_str(), vec));
            }
            PendingWritebackEmbed::Fact { fact_id, text } => {
                if !is_builtin && text.trim().is_empty() {
                    continue;
                }
                let raw = embedder.embed(text)?;
                let vec = if is_builtin {
                    validate_vector(&raw)?;
                    raw
                } else {
                    sanitize_and_validate_embedding(&raw)?
                };
                fact_vecs.push((fact_id.as_str(), vec));
            }
        }
    }

    if chunk_vecs.is_empty() && fact_vecs.is_empty() {
        return Ok(());
    }
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for (chunk_id, vector) in &chunk_vecs {
        guard_and_lock_tx_embedding_dims(&tx, model_id, vector.len())?;
        let blob = encode_vector_le_bytes(vector);
        tx.execute(
            "INSERT OR REPLACE INTO chunk_vectors (chunk_id, dims, vec_blob, model_id)
             VALUES (?1, ?2, ?3, ?4)",
            params![chunk_id, vector.len(), blob, model_id],
        )?;
    }
    for (fact_id, vector) in &fact_vecs {
        guard_and_lock_tx_embedding_dims(&tx, model_id, vector.len())?;
        let blob = encode_vector_le_bytes(vector);
        tx.execute(
            "INSERT OR REPLACE INTO fact_vectors (fact_id, dims, vec_blob, model_id)
             VALUES (?1, ?2, ?3, ?4)",
            params![fact_id, vector.len(), blob, model_id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

struct WritebackSourceSpec<'a> {
    uri: &'a str,
    redacted_body: &'a str,
    readable_by: &'a AudienceSpec,
    source_scope_id: Option<&'a SourceScopeId>,
    policy: &'a ModelRoutePolicy,
}

fn write_writeback_source_tx(
    tx: &Connection,
    env: &WritebackTxEnv<'_>,
    spec: &WritebackSourceSpec<'_>,
) -> Result<(SourceId, String, CharSpan)> {
    let WritebackTxEnv {
        library_id,
        actor: viewer,
        at,
        tokenizer,
        embedder_enabled,
        embedder_allows_raw_only,
        pending_embeds,
    } = *env;
    let WritebackSourceSpec {
        uri,
        redacted_body,
        readable_by,
        source_scope_id,
        policy,
    } = *spec;
    let wb_key: LogicalKey = format!("writeback:{}", Uuid::now_v7()).parse()?;
    let opaque = crate::tombstone_id(library_id.as_str().as_bytes(), library_id, &wb_key)?;
    guard(tx, &opaque)?;
    let source_id = SourceId::new(opaque.as_str())?;

    let bytes = redacted_body.as_bytes();
    let byte_len = i64::try_from(bytes.len())
        .map_err(|_| KpError::invalid("writeback", "content exceeds SQLite byte range"))?;
    let content_sha256 = ContentHash::digest(bytes);
    let char_count = u32::try_from(redacted_body.chars().count())
        .map_err(|_| KpError::invalid("writeback", "content exceeds char span range"))?;
    let span = CharSpan::new(0, char_count.max(1))?;

    let chunk_anchor = AnchorSpec {
        source_id: Some(source_id.clone()),
        chunk_id: Some(format!("{source_id}-0")),
        char_start: (char_count > 0).then_some(span.char_start),
        char_end: (char_count > 0).then_some(span.char_end),
        verbatim_quote: (!redacted_body.is_empty()).then(|| redacted_body.to_owned()),
        ..AnchorSpec::default()
    };

    let is_raw_only = matches!(policy, ModelRoutePolicy::RawOnly);
    let skip_for_raw_only = is_raw_only && !embedder_allows_raw_only;

    tx.execute(
        "INSERT OR IGNORE INTO raw_blobs (content_sha256, byte_len, content_blob, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![content_sha256.as_str(), byte_len, bytes, at.0],
    )?;

    let audience_json = serde_json::to_string(readable_by)?;
    tx.execute(
        "INSERT INTO raw_sources
            (source_id, uri, media_type, content_sha256, git_blob_or_commit, byte_len,
             visibility_scope, captured_at, self_echo_filtered, ingested_by_actor, source_scope_id)
         VALUES (?1, ?2, 'text/markdown', ?3, ?3, ?4, ?5, ?6, 0, ?7, ?8)",
        params![
            source_id.as_str(),
            uri,
            content_sha256.as_str(),
            byte_len,
            &audience_json,
            at.0,
            viewer.as_str(),
            source_scope_id.map(SourceScopeId::as_str),
        ],
    )?;

    let chunk_id = format!("{source_id}-0");
    let segmented = tokenizer.segment_text(redacted_body);
    let fts_rowid: i64 = tx.query_row(
        "INSERT INTO chunks
            (chunk_id, source_id, source_sha256, ordinal, heading, body, segmented_body,
             anchor_json, readable_by_json, created_at)
         VALUES (?1, ?2, ?3, 0, '', ?4, ?5, ?6, ?7, ?8) RETURNING fts_rowid",
        params![
            &chunk_id,
            source_id.as_str(),
            content_sha256.as_str(),
            redacted_body,
            &segmented,
            serde_json::to_string(&chunk_anchor)?,
            &audience_json,
            at.0,
        ],
        |row| row.get(0),
    )?;

    if embedder_enabled && !skip_for_raw_only {
        pending_embeds
            .borrow_mut()
            .push(PendingWritebackEmbed::Chunk {
                chunk_id: chunk_id.clone(),
                body: redacted_body.to_owned(),
            });
    }
    tx.execute(
        "INSERT INTO chunks_fts (rowid, title, heading, segmented_body) VALUES (?1, ?2, '', ?3)",
        params![fts_rowid, tokenizer.segment_text(uri), &segmented],
    )?;

    tx.execute(
        "INSERT INTO change_log (op, target_id, origin_library_id, logical_key, content_sha256, actor_id, created_at)
         VALUES ('ingest_source', ?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            source_id.as_str(),
            library_id.as_str(),
            wb_key.as_str(),
            content_sha256.as_str(),
            viewer.as_str(),
            at.0,
        ],
    )?;

    let meta = StoredPublicSourceMeta {
        explicit_policy: Some(policy.clone()),
        effective_policy: Some(policy.clone()),
        speaker: Some(viewer.to_string()),
    };
    tx.execute(
        "INSERT INTO change_log (op, target_id, logical_key, payload_json, created_at)
         VALUES ('public_source_meta', ?1, ?2, ?3, ?4)",
        params![
            source_id.as_str(),
            wb_key.as_str(),
            serde_json::to_string(&meta)?,
            at.0,
        ],
    )?;
    if is_raw_only {
        tx.execute(
            "INSERT INTO change_log (op, target_id, created_at) VALUES ('raw_only_source', ?1, ?2)",
            params![source_id.as_str(), at.0],
        )?;
    }

    Ok((source_id, chunk_id, span))
}

fn copy_fact_entities_tx(tx: &Connection, from_fact: &FactId, to_fact: &FactId) -> Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO fact_entities (fact_id, entity_slug, role)
         SELECT ?2, entity_slug, role FROM fact_entities WHERE fact_id = ?1",
        params![from_fact.as_str(), to_fact.as_str()],
    )?;
    Ok(())
}

fn copy_fact_concepts_tx(
    tx: &Connection,
    from_fact: &FactId,
    to_fact: &FactId,
    at: EpochMs,
) -> Result<()> {
    let concept_ids: Vec<kp_model::ConceptId> = {
        let mut stmt = tx.prepare(
            "SELECT DISTINCT logical_key FROM change_log
             WHERE op = 'fact_concept' AND target_id = ?1 AND logical_key IS NOT NULL
             ORDER BY seq ASC",
        )?;
        let mut rows = stmt.query([from_fact.as_str()])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            if let Ok(cid) = row.get::<_, String>(0)?.parse::<kp_model::ConceptId>() {
                out.push(cid);
            }
        }
        out
    };
    for cid in &concept_ids {
        crate::exchange::link_fact_concept_tx(tx, to_fact, cid, at)?;
    }
    Ok(())
}

pub(super) struct WritebackProposalContext<'a> {
    pub(super) viewer: &'a ActorId,
    pub(super) base_seq: i64,
    pub(super) current_seq: i64,
    pub(super) at: EpochMs,
}

pub(super) struct WritebackProposalInput<'a> {
    pub(super) op: WritebackProposalOp,
    pub(super) review_target_id: String,
    pub(super) target_fact_id: Option<&'a FactId>,
    pub(super) redacted_proposed_claim: Option<String>,
    pub(super) redacted_raw_markdown: String,
    pub(super) source_file: &'a str,
    pub(super) reason: &'a str,
    pub(super) route_policy: Option<&'a ModelRoutePolicy>,
}

pub(super) fn enqueue_writeback_proposal_tx(
    tx: &Connection,
    ctx: &WritebackProposalContext<'_>,
    input: WritebackProposalInput<'_>,
) -> Result<ReviewId> {
    let WritebackProposalContext {
        viewer,
        base_seq,
        current_seq,
        at,
    } = *ctx;
    let WritebackProposalInput {
        op,
        review_target_id,
        target_fact_id,
        redacted_proposed_claim,
        redacted_raw_markdown,
        source_file,
        reason,
        route_policy,
    } = input;
    let excerpt_chars: String = sanitize_md_inline(&redacted_raw_markdown)
        .chars()
        .take(96)
        .collect();
    let question = match (op, target_fact_id) {
        (WritebackProposalOp::EditFact, Some(fid)) => format!(
            "Apply proposed Markdown edit to `{}` ({reason}): {excerpt_chars}",
            format_kp_ref(fid)
        ),
        (WritebackProposalOp::DeleteFact, Some(fid)) => format!(
            "Confirm proposed withdrawal of `{}` ({reason})",
            format_kp_ref(fid)
        ),
        _ => format!(
            "Import freeform Markdown addition from `{source_file}` ({reason}): {excerpt_chars}"
        ),
    };

    let payload = json!({
        "proposal_op": op.as_str(),
        "target_fact_id": target_fact_id.map(FactId::as_str),
        "proposed_claim": redacted_proposed_claim,
        "raw_markdown": redacted_raw_markdown,
        "source_file": source_file,
        "reason": reason,
        "route_policy": route_policy,
        "base_seq": base_seq,
        "current_seq": current_seq,
    });

    let item = ReviewItem {
        id: new_review_id()?,
        actor: viewer.clone(),
        kind: ReviewKind::WritebackProposal,
        target_id: review_target_id,
        question,
        options: vec!["confirm".to_owned(), "dismiss".to_owned()],
        payload,
        status: ReviewStatus::Open,
        created_at: at,
        resolved_at: None,
    };

    let stored = upsert_open_review_item_tx(tx, &item)?;
    tx.execute(
        "INSERT INTO change_log (op, target_id, actor_id, payload_json, created_at)
         VALUES ('enqueue_review', ?1, ?2, ?3, ?4)",
        params![
            stored.id.as_str(),
            viewer.as_str(),
            serde_json::to_string(&json!({
                "kind": ReviewKind::WritebackProposal.as_str(),
                "proposal_op": op.as_str(),
                "target_id": &stored.target_id,
            }))?,
            at.0,
        ],
    )?;
    Ok(stored.id)
}

pub(super) fn supersede_fact_via_writeback_tx(
    tx: &Connection,
    env: &WritebackTxEnv<'_>,
    old_fact: &Fact,
    updated_object: Value,
    redacted_claim: &str,
    orig_policy: &ModelRoutePolicy,
) -> Result<WritebackFactUpdate> {
    let WritebackTxEnv {
        library_id,
        actor: viewer,
        at,
        tokenizer,
        embedder_enabled,
        embedder_allows_raw_only,
        pending_embeds,
    } = *env;
    let FactAccess::Base { audience, stamp } = &old_fact.access else {
        return Err(
            KpError::invalid("access", "only base facts can be superseded in place").into(),
        );
    };
    let uri = format!("writeback:{viewer}:{}", old_fact.id);
    let (wb_source_id, chunk_id, span) = write_writeback_source_tx(
        tx,
        env,
        &WritebackSourceSpec {
            uri: &uri,
            redacted_body: redacted_claim,
            readable_by: audience,
            source_scope_id: old_fact.source_scope_id.as_ref(),
            policy: orig_policy,
        },
    )?;

    let new_id = new_fact_id()?;
    let valid_until = if stamp.valid.valid_until > at {
        stamp.valid.valid_until
    } else {
        EpochMs::OPEN_END
    };
    let mut tags = old_fact.tags.clone();
    if matches!(orig_policy, ModelRoutePolicy::RawOnly)
        && !tags.iter().any(|t| t == "raw_only" || t == "kp.raw_only")
    {
        tags.insert("kp.raw_only".to_owned());
    }

    let replacement = Fact {
        id: new_id.clone(),
        origin_library: library_id.clone(),
        logical_key: new_id.as_str().parse()?,
        kind: old_fact.kind,
        subject: old_fact.subject.clone(),
        predicate: old_fact.predicate.clone(),
        object: updated_object,
        tags,
        author: viewer.clone(),
        ingested_by: Some(viewer.clone()),
        trust_tier: old_fact.trust_tier,
        access: FactAccess::Base {
            audience: audience.clone(),
            stamp: BitemporalStamp {
                valid: ValidityWindow {
                    valid_from: at,
                    valid_until,
                },
                recorded_at: at,
                expired_at: None,
            },
        },
        source_scope_id: old_fact.source_scope_id.clone(),
        governed_scope: old_fact.governed_scope.clone(),
        broader_scope: None,
        status: FactStatus::Active,
        superseded_by: None,
        rank: old_fact.rank,
        source_anchor: Some(AnchorSpec {
            source_id: Some(wb_source_id.clone()),
            chunk_id: Some(chunk_id),
            char_start: Some(span.char_start),
            char_end: Some(span.char_end),
            verbatim_quote: Some(redacted_claim.to_owned()),
            ..AnchorSpec::default()
        }),
        unverified_gaps: Vec::new(),
        miss_count: 0,
    };
    replacement.validate()?;

    let skip_for_raw_only =
        matches!(orig_policy, ModelRoutePolicy::RawOnly) && !embedder_allows_raw_only;
    supersede_indexed_tx(tx, &old_fact.id, &replacement, tokenizer, &FtsOnlyEmbedder)?;
    if embedder_enabled && !skip_for_raw_only {
        pending_embeds
            .borrow_mut()
            .push(PendingWritebackEmbed::Fact {
                fact_id: replacement.id.clone(),
                text: fact_default_passage(&replacement),
            });
    }
    copy_fact_entities_tx(tx, &old_fact.id, &replacement.id)?;
    copy_fact_concepts_tx(tx, &old_fact.id, &replacement.id, at)?;

    let sref = replacement.strong_ref();
    tx.execute(
        "INSERT INTO change_log (op, target_id, origin_library_id, logical_key, content_sha256, actor_id, payload_json, created_at)
         VALUES ('writeback_supersede', ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            replacement.id.as_str(),
            sref.origin_library.as_str(),
            sref.logical_key.as_str(),
            sref.content_sha256.as_str(),
            viewer.as_str(),
            serde_json::to_string(&json!({
                "old_fact_id": old_fact.id,
                "source_id": wb_source_id,
            }))?,
            at.0,
        ],
    )?;

    Ok(WritebackFactUpdate {
        old_fact_id: old_fact.id.clone(),
        new_fact_id: replacement.id,
        source_id: wb_source_id,
    })
}

pub(super) struct StandaloneWritebackFactSpec<'a> {
    pub(super) kind: FactKind,
    pub(super) subject: String,
    pub(super) predicate: String,
    pub(super) object: Value,
    pub(super) audience: AudienceSpec,
    pub(super) source_scope_id: Option<SourceScopeId>,
    pub(super) governed_scope: Option<kp_model::GovernedScope>,
    pub(super) redacted_body: &'a str,
    pub(super) policy: &'a ModelRoutePolicy,
}

pub(super) fn insert_standalone_writeback_fact_tx(
    tx: &Connection,
    env: &WritebackTxEnv<'_>,
    spec: StandaloneWritebackFactSpec<'_>,
) -> Result<FactId> {
    let WritebackTxEnv {
        library_id,
        actor,
        at,
        tokenizer,
        embedder_enabled,
        embedder_allows_raw_only,
        pending_embeds,
    } = *env;
    let StandaloneWritebackFactSpec {
        kind,
        subject,
        predicate,
        object,
        audience,
        source_scope_id,
        governed_scope,
        redacted_body,
        policy,
    } = spec;
    let new_id = new_fact_id()?;
    let uri = format!("writeback:{actor}:{new_id}");
    let (wb_source_id, chunk_id, span) = write_writeback_source_tx(
        tx,
        env,
        &WritebackSourceSpec {
            uri: &uri,
            redacted_body,
            readable_by: &audience,
            source_scope_id: source_scope_id.as_ref(),
            policy,
        },
    )?;

    let mut tags = BTreeSet::new();
    if matches!(policy, ModelRoutePolicy::RawOnly) {
        tags.insert("kp.raw_only".to_owned());
    }

    let fact = Fact {
        id: new_id.clone(),
        origin_library: library_id.clone(),
        logical_key: new_id.as_str().parse()?,
        kind,
        subject,
        predicate,
        object,
        tags,
        author: actor.clone(),
        ingested_by: Some(actor.clone()),
        trust_tier: TrustTier::HumanVerbatim,
        access: FactAccess::Base {
            audience,
            stamp: BitemporalStamp {
                valid: ValidityWindow {
                    valid_from: at,
                    valid_until: EpochMs::OPEN_END,
                },
                recorded_at: at,
                expired_at: None,
            },
        },
        source_scope_id,
        governed_scope,
        broader_scope: None,
        status: FactStatus::Active,
        superseded_by: None,
        rank: FactRank::Normal,
        source_anchor: Some(AnchorSpec {
            source_id: Some(wb_source_id),
            chunk_id: Some(chunk_id),
            char_start: Some(span.char_start),
            char_end: Some(span.char_end),
            verbatim_quote: Some(redacted_body.to_owned()),
            ..AnchorSpec::default()
        }),
        unverified_gaps: Vec::new(),
        miss_count: 0,
    };
    fact.validate()?;

    let opaque = crate::tombstone_id(
        fact.origin_library.as_str().as_bytes(),
        &fact.origin_library,
        &fact.logical_key,
    )?;
    let skip_for_raw_only =
        matches!(policy, ModelRoutePolicy::RawOnly) && !embedder_allows_raw_only;
    write_fact_indexed(tx, &fact, &opaque, tokenizer, &FtsOnlyEmbedder)?;
    if embedder_enabled && !skip_for_raw_only {
        pending_embeds
            .borrow_mut()
            .push(PendingWritebackEmbed::Fact {
                fact_id: fact.id.clone(),
                text: fact_default_passage(&fact),
            });
    }
    crate::exchange::mark_alignment_dirty_tx(tx, at)?;
    Ok(new_id)
}
