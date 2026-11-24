use std::collections::{BTreeMap, BTreeSet};

use kp_model::{normalize_numeric_epoch, parse_timestamp_ms_in_tz, EpochMs, TimeZone};
use regex::Regex;
use serde_json::Value;

use crate::{ConnectError, ExternalIdentityProposal, Result};

pub(super) fn unwrap_data(val: &Value) -> &Value {
    val.get("data").unwrap_or(val)
}

pub(super) fn record_identity(
    proposals: &mut BTreeMap<String, ExternalIdentityProposal>,
    external_id: &str,
    display_name: &str,
) {
    let entry =
        proposals
            .entry(external_id.to_owned())
            .or_insert_with(|| ExternalIdentityProposal {
                provider: "feishu".into(),
                external_id: external_id.to_owned(),
                display_name: display_name.to_owned(),
                proposed_actor: None,
            });
    if entry.display_name == entry.external_id && display_name != external_id {
        entry.display_name = display_name.to_owned();
    }
}

pub(super) fn collect_permission_members(
    raw: &Value,
    roster_members: &mut BTreeSet<String>,
    proposals: &mut BTreeMap<String, ExternalIdentityProposal>,
) {
    let container = unwrap_data(raw);
    let Some(items) = container
        .get("items")
        .or_else(|| container.get("members"))
        .and_then(Value::as_array)
    else {
        return;
    };
    for item in items {
        let member_type = item
            .get("member_type")
            .and_then(Value::as_str)
            .unwrap_or("openid");
        if matches!(
            member_type,
            "openid" | "open_id" | "userid" | "user_id" | "user"
        ) {
            if let Some(id) = item
                .get("member_id")
                .or_else(|| item.get("open_id"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                let name = item.get("name").and_then(Value::as_str).unwrap_or(id);
                roster_members.insert(id.to_owned());
                record_identity(proposals, id, name);
            }
        }
    }
}

pub(super) fn extract_sender(
    msg: &Value,
    member_names: &BTreeMap<String, String>,
) -> (String, String) {
    let Some(sender) = msg.get("sender") else {
        return ("unknown".into(), "unknown".into());
    };
    let id = sender
        .get("id")
        .or_else(|| sender.pointer("/sender_id/open_id"))
        .or_else(|| sender.pointer("/sender_id/user_id"))
        .or_else(|| sender.get("open_id"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let name = sender
        .get("name")
        .or_else(|| sender.get("display_name"))
        .and_then(Value::as_str)
        .or_else(|| member_names.get(id).map(String::as_str))
        .unwrap_or(id);
    (id.to_owned(), name.to_owned())
}

pub(super) fn extract_field_epoch_ms(obj: &Value, field: &str) -> Option<EpochMs> {
    let v = obj.get(field)?;
    match v {
        Value::Number(n) => n.as_i64().map(normalize_numeric_epoch),
        Value::String(s) => parse_timestamp_ms_in_tz(s, &TimeZone::UTC),
        _ => None,
    }
}

/// Parses both `lark-cli im +chat-messages-list` top-level `content` strings AND raw Feishu OpenAPI
/// `body.content` escaped JSON strings (`text`, `post` 2D paragraph blocks, and `@_user_N` mentions).
pub(super) fn extract_message_text(msg: &Value) -> String {
    let raw = msg
        .pointer("/body/content")
        .or_else(|| msg.get("content"))
        .unwrap_or(&Value::Null);
    let mut text = match raw {
        Value::String(s) => {
            if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                extract_structured_feishu_body(&parsed).unwrap_or_else(|| s.clone())
            } else {
                s.clone()
            }
        }
        Value::Object(_) => extract_structured_feishu_body(raw).unwrap_or_default(),
        _ => String::new(),
    };

    if let Some(mentions) = msg.get("mentions").and_then(Value::as_array) {
        for m in mentions {
            if let (Some(key), Some(name)) = (
                m.get("key").and_then(Value::as_str),
                m.get("name").and_then(Value::as_str),
            ) {
                if !key.is_empty() && text.contains(key) {
                    text = text.replace(key, &format!("@{name}"));
                }
            }
        }
    }
    text
}

fn extract_structured_feishu_body(parsed: &Value) -> Option<String> {
    if let Some(text) = parsed.get("text").and_then(Value::as_str) {
        return Some(text.to_owned());
    }
    let post_root = if parsed.get("content").is_some() || parsed.get("title").is_some() {
        Some(parsed)
    } else {
        parsed
            .get("zh_cn")
            .or_else(|| parsed.get("en_us"))
            .or_else(|| parsed.get("ja_jp"))
    };
    if let Some(post) = post_root {
        let mut lines = Vec::new();
        if let Some(title) = post
            .get("title")
            .and_then(Value::as_str)
            .filter(|t| !t.trim().is_empty())
        {
            lines.push(title.to_owned());
        }
        if let Some(paragraphs) = post.get("content").and_then(Value::as_array) {
            for para in paragraphs {
                if let Some(elems) = para.as_array() {
                    let mut line = String::new();
                    for elem in elems {
                        match elem.get("tag").and_then(Value::as_str).unwrap_or("text") {
                            "text" | "a" | "code_block" => {
                                if let Some(t) = elem.get("text").and_then(Value::as_str) {
                                    line.push_str(t);
                                }
                            }
                            "at" => {
                                let name = elem
                                    .get("user_name")
                                    .or_else(|| elem.get("user_id"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("all");
                                line.push('@');
                                line.push_str(name);
                            }
                            _ => {}
                        }
                    }
                    if !line.trim().is_empty() {
                        lines.push(line);
                    }
                }
            }
        }
        if !lines.is_empty() {
            return Some(lines.join("\n"));
        }
    }
    None
}

/// Extracts readable Markdown/text from `lark-cli docs +fetch` (`data.document.content` DocxXML or
/// `data.markdown`) as well as OpenAPI `docx/v1/documents/:id/raw_content` (`data.content`).
pub(super) fn extract_document_markdown(raw: &Value, fallback_title: &str) -> Result<String> {
    let container = unwrap_data(raw);
    if let Some(md) = container
        .get("markdown")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        return Ok(md.to_owned());
    }
    if let Some(content) = container
        .pointer("/document/content")
        .or_else(|| container.get("content"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        if content.contains('<') && content.contains('>') {
            return docx_xml_to_markdown(content);
        }
        return Ok(content.to_owned());
    }
    Ok(format!("# {fallback_title}"))
}

fn docx_xml_to_markdown(xml: &str) -> Result<String> {
    let with_headings = xml
        .replace("<title>", "# ")
        .replace("</title>", "\n\n")
        .replace("<h1>", "# ")
        .replace("</h1>", "\n\n")
        .replace("<h2>", "## ")
        .replace("</h2>", "\n\n")
        .replace("<h3>", "### ")
        .replace("</h3>", "\n\n")
        .replace("<li>", "- ")
        .replace("</li>", "\n")
        .replace("</p>", "\n")
        .replace("<br/>", "\n")
        .replace("<br />", "\n");
    let tag_re =
        Regex::new(r"<[^>]+>").map_err(|e| ConnectError::Transport(format!("xml regex: {e}")))?;
    let stripped = tag_re.replace_all(&with_headings, "");
    let unescaped = stripped
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    let compact: Vec<&str> = unescaped
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    Ok(compact.join("\n"))
}

/// Prefers verbatim `transcript` (loaded from `artifacts.transcript_file` or inline `transcript`)
/// ahead of `artifacts.summary`.
pub(super) fn extract_minutes_text(container: &Value, fallback_title: &str) -> String {
    if let Some(transcript) = container
        .get("transcript")
        .or_else(|| container.pointer("/notes/transcript"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        return transcript.to_owned();
    }
    if let Some(paragraphs) = container
        .get("paragraphs")
        .or_else(|| container.pointer("/notes/paragraphs"))
        .and_then(Value::as_array)
    {
        let lines: Vec<String> = paragraphs
            .iter()
            .filter_map(|p| {
                let speaker = p
                    .get("speaker")
                    .and_then(Value::as_str)
                    .unwrap_or("speaker");
                let text = p.get("text").and_then(Value::as_str)?;
                Some(format!("{speaker}: {text}"))
            })
            .collect();
        if !lines.is_empty() {
            return lines.join("\n");
        }
    }
    if let Some(summary) = container
        .pointer("/artifacts/summary")
        .or_else(|| container.get("summary"))
    {
        if let Some(prose) = flatten_summary_value(summary) {
            if !prose.trim().is_empty() {
                return prose;
            }
        }
    }
    format!("# {fallback_title}")
}

fn flatten_summary_value(val: &Value) -> Option<String> {
    match val {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().filter_map(flatten_summary_value).collect();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        Value::Object(map) => {
            let mut parts = Vec::new();
            for key in ["overview", "summary", "content", "text", "title"] {
                if let Some(v) = map.get(key).and_then(flatten_summary_value) {
                    parts.push(v);
                }
            }
            if parts.is_empty() {
                for v in map.values() {
                    if let Some(s) = flatten_summary_value(v) {
                        parts.push(s);
                    }
                }
            }
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        _ => None,
    }
}
