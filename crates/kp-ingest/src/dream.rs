use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};

use kp_model::{
    verify_verbatim_quote, ActorId, Attribution, AudienceSpec, BitemporalStamp, ContentHash,
    ContrastKind, DerivationView, DistilledFactSpec, DreamDecision, DreamDistillEnvelope, EpochMs,
    Fact, FactAccess, FactId, FactKind, FactRank, FactStatus, LlmClient, ModelRoutePolicy,
    QuoteLocationHint, RejectionEvidenceKey, RsiDimension, SecretRedactor, SourceId,
    SourceModality, TrustTier, ValidityWindow, EVIDENCE_BOUNDARY,
};
use kp_store::{Episode, FactInput, FtsOnlyEmbedder, Store};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{IngestError, Result};

#[path = "dream_http.rs"]
mod http;
pub use http::HttpDreamAttributor;

static UUID_RE: LazyLock<std::result::Result<Regex, regex::Error>> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
});
static BACKTICK_SPAN_RE: LazyLock<std::result::Result<Regex, regex::Error>> =
    LazyLock::new(|| Regex::new(r"`([^`]+)`"));
static LONG_FLAG_RE: LazyLock<std::result::Result<Regex, regex::Error>> =
    LazyLock::new(|| Regex::new(r"--[a-zA-Z0-9][a-zA-Z0-9_-]{2,}"));

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExistingSkillBrief {
    pub skill_slug: String,
    pub dimension: RsiDimension,
    pub trigger: String,
}

/// Constructed only from this actor's redacted episode, visible premise slice, and existing skill briefs.
#[derive(Serialize)]
pub struct DreamInput {
    episode: Episode,
    premises: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    existing_skills: Vec<ExistingSkillBrief>,
}

impl DreamInput {
    pub fn episode(&self) -> &Episode {
        &self.episode
    }
    pub fn premises(&self) -> &[Value] {
        &self.premises
    }
    pub fn existing_skills(&self) -> &[ExistingSkillBrief] {
        &self.existing_skills
    }
}

pub trait DreamAttributor: Send + Sync {
    fn uses_external_model(&self) -> bool {
        false
    }

    fn attribute<'a>(
        &'a self,
        input: &'a DreamInput,
    ) -> Pin<Box<dyn Future<Output = Result<DreamDecision>> + Send + 'a>>;

    fn distill<'a>(
        &'a self,
        input: &'a DreamInput,
    ) -> Pin<Box<dyn Future<Output = Result<DreamDistillEnvelope>> + Send + 'a>> {
        Box::pin(async move {
            Ok(DreamDistillEnvelope {
                decision: self.attribute(input).await?,
                ..DreamDistillEnvelope::default()
            })
        })
    }
}

pub struct StructuredDreamAttributor;

impl DreamAttributor for StructuredDreamAttributor {
    fn attribute<'a>(
        &'a self,
        input: &'a DreamInput,
    ) -> Pin<Box<dyn Future<Output = Result<DreamDecision>> + Send + 'a>> {
        Box::pin(async move {
            let episode = input.episode();
            if !episode.has_contrast() || episode.step_ref.trim().is_empty() {
                return Ok(DreamDecision::KeepBase);
            }
            if let Some(value) = episode
                .after_trace
                .get("attribution")
                .filter(|v| !v.is_null())
            {
                if episode.signal_kind != ContrastKind::HumanCorrection && !verified(episode) {
                    return Ok(DreamDecision::KeepBase);
                }
                return match serde_json::from_value::<Attribution>(value.clone()) {
                    Ok(attribution) => Ok(DreamDecision::Update(attribution.sanitize()?)),
                    Err(_) => Ok(DreamDecision::KeepBase),
                };
            }
            for env_key in ["fallback_attribution", "mined_attribution"] {
                if let Some(value) = episode.environment.get(env_key).filter(|v| !v.is_null()) {
                    if episode.signal_kind != ContrastKind::HumanCorrection && !verified(episode) {
                        return Ok(DreamDecision::KeepBase);
                    }
                    return match serde_json::from_value::<Attribution>(value.clone()) {
                        Ok(attribution) => Ok(DreamDecision::Update(attribution.sanitize()?)),
                        Err(_) => Ok(DreamDecision::KeepBase),
                    };
                }
            }
            let (slug, trigger, wrong_move, fix) = match episode.signal_kind {
                ContrastKind::HumanCorrection
                    if crate::signals::is_correction(&trace_text(&episode.after_trace)) =>
                {
                    (
                        "apply-explicit-correction",
                        "a user corrects the preceding action",
                        "Continue using the assumption that the user corrected",
                        "Replace the corrected assumption before the next action; check the next result against the user's correction.",
                    )
                }
                ContrastKind::FailureToRecovery if verified(episode) => (
                    "verify-command-recovery",
                    "recovering a failed tool or build on the same target",
                    "Treat an attempted repair as verified without rerunning the failed target",
                    "Read the failure diagnostic, make a targeted change, rerun the same target, and require a successful result before continuing.",
                ),
                ContrastKind::HumanCorrection
                | ContrastKind::FailureToRecovery
                | ContrastKind::CrossAvatarContrast => return Ok(DreamDecision::KeepBase),
            };
            Ok(DreamDecision::Update(Attribution {
                skill_slug: slug.parse()?,
                trigger: trigger.into(),
                wrong_move: wrong_move.into(),
                verified_fix: fix.into(),
                dimension: RsiDimension::Inv,
                cited_premise_ids: Vec::new(),
            }))
        })
    }
}

