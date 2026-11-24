mod candidate;
mod render;
mod semiring_filter;

pub use render::strip_turn_tags_and_assistant_lines;
pub(crate) use render::{
    lookup_scope_label, render_fact_claim_text, render_premise_human_quote,
    resolve_fact_source_label,
};
pub(crate) use semiring_filter::filter_visible_fact_ids_in_tx;

use std::collections::{BTreeMap, BTreeSet};

use kp_model::{
    ActorId, AnchorSpec, CommunityConcept, ConceptId, EpochMs, Fact, FactAccess, FactId, FactKind,
    FactStatus, LibraryId, MemberRole,
};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::embedding::sanitize_and_validate_embedding;
use crate::facts::read_graph;
use crate::{validate_vector, CodeLocus, Embedder, Result, Store};
use render::resolve_author_ambiguous_question;
use semiring_filter::{
    decode_or_embed_vector, load_fact_code_loci, load_linked_concepts_for_guard,
    materialize_fact_doc_with_entities, row_str, ReverseSemiringChecker,
};

/// Embeds `query` for retrieval:
/// - Returns `Ok(None)` when `!embedder.is_enabled()` (`none` or unloaded local ONNX);
/// - Propagates validation errors on `"custom"` unit-test embedders (`FixedVector` etc.);
/// - Degrades gracefully to `Ok(None)` (FTS5 fallback, zero error) if a real provider / ONNX
///   embedder times out, fails over HTTP, or returns a degenerate vector at query time.
pub(crate) fn embed_query_with_fts_fallback(
    query: &str,
    embedder: &(impl Embedder + ?Sized),
) -> Result<Option<Vec<f32>>> {
    if !embedder.is_enabled() {
        return Ok(None);
    }
    let model_id = embedder.model_id();
    if model_id == "custom" || model_id == "hash:v1" {
        let vec = embedder.embed(query)?;
        validate_vector(&vec)?;
        return Ok(Some(vec));
    }
    if query.trim().is_empty() {
        return Ok(None);
    }
    match embedder.embed(query) {
        Ok(raw) => Ok(sanitize_and_validate_embedding(&raw).ok()),
        Err(_) => Ok(None),
    }
}

/// Only Store can construct the snapshot used by ranking and judge calls.
pub struct RetrievalSnapshot {
    pub(crate) documents: Vec<VisibleDocument>,
}

impl RetrievalSnapshot {
    pub fn documents(&self) -> &[VisibleDocument] {
        &self.documents
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct VisibleDocument {
    pub id: String,
    pub library_id: LibraryId,
    pub kind: String,
    pub heading: String,
    pub passage: String,
    pub anchor: Option<AnchorSpec>,
    pub entities: BTreeSet<String>,
    pub tags: BTreeSet<String>,
    #[serde(skip)]
    pub vector: Vec<f32>,
}

/// One viewer-visible supporting premise with verified `StrongRef` and clean human quote/claim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptPremise {
    pub fact_id: FactId,
    pub recorded_at: EpochMs,
    pub speaker: ActorId,
    pub source_label: String,
    pub claim_or_quote: String,
    #[serde(default)]
    pub is_untrusted: bool,
}

/// Lightweight per-candidate provenance & serendipitous review metadata fetched by O(1) PK lookups.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PromptDocMeta {
    pub doc_id: String,
    pub ref_id: String,
    pub kind: Option<FactKind>,
    pub recorded_at: EpochMs,
    pub speaker: ActorId,
    pub fact_author: Option<ActorId>,
    pub source_label: String,
    pub claim_text: String,
    #[serde(default)]
    pub is_untrusted: bool,
    pub pending_ambiguous_question: Option<String>,
    #[serde(default)]
    pub supporting_premises: Vec<PromptPremise>,
}

/// Viewer-visible supporting premise resolved with verified `StrongRef` for a `CollisionWarning` guard.
#[derive(Clone, Debug, Serialize)]
pub struct VisibleGuardPremise {
    pub fact: Fact,
    pub source_label: String,
    pub code_loci: Vec<CodeLocus>,
    pub pending_ambiguous_question: Option<String>,
}

/// Pre-resolved viewer-visible guard candidate returned by [`Store::visible_guard_candidates`].
#[derive(Clone, Debug, Serialize)]
pub struct VisibleGuardCandidate {
    pub fact: Fact,
    pub source_label: String,
    pub author_role: Option<MemberRole>,
    pub premises: Vec<VisibleGuardPremise>,
    pub linked_concepts: Vec<CommunityConcept>,
    pub code_loci: Vec<CodeLocus>,
    pub pending_ambiguous_question: Option<String>,
}

