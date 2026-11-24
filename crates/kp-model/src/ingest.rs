//! Public JSONL Ingest Contract wire types, validation, and parser (`Rev 5 §0.1.2`).
//!
//! External callers, hooks, and decoupled connectors emit newline-delimited JSON (`JSONL`) lines:
//! - [`PublicContainerRoster`]: live container membership updates (`container_id`, `label`,
//!   `members`, `all_library_members`, `synced_at`), emitted ahead of events so container access
//!   ceilings stay current before records are evaluated;
//! - [`PublicIngestRecord`]: standard inbound event records (`source: { kind, uri, external_id,
//!   container_id? }`, `speaker`, `audience`, `occurred_at`, `content`, `op: upsert | supersede |
//!   withdraw`).

use std::fmt;

use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::time::{normalize_numeric_epoch, parse_timestamp_ms_in_tz, TimeZone};
use crate::{ActorId, EpochMs, KpError, ModelRoutePolicy, Result};

/// Upstream source coordinates on a [`PublicIngestRecord`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicIngestSource {
    /// Source category slug (e.g. `"chat"`, `"repo"`, `"session"`, `"manual"`).
    pub kind: String,
    /// Canonical upstream source URI (e.g. `"chat://room_alpha/msg_1001"` or `"stdin://note/1"`).
    pub uri: String,
    /// Stable upstream record identifier (`external_id`).
    pub external_id: String,
    /// Optional container identifier (`source_scope_id`) governing dynamic container membership.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_id: Option<String>,
}

impl PublicIngestSource {
    /// Validates required source coordinates and rejects blank or control-character strings.
    pub fn validate(&self) -> Result<()> {
        validate_nonempty_clean(&self.kind, "source.kind", "source kind must be nonempty")?;
        validate_nonempty_clean(&self.uri, "source.uri", "source uri must be nonempty")?;
        validate_nonempty_clean(
            &self.external_id,
            "source.external_id",
            "external_id must be nonempty",
        )?;
        if let Some(cid) = &self.container_id {
            validate_nonempty_clean(
                cid,
                "source.container_id",
                "container_id must be nonempty when provided",
            )?;
        }
        Ok(())
    }
}

/// Speaker attribution on a [`PublicIngestRecord`]: either a library `actor_id` or an external
/// `namespace:id` identifier, with an optional human-readable display name.
///
/// Serializes uniformly as `{"id": "...", "display_name": "..."}` (omitting `display_name` when
/// `None`), and deserializes from either a plain string (`"eng_alice"` / `"ext_ns:user_1"`) or
/// an object (`{"id": "ext_ns:user_1", "display_name": "李四"}`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicSpeaker {
    pub id: String,
    pub display_name: Option<String>,
}

impl PublicSpeaker {
    pub fn new(id: impl Into<String>, display_name: Option<String>) -> Self {
        let id_str = id.into();
        let clean_display = display_name
            .map(|d| d.trim().to_owned())
            .filter(|d| !d.is_empty());
        Self {
            id: id_str,
            display_name: clean_display,
        }
    }

    /// Validates that `id` is either a valid library [`ActorId`] or a `namespace:id` external identity.
    pub fn validate(&self) -> Result<()> {
        validate_actor_or_external_id(&self.id, "speaker.id")?;
        if let Some(name) = &self.display_name {
            validate_nonempty_clean(
                name,
                "speaker.display_name",
                "speaker display_name must be nonempty when provided",
            )?;
        }
        Ok(())
    }
}

impl Serialize for PublicSpeaker {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let field_count = if self.display_name.is_some() { 2 } else { 1 };
        let mut st = serializer.serialize_struct("PublicSpeaker", field_count)?;
        st.serialize_field("id", &self.id)?;
        if let Some(name) = &self.display_name {
            st.serialize_field("display_name", name)?;
        }
        st.end()
    }
}

impl<'de> Deserialize<'de> for PublicSpeaker {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct SpeakerVisitor;

        impl<'de> Visitor<'de> for SpeakerVisitor {
            type Value = PublicSpeaker;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(
                    "a speaker string (`actor_id` or `namespace:id`) or object `{\"id\": ..., \"display_name\": ...}`",
                )
            }

            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
                let trimmed = v.trim();
                if trimmed.is_empty() {
                    return Err(E::custom("speaker id cannot be empty"));
                }
                Ok(PublicSpeaker::new(trimmed.to_owned(), None))
            }

            fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<Self::Value, E> {
                self.visit_str(&v)
            }

            fn visit_map<M: MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<Self::Value, M::Error> {
                let mut id: Option<String> = None;
                let mut display_name: Option<Option<String>> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "id" => {
                            if id.is_some() {
                                return Err(de::Error::duplicate_field("id"));
                            }
                            id = Some(map.next_value()?);
                        }
                        "display_name" => {
                            if display_name.is_some() {
                                return Err(de::Error::duplicate_field("display_name"));
                            }
                            display_name = Some(map.next_value()?);
                        }
                        other => {
                            return Err(de::Error::unknown_field(other, &["id", "display_name"]));
                        }
                    }
                }
                let speaker_id = id
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| de::Error::missing_field("id"))?;
                Ok(PublicSpeaker::new(speaker_id, display_name.flatten()))
            }
        }

        deserializer.deserialize_any(SpeakerVisitor)
    }
}

