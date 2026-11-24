use kp_model::{FactId, LibraryMode};
use proptest::prelude::*;
use proptest::test_runner::RngSeed;

use super::*;
use crate::test_support::{derived, fact, justification, library, source, AT};

#[test]
fn purge_rejects_dead_or_mismatched_alternative_proofs() -> Result<()> {
    for mutation in [
        "UPDATE facts SET status='withdrawn' WHERE fact_id='alternative'",
        "UPDATE facts SET expired_at=1 WHERE fact_id='alternative'",
        "UPDATE justification_premises SET premise_sha256=printf('%064d',0) WHERE just_id='d2'",
        "UPDATE justification_premises SET premise_origin_lib='wrong' WHERE just_id='d2'",
        "UPDATE justification_premises SET premise_logical_key='wrong' WHERE just_id='d2'",
        "UPDATE justifications SET active=0 WHERE just_id='d2'",
        "UPDATE justifications SET invalidated_at=1 WHERE just_id='d2'",
        "UPDATE facts SET status='withdrawn' WHERE fact_id='conclusion'",
        "UPDATE facts SET expired_at=1 WHERE fact_id='conclusion'",
    ] {
        let (_dir, mut store) = library(LibraryMode::Personal)?;
        let secret = fact("secret", &["owner"])?;
        let alternative = fact("alternative", &["owner"])?;
        let conclusion = derived("conclusion")?;
        let descendant = derived("descendant")?;
        for record in [&secret, &alternative, &conclusion, &descendant] {
            store.insert_fact(record)?;
        }
        store.insert_justification(&justification("d1", &conclusion, &[&secret])?)?;
        store.insert_justification(&justification("d2", &conclusion, &[&alternative])?)?;
        store.insert_justification(&justification("d3", &descendant, &[&conclusion])?)?;
        store.db.execute(mutation, [])?;

        store.purge(&secret.origin_library, &secret.logical_key, AT, &[])?;

        assert_eq!(store.status()?.facts, 1, "{mutation}");
        for record in [&secret, &conclusion, &descendant] {
            assert!(
                store.is_tombstoned(&record.origin_library, &record.logical_key)?,
                "{mutation}"
            );
        }
        assert!(!store.is_tombstoned(&alternative.origin_library, &alternative.logical_key)?);
    }
    Ok(())
}

