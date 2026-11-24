// Non-intrusive async hook behavior and always-exit-0 invariants adapted from
// claude-mem main (Apache-2.0); see THIRD_PARTY_NOTICES.md.
use std::collections::BTreeSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use kp_model::{
    format_date_iso_in_tz, system_time_zone, ActorId, ContentHash, ContrastKind, EpochMs, FactId,
    FactKind, ModelRoutePolicy, PublicAudience, PublicIngestOp, PublicIngestRecord,
    PublicIngestSource, PublicSpeaker, SecretRedactor, TimeZone,
};
use kp_retrieve::guard::{
    evaluate_tool_guards, extract_tool_targets, is_guarded_tool, GuardCheckInput, GuardVerdict,
};
use kp_retrieve::{
    retrieve_prompt_context, PromptBudgetConfig, DEFAULT_MIN_RELEVANCE_SCORE,
    MAX_PROMPT_CONTEXT_ITEMS, MAX_PROMPT_CONTEXT_TOKENS,
};
use kp_store::{Episode, Store};
use serde_json::{json, Value};

use crate::hook_cursor::{
    effective_complete_prefix_len, extract_turn_in_tz, parse_transcript_slice,
    read_delta_transcript, save_cursor, DeltaTranscript,
};
use crate::{connect::EVENTS, IngestError, Result};

pub use crate::hook_cursor::{
    effective_complete_transcript_len, estimate_tokens, filter_echo, infer_client_kind,
    inspect_transcript_cursor_file, is_low_signal_ack, strip_host_system_blocks,
};
pub use kp_retrieve::format_fact_kp_ref as format_fact_ref_id;

pub const INTERNAL_EXTRACTION_ENV: &str = "KP_INTERNAL_EXTRACTION";
pub const MAX_PROMPT_TOKENS: usize = MAX_PROMPT_CONTEXT_TOKENS;
pub const MAX_PROMPT_ITEMS: usize = MAX_PROMPT_CONTEXT_ITEMS;
pub const PROMPT_RELEVANCE_THRESHOLD: f64 = DEFAULT_MIN_RELEVANCE_SCORE;

/// Anti-recursion check: background `kp` extraction child processes set `KP_INTERNAL_EXTRACTION=1`.
pub fn is_internal_extraction() -> bool {
    std::env::var_os(INTERNAL_EXTRACTION_ENV).is_some()
}

/// Returns `true` when `(event, payload)` can execute purely against `Store::open_readonly`
/// (`session-start`, `user-prompt-submit`, and Claude Code `pre-tool-use`).
pub fn is_readonly_hook_invocation(event: &str, payload: &Value) -> bool {
    match event {
        "session-start" | "user-prompt-submit" => true,
        "pre-tool-use" => {
            let tool_name = string(payload, "tool_name").unwrap_or("");
            !is_codex_pre_tool_payload(payload, tool_name)
        }
        _ => false,
    }
}

/// Executes read-only hook events (`session-start`, `user-prompt-submit`, and Claude `pre-tool-use`)
/// strictly against `&Store` (`Store::open_readonly` compatible, zero write locks).
pub fn run_readonly(
    store: &Store,
    event: &str,
    payload: &Value,
    actor: &ActorId,
    at: EpochMs,
) -> Result<Value> {
    if is_internal_extraction() {
        return Ok(json!({}));
    }
    if store.member_role(actor)?.is_none() {
        return Ok(json!({}));
    }
    let Some(wire) = EVENTS
        .iter()
        .find(|(name, _)| *name == event)
        .map(|(_, wire)| *wire)
    else {
        return Ok(json!({}));
    };
    match event {
        "session-start" => Ok(json!({})),
        "user-prompt-submit" => user_prompt_submit(store, payload, actor, at, wire),
        "pre-tool-use" => pre_tool_readonly(store, payload, actor, at, wire),
        _ => Err(IngestError::Invalid(
            "mutating hook event requires read-write Store",
        )),
    }
}

pub async fn run(
    store: &mut Store,
    event: &str,
    payload: &Value,
    actor: &ActorId,
    at: EpochMs,
) -> Result<Value> {
    run_with_policy(store, event, payload, actor, at, &ModelRoutePolicy::HostCli).await
}

pub async fn run_with_policy(
    store: &mut Store,
    event: &str,
    payload: &Value,
    actor: &ActorId,
    at: EpochMs,
    global_policy: &ModelRoutePolicy,
) -> Result<Value> {
    if is_internal_extraction() {
        return Ok(json!({}));
    }
    if store.member_role(actor)?.is_none() {
        return Ok(json!({}));
    }
    let Some(wire) = EVENTS
        .iter()
        .find(|(name, _)| *name == event)
        .map(|(_, wire)| *wire)
    else {
        return Ok(json!({}));
    };
    match event {
        // SessionStart defaults to zero injection (zero system-prompt bloat).
        "session-start" => Ok(json!({})),
        "user-prompt-submit" => user_prompt_submit(store, payload, actor, at, wire),
        "pre-tool-use" => pre_tool(store, payload, actor, at, wire),
        "post-tool-use" => post_tool(store, payload, actor, at),
        "stop" => capture_session_with_policy(store, payload, actor, at, false, global_policy),
        "session-end" => {
            capture_session_with_policy(store, payload, actor, at, true, global_policy)
        }
        _ => Ok(json!({})),
    }
}

