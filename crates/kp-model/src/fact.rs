use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::schema::text_enum;
use crate::{
    compute_fact_content_hash, ActorId, BitemporalStamp, BroaderScopeCandidate, CharSpan,
    ContentHash, EpochMs, FactId, GovernedScope, KpError, LibraryId, LogicalKey,
    MembershipRelation, Result, SourceId, SourceScopeId, StrongRef,
};

text_enum!(FactKind {
    Observation => "observation", FactSlot => "fact_slot", Event => "event",
    Constraint => "constraint", Decision => "decision", Membership => "membership",
    ClosedNegative => "closed_negative", Alias => "alias", DerivedFact => "derived_fact",
    CollisionWarning => "collision_warning", Conflict => "conflict", Synthesis => "synthesis",
    PlaybookBullet => "playbook_bullet", SkillVersion => "skill_version"
});

impl FactKind {
    pub fn is_derived(self) -> bool {
        match self {
            Self::Alias
            | Self::DerivedFact
            | Self::CollisionWarning
            | Self::Conflict
            | Self::Synthesis
            | Self::PlaybookBullet
            | Self::SkillVersion => true,
            Self::Observation
            | Self::FactSlot
            | Self::Event
            | Self::Constraint
            | Self::Decision
            | Self::Membership
            | Self::ClosedNegative => false,
        }
    }
}
text_enum!(FactStatus {
    Active => "active", Superseded => "superseded", Withdrawn => "withdrawn",
    Invalidated => "invalidated", StalePremise => "stale_premise", Quarantined => "quarantined",
    UnreachableHidden => "unreachable_hidden"
});
text_enum!(FactRank { Preferred => "preferred", Normal => "normal", Deprecated => "deprecated" });
text_enum!(DerivationMode {
    HardRule => "hard_rule",
    ScopedSynthesis => "scoped_synthesis",
    RsiAttribution => "rsi_attribution",
    AssumedPremise => "assumed_premise"
});

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
#[repr(u8)]
pub enum TrustTier {
    OwnerExplicit = 0,
    HumanVerbatim = 1,
    ToolOrModelVerified = 2,
    ExternalUntrusted = 3,
}

impl TryFrom<u8> for TrustTier {
    type Error = KpError;
    fn try_from(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::OwnerExplicit),
            1 => Ok(Self::HumanVerbatim),
            2 => Ok(Self::ToolOrModelVerified),
            3 => Ok(Self::ExternalUntrusted),
            _ => Err(KpError::invalid(
                "trust_tier",
                "expected a value from 0 to 3",
            )),
        }
    }
}

impl From<TrustTier> for u8 {
    fn from(value: TrustTier) -> Self {
        value as Self
    }
}

/// Wire format matches SQLite: an actor array, ["*"], or "__semiring__".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Value", into = "Value")]
pub enum AudienceSpec {
    Explicit(BTreeSet<ActorId>),
    AllMembers,
    SemiringDerived,
}

impl AudienceSpec {
    pub fn allows(&self, viewer: &ActorId, members: &BTreeSet<ActorId>) -> bool {
        match self {
            Self::Explicit(actors) => actors.contains(viewer),
            Self::AllMembers => members.contains(viewer),
            Self::SemiringDerived => false,
        }
    }
}

impl TryFrom<Value> for AudienceSpec {
    type Error = KpError;
    fn try_from(value: Value) -> Result<Self> {
        if value == Value::String("__semiring__".into()) {
            return Ok(Self::SemiringDerived);
        }
        if value == serde_json::json!(["*"]) {
            return Ok(Self::AllMembers);
        }
        let actors = serde_json::from_value(value).map_err(|_| {
            KpError::invalid("audience", "expected actors, [\"*\"], or semiring marker")
        })?;
        Ok(Self::Explicit(actors))
    }
}

impl From<AudienceSpec> for Value {
    fn from(value: AudienceSpec) -> Self {
        match value {
            AudienceSpec::Explicit(actors) => Self::Array(
                actors
                    .into_iter()
                    .map(|a| Self::String(a.to_string()))
                    .collect(),
            ),
            AudienceSpec::AllMembers => serde_json::json!(["*"]),
            AudienceSpec::SemiringDerived => Self::String("__semiring__".into()),
        }
    }
}

