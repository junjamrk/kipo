use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    verify_verbatim_quote, ActorId, AnchorSpec, CalendarTable, CharSpan, CommunityConcept,
    ConceptId, ConceptKind, ConceptPolarity, ContentHash, FactId, FactKind, KpError,
    QuoteLocationHint, Result, SourceId, ValidityWindow, VerifiedQuoteMatch,
};

pub fn normalize_for_dedup(text: &str) -> String {
    let mut tokens: Vec<char> = Vec::with_capacity(text.len());
    let mut prev_space = true;
    for raw in text.chars() {
        let folded = if ('\u{FF01}'..='\u{FF5E}').contains(&raw) {
            char::from_u32((raw as u32) - 0xFEE0).unwrap_or(raw)
        } else {
            raw
        };
        let is_sep = folded.is_whitespace()
            || matches!(
                folded,
                '\u{3000}'
                    | '\u{201C}'
                    | '\u{201D}'
                    | '\u{2018}'
                    | '\u{2019}'
                    | '「'
                    | '」'
                    | '『'
                    | '』'
                    | '。'
                    | '、'
                    | '.'
                    | '!'
                    | '?'
                    | ','
                    | ';'
                    | ':'
                    | '"'
                    | '\''
                    | '('
                    | ')'
                    | '['
                    | ']'
            );
        if is_sep {
            if !prev_space {
                tokens.push(' ');
                prev_space = true;
            }
        } else {
            let ch = folded.to_ascii_lowercase();
            if prev_space && tokens.last() == Some(&' ') {
                let prev_non_space = tokens.iter().rev().nth(1).copied();
                if !ch.is_ascii() || prev_non_space.is_some_and(|p| !p.is_ascii()) {
                    tokens.pop();
                }
            }
            tokens.push(ch);
            prev_space = false;
        }
    }
    if tokens.last() == Some(&' ') {
        tokens.pop();
    }
    tokens.into_iter().collect()
}

pub fn is_trivial_turn_text(text: &str) -> bool {
    fn is_single_ack(norm: &str) -> bool {
        matches!(
            norm,
            "好的"
                | "好"
                | "收到"
                | "明白"
                | "知道了"
                | "已阅"
                | "嗯"
                | "嗯嗯"
                | "恩"
                | "哦"
                | "行"
                | "可以"
                | "没问题"
                | "同意"
                | "赞成"
                | "谢谢"
                | "感谢"
                | "ok"
                | "okay"
                | "k"
                | "ack"
                | "lgtm"
                | "+1"
                | "yes"
                | "yep"
                | "agreed"
                | "ship it"
                | "thanks"
                | "thx"
                | "got it"
                | "roger"
        )
    }

    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == "<skip_summary/>" {
        return true;
    }
    let norm = normalize_for_dedup(trimmed);
    if norm.is_empty() {
        return true;
    }
    if is_single_ack(norm.as_str()) {
        return true;
    }
    let clauses: Vec<String> = trimmed
        .split([
            '\n', '\r', ',', '，', '.', '。', '!', '！', '?', '？', ';', '；', '、', '~', '～',
        ])
        .map(normalize_for_dedup)
        .filter(|s| !s.is_empty())
        .collect();
    !clauses.is_empty() && clauses.iter().all(|c| is_single_ack(c.as_str()))
}

/// Line prefix marking an assistant turn inside an extraction chunk (context only, never a claim).
pub const ASSISTANT_TURN_TAG: &str = "[assistant]";

/// Line prefix marking a human speaker turn inside an extraction chunk: `[speaker:<actor_id>]`.
pub const SPEAKER_TURN_TAG_PREFIX: &str = "[speaker:";

/// One attributed turn used to render a multi-speaker extraction chunk via [`format_tagged_turns`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkTurn {
    pub speaker: Option<ActorId>,
    pub is_assistant: bool,
    pub text: String,
}

/// Parsed turn of a chunk body: who spoke and where the spoken text (excluding the tag) lies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkTurnSpan {
    pub turn_ordinal: u32,
    /// `None` for untagged context text (e.g. a heading before the first tagged turn).
    pub speaker: Option<ActorId>,
    /// Assistant turns and malformed tags are context only and can never anchor a claim.
    pub is_assistant: bool,
    /// Character span of the turn text inside the chunk body (`None` if the turn has no text).
    pub text_span: Option<CharSpan>,
}

