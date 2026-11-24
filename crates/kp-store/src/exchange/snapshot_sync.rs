use std::collections::{BTreeMap, BTreeSet};

use kp_model::{ActorId, ContentHash, EpochMs, Fact, FactId, FactStatus};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};

use super::{read_origin_references, write_origin_reference, ChangeSnapshot, Tombstone};
use crate::facts::{read_graph, write_fact_indexed, write_justification};
use crate::{Result, Store, StoreError};

impl Store {
    pub fn change_snapshot(&mut self, viewer: &ActorId, at: EpochMs) -> Result<ChangeSnapshot> {
        self.change_snapshot_since(viewer, at, None)
    }

    pub fn change_snapshot_since(
        &mut self,
        viewer: &ActorId,
        at: EpochMs,
        since: Option<&str>,
    ) -> Result<ChangeSnapshot> {
        Ok(self
            .change_snapshot_with_governance_since(viewer, at, since)?
            .0)
    }

    /// Computes a joint `(viewer, at)` projection of facts, proofs, tombstones, origin references,
    /// referenced `CommunityConcept`s, and referenced `SourceScope`s with zero cross-viewer leakage.
    pub fn change_snapshot_with_governance_since(
        &mut self,
        viewer: &ActorId,
        at: EpochMs,
        since: Option<&str>,
    ) -> Result<(
        ChangeSnapshot,
        Vec<kp_model::CommunityConcept>,
        Vec<kp_model::SourceScope>,
    )> {
        let tx = self.db.transaction()?;
        let graph = read_graph(&tx)?;
        let visible = graph.evaluate(viewer, at);
        let facts: Vec<_> = graph
            .facts
            .values()
            .filter(|f| visible.facts.contains(&f.id))
            .cloned()
            .collect();
        let proof_ids: BTreeSet<_> = visible.justifications.values().flatten().collect();
        let proofs: Vec<_> = graph
            .justifications
            .values()
            .filter(|p| proof_ids.contains(&p.id))
            .cloned()
            .collect();
        for fact in &facts {
            let opaque = crate::tombstone_id(
                fact.origin_library.as_str().as_bytes(),
                &fact.origin_library,
                &fact.logical_key,
            )?;
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM change_log WHERE op='shared_identity' AND target_id=?1 AND actor_id=?2)",
                params![opaque.as_str(), viewer.as_str()], |r| r.get(0),
            )?;
            if !exists {
                tx.execute("INSERT INTO change_log (op,target_id,actor_id,created_at) VALUES ('shared_identity',?1,?2,?3)", params![opaque.as_str(),viewer.as_str(),at.0])?;
            }
        }
        let mut tombstones = Vec::new();
        {
            let mut stmt = tx.prepare("SELECT t.tombstone_id,t.origin_library_id,t.purged_at,t.reason FROM tombstones t WHERE EXISTS (SELECT 1 FROM change_log c WHERE c.op='shared_identity' AND c.target_id=t.tombstone_id AND c.actor_id=?1) ORDER BY t.tombstone_id")?;
            let mut rows = stmt.query([viewer.as_str()])?;
            while let Some(row) = rows.next()? {
                tombstones.push(Tombstone {
                    tombstone_id: row.get::<_, String>(0)?.parse()?,
                    origin_library_id: row.get::<_, String>(1)?.parse()?,
                    purged_at: EpochMs(row.get(2)?),
                    reason: row.get(3)?,
                });
            }
        }
        let origin_references = read_origin_references(&tx)?
            .into_iter()
            .filter(|(id, _)| visible.facts.contains(id))
            .collect::<BTreeMap<_, _>>();
        let concepts = read_concepts_for_facts(&tx, &facts)?;
        let source_scopes = read_source_scopes_for_facts(&tx, &facts)?;
        let cursor = ContentHash::digest(serde_json::to_vec(&(
            &facts,
            &proofs,
            &tombstones,
            &origin_references,
            &concepts,
            &source_scopes,
        ))?);
        tx.commit()?;
        let unchanged = since == Some(cursor.as_str());
        let snapshot = ChangeSnapshot {
            library_id: self.library_id.clone(),
            cursor,
            unchanged,
            facts: if unchanged { Vec::new() } else { facts },
            justifications: if unchanged { Vec::new() } else { proofs },
            tombstones: if unchanged { Vec::new() } else { tombstones },
            origin_references: if unchanged {
                BTreeMap::new()
            } else {
                origin_references
            },
        };
        Ok((
            snapshot,
            if unchanged { Vec::new() } else { concepts },
            if unchanged { Vec::new() } else { source_scopes },
        ))
    }

    /// Tombstones take precedence over every record in the incoming projection.
    pub fn import_snapshot(
        &mut self,
        source: &str,
        snapshot: &ChangeSnapshot,
        at: EpochMs,
    ) -> Result<()> {
        self.import_snapshot_with_governance(source, snapshot, &[], &[], None, at)
    }

    /// Cursor acceptance and the imported projection commit together, including credential changes.
    pub fn import_snapshot_with_cursor(
        &mut self,
        source: &str,
        snapshot: &ChangeSnapshot,
        credential: &ContentHash,
        at: EpochMs,
    ) -> Result<()> {
        self.import_snapshot_with_governance(source, snapshot, &[], &[], Some(credential), at)
    }

    /// Atomically imports tombstones, live `SourceScope`s, `CommunityConcept`s, facts, proofs,
    /// and sync cursor for a viewer projection.
    pub fn import_snapshot_with_governance(
        &mut self,
        source: &str,
        snapshot: &ChangeSnapshot,
        concepts: &[kp_model::CommunityConcept],
        source_scopes: &[kp_model::SourceScope],
        credential: Option<&ContentHash>,
        at: EpochMs,
    ) -> Result<()> {
        if snapshot.unchanged {
            self.sync_skills_now()?;
            return Ok(());
        }
        for tombstone in &snapshot.tombstones {
            self.apply_tombstone(tombstone)?;
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for scope in source_scopes {
            upsert_source_scope_tx(&tx, scope)?;
        }
        for concept in concepts {
            upsert_concept_tx(&tx, concept, true)?;
        }
        let previous: Option<String> = tx.query_row("SELECT payload_json FROM change_log WHERE op='sync_snapshot' AND target_id=?1 ORDER BY seq DESC LIMIT 1", [source], |r| r.get(0)).optional()?;
        let previous: BTreeSet<FactId> = previous
            .map(|p| serde_json::from_str(&p))
            .transpose()?
            .unwrap_or_default();
        let incoming: BTreeSet<_> = snapshot.facts.iter().map(|f| f.id.clone()).collect();
        for id in previous.difference(&incoming) {
            tx.execute(
                "UPDATE facts SET status='unreachable_hidden' WHERE fact_id=?1 AND status='active'",
                [id.as_str()],
            )?;
        }
        let graph = read_graph(&tx)?;
        let mut managed: BTreeSet<_> = previous
            .iter()
            .filter(|id| graph.facts.contains_key(*id))
            .cloned()
            .collect();
        let mut accepted = BTreeSet::new();
        for fact in &snapshot.facts {
            let opaque = crate::tombstone_id(
                fact.origin_library.as_str().as_bytes(),
                &fact.origin_library,
                &fact.logical_key,
            )?;
            match crate::tombstone::guard(&tx, &opaque) {
                Err(StoreError::Tombstoned(_)) => continue,
                result => result?,
            }
            if let Some(old) = graph.facts.get(&fact.id) {
                if old.strong_ref() != fact.strong_ref() {
                    return Err(StoreError::Conflict);
                }
                // Only rows already imported from this endpoint may have lifecycle metadata replaced.
                if previous.contains(&fact.id)
                    && matches!(
                        old.status,
                        FactStatus::Active | FactStatus::UnreachableHidden
                    )
                {
                    let audience = match &fact.access {
                        kp_model::FactAccess::Base { audience, .. } => audience.clone(),
                        kp_model::FactAccess::Derived { .. } => {
                            kp_model::AudienceSpec::SemiringDerived
                        }
                    };
                    tx.execute(
                        "UPDATE facts SET status=?1,audience_json=?2,ingested_by_actor=?3,source_scope_id=?4,governed_scope_json=?5,broader_scope_json=?6,superseded_by=?7,rank=?8,expired_at=?9,miss_count=?10 WHERE fact_id=?11",
                        params![
                            fact.status.as_str(),
                            serde_json::to_string(&audience)?,
                            fact.ingested_by.as_ref().map(ActorId::as_str),
                            fact.source_scope_id.as_ref().map(kp_model::SourceScopeId::as_str),
                            fact.governed_scope.as_ref().map(serde_json::to_string).transpose()?,
                            fact.broader_scope.as_ref().map(serde_json::to_string).transpose()?,
                            fact.superseded_by.as_ref().map(FactId::as_str),
                            fact.rank.as_str(),
                            fact.access.expired_at().map(|t| t.0),
                            if fact.status == FactStatus::Active { 0 } else { fact.miss_count },
                            fact.id.as_str()
                        ],
                    )?;
                }
            } else {
                write_fact_indexed(&tx, fact, &opaque, &self.tokenizer, self.embedder.as_ref())?;
                managed.insert(fact.id.clone());
            }
            accepted.insert(fact.id.clone());
        }
        if !concepts.is_empty() && !accepted.is_empty() {
            let concept_map: BTreeMap<_, _> =
                concepts.iter().map(|c| (c.id.clone(), c.clone())).collect();
            let mut matched_any = BTreeSet::new();
            for fact in snapshot.facts.iter().filter(|f| accepted.contains(&f.id)) {
                let mut refs = BTreeSet::new();
                collect_inline_fact_concepts(fact, &concept_map, &mut refs);
                for cid in &refs {
                    link_fact_concept_tx(&tx, &fact.id, cid, at)?;
                    matched_any.insert(cid.clone());
                }
            }
            // If an explicit concept accompanied a single-fact or cohesive projection without inline IDs,
            // bind it to the accepted base facts so re-exporting from this mirror preserves the link.
            for cid in concept_map.keys().filter(|cid| !matched_any.contains(*cid)) {
                for fact in snapshot
                    .facts
                    .iter()
                    .filter(|f| accepted.contains(&f.id) && !f.kind.is_derived())
                {
                    link_fact_concept_tx(&tx, &fact.id, cid, at)?;
                }
            }
        }
        // Remove whole old proofs: dropping a single premise would weaken a conjunction.
        for proof in graph
            .justifications
            .values()
            .filter(|p| previous.contains(&p.conclusion))
        {
            tx.execute(
                "DELETE FROM justifications WHERE just_id=?1",
                [proof.id.as_str()],
            )?;
        }
        for proof in &snapshot.justifications {
            if accepted.contains(&proof.conclusion)
                && managed.contains(&proof.conclusion)
                && proof.premises.iter().all(|p| accepted.contains(&p.fact_id))
                && (!graph.justifications.contains_key(&proof.id)
                    || previous.contains(&proof.conclusion))
            {
                write_justification(&tx, proof)?;
            }
        }
        for (id, reference) in &snapshot.origin_references {
            if accepted.contains(id) {
                write_origin_reference(&tx, id, reference)?;
            }
        }
        tx.execute(
            "DELETE FROM change_log WHERE op='sync_snapshot' AND target_id=?1",
            [source],
        )?;
        tx.execute("INSERT INTO change_log (op,target_id,payload_json,created_at) VALUES ('sync_snapshot',?1,?2,?3)", params![source,serde_json::to_string(&managed)?,at.0])?;
        tx.execute(
            "DELETE FROM change_log WHERE op='sync_cursor' AND target_id=?1",
            [source],
        )?;
        if let Some(credential) = credential {
            tx.execute("INSERT INTO change_log (op,target_id,actor_id,content_sha256,created_at) VALUES ('sync_cursor',?1,?2,?3,?4)", params![source,credential.as_str(),snapshot.cursor.as_str(),at.0])?;
        }
        mark_alignment_dirty_tx(&tx, at)?;
        tx.commit()?;
        self.sync_skills_now()?;
        Ok(())
    }

    /// Reconciles a mirrored fact against a remote origin under Rev 4 offline non-invalidation:
    /// - `Some(true)`: origin confirmed active -> `FactStatus::Active` (`miss_count = 0`).
    /// - `Some(false)`: origin explicitly confirmed withdrawn/superseded/purged -> permanent `Invalidated` + cascade.
    /// - `None`: transient network error / timeout / offline miss -> `FactStatus::UnreachableHidden` ONLY,
    ///   NEVER permanently marking `Invalidated` regardless of consecutive miss count (2, 3, or 10+ misses).
    pub fn reconcile_origin_fact(
        &mut self,
        id: &FactId,
        confirmed: Option<bool>,
        at: EpochMs,
    ) -> Result<()> {
        self.reconcile_fact(id, confirmed, at)
    }

    pub fn exchange_upsert_concept(
        &mut self,
        concept: &kp_model::CommunityConcept,
        allow_overwrite: bool,
    ) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        upsert_concept_tx(&tx, concept, allow_overwrite)?;
        tx.commit()?;
        Ok(())
    }

    pub fn exchange_link_fact_concepts(
        &mut self,
        fact_id: &FactId,
        concept_ids: &[kp_model::ConceptId],
        at: EpochMs,
    ) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for cid in concept_ids {
            link_fact_concept_tx(&tx, fact_id, cid, at)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn exchange_concepts_for_facts(
        &self,
        facts: &[Fact],
    ) -> Result<Vec<kp_model::CommunityConcept>> {
        read_concepts_for_facts(&self.db, facts)
    }

    pub fn exchange_active_concepts(&self) -> Result<Vec<kp_model::CommunityConcept>> {
        Ok(read_all_active_concepts(&self.db)?.into_values().collect())
    }

    /// Reads the durable `needs_alignment` marker (`dirty_gen > clean_gen` in
    /// `change_log(op='alignment_state', target_id='global')`).
    pub fn needs_alignment(&self) -> Result<bool> {
        let (dirty_gen, clean_gen) = read_alignment_generations(&self.db)?;
        Ok(dirty_gen > clean_gen)
    }

    /// Reads the current `dirty_gen` generation counter from `alignment_state`.
    pub fn alignment_dirty_generation(&self) -> Result<u64> {
        Ok(read_alignment_generations(&self.db)?.0)
    }

    /// Atomically advances `clean_gen = clean_gen.max(completed_gen.min(dirty_gen))` so concurrent
    /// writes (`dirty_gen > completed_gen`) committed while LLM alignment ran preserve `needs_alignment() == true`.
    pub fn ack_alignment_generation(&mut self, completed_gen: u64, at: EpochMs) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (dirty_gen, clean_gen) = read_alignment_generations(&tx)?;
        let next_clean = clean_gen.max(completed_gen.min(dirty_gen));
        write_alignment_generations_tx(&tx, dirty_gen, next_clean, at)?;
        tx.commit()?;
        Ok(())
    }

    /// Persists the durable `needs_alignment` marker in `change_log(op='alignment_state', target_id='global')`.
    pub fn set_needs_alignment(&mut self, needs: bool, at: EpochMs) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if needs {
            mark_alignment_dirty_tx(&tx, at)?;
        } else {
            let (dirty_gen, _) = read_alignment_generations(&tx)?;
            write_alignment_generations_tx(&tx, dirty_gen, dirty_gen, at)?;
        }
        tx.commit()?;
        Ok(())
    }
}

