use kp_model::{ActorId, ContentHash, EpochMs, KpError, LibraryMode};

use super::*;
use crate::governance::new_source_id;
use crate::test_support::{library, AT};
use crate::StoreError;

#[test]
fn session_cursors_track_incremental_offsets_and_idle_windows() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;

    let s1 = SessionCursor {
        session_id: "claude-sess-1".into(),
        client_kind: "claude-code".into(),
        transcript_path: Some("/tmp/s1.jsonl".into()),
        last_byte_offset: 4096,
        last_line_count: 18,
        prefix_sha256: ContentHash::digest("line1").to_string(),
        last_activity_at: EpochMs(1_000),
        last_extracted_offset: 1024,
    };
    let s2 = SessionCursor {
        session_id: "codex-sess-2".into(),
        client_kind: "codex".into(),
        transcript_path: Some("/tmp/s2.jsonl".into()),
        last_byte_offset: 2048,
        last_line_count: 9,
        prefix_sha256: ContentHash::digest("codex1").to_string(),
        last_activity_at: EpochMs(9_000),
        last_extracted_offset: 0,
    };
    store.upsert_session_cursor(&s1)?;
    store.upsert_session_cursor(&s2)?;

    assert_eq!(store.get_session_cursor("claude-sess-1")?, Some(s1.clone()));
    assert!(store.get_session_cursor("missing")?.is_none());

    // Only s1 is idle at t <= 5_000.
    let idle = store.idle_unextracted_sessions(EpochMs(5_000))?;
    assert_eq!(idle.len(), 1);
    assert_eq!(idle[0].session_id, "claude-sess-1");

    // Advancing `last_extracted_offset` removes s1 from unextracted queue.
    assert!(store.mark_session_extracted("claude-sess-1", 4096)?);
    assert!(store.idle_unextracted_sessions(EpochMs(5_000))?.is_empty());
    assert_eq!(store.idle_unextracted_sessions(EpochMs(10_000))?.len(), 1);
    Ok(())
}

