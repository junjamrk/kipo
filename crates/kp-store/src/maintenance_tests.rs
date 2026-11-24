use kp_model::{EpochMs, FactAccess, FactStatus, LibraryMode};

use crate::test_support::{fact, library};
use crate::Result;

#[test]
fn local_rule_retirement_preserves_imported_proofs_until_their_premise_is_withdrawn() -> Result<()>
{
    use kp_model::{derived_fact, justification, DerivationMode, FactKind};

    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let premise = fact("premise", &["owner"])?;
    store.insert_fact(&premise)?;
    let make = |origin: &str| {
        derived_fact(
            &origin.parse()?,
            &"owner".parse()?,
            FactKind::CollisionWarning,
            "weather",
            "kp.core.collision",
            serde_json::json!("rain conflicts with driving"),
            EpochMs(10),
        )
    };
    let local = make("local")?;
    let remote = make("community")?;
    let local_proof = justification(&local, DerivationMode::HardRule, "R2", &[&premise])?;
    let remote_proof = justification(&remote, DerivationMode::HardRule, "R2", &[&premise])?;
    store.save_derivations(
        &[local.clone(), remote.clone()],
        &[local_proof.clone(), remote_proof.clone()],
    )?;
    store.retain_hard_proofs(&Default::default(), EpochMs(20))?;
    let graph = store.derivation_view()?;
    assert_eq!(graph.facts[&local.id].status, FactStatus::Invalidated);
    assert!(!graph.justifications[&local_proof.id].active);
    assert_eq!(graph.facts[&remote.id].status, FactStatus::Active);
    assert!(graph.justifications[&remote_proof.id].active);
    assert!(graph
        .evaluate(&"owner".parse()?, EpochMs(20))
        .facts
        .contains(&remote.id));
    store.forget(premise.id.as_str(), &"owner".parse()?, EpochMs(30))?;
    assert_eq!(
        store.derivation_view()?.facts[&remote.id].status,
        FactStatus::Invalidated
    );
    Ok(())
}

#[test]
fn supersession_closes_old_validity_at_later_replacement() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let old = fact("old", &["owner"])?;
    let mut next = fact("next", &["owner"])?;
    if let FactAccess::Base { stamp, .. } = &mut next.access {
        stamp.valid.valid_from = EpochMs(20);
    }
    store.insert_fact(&old)?;
    store.supersede(&old.id, &next)?;
    let graph = store.derivation_view()?;
    assert_eq!(graph.facts[&old.id].status, FactStatus::Superseded);
    assert!(
        matches!(&graph.facts[&old.id].access, FactAccess::Base { stamp, .. } if stamp.valid.valid_until == EpochMs(20))
    );
    let before = store.retrieval_snapshot(&"owner".parse()?, EpochMs(19))?;
    assert!(before.documents().iter().any(|d| d.id == "fact-old"));
    let after = store.retrieval_snapshot(&"owner".parse()?, EpochMs(20))?;
    assert!(!after.documents().iter().any(|d| d.id == "fact-old"));
    assert!(after.documents().iter().any(|d| d.id == "fact-next"));
    Ok(())
}

#[test]
fn supersession_preserves_valid_window_for_equal_or_earlier_replacement() -> Result<()> {
    for from in [0, 10] {
        let (_dir, mut store) = library(LibraryMode::Personal)?;
        let mut old = fact("old", &["owner"])?;
        if let FactAccess::Base { stamp, .. } = &mut old.access {
            stamp.valid.valid_from = EpochMs(10);
            stamp.valid.valid_until = EpochMs(30);
        }
        let mut next = fact("next", &["owner"])?;
        if let FactAccess::Base { stamp, .. } = &mut next.access {
            stamp.valid.valid_from = EpochMs(from);
        }
        store.insert_fact(&old)?;
        store.supersede(&old.id, &next)?;
        assert!(
            matches!(&store.derivation_view()?.facts[&old.id].access, FactAccess::Base { stamp, .. } if stamp.valid.valid_until == EpochMs(30))
        );
    }
    Ok(())
}

#[test]
fn repeated_transient_origin_misses_stay_unreachable_hidden_and_recover_cleanly() -> Result<()> {
    use kp_model::{derived_fact, justification, DerivationMode, FactKind};

    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let owner = "owner".parse()?;
    let remote = fact("remote-premise", &["owner"])?;
    store.insert_fact(&remote)?;

    let derived = derived_fact(
        &"local".parse()?,
        &owner,
        FactKind::CollisionWarning,
        "alice",
        "kp.core.collision",
        serde_json::json!("周四暂停离线报表批处理"),
        EpochMs(10),
    )?;
    let proof = justification(&derived, DerivationMode::HardRule, "R2", &[&remote])?;
    store.save_derivations(std::slice::from_ref(&derived), std::slice::from_ref(&proof))?;

    for step in 1..=10_u32 {
        store.reconcile_fact(&remote.id, None, EpochMs(i64::from(step) * 100))?;
        let graph = store.derivation_view()?;
        assert_eq!(
            graph.facts[&remote.id].status,
            FactStatus::UnreachableHidden
        );
        assert_eq!(graph.facts[&remote.id].miss_count, step);
        // Downstream conclusion is hidden at read time while premise is unreachable, NOT permanently invalidated.
        assert_eq!(graph.facts[&derived.id].status, FactStatus::Active);
        assert!(graph.justifications[&proof.id].active);
        assert!(store
            .visible_facts(&owner, EpochMs(i64::from(step) * 100))?
            .is_empty());
    }

    // Origin comes back online: premise & derived conclusion immediately recover!
    store.reconcile_fact(&remote.id, Some(true), EpochMs(2_000))?;
    let graph = store.derivation_view()?;
    assert_eq!(graph.facts[&remote.id].status, FactStatus::Active);
    assert_eq!(graph.facts[&remote.id].miss_count, 0);
    assert_eq!(store.visible_facts(&owner, EpochMs(2_000))?.len(), 2);

    // Explicit origin withdrawal (`Some(false)`) permanently invalidates both premise and proof.
    store.reconcile_fact(&remote.id, Some(false), EpochMs(3_000))?;
    let graph = store.derivation_view()?;
    assert_eq!(graph.facts[&remote.id].status, FactStatus::Invalidated);
    assert_eq!(graph.facts[&derived.id].status, FactStatus::Invalidated);
    assert!(!graph.justifications[&proof.id].active);
    Ok(())
}
