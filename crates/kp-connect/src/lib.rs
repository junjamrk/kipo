//! Decoupled external connector crate (`Rev 5 §0.1 & §0.2`) for Feishu (`lark-cli`),
//! GitHub (`reqwest` / `gh`), and local agent session backfill.
//!
//! Architectural invariants enforced across every connector:
//! 1. Zero dependency on the core storage engine: connectors persist cursors and scope configurations in their
//!    own atomic JSON state store ([`ConnectorStateStore`], e.g. `~/.kp/connectors/state.json`) and
//!    emit standard JSONL records ([`PublicIngestRecord`] + [`PublicContainerRoster`]) for `kp ingest`.
//! 2. Explicit user-selected scope allowlists (nothing syncs by default).
//! 3. External identity proposals confirmed by the library owner before merging actors.
//! 4. Live container membership synced via [`PublicContainerRoster`] / [`SourceScopeRoster`]; facts
//!    record only `container_id` (`source_scope_id`) so read-time visibility and publish ceiling
//!    checks follow live membership.
//! 5. Speaker (`speaker_external_id`) and importer (`ingested_by`) are recorded separately.
//! 6. Edits map to `Supersede`; recalls/deletions map to `Withdraw`.
//! 7. Connectors inherit the global model setting by default (`ModelRoutePolicy::HostCli`), with
//!    `ModelRoutePolicy::RawOnly` and `ModelRoutePolicy::NamedEndpoint` available as explicit
//!    advanced overrides (`Rev 5 §0.2.4`).

pub mod envelope;
pub mod feishu;
pub mod github;
pub mod runner;
pub mod sessions;
pub mod state;
pub mod subprocess;

use std::collections::{BTreeMap, BTreeSet};

