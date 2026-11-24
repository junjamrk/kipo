use std::collections::{BTreeMap, BTreeSet};

use kp_model::{
    canonical_json, derived_fact, ActorId, AudienceSpec, Cardinality, ContentHash, DerivationMode,
    EpochMs, Fact, FactAccess, FactId, FactKind, FactStatus, Justification, KpError, Mutability,
    Premise, SlotContract, StrongRef,
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;

use crate::embedding::{sanitize_and_validate_embedding, FtsOnlyEmbedder};
use crate::facts::{is_fact_raw_only, write_fact_indexed_with_hash, write_justification};
use crate::maintenance::invalidate;
use crate::tombstone::tombstone_id;
use crate::{Embedder, Result, Store, StoreError, Tokenizer};

#[derive(Clone, Debug)]
pub struct FactInput {
    pub fact: Fact,
    pub entities: BTreeSet<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct Remembered {
    pub facts: Vec<FactId>,
    pub conflicts: Vec<FactId>,
    pub superseded: Vec<FactId>,
}

impl Store {
    /// Defines an immutable slot contract; redefining a predicate with different semantics fails.
    pub fn define_slot(&mut self, contract: &SlotContract) -> Result<()> {
        validate_contract(contract)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = read_slot(&tx, &contract.predicate)? {
            if existing != *contract {
                return Err(StoreError::Conflict);
            }
        } else {
            write_slot(&tx, contract)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Applies EDC output, slot transitions, provenance, and entity incidence atomically.
    pub fn remember(&mut self, inputs: &[FactInput]) -> Result<Remembered> {
        let embedder = self.embedder.clone();
        self.remember_with_embedder(inputs, embedder.as_ref())
    }

    /// Alias for [`Store::remember`] using `self.embedder()`.
    pub fn remember_facts(&mut self, inputs: &[FactInput]) -> Result<Remembered> {
        self.remember(inputs)
    }

    /// Applies EDC output, slot transitions, provenance, and entity incidence atomically with `embedder`.
    pub fn remember_with_embedder(
        &mut self,
        inputs: &[FactInput],
        embedder: &(impl Embedder + ?Sized),
    ) -> Result<Remembered> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = remember_facts_indexed(&tx, inputs, &self.tokenizer, embedder)?;
        tx.commit()?;
        self.register_entities(inputs);
        if !inputs.is_empty() {
            self.sync_skills_now()?;
        }
        Ok(result)
    }

    pub(crate) fn register_entities(&mut self, inputs: &[FactInput]) {
        for input in inputs {
            for word in input
                .entities
                .iter()
                .chain(std::iter::once(&input.fact.subject))
            {
                self.tokenizer.add_entity_word(word);
            }
        }
    }

    /// Withdraws one fact ID or every version of a local logical key; retains audit rows and proofs.
    pub fn forget(&mut self, target: &str, actor: &ActorId, at: EpochMs) -> Result<Vec<FactId>> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ids = {
            let mut stmt = tx.prepare(
                "SELECT fact_id FROM facts WHERE fact_id=?1
                OR (origin_library_id=?2 AND logical_key=?1) ORDER BY fact_id",
            )?;
            let ids = stmt.query_map(params![target, self.library_id.as_str()], |r| {
                r.get::<_, String>(0)
            })?;
            ids.collect::<std::result::Result<Vec<_>, _>>()?
        };
        if ids.is_empty() {
            return Err(StoreError::NotFound);
        }
        let withdrawn = withdraw_facts(&tx, ids, actor, at)?;
        tx.commit()?;
        self.sync_skills_now()?;
        Ok(withdrawn)
    }

    /// Withdraws exactly one fact, retaining audit rows and invalidating dependent proofs.
    /// Returns `NotFound` if the ID does not exist; repeated withdrawals are idempotent.
    pub fn forget_fact_id(
        &mut self,
        id: &FactId,
        actor: &ActorId,
        at: EpochMs,
    ) -> Result<Vec<FactId>> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id: String = tx
            .query_row(
                "SELECT fact_id FROM facts WHERE fact_id=?1",
                [id.as_str()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        let withdrawn = withdraw_facts(&tx, vec![id], actor, at)?;
        tx.commit()?;
        self.sync_skills_now()?;
        Ok(withdrawn)
    }
}

pub(crate) fn withdraw_facts(
    db: &Connection,
    ids: Vec<String>,
    actor: &ActorId,
    at: EpochMs,
) -> Result<Vec<FactId>> {
    let mut withdrawn = Vec::new();
    for id in ids {
        let id: FactId = id.parse()?;
        let changed = db.execute(
            "UPDATE facts SET status='withdrawn',expired_at=?1
                WHERE fact_id=?2 AND expired_at IS NULL",
            params![at.0, id.as_str()],
        )?;
        if changed > 0 {
            invalidate(db, &id, at, "withdrawn")?;
            crate::exchange::cancel_publishes_and_queue_withdrawals(db, &id)?;
            if let Some((origin_str, key_str, sha_str)) = db
                .query_row(
                    "SELECT origin_library_id, logical_key, content_sha256 FROM facts WHERE fact_id=?1",
                    [id.as_str()],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
                )
                .optional()?
            {
                crate::exchange::record_withdrawn_version_tx(
                    db,
                    &origin_str.parse()?,
                    &key_str.parse()?,
                    &sha_str.parse()?,
                    actor,
                    at,
                )?;
            }
            db.execute("INSERT INTO change_log (op,target_id,origin_library_id,logical_key,actor_id,created_at)
                    SELECT 'forget',fact_id,origin_library_id,logical_key,?2,?3 FROM facts WHERE fact_id=?1",
                    params![id.as_str(),actor.as_str(),at.0])?;
        }
        withdrawn.push(id);
    }
    crate::exchange::mark_alignment_dirty_tx(db, at)?;
    Ok(withdrawn)
}

pub(crate) fn read_slot(db: &Connection, predicate: &str) -> Result<Option<SlotContract>> {
    let raw = db
        .query_row(
            "SELECT namespace,cardinality,mutability,value_kind,description,instruction
        FROM schema_slots WHERE predicate=?1",
            [predicate],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            },
        )
        .optional()?;
    raw.map(
        |(namespace, cardinality, mutability, value_kind, description, instruction)| {
            Ok(SlotContract {
                predicate: predicate.into(),
                namespace,
                cardinality: cardinality.parse()?,
                mutability: mutability.parse()?,
                value_kind,
                description,
                instruction,
            })
        },
    )
    .transpose()
}

fn validate_contract(slot: &SlotContract) -> Result<()> {
    if slot.predicate.trim().is_empty()
        || slot.namespace.trim().is_empty()
        || !matches!(
            slot.value_kind.as_str(),
            "json" | "string" | "number" | "boolean" | "array" | "object"
        )
    {
        return Err(KpError::invalid("slot", "invalid predicate, namespace, or value kind").into());
    }
    Ok(())
}

pub(crate) fn validate_value(slot: &SlotContract, value: &serde_json::Value) -> Result<()> {
    let valid = match slot.value_kind.as_str() {
        "json" => true,
        "string" => value.is_string(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    };
    if !valid {
        return Err(KpError::invalid("value", "does not match slot contract").into());
    }
    Ok(())
}

fn write_slot(db: &Connection, slot: &SlotContract) -> Result<()> {
    db.execute(
        "INSERT INTO schema_slots VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![
            slot.predicate,
            slot.namespace,
            slot.cardinality.as_str(),
            slot.mutability.as_str(),
            slot.value_kind,
            slot.description,
            slot.instruction
        ],
    )?;
    Ok(())
}

#[cfg(test)]
#[path = "slots_tests.rs"]
mod tests;

struct TimeVaryingPreviousRow {
    fact_id: FactId,
    author: String,
    audience_json: String,
}

struct FixedPreviousRow {
    fact_id: FactId,
    origin_library: String,
    logical_key: String,
    content_sha256: String,
    subject: String,
    predicate: String,
    object_json: String,
    recorded_at: EpochMs,
}

pub(crate) fn prevalidate_fact_inputs(
    db: &Connection,
    inputs: &[FactInput],
    embedder: &(impl Embedder + ?Sized),
) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    let mut slot_cache = BTreeMap::<String, Option<SlotContract>>::new();
    let mut seen_ids = BTreeSet::<&str>::new();
    for input in inputs {
        let fact = &input.fact;
        let FactAccess::Base { stamp, .. } = &fact.access else {
            return Err(KpError::invalid("remember", "requires a base fact").into());
        };
        if fact.status != FactStatus::Active || stamp.expired_at.is_some() {
            return Err(KpError::invalid("remember", "requires an active fact").into());
        }
        fact.validate()?;
        if !seen_ids.insert(fact.id.as_str()) {
            return Err(StoreError::Conflict);
        }
        let existing_id: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM facts WHERE fact_id = ?1)",
            [fact.id.as_str()],
            |r| r.get(0),
        )?;
        if existing_id {
            return Err(StoreError::Conflict);
        }
        let opaque = tombstone_id(
            fact.origin_library.as_str().as_bytes(),
            &fact.origin_library,
            &fact.logical_key,
        )?;
        crate::tombstone::guard(db, &opaque)?;
        let source_collision: bool = db
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM raw_sources WHERE source_id=?1) AND (SELECT mode FROM library_meta)='community'")?
            .query_row([opaque.as_str()], |row| row.get(0))?;
        if source_collision {
            return Err(StoreError::Conflict);
        }
        if let Some(source) = fact
            .source_anchor
            .as_ref()
            .and_then(|a| a.source_id.as_ref())
        {
            if let Ok(src_opaque) = source.as_str().parse() {
                crate::tombstone::guard(db, &src_opaque)?;
            }
        }
        let cached_slot = if let Some(entry) = slot_cache.get(&fact.predicate) {
            entry.clone()
        } else {
            let loaded = read_slot(db, &fact.predicate)?;
            slot_cache.insert(fact.predicate.clone(), loaded.clone());
            loaded
        };
        if let Some(slot) = cached_slot {
            validate_value(&slot, &fact.object)?;
        }
        let skip_for_raw_only = !embedder.allows_raw_only() && is_fact_raw_only(db, fact)?;
        if embedder.is_enabled() && !skip_for_raw_only {
            let passage = crate::facts::fact_default_passage(fact);
            let model_id = embedder.model_id();
            if model_id != "hash:v1" && model_id != "custom" {
                if !passage.trim().is_empty() {
                    let vec = embedder.embed(&passage)?;
                    let _ = sanitize_and_validate_embedding(&vec)?;
                }
            } else {
                let vec = embedder.embed(&passage)?;
                crate::embedding::validate_vector(&vec)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn remember_facts_indexed(
    db: &Connection,
    inputs: &[FactInput],
    tokenizer: &Tokenizer,
    embedder: &(impl Embedder + ?Sized),
) -> Result<Remembered> {
    let mut result = Remembered::default();
    let mut slot_cache = BTreeMap::<String, SlotContract>::new();

    for input in inputs {
        let fact = &input.fact;
        let FactAccess::Base { audience, stamp } = &fact.access else {
            return Err(KpError::invalid("remember", "requires a base fact").into());
        };
        if fact.status != FactStatus::Active || stamp.expired_at.is_some() {
            return Err(KpError::invalid("remember", "requires an active fact").into());
        }
        let slot = if let Some(cached) = slot_cache.get(&fact.predicate) {
            cached.clone()
        } else {
            let resolved = match read_slot(db, &fact.predicate)? {
                Some(slot) => slot,
                None => {
                    let slot = SlotContract {
                        predicate: fact.predicate.clone(),
                        namespace: "kp.community.local".into(),
                        cardinality: Cardinality::Multi,
                        mutability: Mutability::Fixed,
                        value_kind: "json".into(),
                        description: "Locally defined predicate".into(),
                        instruction: "Preserve the original value and attribution.".into(),
                    };
                    write_slot(db, &slot)?;
                    slot
                }
            };
            slot_cache.insert(fact.predicate.clone(), resolved.clone());
            resolved
        };
        validate_value(&slot, &fact.object)?;

        let new_strong_ref = fact.strong_ref();

        // Only `Cardinality::Single` slots inspect existing overlapping rows; `Cardinality::Multi`
        // (such as observations/preferences/events) performs zero historical slot scans.
        let (tv_previous, fixed_previous) = match (slot.cardinality, slot.mutability) {
            (Cardinality::Multi, _) => (Vec::new(), Vec::new()),
            (Cardinality::Single, Mutability::TimeVarying) => {
                let mut stmt = db.prepare(
                    "SELECT fact_id, author_actor, audience_json
                     FROM facts
                     WHERE subject = ?1 AND predicate = ?2 AND origin_library_id = ?3
                       AND status = 'active' AND expired_at IS NULL
                       AND audience_json != '\"__semiring__\"'
                       AND valid_from < ?4 AND valid_until > ?5
                     ORDER BY fact_id",
                )?;
                let rows = stmt.query_map(
                    params![
                        fact.subject,
                        fact.predicate,
                        fact.origin_library.as_str(),
                        stamp.valid.valid_until.0,
                        stamp.valid.valid_from.0,
                    ],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    },
                )?;
                let mut items = Vec::new();
                for row in rows {
                    let (id_str, author, audience_json) = row?;
                    items.push(TimeVaryingPreviousRow {
                        fact_id: id_str.parse()?,
                        author,
                        audience_json,
                    });
                }
                (items, Vec::new())
            }
            (Cardinality::Single, Mutability::Fixed) => {
                let mut stmt = db.prepare(
                    "SELECT fact_id, origin_library_id, logical_key, content_sha256,
                            subject, predicate, object_json, recorded_at
                     FROM facts
                     WHERE subject = ?1 AND predicate = ?2 AND origin_library_id = ?3
                       AND status = 'active' AND expired_at IS NULL
                       AND audience_json != '\"__semiring__\"'
                       AND valid_from < ?4 AND valid_until > ?5
                     ORDER BY fact_id",
                )?;
                let rows = stmt.query_map(
                    params![
                        fact.subject,
                        fact.predicate,
                        fact.origin_library.as_str(),
                        stamp.valid.valid_until.0,
                        stamp.valid.valid_from.0,
                    ],
                    |r| {
                        Ok(FixedPreviousRow {
                            fact_id: r
                                .get::<_, String>(0)?
                                .parse()
                                .unwrap_or_else(|_| fact.id.clone()),
                            origin_library: r.get(1)?,
                            logical_key: r.get(2)?,
                            content_sha256: r.get(3)?,
                            subject: r.get(4)?,
                            predicate: r.get(5)?,
                            object_json: r.get(6)?,
                            recorded_at: EpochMs(r.get(7)?),
                        })
                    },
                )?;
                let items = rows.collect::<std::result::Result<Vec<_>, _>>()?;
                (Vec::new(), items)
            }
        };

        let opaque = tombstone_id(
            fact.origin_library.as_str().as_bytes(),
            &fact.origin_library,
            &fact.logical_key,
        )?;
        write_fact_indexed_with_hash(
            db,
            fact,
            &opaque,
            Some(&new_strong_ref.content_sha256),
            tokenizer,
            embedder,
        )?;
        for entity in input.entities.iter().chain(std::iter::once(&fact.subject)) {
            let entity = entity.trim().to_lowercase();
            if !entity.is_empty() {
                db.execute(
                    "INSERT OR IGNORE INTO fact_entities VALUES (?1,?2,'mention')",
                    params![fact.id.as_str(), entity],
                )?;
            }
        }

        if slot.cardinality == Cardinality::Single {
            match slot.mutability {
                Mutability::TimeVarying => {
                    for old in tv_previous {
                        // A private replacement must not withdraw another audience's value.
                        if old.author != fact.author.as_str() {
                            continue;
                        }
                        let old_audience: AudienceSpec = serde_json::from_str(&old.audience_json)?;
                        if old_audience != *audience {
                            continue;
                        }
                        db.execute(
                            "UPDATE facts SET status='superseded',superseded_by=?1,
                             valid_until=CASE WHEN valid_from < ?2 THEN ?2 ELSE valid_until END WHERE fact_id=?3",
                            params![fact.id.as_str(), stamp.valid.valid_from.0, old.fact_id.as_str()],
                        )?;
                        invalidate(db, &old.fact_id, stamp.recorded_at, "superseded")?;
                        crate::exchange::cancel_publishes_and_queue_withdrawals(db, &old.fact_id)?;
                        result.superseded.push(old.fact_id);
                    }
                }
                Mutability::Fixed => {
                    for old in fixed_previous {
                        // Use stored `content_sha256` directly instead of recomputing SHA-256 over JSON.
                        if old.content_sha256 == new_strong_ref.content_sha256.as_str() {
                            continue;
                        }
                        let old_object: serde_json::Value = serde_json::from_str(&old.object_json)?;
                        if old_object == fact.object {
                            continue;
                        }
                        let old_strong_ref = StrongRef {
                            origin_library: old.origin_library.parse()?,
                            logical_key: old.logical_key.parse()?,
                            content_sha256: old.content_sha256.parse()?,
                        };
                        let conflict = build_stored_slot_conflict(
                            &fact.origin_library,
                            &fact.author,
                            (
                                &old_strong_ref,
                                &old.subject,
                                &old.predicate,
                                &old_object,
                                old.recorded_at,
                            ),
                            (
                                &new_strong_ref,
                                &fact.subject,
                                &fact.predicate,
                                &fact.object,
                                stamp.recorded_at,
                            ),
                        )?;
                        let conflict_sha = conflict.strong_ref().content_sha256;
                        let conflict_opaque = tombstone_id(
                            conflict.origin_library.as_str().as_bytes(),
                            &conflict.origin_library,
                            &conflict.logical_key,
                        )?;
                        let conflict_raw_only = !embedder.allows_raw_only()
                            && (is_fact_raw_only(db, fact)?
                                || {
                                    let old_src: Option<String> = db
                                    .query_row(
                                        "SELECT json_extract(source_anchor_json, '$.source_id') FROM facts WHERE fact_id = ?1",
                                        [old.fact_id.as_str()],
                                        |r| r.get(0),
                                    )
                                    .optional()?
                                    .flatten();
                                    match old_src {
                                        Some(sid) => {
                                            crate::facts::is_source_marked_raw_only(db, &sid)?
                                        }
                                        None => false,
                                    }
                                });
                        if conflict_raw_only {
                            write_fact_indexed_with_hash(
                                db,
                                &conflict,
                                &conflict_opaque,
                                Some(&conflict_sha),
                                tokenizer,
                                &FtsOnlyEmbedder,
                            )?;
                        } else {
                            write_fact_indexed_with_hash(
                                db,
                                &conflict,
                                &conflict_opaque,
                                Some(&conflict_sha),
                                tokenizer,
                                embedder,
                            )?;
                        }
                        let proof = build_stored_justification(
                            &conflict,
                            &conflict_sha,
                            DerivationMode::HardRule,
                            "R1_SLOT_CONFLICT",
                            vec![
                                Premise {
                                    fact_id: old.fact_id,
                                    strong_ref: old_strong_ref,
                                },
                                Premise {
                                    fact_id: fact.id.clone(),
                                    strong_ref: new_strong_ref.clone(),
                                },
                            ],
                        )?;
                        write_justification(db, &proof)?;
                        result.conflicts.push(conflict.id);
                    }
                }
            }
        }
        result.facts.push(fact.id.clone());
    }
    if let Some(max_at) = inputs
        .iter()
        .filter(|i| i.fact.predicate != "kp.core.concept_alignment")
        .map(|i| i.fact.access.recorded_at())
        .max()
    {
        crate::exchange::mark_alignment_dirty_tx(db, max_at)?;
    }
    Ok(result)
}

type ConflictSlotItem<'a> = (
    &'a StrongRef,
    &'a str,
    &'a str,
    &'a serde_json::Value,
    EpochMs,
);

