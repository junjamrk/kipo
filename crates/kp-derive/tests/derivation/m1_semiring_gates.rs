use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use kp_derive::why::why;
use kp_derive::{
    align_concepts_and_derive, apply_extracted_atoms, build_extraction_batch_request, derive,
    record_manual_concept_alignment, reject_fact_and_record, ConceptAlignmentTable, HostCliConfig,
    HostCliRunner, NlFunnelOptions, PolicyRoutedLlmClient, RecordingMockTransport,
    SubprocessRawOutput, CONCEPT_ALIGNMENT_PREDICATE,
};
use kp_model::{
    ActorId, AudienceSpec, BoxFuture, CalendarTable, ConceptKind, ConceptPolarity,
    DeterministicMockLlm, EntailmentQuery, EntailmentVerdict, EpochMs, ExtractionBatchOutcome,
    ExtractionBatchRequest, ExtractionChunkSlice, FactId, FactKind, FactStatus, GovernedScope,
    KpError, LibraryMode, LlmClient, MemberRole, ModelAmbiguousScope, ModelConceptRef,
    ModelDateRange, ModelExtractedAtom, ModelQuoteAnchor, ModelRoutePolicy, ModelScopeRef,
    ModelWriteOp, ReviewKind, TimeZone, ValidityWindow,
};
use kp_store::{InitOptions, Store};
use serde_json::json;

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

fn three_member_store(
) -> std::result::Result<(tempfile::TempDir, Store), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let t0 = EpochMs(1_795_089_600_000);
    let mut store = Store::init(
        dir.path(),
        InitOptions {
            library_id: "m1-semiring-gates".parse()?,
            name: "M1 Semiring Gates".into(),
            mode: LibraryMode::Community,
            owner: "alice".parse()?,
            created_at: t0,
        },
    )?;
    store.add_member("bob".parse()?, MemberRole::Member, t0)?;
    store.add_member("carol".parse()?, MemberRole::Member, t0)?;
    Ok((dir, store))
}

