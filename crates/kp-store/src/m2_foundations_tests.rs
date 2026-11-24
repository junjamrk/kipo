use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use kp_model::{
    derived_fact, justification, ActorId, AnchorSpec, AudienceSpec, BitemporalStamp,
    DerivationMode, EpochMs, Fact, FactAccess, FactKind, FactRank, FactStatus, GovernedScope,
    LibraryMode, MemberRole, ModelRoutePolicy, PublicAudience, PublicContainerRoster,
    PublicIngestOp, PublicIngestRecord, PublicIngestSource, PublicSpeaker, TrustTier,
    ValidityWindow,
};
use serde_json::json;

use crate::test_support::{fact, library, options, source, AT};
use crate::{
    Embedder, FactInput, FtsOnlyEmbedder, PublicRecordAppliedKind, Result, Store, StoreError,
};

struct CountingEmbedder {
    id: String,
    dims: usize,
    allow_raw_only: bool,
    fail_all: bool,
    calls: AtomicUsize,
}

impl CountingEmbedder {
    fn new(id: &str, dims: usize, allow_raw_only: bool) -> Self {
        Self {
            id: id.to_owned(),
            dims,
            allow_raw_only,
            fail_all: false,
            calls: AtomicUsize::new(0),
        }
    }

    fn failing(id: &str, dims: usize) -> Self {
        Self {
            id: id.to_owned(),
            dims,
            allow_raw_only: false,
            fail_all: true,
            calls: AtomicUsize::new(0),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Embedder for CountingEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_all {
            return Err(StoreError::Embedding("simulated provider timeout".into()));
        }
        let mut vec = vec![0.0_f32; self.dims];
        let hash = text.bytes().fold(0xcbf29ce484222325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
        });
        let idx_a = (hash as usize) % self.dims;
        let idx_b = ((hash >> 16) as usize) % self.dims;
        if idx_a == idx_b {
            vec[idx_a] = 1.0;
        } else {
            vec[idx_a] = 0.6;
            vec[idx_b] = 0.8;
        }
        Ok(vec)
    }

    fn model_id(&self) -> &str {
        &self.id
    }

    fn expected_dims(&self) -> Option<usize> {
        Some(self.dims)
    }

    fn allows_raw_only(&self) -> bool {
        self.allow_raw_only
    }
}

#[test]
fn dimension_guard_auto_locks_zero_dims_rejects_mismatch_and_scans_stored_vectors_without_reembedding(
) -> Result<()> {
    let dir = tempfile::tempdir()?;
    // 1. Initialize with `volcengine:ep-auto` (`dims = 0` -> auto-locks on first write).
    let mut store = Store::init_with_embedding(
        dir.path(),
        options(LibraryMode::Personal)?,
        "volcengine:ep-auto",
        0,
    )?;
    assert_eq!(
        store.embedding_config()?,
        ("volcengine:ep-auto".to_owned(), 0)
    );

    let emb_64 = Arc::new(CountingEmbedder::new("volcengine:ep-auto", 64, false));
    store.set_embedder(emb_64.clone());

    let mut f1 = fact("f_budget", &["owner"])?;
    f1.subject = "差旅费用申请".into();
    f1.object = json!("差旅住宿费用申请单必须在下月五日前提交");
    store.insert_fact(&f1)?;

    // Auto-locked to 64d!
    assert_eq!(store.status()?.embedding_dims, 64);
    assert_eq!(store.status()?.embedding_model, "volcengine:ep-auto");

    let mut s1 = source(&["owner"])?;
    s1.content = "研发采购报销制度说明".as_bytes().to_vec();
    s1.chunks[0].body = "研发采购报销制度说明".into();
    store.ingest_source(&"local".parse()?, &"doc_expense".parse()?, &s1)?;

    // 2. Attempting to write a 128d vector into a 64d locked library fails immediately with fix hint!
    let emb_128 = CountingEmbedder::new("openai:text-embedding-3-small", 128, false);
    let mismatch_err = store
        .insert_fact_with_embedder(&fact("f_bad_dim", &["owner"])?, &emb_128)
        .err()
        .ok_or(StoreError::NotFound)?
        .to_string();
    assert!(
        mismatch_err.contains("kp embed --stale --to openai:text-embedding-3-small --dim 128"),
        "unexpected mismatch error: {mismatch_err}"
    );

    // 3. Querying with `emb_64` embeds ONLY the query (1 call), scanning stored `fact_vectors`/`chunk_vectors`
    //    directly without re-embedding stored documents!
    let before_calls = emb_64.call_count();
    let snap = store.search_snapshot(&"owner".parse()?, AT, "费用申请报销", false, 10)?;
    let after_calls = emb_64.call_count();
    assert_eq!(after_calls - before_calls, 1, "must embed only query");
    assert_eq!(snap.documents().len(), 2);
    assert!(snap.documents().iter().all(|d| d.vector.len() == 64));

    // 4. When query embedding times out / fails, `search_snapshot` seamlessly falls back to FTS5!
    let failing = CountingEmbedder::failing("volcengine:ep-auto", 64);
    let fallback_snap = store.search_snapshot_with_embedder(
        &"owner".parse()?,
        AT,
        "费用申请",
        false,
        10,
        &failing,
    )?;
    assert_eq!(fallback_snap.documents().len(), 1);
    assert_eq!(fallback_snap.documents()[0].id, "fact-f_budget");
    assert!(fallback_snap.documents()[0].vector.is_empty());

    // 5. `FtsOnlyEmbedder` (`--no-embedding`) also falls back cleanly to FTS5!
    let fts_snap = store.search_snapshot_with_embedder(
        &"owner".parse()?,
        AT,
        "报销制度",
        false,
        10,
        &FtsOnlyEmbedder,
    )?;
    assert_eq!(fts_snap.documents().len(), 1);
    Ok(())
}

#[test]
fn reembed_stale_migrates_dimensions_protects_raw_only_sources_from_cloud_and_supports_none(
) -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let owner: ActorId = "owner".parse()?;

    // Ingest 1 normal fact + 1 normal source (`hash:v1`, 256d).
    store.remember_facts(&[FactInput {
        fact: fact("f_normal", &["owner"])?,
        entities: BTreeSet::new(),
    }])?;
    store.ingest_source(
        &"local".parse()?,
        &"normal_doc".parse()?,
        &source(&["owner"])?,
    )?;

    // Ingest 1 `RawOnly` public record + anchored fact.
    let raw_only_rec = PublicIngestRecord {
        source: PublicIngestSource {
            kind: "ext_chat".into(),
            uri: "chat://room_sec/msg_1".into(),
            external_id: "msg_1".into(),
            container_id: None,
        },
        speaker: PublicSpeaker::new("owner", None),
        audience: PublicAudience::Explicit(vec![owner.clone()]),
        occurred_at: AT,
        content: "内部合规原文不得外发云端".into(),
        op: PublicIngestOp::Upsert,
        ingested_by: Some(owner.clone()),
        media_type: Some("text/plain".into()),
        model_policy: Some(ModelRoutePolicy::RawOnly),
    };
    let raw_out =
        store.apply_public_ingest_record(&raw_only_rec, &owner, &ModelRoutePolicy::HostCli)?;
    let raw_sid = raw_out.source_id.ok_or(StoreError::NotFound)?;

    let mut raw_fact = fact("f_raw_only", &["owner"])?;
    raw_fact.source_anchor = Some(AnchorSpec {
        source_id: Some(raw_sid),
        ..AnchorSpec::default()
    });
    store.insert_fact(&raw_fact)?;

    // Migrate from `hash:v1` (`256d`) to cloud `dashscope:text-embedding-v3` (`64d`):
    // normal chunk & fact are re-embedded; `RawOnly` chunk & fact are skipped (`skipped_raw_only == 2`)!
    let cloud_emb = CountingEmbedder::new("dashscope:text-embedding-v3", 64, false);
    let rep_cloud = store.reembed_stale(&cloud_emb, Some(("dashscope:text-embedding-v3", 64)))?;
    assert_eq!(rep_cloud.model_id, "dashscope:text-embedding-v3");
    assert_eq!(rep_cloud.dims, 64);
    assert_eq!(rep_cloud.chunks_reembedded, 1);
    assert_eq!(rep_cloud.facts_reembedded, 1);
    assert_eq!(rep_cloud.skipped_raw_only, 2);

    // Re-running without changes does zero work (`up_to_date`).
    let rep_idem = store.reembed_stale(&cloud_emb, None)?;
    assert_eq!(rep_idem.chunks_reembedded, 0);
    assert_eq!(rep_idem.facts_reembedded, 0);
    assert_eq!(rep_idem.chunks_up_to_date, 1);
    assert_eq!(rep_idem.facts_up_to_date, 1);
    assert_eq!(rep_idem.skipped_raw_only, 2);

    // Migrating to local ONNX (`allows_raw_only == true`) embeds ALL 2 chunks and 2 facts!
    let local_emb = CountingEmbedder::new("local:bge-small-zh-v1.5", 64, true);
    let rep_local = store.reembed_stale(&local_emb, Some(("local:bge-small-zh-v1.5", 64)))?;
    assert_eq!(rep_local.chunks_reembedded, 2);
    assert_eq!(rep_local.facts_reembedded, 2);
    assert_eq!(rep_local.skipped_raw_only, 0);

    // Migrating to `none` (`0d`) clears vectors cleanly.
    let rep_none = store.reembed_stale(&FtsOnlyEmbedder, Some(("none", 0)))?;
    assert_eq!(rep_none.model_id, "none");
    assert_eq!(rep_none.dims, 0);
    assert_eq!(store.embedding_config()?, ("none".to_owned(), 0));
    Ok(())
}

