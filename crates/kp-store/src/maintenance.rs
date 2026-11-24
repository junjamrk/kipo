use std::collections::{BTreeMap, BTreeSet};

use kp_model::{
    DerivationMode, EpochMs, Fact, FactAccess, FactId, FactStatus, Justification, KpError,
    SlotContract,
};
use rusqlite::{params, Connection, TransactionBehavior};

use crate::facts::{read_graph, write_fact, write_fact_indexed, write_justification};
use crate::{Embedder, Result, Store, Tokenizer};

impl Store {
    pub fn slots(&self) -> Result<BTreeMap<String, SlotContract>> {
        let mut stmt = self
            .db
            .prepare("SELECT predicate FROM schema_slots ORDER BY predicate")?;
        let predicates = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut slots = BTreeMap::new();
        for predicate in predicates {
            if let Some(slot) = crate::slots::read_slot(&self.db, &predicate)? {
                slots.insert(predicate, slot);
            }
        }
        Ok(slots)
    }

    pub fn entities(&self) -> Result<BTreeMap<FactId, BTreeSet<String>>> {
        let mut result = BTreeMap::<FactId, BTreeSet<String>>::new();
        let mut stmt = self.db.prepare(
            "SELECT fact_id,entity_slug FROM fact_entities ORDER BY fact_id,entity_slug",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            result
                .entry(row.get::<_, String>(0)?.parse()?)
                .or_default()
                .insert(row.get(1)?);
        }
        Ok(result)
    }

    pub fn stale_anchors(&self) -> Result<BTreeSet<FactId>> {
        let mut stmt = self.db.prepare("SELECT DISTINCT fact_id FROM code_anchors WHERE verify_status IN ('sig_changed','orphaned')")?;
        let result = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .map(|r| Ok(r?.parse()?))
            .collect();
        result
    }

    pub fn set_anchor_status(&mut self, fact: &FactId, status: &str, at: EpochMs) -> Result<()> {
        if !matches!(
            status,
            "verified" | "moved_unchanged" | "body_modified" | "sig_changed" | "orphaned"
        ) {
            return Err(KpError::invalid("anchor", "unknown verification status").into());
        }
        self.db.execute("INSERT INTO code_anchors (anchor_id,fact_id,verify_status,verified_at) VALUES (?1,?1,?2,?3)
            ON CONFLICT(anchor_id) DO UPDATE SET verify_status=excluded.verify_status,verified_at=excluded.verified_at", params![fact.as_str(),status,at.0])?;
        Ok(())
    }

    /// Commits conclusions and complete conjunctive proofs together, including cyclic graphs.
    pub fn save_derivations(&mut self, facts: &[Fact], proofs: &[Justification]) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        save_derivations_tx(&tx, facts, proofs)?;
        tx.commit()?;
        Ok(())
    }

    pub fn expire(&mut self, at: EpochMs) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let graph = read_graph(&tx)?;
        for fact in graph.facts.values() {
            if fact.status == FactStatus::Active
                && matches!(&fact.access, FactAccess::Base { stamp, .. } if stamp.valid.valid_until <= at)
            {
                tx.execute(
                    "UPDATE facts SET expired_at=?1 WHERE fact_id=?2",
                    params![at.0, fact.id.as_str()],
                )?;
                invalidate(&tx, &fact.id, at, "expired")?;
            }
        }
        tx.commit()?;
        self.sync_skills_now()?;
        Ok(())
    }

    pub fn reconcile_fact(
        &mut self,
        id: &FactId,
        confirmed: Option<bool>,
        at: EpochMs,
    ) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let graph = read_graph(&tx)?;
        let fact = graph.facts.get(id).ok_or(crate::StoreError::NotFound)?;
        if matches!(
            fact.status,
            FactStatus::Active | FactStatus::UnreachableHidden
        ) {
            match confirmed {
                Some(true) => {
                    tx.execute(
                        "UPDATE facts SET status='active',miss_count=0 WHERE fact_id=?1",
                        [id.as_str()],
                    )?;
                }
                Some(false) => {
                    tx.execute(
                        "UPDATE facts SET status='invalidated' WHERE fact_id=?1",
                        [id.as_str()],
                    )?;
                    invalidate(&tx, id, at, "origin_withdrawn")?;
                }
                None => {
                    // Transient network/origin misses hide mirrors without permanently invalidating
                    // them or cascading destruction, recovering cleanly once the origin returns.
                    let misses = fact.miss_count.saturating_add(1);
                    tx.execute(
                        "UPDATE facts SET status='unreachable_hidden',miss_count=?1 WHERE fact_id=?2",
                        params![misses, id.as_str()],
                    )?;
                }
            }
        }
        tx.commit()?;
        self.sync_skills_now()?;
        Ok(())
    }

    pub fn narrow_audience(
        &mut self,
        id: &FactId,
        audience: &kp_model::AudienceSpec,
    ) -> Result<()> {
        if matches!(audience, kp_model::AudienceSpec::SemiringDerived) {
            return Err(KpError::invalid("audience", "base audience required").into());
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE facts SET audience_json=?1 WHERE fact_id=?2 AND audience_json != '\"__semiring__\"'",
            params![serde_json::to_string(audience)?, id.as_str()],
        )?;
        if changed > 0 {
            use rusqlite::OptionalExtension;
            if let Some((origin_str, key_str, author_str)) = tx
                .query_row(
                    "SELECT origin_library_id, logical_key, author_actor FROM facts WHERE fact_id=?1",
                    [id.as_str()],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
                )
                .optional()?
            {
                let graph = read_graph(&tx)?;
                crate::exchange::cancel_unrevised_self_only_pending_publishes_tx(
                    &tx,
                    id,
                    &origin_str.parse()?,
                    &key_str.parse()?,
                    &author_str.parse()?,
                    audience,
                    &graph.members,
                )?;
            }
            crate::exchange::mark_alignment_dirty_tx(&tx, EpochMs(0))?;
        }
        tx.commit()?;
        self.sync_skills_now()?;
        Ok(())
    }

    pub fn supersede(&mut self, old: &FactId, replacement: &Fact) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        supersede_tx(&tx, old, replacement)?;
        tx.commit()?;
        self.sync_skills_now()?;
        Ok(())
    }
}