#[tokio::test]
async fn raw_only_skip_leaves_zero_unrelated_cache_and_subsequent_host_cli_aligns_and_derives(
) -> TestResult {
    let (_dir, mut store) = three_member_store()?;
    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;
    let t0 = EpochMs(1_795_089_600_000);
    let shanghai = TimeZone::get("Asia/Shanghai")?;
    let calendar = CalendarTable::from_reference_in_tz(t0, &shanghai)?;
    let fri_noon = EpochMs(calendar.day_window_for_token("2026-11-20")?.valid_from.0 + 43_200_000);

    let req = build_extraction_batch_request(
        &store,
        "src_raw_only".parse()?,
        alice.clone(),
        calendar,
        Vec::new(),
        vec![ExtractionChunkSlice {
            chunk_id: "ro_0".into(),
            heading: "notice".into(),
            body: "[speaker:alice] 周五三号楼配电箱停电检修。\n[speaker:bob] 遇到断电施工时必须提前关闭实验机柜。".into(),
        }],
        5,
    )?;

    let out = apply_extracted_atoms(
        &mut store,
        &req,
        &[
            ModelExtractedAtom {
                kind: FactKind::Event,
                claim_text: "周五三号楼配电箱停电检修".into(),
                value: json!({}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "配电箱停电检修".into(),
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
                speaker: Some(alice.clone()),
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: "ro_0".into(),
                    verbatim_quote: "周五三号楼配电箱停电检修。".into(),
                    turn_ordinal: Some(0),
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
            ModelExtractedAtom {
                kind: FactKind::Constraint,
                claim_text: "遇到断电施工时必须提前关闭实验机柜".into(),
                value: json!({}),
                scope: Some(ModelScopeRef::NamedMentions {
                    mentions: vec!["alice".into(), "bob".into()],
                }),
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "断电施工".into(),
                    domain: "lab".into(),
                    polarity: ConceptPolarity::Negative,
                }),
                action_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Action,
                    label: "提前关闭实验机柜".into(),
                    domain: "lab".into(),
                    polarity: ConceptPolarity::Positive,
                }),
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: None,
                speaker: Some(bob.clone()),
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: "ro_0".into(),
                    verbatim_quote: "遇到断电施工时必须提前关闭实验机柜。".into(),
                    turn_ordinal: Some(1),
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
        ],
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([alice.clone(), bob.clone()])),
            t0,
        ),
    )?;

    // 1) Run under `RawOnly` (both direct `HostCliRunner` and `PolicyRoutedLlmClient`):
    let raw_transport = RecordingMockTransport::new(|_| {
        Err(KpError::invalid(
            "raw_only",
            "RawOnly must never spawn subprocess",
        ))
    });
    let mut raw_cfg = HostCliConfig::codex_default();
    raw_cfg.route_policy = ModelRoutePolicy::RawOnly;
    let raw_runner = Arc::new(HostCliRunner::with_transport(raw_cfg, raw_transport)?);
    assert!(raw_runner.is_raw_only());

    let routed_raw = PolicyRoutedLlmClient::new(
        ModelRoutePolicy::RawOnly,
        Arc::clone(&raw_runner) as Arc<dyn LlmClient>,
    );
    assert!(routed_raw.is_raw_only());
    let routed_host_delegated_raw = PolicyRoutedLlmClient::new(
        ModelRoutePolicy::HostCli,
        Arc::clone(&raw_runner) as Arc<dyn LlmClient>,
    );
    assert!(routed_host_delegated_raw.is_raw_only());

    let raw_out = align_concepts_and_derive(
        &mut store,
        &routed_host_delegated_raw,
        &out.active_concepts,
        &[],
        EpochMs(t0.0 + 50),
    )
    .await?;
    assert_eq!(raw_out.derived_warnings, 0);

    // Zero `CONCEPT_ALIGNMENT_PREDICATE` facts persisted after `RawOnly` skip!
    let graph_after_raw = store.derivation_view()?;
    assert!(!graph_after_raw
        .facts
        .values()
        .any(|f| f.predicate == CONCEPT_ALIGNMENT_PREDICATE));
    let table_after_raw = ConceptAlignmentTable::from_graph(&graph_after_raw);
    assert!(table_after_raw.unrelated.is_empty());
    assert!(table_after_raw.fact_unrelated.is_empty());

    // 2) Subsequent `HostCli` run is NOT blocked by stale `Unrelated` cache: aligns & derives!
    let live_transport = RecordingMockTransport::new(|spec| {
        if let Some(out_path) = &spec.output_file {
            std::fs::write(out_path, br#"{"verdict":"entails","question":""}"#)
                .map_err(|_| KpError::invalid("io", "write mock output"))?;
        }
        Ok(SubprocessRawOutput {
            exit_code: Some(0),
            stdout: r#"{"structured_output":{"verdict":"entails","question":""}}"#.into(),
            stderr: String::new(),
            output_file_content: Some(r#"{"verdict":"entails","question":""}"#.into()),
        })
    });
    let host_runner =
        HostCliRunner::with_transport(HostCliConfig::claude_default(), live_transport)?;
    assert!(!host_runner.is_raw_only());
    let routed_host = PolicyRoutedLlmClient::new(ModelRoutePolicy::HostCli, Arc::new(host_runner));
    assert!(!routed_host.is_raw_only());

    let host_out = align_concepts_and_derive(
        &mut store,
        &routed_host,
        &out.active_concepts,
        &[],
        EpochMs(t0.0 + 100),
    )
    .await?;
    assert!(host_out.derived_warnings >= 1);
    assert!(store
        .visible_facts(&alice, fri_noon)?
        .iter()
        .any(|f| f.kind == FactKind::CollisionWarning && f.subject == "alice"));
    Ok(())
}

fn viewer_state_bytes(
    store: &Store,
    viewer: &ActorId,
    at: EpochMs,
) -> std::result::Result<Vec<u8>, Box<dyn std::error::Error>> {
    let vis = store.visible_facts(viewer, at)?;
    let reviews = store.list_review_items(viewer, None)?;
    let mut proofs = Vec::with_capacity(vis.len());
    for f in &vis {
        proofs.push(why(store, &f.id, viewer, at)?);
    }
    Ok(serde_json::to_vec(&json!({
        "visible_facts": vis,
        "review_items": reviews,
        "why_trees": proofs,
    }))?)
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn viewer_non_interference_byte_for_byte_across_private_quote_events_sharing_concept_ids(
) -> TestResult {
    let (_dir, mut store) = three_member_store()?;
    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;
    let carol: ActorId = "carol".parse()?;
    let t0 = EpochMs(1_795_089_600_000);
    let shanghai = TimeZone::get("Asia/Shanghai")?;
    let calendar = CalendarTable::from_reference_in_tz(t0, &shanghai)?;
    let fri_noon = EpochMs(calendar.day_window_for_token("2026-11-20")?.valid_from.0 + 43_200_000);

    let bob_constraint_quote = "遇到断电施工时必须关闭精密恒温箱。";
    let bob_benign_quote = "园区通告：周五开展绿化修剪。";
    let bob_ambig_quote = "园区通告：周五开展弱电井巡检。";
    let bob_entails_quote = "园区通告：周五二号楼全天断电施工。";

    let bob_req = build_extraction_batch_request(
        &store,
        "src_bob_base".parse()?,
        bob.clone(),
        calendar.clone(),
        Vec::new(),
        vec![ExtractionChunkSlice {
            chunk_id: "b0".into(),
            heading: "bob-notes".into(),
            body: format!(
                "{bob_constraint_quote}\n{bob_benign_quote}\n{bob_ambig_quote}\n{bob_entails_quote}"
            ),
        }],
        5,
    )?;

    let bob_base_out = apply_extracted_atoms(
        &mut store,
        &bob_req,
        &[
            ModelExtractedAtom {
                kind: FactKind::Constraint,
                claim_text: "遇到断电施工时必须关闭精密恒温箱".into(),
                value: json!({}),
                scope: Some(ModelScopeRef::NamedMentions {
                    mentions: vec!["alice".into(), "bob".into()],
                }),
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "断电施工".into(),
                    domain: "lab".into(),
                    polarity: ConceptPolarity::Negative,
                }),
                action_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Action,
                    label: "关闭精密恒温箱".into(),
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
                    chunk_id: "b0".into(),
                    verbatim_quote: bob_constraint_quote.into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
            ModelExtractedAtom {
                kind: FactKind::Event,
                claim_text: "园区通告周五绿化修剪".into(),
                value: json!({}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "园区通告".into(),
                    domain: "campus".into(),
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
                    chunk_id: "b0".into(),
                    verbatim_quote: bob_benign_quote.into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
            ModelExtractedAtom {
                kind: FactKind::Event,
                claim_text: "园区通告周五弱电井巡检".into(),
                value: json!({}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "工程巡检通告".into(),
                    domain: "campus".into(),
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
                    chunk_id: "b0".into(),
                    verbatim_quote: bob_ambig_quote.into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
            ModelExtractedAtom {
                kind: FactKind::Event,
                claim_text: "园区通告周五二号楼断电施工".into(),
                value: json!({}),
                scope: Some(ModelScopeRef::SpeakerSelf),
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "二号楼供电通告".into(),
                    domain: "campus".into(),
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
                    chunk_id: "b0".into(),
                    verbatim_quote: bob_entails_quote.into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
        ],
        &NlFunnelOptions::personal_or_shared(AudienceSpec::AllMembers, t0),
    )?;
    let bob_constraint_id = bob_base_out.created_fact_ids[0].clone();

    let notice_cid = bob_base_out
        .active_concepts
        .iter()
        .find(|c| c.canonical_label == "园区通告")
        .ok_or("missing 园区通告 concept")?
        .id
        .clone();
    let inspect_cid = bob_base_out
        .active_concepts
        .iter()
        .find(|c| c.canonical_label == "工程巡检通告")
        .ok_or("missing 工程巡检通告 concept")?
        .id
        .clone();
    let rule_cid = bob_base_out
        .active_concepts
        .iter()
        .find(|c| c.canonical_label == "断电施工")
        .ok_or("missing 断电施工 concept")?
        .id
        .clone();

    let alice_priv_entails_quote = "内部备忘：周五一号楼实验层全天断电施工。";
    let alice_priv_ambig_quote = "内部备忘：周五实验层备用UPS切换演练。";
    let alice_priv_edited_quote = "内部备忘：周五一号楼外墙清洗。";

    let mock_llm = DeterministicMockLlm::default();
    mock_llm.register_entailment_with_quotes(
        "工程巡检通告",
        "断电施工",
        Some(bob_ambig_quote.into()),
        Some(bob_constraint_quote.into()),
        EntailmentVerdict::Ambiguous {
            question: "弱电井巡检是否会导致断电？".into(),
        },
    )?;
    mock_llm.register_entailment_with_quotes(
        "二号楼供电通告",
        "断电施工",
        Some(bob_entails_quote.into()),
        Some(bob_constraint_quote.into()),
        EntailmentVerdict::Entails,
    )?;
    mock_llm.register_entailment_with_quotes(
        "园区通告",
        "断电施工",
        Some(alice_priv_entails_quote.into()),
        Some(bob_constraint_quote.into()),
        EntailmentVerdict::Entails,
    )?;
    mock_llm.register_entailment_with_quotes(
        "工程巡检通告",
        "断电施工",
        Some(alice_priv_ambig_quote.into()),
        Some(bob_constraint_quote.into()),
        EntailmentVerdict::Ambiguous {
            question: "备用UPS切换演练是否算断电施工？".into(),
        },
    )?;
    mock_llm.register_entailment_with_quotes(
        "园区通告",
        "断电施工",
        Some(alice_priv_edited_quote.into()),
        Some(bob_constraint_quote.into()),
        EntailmentVerdict::Unrelated,
    )?;

    // Baseline alignment & review persistence before Alice touches any private events:
    let base_open = store.list_all_open_review_items()?;
    let base_col = align_concepts_and_derive(
        &mut store,
        &mock_llm,
        &bob_base_out.active_concepts,
        &base_open,
        EpochMs(t0.0 + 50),
    )
    .await?;
    for rev in &base_col.emitted_reviews {
        store.upsert_open_review_item(rev)?;
    }

    let bob_baseline = viewer_state_bytes(&store, &bob, fri_noon)?;
    let carol_baseline = viewer_state_bytes(&store, &carol, fri_noon)?;
    assert_eq!(store.list_review_items(&bob, None)?.len(), 1);
    assert_eq!(store.list_review_items(&carol, None)?.len(), 0);

    // 1) Alice adds TWO private-quote `Event`s (`audience = [alice]`) sharing `notice_cid` & `inspect_cid`:
    let alice_req1 = build_extraction_batch_request(
        &store,
        "src_alice_priv_1".parse()?,
        alice.clone(),
        calendar.clone(),
        bob_base_out.active_concepts.clone(),
        vec![ExtractionChunkSlice {
            chunk_id: "a1".into(),
            heading: "alice-private".into(),
            body: format!("{alice_priv_entails_quote}\n{alice_priv_ambig_quote}"),
        }],
        5,
    )?;
    let alice_priv_opts = NlFunnelOptions::personal_or_shared(
        AudienceSpec::Explicit(BTreeSet::from([alice.clone()])),
        EpochMs(t0.0 + 100),
    );
    let alice_add_out = apply_extracted_atoms(
        &mut store,
        &alice_req1,
        &[
            ModelExtractedAtom {
                kind: FactKind::Event,
                claim_text: "周五一号楼实验层断电施工".into(),
                value: json!({}),
                scope: Some(ModelScopeRef::SpeakerSelf),
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::Existing {
                    concept_id: notice_cid.clone(),
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
                    verbatim_quote: alice_priv_entails_quote.into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
            ModelExtractedAtom {
                kind: FactKind::Event,
                claim_text: "周五实验层UPS切换演练".into(),
                value: json!({}),
                scope: Some(ModelScopeRef::SpeakerSelf),
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::Existing {
                    concept_id: inspect_cid.clone(),
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
                    verbatim_quote: alice_priv_ambig_quote.into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
        ],
        &alice_priv_opts,
    )?;
    let alice_ev_entails_id = alice_add_out.created_fact_ids[0].clone();
    let alice_ev_ambig_id = alice_add_out.created_fact_ids[1].clone();

    let open_now = store.list_all_open_review_items()?;
    let after_add = align_concepts_and_derive(
        &mut store,
        &mock_llm,
        &alice_add_out.active_concepts,
        &open_now,
        EpochMs(t0.0 + 150),
    )
    .await?;
    for rev in &after_add.emitted_reviews {
        store.upsert_open_review_item(rev)?;
    }

    // Alice gets her private warning AND her own private `ConceptAlignment` review with fact-bound target_id:
    assert!(store
        .visible_facts(&alice, fri_noon)?
        .iter()
        .any(|f| f.kind == FactKind::CollisionWarning && f.subject == "alice"));
    let alice_reviews = store.list_review_items(&alice, None)?;
    assert_eq!(alice_reviews.len(), 1);
    assert_eq!(alice_reviews[0].kind, ReviewKind::ConceptAlignment);
    assert_eq!(
        alice_reviews[0].target_id,
        format!("{inspect_cid}->{rule_cid}->{alice_ev_ambig_id}:{bob_constraint_id}")
    );

    // Bystanders `bob` and `carol` are BYTE-FOR-BYTE identical to baseline!
    assert_eq!(viewer_state_bytes(&store, &bob, fri_noon)?, bob_baseline);
    assert_eq!(
        viewer_state_bytes(&store, &carol, fri_noon)?,
        carol_baseline
    );

    // 2) Alice edits (`SupersedeOwn`) her `Entails` private event into an `Unrelated` quote:
    let alice_req2 = build_extraction_batch_request(
        &store,
        "src_alice_priv_2".parse()?,
        alice.clone(),
        calendar,
        alice_add_out.active_concepts.clone(),
        vec![ExtractionChunkSlice {
            chunk_id: "a2".into(),
            heading: "alice-edit".into(),
            body: alice_priv_edited_quote.into(),
        }],
        5,
    )?;
    let mut edit_opts = alice_priv_opts.clone();
    edit_opts.at = EpochMs(t0.0 + 200);
    let alice_edit_out = apply_extracted_atoms(
        &mut store,
        &alice_req2,
        &[ModelExtractedAtom {
            kind: FactKind::Event,
            claim_text: "周五一号楼外墙清洗".into(),
            value: json!({}),
            scope: Some(ModelScopeRef::SpeakerSelf),
            ambiguous_scope: None,
            condition_concept: Some(ModelConceptRef::Existing {
                concept_id: notice_cid.clone(),
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
                chunk_id: "a2".into(),
                verbatim_quote: alice_priv_edited_quote.into(),
                turn_ordinal: None,
                approx_char_offset: None,
            },
            write_op: ModelWriteOp::SupersedeOwn {
                target_fact_id: alice_ev_entails_id,
            },
        }],
        &edit_opts,
    )?;
    let alice_edited_ev_id = alice_edit_out.created_fact_ids[0].clone();

    let open_after_edit = store.list_all_open_review_items()?;
    align_concepts_and_derive(
        &mut store,
        &mock_llm,
        &alice_edit_out.active_concepts,
        &open_after_edit,
        EpochMs(t0.0 + 250),
    )
    .await?;

    // Alice's warning disappears after superseding her private premise; `bob` and `carol` stay byte-identical!
    assert!(!store
        .visible_facts(&alice, fri_noon)?
        .iter()
        .any(|f| f.kind == FactKind::CollisionWarning && f.subject == "alice"));
    assert_eq!(viewer_state_bytes(&store, &bob, fri_noon)?, bob_baseline);
    assert_eq!(
        viewer_state_bytes(&store, &carol, fri_noon)?,
        carol_baseline
    );

    // 3) Manual concept alignment bound to `(alice_ev_ambig_id, bob_constraint_id)` derives only for Alice!
    record_manual_concept_alignment(
        &mut store,
        &inspect_cid,
        &rule_cid,
        EntailmentVerdict::Entails,
        Some(&alice_ev_ambig_id),
        Some(&bob_constraint_id),
        EpochMs(t0.0 + 300),
    )?;
    derive(&mut store)?;
    assert!(store
        .visible_facts(&alice, fri_noon)?
        .iter()
        .any(|f| f.kind == FactKind::CollisionWarning && f.subject == "alice"));
    assert_eq!(viewer_state_bytes(&store, &bob, fri_noon)?, bob_baseline);
    assert_eq!(
        viewer_state_bytes(&store, &carol, fri_noon)?,
        carol_baseline
    );

    // 4) Alice retracts (`forget` + `reject`) all her private events: her warning vanishes and
    //    `bob`/`carol` remain byte-for-byte identical!
    store.forget_fact_id(&alice_edited_ev_id, &alice, EpochMs(t0.0 + 350))?;
    reject_fact_and_record(
        &mut store,
        &alice_ev_ambig_id,
        &alice,
        "not a real outage",
        None,
        None,
        EpochMs(t0.0 + 360),
    )?;
    derive(&mut store)?;
    assert!(!store
        .visible_facts(&alice, fri_noon)?
        .iter()
        .any(|f| f.kind == FactKind::CollisionWarning && f.subject == "alice"));
    assert_eq!(viewer_state_bytes(&store, &bob, fri_noon)?, bob_baseline);
    assert_eq!(
        viewer_state_bytes(&store, &carol, fri_noon)?,
        carol_baseline
    );
    Ok(())
}

#[tokio::test]
async fn speaker_self_event_with_condition_and_action_stays_event_and_collides() -> TestResult {
    let (_dir, mut store) = three_member_store()?;
    let alice: ActorId = "alice".parse()?;
    let t0 = EpochMs(1_795_089_600_000);
    let shanghai = TimeZone::get("Asia/Shanghai")?;
    let calendar = CalendarTable::from_reference_in_tz(t0, &shanghai)?;
    let fri_noon = EpochMs(calendar.day_window_for_token("2026-11-20")?.valid_from.0 + 43_200_000);

    let req = build_extraction_batch_request(
        &store,
        "src_self_event".parse()?,
        alice.clone(),
        calendar,
        Vec::new(),
        vec![ExtractionChunkSlice {
            chunk_id: "se0".into(),
            heading: "self-schedule".into(),
            body: "我自己周五下午要去低温冷库清点样品。\n进低温冷库作业必须穿防寒服。".into(),
        }],
        5,
    )?;

    let out = apply_extracted_atoms(
        &mut store,
        &req,
        &[
            ModelExtractedAtom {
                kind: FactKind::Event,
                claim_text: "自己周五下午去低温冷库清点样品".into(),
                value: json!({}),
                scope: Some(ModelScopeRef::SpeakerSelf),
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "进入低温冷库".into(),
                    domain: "lab".into(),
                    polarity: ConceptPolarity::Neutral,
                }),
                action_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Action,
                    label: "清点样品".into(),
                    domain: "lab".into(),
                    polarity: ConceptPolarity::Neutral,
                }),
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
                    chunk_id: "se0".into(),
                    verbatim_quote: "我自己周五下午要去低温冷库清点样品。".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
            ModelExtractedAtom {
                kind: FactKind::Constraint,
                claim_text: "进低温冷库作业必须穿防寒服".into(),
                value: json!({}),
                scope: Some(ModelScopeRef::SpeakerSelf),
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "进入低温冷库".into(),
                    domain: "lab".into(),
                    polarity: ConceptPolarity::Neutral,
                }),
                action_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Action,
                    label: "穿防寒服".into(),
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
                    chunk_id: "se0".into(),
                    verbatim_quote: "进低温冷库作业必须穿防寒服。".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
        ],
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([alice.clone()])),
            t0,
        ),
    )?;

    let graph = store.derivation_view()?;
    let ev_fact = &graph.facts[&out.created_fact_ids[0]];
    assert_eq!(ev_fact.kind, FactKind::Event);
    assert_eq!(
        ev_fact.governed_scope,
        Some(GovernedScope::SelfOnly {
            actor: alice.clone()
        })
    );

    let mock_llm = DeterministicMockLlm::default();
    let col =
        align_concepts_and_derive(&mut store, &mock_llm, &out.active_concepts, &[], t0).await?;
    assert_eq!(col.derived_warnings, 1);
    assert!(store
        .visible_facts(&alice, fri_noon)?
        .iter()
        .any(|f| f.kind == FactKind::CollisionWarning && f.subject == "alice"));
    Ok(())
}

#[test]
fn nl_funnel_preserves_multi_day_spans_and_never_truncates_multi_token_quotes() -> TestResult {
    let (_dir, mut store) = three_member_store()?;
    let alice: ActorId = "alice".parse()?;
    let shanghai = TimeZone::get("Asia/Shanghai")?;
    // Sunday 2026-11-22 10:00 +08:00:
    let sunday = kp_model::parse_timestamp_ms_in_tz("2026-11-22T10:00:00+08:00", &shanghai)
        .ok_or("parse sunday")?;
    let calendar = CalendarTable::from_reference_in_tz(sunday, &shanghai)?;

    let body = "通知一：周五到周日阅览室闭馆。\n通知二：周五、周六、周日三天展厅施工。";
    let req = build_extraction_batch_request(
        &store,
        "src_multiday".parse()?,
        alice.clone(),
        calendar.clone(),
        Vec::new(),
        vec![ExtractionChunkSlice {
            chunk_id: "md0".into(),
            heading: "spans".into(),
            body: body.into(),
        }],
        5,
    )?;

    let out = apply_extracted_atoms(
        &mut store,
        &req,
        &[
            ModelExtractedAtom {
                kind: FactKind::Event,
                claim_text: "周五到周日阅览室闭馆".into(),
                value: json!({}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "阅览室闭馆".into(),
                    domain: "club".into(),
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
                    chunk_id: "md0".into(),
                    verbatim_quote: "通知一：周五到周日阅览室闭馆。".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
            ModelExtractedAtom {
                kind: FactKind::Event,
                claim_text: "周五至周日三天展厅施工".into(),
                value: json!({}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "展厅施工".into(),
                    domain: "club".into(),
                    polarity: ConceptPolarity::Negative,
                }),
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: Some(ModelDateRange {
                    start_date_iso: "2026-11-27".into(),
                    end_date_iso: Some("2026-11-29".into()),
                    open_ended: false,
                }),
                speaker: None,
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: "md0".into(),
                    verbatim_quote: "通知二：周五、周六、周日三天展厅施工。".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
        ],
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([alice])),
            sunday,
        ),
    )?;

    let expected_3d = ValidityWindow {
        valid_from: calendar.day_window_for_token("2026-11-27")?.valid_from,
        valid_until: calendar.day_window_for_token("2026-11-29")?.valid_until,
    };
    let graph = store.derivation_view()?;
    for id in &out.created_fact_ids {
        let kp_model::FactAccess::Base { stamp, .. } = &graph.facts[id].access else {
            return Err("expected base fact".into());
        };
        assert_eq!(stamp.valid, expected_3d);
    }
    Ok(())
}

#[test]
fn partial_named_mentions_failure_downgrades_entire_atom_to_self_only_with_diagnostic() -> TestResult
{
    let (_dir, mut store) = three_member_store()?;
    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;
    let t0 = EpochMs(1_795_089_600_000);
    let shanghai = TimeZone::get("Asia/Shanghai")?;
    let calendar = CalendarTable::from_reference_in_tz(t0, &shanghai)?;

    let body = "一：我和bob还有外包小张值班时不接私活。\n二：也许bob和临时访客都要登记工牌。\n三：bob周五带备用钥匙。";
    let req = build_extraction_batch_request(
        &store,
        "src_partial_mentions".parse()?,
        alice.clone(),
        calendar,
        Vec::new(),
        vec![ExtractionChunkSlice {
            chunk_id: "pm0".into(),
            heading: "mentions".into(),
            body: body.into(),
        }],
        5,
    )?;

    let out = apply_extracted_atoms(
        &mut store,
        &req,
        &[
            ModelExtractedAtom {
                kind: FactKind::Constraint,
                claim_text: "值班时不接私活".into(),
                value: json!({}),
                scope: Some(ModelScopeRef::NamedMentions {
                    mentions: vec!["bob".into(), "外包小张".into()],
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
                    chunk_id: "pm0".into(),
                    verbatim_quote: "一：我和bob还有外包小张值班时不接私活。".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
            ModelExtractedAtom {
                kind: FactKind::Constraint,
                claim_text: "进门登记工牌".into(),
                value: json!({}),
                scope: None,
                ambiguous_scope: Some(ModelAmbiguousScope {
                    broader_candidate: ModelScopeRef::NamedMentions {
                        mentions: vec!["bob".into(), "临时访客".into()],
                    },
                    question: "是否也要求bob和临时访客登记工牌？".into(),
                }),
                condition_concept: None,
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: None,
                speaker: None,
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: "pm0".into(),
                    verbatim_quote: "二：也许bob和临时访客都要登记工牌。".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
            ModelExtractedAtom {
                kind: FactKind::Constraint,
                claim_text: "周五带备用钥匙".into(),
                value: json!({}),
                scope: Some(ModelScopeRef::NamedMentions {
                    mentions: vec!["bob".into()],
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
                    chunk_id: "pm0".into(),
                    verbatim_quote: "三：bob周五带备用钥匙。".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
        ],
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([alice.clone()])),
            t0,
        ),
    )?;

    assert_eq!(out.created_fact_ids.len(), 3);
    assert_eq!(out.rejected_atoms.len(), 2);
    assert!(out.rejected_atoms.iter().all(|r| r
        .reason
        .starts_with("unresolved mentions downgraded to self_only:")));

    let graph = store.derivation_view()?;
    let f1 = &graph.facts[&out.created_fact_ids[0]];
    assert_eq!(
        f1.governed_scope,
        Some(GovernedScope::SelfOnly {
            actor: alice.clone()
        })
    );
    // Because partial mention downgraded to `SelfOnly(alice)`, `bob` was NOT added to `f1`'s audience!
    assert!(!store.visible_facts(&bob, t0)?.iter().any(|f| f.id == f1.id));

    let f2 = &graph.facts[&out.created_fact_ids[1]];
    assert_eq!(
        f2.governed_scope,
        Some(GovernedScope::SelfOnly { actor: alice })
    );
    assert_eq!(f2.broader_scope, None);

    let f3 = &graph.facts[&out.created_fact_ids[2]];
    assert_eq!(
        f3.governed_scope,
        Some(GovernedScope::Actors {
            actors: BTreeSet::from([bob.clone()])
        })
    );
    assert!(store.visible_facts(&bob, t0)?.iter().any(|f| f.id == f3.id));
    Ok(())
}

struct MidFlightWithdrawLlm {
    store_path: PathBuf,
    withdraw_fact_id: FactId,
    withdraw_actor: ActorId,
    withdraw_at: EpochMs,
    verdict_to_return: EntailmentVerdict,
}

impl LlmClient for MidFlightWithdrawLlm {
    fn extract_batch<'a>(
        &'a self,
        _request: &'a ExtractionBatchRequest,
    ) -> BoxFuture<'a, kp_model::Result<ExtractionBatchOutcome>> {
        Box::pin(async { Ok(ExtractionBatchOutcome::SkippedEmpty) })
    }

    fn judge_entailment<'a>(
        &'a self,
        _query: &'a EntailmentQuery,
    ) -> BoxFuture<'a, kp_model::Result<EntailmentVerdict>> {
        Box::pin(async move {
            let mut concurrent = Store::open(&self.store_path)
                .map_err(|_| KpError::invalid("test", "open concurrent store"))?;
            concurrent
                .forget_fact_id(
                    &self.withdraw_fact_id,
                    &self.withdraw_actor,
                    self.withdraw_at,
                )
                .map_err(|_| KpError::invalid("test", "withdraw mid-flight"))?;
            Ok(self.verdict_to_return.clone())
        })
    }
}

#[tokio::test]
async fn secondary_premise_validation_blocks_stale_writeback_after_async_judgement() -> TestResult {
    let (dir, mut store) = three_member_store()?;
    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;
    let t0 = EpochMs(1_795_089_600_000);
    let shanghai = TimeZone::get("Asia/Shanghai")?;
    let calendar = CalendarTable::from_reference_in_tz(t0, &shanghai)?;

    let req = build_extraction_batch_request(
        &store,
        "src_b2_race".parse()?,
        alice.clone(),
        calendar,
        Vec::new(),
        vec![ExtractionChunkSlice {
            chunk_id: "race0".into(),
            heading: "race".into(),
            body: "[speaker:alice] 周五空调机组检修。\n[speaker:bob] 机房升温时暂停压测任务。"
                .into(),
        }],
        5,
    )?;

    let out = apply_extracted_atoms(
        &mut store,
        &req,
        &[
            ModelExtractedAtom {
                kind: FactKind::Event,
                claim_text: "周五空调机组检修".into(),
                value: json!({}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "空调机组检修".into(),
                    domain: "infra".into(),
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
                speaker: Some(alice.clone()),
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: "race0".into(),
                    verbatim_quote: "周五空调机组检修。".into(),
                    turn_ordinal: Some(0),
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
            ModelExtractedAtom {
                kind: FactKind::Constraint,
                claim_text: "机房升温时暂停压测任务".into(),
                value: json!({}),
                scope: Some(ModelScopeRef::NamedMentions {
                    mentions: vec!["alice".into(), "bob".into()],
                }),
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "机房升温".into(),
                    domain: "infra".into(),
                    polarity: ConceptPolarity::Negative,
                }),
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: None,
                speaker: Some(bob.clone()),
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: "race0".into(),
                    verbatim_quote: "机房升温时暂停压测任务。".into(),
                    turn_ordinal: Some(1),
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
        ],
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([alice.clone(), bob.clone()])),
            t0,
        ),
    )?;
    let ev_id = out.created_fact_ids[0].clone();

    let racing_llm = MidFlightWithdrawLlm {
        store_path: dir.path().to_path_buf(),
        withdraw_fact_id: ev_id.clone(),
        withdraw_actor: alice,
        withdraw_at: EpochMs(t0.0 + 20),
        verdict_to_return: EntailmentVerdict::Ambiguous {
            question: "空调检修是否会导致机房升温？".into(),
        },
    };

    let col = align_concepts_and_derive(
        &mut store,
        &racing_llm,
        &out.active_concepts,
        &[],
        EpochMs(t0.0 + 30),
    )
    .await?;

    // Supporting event was withdrawn while `judge_entailment` was in flight:
    // ZERO alignment facts persisted, ZERO reviews emitted, ZERO warnings derived!
    assert_eq!(col.derived_warnings, 0);
    assert!(col.emitted_reviews.is_empty());
    let fresh = store.derivation_view()?;
    assert_eq!(fresh.facts[&ev_id].status, FactStatus::Withdrawn);
    assert!(!fresh
        .facts
        .values()
        .any(|f| f.predicate == CONCEPT_ALIGNMENT_PREDICATE));
    Ok(())
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn legacy_v1_unbound_alignment_cache_rows_are_ignored_on_upgrade_preserving_non_interference(
) -> TestResult {
    let (_dir, mut store) = three_member_store()?;
    let bob: ActorId = "bob".parse()?;
    let carol: ActorId = "carol".parse()?;
    let t0 = EpochMs(1_795_089_600_000);
    let shanghai = TimeZone::get("Asia/Shanghai")?;
    let calendar = CalendarTable::from_reference_in_tz(t0, &shanghai)?;
    let fri_noon = EpochMs(calendar.day_window_for_token("2026-11-20")?.valid_from.0 + 43_200_000);

    let bob_rule_quote = "遇到高压停电时必须关闭实验台电源。";
    let bob_benign_ev1 = "物业通告：周五园区草坪浇水。";
    let bob_benign_ev2 = "后勤通告：周五走廊灯具清洁。";

    let req = build_extraction_batch_request(
        &store,
        "src_v1_upgrade".parse()?,
        bob.clone(),
        calendar,
        Vec::new(),
        vec![ExtractionChunkSlice {
            chunk_id: "v1_0".into(),
            heading: "bob".into(),
            body: format!("{bob_rule_quote}\n{bob_benign_ev1}\n{bob_benign_ev2}"),
        }],
        5,
    )?;

    let out = apply_extracted_atoms(
        &mut store,
        &req,
        &[
            ModelExtractedAtom {
                kind: FactKind::Constraint,
                claim_text: "遇到高压停电时必须关闭实验台电源".into(),
                value: json!({}),
                scope: Some(ModelScopeRef::SpeakerSelf),
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "高压停电".into(),
                    domain: "lab".into(),
                    polarity: ConceptPolarity::Negative,
                }),
                action_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Action,
                    label: "关闭实验台电源".into(),
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
                    chunk_id: "v1_0".into(),
                    verbatim_quote: bob_rule_quote.into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
            ModelExtractedAtom {
                kind: FactKind::Event,
                claim_text: "周五园区草坪浇水".into(),
                value: json!({}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "物业通告".into(),
                    domain: "campus".into(),
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
                    chunk_id: "v1_0".into(),
                    verbatim_quote: bob_benign_ev1.into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
            ModelExtractedAtom {
                kind: FactKind::Event,
                claim_text: "周五走廊灯具清洁".into(),
                value: json!({}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "后勤通告".into(),
                    domain: "campus".into(),
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
                    chunk_id: "v1_0".into(),
                    verbatim_quote: bob_benign_ev2.into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
        ],
        &NlFunnelOptions::personal_or_shared(AudienceSpec::AllMembers, t0),
    )?;

    let rule_cid = out
        .active_concepts
        .iter()
        .find(|c| c.canonical_label == "高压停电")
        .ok_or("missing 高压停电")?
        .id
        .clone();
    let prop_cid = out
        .active_concepts
        .iter()
        .find(|c| c.canonical_label == "物业通告")
        .ok_or("missing 物业通告")?
        .id
        .clone();
    let logi_cid = out
        .active_concepts
        .iter()
        .find(|c| c.canonical_label == "后勤通告")
        .ok_or("missing 后勤通告")?
        .id
        .clone();

    let mock_llm = DeterministicMockLlm::default();
    let clean_col = align_concepts_and_derive(
        &mut store,
        &mock_llm,
        &out.active_concepts,
        &[],
        EpochMs(t0.0 + 10),
    )
    .await?;
    assert_eq!(clean_col.derived_warnings, 0);
    assert!(clean_col.emitted_reviews.is_empty());

    let bob_clean_bytes = viewer_state_bytes(&store, &bob, fri_noon)?;
    let carol_clean_bytes = viewer_state_bytes(&store, &carol, fri_noon)?;

    // Inject two pre-upgrade legacy `v1` `kp.core.concept_alignment` cache rows (lacking BOTH
    // `evidence_refs` and `"binding": "concept_label_v2"`): one `"entails"` and one `"ambiguous"`!
    let alice: ActorId = "alice".parse()?;
    for (id_str, left_cid, obj) in [
        (
            "align-legacy-v1-entails",
            &prop_cid,
            json!({
                "left_concept_id": prop_cid,
                "right_concept_id": rule_cid,
                "verdict": "entails",
            }),
        ),
        (
            "align-legacy-v1-ambig",
            &logi_cid,
            json!({
                "left_concept_id": logi_cid,
                "right_concept_id": rule_cid,
                "verdict": "ambiguous",
                "question": "alice旧版私有引文遗留问题？",
            }),
        ),
    ] {
        store.insert_fact(&kp_model::Fact {
            id: id_str.parse()?,
            origin_library: store.library_id().clone(),
            logical_key: id_str.parse()?,
            kind: FactKind::Observation,
            subject: left_cid.to_string(),
            predicate: CONCEPT_ALIGNMENT_PREDICATE.into(),
            object: obj,
            tags: BTreeSet::new(),
            author: alice.clone(),
            ingested_by: None,
            trust_tier: kp_model::TrustTier::ToolOrModelVerified,
            access: kp_model::FactAccess::Base {
                audience: AudienceSpec::Explicit(BTreeSet::new()),
                stamp: kp_model::BitemporalStamp {
                    valid: ValidityWindow {
                        valid_from: EpochMs(0),
                        valid_until: EpochMs::OPEN_END,
                    },
                    recorded_at: EpochMs(t0.0 + 20),
                    expired_at: None,
                },
            },
            source_scope_id: None,
            governed_scope: None,
            broader_scope: None,
            status: FactStatus::Active,
            superseded_by: None,
            rank: kp_model::FactRank::Normal,
            source_anchor: None,
            unverified_gaps: Vec::new(),
            miss_count: 0,
        })?;
    }

    let tbl = ConceptAlignmentTable::from_graph(&store.derivation_view()?);
    assert!(tbl.entails.is_empty());
    assert!(tbl.ambiguous.is_empty());

    let after_v1_col = align_concepts_and_derive(
        &mut store,
        &mock_llm,
        &out.active_concepts,
        &[],
        EpochMs(t0.0 + 30),
    )
    .await?;
    assert_eq!(after_v1_col.derived_warnings, 0);
    assert!(after_v1_col.emitted_reviews.is_empty());
    assert_eq!(viewer_state_bytes(&store, &bob, fri_noon)?, bob_clean_bytes);
    assert_eq!(
        viewer_state_bytes(&store, &carol, fri_noon)?,
        carol_clean_bytes
    );
    Ok(())
}

#[test]
fn nl_funnel_resolves_rescheduled_weekday_to_target_and_preserves_multi_clause_weekdays(
) -> TestResult {
    let (_dir, mut store) = three_member_store()?;
    let alice: ActorId = "alice".parse()?;
    let shanghai = TimeZone::get("Asia/Shanghai")?;
    // Thursday 2026-11-19 20:00 +08:00: upcoming Monday is 2026-11-23, Wednesday is 2026-11-25.
    let thursday = kp_model::parse_timestamp_ms_in_tz("2026-11-19T20:00:00+08:00", &shanghai)
        .ok_or("parse thursday")?;
    let calendar = CalendarTable::from_reference_in_tz(thursday, &shanghai)?;

    let body = "例会从周一改到周三。\n周一交提纲，周三做分享。";
    let req = build_extraction_batch_request(
        &store,
        "src_resched_multi".parse()?,
        alice.clone(),
        calendar.clone(),
        Vec::new(),
        vec![ExtractionChunkSlice {
            chunk_id: "rm0".into(),
            heading: "schedule".into(),
            body: body.into(),
        }],
        5,
    )?;

    let out = apply_extracted_atoms(
        &mut store,
        &req,
        &[
            // 1) Rescheduled event `"例会从周一改到周三"`: even if model mistakenly emitted Monday,
            //    deterministic reschedule resolution lands on Wednesday (`2026-11-25`)!
            ModelExtractedAtom {
                kind: FactKind::Event,
                claim_text: "例会从周一改到周三".into(),
                value: json!({}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "组内例会".into(),
                    domain: "club".into(),
                    polarity: ConceptPolarity::Neutral,
                }),
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: Some(ModelDateRange {
                    start_date_iso: "2026-11-23".into(),
                    end_date_iso: None,
                    open_ended: false,
                }),
                speaker: None,
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: "rm0".into(),
                    verbatim_quote: "例会从周一改到周三。".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
            // 2) Multi-clause quote `"周一交提纲，周三做分享"` -> Atom A stays Monday (`2026-11-23`)!
            ModelExtractedAtom {
                kind: FactKind::Event,
                claim_text: "周一交提纲".into(),
                value: json!({}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "提交读书提纲".into(),
                    domain: "club".into(),
                    polarity: ConceptPolarity::Neutral,
                }),
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: Some(ModelDateRange {
                    start_date_iso: "2026-11-23".into(),
                    end_date_iso: None,
                    open_ended: false,
                }),
                speaker: None,
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: "rm0".into(),
                    verbatim_quote: "周一交提纲，周三做分享。".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
            // 3) Multi-clause quote `"周一交提纲，周三做分享"` -> Atom B stays Wednesday (`2026-11-25`)!
            ModelExtractedAtom {
                kind: FactKind::Event,
                claim_text: "周三做分享".into(),
                value: json!({}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "做专题分享".into(),
                    domain: "club".into(),
                    polarity: ConceptPolarity::Neutral,
                }),
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: Some(ModelDateRange {
                    start_date_iso: "2026-11-25".into(),
                    end_date_iso: None,
                    open_ended: false,
                }),
                speaker: None,
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: "rm0".into(),
                    verbatim_quote: "周一交提纲，周三做分享。".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
        ],
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([alice])),
            thursday,
        ),
    )?;

    let mon_win = calendar.day_window_for_token("2026-11-23")?;
    let wed_win = calendar.day_window_for_token("2026-11-25")?;
    let graph = store.derivation_view()?;

    let kp_model::FactAccess::Base {
        stamp: s_resched, ..
    } = &graph.facts[&out.created_fact_ids[0]].access
    else {
        return Err("expected base fact".into());
    };
    assert_eq!(s_resched.valid, wed_win);

    let kp_model::FactAccess::Base { stamp: s_mon, .. } =
        &graph.facts[&out.created_fact_ids[1]].access
    else {
        return Err("expected base fact".into());
    };
    assert_eq!(s_mon.valid, mon_win);

    let kp_model::FactAccess::Base { stamp: s_wed, .. } =
        &graph.facts[&out.created_fact_ids[2]].access
    else {
        return Err("expected base fact".into());
    };
    assert_eq!(s_wed.valid, wed_win);

    // R9: Even if the LLM mistakenly returns a multi-day range (`2026-11-23..2026-11-25`) on a reschedule
    // quote `"例会从周一改到周三。"`, deterministic reschedule resolution STILL pins Wednesday (`2026-11-25`)!
    let out_multiday_resched = apply_extracted_atoms(
        &mut store,
        &req,
        &[ModelExtractedAtom {
            kind: FactKind::Event,
            claim_text: "例会调整至周三".into(),
            value: json!({}),
            scope: None,
            ambiguous_scope: None,
            condition_concept: Some(ModelConceptRef::ProposeNew {
                kind: ConceptKind::Condition,
                label: "调整例会".into(),
                domain: "club".into(),
                polarity: ConceptPolarity::Neutral,
            }),
            action_concept: None,
            group_concept: None,
            member_mention: None,
            valid_window: None,
            valid_dates: Some(ModelDateRange {
                start_date_iso: "2026-11-23".into(),
                end_date_iso: Some("2026-11-25".into()),
                open_ended: false,
            }),
            speaker: None,
            assumed_premise_ids: Vec::new(),
            quote: ModelQuoteAnchor {
                chunk_id: "rm0".into(),
                verbatim_quote: "例会从周一改到周三。".into(),
                turn_ordinal: None,
                approx_char_offset: None,
            },
            write_op: ModelWriteOp::Add,
        }],
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from(["alice".parse()?])),
            EpochMs(thursday.0 + 10),
        ),
    )?;
    let graph2 = store.derivation_view()?;
    let kp_model::FactAccess::Base {
        stamp: s_resched_range,
        ..
    } = &graph2.facts[&out_multiday_resched.created_fact_ids[0]].access
    else {
        return Err("expected base fact".into());
    };
    assert_eq!(s_resched_range.valid, wed_win);
    Ok(())
}

#[test]
fn expired_lease_commit_staged_extraction_job_fenced_rolls_back_all_fact_and_concept_writes(
) -> TestResult {
    let (_dir, mut store) = three_member_store()?;
    let alice: ActorId = "alice".parse()?;
    let t0 = EpochMs(1_795_089_600_000);
    let shanghai = TimeZone::get("Asia/Shanghai")?;
    let calendar = CalendarTable::from_reference_in_tz(t0, &shanghai)?;

    let body = "下周五仪器室校准气压表。";
    let origin_lib = store.library_id().clone();
    let stored = store.ingest_source(
        &origin_lib,
        &"doc:r7-fence".parse()?,
        &kp_store::RawSourceInput {
            uri: "doc:r7-fence".into(),
            media_type: "text/markdown".into(),
            content: body.as_bytes().to_vec(),
            readable_by: AudienceSpec::Explicit(BTreeSet::from([alice.clone()])),
            captured_at: t0,
            chunks: vec![kp_store::ChunkInput {
                heading: "r7".into(),
                body: body.into(),
                anchor: kp_model::AnchorSpec::default(),
            }],
            self_echo_filtered: false,
        },
    )?;
    let chunk_id = format!("{}-0", stored.source_id);
    let chunk_sha = kp_model::ContentHash::digest(body);
    let enqueued = store.enqueue_extraction_job(&kp_store::ExtractionJobInput {
        source_id: stored.source_id.clone(),
        chunk_ids: vec![chunk_id.clone()],
        chunk_batch_sha256: chunk_sha.clone(),
        author: alice.clone(),
        model_policy: "auto".into(),
        priority: 10,
        reference_time: t0,
        created_at: t0,
    })?;

    // Worker 1 leases the job for 1,000 ms (`lease_until = t0 + 1_000`, `attempts == 1`).
    let job_w1 = store
        .lease_next_extraction_job(t0, 1_000)?
        .ok_or("expected leased job for worker 1")?;
    assert_eq!(job_w1.job_id, enqueued.job_id);
    assert_eq!(job_w1.attempts, 1);

    let req = build_extraction_batch_request(
        &store,
        stored.source_id,
        alice.clone(),
        calendar,
        Vec::new(),
        vec![ExtractionChunkSlice {
            chunk_id: chunk_id.clone(),
            heading: "r7".into(),
            body: body.into(),
        }],
        5,
    )?;
    let (_outcome, staged_w1) = kp_derive::stage_extracted_atoms(
        &store,
        &req,
        &[ModelExtractedAtom {
            kind: FactKind::Event,
            claim_text: "仪器室校准气压表".into(),
            value: json!({}),
            scope: None,
            ambiguous_scope: None,
            condition_concept: Some(ModelConceptRef::ProposeNew {
                kind: ConceptKind::Condition,
                label: "校准气压表".into(),
                domain: "lab".into(),
                polarity: ConceptPolarity::Neutral,
            }),
            action_concept: None,
            group_concept: None,
            member_mention: None,
            valid_window: None,
            valid_dates: Some(ModelDateRange {
                start_date_iso: "2026-11-27".into(),
                end_date_iso: None,
                open_ended: false,
            }),
            speaker: None,
            assumed_premise_ids: Vec::new(),
            quote: ModelQuoteAnchor {
                chunk_id,
                verbatim_quote: body.into(),
                turn_ordinal: None,
                approx_char_offset: None,
            },
            write_op: ModelWriteOp::Add,
        }],
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([alice.clone()])),
            t0,
        ),
    )?;
    assert_eq!(staged_w1.writes.len(), 1);
    assert!(store.derivation_view()?.facts.is_empty());

    // Lease expires at `t0 + 2_000` and Worker 2 steals the job (`attempts == 2`).
    let job_w2 = store
        .lease_next_extraction_job(EpochMs(t0.0 + 2_000), 60_000)?
        .ok_or("expected re-leased job for worker 2")?;
    assert_eq!(job_w2.attempts, 2);

    // Late Worker 1 tries to commit with stale `attempts == 1`: MUST fail with `StoreError::Conflict`
    // and leave ZERO facts, ZERO concepts, and ZERO `extracted_chunks` in SQLite!
    assert!(matches!(
        store.commit_staged_extraction_job_fenced(&kp_store::FencedJobCommitSpec {
            job_id: &job_w1.job_id,
            expected_attempts: job_w1.attempts,
            lease_now: EpochMs(t0.0 + 2_100),
            batch: &staged_w1,
            chunk_hashes: std::slice::from_ref(&chunk_sha),
            author: Some(&alice),
            schema_ver: "m1-nl-v1",
            at: EpochMs(t0.0 + 2_100),
        }),
        Err(kp_store::StoreError::Conflict)
    ));
    assert!(store.derivation_view()?.facts.is_empty());
    assert!(store.list_active_concepts()?.is_empty());
    assert!(!store.is_chunk_extracted(&chunk_sha, &alice, "m1-nl-v1")?);

    // Worker 2 commits cleanly under `attempts == 2`!
    store.commit_staged_extraction_job_fenced(&kp_store::FencedJobCommitSpec {
        job_id: &job_w2.job_id,
        expected_attempts: job_w2.attempts,
        lease_now: EpochMs(t0.0 + 2_200),
        batch: &staged_w1,
        chunk_hashes: std::slice::from_ref(&chunk_sha),
        author: Some(&alice),
        schema_ver: "m1-nl-v1",
        at: EpochMs(t0.0 + 2_200),
    })?;
    assert_eq!(store.derivation_view()?.facts.len(), 1);
    assert_eq!(store.list_active_concepts()?.len(), 1);
    assert!(store.is_chunk_extracted(&chunk_sha, &alice, "m1-nl-v1")?);
    Ok(())
}

#[tokio::test]
async fn ambiguous_scope_review_hides_and_blocks_resolution_after_supporting_membership_withdrawal(
) -> TestResult {
    let (_dir, mut store) = three_member_store()?;
    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;
    let t0 = EpochMs(1_795_089_600_000);
    let shanghai = TimeZone::get("Asia/Shanghai")?;
    let calendar = CalendarTable::from_reference_in_tz(t0, &shanghai)?;

    let body = "bob是夜班组的一员。\n也许夜班组值守前都要双人签到。";
    let req = build_extraction_batch_request(
        &store,
        "src_r11_scope".parse()?,
        alice.clone(),
        calendar,
        Vec::new(),
        vec![ExtractionChunkSlice {
            chunk_id: "r11_0".into(),
            heading: "night-shift".into(),
            body: body.into(),
        }],
        5,
    )?;

    let out = apply_extracted_atoms(
        &mut store,
        &req,
        &[
            ModelExtractedAtom {
                kind: FactKind::Membership,
                claim_text: "bob加入夜班组".into(),
                value: json!({}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: None,
                action_concept: None,
                group_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Group,
                    label: "夜班组".into(),
                    domain: "ops".into(),
                    polarity: ConceptPolarity::Neutral,
                }),
                member_mention: Some("bob".into()),
                valid_window: None,
                valid_dates: None,
                speaker: None,
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: "r11_0".into(),
                    verbatim_quote: "bob是夜班组的一员。".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
            ModelExtractedAtom {
                kind: FactKind::Constraint,
                claim_text: "值守前双人签到".into(),
                value: json!({}),
                scope: None,
                ambiguous_scope: Some(ModelAmbiguousScope {
                    broader_candidate: ModelScopeRef::RelationGroup {
                        group_concept: ModelConceptRef::ProposeNew {
                            kind: ConceptKind::Group,
                            label: "夜班组".into(),
                            domain: "ops".into(),
                            polarity: ConceptPolarity::Neutral,
                        },
                    },
                    question: "是否把双人签到扩展到整个夜班组？".into(),
                }),
                condition_concept: None,
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: None,
                speaker: None,
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: "r11_0".into(),
                    verbatim_quote: "也许夜班组值守前都要双人签到。".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
        ],
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([alice.clone(), bob.clone()])),
            t0,
        ),
    )?;
    let membership_id = out.created_fact_ids[0].clone();
    let constraint_id = out.created_fact_ids[1].clone();

    let mock_llm = DeterministicMockLlm::default();
    let col = align_concepts_and_derive(
        &mut store,
        &mock_llm,
        &out.active_concepts,
        &[],
        EpochMs(t0.0 + 10),
    )
    .await?;
    assert_eq!(col.emitted_reviews.len(), 1);
    let rev_item = col.emitted_reviews[0].clone();
    assert_eq!(rev_item.kind, ReviewKind::AmbiguousScope);
    store.upsert_open_review_item(&rev_item)?;
    assert_eq!(
        store
            .list_review_items(&alice, Some(kp_model::ReviewStatus::Open))?
            .len(),
        1
    );

    // Withdraw Bob's `Membership` in `夜班组` -> no non-speaker member remains in `夜班组`:
    // the open `AmbiguousScope` review MUST immediately vanish from `list_review_items` AND reject resolution!
    store.forget_fact_id(&membership_id, &alice, EpochMs(t0.0 + 30))?;
    assert!(store
        .list_review_items(&alice, Some(kp_model::ReviewStatus::Open))?
        .is_empty());
    assert!(matches!(
        kp_derive::resolve_ambiguous_scope_review(
            &mut store,
            &constraint_id,
            &alice,
            kp_derive::ScopeReviewDecision::ConfirmBroader,
            Some(&rev_item),
            EpochMs(t0.0 + 40),
        ),
        Err(kp_store::StoreError::NotFound)
    ));

    // Legacy `ReviewItem` lacking `supporting_membership_ids` (pre-R11 format) ALSO hides and rejects
    // resolution when no live `Membership` in `夜班组` remains — EVEN IF Bob holds an active `Membership`
    // in another group `白班组` AND an unbound legacy `v1` `白班组 -> 夜班组 entails` cache row exists!
    let other_grp: kp_model::ConceptId = "cpt_other_day_shift".parse()?;
    store.upsert_community_concept(&kp_model::CommunityConcept {
        id: other_grp.clone(),
        kind: ConceptKind::Group,
        canonical_label: "白班组".into(),
        domain: "ops".into(),
        polarity: ConceptPolarity::Neutral,
        proposed_by: alice.clone(),
        created_at: EpochMs(t0.0 + 41),
    })?;
    let mut other_mem = store.derivation_view()?.facts[&membership_id].clone();
    other_mem.id = "fact_other_mem_h".parse()?;
    other_mem.logical_key = "other-mem-h".parse()?;
    other_mem.status = FactStatus::Active;
    if let kp_model::FactAccess::Base { ref mut stamp, .. } = other_mem.access {
        stamp.expired_at = None;
    }
    other_mem.object = serde_json::to_value(kp_model::MembershipRelation {
        anchor: alice.clone(),
        member: bob.clone(),
        group_concept_id: other_grp.clone(),
    })?;
    store.insert_fact(&other_mem)?;
    assert!(store
        .visible_facts(&alice, EpochMs(t0.0 + 45))?
        .iter()
        .any(|f| f.id == other_mem.id));
    let night_grp = out
        .active_concepts
        .iter()
        .find(|c| c.canonical_label == "夜班组")
        .ok_or("missing 夜班组")?
        .id
        .clone();
    store.insert_fact(&kp_model::Fact {
        id: "align-legacy-v1-group-h-to-g".parse()?,
        origin_library: store.library_id().clone(),
        logical_key: "align-legacy-v1-group-h-to-g".parse()?,
        kind: FactKind::Observation,
        subject: other_grp.to_string(),
        predicate: CONCEPT_ALIGNMENT_PREDICATE.into(),
        object: json!({
            "left_concept_id": other_grp,
            "right_concept_id": night_grp,
            "verdict": "entails",
        }),
        tags: BTreeSet::new(),
        author: alice.clone(),
        ingested_by: None,
        trust_tier: kp_model::TrustTier::ToolOrModelVerified,
        access: kp_model::FactAccess::Base {
            audience: AudienceSpec::Explicit(BTreeSet::new()),
            stamp: kp_model::BitemporalStamp {
                valid: ValidityWindow {
                    valid_from: EpochMs(0),
                    valid_until: EpochMs::OPEN_END,
                },
                recorded_at: EpochMs(t0.0 + 42),
                expired_at: None,
            },
        },
        source_scope_id: None,
        governed_scope: None,
        broader_scope: None,
        status: FactStatus::Active,
        superseded_by: None,
        rank: kp_model::FactRank::Normal,
        source_anchor: None,
        unverified_gaps: Vec::new(),
        miss_count: 0,
    })?;

    let mut legacy_rev = rev_item;
    if let Some(obj) = legacy_rev.payload.as_object_mut() {
        obj.remove("supporting_membership_ids");
    }
    assert!(!kp_store::is_ambiguous_scope_review_supported(
        &store.derivation_view()?,
        &legacy_rev
    ));
    assert!(matches!(
        kp_derive::resolve_ambiguous_scope_review(
            &mut store,
            &constraint_id,
            &alice,
            kp_derive::ScopeReviewDecision::ConfirmBroader,
            Some(&legacy_rev),
            EpochMs(t0.0 + 45),
        ),
        Err(kp_store::StoreError::NotFound)
    ));
    Ok(())
}

struct CountingPolicyMockLlm {
    policy: ModelRoutePolicy,
    verdict: EntailmentVerdict,
    entailment_calls: std::sync::atomic::AtomicUsize,
}

impl CountingPolicyMockLlm {
    fn new(policy: ModelRoutePolicy, verdict: EntailmentVerdict) -> Self {
        Self {
            policy,
            verdict,
            entailment_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.entailment_calls
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl LlmClient for CountingPolicyMockLlm {
    fn is_raw_only(&self) -> bool {
        self.policy == ModelRoutePolicy::RawOnly
    }

    fn route_policy(&self) -> ModelRoutePolicy {
        self.policy.clone()
    }

    fn extract_batch<'a>(
        &'a self,
        request: &'a ExtractionBatchRequest,
    ) -> BoxFuture<'a, kp_model::Result<ExtractionBatchOutcome>> {
        Box::pin(async move {
            request.validate_request()?;
            Ok(ExtractionBatchOutcome::SkippedEmpty)
        })
    }

    fn judge_entailment<'a>(
        &'a self,
        _query: &'a EntailmentQuery,
    ) -> BoxFuture<'a, kp_model::Result<EntailmentVerdict>> {
        Box::pin(async move {
            self.entailment_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.verdict.clone())
        })
    }
}

