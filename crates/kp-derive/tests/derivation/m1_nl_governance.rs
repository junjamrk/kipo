use std::collections::BTreeSet;

use kp_derive::{
    align_concepts_and_derive, apply_extracted_atoms, build_extraction_batch_request,
    extract_and_ingest_batch, reject_fact_and_record, resolve_ambiguous_scope_review,
    verify_hard_rules_match_full_recompute, HostCliConfig, HostCliRunner, NlBatchStatus,
    NlFunnelOptions, RecordingMockTransport, ScopeReviewDecision,
};
use kp_model::{
    format_tagged_turns, is_bare_content_hash, AudienceSpec, BitemporalStamp, CalendarTable,
    ChunkTurn, ConceptKind, ConceptPolarity, DeterministicMockLlm, EntailmentVerdict, EpochMs,
    ExtractionBatchOutcome, ExtractionChunkSlice, Fact, FactAccess, FactKind, FactRank, FactStatus,
    GovernedScope, LibraryMode, LlmClient, MemberRole, ModelAmbiguousScope, ModelConceptRef,
    ModelDateRange, ModelExtractedAtom, ModelQuoteAnchor, ModelRoutePolicy, ModelScopeRef,
    ModelWriteOp, ReviewKind, ReviewStatus, SourceOriginMeta, SourceScope, SourceScopeId,
    TrustTier, ValidityWindow,
};
use kp_store::{InitOptions, Store};
use serde_json::json;

use super::Result;

