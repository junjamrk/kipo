use std::collections::BTreeSet;

use kp_model::{
    derived_fact, fact_claim_text, fact_condition_concept_id, ActorId, ConceptId, DerivationView,
    EntailmentVerdict, EpochMs, Fact, FactAccess, FactId, FactKind, FactStatus, GovernedScope,
    KpError, LibraryId, MembershipRelation, ReviewItem, ReviewKind, ReviewStatus, StrongRef,
};
use kp_store::{Result, Store};
use serde_json::json;

use super::align::{persist_alignment_verdict, AlignmentVerdictSpec};
use super::{
    compute_quotes_sha256, effective_fact_viewers, event_audience_covers, fact_quote_or_claim,
    is_governed_or_concept_fact, is_system_internal_fact, ConceptAlignmentTable,
    ScopeResolutionOutcome, ScopeReviewDecision, CONCEPT_ALIGNMENT_PREDICATE,
};
use crate::nl_funnel::{alloc_opaque_fact_id, alloc_opaque_review_id};
use crate::rules::{common_windows, derive, DeriveResult};

/// Evaluates all active constraints carrying `broader_scope` (`governed_scope == SelfOnly(author)`):
/// immediately emits an `AmbiguousScope` `ReviewItem` for author `B` whenever `broader_candidate`
/// covers at least one known member `A != B` (without requiring `A`'s private local `Event`s to
/// exist in `B`'s library).
pub fn evaluate_ambiguous_scope_reviews(
    graph: &DerivationView,
    alignment: &ConceptAlignmentTable,
    existing_open_reviews: &[ReviewItem],
    at: EpochMs,
) -> kp_model::Result<Vec<ReviewItem>> {
    let mut reviews = Vec::new();
    let all_memberships: Vec<(&Fact, MembershipRelation)> = graph
        .facts
        .values()
        .filter(|f| f.kind == FactKind::Membership && f.status == FactStatus::Active)
        .filter_map(|f| {
            serde_json::from_value::<MembershipRelation>(f.object.clone())
                .ok()
                .map(|rel| (f, rel))
        })
        .collect();

    for constraint in graph
        .facts
        .values()
        .filter(|f| f.kind == FactKind::Constraint && f.status == FactStatus::Active)
    {
        let Some(broader) = &constraint.broader_scope else {
            continue;
        };
        let author_b = &constraint.author;
        if constraint.governed_scope
            != Some(GovernedScope::SelfOnly {
                actor: author_b.clone(),
            })
        {
            continue;
        }
        // Strict side-channel guard: ONLY consider facts visible to `author_b`!
        let visible_to_b_any_time = effective_visible_base_facts_for_author(graph, author_b);
        if !visible_to_b_any_time.contains(&constraint.id) {
            continue;
        }

        let b_memberships: Vec<(&Fact, MembershipRelation)> = all_memberships
            .iter()
            .filter(|(mf, _)| visible_to_b_any_time.contains(&mf.id))
            .cloned()
            .collect();

        let mut impacted_actors = BTreeSet::new();
        let mut supporting_membership_ids = BTreeSet::new();
        match &broader.broader_candidate {
            GovernedScope::SelfOnly { .. } | GovernedScope::Resource { .. } => {}
            GovernedScope::Actors { actors } => {
                for actor in actors {
                    if actor != author_b && graph.members.contains(actor) {
                        impacted_actors.insert(actor.clone());
                    }
                }
            }
            GovernedScope::RelationGroup {
                anchor,
                group_concept_id,
            } => {
                for (mf, m_rel) in &b_memberships {
                    if &m_rel.anchor == anchor
                        && m_rel.member != *author_b
                        && graph.members.contains(&m_rel.member)
                        && alignment.entails(&m_rel.group_concept_id, group_concept_id)
                    {
                        impacted_actors.insert(m_rel.member.clone());
                        supporting_membership_ids.insert(mf.id.clone());
                    }
                }
            }
        }

        if impacted_actors.is_empty() {
            continue;
        }

        let target_id = constraint.id.to_string();
        let existing_match = existing_open_reviews.iter().find(|r| {
            r.actor == *author_b
                && r.kind == ReviewKind::AmbiguousScope
                && r.target_id == target_id
                && r.status == ReviewStatus::Open
        });
        let duplicate_in_batch = reviews.iter().any(|r: &ReviewItem| {
            r.actor == *author_b && r.kind == ReviewKind::AmbiguousScope && r.target_id == target_id
        });
        if !duplicate_in_batch {
            let mut payload_obj = json!({
                "constraint_fact_id": constraint.id,
                "narrow_active": broader.narrow_active,
                "broader_candidate": broader.broader_candidate,
                "impacted_actors": impacted_actors,
                "trigger_event_ids": Vec::<FactId>::new(),
            });
            if !supporting_membership_ids.is_empty() {
                if let Some(map) = payload_obj.as_object_mut() {
                    map.insert(
                        "supporting_membership_ids".into(),
                        json!(supporting_membership_ids),
                    );
                }
            }
            let unchanged = existing_match.is_some_and(|prev| prev.payload == payload_obj);
            if !unchanged {
                reviews.push(ReviewItem {
                    id: match existing_match {
                        Some(prev) => prev.id.clone(),
                        None => alloc_opaque_review_id()?,
                    },
                    actor: author_b.clone(),
                    kind: ReviewKind::AmbiguousScope,
                    target_id,
                    question: broader.question.clone(),
                    options: vec![
                        "confirm_broader".into(),
                        "keep_narrow".into(),
                        "dismiss".into(),
                    ],
                    payload: payload_obj,
                    status: ReviewStatus::Open,
                    created_at: existing_match.map_or(at, |p| p.created_at),
                    resolved_at: None,
                });
            }
        }
    }
    Ok(reviews)
}

