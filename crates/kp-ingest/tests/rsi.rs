mod support;

use kp_ingest::dream::{dream, StructuredDreamAttributor};
use kp_model::{
    Attribution, ContrastKind, EpochMs, Fact, FactAccess, FactId, FactKind, FactStatus,
    FeedbackOutcome, MemberRole, RsiDimension, SkillPackage,
};
use kp_store::{Episode, Store};
use serde_json::json;

use support::{library, options, Result};

fn attribution(premises: &[FactId]) -> Result<Attribution> {
    Ok(Attribution {
        skill_slug: "check-api-contract".parse()?,
        trigger: "constructing a versioned API request".into(),
        wrong_move: "Assume request fields are compatible across API versions".into(),
        verified_fix:
            "Compare each request field with the API_CONTRACT_MARKER contract before sending."
                .into(),
        dimension: RsiDimension::Dom,
        cited_premise_ids: premises.to_vec(),
    })
}

fn episode(session: &str, premises: &[FactId]) -> Result<Episode> {
    Ok(Episode {
        signal_kind: ContrastKind::HumanCorrection,
        session_id: session.into(),
        before_trace: json!({"content":"PRIVATE_TRANSCRIPT_MARKER: used the old request shape. api_key=secret-before"}),
        after_trace: json!({"content":"PRIVATE_AFTER_MARKER: corrected request. token=secret-after","attribution":attribution(premises)?}),
        step_ref: "request:3 ghp_step_secret".into(),
        environment: json!({"actor":"alice","premise_ids":premises,"password":"secret-environment"}),
    })
}

fn contract(store: &mut Store) -> Result<Fact> {
    let mut input = kp_derive::base_fact(
        store,
        kp_derive::Memory {
            subject: "versioned-api".into(),
            predicate: "kp.code.api_contract".into(),
            value: json!("API_CONTRACT_MARKER"),
            kind: FactKind::FactSlot,
            logical_key: Some("api-contract".parse()?),
            tags: Default::default(),
            entities: Default::default(),
            valid: kp_model::ValidityWindow {
                valid_from: EpochMs(0),
                valid_until: EpochMs::OPEN_END,
            },
        },
        &options()?,
    )?;
    let opts = options()?;
    store.upsert_governed_source_with_facts(
        kp_store::GovernedSourceInput {
            external_source_key: "file:///api.md".into(),
            uri: "api.md".into(),
            media_type: "text/markdown".into(),
            bytes: b"API_CONTRACT_MARKER".to_vec(),
            chunks: vec![kp_store::ChunkInput {
                heading: "API".into(),
                body: "API_CONTRACT_MARKER".into(),
                anchor: Default::default(),
            }],
            speaker: opts.actor.clone(),
            ingested_by: opts.actor,
            readable_by: opts.audience,
            source_scope_id: None,
            effective_policy: kp_model::ModelRoutePolicy::RawOnly,
            op_kind: kp_store::GovernedUpsertMode::AutoSupersede,
            thread_key: None,
            reply_to_key: None,
            captured_at: EpochMs(10),
        },
        std::slice::from_mut(&mut input),
    )?;
    Ok(input.fact)
}

async fn promote(store: &mut Store, premises: &[FactId]) -> Result<(FactId, FactId)> {
    for session in ["ep-1", "ep-2"] {
        store.record_episodes(&[episode(session, premises)?], EpochMs(20))?;
        dream(
            store,
            &"alice".parse()?,
            EpochMs(30),
            &StructuredDreamAttributor,
        )
        .await?;
    }
    let facts = store.visible_facts(&"alice".parse()?, EpochMs(40))?;
    let bullet = facts
        .iter()
        .find(|f| f.kind == FactKind::PlaybookBullet)
        .ok_or("bullet")?
        .id
        .clone();
    let skill = facts
        .iter()
        .find(|f| f.kind == FactKind::SkillVersion)
        .ok_or("skill")?
        .id
        .clone();
    Ok((bullet, skill))
}

