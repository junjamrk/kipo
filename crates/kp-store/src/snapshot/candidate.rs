use std::collections::{BTreeMap, BTreeSet};

use kp_model::{ActorId, EpochMs, FactId, FactKind};
use rusqlite::{params, OptionalExtension};

use super::render::{
    collapse_inline, fact_reference_time, lookup_scope_label, premise_display_order,
    render_fact_claim_text, render_premise_human_quote, resolve_author_ambiguous_question,
    resolve_fact_source_label, strip_turn_tags_and_assistant_lines,
};
use super::semiring_filter::{
    decode_or_embed_vector, dot_product_le_bytes, dot_product_slice, materialize_fact_doc,
    materialize_fact_doc_for_candidate, row_blob_opt, row_str, ReverseSemiringChecker,
};
use super::{
    embed_query_with_fts_fallback, PromptDocMeta, PromptPremise, RetrievalSnapshot, VisibleDocument,
};
use crate::{validate_vector, Embedder, Result, Store, Tokenizer};

impl Store {
    /// Candidate-bounded incremental retrieval snapshot for fast Hook / prompt-context paths.
    ///
    /// Queries `facts_fts`, `chunks_fts`, `fact_vectors`, and `chunk_vectors` top-K FIRST and
    /// evaluates semiring visibility via reverse-DFS ONLY for candidate conclusions rather than
    /// loading or re-embedding the full library. Strictly preserves joint `(viewer, at)` semiring
    /// + `source_scopes` noninterference (hidden facts, chunks, and entity DF never affect output).
    pub fn candidate_retrieval_snapshot(
        &self,
        viewer: &ActorId,
        at: EpochMs,
        query: &str,
        candidate_limit: usize,
    ) -> Result<RetrievalSnapshot> {
        self.candidate_retrieval_snapshot_with_embedder(
            viewer,
            at,
            query,
            candidate_limit,
            self.embedder.as_ref(),
        )
    }

    pub fn candidate_retrieval_snapshot_with_embedder(
        &self,
        viewer: &ActorId,
        at: EpochMs,
        query: &str,
        candidate_limit: usize,
        embedder: &(impl Embedder + ?Sized),
    ) -> Result<RetrievalSnapshot> {
        self.candidate_retrieval_snapshot_internal(
            viewer,
            at,
            query,
            candidate_limit,
            true,
            embedder,
        )
    }

    pub(super) fn candidate_retrieval_snapshot_internal(
        &self,
        viewer: &ActorId,
        at: EpochMs,
        query: &str,
        candidate_limit: usize,
        exclude_session_chunks: bool,
        embedder: &(impl Embedder + ?Sized),
    ) -> Result<RetrievalSnapshot> {
        let query_vector = embed_query_with_fts_fallback(query, embedder)?;
        self.candidate_retrieval_snapshot_with_query_vector_internal(
            viewer,
            at,
            query,
            query_vector.as_deref(),
            candidate_limit,
            exclude_session_chunks,
            embedder,
        )
    }

