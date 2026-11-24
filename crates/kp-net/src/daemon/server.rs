#[cfg(any(test, feature = "test-inproc-uds"))]
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
#[cfg(any(test, feature = "test-inproc-uds"))]
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::sync::{mpsc as std_mpsc, Arc};
#[cfg(any(test, feature = "test-inproc-uds"))]
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use kp_model::FactId;
use kp_retrieve::PromptBudgetConfig;
use kp_store::{ExtractionJobStatus, Store};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, watch};

use super::protocol::{
    read_bounded_line, trim_ascii_newline, DaemonConfig, DaemonHandler, DaemonRequest,
    DaemonResponse, DaemonStatus, IngestJsonlRequest, PreToolCheckRequest, PreToolCheckResult,
    QueryContextRequest, QueryContextResult, ToolPermissionDecision, DEFAULT_CONTEXT_MAX_ITEMS,
    DEFAULT_CONTEXT_MAX_TOKENS,
};
use super::runtime_dir::{
    current_process_uid, daemon_lock_path_for_socket, ensure_socket_parent_dir_with_uid,
    is_socket_lock_held, validate_socket_path, verify_stream_peer_uid,
};
use super::scheduler::{
    run_idle_scheduler, ActorCommand, IdleScheduleSignal, IdleSchedulerParams, ScanSnapshot,
};
use super::web::{
    execute_inspector_overview, execute_inspector_why, resolve_daemon_embedder_status,
    validate_loopback_inspector_addr, WebInspectorManager,
};
use crate::{NetError, Result};

#[cfg(any(test, feature = "test-inproc-uds"))]
pub(super) type DaemonSocketRegistry =
    Mutex<BTreeMap<PathBuf, mpsc::UnboundedSender<StdUnixStream>>>;

#[cfg(any(test, feature = "test-inproc-uds"))]
pub(super) fn daemon_socket_registry() -> &'static DaemonSocketRegistry {
    static REGISTRY: OnceLock<DaemonSocketRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(BTreeMap::new()))
}

#[cfg(any(test, feature = "test-inproc-uds"))]
fn unregister_daemon_socket(path: &Path) {
    if let Ok(mut guard) = daemon_socket_registry().lock() {
        guard.remove(path);
    }
}

#[cfg(not(any(test, feature = "test-inproc-uds")))]
fn unregister_daemon_socket(_path: &Path) {}

struct NoopExtractionHandler;

impl DaemonHandler for NoopExtractionHandler {}

/// Guards the daemon socket file and its exclusive OS lock so `remove_file(socket_path)`
/// runs at most once and ALWAYS strictly before `lock_file.unlock()`.
struct DaemonSocketGuard {
    socket_path: PathBuf,
    lock_file: std::fs::File,
    cleaned: std::sync::atomic::AtomicBool,
}

impl DaemonSocketGuard {
    fn new(socket_path: PathBuf, lock_file: std::fs::File) -> Self {
        Self {
            socket_path,
            lock_file,
            cleaned: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn cleanup_once_while_locked(&self) {
        if self
            .cleaned
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
        {
            unregister_daemon_socket(&self.socket_path);
            let _ = std::fs::remove_file(&self.socket_path);
        }
    }
}

impl Drop for DaemonSocketGuard {
    fn drop(&mut self) {
        self.cleanup_once_while_locked();
        let _ = self.lock_file.unlock();
    }
}

/// Running handle for a local `kp daemon` UDS instance.
pub struct DaemonHandle {
    socket_path: PathBuf,
    socket_guard: Option<Arc<DaemonSocketGuard>>,
    web_inspector: Arc<WebInspectorManager>,
    shutdown_tx: watch::Sender<bool>,
    actor_tx: std_mpsc::Sender<ActorCommand>,
    listener_task: Option<tokio::task::JoinHandle<()>>,
    scheduler_task: Option<tokio::task::JoinHandle<()>>,
    actor_thread: Option<std::thread::JoinHandle<()>>,
}

impl DaemonHandle {
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn web_inspector_url(&self) -> Option<String> {
        self.web_inspector.web_inspector_url()
    }

    pub fn web_inspector_addr(&self) -> Option<SocketAddr> {
        self.web_inspector.web_inspector_addr()
    }

    pub fn subscribe_shutdown(&self) -> watch::Receiver<bool> {
        self.shutdown_tx.subscribe()
    }

    /// Awaits a daemon shutdown signal (e.g. from a `Shutdown` RPC or `idle_shutdown_timeout`) without polling over UDS.
    pub async fn wait(&mut self) {
        let mut rx = self.shutdown_tx.subscribe();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                break;
            }
        }
    }

