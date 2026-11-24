//! Narrow-to-domain natural-language extraction funnel (`ModelExtractedAtom` -> governed `Fact`s).
//!
//! Enforces:
//! - Strict narrow boundary validation (`ExtractionBatchRequest::validate_model_atom`);
//! - Width/whitespace-normalized verbatim quote anchoring (`CharSpan` + `chunk_sha256`);
//! - Dual-key sticky rejection filtering (`RejectionRecord::matches_candidate`) immune to LLM rewording;
//! - Opaque UUIDv7 ID allocation (`fact_*`, `cpt_*`, `rev_*`, `rej_*` — never bare content hashes);
//! - Narrow-first `AmbiguousScope` activation (`SelfOnly(speaker)` + `BroaderScopeCandidate`);
//! - Whitelisted `active_decisions` -> `DerivationMode::AssumedPremise` conjunctive justifications;
//! - Speaker-verified `ModelWriteOp::SupersedeOwn`.

mod stage;

pub use stage::{apply_extracted_atoms, extract_and_ingest_batch, stage_extracted_atoms};

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use kp_model::{
    format_opaque_id, format_tagged_turns, partition_chunks_by_token_budget,
    partition_turns_by_token_budget, ActiveDecisionBrief, ActorId, AudienceSpec, CalendarTable,
    CharSpan, ChunkTurn, CommunityConcept, ConceptId, ContentHash, DerivationView, EpochMs,
    ExtractionBatchRequest, ExtractionChunkSlice, Fact, FactAccess, FactId, FactKind, FactStatus,
    GovernedScope, KpError, ModelRoutePolicy, RejectionId, RejectionRecord, ReviewId, SourceId,
    SourceOriginMeta, SpeakerFactBrief, TrustTier, DEFAULT_MAX_BATCH_TOKENS,
    MAX_ACTIVE_DECISIONS_PER_BATCH, MAX_SPEAKER_FACTS_PER_BATCH,
};
use kp_store::{Result, Store};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::prompts::normalize_for_dedup;

/// Allocates a prefixed opaque `FactId` (`fact_<uuidv7>`).
pub fn alloc_opaque_fact_id() -> kp_model::Result<FactId> {
    format_opaque_id("fact", &Uuid::now_v7().to_string())?.parse()
}

/// Allocates a prefixed opaque `ConceptId` (`cpt_<uuidv7>`).
pub fn alloc_opaque_concept_id() -> kp_model::Result<ConceptId> {
    format_opaque_id("cpt", &Uuid::now_v7().to_string())?.parse()
}

/// Allocates a prefixed opaque `ReviewId` (`rev_<uuidv7>`).
pub fn alloc_opaque_review_id() -> kp_model::Result<ReviewId> {
    format_opaque_id("rev", &Uuid::now_v7().to_string())?.parse()
}

/// Allocates a prefixed opaque `RejectionId` (`rej_<uuidv7>`).
pub fn alloc_opaque_rejection_id() -> kp_model::Result<RejectionId> {
    format_opaque_id("rej", &Uuid::now_v7().to_string())?.parse()
}

/// Execution options for applying a narrow extraction batch into a `Store`.
#[derive(Clone, Debug)]
pub struct NlFunnelOptions {
    pub audience: AudienceSpec,
    pub at: EpochMs,
    pub route_policy: ModelRoutePolicy,
    /// Minimum (weakest ceiling) trust tier permitted by the source origin (`ExternalUntrusted` for `http(s)://`).
    pub source_trust_floor: TrustTier,
    /// Optional connector provenance when `ingested_by != speaker` or governed by live `SourceScope`.
    pub source_origin: Option<SourceOriginMeta>,
    /// Durable dual-key rejection records (`rejected_claims`) active in this library.
    pub rejections: Vec<RejectionRecord>,
    /// Extra known community concepts beyond those passed in `request.active_concepts`.
    pub extra_concepts: Vec<CommunityConcept>,
}

impl NlFunnelOptions {
    pub fn personal_or_shared(audience: AudienceSpec, at: EpochMs) -> Self {
        Self {
            audience,
            at,
            route_policy: ModelRoutePolicy::HostCli,
            source_trust_floor: TrustTier::ToolOrModelVerified,
            source_origin: None,
            rejections: Vec::new(),
            extra_concepts: Vec::new(),
        }
    }

    pub fn trust_floor_for_uri(uri: &str) -> TrustTier {
        let trimmed = uri.trim();
        if trimmed
            .get(..7)
            .is_some_and(|p| p.eq_ignore_ascii_case("http://"))
            || trimmed
                .get(..8)
                .is_some_and(|p| p.eq_ignore_ascii_case("https://"))
        {
            TrustTier::ExternalUntrusted
        } else {
            TrustTier::ToolOrModelVerified
        }
    }
}

/// Diagnostic record for an extracted candidate dropped by `RejectionRecord::matches_candidate`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroppedRejectedCandidate {
    pub matched_rejection_id: RejectionId,
    pub claim_text: String,
    pub chunk_id: String,
    pub quote_span: CharSpan,
}

/// Diagnostic record for a model-emitted atom dropped due to non-security localization/date validation failure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectedModelAtom {
    pub claim_text: String,
    pub chunk_id: String,
    pub verbatim_quote: String,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NlBatchStatus {
    Completed,
    SkippedEmpty,
    SkippedLowQuota,
    SkippedRawOnlyPolicy,
}

