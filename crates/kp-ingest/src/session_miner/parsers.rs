use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use kp_model::SecretRedactor;
use serde_json::Value;

use super::classifier::{classify_command_output, has_meaningful_failure_signal};
use super::{derive_file_session_id, scrub_session_privacy, SessionEvent};
use crate::hook_cursor::{filter_echo, strip_host_system_blocks};
use crate::Result;

pub(super) fn parse_session_file(
    path: &Path,
    redactor: &SecretRedactor,
) -> Result<(String, Vec<SessionEvent>)> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "md" {
        let file = File::open(path)?;
        let bytes = crate::read_bounded(file, 32 * 1024 * 1024)?;
        let raw = String::from_utf8_lossy(&bytes);
        let session_id = derive_file_session_id(path, None, redactor);
        let events = parse_markdown_or_tagged_dialogue(&raw, redactor);
        return Ok((session_id, events));
    }

    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut embedded_session_id: Option<String> = None;
    let mut state = JsonlParseState::default();
    let mut line_idx = 0usize;

    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if embedded_session_id.is_none() {
            embedded_session_id = extract_embedded_session_id(&val);
        }
        feed_jsonl_step(&val, line_idx, redactor, &mut state);
        line_idx += 1;
    }

    let session_id = derive_file_session_id(path, embedded_session_id.as_deref(), redactor);
    Ok((session_id, state.events))
}

pub(super) fn parse_text_or_jsonl_str(raw: &str, redactor: &SecretRedactor) -> Vec<SessionEvent> {
    let first_non_empty = raw.lines().map(str::trim).find(|l| !l.is_empty());
    if first_non_empty.is_some_and(|l| l.starts_with('{')) {
        let mut state = JsonlParseState::default();
        for (idx, line) in raw.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
                feed_jsonl_step(&val, idx, redactor, &mut state);
            }
        }
        if !state.events.is_empty() {
            return state.events;
        }
    }
    parse_markdown_or_tagged_dialogue(raw, redactor)
}

#[derive(Default)]
struct JsonlParseState {
    events: Vec<SessionEvent>,
    tool_calls_by_id: BTreeMap<String, (String, Option<String>)>,
    last_ag_cmd_hint: Option<String>,
    last_ag_had_mutation: bool,
}

fn extract_embedded_session_id(val: &Value) -> Option<String> {
    for ptr in [
        "/sessionId",
        "/session_id",
        "/conversation_id",
        "/payload/id",
        "/payload/session_id",
    ] {
        if let Some(s) = val
            .pointer(ptr)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(s.to_owned());
        }
    }
    None
}

fn clean_human_or_assistant_text(raw: &str, redactor: &SecretRedactor) -> String {
    let (no_echo, _) = filter_echo(raw);
    let no_sys = strip_host_system_blocks(&no_echo);
    scrub_session_privacy(no_sys.trim(), redactor)
}

fn is_mutating_tool_name(name: &str) -> bool {
    matches!(
        name,
        "edit_file"
            | "write_file"
            | "multi_edit"
            | "replace_file_content"
            | "multi_replace_file_content"
            | "Edit"
            | "Write"
            | "MultiEdit"
            | "NotebookEdit"
            | "apply_patch"
    )
}

