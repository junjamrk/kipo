use std::collections::{BTreeMap, BTreeSet};

use kp_model::{
    ActorId, AudienceSpec, CommunityConcept, ConceptId, EpochMs, Fact, FactAccess, FactId,
    FactKind, FactStatus, GovernedScope, KpError, LibraryId, MembershipRelation, Premise,
    SourceScopeId, StrongRef,
};
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;

use super::{VisibleDocument, VisibleGuardPremise};
use crate::facts::{fact_default_passage, read_fact};
use crate::tombstone::tombstone_id;
use crate::{validate_vector, CodeLocus, Embedder, Result};

pub(crate) fn filter_visible_fact_ids_in_tx(
    db: &Connection,
    viewer: &ActorId,
    at: EpochMs,
    candidate_ids: &[FactId],
) -> Result<BTreeSet<FactId>> {
    if candidate_ids.is_empty() {
        return Ok(BTreeSet::new());
    }
    let mut checker = ReverseSemiringChecker::new(db, viewer, at)?;
    let mut visible = BTreeSet::new();
    for fid in candidate_ids {
        if checker.is_fact_visible(fid)? {
            visible.insert(fid.clone());
        }
    }
    Ok(visible)
}

pub(super) struct ReverseSemiringChecker<'a> {
    db: &'a Connection,
    viewer: &'a ActorId,
    at: EpochMs,
    pub(super) members: BTreeSet<ActorId>,
    source_scopes: BTreeMap<SourceScopeId, AudienceSpec>,
    tombstones: BTreeSet<String>,
    alias_conflicts: BTreeSet<(String, String)>,
    fact_cache: BTreeMap<FactId, Option<Fact>>,
    memo: BTreeMap<FactId, bool>,
}

impl<'a> ReverseSemiringChecker<'a> {
    pub(super) fn new(db: &'a Connection, viewer: &'a ActorId, at: EpochMs) -> Result<Self> {
        let mut members = BTreeSet::new();
        {
            let mut stmt = db.prepare_cached(
                "SELECT actor_id FROM members_tokens WHERE revoked_at IS NULL ORDER BY actor_id",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                members.insert(row.get::<_, String>(0)?.parse()?);
            }
        }
        let mut source_scopes = BTreeMap::new();
        {
            let mut stmt = db.prepare_cached(
                "SELECT scope_id, current_audience_json FROM source_scopes ORDER BY scope_id",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let sid: SourceScopeId = row.get::<_, String>(0)?.parse()?;
                let aud: AudienceSpec = serde_json::from_str(&row.get::<_, String>(1)?)?;
                source_scopes.insert(sid, aud);
            }
        }
        let mut tombstones = BTreeSet::new();
        {
            let mut stmt = db.prepare_cached("SELECT tombstone_id FROM tombstones")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                tombstones.insert(row.get::<_, String>(0)?);
            }
        }

        let mut checker = Self {
            db,
            viewer,
            at,
            members,
            source_scopes,
            tombstones,
            alias_conflicts: BTreeSet::new(),
            fact_cache: BTreeMap::new(),
            memo: BTreeMap::new(),
        };

        // Evaluate positive visibility of any `kp.core.alias_conflict` facts first (usually 0 rows).
        let conflict_candidates: Vec<FactId> = {
            let mut stmt = db.prepare_cached(
                "SELECT fact_id FROM facts
                 WHERE predicate = 'kp.core.alias_conflict'
                   AND status IN ('active', 'superseded')
                   AND expired_at IS NULL
                 ORDER BY fact_id",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.map(|r| Ok(r?.parse()?)).collect::<Result<Vec<_>>>()?
        };
        if !conflict_candidates.is_empty() {
            let mut visible_conflicts = BTreeSet::new();
            for cid in conflict_candidates {
                if checker.is_fact_visible(&cid)? {
                    if let Some(fact) = checker.load_fact(&cid)? {
                        if let Some(other) = fact.object.as_str() {
                            visible_conflicts.insert((fact.subject.clone(), other.to_owned()));
                        }
                    }
                }
            }
            if !visible_conflicts.is_empty() {
                checker.alias_conflicts = visible_conflicts;
                checker.memo.clear();
            }
        }

        Ok(checker)
    }

    pub(super) fn cached_fact(&self, id: &FactId) -> Option<&Fact> {
        self.fact_cache.get(id).and_then(Option::as_ref)
    }

