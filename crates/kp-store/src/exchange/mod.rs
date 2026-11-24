mod publish;
mod publish_helpers;
mod session_cursors;
mod snapshot_sync;

pub use publish::{AcceptInitialPublishSpec, FinalizePublishSpec, PublishEnqueueSpec};
pub(crate) use publish_helpers::{
    cancel_publishes_and_queue_withdrawals, cancel_unrevised_self_only_pending_publishes_tx,
    parse_publication_payload, reconcile_pending_publishes_for_scope_tx,
    record_withdrawn_version_tx,
};
pub(crate) use session_cursors::sync_session_cursor_meta_after_advance_tx;
pub use session_cursors::{SessionCursorUpdate, UnextractedSessionCursor};
pub(crate) use snapshot_sync::{
    collect_all_referenced_concept_ids_tx, link_fact_concept_tx, mark_alignment_dirty_tx,
    upsert_concept_tx,
};

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

use kp_model::{
    ActorId, ContentHash, EpochMs, Fact, FactId, FactStatus, Justification, LibraryId, MemberRole,
};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{Result, Store};

/// Computes the lowercase hex SHA-256 digest of bytes `0..len` of `path` in 64 KiB streaming chunks.
/// Returns `Ok(None)` if `path` does not exist or its byte length is shorter than `len`.
pub fn sha256_file_prefix(path: &Path, len: u64) -> Result<Option<String>> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    if file.metadata()?.len() < len {
        return Ok(None);
    }
    let mut hasher = Sha256::new();
    let mut remaining = len;
    let mut buf = [0u8; 65_536];
    while remaining > 0 {
        let to_read = usize::try_from(remaining.min(buf.len() as u64)).unwrap_or(buf.len());
        let n = file.read(&mut buf[..to_read])?;
        if n == 0 {
            return Ok(None);
        }
        hasher.update(&buf[..n]);
        remaining = remaining.saturating_sub(n as u64);
    }
    Ok(Some(hex::encode(hasher.finalize())))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tombstone {
    pub tombstone_id: ContentHash,
    pub origin_library_id: LibraryId,
    pub purged_at: EpochMs,
    pub reason: String,
}

/// A complete viewer projection. Its cursor hashes visible state, never global CDC ordinals.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangeSnapshot {
    pub library_id: LibraryId,
    pub cursor: ContentHash,
    #[serde(default)]
    pub unchanged: bool,
    pub facts: Vec<Fact>,
    pub justifications: Vec<Justification>,
    pub tombstones: Vec<Tombstone>,
    #[serde(default)]
    pub origin_references: BTreeMap<FactId, kp_model::StrongRef>,
}

#[derive(Clone, Debug)]
pub enum InitialPublishWrite {
    Base {
        fact: Box<Fact>,
        entities: BTreeSet<String>,
    },
    Skill {
        anchor: Box<Fact>,
        skill_fact: Box<Fact>,
        proof: Box<Justification>,
    },
}

#[derive(Debug)]
pub struct OutboxEntry {
    pub id: i64,
    pub destination: String,
    pub kind: String,
    pub key: String,
    pub payload: Value,
    pub retry_count: u32,
}

