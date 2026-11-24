use kp_model::{DerivationMode, FactKind, LibraryMode};

use super::*;
use crate::test_support::{fact, library, AT};

#[test]
fn missing_justification_conclusion_returns_not_found() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let base = fact("base", &["owner"])?;
    let missing = crate::test_support::derived("missing")?;
    store.insert_fact(&base)?;
    let support = crate::test_support::justification("support", &missing, &[&base])?;
    assert!(matches!(
        store.insert_justification(&support),
        Err(StoreError::NotFound)
    ));
    assert_eq!(
        store
            .db
            .query_row("SELECT count(*) FROM justifications", [], |r| r
                .get::<_, i64>(0))?,
        0
    );
    Ok(())
}

#[test]
fn stored_facts_round_trip_and_derived_access_stays_read_time() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let base = fact("base", &["owner"])?;
    let mut conclusion = fact("conclusion", &[])?;
    conclusion.kind = FactKind::DerivedFact;
    conclusion.access = FactAccess::Derived {
        recorded_at: AT,
        expired_at: None,
    };
    store.insert_fact(&base)?;
    store.insert_fact(&conclusion)?;
    let support = Justification {
        id: "support".parse()?,
        conclusion: conclusion.id.clone(),
        mode: DerivationMode::HardRule,
        rule_id: "rule".into(),
        rule_version: "1".into(),
        active: true,
        created_at: AT,
        invalidated_at: None,
        invalidation_reason: None,
        premises: vec![Premise {
            fact_id: base.id.clone(),
            strong_ref: base.strong_ref(),
        }],
    };
    store.insert_justification(&support)?;
    assert_eq!(
        store.visible_facts(&"owner".parse()?, AT)?,
        vec![base.clone(), conclusion.clone()]
    );
    assert!(store.visible_facts(&"outsider".parse()?, AT)?.is_empty());
    let (audience, from, until): (String, i64, i64) = store.db.query_row(
        "SELECT audience_json,valid_from,valid_until FROM facts WHERE fact_id='conclusion'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    assert_eq!(
        (audience.as_str(), from, until),
        ("\"__semiring__\"", 0, i64::MAX)
    );
    store.db.execute(
        "UPDATE facts SET audience_json='[]' WHERE fact_id='base'",
        [],
    )?;
    assert!(store.visible_facts(&"owner".parse()?, AT)?.is_empty());
    store.purge(&base.origin_library, &base.logical_key, AT, &[])?;
    assert_eq!(store.status()?.facts, 0);
    assert!(matches!(
        store.insert_fact(&conclusion),
        Err(StoreError::Tombstoned(_))
    ));
    assert_eq!(
        store
            .db
            .query_row("SELECT count(*) FROM justifications", [], |r| r
                .get::<_, i64>(0))?,
        0
    );
    Ok(())
}

#[test]
fn invalid_strong_ref_rolls_back_the_whole_justification() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let base = fact("base", &["owner"])?;
    let mut conclusion = fact("conclusion", &[])?;
    conclusion.kind = FactKind::DerivedFact;
    conclusion.access = FactAccess::Derived {
        recorded_at: AT,
        expired_at: None,
    };
    store.insert_fact(&base)?;
    store.insert_fact(&conclusion)?;
    let mut reference = base.strong_ref();
    reference.content_sha256 = kp_model::ContentHash::digest("wrong");
    let support = Justification {
        id: "bad".parse()?,
        conclusion: conclusion.id,
        mode: DerivationMode::HardRule,
        rule_id: "rule".into(),
        rule_version: "1".into(),
        active: true,
        created_at: AT,
        invalidated_at: None,
        invalidation_reason: None,
        premises: vec![Premise {
            fact_id: base.id,
            strong_ref: reference,
        }],
    };
    assert!(matches!(
        store.insert_justification(&support),
        Err(StoreError::Conflict)
    ));
    assert_eq!(
        store
            .db
            .query_row("SELECT count(*) FROM justifications", [], |r| r
                .get::<_, i64>(0))?,
        0
    );
    Ok(())
}

#[test]
fn purging_hidden_support_preserves_independent_visible_proofs() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Community)?;
    let visible = fact("visible", &["C"])?;
    let hidden = fact("hidden", &["A"])?;
    let mut conclusion = fact("conclusion", &[])?;
    conclusion.kind = FactKind::DerivedFact;
    conclusion.access = FactAccess::Derived {
        recorded_at: AT,
        expired_at: None,
    };
    store.insert_fact(&visible)?;
    store.insert_fact(&hidden)?;
    store.insert_fact(&conclusion)?;
    for base in [&visible, &hidden] {
        store.insert_justification(&Justification {
            id: base.id.as_str().parse()?,
            conclusion: conclusion.id.clone(),
            mode: DerivationMode::HardRule,
            rule_id: "rule".into(),
            rule_version: "1".into(),
            active: true,
            created_at: AT,
            invalidated_at: None,
            invalidation_reason: None,
            premises: vec![Premise {
                fact_id: base.id.clone(),
                strong_ref: base.strong_ref(),
            }],
        })?;
    }
    let viewer = "C".parse()?;
    let before = serde_json::to_vec(&store.derivation_view()?.evaluate(&viewer, AT))?;
    let facts = serde_json::to_vec(&store.visible_facts(&viewer, AT)?)?;
    store.purge(&hidden.origin_library, &hidden.logical_key, AT, &[])?;
    assert_eq!(
        serde_json::to_vec(&store.derivation_view()?.evaluate(&viewer, AT))?,
        before
    );
    assert_eq!(
        serde_json::to_vec(&store.visible_facts(&viewer, AT)?)?,
        facts
    );
    assert!(!store.is_tombstoned(&conclusion.origin_library, &conclusion.logical_key)?);
    Ok(())
}

#[test]
fn purge_does_not_leave_an_ungrounded_derivation_cycle() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let base = fact("base", &["owner"])?;
    let mut x = fact("x", &[])?;
    x.kind = FactKind::DerivedFact;
    x.access = FactAccess::Derived {
        recorded_at: AT,
        expired_at: None,
    };
    let mut y = x.clone();
    y.id = "y".parse()?;
    y.logical_key = "y".parse()?;
    for node in [&base, &x, &y] {
        store.insert_fact(node)?;
    }
    for (id, head, premise) in [("ground", &x, &base), ("xy", &x, &y), ("yx", &y, &x)] {
        store.insert_justification(&Justification {
            id: id.parse()?,
            conclusion: head.id.clone(),
            mode: DerivationMode::HardRule,
            rule_id: "rule".into(),
            rule_version: "1".into(),
            active: true,
            created_at: AT,
            invalidated_at: None,
            invalidation_reason: None,
            premises: vec![Premise {
                fact_id: premise.id.clone(),
                strong_ref: premise.strong_ref(),
            }],
        })?;
    }
    assert_eq!(store.visible_facts(&"owner".parse()?, AT)?.len(), 3);
    store.purge(&base.origin_library, &base.logical_key, AT, &[])?;
    assert_eq!(store.status()?.facts, 0);
    assert!(store.is_tombstoned(&x.origin_library, &x.logical_key)?);
    assert!(store.is_tombstoned(&y.origin_library, &y.logical_key)?);
    Ok(())
}
