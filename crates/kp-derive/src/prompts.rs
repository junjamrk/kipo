//! Extraction & entailment prompts, JSON schemas, and 3-gram MinHash/Jaccard deduplication.
//!
//! Portions adapted under Apache-2.0 (see `THIRD_PARTY_NOTICES.md`):
//! - Graphiti (`https://github.com/getzep/graphiti`, Apache-2.0):
//!   - `graphiti_core/prompts/extract_edges.py`: relative date resolution against `reference_time`
//!     and `CalendarTable` (`valid_at` / `invalid_at`; future plans take statement `reference_time`
//!     unless a concrete event window is stated; soft deadlines never become `valid_until`).
//!   - `graphiti_core/prompts/dedupe_edges.py`: semantic contradiction vs entailment vs unrelated
//!     disambiguation.
//!   - `graphiti_core/utils/maintenance/dedup_helpers.py`: normalized 3-gram character shingles,
//!     Shannon entropy gate, MinHash/LSH banding, and `0.9` Jaccard near-duplicate threshold.
//! - mem0 (`https://github.com/mem0ai/mem0`, Apache-2.0):
//!   - `mem0/configs/prompts.py`: `ADDITIVE_EXTRACTION_PROMPT` & `DEFAULT_UPDATE_MEMORY_PROMPT`
//!     incremental write operation selection (`add` / `supersede_own` / `none`).
//! - claude-mem (`https://github.com/thedotmack/claude-mem`, `main` branch Apache-2.0):
//!   - `<skip_summary/>` sentinel for trivial/acknowledgement-only turns.

use std::collections::{BTreeMap, BTreeSet};

pub use kp_model::{is_trivial_turn_text, normalize_for_dedup};
use kp_model::{
    ActorId, CommandGuardSpec, ConceptKind, ConceptPolarity, ContentHash, EntailmentQuery,
    EntailmentVerdict, ExtractionBatchRequest, FactId, FactKind, KpError, ModelAmbiguousScope,
    ModelConceptRef, ModelDateRange, ModelExtractedAtom, ModelQuoteAnchor, ModelScopeRef,
    ModelWriteOp, SecretRedactor, SliceRole,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Sentinel adapted from `claude-mem` (`main`, Apache-2.0) indicating zero durable claims.
pub const SKIP_SUMMARY_SENTINEL: &str = "<skip_summary/>";

/// Near-duplicate 3-gram Jaccard threshold adapted from Graphiti `dedup_helpers.py` (`0.9`).
pub const JACCARD_DEDUP_THRESHOLD: f64 = 0.9;

/// Minimum character Shannon entropy required before fuzzy MinHash/Jaccard matching.
pub const MIN_SHANNON_ENTROPY: f64 = 1.2;

/// Number of deterministic hash permutations in `MinHashSignature`.
pub const MINHASH_NUM_PERM: usize = 64;

/// Structured JSON envelope emitted by `LlmClient::extract_batch` runners.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelExtractionEnvelope {
    #[serde(default)]
    pub skip_summary: bool,
    #[serde(default)]
    pub atoms: Vec<ModelExtractedAtom>,
}

/// Returns true when an entire chunk contains zero extractable human turns (either untagged
/// trivial/ack text, only `[assistant]` context lines, or human `[speaker:..]` turns that are all acks).
pub fn is_chunk_trivial_or_ack_only(body: &str) -> bool {
    if !kp_model::has_tagged_turns(body) {
        return is_trivial_turn_text(body);
    }
    let chars: Vec<char> = body.chars().collect();
    let turns = kp_model::parse_tagged_turns(body);
    let mut saw_substantive_human = false;
    for turn in turns {
        if turn.is_assistant || turn.speaker.is_none() {
            continue;
        }
        if let Some(span) = turn.text_span {
            let start = (span.char_start as usize).min(chars.len());
            let end = (span.char_end as usize).min(chars.len());
            let text: String = chars[start..end].iter().collect();
            if !is_trivial_turn_text(&text) {
                saw_substantive_human = true;
                break;
            }
        }
    }
    !saw_substantive_human
}

/// Computes Shannon entropy (bits per character) over non-whitespace characters of `text`.
pub fn shannon_entropy(text: &str) -> f64 {
    let mut counts: BTreeMap<char, usize> = BTreeMap::new();
    let mut total = 0usize;
    for ch in text.chars().filter(|c| !c.is_whitespace()) {
        *counts.entry(ch).or_insert(0) += 1;
        total += 1;
    }
    if total <= 1 {
        return 0.0;
    }
    let total_f = total as f64;
    counts.values().fold(0.0, |acc, &count| {
        let p = (count as f64) / total_f;
        acc - p * p.log2()
    })
}

/// Extracts normalized 3-character shingles (falling back to whole normalized string when `< 3` chars).
pub fn shingles_3gram(text: &str) -> BTreeSet<String> {
    let norm = normalize_for_dedup(text);
    if norm.is_empty() {
        return BTreeSet::new();
    }
    let chars: Vec<char> = norm.chars().collect();
    if chars.len() < 3 {
        return BTreeSet::from([norm]);
    }
    let mut shingles = BTreeSet::new();
    for window in chars.windows(3) {
        shingles.insert(window.iter().collect());
    }
    shingles
}

/// Computes exact 3-gram Jaccard similarity in `[0.0, 1.0]`.
pub fn jaccard_3gram(left: &str, right: &str) -> f64 {
    let a = shingles_3gram(left);
    let b = shingles_3gram(right);
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(&b).count() as f64;
    let union = a.union(&b).count() as f64;
    inter / union
}

/// Deterministic 64-permutation MinHash signature over 3-gram character shingles (zero `unsafe`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MinHashSignature {
    values: [u64; MINHASH_NUM_PERM],
}

impl MinHashSignature {
    pub fn from_text(text: &str) -> Self {
        let shingles = shingles_3gram(text);
        let mut values = [u64::MAX; MINHASH_NUM_PERM];
        if shingles.is_empty() {
            return Self { values };
        }
        for shingle in &shingles {
            let hex_digest = ContentHash::digest(shingle.as_bytes());
            let hex_str = hex_digest.as_str();
            let h1 = u64::from_str_radix(&hex_str[0..16], 16).unwrap_or(0);
            let h2 = u64::from_str_radix(&hex_str[16..32], 16).unwrap_or(1) | 1;
            for (idx, slot) in values.iter_mut().enumerate() {
                let idx_u64 = idx as u64;
                let mixed = h1
                    .wrapping_add(idx_u64.wrapping_mul(h2))
                    .wrapping_add(idx_u64.wrapping_mul(0x9E37_79B9_7F4A_7C15));
                let perm_hash = splitmix64(mixed);
                if perm_hash < *slot {
                    *slot = perm_hash;
                }
            }
        }
        Self { values }
    }

    pub fn estimate_jaccard(&self, other: &Self) -> f64 {
        let equal = self
            .values
            .iter()
            .zip(other.values.iter())
            .filter(|(a, b)| a == b)
            .count();
        (equal as f64) / (MINHASH_NUM_PERM as f64)
    }

    /// Partitions the 64-value signature into 16 LSH bands of 4 rows each.
    pub fn lsh_bands(&self) -> [u64; 16] {
        let mut bands = [0u64; 16];
        for (band_idx, band_slot) in bands.iter_mut().enumerate() {
            let base = band_idx * 4;
            let mut acc = 0x517C_C1B7_2722_0A95_u64 ^ (band_idx as u64);
            for offset in 0..4 {
                acc = splitmix64(acc ^ self.values[base + offset]);
            }
            *band_slot = acc;
        }
        bands
    }