fn feed_jsonl_step(
    val: &Value,
    line_idx: usize,
    redactor: &SecretRedactor,
    state: &mut JsonlParseState,
) {
    let step_num = val
        .get("step_index")
        .and_then(|v| {
            v.as_u64()
                .map(|n| n.to_string())
                .or_else(|| v.as_str().map(str::to_owned))
        })
        .unwrap_or_else(|| line_idx.to_string());
    let step_label = format!("step:{step_num}");

    if val.get("isMeta").and_then(Value::as_bool) == Some(true)
        || val.pointer("/message/isMeta").and_then(Value::as_bool) == Some(true)
    {
        return;
    }

    if let Some(top_type) = val.get("type").and_then(Value::as_str) {
        match top_type {
            // 1. Antigravity `transcript.jsonl`
            "USER_INPUT" => {
                if val
                    .get("source")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s != "USER_EXPLICIT")
                {
                    return;
                }
                let raw = val
                    .get("content")
                    .map(extract_value_text)
                    .unwrap_or_default();
                let cleaned = clean_human_or_assistant_text(&raw, redactor);
                if !cleaned.is_empty() {
                    state.events.push(SessionEvent::UserTurn {
                        step_label,
                        text: cleaned,
                    });
                }
                return;
            }
            "PLANNER_RESPONSE" => {
                let mut has_mutation = false;
                let mut cmd_hint = None;
                if let Some(tcs) = val.get("tool_calls").and_then(Value::as_array) {
                    for tc in tcs {
                        let name = tc.get("name").and_then(Value::as_str).unwrap_or("");
                        if is_mutating_tool_name(name) {
                            has_mutation = true;
                        }
                        if name == "run_command" {
                            cmd_hint = tc
                                .pointer("/args/CommandLine")
                                .and_then(Value::as_str)
                                .or_else(|| tc.pointer("/args/toolSummary").and_then(Value::as_str))
                                .map(|s| scrub_session_privacy(s, redactor));
                        }
                    }
                }
                state.last_ag_had_mutation = has_mutation;
                state.last_ag_cmd_hint = cmd_hint;
                let raw = val
                    .get("content")
                    .map(extract_value_text)
                    .unwrap_or_default();
                let cleaned = clean_human_or_assistant_text(&raw, redactor);
                if !cleaned.is_empty() || has_mutation {
                    state.events.push(SessionEvent::AssistantTurn {
                        step_label,
                        text: cleaned,
                        has_code_mutation: has_mutation,
                    });
                }
                return;
            }
            "GENERIC" => {
                let raw = val
                    .get("content")
                    .map(extract_value_text)
                    .unwrap_or_default();
                let scrubbed = scrub_session_privacy(&raw, redactor);
                let is_mutation = state.last_ag_had_mutation
                    || (scrubbed.contains("Made ") && scrubbed.contains(" replacement in "))
                    || scrubbed.contains("Created file ");
                state.last_ag_had_mutation = false;
                let cmd_hint = state.last_ag_cmd_hint.take();
                let (failed, succeeded) = classify_command_output(&scrubbed, cmd_hint.as_deref());
                if failed || succeeded || is_mutation {
                    state.events.push(SessionEvent::ToolExecution {
                        step_label,
                        command_hint: cmd_hint,
                        output: scrubbed,
                        failed,
                        succeeded,
                        is_code_mutation: is_mutation,
                    });
                }
                return;
            }
            "SYSTEM_MESSAGE" => {
                // Ignore compaction/board wrappers; ONLY inspect background `run_command` task completions.
                let raw = val
                    .get("content")
                    .map(extract_value_text)
                    .unwrap_or_default();
                for (sub_idx, task_out) in extract_antigravity_bg_task_outputs(&raw)
                    .into_iter()
                    .enumerate()
                {
                    let scrubbed = scrub_session_privacy(&task_out, redactor);
                    let (failed, succeeded) = classify_command_output(&scrubbed, Some("cargo"));
                    if failed || succeeded {
                        state.events.push(SessionEvent::ToolExecution {
                            step_label: format!("{step_label}:bg{sub_idx}"),
                            command_hint: Some("background_command".into()),
                            output: scrubbed,
                            failed,
                            succeeded,
                            is_code_mutation: false,
                        });
                    }
                }
                return;
            }
            // 2. Codex `.jsonl`
            "event_msg" => {
                let Some(payload) = val.get("payload") else {
                    return;
                };
                match payload.get("type").and_then(Value::as_str) {
                    Some("user_message") => {
                        let raw = extract_payload_text(payload);
                        let cleaned = clean_human_or_assistant_text(&raw, redactor);
                        if !cleaned.is_empty() {
                            state.events.push(SessionEvent::UserTurn {
                                step_label,
                                text: cleaned,
                            });
                        }
                    }
                    Some("agent_message") => {
                        let raw = extract_payload_text(payload);
                        let cleaned = clean_human_or_assistant_text(&raw, redactor);
                        if !cleaned.is_empty() {
                            state.events.push(SessionEvent::AssistantTurn {
                                step_label,
                                text: cleaned,
                                has_code_mutation: false,
                            });
                        }
                    }
                    _ => {}
                }
                return;
            }
            "response_item" => {
                let Some(payload) = val.get("payload") else {
                    return;
                };
                match payload.get("type").and_then(Value::as_str) {
                    Some("agent_message") => {
                        let cleaned =
                            clean_human_or_assistant_text(&extract_payload_text(payload), redactor);
                        if !cleaned.is_empty() {
                            state.events.push(SessionEvent::AssistantTurn {
                                step_label,
                                text: cleaned,
                                has_code_mutation: false,
                            });
                        }
                    }
                    Some("message") => {
                        let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
                        let cleaned =
                            clean_human_or_assistant_text(&extract_payload_text(payload), redactor);
                        if cleaned.is_empty() {
                            return;
                        }
                        if role == "user" {
                            state.events.push(SessionEvent::UserTurn {
                                step_label,
                                text: cleaned,
                            });
                        } else if role == "assistant" {
                            state.events.push(SessionEvent::AssistantTurn {
                                step_label,
                                text: cleaned,
                                has_code_mutation: false,
                            });
                        }
                    }
                    Some("function_call" | "custom_tool_call" | "local_shell_call") => {
                        let name = payload
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("shell")
                            .to_owned();
                        let args_raw = payload
                            .get("arguments")
                            .or_else(|| payload.get("input"))
                            .or_else(|| payload.get("action"))
                            .map(extract_value_text)
                            .unwrap_or_default();
                        let args_clean = scrub_session_privacy(&args_raw, redactor);
                        let is_mut = is_mutating_tool_name(&name)
                            || args_clean.contains("*** Update File:")
                            || args_clean.contains("*** Add File:")
                            || args_clean.contains("apply_patch");
                        if let Some(cid) = payload.get("call_id").and_then(Value::as_str) {
                            state
                                .tool_calls_by_id
                                .insert(cid.to_owned(), (name.clone(), Some(args_clean.clone())));
                        }
                        if is_mut {
                            state.events.push(SessionEvent::AssistantTurn {
                                step_label,
                                text: String::new(),
                                has_code_mutation: true,
                            });
                        }
                    }
                    Some(
                        "function_call_output"
                        | "custom_tool_call_output"
                        | "local_shell_call_output",
                    ) => {
                        let call_id = payload.get("call_id").and_then(Value::as_str).unwrap_or("");
                        let (tool_name, cmd_hint) = state
                            .tool_calls_by_id
                            .remove(call_id)
                            .unwrap_or_else(|| ("exec_command".into(), None));
                        let (out_text, explicit_exit) = extract_codex_tool_output(payload);
                        let scrubbed = scrub_session_privacy(&out_text, redactor);
                        let is_mut = is_mutating_tool_name(&tool_name)
                            || scrubbed.contains("Success. Updated the following files");
                        let (mut failed, mut succeeded) =
                            classify_command_output(&scrubbed, cmd_hint.as_deref());
                        if let Some(code) = explicit_exit {
                            if code != 0
                                && has_meaningful_failure_signal(&scrubbed, cmd_hint.as_deref())
                            {
                                failed = true;
                                succeeded = false;
                            } else if code == 0 && !failed {
                                succeeded = true;
                            }
                        }
                        if failed || succeeded || is_mut {
                            state.events.push(SessionEvent::ToolExecution {
                                step_label,
                                command_hint: cmd_hint,
                                output: scrubbed,
                                failed,
                                succeeded,
                                is_code_mutation: is_mut,
                            });
                        }
                    }
                    _ => {}
                }
                return;
            }
            "summary"
            | "file-history-snapshot"
            | "system"
            | "progress"
            | "queue-operation"
            | "session_meta"
            | "turn_context"
            | "compacted"
            | "CHECKPOINT"
            | "ERROR_MESSAGE" => return,
            _ => {}
        }
    }

    // 3. Claude Code `.jsonl` (`type: "user" | "assistant"` or `message.role`)
    let inner = val.get("message").unwrap_or(val);
    let role = inner
        .get("role")
        .and_then(Value::as_str)
        .or_else(|| val.get("type").and_then(Value::as_str))
        .unwrap_or("");

    if role == "assistant" {
        let mut text_parts = Vec::new();
        let mut has_mut = false;
        if let Some(arr) = inner.get("content").and_then(Value::as_array) {
            for item in arr {
                match item.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = item.get("text").and_then(Value::as_str) {
                            text_parts.push(t.to_owned());
                        }
                    }
                    Some("tool_use") => {
                        let name = item.get("name").and_then(Value::as_str).unwrap_or("Bash");
                        if is_mutating_tool_name(name) {
                            has_mut = true;
                        }
                        let cmd = item
                            .pointer("/input/command")
                            .and_then(Value::as_str)
                            .or_else(|| item.pointer("/input/file_path").and_then(Value::as_str))
                            .map(|s| scrub_session_privacy(s, redactor));
                        if let Some(id) = item.get("id").and_then(Value::as_str) {
                            state
                                .tool_calls_by_id
                                .insert(id.to_owned(), (name.to_owned(), cmd));
                        }
                    }
                    _ => {}
                }
            }
        } else {
            let t = extract_payload_text(inner);
            if !t.is_empty() {
                text_parts.push(t);
            }
        }
        let cleaned = clean_human_or_assistant_text(&text_parts.join("\n"), redactor);
        if !cleaned.is_empty() || has_mut {
            state.events.push(SessionEvent::AssistantTurn {
                step_label,
                text: cleaned,
                has_code_mutation: has_mut,
            });
        }
    } else if role == "user" {
        let mut human_parts = Vec::new();
        if let Some(arr) = inner.get("content").and_then(Value::as_array) {
            for (sub_idx, item) in arr.iter().enumerate() {
                match item.get("type").and_then(Value::as_str) {
                    Some("tool_result") => {
                        let tuid = item
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let (tool_name, cmd_hint) = state
                            .tool_calls_by_id
                            .remove(tuid)
                            .unwrap_or_else(|| ("Bash".into(), None));
                        let is_err_flag =
                            item.get("is_error").and_then(Value::as_bool) == Some(true);
                        let raw_out = item
                            .get("content")
                            .map(extract_value_text)
                            .unwrap_or_default();
                        let scrubbed = scrub_session_privacy(&raw_out, redactor);
                        let is_mut = is_mutating_tool_name(&tool_name);
                        let (mut failed, mut succeeded) =
                            classify_command_output(&scrubbed, cmd_hint.as_deref());
                        if is_err_flag
                            && has_meaningful_failure_signal(&scrubbed, cmd_hint.as_deref())
                        {
                            failed = true;
                            succeeded = false;
                        } else if !is_err_flag
                            && !failed
                            && tool_name == "Bash"
                            && !scrubbed.trim().is_empty()
                        {
                            succeeded = true;
                        }
                        if failed || succeeded || is_mut {
                            state.events.push(SessionEvent::ToolExecution {
                                step_label: format!("{step_label}:tr{sub_idx}"),
                                command_hint: cmd_hint,
                                output: scrubbed,
                                failed,
                                succeeded,
                                is_code_mutation: is_mut,
                            });
                        }
                    }
                    Some("text") => {
                        if let Some(t) = item.get("text").and_then(Value::as_str) {
                            human_parts.push(t.to_owned());
                        }
                    }
                    _ => {}
                }
            }
        } else {
            let t = extract_payload_text(inner);
            if !t.is_empty() {
                human_parts.push(t);
            }
        }
        let cleaned = clean_human_or_assistant_text(&human_parts.join("\n"), redactor);
        if !cleaned.is_empty() {
            state.events.push(SessionEvent::UserTurn {
                step_label,
                text: cleaned,
            });
        }
    }
}