fn read_alignment_generations(db: &rusqlite::Connection) -> Result<(u64, u64)> {
    let raw: Option<Option<String>> = db
        .query_row(
            "SELECT payload_json FROM change_log
             WHERE op='alignment_state' AND target_id='global'
             ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let Some(Some(payload)) = raw else {
        return Ok((0, 0));
    };
    let val: Value = serde_json::from_str(&payload)?;
    Ok(match val {
        Value::Bool(b) => (u64::from(b), 0),
        Value::Object(map) => {
            if map.contains_key("dirty_gen") || map.contains_key("clean_gen") {
                let dirty = map.get("dirty_gen").and_then(Value::as_u64).unwrap_or(0);
                let clean = map.get("clean_gen").and_then(Value::as_u64).unwrap_or(0);
                (dirty, clean)
            } else {
                let needs = map
                    .get("needs_alignment")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                (u64::from(needs), 0)
            }
        }
        _ => (0, 0),
    })
}

fn write_alignment_generations_tx(
    db: &rusqlite::Connection,
    dirty_gen: u64,
    clean_gen: u64,
    at: EpochMs,
) -> Result<()> {
    let payload = serde_json::to_string(&json!({
        "dirty_gen": dirty_gen,
        "clean_gen": clean_gen,
    }))?;
    let existing_seq: Option<i64> = db
        .query_row(
            "SELECT seq FROM change_log
             WHERE op='alignment_state' AND target_id='global'
             ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(seq) = existing_seq {
        db.execute(
            "UPDATE change_log SET payload_json=?1, created_at=?2 WHERE seq=?3",
            params![payload, at.0, seq],
        )?;
    } else {
        db.execute(
            "INSERT INTO change_log (op,target_id,payload_json,created_at)
             VALUES ('alignment_state','global',?1,?2)",
            params![payload, at.0],
        )?;
    }
    Ok(())
}

pub(crate) fn mark_alignment_dirty_tx(db: &rusqlite::Connection, at: EpochMs) -> Result<u64> {
    let (dirty_gen, clean_gen) = read_alignment_generations(db)?;
    let next_dirty = dirty_gen.saturating_add(1).max(clean_gen.saturating_add(1));
    write_alignment_generations_tx(db, next_dirty, clean_gen, at)?;
    Ok(next_dirty)
}

pub(crate) fn upsert_concept_tx(
    db: &rusqlite::Connection,
    concept: &kp_model::CommunityConcept,
    allow_overwrite: bool,
) -> Result<()> {
    if concept.canonical_label.trim().is_empty() {
        return Err(kp_model::KpError::invalid(
            "community_concept",
            "canonical_label must be nonempty",
        )
        .into());
    }
    if allow_overwrite {
        db.execute(
            "INSERT INTO community_concepts (concept_id,concept_kind,canonical_label,domain,polarity,proposed_by,status,created_at)
             VALUES (?1,?2,?3,?4,?5,?6,'active',?7)
             ON CONFLICT(concept_id) DO UPDATE SET
                 concept_kind=excluded.concept_kind,
                 canonical_label=excluded.canonical_label,
                 domain=excluded.domain,
                 polarity=excluded.polarity,
                 status='active'",
            params![
                concept.id.as_str(),
                concept.kind.as_str(),
                concept.canonical_label,
                concept.domain,
                concept.polarity.as_str(),
                concept.proposed_by.as_str(),
                concept.created_at.0
            ],
        )?;
    } else {
        db.execute(
            "INSERT INTO community_concepts (concept_id,concept_kind,canonical_label,domain,polarity,proposed_by,status,created_at)
             VALUES (?1,?2,?3,?4,?5,?6,'active',?7)
             ON CONFLICT(concept_id) DO NOTHING",
            params![
                concept.id.as_str(),
                concept.kind.as_str(),
                concept.canonical_label,
                concept.domain,
                concept.polarity.as_str(),
                concept.proposed_by.as_str(),
                concept.created_at.0
            ],
        )?;
    }
    Ok(())
}

fn upsert_source_scope_tx(db: &rusqlite::Connection, scope: &kp_model::SourceScope) -> Result<()> {
    if matches!(
        scope.current_audience,
        kp_model::AudienceSpec::SemiringDerived
    ) {
        return Err(kp_model::KpError::invalid(
            "source_scope",
            "source scope cannot use semiring marker",
        )
        .into());
    }
    let conflicting_other: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM source_scopes WHERE provider=?1 AND external_container_id=?2 AND scope_id!=?3)",
        params![
            scope.provider,
            scope.external_container_id,
            scope.scope_id.as_str()
        ],
        |r| r.get(0),
    )?;
    let container_id = if conflicting_other {
        format!("{}#{}", scope.external_container_id, scope.scope_id)
    } else {
        scope.external_container_id.clone()
    };
    db.execute(
        "INSERT INTO source_scopes (scope_id,provider,external_container_id,label,current_audience_json,synced_at)
         VALUES (?1,?2,?3,?4,?5,?6)
         ON CONFLICT(scope_id) DO UPDATE SET
             provider=excluded.provider,
             external_container_id=excluded.external_container_id,
             label=excluded.label,
             current_audience_json=excluded.current_audience_json,
             synced_at=excluded.synced_at
         WHERE excluded.synced_at >= source_scopes.synced_at",
        params![
            scope.scope_id.as_str(),
            scope.provider,
            container_id,
            scope.label,
            serde_json::to_string(&scope.current_audience)?,
            scope.synced_at.0
        ],
    )?;
    Ok(())
}