pub async fn run_with_path(
    library_path: Option<&Path>,
    store: &mut Store,
    event: &str,
    payload: &Value,
    actor: &ActorId,
    at: EpochMs,
) -> Result<Value> {
    run_with_path_and_policy(
        library_path,
        store,
        event,
        payload,
        actor,
        at,
        &ModelRoutePolicy::HostCli,
    )
    .await
}

pub async fn run_with_path_and_policy(
    library_path: Option<&Path>,
    store: &mut Store,
    event: &str,
    payload: &Value,
    actor: &ActorId,
    at: EpochMs,
    global_policy: &ModelRoutePolicy,
) -> Result<Value> {
    let _ = library_path;
    run_with_policy(store, event, payload, actor, at, global_policy).await
}

fn context_output(event: &str, context: &str) -> Value {
    json!({"hookSpecificOutput":{"hookEventName":event,"additionalContext":context}})
}

fn user_prompt_submit(
    store: &Store,
    payload: &Value,
    actor: &ActorId,
    at: EpochMs,
    wire: &str,
) -> Result<Value> {
    let query = string(payload, "prompt")
        .or_else(|| string(payload, "user_prompt"))
        .unwrap_or("")
        .trim();
    let wrapper_overhead = estimate_tokens("<kp-context>\n\n</kp-context>") + 2 * MAX_PROMPT_ITEMS;
    let (raw_text, _, _, _) = query_prompt_context_raw(
        store,
        query,
        actor,
        at,
        MAX_PROMPT_TOKENS.saturating_sub(wrapper_overhead),
        MAX_PROMPT_ITEMS,
    )?;
    Ok(wrap_prompt_context_output(wire, &raw_text))
}

/// Evaluates bounded candidate prompt context (`<= max_tokens`, `<= max_items`) including active RSI skills.
pub fn query_prompt_context_raw(
    store: &Store,
    raw_query: &str,
    actor: &ActorId,
    at: EpochMs,
    max_tokens: usize,
    max_items: usize,
) -> Result<(String, usize, usize, Vec<FactId>)> {
    if max_tokens == 0 || max_items == 0 {
        return Ok((String::new(), 0, 0, Vec::new()));
    }
    let config = PromptBudgetConfig {
        max_tokens: max_tokens.clamp(1, MAX_PROMPT_TOKENS),
        max_items: max_items.clamp(1, MAX_PROMPT_ITEMS),
        min_score: PROMPT_RELEVANCE_THRESHOLD,
        time_zone: system_time_zone(),
        candidate_limit: 24,
    };
    let ctx = retrieve_prompt_context(store, actor, at, raw_query, &config)?;
    if ctx.text.is_empty() {
        return Ok((String::new(), 0, 0, Vec::new()));
    }
    let normalized = ensure_fact_ref_prefixes(&ctx.text);
    let total_tokens = estimate_tokens(&normalized);
    let cited_ids = ctx
        .items
        .iter()
        .filter_map(|item| item.ref_id.parse::<FactId>().ok())
        .collect();
    Ok((normalized, ctx.items.len(), total_tokens, cited_ids))
}

/// Wraps raw prompt context lines in `<kp-context>...</kp-context>`, strictly enforcing `MAX_PROMPT_TOKENS`.
pub fn wrap_prompt_context_output(wire: &str, raw_text: &str) -> Value {
    if raw_text.trim().is_empty() {
        return json!({});
    }
    let normalized = ensure_fact_ref_prefixes(raw_text);
    let mut lines: Vec<&str> = normalized.lines().collect();
    while !lines.is_empty() {
        let wrapped = format!("<kp-context>\n{}\n</kp-context>", lines.join("\n"));
        if estimate_tokens(&wrapped) <= MAX_PROMPT_TOKENS {
            return context_output(wire, &wrapped);
        }
        lines.pop();
    }
    json!({})
}