/// Visibility target for a [`PublicIngestRecord`] (`"container_members"` or `{"explicit": [...]}`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicAudience {
    /// Dynamic visibility following the live membership of `source.container_id`.
    ContainerMembers,
    /// Explicit allowlist of library [`ActorId`]s.
    Explicit(Vec<ActorId>),
}

impl PublicAudience {
    /// Validates audience invariants against `source.container_id`.
    pub fn validate(&self, container_id: Option<&str>) -> Result<()> {
        match self {
            Self::ContainerMembers => {
                if container_id.map(str::trim).is_none_or(str::is_empty) {
                    return Err(KpError::invalid(
                        "audience",
                        "container_members audience requires source.container_id",
                    ));
                }
                Ok(())
            }
            Self::Explicit(actors) => {
                if actors.is_empty() {
                    return Err(KpError::invalid(
                        "audience",
                        "explicit audience must list at least one actor",
                    ));
                }
                Ok(())
            }
        }
    }
}

/// Lifecycle operation on a [`PublicIngestRecord`]: `"upsert"`, `"supersede"` (with
/// `previous_external_id`), or `"withdraw"`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PublicIngestOp {
    Upsert,
    Supersede { previous_external_id: String },
    Withdraw,
}

impl PublicIngestOp {
    /// Validates operation-specific fields against `current_external_id` and `content`.
    pub fn validate(&self, current_external_id: &str, content: &str) -> Result<()> {
        match self {
            Self::Upsert => {
                if content.trim().is_empty() {
                    return Err(KpError::invalid(
                        "content",
                        "upsert record content must be nonempty",
                    ));
                }
                Ok(())
            }
            Self::Supersede {
                previous_external_id,
            } => {
                validate_nonempty_clean(
                    previous_external_id,
                    "previous_external_id",
                    "supersede requires nonempty previous_external_id",
                )?;
                if previous_external_id.trim() == current_external_id.trim() {
                    return Err(KpError::invalid(
                        "previous_external_id",
                        "supersede previous_external_id must differ from source.external_id",
                    ));
                }
                if content.trim().is_empty() {
                    return Err(KpError::invalid(
                        "content",
                        "supersede record content must be nonempty",
                    ));
                }
                Ok(())
            }
            Self::Withdraw => Ok(()),
        }
    }
}

/// Public JSONL ingest envelope emitted by hooks, connectors, and external scripts for `kp ingest`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PublicIngestRecord {
    pub source: PublicIngestSource,
    pub speaker: PublicSpeaker,
    pub audience: PublicAudience,
    pub occurred_at: EpochMs,
    pub content: String,
    #[serde(flatten)]
    pub op: PublicIngestOp,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ingested_by: Option<ActorId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    /// Optional advanced model routing override; omitted (`None`) when inheriting the global model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_policy: Option<ModelRoutePolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

const PUBLIC_INGEST_RECORD_FIELDS: &[&str] = &[
    "source",
    "speaker",
    "audience",
    "occurred_at",
    "content",
    "op",
    "previous_external_id",
    "ingested_by",
    "media_type",
    "model_policy",
    "thread_key",
    "reply_to",
];

impl<'de> Deserialize<'de> for PublicIngestRecord {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct FlexibleEpochMs(
            #[serde(deserialize_with = "deserialize_epoch_ms_flexible")] EpochMs,
        );

        struct RecordVisitor;

        impl<'de> Visitor<'de> for RecordVisitor {
            type Value = PublicIngestRecord;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a PublicIngestRecord JSON object")
            }

