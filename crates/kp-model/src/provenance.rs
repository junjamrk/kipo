use std::collections::BTreeSet;

use serde_json::Value;

use crate::{
    ActorId, ContentHash, DerivationMode, EpochMs, Fact, FactAccess, FactKind, FactRank,
    FactStatus, Justification, LibraryId, Premise, Result, TrustTier,
};

pub fn derived_fact(
    origin: &LibraryId,
    author: &ActorId,
    kind: FactKind,
    subject: &str,
    predicate: &str,
    object: Value,
    at: EpochMs,
) -> Result<Fact> {
    let hash = crate::compute_fact_content_hash(subject, predicate, &object, kind);
    // Logical keys remain stable so pre-existing StrongRefs and tombstones still apply.
    let logical_key = format!("derived-{hash}").parse()?;
    // A personal library can re-derive an imported conclusion with identical content.
    // Its local identity must not overwrite the other library's StrongRef and proofs.
    let hash = ContentHash::digest(crate::canonical_json(&serde_json::json!([origin, hash])));
    let id = format!("derived-{hash}");
    Ok(Fact {
        id: id.parse()?,
        origin_library: origin.clone(),
        logical_key,
        kind,
        subject: subject.into(),
        predicate: predicate.into(),
        object,
        tags: BTreeSet::new(),
        author: author.clone(),
        ingested_by: None,
        trust_tier: TrustTier::ToolOrModelVerified,
        access: FactAccess::Derived {
            recorded_at: at,
            expired_at: None,
        },
        source_scope_id: None,
        governed_scope: None,
        broader_scope: None,
        status: FactStatus::Active,
        superseded_by: None,
        rank: FactRank::Normal,
        source_anchor: None,
        unverified_gaps: Vec::new(),
        miss_count: 0,
    })
}

/// A proof is a conjunction, never a union of alternative lineages.
pub fn justification(
    conclusion: &Fact,
    mode: DerivationMode,
    rule: &str,
    premises: &[&Fact],
) -> Result<Justification> {
    let mut premises: Vec<_> = premises
        .iter()
        .map(|p| Premise {
            fact_id: p.id.clone(),
            strong_ref: p.strong_ref(),
        })
        .collect();
    premises.sort_by(|a, b| {
        a.strong_ref
            .cmp(&b.strong_ref)
            .then(a.fact_id.cmp(&b.fact_id))
    });
    premises.dedup_by(|a, b| a.fact_id == b.fact_id);
    let refs: Vec<_> = premises.iter().map(|p| &p.strong_ref).collect();
    let signature = serde_json::json!([
        conclusion.id,
        conclusion.strong_ref(),
        mode,
        rule,
        "1",
        refs
    ]);
    let hash = ContentHash::digest(crate::canonical_json(&signature));
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

pub fn slot_conflict(
    origin: &LibraryId,
    author: &ActorId,
    left: &Fact,
    right: &Fact,
) -> Result<Fact> {
    let mut pair = [left, right];
    pair.sort_by_key(|f| f.strong_ref());
    derived_fact(
        origin,
        author,
        FactKind::Conflict,
        &pair[0].subject,
        &pair[0].predicate,
        serde_json::json!({"values": [pair[0].object, pair[1].object], "premises": [pair[0].strong_ref(), pair[1].strong_ref()]}),
        left.access.recorded_at().max(right.access.recorded_at()),
    )
}

#[cfg(test)]
#[path = "provenance_tests.rs"]
mod tests;