/// Ensures legacy bare UUID FactIds still carry `kp-ref:fact_`.
pub fn ensure_fact_ref_prefixes(text: &str) -> String {
    text.lines()
        .map(|line| {
            if let Some(pos) = line.find("kp-ref:") {
                let after = &line[pos + "kp-ref:".len()..];
                if !after.starts_with("fact_")
                    && !after.starts_with("src_")
                    && !after.starts_with("chunk_")
                {
                    let mut out = String::with_capacity(line.len() + 5);
                    out.push_str(&line[..pos + "kp-ref:".len()]);
                    out.push_str("fact_");
                    out.push_str(after);
                    return out;
                }
            }
            line.to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn pre_tool_readonly(
    store: &Store,
    payload: &Value,
    actor: &ActorId,
    at: EpochMs,
    wire: &str,
) -> Result<Value> {
    let Some(tool_name) = string(payload, "tool_name") else {
        return Ok(json!({}));
    };
    let verdict = evaluate_pre_tool_verdict(store, payload, actor, at)?;
    if !verdict.is_blocked() {
        return Ok(json!({}));
    }
    let reasons: Vec<String> = verdict
        .hits
        .iter()
        .map(|h| h.formatted_reason.clone())
        .collect();
    let allow_refs = verdict.allow_refs();
    Ok(format_pre_tool_decision(
        wire,
        payload,
        tool_name,
        &reasons,
        &allow_refs,
    ))
}

fn pre_tool(
    store: &mut Store,
    payload: &Value,
    actor: &ActorId,
    at: EpochMs,
    wire: &str,
) -> Result<Value> {
    let Some(tool_name) = string(payload, "tool_name") else {
        return Ok(json!({}));
    };
    let verdict = evaluate_pre_tool_verdict(store, payload, actor, at)?;
    if !verdict.is_blocked() {
        return Ok(json!({}));
    }

    // Host protocol split:
    // - Codex (`_kp_client == "codex"` or Codex payload): atomically consume all `allow_guard_once`
    //   grants across all matched hits (all-or-nothing), or emit `"deny"` + `kp allow <missing_refs>`.
    // - Claude Code: never consume `allow_guard_once` grants; emit `"ask"` for interactive host confirmation.
    let is_codex = is_codex_pre_tool_payload(payload, tool_name);
    let (reasons, allow_refs) = if is_codex {
        let all_ids: Vec<FactId> = verdict.hits.iter().map(|h| h.fact_id.clone()).collect();
        let missing = store.try_consume_all_guard_allows(&all_ids, at)?;
        if missing.is_empty() {
            return Ok(json!({}));
        }
        let missing_set: BTreeSet<&FactId> = missing.iter().collect();
        let mut reasons = Vec::new();
        let mut allow_refs = Vec::new();
        for hit in verdict.hits {
            if missing_set.contains(&hit.fact_id) {
                reasons.push(hit.formatted_reason);
                allow_refs.push(hit.kp_ref);
            }
        }
        (reasons, allow_refs)
    } else {
        let reasons = verdict
            .hits
            .iter()
            .map(|h| h.formatted_reason.clone())
            .collect();
        let allow_refs = verdict.allow_refs();
        (reasons, allow_refs)
    };

    Ok(format_pre_tool_decision(
        wire,
        payload,
        tool_name,
        &reasons,
        &allow_refs,
    ))
}

/// Evaluates active authorized hard constraints and derived `CollisionWarning`s (with viewer-visible
/// supporting premises) matching `payload` in strictly read-only mode (`&Store`), without consuming grants.
pub fn evaluate_pre_tool_verdict(
    store: &Store,
    payload: &Value,
    actor: &ActorId,
    at: EpochMs,
) -> Result<GuardVerdict> {
    let Some(tool_name) = string(payload, "tool_name") else {
        return Ok(GuardVerdict::default());
    };
    if !is_guarded_tool(tool_name) {
        return Ok(GuardVerdict::default());
    }
    Ok(evaluate_tool_guards(
        store,
        &GuardCheckInput {
            tool_name: tool_name.to_owned(),
            tool_input: payload.clone(),
            cwd: string(payload, "cwd").map(str::to_owned),
            viewer: actor.clone(),
            at,
            time_zone: system_time_zone(),
        },
    )?)
}

/// Evaluates active authorized hard constraints matching `payload` as `(FactId, formatted_reason)` pairs.
pub fn evaluate_pre_tool_guards(
    store: &Store,
    payload: &Value,
    actor: &ActorId,
    at: EpochMs,
) -> Result<Vec<(FactId, String)>> {
    let verdict = evaluate_pre_tool_verdict(store, payload, actor, at)?;
    Ok(verdict
        .hits
        .into_iter()
        .map(|h| (h.fact_id, h.formatted_reason))
        .collect())
}

/// Formats client-split `PreToolUse` JSON (`"ask"` on Claude Code; `"deny"` + `kp allow` on Codex).
pub fn format_pre_tool_decision(
    wire: &str,
    payload: &Value,
    tool_name: &str,
    reasons: &[String],
    allow_refs: &[String],
) -> Value {
    if reasons.is_empty() {
        return json!({});
    }
    let is_codex = is_codex_pre_tool_payload(payload, tool_name);
    let (decision, final_reason) = if is_codex {
        (
            "deny",
            format!(
                "{}\n先问用户；用户同意后，请用户执行 kp allow {}。",
                reasons.join("\n"),
                allow_refs.join(" ")
            ),
        )
    } else {
        ("ask", reasons.join("\n"))
    };

    json!({
        "hookSpecificOutput": {
            "hookEventName": wire,
            "permissionDecision": decision,
            "permissionDecisionReason": final_reason
        }
    })
}

pub fn is_codex_pre_tool_payload(payload: &Value, tool_name: &str) -> bool {
    if let Some(explicit) = string(payload, "_kp_client").or_else(|| string(payload, "client")) {
        let trimmed = explicit.trim();
        if trimmed.eq_ignore_ascii_case("codex") {
            return true;
        }
        if matches!(
            trimmed.to_ascii_lowercase().as_str(),
            "claude-code" | "claude_code" | "claude"
        ) {
            return false;
        }
    }
    if let Some(tp) = string(payload, "transcript_path") {
        if tp.contains(".codex") || tp.contains("rollout-") {
            return true;
        }
        if tp.contains(".claude") {
            return false;
        }
    }
    if matches!(
        tool_name,
        "apply_patch"
            | "exec_command"
            | "shell"
            | "shell_command"
            | "local_shell"
            | "command_execution"
    ) {
        return true;
    }
    payload.get("turn_id").is_some()
}

fn post_tool(store: &mut Store, payload: &Value, actor: &ActorId, at: EpochMs) -> Result<Value> {
    let Some(session) = string(payload, "session_id").filter(|s| !s.trim().is_empty()) else {
        return Ok(json!({}));
    };
    let Some(tool) = string(payload, "tool_name") else {
        return Ok(json!({}));
    };
    let input = &payload["tool_input"];
    let targets = extract_tool_targets(payload);
    let files = targets.files;
    let commands = targets.commands;
    let Some(primary_target) = files
        .first()
        .or_else(|| commands.first())
        .map(String::as_str)
    else {
        return Ok(json!({}));
    };
    let response = payload
        .get("tool_response")
        .or_else(|| payload.get("tool_result"))
        .unwrap_or(&Value::Null);
    if payload.get("is_interrupt").and_then(Value::as_bool) == Some(true) {
        return Ok(json!({"episodes":0}));
    }
    let outcome = failed(response).or_else(|| failed(payload)).or_else(|| {
        match string(payload, "hook_event_name") {
            Some("PostToolUseFailure") => Some(true),
            Some("PostToolUse") => Some(false),
            _ => None,
        }
    });
    let Some(failed) = outcome else {
        return Ok(json!({"episodes":0}));
    };
    let redactor = SecretRedactor::new()?;
    let trace = redactor.redact_json(&json!({
        "tool_name": tool,
        "tool_input": input,
        "tool_response": response,
        "error": payload.get("error"),
        "step_ref": payload.get("tool_use_id")
    }));
    let target = if let Some(target) = string(payload, "target").or_else(|| string(input, "target"))
    {
        format!("target:{}:{target}", string(payload, "cwd").unwrap_or(""))
    } else if !files.is_empty() {
        let cwd = string(payload, "cwd").unwrap_or("");
        format!("file:{}", Path::new(cwd).join(primary_target).display())
    } else {
        format!(
            "command:{}:{primary_target}",
            string(payload, "cwd").unwrap_or("")
        )
    };
    let target = redactor.redact(&target);
    let session = redactor.redact(session);
    let mut count = store.record_tool_outcome(actor, &session, &target, failed, &trace, at)?;
    if !failed {
        if let Some(working) = string(payload, "working_fact_id") {
            if let Ok(working_id) = working.parse() {
                count += crate::signals::cross_avatar(store, &working_id, actor, &session, at)?;
            }
        }
        if let Some(locus) = string(payload, "target").or_else(|| files.first().map(String::as_str))
        {
            let dead_ends = store
                .visible_facts_by_kinds(
                    actor,
                    at,
                    &[FactKind::ClosedNegative, FactKind::CollisionWarning],
                )?
                .into_iter()
                .filter(|f| {
                    f.author != *actor
                        && (f.subject == locus
                            || f.source_anchor.as_ref().and_then(|a| a.symbol.as_deref())
                                == Some(locus))
                })
                .collect::<Vec<_>>();
            for dead in dead_ends {
                count += crate::signals::record(
                    store,
                    Episode {
                        signal_kind: ContrastKind::CrossAvatarContrast,
                        session_id: session.clone(),
                        before_trace: json!({"fact_id":dead.id,"content":dead.object,"target":locus,"success":false}),
                        after_trace: json!({"content":trace,"target":locus,"success":true,"attribution":payload.get("attribution")}),
                        step_ref: string(payload, "tool_use_id")
                            .unwrap_or("tool:recovery")
                            .into(),
                        environment: json!({"actor":actor}),
                    },
                    actor,
                    &[dead.id],
                    at,
                )?;
            }
        }
    }
    Ok(json!({"episodes":count}))
}

fn failed(response: &Value) -> Option<bool> {
    response
        .get("exit_code")
        .or_else(|| response.get("exitCode"))
        .and_then(Value::as_i64)
        .map(|code| code != 0)
        .or_else(|| {
            response
                .get("is_error")
                .or_else(|| response.get("isError"))
                .and_then(Value::as_bool)
        })
        .or_else(|| {
            response
                .get("success")
                .and_then(Value::as_bool)
                .map(|ok| !ok)
        })
        .or_else(|| {
            response
                .get("error")
                .filter(|e| !e.is_null() && **e != Value::Bool(false))
                .map(|_| true)
        })
}

pub fn capture_session_with_policy(
    store: &mut Store,
    payload: &Value,
    actor: &ActorId,
    at: EpochMs,
    is_session_end: bool,
    global_policy: &ModelRoutePolicy,
) -> Result<Value> {
    capture_session_in_tz_with_policy(
        store,
        payload,
        actor,
        at,
        is_session_end,
        &system_time_zone(),
        global_policy,
    )
}

/// Captures incremental `Stop` / `SessionEnd` session turns using explicit IANA `tz` to resolve
/// offsetless turn timestamps and split human turns across local `00:00` day boundaries.
pub fn capture_session_in_tz(
    store: &mut Store,
    payload: &Value,
    actor: &ActorId,
    at: EpochMs,
    is_session_end: bool,
    tz: &TimeZone,
) -> Result<Value> {
    capture_session_in_tz_with_policy(
        store,
        payload,
        actor,
        at,
        is_session_end,
        tz,
        &ModelRoutePolicy::HostCli,
    )
}

/// Captures incremental `Stop` / `SessionEnd` session turns through the canonical Public Ingest
/// Contract (`Store::ingest_public_session_subbatch`) while honoring `global_policy`.
pub fn capture_session_in_tz_with_policy(
    store: &mut Store,
    payload: &Value,
    actor: &ActorId,
    at: EpochMs,
    is_session_end: bool,
    tz: &TimeZone,
    global_policy: &ModelRoutePolicy,
) -> Result<Value> {
    let Some(raw_session) = string(payload, "session_id").filter(|s| !s.trim().is_empty()) else {
        return Ok(json!({}));
    };
    let redactor = SecretRedactor::new()?;
    let session = redactor.redact(raw_session);

    let delta = read_delta_transcript(store, &session, payload, at, is_session_end)?;

    let (indexed_turns, slice_start_byte, slice_end_byte, next_cursor) = match delta {
        DeltaTranscript::Unchanged { self_echo_filtered } => {
            return Ok(json!({
                "captured": false,
                "duplicate": true,
                "self_echo_filtered": self_echo_filtered,
                "episodes": 0
            }));
        }
        DeltaTranscript::Slice {
            turns,
            slice_start_byte,
            slice_end_byte,
            next_cursor,
        } => (turns, slice_start_byte, slice_end_byte, next_cursor),
    };

    let mut filtered = false;
    let mut skipped_low_signal = 0_usize;
    let mut turns = Vec::new();
    let mut turn_spans = Vec::new();
    for (index, turn_start, turn_end, raw_turn) in indexed_turns {
        let Some(parsed) = extract_turn_in_tz(&raw_turn, tz) else {
            continue;
        };
        let (after_echo, echo) = filter_echo(&parsed.raw_text);
        filtered |= echo;
        let cleaned = strip_host_system_blocks(&after_echo);
        if cleaned.trim().is_empty() {
            continue;
        }
        if is_low_signal_ack(&cleaned) {
            skipped_low_signal += 1;
            continue;
        }
        let meta = &parsed.meta_source;
        turns.push(json!({
            "role": parsed.role,
            "content": redactor.redact(&cleaned),
            "step_ref": format!("turn:{index}"),
            "timestamp_ms": parsed.timestamp_ms.map(|ts| ts.0),
            "actor": meta.get("actor").or_else(|| meta.get("model")),
            "target": meta.get("target"),
            "success": meta.get("success"),
            "fact_id": meta.get("fact_id"),
            "attribution": meta.get("attribution"),
        }));
        turn_spans.push((turn_start, turn_end));
    }

    let turns = redactor.redact_json(&Value::Array(turns));
    let turns = turns
        .as_array()
        .ok_or(IngestError::Invalid("invalid transcript"))?;
    let episodes = sniff(turns, &session, actor);
    let subbatches = partition_turns_by_local_date(
        turns,
        &turn_spans,
        actor,
        tz,
        at,
        slice_start_byte,
        slice_end_byte,
    );
    if subbatches.is_empty() {
        save_cursor(store, &next_cursor, is_session_end)?;
        return Ok(json!({
            "captured": false,
            "self_echo_filtered": filtered,
            "skipped_low_signal": skipped_low_signal,
            "episodes": 0
        }));
    }

    let job_priority = if is_session_end { 10 } else { 0 };
    let mut first_stored = None;
    let mut first_source_id = None;
    let mut any_conflict = false;
    for (sub_idx, (body, ref_time, sub_start_byte, sub_end_byte)) in
        subbatches.into_iter().enumerate()
    {
        let body_hash = ContentHash::digest(&body);
        let public_rec = session_subbatch_to_public_ingest_record(
            &session,
            actor,
            sub_start_byte,
            sub_end_byte,
            &body,
            &body_hash,
            ref_time,
        )?;
        let span_at = EpochMs(at.0.saturating_add(i64::try_from(sub_idx).unwrap_or(0)));
        match store.ingest_public_session_subbatch(&kp_store::PublicSessionSubbatchSpec {
            caller: actor,
            record: &public_rec,
            global_policy,
            session_id: &session,
            start_byte: sub_start_byte,
            end_byte: sub_end_byte,
            job_priority,
            self_echo_filtered: filtered,
            at: span_at,
        }) {
            Ok((stored, true)) => {
                if first_source_id.is_none() {
                    first_source_id = Some(stored.source_id.clone());
                }
                if first_stored.is_none() {
                    first_stored = Some(stored);
                }
            }
            Ok((stored, false)) => {
                if first_source_id.is_none() {
                    first_source_id = Some(stored.source_id);
                }
                any_conflict = true;
            }
            Err(kp_store::StoreError::Conflict) => {
                any_conflict = true;
            }
            Err(error) => return Err(error.into()),
        }
    }

    let mut result = match first_stored {
        Some(stored) => json!({
            "captured": true,
            "source": stored,
            "self_echo_filtered": filtered,
            "skipped_low_signal": skipped_low_signal,
            "cursor_byte_offset": next_cursor.last_byte_offset
        }),
        None if any_conflict => json!({
            "captured": false,
            "duplicate": true,
            "self_echo_filtered": filtered,
            "skipped_low_signal": skipped_low_signal
        }),
        None => json!({
            "captured": false,
            "self_echo_filtered": filtered,
            "skipped_low_signal": skipped_low_signal
        }),
    };

    save_cursor(store, &next_cursor, is_session_end)?;

    let mut count = 0;
    for mut episode in episodes {
        let mut premises = episode
            .before_trace
            .get("fact_id")
            .and_then(Value::as_str)
            .map(str::parse)
            .transpose()?
            .into_iter()
            .collect::<Vec<FactId>>();
        if let Some(ids) = episode
            .after_trace
            .pointer("/attribution/cited_premise_ids")
        {
            premises.extend(serde_json::from_value::<Vec<FactId>>(ids.clone())?);
        }
        premises.sort();
        premises.dedup();
        if let Some(ref source_id) = first_source_id {
            episode.environment["source_id"] = json!(source_id);
        }
        count += crate::signals::record(store, episode, actor, &premises, at)?;
    }
    result["episodes"] = json!(count);
    Ok(result)
}

/// Formats cleaned session turns and splits into separate
/// `(markdown_body, reference_time, sub_start_byte, sub_end_byte)` sub-batches ONLY when human
/// (`role == "user"`) turns with timestamps cross local `00:00` in `tz`. Byte spans partition
/// `[slice_start_byte..slice_end_byte]` monotonically with final `sub_end_byte == slice_end_byte`.
fn partition_turns_by_local_date(
    turns: &[Value],
    turn_spans: &[(u64, u64)],
    actor: &ActorId,
    tz: &TimeZone,
    fallback_at: EpochMs,
    slice_start_byte: u64,
    slice_end_byte: u64,
) -> Vec<(String, EpochMs, u64, u64)> {
    let mut groups: Vec<(Vec<&Value>, Option<EpochMs>, u64)> = Vec::new();
    let mut current_turns: Vec<&Value> = Vec::new();
    let mut current_last_end = slice_start_byte;
    let mut current_date: Option<String> = None;
    let mut last_human_ts: Option<EpochMs> = None;

    for (idx, t) in turns.iter().enumerate() {
        let (_, t_end) = turn_spans
            .get(idx)
            .copied()
            .unwrap_or((slice_start_byte, slice_end_byte));
        if string(t, "role") == Some("user") {
            if let Some(ts) = t.get("timestamp_ms").and_then(Value::as_i64).map(EpochMs) {
                let date = format_date_iso_in_tz(ts, tz);
                if current_date
                    .as_deref()
                    .is_some_and(|prev| prev != date.as_str())
                    && !current_turns.is_empty()
                {
                    groups.push((
                        std::mem::take(&mut current_turns),
                        last_human_ts,
                        current_last_end,
                    ));
                }
                current_date = Some(date);
                last_human_ts = Some(ts);
            }
        }
        current_turns.push(t);
        current_last_end = current_last_end.max(t_end);
    }
    if !current_turns.is_empty() {
        groups.push((current_turns, last_human_ts, current_last_end));
    }

    let rendered: Vec<(String, EpochMs, u64)> = groups
        .into_iter()
        .filter_map(|(slice, human_ts, raw_end)| {
            let body = format_turns_markdown(&slice, actor);
            (!body.is_empty()).then_some((body, human_ts.unwrap_or(fallback_at), raw_end))
        })
        .collect();

    let total = rendered.len();
    let mut prev_end = slice_start_byte;
    rendered
        .into_iter()
        .enumerate()
        .map(|(idx, (body, ref_time, raw_end))| {
            let sub_start = prev_end;
            let sub_end = if idx + 1 == total {
                slice_end_byte.max(sub_start)
            } else {
                raw_end.clamp(sub_start, slice_end_byte.max(sub_start))
            };
            prev_end = sub_end;
            (body, ref_time, sub_start, sub_end)
        })
        .collect()
}

fn format_turns_markdown(turns: &[&Value], actor: &ActorId) -> String {
    turns
        .iter()
        .map(|t| {
            let role = string(t, "role").unwrap_or("unknown");
            let tag = match role {
                "user" => format!("[speaker:{actor}]"),
                "assistant" => "[assistant]".to_owned(),
                other => format!("[{other}]"),
            };
            string(t, "content")
                .unwrap_or("")
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| format!("{tag} {line}\n"))
                .collect::<String>()
        })
        .collect()
}

fn sniff(turns: &[Value], session: &str, actor: &ActorId) -> Vec<Episode> {
    turns
        .windows(2)
        .filter_map(|pair| {
            let before = &pair[0];
            let after = &pair[1];
            let correction = string(after, "content").unwrap_or("").trim().to_lowercase();
            let kind = if string(before, "role") == Some("assistant")
                && string(after, "role") == Some("user")
                && crate::signals::is_correction(&correction)
            {
                Some(ContrastKind::HumanCorrection)
            } else if before
                .get("target")
                .is_some_and(|t| !t.is_null() && Some(t) == after.get("target"))
                && before.get("success").and_then(Value::as_bool) == Some(false)
                && after.get("success").and_then(Value::as_bool) == Some(true)
            {
                if before
                    .get("actor")
                    .is_some_and(|a| !a.is_null() && Some(a) != after.get("actor"))
                    && after.get("actor").is_some_and(|a| !a.is_null())
                {
                    Some(ContrastKind::CrossAvatarContrast)
                } else {
                    Some(ContrastKind::FailureToRecovery)
                }
            } else {
                None
            }?;
            Some(Episode {
                signal_kind: kind,
                session_id: session.into(),
                before_trace: before.clone(),
                after_trace: after.clone(),
                step_ref: string(after, "step_ref").unwrap_or("").into(),
                environment: json!({"actor":actor}),
            })
        })
        .collect()
}

fn string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// Detailed outcome of ingesting a `[start_byte_offset..end_byte_offset]` daemon session slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionSliceIngestOutcome {
    pub committed_end_byte_offset: u64,
    pub subbatches_ingested: usize,
}

/// Parameters for capturing a coalesced `[start_byte_offset..end_byte_offset]` daemon transcript
/// slice through [`Store::ingest_public_session_subbatch`].
#[derive(Clone, Debug)]
pub struct DaemonSessionSliceSpec<'a> {
    pub raw_session: &'a str,
    pub transcript_path: &'a Path,
    pub start_byte_offset: u64,
    pub end_byte_offset: u64,
    pub actor: &'a ActorId,
    pub at: EpochMs,
    pub is_session_end: bool,
    pub tz: Option<&'a TimeZone>,
    pub global_policy: &'a ModelRoutePolicy,
}