pub(crate) fn save_derivations_tx(
    tx: &Connection,
    facts: &[Fact],
    proofs: &[Justification],
) -> Result<()> {
    let existing = read_graph(tx)?;
    for fact in facts {
        if let Some(old) = existing.facts.get(&fact.id) {
            if old.strong_ref() != fact.strong_ref() {
                return Err(crate::StoreError::Conflict);
            }
            if old.status == FactStatus::Invalidated && fact.status == FactStatus::Active {
                tx.execute(
                    "UPDATE facts SET status='active',expired_at=NULL WHERE fact_id=?1",
                    [fact.id.as_str()],
                )?;
            }
        } else {
            let opaque = crate::tombstone_id(
                fact.origin_library.as_str().as_bytes(),
                &fact.origin_library,
                &fact.logical_key,
            )?;
            write_fact(tx, fact, &opaque)?;
        }
    }
    for proof in proofs {
        if let Some(old) = existing.justifications.get(&proof.id) {
            if old.conclusion != proof.conclusion || old.premises != proof.premises {
                return Err(crate::StoreError::Conflict);
            }
            tx.execute(
                "UPDATE justifications SET active=1,invalidated_at=NULL,invalidation_reason=NULL WHERE just_id=?1",
                [proof.id.as_str()],
            )?;
        } else {
            write_justification(tx, proof)?;
        }
    }
    Ok(())
}

pub(crate) fn supersede_tx(tx: &Connection, old: &FactId, replacement: &Fact) -> Result<()> {
    let opaque = crate::tombstone_id(
        replacement.origin_library.as_str().as_bytes(),
        &replacement.origin_library,
        &replacement.logical_key,
    )?;
    write_fact(tx, replacement, &opaque)?;
    finish_supersede_tx(tx, old, replacement)
}

pub(crate) fn supersede_indexed_tx(
    tx: &Connection,
    old: &FactId,
    replacement: &Fact,
    tokenizer: &Tokenizer,
    embedder: &(impl Embedder + ?Sized),
) -> Result<()> {
    let opaque = crate::tombstone_id(
        replacement.origin_library.as_str().as_bytes(),
        &replacement.origin_library,
        &replacement.logical_key,
    )?;
    write_fact_indexed(tx, replacement, &opaque, tokenizer, embedder)?;
    finish_supersede_tx(tx, old, replacement)
}

fn finish_supersede_tx(tx: &Connection, old: &FactId, replacement: &Fact) -> Result<()> {
    let valid_from = match &replacement.access {
        FactAccess::Base { stamp, .. } => Some(stamp.valid.valid_from.0),
        FactAccess::Derived { .. } => None,
    };
    let changed = tx.execute(
        "UPDATE facts SET status='superseded',superseded_by=?1,
        valid_until=CASE WHEN valid_from < ?3 THEN ?3 ELSE valid_until END
        WHERE fact_id=?2 AND status='active'",
        params![replacement.id.as_str(), old.as_str(), valid_from],
    )?;
    if changed == 0 {
        return Err(crate::StoreError::NotFound);
    }
    invalidate(tx, old, replacement.access.recorded_at(), "superseded")?;
    crate::exchange::cancel_publishes_and_queue_withdrawals(tx, old)?;
    if replacement.predicate != "kp.core.concept_alignment" {
        crate::exchange::mark_alignment_dirty_tx(tx, replacement.access.recorded_at())?;
    }
    Ok(())
}

