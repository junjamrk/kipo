use std::collections::BTreeSet;
use std::path::Path;

use kp_model::{CommandGuardSpec, FactKind, KpError};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractedFact {
    pub subject: String,
    pub predicate: String,
    pub value: Value,
    #[serde(default = "observation")]
    pub kind: FactKind,
    #[serde(default)]
    pub entities: BTreeSet<String>,
    #[serde(default)]
    pub tags: BTreeSet<String>,
}

fn observation() -> FactKind {
    FactKind::Observation
}

pub trait Extractor {
    fn extract(
        &self,
        markdown: &str,
        default_subject: &str,
    ) -> kp_store::Result<Vec<ExtractedFact>>;

    fn extract_with_media_type(
        &self,
        content: &str,
        uri: &str,
        media_type: Option<&str>,
    ) -> kp_store::Result<Vec<ExtractedFact>> {
        let _ = media_type;
        self.extract(content, uri)
    }

    fn extract_chunk(
        &self,
        content: &str,
        uri: &str,
        heading: Option<&str>,
        media_type: Option<&str>,
    ) -> kp_store::Result<Vec<ExtractedFact>> {
        let _ = heading;
        self.extract_with_media_type(content, uri, media_type)
    }
}

#[derive(Default)]
pub struct RuleExtractor;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StructuredBlockKind {
    Kp,
    Json,
}

struct OpenFence {
    marker: char,
    len: usize,
    structured: Option<(StructuredBlockKind, String)>,
}

impl Extractor for RuleExtractor {
    fn extract(
        &self,
        markdown: &str,
        default_subject: &str,
    ) -> kp_store::Result<Vec<ExtractedFact>> {
        self.extract_with_media_type(markdown, default_subject, None)
    }

    fn extract_with_media_type(
        &self,
        content: &str,
        uri: &str,
        media_type: Option<&str>,
    ) -> kp_store::Result<Vec<ExtractedFact>> {
        self.extract_chunk(content, uri, None, media_type)
    }

