use crate::AudienceSpec;
use proptest::prelude::*;
use proptest::test_runner::RngSeed;

use super::*;
use crate::test_support::{derived, fact, graph, justification};
use crate::Result;

#[test]
fn conjunction_intersects_audiences() -> Result<()> {
    let p = fact("p", &["A", "B", "C"], 0, 10)?;
    let q = fact("q", &["A", "B"], 0, 10)?;
    let k = derived("k")?;
    let support = justification("j", &k, &[&p, &q])?;
    let graph = graph(vec![p, q, k], vec![support]);
    assert!(is_visible_and_valid(
        &"k".parse()?,
        &"A".parse()?,
        EpochMs(5),
        &graph
    ));
    assert!(is_visible_and_valid(
        &"k".parse()?,
        &"B".parse()?,
        EpochMs(5),
        &graph
    ));
    assert!(!is_visible_and_valid(
        &"k".parse()?,
        &"C".parse()?,
        EpochMs(5),
        &graph
    ));
    assert!(visible_justifications(&"k".parse()?, &"C".parse()?, EpochMs(5), &graph).is_empty());
    Ok(())
}

#[test]
fn audience_and_time_are_joint_per_justification() -> Result<()> {
    let saturday = fact("saturday", &["A", "B"], 0, 7)?;
    let friday = fact("friday", &["A", "B", "C"], 0, 6)?;
    let k = derived("k")?;
    let d1 = justification("d1", &k, &[&saturday])?;
    let d2 = justification("d2", &k, &[&friday])?;
    let graph = graph(vec![saturday, friday, k], vec![d1, d2]);
    assert!(!is_visible_and_valid(
        &"k".parse()?,
        &"C".parse()?,
        EpochMs(6),
        &graph
    ));
    assert!(is_visible_and_valid(
        &"k".parse()?,
        &"A".parse()?,
        EpochMs(6),
        &graph
    ));
    assert_eq!(
        visible_justifications(&"k".parse()?, &"C".parse()?, EpochMs(5), &graph),
        vec!["d2".parse()?]
    );
    Ok(())
}

#[test]
fn cycles_require_grounded_support() -> Result<()> {
    let x = derived("x")?;
    let y = derived("y")?;
    let xy = justification("xy", &x, &[&y])?;
    let yx = justification("yx", &y, &[&x])?;
    let empty = justification("empty", &x, &[])?;
    let mut view = graph(vec![x.clone(), y], vec![xy, yx, empty]);
    assert!(view.evaluate(&"A".parse()?, EpochMs(5)).facts.is_empty());
    let p = fact("p", &["A"], 0, 10)?;
    let grounded = justification("grounded", &x, &[&p])?;
    view.facts.insert(p.id.clone(), p);
    view.justifications.insert(grounded.id.clone(), grounded);
    assert_eq!(view.evaluate(&"A".parse()?, EpochMs(5)).facts.len(), 3);
    assert!(
        !visible_justifications(&x.id, &"A".parse()?, EpochMs(5), &view)
            .contains(&"empty".parse()?)
    );
    Ok(())
}

#[test]
fn withdrawal_expiry_and_hash_mismatch_hide_descendants() -> Result<()> {
    let p = fact("p", &["A"], 0, 10)?;
    let k = derived("k")?;
    let tail = derived("tail")?;
    let first = justification("j1", &k, &[&p])?;
    let second = justification("j2", &tail, &[&k])?;
    let view = graph(vec![p.clone(), k, tail], vec![first, second]);
    assert_eq!(view.evaluate(&"A".parse()?, EpochMs(5)).facts.len(), 3);
    for status in [
        FactStatus::Withdrawn,
        FactStatus::Invalidated,
        FactStatus::Superseded,
        FactStatus::StalePremise,
        FactStatus::Quarantined,
        FactStatus::UnreachableHidden,
    ] {
        let mut changed = view.clone();
        let mut p = p.clone();
        p.status = status;
        changed.facts.insert(p.id.clone(), p);
        assert!(changed.evaluate(&"A".parse()?, EpochMs(5)).facts.is_empty());
    }
    let mut changed = view.clone();
    let mut edited = p.clone();
    edited.object = serde_json::json!("changed");
    changed.facts.insert(edited.id.clone(), edited);
    assert_eq!(changed.evaluate(&"A".parse()?, EpochMs(5)).facts.len(), 1);
    assert!(view.evaluate(&"A".parse()?, EpochMs(10)).facts.is_empty());
    let mut expired = p;
    if let FactAccess::Base { stamp, .. } = &mut expired.access {
        stamp.expired_at = Some(EpochMs(99));
    }
    changed.facts.insert(expired.id.clone(), expired);
    assert!(changed.evaluate(&"A".parse()?, EpochMs(5)).facts.is_empty());
    Ok(())
}

#[test]
fn all_members_does_not_include_outsiders() -> Result<()> {
    let mut p = fact("p", &[], 0, 10)?;
    if let FactAccess::Base { audience, .. } = &mut p.access {
        *audience = AudienceSpec::AllMembers;
    }
    let mut view = graph(vec![p], vec![]);
    view.members.insert("A".parse()?);
    assert_eq!(view.evaluate(&"A".parse()?, EpochMs(5)).facts.len(), 1);
    assert!(view.evaluate(&"C".parse()?, EpochMs(5)).facts.is_empty());
    Ok(())
}