impl Store {
    pub fn member_role(&self, actor: &ActorId) -> Result<Option<MemberRole>> {
        let role: Option<String> = self
            .db
            .query_row(
                "SELECT role FROM members_tokens WHERE actor_id=?1 AND revoked_at IS NULL",
                [actor.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        role.map(|s| Ok(s.parse()?)).transpose()
    }

    pub fn sync_cursor(
        &self,
        endpoint: &str,
        credential: &ContentHash,
    ) -> Result<Option<ContentHash>> {
        let cursor: Option<String> = self.db.query_row("SELECT content_sha256 FROM change_log WHERE op='sync_cursor' AND target_id=?1 AND actor_id=?2 ORDER BY seq DESC LIMIT 1", params![endpoint,credential.as_str()], |r| r.get(0)).optional()?;
        cursor.map(|s| Ok(s.parse()?)).transpose()
    }

    pub fn authenticate(&self, token: &str) -> Result<Option<(ActorId, MemberRole)>> {
        let hash = ContentHash::digest(token);
        let row: Option<(String, String)> = self.db.query_row("SELECT actor_id,role FROM members_tokens WHERE token_sha256=?1 AND revoked_at IS NULL", [hash.as_str()], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        row.map(|(actor, role)| Ok((actor.parse()?, role.parse()?)))
            .transpose()
    }

    pub fn revoke_member(&mut self, actor: &ActorId, at: EpochMs) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE members_tokens SET revoked_at=?1 WHERE actor_id=?2",
            params![at.0, actor.as_str()],
        )?;
        mark_alignment_dirty_tx(&tx, at)?;
        tx.commit()?;
        Ok(())
    }

    pub fn resolve_fact(&self, target: &str) -> Result<Option<Fact>> {
        self.resolve_fact_target(target, None, None)
    }

    /// Exact IDs are unambiguous; logical keys are resolved only within the supplied scope.
    pub fn resolve_fact_target(
        &self,
        target: &str,
        author: Option<&ActorId>,
        origin: Option<&LibraryId>,
    ) -> Result<Option<Fact>> {
        let graph = self.derivation_view()?;
        Ok(graph
            .facts
            .values()
            .find(|f| f.id.as_str() == target && origin.is_none_or(|o| o == &f.origin_library))
            .or_else(|| {
                graph.facts.values().find(|f| {
                    f.logical_key.as_str() == target
                        && f.status == FactStatus::Active
                        && author.is_none_or(|a| a == &f.author)
                        && origin.is_none_or(|o| o == &f.origin_library)
                })
            })
            .cloned())
    }

    pub fn enqueue(
        &mut self,
        destination: &str,
        kind: &str,
        key: &str,
        payload: &Value,
    ) -> Result<()> {
        self.db.execute("INSERT INTO outbox (dest_community_url,event_kind,idempotency_key,payload_json) VALUES (?1,?2,?3,?4) ON CONFLICT(idempotency_key) DO NOTHING", params![destination,kind,key,serde_json::to_string(payload)?])?;
        Ok(())
    }

    pub fn pending_outbox(&self, destination: &str) -> Result<Vec<OutboxEntry>> {
        let mut stmt = self.db.prepare("SELECT outbox_id,dest_community_url,event_kind,idempotency_key,payload_json,retry_count FROM outbox WHERE delivered_at IS NULL AND dest_community_url=?1 ORDER BY outbox_id")?;
        let mut rows = stmt.query([destination])?;
        let mut entries = Vec::new();
        while let Some(row) = rows.next()? {
            entries.push(OutboxEntry {
                id: row.get(0)?,
                destination: row.get(1)?,
                kind: row.get(2)?,
                key: row.get(3)?,
                payload: serde_json::from_str(&row.get::<_, String>(4)?)?,
                retry_count: row.get(5)?,
            });
        }
        Ok(entries)
    }

    pub fn outbox_result(&mut self, id: i64, delivered: Option<EpochMs>) -> Result<()> {
        self.db.execute("UPDATE outbox SET delivered_at=?1,retry_count=retry_count+CASE WHEN ?1 IS NULL THEN 1 ELSE 0 END WHERE outbox_id=?2", params![delivered.map(|at| at.0),id])?;
        Ok(())
    }

    /// Records a failed `POST /v1/publish` attempt on `id`:
    /// - When `definitely_never_sent` is `true` (e.g. TCP `ECONNREFUSED` before any bytes left the host)
    ///   AND no prior ambiguous attempt latched `"inflight_uncertain"`, clears `"inflight_dispatched"`
    ///   so pre-receipt `SelfOnly` shrinks cancel locally without leaking a `SelfOnly` claim to the server.
    /// - Otherwise latches `"inflight_uncertain": true` permanently so any concurrent/subsequent shrink
    ///   transmits a higher-revision `"shrunk_from_inflight"` update upon recovery.
    pub fn outbox_publish_attempt_failed(
        &mut self,
        id: i64,
        definitely_never_sent: bool,
    ) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let raw_opt: Option<String> = tx
            .query_row(
                "SELECT payload_json FROM outbox WHERE outbox_id=?1 AND delivered_at IS NULL",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(raw) = raw_opt {
            if let Ok(mut val) = serde_json::from_str::<Value>(&raw) {
                if let Some(obj) = val.as_object_mut() {
                    if definitely_never_sent {
                        if !obj
                            .get("inflight_uncertain")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                        {
                            obj.remove("inflight_dispatched");
                        }
                    } else {
                        obj.insert("inflight_uncertain".into(), Value::Bool(true));
                    }
                    tx.execute(
                        "UPDATE outbox SET payload_json=?1, retry_count=retry_count+1
                         WHERE outbox_id=?2 AND delivered_at IS NULL",
                        params![serde_json::to_string(&val)?, id],
                    )?;
                    tx.commit()?;
                    return Ok(());
                }
            }
        }
        tx.execute(
            "UPDATE outbox SET retry_count=retry_count+1 WHERE outbox_id=?1 AND delivered_at IS NULL",
            [id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn exchange_drop_outbox(&mut self, id: i64) -> Result<()> {
        self.db
            .execute("DELETE FROM outbox WHERE outbox_id=?1", [id])?;
        Ok(())
    }

    pub fn exchange_library_path(&self) -> &std::path::Path {
        &self.path
    }

    /// Bind first registration durably, including after all claims from this origin are purged.
    pub fn bind_origin_owner(&mut self, origin: &LibraryId, actor: &ActorId) -> Result<bool> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let owners = origin_owners(&tx, origin)?;
        if owners.iter().any(|owner| owner != actor) {
            return Ok(false);
        }
        // Legacy opaque tombstones without an owner cannot establish a new member's ownership.
        if owners.is_empty()
            && tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM tombstones WHERE origin_library_id=?1)",
                [origin.as_str()],
                |r| r.get::<_, bool>(0),
            )?
        {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO change_log (op,origin_library_id,actor_id,created_at)
            SELECT 'origin_owner',?1,?2,0 WHERE NOT EXISTS
            (SELECT 1 FROM change_log WHERE op='origin_owner' AND origin_library_id=?1)",
            params![origin.as_str(), actor.as_str()],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn owns_origin(&self, origin: &LibraryId, actor: &ActorId) -> Result<bool> {
        Ok(origin_owners(&self.db, origin)? == BTreeSet::from([actor.clone()]))
    }

    pub fn origin_has_other_owner(&self, origin: &LibraryId, actor: &ActorId) -> Result<bool> {
        Ok(origin_owners(&self.db, origin)?
            .iter()
            .any(|owner| owner != actor))
    }

    /// Counts viewer-readable chunks via SQL without materializing `RetrievalSnapshot` or vectors,
    /// enforcing the exact same `raw_sources`, `tombstones`, `readable_by_json`, and `source_scopes` gates.
    pub fn exchange_visible_chunk_count(&self, viewer: &ActorId) -> Result<usize> {
        let count: i64 = self.db.query_row(
            "SELECT COUNT(*)
             FROM chunks c
             JOIN raw_sources s ON s.source_id = c.source_id AND s.content_sha256 = c.source_sha256
             WHERE s.superseded_by_source IS NULL
               AND NOT EXISTS (SELECT 1 FROM tombstones t WHERE t.tombstone_id = c.source_id)
               AND EXISTS (
                   SELECT 1 FROM json_each(c.readable_by_json) a
                   WHERE a.value = ?1
                      OR (a.value = '*' AND EXISTS (
                          SELECT 1 FROM members_tokens m WHERE m.actor_id = ?1 AND m.revoked_at IS NULL
                      ))
               )
               AND (s.source_scope_id IS NULL OR EXISTS (
                   SELECT 1 FROM source_scopes sc, json_each(sc.current_audience_json) sa
                   WHERE sc.scope_id = s.source_scope_id
                     AND (sa.value = ?1
                          OR (sa.value = '*' AND EXISTS (
                              SELECT 1 FROM members_tokens m WHERE m.actor_id = ?1 AND m.revoked_at IS NULL
                          )))
               ))",
            params![viewer.as_str()],
            |r| r.get(0),
        )?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    pub fn apply_tombstone(&mut self, tombstone: &Tombstone) -> Result<()> {
        let graph = self.derivation_view()?;
        for fact in graph.facts.values() {
            if fact.origin_library == tombstone.origin_library_id
                && self.tombstone_id(&fact.origin_library, &fact.logical_key)?
                    == tombstone.tombstone_id
            {
                self.purge(
                    &fact.origin_library,
                    &fact.logical_key,
                    tombstone.purged_at,
                    &[],
                )?;
            }
        }
        self.db.execute(
            "INSERT INTO tombstones VALUES (?1,?2,?3,?4) ON CONFLICT(tombstone_id) DO NOTHING",
            params![
                tombstone.tombstone_id.as_str(),
                tombstone.origin_library_id.as_str(),
                tombstone.purged_at.0,
                tombstone.reason
            ],
        )?;
        self.sync_skills_now()
    }
}

impl Store {
    pub fn tombstone_ids(&self) -> Result<BTreeSet<ContentHash>> {
        let mut stmt = self.db.prepare("SELECT tombstone_id FROM tombstones")?;
        let result = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .map(|r| Ok(r?.parse()?))
            .collect();
        result
    }
}

impl Store {
    pub fn origin_references(&self) -> Result<BTreeMap<FactId, kp_model::StrongRef>> {
        read_origin_references(&self.db)
    }
    pub fn record_origin_reference(
        &mut self,
        id: &FactId,
        reference: &kp_model::StrongRef,
    ) -> Result<()> {
        write_origin_reference(&self.db, id, reference)
    }
}
fn read_origin_references(
    db: &rusqlite::Connection,
) -> Result<BTreeMap<FactId, kp_model::StrongRef>> {
    let mut stmt = db.prepare(
        "SELECT target_id,payload_json FROM change_log WHERE op='origin_reference' ORDER BY seq",
    )?;
    let mut rows = stmt.query([])?;
    let mut refs = BTreeMap::new();
    while let Some(row) = rows.next()? {
        refs.insert(
            row.get::<_, String>(0)?.parse()?,
            serde_json::from_str(&row.get::<_, String>(1)?)?,
        );
    }
    Ok(refs)
}
fn write_origin_reference(
    db: &rusqlite::Connection,
    id: &FactId,
    reference: &kp_model::StrongRef,
) -> Result<()> {
    db.execute("INSERT INTO change_log (op,target_id,payload_json,created_at) SELECT 'origin_reference',?1,?2,0 WHERE NOT EXISTS (SELECT 1 FROM change_log WHERE op='origin_reference' AND target_id=?1)",params![id.as_str(),serde_json::to_string(reference)?])?;
    Ok(())
}

fn origin_owners(db: &rusqlite::Connection, origin: &LibraryId) -> Result<BTreeSet<ActorId>> {
    let mut stmt = db.prepare(
        "SELECT actor_id FROM change_log WHERE op='origin_owner' AND origin_library_id=?1
        UNION SELECT COALESCE(ingested_by_actor, author_actor) FROM facts WHERE origin_library_id=?1",
    )?;
    let owners = stmt
        .query_map([origin.as_str()], |r| r.get::<_, String>(0))?
        .map(|r| Ok(r?.parse()?))
        .collect();
    owners
}

#[cfg(test)]
#[path = "../exchange_tests.rs"]
mod tests;
