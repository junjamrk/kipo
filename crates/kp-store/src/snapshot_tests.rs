use kp_model::{KpError, LibraryMode};
use proptest::prelude::*;
use proptest::test_runner::RngSeed;

use super::*;
use crate::test_support::{fact, library, source, AT};
use crate::StoreError;

struct FixedVector(Vec<f32>);

impl Embedder for FixedVector {
    fn embed(&self, _text: &str) -> Result<Vec<f32>> {
        Ok(self.0.clone())
    }
}

fn assert_invalid_embeddings_rejected(store: &Store) -> Result<()> {
    let viewer = "owner".parse()?;
    for vector in [
        vec![],
        vec![f32::NAN],
        vec![f32::INFINITY],
        vec![f32::NEG_INFINITY],
        vec![2.0],
        vec![0.5],
    ] {
        assert!(matches!(
            store.retrieval_snapshot_with_embedder(&viewer, AT, &FixedVector(vector)),
            Err(StoreError::Model(KpError::InvalidInput {
                field: "embedding",
                ..
            }))
        ));
    }
    assert_eq!(store.retrieval_snapshot(&viewer, AT)?.documents().len(), 1);
    Ok(())
}

#[test]
fn custom_fact_embeddings_are_validated() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    store.insert_fact(&fact("visible", &["owner"])?)?;
    assert_invalid_embeddings_rejected(&store)
}

#[test]
fn custom_fallback_chunk_embeddings_are_validated() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    store.ingest_source(&"local".parse()?, &"raw".parse()?, &source(&["owner"])?)?;
    assert_eq!(store.db.execute("DELETE FROM chunk_vectors", [])?, 1);
    assert_invalid_embeddings_rejected(&store)
}

#[test]
fn custom_normalized_and_zero_sentinel_embeddings_remain_valid() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    store.insert_fact(&fact("visible", &["owner"])?)?;
    store.ingest_source(&"local".parse()?, &"raw".parse()?, &source(&["owner"])?)?;
    assert_eq!(store.db.execute("DELETE FROM chunk_vectors", [])?, 1);
    for vector in [vec![0.6, 0.8], vec![0.0, 0.0]] {
        let snapshot = store.retrieval_snapshot_with_embedder(
            &"owner".parse()?,
            AT,
            &FixedVector(vector.clone()),
        )?;
        assert_eq!(snapshot.documents().len(), 2);
        assert!(snapshot
            .documents()
            .iter()
            .all(|document| document.vector == vector));
    }
    Ok(())
}

