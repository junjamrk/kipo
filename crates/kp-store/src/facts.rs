use kp_model::{
    ActorId, AudienceSpec, BitemporalStamp, DerivationView, EpochMs, Fact, FactAccess, FactId,
    Justification, KpError, ModelRoutePolicy, Premise, SourceId, SourceScope, SourceScopeId,
    StrongRef, TrustTier, ValidityWindow,
};
use rusqlite::{params, Connection, OptionalExtension, Row, TransactionBehavior};

use crate::embedding::{
    default_embedder_for_meta, encode_vector_le_bytes, guard_and_lock_tx_embedding_dims,
    sanitize_and_validate_embedding,
};
use crate::tombstone::guard;
use crate::{validate_vector, Embedder, Result, Store, StoreError, Tokenizer};

impl Store {
    /// Inserts an immutable fact. Import and replay use this same guarded entry point.
    pub fn insert_fact(&mut self, fact: &Fact) -> Result<()> {
        let embedder = self.embedder.clone();
        self.insert_fact_with_embedder(fact, embedder.as_ref())
    }

    /// Inserts an immutable fact and indexes its lexical tokens and embedding vector atomically.
    pub fn insert_fact_with_embedder(
        &mut self,
        fact: &Fact,
        embedder: &(impl Embedder + ?Sized),
    ) -> Result<()> {
        let opaque = self.tombstone_id(&fact.origin_library, &fact.logical_key)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        write_fact_indexed(&tx, fact, &opaque, &self.tokenizer, embedder)?;
        if fact.predicate != "kp.core.concept_alignment" {
            crate::exchange::mark_alignment_dirty_tx(&tx, fact.access.recorded_at())?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn insert_justification(&mut self, justification: &Justification) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        write_justification(&tx, justification)?;
        tx.commit()?;
        Ok(())
    }

    /// Reads a consistent snapshot and applies the same pure semiring evaluator in either mode.
    pub fn visible_facts(&self, viewer: &ActorId, at: EpochMs) -> Result<Vec<Fact>> {
        let graph = self.derivation_view()?;
        let visible = graph.evaluate(viewer, at);
        Ok(graph
            .facts
            .into_iter()
            .filter_map(|(id, fact)| visible.facts.contains(&id).then_some(fact))
            .collect())
    }

    /// Local administrative graph access. Viewer-facing consumers must call evaluate before emitting data.
    pub fn derivation_view(&self) -> Result<DerivationView> {
        let tx = self.db.unchecked_transaction()?;
        let graph = read_graph(&tx)?;
        tx.commit()?;
        Ok(graph)
    }

    /// Reads the library mode (`personal` or `community`) directly without counting tables.
    #[inline]
    pub fn mode(&self) -> Result<kp_model::LibraryMode> {
        Ok(self.mode)
    }

    /// Returns `true` when `source_id` carries an explicit `public_source_meta` or `raw_only_source`
    /// policy record in `change_log`.
    pub fn has_public_source_meta(&self, source_id: &SourceId) -> Result<bool> {
        if is_source_marked_raw_only(&self.db, source_id.as_str())? {
            return Ok(true);
        }
        Ok(
            crate::public_ingest::load_public_source_meta_tx(&self.db, source_id.as_str())?
                .and_then(|m| m.effective_policy)
                .is_some(),
        )
    }

    /// Resolves the effective [`ModelRoutePolicy`] governing a stored raw source (`source_id`).
    ///
    /// - Returns [`ModelRoutePolicy::RawOnly`] whenever `raw_only_source` is recorded;
    /// - Otherwise returns the latest `effective_policy` from `public_source_meta` (or decoded
    ///   `extraction_queue.model_policy`), defaulting to [`ModelRoutePolicy::HostCli`].
    pub fn source_effective_route_policy(&self, source_id: &SourceId) -> Result<ModelRoutePolicy> {
        source_effective_route_policy_in_tx(&self.db, source_id)
    }

    /// Resolves the effective [`ModelRoutePolicy`] governing `fact`:
    /// - Any `RawOnly` marker on `fact.object.route_policy`, its anchored source, or its derivation
    ///   premises wins unconditionally (`ModelRoutePolicy::RawOnly`);
    /// - Otherwise prefers `fact.object.get("route_policy")` if present, then the anchored source's
    ///   effective policy, defaulting to [`ModelRoutePolicy::HostCli`].
    pub fn fact_effective_route_policy(&self, fact: &Fact) -> Result<ModelRoutePolicy> {
        fact_effective_route_policy_in_tx(&self.db, fact)
    }

    /// Upserts the live membership roster for an external collaboration container (`source_scopes`).
    pub fn upsert_source_scope(&mut self, scope: &SourceScope) -> Result<()> {
        if matches!(scope.current_audience, AudienceSpec::SemiringDerived) {
            return Err(KpError::invalid(
                "source_scope",
                "source scope cannot use semiring marker",
            )
            .into());
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO source_scopes (scope_id,provider,external_container_id,label,current_audience_json,synced_at)
             VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(scope_id) DO UPDATE SET
                 provider=excluded.provider,
                 external_container_id=excluded.external_container_id,
                 label=excluded.label,
                 current_audience_json=excluded.current_audience_json,
                 synced_at=excluded.synced_at",
            params![
                scope.scope_id.as_str(),
                scope.provider,
                scope.external_container_id,
                scope.label,
                serde_json::to_string(&scope.current_audience)?,
                scope.synced_at.0
            ],
        )?;
        crate::exchange::reconcile_pending_publishes_for_scope_tx(&tx, self.mode, &scope.scope_id)?;
        crate::exchange::mark_alignment_dirty_tx(&tx, scope.synced_at)?;
        tx.commit()?;
        self.sync_skills_now()?;
        Ok(())
    }

    pub fn source_scope(&self, scope_id: &SourceScopeId) -> Result<Option<SourceScope>> {
        self.db
            .query_row(
                "SELECT scope_id,provider,external_container_id,label,current_audience_json,synced_at
                 FROM source_scopes WHERE scope_id=?1",
                [scope_id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .optional()?
            .map(
                |(id, provider, external_container_id, label, aud_json, synced_at)| {
                    Ok(SourceScope {
                        scope_id: id.parse()?,
                        provider,
                        external_container_id,
                        label,
                        current_audience: serde_json::from_str(&aud_json)?,
                        synced_at: EpochMs(synced_at),
                    })
                },
            )
            .transpose()
    }
}

pub(crate) fn fact_default_passage(fact: &Fact) -> String {
    format!(
        "{} {} {} {}",
        fact.subject,
        fact.predicate,
        fact.object,
        fact.tags.iter().cloned().collect::<Vec<_>>().join(" ")
    )
}

pub(crate) fn fact_fts_columns(
    tokenizer: &Tokenizer,
    subject: &str,
    predicate: &str,
    object_json: &str,
    tags_json: &str,
    anchor_json: Option<&str>,
) -> (String, String, String) {
    let quote = anchor_json
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|val| {
            val.get("verbatim_quote")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    let combined = if quote.is_empty() {
        format!("{object_json} {tags_json}")
    } else {
        format!("{object_json} {tags_json} {quote}")
    };
    (
        tokenizer.segment_text(subject),
        tokenizer.segment_text(predicate),
        tokenizer.segment_text(&combined),
    )
}

pub(crate) fn source_effective_route_policy_in_tx(
    db: &Connection,
    source_id: &SourceId,
) -> Result<ModelRoutePolicy> {
    if is_source_marked_raw_only(db, source_id.as_str())? {
        return Ok(ModelRoutePolicy::RawOnly);
    }
    if let Some(meta) = crate::public_ingest::load_public_source_meta_tx(db, source_id.as_str())? {
        if let Some(pol) = meta.effective_policy {
            return Ok(pol);
        }
    }
    let queued_pol: Option<String> = db
        .query_row(
            "SELECT model_policy FROM extraction_queue
             WHERE source_id = ?1
             ORDER BY created_at DESC LIMIT 1",
            [source_id.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(raw) = queued_pol {
        return crate::public_ingest::decode_queue_model_policy(&raw);
    }
    Ok(ModelRoutePolicy::HostCli)
}

pub(crate) fn fact_effective_route_policy_in_tx(
    db: &Connection,
    fact: &Fact,
) -> Result<ModelRoutePolicy> {
    if fact
        .tags
        .iter()
        .any(|t| t == "raw_only" || t == "kp.raw_only")
        || fact.object.get("raw_only").and_then(|v| v.as_bool()) == Some(true)
        || is_fact_raw_only(db, fact)?
    {
        return Ok(ModelRoutePolicy::RawOnly);
    }
    let obj_pol: Option<ModelRoutePolicy> = match fact.object.get("route_policy") {
        None | Some(serde_json::Value::Null) => None,
        Some(val) => match serde_json::from_value::<ModelRoutePolicy>(val.clone()) {
            Ok(parsed) => Some(parsed),
            Err(_) => return Ok(ModelRoutePolicy::RawOnly),
        },
    };
    if matches!(obj_pol, Some(ModelRoutePolicy::RawOnly)) {
        return Ok(ModelRoutePolicy::RawOnly);
    }
    let src_pol = match fact
        .source_anchor
        .as_ref()
        .and_then(|a| a.source_id.as_ref())
    {
        Some(sid) => Some(source_effective_route_policy_in_tx(db, sid)?),
        None => None,
    };
    if matches!(src_pol, Some(ModelRoutePolicy::RawOnly)) {
        return Ok(ModelRoutePolicy::RawOnly);
    }
    Ok(obj_pol.or(src_pol).unwrap_or(ModelRoutePolicy::HostCli))
}

pub(crate) fn is_source_marked_raw_only(db: &Connection, source_id: &str) -> Result<bool> {
    Ok(db
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM change_log WHERE op = 'raw_only_source' AND target_id = ?1)",
        )?
        .query_row([source_id], |r| r.get(0))?)
}

pub(crate) fn is_fact_raw_only(db: &Connection, fact: &Fact) -> Result<bool> {
    if fact
        .tags
        .iter()
        .any(|t| t == "raw_only" || t == "kp.raw_only")
        || fact.object.get("raw_only").and_then(|v| v.as_bool()) == Some(true)
        || fact.object.get("route_policy").is_some_and(|v| {
            !v.is_null()
                && serde_json::from_value::<ModelRoutePolicy>(v.clone())
                    .map_or(true, |p| matches!(p, ModelRoutePolicy::RawOnly))
        })
    {
        return Ok(true);
    }
    if let Some(source_id) = fact
        .source_anchor
        .as_ref()
        .and_then(|a| a.source_id.as_ref())
    {
        if is_source_marked_raw_only(db, source_id.as_str())? {
            return Ok(true);
        }
    }
    let has_raw_only_premise: bool = db
        .prepare_cached(
            "SELECT EXISTS(
                WITH RECURSIVE anc(fid) AS (
                    SELECT p.premise_fact_id
                    FROM justifications j
                    JOIN justification_premises p ON p.just_id = j.just_id
                    WHERE j.conclusion_fact_id = ?1
                    UNION
                    SELECT p2.premise_fact_id
                    FROM anc a
                    JOIN justifications j2 ON j2.conclusion_fact_id = a.fid
                    JOIN justification_premises p2 ON p2.just_id = j2.just_id
                )
                SELECT 1
                FROM anc a
                JOIN facts f ON f.fact_id = a.fid
                WHERE EXISTS (
                    SELECT 1 FROM change_log cl
                    WHERE cl.op = 'raw_only_source'
                      AND cl.target_id = json_extract(f.source_anchor_json, '$.source_id')
                )
                   OR EXISTS (
                    SELECT 1 FROM json_each(f.tags_json) jt
                    WHERE jt.value IN ('raw_only', 'kp.raw_only')
                )
                   OR json_extract(f.object_json, '$.route_policy') = 'raw_only'
                   OR json_extract(f.object_json, '$.route_policy.mode') = 'raw_only'
                   OR json_extract(f.object_json, '$.raw_only') = 1
            )",
        )?
        .query_row([fact.id.as_str()], |r| r.get(0))?;
    Ok(has_raw_only_premise)
}

pub(crate) fn write_fact(
    db: &Connection,
    fact: &Fact,
    opaque: &kp_model::ContentHash,
) -> Result<()> {
    let (model, dims): (String, i64) = db.query_row(
        "SELECT embedding_model, embedding_dims FROM library_meta LIMIT 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let fallback = default_embedder_for_meta(&model, usize::try_from(dims).unwrap_or(0));
    write_fact_indexed_with_hash(
        db,
        fact,
        opaque,
        None,
        Tokenizer::shared(),
        fallback.as_ref(),
    )
}

pub(crate) fn write_fact_indexed(
    db: &Connection,
    fact: &Fact,
    opaque: &kp_model::ContentHash,
    tokenizer: &Tokenizer,
    embedder: &(impl Embedder + ?Sized),
) -> Result<()> {
    write_fact_indexed_with_hash(db, fact, opaque, None, tokenizer, embedder)
}

pub(crate) fn write_fact_indexed_with_hash(
    db: &Connection,
    fact: &Fact,
    opaque: &kp_model::ContentHash,
    precomputed_sha256: Option<&kp_model::ContentHash>,
    tokenizer: &Tokenizer,
    embedder: &(impl Embedder + ?Sized),
) -> Result<()> {
    guard(db, opaque)?;
    let source_collision: bool = db
        .prepare_cached("SELECT EXISTS(SELECT 1 FROM raw_sources WHERE source_id=?1) AND (SELECT mode FROM library_meta)='community'")?
        .query_row([opaque.as_str()], |row| row.get(0))?;
    if source_collision {
        return Err(StoreError::Conflict);
    }
    fact.validate()?;
    if let Some(source) = fact
        .source_anchor
        .as_ref()
        .and_then(|a| a.source_id.as_ref())
    {
        if let Ok(opaque) = source.as_str().parse() {
            guard(db, &opaque)?;
        }
    }
    let computed_sha256;
    let content_sha256 = match precomputed_sha256 {
        Some(hash) => hash,
        None => {
            computed_sha256 = fact.strong_ref().content_sha256;
            &computed_sha256
        }
    };

    let skip_for_raw_only = !embedder.allows_raw_only() && is_fact_raw_only(db, fact)?;
    let computed_vec = if embedder.is_enabled() && !skip_for_raw_only {
        let passage = fact_default_passage(fact);
        let model_id = embedder.model_id();
        if model_id != "hash:v1" && model_id != "custom" {
            if passage.trim().is_empty() {
                None
            } else {
                let raw = embedder.embed(&passage)?;
                Some(sanitize_and_validate_embedding(&raw)?)
            }
        } else {
            let v = embedder.embed(&passage)?;
            validate_vector(&v)?;
            Some(v)
        }
    } else {
        None
    };

    let (audience, from, until) = match &fact.access {
        FactAccess::Base { audience, stamp } => (
            audience.clone(),
            stamp.valid.valid_from.0,
            stamp.valid.valid_until.0,
        ),
        // SQL's NOT NULL columns have neutral sentinels, never a materialized derived interval.
        FactAccess::Derived { .. } => (AudienceSpec::SemiringDerived, 0, i64::MAX),
    };
    let object_json = serde_json::to_string(&fact.object)?;
    let tags_json = serde_json::to_string(&fact.tags)?;
    let anchor_json = fact
        .source_anchor
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    let fts_rowid: i64 = db
        .prepare_cached("INSERT INTO facts (
            fact_id,origin_library_id,logical_key,content_sha256,kind,subject,predicate,object_json,tags_json,
            author_actor,ingested_by_actor,trust_tier,audience_json,source_scope_id,governed_scope_json,broader_scope_json,
            valid_from,valid_until,recorded_at,expired_at,status,
            superseded_by,rank,source_anchor_json,unverified_gaps_json,miss_count)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26)
            RETURNING fts_rowid")?
        .query_row(params![
                fact.id.as_str(),fact.origin_library.as_str(),fact.logical_key.as_str(),content_sha256.as_str(),
                fact.kind.as_str(),fact.subject,fact.predicate,&object_json,&tags_json,
                fact.author.as_str(),fact.ingested_by.as_ref().map(ActorId::as_str),u8::from(fact.trust_tier),serde_json::to_string(&audience)?,
                fact.source_scope_id.as_ref().map(SourceScopeId::as_str),
                fact.governed_scope.as_ref().map(serde_json::to_string).transpose()?,
                fact.broader_scope.as_ref().map(serde_json::to_string).transpose()?,
                from,until,
                fact.access.recorded_at().0,fact.access.expired_at().map(|t| t.0),fact.status.as_str(),
                fact.superseded_by.as_ref().map(FactId::as_str),fact.rank.as_str(),
                anchor_json.as_deref(),serde_json::to_string(&fact.unverified_gaps)?,fact.miss_count], |row| row.get(0))?;
    let (seg_subject, seg_predicate, seg_object) = fact_fts_columns(
        tokenizer,
        &fact.subject,
        &fact.predicate,
        &object_json,
        &tags_json,
        anchor_json.as_deref(),
    );
    db.prepare_cached(
        "INSERT INTO facts_fts (rowid, subject, predicate, segmented_object) VALUES (?1, ?2, ?3, ?4)",
    )?
    .execute(params![fts_rowid, seg_subject, seg_predicate, seg_object])?;
    if let Some(vector) = computed_vec {
        let model_id = embedder.model_id();
        guard_and_lock_tx_embedding_dims(db, model_id, vector.len())?;
        let vec_blob = encode_vector_le_bytes(&vector);
        db.prepare_cached(
            "INSERT INTO fact_vectors (fact_id, dims, vec_blob, model_id) VALUES (?1, ?2, ?3, ?4)",
        )?
        .execute(params![fact.id.as_str(), vector.len(), vec_blob, model_id])?;
    }
    db.prepare_cached(
        "INSERT INTO change_log (op,target_id,origin_library_id,logical_key,content_sha256,actor_id,created_at)
         VALUES ('insert_fact',?1,?2,?3,?4,?5,?6)",
    )?
    .execute(params![
        fact.id.as_str(),
        fact.origin_library.as_str(),
        fact.logical_key.as_str(),
        content_sha256.as_str(),
        fact.author.as_str(),
        fact.access.recorded_at().0,
    ])?;
    Ok(())
}

pub(crate) fn write_justification(db: &Connection, justification: &Justification) -> Result<()> {
    if justification.premises.is_empty() {
        return Err(
            KpError::invalid("justification", "requires at least one grounded premise").into(),
        );
    }
    let conclusion: String = db
        .query_row(
            "SELECT audience_json FROM facts WHERE fact_id=?1",
            [justification.conclusion.as_str()],
            |row| row.get(0),
        )
        .optional()?
        .ok_or(StoreError::NotFound)?;
    if serde_json::from_str::<AudienceSpec>(&conclusion)? != AudienceSpec::SemiringDerived {
        return Err(
            KpError::invalid("justification", "conclusion must have derived access").into(),
        );
    }
    db.execute(
        "INSERT INTO justifications VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            justification.id.as_str(),
            justification.conclusion.as_str(),
            justification.mode.as_str(),
            justification.rule_id,
            justification.rule_version,
            justification.active,
            justification.created_at.0,
            justification.invalidated_at.map(|t| t.0),
            justification.invalidation_reason
        ],
    )?;
    for premise in &justification.premises {
        let reference = &premise.strong_ref;
        let valid: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM facts WHERE fact_id=?1 AND origin_library_id=?2 AND logical_key=?3 AND content_sha256=?4)",
                params![premise.fact_id.as_str(),reference.origin_library.as_str(),reference.logical_key.as_str(),reference.content_sha256.as_str()], |row| row.get(0))?;
        if !valid {
            return Err(StoreError::Conflict);
        }
        db.execute(
            "INSERT INTO justification_premises VALUES (?1,?2,?3,?4,?5)",
            params![
                justification.id.as_str(),
                premise.fact_id.as_str(),
                reference.origin_library.as_str(),
                reference.logical_key.as_str(),
                reference.content_sha256.as_str()
            ],
        )?;
    }
    Ok(())
}

