use std::io::{BufRead, BufReader as StdBufReader, Read as StdRead, Write as StdWrite};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kp_model::{ActorId, LibraryId};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use super::protocol::{
    read_bounded_line, trim_ascii_newline, CursorNotification, DaemonRequest, DaemonResponse,
    DaemonStatus, IngestJsonlRequest, IngestJsonlResult, PreToolCheckRequest, PreToolCheckResult,
    QueryContextRequest, QueryContextResult, WebInspectorSession, DEFAULT_MAX_FRAME_BYTES,
};
use super::runtime_dir::{
    current_process_uid, daemon_socket_path, validate_socket_path,
    verify_socket_parent_dir_with_uid, verify_std_stream_peer_uid, verify_stream_peer_uid,
};
#[cfg(any(test, feature = "test-inproc-uds"))]
use super::server::daemon_socket_registry;
use crate::{NetError, Result};

fn connect_daemon_sync_stream(
    socket_path: &Path,
    expected_dir_uid: u32,
    expected_peer_uid: u32,
) -> Result<StdUnixStream> {
    verify_socket_parent_dir_with_uid(socket_path, expected_dir_uid)?;
    let stream = match StdUnixStream::connect(socket_path) {
        Ok(stream) => stream,
        #[cfg(any(test, feature = "test-inproc-uds"))]
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            let sender = daemon_socket_registry()
                .lock()
                .ok()
                .and_then(|guard| guard.get(socket_path).cloned())
                .ok_or(NetError::Unavailable)?;
            let (client_std, server_std) =
                StdUnixStream::pair().map_err(|_| NetError::Unavailable)?;
            sender.send(server_std).map_err(|_| NetError::Unavailable)?;
            client_std
        }
        Err(_) => return Err(NetError::Unavailable),
    };
    verify_std_stream_peer_uid(&stream, expected_peer_uid)?;
    Ok(stream)
}

async fn connect_daemon_async_stream(
    socket_path: &Path,
    expected_dir_uid: u32,
    expected_peer_uid: u32,
) -> Result<UnixStream> {
    verify_socket_parent_dir_with_uid(socket_path, expected_dir_uid)?;
    let stream = match UnixStream::connect(socket_path).await {
        Ok(stream) => stream,
        #[cfg(any(test, feature = "test-inproc-uds"))]
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            let client_std =
                connect_daemon_sync_stream(socket_path, expected_dir_uid, expected_peer_uid)?;
            client_std
                .set_nonblocking(true)
                .map_err(|_| NetError::Unavailable)?;
            UnixStream::from_std(client_std).map_err(|_| NetError::Unavailable)?
        }
        Err(_) => return Err(NetError::Unavailable),
    };
    verify_stream_peer_uid(&stream, expected_peer_uid)?;
    Ok(stream)
}

fn split_ingest_jsonl_batches(
    req: &IngestJsonlRequest,
    max_frame_bytes: usize,
) -> Result<Vec<IngestJsonlRequest>> {
    if max_frame_bytes == 0 {
        return Err(NetError::Invalid("daemon frame exceeds max_frame_bytes"));
    }
    let single_len = serde_json::to_vec(&DaemonRequest::IngestJsonl(req.clone()))?
        .len()
        .saturating_add(1);
    if single_len <= max_frame_bytes {
        return Ok(vec![req.clone()]);
    }

    let lines: Vec<&str> = req
        .jsonl
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    // Use `extract_now: false` ("false" is 5 bytes >= "true" 4 bytes) as conservative envelope overhead.
    let envelope_probe = IngestJsonlRequest {
        jsonl: String::new(),
        actor: req.actor.clone(),
        at: req.at,
        extract_now: false,
    };
    let base_overhead = serde_json::to_vec(&DaemonRequest::IngestJsonl(envelope_probe))?
        .len()
        .saturating_add(1);
    if base_overhead > max_frame_bytes {
        return Err(NetError::Invalid("daemon frame exceeds max_frame_bytes"));
    }
    if lines.is_empty() {
        return Ok(vec![IngestJsonlRequest {
            jsonl: String::new(),
            actor: req.actor.clone(),
            at: req.at,
            extract_now: req.extract_now,
        }]);
    }

    let mut batches = Vec::new();
    let mut current_lines: Vec<&str> = Vec::new();
    let mut current_frame_len = base_overhead;

    for line in lines {
        let escaped_len = serde_json::to_vec(line)?.len().saturating_sub(2);
        if base_overhead.saturating_add(escaped_len) > max_frame_bytes {
            return Err(NetError::Invalid("daemon frame exceeds max_frame_bytes"));
        }
        let added = if current_lines.is_empty() {
            escaped_len
        } else {
            // `\n` separator inside JSON string encodes as two bytes (`\n` -> `\\n`).
            escaped_len.saturating_add(2)
        };
        if !current_lines.is_empty() && current_frame_len.saturating_add(added) > max_frame_bytes {
            batches.push(IngestJsonlRequest {
                jsonl: current_lines.join("\n"),
                actor: req.actor.clone(),
                at: req.at,
                extract_now: false,
            });
            current_lines.clear();
            current_frame_len = base_overhead.saturating_add(escaped_len);
            current_lines.push(line);
        } else {
            current_frame_len = current_frame_len.saturating_add(added);
            current_lines.push(line);
        }
    }

    if !current_lines.is_empty() {
        batches.push(IngestJsonlRequest {
            jsonl: current_lines.join("\n"),
            actor: req.actor.clone(),
            at: req.at,
            extract_now: req.extract_now,
        });
    }
    Ok(batches)
}
#[derive(Clone, Debug)]
pub struct DaemonClient {
    socket_path: PathBuf,
    timeout: Duration,
    max_frame_bytes: usize,
    #[cfg(any(test, feature = "test-inproc-uds"))]
    expected_peer_uid_override: Option<u32>,
}

