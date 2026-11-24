use kp_model::{FactStatus, LibraryMode};

use crate::test_support::{fact, library, AT};
use crate::{Result, Store};

#[test]
fn returning_mirror_recovers_after_multiple_omitted_snapshots_and_reopen() -> Result<()> {
    let (_origin_dir, mut origin) = library(LibraryMode::Personal)?;
    let f = fact("remote", &["owner"])?;
    origin.insert_fact(&f)?;
    let full = origin.change_snapshot(&"owner".parse()?, AT)?;
    let (dir, mut mirror) = library(LibraryMode::Personal)?;
    mirror.import_snapshot("endpoint", &full, AT)?;
    mirror.reconcile_fact(&f.id, None, AT)?;
    let mut empty = full.clone();
    empty.facts.clear();
    mirror.import_snapshot("endpoint", &empty, AT)?;
    mirror.import_snapshot("endpoint", &empty, AT)?;
    assert_eq!(
        mirror.derivation_view()?.facts[&f.id].status,
        FactStatus::UnreachableHidden
    );
    drop(mirror);
    let mut mirror = Store::open(dir.path())?;
    mirror.import_snapshot("endpoint", &full, AT)?;
    let recovered = &mirror.derivation_view()?.facts[&f.id];
    assert_eq!(recovered.status, FactStatus::Active);
    assert_eq!(recovered.miss_count, 0);
    assert_eq!(mirror.visible_facts(&"owner".parse()?, AT)?.len(), 1);
    assert!(mirror
        .retrieval_snapshot(&"owner".parse()?, AT)?
        .documents()
        .iter()
        .any(|d| d.id == "fact-remote"));
    mirror.purge(&f.origin_library, &f.logical_key, AT, &[])?;
    mirror.import_snapshot("endpoint", &full, AT)?;
    assert!(!mirror.derivation_view()?.facts.contains_key(&f.id));
    Ok(())
}

#[test]
fn unchanged_projection_does_not_hide_existing_mirrors() -> Result<()> {
    let (_origin_dir, mut origin) = library(LibraryMode::Personal)?;
    origin.insert_fact(&fact("remote", &["owner"])?)?;
    let full = origin.change_snapshot(&"owner".parse()?, AT)?;
    let (_mirror_dir, mut mirror) = library(LibraryMode::Personal)?;
    mirror.import_snapshot("endpoint", &full, AT)?;
    let unchanged =
        origin.change_snapshot_since(&"owner".parse()?, AT, Some(full.cursor.as_str()))?;
    assert!(unchanged.unchanged);
    assert!(unchanged.facts.is_empty());
    mirror.import_snapshot("endpoint", &unchanged, AT)?;
    assert_eq!(mirror.visible_facts(&"owner".parse()?, AT)?.len(), 1);
    Ok(())
}

#[test]
fn snapshot_with_existing_receipts_succeeds_while_another_connection_holds_write_lock() -> Result<()>
{
    let (dir, mut store) = library(LibraryMode::Community)?;
    store.insert_fact(&fact("visible", &["owner"])?)?;
    let first = store.change_snapshot(&"owner".parse()?, AT)?;
    let mut writer = rusqlite::Connection::open(dir.path().join("kp.sqlite"))?;
    let tx = writer.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    store.db.busy_timeout(std::time::Duration::ZERO)?;
    let second = store.change_snapshot(&"owner".parse()?, AT)?;
    assert_eq!(first.cursor, second.cursor);
    let unchanged =
        store.change_snapshot_since(&"owner".parse()?, AT, Some(first.cursor.as_str()))?;
    assert!(unchanged.unchanged);
    tx.rollback()?;
    Ok(())
}

#[test]
fn origin_binding_survives_purge_and_reopen() -> Result<()> {
    let (dir, mut store) = library(LibraryMode::Community)?;
    let f = fact("registered", &["owner"])?;
    store.insert_fact(&f)?;
    assert!(store.bind_origin_owner(&f.origin_library, &f.author)?);
    store.purge(&f.origin_library, &f.logical_key, AT, &[])?;
    drop(store);
    let mut store = Store::open(dir.path())?;
    assert!(store.owns_origin(&f.origin_library, &f.author)?);
    assert!(!store.bind_origin_owner(&f.origin_library, &"intruder".parse()?)?);
    Ok(())
}

#[test]
fn matching_local_fact_does_not_become_endpoint_managed() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let f = fact("local", &["owner"])?;
    store.insert_fact(&f)?;
    let snapshot = store.change_snapshot(&"owner".parse()?, AT)?;
    store.import_snapshot("endpoint", &snapshot, AT)?;
    let mut empty = snapshot.clone();
    empty.facts.clear();
    store.import_snapshot("endpoint", &empty, AT)?;
    assert_eq!(
        store.derivation_view()?.facts[&f.id].status,
        FactStatus::Active
    );
    Ok(())
}