    pub async fn shutdown(mut self) -> Result<()> {
        let _ = self.shutdown_tx.send(true);
        self.web_inspector.shutdown_and_join().await;
        let _ = self.actor_tx.send(ActorCommand::Stop);
        if let Some(task) = self.listener_task.take() {
            let _ = task.await;
        }
        if let Some(task) = self.scheduler_task.take() {
            let _ = task.await;
        }
        if let Some(thread) = self.actor_thread.take() {
            let _ = tokio::task::spawn_blocking(move || {
                let _ = thread.join();
            })
            .await;
        }
        if let Some(guard) = self.socket_guard.take() {
            guard.cleanup_once_while_locked();
            drop(guard);
        }
        Ok(())
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
        self.web_inspector.abort();
        let _ = self.actor_tx.send(ActorCommand::Stop);
        if let Some(task) = self.listener_task.take() {
            task.abort();
        }
        if let Some(task) = self.scheduler_task.take() {
            task.abort();
        }
        if let Some(guard) = self.socket_guard.take() {
            guard.cleanup_once_while_locked();
            drop(guard);
        }
    }
}

pub struct DaemonServer;

impl DaemonServer {
    pub async fn spawn_default(store: Store, config: DaemonConfig) -> Result<DaemonHandle> {
        Self::spawn(store, config, Arc::new(NoopExtractionHandler)).await
    }

    /// Binds the local UDS listener inside a verified `0700` user-owned directory, enforces
    /// `peer_cred()` UID checks, and launches the single-writer `Store` actor + persistent scheduler.
    pub async fn spawn(
        store: Store,
        config: DaemonConfig,
        handler: Arc<dyn DaemonHandler>,
    ) -> Result<DaemonHandle> {
        let uid = current_process_uid()?;
        Self::spawn_inner(store, config, handler, uid, uid).await
    }

    #[cfg(any(test, feature = "test-inproc-uds"))]
    pub async fn spawn_with_expected_peer_uid_for_test(
        store: Store,
        config: DaemonConfig,
        handler: Arc<dyn DaemonHandler>,
        expected_peer_uid: u32,
    ) -> Result<DaemonHandle> {
        let dir_uid = current_process_uid()?;
        Self::spawn_inner(store, config, handler, dir_uid, expected_peer_uid).await
    }

