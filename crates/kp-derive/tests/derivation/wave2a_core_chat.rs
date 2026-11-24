use std::collections::BTreeSet;

use kp_derive::{
    apply_extracted_atoms, build_extraction_batch_request, build_extraction_prompt,
    stage_extracted_atoms, NlFunnelOptions,
};
use kp_model::{
    format_tagged_turns, ActorId, AnchorSpec, AudienceSpec, CalendarTable, ChunkTurn, ConceptKind,
    ConceptPolarity, ContentHash, EpochMs, FactKind, FactStatus, LibraryMode, MemberRole,
    ModelConceptRef, ModelExtractedAtom, ModelQuoteAnchor, ModelRoutePolicy, ModelScopeRef,
    ModelWriteOp, PublicAudience, PublicContainerRoster, PublicIngestOp, PublicIngestRecord,
    PublicIngestSource, PublicSpeaker, SecretRedactor, SliceRole, SourceOriginMeta, SourceScopeId,
    TimeZone, DEFAULT_MAX_BATCH_TOKENS,
};
use kp_store::{
    ChunkInput, FencedJobCommitSpec, GovernedSourceInput, GovernedUpsertMode, InitOptions,
    PublicRecordAppliedKind, Store, StoreError,
};
use serde_json::json;

use crate::Result;

fn wave2a_community_store() -> Result<(tempfile::TempDir, Store)> {
    let dir = tempfile::tempdir()?;
    let t0 = EpochMs(1_795_089_600_000);
    let mut store = Store::init(
        dir.path(),
        InitOptions {
            library_id: "wave2a-lib".parse()?,
            name: "Wave 2A Test Library".into(),
            mode: LibraryMode::Community,
            owner: "alice".parse()?,
            created_at: t0,
        },
    )?;
    for member in ["bob", "carol", "dave"] {
        store.add_member(member.parse()?, MemberRole::Maintainer, t0)?;
    }
    Ok((dir, store))
}