#[test]
fn purge_scopes_shared_keys_to_their_origin_in_facts_logs_and_outbox() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let mut removed = fact("removed", &["owner"])?;
    removed.logical_key = "shared_key".parse()?;
    removed.origin_library = "origin_A".parse()?;
    let mut kept = fact("kept", &["owner"])?;
    kept.logical_key = removed.logical_key.clone();
    kept.origin_library = "origin_B".parse()?;
    for record in [&removed, &kept] {
        store.insert_fact(record)?;
        store.ingest_source(
            &record.origin_library,
            &record.logical_key,
            &source(&["owner"])?,
        )?;
        // Key-only mutations must be scoped even when no target or fact ID is supplied.
        store.db.execute(
            "INSERT INTO change_log (op,origin_library_id,logical_key,payload_json,created_at)
             VALUES ('update',?1,?2,'unchanged',?3)",
            params![
                record.origin_library.as_str(),
                record.logical_key.as_str(),
                AT.0
            ],
        )?;
        for kind in ["publish", "withdraw"] {
            let payload = serde_json::to_string(&serde_json::json!({
                "origin_library_id":record.origin_library, "logical_key":record.logical_key
            }))?;
            store.db.execute(
                "INSERT INTO outbox (dest_community_url,event_kind,idempotency_key,payload_json)
                 VALUES ('https://community.example',?1,?2,?3)",
                params![kind, format!("{}-{kind}", record.origin_library), payload],
            )?;
        }
    }
    let snapshot = |store: &Store| -> Result<(String, String)> {
        let logs = store.db.query_row(
            "SELECT json_group_array(json_array(seq,op,target_id,origin_library_id,logical_key,
                content_sha256,actor_id,payload_json,created_at)) FROM
             (SELECT * FROM change_log WHERE origin_library_id='origin_B' ORDER BY seq)",
            [],
            |r| r.get(0),
        )?;
        let outbox = store.db.query_row(
            "SELECT json_group_array(json_array(outbox_id,dest_community_url,event_kind,
                idempotency_key,payload_json,delivered_at,retry_count)) FROM
             (SELECT * FROM outbox WHERE json_extract(payload_json,'$.origin_library_id')='origin_B'
              ORDER BY outbox_id)",
            [],
            |r| r.get(0),
        )?;
        Ok((logs, outbox))
    };
    assert_eq!(
        store.db.query_row(
            "SELECT count(*) FROM change_log WHERE origin_library_id='origin_B'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        3
    );
    let before = snapshot(&store)?;
    store.purge(&removed.origin_library, &removed.logical_key, AT, &[])?;
    assert_eq!(snapshot(&store)?, before);
    assert_eq!(store.visible_facts(&"owner".parse()?, AT)?, vec![kept]);
    assert_eq!(store.status()?.sources, 1);
    assert_eq!(
        store.db.query_row(
            "SELECT count(*) FROM change_log WHERE origin_library_id='origin_A'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    assert_eq!(
        store.db.query_row(
            "SELECT count(*) FROM outbox WHERE event_kind != 'purge' AND json_extract(payload_json,'$.origin_library_id')='origin_A'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    assert_eq!(
        store.db.query_row(
            "SELECT count(*) FROM outbox WHERE event_kind = 'purge' AND json_extract(payload_json,'$.origin_library_id')='origin_A' AND json_extract(payload_json,'$.logical_key') IS NULL",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        1
    );
    Ok(())
}

#[test]
fn cascaded_tombstones_queue_opaque_idempotent_purge_events() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let secret = fact("secret", &["owner"])?;
    let conclusion = derived("conclusion")?;
    let descendant = derived("descendant")?;
    let records = [&secret, &conclusion, &descendant];
    for record in records {
        store.insert_fact(record)?;
    }
    store.insert_justification(&justification("d1", &conclusion, &[&secret])?)?;
    store.insert_justification(&justification("d2", &descendant, &[&conclusion])?)?;
    let destinations = vec!["https://one.example".into(), "https://two.example".into()];
    store.purge(
        &secret.origin_library,
        &secret.logical_key,
        AT,
        &destinations,
    )?;
    store.purge(
        &secret.origin_library,
        &secret.logical_key,
        EpochMs(AT.0 + 1),
        &destinations,
    )?;

    assert_eq!(
        store
            .db
            .query_row("SELECT count(*) FROM outbox", [], |r| r.get::<_, i64>(0))?,
        6
    );
    for record in records {
        let id = store.tombstone_id(&record.origin_library, &record.logical_key)?;
        for destination in &destinations {
            let dedup = ContentHash::digest(serde_json::to_vec(&(destination, id.as_str()))?);
            let payload: String = store.db.query_row(
                "SELECT payload_json FROM outbox WHERE event_kind='purge' AND idempotency_key=?1",
                [dedup.as_str()],
                |r| r.get(0),
            )?;
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&payload)?,
                serde_json::json!({
                    "tombstone_id":id, "origin_library_id":record.origin_library,
                    "purged_at":AT, "reason":"purged"
                })
            );
        }
    }
    assert_eq!(
        store
            .db
            .query_row("SELECT count(*) FROM change_log", [], |r| r
                .get::<_, i64>(0))?,
        0
    );
    Ok(())
}

#[test]
fn referenced_premise_cannot_be_deleted_to_weaken_a_conjunction() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let a = fact("a", &["owner"])?;
    let b = fact("b", &["owner"])?;
    let conclusion = derived("conclusion")?;
    for record in [&a, &b, &conclusion] {
        store.insert_fact(record)?;
    }
    store.insert_justification(&justification("conjunction", &conclusion, &[&a, &b])?)?;
    let deletion = store
        .db
        .execute("DELETE FROM facts WHERE fact_id=?1", [a.id.as_str()]);
    assert!(
        matches!(deletion, Err(rusqlite::Error::SqliteFailure(error, _))
        if error.code == rusqlite::ErrorCode::ConstraintViolation)
    );
    assert_eq!(
        store
            .db
            .query_row("SELECT count(*) FROM justification_premises", [], |r| r
                .get::<_, i64>(0))?,
        2
    );
    store.purge(&a.origin_library, &a.logical_key, AT, &[])?;
    assert_eq!(store.visible_facts(&"owner".parse()?, AT)?, vec![b]);
    assert_eq!(
        store
            .db
            .query_row("SELECT count(*) FROM justifications", [], |r| r
                .get::<_, i64>(0))?,
        0
    );
    Ok(())
}