    async fn spawn_inner(
        store: Store,
        config: DaemonConfig,
        handler: Arc<dyn DaemonHandler>,
        expected_dir_uid: u32,
        expected_peer_uid: u32,
    ) -> Result<DaemonHandle> {
        validate_socket_path(&config.socket_path)?;
        ensure_socket_parent_dir_with_uid(&config.socket_path, expected_dir_uid)?;
        if let Some(web_cfg) = &config.web_inspector {
            validate_loopback_inspector_addr(web_cfg.bind_addr)?;
            if store.member_role(&web_cfg.viewer)?.is_none() {
                return Err(NetError::Forbidden);
            }
        }

        let lock_path = daemon_lock_path_for_socket(&config.socket_path);
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&lock_path)?;
        match lock_file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(NetError::Invalid("daemon already running on socket"));
            }
            Err(std::fs::TryLockError::Error(err)) => return Err(NetError::Io(err)),
        }

        prepare_socket_file(&config.socket_path, true).await?;
        let listener = UnixListener::bind(&config.socket_path)?;
        let socket_guard = Arc::new(DaemonSocketGuard::new(
            config.socket_path.clone(),
            lock_file,
        ));
        std::fs::set_permissions(&config.socket_path, std::fs::Permissions::from_mode(0o600))?;

        #[cfg(any(test, feature = "test-inproc-uds"))]
        let mut pair_rx = {
            let (pair_tx, pair_rx) = mpsc::unbounded_channel::<StdUnixStream>();
            if let Ok(mut guard) = daemon_socket_registry().lock() {
                guard.insert(config.socket_path.clone(), pair_tx);
            }
            pair_rx
        };

        let library_dir = store.exchange_library_path().to_path_buf();
        let started_at = Instant::now();
        let (actor_tx, actor_rx) = std_mpsc::channel::<ActorCommand>();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (sched_tx, sched_rx) = mpsc::unbounded_channel::<IdleScheduleSignal>();

        let web_inspector = Arc::new(WebInspectorManager::new(
            actor_tx.clone(),
            sched_tx.clone(),
            shutdown_rx.clone(),
        ));

        let handler_actor = Arc::clone(&handler);
        let web_inspector_actor = Arc::clone(&web_inspector);
        let actor_thread = std::thread::Builder::new()
            .name("kp-daemon-writer".into())
            .spawn(move || {
                run_store_actor(
                    store,
                    started_at,
                    handler_actor,
                    web_inspector_actor,
                    actor_rx,
                )
            })?;

        let scheduler_task = tokio::spawn(run_idle_scheduler(IdleSchedulerParams {
            library_dir: library_dir.clone(),
            idle_window: config.idle_extract_window,
            idle_shutdown_timeout: config.idle_shutdown_timeout,
            handler: Arc::clone(&handler),
            actor_tx: actor_tx.clone(),
            sched_rx,
            shutdown_tx: shutdown_tx.clone(),
            shutdown_rx: shutdown_rx.clone(),
        }));

        let socket_path_cleanup = config.socket_path.clone();
        let max_frame_bytes = config.max_frame_bytes;
        let actor_tx_listener = actor_tx.clone();
        let shutdown_tx_listener = shutdown_tx.clone();
        let mut shutdown_rx_listener = shutdown_rx;
        let guard_listener = Arc::clone(&socket_guard);
        let library_dir_listener = library_dir;
        let handler_listener = handler;
        let web_inspector_listener = Arc::clone(&web_inspector);

        let listener_task = tokio::spawn(async move {
            loop {
                #[cfg(any(test, feature = "test-inproc-uds"))]
                let maybe_stream = tokio::select! {
                    changed = shutdown_rx_listener.changed() => {
                        if changed.is_err() || *shutdown_rx_listener.borrow() {
                            break;
                        }
                        None
                    }
                    accepted = listener.accept() => {
                        accepted.ok().map(|(stream, _addr)| stream)
                    }
                    paired = pair_rx.recv() => {
                        match paired {
                            Some(std_stream) => {
                                if std_stream.set_nonblocking(true).is_ok() {
                                    UnixStream::from_std(std_stream).ok()
                                } else {
                                    None
                                }
                            }
                            None => break,
                        }
                    }
                };

                #[cfg(not(any(test, feature = "test-inproc-uds")))]
                let maybe_stream = tokio::select! {
                    changed = shutdown_rx_listener.changed() => {
                        if changed.is_err() || *shutdown_rx_listener.borrow() {
                            break;
                        }
                        None
                    }
                    accepted = listener.accept() => {
                        accepted.ok().map(|(stream, _addr)| stream)
                    }
                };

                let Some(stream) = maybe_stream else {
                    continue;
                };
                let actor_tx_conn = actor_tx_listener.clone();
                let sched_tx_conn = sched_tx.clone();
                let shutdown_tx_conn = shutdown_tx_listener.clone();
                let sock_conn = socket_path_cleanup.clone();
                let dir_conn = library_dir_listener.clone();
                let handler_conn = Arc::clone(&handler_listener);
                let web_conn = Arc::clone(&web_inspector_listener);
                tokio::spawn(async move {
                    handle_uds_connection(
                        stream,
                        UdsConnectionCtx {
                            expected_peer_uid,
                            max_frame_bytes,
                            library_dir: dir_conn,
                            handler: handler_conn,
                            web_inspector: web_conn,
                            actor_tx: actor_tx_conn,
                            sched_tx: sched_tx_conn,
                            shutdown_tx: shutdown_tx_conn,
                            socket_path: sock_conn,
                        },
                    )
                    .await;
                });
            }
            web_inspector_listener.abort();
            guard_listener.cleanup_once_while_locked();
            let _ = actor_tx_listener.send(ActorCommand::Stop);
            let _ = shutdown_tx_listener.send(true);
            drop(guard_listener);
        });

        let handle = DaemonHandle {
            socket_path: config.socket_path,
            socket_guard: Some(socket_guard),
            web_inspector: Arc::clone(&web_inspector),
            shutdown_tx,
            actor_tx,
            listener_task: Some(listener_task),
            scheduler_task: Some(scheduler_task),
            actor_thread: Some(actor_thread),
        };

        if let Some(web_cfg) = config.web_inspector {
            if let Err(err) = web_inspector
                .ensure_started_for_viewer(web_cfg.bind_addr, web_cfg.viewer)
                .await
            {
                let _ = handle.shutdown().await;
                return Err(err);
            }
        }

        Ok(handle)
    }
}