fn community_three_members() -> Result<(tempfile::TempDir, Store)> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::init(
        dir.path(),
        InitOptions {
            library_id: "club-hub".parse()?,
            name: "Club Hub".into(),
            mode: LibraryMode::Community,
            owner: "alice".parse()?,
            created_at: EpochMs(1_795_000_000_000),
        },
    )?;
    store.add_member(
        "bob".parse()?,
        MemberRole::Member,
        EpochMs(1_795_000_000_000),
    )?;
    store.add_member(
        "carol".parse()?,
        MemberRole::Member,
        EpochMs(1_795_000_000_000),
    )?;
    Ok((dir, store))
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn m1_journey_open_concepts_three_premise_collision_privacy_expiry_and_reopen() -> Result {
    let (_dir, mut store) = community_three_members()?;
    let alice: kp_model::ActorId = "alice".parse()?;
    let bob: kp_model::ActorId = "bob".parse()?;
    let carol: kp_model::ActorId = "carol".parse()?;

    // Reference time: Thursday 2026-11-19 20:00 UTC+8.
    let t_thu = EpochMs(1_795_089_600_000);
    let shanghai = kp_model::TimeZone::get("Asia/Shanghai")?;
    let calendar = CalendarTable::from_reference_in_tz(t_thu, &shanghai)?;
    let fri_win = calendar
        .window_for_date("2026-11-20")
        .ok_or("missing Friday window")?;
    let fri_noon = EpochMs(fri_win.valid_from.0 + 12 * 3_600_000);
    let sat_noon = EpochMs(fri_win.valid_until.0 + 12 * 3_600_000);

    let mock_llm = DeterministicMockLlm::default();
    mock_llm.register_entailment("泳池换水停业", "场馆闭馆检修", EntailmentVerdict::Entails)?;
    mock_llm.register_entailment("器械区消杀", "场馆闭馆检修", EntailmentVerdict::Entails)?;
    mock_llm.register_entailment(
        "泳池恢复开放",
        "泳池换水停业",
        EntailmentVerdict::Contradicts,
    )?;

    // 1) Bob states team membership + group training constraint (`audience = {alice, bob}`).
    let bob_body = "Alice 是我们游泳队队员。我们游泳队场馆闭馆检修时改练陆上核心训练。";
    mock_llm.register_chunk_outcome(
        bob_body,
        ExtractionBatchOutcome::Completed {
            atoms: vec![
                ModelExtractedAtom {
                    kind: FactKind::Membership,
                    claim_text: "Alice 是游泳队队员".into(),
                    value: json!({"relation": "swim_team"}),
                    scope: None,
                    ambiguous_scope: None,
                    condition_concept: None,
                    action_concept: None,
                    group_concept: Some(ModelConceptRef::ProposeNew {
                        kind: ConceptKind::Group,
                        label: "游泳队".into(),
                        domain: "sports".into(),
                        polarity: ConceptPolarity::Neutral,
                    }),
                    member_mention: Some("Alice".into()),
                    valid_window: None,
                    valid_dates: None,
                    speaker: None,
                    assumed_premise_ids: Vec::new(),
                    quote: ModelQuoteAnchor {
                        chunk_id: "b0".into(),
                        verbatim_quote: "Alice 是我们游泳队队员".into(),
                        turn_ordinal: None,
                        approx_char_offset: None,
                    },
                    write_op: ModelWriteOp::Add,
                    evidence: None,
                    command_guard: None,
                },
                ModelExtractedAtom {
                    kind: FactKind::Constraint,
                    claim_text: "游泳队场馆闭馆检修时改练陆上核心训练".into(),
                    value: json!({"substitute": "core_workout"}),
                    scope: Some(ModelScopeRef::RelationGroup {
                        group_concept: ModelConceptRef::ProposeNew {
                            kind: ConceptKind::Group,
                            label: "游泳队".into(),
                            domain: "sports".into(),
                            polarity: ConceptPolarity::Neutral,
                        },
                    }),
                    ambiguous_scope: None,
                    condition_concept: Some(ModelConceptRef::ProposeNew {
                        kind: ConceptKind::Condition,
                        label: "场馆闭馆检修".into(),
                        domain: "facility".into(),
                        polarity: ConceptPolarity::Negative,
                    }),
                    action_concept: Some(ModelConceptRef::ProposeNew {
                        kind: ConceptKind::Action,
                        label: "改练陆上核心训练".into(),
                        domain: "sports".into(),
                        polarity: ConceptPolarity::Positive,
                    }),
                    group_concept: None,
                    member_mention: None,
                    valid_window: None,
                    valid_dates: None,
                    speaker: None,
                    assumed_premise_ids: Vec::new(),
                    quote: ModelQuoteAnchor {
                        chunk_id: "b0".into(),
                        verbatim_quote: "我们游泳队场馆闭馆检修时改练陆上核心训练".into(),
                        turn_ordinal: None,
                        approx_char_offset: None,
                    },
                    write_op: ModelWriteOp::Add,
                    evidence: None,
                    command_guard: None,
                },
            ],
        },
    )?;

    let bob_req = build_extraction_batch_request(
        &store,
        "src_bob_1".parse()?,
        bob.clone(),
        calendar.clone(),
        Vec::new(),
        vec![ExtractionChunkSlice {
            chunk_id: "b0".into(),
            heading: "swim-chat".into(),
            body: bob_body.into(),
            source_id: "src_bob_1".parse()?,
            author: bob.clone(),
            base_anchor: Default::default(),
            role: Default::default(),
            reply_to_chunk_id: None,
        }],
        10,
    )?;
    let bob_out = extract_and_ingest_batch(
        &mut store,
        &mock_llm,
        &bob_req,
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([alice.clone(), bob.clone()])),
            t_thu,
        ),
    )
    .await?;
    assert_eq!(bob_out.status, NlBatchStatus::Completed);
    assert_eq!(bob_out.created_fact_ids.len(), 2);
    // "游泳队" proposed twice in the same batch reuses the same opaque `cpt_*` ID!
    assert_eq!(bob_out.created_concepts.len(), 3);
    assert!(bob_out
        .created_concepts
        .iter()
        .all(|c| c.id.as_str().starts_with("cpt_") && !is_bare_content_hash(c.id.as_str())));
    assert!(bob_out
        .created_fact_ids
        .iter()
        .all(|id| id.as_str().starts_with("fact_") && !is_bare_content_hash(id.as_str())));

    // 2) Alice posts facility notice using calendar `valid_dates` (no epoch ms copying!).
    let alice_body = "场馆公告：周五（11/20）泳池换水停业！";
    mock_llm.register_chunk_outcome(
        alice_body,
        ExtractionBatchOutcome::Completed {
            atoms: vec![ModelExtractedAtom {
                kind: FactKind::Event,
                claim_text: "周五泳池换水停业".into(),
                value: json!({"facility": "pool_closed"}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "泳池换水停业".into(),
                    domain: "facility".into(),
                    polarity: ConceptPolarity::Negative,
                }),
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: Some(ModelDateRange {
                    start_date_iso: "2026-11-20".into(),
                    end_date_iso: None,
                    open_ended: false,
                }),
                speaker: None,
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: "a0".into(),
                    verbatim_quote: "周五(11/20)泳池换水停业!".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
                evidence: None,
                command_guard: None,
            }],
        },
    )?;

    let alice_req = build_extraction_batch_request(
        &store,
        "src_alice_1".parse()?,
        alice.clone(),
        calendar.clone(),
        bob_out.active_concepts.clone(),
        vec![ExtractionChunkSlice {
            chunk_id: "a0".into(),
            heading: "notice".into(),
            body: alice_body.into(),
            source_id: "src_alice_1".parse()?,
            author: alice.clone(),
            base_anchor: Default::default(),
            role: Default::default(),
            reply_to_chunk_id: None,
        }],
        10,
    )?;
    let alice_out = extract_and_ingest_batch(
        &mut store,
        &mock_llm,
        &alice_req,
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([alice.clone(), bob.clone()])),
            EpochMs(t_thu.0 + 1_000),
        ),
    )
    .await?;
    let all_concepts = alice_out.active_concepts.clone();

    // 3) Align open concepts and derive person-targeted collisions.
    let col_out = align_concepts_and_derive(
        &mut store,
        &mock_llm,
        &all_concepts,
        &[],
        EpochMs(t_thu.0 + 2_000),
    )
    .await?;
    assert!(col_out.derived_warnings >= 1);
    assert!(col_out.emitted_reviews.is_empty());
    verify_hard_rules_match_full_recompute(&store)?;

    let alice_fri = store.visible_facts(&alice, fri_noon)?;
    let alice_warnings: Vec<&Fact> = alice_fri
        .iter()
        .filter(|f| f.kind == FactKind::CollisionWarning)
        .collect();
    assert_eq!(alice_warnings.len(), 1);
    assert_eq!(alice_warnings[0].subject, "alice");

    // Single conjunctive Justification over all 3 premises (`event`, `constraint`, `membership`).
    let proof_tree = kp_derive::why::why(&store, &alice_warnings[0].id, &alice, fri_noon)?
        .ok_or("missing proof tree for Alice warning")?;
    assert_eq!(proof_tree.justifications.len(), 1);
    assert_eq!(proof_tree.justifications[0].premises.len(), 3);

    // Outsider Carol sees ZERO facts, ZERO concepts/alignments, and ZERO warnings!
    assert!(store.visible_facts(&carol, fri_noon)?.is_empty());

    // After Friday window expires (Saturday noon), Alice's warning disappears automatically!
    let alice_sat = store.visible_facts(&alice, sat_noon)?;
    assert!(!alice_sat
        .iter()
        .any(|f| f.kind == FactKind::CollisionWarning));

    // 4) Contradicting update ("泳池恢复开放") supersedes "泳池换水停业" -> Friday warning vanishes!
    let reopen_body = "补充公告：换水提前完成，周五泳池恢复开放！";
    mock_llm.register_chunk_outcome(
        reopen_body,
        ExtractionBatchOutcome::Completed {
            atoms: vec![ModelExtractedAtom {
                kind: FactKind::Event,
                claim_text: "周五泳池恢复开放".into(),
                value: json!({"facility": "pool_open"}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "泳池恢复开放".into(),
                    domain: "facility".into(),
                    polarity: ConceptPolarity::Resolved,
                }),
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: Some(ModelDateRange {
                    start_date_iso: "2026-11-20".into(),
                    end_date_iso: None,
                    open_ended: false,
                }),
                speaker: None,
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: "a1".into(),
                    verbatim_quote: "周五泳池恢复开放".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
                evidence: None,
                command_guard: None,
            }],
        },
    )?;

    let reopen_req = build_extraction_batch_request(
        &store,
        "src_alice_2".parse()?,
        alice.clone(),
        calendar,
        all_concepts,
        vec![ExtractionChunkSlice {
            chunk_id: "a1".into(),
            heading: "notice".into(),
            body: reopen_body.into(),
            source_id: "src_alice_2".parse()?,
            author: alice.clone(),
            base_anchor: Default::default(),
            role: Default::default(),
            reply_to_chunk_id: None,
        }],
        10,
    )?;
    let reopen_out = extract_and_ingest_batch(
        &mut store,
        &mock_llm,
        &reopen_req,
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([alice.clone(), bob.clone()])),
            EpochMs(t_thu.0 + 5_000),
        ),
    )
    .await?;

    align_concepts_and_derive(
        &mut store,
        &mock_llm,
        &reopen_out.active_concepts,
        &[],
        EpochMs(t_thu.0 + 6_000),
    )
    .await?;
    verify_hard_rules_match_full_recompute(&store)?;

    let alice_after_reopen = store.visible_facts(&alice, fri_noon)?;
    assert!(!alice_after_reopen
        .iter()
        .any(|f| f.kind == FactKind::CollisionWarning));
    Ok(())
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn m1_journey_self_only_never_leaks_and_ambiguous_scope_reviews_only_on_cross_person_impact(
) -> Result {
    let (_dir, mut store) = community_three_members()?;
    let alice: kp_model::ActorId = "alice".parse()?;
    let bob: kp_model::ActorId = "bob".parse()?;

    let t0 = EpochMs(1_795_089_600_000);
    let shanghai = kp_model::TimeZone::get("Asia/Shanghai")?;
    let calendar = CalendarTable::from_reference_in_tz(t0, &shanghai)?;
    let fri_win = calendar
        .window_for_date("2026-11-20")
        .ok_or("missing fri")?;
    let fri_noon = EpochMs(fri_win.valid_from.0 + 10 * 3_600_000);

    let mock_llm = DeterministicMockLlm::default();
    mock_llm.register_entailment("跨城深夜航班", "出差航班", EntailmentVerdict::Entails)?;

    let bob_chunk =
        "Alice 是我们差旅组成员。我自己出差航班不选红眼班次。另外记一下：出差航班不选红眼班次。";
    let setup_req = build_extraction_batch_request(
        &store,
        "src_bob_scope".parse()?,
        bob.clone(),
        calendar.clone(),
        Vec::new(),
        vec![ExtractionChunkSlice {
            chunk_id: "c0".into(),
            heading: "travel-policy".into(),
            body: bob_chunk.into(),
            source_id: "src_bob_scope".parse()?,
            author: bob.clone(),
            base_anchor: Default::default(),
            role: Default::default(),
            reply_to_chunk_id: None,
        }],
        10,
    )?;
    let setup_atoms = [
        ModelExtractedAtom {
            kind: FactKind::Membership,
            claim_text: "Alice 是差旅组成员".into(),
            value: json!({}),
            scope: None,
            ambiguous_scope: None,
            condition_concept: None,
            action_concept: None,
            group_concept: Some(ModelConceptRef::ProposeNew {
                kind: ConceptKind::Group,
                label: "差旅组".into(),
                domain: "org".into(),
                polarity: ConceptPolarity::Neutral,
            }),
            member_mention: Some("Alice".into()),
            valid_window: None,
            valid_dates: None,
            speaker: None,
            assumed_premise_ids: Vec::new(),
            quote: ModelQuoteAnchor {
                chunk_id: "c0".into(),
                verbatim_quote: "Alice 是我们差旅组成员".into(),
                turn_ordinal: None,
                approx_char_offset: None,
            },
            write_op: ModelWriteOp::Add,
            evidence: None,
            command_guard: None,
        },
        ModelExtractedAtom {
            kind: FactKind::Constraint,
            claim_text: "我自己出差航班不选红眼班次".into(),
            value: json!({"mode": "self"}),
            scope: Some(ModelScopeRef::SpeakerSelf),
            ambiguous_scope: None,
            condition_concept: Some(ModelConceptRef::ProposeNew {
                kind: ConceptKind::Condition,
                label: "出差航班".into(),
                domain: "travel".into(),
                polarity: ConceptPolarity::Neutral,
            }),
            action_concept: Some(ModelConceptRef::ProposeNew {
                kind: ConceptKind::Action,
                label: "不选红眼班次".into(),
                domain: "travel".into(),
                polarity: ConceptPolarity::Negative,
            }),
            group_concept: None,
            member_mention: None,
            valid_window: None,
            valid_dates: None,
            speaker: None,
            assumed_premise_ids: Vec::new(),
            quote: ModelQuoteAnchor {
                chunk_id: "c0".into(),
                verbatim_quote: "我自己出差航班不选红眼班次".into(),
                turn_ordinal: None,
                approx_char_offset: None,
            },
            write_op: ModelWriteOp::Add,
            evidence: None,
            command_guard: None,
        },
        ModelExtractedAtom {
            kind: FactKind::Constraint,
            claim_text: "出差航班不选红眼班次".into(),
            value: json!({"mode": "ambiguous_subjectless"}),
            scope: None,
            ambiguous_scope: Some(ModelAmbiguousScope {
                broader_candidate: ModelScopeRef::RelationGroup {
                    group_concept: ModelConceptRef::ProposeNew {
                        kind: ConceptKind::Group,
                        label: "差旅组".into(),
                        domain: "org".into(),
                        polarity: ConceptPolarity::Neutral,
                    },
                },
                question: "这条差旅偏好只适用于你自己，还是整个差旅组？".into(),
            }),
            condition_concept: Some(ModelConceptRef::ProposeNew {
                kind: ConceptKind::Condition,
                label: "出差航班".into(),
                domain: "travel".into(),
                polarity: ConceptPolarity::Neutral,
            }),
            action_concept: Some(ModelConceptRef::ProposeNew {
                kind: ConceptKind::Action,
                label: "不选红眼班次".into(),
                domain: "travel".into(),
                polarity: ConceptPolarity::Negative,
            }),
            group_concept: None,
            member_mention: None,
            valid_window: None,
            valid_dates: None,
            speaker: None,
            assumed_premise_ids: Vec::new(),
            quote: ModelQuoteAnchor {
                chunk_id: "c0".into(),
                verbatim_quote: "出差航班不选红眼班次".into(),
                turn_ordinal: None,
                approx_char_offset: Some(37),
            },
            write_op: ModelWriteOp::Add,
            evidence: None,
            command_guard: None,
        },
    ];

    // Step 1: Bob first states the self-only + ambiguous constraints BEFORE any other member is
    // added to `差旅组` -> `impacted_actors` is empty, so ZERO review items are emitted!
    let bob_out = apply_extracted_atoms(
        &mut store,
        &setup_req,
        &setup_atoms[1..],
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([alice.clone(), bob.clone()])),
            t0,
        ),
    )?;
    let ambiguous_constraint_id = bob_out.created_fact_ids[1].clone();
    let stored_amb = &store.derivation_view()?.facts[&ambiguous_constraint_id];
    assert_eq!(
        stored_amb.governed_scope,
        Some(GovernedScope::SelfOnly { actor: bob.clone() })
    );
    assert!(stored_amb.broader_scope.is_some());

    let before_members =
        align_concepts_and_derive(&mut store, &mock_llm, &bob_out.active_concepts, &[], t0).await?;
    assert!(before_members.emitted_reviews.is_empty());

    // Step 1b: Alice records a PRIVATE flight event (`audience = {alice}`, hidden from Bob)
    // -> MUST NOT trigger a side-channel review item for Bob or leak a warning!
    let priv_req = build_extraction_batch_request(
        &store,
        "src_alice_priv".parse()?,
        alice.clone(),
        calendar.clone(),
        bob_out.active_concepts.clone(),
        vec![ExtractionChunkSlice {
            chunk_id: "ap".into(),
            heading: "itinerary".into(),
            body: "周五有跨城深夜航班".into(),
            source_id: "src_alice_priv".parse()?,
            author: alice.clone(),
            base_anchor: Default::default(),
            role: Default::default(),
            reply_to_chunk_id: None,
        }],
        10,
    )?;
    let priv_out = apply_extracted_atoms(
        &mut store,
        &priv_req,
        &[ModelExtractedAtom {
            kind: FactKind::Event,
            claim_text: "周五有跨城深夜航班".into(),
            value: json!({}),
            scope: Some(ModelScopeRef::SpeakerSelf),
            ambiguous_scope: None,
            condition_concept: Some(ModelConceptRef::ProposeNew {
                kind: ConceptKind::Condition,
                label: "跨城深夜航班".into(),
                domain: "travel".into(),
                polarity: ConceptPolarity::Neutral,
            }),
            action_concept: None,
            group_concept: None,
            member_mention: None,
            valid_window: None,
            valid_dates: Some(ModelDateRange {
                start_date_iso: "2026-11-20".into(),
                end_date_iso: None,
                open_ended: false,
            }),
            speaker: None,
            assumed_premise_ids: Vec::new(),
            quote: ModelQuoteAnchor {
                chunk_id: "ap".into(),
                verbatim_quote: "周五有跨城深夜航班".into(),
                turn_ordinal: None,
                approx_char_offset: None,
            },
            write_op: ModelWriteOp::Add,
            evidence: None,
            command_guard: None,
        }],
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([alice.clone()])),
            EpochMs(t0.0 + 100),
        ),
    )?;
    let after_priv = align_concepts_and_derive(
        &mut store,
        &mock_llm,
        &priv_out.active_concepts,
        &[],
        EpochMs(t0.0 + 200),
    )
    .await?;
    assert!(after_priv.emitted_reviews.is_empty());
    assert!(!store
        .visible_facts(&alice, fri_noon)?
        .iter()
        .any(|f| f.kind == FactKind::CollisionWarning));

    // Step 1c: Bob records `Membership(bob -> alice, 差旅组)` locally: even BEFORE any shared
    // flight event exists in Bob's view, `AmbiguousScope` review is immediately emitted for Bob
    // because `差旅组` now governs known member `alice != bob`!
    let mut mem_opts = NlFunnelOptions::personal_or_shared(
        AudienceSpec::Explicit(BTreeSet::from([alice.clone(), bob.clone()])),
        EpochMs(t0.0 + 250),
    );
    mem_opts.extra_concepts = priv_out.active_concepts.clone();
    let mem_out = apply_extracted_atoms(&mut store, &setup_req, &setup_atoms[0..1], &mem_opts)?;
    let local_rev_out = align_concepts_and_derive(
        &mut store,
        &mock_llm,
        &mem_out.active_concepts,
        &[],
        EpochMs(t0.0 + 260),
    )
    .await?;
    assert_eq!(local_rev_out.emitted_reviews.len(), 1);
    assert_eq!(local_rev_out.emitted_reviews[0].actor, bob);
    assert_eq!(
        local_rev_out.emitted_reviews[0].kind,
        ReviewKind::AmbiguousScope
    );

    // Step 2: Shared flight event (`audience = {alice, bob}`) arrives!
    let shared_req = build_extraction_batch_request(
        &store,
        "src_shared_flight".parse()?,
        alice.clone(),
        calendar,
        mem_out.active_concepts.clone(),
        vec![ExtractionChunkSlice {
            chunk_id: "as".into(),
            heading: "team-trip".into(),
            body: "行政通知：周五有跨城深夜航班".into(),
            source_id: "src_shared_flight".parse()?,
            author: alice.clone(),
            base_anchor: Default::default(),
            role: Default::default(),
            reply_to_chunk_id: None,
        }],
        10,
    )?;
    let shared_out = apply_extracted_atoms(
        &mut store,
        &shared_req,
        &[ModelExtractedAtom {
            kind: FactKind::Event,
            claim_text: "周五有跨城深夜航班".into(),
            value: json!({}),
            scope: None,
            ambiguous_scope: None,
            condition_concept: Some(ModelConceptRef::Existing {
                concept_id: mem_out
                    .active_concepts
                    .iter()
                    .find(|c| c.canonical_label == "跨城深夜航班")
                    .ok_or("missing flight concept")?
                    .id
                    .clone(),
            }),
            action_concept: None,
            group_concept: None,
            member_mention: None,
            valid_window: None,
            valid_dates: Some(ModelDateRange {
                start_date_iso: "2026-11-20".into(),
                end_date_iso: None,
                open_ended: false,
            }),
            speaker: None,
            assumed_premise_ids: Vec::new(),
            quote: ModelQuoteAnchor {
                chunk_id: "as".into(),
                verbatim_quote: "周五有跨城深夜航班".into(),
                turn_ordinal: None,
                approx_char_offset: None,
            },
            write_op: ModelWriteOp::Add,
            evidence: None,
            command_guard: None,
        }],
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([alice.clone(), bob.clone()])),
            EpochMs(t0.0 + 300),
        ),
    )?;

    let pending = align_concepts_and_derive(
        &mut store,
        &mock_llm,
        &shared_out.active_concepts,
        &[],
        EpochMs(t0.0 + 400),
    )
    .await?;

    // Before Bob confirms: Alice STILL sees ZERO CollisionWarnings!
    assert!(!store
        .visible_facts(&alice, fri_noon)?
        .iter()
        .any(|f| f.kind == FactKind::CollisionWarning));
    assert!(store
        .visible_facts(&bob, fri_noon)?
        .iter()
        .any(|f| f.kind == FactKind::CollisionWarning && f.subject == "bob"));
    assert_eq!(pending.emitted_reviews.len(), 1);
    let rev = &pending.emitted_reviews[0];
    assert_eq!(rev.actor, bob);
    assert_eq!(rev.kind, ReviewKind::AmbiguousScope);
    assert_eq!(rev.target_id, ambiguous_constraint_id.as_str());

    // Alice cannot confirm Bob's ambiguous constraint!
    assert!(resolve_ambiguous_scope_review(
        &mut store,
        &ambiguous_constraint_id,
        &alice,
        ScopeReviewDecision::ConfirmBroader,
        Some(rev),
        EpochMs(t0.0 + 500),
    )
    .is_err());

    // Step 3: Bob confirms broader group scope -> Alice's CollisionWarning immediately materializes!
    let resolved = resolve_ambiguous_scope_review(
        &mut store,
        &ambiguous_constraint_id,
        &bob,
        ScopeReviewDecision::ConfirmBroader,
        Some(rev),
        EpochMs(t0.0 + 600),
    )?;
    assert_eq!(
        resolved.resolved_review.as_ref().map(|r| r.status),
        Some(ReviewStatus::Confirmed)
    );
    verify_hard_rules_match_full_recompute(&store)?;

    let alice_after = store.visible_facts(&alice, fri_noon)?;
    assert!(alice_after
        .iter()
        .any(|f| f.kind == FactKind::CollisionWarning && f.subject == "alice"));
    Ok(())
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn m1_journey_unrelated_city_blocks_false_positive_and_ambiguous_alignment_queues_review(
) -> Result {
    let (_dir, mut store) = community_three_members()?;
    let alice: kp_model::ActorId = "alice".parse()?;
    let bob: kp_model::ActorId = "bob".parse()?;
    let carol: kp_model::ActorId = "carol".parse()?;

    let t0 = EpochMs(1_795_089_600_000);
    let shanghai = kp_model::TimeZone::get("Asia/Shanghai")?;
    let calendar = CalendarTable::from_reference_in_tz(t0, &shanghai)?;
    let fri_win = calendar
        .window_for_date("2026-11-20")
        .ok_or("missing fri")?;
    let fri_noon = EpochMs(fri_win.valid_from.0 + 3_600_000);

    let mock_llm = DeterministicMockLlm::default();
    mock_llm.register_entailment("上海国际书展", "聚餐不点刺身", EntailmentVerdict::Unrelated)?;
    mock_llm.register_entailment(
        "轻度浮尘",
        "空气污染停跑",
        EntailmentVerdict::Ambiguous {
            question: "“轻度浮尘”是否触发“空气污染停跑”？".into(),
        },
    )?;

    let chunk = "周五上海国际书展。在上海聚餐不点刺身。周五户外有轻度浮尘。空气污染停跑。";
    let req = build_extraction_batch_request(
        &store,
        "src_sh".parse()?,
        bob.clone(),
        calendar,
        Vec::new(),
        vec![ExtractionChunkSlice {
            chunk_id: "sh0".into(),
            heading: "上海".into(),
            body: chunk.into(),
            source_id: "src_sh".parse()?,
            author: bob.clone(),
            base_anchor: Default::default(),
            role: Default::default(),
            reply_to_chunk_id: None,
        }],
        10,
    )?;
    let out = apply_extracted_atoms(
        &mut store,
        &req,
        &[
            ModelExtractedAtom {
                kind: FactKind::Event,
                claim_text: "周五上海国际书展".into(),
                value: json!({"subject": "上海"}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "上海国际书展".into(),
                    domain: "expo".into(),
                    polarity: ConceptPolarity::Neutral,
                }),
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: Some(ModelDateRange {
                    start_date_iso: "2026-11-20".into(),
                    end_date_iso: None,
                    open_ended: false,
                }),
                speaker: None,
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: "sh0".into(),
                    verbatim_quote: "周五上海国际书展".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
                evidence: None,
                command_guard: None,
            },
            ModelExtractedAtom {
                kind: FactKind::Constraint,
                claim_text: "在上海聚餐不点刺身".into(),
                value: json!({"subject": "上海"}),
                scope: Some(ModelScopeRef::NamedMentions {
                    mentions: vec!["Alice".into(), "Bob".into()],
                }),
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "聚餐不点刺身".into(),
                    domain: "dining".into(),
                    polarity: ConceptPolarity::Negative,
                }),
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: None,
                speaker: None,
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: "sh0".into(),
                    verbatim_quote: "在上海聚餐不点刺身".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
                evidence: None,
                command_guard: None,
            },
            ModelExtractedAtom {
                kind: FactKind::Event,
                claim_text: "周五户外有轻度浮尘".into(),
                value: json!({"subject": "上海"}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "轻度浮尘".into(),
                    domain: "air".into(),
                    polarity: ConceptPolarity::Negative,
                }),
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: Some(ModelDateRange {
                    start_date_iso: "2026-11-20".into(),
                    end_date_iso: None,
                    open_ended: false,
                }),
                speaker: None,
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: "sh0".into(),
                    verbatim_quote: "周五户外有轻度浮尘".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
                evidence: None,
                command_guard: None,
            },
            ModelExtractedAtom {
                kind: FactKind::Constraint,
                claim_text: "空气污染停跑".into(),
                value: json!({"subject": "上海"}),
                scope: Some(ModelScopeRef::SpeakerSelf),
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "空气污染停跑".into(),
                    domain: "running".into(),
                    polarity: ConceptPolarity::Negative,
                }),
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: None,
                speaker: None,
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: "sh0".into(),
                    verbatim_quote: "空气污染停跑".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
                evidence: None,
                command_guard: None,
            },
        ],
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([alice.clone(), bob.clone()])),
            t0,
        ),
    )?;

    let col =
        align_concepts_and_derive(&mut store, &mock_llm, &out.active_concepts, &[], t0).await?;
    assert_eq!(col.derived_warnings, 0);
    assert!(!store
        .visible_facts(&alice, fri_noon)?
        .iter()
        .any(|f| f.kind == FactKind::CollisionWarning));
    assert!(!store
        .visible_facts(&bob, fri_noon)?
        .iter()
        .any(|f| f.kind == FactKind::CollisionWarning));

    assert_eq!(col.emitted_reviews.len(), 1);
    assert_eq!(col.emitted_reviews[0].kind, ReviewKind::ConceptAlignment);
    assert_eq!(col.emitted_reviews[0].actor, bob);
    assert!(store.visible_facts(&carol, fri_noon)?.is_empty());
    Ok(())
}

