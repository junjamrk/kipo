use std::task::{Context, Poll, Waker};

use serde_json::json;

use crate::{
    format_tagged_turns, parse_tagged_turns, ActiveDecisionBrief, ActorId, CalendarTable,
    ChunkTurn, CommunityConcept, ConceptKind, ConceptPolarity, DeterministicMockLlm,
    EntailmentQuery, EntailmentVerdict, EpochMs, ExtractionBatchOutcome, ExtractionBatchRequest,
    ExtractionChunkSlice, FactKind, KpError, LlmClient, ModelConceptRef, ModelDateRange,
    ModelExtractedAtom, ModelQuoteAnchor, ModelScopeRef, ModelWriteOp, Result, SpeakerFactBrief,
};

fn poll_ready<T>(
    mut future: std::pin::Pin<Box<dyn std::future::Future<Output = Result<T>> + Send + '_>>,
) -> Result<T> {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    match future.as_mut().poll(&mut cx) {
        Poll::Ready(output) => output,
        Poll::Pending => Err(KpError::invalid("mock_llm", "unexpected pending future")),
    }
}

#[test]
fn deterministic_mock_llm_defaults_to_unrelated_and_skipped_empty() -> Result<()> {
    let mock = DeterministicMockLlm::default();
    mock.register_entailment("服务器宕机", "服务不可用", EntailmentVerdict::Entails)?;
    let hit = poll_ready(mock.judge_entailment(&EntailmentQuery {
        left_label: "服务器宕机".into(),
        left_domain: "ops".into(),
        right_label: "服务不可用".into(),
        right_domain: "ops".into(),
        left_quote: None,
        right_quote: None,
    }))?;
    assert_eq!(hit, EntailmentVerdict::Entails);

    let miss = poll_ready(mock.judge_entailment(&EntailmentQuery {
        left_label: "上海车展".into(),
        left_domain: "event".into(),
        right_label: "不吃海鲜".into(),
        right_domain: "diet".into(),
        left_quote: None,
        right_quote: None,
    }))?;
    assert_eq!(miss, EntailmentVerdict::Unrelated);

    let req = ExtractionBatchRequest {
        source_id: "src_1".parse()?,
        speaker: "alice".parse()?,
        reference_calendar: CalendarTable::from_reference_in_tz(EpochMs(0), &crate::TimeZone::UTC)?,
        active_concepts: Vec::new(),
        active_decisions: Vec::new(),
        speaker_own_facts: Vec::new(),
        speaker_own_facts_by_actor: Default::default(),
        chunks: vec![ExtractionChunkSlice {
            chunk_id: "c0".into(),
            source_id: "src_1".parse()?,
            author: "alice".parse()?,
            base_anchor: Default::default(),
            role: Default::default(),
            reply_to_chunk_id: None,
            heading: String::new(),
            body: "好的".into(),
        }],
        min_rate_limit_remaining_pct: 10,
    };
    let out = poll_ready(mock.extract_batch(&req))?;
    assert_eq!(out, ExtractionBatchOutcome::SkippedEmpty);
    Ok(())
}