/// Captures a coalesced `[start_byte_offset..end_byte_offset]` transcript slice handed off by `kp daemon`
/// into L0 `raw_sources`, `chunks`, `extraction_queue`, and RSI contrast episodes, returning both
/// `committed_end_byte_offset` and `subbatches_ingested`.
pub fn ingest_daemon_session_slice(
    store: &mut Store,
    spec: &DaemonSessionSliceSpec<'_>,
) -> Result<SessionSliceIngestOutcome> {
    let DaemonSessionSliceSpec {
        raw_session,
        transcript_path,
        start_byte_offset,
        end_byte_offset,
        actor,
        at,
        is_session_end,
        tz,
        global_policy,
    } = *spec;
    let owned_tz = if tz.is_none() {
        Some(system_time_zone())
    } else {
        None
    };
    let tz = tz
        .or(owned_tz.as_ref())
        .ok_or(IngestError::Invalid("missing timezone"))?;
    if is_internal_extraction() || raw_session.trim().is_empty() {
        return Ok(SessionSliceIngestOutcome {
            committed_end_byte_offset: start_byte_offset,
            subbatches_ingested: 0,
        });
    }
    if store.member_role(actor)?.is_none() {
        return Ok(SessionSliceIngestOutcome {
            committed_end_byte_offset: start_byte_offset,
            subbatches_ingested: 0,
        });
    }

    let mut file = std::fs::File::open(transcript_path)?;
    let effective_file_end = effective_complete_transcript_len(&mut file)?;
    let clamped = effective_file_end.min(end_byte_offset);
    let end = if clamped < effective_file_end {
        effective_complete_prefix_len(&mut file, clamped)?
    } else {
        clamped
    };
    if end <= start_byte_offset {
        return Ok(SessionSliceIngestOutcome {
            committed_end_byte_offset: start_byte_offset,
            subbatches_ingested: 0,
        });
    }

    let redactor = SecretRedactor::new()?;
    let session = redactor.redact(raw_session);
    let resume_start =
        store.advance_past_ingested_session_spans(&session, start_byte_offset, end)?;
    if resume_start >= end {
        return Ok(SessionSliceIngestOutcome {
            committed_end_byte_offset: end,
            subbatches_ingested: 0,
        });
    }

    let delta_len = usize::try_from(end - resume_start)
        .map_err(|_| IngestError::Invalid("transcript slice exceeds addressable memory"))?;
    let mut delta_bytes = vec![0_u8; delta_len];
    file.seek(SeekFrom::Start(resume_start))?;
    file.read_exact(&mut delta_bytes)?;
    let delta_str = std::str::from_utf8(&delta_bytes)
        .map_err(|_| IngestError::Invalid("transcript slice must be UTF-8 boundary"))?;
    let (raw_turns, _) = parse_transcript_slice(delta_str, resume_start, end, 0);
    if raw_turns.is_empty() {
        return Ok(SessionSliceIngestOutcome {
            committed_end_byte_offset: end,
            subbatches_ingested: 0,
        });
    }

    let mut filtered = false;
    let mut turns = Vec::new();
    let mut turn_spans = Vec::new();
    for (index, turn_start, turn_end, raw_turn) in raw_turns {
        let Some(parsed) = extract_turn_in_tz(&raw_turn, tz) else {
            continue;
        };
        let (after_echo, echo) = filter_echo(&parsed.raw_text);
        filtered |= echo;
        let cleaned = strip_host_system_blocks(&after_echo);
        if cleaned.trim().is_empty() || is_low_signal_ack(&cleaned) {
            continue;
        }
        let meta = &parsed.meta_source;
        turns.push(json!({
            "role": parsed.role,
            "content": redactor.redact(&cleaned),
            "step_ref": format!("turn:{index}"),
            "timestamp_ms": parsed.timestamp_ms.map(|ts| ts.0),
            "actor": meta.get("actor").or_else(|| meta.get("model")),
            "target": meta.get("target"),
            "success": meta.get("success"),
            "fact_id": meta.get("fact_id"),
            "attribution": meta.get("attribution"),
        }));
        turn_spans.push((turn_start, turn_end));
    }

    let turns_val = redactor.redact_json(&Value::Array(turns));
    let turns = turns_val
        .as_array()
        .ok_or(IngestError::Invalid("invalid transcript"))?;
    let episodes = sniff(turns, &session, actor);
    let subbatches =
        partition_turns_by_local_date(turns, &turn_spans, actor, tz, at, resume_start, end);
    if subbatches.is_empty() {
        return Ok(SessionSliceIngestOutcome {
            committed_end_byte_offset: end,
            subbatches_ingested: 0,
        });
    }

    let job_priority = if is_session_end { 10 } else { 0 };
    let mut first_source_id = None;
    let mut subbatches_ingested = 0_usize;
    for (sub_idx, (body, ref_time, sub_start_byte, sub_end_byte)) in
        subbatches.into_iter().enumerate()
    {
        let body_hash = ContentHash::digest(&body);
        let public_rec = session_subbatch_to_public_ingest_record(
            &session,
            actor,
            sub_start_byte,
            sub_end_byte,
            &body,
            &body_hash,
            ref_time,
        )?;
        let span_at = EpochMs(at.0.saturating_add(i64::try_from(sub_idx).unwrap_or(0)));
        match store.ingest_public_session_subbatch(&kp_store::PublicSessionSubbatchSpec {
            caller: actor,
            record: &public_rec,
            global_policy,
            session_id: &session,
            start_byte: sub_start_byte,
            end_byte: sub_end_byte,
            job_priority,
            self_echo_filtered: filtered,
            at: span_at,
        }) {
            Ok((stored, _)) => {
                if first_source_id.is_none() {
                    first_source_id = Some(stored.source_id);
                }
                subbatches_ingested += 1;
            }
            Err(kp_store::StoreError::Conflict) => {
                subbatches_ingested += 1;
            }
            Err(err) => return Err(err.into()),
        }
    }

    for mut episode in episodes {
        let mut premises = episode
            .before_trace
            .get("fact_id")
            .and_then(Value::as_str)
            .map(str::parse)
            .transpose()?
            .into_iter()
            .collect::<Vec<FactId>>();
        if let Some(ids) = episode
            .after_trace
            .pointer("/attribution/cited_premise_ids")
        {
            premises.extend(serde_json::from_value::<Vec<FactId>>(ids.clone())?);
        }
        premises.sort();
        premises.dedup();
        if let Some(ref source_id) = first_source_id {
            episode.environment["source_id"] = json!(source_id);
        }
        let _ = crate::signals::record(store, episode, actor, &premises, at)?;
    }

    Ok(SessionSliceIngestOutcome {
        committed_end_byte_offset: end,
        subbatches_ingested,
    })
}

