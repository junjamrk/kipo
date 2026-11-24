use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::schema::text_enum;
use crate::{
    canonical_json, ActorId, AudienceSpec, ConceptId, ContentHash, EpochMs, Fact, FactAccess,
    FactId, FactKind, KpError, RejectionId, Result, ReviewId, SourceScopeId, ValidityWindow,
};

/// Half-open Unicode scalar offset interval `[char_start, char_end)` inside a source chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct CharSpan {
    pub char_start: u32,
    pub char_end: u32,
}

impl CharSpan {
    pub fn new(char_start: u32, char_end: u32) -> Result<Self> {
        if char_end <= char_start {
            return Err(KpError::invalid(
                "char_span",
                "char_end must be greater than char_start",
            ));
        }
        Ok(Self {
            char_start,
            char_end,
        })
    }

    pub fn overlaps(&self, other: &Self) -> bool {
        self.char_start < other.char_end && other.char_start < self.char_end
    }
}

/// Explicit target scope for a constraint or norm (`GovernedScope`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GovernedScope {
    /// Applies strictly to the speaker alone (e.g. "I personally never book red-eye flights").
    SelfOnly { actor: ActorId },
    /// Applies to an explicit set of named actors.
    Actors { actors: BTreeSet<ActorId> },
    /// Applies to `anchor` plus members linked via `FactKind::Membership` sharing `group_concept_id`.
    RelationGroup {
        anchor: ActorId,
        group_concept_id: ConceptId,
    },
    /// Applies to a code symbol, file path, repository, or service resource.
    Resource { target: String },
}

/// Structured payload stored in `Fact.object` when `Fact.kind == FactKind::Membership`.
/// Both `GovernedScope::RelationGroup` and `MembershipRelation` reference the same `ConceptId`
/// (`ConceptKind::Group`), ensuring cross-library alignment instead of free-text string drift.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MembershipRelation {
    pub anchor: ActorId,
    pub member: ActorId,
    pub group_concept_id: ConceptId,
}

/// Stored when an utterance's scope is ambiguous: enters active state as `narrow_active`
/// (`SelfOnly`), surfacing in `kp review` only if `broader_candidate` triggers a cross-person impact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BroaderScopeCandidate {
    pub narrow_active: GovernedScope,
    pub broader_candidate: GovernedScope,
    pub question: String,
}

/// Per-event attribution separating original speaker from importer while referencing a live `SourceScope`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceOriginMeta {
    pub speaker: ActorId,
    pub ingested_by: ActorId,
    pub source_scope_id: SourceScopeId,
}

/// Live container membership roster (`source_scopes` table) for external collaboration containers and repositories.
/// Read-time visibility computes `current_audience ∩ library_members` dynamically so group departures
/// immediately narrow visibility of base facts and semiring-derived conclusions without rewriting facts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceScope {
    pub scope_id: SourceScopeId,
    pub provider: String,
    pub external_container_id: String,
    pub label: String,
    pub current_audience: AudienceSpec,
    pub synced_at: EpochMs,
}

impl SourceScope {
    pub fn effective_members(&self, library_members: &BTreeSet<ActorId>) -> BTreeSet<ActorId> {
        match &self.current_audience {
            AudienceSpec::Explicit(actors) => {
                actors.intersection(library_members).cloned().collect()
            }
            AudienceSpec::AllMembers => library_members.clone(),
            AudienceSpec::SemiringDerived => BTreeSet::new(),
        }
    }

    pub fn allows(&self, viewer: &ActorId, library_members: &BTreeSet<ActorId>) -> bool {
        self.current_audience.allows(viewer, library_members)
    }

    /// Enforces `requested ⊆ (current_audience ∩ library_members)` against live source membership.
    pub fn validate_publish_audience(
        &self,
        requested: &AudienceSpec,
        library_members: &BTreeSet<ActorId>,
    ) -> Result<()> {
        if matches!(
            (&self.current_audience, requested),
            (_, AudienceSpec::SemiringDerived) | (AudienceSpec::SemiringDerived, _)
        ) {
            return Err(KpError::invalid(
                "audience",
                "publish ceiling check requires explicit or all-member audiences",
            ));
        }
        match (&self.current_audience, requested) {
            (AudienceSpec::AllMembers, AudienceSpec::AllMembers) => Ok(()),
            (AudienceSpec::AllMembers, AudienceSpec::Explicit(target)) => {
                if target.iter().all(|actor| library_members.contains(actor)) {
                    Ok(())
                } else {
                    Err(KpError::invalid(
                        "audience",
                        "requested audience includes non-members of the library",
                    ))
                }
            }
            (AudienceSpec::Explicit(_), AudienceSpec::AllMembers) => Err(KpError::invalid(
                "audience",
                "requested all-members audience exceeds explicit live source scope",
            )),
            (AudienceSpec::Explicit(_), AudienceSpec::Explicit(target)) => {
                let allowed = self.effective_members(library_members);
                if target.is_subset(&allowed) {
                    Ok(())
                } else {
                    Err(KpError::invalid(
                        "audience",
                        "requested audience exceeds current source scope membership",
                    ))
                }
            }
            (AudienceSpec::SemiringDerived, _) | (_, AudienceSpec::SemiringDerived) => {
                Err(KpError::invalid("audience", "invalid semiring marker"))
            }
        }
    }
}

