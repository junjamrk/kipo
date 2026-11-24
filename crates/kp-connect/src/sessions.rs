//! Historical Agent Session Backfill (`Claude Code`, `Codex`, and `Antigravity` JSONL).
//!
//! Reads local JSONL session transcripts incrementally by byte & line offset, skips meta/harness
//! entries (`"isMeta": true`), strips `<kp-context>`, `[kp-ref:...]`, and harness-injected XML /
//! `AGENTS.md` instruction blocks (`<system-reminder>`, `<command-message>`, `<command-name>`,
//! `<environment_context>`, `<guardian_tool_descriptions>`, `<INSTRUCTIONS>`, `<SYSTEM_MESSAGE>`,
//! etc.), redacts secrets, skips `is_low_signal_ack` turns, and emits `SessionHandoffCursor`
//! records matching `session_cursors` in `schema.sql` for seamless live `Stop`/`SessionEnd` handoff.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::{
    is_low_signal_ack, ConnectError, Connector, ConnectorCursor, ConnectorKind, ConnectorScope,
    ExternalIdentityProposal, IngestedSourceEvent, PollBatch, Result, SourceEventOp,
    SourceScopeRoster,
};
use kp_model::{
    normalize_numeric_epoch, parse_timestamp_ms_in_tz, system_time_zone, ActorId, BoxFuture,
    ContentHash, EpochMs, SecretRedactor,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Default initial lookback window for historical agent sessions (30 days in ms).
pub const DEFAULT_SESSION_INITIAL_BACKFILL_MS: i64 = 30 * 86_400 * 1_000;

/// Maximum bytes read from a single JSONL session file per backfill pass (32 MiB safety bound).
pub const MAX_SESSION_FILE_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionClientKind {
    ClaudeCode,
    Codex,
    Antigravity,
}

impl SessionClientKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Antigravity => "antigravity",
        }
    }

    pub fn detect_from_path(path: &Path) -> Self {
        let lower = path.to_string_lossy().to_ascii_lowercase();
        if lower.contains("antigravity") || lower.ends_with("transcript.jsonl") {
            Self::Antigravity
        } else if lower.contains(".codex") || lower.contains("rollout-") {
            Self::Codex
        } else {
            Self::ClaudeCode
        }
    }
}

/// Cursor state matching the `session_cursors` SQLite table for zero-gap live hook handoff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHandoffCursor {
    pub session_id: String,
    pub client_kind: SessionClientKind,
    pub transcript_path: String,
    pub last_byte_offset: u64,
    pub last_line_count: u64,
    pub prefix_sha256: String,
    pub last_activity_at: EpochMs,
    pub last_extracted_offset: u64,
    #[serde(default)]
    pub active_external_key: Option<String>,
    #[serde(default)]
    pub revision: u32,
}

/// One normalized conversational turn extracted from an agent session JSONL slice.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedSessionTurn {
    pub line_number: u64,
    pub role: String,
    pub content: String,
    pub timestamp: Option<EpochMs>,
}

/// Outcome of incrementally reading a single JSONL session file from `previous_cursor`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionIncrementalRead {
    pub cursor: SessionHandoffCursor,
    pub turns: Vec<ParsedSessionTurn>,
    pub skipped_low_signal: usize,
    pub self_echo_filtered: bool,
    pub file_rewritten: bool,
}

struct SessionSanitizer {
    paired_blocks: Vec<Regex>,
    unclosed_kp: Regex,
    agents_md_with_instructions: Regex,
    agents_md_standalone: Regex,
    user_request_wrapper: Regex,
}