#[test]
fn narrow_model_boundary_rejects_unapproved_concepts_premises_and_foreign_updates() -> Result<()> {
    let book_club_concept = CommunityConcept {
        id: "cpt_group_book_club".parse()?,
        kind: ConceptKind::Group,
        canonical_label: "读书会".into(),
        domain: "social".into(),
        polarity: ConceptPolarity::Neutral,
        proposed_by: "bob".parse()?,
        created_at: EpochMs(10),
    };
    let req = ExtractionBatchRequest {
        source_id: "src_b".parse()?,
        speaker: "bob".parse()?,
        reference_calendar: CalendarTable::from_reference_in_tz(
            EpochMs(0),
            &crate::TimeZone::get("Asia/Shanghai")
                .map_err(|_| crate::KpError::invalid("tz", "tz"))?,
        )?,
        active_concepts: vec![book_club_concept],
        active_decisions: vec![ActiveDecisionBrief {
            fact_id: "dec_postgres".parse()?,
            subject: "kp".into(),
            predicate: "arch.engine".into(),
            value: json!("postgres"),
        }],
        speaker_own_facts: vec![SpeakerFactBrief {
            fact_id: "bob_old_rule".parse()?,
            kind: FactKind::Constraint,
            summary: "旧规矩".into(),
        }],
        speaker_own_facts_by_actor: Default::default(),
        chunks: vec![ExtractionChunkSlice {
            chunk_id: "c1".into(),
            source_id: "src_b".parse()?,
            author: "bob".parse()?,
            base_anchor: Default::default(),
            role: Default::default(),
            reply_to_chunk_id: None,
            heading: String::new(),
            body: "我们读书会每次活动都不迟到。".into(),
        }],
        min_rate_limit_remaining_pct: 10,
    };

    let valid_atom = ModelExtractedAtom {
        kind: FactKind::Constraint,
        claim_text: "活动不迟到".into(),
        value: json!({"action": "arrive_on_time"}),
        scope: Some(ModelScopeRef::RelationGroup {
            group_concept: ModelConceptRef::Existing {
                concept_id: "cpt_group_book_club".parse()?,
            },
        }),
        ambiguous_scope: None,
        condition_concept: Some(ModelConceptRef::ProposeNew {
            kind: ConceptKind::Condition,
            label: "读书会活动".into(),
            domain: "club".into(),
            polarity: ConceptPolarity::Neutral,
        }),
        action_concept: Some(ModelConceptRef::ProposeNew {
            kind: ConceptKind::Action,
            label: "不迟到".into(),
            domain: "travel".into(),
            polarity: ConceptPolarity::Negative,
        }),
        group_concept: None,
        member_mention: None,
        valid_window: None,
        valid_dates: None,
        speaker: None,
        assumed_premise_ids: vec!["dec_postgres".parse()?],
        quote: ModelQuoteAnchor {
            chunk_id: "c1".into(),
            verbatim_quote: "我们读书会每次活动都不迟到".into(),
            turn_ordinal: None,
            approx_char_offset: None,
        },
        evidence: None,
        write_op: ModelWriteOp::SupersedeOwn {
            target_fact_id: "bob_old_rule".parse()?,
        },
        command_guard: None,
    };
    let verified = req.validate_model_atom(&valid_atom)?;
    assert_eq!(verified.exact_quote, "我们读书会每次活动都不迟到");

    // 1) Invented concept ID not in active_concepts -> rejected
    let mut bad_concept = valid_atom.clone();
    bad_concept.scope = Some(ModelScopeRef::RelationGroup {
        group_concept: ModelConceptRef::Existing {
            concept_id: "cpt_invented".parse()?,
        },
    });
    assert!(req.validate_model_atom(&bad_concept).is_err());

    // 2) Assumed premise absent from active_decisions -> rejected
    let mut bad_premise = valid_atom.clone();
    bad_premise.assumed_premise_ids = vec!["fact_arbitrary".parse()?];
    assert!(req.validate_model_atom(&bad_premise).is_err());

    // 3) SupersedeOwn targeting someone else's fact -> rejected
    let mut bad_update = valid_atom;
    bad_update.write_op = ModelWriteOp::SupersedeOwn {
        target_fact_id: "alice_fact".parse()?,
    };
    assert!(req.validate_model_atom(&bad_update).is_err());
    Ok(())
}