#[tokio::test]
async fn keep_base_redaction_ace_and_independent_promotion() -> Result {
    let (dir, mut store) = library()?;
    let actor = "alice".parse()?;
    let routine = json!({"session_id":"routine","messages":[{"role":"user","content":"What is Rust?"},{"role":"assistant","content":"A language."}]});
    assert_eq!(
        kp_ingest::hooks::run(&mut store, "stop", &routine, &actor, EpochMs(10)).await?["episodes"],
        0
    );
    let mut noise = episode("noise", &[])?;
    noise.after_trace = json!("thanks for the answer");
    store.record_episodes(&[noise], EpochMs(10))?;
    let report = dream(&mut store, &actor, EpochMs(20), &StructuredDreamAttributor).await?;
    assert_eq!(report.kept_base, 1);
    assert_eq!(store.status()?.facts, 0);
    let first = episode("ep-1", &[])?;
    store.record_episodes(std::slice::from_ref(&first), EpochMs(21))?;
    let stored = serde_json::to_string(
        &store
            .pending_episodes(&actor)?
            .into_iter()
            .find(|p| p.episode.session_id == "ep-1")
            .ok_or("ep-1")?
            .episode,
    )?;
    for secret in [
        "secret-before",
        "secret-after",
        "ghp_step_secret",
        "secret-environment",
    ] {
        assert!(!stored.contains(secret));
    }
    assert!(stored.contains("[REDACTED]"));
    let first_report = dream(&mut store, &actor, EpochMs(30), &StructuredDreamAttributor).await?;
    assert_eq!(first_report.bullets_created, 1);
    assert!(first_report.promoted.is_empty());
    assert!(!dir
        .path()
        .join("skills/check-api-contract/SKILL.md")
        .exists());
    assert_eq!(store.record_episodes(&[first], EpochMs(31))?, 0);
    assert_eq!(
        dream(&mut store, &actor, EpochMs(31), &StructuredDreamAttributor)
            .await?
            .processed,
        0
    );
    let mut same_session = episode("ep-1", &[])?;
    same_session.step_ref = "request:4".into();
    store.record_episodes(&[same_session], EpochMs(32))?;
    assert!(
        dream(&mut store, &actor, EpochMs(33), &StructuredDreamAttributor)
            .await?
            .promoted
            .is_empty()
    );
    store.record_episodes(&[episode("ep-2", &[])?], EpochMs(34))?;
    let second = dream(&mut store, &actor, EpochMs(35), &StructuredDreamAttributor).await?;
    assert_eq!(second.bullets_created, 0);
    assert_eq!(second.promoted.len(), 1);
    let facts = store.visible_facts(&actor, EpochMs(40))?;
    assert_eq!(
        facts
            .iter()
            .filter(|f| f.kind == FactKind::PlaybookBullet)
            .count(),
        1
    );
    let bullet = facts
        .iter()
        .find(|f| f.kind == FactKind::PlaybookBullet)
        .ok_or("bullet")?;
    assert_eq!(
        store
            .feedback_counts(&bullet.id, &actor, EpochMs(40))?
            .helped,
        2
    );
    let skill = store
        .active_skills(&actor, EpochMs(40))?
        .pop()
        .ok_or("skill")?;
    let text = std::fs::read_to_string(dir.path().join("skills/check-api-contract/SKILL.md"))?;
    skill.package.validate_markdown(&text, true)?;
    assert!(text.contains(
        "When constructing a versioned API request, load the `check-api-contract` skill."
    ));
    for secret in [
        "PRIVATE_TRANSCRIPT_MARKER",
        "PRIVATE_AFTER_MARKER",
        "secret-before",
        "secret-after",
    ] {
        assert!(!text.contains(secret));
        assert!(!bullet.object.to_string().contains(secret));
    }
    let start_output =
        kp_ingest::hooks::run(&mut store, "session-start", &json!({}), &actor, EpochMs(40)).await?;
    assert_eq!(start_output, json!({}));
    let context = kp_ingest::hooks::run(
        &mut store,
        "user-prompt-submit",
        &json!({"prompt": "constructing a versioned API request check-api-contract"}),
        &actor,
        EpochMs(40),
    )
    .await?;
    assert!(context
        .to_string()
        .contains(&format!("kp-ref:fact_{}", skill.fact_id)));
    assert!(context
        .to_string()
        .contains("load the `check-api-contract` skill"));
    Ok(())
}

