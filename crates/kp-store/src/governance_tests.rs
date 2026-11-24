use std::collections::BTreeSet;

use kp_model::{
    is_bare_content_hash, AnchorSpec, CharSpan, CommunityConcept, ConceptKind, ConceptPolarity,
    ContentHash, DerivationMode, EpochMs, FactAccess, FactKind, FactStatus, GovernedScope, KpError,
    LibraryMode, RejectionSemanticKey, ReviewItem, ReviewKind, ReviewStatus, ValidityWindow,
};

use super::*;
use crate::test_support::{derived, fact, justification, library, source, AT};
use crate::{ChunkInput, StoreError};

#[test]
fn prefixed_opaque_ids_never_expose_bare_hashes_and_reject_raw_digests() -> Result<()> {
    let fact_id = new_fact_id()?;
    let src_id = new_source_id()?;
    let rev_id = new_review_id()?;
    let rej_id = new_rejection_id()?;
    let cpt_id = new_concept_id()?;
    let jst_id = new_justification_id()?;
    let job_id = new_job_id()?;

    assert!(fact_id.as_str().starts_with("fact_"));
    assert!(src_id.as_str().starts_with("src_"));
    assert!(rev_id.as_str().starts_with("rev_"));
    assert!(rej_id.as_str().starts_with("rej_"));
    assert!(cpt_id.as_str().starts_with("cpt_"));
    assert!(jst_id.as_str().starts_with("jst_"));
    assert!(job_id.starts_with("job_"));

    for s in [
        fact_id.as_str(),
        src_id.as_str(),
        rev_id.as_str(),
        rej_id.as_str(),
        cpt_id.as_str(),
        jst_id.as_str(),
        job_id.as_str(),
    ] {
        assert!(!is_bare_content_hash(s), "unexpected bare digest: {s}");
    }
    assert_ne!(fact_id, new_fact_id()?);

    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let bare_hex = ContentHash::digest("sensitive-raw-text").to_string();
    let bad_concept = CommunityConcept {
        id: bare_hex.parse()?,
        kind: ConceptKind::Condition,
        canonical_label: "膝盖酸痛".into(),
        domain: "fitness".into(),
        polarity: ConceptPolarity::Neutral,
        proposed_by: "owner".parse()?,
        created_at: AT,
    };
    assert!(matches!(
        store.upsert_community_concept(&bad_concept),
        Err(StoreError::Model(KpError::InvalidInput {
            field: "concept_id",
            ..
        }))
    ));
    Ok(())
}

#[test]
fn community_concepts_crud_and_normalized_lookup() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Community)?;
    let owner: ActorId = "owner".parse()?;

    let soreness = store.ensure_community_concept(
        ConceptKind::Condition,
        "  膝盖酸痛  ",
        "fitness",
        ConceptPolarity::Negative,
        &owner,
        AT,
    )?;
    assert!(soreness.id.as_str().starts_with("cpt_"));

    // Fullwidth/whitespace/case variants resolve to the exact same active concept.
    let soreness_dup = store.ensure_community_concept(
        ConceptKind::Condition,
        "膝盖酸痛",
        "fitness",
        ConceptPolarity::Negative,
        &owner,
        EpochMs(AT.0 + 100),
    )?;
    assert_eq!(soreness.id, soreness_dup.id);

    let cutover = store.ensure_community_concept(
        ConceptKind::Condition,
        "Primary   Cutover",
        "database",
        ConceptPolarity::Neutral,
        &owner,
        AT,
    )?;
    assert_eq!(
        store
            .find_concept_by_label(ConceptKind::Condition, "  Ｐｒｉｍａｒｙ cutover ")?
            .map(|c| c.id),
        Some(cutover.id.clone())
    );
    // Different ConceptKind does not collide on identical text.
    assert!(store
        .find_concept_by_label(ConceptKind::Action, "primary cutover")?
        .is_none());

    assert_eq!(store.list_active_concepts()?.len(), 2);
    assert_eq!(
        store.get_community_concept(&soreness.id)?.map(|c| c.id),
        Some(soreness.id.clone())
    );

    assert!(store.deprecate_community_concept(&soreness.id)?);
    assert!(!store.deprecate_community_concept(&soreness.id)?);
    assert!(store.get_community_concept(&soreness.id)?.is_none());
    assert!(store
        .find_concept_by_label(ConceptKind::Condition, "膝盖酸痛")?
        .is_none());
    assert_eq!(store.list_active_concepts()?.len(), 1);
    Ok(())
}

