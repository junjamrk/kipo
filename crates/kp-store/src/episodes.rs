use kp_model::{
    ActorId, AnchorSpec, AudienceSpec, ContentHash, ContrastKind, EpochMs, KpError, SecretRedactor,
    SourceId, SourceModality, SourceScopeId,
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::facts::read_graph;
use crate::{Result, Store, StoreError};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnchoredSourceChunk {
    pub chunk_id: String,
    pub heading: String,
    pub body: String,
    pub anchor: AnchorSpec,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Episode {
    pub signal_kind: ContrastKind,
    pub session_id: String,
    pub before_trace: Value,
    pub after_trace: Value,
    pub step_ref: String,
    pub environment: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingEpisode {
    pub id: ContentHash,
    pub episode: Episode,
}

impl Episode {
    pub fn redacted(&self) -> Result<Self> {
        let redactor =
            SecretRedactor::new().map_err(|_| KpError::invalid("redactor", "invalid pattern"))?;
        Ok(Self {
            signal_kind: self.signal_kind,
            session_id: redactor.redact(&self.session_id),
            before_trace: redactor.redact_json(&self.before_trace),
            after_trace: redactor.redact_json(&self.after_trace),
            step_ref: redactor.redact(&self.step_ref),
            environment: redactor.redact_json(&self.environment),
        })
    }

    pub fn has_contrast(&self) -> bool {
        fn nonempty(value: &Value) -> bool {
            match value {
                Value::String(text) => !text.trim().is_empty(),
                Value::Object(fields) => !fields.is_empty(),
                Value::Array(values) => !values.is_empty(),
                Value::Null | Value::Bool(_) | Value::Number(_) => false,
            }
        }
        nonempty(&self.before_trace)
            && nonempty(&self.after_trace)
            && self.before_trace != self.after_trace
            && self.after_trace.get("success").and_then(Value::as_bool) != Some(false)
    }
}

impl Store {
    pub fn pending_episodes(&self, actor: &ActorId) -> Result<Vec<PendingEpisode>> {
        let owner = self.status()?.owner_actor;
        let mut stmt = self.db.prepare("SELECT episode_id,signal_kind,session_id,before_trace_json,after_trace_json,step_ref,environment_json FROM rsi_episodes WHERE processed=0 AND COALESCE(json_extract(environment_json,'$.actor'),?2)=?1 ORDER BY created_at,episode_id")?;
        let mut rows = stmt.query(params![actor.as_str(), owner.as_str()])?;
        let mut pending = Vec::new();
        while let Some(row) = rows.next()? {
            pending.push(PendingEpisode {
                id: row.get::<_, String>(0)?.parse()?,
                episode: Episode {
                    signal_kind: row.get::<_, String>(1)?.parse()?,
                    session_id: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    before_trace: serde_json::from_str(
                        &row.get::<_, Option<String>>(3)?
                            .unwrap_or_else(|| "null".into()),
                    )?,
                    after_trace: serde_json::from_str(
                        &row.get::<_, Option<String>>(4)?
                            .unwrap_or_else(|| "null".into()),
                    )?,
                    step_ref: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                    environment: serde_json::from_str(
                        &row.get::<_, Option<String>>(6)?
                            .unwrap_or_else(|| "{}".into()),
                    )?,
                }
                .redacted()?,
            });
        }
        Ok(pending)
    }

    /// Content identity makes repeated Stop notifications harmless.
    pub fn record_episodes(&mut self, episodes: &[Episode], at: EpochMs) -> Result<usize> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut inserted = 0;
        for episode in episodes {
            inserted += write_episode(&tx, episode, at)?;
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// Failure consumption and recovery attribution share one transaction across hook processes.
    pub fn record_tool_outcome(
        &mut self,
        actor: &ActorId,
        session: &str,
        target: &str,
        failed: bool,
        trace: &Value,
        at: EpochMs,
    ) -> Result<usize> {
        let redactor =
            SecretRedactor::new().map_err(|_| KpError::invalid("redactor", "invalid pattern"))?;
        let trace = redactor.redact_json(trace);
        let session = redactor.redact(session);
        let target = redactor.redact(target);
        let key = ContentHash::digest(serde_json::to_vec(&(actor, &session, &target))?);
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let before: Option<String> = tx.query_row("SELECT payload_json FROM change_log WHERE op='hook_failure' AND target_id=?1 ORDER BY seq DESC LIMIT 1", [key.as_str()], |r| r.get(0)).optional()?;
        tx.execute(
            "DELETE FROM change_log WHERE op='hook_failure' AND target_id=?1",
            [key.as_str()],
        )?;
        let mut inserted = 0;
        if failed {
            tx.execute("INSERT INTO change_log (op,target_id,actor_id,payload_json,created_at) VALUES ('hook_failure',?1,?2,?3,?4)", params![key.as_str(),actor.as_str(),serde_json::to_string(&trace)?,at.0])?;
        } else if let Some(before) = before {
            let before: Value = serde_json::from_str(&before)?;
            let step_ref = before
                .get("step_ref")
                .and_then(Value::as_str)
                .or_else(|| trace.get("step_ref").and_then(Value::as_str))
                .map(str::to_owned)
                .unwrap_or_else(|| format!("tool:{key}"));
            inserted = write_episode(
                &tx,
                &Episode {
                    signal_kind: ContrastKind::FailureToRecovery,
                    session_id: session,
                    before_trace: before,
                    after_trace: trace.clone(),
                    step_ref,
                    environment: serde_json::json!({"actor":actor,"target":target,"verified":true}),
                },
                at,
            )?;
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// Lists non-tombstoned session/transcript `SourceId`s currently readable by `actor` that have
    /// not yet been mined by `kp dream` for `actor`.
    pub fn unmined_session_source_ids(&self, actor: &ActorId) -> Result<Vec<SourceId>> {
        if self.member_role(actor)?.is_none() {
            return Err(StoreError::PermissionDenied);
        }
        let graph = read_graph(&self.db)?;
        let mut stmt = self.db.prepare(
            "SELECT s.source_id, s.uri, s.media_type, s.visibility_scope, s.source_scope_id
             FROM raw_sources s
             WHERE s.superseded_by_source IS NULL
               AND s.uri NOT LIKE 'dream-session:%'
               AND NOT EXISTS (SELECT 1 FROM tombstones t WHERE t.tombstone_id = s.source_id)
               AND NOT EXISTS (
                   SELECT 1 FROM change_log c
                   WHERE c.op = 'dream_mined_source' AND c.target_id = s.source_id AND c.actor_id = ?1
               )
             ORDER BY s.captured_at ASC, s.source_id ASC",
        )?;
        let mut rows = stmt.query([actor.as_str()])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let sid_str: String = row.get(0)?;
            let uri: String = row.get(1)?;
            let media_type: String = row.get(2)?;
            if SourceModality::infer(&uri, &media_type) != SourceModality::AgentSession {
                continue;
            }
            let vis_raw: String = row.get(3)?;
            let scope_opt: Option<String> = row.get(4)?;
            let Ok(vis) = serde_json::from_str::<AudienceSpec>(&vis_raw) else {
                continue;
            };
            if !vis.allows(actor, &graph.members) {
                continue;
            }
            if let Some(scope_str) = scope_opt.filter(|s| !s.trim().is_empty()) {
                let Ok(scope_id) = scope_str.parse::<SourceScopeId>() else {
                    continue;
                };
                let Some(scope_aud) = graph.source_scopes.get(&scope_id) else {
                    continue;
                };
                if !scope_aud.allows(actor, &graph.members) {
                    continue;
                }
            }
            if let Ok(source_id) = sid_str.parse::<SourceId>() {
                out.push(source_id);
            }
        }
        Ok(out)
    }

    /// Loads ordered chunks of `source_id` together with their full persisted `AnchorSpec`
    /// (`page`, `slide`, `sheet`, `file_path`, `symbol`, etc.).
    pub fn source_anchored_chunks(&self, source_id: &SourceId) -> Result<Vec<AnchoredSourceChunk>> {
        let mut stmt = self.db.prepare(
            "SELECT chunk_id, heading, body, anchor_json
             FROM chunks
             WHERE source_id = ?1
             ORDER BY ordinal ASC",
        )?;
        let rows = stmt.query_map([source_id.as_str()], |r| {
            let cid: String = r.get(0)?;
            let heading: String = r.get(1)?;
            let body: String = r.get(2)?;
            let anchor_raw: String = r.get(3)?;
            Ok((cid, heading, body, anchor_raw))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (chunk_id, heading, body, anchor_raw) = row?;
            let anchor: AnchorSpec = serde_json::from_str(&anchor_raw).unwrap_or_default();
            out.push(AnchoredSourceChunk {
                chunk_id,
                heading,
                body,
                anchor,
            });
        }
        Ok(out)
    }

    /// Counts unfinished non-`raw_only` jobs in `extraction_queue`.
    pub fn pending_extraction_job_count(&self) -> Result<usize> {
        let count: i64 = self.db.query_row(
            "SELECT count(*) FROM extraction_queue
             WHERE status IN ('pending', 'leased', 'paused_quota')
               AND NOT EXISTS (
                   SELECT 1 FROM change_log cl
                   WHERE cl.op = 'raw_only_source' AND cl.target_id = extraction_queue.source_id
               )",
            [],
            |r| r.get(0),
        )?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// Fast count of pending `extraction_queue` jobs, unprocessed `rsi_episodes`, and unmined
    /// `AgentSession` sources visible to `actor`.
    pub fn pending_dream_work_count(&self, actor: &ActorId) -> Result<usize> {
        if self.member_role(actor)?.is_none() {
            return Err(StoreError::PermissionDenied);
        }
        let jobs = self.pending_extraction_job_count()?;
        let ep = self.pending_episodes(actor)?.len();
        let sessions = self.unmined_session_source_ids(actor)?.len();
        Ok(jobs.saturating_add(ep).saturating_add(sessions))
    }

    /// Marks `source_id` as mined for `actor` in `change_log(op = 'dream_mined_source')`.
    pub fn mark_session_source_dream_mined(
        &mut self,
        source_id: &SourceId,
        actor: &ActorId,
        at: EpochMs,
    ) -> Result<()> {
        if self.member_role(actor)?.is_none() {
            return Err(StoreError::PermissionDenied);
        }
        self.db.execute(
            "INSERT INTO change_log (op, target_id, actor_id, created_at)
             SELECT 'dream_mined_source', ?1, ?2, ?3
             WHERE NOT EXISTS (
                 SELECT 1 FROM change_log
                 WHERE op = 'dream_mined_source' AND target_id = ?1 AND actor_id = ?2
             )",
            params![source_id.as_str(), actor.as_str(), at.0],
        )?;
        Ok(())
    }
}

fn write_episode(db: &Connection, episode: &Episode, at: EpochMs) -> Result<usize> {
    let episode = episode.redacted()?;
    if !episode.has_contrast() {
        return Ok(0);
    }
    let key = ContentHash::digest(serde_json::to_vec(&episode)?);
    let origin: String =
        db.query_row("SELECT library_id FROM library_meta", [], |row| row.get(0))?;
    let opaque = crate::tombstone_id(
        origin.as_bytes(),
        &origin.parse()?,
        &format!("rsi-episode:{key}").parse()?,
    )?;
    crate::tombstone::guard(db, &opaque)?;
    let inserted = db.execute("INSERT INTO rsi_episodes (episode_id,signal_kind,session_id,before_trace_json,after_trace_json,step_ref,environment_json,processed,created_at)
        VALUES (?1,?2,?3,?4,?5,?6,?7,0,?8) ON CONFLICT(episode_id) DO NOTHING", params![
        key.as_str(),episode.signal_kind.as_str(),episode.session_id,serde_json::to_string(&episode.before_trace)?,
        serde_json::to_string(&episode.after_trace)?,episode.step_ref,serde_json::to_string(&episode.environment)?,at.0])?;
    let premises: Vec<kp_model::FactId> = episode
        .environment
        .get("premise_ids")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    for id in premises {
        db.execute(
            "INSERT OR IGNORE INTO rsi_episode_facts VALUES (?1,?2)",
            params![key.as_str(), id.as_str()],
        )?;
    }
    Ok(inserted)
}