async fn prepare_socket_file(socket_path: &Path, caller_holds_lock: bool) -> Result<()> {
    if !caller_holds_lock && is_socket_lock_held(socket_path) {
        return Err(NetError::Invalid("daemon already running on socket"));
    }
    #[cfg(any(test, feature = "test-inproc-uds"))]
    if daemon_socket_registry()
        .lock()
        .ok()
        .is_some_and(|guard| guard.contains_key(socket_path))
    {
        return Err(NetError::Invalid("daemon already running on socket"));
    }
    let Ok(meta) = std::fs::symlink_metadata(socket_path) else {
        return Ok(());
    };
    if !meta.file_type().is_socket() {
        let _ = std::fs::remove_file(socket_path);
        return Ok(());
    }
    match tokio::time::timeout(Duration::from_millis(200), UnixStream::connect(socket_path)).await {
        Ok(Ok(_)) => Err(NetError::Invalid("daemon already running on socket")),
        Ok(Err(err))
            if matches!(
                err.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) || (caller_holds_lock && err.kind() == std::io::ErrorKind::PermissionDenied) =>
        {
            let _ = std::fs::remove_file(socket_path);
            Ok(())
        }
        _ => Err(NetError::Invalid("daemon already running on socket")),
    }
}

fn build_scan_snapshot(store: &Store, data_version: Option<i64>) -> ScanSnapshot {
    let rows = store
        .daemon_unextracted_session_cursors()
        .unwrap_or_default();
    let (_, total_pending) = store.daemon_queue_stats().unwrap_or((0, 0));
    let pending_jobs = total_pending.saturating_sub(u64::try_from(rows.len()).unwrap_or(0));
    let pending_outbox = store.daemon_pending_outbox_count().unwrap_or(0);
    let needs_alignment = store.needs_alignment().unwrap_or(false);
    ScanSnapshot {
        data_version,
        rows,
        pending_jobs,
        pending_outbox,
        needs_alignment,
    }
}

fn run_store_actor(
    mut store: Store,
    started_at: Instant,
    handler: Arc<dyn DaemonHandler>,
    web_inspector: Arc<WebInspectorManager>,
    rx: std_mpsc::Receiver<ActorCommand>,
) {
    let library_dir = store.exchange_library_path().to_path_buf();
    let _ = store.daemon_passive_wal_checkpoint();
    while let Ok(command) = rx.recv() {
        match command {
            ActorCommand::Stop => break,
            ActorCommand::MarkExtracted {
                session_id,
                prefix_sha256,
                extracted_offset,
                reply,
            } => {
                let more = store
                    .daemon_mark_session_extracted(&session_id, &prefix_sha256, extracted_offset)
                    .unwrap_or(false);
                let needs_alignment = store.needs_alignment().unwrap_or(false);
                let _ = store.daemon_passive_wal_checkpoint();
                let _ = reply.send((more, needs_alignment));
            }
            ActorCommand::RecordFailure {
                session_id,
                paused_quota,
                error,
                at,
                reply,
            } => {
                let state = store
                    .daemon_record_session_extraction_failure(
                        &session_id,
                        paused_quota,
                        error.as_deref(),
                        at,
                    )
                    .unwrap_or((
                        if paused_quota {
                            ExtractionJobStatus::PausedQuota
                        } else {
                            ExtractionJobStatus::Pending
                        },
                        1,
                    ));
                let _ = reply.send(state);
            }
            ActorCommand::ScanUnextracted {
                since_data_version,
                reply,
            } => {
                let current_dv = store.daemon_sqlite_data_version().ok();
                if since_data_version.is_some() && current_dv == since_data_version {
                    let _ = reply.send(None);
                } else {
                    let _ = reply.send(Some(build_scan_snapshot(&store, current_dv)));
                }
            }
            ActorCommand::InspectIdleWork { reply } => {
                let current_dv = store.daemon_sqlite_data_version().ok();
                let _ = reply.send(build_scan_snapshot(&store, current_dv));
            }
            ActorCommand::ResolveIngestActor { requested, reply }
            | ActorCommand::ResolveInspectorViewer { requested, reply } => {
                let resolved = match requested {
                    None => Ok(store.owner_actor().clone()),
                    Some(actor) => match store.member_role(&actor) {
                        Ok(Some(_)) => Ok(actor),
                        Ok(None) => Err("actor is not an active library member".to_owned()),
                        Err(err) => Err(err.to_string()),
                    },
                };
                let _ = reply.send(resolved);
            }
            ActorCommand::InspectorVerifyMember { viewer, reply } => {
                let is_active = matches!(store.member_role(&viewer), Ok(Some(_)));
                let _ = reply.send(is_active);
            }
            ActorCommand::InspectorOverview {
                viewer,
                params,
                reply,
            } => {
                let res = execute_inspector_overview(
                    &library_dir,
                    &mut store,
                    handler.as_ref(),
                    &viewer,
                    &params,
                );
                let _ = reply.send(res);
            }
            ActorCommand::InspectorWhy {
                viewer,
                raw_fact_id,
                at,
                reply,
            } => {
                let res = execute_inspector_why(
                    &library_dir,
                    &mut store,
                    handler.as_ref(),
                    &viewer,
                    &raw_fact_id,
                    at,
                );
                let _ = reply.send(res);
            }
            ActorCommand::Rpc { request, reply } => {
                let uptime_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
                let mut schedule_signal = None;
                let should_stop = matches!(request, DaemonRequest::Shutdown);
                let response = match dispatch_daemon_rpc(
                    &library_dir,
                    &mut store,
                    handler.as_ref(),
                    &web_inspector,
                    request,
                    uptime_ms,
                    &mut schedule_signal,
                ) {
                    Ok(resp) => resp,
                    Err(err) => DaemonResponse::Error {
                        message: err.to_string(),
                    },
                };
                if !should_stop && schedule_signal.is_none() {
                    schedule_signal = Some(IdleScheduleSignal::ClientActivity);
                }
                let _ = reply.send((response, schedule_signal));
                if should_stop {
                    break;
                }
            }
        }
    }
}

