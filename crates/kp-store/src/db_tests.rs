use kp_model::{core_lexicon, LibraryMode};

use super::*;
use crate::test_support::{library, options, AT};

#[test]
fn personal_and_community_share_schema_and_pragmas() -> Result<()> {
    let (personal_dir, personal) = library(LibraryMode::Personal)?;
    let (community_dir, community) = library(LibraryMode::Community)?;
    assert_eq!(
        personal.status()?.schema_ir_hash,
        community.status()?.schema_ir_hash
    );
    let ddl = |store: &Store| -> Result<Vec<(String, String)>> {
        let mut stmt = store
            .db
            .prepare("SELECT name,sql FROM sqlite_schema WHERE sql IS NOT NULL ORDER BY name")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    };
    assert_eq!(ddl(&personal)?, ddl(&community)?);
    for store in [&personal, &community] {
        assert_eq!(
            store
                .db
                .pragma_query_value(None, "journal_mode", |r| r.get::<_, String>(0))?,
            "wal"
        );
        assert_eq!(
            store
                .db
                .pragma_query_value(None, "foreign_keys", |r| r.get::<_, i64>(0))?,
            1
        );
        assert_eq!(
            store
                .db
                .pragma_query_value(None, "synchronous", |r| r.get::<_, i64>(0))?,
            1
        );
        assert_eq!(
            store
                .db
                .pragma_query_value(None, "secure_delete", |r| r.get::<_, i64>(0))?,
            1
        );
        assert_eq!(
            store
                .db
                .query_row("SELECT count(*) FROM schema_slots", [], |r| r
                    .get::<_, usize>(0))?,
            core_lexicon().len()
        );
        assert_eq!(store.status()?.members, 1);
        let schema = ddl(store)?;
        for expected in [
            "library_meta",
            "members_tokens",
            "tombstones",
            "source_scopes",
            "raw_blobs",
            "raw_sources",
            "chunks",
            "chunks_fts",
            "chunk_vectors",
            "schema_slots",
            "facts",
            "facts_fts",
            "fact_vectors",
            "fact_entities",
            "justifications",
            "justification_premises",
            "code_anchors",
            "rsi_episodes",
            "rsi_episode_facts",
            "rsi_feedback",
            "skill_approvals",
            "session_cursors",
            "extraction_queue",
            "extracted_chunks",
            "rejected_claims",
            "review_items",
            "community_concepts",
            "external_identities",
            "projection_pages",
            "change_log",
            "outbox",
        ] {
            assert!(
                schema.iter().any(|(name, _)| name == expected),
                "missing {expected}"
            );
        }
        assert!(
            !schema.iter().any(|(name, _)| name == "connector_cursors"),
            "connector_cursors must be removed from Core schema"
        );
    }
    assert!(!personal_dir.path().join("raw").exists());
    assert!(!community_dir.path().join("raw").exists());
    drop(personal);
    drop(community);
    assert_eq!(
        Store::open(personal_dir.path())?.status()?.mode,
        LibraryMode::Personal
    );
    assert_eq!(
        Store::open(community_dir.path())?.status()?.mode,
        LibraryMode::Community
    );
    Ok(())
}

#[test]
fn init_and_open_never_overwrite_or_create_existing_state() -> Result<()> {
    let (dir, store) = library(LibraryMode::Personal)?;
    let before = serde_json::to_vec(&store.status()?)?;
    assert!(matches!(
        Store::init(dir.path(), options(LibraryMode::Community)?),
        Err(StoreError::AlreadyInitialized(_))
    ));
    assert_eq!(serde_json::to_vec(&store.status()?)?, before);
    let missing = dir.path().join("missing");
    assert!(matches!(
        Store::open(&missing),
        Err(StoreError::NotInitialized(_))
    ));
    assert!(!missing.exists());
    store.db.pragma_update(None, "user_version", 99)?;
    assert!(matches!(
        Store::open(dir.path()),
        Err(StoreError::IncompatibleSchema)
    ));
    assert_eq!(
        store
            .db
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?,
        99
    );
    Ok(())
}

