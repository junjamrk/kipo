use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use kp_model::EpochMs;
use kp_store::Store;
use serde_json::{json, Value};

use crate::policy::Principal;
use crate::service::{dispatch, dispatch_readonly, Operation};
use crate::{NetError, Result};

pub(crate) fn is_readonly_mcp_body(body: &Value) -> bool {
    let check_single = |req: &Value| -> bool {
        if !valid_request(req) {
            return true;
        }
        match req.get("method").and_then(Value::as_str) {
            Some("initialize" | "notifications/initialized" | "tools/list") => true,
            Some("tools/call") => {
                let name = req
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                matches!(name, "kp_search" | "search" | "kp_why" | "why")
            }
            _ => true,
        }
    };
    match body {
        Value::Array(items) => items.iter().all(check_single),
        single => check_single(single),
    }
}

pub(crate) fn is_readonly_a2a_body(body: &Value) -> bool {
    !matches!(
        body.get("task").and_then(Value::as_str),
        Some("propose_fact")
    )
}

pub(crate) async fn mcp_readonly(
    store: &Store,
    principal: &Principal,
    body: Value,
    now: EpochMs,
) -> Result<Response> {
    let batch = body.is_array();
    let requests = match body {
        Value::Array(requests) => requests,
        request => vec![request],
    };
    if requests.is_empty() {
        return Ok(Json(invalid_request(Value::Null)).into_response());
    }
    let mut responses = Vec::new();
    for request in requests {
        if !valid_request(&request) {
            responses.push(invalid_request(Value::Null));
            continue;
        }
        let notification = request.get("id").is_none();
        let response = mcp_request_readonly(store, principal, request, now).await?;
        if !notification {
            responses.push(response);
        }
    }
    if responses.is_empty() {
        return Ok(StatusCode::ACCEPTED.into_response());
    }
    let response = if batch {
        Value::Array(responses)
    } else {
        responses.remove(0)
    };
    Ok(Json(response).into_response())
}

async fn mcp_request_readonly(
    store: &Store,
    principal: &Principal,
    body: Value,
    now: EpochMs,
) -> Result<Value> {
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    let result = match body.get("method").and_then(Value::as_str) {
        Some("initialize") => Ok(
            json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"kp","version":"0.1.0"}}),
        ),
        Some("notifications/initialized") => Ok(Value::Null),
        Some("tools/list") => Ok(mcp_tools_list()),
        Some("tools/call") => {
            let name = body
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let args = body
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or(json!({}));
            match tool_operation(name, args, now) {
                Ok(operation) => dispatch_readonly(store, principal, operation, now)
                    .await
                    .map(|value| json!({"content":[{"type":"text","text":value.to_string()}],"structuredContent":value,"isError":false})),
                Err(error) => Err(error),
            }
        }
        _ => Err(NetError::Invalid("unknown method")),
    };
    Ok(match result {
        Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
        Err(error) => {
            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":crate::server::public_error(&error)}})
        }
    })
}

fn mcp_tools_list() -> Value {
    json!({"tools":[
        {"name":"kp_search","description":"Search visible knowledge","inputSchema":{"type":"object","properties":{"q":{"type":"string"},"at":{"type":"integer"},"deep":{"type":"boolean"}},"required":["q"]}},
        {"name":"kp_publish","description":"Publish a structured claim","inputSchema":{"type":"object","properties":{"fact":{"type":"object"},"origin_reference":{"type":"object"},"idempotency_key":{"type":"string"}},"required":["fact","origin_reference","idempotency_key"]}},
        {"name":"kp_withdraw","description":"Withdraw your claim","inputSchema":{"type":"object","properties":{"fact_id":{"type":"string"}},"required":["fact_id"]}},
        {"name":"kp_why","description":"Explain a visible fact","inputSchema":{"type":"object","properties":{"fact_id":{"type":"string"},"at":{"type":"integer"}},"required":["fact_id"]}},
        {"name":"kp_synthesize","description":"Synthesize an audience intersection","inputSchema":{"type":"object","properties":{"query":{"type":"string"},"target_audience":{"type":"array","items":{"type":"string"}}},"required":["query","target_audience"]}}
    ]})
}