impl SessionSanitizer {
    fn new() -> Result<Self> {
        let block_tags = [
            r"(?is)<kp-context\b[^>]*>.*?</kp-context>",
            r"(?is)<system-reminder\b[^>]*>.*?</system-reminder>",
            r"(?is)<command-message\b[^>]*>.*?</command-message>",
            r"(?is)<command-name\b[^>]*>.*?</command-name>",
            r"(?is)<command-args\b[^>]*>.*?</command-args>",
            r"(?is)<local-command-stdout\b[^>]*>.*?</local-command-stdout>",
            r"(?is)<local-command-caveat\b[^>]*>.*?</local-command-caveat>",
            r"(?is)<environment_context\b[^>]*>.*?</environment_context>",
            r"(?is)<guardian_tool_descriptions\b[^>]*>.*?</guardian_tool_descriptions>",
            r"(?is)<user_instructions\b[^>]*>.*?</user_instructions>",
            r"(?is)<permissions instructions\b[^>]*>.*?</permissions instructions>",
            r"(?is)<collaboration_mode\b[^>]*>.*?</collaboration_mode>",
            r"(?is)<INSTRUCTIONS\b[^>]*>.*?</INSTRUCTIONS>",
            r"(?is)<SYSTEM_MESSAGE\b[^>]*>.*?</SYSTEM_MESSAGE>",
            r"(?is)<CONTEXT_SUMMARY\b[^>]*>.*?</CONTEXT_SUMMARY>",
        ];
        let paired_blocks = block_tags
            .into_iter()
            .map(|pat| {
                Regex::new(pat)
                    .map_err(|e| ConnectError::Transport(format!("session sanitizer regex: {e}")))
            })
            .collect::<Result<Vec<_>>>()?;

        let unclosed_kp = Regex::new(r"(?is)<kp-context\b[^>]*>.*$")
            .map_err(|e| ConnectError::Transport(format!("unclosed kp-context regex: {e}")))?;
        let agents_md_with_instructions = Regex::new(
            r"(?is)(?:^|\n)#\s*AGENTS\.md instructions for [^\r\n]*\s*<INSTRUCTIONS\b[^>]*>.*?</INSTRUCTIONS>",
        )
        .map_err(|e| ConnectError::Transport(format!("agents.md instructions regex: {e}")))?;
        let agents_md_standalone = Regex::new(r"(?is)^\s*#\s*AGENTS\.md instructions for .*$")
            .map_err(|e| ConnectError::Transport(format!("agents.md standalone regex: {e}")))?;
        let user_request_wrapper = Regex::new(r"(?i)</?USER_REQUEST>")
            .map_err(|e| ConnectError::Transport(format!("user_request regex: {e}")))?;

        Ok(Self {
            paired_blocks,
            unclosed_kp,
            agents_md_with_instructions,
            agents_md_standalone,
            user_request_wrapper,
        })
    }

    /// Strips `<kp-context>`, `[kp-ref:...]`, and Claude/Codex/Antigravity harness-injected blocks.
    fn sanitize_text(&self, raw: &str) -> (String, bool) {
        let mut filtered = false;
        let mut current = raw.to_owned();

        if self.agents_md_with_instructions.is_match(&current) {
            filtered = true;
            current = self
                .agents_md_with_instructions
                .replace_all(&current, "")
                .into_owned();
        }
        for re in &self.paired_blocks {
            if re.is_match(&current) {
                filtered = true;
                current = re.replace_all(&current, "").into_owned();
            }
        }
        if self.unclosed_kp.is_match(&current) {
            filtered = true;
            current = self.unclosed_kp.replace_all(&current, "").into_owned();
        }
        if self.agents_md_standalone.is_match(current.trim()) {
            filtered = true;
            current.clear();
        }
        if self.user_request_wrapper.is_match(&current) {
            current = self
                .user_request_wrapper
                .replace_all(&current, "")
                .into_owned();
        }

        let mut kept_lines = String::with_capacity(current.len());
        for line in current.split_inclusive('\n') {
            if line.contains("[kp-ref:") || line.contains("<skip_summary/>") {
                filtered = true;
            } else {
                kept_lines.push_str(line);
            }
        }
        (kept_lines.trim().to_owned(), filtered)
    }
}

