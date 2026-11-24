use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kp_model::{ActorId, ContentHash, EpochMs, KpError, LibraryId, LibraryMode, MemberRole};
use rusqlite::{params, Connection, OpenFlags, TransactionBehavior};
use serde::Serialize;
use uuid::Uuid;

use crate::embedding::default_embedder_for_meta;
use crate::{migrations, Embedder, Result, StoreError, Tokenizer};

pub struct InitOptions {
    pub library_id: LibraryId,
    pub name: String,
    pub mode: LibraryMode,
    pub owner: ActorId,
    pub created_at: EpochMs,
}

#[derive(Clone, Debug, Serialize)]
pub struct LibraryStatus {
    pub library_id: LibraryId,
    pub name: String,
    pub mode: LibraryMode,
    pub owner_actor: ActorId,
    pub schema_ir_hash: ContentHash,
    pub created_at: EpochMs,
    pub embedding_model: String,
    pub embedding_dims: usize,
    pub members: u64,
    pub sources: u64,
    pub facts: u64,
}

/// The only API returning a bearer token. Debug deliberately omits the credential.
#[derive(Serialize)]
pub struct MemberToken {
    pub actor_id: ActorId,
    pub role: MemberRole,
    pub token: String,
}

pub struct Store {
    pub(crate) path: PathBuf,
    pub(crate) db: Connection,
    pub(crate) library_id: LibraryId,
    pub(crate) mode: LibraryMode,
    pub(crate) owner_actor: ActorId,
    pub(crate) tokenizer: Tokenizer,
    pub(crate) embedder: Arc<dyn Embedder + Send + Sync>,
}

impl Store {
    /// Initializes a new library using the deterministic zero-network `hash:v1` (`256d`) default.
    /// Existing databases are never overwritten.
    pub fn init(path: impl AsRef<Path>, options: InitOptions) -> Result<Self> {
        Self::init_with_embedding(path, options, "hash:v1", 256)
    }