fn parse_markdown_or_tagged_dialogue(raw: &str, redactor: &SecretRedactor) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    let mut current_role: Option<&str> = None;
    let mut current_lines: Vec<String> = Vec::new();
    let mut block_idx = 0usize;

    let flush_block = |role_opt: Option<&str>,
                       lines: &mut Vec<String>,
                       idx: &mut usize,
                       out: &mut Vec<SessionEvent>| {
        let Some(role) = role_opt else {
            lines.clear();
            return;
        };
        let joined = clean_human_or_assistant_text(&lines.join("\n"), redactor);
        lines.clear();
        if joined.is_empty() {
            return;
        }
        let label = format!("turn:{}", *idx);
        *idx += 1;
        if role == "user" {
            out.push(SessionEvent::UserTurn {
                step_label: label,
                text: joined,
            });
        } else {
            let (failed, succeeded) = classify_command_output(&joined, None);
            let has_mut = joined.contains("edit_file")
                || joined.contains("apply_patch")
                || joined.contains("已修改")
                || joined.contains("Updated file");
            out.push(SessionEvent::AssistantTurn {
                step_label: label.clone(),
                text: joined.clone(),
                has_code_mutation: has_mut,
            });
            if failed || succeeded {
                out.push(SessionEvent::ToolExecution {
                    step_label: format!("{label}:exec"),
                    command_hint: None,
                    output: joined,
                    failed,
                    succeeded,
                    is_code_mutation: has_mut,
                });
            }
        }
    };

    for line in raw.lines() {
        let trimmed = line.trim();
        let rest_after_turn = if let Some(after_open) = trimmed.strip_prefix("[turn:") {
            after_open
                .split_once(']')
                .map(|(_, r)| r.trim_start())
                .unwrap_or(trimmed)
        } else {
            trimmed
        };
        if let Some(after_spk) = rest_after_turn.strip_prefix("[speaker:") {
            flush_block(
                current_role,
                &mut current_lines,
                &mut block_idx,
                &mut events,
            );
            current_role = Some("user");
            if let Some((_, body)) = after_spk.split_once(']') {
                if !body.trim().is_empty() {
                    current_lines.push(body.trim().to_owned());
                }
            }
            continue;
        }
        if let Some(body) = rest_after_turn.strip_prefix("[assistant]") {
            flush_block(
                current_role,
                &mut current_lines,
                &mut block_idx,
                &mut events,
            );
            current_role = Some("assistant");
            if !body.trim().is_empty() {
                current_lines.push(body.trim().to_owned());
            }
            continue;
        }
        if let Some(hdr) = trimmed.strip_prefix("## ") {
            let lower = hdr.to_lowercase();
            if lower.starts_with("用户") || lower.starts_with("user") || lower.starts_with("human")
            {
                flush_block(
                    current_role,
                    &mut current_lines,
                    &mut block_idx,
                    &mut events,
                );
                current_role = Some("user");
                continue;
            }
            if lower.starts_with("assistant")
                || lower.starts_with("codex")
                || lower.starts_with("claude")
                || lower.starts_with("助手")
            {
                flush_block(
                    current_role,
                    &mut current_lines,
                    &mut block_idx,
                    &mut events,
                );
                current_role = Some("assistant");
                continue;
            }
        }
        current_lines.push(line.to_owned());
    }
    flush_block(
        current_role,
        &mut current_lines,
        &mut block_idx,
        &mut events,
    );
    events
}

