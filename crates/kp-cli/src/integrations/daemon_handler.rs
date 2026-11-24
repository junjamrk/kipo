#[cfg(unix)]
use std::error::Error;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::process::Stdio;
#[cfg(unix)]
use std::sync::{Arc, RwLock};
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use kp_model::{BoxFuture, EpochMs};
#[cfg(unix)]
use kp_net::{DaemonClient, DaemonEmbedderRuntimeStatus, DaemonHandler, IdleSessionBatch};
#[cfg(unix)]
use kp_store::{Store, StoreError};

#[cfg(unix)]
use crate::Cli;

/// Daemon handler wiring background session delta capture, Public Ingest Contract UDS `IngestJsonl`,
/// resident `SwappableEmbedder` vector reuse & dynamic `library_meta` refresh, out-of-session LLM
/// extraction, and joined-community synchronization/derivation onto `kp_net::DaemonServer`.
#[cfg(unix)]
#[derive(Clone, Default)]
pub struct CliDaemonHandler {
    embedder: Arc<RwLock<Option<Arc<crate::providers_cli::SwappableEmbedder>>>>,
}

#[cfg(unix)]
impl CliDaemonHandler {
    pub(crate) fn with_embedder(embedder: Arc<crate::providers_cli::SwappableEmbedder>) -> Self {
        Self {
            embedder: Arc::new(RwLock::new(Some(embedder))),
        }
    }
}