#[test]
fn forget_allows_future_relearning_while_reject_blocks_reworded_overlapping_extractions(
) -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let owner: ActorId = "owner".parse()?;
    let other_speaker: ActorId = "alice".parse()?;
    let origin = store.library_id().clone();

    let chunk_body = "周三下午不安排评审，而且膝盖酸痛时绝不做负重深蹲。";
    let mut raw = source(&["owner"])?;
    raw.content = chunk_body.as_bytes().to_vec();
    raw.chunks = vec![ChunkInput {
        heading: "训练备注".into(),
        body: chunk_body.into(),
        anchor: AnchorSpec::default(),
    }];
    let stored_src = store.ingest_source(&origin, &"chat-1".parse()?, &raw)?;
    let chunk_id = format!("{}-0", stored_src.source_id);
    let chunk_sha256 = ContentHash::digest(chunk_body);

    let knee_cpt = store.ensure_community_concept(
        ConceptKind::Condition,
        "膝盖酸痛",
        "fitness",
        ConceptPolarity::Neutral,
        &owner,
        AT,
    )?;
    let squat_cpt = store.ensure_community_concept(
        ConceptKind::Action,
        "负重深蹲",
        "fitness",
        ConceptPolarity::Negative,
        &owner,
        AT,
    )?;

    let verified = kp_model::verify_verbatim_quote(
        chunk_body,
        "膝盖酸痛时绝不做负重深蹲",
        QuoteLocationHint::default(),
    )?;

    // 1) `forget` withdraws the existing fact without creating a rejection block.
    let mut forgotten = fact(new_fact_id()?.as_str(), &["owner"])?;
    forgotten.origin_library = origin.clone();
    forgotten.kind = FactKind::Constraint;
    forgotten.object = serde_json::json!({
        "summary": "膝盖酸痛不做负重深蹲",
        "condition_concept_id": knee_cpt.id,
        "action_concept_id": squat_cpt.id,
    });
    forgotten.governed_scope = Some(GovernedScope::SelfOnly {
        actor: owner.clone(),
    });
    forgotten.source_anchor = Some(AnchorSpec {
        source_id: Some(stored_src.source_id.clone()),
        chunk_id: Some(chunk_id.clone()),
        verbatim_quote: Some(verified.exact_quote.clone()),
        char_start: Some(verified.span.char_start),
        char_end: Some(verified.span.char_end),
        ..Default::default()
    });
    store.insert_fact(&forgotten)?;
    store.forget_fact_id(&forgotten.id, &owner, EpochMs(AT.0 + 10))?;
    assert!(store.visible_facts(&owner, EpochMs(AT.0 + 10))?.is_empty());

    let sem_key = RejectionSemanticKey {
        kind: FactKind::Constraint,
        concept_ids: BTreeSet::from([knee_cpt.id.clone(), squat_cpt.id.clone()]),
        governed_scope: Some(GovernedScope::SelfOnly {
            actor: owner.clone(),
        }),
        valid_window: ValidityWindow {
            valid_from: EpochMs(0),
            valid_until: EpochMs::OPEN_END,
        },
    };
    assert!(!store.is_candidate_rejected(&owner, &chunk_sha256, verified.span, Some(&sem_key))?);

    // Re-learning after `forget` succeeds cleanly.
    let mut relearned = forgotten.clone();
    relearned.id = new_fact_id()?;
    relearned.logical_key = relearned.id.as_str().parse()?;
    store.insert_fact(&relearned)?;

    // Attach a downstream derived warning to verify atomic cascade on `reject_fact`.
    let mut warning = derived(new_fact_id()?.as_str())?;
    warning.origin_library = origin.clone();
    warning.kind = FactKind::CollisionWarning;
    store.insert_fact(&warning)?;
    let mut jst = justification(new_justification_id()?.as_str(), &warning, &[&relearned])?;
    jst.mode = DerivationMode::HardRule;
    store.insert_justification(&jst)?;
    assert_eq!(store.visible_facts(&owner, AT)?.len(), 2);

    // 2) `reject_fact` withdraws `relearned`, invalidates `warning`, and records dual keys.
    let rejection =
        store.reject_fact(&relearned.id, &owner, "当时随口一说，已经恢复正常训练", AT)?;
    assert!(rejection.id.as_str().starts_with("rej_"));
    assert!(rejection.evidence_key.is_some());
    assert!(rejection.semantic_key.is_some());
    assert!(store.visible_facts(&owner, AT)?.is_empty());
    let graph = store.derivation_view()?;
    assert_eq!(graph.facts[&relearned.id].status, FactStatus::Withdrawn);
    assert_eq!(graph.facts[&warning.id].status, FactStatus::Invalidated);

    // Even if the LLM completely rewords subject/predicate/object and omits semantic_key,
    // ANY overlapping sub-span on `(owner, chunk_sha256)` is blocked!
    let reworded_subspan =
        kp_model::verify_verbatim_quote(chunk_body, "不做负重深蹲", QuoteLocationHint::default())?
            .span;
    assert!(store.is_candidate_rejected(&owner, &chunk_sha256, reworded_subspan, None)?);

    // Non-overlapping span on the same chunk ("周三下午不安排评审") is NOT blocked.
    let unrelated_span = kp_model::verify_verbatim_quote(
        chunk_body,
        "周三下午不安排评审",
        QuoteLocationHint::default(),
    )?
    .span;
    assert!(!store.is_candidate_rejected(&owner, &chunk_sha256, unrelated_span, None)?);

    // Same span spoken by a different actor on another turn is NOT blocked by owner's evidence key.
    assert!(!store.is_candidate_rejected(&other_speaker, &chunk_sha256, reworded_subspan, None)?);

    // Semantic key blocks re-extraction even on a brand-new chunk with non-overlapping span.
    let other_chunk_hash = ContentHash::digest("另一段训练群记录：膝盖不舒服就不练深蹲。");
    assert!(store.is_candidate_rejected(
        &owner,
        &other_chunk_hash,
        CharSpan::new(0, 4)?,
        Some(&sem_key),
    )?);

    assert_eq!(store.list_rejections(Some(&owner))?.len(), 1);
    Ok(())
}

