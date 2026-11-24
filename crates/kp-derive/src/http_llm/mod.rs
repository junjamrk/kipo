//! Direct HTTP generative model runner (`OpenAI`-compatible `/chat/completions` & `Anthropic`
//! `/v1/messages`) plus zero-configuration global single-model auto-detection (`docs/ARCHITECTURE.md`).

mod detect;
mod probes;

pub use detect::{
    build_named_endpoint_llm_client, detect_global_generative_model, load_secure_kp_config,
    resolve_named_endpoint_config, DetectedGenerativeModel, DetectedHostCli, KpGlobalModelConfig,
    ModelDetectEnv, NamedEndpointConfig,
};
pub use probes::{probe_claude_logged_in, probe_codex_logged_in};

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use kp_model::{
    BoxFuture, EntailmentQuery, EntailmentVerdict, ExtractionBatchOutcome, ExtractionBatchRequest,
    KpError, LlmClient, ModelRoutePolicy, Result, SecretRedactor,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::llm_runner::strip_markdown_fence;
use crate::prompts::{
    build_entailment_prompt, build_extraction_prompt, entailment_verdict_json_schema,
    extraction_batch_json_schema, is_chunk_trivial_or_ack_only, parse_entailment_verdict,
    parse_extraction_envelope, ModelExtractionEnvelope, SKIP_SUMMARY_SENTINEL,
};

const MAX_HTTP_LLM_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const ANTHROPIC_API_VERSION: &str = "2023-06-01";

/// Wire protocol family spoken by [`HttpApiLlmClient`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpApiWireKind {
    OpenAiChatCompletions,
    AnthropicMessages,
}

/// Supported cloud and enterprise-gateway generative API providers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerativeApiProvider {
    OpenAi,
    Anthropic,
    DashScope,
    Volcengine,
    Zhipu,
    OpenRouter,
}

impl GenerativeApiProvider {
    pub fn id(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::DashScope => "dashscope",
            Self::Volcengine => "volcengine",
            Self::Zhipu => "zhipu",
            Self::OpenRouter => "openrouter",
        }
    }

    pub fn from_id(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "openai" => Some(Self::OpenAi),
            "anthropic" | "claude" => Some(Self::Anthropic),
            "dashscope" | "qwen" => Some(Self::DashScope),
            "volcengine" | "ark" | "doubao" => Some(Self::Volcengine),
            "zhipu" | "zhipuai" | "glm" => Some(Self::Zhipu),
            "openrouter" => Some(Self::OpenRouter),
            _ => None,
        }
    }

    pub fn wire_kind(self) -> HttpApiWireKind {
        match self {
            Self::Anthropic => HttpApiWireKind::AnthropicMessages,
            Self::OpenAi | Self::DashScope | Self::Volcengine | Self::Zhipu | Self::OpenRouter => {
                HttpApiWireKind::OpenAiChatCompletions
            }
        }
    }

    pub fn auth_env(self) -> &'static str {
        match self {
            Self::OpenAi => "OPENAI_API_KEY",
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::DashScope => "DASHSCOPE_API_KEY",
            Self::Volcengine => "ARK_API_KEY",
            Self::Zhipu => "ZHIPUAI_API_KEY",
            Self::OpenRouter => "OPENROUTER_API_KEY",
        }
    }

    pub fn base_url_env(self) -> &'static str {
        match self {
            Self::OpenAi => "OPENAI_BASE_URL",
            Self::Anthropic => "ANTHROPIC_BASE_URL",
            Self::DashScope => "DASHSCOPE_BASE_URL",
            Self::Volcengine => "ARK_BASE_URL",
            Self::Zhipu => "ZHIPUAI_BASE_URL",
            Self::OpenRouter => "OPENROUTER_BASE_URL",
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::OpenAi => "https://api.openai.com/v1",
            Self::Anthropic => "https://api.anthropic.com",
            Self::DashScope => "https://dashscope.aliyuncs.com/compatible-mode/v1",
            Self::Volcengine => "https://ark.cn-beijing.volces.com/api/v3",
            Self::Zhipu => "https://open.bigmodel.cn/api/paas/v4",
            Self::OpenRouter => "https://openrouter.ai/api/v1",
        }
    }

    pub fn default_model(self) -> &'static str {
        match self {
            Self::OpenAi => "gpt-4o-mini",
            Self::Anthropic => "claude-sonnet-4-6",
            Self::DashScope => "qwen-plus",
            Self::Volcengine => "doubao-1-5-pro-32k-250115",
            Self::Zhipu => "glm-4-flash",
            Self::OpenRouter => "openai/gpt-4o-mini",
        }
    }

    fn prefers_strict_json_schema(self) -> bool {
        matches!(self, Self::OpenAi | Self::OpenRouter)
    }
}

