use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use kp_store::VisibleDocument;

/// Ranks pre-recalled candidate documents directly against pre-tokenized `query_terms` without
/// building an in-memory SQLite `ScopedIndex` or re-running `Jieba` over candidate passages.
///
/// Each document's lexical score is strictly pointwise over `(query_terms, doc)`, preserving
/// 100% viewer noninterference.
pub(crate) fn rank_candidates_lexical(
    docs: &[VisibleDocument],
    query_terms: &[String],
    limit: usize,
    require_substantive_if_present: bool,
) -> Vec<(usize, f64)> {
    if docs.is_empty() || query_terms.is_empty() || limit == 0 {
        return Vec::new();
    }

    let mut unique_terms = Vec::<&str>::new();
    let mut seen = BTreeSet::<&str>::new();
    for term in query_terms {
        let s = term.as_str();
        if !s.is_empty() && seen.insert(s) {
            unique_terms.push(s);
        }
    }
    if unique_terms.is_empty() {
        return Vec::new();
    }

    let has_ascii_terms = unique_terms.iter().any(|t| t.is_ascii());
    let has_unicode_terms = unique_terms.iter().any(|t| !t.is_ascii());
    let has_substantive_query_term = unique_terms.iter().any(|t| !is_cjk_function_particle(t));

    let mut hits = Vec::<(usize, f64)>::new();

    for (idx, doc) in docs.iter().enumerate() {
        let quote = doc
            .anchor
            .as_ref()
            .and_then(|a| a.verbatim_quote.as_deref())
            .unwrap_or("");

        let ascii_heading = if has_ascii_terms {
            extract_ascii_tokens(&doc.heading)
        } else {
            BTreeMap::new()
        };
        let ascii_passage = if has_ascii_terms {
            extract_ascii_tokens(&doc.passage)
        } else {
            BTreeMap::new()
        };
        let ascii_quote = if has_ascii_terms && !quote.is_empty() {
            extract_ascii_tokens(quote)
        } else {
            BTreeMap::new()
        };

        let uni_heading = if has_unicode_terms {
            lowercase_unicode_if_needed(&doc.heading)
        } else {
            Cow::Borrowed("")
        };
        let uni_passage = if has_unicode_terms {
            lowercase_unicode_if_needed(&doc.passage)
        } else {
            Cow::Borrowed("")
        };
        let uni_quote = if has_unicode_terms && !quote.is_empty() {
            lowercase_unicode_if_needed(quote)
        } else {
            Cow::Borrowed("")
        };

        let doc_len = (doc.heading.chars().count() + doc.passage.chars().count()).max(1) as f64;
        let mut score = 0.0_f64;
        let mut matched_any = false;
        let mut matched_substantive = false;

        for &term in &unique_terms {
            let (tf_h, tf_p, tf_q) = if term.is_ascii() {
                (
                    ascii_heading.get(term).copied().unwrap_or(0),
                    ascii_passage.get(term).copied().unwrap_or(0),
                    ascii_quote.get(term).copied().unwrap_or(0),
                )
            } else {
                (
                    uni_heading.matches(term).count(),
                    uni_passage.matches(term).count(),
                    if uni_quote.is_empty() {
                        0
                    } else {
                        uni_quote.matches(term).count()
                    },
                )
            };

            let raw_tf = (2usize
                .saturating_mul(tf_h)
                .saturating_add(tf_p)
                .saturating_add(tf_q)) as f64;
            if raw_tf <= 0.0 {
                continue;
            }
            matched_any = true;
            let norm_tf = (raw_tf * 2.2) / (raw_tf + 1.2 * (0.25 + 0.75 * (doc_len / 64.0)));
            if is_cjk_function_particle(term) {
                score += 0.15 * norm_tf;
            } else {
                matched_substantive = true;
                score += 1.0 + norm_tf;
            }
        }

        if matched_any
            && (!require_substantive_if_present
                || !has_substantive_query_term
                || matched_substantive)
        {
            hits.push((idx, score));
        }
    }

    hits.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    hits.truncate(limit);
    hits
}

fn lowercase_unicode_if_needed(text: &str) -> Cow<'_, str> {
    if text.chars().any(|c| !c.is_ascii() && c.is_uppercase()) {
        Cow::Owned(text.to_lowercase())
    } else {
        Cow::Borrowed(text)
    }
}

pub(crate) fn is_cjk_function_particle(term: &str) -> bool {
    matches!(
        term,
        "的" | "了"
            | "吗"
            | "呢"
            | "吧"
            | "啊"
            | "在"
            | "是"
            | "有"
            | "和"
            | "与"
            | "及"
            | "或"
            | "把"
            | "被"
            | "让"
            | "给"
            | "对"
            | "从"
            | "向"
            | "到"
            | "里"
            | "上"
            | "下"
            | "中"
            | "个"
            | "这"
            | "那"
            | "我"
            | "你"
            | "他"
            | "她"
            | "它"
            | "们"
    )
}

/// Extracts ASCII identifier, `snake_case`, and `camelCase` tokens with exact parity to `kp_store::Tokenizer`.
pub(crate) fn extract_ascii_tokens(text: &str) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::<String, usize>::new();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if !(bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
            continue;
        }
        let start = i;
        let mut has_alnum = false;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            if bytes[i].is_ascii_alphanumeric() {
                has_alnum = true;
            }
            i += 1;
        }
        if !has_alnum {
            continue;
        }
        let span = &text[start..i];
        let whole = span.to_ascii_lowercase();
        *counts.entry(whole.clone()).or_insert(0) += 1;
        for part in span.split('_').filter(|p| !p.is_empty()) {
            let pbytes = part.as_bytes();
            let mut pstart = 0usize;
            for idx in 1..pbytes.len() {
                let boundary = pbytes[idx].is_ascii_uppercase()
                    && (pbytes[idx - 1].is_ascii_lowercase()
                        || pbytes[idx - 1].is_ascii_digit()
                        || (pbytes[idx - 1].is_ascii_uppercase()
                            && pbytes.get(idx + 1).is_some_and(u8::is_ascii_lowercase)));
                if boundary {
                    let sub = part[pstart..idx].to_ascii_lowercase();
                    if sub != whole {
                        *counts.entry(sub).or_insert(0) += 1;
                    }
                    pstart = idx;
                }
            }
            let sub = part[pstart..].to_ascii_lowercase();
            if sub != whole {
                *counts.entry(sub).or_insert(0) += 1;
            }
        }
    }
    counts
}