#[allow(clippy::too_many_lines)]
#[test]
fn m1_journey_dual_key_rejection_blocks_reworded_reextraction_while_forget_allows_relearning(
) -> Result {
    let (_dir, mut store) = community_three_members()?;
    let bob: kp_model::ActorId = "bob".parse()?;
    let t0 = EpochMs(1_795_089_600_000);
    let shanghai = kp_model::TimeZone::get("Asia/Shanghai")?;
    let calendar = CalendarTable::from_reference_in_tz(t0, &shanghai)?;

    let chunk_body = "Bob：周六读书会分享环节一律禁止录音，切记。";
    let req = build_extraction_batch_request(
        &store,
        "src_rej".parse()?,
        bob.clone(),
        calendar.clone(),
        Vec::new(),
        vec![ExtractionChunkSlice {
            chunk_id: "r0".into(),
            heading: "club".into(),
            body: chunk_body.into(),
            source_id: "src_rej".parse()?,
            author: bob.clone(),
            base_anchor: Default::default(),
            role: Default::default(),
            reply_to_chunk_id: None,
        }],
        10,
    )?;

    let initial = apply_extracted_atoms(
        &mut store,
        &req,
        &[ModelExtractedAtom {
            kind: FactKind::Constraint,
            claim_text: "读书会分享禁止录音".into(),
            value: json!({"v": 1}),
            scope: Some(ModelScopeRef::SpeakerSelf),
            ambiguous_scope: None,
            condition_concept: Some(ModelConceptRef::ProposeNew {
                kind: ConceptKind::Condition,
                label: "读书会分享".into(),
                domain: "club".into(),
                polarity: ConceptPolarity::Neutral,
            }),
            action_concept: Some(ModelConceptRef::ProposeNew {
                kind: ConceptKind::Action,
                label: "禁止录音".into(),
                domain: "club".into(),
                polarity: ConceptPolarity::Negative,
            }),
            group_concept: None,
            member_mention: None,
            valid_window: None,
            valid_dates: None,
            speaker: None,
            assumed_premise_ids: Vec::new(),
            quote: ModelQuoteAnchor {
                chunk_id: "r0".into(),
                verbatim_quote: "读书会分享环节一律禁止录音".into(),
                turn_ordinal: None,
                approx_char_offset: None,
            },
            write_op: ModelWriteOp::Add,
            evidence: None,
            command_guard: None,
        }],
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([bob.clone()])),
            t0,
        ),
    )?;
    let fact_id = initial.created_fact_ids[0].clone();

    let rejection = reject_fact_and_record(
        &mut store,
        &fact_id,
        &bob,
        "记错了，允许经同意后录音",
        None,
        None,
        EpochMs(t0.0 + 100),
    )?;
    assert!(rejection.id.as_str().starts_with("rej_"));
    assert!(store.visible_facts(&bob, EpochMs(t0.0 + 200))?.is_empty());

    // Re-extraction on the same chunk where LLM completely rewords claim_text & concepts
    // on an overlapping span -> BLOCKED by evidence key!
    let mut opts_with_rej = NlFunnelOptions::personal_or_shared(
        AudienceSpec::Explicit(BTreeSet::from([bob.clone()])),
        EpochMs(t0.0 + 300),
    );
    opts_with_rej.rejections = vec![rejection];
    opts_with_rej.extra_concepts = initial.active_concepts.clone();

    let reworded_out = apply_extracted_atoms(
        &mut store,
        &req,
        &[ModelExtractedAtom {
            kind: FactKind::Constraint,
            claim_text: "沙龙讨论期间严禁开启录音设备".into(),
            value: json!({"completely_reworded": true}),
            scope: Some(ModelScopeRef::SpeakerSelf),
            ambiguous_scope: None,
            condition_concept: Some(ModelConceptRef::ProposeNew {
                kind: ConceptKind::Condition,
                label: "沙龙讨论".into(),
                domain: "club".into(),
                polarity: ConceptPolarity::Neutral,
            }),
            action_concept: None,
            group_concept: None,
            member_mention: None,
            valid_window: None,
            valid_dates: None,
            speaker: None,
            assumed_premise_ids: Vec::new(),
            quote: ModelQuoteAnchor {
                chunk_id: "r0".into(),
                verbatim_quote: "环节一律禁止录音，切记".into(),
                turn_ordinal: None,
                approx_char_offset: None,
            },
            write_op: ModelWriteOp::Add,
            evidence: None,
            command_guard: None,
        }],
        &opts_with_rej,
    )?;
    assert!(reworded_out.created_fact_ids.is_empty());
    assert!(reworded_out.created_concepts.is_empty());
    assert_eq!(reworded_out.dropped_rejected.len(), 1);
    assert!(store.visible_facts(&bob, EpochMs(t0.0 + 400))?.is_empty());

    // Re-extraction on a DIFFERENT chunk with identical normalized semantic key -> BLOCKED by semantic key!
    let req2 = build_extraction_batch_request(
        &store,
        "src_rej_2".parse()?,
        bob.clone(),
        calendar.clone(),
        initial.active_concepts.clone(),
        vec![ExtractionChunkSlice {
            chunk_id: "r1".into(),
            heading: "later".into(),
            body: "换个地方再提一次：读书会禁止录音。".into(),
            source_id: "src_rej_2".parse()?,
            author: bob.clone(),
            base_anchor: Default::default(),
            role: Default::default(),
            reply_to_chunk_id: None,
        }],
        10,
    )?;
    let sem_blocked = apply_extracted_atoms(
        &mut store,
        &req2,
        &[ModelExtractedAtom {
            kind: FactKind::Constraint,
            claim_text: "换种措辞：沙龙不录音".into(),
            value: json!({"v": 99}),
            scope: Some(ModelScopeRef::SpeakerSelf),
            ambiguous_scope: None,
            condition_concept: Some(ModelConceptRef::Existing {
                concept_id: initial.active_concepts[0].id.clone(),
            }),
            action_concept: Some(ModelConceptRef::Existing {
                concept_id: initial.active_concepts[1].id.clone(),
            }),
            group_concept: None,
            member_mention: None,
            valid_window: None,
            valid_dates: None,
            speaker: None,
            assumed_premise_ids: Vec::new(),
            quote: ModelQuoteAnchor {
                chunk_id: "r1".into(),
                verbatim_quote: "读书会禁止录音".into(),
                turn_ordinal: None,
                approx_char_offset: None,
            },
            write_op: ModelWriteOp::Add,
            evidence: None,
            command_guard: None,
        }],
        &opts_with_rej,
    )?;
    assert!(sem_blocked.created_fact_ids.is_empty());
    assert_eq!(sem_blocked.dropped_rejected.len(), 1);

    // Contrast with `kp forget` (`tms::withdraw` without `RejectionRecord`): future mention re-learns!
    let fresh_req = build_extraction_batch_request(
        &store,
        "src_forget".parse()?,
        bob.clone(),
        calendar,
        Vec::new(),
        vec![ExtractionChunkSlice {
            chunk_id: "f0".into(),
            heading: "note".into(),
            body: "周四下午暂停例会。".into(),
            source_id: "src_forget".parse()?,
            author: bob.clone(),
            base_anchor: Default::default(),
            role: Default::default(),
            reply_to_chunk_id: None,
        }],
        10,
    )?;
    let learned_1 = apply_extracted_atoms(
        &mut store,
        &fresh_req,
        &[ModelExtractedAtom {
            kind: FactKind::Observation,
            claim_text: "周四下午暂停例会".into(),
            value: json!("no_standup_thu"),
            scope: None,
            ambiguous_scope: None,
            condition_concept: None,
            action_concept: None,
            group_concept: None,
            member_mention: None,
            valid_window: None,
            valid_dates: None,
            speaker: None,
            assumed_premise_ids: Vec::new(),
            quote: ModelQuoteAnchor {
                chunk_id: "f0".into(),
                verbatim_quote: "周四下午暂停例会".into(),
                turn_ordinal: None,
                approx_char_offset: None,
            },
            write_op: ModelWriteOp::Add,
            evidence: None,
            command_guard: None,
        }],
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([bob.clone()])),
            EpochMs(t0.0 + 500),
        ),
    )?;
    kp_derive::tms::withdraw(
        &mut store,
        learned_1.created_fact_ids[0].as_str(),
        &bob,
        EpochMs(t0.0 + 600),
    )?;
    assert!(store.visible_facts(&bob, EpochMs(t0.0 + 600))?.is_empty());

    let relearned = apply_extracted_atoms(
        &mut store,
        &fresh_req,
        &[ModelExtractedAtom {
            kind: FactKind::Observation,
            claim_text: "周四下午暂停例会".into(),
            value: json!("no_standup_thu"),
            scope: None,
            ambiguous_scope: None,
            condition_concept: None,
            action_concept: None,
            group_concept: None,
            member_mention: None,
            valid_window: None,
            valid_dates: None,
            speaker: None,
            assumed_premise_ids: Vec::new(),
            quote: ModelQuoteAnchor {
                chunk_id: "f0".into(),
                verbatim_quote: "周四下午暂停例会".into(),
                turn_ordinal: None,
                approx_char_offset: None,
            },
            write_op: ModelWriteOp::Add,
            evidence: None,
            command_guard: None,
        }],
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([bob.clone()])),
            EpochMs(t0.0 + 700),
        ),
    )?;
    assert_eq!(relearned.created_fact_ids.len(), 1);
    assert_eq!(store.visible_facts(&bob, EpochMs(t0.0 + 800))?.len(), 1);
    Ok(())
}