/// Configuration for [`HttpApiLlmClient`]. Custom `Debug` redacts `api_key`.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpApiConfig {
    pub provider: GenerativeApiProvider,
    pub wire_kind: HttpApiWireKind,
    pub base_url: String,
    pub is_custom_gateway: bool,
    pub api_key: String,
    pub model: String,
    pub route_policy: ModelRoutePolicy,
    pub timeout: Duration,
    pub remaining_quota_pct: Option<u8>,
    pub(crate) invalid_state: Option<(&'static str, &'static str, String)>,
}

impl fmt::Debug for HttpApiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpApiConfig")
            .field("provider", &self.provider)
            .field("wire_kind", &self.wire_kind)
            .field("base_url", &self.base_url)
            .field("is_custom_gateway", &self.is_custom_gateway)
            .field("api_key", &"[REDACTED]")
            .field("model", &self.model)
            .field("route_policy", &self.route_policy)
            .field("timeout", &self.timeout)
            .field("remaining_quota_pct", &self.remaining_quota_pct)
            .field("invalid_state", &self.invalid_state)
            .finish()
    }
}

impl HttpApiConfig {
    pub fn for_provider(provider: GenerativeApiProvider, api_key: impl Into<String>) -> Self {
        Self {
            provider,
            wire_kind: provider.wire_kind(),
            base_url: provider.default_base_url().to_owned(),
            is_custom_gateway: false,
            api_key: api_key.into(),
            model: provider.default_model().to_owned(),
            route_policy: ModelRoutePolicy::HostCli,
            timeout: Duration::from_secs(120),
            remaining_quota_pct: None,
            invalid_state: None,
        }
    }

    pub fn invalid_gateway_reason(&self) -> Option<&str> {
        self.invalid_state
            .as_ref()
            .filter(|(field, _, _)| *field == "base_url")
            .map(|(_, _, detail)| detail.as_str())
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        let raw = base_url.into();
        let trimmed = raw.trim().trim_end_matches('/').to_owned();
        self.is_custom_gateway = trimmed != self.provider.default_base_url().trim_end_matches('/');
        self.base_url = trimmed;
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        let trimmed = model.into();
        if !trimmed.trim().is_empty() {
            self.model = trimmed.trim().to_owned();
        }
        self
    }

    pub fn resolved_endpoint_url(&self) -> Result<String> {
        let validated = validate_gateway_base_url(&self.base_url)?;
        let base = validated.as_str().trim_end_matches('/');
        let full = match self.wire_kind {
            HttpApiWireKind::OpenAiChatCompletions => {
                if base.ends_with("/chat/completions") {
                    base.to_owned()
                } else {
                    format!("{base}/chat/completions")
                }
            }
            HttpApiWireKind::AnthropicMessages => {
                if base.ends_with("/v1/messages") {
                    base.to_owned()
                } else if base.ends_with("/v1") {
                    format!("{base}/messages")
                } else {
                    format!("{base}/v1/messages")
                }
            }
        };
        Ok(full)
    }
}