            fn visit_map<M: MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<Self::Value, M::Error> {
                let mut source: Option<PublicIngestSource> = None;
                let mut speaker: Option<PublicSpeaker> = None;
                let mut audience: Option<PublicAudience> = None;
                let mut occurred_at: Option<EpochMs> = None;
                let mut content: Option<Option<String>> = None;
                let mut op_raw: Option<String> = None;
                let mut previous_external_id: Option<Option<String>> = None;
                let mut ingested_by: Option<Option<ActorId>> = None;
                let mut media_type: Option<Option<String>> = None;
                let mut model_policy: Option<Option<ModelRoutePolicy>> = None;
                let mut thread_key: Option<Option<String>> = None;
                let mut reply_to: Option<Option<String>> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "source" => {
                            if source.is_some() {
                                return Err(de::Error::duplicate_field("source"));
                            }
                            source = Some(map.next_value()?);
                        }
                        "speaker" => {
                            if speaker.is_some() {
                                return Err(de::Error::duplicate_field("speaker"));
                            }
                            speaker = Some(map.next_value()?);
                        }
                        "audience" => {
                            if audience.is_some() {
                                return Err(de::Error::duplicate_field("audience"));
                            }
                            audience = Some(map.next_value()?);
                        }
                        "occurred_at" => {
                            if occurred_at.is_some() {
                                return Err(de::Error::duplicate_field("occurred_at"));
                            }
                            let FlexibleEpochMs(ts) = map.next_value()?;
                            occurred_at = Some(ts);
                        }
                        "content" => {
                            if content.is_some() {
                                return Err(de::Error::duplicate_field("content"));
                            }
                            content = Some(map.next_value()?);
                        }
                        "op" => {
                            if op_raw.is_some() {
                                return Err(de::Error::duplicate_field("op"));
                            }
                            op_raw = Some(map.next_value()?);
                        }
                        "previous_external_id" => {
                            if previous_external_id.is_some() {
                                return Err(de::Error::duplicate_field("previous_external_id"));
                            }
                            previous_external_id = Some(map.next_value()?);
                        }
                        "ingested_by" => {
                            if ingested_by.is_some() {
                                return Err(de::Error::duplicate_field("ingested_by"));
                            }
                            ingested_by = Some(map.next_value()?);
                        }
                        "media_type" => {
                            if media_type.is_some() {
                                return Err(de::Error::duplicate_field("media_type"));
                            }
                            media_type = Some(map.next_value()?);
                        }
                        "model_policy" => {
                            if model_policy.is_some() {
                                return Err(de::Error::duplicate_field("model_policy"));
                            }
                            model_policy = Some(map.next_value()?);
                        }
                        "thread_key" => {
                            if thread_key.is_some() {
                                return Err(de::Error::duplicate_field("thread_key"));
                            }
                            thread_key = Some(map.next_value()?);
                        }
                        "reply_to" => {
                            if reply_to.is_some() {
                                return Err(de::Error::duplicate_field("reply_to"));
                            }
                            reply_to = Some(map.next_value()?);
                        }
                        other => {
                            return Err(de::Error::unknown_field(
                                other,
                                PUBLIC_INGEST_RECORD_FIELDS,
                            ));
                        }
                    }
                }

                let source = source.ok_or_else(|| de::Error::missing_field("source"))?;
                let speaker = speaker.ok_or_else(|| de::Error::missing_field("speaker"))?;
                let audience = audience.ok_or_else(|| de::Error::missing_field("audience"))?;
                let occurred_at =
                    occurred_at.ok_or_else(|| de::Error::missing_field("occurred_at"))?;
                let op_str = op_raw.ok_or_else(|| de::Error::missing_field("op"))?;

                let op = match op_str.as_str() {
                    "upsert" => {
                        if previous_external_id.is_some() {
                            return Err(de::Error::custom(
                                "`previous_external_id` is only allowed when `op` is `supersede`",
                            ));
                        }
                        PublicIngestOp::Upsert
                    }
                    "withdraw" => {
                        if previous_external_id.is_some() {
                            return Err(de::Error::custom(
                                "`previous_external_id` is only allowed when `op` is `supersede`",
                            ));
                        }
                        PublicIngestOp::Withdraw
                    }
                    "supersede" => {
                        let prev = previous_external_id
                            .flatten()
                            .ok_or_else(|| de::Error::missing_field("previous_external_id"))?;
                        PublicIngestOp::Supersede {
                            previous_external_id: prev,
                        }
                    }
                    other => {
                        return Err(de::Error::unknown_variant(
                            other,
                            &["upsert", "supersede", "withdraw"],
                        ));
                    }
                };

                Ok(PublicIngestRecord {
                    source,
                    speaker,
                    audience,
                    occurred_at,
                    content: content.flatten().unwrap_or_default(),
                    op,
                    ingested_by: ingested_by.flatten(),
                    media_type: media_type.flatten(),
                    model_policy: model_policy.flatten(),
                    thread_key: thread_key.flatten(),
                    reply_to: reply_to.flatten(),
                })
            }
        }

        deserializer.deserialize_map(RecordVisitor)
    }
}

impl PublicIngestRecord {
    /// Validates all public ingest contract invariants on this record.
    pub fn validate(&self) -> Result<()> {
        self.source.validate()?;
        self.speaker.validate()?;
        self.audience
            .validate(self.source.container_id.as_deref())?;
        self.op.validate(&self.source.external_id, &self.content)?;
        if let Some(mt) = &self.media_type {
            validate_nonempty_clean(mt, "media_type", "media_type must be nonempty")?;
        }
        if let Some(tk) = &self.thread_key {
            validate_nonempty_clean(tk, "thread_key", "thread_key must be nonempty")?;
        }
        if let Some(rt) = &self.reply_to {
            validate_nonempty_clean(rt, "reply_to", "reply_to must be nonempty")?;
        }
        Ok(())
    }