    pub fn shares_lsh_band(&self, other: &Self) -> bool {
        let left = self.lsh_bands();
        let right = other.lsh_bands();
        left.iter().zip(right.iter()).any(|(a, b)| a == b)
    }
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Graphiti-style near-duplicate detector combining normalization, Shannon entropy gate,
/// MinHash LSH candidate filtering, and exact `0.9` 3-gram Jaccard verification.
pub fn is_near_duplicate(left: &str, right: &str) -> bool {
    let norm_l = normalize_for_dedup(left);
    let norm_r = normalize_for_dedup(right);
    if norm_l.is_empty() || norm_r.is_empty() {
        return false;
    }
    if norm_l == norm_r {
        return true;
    }
    if norm_l.chars().count() < 3
        || norm_r.chars().count() < 3
        || shannon_entropy(&norm_l) < MIN_SHANNON_ENTROPY
        || shannon_entropy(&norm_r) < MIN_SHANNON_ENTROPY
    {
        return false;
    }
    let sig_l = MinHashSignature::from_text(&norm_l);
    let sig_r = MinHashSignature::from_text(&norm_r);
    if !sig_l.shares_lsh_band(&sig_r) {
        return false;
    }
    jaccard_3gram(&norm_l, &norm_r) >= JACCARD_DEDUP_THRESHOLD
}

/// Wraps `schema` as nullable for OpenAI strict Structured Outputs (`anyOf: [schema, null]`).
fn strict_nullable(schema: Value) -> Value {
    json!({ "anyOf": [schema, { "type": "null" }] })
}

fn strict_nullable_described(schema: Value, description: &str) -> Value {
    json!({
        "description": description,
        "anyOf": [schema, { "type": "null" }]
    })
}

/// Builds a closed object schema listing EVERY property in `required` with `additionalProperties: false`,
/// as mandated by OpenAI strict Structured Outputs (`codex exec --output-schema`) and Claude `--json-schema`.
fn strict_object(props: Vec<(&str, Value)>) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::with_capacity(props.len());
    for (key, schema) in props {
        required.push(Value::String(key.to_owned()));
        properties.insert(key.to_owned(), schema);
    }
    json!({
        "type": "object",
        "properties": Value::Object(properties),
        "required": Value::Array(required),
        "additionalProperties": false
    })
}

fn strict_enum(values: &[&str]) -> Value {
    json!({ "type": "string", "enum": values })
}

fn strict_concept_ref_schema() -> Value {
    strict_object(vec![
        ("ref_type", strict_enum(&["existing", "propose_new"])),
        ("concept_id", json!({ "type": ["string", "null"] })),
        (
            "kind",
            strict_nullable(strict_enum(&["condition", "action", "group", "topic"])),
        ),
        ("label", json!({ "type": ["string", "null"] })),
        ("domain", json!({ "type": ["string", "null"] })),
        (
            "polarity",
            strict_nullable(strict_enum(&[
                "positive", "negative", "neutral", "resolved",
            ])),
        ),
    ])
}

fn strict_scope_ref_schema() -> Value {
    strict_object(vec![
        (
            "scope",
            strict_enum(&[
                "speaker_self",
                "named_mentions",
                "relation_group",
                "resource",
            ]),
        ),
        (
            "mentions",
            strict_nullable(json!({ "type": "array", "items": { "type": "string" } })),
        ),
        (
            "group_concept",
            strict_nullable(strict_concept_ref_schema()),
        ),
        ("target", json!({ "type": ["string", "null"] })),
    ])
}

fn strict_command_guard_schema() -> Value {
    strict_object(vec![
        (
            "forbidden_commands",
            json!({
                "type": "array",
                "items": { "type": "string" },
                "description": "Banned CLI command prefixes (e.g. [\"pip install\", \"git push --force\"])"
            }),
        ),
        (
            "allowed_commands",
            json!({
                "type": "array",
                "items": { "type": "string" },
                "description": "Permitted or replacement CLI command prefixes (e.g. [\"uv pip install\"])"
            }),
        ),
        (
            "required_flags",
            json!({
                "type": "array",
                "items": { "type": "string" },
                "description": "Mandatory CLI flags required when running a guarded command (e.g. [\"--schema\", \"--locked\"])"
            }),
        ),
    ])
}

/// Strict JSON Schema (OpenAI Structured Outputs strict mode compatible: root object, no `oneOf`,
/// no untyped `{}` nodes, `additionalProperties: false` and `required == properties` on every object)
/// for the extraction envelope passed to `claude -p --output-format json --json-schema` and
/// `codex exec --output-schema`. Decode replies with [`parse_extraction_envelope`].
pub fn extraction_batch_json_schema() -> Value {
    let atom = strict_object(vec![
        (
            "kind",
            json!({
                "type": "string",
                "enum": [
                    "observation",
                    "fact_slot",
                    "event",
                    "constraint",
                    "decision",
                    "membership",
                    "closed_negative"
                ],
                "description": "Use \"constraint\" not only for prohibitions, but ALSO for habitual team practices, routines, conventions, and defaults (\"遇到 X 的时候都 Y\", \"一般不……\", \"从不……\", \"默认……\")."
            }),
        ),
        ("claim_text", json!({ "type": "string" })),
        (
            "value_json",
            json!({
                "type": "string",
                "description": "JSON-encoded structured value of the claim, e.g. \"{\\\"room\\\":\\\"B2\\\"}\"; use \"null\" when none"
            }),
        ),
        ("scope", strict_nullable(strict_scope_ref_schema())),
        (
            "ambiguous_scope",
            strict_nullable(strict_object(vec![
                ("broader_candidate", strict_scope_ref_schema()),
                ("question", json!({ "type": "string" })),
            ])),
        ),
        (
            "condition_concept",
            strict_nullable_described(
                strict_concept_ref_schema(),
                "MANDATORY non-null (kind=\"condition\") for EVERY kind=\"event\" (even when active_concepts is empty — use propose_new!) and for conditional kind=\"constraint\": name WHAT happened or the triggering condition (e.g. \"园区停电\" -> \"停电\", NEVER a location like \"园区\" or a time/day like \"周四\").",
            ),
        ),
        (
            "action_concept",
            strict_nullable(strict_concept_ref_schema()),
        ),
        (
            "group_concept",
            strict_nullable(strict_concept_ref_schema()),
        ),
        ("member_mention", json!({ "type": ["string", "null"] })),
        (
            "valid_dates",
            strict_nullable(strict_object(vec![
                (
                    "start_date_iso",
                    json!({
                        "type": "string",
                        "description": "YYYY-MM-DD (or a CalendarTable label) of the first day"
                    }),
                ),
                ("end_date_iso", json!({ "type": ["string", "null"] })),
                ("open_ended", json!({ "type": "boolean" })),
            ])),
        ),
        (
            "speaker",
            json!({
                "type": ["string", "null"],
                "description": "actor id from the [speaker:<actor_id>] tag of the quoted turn"
            }),
        ),
        (
            "assumed_premise_ids",
            json!({ "type": "array", "items": { "type": "string" } }),
        ),
        (
            "quote",
            strict_object(vec![
                ("chunk_id", json!({ "type": "string" })),
                (
                    "verbatim_quote",
                    json!({
                        "type": "string",
                        "description": "Exact contiguous substring from inside a target <chunk> human turn's text, excluding [turn:N], [speaker:...], and [assistant] prefixes"
                    }),
                ),
                (
                    "turn_ordinal",
                    json!({
                        "type": ["integer", "null"],
                        "description": "REQUIRED 0-based turn index N matching the [turn:N] prefix of the quoted line/turn in <chunk>; every extracted item MUST fill this exact integer N"
                    }),
                ),
                ("approx_char_offset", json!({ "type": ["integer", "null"] })),
            ]),
        ),
        (
            "evidence",
            strict_nullable_described(
                strict_object(vec![
                    ("chunk_id", json!({ "type": "string" })),
                    (
                        "verbatim_quote",
                        json!({
                            "type": "string",
                            "description": "Exact contiguous substring from an earlier turn or <context_chunk> holding the ratified proposal content"
                        }),
                    ),
                    (
                        "turn_ordinal",
                        json!({
                            "type": ["integer", "null"],
                            "description": "0-based turn index N matching [turn:N] in the evidence chunk"
                        }),
                    ),
                    ("approx_char_offset", json!({ "type": ["integer", "null"] })),
                ]),
                "Optional content quote ONLY on kind=\"decision\" or kind=\"constraint\" when `quote` ratifies an earlier proposal; null for self-contained statements.",
            ),
        ),
        (
            "command_guard",
            strict_nullable_described(
                strict_command_guard_schema(),
                "Populate ONLY on kind=\"constraint\" or kind=\"decision\" when the rule specifies banned CLI commands (`forbidden_commands`), replacement/allowed CLI commands (`allowed_commands`), or mandatory CLI flags (`required_flags`, e.g. [\"--schema\"], [\"--locked\"]); null otherwise.",
            ),
        ),
        (
            "write_op",
            strict_object(vec![
                ("op", strict_enum(&["add", "supersede_own", "none"])),
                ("target_fact_id", json!({ "type": ["string", "null"] })),
            ]),
        ),
    ]);
    strict_object(vec![
        ("skip_summary", json!({ "type": "boolean" })),
        ("atoms", json!({ "type": "array", "items": atom })),
    ])
}