#[tokio::test]
async fn hurt_tasks_are_deduplicated_and_do_not_resurrect_on_dream() -> Result {
    for target_bullet in [false, true] {
        let (dir, mut store) = library()?;
        let (bullet, skill) = promote(&mut store, &[]).await?;
        let actor = "alice".parse()?;
        let target = if target_bullet {
            bullet.as_str()
        } else {
            "check-api-contract"
        };
        let one = store.feedback(
            target,
            &actor,
            "task-1",
            FeedbackOutcome::Hurt,
            None,
            EpochMs(50),
        )?;
        assert_eq!(one.counts.hurt, 1);
        assert!(!one.rolled_back);
        let spam = store.feedback(
            target,
            &actor,
            "task-1",
            FeedbackOutcome::Hurt,
            None,
            EpochMs(51),
        )?;
        assert!(!spam.recorded);
        assert_eq!(spam.counts.hurt, 1);
        let two = store.feedback(
            target,
            &actor,
            "task-2",
            FeedbackOutcome::Hurt,
            Some("local"),
            EpochMs(52),
        )?;
        assert!(two.rolled_back);
        assert_eq!(
            store.derivation_view()?.facts[&skill].status,
            FactStatus::Invalidated
        );
        assert_eq!(
            store.derivation_view()?.facts[&bullet].status,
            FactStatus::Invalidated
        );
        let snapshot = store.retrieval_snapshot(&actor, EpochMs(53))?;
        assert!(!snapshot
            .documents()
            .iter()
            .any(|doc| doc.id == format!("fact-{bullet}")));
        assert!(!dir.path().join("skills/check-api-contract").exists());
        assert!(
            dream(&mut store, &actor, EpochMs(53), &StructuredDreamAttributor)
                .await?
                .promoted
                .is_empty()
        );
        assert!(!dir.path().join("skills/check-api-contract").exists());
        for session in ["improved-1", "improved-2"] {
            let mut improved = episode(session, &[])?;
            improved.after_trace["attribution"]["verified_fix"] =
                json!("Check the API version and validate every field against its current schema.");
            store.record_episodes(&[improved], EpochMs(60))?;
        }
        let improved = dream(&mut store, &actor, EpochMs(70), &StructuredDreamAttributor).await?;
        assert_eq!(improved.promoted.len(), 1);
        assert_eq!(
            store.active_skills(&actor, EpochMs(70))?[0].package.version,
            2
        );
        assert!(dir
            .path()
            .join("skills/check-api-contract/SKILL.md")
            .is_file());
    }
    Ok(())
}