#[test]
fn tombstone_rejects_fact_replay_even_with_a_new_id() -> Result<()> {
    let (dir, mut store) = library(LibraryMode::Personal)?;
    let mut record = fact("secret", &["owner"])?;
    store.insert_fact(&record)?;
    store.purge(&record.origin_library, &record.logical_key, AT, &[])?;
    assert!(store.is_tombstoned(&record.origin_library, &record.logical_key)?);
    assert!(matches!(
        store.insert_fact(&record),
        Err(StoreError::Tombstoned(_))
    ));
    record.id = FactId::new("new-id")?;
    drop(store);
    assert!(matches!(
        Store::open(dir.path())?.insert_fact(&record),
        Err(StoreError::Tombstoned(_))
    ));
    Ok(())
}

#[test]
fn purge_before_arrival_blocks_out_of_order_import() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Community)?;
    let record = fact("never-arrived", &["owner"])?;
    store.purge(&record.origin_library, &record.logical_key, AT, &[])?;
    assert!(matches!(
        store.insert_fact(&record),
        Err(StoreError::Tombstoned(_))
    ));
    assert!(store.visible_facts(&"owner".parse()?, AT)?.is_empty());
    Ok(())
}

#[test]
fn opaque_tombstones_are_portable_between_library_modes() -> Result<()> {
    let (_personal_dir, personal) = library(LibraryMode::Personal)?;
    let community_dir = tempfile::tempdir()?;
    let mut options = crate::test_support::options(LibraryMode::Community)?;
    options.library_id = "other-local-library".parse()?;
    let community = Store::init(community_dir.path(), options)?;
    let origin = "origin".parse()?;
    let key = "private:logical/key".parse()?;
    assert_eq!(
        personal.tombstone_id(&origin, &key)?,
        community.tombstone_id(&origin, &key)?
    );
    Ok(())
}

#[test]
fn hmac_matches_known_vector() -> Result<()> {
    // Independently computed with Python's hmac/sha256, including the origin:key framing.
    let id = tombstone_id(b"secret", &"origin".parse()?, &"key".parse()?)?;
    assert_eq!(
        id.as_str(),
        "8c2edcc79505acb3c0c5d0395f403fbdb53c5b1eb5b48174ddfe715544ca64d4"
    );
    assert_ne!(id, ContentHash::digest("origin:key"));
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, rng_seed: RngSeed::Fixed(202), ..ProptestConfig::default() })]
    #[test]
    fn opaque_identity_is_stable_and_scoped(key in "[a-z0-9:/.-]{1,80}") {
        let origin = "origin".parse()?;
        let key = key.parse()?;
        let first = tombstone_id(b"local", &origin, &key)?;
        prop_assert_eq!(&first, &tombstone_id(b"local", &origin, &key)?);
        prop_assert_ne!(&first, &tombstone_id(b"other-local", &origin, &key)?);
        prop_assert_ne!(&first, &tombstone_id(b"local", &"other-origin".parse()?, &key)?);
    }
}

#[test]
fn community_fact_purge_preserves_another_members_shared_source() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Community)?;
    let source = store.ingest_source(
        &"origin".parse()?,
        &"shared".parse()?,
        &source(&["owner", "bob"])?,
    )?;
    let mut owner_fact = fact("owner-fact", &["owner", "bob"])?;
    owner_fact.source_anchor = Some(kp_model::AnchorSpec {
        source_id: Some(source.source_id.clone()),
        ..Default::default()
    });
    let mut bob_fact = fact("bob-fact", &["owner", "bob"])?;
    bob_fact.author = "bob".parse()?;
    bob_fact.source_anchor = owner_fact.source_anchor.clone();
    store.insert_fact(&owner_fact)?;
    store.insert_fact(&bob_fact)?;
    store.purge(&bob_fact.origin_library, &bob_fact.logical_key, AT, &[])?;
    assert_eq!(store.status()?.sources, 1);
    assert!(store.derivation_view()?.facts.contains_key(&owner_fact.id));
    assert!(!store.derivation_view()?.facts.contains_key(&bob_fact.id));
    assert!(!store.search_chunks("开车", &"owner".parse()?)?.is_empty());
    Ok(())
}

#[test]
fn community_fact_key_cannot_impersonate_another_members_raw_source() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Community)?;
    let source = store.ingest_source(
        &"origin".parse()?,
        &"shared".parse()?,
        &source(&["owner", "bob"])?,
    )?;
    let mut claim = fact("bob-claim", &["owner", "bob"])?;
    claim.author = "bob".parse()?;
    claim.logical_key = "shared".parse()?;
    claim.source_anchor = Some(kp_model::AnchorSpec {
        source_id: Some(source.source_id),
        ..Default::default()
    });
    assert!(matches!(
        store.insert_fact(&claim),
        Err(StoreError::Conflict)
    ));
    assert_eq!(store.status()?.sources, 1);
    Ok(())
}

