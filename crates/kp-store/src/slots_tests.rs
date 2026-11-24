use kp_model::{ContentHash, FactKind};
use kp_model::{LibraryMode, ValidityWindow};

use super::*;
use crate::test_support::{derived, fact, justification, library, source, AT};

fn input(fact: Fact) -> FactInput {
    FactInput {
        fact,
        entities: BTreeSet::from(["杭州".into()]),
    }
}

#[test]
fn time_varying_supersession_closes_history_and_invalidates_proofs() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let old = fact("old", &["owner"])?;
    store.remember(&[input(old.clone())])?;
    let conclusion = derived("trip")?;
    store.insert_fact(&conclusion)?;
    store.insert_justification(&justification("proof", &conclusion, &[&old])?)?;
    let mut new = fact("new", &["owner"])?;
    new.object = serde_json::json!("sunny");
    if let FactAccess::Base { stamp, .. } = &mut new.access {
        stamp.valid.valid_from = EpochMs(100);
    }
    let result = store.remember(&[input(new)])?;
    assert_eq!(result.superseded, vec![old.id.clone()]);
    let graph = store.derivation_view()?;
    assert_eq!(graph.facts[&old.id].status, FactStatus::Superseded);
    assert!(
        matches!(graph.facts[&old.id].access,FactAccess::Base { stamp,.. } if stamp.valid.valid_until == EpochMs(100))
    );
    assert_eq!(graph.facts[&conclusion.id].status, FactStatus::Invalidated);
    assert!(!graph.justifications[&"proof".parse()?].active);
    let before = store.retrieval_snapshot(&"owner".parse()?, EpochMs(99))?;
    let after = store.retrieval_snapshot(&"owner".parse()?, EpochMs(100))?;
    assert!(before.documents().iter().any(|d| d.id == "fact-old"));
    assert!(!before.documents().iter().any(|d| d.id == "fact-new"));
    assert!(after.documents().iter().any(|d| d.id == "fact-new"));
    assert!(!after.documents().iter().any(|d| d.id == "fact-old"));
    Ok(())
}

#[test]
fn fixed_conflict_is_jointly_visible_and_multi_values_coexist() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Community)?;
    store.define_slot(&SlotContract {
        predicate: "birthplace".into(),
        namespace: "kp.community.test".into(),
        cardinality: Cardinality::Single,
        mutability: Mutability::Fixed,
        value_kind: "string".into(),
        description: "Birthplace".into(),
        instruction: "Keep attribution".into(),
    })?;
    let mut old = fact("old", &["owner", "carol"])?;
    old.predicate = "birthplace".into();
    let mut new = fact("new", &["owner"])?;
    new.predicate = "birthplace".into();
    new.object = serde_json::json!("other");
    store.remember(&[input(old.clone()), input(new.clone())])?;
    let owner = store.visible_facts(&"owner".parse()?, AT)?;
    assert_eq!(
        owner
            .iter()
            .filter(|f| f.kind == FactKind::Conflict)
            .count(),
        1
    );
    assert_eq!(store.visible_facts(&"carol".parse()?, AT)?.len(), 1);
    let graph = store.derivation_view()?;
    assert!(graph
        .justifications
        .values()
        .any(|j| j.rule_id == "R1_SLOT_CONFLICT"));
    assert!(graph.facts.values().all(|f| f.status == FactStatus::Active));
    for id in ["one", "two"] {
        let mut value = fact(id, &["owner"])?;
        value.predicate = "kp.core.preference".into();
        store.remember(&[input(value)])?;
    }
    assert_eq!(store.visible_facts(&"owner".parse()?, AT)?.len(), 5);
    Ok(())
}