    pub(super) fn load_fact(&mut self, id: &FactId) -> Result<Option<Fact>> {
        if let Some(cached) = self.fact_cache.get(id) {
            return Ok(cached.clone());
        }
        let fetched = self
            .db
            .prepare_cached("SELECT * FROM facts WHERE fact_id = ?1")?
            .query_row([id.as_str()], |row| Ok(read_fact(row)))
            .optional()?
            .transpose()?;
        let guarded = match fetched {
            Some(fact) if self.tombstones.is_empty() => Some(fact),
            Some(fact) => {
                let opaque = tombstone_id(
                    fact.origin_library.as_str().as_bytes(),
                    &fact.origin_library,
                    &fact.logical_key,
                )?;
                if self.tombstones.contains(opaque.as_str()) {
                    None
                } else {
                    Some(fact)
                }
            }
            None => None,
        };
        self.fact_cache.insert(id.clone(), guarded.clone());
        Ok(guarded)
    }

    pub(super) fn is_fact_eligible(&mut self, fact: &Fact) -> Result<bool> {
        if fact.access.expired_at().is_some() || fact.validate().is_err() {
            return Ok(false);
        }
        if fact.status == FactStatus::Active {
            return Ok(true);
        }
        if fact.status == FactStatus::Superseded {
            if let FactAccess::Base { stamp, .. } = &fact.access {
                if stamp.valid.contains(self.at) {
                    if let Some(next_id) = &fact.superseded_by {
                        if let Some(next) = self.load_fact(next_id)? {
                            if let FactAccess::Base {
                                stamp: next_stamp, ..
                            } = &next.access
                            {
                                return Ok(self.at < next_stamp.valid.valid_from);
                            }
                        }
                    }
                }
            }
        }
        Ok(false)
    }

    pub(super) fn is_fact_visible(&mut self, id: &FactId) -> Result<bool> {
        let mut stack = BTreeSet::new();
        let (vis, _) = self.eval_node(id, &mut stack)?;
        Ok(vis)
    }

    fn eval_node(&mut self, id: &FactId, stack: &mut BTreeSet<FactId>) -> Result<(bool, bool)> {
        if let Some(&cached) = self.memo.get(id) {
            return Ok((cached, false));
        }
        if stack.contains(id) {
            return Ok((false, true));
        }
        let Some(fact) = self.load_fact(id)? else {
            self.memo.insert(id.clone(), false);
            return Ok((false, false));
        };
        if !self.is_fact_eligible(&fact)? {
            self.memo.insert(id.clone(), false);
            return Ok((false, false));
        }

        match &fact.access {
            FactAccess::Base { audience, stamp } => {
                let scope_allowed = match &fact.source_scope_id {
                    None => true,
                    Some(scope_id) => self
                        .source_scopes
                        .get(scope_id)
                        .is_some_and(|aud| aud.allows(self.viewer, &self.members)),
                };
                let visible = audience.allows(self.viewer, &self.members)
                    && scope_allowed
                    && stamp.valid.contains(self.at);
                self.memo.insert(id.clone(), visible);
                Ok((visible, false))
            }
            FactAccess::Derived { .. } => {
                if !kp_model::derived_scope_allows(&fact, self.viewer) {
                    self.memo.insert(id.clone(), false);
                    return Ok((false, false));
                }
                if fact.kind == FactKind::Alias
                    && fact.object.as_str().is_some_and(|other| {
                        self.alias_conflicts
                            .contains(&(fact.subject.clone(), other.to_owned()))
                            || self
                                .alias_conflicts
                                .contains(&(other.to_owned(), fact.subject.clone()))
                    })
                {
                    self.memo.insert(id.clone(), false);
                    return Ok((false, false));
                }

                let just_ids: Vec<String> = {
                    let mut stmt = self.db.prepare_cached(
                        "SELECT just_id FROM justifications
                         WHERE conclusion_fact_id = ?1 AND active = 1 AND invalidated_at IS NULL
                         ORDER BY just_id",
                    )?;
                    let rows = stmt.query_map([id.as_str()], |r| r.get::<_, String>(0))?;
                    rows.collect::<std::result::Result<Vec<_>, _>>()?
                };

                stack.insert(id.clone());
                let mut any_cycle = false;

                for just_id in just_ids {
                    let premises: Vec<Premise> = {
                        let mut stmt = self.db.prepare_cached(
                            "SELECT premise_fact_id, premise_origin_lib, premise_logical_key, premise_sha256
                             FROM justification_premises
                             WHERE just_id = ?1
                             ORDER BY premise_fact_id",
                        )?;
                        let rows = stmt.query_map([&just_id], |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, String>(2)?,
                                r.get::<_, String>(3)?,
                            ))
                        })?;
                        let mut list = Vec::new();
                        for row in rows {
                            let (fid, orig, key, sha) = row?;
                            list.push(Premise {
                                fact_id: fid.parse()?,
                                strong_ref: StrongRef {
                                    origin_library: orig.parse()?,
                                    logical_key: key.parse()?,
                                    content_sha256: sha.parse()?,
                                },
                            });
                        }
                        list
                    };
                    if premises.is_empty() {
                        continue;
                    }
                    let mut all_supported = true;
                    for premise in &premises {
                        let Some(p_fact) = self.load_fact(&premise.fact_id)? else {
                            all_supported = false;
                            break;
                        };
                        if p_fact.strong_ref() != premise.strong_ref {
                            all_supported = false;
                            break;
                        }
                        let (p_vis, p_cycle) = self.eval_node(&premise.fact_id, stack)?;
                        if !p_vis {
                            all_supported = false;
                            any_cycle |= p_cycle;
                            break;
                        }
                    }
                    if all_supported {
                        stack.remove(id);
                        self.memo.insert(id.clone(), true);
                        return Ok((true, false));
                    }
                }