#[test]
fn governance_snapshot_syncs_concepts_membership_scopes_and_preserves_outsider_noninterference(
) -> Result<()> {
    use kp_model::{
        AudienceSpec, BroaderScopeCandidate, CommunityConcept, ConceptKind, ConceptPolarity,
        FactKind, GovernedScope, MemberRole, MembershipRelation, SourceScope,
    };

    let (_cdir, mut community) = library(LibraryMode::Community)?;
    community.add_member("alice".parse()?, MemberRole::Member, AT)?;
    community.add_member("bob".parse()?, MemberRole::Member, AT)?;
    community.add_member("carol".parse()?, MemberRole::Member, AT)?;

    let public_fact = fact("public-seed", &["alice", "bob", "carol"])?;
    community.insert_fact(&public_fact)?;

    let carol_before =
        community.change_snapshot_with_governance_since(&"carol".parse()?, AT, None)?;

    let gym_concept = CommunityConcept {
        id: "cpt_gym_club".parse()?,
        kind: ConceptKind::Group,
        canonical_label: "训练组".into(),
        domain: "fitness".into(),
        polarity: ConceptPolarity::Neutral,
        proposed_by: "bob".parse()?,
        created_at: AT,
    };
    let sprain_concept = CommunityConcept {
        id: "cpt_ankle_sprain".parse()?,
        kind: ConceptKind::Condition,
        canonical_label: "脚踝扭伤".into(),
        domain: "fitness".into(),
        polarity: ConceptPolarity::Positive,
        proposed_by: "bob".parse()?,
        created_at: AT,
    };
    community.exchange_upsert_concept(&gym_concept, true)?;
    community.exchange_upsert_concept(&sprain_concept, true)?;

    let scope_ab = SourceScope {
        scope_id: "scope_chat_ab".parse()?,
        provider: "ext_chat".into(),
        external_container_id: "oc_ab".into(),
        label: "训练打卡协作讨论组".into(),
        current_audience: AudienceSpec::Explicit(["alice".parse()?, "bob".parse()?].into()),
        synced_at: AT,
    };
    community.upsert_source_scope(&scope_ab)?;

    let mut membership = fact("fact_mem_ab", &["alice", "bob"])?;
    membership.kind = FactKind::Membership;
    membership.author = "bob".parse()?;
    membership.object = serde_json::to_value(MembershipRelation {
        anchor: "bob".parse()?,
        member: "alice".parse()?,
        group_concept_id: "cpt_gym_club".parse()?,
    })?;
    community.insert_fact(&membership)?;

    let mut constraint = fact("fact_sprain_rule", &["alice", "bob"])?;
    constraint.kind = FactKind::Constraint;
    constraint.author = "alice".parse()?;
    constraint.ingested_by = Some("bob".parse()?);
    constraint.source_scope_id = Some("scope_chat_ab".parse()?);
    constraint.governed_scope = Some(GovernedScope::SelfOnly {
        actor: "alice".parse()?,
    });
    constraint.broader_scope = Some(BroaderScopeCandidate {
        narrow_active: GovernedScope::SelfOnly {
            actor: "alice".parse()?,
        },
        broader_candidate: GovernedScope::RelationGroup {
            anchor: "bob".parse()?,
            group_concept_id: "cpt_gym_club".parse()?,
        },
        question: "只管你自己，还是整个训练组？".into(),
    });
    constraint.object = serde_json::json!({
        "rule": "扭伤恢复期不做负重深蹲",
        "condition_concept_id": "cpt_ankle_sprain"
    });
    community.insert_fact(&constraint)?;
    community.exchange_link_fact_concepts(&constraint.id, &["cpt_ankle_sprain".parse()?], AT)?;

    // Outsider Carol sees zero trace of {Alice, Bob} facts, concepts, or source_scopes.
    let carol_after =
        community.change_snapshot_with_governance_since(&"carol".parse()?, AT, None)?;
    assert_eq!(carol_before.0.cursor, carol_after.0.cursor);
    assert_eq!(carol_after.0.facts.len(), 1);
    assert!(carol_after.1.is_empty());
    assert!(carol_after.2.is_empty());
    let carol_unchanged = community.change_snapshot_with_governance_since(
        &"carol".parse()?,
        AT,
        Some(carol_before.0.cursor.as_str()),
    )?;
    assert!(carol_unchanged.0.unchanged);

    // Alice mirrors the governance projection into her personal library.
    let (alice_snap, alice_concepts, alice_scopes) =
        community.change_snapshot_with_governance_since(&"alice".parse()?, AT, None)?;
    assert_eq!(alice_snap.facts.len(), 3);
    assert_eq!(alice_concepts.len(), 2);
    assert_eq!(alice_scopes.len(), 1);

    let (_mdir, mut alice_mirror) = crate::InitOptions {
        library_id: "alice-personal".parse()?,
        name: "Alice".into(),
        mode: LibraryMode::Personal,
        owner: "alice".parse()?,
        created_at: AT,
    }
    .pipe_init()?;

    alice_mirror.import_snapshot_with_governance(
        "https://community.example",
        &alice_snap,
        &alice_concepts,
        &alice_scopes,
        None,
        AT,
    )?;

    assert_eq!(alice_mirror.exchange_active_concepts()?.len(), 2);
    assert!(alice_mirror
        .source_scope(&"scope_chat_ab".parse()?)?
        .is_some());
    let visible = alice_mirror.visible_facts(&"alice".parse()?, AT)?;
    assert_eq!(visible.len(), 3);
    let imported_constraint = visible
        .iter()
        .find(|f| f.id == constraint.id)
        .ok_or(crate::StoreError::NotFound)?;
    assert_eq!(imported_constraint.ingested_by, Some("bob".parse()?));
    assert_eq!(
        imported_constraint.source_scope_id,
        Some("scope_chat_ab".parse()?)
    );
    assert_eq!(
        imported_constraint.governed_scope,
        constraint.governed_scope
    );
    assert_eq!(imported_constraint.broader_scope, constraint.broader_scope);
    assert!(visible.iter().any(|f| f.kind == FactKind::Membership));
    Ok(())
}

trait PipeInit {
    fn pipe_init(self) -> Result<(tempfile::TempDir, Store)>;
}

impl PipeInit for crate::InitOptions {
    fn pipe_init(self) -> Result<(tempfile::TempDir, Store)> {
        let dir = tempfile::tempdir()?;
        let store = Store::init(dir.path(), self)?;
        Ok((dir, store))
    }
}

#[test]
fn reconcile_origin_fact_never_invalidates_on_ten_consecutive_network_misses_and_recovers(
) -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let premise = fact("remote_premise", &["owner"])?;
    let conclusion = crate::test_support::derived("derived_warning")?;
    let proof = crate::test_support::justification("j1", &conclusion, &[&premise])?;
    store.insert_fact(&premise)?;
    store.save_derivations(std::slice::from_ref(&conclusion), &[proof])?;
    assert_eq!(store.visible_facts(&"owner".parse()?, AT)?.len(), 2);

    // 10 consecutive transient network misses (including 2, 3, and 10) stay UnreachableHidden.
    for expected in 1..=10 {
        store.reconcile_origin_fact(&premise.id, None, AT)?;
        let graph = store.derivation_view()?;
        assert_eq!(
            graph.facts[&premise.id].status,
            FactStatus::UnreachableHidden
        );
        assert_eq!(graph.facts[&premise.id].miss_count, expected);
        // Dependent derived conclusion stays structurally intact (Active), hidden only at read time.
        assert_eq!(graph.facts[&conclusion.id].status, FactStatus::Active);
        assert!(graph.justifications[&"j1".parse()?].active);
        assert!(store.visible_facts(&"owner".parse()?, AT)?.is_empty());
    }

    // Origin comes back online -> immediate full recovery of both premise and derived conclusion.
    store.reconcile_origin_fact(&premise.id, Some(true), AT)?;
    let graph = store.derivation_view()?;
    assert_eq!(graph.facts[&premise.id].status, FactStatus::Active);
    assert_eq!(graph.facts[&premise.id].miss_count, 0);
    assert_eq!(store.visible_facts(&"owner".parse()?, AT)?.len(), 2);

    // Explicit origin withdrawal (`Some(false)`) permanently invalidates premise and cascades.
    store.reconcile_origin_fact(&premise.id, Some(false), AT)?;
    let graph = store.derivation_view()?;
    assert_eq!(graph.facts[&premise.id].status, FactStatus::Invalidated);
    assert_eq!(graph.facts[&conclusion.id].status, FactStatus::Invalidated);
    assert!(!graph.justifications[&"j1".parse()?].active);
    Ok(())
}