fn dispatch_daemon_rpc(
    library_dir: &Path,
    store: &mut Store,
    handler: &dyn DaemonHandler,
    web_inspector: &WebInspectorManager,
    request: DaemonRequest,
    uptime_ms: u64,
    schedule_signal: &mut Option<IdleScheduleSignal>,
) -> Result<DaemonResponse> {
    match request {
        DaemonRequest::Ping => Ok(DaemonResponse::Pong {
            library_id: store.library_id().clone(),
            uptime_ms,
        }),
        DaemonRequest::Status => {
            handler.refresh_store_embedder(library_dir, store);
            let status = store.status()?;
            let (tracked_sessions, pending_extractions) = store.daemon_queue_stats()?;
            let embedder = resolve_daemon_embedder_status(store, handler, &status.embedding_model);
            let (web_inspector_url, web_inspector_viewer) = match web_inspector.status_fields() {
                (Some(url), Some(v)) if store.member_role(&v).ok().flatten().is_some() => {
                    (Some(url), Some(v))
                }
                _ => (None, None),
            };
            Ok(DaemonResponse::Status(DaemonStatus {
                library_id: status.library_id,
                name: status.name,
                mode: status.mode,
                owner_actor: store.owner_actor().clone(),
                facts: status.facts,
                sources: status.sources,
                tracked_sessions,
                pending_extractions,
                uptime_ms,
                embedder,
                web_inspector_url,
                web_inspector_viewer,
            }))
        }
        DaemonRequest::NotifyCursor(notification) => {
            let (last_extracted_offset, queued_extraction) =
                store.daemon_record_session_cursor(&kp_store::SessionCursorUpdate {
                    session_id: &notification.session_id,
                    client_kind: &notification.client_kind,
                    transcript_path: notification.transcript_path.as_deref(),
                    byte_offset: notification.byte_offset,
                    line_count: notification.line_count,
                    prefix_sha256: &notification.prefix_sha256,
                    prev_byte_offset: None,
                    prev_prefix_sha256: None,
                    actor: Some(&notification.actor),
                    session_ended: notification.session_ended,
                    at: notification.at,
                })?;
            if queued_extraction {
                *schedule_signal = Some(IdleScheduleSignal::Cursor {
                    notification,
                    start_byte_offset: last_extracted_offset,
                });
            }
            Ok(DaemonResponse::CursorAck {
                queued_extraction,
                last_extracted_offset,
            })
        }
        DaemonRequest::QueryContext(req) => {
            handler.refresh_store_embedder(library_dir, store);
            Ok(DaemonResponse::Context(handle_query_context(store, &req)?))
        }
        DaemonRequest::PreToolCheck(req) => {
            handler.refresh_store_embedder(library_dir, store);
            Ok(DaemonResponse::PreTool(handle_pre_tool_check(store, &req)?))
        }
        DaemonRequest::IngestJsonl(_) | DaemonRequest::StartWebInspector { .. } => Err(
            NetError::Invalid("async daemon RPC must be dispatched via UDS connection handler"),
        ),
        DaemonRequest::TriggerSync => {
            *schedule_signal = Some(IdleScheduleSignal::TriggerSync);
            Ok(DaemonResponse::SyncTriggered)
        }
        DaemonRequest::Shutdown => Ok(DaemonResponse::ShuttingDown),
    }
}