/// Validates a generative HTTP base URL (`http`/`https`, no userinfo/query/fragment).
pub fn validate_gateway_base_url(raw: &str) -> Result<reqwest::Url> {
    let trimmed = raw.trim();
    let url = reqwest::Url::parse(trimmed)
        .map_err(|_| KpError::invalid("base_url", "invalid HTTP gateway URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host_str().is_none()
    {
        return Err(KpError::invalid(
            "base_url",
            "gateway URL must be http(s) without credentials, query, or fragment",
        ));
    }
    Ok(url)
}

#[derive(Clone, Debug)]
pub struct HttpJsonRequest {
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: Value,
    pub timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct HttpJsonResponse {
    pub status: u16,
    pub body: String,
}

/// Pluggable JSON POST transport enabling both real `reqwest` execution and hermetic offline tests.
pub trait HttpJsonTransport: Send + Sync {
    fn post_json<'a>(&'a self, req: &'a HttpJsonRequest)
        -> BoxFuture<'a, Result<HttpJsonResponse>>;
}

/// Production `reqwest::Client` transport enforcing redirect blocking and response body bounds.
pub struct ReqwestHttpTransport {
    client: reqwest::Client,
}

impl ReqwestHttpTransport {
    pub fn new(base_url: &str, timeout: Duration) -> Result<Self> {
        let parsed = validate_gateway_base_url(base_url)?;
        let host = parsed.host_str().unwrap_or("");
        let is_loopback = host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());

        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout);
        if is_loopback {
            builder = builder.no_proxy();
        }
        if host == "localhost" {
            let port = parsed.port_or_known_default().unwrap_or(80);
            builder = builder.resolve(
                "localhost",
                std::net::SocketAddr::new(
                    std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                    port,
                ),
            );
        }
        let client = builder
            .build()
            .map_err(|_| KpError::invalid("http_llm", "failed building HTTP client"))?;
        Ok(Self { client })
    }
}

impl HttpJsonTransport for ReqwestHttpTransport {
    fn post_json<'a>(
        &'a self,
        req: &'a HttpJsonRequest,
    ) -> BoxFuture<'a, Result<HttpJsonResponse>> {
        Box::pin(async move {
            let mut rb = self.client.post(&req.url).timeout(req.timeout);
            for (k, v) in &req.headers {
                rb = rb.header(k.as_str(), v.as_str());
            }
            let mut resp = rb
                .json(&req.body)
                .send()
                .await
                .map_err(|_| KpError::invalid("http_llm", "HTTP transport request failed"))?;
            let status = resp.status().as_u16();
            let mut bytes = Vec::new();
            while let Some(chunk) = resp
                .chunk()
                .await
                .map_err(|_| KpError::invalid("http_llm", "failed reading HTTP response stream"))?
            {
                if bytes.len().saturating_add(chunk.len()) > MAX_HTTP_LLM_RESPONSE_BYTES {
                    return Err(KpError::invalid(
                        "http_llm",
                        "HTTP LLM response exceeded 4 MiB limit",
                    ));
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(HttpJsonResponse {
                status,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            })
        })
    }
}

/// Direct HTTP generative LLM client supporting OpenAI-compatible `/chat/completions` and
/// Anthropic `/v1/messages` with `SecretRedactor` scrubbing and strict narrow-boundary parsing.
pub struct HttpApiLlmClient<T: HttpJsonTransport = ReqwestHttpTransport> {
    config: HttpApiConfig,
    endpoint_url: String,
    transport: Arc<T>,
    redactor: Arc<SecretRedactor>,
}

impl HttpApiLlmClient<ReqwestHttpTransport> {
    pub fn new(config: HttpApiConfig) -> Result<Self> {
        if let Some((field, static_reason, _)) = &config.invalid_state {
            return Err(KpError::invalid(field, static_reason));
        }
        let transport = ReqwestHttpTransport::new(&config.base_url, config.timeout)?;
        Self::with_transport(config, transport)
    }
}

impl<T: HttpJsonTransport + 'static> HttpApiLlmClient<T> {
    pub fn with_transport(config: HttpApiConfig, transport: T) -> Result<Self> {
        if let Some((field, static_reason, _)) = &config.invalid_state {
            return Err(KpError::invalid(field, static_reason));
        }
        let endpoint_url = if config.route_policy == ModelRoutePolicy::RawOnly {
            String::new()
        } else {
            if config.api_key.trim().is_empty() {
                return Err(KpError::invalid(
                    "api_key",
                    "HTTP generative client requires a nonempty API key",
                ));
            }
            config.resolved_endpoint_url()?
        };
        let redactor =
            SecretRedactor::new().map_err(|_| KpError::invalid("redactor", "invalid regex"))?;
        Ok(Self {
            config,
            endpoint_url,
            transport: Arc::new(transport),
            redactor: Arc::new(redactor),
        })
    }

    pub fn config(&self) -> &HttpApiConfig {
        &self.config
    }

    fn build_headers(&self) -> BTreeMap<String, String> {
        let mut headers =
            BTreeMap::from([("content-type".to_owned(), "application/json".to_owned())]);
        match self.config.wire_kind {
            HttpApiWireKind::OpenAiChatCompletions => {
                headers.insert(
                    "authorization".to_owned(),
                    format!("Bearer {}", self.config.api_key),
                );
            }
            HttpApiWireKind::AnthropicMessages => {
                headers.insert("x-api-key".to_owned(), self.config.api_key.clone());
                headers.insert(
                    "anthropic-version".to_owned(),
                    ANTHROPIC_API_VERSION.to_owned(),
                );
            }
        }
        headers
    }

    fn build_request_body(
        &self,
        schema_name: &str,
        schema: &Value,
        prompt: &str,
        use_strict_schema: bool,
    ) -> Value {
        match self.config.wire_kind {
            HttpApiWireKind::OpenAiChatCompletions => {
                let response_format = if use_strict_schema {
                    json!({
                        "type": "json_schema",
                        "json_schema": {
                            "name": schema_name,
                            "strict": true,
                            "schema": schema,
                        }
                    })
                } else {
                    json!({ "type": "json_object" })
                };
                json!({
                    "model": self.config.model,
                    "temperature": 0,
                    "response_format": response_format,
                    "messages": [
                        {
                            "role": "system",
                            "content": "Return ONLY valid JSON matching the requested schema."
                        },
                        {
                            "role": "user",
                            "content": prompt
                        }
                    ]
                })
            }
            HttpApiWireKind::AnthropicMessages => json!({
                "model": self.config.model,
                "max_tokens": 4096,
                "temperature": 0,
                "messages": [
                    {
                        "role": "user",
                        "content": prompt
                    }
                ],
                "tools": [
                    {
                        "name": schema_name,
                        "description": "Emit structured JSON matching the required schema.",
                        "input_schema": schema,
                    }
                ],
                "tool_choice": {
                    "type": "tool",
                    "name": schema_name,
                }
            }),
        }
    }

    async fn execute_structured_call(
        &self,
        schema_name: &str,
        schema: &Value,
        prompt: &str,
    ) -> Result<HttpCallOutcome> {
        let headers = self.build_headers();
        let prefer_strict = self.config.provider.prefers_strict_json_schema();
        let first_body = self.build_request_body(schema_name, schema, prompt, prefer_strict);
        let mut resp = self
            .transport
            .post_json(&HttpJsonRequest {
                url: self.endpoint_url.clone(),
                headers: headers.clone(),
                body: first_body,
                timeout: self.config.timeout,
            })
            .await?;

        // Enterprise OpenAI-compatible gateways (vLLM / LiteLLM / older proxies) sometimes reject
        // `json_schema` with HTTP 400 while accepting `json_object`: retry once transparently.
        if prefer_strict
            && self.config.is_custom_gateway
            && resp.status == 400
            && (resp.body.contains("response_format") || resp.body.contains("json_schema"))
        {
            let fallback_body = self.build_request_body(schema_name, schema, prompt, false);
            resp = self
                .transport
                .post_json(&HttpJsonRequest {
                    url: self.endpoint_url.clone(),
                    headers,
                    body: fallback_body,
                    timeout: self.config.timeout,
                })
                .await?;
        }

        if let Some(reason) = detect_http_low_quota(resp.status, &resp.body) {
            return Ok(HttpCallOutcome::LowQuota(reason));
        }
        if !(200..300).contains(&resp.status) {
            return Err(KpError::invalid(
                "http_llm_status",
                "generative HTTP endpoint returned non-2xx status",
            ));
        }

        extract_structured_content(self.config.wire_kind, &resp.body)
    }
}

