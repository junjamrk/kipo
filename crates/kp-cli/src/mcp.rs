use std::collections::BTreeSet;
use std::error::Error;
use std::io::{self, BufRead, Read, Write};
use std::path::Path;

use kp_model::{
    format_opaque_id, verify_verbatim_quote, ActorId, AnchorSpec, AudienceSpec, BitemporalStamp,
    EpochMs, Fact, FactAccess, FactId, FactKind, FactRank, FactStatus, GovernedScope, LogicalKey,
    PublicAudience, PublicIngestOp, PublicIngestRecord, PublicIngestSource, PublicSpeaker,
    QuoteLocationHint, SecretRedactor, TrustTier, ValidityWindow,
};
use kp_store::{FactInput, Store};
use serde_json::{json, Value};
use uuid::Uuid;

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const MAX_MESSAGE_BYTES: u64 = 1024 * 1024;

pub async fn serve(_library_path: &Path, store: &mut Store, actor: &ActorId) -> Result<()> {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    loop {
        let mut line = Vec::new();
        if Read::by_ref(&mut input)
            .take(MAX_MESSAGE_BYTES + 1)
            .read_until(b'\n', &mut line)?
            == 0
        {
            break;
        }
        let response = if line.len() as u64 > MAX_MESSAGE_BYTES {
            if line.last() != Some(&b'\n') {
                input.skip_until(b'\n')?;
            }
            Some(error(Value::Null, -32600, "Request too large"))
        } else {
            match serde_json::from_slice::<Value>(&line) {
                Ok(request) => dispatch(store, actor, request).await,
                Err(_) => Some(error(Value::Null, -32700, "Parse error")),
            }
        };
        if let Some(response) = response {
            serde_json::to_writer(&mut output, &response)?;
            writeln!(output)?;
            output.flush()?;
        }
    }
    Ok(())
}

async fn dispatch(store: &mut Store, actor: &ActorId, request: Value) -> Option<Value> {
    if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || !request.get("method").is_some_and(Value::is_string)
        || request
            .get("id")
            .is_some_and(|id| !id.is_null() && !id.is_number() && !id.is_string())
        || request.get("params").is_some_and(|p| !p.is_object())
    {
        return Some(error(Value::Null, -32600, "Invalid Request"));
    }
    // Notifications do not execute mutating tools and never produce a response.
    let id = request.get("id")?.clone();
    let method = request["method"].as_str().unwrap_or("");
    let result = match method {
        "initialize" => {
            json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"kp","version":env!("CARGO_PKG_VERSION")}})
        }
        "ping" => json!({}),
        "tools/list" => catalog(),
        "tools/call" => {
            let Some(name) = request.pointer("/params/name").and_then(Value::as_str) else {
                return Some(error(id, -32602, "Tool name required"));
            };
            let args = request
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or(json!({}));
            if !args.is_object() {
                return Some(error(id, -32602, "Tool arguments must be an object"));
            }
            match call(store, actor, name, args).await {
                Ok(value) => {
                    json!({"content":[{"type":"text","text":value.to_string()}],"structuredContent":value,"isError":false})
                }
                Err(_) => {
                    json!({"content":[{"type":"text","text":"Operation unavailable or invalid arguments"}],"isError":true})
                }
            }
        }
        _ => return Some(error(id, -32601, "Method not found")),
    };
    Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

/// Trimmed Rev 4 3-tool MCP surface (`kp_search`, `kp_note`, `kp_why`); all admin/dream/skill actions stay CLI-only.
fn catalog() -> Value {
    let string = json!({"type":"string"});
    let tools = [
        (
            "kp_search",
            "Search visible memories",
            json!({
                "query": string,
                "limit": {"type": "integer"}
            }),
            vec!["query"],
        ),
        (
            "kp_note",
            "Save explicit user note",
            json!({
                "text": string,
                "jsonl": string,
                "verbatim_quote": string,
                "is_correction": {"type": "boolean"}
            }),
            Vec::new(),
        ),
        (
            "kp_why",
            "Explain fact provenance",
            json!({"fact_id": string}),
            vec!["fact_id"],
        ),
    ]
    .into_iter()
    .map(|(name, description, properties, required)| {
        json!({
            "name": name,
            "description": description,
            "inputSchema": {
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false
            }
        })
    })
    .collect::<Vec<_>>();
    json!({"tools": tools})
}

async fn call(store: &mut Store, actor: &ActorId, name: &str, args: Value) -> Result<Value> {
    if store.member_role(actor)?.is_none() {
        return Err("inactive actor".into());
    }
    crate::providers_cli::attach_ready_embedder(store);
    let at = crate::now()?;
    match name {
        "kp_search" => {
            let query = args
                .get("query")
                .or_else(|| args.get("q"))
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or("query required")?;
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(10)
                .clamp(1, 100) as usize;
            Ok(serde_json::to_value(kp_retrieve::search_shallow(
                store, actor, at, query, limit,
            )?)?)
        }
        "kp_note" => record_explicit_note(store, actor, at, &args),
        "kp_why" => {
            let id = resolve_visible_or_raw_fact_id(store, actor, at, required(&args, "fact_id")?)?;
            Ok(json!({"proof": kp_derive::why::why(store, &id, actor, at)?}))
        }
        _ => Err("unknown tool".into()),
    }
}

