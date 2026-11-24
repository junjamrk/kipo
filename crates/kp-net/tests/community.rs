use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;

use kp_model::*;
use kp_net::{CommunityClient, PublishOptions};
use kp_store::{InitOptions, Store};
use serde_json::json;

#[path = "community/rsi.rs"]
mod rsi;

type Result<T = ()> = std::result::Result<T, Box<dyn Error>>;
const AT: EpochMs = EpochMs(10);

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
fn fact(id: &str, author: &str, kind: FactKind, actors: &[&str]) -> Result<Fact> {
    Ok(Fact {
        id: id.parse()?,
        origin_library: format!("{author}-personal").parse()?,
        logical_key: id.parse()?,
        kind,
        subject: id.into(),
        predicate: "kp.core.schedule".into(),
        object: json!(if kind == FactKind::Event {
            "Friday offsite"
        } else {
            "no_driving_in_rain -> take_hsr"
        }),
        tags: BTreeSet::from(["hangzhou_rain".into(), "outdoor".into()]),
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
                recorded_at: EpochMs(0),
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
struct Server {
    task: tokio::task::JoinHandle<()>,
    url: String,
    tokens: BTreeMap<String, String>,
    _dir: tempfile::TempDir,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn server(store: Store, dir: tempfile::TempDir) -> Result<Server> {
    server_with_alice_role(store, dir, MemberRole::Owner).await
}
async fn server_with_alice_role(
    mut store: Store,
    dir: tempfile::TempDir,
    alice_role: MemberRole,
) -> Result<Server> {
    let mut tokens = BTreeMap::new();
    for (actor, role) in [
        ("alice", alice_role),
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let task = tokio::spawn(async move {
        let result = axum::serve(listener, kp_net::router(store)).await;
        assert!(result.is_ok());
    });
    Ok(Server {
        task,
        url,
        tokens,
        _dir: dir,
    })
}
fn client(server: &Server, actor: &str) -> Result<CommunityClient> {
    Ok(CommunityClient::new(
        &server.url,
        server.tokens.get(actor).ok_or("missing token")?,
    )?)
}
async fn bytes(server: &Server, actor: &str, path: &str) -> Result<(u16, Vec<u8>)> {
    let response = reqwest::Client::new()
        .get(format!("{}{path}", server.url))
        .bearer_auth(server.tokens.get(actor).ok_or("missing token")?)
        .send()
        .await?;
    Ok((response.status().as_u16(), response.bytes().await?.to_vec()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_server_cross_machine_collision_and_byte_identical_noninterference() -> Result {
    let (dir, q) = library("q", "alice", LibraryMode::Community)?;
    let server = server(q, dir).await?;
    let alice = client(&server, "alice")?;
    let bob = client(&server, "bob")?;
    let (_adir, mut a) = library("alice-personal", "alice", LibraryMode::Personal)?;
    let (_bdir, mut b) = library("bob-personal", "bob", LibraryMode::Personal)?;
    a.insert_fact(&fact(
        "friday_offsite",
        "alice",
        FactKind::Event,
        &["alice", "bob", "carol"],
    )?)?;
    b.insert_fact(&fact(
        "bob_rain_commute",
        "bob",
        FactKind::Constraint,
        &["alice", "bob"],
    )?)?;
    kp_net::publish(
        &mut a,
        "friday_offsite",
        &alice,
        &PublishOptions::default(),
        AT,
    )
    .await?;
    let paths = [
        "/v1/search?q=hangzhou_rain&deep=true&at=10&limit=100",
        "/v1/changes?since=0&at=10",
        "/v1/status?at=10",
    ];
    let mut before = Vec::new();
    for path in paths {
        before.push(bytes(&server, "carol", path).await?);
    }
    kp_net::publish(
        &mut b,
        "bob_rain_commute",
        &bob,
        &PublishOptions::default(),
        AT,
    )
    .await?;
    for (index, path) in paths.iter().enumerate() {
        assert_eq!(before[index], bytes(&server, "carol", path).await?);
    }
    let snapshot = alice.get("/v1/changes?at=10").await?;
    let facts = snapshot["facts"].as_array().ok_or("missing facts")?;
    let collision = facts
        .iter()
        .find(|f| f["kind"] == "collision_warning")
        .ok_or("missing collision")?;
    let collision_id = collision["id"].as_str().ok_or("missing id")?;
    for (actor, c) in [("alice", &alice), ("bob", &bob)] {
        let search = c.get(paths[0]).await?;
        assert!(
            search["results"]
                .as_array()
                .ok_or("missing results")?
                .iter()
                .any(|r| r["kind"] == "collision_warning"),
            "{actor}"
        );
        assert_eq!(
            c.get(&format!("/v1/why/{collision_id}?at=10")).await?["fact_id"],
            collision_id
        );
    }
    assert_eq!(
        bytes(&server, "carol", &format!("/v1/why/{collision_id}?at=10")).await?,
        bytes(&server, "carol", "/v1/why/fact_does_not_exist?at=10").await?
    );
    kp_net::sync(&mut a, &alice, AT).await?;
    kp_net::sync(&mut b, &bob, AT).await?;
    assert!(a
        .visible_facts(&"alice".parse()?, AT)?
        .iter()
        .any(|f| f.kind == FactKind::CollisionWarning));
    assert!(b
        .visible_facts(&"bob".parse()?, AT)?
        .iter()
        .any(|f| f.kind == FactKind::CollisionWarning));
    let bob_id = facts
        .iter()
        .find(|f| f["kind"] == "constraint")
        .and_then(|f| f["id"].as_str())
        .ok_or("missing bob id")?;
    bob.post("/v1/withdraw", &json!({"fact_id":bob_id})).await?;
    for (index, path) in paths.iter().enumerate() {
        assert_eq!(before[index], bytes(&server, "carol", path).await?);
    }
    kp_net::sync(&mut a, &alice, AT).await?;
    assert!(!a
        .visible_facts(&"alice".parse()?, AT)?
        .iter()
        .any(|f| f.kind == FactKind::CollisionWarning));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn joint_viewer_time_check_applies_to_http_and_protocols() -> Result {
    let (dir, mut q) = library("q", "alice", LibraryMode::Community)?;
    let mut sat = fact("sat", "alice", FactKind::Event, &["alice", "bob"])?;
    let mut fri = fact("fri", "alice", FactKind::Event, &["alice", "bob", "carol"])?;
    if let FactAccess::Base { stamp, .. } = &mut sat.access {
        stamp.valid = ValidityWindow {
            valid_from: EpochMs(20),
            valid_until: EpochMs(30),
        };
    }
    if let FactAccess::Base { stamp, .. } = &mut fri.access {
        stamp.valid = ValidityWindow {
            valid_from: EpochMs(10),
            valid_until: EpochMs(20),
        };
    }
    let conclusion = derived_fact(
        q.library_id(),
        &"alice".parse()?,
        FactKind::DerivedFact,
        "weekend",
        "schedule",
        json!("rain"),
        EpochMs(0),
    )?;
    q.insert_fact(&sat)?;
    q.insert_fact(&fri)?;
    q.save_derivations(
        std::slice::from_ref(&conclusion),
        &[
            justification(&conclusion, DerivationMode::HardRule, "sat", &[&sat])?,
            justification(&conclusion, DerivationMode::HardRule, "fri", &[&fri])?,
        ],
    )?;
    let server = server(q, dir).await?;
    let carol = client(&server, "carol")?;
    assert_eq!(
        bytes(
            &server,
            "carol",
            &format!("/v1/why/{}?at=25", conclusion.id)
        )
        .await?
        .0,
        404
    );
    assert_eq!(
        carol.get("/v1/search?q=rain&at=25").await?["results"],
        json!([])
    );
    let mcp = carol.post("/mcp",&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"kp_search","arguments":{"q":"rain","at":25}}})).await?;
    assert_eq!(mcp["result"]["structuredContent"]["results"], json!([]));
    let a2a = carol
        .post(
            "/a2a",
            &json!({"task":"search","input":{"q":"rain","at":25}}),
        )
        .await?;
    assert_eq!(a2a["output"]["results"], json!([]));
    let tools = carol
        .post(
            "/mcp",
            &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        )
        .await?;
    assert_eq!(
        tools["result"]["tools"]
            .as_array()
            .ok_or("missing tools")?
            .len(),
        5
    );
    assert!(
        reqwest::get(format!("{}/.well-known/agent.json", server.url))
            .await?
            .status()
            .is_success()
    );
    assert_eq!(
        CommunityClient::new(&server.url, "unknown")?
            .get("/v1/status")
            .await
            .err()
            .map(|e| e.to_string()),
        Some("remote returned HTTP 401".into())
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outbox_retries_once_server_returns_and_replay_is_idempotent() -> Result {
    let (dir, mut q) = library("q", "alice", LibraryMode::Community)?;
    let token = q.add_member("alice".parse()?, MemberRole::Owner, AT)?.token;
    let reserve = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = reserve.local_addr()?;
    drop(reserve);
    let client = CommunityClient::new(&format!("http://{addr}"), &token)?;
    let (_adir, mut a) = library("alice-personal", "alice", LibraryMode::Personal)?;
    a.insert_fact(&fact("rain", "alice", FactKind::Event, &["alice"])?)?;
    assert!(
        kp_net::publish(&mut a, "rain", &client, &PublishOptions::default(), AT)
            .await?
            .queued
    );
    assert_eq!(
        a.pending_outbox(client.url())?
            .first()
            .ok_or("missing outbox")?
            .retry_count,
        1
    );
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, kp_net::router(q)).await;
    });
    let result = kp_net::sync(&mut a, &client, AT).await?;
    assert_eq!(result.delivered, 1);
    assert!(a.pending_outbox(client.url())?.is_empty());
    kp_net::publish(&mut a, "rain", &client, &PublishOptions::default(), AT).await?;
    assert_eq!(client.get("/v1/status?at=10").await?["facts"], 1);
    task.abort();
    drop(dir);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconciler_hides_across_repeated_offline_misses_recovers_and_invalidates_only_when_withdrawn(
) -> Result {
    let (dir, mut origin) = library("alice-personal", "alice", LibraryMode::Community)?;
    let remote = fact("remote", "alice", FactKind::Event, &["alice"])?;
    origin.insert_fact(&remote)?;
    let server = server(origin, dir).await?;
    let addr = server.url.strip_prefix("http://").ok_or("addr")?.to_owned();
    let (_localdir, mut local) = library("local", "alice", LibraryMode::Personal)?;
    local.insert_fact(&remote)?;
    let origins = BTreeMap::from([("alice-personal".parse()?, client(&server, "alice")?)]);
    local.reconcile_origin_fact(&remote.id, None, AT)?;
    assert_eq!(
        local.derivation_view()?.facts[&remote.id].status,
        FactStatus::UnreachableHidden
    );
    kp_net::reconcile_remote_origins(&mut local, &origins, AT).await?;
    assert_eq!(
        local.derivation_view()?.facts[&remote.id].status,
        FactStatus::Active
    );
    // 401/403 must only hide (UnreachableHidden), never invalidate.
    let bad_auth = BTreeMap::from([(
        "alice-personal".parse()?,
        kp_net::CommunityClient::new(&server.url, "kp_expired_token")?,
    )]);
    kp_net::reconcile_remote_origins(&mut local, &bad_auth, AT).await?;
    assert_eq!(
        local.derivation_view()?.facts[&remote.id].status,
        FactStatus::UnreachableHidden
    );
    kp_net::reconcile_remote_origins(&mut local, &origins, AT).await?;
    assert_eq!(
        local.derivation_view()?.facts[&remote.id].status,
        FactStatus::Active
    );

    server.task.abort();
    for expected_misses in 1..=10 {
        kp_net::reconcile_remote_origins(&mut local, &origins, AT).await?;
        let hidden = &local.derivation_view()?.facts[&remote.id];
        assert_eq!(hidden.status, FactStatus::UnreachableHidden);
        assert_eq!(hidden.miss_count, expected_misses);
    }

    let reopened = Store::open(server._dir.path())?;
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let restart_task = tokio::spawn(async move {
        let _ = axum::serve(listener, kp_net::router(reopened)).await;
    });
    kp_net::reconcile_remote_origins(&mut local, &origins, AT).await?;
    let recovered = &local.derivation_view()?.facts[&remote.id];
    assert_eq!(recovered.status, FactStatus::Active);
    assert_eq!(recovered.miss_count, 0);
    client(&server, "alice")?
        .post("/v1/withdraw", &json!({"fact_id":remote.id}))
        .await?;
    kp_net::reconcile_remote_origins(&mut local, &origins, AT).await?;
    assert_eq!(
        local.derivation_view()?.facts[&remote.id].status,
        FactStatus::Invalidated
    );
    restart_task.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_rejects_forged_author_and_cross_member_deletion() -> Result {
    let (dir, q) = library("q", "alice", LibraryMode::Community)?;
    let server = server(q, dir).await?;
    let alice = client(&server, "alice")?;
    let bob = client(&server, "bob")?;
    let f = fact("rain", "alice", FactKind::Event, &["alice", "bob"])?;
    let body = kp_net::PublishRequest {
        audience_revision: 1,
        skill_audience: None,
        origin_reference: f.strong_ref(),
        fact: f,
        entities: BTreeSet::new(),
        idempotency_key: "id".into(),
        concepts: Vec::new(),
        source_scope: None,
    };
    assert!(matches!(
        bob.post("/v1/publish", &body).await,
        Err(kp_net::NetError::Http(403))
    ));
    let published = alice.post("/v1/publish", &body).await?;
    assert!(matches!(
        bob.post("/v1/purge", &json!({"fact_id":published["fact_id"]}))
            .await,
        Err(kp_net::NetError::Http(403))
    ));
    assert!(matches!(
        bob.post("/v1/withdraw", &json!({"fact_id":published["fact_id"]}))
            .await,
        Err(kp_net::NetError::Http(403))
    ));
    let mut admin = Store::open(server._dir.path())?;
    admin.revoke_member(&"bob".parse()?, AT)?;
    assert!(matches!(
        bob.get("/v1/status").await,
        Err(kp_net::NetError::Http(401))
    ));
    Ok(())
}

#[test]
fn unknown_principal_is_denied_for_every_action() {
    use kp_net::policy::{authorize, Action, Decision};
    for action in [
        Action::Read,
        Action::Publish,
        Action::Withdraw,
        Action::Synthesize,
        Action::AdoptSkill,
        Action::Purge,
        Action::ManageMembers,
    ] {
        assert_eq!(
            authorize(None, LibraryMode::Community, action, None),
            Decision::Deny
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_withdraw_and_purge_propagate_and_tombstones_prevent_replay() -> Result {
    let (dir, q) = library("q", "alice", LibraryMode::Community)?;
    let server = server(q, dir).await?;
    let alice = client(&server, "alice")?;
    let (_adir, mut a) = library("alice-personal", "alice", LibraryMode::Personal)?;
    let original = fact("rain", "alice", FactKind::Event, &["alice", "bob"])?;
    a.insert_fact(&original)?;
    kp_net::publish(&mut a, "rain", &alice, &PublishOptions::default(), AT).await?;
    let saved = alice.get("/v1/changes?at=10").await?;
    let (_mdir, mut mirror) = library("mirror", "alice", LibraryMode::Personal)?;
    kp_net::sync(&mut mirror, &alice, AT).await?;
    a.forget("rain", &"alice".parse()?, AT)?;
    assert_eq!(a.pending_outbox(alice.url())?.len(), 1);
    kp_net::sync(&mut a, &alice, AT).await?;
    assert_eq!(alice.get("/v1/status?at=10").await?["facts"], 0);
    a.purge(&original.origin_library, &original.logical_key, AT, &[])?;
    kp_net::sync(&mut a, &alice, AT).await?;
    kp_net::sync(&mut mirror, &alice, AT).await?;
    assert!(mirror.is_tombstoned(&original.origin_library, &original.logical_key)?);
    mirror.import_snapshot(alice.url(), &serde_json::from_value(saved)?, AT)?;
    assert!(mirror.visible_facts(&"alice".parse()?, AT)?.is_empty());
    assert_eq!(
        client(&server, "carol")?.get("/v1/changes?at=10").await?["tombstones"],
        json!([])
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publishing_redacts_credentials_and_never_transmits_raw_anchors() -> Result {
    let (dir, q) = library("q", "alice", LibraryMode::Community)?;
    let server = server(q, dir).await?;
    let alice = client(&server, "alice")?;
    let (_adir, mut a) = library("alice-personal", "alice", LibraryMode::Personal)?;
    let mut original = fact("rain", "alice", FactKind::Event, &["alice"])?;
    original.object =
        json!({"api_key":"private-value","text":"gho_secrettoken https://user:pass@host"});
    original.source_anchor = Some(AnchorSpec {
        symbol: Some("private-transcript".into()),
        ..AnchorSpec::default()
    });
    a.insert_fact(&original)?;
    kp_net::publish(&mut a, "rain", &alice, &PublishOptions::default(), AT).await?;
    let output = alice.get("/v1/changes?at=10").await?.to_string();
    assert!(!output.contains("private-value"));
    assert!(!output.contains("secrettoken"));
    assert!(!output.contains("user:pass"));
    assert!(!output.contains("private-transcript"));
    assert!(output.contains("[REDACTED]"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_notifications_batches_and_origin_validation_follow_http_transport() -> Result {
    let (dir, q) = library("q", "alice", LibraryMode::Community)?;
    let server = server(q, dir).await?;
    let http = reqwest::Client::new();
    let response = http
        .post(format!("{}/mcp", server.url))
        .bearer_auth(&server.tokens["alice"])
        .json(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        .send()
        .await?;
    assert_eq!(response.status().as_u16(), 202);
    assert!(response.bytes().await?.is_empty());
    let batch = client(&server, "alice")?
        .post(
            "/mcp",
            &json!([
                {"jsonrpc":"2.0","id":1,"method":"initialize"},
                {"jsonrpc":"2.0","id":2,"method":"tools/list"}
            ]),
        )
        .await?;
    assert_eq!(batch.as_array().ok_or("missing batch")?.len(), 2);
    let rejected = http
        .post(format!("{}/mcp", server.url))
        .bearer_auth(&server.tokens["alice"])
        .header("Origin", "https://foreign.invalid")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
        .send()
        .await?;
    assert_eq!(rejected.status().as_u16(), 403);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mapped_claim_retains_original_strong_reference_for_origin_verification() -> Result {
    let (odir, mut original) = library("alice-personal", "alice", LibraryMode::Community)?;
    let claim = fact("claim", "alice", FactKind::Event, &["alice"])?;
    original.insert_fact(&claim)?;
    let origin_server = server(original, odir).await?;
    let (qdir, q) = library("q", "alice", LibraryMode::Community)?;
    let community = server(q, qdir).await?;
    let (_pdir, mut personal) = library("personal", "alice", LibraryMode::Personal)?;
    personal.insert_fact(&claim)?;
    let c = client(&community, "alice")?;
    kp_net::publish(
        &mut personal,
        "claim",
        &c,
        &PublishOptions {
            audience: None,
            map_predicate: Some("kp.community.schedule".into()),
        },
        AT,
    )
    .await?;
    let (_mdir, mut mirror) = library("mirror", "alice", LibraryMode::Personal)?;
    kp_net::sync(&mut mirror, &c, AT).await?;
    kp_net::reconcile_remote_origins(
        &mut mirror,
        &BTreeMap::from([("alice-personal".parse()?, client(&origin_server, "alice")?)]),
        AT,
    )
    .await?;
    let visible = mirror.visible_facts(&"alice".parse()?, AT)?;
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].predicate, "kp.community.schedule");
    Ok(())
}

#[path = "community/regressions.rs"]
mod regressions;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_returns_authenticated_actor_and_ordered_audience_revisions_widen_shrink_and_reject_stale(
) -> Result {
    let (dir, mut q) = library("q", "admin", LibraryMode::Community)?;
    let scope_abc = SourceScope {
        scope_id: "scope_study_room".parse()?,
        provider: "container".into(),
        external_container_id: "room_abc".into(),
        label: "Study Room".into(),
        current_audience: AudienceSpec::Explicit(
            ["alice".parse()?, "bob".parse()?, "carol".parse()?].into(),
        ),
        synced_at: AT,
    };
    q.upsert_source_scope(&scope_abc)?;
    let dave_token = q.add_member("dave".parse()?, MemberRole::Member, AT)?.token;
    let server = server(q, dir).await?;
    let alice = client(&server, "alice")?;
    let bob = client(&server, "bob")?;
    let carol = client(&server, "carol")?;
    let dave = CommunityClient::new(&server.url, &dave_token)?;

    // (d) `/v1/status` returns authenticated `"actor"` for every caller.
    assert_eq!(alice.get("/v1/status?at=10").await?["actor"], "alice");
    assert_eq!(bob.get("/v1/status?at=10").await?["actor"], "bob");
    assert_eq!(carol.get("/v1/status?at=10").await?["actor"], "carol");
    assert_eq!(dave.get("/v1/status?at=10").await?["actor"], "dave");

    // (b) Publish initial `{alice, bob}` rule within `scope_study_room` + Bob's colliding event.
    let (_adir, mut a) = library("alice-personal", "alice", LibraryMode::Personal)?;
    a.add_member("bob".parse()?, MemberRole::Member, AT)?;
    a.add_member("carol".parse()?, MemberRole::Member, AT)?;
    a.add_member("dave".parse()?, MemberRole::Member, AT)?;
    a.upsert_source_scope(&scope_abc)?;

    let cpt_group = CommunityConcept {
        id: "cpt_study_group".parse()?,
        kind: ConceptKind::Group,
        canonical_label: "自习组".into(),
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

    let mut mem_bob = fact("mem_bob", "alice", FactKind::Membership, &["alice", "bob"])?;
    mem_bob.predicate = "kp.core.membership".into();
    mem_bob.tags.clear();
    mem_bob.object = serde_json::to_value(MembershipRelation {
        anchor: "alice".parse()?,
        member: "bob".parse()?,
        group_concept_id: "cpt_study_group".parse()?,
    })?;
    a.insert_fact(&mem_bob)?;

    let mut rule = fact("rain_rule", "alice", FactKind::Constraint, &["alice"])?;
    rule.source_scope_id = Some("scope_study_room".parse()?);
    rule.governed_scope = Some(GovernedScope::RelationGroup {
        anchor: "alice".parse()?,
        group_concept_id: "cpt_study_group".parse()?,
    });
    rule.object = json!({
        "claim_text": "自习组成员在自习时段保持静音",
        "condition_concept_id": "cpt_study_cond",
        "action_concept_id": "cpt_study_act"
    });
    a.insert_fact(&rule)?;

    let (_bdir, mut b) = library("bob-personal", "bob", LibraryMode::Personal)?;
    b.exchange_upsert_concept(&cpt_cond, true)?;
    let mut bob_evt = fact(
        "rain_evt",
        "bob",
        FactKind::Event,
        &["alice", "bob", "carol"],
    )?;
    bob_evt.object = json!({
        "claim_text": "晚间自习时段进行小组语音讨论",
        "condition_concept_id": "cpt_study_cond"
    });
    b.insert_fact(&bob_evt)?;
    kp_net::publish(&mut b, "rain_evt", &bob, &PublishOptions::default(), AT).await?;

    // `auto_publish_and_sync` reconciles `RelationGroup` -> `{alice, bob}` and publishes rev 1!
    kp_net::auto_publish_and_sync(&mut a, &alice, AT).await?;
    kp_net::sync(&mut b, &bob, AT).await?;
    assert!(b
        .visible_facts(&"bob".parse()?, AT)?
        .iter()
        .any(|f| f.kind == FactKind::CollisionWarning));
    assert_eq!(carol.get("/v1/status?at=10").await?["facts"], 1); // only `rain_evt`

    // Add Carol + Dave to `RelationGroup`: `scope_study_room` ceiling caps audience to `{alice, bob, carol}` (excluding `dave`!),
    // and `auto_publish_and_sync` widens `/v1/publish` at revision 2!
    for (mid, member) in [("mem_carol", "carol"), ("mem_dave", "dave")] {
        let mut m = fact(mid, "alice", FactKind::Membership, &["alice"])?;
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
        carol.get("/v1/status?at=10").await?["facts"]
            .as_u64()
            .unwrap_or(0)
            >= 2
    );
    assert_eq!(dave.get("/v1/status?at=10").await?["facts"], 0);

    // Direct widening past `scope_study_room` ceiling (`dave`) is rejected with 403!
    assert!(matches!(
        kp_net::publish(
            &mut a,
            "rain_rule",
            &alice,
            &PublishOptions {
                audience: Some(AudienceSpec::Explicit(
                    ["alice".parse()?, "dave".parse()?].into(),
                )),
                map_predicate: None,
            },
            AT,
        )
        .await,
        Err(kp_net::NetError::Forbidden)
    ));

    // Stale `audience_revision = 1` replay attempting to shrink Carol out is ignored by server!
    let mut stale_scoped = rule.clone();
    stale_scoped.access = FactAccess::Base {
        audience: AudienceSpec::Explicit(["alice".parse()?, "bob".parse()?].into()),
        stamp: BitemporalStamp {
            valid: ValidityWindow {
                valid_from: EpochMs(0),
                valid_until: EpochMs::OPEN_END,
            },
            recorded_at: EpochMs(0),
            expired_at: None,
        },
    };
    let stale_req = kp_net::PublishRequest {
        origin_reference: stale_scoped.strong_ref(),
        fact: stale_scoped,
        entities: BTreeSet::new(),
        idempotency_key: "stale-rev-1".into(),
        audience_revision: 1,
        skill_audience: None,
        concepts: Vec::new(),
        source_scope: None,
    };
    alice.post("/v1/publish", &stale_req).await?;
    assert!(
        carol.get("/v1/status?at=10").await?["facts"]
            .as_u64()
            .unwrap_or(0)
            >= 2
    );

    // Withdraw Bob & Carol's memberships -> `auto_publish_and_sync` shrinks `rain_rule` to `{alice}` (revision 3)
    // -> Bob's `/v1/changes` sync hides BOTH `rain_rule` and derived `CollisionWarning`!
    a.forget("mem_bob", &"alice".parse()?, AT)?;
    a.forget("mem_carol", &"alice".parse()?, AT)?;
    kp_net::auto_publish_and_sync(&mut a, &alice, AT).await?;
    kp_net::sync(&mut b, &bob, AT).await?;
    assert!(!b
        .visible_facts(&"bob".parse()?, AT)?
        .iter()
        .any(|f| f.kind == FactKind::CollisionWarning || f.kind == FactKind::Constraint));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forget_supersede_and_purge_reconcile_pending_and_inflight_publishes() -> Result {
    let (dir, q) = library("q", "admin", LibraryMode::Community)?;
    let server = server_with_alice_role(q, dir, MemberRole::Member).await?;
    let alice = client(&server, "alice")?;
    let bob = client(&server, "bob")?;

    let (_adir, mut a) = library("alice-personal", "alice", LibraryMode::Personal)?;

    // 1. Offline publish + local `forget` before flush -> cancels `'publish'` into `'withdraw'` (404 acked, never lands).
    let reserve = std::net::TcpListener::bind("127.0.0.1:0")?;
    let offline_addr = reserve.local_addr()?;
    drop(reserve);
    let offline_alice =
        CommunityClient::new(&format!("http://{offline_addr}"), &server.tokens["alice"])?;
    let f_cancel = fact(
        "pending_forget",
        "alice",
        FactKind::Event,
        &["alice", "bob"],
    )?;
    a.insert_fact(&f_cancel)?;
    assert!(
        kp_net::publish(
            &mut a,
            "pending_forget",
            &offline_alice,
            &PublishOptions::default(),
            AT
        )
        .await?
        .queued
    );
    a.forget("pending_forget", &"alice".parse()?, AT)?;
    let offline_outbox = a.pending_outbox(offline_alice.url())?;
    assert_eq!(offline_outbox.len(), 1);
    assert_eq!(offline_outbox[0].kind, "withdraw");

    // 2. Published claim superseded locally -> queues `'withdraw'`, and delayed `/v1/publish` retry returns 404 (never resurrects!).
    let f_sup_v1 = fact("sup_v1", "alice", FactKind::Event, &["alice", "bob"])?;
    let mut f_sup_v2 = fact("sup_v2", "alice", FactKind::Event, &["alice", "bob"])?;
    f_sup_v2.object = json!("Saturday offsite");
    a.insert_fact(&f_sup_v1)?;
    kp_net::publish(&mut a, "sup_v1", &alice, &PublishOptions::default(), AT).await?;
    assert_eq!(bob.get("/v1/status?at=10").await?["facts"], 1);
    a.supersede(&f_sup_v1.id, &f_sup_v2)?;
    assert_eq!(a.pending_outbox(alice.url())?[0].kind, "withdraw");
    kp_net::sync(&mut a, &alice, AT).await?;
    assert_eq!(bob.get("/v1/status?at=10").await?["facts"], 0);

    let delayed_retry = kp_net::PublishRequest {
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
        Err(kp_net::NetError::Http(404))
    ));
    assert_eq!(bob.get("/v1/status?at=10").await?["facts"], 0);

    // 3. Simulated in-flight publish where local `forget` vs `purge` already happened before `sync` 2-pass flush:
    //    Pass 0 lands `/v1/publish` -> `finalize_inflight_publish` arms `'withdraw'` (or `'purge'` tombstone) -> Pass 1 delivers it!
    let f_inflight_w = fact("inflight_w", "alice", FactKind::Event, &["alice", "bob"])?;
    a.insert_fact(&f_inflight_w)?;
    a.forget("inflight_w", &"alice".parse()?, AT)?;
    for e in a.pending_outbox(alice.url())? {
        a.outbox_result(e.id, Some(AT))?;
    }
    let req_w = kp_net::PublishRequest {
        origin_reference: f_inflight_w.strong_ref(),
        fact: f_inflight_w.clone(),
        entities: BTreeSet::new(),
        idempotency_key: "inflight-w-key".into(),
        audience_revision: 1,
        skill_audience: None,
        concepts: Vec::new(),
        source_scope: None,
    };
    a.enqueue(
        alice.url(),
        "publish",
        "inflight-w-key",
        &serde_json::to_value(&req_w)?,
    )?;
    let sync_w = kp_net::sync(&mut a, &alice, AT).await?;
    assert_eq!(sync_w.delivered, 1); // Pre-send check drops stale publish and delivers only compensating withdraw!
    assert!(a.pending_outbox(alice.url())?.is_empty());
    assert_eq!(bob.get("/v1/status?at=10").await?["facts"], 0);

    let f_inflight_p = fact("inflight_p", "alice", FactKind::Event, &["alice", "bob"])?;
    a.insert_fact(&f_inflight_p)?;
    let req_p = kp_net::PublishRequest {
        origin_reference: f_inflight_p.strong_ref(),
        fact: f_inflight_p.clone(),
        entities: BTreeSet::new(),
        idempotency_key: "inflight-p-key".into(),
        audience_revision: 1,
        skill_audience: None,
        concepts: Vec::new(),
        source_scope: None,
    };
    a.purge(
        &f_inflight_p.origin_library,
        &f_inflight_p.logical_key,
        AT,
        &[],
    )?;
    for e in a.pending_outbox(alice.url())? {
        a.outbox_result(e.id, Some(AT))?;
    }
    a.enqueue(
        alice.url(),
        "publish",
        "inflight-p-key",
        &serde_json::to_value(&req_p)?,
    )?;
    let sync_p = kp_net::sync(&mut a, &alice, AT).await?;
    assert_eq!(sync_p.delivered, 1); // Pre-send check drops stale publish and delivers only compensating purge tombstone!
    assert!(a.pending_outbox(alice.url())?.is_empty());
    assert_eq!(bob.get("/v1/status?at=10").await?["facts"], 0);
    assert!(Store::open(server._dir.path())?
        .is_tombstoned(&f_inflight_p.origin_library, &f_inflight_p.logical_key)?);

    // 4. Offline queued `{alice, bob}` publish shrunk to `SelfOnly` (`{alice}`) before first receipt is cancelled pre-send!
    let f_shrink = fact(
        "shrink_pre_receipt",
        "alice",
        FactKind::Event,
        &["alice", "bob"],
    )?;
    a.insert_fact(&f_shrink)?;
    let reserve = std::net::TcpListener::bind("127.0.0.1:0")?;
    let offline_addr = reserve.local_addr()?;
    drop(reserve);
    let (off_dir, mut off_q) = library("q-offline", "admin", LibraryMode::Community)?;
    let off_alice_tok = off_q
        .add_member("alice".parse()?, MemberRole::Member, AT)?
        .token;
    let off_bob_tok = off_q
        .add_member("bob".parse()?, MemberRole::Member, AT)?
        .token;
    let off_alice = CommunityClient::new(&format!("http://{offline_addr}"), &off_alice_tok)?;
    let off_bob = CommunityClient::new(&format!("http://{offline_addr}"), &off_bob_tok)?;
    assert!(
        kp_net::publish(
            &mut a,
            "shrink_pre_receipt",
            &off_alice,
            &PublishOptions::default(),
            AT
        )
        .await?
        .queued
    );
    a.narrow_audience(
        &f_shrink.id,
        &AudienceSpec::Explicit(BTreeSet::from(["alice".parse()?])),
    )?;
    let off_listener = tokio::net::TcpListener::bind(offline_addr).await?;
    let off_task = tokio::spawn(async move {
        let _ = axum::serve(off_listener, kp_net::router(off_q)).await;
    });
    let off_sync = kp_net::sync(&mut a, &off_alice, AT).await?;
    assert_eq!(off_sync.delivered, 0);
    assert!(a.pending_outbox(off_alice.url())?.is_empty());
    assert_eq!(off_bob.get("/v1/status?at=10").await?["facts"], 0);

    // 5. Same-revision conflicting audience is rejected with HTTP 409 Conflict!
    let f_conf = fact("rev_conflict", "alice", FactKind::Event, &["alice", "bob"])?;
    a.insert_fact(&f_conf)?;
    kp_net::publish(
        &mut a,
        "rev_conflict",
        &off_alice,
        &PublishOptions::default(),
        AT,
    )
    .await?;
    let mut wider = f_conf.clone();
    wider.access = FactAccess::Base {
        audience: AudienceSpec::Explicit(BTreeSet::from(["alice".parse()?, "bob".parse()?])),
        stamp: kp_model::BitemporalStamp {
            valid: kp_model::ValidityWindow {
                valid_from: EpochMs(0),
                valid_until: EpochMs::OPEN_END,
            },
            recorded_at: AT,
            expired_at: None,
        },
    };
    // Narrow to `{alice}` at revision 2, then attempt `{alice, bob}` at the SAME revision 2 -> 409!
    let mut narrow_v2 = wider.clone();
    narrow_v2.access = FactAccess::Base {
        audience: AudienceSpec::Explicit(BTreeSet::from(["alice".parse()?])),
        stamp: kp_model::BitemporalStamp {
            valid: kp_model::ValidityWindow {
                valid_from: EpochMs(0),
                valid_until: EpochMs::OPEN_END,
            },
            recorded_at: AT,
            expired_at: None,
        },
    };
    off_alice
        .post(
            "/v1/publish",
            &kp_net::PublishRequest {
                origin_reference: f_conf.strong_ref(),
                fact: narrow_v2,
                entities: BTreeSet::new(),
                idempotency_key: "rev2-narrow".into(),
                audience_revision: 2,
                skill_audience: None,
                concepts: Vec::new(),
                source_scope: None,
            },
        )
        .await?;
    assert_eq!(off_bob.get("/v1/status?at=10").await?["facts"], 0);
    let conflict_err = off_alice
        .post(
            "/v1/publish",
            &kp_net::PublishRequest {
                origin_reference: f_conf.strong_ref(),
                fact: wider,
                entities: BTreeSet::new(),
                idempotency_key: "rev2-wide-conflict".into(),
                audience_revision: 2,
                skill_audience: None,
                concepts: Vec::new(),
                source_scope: None,
            },
        )
        .await;
    assert!(matches!(conflict_err, Err(kp_net::NetError::Http(409))));
    assert_eq!(off_bob.get("/v1/status?at=10").await?["facts"], 0);

    // 6. Early `/v1/withdraw` barrier blocks delayed first `/v1/publish` of that exact `StrongRef`
    //    while allowing re-learned v2 on the same `logical_key`, and cross-actor token in `auto_publish_and_sync` fails!
    let early_v1 = fact(
        "early_withdraw_key",
        "alice",
        FactKind::Event,
        &["alice", "bob"],
    )?;
    let early_sref = early_v1.strong_ref();
    // Bob cannot pre-withdraw Alice's owned origin!
    let bob_spoof = off_bob
        .post(
            "/v1/withdraw",
            &json!({
                "fact_id": "published-nonexistent",
                "origin_library": early_sref.origin_library,
                "logical_key": early_sref.logical_key,
                "content_sha256": early_sref.content_sha256,
            }),
        )
        .await;
    assert!(matches!(bob_spoof, Err(kp_net::NetError::Http(403))));
    // Alice's early withdraw records version barrier (200 OK):
    off_alice
        .post(
            "/v1/withdraw",
            &json!({
                "fact_id": "published-nonexistent",
                "origin_library": early_sref.origin_library,
                "logical_key": early_sref.logical_key,
                "content_sha256": early_sref.content_sha256,
            }),
        )
        .await?;
    // Delayed publish of v1 is rejected with 404!
    let delayed_res = off_alice
        .post(
            "/v1/publish",
            &kp_net::PublishRequest {
                origin_reference: early_sref.clone(),
                fact: early_v1,
                entities: BTreeSet::new(),
                idempotency_key: "delayed-v1".into(),
                audience_revision: 1,
                skill_audience: None,
                concepts: Vec::new(),
                source_scope: None,
            },
        )
        .await;
    assert!(matches!(delayed_res, Err(kp_net::NetError::Http(404))));
    // Re-learned v2 on the same `logical_key` (different `content_sha256`) publishes cleanly!
    let mut relearned_v2 = fact(
        "early_withdraw_key",
        "alice",
        FactKind::Event,
        &["alice", "bob"],
    )?;
    relearned_v2.object = json!({"relearned": 2});
    off_alice
        .post(
            "/v1/publish",
            &kp_net::PublishRequest {
                origin_reference: relearned_v2.strong_ref(),
                fact: relearned_v2,
                entities: BTreeSet::new(),
                idempotency_key: "relearned-v2".into(),
                audience_revision: 1,
                skill_audience: None,
                concepts: Vec::new(),
                source_scope: None,
            },
        )
        .await?;
    assert_eq!(off_bob.get("/v1/status?at=10").await?["facts"], 1);

    // Cross-actor token (`off_bob` against Alice's personal store `a`) is rejected by `auto_publish_and_sync`!
    assert!(matches!(
        kp_net::auto_publish_and_sync(&mut a, &off_bob, AT).await,
        Err(kp_net::NetError::Forbidden)
    ));

    // R5: Even a MODERATOR (`Maintainer`) token belonging to another actor (`bob`) MUST fail at the very top of
    // `auto_publish_and_sync` BEFORE enqueueing or delivering any local multi-actor facts from Alice's store!
    let (_mod_dir, mut mod_srv) = library("comm-r5", "admin", LibraryMode::Community)?;
    let bob_mod_tok = mod_srv
        .add_member("bob".parse()?, MemberRole::Maintainer, AT)?
        .token;
    let mod_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let mod_addr = mod_listener.local_addr()?;
    let mod_task = tokio::spawn(async move {
        let _ = axum::serve(mod_listener, kp_net::router(mod_srv)).await;
    });
    let bob_mod_client = CommunityClient::new(&format!("http://{mod_addr}"), &bob_mod_tok)?;
    a.insert_fact(&fact(
        "r5_unshared",
        "alice",
        FactKind::Event,
        &["alice", "bob"],
    )?)?;
    assert!(matches!(
        kp_net::auto_publish_and_sync(&mut a, &bob_mod_client, AT).await,
        Err(kp_net::NetError::Forbidden)
    ));
    assert!(a.pending_outbox(bob_mod_client.url())?.is_empty());
    assert_eq!(bob_mod_client.get("/v1/status?at=10").await?["facts"], 0);
    mod_task.abort();

    // R4: Member B (`off_bob`) issuing an early withdraw on an unbound `origin_library` MUST NOT poison
    // publisher A (`off_alice`)'s subsequent initial `/v1/publish`!
    let mut r4_fact = fact(
        "r4_actor_scoped",
        "alice",
        FactKind::Event,
        &["alice", "bob"],
    )?;
    r4_fact.origin_library = "alice-fresh-unbound-lib".parse()?;
    let r4_sref = r4_fact.strong_ref();
    let _ = off_bob
        .post(
            "/v1/withdraw",
            &json!({
                "fact_id": "nonexistent-r4",
                "origin_library": r4_sref.origin_library,
                "logical_key": r4_sref.logical_key,
                "content_sha256": r4_sref.content_sha256,
            }),
        )
        .await;
    off_alice
        .post(
            "/v1/publish",
            &kp_net::PublishRequest {
                origin_reference: r4_sref,
                fact: r4_fact,
                entities: BTreeSet::new(),
                idempotency_key: "r4-alice-pub".into(),
                audience_revision: 1,
                skill_audience: None,
                concepts: Vec::new(),
                source_scope: None,
            },
        )
        .await?;

    // R3: Initial `/v1/publish` arriving at `audience_revision == 3` atomically records `rev 3` in the
    // same transaction, so a delayed out-of-order `audience_revision == 2` widening is ignored!
    let r3_fact = fact("r3_init_rev3", "alice", FactKind::Event, &["alice"])?;
    let mut r3_stale_wide = r3_fact.clone();
    if let FactAccess::Base { audience, .. } = &mut r3_stale_wide.access {
        *audience = AudienceSpec::Explicit(BTreeSet::from(["alice".parse()?, "bob".parse()?]));
    }
    off_alice
        .post(
            "/v1/publish",
            &kp_net::PublishRequest {
                origin_reference: r3_fact.strong_ref(),
                fact: r3_fact,
                entities: BTreeSet::new(),
                idempotency_key: "r3-rev3".into(),
                audience_revision: 3,
                skill_audience: None,
                concepts: Vec::new(),
                source_scope: None,
            },
        )
        .await?;
    off_alice
        .post(
            "/v1/publish",
            &kp_net::PublishRequest {
                origin_reference: r3_stale_wide.strong_ref(),
                fact: r3_stale_wide,
                entities: BTreeSet::new(),
                idempotency_key: "r3-stale-rev2".into(),
                audience_revision: 2,
                skill_audience: None,
                concepts: Vec::new(),
                source_scope: None,
            },
        )
        .await?;
    // Bob only sees `relearned-v2` + `r4_actor_scoped` (2 facts), NOT `r3_init_rev3`!
    assert_eq!(off_bob.get("/v1/status?at=10").await?["facts"], 2);

    // R1 & R2 & R6:
    // 1) R1: First `{alice, bob}` publish dispatched in-flight (`prepare_outbox_publish`), concurrently shrunk
    //    locally to `{alice}` BEFORE receipt is recorded, yet the server accepted the `{alice, bob}` HTTP POST:
    //    crash-recovery `sync` MUST still deliver a higher-revision shrink so Bob loses visibility!
    let r1_reserve = std::net::TcpListener::bind("127.0.0.1:0")?;
    let r1_addr = r1_reserve.local_addr()?;
    drop(r1_reserve);
    let (r1_q_dir, mut r1_q) = library("q-r1-r2", "admin", LibraryMode::Community)?;
    let r1_alice_tok = r1_q
        .add_member("alice".parse()?, MemberRole::Member, AT)?
        .token;
    let r1_bob_tok = r1_q
        .add_member("bob".parse()?, MemberRole::Member, AT)?
        .token;
    let r1_carol_tok = r1_q
        .add_member("carol".parse()?, MemberRole::Member, AT)?
        .token;
    drop(r1_q);
    let r1_alice = CommunityClient::new(&format!("http://{r1_addr}"), &r1_alice_tok)?;
    let r1_bob = CommunityClient::new(&format!("http://{r1_addr}"), &r1_bob_tok)?;
    let r1_carol = CommunityClient::new(&format!("http://{r1_addr}"), &r1_carol_tok)?;

    let (_r1_dir, mut r1_store) = library("alice-personal", "alice", LibraryMode::Personal)?;
    r1_store.register_peer_members(&["alice".parse()?, "bob".parse()?, "carol".parse()?], AT)?;
    let r1_grp: kp_model::ConceptId = "concept_r1_grp".parse()?;
    r1_store.upsert_community_concept(&kp_model::CommunityConcept {
        id: r1_grp.clone(),
        kind: kp_model::ConceptKind::Group,
        canonical_label: "R1值班组".into(),
        domain: "ops".into(),
        polarity: kp_model::ConceptPolarity::Neutral,
        proposed_by: "alice".parse()?,
        created_at: AT,
    })?;
    let mut r1_mem = fact("r1_bob_mem", "alice", FactKind::Membership, &["alice"])?;
    r1_mem.predicate = "kp.core.membership".into();
    r1_mem.tags.clear();
    r1_mem.object = serde_json::to_value(MembershipRelation {
        anchor: "alice".parse()?,
        member: "bob".parse()?,
        group_concept_id: r1_grp.clone(),
    })?;
    let mut r1_rule = fact("r1_rule", "alice", FactKind::Constraint, &["alice", "bob"])?;
    r1_rule.governed_scope = Some(kp_model::GovernedScope::RelationGroup {
        anchor: "alice".parse()?,
        group_concept_id: r1_grp.clone(),
    });
    r1_store.insert_fact(&r1_mem)?;
    r1_store.insert_fact(&r1_rule)?;
    assert!(
        kp_net::publish(
            &mut r1_store,
            "r1_rule",
            &r1_alice,
            &PublishOptions::default(),
            AT,
        )
        .await?
        .queued
    );

    // Queue `r2_scoped_evt` under a `SourceScope` `{alice, bob, carol}` while `r1_addr` is still offline (for R2)!
    let r2_scope_id: kp_model::SourceScopeId = "chat.oc_r2".parse()?;
    r1_store.upsert_source_scope(&kp_model::SourceScope {
        scope_id: r2_scope_id.clone(),
        provider: "chat".into(),
        external_container_id: "oc_r2".into(),
        label: "R2群".into(),
        current_audience: AudienceSpec::Explicit(BTreeSet::from([
            "alice".parse()?,
            "bob".parse()?,
            "carol".parse()?,
        ])),
        synced_at: AT,
    })?;
    let mut r2_evt = fact(
        "r2_scoped_evt",
        "alice",
        FactKind::Event,
        &["alice", "bob", "carol"],
    )?;
    r2_evt.source_scope_id = Some(r2_scope_id.clone());
    r1_store.insert_fact(&r2_evt)?;
    assert!(
        kp_net::publish(
            &mut r1_store,
            "r2_scoped_evt",
            &r1_alice,
            &PublishOptions {
                audience: Some(AudienceSpec::Explicit(BTreeSet::from([
                    "alice".parse()?,
                    "bob".parse()?,
                    "carol".parse()?,
                ]))),
                map_predicate: None,
            },
            AT,
        )
        .await?
        .queued
    );

    let r1_outbox = r1_store.pending_outbox(r1_alice.url())?;
    assert_eq!(r1_outbox.len(), 2);
    let in_flight_payload = r1_store
        .prepare_outbox_publish(r1_outbox[0].id)?
        .ok_or("expected prepared payload")?;

    let r1_listener = tokio::net::TcpListener::bind(r1_addr).await?;
    let r1_srv_store = Store::open(r1_q_dir.path())?;
    let r1_task = tokio::spawn(async move {
        let _ = axum::serve(r1_listener, kp_net::router(r1_srv_store)).await;
    });
    r1_alice.post("/v1/publish", &in_flight_payload).await?;
    assert_eq!(r1_bob.get("/v1/status?at=10").await?["facts"], 1);

    r1_store.forget("r1_bob_mem", &"alice".parse()?, AT)?;
    let _ = r1_store.reconcile_relation_group_audiences(|l, r| l == r, AT)?;
    r1_store.upsert_source_scope(&kp_model::SourceScope {
        scope_id: r2_scope_id,
        provider: "chat".into(),
        external_container_id: "oc_r2".into(),
        label: "R2群".into(),
        current_audience: AudienceSpec::Explicit(BTreeSet::from([
            "alice".parse()?,
            "carol".parse()?,
        ])),
        synced_at: EpochMs(AT.0 + 1),
    })?;

    kp_net::sync(&mut r1_store, &r1_alice, EpochMs(AT.0 + 2)).await?;
    assert_eq!(r1_bob.get("/v1/status?at=10").await?["facts"], 0);
    assert_eq!(r1_carol.get("/v1/status?at=10").await?["facts"], 1);
    assert_eq!(r1_alice.get("/v1/status?at=10").await?["facts"], 2);

    // 3) Delivered `Event` (`r2_scoped_evt`, already receipted at `{alice, carol}`) whose `SourceScope`
    //    subsequently shrinks to `{alice}` via `upsert_source_scope` + `auto_publish_and_sync` MUST
    //    push a higher-revision audience shrink so Carol immediately loses remote read access too!
    r1_store.upsert_source_scope(&kp_model::SourceScope {
        scope_id: "chat.oc_r2".parse()?,
        provider: "chat".into(),
        external_container_id: "oc_r2".into(),
        label: "R2群".into(),
        current_audience: AudienceSpec::Explicit(BTreeSet::from(["alice".parse()?])),
        synced_at: EpochMs(AT.0 + 3),
    })?;
    kp_net::auto_publish_and_sync(&mut r1_store, &r1_alice, EpochMs(AT.0 + 4)).await?;
    assert_eq!(r1_carol.get("/v1/status?at=10").await?["facts"], 0);
    assert_eq!(r1_bob.get("/v1/status?at=10").await?["facts"], 0);
    assert_eq!(r1_alice.get("/v1/status?at=10").await?["facts"], 2);

    r1_task.abort();
    drop(r1_q_dir);

    off_task.abort();
    drop(off_dir);
    Ok(())
}