#[test]
fn reject_fact_with_replacement_supersedes_and_links_replacement() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let owner: ActorId = "owner".parse()?;
    let old = fact(new_fact_id()?.as_str(), &["owner"])?;
    store.insert_fact(&old)?;

    let mut replacement = fact(new_fact_id()?.as_str(), &["owner"])?;
    replacement.object = serde_json::json!("normal");
    if let FactAccess::Base { stamp, .. } = &mut replacement.access {
        stamp.valid.valid_from = EpochMs(50);
    }

    let rec = store.reject_fact_with_options(
        &old.id,
        &owner,
        "纠正排班状态",
        AT,
        RejectOptions {
            replacement: Some(&replacement),
            ..Default::default()
        },
    )?;
    assert_eq!(rec.replacement_fact_id, Some(replacement.id.clone()));
    let graph = store.derivation_view()?;
    assert_eq!(graph.facts[&old.id].status, FactStatus::Superseded);
    assert_eq!(graph.facts[&replacement.id].status, FactStatus::Active);
    Ok(())
}

#[test]
fn review_items_upsert_idempotently_and_isolate_actors() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Community)?;
    let bob: ActorId = "bob".parse()?;
    let alice: ActorId = "alice".parse()?;
    store.add_member(bob.clone(), MemberRole::Member, AT)?;
    store.add_member(alice.clone(), MemberRole::Member, AT)?;
    let target_fact = new_fact_id()?.to_string();

    let first = store.upsert_open_review_item(&ReviewItem {
        id: new_review_id()?,
        actor: bob.clone(),
        kind: ReviewKind::AmbiguousScope,
        target_id: target_fact.clone(),
        question: "只管你自己，还是读书会全员？".into(),
        options: vec!["只管自己".into(), "读书会全员".into()],
        payload: serde_json::json!({"narrow": "self"}),
        status: ReviewStatus::Open,
        created_at: AT,
        resolved_at: None,
    })?;

    // Repeated derivation pass for the same `(actor, kind, target_id)` updates in place without duplicating.
    let second = store.upsert_open_review_item(&ReviewItem {
        id: new_review_id()?,
        actor: bob.clone(),
        kind: ReviewKind::AmbiguousScope,
        target_id: target_fact.clone(),
        question: "只管你自己，还是整个读书会？".into(),
        options: vec!["仅自己".into(), "读书会".into()],
        payload: serde_json::json!({"narrow": "self", "v": 2}),
        status: ReviewStatus::Open,
        created_at: EpochMs(AT.0 + 5_000),
        resolved_at: None,
    })?;
    assert_eq!(first.id, second.id);
    assert_eq!(second.created_at, AT);
    assert_eq!(second.question, "只管你自己，还是整个读书会？");

    assert_eq!(
        store
            .list_review_items(&bob, Some(ReviewStatus::Open))?
            .len(),
        1
    );
    // Alice sees zero of Bob's review items and cannot resolve them.
    assert!(store.list_review_items(&alice, None)?.is_empty());
    assert!(matches!(
        store.resolve_review_item(&first.id, &alice, ReviewStatus::Confirmed, AT),
        Err(StoreError::PermissionDenied)
    ));
    assert!(matches!(
        store.resolve_review_item(&first.id, &bob, ReviewStatus::Open, AT),
        Err(StoreError::Model(KpError::InvalidInput {
            field: "review_status",
            ..
        }))
    ));

    let resolved = store.resolve_review_item(
        &first.id,
        &bob,
        ReviewStatus::Confirmed,
        EpochMs(AT.0 + 9_000),
    )?;
    assert_eq!(resolved.status, ReviewStatus::Confirmed);
    assert_eq!(resolved.resolved_at, Some(EpochMs(AT.0 + 9_000)));
    assert!(store
        .list_review_items(&bob, Some(ReviewStatus::Open))?
        .is_empty());
    assert_eq!(
        store
            .list_review_items(&bob, Some(ReviewStatus::Confirmed))?
            .len(),
        1
    );
    Ok(())
}

