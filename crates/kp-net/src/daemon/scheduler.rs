use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{mpsc as std_mpsc, Arc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kp_model::{ActorId, EpochMs};
use kp_store::ExtractionJobStatus;
use tokio::sync::{mpsc, oneshot, watch};

use super::protocol::{
    CursorNotification, DaemonHandler, DaemonRequest, DaemonResponse, IdleSessionBatch,
};
use crate::NetError;

pub(super) struct ScanSnapshot {
    pub data_version: Option<i64>,
    pub rows: Vec<kp_store::UnextractedSessionCursor>,
    pub pending_jobs: u64,
    pub pending_outbox: u64,
    pub needs_alignment: bool,
}

pub(super) enum InspectorQueryError {
    Forbidden,
    NotFound,
    Failed,
}

pub(super) struct InspectorOverviewParams {
    pub query: Option<String>,
    pub limit: usize,
    pub target_filter: Option<String>,
    pub at: EpochMs,
}

pub(super) enum ActorCommand {
    Rpc {
        request: DaemonRequest,
        reply: oneshot::Sender<(DaemonResponse, Option<IdleScheduleSignal>)>,
    },
    MarkExtracted {
        session_id: String,
        prefix_sha256: String,
        extracted_offset: u64,
        reply: oneshot::Sender<(bool, bool)>,
    },
    RecordFailure {
        session_id: String,
        paused_quota: bool,
        error: Option<String>,
        at: EpochMs,
        reply: oneshot::Sender<(ExtractionJobStatus, u32)>,
    },
    ScanUnextracted {
        since_data_version: Option<i64>,
        reply: oneshot::Sender<Option<ScanSnapshot>>,
    },
    InspectIdleWork {
        reply: oneshot::Sender<ScanSnapshot>,
    },
    ResolveIngestActor {
        requested: Option<ActorId>,
        reply: oneshot::Sender<std::result::Result<ActorId, String>>,
    },
    ResolveInspectorViewer {
        requested: Option<ActorId>,
        reply: oneshot::Sender<std::result::Result<ActorId, String>>,
    },
    InspectorVerifyMember {
        viewer: ActorId,
        reply: oneshot::Sender<bool>,
    },
    InspectorOverview {
        viewer: ActorId,
        params: InspectorOverviewParams,
        reply: oneshot::Sender<std::result::Result<serde_json::Value, InspectorQueryError>>,
    },
    InspectorWhy {
        viewer: ActorId,
        raw_fact_id: String,
        at: EpochMs,
        reply: oneshot::Sender<std::result::Result<serde_json::Value, InspectorQueryError>>,
    },
    Stop,
}

#[derive(Clone, Debug)]
pub(super) enum IdleScheduleSignal {
    ClientActivity,
    WebInspectorActive,
    IngestStarted,
    IngestFinished {
        wake_jobs_drain: bool,
        wake_sync: bool,
    },
    Cursor {
        notification: CursorNotification,
        start_byte_offset: u64,
    },
    TriggerSync,
}

struct PendingSessionEntry {
    batch: IdleSessionBatch,
    deadline: tokio::time::Instant,
}

enum BatchOutcome {
    Extracted(u64),
    PausedQuota(Option<String>),
    TransientFailure(String),
}

fn now_epoch_ms() -> EpochMs {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(0))
        .unwrap_or(0);
    EpochMs(ms)
}

fn is_quota_error(err: &NetError) -> bool {
    let lower = err.to_string().to_ascii_lowercase();
    lower.contains("quota")
        || lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("429")
        || lower.contains("too many requests")
}

fn retry_backoff_delay(idle_window: Duration, attempts: u32) -> Duration {
    let base = (idle_window / 2).clamp(Duration::from_millis(20), Duration::from_secs(5));
    let shift = attempts.saturating_sub(1).min(8);
    base.saturating_mul(1u32 << shift)
        .min(Duration::from_secs(300))
}

fn quota_cooldown_delay(idle_window: Duration) -> Duration {
    idle_window
        .saturating_mul(3)
        .clamp(Duration::from_millis(90), Duration::from_secs(300))
}

fn periodic_scan_interval(idle_window: Duration) -> Duration {
    idle_window.clamp(Duration::from_millis(50), Duration::from_secs(5))
}

