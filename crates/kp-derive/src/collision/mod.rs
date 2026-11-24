//! Open community concept alignment (`Entails / Contradicts / Unrelated / Ambiguous`) and
//! person-targeted collision derivation + non-intrusive cross-person `AmbiguousScope` review.

mod align;
mod reviews;

pub use align::{
    align_concepts_and_derive, reconcile_contradicted_events, record_manual_concept_alignment,
};
pub(crate) use reviews::compute_governed_collisions;
pub use reviews::{evaluate_ambiguous_scope_reviews, resolve_ambiguous_scope_review};

use std::collections::{BTreeMap, BTreeSet};

use kp_model::{
    fact_claim_text, fact_condition_concept_id, ActorId, AudienceSpec, ConceptId, ContentHash,
    DerivationView, EntailmentVerdict, Fact, FactAccess, FactId, FactStatus, LlmClient,
    ModelRoutePolicy, ReviewItem, StrongRef,
};
use kp_store::{Result, Store};
use serde::{Deserialize, Serialize};

use crate::rules::common_windows;

pub const CONCEPT_ALIGNMENT_PREDICATE: &str = "kp.core.concept_alignment";

/// Internal system predicate marking a contradicted event's supersession state (`audience = []`).
pub const EVENT_SUPERSEDED_PREDICATE: &str = "kp.core.event_superseded";

/// Explicit binding tag required on all `v2` label-only (`evidence == None`) alignment facts so
/// pre-upgrade `v1` unversioned cache rows (which may have been derived from private quotes) are ignored.
pub const CONCEPT_LABEL_BINDING_V2: &str = "concept_label_v2";

/// Key for evidence-bound quote-dependent alignment verdicts: `(left_fact_id, right_fact_id, quotes_sha256)`.
pub type FactAlignmentKey = (FactId, FactId, String);

/// Deterministic alignment relation table loaded from `DerivationView` or populated via `LlmClient`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConceptAlignmentTable {
    pub entails: BTreeSet<(ConceptId, ConceptId)>,
    pub contradicts: BTreeSet<(ConceptId, ConceptId)>,
    pub unrelated: BTreeSet<(ConceptId, ConceptId)>,
    pub unrelated_quote_hashes: BTreeMap<(ConceptId, ConceptId), BTreeSet<String>>,
    pub ambiguous: BTreeMap<(ConceptId, ConceptId), String>,
    pub fact_entails: BTreeSet<FactAlignmentKey>,
    pub fact_contradicts: BTreeSet<FactAlignmentKey>,
    pub fact_unrelated: BTreeSet<FactAlignmentKey>,
    pub fact_ambiguous: BTreeMap<FactAlignmentKey, String>,
}

