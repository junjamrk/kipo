use std::collections::{BTreeMap, BTreeSet};

use kp_model::{
    fact_condition_concept_id, ActorId, AudienceSpec, BitemporalStamp, CommunityConcept, ConceptId,
    ConceptKind, ContentHash, EntailmentQuery, EntailmentVerdict, EpochMs, Fact, FactAccess,
    FactId, FactKind, FactRank, FactStatus, GovernedScope, KpError, LlmClient, MembershipRelation,
    ModelRoutePolicy, ReviewItem, ReviewKind, ReviewStatus, TrustTier, ValidityWindow,
};
use kp_store::{Result, Store};
use serde_json::json;

use super::reviews::evaluate_ambiguous_scope_reviews;
use super::{
    compute_quotes_sha256, effective_fact_viewers, event_audience_covers,
    evidence_allows_llm_egress, fact_quote_or_claim, is_system_internal_fact,
    validate_live_evidence, AlignmentEvidence, CollisionDeriveOutcome, ConceptAlignmentTable,
    CONCEPT_ALIGNMENT_PREDICATE, CONCEPT_LABEL_BINDING_V2, EVENT_SUPERSEDED_PREDICATE,
};
use crate::nl_funnel::{alloc_opaque_fact_id, alloc_opaque_review_id};
use crate::prompts::{is_near_duplicate, normalize_for_dedup};
use crate::rules::{common_windows, derive, DeriveResult};

pub(super) struct AlignmentVerdictSpec<'a> {
    pub(super) owner: &'a ActorId,
    pub(super) left: &'a ConceptId,
    pub(super) right: &'a ConceptId,
    pub(super) verdict: &'a EntailmentVerdict,
    pub(super) evidence: Option<(&'a Fact, &'a Fact)>,
    pub(super) quotes_sha256: Option<&'a str>,
    pub(super) seen_quotes_sha256: Option<&'a BTreeSet<String>>,
    pub(super) at: EpochMs,
}

