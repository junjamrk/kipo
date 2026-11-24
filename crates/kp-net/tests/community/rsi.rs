use std::time::{SystemTime, UNIX_EPOCH};

use super::*;

fn wall_now_ms() -> Result<EpochMs> {
    let ms = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?;
    Ok(EpochMs(ms))
}

async fn promoted(store: &mut Store, at: EpochMs) -> Result<FactId> {
    for session in ["independent-1", "independent-2"] {
        store.record_episodes(
            &[kp_store::Episode {
                signal_kind: ContrastKind::HumanCorrection,
                session_id: session.into(),
                before_trace: json!("PRIVATE_SESSION_TEXT use blind repetition"),
                after_trace: json!("No, use a bounded retry and verify the result."),
                step_ref: "tool:3".into(),
                environment: json!({"actor":"alice"}),
            }],
            at,
        )?;
        kp_ingest::dream::dream(
            store,
            &"alice".parse()?,
            at,
            &kp_ingest::dream::StructuredDreamAttributor,
        )
        .await?;
    }
    Ok(store
        .active_skills(&"alice".parse()?, at)?
        .pop()
        .ok_or("skill")?
        .fact_id)
}

#[tokio::test]
async fn community_skill_quarantine_local_owner_approval_and_remote_hurt_retraction() -> Result {
    let (_alice_dir, mut alice) = library("alice-personal", "alice", LibraryMode::Personal)?;
    let (bob_dir, mut bob) = library("bob-personal", "bob", LibraryMode::Personal)?;
    let (q_dir, q) = library("q", "admin", LibraryMode::Community)?;
    let server = server_with_alice_role(q, q_dir, MemberRole::Member).await?;
    let alice_client = client(&server, "alice")?;
    let bob_client = client(&server, "bob")?;
    let t0 = wall_now_ms()?;
    let skill = promoted(&mut alice, t0).await?;
    assert!(
        alice.active_skills(&"alice".parse()?, t0)?[0]
            .package
            .exec_approved
    );
    let shared = PublishOptions {
        audience: Some(AudienceSpec::Explicit(
            ["alice".parse()?, "bob".parse()?].into(),
        )),
        map_predicate: None,
    };
    assert!(
        !kp_net::publish(&mut alice, skill.as_str(), &alice_client, &shared, t0)
            .await?
            .queued
    );
    let at = EpochMs(wall_now_ms()?.0 + 1_000);
    kp_net::sync(&mut bob, &bob_client, at).await?;
    let imported = bob
        .active_skills(&"bob".parse()?, at)?
        .pop()
        .ok_or("imported skill")?;
    assert!(!imported.package.exec_approved);
    assert_eq!(imported.notice, Some(READ_ONLY_SKILL));
    assert_search_approval(&bob, &imported.fact_id, false, at).await?;
    let path = bob_dir
        .path()
        .join(format!("skills/{}/SKILL.md", imported.package.name));
    let text = std::fs::read_to_string(&path)?;
    assert!(text.contains("exec_approved: false"));
    assert!(!text.contains("PRIVATE_SESSION_TEXT"));
    let session_start = kp_ingest::hooks::run(
        &mut bob,
        "session-start",
        &json!({"prompt": &imported.package.trigger}),
        &"bob".parse()?,
        at,
    )
    .await?;
    assert_eq!(session_start, json!({}));

    let prompt_submit = kp_ingest::hooks::run(
        &mut bob,
        "user-prompt-submit",
        &json!({"prompt": format!("{} {}", imported.package.name, imported.package.trigger)}),
        &"bob".parse()?,
        at,
    )
    .await?;
    assert!(prompt_submit.to_string().contains(READ_ONLY_SKILL));
    assert!(!prompt_submit.to_string().contains("exec_approved\\\":true"));
    let snapshot = bob.derivation_view()?;
    assert!(snapshot
        .justifications
        .values()
        .any(|p| p.conclusion == imported.fact_id && p.mode == DerivationMode::RsiAttribution));
    assert!(!serde_json::to_string(&snapshot.facts)?.contains("PRIVATE_SESSION_TEXT"));
    for (name, role) in [
        ("member", MemberRole::Member),
        ("maintainer", MemberRole::Maintainer),
    ] {
        bob.add_member(name.parse()?, role, t0)?;
        assert!(matches!(
            bob.approve_skill(&imported.package.name, &name.parse()?, at),
            Err(kp_store::StoreError::PermissionDenied)
        ));
    }
    bob.approve_skill(&imported.package.name, &"bob".parse()?, at)?;
    assert!(
        bob.active_skills(&"bob".parse()?, at)?[0]
            .package
            .exec_approved
    );
    assert!(std::fs::read_to_string(&path)?.contains("exec_approved: true"));
    assert_search_approval(&bob, &imported.fact_id, true, at).await?;
    // A repeated remote snapshot preserves this version's local approval without granting any new one.
    kp_net::sync(&mut bob, &bob_client, at).await?;
    assert!(
        bob.active_skills(&"bob".parse()?, at)?[0]
            .package
            .exec_approved
    );
    let after_at = EpochMs(wall_now_ms()?.0 + 2_000);
    for task in ["hurt-1", "hurt-2"] {
        alice.feedback(
            skill.as_str(),
            &"alice".parse()?,
            task,
            FeedbackOutcome::Hurt,
            None,
            after_at,
        )?;
    }
    assert!(!alice.pending_outbox(alice_client.url())?.is_empty());
    kp_net::sync(&mut alice, &alice_client, after_at).await?;
    kp_net::sync(&mut bob, &bob_client, after_at).await?;
    assert!(bob.active_skills(&"bob".parse()?, after_at)?.is_empty());
    assert!(!path.exists());
    assert!(matches!(
        kp_net::publish(&mut alice, skill.as_str(), &alice_client, &shared, after_at).await,
        Err(kp_net::NetError::NotFound)
    ));
    Ok(())
}