enum HttpCallOutcome {
    SkipSentinel,
    LowQuota(String),
    Json(Value),
}

fn detect_http_low_quota(status: u16, body: &str) -> Option<String> {
    if status == 429 {
        return Some("HTTP 429 rate limit".to_owned());
    }
    if status >= 400 {
        let lower = body.to_ascii_lowercase();
        for marker in [
            "insufficient_quota",
            "quota exceeded",
            "rate_limit_exceeded",
            "credit balance is too low",
        ] {
            if lower.contains(marker) {
                return Some(format!("HTTP {status} quota exhausted: {marker}"));
            }
        }
    }
    None
}

fn extract_structured_content(
    wire_kind: HttpApiWireKind,
    raw_body: &str,
) -> Result<HttpCallOutcome> {
    let envelope: Value = serde_json::from_str(raw_body.trim())
        .map_err(|_| KpError::invalid("http_llm_json", "response body is not valid JSON"))?;

    match wire_kind {
        HttpApiWireKind::OpenAiChatCompletions => {
            let Some(message) = envelope
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|arr| arr.first())
                .and_then(|c| c.get("message"))
            else {
                return Err(KpError::invalid(
                    "http_llm_json",
                    "missing choices[0].message in OpenAI response",
                ));
            };
            if let Some(parsed) = message.get("parsed").filter(|v| v.is_object()) {
                return Ok(HttpCallOutcome::Json(parsed.clone()));
            }
            let content_text = match message.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(parts)) => parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(""),
                _ => String::new(),
            };
            decode_text_or_sentinel(&content_text)
        }
        HttpApiWireKind::AnthropicMessages => {
            let Some(blocks) = envelope.get("content").and_then(Value::as_array) else {
                return Err(KpError::invalid(
                    "http_llm_json",
                    "missing content array in Anthropic response",
                ));
            };
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    if let Some(input) = block.get("input").filter(|v| v.is_object()) {
                        return Ok(HttpCallOutcome::Json(input.clone()));
                    }
                }
            }
            let combined_text: String = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("");
            decode_text_or_sentinel(&combined_text)
        }
    }
}