    /// Validates and serializes this record as a single compact JSONL line (without trailing newline).
    pub fn to_jsonl_line(&self) -> Result<String> {
        self.validate()?;
        serde_json::to_string(self)
            .map_err(|err| KpError::invalid_input_json("public_ingest_record", err.to_string()))
    }
}

/// Public JSONL container membership roster so `source_scopes` stays live as members join or
/// leave upstream containers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicContainerRoster {
    pub container_id: String,
    pub label: String,
    /// Resolved library `actor_id`s or external `namespace:id` identifiers.
    pub members: Vec<String>,
    pub all_library_members: bool,
    #[serde(deserialize_with = "deserialize_epoch_ms_flexible")]
    pub synced_at: EpochMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_container_id: Option<String>,
}

impl PublicContainerRoster {
    /// Validates container roster fields and member identifiers.
    pub fn validate(&self) -> Result<()> {
        validate_nonempty_clean(
            &self.container_id,
            "container_id",
            "container_id must be nonempty",
        )?;
        validate_nonempty_clean(&self.label, "label", "container label must be nonempty")?;
        for member in &self.members {
            validate_actor_or_external_id(member, "members")?;
        }
        if let Some(provider) = &self.provider {
            validate_nonempty_clean(provider, "provider", "provider must be nonempty")?;
        }
        if let Some(upstream) = &self.upstream_container_id {
            validate_nonempty_clean(
                upstream,
                "upstream_container_id",
                "upstream_container_id must be nonempty",
            )?;
        }
        Ok(())
    }

    /// Validates and serializes this container roster as a single compact JSONL line (without trailing newline).
    pub fn to_jsonl_line(&self) -> Result<String> {
        self.validate()?;
        serde_json::to_string(self)
            .map_err(|err| KpError::invalid_input_json("public_container_roster", err.to_string()))
    }
}

/// Any JSONL line in a public ingest stream ([`PublicContainerRoster`] or [`PublicIngestRecord`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum PublicIngestLine {
    Record(Box<PublicIngestRecord>),
    ContainerRoster(PublicContainerRoster),
}

impl<'de> Deserialize<'de> for PublicIngestLine {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let Value::Object(map) = value else {
            return Err(de::Error::custom("expected JSON object on JSONL line"));
        };
        if map.contains_key("container_id") && !map.contains_key("source") {
            serde_json::from_value::<PublicContainerRoster>(Value::Object(map))
                .map(Self::ContainerRoster)
                .map_err(de::Error::custom)
        } else {
            serde_json::from_value::<PublicIngestRecord>(Value::Object(map))
                .map(|rec| Self::Record(Box::new(rec)))
                .map_err(de::Error::custom)
        }
    }
}

impl PublicIngestLine {
    /// Validates the underlying record or container roster.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Record(record) => record.validate(),
            Self::ContainerRoster(roster) => roster.validate(),
        }
    }

    /// Validates and serializes this item as a single compact JSONL line (without trailing newline).
    pub fn to_jsonl_line(&self) -> Result<String> {
        self.validate()?;
        serde_json::to_string(self)
            .map_err(|err| KpError::invalid_input_json("public_ingest_line", err.to_string()))
    }
}

/// Parses and validates a multi-line public JSONL stream into [`PublicIngestLine`] items,
/// ignoring blank lines.
pub fn parse_public_jsonl_str(input: &str) -> Result<Vec<PublicIngestLine>> {
    let mut out = Vec::new();
    for (idx, raw_line) in input.lines().enumerate() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let line_ctx = || format!("jsonl:line:{}", idx + 1);
        let probe: Value = serde_json::from_str(trimmed)
            .map_err(|err| KpError::invalid_input_json(line_ctx(), err.to_string()))?;
        let Value::Object(map) = probe else {
            return Err(KpError::invalid_input_json(
                line_ctx(),
                "expected JSON object on JSONL line",
            ));
        };
        let item = if map.contains_key("container_id") && !map.contains_key("source") {
            let roster: PublicContainerRoster = serde_json::from_str(trimmed)
                .map_err(|err| KpError::invalid_input_json(line_ctx(), err.to_string()))?;
            PublicIngestLine::ContainerRoster(roster)
        } else {
            let record: PublicIngestRecord = serde_json::from_str(trimmed)
                .map_err(|err| KpError::invalid_input_json(line_ctx(), err.to_string()))?;
            PublicIngestLine::Record(Box::new(record))
        };
        item.validate()?;
        out.push(item);
    }
    Ok(out)
}

fn validate_nonempty_clean(
    value: &str,
    field: &'static str,
    empty_reason: &'static str,
) -> Result<()> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(KpError::invalid(field, empty_reason));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(KpError::invalid(
            field,
            "value must not contain control characters",
        ));
    }
    Ok(())
}

