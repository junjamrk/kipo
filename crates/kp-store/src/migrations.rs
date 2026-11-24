use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use kp_model::{canonical_json, core_lexicon, ContentHash};
use rusqlite::{params, Connection, Transaction, TransactionBehavior};

use crate::{Result, StoreError};

pub(crate) const VERSION: i64 = 2;
pub(crate) const DDL: &str = include_str!("schema.sql");

static SCHEMA_IR_HASH: OnceLock<ContentHash> = OnceLock::new();

pub fn schema_ir_hash() -> Result<ContentHash> {
    if let Some(cached) = SCHEMA_IR_HASH.get() {
        return Ok(cached.clone());
    }
    let lexicon = canonical_json(&serde_json::to_value(core_lexicon())?);
    let computed = ContentHash::digest(format!("{DDL}\n{lexicon}"));
    let _ = SCHEMA_IR_HASH.set(computed.clone());
    Ok(computed)
}

pub(crate) fn initialize(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(DDL)?;
    for slot in core_lexicon() {
        tx.execute(
            "INSERT INTO schema_slots VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                slot.predicate,
                slot.namespace,
                slot.cardinality.as_str(),
                slot.mutability.as_str(),
                slot.value_kind,
                slot.description,
                slot.instruction,
            ],
        )?;
    }
    tx.pragma_update(None, "user_version", VERSION)?;
    Ok(())
}