fn decode_text_or_sentinel(text: &str) -> Result<HttpCallOutcome> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.contains(SKIP_SUMMARY_SENTINEL) {
        return Ok(HttpCallOutcome::SkipSentinel);
    }
    let clean = strip_markdown_fence(trimmed);
    let val: Value = serde_json::from_str(clean).map_err(|_| {
        KpError::invalid("http_llm_json", "model message content was not valid JSON")
    })?;
    Ok(HttpCallOutcome::Json(val))
}

impl<T: HttpJsonTransport + 'static> LlmClient for HttpApiLlmClient<T> {
    fn is_raw_only(&self) -> bool {
        self.config.route_policy == ModelRoutePolicy::RawOnly
    }

    fn route_policy(&self) -> ModelRoutePolicy {
        self.config.route_policy.clone()
    }

    fn extract_batch<'a>(
        &'a self,
        request: &'a ExtractionBatchRequest,
    ) -> BoxFuture<'a, Result<ExtractionBatchOutcome>> {
        Box::pin(async move {
            request.validate_request()?;
            if self.is_raw_only() {
                return Ok(ExtractionBatchOutcome::SkippedEmpty);
            }
            if let Some(remaining) = self.config.remaining_quota_pct {
                if remaining < request.min_rate_limit_remaining_pct {
                    return Ok(ExtractionBatchOutcome::SkippedLowQuota {
                        remaining_pct: Some(remaining),
                        reason: format!(
                            "remaining quota {remaining}% below watermark {}%",
                            request.min_rate_limit_remaining_pct
                        ),
                    });
                }
            }
            if request.chunks.is_empty()
                || request
                    .chunks
                    .iter()
                    .all(|chunk| is_chunk_trivial_or_ack_only(&chunk.body))
            {
                return Ok(ExtractionBatchOutcome::SkippedEmpty);
            }

            let prompt = build_extraction_prompt(request, &self.redactor);
            let schema = extraction_batch_json_schema();
            match self
                .execute_structured_call("kp_extraction_batch", &schema, &prompt)
                .await?
            {
                HttpCallOutcome::SkipSentinel => Ok(ExtractionBatchOutcome::SkippedEmpty),
                HttpCallOutcome::LowQuota(reason) => Ok(ExtractionBatchOutcome::SkippedLowQuota {
                    remaining_pct: None,
                    reason,
                }),
                HttpCallOutcome::Json(json_val) => {
                    let envelope: ModelExtractionEnvelope = parse_extraction_envelope(json_val)?;
                    if envelope.skip_summary || envelope.atoms.is_empty() {
                        Ok(ExtractionBatchOutcome::SkippedEmpty)
                    } else {
                        Ok(ExtractionBatchOutcome::Completed {
                            atoms: envelope.atoms,
                        })
                    }
                }
            }
        })
    }

    fn judge_entailment<'a>(
        &'a self,
        query: &'a EntailmentQuery,
    ) -> BoxFuture<'a, Result<EntailmentVerdict>> {
        Box::pin(async move {
            if self.is_raw_only() {
                return Ok(EntailmentVerdict::Unrelated);
            }
            let prompt = build_entailment_prompt(query, &self.redactor);
            let schema = entailment_verdict_json_schema();
            match self
                .execute_structured_call("kp_entailment_verdict", &schema, &prompt)
                .await?
            {
                HttpCallOutcome::SkipSentinel => Ok(EntailmentVerdict::Unrelated),
                HttpCallOutcome::LowQuota(_) => Err(KpError::invalid(
                    "http_llm_quota",
                    "HTTP rate limit or quota exhausted during entailment",
                )),
                HttpCallOutcome::Json(json_val) => parse_entailment_verdict(json_val),
            }
        })
    }

    fn complete_structured_json<'a>(
        &'a self,
        schema_name: &'a str,
        json_schema: &'a Value,
        prompt: &'a str,
    ) -> BoxFuture<'a, Result<Option<Value>>> {
        Box::pin(async move {
            if self.is_raw_only() {
                return Ok(None);
            }
            let safe_prompt = self.redactor.redact(prompt);
            match self
                .execute_structured_call(schema_name, json_schema, &safe_prompt)
                .await?
            {
                HttpCallOutcome::SkipSentinel => Ok(None),
                HttpCallOutcome::LowQuota(_) => Err(KpError::invalid(
                    "http_llm_quota",
                    "HTTP rate limit or quota exhausted during structured completion",
                )),
                HttpCallOutcome::Json(json_val) => Ok(Some(self.redactor.redact_json(&json_val))),
            }
        })
    }
}

