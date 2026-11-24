use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use kp_model::{DreamDecision, DreamDistillEnvelope, SecretRedactor, EVIDENCE_BOUNDARY};
use serde_json::{json, Value};

use super::{DreamAttributor, DreamInput};
use crate::{IngestError, Result};

// Adapted from harness2's _INV_TEMPLATE, _DOM_TEMPLATE and _KEEP_BASE (Apache-2.0).
const ATTRIBUTION_PROMPT: &str = "Attribute the earliest observed divergence at step_ref across coding sessions, chat dialogues, documents, web links, or audio/video transcripts. INV: how the agent or CLI workflow operates; distinguish bad input, timeout, missing capability, and permission; specify a bounded repair and a checkable stop condition. DOM: domain invariant, required artifact contract, or decision rule and its supporting evidence. Never invent authority, hallucinate CLI flags absent from the trace, or generalize a task-local path/UUID. KEEP_BASE is a valid answer when no actionable, verified contrast exists. Return only JSON: {\"decision\":\"KEEP_BASE\"} or {\"decision\":\"UPDATE\",\"attribution\":{\"skill_slug\":\"safe-slug\",\"trigger\":\"narrow situation\",\"wrong_move\":\"failed action\",\"verified_fix\":\"checkable reusable rule\",\"dimension\":\"INV or DOM\",\"cited_premise_ids\":[]},\"extra_rules\":[],\"memory_facts\":[]}. Cite only supplied premise IDs. Never copy private transcripts. A promoted skill requires the literal line: When <trigger>, load the `<skill_slug>` skill.";

pub struct HttpDreamAttributor {
    endpoint: reqwest::Url,
    model: String,
    api_key: Option<String>,
    client: reqwest::Client,
    redactor: SecretRedactor,
}

impl HttpDreamAttributor {
    pub fn new(endpoint: &str, model: &str, allow_cloud: bool) -> Result<Self> {
        Self::with_api_key(endpoint, model, allow_cloud, None)
    }

    pub fn with_api_key(
        endpoint: &str,
        model: &str,
        allow_cloud: bool,
        api_key: Option<String>,
    ) -> Result<Self> {
        let endpoint = reqwest::Url::parse(endpoint)
            .map_err(|_| IngestError::Invalid("invalid dream endpoint"))?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || model.trim().is_empty()
        {
            return Err(IngestError::Invalid("invalid dream endpoint or model"));
        }
        let host = endpoint
            .host_str()
            .ok_or(IngestError::Invalid("missing dream host"))?;
        let loopback = host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        if !loopback && !allow_cloud {
            return Err(IngestError::Invalid(
                "non-loopback dream endpoint requires --allow-cloud-llm",
            ));
        }
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30));
        if host == "localhost" {
            builder = builder.resolve(
                "localhost",
                std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    endpoint.port_or_known_default().unwrap_or(80),
                )),
            );
        }
        Ok(Self {
            endpoint,
            model: model.into(),
            api_key: api_key
                .map(|k| k.trim().to_owned())
                .filter(|k| !k.is_empty()),
            client: builder
                .build()
                .map_err(|_| IngestError::Invalid("dream client initialization failed"))?,
            redactor: SecretRedactor::new()?,
        })
    }
}

impl DreamAttributor for HttpDreamAttributor {
    fn uses_external_model(&self) -> bool {
        true
    }

    fn attribute<'a>(
        &'a self,
        input: &'a DreamInput,
    ) -> Pin<Box<dyn Future<Output = Result<DreamDecision>> + Send + 'a>> {
        Box::pin(async move { Ok(self.distill(input).await?.decision) })
    }

    fn distill<'a>(
        &'a self,
        input: &'a DreamInput,
    ) -> Pin<Box<dyn Future<Output = Result<DreamDistillEnvelope>> + Send + 'a>> {
        Box::pin(async move {
            let mut stripped_val = serde_json::to_value(input)?;
            if let Some(env_obj) = stripped_val
                .pointer_mut("/episode/environment")
                .and_then(Value::as_object_mut)
            {
                env_obj.remove("fallback_attribution");
                env_obj.remove("mined_attribution");
            }
            let evidence = self.redactor.redact_json(&stripped_val);
            let is_anthropic = self.endpoint.path().ends_with("/v1/messages");
            let payload = if is_anthropic {
                self.redactor.redact_json(&json!({
                    "model": self.model,
                    "max_tokens": 1024,
                    "system": format!("{ATTRIBUTION_PROMPT}\n{EVIDENCE_BOUNDARY}"),
                    "messages": [{"role": "user", "content": serde_json::to_string(&evidence)?}]
                }))
            } else {
                self.redactor.redact_json(&json!({
                    "model": self.model,
                    "messages": [
                        {"role": "system", "content": format!("{ATTRIBUTION_PROMPT}\n{EVIDENCE_BOUNDARY}")},
                        {"role": "user", "content": serde_json::to_string(&evidence)?}
                    ],
                    "response_format": {"type": "json_object"}
                }))
            };
            let mut req = self.client.post(self.endpoint.clone()).json(&payload);
            if let Some(ref key) = self.api_key {
                if is_anthropic {
                    req = req
                        .header("x-api-key", key)
                        .header("anthropic-version", "2023-06-01");
                } else {
                    req = req.bearer_auth(key);
                }
            }
            let mut response = req
                .send()
                .await
                .map_err(|_| IngestError::Invalid("dream transport failed"))?;
            if !response.status().is_success() {
                return Err(IngestError::Invalid("dream HTTP request failed"));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| IngestError::Invalid("dream transport failed"))?
            {
                if bytes.len().saturating_add(chunk.len()) > 1_048_576 {
                    return Err(IngestError::Limit(1_048_576));
                }
                bytes.extend_from_slice(&chunk);
            }
            if bytes.trim_ascii() == b"KEEP_BASE" {
                return Ok(DreamDistillEnvelope::default());
            }
            let value: Value = serde_json::from_slice(&bytes)?;
            let inner = if let Some(text) = value
                .pointer("/choices/0/message/content")
                .or_else(|| value.pointer("/content/0/text"))
                .and_then(Value::as_str)
            {
                if text.trim() == "KEEP_BASE" {
                    return Ok(DreamDistillEnvelope::default());
                }
                serde_json::from_str::<Value>(text.trim())?
            } else if value == "KEEP_BASE" {
                return Ok(DreamDistillEnvelope::default());
            } else {
                value
            };
            let redacted = self.redactor.redact_json(&inner);
            super::parse_distill_envelope(redacted)
        })
    }
}