#[test]
fn extraction_queue_handles_priority_lease_crash_recovery_quota_pause_and_chunk_idempotency(
) -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let owner: ActorId = "owner".parse()?;
    let alice: ActorId = "alice".parse()?;

    // Compliance-locked `raw_only` sources are rejected at enqueue boundary.
    let raw_only_attempt = ExtractionJobInput {
        source_id: new_source_id()?,
        chunk_ids: vec!["c0".into()],
        chunk_batch_sha256: ContentHash::digest("ext-chat-raw"),
        author: owner.clone(),
        model_policy: "raw_only".into(),
        priority: 10,
        reference_time: AT,
        created_at: AT,
    };
    assert!(matches!(
        store.enqueue_extraction_job(&raw_only_attempt),
        Err(StoreError::Model(KpError::InvalidInput {
            field: "model_policy",
            ..
        }))
    ));

    let low_prio_src = new_source_id()?;
    let high_prio_src = new_source_id()?;
    let low_input = ExtractionJobInput {
        source_id: low_prio_src,
        chunk_ids: vec!["low-0".into()],
        chunk_batch_sha256: ContentHash::digest("low-batch"),
        author: owner.clone(),
        model_policy: "host".into(),
        priority: 1,
        reference_time: AT,
        created_at: AT,
    };
    let high_input = ExtractionJobInput {
        source_id: high_prio_src,
        chunk_ids: vec!["high-0".into(), "high-1".into()],
        chunk_batch_sha256: ContentHash::digest("high-batch"),
        author: owner.clone(),
        model_policy: "host".into(),
        priority: 5,
        reference_time: EpochMs(AT.0 + 1_000),
        created_at: AT,
    };

    let low_job = store.enqueue_extraction_job(&low_input)?;
    // Duplicate enqueue for the same `(source_id, chunk_batch_sha256, author)` is idempotent.
    let low_dup = store.enqueue_extraction_job(&low_input)?;
    assert_eq!(low_job.job_id, low_dup.job_id);
    let high_job = store.enqueue_extraction_job(&high_input)?;

    // Higher-priority job leases first.
    let leased_high = store
        .lease_next_extraction_job(EpochMs(10_000), 30_000)?
        .ok_or(StoreError::NotFound)?;
    assert_eq!(leased_high.job_id, high_job.job_id);
    assert_eq!(leased_high.status, ExtractionJobStatus::Leased);
    assert_eq!(leased_high.attempts, 1);
    assert_eq!(leased_high.lease_until, Some(EpochMs(40_000)));

    // While `high_job` is leased (t = 20_000 < 40_000), next lease picks `low_job`.
    let leased_low = store
        .lease_next_extraction_job(EpochMs(20_000), 30_000)?
        .ok_or(StoreError::NotFound)?;
    assert_eq!(leased_low.job_id, low_job.job_id);

    // No pending jobs remain while both leases are active.
    assert!(store
        .lease_next_extraction_job(EpochMs(25_000), 30_000)?
        .is_none());

    // Quota exhaustion pauses `low_job`; it does NOT get leased until resumed.
    store.pause_extraction_job_quota(&low_job.job_id, "remaining rate limit 8% < 15%")?;
    assert_eq!(
        store.get_extraction_job(&low_job.job_id)?.map(|j| j.status),
        Some(ExtractionJobStatus::PausedQuota)
    );
    assert!(store
        .lease_next_extraction_job(EpochMs(30_000), 30_000)?
        .is_none());

    // Simulate daemon crash: at t = 40_000 (`>= high_job.lease_until`), `high_job` self-heals and re-leases!
    let recovered = store
        .lease_next_extraction_job(EpochMs(40_000), 30_000)?
        .ok_or(StoreError::NotFound)?;
    assert_eq!(recovered.job_id, high_job.job_id);
    assert_eq!(recovered.attempts, 2);

    // Complete `high_job` atomically with `extracted_chunks` tracking.
    let c0_hash = ContentHash::digest("chunk-0-body");
    let c1_hash = ContentHash::digest("chunk-1-body");
    assert!(!store.is_chunk_extracted(&c0_hash, &owner, "rev4.v1")?);
    store.complete_extraction_job_with_chunks(
        &high_job.job_id,
        &[c0_hash.clone(), c1_hash.clone()],
        Some(&owner),
        "rev4.v1",
        EpochMs(41_000),
    )?;
    assert!(store.is_chunk_extracted(&c0_hash, &owner, "rev4.v1")?);
    assert!(store.is_chunk_extracted(&c1_hash, &owner, "rev4.v1")?);
    // Different author or bumped schema_ver is tracked independently.
    assert!(!store.is_chunk_extracted(&c0_hash, &alice, "rev4.v1")?);
    assert!(!store.is_chunk_extracted(&c0_hash, &owner, "rev4.v2")?);
    assert!(!store.mark_chunk_extracted(&c0_hash, &owner, "rev4.v1", EpochMs(42_000))?);
    assert!(store.mark_chunk_extracted(&c0_hash, &owner, "rev4.v2", EpochMs(42_000))?);

    // Resume quota-paused jobs and complete `low_job`.
    assert_eq!(store.resume_paused_quota_jobs()?, 1);
    let resumed_low = store
        .lease_next_extraction_job(EpochMs(45_000), 30_000)?
        .ok_or(StoreError::NotFound)?;
    assert_eq!(resumed_low.job_id, low_job.job_id);
    store.complete_extraction_job(&low_job.job_id)?;
    assert_eq!(
        store.get_extraction_job(&low_job.job_id)?.map(|j| j.status),
        Some(ExtractionJobStatus::Done)
    );
    Ok(())
}