/// Zero-network, zero-subprocess `LlmClient` returned when global model policy resolves to `RawOnly`.
#[derive(Clone, Copy, Debug, Default)]
pub struct RawOnlyLlmClient;

impl LlmClient for RawOnlyLlmClient {
    fn is_raw_only(&self) -> bool {
        true
    }

    fn route_policy(&self) -> ModelRoutePolicy {
        ModelRoutePolicy::RawOnly
    }

    fn extract_batch<'a>(
        &'a self,
        request: &'a ExtractionBatchRequest,
    ) -> BoxFuture<'a, Result<ExtractionBatchOutcome>> {
        Box::pin(async move {
            request.validate_request()?;
            Ok(ExtractionBatchOutcome::SkippedEmpty)
        })
    }

    fn judge_entailment<'a>(
        &'a self,
        _query: &'a EntailmentQuery,
    ) -> BoxFuture<'a, Result<EntailmentVerdict>> {
        Box::pin(async { Ok(EntailmentVerdict::Unrelated) })
    }
}

/// Returns `true` when `err` represents an HTTP `429` rate-limit or quota exhaustion condition.
///
/// Quota errors MUST propagate upward (`paused_quota`) and MUST NEVER trigger failover to a
/// paid secondary backup API key.
pub fn is_quota_exhausted_error(err: &KpError) -> bool {
    if let KpError::InvalidInput { field, reason } = err {
        if field.contains("quota") {
            return true;
        }
        let lower = reason.to_ascii_lowercase();
        if lower.contains("quota")
            || lower.contains("rate limit")
            || lower.contains("rate_limit")
            || lower.contains("429")
            || lower.contains("too many requests")
            || lower.contains("credit balance is too low")
        {
            return true;
        }
    }
    let lower = err.to_string().to_ascii_lowercase();
    lower.contains("quota")
        || lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("429")
        || lower.contains("too many requests")
        || lower.contains("credit balance is too low")
}