/// Derived facts carry no audience or validity interval to become stale.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FactAccess {
    Base {
        audience: AudienceSpec,
        stamp: BitemporalStamp,
    },
    Derived {
        recorded_at: EpochMs,
        expired_at: Option<EpochMs>,
    },
}

impl FactAccess {
    pub fn recorded_at(&self) -> EpochMs {
        match self {
            Self::Base { stamp, .. } => stamp.recorded_at,
            Self::Derived { recorded_at, .. } => *recorded_at,
        }
    }

    pub fn expired_at(&self) -> Option<EpochMs> {
        match self {
            Self::Base { stamp, .. } => stamp.expired_at,
            Self::Derived { expired_at, .. } => *expired_at,
        }
    }
}

/// Coordinates are optional because a source can combine document, media, and code loci.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AnchorSpec {
    pub file_path: Option<String>,
    pub paragraph: Option<u32>,
    pub slide: Option<u32>,
    pub sheet: Option<String>,
    pub cell: Option<String>,
    pub source_id: Option<SourceId>,
    pub page: Option<u32>,
    pub bbox: Option<[f64; 4]>,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
    pub start_byte: Option<u64>,
    pub end_byte: Option<u64>,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    pub commit_sha: Option<String>,
    pub symbol: Option<String>,
    pub sig_hash: Option<ContentHash>,
    pub body_hash: Option<ContentHash>,
    #[serde(default)]
    pub chunk_id: Option<String>,
    #[serde(default)]
    pub verbatim_quote: Option<String>,
    #[serde(default)]
    pub char_start: Option<u32>,
    #[serde(default)]
    pub char_end: Option<u32>,
}

/// Optional disambiguation hint when the same quote appears more than once in a chunk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteLocationHint {
    pub turn_ordinal: Option<u32>,
    pub approx_char_offset: Option<u32>,
}

/// Verified verbatim substring mapped back onto the exact original characters and bytes of `chunk_body`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedQuoteMatch {
    pub exact_quote: String,
    pub span: CharSpan,
    pub byte_start: u64,
    pub byte_end: u64,
}

fn fold_quote_char(c: char) -> char {
    if ('\u{FF01}'..='\u{FF5E}').contains(&c) {
        return char::from_u32((c as u32) - 0xFEE0).unwrap_or(c);
    }
    match c {
        '\u{201C}' | '\u{201D}' => '"',
        '\u{2018}' | '\u{2019}' => '\'',
        '\u{2013}' | '\u{2014}' => '-',
        other => other,
    }
}

struct NormalizedText {
    chars: Vec<char>,
    spans: Vec<(u32, u32)>,
    char_to_byte: Vec<usize>,
}

fn normalize_with_spans(text: &str) -> Result<NormalizedText> {
    let mut raw_chars = Vec::new();
    let mut char_to_byte = Vec::new();
    for (byte_idx, ch) in text.char_indices() {
        raw_chars.push(ch);
        char_to_byte.push(byte_idx);
    }
    char_to_byte.push(text.len());

    let mut chars = Vec::with_capacity(raw_chars.len());
    let mut spans = Vec::with_capacity(raw_chars.len());
    let mut idx = 0usize;
    while idx < raw_chars.len() {
        if raw_chars[idx].is_whitespace() {
            let start = idx;
            while idx < raw_chars.len() && raw_chars[idx].is_whitespace() {
                idx += 1;
            }
            let start_u32 = u32::try_from(start)
                .map_err(|_| KpError::invalid("char_start", "character offset overflow"))?;
            let end_u32 = u32::try_from(idx)
                .map_err(|_| KpError::invalid("char_end", "character offset overflow"))?;
            chars.push(' ');
            spans.push((start_u32, end_u32));
        } else {
            let start_u32 = u32::try_from(idx)
                .map_err(|_| KpError::invalid("char_start", "character offset overflow"))?;
            let end_u32 = u32::try_from(idx + 1)
                .map_err(|_| KpError::invalid("char_end", "character offset overflow"))?;
            chars.push(fold_quote_char(raw_chars[idx]));
            spans.push((start_u32, end_u32));
            idx += 1;
        }
    }
    Ok(NormalizedText {
        chars,
        spans,
        char_to_byte,
    })
}