/// Multi-source generative `DreamAttributor` backed by any `kp_model::LlmClient` (`HostCliRunner`,
/// `HttpApiLlmClient`, `FallbackLlmClient`, `PolicyRoutedLlmClient`, or `DeterministicMockLlm`).
pub struct LlmDreamAttributor {
    client: Arc<dyn LlmClient>,
    redactor: SecretRedactor,
}

impl LlmDreamAttributor {
    pub fn new(client: Arc<dyn LlmClient>) -> Result<Self> {
        Ok(Self {
            client,
            redactor: SecretRedactor::new()?,
        })
    }
}

const LLM_DISTILL_INSTRUCTIONS: &str = "\
You are Kipo's sleep-time knowledge consolidator and procedural skill compiler.
Input may come from coding sessions, team chat logs, technical documents, web links, or timed audio/video transcripts.

Follow these strict rules:
1. Return `decision = \"KEEP_BASE\"` (with `rejection_class`) when the episode is a one-off task redirect, unverified intermediate guess, timeout/interrupt, or baseline language trivia.
2. Separate declarative facts from procedural skills:
   - Single-point project constraints, banned commands, architectural decisions, or verified pitfalls belong in `memory_facts` (`kind`: `constraint`, `decision`, `closed_negative`, or `fact_slot`), each citing an exact `verbatim_quote` from `premises`.
   - Reusable multi-step troubleshooting or operational workflows verified by tool exit 0 or explicit confirmation belong in `decision = \"UPDATE\"` (`attribution` + optional `extra_rules`).
3. If `existing_skills` already lists a matching `(skill_slug, dimension)` for the same root pattern, reuse that exact `skill_slug` so Kipo clusters cross-source corroboration instead of fragmenting duplicates.
4. Never include personal home paths, UUIDs, secrets, or CLI flags absent from the observed trace/premises. Never copy >= 48 contiguous characters from transcripts.";

pub fn dream_distill_json_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["decision"],
        "properties": {
            "decision": { "type": "string", "enum": ["KEEP_BASE", "UPDATE"] },
            "rejection_class": {
                "type": ["string", "null"],
                "enum": [
                    "one_off_task_redirect",
                    "unverified_or_interrupted",
                    "baseline_model_knowledge",
                    "confounded_multi_change",
                    "scanner_literal_leak",
                    "applicability_ungrounded",
                    null
                ]
            },
            "attribution": {
                "type": ["object", "null"],
                "additionalProperties": false,
                "required": ["skill_slug", "trigger", "wrong_move", "verified_fix", "dimension"],
                "properties": {
                    "skill_slug": { "type": "string" },
                    "trigger": { "type": "string" },
                    "wrong_move": { "type": "string" },
                    "verified_fix": { "type": "string" },
                    "dimension": { "type": "string", "enum": ["INV", "DOM"] },
                    "cited_premise_ids": { "type": "array", "items": { "type": "string" } }
                }
            },
            "extra_rules": { "type": "array", "items": { "type": "string" } },
            "memory_facts": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["kind", "subject", "predicate", "summary", "verbatim_quote"],
                    "properties": {
                        "kind": { "type": "string", "enum": ["constraint", "decision", "closed_negative", "fact_slot", "observation"] },
                        "subject": { "type": "string" },
                        "predicate": { "type": "string" },
                        "summary": { "type": "string" },
                        "verbatim_quote": { "type": "string" },
                        "hard_guard": { "type": "boolean" },
                        "forbidden_commands": { "type": "array", "items": { "type": "string" } },
                        "allowed_commands": { "type": "array", "items": { "type": "string" } },
                        "required_flags": { "type": "array", "items": { "type": "string" } }
                    }
                }
            }
        }
    })
}