#[test]
fn guard_allow_once_enforces_role_visibility_single_use_and_ttl() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Community)?;
    let owner: ActorId = "owner".parse()?;
    let maint: ActorId = "maint".parse()?;
    let member: ActorId = "bob".parse()?;
    store.add_member(maint.clone(), MemberRole::Maintainer, AT)?;
    store.add_member(member.clone(), MemberRole::Member, AT)?;

    let visible_fact = crate::test_support::fact("fact_guard_target", &["owner", "maint", "bob"])?;
    let private_fact = crate::test_support::fact("fact_owner_secret", &["owner"])?;
    store.insert_fact(&visible_fact)?;
    store.insert_fact(&private_fact)?;

    // Regular member cannot grant `allow_guard_once`.
    assert!(matches!(
        store.grant_guard_allow_once(&member, &visible_fact.id, 600_000, AT),
        Err(StoreError::PermissionDenied)
    ));
    // Maintainer cannot grant `allow_guard_once` on a fact invisible to Maintainer (`NotFound`, zero existence leak).
    assert!(matches!(
        store.grant_guard_allow_once(&maint, &private_fact.id, 600_000, AT),
        Err(StoreError::NotFound)
    ));

    // Owner issues a 10-minute single-use grant; consumed once within TTL, then exhausted.
    let grant = store.grant_guard_allow_once(&owner, &visible_fact.id, 600_000, AT)?;
    assert_eq!(grant.kp_ref, "fact_guard_target");
    assert_eq!(grant.expires_at, EpochMs(AT.0 + 600_000));
    assert!(store.consume_guard_allow_once(&visible_fact.id, EpochMs(AT.0 + 1_000))?);
    assert!(!store.consume_guard_allow_once(&visible_fact.id, EpochMs(AT.0 + 2_000))?);

    // Expired grant is never consumed.
    store.grant_guard_allow_once(&maint, &visible_fact.id, 10_000, AT)?;
    assert!(!store.consume_guard_allow_once(&visible_fact.id, EpochMs(AT.0 + 10_000))?);
    Ok(())
}

