use super::*;
use serde_json::Value;

fn publication(fact: Fact) -> kp_net::PublishRequest {
    kp_net::PublishRequest {
        audience_revision: 1,
        skill_audience: None,
        origin_reference: fact.strong_ref(),
        fact,
        entities: BTreeSet::new(),
        idempotency_key: "retry-key".into(),
        concepts: Vec::new(),
        source_scope: None,
    }
}
fn tombstone(origin: &str, key: &str) -> Result<kp_store::Tombstone> {
    Ok(kp_store::Tombstone {
        tombstone_id: kp_store::tombstone_id(origin.as_bytes(), &origin.parse()?, &key.parse()?)?,
        origin_library_id: origin.parse()?,
        purged_at: AT,
        reason: "purged".into(),
    })
}
async fn post_bytes(
    server: &Server,
    actor: &str,
    path: &str,
    body: &impl serde::Serialize,
) -> Result<(u16, Vec<u8>)> {
    let response = reqwest::Client::new()
        .post(format!("{}{path}", server.url))
        .bearer_auth(server.tokens.get(actor).ok_or("missing token")?)
        .json(body)
        .send()
        .await?;
    Ok((response.status().as_u16(), response.bytes().await?.to_vec()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn member_origin_cannot_be_squatted_and_remains_owned_after_purge() -> Result {
    let (dir, q) = library("q", "admin", LibraryMode::Community)?;
    let server = server_with_alice_role(q, dir, MemberRole::Member).await?;
    let alice = client(&server, "alice")?;
    let seed = publication(fact("seed", "alice", FactKind::Event, &["alice"])?);
    let seed_id = alice.post("/v1/publish", &seed).await?["fact_id"].clone();
    let mut squat = fact("k1", "bob", FactKind::Event, &["bob"])?;
    squat.origin_library = "alice-personal".parse()?;
    let denied = post_bytes(&server, "bob", "/v1/publish", &publication(squat.clone())).await?;
    assert_eq!(denied.0, 403);
    squat.id = seed_id.as_str().ok_or("seed id")?.parse()?;
    squat.logical_key = "seed".parse()?;
    assert_eq!(
        post_bytes(&server, "bob", "/v1/publish", &publication(squat.clone())).await?,
        denied
    );
    squat.origin_library = "q".parse()?;
    assert_eq!(
        post_bytes(&server, "bob", "/v1/publish", &publication(squat)).await?,
        denied
    );
    let future = tombstone("alice-personal", "k1")?;
    let forbidden = post_bytes(&server, "bob", "/v1/purge", &future).await?;
    assert_eq!(forbidden.0, 403);
    let own = publication(fact("k1", "alice", FactKind::Event, &["alice"])?);
    let published = alice.post("/v1/publish", &own).await?;
    alice
        .post("/v1/purge", &json!({"fact_id":published["fact_id"]}))
        .await?;
    assert_eq!(
        post_bytes(&server, "bob", "/v1/purge", &future).await?,
        forbidden
    );
    alice.post("/v1/purge", &json!({"fact_id":seed_id})).await?;
    let mut retry = fact("new-key", "bob", FactKind::Event, &["bob"])?;
    retry.origin_library = "alice-personal".parse()?;
    assert_eq!(
        post_bytes(&server, "bob", "/v1/publish", &publication(retry)).await?,
        denied
    );
    assert!(Store::open(server._dir.path())?
        .owns_origin(&"alice-personal".parse()?, &"alice".parse()?)?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_fact_mutations_are_byte_identical_to_missing_targets() -> Result {
    let (dir, mut q) = library("q", "admin", LibraryMode::Community)?;
    q.insert_fact(&fact(
        "alice-private",
        "alice",
        FactKind::Event,
        &["alice", "bob"],
    )?)?;
    q.insert_fact(&fact(
        "bob-private",
        "bob",
        FactKind::Event,
        &["alice", "bob"],
    )?)?;
    let server = server_with_alice_role(q, dir, MemberRole::Member).await?;
    for path in ["/v1/withdraw", "/v1/purge"] {
        let missing = post_bytes(&server, "carol", path, &json!({"fact_id":"nonexistent"})).await?;
        assert_eq!(missing.0, 404);
        assert_eq!(
            serde_json::from_slice::<Value>(&missing.1)?,
            json!({"error":"not found"})
        );
        for id in ["alice-private", "bob-private"] {
            assert_eq!(
                post_bytes(&server, "carol", path, &json!({"fact_id":id})).await?,
                missing
            );
        }
        assert_eq!(
            post_bytes(&server, "bob", path, &json!({"fact_id":"alice-private"}))
                .await?
                .0,
            403
        );
    }
    client(&server, "maintainer")?
        .post("/v1/purge", &json!({"fact_id":"alice-private"}))
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prearrival_tombstone_blocks_delayed_publish_for_verified_owner_and_moderators() -> Result {
    let (dir, q) = library("q", "admin", LibraryMode::Community)?;
    let server = server_with_alice_role(q, dir, MemberRole::Member).await?;
    client(&server, "alice")?
        .post(
            "/v1/publish",
            &publication(fact("register", "alice", FactKind::Event, &["alice"])?),
        )
        .await?;
    for (actor, origin) in [
        ("alice", "alice-personal"),
        ("admin", "admin-origin"),
        ("maintainer", "maintainer-origin"),
    ] {
        let marker = tombstone(origin, "delayed")?;
        assert_eq!(
            post_bytes(&server, "bob", "/v1/purge", &marker).await?.0,
            403
        );
        assert_eq!(
            post_bytes(&server, actor, "/v1/purge", &marker).await?.0,
            200
        );
        assert_eq!(
            post_bytes(&server, actor, "/v1/purge", &marker).await?.0,
            200
        );
        assert!(Store::open(server._dir.path())?
            .tombstone_ids()?
            .contains(&marker.tombstone_id));
        let mut delayed = fact("delayed", actor, FactKind::Event, &[actor])?;
        delayed.origin_library = origin.parse()?;
        assert_eq!(
            post_bytes(&server, actor, "/v1/publish", &publication(delayed))
                .await?
                .0,
            404
        );
        assert_eq!(
            post_bytes(&server, "bob", "/v1/purge", &marker).await?.0,
            403
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bare_key_mutations_select_the_callers_fact_even_for_moderators() -> Result {
    let (dir, mut q) = library("q", "admin", LibraryMode::Community)?;
    for (id, actor) in [
        ("a-other", "alice"),
        ("z-own", "bob"),
        ("zz-admin", "admin"),
    ] {
        let mut f = fact(id, actor, FactKind::Event, &["alice", "bob", "admin"])?;
        f.logical_key = "same-name".parse()?;
        q.insert_fact(&f)?;
    }
    let server = server_with_alice_role(q, dir, MemberRole::Member).await?;
    client(&server, "bob")?
        .post("/v1/withdraw", &json!({"fact_id":"same-name"}))
        .await?;
    let graph = Store::open(server._dir.path())?.derivation_view()?;
    assert_eq!(graph.facts[&"z-own".parse()?].status, FactStatus::Withdrawn);
    assert_eq!(graph.facts[&"a-other".parse()?].status, FactStatus::Active);
    client(&server, "admin")?
        .post("/v1/purge", &json!({"fact_id":"same-name"}))
        .await?;
    let graph = Store::open(server._dir.path())?.derivation_view()?;
    assert!(graph.facts.contains_key(&"a-other".parse()?));
    assert!(!graph.facts.contains_key(&"zz-admin".parse()?));
    client(&server, "admin")?
        .post(
            "/v1/purge",
            &json!({"fact_id":"same-name","origin_library":"alice-personal"}),
        )
        .await?;
    assert!(!Store::open(server._dir.path())?
        .derivation_view()?
        .facts
        .contains_key(&"a-other".parse()?));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn synthesis_is_authored_by_caller_who_can_withdraw_and_purge_it() -> Result {
    let (dir, mut q) = library("q", "admin", LibraryMode::Community)?;
    q.insert_fact(&fact(
        "shared",
        "alice",
        FactKind::Event,
        &["alice", "bob"],
    )?)?;
    let server = server_with_alice_role(q, dir, MemberRole::Member).await?;
    let bob = client(&server, "bob")?;
    let request = json!({"query":"summary","target_audience":["alice","bob"],"at":10});
    let alice_result = client(&server, "alice")?
        .post("/v1/synthesize", &request)
        .await?;
    let bob_result = bob.post("/v1/synthesize", &request).await?;
    let id: FactId = bob_result["facts"][0]
        .as_str()
        .ok_or("no synthesis")?
        .parse()?;
    assert_ne!(alice_result["facts"][0], bob_result["facts"][0]);
    assert_eq!(
        Store::open(server._dir.path())?.derivation_view()?.facts[&id]
            .author
            .as_str(),
        "bob"
    );
    bob.post("/v1/withdraw", &json!({"fact_id":id})).await?;
    assert_eq!(
        Store::open(server._dir.path())?.derivation_view()?.facts[&id].status,
        FactStatus::Withdrawn
    );
    bob.post("/v1/purge", &json!({"fact_id":id})).await?;
    assert!(!Store::open(server._dir.path())?
        .derivation_view()?
        .facts
        .contains_key(&id));
    assert_eq!(
        post_bytes(
            &server,
            "bob",
            "/v1/synthesize",
            &json!({"query":"summary","target_audience":["alice"]})
        )
        .await?
        .0,
        403
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unchanged_cursor_ignores_private_writes_but_changes_for_visible_writes() -> Result {
    let (dir, mut q) = library("q", "admin", LibraryMode::Community)?;
    q.insert_fact(&fact(
        "visible",
        "alice",
        FactKind::Event,
        &["alice", "carol"],
    )?)?;
    let server = server_with_alice_role(q, dir, MemberRole::Member).await?;
    let carol = client(&server, "carol")?;
    let full = carol.get("/v1/changes?at=10").await?;
    let path = format!(
        "/v1/changes?at=10&since={}",
        full["cursor"].as_str().ok_or("cursor")?
    );
    let unchanged = bytes(&server, "carol", &path).await?;
    let body: Value = serde_json::from_slice(&unchanged.1)?;
    assert_eq!(body["unchanged"], true);
    assert_eq!(body["facts"], json!([]));
    assert_eq!(body["justifications"], json!([]));
    assert_eq!(body["tombstones"], json!([]));
    assert_eq!(body["origin_references"], json!({}));
    client(&server, "bob")?
        .post(
            "/v1/publish",
            &publication(fact("private", "bob", FactKind::Event, &["bob"])?),
        )
        .await?;
    assert_eq!(bytes(&server, "carol", &path).await?, unchanged);
    client(&server, "alice")?
        .post(
            "/v1/publish",
            &publication(fact(
                "new-visible",
                "alice",
                FactKind::Event,
                &["alice", "carol"],
            )?),
        )
        .await?;
    let changed = carol.get(&path).await?;
    assert_eq!(changed["unchanged"], false);
    assert_ne!(changed["cursor"], full["cursor"]);
    assert_eq!(changed["facts"].as_array().ok_or("facts")?.len(), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_rpc_frames_are_errors_and_only_valid_notifications_are_silent() -> Result {
    let (dir, q) = library("q", "alice", LibraryMode::Community)?;
    let server = server(q, dir).await?;
    let alice = client(&server, "alice")?;
    for invalid in [
        json!({"jsonrpc":"2.0","id":1}),
        json!({"jsonrpc":"2.0"}),
        json!({"jsonrpc":"2.0","method":42}),
        json!([]),
    ] {
        let result = alice.post("/mcp", &invalid).await?;
        assert_eq!(result["error"]["code"], -32600);
        assert_eq!(result["id"], Value::Null);
    }
    let batch = alice
        .post(
            "/mcp",
            &json!([
                {"jsonrpc":"2.0","method":"notifications/initialized"},
                {"jsonrpc":"2.0","id":1},
                {"jsonrpc":"2.0"},
                {"jsonrpc":"2.0","id":2,"method":"initialize"}
            ]),
        )
        .await?;
    assert_eq!(batch.as_array().ok_or("batch")?.len(), 3);
    assert_eq!(batch[0]["error"]["code"], -32600);
    assert_eq!(batch[1]["error"]["code"], -32600);
    assert!(batch[2].get("result").is_some());
    assert_eq!(
        post_bytes(
            &server,
            "alice",
            "/mcp",
            &json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .await?,
        (202, Vec::new())
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconciler_never_contacts_origins_for_terminal_facts() -> Result {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let app = axum::Router::new().route(
        "/v1/verify",
        axum::routing::post(move || {
            let observed = observed.clone();
            async move {
                observed.fetch_add(1, Ordering::SeqCst);
                axum::Json(json!({"confirmed":true}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let (_dir, mut local) = library("local", "alice", LibraryMode::Personal)?;
    for (id, status) in [
        ("withdrawn", FactStatus::Withdrawn),
        ("invalidated", FactStatus::Invalidated),
        ("superseded", FactStatus::Superseded),
    ] {
        let mut f = fact(id, "alice", FactKind::Event, &["alice"])?;
        f.status = status;
        local.insert_fact(&f)?;
    }
    let origins = BTreeMap::from([(
        "alice-personal".parse()?,
        CommunityClient::new(&url, "token")?,
    )]);
    kp_net::reconcile_remote_origins(&mut local, &origins, AT).await?;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    local.insert_fact(&fact("active", "alice", FactKind::Event, &["alice"])?)?;
    kp_net::reconcile_remote_origins(&mut local, &origins, AT).await?;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    task.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn omitted_mirror_reappears_in_search_after_snapshot_recovery() -> Result {
    let (_dir, mut origin) = library("origin", "alice", LibraryMode::Personal)?;
    let f = fact("remote", "alice", FactKind::Event, &["alice"])?;
    origin.insert_fact(&f)?;
    let full = origin.change_snapshot(&"alice".parse()?, AT)?;
    let (_dir, mut mirror) = library("mirror", "alice", LibraryMode::Personal)?;
    mirror.import_snapshot("endpoint", &full, AT)?;
    let mut empty = full.clone();
    empty.facts.clear();
    mirror.import_snapshot("endpoint", &empty, AT)?;
    assert_eq!(
        mirror.derivation_view()?.facts[&f.id].status,
        FactStatus::UnreachableHidden
    );
    mirror.import_snapshot("endpoint", &full, AT)?;
    let output = kp_retrieve::search(
        &mirror,
        &kp_retrieve::SearchOptions {
            query: "Friday offsite".into(),
            viewer: "alice".parse()?,
            at: AT,
            limit: 10,
            deep: false,
            max_hops: 2,
        },
        &kp_retrieve::judge::OffJudge,
    )
    .await?;
    assert!(serde_json::to_value(output)?["results"]
        .as_array()
        .ok_or("results")?
        .iter()
        .any(|r| r["id"] == "mirror:fact-remote"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_purge_acknowledgment_keeps_tombstone_in_outbox() -> Result {
    let (_remote_dir, mut remote) = library("remote", "alice", LibraryMode::Personal)?;
    let snapshot = remote.change_snapshot(&"alice".parse()?, AT)?;
    let app = axum::Router::new()
        .route(
            "/v1/purge",
            axum::routing::post(|| async { axum::http::StatusCode::NOT_FOUND }),
        )
        .route(
            "/v1/changes",
            axum::routing::get(move || {
                let snapshot = snapshot.clone();
                async move { axum::Json(snapshot) }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let client = CommunityClient::new(&url, "token")?;
    let (_dir, mut local) = library("local", "alice", LibraryMode::Personal)?;
    let marker = tombstone("alice-personal", "delayed")?;
    local.enqueue(
        client.url(),
        "purge",
        "marker",
        &serde_json::to_value(marker)?,
    )?;
    let synced = kp_net::sync(&mut local, &client, AT).await?;
    assert_eq!(synced.delivered, 0);
    assert_eq!(synced.pending, 1);
    assert_eq!(local.pending_outbox(client.url())?.len(), 1);
    task.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publish_deduplication_never_resolves_another_origins_private_logical_key() -> Result {
    let (dir, mut q) = library("q", "admin", LibraryMode::Community)?;
    let own = publication(fact("own", "bob", FactKind::Event, &["bob"])?);
    let mut other = fact("private", "alice", FactKind::Event, &["alice"])?;
    other.logical_key = format!(
        "published-{}",
        ContentHash::digest(serde_json::to_vec(&own.fact.strong_ref())?)
    )
    .parse()?;
    q.insert_fact(&other)?;
    let server = server_with_alice_role(q, dir, MemberRole::Member).await?;
    let bob = client(&server, "bob")?;
    let response = bob.post("/v1/publish", &own).await?;
    assert_eq!(response["fact_id"], other.logical_key.as_str());
    assert_eq!(bob.post("/v1/publish", &own).await?, response);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_cursor_survives_reopen_skips_unchanged_and_resets_on_credentials() -> Result {
    let (dir, mut community) = library("cursor-community", "admin", LibraryMode::Community)?;
    community.insert_fact(&fact("visible", "alice", FactKind::Event, &["alice"])?)?;
    let server = server_with_alice_role(community, dir, MemberRole::Member).await?;
    let alice = client(&server, "alice")?;
    let carol = client(&server, "carol")?;
    let (mirror_dir, mut mirror) = library("mirror", "alice", LibraryMode::Personal)?;
    assert!(!kp_net::sync(&mut mirror, &alice, AT).await?.unchanged);
    let credential = ContentHash::digest(server.tokens.get("alice").ok_or("token")?);
    let cursor = mirror
        .sync_cursor(alice.url(), &credential)?
        .ok_or("cursor")?;
    assert_eq!(
        alice
            .get(&format!("/v1/changes?since={cursor}&at={AT}"))
            .await?["unchanged"],
        true
    );
    drop(mirror);
    let mut mirror = Store::open(mirror_dir.path())?;
    let unchanged = kp_net::sync(&mut mirror, &alice, AT).await?;
    assert!(unchanged.unchanged);
    assert_eq!(unchanged.mirrored, 0);
    assert!(!kp_net::sync(&mut mirror, &carol, AT).await?.unchanged);
    assert!(mirror.visible_facts(&"alice".parse()?, AT)?.is_empty());
    assert!(!kp_net::sync(&mut mirror, &alice, AT).await?.unchanged);
    assert_eq!(mirror.visible_facts(&"alice".parse()?, AT)?.len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn withdrawal_cannot_expand_an_authorized_id_into_another_actors_private_key() -> Result {
    let (dir, mut q) = library("q", "admin", LibraryMode::Community)?;
    let alice = fact("alice-fact", "alice", FactKind::Event, &["alice"])?;
    let mut bob = fact("bob-private", "bob", FactKind::Event, &["bob"])?;
    bob.origin_library = q.library_id().clone();
    bob.logical_key = alice.id.as_str().parse()?;
    q.insert_fact(&alice)?;
    q.insert_fact(&bob)?;
    let server = server_with_alice_role(q, dir, MemberRole::Member).await?;
    let response = client(&server, "alice")?
        .post("/v1/withdraw", &json!({"fact_id":alice.id}))
        .await?;
    assert_eq!(response, json!({"ok":true}));
    let store = Store::open(server._dir.path())?;
    let graph = store.derivation_view()?;
    assert_eq!(graph.facts[&alice.id].status, FactStatus::Withdrawn);
    assert_eq!(graph.facts[&bob.id].status, FactStatus::Active);
    assert!(!store
        .visible_facts(&"alice".parse()?, AT)?
        .iter()
        .any(|f| f.id == bob.id));
    assert!(store
        .visible_facts(&"bob".parse()?, AT)?
        .iter()
        .any(|f| f.id == bob.id));
    assert_eq!(
        post_bytes(&server, "alice", "/v1/withdraw", &json!({"fact_id":bob.id})).await?,
        post_bytes(
            &server,
            "alice",
            "/v1/withdraw",
            &json!({"fact_id":"missing"})
        )
        .await?
    );
    Ok(())
}
