use std::fs;
use std::path::Path;

use kp_model::LibraryMode;

use super::*;
use crate::test_support::{library, source, AT};
use crate::tombstone::find_sqlite_table_containing_bytes;

fn assert_no_plaintext(path: &Path, needle: &[u8]) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            assert_no_plaintext(&path, needle)?;
        } else {
            let bytes = fs::read(&path)?;
            assert!(
                !bytes.windows(needle.len()).any(|window| window == needle),
                "residue in {path:?}"
            );
        }
    }
    Ok(())
}

#[test]
fn fts_rowids_survive_vacuum_after_earlier_sources_are_purged() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let origin = "origin".parse()?;
    let viewer = "owner".parse()?;
    let mut saved = Vec::new();
    for term in ["firstcanary", "secondcanary", "survivorcanary"] {
        let mut input = source(&["owner"])?;
        input.content = term.as_bytes().to_vec();
        input.chunks[0].body = term.into();
        saved.push(store.ingest_source(&origin, &term.parse()?, &input)?);
    }
    let before = store.search_chunks("survivorcanary", &viewer)?;
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].source_id, saved[2].source_id);
    for term in ["firstcanary", "secondcanary"] {
        store.purge(&origin, &term.parse()?, AT, &[])?;
    }
    store.db.execute_batch("VACUUM;")?;
    assert_eq!(store.search_chunks("survivorcanary", &viewer)?, before);
    for term in ["firstcanary", "secondcanary"] {
        assert!(store.search_chunks(term, &viewer)?.is_empty());
        assert_eq!(
            store.db.query_row(
                "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH ?1",
                [term],
                |r| r.get::<_, i64>(0)
            )?,
            0
        );
    }
    // The surviving FTS row must still be removed by the trigger after VACUUM.
    store.purge(&origin, &"survivorcanary".parse()?, AT, &[])?;
    assert_eq!(
        store
            .db
            .query_row("SELECT count(*) FROM chunks_fts", [], |r| r
                .get::<_, i64>(0))?,
        0
    );
    Ok(())
}

#[test]
fn purge_erases_raw_blobs_with_zero_residue_and_preserves_surviving_blob_references() -> Result<()>
{
    const SECRET: &str = "TOP_SECRET_SQLITE_BLOB_CANARY_7788";
    let (dir, mut store) = library(LibraryMode::Personal)?;
    assert!(!dir.path().join("raw").exists());

    let origin = "origin".parse()?;
    let key = "secret".parse()?;
    let mut input = source(&["owner"])?;
    input.content = SECRET.as_bytes().to_vec();
    input.chunks[0].body = SECRET.into();
    let removed = store.ingest_source(&origin, &key, &input)?;
    assert_eq!(
        store.raw_source_bytes(&removed.source_id)?.as_deref(),
        Some(SECRET.as_bytes())
    );
    assert_eq!(
        store.raw_source_text(&removed.source_id)?.as_deref(),
        Some(SECRET)
    );

    // Ingest two surviving sources sharing identical content to verify deduplicated `raw_blobs` ref-preservation.
    let surviving_input = source(&["owner"])?;
    let survivor_a = store.ingest_source(&origin, &"survivor_a".parse()?, &surviving_input)?;
    let survivor_b = store.ingest_source(&origin, &"survivor_b".parse()?, &surviving_input)?;
    assert_eq!(survivor_a.content_sha256, survivor_b.content_sha256);

    store.purge(&origin, &key, AT, &[])?;
    store.purge(&origin, &key, AT, &[])?;

    assert!(store.raw_source_bytes(&removed.source_id)?.is_none());
    assert!(store.raw_source_text(&removed.source_id)?.is_none());
    assert_eq!(
        find_sqlite_table_containing_bytes(dir.path(), SECRET.as_bytes())?,
        None
    );
    assert_no_plaintext(dir.path(), SECRET.as_bytes())?;
    assert_no_plaintext(dir.path(), SECRET.to_ascii_lowercase().as_bytes())?;

    // Purging `survivor_a` keeps the shared `raw_blobs` row alive for `survivor_b`.
    store.purge(&origin, &"survivor_a".parse()?, AT, &[])?;
    assert!(store.raw_source_bytes(&survivor_a.source_id)?.is_none());
    assert_eq!(
        store.raw_source_bytes(&survivor_b.source_id)?.as_deref(),
        Some(surviving_input.content.as_slice())
    );

    let stored: String = store.db.query_row(
        "SELECT git_blob_or_commit FROM raw_sources WHERE source_id=?1",
        [survivor_b.source_id.as_str()],
        |r| r.get(0),
    )?;
    assert_eq!(stored, survivor_b.git_blob);
    assert!(store.search_chunks(SECRET, &"owner".parse()?)?.is_empty());

    drop(store);
    let mut store = Store::open(dir.path())?;
    assert_eq!(store.status()?.sources, 1);
    let new_source = store.ingest_source(&origin, &"after-purge".parse()?, &surviving_input)?;
    assert_eq!(
        store.raw_source_bytes(&new_source.source_id)?.as_deref(),
        Some(surviving_input.content.as_slice())
    );
    Ok(())
}