#[test]
fn daemon_session_cursor_handles_incremental_advances_and_prefix_rewrite() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let (last_ext, queued) = store.daemon_record_session_cursor(&crate::SessionCursorUpdate {
        session_id: "sess_1",
        client_kind: "claude-code",
        transcript_path: Some("/tmp/s1.jsonl"),
        byte_offset: 400,
        line_count: 5,
        prefix_sha256: "hash_v1",
        prev_byte_offset: None,
        prev_prefix_sha256: None,
        actor: None,
        session_ended: false,
        at: AT,
    })?;
    assert_eq!(last_ext, 0);
    assert!(queued);
    assert_eq!(store.daemon_queue_stats()?, (1, 1));

    let more = store.daemon_mark_session_extracted("sess_1", "hash_v1", 400)?;
    assert!(!more);
    assert_eq!(store.daemon_queue_stats()?, (1, 0));

    // Appending bytes keeps previous extracted offset.
    let (last_ext2, queued2) = store.daemon_record_session_cursor(&crate::SessionCursorUpdate {
        session_id: "sess_1",
        client_kind: "claude-code",
        transcript_path: None,
        byte_offset: 900,
        line_count: 11,
        prefix_sha256: "hash_v1",
        prev_byte_offset: None,
        prev_prefix_sha256: None,
        actor: None,
        session_ended: false,
        at: AT,
    })?;
    assert_eq!(last_ext2, 400);
    assert!(queued2);

    // Rewriting transcript (prefix_sha256 changed) resets last_extracted_offset to 0.
    let (last_ext3, queued3) = store.daemon_record_session_cursor(&crate::SessionCursorUpdate {
        session_id: "sess_1",
        client_kind: "claude-code",
        transcript_path: None,
        byte_offset: 250,
        line_count: 3,
        prefix_sha256: "hash_rewritten",
        prev_byte_offset: None,
        prev_prefix_sha256: None,
        actor: None,
        session_ended: false,
        at: AT,
    })?;
    assert_eq!(last_ext3, 0);
    assert!(queued3);
    Ok(())
}

#[test]
fn visible_chunk_count_matches_retrieval_snapshot_across_source_scope_narrowing() -> Result<()> {
    use kp_model::{AudienceSpec, MemberRole, SourceScope};

    let (_dir, mut store) = library(LibraryMode::Community)?;
    store.add_member("alice".parse()?, MemberRole::Member, AT)?;
    store.add_member("bob".parse()?, MemberRole::Member, AT)?;
    store.add_member("carol".parse()?, MemberRole::Member, AT)?;

    let scope = SourceScope {
        scope_id: "scope_doc".parse()?,
        provider: "ext_chat".into(),
        external_container_id: "doc_1".into(),
        label: "Doc".into(),
        current_audience: AudienceSpec::Explicit(["alice".parse()?, "bob".parse()?].into()),
        synced_at: AT,
    };
    store.upsert_source_scope(&scope)?;
    let lib = store.library_id().clone();
    let stored = store.ingest_source(
        &lib,
        &"k1".parse()?,
        &crate::test_support::source(&["alice", "bob"])?,
    )?;
    store.db.execute(
        "UPDATE raw_sources SET source_scope_id = 'scope_doc' WHERE source_id = ?1",
        [stored.source_id.as_str()],
    )?;

    assert_eq!(store.exchange_visible_chunk_count(&"alice".parse()?)?, 1);
    assert_eq!(store.exchange_visible_chunk_count(&"bob".parse()?)?, 1);
    assert_eq!(store.exchange_visible_chunk_count(&"carol".parse()?)?, 0);

    // Narrow live SourceScope to Alice -> Bob immediately sees 0 chunks.
    store.upsert_source_scope(&SourceScope {
        current_audience: AudienceSpec::Explicit(["alice".parse()?].into()),
        synced_at: kp_model::EpochMs(AT.0 + 1),
        ..scope
    })?;
    assert_eq!(store.exchange_visible_chunk_count(&"alice".parse()?)?, 1);
    assert_eq!(store.exchange_visible_chunk_count(&"bob".parse()?)?, 0);
    Ok(())
}

#[test]
fn daemon_session_cursor_scan_and_retry_state_persist_across_restarts() -> Result<()> {
    let (dir, mut store) = library(LibraryMode::Personal)?;
    let owner: kp_model::ActorId = "owner".parse()?;
    store.daemon_record_session_cursor(&crate::SessionCursorUpdate {
        session_id: "sess_retry",
        client_kind: "codex",
        transcript_path: Some("/tmp/rollout.jsonl"),
        byte_offset: 800,
        line_count: 12,
        prefix_sha256: "p1",
        prev_byte_offset: None,
        prev_prefix_sha256: None,
        actor: Some(&owner),
        session_ended: true,
        at: AT,
    })?;
    let (st1, att1) = store.daemon_record_session_extraction_failure(
        "sess_retry",
        false,
        Some("transient timeout"),
        AT,
    )?;
    assert_eq!(st1, crate::ExtractionJobStatus::Pending);
    assert_eq!(att1, 1);

    let (st2, att2) = store.daemon_record_session_extraction_failure(
        "sess_retry",
        true,
        Some("429 quota exhausted"),
        AT,
    )?;
    assert_eq!(st2, crate::ExtractionJobStatus::PausedQuota);
    assert_eq!(att2, 2);

    drop(store);
    let mut reopened = Store::open(dir.path())?;
    let scanned = reopened.daemon_unextracted_session_cursors()?;
    assert_eq!(scanned.len(), 1);
    assert_eq!(scanned[0].cursor.session_id, "sess_retry");
    assert_eq!(scanned[0].cursor.last_byte_offset, 800);
    assert_eq!(scanned[0].cursor.last_extracted_offset, 0);
    assert_eq!(scanned[0].actor, owner);
    assert!(scanned[0].session_ended);
    assert_eq!(scanned[0].status, crate::ExtractionJobStatus::PausedQuota);
    assert_eq!(scanned[0].attempts, 2);

    assert!(!reopened.daemon_mark_session_extracted("sess_retry", "p1", 800)?);
    assert!(reopened.daemon_unextracted_session_cursors()?.is_empty());
    assert_eq!(
        reopened.daemon_session_cursor_retry_state("sess_retry")?,
        Some((crate::ExtractionJobStatus::Done, 0, None))
    );
    Ok(())
}

#[test]
fn daemon_passive_wal_checkpoint_and_sqlite_data_version_track_external_commits() -> Result<()> {
    let (dir, store) = library(LibraryMode::Personal)?;
    store.daemon_passive_wal_checkpoint()?;
    let dv_initial = store.daemon_sqlite_data_version()?;
    assert_eq!(store.daemon_sqlite_data_version()?, dv_initial);

    let mut external_writer = Store::open(dir.path())?;
    external_writer.daemon_record_session_cursor(&crate::SessionCursorUpdate {
        session_id: "sess_ext",
        client_kind: "codex",
        transcript_path: None,
        byte_offset: 512,
        line_count: 4,
        prefix_sha256: "p_ext",
        prev_byte_offset: None,
        prev_prefix_sha256: None,
        actor: None,
        session_ended: false,
        at: AT,
    })?;

    let dv_after_external = store.daemon_sqlite_data_version()?;
    assert_ne!(dv_after_external, dv_initial);
    assert_eq!(store.daemon_sqlite_data_version()?, dv_after_external);
    store.daemon_passive_wal_checkpoint()?;
    Ok(())
}