pub(crate) fn has_column(db: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = db.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let col_name: String = row.get(1)?;
        if col_name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn isolated_git_cat(raw_dir: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let git_bin = [
        "/usr/bin/git",
        "/opt/homebrew/bin/git",
        "/usr/local/bin/git",
        "git",
    ]
    .into_iter()
    .find(|candidate| *candidate == "git" || Path::new(candidate).is_file())?;

    let out = Command::new(git_bin)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .arg("-c")
        .arg("protocol.allow=never")
        .arg("-C")
        .arg(raw_dir)
        .args(args)
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

/// Reads legacy M1 raw source bytes from `<library_path>/raw/<source_id>.md` or `<library_path>/raw/.git`.
pub(crate) fn read_legacy_raw_source_bytes(
    library_path: &Path,
    source_id: &str,
    git_blob_or_commit: &str,
) -> Option<Vec<u8>> {
    let raw_dir = library_path.join("raw");
    let wt_file = raw_dir.join(format!("{source_id}.md"));
    if let Ok(bytes) = fs::read(&wt_file) {
        return Some(bytes);
    }
    if raw_dir.join(".git").exists() {
        let head_spec = format!("HEAD:{source_id}.md");
        if let Some(bytes) = isolated_git_cat(&raw_dir, &["show", &head_spec]) {
            return Some(bytes);
        }
        let git_ref = git_blob_or_commit.trim();
        if !git_ref.is_empty() {
            let commit_spec = format!("{git_ref}:{source_id}.md");
            if let Some(bytes) = isolated_git_cat(&raw_dir, &["show", &commit_spec]) {
                return Some(bytes);
            }
            if let Some(bytes) = isolated_git_cat(&raw_dir, &["cat-file", "blob", git_ref]) {
                return Some(bytes);
            }
        }
    }
    None
}

/// Idempotently upgrades an M1 (`user_version == 1`) database to `VERSION == 2`:
/// - Adds `library_meta.(embedding_model, embedding_dims)` and `(chunk_vectors, fact_vectors).model_id`;
/// - Drops `connector_cursors` and creates `raw_sources_ext_key` index;
/// - Migrates every unpurged `raw_sources` payload from `raw/{source_id}.md` / `raw/.git` into
///   `raw_blobs` with SHA-256 verification;
/// - Updates `library_meta.schema_ir_hash` and `PRAGMA user_version = 2`;
/// - Removes `<library_path>/raw` only after `tx.commit()` confirms zero unmigrated sources remain.
pub(crate) fn migrate_v1_to_v2(db: &mut Connection, library_path: &Path) -> Result<()> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let cur_ver: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if cur_ver == VERSION {
        tx.commit()?;
        return Ok(());
    }
    if cur_ver != 1 {
        return Err(StoreError::IncompatibleSchema);
    }

    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS raw_blobs (
            content_sha256 TEXT PRIMARY KEY NOT NULL,
            byte_len INTEGER NOT NULL CHECK(byte_len >= 0),
            content_blob BLOB NOT NULL,
            created_at INTEGER NOT NULL
        );",
    )?;
    if !has_column(&tx, "library_meta", "embedding_model")? {
        tx.execute(
            "ALTER TABLE library_meta ADD COLUMN embedding_model TEXT NOT NULL DEFAULT 'hash:v1'",
            [],
        )?;
    }
    if !has_column(&tx, "library_meta", "embedding_dims")? {
        tx.execute(
            "ALTER TABLE library_meta ADD COLUMN embedding_dims INTEGER NOT NULL DEFAULT 256 CHECK(embedding_dims >= 0)",
            [],
        )?;
    }
    if !has_column(&tx, "chunk_vectors", "model_id")? {
        tx.execute(
            "ALTER TABLE chunk_vectors ADD COLUMN model_id TEXT NOT NULL DEFAULT 'hash:v1'",
            [],
        )?;
    }
    if !has_column(&tx, "fact_vectors", "model_id")? {
        tx.execute(
            "ALTER TABLE fact_vectors ADD COLUMN model_id TEXT NOT NULL DEFAULT 'hash:v1'",
            [],
        )?;
    }
    if !has_column(&tx, "raw_sources", "external_source_key")? {
        tx.execute(
            "ALTER TABLE raw_sources ADD COLUMN external_source_key TEXT",
            [],
        )?;
    }
    if !has_column(&tx, "raw_sources", "superseded_by_source")? {
        tx.execute(
            "ALTER TABLE raw_sources ADD COLUMN superseded_by_source TEXT",
            [],
        )?;
    }
    if !has_column(&tx, "extraction_queue", "thread_key")? {
        tx.execute(
            "ALTER TABLE extraction_queue ADD COLUMN thread_key TEXT",
            [],
        )?;
    }
    if !has_column(&tx, "extraction_queue", "reply_to_key")? {
        tx.execute(
            "ALTER TABLE extraction_queue ADD COLUMN reply_to_key TEXT",
            [],
        )?;
    }
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS raw_sources_ext_key ON raw_sources(external_source_key) WHERE external_source_key IS NOT NULL;
         CREATE INDEX IF NOT EXISTS extraction_queue_thread ON extraction_queue(thread_key, reference_time ASC, created_at ASC) WHERE thread_key IS NOT NULL;
         DROP TABLE IF EXISTS connector_cursors;",
    )?;

    for slot in core_lexicon() {
        tx.execute(
            "INSERT OR IGNORE INTO schema_slots VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                slot.predicate,
                slot.namespace,
                slot.cardinality.as_str(),
                slot.mutability.as_str(),
                slot.value_kind,
                slot.description,
                slot.instruction,
            ],
        )?;
    }

    let missing_sources: Vec<(String, String, String, i64)> = {
        let mut stmt = tx.prepare(
            "SELECT s.source_id, s.content_sha256, s.git_blob_or_commit, s.captured_at
             FROM raw_sources s
             WHERE NOT EXISTS (
                 SELECT 1 FROM raw_blobs b WHERE b.content_sha256 = s.content_sha256
             )
               AND NOT EXISTS (
                 SELECT 1 FROM tombstones t WHERE t.tombstone_id = s.source_id
             )
             ORDER BY s.captured_at ASC, s.source_id ASC",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        rows.collect::<std::result::Result<_, _>>()?
    };

    for (source_id, expected_sha, git_ref, captured_at) in missing_sources {
        let Some(bytes) = read_legacy_raw_source_bytes(library_path, &source_id, &git_ref) else {
            return Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("legacy raw source `{source_id}` missing during v1->v2 migration"),
            )));
        };
        let actual_sha = ContentHash::digest(&bytes);
        if actual_sha.as_str() != expected_sha {
            return Err(StoreError::IncompatibleSchema);
        }
        let byte_len = i64::try_from(bytes.len()).map_err(|_| StoreError::IncompatibleSchema)?;
        tx.execute(
            "INSERT OR IGNORE INTO raw_blobs (content_sha256, byte_len, content_blob, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![expected_sha, byte_len, &bytes, captured_at],
        )?;
        tx.execute(
            "UPDATE raw_sources SET git_blob_or_commit = ?1 WHERE source_id = ?2",
            params![expected_sha, source_id],
        )?;
    }

    let new_hash = schema_ir_hash()?;
    tx.execute(
        "UPDATE library_meta SET schema_ir_hash = ?1",
        [new_hash.as_str()],
    )?;
    tx.pragma_update(None, "user_version", VERSION)?;
    tx.commit()?;

    finalize_legacy_raw_dir(db, library_path)
}

