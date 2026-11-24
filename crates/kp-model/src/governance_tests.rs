use std::collections::BTreeSet;

use serde_json::json;

use crate::{
    format_opaque_id, is_bare_content_hash, test_support, verify_verbatim_quote,
    verify_verbatim_substring, ActorId, AudienceSpec, CalendarTable, CharSpan, ConceptId,
    ContentHash, EpochMs, FactKind, GovernedScope, MembershipRelation, QuoteLocationHint,
    RejectionEvidenceKey, RejectionRecord, RejectionSemanticKey, Result, SourceScope,
    SourceScopeId, ValidityWindow,
};

#[test]
fn verbatim_quote_normalizes_width_and_whitespace_but_returns_exact_original_slice() -> Result<()> {
    let chunk = "Alice：  周五（11/22）健身房闭馆检修！\n请大家改天再去。";
    let matched = verify_verbatim_quote(
        chunk,
        "周五(11/22)健身房闭馆检修! 请大家改天再去。",
        QuoteLocationHint::default(),
    )?;
    assert_eq!(
        matched.exact_quote,
        "周五（11/22）健身房闭馆检修！\n请大家改天再去。"
    );
    assert_eq!(
        &chunk[matched.byte_start as usize..matched.byte_end as usize],
        matched.exact_quote
    );
    assert!(verify_verbatim_substring(chunk, "周五健身房检修").is_err());
    assert!(verify_verbatim_substring(chunk, "   ").is_err());
    Ok(())
}

#[test]
fn repeated_verbatim_quotes_require_turn_or_offset_disambiguation() -> Result<()> {
    let chunk = "Turn 0: 出差只坐高铁\n\nTurn 1: 我们组规定，出差只坐高铁";
    assert!(verify_verbatim_quote(chunk, "出差只坐高铁", QuoteLocationHint::default()).is_err());

    let by_turn = verify_verbatim_quote(
        chunk,
        "出差只坐高铁",
        QuoteLocationHint {
            turn_ordinal: Some(1),
            approx_char_offset: None,
        },
    )?;
    assert!(by_turn.span.char_start > 15);
    assert_eq!(by_turn.exact_quote, "出差只坐高铁");

    let by_offset = verify_verbatim_quote(
        chunk,
        "出差只坐高铁",
        QuoteLocationHint {
            turn_ordinal: None,
            approx_char_offset: Some(28),
        },
    )?;
    assert_eq!(by_offset.span, by_turn.span);
    Ok(())
}

#[test]
fn dual_key_rejection_catches_overlapping_quote_spans_even_when_model_rewords_claim() -> Result<()>
{
    let speaker: ActorId = "bob".parse()?;
    let chunk_hash = ContentHash::digest("bob: 周五别去健身房，正在检修");
    let gym_buddies_concept: ConceptId = "cpt_group_gym_buddies".parse()?;
    let gym_closed_concept: ConceptId = "cpt_cond_gym_closed".parse()?;
    let skip_gym_concept: ConceptId = "cpt_act_skip_gym".parse()?;

    let record = RejectionRecord {
        id: "rej_001".parse()?,
        rejected_by: speaker.clone(),
        evidence_key: Some(RejectionEvidenceKey {
            speaker: speaker.clone(),
            chunk_sha256: chunk_hash.clone(),
            quote_span: CharSpan::new(5, 10)?,
        }),
        semantic_key: Some(RejectionSemanticKey {
            kind: FactKind::Constraint,
            concept_ids: BTreeSet::from([gym_closed_concept.clone(), skip_gym_concept.clone()]),
            governed_scope: Some(GovernedScope::RelationGroup {
                anchor: speaker.clone(),
                group_concept_id: gym_buddies_concept.clone(),
            }),
            valid_window: ValidityWindow {
                valid_from: EpochMs(100),
                valid_until: EpochMs(200),
            },
        }),
        replacement_fact_id: None,
        reason: "这条记错了".into(),
        created_at: EpochMs(150),
    };
    record.validate()?;

    // Re-extraction rewords the claim text and captures a slightly wider overlapping span [4, 15).
    let reworded_candidate = RejectionEvidenceKey {
        speaker: speaker.clone(),
        chunk_sha256: chunk_hash,
        quote_span: CharSpan::new(4, 15)?,
    };
    assert!(record.matches_candidate(Some(&reworded_candidate), None));

    // Different chunk, no overlapping quote, but identical normalized semantic key.
    let same_semantic = RejectionSemanticKey {
        kind: FactKind::Constraint,
        concept_ids: BTreeSet::from([skip_gym_concept, gym_closed_concept]),
        governed_scope: Some(GovernedScope::RelationGroup {
            anchor: speaker,
            group_concept_id: gym_buddies_concept,
        }),
        valid_window: ValidityWindow {
            valid_from: EpochMs(100),
            valid_until: EpochMs(200),
        },
    };
    assert!(record.matches_candidate(None, Some(&same_semantic)));
    Ok(())
}