fn build_stored_slot_conflict(
    origin: &kp_model::LibraryId,
    author: &ActorId,
    left: ConflictSlotItem<'_>,
    right: ConflictSlotItem<'_>,
) -> Result<Fact> {
    let mut pair = [left, right];
    pair.sort_by_key(|item| item.0);
    Ok(derived_fact(
        origin,
        author,
        FactKind::Conflict,
        pair[0].1,
        pair[0].2,
        serde_json::json!({
            "values": [pair[0].3, pair[1].3],
            "premises": [pair[0].0, pair[1].0]
        }),
        left.4.max(right.4),
    )?)
}

fn build_stored_justification(
    conclusion: &Fact,
    conclusion_sha256: &ContentHash,
    mode: DerivationMode,
    rule: &str,
    mut premises: Vec<Premise>,
) -> Result<Justification> {
    premises.sort_by(|a, b| {
        a.strong_ref
            .cmp(&b.strong_ref)
            .then(a.fact_id.cmp(&b.fact_id))
    });
    premises.dedup_by(|a, b| a.fact_id == b.fact_id);
    let conclusion_ref = StrongRef {
        origin_library: conclusion.origin_library.clone(),
        logical_key: conclusion.logical_key.clone(),
        content_sha256: conclusion_sha256.clone(),
    };
    let refs: Vec<_> = premises.iter().map(|p| &p.strong_ref).collect();
    let signature = serde_json::json!([conclusion.id, conclusion_ref, mode, rule, "1", refs]);
    let hash = ContentHash::digest(canonical_json(&signature));
    Ok(Justification {
        id: format!("proof-{hash}").parse()?,
        conclusion: conclusion.id.clone(),
        mode,
        rule_id: rule.into(),
        rule_version: "1".into(),
        premises,
        active: true,
        created_at: conclusion.access.recorded_at(),
        invalidated_at: None,
        invalidation_reason: None,
    })
}
