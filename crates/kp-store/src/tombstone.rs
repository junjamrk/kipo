use hmac::{Hmac, Mac};
use kp_model::{ContentHash, EpochMs, KpError, LibraryId, LogicalKey, SourceId};
use rusqlite::{params, Connection, TransactionBehavior};
use sha2::Sha256;

use crate::{Result, Store, StoreError};

/// Library IDs exclude ':', making the specified origin:key encoding unambiguous.
pub fn tombstone_id(secret: &[u8], origin: &LibraryId, key: &LogicalKey) -> Result<ContentHash> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret)
        .map_err(|_| KpError::invalid("tombstone key", "invalid HMAC key"))?;
    mac.update(origin.as_str().as_bytes());
    mac.update(b":");
    mac.update(key.as_str().as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()).parse()?)
}

pub(crate) fn guard(db: &Connection, id: &ContentHash) -> Result<()> {
    let exists: bool = db
        .prepare_cached("SELECT EXISTS(SELECT 1 FROM tombstones WHERE tombstone_id=?1)")?
        .query_row([id.as_str()], |row| row.get(0))?;
    if exists {
        return Err(StoreError::Tombstoned(id.clone()));
    }
    Ok(())
}

impl Store {
    pub fn is_tombstoned(&self, origin: &LibraryId, key: &LogicalKey) -> Result<bool> {
        let id = self.tombstone_id(origin, key)?;
        Ok(self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM tombstones WHERE tombstone_id=?1)",
            [id.as_str()],
            |row| row.get(0),
        )?)
    }

    pub(crate) fn tombstone_id(&self, origin: &LibraryId, key: &LogicalKey) -> Result<ContentHash> {
        // Origin IDs make opaque tombstones portable across personal/community mirrors.
        tombstone_id(origin.as_str().as_bytes(), origin, key)
    }

    /// Idempotently blocks replay and erases local records and raw blobs.
    /// Destinations receive only the opaque tombstone, never the logical key or content.
    pub fn purge(
        &mut self,
        origin: &LibraryId,
        key: &LogicalKey,
        at: EpochMs,
        destinations: &[String],
    ) -> Result<()> {
        self.purge_with_reason(origin, key, at, destinations, "purged")
    }

    /// Resolves a source ID through its ingestion identity before applying the same tombstone guard.
    pub fn purge_target(
        &mut self,
        origin: &LibraryId,
        target: &str,
        at: EpochMs,
        reason: &str,
    ) -> Result<()> {
        use rusqlite::OptionalExtension;
        let key = self.db.query_row("SELECT logical_key FROM change_log WHERE op='ingest_source' AND target_id=?1 AND origin_library_id=?2",
            params![target,origin.as_str()], |row| row.get::<_, String>(0)).optional()?;
        if key.is_none() {
            let tombstoned: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM tombstones WHERE tombstone_id=?1 AND origin_library_id=?2)",
                params![target,origin.as_str()], |r| r.get(0))?;
            if tombstoned {
                let _ = SourceId::new(target)?;
                let tx = self
                    .db
                    .transaction_with_behavior(TransactionBehavior::Immediate)?;
                tx.execute(
                    "DELETE FROM raw_sources WHERE source_id IN (SELECT tombstone_id FROM tombstones)",
                    [],
                )?;
                tx.execute(
                    "DELETE FROM raw_blobs WHERE content_sha256 NOT IN (SELECT content_sha256 FROM raw_sources)",
                    [],
                )?;
                sweep_orphaned_community_concepts_tx(&tx)?;
                rebuild_fts_shadow_tables(&tx, &self.tokenizer)?;
                tx.commit()?;
                self.db.execute_batch(
                    "PRAGMA wal_checkpoint(TRUNCATE); VACUUM; PRAGMA wal_checkpoint(TRUNCATE);",
                )?;
                cleanup_legacy_raw_dir(&self.path, &self.db, &[target.to_owned()])?;
                return Ok(());
            }
        }
        let key = key.as_deref().unwrap_or(target).parse()?;
        self.purge_with_reason(origin, &key, at, &[], reason)
    }

    fn purge_with_reason(
        &mut self,
        origin: &LibraryId,
        key: &LogicalKey,
        at: EpochMs,
        destinations: &[String],
        reason: &str,
    ) -> Result<()> {
        let mut destinations = destinations.to_vec();
        {
            let mut stmt = self.db.prepare("SELECT DISTINCT payload_json FROM change_log WHERE op='publication' AND origin_library_id=?1 AND logical_key=?2")?;
            for row in stmt.query_map(params![origin.as_str(), key.as_str()], |r| {
                r.get::<_, String>(0)
            })? {
                destinations.push(crate::exchange::parse_publication_payload(&row?)?.0);
            }
        }
        destinations.sort();
        destinations.dedup();
        let id = self.tombstone_id(origin, key)?;
        let source_id = SourceId::new(id.as_str())?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Capture the transitive closure before deleting edges. UNION terminates even on cycles.
        tx.execute_batch(
            "CREATE TEMP TABLE purge_sources (source_id TEXT PRIMARY KEY);
            CREATE TEMP TABLE purge_candidates (fact_id TEXT PRIMARY KEY);
            CREATE TEMP TABLE purge_facts (fact_id TEXT PRIMARY KEY);
            CREATE TEMP TABLE grounded_facts (fact_id TEXT PRIMARY KEY);",
        )?;
        tx.execute(
            "INSERT INTO purge_sources SELECT source_id FROM raw_sources WHERE (source_id=?1
                AND ((SELECT mode FROM library_meta)='personal' OR NOT EXISTS (
                    SELECT 1 FROM facts WHERE origin_library_id=?2 AND logical_key=?3)))
            OR source_id IN (SELECT json_extract(source_anchor_json,'$.source_id') FROM facts
                WHERE origin_library_id=?2 AND logical_key=?3
                  AND (SELECT mode FROM library_meta)='personal')",
            params![source_id.as_str(), origin.as_str(), key.as_str()],
        )?;
        loop {
            tx.execute(
                "INSERT OR IGNORE INTO purge_candidates WITH RECURSIVE affected(fact_id) AS (
                SELECT fact_id FROM facts WHERE (origin_library_id=?1 AND logical_key=?2)
                    OR json_extract(source_anchor_json,'$.source_id') IN purge_sources
                UNION
                SELECT j.conclusion_fact_id FROM affected a
                JOIN justification_premises p ON p.premise_fact_id=a.fact_id
                JOIN justifications j ON j.just_id=p.just_id
             ) SELECT fact_id FROM affected",
                params![origin.as_str(), key.as_str()],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO purge_facts SELECT fact_id FROM facts
            WHERE (origin_library_id=?1 AND logical_key=?2)
                OR json_extract(source_anchor_json,'$.source_id') IN purge_sources",
                params![origin.as_str(), key.as_str()],
            )?;
            // Synthesis is an indivisible text claim: removing any contributing proof erases it.
            // Repeat grounding after each expansion so its downstream hard proofs cannot survive.
            loop {
                tx.execute("DELETE FROM grounded_facts", [])?;
                tx.execute(
                    "INSERT INTO grounded_facts SELECT fact_id FROM facts
                 WHERE status='active' AND expired_at IS NULL
                 AND audience_json != '\"__semiring__\"' AND fact_id NOT IN purge_facts",
                    [],
                )?;
                loop {
                    let added = tx.execute("INSERT OR IGNORE INTO grounded_facts
                    SELECT j.conclusion_fact_id FROM justifications j
                    JOIN facts conclusion ON conclusion.fact_id=j.conclusion_fact_id
                    WHERE conclusion.status='active' AND conclusion.expired_at IS NULL
                        AND j.active=1 AND j.invalidated_at IS NULL AND j.conclusion_fact_id NOT IN purge_facts
                        AND EXISTS (SELECT 1 FROM justification_premises p WHERE p.just_id=j.just_id)
                        AND NOT EXISTS (SELECT 1 FROM justification_premises p
                            JOIN facts f ON f.fact_id=p.premise_fact_id
                            WHERE p.just_id=j.just_id AND (p.premise_fact_id NOT IN grounded_facts
                                OR p.premise_sha256 != f.content_sha256
                                OR p.premise_origin_lib != f.origin_library_id
                                OR p.premise_logical_key != f.logical_key))", [])?;
                    if added == 0 {
                        break;
                    }
                }
                let added = tx.execute(
                "INSERT OR IGNORE INTO purge_facts SELECT fact_id FROM purge_candidates
                 WHERE fact_id NOT IN grounded_facts OR fact_id IN (
                    SELECT conclusion_fact_id FROM justifications WHERE derivation_mode IN ('scoped_synthesis','rsi_attribution','assumed_premise')
                    AND just_id IN (SELECT just_id FROM justification_premises WHERE premise_fact_id IN purge_facts))", [],
            )?;
                if added == 0 {
                    break;
                }
            }
            let added=tx.execute("INSERT OR IGNORE INTO purge_sources SELECT s.source_id FROM raw_sources s JOIN rsi_episodes e ON json_extract(e.environment_json,'$.source_id')=s.source_id WHERE EXISTS (SELECT 1 FROM rsi_episode_facts p WHERE p.episode_id=e.episode_id AND p.fact_id IN purge_facts)", [])?;
            if added == 0 {
                break;
            }
        }
        let mut stmt = tx.prepare("SELECT DISTINCT c.payload_json FROM change_log c JOIN facts f ON c.origin_library_id=f.origin_library_id AND c.logical_key=f.logical_key WHERE c.op='publication' AND f.fact_id IN purge_facts")?;
        for destination in stmt.query_map([], |r| r.get::<_, String>(0))? {
            destinations.push(crate::exchange::parse_publication_payload(&destination?)?.0);
        }
        drop(stmt);
        let identities = {
            let mut stmt = tx.prepare(
                "SELECT origin_library_id,logical_key FROM facts JOIN purge_facts USING(fact_id)
                 UNION SELECT origin_library_id,logical_key FROM change_log
                    WHERE op='ingest_source' AND target_id IN purge_sources
                 UNION SELECT ?1,?2",
            )?;
            let rows = stmt.query_map(params![origin.as_str(), key.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        {
            let mut outbox_id_stmt = tx.prepare(
                "SELECT DISTINCT dest_community_url FROM outbox
                 WHERE event_kind != 'purge'
                   AND ((json_extract(payload_json,'$.origin_library_id')=?1 AND json_extract(payload_json,'$.logical_key')=?2)
                     OR (json_extract(payload_json,'$.fact.origin_library')=?1 AND json_extract(payload_json,'$.fact.logical_key')=?2)
                     OR (json_extract(payload_json,'$.origin_reference.origin_library')=?1 AND json_extract(payload_json,'$.origin_reference.logical_key')=?2))",
            )?;
            for (fact_origin, fact_key) in &identities {
                for dest in outbox_id_stmt
                    .query_map(params![fact_origin, fact_key], |r| r.get::<_, String>(0))?
                {
                    destinations.push(dest?);
                }
            }
            let mut outbox_fact_stmt = tx.prepare(
                "SELECT DISTINCT dest_community_url FROM outbox
                 WHERE event_kind != 'purge'
                   AND (json_extract(payload_json,'$.fact_id') IN (SELECT fact_id FROM purge_facts)
                     OR json_extract(payload_json,'$.fact.id') IN (SELECT fact_id FROM purge_facts)
                     OR json_extract(payload_json,'$.source_id')=?1
                     OR json_extract(payload_json,'$.source_id') IN purge_sources)",
            )?;
            for dest in
                outbox_fact_stmt.query_map([source_id.as_str()], |r| r.get::<_, String>(0))?
            {
                destinations.push(dest?);
            }
        }
        destinations.sort();
        destinations.dedup();
        for (fact_origin, fact_key) in identities {
            let opaque = tombstone_id(
                fact_origin.as_bytes(),
                &fact_origin.parse()?,
                &fact_key.parse()?,
            )?;
            tx.execute(
                "INSERT INTO tombstones VALUES (?1,?2,?3,?4) ON CONFLICT(tombstone_id) DO NOTHING",
                params![opaque.as_str(), fact_origin, at.0, reason],
            )?;
            tx.execute(
                "DELETE FROM change_log WHERE origin_library_id=?1 AND logical_key=?2",
                params![fact_origin, fact_key],
            )?;
            tx.execute(
                "DELETE FROM outbox WHERE event_kind != 'purge'
                 AND ((json_extract(payload_json,'$.origin_library_id')=?1 AND json_extract(payload_json,'$.logical_key')=?2) OR (json_extract(payload_json,'$.fact.origin_library')=?1 AND json_extract(payload_json,'$.fact.logical_key')=?2) OR (json_extract(payload_json,'$.origin_reference.origin_library')=?1 AND json_extract(payload_json,'$.origin_reference.logical_key')=?2))",
                params![fact_origin, fact_key],
            )?;
            let payload = serde_json::to_string(&serde_json::json!({
                "tombstone_id":opaque, "origin_library_id":fact_origin, "purged_at":at, "reason":reason
            }))?;
            for destination in &destinations {
                let dedup =
                    ContentHash::digest(serde_json::to_vec(&(destination, opaque.as_str()))?);
                tx.execute(
                    "INSERT INTO outbox (dest_community_url,event_kind,idempotency_key,payload_json)
                     VALUES (?1,'purge',?2,?3) ON CONFLICT(idempotency_key) DO NOTHING",
                    params![destination, dedup.as_str(), payload],
                )?;
            }
        }
        tx.execute("DELETE FROM change_log WHERE target_id IN (SELECT fact_id FROM purge_facts) OR (target_id=?1 AND op != 'shared_identity') OR target_id IN purge_sources",
            [source_id.as_str()])?;
        tx.execute(
            "DELETE FROM outbox WHERE event_kind != 'purge' AND (
            json_extract(payload_json,'$.fact_id') IN (SELECT fact_id FROM purge_facts)
            OR json_extract(payload_json,'$.source_id')=?1 OR json_extract(payload_json,'$.source_id') IN purge_sources)",
            [source_id.as_str()],
        )?;
        // Remove whole proofs before their premises, so cascading FKs cannot weaken a conjunction.
        tx.execute(
            "DELETE FROM justifications WHERE just_id IN (
            SELECT just_id FROM justification_premises WHERE premise_fact_id IN purge_facts)",
            [],
        )?;
        {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT episode_id FROM rsi_episode_facts WHERE fact_id IN purge_facts",
            )?;
            let episodes = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for episode in episodes {
                let key = format!("rsi-episode:{episode}").parse()?;
                let opaque =
                    tombstone_id(self.library_id.as_str().as_bytes(), &self.library_id, &key)?;
                tx.execute(
                    "INSERT OR IGNORE INTO tombstones VALUES (?1,?2,?3,'purged')",
                    params![opaque.as_str(), self.library_id.as_str(), at.0],
                )?;
            }
        }
        tx.execute("DELETE FROM rsi_episodes WHERE episode_id IN (SELECT episode_id FROM rsi_episode_facts WHERE fact_id IN purge_facts)", [])?;
        tx.execute("DELETE FROM facts WHERE fact_id IN purge_facts", [])?;
        sweep_orphaned_community_concepts_tx(&tx)?;
        tx.execute(
            "DELETE FROM raw_sources WHERE source_id IN (SELECT tombstone_id FROM tombstones)",
            [],
        )?;
        tx.execute(
            "DELETE FROM raw_blobs WHERE content_sha256 NOT IN (SELECT content_sha256 FROM raw_sources)",
            [],
        )?;
        tx.execute(
            "DELETE FROM extraction_queue WHERE source_id IN (SELECT tombstone_id FROM tombstones)",
            [],
        )?;
        rebuild_fts_shadow_tables(&tx, &self.tokenizer)?;
        let any_active_facts: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM facts WHERE status='active' AND expired_at IS NULL)",
            [],
            |r| r.get(0),
        )?;
        if any_active_facts {
            crate::exchange::mark_alignment_dirty_tx(&tx, at)?;
        } else {
            tx.execute(
                "DELETE FROM change_log WHERE op='alignment_state' AND target_id='global'",
                [],
            )?;
        }
        let purged_source_ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT source_id FROM purge_sources")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        tx.execute_batch(
            "DROP TABLE purge_sources; DROP TABLE purge_candidates; DROP TABLE purge_facts; DROP TABLE grounded_facts;",
        )?;
        tx.commit()?;
        self.sync_skills_now()?;
        self.db.execute_batch(
            "PRAGMA wal_checkpoint(TRUNCATE); VACUUM; PRAGMA wal_checkpoint(TRUNCATE);",
        )?;
        cleanup_legacy_raw_dir(&self.path, &self.db, &purged_source_ids)?;
        Ok(())
    }
}