#[test]
fn cli_governance_store_methods_cover_reviews_extraction_context_scope_updates_and_identities(
) -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Community)?;
    let owner: ActorId = "owner".parse()?;
    let bob: ActorId = "bob".parse()?;
    store.add_member(bob.clone(), MemberRole::Member, AT)?;

    let r1 = store.upsert_open_review_item(&ReviewItem {
        id: new_review_id()?,
        actor: owner.clone(),
        kind: ReviewKind::AmbiguousScope,
        target_id: "fact_1".into(),
        question: "q1".into(),
        options: vec!["a".into()],
        payload: json!({}),
        status: ReviewStatus::Open,
        created_at: AT,
        resolved_at: None,
    })?;
    let r2 = store.upsert_open_review_item(&ReviewItem {
        id: new_review_id()?,
        actor: bob.clone(),
        kind: ReviewKind::IdentityMapping,
        target_id: "chat:ext_1".into(),
        question: "q2".into(),
        options: vec!["b".into()],
        payload: json!({}),
        status: ReviewStatus::Open,
        created_at: EpochMs(AT.0 + 10),
        resolved_at: None,
    })?;
    let all_open = store.list_all_open_review_items()?;
    assert_eq!(all_open.len(), 2);
    assert_eq!(all_open[0].id, r1.id);
    assert_eq!(all_open[1].id, r2.id);

    // Extraction source context lookup
    let src_input = crate::test_support::source(&["owner", "bob"])?;
    let saved = store.ingest_source(&"origin".parse()?, &"ctx_key".parse()?, &src_input)?;
    let chunk_id = format!("{}-0", saved.source_id);
    let ctx = store
        .extraction_source_context(&saved.source_id, std::slice::from_ref(&chunk_id))?
        .ok_or(StoreError::NotFound)?;
    assert_eq!(ctx.uri, src_input.uri);
    assert_eq!(ctx.chunks.len(), 1);
    assert_eq!(ctx.chunks[0].0, chunk_id);

    // Scope review fact updates (widening audience, copying entities, clearing broader_scope, audit log)
    let mut f_narrow = crate::test_support::fact("fact_narrow", &["owner"])?;
    f_narrow.broader_scope = Some(kp_model::BroaderScopeCandidate {
        narrow_active: GovernedScope::SelfOnly {
            actor: owner.clone(),
        },
        broader_candidate: GovernedScope::Actors {
            actors: BTreeSet::from([owner.clone(), bob.clone()]),
        },
        question: "widen?".into(),
    });
    store.insert_fact(&f_narrow)?;

    let f_rep = crate::test_support::fact("fact_replacement", &["owner"])?;
    store.insert_fact(&f_rep)?;
    let widened = AudienceSpec::Explicit(BTreeSet::from([owner.clone(), bob.clone()]));
    let copied = BTreeSet::from(["ops_window".to_owned()]);
    store.apply_scope_review_fact_updates(&ScopeReviewFactUpdates {
        replacement_fact_id: Some(&f_rep.id),
        widened_audience: Some(&widened),
        copied_entities: Some(&copied),
        clear_broader_scope: Some((&f_narrow.id, &owner)),
        change_log_entry: Some((&r1.id, &owner, json!({"choice": "confirm"}), AT)),
    })?;
    assert!(store
        .visible_facts(&bob, AT)?
        .iter()
        .any(|f| f.id == f_rep.id));
    assert!(store
        .entities()?
        .get(&f_rep.id)
        .is_some_and(|ents| ents.contains("ops_window")));
    assert!(store
        .derivation_view()?
        .facts
        .get(&f_narrow.id)
        .is_some_and(|f| f.broader_scope.is_none()));

    // External identity mapping resolution
    store.db.execute(
        "INSERT INTO external_identities (provider, external_id, display_name, proposed_actor_id, status, updated_at)
         VALUES ('chat', 'ext_bob', 'Bob', 'bob', 'proposed', ?1)",
        [AT.0],
    )?;
    assert!(store.resolve_external_identity_mapping(
        "chat",
        "ext_bob",
        Some(&bob),
        EpochMs(AT.0 + 50)
    )?);
    let (status, confirmed): (String, Option<String>) = store.db.query_row(
        "SELECT status, confirmed_actor_id FROM external_identities WHERE provider = 'chat' AND external_id = 'ext_bob'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(status, "confirmed");
    assert_eq!(confirmed.as_deref(), Some("bob"));

    assert!(store.resolve_external_identity_mapping(
        "chat",
        "ext_bob",
        None,
        EpochMs(AT.0 + 60)
    )?);
    let (status_rej, confirmed_rej): (String, Option<String>) = store.db.query_row(
        "SELECT status, confirmed_actor_id FROM external_identities WHERE provider = 'chat' AND external_id = 'ext_bob'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(status_rej, "rejected");
    assert!(confirmed_rej.is_none());
    Ok(())
}

#[test]
fn multi_guard_grant_and_consumption_are_all_or_nothing() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let owner: ActorId = "owner".parse()?;
    let maint: ActorId = "maint".parse()?;
    let member: ActorId = "member".parse()?;
    let bob: ActorId = "bob".parse()?;
    store.add_member(maint.clone(), MemberRole::Maintainer, AT)?;
    store.add_member(member.clone(), MemberRole::Member, AT)?;
    store.add_member(bob.clone(), MemberRole::Member, AT)?;

    let g1 = crate::test_support::fact("fact_guard_alpha", &["owner", "maint", "member"])?;
    let g2 = crate::test_support::fact("fact_guard_beta", &["owner", "maint", "member"])?;
    let secret_bob = crate::test_support::fact("fact_secret_bob", &["bob"])?;
    store.insert_fact(&g1)?;
    store.insert_fact(&g2)?;
    store.insert_fact(&secret_bob)?;

    // Non-moderator is denied; empty targets are rejected.
    assert!(matches!(
        store.grant_guard_allow_once_many(&member, std::slice::from_ref(&g1.id), 600_000, AT),
        Err(StoreError::PermissionDenied)
    ));
    assert!(store
        .grant_guard_allow_once_many(&owner, &[], 600_000, AT)
        .is_err());

    // Partial visibility failure inserts ZERO grants.
    assert!(matches!(
        store.grant_guard_allow_once_many(
            &maint,
            &[g1.id.clone(), secret_bob.id.clone()],
            600_000,
            AT
        ),
        Err(StoreError::NotFound)
    ));
    assert_eq!(
        store.try_consume_all_guard_allows(std::slice::from_ref(&g1.id), AT)?,
        vec![g1.id.clone()]
    );

    // Grant ONLY `g1`: attempting to consume `[g1, g2, g1]` returns missing `[g2]` and consumes ZERO grants!
    store.grant_guard_allow_once(&owner, &g1.id, 600_000, AT)?;
    let missing = store.try_consume_all_guard_allows(
        &[g1.id.clone(), g2.id.clone(), g1.id.clone()],
        EpochMs(AT.0 + 1_000),
    )?;
    assert_eq!(missing, vec![g2.id.clone()]);

    // Now grant `g2`: `g1`'s earlier grant is still unconsumed, so `[g1, g2, g1]` succeeds atomically!
    store.grant_guard_allow_once(&maint, &g2.id, 600_000, AT)?;
    let missing_after = store.try_consume_all_guard_allows(
        &[g1.id.clone(), g2.id.clone(), g1.id.clone()],
        EpochMs(AT.0 + 2_000),
    )?;
    assert!(missing_after.is_empty());

    // Single-use: both grants are now spent.
    assert_eq!(
        store
            .try_consume_all_guard_allows(&[g1.id.clone(), g2.id.clone()], EpochMs(AT.0 + 3_000))?,
        vec![g1.id.clone(), g2.id.clone()]
    );

    // Multi-grant atomically issues grants for all targets in one call.
    let batch_grants = store.grant_guard_allow_once_many(
        &owner,
        &[g1.id.clone(), g2.id.clone(), g1.id.clone()],
        600_000,
        EpochMs(AT.0 + 4_000),
    )?;
    assert_eq!(batch_grants.len(), 2);
    assert!(store
        .try_consume_all_guard_allows(&[g1.id, g2.id], EpochMs(AT.0 + 5_000))?
        .is_empty());
    Ok(())
}