#[allow(clippy::too_many_lines)]
#[test]
fn m1_security_narrow_boundary_alias_resolution_source_scope_and_assumed_premise_staleness(
) -> Result {
    let (_dir, mut store) = community_three_members()?;
    let alice: kp_model::ActorId = "alice".parse()?;
    let bob: kp_model::ActorId = "bob".parse()?;
    let t0 = EpochMs(1_795_089_600_000);
    let shanghai = kp_model::TimeZone::get("Asia/Shanghai")?;
    let calendar = CalendarTable::from_reference_in_tz(t0, &shanghai)?;

    // Trusted Alias fact: "搭档" -> "alice".
    store.insert_fact(&Fact {
        id: "alias_partner".parse()?,
        origin_library: store.library_id().clone(),
        logical_key: "alias_partner".parse()?,
        kind: FactKind::Alias,
        subject: "搭档".into(),
        predicate: "kp.core.alias".into(),
        object: json!("alice"),
        tags: BTreeSet::new(),
        author: bob.clone(),
        ingested_by: None,
        trust_tier: TrustTier::OwnerExplicit,
        access: FactAccess::Base {
            audience: AudienceSpec::Explicit(BTreeSet::from([alice.clone(), bob.clone()])),
            stamp: BitemporalStamp {
                valid: ValidityWindow {
                    valid_from: EpochMs(0),
                    valid_until: EpochMs::OPEN_END,
                },
                recorded_at: t0,
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
    })?;

    // Active architectural decision D1 ("审计日志存 Postgres").
    let dec_pg = Fact {
        id: "fact_dec_pg".parse()?,
        origin_library: store.library_id().clone(),
        logical_key: "fact_dec_pg".parse()?,
        kind: FactKind::Decision,
        subject: "audit.storage".into(),
        predicate: "kp.core.decision".into(),
        object: json!({"claim_text": "统一采用 Postgres 存储审计日志"}),
        tags: BTreeSet::new(),
        author: alice.clone(),
        ingested_by: None,
        trust_tier: TrustTier::HumanVerbatim,
        access: FactAccess::Base {
            audience: AudienceSpec::Explicit(BTreeSet::from([alice.clone(), bob.clone()])),
            stamp: BitemporalStamp {
                valid: ValidityWindow {
                    valid_from: t0,
                    valid_until: EpochMs::OPEN_END,
                },
                recorded_at: t0,
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
    };
    store.insert_fact(&dec_pg)?;

    let scope_id: SourceScopeId = "ext_chat.infra".parse()?;
    store.upsert_source_scope(&SourceScope {
        scope_id: scope_id.clone(),
        provider: "ext_chat".into(),
        external_container_id: "oc_infra".into(),
        label: "基础架构群".into(),
        current_audience: AudienceSpec::Explicit(BTreeSet::from([alice.clone(), bob.clone()])),
        synced_at: t0,
    })?;

    let body = "搭档下周值班提前交接。既然用 Postgres 存审计日志，就按月建分区表。";
    let req = build_extraction_batch_request(
        &store,
        "src_sec".parse()?,
        bob.clone(),
        calendar,
        Vec::new(),
        vec![ExtractionChunkSlice {
            chunk_id: "s0".into(),
            heading: "infra".into(),
            body: body.into(),
            source_id: "src_sec".parse()?,
            author: bob.clone(),
            base_anchor: Default::default(),
            role: Default::default(),
            reply_to_chunk_id: None,
        }],
        10,
    )?;

    // 1) Unregistered mention `"mallory_admin"` never smuggles a foreign ActorId: it downgrades
    //    `governed_scope` to `SelfOnly(bob)` and logs a diagnostic entry in `rejected_atoms`!
    let smuggle_actor = ModelExtractedAtom {
        kind: FactKind::Constraint,
        claim_text: "提前交接".into(),
        value: json!({}),
        scope: Some(ModelScopeRef::NamedMentions {
            mentions: vec!["mallory_admin".into()],
        }),
        ambiguous_scope: None,
        condition_concept: None,
        action_concept: None,
        group_concept: None,
        member_mention: None,
        valid_window: None,
        valid_dates: None,
        speaker: None,
        assumed_premise_ids: Vec::new(),
        quote: ModelQuoteAnchor {
            chunk_id: "s0".into(),
            verbatim_quote: "搭档下周值班提前交接".into(),
            turn_ordinal: None,
            approx_char_offset: None,
        },
        write_op: ModelWriteOp::Add,
        evidence: None,
        command_guard: None,
    };
    let smuggle_out = apply_extracted_atoms(
        &mut store,
        &req,
        &[smuggle_actor],
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([alice.clone(), bob.clone()])),
            t0,
        ),
    )?;
    assert_eq!(smuggle_out.rejected_atoms.len(), 1);
    let downgraded_id = &smuggle_out.created_fact_ids[0];
    assert_eq!(
        store.derivation_view()?.facts[downgraded_id].governed_scope,
        Some(GovernedScope::SelfOnly { actor: bob.clone() })
    );
    store.forget_fact_id(downgraded_id, &bob, EpochMs(t0.0 + 10))?;

    // 2) Valid alias `"搭档"` resolves to `"alice"` AND whitelisted `fact_dec_pg` creates
    // an `AssumedPremise` derived conclusion with `SourceOriginMeta` (`speaker != ingested_by`).
    let mut opts = NlFunnelOptions::personal_or_shared(
        AudienceSpec::Explicit(BTreeSet::from([alice.clone(), bob.clone()])),
        EpochMs(t0.0 + 100),
    );
    opts.source_origin = Some(SourceOriginMeta {
        speaker: bob.clone(),
        ingested_by: alice.clone(),
        source_scope_id: scope_id.clone(),
    });

    let valid_out = apply_extracted_atoms(
        &mut store,
        &req,
        &[
            ModelExtractedAtom {
                kind: FactKind::Constraint,
                claim_text: "下周值班提前交接".into(),
                value: json!({}),
                scope: Some(ModelScopeRef::NamedMentions {
                    mentions: vec!["搭档".into()],
                }),
                ambiguous_scope: None,
                condition_concept: None,
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: None,
                speaker: None,
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: "s0".into(),
                    verbatim_quote: "搭档下周值班提前交接".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
                evidence: None,
                command_guard: None,
            },
            ModelExtractedAtom {
                kind: FactKind::Observation,
                claim_text: "审计日志按月建分区表".into(),
                value: json!({"layout": "monthly_partitions"}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: None,
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: None,
                speaker: None,
                assumed_premise_ids: vec!["fact_dec_pg".parse()?],
                quote: ModelQuoteAnchor {
                    chunk_id: "s0".into(),
                    verbatim_quote: "既然用 Postgres 存审计日志，就按月建分区表".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
                evidence: None,
                command_guard: None,
            },
        ],
        &opts,
    )?;

    let resolved_constraint = &store.derivation_view()?.facts[&valid_out.created_fact_ids[0]];
    assert_eq!(
        resolved_constraint.governed_scope,
        Some(GovernedScope::Actors {
            actors: BTreeSet::from([alice.clone()]),
        })
    );
    assert_eq!(resolved_constraint.author, bob);
    assert_eq!(resolved_constraint.ingested_by, Some(alice.clone()));
    assert_eq!(resolved_constraint.source_scope_id, Some(scope_id.clone()));

    let assumed_id = &valid_out.derived_assumed_fact_ids[0];
    assert!(store
        .visible_facts(&bob, EpochMs(t0.0 + 200))?
        .iter()
        .any(|f| &f.id == assumed_id));

    // Leaving `SourceScope` hides BOTH base fact and `AssumedPremise` derived conclusion at read time!
    store.upsert_source_scope(&SourceScope {
        scope_id: scope_id.clone(),
        provider: "ext_chat".into(),
        external_container_id: "oc_infra".into(),
        label: "基础架构群".into(),
        current_audience: AudienceSpec::Explicit(BTreeSet::from([alice.clone()])),
        synced_at: EpochMs(t0.0 + 250),
    })?;
    assert!(!store
        .visible_facts(&bob, EpochMs(t0.0 + 300))?
        .iter()
        .any(|f| &f.id == assumed_id || f.id == valid_out.created_fact_ids[1]));

    // Restore Bob to `SourceScope`, then supersede `fact_dec_pg` -> `AssumedPremise` becomes `StalePremise`!
    store.upsert_source_scope(&SourceScope {
        scope_id,
        provider: "ext_chat".into(),
        external_container_id: "oc_infra".into(),
        label: "基础架构群".into(),
        current_audience: AudienceSpec::Explicit(BTreeSet::from([alice.clone(), bob.clone()])),
        synced_at: EpochMs(t0.0 + 350),
    })?;
    let mut dec_ch = dec_pg.clone();
    dec_ch.id = "fact_dec_clickhouse".parse()?;
    dec_ch.logical_key = "fact_dec_clickhouse".parse()?;
    dec_ch.object = json!({"claim_text": "迁移至 ClickHouse 列式存储"});
    store.supersede(&dec_pg.id, &dec_ch)?;

    let graph_after = store.derivation_view()?;
    assert_eq!(
        graph_after.facts[assumed_id].status,
        FactStatus::StalePremise
    );
    assert_eq!(
        graph_after.facts[assumed_id]
            .source_anchor
            .as_ref()
            .and_then(|a| a.verbatim_quote.as_deref()),
        Some("既然用 Postgres 存审计日志，就按月建分区表")
    );

    // RawOnly policy skips extraction without touching Store.
    let mut raw_opts = opts;
    raw_opts.route_policy = ModelRoutePolicy::RawOnly;
    let raw_out = tokio::runtime::Runtime::new()?.block_on(extract_and_ingest_batch(
        &mut store,
        &DeterministicMockLlm::default(),
        &req,
        &raw_opts,
    ))?;
    assert_eq!(raw_out.status, NlBatchStatus::SkippedRawOnlyPolicy);

    // 3) Speaker-owned `SupersedeOwn` succeeds for Bob's own fact and fails if Bob targets Alice's fact.
    let update_body = "更正一下：下周值班延后一天交接。这条已经记过了。";
    let update_req = build_extraction_batch_request(
        &store,
        "src_upd".parse()?,
        bob.clone(),
        CalendarTable::from_reference_in_tz(
            EpochMs(t0.0 + 400),
            &kp_model::TimeZone::get("Asia/Shanghai")?,
        )?,
        Vec::new(),
        vec![ExtractionChunkSlice {
            chunk_id: "u0".into(),
            heading: "upd".into(),
            body: update_body.into(),
            source_id: "src_upd".parse()?,
            author: bob.clone(),
            base_anchor: Default::default(),
            role: Default::default(),
            reply_to_chunk_id: None,
        }],
        10,
    )?;
    assert!(update_req
        .speaker_own_facts
        .iter()
        .any(|f| f.fact_id == valid_out.created_fact_ids[0]));
    assert!(!update_req
        .speaker_own_facts
        .iter()
        .any(|f| f.fact_id == dec_ch.id));

    let bad_foreign_supersede = ModelExtractedAtom {
        kind: FactKind::Constraint,
        claim_text: "下周值班延后一天交接".into(),
        value: json!({}),
        scope: Some(ModelScopeRef::SpeakerSelf),
        ambiguous_scope: None,
        condition_concept: None,
        action_concept: None,
        group_concept: None,
        member_mention: None,
        valid_window: None,
        valid_dates: None,
        speaker: None,
        assumed_premise_ids: Vec::new(),
        quote: ModelQuoteAnchor {
            chunk_id: "u0".into(),
            verbatim_quote: "下周值班延后一天交接".into(),
            turn_ordinal: None,
            approx_char_offset: None,
        },
        write_op: ModelWriteOp::SupersedeOwn {
            target_fact_id: dec_ch.id,
        },
        evidence: None,
        command_guard: None,
    };
    assert!(apply_extracted_atoms(
        &mut store,
        &update_req,
        &[bad_foreign_supersede],
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([alice.clone(), bob.clone()])),
            EpochMs(t0.0 + 450),
        ),
    )
    .is_err());

    let upd_out = apply_extracted_atoms(
        &mut store,
        &update_req,
        &[
            ModelExtractedAtom {
                kind: FactKind::Constraint,
                claim_text: "下周值班延后一天交接".into(),
                value: json!({"delay_days": 1}),
                scope: Some(ModelScopeRef::SpeakerSelf),
                ambiguous_scope: None,
                condition_concept: None,
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: None,
                speaker: None,
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: "u0".into(),
                    verbatim_quote: "下周值班延后一天交接".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::SupersedeOwn {
                    target_fact_id: valid_out.created_fact_ids[0].clone(),
                },
                evidence: None,
                command_guard: None,
            },
            ModelExtractedAtom {
                kind: FactKind::Observation,
                claim_text: "这条已经记过了".into(),
                value: json!({}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: None,
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: None,
                speaker: None,
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: "u0".into(),
                    verbatim_quote: "这条已经记过了".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::None,
                evidence: None,
                command_guard: None,
            },
        ],
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([alice, bob])),
            EpochMs(t0.0 + 500),
        ),
    )?;
    assert_eq!(
        upd_out.superseded_old_fact_ids,
        vec![valid_out.created_fact_ids[0].clone()]
    );
    assert_eq!(upd_out.skipped_noop, 1);
    assert_eq!(
        store.derivation_view()?.facts[&valid_out.created_fact_ids[0]].status,
        FactStatus::Superseded
    );
    Ok(())
}

fn plain_base_fact(
    id: &str,
    kind: FactKind,
    subject: &str,
    predicate: &str,
    object: serde_json::Value,
    author: &str,
    audience: &[&str],
) -> Result<Fact> {
    Ok(Fact {
        id: id.parse()?,
        origin_library: "club-hub".parse()?,
        logical_key: id.parse()?,
        kind,
        subject: subject.into(),
        predicate: predicate.into(),
        object,
        tags: BTreeSet::new(),
        author: author.parse()?,
        ingested_by: None,
        trust_tier: TrustTier::OwnerExplicit,
        access: FactAccess::Base {
            audience: AudienceSpec::Explicit(
                audience
                    .iter()
                    .map(|a| a.parse())
                    .collect::<kp_model::Result<_>>()?,
            ),
            stamp: BitemporalStamp {
                valid: ValidityWindow {
                    valid_from: EpochMs(0),
                    valid_until: EpochMs::OPEN_END,
                },
                recorded_at: EpochMs(1_795_000_000_000),
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

fn named_mention_atom(chunk_id: &str, quote: &str, mention: &str) -> ModelExtractedAtom {
    ModelExtractedAtom {
        kind: FactKind::Constraint,
        claim_text: quote.into(),
        value: json!({}),
        scope: Some(ModelScopeRef::NamedMentions {
            mentions: vec![mention.into()],
        }),
        ambiguous_scope: None,
        condition_concept: None,
        action_concept: None,
        group_concept: None,
        member_mention: None,
        valid_window: None,
        valid_dates: None,
        speaker: None,
        assumed_premise_ids: Vec::new(),
        quote: ModelQuoteAnchor {
            chunk_id: chunk_id.into(),
            verbatim_quote: quote.into(),
            turn_ordinal: None,
            approx_char_offset: None,
        },
        write_op: ModelWriteOp::Add,
        evidence: None,
        command_guard: None,
    }
}

#[test]
fn m1_privacy_private_aliases_and_conflicts_neither_resolve_nor_block_other_speakers() -> Result {
    let (_dir, mut store) = community_three_members()?;
    let alice: kp_model::ActorId = "alice".parse()?;
    let bob: kp_model::ActorId = "bob".parse()?;
    let t0 = EpochMs(1_795_089_600_000);

    let private_alias = plain_base_fact(
        "alias_lead_private",
        FactKind::Alias,
        "组长",
        "kp.core.alias",
        json!("alice"),
        "alice",
        &["alice"],
    )?;
    store.insert_fact(&private_alias)?;
    store.insert_fact(&plain_base_fact(
        "alias_partner_shared",
        FactKind::Alias,
        "搭档",
        "kp.core.alias",
        json!("alice"),
        "bob",
        &["alice", "bob"],
    )?)?;
    store.insert_fact(&plain_base_fact(
        "carol_private_conflict",
        FactKind::Observation,
        "搭档",
        "kp.core.alias_conflict",
        json!("alice"),
        "carol",
        &["carol"],
    )?)?;

    let shanghai = kp_model::TimeZone::get("Asia/Shanghai")?;
    let calendar = CalendarTable::from_reference_in_tz(t0, &shanghai)?;
    let req = build_extraction_batch_request(
        &store,
        "src_alias_priv".parse()?,
        bob.clone(),
        calendar,
        Vec::new(),
        vec![ExtractionChunkSlice {
            chunk_id: "p0".into(),
            heading: "chat".into(),
            body: "组长周五核对账单。搭档周五也核对账单。".into(),
            source_id: "src_alias_priv".parse()?,
            author: bob.clone(),
            base_anchor: Default::default(),
            role: Default::default(),
            reply_to_chunk_id: None,
        }],
        10,
    )?;
    let opts = NlFunnelOptions::personal_or_shared(
        AudienceSpec::Explicit(BTreeSet::from([alice.clone(), bob.clone()])),
        t0,
    );

    let priv_mention_out = apply_extracted_atoms(
        &mut store,
        &req,
        &[named_mention_atom("p0", "组长周五核对账单", "组长")],
        &opts,
    )?;
    assert_eq!(priv_mention_out.rejected_atoms.len(), 1);
    assert_eq!(
        store.derivation_view()?.facts[&priv_mention_out.created_fact_ids[0]].governed_scope,
        Some(GovernedScope::SelfOnly { actor: bob.clone() }),
        "Alice's private alias must never resolve for Bob; unresolved mention downgrades to SelfOnly(bob)"
    );

    let out = apply_extracted_atoms(
        &mut store,
        &req,
        &[named_mention_atom("p0", "搭档周五也核对账单", "搭档")],
        &opts,
    )?;
    assert_eq!(
        store.derivation_view()?.facts[&out.created_fact_ids[0]].governed_scope,
        Some(GovernedScope::Actors {
            actors: BTreeSet::from([alice.clone()]),
        })
    );

    let graph = store.derivation_view()?;
    let alice_visible = graph.evaluate(&alice, t0).facts;
    assert_eq!(
        kp_derive::resolve_actor_mention("组长", &graph, &alice_visible)?,
        alice
    );
    let bob_visible = graph.evaluate(&bob, t0).facts;
    assert!(kp_derive::resolve_actor_mention("组长", &graph, &bob_visible).is_err());
    Ok(())
}

fn schedule_event_atom(
    chunk_id: &str,
    quote: &str,
    label: &str,
    date_iso: &str,
) -> ModelExtractedAtom {
    ModelExtractedAtom {
        kind: FactKind::Event,
        claim_text: quote.into(),
        value: json!({}),
        scope: None,
        ambiguous_scope: None,
        condition_concept: Some(ModelConceptRef::ProposeNew {
            kind: ConceptKind::Condition,
            label: label.into(),
            domain: "facility".into(),
            polarity: ConceptPolarity::Negative,
        }),
        action_concept: None,
        group_concept: None,
        member_mention: None,
        valid_window: None,
        valid_dates: Some(ModelDateRange {
            start_date_iso: date_iso.into(),
            end_date_iso: None,
            open_ended: false,
        }),
        speaker: None,
        assumed_premise_ids: Vec::new(),
        quote: ModelQuoteAnchor {
            chunk_id: chunk_id.into(),
            verbatim_quote: quote.into(),
            turn_ordinal: None,
            approx_char_offset: None,
        },
        write_op: ModelWriteOp::Add,
        evidence: None,
        command_guard: None,
    }
}

#[tokio::test]
async fn m1_privacy_outsider_contradiction_never_supersedes_private_event() -> Result {
    let (_dir, mut store) = community_three_members()?;
    let alice: kp_model::ActorId = "alice".parse()?;
    let carol: kp_model::ActorId = "carol".parse()?;
    let t0 = EpochMs(1_795_089_600_000);
    let shanghai = kp_model::TimeZone::get("Asia/Shanghai")?;
    let calendar = CalendarTable::from_reference_in_tz(t0, &shanghai)?;

    let mock_llm = DeterministicMockLlm::default();
    mock_llm.register_entailment(
        "自习室恢复开放",
        "自习室停电关闭",
        EntailmentVerdict::Contradicts,
    )?;

    let a_req = build_extraction_batch_request(
        &store,
        "src_a_priv".parse()?,
        alice.clone(),
        calendar.clone(),
        Vec::new(),
        vec![ExtractionChunkSlice {
            chunk_id: "a0".into(),
            heading: "notes".into(),
            body: "周五自习室停电关闭".into(),
            source_id: "src_a_priv".parse()?,
            author: alice.clone(),
            base_anchor: Default::default(),
            role: Default::default(),
            reply_to_chunk_id: None,
        }],
        10,
    )?;
    let a_out = apply_extracted_atoms(
        &mut store,
        &a_req,
        &[schedule_event_atom(
            "a0",
            "周五自习室停电关闭",
            "自习室停电关闭",
            "2026-11-20",
        )],
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([alice.clone()])),
            t0,
        ),
    )?;
    let alice_event = a_out.created_fact_ids[0].clone();

    let c_req = build_extraction_batch_request(
        &store,
        "src_c_pub".parse()?,
        carol.clone(),
        calendar.clone(),
        Vec::new(),
        vec![ExtractionChunkSlice {
            chunk_id: "c0".into(),
            heading: "public".into(),
            body: "周五自习室恢复开放".into(),
            source_id: "src_c_pub".parse()?,
            author: carol.clone(),
            base_anchor: Default::default(),
            role: Default::default(),
            reply_to_chunk_id: None,
        }],
        10,
    )?;
    let c_out = apply_extracted_atoms(
        &mut store,
        &c_req,
        &[schedule_event_atom(
            "c0",
            "周五自习室恢复开放",
            "自习室恢复开放",
            "2026-11-20",
        )],
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([
                alice.clone(),
                "bob".parse()?,
                carol.clone(),
            ])),
            EpochMs(t0.0 + 100),
        ),
    )?;

    let mut concepts = a_out.active_concepts.clone();
    concepts.extend(c_out.created_concepts.clone());
    let superseded = kp_derive::reconcile_contradicted_events(
        &mut store,
        &mock_llm,
        &concepts,
        EpochMs(t0.0 + 200),
    )
    .await?;
    assert!(superseded.is_empty());
    assert_eq!(
        store.derivation_view()?.facts[&alice_event].status,
        FactStatus::Active
    );

    // Positive control: Alice's own later update DOES supersede her private event.
    let a2_req = build_extraction_batch_request(
        &store,
        "src_a_reopen".parse()?,
        alice.clone(),
        calendar,
        concepts,
        vec![ExtractionChunkSlice {
            chunk_id: "a1".into(),
            heading: "notes".into(),
            body: "更新：周五自习室恢复开放".into(),
            source_id: "src_a_reopen".parse()?,
            author: alice.clone(),
            base_anchor: Default::default(),
            role: Default::default(),
            reply_to_chunk_id: None,
        }],
        10,
    )?;
    let a2_out = apply_extracted_atoms(
        &mut store,
        &a2_req,
        &[schedule_event_atom(
            "a1",
            "周五自习室恢复开放",
            "自习室恢复开放",
            "2026-11-20",
        )],
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([alice])),
            EpochMs(t0.0 + 300),
        ),
    )?;
    let superseded = kp_derive::reconcile_contradicted_events(
        &mut store,
        &mock_llm,
        &a2_out.active_concepts,
        EpochMs(t0.0 + 400),
    )
    .await?;
    assert_eq!(superseded, vec![alice_event.clone()]);
    assert_eq!(
        store.derivation_view()?.facts[&alice_event].status,
        FactStatus::Superseded
    );
    Ok(())
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn m1_multi_speaker_window_batch_blocks_assistant_quotes_and_attributes_per_turn() -> Result {
    let (_dir, mut store) = community_three_members()?;
    let alice: kp_model::ActorId = "alice".parse()?;
    let bob: kp_model::ActorId = "bob".parse()?;
    let carol: kp_model::ActorId = "carol".parse()?;
    let t0 = EpochMs(1_795_089_600_000);
    let shanghai = kp_model::TimeZone::get("Asia/Shanghai")?;
    let calendar = CalendarTable::from_reference_in_tz(t0, &shanghai)?;
    let fri_win = calendar
        .window_for_date("2026-11-20")
        .ok_or("missing fri")?;
    let next_wed_win = calendar
        .window_for_date("2026-11-25")
        .ok_or("missing next wed")?;

    // 1) Tagged chunk with ONLY assistant prose + human acks is skipped with zero CLI spawns.
    let ack_only_chunk = format_tagged_turns(&[
        ChunkTurn {
            speaker: None,
            is_assistant: true,
            text: "我已经整理好了差旅报销新模板，请大家查收。".into(),
        },
        ChunkTurn {
            speaker: Some(alice.clone()),
            is_assistant: false,
            text: "好的！".into(),
        },
        ChunkTurn {
            speaker: Some(bob.clone()),
            is_assistant: false,
            text: "收到".into(),
        },
    ]);
    let ack_req = build_extraction_batch_request(
        &store,
        "src_ack_only".parse()?,
        alice.clone(),
        calendar.clone(),
        Vec::new(),
        vec![ExtractionChunkSlice {
            chunk_id: "ack0".into(),
            heading: "chat".into(),
            body: ack_only_chunk,
            source_id: "src_ack_only".parse()?,
            author: alice.clone(),
            base_anchor: Default::default(),
            role: Default::default(),
            reply_to_chunk_id: None,
        }],
        10,
    )?;
    let runner = HostCliRunner::with_transport(
        HostCliConfig::codex_default(),
        RecordingMockTransport::new(|_| {
            Err(kp_model::KpError::invalid(
                "test",
                "ack-only tagged chunk must not spawn CLI",
            ))
        }),
    )?;
    assert_eq!(
        runner.extract_batch(&ack_req).await?,
        ExtractionBatchOutcome::SkippedEmpty
    );

    // 2) Multi-speaker chunk mixing `[speaker:alice]`, `[assistant]`, `[speaker:bob]`, `[speaker:carol]`.
    let group_body = format_tagged_turns(&[
        ChunkTurn {
            speaker: Some(alice.clone()),
            is_assistant: false,
            text: "明天下午三点读书会改到二楼露台举行。".into(),
        },
        ChunkTurn {
            speaker: None,
            is_assistant: true,
            text: "建议顺便规定所有报销单必须在三天内提交财务。".into(),
        },
        ChunkTurn {
            speaker: Some(bob.clone()),
            is_assistant: false,
            text: "下周三起差旅住宿报销上限调整为每晚五百元。".into(),
        },
        ChunkTurn {
            speaker: Some(carol.clone()),
            is_assistant: false,
            text: "收到！".into(),
        },
    ]);
    let batch_req = build_extraction_batch_request(
        &store,
        "src_multi_1".parse()?,
        alice.clone(),
        calendar,
        Vec::new(),
        vec![ExtractionChunkSlice {
            chunk_id: "m0".into(),
            heading: "group-chat".into(),
            body: group_body,
            source_id: "src_multi_1".parse()?,
            author: alice.clone(),
            base_anchor: Default::default(),
            role: Default::default(),
            reply_to_chunk_id: None,
        }],
        10,
    )?;

    // Attempting to extract from the `[assistant]` line fails closed!
    let assistant_leak_atom = ModelExtractedAtom {
        kind: FactKind::Constraint,
        claim_text: "所有报销单三天内提交财务".into(),
        value: json!({}),
        scope: Some(ModelScopeRef::SpeakerSelf),
        ambiguous_scope: None,
        condition_concept: None,
        action_concept: None,
        group_concept: None,
        member_mention: None,
        valid_window: None,
        valid_dates: None,
        speaker: Some(bob.clone()),
        assumed_premise_ids: Vec::new(),
        quote: ModelQuoteAnchor {
            chunk_id: "m0".into(),
            verbatim_quote: "建议顺便规定所有报销单必须在三天内提交财务".into(),
            turn_ordinal: None,
            approx_char_offset: None,
        },
        write_op: ModelWriteOp::Add,
        evidence: None,
        command_guard: None,
    };
    assert!(apply_extracted_atoms(
        &mut store,
        &batch_req,
        &[assistant_leak_atom],
        &NlFunnelOptions::personal_or_shared(AudienceSpec::AllMembers, t0),
    )
    .is_err());

    // Valid multi-speaker extraction attributes Alice's turn to `alice` and Bob's turn to `bob`.
    let out = apply_extracted_atoms(
        &mut store,
        &batch_req,
        &[
            ModelExtractedAtom {
                kind: FactKind::Event,
                claim_text: "明天下午读书会改到二楼露台".into(),
                value: json!({"venue": "2F_terrace"}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "读书会换到二楼露台".into(),
                    domain: "club".into(),
                    polarity: ConceptPolarity::Neutral,
                }),
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: Some(ModelDateRange {
                    start_date_iso: "2026-11-20".into(),
                    end_date_iso: None,
                    open_ended: false,
                }),
                speaker: Some(alice.clone()),
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: "m0".into(),
                    verbatim_quote: "明天下午三点读书会改到二楼露台举行".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
                evidence: None,
                command_guard: None,
            },
            ModelExtractedAtom {
                kind: FactKind::Constraint,
                claim_text: "差旅住宿报销上限每晚五百元".into(),
                value: json!({"hotel_cap_cny": 500}),
                scope: Some(ModelScopeRef::SpeakerSelf),
                ambiguous_scope: None,
                condition_concept: None,
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: Some(ModelDateRange {
                    start_date_iso: "2026-11-25".into(),
                    end_date_iso: None,
                    open_ended: true,
                }),
                speaker: Some(bob.clone()),
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: "m0".into(),
                    verbatim_quote: "下周三起差旅住宿报销上限调整为每晚五百元".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
                evidence: None,
                command_guard: None,
            },
        ],
        &NlFunnelOptions::personal_or_shared(AudienceSpec::AllMembers, t0),
    )?;
    assert_eq!(out.created_fact_ids.len(), 2);
    let graph = store.derivation_view()?;
    let alice_fact = &graph.facts[&out.created_fact_ids[0]];
    let bob_fact = &graph.facts[&out.created_fact_ids[1]];
    assert_eq!(alice_fact.author, alice);
    assert_eq!(bob_fact.author, bob);
    assert_eq!(
        bob_fact.governed_scope,
        Some(GovernedScope::SelfOnly { actor: bob })
    );
    if let FactAccess::Base { stamp, .. } = &alice_fact.access {
        assert_eq!(stamp.valid, fri_win);
    } else {
        return Err("expected base access".into());
    }
    if let FactAccess::Base { stamp, .. } = &bob_fact.access {
        assert_eq!(
            stamp.valid,
            ValidityWindow {
                valid_from: next_wed_win.valid_from,
                valid_until: EpochMs::OPEN_END,
            }
        );
    } else {
        return Err("expected base access".into());
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn m1_two_personal_libraries_sunday_upcoming_friday_habitual_rule_and_quote_aware_alignment(
) -> Result {
    let shanghai = kp_model::TimeZone::get("Asia/Shanghai")?;
    // Sunday 2026-11-22 10:00:00 +08:00 (`weekday_iso == 7`).
    let sunday_at = kp_model::parse_timestamp_ms_in_tz("2026-11-22T10:00:00+08:00", &shanghai)
        .ok_or("parse sunday")?;
    let calendar = CalendarTable::from_reference_in_tz(sunday_at, &shanghai)?;
    assert_eq!(calendar.reference_date.weekday_iso, 7);

    let upcoming_fri_win = calendar.day_window_for_token("2026-11-27")?;
    let past_fri_noon =
        EpochMs(calendar.day_window_for_token("2026-11-20")?.valid_from.0 + 12 * 3_600_000);
    let upcoming_fri_noon = EpochMs(upcoming_fri_win.valid_from.0 + 12 * 3_600_000);

    let alice: kp_model::ActorId = "alice".parse()?;
    let bob: kp_model::ActorId = "bob".parse()?;

    let alice_dir = tempfile::tempdir()?;
    let mut alice_store = Store::init(
        alice_dir.path(),
        InitOptions {
            library_id: "alice-personal".parse()?,
            name: "Alice Personal".into(),
            mode: LibraryMode::Personal,
            owner: alice.clone(),
            created_at: EpochMs(sunday_at.0 - 10_000),
        },
    )?;

    let bob_dir = tempfile::tempdir()?;
    let mut bob_store = Store::init(
        bob_dir.path(),
        InitOptions {
            library_id: "bob-personal".parse()?,
            name: "Bob Personal".into(),
            mode: LibraryMode::Personal,
            owner: bob.clone(),
            created_at: EpochMs(sunday_at.0 - 10_000),
        },
    )?;
    // Bob's personal library maps "Alice" -> `alice` via a trusted local alias.
    bob_store.insert_fact(&Fact {
        id: "alias_alice_in_bob".parse()?,
        origin_library: bob_store.library_id().clone(),
        logical_key: "alias_alice_in_bob".parse()?,
        kind: FactKind::Alias,
        subject: "Alice".into(),
        predicate: "kp.core.alias".into(),
        object: json!("alice"),
        tags: BTreeSet::new(),
        author: bob.clone(),
        ingested_by: None,
        trust_tier: TrustTier::OwnerExplicit,
        access: FactAccess::Base {
            audience: AudienceSpec::Explicit(BTreeSet::from([bob.clone()])),
            stamp: BitemporalStamp {
                valid: ValidityWindow {
                    valid_from: EpochMs(0),
                    valid_until: EpochMs::OPEN_END,
                },
                recorded_at: EpochMs(sunday_at.0 - 5_000),
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
    })?;

    // (b) Event speaker (`alice`) extracted FIRST when `active_concepts` is completely empty,
    //     spoken on Sunday saying `"本周五……"`, with the model mistakenly emitting past Friday
    //     `"2026-11-20"`. Rust `CalendarTable` overrides `valid_window` to upcoming Friday `+5d`
    //     (`2026-11-27`)! Alice's event stays `{alice}` local in `alice_store`.
    let alice_quote = "本周五下午园区变电站检修停电。";
    let alice_req = build_extraction_batch_request(
        &alice_store,
        "src_alice_sun".parse()?,
        alice.clone(),
        calendar.clone(),
        Vec::new(),
        vec![ExtractionChunkSlice {
            chunk_id: "a_sun".into(),
            heading: "session".into(),
            body: alice_quote.into(),
            source_id: "src_alice_sun".parse()?,
            author: alice.clone(),
            base_anchor: Default::default(),
            role: Default::default(),
            reply_to_chunk_id: None,
        }],
        10,
    )?;
    assert!(alice_req.active_concepts.is_empty());

    let alice_out = apply_extracted_atoms(
        &mut alice_store,
        &alice_req,
        &[ModelExtractedAtom {
            kind: FactKind::Event,
            claim_text: "本周五下午园区变电站检修停电".into(),
            value: json!({}),
            scope: None,
            ambiguous_scope: None,
            condition_concept: Some(ModelConceptRef::ProposeNew {
                kind: ConceptKind::Condition,
                label: "变电站停电".into(),
                domain: "power".into(),
                polarity: ConceptPolarity::Negative,
            }),
            action_concept: None,
            group_concept: None,
            member_mention: None,
            valid_window: None,
            // Deliberately wrong past Friday emitted by model; deterministic weekday rule must override!
            valid_dates: Some(ModelDateRange {
                start_date_iso: "2026-11-20".into(),
                end_date_iso: None,
                open_ended: false,
            }),
            speaker: None,
            assumed_premise_ids: Vec::new(),
            quote: ModelQuoteAnchor {
                chunk_id: "a_sun".into(),
                verbatim_quote: alice_quote.into(),
                turn_ordinal: None,
                approx_char_offset: None,
            },
            write_op: ModelWriteOp::Add,
            evidence: None,
            command_guard: None,
        }],
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([alice.clone()])),
            sunday_at,
        ),
    )?;
    assert_eq!(alice_out.created_fact_ids.len(), 1);
    let stored_event = &alice_store.derivation_view()?.facts[&alice_out.created_fact_ids[0]];
    if let FactAccess::Base { stamp, .. } = &stored_event.access {
        assert_eq!(
            stamp.valid, upcoming_fri_win,
            "Sunday '本周五' must land on upcoming Friday 2026-11-27 (+5d), overriding model's 2026-11-20"
        );
    } else {
        return Err("expected base event".into());
    }

    // (a) Bob in his personal library states membership + habitual group practice
    //     `"遇到……都……"`, which the model mislabels as `kind: FactKind::Observation`.
    //     Structural narrowing coerces it to `FactKind::Constraint`!
    let bob_rule_quote = "我们硬件实验室遇到断电维护的时候都关闭精密仪器电源。";
    let bob_body = format!("Alice 是我们硬件实验室成员。{bob_rule_quote}");
    let bob_req = build_extraction_batch_request(
        &bob_store,
        "src_bob_sun".parse()?,
        bob.clone(),
        calendar,
        Vec::new(),
        vec![ExtractionChunkSlice {
            chunk_id: "b_sun".into(),
            heading: "session".into(),
            body: bob_body,
            source_id: "src_bob_sun".parse()?,
            author: bob.clone(),
            base_anchor: Default::default(),
            role: Default::default(),
            reply_to_chunk_id: None,
        }],
        10,
    )?;
    let bob_out = apply_extracted_atoms(
        &mut bob_store,
        &bob_req,
        &[
            ModelExtractedAtom {
                kind: FactKind::Membership,
                claim_text: "Alice 是硬件实验室成员".into(),
                value: json!({}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: None,
                action_concept: None,
                group_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Group,
                    label: "硬件实验室".into(),
                    domain: "lab".into(),
                    polarity: ConceptPolarity::Neutral,
                }),
                member_mention: Some("Alice".into()),
                valid_window: None,
                valid_dates: None,
                speaker: None,
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: "b_sun".into(),
                    verbatim_quote: "Alice 是我们硬件实验室成员。".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
                evidence: None,
                command_guard: None,
            },
            ModelExtractedAtom {
                // Model mislabels habitual group norm as `"observation"` -> coerced to `Constraint`!
                kind: FactKind::Observation,
                claim_text: "硬件实验室遇到断电维护都关闭精密仪器电源".into(),
                value: json!({}),
                scope: Some(ModelScopeRef::RelationGroup {
                    group_concept: ModelConceptRef::ProposeNew {
                        kind: ConceptKind::Group,
                        label: "硬件实验室".into(),
                        domain: "lab".into(),
                        polarity: ConceptPolarity::Neutral,
                    },
                }),
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "出现断电维护的时候".into(),
                    domain: "lab".into(),
                    polarity: ConceptPolarity::Negative,
                }),
                action_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Action,
                    label: "关闭精密仪器电源".into(),
                    domain: "lab".into(),
                    polarity: ConceptPolarity::Positive,
                }),
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: None,
                speaker: None,
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: "b_sun".into(),
                    verbatim_quote: bob_rule_quote.into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
                evidence: None,
                command_guard: None,
            },
        ],
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([alice.clone(), bob.clone()])),
            EpochMs(sunday_at.0 + 100),
        ),
    )?;
    assert_eq!(bob_out.created_fact_ids.len(), 2);
    assert_eq!(
        bob_store.derivation_view()?.facts[&bob_out.created_fact_ids[1]].kind,
        FactKind::Constraint
    );
    // Bob's library NEVER receives Alice's private local event!
    assert!(!bob_store
        .derivation_view()?
        .facts
        .values()
        .any(|f| f.kind == FactKind::Event));

    // (c) Sync Bob's shared membership + habitual rule + concepts into Alice's personal library.
    let (bob_snap, bob_concepts, bob_scopes) = bob_store.change_snapshot_with_governance_since(
        &alice,
        EpochMs(sunday_at.0 + 200),
        None,
    )?;
    assert_eq!(bob_snap.facts.len(), 2);
    assert_eq!(bob_concepts.len(), 3);
    alice_store.import_snapshot_with_governance(
        "https://hub.example",
        &bob_snap,
        &bob_concepts,
        &bob_scopes,
        None,
        EpochMs(sunday_at.0 + 300),
    )?;

    // First, suppose an earlier alignment pass without matching quotes returned `Unrelated`
    // (stored with its `quotes_sha256`). When re-evaluated with the real quotes, `judge_pair_cached`
    // MUST NOT stay stuck on cached `Unrelated` — it re-judges with quotes and upgrades to `Entails`!
    let unquoted_llm = DeterministicMockLlm::default();
    // Seed a stale `unrelated` alignment under a different quote hash first:
    let ev_concept = &alice_out.created_concepts[0];
    let rule_cond_concept = bob_concepts
        .iter()
        .find(|c| c.canonical_label == "出现断电维护的时候")
        .ok_or("missing rule condition concept")?;
    alice_store.insert_fact(&Fact {
        id: "align-stale-unrelated".parse()?,
        origin_library: alice_store.library_id().clone(),
        logical_key: "align-stale-unrelated".parse()?,
        kind: FactKind::Observation,
        subject: ev_concept.id.to_string(),
        predicate: kp_derive::CONCEPT_ALIGNMENT_PREDICATE.into(),
        object: json!({
            "left_concept_id": ev_concept.id,
            "right_concept_id": rule_cond_concept.id,
            "verdict": "unrelated",
            "quotes_sha256": "0000000000000000000000000000000000000000000000000000000000000000",
        }),
        tags: BTreeSet::new(),
        author: alice.clone(),
        ingested_by: None,
        trust_tier: TrustTier::ToolOrModelVerified,
        access: FactAccess::Base {
            audience: AudienceSpec::Explicit(BTreeSet::new()),
            stamp: BitemporalStamp {
                valid: ValidityWindow {
                    valid_from: EpochMs(0),
                    valid_until: EpochMs::OPEN_END,
                },
                recorded_at: EpochMs(sunday_at.0 + 310),
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
    })?;
    let _ = unquoted_llm;

    // Quote-aware LLM: returns `Unrelated` on bare labels, `Entails` ONLY when BOTH human verbatim quotes are supplied!
    let quote_aware_llm = DeterministicMockLlm::default();
    quote_aware_llm.register_entailment_with_quotes(
        "变电站停电",
        "出现断电维护的时候",
        Some(alice_quote.into()),
        Some(bob_rule_quote.into()),
        EntailmentVerdict::Entails,
    )?;

    let col_out = align_concepts_and_derive(
        &mut alice_store,
        &quote_aware_llm,
        &[],
        &[],
        EpochMs(sunday_at.0 + 400),
    )
    .await?;
    assert!(col_out.derived_warnings >= 1);

    let recorded_queries = quote_aware_llm.recorded_entailment_queries()?;
    assert!(recorded_queries.iter().any(|q| {
        q.left_label == "变电站停电"
            && q.right_label == "出现断电维护的时候"
            && q.left_quote.as_deref() == Some(alice_quote)
            && q.right_quote.as_deref() == Some(bob_rule_quote)
    }));

    // Past Friday (`2026-11-20`, `-2d`) has ZERO warnings; upcoming Friday (`2026-11-27`, `+5d`)
    // surfaces `CollisionWarning` for `alice`!
    assert!(!alice_store
        .visible_facts(&alice, past_fri_noon)?
        .iter()
        .any(|f| f.kind == FactKind::CollisionWarning));
    let alice_warnings: Vec<_> = alice_store
        .visible_facts(&alice, upcoming_fri_noon)?
        .into_iter()
        .filter(|f| f.kind == FactKind::CollisionWarning && f.subject == "alice")
        .collect();
    assert_eq!(alice_warnings.len(), 1);
    assert!(alice_warnings[0].object["advice"]
        .as_str()
        .is_some_and(|s| s.contains("关闭精密仪器电源")));
    verify_hard_rules_match_full_recompute(&alice_store)?;
    Ok(())
}