impl Store {
    /// Unified retrieval entry point: routes non-`deep` queries (both Personal and Community)
    /// through fast candidate-bounded reverse-DFS retrieval (`facts_fts`/`chunks_fts` + stored
    /// vectors, strictly preserving viewer non-interference), retaining full-library snapshots
    /// for explicit `--deep` multi-hop queries.
    pub fn search_snapshot(
        &self,
        viewer: &ActorId,
        at: EpochMs,
        query: &str,
        deep: bool,
        candidate_limit: usize,
    ) -> Result<RetrievalSnapshot> {
        self.search_snapshot_with_embedder(
            viewer,
            at,
            query,
            deep,
            candidate_limit,
            self.embedder.as_ref(),
        )
    }

    pub fn search_snapshot_with_embedder(
        &self,
        viewer: &ActorId,
        at: EpochMs,
        query: &str,
        deep: bool,
        candidate_limit: usize,
        embedder: &(impl Embedder + ?Sized),
    ) -> Result<RetrievalSnapshot> {
        if !deep {
            self.candidate_retrieval_snapshot_internal(
                viewer,
                at,
                query,
                candidate_limit,
                self.mode == kp_model::LibraryMode::Personal,
                embedder,
            )
        } else {
            self.retrieval_snapshot_with_embedder(viewer, at, embedder)
        }
    }

    /// Evaluates semiring + `source_scopes` visibility via reverse-DFS only for `candidate_ids`.
    pub fn filter_visible_fact_ids(
        &self,
        viewer: &ActorId,
        at: EpochMs,
        candidate_ids: &[FactId],
    ) -> Result<BTreeSet<FactId>> {
        if candidate_ids.is_empty() {
            return Ok(BTreeSet::new());
        }
        let tx = self.db.unchecked_transaction()?;
        let visible = filter_visible_fact_ids_in_tx(&tx, viewer, at, candidate_ids)?;
        tx.commit()?;
        Ok(visible)
    }

