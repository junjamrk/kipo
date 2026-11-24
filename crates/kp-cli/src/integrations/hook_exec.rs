use std::error::Error;
use std::io::{self, Write};
use std::path::Path;

#[cfg(unix)]
use kp_ingest::hooks::{MAX_PROMPT_ITEMS, MAX_PROMPT_TOKENS};
use kp_model::{ActorId, EpochMs};
#[cfg(unix)]
use kp_net::{
    CursorNotification, DaemonRequest, DaemonResponse, DaemonSyncClient, PreToolCheckRequest,
    QueryContextRequest,
};
use kp_store::Store;
use serde_json::{json, Value};

#[cfg(unix)]
use super::daemon_handler::maybe_autospawn_daemon_from_hook;
use super::{Client, Event};
use crate::{now, Cli};

/// Executes a hook command and guarantees exit code 0 on all paths, including missing library or bad stdin.
pub async fn run_hook_infallible(cli: &Cli, event: Event, client: Option<Client>) {
    let result = execute_hook(cli, event, client)
        .await
        .unwrap_or_else(|_| json!({}));
    let mut stdout = io::stdout().lock();
    if serde_json::to_writer(&mut stdout, &result).is_ok() {
        let _ = writeln!(stdout);
    } else {
        let _ = writeln!(stdout, "{{}}");
    }
}

async fn execute_hook(
    cli: &Cli,
    event: Event,
    client: Option<Client>,
) -> Result<Value, Box<dyn Error>> {
    if kp_ingest::hooks::is_internal_extraction() {
        return Ok(json!({}));
    }
    let input = kp_ingest::read_bounded(io::stdin().lock(), kp_ingest::MAX_INPUT_BYTES)?;
    execute_hook_with_input(cli, event, client, &input, now()?).await
}

pub(super) async fn execute_hook_with_input(
    cli: &Cli,
    event: Event,
    client: Option<Client>,
    input: &[u8],
    at: EpochMs,
) -> Result<Value, Box<dyn Error>> {
    if kp_ingest::hooks::is_internal_extraction() {
        return Ok(json!({}));
    }
    let mut payload: Value = if input.iter().all(u8::is_ascii_whitespace) {
        json!({})
    } else {
        serde_json::from_slice(input)?
    };
    if let (Some(c), Some(obj)) = (client, payload.as_object_mut()) {
        obj.insert(
            "_kp_client".to_owned(),
            Value::String(c.as_str().to_owned()),
        );
    }

    let Some(actor) = resolve_active_hook_actor(&cli.path, cli.actor.as_ref()) else {
        return Ok(json!({}));
    };

    #[cfg(unix)]
    {
        if matches!(event, Event::SessionStart) {
            let lib_for_spawn = cli.path.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if DaemonSyncClient::for_library(&lib_for_spawn)
                    .and_then(|c| c.request(&DaemonRequest::Ping))
                    .is_err()
                {
                    maybe_autospawn_daemon_from_hook(&lib_for_spawn);
                }
            })
            .await;
            return Ok(json!({}));
        }

        let lib_for_uds = cli.path.clone();
        let payload_for_uds = payload.clone();
        let actor_for_uds = actor.clone();
        if let Ok(Some(uds_out)) = tokio::task::spawn_blocking(move || {
            let res = try_execute_hook_via_daemon(
                &lib_for_uds,
                event,
                &payload_for_uds,
                &actor_for_uds,
                at,
            );
            if res.is_none() {
                maybe_autospawn_daemon_from_hook(&lib_for_uds);
            }
            res
        })
        .await
        {
            if matches!(event, Event::SessionEnd) {
                crate::rsi::maybe_autospawn_dream_in_background(&cli.path, &actor);
            }
            return Ok(uds_out);
        }
    }
    #[cfg(not(unix))]
    if matches!(event, Event::SessionStart) {
        return Ok(json!({}));
    }

    if kp_ingest::hooks::is_readonly_hook_invocation(event.as_str(), &payload) {
        let store = Store::open_readonly(&cli.path)?;
        return Ok(kp_ingest::hooks::run_readonly(
            &store,
            event.as_str(),
            &payload,
            &actor,
            at,
        )?);
    }

    let mut store = Store::open(&cli.path)?;
    crate::providers_cli::attach_non_local_embedder_for_hook(&mut store);
    let global_policy = crate::governance::default_auto_route_policy();
    let out = kp_ingest::hooks::run_with_path_and_policy(
        Some(&cli.path),
        &mut store,
        event.as_str(),
        &payload,
        &actor,
        at,
        &global_policy,
    )
    .await?;
    drop(store);
    if matches!(event, Event::SessionEnd) {
        crate::rsi::maybe_autospawn_dream_in_background(&cli.path, &actor);
    }
    Ok(out)
}

