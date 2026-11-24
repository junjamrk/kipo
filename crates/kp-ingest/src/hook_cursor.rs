// Incremental session cursor tracking and anti-echo / host-boilerplate stripping adapted from
// claude-mem main (Apache-2.0); see THIRD_PARTY_NOTICES.md.
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use kp_model::{
    normalize_numeric_epoch, parse_timestamp_ms_in_tz, system_time_zone, ContentHash, EpochMs,
    TimeZone,
};
use kp_store::{SessionCursor, Store};
use serde_json::{json, Value};

use crate::{IngestError, Result};

pub use kp_retrieve::{estimate_tokens, is_low_signal_ack};

pub(crate) enum DeltaTranscript {
    Unchanged {
        self_echo_filtered: bool,
    },
    Slice {
        turns: Vec<(usize, u64, u64, Value)>,
        slice_start_byte: u64,
        slice_end_byte: u64,
        next_cursor: SessionCursor,
    },
}

/// Extracted speaker role, raw text, and optional event timestamp from a single Claude Code,
/// Codex 0.164, or Antigravity JSONL turn.
pub(crate) struct ParsedTurn {
    pub role: String,
    pub raw_text: String,
    pub meta_source: Value,
    pub timestamp_ms: Option<EpochMs>,
}

/// Strips `<kp-context>...</kp-context>` blocks and any line carrying `kp-ref:` citations.
pub fn filter_echo(text: &str) -> (String, bool) {
    let (without_blocks, mut filtered) = strip_tagged_block(text, "<kp-context>", "</kp-context>");
    let cleaned = without_blocks
        .split_inclusive('\n')
        .filter(|line| {
            let echo = line.contains("kp-ref:");
            filtered |= echo;
            !echo
        })
        .collect();
    (cleaned, filtered)
}

/// Strips host-injected non-human blocks from Claude Code, Codex 0.164, and Antigravity transcripts
/// so system instructions, reminders, compaction summaries, slash-command wrappers, and environment
/// blocks are never misattributed as human speech.
pub fn strip_host_system_blocks(text: &str) -> String {
    const TAG_PAIRS: [(&str, &str); 16] = [
        ("<system-reminder>", "</system-reminder>"),
        ("<command-message>", "</command-message>"),
        ("<command-name>", "</command-name>"),
        ("<command-args>", "</command-args>"),
        ("<local-command-stdout>", "</local-command-stdout>"),
        ("<local-command-stderr>", "</local-command-stderr>"),
        ("<local-command-caveat>", "</local-command-caveat>"),
        ("<user-prompt-submit-hook>", "</user-prompt-submit-hook>"),
        ("<environment_context>", "</environment_context>"),
        (
            "<guardian_tool_descriptions>",
            "</guardian_tool_descriptions>",
        ),
        ("<permissions instructions>", "</permissions instructions>"),
        ("<collaboration_mode>", "</collaboration_mode>"),
        ("<subagent_notification>", "</subagent_notification>"),
        ("<user_instructions>", "</user_instructions>"),
        ("<CONTEXT_SUMMARY>", "</CONTEXT_SUMMARY>"),
        ("<SYSTEM_MESSAGE>", "</SYSTEM_MESSAGE>"),
    ];
    let mut current = strip_agents_md_blocks(text);
    for (open, close) in TAG_PAIRS {
        if current.contains(open) || current.contains(close) {
            let (next, _) = strip_tagged_block(&current, open, close);
            current = next;
        }
    }
    if current.contains("<USER_REQUEST>") || current.contains("</USER_REQUEST>") {
        current = current
            .replace("<USER_REQUEST>", "")
            .replace("</USER_REQUEST>", "");
    }
    current
}