fn tagged_request(body: String) -> Result<ExtractionBatchRequest> {
    Ok(ExtractionBatchRequest {
        source_id: "src_team".parse()?,
        speaker: "dave".parse()?,
        reference_calendar: CalendarTable::from_reference_in_tz(
            EpochMs(1_795_089_600_000),
            &crate::TimeZone::get("Asia/Shanghai")
                .map_err(|_| crate::KpError::invalid("tz", "tz"))?,
        )?,
        active_concepts: Vec::new(),
        active_decisions: Vec::new(),
        speaker_own_facts: Vec::new(),
        speaker_own_facts_by_actor: std::collections::BTreeMap::from([(
            "erin".parse()?,
            vec![SpeakerFactBrief {
                fact_id: "erin_old_rule".parse()?,
                kind: FactKind::Constraint,
                summary: "旧的报销规矩".into(),
            }],
        )]),
        chunks: vec![ExtractionChunkSlice {
            chunk_id: "t0".into(),
            source_id: "src_team".parse()?,
            author: "dave".parse()?,
            base_anchor: Default::default(),
            role: Default::default(),
            reply_to_chunk_id: None,
            heading: "team chat".into(),
            body,
        }],
        min_rate_limit_remaining_pct: 10,
    })
}

fn quote_atom(quote: &str, speaker: Option<&str>) -> Result<ModelExtractedAtom> {
    Ok(ModelExtractedAtom {
        kind: FactKind::Constraint,
        claim_text: quote.into(),
        value: json!(null),
        scope: Some(ModelScopeRef::SpeakerSelf),
        ambiguous_scope: None,
        condition_concept: None,
        action_concept: None,
        group_concept: None,
        member_mention: None,
        valid_window: None,
        valid_dates: None,
        speaker: speaker.map(str::parse::<ActorId>).transpose()?,
        assumed_premise_ids: Vec::new(),
        quote: ModelQuoteAnchor {
            chunk_id: "t0".into(),
            verbatim_quote: quote.into(),
            turn_ordinal: None,
            approx_char_offset: None,
        },
        evidence: None,
        write_op: ModelWriteOp::Add,
        command_guard: None,
    })
}

#[test]
fn tagged_turns_attribute_quotes_to_human_speakers_and_reject_assistant_text() -> Result<()> {
    let body = format_tagged_turns(&[
        ChunkTurn {
            speaker: Some("dave".parse()?),
            is_assistant: false,
            text: "下周二我去深圳参加客户会".into(),
        },
        ChunkTurn {
            speaker: None,
            is_assistant: true,
            text: "建议报销一律走线上审批".into(),
        },
        ChunkTurn {
            speaker: Some("erin".parse()?),
            is_assistant: false,
            text: "同意，以后报销一律走线上审批\n别再交纸质单".into(),
        },
    ]);
    let turns = parse_tagged_turns(&body);
    assert_eq!(turns.len(), 3);
    assert!(turns[1].is_assistant);
    assert_eq!(turns[2].speaker, Some("erin".parse()?));

    let req = tagged_request(body)?;
    assert_eq!(req.batch_speakers().len(), 2);

    // Dave's own turn -> attributed to dave (model omitted speaker).
    let (_, who) =
        req.validate_model_atom_attributed(&quote_atom("下周二我去深圳参加客户会", None)?)?;
    assert_eq!(who.as_str(), "dave");
    // Erin's agreement turn -> attributed to erin; flattened newline stays inside her turn.
    let (_, who) = req.validate_model_atom_attributed(&quote_atom(
        "以后报销一律走线上审批 别再交纸质单",
        Some("erin"),
    )?)?;
    assert_eq!(who.as_str(), "erin");
    // A quote that exists ONLY in the assistant turn is rejected; claiming a human speaker on it
    // is classified as a `"speaker"` attribution violation.
    assert!(req
        .validate_model_atom(&quote_atom("建议报销一律走线上审批", None)?)
        .is_err());
    assert!(matches!(
        req.validate_model_atom(&quote_atom("建议报销一律走线上审批", Some("dave"))?),
        Err(KpError::InvalidInput {
            field: "speaker",
            ..
        })
    ));
    // Model-claimed speaker that differs from the quoted turn's speaker is rejected.
    assert!(req
        .validate_model_atom(&quote_atom("下周二我去深圳参加客户会", Some("erin"))?)
        .is_err());
    // Quotes containing the tag itself or crossing turns are rejected.
    assert!(req
        .validate_model_atom(&quote_atom("[speaker:dave] 下周二", None)?)
        .is_err());

    // SupersedeOwn is checked against the ATTRIBUTED speaker's own facts.
    let mut erin_update = quote_atom("以后报销一律走线上审批", None)?;
    erin_update.write_op = ModelWriteOp::SupersedeOwn {
        target_fact_id: "erin_old_rule".parse()?,
    };
    req.validate_model_atom(&erin_update)?;
    let mut dave_steals = quote_atom("下周二我去深圳参加客户会", None)?;
    dave_steals.write_op = ModelWriteOp::SupersedeOwn {
        target_fact_id: "erin_old_rule".parse()?,
    };
    assert!(req.validate_model_atom(&dave_steals).is_err());

    // Calendar date ranges resolve deterministically; garbage dates are rejected.
    let mut dated = quote_atom("下周二我去深圳参加客户会", None)?;
    dated.valid_dates = Some(ModelDateRange {
        start_date_iso: "2026-11-24".into(),
        end_date_iso: None,
        open_ended: false,
    });
    let window = req
        .resolve_atom_window(&dated)?
        .ok_or_else(|| KpError::invalid("t", "w"))?;
    assert_eq!(
        Some(window),
        req.reference_calendar.window_for_date("2026-11-24")
    );
    dated.valid_dates = Some(ModelDateRange {
        start_date_iso: "next tuesday-ish".into(),
        end_date_iso: None,
        open_ended: false,
    });
    assert!(req.validate_model_atom(&dated).is_err());
    Ok(())
}