/// Builds and validates a canonical [`PublicIngestRecord`] (`source.kind = "session"`, `op = Upsert`)
/// for a captured human session sub-batch so Claude Code and Codex hooks share the exact Public Ingest
/// Contract envelope and validation invariants as external callers (`kp ingest`, MCP, UDS).
pub fn session_subbatch_to_public_ingest_record(
    session_id: &str,
    actor: &ActorId,
    sub_start_byte: u64,
    sub_end_byte: u64,
    body: &str,
    body_hash: &ContentHash,
    occurred_at: EpochMs,
) -> Result<PublicIngestRecord> {
    let external_id = format!(
        "{}:{}:{}:{}:{}",
        actor,
        ContentHash::digest(session_id),
        sub_start_byte,
        sub_end_byte,
        body_hash
    );
    let record = PublicIngestRecord {
        source: PublicIngestSource {
            kind: "session".to_owned(),
            uri: format!("session:{session_id}"),
            external_id,
            container_id: None,
        },
        speaker: PublicSpeaker::new(actor.as_str(), None),
        audience: PublicAudience::Explicit(vec![actor.clone()]),
        occurred_at,
        content: body.to_owned(),
        op: PublicIngestOp::Upsert,
        ingested_by: Some(actor.clone()),
        media_type: Some("text/markdown".to_owned()),
        model_policy: None,
        thread_key: None,
        reply_to: None,
    };
    record.validate()?;
    Ok(record)
}