#[test]
fn export_markdown_directory_enforces_semiring_visibility_frontmatter_and_projection_pages(
) -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Community)?;
    let owner: ActorId = "owner".parse()?;
    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;
    store.add_member(alice.clone(), MemberRole::Member, AT)?;
    store.add_member(bob.clone(), MemberRole::Member, AT)?;

    // Outsider cannot export!
    let out_dir = tempfile::tempdir()?;
    assert!(matches!(
        store.export_markdown_directory(out_dir.path(), &"outsider".parse()?, AT),
        Err(StoreError::PermissionDenied)
    ));

    // Insert shared rule + decision visible to `{owner, alice}`, and private fact visible ONLY to `bob`.
    let mut rule = fact("fact_rule_cutover", &["owner", "alice"])?;
    rule.kind = FactKind::Constraint;
    rule.subject = "数据库维护窗口".into();
    rule.object = json!({"claim_text": "周五晚高峰严禁执行主从切换"});
    rule.source_anchor = Some(AnchorSpec {
        verbatim_quote: Some("周五晚高峰严禁执行主从切换".into()),
        ..AnchorSpec::default()
    });
    store.insert_fact(&rule)?;

    let mut ev = fact("fact_ev_friday", &["owner", "alice"])?;
    ev.kind = FactKind::Event;
    ev.subject = "切换计划".into();
    ev.object = json!({"claim_text": "周五晚八点执行账单库切换"});
    store.insert_fact(&ev)?;

    let mut warning = derived_fact(
        store.library_id(),
        &owner,
        FactKind::CollisionWarning,
        "alice",
        "kp.core.collision_warning",
        json!({"summary": "周五晚八点切换与晚高峰禁令冲突"}),
        AT,
    )?;
    warning.id = "fact_warn_01".parse()?;
    warning.governed_scope = Some(GovernedScope::Actors {
        actors: BTreeSet::from([alice.clone()]),
    });
    store.insert_fact(&warning)?;
    let just = justification(
        &warning,
        DerivationMode::HardRule,
        "collision_v1",
        &[&rule, &ev],
    )?;
    store.insert_justification(&just)?;

    let mut bob_secret = fact("fact_bob_secret", &["bob"])?;
    bob_secret.author = bob.clone();
    bob_secret.subject = "Bob 私密备忘".into();
    bob_secret.object = json!("TOP_SECRET_BOB_ONLY_TOKEN_9911");
    store.insert_fact(&bob_secret)?;

    let mut alice_src = source(&["alice"])?;
    alice_src.uri = "docs://runbook.md".into();
    alice_src.content = "账单服务运维手册摘要".as_bytes().to_vec();
    alice_src.chunks[0].body = "账单服务运维手册摘要".into();
    store.ingest_source(&"local".parse()?, &"runbook".parse()?, &alice_src)?;

    let mut bob_src = source(&["bob"])?;
    bob_src.uri = "docs://bob-private.md".into();
    bob_src.content = "BOB_PRIVATE_SOURCE_CANARY_8822".as_bytes().to_vec();
    bob_src.chunks[0].body = "BOB_PRIVATE_SOURCE_CANARY_8822".into();
    store.ingest_source(&"local".parse()?, &"bob_priv".parse()?, &bob_src)?;

    let alice_export_dir = out_dir.path().join("alice");
    let report = store.export_markdown_directory(&alice_export_dir, &alice, AT)?;
    assert_eq!(report.viewer, alice);
    assert_eq!(report.visible_facts, 3);
    assert_eq!(report.visible_sources, 1);
    assert_eq!(report.pages.len(), 2);

    let index_text = std::fs::read_to_string(alice_export_dir.join("index.md"))?;
    let sources_text = std::fs::read_to_string(alice_export_dir.join("sources.md"))?;

    // Frontmatter + anchors + quotes + premises present for Alice:
    assert!(index_text.starts_with("---\npage_slug: index\n"));
    assert!(index_text.contains(&format!("library_id: {}\n", store.library_id())));
    assert!(index_text.contains("viewer: alice\n"));
    assert!(index_text.contains(&format!("base_seq: {}\n", report.base_seq)));
    assert!(index_text.contains("[kp-ref:fact_rule_cutover]"));
    assert!(index_text.contains("[kp-ref:fact_warn_01]"));
    assert!(index_text.contains("周五晚高峰严禁执行主从切换"));
    assert!(sources_text.contains("docs://runbook.md"));

    // Zero leakage of Bob's private fact or Bob's private source!
    for forbidden in [
        "TOP_SECRET_BOB_ONLY_TOKEN_9911",
        "fact_bob_secret",
        "BOB_PRIVATE_SOURCE_CANARY_8822",
        "bob-private.md",
    ] {
        assert!(
            !index_text.contains(forbidden) && !sources_text.contains(forbidden),
            "leaked `{forbidden}` into Alice's export!"
        );
    }

    // `projection_pages` recorded deterministically and repeats byte-identically:
    let meta = store
        .projection_page("index", &alice)?
        .ok_or(StoreError::NotFound)?;
    assert_eq!(
        meta.page_sha256,
        kp_model::ContentHash::digest(index_text.as_bytes())
    );
    let report2 = store.export_markdown_directory(&alice_export_dir, &alice, AT)?;
    assert_eq!(report.pages, report2.pages);
    Ok(())
}

#[test]
fn public_ingest_contract_handles_roster_identity_mapping_upsert_supersede_and_withdraw(
) -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let owner: ActorId = "owner".parse()?;
    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;
    store.add_member(alice.clone(), MemberRole::Member, AT)?;
    store.add_member(bob.clone(), MemberRole::Member, AT)?;

    // 1. Apply container roster with `alice` + unconfirmed external `ext_chat:u_bob`.
    let roster = PublicContainerRoster {
        container_id: "scope.eng_sync".into(),
        label: "研发同步讨论组".into(),
        members: vec!["alice".into(), "ext_chat:u_bob".into()],
        all_library_members: false,
        synced_at: EpochMs(1_000),
        provider: Some("ext_chat".into()),
        upstream_container_id: Some("room_eng".into()),
    };
    let scope = store.apply_public_container_roster(&owner, &roster)?;
    assert_eq!(
        scope.current_audience,
        AudienceSpec::Explicit(BTreeSet::from([alice.clone()]))
    );

    // Confirm `ext_chat:u_bob` -> `bob`, then advance roster `synced_at` so `bob` becomes live in container!
    assert!(store.resolve_external_identity_mapping(
        "ext_chat",
        "u_bob",
        Some(&bob),
        EpochMs(1_050),
    )?);
    let mut roster_confirmed = roster.clone();
    roster_confirmed.synced_at = EpochMs(1_050);
    let scope_confirmed = store.apply_public_container_roster(&owner, &roster_confirmed)?;
    assert_eq!(
        scope_confirmed.current_audience,
        AudienceSpec::Explicit(BTreeSet::from([alice.clone(), bob.clone()]))
    );

    // 2. Idempotent `Upsert` of `msg_100` spoken by `ext_chat:u_bob` (resolves to `bob`!).
    let upsert_100 = PublicIngestRecord {
        source: PublicIngestSource {
            kind: "ext_chat".into(),
            uri: "chat://room_eng/msg_100".into(),
            external_id: "msg_100".into(),
            container_id: Some("scope.eng_sync".into()),
        },
        speaker: PublicSpeaker::new("ext_chat:u_bob", Some("Bob".into())),
        audience: PublicAudience::ContainerMembers,
        occurred_at: EpochMs(1_100),
        content: "周三下午两点执行灰度发布窗口".into(),
        op: PublicIngestOp::Upsert,
        ingested_by: Some(owner.clone()),
        media_type: Some("text/plain".into()),
        model_policy: None,
    };
    let out1 = store.apply_public_ingest_record(&upsert_100, &owner, &ModelRoutePolicy::HostCli)?;
    assert_eq!(out1.kind, PublicRecordAppliedKind::Upserted);
    assert_eq!(out1.resolved_speaker, bob);
    assert!(out1.enqueued_job_id.is_some());
    let sid_100 = out1.source_id.clone().ok_or(StoreError::NotFound)?;
    assert_eq!(
        store.raw_source_text(&sid_100)?.as_deref(),
        Some("周三下午两点执行灰度发布窗口")
    );

    // Duplicate `Upsert` with identical content is idempotent; mutated content on same `external_id` conflicts!
    let dup = store.apply_public_ingest_record(&upsert_100, &owner, &ModelRoutePolicy::HostCli)?;
    assert_eq!(dup.kind, PublicRecordAppliedKind::DuplicateIdempotent);
    assert_eq!(dup.source_id, Some(sid_100.clone()));

    let mut mutated_same_id = upsert_100.clone();
    mutated_same_id.content = "篡改同 ID 内容".into();
    assert!(matches!(
        store.apply_public_ingest_record(&mutated_same_id, &owner, &ModelRoutePolicy::HostCli),
        Err(StoreError::Conflict)
    ));

    // Attach an extracted fact + published community record to `sid_100` to verify `Supersede`
    // retires the old fact and queues an outbox withdrawal!
    let extracted_100 = Fact {
        id: "fact_gray_100".parse()?,
        origin_library: store.library_id().clone(),
        logical_key: "gray_100".parse()?,
        kind: FactKind::Decision,
        subject: "灰度发布".into(),
        predicate: "kp.core.decision".into(),
        object: json!("周三下午两点执行灰度发布窗口"),
        tags: BTreeSet::new(),
        author: bob.clone(),
        ingested_by: Some(owner.clone()),
        trust_tier: TrustTier::HumanVerbatim,
        access: FactAccess::Base {
            audience: AudienceSpec::AllMembers,
            stamp: BitemporalStamp {
                valid: ValidityWindow {
                    valid_from: EpochMs(1_100),
                    valid_until: EpochMs::OPEN_END,
                },
                recorded_at: EpochMs(1_100),
                expired_at: None,
            },
        },
        source_scope_id: Some("scope.eng_sync".parse()?),
        governed_scope: None,
        broader_scope: None,
        status: FactStatus::Active,
        superseded_by: None,
        rank: FactRank::Normal,
        source_anchor: Some(AnchorSpec {
            source_id: Some(sid_100.clone()),
            ..AnchorSpec::default()
        }),
        unverified_gaps: Vec::new(),
        miss_count: 0,
    };
    extracted_100.validate()?;
    store.insert_fact(&extracted_100)?;
    store.record_publication(
        "https://community.example",
        &extracted_100.strong_ref(),
        extracted_100.id.as_str(),
        EpochMs(1_150),
    )?;
    assert_eq!(store.search_chunks("两点", &alice)?.len(), 1);

    // 3. `Supersede` `msg_100` -> `msg_100@v2` ("周三下午四点执行灰度发布窗口"):
    let supersede_100 = PublicIngestRecord {
        source: PublicIngestSource {
            external_id: "msg_100@v2".into(),
            ..upsert_100.source.clone()
        },
        content: "周三下午四点执行灰度发布窗口".into(),
        op: PublicIngestOp::Supersede {
            previous_external_id: "msg_100".into(),
        },
        occurred_at: EpochMs(1_200),
        ..upsert_100.clone()
    };
    let sup_out =
        store.apply_public_ingest_record(&supersede_100, &owner, &ModelRoutePolicy::HostCli)?;
    assert_eq!(sup_out.kind, PublicRecordAppliedKind::Superseded);
    assert_eq!(sup_out.superseded_source_id, Some(sid_100));
    assert_eq!(sup_out.affected_facts, vec![extracted_100.id.clone()]);

    // Old chunk ("两点") and old fact (`fact_gray_100`) are immediately hidden, new chunk ("四点") is live,
    // and community outbox has a pending `'withdraw'`!
    assert!(store.search_chunks("两点", &alice)?.is_empty());
    assert_eq!(store.search_chunks("四点", &alice)?.len(), 1);
    assert!(store.visible_facts(&alice, EpochMs(1_250))?.is_empty());
    assert_eq!(store.daemon_pending_outbox_count()?, 1);

    // 4. `Withdraw` `msg_100@v2` cancels its pending extraction job and hides its chunk!
    let withdraw_v2 = PublicIngestRecord {
        source: PublicIngestSource {
            external_id: "msg_100@v2".into(),
            ..upsert_100.source.clone()
        },
        content: String::new(),
        op: PublicIngestOp::Withdraw,
        occurred_at: EpochMs(1_300),
        ..upsert_100
    };
    let wd_out =
        store.apply_public_ingest_record(&withdraw_v2, &owner, &ModelRoutePolicy::HostCli)?;
    assert_eq!(wd_out.kind, PublicRecordAppliedKind::Withdrawn);
    assert!(store.search_chunks("四点", &alice)?.is_empty());
    assert!(store
        .lease_next_extraction_job(EpochMs(1_400), 60_000)?
        .is_none());
    Ok(())
}