#[test]
fn purging_synthesis_premise_erases_text_and_descendants_despite_alternative_support() -> Result<()>
{
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let secret = fact("secret", &["owner"])?;
    let public = fact("public", &["owner"])?;
    let mut synthesis = derived("synthesis")?;
    synthesis.kind = kp_model::FactKind::Synthesis;
    let child = derived("child")?;
    for record in [&secret, &public, &synthesis, &child] {
        store.insert_fact(record)?;
    }
    let mut private_proof = justification("private-proof", &synthesis, &[&secret])?;
    private_proof.mode = kp_model::DerivationMode::ScopedSynthesis;
    let mut public_proof = justification("public-proof", &synthesis, &[&public])?;
    public_proof.mode = kp_model::DerivationMode::ScopedSynthesis;
    store.insert_justification(&private_proof)?;
    store.insert_justification(&public_proof)?;
    store.insert_justification(&justification("child-proof", &child, &[&synthesis])?)?;
    store.purge(&secret.origin_library, &secret.logical_key, AT, &[])?;
    let graph = store.derivation_view()?;
    assert!(!graph.facts.contains_key(&synthesis.id));
    assert!(!graph.facts.contains_key(&child.id));
    assert!(graph.facts.contains_key(&public.id));
    Ok(())
}

#[test]
fn purge_preserves_episodes_and_sources_of_independently_grounded_survivors() -> Result<()> {
    for survives in [true, false] {
        let (dir, mut store) = library(LibraryMode::Personal)?;
        let secret = fact("secret", &["owner"])?;
        let independent = fact("independent", &["owner"])?;
        let conclusion = derived("conclusion")?;
        for record in [&secret, &independent, &conclusion] {
            store.insert_fact(record)?;
        }
        store.insert_justification(&justification("removed-proof", &conclusion, &[&secret])?)?;
        if survives {
            store.insert_justification(&justification(
                "surviving-proof",
                &conclusion,
                &[&independent],
            )?)?;
        }
        let origin = store.library_id().clone();
        let source_key = "episode-source".parse()?;
        let captured = store.ingest_source(&origin, &source_key, &source(&["owner"])?)?;
        let episode = crate::Episode {
            signal_kind: kp_model::ContrastKind::HumanCorrection,
            session_id: "surviving-episode".into(),
            before_trace: serde_json::json!("incorrect answer"),
            after_trace: serde_json::json!("No, use the verified answer."),
            step_ref: "tool:1".into(),
            environment: serde_json::json!({"actor":"owner", "source_id":captured.source_id, "premise_ids":[conclusion.id]}),
        };
        store.record_episodes(std::slice::from_ref(&episode), AT)?;
        let pending = store
            .pending_episodes(&"owner".parse()?)?
            .pop()
            .ok_or(StoreError::NotFound)?;
        store.purge(&secret.origin_library, &secret.logical_key, AT, &[])?;
        assert_eq!(
            store.derivation_view()?.facts.contains_key(&conclusion.id),
            survives
        );
        assert_eq!(
            store.pending_episodes(&"owner".parse()?)?.len(),
            usize::from(survives)
        );
        assert_eq!(
            store.raw_source_bytes(&captured.source_id)?.is_some(),
            survives
        );
        assert!(!dir.path().join("raw").exists());
        assert_eq!(store.status()?.sources, usize::from(survives) as u64);
        assert_eq!(store.is_tombstoned(&origin, &source_key)?, !survives);
        assert_eq!(
            store.is_tombstoned(&origin, &format!("rsi-episode:{}", pending.id).parse()?)?,
            !survives
        );
        if survives {
            assert_eq!(store.record_episodes(&[episode], AT)?, 0);
            assert!(store
                .retrieval_snapshot(&"owner".parse()?, AT)?
                .documents()
                .iter()
                .any(|doc| doc.id.starts_with("chunk-")));
        } else {
            assert!(matches!(
                store.record_episodes(&[episode], AT),
                Err(StoreError::Tombstoned(_))
            ));
        }
    }
    Ok(())
}