fn resolve_request_time_zone(tz_name: Option<&str>) -> kp_model::TimeZone {
    tz_name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|name| kp_model::TimeZone::get(name).ok())
        .unwrap_or_else(kp_model::system_time_zone)
}

fn handle_query_context(store: &Store, req: &QueryContextRequest) -> Result<QueryContextResult> {
    if req.prompt.trim().is_empty() || store.member_role(&req.viewer)?.is_none() {
        return Ok(QueryContextResult::default());
    }
    let config = PromptBudgetConfig {
        max_tokens: req.max_tokens.clamp(1, DEFAULT_CONTEXT_MAX_TOKENS),
        max_items: req.max_items.clamp(1, DEFAULT_CONTEXT_MAX_ITEMS),
        time_zone: resolve_request_time_zone(req.time_zone.as_deref()),
        ..PromptBudgetConfig::default()
    };
    let ctx =
        kp_retrieve::retrieve_prompt_context(store, &req.viewer, req.at, &req.prompt, &config)?;
    if ctx.text.is_empty() {
        return Ok(QueryContextResult::default());
    }

    let mut probe_ids = Vec::with_capacity(ctx.items.len() * 2);
    for item in &ctx.items {
        let raw = item.ref_id.trim();
        if let Ok(fid) = raw.parse::<FactId>() {
            probe_ids.push(fid);
        }
        if let Some(stripped) = raw.strip_prefix("fact_") {
            if let Ok(fid) = stripped.parse::<FactId>() {
                probe_ids.push(fid);
            }
        }
    }
    let visible = store.filter_visible_fact_ids(&req.viewer, req.at, &probe_ids)?;
    let mut cited_fact_ids = Vec::with_capacity(ctx.items.len());
    for item in &ctx.items {
        let raw = item.ref_id.trim();
        let matched = raw
            .parse::<FactId>()
            .ok()
            .filter(|fid| visible.contains(fid))
            .or_else(|| {
                raw.strip_prefix("fact_")
                    .and_then(|s| s.parse::<FactId>().ok())
                    .filter(|fid| visible.contains(fid))
            });
        if let Some(fid) = matched {
            if !cited_fact_ids.contains(&fid) {
                cited_fact_ids.push(fid);
            }
        }
    }

    Ok(QueryContextResult {
        additional_context: ctx.text,
        item_count: ctx.items.len(),
        estimated_tokens: ctx.total_tokens,
        cited_fact_ids,
    })
}

fn handle_pre_tool_check(
    store: &mut Store,
    req: &PreToolCheckRequest,
) -> Result<PreToolCheckResult> {
    if !kp_retrieve::guard::is_guarded_tool(&req.tool_name)
        || store.member_role(&req.viewer)?.is_none()
    {
        return Ok(PreToolCheckResult::default());
    }
    let verdict = kp_retrieve::guard::evaluate_tool_guards(
        store,
        &kp_retrieve::guard::GuardCheckInput {
            tool_name: req.tool_name.clone(),
            tool_input: req.tool_input.clone(),
            cwd: req.cwd.clone(),
            viewer: req.viewer.clone(),
            at: req.at,
            time_zone: resolve_request_time_zone(req.time_zone.as_deref()),
        },
    )?;

    let is_codex = req
        .client_kind
        .as_deref()
        .is_some_and(|k| k.trim().eq_ignore_ascii_case("codex"));
    if is_codex {
        if verdict.hits.is_empty() {
            return Ok(PreToolCheckResult::default());
        }
        let all_ids: Vec<FactId> = verdict.hits.iter().map(|h| h.fact_id.clone()).collect();
        let missing = store.try_consume_all_guard_allows(&all_ids, req.at)?;
        if missing.is_empty() {
            return Ok(PreToolCheckResult::default());
        }
        let missing_set: BTreeSet<FactId> = missing.into_iter().collect();
        let remaining: Vec<_> = verdict
            .hits
            .into_iter()
            .filter(|h| missing_set.contains(&h.fact_id))
            .collect();
        let reasons: Vec<&str> = remaining
            .iter()
            .map(|h| h.formatted_reason.as_str())
            .collect();
        let matched_fact_ids: Vec<FactId> = remaining.iter().map(|h| h.fact_id.clone()).collect();
        let allow_refs: Vec<String> = remaining.iter().map(|h| h.kp_ref.clone()).collect();
        let reason = format!(
            "{}\n先问用户；用户同意后，请用户执行 kp allow {}。",
            reasons.join("\n"),
            allow_refs.join(" ")
        );
        Ok(PreToolCheckResult {
            permission_decision: Some(ToolPermissionDecision::Deny),
            permission_decision_reason: Some(reason),
            matched_fact_ids,
            allow_refs,
        })
    } else {
        if !verdict.is_blocked() {
            return Ok(PreToolCheckResult::default());
        }
        Ok(PreToolCheckResult {
            permission_decision: Some(ToolPermissionDecision::Ask),
            permission_decision_reason: Some(verdict.joined_reason()),
            matched_fact_ids: verdict.matched_fact_ids(),
            allow_refs: verdict.allow_refs(),
        })
    }
}