#[test]
fn m2_r1_public_ingest_authorization_anti_rollback_redaction_and_policy_narrowing() -> Result<()> {
    let (dir, mut store) = library(LibraryMode::Community)?;
    let owner: ActorId = "owner".parse()?;
    let maint: ActorId = "maint".parse()?;
    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;
    let outsider: ActorId = "outsider".parse()?;
    store.add_member(maint.clone(), MemberRole::Maintainer, AT)?;
    store.add_member(alice.clone(), MemberRole::Member, AT)?;
    store.add_member(bob.clone(), MemberRole::Member, AT)?;

    // 1. Only Owner | Maintainer can apply rosters; label is redacted;
    //    replay `1000:[alice,bob] -> 2000:[alice] -> 1000:[alice,bob]` is rejected (`Conflict`)!
    let r_1000 = PublicContainerRoster {
        container_id: "scope.team_alpha".into(),
        label: "协作组 api_key=rostersecret777".into(),
        members: vec!["alice".into(), "bob".into()],
        all_library_members: false,
        synced_at: EpochMs(1_000),
        provider: Some("ext_chat".into()),
        upstream_container_id: Some("room_alpha".into()),
    };
    assert!(matches!(
        store.apply_public_container_roster(&bob, &r_1000),
        Err(StoreError::PermissionDenied)
    ));
    assert!(matches!(
        store.apply_public_container_roster(&outsider, &r_1000),
        Err(StoreError::PermissionDenied)
    ));
    let applied_1000 = store.apply_public_container_roster(&maint, &r_1000)?;
    assert!(applied_1000.label.contains("[REDACTED]"));
    assert!(!applied_1000.label.contains("rostersecret777"));

    // Same `synced_at` with identical content is idempotent; conflicting members at same `synced_at` fails!
    let _ = store.apply_public_container_roster(&owner, &r_1000)?;
    let mut r_1000_conflict = r_1000.clone();
    r_1000_conflict.members = vec!["alice".into()];
    assert!(matches!(
        store.apply_public_container_roster(&owner, &r_1000_conflict),
        Err(StoreError::Conflict)
    ));

    // Advance to `2000:[alice]` (bob leaves), then verify stale `1000:[alice,bob]` replay is rejected!
    let mut r_2000 = r_1000.clone();
    r_2000.members = vec!["alice".into()];
    r_2000.synced_at = EpochMs(2_000);
    let _ = store.apply_public_container_roster(&owner, &r_2000)?;
    assert!(matches!(
        store.apply_public_container_roster(&owner, &r_1000),
        Err(StoreError::Conflict)
    ));
    let cur_scope = store
        .source_scope(&"scope.team_alpha".parse()?)?
        .ok_or(StoreError::NotFound)?;
    assert_eq!(
        cur_scope.current_audience,
        AudienceSpec::Explicit(BTreeSet::from([alice.clone()]))
    );

    // 2. Cross-speaker & `ingested_by` & container membership checks for ordinary `Member`.
    // `bob` cannot spoof `ingested_by: owner`, cannot post into `scope.team_alpha` after removal,
    // and cannot forge `speaker: alice` outside a shared container!
    let bob_forged_ingester = PublicIngestRecord {
        source: PublicIngestSource {
            kind: "note".into(),
            uri: "note://b1".into(),
            external_id: "b1".into(),
            container_id: None,
        },
        speaker: PublicSpeaker::new("bob", None),
        audience: PublicAudience::Explicit(vec![bob.clone()]),
        occurred_at: EpochMs(2_100),
        content: "bob note".into(),
        op: PublicIngestOp::Upsert,
        ingested_by: Some(owner.clone()),
        media_type: None,
        model_policy: None,
    };
    assert!(matches!(
        store.apply_public_ingest_record(&bob_forged_ingester, &bob, &ModelRoutePolicy::HostCli),
        Err(StoreError::PermissionDenied)
    ));

    let bob_forged_speaker = PublicIngestRecord {
        ingested_by: None,
        speaker: PublicSpeaker::new("alice", None),
        ..bob_forged_ingester.clone()
    };
    assert!(matches!(
        store.apply_public_ingest_record(&bob_forged_speaker, &bob, &ModelRoutePolicy::HostCli),
        Err(StoreError::PermissionDenied)
    ));

    let bob_removed_container = PublicIngestRecord {
        source: PublicIngestSource {
            kind: "ext_chat".into(),
            uri: "chat://room_alpha/m_unauth".into(),
            external_id: "m_unauth".into(),
            container_id: Some("scope.team_alpha".into()),
        },
        speaker: PublicSpeaker::new("bob", None),
        audience: PublicAudience::ContainerMembers,
        occurred_at: EpochMs(2_150),
        content: "unauthorized container write".into(),
        op: PublicIngestOp::Upsert,
        ingested_by: None,
        media_type: None,
        model_policy: None,
    };
    assert!(matches!(
        store.apply_public_ingest_record(&bob_removed_container, &bob, &ModelRoutePolicy::HostCli),
        Err(StoreError::PermissionDenied)
    ));

    // `alice` (active member of `scope.team_alpha`) CAN record a message in `scope.team_alpha`
    // AND secrets in `content` and `speaker.display_name` are scrubbed before hitting SQLite (#4)!
    let alice_msg = PublicIngestRecord {
        source: PublicIngestSource {
            kind: "ext_chat".into(),
            uri: "chat://room_alpha/m_1".into(),
            external_id: "m_1".into(),
            container_id: Some("scope.team_alpha".into()),
        },
        speaker: PublicSpeaker::new(
            "ext_chat:u_peer",
            Some("Peer sk-1234567890abcdefghijklmnop".into()),
        ),
        audience: PublicAudience::ContainerMembers,
        occurred_at: EpochMs(2_200),
        content: "构建流水线令牌 ghp_1234567890abcdefghijklmnopqrstuv 已轮换".into(),
        op: PublicIngestOp::Upsert,
        ingested_by: None,
        media_type: None,
        model_policy: None,
    };
    let alice_out =
        store.apply_public_ingest_record(&alice_msg, &alice, &ModelRoutePolicy::HostCli)?;
    let alice_sid = alice_out.source_id.ok_or(StoreError::NotFound)?;
    let stored_txt = store
        .raw_source_text(&alice_sid)?
        .ok_or(StoreError::NotFound)?;
    assert!(stored_txt.contains("[REDACTED]"));
    assert_eq!(
        crate::find_sqlite_table_containing_bytes(
            dir.path(),
            b"ghp_1234567890abcdefghijklmnopqrstuv"
        )?,
        None
    );
    assert_eq!(
        crate::find_sqlite_table_containing_bytes(dir.path(), b"sk-1234567890abcdefghijklmnop")?,
        None
    );
    assert_eq!(
        crate::find_sqlite_table_containing_bytes(dir.path(), b"rostersecret777")?,
        None
    );

    // `bob` (even after re-joining container at t=2300) CANNOT withdraw or supersede `alice`'s source!
    let mut r_2300 = r_2000;
    r_2300.members = vec!["alice".into(), "bob".into()];
    r_2300.synced_at = EpochMs(2_300);
    let _ = store.apply_public_container_roster(&owner, &r_2300)?;

    let bob_steal_withdraw = PublicIngestRecord {
        content: String::new(),
        op: PublicIngestOp::Withdraw,
        occurred_at: EpochMs(2_350),
        speaker: PublicSpeaker::new("bob", None),
        ..alice_msg.clone()
    };
    assert!(matches!(
        store.apply_public_ingest_record(&bob_steal_withdraw, &bob, &ModelRoutePolicy::HostCli),
        Err(StoreError::PermissionDenied)
    ));
    let bob_steal_supersede = PublicIngestRecord {
        source: PublicIngestSource {
            external_id: "m_1_hijack".into(),
            ..alice_msg.source.clone()
        },
        speaker: PublicSpeaker::new("bob", None),
        content: "hijacked".into(),
        op: PublicIngestOp::Supersede {
            previous_external_id: "m_1".into(),
        },
        occurred_at: EpochMs(2_360),
        ..alice_msg.clone()
    };
    assert!(matches!(
        store.apply_public_ingest_record(&bob_steal_supersede, &bob, &ModelRoutePolicy::HostCli),
        Err(StoreError::PermissionDenied)
    ));

    // `alice` (original ingester) CAN withdraw `m_1`!
    let alice_wd = PublicIngestRecord {
        speaker: PublicSpeaker::new("alice", None),
        ..bob_steal_withdraw
    };
    assert_eq!(
        store
            .apply_public_ingest_record(&alice_wd, &alice, &ModelRoutePolicy::HostCli)?
            .kind,
        PublicRecordAppliedKind::Withdrawn
    );

    // 3. Duplicate `Upsert` policy narrowing (`HostCli` -> `RawOnly`) cancels
    //    pending job & purges cloud `chunk_vectors`, while conflicting explicit policies fail (`Conflict`)!
    let cloud_emb = Arc::new(CountingEmbedder::new(
        "dashscope:text-embedding-v3",
        64,
        false,
    ));
    let _ = store.reembed_stale(
        cloud_emb.as_ref(),
        Some(("dashscope:text-embedding-v3", 64)),
    )?;
    store.set_embedder(cloud_emb.clone());

    let p_rec = PublicIngestRecord {
        source: PublicIngestSource {
            kind: "doc".into(),
            uri: "doc://spec-6".into(),
            external_id: "spec-6".into(),
            container_id: None,
        },
        speaker: PublicSpeaker::new("owner", None),
        audience: PublicAudience::Explicit(vec![owner.clone()]),
        occurred_at: EpochMs(3_000),
        content: "架构策略收敛验证文档".into(),
        op: PublicIngestOp::Upsert,
        ingested_by: None,
        media_type: None,
        model_policy: Some(ModelRoutePolicy::NamedEndpoint("ep-alpha".into())),
    };
    let p_out = store.apply_public_ingest_record(&p_rec, &owner, &ModelRoutePolicy::HostCli)?;
    let p_sid = p_out.source_id.ok_or(StoreError::NotFound)?;
    assert!(p_out.enqueued_job_id.is_some());

    // Conflicting `NamedEndpoint("ep-beta")` on same key/content is rejected!
    let mut p_conflict = p_rec.clone();
    p_conflict.model_policy = Some(ModelRoutePolicy::NamedEndpoint("ep-beta".into()));
    assert!(matches!(
        store.apply_public_ingest_record(&p_conflict, &owner, &ModelRoutePolicy::HostCli),
        Err(StoreError::Conflict)
    ));

    // Narrowing to `RawOnly` on same key/content atomically marks `raw_only_source`, cancels the
    // pending extraction job, and deletes the cloud `chunk_vectors` row!
    let mut p_narrow = p_rec.clone();
    p_narrow.model_policy = Some(ModelRoutePolicy::RawOnly);
    let narrow_out =
        store.apply_public_ingest_record(&p_narrow, &owner, &ModelRoutePolicy::HostCli)?;
    assert_eq!(
        narrow_out.kind,
        PublicRecordAppliedKind::DuplicateIdempotent
    );
    assert!(narrow_out.enqueued_job_id.is_none());
    assert!(crate::facts::is_source_marked_raw_only(
        &store.db,
        p_sid.as_str()
    )?);
    let remaining_vecs: i64 = store.db.query_row(
        "SELECT count(*) FROM chunk_vectors WHERE chunk_id = ?1",
        [format!("{p_sid}-0")],
        |r| r.get(0),
    )?;
    assert_eq!(remaining_vecs, 0);
    let pending_jobs: i64 = store.db.query_row(
        "SELECT count(*) FROM extraction_queue WHERE source_id = ?1 AND status != 'done'",
        [p_sid.as_str()],
        |r| r.get(0),
    )?;
    assert_eq!(pending_jobs, 0);

    // Attempting to widen `RawOnly` -> `HostCli` on the same key without `supersede` fails (`Conflict`)!
    let mut p_widen = p_rec;
    p_widen.model_policy = Some(ModelRoutePolicy::HostCli);
    assert!(matches!(
        store.apply_public_ingest_record(&p_widen, &owner, &ModelRoutePolicy::HostCli),
        Err(StoreError::Conflict)
    ));

    Ok(())
}

