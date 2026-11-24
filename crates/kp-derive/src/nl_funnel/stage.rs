use std::collections::BTreeSet;

use kp_model::{
    contains_explicit_numeric_date, derived_fact, justification, ActorId, AnchorSpec, AudienceSpec,
    BitemporalStamp, BroaderScopeCandidate, CommunityConcept, ConceptId, ConceptKind, ContentHash,
    DerivationMode, DerivationView, EpochMs, ExtractionBatchOutcome, ExtractionBatchRequest, Fact,
    FactAccess, FactId, FactKind, FactRank, FactStatus, GovernedScope, KpError, LlmClient,
    MembershipRelation, ModelConceptRef, ModelExtractedAtom, ModelRoutePolicy, ModelScopeRef,
    ModelWriteOp, RejectionEvidenceKey, RejectionSemanticKey, TrustTier, ValidityWindow,
};
use kp_store::{FactInput, Result, Store};
use serde_json::{json, Value};

use super::{
    alloc_opaque_concept_id, alloc_opaque_fact_id, resolve_actor_mention, DroppedRejectedCandidate,
    NlBatchStatus, NlFunnelOptions, NlFunnelOutcome, RejectedModelAtom,
};
use crate::prompts::{is_near_duplicate, is_trivial_turn_text, normalize_for_dedup};

/// Runs `LlmClient::extract_batch` under `options.route_policy` and applies validated atoms to `store`.
pub async fn extract_and_ingest_batch(
    store: &mut Store,
    llm: &dyn LlmClient,
    request: &ExtractionBatchRequest,
    options: &NlFunnelOptions,
) -> Result<NlFunnelOutcome> {
    request.validate_request()?;
    let merged_concepts = merge_initial_concepts(&request.active_concepts, &options.extra_concepts);
    if options.route_policy == ModelRoutePolicy::RawOnly {
        return Ok(NlFunnelOutcome::empty(
            NlBatchStatus::SkippedRawOnlyPolicy,
            merged_concepts,
        ));
    }

    let outcome = llm.extract_batch(request).await?;
    match outcome {
        ExtractionBatchOutcome::SkippedEmpty => Ok(NlFunnelOutcome::empty(
            NlBatchStatus::SkippedEmpty,
            merged_concepts,
        )),
        ExtractionBatchOutcome::SkippedLowQuota { .. } => Ok(NlFunnelOutcome::empty(
            NlBatchStatus::SkippedLowQuota,
            merged_concepts,
        )),
        ExtractionBatchOutcome::Completed { atoms } => {
            apply_extracted_atoms_with_alignment(store, Some(llm), request, &atoms, options).await
        }
    }
}

/// Synchronously validates, filters against dual-key `RejectionRecord`s, resolves concepts/scopes,
/// and stages all resulting writes in memory without mutating `store` (so lease-fenced callers can
/// commit facts, supersessions, derivations, concepts, and job completion in one transaction).
pub fn stage_extracted_atoms(
    store: &Store,
    request: &ExtractionBatchRequest,
    atoms: &[ModelExtractedAtom],
    options: &NlFunnelOptions,
) -> Result<(NlFunnelOutcome, kp_store::StagedExtractionBatch)> {
    stage_extracted_atoms_inner(store, request, atoms, options)
}

/// Synchronously validates, filters against dual-key `RejectionRecord`s, resolves concepts/scopes,
/// and persists `atoms` into `store`.
pub fn apply_extracted_atoms(
    store: &mut Store,
    request: &ExtractionBatchRequest,
    atoms: &[ModelExtractedAtom],
    options: &NlFunnelOptions,
) -> Result<NlFunnelOutcome> {
    let (outcome, staged) = stage_extracted_atoms_inner(store, request, atoms, options)?;
    store.commit_staged_extraction_batch(&staged)?;
    Ok(outcome)
}

async fn apply_extracted_atoms_with_alignment(
    store: &mut Store,
    llm: Option<&dyn LlmClient>,
    request: &ExtractionBatchRequest,
    atoms: &[ModelExtractedAtom],
    options: &NlFunnelOptions,
) -> Result<NlFunnelOutcome> {
    let outcome = apply_extracted_atoms(store, request, atoms, options)?;
    if let Some(client) = llm {
        // Check if any newly added Event contradicts an earlier active Event (e.g. "航班恢复" -> "航班取消").
        crate::collision::reconcile_contradicted_events(
            store,
            client,
            &outcome.active_concepts,
            options.at,
        )
        .await?;
    }
    Ok(outcome)
}

fn merge_initial_concepts(
    req_concepts: &[CommunityConcept],
    extra: &[CommunityConcept],
) -> Vec<CommunityConcept> {
    let mut out = req_concepts.to_vec();
    for c in extra {
        if !out.iter().any(|existing| existing.id == c.id) {
            out.push(c.clone());
        }
    }
    out
}