#[test]
fn candidate_retrieval_snapshot_scales_via_reverse_dfs_and_enforces_semiring_and_source_scopes(
) -> Result<()> {
    use kp_model::{
        AudienceSpec, DerivationMode, FactAccess, FactKind, Justification, MemberRole, Premise,
        SourceScope, SourceScopeId,
    };

    let (_dir, mut store) = library(LibraryMode::Community)?;
    let alice: kp_model::ActorId = "alice".parse()?;
    let bob: kp_model::ActorId = "bob".parse()?;
    let carol: kp_model::ActorId = "carol".parse()?;
    store.add_member(alice.clone(), MemberRole::Member, AT)?;
    store.add_member(bob.clone(), MemberRole::Member, AT)?;
    store.add_member(carol.clone(), MemberRole::Member, AT)?;

    // Bulk-insert 2,000 unrelated background facts in one transaction to verify candidate-first bounding.
    {
        let tx = store
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for idx in 0..2_000 {
            let mut bg = fact(&format!("fact_bg_{idx}"), &["alice", "bob", "carol"])?;
            bg.subject = format!("module_{idx}");
            bg.object = serde_json::json!(format!("background metric {idx}"));
            let opaque = crate::tombstone_id(
                bg.origin_library.as_str().as_bytes(),
                &bg.origin_library,
                &bg.logical_key,
            )?;
            crate::facts::write_fact(&tx, &bg, &opaque)?;
        }
        tx.commit()?;
    }

    let scope_id: SourceScopeId = "ext_chat.gymclub".parse()?;
    store.upsert_source_scope(&SourceScope {
        scope_id: scope_id.clone(),
        provider: "ext_chat".into(),
        external_container_id: "oc_gymclub".into(),
        label: "训练协作讨论组".into(),
        current_audience: AudienceSpec::Explicit(
            [alice.clone(), bob.clone()].into_iter().collect(),
        ),
        synced_at: AT,
    })?;

    // 3-premise person-targeted collision (`event` + `constraint` + `membership`) visible only to `{alice, bob}`.
    let mut soreness_ev = fact(crate::new_fact_id()?.as_str(), &["alice", "bob", "carol"])?;
    soreness_ev.kind = FactKind::Event;
    soreness_ev.author = alice.clone();
    soreness_ev.subject = "体能训练".into();
    soreness_ev.object = serde_json::json!("周四膝盖酸痛");
    if let FactAccess::Base { stamp, .. } = &mut soreness_ev.access {
        stamp.valid.valid_from = EpochMs(AT.0 - 1_000);
        stamp.valid.valid_until = EpochMs(AT.0 + 86_400_000);
    }

    let mut no_squat = fact(crate::new_fact_id()?.as_str(), &["alice", "bob"])?;
    no_squat.kind = FactKind::Constraint;
    no_squat.author = bob.clone();
    no_squat.source_scope_id = Some(scope_id.clone());
    no_squat.subject = "小组课表".into();
    no_squat.object = serde_json::json!("膝盖酸痛不做负重深蹲");

    let club_cpt = store.ensure_community_concept(
        kp_model::ConceptKind::Group,
        "训练小组",
        "fitness",
        kp_model::ConceptPolarity::Neutral,
        &bob,
        AT,
    )?;
    let mut membership = fact(crate::new_fact_id()?.as_str(), &["alice", "bob"])?;
    membership.kind = FactKind::Membership;
    membership.author = bob.clone();
    membership.subject = "bob".into();
    membership.object = serde_json::to_value(kp_model::MembershipRelation {
        anchor: bob.clone(),
        member: alice.clone(),
        group_concept_id: club_cpt.id,
    })?;

    for f in [&soreness_ev, &no_squat, &membership] {
        store.insert_fact(f)?;
    }

    let warning = kp_model::derived_fact(
        store.library_id(),
        &bob,
        FactKind::CollisionWarning,
        "alice",
        "kp.collision.person_warning",
        serde_json::json!({"summary": "周四膝盖酸痛恢复期，建议 Alice 暂停负重深蹲训练"}),
        AT,
    )?;
    let proof = Justification {
        id: crate::new_justification_id()?,
        conclusion: warning.id.clone(),
        mode: DerivationMode::HardRule,
        rule_id: "R2_PERSON_COLLISION".into(),
        rule_version: "4".into(),
        premises: [&soreness_ev, &no_squat, &membership]
            .into_iter()
            .map(|f| Premise {
                fact_id: f.id.clone(),
                strong_ref: f.strong_ref(),
            })
            .collect(),
        active: true,
        created_at: AT,
        invalidated_at: None,
        invalidation_reason: None,
    };
    store.save_derivations(std::slice::from_ref(&warning), std::slice::from_ref(&proof))?;

    // Also add an ungrounded 2-cycle matching the query to prove reverse-DFS rejects cycles!
    let cyc_a = kp_model::derived_fact(
        store.library_id(),
        &alice,
        FactKind::DerivedFact,
        "alice",
        "kp.cycle.a",
        serde_json::json!("周四负重深蹲循环A"),
        AT,
    )?;
    let cyc_b = kp_model::derived_fact(
        store.library_id(),
        &alice,
        FactKind::DerivedFact,
        "alice",
        "kp.cycle.b",
        serde_json::json!("周四负重深蹲循环B"),
        AT,
    )?;
    let p_ab = kp_model::justification(&cyc_a, DerivationMode::HardRule, "CYC", &[&cyc_b])?;
    let p_ba = kp_model::justification(&cyc_b, DerivationMode::HardRule, "CYC", &[&cyc_a])?;
    store.save_derivations(&[cyc_a.clone(), cyc_b.clone()], &[p_ab, p_ba])?;

    let alice_snap =
        store.candidate_retrieval_snapshot(&alice, AT, "周四还能做负重深蹲训练吗", 12)?;
    assert!(alice_snap
        .documents()
        .iter()
        .any(|d| d.id == format!("fact-{}", warning.id)));
    assert!(!alice_snap
        .documents()
        .iter()
        .any(|d| d.id == format!("fact-{}", cyc_a.id) || d.id == format!("fact-{}", cyc_b.id)));

    // Carol is outside `{alice, bob}`: sees soreness_ev but ZERO leak of `no_squat` or `warning`!
    let carol_snap =
        store.candidate_retrieval_snapshot(&carol, AT, "周四还能做负重深蹲训练吗", 12)?;
    assert!(
        !carol_snap
            .documents()
            .iter()
            .any(|d| d.id == format!("fact-{}", warning.id)
                || d.id == format!("fact-{}", no_squat.id))
    );

    // After Thursday window expires, `warning` automatically disappears at read time!
    let after_window = EpochMs(AT.0 + 90_000_000);
    let expired_snap =
        store.candidate_retrieval_snapshot(&alice, after_window, "周四还能做负重深蹲训练吗", 12)?;
    assert!(!expired_snap
        .documents()
        .iter()
        .any(|d| d.id == format!("fact-{}", warning.id)));

    // When Alice leaves `ext_chat.gymclub` (`SourceScope`), both `no_squat` and `warning` vanish immediately!
    store.upsert_source_scope(&SourceScope {
        scope_id,
        provider: "ext_chat".into(),
        external_container_id: "oc_gymclub".into(),
        label: "训练协作讨论组".into(),
        current_audience: AudienceSpec::Explicit([bob].into_iter().collect()),
        synced_at: EpochMs(AT.0 + 500),
    })?;
    let narrowed_snap =
        store.candidate_retrieval_snapshot(&alice, AT, "周四还能做负重深蹲训练吗", 12)?;
    assert!(
        !narrowed_snap
            .documents()
            .iter()
            .any(|d| d.id == format!("fact-{}", warning.id)
                || d.id == format!("fact-{}", no_squat.id))
    );
    Ok(())
}