async fn assert_search_approval(store: &Store, id: &FactId, approved: bool, at: EpochMs) -> Result {
    let output = kp_retrieve::search(
        store,
        &kp_retrieve::SearchOptions {
            query: "apply explicit correction".into(),
            viewer: "bob".parse()?,
            at,
            limit: 100,
            deep: false,
            max_hops: 0,
        },
        &kp_retrieve::judge::OffJudge,
    )
    .await?;
    let result = output
        .results
        .iter()
        .find(|result| result.id == format!("{}:fact-{id}", store.library_id()))
        .ok_or("skill search result")?;
    assert!(
        result
            .passage
            .contains(&format!("\"exec_approved\":{approved}")),
        "{}",
        result.passage
    );
    assert_eq!(result.passage.contains(READ_ONLY_SKILL), !approved);
    assert_eq!(
        store.resolve_fact(id.as_str())?.ok_or("skill")?.object["exec_approved"],
        true
    );
    Ok(())
}

#[tokio::test]
async fn foundational_purge_reaches_subscribers_without_exporting_private_episodes() -> Result {
    let (_alice_dir, mut alice) = library("alice-personal", "alice", LibraryMode::Personal)?;
    let (bob_dir, mut bob) = library("bob-personal", "bob", LibraryMode::Personal)?;
    let (q_dir, q) = library("q", "admin", LibraryMode::Community)?;
    let server = server_with_alice_role(q, q_dir, MemberRole::Member).await?;
    let a = client(&server, "alice")?;
    let b = client(&server, "bob")?;
    let t0 = wall_now_ms()?;
    let skill = promoted(&mut alice, t0).await?;
    let options = PublishOptions {
        audience: Some(AudienceSpec::AllMembers),
        map_predicate: None,
    };
    assert!(
        !kp_net::publish(&mut alice, skill.as_str(), &a, &options, t0)
            .await?
            .queued
    );
    let at = EpochMs(wall_now_ms()?.0 + 1_000);
    kp_net::sync(&mut bob, &b, at).await?;
    assert!(!bob.active_skills(&"bob".parse()?, at)?.is_empty());
    let graph = alice.derivation_view()?;
    let anchor = graph
        .facts
        .values()
        .find(|f| f.predicate == "kp.rsi.episode")
        .ok_or("episode anchor")?;
    let after_at = EpochMs(wall_now_ms()?.0 + 2_000);
    alice.purge(&anchor.origin_library, &anchor.logical_key, after_at, &[])?;
    let queued = alice.pending_outbox(a.url())?;
    assert!(queued.iter().any(|e| e.kind == "purge"));
    assert!(
        !serde_json::to_string(&queued.iter().map(|e| &e.payload).collect::<Vec<_>>())?
            .contains("PRIVATE_SESSION_TEXT")
    );
    kp_net::sync(&mut alice, &a, after_at).await?;
    kp_net::sync(&mut bob, &b, after_at).await?;
    assert!(bob.active_skills(&"bob".parse()?, after_at)?.is_empty());
    assert!(std::fs::read_dir(bob_dir.path().join("skills"))?
        .next()
        .is_none());
    Ok(())
}