/// Returns `true` when `err` indicates that a primary [`HostCliRunner`] failed due to missing/expired
/// login credentials or an unspawnable CLI binary (excluding rate-limit/quota errors).
pub fn is_cli_auth_or_spawn_error(err: &KpError) -> bool {
    if is_quota_exhausted_error(err) {
        return false;
    }
    match err {
        KpError::InvalidInput { field, reason } => {
            if *field == "host_cli_auth" || *field == "host_cli_spawn" {
                return true;
            }
            if !field.starts_with("host_cli") {
                return false;
            }
            let lower = reason.to_ascii_lowercase();
            lower.contains("failed to spawn")
                || lower.contains("authentication")
                || lower.contains("unauthorized")
                || lower.contains("unauthenticated")
                || lower.contains("not logged in")
                || lower.contains("login")
                || lower.contains("session expired")
                || lower.contains("token expired")
                || lower.contains("api key")
                || lower.contains("no such file")
        }
        _ => false,
    }
}

/// Primary-to-secondary generative LLM client wrapper: executes `primary` (typically a logged-in
/// [`HostCliRunner`]) and seamlessly fails over to `secondary` (a backup [`HttpApiLlmClient`])
/// when `primary` fails due to authentication expiry, missing login, or an unspawnable CLI binary.
///
/// Rate-limit and quota-exhaustion outcomes (`ExtractionBatchOutcome::SkippedLowQuota` or HTTP
/// `429` / quota errors) are strictly preserved and NEVER fail over to burn backup paid API keys.
pub struct FallbackLlmClient {
    primary: Arc<dyn LlmClient>,
    secondary: Arc<dyn LlmClient>,
}

impl FallbackLlmClient {
    pub fn new(primary: Arc<dyn LlmClient>, secondary: Arc<dyn LlmClient>) -> Self {
        Self { primary, secondary }
    }
}

impl LlmClient for FallbackLlmClient {
    fn is_raw_only(&self) -> bool {
        self.primary.is_raw_only() && self.secondary.is_raw_only()
    }

    fn route_policy(&self) -> ModelRoutePolicy {
        if self.is_raw_only() {
            ModelRoutePolicy::RawOnly
        } else {
            self.primary.route_policy()
        }
    }

    fn extract_batch<'a>(
        &'a self,
        request: &'a ExtractionBatchRequest,
    ) -> BoxFuture<'a, Result<ExtractionBatchOutcome>> {
        Box::pin(async move {
            match self.primary.extract_batch(request).await {
                Ok(outcome @ ExtractionBatchOutcome::SkippedLowQuota { .. }) => Ok(outcome),
                Ok(outcome) => Ok(outcome),
                Err(err) if is_quota_exhausted_error(&err) => Err(err),
                Err(err) if is_cli_auth_or_spawn_error(&err) => {
                    self.secondary.extract_batch(request).await
                }
                Err(err) => Err(err),
            }
        })
    }

    fn judge_entailment<'a>(
        &'a self,
        query: &'a EntailmentQuery,
    ) -> BoxFuture<'a, Result<EntailmentVerdict>> {
        Box::pin(async move {
            match self.primary.judge_entailment(query).await {
                Ok(verdict) => Ok(verdict),
                Err(err) if is_quota_exhausted_error(&err) => Err(err),
                Err(err) if is_cli_auth_or_spawn_error(&err) => {
                    self.secondary.judge_entailment(query).await
                }
                Err(err) => Err(err),
            }
        })
    }

    fn complete_structured_json<'a>(
        &'a self,
        schema_name: &'a str,
        json_schema: &'a Value,
        prompt: &'a str,
    ) -> BoxFuture<'a, Result<Option<Value>>> {
        Box::pin(async move {
            match self
                .primary
                .complete_structured_json(schema_name, json_schema, prompt)
                .await
            {
                Ok(val) => Ok(val),
                Err(err) if is_quota_exhausted_error(&err) => Err(err),
                Err(err) if is_cli_auth_or_spawn_error(&err) => {
                    self.secondary
                        .complete_structured_json(schema_name, json_schema, prompt)
                        .await
                }
                Err(err) => Err(err),
            }
        })
    }
}

#[cfg(test)]
#[path = "../http_llm_tests.rs"]
mod tests;
