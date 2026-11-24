use std::collections::BTreeSet;

use kp_model::{
    verify_verbatim_quote, ActorId, Attribution, AudienceSpec, BitemporalStamp, ContentHash,
    ContrastKind, EpochMs, Fact, FactAccess, FactId, FactKind, FactRank, FactStatus,
    ModelRoutePolicy, QuoteLocationHint, RejectionEvidenceKey, RejectionRecord, RsiDimension,
    SecretRedactor, SourceId, SourceModality, SourceScopeId, TrustTier, ValidityWindow,
};
use kp_store::{AnchoredSourceChunk, Episode, FactInput, FtsOnlyEmbedder, Store, StoreError};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::LazyLock;

use crate::session_miner::scrub_personal_home_paths;
use crate::Result;

static SLUG_TOKEN_RE: LazyLock<std::result::Result<Regex, regex::Error>> =
    LazyLock::new(|| Regex::new(r"[a-zA-Z][a-zA-Z0-9_-]{2,24}"));

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultiSourceMineReport {
    pub sources_scanned: usize,
    pub declarative_facts_created: usize,
    pub episodes_recorded: usize,
}

pub fn mine_unprocessed_non_session_sources(
    store: &mut Store,
    actor: &ActorId,
    at: EpochMs,
) -> Result<MultiSourceMineReport> {
    if store.member_role(actor)?.is_none() {
        return Err(StoreError::PermissionDenied.into());
    }
    let unmined: Vec<_> = store
        .unmined_dream_sources(actor)?
        .into_iter()
        .filter(|s| s.modality != SourceModality::AgentSession)
        .collect();
    if unmined.is_empty() {
        return Ok(MultiSourceMineReport::default());
    }

    let rejections = store.list_rejections(None)?;
    let redactor = SecretRedactor::new()?;
    let mut report = MultiSourceMineReport::default();

    for src in unmined {
        let chunks = store.source_anchored_chunks(&src.source_id)?;
        if chunks.is_empty() {
            store.mark_session_source_dream_mined(&src.source_id, actor, at)?;
            continue;
        }
        let ctx_opt = store.extraction_source_context(&src.source_id, &[])?;
        let audience = ctx_opt
            .as_ref()
            .map(|c| c.readable_by.clone())
            .unwrap_or_else(|| AudienceSpec::Explicit(BTreeSet::from([actor.clone()])));
        let route_policy = store.source_effective_route_policy(&src.source_id)?;
        let trust_tier = if src.modality == SourceModality::WebLink {
            TrustTier::ExternalUntrusted
        } else {
            TrustTier::HumanVerbatim
        };
        let unit_id = derive_independent_source_unit(&src.uri, &src.source_id);

        report.sources_scanned += 1;
        let mut episodes_in_source = 0usize;

        for (c_idx, chunk) in chunks.iter().enumerate() {
            let candidates =
                extract_chunk_candidates(&chunk.heading, &chunk.body, src.modality, &redactor)?;
            for (k_idx, cand) in candidates.into_iter().enumerate() {
                let step_at =
                    EpochMs(at.0.saturating_add(i64::try_from(c_idx * 16 + k_idx).unwrap_or(0)));
                if let Some(decl_kind) = cand.declarative_kind {
                    if let Ok(Some((_, true))) = persist_chunk_fact(
                        store,
                        &ChunkFactSpec {
                            source_id: &src.source_id,
                            source_scope_id: src.source_scope_id.clone(),
                            chunk,
                            actor,
                            audience: &audience,
                            trust_tier,
                            route_policy: &route_policy,
                            rejections: &rejections,
                            kind: decl_kind,
                            subject: &cand.subject,
                            predicate: &cand.predicate,
                            summary: &cand.clean_summary,
                            verbatim_quote: &cand.verbatim_quote,
                            hard_guard: cand.hard_guard
                                && trust_tier != TrustTier::ExternalUntrusted,
                            logical_prefix: "dream-decl",
                            at: step_at,
                        },
                    ) {
                        report.declarative_facts_created += 1;
                    }
                }

                if episodes_in_source < 4 {
                    if let Some((wrong_move, verified_fix, slug, trigger, dim)) = cand.contrast {
                        let ev_res = persist_chunk_fact(
                            store,
                            &ChunkFactSpec {
                                source_id: &src.source_id,
                                source_scope_id: src.source_scope_id.clone(),
                                chunk,
                                actor,
                                audience: &AudienceSpec::Explicit(BTreeSet::from([actor.clone()])),
                                trust_tier,
                                route_policy: &route_policy,
                                rejections: &rejections,
                                kind: FactKind::Observation,
                                subject: &format!("source:{unit_id}"),
                                predicate: "kp.rsi.evidence",
                                summary: &cand.verbatim_quote,
                                verbatim_quote: &cand.verbatim_quote,
                                hard_guard: false,
                                logical_prefix: "rsi-evidence",
                                at: step_at,
                            },
                        );
                        let Ok(Some((ev_id, _))) = ev_res else {
                            continue;
                        };
                        let Ok(parsed_slug) = slug.parse() else {
                            continue;
                        };
                        let fallback_attr = Attribution {
                            skill_slug: parsed_slug,
                            trigger,
                            wrong_move: wrong_move.clone(),
                            verified_fix: verified_fix.clone(),
                            dimension: dim,
                            cited_premise_ids: Vec::new(),
                        };
                        let Ok(fallback_attr) = fallback_attr.sanitize() else {
                            continue;
                        };
                        let mut env = json!({
                            "actor": actor,
                            "verified": true,
                            "source_id": src.source_id,
                            "modality": src.modality.as_str(),
                            "target": fallback_attr.skill_slug.as_str(),
                            "fallback_attribution": fallback_attr,
                        });
                        if route_policy == ModelRoutePolicy::RawOnly {
                            env["model_policy"] = json!("raw_only");
                            env["raw_only"] = json!(true);
                        }
                        let ep = Episode {
                            signal_kind: ContrastKind::HumanCorrection,
                            session_id: format!("src:{unit_id}"),
                            before_trace: json!({
                                "content": format!("[{}] unguarded execution", cand.subject),
                                "success": false,
                                "target": fallback_attr.skill_slug.as_str(),
                            }),
                            after_trace: json!({
                                "content": cand.clean_summary,
                                "success": true,
                                "target": fallback_attr.skill_slug.as_str(),
                            }),
                            step_ref: format!("chunk:{}:{k_idx}", chunk.chunk_id),
                            environment: env,
                        };
                        if let Ok(n) = crate::signals::record(
                            store,
                            ep,
                            actor,
                            std::slice::from_ref(&ev_id),
                            step_at,
                        ) {
                            report.episodes_recorded += n;
                            episodes_in_source += n;
                        }
                    }
                }
            }
        }

        store.mark_session_source_dream_mined(&src.source_id, actor, at)?;
    }

    Ok(report)
}