fn resolve_or_stage_concept(
    concept_ref: &ModelConceptRef,
    expected_kind: ConceptKind,
    speaker: &ActorId,
    at: EpochMs,
    working: &[CommunityConcept],
    staged: &mut Vec<CommunityConcept>,
) -> kp_model::Result<CommunityConcept> {
    match concept_ref {
        ModelConceptRef::Existing { concept_id } => working
            .iter()
            .chain(staged.iter())
            .find(|c| c.id == *concept_id && c.kind == expected_kind)
            .cloned()
            .ok_or_else(|| {
                KpError::invalid("concept_id", "referenced concept missing or wrong kind")
            }),
        ModelConceptRef::ProposeNew {
            kind,
            label,
            domain,
            polarity,
        } => {
            if *kind != expected_kind {
                return Err(KpError::invalid(
                    "concept_kind",
                    "proposed concept kind mismatch",
                ));
            }
            let clean_label = label.trim();
            if let Some(existing) = working.iter().chain(staged.iter()).find(|c| {
                c.kind == *kind
                    && c.polarity == *polarity
                    && (normalize_for_dedup(&c.canonical_label) == normalize_for_dedup(clean_label)
                        || is_near_duplicate(&c.canonical_label, clean_label))
            }) {
                return Ok(existing.clone());
            }
            let created = CommunityConcept {
                id: alloc_opaque_concept_id()?,
                kind: *kind,
                canonical_label: clean_label.to_owned(),
                domain: domain.trim().to_owned(),
                polarity: *polarity,
                proposed_by: speaker.clone(),
                created_at: at,
            };
            staged.push(created.clone());
            Ok(created)
        }
    }
}

fn effective_atom_kind(atom: &ModelExtractedAtom) -> FactKind {
    let has_cross_person_scope = atom
        .ambiguous_scope
        .as_ref()
        .is_some_and(|amb| !matches!(amb.broader_candidate, ModelScopeRef::SpeakerSelf))
        || (atom.ambiguous_scope.is_none()
            && matches!(
                atom.scope,
                Some(
                    ModelScopeRef::NamedMentions { .. }
                        | ModelScopeRef::RelationGroup { .. }
                        | ModelScopeRef::Resource { .. }
                )
            ));
    if atom.kind != FactKind::Membership
        && has_cross_person_scope
        && atom.condition_concept.is_some()
        && atom.action_concept.is_some()
    {
        FactKind::Constraint
    } else {
        atom.kind
    }
}

struct ScopeRefResolutionCtx<'a> {
    speaker: &'a ActorId,
    at: EpochMs,
    graph: &'a DerivationView,
    visible_to_speaker: &'a BTreeSet<FactId>,
    working: &'a [CommunityConcept],
}

fn resolve_scope_ref_lenient(
    scope_ref: &ModelScopeRef,
    ctx: &ScopeRefResolutionCtx<'_>,
    staged: &mut Vec<CommunityConcept>,
) -> Result<(Option<GovernedScope>, Option<String>)> {
    let ScopeRefResolutionCtx {
        speaker,
        at,
        graph,
        visible_to_speaker,
        working,
    } = *ctx;
    match scope_ref {
        ModelScopeRef::SpeakerSelf => Ok((
            Some(GovernedScope::SelfOnly {
                actor: speaker.clone(),
            }),
            None,
        )),
        ModelScopeRef::NamedMentions { mentions } => {
            let mut actors = BTreeSet::new();
            let mut unresolved = Vec::new();
            for mention in mentions {
                match resolve_actor_mention(mention, graph, visible_to_speaker) {
                    Ok(actor) => {
                        actors.insert(actor);
                    }
                    Err(err) => {
                        unresolved.push(format!("{mention}: {err}"));
                    }
                }
            }
            if !unresolved.is_empty() || actors.is_empty() {
                Ok((
                    None,
                    Some(format!(
                        "unresolved mentions downgraded to self_only: {}",
                        unresolved.join("; ")
                    )),
                ))
            } else {
                Ok((Some(GovernedScope::Actors { actors }), None))
            }
        }
        ModelScopeRef::RelationGroup { group_concept } => {
            let concept = resolve_or_stage_concept(
                group_concept,
                ConceptKind::Group,
                speaker,
                at,
                working,
                staged,
            )?;
            Ok((
                Some(GovernedScope::RelationGroup {
                    anchor: speaker.clone(),
                    group_concept_id: concept.id,
                }),
                None,
            ))
        }
        ModelScopeRef::Resource { target } => {
            let clean = target.trim();
            if clean.is_empty() {
                Ok((
                    None,
                    Some("empty resource target downgraded to self_only".into()),
                ))
            } else {
                Ok((
                    Some(GovernedScope::Resource {
                        target: clean.to_owned(),
                    }),
                    None,
                ))
            }
        }
    }
}