#[test]
fn real_transcript_append_preserves_extracted_offset_and_detects_in_place_rewrite() -> Result<()> {
    use std::io::Write;

    let (dir, mut store) = library(LibraryMode::Personal)?;
    let transcript = dir.path().join("rollout-real.jsonl");
    let path_str = transcript.to_string_lossy().into_owned();

    let turn1 = b"{\"role\":\"user\",\"content\":\"first turn\"}\n";
    std::fs::write(&transcript, turn1)?;
    let len1 = turn1.len() as u64;
    let h1 = crate::sha256_file_prefix(&transcript, len1)?.ok_or(crate::StoreError::NotFound)?;
    assert!(crate::sha256_file_prefix(&transcript, len1 + 1)?.is_none());

    let (ext1, queued1) = store.daemon_record_session_cursor(&crate::SessionCursorUpdate {
        session_id: "sess_file",
        client_kind: "codex",
        transcript_path: Some(&path_str),
        byte_offset: len1,
        line_count: 1,
        prefix_sha256: &h1,
        prev_byte_offset: None,
        prev_prefix_sha256: None,
        actor: None,
        session_ended: false,
        at: AT,
    })?;
    assert_eq!(ext1, 0);
    assert!(queued1);
    assert!(!store.daemon_mark_session_extracted("sess_file", &h1, len1)?);

    // Append Turn 2 on disk -> full-file prefix hash changes (`h2 != h1`), but `0..len1` matches `h1`,
    // so `last_extracted_offset` stays `len1`!
    let turn2 = b"{\"role\":\"user\",\"content\":\"second turn\"}\n";
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(&transcript)?;
        f.write_all(turn2)?;
    }
    let len2 = len1 + turn2.len() as u64;
    let h2 = crate::sha256_file_prefix(&transcript, len2)?.ok_or(crate::StoreError::NotFound)?;
    assert_ne!(h1, h2);

    let (ext2, queued2) = store.daemon_record_session_cursor(&crate::SessionCursorUpdate {
        session_id: "sess_file",
        client_kind: "codex",
        transcript_path: Some(&path_str),
        byte_offset: len2,
        line_count: 2,
        prefix_sha256: &h2,
        prev_byte_offset: None,
        prev_prefix_sha256: None,
        actor: None,
        session_ended: false,
        at: AT,
    })?;
    assert_eq!(ext2, len1);
    assert!(queued2);

    // Concurrent Turn 3 append BEFORE Turn 2 finishes extracting:
    let turn3 = b"{\"role\":\"user\",\"content\":\"third turn\"}\n";
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(&transcript)?;
        f.write_all(turn3)?;
    }
    let len3 = len2 + turn3.len() as u64;
    let h3 = crate::sha256_file_prefix(&transcript, len3)?.ok_or(crate::StoreError::NotFound)?;
    let (ext3, queued3) = store.daemon_record_session_cursor(&crate::SessionCursorUpdate {
        session_id: "sess_file",
        client_kind: "codex",
        transcript_path: Some(&path_str),
        byte_offset: len3,
        line_count: 3,
        prefix_sha256: &h3,
        prev_byte_offset: Some(len2),
        prev_prefix_sha256: Some(&h2),
        actor: None,
        session_ended: false,
        at: AT,
    })?;
    assert_eq!(ext3, len1);
    assert!(queued3);

    // Turn 2 finishes with `(h2, len2)` even though cursor is already at `(h3, len3)` -> advances to `len2`!
    let more = store.daemon_mark_session_extracted("sess_file", &h2, len2)?;
    assert!(more);
    assert_eq!(
        store
            .get_session_cursor("sess_file")?
            .map(|c| c.last_extracted_offset),
        Some(len2)
    );

    // Rewrite file in-place (corrupting prefix `0..len2`) -> `daemon_mark_session_extracted` refuses to advance,
    // and `daemon_record_session_cursor` resets `last_extracted_offset` to `0`!
    let rewritten = vec![b'X'; len3 as usize];
    std::fs::write(&transcript, &rewritten)?;
    let h_rewritten =
        crate::sha256_file_prefix(&transcript, len3)?.ok_or(crate::StoreError::NotFound)?;
    assert!(store.daemon_mark_session_extracted("sess_file", &h3, len3)?);
    assert_eq!(
        store
            .get_session_cursor("sess_file")?
            .map(|c| c.last_extracted_offset),
        Some(len2)
    );
    let (ext_reset, _) = store.daemon_record_session_cursor(&crate::SessionCursorUpdate {
        session_id: "sess_file",
        client_kind: "codex",
        transcript_path: Some(&path_str),
        byte_offset: len3,
        line_count: 1,
        prefix_sha256: &h_rewritten,
        prev_byte_offset: None,
        prev_prefix_sha256: None,
        actor: None,
        session_ended: false,
        at: AT,
    })?;
    assert_eq!(ext_reset, 0);
    Ok(())
}