/// Ensures any residual `<library_path>/raw` directory left behind by a prior crash after `v1->v2`
/// commit (or manual legacy copy) has all surviving unpurged `raw_sources` backed by `raw_blobs`,
/// then removes `<library_path>/raw` while propagating any non-`NotFound` filesystem error.
pub(crate) fn finalize_legacy_raw_dir(db: &mut Connection, library_path: &Path) -> Result<()> {
    let raw_dir = library_path.join("raw");
    if !raw_dir.exists() {
        return Ok(());
    }

    let missing_sources: Vec<(String, String, String, i64)> = {
        let mut stmt = db.prepare(
            "SELECT s.source_id, s.content_sha256, s.git_blob_or_commit, s.captured_at
             FROM raw_sources s
             WHERE NOT EXISTS (
                 SELECT 1 FROM raw_blobs b WHERE b.content_sha256 = s.content_sha256
             )
               AND NOT EXISTS (
                 SELECT 1 FROM tombstones t WHERE t.tombstone_id = s.source_id
             )
             ORDER BY s.captured_at ASC, s.source_id ASC",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        rows.collect::<std::result::Result<_, _>>()?
    };

    if !missing_sources.is_empty() {
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (source_id, expected_sha, git_ref, captured_at) in missing_sources {
            let Some(bytes) = read_legacy_raw_source_bytes(library_path, &source_id, &git_ref)
            else {
                return Err(StoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("legacy raw source `{source_id}` missing during raw/ finalization"),
                )));
            };
            let actual_sha = ContentHash::digest(&bytes);
            if actual_sha.as_str() != expected_sha {
                return Err(StoreError::IncompatibleSchema);
            }
            let byte_len =
                i64::try_from(bytes.len()).map_err(|_| StoreError::IncompatibleSchema)?;
            tx.execute(
                "INSERT OR IGNORE INTO raw_blobs (content_sha256, byte_len, content_blob, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![expected_sha, byte_len, &bytes, captured_at],
            )?;
            tx.execute(
                "UPDATE raw_sources SET git_blob_or_commit = ?1 WHERE source_id = ?2",
                params![expected_sha, source_id],
            )?;
        }
        tx.commit()?;
    }

    match fs::remove_dir_all(&raw_dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(StoreError::Io(err)),
    }
}

pub(crate) fn ensure_v2_queue_columns(db: &mut Connection) -> Result<()> {
    let has_thread = has_column(db, "extraction_queue", "thread_key")?;
    let has_reply = has_column(db, "extraction_queue", "reply_to_key")?;
    if has_thread && has_reply {
        return Ok(());
    }
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if !has_column(&tx, "extraction_queue", "thread_key")? {
        tx.execute(
            "ALTER TABLE extraction_queue ADD COLUMN thread_key TEXT",
            [],
        )?;
    }
    if !has_column(&tx, "extraction_queue", "reply_to_key")? {
        tx.execute(
            "ALTER TABLE extraction_queue ADD COLUMN reply_to_key TEXT",
            [],
        )?;
    }
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS extraction_queue_thread ON extraction_queue(thread_key, reference_time ASC, created_at ASC) WHERE thread_key IS NOT NULL;",
    )?;
    let new_hash = schema_ir_hash()?;
    tx.execute(
        "UPDATE library_meta SET schema_ir_hash = ?1",
        [new_hash.as_str()],
    )?;
    tx.commit()?;
    Ok(())
}