/// Strict JSON Schema for the 4-way entailment verdict (single root object, `question` nullable).
/// Decode replies with [`parse_entailment_verdict`].
pub fn entailment_verdict_json_schema() -> Value {
    strict_object(vec![
        (
            "verdict",
            strict_enum(&["entails", "contradicts", "unrelated", "ambiguous"]),
        ),
        ("question", json!({ "type": ["string", "null"] })),
    ])
}

/// Strict JSON Schema for one-shot sleep-time `CommandGuardSpec` backfill over legacy prose rules.
pub fn command_guard_backfill_json_schema() -> Value {
    strict_object(vec![(
        "command_guard",
        strict_nullable(strict_command_guard_schema()),
    )])
}

pub fn build_command_guard_backfill_prompt(
    subject: &str,
    claim_or_summary: &str,
    verbatim_quote: Option<&str>,
    redactor: &SecretRedactor,
) -> String {
    let payload = json!({
        "subject": redactor.redact(subject.trim()),
        "claim": redactor.redact(claim_or_summary.trim()),
        "verbatim_quote": verbatim_quote.map(|q| redactor.redact(q.trim())),
    });
    format!(
        "You are `kp`'s write-time CLI command guard classifier.\n\
Determine whether the following human-authored constraint or decision specifies concrete CLI command restrictions.\n\
- Populate `command_guard = {{\"forbidden_commands\": [...], \"allowed_commands\": [...], \"required_flags\": [...]}}` ONLY when the rule explicitly bans CLI commands (`forbidden_commands`, e.g. `[\"pip install\"]`), names permitted replacement commands (`allowed_commands`, e.g. `[\"uv pip install\"]`), or mandates CLI flags (`required_flags`, e.g. `[\"--schema\"]`).\n\
- When a command is prohibited ONLY when missing a mandatory flag, include the command in `forbidden_commands` AND the flag in `required_flags`.\n\
- If the rule is a general habit, schedule, architecture note, or positive encouragement (`\"always run cargo clippy\"`) with no banned command or mandatory flag gate, return `{{\"command_guard\": null}}`.\n\
GUARD_BACKFILL_JSON: {payload}"
    )
}

pub fn parse_command_guard_backfill_reply(
    value: Value,
) -> kp_model::Result<Option<CommandGuardSpec>> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct WireBackfillEnvelope {
        command_guard: Option<WireCommandGuard>,
    }
    let env: WireBackfillEnvelope = serde_json::from_value(value)
        .map_err(|_| KpError::invalid("guard_backfill_json", "invalid backfill envelope"))?;
    Ok(env.command_guard.map(|cg| {
        CommandGuardSpec {
            forbidden_commands: cg.forbidden_commands,
            allowed_commands: cg.allowed_commands,
            required_flags: cg.required_flags,
        }
        .normalized()
    }))
}