text_enum!(ReviewKind {
    AmbiguousScope => "ambiguous_scope",
    ConceptAlignment => "concept_alignment",
    IdentityMapping => "identity_mapping",
    WritebackProposal => "writeback_proposal"
});

text_enum!(ReviewStatus {
    Open => "open",
    Confirmed => "confirmed",
    Dismissed => "dismissed"
});

/// Non-intrusive review queue item (`kp review`), never injected unsolicited at `SessionStart`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewItem {
    pub id: ReviewId,
    pub actor: ActorId,
    pub kind: ReviewKind,
    pub target_id: String,
    pub question: String,
    pub options: Vec<String>,
    pub payload: Value,
    pub status: ReviewStatus,
    pub created_at: EpochMs,
    pub resolved_at: Option<EpochMs>,
}

/// Evidence-bound rejection key anchored on `(speaker, chunk_sha256, quote_span)` rather than LLM wording.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectionEvidenceKey {
    pub speaker: ActorId,
    pub chunk_sha256: ContentHash,
    pub quote_span: CharSpan,
}

impl RejectionEvidenceKey {
    /// Returns true if a re-extracted candidate on the same speaker & chunk overlaps the rejected quote span.
    pub fn overlaps_candidate(&self, other: &Self) -> bool {
        self.speaker == other.speaker
            && self.chunk_sha256 == other.chunk_sha256
            && self.quote_span.overlaps(&other.quote_span)
    }
}

/// Canonical semantic rejection key over normalized domain identifiers rather than free-form model prose.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectionSemanticKey {
    pub kind: FactKind,
    pub concept_ids: BTreeSet<ConceptId>,
    pub governed_scope: Option<GovernedScope>,
    pub valid_window: ValidityWindow,
}

impl RejectionSemanticKey {
    pub fn fingerprint(&self) -> ContentHash {
        let normalized = serde_json::json!([
            self.kind.as_str(),
            &self.concept_ids,
            &self.governed_scope,
            self.valid_window.valid_from.0,
            self.valid_window.valid_until.0,
        ]);
        ContentHash::digest(canonical_json(&normalized))
    }
}

/// Durable dual-key "OUT" record for `"这条错了"` so re-extraction never resurrects rejected claims
/// even if the model rewords `subject`/`predicate`/`object`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectionRecord {
    pub id: RejectionId,
    pub rejected_by: ActorId,
    pub evidence_key: Option<RejectionEvidenceKey>,
    pub semantic_key: Option<RejectionSemanticKey>,
    pub replacement_fact_id: Option<FactId>,
    pub reason: String,
    pub created_at: EpochMs,
}

impl RejectionRecord {
    pub fn validate(&self) -> Result<()> {
        if self.evidence_key.is_none() && self.semantic_key.is_none() {
            return Err(KpError::invalid(
                "rejection_record",
                "requires at least an evidence key or a semantic key",
            ));
        }
        if let Some(ev) = &self.evidence_key {
            CharSpan::new(ev.quote_span.char_start, ev.quote_span.char_end)?;
        }
        if let Some(sem) = &self.semantic_key {
            if sem.valid_window.valid_from >= sem.valid_window.valid_until {
                return Err(KpError::invalid(
                    "rejection_record",
                    "semantic key validity window must be nonempty",
                ));
            }
        }
        Ok(())
    }

    /// Matches if EITHER the evidence span overlaps on the same `(speaker, chunk_sha256)`
    /// OR the canonical semantic key fingerprint matches.
    pub fn matches_candidate(
        &self,
        candidate_evidence: Option<&RejectionEvidenceKey>,
        candidate_semantic: Option<&RejectionSemanticKey>,
    ) -> bool {
        let evidence_hit = match (&self.evidence_key, candidate_evidence) {
            (Some(rejected), Some(candidate)) => rejected.overlaps_candidate(candidate),
            _ => false,
        };
        if evidence_hit {
            return true;
        }
        match (&self.semantic_key, candidate_semantic) {
            (Some(rejected), Some(candidate)) => rejected.fingerprint() == candidate.fingerprint(),
            _ => false,
        }
    }