#[test]
fn live_source_scope_governs_read_time_semiring_and_publish_ceiling() -> Result<()> {
    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;
    let carol: ActorId = "carol".parse()?;
    let scope_id: SourceScopeId = "ext_chat.oc_release".parse()?;
    let members = BTreeSet::from([alice.clone(), bob.clone(), carol.clone()]);

    let mut scope = SourceScope {
        scope_id: scope_id.clone(),
        provider: "ext_chat".into(),
        external_container_id: "oc_release".into(),
        label: "研发发布群".into(),
        current_audience: AudienceSpec::Explicit(BTreeSet::from([alice.clone(), bob.clone()])),
        synced_at: EpochMs(10),
    };
    scope.validate_publish_audience(
        &AudienceSpec::Explicit(BTreeSet::from([alice.clone()])),
        &members,
    )?;
    assert!(scope
        .validate_publish_audience(
            &AudienceSpec::Explicit(BTreeSet::from([alice.clone(), carol.clone()])),
            &members,
        )
        .is_err());

    let mut base = test_support::fact("freeze", &["alice", "bob", "carol"], 0, 100)?;
    base.source_scope_id = Some(scope_id.clone());
    let conclusion = test_support::derived("blocked_merge")?;
    let proof = test_support::justification("j1", &conclusion, &[&base])?;
    let mut graph = test_support::graph(vec![base, conclusion.clone()], vec![proof]);
    graph.members = members.clone();
    graph
        .source_scopes
        .insert(scope_id.clone(), scope.current_audience.clone());

    assert!(graph
        .evaluate(&bob, EpochMs(50))
        .facts
        .contains(&conclusion.id));
    assert!(!graph
        .evaluate(&carol, EpochMs(50))
        .facts
        .contains(&conclusion.id));

    // Bob leaves the external container: updating `source_scopes` hides both base fact and derived conclusion!
    scope.current_audience = AudienceSpec::Explicit(BTreeSet::from([alice.clone()]));
    graph.source_scopes.insert(scope_id, scope.current_audience);
    assert!(!graph
        .evaluate(&bob, EpochMs(50))
        .facts
        .contains(&conclusion.id));
    assert!(graph
        .evaluate(&alice, EpochMs(50))
        .facts
        .contains(&conclusion.id));
    Ok(())
}

#[test]
fn membership_facts_and_relation_groups_share_typed_group_concept_id() -> Result<()> {
    let mut m = test_support::fact("m1", &["B"], 0, 100)?;
    m.kind = FactKind::Membership;
    m.object = json!({"anchor": "B", "member": "A", "free_text": "family"});
    assert!(m.validate().is_err());

    let valid_rel = MembershipRelation {
        anchor: "B".parse()?,
        member: "A".parse()?,
        group_concept_id: "cpt_group_gym_buddies".parse()?,
    };
    m.object = serde_json::to_value(&valid_rel)
        .map_err(|_| crate::KpError::invalid("json", "serialization failed"))?;
    m.validate()?;

    let digest = ContentHash::digest("sensitive claim");
    assert!(is_bare_content_hash(digest.as_str()));
    let opaque = format_opaque_id("fact", "01934e7a_abcd")?;
    assert!(!is_bare_content_hash(&opaque));

    let shanghai = crate::TimeZone::get("Asia/Shanghai")
        .map_err(|_| crate::KpError::invalid("test", "Asia/Shanghai"))?;
    let table = CalendarTable::from_reference_in_tz(EpochMs(1_795_099_200_000), &shanghai)?;
    assert_eq!(table.reference_date.weekday_iso, 4);
    Ok(())
}