#[tokio::test]
async fn premises_withdraw_supersede_expire_and_purge_the_skill_projection() -> Result {
    for operation in ["withdraw", "supersede", "expire", "purge"] {
        let (dir, mut store) = library()?;
        let mut premise = contract(&mut store)?;
        if operation == "expire" {
            store.forget_fact_id(&premise.id, &"alice".parse()?, EpochMs(11))?;
            premise.id = "expiring-contract".parse()?;
            premise.logical_key = "expiring".parse()?;
            if let FactAccess::Base { stamp, .. } = &mut premise.access {
                stamp.valid.valid_until = EpochMs(50);
            }
            store.insert_fact(&premise)?;
        }
        let (bullet, skill) = promote(&mut store, &[premise.id.clone()]).await?;
        match operation {
            "withdraw" => {
                store.forget_fact_id(&premise.id, &"alice".parse()?, EpochMs(50))?;
            }
            "supersede" => {
                let mut replacement = premise.clone();
                replacement.id = "new-contract".parse()?;
                replacement.object = json!("new contract");
                if let FactAccess::Base { stamp, .. } = &mut replacement.access {
                    stamp.valid.valid_from = EpochMs(50);
                    stamp.recorded_at = EpochMs(50);
                }
                store.remember(&[kp_store::FactInput {
                    fact: replacement,
                    entities: Default::default(),
                }])?;
            }
            "expire" => store.expire(EpochMs(50))?,
            "purge" => store.purge(
                &premise.origin_library,
                &premise.logical_key,
                EpochMs(50),
                &[],
            )?,
            _ => return Err("unexpected operation".into()),
        }
        let graph = store.derivation_view()?;
        if operation == "purge" {
            assert!(!graph.facts.contains_key(&bullet) && !graph.facts.contains_key(&skill));
            assert!(!dir.path().join("raw").exists());
            let bytes = std::fs::read(dir.path().join("kp.sqlite"))?;
            assert!(!String::from_utf8_lossy(&bytes).contains("API_CONTRACT_MARKER"));
            assert!(!String::from_utf8_lossy(&bytes).contains("PRIVATE_TRANSCRIPT_MARKER"));
            assert_eq!(
                kp_store::find_sqlite_table_containing_bytes(dir.path(), b"API_CONTRACT_MARKER")?,
                None
            );
            assert_eq!(
                kp_store::find_sqlite_table_containing_bytes(
                    dir.path(),
                    b"PRIVATE_TRANSCRIPT_MARKER"
                )?,
                None
            );
        } else {
            assert_eq!(
                graph.facts[&bullet].status,
                FactStatus::Invalidated,
                "{operation}"
            );
            assert_eq!(
                graph.facts[&skill].status,
                FactStatus::Invalidated,
                "{operation}"
            );
        }
        assert!(
            !dir.path().join("skills/check-api-contract").exists(),
            "{operation}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn private_rsi_is_noninterfering_and_hidden_feedback_is_not_found() -> Result {
    let (_dir, mut store) = library()?;
    let bob: kp_model::ActorId = "bob".parse()?;
    store.add_member(bob.clone(), MemberRole::Member, EpochMs(0))?;
    let before = serde_json::to_vec(&store.change_snapshot(&bob, EpochMs(40))?)?;
    let (bullet, skill) = promote(&mut store, &[]).await?;
    assert_eq!(
        before,
        serde_json::to_vec(&store.change_snapshot(&bob, EpochMs(40))?)?
    );
    assert!(store.pending_episodes(&bob)?.is_empty());
    assert!(store.active_skills(&bob, EpochMs(40))?.is_empty());
    assert!(kp_derive::why::why(&store, &skill, &bob, EpochMs(40))?.is_none());
    for target in [bullet.as_str(), skill.as_str(), "check-api-contract"] {
        assert!(matches!(
            store.feedback(
                target,
                &bob,
                "probe",
                FeedbackOutcome::Hurt,
                None,
                EpochMs(40)
            ),
            Err(kp_store::StoreError::NotFound)
        ));
    }
    store.forget_fact_id(&bullet, &"alice".parse()?, EpochMs(41))?;
    assert_eq!(
        before,
        serde_json::to_vec(&store.change_snapshot(&bob, EpochMs(40))?)?
    );
    Ok(())
}

#[tokio::test]
async fn failed_citation_validation_does_not_consume_episode_or_commit_partial_facts() -> Result {
    let (_dir, mut store) = library()?;
    store.record_episodes(&[episode("bad", &[])?], EpochMs(10))?;
    let pending = store
        .pending_episodes(&"alice".parse()?)?
        .pop()
        .ok_or("pending")?;
    assert!(store
        .apply_attribution(
            &pending,
            &"alice".parse()?,
            Some(&attribution(&["missing".parse()?])?),
            EpochMs(20)
        )
        .is_err());
    assert_eq!(store.status()?.facts, 0);
    assert_eq!(store.pending_episodes(&"alice".parse()?)?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn cross_avatar_requires_a_visible_active_dead_end_and_verified_matching_fix() -> Result {
    let (_dir, mut store) = library()?;
    store.add_member("bob".parse()?, MemberRole::Member, EpochMs(0))?;
    let mut dead = contract(&mut store)?;
    dead.id = "dead-end".parse()?;
    dead.logical_key = "dead-end".parse()?;
    dead.kind = FactKind::ClosedNegative;
    dead.author = "bob".parse()?;
    store.insert_fact(&dead)?;
    let mut fix = dead.clone();
    fix.id = "verified-fix".parse()?;
    fix.logical_key = "verified-fix".parse()?;
    fix.kind = FactKind::Observation;
    fix.author = "alice".parse()?;
    let mut rule = attribution(&[dead.id.clone(), fix.id.clone()])?;
    rule.dimension = RsiDimension::Dom;
    fix.object = json!({"verified":true,"attribution":rule});
    store.insert_fact(&fix)?;
    assert_eq!(
        kp_ingest::signals::cross_avatar(
            &mut store,
            &fix.id,
            &"alice".parse()?,
            "cross-1",
            EpochMs(20)
        )?,
        1
    );
    assert_eq!(
        dream(
            &mut store,
            &"alice".parse()?,
            EpochMs(30),
            &StructuredDreamAttributor
        )
        .await?
        .bullets_created,
        1
    );
    store.forget_fact_id(&dead.id, &"alice".parse()?, EpochMs(40))?;
    assert_eq!(
        kp_ingest::signals::cross_avatar(
            &mut store,
            &fix.id,
            &"alice".parse()?,
            "cross-2",
            EpochMs(41)
        )?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn approval_is_local_version_scoped_and_symlink_paths_fail_closed() -> Result {
    let (dir, mut store) = library()?;
    let (_, skill) = promote(&mut store, &[]).await?;
    let mut package: SkillPackage =
        serde_json::from_value(store.resolve_fact(skill.as_str())?.ok_or("skill")?.object)?;
    assert!(package.exec_approved);
    store.add_member("maintainer".parse()?, MemberRole::Maintainer, EpochMs(0))?;
    assert!(matches!(
        store.approve_skill(&package.name, &"maintainer".parse()?, EpochMs(50)),
        Err(kp_store::StoreError::PermissionDenied)
    ));
    package.name = "another".parse()?;
    let text = package.render(false)?;
    assert!(text.contains("exec_approved: false"));
    #[cfg(unix)]
    {
        let external = tempfile::tempdir()?;
        std::fs::remove_dir_all(dir.path().join("skills/check-api-contract"))?;
        std::os::unix::fs::symlink(
            external.path(),
            dir.path().join("skills/check-api-contract"),
        )?;
        assert!(store.sync_skills_now().is_err());
        assert!(!external.path().join("SKILL.md").exists());
    }
    Ok(())
}

#[tokio::test]
async fn purge_erases_hook_transcripts_and_blocks_episode_replay() -> Result {
    let (dir, mut store) = library()?;
    let premise = contract(&mut store)?;
    let actor = "alice".parse()?;
    let mut sources = Vec::new();
    for session in ["hook-1", "hook-2"] {
        let payload = json!({"session_id":session,"messages":[
            {"role":"assistant","content":"PRIVATE_HOOK_TRANSCRIPT: used the wrong API shape"},
            {"role":"user","content":"No, use the current API contract.","attribution":attribution(std::slice::from_ref(&premise.id))?}
        ]});
        let captured =
            kp_ingest::hooks::run(&mut store, "stop", &payload, &actor, EpochMs(20)).await?;
        assert_eq!(captured["episodes"], 1);
        sources.push(
            captured["source"]["source_id"]
                .as_str()
                .ok_or("source")?
                .to_owned(),
        );
    }
    let replay = store
        .pending_episodes(&actor)?
        .pop()
        .ok_or("episode")?
        .episode;
    assert_eq!(
        dream(&mut store, &actor, EpochMs(30), &StructuredDreamAttributor)
            .await?
            .promoted
            .len(),
        1
    );
    store.purge(
        &premise.origin_library,
        &premise.logical_key,
        EpochMs(40),
        &[],
    )?;
    assert!(matches!(
        store.record_episodes(&[replay], EpochMs(41)),
        Err(kp_store::StoreError::Tombstoned(_))
    ));
    for source in sources {
        assert!(store.raw_source_bytes(&source.parse()?)?.is_none());
    }
    assert!(!dir.path().join("raw").exists());
    let bytes = std::fs::read(dir.path().join("kp.sqlite"))?;
    assert!(!String::from_utf8_lossy(&bytes).contains("PRIVATE_HOOK_TRANSCRIPT"));
    assert_eq!(
        kp_store::find_sqlite_table_containing_bytes(dir.path(), b"PRIVATE_HOOK_TRANSCRIPT")?,
        None
    );
    Ok(())
}

#[tokio::test]
async fn changed_commands_on_the_same_explicit_target_form_a_recovery() -> Result {
    let (_dir, mut store) = library()?;
    let actor = "alice".parse()?;
    let mut payload = json!({"session_id":"build","tool_name":"Bash","tool_use_id":"failed-step","target":"crate:payments","tool_input":{"command":"cargo test -p payments"},"tool_response":{"exit_code":1}});
    assert_eq!(
        kp_ingest::hooks::run(&mut store, "post-tool-use", &payload, &actor, EpochMs(10)).await?
            ["episodes"],
        0
    );
    payload["tool_input"]["command"] = json!("cargo test -p payments --features integration");
    payload["tool_response"]["exit_code"] = json!(0);
    payload["tool_use_id"] = json!("verified-step");
    assert_eq!(
        kp_ingest::hooks::run(&mut store, "post-tool-use", &payload, &actor, EpochMs(20)).await?
            ["episodes"],
        1
    );
    assert_eq!(
        dream(&mut store, &actor, EpochMs(30), &StructuredDreamAttributor)
            .await?
            .bullets_created,
        1
    );
    Ok(())
}

#[test]
fn simultaneous_dream_commits_consume_an_episode_once() -> Result {
    let (dir, mut store) = library()?;
    let actor = "alice".parse()?;
    store.record_episodes(&[episode("concurrent", &[])?], EpochMs(10))?;
    let pending = store.pending_episodes(&actor)?.pop().ok_or("pending")?;
    let second = Store::open(dir.path())?;
    let barrier = std::sync::Barrier::new(2);
    let attribution = attribution(&[])?;
    let outcomes = std::thread::scope(|scope| {
        let handles = [store, second]
            .into_iter()
            .map(|mut connection| {
                let barrier = &barrier;
                let pending = &pending;
                let actor = &actor;
                let attribution = &attribution;
                scope.spawn(move || {
                    barrier.wait();
                    connection.apply_attribution(pending, actor, Some(attribution), EpochMs(20))
                })
            })
            .collect::<Vec<_>>();
        handles.into_iter().map(|h| h.join()).collect::<Vec<_>>()
    });
    let mut processed = 0;
    let mut created = 0;
    for outcome in outcomes {
        let result = outcome.map_err(|_| "worker panicked")??;
        processed += usize::from(result.processed);
        created += usize::from(result.created);
    }
    assert_eq!((processed, created), (1, 1));
    let store = Store::open(dir.path())?;
    let bullet = store
        .visible_facts(&actor, EpochMs(30))?
        .into_iter()
        .find(|f| f.kind == FactKind::PlaybookBullet)
        .ok_or("bullet")?;
    assert_eq!(
        store
            .feedback_counts(&bullet.id, &actor, EpochMs(30))?
            .helped,
        1
    );
    Ok(())
}

#[path = "rsi/neon.rs"]
mod neon;
