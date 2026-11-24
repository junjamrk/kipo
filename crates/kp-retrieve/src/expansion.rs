// Adapted from SAG-Wiki core/src/retrieval.rs (MIT); see THIRD_PARTY_NOTICES.md.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use kp_store::{Tokenizer, VisibleDocument};
use regex::Regex;
use serde::Serialize;

use crate::{Result, RetrieveError};

static NAME_PHRASES: LazyLock<std::result::Result<Regex, regex::Error>> = LazyLock::new(|| {
    let word = r"(?:\p{Lu}\.|\p{Lu}[\p{L}\p{M}]*(?:[-’']\p{Lu}[\p{L}\p{M}]*)*)";
    Regex::new(&format!(r"\b{word}(?:[ \t]+{word})+"))
});

#[derive(Clone, Debug, Serialize)]
pub struct Clue {
    pub term: String,
    pub score: f64,
    pub name_phrase: bool,
}

pub(crate) struct Vocabulary {
    terms: Vec<BTreeSet<String>>,
}

impl Vocabulary {
    pub fn new(documents: &[VisibleDocument], tokenizer: &Tokenizer) -> Self {
        Self {
            terms: documents
                .iter()
                .map(|d| {
                    tokenizer
                        .tokenize(&format!("{} {}", d.heading, d.passage))
                        .into_iter()
                        .collect()
                })
                .collect(),
        }
    }

    pub fn df(&self, term: &str, tokenizer: &Tokenizer) -> usize {
        let words = tokenizer.tokenize(term);
        if words.is_empty() {
            return 0;
        }
        self.terms
            .iter()
            .filter(|terms| words.iter().all(|t| terms.contains(t)))
            .count()
    }

    pub fn idf(&self, term: &str, tokenizer: &Tokenizer) -> f64 {
        let df = self.df(term, tokenizer) as f64;
        (1.0 + (self.terms.len() as f64 - df + 0.5) / (df + 0.5)).ln()
    }
}

pub(crate) fn expansion_terms(
    query: &str,
    seeds: &[(&VisibleDocument, f64)],
    vocabulary: &Vocabulary,
    tokenizer: &Tokenizer,
) -> Result<Vec<Clue>> {
    let query_set: BTreeSet<_> = tokenizer.tokenize(query).into_iter().collect();
    let maximum = seeds.iter().map(|(_, score)| *score).fold(0.0, f64::max);
    let mut ranked = BTreeMap::<String, Clue>::new();
    for (seed, score) in seeds {
        let weight = 0.5 + 0.5 * if maximum > 0.0 { score / maximum } else { 0.0 };
        let mut counts = BTreeMap::<String, usize>::new();
        let mut contexts = BTreeMap::<String, f64>::new();
        let mut names = BTreeSet::new();
        for sentence in sentences(&seed.passage) {
            let sentence_words: BTreeSet<_> = tokenizer.tokenize(sentence).into_iter().collect();
            let context: f64 = sentence_words
                .intersection(&query_set)
                .map(|t| vocabulary.idf(t, tokenizer))
                .sum();
            let phrases = name_phrases(sentence, tokenizer)?;
            let mut words = tokenizer.tokenize(sentence);
            for phrase in phrases {
                names.insert(phrase.clone());
                words.push(phrase);
            }
            for word in words {
                *counts.entry(word.clone()).or_default() += 1;
                let current = contexts.entry(word).or_default();
                *current = current.max(context);
            }
        }
        for (term, tf) in counts {
            if tokenizer
                .tokenize(&term)
                .iter()
                .all(|t| query_set.contains(t))
                || vocabulary.df(&term, tokenizer) == 0
            {
                continue;
            }
            let score = (1.0 + (tf as f64).ln())
                * vocabulary.idf(&term, tokenizer)
                * (1.0 + contexts.get(&term).copied().unwrap_or_default())
                * weight;
            let clue = Clue {
                name_phrase: names.contains(&term),
                term: term.clone(),
                score,
            };
            if ranked.get(&term).is_none_or(|old| score > old.score) {
                ranked.insert(term, clue);
            }
        }
    }
    let mut clues: Vec<_> = ranked.into_values().collect();
    clues.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.term.cmp(&b.term)));
    clues.truncate(16);
    Ok(clues)
}

fn name_phrases(text: &str, tokenizer: &Tokenizer) -> Result<Vec<String>> {
    let pattern = NAME_PHRASES
        .as_ref()
        .map_err(|error| RetrieveError::Pattern(error.clone()))?;
    Ok(pattern
        .find_iter(text)
        .map(|m| tokenizer.tokenize(m.as_str()).join(" "))
        .collect())
}

fn sentences(text: &str) -> Vec<&str> {
    let mut start = 0;
    let mut result = Vec::new();
    for (index, ch) in text.char_indices() {
        let next = index + ch.len_utf8();
        let initial = ch == '.'
            && text[start..index]
                .split_whitespace()
                .next_back()
                .is_some_and(|s| s.chars().count() == 1 && s.chars().all(char::is_uppercase));
        let boundary = matches!(ch, '。' | '！' | '？' | '\n')
            || (matches!(ch, '.' | '!' | '?')
                && !initial
                && text[next..].chars().next().is_none_or(char::is_whitespace));
        if boundary {
            result.push(&text[start..next]);
            start = next;
        }
    }
    result.push(&text[start..]);
    result
}

#[cfg(test)]
#[path = "expansion_tests.rs"]
mod tests;