/// Default review question used when a model returns `ambiguous` without a usable question.
pub const DEFAULT_AMBIGUOUS_QUESTION: &str =
    "这两个说法是否属于同一触发条件？请在 kp review 中确认。";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireConceptRef {
    ref_type: String,
    concept_id: Option<String>,
    kind: Option<ConceptKind>,
    label: Option<String>,
    domain: Option<String>,
    polarity: Option<ConceptPolarity>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireScopeRef {
    scope: String,
    #[serde(default)]
    mentions: Option<Vec<String>>,
    #[serde(default)]
    group_concept: Option<WireConceptRef>,
    #[serde(default)]
    target: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireCommandGuard {
    #[serde(default)]
    forbidden_commands: Vec<String>,
    #[serde(default)]
    allowed_commands: Vec<String>,
    #[serde(default)]
    required_flags: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireAmbiguousScope {
    broader_candidate: WireScopeRef,
    question: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireDates {
    start_date_iso: String,
    end_date_iso: Option<String>,
    open_ended: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireQuote {
    chunk_id: String,
    verbatim_quote: String,
    turn_ordinal: Option<u32>,
    approx_char_offset: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireWriteOp {
    op: String,
    target_fact_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireAtom {
    kind: FactKind,
    claim_text: String,
    value_json: String,
    scope: Option<WireScopeRef>,
    ambiguous_scope: Option<WireAmbiguousScope>,
    condition_concept: Option<WireConceptRef>,
    action_concept: Option<WireConceptRef>,
    group_concept: Option<WireConceptRef>,
    member_mention: Option<String>,
    valid_dates: Option<WireDates>,
    speaker: Option<String>,
    #[serde(default)]
    assumed_premise_ids: Vec<String>,
    quote: WireQuote,
    #[serde(default)]
    evidence: Option<WireQuote>,
    #[serde(default)]
    command_guard: Option<WireCommandGuard>,
    write_op: WireWriteOp,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireEnvelope {
    skip_summary: bool,
    atoms: Vec<WireAtom>,
}

#[derive(Deserialize)]
struct WireVerdict {
    verdict: String,
    #[serde(default)]
    question: Option<String>,
}

fn wire_concept(wire: WireConceptRef) -> kp_model::Result<ModelConceptRef> {
    match wire.ref_type.as_str() {
        "existing" => {
            let raw = wire.concept_id.ok_or_else(|| {
                KpError::invalid("concept_ref", "existing concept ref requires concept_id")
            })?;
            Ok(ModelConceptRef::Existing {
                concept_id: raw.trim().parse()?,
            })
        }
        "propose_new" => {
            let kind = wire.kind.ok_or_else(|| {
                KpError::invalid("concept_ref", "propose_new concept ref requires kind")
            })?;
            let label = wire.label.ok_or_else(|| {
                KpError::invalid("concept_ref", "propose_new concept ref requires label")
            })?;
            Ok(ModelConceptRef::ProposeNew {
                kind,
                label,
                domain: wire.domain.unwrap_or_default(),
                polarity: wire.polarity.unwrap_or(ConceptPolarity::Neutral),
            })
        }
        _ => Err(KpError::invalid("concept_ref", "unknown ref_type")),
    }
}

fn wire_scope(wire: WireScopeRef) -> kp_model::Result<ModelScopeRef> {
    wire_scope_inner(wire, false)
}

fn wire_scope_inner(
    wire: WireScopeRef,
    default_missing_group_for_ambiguous: bool,
) -> kp_model::Result<ModelScopeRef> {
    match wire.scope.as_str() {
        "speaker_self" => Ok(ModelScopeRef::SpeakerSelf),
        "named_mentions" => Ok(ModelScopeRef::NamedMentions {
            mentions: wire.mentions.ok_or_else(|| {
                KpError::invalid("scope", "named_mentions scope requires mentions")
            })?,
        }),
        "relation_group" => {
            let group_concept = match wire.group_concept {
                Some(gc) => wire_concept(gc)?,
                None if default_missing_group_for_ambiguous => ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Group,
                    label: "当前团队".into(),
                    domain: "team".into(),
                    polarity: ConceptPolarity::Neutral,
                },
                None => {
                    return Err(KpError::invalid(
                        "scope",
                        "relation_group scope requires group_concept",
                    ))
                }
            };
            Ok(ModelScopeRef::RelationGroup { group_concept })
        }
        "resource" => {
            let target = wire
                .target
                .map(|t| t.trim().to_owned())
                .filter(|t| !t.is_empty())
                .ok_or_else(|| KpError::invalid("scope", "resource scope requires target"))?;
            Ok(ModelScopeRef::Resource { target })
        }
        _ => Err(KpError::invalid("scope", "unknown scope variant")),
    }
}

fn wire_atom(wire: WireAtom) -> kp_model::Result<ModelExtractedAtom> {
    let raw_value = wire.value_json.trim();
    let value = if raw_value.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(raw_value).unwrap_or_else(|_| Value::String(raw_value.to_owned()))
    };
    let write_op = match wire.write_op.op.as_str() {
        "add" => ModelWriteOp::Add,
        "none" => ModelWriteOp::None,
        "supersede_own" => ModelWriteOp::SupersedeOwn {
            target_fact_id: wire
                .write_op
                .target_fact_id
                .ok_or_else(|| {
                    KpError::invalid("write_op", "supersede_own requires target_fact_id")
                })?
                .trim()
                .parse()?,
        },
        _ => return Err(KpError::invalid("write_op", "unknown write op")),
    };
    let assumed_premise_ids = wire
        .assumed_premise_ids
        .iter()
        .map(|id| id.trim().parse::<FactId>())
        .collect::<kp_model::Result<Vec<_>>>()?;
    Ok(ModelExtractedAtom {
        kind: wire.kind,
        claim_text: wire.claim_text,
        value,
        scope: wire.scope.map(wire_scope).transpose()?,
        ambiguous_scope: wire
            .ambiguous_scope
            .map(|amb| {
                Ok::<_, KpError>(ModelAmbiguousScope {
                    broader_candidate: wire_scope_inner(amb.broader_candidate, true)?,
                    question: amb.question,
                })
            })
            .transpose()?,
        condition_concept: wire.condition_concept.map(wire_concept).transpose()?,
        action_concept: wire.action_concept.map(wire_concept).transpose()?,
        group_concept: wire.group_concept.map(wire_concept).transpose()?,
        member_mention: wire.member_mention,
        // The model never emits epoch milliseconds; `kp` resolves `valid_dates` via `CalendarTable`.
        valid_window: None,
        valid_dates: wire.valid_dates.map(|d| ModelDateRange {
            start_date_iso: d.start_date_iso,
            end_date_iso: d.end_date_iso,
            open_ended: d.open_ended,
        }),
        speaker: wire
            .speaker
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .map(|s| s.parse::<ActorId>())
            .transpose()?,
        assumed_premise_ids,
        quote: ModelQuoteAnchor {
            chunk_id: wire.quote.chunk_id,
            verbatim_quote: wire.quote.verbatim_quote,
            turn_ordinal: wire.quote.turn_ordinal,
            approx_char_offset: wire.quote.approx_char_offset,
        },
        evidence: wire.evidence.map(|ev| ModelQuoteAnchor {
            chunk_id: ev.chunk_id,
            verbatim_quote: ev.verbatim_quote,
            turn_ordinal: ev.turn_ordinal,
            approx_char_offset: ev.approx_char_offset,
        }),
        command_guard: wire.command_guard.map(|cg| {
            CommandGuardSpec {
                forbidden_commands: cg.forbidden_commands,
                allowed_commands: cg.allowed_commands,
                required_flags: cg.required_flags,
            }
            .normalized()
        }),
        write_op,
    })
}

/// Decodes a model reply produced under [`extraction_batch_json_schema`] (strict flat wire shape with
/// nullable keys, JSON-encoded `value_json`, and calendar `valid_dates`) into the narrow-boundary
/// [`ModelExtractionEnvelope`]. Replies in any other shape (e.g. carrying epoch `valid_window`) fail.
pub fn parse_extraction_envelope(value: Value) -> kp_model::Result<ModelExtractionEnvelope> {
    let wire: WireEnvelope = serde_json::from_value(value)
        .map_err(|_| KpError::invalid("host_cli_json", "invalid extraction envelope"))?;
    Ok(ModelExtractionEnvelope {
        skip_summary: wire.skip_summary,
        atoms: wire
            .atoms
            .into_iter()
            .map(wire_atom)
            .collect::<kp_model::Result<Vec<_>>>()?,
    })
}

/// Decodes a model reply produced under [`entailment_verdict_json_schema`], normalizing an `ambiguous`
/// verdict with a null/blank `question` to [`DEFAULT_AMBIGUOUS_QUESTION`].
pub fn parse_entailment_verdict(value: Value) -> kp_model::Result<EntailmentVerdict> {
    let wire: WireVerdict = serde_json::from_value(value)
        .map_err(|_| KpError::invalid("host_cli_json", "invalid entailment verdict"))?;
    match wire.verdict.trim() {
        "entails" => Ok(EntailmentVerdict::Entails),
        "contradicts" => Ok(EntailmentVerdict::Contradicts),
        "unrelated" => Ok(EntailmentVerdict::Unrelated),
        "ambiguous" => {
            let question = wire
                .question
                .map(|q| q.trim().to_owned())
                .filter(|q| !q.is_empty())
                .unwrap_or_else(|| DEFAULT_AMBIGUOUS_QUESTION.to_owned());
            Ok(EntailmentVerdict::Ambiguous { question })
        }
        _ => Err(KpError::invalid(
            "host_cli_json",
            "unknown entailment verdict",
        )),
    }
}

fn render_chunk_with_turn_ordinals(raw_body: &str, redactor: &SecretRedactor) -> String {
    let delimiter = if raw_body.contains("\n\n") {
        "\n\n"
    } else {
        "\n"
    };
    let mut rendered = Vec::new();
    let mut turn_idx = 0usize;
    for part in raw_body.split(delimiter) {
        if part.trim().is_empty() {
            continue;
        }
        let safe_part = redactor.redact(part);
        rendered.push(format!("[turn:{turn_idx}] {safe_part}"));
        turn_idx += 1;
    }
    rendered.join(delimiter)
}

/// Builds the narrow-boundary extraction prompt combining Graphiti temporal rules,
/// mem0 `Add / SupersedeOwn / None` merge semantics, and `claude-mem` `<skip_summary/>` gating.
///
/// The prompt never shows epoch milliseconds: the model answers with calendar dates
/// (`valid_dates`), which `kp` converts deterministically via `CalendarTable`.
pub fn build_extraction_prompt(
    request: &ExtractionBatchRequest,
    redactor: &SecretRedactor,
) -> String {
    let mut calendar_lines = String::new();
    for anchor in &request.reference_calendar.anchors {
        calendar_lines.push_str(&format!(
            "  - {} => {} (weekday_iso={})\n",
            anchor.label, anchor.date_iso, anchor.weekday_iso
        ));
    }

    let concepts_json =
        serde_json::to_string(&request.active_concepts).unwrap_or_else(|_| "[]".into());
    let decisions_val = serde_json::to_value(&request.active_decisions).unwrap_or(Value::Null);
    let decisions_json = serde_json::to_string(&redactor.redact_json(&decisions_val))
        .unwrap_or_else(|_| "[]".into());
    let mut own_facts_by_speaker = serde_json::Map::new();
    for speaker in request.batch_speakers() {
        let facts: Vec<_> = request.own_facts_for(&speaker).cloned().collect();
        if facts.is_empty() {
            continue;
        }
        let val = serde_json::to_value(&facts).unwrap_or(Value::Null);
        own_facts_by_speaker.insert(speaker.to_string(), redactor.redact_json(&val));
    }
    let own_facts_json =
        serde_json::to_string(&Value::Object(own_facts_by_speaker)).unwrap_or_else(|_| "{}".into());

    let mut context_section = String::new();
    let mut chunks_section = String::new();
    for chunk in &request.chunks {
        let safe_heading = redactor.redact(chunk.heading.trim());
        let safe_body = render_chunk_with_turn_ordinals(&chunk.body, redactor);
        let reply_attr = chunk
            .reply_to_chunk_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|rt| format!(" reply_to=\"{}\"", redactor.redact(rt)))
            .unwrap_or_default();
        let heading_attr = if safe_heading.is_empty() {
            String::new()
        } else {
            format!(" heading=\"{safe_heading}\"")
        };
        let section_banner = if safe_heading.is_empty() {
            String::new()
        } else {
            format!("[section_metadata (non-quotable): {safe_heading}]\n")
        };
        match chunk.role {
            SliceRole::Context => {
                context_section.push_str(&format!(
                    "<context_chunk id=\"{}\" author=\"{}\"{}{}>\n{}{}\n</context_chunk>\n",
                    chunk.chunk_id,
                    chunk.author,
                    reply_attr,
                    heading_attr,
                    section_banner,
                    safe_body
                ));
            }
            SliceRole::Target => {
                chunks_section.push_str(&format!(
                    "<chunk id=\"{}\" author=\"{}\"{}{}>\n{}{}\n</chunk>\n",
                    chunk.chunk_id,
                    chunk.author,
                    reply_attr,
                    heading_attr,
                    section_banner,
                    safe_body
                ));
            }
        }
    }
    let context_wrapper = if context_section.is_empty() {
        String::new()
    } else {
        format!(
            "## 5a. Prior Context (`<context_readonly>` — NEVER use as `quote` authority!)\n\
             <context_readonly>\n\
             {context_section}\
             </context_readonly>\n\n"
        )
    };

    format!(
        "You are `kp`'s out-of-session memory extraction engine.\n\
Extract ONLY durable facts, events, constraints, decisions, memberships, and proven pitfalls stated or ratified by HUMAN speakers.\n\
\n\
## 0. Speakers, Turns, Dual-Anchor Ratification & 5-Way Speech-Act Gate\n\
- Each line/turn inside `<chunk>` or `<context_chunk>` is prefixed with its 0-based index `[turn:N]` (`N = 0, 1, 2, ...`).\n\
- `[section_metadata (non-quotable): ...]` headers provide section/table context ONLY and are NOT part of any turn's quotable text.\n\
- `<context_readonly>` (`<context_chunk>`) contains already-processed prior turns for coreference, pronoun/entity resolution, and `evidence` quotes ONLY — `quote` (authority) MUST ALWAYS reference a target `<chunk>`, NEVER a `<context_chunk>`!\n\
- Lines tagged `[speaker:<actor_id>]` are human turns; lines tagged `[assistant]` are AI assistant output and can NEVER carry `quote` authority.\n\
- Set `speaker` to the `<actor_id>` of the quoted turn (or the target `<chunk>`'s `author`, or null on untagged chunks owned by primary speaker `{speaker}`). `quote.turn_ordinal` MUST equal `N` from `[turn:N]`, and `verbatim_quote` MUST exclude `[turn:N]`, `[speaker:...]`, `[assistant]`, and `[section_metadata ...]`.\n\
- Dual-anchor ratification (`quote` + `evidence`, ONLY allowed on `kind = \"decision\"` or `kind = \"constraint\"`):\n\
  * Self-contained human statements set `evidence = null` and `quote` to the human's verbatim words.\n\
  * When an earlier turn (`[assistant]` or another speaker `A`, in `<context_chunk>` or an earlier `<chunk>` turn) proposes a concrete rule/decision and human speaker `B` (`B != A`) ratifies it in a target `<chunk>` (even with a bare agreement like `\"lgtm\"`, `\"同意\"`, `\"可以\"`, `\"yes\"`, or via `reply_to`), set `evidence` to the verbatim proposal in `A`'s / `[assistant]`'s turn and set `quote` to `B`'s ratifying words (`speaker = B`).\n\
  * NEVER pair a bare acknowledgement (`\"收到\"`, `\"好的\"`, `\"ok\"`, `\"lgtm\"`, `\"+1\"`) with a prior proposal if a non-ack question or counter-comment intervened between them without an explicit `reply_to`.\n\
  * Standalone bare acknowledgements that do NOT ratify a concrete proposal MUST NOT be extracted.\n\
- 5-Way Speech-Act Gate:\n\
  1. Open questions (`\"Should we switch to Postgres?\"`, `\"what about tests?\"`) without ratification => DROP.\n\
  2. Jokes, sarcasm, hypotheticals, and banter => DROP.\n\
  3. Unratified brainstorms or tentative `[assistant]` / human suggestions (`\"Maybe we could...\"`) => DROP.\n\
  4. Unmerged PR nitpicks or one-off code review remarks not ratified as standing team rules => DROP.\n\
  5. Solo normative opinions with unclear team authority => extract narrowly with `scope = speaker_self` and `ambiguous_scope` (`broader_candidate`); ratified proposals => extract as `decision` or `constraint` with `evidence` + `quote`.\n\
\n\
## 1. Empty / Trivial Turn Sentinel (adapted from claude-mem Apache-2.0)\n\
If target `<chunk>` blocks contain only greetings, standalone acknowledgements with nothing to ratify, open questions, or ephemeral chatter with zero durable memory, return `{{\"skip_summary\": true, \"atoms\": []}}`.\n\
\n\
## 2. Temporal Resolution Rules (adapted from Graphiti `extract_edges.py` Apache-2.0)\n\
Statement date: `{ref_y:04}-{ref_m:02}-{ref_d:02}` (weekday_iso={ref_wd}, utc_offset_minutes={tz}).\n\
`CalendarTable` (label => date):\n\
{calendar_lines}\
- Resolve relative dates (today, tomorrow, this/next weekday, weekend) ONLY by looking up the date in `CalendarTable`. NEVER output timestamps or epoch numbers.\n\
- `valid_dates = {{\"start_date_iso\": \"YYYY-MM-DD\", \"end_date_iso\": \"YYYY-MM-DD\" | null, \"open_ended\": true|false}}`.\n\
- Scheduled events (`kind = \"event\"`, e.g. \"the staging cluster is down for maintenance next Tuesday\") set `start_date_iso` to the event day; a multi-day event also sets `end_date_iso`; \"starting from <day>\" with no end sets `open_ended = true`.\n\
- Standing rules, decisions, and memberships (`constraint`, `decision`, `membership`) with no stated dates use `valid_dates = null` (they apply from the statement date on). A soft deadline is NEVER an end date.\n\
\n\
## 3. Incremental Write Operations (adapted from mem0 `prompts.py` Apache-2.0)\n\
Each speaker's own active facts (by actor id): {own_facts_json}\n\
- Use `{{\"op\": \"add\", \"target_fact_id\": null}}` for genuinely new claims.\n\
- Use `{{\"op\": \"supersede_own\", \"target_fact_id\": \"<fact_id>\"}}` ONLY when the quoted speaker updates, corrects, or reverses one of THEIR OWN facts listed above.\n\
- Use `{{\"op\": \"none\", \"target_fact_id\": null}}` if already captured unchanged.\n\
- NEVER invent IDs and NEVER supersede another speaker's fact.\n\
\n\
## 4. Narrow Boundary, Open Concepts & Governed Scope\n\
Active concepts (`active_concepts`): {concepts_json}\n\
Active decisions (`active_decisions`): {decisions_json}\n\
- Concept references: cite `{{\"ref_type\": \"existing\", \"concept_id\": \"...\", \"kind\": null, \"label\": null, \"domain\": null, \"polarity\": null}}` ONLY for IDs in `active_concepts` of matching `ConceptKind`; otherwise emit `{{\"ref_type\": \"propose_new\", \"concept_id\": null, \"kind\": \"condition|action|group|topic\", \"label\": \"...\", \"domain\": \"...\", \"polarity\": \"positive|negative|neutral|resolved\"}}`.\n\
- Events (`kind = \"event\"`): EVERY `kind = \"event\"` MUST populate a non-null `condition_concept` (`kind = \"condition\"`), even when `active_concepts` is empty (use `propose_new`)! `condition_concept.label` MUST name **what happened / the resulting condition** (e.g. `\"园区停电\"` -> `\"停电\"`, NEVER a location like `\"园区\"` or a time/weekday like `\"周四\"`).\n\
- Constraints & habitual norms (`kind = \"constraint\"`): Habitual team practices, routines, conventions, and standing defaults (`\"遇到 X 的时候都 Y\"`, `\"一般不……\"`, `\"从不……\"`, `\"默认……\"`) are `kind = \"constraint\"` just like prohibitions — populate `condition_concept` (`kind = \"condition\"`, `X`) and `action_concept` (`kind = \"action\"`, `Y`).\n\
- Scope rules (every scope object has keys `scope`, `mentions`, `group_concept`, `target`; unused keys are null):\n\
  * Explicitly speaker-only (\"I personally never book red-eye flights\") => `scope = {{\"scope\": \"speaker_self\", \"mentions\": null, \"group_concept\": null, \"target\": null}}`, `ambiguous_scope = null`.\n\
  * Explicit group (\"our team requires two approvals for every code review\") => `scope = {{\"scope\": \"relation_group\", \"mentions\": null, \"group_concept\": <concept ref of kind group>, \"target\": null}}`, `ambiguous_scope = null`.\n\
  * Explicit named people (\"Dave should not take night shifts next week\") => `scope = {{\"scope\": \"named_mentions\", \"mentions\": [\"Dave\"], \"group_concept\": null, \"target\": null}}`, `ambiguous_scope = null`.\n\
  * Protected file, directory, or resource target (\"do not edit src/lib.rs without review\") => `scope = {{\"scope\": \"resource\", \"mentions\": null, \"group_concept\": null, \"target\": \"src/lib.rs\"}}`, `ambiguous_scope = null`.\n\
  * Subjectless / unclear who it governs (\"no overtime on weekends\") => `scope = {{\"scope\": \"speaker_self\", \"mentions\": null, \"group_concept\": null, \"target\": null}}` AND `ambiguous_scope = {{\"broader_candidate\": {{\"scope\": \"relation_group\", ...}}, \"question\": \"<ask whether it applies only to the speaker or to the whole group>\"}}`.\n\
- CLI Command Guards (`command_guard`, ONLY on `kind = \"constraint\"` or `kind = \"decision\"`; null otherwise):\n\
  * Populate `command_guard = {{\"forbidden_commands\": [...], \"allowed_commands\": [...], \"required_flags\": [...]}}` whenever a `constraint` or `decision` bans CLI commands (`forbidden_commands`, e.g. `[\"pip install\"]`), names permitted replacement commands (`allowed_commands`, e.g. `[\"uv pip install\"]`), or mandates CLI flags (`required_flags`, e.g. `[\"--schema\"]` or `[\"--locked\"]`).\n\
  * When a command is prohibited ONLY when missing a required flag (e.g. \"require `prisma migrate deploy --schema`\"), list the base command in `forbidden_commands` (`[\"prisma migrate deploy\"]`) AND the mandatory flag in `required_flags` (`[\"--schema\"]`).\n\
- Membership (`kind = \"membership\"`, e.g. \"Erin is in our book club\"): populate BOTH `group_concept` (`kind = \"group\"`) and `member_mention` (\"Erin\").\n\
- `assumed_premise_ids`: include ONLY IDs from `active_decisions` that this claim explicitly presupposes (else `[]`).\n\
- `quote`: EVERY extracted item MUST set `turn_ordinal` to that turn's exact 0-based integer `N` from `[turn:N]`, and `verbatim_quote` MUST be an exact contiguous substring of that target `<chunk>` turn's text.\n\
- `evidence`: set to null unless ratifying an earlier proposal on `decision`/`constraint`.\n\
- `value_json`: the claim's structured value encoded as a JSON STRING (e.g. `\"{{\\\"room\\\":\\\"B2\\\"}}\"`), or `\"null\"`.\n\
- Output format: EVERY key defined by the output schema MUST be present on every object; use null for unused optional keys.\n\
\n\
{context_wrapper}## 5. Target Source Chunks\n\
{chunks_section}",
        speaker = request.speaker,
        ref_y = request.reference_calendar.reference_date.year,
        ref_m = request.reference_calendar.reference_date.month,
        ref_d = request.reference_calendar.reference_date.day,
        ref_wd = request.reference_calendar.reference_date.weekday_iso,
        tz = request.reference_calendar.utc_offset_minutes,
    )
}

/// Builds the 4-way semantic entailment & contradiction prompt adapted from Graphiti `dedupe_edges.py`.
pub fn build_entailment_prompt(query: &EntailmentQuery, redactor: &SecretRedactor) -> String {
    let left_label = redactor.redact(&query.left_label);
    let left_domain = redactor.redact(&query.left_domain);
    let right_label = redactor.redact(&query.right_label);
    let right_domain = redactor.redact(&query.right_domain);
    let left_quote_line = query
        .left_quote
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|q| format!("  verbatim_quote=\"{}\"\n", redactor.redact(q)))
        .unwrap_or_default();
    let right_quote_line = query
        .right_quote
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|q| format!("  verbatim_quote=\"{}\"\n", redactor.redact(q)))
        .unwrap_or_default();

    format!(
        "You are `kp`'s strict semantic entailment & contradiction judge (adapted from Graphiti `dedupe_edges.py`, Apache-2.0).\n\
Question: When this event happens, does the condition of this rule hold?（这件事发生时，这条规矩的条件成立吗？）\n\
Context: LEFT and RIGHT are spoken by colleagues sharing the same workspace/campus/team; unless the two sentences explicitly specify conflicting cities or isolated facilities, assume shared campus/site announcements (e.g. a campus substation outage vs a lab power-outage rule) apply to the same environment.\n\
Judge using BOTH the concept labels AND the original human `verbatim_quote` sentences when present:\n\
- LEFT (event / occurrence): label=\"{left_label}\", domain=\"{left_domain}\"\n\
{left_quote_line}- RIGHT (rule condition / target): label=\"{right_label}\", domain=\"{right_domain}\"\n\
{right_quote_line}\n\
Return one JSON object `{{\"verdict\": ..., \"question\": ...}}` matching the output schema (`question` is null unless the verdict is `ambiguous`) with one of four verdicts:\n\
1. `{{\"verdict\": \"entails\", \"question\": null}}`: When LEFT happens, condition RIGHT holds / is satisfied — including paraphrases, specific-to-general instances (e.g. \"database primary is down\" entails \"database unavailable\", or \"campus substation outage\" entails \"when a power outage occurs in our lab\"), and expressions sharing words that describe the SAME situation (e.g. \"X\" vs \"出现 X 的时候\" SHOULD be `entails`, NOT `unrelated` or `ambiguous`).\n\
2. `{{\"verdict\": \"contradicts\", \"question\": null}}`: LEFT cancels, resolves, or negates RIGHT within the same domain (e.g. \"flight reinstated\" contradicts \"flight cancelled\").\n\
3. `{{\"verdict\": \"unrelated\", \"question\": null}}`: The underlying situations have no causal or logical trigger relation. Merely sharing the same location or date/time when the underlying situations are unrelated is ALWAYS `unrelated` (e.g. \"Berlin trade fair on Friday\" vs \"no shellfish at Berlin dinners\").\n\
4. `{{\"verdict\": \"ambiguous\", \"question\": \"...\"}}`: Borderline scope where human confirmation is required."
    )
}

#[cfg(test)]
mod tests {
    use kp_model::{CalendarTable, EpochMs, SourceId};

    use super::*;

    /// Recursively validates OpenAI strict Structured Outputs rules on a schema node.
    fn check_strict_node(node: &Value, path: &str, errors: &mut Vec<String>) {
        let Some(obj) = node.as_object() else {
            errors.push(format!("{path}: schema node is not an object"));
            return;
        };
        if obj.is_empty() {
            errors.push(format!("{path}: bare {{}} schema node"));
            return;
        }
        if obj.contains_key("oneOf") || obj.contains_key("allOf") {
            errors.push(format!("{path}: oneOf/allOf is forbidden"));
        }
        if obj.contains_key("$schema") || obj.contains_key("const") {
            errors.push(format!("{path}: $schema/const keyword is not allowed"));
        }
        if let Some(any_of) = obj.get("anyOf") {
            if obj.contains_key("type") {
                errors.push(format!("{path}: anyOf wrapper must not also declare type"));
            }
            match any_of.as_array() {
                Some(branches) if !branches.is_empty() => {
                    for (idx, branch) in branches.iter().enumerate() {
                        check_strict_node(branch, &format!("{path}.anyOf[{idx}]"), errors);
                    }
                }
                _ => errors.push(format!("{path}: anyOf must be a nonempty array")),
            }
            return;
        }
        let types: Vec<&str> = match obj.get("type") {
            Some(Value::String(t)) => vec![t.as_str()],
            Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
            _ => {
                errors.push(format!("{path}: missing type"));
                return;
            }
        };
        if types.contains(&"object") {
            if obj.get("additionalProperties") != Some(&Value::Bool(false)) {
                errors.push(format!("{path}: additionalProperties must be false"));
            }
            let Some(props) = obj.get("properties").and_then(Value::as_object) else {
                errors.push(format!("{path}: object without properties"));
                return;
            };
            let prop_keys: BTreeSet<&str> = props.keys().map(String::as_str).collect();
            let required: BTreeSet<&str> = obj
                .get("required")
                .and_then(Value::as_array)
                .map(|r| r.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if prop_keys != required {
                errors.push(format!(
                    "{path}: required {required:?} != properties {prop_keys:?}"
                ));
            }
            for (key, child) in props {
                check_strict_node(child, &format!("{path}.{key}"), errors);
            }
        }
        if types.contains(&"array") {
            match obj.get("items") {
                Some(items) => check_strict_node(items, &format!("{path}[]"), errors),
                None => errors.push(format!("{path}: array without items")),
            }
        }
    }

    #[test]
    fn extraction_and_entailment_schemas_are_openai_strict_compliant() {
        for (name, schema) in [
            ("extraction", extraction_batch_json_schema()),
            ("entailment", entailment_verdict_json_schema()),
            ("guard_backfill", command_guard_backfill_json_schema()),
        ] {
            assert_eq!(
                schema.get("type"),
                Some(&json!("object")),
                "{name}: root must be an object"
            );
            assert!(
                schema.get("anyOf").is_none(),
                "{name}: root anyOf forbidden"
            );
            let mut errors = Vec::new();
            check_strict_node(&schema, name, &mut errors);
            assert!(errors.is_empty(), "{name}: {errors:#?}");
            assert!(!schema.to_string().contains("oneOf"));
        }
    }

    #[test]
    fn strict_wire_payloads_decode_into_narrow_model_atoms() -> kp_model::Result<()> {
        let null_scope = json!({"scope": "speaker_self", "mentions": null, "group_concept": null});
        let group_scope = json!({
            "scope": "relation_group",
            "mentions": null,
            "group_concept": {
                "ref_type": "existing", "concept_id": "cpt_family",
                "kind": null, "label": null, "domain": null, "polarity": null
            }
        });
        let payload = json!({
            "skip_summary": false,
            "atoms": [{
                "kind": "constraint",
                "claim_text": "周末不加班",
                "value_json": "{\"prohibit\":\"driving\"}",
                "scope": null_scope,
                "ambiguous_scope": {"broader_candidate": group_scope, "question": "只管你自己，还是全家？"},
                "condition_concept": {
                    "ref_type": "propose_new", "concept_id": null, "kind": "condition",
                    "label": "周末", "domain": null, "polarity": null
                },
                "action_concept": null,
                "group_concept": null,
                "member_mention": null,
                "valid_dates": {"start_date_iso": "2026-11-21", "end_date_iso": "2026-11-22", "open_ended": false},
                "speaker": "dave",
                "assumed_premise_ids": ["fact_dec_1"],
                "quote": {"chunk_id": "c0", "verbatim_quote": "周末不加班", "turn_ordinal": null, "approx_char_offset": 3},
                "evidence": null,
                "write_op": {"op": "supersede_own", "target_fact_id": "fact_old"}
            }]
        });
        let env = parse_extraction_envelope(payload)?;
        assert_eq!(env.atoms.len(), 1);
        let atom = &env.atoms[0];
        assert_eq!(atom.value, json!({"prohibit": "driving"}));
        assert_eq!(atom.scope, Some(ModelScopeRef::SpeakerSelf));
        let amb = atom
            .ambiguous_scope
            .as_ref()
            .ok_or_else(|| KpError::invalid("t", "amb"))?;
        assert_eq!(
            amb.broader_candidate,
            ModelScopeRef::RelationGroup {
                group_concept: ModelConceptRef::Existing {
                    concept_id: "cpt_family".parse()?
                }
            }
        );
        assert_eq!(
            atom.condition_concept,
            Some(ModelConceptRef::ProposeNew {
                kind: ConceptKind::Condition,
                label: "周末".into(),
                domain: String::new(),
                polarity: ConceptPolarity::Neutral,
            })
        );
        assert_eq!(atom.valid_window, None);
        assert_eq!(
            atom.valid_dates,
            Some(ModelDateRange {
                start_date_iso: "2026-11-21".into(),
                end_date_iso: Some("2026-11-22".into()),
                open_ended: false,
            })
        );
        assert_eq!(atom.speaker, Some("dave".parse()?));
        assert_eq!(atom.quote.approx_char_offset, Some(3));
        assert_eq!(
            atom.write_op,
            ModelWriteOp::SupersedeOwn {
                target_fact_id: "fact_old".parse()?
            }
        );

        // Structurally valid wire objects with inconsistent variants fail closed.
        let bad = json!({
            "skip_summary": false,
            "atoms": [{
                "kind": "constraint", "claim_text": "x", "value_json": "null",
                "scope": {"scope": "named_mentions", "mentions": null, "group_concept": null},
                "ambiguous_scope": null, "condition_concept": null, "action_concept": null,
                "group_concept": null, "member_mention": null, "valid_dates": null, "speaker": null,
                "assumed_premise_ids": [],
                "quote": {"chunk_id": "c0", "verbatim_quote": "x", "turn_ordinal": null, "approx_char_offset": null},
                "write_op": {"op": "add", "target_fact_id": null}
            }]
        });
        assert!(parse_extraction_envelope(bad).is_err());
        assert!(
            parse_extraction_envelope(json!({"skip_summary": true, "atoms": []}))?.skip_summary
        );
        // Replies smuggling epoch `valid_window` (non-strict shape) are rejected outright.
        let epoch_smuggle = json!({
            "skip_summary": false,
            "atoms": [{
                "kind": "event", "claim_text": "x", "value": null,
                "valid_window": {"valid_from": 1, "valid_until": 2},
                "quote": {"chunk_id": "c0", "verbatim_quote": "x"},
                "write_op": {"op": "add"}
            }]
        });
        assert!(parse_extraction_envelope(epoch_smuggle).is_err());
        Ok(())
    }

    #[test]
    fn minhash_and_3gram_jaccard_detect_near_duplicates_and_reject_distinct_phrases() {
        assert!(is_near_duplicate(
            "我们读书会每月第一个周六聚会！",
            "我们读书会每月第一个周六聚会"
        ));
        assert!(is_near_duplicate(
            "维护窗口内只允许回滚 PR",
            "维护窗口内只允许回滚PR。"
        ));
        // Non-identical normalized strings with >= 0.9 3-gram Jaccard pass LSH + Jaccard check.
        assert!(is_near_duplicate(
            "下周二起数据库迁移窗口期间严禁执行任何非回滚类的线上变更请求",
            "下周二起数据库迁移窗口期间严禁执行任何非回滚类的线上变更请求单"
        ));
        assert!(!is_near_duplicate("周六柏林有展会", "在柏林吃饭不点贝类"));
        assert!(!is_near_duplicate("航班取消", "航班恢复"));
        // Low-entropy repetition gate blocks spurious fuzzy collisions.
        assert!(!is_near_duplicate("哈哈哈哈哈哈", "哈哈哈哈哈哈哈"));
        assert!(is_near_duplicate("哈哈哈哈哈哈", "哈哈哈哈哈哈"));
    }

    #[test]
    fn trivial_turn_sentinel_catches_acks_and_skip_summary() {
        for trivial in [
            "",
            "   ",
            "<skip_summary/>",
            "好的！",
            "收到。",
            "OK",
            "lgtm",
        ] {
            assert!(is_trivial_turn_text(trivial), "{trivial:?}");
        }
        assert!(!is_trivial_turn_text("周五健身房闭馆"));
        assert!(!is_trivial_turn_text("周末不加班"));
    }

    #[test]
    fn extraction_and_entailment_prompts_redact_secrets_and_embed_calendar() -> kp_model::Result<()>
    {
        let redactor =
            SecretRedactor::new().map_err(|_| kp_model::KpError::invalid("re", "err"))?;
        let req = ExtractionBatchRequest {
            source_id: SourceId::new("src_test")?,
            speaker: "bob".parse()?,
            reference_calendar: CalendarTable::from_reference_in_tz(
                EpochMs(1_795_089_600_000),
                &kp_model::TimeZone::get("Asia/Shanghai")
                    .map_err(|_| kp_model::KpError::invalid("tz", "Asia/Shanghai"))?,
            )?,
            active_concepts: Vec::new(),
            active_decisions: Vec::new(),
            speaker_own_facts: Vec::new(),
            speaker_own_facts_by_actor: BTreeMap::new(),
            chunks: vec![kp_model::ExtractionChunkSlice {
                chunk_id: "c0".into(),
                source_id: SourceId::new("src_test")?,
                author: "bob".parse()?,
                base_anchor: Default::default(),
                role: kp_model::SliceRole::Target,
                reply_to_chunk_id: None,
                heading: "chat".into(),
                body: "我们组周末不加班 token=ghp_supersecret123456".into(),
            }],
            min_rate_limit_remaining_pct: 15,
        };
        let prompt = build_extraction_prompt(&req, &redactor);
        assert!(!prompt.contains("ghp_supersecret123456"));
        assert!(prompt.contains("[REDACTED]"));
        assert!(prompt.contains("2026-11-20"));
        // The model is never shown epoch milliseconds to copy.
        assert!(!prompt.contains("1795"));
        assert!(!prompt.contains("9223372036854775807"));
        assert!(prompt.contains("supersede_own"));

        let eq = EntailmentQuery {
            left_label: "数据库主库宕机".into(),
            left_domain: "ops".into(),
            right_label: "数据库不可用".into(),
            right_domain: "ops".into(),
            left_quote: Some("本周五主库宕机维护 token=ghp_supersecret123456".into()),
            right_quote: Some("遇到数据库不可用的时候都切只读副本".into()),
        };
        let ep = build_entailment_prompt(&eq, &redactor);
        assert!(ep.contains("unrelated"));
        assert!(ep.contains("contradicts"));
        assert!(ep.contains("这件事发生时，这条规矩的条件成立吗？"));
        assert!(ep.contains("遇到数据库不可用的时候都切只读副本"));
        assert!(!ep.contains("ghp_supersecret123456"));
        Ok(())
    }

    #[test]
    fn prompt_turn_prefixes_align_one_to_one_with_verify_verbatim_quote_turn_ordinal(
    ) -> kp_model::Result<()> {
        let redactor =
            SecretRedactor::new().map_err(|_| kp_model::KpError::invalid("re", "err"))?;
        let turns = vec![
            kp_model::ChunkTurn {
                speaker: Some("alice".parse()?),
                is_assistant: false,
                text: "周五下午三点核对书单。".into(),
            },
            kp_model::ChunkTurn {
                speaker: Some("bob".parse()?),
                is_assistant: false,
                text: "收到".into(),
            },
            kp_model::ChunkTurn {
                speaker: Some("alice".parse()?),
                is_assistant: false,
                text: "周五下午三点核对书单。".into(),
            },
        ];
        let body = kp_model::format_tagged_turns(&turns);
        let req = ExtractionBatchRequest {
            source_id: SourceId::new("src_turns")?,
            speaker: "alice".parse()?,
            reference_calendar: CalendarTable::from_reference_in_tz(
                EpochMs(1_795_089_600_000),
                &kp_model::TimeZone::get("Asia/Shanghai")
                    .map_err(|_| kp_model::KpError::invalid("tz", "Asia/Shanghai"))?,
            )?,
            active_concepts: Vec::new(),
            active_decisions: Vec::new(),
            speaker_own_facts: Vec::new(),
            speaker_own_facts_by_actor: BTreeMap::new(),
            chunks: vec![kp_model::ExtractionChunkSlice {
                chunk_id: "c_dup".into(),
                source_id: SourceId::new("src_turns")?,
                author: "alice".parse()?,
                base_anchor: Default::default(),
                role: kp_model::SliceRole::Target,
                reply_to_chunk_id: None,
                heading: "group".into(),
                body: body.clone(),
            }],
            min_rate_limit_remaining_pct: 15,
        };
        let prompt = build_extraction_prompt(&req, &redactor);
        assert!(prompt.contains("[turn:0] [speaker:alice] 周五下午三点核对书单。"));
        assert!(prompt.contains("[turn:1] [speaker:bob] 收到"));
        assert!(prompt.contains("[turn:2] [speaker:alice] 周五下午三点核对书单。"));
        // Stored `chunk.body` was not mutated.
        assert!(!req.chunks[0].body.contains("[turn:"));

        // Duplicate quote disambiguates cleanly using `[turn:0]` vs `[turn:2]` indices.
        let m0 = kp_model::verify_verbatim_quote(
            &body,
            "周五下午三点核对书单。",
            kp_model::QuoteLocationHint {
                turn_ordinal: Some(0),
                approx_char_offset: None,
            },
        )?;
        let m2 = kp_model::verify_verbatim_quote(
            &body,
            "周五下午三点核对书单。",
            kp_model::QuoteLocationHint {
                turn_ordinal: Some(2),
                approx_char_offset: None,
            },
        )?;
        assert!(m0.span.char_start < m2.span.char_start);
        Ok(())
    }
}