pub(crate) fn read_graph(db: &Connection) -> Result<DerivationView> {
    let mut graph = DerivationView::default();
    {
        let mut stmt = db.prepare(
            "SELECT actor_id FROM members_tokens WHERE revoked_at IS NULL ORDER BY actor_id",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            graph.members.insert(row.get::<_, String>(0)?.parse()?);
        }
    }
    {
        let mut stmt = db.prepare(
            "SELECT scope_id, current_audience_json FROM source_scopes ORDER BY scope_id",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let scope_id: SourceScopeId = row.get::<_, String>(0)?.parse()?;
            let aud: AudienceSpec = serde_json::from_str(&row.get::<_, String>(1)?)?;
            graph.source_scopes.insert(scope_id, aud);
        }
    }
    {
        let mut tombstones = std::collections::BTreeSet::new();
        let mut t_stmt = db.prepare("SELECT tombstone_id FROM tombstones")?;
        let mut t_rows = t_stmt.query([])?;
        while let Some(row) = t_rows.next()? {
            tombstones.insert(row.get::<_, String>(0)?);
        }
        let mut stmt = db.prepare("SELECT * FROM facts ORDER BY fact_id")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let fact = read_fact(row)?;
            let is_tombstoned = if tombstones.is_empty() {
                false
            } else {
                let opaque = crate::tombstone::tombstone_id(
                    fact.origin_library.as_str().as_bytes(),
                    &fact.origin_library,
                    &fact.logical_key,
                )?;
                tombstones.contains(opaque.as_str())
            };
            if !is_tombstoned {
                graph.facts.insert(fact.id.clone(), fact);
            }
        }
    }
    {
        let mut stmt = db.prepare("SELECT * FROM justifications ORDER BY just_id")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let justification = Justification {
                id: row.get::<_, String>("just_id")?.parse()?,
                conclusion: row.get::<_, String>("conclusion_fact_id")?.parse()?,
                mode: row.get::<_, String>("derivation_mode")?.parse()?,
                rule_id: row.get("rule_id")?,
                rule_version: row.get("rule_version")?,
                active: row.get("active")?,
                created_at: EpochMs(row.get("created_at")?),
                invalidated_at: row.get::<_, Option<i64>>("invalidated_at")?.map(EpochMs),
                invalidation_reason: row.get("invalidation_reason")?,
                premises: Vec::new(),
            };
            graph
                .justifications
                .insert(justification.id.clone(), justification);
        }
        let mut stmt =
            db.prepare("SELECT * FROM justification_premises ORDER BY just_id,premise_fact_id")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let id = row.get::<_, String>("just_id")?.parse()?;
            let justification = graph
                .justifications
                .get_mut(&id)
                .ok_or(StoreError::NotFound)?;
            justification.premises.push(Premise {
                fact_id: row.get::<_, String>("premise_fact_id")?.parse()?,
                strong_ref: StrongRef {
                    origin_library: row.get::<_, String>("premise_origin_lib")?.parse()?,
                    logical_key: row.get::<_, String>("premise_logical_key")?.parse()?,
                    content_sha256: row.get::<_, String>("premise_sha256")?.parse()?,
                },
            });
        }
    }
    for proof in graph.justifications.values_mut() {
        proof.premises.sort_by(|a, b| {
            a.strong_ref
                .cmp(&b.strong_ref)
                .then(a.fact_id.cmp(&b.fact_id))
        });
    }
    Ok(graph)
}