#[test]
fn same_priority_cross_midnight_jobs_lease_chronologically_and_spans_advance_monotonically(
) -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let owner: ActorId = "owner".parse()?;

    // Session file has grown concurrently to 1500 bytes while sub-batches cover 0..1000.
    store.upsert_session_cursor(&SessionCursor {
        session_id: "sess-midnight".into(),
        client_kind: "codex".into(),
        transcript_path: None,
        last_byte_offset: 1_500,
        last_line_count: 30,
        prefix_sha256: "prefix-1500".into(),
        last_activity_at: EpochMs(AT.0 + 9_000),
        last_extracted_offset: 0,
    })?;

    let day1_src = new_source_id()?;
    let day2_src = new_source_id()?;
    let day3_src = new_source_id()?;
    store.record_session_source_span(&day1_src, "sess-midnight", 0, 300, AT)?;
    store.record_session_source_span(&day2_src, "sess-midnight", 300, 700, EpochMs(AT.0 + 1))?;
    store.record_session_source_span(&day3_src, "sess-midnight", 700, 1_000, EpochMs(AT.0 + 2))?;

    let j1 = store.enqueue_extraction_job(&ExtractionJobInput {
        source_id: day1_src.clone(),
        chunk_ids: vec!["d1".into()],
        chunk_batch_sha256: ContentHash::digest("day1"),
        author: owner.clone(),
        model_policy: "host".into(),
        priority: 0,
        reference_time: AT,
        created_at: AT,
    })?;
    let j2 = store.enqueue_extraction_job(&ExtractionJobInput {
        source_id: day2_src.clone(),
        chunk_ids: vec!["d2".into()],
        chunk_batch_sha256: ContentHash::digest("day2"),
        author: owner.clone(),
        model_policy: "host".into(),
        priority: 0,
        reference_time: EpochMs(AT.0 + 86_400_000),
        created_at: EpochMs(AT.0 + 1),
    })?;
    let j3 = store.enqueue_extraction_job(&ExtractionJobInput {
        source_id: day3_src.clone(),
        chunk_ids: vec!["d3".into()],
        chunk_batch_sha256: ContentHash::digest("day3"),
        author: owner,
        model_policy: "host".into(),
        priority: 0,
        reference_time: EpochMs(AT.0 + 172_800_000),
        created_at: EpochMs(AT.0 + 2),
    })?;

    // Same-priority cross-midnight sub-batches lease in strict chronological causal order (Day 1 -> Day 2 -> Day 3).
    let l1 = store
        .lease_next_extraction_job(EpochMs(AT.0 + 10), 60_000)?
        .ok_or(StoreError::NotFound)?;
    let l2 = store
        .lease_next_extraction_job(EpochMs(AT.0 + 11), 60_000)?
        .ok_or(StoreError::NotFound)?;
    let l3 = store
        .lease_next_extraction_job(EpochMs(AT.0 + 12), 60_000)?
        .ok_or(StoreError::NotFound)?;
    assert_eq!(l1.job_id, j1.job_id);
    assert_eq!(l2.job_id, j2.job_id);
    assert_eq!(l3.job_id, j3.job_id);

    // Day 1 completes -> cursor advances to 300 (NEVER jumping to last_byte_offset = 1500).
    store.complete_extraction_job(&j1.job_id)?;
    assert_eq!(
        store.complete_session_source_span(&day1_src)?,
        Some(("sess-midnight".into(), 300))
    );
    assert_eq!(
        store
            .get_session_cursor("sess-midnight")?
            .map(|c| c.last_extracted_offset),
        Some(300)
    );

    // Day 2 fails (or pauses) while Day 3 completes -> cursor MUST stay at 300!
    store.pause_extraction_job_quota(&j2.job_id, "429 quota")?;
    store.complete_extraction_job(&j3.job_id)?;
    assert_eq!(
        store.complete_session_source_span(&day3_src)?,
        Some(("sess-midnight".into(), 300))
    );
    assert_eq!(
        store
            .get_session_cursor("sess-midnight")?
            .map(|c| c.last_extracted_offset),
        Some(300)
    );

    // Once Day 2 resumes and completes, completing Day 3's span advances to 1000 (leaving 1000..1500 unextracted!).
    store.resume_paused_quota_jobs()?;
    store.complete_extraction_job(&j2.job_id)?;
    assert_eq!(
        store.complete_session_source_span(&day3_src)?,
        Some(("sess-midnight".into(), 1_000))
    );
    let final_cur = store
        .get_session_cursor("sess-midnight")?
        .ok_or(StoreError::NotFound)?;
    assert_eq!(final_cur.last_extracted_offset, 1_000);
    assert_eq!(final_cur.last_byte_offset, 1_500);

    // Non-session source returns `Ok(None)`.
    assert_eq!(store.complete_session_source_span(&new_source_id()?)?, None);
    Ok(())
}