#[cfg(unix)]
impl DaemonHandler for CliDaemonHandler {
    fn extract_session_batch<'a>(
        &'a self,
        library_dir: &'a Path,
        batch: IdleSessionBatch,
    ) -> BoxFuture<'a, kp_net::Result<Option<u64>>> {
        let dir = library_dir.to_path_buf();
        let daemon_slot = Arc::clone(&self.embedder);
        Box::pin(async move {
            let end_offset = batch.end_byte_offset;
            let Some(tp) = batch.transcript_path.clone() else {
                return Ok(Some(end_offset));
            };
            let dir_capture = dir.clone();
            let session_id = batch.session_id.clone();
            let start_offset = batch.start_byte_offset;
            let batch_at = batch.at;
            let slot_for_capture = Arc::clone(&daemon_slot);
            let session_ended = batch.session_ended;
            let batch_actor = batch.actor.clone();
            let outcome = tokio::task::spawn_blocking(
                move || -> Result<kp_ingest::hooks::SessionSliceIngestOutcome, String> {
                    let mut store = Store::open(&dir_capture).map_err(|e| e.to_string())?;
                    crate::providers_cli::attach_or_refresh_daemon_embedder(
                        &mut store,
                        &dir_capture,
                        &slot_for_capture,
                    );
                    let global_policy = crate::governance::default_auto_route_policy();
                    kp_ingest::hooks::ingest_daemon_session_slice(
                        &mut store,
                        &kp_ingest::hooks::DaemonSessionSliceSpec {
                            raw_session: &batch.session_id,
                            transcript_path: Path::new(&tp),
                            start_byte_offset: batch.start_byte_offset,
                            end_byte_offset: batch.end_byte_offset,
                            actor: &batch.actor,
                            at: batch.at,
                            is_session_end: batch.session_ended,
                            tz: None,
                            global_policy: &global_policy,
                        },
                    )
                    .map_err(|e| e.to_string())
                },
            )
            .await
            .map_err(|e| kp_net::NetError::Daemon(e.to_string()))?
            .map_err(kp_net::NetError::Daemon)?;

            let committed_end = outcome.committed_end_byte_offset;
            if session_ended {
                crate::rsi::maybe_autospawn_dream_in_background(&dir, &batch_actor);
            }
            if committed_end <= start_offset {
                return Ok(Some(start_offset));
            }

            match crate::governance::drain_pending_extractions_for_daemon_with_embedder(
                &dir,
                batch_at,
                daemon_slot,
            )
            .await
            {
                Ok(true) => Ok(None),
                Ok(false) => {
                    let ro = Store::open_readonly(&dir)
                        .map_err(|e| kp_net::NetError::Daemon(e.to_string()))?;
                    let redactor = kp_model::SecretRedactor::new()
                        .map_err(|e| kp_net::NetError::Daemon(e.to_string()))?;
                    let clean_session = redactor.redact(&session_id);
                    let cur = ro
                        .get_session_cursor(&clean_session)
                        .map_err(|e| kp_net::NetError::Daemon(e.to_string()))?;
                    let cur_extracted = cur.map_or(0, |c| c.last_extracted_offset);
                    let (has_incomplete, has_paused_quota) = ro
                        .inspect_incomplete_session_spans_up_to(&clean_session, committed_end)
                        .map_err(|e| kp_net::NetError::Daemon(e.to_string()))?;
                    if has_incomplete {
                        if has_paused_quota {
                            return Ok(None);
                        }
                        return Err(kp_net::NetError::Daemon(
                            "session span extraction still leased or incomplete".into(),
                        ));
                    }
                    if crate::governance::default_auto_route_policy()
                        == kp_model::ModelRoutePolicy::RawOnly
                    {
                        return Ok(Some(committed_end));
                    }
                    let completed =
                        if outcome.subbatches_ingested == 0 && cur_extracted >= start_offset {
                            cur_extracted.max(committed_end)
                        } else {
                            cur_extracted
                        };
                    Ok(Some(completed))
                }
                Err(err) => Err(kp_net::NetError::Daemon(err)),
            }
        })
    }

    fn sync_and_derive<'a>(
        &'a self,
        library_dir: &'a Path,
        at: EpochMs,
    ) -> BoxFuture<'a, kp_net::Result<usize>> {
        let dir = library_dir.to_path_buf();
        let daemon_slot = Arc::clone(&self.embedder);
        Box::pin(async move {
            let mut store =
                Store::open(&dir).map_err(|e| kp_net::NetError::Daemon(e.to_string()))?;
            crate::providers_cli::attach_or_refresh_daemon_embedder(&mut store, &dir, &daemon_slot);
            crate::community::sync_joined_communities_and_derive(&dir, &mut store, at)
                .await
                .map_err(|e| kp_net::NetError::Daemon(e.to_string()))
        })
    }

    fn drain_queued_jobs<'a>(
        &'a self,
        library_dir: &'a Path,
        at: EpochMs,
    ) -> BoxFuture<'a, kp_net::Result<bool>> {
        let dir = library_dir.to_path_buf();
        let daemon_slot = Arc::clone(&self.embedder);
        Box::pin(async move {
            crate::governance::drain_pending_extractions_for_daemon_with_embedder(
                &dir,
                at,
                daemon_slot,
            )
            .await
            .map_err(kp_net::NetError::Daemon)
        })
    }

    fn ingest_jsonl<'a>(
        &'a self,
        library_dir: &'a Path,
        request: kp_net::IngestJsonlRequest,
    ) -> BoxFuture<'a, kp_net::Result<kp_net::IngestJsonlResult>> {
        let dir = library_dir.to_path_buf();
        let daemon_slot = Arc::clone(&self.embedder);
        Box::pin(async move {
            let dir_apply = dir.clone();
            let jsonl = request.jsonl;
            let requested_actor = request.actor;
            let extract_now = request.extract_now;
            let at = request
                .at
                .unwrap_or_else(|| crate::now().unwrap_or(EpochMs(0)));
            let slot_for_apply = Arc::clone(&daemon_slot);

            let report = tokio::task::spawn_blocking(
                move || -> Result<kp_store::PublicIngestStreamReport, String> {
                    let mut store = Store::open(&dir_apply).map_err(|e| e.to_string())?;
                    crate::providers_cli::attach_or_refresh_daemon_embedder(
                        &mut store,
                        &dir_apply,
                        &slot_for_apply,
                    );
                    let actor = match requested_actor {
                        Some(a) => a,
                        None => store.owner_actor().clone(),
                    };
                    if store
                        .member_role(&actor)
                        .map_err(|e| e.to_string())?
                        .is_none()
                    {
                        return Err("actor is not an active library member".into());
                    }
                    let global_policy = crate::governance::default_auto_route_policy();
                    let rep = store
                        .apply_public_ingest_jsonl(&jsonl, &actor, &global_policy)
                        .map_err(|e| e.to_string())?;
                    if rep.records_superseded > 0
                        || rep.records_withdrawn > 0
                        || rep.rosters_applied > 0
                    {
                        let _ = kp_derive::derive(&mut store);
                    }
                    Ok(rep)
                },
            )
            .await
            .map_err(|e| kp_net::NetError::Daemon(e.to_string()))?
            .map_err(kp_net::NetError::Daemon)?;

            if extract_now {
                let _ = crate::governance::drain_pending_extractions_for_daemon_with_embedder(
                    &dir,
                    at,
                    Arc::clone(&daemon_slot),
                )
                .await
                .map_err(kp_net::NetError::Daemon)?;
                let mut store =
                    Store::open(&dir).map_err(|e| kp_net::NetError::Daemon(e.to_string()))?;
                crate::providers_cli::attach_or_refresh_daemon_embedder(
                    &mut store,
                    &dir,
                    &daemon_slot,
                );
                let _ = crate::community::sync_joined_communities_and_derive(&dir, &mut store, at)
                    .await
                    .map_err(|e| kp_net::NetError::Daemon(e.to_string()))?;
            }

            Ok(kp_net::IngestJsonlResult {
                rosters_applied: report.rosters_applied,
                records_upserted: report.records_upserted,
                records_superseded: report.records_superseded,
                records_withdrawn: report.records_withdrawn,
                jobs_enqueued: report.jobs_enqueued,
                source_ids: report.source_ids,
            })
        })
    }

    fn embedder_status(&self) -> DaemonEmbedderRuntimeStatus {
        self.embedder
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|sw| sw.status_snapshot()))
            .unwrap_or_default()
    }

    fn refresh_store_embedder(&self, library_dir: &Path, store: &mut Store) {
        crate::providers_cli::attach_or_refresh_daemon_embedder(store, library_dir, &self.embedder);
    }
}