pub(crate) fn read_fact(row: &Row<'_>) -> Result<Fact> {
    let audience: AudienceSpec = serde_json::from_str(&row.get::<_, String>("audience_json")?)?;
    let recorded_at = EpochMs(row.get("recorded_at")?);
    let expired_at = row.get::<_, Option<i64>>("expired_at")?.map(EpochMs);
    let access = if audience == AudienceSpec::SemiringDerived {
        FactAccess::Derived {
            recorded_at,
            expired_at,
        }
    } else {
        FactAccess::Base {
            audience,
            stamp: BitemporalStamp {
                valid: ValidityWindow {
                    valid_from: EpochMs(row.get("valid_from")?),
                    valid_until: EpochMs(row.get("valid_until")?),
                },
                recorded_at,
                expired_at,
            },
        }
    };
    let fact = Fact {
        id: row.get::<_, String>("fact_id")?.parse()?,
        origin_library: row.get::<_, String>("origin_library_id")?.parse()?,
        logical_key: row.get::<_, String>("logical_key")?.parse()?,
        kind: row.get::<_, String>("kind")?.parse()?,
        subject: row.get("subject")?,
        predicate: row.get("predicate")?,
        object: serde_json::from_str(&row.get::<_, String>("object_json")?)?,
        tags: serde_json::from_str(&row.get::<_, String>("tags_json")?)?,
        author: row.get::<_, String>("author_actor")?.parse()?,
        ingested_by: row
            .get::<_, Option<String>>("ingested_by_actor")?
            .map(|id| id.parse())
            .transpose()?,
        trust_tier: TrustTier::try_from(row.get::<_, u8>("trust_tier")?)?,
        access,
        source_scope_id: row
            .get::<_, Option<String>>("source_scope_id")?
            .map(|id| id.parse())
            .transpose()?,
        governed_scope: row
            .get::<_, Option<String>>("governed_scope_json")?
            .map(|json| serde_json::from_str(&json))
            .transpose()?,
        broader_scope: row
            .get::<_, Option<String>>("broader_scope_json")?
            .map(|json| serde_json::from_str(&json))
            .transpose()?,
        status: row.get::<_, String>("status")?.parse()?,
        superseded_by: row
            .get::<_, Option<String>>("superseded_by")?
            .map(|id| id.parse())
            .transpose()?,
        rank: row.get::<_, String>("rank")?.parse()?,
        source_anchor: row
            .get::<_, Option<String>>("source_anchor_json")?
            .map(|json| serde_json::from_str(&json))
            .transpose()?,
        unverified_gaps: serde_json::from_str(&row.get::<_, String>("unverified_gaps_json")?)?,
        miss_count: row.get("miss_count")?,
    };
    if row.get::<_, String>("content_sha256")? != fact.strong_ref().content_sha256.as_str() {
        return Err(StoreError::Conflict);
    }
    fact.validate()?;
    Ok(fact)
}

#[cfg(test)]
#[path = "facts_tests.rs"]
mod tests;