#[test]
fn audience_revisions_reject_stale_packets_sync_shrunk_mirrors_and_reconcile_relation_groups(
) -> Result<()> {
    use kp_model::{
        AudienceSpec, CommunityConcept, ConceptKind, ConceptPolarity, FactKind, GovernedScope,
        MemberRole, MembershipRelation, SourceScope,
    };

    let dest = "https://community.example/";
    let (_adir, mut alice_personal) = crate::InitOptions {
        library_id: "alice-lib".parse()?,
        name: "Alice".into(),
        mode: LibraryMode::Personal,
        owner: "alice".parse()?,
        created_at: AT,
    }
    .pipe_init()?;
    alice_personal.register_peer_members(&["bob".parse()?, "dave".parse()?], AT)?;

    let team_cpt = CommunityConcept {
        id: "cpt_team_group".parse()?,
        kind: ConceptKind::Group,
        canonical_label: "研发组".into(),
        domain: "work".into(),
        polarity: ConceptPolarity::Neutral,
        proposed_by: "alice".parse()?,
        created_at: AT,
    };
    alice_personal.upsert_community_concept(&team_cpt)?;

    // Self-only unpublished fact never appears in `unshared_multi_actor_facts`.
    let mut private_note = fact("fact_self_only", &["alice"])?;
    private_note.origin_library = "alice-lib".parse()?;
    private_note.author = "alice".parse()?;
    alice_personal.insert_fact(&private_note)?;
    assert!(alice_personal.unshared_multi_actor_facts(dest)?.is_empty());

    // Membership for Bob + RelationGroup constraint initially visible to `{alice, bob}`.
    let mut mem_bob = fact("fact_mem_bob", &["alice", "bob"])?;
    mem_bob.origin_library = "alice-lib".parse()?;
    mem_bob.author = "alice".parse()?;
    mem_bob.kind = FactKind::Membership;
    mem_bob.object = serde_json::to_value(MembershipRelation {
        anchor: "alice".parse()?,
        member: "bob".parse()?,
        group_concept_id: team_cpt.id.clone(),
    })?;
    alice_personal.insert_fact(&mem_bob)?;

    let mut rule = fact("fact_team_rule", &["alice", "bob"])?;
    rule.origin_library = "alice-lib".parse()?;
    rule.author = "alice".parse()?;
    rule.kind = FactKind::Constraint;
    rule.governed_scope = Some(GovernedScope::RelationGroup {
        anchor: "alice".parse()?,
        group_concept_id: team_cpt.id.clone(),
    });
    alice_personal.insert_fact(&rule)?;

    assert_eq!(
        alice_personal.next_publication_revision(dest, &rule.origin_library, &rule.logical_key)?,
        1
    );
    assert!(alice_personal
        .unshared_multi_actor_facts(dest)?
        .contains(&rule.id));

    let aud_ab = AudienceSpec::Explicit(["alice".parse()?, "bob".parse()?].into());
    alice_personal.record_publication_with_audience(
        dest,
        &rule.strong_ref(),
        "published-rule",
        &aud_ab,
        1,
        AT,
    )?;
    assert_eq!(
        alice_personal.next_publication_revision(dest, &rule.origin_library, &rule.logical_key)?,
        2
    );
    assert!(!alice_personal
        .unshared_multi_actor_facts(dest)?
        .contains(&rule.id));

    // Dave joins `cpt_team_group` -> `reconcile_relation_group_audiences` widens audience to `{alice, bob, dave}`.
    let mut mem_dave = fact("fact_mem_dave", &["alice", "dave"])?;
    mem_dave.origin_library = "alice-lib".parse()?;
    mem_dave.author = "alice".parse()?;
    mem_dave.kind = FactKind::Membership;
    mem_dave.object = serde_json::to_value(MembershipRelation {
        anchor: "alice".parse()?,
        member: "dave".parse()?,
        group_concept_id: team_cpt.id.clone(),
    })?;
    alice_personal.insert_fact(&mem_dave)?;
    assert_eq!(
        alice_personal.reconcile_relation_group_audiences(|a, b| a == b, AT)?,
        1
    );
    assert!(alice_personal
        .unshared_multi_actor_facts(dest)?
        .contains(&rule.id));

    // Forget both memberships -> `reconcile_relation_group_audiences` shrinks audience down to `Explicit({alice})`,
    // and `unshared_multi_actor_facts` STILL returns `rule.id` so the shrink is pushed to `dest`!
    alice_personal.forget_fact_id(&mem_bob.id, &"alice".parse()?, AT)?;
    alice_personal.forget_fact_id(&mem_dave.id, &"alice".parse()?, AT)?;
    assert_eq!(
        alice_personal.reconcile_relation_group_audiences(|a, b| a == b, AT)?,
        1
    );
    assert!(alice_personal
        .unshared_multi_actor_facts(dest)?
        .contains(&rule.id));

    // Verify `exchange_replace_undelivered_publish` replaces older undelivered publish for same identity.
    alice_personal.exchange_replace_undelivered_publish(
        dest,
        &rule.origin_library,
        &rule.logical_key,
        "idem-rev2",
        &serde_json::json!({
            "origin_reference": rule.strong_ref(),
            "revision": 2,
        }),
    )?;
    alice_personal.exchange_replace_undelivered_publish(
        dest,
        &rule.origin_library,
        &rule.logical_key,
        "idem-rev3",
        &serde_json::json!({
            "origin_reference": rule.strong_ref(),
            "revision": 3,
        }),
    )?;
    let pending = alice_personal.pending_outbox(dest)?;
    let rule_publishes: Vec<_> = pending.iter().filter(|e| e.kind == "publish").collect();
    assert_eq!(rule_publishes.len(), 1);
    assert_eq!(rule_publishes[0].key, "idem-rev3");

    // Verify SourceScope ceiling bounds `reconcile_relation_group_audiences`:
    alice_personal.upsert_source_scope(&SourceScope {
        scope_id: "scope_ab_only".parse()?,
        provider: "container".into(),
        external_container_id: "c_ab".into(),
        label: "AB".into(),
        current_audience: aud_ab.clone(),
        synced_at: AT,
    })?;
    let mut mem_dave2 = mem_dave.clone();
    mem_dave2.id = "fact_mem_dave2".parse()?;
    mem_dave2.logical_key = "fact_mem_dave2".parse()?;
    alice_personal.insert_fact(&mem_dave2)?;
    alice_personal.db.execute(
        "UPDATE facts SET source_scope_id='scope_ab_only' WHERE fact_id=?1",
        [rule.id.as_str()],
    )?;
    alice_personal.reconcile_relation_group_audiences(|a, b| a == b, AT)?;
    assert!(alice_personal
        .visible_facts(&"dave".parse()?, AT)?
        .iter()
        .all(|f| f.id != rule.id));

    // Community server `exchange_revise_published_audience` + subscriber `import_snapshot_with_governance`:
    let (_cdir, mut community) = library(LibraryMode::Community)?;
    community.add_member("alice".parse()?, MemberRole::Member, AT)?;
    community.add_member("bob".parse()?, MemberRole::Member, AT)?;
    community.add_member("dave".parse()?, MemberRole::Member, AT)?;

    let mut pub_fact = fact("published-rule", &["alice", "bob"])?;
    pub_fact.author = "alice".parse()?;
    community.insert_fact(&pub_fact)?;

    let (_bdir, mut bob_mirror) = crate::InitOptions {
        library_id: "bob-lib".parse()?,
        name: "Bob".into(),
        mode: LibraryMode::Personal,
        owner: "bob".parse()?,
        created_at: AT,
    }
    .pipe_init()?;
    let (bob_snap1, c1, s1) =
        community.change_snapshot_with_governance_since(&"bob".parse()?, AT, None)?;
    assert_eq!(bob_snap1.facts.len(), 1);
    bob_mirror.import_snapshot_with_governance(dest, &bob_snap1, &c1, &s1, None, AT)?;
    let local_warn = crate::test_support::derived("fact_bob_local_warn")?;
    let warn_proof = crate::test_support::justification("j_warn", &local_warn, &[&pub_fact])?;
    bob_mirror.save_derivations(std::slice::from_ref(&local_warn), &[warn_proof])?;
    assert_eq!(bob_mirror.visible_facts(&"bob".parse()?, AT)?.len(), 2);

    // Widen to `{alice, bob, dave}` at revision 2 -> Dave sees it; stale revision 1 is rejected!
    let aud_abd =
        AudienceSpec::Explicit(["alice".parse()?, "bob".parse()?, "dave".parse()?].into());
    assert!(community.exchange_revise_published_audience(&pub_fact.id, &aud_abd, 2, false, AT,)?);
    assert_eq!(community.visible_facts(&"dave".parse()?, AT)?.len(), 1);
    assert!(!community.exchange_revise_published_audience(&pub_fact.id, &aud_ab, 1, false, AT,)?);
    assert_eq!(community.visible_facts(&"dave".parse()?, AT)?.len(), 1);

    // Shrink to `{alice}` at revision 3 -> Bob's snapshot omits `pub_fact` and hides both mirror + derived warning!
    let aud_alice = AudienceSpec::Explicit(["alice".parse()?].into());
    assert!(community.exchange_revise_published_audience(
        &pub_fact.id,
        &aud_alice,
        3,
        false,
        AT,
    )?);
    let (bob_snap2, c2, s2) = community.change_snapshot_with_governance_since(
        &"bob".parse()?,
        AT,
        Some(bob_snap1.cursor.as_str()),
    )?;
    assert!(!bob_snap2.unchanged);
    assert!(bob_snap2.facts.is_empty());
    bob_mirror.import_snapshot_with_governance(dest, &bob_snap2, &c2, &s2, None, AT)?;
    assert!(bob_mirror.visible_facts(&"bob".parse()?, AT)?.is_empty());
    assert_eq!(
        bob_mirror.derivation_view()?.facts[&pub_fact.id].status,
        FactStatus::UnreachableHidden
    );
    Ok(())
}