/// Renders attributed turns as `[speaker:<actor>] text` / `[assistant] text` lines (one per turn).
/// Newlines inside a turn are flattened to spaces so every turn stays on its own tagged line.
pub fn format_tagged_turns(turns: &[ChunkTurn]) -> String {
    turns
        .iter()
        .map(|turn| {
            let text = turn
                .text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            match (&turn.speaker, turn.is_assistant) {
                (_, true) => format!("{ASSISTANT_TURN_TAG} {text}"),
                (Some(actor), false) => format!("{SPEAKER_TURN_TAG_PREFIX}{actor}] {text}"),
                (None, false) => text,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Returns true when `body` contains at least one `[speaker:..]` or `[assistant]` tagged line.
pub fn has_tagged_turns(body: &str) -> bool {
    body.lines().any(|line| {
        let l = line.trim_start();
        l.starts_with(ASSISTANT_TURN_TAG) || l.starts_with(SPEAKER_TURN_TAG_PREFIX)
    })
}

/// Splits a chunk body into attributed turns. A turn starts at a line beginning with
/// `[speaker:<actor>]` or `[assistant]` and continues until the next tagged line.
pub fn parse_tagged_turns(body: &str) -> Vec<ChunkTurnSpan> {
    struct Open {
        speaker: Option<ActorId>,
        is_assistant: bool,
        text_start: Option<usize>,
        text_end: usize,
    }
    fn close(open: Open, turns: &mut Vec<ChunkTurnSpan>) {
        let text_span = open.text_start.and_then(|start| {
            let s = u32::try_from(start).ok()?;
            let e = u32::try_from(open.text_end).ok()?;
            CharSpan::new(s, e).ok()
        });
        let turn_ordinal = u32::try_from(turns.len()).unwrap_or(u32::MAX);
        turns.push(ChunkTurnSpan {
            turn_ordinal,
            speaker: open.speaker,
            is_assistant: open.is_assistant,
            text_span,
        });
    }

    let mut turns = Vec::new();
    let mut current: Option<Open> = None;
    let mut char_cursor = 0usize;
    for raw_line in body.split_inclusive('\n') {
        let line_chars: Vec<char> = raw_line.chars().collect();
        let line_start = char_cursor;
        char_cursor += line_chars.len();
        let leading_ws = line_chars.iter().take_while(|c| c.is_whitespace()).count();
        let rest: String = line_chars[leading_ws..].iter().collect();

        let tag = if rest.starts_with(ASSISTANT_TURN_TAG) {
            Some((None, true, ASSISTANT_TURN_TAG.chars().count()))
        } else if rest.starts_with(SPEAKER_TURN_TAG_PREFIX) {
            match rest.find(']') {
                Some(close_byte) => {
                    let id_text = &rest[SPEAKER_TURN_TAG_PREFIX.len()..close_byte];
                    let tag_chars = rest[..=close_byte].chars().count();
                    match id_text.trim().parse::<ActorId>() {
                        Ok(actor) => Some((Some(actor), false, tag_chars)),
                        // Malformed speaker ids are never attributable: treat as context only.
                        Err(_) => Some((None, true, tag_chars)),
                    }
                }
                None => Some((None, true, rest.chars().count())),
            }
        } else {
            None
        };

        let content_from = match &tag {
            Some((speaker, is_assistant, tag_chars)) => {
                if let Some(open) = current.take() {
                    close(open, &mut turns);
                }
                current = Some(Open {
                    speaker: speaker.clone(),
                    is_assistant: *is_assistant,
                    text_start: None,
                    text_end: 0,
                });
                leading_ws + tag_chars
            }
            None => {
                if current.is_none() {
                    current = Some(Open {
                        speaker: None,
                        is_assistant: false,
                        text_start: None,
                        text_end: 0,
                    });
                }
                0
            }
        };
        if let Some(open) = current.as_mut() {
            let content = &line_chars[content_from.min(line_chars.len())..];
            if let Some(first) = content.iter().position(|c| !c.is_whitespace()) {
                let last = content
                    .iter()
                    .rposition(|c| !c.is_whitespace())
                    .unwrap_or(first);
                if open.text_start.is_none() {
                    open.text_start = Some(line_start + content_from + first);
                }
                open.text_end = line_start + content_from + last + 1;
            }
        }
    }
    if let Some(open) = current.take() {
        close(open, &mut turns);
    }
    turns
}

/// Calendar date range emitted by the model instead of raw epoch milliseconds; `kp` resolves it
/// deterministically against the request's [`CalendarTable`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelDateRange {
    /// `YYYY-MM-DD` or a `CalendarTable` anchor label.
    pub start_date_iso: String,
    #[serde(default)]
    pub end_date_iso: Option<String>,
    /// True when there is no end date (e.g. "from next Wednesday on").
    #[serde(default)]
    pub open_ended: bool,
}

/// Upper bound on active decisions passed to a single extraction batch (top relevant candidates only).
pub const MAX_ACTIVE_DECISIONS_PER_BATCH: usize = 16;

/// Upper bound on the speaker's own active facts passed for `SupersedeOwn` resolution.
pub const MAX_SPEAKER_FACTS_PER_BATCH: usize = 32;

/// Default token budget for rendered turn/chunk text grouped into one extraction batch call.
pub const DEFAULT_MAX_BATCH_TOKENS: usize = 2_400;

/// Conservative deterministic BPE token upper-bound estimator across mixed CJK, ASCII identifiers,
/// punctuation, and speaker tags (`[speaker:...]`).
pub fn estimate_batch_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let mut ascii_tokens = 0usize;
    let mut ascii_run = 0usize;
    let mut non_ascii_scalars = 0usize;

    for ch in text.chars() {
        if ch.is_ascii() {
            if ch.is_ascii_alphanumeric() {
                ascii_run += 1;
            } else {
                if ascii_run > 0 {
                    ascii_tokens += ascii_run.div_ceil(4);
                    ascii_run = 0;
                }
                if ch == '\n' || !ch.is_ascii_whitespace() {
                    ascii_tokens += 1;
                }
            }
        } else {
            if ascii_run > 0 {
                ascii_tokens += ascii_run.div_ceil(4);
                ascii_run = 0;
            }
            if !ch.is_whitespace() {
                non_ascii_scalars += 1;
            }
        }
    }
    if ascii_run > 0 {
        ascii_tokens += ascii_run.div_ceil(4);
    }
    ascii_tokens + (non_ascii_scalars.saturating_mul(3)).div_ceil(2)
}

/// Groups consecutive attributed [`ChunkTurn`]s into batches bounded by `max_batch_tokens`
/// rather than a fixed turn count. A single turn exceeding `max_batch_tokens` is emitted in its
/// own 1-turn batch so no turn is ever dropped.
pub fn partition_turns_by_token_budget(
    turns: &[ChunkTurn],
    max_batch_tokens: usize,
) -> Vec<Vec<ChunkTurn>> {
    let budget = max_batch_tokens.max(1);
    let mut batches: Vec<Vec<ChunkTurn>> = Vec::new();
    let mut current: Vec<ChunkTurn> = Vec::new();
    let mut current_tokens = 0usize;

    for turn in turns {
        let rendered = format_tagged_turns(std::slice::from_ref(turn));
        let turn_tokens = estimate_batch_tokens(&rendered).saturating_add(1).max(1);
        if !current.is_empty() && current_tokens.saturating_add(turn_tokens) > budget {
            batches.push(std::mem::take(&mut current));
            current_tokens = 0;
        }
        current_tokens = current_tokens.saturating_add(turn_tokens);
        current.push(turn.clone());
        if current_tokens >= budget {
            batches.push(std::mem::take(&mut current));
            current_tokens = 0;
        }
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

/// Groups consecutive [`ExtractionChunkSlice`]s into batches bounded by `max_batch_tokens`.
/// Oversized single chunks form a 1-chunk batch rather than being dropped.
pub fn partition_chunks_by_token_budget(
    chunks: &[ExtractionChunkSlice],
    max_batch_tokens: usize,
) -> Vec<Vec<ExtractionChunkSlice>> {
    let budget = max_batch_tokens.max(1);
    let mut batches: Vec<Vec<ExtractionChunkSlice>> = Vec::new();
    let mut current: Vec<ExtractionChunkSlice> = Vec::new();
    let mut current_tokens = 0usize;

    for chunk in chunks {
        let chunk_tokens = estimate_batch_tokens(&chunk.heading)
            .saturating_add(estimate_batch_tokens(&chunk.body))
            .saturating_add(4)
            .max(1);
        if !current.is_empty() && current_tokens.saturating_add(chunk_tokens) > budget {
            batches.push(std::mem::take(&mut current));
            current_tokens = 0;
        }
        current_tokens = current_tokens.saturating_add(chunk_tokens);
        current.push(chunk.clone());
        if current_tokens >= budget {
            batches.push(std::mem::take(&mut current));
            current_tokens = 0;
        }
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

/// Per-connector or per-library model routing policy for compliance isolation.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(
    tag = "mode",
    content = "endpoint",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ModelRoutePolicy {
    /// Default out-of-session host CLI (`claude -p --bare` / `codex exec`).
    #[default]
    HostCli,
    /// Explicit OpenAI-compatible internal or cloud endpoint name.
    NamedEndpoint(String),
    /// Store L0 raw snapshot and FTS/local vector only; zero generative model calls.
    RawOnly,
}

/// Four-way strict semantic entailment verdict for cross-library open concept alignment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum EntailmentVerdict {
    /// Occurrence of `left` necessarily satisfies/triggers condition `right`.
    Entails,
    /// `left` resolves, cancels, or contradicts `right` in the same domain/window.
    Contradicts,
    /// Merely same city/topic without causal or logical trigger relation (blocks false positives).
    Unrelated,
    /// Boundary unclear; route to `kp review` rather than guessing.
    Ambiguous { question: String },
}

fn default_neutral_polarity() -> ConceptPolarity {
    ConceptPolarity::Neutral
}

/// Narrow model-boundary concept reference: either cites an `active_concepts` ID or proposes a label;
/// `kp` allocates `ConceptId`, `proposed_by`, and `created_at`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "ref_type", rename_all = "snake_case")]
pub enum ModelConceptRef {
    Existing {
        concept_id: ConceptId,
    },
    ProposeNew {
        kind: ConceptKind,
        label: String,
        #[serde(default)]
        domain: String,
        #[serde(default = "default_neutral_polarity")]
        polarity: ConceptPolarity,
    },
}

/// Narrow model-boundary scope reference: the model cannot emit raw `ActorId`s directly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum ModelScopeRef {
    /// Strictly governs the speaker alone ("我自己...").
    SpeakerSelf,
    /// Named people mentioned in the text; `kp` resolves mentions against known members and aliases.
    NamedMentions { mentions: Vec<String> },
    /// A relation group anchored on the speaker, referencing a `ConceptKind::Group` concept.
    RelationGroup { group_concept: ModelConceptRef },
}

/// Ambiguous scope emitted by the model: `kp` always activates `SpeakerSelf` first and stores `broader_candidate`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelAmbiguousScope {
    pub broader_candidate: ModelScopeRef,
    pub question: String,
}

/// Narrow model-boundary write operation: hard deletion and mutating another actor's fact are impossible.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ModelWriteOp {
    #[default]
    Add,
    /// Supersedes an existing fact authored by the SAME speaker listed in `speaker_own_facts`.
    SupersedeOwn {
        target_fact_id: FactId,
    },
    None,
}

/// Verbatim quote anchor emitted by the model with optional turn/offset hints for duplicate disambiguation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelQuoteAnchor {
    pub chunk_id: String,
    pub verbatim_quote: String,
    #[serde(default)]
    pub turn_ordinal: Option<u32>,
    #[serde(default)]
    pub approx_char_offset: Option<u32>,
}

impl ModelQuoteAnchor {
    pub fn hint(&self) -> QuoteLocationHint {
        QuoteLocationHint {
            turn_ordinal: self.turn_ordinal,
            approx_char_offset: self.approx_char_offset,
        }
    }
}

/// Narrow model-boundary extraction atom validated and resolved into domain objects by `kp`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelExtractedAtom {
    pub kind: FactKind,
    pub claim_text: String,
    pub value: Value,
    #[serde(default)]
    pub scope: Option<ModelScopeRef>,
    #[serde(default)]
    pub ambiguous_scope: Option<ModelAmbiguousScope>,
    #[serde(default)]
    pub condition_concept: Option<ModelConceptRef>,
    #[serde(default)]
    pub action_concept: Option<ModelConceptRef>,
    #[serde(default)]
    pub group_concept: Option<ModelConceptRef>,
    #[serde(default)]
    pub member_mention: Option<String>,
    /// Legacy/internal epoch window. Model replies use `valid_dates` instead (never epoch ms).
    #[serde(default)]
    pub valid_window: Option<ValidityWindow>,
    /// Calendar date range emitted by the model; takes precedence over `valid_window`.
    #[serde(default)]
    pub valid_dates: Option<ModelDateRange>,
    /// Speaker the model attributes the claim to (must match the `[speaker:..]` turn holding the quote).
    #[serde(default)]
    pub speaker: Option<ActorId>,
    #[serde(default)]
    pub assumed_premise_ids: Vec<FactId>,
    pub quote: ModelQuoteAnchor,
    #[serde(default)]
    pub evidence: Option<ModelQuoteAnchor>,
    #[serde(default)]
    pub write_op: ModelWriteOp,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SliceRole {
    #[default]
    Target,
    Context,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExtractionChunkSlice {
    pub chunk_id: String,
    pub source_id: SourceId,
    pub author: ActorId,
    #[serde(default)]
    pub base_anchor: AnchorSpec,
    #[serde(default)]
    pub role: SliceRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_chunk_id: Option<String>,
    pub heading: String,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedEvidenceAnchor {
    pub slice_index: usize,
    pub matched: VerifiedQuoteMatch,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActiveDecisionBrief {
    pub fact_id: FactId,
    pub subject: String,
    pub predicate: String,
    pub value: Value,
}

/// Summary of an active fact authored by `ExtractionBatchRequest.speaker` eligible for `SupersedeOwn`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeakerFactBrief {
    pub fact_id: FactId,
    pub kind: FactKind,
    pub summary: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExtractionBatchRequest {
    pub source_id: SourceId,
    /// Primary speaker: owns untagged chunks; tagged chunks attribute each turn explicitly.
    pub speaker: ActorId,
    pub reference_calendar: CalendarTable,
    pub active_concepts: Vec<CommunityConcept>,
    /// Top chunk-relevant active decisions only (bounded by `MAX_ACTIVE_DECISIONS_PER_BATCH`).
    pub active_decisions: Vec<ActiveDecisionBrief>,
    /// Top chunk-relevant active facts authored by `speaker` (bounded by `MAX_SPEAKER_FACTS_PER_BATCH`).
    #[serde(default)]
    pub speaker_own_facts: Vec<SpeakerFactBrief>,
    /// Own active facts of additional tagged speakers in multi-speaker batches (each bounded).
    #[serde(default)]
    pub speaker_own_facts_by_actor: BTreeMap<ActorId, Vec<SpeakerFactBrief>>,
    pub chunks: Vec<ExtractionChunkSlice>,
    pub min_rate_limit_remaining_pct: u8,
}

impl ExtractionBatchRequest {
    pub fn validate_request(&self) -> Result<()> {
        if self.active_decisions.len() > MAX_ACTIVE_DECISIONS_PER_BATCH {
            return Err(KpError::invalid(
                "active_decisions",
                "active_decisions must be pre-filtered to top chunk-relevant entries",
            ));
        }
        if self.speaker_own_facts.len() > MAX_SPEAKER_FACTS_PER_BATCH
            || self
                .speaker_own_facts_by_actor
                .values()
                .any(|facts| facts.len() > MAX_SPEAKER_FACTS_PER_BATCH)
        {
            return Err(KpError::invalid(
                "speaker_own_facts",
                "speaker_own_facts exceeds batch limit",
            ));
        }
        Ok(())
    }

    /// Own facts eligible for `SupersedeOwn` by `speaker` in this batch.
    pub fn own_facts_for<'a>(
        &'a self,
        speaker: &'a ActorId,
    ) -> impl Iterator<Item = &'a SpeakerFactBrief> + 'a {
        let primary = if *speaker == self.speaker {
            self.speaker_own_facts.as_slice()
        } else {
            &[]
        };
        primary.iter().chain(
            self.speaker_own_facts_by_actor
                .get(speaker)
                .into_iter()
                .flatten(),
        )
    }

    /// Human speakers appearing across target chunks (plus the primary speaker).
    pub fn batch_speakers(&self) -> std::collections::BTreeSet<ActorId> {
        let mut speakers = std::collections::BTreeSet::from([self.speaker.clone()]);
        for chunk in &self.chunks {
            if chunk.role != SliceRole::Target {
                continue;
            }
            if has_tagged_turns(&chunk.body) {
                for turn in parse_tagged_turns(&chunk.body) {
                    if let (Some(s), false) = (turn.speaker, turn.is_assistant) {
                        speakers.insert(s);
                    }
                }
            } else {
                speakers.insert(chunk.author.clone());
            }
        }
        speakers
    }

    /// Deterministically attributes a verified quote to its speaker. Authority quotes must reference
    /// a `SliceRole::Target` slice; untagged target slices belong to `slice.author`, while tagged
    /// slices require a single human `[speaker:<actor>]` turn.
    pub fn resolve_atom_speaker(
        &self,
        atom: &ModelExtractedAtom,
        verified: &VerifiedQuoteMatch,
    ) -> Result<ActorId> {
        let chunk = self
            .chunks
            .iter()
            .find(|c| c.chunk_id == atom.quote.chunk_id)
            .ok_or_else(|| KpError::invalid("chunk_id", "quote references an unknown chunk"))?;
        if chunk.role != SliceRole::Target {
            return Err(KpError::invalid(
                "chunk_id",
                "authority quote must reference a target slice, not readonly context",
            ));
        }
        if !has_tagged_turns(&chunk.body) {
            if atom.speaker.as_ref().is_some_and(|s| *s != chunk.author) {
                return Err(KpError::invalid(
                    "speaker",
                    "claimed speaker does not own this untagged chunk",
                ));
            }
            return Ok(chunk.author.clone());
        }
        let turn = parse_tagged_turns(&chunk.body)
            .into_iter()
            .find(|t| {
                t.text_span.is_some_and(|span| {
                    span.char_start <= verified.span.char_start
                        && verified.span.char_end <= span.char_end
                })
            })
            .ok_or_else(|| {
                KpError::invalid(
                    "verbatim_quote",
                    "quote must lie inside a single tagged turn's text",
                )
            })?;
        if let Some(claimed) = &atom.speaker {
            if turn.is_assistant || turn.speaker.as_ref() != Some(claimed) {
                return Err(KpError::invalid(
                    "speaker",
                    "claimed speaker differs from the speaker of the quoted turn",
                ));
            }
        }
        if turn.is_assistant {
            return Err(KpError::invalid(
                "verbatim_quote",
                "assistant turns are context only and cannot anchor a claim",
            ));
        }
        let turn_speaker = turn.speaker.ok_or_else(|| {
            KpError::invalid(
                "verbatim_quote",
                "untagged context in a tagged chunk cannot anchor a claim",
            )
        })?;
        Ok(turn_speaker)
    }

    /// Resolves the atom's validity window: model `valid_dates` are converted against
    /// `reference_calendar`; otherwise any internal `valid_window` is used as-is.
    pub fn resolve_atom_window(&self, atom: &ModelExtractedAtom) -> Result<Option<ValidityWindow>> {
        match &atom.valid_dates {
            Some(dates) => self
                .reference_calendar
                .resolve_date_range(
                    &dates.start_date_iso,
                    dates.end_date_iso.as_deref(),
                    dates.open_ended,
                )
                .map(Some),
            None => Ok(atom.valid_window),
        }
    }

    fn validate_concept_ref(
        &self,
        concept_ref: &ModelConceptRef,
        expected_kind: Option<ConceptKind>,
    ) -> Result<()> {
        match concept_ref {
            ModelConceptRef::Existing { concept_id } => {
                let Some(found) = self.active_concepts.iter().find(|c| c.id == *concept_id) else {
                    return Err(KpError::invalid(
                        "concept_id",
                        "model cited a concept_id absent from active_concepts",
                    ));
                };
                if let Some(kind) = expected_kind {
                    if found.kind != kind {
                        return Err(KpError::invalid(
                            "concept_kind",
                            "cited concept_id does not match required ConceptKind",
                        ));
                    }
                }
                Ok(())
            }
            ModelConceptRef::ProposeNew { kind, label, .. } => {
                if label.trim().is_empty() {
                    return Err(KpError::invalid(
                        "concept_label",
                        "proposed concept label must be nonempty",
                    ));
                }
                if let Some(expected) = expected_kind {
                    if *kind != expected {
                        return Err(KpError::invalid(
                            "concept_kind",
                            "proposed concept kind does not match required ConceptKind",
                        ));
                    }
                }
                Ok(())
            }
        }
    }

    fn validate_scope_ref(&self, scope_ref: &ModelScopeRef) -> Result<()> {
        match scope_ref {
            ModelScopeRef::SpeakerSelf => Ok(()),
            ModelScopeRef::NamedMentions { mentions } => {
                if mentions.is_empty() || mentions.iter().any(|m| m.trim().is_empty()) {
                    return Err(KpError::invalid(
                        "mentions",
                        "named mentions must contain at least one nonempty name",
                    ));
                }
                Ok(())
            }
            ModelScopeRef::RelationGroup { group_concept } => {
                self.validate_concept_ref(group_concept, Some(ConceptKind::Group))
            }
        }
    }

    /// Enforces all narrow model-boundary invariants and returns the verified original quote slice + span.
    pub fn validate_model_atom(&self, atom: &ModelExtractedAtom) -> Result<VerifiedQuoteMatch> {
        self.validate_model_atom_attributed(atom)
            .map(|(verified, _)| verified)
    }

    /// Like [`Self::validate_model_atom`], additionally returning the deterministically attributed
    /// speaker (see [`Self::resolve_atom_speaker`]).
    pub fn validate_model_atom_attributed(
        &self,
        atom: &ModelExtractedAtom,
    ) -> Result<(VerifiedQuoteMatch, ActorId)> {
        self.validate_model_atom_with_evidence(atom)
            .map(|(verified, _, speaker)| (verified, speaker))
    }

    /// Validates narrow-boundary structure, authority quote speaker attribution, `SupersedeOwn`
    /// ownership, and optional `evidence` quote + turn-adjacency rules.
    pub fn validate_model_atom_with_evidence(
        &self,
        atom: &ModelExtractedAtom,
    ) -> Result<(VerifiedQuoteMatch, Option<VerifiedEvidenceAnchor>, ActorId)> {
        let verified = self.validate_model_atom_structure(atom)?;
        let speaker = self.resolve_atom_speaker(atom, &verified)?;
        self.check_supersede_own(atom, &speaker)?;
        let verified_evidence = self.validate_evidence_and_adjacency(atom, &verified, &speaker)?;
        Ok((verified, verified_evidence, speaker))
    }

    fn validate_evidence_and_adjacency(
        &self,
        atom: &ModelExtractedAtom,
        verified_auth: &VerifiedQuoteMatch,
        authority_speaker: &ActorId,
    ) -> Result<Option<VerifiedEvidenceAnchor>> {
        let Some(ev_quote) = &atom.evidence else {
            return Ok(None);
        };
        if !matches!(atom.kind, FactKind::Constraint | FactKind::Decision) {
            return Err(KpError::invalid(
                "evidence",
                "evidence quote is only permitted on constraint or decision atoms",
            ));
        }
        let Some((ev_slice_idx, ev_slice)) = self
            .chunks
            .iter()
            .enumerate()
            .find(|(_, c)| c.chunk_id == ev_quote.chunk_id)
        else {
            return Err(KpError::invalid(
                "evidence",
                "evidence references a chunk_id absent from ExtractionBatchRequest",
            ));
        };
        let verified_ev =
            verify_verbatim_quote(&ev_slice.body, &ev_quote.verbatim_quote, ev_quote.hint())
                .map_err(|_| {
                    KpError::invalid(
                        "evidence",
                        "evidence quote is not a verbatim substring of its slice",
                    )
                })?;

        let auth_slice_idx = self
            .chunks
            .iter()
            .position(|c| c.chunk_id == atom.quote.chunk_id)
            .ok_or_else(|| KpError::invalid("chunk_id", "missing authority slice"))?;

        let window_turns = flatten_window_turns(&self.chunks);
        let ev_turn_idx = window_turns
            .iter()
            .position(|t| {
                t.slice_index == ev_slice_idx
                    && t.char_start <= verified_ev.span.char_start
                    && verified_ev.span.char_end <= t.char_end
            })
            .ok_or_else(|| {
                KpError::invalid(
                    "evidence",
                    "evidence quote must lie inside a single turn's text",
                )
            })?;
        let auth_turn_idx = window_turns
            .iter()
            .position(|t| {
                t.slice_index == auth_slice_idx
                    && t.char_start <= verified_auth.span.char_start
                    && verified_auth.span.char_end <= t.char_end
            })
            .ok_or_else(|| {
                KpError::invalid(
                    "verbatim_quote",
                    "authority quote must lie inside a single turn's text",
                )
            })?;

        if ev_turn_idx >= auth_turn_idx {
            return Err(KpError::invalid(
                "evidence",
                "evidence must precede authority quote in conversation order",
            ));
        }

        let ev_turn = &window_turns[ev_turn_idx];
        let auth_turn = &window_turns[auth_turn_idx];
        if is_trivial_turn_text(&verified_auth.exact_quote) {
            if ev_turn.speaker.as_ref() == Some(authority_speaker) {
                return Err(KpError::invalid(
                    "evidence",
                    "bare-ack ratification requires a distinct proposer speaker",
                ));
            }
            if is_trivial_turn_text(&ev_turn.text) {
                return Err(KpError::invalid(
                    "evidence",
                    "evidence turn cannot itself be a trivial acknowledgement",
                ));
            }
            let explicit_reply_matches = auth_turn
                .reply_to_chunk_id
                .is_some_and(|rt| rt == ev_turn.chunk_id);
            let only_acks_between = window_turns[ev_turn_idx + 1..auth_turn_idx]
                .iter()
                .all(|mid| is_trivial_turn_text(&mid.text));
            if !explicit_reply_matches && !only_acks_between {
                return Err(KpError::invalid(
                    "evidence",
                    "bare-ack authority requires explicit reply_to or only trivial acks after evidence",
                ));
            }
        } else if auth_turn_idx - ev_turn_idx > 6 {
            return Err(KpError::invalid(
                "evidence",
                "substantive ratification evidence exceeds maximum distance of 6 turns",
            ));
        }

        Ok(Some(VerifiedEvidenceAnchor {
            slice_index: ev_slice_idx,
            matched: verified_ev,
        }))
    }

    /// Verifies a `SupersedeOwn` target belongs to the attributed `speaker`'s own facts.
    pub fn check_supersede_own(&self, atom: &ModelExtractedAtom, speaker: &ActorId) -> Result<()> {
        if let ModelWriteOp::SupersedeOwn { target_fact_id } = &atom.write_op {
            if !self
                .own_facts_for(speaker)
                .any(|f| f.fact_id == *target_fact_id)
            {
                return Err(KpError::invalid(
                    "write_op",
                    "SupersedeOwn may only target a fact authored by the quoted speaker",
                ));
            }
        }
        Ok(())
    }

    /// All speaker-independent narrow-boundary checks (concepts, scopes, premises, dates, quote).
    /// Callers must still attribute the speaker and run [`Self::check_supersede_own`].
    pub fn validate_model_atom_structure(
        &self,
        atom: &ModelExtractedAtom,
    ) -> Result<VerifiedQuoteMatch> {
        if atom.kind.is_derived() {
            return Err(KpError::invalid(
                "kind",
                "model extraction may only emit base fact kinds",
            ));
        }
        if atom.claim_text.trim().is_empty() {
            return Err(KpError::invalid(
                "claim_text",
                "claim_text must be nonempty",
            ));
        }
        if let Some(window) = &atom.valid_window {
            if window.valid_from >= window.valid_until {
                return Err(KpError::invalid(
                    "valid_window",
                    "valid_window must be nonempty",
                ));
            }
        }
        self.resolve_atom_window(atom)?;
        if let Some(scope) = &atom.scope {
            self.validate_scope_ref(scope)?;
        }
        if let Some(amb) = &atom.ambiguous_scope {
            if amb.question.trim().is_empty() {
                return Err(KpError::invalid(
                    "ambiguous_scope",
                    "ambiguous scope question must be nonempty",
                ));
            }
            self.validate_scope_ref(&amb.broader_candidate)?;
        }
        if let Some(cond) = &atom.condition_concept {
            self.validate_concept_ref(cond, Some(ConceptKind::Condition))?;
        }
        if let Some(act) = &atom.action_concept {
            self.validate_concept_ref(act, Some(ConceptKind::Action))?;
        }
        if let Some(grp) = &atom.group_concept {
            self.validate_concept_ref(grp, Some(ConceptKind::Group))?;
        }
        if atom.kind == FactKind::Membership
            && (atom.group_concept.is_none()
                || atom
                    .member_mention
                    .as_deref()
                    .is_none_or(|m| m.trim().is_empty()))
        {
            return Err(KpError::invalid(
                "membership",
                "membership extraction requires group_concept and member_mention",
            ));
        }
        for premise_id in &atom.assumed_premise_ids {
            if !self
                .active_decisions
                .iter()
                .any(|d| d.fact_id == *premise_id)
            {
                return Err(KpError::invalid(
                    "assumed_premise_ids",
                    "assumed premise must reference an ID listed in active_decisions",
                ));
            }
        }
        if let ModelWriteOp::SupersedeOwn { target_fact_id } = &atom.write_op {
            let known = self
                .speaker_own_facts
                .iter()
                .any(|f| f.fact_id == *target_fact_id)
                || self
                    .speaker_own_facts_by_actor
                    .values()
                    .flatten()
                    .any(|f| f.fact_id == *target_fact_id);
            if !known {
                return Err(KpError::invalid(
                    "write_op",
                    "SupersedeOwn target is not an own fact of any batch speaker",
                ));
            }
        }
        let Some(chunk) = self
            .chunks
            .iter()
            .find(|c| c.chunk_id == atom.quote.chunk_id)
        else {
            return Err(KpError::invalid(
                "chunk_id",
                "quote references a chunk_id absent from ExtractionBatchRequest",
            ));
        };
        if chunk.role != SliceRole::Target {
            return Err(KpError::invalid(
                "chunk_id",
                "authority quote must reference a target slice, not readonly context",
            ));
        }
        verify_verbatim_quote(&chunk.body, &atom.quote.verbatim_quote, atom.quote.hint())
    }
}

struct WindowTurn<'a> {
    slice_index: usize,
    chunk_id: &'a str,
    speaker: Option<ActorId>,
    char_start: u32,
    char_end: u32,
    text: String,
    reply_to_chunk_id: Option<&'a str>,
}

fn flatten_window_turns(chunks: &[ExtractionChunkSlice]) -> Vec<WindowTurn<'_>> {
    let mut out = Vec::new();
    for (slice_index, slice) in chunks.iter().enumerate() {
        if !has_tagged_turns(&slice.body) {
            let char_end = u32::try_from(slice.body.chars().count()).unwrap_or(u32::MAX);
            out.push(WindowTurn {
                slice_index,
                chunk_id: &slice.chunk_id,
                speaker: Some(slice.author.clone()),
                char_start: 0,
                char_end,
                text: slice.body.clone(),
                reply_to_chunk_id: slice.reply_to_chunk_id.as_deref(),
            });
        } else {
            let chars: Vec<char> = slice.body.chars().collect();
            for turn in parse_tagged_turns(&slice.body) {
                if let Some(span) = turn.text_span {
                    let start = (span.char_start as usize).min(chars.len());
                    let end = (span.char_end as usize).min(chars.len());
                    let text: String = chars[start..end].iter().collect();
                    out.push(WindowTurn {
                        slice_index,
                        chunk_id: &slice.chunk_id,
                        speaker: turn.speaker,
                        char_start: span.char_start,
                        char_end: span.char_end,
                        text,
                        reply_to_chunk_id: None,
                    });
                }
            }
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ExtractionBatchOutcome {
    Completed {
        atoms: Vec<ModelExtractedAtom>,
    },
    /// Equivalent to claude-mem's `<skip_summary/>` sentinel for trivial/ack-only turns.
    SkippedEmpty,
    /// Equivalent to Codex's `min_rate_limit_remaining_percent` low-quota skip.
    SkippedLowQuota {
        remaining_pct: Option<u8>,
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntailmentQuery {
    pub left_label: String,
    pub left_domain: String,
    pub right_label: String,
    pub right_domain: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_quote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub right_quote: Option<String>,
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Unified asynchronous interface implemented by Host CLI runners, HTTP endpoints, and test doubles.
pub trait LlmClient: Send + Sync {
    /// Returns true when the client is configured under `ModelRoutePolicy::RawOnly` (zero generative
    /// model calls), so callers skip evaluations ephemerally without caching `Unrelated` verdicts.
    fn is_raw_only(&self) -> bool {
        false
    }

    /// Returns the active [`ModelRoutePolicy`] of this client so post-extraction semantic alignment
    /// enforces strict per-policy isolation (e.g. global `HostCli` never evaluates quotes belonging
    /// to `NamedEndpoint`, and distinct `NamedEndpoint`s never cross-talk).
    fn route_policy(&self) -> ModelRoutePolicy {
        if self.is_raw_only() {
            ModelRoutePolicy::RawOnly
        } else {
            ModelRoutePolicy::HostCli
        }
    }

    fn extract_batch<'a>(
        &'a self,
        request: &'a ExtractionBatchRequest,
    ) -> BoxFuture<'a, Result<ExtractionBatchOutcome>>;

    fn judge_entailment<'a>(
        &'a self,
        query: &'a EntailmentQuery,
    ) -> BoxFuture<'a, Result<EntailmentVerdict>>;

    fn complete_structured_json<'a>(
        &'a self,
        _schema_name: &'a str,
        _json_schema: &'a Value,
        _prompt: &'a str,
    ) -> BoxFuture<'a, Result<Option<Value>>> {
        Box::pin(async { Ok(None) })
    }
}

type QuoteKey = (String, String, Option<String>, Option<String>);

/// Deterministic zero-network test double for unit tests and offline CI.
#[derive(Default)]
pub struct DeterministicMockLlm {
    extractions: Mutex<BTreeMap<ContentHash, ExtractionBatchOutcome>>,
    entailments: Mutex<BTreeMap<(String, String), EntailmentVerdict>>,
    quote_entailments: Mutex<BTreeMap<QuoteKey, EntailmentVerdict>>,
    recorded_entailments: Mutex<Vec<EntailmentQuery>>,
    structured_responses: Mutex<Vec<(String, Value)>>,
}

impl DeterministicMockLlm {
    pub fn register_chunk_outcome(
        &self,
        chunk_body: &str,
        outcome: ExtractionBatchOutcome,
    ) -> Result<()> {
        let mut guard = self
            .extractions
            .lock()
            .map_err(|_| KpError::invalid("mock_llm", "poisoned lock"))?;
        guard.insert(ContentHash::digest(chunk_body), outcome);
        Ok(())
    }

    pub fn register_entailment(
        &self,
        left_label: impl Into<String>,
        right_label: impl Into<String>,
        verdict: EntailmentVerdict,
    ) -> Result<()> {
        let mut guard = self
            .entailments
            .lock()
            .map_err(|_| KpError::invalid("mock_llm", "poisoned lock"))?;
        guard.insert((left_label.into(), right_label.into()), verdict);
        Ok(())
    }

    pub fn register_entailment_with_quotes(
        &self,
        left_label: impl Into<String>,
        right_label: impl Into<String>,
        left_quote: Option<String>,
        right_quote: Option<String>,
        verdict: EntailmentVerdict,
    ) -> Result<()> {
        let mut guard = self
            .quote_entailments
            .lock()
            .map_err(|_| KpError::invalid("mock_llm", "poisoned lock"))?;
        guard.insert(
            (
                left_label.into(),
                right_label.into(),
                left_quote,
                right_quote,
            ),
            verdict,
        );
        Ok(())
    }

    pub fn recorded_entailment_queries(&self) -> Result<Vec<EntailmentQuery>> {
        self.recorded_entailments
            .lock()
            .map(|g| g.clone())
            .map_err(|_| KpError::invalid("mock_llm", "poisoned lock"))
    }

    pub fn register_structured_response(
        &self,
        prompt_contains: impl Into<String>,
        response: Value,
    ) -> Result<()> {
        self.structured_responses
            .lock()
            .map_err(|_| KpError::invalid("mock_llm", "poisoned lock"))?
            .push((prompt_contains.into(), response));
        Ok(())
    }
}

impl LlmClient for DeterministicMockLlm {
    fn extract_batch<'a>(
        &'a self,
        request: &'a ExtractionBatchRequest,
    ) -> BoxFuture<'a, Result<ExtractionBatchOutcome>> {
        Box::pin(async move {
            request.validate_request()?;
            let guard = self
                .extractions
                .lock()
                .map_err(|_| KpError::invalid("mock_llm", "poisoned lock"))?;
            let mut combined = Vec::new();
            for chunk in &request.chunks {
                if chunk.role != SliceRole::Target {
                    continue;
                }
                if let Some(outcome) = guard.get(&ContentHash::digest(&chunk.body)) {
                    match outcome {
                        ExtractionBatchOutcome::Completed { atoms } => {
                            combined.extend(atoms.iter().cloned());
                        }
                        ExtractionBatchOutcome::SkippedLowQuota {
                            remaining_pct,
                            reason,
                        } => {
                            return Ok(ExtractionBatchOutcome::SkippedLowQuota {
                                remaining_pct: *remaining_pct,
                                reason: reason.clone(),
                            });
                        }
                        ExtractionBatchOutcome::SkippedEmpty => {}
                    }
                }
            }
            if combined.is_empty() {
                Ok(ExtractionBatchOutcome::SkippedEmpty)
            } else {
                Ok(ExtractionBatchOutcome::Completed { atoms: combined })
            }
        })
    }

    fn judge_entailment<'a>(
        &'a self,
        query: &'a EntailmentQuery,
    ) -> BoxFuture<'a, Result<EntailmentVerdict>> {
        Box::pin(async move {
            self.recorded_entailments
                .lock()
                .map_err(|_| KpError::invalid("mock_llm", "poisoned lock"))?
                .push(query.clone());
            let quote_guard = self
                .quote_entailments
                .lock()
                .map_err(|_| KpError::invalid("mock_llm", "poisoned lock"))?;
            let qkey = (
                query.left_label.clone(),
                query.right_label.clone(),
                query.left_quote.clone(),
                query.right_quote.clone(),
            );
            if let Some(verdict) = quote_guard.get(&qkey) {
                return Ok(verdict.clone());
            }
            drop(quote_guard);
            let guard = self
                .entailments
                .lock()
                .map_err(|_| KpError::invalid("mock_llm", "poisoned lock"))?;
            Ok(guard
                .get(&(query.left_label.clone(), query.right_label.clone()))
                .cloned()
                .unwrap_or(EntailmentVerdict::Unrelated))
        })
    }

    fn complete_structured_json<'a>(
        &'a self,
        _schema_name: &'a str,
        _json_schema: &'a Value,
        prompt: &'a str,
    ) -> BoxFuture<'a, Result<Option<Value>>> {
        Box::pin(async move {
            let guard = self
                .structured_responses
                .lock()
                .map_err(|_| KpError::invalid("mock_llm", "poisoned lock"))?;
            for (needle, resp) in guard.iter() {
                if prompt.contains(needle.as_str()) {
                    return Ok(Some(resp.clone()));
                }
            }
            Ok(None)
        })
    }
}

#[cfg(test)]
#[path = "llm_tests.rs"]
mod tests;