struct IngestActivityGuard {
    sched_tx: mpsc::UnboundedSender<IdleScheduleSignal>,
    finished: bool,
}

impl IngestActivityGuard {
    fn start(sched_tx: mpsc::UnboundedSender<IdleScheduleSignal>) -> Self {
        let _ = sched_tx.send(IdleScheduleSignal::IngestStarted);
        Self {
            sched_tx,
            finished: false,
        }
    }

    fn finish(mut self, wake_jobs_drain: bool, wake_sync: bool) {
        self.finished = true;
        let _ = self.sched_tx.send(IdleScheduleSignal::IngestFinished {
            wake_jobs_drain,
            wake_sync,
        });
    }
}

impl Drop for IngestActivityGuard {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.sched_tx.send(IdleScheduleSignal::IngestFinished {
                wake_jobs_drain: false,
                wake_sync: false,
            });
        }
    }
}

async fn execute_ingest_jsonl_rpc(
    library_dir: &Path,
    handler: &Arc<dyn DaemonHandler>,
    actor_tx: &std_mpsc::Sender<ActorCommand>,
    sched_tx: &mpsc::UnboundedSender<IdleScheduleSignal>,
    mut req: IngestJsonlRequest,
) -> DaemonResponse {
    if let Err(err) = kp_model::parse_public_jsonl_str(&req.jsonl) {
        let _ = sched_tx.send(IdleScheduleSignal::ClientActivity);
        return DaemonResponse::Error {
            message: err.to_string(),
        };
    }

    let (reply_tx, reply_rx) = oneshot::channel();
    if actor_tx
        .send(ActorCommand::ResolveIngestActor {
            requested: req.actor.clone(),
            reply: reply_tx,
        })
        .is_err()
    {
        return DaemonResponse::Error {
            message: "daemon store actor stopped".into(),
        };
    }
    match reply_rx.await {
        Ok(Ok(actor)) => {
            req.actor = Some(actor);
        }
        Ok(Err(message)) => {
            let _ = sched_tx.send(IdleScheduleSignal::ClientActivity);
            return DaemonResponse::Error { message };
        }
        Err(_) => {
            return DaemonResponse::Error {
                message: "daemon store actor dropped response".into(),
            };
        }
    }

    let guard = IngestActivityGuard::start(sched_tx.clone());
    match handler.ingest_jsonl(library_dir, req).await {
        Ok(res) => {
            guard.finish(res.jobs_enqueued > 0, res.has_mutations());
            DaemonResponse::Ingested(res)
        }
        Err(err) => {
            guard.finish(false, false);
            DaemonResponse::Error {
                message: err.to_string(),
            }
        }
    }
}

