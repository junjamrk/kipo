use std::collections::BTreeSet;

use kp_model::{
    ActorId, AnchorSpec, AudienceSpec, BitemporalStamp, ContentHash, EpochMs, Fact, FactAccess,
    FactKind, FactRank, FactStatus, KpError, LogicalKey, TrustTier, ValidityWindow,
};
use kp_store::{ChunkInput, FactInput, RawSourceInput, Remembered, Result, Store, StoredSource};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::{ExtractedFact, Extractor};

pub struct Memory {
    pub subject: String,
    pub predicate: String,
    pub value: Value,
    pub kind: FactKind,
    pub logical_key: Option<LogicalKey>,
    pub tags: BTreeSet<String>,
    pub entities: BTreeSet<String>,
    pub valid: ValidityWindow,
}

pub struct FunnelOptions {
    pub actor: ActorId,
    pub audience: AudienceSpec,
    pub at: EpochMs,
}

#[derive(Serialize)]
pub struct FunnelResult {
    pub source: StoredSource,
    pub extracted: Remembered,
}

pub fn remember(store: &mut Store, memory: Memory, options: &FunnelOptions) -> Result<Remembered> {
    let fact = base_fact(store, memory, options)?;
    store.remember(&[fact])
}

pub fn add_markdown(
    store: &mut Store,
    uri: &str,
    markdown: &str,
    options: &FunnelOptions,
    extractor: Option<&dyn Extractor>,
) -> Result<FunnelResult> {
    let extracted = match extractor {
        Some(extractor) => {
            extractor.extract_with_media_type(markdown, uri, Some("text/markdown"))?
        }
        None => Vec::new(),
    };
    let hash = ContentHash::digest(markdown);
    // An explicit immutable version key permits later edits without rewriting the original.
    let key: LogicalKey = format!("source:{uri}:{hash}").parse()?;
    let chunks = markdown_chunks(markdown)?;
    let mut inputs = Vec::new();
    for ExtractedFact {
        subject,
        predicate,
        value,
        kind,
        entities,
        tags,
    } in extracted
    {
        inputs.push(base_fact(
            store,
            Memory {
                subject,
                predicate,
                value,
                kind,
                logical_key: None,
                entities,
                tags,
                valid: ValidityWindow {
                    valid_from: options.at,
                    valid_until: EpochMs::OPEN_END,
                },
            },
            options,
        )?);
    }
    let end_line = u32::try_from(markdown.lines().count())
        .map_err(|_| KpError::invalid("markdown", "too many lines"))?;
    for input in &mut inputs {
        input.fact.source_anchor = Some(AnchorSpec {
            start_line: Some(1),
            end_line: Some(end_line),
            ..AnchorSpec::default()
        });
    }
    let (source, extracted) = store.ingest_funnel(
        &store.library_id().clone(),
        &key,
        &RawSourceInput {
            self_echo_filtered: false,
            uri: uri.into(),
            media_type: "text/markdown".into(),
            content: markdown.as_bytes().to_vec(),
            readable_by: options.audience.clone(),
            captured_at: options.at,
            chunks,
        },
        &kp_store::HashEmbedder,
        &mut inputs,
    )?;
    Ok(FunnelResult { source, extracted })
}

pub fn base_fact(store: &Store, memory: Memory, options: &FunnelOptions) -> Result<FactInput> {
    let id = kp_model::FactId::new(Uuid::new_v4().to_string())?;
    let logical_key = match memory.logical_key {
        Some(key) => key,
        None => id.as_str().parse()?,
    };
    let fact = Fact {
        id,
        origin_library: store.library_id().clone(),
        logical_key,
        kind: memory.kind,
        subject: memory.subject.trim().into(),
        predicate: memory.predicate.trim().into(),
        object: memory.value,
        tags: memory.tags,
        author: options.actor.clone(),
        ingested_by: None,
        trust_tier: TrustTier::HumanVerbatim,
        access: FactAccess::Base {
            audience: options.audience.clone(),
            stamp: BitemporalStamp {
                valid: memory.valid,
                recorded_at: options.at,
                expired_at: None,
            },
        },
        source_scope_id: None,
        governed_scope: None,
        broader_scope: None,
        status: FactStatus::Active,
        superseded_by: None,
        rank: FactRank::Normal,
        source_anchor: None,
        unverified_gaps: Vec::new(),
        miss_count: 0,
    };
    fact.validate()?;
    Ok(FactInput {
        fact,
        entities: memory
            .entities
            .into_iter()
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect(),
    })
}

/// Heading/paragraph boundaries with exact source byte and line anchors; fenced code stays intact.
pub fn markdown_chunks(markdown: &str) -> Result<Vec<ChunkInput>> {
    let mut chunks = Vec::new();
    let mut heading = String::new();
    let mut start = 0;
    let mut start_line = 1;
    let mut offset = 0;
    let mut fenced = false;
    let lines: Vec<_> = markdown.split_inclusive('\n').collect();
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
        }
        if !fenced && (trimmed.is_empty() || trimmed.starts_with('#')) && offset > start {
            push_chunk(
                &mut chunks,
                markdown,
                &heading,
                start..offset,
                start_line,
                index,
            )?;
            start = offset;
            start_line = index + 1;
        }
        if !fenced && trimmed.starts_with('#') {
            heading = trimmed.trim_start_matches('#').trim().into();
        }
        offset += line.len();
    }
    push_chunk(
        &mut chunks,
        markdown,
        &heading,
        start..markdown.len(),
        start_line,
        lines.len(),
    )?;
    Ok(chunks)
}

fn push_chunk(
    chunks: &mut Vec<ChunkInput>,
    markdown: &str,
    heading: &str,
    range: std::ops::Range<usize>,
    start_line: usize,
    end_line: usize,
) -> Result<()> {
    let body = &markdown[range.clone()];
    if !body.trim().is_empty() {
        chunks.push(ChunkInput {
            heading: heading.into(),
            body: body.into(),
            anchor: AnchorSpec {
                start_byte: Some(range.start as u64),
                end_byte: Some(range.end as u64),
                start_line: Some(
                    u32::try_from(start_line)
                        .map_err(|_| KpError::invalid("markdown", "too many lines"))?,
                ),
                end_line: Some(
                    u32::try_from(end_line)
                        .map_err(|_| KpError::invalid("markdown", "too many lines"))?,
                ),
                ..AnchorSpec::default()
            },
        });
    }
    Ok(())
}