/// Structured result of running natural-language extraction & domain resolution on a batch.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NlFunnelOutcome {
    pub status: NlBatchStatus,
    pub created_fact_ids: Vec<FactId>,
    pub superseded_old_fact_ids: Vec<FactId>,
    pub derived_assumed_fact_ids: Vec<FactId>,
    pub created_concepts: Vec<CommunityConcept>,
    pub active_concepts: Vec<CommunityConcept>,
    pub dropped_rejected: Vec<DroppedRejectedCandidate>,
    pub rejected_atoms: Vec<RejectedModelAtom>,
    pub skipped_noop: usize,
    pub skipped_near_duplicates: usize,
}

impl NlFunnelOutcome {
    pub(super) fn empty(status: NlBatchStatus, active_concepts: Vec<CommunityConcept>) -> Self {
        Self {
            status,
            created_fact_ids: Vec::new(),
            superseded_old_fact_ids: Vec::new(),
            derived_assumed_fact_ids: Vec::new(),
            created_concepts: Vec::new(),
            active_concepts,
            dropped_rejected: Vec::new(),
            rejected_atoms: Vec::new(),
            skipped_noop: 0,
            skipped_near_duplicates: 0,
        }
    }
}

fn collect_json_concept_ids(value: &Value, out: &mut BTreeSet<ConceptId>) {
    match value {
        Value::String(s) => {
            if let Ok(cid) = s.parse::<ConceptId>() {
                out.insert(cid);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_json_concept_ids(item, out);
            }
        }
        Value::Object(fields) => {
            for (k, v) in fields {
                if let Ok(cid) = k.parse::<ConceptId>() {
                    out.insert(cid);
                }
                collect_json_concept_ids(v, out);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn collect_fact_referenced_concepts(fact: &Fact) -> BTreeSet<ConceptId> {
    let mut out = BTreeSet::new();
    collect_json_concept_ids(&fact.object, &mut out);
    for s in fact
        .tags
        .iter()
        .map(String::as_str)
        .chain([fact.subject.as_str(), fact.predicate.as_str()])
    {
        if let Ok(cid) = s.parse::<ConceptId>() {
            out.insert(cid);
        }
    }
    if let Some(GovernedScope::RelationGroup {
        group_concept_id, ..
    }) = &fact.governed_scope
    {
        out.insert(group_concept_id.clone());
    }
    if let Some(broader) = &fact.broader_scope {
        if let GovernedScope::RelationGroup {
            group_concept_id, ..
        } = &broader.narrow_active
        {
            out.insert(group_concept_id.clone());
        }
        if let GovernedScope::RelationGroup {
            group_concept_id, ..
        } = &broader.broader_candidate
        {
            out.insert(group_concept_id.clone());
        }
    }
    out
}

/// Inputs for constructing a route-policy-isolated [`ExtractionBatchRequest`].
#[derive(Clone, Debug)]
pub struct ExtractionBatchBuildInput {
    pub source_id: SourceId,
    pub speaker: ActorId,
    pub reference_calendar: CalendarTable,
    pub active_concepts: Vec<CommunityConcept>,
    pub chunks: Vec<ExtractionChunkSlice>,
    pub min_rate_limit_remaining_pct: u8,
}

/// Builds a bounded `ExtractionBatchRequest` pre-filtering `active_decisions` (`<= 16`),
/// per-speaker `speaker_own_facts` / `speaker_own_facts_by_actor` (`<= 32` each), and
/// `active_concepts` strictly to the effective [`ModelRoutePolicy`] of `source_id`.
pub fn build_extraction_batch_request(
    store: &Store,
    source_id: SourceId,
    speaker: ActorId,
    reference_calendar: CalendarTable,
    active_concepts: Vec<CommunityConcept>,
    chunks: Vec<ExtractionChunkSlice>,
    min_rate_limit_remaining_pct: u8,
) -> Result<ExtractionBatchRequest> {
    let target_policy = store.source_effective_route_policy(&source_id)?;
    build_extraction_batch_request_for_policy(
        store,
        ExtractionBatchBuildInput {
            source_id,
            speaker,
            reference_calendar,
            active_concepts,
            chunks,
            min_rate_limit_remaining_pct,
        },
        &target_policy,
    )
}

/// Route-policy-isolated variant of [`build_extraction_batch_request`]: excludes any decision,
/// speaker-own fact, or fact-bound concept whose effective route policy differs from `target_policy`
/// (or is `RawOnly`), preventing cross-endpoint or post-`RawOnly`-narrowing prompt leakage.
pub fn build_extraction_batch_request_for_policy(
    store: &Store,
    input: ExtractionBatchBuildInput,
    target_policy: &ModelRoutePolicy,
) -> Result<ExtractionBatchRequest> {
    let ExtractionBatchBuildInput {
        source_id,
        speaker,
        reference_calendar,
        active_concepts,
        chunks,
        min_rate_limit_remaining_pct,
    } = input;
    let graph = store.derivation_view()?;
    let mut batch_speakers = BTreeSet::from([speaker.clone()]);
    for chunk in &chunks {
        if chunk.role != kp_model::SliceRole::Target {
            continue;
        }
        if kp_model::has_tagged_turns(&chunk.body) {
            for turn in kp_model::parse_tagged_turns(&chunk.body) {
                if let (Some(actor), false) = (turn.speaker, turn.is_assistant) {
                    batch_speakers.insert(actor);
                }
            }
        } else {
            batch_speakers.insert(chunk.author.clone());
        }
    }

    let mut shared_visible = graph
        .evaluate(&speaker, reference_calendar.reference_at)
        .facts;
    for other in &batch_speakers {
        if other != &speaker {
            let other_vis = graph.evaluate(other, reference_calendar.reference_at).facts;
            shared_visible.retain(|id| other_vis.contains(id));
        }
    }

    let mut fact_policies: BTreeMap<FactId, ModelRoutePolicy> = BTreeMap::new();
    let mut allowed_concept_ids: BTreeSet<ConceptId> = BTreeSet::new();
    let mut excluded_concept_ids: BTreeSet<ConceptId> = BTreeSet::new();
    for f in graph.facts.values() {
        let pol = store.fact_effective_route_policy(f)?;
        let cids = collect_fact_referenced_concepts(f);
        if *target_policy != ModelRoutePolicy::RawOnly && pol == *target_policy {
            allowed_concept_ids.extend(cids);
        } else {
            excluded_concept_ids.extend(cids);
        }
        fact_policies.insert(f.id.clone(), pol);
    }

    let route_matches = |f: &Fact| -> bool {
        *target_policy != ModelRoutePolicy::RawOnly
            && fact_policies
                .get(&f.id)
                .is_some_and(|pol| pol == target_policy)
    };

    let seeded_cids = if *target_policy == ModelRoutePolicy::HostCli {
        store.explicitly_seeded_concept_ids()?
    } else {
        BTreeSet::new()
    };

    let filtered_concepts: Vec<CommunityConcept> = if *target_policy == ModelRoutePolicy::RawOnly {
        Vec::new()
    } else {
        active_concepts
            .into_iter()
            .filter(|c| {
                allowed_concept_ids.contains(&c.id)
                    || (seeded_cids.contains(&c.id) && !excluded_concept_ids.contains(&c.id))
            })
            .collect()
    };

    let combined_chunk_norm = normalize_for_dedup(
        &chunks
            .iter()
            .map(|c| format!("{} {}", c.heading, c.body))
            .collect::<Vec<_>>()
            .join(" "),
    );
    let chunk_shingles: BTreeSet<String> = crate::prompts::shingles_3gram(&combined_chunk_norm);

    let mut decision_candidates: Vec<(usize, &Fact)> = graph
        .facts
        .values()
        .filter(|f| {
            f.kind == FactKind::Decision
                && f.status == FactStatus::Active
                && shared_visible.contains(&f.id)
                && route_matches(f)
        })
        .map(|f| {
            let text = format!(
                "{} {} {}",
                f.subject,
                f.predicate,
                kp_model::fact_claim_text(f)
            );
            let overlap = crate::prompts::shingles_3gram(&text)
                .intersection(&chunk_shingles)
                .count();
            (overlap, f)
        })
        .collect();
    decision_candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));

    let active_decisions: Vec<ActiveDecisionBrief> = decision_candidates
        .into_iter()
        .take(MAX_ACTIVE_DECISIONS_PER_BATCH)
        .map(|(_, f)| ActiveDecisionBrief {
            fact_id: f.id.clone(),
            subject: f.subject.clone(),
            predicate: f.predicate.clone(),
            value: f.object.clone(),
        })
        .collect();

    let collect_own_for = |actor: &ActorId, require_shared_vis: bool| -> Vec<SpeakerFactBrief> {
        let mut candidates: Vec<(usize, &Fact)> = graph
            .facts
            .values()
            .filter(|f| {
                &f.author == actor
                    && f.status == FactStatus::Active
                    && matches!(f.access, FactAccess::Base { .. })
                    && (!require_shared_vis || shared_visible.contains(&f.id))
                    && route_matches(f)
            })
            .map(|f| {
                let summary = kp_model::fact_claim_text(f);
                let overlap = crate::prompts::shingles_3gram(&summary)
                    .intersection(&chunk_shingles)
                    .count();
                (overlap, f)
            })
            .collect();
        candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
        candidates
            .into_iter()
            .take(MAX_SPEAKER_FACTS_PER_BATCH)
            .map(|(_, f)| SpeakerFactBrief {
                fact_id: f.id.clone(),
                kind: f.kind,
                summary: kp_model::fact_claim_text(f),
            })
            .collect()
    };

    let multi_speaker = batch_speakers.len() > 1;
    let speaker_own_facts = collect_own_for(&speaker, multi_speaker);
    let mut speaker_own_facts_by_actor = BTreeMap::new();
    for actor in &batch_speakers {
        if actor != &speaker {
            let own = collect_own_for(actor, true);
            if !own.is_empty() {
                speaker_own_facts_by_actor.insert(actor.clone(), own);
            }
        }
    }

    let req = ExtractionBatchRequest {
        source_id,
        speaker,
        reference_calendar,
        active_concepts: filtered_concepts,
        active_decisions,
        speaker_own_facts,
        speaker_own_facts_by_actor,
        chunks,
        min_rate_limit_remaining_pct,
    };
    req.validate_request()?;
    Ok(req)
}

/// Parameters for grouping attributed turns or chunks into token-bounded `ExtractionBatchRequest`s.
#[derive(Clone, Debug)]
pub struct TokenBatchSpec {
    pub source_id: SourceId,
    pub default_speaker: ActorId,
    pub reference_calendar: CalendarTable,
    pub active_concepts: Vec<CommunityConcept>,
    pub heading: String,
    /// Token ceiling per batch (`DEFAULT_MAX_BATCH_TOKENS` when `0` is passed).
    pub max_batch_tokens: usize,
    pub min_rate_limit_remaining_pct: u8,
}

/// Groups `turns` by `spec.max_batch_tokens` (rather than turn count), drops ack-only/assistant-only
/// windows, and builds bounded `ExtractionBatchRequest`s ready for `LlmClient::extract_batch`.
pub fn build_token_budgeted_turn_batches(
    store: &Store,
    spec: &TokenBatchSpec,
    turns: &[ChunkTurn],
) -> Result<Vec<ExtractionBatchRequest>> {
    let budget = if spec.max_batch_tokens == 0 {
        DEFAULT_MAX_BATCH_TOKENS
    } else {
        spec.max_batch_tokens
    };
    let windows = partition_turns_by_token_budget(turns, budget);
    let mut requests = Vec::with_capacity(windows.len());
    for (idx, window) in windows.into_iter().enumerate() {
        let body = format_tagged_turns(&window);
        if crate::prompts::is_chunk_trivial_or_ack_only(&body) {
            continue;
        }
        let primary_speaker = window
            .iter()
            .find_map(|t| (!t.is_assistant).then(|| t.speaker.clone()).flatten())
            .unwrap_or_else(|| spec.default_speaker.clone());
        let chunk = ExtractionChunkSlice {
            chunk_id: format!("tb_{idx}"),
            source_id: spec.source_id.clone(),
            author: primary_speaker.clone(),
            base_anchor: Default::default(),
            role: kp_model::SliceRole::Target,
            reply_to_chunk_id: None,
            heading: spec.heading.clone(),
            body,
        };
        requests.push(build_extraction_batch_request(
            store,
            spec.source_id.clone(),
            primary_speaker,
            spec.reference_calendar.clone(),
            spec.active_concepts.clone(),
            vec![chunk],
            spec.min_rate_limit_remaining_pct,
        )?);
    }
    Ok(requests)
}

/// Groups pre-built `ExtractionChunkSlice`s by `spec.max_batch_tokens`, dropping trivial/ack-only
/// windows, and builds bounded `ExtractionBatchRequest`s.
pub fn build_token_budgeted_chunk_batches(
    store: &Store,
    spec: &TokenBatchSpec,
    chunks: &[ExtractionChunkSlice],
) -> Result<Vec<ExtractionBatchRequest>> {
    let budget = if spec.max_batch_tokens == 0 {
        DEFAULT_MAX_BATCH_TOKENS
    } else {
        spec.max_batch_tokens
    };
    let non_trivial: Vec<ExtractionChunkSlice> = chunks
        .iter()
        .filter(|c| !crate::prompts::is_chunk_trivial_or_ack_only(&c.body))
        .cloned()
        .collect();
    let windows = partition_chunks_by_token_budget(&non_trivial, budget);
    let mut requests = Vec::with_capacity(windows.len());
    for window in windows {
        requests.push(build_extraction_batch_request(
            store,
            spec.source_id.clone(),
            spec.default_speaker.clone(),
            spec.reference_calendar.clone(),
            spec.active_concepts.clone(),
            window,
            spec.min_rate_limit_remaining_pct,
        )?);
    }
    Ok(requests)
}

/// Resolves a natural-language mention against `graph.members` and active `FactKind::Alias` facts.
/// Rejects unresolvable or alias-conflicted mentions so LLM output can never smuggle raw `ActorId`s.
///
/// Only `Alias` and `kp.core.alias_conflict` facts contained in `visible_to_speaker` (the speaker's
/// `(viewer, at)` semiring projection) participate, so another member's private aliases can neither
/// resolve a mention (leak) nor block it (cross-viewer denial of service).
pub fn resolve_actor_mention(
    mention: &str,
    graph: &DerivationView,
    visible_to_speaker: &BTreeSet<FactId>,
) -> Result<ActorId> {
    let start_norm = mention.trim().to_lowercase();
    if start_norm.is_empty() {
        return Err(KpError::invalid("mentions", "actor mention must be nonempty").into());
    }

    if let Some(member) = graph
        .members
        .iter()
        .find(|m| m.as_str().to_lowercase() == start_norm)
    {
        return Ok(member.clone());
    }

    let conflicted_pairs: BTreeSet<(String, String)> = graph
        .facts
        .values()
        .filter(|f| {
            f.predicate == "kp.core.alias_conflict"
                && f.status == FactStatus::Active
                && visible_to_speaker.contains(&f.id)
        })
        .filter_map(|f| {
            f.object
                .as_str()
                .map(|other| (f.subject.trim().to_lowercase(), other.trim().to_lowercase()))
        })
        .collect();

    let mut adj: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut raw_tokens: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for fact in graph.facts.values() {
        if fact.kind != FactKind::Alias
            || fact.status != FactStatus::Active
            || fact.access.expired_at().is_some()
            || !visible_to_speaker.contains(&fact.id)
        {
            continue;
        }
        let Some(obj_str) = fact.object.as_str() else {
            continue;
        };
        let left_raw = fact.subject.trim();
        let right_raw = obj_str.trim();
        if left_raw.is_empty() || right_raw.is_empty() {
            continue;
        }
        let left = left_raw.to_lowercase();
        let right = right_raw.to_lowercase();
        if conflicted_pairs.contains(&(left.clone(), right.clone()))
            || conflicted_pairs.contains(&(right.clone(), left.clone()))
        {
            return Err(KpError::invalid(
                "mentions",
                "alias conflict blocks actor mention resolution",
            )
            .into());
        }
        raw_tokens
            .entry(left.clone())
            .or_default()
            .push(left_raw.to_owned());
        raw_tokens
            .entry(right.clone())
            .or_default()
            .push(right_raw.to_owned());
        adj.entry(left.clone()).or_default().push(right.clone());
        adj.entry(right).or_default().push(left);
    }

    let mut visited = BTreeSet::from([start_norm.clone()]);
    let has_alias_entry = adj.contains_key(&start_norm);
    let mut queue = VecDeque::from([start_norm]);
    while let Some(curr) = queue.pop_front() {
        if let Some(neighbors) = adj.get(&curr) {
            for nxt in neighbors {
                if visited.insert(nxt.clone()) {
                    queue.push_back(nxt.clone());
                }
            }
        }
    }

    if visited.len() <= 1 && !has_alias_entry {
        return Err(KpError::invalid(
            "mentions",
            "mention does not match any registered member or alias",
        )
        .into());
    }

    let matched_members: BTreeSet<ActorId> = graph
        .members
        .iter()
        .filter(|m| visited.contains(&m.as_str().to_lowercase()))
        .cloned()
        .collect();
    if matched_members.len() > 1 {
        return Err(
            KpError::invalid("mentions", "ambiguous mention matches multiple members").into(),
        );
    }
    if let Some(member) = matched_members.into_iter().next() {
        return Ok(member);
    }

    // Personal library fallback when an explicit trusted `Alias` maps a name/nickname to an ActorId.
    let mut valid_actor_ids: BTreeSet<ActorId> = BTreeSet::new();
    for norm_node in &visited {
        if let Some(raw_list) = raw_tokens.get(norm_node) {
            for raw in raw_list {
                if let Ok(actor_id) = raw.parse::<ActorId>() {
                    valid_actor_ids.insert(actor_id);
                }
            }
        }
    }
    let distinct_lower: BTreeSet<String> = valid_actor_ids
        .iter()
        .map(|id| id.as_str().to_lowercase())
        .collect();
    if distinct_lower.len() == 1 {
        if let Some(lower_first) = valid_actor_ids
            .iter()
            .find(|id| id.as_str() == id.as_str().to_lowercase())
            .or_else(|| valid_actor_ids.iter().next())
        {
            return Ok(lower_first.clone());
        }
    }

    Err(KpError::invalid(
        "mentions",
        "mention could not be uniquely resolved to a valid ActorId",
    )
    .into())
}

/// Atomically withdraws (or supersedes) `fact_id` in `store` and builds a durable dual-key
/// `RejectionRecord` (`rej_<uuidv7>`) anchored on `(speaker, chunk_sha256, quote_span)` + `RejectionSemanticKey`.
pub fn reject_fact_and_record(
    store: &mut Store,
    fact_id: &FactId,
    rejected_by: &ActorId,
    reason: impl Into<String>,
    replacement: Option<&Fact>,
    chunk_sha256_override: Option<ContentHash>,
    at: EpochMs,
) -> Result<RejectionRecord> {
    let graph = store.derivation_view()?;
    let target = graph
        .facts
        .get(fact_id)
        .ok_or(kp_store::StoreError::NotFound)?;
    let record = RejectionRecord::from_fact(
        alloc_opaque_rejection_id()?,
        target,
        rejected_by.clone(),
        reason,
        replacement.map(|r| r.id.clone()),
        chunk_sha256_override,
        at,
    )?;
    if let Some(repl) = replacement {
        store.supersede(fact_id, repl)?;
    } else {
        store.forget_fact_id(fact_id, rejected_by, at)?;
    }
    Ok(record)
}

#[cfg(test)]
mod tests {
    use kp_model::{
        CalendarTable, ChunkTurn, ConceptKind, EpochMs, LibraryMode, MemberRole, ModelConceptRef,
        ModelExtractedAtom, ModelScopeRef, ModelWriteOp,
    };
    use kp_store::{InitOptions, Store};
    use serde_json::json;

    use super::*;

    #[test]
    fn token_budgeted_turn_batches_skip_ack_windows_and_respect_max_batch_tokens(
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let t0 = EpochMs(1_795_089_600_000);
        let mut store = Store::init(
            dir.path(),
            InitOptions {
                library_id: "tb-test".parse()?,
                name: "Token Batch Test".into(),
                mode: LibraryMode::Community,
                owner: "alice".parse()?,
                created_at: t0,
            },
        )?;
        store.add_member("bob".parse()?, MemberRole::Member, t0)?;

        let alice: ActorId = "alice".parse()?;
        let bob: ActorId = "bob".parse()?;
        let shanghai = kp_model::TimeZone::get("Asia/Shanghai")
            .map_err(|_| kp_model::KpError::invalid("tz", "Asia/Shanghai"))?;
        let calendar = CalendarTable::from_reference_in_tz(t0, &shanghai)?;

        let mut turns = vec![
            ChunkTurn {
                speaker: Some(alice.clone()),
                is_assistant: false,
                text: "好的，收到！".into(),
            },
            ChunkTurn {
                speaker: None,
                is_assistant: true,
                text: "已记录会议安排。".into(),
            },
        ];
        for idx in 0..20 {
            turns.push(ChunkTurn {
                speaker: Some(if idx % 2 == 0 {
                    alice.clone()
                } else {
                    bob.clone()
                }),
                is_assistant: false,
                text: format!("周五下午第{idx}组读书会成员在三楼会议室核对报销单据。"),
            });
        }

        // Default `max_batch_tokens` packs all 22 turns into 1 request.
        let wide_spec = TokenBatchSpec {
            source_id: "src_wide".parse()?,
            default_speaker: alice.clone(),
            reference_calendar: calendar.clone(),
            active_concepts: Vec::new(),
            heading: "读书会群聊".into(),
            max_batch_tokens: DEFAULT_MAX_BATCH_TOKENS,
            min_rate_limit_remaining_pct: 5,
        };
        let single_batch = build_token_budgeted_turn_batches(&store, &wide_spec, &turns)?;
        assert_eq!(single_batch.len(), 1);

        // Tight `max_batch_tokens = 45` splits by token budget AND drops the leading ack/assistant-only window.
        let tight_spec = TokenBatchSpec {
            max_batch_tokens: 45,
            ..wide_spec
        };
        let split_batches = build_token_budgeted_turn_batches(&store, &tight_spec, &turns)?;
        assert!(split_batches.len() >= 10);
        assert!(split_batches
            .iter()
            .all(|req| !req.chunks[0].body.contains("好的，收到")));
        Ok(())
    }

    #[test]
    fn unlocatable_duplicate_quote_drops_only_bad_atom_while_security_violations_still_abort(
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let t0 = EpochMs(1_795_089_600_000);
        let mut store = Store::init(
            dir.path(),
            InitOptions {
                library_id: "resilient-batch".parse()?,
                name: "Resilient Batch Test".into(),
                mode: LibraryMode::Community,
                owner: "alice".parse()?,
                created_at: t0,
            },
        )?;
        for member in ["bob", "carol", "erin"] {
            store.add_member(member.parse()?, MemberRole::Member, t0)?;
        }

        let alice: ActorId = "alice".parse()?;
        let bob: ActorId = "bob".parse()?;
        let carol: ActorId = "carol".parse()?;
        let erin: ActorId = "erin".parse()?;
        let shanghai = kp_model::TimeZone::get("Asia/Shanghai")
            .map_err(|_| kp_model::KpError::invalid("tz", "Asia/Shanghai"))?;
        let calendar = CalendarTable::from_reference_in_tz(t0, &shanghai)?;

        let body = format_tagged_turns(&[
            ChunkTurn {
                speaker: Some(alice.clone()),
                is_assistant: false,
                text: "周五下午三点我们在三楼阅览室核对读书会书单。".into(),
            },
            ChunkTurn {
                speaker: Some(bob.clone()),
                is_assistant: false,
                text: "收到".into(),
            },
            ChunkTurn {
                speaker: Some(erin.clone()),
                is_assistant: false,
                text: "我自己平时做读书笔记只用纯文本格式。".into(),
            },
            ChunkTurn {
                speaker: Some(carol.clone()),
                is_assistant: false,
                text: "收到".into(),
            },
        ]);
        let req = build_extraction_batch_request(
            &store,
            "src_dup_batch".parse()?,
            alice.clone(),
            calendar,
            Vec::new(),
            vec![ExtractionChunkSlice {
                chunk_id: "c_multi".into(),
                heading: "20-turn window".into(),
                body,
                source_id: "src_dup_batch".parse()?,
                author: alice.clone(),
                base_anchor: Default::default(),
                role: Default::default(),
                reply_to_chunk_id: None,
            }],
            5,
        )?;
        let opts = NlFunnelOptions::personal_or_shared(AudienceSpec::AllMembers, t0);

        let valid_alice = ModelExtractedAtom {
            kind: FactKind::Event,
            claim_text: "周五下午三点在三楼阅览室核对读书会书单".into(),
            value: json!({"room": "3F"}),
            scope: None,
            ambiguous_scope: None,
            condition_concept: Some(ModelConceptRef::ProposeNew {
                kind: ConceptKind::Condition,
                label: "核对读书会书单".into(),
                domain: "club".into(),
                polarity: kp_model::ConceptPolarity::Neutral,
            }),
            action_concept: None,
            group_concept: None,
            member_mention: None,
            valid_window: None,
            valid_dates: Some(kp_model::ModelDateRange {
                start_date_iso: "2026-11-20".into(),
                end_date_iso: None,
                open_ended: false,
            }),
            speaker: Some(alice.clone()),
            assumed_premise_ids: Vec::new(),
            quote: kp_model::ModelQuoteAnchor {
                chunk_id: "c_multi".into(),
                verbatim_quote: "周五下午三点我们在三楼阅览室核对读书会书单。".into(),
                turn_ordinal: Some(0),
                approx_char_offset: None,
            },
            write_op: ModelWriteOp::Add,
            evidence: None,
            command_guard: None,
        };
        let event_missing_cond = ModelExtractedAtom {
            condition_concept: None,
            claim_text: "缺少条件的事件".into(),
            ..valid_alice.clone()
        };
        let duplicate_unhinted = ModelExtractedAtom {
            kind: FactKind::Observation,
            claim_text: "收到通知".into(),
            value: json!(null),
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
            quote: kp_model::ModelQuoteAnchor {
                chunk_id: "c_multi".into(),
                verbatim_quote: "收到".into(),
                turn_ordinal: None,
                approx_char_offset: None,
            },
            write_op: ModelWriteOp::Add,
            evidence: None,
            command_guard: None,
        };
        let bad_date_atom = ModelExtractedAtom {
            kind: FactKind::Event,
            claim_text: "无效日期条目".into(),
            value: json!(null),
            scope: None,
            ambiguous_scope: None,
            condition_concept: Some(ModelConceptRef::ProposeNew {
                kind: ConceptKind::Condition,
                label: "做读书笔记".into(),
                domain: "club".into(),
                polarity: kp_model::ConceptPolarity::Neutral,
            }),
            action_concept: None,
            group_concept: None,
            member_mention: None,
            valid_window: None,
            valid_dates: Some(kp_model::ModelDateRange {
                start_date_iso: "2026-02-30".into(),
                end_date_iso: None,
                open_ended: false,
            }),
            speaker: Some(erin.clone()),
            assumed_premise_ids: Vec::new(),
            quote: kp_model::ModelQuoteAnchor {
                chunk_id: "c_multi".into(),
                verbatim_quote: "我自己平时做读书笔记只用纯文本格式。".into(),
                turn_ordinal: Some(2),
                approx_char_offset: None,
            },
            write_op: ModelWriteOp::Add,
            evidence: None,
            command_guard: None,
        };
        let valid_erin = ModelExtractedAtom {
            // Even when model labels a scoped condition+action rule as `"observation"`,
            // structural narrowing coerces it to `FactKind::Constraint`!
            kind: FactKind::Observation,
            claim_text: "自己做读书笔记只用纯文本格式".into(),
            value: json!({"format": "plain_text"}),
            scope: Some(ModelScopeRef::NamedMentions {
                mentions: vec!["nonexistent_guest".into()],
            }),
            ambiguous_scope: None,
            condition_concept: Some(ModelConceptRef::ProposeNew {
                kind: ConceptKind::Condition,
                label: "做读书笔记".into(),
                domain: "club".into(),
                polarity: kp_model::ConceptPolarity::Neutral,
            }),
            action_concept: Some(ModelConceptRef::ProposeNew {
                kind: ConceptKind::Action,
                label: "只用纯文本格式".into(),
                domain: "club".into(),
                polarity: kp_model::ConceptPolarity::Positive,
            }),
            group_concept: None,
            member_mention: None,
            valid_window: None,
            valid_dates: None,
            speaker: Some(erin.clone()),
            assumed_premise_ids: Vec::new(),
            quote: kp_model::ModelQuoteAnchor {
                chunk_id: "c_multi".into(),
                verbatim_quote: "我自己平时做读书笔记只用纯文本格式。".into(),
                turn_ordinal: Some(2),
                approx_char_offset: None,
            },
            write_op: ModelWriteOp::Add,
            evidence: None,
            command_guard: None,
        };
        let bad_membership = ModelExtractedAtom {
            kind: FactKind::Membership,
            claim_text: "未知访客加入读书会".into(),
            value: json!({}),
            scope: None,
            ambiguous_scope: None,
            condition_concept: None,
            action_concept: None,
            group_concept: Some(ModelConceptRef::ProposeNew {
                kind: ConceptKind::Group,
                label: "读书会".into(),
                domain: "club".into(),
                polarity: kp_model::ConceptPolarity::Neutral,
            }),
            member_mention: Some("unknown_stranger".into()),
            valid_window: None,
            valid_dates: None,
            speaker: Some(alice.clone()),
            assumed_premise_ids: Vec::new(),
            quote: kp_model::ModelQuoteAnchor {
                chunk_id: "c_multi".into(),
                verbatim_quote: "周五下午三点我们在三楼阅览室核对读书会书单。".into(),
                turn_ordinal: Some(0),
                approx_char_offset: None,
            },
            write_op: ModelWriteOp::Add,
            evidence: None,
            command_guard: None,
        };

        // 1) Security boundary violations (`speaker`, `write_op`, `assumed_premise_ids`)
        //    mixed into a batch STILL abort the entire batch before mutating `store`!
        let mut spoofed_speaker = valid_alice.clone();
        spoofed_speaker.speaker = Some(bob);
        let mut forged_supersede = valid_alice.clone();
        forged_supersede.write_op = ModelWriteOp::SupersedeOwn {
            target_fact_id: "fact_foreign".parse()?,
        };
        let mut forged_premise = valid_alice.clone();
        forged_premise.assumed_premise_ids = vec!["fact_unlisted_decision".parse()?];
        for security_violation in [spoofed_speaker, forged_supersede, forged_premise] {
            assert!(apply_extracted_atoms(
                &mut store,
                &req,
                &[valid_erin.clone(), security_violation],
                &opts,
            )
            .is_err());
            assert!(store.derivation_view()?.facts.is_empty());
        }

        // 2) Duplicate unlocatable quote, malformed date, event missing condition_concept,
        //    unresolvable membership mention, and unresolvable NamedMentions (downgraded to SelfOnly)
        //    are all isolated per-atom while valid claims in the same batch are saved!
        let out = apply_extracted_atoms(
            &mut store,
            &req,
            &[
                event_missing_cond,
                valid_alice,
                duplicate_unhinted,
                bad_date_atom,
                bad_membership,
                valid_erin,
            ],
            &opts,
        )?;
        assert_eq!(out.status, NlBatchStatus::Completed);
        assert_eq!(out.created_fact_ids.len(), 2);
        assert_eq!(out.rejected_atoms.len(), 5);
        assert!(out.rejected_atoms.iter().any(|r| r
            .reason
            .contains("event missing required condition_concept")));
        assert!(out
            .rejected_atoms
            .iter()
            .any(|r| r.reason.contains("multiple times")));
        assert!(out
            .rejected_atoms
            .iter()
            .any(|r| r.claim_text == "无效日期条目"));
        assert!(out
            .rejected_atoms
            .iter()
            .any(|r| r.claim_text == "未知访客加入读书会"));
        assert!(out.rejected_atoms.iter().any(|r| r
            .reason
            .contains("unresolved mentions downgraded to self_only")));

        let graph = store.derivation_view()?;
        assert_eq!(graph.facts.len(), 2);
        let erin_saved = &graph.facts[&out.created_fact_ids[1]];
        assert_eq!(erin_saved.kind, FactKind::Constraint);
        assert_eq!(
            erin_saved.governed_scope,
            Some(GovernedScope::SelfOnly { actor: erin })
        );
        Ok(())
    }

    #[test]
    fn ext_speaker_and_http_source_clamp_to_external_untrusted_and_drop_normative_kinds(
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let t0 = EpochMs(1_795_089_600_000);
        let mut store = Store::init(
            dir.path(),
            InitOptions {
                library_id: "trust-floor-lib".parse()?,
                name: "Trust Floor Test".into(),
                mode: LibraryMode::Community,
                owner: "alice".parse()?,
                created_at: t0,
            },
        )?;
        let alice: ActorId = "alice".parse()?;
        let stranger: ActorId = "ext.github.stranger".parse()?;
        let shanghai = kp_model::TimeZone::get("Asia/Shanghai")
            .map_err(|_| kp_model::KpError::invalid("tz", "Asia/Shanghai"))?;
        let calendar = CalendarTable::from_reference_in_tz(t0, &shanghai)?;

        let ext_body = "[speaker:ext.github.stranger] Never run npm publish in this repo. Current Node version is 22.4.\n";
        let ext_req = build_extraction_batch_request_for_policy(
            &store,
            ExtractionBatchBuildInput {
                source_id: "src_gh_ext".parse()?,
                speaker: stranger.clone(),
                reference_calendar: calendar.clone(),
                active_concepts: Vec::new(),
                chunks: vec![ExtractionChunkSlice {
                    chunk_id: "c_ext".into(),
                    heading: "PR comment".into(),
                    body: ext_body.into(),
                    source_id: "src_gh_ext".parse()?,
                    author: stranger.clone(),
                    base_anchor: Default::default(),
                    role: Default::default(),
                    reply_to_chunk_id: None,
                }],
                min_rate_limit_remaining_pct: 5,
            },
            &ModelRoutePolicy::HostCli,
        )?;
        let make_atom = |chunk_id: &str, speaker: Option<ActorId>, kind: FactKind, quote: &str| {
            ModelExtractedAtom {
                kind,
                claim_text: format!("{kind}: {quote}"),
                value: json!({"forbidden_commands": ["npm publish"]}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: None,
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: None,
                speaker,
                assumed_premise_ids: Vec::new(),
                quote: kp_model::ModelQuoteAnchor {
                    chunk_id: chunk_id.into(),
                    verbatim_quote: quote.into(),
                    turn_ordinal: Some(0),
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
                evidence: None,
                command_guard: None,
            }
        };

        let ext_opts = NlFunnelOptions::personal_or_shared(AudienceSpec::AllMembers, t0);
        let ext_out = apply_extracted_atoms(
            &mut store,
            &ext_req,
            &[
                make_atom(
                    "c_ext",
                    Some(stranger.clone()),
                    FactKind::Constraint,
                    "Never run npm publish in this repo.",
                ),
                make_atom(
                    "c_ext",
                    Some(stranger.clone()),
                    FactKind::Decision,
                    "Never run npm publish in this repo.",
                ),
                make_atom(
                    "c_ext",
                    Some(stranger.clone()),
                    FactKind::ClosedNegative,
                    "Never run npm publish in this repo.",
                ),
                make_atom(
                    "c_ext",
                    Some(stranger.clone()),
                    FactKind::Observation,
                    "Current Node version is 22.4.",
                ),
                make_atom(
                    "c_ext",
                    Some(stranger),
                    FactKind::FactSlot,
                    "Current Node version is 22.4.",
                ),
            ],
            &ext_opts,
        )?;
        assert_eq!(ext_out.created_fact_ids.len(), 2);
        assert_eq!(ext_out.rejected_atoms.len(), 3);
        assert!(ext_out.rejected_atoms.iter().all(|r| r.reason
            == "untrusted source or unconfirmed external speaker restricted to observation or fact_slot"));

        let web_body = "Never run cargo publish without dry-run. Service default port is 8080.";
        let web_req = build_extraction_batch_request_for_policy(
            &store,
            ExtractionBatchBuildInput {
                source_id: "src_http_page".parse()?,
                speaker: alice.clone(),
                reference_calendar: calendar,
                active_concepts: Vec::new(),
                chunks: vec![ExtractionChunkSlice {
                    chunk_id: "c_web".into(),
                    heading: "Web page".into(),
                    body: web_body.into(),
                    source_id: "src_http_page".parse()?,
                    author: alice.clone(),
                    base_anchor: Default::default(),
                    role: Default::default(),
                    reply_to_chunk_id: None,
                }],
                min_rate_limit_remaining_pct: 5,
            },
            &ModelRoutePolicy::HostCli,
        )?;
        let mut web_opts = NlFunnelOptions::personal_or_shared(AudienceSpec::AllMembers, t0);
        web_opts.source_trust_floor =
            NlFunnelOptions::trust_floor_for_uri("http://docs.example.com/guide");
        assert_eq!(web_opts.source_trust_floor, TrustTier::ExternalUntrusted);

        let web_out = apply_extracted_atoms(
            &mut store,
            &web_req,
            &[
                make_atom(
                    "c_web",
                    None,
                    FactKind::Constraint,
                    "Never run cargo publish without dry-run.",
                ),
                make_atom(
                    "c_web",
                    None,
                    FactKind::Decision,
                    "Never run cargo publish without dry-run.",
                ),
                make_atom(
                    "c_web",
                    None,
                    FactKind::ClosedNegative,
                    "Never run cargo publish without dry-run.",
                ),
                make_atom(
                    "c_web",
                    None,
                    FactKind::Observation,
                    "Service default port is 8080.",
                ),
                make_atom(
                    "c_web",
                    None,
                    FactKind::FactSlot,
                    "Service default port is 8080.",
                ),
            ],
            &web_opts,
        )?;
        assert_eq!(web_out.created_fact_ids.len(), 2);
        assert_eq!(web_out.rejected_atoms.len(), 3);

        let graph = store.derivation_view()?;
        for fid in ext_out
            .created_fact_ids
            .iter()
            .chain(web_out.created_fact_ids.iter())
        {
            let fact = &graph.facts[fid];
            assert!(matches!(
                fact.kind,
                FactKind::Observation | FactKind::FactSlot
            ));
            assert_eq!(fact.trust_tier, TrustTier::ExternalUntrusted);
        }
        Ok(())
    }
}
