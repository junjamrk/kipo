#![cfg(unix)]

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kp_model::*;
use kp_net::{
    current_process_uid, daemon_runtime_dir, daemon_socket_path, validate_socket_path,
    verify_socket_parent_dir_with_uid, verify_std_stream_peer_uid, verify_stream_peer_uid,
    CommunityChangeFeed, CommunityClient, CursorNotification, DaemonClient, DaemonConfig,
    DaemonEmbedderRuntimeStatus, DaemonHandler, DaemonServer, DaemonSyncClient, IdleSessionBatch,
    IngestJsonlRequest, IngestJsonlResult, NetError, PreToolCheckRequest, PreToolCheckResult,
    PublishOptions, PublishRequest, QueryContextRequest, ToolPermissionDecision,
    MAX_DAEMON_FRAME_BYTES, MAX_UDS_PATH_LEN,
};
use kp_store::{ExtractionJobStatus, InitOptions, Store};
use serde_json::{json, Value};
use tokio::sync::Notify;

type Result<T = ()> = std::result::Result<T, Box<dyn Error>>;

const AT: EpochMs = EpochMs(1_763_683_200_000); // 2025-11-21T00:00:00Z
static NEXT_SOCK_SEQ: AtomicU64 = AtomicU64::new(1);

fn short_test_socket(tag: &str) -> Result<PathBuf> {
    let dir = daemon_runtime_dir()?;
    let seq = NEXT_SOCK_SEQ.fetch_add(1, Ordering::Relaxed);
    Ok(dir.join(format!("{tag}-{seq}.s")))
}

fn sandbox_blocks_raw_uds_connect() -> Result<bool> {
    if cfg!(feature = "test-inproc-uds") {
        return Ok(false);
    }
    let probe = short_test_socket("prb")?;
    let _ = std::fs::remove_file(&probe);
    let _listener = std::os::unix::net::UnixListener::bind(&probe)?;
    let blocked = matches!(
        std::os::unix::net::UnixStream::connect(&probe),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied
    );
    let _ = std::fs::remove_file(&probe);
    Ok(blocked)
}

fn library(id: &str, owner: &str, mode: LibraryMode) -> Result<(tempfile::TempDir, Store)> {
    let dir = tempfile::tempdir()?;
    let store = Store::init(
        dir.path(),
        InitOptions {
            library_id: id.parse()?,
            name: id.into(),
            mode,
            owner: owner.parse()?,
            created_at: EpochMs(0),
        },
    )?;
    Ok((dir, store))
}

fn base_fact(id: &str, author: &str, kind: FactKind, actors: &[&str]) -> Result<Fact> {
    Ok(Fact {
        id: id.parse()?,
        origin_library: format!("{author}-personal").parse()?,
        logical_key: id.parse()?,
        kind,
        subject: id.into(),
        predicate: "kp.core.schedule".into(),
        object: json!(if kind == FactKind::Event {
            "周四晚读书会改在社区书店"
        } else {
            "晚间自习时段保持静音"
        }),
        tags: BTreeSet::from(["book_club".into(), "evening_study".into()]),
        author: author.parse()?,
        ingested_by: None,
        trust_tier: TrustTier::HumanVerbatim,
        access: FactAccess::Base {
            audience: AudienceSpec::Explicit(
                actors
                    .iter()
                    .map(|a| a.parse())
                    .collect::<kp_model::Result<_>>()?,
            ),
            stamp: BitemporalStamp {
                valid: ValidityWindow {
                    valid_from: EpochMs(0),
                    valid_until: EpochMs::OPEN_END,
                },
                recorded_at: AT,
                expired_at: None,
            },
        },
        source_scope_id: None,
        governed_scope: None,
        broader_scope: None,
        status: FactStatus::Active,
        superseded_by: None,
        rank: FactRank::Normal,
        source_anchor: None,
        unverified_gaps: Vec::new(),
        miss_count: 0,
    })
}

struct UdsHttpServer {
    handle: kp_net::UnixHttpHandle,
    socket_path: PathBuf,
    url: String,
    tokens: BTreeMap<String, String>,
    dir: tempfile::TempDir,
}

impl UdsHttpServer {
    async fn start(mut store: Store, dir: tempfile::TempDir, tag: &str) -> Result<Self> {
        let mut tokens = BTreeMap::new();
        for (actor, role) in [
            ("alice", MemberRole::Member),
            ("admin", MemberRole::Owner),
            ("maintainer", MemberRole::Maintainer),
            ("bob", MemberRole::Member),
            ("carol", MemberRole::Member),
        ] {
            tokens.insert(
                actor.into(),
                store.add_member(actor.parse()?, role, AT)?.token,
            );
        }
        let socket_path = short_test_socket(tag)?;
        let handle = kp_net::serve_unix(&socket_path, kp_net::router(store))?;
        let url = format!("http://{tag}.kp.local");
        Ok(Self {
            handle,
            socket_path,
            url,
            tokens,
            dir,
        })
    }

    fn client(&self, actor: &str) -> Result<CommunityClient> {
        let token = self.tokens.get(actor).ok_or("missing actor token")?;
        Ok(CommunityClient::with_unix_socket(
            &self.url,
            token,
            &self.socket_path,
        )?)
    }

