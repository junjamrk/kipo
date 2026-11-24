use super::*;

#[tokio::test]
async fn stale_episode_premises_are_consumed_without_blocking_valid_promotions() -> Result {
    for operation in ["withdraw", "supersede", "expire", "hidden"] {
        let (_dir, mut store) = library()?;
        let actor = "alice".parse()?;
        let mut premise = contract(&mut store)?;
        if operation == "expire" {
            premise.id = "expiring-premise".parse()?;
            premise.logical_key = "expiring-premise".parse()?;
            if let FactAccess::Base { stamp, .. } = &mut premise.access {
                stamp.valid.valid_until = EpochMs(50);
            }
            store.insert_fact(&premise)?;
        }
        store.record_episodes(&[episode("ep-1", &[premise.id.clone()])?], EpochMs(20))?;
        match operation {
            "withdraw" => {
                store.forget_fact_id(&premise.id, &actor, EpochMs(50))?;
            }
            "supersede" => {
                let mut replacement = premise.clone();
                replacement.id = "replacement".parse()?;
                replacement.object = json!("updated API contract");
                if let FactAccess::Base { stamp, .. } = &mut replacement.access {
                    stamp.valid.valid_from = EpochMs(50);
                    stamp.recorded_at = EpochMs(50);
                }
                store.supersede(&premise.id, &replacement)?;
            }
            "hidden" => store.narrow_audience(
                &premise.id,
                &kp_model::AudienceSpec::Explicit(Default::default()),
            )?,
            "expire" => {}
            _ => return Err("unexpected operation".into()),
        }
        store.record_episodes(&[episode("ep-2", &[])?, episode("ep-3", &[])?], EpochMs(51))?;
        let report = dream(&mut store, &actor, EpochMs(60), &StructuredDreamAttributor).await?;
        assert_eq!(
            (report.processed, report.kept_base, report.bullets_created),
            (3, 1, 1),
            "{operation}"
        );
        assert_eq!(report.promoted.len(), 1, "{operation}");
        assert!(store.pending_episodes(&actor)?.is_empty());
        assert_eq!(
            dream(&mut store, &actor, EpochMs(61), &StructuredDreamAttributor)
                .await?
                .processed,
            0
        );
        assert_eq!(store.active_skills(&actor, EpochMs(61))?.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn dream_still_rejects_invented_citations_without_consuming_the_episode() -> Result {
    let (_dir, mut store) = library()?;
    let actor = "alice".parse()?;
    let visible = contract(&mut store)?;
    for cited in [visible.id, "missing".parse()?] {
        let mut bad = episode("bad-citation", &[])?;
        bad.after_trace["attribution"] = serde_json::to_value(attribution(&[cited])?)?;
        store.record_episodes(&[bad], EpochMs(20))?;
    }
    assert!(
        dream(&mut store, &actor, EpochMs(30), &StructuredDreamAttributor)
            .await
            .is_err()
    );
    assert_eq!(store.pending_episodes(&actor)?.len(), 2);
    assert!(store.active_skills(&actor, EpochMs(30))?.is_empty());
    Ok(())
}

#[tokio::test]
async fn historical_mutations_preserve_the_current_skill_projection() -> Result {
    for operation in [
        "dream",
        "feedback",
        "forget",
        "supersede",
        "purge",
        "tombstone",
        "import",
        "approve",
    ] {
        let (dir, mut store) = library()?;
        let actor = "alice".parse()?;
        let unrelated = contract(&mut store)?;
        store.record_episodes(&[episode("early", &[])?], EpochMs(10))?;
        dream(&mut store, &actor, EpochMs(20), &StructuredDreamAttributor).await?;
        let bullet = store
            .visible_facts(&actor, EpochMs(20))?
            .into_iter()
            .find(|f| f.kind == FactKind::PlaybookBullet)
            .ok_or("bullet")?;
        store.record_episodes(&[episode("later", &[])?], EpochMs(90))?;
        assert_eq!(
            dream(&mut store, &actor, EpochMs(100), &StructuredDreamAttributor)
                .await?
                .promoted
                .len(),
            1
        );
        let path = dir.path().join("skills/check-api-contract/SKILL.md");
        let before = std::fs::read(&path)?;
        match operation {
            "dream" => {
                dream(&mut store, &actor, EpochMs(0), &StructuredDreamAttributor).await?;
            }
            "feedback" => {
                store.feedback(
                    bullet.id.as_str(),
                    &actor,
                    "neutral",
                    FeedbackOutcome::Neutral,
                    None,
                    EpochMs(30),
                )?;
            }
            "forget" => {
                store.forget_fact_id(&unrelated.id, &actor, EpochMs(0))?;
            }
            "supersede" => {
                let mut replacement = unrelated.clone();
                replacement.id = "replacement".parse()?;
                replacement.object = json!("new unrelated contract");
                store.supersede(&unrelated.id, &replacement)?;
            }
            "purge" => store.purge(
                &unrelated.origin_library,
                &unrelated.logical_key,
                EpochMs(0),
                &[],
            )?,
            "tombstone" => store.apply_tombstone(&kp_store::Tombstone {
                tombstone_id: kp_store::tombstone_id(
                    unrelated.origin_library.as_str().as_bytes(),
                    &unrelated.origin_library,
                    &unrelated.logical_key,
                )?,
                origin_library_id: unrelated.origin_library.clone(),
                purged_at: EpochMs(0),
                reason: "remote purge".into(),
            })?,
            "import" => {
                let snapshot = kp_store::ChangeSnapshot {
                    library_id: "remote".parse()?,
                    cursor: kp_model::ContentHash::digest("empty"),
                    unchanged: false,
                    facts: Vec::new(),
                    justifications: Vec::new(),
                    tombstones: Vec::new(),
                    origin_references: Default::default(),
                };
                store.import_snapshot("https://remote.example", &snapshot, EpochMs(0))?;
            }
            "approve" => {
                store.approve_skill(&"check-api-contract".parse()?, &actor, EpochMs(100))?
            }
            _ => return Err("unexpected operation".into()),
        }
        assert_eq!(std::fs::read(&path)?, before, "{operation}");
    }
    Ok(())
}

#[tokio::test]
async fn read_only_hooks_do_not_wait_for_writers_or_mutate_the_projection() -> Result {
    let (dir, mut store) = library()?;
    promote(&mut store, &[]).await?;
    let marker = dir.path().join("skills/read-only-marker");
    std::fs::write(&marker, "untouched")?;
    let ro_store = kp_store::Store::open_readonly(dir.path())?;
    let session_start = kp_ingest::hooks::run_readonly(
        &ro_store,
        "session-start",
        &json!({"prompt":"check api contract"}),
        &"alice".parse()?,
        EpochMs(40),
    )?;
    assert_eq!(session_start, json!({}));
    assert_eq!(std::fs::read_to_string(&marker)?, "untouched");

    let context = kp_ingest::hooks::run_readonly(
        &ro_store,
        "user-prompt-submit",
        &json!({"prompt":"check api contract"}),
        &"alice".parse()?,
        EpochMs(40),
    )?;
    assert!(context.to_string().contains("check-api-contract"));
    assert_eq!(std::fs::read_to_string(&marker)?, "untouched");
    Ok(())
}