    pub fn candidate_retrieval_snapshot_with_query_vector(
        &self,
        viewer: &ActorId,
        at: EpochMs,
        query: &str,
        query_vector: Option<&[f32]>,
        candidate_limit: usize,
        embedder: &(impl Embedder + ?Sized),
    ) -> Result<RetrievalSnapshot> {
        self.candidate_retrieval_snapshot_with_query_vector_internal(
            viewer,
            at,
            query,
            query_vector,
            candidate_limit,
            true,
            embedder,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn candidate_retrieval_snapshot_with_query_vector_internal(
        &self,
        viewer: &ActorId,
        at: EpochMs,
        query: &str,
        query_vector: Option<&[f32]>,
        candidate_limit: usize,
        exclude_session_chunks: bool,
        embedder: &(impl Embedder + ?Sized),
    ) -> Result<RetrievalSnapshot> {
        if candidate_limit == 0 {
            return Ok(RetrievalSnapshot {
                documents: Vec::new(),
            });
        }
        // Process-wide static base tokenizer (`BASE_JIEBA`) avoids dictionary parsing, heap
        // allocation, and cross-actor entity word leakage.
        let query_terms = Tokenizer::shared().tokenize(query);
        let query_vec_nonzero = query_vector.is_some_and(|v| v.iter().any(|&x| x != 0.0));
        if query_terms.is_empty() && !query_vec_nonzero {
            return Ok(RetrievalSnapshot {
                documents: Vec::new(),
            });
        }

        let tx = self.db.unchecked_transaction()?;
        let mut checker = ReverseSemiringChecker::new(&tx, viewer, at)?;
        let per_channel_k = candidate_limit.max(8).saturating_mul(2);
        let exclude_session_chunks = i64::from(exclude_session_chunks);

        let mut candidate_facts = BTreeSet::<FactId>::new();
        let mut candidate_chunks = BTreeSet::<String>::new();
        let mut fact_term_hits = BTreeMap::<String, u32>::new();
        let mut chunk_term_hits = BTreeMap::<String, u32>::new();

        // Channel 1 & 2: Viewer-noninterfering per-term FTS5 boolean match count (zero global BM25
        // corpus DF leakage, bounded to top `per_channel_k` visible candidates before materializing).
        if !query_terms.is_empty() {
            let mut unique_terms = Vec::<String>::new();
            let mut seen_terms = BTreeSet::<&str>::new();
            for term in &query_terms {
                if seen_terms.insert(term.as_str()) {
                    unique_terms.push(format!("\"{}\"", term.replace('"', "\"\"")));
                    if unique_terms.len() >= 12 {
                        break;
                    }
                }
            }

            {
                let mut stmt = tx.prepare_cached(
                    "SELECT f.fact_id
                     FROM facts_fts
                     JOIN facts f ON f.fts_rowid = facts_fts.rowid
                     WHERE facts_fts MATCH ?1
                       AND f.status IN ('active', 'superseded')
                       AND f.expired_at IS NULL",
                )?;
                for quoted_term in &unique_terms {
                    let mut rows = stmt.query([quoted_term])?;
                    while let Some(row) = rows.next()? {
                        let fid_str = row_str(row, 0)?;
                        *fact_term_hits.entry(fid_str.to_owned()).or_insert(0) += 1;
                    }
                }
            }
            let mut ranked_lex_facts: Vec<(u32, String)> = fact_term_hits
                .iter()
                .map(|(id, &hits)| (hits, id.clone()))
                .collect();
            ranked_lex_facts.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            let mut lex_collected = 0usize;
            for (_, id_str) in ranked_lex_facts {
                if lex_collected >= per_channel_k {
                    break;
                }
                let fid: FactId = id_str.parse()?;
                if checker.is_fact_visible(&fid)? {
                    candidate_facts.insert(fid);
                    lex_collected += 1;
                }
            }

            {
                let mut stmt = tx.prepare_cached(
                    "SELECT c.chunk_id
                     FROM chunks_fts
                     JOIN chunks c ON c.fts_rowid = chunks_fts.rowid
                     JOIN raw_sources s ON s.source_id = c.source_id AND s.content_sha256 = c.source_sha256
                     WHERE chunks_fts MATCH ?1
                       AND s.superseded_by_source IS NULL
                       AND (?3 = 0 OR s.uri NOT LIKE 'session:%')
                       AND NOT EXISTS (SELECT 1 FROM tombstones t WHERE t.tombstone_id = c.source_id)
                       AND EXISTS (
                           SELECT 1 FROM json_each(c.readable_by_json) a
                           WHERE a.value = ?2
                              OR (a.value = '*' AND EXISTS (
                                  SELECT 1 FROM members_tokens m WHERE m.actor_id = ?2 AND m.revoked_at IS NULL
                              ))
                       )
                       AND (s.source_scope_id IS NULL OR EXISTS (
                           SELECT 1 FROM source_scopes sc, json_each(sc.current_audience_json) sa
                           WHERE sc.scope_id = s.source_scope_id
                             AND (sa.value = ?2
                                  OR (sa.value = '*' AND EXISTS (
                                      SELECT 1 FROM members_tokens m WHERE m.actor_id = ?2 AND m.revoked_at IS NULL
                                  )))
                       ))",
                )?;
                for quoted_term in &unique_terms {
                    let mut rows = stmt.query(params![
                        quoted_term,
                        viewer.as_str(),
                        exclude_session_chunks
                    ])?;
                    while let Some(row) = rows.next()? {
                        let cid_str = row_str(row, 0)?;
                        *chunk_term_hits.entry(cid_str.to_owned()).or_insert(0) += 1;
                    }
                }
            }
            let mut ranked_lex_chunks: Vec<(u32, String)> = chunk_term_hits
                .iter()
                .map(|(id, &hits)| (hits, id.clone()))
                .collect();
            ranked_lex_chunks.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            for (_, cid) in ranked_lex_chunks.into_iter().take(per_channel_k) {
                candidate_chunks.insert(cid);
            }
        }

        // Channel 3 & 4: Stored `fact_vectors` / `chunk_vectors` top-K pointwise cosine similarity.
        // Whenever `embedder.model_id() != "custom"`, directly scans persisted rows matching
        // `(model_id == embedder.model_id() && dims == query_vector.len())` without ever
        // re-embedding documents on the fly during a query.
        let model_id = embedder.model_id();
        let is_custom_embedder = model_id == "custom";
        let has_vec_model_id = crate::migrations::has_column(&tx, "fact_vectors", "model_id")?;
        let mut fact_vec_sims = BTreeMap::<String, f64>::new();
        let mut chunk_vec_sims = BTreeMap::<String, f64>::new();

        if let Some(qvec) = query_vector.filter(|_| query_vec_nonzero) {
            let mut scored_facts = Vec::<(f64, String)>::new();
            if !is_custom_embedder {
                if has_vec_model_id {
                    let mut stmt = tx.prepare_cached(
                        "SELECT fact_id, dims, vec_blob
                         FROM fact_vectors
                         WHERE model_id = ?1 AND dims = ?2",
                    )?;
                    let mut rows = stmt.query(params![model_id, qvec.len()])?;
                    while let Some(row) = rows.next()? {
                        let dims: usize = row.get(1)?;
                        if let Some(bytes) = row_blob_opt(row, 2)? {
                            let sim = dot_product_le_bytes(qvec, bytes, dims)?;
                            if sim > 0.0 {
                                scored_facts.push((sim, row_str(row, 0)?.to_owned()));
                            }
                        }
                    }
                } else if model_id == "hash:v1" {
                    let mut stmt = tx.prepare_cached(
                        "SELECT fact_id, dims, vec_blob FROM fact_vectors WHERE dims = ?1",
                    )?;
                    let mut rows = stmt.query([qvec.len()])?;
                    while let Some(row) = rows.next()? {
                        let dims: usize = row.get(1)?;
                        if let Some(bytes) = row_blob_opt(row, 2)? {
                            let sim = dot_product_le_bytes(qvec, bytes, dims)?;
                            if sim > 0.0 {
                                scored_facts.push((sim, row_str(row, 0)?.to_owned()));
                            }
                        }
                    }
                }
            } else {
                let mut fallback_ids = Vec::<FactId>::new();
                {
                    let mut stmt = tx.prepare_cached(
                        "SELECT f.fact_id
                         FROM facts f
                         WHERE f.status IN ('active', 'superseded')
                           AND f.expired_at IS NULL
                         ORDER BY f.fact_id",
                    )?;
                    let mut rows = stmt.query([])?;
                    while let Some(row) = rows.next()? {
                        fallback_ids.push(row_str(row, 0)?.parse()?);
                    }
                }
                for fid in fallback_ids {
                    if checker.is_fact_visible(&fid)? {
                        if let Some(fact) = checker.cached_fact(&fid) {
                            let doc =
                                materialize_fact_doc(&tx, &self.library_id, fact, None, embedder)?;
                            let sim = dot_product_slice(qvec, &doc.vector);
                            if sim > 0.0 {
                                scored_facts.push((sim, fid.to_string()));
                            }
                        }
                    }
                }
            }
            scored_facts.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            let mut collected = 0usize;
            for (sim, id_str) in scored_facts {
                if collected >= per_channel_k {
                    break;
                }
                let fid: FactId = id_str.parse()?;
                if checker.is_fact_visible(&fid)? {
                    fact_vec_sims.insert(id_str, sim);
                    candidate_facts.insert(fid);
                    collected += 1;
                }
            }

            if !is_custom_embedder {
                let mut scored_chunks = Vec::<(f64, String)>::new();
                if has_vec_model_id {
                    let mut stmt = tx.prepare_cached(
                        "SELECT chunk_id, dims, vec_blob
                         FROM chunk_vectors
                         WHERE model_id = ?1 AND dims = ?2",
                    )?;
                    let mut rows = stmt.query(params![model_id, qvec.len()])?;
                    while let Some(row) = rows.next()? {
                        let dims: usize = row.get(1)?;
                        if let Some(bytes) = row_blob_opt(row, 2)? {
                            let sim = dot_product_le_bytes(qvec, bytes, dims)?;
                            if sim > 0.0 {
                                scored_chunks.push((sim, row_str(row, 0)?.to_owned()));
                            }
                        }
                    }
                } else if model_id == "hash:v1" {
                    let mut stmt = tx.prepare_cached(
                        "SELECT chunk_id, dims, vec_blob FROM chunk_vectors WHERE dims = ?1",
                    )?;
                    let mut rows = stmt.query([qvec.len()])?;
                    while let Some(row) = rows.next()? {
                        let dims: usize = row.get(1)?;
                        if let Some(bytes) = row_blob_opt(row, 2)? {
                            let sim = dot_product_le_bytes(qvec, bytes, dims)?;
                            if sim > 0.0 {
                                scored_chunks.push((sim, row_str(row, 0)?.to_owned()));
                            }
                        }
                    }
                }
                scored_chunks.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
                let mut auth_stmt = tx.prepare_cached(
                    "SELECT 1
                     FROM chunks c
                     JOIN raw_sources s ON s.source_id = c.source_id AND s.content_sha256 = c.source_sha256
                     WHERE c.chunk_id = ?1
                       AND s.superseded_by_source IS NULL
                       AND (?3 = 0 OR s.uri NOT LIKE 'session:%')
                       AND NOT EXISTS (SELECT 1 FROM tombstones t WHERE t.tombstone_id = c.source_id)
                       AND EXISTS (
                           SELECT 1 FROM json_each(c.readable_by_json) a
                           WHERE a.value = ?2
                              OR (a.value = '*' AND EXISTS (
                                  SELECT 1 FROM members_tokens m WHERE m.actor_id = ?2 AND m.revoked_at IS NULL
                              ))
                       )
                       AND (s.source_scope_id IS NULL OR EXISTS (
                           SELECT 1 FROM source_scopes sc, json_each(sc.current_audience_json) sa
                           WHERE sc.scope_id = s.source_scope_id
                             AND (sa.value = ?2
                                  OR (sa.value = '*' AND EXISTS (
                                      SELECT 1 FROM members_tokens m WHERE m.actor_id = ?2 AND m.revoked_at IS NULL
                                  )))
                       ))",
                )?;
                let mut chunk_collected = 0usize;
                for (sim, chunk_id) in scored_chunks {
                    if chunk_collected >= per_channel_k {
                        break;
                    }
                    let allowed = auth_stmt
                        .query_row(
                            params![chunk_id.as_str(), viewer.as_str(), exclude_session_chunks],
                            |_| Ok(true),
                        )
                        .optional()?
                        .unwrap_or(false);
                    if allowed {
                        chunk_vec_sims.insert(chunk_id.clone(), sim);
                        candidate_chunks.insert(chunk_id);
                        chunk_collected += 1;
                    }
                }
            } else {
                let mut scored_chunks = Vec::<(f64, String)>::new();
                {
                    let mut stmt = tx.prepare_cached(
                        "SELECT c.chunk_id, c.heading, c.body
                         FROM chunks c
                         JOIN raw_sources s ON s.source_id = c.source_id AND s.content_sha256 = c.source_sha256
                         WHERE s.superseded_by_source IS NULL
                           AND (?2 = 0 OR s.uri NOT LIKE 'session:%')
                           AND NOT EXISTS (SELECT 1 FROM tombstones t WHERE t.tombstone_id = c.source_id)
                           AND EXISTS (
                               SELECT 1 FROM json_each(c.readable_by_json) a
                               WHERE a.value = ?1
                                  OR (a.value = '*' AND EXISTS (
                                      SELECT 1 FROM members_tokens m WHERE m.actor_id = ?1 AND m.revoked_at IS NULL
                                  ))
                           )
                           AND (s.source_scope_id IS NULL OR EXISTS (
                               SELECT 1 FROM source_scopes sc, json_each(sc.current_audience_json) sa
                               WHERE sc.scope_id = s.source_scope_id
                                 AND (sa.value = ?1
                                      OR (sa.value = '*' AND EXISTS (
                                          SELECT 1 FROM members_tokens m WHERE m.actor_id = ?1 AND m.revoked_at IS NULL
                                      )))
                           ))
                         ORDER BY c.chunk_id",
                    )?;
                    let mut rows = stmt.query(params![viewer.as_str(), exclude_session_chunks])?;
                    while let Some(row) = rows.next()? {
                        let chunk_ref = row_str(row, 0)?;
                        let heading = row_str(row, 1)?;
                        let body = row_str(row, 2)?;
                        let vec = embedder.embed(&format!("{heading}\n{body}"))?;
                        validate_vector(&vec)?;
                        let sim = dot_product_slice(qvec, &vec);
                        if sim > 0.0 {
                            scored_chunks.push((sim, chunk_ref.to_owned()));
                        }
                    }
                }
                scored_chunks.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
                for (sim, chunk_id) in scored_chunks.into_iter().take(per_channel_k) {
                    chunk_vec_sims.insert(chunk_id.clone(), sim);
                    candidate_chunks.insert(chunk_id);
                }
            }
        }

        // Bound to `candidate_limit` BEFORE materializing documents using pointwise FTS hit counts
        // + vector similarities (zero `ScopedIndex::new`, zero `Jieba` re-tokenization, 100% viewer noninterfering).
        if candidate_facts.len() + candidate_chunks.len() > candidate_limit {
            let mut entries = Vec::<PreMaterializedCandidate>::with_capacity(
                candidate_facts.len() + candidate_chunks.len(),
            );
            for fid in &candidate_facts {
                let lex_hits = fact_term_hits.get(fid.as_str()).copied().unwrap_or(0);
                let vec_sim = if let Some(&s) = fact_vec_sims.get(fid.as_str()) {
                    s
                } else if let (Some(qvec), false) = (
                    query_vector.as_ref().filter(|_| query_vec_nonzero),
                    is_custom_embedder,
                ) {
                    let stored: Option<(Vec<u8>, usize)> = if has_vec_model_id {
                        tx.prepare_cached(
                            "SELECT vec_blob, dims FROM fact_vectors
                             WHERE fact_id = ?1 AND model_id = ?2 AND dims = ?3",
                        )?
                        .query_row(params![fid.as_str(), model_id, qvec.len()], |r| {
                            Ok((r.get(0)?, r.get(1)?))
                        })
                        .optional()?
                    } else if model_id == "hash:v1" {
                        tx.prepare_cached(
                            "SELECT vec_blob, dims FROM fact_vectors WHERE fact_id = ?1 AND dims = ?2",
                        )?
                        .query_row(params![fid.as_str(), qvec.len()], |r| {
                            Ok((r.get(0)?, r.get(1)?))
                        })
                        .optional()?
                    } else {
                        None
                    };
                    match stored {
                        Some((blob, dims)) => dot_product_le_bytes(qvec, &blob, dims)?.max(0.0),
                        None => 0.0,
                    }
                } else {
                    0.0
                };
                entries.push(PreMaterializedCandidate {
                    doc_id: format!("fact-{fid}"),
                    kind: CandidateTarget::Fact(fid.clone()),
                    lex_hits,
                    vec_sim,
                });
            }
            for cid in &candidate_chunks {
                let lex_hits = chunk_term_hits.get(cid.as_str()).copied().unwrap_or(0);
                let vec_sim = if let Some(&s) = chunk_vec_sims.get(cid.as_str()) {
                    s
                } else if let (Some(qvec), false) = (
                    query_vector.as_ref().filter(|_| query_vec_nonzero),
                    is_custom_embedder,
                ) {
                    let stored: Option<(Vec<u8>, usize)> = if has_vec_model_id {
                        tx.prepare_cached(
                            "SELECT vec_blob, dims FROM chunk_vectors
                             WHERE chunk_id = ?1 AND model_id = ?2 AND dims = ?3",
                        )?
                        .query_row(params![cid.as_str(), model_id, qvec.len()], |r| {
                            Ok((r.get(0)?, r.get(1)?))
                        })
                        .optional()?
                    } else if model_id == "hash:v1" {
                        tx.prepare_cached(
                            "SELECT vec_blob, dims FROM chunk_vectors WHERE chunk_id = ?1 AND dims = ?2",
                        )?
                        .query_row(params![cid.as_str(), qvec.len()], |r| {
                            Ok((r.get(0)?, r.get(1)?))
                        })
                        .optional()?
                    } else {
                        None
                    };
                    match stored {
                        Some((blob, dims)) => dot_product_le_bytes(qvec, &blob, dims)?.max(0.0),
                        None => 0.0,
                    }
                } else {
                    0.0
                };
                entries.push(PreMaterializedCandidate {
                    doc_id: format!("chunk-{cid}"),
                    kind: CandidateTarget::Chunk(cid.clone()),
                    lex_hits,
                    vec_sim,
                });
            }

            let mut rrf = vec![0.0_f64; entries.len()];
            let mut lex_order: Vec<usize> = (0..entries.len())
                .filter(|&i| entries[i].lex_hits > 0)
                .collect();
            lex_order.sort_by(|&a, &b| {
                entries[b]
                    .lex_hits
                    .cmp(&entries[a].lex_hits)
                    .then_with(|| entries[a].doc_id.cmp(&entries[b].doc_id))
            });
            for (rank, idx) in lex_order.into_iter().enumerate() {
                rrf[idx] += 1.0 / (60.0 + (rank + 1) as f64);
            }

            let mut vec_order: Vec<usize> = (0..entries.len())
                .filter(|&i| entries[i].vec_sim > 0.0)
                .collect();
            vec_order.sort_by(|&a, &b| {
                entries[b]
                    .vec_sim
                    .total_cmp(&entries[a].vec_sim)
                    .then_with(|| entries[a].doc_id.cmp(&entries[b].doc_id))
            });
            for (rank, idx) in vec_order.into_iter().enumerate() {
                rrf[idx] += 1.0 / (60.0 + (rank + 1) as f64);
            }

            let mut order: Vec<usize> = (0..entries.len()).collect();
            order.sort_by(|&a, &b| {
                rrf[b]
                    .total_cmp(&rrf[a])
                    .then_with(|| entries[a].doc_id.cmp(&entries[b].doc_id))
            });
            order.truncate(candidate_limit);

            candidate_facts.clear();
            candidate_chunks.clear();
            for idx in order {
                match &entries[idx].kind {
                    CandidateTarget::Fact(fid) => {
                        candidate_facts.insert(fid.clone());
                    }
                    CandidateTarget::Chunk(cid) => {
                        candidate_chunks.insert(cid.clone());
                    }
                }
            }
        }

        let target_qdims = query_vector.map(<[f32]>::len);
        let mut documents = Vec::with_capacity(candidate_facts.len() + candidate_chunks.len());
        {
            let fv_sql = if has_vec_model_id {
                "SELECT vec_blob, dims FROM fact_vectors WHERE fact_id = ?1 AND model_id = ?2"
            } else {
                "SELECT vec_blob, dims FROM fact_vectors WHERE fact_id = ?1 AND ?2 IN ('hash:v1', 'custom')"
            };
            let mut fv_stmt = tx.prepare_cached(fv_sql)?;
            for fid in &candidate_facts {
                let Some(fact) = checker.load_fact(fid)? else {
                    continue;
                };
                let precomputed = match (is_custom_embedder, target_qdims) {
                    (false, Some(qdims)) if qdims > 0 => {
                        let stored_vec: Option<(Vec<u8>, usize)> = fv_stmt
                            .query_row(params![fid.as_str(), model_id], |row| {
                                Ok((row.get(0)?, row.get(1)?))
                            })
                            .optional()?;
                        stored_vec.filter(|(_, dims)| *dims == qdims)
                    }
                    _ => None,
                };
                documents.push(materialize_fact_doc_for_candidate(
                    &tx,
                    &self.library_id,
                    &fact,
                    precomputed.as_ref().map(|(b, d)| (b.as_slice(), *d)),
                    target_qdims.is_some(),
                    embedder,
                )?);
            }
        }

        {
            let chunk_doc_sql = if has_vec_model_id {
                "SELECT c.heading, c.body, c.anchor_json, v.vec_blob, v.dims
                 FROM chunks c
                 LEFT JOIN chunk_vectors v ON v.chunk_id = c.chunk_id AND v.model_id = ?2
                 WHERE c.chunk_id = ?1"
            } else {
                "SELECT c.heading, c.body, c.anchor_json,
                        CASE WHEN ?2 IN ('hash:v1', 'custom') THEN v.vec_blob ELSE NULL END,
                        CASE WHEN ?2 IN ('hash:v1', 'custom') THEN v.dims ELSE NULL END
                 FROM chunks c
                 LEFT JOIN chunk_vectors v ON v.chunk_id = c.chunk_id
                 WHERE c.chunk_id = ?1"
            };
            let mut chunk_stmt = tx.prepare_cached(chunk_doc_sql)?;
            for chunk_id in &candidate_chunks {
                let row: Option<ChunkDocRow> = chunk_stmt
                    .query_row(params![chunk_id.as_str(), model_id], |r| {
                        Ok(ChunkDocRow {
                            heading: r.get(0)?,
                            passage: r.get(1)?,
                            anchor_json: r.get(2)?,
                            blob: r.get(3)?,
                            dims: r.get(4)?,
                        })
                    })
                    .optional()?;
                let Some(row) = row else {
                    continue;
                };
                let vector = match (is_custom_embedder, target_qdims) {
                    (_, None) => Vec::new(),
                    (false, Some(qdims)) => decode_or_embed_vector(
                        row.blob.as_deref(),
                        row.dims,
                        Some(qdims),
                        &format!("{}\n{}", row.heading, row.passage),
                        embedder,
                    )?,
                    (true, Some(qdims)) => decode_or_embed_vector(
                        None,
                        None,
                        Some(qdims),
                        &format!("{}\n{}", row.heading, row.passage),
                        embedder,
                    )?,
                };
                documents.push(VisibleDocument {
                    id: format!("chunk-{chunk_id}"),
                    library_id: self.library_id.clone(),
                    kind: "chunk".into(),
                    heading: row.heading,
                    passage: row.passage,
                    anchor: Some(serde_json::from_str(&row.anchor_json)?),
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

    /// Finds active viewer-visible `CollisionWarning` conclusions supported by any of `premise_ids`,
    /// verified strictly via `ReverseSemiringChecker` for `(viewer, at)`.
    pub fn visible_collision_warnings_for_premises(
        &self,
        viewer: &ActorId,
        at: EpochMs,
        premise_ids: &[FactId],
    ) -> Result<Vec<FactId>> {
        if premise_ids.is_empty() {
            return Ok(Vec::new());
        }
        let has_warnings: bool = self
            .db
            .prepare_cached(
                "SELECT EXISTS(
                    SELECT 1 FROM facts
                    WHERE kind = 'collision_warning' AND status = 'active' AND expired_at IS NULL
                )",
            )?
            .query_row([], |r| r.get(0))?;
        if !has_warnings {
            return Ok(Vec::new());
        }

        let tx = self.db.unchecked_transaction()?;
        let mut candidates = BTreeSet::<FactId>::new();
        {
            let mut stmt = tx.prepare_cached(
                "SELECT DISTINCT j.conclusion_fact_id
                 FROM justification_premises jp
                 JOIN justifications j ON j.just_id = jp.just_id
                 JOIN facts cf ON cf.fact_id = j.conclusion_fact_id
                 WHERE jp.premise_fact_id = ?1
                   AND j.active = 1
                   AND j.invalidated_at IS NULL
                   AND cf.kind = 'collision_warning'
                   AND cf.status = 'active'
                   AND cf.expired_at IS NULL
                 ORDER BY j.conclusion_fact_id",
            )?;
            for pid in premise_ids {
                let mut rows = stmt.query([pid.as_str()])?;
                while let Some(row) = rows.next()? {
                    candidates.insert(row_str(row, 0)?.parse()?);
                }
            }
        }
        if candidates.is_empty() {
            tx.commit()?;
            return Ok(Vec::new());
        }

        let mut checker = ReverseSemiringChecker::new(&tx, viewer, at)?;
        let mut visible = Vec::new();
        for wid in candidates {
            if checker.is_fact_visible(&wid)? {
                visible.push(wid);
            }
        }
        tx.commit()?;
        Ok(visible)
    }

    /// Resolves O(1) indexed provenance and serendipitous review metadata for visible document IDs
    /// (`"fact-<id>"` or `"chunk-<id>"`) without loading the full graph.
    pub fn prompt_candidate_meta(
        &self,
        viewer: &ActorId,
        at: EpochMs,
        doc_ids: &[String],
    ) -> Result<BTreeMap<String, PromptDocMeta>> {
        if doc_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let tx = self.db.unchecked_transaction()?;
        let mut checker = ReverseSemiringChecker::new(&tx, viewer, at)?;
        let owner_actor = self.owner_actor.clone();

        let mut out = BTreeMap::new();
        for doc_id in doc_ids {
            if let Some(raw_fact_id) = doc_id.strip_prefix("fact-") {
                let Ok(fid) = raw_fact_id.parse::<FactId>() else {
                    continue;
                };
                if !checker.is_fact_visible(&fid)? {
                    continue;
                };
                let Some(fact) = checker.load_fact(&fid)? else {
                    continue;
                };
                let source_label = resolve_fact_source_label(&tx, &self.library_id, &fact)?;
                let claim_text = render_fact_claim_text(&tx, &fact)?;
                if claim_text.trim().is_empty() {
                    continue;
                }
                let pending_ambiguous_question = if fact.author == *viewer {
                    resolve_author_ambiguous_question(&tx, viewer, &fact)?
                } else {
                    None
                };

                let mut supporting_premises = Vec::new();
                if matches!(
                    fact.kind,
                    FactKind::CollisionWarning | FactKind::DerivedFact | FactKind::Synthesis
                ) {
                    let prem_ids = checker.visible_supported_premise_ids(&fact.id)?;
                    let mut keyed_premises = Vec::with_capacity(prem_ids.len());
                    for pid in prem_ids {
                        if let Some(pfact) = checker.load_fact(&pid)? {
                            let claim_or_quote = render_premise_human_quote(&tx, &pfact)?;
                            if claim_or_quote.trim().is_empty() {
                                continue;
                            }
                            let p_source =
                                resolve_fact_source_label(&tx, &self.library_id, &pfact)?;
                            keyed_premises.push((
                                premise_display_order(pfact.kind),
                                PromptPremise {
                                    fact_id: pfact.id.clone(),
                                    recorded_at: fact_reference_time(&pfact, at),
                                    speaker: pfact.author.clone(),
                                    source_label: p_source,
                                    claim_or_quote,
                                },
                            ));
                        }
                    }
                    keyed_premises
                        .sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.fact_id.cmp(&b.1.fact_id)));
                    supporting_premises = keyed_premises.into_iter().map(|(_, p)| p).collect();
                }

                out.insert(
                    doc_id.clone(),
                    PromptDocMeta {
                        doc_id: doc_id.clone(),
                        ref_id: fact.id.to_string(),
                        kind: Some(fact.kind),
                        recorded_at: fact_reference_time(&fact, at),
                        speaker: fact.author.clone(),
                        fact_author: Some(fact.author),
                        source_label,
                        claim_text,
                        pending_ambiguous_question,
                        supporting_premises,
                    },
                );
            } else if let Some(chunk_id) = doc_id.strip_prefix("chunk-") {
                let row: Option<ChunkMetaRow> = tx
                    .query_row(
                        "SELECT c.heading, c.body, c.created_at, s.uri, s.ingested_by_actor, s.source_scope_id
                         FROM chunks c
                         JOIN raw_sources s ON s.source_id = c.source_id AND s.content_sha256 = c.source_sha256
                         WHERE c.chunk_id = ?1
                           AND s.superseded_by_source IS NULL
                           AND s.uri NOT LIKE 'session:%'
                           AND NOT EXISTS (SELECT 1 FROM tombstones t WHERE t.tombstone_id = c.source_id)
                           AND EXISTS (
                               SELECT 1 FROM json_each(c.readable_by_json) a
                               WHERE a.value = ?2
                                  OR (a.value = '*' AND EXISTS (
                                      SELECT 1 FROM members_tokens m WHERE m.actor_id = ?2 AND m.revoked_at IS NULL
                                  ))
                           )
                           AND (s.source_scope_id IS NULL OR EXISTS (
                               SELECT 1 FROM source_scopes sc, json_each(sc.current_audience_json) sa
                               WHERE sc.scope_id = s.source_scope_id
                                 AND (sa.value = ?2
                                      OR (sa.value = '*' AND EXISTS (
                                          SELECT 1 FROM members_tokens m WHERE m.actor_id = ?2 AND m.revoked_at IS NULL
                                      )))
                           ))",
                        params![chunk_id, viewer.as_str()],
                        |r| {
                            Ok(ChunkMetaRow {
                                heading: r.get(0)?,
                                body: r.get(1)?,
                                created_at: r.get(2)?,
                                uri: r.get(3)?,
                                ingested_by: r.get(4)?,
                                scope_id: r.get(5)?,
                            })
                        },
                    )
                    .optional()?;
                let Some(row) = row else {
                    continue;
                };
                let clean_heading =
                    collapse_inline(&strip_turn_tags_and_assistant_lines(&row.heading));
                let clean_body = collapse_inline(&strip_turn_tags_and_assistant_lines(&row.body));
                if clean_body.is_empty() && clean_heading.is_empty() {
                    continue;
                }
                let speaker = row
                    .ingested_by
                    .and_then(|s| s.parse::<ActorId>().ok())
                    .unwrap_or_else(|| owner_actor.clone());
                let source_label = row
                    .scope_id
                    .and_then(|sid| lookup_scope_label(&tx, &sid).ok().flatten())
                    .filter(|l| !l.trim().is_empty())
                    .unwrap_or(row.uri);
                let claim_text = if clean_heading.is_empty() {
                    clean_body
                } else if clean_body.is_empty() {
                    clean_heading
                } else {
                    format!("{clean_heading}: {clean_body}")
                };
                out.insert(
                    doc_id.clone(),
                    PromptDocMeta {
                        doc_id: doc_id.clone(),
                        ref_id: chunk_id.to_owned(),
                        kind: None,
                        recorded_at: EpochMs(row.created_at),
                        speaker,
                        fact_author: None,
                        source_label,
                        claim_text,
                        pending_ambiguous_question: None,
                        supporting_premises: Vec::new(),
                    },
                );
            }
        }
        tx.commit()?;
        Ok(out)
    }
}

enum CandidateTarget {
    Fact(FactId),
    Chunk(String),
}

struct PreMaterializedCandidate {
    doc_id: String,
    kind: CandidateTarget,
    lex_hits: u32,
    vec_sim: f64,
}

struct ChunkDocRow {
    heading: String,
    passage: String,
    anchor_json: String,
    blob: Option<Vec<u8>>,
    dims: Option<usize>,
}

struct ChunkMetaRow {
    heading: String,
    body: String,
    created_at: i64,
    uri: String,
    ingested_by: Option<String>,
    scope_id: Option<String>,
}