fn periodic_sync_interval(idle_window: Duration) -> Duration {
    idle_window.clamp(Duration::from_millis(100), Duration::from_secs(5))
}

fn seed_pending_from_rows(
    rows: Vec<kp_store::UnextractedSessionCursor>,
    pending: &mut BTreeMap<String, PendingSessionEntry>,
    in_flight: &BTreeSet<String>,
    idle_window: Duration,
    is_startup: bool,
) {
    let now_inst = tokio::time::Instant::now();
    for kp_store::UnextractedSessionCursor {
        cursor,
        actor,
        session_ended,
        status,
        attempts,
    } in rows
    {
        if cursor.last_byte_offset <= cursor.last_extracted_offset {
            continue;
        }
        if in_flight.contains(&cursor.session_id) {
            continue;
        }
        if let Some(existing) = pending.get_mut(&cursor.session_id) {
            if cursor.last_byte_offset > existing.batch.end_byte_offset {
                existing.batch.end_byte_offset = cursor.last_byte_offset;
                existing.batch.line_count = cursor.last_line_count;
            }
            continue;
        }
        let deadline = if session_ended
            || is_startup && attempts == 0 && status == ExtractionJobStatus::Pending
        {
            now_inst
        } else if status == ExtractionJobStatus::PausedQuota {
            now_inst + quota_cooldown_delay(idle_window)
        } else if attempts > 0 {
            now_inst + retry_backoff_delay(idle_window, attempts)
        } else {
            now_inst + idle_window
        };
        pending.insert(
            cursor.session_id.clone(),
            PendingSessionEntry {
                batch: IdleSessionBatch {
                    session_id: cursor.session_id,
                    client_kind: cursor.client_kind,
                    transcript_path: cursor.transcript_path,
                    start_byte_offset: cursor.last_extracted_offset,
                    end_byte_offset: cursor.last_byte_offset,
                    line_count: cursor.last_line_count,
                    prefix_sha256: cursor.prefix_sha256,
                    actor,
                    at: cursor.last_activity_at,
                    session_ended,
                },
                deadline,
            },
        );
    }
}

pub(super) struct IdleSchedulerParams {
    pub library_dir: PathBuf,
    pub idle_window: Duration,
    pub idle_shutdown_timeout: Option<Duration>,
    pub handler: Arc<dyn DaemonHandler>,
    pub actor_tx: std_mpsc::Sender<ActorCommand>,
    pub sched_rx: mpsc::UnboundedReceiver<IdleScheduleSignal>,
    pub shutdown_tx: watch::Sender<bool>,
    pub shutdown_rx: watch::Receiver<bool>,
}

struct SchedulerState {
    pending: BTreeMap<String, PendingSessionEntry>,
    in_flight: BTreeSet<String>,
    last_data_version: Option<i64>,
    last_drained_dv: Option<i64>,
    last_synced_dv: Option<i64>,
    web_inspector_active: bool,
    ingest_in_flight: usize,
    jobs_drain_queued: bool,
    jobs_drain_deadline: Option<tokio::time::Instant>,
    sync_queued: bool,
    sync_is_periodic_only: bool,
    idle_since: tokio::time::Instant,
}

impl SchedulerState {
    fn new() -> Self {
        Self {
            pending: BTreeMap::new(),
            in_flight: BTreeSet::new(),
            last_data_version: None,
            last_drained_dv: None,
            last_synced_dv: None,
            web_inspector_active: false,
            ingest_in_flight: 0,
            jobs_drain_queued: false,
            jobs_drain_deadline: None,
            sync_queued: false,
            sync_is_periodic_only: false,
            idle_since: tokio::time::Instant::now(),
        }
    }