fn extract_antigravity_bg_task_outputs(raw: &str) -> Vec<String> {
    let marker = "finished with result:";
    if !raw.contains("Task id \"") || !raw.contains(marker) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for segment in raw.split("Task id \"") {
        let Some((_, after_marker)) = segment.split_once(marker) else {
            continue;
        };
        let body = after_marker
            .split("</SYSTEM_MESSAGE>")
            .next()
            .unwrap_or(after_marker)
            .trim();
        if !body.is_empty() {
            out.push(body.to_owned());
        }
    }
    out
}

fn extract_codex_tool_output(payload: &Value) -> (String, Option<i64>) {
    let Some(out_val) = payload.get("output") else {
        return (String::new(), None);
    };
    if let Some(s) = out_val.as_str() {
        if let Ok(parsed) = serde_json::from_str::<Value>(s) {
            let text = parsed
                .get("output")
                .and_then(Value::as_str)
                .unwrap_or(s)
                .to_owned();
            let code = parsed
                .pointer("/metadata/exit_code")
                .and_then(Value::as_i64);
            return (text, code);
        }
        let code = parse_exit_code_from_text(s);
        return (s.to_owned(), code);
    }
    let text = extract_value_text(out_val);
    let code = out_val
        .pointer("/metadata/exit_code")
        .and_then(Value::as_i64)
        .or_else(|| parse_exit_code_from_text(&text));
    (text, code)
}

fn parse_exit_code_from_text(text: &str) -> Option<i64> {
    for prefix in [
        "Command exited with code ",
        "Process exited with code ",
        "Exit code: ",
        "exit code: ",
    ] {
        if let Some((_, rest)) = text.rsplit_once(prefix) {
            let digits: String = rest
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '-')
                .collect();
            if let Ok(code) = digits.parse::<i64>() {
                return Some(code);
            }
        }
    }
    None
}

fn extract_payload_text(container: &Value) -> String {
    if let Some(c) = container.get("content") {
        let s = extract_value_text(c);
        if !s.is_empty() {
            return s;
        }
    }
    for key in ["message", "text", "output"] {
        if let Some(s) = container.get(key).and_then(Value::as_str) {
            if !s.is_empty() {
                return s.to_owned();
            }
        }
    }
    String::new()
}

fn extract_value_text(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|item| {
                item.as_str().map(str::to_owned).or_else(|| {
                    item.get("text")
                        .or_else(|| item.get("content"))
                        .map(extract_value_text)
                })
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(map) => map
            .get("text")
            .or_else(|| map.get("content"))
            .or_else(|| map.get("output"))
            .map(extract_value_text)
            .unwrap_or_default(),
        _ => String::new(),
    }
}