/// Strips `<kp-context>...</kp-context>`, `[kp-ref:...]`, and harness-injected XML/AGENTS.md blocks.
pub fn strip_kp_echoes(raw: &str) -> Result<(String, bool)> {
    Ok(SessionSanitizer::new()?.sanitize_text(raw))
}

/// Incrementally reads newly appended JSONL lines from `path` starting at `previous`,
/// detecting file truncation/rewrite via `prefix_sha256` and leaving incomplete trailing writes untouched.
pub fn read_session_file_incremental(
    path: &Path,
    client_hint: Option<SessionClientKind>,
    previous: Option<&SessionHandoffCursor>,
    fallback_now: EpochMs,
) -> Result<SessionIncrementalRead> {
    let meta = fs::metadata(path)?;
    if meta.len() > MAX_SESSION_FILE_BYTES {
        return Err(ConnectError::Transport(format!(
            "session file `{}` exceeds {} byte safety cap",
            path.display(),
            MAX_SESSION_FILE_BYTES
        )));
    }
    let bytes = fs::read(path)?;
    let client_kind = client_hint.unwrap_or_else(|| SessionClientKind::detect_from_path(path));
    let redactor = SecretRedactor::new().map_err(|e| ConnectError::Transport(e.to_string()))?;
    let sanitizer = SessionSanitizer::new()?;

    let (start_offset, base_lines, file_rewritten, prev_active_key, prev_rev) = match previous {
        Some(prev) if prev.last_byte_offset > 0 => {
            let prev_off = usize::try_from(prev.last_byte_offset).unwrap_or(usize::MAX);
            if bytes.len() < prev_off {
                (
                    0_usize,
                    0_u64,
                    true,
                    prev.active_external_key.clone(),
                    prev.revision,
                )
            } else {
                let current_prefix_hash = ContentHash::digest(&bytes[..prev_off]).to_string();
                if !prev.prefix_sha256.is_empty() && current_prefix_hash != prev.prefix_sha256 {
                    (
                        0_usize,
                        0_u64,
                        true,
                        prev.active_external_key.clone(),
                        prev.revision,
                    )
                } else {
                    (
                        prev_off,
                        prev.last_line_count,
                        false,
                        prev.active_external_key.clone(),
                        prev.revision,
                    )
                }
            }
        }
        Some(prev) => (
            0_usize,
            0_u64,
            false,
            prev.active_external_key.clone(),
            prev.revision,
        ),
        None => (0_usize, 0_u64, false, None, 0),
    };

    let slice = &bytes[start_offset..];
    let consumable_len = compute_consumable_jsonl_len(slice);
    let end_offset = start_offset + consumable_len;
    let prefix_sha256 = if end_offset == 0 {
        String::new()
    } else {
        ContentHash::digest(&bytes[..end_offset]).to_string()
    };

    let text_slice = String::from_utf8_lossy(&bytes[start_offset..end_offset]);
    let mut detected_session_id = previous.map(|p| p.session_id.clone());
    let mut line_count = base_lines;
    let mut latest_ts = previous.map(|p| p.last_activity_at).unwrap_or(fallback_now);
    let mut turns = Vec::<ParsedSessionTurn>::new();
    let mut skipped_low_signal = 0_usize;
    let mut self_echo_filtered = false;
    let mut last_dedup_pair: Option<(String, String)> = None;

    for raw_line in text_slice.lines() {
        line_count += 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };

        if detected_session_id.is_none() {
            detected_session_id = extract_session_id_from_line(&val);
        }
        if is_internal_extraction_line(&val) {
            self_echo_filtered = true;
            continue;
        }
        // Real Claude Code entries carrying `"isMeta": true` are harness caveats/command metadata, never human speech.
        if is_claude_meta_entry(&val) {
            continue;
        }

        let line_ts = extract_line_timestamp(&val);
        if let Some(ts) = line_ts {
            if ts.0 > latest_ts.0 {
                latest_ts = ts;
            }
        }

        let Some((role, raw_content, block_had_echo)) =
            extract_turn_for_client(client_kind, &val, &sanitizer)
        else {
            continue;
        };
        let (stripped, had_echo) = sanitizer.sanitize_text(&raw_content);
        self_echo_filtered |= block_had_echo || had_echo;
        if stripped.is_empty() {
            continue;
        }
        let redacted = redactor.redact(&stripped);
        if is_low_signal_ack(&redacted) {
            skipped_low_signal += 1;
            continue;
        }

        // Deduplicate adjacent identical `(role, content)` pairs emitted by Codex `event_msg` + `response_item`.
        let pair = (role.clone(), redacted.clone());
        if last_dedup_pair.as_ref() == Some(&pair) {
            continue;
        }
        last_dedup_pair = Some(pair);

        turns.push(ParsedSessionTurn {
            line_number: line_count,
            role,
            content: redacted,
            timestamp: line_ts,
        });
    }

    let session_id =
        detected_session_id.unwrap_or_else(|| default_session_id_for_path(path, client_kind));
    let end_offset_u64 = u64::try_from(end_offset).unwrap_or(0);

    Ok(SessionIncrementalRead {
        cursor: SessionHandoffCursor {
            session_id,
            client_kind,
            transcript_path: path.to_string_lossy().into_owned(),
            last_byte_offset: end_offset_u64,
            last_line_count: line_count,
            prefix_sha256,
            last_activity_at: latest_ts,
            last_extracted_offset: end_offset_u64,
            active_external_key: prev_active_key,
            revision: prev_rev,
        },
        turns,
        skipped_low_signal,
        self_echo_filtered,
        file_rewritten,
    })
}