    async fn scan_and_seed_unextracted_cursors(
        &mut self,
        actor_tx: &std_mpsc::Sender<ActorCommand>,
        idle_window: Duration,
        is_startup: bool,
    ) -> (u64, bool, u64) {
        let since_data_version = if is_startup {
            None
        } else {
            self.last_data_version
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        if actor_tx
            .send(ActorCommand::ScanUnextracted {
                since_data_version,
                reply: reply_tx,
            })
            .is_err()
        {
            return (0, false, 0);
        }
        let Ok(Some(snap)) = reply_rx.await else {
            return (0, false, 0);
        };
        if snap.data_version.is_some() {
            self.last_data_version = snap.data_version;
        }
        seed_pending_from_rows(
            snap.rows,
            &mut self.pending,
            &self.in_flight,
            idle_window,
            is_startup,
        );
        (snap.pending_jobs, snap.needs_alignment, snap.pending_outbox)
    }

    fn apply_schedule_signal(&mut self, sig: IdleScheduleSignal, idle_window: Duration) {
        let now = tokio::time::Instant::now();
        self.idle_since = now;
        match sig {
            IdleScheduleSignal::ClientActivity => {}
            IdleScheduleSignal::WebInspectorActive => {
                self.web_inspector_active = true;
            }
            IdleScheduleSignal::IngestStarted => {
                self.ingest_in_flight = self.ingest_in_flight.saturating_add(1);
            }
            IdleScheduleSignal::IngestFinished {
                wake_jobs_drain,
                wake_sync,
            } => {
                self.ingest_in_flight = self.ingest_in_flight.saturating_sub(1);
                if wake_jobs_drain {
                    self.jobs_drain_deadline = None;
                    self.jobs_drain_queued = true;
                    self.last_drained_dv = None;
                }
                if wake_sync {
                    self.sync_queued = true;
                    self.sync_is_periodic_only = false;
                    self.last_synced_dv = None;
                }
            }
            IdleScheduleSignal::TriggerSync => {
                self.sync_queued = true;
                self.sync_is_periodic_only = false;
            }
            IdleScheduleSignal::Cursor {
                notification,
                start_byte_offset,
            } => {
                let sid = notification.session_id.clone();
                let immediate = notification.session_ended;
                let deadline = if immediate { now } else { now + idle_window };
                self.pending
                    .entry(sid)
                    .and_modify(|existing| {
                        if existing.batch.prefix_sha256 != notification.prefix_sha256
                            && !existing.batch.prefix_sha256.is_empty()
                            && !notification.prefix_sha256.is_empty()
                        {
                            existing.batch.start_byte_offset = start_byte_offset;
                        }
                        existing.batch.client_kind = notification.client_kind.clone();
                        if notification.transcript_path.is_some() {
                            existing.batch.transcript_path = notification.transcript_path.clone();
                        }
                        existing.batch.end_byte_offset = notification.byte_offset;
                        existing.batch.line_count = notification.line_count;
                        existing.batch.prefix_sha256 = notification.prefix_sha256.clone();
                        existing.batch.actor = notification.actor.clone();
                        existing.batch.at = notification.at;
                        existing.batch.session_ended |= immediate;
                        existing.deadline = deadline;
                    })
                    .or_insert_with(|| PendingSessionEntry {
                        batch: IdleSessionBatch {
                            session_id: notification.session_id,
                            client_kind: notification.client_kind,
                            transcript_path: notification.transcript_path,
                            start_byte_offset,
                            end_byte_offset: notification.byte_offset,
                            line_count: notification.line_count,
                            prefix_sha256: notification.prefix_sha256,
                            actor: notification.actor,
                            at: notification.at,
                            session_ended: immediate,
                        },
                        deadline,
                    });
            }
        }
    }
}

enum JobsDrainOutcome {
    Drained,
    PausedQuota,
    TransientFailure,
}

pub(super) async fn run_idle_scheduler(params: IdleSchedulerParams) {
    let IdleSchedulerParams {
        library_dir,
        idle_window,
        idle_shutdown_timeout,
        handler,
        actor_tx,
        mut sched_rx,
        shutdown_tx,
        mut shutdown_rx,
    } = params;
    let mut state = SchedulerState::new();
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<(IdleSessionBatch, BatchOutcome)>();

    // Startup scan: resume unextracted `session_cursors`, fallback `extraction_queue` jobs, pending outbox, and dirty `needs_alignment`.
    let (startup_pending_jobs, startup_needs_alignment, startup_pending_outbox) = state
        .scan_and_seed_unextracted_cursors(&actor_tx, idle_window, true)
        .await;
    let mut last_seen_pending_outbox = startup_pending_outbox;

    let mut scan_tick = tokio::time::interval(periodic_scan_interval(idle_window));
    scan_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Consume the immediate first tick since we already ran the startup scan above.
    scan_tick.tick().await;

    let mut sync_tick = tokio::time::interval(periodic_sync_interval(idle_window));
    sync_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    sync_tick.tick().await;

    state.jobs_drain_queued = startup_pending_jobs > 0;
    let mut jobs_drain_in_flight = false;
    let mut jobs_drain_attempts: u32 = 0;
    let (jobs_done_tx, mut jobs_done_rx) = mpsc::unbounded_channel::<JobsDrainOutcome>();

    let mut sync_in_flight = false;
    let mut sync_started_dv: Option<i64> = None;
    state.sync_queued = startup_needs_alignment
        || startup_pending_outbox > 0
        || crate::communities::communities_config_path(&library_dir).is_file();
    let (sync_done_tx, mut sync_done_rx) = mpsc::unbounded_channel::<(usize, Duration, bool)>();

    loop {
        if !library_dir.join("kp.sqlite").exists() {
            break;
        }

        if !jobs_drain_in_flight
            && state
                .jobs_drain_deadline
                .is_some_and(|dl| dl <= tokio::time::Instant::now())
        {
            state.jobs_drain_deadline = None;
            state.jobs_drain_queued = true;
        }

        if state.jobs_drain_queued && !jobs_drain_in_flight {
            state.jobs_drain_queued = false;
            jobs_drain_in_flight = true;
            state.last_drained_dv = state.last_data_version;
            let handler_jobs = Arc::clone(&handler);
            let dir_jobs = library_dir.clone();
            let done_jobs = jobs_done_tx.clone();
            let at = now_epoch_ms();
            tokio::spawn(async move {
                let outcome = match handler_jobs.drain_queued_jobs(&dir_jobs, at).await {
                    Ok(false) => JobsDrainOutcome::Drained,
                    Ok(true) => JobsDrainOutcome::PausedQuota,
                    Err(err) if is_quota_error(&err) => JobsDrainOutcome::PausedQuota,
                    Err(_) => JobsDrainOutcome::TransientFailure,
                };
                let _ = done_jobs.send(outcome);
            });
        }

        if state.sync_queued && !sync_in_flight {
            let was_periodic_only = state.sync_is_periodic_only;
            state.sync_queued = false;
            state.sync_is_periodic_only = false;
            sync_in_flight = true;
            sync_started_dv = state.last_data_version;
            let handler_sync = Arc::clone(&handler);
            let dir_sync = library_dir.clone();
            let done_sync = sync_done_tx.clone();
            let at = now_epoch_ms();
            tokio::spawn(async move {
                let started = tokio::time::Instant::now();
                let count = handler_sync
                    .sync_and_derive(&dir_sync, at)
                    .await
                    .unwrap_or(0);
                let _ = done_sync.send((count, started.elapsed(), was_periodic_only));
            });
        }

        // Dispatch any ready entries before waiting so startup-recovered sessions run immediately.
        let now = tokio::time::Instant::now();
        let ready: Vec<String> = state
            .pending
            .iter()
            .filter(|(sid, entry)| !state.in_flight.contains(*sid) && entry.deadline <= now)
            .map(|(sid, _)| sid.clone())
            .collect();

        for sid in ready {
            let Some(entry) = state.pending.remove(&sid) else {
                continue;
            };
            state.in_flight.insert(sid);
            let handler_clone = Arc::clone(&handler);
            let dir_clone = library_dir.clone();
            let done_tx_clone = done_tx.clone();
            let batch = entry.batch;
            tokio::spawn(async move {
                let outcome = match handler_clone
                    .extract_session_batch(&dir_clone, batch.clone())
                    .await
                {
                    Ok(Some(offset)) => BatchOutcome::Extracted(offset),
                    Ok(None) => BatchOutcome::PausedQuota(None),
                    Err(err) if is_quota_error(&err) => {
                        BatchOutcome::PausedQuota(Some(err.to_string()))
                    }
                    Err(err) => BatchOutcome::TransientFailure(err.to_string()),
                };
                let _ = done_tx_clone.send((batch, outcome));
            });
        }

        let is_busy = state.web_inspector_active
            || state.ingest_in_flight > 0
            || !state.in_flight.is_empty()
            || !state.pending.is_empty()
            || state.jobs_drain_queued
            || jobs_drain_in_flight
            || state.jobs_drain_deadline.is_some()
            || sync_in_flight
            || state.sync_queued;

        if let Some(timeout) = idle_shutdown_timeout {
            if !is_busy && state.idle_since.elapsed() >= timeout {
                let (reply_tx, reply_rx) = oneshot::channel();
                if actor_tx
                    .send(ActorCommand::InspectIdleWork { reply: reply_tx })
                    .is_err()
                {
                    break;
                }
                let Ok(snap) = reply_rx.await else {
                    break;
                };
                if let Ok(sig) = sched_rx.try_recv() {
                    state.apply_schedule_signal(sig, idle_window);
                    continue;
                }
                if snap.data_version.is_some() {
                    state.last_data_version = snap.data_version;
                }
                last_seen_pending_outbox = snap.pending_outbox;
                let pending_extractions = u64::try_from(snap.rows.len())
                    .unwrap_or(u64::MAX)
                    .saturating_add(snap.pending_jobs);
                let unsynced_alignment = snap.needs_alignment
                    && (snap.data_version.is_none() || snap.data_version != state.last_synced_dv);
                if pending_extractions == 0 && snap.pending_outbox == 0 && !unsynced_alignment {
                    let _ = shutdown_tx.send(true);
                    break;
                }
                seed_pending_from_rows(
                    snap.rows,
                    &mut state.pending,
                    &state.in_flight,
                    idle_window,
                    false,
                );
                if snap.pending_jobs > 0
                    && (snap.data_version.is_none() || snap.data_version != state.last_drained_dv)
                {
                    state.jobs_drain_queued = true;
                }
                if unsynced_alignment {
                    state.sync_queued = true;
                    state.sync_is_periodic_only = false;
                }
                state.idle_since = tokio::time::Instant::now();
                continue;
            }
        }

        let pending_dl = state
            .pending
            .iter()
            .filter(|(sid, _)| !state.in_flight.contains(*sid))
            .map(|(_, entry)| entry.deadline)
            .min();
        let jobs_dl = state.jobs_drain_deadline.filter(|_| !jobs_drain_in_flight);
        let idle_dl = idle_shutdown_timeout
            .filter(|_| !is_busy)
            .map(|timeout| state.idle_since + timeout);
        let next_deadline = [pending_dl, jobs_dl, idle_dl].into_iter().flatten().min();

        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            _ = scan_tick.tick() => {
                if !library_dir.join("kp.sqlite").exists() {
                    break;
                }
                let (pending_jobs, needs_alignment, pending_outbox) = state
                    .scan_and_seed_unextracted_cursors(&actor_tx, idle_window, false)
                    .await;
                last_seen_pending_outbox = pending_outbox;
                if pending_jobs > 0 && !jobs_drain_in_flight && state.jobs_drain_deadline.is_none() {
                    state.jobs_drain_queued = true;
                }
                if needs_alignment || pending_outbox > 0 {
                    state.sync_queued = true;
                    state.sync_is_periodic_only = false;
                }
            }
            _ = sync_tick.tick() => {
                if (crate::communities::communities_config_path(&library_dir).is_file()
                    || last_seen_pending_outbox > 0)
                    && !state.sync_queued
                {
                    state.sync_queued = true;
                    state.sync_is_periodic_only = true;
                }
            }
            Some((changed_count, elapsed, was_periodic_only)) = sync_done_rx.recv() => {
                sync_in_flight = false;
                state.last_synced_dv = sync_started_dv;
                if !was_periodic_only || changed_count > 0 {
                    state.idle_since = tokio::time::Instant::now();
                } else {
                    state.idle_since += elapsed;
                }
            }
            Some(jobs_outcome) = jobs_done_rx.recv() => {
                jobs_drain_in_flight = false;
                state.idle_since = tokio::time::Instant::now();
                match jobs_outcome {
                    JobsDrainOutcome::Drained => {
                        jobs_drain_attempts = 0;
                        state.jobs_drain_deadline = None;
                        let (reply_tx, reply_rx) = oneshot::channel();
                        if actor_tx
                            .send(ActorCommand::InspectIdleWork { reply: reply_tx })
                            .is_ok()
                        {
                            if let Ok(snap) = reply_rx.await {
                                if snap.data_version.is_some() {
                                    state.last_data_version = snap.data_version;
                                }
                                if snap.pending_jobs > 0
                                    && snap.data_version.is_some()
                                    && snap.data_version != state.last_drained_dv
                                {
                                    state.jobs_drain_queued = true;
                                }
                                if snap.needs_alignment
                                    || crate::communities::communities_config_path(&library_dir)
                                        .is_file()
                                {
                                    state.sync_queued = true;
                                    state.sync_is_periodic_only = false;
                                }
                            }
                        } else if crate::communities::communities_config_path(&library_dir).is_file()
                        {
                            state.sync_queued = true;
                            state.sync_is_periodic_only = false;
                        }
                    }
                    JobsDrainOutcome::PausedQuota => {
                        state.jobs_drain_deadline =
                            Some(tokio::time::Instant::now() + quota_cooldown_delay(idle_window));
                    }
                    JobsDrainOutcome::TransientFailure => {
                        jobs_drain_attempts = jobs_drain_attempts.saturating_add(1);
                        state.jobs_drain_deadline = Some(
                            tokio::time::Instant::now()
                                + retry_backoff_delay(idle_window, jobs_drain_attempts),
                        );
                    }
                }
            }
            maybe_sig = sched_rx.recv() => {
                let Some(sig) = maybe_sig else {
                    break;
                };
                state.apply_schedule_signal(sig, idle_window);
            }
            Some((finished_batch, outcome)) = done_rx.recv() => {
                let sid = finished_batch.session_id.clone();
                state.in_flight.remove(&sid);
                state.idle_since = tokio::time::Instant::now();
                match outcome {
                    BatchOutcome::Extracted(extracted_offset) => {
                        let mut needs_alignment = false;
                        let (reply_tx, reply_rx) = oneshot::channel();
                        if actor_tx
                            .send(ActorCommand::MarkExtracted {
                                session_id: sid.clone(),
                                prefix_sha256: finished_batch.prefix_sha256.clone(),
                                extracted_offset,
                                reply: reply_tx,
                            })
                            .is_ok()
                        {
                            if let Ok((_more, dirty_align)) = reply_rx.await {
                                needs_alignment = dirty_align;
                            }
                        }
                        if let Some(entry) = state.pending.get_mut(&sid) {
                            if entry.batch.end_byte_offset <= extracted_offset {
                                state.pending.remove(&sid);
                            } else {
                                entry.batch.start_byte_offset = extracted_offset;
                            }
                        }
                        if needs_alignment
                            || crate::communities::communities_config_path(&library_dir).is_file()
                        {
                            state.sync_queued = true;
                            state.sync_is_periodic_only = false;
                        }
                    }
                    BatchOutcome::PausedQuota(err_msg) => {
                        let (reply_tx, reply_rx) = oneshot::channel();
                        if actor_tx
                            .send(ActorCommand::RecordFailure {
                                session_id: sid.clone(),
                                paused_quota: true,
                                error: err_msg,
                                at: now_epoch_ms(),
                                reply: reply_tx,
                            })
                            .is_ok()
                        {
                            let _ = reply_rx.await;
                        }
                        let cooldown_dl = tokio::time::Instant::now() + quota_cooldown_delay(idle_window);
                        state
                            .pending
                            .entry(sid)
                            .and_modify(|existing| {
                                if !existing.batch.session_ended {
                                    existing.deadline = cooldown_dl;
                                }
                            })
                            .or_insert(PendingSessionEntry {
                                batch: finished_batch,
                                deadline: cooldown_dl,
                            });
                    }
                    BatchOutcome::TransientFailure(err_msg) => {
                        let mut attempts = 1u32;
                        let (reply_tx, reply_rx) = oneshot::channel();
                        if actor_tx
                            .send(ActorCommand::RecordFailure {
                                session_id: sid.clone(),
                                paused_quota: false,
                                error: Some(err_msg),
                                at: now_epoch_ms(),
                                reply: reply_tx,
                            })
                            .is_ok()
                        {
                            if let Ok((_st, recorded_attempts)) = reply_rx.await {
                                attempts = recorded_attempts;
                            }
                        }
                        let backoff_dl =
                            tokio::time::Instant::now() + retry_backoff_delay(idle_window, attempts);
                        state
                            .pending
                            .entry(sid)
                            .and_modify(|existing| {
                                existing.deadline = backoff_dl;
                            })
                            .or_insert(PendingSessionEntry {
                                batch: finished_batch,
                                deadline: backoff_dl,
                            });
                    }
                }
            }
            () = async {
                if let Some(dl) = next_deadline {
                    tokio::time::sleep_until(dl).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {}
        }
    }
}