    /// Builds a dual-key `RejectionRecord` directly from a persisted `Fact` (extracting quote span
    /// & chunk SHA-256 from `source_anchor` when available, plus normalized `RejectionSemanticKey`).
    pub fn from_fact(
        id: RejectionId,
        fact: &Fact,
        rejected_by: ActorId,
        reason: impl Into<String>,
        replacement_fact_id: Option<FactId>,
        chunk_sha256_override: Option<ContentHash>,
        created_at: EpochMs,
    ) -> Result<Self> {
        let evidence_key = match &fact.source_anchor {
            Some(anchor) => {
                let sha = chunk_sha256_override.or_else(|| anchor.body_hash.clone());
                match (sha, anchor.char_start, anchor.char_end) {
                    (Some(chunk_sha256), Some(start), Some(end)) if end > start => {
                        Some(RejectionEvidenceKey {
                            speaker: fact.author.clone(),
                            chunk_sha256,
                            quote_span: CharSpan::new(start, end)?,
                        })
                    }
                    _ => None,
                }
            }
            None => None,
        };
        let valid_window = match &fact.access {
            FactAccess::Base { stamp, .. } => stamp.valid,
            FactAccess::Derived { recorded_at, .. } => ValidityWindow {
                valid_from: *recorded_at,
                valid_until: EpochMs::OPEN_END,
            },
        };
        let semantic_key = Some(RejectionSemanticKey {
            kind: fact.kind,
            concept_ids: fact_concept_ids(fact),
            governed_scope: fact.governed_scope.clone(),
            valid_window,
        });
        let record = Self {
            id,
            rejected_by,
            evidence_key,
            semantic_key,
            replacement_fact_id,
            reason: reason.into(),
            created_at,
        };
        record.validate()?;
        Ok(record)
    }
}

/// Extracts `condition_concept_id` stored on a structured natural-language `Fact`.
pub fn fact_condition_concept_id(fact: &Fact) -> Option<ConceptId> {
    fact.object
        .get("condition_concept_id")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
}

/// Extracts `action_concept_id` stored on a structured natural-language `Fact`.
pub fn fact_action_concept_id(fact: &Fact) -> Option<ConceptId> {
    fact.object
        .get("action_concept_id")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
}

/// Extracts all semantic `ConceptId`s attached to `fact` (`condition`, `action`, `membership`, `concept_ids`).
pub fn fact_concept_ids(fact: &Fact) -> BTreeSet<ConceptId> {
    let mut ids = BTreeSet::new();
    if let Some(c) = fact_condition_concept_id(fact) {
        ids.insert(c);
    }
    if let Some(a) = fact_action_concept_id(fact) {
        ids.insert(a);
    }
    if let Some(g) = fact
        .object
        .get("group_concept_id")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
    {
        ids.insert(g);
    }
    if let Some(arr) = fact.object.get("concept_ids").and_then(Value::as_array) {
        for v in arr {
            if let Some(cid) = v.as_str().and_then(|s| s.parse().ok()) {
                ids.insert(cid);
            }
        }
    }
    ids
}

/// Extracts human-readable claim text from `fact.object` (falling back to stringified object).
pub fn fact_claim_text(fact: &Fact) -> String {
    if let Some(text) = fact.object.get("claim_text").and_then(Value::as_str) {
        return text.to_owned();
    }
    if let Some(text) = fact.object.as_str() {
        return text.to_owned();
    }
    fact.object.to_string()
}

text_enum!(ConceptKind {
    Condition => "condition",
    Action => "action",
    Group => "group",
    Topic => "topic"
});

text_enum!(ConceptPolarity {
    Positive => "positive",
    Negative => "negative",
    Neutral => "neutral",
    Resolved => "resolved"
});

/// Open community lexicon entry (no domain test words hardcoded in `kp.core`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityConcept {
    pub id: ConceptId,
    pub kind: ConceptKind,
    pub canonical_label: String,
    pub domain: String,
    pub polarity: ConceptPolarity,
    pub proposed_by: ActorId,
    pub created_at: EpochMs,
}

/// Viewer-scoped baseline header embedded in exported Markdown knowledge pages.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionPageMeta {
    pub page_slug: String,
    pub viewer: ActorId,
    pub base_seq: i64,
    pub page_sha256: ContentHash,
    pub rendered_at: EpochMs,
}

/// Parsed diff action from `kp apply` over a viewer-projected Markdown page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ProjectionWritebackOp {
    Supersede {
        fact_id: FactId,
        new_value: Value,
        reason: String,
    },
    Withdraw {
        fact_id: FactId,
    },
    Create {
        subject: String,
        predicate: String,
        value: Value,
    },
    /// Edits against an outdated `base_seq` or unmapped lines become proposals, never dropped.
    Proposal {
        line_number: u32,
        raw_line: String,
        reason: String,
    },
}

#[cfg(test)]
#[path = "governance_tests.rs"]
mod tests;