fn strip_agents_md_blocks(text: &str) -> String {
    let mut current = text.to_owned();
    loop {
        if let Some(hdr_idx) = current.find("# AGENTS.md instructions") {
            let after_hdr = &current[hdr_idx..];
            if let Some(close_rel) = after_hdr.find("</INSTRUCTIONS>") {
                let mut end = hdr_idx + close_rel + "</INSTRUCTIONS>".len();
                if current[end..].starts_with("\r\n") {
                    end += 2;
                } else if current[end..].starts_with('\n') {
                    end += 1;
                }
                current.replace_range(hdr_idx..end, "");
                continue;
            }
            current.truncate(hdr_idx);
            break;
        }
        break;
    }
    if current.contains("<INSTRUCTIONS>") || current.contains("</INSTRUCTIONS>") {
        let (next, _) = strip_tagged_block(&current, "<INSTRUCTIONS>", "</INSTRUCTIONS>");
        current = next;
    }
    current
}

fn strip_tagged_block(text: &str, open: &str, close: &str) -> (String, bool) {
    if !text.contains(open) && !text.contains(close) {
        return (text.to_owned(), false);
    }
    let mut filtered = false;
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(open) {
        filtered = true;
        out.push_str(&rest[..start]);
        let after_open = &rest[start + open.len()..];
        if let Some(end) = after_open.find(close) {
            let mut tail = &after_open[end + close.len()..];
            if let Some(stripped) = tail.strip_prefix("\r\n") {
                tail = stripped;
            } else if let Some(stripped) = tail.strip_prefix('\n') {
                tail = stripped;
            }
            rest = tail;
        } else {
            rest = "";
            break;
        }
    }
    if rest.contains(close) {
        filtered = true;
        out.push_str(&rest.replace(close, ""));
    } else {
        out.push_str(rest);
    }
    (out, filtered)
}

/// Extracts `(role, raw_text, meta_source, timestamp_ms)` from Claude Code, Codex 0.164,
/// Antigravity, or inline turn JSON using the system `TimeZone` for offsetless timestamps.
#[allow(dead_code)]
pub(crate) fn extract_turn(raw_turn: &Value) -> Option<ParsedTurn> {
    extract_turn_in_tz(raw_turn, &system_time_zone())
}