/// Determines how many bytes of `slice` represent complete JSONL lines.
fn compute_consumable_jsonl_len(slice: &[u8]) -> usize {
    if slice.is_empty() || slice.ends_with(b"\n") {
        return slice.len();
    }
    let last_nl_end = slice
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |idx| idx + 1);
    let tail = &slice[last_nl_end..];
    let tail_trimmed = String::from_utf8_lossy(tail);
    if serde_json::from_str::<Value>(tail_trimmed.trim()).is_ok() {
        slice.len()
    } else {
        last_nl_end
    }
}

fn is_claude_meta_entry(val: &Value) -> bool {
    val.get("isMeta").and_then(Value::as_bool) == Some(true)
        || val.pointer("/message/isMeta").and_then(Value::as_bool) == Some(true)
}

fn extract_session_id_from_line(val: &Value) -> Option<String> {
    val.get("sessionId")
        .or_else(|| val.get("session_id"))
        .or_else(|| val.get("conversation_id"))
        .or_else(|| {
            if val.get("type").and_then(Value::as_str) == Some("session_meta") {
                val.pointer("/payload/id")
            } else {
                None
            }
        })
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}

fn default_session_id_for_path(path: &Path, client_kind: SessionClientKind) -> String {
    if client_kind == SessionClientKind::Antigravity {
        for ancestor in path.ancestors().skip(1) {
            if let Some(name) = ancestor.file_name().and_then(|n| n.to_str()) {
                if name != "logs" && name != ".system_generated" && name != "brain" {
                    return format!("antigravity:{name}");
                }
            }
        }
    }
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session");
    format!("{}:{stem}", client_kind.as_str())
}

fn is_internal_extraction_line(val: &Value) -> bool {
    if val
        .pointer("/env/KP_INTERNAL_EXTRACTION")
        .or_else(|| val.get("kp_internal_extraction"))
        .is_some()
    {
        return true;
    }
    let raw = val.to_string();
    raw.contains("KP_INTERNAL_EXTRACTION=1")
}

fn extract_line_timestamp(val: &Value) -> Option<EpochMs> {
    let ts_val = val
        .get("timestamp")
        .or_else(|| val.get("created_at"))
        .or_else(|| val.pointer("/payload/timestamp"))?;
    match ts_val {
        Value::String(s) => parse_timestamp_ms_in_tz(s, &system_time_zone()),
        Value::Number(n) => n.as_i64().map(normalize_numeric_epoch),
        _ => None,
    }
}

