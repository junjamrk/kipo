use crate::*;

pub(crate) fn fact(id: &str, actors: &[&str], from: i64, until: i64) -> Result<Fact> {
    Ok(Fact {
        id: id.parse()?,
        origin_library: "library".parse()?,
        logical_key: id.parse()?,
        kind: FactKind::Observation,
        subject: "weather".into(),
        predicate: "kp.core.weather".into(),
        object: serde_json::json!({"condition":"rain"}),
        tags: Default::default(),
        author: "A".parse()?,
        ingested_by: None,
        trust_tier: TrustTier::HumanVerbatim,
        access: FactAccess::Base {
            audience: AudienceSpec::Explicit(
                actors
                    .iter()
                    .map(|actor| actor.parse())
                    .collect::<Result<_>>()?,
            ),
            stamp: BitemporalStamp {
                valid: ValidityWindow {
                    valid_from: EpochMs(from),
                    valid_until: EpochMs(until),
                },
                recorded_at: EpochMs(0),
                expired_at: None,
            },
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

pub(crate) fn derived(id: &str) -> Result<Fact> {
    let mut fact = fact(id, &[], 0, 1)?;
    fact.kind = FactKind::DerivedFact;
    fact.access = FactAccess::Derived {
        recorded_at: EpochMs(0),
        expired_at: None,
    };
    Ok(fact)
}

pub(crate) fn justification(
    id: &str,
    conclusion: &Fact,
    premises: &[&Fact],
) -> Result<Justification> {
    Ok(Justification {
        id: id.parse()?,
        conclusion: conclusion.id.clone(),
        mode: DerivationMode::HardRule,
        rule_id: "test".into(),
        rule_version: "1".into(),
        active: true,
        created_at: EpochMs(0),
        invalidated_at: None,
        invalidation_reason: None,
        premises: premises
            .iter()
            .map(|fact| Premise {
                fact_id: fact.id.clone(),
                strong_ref: fact.strong_ref(),
            })
            .collect(),
    })
}

pub(crate) fn graph(facts: Vec<Fact>, justifications: Vec<Justification>) -> DerivationView {
    DerivationView {
        members: Default::default(),
        source_scopes: Default::default(),
        facts: facts
            .into_iter()
            .map(|fact| (fact.id.clone(), fact))
            .collect(),
        justifications: justifications
            .into_iter()
            .map(|justification| (justification.id.clone(), justification))
            .collect(),
    }
}
