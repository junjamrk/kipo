use std::collections::BTreeSet;

/// Numeric references use one-based positions in this exact returned candidate list.
pub fn sanitize_citations(answer_text: &str, allowed_candidate_ids: &[String]) -> String {
    let allowed: BTreeSet<_> = allowed_candidate_ids.iter().map(String::as_str).collect();
    let mut result = String::new();
    let mut rest = answer_text;
    while let Some((before, tail)) = rest.split_once('[') {
        result.push_str(before);
        let Some((citation, after)) = tail.split_once(']') else {
            result.push('[');
            result.push_str(tail);
            return result;
        };
        if let Some(nested) = citation.find('[') {
            result.push('[');
            result.push_str(&tail[..nested]);
            rest = &tail[nested..];
            continue;
        }
        let keep = if let Some(id) = citation.strip_prefix("kp-ref:") {
            allowed.contains(id)
        } else if !citation.is_empty() && citation.bytes().all(|b| b.is_ascii_digit()) {
            citation
                .parse::<usize>()
                .is_ok_and(|n| n > 0 && n <= allowed_candidate_ids.len())
        } else {
            true
        };
        if keep {
            result.push('[');
            result.push_str(citation);
            result.push(']');
        }
        rest = after;
    }
    result.push_str(rest);
    result
}

#[cfg(test)]
#[path = "citations_tests.rs"]
mod tests;