#[test]
fn retraction_and_supersession_cancel_pending_or_inflight_publishes_and_emit_withdraw_vs_purge_tombstone(
) -> Result<()> {
    use kp_model::{AudienceSpec, Cardinality, ContentHash, Mutability, SlotContract};

    let dest = "https://community.example/";
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let owner: kp_model::ActorId = "owner".parse()?;
    let expected_remote = |f: &kp_model::Fact| -> Result<String> {
        Ok(format!(
            "published-{}",
            ContentHash::digest(serde_json::to_vec(&f.strong_ref())?)
        ))
    };

    // 1) `forget` cancels pending `outbox('publish')` and queues `'withdraw'` (never `'purge'`).
    let f_forget = fact("fact_pending_forget", &["owner"])?;
    store.insert_fact(&f_forget)?;
    store.enqueue(
        dest,
        "publish",
        "pub-forget",
        &serde_json::json!({
            "fact": f_forget,
            "origin_reference": f_forget.strong_ref(),
        }),
    )?;
    store.forget_fact_id(&f_forget.id, &owner, AT)?;
    let out1 = store.pending_outbox(dest)?;
    assert_eq!(out1.len(), 1);
    assert_eq!(out1[0].kind, "withdraw");
    assert_eq!(out1[0].payload["fact_id"], expected_remote(&f_forget)?);
    store.outbox_result(out1[0].id, Some(AT))?;

    // 2) `reject_fact_with_options` (both without and with replacement) cancels pending publish and queues `'withdraw'`.
    let f_rej1 = fact("fact_pending_rej1", &["owner"])?;
    store.insert_fact(&f_rej1)?;
    store.enqueue(
        dest,
        "publish",
        "pub-rej1",
        &serde_json::json!({ "origin_reference": f_rej1.strong_ref() }),
    )?;
    store.reject_fact(&f_rej1.id, &owner, "wrong claim", AT)?;
    let out2 = store.pending_outbox(dest)?;
    assert_eq!(out2.len(), 1);
    assert_eq!(out2[0].kind, "withdraw");
    store.outbox_result(out2[0].id, Some(AT))?;

    let f_rej2 = fact("fact_pending_rej2", &["owner"])?;
    let f_rej2_rep = fact("fact_pending_rej2_rep", &["owner"])?;
    store.insert_fact(&f_rej2)?;
    store.enqueue(
        dest,
        "publish",
        "pub-rej2",
        &serde_json::json!({ "origin_reference": f_rej2.strong_ref() }),
    )?;
    store.reject_fact_with_options(
        &f_rej2.id,
        &owner,
        "replaced",
        AT,
        crate::RejectOptions {
            replacement: Some(&f_rej2_rep),
            ..Default::default()
        },
    )?;
    let out3 = store.pending_outbox(dest)?;
    assert_eq!(out3.len(), 1);
    assert_eq!(out3[0].kind, "withdraw");
    assert_eq!(out3[0].payload["fact_id"], expected_remote(&f_rej2)?);
    store.outbox_result(out3[0].id, Some(AT))?;

    // 3) `Store::supersede` and `Store::remember` (time-varying slot supersession) queue `'withdraw'`.
    let f_sup = fact("fact_pending_sup", &["owner"])?;
    let f_sup_new = fact("fact_pending_sup_new", &["owner"])?;
    store.insert_fact(&f_sup)?;
    store.record_publication(dest, &f_sup.strong_ref(), &expected_remote(&f_sup)?, AT)?;
    store.supersede(&f_sup.id, &f_sup_new)?;
    let out4 = store.pending_outbox(dest)?;
    assert_eq!(out4.len(), 1);
    assert_eq!(out4[0].kind, "withdraw");
    store.outbox_result(out4[0].id, Some(AT))?;

    store.define_slot(&SlotContract {
        predicate: "kp.test.single_tv".into(),
        namespace: "kp.test".into(),
        cardinality: Cardinality::Single,
        mutability: Mutability::TimeVarying,
        value_kind: "json".into(),
        description: "tv".into(),
        instruction: "tv".into(),
    })?;
    let mut slot_v1 = fact("fact_slot_v1", &["owner"])?;
    slot_v1.subject = "svc".into();
    slot_v1.predicate = "kp.test.single_tv".into();
    store.remember(&[crate::FactInput {
        fact: slot_v1.clone(),
        entities: Default::default(),
    }])?;
    store.enqueue(
        dest,
        "publish",
        "pub-slot-v1",
        &serde_json::json!({ "origin_reference": slot_v1.strong_ref() }),
    )?;
    let mut slot_v2 = fact("fact_slot_v2", &["owner"])?;
    slot_v2.subject = "svc".into();
    slot_v2.predicate = "kp.test.single_tv".into();
    slot_v2.object = serde_json::json!("updated");
    store.remember(&[crate::FactInput {
        fact: slot_v2,
        entities: Default::default(),
    }])?;
    let out5 = store.pending_outbox(dest)?;
    assert_eq!(out5.len(), 1);
    assert_eq!(out5[0].kind, "withdraw");
    assert_eq!(out5[0].payload["fact_id"], expected_remote(&slot_v1)?);
    store.outbox_result(out5[0].id, Some(AT))?;

    // 4) In-flight publish race: local `forget` or `purge` finishes while HTTP `/v1/publish` was in flight!
    let f_inflight = fact("fact_inflight_withdraw", &["owner"])?;
    store.insert_fact(&f_inflight)?;
    store.enqueue(
        dest,
        "publish",
        "pub-inflight",
        &serde_json::json!({ "origin_reference": f_inflight.strong_ref() }),
    )?;
    let inflight_id = store.pending_outbox(dest)?[0].id;
    // Concurrent `forget` runs and its early `'withdraw'` even gets flushed (e.g. 404 before publish landed)!
    store.forget_fact_id(&f_inflight.id, &owner, AT)?;
    for w in store.pending_outbox(dest)? {
        store.outbox_result(w.id, Some(AT))?;
    }
    assert!(store.pending_outbox(dest)?.is_empty());
    // Now slow `/v1/publish` returns 200 OK -> `finalize_inflight_publish` re-arms `'withdraw'`!
    let aud_owner = AudienceSpec::Explicit([owner].into());
    store.finalize_inflight_publish(&crate::FinalizePublishSpec {
        outbox_id: inflight_id,
        destination: dest,
        reference: &f_inflight.strong_ref(),
        remote_id: &expected_remote(&f_inflight)?,
        audience: &aud_owner,
        revision: 1,
        at: AT,
    })?;
    let out_inflight = store.pending_outbox(dest)?;
    assert_eq!(out_inflight.len(), 1);
    assert_eq!(out_inflight[0].kind, "withdraw");
    store.outbox_result(out_inflight[0].id, Some(AT))?;

    // 5) `purge` emits `'purge'` tombstone (both directly and on in-flight completion).
    let f_purge = fact("fact_purge_tombstone", &["owner"])?;
    store.insert_fact(&f_purge)?;
    store.record_publication(dest, &f_purge.strong_ref(), &expected_remote(&f_purge)?, AT)?;
    store.purge(&f_purge.origin_library, &f_purge.logical_key, AT, &[])?;
    let out_purge = store.pending_outbox(dest)?;
    assert_eq!(out_purge.len(), 1);
    assert_eq!(out_purge[0].kind, "purge");
    assert!(out_purge[0].payload.get("tombstone_id").is_some());
    Ok(())
}