#[test]
fn candidate_retrieval_snapshot_preserves_byte_identical_viewer_noninterference() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Community)?;
    let alice: kp_model::ActorId = "alice".parse()?;
    let bob: kp_model::ActorId = "bob".parse()?;
    store.add_member(alice.clone(), kp_model::MemberRole::Member, AT)?;
    store.add_member(bob.clone(), kp_model::MemberRole::Member, AT)?;

    for idx in 0..6 {
        let mut pub_fact = fact(&format!("alice_vis_{idx}"), &["alice"])?;
        pub_fact.subject = format!("主从切换规则{idx}");
        pub_fact.object = serde_json::json!(format!("周四切换窗口暂停离线跑批任务 {idx}"));
        store.insert_fact(&pub_fact)?;
    }

    let baseline = serde_json::to_vec(
        &store
            .candidate_retrieval_snapshot(&alice, AT, "周四主从切换离线跑批", 4)?
            .documents(),
    )?;

    // Flood the store with 80 high-similarity hidden facts and chunks owned by `bob`.
    for idx in 0..80 {
        let mut hidden_fact = fact(&format!("bob_secret_{idx}"), &["bob"])?;
        hidden_fact.author = bob.clone();
        hidden_fact.subject = "周四主从切换离线跑批".into();
        hidden_fact.object =
            serde_json::json!(format!("BOB_PRIVATE_SECRET_{idx} 周四主从切换离线跑批"));
        store.remember(&[crate::FactInput {
            fact: hidden_fact,
            entities: BTreeSet::from(["周四主从切换".into()]),
        }])?;

        let mut hidden_src = source(&["bob"])?;
        hidden_src.content = format!("BOB_RAW_SECRET_{idx} 周四主从切换离线跑批").into_bytes();
        hidden_src.chunks[0].body = format!("BOB_RAW_SECRET_{idx} 周四主从切换离线跑批");
        store.ingest_source(
            &"origin".parse()?,
            &format!("bob-src-{idx}").parse()?,
            &hidden_src,
        )?;
    }

    let after_flood = serde_json::to_vec(
        &store
            .candidate_retrieval_snapshot(&alice, AT, "周四主从切换离线跑批", 4)?
            .documents(),
    )?;
    assert_eq!(baseline, after_flood);

    store.forget("bob_secret_0", &bob, AT)?;
    store.purge(&"origin".parse()?, &"bob-src-0".parse()?, AT, &[])?;
    let after_lifecycle = serde_json::to_vec(
        &store
            .candidate_retrieval_snapshot(&alice, AT, "周四主从切换离线跑批", 4)?
            .documents(),
    )?;
    assert_eq!(baseline, after_lifecycle);
    Ok(())
}

