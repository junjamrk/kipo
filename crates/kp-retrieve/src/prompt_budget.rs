use std::sync::OnceLock;

use kp_model::{
    format_date_iso_in_tz, system_time_zone, ActorId, EpochMs, FactId, FactKind, TimeZone,
};
pub use kp_store::{strip_turn_tags_and_assistant_lines, PromptPremise};
use kp_store::{Embedder, Store, Tokenizer};
use serde::{Deserialize, Serialize};

use crate::candidate_score::{
    extract_ascii_tokens, is_cjk_function_particle, rank_candidates_lexical,
};
use crate::engine::embed_query_optional;
use crate::{Result, RetrieveError, SecretRedactor};

static SHARED_REDACTOR: OnceLock<SecretRedactor> = OnceLock::new();

pub(crate) fn shared_redactor() -> Result<&'static SecretRedactor> {
    if let Some(r) = SHARED_REDACTOR.get() {
        return Ok(r);
    }
    let compiled = SecretRedactor::new()?;
    let _ = SHARED_REDACTOR.set(compiled);
    SHARED_REDACTOR
        .get()
        .ok_or(RetrieveError::Config("redactor initialization failed"))
}

/// Hard ceiling on tokens injected by `UserPromptSubmit` (`docs/ARCHITECTURE.md`).
pub const MAX_PROMPT_CONTEXT_TOKENS: usize = 250;

/// Hard ceiling on memory items injected per prompt (`docs/ARCHITECTURE.md`).
pub const MAX_PROMPT_CONTEXT_ITEMS: usize = 3;

/// Default relevance probability gate below which zero context is injected.
pub const DEFAULT_MIN_RELEVANCE_SCORE: f64 = 0.30;

/// Minimum cosine similarity required for a vector-only hit (without lexical match) to count,
/// preventing low-magnitude hash collisions on unrelated prompts from crossing the threshold.
const MIN_SEMANTIC_ONLY_SIMILARITY: f64 = 0.28;

/// Budget and formatting configuration for Hook prompt context injection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PromptBudgetConfig {
    pub max_tokens: usize,
    pub max_items: usize,
    pub min_score: f64,
    #[serde(skip, default = "system_time_zone")]
    pub time_zone: TimeZone,
    pub candidate_limit: usize,
}

impl Default for PromptBudgetConfig {
    fn default() -> Self {
        Self {
            max_tokens: MAX_PROMPT_CONTEXT_TOKENS,
            max_items: MAX_PROMPT_CONTEXT_ITEMS,
            min_score: DEFAULT_MIN_RELEVANCE_SCORE,
            time_zone: system_time_zone(),
            candidate_limit: 24,
        }
    }
}

impl PromptBudgetConfig {
    pub fn with_time_zone(mut self, time_zone: TimeZone) -> Self {
        self.time_zone = time_zone;
        self
    }
}

/// Pre-resolved candidate memory item ready for budgeted prompt formatting.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PromptCandidate {
    pub ref_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<FactKind>,
    pub recorded_at: EpochMs,
    pub speaker: ActorId,
    pub fact_author: Option<ActorId>,
    pub source_label: String,
    pub claim_text: String,
    pub score: f64,
    pub pending_ambiguous_question: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supporting_premises: Vec<PromptPremise>,
}

/// One rendered memory line accepted within the ≤250-token / ≤3-item budget.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FormattedPromptItem {
    pub ref_id: String,
    pub rendered: String,
    pub tokens: usize,
    pub score: f64,
    pub has_review_hint: bool,
}

/// Final Hook prompt context output; `text` is empty (`""`) whenever nothing clears `min_score`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FormattedPromptContext {
    pub text: String,
    pub items: Vec<FormattedPromptItem>,
    pub total_tokens: usize,
}

/// Conservative deterministic BPE token upper-bound estimator across mixed CJK, ASCII identifiers,
/// punctuation, and opaque `fact_<uuidv7>` anchors.
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let mut ascii_tokens = 0usize;
    let mut ascii_run = 0usize;
    let mut non_ascii_scalars = 0usize;

    for ch in text.chars() {
        if ch.is_ascii() {
            if ch.is_ascii_alphanumeric() {
                ascii_run += 1;
            } else {
                if ascii_run > 0 {
                    ascii_tokens += ascii_run.div_ceil(4);
                    ascii_run = 0;
                }
                if ch == '\n' || !ch.is_ascii_whitespace() {
                    ascii_tokens += 1;
                }
            }
        } else {
            if ascii_run > 0 {
                ascii_tokens += ascii_run.div_ceil(4);
                ascii_run = 0;
            }
            if !ch.is_whitespace() {
                non_ascii_scalars += 1;
            }
        }
    }
    if ascii_run > 0 {
        ascii_tokens += ascii_run.div_ceil(4);
    }
    ascii_tokens + (non_ascii_scalars.saturating_mul(3)).div_ceil(2)
}

