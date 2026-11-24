use kp_model::{
    ActorId, EpochMs, Fact, FactAccess, FactKind, FactStatus, LibraryId, MembershipRelation,
    ReviewStatus,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::Result;

pub(crate) fn lookup_scope_label(db: &Connection, scope_id: &str) -> Result<Option<String>> {
    let row: Option<(String, String, String)> = db
        .prepare_cached(
            "SELECT label, provider, external_container_id FROM source_scopes WHERE scope_id = ?1",
        )?
        .query_row([scope_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    Ok(row.map(|(label, provider, ext_id)| {
        if !label.trim().is_empty() {
            label
        } else {
            format!("{provider}:{ext_id}")
        }
    }))
}

pub(crate) fn resolve_fact_source_label(
    db: &Connection,
    local_library_id: &LibraryId,
    fact: &Fact,
) -> Result<String> {
    if let Some(scope_id) = &fact.source_scope_id {
        if let Some(label) = lookup_scope_label(db, scope_id.as_str())? {
            if !label.trim().is_empty() {
                return Ok(label);
            }
        }
    }
    if let Some(anchor) = &fact.source_anchor {
        if let Some(source_id) = &anchor.source_id {
            let uri_or_scope: Option<(String, Option<String>)> = db
                .query_row(
                    "SELECT uri, source_scope_id FROM raw_sources WHERE source_id = ?1",
                    [source_id.as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((uri, scope_id)) = uri_or_scope {
                if let Some(sid) = scope_id {
                    if let Some(label) = lookup_scope_label(db, &sid)? {
                        if !label.trim().is_empty() {
                            return Ok(label);
                        }
                    }
                }
                if !uri.trim().is_empty() {
                    return Ok(uri);
                }
            }
        }
        if let Some(path) = &anchor.file_path {
            if !path.trim().is_empty() {
                return Ok(path.clone());
            }
        }
    }
    if uuid::Uuid::parse_str(fact.origin_library.as_str()).is_ok() {
        return Ok(if &fact.origin_library == local_library_id {
            "local".to_owned()
        } else {
            "community".to_owned()
        });
    }
    Ok(fact.origin_library.to_string())
}

pub(super) fn fact_reference_time(fact: &Fact, fallback: EpochMs) -> EpochMs {
    match &fact.access {
        FactAccess::Base { stamp, .. } => {
            if stamp.recorded_at.0 != 0 {
                stamp.recorded_at
            } else if stamp.valid.valid_from.0 != 0 {
                stamp.valid.valid_from
            } else {
                fallback
            }
        }
        FactAccess::Derived { recorded_at, .. } => {
            if recorded_at.0 != 0 {
                *recorded_at
            } else {
                fallback
            }
        }
    }
}

pub(super) fn premise_display_order(kind: FactKind) -> u8 {
    match kind {
        FactKind::Constraint | FactKind::Decision | FactKind::ClosedNegative => 0,
        FactKind::Event | FactKind::Observation | FactKind::PlaybookBullet => 1,
        FactKind::Membership => 2,
        _ => 3,
    }
}

fn render_membership_sentence(db: &Connection, fact: &Fact) -> Result<Option<String>> {
    if fact.kind != FactKind::Membership {
        return Ok(None);
    }
    let Ok(rel) = serde_json::from_value::<MembershipRelation>(fact.object.clone()) else {
        return Ok(None);
    };
    let group_label: Option<String> = db
        .prepare_cached("SELECT canonical_label FROM community_concepts WHERE concept_id = ?1")?
        .query_row([rel.group_concept_id.as_str()], |r| r.get(0))
        .optional()?;
    let clean_label = group_label
        .map(|l| collapse_inline(&strip_turn_tags_and_assistant_lines(&l)))
        .filter(|l| !l.is_empty());
    Ok(Some(match clean_label {
        Some(lbl) => format!("{} 属于 {} 的「{lbl}」", rel.member, rel.anchor),
        None => format!("{} 属于 {} 的关联成员组", rel.member, rel.anchor),
    }))
}

pub(crate) fn render_premise_human_quote(db: &Connection, fact: &Fact) -> Result<String> {
    if let Some(raw_quote) = fact
        .source_anchor
        .as_ref()
        .and_then(|a| a.verbatim_quote.as_deref())
    {
        let clean_quote = collapse_inline(&strip_turn_tags_and_assistant_lines(raw_quote));
        if !clean_quote.is_empty() {
            return Ok(clean_quote);
        }
    }
    if let Some(mem) = render_membership_sentence(db, fact)? {
        return Ok(mem);
    }
    render_fact_claim_text(db, fact)
}

pub(crate) fn render_fact_claim_text(db: &Connection, fact: &Fact) -> Result<String> {
    if let Some(quote) = fact
        .source_anchor
        .as_ref()
        .and_then(|a| a.verbatim_quote.as_deref())
        .map(strip_turn_tags_and_assistant_lines)
        .filter(|q| !q.trim().is_empty())
    {
        if matches!(
            fact.kind,
            FactKind::Constraint
                | FactKind::Decision
                | FactKind::Event
                | FactKind::Observation
                | FactKind::Membership
        ) {
            let q = collapse_inline(&quote);
            if let Some(summary) = extract_human_value_text(&fact.object) {
                let s = collapse_inline(&strip_turn_tags_and_assistant_lines(&summary));
                if s.is_empty() {
                    return Ok(q);
                }
                if s == q || s.contains(&q) {
                    return Ok(s);
                }
                return Ok(format!("{s}（原话：{q}）"));
            }
            return Ok(q);
        }
    }
    if let Some(mem) = render_membership_sentence(db, fact)? {
        return Ok(mem);
    }
    let clean_subject = collapse_inline(&strip_turn_tags_and_assistant_lines(&fact.subject));
    let mut base = if let Some(summary) = extract_human_value_text(&fact.object) {
        let s = collapse_inline(&strip_turn_tags_and_assistant_lines(&summary));
        if s.is_empty() {
            clean_subject
        } else if clean_subject.is_empty()
            || s.to_lowercase().contains(&clean_subject.to_lowercase())
        {
            s
        } else {
            format!("{clean_subject}: {s}")
        }
    } else {
        format!(
            "{} {}: {}",
            clean_subject,
            collapse_inline(&fact.predicate),
            collapse_inline(&strip_turn_tags_and_assistant_lines(
                &fact.object.to_string()
            ))
        )
    };
    if fact.kind == FactKind::SkillVersion && !crate::skills::locally_approved(db, fact)? {
        base.push(' ');
        base.push_str(kp_model::READ_ONLY_SKILL);
    }
    Ok(base)
}

fn extract_human_value_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        Value::Object(map) => {
            for key in [
                "summary",
                "claim_text",
                "advice",
                "claim",
                "reason",
                "rule",
                "message",
                "text",
                "description",
            ] {
                if let Some(s) = map.get(key).and_then(Value::as_str) {
                    if !s.trim().is_empty() {
                        return Some(s.to_owned());
                    }
                }
            }
            if let (Some(ev), Some(con)) = (map.get("event"), map.get("constraint")) {
                let ev_txt = extract_human_value_text(ev);
                let con_txt = extract_human_value_text(con);
                match (ev_txt, con_txt) {
                    (Some(e), Some(c)) => return Some(format!("{e} — {c}")),
                    (Some(e), None) => return Some(e),
                    (None, Some(c)) => return Some(c),
                    (None, None) => {}
                }
            }
            None
        }
        _ => None,
    }
}

/// Defense-in-depth sanitizer for `<kp-context>` prompt candidates and `PreToolUse` guard reasons:
/// - Drops any line whose trimmed prefix (after optional `[turn:N]`) is `[assistant]`, `[system]`,
///   `[developer]`, or `[tool]` (plus inline `[assistant]` segments in pre-collapsed transcripts).
/// - Strips leading `[turn:N]`, `[speaker:<id>]`, and `[user]` markers on human lines.
pub fn strip_turn_tags_and_assistant_lines(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.starts_with('「') && trimmed.ends_with('」') && trimmed.chars().count() >= 2 {
        let inner = &trimmed['「'.len_utf8()..trimmed.len() - '」'.len_utf8()];
        let cleaned = strip_turn_tags_and_assistant_lines(inner);
        return if cleaned.is_empty() {
            String::new()
        } else {
            format!("「{cleaned}」")
        };
    }

    let mut kept_lines = Vec::new();
    for raw_line in text.lines() {
        let mut line_parts = Vec::new();
        for seg in split_inline_transcript_segments(raw_line) {
            if let Some(clean_seg) = sanitize_single_turn_segment(seg) {
                line_parts.push(clean_seg);
            }
        }
        if !line_parts.is_empty() {
            kept_lines.push(line_parts.join(" "));
        }
    }
    kept_lines.join("\n")
}

fn split_inline_transcript_segments(line: &str) -> Vec<&str> {
    let mut splits = vec![0usize];
    let mut prev_was_ws = false;
    for (idx, ch) in line.char_indices() {
        if idx > 0 && prev_was_ws && ch == '[' && is_inline_turn_split_marker(&line[idx..]) {
            splits.push(idx);
        }
        prev_was_ws = ch.is_whitespace();
    }
    let mut out = Vec::with_capacity(splits.len());
    for (i, &start) in splits.iter().enumerate() {
        let end = splits.get(i + 1).copied().unwrap_or(line.len());
        out.push(&line[start..end]);
    }
    out
}

fn is_inline_turn_split_marker(rest: &str) -> bool {
    strip_leading_turn_marker(rest).is_some()
        || strip_leading_speaker_marker(rest).is_some()
        || strip_leading_role_marker(rest, "assistant").is_some()
}

fn sanitize_single_turn_segment(seg: &str) -> Option<String> {
    let mut s = seg.trim();
    while let Some(next) = strip_leading_turn_marker(s) {
        s = next;
    }
    if is_non_human_role_prefix(s) {
        return None;
    }
    loop {
        if let Some(next) = strip_leading_turn_marker(s) {
            s = next;
            continue;
        }
        if let Some(next) = strip_leading_speaker_marker(s) {
            s = next;
            continue;
        }
        if let Some(next) = strip_leading_role_marker(s, "user") {
            s = next;
            continue;
        }
        break;
    }
    if is_non_human_role_prefix(s) {
        return None;
    }
    let cleaned = s.trim();
    (!cleaned.is_empty()).then(|| cleaned.to_owned())
}

fn is_non_human_role_prefix(s: &str) -> bool {
    ["assistant", "system", "developer", "tool"]
        .iter()
        .any(|role| strip_leading_role_marker(s, role).is_some())
}

fn strip_leading_turn_marker(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    if bytes.len() < 8 || !bytes[..6].eq_ignore_ascii_case(b"[turn:") {
        return None;
    }
    let close_rel = s[6..].find(']')?;
    let digits = &s[6..6 + close_rel];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let after = &s[6 + close_rel + 1..];
    Some(after.strip_prefix(':').unwrap_or(after).trim_start())
}

fn strip_leading_speaker_marker(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    if bytes.len() < 11 || !bytes[..9].eq_ignore_ascii_case(b"[speaker:") {
        return None;
    }
    let close_rel = s[9..].find(']')?;
    let speaker_id = &s[9..9 + close_rel];
    if speaker_id.is_empty() || speaker_id.chars().any(char::is_whitespace) {
        return None;
    }
    let after = &s[9 + close_rel + 1..];
    Some(after.strip_prefix(':').unwrap_or(after).trim_start())
}

fn strip_leading_role_marker<'a>(s: &'a str, role: &str) -> Option<&'a str> {
    let tag_len = role.len() + 2;
    if s.len() < tag_len
        || s.as_bytes()[0] != b'['
        || !s
            .get(1..1 + role.len())
            .is_some_and(|r| r.eq_ignore_ascii_case(role))
        || s.as_bytes().get(1 + role.len()) != Some(&b']')
    {
        return None;
    }
    let after = &s[tag_len..];
    match after.chars().next() {
        None => Some(""),
        Some(':') => Some(after[1..].trim_start()),
        Some(ch) if ch.is_whitespace() => Some(after.trim_start()),
        Some(_) => None,
    }
}

pub(super) fn resolve_author_ambiguous_question(
    db: &Connection,
    viewer: &ActorId,
    fact: &Fact,
) -> Result<Option<String>> {
    if fact.status != FactStatus::Active
        || fact.access.expired_at().is_some()
        || fact.broader_scope.is_none()
    {
        return Ok(None);
    }
    let item: Option<(String, String)> = db
        .query_row(
            "SELECT status, question FROM review_items
             WHERE actor_id = ?1 AND kind = 'ambiguous_scope' AND target_id = ?2
             ORDER BY CASE WHEN status = 'open' THEN 0 ELSE 1 END, created_at DESC, review_id ASC
             LIMIT 1",
            params![viewer.as_str(), fact.id.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((status_str, question)) = item {
        let status: ReviewStatus = status_str.parse()?;
        return if status == ReviewStatus::Open {
            Ok(Some(collapse_inline(&question)))
        } else {
            Ok(None)
        };
    }
    Ok(fact
        .broader_scope
        .as_ref()
        .map(|b| collapse_inline(&b.question))
        .filter(|q| !q.is_empty()))
}

pub(super) fn collapse_inline(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