#[test]
fn purge_rebuilds_facts_fts_and_chunks_fts_and_vacuums_zero_binary_residue() -> Result<()> {
    use std::fs;
    use std::path::Path;

    fn assert_dir_free_of_canary(dir: &Path, canary: &[u8]) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let p = entry.path();
            if entry.file_type()?.is_dir() {
                assert_dir_free_of_canary(&p, canary)?;
            } else {
                let bytes = fs::read(&p)?;
                assert!(
                    !bytes.windows(canary.len()).any(|w| w == canary),
                    "residual canary bytes found in {p:?}"
                );
            }
        }
        Ok(())
    }

    const FACT_CANARY: &str = "ULTRASECRET_FACT_CANARY_992211";
    const QUOTE_CANARY: &str = "ULTRASECRET_QUOTE_CANARY_884400";

    let (dir, mut store) = library(LibraryMode::Personal)?;
    let owner = "owner".parse()?;
    let origin = store.library_id().clone();

    // Insert enough sibling facts to populate FTS5 segments and vectors alongside the secret fact.
    for idx in 0..40 {
        let mut keep = fact(&format!("fact_keep_{idx}"), &["owner"])?;
        keep.origin_library = origin.clone();
        keep.subject = format!("survivor_subject_{idx}");
        keep.object = serde_json::json!(format!("survivor_fact_token_{idx}"));
        store.insert_fact(&keep)?;
    }

    let mut secret_fact = fact(crate::new_fact_id()?.as_str(), &["owner"])?;
    secret_fact.origin_library = origin.clone();
    secret_fact.subject = FACT_CANARY.into();
    secret_fact.object = serde_json::json!({"summary": FACT_CANARY});
    secret_fact.source_anchor = Some(kp_model::AnchorSpec {
        verbatim_quote: Some(QUOTE_CANARY.into()),
        ..Default::default()
    });
    store.insert_fact(&secret_fact)?;

    // Verify `facts_fts` and `fact_vectors` were populated on insert.
    assert_eq!(
        store.db.query_row(
            "SELECT count(*) FROM facts_fts WHERE facts_fts MATCH ?1",
            [&FACT_CANARY.to_ascii_lowercase()],
            |r| r.get::<_, i64>(0)
        )?,
        1
    );
    assert_eq!(
        store.db.query_row(
            "SELECT count(*) FROM fact_vectors WHERE fact_id = ?1",
            [secret_fact.id.as_str()],
            |r| r.get::<_, i64>(0)
        )?,
        1
    );

    store.purge(
        &secret_fact.origin_library,
        &secret_fact.logical_key,
        AT,
        &[],
    )?;

    // Surviving facts remain indexed in `facts_fts` and retrievable via candidate snapshot.
    assert_eq!(
        store.db.query_row(
            "SELECT count(*) FROM facts_fts WHERE facts_fts MATCH 'survivor_fact_token_25'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        1
    );
    let snap = store.candidate_retrieval_snapshot(&owner, AT, "survivor_fact_token_25", 8)?;
    assert!(snap.documents().iter().any(|d| d.id == "fact-fact_keep_25"));

    // Zero trace of secret canaries in SQLite pages, FTS5 shadow tables, or WAL files on disk.
    for needle in [
        FACT_CANARY.as_bytes(),
        FACT_CANARY.to_ascii_lowercase().as_bytes(),
        QUOTE_CANARY.as_bytes(),
        QUOTE_CANARY.to_ascii_lowercase().as_bytes(),
    ] {
        assert_dir_free_of_canary(dir.path(), needle)?;
    }
    Ok(())
}

#[test]
fn purge_discovers_destinations_from_inflight_outbox_publish_without_publication_receipt(
) -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let dest = "https://community.example/";
    let mut secret = fact("fact_inflight_purge", &["owner"])?;
    secret.origin_library = store.library_id().clone();
    store.insert_fact(&secret)?;

    // Enqueue a pending `'publish'` in `outbox` with ZERO `change_log(op='publication')` receipt,
    // then run `purge(..., &[])`: `purge` must discover `dest` from `outbox`, delete `'publish'`,
    // and emit an opaque `'purge'` tombstone to `dest`!
    store.enqueue(
        dest,
        "publish",
        "idem-first-publish",
        &serde_json::json!({
            "fact": secret,
            "origin_reference": secret.strong_ref(),
        }),
    )?;
    store.purge(&secret.origin_library, &secret.logical_key, AT, &[])?;

    let queued = store.pending_outbox(dest)?;
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].kind, "purge");
    assert!(queued[0].payload.get("tombstone_id").is_some());
    assert!(queued[0].payload.get("logical_key").is_none());
    Ok(())
}