impl ConceptAlignmentTable {
    pub fn from_graph(graph: &DerivationView) -> Self {
        let mut table = Self::default();
        for fact in graph.facts.values() {
            if fact.predicate != CONCEPT_ALIGNMENT_PREDICATE
                || fact.status != FactStatus::Active
                || fact.access.expired_at().is_some()
            {
                continue;
            }
            let Some(left) = fact
                .object
                .get("left_concept_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<ConceptId>().ok())
            else {
                continue;
            };
            let Some(right) = fact
                .object
                .get("right_concept_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<ConceptId>().ok())
            else {
                continue;
            };

            let has_evidence_binding = fact.object.get("evidence_refs").is_some()
                || fact.object.get("left_fact_id").is_some()
                || fact.object.get("right_fact_id").is_some();

            if has_evidence_binding {
                let Some(lf_id) = fact
                    .object
                    .get("left_fact_id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<FactId>().ok())
                else {
                    continue;
                };
                let Some(rf_id) = fact
                    .object
                    .get("right_fact_id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<FactId>().ok())
                else {
                    continue;
                };
                let Some(recorded_qh) = fact
                    .object
                    .get("quotes_sha256")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                else {
                    continue;
                };
                let Some(expected_refs) = fact
                    .object
                    .get("evidence_refs")
                    .cloned()
                    .and_then(|v| serde_json::from_value::<[StrongRef; 2]>(v).ok())
                else {
                    continue;
                };
                let (Some(lf), Some(rf)) = (graph.facts.get(&lf_id), graph.facts.get(&rf_id))
                else {
                    continue;
                };
                if lf.status != FactStatus::Active
                    || rf.status != FactStatus::Active
                    || lf.access.expired_at().is_some()
                    || rf.access.expired_at().is_some()
                    || lf.strong_ref() != expected_refs[0]
                    || rf.strong_ref() != expected_refs[1]
                {
                    continue;
                }
                let current_qh = compute_quotes_sha256(
                    fact_quote_or_claim(lf).as_deref(),
                    fact_quote_or_claim(rf).as_deref(),
                );
                if current_qh != recorded_qh {
                    continue;
                }
                if effective_fact_viewers(graph, lf).is_disjoint(&effective_fact_viewers(graph, rf))
                {
                    continue;
                }

                let fkey = (lf_id, rf_id, current_qh);
                match fact.object.get("verdict").and_then(|v| v.as_str()) {
                    Some("entails") => {
                        table.fact_entails.insert(fkey);
                    }
                    Some("contradicts") => {
                        table.fact_contradicts.insert(fkey);
                    }
                    Some("unrelated") => {
                        table.fact_unrelated.insert(fkey);
                    }
                    Some("ambiguous") => {
                        let q = fact
                            .object
                            .get("question")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_owned();
                        table.fact_ambiguous.insert(fkey, q);
                    }
                    _ => {}
                }
                continue;
            }

            if fact.object.get("binding").and_then(|v| v.as_str()) != Some(CONCEPT_LABEL_BINDING_V2)
            {
                continue;
            }

            match fact.object.get("verdict").and_then(|v| v.as_str()) {
                Some("entails") => {
                    table.entails.insert((left, right));
                }
                Some("contradicts") => {
                    table.contradicts.insert((left, right));
                }
                Some("unrelated") => {
                    let pair = (left, right);
                    table.unrelated.insert(pair.clone());
                    let entry = table.unrelated_quote_hashes.entry(pair).or_default();
                    if let Some(qh) = fact
                        .object
                        .get("quotes_sha256")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        entry.insert(qh.to_owned());
                    }
                    if let Some(arr) = fact
                        .object
                        .get("seen_quotes_sha256")
                        .and_then(|v| v.as_array())
                    {
                        for item in arr.iter().filter_map(|v| v.as_str()) {
                            if !item.is_empty() {
                                entry.insert(item.to_owned());
                            }
                        }
                    }
                }
                Some("ambiguous") => {
                    let q = fact
                        .object
                        .get("question")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned();
                    table.ambiguous.insert((left, right), q);
                }
                _ => {}
            }
        }
        table
    }

    pub fn has_verdict(&self, left: &ConceptId, right: &ConceptId) -> bool {
        let pair = (left.clone(), right.clone());
        self.entails.contains(&pair)
            || self.contradicts.contains(&pair)
            || self.unrelated.contains(&pair)
            || self.ambiguous.contains_key(&pair)
    }

    pub fn entails(&self, left: &ConceptId, right: &ConceptId) -> bool {
        left == right || self.entails.contains(&(left.clone(), right.clone()))
    }

    pub fn contradicts(&self, left: &ConceptId, right: &ConceptId) -> bool {
        self.contradicts.contains(&(left.clone(), right.clone()))
    }

    pub fn entails_facts(
        &self,
        left_fact: &FactId,
        right_fact: &FactId,
        quotes_sha256: &str,
    ) -> bool {
        self.fact_entails.contains(&(
            left_fact.clone(),
            right_fact.clone(),
            quotes_sha256.to_owned(),
        ))
    }

    pub fn contradicts_facts(
        &self,
        left_fact: &FactId,
        right_fact: &FactId,
        quotes_sha256: &str,
    ) -> bool {
        self.fact_contradicts.contains(&(
            left_fact.clone(),
            right_fact.clone(),
            quotes_sha256.to_owned(),
        ))
    }

    pub fn unrelated_facts(
        &self,
        left_fact: &FactId,
        right_fact: &FactId,
        quotes_sha256: &str,
    ) -> bool {
        self.fact_unrelated.contains(&(
            left_fact.clone(),
            right_fact.clone(),
            quotes_sha256.to_owned(),
        ))
    }

    pub fn ambiguous_facts(
        &self,
        left_fact: &FactId,
        right_fact: &FactId,
        quotes_sha256: &str,
    ) -> Option<&str> {
        self.fact_ambiguous
            .get(&(
                left_fact.clone(),
                right_fact.clone(),
                quotes_sha256.to_owned(),
            ))
            .map(String::as_str)
    }

    fn fact_verdict(
        &self,
        left_fact: &FactId,
        right_fact: &FactId,
        quotes_sha256: &str,
    ) -> Option<EntailmentVerdict> {
        if self.entails_facts(left_fact, right_fact, quotes_sha256) {
            Some(EntailmentVerdict::Entails)
        } else if self.contradicts_facts(left_fact, right_fact, quotes_sha256) {
            Some(EntailmentVerdict::Contradicts)
        } else if let Some(q) = self.ambiguous_facts(left_fact, right_fact, quotes_sha256) {
            Some(EntailmentVerdict::Ambiguous {
                question: q.to_owned(),
            })
        } else if self.unrelated_facts(left_fact, right_fact, quotes_sha256) {
            Some(EntailmentVerdict::Unrelated)
        } else {
            None
        }
    }

    pub(super) fn insert_fact_verdict(
        &mut self,
        left_fact: &FactId,
        right_fact: &FactId,
        quotes_sha256: &str,
        verdict: &EntailmentVerdict,
    ) {
        let fkey = (
            left_fact.clone(),
            right_fact.clone(),
            quotes_sha256.to_owned(),
        );
        self.fact_entails.remove(&fkey);
        self.fact_contradicts.remove(&fkey);
        self.fact_unrelated.remove(&fkey);
        self.fact_ambiguous.remove(&fkey);
        match verdict {
            EntailmentVerdict::Entails => {
                self.fact_entails.insert(fkey);
            }
            EntailmentVerdict::Contradicts => {
                self.fact_contradicts.insert(fkey);
            }
            EntailmentVerdict::Unrelated => {
                self.fact_unrelated.insert(fkey);
            }
            EntailmentVerdict::Ambiguous { question } => {
                self.fact_ambiguous.insert(fkey, question.clone());
            }
        }
    }
}

/// Combined outcome of open concept alignment, contradicted-event supersession,
/// person-targeted collision derivation, and cross-person `AmbiguousScope` review checks.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CollisionDeriveOutcome {
    pub derived_warnings: usize,
    pub justifications: usize,
    pub superseded_contradicted_events: Vec<FactId>,
    pub emitted_reviews: Vec<ReviewItem>,
}

/// Resolution choices when an author responds to an `AmbiguousScope` `ReviewItem` in `kp review`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeReviewDecision {
    /// Promotes the constraint's `governed_scope` to `broader_candidate` and re-derives immediately.
    ConfirmBroader,
    /// Keeps `SelfOnly` permanently and clears `broader_scope` so `kp` never prompts again.
    KeepNarrow,
    /// Dismisses the current review item without altering the constraint.
    Dismiss,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScopeResolutionOutcome {
    pub replacement_fact_id: Option<FactId>,
    pub resolved_review: Option<ReviewItem>,
    pub derive_derived: usize,
    pub derive_justifications: usize,
}

pub(crate) fn is_system_internal_fact(fact: &Fact) -> bool {
    matches!(
        fact.predicate.as_str(),
        CONCEPT_ALIGNMENT_PREDICATE | EVENT_SUPERSEDED_PREDICATE
    )
}

pub(crate) fn is_governed_or_concept_fact(fact: &Fact) -> bool {
    fact.governed_scope.is_some()
        || fact.broader_scope.is_some()
        || fact_condition_concept_id(fact).is_some()
        || kp_model::fact_action_concept_id(fact).is_some()
}

pub(super) fn effective_fact_viewers(graph: &DerivationView, fact: &Fact) -> BTreeSet<ActorId> {
    let FactAccess::Base { audience, .. } = &fact.access else {
        return BTreeSet::new();
    };
    let mut viewers: BTreeSet<ActorId> = match audience {
        AudienceSpec::Explicit(actors) => actors.clone(),
        AudienceSpec::AllMembers => graph.members.clone(),
        AudienceSpec::SemiringDerived => BTreeSet::new(),
    };
    if let Some(scope_id) = &fact.source_scope_id {
        if let Some(scope_aud) = graph.source_scopes.get(scope_id) {
            viewers.retain(|v| scope_aud.allows(v, &graph.members));
        } else {
            viewers.clear();
        }
    }
    viewers
}

/// A newer event may contradict/supersede an older one only when (1) the newer event's author can
/// see the older event (no cross-viewer mutation of private facts) and (2) everyone who can see the
/// older event can also see the newer one (no viewer loses a warning without seeing why).
pub(super) fn event_audience_covers(graph: &DerivationView, newer: &Fact, older: &Fact) -> bool {
    let older_viewers = effective_fact_viewers(graph, older);
    if !older_viewers.contains(&newer.author) {
        return false;
    }
    let newer_viewers = effective_fact_viewers(graph, newer);
    older_viewers.is_subset(&newer_viewers)
}

pub(super) fn fact_quote_or_claim(fact: &Fact) -> Option<String> {
    fact.source_anchor
        .as_ref()
        .and_then(|a| a.verbatim_quote.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            let claim = fact_claim_text(fact);
            let trimmed = claim.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        })
}

pub(super) fn compute_quotes_sha256(left_quote: Option<&str>, right_quote: Option<&str>) -> String {
    let l = left_quote.map(str::trim).unwrap_or("");
    let r = right_quote.map(str::trim).unwrap_or("");
    ContentHash::digest(format!("{l}\0{r}")).to_string()
}

/// Returns the effective [`ModelRoutePolicy`] recorded on `fact` (failing closed to `RawOnly`
/// if `fact.tags` marks `raw_only` or if `"route_policy"` is malformed, and defaulting to
/// `ModelRoutePolicy::HostCli` when untagged).
pub fn fact_route_policy(fact: &Fact) -> ModelRoutePolicy {
    if fact
        .tags
        .iter()
        .any(|t| t == "raw_only" || t == "kp.raw_only")
        || fact.object.get("raw_only").and_then(|v| v.as_bool()) == Some(true)
    {
        return ModelRoutePolicy::RawOnly;
    }
    match fact.object.get("route_policy") {
        None | Some(serde_json::Value::Null) => ModelRoutePolicy::HostCli,
        Some(val) => serde_json::from_value::<ModelRoutePolicy>(val.clone())
            .unwrap_or(ModelRoutePolicy::RawOnly),
    }
}

#[derive(Clone, Copy)]
pub(super) enum AlignmentEvidence<'a> {
    LabelOnly {
        route_compatible: bool,
    },
    ContradictedEvents {
        newer: &'a Fact,
        older: &'a Fact,
    },
    EventConstraint {
        event: &'a Fact,
        constraint: &'a Fact,
    },
}