#[test]
fn inactive_missing_and_expired_supports_fail_closed() -> Result<()> {
    let p = fact("p", &["A"], 0, 10)?;
    let k = derived("k")?;
    let mut support = justification("support", &k, &[&p])?;
    support.active = false;
    let view = graph(vec![p.clone(), k.clone()], vec![support.clone()]);
    assert!(!is_visible_and_valid(
        &k.id,
        &"A".parse()?,
        EpochMs(5),
        &view
    ));
    support.active = true;
    let view = graph(vec![k.clone()], vec![support.clone()]);
    assert!(!is_visible_and_valid(
        &k.id,
        &"A".parse()?,
        EpochMs(5),
        &view
    ));
    let mut expired = k;
    expired.access = FactAccess::Derived {
        recorded_at: EpochMs(0),
        expired_at: Some(EpochMs(6)),
    };
    let view = graph(vec![p, expired.clone()], vec![support]);
    assert!(!is_visible_and_valid(
        &expired.id,
        &"A".parse()?,
        EpochMs(5),
        &view
    ));
    Ok(())
}

#[test]
fn self_only_derived_conclusion_never_surfaces_for_other_viewers() -> Result<()> {
    let event = fact("rain", &["alice", "bob"], 0, 10)?;
    let rule = fact("rule", &["alice", "bob"], 0, 10)?;
    let mut self_warning = derived("warn_bob")?;
    self_warning.subject = "bob".into();
    self_warning.governed_scope = Some(crate::GovernedScope::SelfOnly {
        actor: "bob".parse()?,
    });
    let proof = justification("j_bob", &self_warning, &[&event, &rule])?;
    let view = graph(vec![event, rule, self_warning.clone()], vec![proof]);
    assert!(is_visible_and_valid(
        &self_warning.id,
        &"bob".parse()?,
        EpochMs(5),
        &view
    ));
    assert!(!is_visible_and_valid(
        &self_warning.id,
        &"alice".parse()?,
        EpochMs(5),
        &view
    ));
    Ok(())
}

// Independent proof search oracle: a finite grounded proof has no repeated node on a path.
fn has_proof(node: u8, bases: u8, rules: &[(u8, Vec<u8>, bool)], path: u8) -> bool {
    if node < 3 {
        return bases & (1 << node) != 0;
    }
    if path & (1 << node) != 0 {
        return false;
    }
    rules.iter().any(|(head, premises, active)| {
        *active
            && head % 3 + 3 == node
            && premises
                .iter()
                .all(|premise| has_proof(premise % 6, bases, rules, path | (1 << node)))
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, rng_seed: RngSeed::Fixed(103), ..ProptestConfig::default() })]
    #[test]
    fn adding_support_is_monotone(count in 0usize..32, at in -10i64..20) {
        let k = derived("k")?;
        let mut view = graph(vec![k.clone()], vec![]);
        let viewer = "C".parse()?;
        let mut before = view.evaluate(&viewer, EpochMs(at));
        for index in 0..count {
            let p = fact(&format!("p{index}"), &["C"], -5, 15)?;
            let support = justification(&format!("j{index}"), &k, &[&p])?;
            view.facts.insert(p.id.clone(), p);
            view.justifications.insert(support.id.clone(), support);
            let after = view.evaluate(&viewer, EpochMs(at));
            prop_assert!(before.facts.is_subset(&after.facts));
            before = after;
        }
        if (-5..15).contains(&at) { prop_assert_eq!(visible_justifications(&k.id, &viewer, EpochMs(at), &view).len(), count); }
    }

    #[test]
    fn hidden_mutations_are_byte_identical(changes in proptest::collection::vec((any::<bool>(), any::<bool>(), -20i64..20, ".{0,32}"), 0..32), at in -10i64..10) {
        let public = fact("public", &["C"], -20, 20)?;
        let k = derived("k")?;
        let visible_support = justification("public_support", &k, &[&public])?;
        let mut view = graph(vec![public,k.clone()], vec![visible_support]);
        let viewer = "C".parse()?;
        let before = serde_json::to_vec(&view.evaluate(&viewer, EpochMs(at)))?;
        for (withdrawn, expired, until, object) in changes {
            let mut hidden = fact("hidden", &["A","B"], -30, until)?;
            hidden.object = serde_json::json!(object);
            if withdrawn { hidden.status = FactStatus::Withdrawn; }
            if let FactAccess::Base { stamp, .. } = &mut hidden.access { stamp.expired_at = expired.then_some(EpochMs(0)); }
            let support = justification("hidden_support", &k, &[&hidden])?;
            view.facts.insert(hidden.id.clone(), hidden);
            view.justifications.insert(support.id.clone(), support);
            prop_assert_eq!(serde_json::to_vec(&view.evaluate(&viewer, EpochMs(at)))?, before.clone());
        }
    }

    #[test]
    fn fixpoint_matches_independent_proof_search(
        bases in any::<u8>(),
        rules in proptest::collection::vec((any::<u8>(), proptest::collection::vec(any::<u8>(), 1..4), any::<bool>()), 0..20)
    ) {
        let mut nodes = Vec::new();
        for index in 0..6 {
            let id = format!("n{index}");
            nodes.push(if index < 3 {
                fact(&id, if bases & (1 << index) != 0 { &["C"] } else { &["A"] }, 0, 10)?
            } else { derived(&id)? });
        }
        let mut supports = Vec::new();
        for (index, (head, premises, active)) in rules.iter().enumerate() {
            let premises: Vec<_> = premises.iter().map(|p| &nodes[usize::from(p % 6)]).collect();
            let mut support = justification(&format!("j{index}"), &nodes[usize::from(head % 3 + 3)], &premises)?;
            support.active = *active;
            supports.push(support);
        }
        let view = graph(nodes, supports);
        let visible = view.evaluate(&"C".parse()?, EpochMs(5));
        for index in 0..6 {
            prop_assert_eq!(visible.facts.contains(&format!("n{index}").parse()?), has_proof(index, bases, &rules, 0));
        }
    }
}