/// Ensures a `FactId` citation token always carries the `fact_` prefix (`fact_<id>`).
pub fn format_fact_kp_ref(fact_id: &FactId) -> String {
    let raw = fact_id.as_str().trim();
    if raw.starts_with("fact_") {
        raw.to_owned()
    } else {
        format!("fact_{raw}")
    }
}

pub(crate) fn normalize_kp_ref(raw_ref: &str, is_fact: bool) -> String {
    let clean = sanitize_header_field(raw_ref);
    if is_fact
        && !clean.is_empty()
        && !clean.starts_with("fact_")
        && !clean.starts_with("src_")
        && !clean.starts_with("chunk_")
    {
        format!("fact_{clean}")
    } else {
        clean
    }
}

/// Shared provenance header formatter across `prompt_budget` and `guard`:
/// `[YYYY-MM-DD · speaker @ source · kp-ref:<ref_id>]`.
pub fn format_citation_header(
    recorded_at: EpochMs,
    time_zone: &TimeZone,
    speaker: &ActorId,
    source_label: &str,
    kp_ref: &str,
) -> Result<String> {
    let redactor = shared_redactor()?;
    let date_str = format_date_iso_in_tz(recorded_at, time_zone);
    let source = sanitize_header_field(&redactor.redact(source_label));
    let source = if source.is_empty() {
        "local".to_owned()
    } else {
        source
    };
    let clean_ref = sanitize_header_field(kp_ref);
    Ok(format!(
        "[{date_str} · {speaker} @ {source} · kp-ref:{clean_ref}]"
    ))
}

/// Shared single-line citation formatter across `prompt_budget` and `guard`:
/// `[YYYY-MM-DD · speaker @ source · kp-ref:fact_xxx] <redacted single-line quote/claim>`.
pub fn format_citation_line(
    recorded_at: EpochMs,
    time_zone: &TimeZone,
    speaker: &ActorId,
    source_label: &str,
    kp_ref: &str,
    claim_or_quote: &str,
    author_review_question: Option<&str>,
) -> Result<String> {
    let redactor = shared_redactor()?;
    let header = format_citation_header(recorded_at, time_zone, speaker, source_label, kp_ref)?;
    let stripped_body = strip_turn_tags_and_assistant_lines(claim_or_quote);
    let raw_body = sanitize_inline(&redactor.redact(&stripped_body));
    let chars: Vec<char> = raw_body.chars().collect();
    let body = if chars.len() > 240 {
        format!("{}…", chars[..239].iter().collect::<String>().trim_end())
    } else {
        raw_body
    };
    let review_suffix = author_review_question
        .map(|q| sanitize_inline(&redactor.redact(q)))
        .filter(|q| !q.is_empty())
        .map(|q| format!("（待确认：{q} 可用 kp review 确认）"))
        .unwrap_or_default();
    Ok(format!("{header} {body}{review_suffix}"))
}

/// Skips zero-information acknowledgements and `<skip_summary/>` sentinels during Hook queries.
pub fn is_low_signal_ack(text: &str) -> bool {
    let trimmed = text
        .trim()
        .trim_matches(['。', '！', '!', '.', '~', '～', ' ', '\n', '\r', '\t'])
        .to_lowercase();
    matches!(
        trimmed.as_str(),
        "" | "<skip_summary/>"
            | "好"
            | "好的"
            | "收到"
            | "明白"
            | "已阅"
            | "谢谢"
            | "ok"
            | "okay"
            | "+1"
            | "ack"
            | "lgtm"
            | "got it"
            | "thanks"
    )
}