impl<'a> AlignmentEvidence<'a> {
    pub(super) fn facts(self) -> Option<(&'a Fact, &'a Fact)> {
        match self {
            Self::LabelOnly { .. } => None,
            Self::ContradictedEvents { newer, older } => Some((newer, older)),
            Self::EventConstraint { event, constraint } => Some((event, constraint)),
        }
    }
}

pub(super) fn evidence_allows_llm_egress(
    store: &Store,
    llm: &dyn LlmClient,
    evidence: AlignmentEvidence<'_>,
) -> Result<bool> {
    if llm.is_raw_only() {
        return Ok(false);
    }
    let client_policy = llm.route_policy();
    if client_policy == ModelRoutePolicy::RawOnly {
        return Ok(false);
    }
    match evidence {
        AlignmentEvidence::LabelOnly { route_compatible } => Ok(route_compatible),
        AlignmentEvidence::ContradictedEvents { newer, older } => {
            let new_pol = store.fact_effective_route_policy(newer)?;
            let old_pol = store.fact_effective_route_policy(older)?;
            Ok(new_pol != ModelRoutePolicy::RawOnly
                && old_pol != ModelRoutePolicy::RawOnly
                && new_pol == client_policy
                && old_pol == client_policy)
        }
        AlignmentEvidence::EventConstraint { event, constraint } => {
            let ev_pol = store.fact_effective_route_policy(event)?;
            let con_pol = store.fact_effective_route_policy(constraint)?;
            Ok(ev_pol != ModelRoutePolicy::RawOnly
                && con_pol != ModelRoutePolicy::RawOnly
                && ev_pol == client_policy
                && con_pol == client_policy)
        }
    }
}