    /// Initializes a new library with an explicit `embedding_model` and locked `embedding_dims`
    /// (`"none"` requires `embedding_dims == 0`; `0` for non-`"none"` models auto-locks on first write).
    pub fn init_with_embedding(
        path: impl AsRef<Path>,
        options: InitOptions,
        embedding_model: &str,
        embedding_dims: usize,
    ) -> Result<Self> {
        let path = path.as_ref();
        if options.name.trim().is_empty() {
            return Err(KpError::invalid("name", "must be nonempty").into());
        }
        let clean_model = embedding_model.trim();
        if clean_model.is_empty() {
            return Err(KpError::invalid("embedding_model", "must be nonempty").into());
        }
        if clean_model == "none" && embedding_dims != 0 {
            return Err(KpError::invalid(
                "embedding_dims",
                "embedding_dims must be 0 when embedding_model is `none`",
            )
            .into());
        }
        let dims_i64 = i64::try_from(embedding_dims)
            .map_err(|_| KpError::invalid("embedding_dims", "embedding dimension overflow"))?;

        fs::create_dir_all(path)?;
        let db_path = path.join("kp.sqlite");
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&db_path)
        {
            Ok(file) => drop(file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(StoreError::AlreadyInitialized(path.into()));
            }
            Err(error) => return Err(error.into()),
        }
        let result = Self::initialize(path, &db_path, options, clean_model, dims_i64);
        if result.is_err() {
            for name in ["kp.sqlite", "kp.sqlite-wal", "kp.sqlite-shm"] {
                let _ = fs::remove_file(path.join(name));
            }
        }
        result
    }

    fn initialize(
        path: &Path,
        db_path: &Path,
        options: InitOptions,
        embedding_model: &str,
        embedding_dims: i64,
    ) -> Result<Self> {
        let mut db = Connection::open(db_path)?;
        db.busy_timeout(Duration::from_millis(10_000))?;
        configure(&db)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        migrations::initialize(&tx)?;
        tx.execute(
            "INSERT INTO library_meta
                (library_id, name, mode, owner_actor, schema_ir_hash, created_at, embedding_model, embedding_dims)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                options.library_id.as_str(),
                options.name,
                options.mode.as_str(),
                options.owner.as_str(),
                migrations::schema_ir_hash()?.as_str(),
                options.created_at.0,
                embedding_model,
                embedding_dims,
            ],
        )?;
        tx.execute(
            "INSERT INTO members_tokens (actor_id,display_name,role,created_at) VALUES (?1,?1,'owner',?2)",
            params![options.owner.as_str(), options.created_at.0],
        )?;
        tx.commit()?;
        let dims_usize = usize::try_from(embedding_dims).unwrap_or(0);
        Ok(Self {
            path: path.to_owned(),
            db,
            library_id: options.library_id,
            mode: options.mode,
            owner_actor: options.owner,
            tokenizer: Tokenizer::default(),
            embedder: default_embedder_for_meta(embedding_model, dims_usize),
        })
    }

    /// Opens an existing library, transparently upgrading M1 (`user_version == 1`) databases to `VERSION 2`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let db_path = path.join("kp.sqlite");
        if !db_path.is_file() {
            return Err(StoreError::NotInitialized(path.into()));
        }
        let mut db = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        db.busy_timeout(Duration::from_millis(10_000))?;
        configure(&db)?;
        let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version == 1 {
            migrations::migrate_v1_to_v2(&mut db, path)?;
        } else if version == migrations::VERSION {
            migrations::ensure_v2_queue_columns(&mut db)?;
            if path.join("raw").exists() {
                migrations::finalize_legacy_raw_dir(&mut db, path)?;
            }
        } else {
            return Err(StoreError::IncompatibleSchema);
        }
        crate::public_ingest::repair_legacy_named_endpoint_queue_rows(&db)?;
        let (id, mode, owner, hash, emb_model, emb_dims): (
            String,
            String,
            String,
            String,
            String,
            i64,
        ) = db.query_row(
            "SELECT library_id,mode,owner_actor,schema_ir_hash,embedding_model,embedding_dims FROM library_meta",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )?;
        if hash != migrations::schema_ir_hash()?.as_str() {
            return Err(StoreError::IncompatibleSchema);
        }
        let dims_usize = usize::try_from(emb_dims)
            .map_err(|_| KpError::invalid("embedding_dims", "negative stored embedding_dims"))?;
        Ok(Self {
            path: path.to_owned(),
            db,
            library_id: id.parse()?,
            mode: mode.parse()?,
            owner_actor: owner.parse()?,
            tokenizer: Tokenizer::default(),
            embedder: default_embedder_for_meta(&emb_model, dims_usize),
        })
    }

    /// Opens an existing library strictly read-only (`SQLITE_OPEN_READ_ONLY` + `PRAGMA query_only=ON`)
    /// without issuing `PRAGMA journal_mode=WAL` (compatible with both `user_version == 1` and `2`).
    pub fn open_readonly(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let db_path = path.join("kp.sqlite");
        if !db_path.is_file() {
            return Err(StoreError::NotInitialized(path.into()));
        }
        let db = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        db.busy_timeout(Duration::from_millis(10_000))?;
        db.execute_batch("PRAGMA query_only=ON; PRAGMA foreign_keys=ON;")?;
        let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let (id, mode, owner, emb_model, dims_usize) = if version == migrations::VERSION {
            let (id, mode, owner, hash, emb_model, emb_dims): (
                String,
                String,
                String,
                String,
                String,
                i64,
            ) = db.query_row(
                "SELECT library_id,mode,owner_actor,schema_ir_hash,embedding_model,embedding_dims FROM library_meta",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )?;
            if hash != migrations::schema_ir_hash()?.as_str() {
                return Err(StoreError::IncompatibleSchema);
            }
            let dims = usize::try_from(emb_dims).map_err(|_| {
                KpError::invalid("embedding_dims", "negative stored embedding_dims")
            })?;
            (id, mode, owner, emb_model, dims)
        } else if version == 1 {
            if migrations::has_column(&db, "library_meta", "embedding_model")? {
                let (id, mode, owner, emb_model, emb_dims): (
                    String,
                    String,
                    String,
                    String,
                    i64,
                ) = db.query_row(
                    "SELECT library_id,mode,owner_actor,embedding_model,embedding_dims FROM library_meta",
                    [],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )?;
                let dims = usize::try_from(emb_dims).map_err(|_| {
                    KpError::invalid("embedding_dims", "negative stored embedding_dims")
                })?;
                (id, mode, owner, emb_model, dims)
            } else {
                let (id, mode, owner): (String, String, String) = db.query_row(
                    "SELECT library_id,mode,owner_actor FROM library_meta",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
                (id, mode, owner, "hash:v1".to_owned(), 256)
            }
        } else {
            return Err(StoreError::IncompatibleSchema);
        };
        Ok(Self {
            path: path.to_owned(),
            db,
            library_id: id.parse()?,
            mode: mode.parse()?,
            owner_actor: owner.parse()?,
            tokenizer: Tokenizer::default(),
            embedder: default_embedder_for_meta(&emb_model, dims_usize),
        })
    }

    pub fn library_id(&self) -> &LibraryId {
        &self.library_id
    }

    #[inline]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[inline]
    pub fn owner_actor(&self) -> &ActorId {
        &self.owner_actor
    }

    pub fn tokenizer_mut(&mut self) -> &mut Tokenizer {
        &mut self.tokenizer
    }

    /// Replaces the active in-process [`Embedder`] used by default write and retrieval methods.
    pub fn set_embedder(&mut self, embedder: Arc<dyn Embedder + Send + Sync>) {
        self.embedder = embedder;
    }

    /// Returns a cloned `Arc` handle to the active [`Embedder`] attached to this `Store`.
    #[inline]
    pub fn embedder_arc(&self) -> Arc<dyn Embedder + Send + Sync> {
        Arc::clone(&self.embedder)
    }

    /// Returns the active [`Embedder`] attached to this `Store`.
    #[inline]
    pub fn embedder(&self) -> &(dyn Embedder + Send + Sync) {
        self.embedder.as_ref()
    }

    /// Reads the persisted `(embedding_model, embedding_dims)` tuple from `library_meta`.
    pub fn embedding_config(&self) -> Result<(String, usize)> {
        if !migrations::has_column(&self.db, "library_meta", "embedding_model")? {
            return Ok(("hash:v1".to_owned(), 256));
        }
        let (model, dims): (String, i64) = self.db.query_row(
            "SELECT embedding_model, embedding_dims FROM library_meta",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let dims_usize = usize::try_from(dims)
            .map_err(|_| KpError::invalid("embedding_dims", "negative stored embedding_dims"))?;
        Ok((model, dims_usize))
    }

    /// Local administrative status; this is not a viewer-scoped query endpoint.
    pub fn status(&self) -> Result<LibraryStatus> {
        let (name, mode, owner, hash, created_at): (String, String, String, String, i64) =
            self.db.query_row(
                "SELECT name,mode,owner_actor,schema_ir_hash,created_at FROM library_meta",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )?;
        let (emb_model, embedding_dims) = self.embedding_config()?;
        Ok(LibraryStatus {
            library_id: self.library_id.clone(),
            name,
            mode: mode.parse()?,
            owner_actor: owner.parse()?,
            schema_ir_hash: hash.parse()?,
            created_at: EpochMs(created_at),
            embedding_model: emb_model,
            embedding_dims,
            members: self.db.query_row(
                "SELECT count(*) FROM members_tokens WHERE revoked_at IS NULL",
                [],
                |r| r.get(0),
            )?,
            sources: self
                .db
                .query_row("SELECT count(*) FROM raw_sources", [], |r| r.get(0))?,
            facts: self
                .db
                .query_row("SELECT count(*) FROM facts", [], |r| r.get(0))?,
        })
    }

    /// Issues one token for a new member (or the initial owner, who has no token yet).
    /// Duplicate actors are rejected without rotating their existing token.
    pub fn add_member(
        &mut self,
        actor: ActorId,
        role: MemberRole,
        at: EpochMs,
    ) -> Result<MemberToken> {
        let token = format!("kp_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let hash = ContentHash::digest(token.as_bytes());
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "INSERT INTO members_tokens (actor_id,display_name,role,token_sha256,created_at)
             VALUES (?1,?1,?2,?3,?4) ON CONFLICT(actor_id) DO UPDATE SET role=excluded.role,token_sha256=excluded.token_sha256
             WHERE members_tokens.token_sha256 IS NULL AND members_tokens.revoked_at IS NULL AND (members_tokens.role=excluded.role OR (members_tokens.role='member' AND excluded.role!='owner'))",
            params![actor.as_str(),role.as_str(),hash.as_str(),at.0])?;
        if changed == 0 {
            return Err(StoreError::Conflict);
        }
        crate::exchange::mark_alignment_dirty_tx(&tx, at)?;
        tx.commit()?;
        Ok(MemberToken {
            actor_id: actor,
            role,
            token,
        })
    }

    /// Registers community peer `ActorId`s in a personal library (`token_sha256 = NULL`) so
    /// natural-language mentions (`"Alice 是我组里的"`) and scope expansions resolve without manual
    /// `kp member add`. Existing members and tokens are never overwritten.
    pub fn register_peer_members(&mut self, actors: &[ActorId], at: EpochMs) -> Result<usize> {
        if actors.is_empty() {
            return Ok(0);
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut inserted = 0usize;
        for actor in actors {
            inserted += tx.execute(
                "INSERT INTO members_tokens (actor_id,display_name,role,token_sha256,created_at)
                 VALUES (?1,?1,'member',NULL,?2)
                 ON CONFLICT(actor_id) DO NOTHING",
                params![actor.as_str(), at.0],
            )?;
        }
        if inserted > 0 {
            crate::exchange::mark_alignment_dirty_tx(&tx, at)?;
        }
        tx.commit()?;
        Ok(inserted)
    }
}

fn configure(db: &Connection) -> Result<()> {
    let journal_mode: String = db.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        db.execute_batch("PRAGMA journal_mode=WAL;")?;
    }
    db.execute_batch(
        "PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL; PRAGMA secure_delete=ON;",
    )?;
    Ok(())
}

#[cfg(test)]
#[path = "db_tests.rs"]
mod tests;