#[test]
fn withdrawal_preserves_audit_and_alternative_grounded_proofs() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let a = fact("a", &["owner"])?;
    let b = fact("b", &["owner"])?;
    let c = derived("c")?;
    let d = derived("d")?;
    for f in [&a, &b, &c, &d] {
        store.insert_fact(f)?;
    }
    for j in [
        justification("ac", &c, &[&a])?,
        justification("bc", &c, &[&b])?,
        justification("cd", &d, &[&c])?,
        justification("dc", &c, &[&d])?,
    ] {
        store.insert_justification(&j)?;
    }
    store.forget("a", &"owner".parse()?, EpochMs(200))?;
    assert_eq!(store.visible_facts(&"owner".parse()?, AT)?.len(), 3);
    store.forget("b", &"owner".parse()?, EpochMs(300))?;
    assert!(store.visible_facts(&"owner".parse()?, AT)?.is_empty());
    let graph = store.derivation_view()?;
    assert_eq!(graph.facts.len(), 4);
    assert!(
        matches!(graph.facts[&a.id].access,FactAccess::Base { stamp,.. }
        if stamp.expired_at == Some(EpochMs(200)) && stamp.valid.contains(EpochMs(100)))
    );
    assert_eq!(graph.facts[&c.id].status, FactStatus::Invalidated);
    assert_eq!(graph.facts[&d.id].status, FactStatus::Invalidated);
    Ok(())
}

#[test]
fn forget_by_id_logs_the_facts_origin_and_logical_key_once() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let mut imported = fact("imported-version", &["owner"])?;
    imported.logical_key = "shared-key".parse()?;
    store.insert_fact(&imported)?;
    let actor: ActorId = "curator".parse()?;
    for _ in 0..2 {
        assert_eq!(
            store.forget(imported.id.as_str(), &actor, AT)?,
            vec![imported.id.clone()]
        );
    }
    let row = store.db.query_row(
        "SELECT target_id,origin_library_id,logical_key,actor_id,created_at FROM change_log WHERE op='forget'",
        [],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?, row.get::<_, i64>(4)?)),
    )?;
    assert_eq!(
        row,
        (
            imported.id.to_string(),
            imported.origin_library.to_string(),
            imported.logical_key.to_string(),
            actor.to_string(),
            AT.0
        )
    );
    let count: usize = store.db.query_row(
        "SELECT count(*) FROM change_log WHERE op='forget'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(count, 1);
    Ok(())
}

#[test]
fn forget_by_logical_key_logs_every_local_version_without_crossing_origins() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    for (id, origin) in [
        ("local-one", "local"),
        ("local-two", "local"),
        ("remote", "remote"),
    ] {
        let mut version = fact(id, &["owner"])?;
        version.origin_library = origin.parse()?;
        version.logical_key = "shared-key".parse()?;
        store.insert_fact(&version)?;
    }
    let withdrawn = store.forget("shared-key", &"owner".parse()?, AT)?;
    assert_eq!(withdrawn, vec!["local-one".parse()?, "local-two".parse()?]);
    let mut stmt = store.db.prepare("SELECT target_id,origin_library_id,logical_key FROM change_log WHERE op='forget' ORDER BY target_id")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(
        rows,
        vec![
            ("local-one".into(), "local".into(), "shared-key".into()),
            ("local-two".into(), "local".into(), "shared-key".into())
        ]
    );
    assert_eq!(
        store
            .visible_facts(&"owner".parse()?, AT)?
            .iter()
            .map(|fact| fact.id.as_str())
            .collect::<Vec<_>>(),
        vec!["remote"]
    );
    Ok(())
}

#[test]
fn invalid_batch_rolls_back_and_nonoverlapping_windows_coexist() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let mut first = fact("first", &["owner"])?;
    if let FactAccess::Base { stamp, .. } = &mut first.access {
        stamp.valid.valid_until = EpochMs(100);
    }
    let mut second = fact("second", &["owner"])?;
    if let FactAccess::Base { stamp, .. } = &mut second.access {
        stamp.valid = ValidityWindow {
            valid_from: EpochMs(100),
            valid_until: EpochMs(200),
        };
    }
    store.remember(&[input(first), input(second)])?;
    assert!(store
        .derivation_view()?
        .facts
        .values()
        .all(|f| f.status == FactStatus::Active));
    let mut bad = fact("bad", &["owner"])?;
    bad.subject.clear();
    assert!(store
        .remember(&[input(fact("third", &["owner"])?), input(bad)])
        .is_err());
    assert_eq!(store.derivation_view()?.facts.len(), 2);
    Ok(())
}