/// Formats ranked candidates under the strict `≤ 250 tokens` and `≤ 3 items` Hook budget.
///
/// - Candidates below `config.min_score` (or non-finite) are dropped; if none pass, returns empty `text`.
/// - Every item starts with `[YYYY-MM-DD · speaker @ source · kp-ref:<ref_id>]` (fact IDs always carry `fact_`).
/// - Serendipitous review hint `（待确认：<question> 可用 kp review 确认）` is appended ONLY when
///   `Some(viewer) == candidate.fact_author.as_ref()` and `pending_ambiguous_question` is non-empty.
pub fn format_prompt_candidates(
    viewer: &ActorId,
    candidates: &[PromptCandidate],
    config: &PromptBudgetConfig,
) -> Result<FormattedPromptContext> {
    if !config.min_score.is_finite() || !(0.0..=1.0).contains(&config.min_score) {
        return Err(RetrieveError::Config(
            "min_score must be a finite probability in [0.0, 1.0]",
        ));
    }
    let token_cap = config.max_tokens.min(MAX_PROMPT_CONTEXT_TOKENS);
    let item_cap = config.max_items.min(MAX_PROMPT_CONTEXT_ITEMS);
    if token_cap == 0 || item_cap == 0 || candidates.is_empty() {
        return Ok(FormattedPromptContext::default());
    }

    let threshold = config.min_score.max(f64::EPSILON);
    let mut eligible: Vec<&PromptCandidate> = candidates
        .iter()
        .filter(|c| c.score.is_finite() && c.score >= threshold && !c.ref_id.trim().is_empty())
        .collect();
    if eligible.is_empty() {
        return Ok(FormattedPromptContext::default());
    }
    let candidate_priority = |c: &PromptCandidate| -> u8 {
        if c.kind == Some(FactKind::CollisionWarning) {
            2
        } else if !c.supporting_premises.is_empty() {
            1
        } else {
            0
        }
    };
    eligible.sort_by(|a, b| {
        candidate_priority(b)
            .cmp(&candidate_priority(a))
            .then_with(|| b.score.total_cmp(&a.score))
            .then_with(|| b.recorded_at.cmp(&a.recorded_at))
            .then_with(|| a.ref_id.cmp(&b.ref_id))
    });

    let redactor = shared_redactor()?;
    let mut accepted = Vec::<FormattedPromptItem>::new();
    let mut seen_refs = std::collections::BTreeSet::<String>::new();
    let mut cited_premise_refs = std::collections::BTreeSet::<String>::new();

    for cand in eligible {
        if accepted.len() >= item_cap {
            break;
        }
        let kp_ref = normalize_kp_ref(&cand.ref_id, cand.fact_author.is_some());
        if cited_premise_refs.contains(&kp_ref) || seen_refs.contains(&kp_ref) {
            continue;
        }
        let header = format_citation_header(
            cand.recorded_at,
            &config.time_zone,
            &cand.speaker,
            &cand.source_label,
            &kp_ref,
        )?;

        let show_hint = cand.fact_author.as_ref() == Some(viewer);
        let review_suffix = if show_hint {
            cand.pending_ambiguous_question
                .as_deref()
                .map(strip_turn_tags_and_assistant_lines)
                .map(|q| sanitize_inline(&redactor.redact(&q)))
                .filter(|q| !q.is_empty())
                .map(|q| format!("（待确认：{q} 可用 kp review 确认）"))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let has_review_hint = !review_suffix.is_empty();

        let stripped_claim = strip_turn_tags_and_assistant_lines(&cand.claim_text);
        let raw_claim = sanitize_inline(&redactor.redact(&stripped_claim));
        if raw_claim.is_empty() {
            continue;
        }

        let current_joined = accepted
            .iter()
            .map(|it| it.rendered.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        let Some(fitted_line) = fit_candidate_line(
            &current_joined,
            &header,
            &raw_claim,
            &review_suffix,
            token_cap,
        ) else {
            continue;
        };
        seen_refs.insert(kp_ref.clone());

        let mut block_lines = vec![fitted_line];
        let mut accepted_prem_refs = Vec::<String>::new();
        for prem in &cand.supporting_premises {
            let prem_kp_ref = format_fact_kp_ref(&prem.fact_id);
            if prem_kp_ref == kp_ref || accepted_prem_refs.contains(&prem_kp_ref) {
                continue;
            }
            let stripped_quote = strip_turn_tags_and_assistant_lines(&prem.claim_or_quote);
            let clean_quote = sanitize_inline(&redactor.redact(&stripped_quote));
            if clean_quote.is_empty() {
                continue;
            }
            let prem_header = format_citation_header(
                prem.recorded_at,
                &config.time_zone,
                &prem.speaker,
                &prem.source_label,
                &prem_kp_ref,
            )?;
            let existing_with_block = if current_joined.is_empty() {
                block_lines.join("\n")
            } else {
                format!("{current_joined}\n{}", block_lines.join("\n"))
            };
            let indented_header = format!("  ↳ {prem_header}");
            let Some(fitted_prem) = fit_candidate_line(
                &existing_with_block,
                &indented_header,
                &clean_quote,
                "",
                token_cap,
            ) else {
                continue;
            };
            block_lines.push(fitted_prem);
            accepted_prem_refs.push(prem_kp_ref);
        }
        cited_premise_refs.extend(accepted_prem_refs);

        let rendered_block = block_lines.join("\n");
        let item_tokens = estimate_tokens(&rendered_block);
        accepted.push(FormattedPromptItem {
            ref_id: cand.ref_id.clone(),
            rendered: rendered_block,
            tokens: item_tokens,
            score: cand.score,
            has_review_hint,
        });
    }

    if accepted.is_empty() {
        return Ok(FormattedPromptContext::default());
    }

    let text = accepted
        .iter()
        .map(|i| i.rendered.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let total_tokens = estimate_tokens(&text);
    Ok(FormattedPromptContext {
        text,
        items: accepted,
        total_tokens,
    })
}

/// End-to-end candidate-bounded Hook retrieval + ≤250-token prompt formatter using `store.embedder()`.
pub fn retrieve_prompt_context(
    store: &Store,
    viewer: &ActorId,
    at: EpochMs,
    query: &str,
    config: &PromptBudgetConfig,
) -> Result<FormattedPromptContext> {
    retrieve_prompt_context_with_embedder(store, viewer, at, query, config, store.embedder())
}

/// End-to-end candidate-bounded Hook retrieval + active RSI skill matching + ≤250-token prompt formatter.
pub fn retrieve_prompt_context_with_embedder(
    store: &Store,
    viewer: &ActorId,
    at: EpochMs,
    query: &str,
    config: &PromptBudgetConfig,
    embedder: &(impl Embedder + ?Sized),
) -> Result<FormattedPromptContext> {
    if !config.min_score.is_finite() || !(0.0..=1.0).contains(&config.min_score) {
        return Err(RetrieveError::Config(
            "min_score must be a finite probability in [0.0, 1.0]",
        ));
    }
    let token_cap = config.max_tokens.min(MAX_PROMPT_CONTEXT_TOKENS);
    let item_cap = config.max_items.min(MAX_PROMPT_CONTEXT_ITEMS);
    let trimmed_query = query.trim();
    if trimmed_query.is_empty()
        || is_low_signal_ack(trimmed_query)
        || token_cap == 0
        || item_cap == 0
    {
        return Ok(FormattedPromptContext::default());
    }

    let terms = Tokenizer::shared().tokenize(trimmed_query);
    let mut prompt_candidates = match_active_skill_candidates(store, viewer, at, &terms)?;

    let candidate_limit = config
        .candidate_limit
        .max(item_cap.saturating_mul(4))
        .max(8);
    let query_vector = embed_query_optional(trimmed_query, embedder)?;
    let snapshot = store.candidate_retrieval_snapshot_with_query_vector(
        viewer,
        at,
        trimmed_query,
        query_vector.as_deref(),
        candidate_limit,
        embedder,
    )?;
    let docs = snapshot.documents();
    if docs.is_empty() {
        if prompt_candidates.is_empty() {
            return Ok(FormattedPromptContext::default());
        }
        return format_prompt_candidates(viewer, &prompt_candidates, config);
    }

    let mut rrf = vec![0.0_f64; docs.len()];
    let mut lexical_matched = vec![false; docs.len()];

    if !terms.is_empty() {
        for (rank, (idx, _)) in rank_candidates_lexical(docs, &terms, docs.len(), true)
            .into_iter()
            .enumerate()
        {
            lexical_matched[idx] = true;
            rrf[idx] += 1.0 / (60.0 + (rank + 1) as f64);
        }
    }

    if let Some(ref qvec) = query_vector {
        let mut vec_hits = Vec::<(usize, f64)>::new();
        for (idx, doc) in docs.iter().enumerate() {
            if doc.vector.is_empty() {
                continue;
            }
            if doc.vector.len() != qvec.len() {
                return Err(RetrieveError::Config(
                    "query and stored embedding dimensions differ",
                ));
            }
            let sim: f64 = qvec
                .iter()
                .zip(&doc.vector)
                .map(|(a, b)| f64::from(*a) * f64::from(*b))
                .sum();
            if (lexical_matched[idx] && sim > 0.0) || sim >= MIN_SEMANTIC_ONLY_SIMILARITY {
                vec_hits.push((idx, sim));
            }
        }
        vec_hits.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        for (rank, (idx, _)) in vec_hits.into_iter().enumerate() {
            rrf[idx] += 1.0 / (60.0 + (rank + 1) as f64);
        }
    }

    let threshold = config.min_score.max(f64::EPSILON);
    let mut scored_indices = Vec::<(usize, f64)>::new();
    for (idx, doc) in docs.iter().enumerate() {
        if doc.kind == FactKind::SkillVersion.as_str() || rrf[idx] <= 0.0 {
            continue;
        }
        let prob = 1.0 - (-30.0 * rrf[idx]).exp();
        if prob >= threshold {
            scored_indices.push((idx, prob));
        }
    }
    if !scored_indices.is_empty() {
        scored_indices.sort_by(|a, b| {
            b.1.total_cmp(&a.1)
                .then_with(|| docs[a.0].id.cmp(&docs[b.0].id))
        });

        let mut scored_doc_scores = std::collections::BTreeMap::<String, f64>::new();
        let mut matched_fact_ids = Vec::<FactId>::new();
        let mut doc_ids = Vec::<String>::with_capacity(scored_indices.len());
        for &(idx, score) in &scored_indices {
            let id = docs[idx].id.clone();
            scored_doc_scores.insert(id.clone(), score);
            if let Some(raw_fid) = id.strip_prefix("fact-") {
                if let Ok(fid) = raw_fid.parse::<FactId>() {
                    matched_fact_ids.push(fid);
                }
            }
            doc_ids.push(id);
        }

        for wid in store.visible_collision_warnings_for_premises(viewer, at, &matched_fact_ids)? {
            let wid_doc = format!("fact-{wid}");
            if !scored_doc_scores.contains_key(&wid_doc) {
                doc_ids.push(wid_doc);
            }
        }

        let meta_map = store.prompt_candidate_meta(viewer, at, &doc_ids)?;

        for doc_id in &doc_ids {
            let Some(meta) = meta_map.get(doc_id) else {
                continue;
            };
            let own_score = scored_doc_scores.get(doc_id).copied().unwrap_or(0.0);
            let prem_score = if meta.kind == Some(FactKind::CollisionWarning) {
                meta.supporting_premises
                    .iter()
                    .filter_map(|p| {
                        scored_doc_scores
                            .get(&format!("fact-{}", p.fact_id))
                            .copied()
                    })
                    .fold(0.0_f64, f64::max)
            } else {
                0.0
            };
            let score = own_score.max(prem_score);
            if score < threshold {
                continue;
            }
            prompt_candidates.push(PromptCandidate {
                ref_id: meta.ref_id.clone(),
                kind: meta.kind,
                recorded_at: meta.recorded_at,
                speaker: meta.speaker.clone(),
                fact_author: meta.fact_author.clone(),
                source_label: meta.source_label.clone(),
                claim_text: meta.claim_text.clone(),
                score,
                pending_ambiguous_question: meta.pending_ambiguous_question.clone(),
                supporting_premises: meta.supporting_premises.clone(),
            });
        }
    }

    if prompt_candidates.is_empty() {
        return Ok(FormattedPromptContext::default());
    }
    format_prompt_candidates(viewer, &prompt_candidates, config)
}

fn is_skill_stopword(term: &str) -> bool {
    if is_cjk_function_particle(term) {
        return true;
    }
    if term.is_ascii() && term.len() < 3 {
        return true;
    }
    matches!(
        term,
        "the" | "for" | "and" | "how" | "with" | "when" | "load" | "skill" | "from" | "into"
    )
}

fn match_active_skill_candidates(
    store: &Store,
    viewer: &ActorId,
    at: EpochMs,
    query_terms: &[String],
) -> Result<Vec<PromptCandidate>> {
    let skills = store.active_skills(viewer, at)?;
    if skills.is_empty() || query_terms.is_empty() {
        return Ok(Vec::new());
    }
    let substantive_terms: Vec<&str> = query_terms
        .iter()
        .map(String::as_str)
        .filter(|t| !t.trim().is_empty() && !is_skill_stopword(t))
        .collect();
    if substantive_terms.is_empty() {
        return Ok(Vec::new());
    }

    let owner = store.owner_actor();
    let mut matched = Vec::new();
    for skill in skills {
        let haystack = format!(
            "{} {} {}",
            skill.package.name, skill.package.trigger, skill.package.description
        )
        .to_lowercase();
        let ascii_tokens = extract_ascii_tokens(&haystack);
        let hits = substantive_terms
            .iter()
            .filter(|&&term| {
                if term.is_ascii() {
                    ascii_tokens.contains_key(term) || haystack.contains(term)
                } else {
                    haystack.contains(term)
                }
            })
            .count();
        if hits == 0 {
            continue;
        }
        // Place quarantine `READ_ONLY_SKILL` notice immediately after `trigger_line()` before `description`
        // so length/token truncation never drops `READ_ONLY_SKILL`.
        let summary = match skill.notice {
            Some(notice) => format!(
                "{} {notice} {}",
                skill.package.trigger_line(),
                skill.package.description
            ),
            None => format!(
                "{} {}",
                skill.package.trigger_line(),
                skill.package.description
            ),
        };
        let score = 0.90 + 0.01 * (hits.min(9) as f64);
        matched.push(PromptCandidate {
            ref_id: skill.fact_id.to_string(),
            kind: Some(FactKind::SkillVersion),
            recorded_at: at,
            speaker: owner.clone(),
            fact_author: Some(owner.clone()),
            source_label: "local".to_owned(),
            claim_text: summary,
            score,
            pending_ambiguous_question: None,
            supporting_premises: Vec::new(),
        });
    }
    matched.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.ref_id.cmp(&b.ref_id))
    });
    matched.truncate(1);
    Ok(matched)
}

