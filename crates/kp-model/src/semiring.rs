use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{
    ActorId, AudienceSpec, DerivationMode, EpochMs, Fact, FactAccess, FactId, FactStatus,
    GovernedScope, JustificationId, SourceScopeId, StrongRef,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Premise {
    pub fact_id: FactId,
    pub strong_ref: StrongRef,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Justification {
    pub id: JustificationId,
    pub conclusion: FactId,
    pub mode: DerivationMode,
    pub rule_id: String,
    pub rule_version: String,
    pub premises: Vec<Premise>,
    pub active: bool,
    pub created_at: EpochMs,
    pub invalidated_at: Option<EpochMs>,
    pub invalidation_reason: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct DerivationView {
    pub members: BTreeSet<ActorId>,
    pub source_scopes: BTreeMap<SourceScopeId, AudienceSpec>,
    pub facts: BTreeMap<FactId, Fact>,
    pub justifications: BTreeMap<JustificationId, Justification>,
}

/// Deterministic projection for one joint (viewer, time) query. No support-count cap.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct VisibleView {
    pub facts: BTreeSet<FactId>,
    pub justifications: BTreeMap<FactId, Vec<JustificationId>>,
}

impl DerivationView {
    pub fn evaluate(&self, viewer: &ActorId, at: EpochMs) -> VisibleView {
        let visible = self.evaluate_positive(viewer, at);
        let conflicts: BTreeSet<_> = self
            .facts
            .values()
            .filter(|f| f.predicate == "kp.core.alias_conflict" && visible.facts.contains(&f.id))
            .filter_map(|f| {
                f.object
                    .as_str()
                    .map(|other| (f.subject.clone(), other.to_owned()))
            })
            .collect();
        if conflicts.is_empty() {
            return visible;
        }
        let mut guarded = self.clone();
        for fact in guarded.facts.values_mut().filter(|f| {
            f.kind == crate::FactKind::Alias && matches!(f.access, FactAccess::Derived { .. })
        }) {
            if fact.object.as_str().is_some_and(|other| {
                conflicts.contains(&(fact.subject.clone(), other.to_owned()))
                    || conflicts.contains(&(other.to_owned(), fact.subject.clone()))
            }) {
                fact.status = FactStatus::Invalidated;
            }
        }
        guarded.evaluate_positive(viewer, at)
    }

    fn evaluate_positive(&self, viewer: &ActorId, at: EpochMs) -> VisibleView {
        let references: BTreeMap<_, _> = self
            .facts
            .iter()
            .map(|(id, fact)| (id.clone(), fact.strong_ref()))
            .collect();
        let mut visible = VisibleView::default();
        for (id, fact) in &self.facts {
            if !eligible(fact) {
                continue;
            }
            if let FactAccess::Base { audience, stamp } = &fact.access {
                let scope_allowed = match &fact.source_scope_id {
                    None => true,
                    Some(scope_id) => self
                        .source_scopes
                        .get(scope_id)
                        .is_some_and(|scope_aud| scope_aud.allows(viewer, &self.members)),
                };
                let allowed = audience.allows(viewer, &self.members) && scope_allowed;
                if allowed && stamp.valid.contains(at) {
                    visible.facts.insert(id.clone());
                }
            }
        }

        // Starting at grounded base facts computes the least fixpoint; cycles cannot seed it.
        loop {
            let before = visible.facts.len();
            for justification in self.justifications.values() {
                if self.supported_for_viewer(justification, viewer, &visible.facts, &references) {
                    visible.facts.insert(justification.conclusion.clone());
                }
            }
            if visible.facts.len() == before {
                break;
            }
        }
        for (id, justification) in &self.justifications {
            if self.supported_for_viewer(justification, viewer, &visible.facts, &references) {
                visible
                    .justifications
                    .entry(justification.conclusion.clone())
                    .or_default()
                    .push(id.clone());
            }
        }
        visible
    }

    fn supported_for_viewer(
        &self,
        justification: &Justification,
        viewer: &ActorId,
        visible: &BTreeSet<FactId>,
        references: &BTreeMap<FactId, StrongRef>,
    ) -> bool {
        self.supported(justification, visible, references)
            && self
                .facts
                .get(&justification.conclusion)
                .is_some_and(|fact| derived_scope_allows(fact, viewer))
    }

    fn supported(
        &self,
        justification: &Justification,
        visible: &BTreeSet<FactId>,
        references: &BTreeMap<FactId, StrongRef>,
    ) -> bool {
        justification.active
            && justification.invalidated_at.is_none()
            && !justification.premises.is_empty()
            && self
                .facts
                .get(&justification.conclusion)
                .is_some_and(|fact| {
                    eligible(fact) && matches!(fact.access, FactAccess::Derived { .. })
                })
            && justification.premises.iter().all(|premise| {
                visible.contains(&premise.fact_id)
                    && references.get(&premise.fact_id) == Some(&premise.strong_ref)
            })
    }
}

/// Enforces target-actor visibility on derived conclusions carrying an explicit `GovernedScope`
/// (`SelfOnly` conclusions never surface to other viewers even when base premises are shared).
pub fn derived_scope_allows(fact: &Fact, viewer: &ActorId) -> bool {
    match &fact.governed_scope {
        Some(GovernedScope::SelfOnly { actor }) => actor == viewer,
        Some(GovernedScope::Actors { actors }) => actors.contains(viewer),
        None | Some(GovernedScope::RelationGroup { .. }) | Some(GovernedScope::Resource { .. }) => {
            true
        }
    }
}

fn eligible(fact: &Fact) -> bool {
    fact.status == FactStatus::Active
        && fact.access.expired_at().is_none()
        && fact.validate().is_ok()
}

pub fn is_visible_and_valid(
    fact_id: &FactId,
    viewer: &ActorId,
    at: EpochMs,
    graph: &DerivationView,
) -> bool {
    graph.evaluate(viewer, at).facts.contains(fact_id)
}

pub fn visible_justifications(
    fact_id: &FactId,
    viewer: &ActorId,
    at: EpochMs,
    graph: &DerivationView,
) -> Vec<JustificationId> {
    graph
        .evaluate(viewer, at)
        .justifications
        .remove(fact_id)
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "semiring_tests.rs"]
mod tests;

/// Union of grounded proof intervals; each conjunction intersects its own premise windows.
/// Cycles add no intervals until grounded, and endpoints come from the finite base set.
pub fn effective_windows(graph: &DerivationView) -> BTreeMap<FactId, Vec<crate::ValidityWindow>> {
    use crate::ValidityWindow;
    let mut windows: BTreeMap<_, _> = graph
        .facts
        .values()
        .filter_map(|f| {
            if !eligible(f) {
                return None;
            }
            match &f.access {
                FactAccess::Base { stamp, .. } => Some((f.id.clone(), vec![stamp.valid])),
                FactAccess::Derived { .. } => None,
            }
        })
        .collect();
    loop {
        let before = windows.clone();
        for proof in graph
            .justifications
            .values()
            .filter(|p| p.active && p.invalidated_at.is_none() && !p.premises.is_empty())
        {
            if !graph.facts.get(&proof.conclusion).is_some_and(eligible) {
                continue;
            }
            let mut intersections = vec![ValidityWindow {
                valid_from: EpochMs(i64::MIN),
                valid_until: EpochMs::OPEN_END,
            }];
            for premise in &proof.premises {
                if !graph
                    .facts
                    .get(&premise.fact_id)
                    .is_some_and(|f| f.strong_ref() == premise.strong_ref)
                {
                    intersections.clear();
                    break;
                }
                intersections = intersections
                    .iter()
                    .flat_map(|a| {
                        windows
                            .get(&premise.fact_id)
                            .into_iter()
                            .flatten()
                            .filter_map(|b| a.intersect(b))
                    })
                    .collect();
            }
            let entries = windows.entry(proof.conclusion.clone()).or_default();
            entries.extend(intersections);
            entries.sort_by_key(|w| (w.valid_from, w.valid_until));
            let mut merged: Vec<ValidityWindow> = Vec::new();
            for window in entries.iter() {
                if let Some(last) = merged
                    .last_mut()
                    .filter(|last| last.valid_until >= window.valid_from)
                {
                    last.valid_until = last.valid_until.max(window.valid_until);
                } else {
                    merged.push(*window);
                }
            }
            *entries = merged;
        }
        if before == windows {
            break;
        }
    }
    windows
}

impl DerivationView {
    /// Structural JTMS support ignores ACL/time, retaining live alternatives without inventing cycle roots.
    pub fn grounded_support(&self) -> BTreeSet<FactId> {
        let mut graph = self.clone();
        for fact in graph
            .facts
            .values_mut()
            .filter(|f| f.status == FactStatus::UnreachableHidden)
        {
            fact.status = FactStatus::Active;
        }
        let references = graph
            .facts
            .iter()
            .map(|(id, f)| (id.clone(), f.strong_ref()))
            .collect();
        let mut grounded: BTreeSet<_> = graph
            .facts
            .values()
            .filter(|f| eligible(f) && matches!(f.access, FactAccess::Base { .. }))
            .map(|f| f.id.clone())
            .collect();
        loop {
            let before = grounded.len();
            for proof in graph.justifications.values() {
                if graph.supported(proof, &grounded, &references) {
                    grounded.insert(proof.conclusion.clone());
                }
            }
            if before == grounded.len() {
                return grounded;
            }
        }
    }
}