pub use kp_model::{
    parse_public_jsonl_str, PublicAudience, PublicContainerRoster, PublicIngestLine,
    PublicIngestOp, PublicIngestRecord, PublicIngestSource, PublicSpeaker,
};
use kp_model::{
    ActorId, AudienceSpec, BoxFuture, ConnectorId, EpochMs, KpError, ModelRoutePolicy,
    SourceOriginMeta, SourceScope, SourceScopeId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub use feishu::feishu_enable_notice;
pub use runner::{
    commit_staged_cursors, list_scopes_in_state, parse_connector_model_policy,
    parse_identity_mapping, pipe_jsonl_to_kp_ingest, poll_enabled_scopes_staged,
    set_scope_enabled_in_state, upsert_scope_in_state, ScopeUpsertSpec, StagedConnectorSync,
};
pub use state::{
    connector_scope_state_key, ConnectorStateSnapshot, ConnectorStateStore, DEFAULT_STATE_REL_PATH,
    ENV_CONNECTOR_STATE_PATH, STATE_SCHEMA_VERSION,
};

#[derive(Debug, Error)]
pub enum ConnectError {
    #[error(transparent)]
    Model(#[from] KpError),
    #[error("JSON serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("I/O operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("connector scope `{0}` is not enabled")]
    ScopeDisabled(String),
    #[error("read-only safety violation: {0}")]
    ReadOnlyViolation(String),
    #[error("rate limited (retry after {retry_after_ms}ms): {reason}")]
    RateLimited { retry_after_ms: u64, reason: String },
    #[error("transport unavailable: {0}")]
    Transport(String),
}

pub type Result<T> = std::result::Result<T, ConnectError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConnectorKind {
    Feishu,
    GitHub,
    AgentSession,
}

impl ConnectorKind {
    /// Canonical lowercase provider slug used in `PublicIngestRecord.source.kind` and namespaces.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Feishu => "feishu",
            Self::GitHub => "github",
            Self::AgentSession => "agent-session",
        }
    }

    /// Connectors inherit the global model setting by default (`Rev 5 §0.2.4`);
    /// `ModelRoutePolicy::RawOnly` remains an explicit advanced override per scope.
    pub fn default_model_policy(self) -> ModelRoutePolicy {
        match self {
            Self::Feishu | Self::GitHub | Self::AgentSession => ModelRoutePolicy::HostCli,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SourceEventOp {
    Upsert,
    Supersede { previous_external_key: String },
    Withdraw,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorScope {
    pub connector_id: ConnectorId,
    pub source_scope_id: SourceScopeId,
    pub upstream_container_id: String,
    pub label: String,
    pub enabled: bool,
    #[serde(default)]
    pub model_policy: ModelRoutePolicy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorCursor {
    pub cursor_token: Option<String>,
    pub high_water_ms: Option<EpochMs>,
    pub updated_at: EpochMs,
}

impl Default for ConnectorCursor {
    fn default() -> Self {
        Self {
            cursor_token: None,
            high_water_ms: None,
            updated_at: EpochMs(0),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalIdentityProposal {
    pub provider: String,
    pub external_id: String,
    pub display_name: String,
    pub proposed_actor: Option<ActorId>,
}

/// Live container membership roster synced into `source_scopes` during polling.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceScopeRoster {
    pub scope_id: SourceScopeId,
    pub provider: String,
    pub external_container_id: String,
    pub label: String,
    pub external_member_ids: Vec<String>,
    pub all_library_members: bool,
    pub synced_at: EpochMs,
}

impl SourceScopeRoster {
    /// Resolves external container member IDs against confirmed `(provider, external_id) -> ActorId`
    /// mappings into a live `SourceScope` for read-time visibility and publish ceiling checks.
    pub fn into_source_scope(
        &self,
        confirmed_identities: &BTreeMap<(String, String), ActorId>,
    ) -> SourceScope {
        let current_audience = if self.all_library_members {
            AudienceSpec::AllMembers
        } else {
            let resolved: BTreeSet<ActorId> = self
                .external_member_ids
                .iter()
                .filter_map(|ext_id| {
                    confirmed_identities
                        .get(&(self.provider.clone(), ext_id.clone()))
                        .cloned()
                })
                .collect();
            AudienceSpec::Explicit(resolved)
        };
        SourceScope {
            scope_id: self.scope_id.clone(),
            provider: self.provider.clone(),
            external_container_id: self.external_container_id.clone(),
            label: self.label.clone(),
            current_audience,
            synced_at: self.synced_at,
        }
    }
}

/// Normalized inbound record emitted by any connector; stores `source_scope_id` rather than a static ACL snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestedSourceEvent {
    pub external_key: String,
    pub uri: String,
    pub media_type: String,
    pub speaker_external_id: String,
    pub speaker_display_name: String,
    pub ingested_by: ActorId,
    pub source_scope_id: SourceScopeId,
    pub content: String,
    pub event_op: SourceEventOp,
    pub occurred_at: EpochMs,
    pub model_policy: ModelRoutePolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

impl IngestedSourceEvent {
    /// Builds `SourceOriginMeta` once `speaker_external_id` has been resolved to an `ActorId`.
    pub fn into_origin_meta(&self, resolved_speaker: ActorId) -> SourceOriginMeta {
        SourceOriginMeta {
            speaker: resolved_speaker,
            ingested_by: self.ingested_by.clone(),
            source_scope_id: self.source_scope_id.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollBatch {
    pub scope_rosters: Vec<SourceScopeRoster>,
    pub events: Vec<IngestedSourceEvent>,
    pub identity_proposals: Vec<ExternalIdentityProposal>,
    pub next_cursor: ConnectorCursor,
    pub skipped_low_signal: usize,
}

/// Deterministic backfill filter skipping zero-information acknowledgements to save host quota.
pub fn is_low_signal_ack(text: &str) -> bool {
    let trimmed = text
        .trim()
        .trim_matches(['。', '！', '!', '.', '~', '～', ' '])
        .to_lowercase();
    matches!(
        trimmed.as_str(),
        "" | "好"
            | "好的"
            | "收到"
            | "明白"
            | "已阅"
            | "谢谢"
            | "ok"
            | "okay"
            | "+1"
            | "ack"
            | "lgtm"
            | "got it"
            | "thanks"
    )
}

/// Pluggable external connector source interface.
pub trait Connector: Send + Sync {
    fn kind(&self) -> ConnectorKind;

    fn poll_incremental<'a>(
        &'a self,
        scope: &'a ConnectorScope,
        cursor: &'a ConnectorCursor,
        ingested_by: &'a ActorId,
    ) -> BoxFuture<'a, Result<PollBatch>>;
}

/// Strictly read-only Feishu transport contract (backed by `lark-cli`, zero write methods).
/// Sync ports WeKnora (MIT) checkpoints/deletion reconciliation/`obj_edit_time`/99991400 cooldown
/// plus recent-window message re-scan (`deleted`/`updated` flags -> withdraw/supersede).
pub trait FeishuTransport: Send + Sync {
    fn list_chat_messages<'a>(
        &'a self,
        chat_id: &'a str,
        start_time_sec: Option<i64>,
        end_time_sec: Option<i64>,
        page_token: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Value>>;

    /// Lists messages inside a Feishu topic/reply thread (`GET /open-apis/im/v1/messages`
    /// with `container_id_type=thread`), since `im +chat-messages-list` requests
    /// `only_thread_root_messages=true` and omits unexpanded thread replies.
    fn list_thread_messages<'a>(
        &'a self,
        thread_id: &'a str,
        page_token: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Value>> {
        let _ = (thread_id, page_token);
        Box::pin(async {
            Ok(serde_json::json!({
                "ok": true,
                "identity": "user",
                "data": { "items": [], "has_more": false }
            }))
        })
    }

    fn list_chat_members<'a>(
        &'a self,
        chat_id: &'a str,
        page_token: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Value>>;

    fn fetch_document<'a>(&'a self, doc_token: &'a str) -> BoxFuture<'a, Result<Value>>;

    fn list_wiki_nodes<'a>(
        &'a self,
        space_id: &'a str,
        parent_node_token: Option<&'a str>,
        page_token: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Value>>;

    fn fetch_minutes_transcript<'a>(
        &'a self,
        minute_token: &'a str,
    ) -> BoxFuture<'a, Result<Value>>;

    fn list_calendar_events<'a>(
        &'a self,
        calendar_id: &'a str,
        start: EpochMs,
        end: EpochMs,
    ) -> BoxFuture<'a, Result<Value>>;

    fn list_permission_members<'a>(
        &'a self,
        resource_type: &'a str,
        resource_token: &'a str,
    ) -> BoxFuture<'a, Result<Value>>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubConditionalPage {
    pub body: Value,
    pub etag: Option<String>,
    pub rate_limit_remaining: Option<u32>,
    pub rate_limit_reset_epoch_sec: Option<i64>,
}

/// Strictly read-only GitHub transport contract with conditional `ETag`, `since` window (RAGFlow Apache-2.0),
/// and rate-limit backoff headers (Onyx MIT).
pub trait GitHubTransport: Send + Sync {
    fn get_json<'a>(
        &'a self,
        endpoint: &'a str,
        etag: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Option<GitHubConditionalPage>>>;
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use kp_model::{AudienceSpec, SourceScope};

    use super::*;

    #[test]
    fn low_signal_filter_skips_bare_acks_without_dropping_substantive_messages() {
        assert!(is_low_signal_ack("好的！"));
        assert!(is_low_signal_ack("  收到  "));
        assert!(is_low_signal_ack("OK."));
        assert!(!is_low_signal_ack("好的，下周四凌晨两点数据库主从切换。"));
        assert!(!is_low_signal_ack("主从切换窗口禁止跑批处理任务"));
    }

    #[test]
    fn feishu_inherits_global_model_with_one_line_notice_and_binds_live_source_scope() -> Result<()>
    {
        assert_eq!(
            ConnectorKind::Feishu.default_model_policy(),
            ModelRoutePolicy::HostCli
        );
        assert_eq!(
            ConnectorKind::GitHub.default_model_policy(),
            ModelRoutePolicy::HostCli
        );
        assert_eq!(
            ConnectorKind::AgentSession.default_model_policy(),
            ModelRoutePolicy::HostCli
        );
        assert_eq!(
            feishu_enable_notice("Codex / OpenAI"),
            "提示：同步内容将使用当前全局模型处理（当前：Codex / OpenAI），如需更改或仅存原文请见 kp config"
        );

        let pm: ActorId = "pm_li".parse()?;
        let eng: ActorId = "eng_alice".parse()?;
        let outsider: ActorId = "carol".parse()?;
        let scope_id: SourceScopeId = "feishu.chat.oc_release".parse()?;
        let event = IngestedSourceEvent {
            external_key: "om_1001".into(),
            uri: "feishu://chat/oc_release/om_1001".into(),
            media_type: "text/plain".into(),
            speaker_external_id: "ou_pm".into(),
            speaker_display_name: "李四".into(),
            ingested_by: eng.clone(),
            source_scope_id: scope_id.clone(),
            content: "下周四凌晨两点数据库主从切换。".into(),
            event_op: SourceEventOp::Upsert,
            occurred_at: EpochMs(1_795_000_000_000),
            model_policy: ConnectorKind::Feishu.default_model_policy(),
            thread_key: None,
            reply_to: None,
        };
        let meta = event.into_origin_meta(pm.clone());
        assert_eq!(meta.speaker, pm);
        assert_eq!(meta.ingested_by, eng);
        assert_eq!(meta.source_scope_id, scope_id);

        let live_scope = SourceScope {
            scope_id,
            provider: "feishu".into(),
            external_container_id: "oc_release".into(),
            label: "发布群".into(),
            current_audience: AudienceSpec::Explicit(BTreeSet::from([pm.clone(), eng.clone()])),
            synced_at: EpochMs(1_795_000_000_000),
        };
        let all_members = BTreeSet::from([pm, eng, outsider.clone()]);
        assert!(live_scope
            .validate_publish_audience(
                &AudienceSpec::Explicit(BTreeSet::from([outsider])),
                &all_members,
            )
            .is_err());
        Ok(())
    }
}