    fn extract_chunk(
        &self,
        content: &str,
        uri: &str,
        heading: Option<&str>,
        media_type: Option<&str>,
    ) -> kp_store::Result<Vec<ExtractedFact>> {
        if is_json_source(uri, media_type) {
            return structured(content.trim(), uri);
        }
        if is_non_markdown_structured_skip(uri, media_type) {
            return Ok(Vec::new());
        }

        let mut facts = Vec::new();
        let all_lines: Vec<&str> = content.lines().collect();
        let start_idx = parse_frontmatter(&all_lines, uri, &mut facts)?;

        let mut subject = heading
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .unwrap_or(uri)
            .to_owned();
        let mut fence: Option<OpenFence> = None;

        for &raw_line in &all_lines[start_idx..] {
            let line = raw_line.trim();
            if let Some(active) = &mut fence {
                if is_fence_close(line, active.marker, active.len) {
                    if let Some((kind, block)) = active.structured.take() {
                        match kind {
                            StructuredBlockKind::Kp => {
                                facts.extend(structured(&block, uri)?);
                            }
                            StructuredBlockKind::Json => {
                                if let Ok(value) = serde_json::from_str::<Value>(&block) {
                                    if looks_like_kp_fact_json(&value) {
                                        if let Ok(parsed) = structured_value(value, uri) {
                                            facts.extend(parsed);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    fence = None;
                } else if let Some((_, block)) = &mut active.structured {
                    block.push_str(raw_line);
                    block.push('\n');
                }
                continue;
            }

            if let Some((marker, len, info)) = parse_fence_open(line) {
                let structured = match info {
                    "kp" => Some((StructuredBlockKind::Kp, String::new())),
                    "json" => Some((StructuredBlockKind::Json, String::new())),
                    _ => None,
                };
                fence = Some(OpenFence {
                    marker,
                    len,
                    structured,
                });
                continue;
            }

            if let Some(heading) = line.strip_prefix('#') {
                let heading = heading.trim_start_matches('#').trim();
                if !heading.is_empty() {
                    subject = heading.to_owned();
                }
                continue;
            }

            let text = strip_single_bullet(line);
            if text.is_empty() {
                continue;
            }

            let (kind, predicate, body) = match text.split_once(':') {
                Some(("fact_slot" | "fact", rest)) => {
                    (FactKind::FactSlot, "kp.core.fact", rest.trim())
                }
                Some(("event", rest)) => (FactKind::Event, "kp.core.schedule", rest.trim()),
                Some(("constraint", rest)) => {
                    (FactKind::Constraint, "kp.core.constraint", rest.trim())
                }
                Some(("decision", rest)) => (FactKind::Decision, "kp.core.decision", rest.trim()),
                Some(("closed_negative", rest)) => {
                    (FactKind::ClosedNegative, "kp.code.pitfall", rest.trim())
                }
                Some(("observation", rest)) => {
                    (FactKind::Observation, "kp.core.observation", rest.trim())
                }
                _ => continue,
            };
            if body.is_empty() {
                continue;
            }

            let entities = mentions(body);
            let mut fact = ExtractedFact {
                subject: subject.clone(),
                predicate: predicate.into(),
                value: Value::String(body.into()),
                kind,
                entities,
                tags: BTreeSet::new(),
            };
            if matches!(kind, FactKind::Constraint | FactKind::Decision) {
                let (guard_spec, cleaned_prose) = parse_bracket_command_guard_tags(body);
                if !guard_spec.is_empty() {
                    let summary = if cleaned_prose.is_empty() {
                        body.to_owned()
                    } else {
                        cleaned_prose
                    };
                    let mut map = serde_json::Map::new();
                    map.insert("summary".into(), Value::String(summary));
                    if guard_spec.has_enforced_guard() {
                        map.insert("hard_guard".into(), json!(true));
                    }
                    map.insert(
                        "forbidden_commands".into(),
                        serde_json::to_value(&guard_spec.forbidden_commands)?,
                    );
                    map.insert(
                        "allowed_commands".into(),
                        serde_json::to_value(&guard_spec.allowed_commands)?,
                    );
                    map.insert(
                        "required_flags".into(),
                        serde_json::to_value(&guard_spec.required_flags)?,
                    );
                    fact.value = Value::Object(map);
                }
            } else if let Some((slot_predicate, slot_value)) = body.split_once('=') {
                if kind == FactKind::FactSlot && !slot_predicate.trim().is_empty() {
                    fact.predicate = slot_predicate.trim().into();
                    fact.value = parse_value(slot_value.trim());
                }
            }
            facts.push(fact);
        }

        if let Some(active) = fence {
            if matches!(active.structured, Some((StructuredBlockKind::Kp, _))) {
                return Err(KpError::invalid("extraction", "unclosed structured block").into());
            }
        }
        Ok(facts)
    }
}

fn is_json_source(uri: &str, media_type: Option<&str>) -> bool {
    if let Some(mt) = media_type {
        let base = mt.split(';').next().unwrap_or(mt).trim();
        return base.eq_ignore_ascii_case("application/json")
            || base.to_ascii_lowercase().ends_with("+json");
    }
    Path::new(uri)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
}

fn is_non_markdown_structured_skip(uri: &str, media_type: Option<&str>) -> bool {
    if let Some(mt) = media_type {
        let base = mt.split(';').next().unwrap_or(mt).trim();
        return !base.eq_ignore_ascii_case("text/markdown")
            && !base.eq_ignore_ascii_case("text/plain");
    }
    Path::new(uri)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "rs" | "py"
                    | "ts"
                    | "tsx"
                    | "js"
                    | "jsx"
                    | "go"
                    | "csv"
                    | "vtt"
                    | "srt"
                    | "pdf"
                    | "docx"
                    | "pptx"
                    | "xlsx"
            )
        })
}

fn parse_frontmatter(
    lines: &[&str],
    uri: &str,
    facts: &mut Vec<ExtractedFact>,
) -> kp_store::Result<usize> {
    if lines.first().copied() != Some("---") {
        return Ok(0);
    }
    let close_idx = lines[1..]
        .iter()
        .position(|line| line.trim() == "---")
        .map(|pos| pos + 1);

    let Some(close_idx) = close_idx else {
        let has_kv = lines[1..].iter().any(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && !trimmed.starts_with('#') && trimmed.contains(':')
        });
        if has_kv {
            return Err(KpError::invalid("frontmatter", "missing closing delimiter").into());
        }
        return Ok(1);
    };

    let mut fields = serde_json::Map::new();
    let mut invalid_line = false;
    for &line in &lines[1..close_idx] {
        if line.trim().is_empty() {
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            fields.insert(key.trim().into(), parse_value(value.trim()));
        } else {
            invalid_line = true;
        }
    }

    let has_kp_keys = fields.contains_key("facts")
        || fields.contains_key("subject")
        || fields.contains_key("predicate");
    if has_kp_keys {
        if invalid_line {
            return Err(KpError::invalid("frontmatter", "expected key: JSON-or-string").into());
        }
        facts.extend(structured_value(Value::Object(fields), uri)?);
    }
    Ok(close_idx + 1)
}

fn parse_fence_open(trimmed: &str) -> Option<(char, usize, &str)> {
    let marker = trimmed.chars().next()?;
    if !matches!(marker, '`' | '~') {
        return None;
    }
    let len = trimmed.chars().take_while(|&c| c == marker).count();
    if len < 3 {
        return None;
    }
    let info = trimmed.trim_start_matches(marker).trim();
    if marker == '`' && info.contains('`') {
        return None;
    }
    Some((marker, len, info))
}

fn is_fence_close(trimmed: &str, marker: char, min_len: usize) -> bool {
    let len = trimmed.chars().take_while(|&c| c == marker).count();
    len >= min_len && trimmed.trim_start_matches(marker).trim().is_empty()
}

fn strip_single_bullet(line: &str) -> &str {
    if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
        rest.trim_start()
    } else {
        line
    }
}

fn looks_like_kp_fact_json(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.contains_key("facts")
                || map.contains_key("subject")
                || map.contains_key("predicate")
        }
        Value::Array(items) => !items.is_empty() && items.iter().all(looks_like_kp_fact_json),
        _ => false,
    }
}