                stack.remove(id);
                if stack.is_empty() || !any_cycle {
                    self.memo.insert(id.clone(), false);
                    Ok((false, false))
                } else {
                    Ok((false, true))
                }
            }
        }
    }

    pub(super) fn visible_supported_premise_ids(
        &mut self,
        conclusion_id: &FactId,
    ) -> Result<BTreeSet<FactId>> {
        let just_ids: Vec<String> = {
            let mut stmt = self.db.prepare_cached(
                "SELECT just_id FROM justifications
                 WHERE conclusion_fact_id = ?1 AND active = 1 AND invalidated_at IS NULL
                 ORDER BY just_id",
            )?;
            let rows = stmt.query_map([conclusion_id.as_str()], |r| r.get::<_, String>(0))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };

        let mut supported = BTreeSet::new();
        for just_id in just_ids {
            let premises: Vec<Premise> = {
                let mut stmt = self.db.prepare_cached(
                    "SELECT premise_fact_id, premise_origin_lib, premise_logical_key, premise_sha256
                     FROM justification_premises
                     WHERE just_id = ?1
                     ORDER BY premise_fact_id",
                )?;
                let rows = stmt.query_map([&just_id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })?;
                let mut list = Vec::new();
                for row in rows {
                    let (fid, orig, key, sha) = row?;
                    list.push(Premise {
                        fact_id: fid.parse()?,
                        strong_ref: StrongRef {
                            origin_library: orig.parse()?,
                            logical_key: key.parse()?,
                            content_sha256: sha.parse()?,
                        },
                    });
                }
                list
            };
            if premises.is_empty() {
                continue;
            }
            let mut all_visible = true;
            for premise in &premises {
                let Some(p_fact) = self.load_fact(&premise.fact_id)? else {
                    all_visible = false;
                    break;
                };
                if p_fact.strong_ref() != premise.strong_ref
                    || !self.is_fact_visible(&premise.fact_id)?
                {
                    all_visible = false;
                    break;
                }
            }
            if all_visible {
                for premise in premises {
                    supported.insert(premise.fact_id);
                }
            }
        }
        Ok(supported)
    }
}

pub(super) fn load_fact_code_loci(db: &Connection, fact_id: &FactId) -> Result<Vec<CodeLocus>> {
    let mut stmt = db.prepare_cached(
        "SELECT repo_root, file_path, language, symbol_name, sig_hash, body_hash, start_line, end_line
         FROM code_anchors
         WHERE fact_id = ?1 AND repo_root IS NOT NULL
         ORDER BY anchor_id",
    )?;
    let mut rows = stmt.query([fact_id.as_str()])?;
    let mut loci = Vec::new();
    while let Some(row) = rows.next()? {
        loci.push(CodeLocus {
            repo_root: row.get(0)?,
            file_path: row.get(1)?,
            language: row.get(2)?,
            symbol_name: row.get(3)?,
            sig_hash: row_str(row, 4)?.parse()?,
            body_hash: row_str(row, 5)?.parse()?,
            start_line: row.get(6)?,
            end_line: row.get(7)?,
        });
    }
    Ok(loci)
}

fn collect_scope_concept_id(
    scope: &GovernedScope,
    known: &BTreeMap<ConceptId, CommunityConcept>,
    out: &mut BTreeSet<ConceptId>,
) {
    if let GovernedScope::RelationGroup {
        group_concept_id, ..
    } = scope
    {
        if known.contains_key(group_concept_id) {
            out.insert(group_concept_id.clone());
        }
    }
}

