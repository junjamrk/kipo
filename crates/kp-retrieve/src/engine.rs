use std::collections::{BTreeMap, BTreeSet};

use kp_model::{ActorId, AnchorSpec, EpochMs, LibraryId};
use kp_store::{
    sanitize_and_validate_embedding, validate_vector, Embedder, ScopedIndex, Store, Tokenizer,
    VisibleDocument,
};
use serde::Serialize;

use crate::candidate_score::rank_candidates_lexical;
use crate::diversify::diversify;
use crate::expansion::{expansion_terms, Clue, Vocabulary};
use crate::judge::{EvidenceBatch, EvidenceJudge};
use crate::{Result, RetrieveError};

pub struct SearchOptions {
    pub query: String,
    pub viewer: ActorId,
    pub at: EpochMs,
    pub limit: usize,
    pub deep: bool,
    pub max_hops: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SearchResult {
    pub id: String,
    pub library_id: LibraryId,
    pub kind: String,
    pub heading: String,
    pub passage: String,
    pub anchor: Option<AnchorSpec>,
    pub score: f64,
    pub hop: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct Trace {
    pub hop: usize,
    pub candidate_ids: Vec<String>,
    pub clues: Vec<Clue>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SearchOutput {
    pub results: Vec<SearchResult>,
    pub trace: Vec<Trace>,
}

pub(crate) fn embed_query_optional(
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

/// Synchronous shallow hybrid search (`FTS5` + stored vectors + reverse-DFS snapshot, shared
/// `Jieba`, pointwise lexical scoring without `ScopedIndex` allocation, top-32 cosine vector hits,
/// and calibrated RRF fusion).
pub fn search_shallow(
    store: &Store,
    viewer: &ActorId,
    at: EpochMs,
    query: &str,
    limit: usize,
) -> Result<SearchOutput> {
    search_shallow_with_embedder(store, viewer, at, query, limit, store.embedder())
}

/// Synchronous shallow hybrid search parameterized by a custom [`Embedder`].
pub fn search_shallow_with_embedder(
    store: &Store,
    viewer: &ActorId,
    at: EpochMs,
    query: &str,
    limit: usize,
    embedder: &(impl Embedder + ?Sized),
) -> Result<SearchOutput> {
    let query_vector = embed_query_optional(query, embedder)?;
    if limit == 0 || query.trim().is_empty() {
        return Ok(SearchOutput {
            results: Vec::new(),
            trace: Vec::new(),
        });
    }
    let terms = Tokenizer::shared().tokenize(query);
    if terms.is_empty() && query_vector.is_none() {
        return Ok(SearchOutput {
            results: Vec::new(),
            trace: Vec::new(),
        });
    }
    let candidate_limit = limit.max(8).saturating_mul(3).min(200);
    let snapshot =
        store.search_snapshot_with_embedder(viewer, at, query, false, candidate_limit, embedder)?;
    let docs = snapshot.documents();
    if docs.is_empty() {
        return Ok(SearchOutput {
            results: Vec::new(),
            trace: Vec::new(),
        });
    }

    let lexical_hits = rank_candidates_lexical(docs, &terms, 32, false);
    let vector_hits = match &query_vector {
        Some(qvec) => rank_candidates_vector(docs, qvec, 32)?,
        None => Vec::new(),
    };
    let mut scores = fuse_rrf_scores(docs.len(), &lexical_hits, &vector_hits);
    let order = rank(&scores);
    let trace = vec![trace_entry(0, &order, docs, Vec::new())];
    calibrate_rrf_scores(&mut scores);
    let results = materialize_search_results(docs, order, &scores, None, limit);
    Ok(SearchOutput { results, trace })
}

pub async fn search(
    store: &Store,
    options: &SearchOptions,
    judge: &impl EvidenceJudge,
) -> Result<SearchOutput> {
    search_with_embedder(store, options, judge, store.embedder()).await
}

pub async fn search_with_embedder(
    store: &Store,
    options: &SearchOptions,
    judge: &impl EvidenceJudge,
    embedder: &(impl Embedder + ?Sized),
) -> Result<SearchOutput> {
    if options.max_hops > 2 {
        return Err(RetrieveError::Config("max_hops must be at most 2"));
    }
    let query_vector = embed_query_optional(&options.query, embedder)?;
    if options.limit == 0 {
        return Ok(SearchOutput {
            results: Vec::new(),
            trace: Vec::new(),
        });
    }
    let is_candidate_mode = !options.deep;
    let snapshot = if is_candidate_mode {
        store.candidate_retrieval_snapshot_with_embedder(
            &options.viewer,
            options.at,
            &options.query,
            options.limit.max(64),
            embedder,
        )?
    } else {
        store.retrieval_snapshot_with_embedder(&options.viewer, options.at, embedder)?
    };
    let docs = snapshot.documents();

    let mut deep_tokenizer = options.deep.then(Tokenizer::default);
    if let Some(tok) = &mut deep_tokenizer {
        for entity in docs.iter().flat_map(|d| &d.entities) {
            tok.add_entity_word(entity);
        }
    }
    let active_tokenizer: &Tokenizer = deep_tokenizer
        .as_ref()
        .unwrap_or_else(|| Tokenizer::shared());
    let terms = active_tokenizer.tokenize(&options.query);
    if (terms.is_empty() && query_vector.is_none()) || docs.is_empty() {
        return Ok(SearchOutput {
            results: Vec::new(),
            trace: Vec::new(),
        });
    }

    // Build transient `ScopedIndex` strictly for `--deep` multi-hop phrase/clue expansion;
    // shallow search (both Personal and Community) scores pre-recalled candidates directly.
    let scoped_index = if is_candidate_mode {
        None
    } else {
        Some(ScopedIndex::new(&snapshot, active_tokenizer)?)
    };
    let deep_vocabulary = options
        .deep
        .then(|| Vocabulary::new(docs, active_tokenizer));

    let lexical_hits = if let Some(index) = &scoped_index {
        index.search(&terms, false, 32)?
    } else {
        rank_candidates_lexical(docs, &terms, 32, false)
    };

    let vector_hits = match &query_vector {
        Some(qvec) => rank_candidates_vector(docs, qvec, 32)?,
        None => Vec::new(),
    };
    let mut scores = fuse_rrf_scores(docs.len(), &lexical_hits, &vector_hits);
    let mut hops = vec![0; docs.len()];
    let seed_order = rank(&scores);
    let protected: Vec<_> = seed_order.iter().copied().take(5).collect();
    let mut trace = vec![trace_entry(0, &seed_order, docs, Vec::new())];
    if let (Some(vocabulary), Some(index)) = (&deep_vocabulary, &scoped_index) {
        entity_hop(docs, &seed_order, &mut scores, &mut hops, 1);
        let mut frontier = rank(&scores);
        for hop in 1..=options.max_hops {
            let seeds: Vec<_> = frontier
                .iter()
                .take(20)
                .map(|&i| (&docs[i], scores[i]))
                .collect();
            let clues = expansion_terms(&options.query, &seeds, vocabulary, active_tokenizer)?;
            let mut lists = Vec::new();
            for clue in &clues {
                let terms = active_tokenizer.tokenize(&clue.term);
                lists.push(
                    index
                        .search(&terms, clue.name_phrase, 20)?
                        .into_iter()
                        .map(|(i, _)| i)
                        .collect::<Vec<_>>(),
                );
            }
            // Interleave rank then clue; a prolific bridge cannot consume the whole expansion budget.
            let mut found = Vec::new();
            let mut seen = BTreeSet::new();
            for rank in 0..20 {
                for list in &lists {
                    if let Some(&i) = list.get(rank) {
                        if seen.insert(i) && found.len() < 64 {
                            found.push(i);
                        }
                    }
                }
            }
            let previous: BTreeSet<_> = rank(&scores).into_iter().collect();
            for (rank, &i) in found.iter().enumerate() {
                if scores[i] == 0.0 {
                    hops[i] = hop;
                }
                scores[i] += 0.7_f64.powi(hop as i32) / (60.0 + (rank + 1) as f64);
            }
            entity_hop(docs, &frontier, &mut scores, &mut hops, hop);
            frontier = rank(&scores)
                .into_iter()
                .filter(|i| !previous.contains(i))
                .collect();
            trace.push(trace_entry(hop, &found, docs, clues));
            if frontier.is_empty() {
                break;
            }
        }
    }
    let mut order = candidate_pool(&scores, if options.deep { &protected } else { &[] });
    let candidates = order.iter().map(|&i| &docs[i]).collect();
    let judged = judge
        .score(EvidenceBatch {
            query: &options.query,
            candidates,
        })
        .await?;
    if let Some(probabilities) = judged {
        if probabilities.len() != order.len()
            || probabilities
                .iter()
                .any(|s| !s.is_finite() || !(0.0..=1.0).contains(s))
        {
            return Err(RetrieveError::Protocol(
                "expected one probability per candidate",
            ));
        }
        for (&i, probability) in order.iter().zip(probabilities) {
            scores[i] = probability;
        }
        order.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
    } else {
        calibrate_rrf_scores(&mut scores);
        if let Some(vocabulary) = &deep_vocabulary {
            diversify(&mut order, &scores, docs, vocabulary, active_tokenizer, 0);
        }
    }
    let results = materialize_search_results(docs, order, &scores, Some(&hops), options.limit);
    Ok(SearchOutput { results, trace })
}

fn rank_candidates_vector(
    docs: &[VisibleDocument],
    query_vector: &[f32],
    limit: usize,
) -> Result<Vec<(usize, f64)>> {
    let mut vector_hits = Vec::new();
    for (i, doc) in docs.iter().enumerate() {
        if doc.vector.is_empty() {
            continue;
        }
        if doc.vector.len() != query_vector.len() {
            return Err(RetrieveError::Config(
                "query and stored embedding dimensions differ",
            ));
        }
        let similarity: f64 = query_vector
            .iter()
            .zip(&doc.vector)
            .map(|(a, b)| f64::from(*a) * f64::from(*b))
            .sum();
        if similarity > 0.0 {
            vector_hits.push((i, similarity));
        }
    }
    vector_hits.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    vector_hits.truncate(limit);
    Ok(vector_hits)
}

fn fuse_rrf_scores(
    doc_count: usize,
    lexical_hits: &[(usize, f64)],
    vector_hits: &[(usize, f64)],
) -> Vec<f64> {
    let mut scores = vec![0.0; doc_count];
    for list in [lexical_hits, vector_hits] {
        for (rank_idx, (i, _)) in list.iter().enumerate() {
            scores[*i] += 1.0 / (60.0 + (rank_idx + 1) as f64);
        }
    }
    scores
}

fn calibrate_rrf_scores(scores: &mut [f64]) {
    for score in scores {
        *score = 1.0 - (-30.0 * *score).exp();
    }
}

fn materialize_search_results(
    docs: &[VisibleDocument],
    order: impl IntoIterator<Item = usize>,
    scores: &[f64],
    hops: Option<&[usize]>,
    limit: usize,
) -> Vec<SearchResult> {
    order
        .into_iter()
        .take(limit)
        .map(|i| SearchResult {
            id: format!("{}:{}", docs[i].library_id, docs[i].id),
            library_id: docs[i].library_id.clone(),
            kind: docs[i].kind.clone(),
            heading: docs[i].heading.clone(),
            passage: docs[i].passage.clone(),
            anchor: docs[i].anchor.clone(),
            score: scores[i],
            hop: hops.map_or(0, |h| h[i]),
        })
        .collect()
}

fn rank(scores: &[f64]) -> Vec<usize> {
    let mut order: Vec<_> = (0..scores.len()).filter(|&i| scores[i] > 0.0).collect();
    order.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
    order
}

fn candidate_pool(scores: &[f64], protected: &[usize]) -> Vec<usize> {
    let mut order = rank(scores);
    // Reserve judging capacity for hop-0 evidence without reserving its final rank.
    order.retain(|i| !protected.contains(i));
    order.truncate(100 - protected.len());
    order.extend_from_slice(protected);
    order.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
    order
}

fn trace_entry(hop: usize, indices: &[usize], docs: &[VisibleDocument], clues: Vec<Clue>) -> Trace {
    Trace {
        hop,
        candidate_ids: indices
            .iter()
            .map(|&i| format!("{}:{}", docs[i].library_id, docs[i].id))
            .collect(),
        clues,
    }
}

fn entity_hop(
    docs: &[VisibleDocument],
    seeds: &[usize],
    scores: &mut [f64],
    hops: &mut [usize],
    hop: usize,
) {
    let mut incidence = BTreeMap::<&str, BTreeSet<usize>>::new();
    for (i, doc) in docs.iter().enumerate() {
        for term in doc.entities.iter().chain(&doc.tags) {
            incidence.entry(term).or_default().insert(i);
        }
    }
    let mut bridges = BTreeSet::new();
    for &i in seeds.iter().take(32) {
        bridges.extend(docs[i].entities.iter().chain(&docs[i].tags));
    }
    let seed_bridges = bridges.clone();
    for alias in docs.iter().filter(|d| d.kind == "alias") {
        if alias
            .entities
            .iter()
            .any(|entity| seed_bridges.contains(entity))
        {
            bridges.extend(&alias.entities);
        }
    }
    let mut boosts = BTreeMap::<usize, f64>::new();
    for bridge in bridges {
        if let Some(neighbors) = incidence.get(bridge.as_str()) {
            let damping = 1.0 / (1.0 + (neighbors.len() as f64).ln());
            for &i in neighbors {
                boosts
                    .entry(i)
                    .and_modify(|s| *s = s.max(damping))
                    .or_insert(damping);
            }
        }
    }
    for (i, boost) in boosts {
        if !seeds.contains(&i) {
            if scores[i] == 0.0 {
                hops[i] = hop;
            }
            scores[i] += boost / 61.0;
        }
    }
}

/// Direct probability ordering; no corpus statistics cross library boundaries.
pub fn merge_federated_results(
    slices: impl IntoIterator<Item = Vec<SearchResult>>,
) -> Vec<SearchResult> {
    let mut results: Vec<_> = slices
        .into_iter()
        .flatten()
        .filter(|r| r.score.is_finite() && (0.0..=1.0).contains(&r.score))
        .collect();
    results.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(a.library_id.cmp(&b.library_id))
            .then(a.id.cmp(&b.id))
    });
    let mut seen = BTreeSet::new();
    results.retain(|r| seen.insert((r.library_id.clone(), r.id.clone())));
    results
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