fn default_predicate_for_kind(kind: FactKind) -> &'static str {
    match kind {
        FactKind::Event => "kp.core.schedule",
        FactKind::Constraint => "kp.core.constraint",
        FactKind::Decision => "kp.core.decision",
        FactKind::Membership => "kp.core.membership",
        FactKind::ClosedNegative => "kp.code.pitfall",
        FactKind::FactSlot => "kp.core.fact",
        FactKind::Observation
        | FactKind::Alias
        | FactKind::DerivedFact
        | FactKind::CollisionWarning
        | FactKind::Conflict
        | FactKind::Synthesis
        | FactKind::PlaybookBullet
        | FactKind::SkillVersion => "kp.core.observation",
    }
}

struct ExtractedAudienceInput<'a> {
    base_audience: &'a AudienceSpec,
    has_source_origin_ceiling: bool,
    kind: FactKind,
    speaker: &'a ActorId,
    membership_member: Option<&'a ActorId>,
    governed_scope: Option<&'a GovernedScope>,
    has_pending_broader_scope: bool,
}

fn compute_extracted_fact_audience(
    input: &ExtractedAudienceInput<'_>,
    graph: &DerivationView,
) -> AudienceSpec {
    let ExtractedAudienceInput {
        base_audience,
        has_source_origin_ceiling,
        kind,
        speaker,
        membership_member,
        governed_scope,
        has_pending_broader_scope,
    } = *input;
    if has_source_origin_ceiling {
        return base_audience.clone();
    }
    let AudienceSpec::Explicit(current) = base_audience else {
        return base_audience.clone();
    };
    let mut expanded = current.clone();
    expanded.insert(speaker.clone());
    if kind == FactKind::Membership {
        if let Some(member) = membership_member {
            if graph.members.contains(member) {
                expanded.insert(member.clone());
            }
        }
        return AudienceSpec::Explicit(expanded);
    }
    if has_pending_broader_scope {
        return AudienceSpec::Explicit(expanded);
    }
    match governed_scope {
        Some(GovernedScope::Actors { actors }) => {
            for a in actors {
                if graph.members.contains(a) {
                    expanded.insert(a.clone());
                }
            }
        }
        Some(GovernedScope::RelationGroup {
            anchor,
            group_concept_id,
        }) => {
            let alignment = crate::ConceptAlignmentTable::from_graph(graph);
            for f in graph.facts.values() {
                if f.status == FactStatus::Active
                    && f.kind == FactKind::Membership
                    && f.author == *anchor
                {
                    if let Ok(rel) = serde_json::from_value::<MembershipRelation>(f.object.clone())
                    {
                        if &rel.anchor == anchor
                            && alignment.entails(&rel.group_concept_id, group_concept_id)
                            && graph.members.contains(&rel.member)
                        {
                            expanded.insert(rel.member);
                        }
                    }
                }
            }
        }
        Some(GovernedScope::SelfOnly { .. } | GovernedScope::Resource { .. }) | None => {}
    }
    AudienceSpec::Explicit(expanded)
}

fn is_slice_byte_identical(store: &Store, slice: &kp_model::ExtractionChunkSlice) -> Result<bool> {
    if let Some(mt) = store.source_media_type(&slice.source_id)? {
        Ok(kp_model::is_byte_identical_media_type(&mt))
    } else {
        let base = &slice.base_anchor;
        Ok(base.page.is_none()
            && base.slide.is_none()
            && base.sheet.is_none()
            && base.cell.is_none()
            && base.paragraph.is_none())
    }
}