#[test]
fn m2_r1_public_session_subbatch_shares_identity_enforces_raw_only_cloud_gate_and_withdraws(
) -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::init_with_embedding(
        dir.path(),
        options(LibraryMode::Personal)?,
        "dashscope:text-embedding-v3",
        64,
    )?;
    let owner: ActorId = "owner".parse()?;
    let cloud_emb = Arc::new(CountingEmbedder::new(
        "dashscope:text-embedding-v3",
        64,
        false,
    ));
    store.set_embedder(cloud_emb.clone());

    let _ = store.daemon_record_session_cursor(&crate::SessionCursorUpdate {
        session_id: "sess-42",
        client_kind: "codex",
        transcript_path: None,
        byte_offset: 512,
        line_count: 4,
        prefix_sha256: "",
        prev_byte_offset: None,
        prev_prefix_sha256: None,
        actor: Some(&owner),
        session_ended: false,
        at: AT,
    })?;

    // 1. `RawOnly` session sub-batch: zero cloud embedder calls, zero extraction jobs, advances cursor!
    let sub_raw = PublicIngestRecord {
        source: PublicIngestSource {
            kind: "session".into(),
            uri: "session:codex:sess-42:0-256".into(),
            external_id: "codex:sess-42:0-256".into(),
            container_id: None,
        },
        speaker: PublicSpeaker::new("owner", None),
        audience: PublicAudience::Explicit(vec![owner.clone()]),
        occurred_at: AT,
        content: "[turn:1 | 2026-11-23 | speaker:owner]\n敏感本机对话 api_key=sesssecret111".into(),
        op: PublicIngestOp::Upsert,
        ingested_by: None,
        media_type: Some("text/plain".into()),
        model_policy: None,
    };
    let (stored_raw, newly) =
        store.ingest_public_session_subbatch(&crate::PublicSessionSubbatchSpec {
            caller: &owner,
            record: &sub_raw,
            global_policy: &ModelRoutePolicy::RawOnly,
            session_id: "sess-42",
            start_byte: 0,
            end_byte: 256,
            job_priority: 10,
            self_echo_filtered: true,
            at: AT,
        })?;
    assert!(newly);
    assert_eq!(cloud_emb.call_count(), 0);
    assert!(store.lease_next_extraction_job(AT, 60_000)?.is_none());
    let cur = store
        .get_session_cursor("sess-42")?
        .ok_or(StoreError::NotFound)?;
    assert_eq!(cur.last_extracted_offset, 256);
    assert_eq!(
        crate::find_sqlite_table_containing_bytes(dir.path(), b"sesssecret111")?,
        None
    );

    // Also verify `Store::ingest_source_with_policy(..., RawOnly)` never calls cloud `Embedder`!
    let lib_id = store.library_id().clone();
    let _ = store.ingest_source_with_policy(
        &lib_id,
        &"raw_doc_policy".parse()?,
        &source(&["owner"])?,
        &ModelRoutePolicy::RawOnly,
    )?;
    assert_eq!(cloud_emb.call_count(), 0);

    // 2. Public `withdraw` on `{ kind: "session", external_id: "codex:sess-42:0-256" }` retires the exact source!
    let wd_sess = PublicIngestRecord {
        content: String::new(),
        op: PublicIngestOp::Withdraw,
        occurred_at: EpochMs(AT.0 + 100),
        ..sub_raw
    };
    let wd_out = store.apply_public_ingest_record(&wd_sess, &owner, &ModelRoutePolicy::RawOnly)?;
    assert_eq!(wd_out.kind, PublicRecordAppliedKind::Withdrawn);
    assert_eq!(wd_out.source_id, Some(stored_raw.source_id));
    Ok(())
}