pub(crate) fn invalidate(
    db: &Connection,
    premise: &FactId,
    at: EpochMs,
    reason: &str,
) -> Result<()> {
    let any_derived_or_proofs: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM justification_premises WHERE premise_fact_id=?1)
         OR EXISTS(SELECT 1 FROM facts WHERE audience_json='\"__semiring__\"' AND status='active' AND expired_at IS NULL)",
        [premise.as_str()],
        |r| r.get(0),
    )?;
    if !any_derived_or_proofs {
        return Ok(());
    }
    let mut pending = vec![premise.clone()];
    let mut seen = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let graph = read_graph(db)?;
        let mut rsi_touched = BTreeSet::new();
        for proof in graph.justifications.values().filter(|j| {
            (j.active
                || matches!(
                    j.mode,
                    DerivationMode::ScopedSynthesis
                        | DerivationMode::RsiAttribution
                        | DerivationMode::AssumedPremise
                ))
                && j.premises.iter().any(|p| p.fact_id == id)
        }) {
            let reason = format!("premise_{reason}");
            db.execute("UPDATE justifications SET active=0,invalidated_at=?1,invalidation_reason=?2 WHERE just_id=?3", params![at.0,reason,proof.id.as_str()])?;
            if matches!(
                proof.mode,
                DerivationMode::ScopedSynthesis | DerivationMode::AssumedPremise
            ) {
                let status = if reason == "premise_superseded" {
                    "stale_premise"
                } else {
                    "quarantined"
                };
                db.execute("UPDATE facts SET status=?1 WHERE fact_id=?2 AND status IN ('active','stale_premise','unreachable_hidden')", params![status,proof.conclusion.as_str()])?;
                crate::exchange::cancel_publishes_and_queue_withdrawals(db, &proof.conclusion)?;
                pending.push(proof.conclusion.clone());
            } else if proof.mode == DerivationMode::RsiAttribution {
                rsi_touched.insert(proof.conclusion.clone());
            }
        }
        if !rsi_touched.is_empty() {
            crate::rsi::reconcile_rsi_conclusions(db, &rsi_touched, at)?;
        }
        let graph = read_graph(db)?;
        let grounded = graph.grounded_support();
        for fact in graph.facts.values().filter(|f| {
            f.status == FactStatus::Active
                && matches!(f.access, FactAccess::Derived { .. })
                && !grounded.contains(&f.id)
        }) {
            db.execute(
                "UPDATE facts SET status='invalidated',expired_at=?1 WHERE fact_id=?2",
                params![at.0, fact.id.as_str()],
            )?;
            crate::exchange::cancel_publishes_and_queue_withdrawals(db, &fact.id)?;
            pending.push(fact.id.clone());
        }
    }
    Ok(())
}

impl Store {
    pub fn retain_hard_proofs(
        &mut self,
        keep: &BTreeSet<kp_model::JustificationId>,
        at: EpochMs,
    ) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let graph = read_graph(&tx)?;
        let mut removed = BTreeSet::new();
        for proof in graph
            .justifications
            .values()
            .filter(|p| {
                p.mode == DerivationMode::HardRule
                    && p.active
                    && !keep.contains(&p.id)
                    // Imported proofs belong to the origin's rule set and anchor state.
                    && graph.facts.get(&p.conclusion).is_some_and(|f| f.origin_library == self.library_id)
            })
        {
            tx.execute("UPDATE justifications SET active=0,invalidated_at=?1,invalidation_reason='rule_no_longer_matches' WHERE just_id=?2", params![at.0,proof.id.as_str()])?;
            removed.insert(proof.conclusion.clone());
        }
        let grounded = read_graph(&tx)?.grounded_support();
        for id in removed.difference(&grounded) {
            tx.execute(
                "UPDATE facts SET status='invalidated' WHERE fact_id=?1 AND status='active'",
                [id.as_str()],
            )?;
            invalidate(&tx, id, at, "rule_no_longer_matches")?;
        }
        tx.commit()?;
        self.sync_skills_now()?;
        Ok(())
    }

    pub fn rule_definitions(&self) -> Result<Vec<serde_json::Value>> {
        let mut stmt = self.db.prepare(
            "SELECT payload_json FROM change_log WHERE op='horn_rule' ORDER BY target_id",
        )?;
        let result = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .map(|row| Ok(serde_json::from_str(&row?)?))
            .collect();
        result
    }

    pub fn register_rule(&mut self, id: &str, definition: &serde_json::Value) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM change_log WHERE op='horn_rule' AND target_id=?1",
            [id],
        )?;
        tx.execute("INSERT INTO change_log (op,target_id,payload_json,created_at) VALUES ('horn_rule',?1,?2,0)", params![id,serde_json::to_string(definition)?])?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "maintenance_tests.rs"]
mod tests;