pub(crate) fn link_fact_concept_tx(
    db: &rusqlite::Connection,
    fact_id: &FactId,
    concept_id: &kp_model::ConceptId,
    at: EpochMs,
) -> Result<()> {
    db.execute(
        "INSERT INTO change_log (op,target_id,logical_key,created_at)
         SELECT 'fact_concept',?1,?2,?3
         WHERE NOT EXISTS (
             SELECT 1 FROM change_log WHERE op='fact_concept' AND target_id=?1 AND logical_key=?2
         )",
        params![fact_id.as_str(), concept_id.as_str(), at.0],
    )?;
    Ok(())
}

fn read_all_active_concepts(
    db: &rusqlite::Connection,
) -> Result<BTreeMap<kp_model::ConceptId, kp_model::CommunityConcept>> {
    let mut stmt = db.prepare(
        "SELECT concept_id,concept_kind,canonical_label,domain,polarity,proposed_by,created_at
         FROM community_concepts WHERE status='active' ORDER BY concept_id",
    )?;
    let mut rows = stmt.query([])?;
    let mut map = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let concept = kp_model::CommunityConcept {
            id: row.get::<_, String>(0)?.parse()?,
            kind: row.get::<_, String>(1)?.parse()?,
            canonical_label: row.get(2)?,
            domain: row.get(3)?,
            polarity: row.get::<_, String>(4)?.parse()?,
            proposed_by: row.get::<_, String>(5)?.parse()?,
            created_at: EpochMs(row.get(6)?),
        };
        map.insert(concept.id.clone(), concept);
    }
    Ok(map)
}