fn stage_extracted_atoms_inner(
    store: &Store,
    request: &ExtractionBatchRequest,
    atoms: &[ModelExtractedAtom],
    options: &NlFunnelOptions,
) -> Result<(NlFunnelOutcome, kp_store::StagedExtractionBatch)> {
    request.validate_request()?;
    if matches!(options.audience, AudienceSpec::SemiringDerived) {
        return Err(KpError::invalid(
            "audience",
            "extracted base facts require an explicit or all-member audience",
        )
        .into());
    }
    let has_any_tagged = request
        .chunks
        .iter()
        .any(|c| kp_model::has_tagged_turns(&c.body));
    let distinct_target_authors: BTreeSet<&ActorId> = request
        .chunks
        .iter()
        .filter(|c| c.role == kp_model::SliceRole::Target)
        .map(|c| &c.author)
        .collect();
    if let Some(origin) = &options.source_origin {
        if !has_any_tagged
            && distinct_target_authors.len() <= 1
            && origin.speaker != request.speaker
        {
            return Err(KpError::invalid(
                "source_origin",
                "SourceOriginMeta.speaker must match ExtractionBatchRequest.speaker on untagged chunks",
            )
            .into());
        }
    }

    let mut working_concepts =
        merge_initial_concepts(&request.active_concepts, &options.extra_concepts);
    let mut working_graph = store.derivation_view()?;
    let mut staged_batch = kp_store::StagedExtractionBatch::default();
    let mut outcome = NlFunnelOutcome::empty(NlBatchStatus::Completed, Vec::new());

    const SECURITY_ABORT_FIELDS: &[&str] = &["write_op", "assumed_premise_ids", "speaker"];

    let mut validated_atoms = Vec::with_capacity(atoms.len());
    for atom in atoms {
        let checked = request.validate_model_atom_with_evidence(atom).and_then(
            |(verified, verified_evidence, atom_speaker)| {
                let resolved_window = request.resolve_atom_window(atom)?;
                Ok((verified, verified_evidence, atom_speaker, resolved_window))
            },
        );
        match checked {
            Ok((verified, verified_evidence, atom_speaker, resolved_window)) => {
                validated_atoms.push((
                    atom,
                    verified,
                    verified_evidence,
                    atom_speaker,
                    resolved_window,
                ));
            }
            Err(KpError::InvalidInput { field, reason })
                if !SECURITY_ABORT_FIELDS.contains(&field) =>
            {
                outcome.rejected_atoms.push(RejectedModelAtom {
                    claim_text: atom.claim_text.clone(),
                    chunk_id: atom.quote.chunk_id.clone(),
                    verbatim_quote: atom.quote.verbatim_quote.clone(),
                    reason: reason.to_owned(),
                });
            }
            Err(err) => return Err(err.into()),
        }
    }

    let mut batch_seen_claims: Vec<(ActorId, FactKind, String)> = Vec::new();

    for (atom, verified_quote, verified_evidence_opt, atom_speaker, resolved_window) in
        validated_atoms
    {
        let is_noop_quote = match &verified_evidence_opt {
            Some(ev) => is_trivial_turn_text(&ev.matched.exact_quote),
            None => is_trivial_turn_text(&verified_quote.exact_quote),
        };
        if atom.write_op == ModelWriteOp::None || is_noop_quote {
            outcome.skipped_noop += 1;
            continue;
        }

        let speaker_tier = if atom_speaker.as_str().starts_with("ext.") {
            TrustTier::ExternalUntrusted
        } else {
            TrustTier::ToolOrModelVerified
        };
        let effective_trust_tier = options.source_trust_floor.weakest(speaker_tier);

        let effective_kind = effective_atom_kind(atom);
        if effective_trust_tier == TrustTier::ExternalUntrusted
            && !matches!(effective_kind, FactKind::Observation | FactKind::FactSlot)
        {
            outcome.rejected_atoms.push(RejectedModelAtom {
                claim_text: atom.claim_text.clone(),
                chunk_id: atom.quote.chunk_id.clone(),
                verbatim_quote: atom.quote.verbatim_quote.clone(),
                reason: "untrusted source or unconfirmed external speaker restricted to observation or fact_slot"
                    .to_owned(),
            });
            continue;
        }
        if effective_kind == FactKind::Event && atom.condition_concept.is_none() {
            outcome.rejected_atoms.push(RejectedModelAtom {
                claim_text: atom.claim_text.clone(),
                chunk_id: atom.quote.chunk_id.clone(),
                verbatim_quote: atom.quote.verbatim_quote.clone(),
                reason: "event missing required condition_concept".to_owned(),
            });
            continue;
        }

        let chunk = request
            .chunks
            .iter()
            .find(|c| c.chunk_id == atom.quote.chunk_id)
            .ok_or_else(|| KpError::invalid("chunk_id", "missing chunk"))?;
        let chunk_sha256 = ContentHash::digest(&chunk.body);

        let candidate_evidence = RejectionEvidenceKey {
            speaker: atom_speaker.clone(),
            chunk_sha256: chunk_sha256.clone(),
            quote_span: verified_quote.span,
        };

        // Pass 1 of dual-key rejection check: span-overlap check BEFORE staging concepts,
        // blocking any model rewording on an overlapping quote interval!
        if let Some(hit) = options
            .rejections
            .iter()
            .find(|rec| rec.matches_candidate(Some(&candidate_evidence), None))
        {
            outcome.dropped_rejected.push(DroppedRejectedCandidate {
                matched_rejection_id: hit.id.clone(),
                claim_text: atom.claim_text.clone(),
                chunk_id: atom.quote.chunk_id.clone(),
                quote_span: verified_quote.span,
            });
            continue;
        }

        // Near-duplicate suppression within the same speaker & batch (Graphiti 3-gram MinHash + Jaccard >= 0.9).
        if effective_kind != FactKind::Membership
            && batch_seen_claims.iter().any(|(spk, k, prev)| {
                *spk == atom_speaker
                    && *k == effective_kind
                    && is_near_duplicate(prev, &atom.claim_text)
            })
        {
            outcome.skipped_near_duplicates += 1;
            continue;
        }

        // Attributed speaker's `(viewer, at)` projection bounds alias resolution and premise citation.
        let visible_to_speaker = working_graph.evaluate(&atom_speaker, options.at).facts;

        let resolved_membership_member = if effective_kind == FactKind::Membership {
            let mention = atom
                .member_mention
                .as_deref()
                .ok_or_else(|| KpError::invalid("membership", "missing member_mention"))?;
            match resolve_actor_mention(mention, &working_graph, &visible_to_speaker) {
                Ok(member) => Some(member),
                Err(err) => {
                    outcome.rejected_atoms.push(RejectedModelAtom {
                        claim_text: atom.claim_text.clone(),
                        chunk_id: atom.quote.chunk_id.clone(),
                        verbatim_quote: atom.quote.verbatim_quote.clone(),
                        reason: err.to_string(),
                    });
                    continue;
                }
            }
        } else {
            None
        };

        let mut staged_concepts: Vec<CommunityConcept> = Vec::new();

        let cond_concept = match &atom.condition_concept {
            Some(cref) => Some(resolve_or_stage_concept(
                cref,
                ConceptKind::Condition,
                &atom_speaker,
                options.at,
                &working_concepts,
                &mut staged_concepts,
            )?),
            None => None,
        };
        let act_concept = match &atom.action_concept {
            Some(cref) => Some(resolve_or_stage_concept(
                cref,
                ConceptKind::Action,
                &atom_speaker,
                options.at,
                &working_concepts,
                &mut staged_concepts,
            )?),
            None => None,
        };
        let grp_concept = match &atom.group_concept {
            Some(cref) => Some(resolve_or_stage_concept(
                cref,
                ConceptKind::Group,
                &atom_speaker,
                options.at,
                &working_concepts,
                &mut staged_concepts,
            )?),
            None => None,
        };

        // Ambiguous scope rule: whenever `ambiguous_scope` is present, ALWAYS activate
        // `GovernedScope::SelfOnly { actor: atom_speaker }` and attach `BroaderScopeCandidate`
        // (or downgrade cleanly to `SelfOnly` clearing `broader_scope` if no named mentions resolve).
        let (governed_scope, broader_scope, pending_mention_diag) =
            if let Some(amb) = &atom.ambiguous_scope {
                let narrow_active = GovernedScope::SelfOnly {
                    actor: atom_speaker.clone(),
                };
                let scope_ctx = ScopeRefResolutionCtx {
                    speaker: &atom_speaker,
                    at: options.at,
                    graph: &working_graph,
                    visible_to_speaker: &visible_to_speaker,
                    working: &working_concepts,
                };
                let (resolved_broader, diag) = resolve_scope_ref_lenient(
                    &amb.broader_candidate,
                    &scope_ctx,
                    &mut staged_concepts,
                )?;
                match resolved_broader {
                    Some(broader_candidate) => (
                        Some(narrow_active.clone()),
                        Some(BroaderScopeCandidate {
                            narrow_active,
                            broader_candidate,
                            question: amb.question.trim().to_owned(),
                        }),
                        diag,
                    ),
                    None => (Some(narrow_active), None, diag),
                }
            } else if let Some(scope_ref) = &atom.scope {
                let scope_ctx = ScopeRefResolutionCtx {
                    speaker: &atom_speaker,
                    at: options.at,
                    graph: &working_graph,
                    visible_to_speaker: &visible_to_speaker,
                    working: &working_concepts,
                };
                let (resolved_scope, diag) =
                    resolve_scope_ref_lenient(scope_ref, &scope_ctx, &mut staged_concepts)?;
                let final_scope = resolved_scope.unwrap_or_else(|| GovernedScope::SelfOnly {
                    actor: atom_speaker.clone(),
                });
                (Some(final_scope), None, diag)
            } else if effective_kind == FactKind::Constraint {
                (
                    Some(GovernedScope::SelfOnly {
                        actor: atom_speaker.clone(),
                    }),
                    None,
                    None,
                )
            } else {
                (None, None, None)
            };

        const MAX_SINGLE_DAY_MS: i64 = 25 * 3_600_000;
        let is_multi_day = |w: ValidityWindow| {
            w.valid_until == EpochMs::OPEN_END
                || w.valid_until.0.saturating_sub(w.valid_from.0) > MAX_SINGLE_DAY_MS
        };
        let matches_or_spans_candidates = |rw: ValidityWindow, cands: &[ValidityWindow]| -> bool {
            cands.contains(&rw)
                || cands.iter().any(|start_c| {
                    start_c.valid_from == rw.valid_from
                        && cands.iter().any(|end_c| {
                            end_c.valid_until == rw.valid_until && rw.valid_until > rw.valid_from
                        })
                })
        };
        let independent_mentions = request
            .reference_calendar
            .count_independent_date_mentions(&verified_quote.exact_quote);
        let candidate_windows = if effective_kind == FactKind::Event && independent_mentions >= 2 {
            request
                .reference_calendar
                .candidate_weekday_windows(&verified_quote.exact_quote)
        } else {
            Vec::new()
        };

        let deterministic_weekday_window = (effective_kind == FactKind::Event
            && !contains_explicit_numeric_date(&verified_quote.exact_quote))
        .then(|| {
            if independent_mentions >= 2 && resolved_window.is_none() {
                if let Some(claim_win) = request
                    .reference_calendar
                    .resolve_chinese_weekday_expression(&atom.claim_text)
                    .filter(|cw| matches_or_spans_candidates(*cw, &candidate_windows))
                {
                    return Some(claim_win);
                }
            }
            request
                .reference_calendar
                .resolve_chinese_weekday_expression(&verified_quote.exact_quote)
        })
        .flatten()
        .filter(|det_win| {
            !resolved_window.is_some_and(|rw| {
                (independent_mentions >= 2 && matches_or_spans_candidates(rw, &candidate_windows))
                    || (!request
                        .reference_calendar
                        .has_reschedule_pair(&verified_quote.exact_quote)
                        && is_multi_day(rw)
                        && !is_multi_day(*det_win)
                        && (request
                            .reference_calendar
                            .count_date_or_weekday_tokens(&verified_quote.exact_quote)
                            > 1
                            || rw.valid_from == det_win.valid_from))
            })
        });

        let valid_window = match deterministic_weekday_window.or(resolved_window) {
            Some(win) => win,
            None => {
                if effective_kind == FactKind::Event {
                    request
                        .reference_calendar
                        .resolve_relative_expression(&verified_quote.exact_quote)
                        .or_else(|| {
                            request
                                .reference_calendar
                                .resolve_relative_expression(&atom.claim_text)
                        })
                        .unwrap_or(ValidityWindow {
                            valid_from: request.reference_calendar.reference_at,
                            valid_until: EpochMs::OPEN_END,
                        })
                } else {
                    ValidityWindow {
                        valid_from: request.reference_calendar.reference_at,
                        valid_until: EpochMs::OPEN_END,
                    }
                }
            }
        };

        let mut semantic_concept_ids = BTreeSet::new();
        if let Some(c) = &cond_concept {
            semantic_concept_ids.insert(c.id.clone());
        }
        if let Some(a) = &act_concept {
            semantic_concept_ids.insert(a.id.clone());
        }
        if let Some(g) = &grp_concept {
            semantic_concept_ids.insert(g.id.clone());
        }

        let candidate_semantic = RejectionSemanticKey {
            kind: effective_kind,
            concept_ids: semantic_concept_ids.clone(),
            governed_scope: governed_scope.clone(),
            valid_window,
        };

        // Pass 2 of dual-key rejection check: normalized semantic key match across chunks/sessions.
        if let Some(hit) = options
            .rejections
            .iter()
            .find(|rec| rec.matches_candidate(Some(&candidate_evidence), Some(&candidate_semantic)))
        {
            outcome.dropped_rejected.push(DroppedRejectedCandidate {
                matched_rejection_id: hit.id.clone(),
                claim_text: atom.claim_text.clone(),
                chunk_id: atom.quote.chunk_id.clone(),
                quote_span: verified_quote.span,
            });
            continue;
        }

        // Verify `assumed_premise_ids` exist in `graph` and are visible to `atom_speaker` at `options.at`.
        for premise_id in &atom.assumed_premise_ids {
            if !visible_to_speaker.contains(premise_id) {
                return Err(KpError::invalid(
                    "assumed_premise_ids",
                    "speaker cannot cite an invisible or inactive decision premise",
                )
                .into());
            }
        }

        // Verify `SupersedeOwn` ownership against actual persisted store state (defense in depth).
        if let ModelWriteOp::SupersedeOwn { target_fact_id } = &atom.write_op {
            let target = working_graph.facts.get(target_fact_id).ok_or_else(|| {
                KpError::invalid("write_op", "SupersedeOwn target fact does not exist")
            })?;
            if target.author != atom_speaker || target.status != FactStatus::Active {
                return Err(KpError::invalid(
                    "write_op",
                    "SupersedeOwn may only supersede an active fact authored by speaker",
                )
                .into());
            }
        }

        // Build `Fact.object` & subject/predicate/entities.
        let mut entities = BTreeSet::new();
        let (subject, predicate, mut object_json) = if effective_kind == FactKind::Membership {
            let group_c = grp_concept
                .as_ref()
                .ok_or_else(|| KpError::invalid("membership", "missing group_concept"))?;
            let member = resolved_membership_member
                .clone()
                .ok_or_else(|| KpError::invalid("membership", "missing resolved member"))?;
            entities.insert(member.as_str().to_lowercase());
            let rel = MembershipRelation {
                anchor: atom_speaker.clone(),
                member,
                group_concept_id: group_c.id.clone(),
            };
            (
                atom_speaker.to_string(),
                "kp.core.membership".to_owned(),
                serde_json::to_value(&rel)?,
            )
        } else {
            let mut obj_map = serde_json::Map::new();
            obj_map.insert(
                "claim_text".into(),
                Value::String(atom.claim_text.trim().to_owned()),
            );
            obj_map.insert("value".into(), atom.value.clone());
            if let Some(c) = &cond_concept {
                obj_map.insert(
                    "condition_concept_id".into(),
                    Value::String(c.id.to_string()),
                );
                entities.insert(c.canonical_label.to_lowercase());
            }
            if let Some(a) = &act_concept {
                obj_map.insert("action_concept_id".into(), Value::String(a.id.to_string()));
                entities.insert(a.canonical_label.to_lowercase());
            }
            if !semantic_concept_ids.is_empty() {
                obj_map.insert(
                    "concept_ids".into(),
                    Value::Array(
                        semantic_concept_ids
                            .iter()
                            .map(|id| Value::String(id.to_string()))
                            .collect(),
                    ),
                );
            }
            let subj = atom
                .value
                .get("subject")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim().to_owned())
                .unwrap_or_else(|| atom_speaker.to_string());
            let pred = atom
                .value
                .get("predicate")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim().to_owned())
                .unwrap_or_else(|| default_predicate_for_kind(effective_kind).to_owned());
            (subj, pred, Value::Object(obj_map))
        };

        if matches!(effective_kind, FactKind::Constraint | FactKind::Decision)
            && effective_trust_tier != TrustTier::ExternalUntrusted
        {
            if let Value::Object(ref mut map) = object_json {
                map.insert("guard_checked".into(), json!(true));
                if let Some(raw_guard) = &atom.command_guard {
                    let guard = raw_guard.normalized();
                    if !guard.is_empty() {
                        map.insert(
                            "forbidden_commands".into(),
                            serde_json::to_value(&guard.forbidden_commands)?,
                        );
                        map.insert(
                            "allowed_commands".into(),
                            serde_json::to_value(&guard.allowed_commands)?,
                        );
                        map.insert(
                            "required_flags".into(),
                            serde_json::to_value(&guard.required_flags)?,
                        );
                        if guard.has_enforced_guard() {
                            map.insert("hard_guard".into(), json!(true));
                        }
                    }
                }
            }
        }

        if options.route_policy != ModelRoutePolicy::HostCli {
            if let Value::Object(ref mut map) = object_json {
                map.insert(
                    "route_policy".into(),
                    serde_json::to_value(&options.route_policy)?,
                );
            }
        }

        if let Some(ref ev) = verified_evidence_opt {
            let ev_slice = request
                .chunks
                .get(ev.slice_index)
                .ok_or_else(|| KpError::invalid("evidence", "missing evidence slice"))?;
            let ev_byte_identical = is_slice_byte_identical(store, ev_slice)?;
            let ev_body_hash = ContentHash::digest(&ev_slice.body);
            let ev_anchor = AnchorSpec::quote_in_chunk(
                ev_slice.source_id.clone(),
                ev_slice.chunk_id.clone(),
                &ev_slice.base_anchor,
                &ev.matched,
                ev_body_hash,
                ev_byte_identical,
            );
            if let Value::Object(ref mut map) = object_json {
                map.insert("evidence_anchor".into(), serde_json::to_value(&ev_anchor)?);
            }
        }

        let fact_id = alloc_opaque_fact_id()?;
        let logical_key = fact_id.as_str().parse()?;
        let byte_identical = is_slice_byte_identical(store, chunk)?;
        let source_anchor = AnchorSpec::quote_in_chunk(
            chunk.source_id.clone(),
            atom.quote.chunk_id.clone(),
            &chunk.base_anchor,
            &verified_quote,
            chunk_sha256,
            byte_identical,
        );

        let (ingested_by, source_scope_id) = match &options.source_origin {
            Some(meta) => (
                Some(meta.ingested_by.clone()),
                Some(meta.source_scope_id.clone()),
            ),
            None => (None, None),
        };

        let fact_audience = compute_extracted_fact_audience(
            &ExtractedAudienceInput {
                base_audience: &options.audience,
                has_source_origin_ceiling: options.source_origin.is_some(),
                kind: effective_kind,
                speaker: &atom_speaker,
                membership_member: resolved_membership_member.as_ref(),
                governed_scope: governed_scope.as_ref(),
                has_pending_broader_scope: broader_scope.is_some(),
            },
            &working_graph,
        );

        let base_fact = Fact {
            id: fact_id.clone(),
            origin_library: store.library_id().clone(),
            logical_key,
            kind: effective_kind,
            subject,
            predicate,
            object: object_json,
            tags: BTreeSet::new(),
            author: atom_speaker.clone(),
            ingested_by,
            trust_tier: effective_trust_tier,
            access: FactAccess::Base {
                audience: fact_audience,
                stamp: BitemporalStamp {
                    valid: valid_window,
                    recorded_at: options.at,
                    expired_at: None,
                },
            },
            source_scope_id,
            governed_scope: governed_scope.clone(),
            broader_scope: broader_scope.clone(),
            status: FactStatus::Active,
            superseded_by: None,
            rank: FactRank::Normal,
            source_anchor: Some(source_anchor.clone()),
            unverified_gaps: Vec::new(),
            miss_count: 0,
        };
        base_fact.validate()?;

        match &atom.write_op {
            ModelWriteOp::Add => {
                staged_batch
                    .writes
                    .push(kp_store::StagedAtomWrite::Remember(FactInput {
                        fact: base_fact.clone(),
                        entities,
                    }));
                working_graph
                    .facts
                    .insert(base_fact.id.clone(), base_fact.clone());
            }
            ModelWriteOp::SupersedeOwn { target_fact_id } => {
                staged_batch
                    .writes
                    .push(kp_store::StagedAtomWrite::Supersede {
                        target_fact_id: target_fact_id.clone(),
                        replacement: base_fact.clone(),
                    });
                if let Some(old_f) = working_graph.facts.get_mut(target_fact_id) {
                    old_f.status = FactStatus::Superseded;
                    old_f.superseded_by = Some(base_fact.id.clone());
                }
                working_graph
                    .facts
                    .insert(base_fact.id.clone(), base_fact.clone());
                outcome.superseded_old_fact_ids.push(target_fact_id.clone());
            }
            ModelWriteOp::None => {}
        }

        if let Some(diag_reason) = pending_mention_diag {
            outcome.rejected_atoms.push(RejectedModelAtom {
                claim_text: atom.claim_text.clone(),
                chunk_id: atom.quote.chunk_id.clone(),
                verbatim_quote: atom.quote.verbatim_quote.clone(),
                reason: diag_reason,
            });
        }

        // Emit `DerivationMode::AssumedPremise` conclusion + justification when active decisions are cited.
        if !atom.assumed_premise_ids.is_empty() {
            let mut premises: Vec<&Fact> = vec![&base_fact];
            for pid in &atom.assumed_premise_ids {
                let pfact = working_graph
                    .facts
                    .get(pid)
                    .ok_or_else(|| KpError::invalid("assumed_premise_ids", "missing premise"))?;
                premises.push(pfact);
            }
            let mut assumed_obj = json!({
                "claim_text": atom.claim_text.trim(),
                "base_fact_id": base_fact.id,
                "assumed_premises": atom.assumed_premise_ids,
                "value": atom.value,
            });
            if options.route_policy != ModelRoutePolicy::HostCli {
                if let Value::Object(ref mut map) = assumed_obj {
                    map.insert(
                        "route_policy".into(),
                        serde_json::to_value(&options.route_policy)?,
                    );
                }
            }
            let mut assumed_conclusion = derived_fact(
                store.library_id(),
                &atom_speaker,
                FactKind::DerivedFact,
                &base_fact.subject,
                "kp.core.assumed_conclusion",
                assumed_obj,
                options.at,
            )?;
            assumed_conclusion.source_anchor = Some(source_anchor);
            let proof = justification(
                &assumed_conclusion,
                DerivationMode::AssumedPremise,
                "NL_ASSUMED_PREMISE",
                &premises,
            )?;
            outcome
                .derived_assumed_fact_ids
                .push(assumed_conclusion.id.clone());
            working_graph
                .facts
                .insert(assumed_conclusion.id.clone(), assumed_conclusion.clone());
            working_graph
                .justifications
                .insert(proof.id.clone(), proof.clone());
            staged_batch.derivations.push((assumed_conclusion, proof));
        }

        let mut linked_concept_ids = semantic_concept_ids;
        if let Some(GovernedScope::RelationGroup {
            group_concept_id, ..
        }) = &governed_scope
        {
            linked_concept_ids.insert(group_concept_id.clone());
        }
        if let Some(b) = &broader_scope {
            if let GovernedScope::RelationGroup {
                group_concept_id, ..
            } = &b.broader_candidate
            {
                linked_concept_ids.insert(group_concept_id.clone());
            }
        }
        for created_c in staged_concepts {
            staged_batch.concepts.push(created_c.clone());
            staged_batch.created_concepts.push(created_c.clone());
            working_concepts.push(created_c.clone());
            outcome.created_concepts.push(created_c);
        }
        for cid in &linked_concept_ids {
            if let Some(existing_c) = working_concepts.iter().find(|c| &c.id == cid) {
                staged_batch.concepts.push(existing_c.clone());
            }
        }
        if !linked_concept_ids.is_empty() {
            let ids_vec: Vec<ConceptId> = linked_concept_ids.into_iter().collect();
            staged_batch
                .fact_concept_links
                .push((fact_id.clone(), ids_vec, options.at));
        }

        batch_seen_claims.push((atom_speaker, effective_kind, atom.claim_text.clone()));
        outcome.created_fact_ids.push(fact_id);
    }

    outcome.active_concepts = working_concepts;
    Ok((outcome, staged_batch))
}
