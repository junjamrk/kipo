//! Authenticated community protocols and reliable, provenance-preserving synchronization.
mod client;
pub mod communities;
#[cfg(unix)]
pub mod daemon;
pub mod policy;
mod protocol;
mod server;
mod service;

pub use client::{
    auto_publish_and_sync, publish, queue_withdraw, reconcile_remote_origins, sync,
    CommunityClient, PublishOptions, PublishResult, SyncResult,
};
pub use communities::{
    communities_config_path, load_joined_communities, remove_joined_community,
    save_joined_communities, upsert_joined_community, JoinedCommunity, COMMUNITIES_CONFIG_FILE,
};
#[cfg(unix)]
pub use daemon::{
    current_process_uid, daemon_lock_path_for_socket, daemon_runtime_dir, daemon_socket_path,
    is_daemon_lock_held, is_socket_lock_held, validate_loopback_inspector_addr,
    validate_socket_path, verify_socket_parent_dir_with_uid, verify_std_stream_peer_uid,
    verify_stream_peer_uid, CursorNotification, DaemonClient, DaemonConfig, DaemonHandle,
    DaemonHandler, DaemonRequest, DaemonResponse, DaemonServer, DaemonStatus, DaemonSyncClient,
    IdleSessionBatch, IngestJsonlRequest, IngestJsonlResult, PreToolCheckRequest,
    PreToolCheckResult, QueryContextRequest, QueryContextResult, ToolPermissionDecision,
    WebInspectorConfig, WebInspectorSession, MAX_DAEMON_FRAME_BYTES, MAX_UDS_PATH_LEN,
};
pub use server::router;
#[cfg(unix)]
pub use server::{serve_unix, UnixHttpHandle};
pub use service::{
    CommunityChangeFeed, PublishRequest, PurgeRequest, SynthesizeRequest, TargetRequest,
};

/// Observable runtime status of the daemon's active embedding model and any background model
/// download/warmup task (`kp daemon status`). Exported unconditionally across all target platforms.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DaemonEmbedderRuntimeStatus {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub download_pct: Option<u8>,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error(transparent)]
    Store(#[from] kp_store::StoreError),
    #[error(transparent)]
    Model(#[from] kp_model::KpError),
    #[error(transparent)]
    Retrieve(#[from] kp_retrieve::RetrieveError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid JSON")]
    Json(#[from] serde_json::Error),
    #[error("transport failed")]
    Transport(#[source] reqwest::Error),
    #[error("remote returned HTTP {0}")]
    Http(u16),
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("policy violation: {0}")]
    Policy(String),
    #[error("not found")]
    NotFound,
    #[error("invalid request: {0}")]
    Invalid(&'static str),
    #[error("daemon error: {0}")]
    Daemon(String),
    #[error("server unavailable")]
    Unavailable,
}
pub type Result<T> = std::result::Result<T, NetError>;