struct ChunkCandidate {
    declarative_kind: Option<FactKind>,
    hard_guard: bool,
    subject: String,
    predicate: String,
    clean_summary: String,
    verbatim_quote: String,
    contrast: Option<(String, String, String, String, RsiDimension)>,
}

struct ChunkFactSpec<'a> {
    source_id: &'a SourceId,
    source_scope_id: Option<SourceScopeId>,
    chunk: &'a AnchoredSourceChunk,
    actor: &'a ActorId,
    audience: &'a AudienceSpec,
    trust_tier: TrustTier,
    route_policy: &'a ModelRoutePolicy,
    rejections: &'a [RejectionRecord],
    kind: FactKind,
    subject: &'a str,
    predicate: &'a str,
    summary: &'a str,
    verbatim_quote: &'a str,
    hard_guard: bool,
    logical_prefix: &'a str,
    at: EpochMs,
}

fn persist_chunk_fact(
    store: &mut Store,
    spec: &ChunkFactSpec<'_>,
) -> Result<Option<(FactId, bool)>> {
    let hint = QuoteLocationHint {
        turn_ordinal: None,
        approx_char_offset: spec
            .chunk
            .body
            .find(spec.verbatim_quote)
            .and_then(|b| u32::try_from(spec.chunk.body[..b].chars().count()).ok()),
    };
    let Ok(verified) = verify_verbatim_quote(&spec.chunk.body, spec.verbatim_quote, hint) else {
        return Ok(None);
    };
    let ev_key = RejectionEvidenceKey {
        speaker: spec.actor.clone(),
        chunk_sha256: ContentHash::digest(&spec.chunk.body),
        quote_span: verified.span,
    };
    if spec
        .rejections
        .iter()
        .any(|rec| rec.matches_candidate(Some(&ev_key), None))
    {
        return Ok(None);
    }

    let digest = ContentHash::digest(
        serde_json::to_vec(&(
            store.library_id(),
            spec.actor,
            spec.source_id,
            &spec.chunk.chunk_id,
            spec.logical_prefix,
            &verified.exact_quote,
            verified.span.char_start,
        ))?
        .as_slice(),
    );
    let logical_key: kp_model::LogicalKey = format!("{}:{digest}", spec.logical_prefix).parse()?;
    if let Some(existing) = store
        .visible_facts(spec.actor, spec.at)?
        .into_iter()
        .find(|f| f.origin_library == *store.library_id() && f.logical_key == logical_key)
    {
        return Ok(Some((existing.id, false)));
    }

    let fact_id = kp_store::new_fact_id()?;
    let mut tags = BTreeSet::new();
    if *spec.route_policy == ModelRoutePolicy::RawOnly {
        tags.insert("kp.raw_only".to_owned());
    }
    let mut merged_anchor = spec.chunk.anchor.clone();
    merged_anchor.source_id = Some(spec.source_id.clone());
    merged_anchor.chunk_id = Some(spec.chunk.chunk_id.clone());
    merged_anchor.verbatim_quote = Some(verified.exact_quote.clone());
    merged_anchor.char_start = Some(verified.span.char_start);
    merged_anchor.char_end = Some(verified.span.char_end);

    let clean_summary = scrub_personal_home_paths(spec.summary);
    let mut obj = json!({
        "summary": clean_summary,
    });
    if spec.hard_guard {
        obj["hard_guard"] = json!(true);
    }

    let fact = Fact {
        id: fact_id.clone(),
        origin_library: store.library_id().clone(),
        logical_key,
        kind: spec.kind,
        subject: spec.subject.to_owned(),
        predicate: spec.predicate.to_owned(),
        object: obj,
        tags,
        author: spec.actor.clone(),
        ingested_by: Some(spec.actor.clone()),
        trust_tier: spec.trust_tier,
        access: FactAccess::Base {
            audience: spec.audience.clone(),
            stamp: BitemporalStamp {
                valid: ValidityWindow {
                    valid_from: EpochMs(0),
                    valid_until: EpochMs::OPEN_END,
                },
                recorded_at: spec.at,
                expired_at: None,
            },
        },
        status: FactStatus::Active,
        superseded_by: None,
        source_anchor: Some(merged_anchor),
        governed_scope: None,
        broader_scope: None,
        source_scope_id: spec.source_scope_id.clone(),
        rank: FactRank::Normal,
        miss_count: 0,
        unverified_gaps: Vec::new(),
    };
    let input = FactInput {
        fact,
        entities: BTreeSet::new(),
    };
    let remembered = if *spec.route_policy == ModelRoutePolicy::RawOnly {
        store.remember_with_embedder(std::slice::from_ref(&input), &FtsOnlyEmbedder)?
    } else {
        store.remember(std::slice::from_ref(&input))?
    };
    let out_id = remembered.facts.into_iter().next().unwrap_or(fact_id);
    Ok(Some((out_id, true)))
}