pub(super) fn persist_alignment_verdict(
    store: &mut Store,
    spec: &AlignmentVerdictSpec<'_>,
) -> Result<()> {
    let AlignmentVerdictSpec {
        owner,
        left,
        right,
        verdict,
        evidence,
        quotes_sha256,
        seen_quotes_sha256,
        at,
    } = *spec;
    let snapshot = store.derivation_view()?;
    let existing_active: Option<FactId> = snapshot
        .facts
        .values()
        .find(|f| {
            if f.predicate != CONCEPT_ALIGNMENT_PREDICATE
                || f.status != FactStatus::Active
                || f.object.get("left_concept_id").and_then(|v| v.as_str()) != Some(left.as_str())
                || f.object.get("right_concept_id").and_then(|v| v.as_str()) != Some(right.as_str())
            {
                return false;
            }
            match evidence {
                Some((lf, rf)) => {
                    f.object.get("left_fact_id").and_then(|v| v.as_str()) == Some(lf.id.as_str())
                        && f.object.get("right_fact_id").and_then(|v| v.as_str())
                            == Some(rf.id.as_str())
                }
                None => {
                    f.object.get("left_fact_id").is_none()
                        && f.object.get("right_fact_id").is_none()
                        && f.object.get("evidence_refs").is_none()
                }
            }
        })
        .map(|f| f.id.clone());

    let verdict_tag = match verdict {
        EntailmentVerdict::Entails => "entails",
        EntailmentVerdict::Contradicts => "contradicts",
        EntailmentVerdict::Unrelated => "unrelated",
        EntailmentVerdict::Ambiguous { .. } => "ambiguous",
    };

    let (candidate_digest, obj) = match evidence {
        Some((lf, rf)) => {
            let qh = match quotes_sha256 {
                Some(s) if !s.is_empty() => s.to_owned(),
                _ => compute_quotes_sha256(
                    fact_quote_or_claim(lf).as_deref(),
                    fact_quote_or_claim(rf).as_deref(),
                ),
            };
            let digest = ContentHash::digest(format!(
                "{}->{}->{verdict_tag}->{qh}->{}->{}",
                left.as_str(),
                right.as_str(),
                lf.id.as_str(),
                rf.id.as_str(),
            ));
            let refs = [lf.strong_ref(), rf.strong_ref()];
            let payload = match verdict {
                EntailmentVerdict::Ambiguous { question } => json!({
                    "left_concept_id": left,
                    "right_concept_id": right,
                    "verdict": verdict_tag,
                    "question": question,
                    "quotes_sha256": qh,
                    "left_fact_id": lf.id,
                    "right_fact_id": rf.id,
                    "evidence_refs": refs,
                }),
                _ => json!({
                    "left_concept_id": left,
                    "right_concept_id": right,
                    "verdict": verdict_tag,
                    "quotes_sha256": qh,
                    "left_fact_id": lf.id,
                    "right_fact_id": rf.id,
                    "evidence_refs": refs,
                }),
            };
            (digest, payload)
        }
        None => {
            let digest = ContentHash::digest(format!(
                "{CONCEPT_LABEL_BINDING_V2}->{}->{}->{verdict_tag}->{}",
                left.as_str(),
                right.as_str(),
                quotes_sha256.unwrap_or("")
            ));
            let payload = match verdict {
                EntailmentVerdict::Entails => json!({
                    "binding": CONCEPT_LABEL_BINDING_V2,
                    "left_concept_id": left,
                    "right_concept_id": right,
                    "verdict": "entails"
                }),
                EntailmentVerdict::Contradicts => json!({
                    "binding": CONCEPT_LABEL_BINDING_V2,
                    "left_concept_id": left,
                    "right_concept_id": right,
                    "verdict": "contradicts"
                }),
                EntailmentVerdict::Unrelated => {
                    let qh = quotes_sha256.unwrap_or("");
                    let seen: Vec<&str> = seen_quotes_sha256
                        .map(|set| set.iter().map(String::as_str).collect())
                        .unwrap_or_default();
                    json!({
                        "binding": CONCEPT_LABEL_BINDING_V2,
                        "left_concept_id": left,
                        "right_concept_id": right,
                        "verdict": "unrelated",
                        "quotes_sha256": qh,
                        "seen_quotes_sha256": seen,
                    })
                }
                EntailmentVerdict::Ambiguous { question } => json!({
                    "binding": CONCEPT_LABEL_BINDING_V2,
                    "left_concept_id": left,
                    "right_concept_id": right,
                    "verdict": "ambiguous",
                    "question": question
                }),
            };
            (digest, payload)
        }
    };

    let candidate_id: FactId = format!("align-{candidate_digest}").parse()?;
    if existing_active.as_ref() == Some(&candidate_id) {
        return Ok(());
    }
    let fact_id = if snapshot.facts.contains_key(&candidate_id) {
        alloc_opaque_fact_id()?
    } else {
        candidate_id
    };

    let meta_fact = Fact {
        id: fact_id.clone(),
        origin_library: store.library_id().clone(),
        logical_key: fact_id.as_str().parse()?,
        kind: FactKind::Observation,
        subject: left.to_string(),
        predicate: CONCEPT_ALIGNMENT_PREDICATE.to_owned(),
        object: obj,
        tags: BTreeSet::new(),
        author: owner.clone(),
        ingested_by: None,
        trust_tier: TrustTier::ToolOrModelVerified,
        access: FactAccess::Base {
            // Empty explicit audience guarantees zero visibility leak to any viewer!
            audience: AudienceSpec::Explicit(BTreeSet::new()),
            stamp: BitemporalStamp {
                valid: ValidityWindow {
                    valid_from: EpochMs(0),
                    valid_until: EpochMs::OPEN_END,
                },
                recorded_at: at,
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
    match existing_active {
        Some(old_id) => store.supersede(&old_id, &meta_fact),
        None => store.insert_fact(&meta_fact),
    }
}

/// Persists an explicit user alignment decision (e.g. from `kp review resolve` on
/// `ReviewKind::ConceptAlignment`), binding to `(left_fact_id, right_fact_id)` when present so
/// private premise verdicts never leak across viewers.
pub fn record_manual_concept_alignment(
    store: &mut Store,
    left: &ConceptId,
    right: &ConceptId,
    verdict: EntailmentVerdict,
    left_fact_id: Option<&FactId>,
    right_fact_id: Option<&FactId>,
    at: EpochMs,
) -> kp_store::Result<()> {
    let owner = store.status()?.owner_actor;
    match (left_fact_id, right_fact_id) {
        (Some(lf_id), Some(rf_id)) => {
            let graph = store.derivation_view()?;
            let (Some(lf), Some(rf)) = (graph.facts.get(lf_id), graph.facts.get(rf_id)) else {
                return Ok(());
            };
            if lf.status != FactStatus::Active
                || rf.status != FactStatus::Active
                || lf.access.expired_at().is_some()
                || rf.access.expired_at().is_some()
                || effective_fact_viewers(&graph, lf)
                    .is_disjoint(&effective_fact_viewers(&graph, rf))
            {
                return Ok(());
            }
            let qh = compute_quotes_sha256(
                fact_quote_or_claim(lf).as_deref(),
                fact_quote_or_claim(rf).as_deref(),
            );
            persist_alignment_verdict(
                store,
                &AlignmentVerdictSpec {
                    owner: &owner,
                    left,
                    right,
                    verdict: &verdict,
                    evidence: Some((lf, rf)),
                    quotes_sha256: Some(&qh),
                    seen_quotes_sha256: None,
                    at,
                },
            )
        }
        (None, None) => {
            let empty_qh = compute_quotes_sha256(None, None);
            let mut seen = BTreeSet::new();
            let (qh_opt, seen_opt) = if matches!(verdict, EntailmentVerdict::Unrelated) {
                seen.insert(empty_qh.clone());
                (Some(empty_qh.as_str()), Some(&seen))
            } else {
                (None, None)
            };
            persist_alignment_verdict(
                store,
                &AlignmentVerdictSpec {
                    owner: &owner,
                    left,
                    right,
                    verdict: &verdict,
                    evidence: None,
                    quotes_sha256: qh_opt,
                    seen_quotes_sha256: seen_opt,
                    at,
                },
            )
        }
        _ => Err(KpError::invalid(
            "evidence_refs",
            "manual evidence-bound alignment requires both left_fact_id and right_fact_id",
        )
        .into()),
    }
}

struct JudgePairInput<'a> {
    owner: &'a ActorId,
    left: &'a CommunityConcept,
    right: &'a CommunityConcept,
    left_quote: Option<&'a str>,
    right_quote: Option<&'a str>,
    evidence: AlignmentEvidence<'a>,
    at: EpochMs,
}

async fn judge_pair_cached(
    store: &mut Store,
    llm: &dyn LlmClient,
    input: JudgePairInput<'_>,
    table: &mut ConceptAlignmentTable,
) -> Result<(EntailmentVerdict, bool)> {
    let JudgePairInput {
        owner,
        left,
        right,
        left_quote,
        right_quote,
        evidence,
        at,
    } = input;
    if left.id == right.id {
        return Ok((
            EntailmentVerdict::Entails,
            validate_live_evidence(store, evidence)?,
        ));
    }
    if left.kind == right.kind
        && left.polarity == right.polarity
        && (normalize_for_dedup(&left.canonical_label)
            == normalize_for_dedup(&right.canonical_label)
            || is_near_duplicate(&left.canonical_label, &right.canonical_label))
    {
        let pair = (left.id.clone(), right.id.clone());
        table.unrelated.remove(&pair);
        table.unrelated_quote_hashes.remove(&pair);
        table.entails.insert(pair);
        persist_alignment_verdict(
            store,
            &AlignmentVerdictSpec {
                owner,
                left: &left.id,
                right: &right.id,
                verdict: &EntailmentVerdict::Entails,
                evidence: None,
                quotes_sha256: None,
                seen_quotes_sha256: None,
                at,
            },
        )?;
        return Ok((
            EntailmentVerdict::Entails,
            validate_live_evidence(store, evidence)?,
        ));
    }

    let pair = (left.id.clone(), right.id.clone());
    let quotes_hash = compute_quotes_sha256(left_quote, right_quote);
    if let Some((lf, rf)) = evidence.facts() {
        if let Some(cached) = table.fact_verdict(&lf.id, &rf.id, &quotes_hash) {
            return Ok((cached, validate_live_evidence(store, evidence)?));
        }
    } else {
        if table.entails.contains(&pair) {
            return Ok((EntailmentVerdict::Entails, true));
        }
        if table.contradicts.contains(&pair) {
            return Ok((EntailmentVerdict::Contradicts, true));
        }
        if let Some(q) = table.ambiguous.get(&pair) {
            return Ok((
                EntailmentVerdict::Ambiguous {
                    question: q.clone(),
                },
                true,
            ));
        }
        if left_quote.is_none() && right_quote.is_none() && table.unrelated.contains(&pair) {
            return Ok((EntailmentVerdict::Unrelated, true));
        }
        if table
            .unrelated_quote_hashes
            .get(&pair)
            .is_some_and(|seen| seen.contains(&quotes_hash))
        {
            return Ok((EntailmentVerdict::Unrelated, true));
        }
    }

    if !evidence_allows_llm_egress(store, llm, evidence)? {
        return Ok((EntailmentVerdict::Unrelated, false));
    }

    let verdict = llm
        .judge_entailment(&EntailmentQuery {
            left_label: left.canonical_label.clone(),
            left_domain: left.domain.clone(),
            right_label: right.canonical_label.clone(),
            right_domain: right.domain.clone(),
            left_quote: left_quote.map(str::to_owned),
            right_quote: right_quote.map(str::to_owned),
        })
        .await?;

    // Secondary premise validation before writeback (B2 #4): verify both supporting facts are
    // STILL active, non-expired, unchanged in `strong_ref()`, and satisfy audience/viewer rules.
    if !validate_live_evidence(store, evidence)? {
        return Ok((EntailmentVerdict::Unrelated, false));
    }

    if let Some((lf, rf)) = evidence.facts() {
        table.insert_fact_verdict(&lf.id, &rf.id, &quotes_hash, &verdict);
        persist_alignment_verdict(
            store,
            &AlignmentVerdictSpec {
                owner,
                left: &left.id,
                right: &right.id,
                verdict: &verdict,
                evidence: Some((lf, rf)),
                quotes_sha256: Some(&quotes_hash),
                seen_quotes_sha256: None,
                at,
            },
        )?;
    } else {
        match &verdict {
            EntailmentVerdict::Entails => {
                table.unrelated.remove(&pair);
                table.unrelated_quote_hashes.remove(&pair);
                table.entails.insert(pair);
                persist_alignment_verdict(
                    store,
                    &AlignmentVerdictSpec {
                        owner,
                        left: &left.id,
                        right: &right.id,
                        verdict: &verdict,
                        evidence: None,
                        quotes_sha256: None,
                        seen_quotes_sha256: None,
                        at,
                    },
                )?;
            }
            EntailmentVerdict::Contradicts => {
                table.unrelated.remove(&pair);
                table.unrelated_quote_hashes.remove(&pair);
                table.contradicts.insert(pair);
                persist_alignment_verdict(
                    store,
                    &AlignmentVerdictSpec {
                        owner,
                        left: &left.id,
                        right: &right.id,
                        verdict: &verdict,
                        evidence: None,
                        quotes_sha256: None,
                        seen_quotes_sha256: None,
                        at,
                    },
                )?;
            }
            EntailmentVerdict::Unrelated => {
                table.unrelated.insert(pair.clone());
                let seen = table.unrelated_quote_hashes.entry(pair).or_default();
                seen.insert(quotes_hash.clone());
                persist_alignment_verdict(
                    store,
                    &AlignmentVerdictSpec {
                        owner,
                        left: &left.id,
                        right: &right.id,
                        verdict: &verdict,
                        evidence: None,
                        quotes_sha256: Some(&quotes_hash),
                        seen_quotes_sha256: Some(seen),
                        at,
                    },
                )?;
            }
            EntailmentVerdict::Ambiguous { question } => {
                table.unrelated.remove(&pair);
                table.unrelated_quote_hashes.remove(&pair);
                table.ambiguous.insert(pair, question.clone());
                persist_alignment_verdict(
                    store,
                    &AlignmentVerdictSpec {
                        owner,
                        left: &left.id,
                        right: &right.id,
                        verdict: &verdict,
                        evidence: None,
                        quotes_sha256: None,
                        seen_quotes_sha256: None,
                        at,
                    },
                )?;
            }
        }
    }
    Ok((verdict, true))
}

fn merged_concept_map(
    store: &Store,
    explicit: &[CommunityConcept],
) -> Result<BTreeMap<ConceptId, CommunityConcept>> {
    let mut map: BTreeMap<ConceptId, CommunityConcept> = store
        .exchange_active_concepts()?
        .into_iter()
        .map(|c| (c.id.clone(), c))
        .collect();
    for c in explicit {
        map.insert(c.id.clone(), c.clone());
    }
    Ok(map)
}

/// Checks active `Event` pairs for semantic contradiction (e.g. `"航班恢复"` vs `"航班取消"`) and
/// supersedes contradicted earlier events, closing their validity window so dependent conclusions vanish.
pub async fn reconcile_contradicted_events(
    store: &mut Store,
    llm: &dyn LlmClient,
    concepts: &[CommunityConcept],
    at: EpochMs,
) -> Result<Vec<FactId>> {
    let owner = store.status()?.owner_actor;
    let concept_map = merged_concept_map(store, concepts)?;
    let graph = store.derivation_view()?;
    let mut table = ConceptAlignmentTable::from_graph(&graph);

    let mut active_events: Vec<Fact> = graph
        .facts
        .values()
        .filter(|f| {
            f.kind == FactKind::Event
                && f.status == FactStatus::Active
                && !is_system_internal_fact(f)
        })
        .cloned()
        .collect();
    active_events.sort_by(|a, b| {
        a.access
            .recorded_at()
            .cmp(&b.access.recorded_at())
            .then_with(|| a.id.cmp(&b.id))
    });

    let mut superseded_ids = Vec::new();
    let mut already_superseded = BTreeSet::new();

    for i in 0..active_events.len() {
        for j in (i + 1)..active_events.len() {
            let older = &active_events[i];
            let newer = &active_events[j];
            if already_superseded.contains(&older.id) || already_superseded.contains(&newer.id) {
                continue;
            }
            let (FactAccess::Base { stamp: old_s, .. }, FactAccess::Base { stamp: new_s, .. }) =
                (&older.access, &newer.access)
            else {
                continue;
            };
            if old_s.valid.intersect(&new_s.valid).is_none() {
                continue;
            }
            if !event_audience_covers(&graph, newer, older) {
                continue;
            }
            let (Some(old_cid), Some(new_cid)) = (
                fact_condition_concept_id(older),
                fact_condition_concept_id(newer),
            ) else {
                continue;
            };
            if old_cid == new_cid {
                continue;
            }
            let (Some(old_c), Some(new_c)) = (concept_map.get(&old_cid), concept_map.get(&new_cid))
            else {
                continue;
            };
            let new_quote = fact_quote_or_claim(newer);
            let old_quote = fact_quote_or_claim(older);
            let (verdict, premises_live) = judge_pair_cached(
                store,
                llm,
                JudgePairInput {
                    owner: &owner,
                    left: new_c,
                    right: old_c,
                    left_quote: new_quote.as_deref(),
                    right_quote: old_quote.as_deref(),
                    evidence: AlignmentEvidence::ContradictedEvents { newer, older },
                    at,
                },
                &mut table,
            )
            .await?;
            if premises_live && verdict == EntailmentVerdict::Contradicts {
                let marker_id = alloc_opaque_fact_id()?;
                let marker = Fact {
                    id: marker_id.clone(),
                    origin_library: store.library_id().clone(),
                    logical_key: marker_id.as_str().parse()?,
                    kind: FactKind::Observation,
                    subject: older.subject.clone(),
                    predicate: EVENT_SUPERSEDED_PREDICATE.to_owned(),
                    object: json!({
                        "superseded_event_id": older.id,
                        "contradicted_by_event_id": newer.id,
                    }),
                    tags: BTreeSet::new(),
                    author: newer.author.clone(),
                    ingested_by: None,
                    trust_tier: TrustTier::ToolOrModelVerified,
                    access: FactAccess::Base {
                        audience: AudienceSpec::Explicit(BTreeSet::new()),
                        stamp: BitemporalStamp {
                            valid: new_s.valid,
                            recorded_at: new_s.recorded_at.max(at),
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
                store.supersede(&older.id, &marker)?;
                already_superseded.insert(older.id.clone());
                superseded_ids.push(older.id.clone());
            }
        }
    }
    Ok(superseded_ids)
}

/// Aligns open concepts across `store` via `LlmClient::judge_entailment`, supersedes contradicted
/// events, derives person-targeted `CollisionWarning`s over `GovernedScope`, and emits non-intrusive
/// `ReviewItem`s (`ConceptAlignment` and cross-person `AmbiguousScope`).
pub async fn align_concepts_and_derive(
    store: &mut Store,
    llm: &dyn LlmClient,
    concepts: &[CommunityConcept],
    existing_open_reviews: &[ReviewItem],
    at: EpochMs,
) -> Result<CollisionDeriveOutcome> {
    let superseded_contradicted_events =
        reconcile_contradicted_events(store, llm, concepts, at).await?;

    let owner = store.status()?.owner_actor;
    let concept_map = merged_concept_map(store, concepts)?;

    let graph = store.derivation_view()?;
    let mut table = ConceptAlignmentTable::from_graph(&graph);
    let mut emitted_reviews = Vec::new();

    // 1) Align Group concepts across Membership & Constraint scopes (including broader_candidate),
    //    checking route-policy compatibility before allowing external LLM egress.
    let client_policy = llm.route_policy();
    let mut membership_group_policies: BTreeMap<ConceptId, BTreeSet<ModelRoutePolicy>> =
        BTreeMap::new();
    for f in graph
        .facts
        .values()
        .filter(|f| f.kind == FactKind::Membership && f.status == FactStatus::Active)
    {
        if let Ok(rel) = serde_json::from_value::<MembershipRelation>(f.object.clone()) {
            let f_pol = store.fact_effective_route_policy(f)?;
            membership_group_policies
                .entry(rel.group_concept_id)
                .or_default()
                .insert(f_pol);
        }
    }

    let mut constraint_group_policies: BTreeMap<ConceptId, BTreeSet<ModelRoutePolicy>> =
        BTreeMap::new();
    for c in graph
        .facts
        .values()
        .filter(|f| f.kind == FactKind::Constraint && f.status == FactStatus::Active)
    {
        let c_pol = store.fact_effective_route_policy(c)?;
        if let Some(GovernedScope::RelationGroup {
            group_concept_id, ..
        }) = &c.governed_scope
        {
            constraint_group_policies
                .entry(group_concept_id.clone())
                .or_default()
                .insert(c_pol.clone());
        }
        if let Some(broader) = &c.broader_scope {
            if let GovernedScope::RelationGroup {
                group_concept_id, ..
            } = &broader.broader_candidate
            {
                constraint_group_policies
                    .entry(group_concept_id.clone())
                    .or_default()
                    .insert(c_pol);
            }
        }
    }

    for (m_gid, m_pols) in &membership_group_policies {
        for (c_gid, c_pols) in &constraint_group_policies {
            if m_gid == c_gid {
                continue;
            }
            if let (Some(mc), Some(cc)) = (concept_map.get(m_gid), concept_map.get(c_gid)) {
                if mc.kind == ConceptKind::Group && cc.kind == ConceptKind::Group {
                    let route_compatible = client_policy != ModelRoutePolicy::RawOnly
                        && m_pols.contains(&client_policy)
                        && c_pols.contains(&client_policy);
                    let _ = judge_pair_cached(
                        store,
                        llm,
                        JudgePairInput {
                            owner: &owner,
                            left: mc,
                            right: cc,
                            left_quote: None,
                            right_quote: None,
                            evidence: AlignmentEvidence::LabelOnly { route_compatible },
                            at,
                        },
                        &mut table,
                    )
                    .await?;
                }
            }
        }
    }

    // 2) Align Event condition concepts against Constraint condition concepts where audiences & windows intersect.
    let events: Vec<Fact> = graph
        .facts
        .values()
        .filter(|f| {
            f.kind == FactKind::Event
                && f.status == FactStatus::Active
                && !is_system_internal_fact(f)
        })
        .cloned()
        .collect();
    let constraints: Vec<Fact> = graph
        .facts
        .values()
        .filter(|f| f.kind == FactKind::Constraint && f.status == FactStatus::Active)
        .cloned()
        .collect();

    for event in &events {
        for constraint in &constraints {
            if common_windows(&graph, &[event, constraint]).is_empty() {
                continue;
            }
            let shared_viewers: BTreeSet<ActorId> = effective_fact_viewers(&graph, event)
                .intersection(&effective_fact_viewers(&graph, constraint))
                .cloned()
                .collect();
            if shared_viewers.is_empty() {
                continue;
            }
            let (Some(e_cid), Some(c_cid)) = (
                fact_condition_concept_id(event),
                fact_condition_concept_id(constraint),
            ) else {
                continue;
            };
            if e_cid == c_cid {
                continue;
            }
            let (Some(ec), Some(cc)) = (concept_map.get(&e_cid), concept_map.get(&c_cid)) else {
                continue;
            };
            let ev_quote = fact_quote_or_claim(event);
            let con_quote = fact_quote_or_claim(constraint);
            let (verdict, premises_live) = judge_pair_cached(
                store,
                llm,
                JudgePairInput {
                    owner: &owner,
                    left: ec,
                    right: cc,
                    left_quote: ev_quote.as_deref(),
                    right_quote: con_quote.as_deref(),
                    evidence: AlignmentEvidence::EventConstraint { event, constraint },
                    at,
                },
                &mut table,
            )
            .await?;
            if !premises_live {
                continue;
            }
            if let EntailmentVerdict::Ambiguous { question } = verdict {
                let fresh_graph = store.derivation_view()?;
                let (Some(f_ev), Some(f_con)) = (
                    fresh_graph.facts.get(&event.id),
                    fresh_graph.facts.get(&constraint.id),
                ) else {
                    continue;
                };
                let fresh_shared: BTreeSet<ActorId> = effective_fact_viewers(&fresh_graph, f_ev)
                    .intersection(&effective_fact_viewers(&fresh_graph, f_con))
                    .cloned()
                    .collect();
                if fresh_shared.is_empty() {
                    continue;
                }
                // Only surface ConceptAlignment review to an actor who can see BOTH premises!
                let review_actor = if fresh_shared.contains(&f_con.author) {
                    Some(f_con.author.clone())
                } else if fresh_shared.contains(&f_ev.author) {
                    Some(f_ev.author.clone())
                } else {
                    fresh_shared.iter().next().cloned()
                };
                if let Some(actor) = review_actor {
                    let target_id = format!("{}->{}->{}:{}", ec.id, cc.id, event.id, constraint.id);
                    let already_open = existing_open_reviews
                        .iter()
                        .chain(emitted_reviews.iter())
                        .any(|r| {
                            r.actor == actor
                                && r.kind == ReviewKind::ConceptAlignment
                                && r.target_id == target_id
                                && r.status == ReviewStatus::Open
                        });
                    if !already_open {
                        emitted_reviews.push(ReviewItem {
                            id: alloc_opaque_review_id()?,
                            actor,
                            kind: ReviewKind::ConceptAlignment,
                            target_id,
                            question,
                            options: vec![
                                "entails".into(),
                                "unrelated".into(),
                                "contradicts".into(),
                            ],
                            payload: json!({
                                "left_concept_id": ec.id,
                                "left_label": ec.canonical_label,
                                "right_concept_id": cc.id,
                                "right_label": cc.canonical_label,
                                "event_fact_id": event.id,
                                "constraint_fact_id": constraint.id,
                            }),
                            status: ReviewStatus::Open,
                            created_at: at,
                            resolved_at: None,
                        });
                    }
                }
            }
        }
    }

    // 3) Run deterministic hard-rule derivation (including governed person-targeted collisions).
    let DeriveResult {
        derived: derived_warnings,
        justifications,
    } = derive(store)?;

    // 4) Evaluate Cross-Person Impact Review Trigger for constraints with `broader_scope`.
    let refreshed_graph = store.derivation_view()?;
    let refreshed_table = ConceptAlignmentTable::from_graph(&refreshed_graph);
    let scope_reviews = evaluate_ambiguous_scope_reviews(
        &refreshed_graph,
        &refreshed_table,
        existing_open_reviews,
        at,
    )?;
    emitted_reviews.extend(scope_reviews);

    Ok(CollisionDeriveOutcome {
        derived_warnings,
        justifications,
        superseded_contradicted_events,
        emitted_reviews,
    })
}