#[tokio::test]
async fn named_endpoint_facts_never_leak_to_global_host_cli_or_foreign_endpoint_during_alignment(
) -> TestResult {
    let (_dir, mut store) = three_member_store()?;
    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;
    let t0 = EpochMs(1_795_089_600_000);
    let shanghai = TimeZone::get("Asia/Shanghai")?;
    let calendar = CalendarTable::from_reference_in_tz(t0, &shanghai)?;
    let fri_noon = EpochMs(calendar.day_window_for_token("2026-11-20")?.valid_from.0 + 43_200_000);

    let req = build_extraction_batch_request(
        &store,
        "src_corp_gw".parse()?,
        alice.clone(),
        calendar,
        Vec::new(),
        vec![ExtractionChunkSlice {
            chunk_id: "cg_0".into(),
            heading: "corp_notice".into(),
            body: "[speaker:alice] 周五三号楼配电箱停电检修。\n[speaker:bob] 遇到断电施工时必须提前关闭实验机柜。".into(),
        }],
        5,
    )?;

    let mut corp_opts = NlFunnelOptions::personal_or_shared(
        AudienceSpec::Explicit(BTreeSet::from([alice.clone(), bob.clone()])),
        t0,
    );
    corp_opts.route_policy = ModelRoutePolicy::NamedEndpoint("corp-gw".into());

    let out = apply_extracted_atoms(
        &mut store,
        &req,
        &[
            ModelExtractedAtom {
                kind: FactKind::Event,
                claim_text: "周五三号楼配电箱停电检修".into(),
                value: json!({}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "配电箱停电检修".into(),
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
                speaker: Some(alice.clone()),
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: "cg_0".into(),
                    verbatim_quote: "周五三号楼配电箱停电检修。".into(),
                    turn_ordinal: Some(0),
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
            ModelExtractedAtom {
                kind: FactKind::Constraint,
                claim_text: "遇到断电施工时必须提前关闭实验机柜".into(),
                value: json!({}),
                scope: Some(ModelScopeRef::NamedMentions {
                    mentions: vec!["alice".into(), "bob".into()],
                }),
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "断电施工".into(),
                    domain: "lab".into(),
                    polarity: ConceptPolarity::Negative,
                }),
                action_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Action,
                    label: "提前关闭实验机柜".into(),
                    domain: "lab".into(),
                    polarity: ConceptPolarity::Positive,
                }),
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: None,
                speaker: Some(bob.clone()),
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: "cg_0".into(),
                    verbatim_quote: "遇到断电施工时必须提前关闭实验机柜。".into(),
                    turn_ordinal: Some(1),
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
            },
        ],
        &corp_opts,
    )?;

    for fid in &out.created_fact_ids {
        let g = store.derivation_view()?;
        let stored = g
            .facts
            .get(fid)
            .ok_or_else(|| KpError::invalid("test", "missing created fact"))?;
        assert_eq!(
            kp_derive::fact_route_policy(stored),
            ModelRoutePolicy::NamedEndpoint("corp-gw".into())
        );
    }

    // 1. Global `HostCli` client MUST send 0 `judge_entailment` calls and cache 0 `Unrelated` rows:
    let host_mock =
        CountingPolicyMockLlm::new(ModelRoutePolicy::HostCli, EntailmentVerdict::Entails);
    let _ = kp_derive::reconcile_contradicted_events(
        &mut store,
        &host_mock,
        &out.active_concepts,
        EpochMs(t0.0 + 10),
    )
    .await?;
    let host_out = align_concepts_and_derive(
        &mut store,
        &host_mock,
        &out.active_concepts,
        &[],
        EpochMs(t0.0 + 20),
    )
    .await?;
    assert_eq!(host_mock.calls(), 0);
    assert_eq!(host_out.derived_warnings, 0);

    // 2. Foreign `NamedEndpoint("other-ep")` client MUST ALSO send 0 calls and cache 0 `Unrelated` rows:
    let foreign_mock = CountingPolicyMockLlm::new(
        ModelRoutePolicy::NamedEndpoint("other-ep".into()),
        EntailmentVerdict::Entails,
    );
    let foreign_out = align_concepts_and_derive(
        &mut store,
        &foreign_mock,
        &out.active_concepts,
        &[],
        EpochMs(t0.0 + 30),
    )
    .await?;
    assert_eq!(foreign_mock.calls(), 0);
    assert_eq!(foreign_out.derived_warnings, 0);

    let g_before_corp = store.derivation_view()?;
    assert!(!g_before_corp
        .facts
        .values()
        .any(|f| f.predicate == CONCEPT_ALIGNMENT_PREDICATE));

    // 3. Matching `NamedEndpoint("corp-gw")` client aligns cleanly and derives `CollisionWarning`!
    let corp_mock = CountingPolicyMockLlm::new(
        ModelRoutePolicy::NamedEndpoint("corp-gw".into()),
        EntailmentVerdict::Entails,
    );
    let corp_out = align_concepts_and_derive(
        &mut store,
        &corp_mock,
        &out.active_concepts,
        &[],
        EpochMs(t0.0 + 40),
    )
    .await?;
    assert!(corp_mock.calls() >= 1);
    assert!(corp_out.derived_warnings >= 1);
    assert!(store
        .visible_facts(&alice, fri_noon)?
        .iter()
        .any(|f| f.kind == FactKind::CollisionWarning && f.subject == "alice"));

    Ok(())
}
