use super::*;

#[test]
fn identical_conclusions_from_different_libraries_coexist_without_identity_conflicts() -> Result<()>
{
    let make = |library: &str, author: &str, at| {
        derived_fact(
            &library.parse()?,
            &author.parse()?,
            FactKind::CollisionWarning,
            "Hangzhou trip",
            "kp.core.collision",
            serde_json::json!({"event":"rain", "constraint":"no driving"}),
            EpochMs(at),
        )
    };
    let community = make("community", "admin", 10)?;
    let personal = make("personal", "alice", 20)?;
    let repeated = make("personal", "bob", 30)?;
    assert_ne!(community.id, personal.id);
    assert_eq!(community.logical_key, personal.logical_key);
    assert_eq!(
        personal.logical_key.as_str(),
        format!("derived-{}", personal.strong_ref().content_sha256)
    );
    assert_eq!(
        community.strong_ref().content_sha256,
        personal.strong_ref().content_sha256
    );
    assert_eq!(personal.id, repeated.id);
    assert_eq!(personal.strong_ref(), repeated.strong_ref());
    let facts = [
        (community.id.clone(), community),
        (personal.id.clone(), personal),
    ]
    .into_iter()
    .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(facts.len(), 2);
    Ok(())
}

#[test]
fn proof_identity_distinguishes_conclusion_ids_with_the_same_strong_reference() -> Result<()> {
    let premise = crate::test_support::fact("premise", &["A"], 0, 100)?;
    let current = derived_fact(
        &"personal".parse()?,
        &"A".parse()?,
        FactKind::DerivedFact,
        "weather",
        "kp.core.weather",
        serde_json::json!("rain"),
        EpochMs(10),
    )?;
    let mut legacy = current.clone();
    legacy.id = legacy.logical_key.as_str().parse()?;
    assert_eq!(legacy.strong_ref(), current.strong_ref());
    let before = justification(&legacy, DerivationMode::HardRule, "R1", &[&premise])?;
    let after = justification(&current, DerivationMode::HardRule, "R1", &[&premise])?;
    assert_ne!(before.id, after.id);
    assert_ne!(before.conclusion, after.conclusion);
    Ok(())
}