fn extract_chunk_candidates(
    heading: &str,
    body: &str,
    modality: SourceModality,
    redactor: &SecretRedactor,
) -> Result<Vec<ChunkCandidate>> {
    let mut out = Vec::new();
    let default_subj = if !heading.trim().is_empty() {
        sanitize_subject(heading)
    } else {
        modality.as_str().to_owned()
    };

    for raw_line in body.lines() {
        let line = strip_speaker_prefix(raw_line).trim();
        if line.chars().count() < 8 || line.chars().count() > 360 {
            continue;
        }
        let scrubbed = scrub_personal_home_paths(&redactor.redact(line));
        let lower = scrubbed.to_lowercase();

        let is_prohibit = [
            "严禁",
            "禁止",
            "绝不",
            "不得",
            "切勿",
            "不要直接",
            "不要使用",
            "不要用",
            "must never",
            "never run",
            "never use",
            "do not use",
            "do not run",
            "don't use",
            "don't run",
        ]
        .iter()
        .any(|k| lower.contains(k));

        let is_decision = [
            "必须统一",
            "统一改用",
            "统一使用",
            "架构决定",
            "必须先",
            "务必执行",
            "always pass",
            "always run",
            "always use",
            "instead of ",
        ]
        .iter()
        .any(|k| lower.contains(k));

        let is_pitfall = [
            "踩坑",
            "死胡同",
            "不要盲目",
            "而不是盲目",
            "pitfall:",
            "dead-end:",
            "anti-pattern:",
        ]
        .iter()
        .any(|k| lower.contains(k));

        if !is_prohibit && !is_decision && !is_pitfall {
            continue;
        }

        let (decl_kind, predicate, hard_guard) = if is_prohibit {
            (Some(FactKind::Constraint), "kp.policy.guard", true)
        } else if is_pitfall {
            (Some(FactKind::ClosedNegative), "kp.code.pitfall", false)
        } else {
            (
                Some(FactKind::Decision),
                "kp.policy.decision",
                is_prohibit || lower.contains("instead of "),
            )
        };

        let contrast = derive_contrast_from_normative_line(&scrubbed, heading);
        out.push(ChunkCandidate {
            declarative_kind: decl_kind,
            hard_guard,
            subject: default_subj.clone(),
            predicate: predicate.to_owned(),
            clean_summary: scrubbed,
            verbatim_quote: line.to_owned(),
            contrast,
        });
    }
    Ok(out)
}