#[test]
fn raw_ingest_records_blob_and_searches_two_character_cjk() -> Result<()> {
    let (dir, mut store) = library(LibraryMode::Personal)?;
    let origin = "origin".parse()?;
    let key = "weather".parse()?;
    let input = source(&["owner"])?;
    let saved = store.ingest_source(&origin, &key, &input)?;
    assert!(!dir.path().join("raw").exists());
    assert_eq!(saved.git_blob, saved.content_sha256.as_str());
    assert_eq!(
        store.raw_source_bytes(&saved.source_id)?.as_deref(),
        Some(input.content.as_slice())
    );
    for query in ["下雨", "开车", "getWeather", "weather", "HTTP_client"] {
        let hits = store.search_chunks(query, &"owner".parse()?)?;
        assert_eq!(hits.len(), 1, "query {query}");
        assert_eq!(hits[0].source_id, saved.source_id);
    }
    assert!(store
        .search_chunks("下雨", &"outsider".parse()?)?
        .is_empty());
    assert!(matches!(
        store.ingest_source(&origin, &key, &input),
        Err(StoreError::Conflict)
    ));
    Ok(())
}

#[test]
fn purge_removes_fts_vectors_raw_and_rejects_source_replay() -> Result<()> {
    let (dir, mut store) = library(LibraryMode::Personal)?;
    let origin = "origin".parse()?;
    let key = "secret-weather".parse()?;
    let input = source(&["owner"])?;
    let saved = store.ingest_source(&origin, &key, &input)?;
    let chunk = store.search_chunks("下雨", &"owner".parse()?)?.remove(0);
    assert_eq!(
        store.db.query_row(
            "SELECT dims FROM chunk_vectors WHERE chunk_id=?1",
            [&chunk.chunk_id],
            |row| row.get::<_, usize>(0)
        )?,
        256
    );
    store.purge(&origin, &key, AT, &["https://community.example".into()])?;
    assert!(store.search_chunks("下雨", &"owner".parse()?)?.is_empty());
    assert_eq!(
        store.db.query_row(
            "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH '下雨'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    assert_eq!(
        store
            .db
            .query_row("SELECT count(*) FROM chunk_vectors", [], |r| r
                .get::<_, i64>(0))?,
        0
    );
    assert_eq!(store.status()?.sources, 0);
    assert!(store.raw_source_bytes(&saved.source_id)?.is_none());
    assert!(matches!(
        store.ingest_source(&origin, &key, &input),
        Err(StoreError::Tombstoned(_))
    ));
    drop(store);
    let mut reopened = Store::open(dir.path())?;
    assert!(matches!(
        reopened.ingest_source(&origin, &key, &input),
        Err(StoreError::Tombstoned(_))
    ));
    reopened.purge(&origin, &key, AT, &["https://community.example".into()])?;
    assert_eq!(
        reopened
            .db
            .query_row("SELECT count(*) FROM outbox", [], |r| r.get::<_, i64>(0))?,
        1
    );
    let payload: String = reopened
        .db
        .query_row("SELECT payload_json FROM outbox", [], |r| r.get(0))?;
    assert!(!payload.contains("secret-weather"));
    Ok(())
}

#[test]
fn hidden_sources_do_not_change_visible_search_bytes() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Community)?;
    store.add_member("bob".parse()?, kp_model::MemberRole::Member, AT)?;
    let before = serde_json::to_vec(&store.search_chunks("secret", &"bob".parse()?)?)?;
    for i in 0..8 {
        let mut hidden = source(&["owner"])?;
        hidden.content = format!("secret hidden token {i}").into_bytes();
        hidden.chunks[0].body = format!("secret hidden token {i}");
        store.ingest_source(&"origin".parse()?, &format!("hidden-{i}").parse()?, &hidden)?;
    }
    let after = serde_json::to_vec(&store.search_chunks("secret", &"bob".parse()?)?)?;
    assert_eq!(before, after);
    Ok(())
}