#[cfg(unix)]
const AUTOSTART_MARKER_FILE: &str = "daemon.autostart";
#[cfg(unix)]
const STOPPED_MARKER_FILE: &str = "daemon.stopped";
#[cfg(unix)]
pub(super) const SPAWNING_LOCK_FILE: &str = "daemon.spawning";

#[cfg(unix)]
pub(crate) fn enable_daemon_autostart(library_dir: &Path) {
    let _ = std::fs::remove_file(library_dir.join(STOPPED_MARKER_FILE));
    let _ = std::fs::write(library_dir.join(AUTOSTART_MARKER_FILE), b"1\n");
}

#[cfg(not(unix))]
pub(crate) fn enable_daemon_autostart(_library_dir: &Path) {}

#[cfg(unix)]
pub(super) fn disable_daemon_autostart(library_dir: &Path) {
    let _ = std::fs::write(library_dir.join(STOPPED_MARKER_FILE), b"1\n");
}

/// Detached `0ms`-wait background daemon launcher modeled on `claude-mem` / `Supermemory` worker
/// auto-start: acquires a short-lived `daemon.spawning` lockfile, spawns `kp daemon run` in a new
/// process group with `Stdio::null()` so Claude Code / Codex hook pipes never hang, and returns immediately.
#[cfg(unix)]
pub(super) fn maybe_autospawn_daemon_from_hook(library_dir: &Path) {
    #[cfg(unix)]
    if kp_net::is_daemon_lock_held(library_dir) {
        return;
    }
    if cfg!(test)
        || kp_ingest::hooks::is_internal_extraction()
        || std::env::var_os("KP_NO_AUTO_DAEMON").is_some()
        || library_dir.join(STOPPED_MARKER_FILE).exists()
    {
        return;
    }
    let should_autostart = library_dir.join(AUTOSTART_MARKER_FILE).exists()
        || kp_net::communities_config_path(library_dir).is_file()
        || std::env::var_os("KP_AUTO_DAEMON").is_some();
    if !should_autostart {
        return;
    }

    let lock_path = library_dir.join(SPAWNING_LOCK_FILE);
    if let Ok(meta) = std::fs::metadata(&lock_path) {
        let fresh = meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age < Duration::from_secs(10));
        if fresh {
            return;
        }
        let _ = std::fs::remove_file(&lock_path);
    }
    if std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .is_err()
    {
        return;
    }

    let Ok(exe) = std::env::current_exe() else {
        let _ = std::fs::remove_file(&lock_path);
        return;
    };
    let idle_secs = std::env::var("KP_DAEMON_IDLE_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(5);
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--path")
        .arg(library_dir)
        .args(["daemon", "run", "--idle-secs", &idle_secs.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    if cmd.spawn().is_err() {
        let _ = std::fs::remove_file(&lock_path);
    }
}

#[cfg(unix)]
pub(super) async fn ensure_daemon_started(
    cli: &Cli,
    idle_secs: u64,
) -> Result<(DaemonClient, PathBuf, bool), Box<dyn Error>> {
    let ro = Store::open_readonly(&cli.path)?;
    if let Some(ref actor) = cli.actor {
        if ro.member_role(actor)?.is_none() {
            return Err(StoreError::PermissionDenied.into());
        }
    }
    enable_daemon_autostart(&cli.path);
    let client = DaemonClient::for_library(&cli.path)?.with_timeout(Duration::from_millis(500));
    let sock = client.socket_path().to_path_buf();
    let already_running = client.ping().await.is_ok() || kp_net::is_daemon_lock_held(&cli.path);
    if !already_running {
        let exe = std::env::current_exe()?;
        let mut child = std::process::Command::new(exe)
            .arg("--path")
            .arg(&cli.path)
            .args(["daemon", "run", "--idle-secs", &idle_secs.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut ready = false;
        while Instant::now() < deadline {
            if client.ping().await.is_ok() {
                ready = true;
                break;
            }
            if let Some(status) = child.try_wait()? {
                return Err(format!("kp daemon exited prematurely with {status}").into());
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        if !ready {
            let _ = child.kill();
            let _ = child.wait();
            return Err("timed out waiting for kp daemon socket".into());
        }
    }
    Ok((client, sock, already_running))
}