fn record_explicit_note(
    store: &mut Store,
    actor: &ActorId,
    at: EpochMs,
    args: &Value,
) -> Result<Value> {
    let raw_jsonl = args
        .get("jsonl")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let raw_text = args
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if raw_jsonl.is_none() && raw_text.is_none() {
        return Err("either text or jsonl is required".into());
    }

    let global_policy = crate::governance::default_auto_route_policy();
    let jsonl_report = if let Some(jsonl_str) = raw_jsonl {
        let report = store.apply_public_ingest_jsonl(jsonl_str, actor, &global_policy)?;
        if report.records_superseded > 0
            || report.records_withdrawn > 0
            || report.rosters_applied > 0
        {
            let _ = kp_derive::derive(store);
        }
        Some(report)
    } else {
        None
    };

    let Some(raw_text) = raw_text else {
        return Ok(serde_json::to_value(jsonl_report)?);
    };

    let raw_quote = args
        .get("verbatim_quote")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let is_correction = args
        .get("is_correction")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let redactor = SecretRedactor::new()?;
    let clean_text = redactor.redact(raw_text);
    if clean_text.trim().is_empty() {
        return Err("note text must be nonempty".into());
    }
    let clean_quote = raw_quote.map(|q| redactor.redact(q));
    let chunk_body = match &clean_quote {
        Some(vq) if !clean_text.contains(vq.as_str()) => format!("{clean_text}\n\n> {vq}\n"),
        _ => format!("{clean_text}\n"),
    };
    let anchor_quote = clean_quote.as_deref().unwrap_or(clean_text.as_str());
    let verified = verify_verbatim_quote(&chunk_body, anchor_quote, QuoteLocationHint::default())?;

    let note_token = Uuid::now_v7().simple().to_string();
    let fact_id_str = format_opaque_id("fact", &note_token)?;
    let fact_id = FactId::new(fact_id_str)?;
    let logical_key: LogicalKey = fact_id.as_str().parse()?;

    let record = PublicIngestRecord {
        source: PublicIngestSource {
            kind: "mcp_note".into(),
            uri: format!("note:{fact_id}"),
            external_id: format!("{actor}:{note_token}"),
            container_id: None,
        },
        speaker: PublicSpeaker::new(actor.as_str(), None),
        audience: PublicAudience::Explicit(vec![actor.clone()]),
        occurred_at: at,
        content: chunk_body,
        op: PublicIngestOp::Upsert,
        ingested_by: Some(actor.clone()),
        media_type: Some("text/markdown".into()),
        model_policy: None,
    };
    record.validate()?;
    let outcome = store.apply_public_ingest_record(&record, actor, &global_policy)?;
    let source_id = outcome
        .source_id
        .ok_or("missing source_id from mcp_note upsert")?;
    let queued = outcome.enqueued_job_id.is_some();

    let trust_tier = if actor == store.owner_actor() {
        TrustTier::OwnerExplicit
    } else {
        TrustTier::HumanVerbatim
    };
    let predicate = if is_correction {
        "kp.core.correction"
    } else {
        "kp.core.note"
    };
    let subject = derive_note_subject(&clean_text);
    let chunk_id = format!("{source_id}-0");
    let fact = Fact {
        id: fact_id.clone(),
        origin_library: store.library_id().clone(),
        logical_key,
        kind: FactKind::Observation,
        subject,
        predicate: predicate.into(),
        object: json!({
            "text": clean_text,
            "verbatim_quote": verified.exact_quote,
            "is_correction": is_correction,
        }),
        tags: BTreeSet::new(),
        author: actor.clone(),
        ingested_by: None,
        trust_tier,
        access: FactAccess::Base {
            audience: AudienceSpec::Explicit(BTreeSet::from([actor.clone()])),
            stamp: BitemporalStamp {
                valid: ValidityWindow {
                    valid_from: at,
                    valid_until: EpochMs::OPEN_END,
                },
                recorded_at: at,
                expired_at: None,
            },
        },
        source_scope_id: None,
        governed_scope: Some(GovernedScope::SelfOnly {
            actor: actor.clone(),
        }),
        broader_scope: None,
        status: FactStatus::Active,
        superseded_by: None,
        rank: FactRank::Normal,
        source_anchor: Some(AnchorSpec {
            source_id: Some(source_id.clone()),
            chunk_id: Some(chunk_id),
            verbatim_quote: Some(verified.exact_quote),
            char_start: Some(verified.span.char_start),
            char_end: Some(verified.span.char_end),
            start_byte: Some(verified.byte_start),
            end_byte: Some(verified.byte_end),
            ..AnchorSpec::default()
        }),
        unverified_gaps: Vec::new(),
        miss_count: 0,
    };
    store.remember(&[FactInput {
        fact,
        entities: BTreeSet::new(),
    }])?;

    let mut out = json!({
        "fact_id": fact_id,
        "source_id": source_id,
        "is_correction": is_correction,
        "queued_for_extraction": queued,
    });
    if let (Some(rep), Some(obj)) = (jsonl_report, out.as_object_mut()) {
        obj.insert("jsonl_report".into(), serde_json::to_value(rep)?);
    }
    Ok(out)
}

fn derive_note_subject(text: &str) -> String {
    let first_line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(text)
        .trim();
    let clause = first_line
        .split(['。', '！', '？', '.', '!', '?', '；', ';', '\n'])
        .next()
        .unwrap_or(first_line)
        .trim();
    let chars: Vec<char> = clause.chars().collect();
    if chars.is_empty() {
        "note".to_owned()
    } else if chars.len() <= 48 {
        clause.to_owned()
    } else {
        chars[..48].iter().collect()
    }
}

fn required<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "required argument missing".into())
}

pub(crate) fn resolve_visible_or_raw_fact_id(
    store: &Store,
    actor: &ActorId,
    at: EpochMs,
    value: &str,
) -> Result<FactId> {
    Ok(store.resolve_visible_or_raw_fact_id(actor, at, value)?)
}