fn extract_turn_for_client(
    client_kind: SessionClientKind,
    val: &Value,
    sanitizer: &SessionSanitizer,
) -> Option<(String, String, bool)> {
    match client_kind {
        SessionClientKind::Antigravity => extract_antigravity_turn(val, sanitizer)
            .or_else(|| extract_claude_turn(val, sanitizer))
            .or_else(|| extract_codex_turn(val, sanitizer)),
        SessionClientKind::Codex => extract_codex_turn(val, sanitizer)
            .or_else(|| extract_claude_turn(val, sanitizer))
            .or_else(|| extract_antigravity_turn(val, sanitizer)),
        SessionClientKind::ClaudeCode => extract_claude_turn(val, sanitizer)
            .or_else(|| extract_codex_turn(val, sanitizer))
            .or_else(|| extract_antigravity_turn(val, sanitizer)),
    }
}

fn extract_antigravity_turn(
    val: &Value,
    sanitizer: &SessionSanitizer,
) -> Option<(String, String, bool)> {
    let step_type = val.get("type").and_then(Value::as_str);
    let source = val.get("source").and_then(Value::as_str);
    // In real Antigravity `transcript.jsonl`, `source="SYSTEM"` is `SYSTEM_MESSAGE` and
    // `type="GENERIC"` is tool call output; only `USER_INPUT`/`USER_EXPLICIT` and
    // `PLANNER_RESPONSE`/`MODEL_RESPONSE` carry human or assistant prose.
    if source == Some("SYSTEM") || step_type == Some("GENERIC") {
        return None;
    }
    let role = match (step_type, source) {
        (Some("USER_INPUT"), _) | (_, Some("USER_EXPLICIT")) => "user",
        (Some("PLANNER_RESPONSE"), _) | (Some("MODEL_RESPONSE"), _) => "assistant",
        _ => return None,
    };
    let (content, had_echo) = val
        .get("content")
        .and_then(|c| extract_content_blocks(c, sanitizer))?;
    if content.trim().is_empty() {
        return None;
    }
    Some((role.into(), content, had_echo))
}

fn extract_codex_turn(val: &Value, sanitizer: &SessionSanitizer) -> Option<(String, String, bool)> {
    let line_type = val.get("type").and_then(Value::as_str);
    if line_type == Some("response_item") {
        let payload = val.get("payload")?;
        let payload_type = payload.get("type").and_then(Value::as_str)?;
        match payload_type {
            "message" => {
                let role = payload.get("role").and_then(Value::as_str)?;
                if !matches!(role, "user" | "assistant") {
                    return None;
                }
                let (text, had_echo) = payload
                    .get("content")
                    .and_then(|c| extract_content_blocks(c, sanitizer))?;
                if text.trim().is_empty() {
                    return None;
                }
                return Some((role.to_owned(), text, had_echo));
            }
            // Real Codex 0.164 emits assistant replies as `type="response_item"` with `payload.type="agent_message"`.
            "agent_message" | "assistant_message" => {
                let (text, had_echo) =
                    if let Some(msg_str) = payload.get("message").or_else(|| payload.get("text")) {
                        extract_content_blocks(msg_str, sanitizer)?
                    } else {
                        payload
                            .get("content")
                            .and_then(|c| extract_content_blocks(c, sanitizer))?
                    };
                if text.trim().is_empty() {
                    return None;
                }
                return Some(("assistant".into(), text, had_echo));
            }
            _ => return None,
        }
    }
    if line_type == Some("event_msg") {
        let payload = val.get("payload")?;
        let ev_type = payload.get("type").and_then(Value::as_str)?;
        let role = match ev_type {
            "user_message" => "user",
            "agent_message" | "assistant_message" => "assistant",
            _ => return None,
        };
        let (text, had_echo) = payload
            .get("message")
            .or_else(|| payload.get("text"))
            .and_then(|c| extract_content_blocks(c, sanitizer))?;
        if text.trim().is_empty() {
            return None;
        }
        return Some((role.into(), text, had_echo));
    }
    None
}