#[test]
fn needs_alignment_persists_across_mutations_and_ignores_internal_alignment_facts() -> Result<()> {
    use kp_model::{AudienceSpec, MemberRole, SourceScope};

    let (dir, mut store) = library(LibraryMode::Personal)?;
    let owner: kp_model::ActorId = "owner".parse()?;
    assert!(!store.needs_alignment()?);

    // `insert_fact` sets `needs_alignment = true` and persists across reopen.
    let f1 = fact("fact_align_1", &["owner"])?;
    store.insert_fact(&f1)?;
    assert!(store.needs_alignment()?);
    drop(store);
    let mut store = Store::open(dir.path())?;
    assert!(store.needs_alignment()?);

    store.set_needs_alignment(false, AT)?;
    assert!(!store.needs_alignment()?);

    // Internal `kp.core.concept_alignment` insert and supersede do NOT dirty `needs_alignment`!
    let mut align_v1 = fact("align_meta_1", &[])?;
    align_v1.predicate = "kp.core.concept_alignment".into();
    store.insert_fact(&align_v1)?;
    assert!(!store.needs_alignment()?);
    let mut align_v2 = fact("align_meta_2", &[])?;
    align_v2.predicate = "kp.core.concept_alignment".into();
    store.supersede(&align_v1.id, &align_v2)?;
    assert!(!store.needs_alignment()?);

    // `remember` -> true
    let f2 = fact("fact_align_2", &["owner"])?;
    store.remember(&[crate::FactInput {
        fact: f2.clone(),
        entities: Default::default(),
    }])?;
    assert!(store.needs_alignment()?);
    store.set_needs_alignment(false, AT)?;

    // `supersede` -> true
    let f2_rep = fact("fact_align_2_rep", &["owner"])?;
    store.supersede(&f2.id, &f2_rep)?;
    assert!(store.needs_alignment()?);
    store.set_needs_alignment(false, AT)?;

    // `forget_fact_id` & `forget` -> true
    store.forget_fact_id(&f2_rep.id, &owner, AT)?;
    assert!(store.needs_alignment()?);
    store.set_needs_alignment(false, AT)?;
    store.forget(f1.id.as_str(), &owner, AT)?;
    assert!(store.needs_alignment()?);
    store.set_needs_alignment(false, AT)?;

    // `reject_fact_with_options` -> true
    let f3 = fact("fact_align_3", &["owner"])?;
    store.insert_fact(&f3)?;
    store.set_needs_alignment(false, AT)?;
    store.reject_fact(&f3.id, &owner, "obsolete", AT)?;
    assert!(store.needs_alignment()?);
    store.set_needs_alignment(false, AT)?;

    // `register_peer_members` (`added > 0` sets true; `added == 0` stays false)
    assert_eq!(store.register_peer_members(&["alice".parse()?], AT)?, 1);
    assert!(store.needs_alignment()?);
    store.set_needs_alignment(false, AT)?;
    assert_eq!(store.register_peer_members(&["alice".parse()?], AT)?, 0);
    assert!(!store.needs_alignment()?);

    // `add_member` & `revoke_member` -> true
    store.add_member("bob".parse()?, MemberRole::Member, AT)?;
    assert!(store.needs_alignment()?);
    store.set_needs_alignment(false, AT)?;
    store.revoke_member(&"bob".parse()?, AT)?;
    assert!(store.needs_alignment()?);
    store.set_needs_alignment(false, AT)?;

    // `upsert_source_scope` -> true
    store.upsert_source_scope(&SourceScope {
        scope_id: "scope_1".parse()?,
        provider: "container".into(),
        external_container_id: "ext_1".into(),
        label: "Scope".into(),
        current_audience: AudienceSpec::AllMembers,
        synced_at: AT,
    })?;
    assert!(store.needs_alignment()?);
    store.set_needs_alignment(false, AT)?;

    // `apply_scope_review_fact_updates` -> true
    store.apply_scope_review_fact_updates(&crate::ScopeReviewFactUpdates::default())?;
    assert!(store.needs_alignment()?);
    store.set_needs_alignment(false, AT)?;

    // `import_snapshot_with_governance` (`!unchanged` -> true, `unchanged` -> stays false)
    let (_odir, mut other) = library(LibraryMode::Personal)?;
    other.insert_fact(&fact("fact_remote_align", &["owner"])?)?;
    let snap = other.change_snapshot(&owner, AT)?;
    store.import_snapshot_with_governance("https://remote.example", &snap, &[], &[], None, AT)?;
    assert!(store.needs_alignment()?);
    store.set_needs_alignment(false, AT)?;
    let unchanged = other.change_snapshot_since(&owner, AT, Some(snap.cursor.as_str()))?;
    store.import_snapshot_with_governance(
        "https://remote.example",
        &unchanged,
        &[],
        &[],
        None,
        AT,
    )?;
    assert!(!store.needs_alignment()?);
    Ok(())
}