#[test]
fn m2_r1_m1_v1_database_and_raw_git_migrate_smoothly_to_v2_and_purge_zero_residue() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let lib_path = dir.path().join("legacy_m1");
    std::fs::create_dir_all(lib_path.join("raw"))?;

    // Construct an authentic M1 `user_version == 1` SQLite database (no `embedding_model`, no
    // `model_id` columns, WITH `connector_cursors` table, and empty `raw_blobs`).
    let m1_ddl = crate::migrations::DDL
        .replace(
            ",\n    embedding_model TEXT NOT NULL DEFAULT 'hash:v1',\n    embedding_dims INTEGER NOT NULL DEFAULT 256 CHECK(embedding_dims >= 0)",
            "",
        )
        .replace(
            ",\n    model_id TEXT NOT NULL DEFAULT 'hash:v1'",
            "",
        );
    let m1_lib_id: kp_model::LibraryId = "m1-lib".parse()?;
    let sid_wt = crate::tombstone_id(b"m1-lib", &m1_lib_id, &"wt-doc".parse()?)?.to_string();
    let sid_git = crate::tombstone_id(b"m1-lib", &m1_lib_id, &"git-doc".parse()?)?.to_string();
    let payload_wt = b"LEGACY_M1_WORKTREE_SECRET_PAYLOAD\n";
    let payload_git = b"LEGACY_M1_GIT_COMMIT_ONLY_PAYLOAD\n";
    let sha_wt = kp_model::ContentHash::digest(payload_wt);
    let sha_git = kp_model::ContentHash::digest(payload_git);
    let legacy_schema_hash = kp_model::ContentHash::digest(b"m1_schema_v1");

    // Write `sid_wt.md` on disk AND commit `sid_git.md` into an isolated `raw/.git` repository
    // (deleting `sid_git.md` from working tree so ONLY `git show HEAD:{sid_git}.md` can recover it!).
    let raw_dir = lib_path.join("raw");
    let git_ok = std::process::Command::new("git")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .arg("-C")
        .arg(&raw_dir)
        .args(["init", "-q"])
        .status()
        .is_ok_and(|s| s.success());
    std::fs::write(raw_dir.join(format!("{sid_wt}.md")), payload_wt)?;
    std::fs::write(raw_dir.join(format!("{sid_git}.md")), payload_git)?;
    if git_ok {
        let _ = std::process::Command::new("git")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .arg("-C")
            .arg(&raw_dir)
            .args([
                "-c",
                "user.name=kp",
                "-c",
                "user.email=kp@local",
                "add",
                &format!("{sid_git}.md"),
            ])
            .status()?;
        let committed = std::process::Command::new("git")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .arg("-C")
            .arg(&raw_dir)
            .args([
                "-c",
                "user.name=kp",
                "-c",
                "user.email=kp@local",
                "commit",
                "-q",
                "-m",
                "m1 raw",
            ])
            .status()
            .is_ok_and(|s| s.success());
        if committed {
            std::fs::remove_file(raw_dir.join(format!("{sid_git}.md")))?;
        }
    }

    {
        let db = rusqlite::Connection::open(lib_path.join("kp.sqlite"))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        db.execute_batch(&m1_ddl)?;
        db.execute_batch(
            "CREATE TABLE connector_cursors (
                connector_id TEXT NOT NULL,
                scope_id TEXT NOT NULL,
                PRIMARY KEY(connector_id, scope_id)
            );",
        )?;
        db.execute(
            "INSERT INTO library_meta (library_id, name, mode, owner_actor, schema_ir_hash, created_at)
             VALUES ('m1-lib', 'Legacy M1', 'personal', 'owner', ?1, 1000)",
            [legacy_schema_hash.as_str()],
        )?;
        db.execute(
            "INSERT INTO members_tokens (actor_id, display_name, role, created_at)
             VALUES ('owner', 'owner', 'owner', 1000)",
            [],
        )?;
        for (sid, sha, key_name, plen) in [
            (&sid_wt, &sha_wt, "wt-doc", payload_wt.len() as i64),
            (&sid_git, &sha_git, "git-doc", payload_git.len() as i64),
        ] {
            db.execute(
                "INSERT INTO raw_sources
                    (source_id, uri, media_type, content_sha256, git_blob_or_commit, byte_len, visibility_scope, captured_at, self_echo_filtered)
                 VALUES (?1, 'file://legacy.md', 'text/markdown', ?2, 'HEAD', ?3, '\"*\"', 1000, 0)",
                rusqlite::params![sid, sha.as_str(), plen],
            )?;
            db.execute(
                "INSERT INTO change_log (op, target_id, origin_library_id, logical_key, content_sha256, created_at)
                 VALUES ('ingest_source', ?1, 'm1-lib', ?2, ?3, 1000)",
                rusqlite::params![sid, key_name, sha.as_str()],
            )?;
        }
        db.pragma_update(None, "user_version", 1_i64)?;
    }

    // 1. `Store::open_readonly` works on unmigrated `user_version == 1` DB!
    {
        let ro = Store::open_readonly(&lib_path)?;
        assert_eq!(ro.status()?.embedding_model, "hash:v1");
        assert_eq!(ro.status()?.embedding_dims, 256);
        assert_eq!(
            ro.raw_source_bytes(&kp_model::SourceId::new(&sid_wt)?)?
                .as_deref(),
            Some(payload_wt.as_slice())
        );
        assert_eq!(
            ro.raw_source_bytes(&kp_model::SourceId::new(&sid_git)?)?
                .as_deref(),
            Some(payload_git.as_slice())
        );
        let _ = ro.retrieval_snapshot(&"owner".parse()?, AT)?;
    }

    // 2. `Store::open` upgrades `v1 -> v2`, migrates both files + `.git` objects into `raw_blobs`,
    //    drops `connector_cursors`, and removes `<lib>/raw`!
    let mut store = Store::open(&lib_path)?;
    assert!(!lib_path.join("raw").exists());
    assert_eq!(
        store
            .db
            .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))?,
        2
    );
    let has_cc: bool = store.db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'connector_cursors')",
        [],
        |r| r.get(0),
    )?;
    assert!(!has_cc);
    assert_eq!(
        store
            .raw_source_bytes(&kp_model::SourceId::new(&sid_wt)?)?
            .as_deref(),
        Some(payload_wt.as_slice())
    );
    assert_eq!(
        store
            .raw_source_bytes(&kp_model::SourceId::new(&sid_git)?)?
            .as_deref(),
        Some(payload_git.as_slice())
    );

    // 3. Even if a stray `raw/` file reappears, `purge_target` erases both `raw_blobs` and `raw/`!
    std::fs::create_dir_all(lib_path.join("raw"))?;
    std::fs::write(
        lib_path.join("raw").join(format!("{sid_wt}.md")),
        payload_wt,
    )?;
    store.purge_target(&"m1-lib".parse()?, &sid_wt, AT, "gdpr")?;
    store.purge_target(&"m1-lib".parse()?, &sid_git, AT, "gdpr")?;
    assert!(!lib_path.join("raw").exists());
    assert_eq!(
        crate::find_sqlite_table_containing_bytes(&lib_path, b"LEGACY_M1_WORKTREE_SECRET_PAYLOAD")?,
        None
    );
    assert_eq!(
        crate::find_sqlite_table_containing_bytes(&lib_path, b"LEGACY_M1_GIT_COMMIT_ONLY_PAYLOAD")?,
        None
    );
    Ok(())
}

struct ConcurrentMutatingEmbedder {
    lib_path: std::path::PathBuf,
}

impl Embedder for ConcurrentMutatingEmbedder {
    fn embed(&self, _text: &str) -> Result<Vec<f32>> {
        let mut v = vec![0.0_f32; 64];
        v[0] = 1.0;
        Ok(v)
    }

    fn model_id(&self) -> &str {
        "dashscope:text-embedding-v3"
    }

    fn expected_dims(&self) -> Option<usize> {
        Some(64)
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        // Simulate another concurrent process migrating `library_meta` to `voyage:voyage-3` (`64d`)
        // while this `reembed_stale` is computing vectors outside the SQLite write lock!
        let mut rival = Store::open(&self.lib_path)?;
        let rival_emb = CountingEmbedder::new("voyage:voyage-3", 64, false);
        let _ = rival.reembed_stale(&rival_emb, Some(("voyage:voyage-3", 64)))?;
        texts.iter().map(|t| self.embed(t)).collect()
    }
}