pub(crate) async fn mcp(
    store: &mut Store,
    principal: &Principal,
    body: Value,
    now: EpochMs,
) -> Result<Response> {
    let batch = body.is_array();
    let requests = match body {
        Value::Array(requests) => requests,
        request => vec![request],
    };
    if requests.is_empty() {
        return Ok(Json(invalid_request(Value::Null)).into_response());
    }
    let mut responses = Vec::new();
    for request in requests {
        if !valid_request(&request) {
            responses.push(invalid_request(Value::Null));
            continue;
        }
        let notification = request.get("id").is_none();
        let response = mcp_request(store, principal, request, now).await?;
        if !notification {
            responses.push(response);
        }
    }
    if responses.is_empty() {
        return Ok(StatusCode::ACCEPTED.into_response());
    }
    let response = if batch {
        Value::Array(responses)
    } else {
        responses.remove(0)
    };
    Ok(Json(response).into_response())
}

fn valid_request(request: &Value) -> bool {
    request.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
        && request.get("method").is_some_and(Value::is_string)
        && request
            .get("id")
            .is_none_or(|id| id.is_null() || id.is_string() || id.is_number())
        && request
            .get("params")
            .is_none_or(|params| params.is_object() || params.is_array())
}

fn invalid_request(id: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32600,"message":"Invalid Request"}})
}

async fn mcp_request(
    store: &mut Store,
    principal: &Principal,
    body: Value,
    now: EpochMs,
) -> Result<Value> {
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    let result = match body.get("method").and_then(Value::as_str) {
        Some("initialize") => Ok(
            json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"kp","version":"0.1.0"}}),
        ),
        Some("notifications/initialized") => Ok(Value::Null),
        Some("tools/list") => Ok(mcp_tools_list()),
        Some("tools/call") => {
            let name = body
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let args = body
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or(json!({}));
            match tool_operation(name, args, now) {
                Ok(operation) => dispatch(store, principal, operation, now).await.map(|value| json!({"content":[{"type":"text","text":value.to_string()}],"structuredContent":value,"isError":false})),
                Err(error) => Err(error),
            }
        }
        _ => Err(NetError::Invalid("unknown method")),
    };
    Ok(match result {
        Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
        Err(error) => {
            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":crate::server::public_error(&error)}})
        }
    })
}

fn tool_operation(name: &str, args: Value, now: EpochMs) -> Result<Operation> {
    match name {
        "kp_search" | "search" => Ok(Operation::Search(serde_json::from_value(args)?)),
        "kp_publish" | "propose_fact" => {
            Ok(Operation::Publish(Box::new(serde_json::from_value(args)?)))
        }
        "kp_withdraw" => Ok(Operation::Withdraw(serde_json::from_value(args)?)),
        "kp_synthesize" => Ok(Operation::Synthesize(serde_json::from_value(args)?)),
        "kp_why" | "why" => Ok(Operation::Why(
            args.get("fact_id")
                .and_then(Value::as_str)
                .ok_or(NetError::Invalid("fact_id required"))?
                .into(),
            args.get("at")
                .and_then(Value::as_i64)
                .map(EpochMs)
                .unwrap_or(now),
        )),
        _ => Err(NetError::Invalid("unknown task")),
    }
}

pub(crate) async fn a2a(
    store: &mut Store,
    principal: &Principal,
    body: Value,
    now: EpochMs,
) -> Result<Value> {
    let task = body
        .get("task")
        .and_then(Value::as_str)
        .ok_or(NetError::Invalid("task required"))?;
    if !matches!(task, "search" | "why" | "propose_fact") {
        return Err(NetError::Invalid("unsupported task"));
    }
    let operation = tool_operation(task, body.get("input").cloned().unwrap_or(json!({})), now)?;
    Ok(json!({"status":"completed","output":dispatch(store, principal, operation, now).await?}))
}

pub(crate) async fn a2a_readonly(
    store: &Store,
    principal: &Principal,
    body: Value,
    now: EpochMs,
) -> Result<Value> {
    let task = body
        .get("task")
        .and_then(Value::as_str)
        .ok_or(NetError::Invalid("task required"))?;
    if !matches!(task, "search" | "why" | "propose_fact") {
        return Err(NetError::Invalid("unsupported task"));
    }
    let operation = tool_operation(task, body.get("input").cloned().unwrap_or(json!({})), now)?;
    Ok(
        json!({"status":"completed","output":dispatch_readonly(store, principal, operation, now).await?}),
    )
}