#[test]
fn snapshot_checks_membership_hash_and_vectors_and_source_id_purge() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let mut input = source(&["owner"])?;
    input.readable_by = kp_model::AudienceSpec::AllMembers;
    let source = store.ingest_source(&"local".parse()?, &"raw".parse()?, &input)?;
    assert!(store
        .retrieval_snapshot(&"stranger".parse()?, AT)?
        .documents()
        .is_empty());
    assert_eq!(
        store
            .retrieval_snapshot(&"owner".parse()?, AT)?
            .documents()
            .len(),
        1
    );
    let vectors: usize = store
        .db
        .query_row("SELECT count(*) FROM chunk_vectors", [], |r| r.get(0))?;
    assert_eq!(vectors, 1);
    store.db.execute(
        "UPDATE chunks SET source_sha256=?1",
        [ContentHash::digest("changed").as_str()],
    )?;
    assert!(store
        .retrieval_snapshot(&"owner".parse()?, AT)?
        .documents()
        .is_empty());
    assert!(store
        .search_chunks("weather", &"owner".parse()?)?
        .is_empty());
    store.purge_target(&"local".parse()?, source.source_id.as_str(), AT, "test")?;
    store.purge_target(&"local".parse()?, source.source_id.as_str(), AT, "test")?;
    assert!(store.raw_source_bytes(&source.source_id)?.is_none());
    assert!(store.is_tombstoned(&"local".parse()?, &"raw".parse()?)?);
    assert_eq!(
        store
            .db
            .query_row("SELECT count(*) FROM chunk_vectors", [], |r| r
                .get::<_, usize>(0))?,
        0
    );
    Ok(())
}

#[test]
fn private_slot_replacement_cannot_withdraw_another_audiences_value() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let public = fact("visible", &["carol"])?;
    store.remember(&[input(public.clone())])?;
    let before = serde_json::to_vec(&store.visible_facts(&"carol".parse()?, AT)?)?;
    let mut private = fact("hidden", &["bob"])?;
    private.object = serde_json::json!("snow");
    store.remember(&[input(private)])?;
    assert_eq!(
        before,
        serde_json::to_vec(&store.visible_facts(&"carol".parse()?, AT)?)?
    );
    store.forget("hidden", &"bob".parse()?, AT)?;
    assert_eq!(
        before,
        serde_json::to_vec(&store.visible_facts(&"carol".parse()?, AT)?)?
    );
    Ok(())
}

#[test]
fn purging_extracted_fact_erases_its_containing_source_and_blocks_replay() -> Result<()> {
    let (dir, mut store) = library(LibraryMode::Personal)?;
    let saved = store.ingest_source(
        &"local".parse()?,
        &"document".parse()?,
        &source(&["owner"])?,
    )?;
    let mut extracted = fact("extracted", &["owner"])?;
    extracted.source_anchor = Some(kp_model::AnchorSpec {
        source_id: Some(saved.source_id.clone()),
        ..Default::default()
    });
    store.insert_fact(&extracted)?;
    store.purge(&extracted.origin_library, &extracted.logical_key, AT, &[])?;
    assert_eq!(store.status()?.sources, 0);
    assert!(store.is_tombstoned(&"local".parse()?, &"document".parse()?)?);
    assert!(store.raw_source_bytes(&saved.source_id)?.is_none());
    assert!(!dir.path().join("raw").exists());
    assert!(store.insert_fact(&extracted).is_err());
    Ok(())
}