fn sweep_orphaned_community_concepts_tx(tx: &Connection) -> Result<()> {
    tx.execute(
        "DELETE FROM change_log
         WHERE op = 'fact_concept'
           AND target_id NOT IN (SELECT fact_id FROM facts)",
        [],
    )?;
    tx.execute(
        "DELETE FROM facts
         WHERE predicate = 'kp.core.concept_alignment'
           AND (
               (json_type(object_json, '$.left_fact_id') = 'text'
                AND json_extract(object_json, '$.left_fact_id') NOT IN (SELECT fact_id FROM facts))
               OR (json_type(object_json, '$.right_fact_id') = 'text'
                   AND json_extract(object_json, '$.right_fact_id') NOT IN (SELECT fact_id FROM facts))
           )",
        [],
    )?;
    let referenced = crate::exchange::collect_all_referenced_concept_ids_tx(tx)?;
    let all_stored: Vec<String> = {
        let mut stmt = tx.prepare("SELECT concept_id FROM community_concepts")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    for raw_cid in all_stored {
        let keep = raw_cid
            .parse::<kp_model::ConceptId>()
            .is_ok_and(|cid| referenced.contains(&cid));
        if !keep {
            tx.execute(
                "DELETE FROM community_concepts WHERE concept_id = ?1",
                [&raw_cid],
            )?;
        }
    }
    Ok(())
}

fn cleanup_legacy_raw_dir(
    library_path: &std::path::Path,
    db: &Connection,
    purged_source_ids: &[String],
) -> Result<()> {
    let raw_dir = library_path.join("raw");
    if !raw_dir.exists() {
        return Ok(());
    }
    for sid in purged_source_ids {
        match std::fs::remove_file(raw_dir.join(format!("{sid}.md"))) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(StoreError::Io(err)),
        }
    }
    let any_unmigrated: bool = db.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM raw_sources s
            WHERE NOT EXISTS (SELECT 1 FROM raw_blobs b WHERE b.content_sha256 = s.content_sha256)
              AND NOT EXISTS (SELECT 1 FROM tombstones t WHERE t.tombstone_id = s.source_id)
        )",
        [],
        |r| r.get(0),
    )?;
    if !any_unmigrated {
        match std::fs::remove_dir_all(&raw_dir) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(StoreError::Io(err)),
        }
    }
    Ok(())
}

