use std::collections::BTreeSet;
use std::error::Error;
use std::future::Future;
use std::pin::Pin;

use kp_derive::{
    derive, derive_with_rules, synthesize_scoped, verify_hard_rules_match_full_recompute, Atom,
    HornRule, ScopedSynthesizer, StructuredSynthesizer, SynthesisClaim, SynthesisInput, Term,
};
use kp_model::*;
use kp_store::{InitOptions, Store};
use serde_json::json;

type Result<T = ()> = std::result::Result<T, Box<dyn Error>>;

fn library() -> Result<(tempfile::TempDir, Store)> {
    let dir = tempfile::tempdir()?;
    let store = Store::init(
        dir.path(),
        InitOptions {
            library_id: "q".parse()?,
            name: "Q".into(),
            mode: LibraryMode::Community,
            owner: "alice".parse()?,
            created_at: EpochMs(0),
        },
    )?;
    Ok((dir, store))
}
fn fact(
    id: &str,
    subject: &str,
    predicate: &str,
    object: &str,
    kind: FactKind,
    actors: &[&str],
) -> Result<Fact> {
    Ok(Fact {
        id: id.parse()?,
        origin_library: "q".parse()?,
        logical_key: id.parse()?,
        kind,
        subject: subject.into(),
        predicate: predicate.into(),
        object: json!(object),
        tags: BTreeSet::new(),
        author: "alice".parse()?,
        ingested_by: None,
        trust_tier: TrustTier::HumanVerbatim,
        access: FactAccess::Base {
            audience: AudienceSpec::Explicit(
                actors
                    .iter()
                    .map(|a| a.parse())
                    .collect::<kp_model::Result<_>>()?,
            ),
            stamp: BitemporalStamp {
                valid: ValidityWindow {
                    valid_from: EpochMs(0),
                    valid_until: EpochMs(100),
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
fn atom(predicate: &str, subject: &str, object: &str) -> Atom {
    Atom {
        predicate: predicate.into(),
        subject: Term::Variable(subject.into()),
        object: Term::Variable(object.into()),
    }
}
fn rule(id: &str, head: &str, body: Vec<Atom>) -> HornRule {
    HornRule {
        id: id.into(),
        head: atom(head, "x", "z"),
        body,
    }
}

#[test]
fn canonical_supports_are_unlimited_and_permutation_invariant() -> Result {
    let (_dir, mut store) = library()?;
    for index in 0..7 {
        store.insert_fact(&fact(
            &format!("left{index}"),
            "a",
            "p",
            &format!("m{index}"),
            FactKind::FactSlot,
            &["alice"],
        )?)?;
        store.insert_fact(&fact(
            &format!("right{index}"),
            &format!("m{index}"),
            "q",
            "z",
            FactKind::FactSlot,
            &["alice"],
        )?)?;
    }
    let rules = [rule(
        "join",
        "joined",
        vec![atom("p", "x", "y"), atom("q", "y", "z")],
    )];
    derive_with_rules(&mut store, &rules)?;
    let graph = store.derivation_view()?;
    let conclusion = graph
        .facts
        .values()
        .find(|f| f.predicate == "joined")
        .ok_or("no conclusion")?;
    assert_eq!(
        graph
            .justifications
            .values()
            .filter(|p| p.conclusion == conclusion.id && p.active)
            .count(),
        7
    );
    let before: BTreeSet<_> = graph.justifications.keys().cloned().collect();
    let mut permuted = rules[0].clone();
    permuted.body.reverse();
    derive_with_rules(&mut store, &[permuted])?;
    assert_eq!(
        before,
        store
            .derivation_view()?
            .justifications
            .keys()
            .cloned()
            .collect()
    );
    verify_hard_rules_match_full_recompute(&store)?;
    Ok(())
}

#[test]
fn overlapping_windows_are_required_and_effective_validity_is_not_materialized() -> Result {
    let (_dir, mut store) = library()?;
    let left = fact("p", "a", "p", "b", FactKind::FactSlot, &["alice"])?;
    let mut right = fact("q", "b", "q", "c", FactKind::FactSlot, &["alice"])?;
    if let FactAccess::Base { stamp, .. } = &mut right.access {
        stamp.valid.valid_from = EpochMs(100);
        stamp.valid.valid_until = EpochMs(200);
    }
    store.insert_fact(&left)?;
    store.insert_fact(&right)?;
    derive_with_rules(
        &mut store,
        &[rule(
            "join",
            "joined",
            vec![atom("p", "x", "y"), atom("q", "y", "z")],
        )],
    )?;
    assert!(store.derivation_view()?.justifications.is_empty());
    Ok(())
}

#[test]
fn alias_guard_is_viewer_scoped_and_flags_fixed_slot_conflicts() -> Result {
    let (_dir, mut store) = library()?;
    store.define_slot(&SlotContract {
        predicate: "identity".into(),
        namespace: "local".into(),
        cardinality: Cardinality::Single,
        mutability: Mutability::Fixed,
        value_kind: "string".into(),
        description: String::new(),
        instruction: String::new(),
    })?;
    for f in [
        fact(
            "ab",
            "a",
            "kp.core.alias",
            "b",
            FactKind::Alias,
            &["alice", "carol"],
        )?,
        fact(
            "bc",
            "b",
            "kp.core.alias",
            "c",
            FactKind::Alias,
            &["alice", "carol"],
        )?,
        fact(
            "ida",
            "a",
            "identity",
            "one",
            FactKind::FactSlot,
            &["alice"],
        )?,
        fact(
            "idc",
            "c",
            "identity",
            "two",
            FactKind::FactSlot,
            &["alice"],
        )?,
    ] {
        store.insert_fact(&f)?;
    }
    derive(&mut store)?;
    let alice = store.visible_facts(&"alice".parse()?, EpochMs(5))?;
    assert!(alice
        .iter()
        .any(|f| f.predicate == "kp.core.alias_conflict"));
    assert!(!alice
        .iter()
        .any(|f| f.kind == FactKind::Alias && f.subject == "a" && f.object == json!("c")));
    let carol = store.visible_facts(&"carol".parse()?, EpochMs(5))?;
    assert!(carol
        .iter()
        .any(|f| f.kind == FactKind::Alias && f.subject == "a" && f.object == json!("c")));
    assert!(!carol.iter().any(|f| f.kind == FactKind::Conflict));
    verify_hard_rules_match_full_recompute(&store)?;
    Ok(())
}

#[test]
fn grounded_cycle_why_terminates_and_withdrawal_removes_ungrounded_cycle() -> Result {
    let (_dir, mut store) = library()?;
    let p = fact("p", "a", "p", "z", FactKind::FactSlot, &["alice"])?;
    store.insert_fact(&p)?;
    derive_with_rules(
        &mut store,
        &[
            rule("seed", "x", vec![atom("p", "x", "z")]),
            rule("forward", "y", vec![atom("x", "x", "z")]),
            rule("back", "x", vec![atom("y", "x", "z")]),
        ],
    )?;
    let graph = store.derivation_view()?;
    let x = graph
        .facts
        .values()
        .find(|f| f.predicate == "x")
        .ok_or("missing x")?;
    let why = kp_derive::why::why(&store, &x.id, &"alice".parse()?, EpochMs(1))?
        .ok_or("missing proof")?;
    assert!(serde_json::to_string(&why)?.contains("\"cycle\":true"));
    store.forget("p", &"alice".parse()?, EpochMs(2))?;
    assert!(store
        .visible_facts(&"alice".parse()?, EpochMs(2))?
        .is_empty());
    assert!(store
        .derivation_view()?
        .facts
        .values()
        .filter(|f| matches!(f.access, FactAccess::Derived { .. }))
        .all(|f| f.status == FactStatus::Invalidated));
    verify_hard_rules_match_full_recompute(&store)?;
    Ok(())
}

struct AdversarialSynthesizer;
impl ScopedSynthesizer for AdversarialSynthesizer {
    fn synthesize<'a>(
        &'a self,
        input: &'a SynthesisInput,
    ) -> Pin<Box<dyn Future<Output = kp_store::Result<Vec<SynthesisClaim>>> + Send + 'a>> {
        Box::pin(async move {
            assert_eq!(
                input
                    .facts()
                    .iter()
                    .map(|f| f.id.as_str())
                    .collect::<Vec<_>>(),
                ["ab", "abc"]
            );
            Ok(vec![SynthesisClaim {
                text: "Shared [premise:ab] [premise:abc] [premise:bob_secret] [premise:invented]"
                    .into(),
                unverified_gaps: vec!["uncertain".into()],
            }])
        })
    }
}

#[tokio::test]
async fn synthesis_airgap_audit_quarantine_and_supersession() -> Result {
    let (_dir, mut store) = library()?;
    for f in [
        fact("ab", "a", "p", "b", FactKind::FactSlot, &["alice", "bob"])?,
        fact(
            "abc",
            "a",
            "p",
            "c",
            FactKind::FactSlot,
            &["alice", "bob", "carol"],
        )?,
        fact(
            "bob_secret",
            "secret",
            "p",
            "z",
            FactKind::FactSlot,
            &["bob"],
        )?,
    ] {
        store.insert_fact(&f)?;
    }
    let audience = BTreeSet::from(["alice".parse()?, "bob".parse()?]);
    let ids = synthesize_scoped(
        &mut store,
        &"alice".parse()?,
        &audience,
        "summary",
        EpochMs(1),
        &AdversarialSynthesizer,
    )
    .await?;
    let id = ids.first().ok_or("no synthesis")?;
    let graph = store.derivation_view()?;
    let synthesis = graph.facts.get(id).ok_or("missing synthesis")?;
    assert!(!synthesis.object.to_string().contains("invented"));
    assert!(!synthesis.object.to_string().contains("bob_secret"));
    assert!(matches!(synthesis.access, FactAccess::Derived { .. }));
    store.forget("ab", &"bob".parse()?, EpochMs(2))?;
    assert_eq!(
        store.derivation_view()?.facts[id].status,
        FactStatus::Quarantined
    );
    let ids = synthesize_scoped(
        &mut store,
        &"alice".parse()?,
        &audience,
        "second summary",
        EpochMs(3),
        &StructuredSynthesizer,
    )
    .await?;
    let replacement = fact(
        "v2",
        "a",
        "p",
        "new",
        FactKind::FactSlot,
        &["alice", "bob", "carol"],
    )?;
    store.supersede(&"abc".parse()?, &replacement)?;
    assert_eq!(
        store.derivation_view()?.facts[ids.first().ok_or("missing second")?].status,
        FactStatus::StalePremise
    );
    store.forget("abc", &"alice".parse()?, EpochMs(4))?;
    assert_eq!(
        store.derivation_view()?.facts[ids.first().ok_or("missing second")?].status,
        FactStatus::Quarantined
    );
    Ok(())
}

#[test]
fn oracle_survives_collision_supersession_forget_and_purge() -> Result {
    let (_dir, mut store) = library()?;
    let mut event = fact(
        "event",
        "outing",
        "schedule",
        "rain",
        FactKind::Event,
        &["alice", "bob"],
    )?;
    event.tags.insert("rain".into());
    let mut constraint = fact(
        "constraint",
        "bob",
        "constraint",
        "take train",
        FactKind::Constraint,
        &["alice", "bob"],
    )?;
    constraint.tags.insert("rain".into());
    store.insert_fact(&event)?;
    store.insert_fact(&constraint)?;
    store.set_anchor_status(&event.id, "sig_changed", EpochMs(0))?;
    derive(&mut store)?;
    verify_hard_rules_match_full_recompute(&store)?;
    assert_eq!(
        store
            .visible_facts(&"alice".parse()?, EpochMs(1))?
            .iter()
            .filter(|f| f.kind == FactKind::CollisionWarning)
            .count(),
        2
    );
    let mut replacement = constraint.clone();
    replacement.id = "v2".parse()?;
    replacement.logical_key = "v2".parse()?;
    store.supersede(&constraint.id, &replacement)?;
    derive(&mut store)?;
    verify_hard_rules_match_full_recompute(&store)?;
    store.forget("v2", &"alice".parse()?, EpochMs(2))?;
    verify_hard_rules_match_full_recompute(&store)?;
    store.purge(&event.origin_library, &event.logical_key, EpochMs(3), &[])?;
    verify_hard_rules_match_full_recompute(&store)?;
    Ok(())
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config { rng_seed: proptest::test_runner::RngSeed::Fixed(0x4d32), ..proptest::test_runner::Config::default() })]
    #[test]
    fn canonical_proof_identity_ignores_premise_order(reverse in proptest::bool::ANY) {
        let result = (|| -> Result {
            let a = fact("a","a","p","b",FactKind::FactSlot,&["alice"])?;
            let b = fact("b","b","q","c",FactKind::FactSlot,&["alice"])?;
            let conclusion = derived_fact(&"q".parse()?, &"alice".parse()?, FactKind::DerivedFact, "a", "joined", json!("c"), EpochMs(0))?;
            let expected = justification(&conclusion,DerivationMode::HardRule,"join",&[&a,&b])?;
            let premises = if reverse { [&b,&a] } else { [&a,&b] };
            assert_eq!(expected,justification(&conclusion,DerivationMode::HardRule,"join",&premises)?);
            Ok(())
        })();
        proptest::prop_assert!(result.is_ok(), "{result:?}");
    }
}

#[test]
fn expired_and_purged_premises_leave_no_active_hard_proofs() -> Result {
    let (_dir, mut store) = library()?;
    let p = fact("p", "a", "p", "z", FactKind::FactSlot, &["alice"])?;
    store.insert_fact(&p)?;
    derive_with_rules(
        &mut store,
        &[rule("copy", "copy", vec![atom("p", "x", "z")])],
    )?;
    store.expire(EpochMs(100))?;
    verify_hard_rules_match_full_recompute(&store)?;
    assert!(store
        .visible_facts(&"alice".parse()?, EpochMs(100))?
        .is_empty());
    Ok(())
}

#[test]
fn purged_derived_conclusions_cannot_be_recreated_by_rules() -> Result {
    let (_dir, mut store) = library()?;
    let p = fact("p", "a", "p", "z", FactKind::FactSlot, &["alice"])?;
    store.insert_fact(&p)?;
    derive_with_rules(
        &mut store,
        &[rule("copy", "copy", vec![atom("p", "x", "z")])],
    )?;
    let graph = store.derivation_view()?;
    let conclusion = graph
        .facts
        .values()
        .find(|f| f.predicate == "copy")
        .ok_or("missing derived")?;
    store.purge(
        &conclusion.origin_library,
        &conclusion.logical_key,
        EpochMs(2),
        &[],
    )?;
    derive(&mut store)?;
    verify_hard_rules_match_full_recompute(&store)?;
    assert_eq!(store.visible_facts(&"alice".parse()?, EpochMs(2))?.len(), 1);
    Ok(())
}

#[test]
fn repeated_derivation_preserves_cross_origin_proof_identity_after_reopen() -> Result {
    let (dir, mut store) = library()?;
    let mut event = fact("z", "a", "schedule", "rain", FactKind::Event, &["alice"])?;
    event.origin_library = "a-origin".parse()?;
    event.tags.insert("rain".into());
    let mut constraint = fact(
        "a",
        "b",
        "constraint",
        "train",
        FactKind::Constraint,
        &["alice"],
    )?;
    constraint.origin_library = "z-origin".parse()?;
    constraint.tags.insert("rain".into());
    store.insert_fact(&event)?;
    store.insert_fact(&constraint)?;
    derive(&mut store)?;
    drop(store);
    let mut store = Store::open(dir.path())?;
    assert_eq!(derive(&mut store)?.derived, 0);
    verify_hard_rules_match_full_recompute(&store)?;
    Ok(())
}

#[test]
fn transient_origin_miss_does_not_destroy_recoverable_proofs_during_unrelated_withdrawal() -> Result
{
    let (_dir, mut store) = library()?;
    store.insert_fact(&fact("p", "a", "p", "z", FactKind::FactSlot, &["alice"])?)?;
    store.insert_fact(&fact(
        "unrelated",
        "b",
        "other",
        "z",
        FactKind::FactSlot,
        &["alice"],
    )?)?;
    derive_with_rules(
        &mut store,
        &[rule("copy", "copy", vec![atom("p", "x", "z")])],
    )?;
    store.reconcile_fact(&"p".parse()?, None, EpochMs(1))?;
    derive(&mut store)?;
    store.forget("unrelated", &"alice".parse()?, EpochMs(2))?;
    assert!(store
        .visible_facts(&"alice".parse()?, EpochMs(2))?
        .is_empty());
    store.reconcile_fact(&"p".parse()?, Some(true), EpochMs(3))?;
    assert_eq!(store.visible_facts(&"alice".parse()?, EpochMs(3))?.len(), 2);
    Ok(())
}

#[path = "derivation/m1_nl_governance.rs"]
mod m1_nl_governance;
#[path = "derivation/m1_semiring_gates.rs"]
mod m1_semiring_gates;
#[path = "derivation/regressions.rs"]
mod regressions;