fn structured(text: &str, uri: &str) -> kp_store::Result<Vec<ExtractedFact>> {
    let value: Value = serde_json::from_str(text)
        .map_err(|err| KpError::invalid_input_json(uri, err.to_string()))?;
    structured_value(value, uri)
}

fn structured_value(value: Value, uri: &str) -> kp_store::Result<Vec<ExtractedFact>> {
    let facts = match value {
        Value::Array(values) => values
            .into_iter()
            .map(serde_json::from_value)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|err| KpError::invalid_input_json(uri, err.to_string()))?,
        Value::Object(mut object) if object.contains_key("facts") => {
            let nested = object
                .remove("facts")
                .ok_or_else(|| KpError::invalid("extraction", "missing facts"))?;
            serde_json::from_value(nested)
                .map_err(|err| KpError::invalid_input_json(uri, err.to_string()))?
        }
        other => vec![serde_json::from_value(other)
            .map_err(|err| KpError::invalid_input_json(uri, err.to_string()))?],
    };
    Ok(facts)
}

fn parse_value(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.trim_matches('"').into()))
}

pub fn parse_bracket_command_guard_tags(body: &str) -> (CommandGuardSpec, String) {
    let mut spec = CommandGuardSpec::default();
    let mut prose = String::with_capacity(body.len());
    let mut idx = 0usize;

    while idx < body.len() {
        let rem = &body[idx..];
        if let Some(after_wiki) = rem.strip_prefix("[[") {
            if let Some(end_wiki) = after_wiki.find("]]") {
                let span_end = 2 + end_wiki + 2;
                prose.push_str(&rem[..span_end]);
                idx += span_end;
                continue;
            }
        }
        if let Some(after_open) = rem.strip_prefix('[') {
            if let Some(close_rel) = find_bracket_close_respecting_backticks(after_open) {
                let inside = &after_open[..close_rel];
                if let Some((raw_key, raw_vals)) = inside.split_once(':') {
                    let key_lc = raw_key.trim().to_ascii_lowercase();
                    if matches!(key_lc.as_str(), "forbid" | "allow" | "require_flag") {
                        let parsed_vals = split_tag_items(raw_vals);
                        match key_lc.as_str() {
                            "forbid" => spec.forbidden_commands.extend(parsed_vals),
                            "allow" => spec.allowed_commands.extend(parsed_vals),
                            "require_flag" => spec.required_flags.extend(parsed_vals),
                            _ => {}
                        }
                        idx += 1 + close_rel + 1;
                        continue;
                    }
                }
            }
        }
        let ch = rem.chars().next().unwrap_or('\0');
        prose.push(ch);
        idx += ch.len_utf8();
    }

    let cleaned = prose.split_whitespace().collect::<Vec<_>>().join(" ");
    (spec.normalized(), cleaned)
}

fn find_bracket_close_respecting_backticks(s: &str) -> Option<usize> {
    let mut in_backtick = false;
    for (idx, ch) in s.char_indices() {
        match ch {
            '`' => in_backtick = !in_backtick,
            ']' if !in_backtick => return Some(idx),
            _ => {}
        }
    }
    None
}

fn split_tag_items(raw: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut cur = String::new();
    let mut in_backtick = false;
    for ch in raw.chars() {
        match ch {
            '`' => {
                in_backtick = !in_backtick;
                cur.push(ch);
            }
            ',' if !in_backtick => {
                if let Some(clean) = clean_tag_token(&cur) {
                    items.push(clean);
                }
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    if let Some(clean) = clean_tag_token(&cur) {
        items.push(clean);
    }
    items
}

fn clean_tag_token(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let unquoted = trimmed
        .strip_prefix('`')
        .and_then(|s| s.strip_suffix('`'))
        .or_else(|| trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
        })
        .unwrap_or(trimmed)
        .trim();
    (!unquoted.is_empty()).then(|| unquoted.to_owned())
}

fn mentions(text: &str) -> BTreeSet<String> {
    let mut entities = BTreeSet::new();
    let mut remaining = text;
    while let Some((_, tail)) = remaining.split_once("[[") {
        let Some((entity, rest)) = tail.split_once("]]") else {
            break;
        };
        if !entity.trim().is_empty() {
            entities.insert(entity.trim().to_lowercase());
        }
        remaining = rest;
    }
    entities
}

#[cfg(test)]
#[path = "extract_tests.rs"]
mod tests;
