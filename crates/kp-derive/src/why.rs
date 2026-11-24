use std::collections::BTreeSet;

use kp_model::{
    ActorId, DerivationMode, DerivationView, EpochMs, FactId, JustificationId, StrongRef,
    ValidityWindow, VisibleView,
};
use kp_store::{Result, Store};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProofTree {
    pub fact_id: FactId,
    pub reference: StrongRef,
    pub unverified_gaps: Vec<String>,
    pub cycle: bool,
    pub justifications: Vec<ProofBranch>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProofBranch {
    pub id: JustificationId,
    pub rule_id: String,
    pub mode: DerivationMode,
    pub effective_validity: Vec<ValidityWindow>,
    pub premises: Vec<ProofTree>,
}

pub fn why(store: &Store, id: &FactId, viewer: &ActorId, at: EpochMs) -> Result<Option<ProofTree>> {
    let graph = store.derivation_view()?;
    let visible = graph.evaluate(viewer, at);
    if !visible.facts.contains(id) {
        return Ok(None);
    }
    let mut slice = graph.clone();
    slice.facts.retain(|id, _| visible.facts.contains(id));
    let proof_ids: BTreeSet<_> = visible.justifications.values().flatten().collect();
    slice.justifications.retain(|id, _| proof_ids.contains(id));
    Ok(render(&slice, &visible, id, &mut BTreeSet::new()))
}

fn render(
    graph: &DerivationView,
    visible: &VisibleView,
    id: &FactId,
    visited: &mut BTreeSet<FactId>,
) -> Option<ProofTree> {
    let fact = graph.facts.get(id)?;
    let cycle = !visited.insert(id.clone());
    let mut tree = ProofTree {
        fact_id: id.clone(),
        reference: fact.strong_ref(),
        unverified_gaps: fact.unverified_gaps.clone(),
        cycle,
        justifications: Vec::new(),
    };
    if cycle {
        return Some(tree);
    }
    for proof_id in visible.justifications.get(id).into_iter().flatten() {
        if let Some(proof) = graph.justifications.get(proof_id) {
            let premises: Vec<_> = proof
                .premises
                .iter()
                .filter_map(|p| graph.facts.get(&p.fact_id))
                .collect();
            tree.justifications.push(ProofBranch {
                id: proof.id.clone(),
                rule_id: proof.rule_id.clone(),
                mode: proof.mode,
                effective_validity: crate::rules::common_windows(graph, &premises),
                premises: proof
                    .premises
                    .iter()
                    .filter_map(|p| render(graph, visible, &p.fact_id, visited))
                    .collect(),
            });
        }
    }
    visited.remove(id);
    Some(tree)
}
