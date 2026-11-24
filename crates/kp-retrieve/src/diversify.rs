// SAG-Wiki sag2_v10.rs greedy Gram-Schmidt DPP (MIT); see THIRD_PARTY_NOTICES.md.
use std::collections::BTreeMap;

use kp_store::{Tokenizer, VisibleDocument};

use crate::expansion::Vocabulary;

pub(crate) fn diversify(
    order: &mut [usize],
    scores: &[f64],
    documents: &[VisibleDocument],
    vocabulary: &Vocabulary,
    tokenizer: &Tokenizer,
    protected: usize,
) {
    let n = order.len().min(20);
    if n == 0 {
        return;
    }
    let vectors: Vec<BTreeMap<String, f64>> = order[..n]
        .iter()
        .map(|&i| {
            let mut counts = BTreeMap::<String, usize>::new();
            for term in tokenizer.tokenize(&documents[i].passage) {
                *counts.entry(term).or_default() += 1;
            }
            let mut vector: BTreeMap<_, _> = counts
                .into_iter()
                .map(|(term, count)| {
                    let weight = (1.0 + (count as f64).ln()) * vocabulary.idf(&term, tokenizer);
                    (term, weight)
                })
                .collect();
            let norm = vector.values().map(|v| v * v).sum::<f64>().sqrt();
            if norm > 0.0 {
                for value in vector.values_mut() {
                    *value /= norm;
                }
            }
            vector
        })
        .collect();
    let mut residual: Vec<_> = order[..n].iter().map(|&i| scores[i].powi(2)).collect();
    let mut kernel = vec![vec![0.0; n]; n];
    for i in 0..n {
        for j in 0..n {
            kernel[i][j] = if i == j {
                residual[i]
            } else {
                scores[order[i]]
                    * scores[order[j]]
                    * vectors[i]
                        .iter()
                        .map(|(t, v)| v * vectors[j].get(t).copied().unwrap_or_default())
                        .sum::<f64>()
            };
        }
    }
    let maximum = residual.iter().copied().fold(0.0, f64::max);
    let tolerance = n as f64 * (f64::from_bits(maximum.to_bits() + 1) - maximum);
    let mut coefficients = vec![Vec::<f64>::new(); n];
    let mut remaining: Vec<_> = (0..n).collect();
    let mut selected = Vec::new();
    while !remaining.is_empty() {
        let winner = if selected.len() < protected.min(n) {
            selected.len()
        } else {
            let Some(&winner) = remaining
                .iter()
                .max_by(|&&a, &&b| residual[a].total_cmp(&residual[b]).then(b.cmp(&a)))
            else {
                break;
            };
            winner
        };
        selected.push(winner);
        remaining.retain(|&i| i != winner);
        if residual[winner] <= tolerance {
            continue;
        }
        let denominator = residual[winner].sqrt();
        for &i in &remaining {
            let projection = (kernel[winner][i]
                - coefficients[winner]
                    .iter()
                    .zip(&coefficients[i])
                    .map(|(a, b)| a * b)
                    .sum::<f64>())
                / denominator;
            coefficients[i].push(projection);
            residual[i] = (residual[i] - projection * projection).max(0.0);
        }
    }
    let original = order[..n].to_vec();
    for (position, index) in selected.into_iter().enumerate() {
        order[position] = original[index];
    }
}

#[cfg(test)]
#[path = "diversify_tests.rs"]
mod tests;