#[test]
fn session_staging_chunks_are_excluded_and_turn_tags_and_assistant_lines_are_stripped() -> Result<()>
{
    use crate::{ChunkInput, RawSourceInput};
    use kp_model::{AnchorSpec, AudienceSpec, ConceptKind, ConceptPolarity, DerivationMode};

    assert_eq!(
        strip_turn_tags_and_assistant_lines(
            "[speaker:alice] 周四下午断网\n[assistant] 好的，记下了。"
        ),
        "周四下午断网"
    );
    assert_eq!(
        strip_turn_tags_and_assistant_lines(
            "[turn:0] [speaker:ext:u1] 周四下午断网 [turn:1] [assistant] 好的，记下了。"
        ),
        "周四下午断网"
    );
    assert_eq!(
        strip_turn_tags_and_assistant_lines(
            "[system] sys\n[developer] dev\n[tool] out\n[turn:2] [user] 暂停跑批脚本"
        ),
        "暂停跑批脚本"
    );
    assert_eq!(
        strip_turn_tags_and_assistant_lines("不要修改 config.toml 里的 [system] 配置"),
        "不要修改 config.toml 里的 [system] 配置"
    );

    let (_dir, mut store) = library(LibraryMode::Community)?;
    let alice: kp_model::ActorId = "alice".parse()?;
    let bob: kp_model::ActorId = "bob".parse()?;
    store.add_member(alice.clone(), kp_model::MemberRole::Member, AT)?;
    store.add_member(bob.clone(), kp_model::MemberRole::Member, AT)?;

    let raw_session_body = "[speaker:alice] 周四下午断网\n[assistant] 好的，记下了。";
    let lib_id = store.library_id().clone();
    let stored_s1 = store.ingest_source(
        &lib_id,
        &"session:s1".parse()?,
        &RawSourceInput {
            self_echo_filtered: true,
            uri: "session:s1".into(),
            media_type: "text/markdown".into(),
            content: raw_session_body.as_bytes().to_vec(),
            readable_by: AudienceSpec::Explicit([alice.clone(), bob.clone()].into_iter().collect()),
            captured_at: AT,
            chunks: vec![ChunkInput {
                heading: String::new(),
                body: raw_session_body.into(),
                anchor: AnchorSpec::default(),
            }],
        },
    )?;

    // Candidate snapshot and `prompt_candidate_meta` never return `session:*` staging chunks!
    let snap = store.candidate_retrieval_snapshot(&alice, AT, "周四下午断网", 12)?;
    assert!(snap.documents().is_empty());
    let chunk_doc_id = format!("chunk-{}-0", stored_s1.source_id);
    let meta = store.prompt_candidate_meta(&alice, AT, std::slice::from_ref(&chunk_doc_id))?;
    assert!(meta.is_empty());

    // Add 3 visible premises + 1 invisible premise (`bob`-only) supporting a `CollisionWarning`.
    let grp = store.ensure_community_concept(
        ConceptKind::Group,
        "机房值班组",
        "ops",
        ConceptPolarity::Neutral,
        &bob,
        AT,
    )?;
    let mut ev = fact(crate::new_fact_id()?.as_str(), &["alice", "bob"])?;
    ev.kind = kp_model::FactKind::Event;
    ev.author = alice.clone();
    ev.subject = "机房网络".into();
    ev.object = serde_json::json!({"summary": "周四下午机房断网维护"});
    ev.source_anchor = Some(AnchorSpec {
        verbatim_quote: Some(
            "[turn:0] [speaker:alice] 周四下午断网\n[assistant] 好的，记下了。".into(),
        ),
        ..AnchorSpec::default()
    });

    let mut rule = fact(crate::new_fact_id()?.as_str(), &["alice", "bob"])?;
    rule.kind = kp_model::FactKind::Constraint;
    rule.author = bob.clone();
    rule.subject = "同步作业".into();
    rule.object = serde_json::json!({"summary": "断网期间暂停全量同步脚本"});
    rule.source_anchor = Some(AnchorSpec {
        verbatim_quote: Some("断网期间不要跑全量同步脚本".into()),
        ..AnchorSpec::default()
    });

    let mut mem = fact(crate::new_fact_id()?.as_str(), &["alice", "bob"])?;
    mem.kind = kp_model::FactKind::Membership;
    mem.author = bob.clone();
    mem.subject = "roster".into();
    mem.object = serde_json::to_value(kp_model::MembershipRelation {
        anchor: bob.clone(),
        member: alice.clone(),
        group_concept_id: grp.id,
    })?;

    let mut secret_prem = fact(crate::new_fact_id()?.as_str(), &["bob"])?;
    secret_prem.kind = kp_model::FactKind::Observation;
    secret_prem.author = bob.clone();
    secret_prem.subject = "私密巡检备注".into();
    secret_prem.object = serde_json::json!({"summary": "BOB_SECRET_PREMISE_DO_NOT_LEAK"});
    secret_prem.source_anchor = Some(AnchorSpec {
        verbatim_quote: Some("BOB_SECRET_PREMISE_DO_NOT_LEAK".into()),
        ..AnchorSpec::default()
    });

    for f in [&ev, &rule, &mem, &secret_prem] {
        store.insert_fact(f)?;
    }

    let warning = kp_model::derived_fact(
        store.library_id(),
        &bob,
        kp_model::FactKind::CollisionWarning,
        "alice",
        "kp.core.event_constraint_collision",
        serde_json::json!({"advice": "周四下午断网期间暂停执行全量同步脚本"}),
        AT,
    )?;
    let j_public = kp_model::justification(
        &warning,
        DerivationMode::HardRule,
        "R2_GOVERNED_COLLISION",
        &[&ev, &rule, &mem],
    )?;
    let j_private = kp_model::justification(
        &warning,
        DerivationMode::HardRule,
        "R2_PRIVATE_EXTRA",
        &[&ev, &secret_prem],
    )?;
    store.save_derivations(std::slice::from_ref(&warning), &[j_public, j_private])?;

    let warn_doc_id = format!("fact-{}", warning.id);
    let warn_meta_map =
        store.prompt_candidate_meta(&alice, AT, std::slice::from_ref(&warn_doc_id))?;
    let warn_meta = warn_meta_map
        .get(&warn_doc_id)
        .ok_or_else(|| KpError::invalid("test", "expected visible warning meta"))?;
    assert_eq!(warn_meta.supporting_premises.len(), 3);
    assert!(warn_meta
        .supporting_premises
        .iter()
        .all(|p| p.fact_id != secret_prem.id
            && !p.claim_or_quote.contains("BOB_SECRET_PREMISE")
            && !p.claim_or_quote.contains("[speaker:")
            && !p.claim_or_quote.contains("[turn:")
            && !p.claim_or_quote.contains("[assistant]")
            && !p.claim_or_quote.contains("好的，记下了")));
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, rng_seed: RngSeed::Fixed(707), ..ProptestConfig::default() })]
    #[test]
    fn reverse_dfs_candidate_snapshot_agrees_with_full_semiring_on_cyclic_graphs(
        base_vis in prop::collection::vec(0u8..4, 3..6),
        edges in prop::collection::vec((0usize..4, prop::collection::vec(0usize..8, 1..=3)), 1..10),
    ) {
        let (_dir, mut store) = library(LibraryMode::Community)
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;
        let alice: kp_model::ActorId = "alice".parse()
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;
        let bob: kp_model::ActorId = "bob".parse()
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;
        store.add_member(alice.clone(), kp_model::MemberRole::Member, AT)
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;
        store.add_member(bob.clone(), kp_model::MemberRole::Member, AT)
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;

        let mut all_facts = Vec::new();
        for (i, mask) in base_vis.iter().enumerate() {
            let aud: Vec<&str> = match mask % 4 {
                0 => vec!["alice"],
                1 => vec!["bob"],
                2 => vec!["alice", "bob"],
                _ => vec!["owner"],
            };
            let mut bf = fact(&format!("b_{i}"), &aud)
                .map_err(|e| TestCaseError::fail(format!("{e}")))?;
            bf.subject = format!("sharedquery node_b_{i}");
            bf.object = serde_json::json!("sharedquery");
            store.insert_fact(&bf).map_err(|e| TestCaseError::fail(format!("{e}")))?;
            all_facts.push(bf);
        }
        let derived_offset = all_facts.len();
        let mut derived_nodes = Vec::new();
        for d in 0..4 {
            let mut df = crate::test_support::derived(&format!("d_{d}"))
                .map_err(|e| TestCaseError::fail(format!("{e}")))?;
            df.subject = format!("sharedquery node_d_{d}");
            df.object = serde_json::json!("sharedquery");
            derived_nodes.push(df.clone());
            all_facts.push(df);
        }

        let mut proofs = Vec::new();
        for (j_idx, (conc_rel, prem_indices)) in edges.into_iter().enumerate() {
            let conclusion = &derived_nodes[conc_rel % derived_nodes.len()];
            let mut uniq_prems = std::collections::BTreeSet::new();
            let mut prem_refs = Vec::new();
            for p_idx in prem_indices {
                let target = &all_facts[p_idx % all_facts.len()];
                if uniq_prems.insert(target.id.clone()) {
                    prem_refs.push(target);
                }
            }
            let jst = crate::test_support::justification(
                &format!("j_{j_idx}"),
                conclusion,
                &prem_refs,
            )
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;
            proofs.push(jst);
        }
        let _ = derived_offset;
        store.save_derivations(&derived_nodes, &proofs)
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;

        for viewer in [&alice, &bob] {
            let full_ids: std::collections::BTreeSet<String> = store
                .retrieval_snapshot(viewer, AT)
                .map_err(|e| TestCaseError::fail(format!("{e}")))?
                .documents()
                .iter()
                .map(|d| d.id.clone())
                .collect();
            let cand_ids: std::collections::BTreeSet<String> = store
                .candidate_retrieval_snapshot(viewer, AT, "sharedquery", 64)
                .map_err(|e| TestCaseError::fail(format!("{e}")))?
                .documents()
                .iter()
                .map(|d| d.id.clone())
                .collect();
            prop_assert_eq!(full_ids, cand_ids);
        }
    }
}