#[test]
fn m2_r1_model_identity_guard_reembed_generation_conflict_and_export_atomicity() -> Result<()> {
    let (dir, mut store) = library(LibraryMode::Personal)?;
    let owner: ActorId = "owner".parse()?;

    // 1. Same dimension (`64d`), different model (`dashscope` vs `voyage`) is rejected!
    let dash_64 = CountingEmbedder::new("dashscope:text-embedding-v3", 64, true);
    let voyage_64 = CountingEmbedder::new("voyage:voyage-3", 64, true);
    let _ = store.reembed_stale(&dash_64, Some(("dashscope:text-embedding-v3", 64)))?;
    let model_mismatch_err = store
        .ingest_source_with_embedder(
            &"local".parse()?,
            &"wrong_model_same_dim".parse()?,
            &source(&["owner"])?,
            &voyage_64,
        )
        .err()
        .ok_or(StoreError::NotFound)?
        .to_string();
    assert!(model_mismatch_err.contains("embedding model mismatch"));
    assert!(model_mismatch_err.contains("kp embed --stale --to voyage:voyage-3 --dim 64"));

    // 2. Concurrent `library_meta` migration mid-`reembed_stale` returns `Conflict`!
    let _ = store.ingest_source_with_embedder(
        &"local".parse()?,
        &"doc_race".parse()?,
        &source(&["owner"])?,
        &FtsOnlyEmbedder,
    )?;
    let racing_emb = ConcurrentMutatingEmbedder {
        lib_path: dir.path().to_path_buf(),
    };
    assert!(matches!(
        store.reembed_stale(&racing_emb, None),
        Err(StoreError::Conflict)
    ));
    assert_eq!(
        store.embedding_config()?,
        ("voyage:voyage-3".to_owned(), 64)
    );

    // 3. Export failure BEFORE commit leaves zero ghost `projection_pages` rows,
    //    and successful export leaves zero `.kp-export.lock` or `.tmp` files!
    let blocked_target = dir.path().join("file_not_dir");
    std::fs::write(&blocked_target, b"not a directory")?;
    assert!(store
        .export_markdown_directory(&blocked_target, &owner, AT)
        .is_err());
    let ghost_pages: i64 =
        store
            .db
            .query_row("SELECT count(*) FROM projection_pages", [], |r| r.get(0))?;
    assert_eq!(ghost_pages, 0);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let ro_dir = dir.path().join("readonly_export");
        std::fs::create_dir_all(&ro_dir)?;
        std::fs::set_permissions(&ro_dir, std::fs::Permissions::from_mode(0o555))?;
        let ro_res = store.export_markdown_directory(&ro_dir, &owner, AT);
        let _ = std::fs::set_permissions(&ro_dir, std::fs::Permissions::from_mode(0o755));
        // Unless running as root (uid 0), writing into `0555` fails and leaves 0 `projection_pages`.
        if ro_res.is_err() {
            let pages_after_ro: i64 =
                store
                    .db
                    .query_row("SELECT count(*) FROM projection_pages", [], |r| r.get(0))?;
            assert_eq!(pages_after_ro, 0);
        }
    }

    let clean_out = dir.path().join("clean_export");
    let rep = store.export_markdown_directory(&clean_out, &owner, AT)?;
    assert_eq!(rep.pages.len(), 2);
    assert!(!clean_out.join(".kp-export.lock").exists());
    for entry in std::fs::read_dir(&clean_out)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        assert!(
            name == "index.md" || name == "sources.md",
            "unexpected leftover file in export dir: {name}"
        );
    }
    Ok(())
}

#[test]
fn m2_r2_duplicate_upsert_spoofing_supersede_preauth_named_endpoint_and_leased_raw_only(
) -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::init_with_embedding(
        dir.path(),
        options(LibraryMode::Community)?,
        "dashscope:text-embedding-v3",
        64,
    )?;
    let owner: ActorId = "owner".parse()?;
    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;
    store.add_member(alice.clone(), MemberRole::Member, AT)?;
    store.add_member(bob.clone(), MemberRole::Member, AT)?;

    let cloud_emb = Arc::new(CountingEmbedder::new(
        "dashscope:text-embedding-v3",
        64,
        false,
    ));
    store.set_embedder(cloud_emb.clone());

    let roster = PublicContainerRoster {
        container_id: "scope.shared".into(),
        label: "Shared Room".into(),
        members: vec!["alice".into(), "bob".into()],
        all_library_members: false,
        synced_at: EpochMs(1_000),
        provider: Some("ext_chat".into()),
        upstream_container_id: Some("room_shared".into()),
    };
    let _ = store.apply_public_container_roster(&owner, &roster)?;

    // 1. R2 Finding 1: Alice writes source S; Bob in same container tries to spoof ownership via
    //    duplicate `Upsert` (`speaker = bob`) and then `Withdraw` S -> blocked at every step!
    let alice_rec = PublicIngestRecord {
        source: PublicIngestSource {
            kind: "ext_chat".into(),
            uri: "chat://room_shared/msg-s1".into(),
            external_id: "msg-s1".into(),
            container_id: Some("scope.shared".into()),
        },
        speaker: PublicSpeaker::new("alice", None),
        audience: PublicAudience::ContainerMembers,
        occurred_at: EpochMs(1_100),
        content: "生产数据库只读副本扩容计划".into(),
        op: PublicIngestOp::Upsert,
        ingested_by: None,
        media_type: None,
        model_policy: None,
    };
    let s1_out =
        store.apply_public_ingest_record(&alice_rec, &alice, &ModelRoutePolicy::HostCli)?;
    let s1_id = s1_out.source_id.ok_or(StoreError::NotFound)?;

    // Bob spoofing `speaker = bob` on same key & content -> `Conflict`!
    let mut bob_spoof_spk = alice_rec.clone();
    bob_spoof_spk.speaker = PublicSpeaker::new("bob", None);
    assert!(matches!(
        store.apply_public_ingest_record(&bob_spoof_spk, &bob, &ModelRoutePolicy::HostCli),
        Err(StoreError::Conflict)
    ));

    // Bob trying to narrow/alter `model_policy` on Alice's source -> `PermissionDenied`!
    let mut bob_spoof_pol = alice_rec.clone();
    bob_spoof_pol.model_policy = Some(ModelRoutePolicy::RawOnly);
    assert!(matches!(
        store.apply_public_ingest_record(&bob_spoof_pol, &bob, &ModelRoutePolicy::HostCli),
        Err(StoreError::PermissionDenied)
    ));

    // Benign identical replay by Bob (`speaker = alice`, `model_policy = None`) writes 0 jobs,
    // and Bob STILL cannot `Withdraw` Alice's source S!
    let bob_replay =
        store.apply_public_ingest_record(&alice_rec, &bob, &ModelRoutePolicy::HostCli)?;
    assert_eq!(
        bob_replay.kind,
        PublicRecordAppliedKind::DuplicateIdempotent
    );
    assert!(bob_replay.enqueued_job_id.is_none());

    let bob_withdraw = PublicIngestRecord {
        content: String::new(),
        op: PublicIngestOp::Withdraw,
        occurred_at: EpochMs(1_200),
        speaker: PublicSpeaker::new("bob", None),
        ..alice_rec.clone()
    };
    assert!(matches!(
        store.apply_public_ingest_record(&bob_withdraw, &bob, &ModelRoutePolicy::HostCli),
        Err(StoreError::PermissionDenied)
    ));
    assert_eq!(store.search_chunks("只读副本扩容", &alice)?.len(), 1);

    // 2. R2 Finding 2: Unauthorized `Supersede` fails BEFORE calling external `Embedder`!
    let calls_before = cloud_emb.call_count();
    let bob_supersede = PublicIngestRecord {
        source: PublicIngestSource {
            external_id: "msg-s1-v2".into(),
            ..alice_rec.source.clone()
        },
        speaker: PublicSpeaker::new("bob", None),
        content: "越权取代内容不应发往云端向量接口".into(),
        op: PublicIngestOp::Supersede {
            previous_external_id: "msg-s1".into(),
        },
        occurred_at: EpochMs(1_300),
        ..alice_rec.clone()
    };
    assert!(matches!(
        store.apply_public_ingest_record(&bob_supersede, &bob, &ModelRoutePolicy::HostCli),
        Err(StoreError::PermissionDenied)
    ));
    assert_eq!(
        cloud_emb.call_count(),
        calls_before,
        "unauthorized Supersede must fail before calling external Embedder"
    );

    // 3. R2 Findings 3 & 4: Unambiguous `named_endpoint:host` queue encoding & effective policy queries!
    let ep_host_rec = PublicIngestRecord {
        source: PublicIngestSource {
            kind: "doc".into(),
            uri: "doc://ep-host".into(),
            external_id: "ep-host".into(),
            container_id: None,
        },
        speaker: PublicSpeaker::new("owner", None),
        audience: PublicAudience::Explicit(vec![owner.clone()]),
        occurred_at: EpochMs(1_400),
        content: "企业网关端点名称刚好叫 host 的边界测试".into(),
        op: PublicIngestOp::Upsert,
        ingested_by: None,
        media_type: None,
        model_policy: Some(ModelRoutePolicy::NamedEndpoint("host".into())),
    };
    let ep_out =
        store.apply_public_ingest_record(&ep_host_rec, &owner, &ModelRoutePolicy::HostCli)?;
    let ep_sid = ep_out.source_id.ok_or(StoreError::NotFound)?;
    let ep_job_id = ep_out.enqueued_job_id.ok_or(StoreError::NotFound)?;
    let ep_job = store
        .get_extraction_job(&ep_job_id)?
        .ok_or(StoreError::NotFound)?;
    assert_eq!(ep_job.model_policy, "named_endpoint_v2:host");
    assert_eq!(
        crate::decode_queue_model_policy(&ep_job.model_policy)?,
        ModelRoutePolicy::NamedEndpoint("host".into())
    );
    assert_eq!(
        crate::decode_queue_model_policy("host")?,
        ModelRoutePolicy::HostCli
    );
    assert_eq!(
        store.source_effective_route_policy(&ep_sid)?,
        ModelRoutePolicy::NamedEndpoint("host".into())
    );

    // Attach a fact to `s1_id` (`HostCli` initially) and a fact with explicit `object.route_policy`!
    let mut f_s1 = fact("fact_on_s1", &["alice", "bob"])?;
    f_s1.author = alice.clone();
    f_s1.source_anchor = Some(AnchorSpec {
        source_id: Some(s1_id.clone()),
        ..AnchorSpec::default()
    });
    store.insert_fact(&f_s1)?;
    assert_eq!(
        store.fact_effective_route_policy(&f_s1)?,
        ModelRoutePolicy::HostCli
    );

    let mut f_tagged = fact("fact_tagged_ep", &["owner"])?;
    f_tagged.object = json!({
        "claim": "专线路由事实",
        "route_policy": ModelRoutePolicy::NamedEndpoint("corp-gw".into()),
    });
    store.insert_fact(&f_tagged)?;
    assert_eq!(
        store.fact_effective_route_policy(&f_tagged)?,
        ModelRoutePolicy::NamedEndpoint("corp-gw".into())
    );

    // 4. R2 Finding 5: Lease Alice's job (`s1_id`), then Alice narrows `msg-s1` to `RawOnly`!
    //    - Both `chunk_vectors` and `fact_vectors` on `s1_id` / `f_s1` are deleted immediately;
    //    - `commit_staged_extraction_job_fenced` on the old lease is rejected (`Conflict`);
    //    - After lease expiry, `lease_next_extraction_job` NEVER revives the cancelled job!
    let leased_s1 = store
        .lease_next_extraction_job(EpochMs(2_000), 5_000)?
        .ok_or(StoreError::NotFound)?;
    assert_eq!(leased_s1.source_id, s1_id);
    let fv_before: i64 = store.db.query_row(
        "SELECT count(*) FROM fact_vectors WHERE fact_id = ?1",
        [f_s1.id.as_str()],
        |r| r.get(0),
    )?;
    assert_eq!(fv_before, 1);

    let mut alice_narrow = alice_rec;
    alice_narrow.model_policy = Some(ModelRoutePolicy::RawOnly);
    let _ = store.apply_public_ingest_record(&alice_narrow, &alice, &ModelRoutePolicy::HostCli)?;
    assert_eq!(
        store.source_effective_route_policy(&s1_id)?,
        ModelRoutePolicy::RawOnly
    );
    assert_eq!(
        store.fact_effective_route_policy(&f_s1)?,
        ModelRoutePolicy::RawOnly
    );

    let cv_after: i64 = store.db.query_row(
        "SELECT count(*) FROM chunk_vectors WHERE chunk_id = ?1",
        [format!("{s1_id}-0")],
        |r| r.get(0),
    )?;
    let fv_after: i64 = store.db.query_row(
        "SELECT count(*) FROM fact_vectors WHERE fact_id = ?1",
        [f_s1.id.as_str()],
        |r| r.get(0),
    )?;
    assert_eq!(cv_after, 0);
    assert_eq!(fv_after, 0);

    assert!(matches!(
        store.commit_staged_extraction_job_fenced(&crate::FencedJobCommitSpec {
            job_id: &leased_s1.job_id,
            expected_attempts: leased_s1.attempts,
            lease_now: EpochMs(2_100),
            batch: &crate::StagedExtractionBatch::default(),
            chunk_hashes: &[],
            author: None,
            schema_ver: "",
            at: EpochMs(2_100),
        }),
        Err(StoreError::Conflict)
    ));

    // Drain the one remaining `ep_host_rec` job, then advance clock past `leased_s1`'s expiry (`10_000`):
    // `leased_s1` MUST NOT revive!
    let leased_ep = store
        .lease_next_extraction_job(EpochMs(10_000), 5_000)?
        .ok_or(StoreError::NotFound)?;
    assert_eq!(leased_ep.source_id, ep_sid);
    store.complete_extraction_job(&leased_ep.job_id)?;
    assert!(store
        .lease_next_extraction_job(EpochMs(20_000), 5_000)?
        .is_none());

    // Same-model `reembed_stale(&cloud_emb, None)` ALSO scrubs any residual cloud vectors on `RawOnly` rows!
    let dummy_vec = crate::embedding::encode_vector_le_bytes(&{
        let mut v = vec![0.0_f32; 64];
        v[0] = 1.0;
        v
    });
    store.db.execute(
        "INSERT INTO fact_vectors (fact_id, dims, vec_blob, model_id) VALUES (?1, 64, ?2, 'dashscope:text-embedding-v3')",
        rusqlite::params![f_s1.id.as_str(), dummy_vec],
    )?;
    let scrub_rep = store.reembed_stale(cloud_emb.as_ref(), None)?;
    assert!(scrub_rep.skipped_raw_only >= 2);
    let fv_scrubbed: i64 = store.db.query_row(
        "SELECT count(*) FROM fact_vectors WHERE fact_id = ?1",
        [f_s1.id.as_str()],
        |r| r.get(0),
    )?;
    assert_eq!(fv_scrubbed, 0);

    Ok(())
}