pub(crate) fn parse_distill_envelope(raw: Value) -> Result<DreamDistillEnvelope> {
    let Some(obj) = raw.as_object() else {
        return Err(IngestError::Invalid("dream response must be a JSON object"));
    };
    let Some(dec_str) = obj.get("decision").and_then(Value::as_str) else {
        return Err(IngestError::Invalid(
            "dream response missing string decision",
        ));
    };
    let decision = match dec_str {
        "KEEP_BASE" => DreamDecision::KeepBase,
        "UPDATE" => {
            let attr_val = obj
                .get("attribution")
                .filter(|v| !v.is_null())
                .cloned()
                .ok_or(IngestError::Invalid("UPDATE decision requires attribution"))?;
            let attr: Attribution = serde_json::from_value(attr_val)?;
            DreamDecision::Update(attr)
        }
        _ => return Err(IngestError::Invalid("unknown dream decision variant")),
    };
    let rejection_class = obj
        .get("rejection_class")
        .filter(|v| !v.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()?;
    let memory_facts: Vec<DistilledFactSpec> = obj
        .get("memory_facts")
        .filter(|v| !v.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    let extra_rules: Vec<String> = obj
        .get("extra_rules")
        .filter(|v| !v.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    Ok(DreamDistillEnvelope {
        decision,
        rejection_class,
        memory_facts,
        extra_rules,
    })
}

impl DreamAttributor for LlmDreamAttributor {
    fn uses_external_model(&self) -> bool {
        !self.client.is_raw_only()
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
            if self.client.is_raw_only() {
                return StructuredDreamAttributor.distill(input).await;
            }
            let mut payload_val = serde_json::to_value(input)?;
            if let Some(env_obj) = payload_val
                .pointer_mut("/episode/environment")
                .and_then(Value::as_object_mut)
            {
                env_obj.remove("fallback_attribution");
                env_obj.remove("mined_attribution");
            }
            let redacted_input = self.redactor.redact_json(&payload_val);
            let prompt = format!(
                "{LLM_DISTILL_INSTRUCTIONS}\n\n{EVIDENCE_BOUNDARY}\n\nINPUT_JSON:\n{}",
                serde_json::to_string(&redacted_input)?
            );
            let schema = dream_distill_json_schema();
            let Some(resp_val) = self
                .client
                .complete_structured_json("kp_dream_distill", &schema, &prompt)
                .await?
            else {
                return Ok(DreamDistillEnvelope::default());
            };
            parse_distill_envelope(self.redactor.redact_json(&resp_val))
        })
    }
}

fn verified(episode: &Episode) -> bool {
    episode.after_trace.get("success").and_then(Value::as_bool) == Some(true)
        || episode.environment.get("verified").and_then(Value::as_bool) == Some(true)
}

fn trace_text(trace: &Value) -> String {
    trace
        .as_str()
        .or_else(|| trace.get("content").and_then(Value::as_str))
        .unwrap_or("")
        .to_owned()
}

#[derive(Default, Debug, Serialize)]
pub struct DreamReport {
    pub processed: usize,
    pub kept_base: usize,
    pub bullets_created: usize,
    pub promoted: Vec<FactId>,
    #[serde(default)]
    pub sessions_mined: usize,
    #[serde(default)]
    pub sources_mined: usize,
    #[serde(default)]
    pub episodes_mined: usize,
    #[serde(default)]
    pub facts_distilled: usize,
    #[serde(default)]
    pub scanner_rejected: usize,
}

fn episode_is_raw_only(
    store: &Store,
    graph: &DerivationView,
    episode: &Episode,
    premise_ids: &[FactId],
) -> Result<bool> {
    if episode.environment.get("raw_only").and_then(Value::as_bool) == Some(true)
        || episode
            .environment
            .get("model_policy")
            .and_then(Value::as_str)
            .is_some_and(|p| p.eq_ignore_ascii_case("raw_only"))
    {
        return Ok(true);
    }
    if let Some(sid_str) = episode.environment.get("source_id").and_then(Value::as_str) {
        if let Ok(sid) = sid_str.parse::<SourceId>() {
            if store.source_effective_route_policy(&sid)? == ModelRoutePolicy::RawOnly {
                return Ok(true);
            }
        }
    }
    for pid in premise_ids {
        if let Some(fact) = graph.facts.get(pid) {
            if store.fact_effective_route_policy(fact)? == ModelRoutePolicy::RawOnly {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn zep_scan_passes(
    attribution: &Attribution,
    extra_rules: &[String],
    episode: &Episode,
    premises: &[Value],
) -> bool {
    let all_texts = [
        attribution.trigger.as_str(),
        attribution.wrong_move.as_str(),
        attribution.verified_fix.as_str(),
    ];
    for text in all_texts
        .iter()
        .copied()
        .chain(extra_rules.iter().map(String::as_str))
    {
        let lower = text.to_ascii_lowercase();
        if lower.contains("/users/")
            || lower.contains("/home/")
            || lower.contains("c:\\users\\")
            || lower.contains("d:\\users\\")
        {
            return false;
        }
        if let Ok(re) = UUID_RE.as_ref() {
            if re.is_match(text) {
                return false;
            }
        }
    }

    let observed_cmds = episode
        .environment
        .get("observed_commands")
        .and_then(Value::as_array)
        .filter(|arr| !arr.is_empty());
    if let Some(cmds) = observed_cmds {
        let mut grounded_corpus = format!(
            "{}\n{}\n{}",
            episode.before_trace,
            episode.after_trace,
            serde_json::to_string(premises).unwrap_or_default()
        )
        .to_ascii_lowercase();
        for c in cmds.iter().filter_map(Value::as_str) {
            grounded_corpus.push('\n');
            grounded_corpus.push_str(&c.to_ascii_lowercase());
        }
        if let (Ok(bt_re), Ok(flag_re)) = (BACKTICK_SPAN_RE.as_ref(), LONG_FLAG_RE.as_ref()) {
            for rule in std::iter::once(attribution.verified_fix.as_str())
                .chain(extra_rules.iter().map(String::as_str))
            {
                for cap in bt_re.captures_iter(rule) {
                    let Some(span) = cap.get(1) else {
                        continue;
                    };
                    for fm in flag_re.find_iter(span.as_str()) {
                        let flag = fm.as_str().to_ascii_lowercase();
                        if !grounded_corpus.contains(&flag) {
                            return false;
                        }
                    }
                }
            }
        }
    }
    true
}

fn collect_existing_skill_briefs(
    graph: &DerivationView,
    visible_facts: &BTreeSet<FactId>,
    actor: &ActorId,
    library_id: &kp_model::LibraryId,
) -> Vec<ExistingSkillBrief> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for fact in graph.facts.values() {
        if fact.kind != FactKind::PlaybookBullet
            || fact.origin_library != *library_id
            || fact.author != *actor
            || !visible_facts.contains(&fact.id)
        {
            continue;
        }
        if let Ok(attr) = serde_json::from_value::<Attribution>(fact.object["attribution"].clone())
        {
            if seen.insert((attr.skill_slug.to_string(), attr.dimension)) {
                out.push(ExistingSkillBrief {
                    skill_slug: attr.skill_slug.to_string(),
                    dimension: attr.dimension,
                    trigger: attr.trigger,
                });
                if out.len() >= 24 {
                    break;
                }
            }
        }
    }
    out
}

fn persist_distilled_memory_facts(
    store: &mut Store,
    actor: &ActorId,
    episode: &Episode,
    specs: &[DistilledFactSpec],
    is_raw_only: bool,
    at: EpochMs,
) -> Result<usize> {
    if specs.is_empty() {
        return Ok(0);
    }
    let Some(sid_str) = episode.environment.get("source_id").and_then(Value::as_str) else {
        return Ok(0);
    };
    let Ok(source_id) = sid_str.parse::<SourceId>() else {
        return Ok(0);
    };
    let chunks = store.source_anchored_chunks(&source_id)?;
    if chunks.is_empty() {
        return Ok(0);
    }
    let ctx_opt = store.extraction_source_context(&source_id, &[])?;
    let audience = ctx_opt
        .as_ref()
        .map(|c| c.readable_by.clone())
        .unwrap_or_else(|| AudienceSpec::Explicit(BTreeSet::from([actor.clone()])));
    let source_scope_id = ctx_opt.as_ref().and_then(|c| c.source_scope_id.clone());
    let uri = ctx_opt.map(|c| c.uri).unwrap_or_default();
    let modality = SourceModality::infer(&uri, "");
    let trust_tier = if modality == SourceModality::WebLink {
        TrustTier::ExternalUntrusted
    } else {
        TrustTier::ToolOrModelVerified
    };
    let rejections = store.list_rejections(None)?;
    let mut created = 0usize;

    for (idx, raw_spec) in specs.iter().enumerate() {
        let Ok(spec) = raw_spec.clone().sanitize() else {
            continue;
        };
        let mut matched = None;
        for ch in &chunks {
            let hint = QuoteLocationHint {
                turn_ordinal: None,
                approx_char_offset: ch
                    .body
                    .find(&spec.verbatim_quote)
                    .and_then(|b| u32::try_from(ch.body[..b].chars().count()).ok()),
            };
            if let Ok(ver) = verify_verbatim_quote(&ch.body, &spec.verbatim_quote, hint) {
                matched = Some((ch, ver));
                break;
            }
        }
        let Some((chunk, verified)) = matched else {
            continue;
        };
        let ev_key = RejectionEvidenceKey {
            speaker: actor.clone(),
            chunk_sha256: ContentHash::digest(&chunk.body),
            quote_span: verified.span,
        };
        if rejections
            .iter()
            .any(|r| r.matches_candidate(Some(&ev_key), None))
        {
            continue;
        }
        let digest = ContentHash::digest(
            serde_json::to_vec(&(
                store.library_id(),
                actor,
                &source_id,
                &chunk.chunk_id,
                spec.kind,
                &spec.subject,
                &spec.predicate,
                &verified.exact_quote,
            ))?
            .as_slice(),
        );
        let logical_key: kp_model::LogicalKey = format!("dream-mem:{digest}").parse()?;
        if store
            .visible_facts(actor, at)?
            .iter()
            .any(|f| f.origin_library == *store.library_id() && f.logical_key == logical_key)
        {
            continue;
        }

        let mut merged_anchor = chunk.anchor.clone();
        merged_anchor.source_id = Some(source_id.clone());
        merged_anchor.chunk_id = Some(chunk.chunk_id.clone());
        merged_anchor.verbatim_quote = Some(verified.exact_quote);
        merged_anchor.char_start = Some(verified.span.char_start);
        merged_anchor.char_end = Some(verified.span.char_end);

        let mut obj = json!({ "summary": spec.summary });
        if spec.hard_guard && trust_tier != TrustTier::ExternalUntrusted {
            obj["hard_guard"] = json!(true);
        }
        if !spec.forbidden_commands.is_empty() {
            obj["forbidden_commands"] = json!(spec.forbidden_commands);
        }
        if !spec.allowed_commands.is_empty() {
            obj["allowed_commands"] = json!(spec.allowed_commands);
        }
        if !spec.required_flags.is_empty() {
            obj["required_flags"] = json!(spec.required_flags);
        }

        let mut tags = BTreeSet::new();
        if is_raw_only {
            tags.insert("kp.raw_only".to_owned());
        }
        let fact = Fact {
            id: kp_store::new_fact_id()?,
            origin_library: store.library_id().clone(),
            logical_key,
            kind: spec.kind,
            subject: spec.subject,
            predicate: spec.predicate,
            object: obj,
            tags,
            author: actor.clone(),
            ingested_by: Some(actor.clone()),
            trust_tier,
            access: FactAccess::Base {
                audience: audience.clone(),
                stamp: BitemporalStamp {
                    valid: ValidityWindow {
                        valid_from: EpochMs(0),
                        valid_until: EpochMs::OPEN_END,
                    },
                    recorded_at: EpochMs(at.0.saturating_add(i64::try_from(idx).unwrap_or(0))),
                    expired_at: None,
                },
            },
            source_scope_id: source_scope_id.clone(),
            governed_scope: None,
            broader_scope: None,
            status: FactStatus::Active,
            superseded_by: None,
            rank: FactRank::Normal,
            source_anchor: Some(merged_anchor),
            unverified_gaps: Vec::new(),
            miss_count: 0,
        };
        let input = FactInput {
            fact,
            entities: BTreeSet::new(),
        };
        if is_raw_only {
            store.remember_with_embedder(std::slice::from_ref(&input), &FtsOnlyEmbedder)?;
        } else {
            store.remember(std::slice::from_ref(&input))?;
        }
        created += 1;
    }
    Ok(created)
}

pub async fn dream(
    store: &mut Store,
    actor: &ActorId,
    at: EpochMs,
    attributor: &dyn DreamAttributor,
) -> Result<DreamReport> {
    if store.member_role(actor)?.is_none() {
        return Err(kp_store::StoreError::PermissionDenied.into());
    }
    store.sync_skills_now()?;
    let auto_sessions = crate::session_miner::mine_unprocessed_store_sessions(store, actor, at)?;
    let auto_sources = crate::source_miner::mine_unprocessed_non_session_sources(store, actor, at)?;
    let mut report = DreamReport {
        sessions_mined: auto_sessions.sessions_scanned,
        sources_mined: auto_sources.sources_scanned,
        episodes_mined: auto_sessions
            .episodes_recorded
            .saturating_add(auto_sources.episodes_recorded),
        facts_distilled: auto_sources.declarative_facts_created,
        ..DreamReport::default()
    };
    let redactor = SecretRedactor::new()?;
    let library_id = store.library_id().clone();
    for pending in store.pending_episodes(actor)? {
        let graph = store.derivation_view()?;
        let visible = graph.evaluate(actor, at);
        let premise_ids: Vec<FactId> = pending
            .episode
            .environment
            .get("premise_ids")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        if premise_ids.iter().any(|id| !visible.facts.contains(id)) {
            let applied = store.apply_attribution(&pending, actor, None, at)?;
            report.processed += usize::from(applied.processed);
            report.kept_base += usize::from(applied.processed);
            continue;
        }
        let is_raw_only = episode_is_raw_only(store, &graph, &pending.episode, &premise_ids)?;
        let mut premises = Vec::new();
        for id in &premise_ids {
            let fact = graph.facts.get(id).ok_or(kp_store::StoreError::NotFound)?;
            premises.push(redactor.redact_json(&json!({
                "id": fact.id,
                "subject": fact.subject,
                "predicate": fact.predicate,
                "object": fact.object
            })));
        }
        let existing_skills =
            collect_existing_skill_briefs(&graph, &visible.facts, actor, &library_id);
        let used_external = !is_raw_only && attributor.uses_external_model();
        let input = DreamInput {
            episode: pending.episode.redacted()?,
            premises,
            existing_skills: if used_external {
                existing_skills
            } else {
                Vec::new()
            },
        };
        let envelope = if input.episode.has_contrast() && !input.episode.step_ref.trim().is_empty()
        {
            if is_raw_only && attributor.uses_external_model() {
                StructuredDreamAttributor.distill(&input).await?
            } else {
                attributor.distill(&input).await?
            }
        } else {
            DreamDistillEnvelope::default()
        };

        if !envelope.memory_facts.is_empty() {
            report.facts_distilled += persist_distilled_memory_facts(
                store,
                actor,
                &input.episode,
                &envelope.memory_facts,
                is_raw_only,
                at,
            )?;
        }

        let extra_rules = DreamDistillEnvelope::sanitize_extra_rules(&envelope.extra_rules)?;
        let attribution = match envelope.decision {
            DreamDecision::KeepBase => None,
            DreamDecision::Update(attribution) => {
                let mut attribution = attribution.sanitize()?;
                if attribution
                    .cited_premise_ids
                    .iter()
                    .any(|id| !premise_ids.contains(id))
                {
                    return Err(IngestError::Invalid(
                        "attribution cites evidence outside the episode",
                    ));
                }
                attribution.cited_premise_ids = premise_ids;
                if [
                    trace_text(&input.episode.before_trace),
                    trace_text(&input.episode.after_trace),
                ]
                .iter()
                .any(|trace| {
                    trace.len() >= 48
                        && [
                            &attribution.trigger,
                            &attribution.wrong_move,
                            &attribution.verified_fix,
                        ]
                        .iter()
                        .copied()
                        .chain(extra_rules.iter())
                        .any(|rule| rule.contains(trace))
                }) {
                    return Err(IngestError::Invalid(
                        "attribution copied a private transcript",
                    ));
                }
                if used_external
                    && !zep_scan_passes(&attribution, &extra_rules, &input.episode, &input.premises)
                {
                    report.scanner_rejected += 1;
                    None
                } else {
                    Some(attribution)
                }
            }
        };
        let applied = store.apply_attribution_with_rules(
            &pending,
            actor,
            attribution.as_ref(),
            &extra_rules,
            at,
        )?;
        report.processed += usize::from(applied.processed);
        report.kept_base += usize::from(applied.processed && attribution.is_none());
        report.bullets_created += usize::from(applied.created);
    }
    report.promoted = crate::skill_pkg::promote_ready(store, actor, at)?;
    Ok(report)
}