async fn execute_start_web_inspector_rpc(
    web_inspector: &Arc<WebInspectorManager>,
    actor_tx: &std_mpsc::Sender<ActorCommand>,
    sched_tx: &mpsc::UnboundedSender<IdleScheduleSignal>,
    port: u16,
    requested_viewer: Option<kp_model::ActorId>,
) -> DaemonResponse {
    let _ = sched_tx.send(IdleScheduleSignal::ClientActivity);
    let (reply_tx, reply_rx) = oneshot::channel();
    if actor_tx
        .send(ActorCommand::ResolveInspectorViewer {
            requested: requested_viewer,
            reply: reply_tx,
        })
        .is_err()
    {
        return DaemonResponse::Error {
            message: "daemon store actor stopped".into(),
        };
    }
    let target_viewer = match reply_rx.await {
        Ok(Ok(actor)) => actor,
        Ok(Err(message)) => return DaemonResponse::Error { message },
        Err(_) => {
            return DaemonResponse::Error {
                message: "daemon store actor dropped response".into(),
            };
        }
    };

    let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    match web_inspector
        .ensure_started_for_viewer(bind_addr, target_viewer)
        .await
    {
        Ok(session) => DaemonResponse::WebInspector {
            url: session.url,
            bind_addr: session.bind_addr,
            viewer: session.viewer,
        },
        Err(err) => DaemonResponse::Error {
            message: err.to_string(),
        },
    }
}

struct UdsConnectionCtx {
    expected_peer_uid: u32,
    max_frame_bytes: usize,
    library_dir: PathBuf,
    handler: Arc<dyn DaemonHandler>,
    web_inspector: Arc<WebInspectorManager>,
    actor_tx: std_mpsc::Sender<ActorCommand>,
    sched_tx: mpsc::UnboundedSender<IdleScheduleSignal>,
    shutdown_tx: watch::Sender<bool>,
    socket_path: PathBuf,
}

async fn handle_uds_connection(stream: UnixStream, ctx: UdsConnectionCtx) {
    let UdsConnectionCtx {
        expected_peer_uid,
        max_frame_bytes,
        library_dir,
        handler,
        web_inspector,
        actor_tx,
        sched_tx,
        shutdown_tx,
        socket_path,
    } = ctx;
    if verify_stream_peer_uid(&stream, expected_peer_uid).is_err() {
        return;
    }
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut buf = Vec::new();

    loop {
        buf.clear();
        let read_res = read_bounded_line(&mut reader, &mut buf, max_frame_bytes).await;
        let response = match read_res {
            Ok(0) => break,
            Err(err) => {
                let frame = DaemonResponse::Error {
                    message: err.to_string(),
                };
                let _ = write_json_line(&mut write_half, &frame).await;
                break;
            }
            Ok(_) => {
                let trimmed = trim_ascii_newline(&buf);
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_slice::<DaemonRequest>(trimmed) {
                    Err(_) => DaemonResponse::Error {
                        message: "invalid daemon JSON frame".into(),
                    },
                    Ok(DaemonRequest::IngestJsonl(ingest_req)) => {
                        execute_ingest_jsonl_rpc(
                            &library_dir,
                            &handler,
                            &actor_tx,
                            &sched_tx,
                            ingest_req,
                        )
                        .await
                    }
                    Ok(DaemonRequest::StartWebInspector { port, viewer }) => {
                        execute_start_web_inspector_rpc(
                            &web_inspector,
                            &actor_tx,
                            &sched_tx,
                            port,
                            viewer,
                        )
                        .await
                    }
                    Ok(request) => {
                        let is_shutdown = matches!(request, DaemonRequest::Shutdown);
                        let (reply_tx, reply_rx) = oneshot::channel();
                        if actor_tx
                            .send(ActorCommand::Rpc {
                                request,
                                reply: reply_tx,
                            })
                            .is_err()
                        {
                            DaemonResponse::Error {
                                message: "daemon store actor stopped".into(),
                            }
                        } else {
                            match reply_rx.await {
                                Ok((resp, signal)) => {
                                    if let Some(sig) = signal {
                                        let _ = sched_tx.send(sig);
                                    }
                                    if is_shutdown {
                                        web_inspector.abort();
                                        unregister_daemon_socket(&socket_path);
                                        let _ = write_json_line(&mut write_half, &resp).await;
                                        let _ = shutdown_tx.send(true);
                                        break;
                                    }
                                    resp
                                }
                                Err(_) => DaemonResponse::Error {
                                    message: "daemon store actor dropped response".into(),
                                },
                            }
                        }
                    }
                }
            }
        };

        if write_json_line(&mut write_half, &response).await.is_err() {
            break;
        }
    }
}

async fn write_json_line<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    response: &DaemonResponse,
) -> Result<()> {
    let mut encoded = serde_json::to_vec(response)?;
    encoded.push(b'\n');
    writer.write_all(&encoded).await?;
    writer.flush().await?;
    Ok(())
}