#[test]
fn m2_r2_v2_residual_raw_dir_recovery_and_export_pre_read_backup_preservation() -> Result<()> {
    let (dir, mut store) = library(LibraryMode::Personal)?;
    let owner: ActorId = "owner".parse()?;
    store.remember_facts(&[FactInput {
        fact: fact("f_export_v1", &["owner"])?,
        entities: BTreeSet::new(),
    }])?;

    // 1. R2 Finding 6: Simulate crash after v1->v2 commit leaving a residual `<lib>/raw` directory
    //    on an already-`v2` database -> next `Store::open` automatically finalizes & deletes `raw/`!
    drop(store);
    std::fs::create_dir_all(dir.path().join("raw/.git"))?;
    std::fs::write(dir.path().join("raw/stray.md"), b"residual")?;
    let mut reopened = Store::open(dir.path())?;
    assert!(!dir.path().join("raw").exists());

    // 2. R2 Finding 8: Export v1 baseline, then replace `sources.md` with a directory so reading
    //    prior backups fails -> `export_markdown_directory` errors BEFORE touching `index.md`!
    let export_dir = dir.path().join("exported");
    let rep1 = reopened.export_markdown_directory(&export_dir, &owner, AT)?;
    let index_v1_bytes = std::fs::read(export_dir.join("index.md"))?;

    let mut f2 = fact("f_export_v2", &["owner"])?;
    f2.object = json!("更新后的第二版事实内容");
    reopened.remember_facts(&[FactInput {
        fact: f2,
        entities: BTreeSet::new(),
    }])?;

    let sources_md_path = export_dir.join("sources.md");
    std::fs::remove_file(&sources_md_path)?;
    std::fs::create_dir(&sources_md_path)?;

    assert!(reopened
        .export_markdown_directory(&export_dir, &owner, EpochMs(AT.0 + 1_000))
        .is_err());
    assert_eq!(
        std::fs::read(export_dir.join("index.md"))?,
        index_v1_bytes,
        "`index.md` must remain byte-for-byte intact when reading `sources.md` backup fails"
    );
    let stored_base_seq: i64 = reopened.db.query_row(
        "SELECT base_seq FROM projection_pages WHERE page_slug = 'index' AND viewer_actor = 'owner'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(stored_base_seq, rep1.base_seq);
    Ok(())
}

#[test]
fn m2_r3_legacy_named_endpoint_queue_repair_idempotent_replay_and_derived_vector_purge(
) -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::init_with_embedding(
        dir.path(),
        options(LibraryMode::Personal)?,
        "openai:text-embedding-3-small",
        4,
    )?;
    let owner: ActorId = "owner".parse()?;
    let cloud_emb = Arc::new(CountingEmbedder::new(
        "openai:text-embedding-3-small",
        4,
        false,
    ));
    store.set_embedder(cloud_emb.clone());

    // 1. Legacy `NamedEndpoint("host")` row stored as bare `"host"` in `extraction_queue` while
    //    `public_source_meta` holds `NamedEndpoint("host")` -> `Store::open` repairs queue row to
    //    `"named_endpoint:host"`!
    let rec_named_host = PublicIngestRecord {
        source: PublicIngestSource {
            kind: "doc".into(),
            uri: "doc://r3/named-host".into(),
            external_id: "r3-named-host".into(),
            container_id: None,
        },
        speaker: PublicSpeaker::new("owner", None),
        audience: PublicAudience::Explicit(vec![owner.clone()]),
        occurred_at: AT,
        content: "旧队列 host 命名端点修复测试".into(),
        op: PublicIngestOp::Upsert,
        ingested_by: None,
        media_type: None,
        model_policy: Some(ModelRoutePolicy::NamedEndpoint("host".into())),
    };
    let out_host =
        store.apply_public_ingest_record(&rec_named_host, &owner, &ModelRoutePolicy::HostCli)?;
    let sid_host = out_host.source_id.ok_or(StoreError::NotFound)?;
    store.db.execute(
        "UPDATE extraction_queue SET model_policy = 'host' WHERE source_id = ?1",
        [sid_host.as_str()],
    )?;

    // 2. Legacy `NamedEndpoint("corp-gw")` row stored as bare `"corp-gw"` + legacy anchored fact
    //    WITHOUT `object.route_policy` -> idempotent replay succeeds (no false `Conflict`), queue
    //    row normalizes to `"named_endpoint:corp-gw"`, and `fact_effective_route_policy` returns
    //    `NamedEndpoint("corp-gw")`!
    let rec_corp = PublicIngestRecord {
        source: PublicIngestSource {
            kind: "doc".into(),
            uri: "doc://r3/corp".into(),
            external_id: "r3-corp".into(),
            container_id: None,
        },
        speaker: PublicSpeaker::new("owner", None),
        audience: PublicAudience::Explicit(vec![owner.clone()]),
        occurred_at: AT,
        content: "旧队列 corp-gw 幂等重放与派生向量擦除测试".into(),
        op: PublicIngestOp::Upsert,
        ingested_by: None,
        media_type: None,
        model_policy: Some(ModelRoutePolicy::NamedEndpoint("corp-gw".into())),
    };
    let out_corp =
        store.apply_public_ingest_record(&rec_corp, &owner, &ModelRoutePolicy::HostCli)?;
    let sid_corp = out_corp.source_id.ok_or(StoreError::NotFound)?;
    store.db.execute(
        "UPDATE extraction_queue SET model_policy = 'corp-gw' WHERE source_id = ?1",
        [sid_corp.as_str()],
    )?;

    let mut legacy_fact = fact("legacy_corp_fact", &["owner"])?;
    legacy_fact.source_anchor = Some(AnchorSpec {
        source_id: Some(sid_corp.clone()),
        chunk_id: Some(format!("{sid_corp}-0")),
        verbatim_quote: Some("旧队列 corp-gw 幂等重放与派生向量擦除测试".into()),
        ..AnchorSpec::default()
    });
    store.remember_facts(&[FactInput {
        fact: legacy_fact.clone(),
        entities: BTreeSet::new(),
    }])?;
    assert_eq!(
        store.fact_effective_route_policy(&legacy_fact)?,
        ModelRoutePolicy::NamedEndpoint("corp-gw".into())
    );

    // Also attach a derived conclusion (`legacy_corp_fact -> derived_chain_fact`) with a cloud `fact_vector`!
    let derived_chain = derived_fact(
        store.library_id(),
        &owner,
        FactKind::DerivedFact,
        "owner",
        "kp.core.assumed_conclusion",
        json!({"claim_text": "派生自来源的结论"}),
        AT,
    )?;
    let derived_id = derived_chain.id.clone();
    let just = justification(
        &derived_chain,
        DerivationMode::AssumedPremise,
        "NL_ASSUMED_PREMISE",
        &[&legacy_fact],
    )?;
    store.save_derivations(&[derived_chain], &[just])?;
    let _ = store.reembed_stale(cloud_emb.as_ref(), None)?;
    let vec_before: i64 = store.db.query_row(
        "SELECT COUNT(*) FROM fact_vectors WHERE fact_id IN (?1, ?2)",
        rusqlite::params![legacy_fact.id.as_str(), derived_id.as_str()],
        |r| r.get(0),
    )?;
    assert_eq!(vec_before, 2);

    drop(store);
    let mut reopened = Store::open(dir.path())?;
    reopened.set_embedder(Arc::new(CountingEmbedder::new(
        "openai:text-embedding-3-small",
        4,
        false,
    )));
    let repaired_host_pol: String = reopened.db.query_row(
        "SELECT model_policy FROM extraction_queue WHERE source_id = ?1",
        [sid_host.as_str()],
        |r| r.get(0),
    )?;
    assert_eq!(repaired_host_pol, "named_endpoint_v2:host");
    assert_eq!(
        crate::decode_queue_model_policy(&repaired_host_pol)?,
        ModelRoutePolicy::NamedEndpoint("host".into())
    );

    // R4 Finding 2: Also verify self-prefixed `NamedEndpoint("named_endpoint:corp-gw")` whose legacy
    // queue row was stored as bare `"named_endpoint:corp-gw"` repairs to `"named_endpoint_v2:named_endpoint:corp-gw"`
    // and decodes injectively without double-prefixing on subsequent `Store::open`!
    let rec_self_prefixed = PublicIngestRecord {
        source: PublicIngestSource {
            kind: "doc".into(),
            uri: "doc://r4/self-prefixed".into(),
            external_id: "r4-self-prefixed".into(),
            container_id: None,
        },
        speaker: PublicSpeaker::new("owner", None),
        audience: PublicAudience::Explicit(vec![owner.clone()]),
        occurred_at: AT,
        content: "自前缀端点名 named_endpoint:corp-gw 升级与幂等测试".into(),
        op: PublicIngestOp::Upsert,
        ingested_by: None,
        media_type: None,
        model_policy: Some(ModelRoutePolicy::NamedEndpoint(
            "named_endpoint:corp-gw".into(),
        )),
    };
    let out_sp = reopened.apply_public_ingest_record(
        &rec_self_prefixed,
        &owner,
        &ModelRoutePolicy::HostCli,
    )?;
    let sid_sp = out_sp.source_id.ok_or(StoreError::NotFound)?;
    reopened.db.execute(
        "UPDATE extraction_queue SET model_policy = 'named_endpoint:corp-gw' WHERE source_id = ?1",
        [sid_sp.as_str()],
    )?;
    drop(reopened);
    let mut reopened = Store::open(dir.path())?;
    reopened.set_embedder(Arc::new(CountingEmbedder::new(
        "openai:text-embedding-3-small",
        4,
        false,
    )));
    let sp_pol_1: String = reopened.db.query_row(
        "SELECT model_policy FROM extraction_queue WHERE source_id = ?1",
        [sid_sp.as_str()],
        |r| r.get(0),
    )?;
    assert_eq!(sp_pol_1, "named_endpoint_v2:named_endpoint:corp-gw");
    assert_eq!(
        crate::decode_queue_model_policy(&sp_pol_1)?,
        ModelRoutePolicy::NamedEndpoint("named_endpoint:corp-gw".into())
    );
    drop(reopened);
    let mut reopened = Store::open(dir.path())?;
    reopened.set_embedder(Arc::new(CountingEmbedder::new(
        "openai:text-embedding-3-small",
        4,
        false,
    )));
    let sp_pol_2: String = reopened.db.query_row(
        "SELECT model_policy FROM extraction_queue WHERE source_id = ?1",
        [sid_sp.as_str()],
        |r| r.get(0),
    )?;
    assert_eq!(sp_pol_2, "named_endpoint_v2:named_endpoint:corp-gw");

    // Simulate an un-repaired bare `"corp-gw"` row right before duplicate Upsert to test in-tx replay too:
    reopened.db.execute(
        "UPDATE extraction_queue SET model_policy = 'corp-gw' WHERE source_id = ?1",
        [sid_corp.as_str()],
    )?;
    let replay =
        reopened.apply_public_ingest_record(&rec_corp, &owner, &ModelRoutePolicy::HostCli)?;
    assert_eq!(replay.kind, PublicRecordAppliedKind::DuplicateIdempotent);
    let normalized_corp_pol: String = reopened.db.query_row(
        "SELECT model_policy FROM extraction_queue WHERE source_id = ?1",
        [sid_corp.as_str()],
        |r| r.get(0),
    )?;
    assert_eq!(normalized_corp_pol, "named_endpoint_v2:corp-gw");

    // 3. Narrow `r3-corp` to `RawOnly` -> purges cloud `fact_vectors` on BOTH anchored AND derived facts!
    let mut rec_corp_raw = rec_corp;
    rec_corp_raw.model_policy = Some(ModelRoutePolicy::RawOnly);
    let _ =
        reopened.apply_public_ingest_record(&rec_corp_raw, &owner, &ModelRoutePolicy::HostCli)?;
    let vec_after: i64 = reopened.db.query_row(
        "SELECT COUNT(*) FROM fact_vectors WHERE fact_id IN (?1, ?2)",
        rusqlite::params![legacy_fact.id.as_str(), derived_id.as_str()],
        |r| r.get(0),
    )?;
    assert_eq!(vec_after, 0);

    // 4. R5 Finding 1: Pre-existing orphaned unseeded `community_concepts` row (left behind by an
    //    older `purge` before upgrade) is swept and VACUUMed on BOTH normal `purge` AND repeat
    //    `purge_target` on an already-tombstoned `source_id`!
    reopened.db.execute(
        "INSERT INTO community_concepts
            (concept_id, concept_kind, canonical_label, domain, polarity, proposed_by, status, created_at)
         VALUES ('cpt_legacy_orphan_1', 'condition', 'LEGACY_ORPHAN_SECRET_881', 'infra', 'neutral', 'owner', 'active', ?1)",
        [AT.0],
    )?;
    let lib_id = reopened.library_id().clone();
    reopened.purge_target(&lib_id, sid_host.as_str(), AT, "purged")?;
    assert!(reopened.list_active_concepts()?.is_empty());
    assert_eq!(
        crate::find_sqlite_table_containing_bytes(dir.path(), b"LEGACY_ORPHAN_SECRET_881")?,
        None
    );

    // Repeat `purge_target` on already-tombstoned `sid_host` (early-return path at tombstone.rs:69)
    // ALSO sweeps any residual orphaned unseeded `community_concepts`, WHILE preserving unseeded
    // concepts that ARE referenced via nested objects (`{"reference":{"concept_id":"cpt_nested"}}`)
    // or top-level string `object_json` (`"cpt_top_str"`) on surviving facts!
    reopened.db.execute(
        "INSERT INTO community_concepts
            (concept_id, concept_kind, canonical_label, domain, polarity, proposed_by, status, created_at)
         VALUES
            ('cpt_legacy_orphan_2', 'condition', 'LEGACY_ORPHAN_REPEAT_882', 'infra', 'neutral', 'owner', 'active', ?1),
            ('cpt_nested_surviving', 'condition', 'NESTED_SURVIVING_CONCEPT_991', 'infra', 'neutral', 'owner', 'active', ?1),
            ('cpt_top_str_surviving', 'topic', 'TOP_STR_SURVIVING_CONCEPT_992', 'infra', 'neutral', 'owner', 'active', ?1)",
        [AT.0],
    )?;
    let mut nested_ref_fact = fact("nested_concept_ref_fact", &["owner"])?;
    nested_ref_fact.object = json!({"reference": {"concept_id": "cpt_nested_surviving"}});
    let mut top_str_fact = fact("top_str_concept_ref_fact", &["owner"])?;
    top_str_fact.object = json!("cpt_top_str_surviving");
    reopened.remember_facts(&[
        FactInput {
            fact: nested_ref_fact,
            entities: BTreeSet::new(),
        },
        FactInput {
            fact: top_str_fact,
            entities: BTreeSet::new(),
        },
    ])?;

    reopened.purge_target(&lib_id, sid_host.as_str(), AT, "purged")?;
    let active_after = reopened.list_active_concepts()?;
    let active_ids: BTreeSet<String> = active_after.iter().map(|c| c.id.to_string()).collect();
    assert!(
        !active_ids.contains("cpt_legacy_orphan_2"),
        "orphaned concept must be deleted: {active_ids:?}"
    );
    assert!(
        active_ids.contains("cpt_nested_surviving") && active_ids.contains("cpt_top_str_surviving"),
        "nested & top-level string-referenced unseeded concepts MUST survive purge: {active_ids:?}"
    );
    assert_eq!(
        crate::find_sqlite_table_containing_bytes(dir.path(), b"LEGACY_ORPHAN_REPEAT_882")?,
        None
    );

    Ok(())
}