#[test]
fn open_review_items_enforce_live_premise_liveness_and_source_scope_visibility() -> Result<()> {
    use kp_model::{BroaderScopeCandidate, GovernedScope, SourceScope};

    let (_dir, mut store) = library(LibraryMode::Community)?;
    let owner: ActorId = "owner".parse()?;
    let bob: ActorId = "bob".parse()?;
    store.add_member(bob.clone(), MemberRole::Member, AT)?;

    let scope_id: kp_model::SourceScopeId = "scope_shared".parse()?;
    store.upsert_source_scope(&SourceScope {
        scope_id: scope_id.clone(),
        provider: "container".into(),
        external_container_id: "c_shared".into(),
        label: "Shared".into(),
        current_audience: AudienceSpec::Explicit([owner.clone(), bob.clone()].into()),
        synced_at: AT,
    })?;

    let mut ev = crate::test_support::fact("fact_ev_bob", &["owner", "bob"])?;
    ev.source_scope_id = Some(scope_id.clone());
    let mut cs = crate::test_support::fact("fact_cs_owner", &["owner", "bob"])?;
    cs.broader_scope = Some(BroaderScopeCandidate {
        narrow_active: GovernedScope::SelfOnly {
            actor: owner.clone(),
        },
        broader_candidate: GovernedScope::Actors {
            actors: BTreeSet::from([owner.clone(), bob.clone()]),
        },
        question: "是否扩大范围？".into(),
    });
    store.insert_fact(&ev)?;
    store.insert_fact(&cs)?;

    // Legacy unbound ConceptAlignment review (non-empty payload without fact IDs) is suppressed.
    let unbound = ReviewItem {
        id: new_review_id()?,
        actor: bob.clone(),
        kind: ReviewKind::ConceptAlignment,
        target_id: "cpt_a->cpt_b".into(),
        question: "旧未绑定问题？".into(),
        options: vec!["entails".into()],
        payload: json!({"left_concept_id": "cpt_a", "right_concept_id": "cpt_b"}),
        status: ReviewStatus::Open,
        created_at: AT,
        resolved_at: None,
    };
    assert!(store.upsert_open_review_item_if_valid(&unbound)?.is_none());

    // Valid evidence-bound ConceptAlignment review for Bob:
    let bound = ReviewItem {
        id: new_review_id()?,
        actor: bob.clone(),
        kind: ReviewKind::ConceptAlignment,
        target_id: format!("cpt_a->cpt_b->{}:{}", ev.id, cs.id),
        question: "两处描述是否冲突？".into(),
        options: vec!["entails".into(), "unrelated".into()],
        payload: json!({
            "left_concept_id": "cpt_a",
            "right_concept_id": "cpt_b",
            "event_fact_id": ev.id,
            "constraint_fact_id": cs.id,
        }),
        status: ReviewStatus::Open,
        created_at: AT,
        resolved_at: None,
    };
    let saved_bound = store
        .upsert_open_review_item_if_valid(&bound)?
        .ok_or(StoreError::NotFound)?;
    assert_eq!(store.list_review_items(&bob, None)?.len(), 1);
    assert!(store.get_review_item(&saved_bound.id)?.is_some());

    // Shrink `source_scope` to exclude Bob -> review immediately disappears and cannot be resolved!
    store.upsert_source_scope(&SourceScope {
        scope_id: scope_id.clone(),
        provider: "container".into(),
        external_container_id: "c_shared".into(),
        label: "Shared".into(),
        current_audience: AudienceSpec::Explicit([owner.clone()].into()),
        synced_at: EpochMs(AT.0 + 1),
    })?;
    assert!(store.list_review_items(&bob, None)?.is_empty());
    assert!(store.list_all_open_review_items()?.is_empty());
    assert!(store.get_review_item(&saved_bound.id)?.is_none());
    assert!(matches!(
        store.resolve_review_item(&saved_bound.id, &bob, ReviewStatus::Dismissed, AT),
        Err(StoreError::NotFound)
    ));

    // Restore Bob to `source_scope`, then withdraw `ev` -> review stays suppressed and mid-flight upsert returns None!
    store.upsert_source_scope(&SourceScope {
        scope_id,
        provider: "container".into(),
        external_container_id: "c_shared".into(),
        label: "Shared".into(),
        current_audience: AudienceSpec::Explicit([owner.clone(), bob.clone()].into()),
        synced_at: EpochMs(AT.0 + 2),
    })?;
    assert_eq!(store.list_review_items(&bob, None)?.len(), 1);
    store.forget_fact_id(&ev.id, &owner, EpochMs(AT.0 + 3))?;
    assert!(store.list_review_items(&bob, None)?.is_empty());
    assert!(store.upsert_open_review_item_if_valid(&bound)?.is_none());

    // `AmbiguousScope` review disappears as soon as `broader_scope` is cleared on the target constraint!
    let amb = store
        .upsert_open_review_item_if_valid(&ReviewItem {
            id: new_review_id()?,
            actor: owner.clone(),
            kind: ReviewKind::AmbiguousScope,
            target_id: cs.id.to_string(),
            question: "是否扩大范围？".into(),
            options: vec!["confirm".into()],
            payload: json!({"constraint_fact_id": cs.id}),
            status: ReviewStatus::Open,
            created_at: AT,
            resolved_at: None,
        })?
        .ok_or(StoreError::NotFound)?;
    assert_eq!(store.list_review_items(&owner, None)?.len(), 1);
    store.apply_scope_review_fact_updates(&ScopeReviewFactUpdates {
        clear_broader_scope: Some((&cs.id, &owner)),
        ..ScopeReviewFactUpdates::default()
    })?;
    assert!(store.list_review_items(&owner, None)?.is_empty());
    assert!(store.get_review_item(&amb.id)?.is_none());
    Ok(())
}