fn validate_actor_or_external_id(raw: &str, field: &'static str) -> Result<()> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.chars().any(char::is_control) {
        return Err(KpError::invalid(field, "identifier must be nonempty"));
    }
    if let Some((namespace, external_id)) = trimmed.split_once(':') {
        let ns = namespace.trim();
        let ext = external_id.trim();
        if ns.is_empty()
            || !ns
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
        {
            return Err(KpError::invalid(
                field,
                "external identity namespace must be ASCII alphanumeric or _-.",
            ));
        }
        if ext.is_empty() {
            return Err(KpError::invalid(
                field,
                "external identity id must be nonempty",
            ));
        }
        Ok(())
    } else {
        ActorId::new(trimmed).map(|_| ())
    }
}

fn deserialize_epoch_ms_flexible<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<EpochMs, D::Error> {
    struct EpochVisitor;

    impl Visitor<'_> for EpochVisitor {
        type Value = EpochMs;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an epoch millisecond integer or ISO-8601 / RFC 3339 timestamp string")
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
            Ok(normalize_numeric_epoch(v))
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
            let signed =
                i64::try_from(v).map_err(|_| E::custom("timestamp integer out of range"))?;
            Ok(normalize_numeric_epoch(signed))
        }

        fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
            parse_timestamp_ms_in_tz(v, &TimeZone::UTC).ok_or_else(|| {
                E::custom(format!(
                    "invalid timestamp `{v}` (expected epoch ms or ISO-8601)"
                ))
            })
        }

        fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<Self::Value, E> {
            self.visit_str(&v)
        }
    }

    deserializer.deserialize_any(EpochVisitor)
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    #[test]
    fn jsonl_serialization_and_roundtrip_parsing_cover_roster_and_ops() -> Result<()> {
        let alice: ActorId = "eng_alice".parse()?;
        let roster = PublicContainerRoster {
            container_id: "scope.ops_cutover".into(),
            label: "运维切换协作组".into(),
            members: vec!["eng_alice".into(), "chat:user_pm_li".into()],
            all_library_members: false,
            synced_at: EpochMs(1_795_245_000_000),
            provider: Some("chat".into()),
            upstream_container_id: Some("room_cutover".into()),
        };

        let upsert_rec = PublicIngestRecord {
            source: PublicIngestSource {
                kind: "chat".into(),
                uri: "chat://room_cutover/msg_1001".into(),
                external_id: "msg_1001".into(),
                container_id: Some("scope.ops_cutover".into()),
            },
            speaker: PublicSpeaker::new("chat:user_pm_li", Some("李四".into())),
            audience: PublicAudience::ContainerMembers,
            occurred_at: EpochMs(1_795_245_000_000),
            content: "下周四凌晨两点数据库主从切换".into(),
            op: PublicIngestOp::Upsert,
            ingested_by: Some(alice.clone()),
            media_type: Some("text/plain".into()),
            model_policy: None,
            thread_key: None,
            reply_to: None,
        };
        let upsert_val: Value = serde_json::from_str(&upsert_rec.to_jsonl_line()?)
            .map_err(|e| KpError::invalid_input_json("test", e.to_string()))?;
        assert_eq!(upsert_val["op"], "upsert");
        assert_eq!(upsert_val["audience"], "container_members");
        assert_eq!(upsert_val["speaker"]["id"], "chat:user_pm_li");
        assert_eq!(upsert_val["speaker"]["display_name"], "李四");
        assert!(upsert_val.get("model_policy").is_none());

        let supersede_rec = PublicIngestRecord {
            op: PublicIngestOp::Supersede {
                previous_external_id: "msg_1001".into(),
            },
            source: PublicIngestSource {
                external_id: "msg_1001@rev1".into(),
                ..upsert_rec.source.clone()
            },
            content: "下周四凌晨三点数据库主从切换".into(),
            ..upsert_rec.clone()
        };
        let sup_val: Value = serde_json::from_str(&supersede_rec.to_jsonl_line()?)
            .map_err(|e| KpError::invalid_input_json("test", e.to_string()))?;
        assert_eq!(sup_val["op"], "supersede");
        assert_eq!(sup_val["previous_external_id"], "msg_1001");

        let withdraw_rec = PublicIngestRecord {
            op: PublicIngestOp::Withdraw,
            content: String::new(),
            audience: PublicAudience::Explicit(vec![alice.clone()]),
            model_policy: Some(ModelRoutePolicy::RawOnly),
            ..upsert_rec.clone()
        };
        let wd_val: Value = serde_json::from_str(&withdraw_rec.to_jsonl_line()?)
            .map_err(|e| KpError::invalid_input_json("test", e.to_string()))?;
        assert_eq!(wd_val["op"], "withdraw");
        assert_eq!(wd_val["audience"]["explicit"][0], "eng_alice");

        let stream = format!(
            "\n{}\n{}\n\n{}\n{}\n",
            PublicIngestLine::ContainerRoster(roster.clone()).to_jsonl_line()?,
            PublicIngestLine::Record(Box::new(upsert_rec.clone())).to_jsonl_line()?,
            PublicIngestLine::Record(Box::new(supersede_rec.clone())).to_jsonl_line()?,
            PublicIngestLine::Record(Box::new(withdraw_rec.clone())).to_jsonl_line()?,
        );
        let parsed = parse_public_jsonl_str(&stream)?;
        assert_eq!(
            parsed,
            vec![
                PublicIngestLine::ContainerRoster(roster),
                PublicIngestLine::Record(Box::new(upsert_rec)),
                PublicIngestLine::Record(Box::new(supersede_rec)),
                PublicIngestLine::Record(Box::new(withdraw_rec)),
            ]
        );
        Ok(())
    }

    #[test]
    fn parses_flexible_iso8601_and_epoch_timestamps_and_speaker_forms() -> Result<()> {
        let alice: ActorId = "eng_alice".parse()?;
        let external_jsonl = r#"
{"container_id":"scope.reading_club","label":"读书分享组","members":["eng_alice","ext_ns:member_wang"],"all_library_members":false,"synced_at":"2026-11-21T07:16:20Z"}
{"source":{"kind":"manual","uri":"stdin://note/1","external_id":"note-1"},"speaker":"eng_alice","audience":{"explicit":["eng_alice"]},"occurred_at":"2026-11-21T15:16:20.250+08:00","content":"本周六读书会改在市民图书馆举办","op":"upsert"}
{"source":{"kind":"manual","uri":"stdin://note/2","external_id":"note-2"},"speaker":{"id":"ext_ns:member_wang","display_name":"  王五  "},"audience":{"explicit":["eng_alice"]},"occurred_at":1795245380,"content":"请自带笔记本做读书摘录","op":"upsert"}
{"source":{"kind":"manual","uri":"stdin://note/1","external_id":"note-1"},"speaker":"eng_alice","audience":{"explicit":["eng_alice"]},"occurred_at":1795245390000,"op":"withdraw"}
"#;
        let parsed = parse_public_jsonl_str(external_jsonl)?;
        assert_eq!(parsed.len(), 4);

        let PublicIngestLine::ContainerRoster(roster) = &parsed[0] else {
            panic!("expected ContainerRoster on line 1");
        };
        let PublicIngestLine::Record(r1) = &parsed[1] else {
            panic!("expected Record on line 2");
        };
        let PublicIngestLine::Record(r2) = &parsed[2] else {
            panic!("expected Record on line 3");
        };
        let PublicIngestLine::Record(r3) = &parsed[3] else {
            panic!("expected Record on line 4");
        };

        assert_eq!(r1.occurred_at.0 - roster.synced_at.0, 250);
        assert_eq!(r1.speaker, PublicSpeaker::new("eng_alice", None));
        assert_eq!(r1.audience, PublicAudience::Explicit(vec![alice]));
        assert_eq!(
            r2.speaker,
            PublicSpeaker::new("ext_ns:member_wang", Some("王五".into()))
        );
        assert_eq!(r2.occurred_at, EpochMs(1_795_245_380_000));
        assert_eq!(r3.op, PublicIngestOp::Withdraw);
        assert_eq!(r3.content, "");

        let bad_date = r#"{"source":{"kind":"manual","uri":"stdin://bad","external_id":"b1"},"speaker":"eng_alice","audience":{"explicit":["eng_alice"]},"occurred_at":"2026-02-30T00:00:00Z","content":"无效日期","op":"upsert"}"#;
        assert!(matches!(
            parse_public_jsonl_str(bad_date),
            Err(KpError::InvalidInputJson { .. })
        ));
        Ok(())
    }

    #[test]
    fn validates_upsert_supersede_withdraw_and_container_vs_explicit_audiences() -> Result<()> {
        // 1. `container_members` without `source.container_id` is rejected.
        let missing_container = r#"{"source":{"kind":"chat","uri":"chat://dm/1","external_id":"m1"},"speaker":"eng_alice","audience":"container_members","occurred_at":1795000000000,"content":"缺少所属容器","op":"upsert"}"#;
        assert_eq!(
            parse_public_jsonl_str(missing_container),
            Err(KpError::invalid(
                "audience",
                "container_members audience requires source.container_id"
            ))
        );

        // 2. Empty `explicit` audience is rejected.
        let empty_explicit = r#"{"source":{"kind":"manual","uri":"stdin://1","external_id":"m1"},"speaker":"eng_alice","audience":{"explicit":[]},"occurred_at":1795000000000,"content":"空受众名单","op":"upsert"}"#;
        assert_eq!(
            parse_public_jsonl_str(empty_explicit),
            Err(KpError::invalid(
                "audience",
                "explicit audience must list at least one actor"
            ))
        );

        // 3. `upsert` with blank content is rejected.
        let blank_upsert = r#"{"source":{"kind":"manual","uri":"stdin://1","external_id":"m1"},"speaker":"eng_alice","audience":{"explicit":["eng_alice"]},"occurred_at":1795000000000,"content":"   ","op":"upsert"}"#;
        assert_eq!(
            parse_public_jsonl_str(blank_upsert),
            Err(KpError::invalid(
                "content",
                "upsert record content must be nonempty"
            ))
        );

        // 4. `supersede` missing `previous_external_id`, blank `previous_external_id`, self-supersede, or blank content are all rejected.
        let missing_prev_json = r#"{"source":{"kind":"manual","uri":"stdin://1","external_id":"m2"},"speaker":"eng_alice","audience":{"explicit":["eng_alice"]},"occurred_at":1795000000000,"content":"修订版","op":"supersede"}"#;
        assert!(matches!(
            parse_public_jsonl_str(missing_prev_json),
            Err(KpError::InvalidInputJson { detail, .. }) if detail.contains("previous_external_id")
        ));

        let blank_prev = r#"{"source":{"kind":"manual","uri":"stdin://1","external_id":"m2"},"speaker":"eng_alice","audience":{"explicit":["eng_alice"]},"occurred_at":1795000000000,"content":"修订版","op":"supersede","previous_external_id":"  "}"#;
        assert_eq!(
            parse_public_jsonl_str(blank_prev),
            Err(KpError::invalid(
                "previous_external_id",
                "supersede requires nonempty previous_external_id"
            ))
        );

        let self_sup = r#"{"source":{"kind":"manual","uri":"stdin://1","external_id":"m1"},"speaker":"eng_alice","audience":{"explicit":["eng_alice"]},"occurred_at":1795000000000,"content":"修订版","op":"supersede","previous_external_id":"m1"}"#;
        assert_eq!(
            parse_public_jsonl_str(self_sup),
            Err(KpError::invalid(
                "previous_external_id",
                "supersede previous_external_id must differ from source.external_id"
            ))
        );

        let blank_sup_content = r#"{"source":{"kind":"manual","uri":"stdin://1","external_id":"m2"},"speaker":"eng_alice","audience":{"explicit":["eng_alice"]},"occurred_at":1795000000000,"content":"","op":"supersede","previous_external_id":"m1"}"#;
        assert_eq!(
            parse_public_jsonl_str(blank_sup_content),
            Err(KpError::invalid(
                "content",
                "supersede record content must be nonempty"
            ))
        );

        // 5. Invalid speaker identifiers (`":id"`, `"ns:"`, `"bad actor"`) are rejected.
        for bad_speaker in [":ext_1", "ext_ns:", "bad actor"] {
            let line = format!(
                r#"{{"source":{{"kind":"manual","uri":"stdin://1","external_id":"m1"}},"speaker":"{bad_speaker}","audience":{{"explicit":["eng_alice"]}},"occurred_at":1795000000000,"content":"测试发言人校验","op":"upsert"}}"#
            );
            assert!(
                matches!(
                    parse_public_jsonl_str(&line),
                    Err(KpError::InvalidInput { .. })
                ),
                "expected InvalidInput for speaker `{bad_speaker}`"
            );
        }

        Ok(())
    }

    #[test]
    fn rejects_unknown_fields_and_forbids_previous_external_id_outside_supersede() -> Result<()> {
        // 1. Typo on top-level `PublicIngestRecord` (`"model_polciy"` instead of `"model_policy"`):
        let typo_policy = r#"{"source":{"kind":"manual","uri":"stdin://1","external_id":"m1"},"speaker":"eng_alice","audience":{"explicit":["eng_alice"]},"occurred_at":1795000000000,"content":"拼写错误必须报错而非静默走云端","op":"upsert","model_polciy":{"mode":"raw_only"}}"#;
        assert!(matches!(
            parse_public_jsonl_str(typo_policy),
            Err(KpError::InvalidInputJson { detail, .. }) if detail.contains("unknown field `model_polciy`")
        ));

        // 2. Unknown fields on `PublicIngestSource`, `PublicSpeaker`, `PublicContainerRoster`, and `ModelRoutePolicy`:
        let unknown_in_source = r#"{"source":{"kind":"manual","uri":"stdin://1","external_id":"m1","extra_src":1},"speaker":"eng_alice","audience":{"explicit":["eng_alice"]},"occurred_at":1795000000000,"content":"正文","op":"upsert"}"#;
        assert!(matches!(
            parse_public_jsonl_str(unknown_in_source),
            Err(KpError::InvalidInputJson { detail, .. }) if detail.contains("unknown field `extra_src`")
        ));

        let unknown_in_speaker = r#"{"source":{"kind":"manual","uri":"stdin://1","external_id":"m1"},"speaker":{"id":"eng_alice","role":"owner"},"audience":{"explicit":["eng_alice"]},"occurred_at":1795000000000,"content":"正文","op":"upsert"}"#;
        assert!(matches!(
            parse_public_jsonl_str(unknown_in_speaker),
            Err(KpError::InvalidInputJson { detail, .. }) if detail.contains("unknown field `role`")
        ));

        let unknown_in_roster = r#"{"container_id":"scope.a","label":"组A","members":["eng_alice"],"all_library_members":false,"synced_at":1795000000000,"unexpected":true}"#;
        assert!(matches!(
            parse_public_jsonl_str(unknown_in_roster),
            Err(KpError::InvalidInputJson { detail, .. }) if detail.contains("unknown field `unexpected`")
        ));

        let unknown_in_policy = r#"{"source":{"kind":"manual","uri":"stdin://1","external_id":"m1"},"speaker":"eng_alice","audience":{"explicit":["eng_alice"]},"occurred_at":1795000000000,"content":"正文","op":"upsert","model_policy":{"mode":"raw_only","extra":1}}"#;
        assert!(matches!(
            parse_public_jsonl_str(unknown_in_policy),
            Err(KpError::InvalidInputJson { .. })
        ));

        // 3. `previous_external_id` is forbidden when `op == "upsert"` or `op == "withdraw"` (even if null):
        for disallowed in [
            r#"{"source":{"kind":"manual","uri":"stdin://1","external_id":"m2"},"speaker":"eng_alice","audience":{"explicit":["eng_alice"]},"occurred_at":1795000000000,"content":"正文","op":"upsert","previous_external_id":"m1"}"#,
            r#"{"source":{"kind":"manual","uri":"stdin://1","external_id":"m2"},"speaker":"eng_alice","audience":{"explicit":["eng_alice"]},"occurred_at":1795000000000,"content":"正文","op":"upsert","previous_external_id":null}"#,
            r#"{"source":{"kind":"manual","uri":"stdin://1","external_id":"m2"},"speaker":"eng_alice","audience":{"explicit":["eng_alice"]},"occurred_at":1795000000000,"op":"withdraw","previous_external_id":"m1"}"#,
        ] {
            assert!(
                matches!(
                    parse_public_jsonl_str(disallowed),
                    Err(KpError::InvalidInputJson { detail, .. }) if detail.contains("previous_external_id")
                ),
                "expected previous_external_id rejection for: {disallowed}"
            );
        }

        // 4. Explicit `"model_policy": null` deserializes cleanly to `None`, and `"content": null`
        //    is allowed on `"withdraw"` while strictly rejected on `"upsert"` and `"supersede"`:
        let null_policy = r#"{"source":{"kind":"manual","uri":"stdin://1","external_id":"m1"},"speaker":{"id":"eng_alice","display_name":null},"audience":{"explicit":["eng_alice"]},"occurred_at":1795000000000,"content":"继承全局模型","op":"upsert","model_policy":null,"ingested_by":null,"media_type":null}"#;
        let parsed = parse_public_jsonl_str(null_policy)?;
        assert_eq!(parsed.len(), 1);
        let PublicIngestLine::Record(rec) = &parsed[0] else {
            return Err(KpError::invalid(
                "test",
                "expected PublicIngestLine::Record",
            ));
        };
        assert_eq!(rec.model_policy, None);
        assert_eq!(rec.speaker.display_name, None);
        assert_eq!(rec.ingested_by, None);
        assert_eq!(rec.media_type, None);

        let withdraw_null_content = r#"{"source":{"kind":"manual","uri":"stdin://1","external_id":"m1"},"speaker":"eng_alice","audience":{"explicit":["eng_alice"]},"occurred_at":1795000000000,"content":null,"op":"withdraw"}"#;
        let parsed_wd = parse_public_jsonl_str(withdraw_null_content)?;
        assert_eq!(parsed_wd.len(), 1);
        let PublicIngestLine::Record(wd_rec) = &parsed_wd[0] else {
            return Err(KpError::invalid(
                "test",
                "expected PublicIngestLine::Record",
            ));
        };
        assert_eq!(wd_rec.op, PublicIngestOp::Withdraw);
        assert_eq!(wd_rec.content, "");

        let upsert_null_content = r#"{"source":{"kind":"manual","uri":"stdin://1","external_id":"m1"},"speaker":"eng_alice","audience":{"explicit":["eng_alice"]},"occurred_at":1795000000000,"content":null,"op":"upsert"}"#;
        assert_eq!(
            parse_public_jsonl_str(upsert_null_content),
            Err(KpError::invalid(
                "content",
                "upsert record content must be nonempty"
            ))
        );

        let supersede_null_content = r#"{"source":{"kind":"manual","uri":"stdin://1","external_id":"m2"},"speaker":"eng_alice","audience":{"explicit":["eng_alice"]},"occurred_at":1795000000000,"content":null,"op":"supersede","previous_external_id":"m1"}"#;
        assert_eq!(
            parse_public_jsonl_str(supersede_null_content),
            Err(KpError::invalid(
                "content",
                "supersede record content must be nonempty"
            ))
        );

        Ok(())
    }
}