impl DaemonClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Result<Self> {
        let socket_path = socket_path.into();
        validate_socket_path(&socket_path)?;
        Ok(Self {
            socket_path,
            timeout: Duration::from_secs(5),
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            #[cfg(any(test, feature = "test-inproc-uds"))]
            expected_peer_uid_override: None,
        })
    }

    pub fn for_library(library_dir: &Path) -> Result<Self> {
        Self::new(daemon_socket_path(library_dir)?)
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_max_frame_bytes(mut self, max_frame_bytes: usize) -> Self {
        self.max_frame_bytes = max_frame_bytes;
        self
    }

    #[cfg(any(test, feature = "test-inproc-uds"))]
    pub fn with_expected_peer_uid_for_test(mut self, expected_peer_uid: u32) -> Self {
        self.expected_peer_uid_override = Some(expected_peer_uid);
        self
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub async fn request(&self, req: &DaemonRequest) -> Result<DaemonResponse> {
        let uid = current_process_uid()?;
        #[cfg(any(test, feature = "test-inproc-uds"))]
        let expected_peer_uid = self.expected_peer_uid_override.unwrap_or(uid);
        #[cfg(not(any(test, feature = "test-inproc-uds")))]
        let expected_peer_uid = uid;
        let mut payload = serde_json::to_vec(req)?;
        payload.push(b'\n');

        let stream = tokio::time::timeout(
            self.timeout,
            connect_daemon_async_stream(&self.socket_path, uid, expected_peer_uid),
        )
        .await
        .map_err(|_| NetError::Unavailable)??;

        let (read_half, mut write_half) = stream.into_split();
        tokio::time::timeout(self.timeout, async {
            write_half.write_all(&payload).await?;
            write_half.flush().await
        })
        .await
        .map_err(|_| NetError::Unavailable)?
        .map_err(NetError::Io)?;

        let mut reader = BufReader::new(read_half);
        let mut buf = Vec::new();
        let n = tokio::time::timeout(
            self.timeout,
            read_bounded_line(&mut reader, &mut buf, self.max_frame_bytes),
        )
        .await
        .map_err(|_| NetError::Unavailable)??;

        if n == 0 {
            return Err(NetError::Unavailable);
        }
        Ok(serde_json::from_slice(trim_ascii_newline(&buf))?)
    }

    pub async fn ping(&self) -> Result<(LibraryId, u64)> {
        match self.request(&DaemonRequest::Ping).await? {
            DaemonResponse::Pong {
                library_id,
                uptime_ms,
            } => Ok((library_id, uptime_ms)),
            DaemonResponse::Error { message } => Err(NetError::Daemon(message)),
            _ => Err(NetError::Invalid("unexpected daemon response")),
        }
    }

    pub async fn status(&self) -> Result<DaemonStatus> {
        match self.request(&DaemonRequest::Status).await? {
            DaemonResponse::Status(status) => Ok(status),
            DaemonResponse::Error { message } => Err(NetError::Daemon(message)),
            _ => Err(NetError::Invalid("unexpected daemon response")),
        }
    }

    pub async fn notify_cursor(&self, notification: CursorNotification) -> Result<(bool, u64)> {
        match self
            .request(&DaemonRequest::NotifyCursor(notification))
            .await?
        {
            DaemonResponse::CursorAck {
                queued_extraction,
                last_extracted_offset,
            } => Ok((queued_extraction, last_extracted_offset)),
            DaemonResponse::Error { message } => Err(NetError::Daemon(message)),
            _ => Err(NetError::Invalid("unexpected daemon response")),
        }
    }

    pub async fn query_context(&self, req: QueryContextRequest) -> Result<QueryContextResult> {
        match self.request(&DaemonRequest::QueryContext(req)).await? {
            DaemonResponse::Context(res) => Ok(res),
            DaemonResponse::Error { message } => Err(NetError::Daemon(message)),
            _ => Err(NetError::Invalid("unexpected daemon response")),
        }
    }

    pub async fn pre_tool_check(&self, req: PreToolCheckRequest) -> Result<PreToolCheckResult> {
        match self.request(&DaemonRequest::PreToolCheck(req)).await? {
            DaemonResponse::PreTool(res) => Ok(res),
            DaemonResponse::Error { message } => Err(NetError::Daemon(message)),
            _ => Err(NetError::Invalid("unexpected daemon response")),
        }
    }

    /// Sends a public JSONL stream to the local daemon, automatically splitting oversized multi-line
    /// streams into `<= self.max_frame_bytes` UDS frames and merging their `IngestJsonlResult`s.
    pub async fn ingest_jsonl(&self, req: IngestJsonlRequest) -> Result<IngestJsonlResult> {
        let batches = split_ingest_jsonl_batches(&req, self.max_frame_bytes)?;
        let mut combined = IngestJsonlResult::default();
        for batch in batches {
            match self.request(&DaemonRequest::IngestJsonl(batch)).await? {
                DaemonResponse::Ingested(res) => combined.merge(res),
                DaemonResponse::Error { message } => return Err(NetError::Daemon(message)),
                _ => return Err(NetError::Invalid("unexpected daemon response")),
            }
        }
        Ok(combined)
    }

    pub async fn trigger_sync(&self) -> Result<()> {
        match self.request(&DaemonRequest::TriggerSync).await? {
            DaemonResponse::SyncTriggered => Ok(()),
            DaemonResponse::Error { message } => Err(NetError::Daemon(message)),
            _ => Err(NetError::Invalid("unexpected daemon response")),
        }
    }

    /// Starts (or attaches to) the local `127.0.0.1` read-only Web Inspector on the running daemon
    /// and returns a 256-bit capability-token URL bound to `viewer` (or library owner when `None`).
    pub async fn start_web_inspector(
        &self,
        port: u16,
        viewer: Option<ActorId>,
    ) -> Result<WebInspectorSession> {
        match self
            .request(&DaemonRequest::StartWebInspector { port, viewer })
            .await?
        {
            DaemonResponse::WebInspector {
                url,
                bind_addr,
                viewer,
            } => Ok(WebInspectorSession {
                url,
                bind_addr,
                viewer,
            }),
            DaemonResponse::Error { message } => Err(NetError::Daemon(message)),
            _ => Err(NetError::Invalid("unexpected daemon response")),
        }
    }

    pub async fn shutdown(&self) -> Result<()> {
        match self.request(&DaemonRequest::Shutdown).await? {
            DaemonResponse::ShuttingDown => Ok(()),
            DaemonResponse::Error { message } => Err(NetError::Daemon(message)),
            _ => Err(NetError::Invalid("unexpected daemon response")),
        }
    }
}

/// Zero-runtime synchronous stdlib UDS client (`std::os::unix::net::UnixStream`) for `< 5ms`
/// hook invocations (`Stop`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`).
#[derive(Clone, Debug)]
pub struct DaemonSyncClient {
    socket_path: PathBuf,
    timeout: Duration,
    max_frame_bytes: usize,
    #[cfg(any(test, feature = "test-inproc-uds"))]
    expected_peer_uid_override: Option<u32>,
}

impl DaemonSyncClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Result<Self> {
        let socket_path = socket_path.into();
        validate_socket_path(&socket_path)?;
        Ok(Self {
            socket_path,
            timeout: Duration::from_millis(500),
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            #[cfg(any(test, feature = "test-inproc-uds"))]
            expected_peer_uid_override: None,
        })
    }

    pub fn for_library(library_dir: &Path) -> Result<Self> {
        Self::new(daemon_socket_path(library_dir)?)
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_max_frame_bytes(mut self, max_frame_bytes: usize) -> Self {
        self.max_frame_bytes = max_frame_bytes;
        self
    }

    #[cfg(any(test, feature = "test-inproc-uds"))]
    pub fn with_expected_peer_uid_for_test(mut self, expected_peer_uid: u32) -> Self {
        self.expected_peer_uid_override = Some(expected_peer_uid);
        self
    }

    pub fn request(&self, req: &DaemonRequest) -> Result<DaemonResponse> {
        let uid = current_process_uid()?;
        #[cfg(any(test, feature = "test-inproc-uds"))]
        let expected_peer_uid = self.expected_peer_uid_override.unwrap_or(uid);
        #[cfg(not(any(test, feature = "test-inproc-uds")))]
        let expected_peer_uid = uid;
        let mut payload = serde_json::to_vec(req)?;
        payload.push(b'\n');

        let mut stream = connect_daemon_sync_stream(&self.socket_path, uid, expected_peer_uid)?;
        // On macOS/Darwin, `setsockopt(SO_SNDTIMEO/SO_RCVTIMEO)` returns `EINVAL` if the peer has
        // already closed its end of an `AF_UNIX` stream; let subsequent read/write observe EOF cleanly.
        let _ = stream.set_write_timeout(Some(self.timeout));
        let _ = stream.set_read_timeout(Some(self.timeout));
        stream.write_all(&payload).map_err(NetError::Io)?;
        stream.flush().map_err(NetError::Io)?;

        let limit = u64::try_from(self.max_frame_bytes).unwrap_or(u64::MAX);
        let mut reader = StdBufReader::new(stream).take(limit.saturating_add(1));
        let mut buf = Vec::new();
        let n = reader.read_until(b'\n', &mut buf).map_err(NetError::Io)?;
        if n == 0 {
            return Err(NetError::Unavailable);
        }
        if buf.len() > self.max_frame_bytes {
            return Err(NetError::Invalid("daemon frame exceeds max_frame_bytes"));
        }
        Ok(serde_json::from_slice(trim_ascii_newline(&buf))?)
    }

    pub fn notify_cursor(&self, notification: CursorNotification) -> Result<(bool, u64)> {
        match self.request(&DaemonRequest::NotifyCursor(notification))? {
            DaemonResponse::CursorAck {
                queued_extraction,
                last_extracted_offset,
            } => Ok((queued_extraction, last_extracted_offset)),
            DaemonResponse::Error { message } => Err(NetError::Daemon(message)),
            _ => Err(NetError::Invalid("unexpected daemon response")),
        }
    }

    pub fn trigger_sync(&self) -> Result<()> {
        match self.request(&DaemonRequest::TriggerSync)? {
            DaemonResponse::SyncTriggered => Ok(()),
            DaemonResponse::Error { message } => Err(NetError::Daemon(message)),
            _ => Err(NetError::Invalid("unexpected daemon response")),
        }
    }

    /// Synchronously starts (or attaches to) the local `127.0.0.1` read-only Web Inspector on the
    /// running daemon and returns a 256-bit capability-token URL bound to `viewer`.
    pub fn start_web_inspector(
        &self,
        port: u16,
        viewer: Option<ActorId>,
    ) -> Result<WebInspectorSession> {
        match self.request(&DaemonRequest::StartWebInspector { port, viewer })? {
            DaemonResponse::WebInspector {
                url,
                bind_addr,
                viewer,
            } => Ok(WebInspectorSession {
                url,
                bind_addr,
                viewer,
            }),
            DaemonResponse::Error { message } => Err(NetError::Daemon(message)),
            _ => Err(NetError::Invalid("unexpected daemon response")),
        }
    }

    /// Synchronously sends a public JSONL stream to the local daemon, automatically splitting oversized
    /// multi-line streams into `<= self.max_frame_bytes` UDS frames and merging their `IngestJsonlResult`s.
    pub fn ingest_jsonl(&self, req: IngestJsonlRequest) -> Result<IngestJsonlResult> {
        let batches = split_ingest_jsonl_batches(&req, self.max_frame_bytes)?;
        let mut combined = IngestJsonlResult::default();
        for batch in batches {
            match self.request(&DaemonRequest::IngestJsonl(batch))? {
                DaemonResponse::Ingested(res) => combined.merge(res),
                DaemonResponse::Error { message } => return Err(NetError::Daemon(message)),
                _ => return Err(NetError::Invalid("unexpected daemon response")),
            }
        }
        Ok(combined)
    }
}