fn strip_speaker_prefix(line: &str) -> &str {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix("[speaker:") {
        if let Some(close) = rest.find(']') {
            return rest[close + 1..].trim_start();
        }
    }
    if let Some(rest) = trimmed.strip_prefix("[assistant]") {
        return rest.trim_start();
    }
    trimmed
}

fn derive_contrast_from_normative_line(
    line: &str,
    heading: &str,
) -> Option<(String, String, String, String, RsiDimension)> {
    let lower = line.to_lowercase();
    let has_pivot = [
        "改用",
        "必须",
        "应该",
        "应当",
        "而不是",
        "先检查",
        "instead of ",
        "without ",
        "always ",
        "before ",
    ]
    .iter()
    .any(|k| lower.contains(k));
    if !has_pivot {
        return None;
    }

    let slug = derive_topic_slug(line, heading);
    let trigger = if !heading.trim().is_empty() && heading.len() <= 80 {
        format!("working on {}", heading.trim())
    } else {
        format!("applying {} workflow rules", slug.replace('-', " "))
    };
    let wrong_move = format!("Proceed without checking `{slug}` constraints first");
    let preview = truncate_utf8_bytes(line, 36);
    let fix =
        format!("Verify `{slug}` constraint ({preview}) and confirm clean exit before continuing.");
    let dim = if [
        "pip", "npm", "pnpm", "yarn", "pytest", "cargo", "prisma", "sqlx", "docker", "git",
    ]
    .iter()
    .any(|t| lower.contains(t))
    {
        RsiDimension::Inv
    } else {
        RsiDimension::Dom
    };
    Some((wrong_move, fix, slug, trigger, dim))
}

fn derive_topic_slug(line: &str, heading: &str) -> String {
    if let Ok(re) = SLUG_TOKEN_RE.as_ref() {
        let mut words = Vec::new();
        for m in re.find_iter(&format!("{heading} {line}")) {
            let w = m.as_str().to_ascii_lowercase().replace('_', "-");
            if matches!(
                w.as_str(),
                "never"
                    | "always"
                    | "without"
                    | "instead"
                    | "before"
                    | "after"
                    | "run"
                    | "use"
                    | "pass"
                    | "the"
                    | "and"
                    | "for"
                    | "track"
                    | "segment"
                    | "cue"
                    | "page"
            ) {
                continue;
            }
            if !words.contains(&w) {
                words.push(w);
            }
            if words.len() >= 3 {
                break;
            }
        }
        if !words.is_empty() {
            let joined = format!("{}-rule", words.join("-"));
            if joined.len() <= 62 {
                return joined;
            }
        }
    }
    let digest = ContentHash::digest(line.as_bytes());
    format!("domain-rule-{}", &digest.as_str()[..10])
}

fn truncate_utf8_bytes(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].trim_end()
}

fn sanitize_subject(raw: &str) -> String {
    let collapsed = raw
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.is_empty() {
        "document".to_owned()
    } else {
        collapsed.chars().take(80).collect()
    }
}

fn derive_independent_source_unit(uri: &str, source_id: &SourceId) -> String {
    let clean = scrub_personal_home_paths(uri);
    let digest = ContentHash::digest(clean.as_bytes());
    format!("u-{}", &digest.as_str()[..16])
        .replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "-")
        + "-"
        + &source_id.as_str()[source_id.as_str().len().saturating_sub(6)..]
}