#[test]
fn shrink_before_first_delivery_prepare_outbox_atomic_enqueue_same_rev_conflict_and_withdrawn_barrier(
) -> Result<()> {
    use kp_model::{AudienceSpec, MemberRole};

    let dest = "https://community.example/";
    let (_dir, mut store) = crate::InitOptions {
        library_id: "alice-personal".parse()?,
        name: "Alice".into(),
        mode: LibraryMode::Personal,
        owner: "alice".parse()?,
        created_at: AT,
    }
    .pipe_init()?;
    let alice: kp_model::ActorId = "alice".parse()?;
    let bob: kp_model::ActorId = "bob".parse()?;
    let dave: kp_model::ActorId = "dave".parse()?;
    store.register_peer_members(&[bob.clone(), dave.clone()], AT)?;
    assert_eq!(store.daemon_pending_outbox_count()?, 0);

    // 1) `enqueue_publish_atomic` + shrink to `SelfOnly` BEFORE any `publication` receipt cancels pending publish!
    let mut f_shrink = fact("fact_shrink_before_send", &["alice", "bob"])?;
    f_shrink.origin_library = "alice-personal".parse()?;
    f_shrink.author = alice.clone();
    store.insert_fact(&f_shrink)?;
    let (rev1, _) = store
        .enqueue_publish_atomic(
            &crate::PublishEnqueueSpec {
                destination: dest,
                origin_library: &f_shrink.origin_library,
                logical_key: &f_shrink.logical_key,
                fact_id: &f_shrink.id,
                explicit_audience_override: None,
                require_multi_actor_or_prior_receipt: false,
            },
            |rev, aud, _| {
                let mut f = f_shrink.clone();
                if let kp_model::FactAccess::Base { audience, .. } = &mut f.access {
                    *audience = aud.clone();
                }
                Ok((
                    format!("idem-{rev}"),
                    serde_json::json!({
                        "fact": f,
                        "origin_reference": f_shrink.strong_ref(),
                    }),
                ))
            },
        )?
        .ok_or(crate::StoreError::NotFound)?;
    assert_eq!(rev1, 1);
    assert_eq!(store.daemon_pending_outbox_count()?, 1);

    // Narrow to `SelfOnly` (`{alice}`) before first delivery -> outbox publish cancelled immediately!
    store.narrow_audience(
        &f_shrink.id,
        &AudienceSpec::Explicit([alice.clone()].into()),
    )?;
    assert_eq!(store.daemon_pending_outbox_count()?, 0);

    // 2) `prepare_outbox_publish` refreshes widened audience & bumps revision, and cancels on retraction!
    let mut f_prep = fact("fact_prepare_refresh", &["alice", "bob"])?;
    f_prep.origin_library = "alice-personal".parse()?;
    f_prep.author = alice.clone();
    store.insert_fact(&f_prep)?;
    store.enqueue_publish_atomic(
        &crate::PublishEnqueueSpec {
            destination: dest,
            origin_library: &f_prep.origin_library,
            logical_key: &f_prep.logical_key,
            fact_id: &f_prep.id,
            explicit_audience_override: None,
            require_multi_actor_or_prior_receipt: false,
        },
        |rev, _, _| {
            Ok((
                format!("prep-{rev}"),
                serde_json::json!({
                    "fact": f_prep,
                    "origin_reference": f_prep.strong_ref(),
                }),
            ))
        },
    )?;
    let out_id = store.pending_outbox(dest)?[0].id;
    let aud_abd = AudienceSpec::Explicit([alice.clone(), bob.clone(), dave.clone()].into());
    store.narrow_audience(&f_prep.id, &aud_abd)?;
    let refreshed = store
        .prepare_outbox_publish(out_id)?
        .ok_or(crate::StoreError::NotFound)?;
    assert_eq!(refreshed["audience_revision"], 2);
    assert_eq!(
        serde_json::from_value::<AudienceSpec>(refreshed["fact"]["access"]["audience"].clone())?,
        aud_abd
    );

    // Forget `f_prep` before HTTP send -> `prepare_outbox_publish` drops publish and leaves `'withdraw'` with StrongRef!
    store.db.execute(
        "UPDATE facts SET status='withdrawn', expired_at=?1 WHERE fact_id=?2",
        rusqlite::params![AT.0, f_prep.id.as_str()],
    )?;
    assert!(store.prepare_outbox_publish(out_id)?.is_none());
    let pending_w = store.pending_outbox(dest)?;
    assert_eq!(pending_w.len(), 1);
    assert_eq!(pending_w[0].kind, "withdraw");
    assert_eq!(pending_w[0].payload["origin_library"], "alice-personal");
    assert_eq!(
        pending_w[0].payload["logical_key"],
        f_prep.logical_key.as_str()
    );
    assert_eq!(
        pending_w[0].payload["content_sha256"],
        f_prep.strong_ref().content_sha256.as_str()
    );

    // 3) Server same-revision conflict check + `withdrawn_version` barrier:
    let (_cdir, mut community) = library(LibraryMode::Community)?;
    community.add_member(alice.clone(), MemberRole::Member, AT)?;
    community.add_member(bob.clone(), MemberRole::Member, AT)?;
    community.add_member(dave.clone(), MemberRole::Member, AT)?;
    let mut pub_f = fact("published-conflict-check", &["alice", "bob"])?;
    pub_f.author = alice.clone();
    community.insert_fact(&pub_f)?;

    let aud_ab = AudienceSpec::Explicit([alice.clone(), bob.clone()].into());
    // Same revision (1) + same audience -> idempotent `Ok(false)`.
    assert!(!community.exchange_revise_published_audience(&pub_f.id, &aud_ab, 1, false, AT)?);
    // Same revision (1) + conflicting audience -> `Err(StoreError::Conflict)` (409)!
    assert!(matches!(
        community.exchange_revise_published_audience(&pub_f.id, &aud_abd, 1, false, AT),
        Err(crate::StoreError::Conflict)
    ));
    // Higher revision (2) -> `Ok(true)`.
    assert!(community.exchange_revise_published_audience(&pub_f.id, &aud_abd, 2, false, AT)?);
    // Same revision (2) + conflicting audience -> `Err(StoreError::Conflict)`.
    assert!(matches!(
        community.exchange_revise_published_audience(&pub_f.id, &aud_ab, 2, false, AT),
        Err(crate::StoreError::Conflict)
    ));

    // Version-bound `withdrawn_version` blocks exact withdrawn StrongRef without blocking new hash/key!
    let sref = pub_f.strong_ref();
    assert!(!community.is_version_withdrawn(
        &sref.origin_library,
        &sref.logical_key,
        &sref.content_sha256
    )?);
    community.record_withdrawn_version(
        &sref.origin_library,
        &sref.logical_key,
        &sref.content_sha256,
        &alice,
        AT,
    )?;
    assert!(community.is_version_withdrawn(
        &sref.origin_library,
        &sref.logical_key,
        &sref.content_sha256
    )?);
    let other_hash = kp_model::ContentHash::digest("relearned-new-version");
    assert!(!community.is_version_withdrawn(
        &sref.origin_library,
        &sref.logical_key,
        &other_hash
    )?);

    // 4) Generation-counted `alignment_state` preserves concurrent write dirty flag across `ack_alignment_generation`!
    store.set_needs_alignment(false, AT)?;
    assert!(!store.needs_alignment()?);
    store.insert_fact(&fact("fact_gen_step_1", &["alice"])?)?;
    let gen_started = store.alignment_dirty_generation()?;
    assert!(store.needs_alignment()?);
    // Concurrent write commits while LLM alignment is running:
    store.insert_fact(&fact("fact_gen_step_2", &["alice"])?)?;
    let gen_after_concurrent = store.alignment_dirty_generation()?;
    assert!(gen_after_concurrent > gen_started);
    // Acking `gen_started` MUST keep `needs_alignment() == true`!
    store.ack_alignment_generation(gen_started, AT)?;
    assert!(store.needs_alignment()?);
    // Acking `gen_after_concurrent` clears `needs_alignment() == false`!
    store.ack_alignment_generation(gen_after_concurrent, AT)?;
    assert!(!store.needs_alignment()?);
    Ok(())
}