#[test]
fn upsert_governed_source_supersedes_prior_facts_cancels_jobs_and_partitions_large_docs() -> Result
{
    let (_dir, mut store) = wave2a_community_store()?;
    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;
    let t0 = EpochMs(1_795_089_600_000);
    let shanghai = TimeZone::get("Asia/Shanghai")?;
    let calendar = CalendarTable::from_reference_in_tz(t0, &shanghai)?;

    let rule_quote = "All workspace crates must forbid unwrap_used and expect_used.";
    let filler_paragraph = "Detailed subsystem invariants and verification notes. ".repeat(110);
    let v1_chunks = vec![
        ChunkInput {
            heading: "Safety Lints".into(),
            body: format!("{rule_quote}\n{filler_paragraph}"),
            anchor: AnchorSpec {
                file_path: Some("docs/architecture.md".into()),
                start_byte: Some(0),
                ..AnchorSpec::default()
            },
        },
        ChunkInput {
            heading: "Storage Layer".into(),
            body: format!("Storage write fencing details.\n{filler_paragraph}"),
            anchor: AnchorSpec {
                file_path: Some("docs/architecture.md".into()),
                ..AnchorSpec::default()
            },
        },
        ChunkInput {
            heading: "Connector Layer".into(),
            body: format!("Connector cursor checkpointing.\n{filler_paragraph}"),
            anchor: AnchorSpec {
                file_path: Some("docs/architecture.md".into()),
                ..AnchorSpec::default()
            },
        },
        ChunkInput {
            heading: "Retrieval Layer".into(),
            body: format!("Semiring visibility evaluation.\n{filler_paragraph}"),
            anchor: AnchorSpec {
                file_path: Some("docs/architecture.md".into()),
                ..AnchorSpec::default()
            },
        },
    ];
    let v1_bytes = v1_chunks
        .iter()
        .map(|c| c.body.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
        .into_bytes();

    let out_v1 = store.upsert_governed_source(GovernedSourceInput {
        external_source_key: "file:///docs/architecture.md".into(),
        uri: "file:///docs/architecture.md".into(),
        media_type: "text/markdown".into(),
        bytes: v1_bytes.clone(),
        chunks: v1_chunks.clone(),
        speaker: alice.clone(),
        ingested_by: alice.clone(),
        readable_by: AudienceSpec::AllMembers,
        source_scope_id: None,
        effective_policy: ModelRoutePolicy::HostCli,
        op_kind: GovernedUpsertMode::AutoSupersede,
        thread_key: None,
        reply_to_key: None,
        captured_at: t0,
    })?;
    assert_eq!(out_v1.kind, PublicRecordAppliedKind::Upserted);
    assert!(
        out_v1.enqueued_job_ids.len() >= 2,
        "large multi-chunk document must partition into >=2 bounded jobs, got {:?}",
        out_v1.enqueued_job_ids
    );
    let src_v1 = out_v1.source.ok_or("expected v1 StoredSource")?;

    let win1 = store
        .lease_extraction_window(t0, 60_000, DEFAULT_MAX_BATCH_TOKENS)?
        .ok_or("expected first partition window")?;
    assert_eq!(win1.jobs.len(), 1);
    let first_slice = win1.slices.first().cloned().ok_or("expected first slice")?;
    let req_v1 = build_extraction_batch_request(
        &store,
        src_v1.source_id.clone(),
        alice.clone(),
        calendar,
        Vec::new(),
        vec![first_slice.clone()],
        5,
    )?;
    let (stage_out, staged_v1) = stage_extracted_atoms(
        &store,
        &req_v1,
        &[ModelExtractedAtom {
            kind: FactKind::Decision,
            claim_text: rule_quote.into(),
            value: json!({"lint": "unwrap_used"}),
            scope: None,
            ambiguous_scope: None,
            condition_concept: None,
            action_concept: None,
            group_concept: None,
            member_mention: None,
            valid_window: None,
            valid_dates: None,
            speaker: Some(alice.clone()),
            assumed_premise_ids: Vec::new(),
            quote: ModelQuoteAnchor {
                chunk_id: first_slice.chunk_id.clone(),
                verbatim_quote: rule_quote.into(),
                turn_ordinal: Some(0),
                approx_char_offset: None,
            },
            evidence: None,
            write_op: ModelWriteOp::Add,
            command_guard: None,
        }],
        &NlFunnelOptions::personal_or_shared(AudienceSpec::AllMembers, t0),
    )?;
    let v1_fact_id = stage_out.created_fact_ids[0].clone();
    store.commit_staged_extraction_job_fenced(&FencedJobCommitSpec {
        jobs: &[(win1.jobs[0].job_id.clone(), win1.jobs[0].attempts)],
        lease_now: EpochMs(t0.0 + 100),
        batch: &staged_v1,
        chunk_hashes: &[(ContentHash::digest(&first_slice.body), alice.clone())],
        schema_ver: "m1-nl-v1",
        at: EpochMs(t0.0 + 100),
    })?;
    assert!(store.pending_extraction_job_count()? >= 1);
    assert_eq!(
        store.derivation_view()?.facts[&v1_fact_id].status,
        FactStatus::Active
    );

    let dup_out = store.upsert_governed_source(GovernedSourceInput {
        external_source_key: "file:///docs/architecture.md".into(),
        uri: "file:///docs/architecture.md".into(),
        media_type: "text/markdown".into(),
        bytes: v1_bytes.clone(),
        chunks: v1_chunks.clone(),
        speaker: alice.clone(),
        ingested_by: alice.clone(),
        readable_by: AudienceSpec::AllMembers,
        source_scope_id: None,
        effective_policy: ModelRoutePolicy::HostCli,
        op_kind: GovernedUpsertMode::AutoSupersede,
        thread_key: None,
        reply_to_key: None,
        captured_at: EpochMs(t0.0 + 200),
    })?;
    assert_eq!(dup_out.kind, PublicRecordAppliedKind::DuplicateIdempotent);
    assert_eq!(
        store.derivation_view()?.facts[&v1_fact_id].status,
        FactStatus::Active
    );

    let v2_body = "Updated rule: workspace crates also forbid todo! macros in release builds.";
    let out_v2 = store.upsert_governed_source(GovernedSourceInput {
        external_source_key: "file:///docs/architecture.md".into(),
        uri: "file:///docs/architecture.md".into(),
        media_type: "text/markdown".into(),
        bytes: v2_body.as_bytes().to_vec(),
        chunks: vec![ChunkInput {
            heading: "Safety Lints".into(),
            body: v2_body.into(),
            anchor: AnchorSpec::default(),
        }],
        speaker: alice.clone(),
        ingested_by: alice.clone(),
        readable_by: AudienceSpec::AllMembers,
        source_scope_id: None,
        effective_policy: ModelRoutePolicy::HostCli,
        op_kind: GovernedUpsertMode::AutoSupersede,
        thread_key: None,
        reply_to_key: None,
        captured_at: EpochMs(t0.0 + 1_000),
    })?;
    assert_eq!(out_v2.kind, PublicRecordAppliedKind::Superseded);
    assert_eq!(out_v2.superseded_source_id, Some(src_v1.source_id.clone()));
    assert!(out_v2.affected_facts.contains(&v1_fact_id));
    assert_eq!(
        store.derivation_view()?.facts[&v1_fact_id].status,
        FactStatus::Superseded
    );
    assert!(store
        .visible_facts(&bob, EpochMs(t0.0 + 1_500))?
        .into_iter()
        .all(|f| f.id != v1_fact_id));
    assert_eq!(
        store.pending_extraction_job_count()?,
        1,
        "only v2's new job should remain pending; v1's pending partition jobs must be cancelled"
    );

    let src_v2 = out_v2.source.ok_or("expected v2 StoredSource")?;
    let cross_ctx = store
        .extraction_source_context(&src_v2.source_id, &[first_slice.chunk_id])?
        .ok_or("expected v2 context")?;
    assert!(
        cross_ctx.chunks.is_empty(),
        "extraction_source_context must never load chunks belonging to a different source_id"
    );

    let out_v3 = store.upsert_governed_source(GovernedSourceInput {
        external_source_key: "file:///docs/architecture.md".into(),
        uri: "file:///docs/architecture.md".into(),
        media_type: "text/markdown".into(),
        bytes: v1_bytes,
        chunks: v1_chunks,
        speaker: alice,
        ingested_by: "alice".parse()?,
        readable_by: AudienceSpec::AllMembers,
        source_scope_id: None,
        effective_policy: ModelRoutePolicy::HostCli,
        op_kind: GovernedUpsertMode::AutoSupersede,
        thread_key: None,
        reply_to_key: None,
        captured_at: EpochMs(t0.0 + 2_000),
    })?;
    assert_eq!(out_v3.kind, PublicRecordAppliedKind::Superseded);
    assert_eq!(out_v3.superseded_source_id, Some(src_v2.source_id));
    Ok(())
}

#[test]
fn base_anchor_slide_page_sheet_survive_nl_funnel_staging_via_quote_in_chunk() -> Result {
    let (_dir, mut store) = wave2a_community_store()?;
    let alice: ActorId = "alice".parse()?;
    let t0 = EpochMs(1_795_089_600_000);
    let shanghai = TimeZone::get("Asia/Shanghai")?;
    let calendar = CalendarTable::from_reference_in_tz(t0, &shanghai)?;

    let cases = [
        (
            "file:///deck/review.pptx",
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            b"PK\x03\x04PPTX_BINARY".as_slice(),
            "Slide 7 > Rollout",
            "Production cutover requires green canary metrics for 30 minutes.",
            AnchorSpec {
                file_path: Some("deck/review.pptx".into()),
                slide: Some(7),
                ..AnchorSpec::default()
            },
        ),
        (
            "file:///specs/handbook.pdf",
            "application/pdf",
            b"%PDF-1.7 BINARY".as_slice(),
            "Page 12",
            "Database backups run every six hours with checksum verification.",
            AnchorSpec {
                file_path: Some("specs/handbook.pdf".into()),
                page: Some(12),
                ..AnchorSpec::default()
            },
        ),
        (
            "file:///ops/sla.xlsx",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            b"PK\x03\x04XLSX_BINARY".as_slice(),
            "Sheet SLA",
            "| Tier | Max RTO |\n| --- | --- |\n| P0 | 15 minutes |",
            AnchorSpec {
                file_path: Some("ops/sla.xlsx".into()),
                sheet: Some("SLA".into()),
                cell: Some("B4".into()),
                ..AnchorSpec::default()
            },
        ),
        (
            "file:///notes/runbook.md",
            "text/markdown",
            b"0123456789ABCDEFDrain worker queues before restarting redis.".as_slice(),
            "Runbook > Redis",
            "Drain worker queues before restarting redis.",
            AnchorSpec {
                file_path: Some("notes/runbook.md".into()),
                start_byte: Some(16),
                start_line: Some(10),
                end_line: Some(12),
                ..AnchorSpec::default()
            },
        ),
    ];

    for (idx, (ext_key, media_type, raw_bytes, heading, body, base_anchor)) in
        cases.into_iter().enumerate()
    {
        store.upsert_governed_source(GovernedSourceInput {
            external_source_key: ext_key.into(),
            uri: ext_key.into(),
            media_type: media_type.into(),
            bytes: raw_bytes.to_vec(),
            chunks: vec![ChunkInput {
                heading: heading.into(),
                body: body.into(),
                anchor: base_anchor,
            }],
            speaker: alice.clone(),
            ingested_by: alice.clone(),
            readable_by: AudienceSpec::AllMembers,
            source_scope_id: None,
            effective_policy: ModelRoutePolicy::HostCli,
            op_kind: GovernedUpsertMode::AutoSupersede,
            thread_key: None,
            reply_to_key: None,
            captured_at: EpochMs(t0.0 + (idx as i64) * 10),
        })?;
    }

    let mut anchors_by_uri = std::collections::BTreeMap::new();
    let redactor = SecretRedactor::new()?;
    while let Some(win) = store.lease_extraction_window(t0, 60_000, DEFAULT_MAX_BATCH_TOKENS)? {
        let slice = win.slices[0].clone();
        let quote_str = if slice.body.contains("P0 | 15 minutes") {
            "P0 | 15 minutes"
        } else {
            slice.body.as_str()
        };
        let req = build_extraction_batch_request(
            &store,
            slice.source_id.clone(),
            alice.clone(),
            calendar.clone(),
            Vec::new(),
            vec![slice.clone()],
            5,
        )?;
        let prompt = build_extraction_prompt(&req, &redactor);
        assert!(prompt.contains("[section_metadata (non-quotable):"));
        assert!(!slice.body.contains("[section_metadata"));

        let out = apply_extracted_atoms(
            &mut store,
            &req,
            &[ModelExtractedAtom {
                kind: FactKind::Decision,
                claim_text: quote_str.into(),
                value: json!({}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: None,
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: None,
                speaker: Some(alice.clone()),
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: slice.chunk_id.clone(),
                    verbatim_quote: quote_str.into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                evidence: None,
                write_op: ModelWriteOp::Add,
                command_guard: None,
            }],
            &NlFunnelOptions::personal_or_shared(AudienceSpec::AllMembers, t0),
        )?;
        let fid = out.created_fact_ids[0].clone();
        let anchor = store.derivation_view()?.facts[&fid]
            .source_anchor
            .clone()
            .ok_or("missing source_anchor")?;
        anchors_by_uri.insert(win.primary_uri, anchor);
        store.complete_extraction_window_fenced(
            &[(win.jobs[0].job_id.clone(), win.jobs[0].attempts)],
            &[(ContentHash::digest(&slice.body), alice.clone())],
            "m1-nl-v1",
            t0,
        )?;
    }

    let pptx_a = &anchors_by_uri["file:///deck/review.pptx"];
    assert_eq!(pptx_a.slide, Some(7));
    assert_eq!(pptx_a.start_byte, None);
    assert_eq!(pptx_a.end_byte, None);

    let pdf_a = &anchors_by_uri["file:///specs/handbook.pdf"];
    assert_eq!(pdf_a.page, Some(12));
    assert_eq!(pdf_a.start_byte, None);
    assert_eq!(pdf_a.end_byte, None);

    let xlsx_a = &anchors_by_uri["file:///ops/sla.xlsx"];
    assert_eq!(xlsx_a.sheet.as_deref(), Some("SLA"));
    assert_eq!(xlsx_a.cell.as_deref(), Some("B4"));
    assert_eq!(xlsx_a.start_byte, None);
    assert_eq!(xlsx_a.end_byte, None);
    assert_eq!(xlsx_a.verbatim_quote.as_deref(), Some("P0 | 15 minutes"));

    let md_a = &anchors_by_uri["file:///notes/runbook.md"];
    assert_eq!(md_a.start_line, Some(10));
    assert_eq!(md_a.end_line, Some(12));
    assert_eq!(md_a.start_byte, Some(16));
    assert_eq!(
        md_a.end_byte,
        Some(16 + "Drain worker queues before restarting redis.".len() as u64)
    );
    Ok(())
}

#[test]
fn multi_turn_chat_ratification_adjacency_and_bidirectional_cascade_retirement() -> Result {
    let (_dir, mut store) = wave2a_community_store()?;
    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;
    let carol: ActorId = "carol".parse()?;
    let dave: ActorId = "dave".parse()?;
    let t0 = EpochMs(1_795_089_600_000);
    let shanghai = TimeZone::get("Asia/Shanghai")?;
    let calendar = CalendarTable::from_reference_in_tz(t0, &shanghai)?;
    let scope_id: SourceScopeId = "feishu.chat.oc_arch".parse()?;

    store.apply_public_container_roster(
        &alice,
        &PublicContainerRoster {
            container_id: scope_id.to_string(),
            label: "Architecture Chat".into(),
            members: vec!["alice".into(), "bob".into(), "carol".into(), "dave".into()],
            all_library_members: false,
            synced_at: t0,
            provider: Some("feishu".into()),
            upstream_container_id: Some("oc_arch".into()),
        },
    )?;

    let ingest_msg = |store: &mut Store,
                      ext_id: &str,
                      spk: &str,
                      text: &str,
                      op: PublicIngestOp,
                      reply_to: Option<&str>,
                      ts: i64|
     -> Result<Option<kp_model::SourceId>> {
        let out = store.apply_public_ingest_record(
            &PublicIngestRecord {
                source: PublicIngestSource {
                    kind: "feishu".into(),
                    uri: format!("feishu://chat/oc_arch/{ext_id}"),
                    external_id: ext_id.into(),
                    container_id: Some(scope_id.to_string()),
                },
                speaker: PublicSpeaker::new(spk, None),
                audience: PublicAudience::ContainerMembers,
                occurred_at: EpochMs(ts),
                content: text.into(),
                op,
                ingested_by: Some(alice.clone()),
                media_type: Some("text/plain".into()),
                model_policy: None,
                thread_key: Some("chat:oc_arch".into()),
                reply_to: reply_to.map(str::to_owned),
            },
            &alice,
            &ModelRoutePolicy::HostCli,
        )?;
        Ok(out.source_id)
    };

    let prop_text = "Should we forbid unwrap_used across all workspace crates?";
    let src_alice_1 = ingest_msg(
        &mut store,
        "om_1",
        "alice",
        prop_text,
        PublicIngestOp::Upsert,
        None,
        t0.0 + 1_000,
    )?
    .ok_or("src_alice_1")?;
    let _src_carol_1 = ingest_msg(
        &mut store,
        "om_2",
        "carol",
        "+1",
        PublicIngestOp::Upsert,
        None,
        t0.0 + 2_000,
    )?
    .ok_or("src_carol_1")?;
    let src_bob_1 = ingest_msg(
        &mut store,
        "om_3",
        "bob",
        "lgtm",
        PublicIngestOp::Upsert,
        None,
        t0.0 + 3_000,
    )?
    .ok_or("src_bob_1")?;

    let win1 = store
        .lease_extraction_window(EpochMs(t0.0 + 3_500), 60_000, DEFAULT_MAX_BATCH_TOKENS)?
        .ok_or("expected 3-job chat window")?;
    assert_eq!(win1.jobs.len(), 3);
    assert_eq!(win1.slices.len(), 3);
    assert_eq!(win1.slices[0].author, alice);
    assert_eq!(win1.slices[1].author, carol);
    assert_eq!(win1.slices[2].author, bob);

    let req1 = build_extraction_batch_request(
        &store,
        win1.slices[0].source_id.clone(),
        alice.clone(),
        calendar.clone(),
        Vec::new(),
        win1.slices.clone(),
        5,
    )?;
    let mut opts1 =
        NlFunnelOptions::personal_or_shared(win1.readable_by.clone(), EpochMs(t0.0 + 3_500));
    opts1.source_origin = Some(SourceOriginMeta {
        speaker: alice.clone(),
        ingested_by: alice.clone(),
        source_scope_id: scope_id.clone(),
    });

    let (out1, staged1) = stage_extracted_atoms(
        &store,
        &req1,
        &[ModelExtractedAtom {
            kind: FactKind::Decision,
            claim_text: "Forbid unwrap_used across all workspace crates".into(),
            value: json!({"forbid": "unwrap_used"}),
            scope: None,
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
                chunk_id: win1.slices[2].chunk_id.clone(),
                verbatim_quote: "lgtm".into(),
                turn_ordinal: None,
                approx_char_offset: None,
            },
            evidence: Some(ModelQuoteAnchor {
                chunk_id: win1.slices[0].chunk_id.clone(),
                verbatim_quote: prop_text.into(),
                turn_ordinal: None,
                approx_char_offset: None,
            }),
            write_op: ModelWriteOp::Add,
            command_guard: None,
        }],
        &opts1,
    )?;
    assert_eq!(out1.created_fact_ids.len(), 1);
    assert!(out1.rejected_atoms.is_empty());
    let bob_decision_id = out1.created_fact_ids[0].clone();

    let job_specs1: Vec<(String, u32)> = win1
        .jobs
        .iter()
        .map(|j| (j.job_id.clone(), j.attempts))
        .collect();
    let hashes1: Vec<(ContentHash, ActorId)> = win1
        .slices
        .iter()
        .map(|s| (ContentHash::digest(&s.body), s.author.clone()))
        .collect();
    store.commit_staged_extraction_job_fenced(&FencedJobCommitSpec {
        jobs: &job_specs1,
        lease_now: EpochMs(t0.0 + 3_600),
        batch: &staged1,
        chunk_hashes: &hashes1,
        schema_ver: "m1-nl-v1",
        at: EpochMs(t0.0 + 3_600),
    })?;

    let graph1 = store.derivation_view()?;
    let bob_dec = &graph1.facts[&bob_decision_id];
    assert_eq!(bob_dec.author, bob);
    assert_eq!(
        bob_dec
            .source_anchor
            .as_ref()
            .and_then(|a| a.source_id.as_ref()),
        Some(&src_bob_1)
    );
    assert_eq!(
        bob_dec.object["evidence_anchor"]["source_id"].as_str(),
        Some(src_alice_1.as_str())
    );
    assert!(store.is_chunk_extracted(&ContentHash::digest("+1"), &carol, "m1-nl-v1")?);
    assert!(store.is_chunk_extracted(&ContentHash::digest("lgtm"), &bob, "m1-nl-v1")?);

    let _src_followup = ingest_msg(
        &mut store,
        "om_4",
        "carol",
        "Since unwrap_used is forbidden, CI clippy runs with --all-targets.",
        PublicIngestOp::Upsert,
        None,
        t0.0 + 4_000,
    )?;
    let win_f = store
        .lease_extraction_window(EpochMs(t0.0 + 4_100), 60_000, DEFAULT_MAX_BATCH_TOKENS)?
        .ok_or("expected followup window")?;
    assert_eq!(
        win_f
            .slices
            .iter()
            .filter(|s| s.role == SliceRole::Context)
            .count(),
        3,
        "up to 5 preceding extracted thread messages must load as SliceRole::Context"
    );
    let target_f = win_f
        .slices
        .iter()
        .find(|s| s.role == SliceRole::Target)
        .cloned()
        .ok_or("target_f")?;
    let req_f = build_extraction_batch_request(
        &store,
        target_f.source_id.clone(),
        carol.clone(),
        calendar.clone(),
        Vec::new(),
        win_f.slices.clone(),
        5,
    )?;
    let redactor = SecretRedactor::new()?;
    let prompt_f = build_extraction_prompt(&req_f, &redactor);
    assert!(prompt_f.contains("<context_readonly>"));
    assert!(prompt_f.contains("<context_chunk id=\""));

    let ctx_chunk_id = win_f.slices[0].chunk_id.clone();
    let mut opts_f =
        NlFunnelOptions::personal_or_shared(win_f.readable_by.clone(), EpochMs(t0.0 + 4_100));
    opts_f.source_origin = Some(SourceOriginMeta {
        speaker: carol.clone(),
        ingested_by: alice.clone(),
        source_scope_id: scope_id.clone(),
    });
    let (out_f, staged_f) = stage_extracted_atoms(
        &store,
        &req_f,
        &[
            ModelExtractedAtom {
                kind: FactKind::Observation,
                claim_text: "illegal context quote".into(),
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
                    chunk_id: ctx_chunk_id,
                    verbatim_quote: prop_text.into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                evidence: None,
                write_op: ModelWriteOp::Add,
                command_guard: None,
            },
            ModelExtractedAtom {
                kind: FactKind::Observation,
                claim_text: "CI clippy runs with --all-targets".into(),
                value: json!({}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: None,
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: None,
                speaker: Some(carol.clone()),
                assumed_premise_ids: vec![bob_decision_id.clone()],
                quote: ModelQuoteAnchor {
                    chunk_id: target_f.chunk_id.clone(),
                    verbatim_quote:
                        "Since unwrap_used is forbidden, CI clippy runs with --all-targets.".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                evidence: None,
                write_op: ModelWriteOp::Add,
                command_guard: None,
            },
        ],
        &opts_f,
    )?;
    assert_eq!(
        out_f.rejected_atoms.len(),
        1,
        "authority quote referencing SliceRole::Context must be isolate-dropped"
    );
    assert_eq!(out_f.derived_assumed_fact_ids.len(), 1);
    let derived_followup_id = out_f.derived_assumed_fact_ids[0].clone();
    store.commit_staged_extraction_job_fenced(&FencedJobCommitSpec {
        jobs: &[(win_f.jobs[0].job_id.clone(), win_f.jobs[0].attempts)],
        lease_now: EpochMs(t0.0 + 4_200),
        batch: &staged_f,
        chunk_hashes: &[(ContentHash::digest(&target_f.body), carol.clone())],
        schema_ver: "m1-nl-v1",
        at: EpochMs(t0.0 + 4_200),
    })?;
    assert!(store
        .visible_facts(&alice, EpochMs(t0.0 + 4_500))?
        .iter()
        .any(|f| f.id == derived_followup_id));

    let prop2 = "Should we also cap batch extraction windows at 2400 tokens?";
    let _src_a2 = ingest_msg(
        &mut store,
        "om_5",
        "alice",
        prop2,
        PublicIngestOp::Upsert,
        None,
        t0.0 + 5_000,
    )?;
    let _src_c2 = ingest_msg(
        &mut store,
        "om_6",
        "carol",
        "what about tests?",
        PublicIngestOp::Upsert,
        None,
        t0.0 + 6_000,
    )?;
    let _src_b2 = ingest_msg(
        &mut store,
        "om_7",
        "bob",
        "lgtm",
        PublicIngestOp::Upsert,
        None,
        t0.0 + 7_000,
    )?;
    let _src_d2 = ingest_msg(
        &mut store,
        "om_8",
        "dave",
        "lgtm",
        PublicIngestOp::Upsert,
        Some("om_5"),
        t0.0 + 8_000,
    )?;

    let win2 = store
        .lease_extraction_window(EpochMs(t0.0 + 8_500), 60_000, DEFAULT_MAX_BATCH_TOKENS)?
        .ok_or("expected wave 2 window")?;
    assert_eq!(win2.jobs.len(), 4);
    let targets2: Vec<_> = win2
        .slices
        .iter()
        .filter(|s| s.role == SliceRole::Target)
        .cloned()
        .collect();
    assert_eq!(targets2.len(), 4);
    assert_eq!(
        targets2[3].reply_to_chunk_id.as_deref(),
        Some(targets2[0].chunk_id.as_str())
    );

    let req2 = build_extraction_batch_request(
        &store,
        targets2[0].source_id.clone(),
        alice.clone(),
        calendar.clone(),
        Vec::new(),
        win2.slices.clone(),
        5,
    )?;
    let mut opts2 =
        NlFunnelOptions::personal_or_shared(win2.readable_by.clone(), EpochMs(t0.0 + 8_500));
    opts2.source_origin = Some(SourceOriginMeta {
        speaker: alice.clone(),
        ingested_by: alice.clone(),
        source_scope_id: scope_id.clone(),
    });
    let (out2, staged2) = stage_extracted_atoms(
        &store,
        &req2,
        &[
            ModelExtractedAtom {
                kind: FactKind::Decision,
                claim_text: "Cap batch extraction windows at 2400 tokens".into(),
                value: json!({"max_tokens": 2400}),
                scope: None,
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
                    chunk_id: targets2[2].chunk_id.clone(),
                    verbatim_quote: "lgtm".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                evidence: Some(ModelQuoteAnchor {
                    chunk_id: targets2[0].chunk_id.clone(),
                    verbatim_quote: prop2.into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                }),
                write_op: ModelWriteOp::Add,
                command_guard: None,
            },
            ModelExtractedAtom {
                kind: FactKind::Decision,
                claim_text: "Cap batch extraction windows at 2400 tokens".into(),
                value: json!({"max_tokens": 2400}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: None,
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: None,
                speaker: Some(dave.clone()),
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: targets2[3].chunk_id.clone(),
                    verbatim_quote: "lgtm".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                evidence: Some(ModelQuoteAnchor {
                    chunk_id: targets2[0].chunk_id.clone(),
                    verbatim_quote: prop2.into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                }),
                write_op: ModelWriteOp::Add,
                command_guard: None,
            },
        ],
        &opts2,
    )?;
    assert_eq!(out2.rejected_atoms.len(), 1);
    assert_eq!(out2.created_fact_ids.len(), 1);
    let dave_decision_id = out2.created_fact_ids[0].clone();
    let specs2: Vec<(String, u32)> = win2
        .jobs
        .iter()
        .map(|j| (j.job_id.clone(), j.attempts))
        .collect();
    let hashes2: Vec<(ContentHash, ActorId)> = targets2
        .iter()
        .map(|s| (ContentHash::digest(&s.body), s.author.clone()))
        .collect();
    store.commit_staged_extraction_job_fenced(&FencedJobCommitSpec {
        jobs: &specs2,
        lease_now: EpochMs(t0.0 + 8_600),
        batch: &staged2,
        chunk_hashes: &hashes2,
        schema_ver: "m1-nl-v1",
        at: EpochMs(t0.0 + 8_600),
    })?;
    assert_eq!(
        store.derivation_view()?.facts[&dave_decision_id].author,
        dave
    );

    ingest_msg(
        &mut store,
        "om_1",
        "alice",
        "",
        PublicIngestOp::Withdraw,
        None,
        t0.0 + 9_000,
    )?;
    assert_eq!(
        store.derivation_view()?.facts[&bob_decision_id].status,
        FactStatus::Withdrawn
    );
    assert!(!store
        .visible_facts(&alice, EpochMs(t0.0 + 9_500))?
        .iter()
        .any(|f| f.id == bob_decision_id || f.id == derived_followup_id));

    ingest_msg(
        &mut store,
        "om_8",
        "dave",
        "",
        PublicIngestOp::Withdraw,
        Some("om_5"),
        t0.0 + 10_000,
    )?;
    assert_eq!(
        store.derivation_view()?.facts[&dave_decision_id].status,
        FactStatus::Withdrawn
    );

    let sess_body = format_tagged_turns(&[
        ChunkTurn {
            speaker: None,
            is_assistant: true,
            text: "Shall we forbid git push --force on main?".into(),
        },
        ChunkTurn {
            speaker: Some(alice.clone()),
            is_assistant: false,
            text: "yes".into(),
        },
    ]);
    let sess_req = build_extraction_batch_request(
        &store,
        "src_agent_sess_ratify".parse()?,
        alice.clone(),
        calendar,
        Vec::new(),
        vec![kp_model::ExtractionChunkSlice {
            chunk_id: "sess_0".into(),
            source_id: "src_agent_sess_ratify".parse()?,
            author: alice.clone(),
            base_anchor: AnchorSpec::default(),
            role: SliceRole::Target,
            reply_to_chunk_id: None,
            heading: "session".into(),
            body: sess_body,
        }],
        5,
    )?;
    let sess_out = apply_extracted_atoms(
        &mut store,
        &sess_req,
        &[ModelExtractedAtom {
            kind: FactKind::Constraint,
            claim_text: "Forbid git push --force on main".into(),
            value: json!({"forbid": "git push --force"}),
            scope: Some(ModelScopeRef::SpeakerSelf),
            ambiguous_scope: None,
            condition_concept: Some(ModelConceptRef::ProposeNew {
                kind: ConceptKind::Condition,
                label: "pushing to main".into(),
                domain: "git".into(),
                polarity: ConceptPolarity::Neutral,
            }),
            action_concept: Some(ModelConceptRef::ProposeNew {
                kind: ConceptKind::Action,
                label: "git push --force".into(),
                domain: "git".into(),
                polarity: ConceptPolarity::Negative,
            }),
            group_concept: None,
            member_mention: None,
            valid_window: None,
            valid_dates: None,
            speaker: Some(alice.clone()),
            assumed_premise_ids: Vec::new(),
            quote: ModelQuoteAnchor {
                chunk_id: "sess_0".into(),
                verbatim_quote: "yes".into(),
                turn_ordinal: Some(1),
                approx_char_offset: None,
            },
            evidence: Some(ModelQuoteAnchor {
                chunk_id: "sess_0".into(),
                verbatim_quote: "forbid git push --force on main".into(),
                turn_ordinal: Some(0),
                approx_char_offset: None,
            }),
            write_op: ModelWriteOp::Add,
            command_guard: None,
        }],
        &NlFunnelOptions::personal_or_shared(
            AudienceSpec::Explicit(BTreeSet::from([alice.clone()])),
            EpochMs(t0.0 + 11_000),
        ),
    )?;
    assert_eq!(sess_out.created_fact_ids.len(), 1);
    assert_eq!(
        store.derivation_view()?.facts[&sess_out.created_fact_ids[0]].author,
        alice
    );
    Ok(())
}

#[test]
fn multi_job_window_isolates_scopes_and_sessions_and_rolls_back_on_concurrent_retire() -> Result {
    let (_dir, mut store) = wave2a_community_store()?;
    let alice: ActorId = "alice".parse()?;
    let carol: ActorId = "carol".parse()?;
    let t0 = EpochMs(1_795_089_600_000);
    let shanghai = TimeZone::get("Asia/Shanghai")?;
    let calendar = CalendarTable::from_reference_in_tz(t0, &shanghai)?;

    let scope_a: SourceScopeId = "feishu.chat.room_a".parse()?;
    let scope_b: SourceScopeId = "feishu.chat.room_b".parse()?;
    store.apply_public_container_roster(
        &alice,
        &PublicContainerRoster {
            container_id: scope_a.to_string(),
            label: "Room A".into(),
            members: vec!["alice".into(), "bob".into(), "carol".into()],
            all_library_members: false,
            synced_at: t0,
            provider: Some("feishu".into()),
            upstream_container_id: Some("room_a".into()),
        },
    )?;
    store.apply_public_container_roster(
        &alice,
        &PublicContainerRoster {
            container_id: scope_b.to_string(),
            label: "Room B".into(),
            members: vec!["alice".into(), "bob".into()],
            all_library_members: false,
            synced_at: t0,
            provider: Some("feishu".into()),
            upstream_container_id: Some("room_b".into()),
        },
    )?;

    for (ext_id, scope, spk, body, ts) in [
        (
            "a_msg_1",
            &scope_a,
            "alice",
            "Canary rollout freezes on Fridays.",
            t0.0 + 100,
        ),
        (
            "a_msg_2",
            &scope_a,
            "bob",
            "Staging soak runs for two hours.",
            t0.0 + 200,
        ),
        (
            "b_msg_1",
            &scope_b,
            "alice",
            "Private Room B billing migration note.",
            t0.0 + 250,
        ),
        (
            "a_msg_3",
            &scope_a,
            "carol",
            "[speaker:carol] Tagged session-like payload must never join chat window.",
            t0.0 + 260,
        ),
        (
            "a_msg_4",
            &scope_a,
            "carol",
            "Rollback playbook stays in docs/rollback.md.",
            t0.0 + 300,
        ),
    ] {
        store.apply_public_ingest_record(
            &PublicIngestRecord {
                source: PublicIngestSource {
                    kind: "feishu".into(),
                    uri: format!("feishu://chat/{ext_id}"),
                    external_id: ext_id.into(),
                    container_id: Some(scope.to_string()),
                },
                speaker: PublicSpeaker::new(spk, None),
                audience: PublicAudience::ContainerMembers,
                occurred_at: EpochMs(ts),
                content: body.into(),
                op: PublicIngestOp::Upsert,
                ingested_by: Some(alice.clone()),
                media_type: Some("text/plain".into()),
                model_policy: None,
                thread_key: Some("shared-thread-key".into()),
                reply_to: None,
            },
            &alice,
            &ModelRoutePolicy::HostCli,
        )?;
    }

    let win = store
        .lease_extraction_window(EpochMs(t0.0 + 400), 60_000, DEFAULT_MAX_BATCH_TOKENS)?
        .ok_or("expected scope A chat window")?;
    assert_eq!(
        win.jobs.len(),
        3,
        "window must batch a_msg_1, a_msg_2, a_msg_4 while excluding cross-scope b_msg_1 and tagged a_msg_3"
    );
    assert_eq!(win.source_scope_id, Some(scope_a.clone()));
    assert!(win
        .slices
        .iter()
        .all(|s| !s.body.contains("Room B") && !s.body.contains("[speaker:carol]")));

    let req = build_extraction_batch_request(
        &store,
        win.slices[0].source_id.clone(),
        alice.clone(),
        calendar,
        Vec::new(),
        win.slices.clone(),
        5,
    )?;
    let mut opts =
        NlFunnelOptions::personal_or_shared(win.readable_by.clone(), EpochMs(t0.0 + 400));
    opts.source_origin = Some(SourceOriginMeta {
        speaker: alice.clone(),
        ingested_by: alice.clone(),
        source_scope_id: scope_a.clone(),
    });
    let (_out, staged) = stage_extracted_atoms(
        &store,
        &req,
        &[ModelExtractedAtom {
            kind: FactKind::Decision,
            claim_text: "Canary rollout freezes on Fridays".into(),
            value: json!({}),
            scope: None,
            ambiguous_scope: None,
            condition_concept: Some(ModelConceptRef::ProposeNew {
                kind: ConceptKind::Condition,
                label: "Friday rollout".into(),
                domain: "release".into(),
                polarity: ConceptPolarity::Negative,
            }),
            action_concept: None,
            group_concept: None,
            member_mention: None,
            valid_window: None,
            valid_dates: None,
            speaker: Some(alice.clone()),
            assumed_premise_ids: Vec::new(),
            quote: ModelQuoteAnchor {
                chunk_id: win.slices[0].chunk_id.clone(),
                verbatim_quote: "Canary rollout freezes on Fridays.".into(),
                turn_ordinal: None,
                approx_char_offset: None,
            },
            evidence: None,
            write_op: ModelWriteOp::Add,
            command_guard: None,
        }],
        &opts,
    )?;

    store.apply_public_ingest_record(
        &PublicIngestRecord {
            source: PublicIngestSource {
                kind: "feishu".into(),
                uri: "feishu://chat/a_msg_2".into(),
                external_id: "a_msg_2".into(),
                container_id: Some(scope_a.to_string()),
            },
            speaker: PublicSpeaker::new("bob", None),
            audience: PublicAudience::ContainerMembers,
            occurred_at: EpochMs(t0.0 + 450),
            content: String::new(),
            op: PublicIngestOp::Withdraw,
            ingested_by: Some(alice.clone()),
            media_type: Some("text/plain".into()),
            model_policy: None,
            thread_key: Some("shared-thread-key".into()),
            reply_to: None,
        },
        &alice,
        &ModelRoutePolicy::HostCli,
    )?;

    let job_specs: Vec<(String, u32)> = win
        .jobs
        .iter()
        .map(|j| (j.job_id.clone(), j.attempts))
        .collect();
    let hashes: Vec<(ContentHash, ActorId)> = win
        .slices
        .iter()
        .map(|s| (ContentHash::digest(&s.body), s.author.clone()))
        .collect();

    assert!(matches!(
        store.commit_staged_extraction_job_fenced(&FencedJobCommitSpec {
            jobs: &job_specs,
            lease_now: EpochMs(t0.0 + 500),
            batch: &staged,
            chunk_hashes: &hashes,
            schema_ver: "m1-nl-v1",
            at: EpochMs(t0.0 + 500),
        }),
        Err(StoreError::Conflict)
    ));

    assert!(store.derivation_view()?.facts.is_empty());
    assert!(store.list_active_concepts()?.is_empty());
    assert!(!store.is_chunk_extracted(&hashes[0].0, &alice, "m1-nl-v1")?);
    assert!(!store.is_chunk_extracted(&hashes[2].0, &carol, "m1-nl-v1")?);
    Ok(())
}

#[test]
fn command_guard_flattens_onto_constraint_and_resource_scope_while_stripping_non_guard_or_untrusted(
) -> Result {
    let (_dir, mut store) = wave2a_community_store()?;
    let alice: ActorId = "alice".parse()?;
    let t0 = EpochMs(1_795_089_600_000);
    let shanghai = TimeZone::get("Asia/Shanghai")?;
    let calendar = CalendarTable::from_reference_in_tz(t0, &shanghai)?;

    let body = "Never run pip install; use uv pip install, and protect src/lib.rs. Node is 22.";
    let out_src = store.upsert_governed_source(GovernedSourceInput {
        external_source_key: "file:///notes/guards.md".into(),
        uri: "file:///notes/guards.md".into(),
        media_type: "text/markdown".into(),
        bytes: body.as_bytes().to_vec(),
        chunks: vec![ChunkInput {
            heading: "Guards".into(),
            body: body.into(),
            anchor: AnchorSpec::default(),
        }],
        speaker: alice.clone(),
        ingested_by: alice.clone(),
        readable_by: AudienceSpec::AllMembers,
        source_scope_id: None,
        effective_policy: ModelRoutePolicy::HostCli,
        op_kind: GovernedUpsertMode::AutoSupersede,
        thread_key: None,
        reply_to_key: None,
        captured_at: t0,
    })?;
    let sid = out_src.source.ok_or("source")?.source_id;
    let cid = format!("{sid}-0");
    let slice = kp_model::ExtractionChunkSlice {
        chunk_id: cid.clone(),
        source_id: sid.clone(),
        author: alice.clone(),
        base_anchor: AnchorSpec::default(),
        role: kp_model::SliceRole::Target,
        reply_to_chunk_id: None,
        heading: "Guards".into(),
        body: body.into(),
    };
    let req = build_extraction_batch_request(
        &store,
        sid,
        alice.clone(),
        calendar,
        Vec::new(),
        vec![slice],
        5,
    )?;

    let guard_spec = kp_model::CommandGuardSpec {
        forbidden_commands: vec!["pip install".into()],
        allowed_commands: vec!["uv pip install".into()],
        required_flags: vec!["--locked".into()],
    };
    let applied = apply_extracted_atoms(
        &mut store,
        &req,
        &[
            ModelExtractedAtom {
                kind: FactKind::Constraint,
                claim_text: "Forbid pip install and protect src/lib.rs".into(),
                value: json!({"note": "packaging"}),
                scope: Some(kp_model::ModelScopeRef::Resource {
                    target: "src/lib.rs".into(),
                }),
                ambiguous_scope: None,
                condition_concept: None,
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: None,
                speaker: Some(alice.clone()),
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: cid.clone(),
                    verbatim_quote:
                        "Never run pip install; use uv pip install, and protect src/lib.rs.".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                evidence: None,
                command_guard: Some(guard_spec.clone()),
                write_op: ModelWriteOp::Add,
            },
            ModelExtractedAtom {
                kind: FactKind::Observation,
                claim_text: "Node is 22".into(),
                value: json!({"node": 22}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: None,
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: None,
                speaker: Some(alice),
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: cid,
                    verbatim_quote: "Node is 22.".into(),
                    turn_ordinal: None,
                    approx_char_offset: None,
                },
                evidence: None,
                command_guard: Some(guard_spec),
                write_op: ModelWriteOp::Add,
            },
        ],
        &NlFunnelOptions::personal_or_shared(AudienceSpec::AllMembers, t0),
    )?;
    assert_eq!(applied.created_fact_ids.len(), 2);
    let graph = store.derivation_view()?;
    let cf = &graph.facts[&applied.created_fact_ids[0]];
    assert_eq!(
        cf.governed_scope,
        Some(kp_model::GovernedScope::Resource {
            target: "src/lib.rs".into()
        })
    );
    assert_eq!(cf.object["hard_guard"], json!(true));
    assert_eq!(cf.object["forbidden_commands"], json!(["pip install"]));
    assert_eq!(cf.object["allowed_commands"], json!(["uv pip install"]));
    assert_eq!(cf.object["required_flags"], json!(["--locked"]));

    let obs = &graph.facts[&applied.created_fact_ids[1]];
    assert!(obs.object.get("forbidden_commands").is_none());
    assert!(obs.object.get("hard_guard").is_none());
    Ok(())
}