fn resolve_active_hook_actor(library_path: &Path, requested: Option<&ActorId>) -> Option<ActorId> {
    let store = Store::open_readonly(library_path).ok()?;
    let actor = match requested {
        Some(a) => a.clone(),
        None => store.owner_actor().clone(),
    };
    store.member_role(&actor).ok().flatten().map(|_| actor)
}

#[cfg(unix)]
fn try_execute_hook_via_daemon(
    library_path: &Path,
    event: Event,
    payload: &Value,
    actor: &ActorId,
    at: EpochMs,
) -> Option<Value> {
    let client = DaemonSyncClient::for_library(library_path).ok()?;
    match event {
        Event::SessionStart => Some(json!({})),
        Event::UserPromptSubmit => {
            let query = payload
                .get("prompt")
                .or_else(|| payload.get("user_prompt"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if query.is_empty() || kp_ingest::hooks::is_low_signal_ack(query) {
                return Some(json!({}));
            }
            let wrapper_overhead =
                kp_ingest::hooks::estimate_tokens("<kp-context>\n\n</kp-context>")
                    + 2 * MAX_PROMPT_ITEMS;
            let resp = client
                .request(&DaemonRequest::QueryContext(QueryContextRequest {
                    prompt: query.to_owned(),
                    viewer: actor.clone(),
                    at,
                    max_tokens: MAX_PROMPT_TOKENS.saturating_sub(wrapper_overhead),
                    max_items: MAX_PROMPT_ITEMS,
                    time_zone: None,
                }))
                .ok()?;
            match resp {
                DaemonResponse::Context(res) => Some(kp_ingest::hooks::wrap_prompt_context_output(
                    "UserPromptSubmit",
                    &res.additional_context,
                )),
                _ => None,
            }
        }
        Event::PreToolUse => {
            let Some(tool_name) = payload.get("tool_name").and_then(Value::as_str) else {
                return Some(json!({}));
            };
            let is_codex = kp_ingest::hooks::is_codex_pre_tool_payload(payload, tool_name);
            let tool_input = payload
                .get("tool_input")
                .filter(|v| !v.is_null())
                .cloned()
                .unwrap_or_else(|| payload.clone());
            let resp = client
                .request(&DaemonRequest::PreToolCheck(PreToolCheckRequest {
                    tool_name: tool_name.to_owned(),
                    tool_input,
                    cwd: payload
                        .get("cwd")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    viewer: actor.clone(),
                    at,
                    client_kind: Some(if is_codex {
                        "codex".into()
                    } else {
                        "claude-code".into()
                    }),
                    time_zone: None,
                }))
                .ok()?;
            let DaemonResponse::PreTool(res) = resp else {
                return None;
            };
            Some(res.to_hook_json())
        }
        Event::Stop | Event::SessionEnd => {
            if payload.get("messages").is_some() || payload.get("transcript").is_some() {
                return None;
            }
            let session_id = payload
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())?;
            let tp = payload
                .get("transcript_path")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())?;
            let tp_path = Path::new(tp);
            if !tp_path.is_file() {
                return None;
            }
            let (byte_offset, line_count, prefix_sha256) =
                kp_ingest::hooks::inspect_transcript_cursor_file(tp_path).ok()?;
            let is_session_end = matches!(event, Event::SessionEnd);
            let (queued_extraction, last_extracted_offset) = client
                .notify_cursor(CursorNotification {
                    session_id: session_id.to_owned(),
                    client_kind: kp_ingest::hooks::infer_client_kind(payload),
                    transcript_path: Some(tp.to_owned()),
                    byte_offset,
                    line_count,
                    prefix_sha256,
                    actor: actor.clone(),
                    at,
                    session_ended: is_session_end,
                })
                .ok()?;
            Some(json!({
                "captured": queued_extraction,
                "queued_daemon": true,
                "cursor_byte_offset": byte_offset,
                "last_extracted_offset": last_extracted_offset,
            }))
        }
        Event::PostToolUse => None,
    }
}