fn effective_visible_base_facts_for_author(
    graph: &DerivationView,
    author: &ActorId,
) -> BTreeSet<FactId> {
    graph
        .facts
        .values()
        .filter(|f| {
            f.status == FactStatus::Active
                && f.access.expired_at().is_none()
                && effective_fact_viewers(graph, f).contains(author)
        })
        .map(|f| f.id.clone())
        .collect()
}

/// Applies `actor`'s decision on an `AmbiguousScope` constraint (superseding to `broader_candidate`
/// on `ConfirmBroader` and re-running derivation immediately so cross-person warnings materialize).
pub fn resolve_ambiguous_scope_review(
    store: &mut Store,
    constraint_fact_id: &FactId,
    actor: &ActorId,
    decision: ScopeReviewDecision,
    review_item: Option<&ReviewItem>,
    at: EpochMs,
) -> Result<ScopeResolutionOutcome> {
    let graph = store.derivation_view()?;
    let constraint = graph
        .facts
        .get(constraint_fact_id)
        .ok_or(kp_store::StoreError::NotFound)?;
    if &constraint.author != actor {
        return Err(KpError::invalid(
            "actor",
            "only the constraint's author may resolve its ambiguous scope",
        )
        .into());
    }
    let broader = constraint.broader_scope.as_ref().ok_or_else(|| {
        KpError::invalid(
            "broader_scope",
            "constraint has no pending broader_scope candidate",
        )
    })?;

    if let Some(rev) = review_item {
        if &rev.actor != actor || rev.kind != ReviewKind::AmbiguousScope {
            return Err(KpError::invalid("review_item", "mismatched review item").into());
        }
        if !kp_store::is_ambiguous_scope_review_supported(&graph, rev) {
            return Err(kp_store::StoreError::NotFound);
        }
    }

    match decision {
        ScopeReviewDecision::Dismiss => {
            let resolved_review = review_item.cloned().map(|mut r| {
                r.status = ReviewStatus::Dismissed;
                r.resolved_at = Some(at);
                r
            });
            Ok(ScopeResolutionOutcome {
                replacement_fact_id: None,
                resolved_review,
                derive_derived: 0,
                derive_justifications: 0,
            })
        }
        ScopeReviewDecision::ConfirmBroader | ScopeReviewDecision::KeepNarrow => {
            let new_scope = if decision == ScopeReviewDecision::ConfirmBroader {
                broader.broader_candidate.clone()
            } else {
                broader.narrow_active.clone()
            };
            let new_id = alloc_opaque_fact_id()?;
            let mut replacement = constraint.clone();
            replacement.id = new_id.clone();
            replacement.logical_key = new_id.as_str().parse()?;
            replacement.governed_scope = Some(new_scope);
            replacement.broader_scope = None;
            replacement.status = FactStatus::Active;
            replacement.superseded_by = None;
            if let FactAccess::Base { stamp, .. } = &mut replacement.access {
                stamp.recorded_at = at;
                stamp.expired_at = None;
            }
            replacement.validate()?;

            let old_constraint = constraint.clone();
            store.supersede(constraint_fact_id, &replacement)?;
            carry_forward_constraint_alignments(store, &graph, &old_constraint, &replacement, at)?;
            let DeriveResult {
                derived,
                justifications,
            } = derive(store)?;

            let resolved_review = review_item.cloned().map(|mut r| {
                r.status = ReviewStatus::Confirmed;
                r.resolved_at = Some(at);
                r
            });
            Ok(ScopeResolutionOutcome {
                replacement_fact_id: Some(new_id),
                resolved_review,
                derive_derived: derived,
                derive_justifications: justifications,
            })
        }
    }
}