#[test]
fn ingest_session_subbatch_and_lease_fenced_retry_recover_atomically() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let owner: ActorId = "owner".parse()?;
    let lib_id = store.library_id().clone();

    store.upsert_session_cursor(&SessionCursor {
        session_id: "sess-fenced".into(),
        client_kind: "codex".into(),
        transcript_path: None,
        last_byte_offset: 900,
        last_line_count: 10,
        prefix_sha256: "pref-900".into(),
        last_activity_at: AT,
        last_extracted_offset: 0,
    })?;

    let key: kp_model::LogicalKey = "session:sess-fenced:0:450".parse()?;
    let expected_src_id = crate::tombstone_id(lib_id.as_str().as_bytes(), &lib_id, &key)?;
    let source_id: kp_model::SourceId = expected_src_id.as_str().parse()?;
    let raw_in = crate::test_support::source(&["owner"])?;
    let job_in = ExtractionJobInput {
        source_id: source_id.clone(),
        chunk_ids: vec![format!("{source_id}-0")],
        chunk_batch_sha256: ContentHash::digest("subbatch-0-450"),
        author: owner.clone(),
        model_policy: "host".into(),
        priority: 1,
        reference_time: AT,
        created_at: AT,
    };

    // 1) Atomic sub-batch ingest + idempotent recovery on duplicate `(origin, key)`.
    let (stored1, newly1) = store.ingest_session_subbatch(&crate::SessionSubbatchSpec {
        origin: &lib_id,
        key: &key,
        source_input: &raw_in,
        job_input: &job_in,
        session_id: "sess-fenced",
        start_byte: 0,
        end_byte: 450,
        at: AT,
    })?;
    assert!(newly1);
    assert_eq!(stored1.source_id, source_id);
    let (stored2, newly2) = store.ingest_session_subbatch(&crate::SessionSubbatchSpec {
        origin: &lib_id,
        key: &key,
        source_input: &raw_in,
        job_input: &job_in,
        session_id: "sess-fenced",
        start_byte: 0,
        end_byte: 450,
        at: AT,
    })?;
    assert!(!newly2);
    assert_eq!(stored2.source_id, stored1.source_id);

    // 2) Lease attempt 1, then transient failure sets backoff `retry_after = AT + 5_000`.
    let leased1 = store
        .lease_next_extraction_job(AT, 30_000)?
        .ok_or(StoreError::NotFound)?;
    assert_eq!(leased1.attempts, 1);
    assert!(store.verify_extraction_job_lease(&leased1.job_id, 1, EpochMs(AT.0 + 100))?);
    assert!(!store.verify_extraction_job_lease(&leased1.job_id, 2, EpochMs(AT.0 + 100))?);

    let st = store.release_extraction_job_for_retry(
        &leased1.job_id,
        1,
        "subprocess timeout",
        EpochMs(AT.0 + 5_000),
        3,
    )?;
    assert_eq!(st, ExtractionJobStatus::Leased);
    // Not eligible before `retry_after`:
    assert!(store
        .lease_next_extraction_job(EpochMs(AT.0 + 4_999), 30_000)?
        .is_none());
    // Auto-recovers and re-leases at `now >= retry_after` (`attempts == 2`):
    let leased2 = store
        .lease_next_extraction_job(EpochMs(AT.0 + 5_000), 30_000)?
        .ok_or(StoreError::NotFound)?;
    assert_eq!(leased2.job_id, leased1.job_id);
    assert_eq!(leased2.attempts, 2);

    // Stale worker (`expected_attempts = 1`) is fenced out with `StoreError::Conflict`!
    assert!(matches!(
        store.complete_extraction_job_fenced(&leased2.job_id, 1, &[], None, "", AT),
        Err(StoreError::Conflict)
    ));
    assert!(matches!(
        store.pause_extraction_job_quota_fenced(&leased2.job_id, 1, "429"),
        Err(StoreError::Conflict)
    ));

    // Current worker (`expected_attempts = 2`) completes job AND advances `session_cursors` atomically!
    let c_hash = ContentHash::digest("chunk-0");
    store.complete_extraction_job_fenced(
        &leased2.job_id,
        2,
        std::slice::from_ref(&c_hash),
        Some(&owner),
        "rev5.v1",
        EpochMs(AT.0 + 6_000),
    )?;
    assert!(store.is_chunk_extracted(&c_hash, &owner, "rev5.v1")?);
    assert_eq!(
        store
            .get_session_cursor("sess-fenced")?
            .map(|c| c.last_extracted_offset),
        Some(450)
    );

    // Reaching `max_attempts` transitions to `FailedPermanent`, and `ingest_session_subbatch` re-arms it!
    let key2: kp_model::LogicalKey = "session:sess-fenced:450:900".parse()?;
    let src2_hash = crate::tombstone_id(lib_id.as_str().as_bytes(), &lib_id, &key2)?;
    let src2: kp_model::SourceId = src2_hash.as_str().parse()?;
    let job_in2 = ExtractionJobInput {
        source_id: src2.clone(),
        chunk_ids: vec![format!("{src2}-0")],
        chunk_batch_sha256: ContentHash::digest("subbatch-450-900"),
        author: owner,
        model_policy: "host".into(),
        priority: 1,
        reference_time: AT,
        created_at: EpochMs(AT.0 + 1),
    };
    store.ingest_session_subbatch(&crate::SessionSubbatchSpec {
        origin: &lib_id,
        key: &key2,
        source_input: &raw_in,
        job_input: &job_in2,
        session_id: "sess-fenced",
        start_byte: 450,
        end_byte: 900,
        at: AT,
    })?;
    let l_fail = store
        .lease_next_extraction_job(EpochMs(AT.0 + 7_000), 30_000)?
        .ok_or(StoreError::NotFound)?;
    assert_eq!(
        store.release_extraction_job_for_retry(
            &l_fail.job_id,
            1,
            "fatal parse",
            EpochMs(AT.0 + 8_000),
            1,
        )?,
        ExtractionJobStatus::FailedPermanent
    );
    // Re-ingesting the same sub-batch re-arms `FailedPermanent` back to `Pending`!
    store.ingest_session_subbatch(&crate::SessionSubbatchSpec {
        origin: &lib_id,
        key: &key2,
        source_input: &raw_in,
        job_input: &job_in2,
        session_id: "sess-fenced",
        start_byte: 450,
        end_byte: 900,
        at: AT,
    })?;
    assert_eq!(
        store.get_extraction_job(&l_fail.job_id)?.map(|j| j.status),
        Some(ExtractionJobStatus::Pending)
    );
    Ok(())
}