fn collect_json_concept_ids(
    value: &Value,
    known: &BTreeMap<ConceptId, CommunityConcept>,
    out: &mut BTreeSet<ConceptId>,
) {
    match value {
        Value::String(s) => {
            if let Ok(cid) = s.parse::<ConceptId>() {
                if known.contains_key(&cid) {
                    out.insert(cid);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_json_concept_ids(item, known, out);
            }
        }
        Value::Object(fields) => {
            for (k, v) in fields {
                if let Ok(cid) = k.parse::<ConceptId>() {
                    if known.contains_key(&cid) {
                        out.insert(cid);
                    }
                }
                collect_json_concept_ids(v, known, out);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn collect_inline_fact_concept_ids(
    fact: &Fact,
    known: &BTreeMap<ConceptId, CommunityConcept>,
    out: &mut BTreeSet<ConceptId>,
) {
    if let Some(scope) = &fact.governed_scope {
        collect_scope_concept_id(scope, known, out);
    }
    if let Some(broader) = &fact.broader_scope {
        collect_scope_concept_id(&broader.narrow_active, known, out);
        collect_scope_concept_id(&broader.broader_candidate, known, out);
    }
    if fact.kind == FactKind::Membership {
        if let Ok(rel) = serde_json::from_value::<MembershipRelation>(fact.object.clone()) {
            if known.contains_key(&rel.group_concept_id) {
                out.insert(rel.group_concept_id);
            }
        }
    }
    for token in fact
        .tags
        .iter()
        .map(String::as_str)
        .chain([fact.subject.as_str(), fact.predicate.as_str()])
    {
        if let Ok(cid) = token.parse::<ConceptId>() {
            if known.contains_key(&cid) {
                out.insert(cid);
            }
        }
    }
    collect_json_concept_ids(&fact.object, known, out);
}

pub(super) fn load_linked_concepts_for_guard(
    db: &Connection,
    fact: &Fact,
    premises: &[VisibleGuardPremise],
    known: &BTreeMap<ConceptId, CommunityConcept>,
) -> Result<Vec<CommunityConcept>> {
    let mut ids = BTreeSet::new();
    let mut link_stmt = db.prepare_cached(
        "SELECT logical_key FROM change_log
         WHERE target_id = ?1 AND op = 'fact_concept'
         ORDER BY seq",
    )?;

    for item in std::iter::once(fact).chain(premises.iter().map(|p| &p.fact)) {
        collect_inline_fact_concept_ids(item, known, &mut ids);
        let mut rows = link_stmt.query([item.id.as_str()])?;
        while let Some(row) = rows.next()? {
            if let Some(raw) = row.get::<_, Option<String>>(0)? {
                if let Ok(cid) = raw.parse::<ConceptId>() {
                    if known.contains_key(&cid) {
                        ids.insert(cid);
                    }
                }
            }
        }
    }

    Ok(ids
        .into_iter()
        .filter_map(|cid| known.get(&cid).cloned())
        .collect())
}

pub(super) fn materialize_fact_doc(
    db: &Connection,
    library_id: &LibraryId,
    fact: &Fact,
    stored_vec: Option<(&[u8], usize)>,
    embedder: &(impl Embedder + ?Sized),
) -> Result<VisibleDocument> {
    materialize_fact_doc_with_entities(db, library_id, fact, None, stored_vec, embedder)
}

pub(super) fn materialize_fact_doc_for_candidate(
    db: &Connection,
    library_id: &LibraryId,
    fact: &Fact,
    stored_vec: Option<(&[u8], usize)>,
    vector_enabled: bool,
    embedder: &(impl Embedder + ?Sized),
) -> Result<VisibleDocument> {
    if !vector_enabled {
        return materialize_fact_doc_with_entities(
            db,
            library_id,
            fact,
            None,
            None,
            &crate::FtsOnlyEmbedder,
        );
    }
    materialize_fact_doc_with_entities(db, library_id, fact, None, stored_vec, embedder)
}

pub(super) fn materialize_fact_doc_with_entities(
    db: &Connection,
    library_id: &LibraryId,
    fact: &Fact,
    preloaded_entities: Option<&BTreeSet<String>>,
    stored_vec: Option<(&[u8], usize)>,
    embedder: &(impl Embedder + ?Sized),
) -> Result<VisibleDocument> {
    let mut entities = BTreeSet::from([fact.subject.to_lowercase()]);
    if let Some(preloaded) = preloaded_entities {
        entities.extend(preloaded.iter().cloned());
    } else {
        let mut stmt = db.prepare_cached(
            "SELECT entity_slug FROM fact_entities WHERE fact_id=?1 ORDER BY entity_slug",
        )?;
        for entity in stmt.query_map([fact.id.as_str()], |r| r.get::<_, String>(0))? {
            entities.insert(entity?);
        }
    }
    if fact.kind == FactKind::Alias {
        if let Some(alias) = fact.object.as_str() {
            entities.insert(alias.to_lowercase());
        }
    }
    let mut object = fact.object.clone();
    let quarantined = if fact.kind == FactKind::SkillVersion {
        let approved = crate::skills::locally_approved(db, fact)?;
        object["exec_approved"] = approved.into();
        !approved
    } else {
        false
    };
    let mut passage = if fact.kind == FactKind::SkillVersion {
        format!(
            "{} {} {} {}",
            fact.subject,
            fact.predicate,
            object,
            fact.tags.iter().cloned().collect::<Vec<_>>().join(" ")
        )
    } else {
        fact_default_passage(fact)
    };
    if quarantined {
        passage.push('\n');
        passage.push_str(kp_model::READ_ONLY_SKILL);
    }
    let model_id = embedder.model_id();
    let vector = if !embedder.is_enabled() {
        Vec::new()
    } else if model_id == "custom" || model_id == "hash:v1" {
        if !quarantined && fact.kind != FactKind::SkillVersion {
            if let Some((bytes, dims)) = stored_vec {
                decode_vector_bytes(bytes, dims)?
            } else {
                let v = embedder.embed(&passage)?;
                validate_vector(&v)?;
                v
            }
        } else {
            let v = embedder.embed(&passage)?;
            validate_vector(&v)?;
            v
        }
    } else if let Some((bytes, dims)) = stored_vec {
        decode_vector_bytes(bytes, dims)?
    } else {
        // Real ONNX / HTTP provider embedders never re-embed documents on the fly during queries.
        Vec::new()
    };
    Ok(VisibleDocument {
        id: format!("fact-{}", fact.id),
        library_id: library_id.clone(),
        kind: fact.kind.to_string(),
        heading: fact.subject.clone(),
        vector,
        passage,
        anchor: fact.source_anchor.clone(),
        entities,
        tags: fact.tags.clone(),
    })
}

fn decode_vector_bytes(blob: &[u8], dims: usize) -> Result<Vec<f32>> {
    if blob.len() != dims.saturating_mul(4) {
        return Err(KpError::invalid("embedding", "stored dimensions disagree").into());
    }
    let vec: Vec<f32> = blob
        .as_chunks::<4>()
        .0
        .iter()
        .map(|v| f32::from_le_bytes(*v))
        .collect();
    validate_vector(&vec)?;
    Ok(vec)
}

pub(super) fn decode_or_embed_vector(
    blob: Option<&[u8]>,
    dims: Option<usize>,
    expected_dims: Option<usize>,
    fallback_text: &str,
    embedder: &(impl Embedder + ?Sized),
) -> Result<Vec<f32>> {
    if !embedder.is_enabled() {
        return Ok(Vec::new());
    }
    if let (Some(bytes), Some(d)) = (blob, dims) {
        if expected_dims.is_none_or(|exp| exp == d) {
            return decode_vector_bytes(bytes, d);
        }
    }
    let model_id = embedder.model_id();
    if model_id == "custom" || model_id == "hash:v1" {
        let vec = embedder.embed(fallback_text)?;
        validate_vector(&vec)?;
        Ok(vec)
    } else {
        Ok(Vec::new())
    }
}

pub(super) fn dot_product_le_bytes(query: &[f32], blob: &[u8], dims: usize) -> Result<f64> {
    if blob.len() != dims.saturating_mul(4) || query.len() != dims {
        return Err(KpError::invalid("embedding", "stored dimensions disagree").into());
    }
    let sum = query
        .iter()
        .zip(blob.as_chunks::<4>().0.iter())
        .map(|(q, bytes)| f64::from(*q) * f64::from(f32::from_le_bytes(*bytes)))
        .sum();
    Ok(sum)
}

pub(super) fn dot_product_slice(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() {
        return 0.0;
    }
    a.iter()
        .zip(b)
        .map(|(x, y)| f64::from(*x) * f64::from(*y))
        .sum()
}

pub(super) fn row_str<'r>(row: &'r rusqlite::Row<'_>, idx: usize) -> Result<&'r str> {
    let val = row.get_ref(idx)?;
    val.as_str().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(idx, val.data_type(), Box::new(e)).into()
    })
}

pub(super) fn row_blob_opt<'r>(row: &'r rusqlite::Row<'_>, idx: usize) -> Result<Option<&'r [u8]>> {
    let val = row.get_ref(idx)?;
    val.as_blob_or_null().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(idx, val.data_type(), Box::new(e)).into()
    })
}