fn extract_claude_turn(
    val: &Value,
    sanitizer: &SessionSanitizer,
) -> Option<(String, String, bool)> {
    if is_claude_meta_entry(val) {
        return None;
    }
    if let Some(line_type) = val.get("type").and_then(Value::as_str) {
        if !matches!(line_type, "user" | "assistant" | "message") {
            return None;
        }
    }
    let msg = val.get("message").unwrap_or(val);
    let role = msg
        .get("role")
        .or_else(|| val.get("type"))
        .and_then(Value::as_str)?;
    if !matches!(role, "user" | "assistant") {
        return None;
    }
    let content_val = msg.get("content").or_else(|| msg.get("text"))?;
    let (text, had_echo) = extract_content_blocks(content_val, sanitizer)?;
    if text.trim().is_empty() {
        return None;
    }
    Some((role.to_owned(), text, had_echo))
}

/// Sanitizes each content block individually before joining so an `AGENTS.md` or
/// `<environment_context>` block in a multi-part `content` array never contaminates or hides
/// a sibling human prompt block.
fn extract_content_blocks(content: &Value, sanitizer: &SessionSanitizer) -> Option<(String, bool)> {
    match content {
        Value::String(s) => {
            let (clean, had_echo) = sanitizer.sanitize_text(s);
            Some((clean, had_echo))
        }
        Value::Array(blocks) => {
            let mut parts = Vec::new();
            let mut any_echo = false;
            for block in blocks {
                let block_type = block.get("type").and_then(Value::as_str).unwrap_or("text");
                // Skip raw tool_use, tool_result, and thinking payloads during session backfill.
                if !matches!(block_type, "text" | "input_text" | "output_text") {
                    continue;
                }
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    let (clean, had_echo) = sanitizer.sanitize_text(t);
                    any_echo |= had_echo;
                    if !clean.is_empty() {
                        parts.push(clean);
                    }
                }
            }
            if parts.is_empty() && !any_echo {
                None
            } else {
                Some((parts.join("\n"), any_echo))
            }
        }
        _ => None,
    }
}

/// Resumable checkpoint for `AgentSessionConnector` storing per-file `SessionHandoffCursor`s.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionBackfillCheckpoint {
    pub high_water_ms: Option<i64>,
    /// Keyed by canonical transcript path so live hooks can take over seamlessly from `last_byte_offset`.
    #[serde(default)]
    pub handoff_cursors: BTreeMap<String, SessionHandoffCursor>,
}

impl SessionBackfillCheckpoint {
    pub fn from_cursor(cursor: &ConnectorCursor) -> Result<Self> {
        let mut cp = match &cursor.cursor_token {
            Some(raw) if !raw.trim().is_empty() => serde_json::from_str::<Self>(raw)?,
            _ => Self::default(),
        };
        if cp.high_water_ms.is_none() {
            cp.high_water_ms = cursor.high_water_ms.map(|ms| ms.0);
        }
        Ok(cp)
    }