fn collect_scope_concept(
    scope: &kp_model::GovernedScope,
    known: &BTreeMap<kp_model::ConceptId, kp_model::CommunityConcept>,
    out: &mut BTreeSet<kp_model::ConceptId>,
) {
    if let kp_model::GovernedScope::RelationGroup {
        group_concept_id, ..
    } = scope
    {
        if known.contains_key(group_concept_id) {
            out.insert(group_concept_id.clone());
        }
    }
}

fn collect_json_concepts(
    value: &Value,
    known: &BTreeMap<kp_model::ConceptId, kp_model::CommunityConcept>,
    out: &mut BTreeSet<kp_model::ConceptId>,
) {
    match value {
        Value::String(s) => {
            if let Ok(cid) = s.parse::<kp_model::ConceptId>() {
                if known.contains_key(&cid) {
                    out.insert(cid);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_json_concepts(item, known, out);
            }
        }
        Value::Object(fields) => {
            for (k, v) in fields {
                if let Ok(cid) = k.parse::<kp_model::ConceptId>() {
                    if known.contains_key(&cid) {
                        out.insert(cid);
                    }
                }
                collect_json_concepts(v, known, out);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn collect_inline_fact_concepts(
    fact: &Fact,
    known: &BTreeMap<kp_model::ConceptId, kp_model::CommunityConcept>,
    out: &mut BTreeSet<kp_model::ConceptId>,
) {
    if let Some(scope) = &fact.governed_scope {
        collect_scope_concept(scope, known, out);
    }
    if let Some(broader) = &fact.broader_scope {
        collect_scope_concept(&broader.narrow_active, known, out);
        collect_scope_concept(&broader.broader_candidate, known, out);
    }
    if fact.kind == kp_model::FactKind::Membership {
        if let Ok(rel) = serde_json::from_value::<kp_model::MembershipRelation>(fact.object.clone())
        {
            if known.contains_key(&rel.group_concept_id) {
                out.insert(rel.group_concept_id);
            }
        }
    }
    for s in fact
        .tags
        .iter()
        .map(String::as_str)
        .chain([fact.subject.as_str(), fact.predicate.as_str()])
    {
        if let Ok(cid) = s.parse::<kp_model::ConceptId>() {
            if known.contains_key(&cid) {
                out.insert(cid);
            }
        }
    }
    collect_json_concepts(&fact.object, known, out);
}

fn read_all_stored_concepts(
    db: &rusqlite::Connection,
) -> Result<BTreeMap<kp_model::ConceptId, kp_model::CommunityConcept>> {
    let mut stmt = db.prepare(
        "SELECT concept_id,concept_kind,canonical_label,domain,polarity,proposed_by,created_at
         FROM community_concepts ORDER BY concept_id",
    )?;
    let mut rows = stmt.query([])?;
    let mut map = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let concept = kp_model::CommunityConcept {
            id: row.get::<_, String>(0)?.parse()?,
            kind: row.get::<_, String>(1)?.parse()?,
            canonical_label: row.get(2)?,
            domain: row.get(3)?,
            polarity: row.get::<_, String>(4)?.parse()?,
            proposed_by: row.get::<_, String>(5)?.parse()?,
            created_at: EpochMs(row.get(6)?),
        };
        map.insert(concept.id.clone(), concept);
    }
    Ok(map)
}

/// Collects every [`kp_model::ConceptId`] in `community_concepts` that is still referenced by:
/// 1. any surviving `Fact` (recursively across `object` strings/arrays/nested objects/keys, `tags`,
///    `subject`, `predicate`, `governed_scope`, and `broader_scope`),
/// 2. any surviving `change_log (op = 'fact_concept')` link, or
/// 3. any explicit `change_log (op = 'concept_seed')` registration.
pub(crate) fn collect_all_referenced_concept_ids_tx(
    db: &rusqlite::Connection,
) -> Result<BTreeSet<kp_model::ConceptId>> {
    let known = read_all_stored_concepts(db)?;
    let mut ids = BTreeSet::new();
    if known.is_empty() {
        return Ok(ids);
    }
    let graph = crate::facts::read_graph(db)?;
    for fact in graph.facts.values() {
        collect_inline_fact_concepts(fact, &known, &mut ids);
    }
    {
        let mut stmt = db.prepare(
            "SELECT c.logical_key FROM change_log c
             JOIN facts f ON f.fact_id = c.target_id
             WHERE c.op = 'fact_concept' AND c.logical_key IS NOT NULL",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            if let Ok(cid) = row.get::<_, String>(0)?.parse::<kp_model::ConceptId>() {
                if known.contains_key(&cid) {
                    ids.insert(cid);
                }
            }
        }
    }
    {
        let mut stmt = db.prepare(
            "SELECT DISTINCT target_id FROM change_log
             WHERE op = 'concept_seed' AND target_id IS NOT NULL",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            if let Ok(cid) = row.get::<_, String>(0)?.parse::<kp_model::ConceptId>() {
                if known.contains_key(&cid) {
                    ids.insert(cid);
                }
            }
        }
    }
    Ok(ids)
}

fn read_concepts_for_facts(
    db: &rusqlite::Connection,
    facts: &[Fact],
) -> Result<Vec<kp_model::CommunityConcept>> {
    if facts.is_empty() {
        return Ok(Vec::new());
    }
    let known = read_all_active_concepts(db)?;
    if known.is_empty() {
        return Ok(Vec::new());
    }
    let mut ids = BTreeSet::new();
    let fact_ids: BTreeSet<&str> = facts.iter().map(|f| f.id.as_str()).collect();
    for fact in facts {
        collect_inline_fact_concepts(fact, &known, &mut ids);
    }
    let mut stmt = db.prepare(
        "SELECT target_id, logical_key FROM change_log WHERE op='fact_concept' ORDER BY seq",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let Some(tid) = row.get::<_, Option<String>>(0)? else {
            continue;
        };
        if !fact_ids.contains(tid.as_str()) {
            continue;
        }
        if let Some(raw) = row.get::<_, Option<String>>(1)? {
            if let Ok(cid) = raw.parse::<kp_model::ConceptId>() {
                if known.contains_key(&cid) {
                    ids.insert(cid);
                }
            }
        }
    }
    Ok(ids
        .into_iter()
        .filter_map(|id| known.get(&id).cloned())
        .collect())
}

fn read_source_scopes_for_facts(
    db: &rusqlite::Connection,
    facts: &[Fact],
) -> Result<Vec<kp_model::SourceScope>> {
    let scope_ids: BTreeSet<_> = facts
        .iter()
        .filter_map(|f| f.source_scope_id.clone())
        .collect();
    if scope_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = db.prepare(
        "SELECT scope_id,provider,external_container_id,label,current_audience_json,synced_at
         FROM source_scopes WHERE scope_id=?1",
    )?;
    let mut out = Vec::new();
    for scope_id in scope_ids {
        let row = stmt
            .query_row([scope_id.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })
            .optional()?;
        if let Some((id, provider, external_container_id, label, aud_json, synced_at)) = row {
            out.push(kp_model::SourceScope {
                scope_id: id.parse()?,
                provider,
                external_container_id,
                label,
                current_audience: serde_json::from_str(&aud_json)?,
                synced_at: EpochMs(synced_at),
            });
        }
    }
    Ok(out)
}