#[test]
fn token_budget_partitioning_packs_short_turns_and_splits_long_turns_without_dropping() -> Result<()>
{
    let alice: crate::ActorId = "alice".parse()?;
    let bob: crate::ActorId = "bob".parse()?;

    // 50 short turns fit comfortably inside `DEFAULT_MAX_BATCH_TOKENS` (1 single batch!).
    let short_turns: Vec<crate::ChunkTurn> = (0..50)
        .map(|idx| crate::ChunkTurn {
            speaker: Some(if idx % 2 == 0 {
                alice.clone()
            } else {
                bob.clone()
            }),
            is_assistant: false,
            text: format!("第{idx}轮读书会签到确认。"),
        })
        .collect();
    let packed =
        crate::partition_turns_by_token_budget(&short_turns, crate::DEFAULT_MAX_BATCH_TOKENS);
    assert_eq!(packed.len(), 1);
    assert_eq!(packed[0].len(), 50);

    // Under a tight 60-token cap, the same 50 turns split into multiple token-bounded batches,
    // preserving exact order and total turn count.
    let split = crate::partition_turns_by_token_budget(&short_turns, 60);
    assert!(split.len() > 5);
    let flattened: Vec<_> = split.iter().flatten().cloned().collect();
    assert_eq!(flattened, short_turns);
    for batch in &split {
        let rendered = crate::format_tagged_turns(batch);
        assert!(crate::estimate_batch_tokens(&rendered) <= 60);
    }

    // A single oversized turn exceeding `max_batch_tokens` still forms its own 1-turn batch.
    let huge_turn = crate::ChunkTurn {
        speaker: Some(alice),
        is_assistant: false,
        text: "长段会议纪要内容整理。".repeat(30),
    };
    let with_oversized = vec![
        short_turns[0].clone(),
        huge_turn.clone(),
        short_turns[1].clone(),
    ];
    let partitioned = crate::partition_turns_by_token_budget(&with_oversized, 80);
    assert_eq!(partitioned.len(), 3);
    assert_eq!(partitioned[1], vec![huge_turn]);
    Ok(())
}
