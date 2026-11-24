use std::path::Path;

use kp_model::{ActorId, EpochMs};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

use super::sha256_file_prefix;
use crate::{Result, Store};

/// Parameters for atomically updating a tracked coding-agent session cursor.
#[derive(Clone, Debug)]
pub struct SessionCursorUpdate<'a> {
    pub session_id: &'a str,
    pub client_kind: &'a str,
    pub transcript_path: Option<&'a str>,
    pub byte_offset: u64,
    pub line_count: u64,
    pub prefix_sha256: &'a str,
    pub prev_byte_offset: Option<u64>,
    pub prev_prefix_sha256: Option<&'a str>,
    pub actor: Option<&'a ActorId>,
    pub session_ended: bool,
    pub at: EpochMs,
}

/// Unextracted session cursor row returned by [`Store::daemon_unextracted_session_cursors`].
#[derive(Clone, Debug)]
pub struct UnextractedSessionCursor {
    pub cursor: crate::SessionCursor,
    pub actor: ActorId,
    pub session_ended: bool,
    pub status: crate::ExtractionJobStatus,
    pub attempts: u32,
}

impl Store {
    /// Updates `session_cursors` atomically in `<1ms`, optionally verifying incremental prefix hashes
    /// across real transcript appends and recording scheduler metadata (`actor`, `session_ended`).
    /// Resets `last_extracted_offset` only when the transcript shrank or its previously hashed prefix
    /// was rewritten. Returns `(last_extracted_offset, has_unextracted_delta)`.
    pub fn daemon_record_session_cursor(
        &mut self,
        update: &SessionCursorUpdate<'_>,
    ) -> Result<(u64, bool)> {
        let SessionCursorUpdate {
            session_id,
            client_kind,
            transcript_path,
            byte_offset,
            line_count,
            prefix_sha256,
            prev_byte_offset,
            prev_prefix_sha256,
            actor,
            session_ended,
            at,
        } = *update;
        if session_id.trim().is_empty() {
            return Err(
                kp_model::KpError::invalid("session_id", "session_id must be nonempty").into(),
            );
        }
        let byte_i64 = i64::try_from(byte_offset)
            .map_err(|_| kp_model::KpError::invalid("byte_offset", "offset overflow"))?;
        let line_i64 = i64::try_from(line_count)
            .map_err(|_| kp_model::KpError::invalid("line_count", "line count overflow"))?;
        let kind = if client_kind.trim().is_empty() {
            "unknown"
        } else {
            client_kind
        };
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(i64, String, i64, Option<String>)> = tx
            .query_row(
                "SELECT last_byte_offset, prefix_sha256, last_extracted_offset, transcript_path
                 FROM session_cursors WHERE session_id=?1",
                [session_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let mut was_rewritten = false;
        let extracted_i64 = match existing {
            Some((old_byte_offset, old_prefix, old_extracted, old_path)) => {
                let old_byte_u64 = u64::try_from(old_byte_offset).unwrap_or(0);
                let effective_path = transcript_path.or(old_path.as_deref());
                let prefix_preserved = if byte_i64 >= old_byte_offset && byte_i64 >= old_extracted {
                    if old_byte_offset == 0
                        || old_prefix.is_empty()
                        || prefix_sha256.is_empty()
                        || old_prefix == prefix_sha256
                        || (prev_byte_offset == Some(old_byte_u64)
                            && prev_prefix_sha256 == Some(old_prefix.as_str()))
                    {
                        true
                    } else if let Some(p) = effective_path.filter(|p| !p.is_empty()) {
                        sha256_file_prefix(Path::new(p), old_byte_u64)?.as_deref()
                            == Some(old_prefix.as_str())
                    } else {
                        false
                    }
                } else {
                    false
                };
                if prefix_preserved {
                    old_extracted
                } else {
                    was_rewritten = true;
                    0
                }
            }
            None => 0,
        };
        if was_rewritten {
            tx.execute(
                "DELETE FROM change_log WHERE op='session_source_span' AND logical_key=?1",
                [session_id],
            )?;
        }
        tx.execute(
            "INSERT INTO session_cursors (session_id,client_kind,transcript_path,last_byte_offset,last_line_count,prefix_sha256,last_activity_at,last_extracted_offset)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(session_id) DO UPDATE SET
                 client_kind=excluded.client_kind,
                 transcript_path=COALESCE(excluded.transcript_path, session_cursors.transcript_path),
                 last_byte_offset=excluded.last_byte_offset,
                 last_line_count=excluded.last_line_count,
                 prefix_sha256=excluded.prefix_sha256,
                 last_activity_at=excluded.last_activity_at,
                 last_extracted_offset=excluded.last_extracted_offset",
            params![
                session_id,
                kind,
                transcript_path,
                byte_i64,
                line_i64,
                prefix_sha256,
                at.0,
                extracted_i64
            ],
        )?;
        let prev_meta = read_session_cursor_meta_tx(&tx, session_id)?;
        let resolved_actor = match actor {
            Some(a) => a.as_str().to_owned(),
            None => match prev_meta.as_ref().and_then(|m| m.actor.clone()) {
                Some(prev_actor) => prev_actor,
                None => tx.query_row("SELECT owner_actor FROM library_meta LIMIT 1", [], |r| {
                    r.get(0)
                })?,
            },
        };
        let last_extracted = u64::try_from(extracted_i64).unwrap_or(0);
        let has_delta = byte_offset > last_extracted;
        let merged_ended = if has_delta {
            session_ended || prev_meta.as_ref().is_some_and(|m| m.session_ended)
        } else {
            false
        };
        let next_status = if has_delta {
            crate::ExtractionJobStatus::Pending
        } else {
            crate::ExtractionJobStatus::Done
        };
        write_session_cursor_meta_tx(
            &tx,
            session_id,
            &resolved_actor,
            &SessionCursorMeta {
                actor: Some(resolved_actor.clone()),
                session_ended: merged_ended,
                status: next_status,
                attempts: 0,
                last_error: None,
            },
            at,
        )?;
        tx.commit()?;
        Ok((last_extracted, has_delta))
    }

    /// Advances `last_extracted_offset` for `session_id` if `0..extracted_offset` still matches
    /// `prefix_sha256` (including when concurrent appends advanced `session_cursors.prefix_sha256`).
    /// Returns `true` if newer unextracted bytes remain (`last_byte_offset > last_extracted_offset`).
    pub fn daemon_mark_session_extracted(
        &mut self,
        session_id: &str,
        prefix_sha256: &str,
        extracted_offset: u64,
    ) -> Result<bool> {
        let extracted_i64 = i64::try_from(extracted_offset)
            .map_err(|_| kp_model::KpError::invalid("extracted_offset", "offset overflow"))?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<(i64, i64, String, Option<String>, i64)> = tx
            .query_row(
                "SELECT last_byte_offset, last_extracted_offset, prefix_sha256, transcript_path, last_activity_at
                 FROM session_cursors WHERE session_id=?1",
                [session_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let Some((cur_byte, cur_extracted, cur_prefix, cur_path, activity_ms)) = current else {
            tx.commit()?;
            return Ok(false);
        };

        let can_advance = if cur_byte >= extracted_i64 {
            if prefix_sha256.is_empty() {
                true
            } else if let Some(p) = cur_path
                .as_deref()
                .filter(|p| !p.is_empty() && Path::new(p).exists())
            {
                sha256_file_prefix(Path::new(p), extracted_offset)?.as_deref()
                    == Some(prefix_sha256)
            } else {
                cur_prefix == prefix_sha256 || cur_prefix.is_empty()
            }
        } else {
            false
        };

        if !can_advance {
            tx.commit()?;
            return Ok(cur_byte > cur_extracted);
        }

        let new_extracted = cur_extracted.max(extracted_i64.min(cur_byte));
        tx.execute(
            "UPDATE session_cursors SET last_extracted_offset = ?1 WHERE session_id = ?2",
            params![new_extracted, session_id],
        )?;
        let more = cur_byte > new_extracted;
        sync_session_cursor_meta_after_advance_tx(&tx, session_id, more, activity_ms)?;
        tx.commit()?;
        Ok(more)
    }

    /// Records a failed or quota-paused session extraction attempt (`attempts + 1`, `paused_quota` vs `pending`).
    pub fn daemon_record_session_extraction_failure(
        &mut self,
        session_id: &str,
        paused_quota: bool,
        last_error: Option<&str>,
        at: EpochMs,
    ) -> Result<(crate::ExtractionJobStatus, u32)> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prev = read_session_cursor_meta_tx(&tx, session_id)?;
        let actor_str = match prev.as_ref().and_then(|m| m.actor.clone()) {
            Some(a) => a,
            None => tx.query_row("SELECT owner_actor FROM library_meta LIMIT 1", [], |r| {
                r.get(0)
            })?,
        };
        let attempts = prev
            .as_ref()
            .map(|m| m.attempts)
            .unwrap_or(0)
            .saturating_add(1);
        let status = if paused_quota {
            crate::ExtractionJobStatus::PausedQuota
        } else {
            crate::ExtractionJobStatus::Pending
        };
        write_session_cursor_meta_tx(
            &tx,
            session_id,
            &actor_str,
            &SessionCursorMeta {
                actor: Some(actor_str.clone()),
                session_ended: prev.as_ref().is_some_and(|m| m.session_ended),
                status,
                attempts,
                last_error: last_error.map(str::to_owned),
            },
            at,
        )?;
        tx.commit()?;
        Ok((status, attempts))
    }

    /// Reads the persisted retry/quota state `(status, attempts, last_error)` for `session_id`.
    pub fn daemon_session_cursor_retry_state(
        &self,
        session_id: &str,
    ) -> Result<Option<(crate::ExtractionJobStatus, u32, Option<String>)>> {
        Ok(read_session_cursor_meta_tx(&self.db, session_id)?
            .map(|m| (m.status, m.attempts, m.last_error)))
    }

    /// Scans all `session_cursors` where `last_byte_offset > last_extracted_offset` for daemon
    /// startup recovery and periodic background scanning.
    pub fn daemon_unextracted_session_cursors(&self) -> Result<Vec<UnextractedSessionCursor>> {
        let default_owner: ActorId = self
            .db
            .query_row("SELECT owner_actor FROM library_meta LIMIT 1", [], |r| {
                r.get::<_, String>(0)
            })?
            .parse()?;
        let mut stmt = self.db.prepare(
            "SELECT session_id, client_kind, transcript_path, last_byte_offset, last_line_count,
                    prefix_sha256, last_activity_at, last_extracted_offset
             FROM session_cursors
             WHERE last_byte_offset > last_extracted_offset
             ORDER BY last_activity_at ASC, session_id ASC",
        )?;
        let mut rows = stmt.query([])?;
        let mut raw_cursors = Vec::new();
        while let Some(row) = rows.next()? {
            raw_cursors.push(crate::SessionCursor {
                session_id: row.get(0)?,
                client_kind: row.get(1)?,
                transcript_path: row.get(2)?,
                last_byte_offset: u64::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
                last_line_count: u64::try_from(row.get::<_, i64>(4)?).unwrap_or(0),
                prefix_sha256: row.get(5)?,
                last_activity_at: EpochMs(row.get(6)?),
                last_extracted_offset: u64::try_from(row.get::<_, i64>(7)?).unwrap_or(0),
            });
        }
        drop(rows);
        drop(stmt);

        let mut out = Vec::with_capacity(raw_cursors.len());
        for cursor in raw_cursors {
            let meta = read_session_cursor_meta_tx(&self.db, &cursor.session_id)?;
            let actor = meta
                .as_ref()
                .and_then(|m| m.actor.as_deref())
                .and_then(|a| a.parse::<ActorId>().ok())
                .unwrap_or_else(|| default_owner.clone());
            let session_ended = meta.as_ref().is_some_and(|m| m.session_ended);
            let status = meta
                .as_ref()
                .map(|m| m.status)
                .unwrap_or(crate::ExtractionJobStatus::Pending);
            let attempts = meta.as_ref().map(|m| m.attempts).unwrap_or(0);
            out.push(UnextractedSessionCursor {
                cursor,
                actor,
                session_ended,
                status,
                attempts,
            });
        }
        Ok(out)
    }

    pub fn daemon_queue_stats(&self) -> Result<(u64, u64)> {
        let tracked: u64 = self
            .db
            .query_row("SELECT count(*) FROM session_cursors", [], |r| r.get(0))?;
        let pending_sessions: u64 = self.db.query_row(
            "SELECT count(*) FROM session_cursors WHERE last_byte_offset > last_extracted_offset",
            [],
            |r| r.get(0),
        )?;
        let pending_jobs: u64 = self.db.query_row(
            "SELECT count(*) FROM extraction_queue WHERE status IN ('pending','leased','paused_quota')",
            [],
            |r| r.get(0),
        )?;
        Ok((tracked, pending_sessions.saturating_add(pending_jobs)))
    }

    /// Runs a non-blocking `PRAGMA wal_checkpoint(PASSIVE)` so WAL frames are folded back into the
    /// main database file without blocking concurrent readers or writers.
    pub fn daemon_passive_wal_checkpoint(&self) -> Result<()> {
        let _: (i64, i64, i64) = self
            .db
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
        Ok(())
    }

    /// Reads SQLite's `PRAGMA data_version` counter, which increments whenever another connection
    /// commits changes to the database file.
    pub fn daemon_sqlite_data_version(&self) -> Result<i64> {
        Ok(self.db.query_row("PRAGMA data_version", [], |r| r.get(0))?)
    }

    /// Returns the number of undelivered entries currently queued in `outbox`.
    pub fn daemon_pending_outbox_count(&self) -> Result<u64> {
        Ok(self.db.query_row(
            "SELECT count(*) FROM outbox WHERE delivered_at IS NULL",
            [],
            |r| r.get(0),
        )?)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SessionCursorMeta {
    #[serde(default)]
    actor: Option<String>,
    #[serde(default)]
    session_ended: bool,
    status: crate::ExtractionJobStatus,
    #[serde(default)]
    attempts: u32,
    #[serde(default)]
    last_error: Option<String>,
}

fn read_session_cursor_meta_tx(
    db: &rusqlite::Connection,
    session_id: &str,
) -> Result<Option<SessionCursorMeta>> {
    let raw: Option<Option<String>> = db
        .query_row(
            "SELECT payload_json FROM change_log
             WHERE op = 'session_cursor_meta' AND target_id = ?1
             ORDER BY seq DESC LIMIT 1",
            [session_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(Some(payload)) = raw else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_str(&payload)?))
}

fn write_session_cursor_meta_tx(
    db: &rusqlite::Connection,
    session_id: &str,
    actor_id: &str,
    meta: &SessionCursorMeta,
    at: EpochMs,
) -> Result<()> {
    let payload = serde_json::to_string(meta)?;
    let existing_seq: Option<i64> = db
        .query_row(
            "SELECT seq FROM change_log
             WHERE op = 'session_cursor_meta' AND target_id = ?1
             ORDER BY seq DESC LIMIT 1",
            [session_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(seq) = existing_seq {
        db.execute(
            "UPDATE change_log SET actor_id = ?1, payload_json = ?2, created_at = ?3 WHERE seq = ?4",
            params![actor_id, payload, at.0, seq],
        )?;
    } else {
        db.execute(
            "INSERT INTO change_log (op,target_id,actor_id,payload_json,created_at)
             VALUES ('session_cursor_meta',?1,?2,?3,?4)",
            params![session_id, actor_id, payload, at.0],
        )?;
    }
    Ok(())
}

pub(crate) fn sync_session_cursor_meta_after_advance_tx(
    db: &rusqlite::Connection,
    session_id: &str,
    more: bool,
    activity_ms: i64,
) -> Result<()> {
    let prev_meta = read_session_cursor_meta_tx(db, session_id)?;
    let actor_str = match prev_meta.as_ref().and_then(|m| m.actor.clone()) {
        Some(a) => a,
        None => db.query_row("SELECT owner_actor FROM library_meta LIMIT 1", [], |r| {
            r.get(0)
        })?,
    };
    write_session_cursor_meta_tx(
        db,
        session_id,
        &actor_str,
        &SessionCursorMeta {
            actor: Some(actor_str.clone()),
            session_ended: more && prev_meta.as_ref().is_some_and(|m| m.session_ended),
            status: if more {
                crate::ExtractionJobStatus::Pending
            } else {
                crate::ExtractionJobStatus::Done
            },
            attempts: 0,
            last_error: None,
        },
        EpochMs(activity_ms),
    )
}
