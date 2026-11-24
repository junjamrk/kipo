use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;

use kp_model::{
    derived_fact, justification, ActorId, ContentHash, DerivationMode, EpochMs, Fact, FactId,
    FactKind, KpError,
};
use kp_store::{Result, Store};
use serde::{Deserialize, Serialize};

/// Construction is confined to the intersection of every target actor's visible slice.
pub struct SynthesisInput {
    query: String,
    facts: Vec<Fact>,
}
impl SynthesisInput {
    pub fn query(&self) -> &str {
        &self.query
    }
    pub fn facts(&self) -> &[Fact] {
        &self.facts
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SynthesisClaim {
    pub text: String,
    pub unverified_gaps: Vec<String>,
}

pub trait ScopedSynthesizer: Send + Sync {
    fn synthesize<'a>(
        &'a self,
        input: &'a SynthesisInput,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<SynthesisClaim>>> + Send + 'a>>;
}

pub struct StructuredSynthesizer;
impl ScopedSynthesizer for StructuredSynthesizer {
    fn synthesize<'a>(
        &'a self,
        input: &'a SynthesisInput,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<SynthesisClaim>>> + Send + 'a>> {
        Box::pin(async move {
            if input.facts.is_empty() {
                return Ok(Vec::new());
            }
            let text = input
                .facts
                .iter()
                .map(|f| {
                    format!(
                        "{} {} {} [premise:{}]",
                        f.subject, f.predicate, f.object, f.id
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            Ok(vec![SynthesisClaim {
                text,
                unverified_gaps: vec![
                    "Structured summary; causal relationships have not been verified.".into(),
                ],
            }])
        })
    }
}

pub async fn synthesize_scoped(
    store: &mut Store,
    author: &ActorId,
    target_audience: &BTreeSet<ActorId>,
    query: &str,
    at: EpochMs,
    synthesizer: &dyn ScopedSynthesizer,
) -> Result<Vec<FactId>> {
    if !target_audience.contains(author) {
        return Err(KpError::invalid("author", "must belong to the target audience").into());
    }
    let first = target_audience
        .first()
        .ok_or_else(|| KpError::invalid("audience", "requires at least one actor"))?;
    let graph = store.derivation_view()?;
    let mut ids = graph.evaluate(first, at).facts;
    for actor in target_audience.iter().skip(1) {
        let visible = graph.evaluate(actor, at);
        ids.retain(|id| visible.facts.contains(id));
    }
    let input = SynthesisInput {
        query: query.into(),
        facts: graph
            .facts
            .values()
            .filter(|f| ids.contains(&f.id))
            .cloned()
            .collect(),
    };
    let claims = synthesizer.synthesize(&input).await?;
    let mut facts = Vec::new();
    let mut proofs = Vec::new();
    for claim in claims {
        let (text, mut citations) = audit(&claim.text, &ids)?;
        let mut gaps = Vec::new();
        for gap in claim.unverified_gaps {
            let (gap, refs) = audit(&gap, &ids)?;
            gaps.push(gap);
            citations.extend(refs);
        }
        if citations.is_empty() {
            continue;
        }
        // Recheck after an asynchronous model call; ACL or lifecycle edits may have happened meanwhile.
        let current = store.derivation_view()?;
        if target_audience
            .iter()
            .any(|actor| !ids.is_subset(&current.evaluate(actor, at).facts))
        {
            continue;
        }
        // Every input can influence model output, even if the model omits its citation.
        let premises: Vec<_> = ids.iter().filter_map(|id| current.facts.get(id)).collect();
        let mut fact = derived_fact(
            store.library_id(),
            author,
            FactKind::Synthesis,
            query,
            "kp.core.synthesis",
            serde_json::json!({"text":text,"unverified_gaps":gaps}),
            at,
        )?;
        let identity = ContentHash::digest(serde_json::to_vec(&(
            author,
            fact.strong_ref(),
            premises
                .iter()
                .map(|f| (&f.id, f.strong_ref()))
                .collect::<Vec<_>>(),
        ))?);
        fact.id = format!("synthesis-{identity}").parse()?;
        fact.logical_key = fact.id.as_str().parse()?;
        fact.unverified_gaps = gaps;
        proofs.push(justification(
            &fact,
            DerivationMode::ScopedSynthesis,
            "SCOPED_SYNTHESIS",
            &premises,
        )?);
        facts.push(fact);
    }
    store.save_derivations(&facts, &proofs)?;
    Ok(facts.into_iter().map(|f| f.id).collect())
}

fn audit(text: &str, allowed: &BTreeSet<FactId>) -> Result<(String, BTreeSet<FactId>)> {
    let mut clean = String::new();
    let mut rest = text;
    let mut citations = BTreeSet::new();
    while let Some(start) = rest.find("[premise:") {
        clean.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = rest.find(']') else {
            rest = "";
            break;
        };
        if let Ok(id) = rest[9..end].parse::<FactId>() {
            if allowed.contains(&id) {
                clean.push_str(&rest[..=end]);
                citations.insert(id);
            }
        }
        rest = &rest[end + 1..];
    }
    clean.push_str(rest);
    Ok((clean, citations))
}

#[cfg(feature = "http-synthesis")]
pub struct HttpSynthesizer {
    endpoint: reqwest::Url,
    client: reqwest::Client,
    redactor: kp_model::SecretRedactor,
}

#[cfg(feature = "http-synthesis")]
impl HttpSynthesizer {
    pub fn new(endpoint: &str, allow_cloud: bool) -> Result<Self> {
        let endpoint = reqwest::Url::parse(endpoint)
            .map_err(|_| KpError::invalid("synthesizer", "invalid endpoint"))?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(KpError::invalid("synthesizer", "invalid endpoint").into());
        }
        let host = endpoint
            .host_str()
            .ok_or_else(|| KpError::invalid("synthesizer", "missing host"))?;
        let loopback = host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        if !loopback && !allow_cloud {
            return Err(KpError::invalid("synthesizer", "remote use requires cloud opt-in").into());
        }
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(30));
        if host == "localhost" {
            builder = builder.resolve(
                "localhost",
                std::net::SocketAddr::new(
                    std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                    endpoint.port_or_known_default().unwrap_or(80),
                ),
            );
        }
        let client = builder
            .build()
            .map_err(|_| KpError::invalid("synthesizer", "client initialization failed"))?;
        Ok(Self {
            endpoint,
            client,
            redactor: kp_model::SecretRedactor::new()
                .map_err(|_| KpError::invalid("redactor", "invalid pattern"))?,
        })
    }
}

#[cfg(feature = "http-synthesis")]
impl ScopedSynthesizer for HttpSynthesizer {
    fn synthesize<'a>(
        &'a self,
        input: &'a SynthesisInput,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<SynthesisClaim>>> + Send + 'a>> {
        Box::pin(async move {
            let candidates: Vec<_> = input.facts.iter().map(|f| serde_json::json!({"id":f.id,"subject":f.subject,"predicate":f.predicate,"object":f.object})).collect();
            let payload = self.redactor.redact_json(&serde_json::json!({"query":input.query,"facts":candidates,"instruction":"Return JSON claims with text, [premise:<fact_id>] citations, and unverified_gaps."}));
            let mut response = self
                .client
                .post(self.endpoint.clone())
                .json(&payload)
                .send()
                .await
                .map_err(|_| KpError::invalid("synthesizer", "transport failed"))?;
            if !response.status().is_success() {
                return Err(KpError::invalid("synthesizer", "HTTP request failed").into());
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| KpError::invalid("synthesizer", "transport failed"))?
            {
                if bytes.len().saturating_add(chunk.len()) > 1_048_576 {
                    return Err(KpError::invalid("synthesizer", "response exceeds one MiB").into());
                }
                bytes.extend_from_slice(&chunk);
            }
            serde_json::from_slice(&bytes)
                .map_err(|_| KpError::invalid("synthesizer", "invalid claims response").into())
        })
    }
}
