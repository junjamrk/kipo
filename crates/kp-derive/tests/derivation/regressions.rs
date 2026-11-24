use super::*;

#[test]
fn legacy_content_key_tombstones_block_origin_scoped_derivation_after_reopen() -> Result {
    let (dir, mut store) = library()?;
    store.insert_fact(&fact("p", "a", "p", "z", FactKind::FactSlot, &["alice"])?)?;
    let hash = compute_fact_content_hash("a", "copy", &json!("z"), FactKind::DerivedFact);
    let legacy_key = format!("derived-{hash}").parse()?;
    store.purge(&"q".parse()?, &legacy_key, EpochMs(2), &[])?;
    drop(store);
    let mut store = Store::open(dir.path())?;
    derive_with_rules(
        &mut store,
        &[rule("copy", "copy", vec![atom("p", "x", "z")])],
    )?;
    verify_hard_rules_match_full_recompute(&store)?;
    assert!(store.is_tombstoned(&"q".parse()?, &legacy_key)?);
    assert_eq!(store.derivation_view()?.facts.len(), 1);
    assert!(!store
        .derivation_view()?
        .facts
        .values()
        .any(|f| f.predicate == "copy"));
    Ok(())
}

struct PartialCitations;
impl ScopedSynthesizer for PartialCitations {
    fn synthesize<'a>(
        &'a self,
        input: &'a SynthesisInput,
    ) -> Pin<Box<dyn Future<Output = kp_store::Result<Vec<SynthesisClaim>>> + Send + 'a>> {
        Box::pin(async move {
            assert_eq!(input.facts().len(), 2);
            Ok(vec![SynthesisClaim {
                text: "Both inputs influenced this text [premise:cited]".into(),
                unverified_gaps: Vec::new(),
            }])
        })
    }
}

#[tokio::test]
async fn every_model_input_is_a_premise_even_when_not_cited() -> Result {
    let (_dir, mut store) = library()?;
    for id in ["cited", "uncited"] {
        store.insert_fact(&fact(
            id,
            id,
            "p",
            "text",
            FactKind::Observation,
            &["alice"],
        )?)?;
    }
    let ids = synthesize_scoped(
        &mut store,
        &"alice".parse()?,
        &BTreeSet::from(["alice".parse()?]),
        "summary",
        EpochMs(1),
        &PartialCitations,
    )
    .await?;
    let id = ids.first().ok_or("synthesis")?;
    let graph = store.derivation_view()?;
    let proof = graph
        .justifications
        .values()
        .find(|p| &p.conclusion == id)
        .ok_or("proof")?;
    assert_eq!(
        proof
            .premises
            .iter()
            .map(|p| p.fact_id.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["cited", "uncited"])
    );
    store.forget("uncited", &"alice".parse()?, EpochMs(2))?;
    assert_eq!(
        store.derivation_view()?.facts[id].status,
        FactStatus::Quarantined
    );
    store.purge(&"q".parse()?, &"uncited".parse()?, EpochMs(3), &[])?;
    assert!(!store.derivation_view()?.facts.contains_key(id));
    Ok(())
}

#[tokio::test]
async fn synthesis_requires_the_explicit_author_in_its_audience() -> Result {
    let (_dir, mut store) = library()?;
    store.insert_fact(&fact(
        "shared",
        "s",
        "p",
        "v",
        FactKind::Observation,
        &["alice", "bob"],
    )?)?;
    let audience = BTreeSet::from(["alice".parse()?, "bob".parse()?]);
    assert!(synthesize_scoped(
        &mut store,
        &"carol".parse()?,
        &audience,
        "summary",
        EpochMs(1),
        &StructuredSynthesizer
    )
    .await
    .is_err());
    let ids = synthesize_scoped(
        &mut store,
        &"bob".parse()?,
        &audience,
        "summary",
        EpochMs(1),
        &StructuredSynthesizer,
    )
    .await?;
    assert_eq!(
        store.derivation_view()?.facts[ids.first().ok_or("synthesis")?]
            .author
            .as_str(),
        "bob"
    );
    Ok(())
}

#[test]
fn event_collision_normalizes_subject_tags_and_object_triggers_against_entities() -> Result {
    let variants = [
        ("  HaNgZhOu_RaIn  ", BTreeSet::new(), json!("offsite")),
        (
            "event",
            BTreeSet::from(["  HaNgZhOu_RaIn  ".into()]),
            json!("offsite"),
        ),
        (
            "event",
            BTreeSet::new(),
            json!({"triggers":"  HaNgZhOu_RaIn  "}),
        ),
        (
            "event",
            BTreeSet::new(),
            json!({"conditions":["  HaNgZhOu_RaIn  "]}),
        ),
    ];
    for (subject, tags, object) in variants {
        let (_dir, mut store) = library()?;
        let mut event = fact(
            "event",
            subject,
            "schedule",
            "offsite",
            FactKind::Event,
            &["alice"],
        )?;
        event.tags = tags;
        event.object = object;
        let constraint = fact(
            "constraint",
            "commute",
            "constraint",
            "train",
            FactKind::Constraint,
            &["alice"],
        )?;
        store.remember(&[
            kp_store::FactInput {
                fact: event,
                entities: BTreeSet::new(),
            },
            kp_store::FactInput {
                fact: constraint,
                entities: BTreeSet::from(["hangzhou_rain".into()]),
            },
        ])?;
        derive(&mut store)?;
        assert_eq!(
            store
                .visible_facts(&"alice".parse()?, EpochMs(1))?
                .iter()
                .filter(|f| f.kind == FactKind::CollisionWarning)
                .count(),
            1
        );
    }
    Ok(())
}