    fn stop_offline(&self) {
        self.handle.stop();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_source_scope_ceiling_rejects_widening_supports_speaker_ingester_and_narrows_on_departure(
) -> Result {
    if sandbox_blocks_raw_uds_connect()? {
        return Ok(());
    }
    let (qdir, mut q) = library("q", "admin", LibraryMode::Community)?;
    let scope_eng = SourceScope {
        scope_id: "scope_chat_eng".parse()?,
        provider: "ext_chat".into(),
        external_container_id: "oc_eng_01".into(),
        label: "研发协作组".into(),
        current_audience: AudienceSpec::Explicit(["alice".parse()?, "bob".parse()?].into()),
        synced_at: AT,
    };
    q.upsert_source_scope(&scope_eng)?;
    let server = UdsHttpServer::start(q, qdir, "scope").await?;
    let alice = server.client("alice")?;
    let bob = server.client("bob")?;
    let carol = server.client("carol")?;
    let admin = server.client("admin")?;

    let (_bdir, mut bob_personal) = library("bob-personal", "bob", LibraryMode::Personal)?;
    bob_personal.upsert_source_scope(&scope_eng)?;

    // Bob imports a container message spoken by Alice (`author = alice`, `ingested_by = bob`).
    let mut cutover_claim = base_fact(
        "fact_cutover",
        "alice",
        FactKind::Constraint,
        &["alice", "bob"],
    )?;
    cutover_claim.origin_library = "bob-personal".parse()?;
    cutover_claim.ingested_by = Some("bob".parse()?);
    cutover_claim.source_scope_id = Some("scope_chat_eng".parse()?);
    cutover_claim.object =
        json!({"command": "gh pr merge", "rule": "主从切换维护窗口暂停合并代码变更"});
    bob_personal.insert_fact(&cutover_claim)?;

    // 1. Client-side live SourceScope ceiling rejects widening to Carol or AllMembers.
    let widen_carol = PublishOptions {
        audience: Some(AudienceSpec::Explicit(
            ["alice".parse()?, "bob".parse()?, "carol".parse()?].into(),
        )),
        map_predicate: None,
    };
    assert!(matches!(
        kp_net::publish(&mut bob_personal, "fact_cutover", &bob, &widen_carol, AT).await,
        Err(NetError::Forbidden)
    ));
    let widen_all = PublishOptions {
        audience: Some(AudienceSpec::AllMembers),
        map_predicate: None,
    };
    assert!(matches!(
        kp_net::publish(&mut bob_personal, "fact_cutover", &bob, &widen_all, AT).await,
        Err(NetError::Forbidden)
    ));
    assert!(bob_personal.pending_outbox(bob.url())?.is_empty());

    // 2. Direct server-side `/v1/publish` also rejects widening and rejects forged source_scope widening!
    let mut widened_fact = cutover_claim.clone();
    widened_fact.access = FactAccess::Base {
        audience: AudienceSpec::Explicit(
            ["alice".parse()?, "bob".parse()?, "carol".parse()?].into(),
        ),
        stamp: BitemporalStamp {
            valid: ValidityWindow {
                valid_from: EpochMs(0),
                valid_until: EpochMs::OPEN_END,
            },
            recorded_at: AT,
            expired_at: None,
        },
    };
    let mut forged_scope = scope_eng.clone();
    forged_scope.current_audience =
        AudienceSpec::Explicit(["alice".parse()?, "bob".parse()?, "carol".parse()?].into());
    let forged_req = PublishRequest {
        fact: widened_fact,
        entities: BTreeSet::new(),
        idempotency_key: "forged-widen".into(),
        origin_reference: cutover_claim.strong_ref(),
        audience_revision: 1,
        skill_audience: None,
        concepts: Vec::new(),
        source_scope: Some(forged_scope),
    };
    assert!(matches!(
        bob.post("/v1/publish", &forged_req).await,
        Err(NetError::Http(403))
    ));

    // 3. Cross-speaker forgery WITHOUT a shared SourceScope is rejected (`403`).
    let mut unscoped_forgery = base_fact("forged", "alice", FactKind::Event, &["alice", "bob"])?;
    unscoped_forgery.origin_library = "bob-personal".parse()?;
    unscoped_forgery.ingested_by = Some("bob".parse()?);
    unscoped_forgery.source_scope_id = None;
    let unscoped_req = PublishRequest {
        origin_reference: unscoped_forgery.strong_ref(),
        fact: unscoped_forgery,
        entities: BTreeSet::new(),
        idempotency_key: "unscoped-forgery".into(),
        audience_revision: 1,
        skill_audience: None,
        concepts: Vec::new(),
        source_scope: None,
    };
    assert!(matches!(
        bob.post("/v1/publish", &unscoped_req).await,
        Err(NetError::Http(403))
    ));

    // 3b. Unprivileged member `carol` CANNOT forge `author = "alice"` by inventing a brand-new `source_scope_id`
    // or self-register an `AllMembers` container on the community server!
    let fake_scope_by_carol = SourceScope {
        scope_id: "scope_fake_by_carol".parse()?,
        provider: "ext_chat".into(),
        external_container_id: "oc_fake".into(),
        label: "伪造群".into(),
        current_audience: AudienceSpec::Explicit(
            ["alice".parse()?, "bob".parse()?, "carol".parse()?].into(),
        ),
        synced_at: AT,
    };
    let mut carol_impersonation = base_fact(
        "forged_via_new_scope",
        "alice",
        FactKind::Event,
        &["alice", "bob", "carol"],
    )?;
    carol_impersonation.origin_library = "carol-personal".parse()?;
    carol_impersonation.ingested_by = Some("carol".parse()?);
    carol_impersonation.source_scope_id = Some("scope_fake_by_carol".parse()?);
    let carol_forged_req = PublishRequest {
        origin_reference: carol_impersonation.strong_ref(),
        fact: carol_impersonation,
        entities: BTreeSet::new(),
        idempotency_key: "carol-impersonation".into(),
        audience_revision: 1,
        skill_audience: None,
        concepts: Vec::new(),
        source_scope: Some(fake_scope_by_carol),
    };
    assert!(matches!(
        carol.post("/v1/publish", &carol_forged_req).await,
        Err(NetError::Http(403))
    ));

    let fake_all_members_scope = SourceScope {
        scope_id: "scope_fake_all".parse()?,
        provider: "ext_repo".into(),
        external_container_id: "org/repo".into(),
        label: "repo".into(),
        current_audience: AudienceSpec::AllMembers,
        synced_at: AT,
    };
    let mut carol_all_claim = base_fact("carol_all", "carol", FactKind::Event, &["carol"])?;
    carol_all_claim.source_scope_id = Some("scope_fake_all".parse()?);
    let carol_all_req = PublishRequest {
        origin_reference: carol_all_claim.strong_ref(),
        fact: carol_all_claim,
        entities: BTreeSet::new(),
        idempotency_key: "carol-all-scope".into(),
        audience_revision: 1,
        skill_audience: None,
        concepts: Vec::new(),
        source_scope: Some(fake_all_members_scope),
    };
    assert!(matches!(
        carol.post("/v1/publish", &carol_all_req).await,
        Err(NetError::Http(403))
    ));

    // 4. Valid publish within `requested_audience ⊆ current_audience ∩ members` succeeds!
    let pub_res = kp_net::publish(
        &mut bob_personal,
        "fact_cutover",
        &bob,
        &PublishOptions::default(),
        AT,
    )
    .await?;
    assert!(!pub_res.queued);
    // Second publish from `bob-personal` also succeeds (`origin_owners` tracks `ingested_by` = bob).
    let mut second_claim = base_fact("fact_cutover_2", "bob", FactKind::Event, &["alice", "bob"])?;
    second_claim.source_scope_id = Some("scope_chat_eng".parse()?);
    bob_personal.insert_fact(&second_claim)?;
    assert!(
        !kp_net::publish(
            &mut bob_personal,
            "fact_cutover_2",
            &bob,
            &PublishOptions::default(),
            AT
        )
        .await?
        .queued
    );

    // Carol sees 0 facts; Alice and Bob both see 3 facts (2 scoped base claims + 1 derived CollisionWarning).
    assert_eq!(carol.get("/v1/status?at=1763683200000").await?["facts"], 0);
    assert_eq!(alice.get("/v1/status?at=1763683200000").await?["facts"], 3);
    assert_eq!(bob.get("/v1/status?at=1763683200000").await?["facts"], 3);

    // 5. Bob leaves the collaboration group -> moderator updates live `SourceScope` on `/v1/scopes`.
    let narrowed_scope = SourceScope {
        current_audience: AudienceSpec::Explicit(["alice".parse()?].into()),
        synced_at: EpochMs(AT.0 + 1000),
        ..scope_eng
    };
    // Non-moderator cannot overwrite `/v1/scopes`.
    assert!(matches!(
        bob.post("/v1/scopes", &narrowed_scope).await,
        Err(NetError::Http(403))
    ));
    admin.post("/v1/scopes", &narrowed_scope).await?;

    // Immediately at read time, Bob sees 0 facts (both base claims AND derived warning hidden!), while Alice still sees 3!
    assert_eq!(bob.get("/v1/status?at=1763683200000").await?["facts"], 0);
    assert_eq!(alice.get("/v1/status?at=1763683200000").await?["facts"], 3);

    // And Alice trying to publish a new `scope_chat_eng` claim to `{alice, bob}` is now rejected by server ceiling!
    let (_adir, mut alice_personal) = library("alice-personal", "alice", LibraryMode::Personal)?;
    let mut post_departure = base_fact(
        "fact_after_leave",
        "alice",
        FactKind::Event,
        &["alice", "bob"],
    )?;
    post_departure.source_scope_id = Some("scope_chat_eng".parse()?);
    alice_personal.insert_fact(&post_departure)?;
    assert!(matches!(
        kp_net::publish(
            &mut alice_personal,
            "fact_after_leave",
            &alice,
            &PublishOptions::default(),
            AT
        )
        .await,
        Err(NetError::Http(403))
    ));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn governance_concepts_membership_and_scopes_sync_with_byte_identical_outsider_noninterference(
) -> Result {
    if sandbox_blocks_raw_uds_connect()? {
        return Ok(());
    }
    let (qdir, q) = library("q", "admin", LibraryMode::Community)?;
    let server = UdsHttpServer::start(q, qdir, "gov").await?;
    let alice = server.client("alice")?;
    let bob = server.client("bob")?;
    let carol = server.client("carol")?;

    let (_adir, mut a) = library("alice-personal", "alice", LibraryMode::Personal)?;
    let (_bdir, mut b) = library("bob-personal", "bob", LibraryMode::Personal)?;

    // Seed one public event visible to all 3 members so Carol has an active cursor & search corpus.
    let public_event = base_fact(
        "fact_public_offsite",
        "alice",
        FactKind::Event,
        &["alice", "bob", "carol"],
    )?;
    a.insert_fact(&public_event)?;
    kp_net::publish(
        &mut a,
        "fact_public_offsite",
        &alice,
        &PublishOptions::default(),
        AT,
    )
    .await?;

    let carol_init: Value = carol.get("/v1/changes?since=0&at=1763683200000").await?;
    let carol_cursor = carol_init["cursor"].as_str().ok_or("cursor")?.to_owned();
    let probe_paths = [
        "/v1/search?q=book_club&deep=true&at=1763683200000&limit=100".to_owned(),
        "/v1/changes?since=0&at=1763683200000".to_owned(),
        format!("/v1/changes?since={carol_cursor}&at=1763683200000"),
        "/v1/status?at=1763683200000".to_owned(),
    ];
    let mut carol_before = Vec::new();
    for path in &probe_paths {
        carol_before.push(carol.get_raw(path).await?);
    }

    // Bob prepares private `{Alice, Bob}` concepts, SourceScope, Membership, and Constraint with GovernedScope + BroaderScopeCandidate.
    let cpt_gym = CommunityConcept {
        id: "cpt_gym_group".parse()?,
        kind: ConceptKind::Group,
        canonical_label: "训练组".into(),
        domain: "fitness".into(),
        polarity: ConceptPolarity::Neutral,
        proposed_by: "bob".parse()?,
        created_at: AT,
    };
    let cpt_sprain = CommunityConcept {
        id: "cpt_ankle_strain".parse()?,
        kind: ConceptKind::Condition,
        canonical_label: "脚踝扭伤".into(),
        domain: "fitness".into(),
        polarity: ConceptPolarity::Positive,
        proposed_by: "bob".parse()?,
        created_at: AT,
    };
    let cpt_squat = CommunityConcept {
        id: "cpt_no_squat".parse()?,
        kind: ConceptKind::Action,
        canonical_label: "不做深蹲".into(),
        domain: "fitness".into(),
        polarity: ConceptPolarity::Negative,
        proposed_by: "bob".parse()?,
        created_at: AT,
    };
    for c in [&cpt_gym, &cpt_sprain, &cpt_squat] {
        b.exchange_upsert_concept(c, true)?;
    }

    let scope_gym = SourceScope {
        scope_id: "scope_gym_chat".parse()?,
        provider: "ext_chat".into(),
        external_container_id: "oc_gym".into(),
        label: "训练打卡群".into(),
        current_audience: AudienceSpec::Explicit(["alice".parse()?, "bob".parse()?].into()),
        synced_at: AT,
    };
    b.upsert_source_scope(&scope_gym)?;

    let mut membership = base_fact(
        "fact_gym_mem",
        "bob",
        FactKind::Membership,
        &["alice", "bob"],
    )?;
    membership.predicate = "kp.core.membership".into();
    membership.source_scope_id = Some("scope_gym_chat".parse()?);
    membership.object = serde_json::to_value(MembershipRelation {
        anchor: "bob".parse()?,
        member: "alice".parse()?,
        group_concept_id: "cpt_gym_group".parse()?,
    })?;
    b.insert_fact(&membership)?;

    let mut sprain_event = base_fact(
        "fact_sprain_event",
        "alice",
        FactKind::Event,
        &["alice", "bob"],
    )?;
    sprain_event.origin_library = "bob-personal".parse()?;
    sprain_event.ingested_by = Some("bob".parse()?);
    sprain_event.source_scope_id = Some("scope_gym_chat".parse()?);
    sprain_event.object = json!({
        "claim_text": "训练时脚踝扭伤",
        "condition_concept_id": "cpt_ankle_strain"
    });
    b.insert_fact(&sprain_event)?;

    let mut group_constraint = base_fact(
        "fact_group_sprain_rule",
        "bob",
        FactKind::Constraint,
        &["alice", "bob"],
    )?;
    group_constraint.ingested_by = Some("bob".parse()?);
    group_constraint.source_scope_id = Some("scope_gym_chat".parse()?);
    group_constraint.governed_scope = Some(GovernedScope::RelationGroup {
        anchor: "bob".parse()?,
        group_concept_id: "cpt_gym_group".parse()?,
    });
    group_constraint.object = json!({
        "claim_text": "训练组成员脚踝扭伤期间不做深蹲",
        "condition_concept_id": "cpt_ankle_strain",
        "action_concept_id": "cpt_no_squat"
    });
    b.insert_fact(&group_constraint)?;

    let mut constraint = base_fact(
        "fact_sprain_constraint",
        "bob",
        FactKind::Constraint,
        &["alice", "bob"],
    )?;
    constraint.ingested_by = Some("bob".parse()?);
    constraint.source_scope_id = Some("scope_gym_chat".parse()?);
    constraint.governed_scope = Some(GovernedScope::SelfOnly {
        actor: "bob".parse()?,
    });
    constraint.broader_scope = Some(BroaderScopeCandidate {
        narrow_active: GovernedScope::SelfOnly {
            actor: "bob".parse()?,
        },
        broader_candidate: GovernedScope::RelationGroup {
            anchor: "bob".parse()?,
            group_concept_id: "cpt_gym_group".parse()?,
        },
        question: "只管你自己，还是整个训练组？".into(),
    });
    constraint.object = json!({
        "claim_text": "脚踝扭伤期间不做深蹲",
        "condition_concept_id": "cpt_ankle_strain",
        "action_concept_id": "cpt_no_squat"
    });
    b.insert_fact(&constraint)?;

    kp_net::publish(&mut b, "fact_gym_mem", &bob, &PublishOptions::default(), AT).await?;
    kp_net::publish(
        &mut b,
        "fact_sprain_event",
        &bob,
        &PublishOptions::default(),
        AT,
    )
    .await?;
    kp_net::publish(
        &mut b,
        "fact_group_sprain_rule",
        &bob,
        &PublishOptions::default(),
        AT,
    )
    .await?;
    kp_net::publish(
        &mut b,
        "fact_sprain_constraint",
        &bob,
        &PublishOptions::default(),
        AT,
    )
    .await?;

    // Strict byte-identical noninterference for outsider Carol across ALL endpoints!
    for (idx, path) in probe_paths.iter().enumerate() {
        assert_eq!(
            carol_before[idx],
            carol.get_raw(path).await?,
            "Carol noninterference violated on {path}"
        );
    }

    // Verify collision warning was derived for {Alice, Bob} and is completely indistinguishable from 404 for Carol.
    let alice_feed: CommunityChangeFeed =
        serde_json::from_value(alice.get("/v1/changes?at=1763683200000").await?)?;
    let collision = alice_feed
        .snapshot
        .facts
        .iter()
        .find(|f| f.kind == FactKind::CollisionWarning)
        .ok_or("expected derived collision warning for {alice, bob}")?;
    assert_eq!(
        carol
            .get_raw(&format!("/v1/why/{}?at=1763683200000", collision.id))
            .await?,
        carol
            .get_raw("/v1/why/fact_nonexistent_probe?at=1763683200000")
            .await?
    );

    // Sync community -> `alice-personal` and verify concepts, SourceScope, Membership, ingested_by, governed_scope, broader_scope!
    let sync_res = kp_net::sync(&mut a, &alice, AT).await?;
    assert!(!sync_res.unchanged);
    assert!(sync_res.mirrored >= 4);

    let synced_concepts: BTreeSet<String> = a
        .exchange_active_concepts()?
        .into_iter()
        .map(|c| c.id.to_string())
        .collect();
    assert_eq!(
        synced_concepts,
        BTreeSet::from([
            "cpt_gym_group".into(),
            "cpt_ankle_strain".into(),
            "cpt_no_squat".into(),
        ])
    );
    assert!(a.source_scope(&"scope_gym_chat".parse()?)?.is_some());

    let alice_visible = a.visible_facts(&"alice".parse()?, AT)?;
    assert!(alice_visible.iter().any(|f| f.kind == FactKind::Membership));
    assert!(alice_visible
        .iter()
        .any(|f| f.kind == FactKind::CollisionWarning));
    let synced_constraint = alice_visible
        .iter()
        .find(|f| f.logical_key == constraint.logical_key)
        .ok_or("missing synced constraint")?;
    assert_eq!(synced_constraint.ingested_by, Some("bob".parse()?));
    assert_eq!(
        synced_constraint.source_scope_id,
        Some("scope_gym_chat".parse()?)
    );
    assert_eq!(synced_constraint.governed_scope, constraint.governed_scope);
    assert_eq!(synced_constraint.broader_scope, constraint.broader_scope);

    // Subsequent sync with no new visible changes returns unchanged = true.
    assert!(kp_net::sync(&mut a, &alice, AT).await?.unchanged);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_origin_never_invalidates_across_ten_misses_recovers_online_and_invalidates_only_on_explicit_withdrawal(
) -> Result {
    if sandbox_blocks_raw_uds_connect()? {
        return Ok(());
    }
    let (odir, mut origin_store) = library("alice-personal", "admin", LibraryMode::Community)?;
    let premise = base_fact(
        "fact_origin_premise",
        "alice",
        FactKind::Event,
        &["alice", "bob"],
    )?;
    origin_store.insert_fact(&premise)?;
    let origin_server = UdsHttpServer::start(origin_store, odir, "orig").await?;
    let origin_sock = origin_server.socket_path.clone();
    let origin_url = origin_server.url.clone();
    let alice_origin_token = origin_server.tokens["alice"].clone();

    let (qdir, q) = library("q", "admin", LibraryMode::Community)?;
    let community_server = UdsHttpServer::start(q, qdir, "comm").await?;
    let alice_comm = community_server.client("alice")?;
    let bob_comm = community_server.client("bob")?;

    let (_apdir, mut alice_local) = library("alice-personal", "alice", LibraryMode::Personal)?;
    alice_local.insert_fact(&premise)?;
    kp_net::publish(
        &mut alice_local,
        "fact_origin_premise",
        &alice_comm,
        &PublishOptions::default(),
        AT,
    )
    .await?;

    let (_bmdir, mut bob_mirror) = library("bob-personal", "bob", LibraryMode::Personal)?;
    kp_net::sync(&mut bob_mirror, &bob_comm, AT).await?;
    let mirrored = bob_mirror
        .visible_facts(&"bob".parse()?, AT)?
        .pop()
        .ok_or("expected mirrored fact")?;

    // Attach a local derived conclusion depending on the mirrored premise.
    let local_derived = derived_fact(
        bob_mirror.library_id(),
        &"bob".parse()?,
        FactKind::DerivedFact,
        "bob",
        "kp.core.advice",
        json!("携带阅读笔记前往社区书店"),
        AT,
    )?;
    let proof = justification(
        &local_derived,
        DerivationMode::HardRule,
        "R_TEST",
        &[&mirrored],
    )?;
    bob_mirror.save_derivations(std::slice::from_ref(&local_derived), &[proof])?;
    assert_eq!(bob_mirror.visible_facts(&"bob".parse()?, AT)?.len(), 2);

    let origins = BTreeMap::from([(
        "alice-personal".parse()?,
        CommunityClient::with_unix_socket(&origin_url, &alice_origin_token, &origin_sock)?,
    )]);

    // Take the origin server completely offline and run 10 consecutive reconciliation passes (covering 2, 3, and 10 misses).
    origin_server.stop_offline();
    for miss_num in 1..=10 {
        kp_net::reconcile_remote_origins(&mut bob_mirror, &origins, AT).await?;
        let graph = bob_mirror.derivation_view()?;
        let state = &graph.facts[&mirrored.id];
        assert_eq!(
            state.status,
            FactStatus::UnreachableHidden,
            "miss #{miss_num} must stay UnreachableHidden, never Invalidated"
        );
        assert_eq!(state.miss_count, miss_num);
        // Both base mirror and dependent derived conclusion are hidden while origin is unreachable.
        assert!(bob_mirror.visible_facts(&"bob".parse()?, AT)?.is_empty());
    }

    // Also verify HTTP 401 (bad/expired token) while origin is back online ONLY hides (does NOT permanently invalidate!).
    let reopened = Store::open(origin_server.dir.path())?;
    let restart_handle = kp_net::serve_unix(&origin_sock, kp_net::router(reopened))?;

    let bad_token_origins = BTreeMap::from([(
        "alice-personal".parse()?,
        CommunityClient::with_unix_socket(&origin_url, "invalid_expired_token", &origin_sock)?,
    )]);
    kp_net::reconcile_remote_origins(&mut bob_mirror, &bad_token_origins, AT).await?;
    assert_eq!(
        bob_mirror.derivation_view()?.facts[&mirrored.id].status,
        FactStatus::UnreachableHidden
    );
    assert_eq!(
        bob_mirror.derivation_view()?.facts[&mirrored.id].miss_count,
        11
    );

    // Valid origin comes back -> mirror and derived conclusion immediately recover!
    kp_net::reconcile_remote_origins(&mut bob_mirror, &origins, AT).await?;
    let graph = bob_mirror.derivation_view()?;
    assert_eq!(graph.facts[&mirrored.id].status, FactStatus::Active);
    assert_eq!(graph.facts[&mirrored.id].miss_count, 0);
    assert_eq!(bob_mirror.visible_facts(&"bob".parse()?, AT)?.len(), 2);

    // Explicit withdrawal on the origin -> `/v1/verify` returns `{"confirmed": false}` -> permanent `Invalidated`!
    let origin_alice =
        CommunityClient::with_unix_socket(&origin_url, &alice_origin_token, &origin_sock)?;
    origin_alice
        .post("/v1/withdraw", &json!({"fact_id": premise.id}))
        .await?;
    kp_net::reconcile_remote_origins(&mut bob_mirror, &origins, AT).await?;
    let final_graph = bob_mirror.derivation_view()?;
    assert_eq!(
        final_graph.facts[&mirrored.id].status,
        FactStatus::Invalidated
    );
    assert_eq!(
        final_graph.facts[&local_derived.id].status,
        FactStatus::Invalidated
    );
    assert!(bob_mirror.visible_facts(&"bob".parse()?, AT)?.is_empty());

    restart_handle.stop();
    Ok(())
}

#[allow(dead_code)]
struct NoopDaemonHandler;

impl DaemonHandler for NoopDaemonHandler {}

struct RecordingDaemonHandler {
    low_quota_mode: AtomicBool,
    extraction_calls: AtomicUsize,
    batches: Mutex<Vec<IdleSessionBatch>>,
}

impl RecordingDaemonHandler {
    fn new(low_quota_first: bool) -> Self {
        Self {
            low_quota_mode: AtomicBool::new(low_quota_first),
            extraction_calls: AtomicUsize::new(0),
            batches: Mutex::new(Vec::new()),
        }
    }
}

impl DaemonHandler for RecordingDaemonHandler {
    fn extract_session_batch<'a>(
        &'a self,
        _library_dir: &'a Path,
        batch: IdleSessionBatch,
    ) -> BoxFuture<'a, kp_net::Result<Option<u64>>> {
        Box::pin(async move {
            self.extraction_calls.fetch_add(1, Ordering::SeqCst);
            if let Ok(mut guard) = self.batches.lock() {
                guard.push(batch.clone());
            }
            if self.low_quota_mode.load(Ordering::SeqCst) {
                // Simulate Codex `min_rate_limit_remaining_percent` skip: preserve cursor!
                Ok(None)
            } else {
                Ok(Some(batch.end_byte_offset))
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_daemon_uds_rpc_fast_cursor_ack_coalesced_idle_extraction_and_hook_checks() -> Result
{
    if sandbox_blocks_raw_uds_connect()? {
        return Ok(());
    }
    // 1. Verify short socket path helper stays well below 90-byte macOS SUN_PATH limit even on deep paths.
    let deep_base = tempfile::tempdir()?;
    let deep_library_dir = deep_base.path().join(
        "deeply/nested/workspace/directory/with/a/very/long/path/that/would/overflow/macos/sun_path/if/used/directly",
    );
    let computed_sock = daemon_socket_path(&deep_library_dir)?;
    assert!(computed_sock.to_string_lossy().len() < MAX_UDS_PATH_LEN);
    assert!(validate_socket_path(Path::new(&"x".repeat(MAX_UDS_PATH_LEN))).is_err());

    // 2. Initialize library with active constraints & ambiguous-scope facts for QueryContext and PreToolCheck.
    let (lib_dir, mut store) = library("alice-personal", "alice", LibraryMode::Personal)?;
    store.add_member("bob".parse()?, MemberRole::Member, AT)?;

    let mut cutover_guard = base_fact(
        "fact_merge_guard",
        "alice",
        FactKind::Constraint,
        &["alice", "bob"],
    )?;
    cutover_guard.subject = "主从切换窗口".into();
    cutover_guard.tags = BTreeSet::from(["gh pr merge".into(), "db_cutover".into()]);
    cutover_guard.object = json!({
        "hard_guard": true,
        "command": "gh pr merge",
        "rule": "下周四主从切换维护窗口暂停合并非回滚分支"
    });
    store.insert_fact(&cutover_guard)?;

    let mut ambiguous_hotel = base_fact(
        "fact_ambig_hotel",
        "alice",
        FactKind::Constraint,
        &["alice", "bob"],
    )?;
    ambiguous_hotel.subject = "差旅住宿报销".into();
    ambiguous_hotel.object = json!("差旅住宿报销每晚上限六百元");
    ambiguous_hotel.governed_scope = Some(GovernedScope::SelfOnly {
        actor: "alice".parse()?,
    });
    ambiguous_hotel.broader_scope = Some(BroaderScopeCandidate {
        narrow_active: GovernedScope::SelfOnly {
            actor: "alice".parse()?,
        },
        broader_candidate: GovernedScope::Actors {
            actors: ["alice".parse()?, "bob".parse()?].into(),
        },
        question: "只管你自己，还是整个项目组？".into(),
    });
    store.insert_fact(&ambiguous_hotel)?;

    // Shared-readable `SelfOnly` and `Actors` constraints for Alice + an actor-named `subject = "bob"` rule.
    let mut alice_self_publish = base_fact(
        "fact_alice_self_publish",
        "alice",
        FactKind::Constraint,
        &["alice", "bob"],
    )?;
    alice_self_publish.subject = "个人发版习惯".into();
    alice_self_publish.tags.clear();
    alice_self_publish.governed_scope = Some(GovernedScope::SelfOnly {
        actor: "alice".parse()?,
    });
    alice_self_publish.object = json!({
        "command": "npm publish",
        "rule": "我个人今天不执行 npm publish"
    });
    store.insert_fact(&alice_self_publish)?;

    let mut alice_only_actors = base_fact(
        "fact_alice_only_actors",
        "alice",
        FactKind::Constraint,
        &["alice", "bob"],
    )?;
    alice_only_actors.subject = "指定成员发版".into();
    alice_only_actors.tags.clear();
    alice_only_actors.governed_scope = Some(GovernedScope::Actors {
        actors: ["alice".parse()?].into(),
    });
    alice_only_actors.object = json!({
        "command": "cargo publish",
        "rule": "仅限 alice 执行前人工确认 cargo publish"
    });
    store.insert_fact(&alice_only_actors)?;

    let mut bob_subject_norm = base_fact(
        "fact_bob_subject_norm",
        "alice",
        FactKind::Constraint,
        &["alice", "bob"],
    )?;
    bob_subject_norm.subject = "bob".into();
    bob_subject_norm.tags.clear();
    bob_subject_norm.object = json!("值班期间每小时巡检队列积压");
    store.insert_fact(&bob_subject_norm)?;

    let sock_path = daemon_socket_path(lib_dir.path())?;
    // Leave a stale non-listening file at `sock_path` to verify stale cleanup on startup.
    std::fs::write(&sock_path, b"stale")?;

    let handler = Arc::new(RecordingDaemonHandler::new(true));
    let config = DaemonConfig {
        socket_path: sock_path.clone(),
        idle_extract_window: Duration::from_millis(40),
        idle_shutdown_timeout: None,
        max_frame_bytes: 64 * 1024,
        web_inspector: None,
    };
    let daemon = DaemonServer::spawn(
        store,
        config.clone(),
        Arc::clone(&handler) as Arc<dyn DaemonHandler>,
    )
    .await?;

    // Socket permissions must be owner-only (`0o600`).
    let mode = std::fs::metadata(&sock_path)?.permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    // Attempting to spawn a second daemon on a live socket must be rejected without unlinking the live socket!
    let (_dup_dir, dup_store) = library("dup", "alice", LibraryMode::Personal)?;
    assert!(DaemonServer::spawn_default(dup_store, config)
        .await
        .is_err());

    let async_client = DaemonClient::for_library(lib_dir.path())?;
    let sync_client = DaemonSyncClient::for_library(lib_dir.path())?;

    let (pong_lib, _uptime) = async_client.ping().await?;
    assert_eq!(pong_lib.as_str(), "alice-personal");

    // 3. Measure synchronous hook `NotifyCursor` fire-and-ack latency (< 5ms requirement) & multi-turn coalescing!
    let mut latencies_us = Vec::new();
    for step in 1..=5u64 {
        let t0 = Instant::now();
        let (queued, last_extracted) = sync_client.notify_cursor(CursorNotification {
            session_id: "sess_coalesce".into(),
            client_kind: "claude-code".into(),
            transcript_path: Some("/tmp/claude-sess.jsonl".into()),
            byte_offset: step * 200,
            line_count: step * 2,
            prefix_sha256: "prefix_v1".into(),
            actor: "alice".parse()?,
            at: AT,
            session_ended: false,
        })?;
        latencies_us.push(t0.elapsed().as_micros());
        assert!(queued);
        assert_eq!(last_extracted, 0);
    }
    latencies_us.sort_unstable();
    let median_us = latencies_us[latencies_us.len() / 2];
    assert!(
        median_us < 5_000,
        "expected <5ms NotifyCursor fire-and-ack, got median {median_us}us"
    );

    // Wait past the 40ms idle window: all 5 rapid `Stop` updates coalesce into 1 batch `0..1000`,
    // and because `low_quota_mode == true`, `last_extracted_offset` stays `0` (`pending_extractions == 1`)!
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        if handler.extraction_calls.load(Ordering::SeqCst) >= 1 {
            break;
        }
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(handler.extraction_calls.load(Ordering::SeqCst), 1);
    {
        let recorded = handler.batches.lock().map_err(|_| "lock")?;
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].start_byte_offset, 0);
        assert_eq!(recorded[0].end_byte_offset, 1000);
    }
    let st_low_quota = async_client.status().await?;
    assert_eq!(st_low_quota.owner_actor.as_str(), "alice");
    assert_eq!(st_low_quota.tracked_sessions, 1);
    assert_eq!(st_low_quota.pending_extractions, 1);

    // Quota recovers and `SessionEnd` arrives -> immediate extraction flushes `0..1200` and advances cursor to 1200!
    handler.low_quota_mode.store(false, Ordering::SeqCst);
    let (queued_end, prev_ext) = async_client
        .notify_cursor(CursorNotification {
            session_id: "sess_coalesce".into(),
            client_kind: "claude-code".into(),
            transcript_path: Some("/tmp/claude-sess.jsonl".into()),
            byte_offset: 1200,
            line_count: 14,
            prefix_sha256: "prefix_v1".into(),
            actor: "alice".parse()?,
            at: AT,
            session_ended: true,
        })
        .await?;
    assert!(queued_end);
    assert_eq!(prev_ext, 0);

    for _ in 0..30 {
        if async_client.status().await?.pending_extractions == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(async_client.status().await?.pending_extractions, 0);

    // 4. Verify `QueryContext` (`<= 250` tokens, `<= 3` items, provenance stamp, serendipitous author-only review hint, empty on irrelevant query).
    let irrelevant = async_client
        .query_context(QueryContextRequest {
            prompt: "quantum chromodynamics lattice gauge".into(),
            viewer: "alice".parse()?,
            at: AT,
            max_tokens: 250,
            max_items: 3,
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert!(irrelevant.additional_context.is_empty());
    assert_eq!(irrelevant.item_count, 0);

    let alice_ctx = async_client
        .query_context(QueryContextRequest {
            prompt: "差旅住宿报销怎么安排".into(),
            viewer: "alice".parse()?,
            at: AT,
            max_tokens: 250,
            max_items: 3,
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert!(alice_ctx.item_count >= 1 && alice_ctx.item_count <= 3);
    assert!(alice_ctx.estimated_tokens <= 250);
    assert!(alice_ctx
        .additional_context
        .contains("[2025-11-21 · alice @ alice-personal · kp-ref:fact_ambig_hotel]"));
    assert!(alice_ctx.additional_context.contains("可用 kp review 确认"));

    // Bob querying the exact same topic does NOT see Alice's author-only serendipitous review prompt!
    let bob_ctx = async_client
        .query_context(QueryContextRequest {
            prompt: "差旅住宿报销怎么安排".into(),
            viewer: "bob".parse()?,
            at: AT,
            max_tokens: 250,
            max_items: 3,
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert!(!bob_ctx.additional_context.contains("可用 kp review 确认"));

    // 5. Verify `PreToolCheck`: silent `{}` on safe commands, Claude `Ask` vs Codex `Deny` + `kp allow`!
    let safe_tool = async_client
        .pre_tool_check(PreToolCheckRequest {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "cargo test --offline"}),
            cwd: None,
            viewer: "bob".parse()?,
            at: AT,
            client_kind: None,
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert_eq!(safe_tool.permission_decision, None);
    assert_eq!(safe_tool.to_hook_json(), json!({}));

    for tool_name in ["Bash", "exec_command"] {
        let guarded = async_client
            .pre_tool_check(PreToolCheckRequest {
                tool_name: tool_name.into(),
                tool_input: json!({"command": "gh pr merge 42 --squash"}),
                cwd: None,
                viewer: "bob".parse()?,
                at: AT,
                client_kind: None,
                time_zone: Some("UTC".into()),
            })
            .await?;
        assert_eq!(
            guarded.permission_decision,
            Some(ToolPermissionDecision::Ask)
        );
        assert_eq!(guarded.allow_refs, vec!["fact_merge_guard".to_owned()]);
        let hook_json = guarded.to_hook_json();
        assert_eq!(
            hook_json["hookSpecificOutput"]["hookEventName"],
            "PreToolUse"
        );
        assert_eq!(hook_json["hookSpecificOutput"]["permissionDecision"], "ask");
        assert!(hook_json["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap_or("")
            .contains("kp-ref:fact_merge_guard"));
    }

    let codex_guarded = async_client
        .pre_tool_check(PreToolCheckRequest {
            tool_name: "exec_command".into(),
            tool_input: json!({"command": "gh pr merge 42 --squash"}),
            cwd: None,
            viewer: "bob".parse()?,
            at: AT,
            client_kind: Some("codex".into()),
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert_eq!(
        codex_guarded.permission_decision,
        Some(ToolPermissionDecision::Deny)
    );
    assert_eq!(
        codex_guarded.allow_refs,
        vec!["fact_merge_guard".to_owned()]
    );
    assert!(codex_guarded
        .permission_decision_reason
        .as_deref()
        .unwrap_or("")
        .ends_with("\n先问用户；用户同意后，请用户执行 kp allow fact_merge_guard。"));

    // 5b. Alice's shared-readable `SelfOnly` and `Actors({"alice"})` constraints trigger `Ask` for Alice,
    //     but NEVER interrupt Bob (`permission_decision == None`, `{}`)!
    let alice_npm = async_client
        .pre_tool_check(PreToolCheckRequest {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "npm publish"}),
            cwd: None,
            viewer: "alice".parse()?,
            at: AT,
            client_kind: None,
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert_eq!(
        alice_npm.permission_decision,
        Some(ToolPermissionDecision::Ask)
    );
    let bob_npm = async_client
        .pre_tool_check(PreToolCheckRequest {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "npm publish"}),
            cwd: None,
            viewer: "bob".parse()?,
            at: AT,
            client_kind: None,
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert_eq!(bob_npm.permission_decision, None);
    assert_eq!(bob_npm.to_hook_json(), json!({}));

    let alice_cargo_pub = async_client
        .pre_tool_check(PreToolCheckRequest {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "cargo publish"}),
            cwd: None,
            viewer: "alice".parse()?,
            at: AT,
            client_kind: None,
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert_eq!(
        alice_cargo_pub.permission_decision,
        Some(ToolPermissionDecision::Ask)
    );
    let bob_cargo_pub = async_client
        .pre_tool_check(PreToolCheckRequest {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "cargo publish"}),
            cwd: None,
            viewer: "bob".parse()?,
            at: AT,
            client_kind: None,
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert_eq!(bob_cargo_pub.permission_decision, None);
    assert_eq!(bob_cargo_pub.to_hook_json(), json!({}));

    // 5c. Actor/topic `subject = "bob"` NEVER false-positives on paths containing `/srv/workspaces/bob/...`!
    let bob_path_edit = async_client
        .pre_tool_check(PreToolCheckRequest {
            tool_name: "Edit".into(),
            tool_input: json!({"file_path": "/srv/workspaces/bob/repo/src/lib.rs"}),
            cwd: None,
            viewer: "bob".parse()?,
            at: AT,
            client_kind: None,
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert_eq!(bob_path_edit.permission_decision, None);
    assert_eq!(bob_path_edit.to_hook_json(), json!({}));

    let bob_path_bash = async_client
        .pre_tool_check(PreToolCheckRequest {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "cargo test --manifest-path /srv/workspaces/bob/repo/Cargo.toml"}),
            cwd: None,
            viewer: "bob".parse()?,
            at: AT,
            client_kind: None,
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert_eq!(bob_path_bash.permission_decision, None);
    assert_eq!(bob_path_bash.to_hook_json(), json!({}));

    // 6. Concurrent 16-client serialized single-writer stress check (zero SQLITE_BUSY!).
    let mut tasks = Vec::new();
    for idx in 0..16u64 {
        let client_c = async_client.clone();
        tasks.push(tokio::spawn(async move {
            client_c
                .notify_cursor(CursorNotification {
                    session_id: format!("concurrent_{idx}"),
                    client_kind: "codex".into(),
                    transcript_path: None,
                    byte_offset: 100 + idx,
                    line_count: 2,
                    prefix_sha256: "p".into(),
                    actor: "alice".parse()?,
                    at: AT,
                    session_ended: false,
                })
                .await?;
            client_c.status().await
        }));
    }
    for task in tasks {
        let st = task.await??;
        assert!(st.tracked_sessions >= 2);
    }

    // 7. Clean `Shutdown` RPC stops daemon and unlinks socket.
    async_client.shutdown().await?;
    daemon.shutdown().await?;
    assert!(!sock_path.exists());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_tool_check_scales_under_20ms_over_2000_observations_and_matches_natural_language_policies(
) -> Result {
    if sandbox_blocks_raw_uds_connect()? {
        return Ok(());
    }
    let (lib_dir, mut store) = library("eng-personal", "alice", LibraryMode::Personal)?;
    store.add_member("bob".parse()?, MemberRole::Member, AT)?;

    // Populate 2,100 background `observation` facts in a single SQLite transaction to simulate real project scale.
    let mut obs_inputs = Vec::with_capacity(2_100);
    for i in 0..2_100usize {
        let mut obs = base_fact(
            &format!("fact_obs_{i:04}"),
            "alice",
            FactKind::Observation,
            &["alice", "bob"],
        )?;
        obs.subject = format!("module_{}", i % 50);
        obs.predicate = "kp.core.note".into();
        obs.tags.clear();
        obs.object = json!(format!("日常架构讨论记录条目 #{i}: 模块接口与缓存细节"));
        obs_inputs.push(kp_store::FactInput {
            fact: obs,
            entities: BTreeSet::new(),
        });
    }
    store.remember(&obs_inputs)?;

    // Insert pure natural-language DB primary-replica cutover facts WITHOUT `"command": "gh pr merge"` or `"gh pr merge"` tags!
    let mut db_cutover_event = base_fact(
        "fact_db_cutover_event",
        "alice",
        FactKind::Event,
        &["alice", "bob"],
    )?;
    db_cutover_event.subject = "运维排期".into();
    db_cutover_event.predicate = "kp.core.event".into();
    db_cutover_event.tags.clear();
    db_cutover_event.object = json!({"claim_text": "下周四凌晨两点进行数据库主从切换"});
    db_cutover_event.source_anchor = Some(AnchorSpec {
        verbatim_quote: Some("下周四凌晨两点进行数据库主从切换".into()),
        ..AnchorSpec::default()
    });
    store.insert_fact(&db_cutover_event)?;

    let mut eng_cutover_rule = base_fact(
        "fact_eng_cutover_rule",
        "bob",
        FactKind::Constraint,
        &["alice", "bob"],
    )?;
    eng_cutover_rule.subject = "研发群规范".into();
    eng_cutover_rule.predicate = "kp.core.constraint".into();
    eng_cutover_rule.tags.clear();
    eng_cutover_rule.object = json!({"claim_text": "主从切换维护窗口暂停合并非回滚分支"});
    eng_cutover_rule.source_anchor = Some(AnchorSpec {
        verbatim_quote: Some("主从切换维护窗口暂停合并非回滚分支".into()),
        ..AnchorSpec::default()
    });
    store.insert_fact(&eng_cutover_rule)?;

    let cutover_warning = derived_fact(
        store.library_id(),
        &"alice".parse()?,
        FactKind::CollisionWarning,
        "bob",
        "kp.core.collision",
        json!({"claim_text": "数据库主从切换窗口合并变更需人工确认是否为回滚分支"}),
        AT,
    )?;
    let cutover_proof = justification(
        &cutover_warning,
        DerivationMode::HardRule,
        "R_CUTOVER_MERGE",
        &[&db_cutover_event, &eng_cutover_rule],
    )?;
    store.save_derivations(std::slice::from_ref(&cutover_warning), &[cutover_proof])?;

    // Concept-linked action constraint (`CommunityConcept` `Action` -> `kubectl apply` check).
    let cpt_no_deploy = CommunityConcept {
        id: "cpt_pause_deploy".parse()?,
        kind: ConceptKind::Action,
        canonical_label: "暂停部署".into(),
        domain: "ops".into(),
        polarity: ConceptPolarity::Negative,
        proposed_by: "alice".parse()?,
        created_at: AT,
    };
    store.exchange_upsert_concept(&cpt_no_deploy, true)?;
    let mut deploy_concept_guard = base_fact(
        "fact_deploy_concept_guard",
        "alice",
        FactKind::Constraint,
        &["alice", "bob"],
    )?;
    deploy_concept_guard.subject = "集群变更".into();
    deploy_concept_guard.predicate = "kp.core.constraint".into();
    deploy_concept_guard.tags.clear();
    deploy_concept_guard.object = json!({"action_concept_id": "cpt_pause_deploy"});
    store.insert_fact(&deploy_concept_guard)?;
    store.exchange_link_fact_concepts(
        &deploy_concept_guard.id,
        &["cpt_pause_deploy".parse()?],
        AT,
    )?;

    let mut no_force_push = base_fact(
        "fact_no_force_push",
        "alice",
        FactKind::Constraint,
        &["alice", "bob"],
    )?;
    no_force_push.subject = "Git规范".into();
    no_force_push.predicate = "kp.core.constraint".into();
    no_force_push.tags.clear();
    no_force_push.object = json!("禁止对主干分支强制推送");
    no_force_push.source_anchor = Some(AnchorSpec {
        verbatim_quote: Some("禁止对主干分支强制推送".into()),
        ..AnchorSpec::default()
    });
    store.insert_fact(&no_force_push)?;

    let mut protect_schema = base_fact(
        "fact_protect_schema",
        "alice",
        FactKind::Constraint,
        &["alice", "bob"],
    )?;
    protect_schema.subject = "存储约束".into();
    protect_schema.predicate = "kp.core.constraint".into();
    protect_schema.tags.clear();
    protect_schema.object = json!("不要直接修改 schema.sql");
    protect_schema.source_anchor = Some(AnchorSpec {
        verbatim_quote: Some("不要直接修改 schema.sql".into()),
        ..AnchorSpec::default()
    });
    store.insert_fact(&protect_schema)?;

    let config = DaemonConfig::for_library(lib_dir.path())?;
    let daemon = DaemonServer::spawn_default(store, config).await?;
    let sync_client = DaemonSyncClient::for_library(lib_dir.path())?;
    let async_client = DaemonClient::for_library(lib_dir.path())?;

    // Warm one ping so UDS connection setup is ready, then benchmark `gh pr merge 42` over 2,100+ facts (< 20ms!).
    let _ = async_client.ping().await?;

    let t0 = Instant::now();
    let merge_resp =
        sync_client.request(&kp_net::DaemonRequest::PreToolCheck(PreToolCheckRequest {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "gh pr merge 42"}),
            cwd: None,
            viewer: "bob".parse()?,
            at: AT,
            client_kind: None,
            time_zone: Some("UTC".into()),
        }))?;
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_millis(20),
        "expected PreToolCheck < 20ms on 2,100+ facts, got {elapsed:?}"
    );

    let kp_net::DaemonResponse::PreTool(merge_check) = merge_resp else {
        return Err("expected PreTool response".into());
    };
    assert_eq!(
        merge_check.permission_decision,
        Some(ToolPermissionDecision::Ask)
    );
    let reason = merge_check
        .permission_decision_reason
        .as_deref()
        .unwrap_or("");
    assert!(
        reason.contains("「主从切换维护窗口暂停合并非回滚分支」"),
        "missing constraint verbatim quote in reason: {reason}"
    );
    assert!(
        reason.contains("「下周四凌晨两点进行数据库主从切换」"),
        "missing event premise verbatim quote in reason: {reason}"
    );

    // Concept-linked `Action` (`cpt_pause_deploy` -> "暂停部署") triggers `Ask` on `kubectl apply`,
    // while normal non-force `git push origin feat` is NOT falsely triggered by `禁止对主干分支强制推送`!
    let deploy_res = async_client
        .pre_tool_check(PreToolCheckRequest {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "kubectl apply -f k8s/service.yaml"}),
            cwd: None,
            viewer: "bob".parse()?,
            at: AT,
            client_kind: None,
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert_eq!(
        deploy_res.permission_decision,
        Some(ToolPermissionDecision::Ask)
    );

    // Safe read/test/non-force-push commands remain 100% silent (`{}`) and fast (< 20ms median).
    for safe_cmd in [
        "cargo test --offline",
        "git status",
        "ls -la",
        "git push origin feat-branch",
    ] {
        let mut samples = Vec::with_capacity(3);
        let mut last_res = None;
        for _ in 0..3 {
            let t_safe = Instant::now();
            let res = async_client
                .pre_tool_check(PreToolCheckRequest {
                    tool_name: "Bash".into(),
                    tool_input: json!({"command": safe_cmd}),
                    cwd: None,
                    viewer: "bob".parse()?,
                    at: AT,
                    client_kind: None,
                    time_zone: Some("UTC".into()),
                })
                .await?;
            samples.push(t_safe.elapsed());
            last_res = Some(res);
        }
        samples.sort_unstable();
        assert!(
            samples[1] < Duration::from_millis(20),
            "expected <20ms median PreToolCheck for `{safe_cmd}`, got {:?}",
            samples[1]
        );
        let res = last_res.ok_or("missing response")?;
        assert_eq!(res.permission_decision, None, "{safe_cmd}");
        assert_eq!(res.to_hook_json(), json!({}));
    }

    // Natural-language force-push rule matches `git push origin main --force` and `-f`.
    let force_res = async_client
        .pre_tool_check(PreToolCheckRequest {
            tool_name: "exec_command".into(),
            tool_input: json!({"cmd": "git push origin main --force"}),
            cwd: None,
            viewer: "bob".parse()?,
            at: AT,
            client_kind: None,
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert_eq!(
        force_res.permission_decision,
        Some(ToolPermissionDecision::Ask)
    );
    assert!(force_res
        .permission_decision_reason
        .as_deref()
        .unwrap_or("")
        .contains("禁止对主干分支强制推送"));

    // Protected file mention (`schema.sql`) triggers on both Claude `Edit` and Codex `apply_patch`,
    // while editing an unrelated file stays silent.
    let schema_edit = async_client
        .pre_tool_check(PreToolCheckRequest {
            tool_name: "Edit".into(),
            tool_input: json!({"file_path": "crates/kp-store/src/schema.sql"}),
            cwd: None,
            viewer: "bob".parse()?,
            at: AT,
            client_kind: None,
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert_eq!(
        schema_edit.permission_decision,
        Some(ToolPermissionDecision::Ask)
    );

    let schema_patch = async_client
        .pre_tool_check(PreToolCheckRequest {
            tool_name: "apply_patch".into(),
            tool_input: json!({"patch": "*** Begin Patch\n*** Update File: crates/kp-store/src/schema.sql\n@@\n*** End Patch"}),
            cwd: None,
            viewer: "bob".parse()?,
            at: AT,
            client_kind: None,
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert_eq!(
        schema_patch.permission_decision,
        Some(ToolPermissionDecision::Ask)
    );

    let other_edit = async_client
        .pre_tool_check(PreToolCheckRequest {
            tool_name: "Edit".into(),
            tool_input: json!({"file_path": "docs/notes.txt"}),
            cwd: None,
            viewer: "bob".parse()?,
            at: AT,
            client_kind: None,
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert_eq!(other_edit.permission_decision, None);

    daemon.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_endpoint_counts_viewer_scoped_chunks_without_leaking_hidden_sources() -> Result {
    if sandbox_blocks_raw_uds_connect()? {
        return Ok(());
    }
    let (qdir, mut q) = library("q", "admin", LibraryMode::Community)?;
    let origin = q.library_id().clone();
    q.ingest_source(
        &origin,
        &"doc-ab".parse()?,
        &kp_store::RawSourceInput {
            uri: "chat://doc_ab".into(),
            media_type: "text/markdown".into(),
            content: b"# Cutover\nPrimary replica switch.\n".to_vec(),
            readable_by: AudienceSpec::Explicit(["alice".parse()?, "bob".parse()?].into()),
            captured_at: AT,
            chunks: vec![kp_store::ChunkInput {
                heading: "Cutover".into(),
                body: "Primary replica switch.".into(),
                anchor: AnchorSpec::default(),
            }],
            self_echo_filtered: false,
        },
    )?;

    let server = UdsHttpServer::start(q, qdir, "stat").await?;
    let alice = server.client("alice")?;
    let bob = server.client("bob")?;
    let carol = server.client("carol")?;
    let admin = server.client("admin")?;

    assert_eq!(alice.get("/v1/status?at=1763683200000").await?["chunks"], 1);
    assert_eq!(bob.get("/v1/status?at=1763683200000").await?["chunks"], 1);
    assert_eq!(carol.get("/v1/status?at=1763683200000").await?["chunks"], 0);

    // Purging the source via tombstone immediately hides the chunk on `/v1/status`.
    let ts = kp_store::Tombstone {
        tombstone_id: kp_store::tombstone_id(
            origin.as_str().as_bytes(),
            &origin,
            &"doc-ab".parse()?,
        )?,
        origin_library_id: origin,
        purged_at: AT,
        reason: "purged".into(),
    };
    admin.post("/v1/purge", &ts).await?;
    assert_eq!(alice.get("/v1/status?at=1763683200000").await?["chunks"], 0);
    assert_eq!(bob.get("/v1/status?at=1763683200000").await?["chunks"], 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn socket_directory_ownership_mode_0700_and_peer_uid_checks_reject_untrusted_endpoints(
) -> Result {
    let uid = current_process_uid()?;
    let (lib_dir, store) = library("sec-lib", "alice", LibraryMode::Personal)?;

    // 1. `daemon_socket_path` NEVER places sockets in world-writable `/tmp/kp-*.sock` and uses a `0700` dir.
    let sock_path = daemon_socket_path(lib_dir.path())?;
    let sock_str = sock_path.to_string_lossy();
    assert!(
        !sock_str.starts_with("/tmp/kp-") || sock_path.parent() != Some(Path::new("/tmp")),
        "socket must never live directly in /tmp: {sock_str}"
    );
    assert!(sock_path.as_os_str().len() < MAX_UDS_PATH_LEN);
    let parent = sock_path.parent().ok_or("missing socket parent")?;
    let parent_meta = std::fs::symlink_metadata(parent)?;
    assert_eq!(std::os::unix::fs::MetadataExt::uid(&parent_meta), uid);
    assert_eq!(parent_meta.permissions().mode() & 0o077, 0);

    // 2. Wrong-owner directory (`/tmp` owned by root UID 0 != current UID, plus explicit `uid + 1` check) is rejected!
    assert!(matches!(
        verify_socket_parent_dir_with_uid(&sock_path, uid.wrapping_add(1)),
        Err(NetError::Forbidden)
    ));
    let world_tmp_sock = PathBuf::from("/tmp/kp-insecure-root-dir.sock");
    let (_bad_dir1, bad_store1) = library("bad1", "alice", LibraryMode::Personal)?;
    assert!(matches!(
        DaemonServer::spawn_default(
            bad_store1,
            DaemonConfig {
                socket_path: world_tmp_sock.clone(),
                idle_extract_window: Duration::from_secs(60),
                idle_shutdown_timeout: None,
                max_frame_bytes: 64 * 1024,
                web_inspector: None,
            }
        )
        .await,
        Err(NetError::Forbidden)
    ));
    assert!(matches!(
        DaemonSyncClient::new(&world_tmp_sock)?.request(&kp_net::DaemonRequest::Ping),
        Err(NetError::Forbidden)
    ));

    // 3. Same-owner directory with group/world permissions (`0o755`) is rejected on both server and client!
    let group_readable_dir = tempfile::tempdir()?;
    std::fs::set_permissions(
        group_readable_dir.path(),
        std::fs::Permissions::from_mode(0o755),
    )?;
    let loose_sock = group_readable_dir.path().join("d.s");
    if validate_socket_path(&loose_sock).is_ok() {
        let (_bad_dir2, bad_store2) = library("bad2", "alice", LibraryMode::Personal)?;
        assert!(matches!(
            DaemonServer::spawn_default(
                bad_store2,
                DaemonConfig {
                    socket_path: loose_sock.clone(),
                    idle_extract_window: Duration::from_secs(60),
                    idle_shutdown_timeout: None,
                    max_frame_bytes: 64 * 1024,
                    web_inspector: None,
                }
            )
            .await,
            Err(NetError::Forbidden)
        ));
        assert!(matches!(
            DaemonClient::new(&loose_sock)?.ping().await,
            Err(NetError::Forbidden)
        ));
        assert!(matches!(
            DaemonSyncClient::new(&loose_sock)?.request(&kp_net::DaemonRequest::Ping),
            Err(NetError::Forbidden)
        ));
    }

    // 4. Server-side `peer_cred()` UID check passes for `uid` and rejects `uid + 1`!
    let (tok_a, _tok_b) = tokio::net::UnixStream::pair()?;
    verify_stream_peer_uid(&tok_a, uid)?;
    assert!(matches!(
        verify_stream_peer_uid(&tok_a, uid.wrapping_add(1)),
        Err(NetError::Forbidden)
    ));

    // 5. Production socketpair gate: when `test-inproc-uds` is NOT enabled and sandbox blocks raw `connect`,
    //    `DaemonSyncClient` / `DaemonClient` fail closed with `NetError::Unavailable`!
    if sandbox_blocks_raw_uds_connect()? {
        let daemon = DaemonServer::spawn_default(
            store,
            DaemonConfig {
                socket_path: sock_path.clone(),
                idle_extract_window: Duration::from_secs(60),
                idle_shutdown_timeout: None,
                max_frame_bytes: 64 * 1024,
                web_inspector: None,
            },
        )
        .await?;
        assert!(matches!(
            DaemonSyncClient::new(&sock_path)?.request(&kp_net::DaemonRequest::Ping),
            Err(NetError::Unavailable)
        ));
        daemon.shutdown().await?;
        return Ok(());
    }

    // Bounded `DaemonSyncClient::request` `.take(max_frame_bytes + 1)` rejects oversized frames.
    let daemon = DaemonServer::spawn_default(
        store,
        DaemonConfig {
            socket_path: sock_path.clone(),
            idle_extract_window: Duration::from_secs(60),
            idle_shutdown_timeout: None,
            max_frame_bytes: 64 * 1024,
            web_inspector: None,
        },
    )
    .await?;
    let tiny_sync = DaemonSyncClient::new(&sock_path)?.with_max_frame_bytes(16);
    assert!(matches!(
        tiny_sync.request(&kp_net::DaemonRequest::Status),
        Err(NetError::Invalid("daemon frame exceeds max_frame_bytes"))
    ));
    daemon.shutdown().await?;

    // End-to-end server-side peer UID mismatch rejection:
    #[cfg(feature = "test-inproc-uds")]
    {
        let (_s2_dir, store2) = library("sec-srv-uid", "alice", LibraryMode::Personal)?;
        let sock2 = short_test_socket("uid")?;
        let strict_daemon = DaemonServer::spawn_with_expected_peer_uid_for_test(
            store2,
            DaemonConfig {
                socket_path: sock2.clone(),
                idle_extract_window: Duration::from_secs(60),
                idle_shutdown_timeout: None,
                max_frame_bytes: 64 * 1024,
                web_inspector: None,
            },
            Arc::new(NoopDaemonHandler),
            uid.wrapping_add(1),
        )
        .await?;
        assert!(matches!(
            DaemonClient::new(&sock2)?.ping().await,
            Err(NetError::Forbidden | NetError::Unavailable | NetError::Io(_))
        ));
        assert!(matches!(
            DaemonSyncClient::new(&sock2)?.request(&kp_net::DaemonRequest::Ping),
            Err(NetError::Forbidden | NetError::Unavailable | NetError::Io(_))
        ));
        strict_daemon.shutdown().await?;
    }

    Ok(())
}

struct FlakyAndQuotaHandler {
    transient_failures_left: AtomicUsize,
    hold_quota: AtomicBool,
    calls: AtomicUsize,
    quota_hit_notify: Notify,
    done_notify: Notify,
    completed: Mutex<Vec<IdleSessionBatch>>,
}

impl FlakyAndQuotaHandler {
    fn new(transient_failures: usize, hold_quota: bool) -> Self {
        Self {
            transient_failures_left: AtomicUsize::new(transient_failures),
            hold_quota: AtomicBool::new(hold_quota),
            calls: AtomicUsize::new(0),
            quota_hit_notify: Notify::new(),
            done_notify: Notify::new(),
            completed: Mutex::new(Vec::new()),
        }
    }
}

impl DaemonHandler for FlakyAndQuotaHandler {
    fn extract_session_batch<'a>(
        &'a self,
        _library_dir: &'a Path,
        batch: IdleSessionBatch,
    ) -> BoxFuture<'a, kp_net::Result<Option<u64>>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.transient_failures_left.load(Ordering::SeqCst) > 0 {
                self.transient_failures_left.fetch_sub(1, Ordering::SeqCst);
                return Err(NetError::Daemon("transient subprocess failure".into()));
            }
            if self.hold_quota.load(Ordering::SeqCst) {
                self.quota_hit_notify.notify_waiters();
                return Err(NetError::Daemon("HTTP 429 rate limit exceeded".into()));
            }
            if let Ok(mut guard) = self.completed.lock() {
                guard.push(batch.clone());
            }
            self.done_notify.notify_waiters();
            Ok(Some(batch.end_byte_offset))
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_cursors_retry_with_exponential_backoff_pause_on_quota_and_resume_after_daemon_restart(
) -> Result {
    if sandbox_blocks_raw_uds_connect()? {
        return Ok(());
    }

    // (a) 1st extraction attempt fails transiently on a `SessionEnd` batch -> 2nd attempt succeeds via backoff!
    let (lib_dir, store) = library("retry-lib", "alice", LibraryMode::Personal)?;
    let sock_path = short_test_socket("rty")?;
    let handler = Arc::new(FlakyAndQuotaHandler::new(1, false));
    let done_transient = handler.done_notify.notified();
    let daemon = DaemonServer::spawn(
        store,
        DaemonConfig {
            socket_path: sock_path.clone(),
            idle_extract_window: Duration::from_millis(30),
            idle_shutdown_timeout: None,
            max_frame_bytes: 64 * 1024,
            web_inspector: None,
        },
        Arc::clone(&handler) as Arc<dyn DaemonHandler>,
    )
    .await?;
    let client = DaemonClient::new(&sock_path)?;
    client
        .notify_cursor(CursorNotification {
            session_id: "sess_transient_fail".into(),
            client_kind: "codex".into(),
            transcript_path: Some("/tmp/sess_transient.jsonl".into()),
            byte_offset: 640,
            line_count: 8,
            prefix_sha256: "px1".into(),
            actor: "alice".parse()?,
            at: AT,
            session_ended: true,
        })
        .await?;

    tokio::time::timeout(Duration::from_secs(5), done_transient).await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if client.status().await?.pending_extractions == 0 {
                return Ok::<(), Box<dyn Error>>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert_eq!(client.status().await?.pending_extractions, 0);
    assert!(handler.calls.load(Ordering::SeqCst) >= 2);
    {
        let done = handler.completed.lock().map_err(|_| "lock")?;
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].session_id, "sess_transient_fail");
        assert_eq!(done[0].end_byte_offset, 640);
    }

    // Quota exhaustion (`429 rate limit exceeded`) marks `paused_quota` in SQLite and recovers after cooldown!
    let quota_handler = Arc::new(FlakyAndQuotaHandler::new(0, true));
    daemon.shutdown().await?;
    let reopened_for_quota = Store::open(lib_dir.path())?;
    let sock_q = short_test_socket("qta")?;
    let daemon_q = DaemonServer::spawn(
        reopened_for_quota,
        DaemonConfig {
            socket_path: sock_q.clone(),
            idle_extract_window: Duration::from_millis(30),
            idle_shutdown_timeout: None,
            max_frame_bytes: 64 * 1024,
            web_inspector: None,
        },
        Arc::clone(&quota_handler) as Arc<dyn DaemonHandler>,
    )
    .await?;
    let client_q = DaemonClient::new(&sock_q)?;
    let quota_notified = quota_handler.quota_hit_notify.notified();
    client_q
        .notify_cursor(CursorNotification {
            session_id: "sess_quota".into(),
            client_kind: "claude-code".into(),
            transcript_path: None,
            byte_offset: 512,
            line_count: 6,
            prefix_sha256: "pxq".into(),
            actor: "alice".parse()?,
            at: AT,
            session_ended: true,
        })
        .await?;

    // Deterministically await `PausedQuota` persistence before releasing `hold_quota`.
    tokio::time::timeout(Duration::from_secs(5), quota_notified).await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let ro = Store::open(lib_dir.path())?;
            if let Some((ExtractionJobStatus::PausedQuota, _, _)) =
                ro.daemon_session_cursor_retry_state("sess_quota")?
            {
                return Ok::<(), Box<dyn Error>>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;

    let quota_done = quota_handler.done_notify.notified();
    quota_handler.hold_quota.store(false, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(5), quota_done).await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if client_q.status().await?.pending_extractions == 0 {
                return Ok::<(), Box<dyn Error>>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert_eq!(client_q.status().await?.pending_extractions, 0);
    assert!(quota_handler.calls.load(Ordering::SeqCst) >= 2);
    daemon_q.shutdown().await?;

    // (b) Daemon restart resumes unextracted `session_cursors` on startup with ZERO new `NotifyCursor` RPCs!
    let (restart_dir, store_first) = library("restart-lib", "alice", LibraryMode::Personal)?;
    let sock_r1 = short_test_socket("rs1")?;
    let noop_handler = Arc::new(FlakyAndQuotaHandler::new(0, false));
    let daemon_r1 = DaemonServer::spawn(
        store_first,
        DaemonConfig {
            socket_path: sock_r1.clone(),
            // Long 60s window so daemon #1 does NOT extract before shutting down!
            idle_extract_window: Duration::from_secs(60),
            idle_shutdown_timeout: None,
            max_frame_bytes: 64 * 1024,
            web_inspector: None,
        },
        Arc::clone(&noop_handler) as Arc<dyn DaemonHandler>,
    )
    .await?;
    let client_r1 = DaemonClient::new(&sock_r1)?;
    client_r1
        .notify_cursor(CursorNotification {
            session_id: "sess_survives_restart".into(),
            client_kind: "codex".into(),
            transcript_path: Some("/tmp/survive.jsonl".into()),
            byte_offset: 1024,
            line_count: 10,
            prefix_sha256: "px_survive".into(),
            actor: "alice".parse()?,
            at: AT,
            session_ended: false,
        })
        .await?;
    assert_eq!(client_r1.status().await?.pending_extractions, 1);
    daemon_r1.shutdown().await?;
    assert_eq!(noop_handler.calls.load(Ordering::SeqCst), 0);

    // Spawn daemon #2 on the reopened `Store` WITHOUT sending any `NotifyCursor` — startup scan extracts `0..1024`!
    let reopened_store = Store::open(restart_dir.path())?;
    let sock_r2 = short_test_socket("rs2")?;
    let resume_handler = Arc::new(FlakyAndQuotaHandler::new(0, false));
    let resume_done = resume_handler.done_notify.notified();
    let daemon_r2 = DaemonServer::spawn(
        reopened_store,
        DaemonConfig {
            socket_path: sock_r2.clone(),
            idle_extract_window: Duration::from_millis(30),
            idle_shutdown_timeout: None,
            max_frame_bytes: 64 * 1024,
            web_inspector: None,
        },
        Arc::clone(&resume_handler) as Arc<dyn DaemonHandler>,
    )
    .await?;
    let client_r2 = DaemonClient::new(&sock_r2)?;
    tokio::time::timeout(Duration::from_secs(5), resume_done).await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if client_r2.status().await?.pending_extractions == 0 {
                return Ok::<(), Box<dyn Error>>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert_eq!(client_r2.status().await?.pending_extractions, 0);
    {
        let recovered = resume_handler.completed.lock().map_err(|_| "lock")?;
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].session_id, "sess_survives_restart");
        assert_eq!(recovered[0].start_byte_offset, 0);
        assert_eq!(recovered[0].end_byte_offset, 1024);
    }
    daemon_r2.shutdown().await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_word_tokenization_and_path_segment_suffix_prevent_merge_base_and_stdlib_false_positives(
) -> Result {
    if sandbox_blocks_raw_uds_connect()? {
        return Ok(());
    }
    let (dir, mut store) = library("guard-precision", "alice", LibraryMode::Personal)?;

    // 1. Merge guard (with `git merge` in `object.command`, prose, and merge verb) must NEVER match `git merge-base`!
    let mut merge_rule = base_fact(
        "fact_git_merge_rule",
        "alice",
        FactKind::Constraint,
        &["alice"],
    )?;
    merge_rule.subject = "分支规则".into();
    merge_rule.tags.clear();
    merge_rule.object = json!({
        "command": "git merge",
        "claim_text": "维护窗口暂停执行 git merge 合并分支"
    });
    store.insert_fact(&merge_rule)?;

    // 2. Protected `lib.rs` guard (via prose, `file_path`, and `Resource`) must NEVER match `stdlib.rs`!
    let mut lib_rule = base_fact(
        "fact_protect_lib_rs",
        "alice",
        FactKind::Constraint,
        &["alice"],
    )?;
    lib_rule.subject = "核心入口".into();
    lib_rule.tags.clear();
    lib_rule.governed_scope = Some(GovernedScope::Resource {
        target: "lib.rs".into(),
    });
    lib_rule.object = json!({
        "file_path": "lib.rs",
        "claim_text": "不要直接修改 lib.rs"
    });
    store.insert_fact(&lib_rule)?;

    let sock = short_test_socket("gpc")?;
    let daemon = DaemonServer::spawn(
        store,
        DaemonConfig {
            socket_path: sock.clone(),
            idle_extract_window: Duration::from_secs(60),
            idle_shutdown_timeout: None,
            max_frame_bytes: 64 * 1024,
            web_inspector: None,
        },
        Arc::new(NoopDaemonHandler),
    )
    .await?;
    let client = DaemonClient::new(&sock)?;
    let viewer: ActorId = "alice".parse()?;

    for readonly_cmd in [
        "git merge-base HEAD main",
        "git -C /srv/repo merge-base origin/main feat",
        "git mergetool",
        "git merge-tree HEAD main",
    ] {
        let res = client
            .pre_tool_check(PreToolCheckRequest {
                tool_name: "Bash".into(),
                tool_input: json!({"command": readonly_cmd}),
                cwd: None,
                viewer: viewer.clone(),
                at: AT,
                client_kind: None,
                time_zone: Some("UTC".into()),
            })
            .await?;
        assert_eq!(
            res.permission_decision, None,
            "false positive on `{readonly_cmd}`"
        );
    }

    for actual_merge_cmd in [
        "git merge feat/branch",
        "git -C /srv/repo merge feat/branch",
        "cargo check && gh pr merge 99",
    ] {
        let res = client
            .pre_tool_check(PreToolCheckRequest {
                tool_name: "Bash".into(),
                tool_input: json!({"command": actual_merge_cmd}),
                cwd: None,
                viewer: viewer.clone(),
                at: AT,
                client_kind: None,
                time_zone: Some("UTC".into()),
            })
            .await?;
        assert_eq!(
            res.permission_decision,
            Some(ToolPermissionDecision::Ask),
            "missed actual merge `{actual_merge_cmd}`"
        );
    }

    for non_matching_path in ["src/stdlib.rs", "stdlib.rs", "crates/kp-net/src/mylib.rs"] {
        let edit_res = client
            .pre_tool_check(PreToolCheckRequest {
                tool_name: "Edit".into(),
                tool_input: json!({"file_path": non_matching_path}),
                cwd: None,
                viewer: viewer.clone(),
                at: AT,
                client_kind: None,
                time_zone: Some("UTC".into()),
            })
            .await?;
        assert_eq!(
            edit_res.permission_decision, None,
            "false positive on `{non_matching_path}`"
        );

        let bash_res = client
            .pre_tool_check(PreToolCheckRequest {
                tool_name: "Bash".into(),
                tool_input: json!({"command": format!("wc -l {non_matching_path}")}),
                cwd: None,
                viewer: viewer.clone(),
                at: AT,
                client_kind: None,
                time_zone: Some("UTC".into()),
            })
            .await?;
        assert_eq!(
            bash_res.permission_decision, None,
            "false positive on bash `{non_matching_path}`"
        );
    }

    for matching_path in ["lib.rs", "src/lib.rs", "crates/kp-net/src/lib.rs"] {
        let edit_res = client
            .pre_tool_check(PreToolCheckRequest {
                tool_name: "Edit".into(),
                tool_input: json!({"file_path": matching_path}),
                cwd: None,
                viewer: viewer.clone(),
                at: AT,
                client_kind: None,
                time_zone: Some("UTC".into()),
            })
            .await?;
        assert_eq!(
            edit_res.permission_decision,
            Some(ToolPermissionDecision::Ask),
            "missed protected path `{matching_path}`"
        );
    }

    // 3. Verify Claude (`Ask`, preserves `allow_guard_once`) vs Codex (`Deny` + single-use `kp allow`) over UDS:
    {
        let mut admin = Store::open(dir.path())?;
        admin.grant_guard_allow_once(&viewer, &merge_rule.id, 600_000, AT)?;
    }

    // Claude Code returns `Ask` and leaves the `allow_guard_once` grant unconsumed.
    let claude_res = client
        .pre_tool_check(PreToolCheckRequest {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "git merge feat/branch"}),
            cwd: None,
            viewer: viewer.clone(),
            at: EpochMs(AT.0 + 1_000),
            client_kind: Some("claude-code".into()),
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert_eq!(
        claude_res.permission_decision,
        Some(ToolPermissionDecision::Ask)
    );
    assert_eq!(
        claude_res.allow_refs,
        vec!["fact_git_merge_rule".to_owned()]
    );

    // First Codex invocation consumes the single-use grant and returns `{}`.
    let codex_pass = client
        .pre_tool_check(PreToolCheckRequest {
            tool_name: "exec_command".into(),
            tool_input: json!({"cmd": "git merge feat/branch"}),
            cwd: None,
            viewer: viewer.clone(),
            at: EpochMs(AT.0 + 2_000),
            client_kind: Some("codex".into()),
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert_eq!(codex_pass, PreToolCheckResult::default());
    assert_eq!(codex_pass.to_hook_json(), json!({}));

    // Second Codex invocation is denied with `kp allow fact_git_merge_rule`.
    let codex_deny = client
        .pre_tool_check(PreToolCheckRequest {
            tool_name: "exec_command".into(),
            tool_input: json!({"cmd": "git merge feat/branch"}),
            cwd: None,
            viewer: viewer.clone(),
            at: EpochMs(AT.0 + 3_000),
            client_kind: Some("codex".into()),
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert_eq!(
        codex_deny.permission_decision,
        Some(ToolPermissionDecision::Deny)
    );
    assert!(codex_deny
        .permission_decision_reason
        .as_deref()
        .unwrap_or("")
        .ends_with("\n先问用户；用户同意后，请用户执行 kp allow fact_git_merge_rule。"));

    // 4. All-or-nothing multi-guard Codex check (`git merge` + `lib.rs` in one command):
    //    granting ONLY `merge_rule` consumes ZERO grants on `cat src/lib.rs && git merge feat`,
    //    then granting `lib_rule` atomically consumes both!
    {
        let mut admin = Store::open(dir.path())?;
        admin.grant_guard_allow_once(&viewer, &merge_rule.id, 600_000, EpochMs(AT.0 + 4_000))?;
    }
    for check_at in [EpochMs(AT.0 + 5_000), EpochMs(AT.0 + 5_500)] {
        let partial = client
            .pre_tool_check(PreToolCheckRequest {
                tool_name: "exec_command".into(),
                tool_input: json!({"cmd": "cat src/lib.rs && git merge feat/branch"}),
                cwd: None,
                viewer: viewer.clone(),
                at: check_at,
                client_kind: Some("codex".into()),
                time_zone: Some("UTC".into()),
            })
            .await?;
        assert_eq!(
            partial.permission_decision,
            Some(ToolPermissionDecision::Deny)
        );
        assert_eq!(partial.matched_fact_ids, vec![lib_rule.id.clone()]);
        assert_eq!(partial.allow_refs, vec!["fact_protect_lib_rs".to_owned()]);
    }
    {
        let mut admin = Store::open(dir.path())?;
        admin.grant_guard_allow_once(&viewer, &lib_rule.id, 600_000, EpochMs(AT.0 + 6_000))?;
    }
    let both_pass = client
        .pre_tool_check(PreToolCheckRequest {
            tool_name: "exec_command".into(),
            tool_input: json!({"cmd": "cat src/lib.rs && git merge feat/branch"}),
            cwd: None,
            viewer: viewer.clone(),
            at: EpochMs(AT.0 + 7_000),
            client_kind: Some("codex".into()),
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert_eq!(both_pass, PreToolCheckResult::default());

    let both_spent = client
        .pre_tool_check(PreToolCheckRequest {
            tool_name: "exec_command".into(),
            tool_input: json!({"cmd": "cat src/lib.rs && git merge feat/branch"}),
            cwd: None,
            viewer,
            at: EpochMs(AT.0 + 8_000),
            client_kind: Some("codex".into()),
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert_eq!(
        both_spent.permission_decision,
        Some(ToolPermissionDecision::Deny)
    );
    assert_eq!(both_spent.allow_refs.len(), 2);

    daemon.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uds_status_actor_ordered_audience_revisions_and_inflight_retraction_vs_purge() -> Result {
    if sandbox_blocks_raw_uds_connect()? {
        return Ok(());
    }
    let (qdir, mut q) = library("q", "admin", LibraryMode::Community)?;
    let scope_abc = SourceScope {
        scope_id: "scope_study_room".parse()?,
        provider: "container".into(),
        external_container_id: "room_abc".into(),
        label: "自习室".into(),
        current_audience: AudienceSpec::Explicit(
            ["alice".parse()?, "bob".parse()?, "carol".parse()?].into(),
        ),
        synced_at: AT,
    };
    q.upsert_source_scope(&scope_abc)?;
    let dave_token = q.add_member("dave".parse()?, MemberRole::Member, AT)?.token;
    let server = UdsHttpServer::start(q, qdir, "audrev").await?;
    let alice = server.client("alice")?;
    let bob = server.client("bob")?;
    let carol = server.client("carol")?;
    let dave = CommunityClient::with_unix_socket(&server.url, &dave_token, &server.socket_path)?;

    // `/v1/status` returns authenticated `"actor"`.
    assert_eq!(
        alice.get("/v1/status?at=1763683200000").await?["actor"],
        "alice"
    );
    assert_eq!(
        bob.get("/v1/status?at=1763683200000").await?["actor"],
        "bob"
    );
    assert_eq!(
        carol.get("/v1/status?at=1763683200000").await?["actor"],
        "carol"
    );
    assert_eq!(
        dave.get("/v1/status?at=1763683200000").await?["actor"],
        "dave"
    );

    // Dynamic `RelationGroup` widening & shrinking via `auto_publish_and_sync` + `source_scopes` ceiling + stale rev rejection.
    let (_adir, mut a) = library("alice-personal", "alice", LibraryMode::Personal)?;
    a.add_member("bob".parse()?, MemberRole::Member, AT)?;
    a.add_member("carol".parse()?, MemberRole::Member, AT)?;
    a.add_member("dave".parse()?, MemberRole::Member, AT)?;
    a.upsert_source_scope(&scope_abc)?;

    let cpt_group = CommunityConcept {
        id: "cpt_study_group".parse()?,
        kind: ConceptKind::Group,
        canonical_label: "晚自习组".into(),
        domain: "study".into(),
        polarity: ConceptPolarity::Neutral,
        proposed_by: "alice".parse()?,
        created_at: AT,
    };
    let cpt_cond = CommunityConcept {
        id: "cpt_study_cond".parse()?,
        kind: ConceptKind::Condition,
        canonical_label: "晚间自习时段".into(),
        domain: "study".into(),
        polarity: ConceptPolarity::Positive,
        proposed_by: "alice".parse()?,
        created_at: AT,
    };
    let cpt_act = CommunityConcept {
        id: "cpt_study_act".parse()?,
        kind: ConceptKind::Action,
        canonical_label: "保持静音".into(),
        domain: "study".into(),
        polarity: ConceptPolarity::Negative,
        proposed_by: "alice".parse()?,
        created_at: AT,
    };
    for c in [&cpt_group, &cpt_cond, &cpt_act] {
        a.exchange_upsert_concept(c, true)?;
    }

    let mut mem_bob = base_fact("mem_bob", "alice", FactKind::Membership, &["alice", "bob"])?;
    mem_bob.predicate = "kp.core.membership".into();
    mem_bob.tags.clear();
    mem_bob.object = serde_json::to_value(MembershipRelation {
        anchor: "alice".parse()?,
        member: "bob".parse()?,
        group_concept_id: "cpt_study_group".parse()?,
    })?;
    a.insert_fact(&mem_bob)?;

    let mut rule = base_fact("study_rule", "alice", FactKind::Constraint, &["alice"])?;
    rule.source_scope_id = Some("scope_study_room".parse()?);
    rule.governed_scope = Some(GovernedScope::RelationGroup {
        anchor: "alice".parse()?,
        group_concept_id: "cpt_study_group".parse()?,
    });
    rule.object = json!({
        "claim_text": "晚自习组成员在自习时段保持静音",
        "condition_concept_id": "cpt_study_cond",
        "action_concept_id": "cpt_study_act"
    });
    a.insert_fact(&rule)?;

    let (_bdir, mut b) = library("bob-personal", "bob", LibraryMode::Personal)?;
    b.exchange_upsert_concept(&cpt_cond, true)?;
    let mut bob_evt = base_fact(
        "study_evt",
        "bob",
        FactKind::Event,
        &["alice", "bob", "carol"],
    )?;
    bob_evt.object = json!({
        "claim_text": "晚间自习时段进行小组语音讨论",
        "condition_concept_id": "cpt_study_cond"
    });
    b.insert_fact(&bob_evt)?;
    kp_net::publish(&mut b, "study_evt", &bob, &PublishOptions::default(), AT).await?;

    kp_net::auto_publish_and_sync(&mut a, &alice, AT).await?;
    kp_net::sync(&mut b, &bob, AT).await?;
    assert!(b
        .visible_facts(&"bob".parse()?, AT)?
        .iter()
        .any(|f| f.kind == FactKind::CollisionWarning));
    assert_eq!(carol.get("/v1/status?at=1763683200000").await?["facts"], 1);

    // Widen `RelationGroup` to Carol + Dave -> `scope_study_room` ceiling caps to `{alice, bob, carol}` (Dave sees 0)!
    for (mid, member) in [("mem_carol", "carol"), ("mem_dave", "dave")] {
        let mut m = base_fact(mid, "alice", FactKind::Membership, &["alice"])?;
        m.predicate = "kp.core.membership".into();
        m.tags.clear();
        m.object = serde_json::to_value(MembershipRelation {
            anchor: "alice".parse()?,
            member: member.parse()?,
            group_concept_id: "cpt_study_group".parse()?,
        })?;
        a.insert_fact(&m)?;
    }
    kp_net::auto_publish_and_sync(&mut a, &alice, AT).await?;
    assert!(
        carol.get("/v1/status?at=1763683200000").await?["facts"]
            .as_u64()
            .unwrap_or(0)
            >= 2
    );
    assert_eq!(dave.get("/v1/status?at=1763683200000").await?["facts"], 0);

    // Stale lower `audience_revision: 1` replay does NOT shrink Carol out!
    let mut stale_fact = rule.clone();
    stale_fact.access = FactAccess::Base {
        audience: AudienceSpec::Explicit(["alice".parse()?, "bob".parse()?].into()),
        stamp: BitemporalStamp {
            valid: ValidityWindow {
                valid_from: EpochMs(0),
                valid_until: EpochMs::OPEN_END,
            },
            recorded_at: AT,
            expired_at: None,
        },
    };
    let stale_req = PublishRequest {
        origin_reference: stale_fact.strong_ref(),
        fact: stale_fact,
        entities: BTreeSet::new(),
        idempotency_key: "stale-rev-1".into(),
        audience_revision: 1,
        skill_audience: None,
        concepts: Vec::new(),
        source_scope: None,
    };
    alice.post("/v1/publish", &stale_req).await?;
    assert!(
        carol.get("/v1/status?at=1763683200000").await?["facts"]
            .as_u64()
            .unwrap_or(0)
            >= 2
    );

    // Shrink `RelationGroup` back to `{alice}` -> Bob's sync hides BOTH mirrored rule and derived `CollisionWarning`!
    a.forget("mem_bob", &"alice".parse()?, AT)?;
    a.forget("mem_carol", &"alice".parse()?, AT)?;
    kp_net::auto_publish_and_sync(&mut a, &alice, AT).await?;
    kp_net::sync(&mut b, &bob, AT).await?;
    assert!(!b
        .visible_facts(&"bob".parse()?, AT)?
        .iter()
        .any(|f| f.kind == FactKind::CollisionWarning || f.kind == FactKind::Constraint));

    // Supersede published claim -> queues `'withdraw'`; delayed `/v1/publish` retry returns 404 (never resurrects!).
    let f_sup_v1 = base_fact("sup_v1", "alice", FactKind::Event, &["alice", "bob"])?;
    let mut f_sup_v2 = base_fact("sup_v2", "alice", FactKind::Event, &["alice", "bob"])?;
    f_sup_v2.object = json!("周五读书会改期");
    a.insert_fact(&f_sup_v1)?;
    kp_net::publish(&mut a, "sup_v1", &alice, &PublishOptions::default(), AT).await?;
    assert_eq!(bob.get("/v1/status?at=1763683200000").await?["facts"], 2);
    a.supersede(&f_sup_v1.id, &f_sup_v2)?;
    assert_eq!(a.pending_outbox(alice.url())?[0].kind, "withdraw");
    kp_net::sync(&mut a, &alice, AT).await?;
    assert_eq!(bob.get("/v1/status?at=1763683200000").await?["facts"], 1);

    let delayed_retry = PublishRequest {
        origin_reference: f_sup_v1.strong_ref(),
        fact: f_sup_v1,
        entities: BTreeSet::new(),
        idempotency_key: "delayed-resurrect".into(),
        audience_revision: 9,
        skill_audience: None,
        concepts: Vec::new(),
        source_scope: None,
    };
    assert!(matches!(
        alice.post("/v1/publish", &delayed_retry).await,
        Err(NetError::Http(404))
    ));

    // In-flight publish race: local `forget` vs `purge` finalized in 2-pass `sync` flush (`withdraw` vs `tombstone`).
    let f_inf_w = base_fact("inflight_w", "alice", FactKind::Event, &["alice", "bob"])?;
    a.insert_fact(&f_inf_w)?;
    a.forget("inflight_w", &"alice".parse()?, AT)?;
    for e in a.pending_outbox(alice.url())? {
        a.outbox_result(e.id, Some(AT))?;
    }
    let req_w = PublishRequest {
        origin_reference: f_inf_w.strong_ref(),
        fact: f_inf_w.clone(),
        entities: BTreeSet::new(),
        idempotency_key: "inf-w".into(),
        audience_revision: 1,
        skill_audience: None,
        concepts: Vec::new(),
        source_scope: None,
    };
    a.enqueue(
        alice.url(),
        "publish",
        "inf-w",
        &serde_json::to_value(&req_w)?,
    )?;
    let sync_w = kp_net::sync(&mut a, &alice, AT).await?;
    assert_eq!(sync_w.delivered, 1);
    assert!(a.pending_outbox(alice.url())?.is_empty());
    assert_eq!(bob.get("/v1/status?at=1763683200000").await?["facts"], 1);

    let f_inf_p = base_fact("inflight_p", "alice", FactKind::Event, &["alice", "bob"])?;
    a.insert_fact(&f_inf_p)?;
    let req_p = PublishRequest {
        origin_reference: f_inf_p.strong_ref(),
        fact: f_inf_p.clone(),
        entities: BTreeSet::new(),
        idempotency_key: "inf-p".into(),
        audience_revision: 1,
        skill_audience: None,
        concepts: Vec::new(),
        source_scope: None,
    };
    a.purge(&f_inf_p.origin_library, &f_inf_p.logical_key, AT, &[])?;
    for e in a.pending_outbox(alice.url())? {
        a.outbox_result(e.id, Some(AT))?;
    }
    a.enqueue(
        alice.url(),
        "publish",
        "inf-p",
        &serde_json::to_value(&req_p)?,
    )?;
    let sync_p = kp_net::sync(&mut a, &alice, AT).await?;
    assert_eq!(sync_p.delivered, 1);
    assert!(a.pending_outbox(alice.url())?.is_empty());
    assert!(Store::open(server.dir.path())?
        .is_tombstoned(&f_inf_p.origin_library, &f_inf_p.logical_key)?);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_advisory_lock_preserves_live_socket_and_idle_shutdown_waits_out_inflight_work(
) -> Result {
    if sandbox_blocks_raw_uds_connect()? {
        return Ok(());
    }
    let (lib_dir, store) = library("lock-idle-lib", "alice", LibraryMode::Personal)?;
    let cfg = DaemonConfig::for_library(lib_dir.path())?;
    let sock = cfg.socket_path.clone();

    assert!(!kp_net::is_daemon_lock_held(lib_dir.path()));
    let daemon = DaemonServer::spawn_default(store, cfg.clone()).await?;
    assert!(kp_net::is_daemon_lock_held(lib_dir.path()));
    assert!(kp_net::is_socket_lock_held(&sock));

    let ino_before = std::os::unix::fs::MetadataExt::ino(&std::fs::symlink_metadata(&sock)?);
    let (_dup_dir, dup_store) = library("dup-lock", "alice", LibraryMode::Personal)?;
    assert!(matches!(
        DaemonServer::spawn_default(dup_store, cfg.clone()).await,
        Err(NetError::Invalid("daemon already running on socket"))
    ));
    let ino_after = std::os::unix::fs::MetadataExt::ino(&std::fs::symlink_metadata(&sock)?);
    assert_eq!(ino_before, ino_after);
    assert_eq!(
        DaemonClient::for_library(lib_dir.path())?
            .ping()
            .await?
            .0
            .as_str(),
        "lock-idle-lib"
    );
    daemon.shutdown().await?;
    assert!(!kp_net::is_daemon_lock_held(lib_dir.path()));

    // Verify `idle_shutdown_timeout` waits out slow in-flight extraction + retry backoff and exits cleanly despite periodic ticks.
    struct SlowRetryHandler {
        fail_first: AtomicBool,
        calls: AtomicUsize,
        done: Notify,
    }
    impl DaemonHandler for SlowRetryHandler {
        fn extract_session_batch<'a>(
            &'a self,
            _library_dir: &'a Path,
            batch: IdleSessionBatch,
        ) -> BoxFuture<'a, kp_net::Result<Option<u64>>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(160)).await;
                if self.fail_first.swap(false, Ordering::SeqCst) {
                    return Err(NetError::Daemon("transient".into()));
                }
                self.done.notify_waiters();
                Ok(Some(batch.end_byte_offset))
            })
        }
    }

    let reopened = Store::open(lib_dir.path())?;
    let handler = Arc::new(SlowRetryHandler {
        fail_first: AtomicBool::new(true),
        calls: AtomicUsize::new(0),
        done: Notify::new(),
    });
    let done_fut = handler.done.notified();
    let idle_cfg = DaemonConfig {
        socket_path: sock.clone(),
        idle_extract_window: Duration::from_millis(20),
        idle_shutdown_timeout: None,
        max_frame_bytes: 64 * 1024,
        web_inspector: None,
    }
    .with_idle_shutdown_timeout(Some(Duration::from_millis(120)));

    let mut idle_daemon = DaemonServer::spawn(
        reopened,
        idle_cfg,
        Arc::clone(&handler) as Arc<dyn DaemonHandler>,
    )
    .await?;
    let client = DaemonClient::new(&sock)?;
    client
        .notify_cursor(CursorNotification {
            session_id: "uds_idle_slow".into(),
            client_kind: "codex".into(),
            transcript_path: None,
            byte_offset: 300,
            line_count: 3,
            prefix_sha256: "p1".into(),
            actor: "alice".parse()?,
            at: AT,
            session_ended: true,
        })
        .await?;

    tokio::time::timeout(Duration::from_secs(5), done_fut).await?;
    assert_eq!(handler.calls.load(Ordering::SeqCst), 2);

    tokio::time::timeout(Duration::from_secs(3), idle_daemon.wait()).await?;
    idle_daemon.shutdown().await?;
    assert!(!sock.exists());

    // Verify `pending_outbox > 0` blocks `idle_shutdown_timeout` until outbox is flushed,
    // and rapid `shutdown() -> spawn()` on the same `sock` never unlinks the successor socket!
    let mut store_outbox = Store::open(lib_dir.path())?;
    store_outbox.enqueue(
        "http://offline-community.local",
        "withdraw",
        "pending-withdraw-guard",
        &json!({"fact_id": "published-1"}),
    )?;
    assert_eq!(store_outbox.daemon_pending_outbox_count()?, 1);

    struct OutboxGateHandler {
        allow_flush: AtomicBool,
    }
    impl DaemonHandler for OutboxGateHandler {
        fn sync_and_derive<'a>(
            &'a self,
            library_dir: &'a Path,
            at: EpochMs,
        ) -> BoxFuture<'a, kp_net::Result<usize>> {
            let dir = library_dir.to_path_buf();
            Box::pin(async move {
                if self.allow_flush.load(Ordering::SeqCst) {
                    let mut st = Store::open(&dir).map_err(|e| NetError::Daemon(e.to_string()))?;
                    for entry in st
                        .pending_outbox("http://offline-community.local")
                        .map_err(|e| NetError::Daemon(e.to_string()))?
                    {
                        st.outbox_result(entry.id, Some(at))
                            .map_err(|e| NetError::Daemon(e.to_string()))?;
                    }
                }
                Ok(0)
            })
        }
    }

    let gate_handler = Arc::new(OutboxGateHandler {
        allow_flush: AtomicBool::new(false),
    });
    let outbox_cfg = DaemonConfig {
        socket_path: sock.clone(),
        idle_extract_window: Duration::from_millis(30),
        idle_shutdown_timeout: None,
        max_frame_bytes: 64 * 1024,
        web_inspector: None,
    }
    .with_idle_shutdown_timeout(Some(Duration::from_millis(90)));

    let mut outbox_daemon = DaemonServer::spawn(
        store_outbox,
        outbox_cfg.clone(),
        Arc::clone(&gate_handler) as Arc<dyn DaemonHandler>,
    )
    .await?;
    // Even after > 2x `idle_shutdown_timeout`, daemon MUST NOT exit while `pending_outbox > 0`!
    tokio::time::sleep(Duration::from_millis(220)).await;
    assert!(!*outbox_daemon.subscribe_shutdown().borrow());
    assert_eq!(
        DaemonClient::new(&sock)?.ping().await?.0.as_str(),
        "lock-idle-lib"
    );

    // Flip gate online -> periodic sync flushes outbox (`pending_outbox == 0`) -> daemon idle-exits!
    gate_handler.allow_flush.store(true, Ordering::SeqCst);
    DaemonClient::new(&sock)?.trigger_sync().await?;
    tokio::time::timeout(Duration::from_secs(3), outbox_daemon.wait()).await?;
    outbox_daemon.shutdown().await?;

    // Immediately spawn a successor daemon on the exact same `sock` and verify its socket remains live.
    let successor = DaemonServer::spawn_default(
        Store::open(lib_dir.path())?,
        DaemonConfig {
            socket_path: sock.clone(),
            idle_extract_window: Duration::from_secs(60),
            idle_shutdown_timeout: None,
            max_frame_bytes: 64 * 1024,
            web_inspector: None,
        },
    )
    .await?;
    assert!(sock.exists());
    assert_eq!(
        DaemonClient::new(&sock)?.ping().await?.0.as_str(),
        "lock-idle-lib"
    );
    successor.shutdown().await?;
    assert!(!sock.exists());

    // R10: If a legacy lockless Unix listener is actively accepting on `sock` (no `.lock` held),
    // a failed `DaemonServer::spawn` MUST NOT unlink the live legacy socket on drop!
    let legacy_listener = std::os::unix::net::UnixListener::bind(&sock)?;
    let legacy_ino = std::os::unix::fs::MetadataExt::ino(&std::fs::symlink_metadata(&sock)?);
    assert!(DaemonServer::spawn_default(
        Store::open(lib_dir.path())?,
        DaemonConfig {
            socket_path: sock.clone(),
            idle_extract_window: Duration::from_secs(60),
            idle_shutdown_timeout: None,
            max_frame_bytes: 64 * 1024,
            web_inspector: None,
        },
    )
    .await
    .is_err());
    assert!(sock.exists());
    assert_eq!(
        std::os::unix::fs::MetadataExt::ino(&std::fs::symlink_metadata(&sock)?),
        legacy_ino
    );
    drop(legacy_listener);
    let _ = std::fs::remove_file(&sock);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_ingest_jsonl_handles_multimegabyte_streams_and_wakes_background_queue() -> Result {
    if sandbox_blocks_raw_uds_connect()? {
        return Ok(());
    }

    struct LargeStreamIngestHandler {
        drain_woken: Notify,
        drain_count: AtomicUsize,
    }

    impl DaemonHandler for LargeStreamIngestHandler {
        fn ingest_jsonl<'a>(
            &'a self,
            _library_dir: &'a Path,
            request: IngestJsonlRequest,
        ) -> BoxFuture<'a, kp_net::Result<IngestJsonlResult>> {
            Box::pin(async move {
                let items = parse_public_jsonl_str(&request.jsonl)?;
                let mut out = IngestJsonlResult::default();
                for item in items {
                    match item {
                        PublicIngestLine::ContainerRoster(_) => out.rosters_applied += 1,
                        PublicIngestLine::Record(rec) => match rec.op {
                            PublicIngestOp::Upsert => {
                                out.records_upserted += 1;
                                out.jobs_enqueued += 1;
                                out.source_ids
                                    .push(format!("src_{}", rec.source.external_id).parse()?);
                            }
                            PublicIngestOp::Supersede { .. } => {
                                out.records_superseded += 1;
                                out.jobs_enqueued += 1;
                            }
                            PublicIngestOp::Withdraw => out.records_withdrawn += 1,
                        },
                    }
                }
                Ok(out)
            })
        }

        fn drain_queued_jobs<'a>(
            &'a self,
            _library_dir: &'a Path,
            _at: EpochMs,
        ) -> BoxFuture<'a, kp_net::Result<bool>> {
            Box::pin(async move {
                self.drain_count.fetch_add(1, Ordering::SeqCst);
                self.drain_woken.notify_waiters();
                Ok(false)
            })
        }
    }

    let (lib_dir, store) = library("ingest-uds-lib", "alice", LibraryMode::Personal)?;
    let cfg = DaemonConfig::for_library(lib_dir.path())?;
    assert_eq!(cfg.max_frame_bytes, MAX_DAEMON_FRAME_BYTES);
    let handler = Arc::new(LargeStreamIngestHandler {
        drain_woken: Notify::new(),
        drain_count: AtomicUsize::new(0),
    });
    let woken = handler.drain_woken.notified();
    let daemon =
        DaemonServer::spawn(store, cfg, Arc::clone(&handler) as Arc<dyn DaemonHandler>).await?;

    // Stream > 1.5 MiB JSONL payload (exceeding old 1 MiB cap, well within 8 MiB default frame cap).
    let filler = "纪要摘要内容".repeat(460);
    let mut big_lines = Vec::with_capacity(220);
    big_lines.push(
        r#"{"container_id":"scope_chat_arch","label":"架构研讨组","members":["alice"],"all_library_members":false,"synced_at":"2026-11-21T00:00:00Z"}"#
            .to_owned(),
    );
    for idx in 0..200usize {
        big_lines.push(format!(
            r#"{{"source":{{"kind":"chat","uri":"chat://arch/msg_{idx}","external_id":"msg_{idx}","container_id":"scope_chat_arch"}},"speaker":{{"id":"ext_chat:member_{idx}","display_name":"成员{idx}"}},"audience":"container_members","occurred_at":1763683200000,"content":"{filler} #{idx}","op":"upsert"}}"#
        ));
    }
    let payload_jsonl = big_lines.join("\n");
    assert!(payload_jsonl.len() > 1_500_000);

    let client = DaemonClient::for_library(lib_dir.path())?;
    let res = client
        .ingest_jsonl(IngestJsonlRequest {
            jsonl: payload_jsonl,
            actor: None,
            at: Some(AT),
            extract_now: false,
        })
        .await?;
    assert_eq!(res.rosters_applied, 1);
    assert_eq!(res.records_upserted, 200);
    assert_eq!(res.jobs_enqueued, 200);
    assert_eq!(res.source_ids.len(), 200);

    tokio::time::timeout(Duration::from_secs(3), woken).await?;
    assert!(handler.drain_count.load(Ordering::SeqCst) >= 1);

    daemon.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bidirectional_client_peer_uid_and_embedder_runtime_status_over_uds() -> Result {
    struct EmbedderStatusHandler {
        pct: AtomicU64,
    }

    impl DaemonHandler for EmbedderStatusHandler {
        fn embedder_status(&self) -> DaemonEmbedderRuntimeStatus {
            let p = u8::try_from(self.pct.load(Ordering::SeqCst)).unwrap_or(100);
            DaemonEmbedderRuntimeStatus {
                model: "local:bge-small-zh-v1.5".into(),
                state: if p < 100 {
                    "downloading".into()
                } else {
                    "ready".into()
                },
                download_pct: Some(p),
                last_error: None,
            }
        }
    }

    let uid = current_process_uid()?;
    let wrong_uid = uid.wrapping_add(1);
    let (s1, _s2) = std::os::unix::net::UnixStream::pair()?;
    verify_std_stream_peer_uid(&s1, uid)?;
    assert!(matches!(
        verify_std_stream_peer_uid(&s1, wrong_uid),
        Err(NetError::Forbidden)
    ));

    if sandbox_blocks_raw_uds_connect()? {
        return Ok(());
    }

    let (lib_dir, store) = library("uds-uid-embed-lib", "alice", LibraryMode::Personal)?;
    let cfg = DaemonConfig::for_library(lib_dir.path())?;
    let sock = cfg.socket_path.clone();
    let handler = Arc::new(EmbedderStatusHandler {
        pct: AtomicU64::new(65),
    });
    let daemon =
        DaemonServer::spawn(store, cfg, Arc::clone(&handler) as Arc<dyn DaemonHandler>).await?;

    #[cfg(feature = "test-inproc-uds")]
    {
        // Mismatched expected peer UID fails closed with `Forbidden` on both async and sync clients.
        assert!(matches!(
            DaemonClient::new(&sock)?
                .with_expected_peer_uid_for_test(wrong_uid)
                .status()
                .await,
            Err(NetError::Forbidden)
        ));
        assert!(matches!(
            DaemonSyncClient::new(&sock)?
                .with_expected_peer_uid_for_test(wrong_uid)
                .trigger_sync(),
            Err(NetError::Forbidden)
        ));
    }

    // Matching UID succeeds and exposes live embedder download progress -> ready transition.
    let client = DaemonClient::new(&sock)?;
    let st_dl = client.status().await?;
    assert_eq!(st_dl.embedder.model, "local:bge-small-zh-v1.5");
    assert_eq!(st_dl.embedder.state, "downloading");
    assert_eq!(st_dl.embedder.download_pct, Some(65));
    assert_eq!(st_dl.embedder.last_error, None);

    handler.pct.store(100, Ordering::SeqCst);
    let st_ok = client.status().await?;
    assert_eq!(st_ok.embedder.state, "ready");
    assert_eq!(st_ok.embedder.download_pct, Some(100));

    daemon.shutdown().await?;
    Ok(())
}
