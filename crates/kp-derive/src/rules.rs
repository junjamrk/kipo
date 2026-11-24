// Fixpoint and distinct-support design adapted from lemmalog/src/eval.rs (MIT).
use std::collections::{BTreeMap, BTreeSet};

use kp_model::{
    derived_fact, justification, ActorId, Cardinality, DerivationMode, DerivationView, EpochMs,
    Fact, FactAccess, FactId, FactKind, FactStatus, Justification, KpError, LibraryId, Mutability,
    SlotContract, ValidityWindow,
};
use kp_store::{Result, Store};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Term {
    Variable(String),
    Constant(Value),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Atom {
    pub predicate: String,
    pub subject: Term,
    pub object: Term,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HornRule {
    pub id: String,
    pub head: Atom,
    pub body: Vec<Atom>,
}

impl HornRule {
    pub fn validate(&self) -> Result<()> {
        let bound: BTreeSet<_> = self
            .body
            .iter()
            .flat_map(|a| [&a.subject, &a.object])
            .filter_map(|t| match t {
                Term::Variable(v) => Some(v),
                Term::Constant(_) => None,
            })
            .collect();
        if self.id.trim().is_empty()
            || self.body.is_empty()
            || self.head.predicate.is_empty()
            || [&self.head.subject, &self.head.object]
                .iter()
                .any(|t| matches!(t, Term::Variable(v) if !bound.contains(v)))
        {
            return Err(KpError::invalid(
                "rule",
                "positive rules require a nonempty body and range-restricted head",
            )
            .into());
        }
        Ok(())
    }
}

#[derive(Default, Serialize)]
pub struct DeriveResult {
    pub derived: usize,
    pub justifications: usize,
}

pub fn derive(store: &mut Store) -> Result<DeriveResult> {
    derive_with_rules(store, &[])
}

pub fn derive_with_rules(store: &mut Store, rules: &[HornRule]) -> Result<DeriveResult> {
    for rule in rules {
        rule.validate()?;
        store.register_rule(&rule.id, &serde_json::to_value(rule)?)?;
    }
    let rules: Vec<HornRule> = store
        .rule_definitions()?
        .into_iter()
        .map(serde_json::from_value)
        .collect::<std::result::Result<_, _>>()?;
    let graph = store.derivation_view()?;
    let (facts, proofs) = recompute(store, &graph, &rules)?;
    let result = DeriveResult {
        derived: facts
            .iter()
            .filter(|f| {
                !graph
                    .facts
                    .get(&f.id)
                    .is_some_and(|old| old.status == FactStatus::Active)
            })
            .count(),
        justifications: proofs
            .iter()
            .filter(|p| {
                !graph
                    .justifications
                    .get(&p.id)
                    .is_some_and(|old| old.active)
            })
            .count(),
    };
    store.save_derivations(&facts, &proofs)?;
    let at = facts
        .iter()
        .map(|f| f.access.recorded_at())
        .max()
        .unwrap_or(EpochMs(0));
    store.retain_hard_proofs(&proofs.iter().map(|p| p.id.clone()).collect(), at)?;
    Ok(result)
}

pub(crate) fn recompute(
    store: &Store,
    graph: &DerivationView,
    rules: &[HornRule],
) -> Result<(Vec<Fact>, Vec<Justification>)> {
    for rule in rules {
        rule.validate()?;
    }
    let status = store.status()?;
    let mut engine = Engine {
        origin: store.library_id().clone(),
        author: status.owner_actor,
        tombstones: store.tombstone_ids()?,
        slots: store.slots()?,
        entities: store.entities()?,
        anchors: store.stale_anchors()?,
        graph: DerivationView {
            members: graph.members.clone(),
            source_scopes: graph.source_scopes.clone(),
            facts: graph
                .facts
                .iter()
                .filter(|(_, f)| {
                    matches!(f.status, FactStatus::Active | FactStatus::UnreachableHidden)
                        && f.access.expired_at().is_none()
                        && matches!(f.access, FactAccess::Base { .. })
                })
                .map(|(id, f)| {
                    let mut fact = f.clone();
                    fact.status = FactStatus::Active;
                    (id.clone(), fact)
                })
                .collect(),
            justifications: BTreeMap::new(),
        },
        outputs: BTreeSet::new(),
    };
    loop {
        let before = (engine.graph.facts.len(), engine.graph.justifications.len());
        engine.step(rules)?;
        if before == (engine.graph.facts.len(), engine.graph.justifications.len()) {
            break;
        }
    }
    let facts = engine
        .graph
        .facts
        .into_values()
        .filter(|f| engine.outputs.contains(&f.id))
        .collect();
    Ok((facts, engine.graph.justifications.into_values().collect()))
}

struct Engine {
    origin: LibraryId,
    author: ActorId,
    tombstones: BTreeSet<kp_model::ContentHash>,
    slots: BTreeMap<String, SlotContract>,
    entities: BTreeMap<FactId, BTreeSet<String>>,
    anchors: BTreeSet<FactId>,
    graph: DerivationView,
    outputs: BTreeSet<FactId>,
}

impl Engine {
    fn emit(
        &mut self,
        kind: FactKind,
        subject: &str,
        predicate: &str,
        object: Value,
        rule: &str,
        premises: &[&Fact],
    ) -> Result<()> {
        let at = premises
            .iter()
            .map(|p| p.access.recorded_at())
            .max()
            .ok_or_else(|| KpError::invalid("proof", "empty support"))?;
        let mut fact = derived_fact(
            &self.origin,
            &self.author,
            kind,
            subject,
            predicate,
            object,
            at,
        )?;
        if kind == FactKind::CollisionWarning {
            fact.tags = premises
                .iter()
                .flat_map(|p| p.tags.iter().cloned())
                .collect();
        }
        self.insert(fact, rule, premises)
    }

    fn insert(&mut self, fact: Fact, rule: &str, premises: &[&Fact]) -> Result<()> {
        let opaque = kp_store::tombstone_id(
            fact.origin_library.as_str().as_bytes(),
            &fact.origin_library,
            &fact.logical_key,
        )?;
        if self.tombstones.contains(&opaque) || common_windows(&self.graph, premises).is_empty() {
            return Ok(());
        }
        let proof = justification(&fact, DerivationMode::HardRule, rule, premises)?;
        self.outputs.insert(fact.id.clone());
        self.graph.facts.entry(fact.id.clone()).or_insert(fact);
        self.graph.justifications.insert(proof.id.clone(), proof);
        Ok(())
    }

    fn step(&mut self, rules: &[HornRule]) -> Result<()> {
        let facts: Vec<_> = self.graph.facts.values().cloned().collect();
        let aliases: Vec<_> = facts.iter().filter(|f| f.kind == FactKind::Alias).collect();
        let claims: Vec<_> = facts
            .iter()
            .filter(|f| {
                matches!(f.access, FactAccess::Base { .. }) || f.kind == FactKind::DerivedFact
            })
            .collect();
        self.alias_closure(&facts, &aliases)?;
        self.slot_conflicts(&claims, &aliases)?;
        self.event_collisions(&claims)?;
        self.governed_event_collisions(&claims)?;
        self.anchor_staleness(&facts)?;
        self.horn_rules(&facts, rules)
    }

    fn alias_closure(&mut self, facts: &[Fact], aliases: &[&Fact]) -> Result<()> {
        for alias in aliases {
            if let Some(other) = alias.object.as_str() {
                if other != alias.subject {
                    self.emit(
                        FactKind::Alias,
                        other,
                        "kp.core.alias",
                        json!(alias.subject),
                        "R3_ALIAS_CLOSURE",
                        &[alias],
                    )?;
                }
            }
            for next in aliases {
                if alias.object.as_str() == Some(&next.subject)
                    && next.object.as_str() != Some(&alias.subject)
                {
                    let Some(other) = next.object.as_str() else {
                        continue;
                    };
                    let premises = [*alias, *next];
                    self.emit(
                        FactKind::Alias,
                        &alias.subject,
                        "kp.core.alias",
                        json!(other),
                        "R3_ALIAS_CLOSURE",
                        &premises,
                    )?;
                    for left in facts {
                        for right in facts {
                            if left.subject == alias.subject
                                && right.subject == other
                                && left.predicate == right.predicate
                                && left.object != right.object
                                && matches!(left.access, FactAccess::Base { .. })
                                && matches!(right.access, FactAccess::Base { .. })
                                && self.slots.get(&left.predicate).is_some_and(|s| {
                                    s.cardinality == Cardinality::Single
                                        && s.mutability == Mutability::Fixed
                                })
                            {
                                self.emit(
                                    FactKind::Conflict,
                                    &alias.subject,
                                    "kp.core.alias_conflict",
                                    json!(other),
                                    "R3_ALIAS_CONFLICT",
                                    &[alias, next, left, right],
                                )?;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn slot_conflicts(&mut self, claims: &[&Fact], aliases: &[&Fact]) -> Result<()> {
        for (i, left) in claims.iter().enumerate() {
            for right in claims.iter().skip(i + 1) {
                if left.predicate == right.predicate
                    && left.object != right.object
                    && self
                        .slots
                        .get(&left.predicate)
                        .is_some_and(|s| s.cardinality == Cardinality::Single)
                {
                    if left.subject == right.subject {
                        let origin = if left.origin_library == right.origin_library {
                            &left.origin_library
                        } else {
                            &self.origin
                        };
                        let fact = kp_model::slot_conflict(origin, &self.author, left, right)?;
                        self.insert(fact, "R1_SLOT_CONFLICT", &[left, right])?;
                    } else {
                        for alias in aliases {
                            if alias.subject == left.subject
                                && alias.object.as_str() == Some(&right.subject)
                            {
                                let fact = kp_model::slot_conflict(
                                    &self.origin,
                                    &self.author,
                                    left,
                                    right,
                                )?;
                                self.insert(fact, "R1_SLOT_CONFLICT", &[left, right, alias])?;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn event_collisions(&mut self, claims: &[&Fact]) -> Result<()> {
        for event in claims.iter().filter(|f| f.kind == FactKind::Event) {
            if crate::collision::is_governed_or_concept_fact(event)
                || crate::collision::is_system_internal_fact(event)
            {
                continue;
            }
            for constraint in claims.iter().filter(|f| f.kind == FactKind::Constraint) {
                if crate::collision::is_governed_or_concept_fact(constraint) {
                    continue;
                }
                let event_triggers = self.triggers(event);
                if !event_triggers.is_disjoint(&self.triggers(constraint)) {
                    self.emit(
                        FactKind::CollisionWarning,
                        &event.subject,
                        "kp.core.event_constraint_collision",
                        json!({
                            "event": event.object,
                            "constraint": constraint.object,
                            "premises": [event.strong_ref(), constraint.strong_ref()],
                        }),
                        "R2_EVENT_CONSTRAINT_COLLISION",
                        &[event, constraint],
                    )?;
                }
            }
        }
        Ok(())
    }

    fn governed_event_collisions(&mut self, claims: &[&Fact]) -> Result<()> {
        let alignment = crate::collision::ConceptAlignmentTable::from_graph(&self.graph);
        let emissions = crate::collision::compute_governed_collisions(
            &self.origin,
            &self.author,
            &self.graph,
            claims,
            &alignment,
        )?;
        for emission in emissions {
            self.insert(
                emission.conclusion,
                "R2_GOVERNED_COLLISION",
                &emission.premises,
            )?;
        }
        Ok(())
    }

    fn anchor_staleness(&mut self, facts: &[Fact]) -> Result<()> {
        for fact in facts {
            if self.anchors.contains(&fact.id) {
                self.emit(
                    FactKind::CollisionWarning,
                    &fact.subject,
                    "kp.code.anchor_stale",
                    json!(fact.strong_ref()),
                    "R4_ANCHOR_STALENESS",
                    &[fact],
                )?;
            }
        }
        Ok(())
    }

    fn horn_rules(&mut self, facts: &[Fact], rules: &[HornRule]) -> Result<()> {
        for rule in rules {
            let mut matches = vec![(BTreeMap::new(), Vec::new())];
            for atom in &rule.body {
                let mut next = Vec::new();
                for (bindings, premises) in matches {
                    for fact in facts.iter().filter(|f| f.predicate == atom.predicate) {
                        let mut bindings = bindings.clone();
                        if unify(&atom.subject, &json!(fact.subject), &mut bindings)
                            && unify(&atom.object, &fact.object, &mut bindings)
                        {
                            let mut premises = premises.clone();
                            premises.push(fact);
                            next.push((bindings, premises));
                        }
                    }
                }
                matches = next;
            }
            for (bindings, premises) in matches {
                let subject = resolve(&rule.head.subject, &bindings)?;
                let subject = subject
                    .as_str()
                    .ok_or_else(|| KpError::invalid("rule head", "subject must be a string"))?;
                self.emit(
                    FactKind::DerivedFact,
                    subject,
                    &rule.head.predicate,
                    resolve(&rule.head.object, &bindings)?,
                    &rule.id,
                    &premises,
                )?;
            }
        }
        Ok(())
    }

    fn triggers(&self, fact: &Fact) -> BTreeSet<String> {
        let mut triggers = fact.tags.clone();
        triggers.insert(fact.subject.clone());
        if let Some(entities) = self.entities.get(&fact.id) {
            triggers.extend(entities.iter().cloned());
        }
        for field in ["tags", "entities", "conditions", "triggers"] {
            if let Some(values) = fact.object.get(field) {
                match values {
                    Value::String(value) => {
                        triggers.insert(value.clone());
                    }
                    Value::Array(values) => {
                        triggers.extend(values.iter().filter_map(Value::as_str).map(str::to_owned))
                    }
                    Value::Null | Value::Bool(_) | Value::Number(_) | Value::Object(_) => {}
                }
            }
        }
        triggers
            .into_iter()
            .map(|value| value.trim().to_lowercase())
            .filter(|value| !value.is_empty())
            .collect()
    }
}

fn unify(term: &Term, value: &Value, bindings: &mut BTreeMap<String, Value>) -> bool {
    match term {
        Term::Constant(expected) => expected == value,
        Term::Variable(name) => match bindings.get(name) {
            Some(expected) => expected == value,
            None => {
                bindings.insert(name.clone(), value.clone());
                true
            }
        },
    }
}
fn resolve(term: &Term, bindings: &BTreeMap<String, Value>) -> Result<Value> {
    match term {
        Term::Constant(value) => Ok(value.clone()),
        Term::Variable(name) => bindings
            .get(name)
            .cloned()
            .ok_or_else(|| KpError::invalid("rule", "unbound variable").into()),
    }
}

pub(crate) fn common_windows(graph: &DerivationView, facts: &[&Fact]) -> Vec<ValidityWindow> {
    let windows = kp_model::effective_windows(graph);
    facts.iter().fold(
        vec![ValidityWindow {
            valid_from: EpochMs(i64::MIN),
            valid_until: EpochMs::OPEN_END,
        }],
        |current, fact| {
            current
                .iter()
                .flat_map(|a| {
                    windows
                        .get(&fact.id)
                        .into_iter()
                        .flatten()
                        .filter_map(|b| a.intersect(b))
                })
                .collect()
        },
    )
}

pub fn verify_hard_rules_match_full_recompute(store: &Store) -> Result<()> {
    let rules = store
        .rule_definitions()?
        .into_iter()
        .map(serde_json::from_value)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    verify_with_rules(store, &rules)
}

pub fn verify_with_rules(store: &Store, rules: &[HornRule]) -> Result<()> {
    let graph = store.derivation_view()?;
    let (facts, proofs) = recompute(store, &graph, rules)?;
    let expected_facts: BTreeSet<_> = facts.iter().map(Fact::strong_ref).collect();
    let proof_identity = |p: &Justification| {
        (
            p.id.clone(),
            (
                p.conclusion.clone(),
                p.rule_id.clone(),
                p.rule_version.clone(),
                p.premises
                    .iter()
                    .map(|premise| (premise.fact_id.clone(), premise.strong_ref.clone()))
                    .collect::<Vec<_>>(),
            ),
        )
    };
    let expected_proofs: BTreeMap<_, _> = proofs.iter().map(proof_identity).collect();
    let live_proofs: BTreeMap<_, _> = graph
        .justifications
        .values()
        .filter(|p| {
            p.active
                && p.mode == DerivationMode::HardRule
                && graph
                    .facts
                    .get(&p.conclusion)
                    .is_some_and(|f| &f.origin_library == store.library_id())
        })
        .map(proof_identity)
        .collect();
    let live_facts: BTreeSet<_> = graph
        .facts
        .values()
        .filter(|f| {
            f.status == FactStatus::Active
                && &f.origin_library == store.library_id()
                && graph
                    .justifications
                    .values()
                    .any(|p| p.conclusion == f.id && p.mode == DerivationMode::HardRule)
        })
        .map(Fact::strong_ref)
        .collect();
    if expected_facts != live_facts || expected_proofs != live_proofs {
        return Err(KpError::invalid(
            "hard-rule oracle",
            "live facts or proofs differ from full recompute",
        )
        .into());
    }
    Ok(())
}