fn carry_forward_constraint_alignments(
    store: &mut Store,
    pre_supersede_graph: &DerivationView,
    old_constraint: &Fact,
    replacement: &Fact,
    at: EpochMs,
) -> Result<()> {
    if old_constraint.strong_ref().content_sha256 != replacement.strong_ref().content_sha256 {
        return Ok(());
    }
    let owner = store.status()?.owner_actor;
    let mut to_carry = Vec::new();
    for fact in pre_supersede_graph.facts.values() {
        if fact.predicate != CONCEPT_ALIGNMENT_PREDICATE
            || fact.status != FactStatus::Active
            || fact.access.expired_at().is_some()
        {
            continue;
        }
        if fact.object.get("right_fact_id").and_then(|v| v.as_str())
            != Some(old_constraint.id.as_str())
        {
            continue;
        }
        let (Some(left_cid), Some(right_cid), Some(lf_id), Some(recorded_qh), Some(expected_refs)) = (
            fact.object
                .get("left_concept_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<ConceptId>().ok()),
            fact.object
                .get("right_concept_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<ConceptId>().ok()),
            fact.object
                .get("left_fact_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<FactId>().ok()),
            fact.object
                .get("quotes_sha256")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            fact.object
                .get("evidence_refs")
                .cloned()
                .and_then(|v| serde_json::from_value::<[StrongRef; 2]>(v).ok()),
        ) else {
            continue;
        };
        let Some(lf) = pre_supersede_graph.facts.get(&lf_id) else {
            continue;
        };
        if lf.status != FactStatus::Active
            || lf.access.expired_at().is_some()
            || lf.strong_ref() != expected_refs[0]
            || old_constraint.strong_ref() != expected_refs[1]
        {
            continue;
        }
        let new_qh = compute_quotes_sha256(
            fact_quote_or_claim(lf).as_deref(),
            fact_quote_or_claim(replacement).as_deref(),
        );
        if new_qh != recorded_qh {
            continue;
        }
        if effective_fact_viewers(pre_supersede_graph, lf)
            .is_disjoint(&effective_fact_viewers(pre_supersede_graph, replacement))
        {
            continue;
        }
        let verdict = match fact.object.get("verdict").and_then(|v| v.as_str()) {
            Some("entails") => EntailmentVerdict::Entails,
            Some("contradicts") => EntailmentVerdict::Contradicts,
            Some("unrelated") => EntailmentVerdict::Unrelated,
            Some("ambiguous") => EntailmentVerdict::Ambiguous {
                question: fact
                    .object
                    .get("question")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned(),
            },
            _ => continue,
        };
        to_carry.push((left_cid, right_cid, verdict, lf.clone(), recorded_qh));
    }

    for (left_cid, right_cid, verdict, lf, recorded_qh) in to_carry {
        persist_alignment_verdict(
            store,
            &AlignmentVerdictSpec {
                owner: &owner,
                left: &left_cid,
                right: &right_cid,
                verdict: &verdict,
                evidence: Some((&lf, replacement)),
                quotes_sha256: Some(&recorded_qh),
                seen_quotes_sha256: None,
                at,
            },
        )?;
    }
    Ok(())
}

pub(crate) struct GovernedCollisionEmission<'a> {
    pub conclusion: Fact,
    pub premises: Vec<&'a Fact>,
}

pub(crate) fn event_entails_constraint(
    event: &Fact,
    constraint: &Fact,
    alignment: &ConceptAlignmentTable,
) -> bool {
    let (Some(e_cid), Some(c_cid)) = (
        fact_condition_concept_id(event),
        fact_condition_concept_id(constraint),
    ) else {
        return false;
    };
    if e_cid == c_cid || alignment.entails(&e_cid, &c_cid) {
        return true;
    }
    let qh = compute_quotes_sha256(
        fact_quote_or_claim(event).as_deref(),
        fact_quote_or_claim(constraint).as_deref(),
    );
    alignment.entails_facts(&event.id, &constraint.id, &qh)
}

pub(crate) fn event_applies_to_actor(
    event: &Fact,
    target_actor: &ActorId,
    memberships: &[(&Fact, MembershipRelation)],
    alignment: &ConceptAlignmentTable,
) -> bool {
    match &event.governed_scope {
        None => true,
        Some(GovernedScope::SelfOnly { actor }) => actor == target_actor,
        Some(GovernedScope::Actors { actors }) => actors.contains(target_actor),
        Some(GovernedScope::RelationGroup {
            anchor,
            group_concept_id,
        }) => {
            anchor == target_actor
                || memberships.iter().any(|(_, rel)| {
                    &rel.anchor == anchor
                        && &rel.member == target_actor
                        && alignment.entails(&rel.group_concept_id, group_concept_id)
                })
        }
        Some(GovernedScope::Resource { .. }) => false,
    }
}

pub(crate) fn is_event_contradicted_in_graph(
    graph: &DerivationView,
    alignment: &ConceptAlignmentTable,
    event: &Fact,
) -> bool {
    let FactAccess::Base { stamp: old_s, .. } = &event.access else {
        return false;
    };
    let Some(old_cid) = fact_condition_concept_id(event) else {
        return false;
    };
    graph.facts.values().any(|other| {
        if other.id == event.id
            || other.kind != FactKind::Event
            || other.status != FactStatus::Active
            || is_system_internal_fact(other)
        {
            return false;
        }
        let FactAccess::Base { stamp: new_s, .. } = &other.access else {
            return false;
        };
        if (new_s.recorded_at, &other.id) <= (old_s.recorded_at, &event.id) {
            return false;
        }
        // Contradicting newer event covers the older event's window and audience.
        if !(new_s.valid.valid_from <= old_s.valid.valid_from
            && new_s.valid.valid_until >= old_s.valid.valid_until)
        {
            return false;
        }
        if !event_audience_covers(graph, other, event) {
            return false;
        }
        let Some(new_cid) = fact_condition_concept_id(other) else {
            return false;
        };
        if alignment.contradicts(&new_cid, &old_cid) {
            return true;
        }
        let qh = compute_quotes_sha256(
            fact_quote_or_claim(other).as_deref(),
            fact_quote_or_claim(event).as_deref(),
        );
        alignment.contradicts_facts(&other.id, &event.id, &qh)
    })
}

/// Computes all person-targeted `CollisionWarning` emissions over `claims` inside `rules::Engine`.
pub(crate) fn compute_governed_collisions<'a>(
    origin: &LibraryId,
    author: &ActorId,
    graph: &DerivationView,
    claims: &[&'a Fact],
    alignment: &ConceptAlignmentTable,
) -> kp_model::Result<Vec<GovernedCollisionEmission<'a>>> {
    let memberships: Vec<(&'a Fact, MembershipRelation)> = claims
        .iter()
        .copied()
        .filter(|f| f.kind == FactKind::Membership)
        .filter_map(|f| {
            serde_json::from_value::<MembershipRelation>(f.object.clone())
                .ok()
                .map(|rel| (f, rel))
        })
        .collect();

    let events: Vec<&'a Fact> = claims
        .iter()
        .copied()
        .filter(|f| {
            f.kind == FactKind::Event
                && !is_system_internal_fact(f)
                && is_governed_or_concept_fact(f)
                && !is_event_contradicted_in_graph(graph, alignment, f)
        })
        .collect();

    let constraints: Vec<&'a Fact> = claims
        .iter()
        .copied()
        .filter(|f| f.kind == FactKind::Constraint && is_governed_or_concept_fact(f))
        .collect();

    let mut out = Vec::new();
    for event in &events {
        for constraint in &constraints {
            if !event_entails_constraint(event, constraint, alignment) {
                continue;
            }
            let effective_scope =
                constraint
                    .governed_scope
                    .clone()
                    .unwrap_or_else(|| GovernedScope::SelfOnly {
                        actor: constraint.author.clone(),
                    });

            match effective_scope {
                GovernedScope::SelfOnly { actor: self_actor } => {
                    if !event_applies_to_actor(event, &self_actor, &memberships, alignment)
                        || common_windows(graph, &[event, constraint]).is_empty()
                    {
                        continue;
                    }
                    out.push(build_person_collision(
                        origin,
                        author,
                        self_actor.as_str(),
                        Some(GovernedScope::SelfOnly {
                            actor: self_actor.clone(),
                        }),
                        &[event, constraint],
                        event,
                        constraint,
                    )?);
                }
                GovernedScope::Actors { actors } => {
                    if common_windows(graph, &[event, constraint]).is_empty() {
                        continue;
                    }
                    for target_actor in actors {
                        if !event_applies_to_actor(event, &target_actor, &memberships, alignment) {
                            continue;
                        }
                        let derived_scope = if target_actor == constraint.author {
                            Some(GovernedScope::SelfOnly {
                                actor: target_actor.clone(),
                            })
                        } else {
                            Some(GovernedScope::Actors {
                                actors: BTreeSet::from([
                                    target_actor.clone(),
                                    constraint.author.clone(),
                                ]),
                            })
                        };
                        out.push(build_person_collision(
                            origin,
                            author,
                            target_actor.as_str(),
                            derived_scope,
                            &[event, constraint],
                            event,
                            constraint,
                        )?);
                    }
                }
                GovernedScope::RelationGroup {
                    anchor,
                    group_concept_id,
                } => {
                    // 1) Cross-person group members via 3-premise conjunctive justification!
                    for (m_fact, m_rel) in &memberships {
                        if m_rel.anchor != anchor
                            || !alignment.entails(&m_rel.group_concept_id, &group_concept_id)
                        {
                            continue;
                        }
                        if !event_applies_to_actor(event, &m_rel.member, &memberships, alignment) {
                            continue;
                        }
                        let premises: [&'a Fact; 3] = [*event, *constraint, *m_fact];
                        if common_windows(graph, &premises).is_empty() {
                            continue;
                        }
                        // `governed_scope = None` so visibility is pure semiring over `[event, constraint, membership]`.
                        out.push(build_person_collision(
                            origin,
                            author,
                            m_rel.member.as_str(),
                            None,
                            &premises,
                            event,
                            constraint,
                        )?);
                    }
                    // 2) Anchor self-coverage (scoped to `SelfOnly(anchor)` so non-members/ex-members never see it).
                    if event_applies_to_actor(event, &anchor, &memberships, alignment)
                        && !common_windows(graph, &[event, constraint]).is_empty()
                    {
                        out.push(build_person_collision(
                            origin,
                            author,
                            anchor.as_str(),
                            Some(GovernedScope::SelfOnly {
                                actor: anchor.clone(),
                            }),
                            &[event, constraint],
                            event,
                            constraint,
                        )?);
                    }
                }
                GovernedScope::Resource { target } => {
                    let resource_matches = match &event.governed_scope {
                        None => true,
                        Some(GovernedScope::Resource { target: ev_target }) => ev_target == &target,
                        _ => false,
                    };
                    if resource_matches && !common_windows(graph, &[event, constraint]).is_empty() {
                        out.push(build_person_collision(
                            origin,
                            author,
                            &target,
                            None,
                            &[event, constraint],
                            event,
                            constraint,
                        )?);
                    }
                }
            }
        }
    }
    Ok(out)
}

fn build_person_collision<'a>(
    origin: &LibraryId,
    author: &ActorId,
    target_subject: &str,
    derived_scope: Option<GovernedScope>,
    premises: &[&'a Fact],
    event: &Fact,
    constraint: &Fact,
) -> kp_model::Result<GovernedCollisionEmission<'a>> {
    let at = premises
        .iter()
        .map(|p| p.access.recorded_at())
        .max()
        .ok_or_else(|| KpError::invalid("proof", "empty support"))?;
    let mut premise_refs: Vec<_> = premises.iter().map(|p| p.strong_ref()).collect();
    premise_refs.sort();

    let scope_mode = match &derived_scope {
        Some(GovernedScope::SelfOnly { .. }) => "self_only",
        Some(GovernedScope::Actors { .. }) => "actors",
        Some(GovernedScope::RelationGroup { .. }) => "relation_group",
        Some(GovernedScope::Resource { .. }) => "resource",
        None => "semiring",
    };
    let advice = format!(
        "{target_subject}: {} — {}",
        fact_claim_text(event),
        fact_claim_text(constraint)
    );
    let mut fact = derived_fact(
        origin,
        author,
        FactKind::CollisionWarning,
        target_subject,
        "kp.core.event_constraint_collision",
        json!({
            "target": target_subject,
            "scope_mode": scope_mode,
            "advice": advice,
            "event": event.object,
            "constraint": constraint.object,
            "premises": premise_refs,
        }),
        at,
    )?;
    fact.governed_scope = derived_scope;
    fact.tags = premises
        .iter()
        .flat_map(|p| p.tags.iter().cloned())
        .collect();
    Ok(GovernedCollisionEmission {
        conclusion: fact,
        premises: premises.to_vec(),
    })
}