fn compute_turn_char_ranges(chunk_body: &str) -> Vec<(u32, u32)> {
    let delimiter = if chunk_body.contains("\n\n") {
        "\n\n"
    } else {
        "\n"
    };
    let mut ranges = Vec::new();
    let mut byte_cursor = 0usize;
    for part in chunk_body.split(delimiter) {
        let part_start_byte = byte_cursor;
        let part_end_byte = part_start_byte + part.len();
        byte_cursor = part_end_byte + delimiter.len();
        if part.trim().is_empty() {
            continue;
        }
        let start_chars = chunk_body[..part_start_byte].chars().count();
        let end_chars = start_chars + part.chars().count();
        if let (Ok(s), Ok(e)) = (u32::try_from(start_chars), u32::try_from(end_chars)) {
            ranges.push((s, e));
        }
    }
    ranges
}

/// Verifies `verbatim_quote` against `chunk_body` after fullwidth/halfwidth and whitespace normalization,
/// maps the match back to the exact original interval in `chunk_body`, and requires a turn or offset hint
/// whenever the quote occurs multiple times.
pub fn verify_verbatim_quote(
    chunk_body: &str,
    verbatim_quote: &str,
    hint: QuoteLocationHint,
) -> Result<VerifiedQuoteMatch> {
    let trimmed_quote = verbatim_quote.trim();
    if trimmed_quote.is_empty() {
        return Err(KpError::invalid(
            "verbatim_quote",
            "quote must be nonempty verbatim text",
        ));
    }
    let chunk_norm = normalize_with_spans(chunk_body)?;
    let quote_norm = normalize_with_spans(trimmed_quote)?;
    if quote_norm.chars.is_empty() || chunk_norm.chars.len() < quote_norm.chars.len() {
        return Err(KpError::invalid(
            "verbatim_quote",
            "quote is not a substring of the source chunk",
        ));
    }

    let mut matches = Vec::new();
    let last_start = chunk_norm.chars.len() - quote_norm.chars.len();
    for pos in 0..=last_start {
        if chunk_norm.chars[pos..pos + quote_norm.chars.len()] == quote_norm.chars[..] {
            let char_start = chunk_norm.spans[pos].0;
            let char_end = chunk_norm.spans[pos + quote_norm.chars.len() - 1].1;
            let byte_start_usize = chunk_norm.char_to_byte[char_start as usize];
            let byte_end_usize = chunk_norm.char_to_byte[char_end as usize];
            let byte_start = u64::try_from(byte_start_usize)
                .map_err(|_| KpError::invalid("byte_start", "byte offset overflow"))?;
            let byte_end = u64::try_from(byte_end_usize)
                .map_err(|_| KpError::invalid("byte_end", "byte offset overflow"))?;
            matches.push(VerifiedQuoteMatch {
                exact_quote: chunk_body[byte_start_usize..byte_end_usize].to_owned(),
                span: CharSpan::new(char_start, char_end)?,
                byte_start,
                byte_end,
            });
        }
    }

    if matches.is_empty() {
        return Err(KpError::invalid(
            "verbatim_quote",
            "quote is not a contiguous substring of the source chunk",
        ));
    }
    if matches.len() == 1 {
        return matches
            .into_iter()
            .next()
            .ok_or_else(|| KpError::invalid("verbatim_quote", "missing match"));
    }

    let mut candidates = matches;
    if let Some(turn_idx) = hint.turn_ordinal {
        let turns = compute_turn_char_ranges(chunk_body);
        let Some(&(turn_start, turn_end)) = turns.get(turn_idx as usize) else {
            return Err(KpError::invalid(
                "turn_ordinal",
                "turn_ordinal out of range for source chunk",
            ));
        };
        candidates.retain(|m| m.span.char_start >= turn_start && m.span.char_end <= turn_end);
    }

    if candidates.len() > 1 {
        if let Some(approx) = hint.approx_char_offset {
            let min_dist = candidates
                .iter()
                .map(|m| (i64::from(m.span.char_start) - i64::from(approx)).unsigned_abs())
                .min()
                .unwrap_or(u64::MAX);
            candidates.retain(|m| {
                (i64::from(m.span.char_start) - i64::from(approx)).unsigned_abs() == min_dist
            });
        }
    }

    if candidates.len() == 1 {
        candidates
            .into_iter()
            .next()
            .ok_or_else(|| KpError::invalid("verbatim_quote", "missing disambiguated match"))
    } else {
        Err(KpError::invalid(
            "verbatim_quote",
            "quote appears multiple times in chunk without an unambiguous turn or offset hint",
        ))
    }
}