fn fit_candidate_line(
    existing_joined: &str,
    header: &str,
    claim: &str,
    review_suffix: &str,
    token_cap: usize,
) -> Option<String> {
    let build_full = |body: &str| {
        let line = format!("{header} {body}{review_suffix}");
        if existing_joined.is_empty() {
            (line.clone(), line)
        } else {
            (format!("{existing_joined}\n{line}"), line)
        }
    };

    // Cap raw single-item body length before token binary search while leaving ample room for `READ_ONLY_SKILL`.
    let chars: Vec<char> = claim.chars().collect();
    let initial_claim = if chars.len() > 240 {
        format!("{}…", chars[..239].iter().collect::<String>().trim_end())
    } else {
        claim.to_owned()
    };

    let (combined, line) = build_full(&initial_claim);
    if estimate_tokens(&combined) <= token_cap {
        return Some(line);
    }

    // Binary search the longest non-empty prefix of `claim` (>= 8 chars) that fits `token_cap`
    // while keeping both `header` and `review_suffix` 100% intact.
    let max_prefix = chars.len().min(239);
    let min_prefix = 8usize.min(max_prefix);
    if min_prefix == 0 {
        return None;
    }
    let min_body = if min_prefix < chars.len() {
        format!(
            "{}…",
            chars[..min_prefix].iter().collect::<String>().trim_end()
        )
    } else {
        chars[..min_prefix].iter().collect()
    };
    if estimate_tokens(&build_full(&min_body).0) > token_cap {
        return None;
    }

    let mut low = min_prefix;
    let mut high = max_prefix;
    let mut best = min_body;
    while low <= high {
        let mid = low + (high - low) / 2;
        let candidate_body = if mid < chars.len() {
            format!("{}…", chars[..mid].iter().collect::<String>().trim_end())
        } else {
            chars[..mid].iter().collect()
        };
        let (cand_combined, _) = build_full(&candidate_body);
        if estimate_tokens(&cand_combined) <= token_cap {
            best = candidate_body;
            low = mid + 1;
        } else if mid == 0 {
            break;
        } else {
            high = mid - 1;
        }
    }
    Some(build_full(&best).1)
}

pub(crate) fn sanitize_header_field(raw: &str) -> String {
    raw.chars()
        .map(|c| match c {
            '[' | ']' | '\n' | '\r' | '\t' => ' ',
            other => other,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn sanitize_inline(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
#[path = "prompt_budget_tests.rs"]
mod tests;
