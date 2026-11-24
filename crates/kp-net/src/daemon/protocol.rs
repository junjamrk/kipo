use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use kp_model::{ActorId, BoxFuture, EpochMs, FactId, LibraryId, LibraryMode, SourceId};
use kp_retrieve::{MAX_PROMPT_CONTEXT_ITEMS, MAX_PROMPT_CONTEXT_TOKENS};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncBufReadExt;

use super::runtime_dir::daemon_socket_path;
use crate::{NetError, Result};

/// Maximum byte length of a single newline-delimited JSON RPC frame on server and client (8 MiB).
pub const MAX_DAEMON_FRAME_BYTES: usize = 8 * 1024 * 1024;

pub(super) const DEFAULT_MAX_FRAME_BYTES: usize = MAX_DAEMON_FRAME_BYTES;
pub(super) const DEFAULT_CONTEXT_MAX_TOKENS: usize = MAX_PROMPT_CONTEXT_TOKENS;
pub(super) const DEFAULT_CONTEXT_MAX_ITEMS: usize = MAX_PROMPT_CONTEXT_ITEMS;

fn default_context_max_tokens() -> usize {
    DEFAULT_CONTEXT_MAX_TOKENS
}

fn default_context_max_items() -> usize {
    DEFAULT_CONTEXT_MAX_ITEMS
}

/// Configuration for the optional `127.0.0.1` read-only 4-column Web Inspector hosted by `kp daemon`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebInspectorConfig {
    pub bind_addr: SocketAddr,
    pub viewer: ActorId,
}

impl WebInspectorConfig {
    /// Creates a loopback Web Inspector configuration bound to `127.0.0.1:<port>` for `viewer`.
    pub fn loopback(port: u16, viewer: ActorId) -> Self {
        Self {
            bind_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
            viewer,
        }
    }
}

/// Active Web Inspector capability-token session returned by `StartWebInspector`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebInspectorSession {
    pub url: String,
    pub bind_addr: String,
    pub viewer: ActorId,
}

#[derive(Clone, Debug)]
pub struct DaemonConfig {
    pub socket_path: PathBuf,
    /// Quiet period after the latest `NotifyCursor` before running idle batch extraction.
    pub idle_extract_window: Duration,
    /// Optional inactivity timeout after which the daemon exits cleanly once all queues and in-flight work finish.
    pub idle_shutdown_timeout: Option<Duration>,
    /// Maximum byte length of a single newline-delimited JSON RPC frame.
    pub max_frame_bytes: usize,
    /// Optional `127.0.0.1` read-only Web Inspector to start alongside the UDS listener.
    pub web_inspector: Option<WebInspectorConfig>,
}

impl DaemonConfig {
    pub fn for_library(library_dir: &Path) -> Result<Self> {
        Ok(Self {
            socket_path: daemon_socket_path(library_dir)?,
            idle_extract_window: Duration::from_secs(60),
            idle_shutdown_timeout: None,
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            web_inspector: None,
        })
    }

    pub fn with_idle_shutdown_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.idle_shutdown_timeout = timeout;
        self
    }

    pub fn with_web_inspector(mut self, web_inspector: Option<WebInspectorConfig>) -> Self {
        self.web_inspector = web_inspector;
        self
    }
}