    pub fn into_cursor(&self, now: EpochMs) -> Result<ConnectorCursor> {
        Ok(ConnectorCursor {
            cursor_token: Some(serde_json::to_string(self)?),
            high_water_ms: self.high_water_ms.map(EpochMs),
            updated_at: now,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionBackfillConfig {
    pub now: EpochMs,
    pub initial_backfill_ms: i64,
    pub max_files_per_poll: usize,
}

impl Default for SessionBackfillConfig {
    fn default() -> Self {
        Self {
            now: EpochMs(1_795_245_380_000),
            initial_backfill_ms: DEFAULT_SESSION_INITIAL_BACKFILL_MS,
            max_files_per_poll: 100,
        }
    }
}

/// Historical Agent Session Backfill connector (`ConnectorKind::AgentSession`).
#[derive(Clone, Debug, Default)]
pub struct AgentSessionConnector {
    config: SessionBackfillConfig,
}

impl AgentSessionConnector {
    pub fn new(config: SessionBackfillConfig) -> Self {
        Self { config }
    }

    pub fn parse_upstream_target(
        upstream_container_id: &str,
    ) -> Result<(Option<SessionClientKind>, PathBuf)> {
        let trimmed = upstream_container_id.trim();
        if trimmed.is_empty() {
            return Err(ConnectError::Transport(
                "empty AgentSession upstream_container_id".into(),
            ));
        }
        if let Some(rest) = trimmed.strip_prefix("claude-code:") {
            return Ok((Some(SessionClientKind::ClaudeCode), PathBuf::from(rest)));
        }
        if let Some(rest) = trimmed.strip_prefix("codex:") {
            return Ok((Some(SessionClientKind::Codex), PathBuf::from(rest)));
        }
        if let Some(rest) = trimmed.strip_prefix("antigravity:") {
            return Ok((Some(SessionClientKind::Antigravity), PathBuf::from(rest)));
        }
        Ok((None, PathBuf::from(trimmed)))
    }
}

impl Connector for AgentSessionConnector {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::AgentSession
    }

    fn poll_incremental<'a>(
        &'a self,
        scope: &'a ConnectorScope,
        cursor: &'a ConnectorCursor,
        ingested_by: &'a ActorId,
    ) -> BoxFuture<'a, Result<PollBatch>> {
        Box::pin(async move {
            if !scope.enabled {
                return Err(ConnectError::ScopeDisabled(
                    scope.source_scope_id.to_string(),
                ));
            }

            let (client_hint, root_path) =
                Self::parse_upstream_target(&scope.upstream_container_id)?;
            if !root_path.exists() {
                return Err(ConnectError::Transport(format!(
                    "agent session path `{}` does not exist",
                    root_path.display()
                )));
            }

            let mut checkpoint = SessionBackfillCheckpoint::from_cursor(cursor)?;
            let mut discovered = discover_session_jsonl_files(&root_path, client_hint)?;
            discovered.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

            let min_window_ms = (self.config.now.0 - self.config.initial_backfill_ms).max(0);
            let mut batch = PollBatch::default();
            let owner_ext_id = format!("local_session:{ingested_by}");

            batch.scope_rosters.push(SourceScopeRoster {
                scope_id: scope.source_scope_id.clone(),
                provider: "agent-session".into(),
                external_container_id: scope.upstream_container_id.clone(),
                label: scope.label.clone(),
                external_member_ids: vec![owner_ext_id.clone()],
                all_library_members: false,
                synced_at: self.config.now,
            });
            batch.identity_proposals.push(ExternalIdentityProposal {
                provider: "agent-session".into(),
                external_id: owner_ext_id.clone(),
                display_name: ingested_by.to_string(),
                proposed_actor: Some(ingested_by.clone()),
            });

            let mut files_processed = 0_usize;
            for (file_path, mtime_ms) in discovered {
                if files_processed >= self.config.max_files_per_poll.max(1) {
                    break;
                }
                let path_key = file_path.to_string_lossy().into_owned();
                let prev_cursor = checkpoint.handoff_cursors.get(&path_key).cloned();

                if prev_cursor.is_none() && mtime_ms > 0 && mtime_ms < min_window_ms {
                    continue;
                }
                files_processed += 1;

                let start_off = prev_cursor.as_ref().map_or(0, |c| c.last_byte_offset);
                let mut read_res = read_session_file_incremental(
                    &file_path,
                    client_hint,
                    prev_cursor.as_ref(),
                    self.config.now,
                )?;
                batch.skipped_low_signal += read_res.skipped_low_signal;

                let effective_start = if read_res.file_rewritten {
                    0
                } else {
                    start_off
                };
                if !read_res.turns.is_empty() {
                    let chunk_turns: Vec<kp_model::ChunkTurn> = read_res
                        .turns
                        .iter()
                        .map(|t| {
                            let is_assistant = t.role == "assistant";
                            kp_model::ChunkTurn {
                                speaker: if is_assistant {
                                    None
                                } else {
                                    Some(ingested_by.clone())
                                },
                                is_assistant,
                                text: t.content.clone(),
                            }
                        })
                        .collect();
                    let formatted_body = kp_model::format_tagged_turns(&chunk_turns);
                    let occurred_at = read_res
                        .turns
                        .iter()
                        .filter_map(|t| t.timestamp)
                        .max()
                        .unwrap_or(read_res.cursor.last_activity_at);

                    checkpoint.high_water_ms = Some(
                        checkpoint
                            .high_water_ms
                            .unwrap_or(occurred_at.0)
                            .max(occurred_at.0),
                    );

                    let (external_key, event_op) = if read_res.file_rewritten {
                        if let Some(prev_key) = read_res.cursor.active_external_key.clone() {
                            let next_rev = read_res.cursor.revision.saturating_add(1);
                            read_res.cursor.revision = next_rev;
                            let new_key = format!(
                                "session:{}:0..{}@rev{next_rev}",
                                read_res.cursor.session_id, read_res.cursor.last_byte_offset
                            );
                            (
                                new_key,
                                SourceEventOp::Supersede {
                                    previous_external_key: prev_key,
                                },
                            )
                        } else {
                            (
                                format!(
                                    "session:{}:0..{}",
                                    read_res.cursor.session_id, read_res.cursor.last_byte_offset
                                ),
                                SourceEventOp::Upsert,
                            )
                        }
                    } else {
                        (
                            format!(
                                "session:{}:{effective_start}..{}",
                                read_res.cursor.session_id, read_res.cursor.last_byte_offset
                            ),
                            SourceEventOp::Upsert,
                        )
                    };

                    read_res.cursor.active_external_key = Some(external_key.clone());
                    batch.events.push(IngestedSourceEvent {
                        external_key,
                        uri: format!(
                            "session://{}/{}#bytes={effective_start}..{}",
                            read_res.cursor.client_kind.as_str(),
                            read_res.cursor.session_id,
                            read_res.cursor.last_byte_offset
                        ),
                        media_type: "text/markdown".into(),
                        speaker_external_id: owner_ext_id.clone(),
                        speaker_display_name: ingested_by.to_string(),
                        ingested_by: ingested_by.clone(),
                        source_scope_id: scope.source_scope_id.clone(),
                        content: formatted_body,
                        event_op,
                        occurred_at,
                        model_policy: scope.model_policy.clone(),
                        thread_key: None,
                        reply_to: None,
                    });
                }

                checkpoint.handoff_cursors.insert(path_key, read_res.cursor);
            }

            batch.next_cursor = checkpoint.into_cursor(self.config.now)?;
            Ok(batch)
        })
    }
}

fn discover_session_jsonl_files(
    root: &Path,
    client_hint: Option<SessionClientKind>,
) -> Result<Vec<(PathBuf, i64)>> {
    if root.is_file() {
        let mtime = file_mtime_ms(root);
        return Ok(vec![(root.to_path_buf(), mtime)]);
    }
    let mut results = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir)?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file()
                && path.extension().and_then(|e| e.to_str()) == Some("jsonl")
            {
                let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if file_name == "transcript_full.jsonl" {
                    continue;
                }
                if client_hint == Some(SessionClientKind::Antigravity)
                    && file_name != "transcript.jsonl"
                {
                    continue;
                }
                results.push((path.clone(), file_mtime_ms(&path)));
            }
        }
    }
    Ok(results)
}

fn file_mtime_ms(path: &Path) -> i64 {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}