#[test]
fn exact_fact_withdrawal_preserves_colliding_keys_and_invalidates_only_its_proofs() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let target = fact("target", &["owner"])?;
    let mut private = fact("private", &["bob"])?;
    private.author = "bob".parse()?;
    private.origin_library = store.library_id().clone();
    private.logical_key = target.id.as_str().parse()?;
    let conclusion = derived("conclusion")?;
    for fact in [&target, &private, &conclusion] {
        store.insert_fact(fact)?;
    }
    store.insert_justification(&justification("proof", &conclusion, &[&target])?)?;
    let actor = "owner".parse()?;
    for _ in 0..2 {
        assert_eq!(
            store.forget_fact_id(&target.id, &actor, AT)?,
            vec![target.id.clone()]
        );
    }
    let graph = store.derivation_view()?;
    assert_eq!(graph.facts[&target.id].status, FactStatus::Withdrawn);
    assert_eq!(graph.facts[&private.id].status, FactStatus::Active);
    assert_eq!(graph.facts[&conclusion.id].status, FactStatus::Invalidated);
    assert!(!graph.justifications[&"proof".parse()?].active);
    let count: usize = store.db.query_row(
        "SELECT count(*) FROM change_log WHERE op='forget'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(count, 1);
    Ok(())
}

#[test]
fn exact_fact_withdrawal_does_not_resolve_a_logical_key_when_the_id_is_missing() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let mut private = fact("private", &["owner"])?;
    private.origin_library = store.library_id().clone();
    private.logical_key = "missing-id".parse()?;
    store.insert_fact(&private)?;
    assert!(matches!(
        store.forget_fact_id(&"missing-id".parse()?, &"owner".parse()?, AT),
        Err(StoreError::NotFound)
    ));
    assert_eq!(
        store.derivation_view()?.facts[&private.id].status,
        FactStatus::Active
    );
    Ok(())
}

#[test]
fn bulk_multi_cardinality_remember_avoids_quadratic_rescan_and_fixed_uses_stored_hash() -> Result<()>
{
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    // 1,200 Multi-cardinality observations sharing the same (subject, predicate) ingest linearly.
    let mut batch = Vec::with_capacity(1_200);
    for idx in 0..1_200 {
        let mut obs = fact(&format!("obs_{idx}"), &["owner"])?;
        obs.predicate = "kp.core.observation".into();
        obs.subject = "shared_doc_section".into();
        obs.object = serde_json::json!(format!("line observation {idx}"));
        batch.push(FactInput {
            fact: obs,
            entities: BTreeSet::from(["杭州".into()]),
        });
    }
    let remembered = store.remember(&batch)?;
    assert_eq!(remembered.facts.len(), 1_200);
    assert!(remembered.superseded.is_empty());
    assert!(remembered.conflicts.is_empty());

    // Fixed single-cardinality slot with identical content_sha256 produces zero conflict;
    // differing content_sha256 produces the exact R1_SLOT_CONFLICT proof from stored columns.
    store.define_slot(&SlotContract {
        predicate: "origin_city".into(),
        namespace: "kp.community.test".into(),
        cardinality: Cardinality::Single,
        mutability: Mutability::Fixed,
        value_kind: "string".into(),
        description: "Origin city".into(),
        instruction: "Keep attribution".into(),
    })?;
    let mut c1 = fact("city_1", &["owner"])?;
    c1.predicate = "origin_city".into();
    c1.object = serde_json::json!("Hangzhou");
    let mut c1_same = fact("city_1_dup", &["owner"])?;
    c1_same.predicate = "origin_city".into();
    c1_same.object = serde_json::json!("Hangzhou");
    let mut c2_diff = fact("city_2_diff", &["owner"])?;
    c2_diff.predicate = "origin_city".into();
    c2_diff.object = serde_json::json!("Suzhou");

    let res = store.remember(&[input(c1.clone()), input(c1_same), input(c2_diff.clone())])?;
    assert_eq!(res.conflicts.len(), 2);
    let expected = kp_model::slot_conflict(&c1.origin_library, &c1.author, &c1, &c2_diff)?;
    assert!(res.conflicts.contains(&expected.id));
    Ok(())
}