/// Convenience wrapper verifying an unambiguous quote without hints and returning `(char_start, char_end)`.
pub fn verify_verbatim_substring(chunk_body: &str, verbatim_quote: &str) -> Result<(u32, u32)> {
    let verified = verify_verbatim_quote(chunk_body, verbatim_quote, QuoteLocationHint::default())?;
    Ok((verified.span.char_start, verified.span.char_end))
}

text_enum!(VerifyStatus {
    Verified => "verified", MovedUnchanged => "moved_unchanged",
    BodyModified => "body_modified", SigChanged => "sig_changed", Orphaned => "orphaned"
});

text_enum!(ContrastKind {
    HumanCorrection => "human_correction", FailureToRecovery => "failure_to_recovery",
    CrossAvatarContrast => "cross_avatar_contrast"
});

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fact {
    pub id: FactId,
    pub origin_library: LibraryId,
    pub logical_key: LogicalKey,
    pub kind: FactKind,
    pub subject: String,
    pub predicate: String,
    pub object: Value,
    pub tags: BTreeSet<String>,
    pub author: ActorId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingested_by: Option<ActorId>,
    pub trust_tier: TrustTier,
    pub access: FactAccess,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_scope_id: Option<SourceScopeId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub governed_scope: Option<GovernedScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broader_scope: Option<BroaderScopeCandidate>,
    pub status: FactStatus,
    pub superseded_by: Option<FactId>,
    pub rank: FactRank,
    pub source_anchor: Option<AnchorSpec>,
    pub unverified_gaps: Vec<String>,
    pub miss_count: u32,
}

impl Fact {
    pub fn strong_ref(&self) -> StrongRef {
        StrongRef {
            origin_library: self.origin_library.clone(),
            logical_key: self.logical_key.clone(),
            content_sha256: compute_fact_content_hash(
                &self.subject,
                &self.predicate,
                &self.object,
                self.kind,
            ),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.subject.trim().is_empty() || self.predicate.trim().is_empty() {
            return Err(KpError::invalid(
                "fact",
                "subject and predicate must be nonempty",
            ));
        }
        if self.kind != FactKind::Alias
            && self.kind.is_derived() != matches!(self.access, FactAccess::Derived { .. })
        {
            return Err(KpError::invalid(
                "access",
                "fact kind and provenance must agree",
            ));
        }
        if let FactAccess::Base { audience, stamp } = &self.access {
            if matches!(audience, AudienceSpec::SemiringDerived) {
                return Err(KpError::invalid(
                    "audience",
                    "base fact cannot use derived access",
                ));
            }
            if stamp.valid.valid_from >= stamp.valid.valid_until {
                return Err(KpError::invalid("validity", "interval must be nonempty"));
            }
        }
        if self.kind == FactKind::Membership {
            serde_json::from_value::<MembershipRelation>(self.object.clone()).map_err(|_| {
                KpError::invalid(
                    "membership",
                    "membership fact object must be a valid MembershipRelation with group_concept_id",
                )
            })?;
        }
        if self.kind == FactKind::SkillVersion {
            let package: crate::SkillPackage = serde_json::from_value(self.object.clone())
                .map_err(|_| KpError::invalid("skill", "invalid package"))?;
            package.validate()?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "fact_tests.rs"]
mod tests;