pub(super) fn validate_live_evidence(
    store: &Store,
    evidence: AlignmentEvidence<'_>,
) -> Result<bool> {
    match evidence {
        AlignmentEvidence::LabelOnly { .. } => Ok(true),
        AlignmentEvidence::ContradictedEvents { newer, older } => {
            let fresh = store.derivation_view()?;
            let (Some(f_new), Some(f_old)) =
                (fresh.facts.get(&newer.id), fresh.facts.get(&older.id))
            else {
                return Ok(false);
            };
            if f_new.status != FactStatus::Active
                || f_old.status != FactStatus::Active
                || f_new.access.expired_at().is_some()
                || f_old.access.expired_at().is_some()
                || f_new.strong_ref() != newer.strong_ref()
                || f_old.strong_ref() != older.strong_ref()
            {
                return Ok(false);
            }
            let (FactAccess::Base { stamp: old_s, .. }, FactAccess::Base { stamp: new_s, .. }) =
                (&f_old.access, &f_new.access)
            else {
                return Ok(false);
            };
            if old_s.valid.intersect(&new_s.valid).is_none() {
                return Ok(false);
            }
            Ok(event_audience_covers(&fresh, f_new, f_old)
                && !effective_fact_viewers(&fresh, f_new)
                    .is_disjoint(&effective_fact_viewers(&fresh, f_old)))
        }
        AlignmentEvidence::EventConstraint { event, constraint } => {
            let fresh = store.derivation_view()?;
            let (Some(f_ev), Some(f_con)) =
                (fresh.facts.get(&event.id), fresh.facts.get(&constraint.id))
            else {
                return Ok(false);
            };
            if f_ev.status != FactStatus::Active
                || f_con.status != FactStatus::Active
                || f_ev.access.expired_at().is_some()
                || f_con.access.expired_at().is_some()
                || f_ev.strong_ref() != event.strong_ref()
                || f_con.strong_ref() != constraint.strong_ref()
            {
                return Ok(false);
            }
            if common_windows(&fresh, &[f_ev, f_con]).is_empty() {
                return Ok(false);
            }
            Ok(!effective_fact_viewers(&fresh, f_ev)
                .is_disjoint(&effective_fact_viewers(&fresh, f_con)))
        }
    }
}