#[test]
fn bind_and_list_confirmed_external_identities_enforce_admin_auth() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let owner: ActorId = "owner".parse()?;
    let bob: ActorId = "bob".parse()?;
    let _ = store.add_member(bob.clone(), MemberRole::Member, AT)?;

    // Ordinary member cannot bind another participant's external identity.
    assert!(matches!(
        store.bind_external_identity(&bob, "feishu", "ou_owner", &bob, Some("Spoof"), AT),
        Err(StoreError::PermissionDenied)
    ));
    assert!(store.confirmed_external_identities()?.is_empty());

    // Owner binds both `owner` and `bob` cleanly and `confirmed_external_identities` returns both.
    store.bind_external_identity(&owner, "feishu", "ou_owner", &owner, Some("Owner"), AT)?;
    store.bind_external_identity(
        &owner,
        "feishu",
        "ou_bob",
        &bob,
        Some("Bob"),
        EpochMs(AT.0 + 1),
    )?;
    let confirmed = store.confirmed_external_identities()?;
    assert_eq!(confirmed.len(), 2);
    assert_eq!(
        confirmed.get(&("feishu".to_owned(), "ou_owner".to_owned())),
        Some(&owner)
    );
    assert_eq!(
        confirmed.get(&("feishu".to_owned(), "ou_bob".to_owned())),
        Some(&bob)
    );
    Ok(())
}