/// Extracts `(role, raw_text, meta_source, timestamp_ms)` from Claude Code, Codex 0.164,
/// Antigravity, or inline turn JSON, skipping `isMeta: true`, `developer`/`system` instructions,
/// reasoning items, and tool-call plumbing.
pub(crate) fn extract_turn_in_tz(raw_turn: &Value, tz: &TimeZone) -> Option<ParsedTurn> {
    if raw_turn.get("isMeta").and_then(Value::as_bool) == Some(true)
        || raw_turn.pointer("/message/isMeta").and_then(Value::as_bool) == Some(true)
    {
        return None;
    }
    if raw_turn.get("source").and_then(Value::as_str) == Some("SYSTEM") {
        return None;
    }
    let timestamp_ms = extract_turn_timestamp_ms(raw_turn, tz);

    if let Some(top_type) = raw_turn.get("type").and_then(Value::as_str) {
        match top_type {
            // Antigravity `.system_generated/logs/transcript.jsonl` steps
            "USER_INPUT" => {
                if raw_turn
                    .get("source")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s != "USER_EXPLICIT")
                {
                    return None;
                }
                let text = extract_text_value(raw_turn.get("content")?);
                return Some(ParsedTurn {
                    role: "user".into(),
                    raw_text: text,
                    meta_source: raw_turn.clone(),
                    timestamp_ms,
                });
            }
            "PLANNER_RESPONSE" => {
                let text = extract_text_value(raw_turn.get("content")?);
                return Some(ParsedTurn {
                    role: "assistant".into(),
                    raw_text: text,
                    meta_source: raw_turn.clone(),
                    timestamp_ms,
                });
            }
            // Claude Code, Codex & Antigravity non-conversation metadata/tool records
            "summary"
            | "file-history-snapshot"
            | "system"
            | "progress"
            | "queue-operation"
            | "session_meta"
            | "turn_context"
            | "compacted"
            | "GENERIC" => return None,
            // Codex `event_msg` records
            "event_msg" => {
                let payload = raw_turn.get("payload")?;
                let ptype = payload.get("type").and_then(Value::as_str)?;
                let role = match ptype {
                    "agent_message" => "assistant",
                    "user_message" => "user",
                    _ => return None,
                };
                let text = extract_payload_text(payload);
                return Some(ParsedTurn {
                    role: role.into(),
                    raw_text: text,
                    meta_source: payload.clone(),
                    timestamp_ms,
                });
            }
            // Codex 0.164 `response_item` records
            "response_item" => {
                let payload = raw_turn.get("payload")?;
                match payload.get("type").and_then(Value::as_str) {
                    Some("agent_message") => {
                        return Some(ParsedTurn {
                            role: "assistant".into(),
                            raw_text: extract_payload_text(payload),
                            meta_source: payload.clone(),
                            timestamp_ms,
                        });
                    }
                    Some("message") => {
                        let role = payload
                            .get("role")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        if matches!(role, "system" | "developer") {
                            return None;
                        }
                        return Some(ParsedTurn {
                            role: role.into(),
                            raw_text: extract_payload_text(payload),
                            meta_source: payload.clone(),
                            timestamp_ms,
                        });
                    }
                    Some(_) => return None,
                    None => {
                        let role = payload
                            .get("role")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        if matches!(role, "system" | "developer") {
                            return None;
                        }
                        return Some(ParsedTurn {
                            role: role.into(),
                            raw_text: extract_payload_text(payload),
                            meta_source: payload.clone(),
                            timestamp_ms,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    let inner = raw_turn
        .get("message")
        .or_else(|| raw_turn.get("payload"))
        .unwrap_or(raw_turn);
    let role = inner
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if matches!(role, "system" | "developer") {
        return None;
    }
    Some(ParsedTurn {
        role: role.into(),
        raw_text: extract_payload_text(inner),
        meta_source: inner.clone(),
        timestamp_ms,
    })
}

fn extract_turn_timestamp_ms(raw_turn: &Value, tz: &TimeZone) -> Option<EpochMs> {
    let mut containers = Vec::with_capacity(3);
    containers.push(raw_turn);
    if let Some(m) = raw_turn.get("message") {
        containers.push(m);
    }
    if let Some(p) = raw_turn.get("payload") {
        containers.push(p);
    }
    for container in containers {
        for key in ["timestamp", "created_at", "occurred_at"] {
            let Some(val) = container.get(key) else {
                continue;
            };
            match val {
                Value::String(s) => {
                    if let Some(ts) = parse_timestamp_ms_in_tz(s, tz) {
                        return Some(ts);
                    }
                }
                Value::Number(n) => {
                    if let Some(i) = n.as_i64() {
                        if i > 0 {
                            return Some(normalize_numeric_epoch(i));
                        }
                    } else if let Some(f) = n.as_f64() {
                        if f.is_finite() && f > 0.0 {
                            return Some(normalize_numeric_epoch(f.round() as i64));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    None
}

fn extract_payload_text(container: &Value) -> String {
    if let Some(content) = container.get("content") {
        let extracted = extract_text_value(content);
        if !extracted.is_empty() {
            return extracted;
        }
    }
    if let Some(msg) = container.get("message").and_then(Value::as_str) {
        return msg.to_owned();
    }
    if let Some(text) = container.get("text").and_then(Value::as_str) {
        return text.to_owned();
    }
    String::new()
}

fn extract_text_value(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| {
                b.get("type")
                    .and_then(Value::as_str)
                    .is_none_or(|t| matches!(t, "text" | "input_text" | "output_text"))
            })
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub(crate) fn save_cursor(
    store: &mut Store,
    cursor: &SessionCursor,
    mark_extracted: bool,
) -> Result<()> {
    let mut updated = cursor.clone();
    if mark_extracted {
        updated.last_extracted_offset = updated.last_byte_offset;
    }
    store.upsert_session_cursor(&updated)?;
    Ok(())
}

/// Returns the effective complete byte length of `file`, excluding any trailing half-written JSONL
/// line that has not yet been newline-terminated or closed as valid JSON by the host CLI.
pub fn effective_complete_transcript_len(file: &mut std::fs::File) -> Result<u64> {
    let total_len = file.metadata()?.len();
    effective_complete_prefix_len(file, total_len)
}

pub(crate) fn effective_complete_prefix_len(
    file: &mut std::fs::File,
    limit_len: u64,
) -> Result<u64> {
    let total_len = file.metadata()?.len();
    let bound = total_len.min(limit_len);
    if bound == 0 {
        return Ok(0);
    }

    let mut last_newline_plus_1 = 0_u64;
    let mut pos = bound;
    let mut chunk = [0_u8; 8192];
    while pos > 0 {
        let to_read = usize::try_from(pos.min(chunk.len() as u64)).unwrap_or(chunk.len());
        let chunk_start = pos - (to_read as u64);
        file.seek(SeekFrom::Start(chunk_start))?;
        file.read_exact(&mut chunk[..to_read])?;
        if let Some(rel) = chunk[..to_read].iter().rposition(|&b| b == b'\n') {
            last_newline_plus_1 = chunk_start + (rel as u64) + 1;
            break;
        }
        pos = chunk_start;
    }

    if last_newline_plus_1 == bound {
        return Ok(bound);
    }

    let tail_len_u64 = bound - last_newline_plus_1;
    let max_tail_bytes = u64::try_from(crate::MAX_INPUT_BYTES).unwrap_or(u64::MAX);
    if tail_len_u64 > max_tail_bytes {
        return Ok(last_newline_plus_1);
    }
    let Ok(tail_len) = usize::try_from(tail_len_u64) else {
        return Ok(last_newline_plus_1);
    };

    let mut tail = vec![0_u8; tail_len];
    file.seek(SeekFrom::Start(last_newline_plus_1))?;
    file.read_exact(&mut tail)?;

    if tail.iter().all(u8::is_ascii_whitespace) {
        return Ok(last_newline_plus_1);
    }

    if serde_json::from_slice::<Value>(&tail).is_ok() {
        return Ok(bound);
    }

    if last_newline_plus_1 > 0 && bound <= max_tail_bytes {
        let peek_len = usize::try_from(bound.min(64)).unwrap_or(64);
        let mut head = [0_u8; 64];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut head[..peek_len])?;
        if head[..peek_len]
            .iter()
            .find(|b| !b.is_ascii_whitespace())
            .copied()
            == Some(b'[')
        {
            if let Ok(full_len) = usize::try_from(bound) {
                let mut full = vec![0_u8; full_len];
                file.seek(SeekFrom::Start(0))?;
                file.read_exact(&mut full)?;
                if matches!(serde_json::from_slice::<Value>(&full), Ok(Value::Array(_))) {
                    return Ok(bound);
                }
            }
        }
    }

    Ok(last_newline_plus_1)
}

fn contains_echo_bytes(haystack: &[u8]) -> bool {
    const NEEDLES: [&[u8]; 3] = [b"kp-ref:", b"<kp-context>", b"</kp-context>"];
    NEEDLES
        .iter()
        .any(|n| haystack.windows(n.len()).any(|w| w == *n))
}

fn stream_inspect_prefix(
    path: &Path,
    file: &mut std::fs::File,
    len: u64,
) -> Result<(u64, String, bool)> {
    if len == 0 {
        return Ok((0, ContentHash::digest(b"").to_string(), false));
    }
    file.seek(SeekFrom::Start(0))?;
    let mut nonempty_lines = 0_u64;
    let mut line_has_non_ws = false;
    let mut has_echo = false;
    let mut echo_carry = Vec::<u8>::with_capacity(65_560);
    let mut utf8_carry = [0_u8; 4];
    let mut utf8_carry_len = 0_usize;
    let mut remaining = len;
    let mut buf = vec![0_u8; 65_540];

    while remaining > 0 {
        buf[..utf8_carry_len].copy_from_slice(&utf8_carry[..utf8_carry_len]);
        let max_read = usize::try_from(remaining.min(65_536)).unwrap_or(65_536);
        let n = file.read(&mut buf[utf8_carry_len..utf8_carry_len + max_read])?;
        if n == 0 {
            return Err(IngestError::Invalid("transcript truncated during read"));
        }
        remaining -= n as u64;

        if !has_echo {
            echo_carry.extend_from_slice(&buf[utf8_carry_len..utf8_carry_len + n]);
            if contains_echo_bytes(&echo_carry) {
                has_echo = true;
                echo_carry.clear();
            } else {
                let keep = echo_carry.len().min(16);
                let drain_to = echo_carry.len() - keep;
                echo_carry.drain(..drain_to);
            }
        }

        let total = utf8_carry_len + n;
        let valid_str = match std::str::from_utf8(&buf[..total]) {
            Ok(s) => {
                utf8_carry_len = 0;
                s
            }
            Err(err) if err.error_len().is_none() && remaining > 0 => {
                let valid = err.valid_up_to();
                let rem = total - valid;
                utf8_carry[..rem].copy_from_slice(&buf[valid..total]);
                utf8_carry_len = rem;
                std::str::from_utf8(&buf[..valid])
                    .map_err(|_| IngestError::Invalid("transcript must be UTF-8"))?
            }
            Err(_) => return Err(IngestError::Invalid("transcript must be UTF-8")),
        };

        let mut parts = valid_str.split('\n');
        if let Some(first) = parts.next() {
            if !first.trim().is_empty() {
                line_has_non_ws = true;
            }
            for part in parts {
                if line_has_non_ws {
                    nonempty_lines += 1;
                }
                line_has_non_ws = !part.trim().is_empty();
            }
        }
    }

    if utf8_carry_len > 0 {
        return Err(IngestError::Invalid("transcript must be UTF-8"));
    }
    if line_has_non_ws {
        nonempty_lines += 1;
    }
    let prefix_sha256 = kp_store::sha256_file_prefix(path, len)?
        .ok_or(IngestError::Invalid("transcript truncated during read"))?;
    Ok((nonempty_lines, prefix_sha256, has_echo))
}

pub(crate) fn read_delta_transcript(
    store: &mut Store,
    session_id: &str,
    payload: &Value,
    at: EpochMs,
    is_session_end: bool,
) -> Result<DeltaTranscript> {
    let client_kind = infer_client_kind(payload);
    let prev = store.get_session_cursor(session_id)?;

    if let Some(path_str) = payload.get("transcript_path").and_then(Value::as_str) {
        let path = Path::new(path_str);
        let mut file = std::fs::File::open(path)?;
        let effective_end = effective_complete_transcript_len(&mut file)?;

        let (start_byte, base_line) = match &prev {
            Some(c)
                if c.last_byte_offset > 0
                    && c.transcript_path.as_deref() == Some(path_str)
                    && effective_end >= c.last_byte_offset
                    && kp_store::sha256_file_prefix(path, c.last_byte_offset)?.as_deref()
                        == Some(c.prefix_sha256.as_str()) =>
            {
                if effective_end == c.last_byte_offset {
                    let (_, _, any_echo_in_full) =
                        stream_inspect_prefix(path, &mut file, effective_end)?;
                    let mut refreshed = c.clone();
                    refreshed.last_activity_at = at;
                    save_cursor(store, &refreshed, is_session_end)?;
                    return Ok(DeltaTranscript::Unchanged {
                        self_echo_filtered: any_echo_in_full,
                    });
                }
                (
                    c.last_byte_offset,
                    usize::try_from(c.last_line_count).unwrap_or(0),
                )
            }
            _ => (0_u64, 0_usize),
        };

        let delta_len = usize::try_from(effective_end - start_byte)
            .map_err(|_| IngestError::Invalid("transcript slice exceeds addressable memory"))?;
        let mut delta_bytes = vec![0_u8; delta_len];
        if delta_len > 0 {
            file.seek(SeekFrom::Start(start_byte))?;
            file.read_exact(&mut delta_bytes)?;
        }
        let delta_str = std::str::from_utf8(&delta_bytes)
            .map_err(|_| IngestError::Invalid("transcript slice must be UTF-8 boundary"))?;
        let (turns, delta_nonempty_lines) =
            parse_transcript_slice(delta_str, start_byte, effective_end, base_line);
        let total_lines = base_line.saturating_add(delta_nonempty_lines);
        let new_prefix_sha256 = kp_store::sha256_file_prefix(path, effective_end)?
            .ok_or(IngestError::Invalid("transcript truncated during read"))?;
        let next_cursor = SessionCursor {
            session_id: session_id.to_owned(),
            client_kind,
            transcript_path: Some(path_str.to_owned()),
            last_byte_offset: effective_end,
            last_line_count: u64::try_from(total_lines).unwrap_or(u64::MAX),
            prefix_sha256: new_prefix_sha256,
            last_activity_at: at,
            last_extracted_offset: prev.as_ref().map_or(0, |c| c.last_extracted_offset),
        };
        return Ok(DeltaTranscript::Slice {
            turns,
            slice_start_byte: start_byte,
            slice_end_byte: effective_end,
            next_cursor,
        });
    }

    let messages = extract_inline_messages(payload);
    let any_echo = messages.iter().any(|m| {
        let raw = m.to_string();
        raw.contains("kp-ref:") || raw.contains("<kp-context>")
    });
    let full_serialized = serde_json::to_vec(&messages)?;
    let full_byte_len = u64::try_from(full_serialized.len()).unwrap_or(u64::MAX);
    let (start_idx, slice_start_byte) = match &prev {
        Some(c) if c.transcript_path.is_none() && c.last_line_count > 0 => {
            let prev_cnt = usize::try_from(c.last_line_count).unwrap_or(0);
            if prev_cnt > 0 && messages.len() >= prev_cnt {
                let prefix_bytes = serde_json::to_vec(&messages[..prev_cnt])?;
                if ContentHash::digest(&prefix_bytes).as_str() == c.prefix_sha256 {
                    if messages.len() == prev_cnt {
                        let mut refreshed = c.clone();
                        refreshed.last_activity_at = at;
                        save_cursor(store, &refreshed, is_session_end)?;
                        return Ok(DeltaTranscript::Unchanged {
                            self_echo_filtered: any_echo,
                        });
                    }
                    (prev_cnt, c.last_byte_offset.min(full_byte_len))
                } else {
                    (0, 0)
                }
            } else {
                (0, 0)
            }
        }
        _ => (0, 0),
    };
    let msg_len = messages.len();
    let turns = messages
        .iter()
        .enumerate()
        .skip(start_idx)
        .map(|(i, v)| {
            let end = if i + 1 == msg_len {
                full_byte_len
            } else {
                slice_start_byte
            };
            (i, slice_start_byte, end, v.clone())
        })
        .collect();
    let next_cursor = SessionCursor {
        session_id: session_id.to_owned(),
        client_kind,
        transcript_path: None,
        last_byte_offset: full_byte_len,
        last_line_count: u64::try_from(messages.len()).unwrap_or(u64::MAX),
        prefix_sha256: ContentHash::digest(&full_serialized).to_string(),
        last_activity_at: at,
        last_extracted_offset: prev.as_ref().map_or(0, |c| c.last_extracted_offset),
    };
    Ok(DeltaTranscript::Slice {
        turns,
        slice_start_byte,
        slice_end_byte: full_byte_len,
        next_cursor,
    })
}

pub fn infer_client_kind(payload: &Value) -> String {
    if let Some(c) = payload
        .get("_kp_client")
        .or_else(|| payload.get("client"))
        .and_then(Value::as_str)
    {
        if !c.trim().is_empty() {
            return c.to_owned();
        }
    }
    if let Some(p) = payload.get("transcript_path").and_then(Value::as_str) {
        if p.contains(".claude") {
            return "claude-code".into();
        }
        if p.contains(".codex") || p.contains("rollout-") {
            return "codex".into();
        }
    }
    "unknown".into()
}

/// Computes `(byte_offset, nonempty_line_count, prefix_sha256)` for the complete prefix of a
/// transcript file on disk in 64 KiB streaming chunks (zero 16 MiB cap, skipping incomplete tails)
/// so `Stop` / `SessionEnd` hooks can send a `< 5ms` `NotifyCursor` frame to `kp daemon`.
pub fn inspect_transcript_cursor_file(path: &Path) -> Result<(u64, u64, String)> {
    let mut file = std::fs::File::open(path)?;
    let effective_end = effective_complete_transcript_len(&mut file)?;
    let (line_count, prefix_sha256, _) = stream_inspect_prefix(path, &mut file, effective_end)?;
    Ok((effective_end, line_count, prefix_sha256))
}

fn count_nonempty_lines(text: &str) -> usize {
    text.lines().filter(|l| !l.trim().is_empty()).count()
}

pub(crate) fn parse_transcript_slice(
    text: &str,
    slice_start_byte: u64,
    slice_end_byte: u64,
    base_line: usize,
) -> (Vec<(usize, u64, u64, Value)>, usize) {
    let nonempty_lines = count_nonempty_lines(text);
    if text.trim().is_empty() {
        return (Vec::new(), nonempty_lines);
    }
    if slice_start_byte == 0 && text.trim_start().starts_with('[') {
        if let Ok(Value::Array(messages)) = serde_json::from_str::<Value>(text) {
            let total = messages.len();
            let turns = messages
                .into_iter()
                .enumerate()
                .map(|(i, v)| {
                    let end = if i + 1 == total {
                        slice_end_byte
                    } else {
                        slice_start_byte
                    };
                    (base_line + i, slice_start_byte, end, v)
                })
                .collect();
            return (turns, nonempty_lines);
        }
    }

    let mut turns = Vec::new();
    let mut offset = slice_start_byte;
    let mut idx = base_line;
    for raw_line in text.split_inclusive('\n') {
        let line_start = offset;
        let line_end = offset + (raw_line.len() as u64);
        offset = line_end;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let val = serde_json::from_str::<Value>(trimmed)
            .unwrap_or_else(|_| json!({"role": "unknown", "content": trimmed}));
        turns.push((idx, line_start, line_end, val));
        idx += 1;
    }
    (turns, nonempty_lines)
}

pub(crate) fn parse_transcript_text(text: &str) -> Vec<Value> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    if let Ok(Value::Array(messages)) = serde_json::from_str(text) {
        return messages;
    }
    let jsonl = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<std::result::Result<Vec<Value>, _>>();
    match jsonl {
        Ok(messages) => messages,
        Err(_) => vec![json!({"role":"unknown","content":text})],
    }
}

fn extract_inline_messages(payload: &Value) -> Vec<Value> {
    if let Some(messages) = payload.get("messages").and_then(Value::as_array) {
        return messages.clone();
    }
    if let Some(messages) = payload.get("transcript").and_then(Value::as_array) {
        return messages.clone();
    }
    if let Some(text) = payload.get("transcript").and_then(Value::as_str) {
        return parse_transcript_text(text);
    }
    Vec::new()
}