    /// Returns visible facts matching `kinds` via candidate-bounded reverse-DFS without scanning unrelated kinds.
    pub fn visible_facts_by_kinds(
        &self,
        viewer: &ActorId,
        at: EpochMs,
        kinds: &[FactKind],
    ) -> Result<Vec<Fact>> {
        if kinds.is_empty() {
            return Ok(Vec::new());
        }
        {
            let mut exists_stmt = self.db.prepare_cached(
                "SELECT EXISTS(
                    SELECT 1 FROM facts
                    WHERE kind = ?1 AND status = 'active' AND expired_at IS NULL
                )",
            )?;
            let mut any_exists = false;
            for kind in kinds {
                if exists_stmt.query_row([kind.as_str()], |r| r.get::<_, bool>(0))? {
                    any_exists = true;
                    break;
                }
            }
            if !any_exists {
                return Ok(Vec::new());
            }
        }
        let tx = self.db.unchecked_transaction()?;
        let mut checker = ReverseSemiringChecker::new(&tx, viewer, at)?;
        let mut matched_ids = BTreeSet::<FactId>::new();
        {
            let mut stmt = tx.prepare_cached(
                "SELECT fact_id FROM facts
                 WHERE kind = ?1 AND status = 'active' AND expired_at IS NULL
                 ORDER BY fact_id",
            )?;
            for kind in kinds {
                let mut rows = stmt.query([kind.as_str()])?;
                while let Some(row) = rows.next()? {
                    let fid: FactId = row.get::<_, String>(0)?.parse()?;
                    if checker.is_fact_visible(&fid)? {
                        matched_ids.insert(fid);
                    }
                }
            }
        }
        let mut facts = Vec::with_capacity(matched_ids.len());
        for fid in matched_ids {
            if let Some(fact) = checker.load_fact(&fid)? {
                facts.push(fact);
            }
        }
        tx.commit()?;
        Ok(facts)
    }

    /// Loads viewer-visible guard candidates (`constraint | decision | closed_negative | collision_warning`)
    /// with upfront `SELECT EXISTS` fast exit (`<0.1ms` when none exist), `ReverseSemiringChecker` visibility,
    /// `StrongRef`-verified visible premises for `CollisionWarning`, resolved `source_label`, `linked_concepts`,
    /// `code_loci`, and `author_role`.
    pub fn visible_guard_candidates(
        &self,
        viewer: &ActorId,
        at: EpochMs,
    ) -> Result<Vec<VisibleGuardCandidate>> {
        let has_guards: bool = self
            .db
            .prepare_cached(
                "SELECT EXISTS(
                    SELECT 1 FROM facts
                    WHERE kind IN ('constraint', 'decision', 'closed_negative', 'collision_warning')
                      AND status = 'active'
                      AND expired_at IS NULL
                )",
            )?
            .query_row([], |r| r.get(0))?;
        if !has_guards {
            return Ok(Vec::new());
        }

        let tx = self.db.unchecked_transaction()?;
        let mut checker = ReverseSemiringChecker::new(&tx, viewer, at)?;
        if !checker.members.contains(viewer) {
            tx.commit()?;
            return Ok(Vec::new());
        }

        let mut visible_ids = Vec::<FactId>::new();
        {
            let mut stmt = tx.prepare_cached(
                "SELECT fact_id FROM facts
                 WHERE kind IN ('constraint', 'decision', 'closed_negative', 'collision_warning')
                   AND status = 'active'
                   AND expired_at IS NULL
                 ORDER BY fact_id",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let fid: FactId = row_str(row, 0)?.parse()?;
                if checker.is_fact_visible(&fid)? {
                    visible_ids.push(fid);
                }
            }
        }
        if visible_ids.is_empty() {
            tx.commit()?;
            return Ok(Vec::new());
        }

        let member_roles: BTreeMap<ActorId, MemberRole> = {
            let mut stmt = tx.prepare_cached(
                "SELECT actor_id, role FROM members_tokens WHERE revoked_at IS NULL",
            )?;
            let mut rows = stmt.query([])?;
            let mut map = BTreeMap::new();
            while let Some(row) = rows.next()? {
                let actor: ActorId = row_str(row, 0)?.parse()?;
                let role: MemberRole = row_str(row, 1)?.parse()?;
                map.insert(actor, role);
            }
            map
        };

        let has_concepts: bool = tx
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM community_concepts WHERE status = 'active')",
            )?
            .query_row([], |r| r.get(0))?;
        let known_concepts: BTreeMap<ConceptId, CommunityConcept> = if has_concepts {
            let mut stmt = tx.prepare_cached(
                "SELECT concept_id, concept_kind, canonical_label, domain, polarity, proposed_by, created_at
                 FROM community_concepts
                 WHERE status = 'active'
                 ORDER BY concept_id",
            )?;
            let mut rows = stmt.query([])?;
            let mut map = BTreeMap::new();
            while let Some(row) = rows.next()? {
                let concept = CommunityConcept {
                    id: row_str(row, 0)?.parse()?,
                    kind: row_str(row, 1)?.parse()?,
                    canonical_label: row.get(2)?,
                    domain: row.get(3)?,
                    polarity: row_str(row, 4)?.parse()?,
                    proposed_by: row_str(row, 5)?.parse()?,
                    created_at: EpochMs(row.get(6)?),
                };
                map.insert(concept.id.clone(), concept);
            }
            map
        } else {
            BTreeMap::new()
        };

        let has_code_anchors: bool = tx
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM code_anchors WHERE repo_root IS NOT NULL)",
            )?
            .query_row([], |r| r.get(0))?;

        let mut candidates = Vec::with_capacity(visible_ids.len());
        for fid in visible_ids {
            let Some(fact) = checker.load_fact(&fid)? else {
                continue;
            };
            let source_label = resolve_fact_source_label(&tx, &self.library_id, &fact)?;
            let author_role = member_roles.get(&fact.author).copied();
            let pending_ambiguous_question = if fact.author == *viewer {
                resolve_author_ambiguous_question(&tx, viewer, &fact)?
            } else {
                None
            };

            let mut premises = Vec::new();
            if fact.kind == FactKind::CollisionWarning {
                let prem_ids = checker.visible_supported_premise_ids(&fact.id)?;
                for pid in prem_ids {
                    if let Some(pfact) = checker.load_fact(&pid)? {
                        let p_source = resolve_fact_source_label(&tx, &self.library_id, &pfact)?;
                        let p_loci = if has_code_anchors {
                            load_fact_code_loci(&tx, &pfact.id)?
                        } else {
                            Vec::new()
                        };
                        let p_question = if pfact.author == *viewer {
                            resolve_author_ambiguous_question(&tx, viewer, &pfact)?
                        } else {
                            None
                        };
                        premises.push(VisibleGuardPremise {
                            fact: pfact,
                            source_label: p_source,
                            code_loci: p_loci,
                            pending_ambiguous_question: p_question,
                        });
                    }
                }
            }

            let linked_concepts = if known_concepts.is_empty() {
                Vec::new()
            } else {
                load_linked_concepts_for_guard(&tx, &fact, &premises, &known_concepts)?
            };

            let code_loci = if has_code_anchors {
                load_fact_code_loci(&tx, &fact.id)?
            } else {
                Vec::new()
            };

            candidates.push(VisibleGuardCandidate {
                fact,
                source_label,
                author_role,
                premises,
                linked_concepts,
                code_loci,
                pending_ambiguous_question,
            });
        }

        tx.commit()?;
        Ok(candidates)
    }

    /// A single read transaction gates facts, proofs, chunks, vectors, and entity vocabulary.
    /// Superseded base rows are projected as active only within their historical valid interval.
    /// Withdrawn facts remain unavailable to retrieval; their original stamps remain in the audit graph.
    pub fn retrieval_snapshot(&self, viewer: &ActorId, at: EpochMs) -> Result<RetrievalSnapshot> {
        self.retrieval_snapshot_with_embedder(viewer, at, self.embedder.as_ref())
    }

    pub fn retrieval_snapshot_with_embedder(
        &self,
        viewer: &ActorId,
        at: EpochMs,
        embedder: &(impl Embedder + ?Sized),
    ) -> Result<RetrievalSnapshot> {
        let model_id = embedder.model_id();
        let is_custom = model_id == "custom";
        let custom_probe_dims = if is_custom {
            let probe = embedder.embed("kp_embedder_identity_probe")?;
            validate_vector(&probe)?;
            Some(probe.len())
        } else {
            embedder.expected_dims()
        };
        let use_stored_vectors = !is_custom && embedder.is_enabled();

        let tx = self.db.unchecked_transaction()?;
        let mut graph = read_graph(&tx)?;
        let historical: BTreeSet<_> = graph
            .facts
            .values()
            .filter(|fact| {
                fact.status == FactStatus::Superseded
                    && fact.access.expired_at().is_none()
                    && matches!(&fact.access, FactAccess::Base { stamp, .. } if stamp.valid.contains(at))
                    && fact
                        .superseded_by
                        .as_ref()
                        .and_then(|id| graph.facts.get(id))
                        .is_some_and(|next| {
                            matches!(&next.access, FactAccess::Base { stamp, .. } if at < stamp.valid.valid_from)
                        })
            })
            .map(|f| f.id.clone())
            .collect();
        for id in historical {
            if let Some(fact) = graph.facts.get_mut(&id) {
                fact.status = FactStatus::Active;
            }
        }
        let visible = graph.evaluate(viewer, at);

        let mut entity_map = BTreeMap::<String, BTreeSet<String>>::new();
        if !visible.facts.is_empty() {
            let mut stmt = tx.prepare(
                "SELECT fact_id, entity_slug FROM fact_entities ORDER BY fact_id, entity_slug",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let fid: String = row.get(0)?;
                let slug: String = row.get(1)?;
                entity_map.entry(fid).or_default().insert(slug);
            }
        }

        let has_vec_model_id = crate::migrations::has_column(&tx, "fact_vectors", "model_id")?;
        let mut stored_fact_vecs = BTreeMap::<String, (Vec<u8>, usize)>::new();
        if use_stored_vectors && !visible.facts.is_empty() {
            if has_vec_model_id {
                let mut stmt = tx.prepare(
                    "SELECT fact_id, vec_blob, dims FROM fact_vectors WHERE model_id = ?1",
                )?;
                let mut rows = stmt.query([model_id])?;
                while let Some(row) = rows.next()? {
                    let dims: usize = row.get(2)?;
                    if custom_probe_dims.is_none_or(|exp| exp == 0 || exp == dims) {
                        stored_fact_vecs.insert(row.get(0)?, (row.get(1)?, dims));
                    }
                }
            } else if model_id == "hash:v1" || model_id == "custom" {
                let mut stmt = tx.prepare("SELECT fact_id, vec_blob, dims FROM fact_vectors")?;
                let mut rows = stmt.query([])?;
                while let Some(row) = rows.next()? {
                    let dims: usize = row.get(2)?;
                    if custom_probe_dims.is_none_or(|exp| exp == 0 || exp == dims) {
                        stored_fact_vecs.insert(row.get(0)?, (row.get(1)?, dims));
                    }
                }
            }
        }

        let mut documents = Vec::with_capacity(visible.facts.len());
        for id in &visible.facts {
            let Some(fact) = graph.facts.get(id) else {
                continue;
            };
            let stored = stored_fact_vecs
                .get(id.as_str())
                .map(|(b, d)| (b.as_slice(), *d));
            let preloaded_entities = entity_map.get(id.as_str());
            let doc = materialize_fact_doc_with_entities(
                &tx,
                &self.library_id,
                fact,
                preloaded_entities,
                stored,
                embedder,
            )?;
            documents.push(doc);
        }
        {
            let chunk_sql = if has_vec_model_id {
                "SELECT c.chunk_id,c.heading,c.body,c.anchor_json,v.vec_blob,v.dims FROM chunks c
                JOIN raw_sources s ON s.source_id=c.source_id AND s.content_sha256=c.source_sha256
                LEFT JOIN chunk_vectors v ON v.chunk_id=c.chunk_id AND v.model_id=?2
                WHERE s.superseded_by_source IS NULL
                AND NOT EXISTS (SELECT 1 FROM tombstones t WHERE t.tombstone_id=c.source_id)
                AND EXISTS (SELECT 1 FROM json_each(c.readable_by_json) a WHERE a.value=?1 OR
                    (a.value='*' AND EXISTS (SELECT 1 FROM members_tokens m WHERE m.actor_id=?1 AND m.revoked_at IS NULL)))
                AND (s.source_scope_id IS NULL OR EXISTS (
                    SELECT 1 FROM source_scopes sc, json_each(sc.current_audience_json) sa
                    WHERE sc.scope_id=s.source_scope_id AND (sa.value=?1 OR
                        (sa.value='*' AND EXISTS (SELECT 1 FROM members_tokens m WHERE m.actor_id=?1 AND m.revoked_at IS NULL)))
                ))
                ORDER BY c.chunk_id"
            } else {
                "SELECT c.chunk_id,c.heading,c.body,c.anchor_json,
                        CASE WHEN ?2 IN ('hash:v1','custom') THEN v.vec_blob ELSE NULL END,
                        CASE WHEN ?2 IN ('hash:v1','custom') THEN v.dims ELSE NULL END
                FROM chunks c
                JOIN raw_sources s ON s.source_id=c.source_id AND s.content_sha256=c.source_sha256
                LEFT JOIN chunk_vectors v ON v.chunk_id=c.chunk_id
                WHERE s.superseded_by_source IS NULL
                AND NOT EXISTS (SELECT 1 FROM tombstones t WHERE t.tombstone_id=c.source_id)
                AND EXISTS (SELECT 1 FROM json_each(c.readable_by_json) a WHERE a.value=?1 OR
                    (a.value='*' AND EXISTS (SELECT 1 FROM members_tokens m WHERE m.actor_id=?1 AND m.revoked_at IS NULL)))
                AND (s.source_scope_id IS NULL OR EXISTS (
                    SELECT 1 FROM source_scopes sc, json_each(sc.current_audience_json) sa
                    WHERE sc.scope_id=s.source_scope_id AND (sa.value=?1 OR
                        (sa.value='*' AND EXISTS (SELECT 1 FROM members_tokens m WHERE m.actor_id=?1 AND m.revoked_at IS NULL)))
                ))
                ORDER BY c.chunk_id"
            };
            let mut stmt = tx.prepare(chunk_sql)?;
            let mut rows = stmt.query(params![viewer.as_str(), model_id])?;
            while let Some(row) = rows.next()? {
                let heading: String = row.get(1)?;
                let passage: String = row.get(2)?;
                let blob: Option<Vec<u8>> = row.get(4)?;
                let dims: Option<usize> = row.get(5)?;
                let vector = decode_or_embed_vector(
                    if use_stored_vectors {
                        blob.as_deref()
                    } else {
                        None
                    },
                    dims,
                    custom_probe_dims.filter(|&d| d > 0),
                    &format!("{heading}\n{passage}"),
                    embedder,
                )?;
                documents.push(VisibleDocument {
                    id: format!("chunk-{}", row.get::<_, String>(0)?),
                    library_id: self.library_id.clone(),
                    kind: "chunk".into(),
                    heading,
                    passage,
                    anchor: Some(serde_json::from_str(&row.get::<_, String>(3)?)?),
                    vector,
                    entities: BTreeSet::new(),
                    tags: BTreeSet::new(),
                });
            }
        }
        tx.commit()?;
        documents.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(RetrievalSnapshot { documents })
    }
}

#[cfg(test)]
#[path = "../snapshot_tests.rs"]
mod tests;