/// Incremental cursor update emitted by `Stop` / `SessionEnd` hooks (`< 5ms` fire-and-ack).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorNotification {
    pub session_id: String,
    #[serde(default)]
    pub client_kind: String,
    #[serde(default)]
    pub transcript_path: Option<String>,
    pub byte_offset: u64,
    pub line_count: u64,
    #[serde(default)]
    pub prefix_sha256: String,
    pub actor: ActorId,
    pub at: EpochMs,
    /// When `true` (`SessionEnd`), flushes idle batch extraction immediately without waiting for quiet window.
    #[serde(default)]
    pub session_ended: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryContextRequest {
    pub prompt: String,
    pub viewer: ActorId,
    pub at: EpochMs,
    #[serde(default = "default_context_max_tokens")]
    pub max_tokens: usize,
    #[serde(default = "default_context_max_items")]
    pub max_items: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_zone: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryContextResult {
    /// Empty when no candidate clears the relevance threshold (zero prompt bloat).
    pub additional_context: String,
    pub item_count: usize,
    pub estimated_tokens: usize,
    pub cited_fact_ids: Vec<FactId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreToolCheckRequest {
    pub tool_name: String,
    #[serde(default)]
    pub tool_input: Value,
    #[serde(default)]
    pub cwd: Option<String>,
    pub viewer: ActorId,
    pub at: EpochMs,
    #[serde(default)]
    pub client_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_zone: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPermissionDecision {
    Allow,
    Ask,
    Deny,
}

impl ToolPermissionDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreToolCheckResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_decision: Option<ToolPermissionDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_decision_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched_fact_ids: Vec<FactId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_refs: Vec<String>,
}

impl PreToolCheckResult {
    /// Formats hook stdout JSON compatible with both Claude Code and Codex `PreToolUse` schemas.
    /// Emits `{}` when silent so normal tool calls experience zero friction.
    pub fn to_hook_json(&self) -> Value {
        let Some(decision) = self.permission_decision else {
            return json!({});
        };
        let reason = self.permission_decision_reason.clone().unwrap_or_default();
        json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": decision.as_str(),
                "permissionDecisionReason": reason,
            }
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestJsonlRequest {
    pub jsonl: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<ActorId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<EpochMs>,
    #[serde(default)]
    pub extract_now: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestJsonlResult {
    pub rosters_applied: usize,
    pub records_upserted: usize,
    pub records_superseded: usize,
    pub records_withdrawn: usize,
    pub jobs_enqueued: usize,
    #[serde(default)]
    pub source_ids: Vec<SourceId>,
}

impl IngestJsonlResult {
    /// Accumulates counts and deduplicated `source_ids` across multi-frame JSONL sub-batches.
    pub fn merge(&mut self, other: Self) {
        self.rosters_applied = self.rosters_applied.saturating_add(other.rosters_applied);
        self.records_upserted = self.records_upserted.saturating_add(other.records_upserted);
        self.records_superseded = self
            .records_superseded
            .saturating_add(other.records_superseded);
        self.records_withdrawn = self
            .records_withdrawn
            .saturating_add(other.records_withdrawn);
        self.jobs_enqueued = self.jobs_enqueued.saturating_add(other.jobs_enqueued);
        for sid in other.source_ids {
            if !self.source_ids.contains(&sid) {
                self.source_ids.push(sid);
            }
        }
    }

    /// Returns `true` when any container roster or ingest record mutated library state.
    pub fn has_mutations(&self) -> bool {
        self.rosters_applied > 0
            || self.records_upserted > 0
            || self.records_superseded > 0
            || self.records_withdrawn > 0
            || self.jobs_enqueued > 0
            || !self.source_ids.is_empty()
    }
}

pub use crate::DaemonEmbedderRuntimeStatus;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub library_id: LibraryId,
    pub name: String,
    pub mode: LibraryMode,
    pub owner_actor: ActorId,
    pub facts: u64,
    pub sources: u64,
    pub tracked_sessions: u64,
    pub pending_extractions: u64,
    pub uptime_ms: u64,
    #[serde(default)]
    pub embedder: DaemonEmbedderRuntimeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_inspector_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_inspector_viewer: Option<ActorId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DaemonRequest {
    Ping,
    Status,
    NotifyCursor(CursorNotification),
    QueryContext(QueryContextRequest),
    PreToolCheck(PreToolCheckRequest),
    IngestJsonl(IngestJsonlRequest),
    StartWebInspector {
        port: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        viewer: Option<ActorId>,
    },
    TriggerSync,
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DaemonResponse {
    Pong {
        library_id: LibraryId,
        uptime_ms: u64,
    },
    Status(DaemonStatus),
    CursorAck {
        queued_extraction: bool,
        last_extracted_offset: u64,
    },
    Context(QueryContextResult),
    PreTool(PreToolCheckResult),
    Ingested(IngestJsonlResult),
    WebInspector {
        url: String,
        bind_addr: String,
        viewer: ActorId,
    },
    SyncTriggered,
    ShuttingDown,
    Error {
        message: String,
    },
}

/// Coalesced session slice handed to `DaemonHandler::extract_session_batch` after idle quiet window
/// or on `SessionEnd`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdleSessionBatch {
    pub session_id: String,
    pub client_kind: String,
    pub transcript_path: Option<String>,
    pub start_byte_offset: u64,
    pub end_byte_offset: u64,
    pub line_count: u64,
    pub prefix_sha256: String,
    pub actor: ActorId,
    pub at: EpochMs,
    pub session_ended: bool,
}

/// Pluggable background extraction and community sync interface allowing `kipo` (`kp`) to attach
/// out-of-session LLM batch extraction and automatic community sync/derivation without cyclic dependencies.
pub trait DaemonHandler: Send + Sync + 'static {
    fn extract_session_batch<'a>(
        &'a self,
        library_dir: &'a Path,
        batch: IdleSessionBatch,
    ) -> BoxFuture<'a, Result<Option<u64>>> {
        let _ = (library_dir, batch);
        Box::pin(async { Ok(None) })
    }

    fn drain_queued_jobs<'a>(
        &'a self,
        library_dir: &'a Path,
        at: EpochMs,
    ) -> BoxFuture<'a, Result<bool>> {
        let _ = (library_dir, at);
        Box::pin(async { Ok(false) })
    }

    fn sync_and_derive<'a>(
        &'a self,
        library_dir: &'a Path,
        at: EpochMs,
    ) -> BoxFuture<'a, Result<usize>> {
        let _ = (library_dir, at);
        Box::pin(async { Ok(0) })
    }

    fn ingest_jsonl<'a>(
        &'a self,
        library_dir: &'a Path,
        request: IngestJsonlRequest,
    ) -> BoxFuture<'a, Result<IngestJsonlResult>> {
        let _ = (library_dir, request);
        Box::pin(async {
            Err(NetError::Daemon(
                "public JSONL ingest handler is not configured on this daemon".into(),
            ))
        })
    }

    fn embedder_status(&self) -> DaemonEmbedderRuntimeStatus {
        DaemonEmbedderRuntimeStatus::default()
    }

    fn refresh_store_embedder(&self, _library_dir: &Path, _store: &mut kp_store::Store) {}
}

pub(super) async fn read_bounded_line<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    out: &mut Vec<u8>,
    max_bytes: usize,
) -> Result<usize> {
    let mut total = 0usize;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(total);
        }
        if let Some(newline_pos) = available.iter().position(|&b| b == b'\n') {
            let take = newline_pos + 1;
            if total.saturating_add(take) > max_bytes {
                reader.consume(take);
                return Err(NetError::Invalid("daemon frame exceeds max_frame_bytes"));
            }
            out.extend_from_slice(&available[..take]);
            reader.consume(take);
            return Ok(total + take);
        }
        let len = available.len();
        if total.saturating_add(len) > max_bytes {
            reader.consume(len);
            return Err(NetError::Invalid("daemon frame exceeds max_frame_bytes"));
        }
        out.extend_from_slice(available);
        reader.consume(len);
        total += len;
    }
}

pub(super) fn trim_ascii_newline(mut slice: &[u8]) -> &[u8] {
    while let Some((&last, rest)) = slice.split_last() {
        if last == b'\n' || last == b'\r' || last == b' ' || last == b'\t' {
            slice = rest;
        } else {
            break;
        }
    }
    slice
}