#[test]
fn member_tokens_are_unique_hashed_and_issued_once() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Community)?;
    let alice = store.add_member("alice".parse()?, MemberRole::Member, AT)?;
    let bob = store.add_member("bob".parse()?, MemberRole::Maintainer, AT)?;
    assert_ne!(alice.token, bob.token);
    let stored: String = store.db.query_row(
        "SELECT token_sha256 FROM members_tokens WHERE actor_id='alice'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(stored, ContentHash::digest(&alice.token).as_str());
    assert!(!serde_json::to_string(&store.status()?)?.contains(&alice.token));
    assert!(matches!(
        store.add_member("alice".parse()?, MemberRole::Owner, AT),
        Err(StoreError::Conflict)
    ));
    let role: String = store.db.query_row(
        "SELECT role FROM members_tokens WHERE actor_id='alice'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(role, "member");
    let owner = store.add_member("owner".parse()?, MemberRole::Owner, AT)?;
    assert!(!owner.token.is_empty());
    assert_eq!(store.status()?.members, 3);
    Ok(())
}

#[test]
fn connections_wait_up_to_ten_seconds_for_busy_databases() -> Result<()> {
    let (dir, initialized) = library(LibraryMode::Personal)?;
    let reopened = Store::open(dir.path())?;
    for store in [&initialized, &reopened] {
        assert_eq!(
            store
                .db
                .pragma_query_value(None, "busy_timeout", |r| r.get::<_, i64>(0))?,
            10_000
        );
    }
    Ok(())
}

#[test]
fn failed_initialization_preserves_preexisting_files_and_creates_no_raw_dir() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let marker = dir.path().join("existing.md");
    fs::write(&marker, "keep me")?;

    let mut bad_opts = options(LibraryMode::Personal)?;
    bad_opts.name = "   ".into();
    assert!(Store::init(dir.path(), bad_opts).is_err());

    assert!(
        Store::init_with_embedding(dir.path(), options(LibraryMode::Personal)?, "none", 256,)
            .is_err()
    );

    assert_eq!(fs::read_to_string(&marker)?, "keep me");
    for name in ["kp.sqlite", "kp.sqlite-wal", "kp.sqlite-shm", "raw"] {
        assert!(!dir.path().join(name).exists());
    }
    let store = Store::init(dir.path(), options(LibraryMode::Personal)?)?;
    assert!(!dir.path().join("raw").exists());
    assert_eq!(store.status()?.embedding_model, "hash:v1");
    assert_eq!(store.status()?.embedding_dims, 256);
    Ok(())
}

#[test]
fn opening_and_reading_current_schema_does_not_acquire_a_write_lock_or_project_skills() -> Result<()>
{
    let (dir, store) = library(LibraryMode::Personal)?;
    let marker = dir.path().join("skills/preserved/SKILL.md");
    fs::create_dir_all(marker.parent().ok_or(StoreError::NotFound)?)?;
    fs::write(&marker, "untouched by reads")?;
    let tx = store.db.unchecked_transaction()?;
    tx.execute("UPDATE library_meta SET name='uncommitted writer'", [])?;
    let reader = Store::open(dir.path())?;
    assert_eq!(reader.status()?.name, "Test Library");
    assert!(reader
        .retrieval_snapshot(&"owner".parse()?, AT)?
        .documents()
        .is_empty());
    assert_eq!(fs::read_to_string(&marker)?, "untouched by reads");
    tx.rollback()?;
    Ok(())
}

#[test]
fn source_scope_departure_immediately_hides_scoped_facts_at_read_time() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Community)?;
    store.add_member("bob".parse()?, MemberRole::Member, AT)?;
    let scope_id: kp_model::SourceScopeId = "ext_chat.oc_eng".parse()?;
    let mut scope = kp_model::SourceScope {
        scope_id: scope_id.clone(),
        provider: "ext_chat".into(),
        external_container_id: "oc_eng".into(),
        label: "协作讨论组".into(),
        current_audience: kp_model::AudienceSpec::Explicit(
            ["owner".parse()?, "bob".parse()?].into_iter().collect(),
        ),
        synced_at: AT,
    };
    store.upsert_source_scope(&scope)?;

    let mut f = crate::test_support::fact("f_scoped", &["owner", "bob"])?;
    f.source_scope_id = Some(scope_id);
    store.insert_fact(&f)?;

    assert_eq!(store.visible_facts(&"bob".parse()?, AT)?.len(), 1);

    scope.current_audience =
        kp_model::AudienceSpec::Explicit(["owner".parse()?].into_iter().collect());
    store.upsert_source_scope(&scope)?;

    assert!(store.visible_facts(&"bob".parse()?, AT)?.is_empty());
    assert_eq!(store.visible_facts(&"owner".parse()?, AT)?.len(), 1);
    Ok(())
}

#[test]
fn open_readonly_caches_owner_requires_no_raw_git_and_enforces_query_only() -> Result<()> {
    let (dir, mut store) = library(LibraryMode::Personal)?;
    let owner: ActorId = "owner".parse()?;
    assert_eq!(store.owner_actor(), &owner);

    let f = crate::test_support::fact("f_ro_probe", &["owner"])?;
    store.insert_fact(&f)?;

    let reopened = Store::open(dir.path())?;
    assert_eq!(reopened.owner_actor(), &owner);
    drop(reopened);

    // Hold an uncommitted write lock on `store.db` to prove `open_readonly` requires zero WAL write lock.
    let tx = store.db.unchecked_transaction()?;
    tx.execute("UPDATE library_meta SET name='uncommitted'", [])?;

    let mut ro = Store::open_readonly(dir.path())?;
    assert_eq!(ro.owner_actor(), &owner);
    assert_eq!(
        ro.db
            .pragma_query_value(None, "query_only", |r| r.get::<_, i64>(0))?,
        1
    );
    assert_eq!(
        ro.db
            .pragma_query_value(None, "busy_timeout", |r| r.get::<_, i64>(0))?,
        10_000
    );
    assert_eq!(ro.visible_facts(&owner, AT)?.len(), 1);
    assert!(ro
        .add_member("bob".parse()?, MemberRole::Member, AT)
        .is_err());
    drop(ro);
    tx.rollback()?;
    drop(store);

    assert!(!dir.path().join("raw").exists());
    let ro_after = Store::open_readonly(dir.path())?;
    assert_eq!(ro_after.owner_actor(), &owner);
    assert_eq!(ro_after.visible_facts(&owner, AT)?.len(), 1);
    Ok(())
}
