mod client;
mod protocol;
mod runtime_dir;
mod scheduler;
mod server;
mod web;

#[cfg(test)]
mod tests;

pub use client::{DaemonClient, DaemonSyncClient};
pub use protocol::{
    CursorNotification, DaemonConfig, DaemonEmbedderRuntimeStatus, DaemonHandler, DaemonRequest,
    DaemonResponse, DaemonStatus, IdleSessionBatch, IngestJsonlRequest, IngestJsonlResult,
    PreToolCheckRequest, PreToolCheckResult, QueryContextRequest, QueryContextResult,
    ToolPermissionDecision, WebInspectorConfig, WebInspectorSession, MAX_DAEMON_FRAME_BYTES,
};
pub(crate) use runtime_dir::ensure_socket_parent_dir_with_uid;
pub use runtime_dir::{
    current_process_uid, daemon_lock_path_for_socket, daemon_runtime_dir, daemon_socket_path,
    is_daemon_lock_held, is_socket_lock_held, validate_socket_path,
    verify_socket_parent_dir_with_uid, verify_std_stream_peer_uid, verify_stream_peer_uid,
    MAX_UDS_PATH_LEN,
};
pub use server::{DaemonHandle, DaemonServer};
pub use web::{validate_loopback_inspector_addr, INSPECTOR_COOKIE_NAME, INSPECTOR_CSP};