/// Rebuilds `chunks_fts` and `facts_fts` shadow b-trees (`_data`, `_idx`, `_docsize`) so deleted
/// terms cannot linger inside FTS5 internal split keys or unmerged segments even before `VACUUM`.
fn rebuild_fts_shadow_tables(db: &Connection, tokenizer: &crate::Tokenizer) -> Result<()> {
    if db
        .execute("INSERT INTO chunks_fts(chunks_fts) VALUES('rebuild')", [])
        .is_err()
    {
        db.execute(
            "INSERT INTO chunks_fts(chunks_fts) VALUES('delete-all')",
            [],
        )?;
        let surviving_chunks = {
            let mut stmt = db.prepare(
                "SELECT c.fts_rowid, s.uri, c.heading, c.segmented_body
                 FROM chunks c
                 JOIN raw_sources s ON s.source_id = c.source_id
                 ORDER BY c.fts_rowid",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        for (rowid, uri, heading, segmented_body) in surviving_chunks {
            db.execute(
                "INSERT INTO chunks_fts (rowid, title, heading, segmented_body) VALUES (?1, ?2, ?3, ?4)",
                params![
                    rowid,
                    tokenizer.segment_text(&uri),
                    tokenizer.segment_text(&heading),
                    segmented_body,
                ],
            )?;
        }
        db.execute("INSERT INTO chunks_fts(chunks_fts) VALUES('optimize')", [])?;
    }

    if db
        .execute("INSERT INTO facts_fts(facts_fts) VALUES('rebuild')", [])
        .is_err()
    {
        db.execute("INSERT INTO facts_fts(facts_fts) VALUES('delete-all')", [])?;
        let surviving_facts = {
            let mut stmt = db.prepare(
                "SELECT fts_rowid, subject, predicate, object_json, tags_json, source_anchor_json
                 FROM facts
                 ORDER BY fts_rowid",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        for (rowid, subject, predicate, object_json, tags_json, anchor_json) in surviving_facts {
            let (seg_sub, seg_pred, seg_obj) = crate::facts::fact_fts_columns(
                tokenizer,
                &subject,
                &predicate,
                &object_json,
                &tags_json,
                anchor_json.as_deref(),
            );
            db.execute(
                "INSERT INTO facts_fts (rowid, subject, predicate, segmented_object) VALUES (?1, ?2, ?3, ?4)",
                params![rowid, seg_sub, seg_pred, seg_obj],
            )?;
        }
        db.execute("INSERT INTO facts_fts(facts_fts) VALUES('optimize')", [])?;
    }
    Ok(())
}

/// Scans every cell of every SQLite table in `library_dir/kp.sqlite` (read-only) and returns
/// `Ok(Some(table_name))` if `needle` appears anywhere in any text or blob column.
pub fn find_sqlite_table_containing_bytes(
    library_dir: &std::path::Path,
    needle: &[u8],
) -> Result<Option<String>> {
    if needle.is_empty() {
        return Ok(None);
    }
    let db = rusqlite::Connection::open_with_flags(
        library_dir.join("kp.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let tables = db
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for table in tables {
        let mut statement =
            db.prepare(&format!("SELECT * FROM \"{}\"", table.replace('"', "\"\"")))?;
        let columns = statement.column_count();
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            for column in 0..columns {
                match row.get_ref(column)? {
                    rusqlite::types::ValueRef::Text(bytes)
                    | rusqlite::types::ValueRef::Blob(bytes) => {
                        if bytes.windows(needle.len()).any(|w| w == needle) {
                            return Ok(Some(table));
                        }
                    }
                    rusqlite::types::ValueRef::Null
                    | rusqlite::types::ValueRef::Integer(_)
                    | rusqlite::types::ValueRef::Real(_) => {}
                }
            }
        }
    }
    Ok(None)
}

/// Read-only audit helper returning `COUNT(*)` of `raw_sources WHERE self_echo_filtered=1`.
pub fn count_self_echo_filtered_sources(library_dir: &std::path::Path) -> Result<i64> {
    let db = rusqlite::Connection::open_with_flags(
        library_dir.join("kp.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    Ok(db.query_row(
        "SELECT COUNT(*) FROM raw_sources WHERE self_echo_filtered=1",
        [],
        |r| r.get(0),
    )?)
}

/// Read-only audit helper fetching `outbox.payload_json` by `idempotency_key`.
pub fn outbox_payload_by_idempotency_key(
    library_dir: &std::path::Path,
    key: &str,
) -> Result<Option<String>> {
    use rusqlite::OptionalExtension;
    let db = rusqlite::Connection::open_with_flags(
        library_dir.join("kp.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    Ok(db
        .query_row(
            "SELECT payload_json FROM outbox WHERE idempotency_key=?1",
            [key],
            |r| r.get(0),
        )
        .optional()?)
}

/// Read-only audit helper counting exact phrase matches in `chunks_fts`.
pub fn count_chunks_fts_matches(library_dir: &std::path::Path, phrase: &str) -> Result<i64> {
    let db = rusqlite::Connection::open_with_flags(
        library_dir.join("kp.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let escaped = format!("\"{}\"", phrase.replace('"', "\"\""));
    Ok(db.query_row(
        "SELECT COUNT(*) FROM chunks_fts WHERE chunks_fts MATCH ?1",
        [escaped],
        |r| r.get(0),
    )?)
}

/// Read-only audit helper counting orphaned rows in `chunk_vectors`.
pub fn count_orphaned_chunk_vectors(library_dir: &std::path::Path) -> Result<i64> {
    let db = rusqlite::Connection::open_with_flags(
        library_dir.join("kp.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    Ok(db.query_row(
        "SELECT COUNT(*) FROM chunk_vectors WHERE chunk_id NOT IN (SELECT chunk_id FROM chunks)",
        [],
        |r| r.get(0),
    )?)
}

#[cfg(test)]
#[path = "tombstone_tests.rs"]
mod tests;
