use kp_model::{FactStatus, LibraryMode, MemberRole, RsiDimension};

use super::*;
use crate::test_support::{derived, fact, justification, library, AT};

fn shared_bullet(store: &mut Store, author: &str, local: bool) -> Result<Fact> {
    let support = fact("support", &["owner", "author", "member", "maintainer"])?;
    let mut bullet = derived("bullet")?;
    bullet.kind = FactKind::PlaybookBullet;
    bullet.author = author.parse()?;
    if local {
        bullet.origin_library = store.library_id().clone();
    }
    let attribution = Attribution {
        skill_slug: "verify-recovery".parse()?,
        trigger: "repairing a failed build".into(),
        wrong_move: "Assume the repair succeeded".into(),
        verified_fix: "Rerun the failing target and check its result.".into(),
        dimension: RsiDimension::Inv,
        cited_premise_ids: vec![support.id.clone()],
    };
    bullet.object = serde_json::json!({"attribution": attribution});
    store.insert_fact(&support)?;
    store.insert_fact(&bullet)?;
    store.insert_justification(&justification("proof", &bullet, &[&support])?)?;
    for (name, role) in [
        ("author", MemberRole::Member),
        ("member", MemberRole::Member),
        ("maintainer", MemberRole::Maintainer),
    ] {
        store.add_member(name.parse()?, role, AT)?;
    }
    Ok(bullet)
}

#[test]
fn visible_member_feedback_cannot_promote_or_roll_back_owner_skills() -> Result<()> {
    let (dir, mut store) = library(LibraryMode::Personal)?;
    let bullet = shared_bullet(&mut store, "owner", true)?;
    let member = "member".parse()?;
    let owner = "owner".parse()?;
    for outcome in [FeedbackOutcome::Helped, FeedbackOutcome::Hurt] {
        for task in ["spoof-1", "spoof-2"] {
            assert!(matches!(
                store.feedback(bullet.id.as_str(), &member, task, outcome, None, AT),
                Err(StoreError::PermissionDenied)
            ));
        }
    }
    assert!(store.promote_skill(&bullet.id, &owner, AT)?.is_none());
    assert_eq!(store.feedback_counts(&bullet.id, &owner, AT)?.helped, 0);
    for task in ["verified-1", "verified-2"] {
        store.feedback(
            bullet.id.as_str(),
            &owner,
            task,
            FeedbackOutcome::Helped,
            None,
            AT,
        )?;
    }
    let skill = store
        .promote_skill(&bullet.id, &owner, AT)?
        .ok_or(StoreError::NotFound)?;
    let path = dir.path().join("skills/verify-recovery/SKILL.md");
    let before = std::fs::read(&path)?;
    for target in [skill.as_str(), "verify-recovery"] {
        for task in ["hurt-1", "hurt-2"] {
            assert!(matches!(
                store.feedback(target, &member, task, FeedbackOutcome::Hurt, None, AT),
                Err(StoreError::PermissionDenied)
            ));
        }
    }
    assert_eq!(store.feedback_counts(&skill, &owner, AT)?.hurt, 0);
    assert_eq!(
        store.derivation_view()?.facts[&skill].status,
        FactStatus::Active
    );
    assert_eq!(std::fs::read(path)?, before);
    Ok(())
}

#[test]
fn lifecycle_counts_ignore_legacy_unauthorized_and_revoked_feedback() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let bullet = shared_bullet(&mut store, "owner", true)?;
    let owner = "owner".parse()?;
    let member = "member".parse()?;
    for task in ["spoof-1", "spoof-2"] {
        write_feedback(
            &store.db,
            &bullet.id,
            &member,
            task,
            FeedbackOutcome::Helped,
            None,
            AT,
        )?;
    }
    assert_eq!(store.feedback_counts(&bullet.id, &owner, AT)?.helped, 0);
    assert!(store.promote_skill(&bullet.id, &owner, AT)?.is_none());
    for task in ["real-1", "real-2"] {
        store.feedback(
            bullet.id.as_str(),
            &owner,
            task,
            FeedbackOutcome::Helped,
            None,
            AT,
        )?;
    }
    let skill = store
        .promote_skill(&bullet.id, &owner, AT)?
        .ok_or(StoreError::NotFound)?;
    for task in ["hurt-1", "hurt-2"] {
        write_feedback(
            &store.db,
            &skill,
            &member,
            task,
            FeedbackOutcome::Hurt,
            None,
            AT,
        )?;
        write_feedback(
            &store.db,
            &skill,
            &"maintainer".parse()?,
            task,
            FeedbackOutcome::Hurt,
            None,
            AT,
        )?;
    }
    store.revoke_member(&"maintainer".parse()?, AT)?;
    let result = store.feedback(
        skill.as_str(),
        &owner,
        "neutral",
        FeedbackOutcome::Neutral,
        None,
        AT,
    )?;
    assert_eq!(result.counts.hurt, 0);
    assert!(!result.rolled_back);
    Ok(())
}

#[test]
fn local_authors_and_administrators_can_govern_feedback_but_only_owner_auto_approves() -> Result<()>
{
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let bullet = shared_bullet(&mut store, "author", true)?;
    for actor in ["author", "maintainer", "owner"] {
        assert!(
            store
                .feedback(
                    bullet.id.as_str(),
                    &actor.parse()?,
                    actor,
                    FeedbackOutcome::Helped,
                    None,
                    AT
                )?
                .recorded
        );
    }
    let author = "author".parse()?;
    let skill = store
        .promote_skill(&bullet.id, &author, AT)?
        .ok_or(StoreError::NotFound)?;
    let active = store.active_skills(&author, AT)?;
    assert_eq!(active.len(), 1);
    assert!(!active[0].package.exec_approved);
    assert_eq!(active[0].notice, Some(kp_model::READ_ONLY_SKILL));
    assert_eq!(store.feedback_counts(&skill, &author, AT)?.helped, 3);
    Ok(())
}

#[test]
fn imported_skill_feedback_requires_local_administrators_even_for_its_author() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let bullet = shared_bullet(&mut store, "author", false)?;
    let mut skill = derived("remote-skill")?;
    skill.kind = FactKind::SkillVersion;
    skill.author = bullet.author.clone();
    skill.object = serde_json::to_value(SkillPackage {
        name: "remote-rule".parse()?,
        description: "Imported rule".into(),
        version: 1,
        dimension: RsiDimension::Inv,
        trigger: "repairing builds".into(),
        rules: vec!["Rerun the failing target.".into()],
        references: vec![bullet.strong_ref()],
        scripts: Vec::new(),
        exec_approved: true,
    })?;
    store.insert_fact(&skill)?;
    store.insert_justification(&justification("remote-proof", &skill, &[&bullet])?)?;
    for target in [&bullet, &skill] {
        for actor in ["author", "member"] {
            assert!(matches!(
                store.feedback(
                    target.id.as_str(),
                    &actor.parse()?,
                    "denied",
                    FeedbackOutcome::Hurt,
                    None,
                    AT
                ),
                Err(StoreError::PermissionDenied)
            ));
            for task in ["old-hurt-1", "old-hurt-2"] {
                write_feedback(
                    &store.db,
                    &target.id,
                    &actor.parse()?,
                    task,
                    FeedbackOutcome::Hurt,
                    None,
                    AT,
                )?;
            }
        }
        for actor in ["owner", "maintainer"] {
            let result = store.feedback(
                target.id.as_str(),
                &actor.parse()?,
                actor,
                FeedbackOutcome::Helped,
                None,
                AT,
            )?;
            assert!(result.recorded);
            assert_eq!(result.counts.hurt, 0);
        }
    }
    Ok(())
}

#[test]
fn historical_remote_tombstones_preserve_skills_in_personal_and_community_libraries() -> Result<()>
{
    for mode in [LibraryMode::Personal, LibraryMode::Community] {
        let (dir, mut store) = library(mode)?;
        let bullet = shared_bullet(&mut store, "owner", true)?;
        let owner = "owner".parse()?;
        for task in ["verified-1", "verified-2"] {
            store.feedback(
                bullet.id.as_str(),
                &owner,
                task,
                FeedbackOutcome::Helped,
                None,
                AT,
            )?;
        }
        assert!(store.promote_skill(&bullet.id, &owner, AT)?.is_some());
        let remote = fact("unrelated-remote", &["owner"])?;
        store.insert_fact(&remote)?;
        let path = dir.path().join("skills/verify-recovery/SKILL.md");
        let before = std::fs::read(&path)?;
        store.apply_tombstone(&crate::Tombstone {
            tombstone_id: store.tombstone_id(&remote.origin_library, &remote.logical_key)?,
            origin_library_id: remote.origin_library.clone(),
            purged_at: EpochMs(0),
            reason: "remote purge".into(),
        })?;
        assert!(store.resolve_fact(remote.id.as_str())?.is_none());
        assert_eq!(std::fs::read(&path)?, before);
    }
    Ok(())
}

#[test]
fn retrieval_approval_is_bound_to_the_local_content_hash() -> Result<()> {
    let (_dir, mut store) = library(LibraryMode::Personal)?;
    let bullet = shared_bullet(&mut store, "owner", true)?;
    let owner = "owner".parse()?;
    for task in ["verified-1", "verified-2"] {
        store.feedback(
            bullet.id.as_str(),
            &owner,
            task,
            FeedbackOutcome::Helped,
            None,
            AT,
        )?;
    }
    let skill = store
        .promote_skill(&bullet.id, &owner, AT)?
        .ok_or(StoreError::NotFound)?;
    let active = store.active_skills(&owner, AT)?;
    assert!(active[0].package.exec_approved && active[0].package.scripts.is_empty());
    store.db.execute(
        "UPDATE skill_approvals SET content_sha256=?1 WHERE fact_id=?2",
        params![
            ContentHash::digest("other version").as_str(),
            skill.as_str()
        ],
    )?;
    let snapshot = store.retrieval_snapshot(&owner, AT)?;
    let doc = snapshot
        .documents()
        .iter()
        .find(|doc| doc.id == format!("fact-{skill}"))
        .ok_or(StoreError::NotFound)?;
    assert!(doc.passage.contains("\"exec_approved\":false"));
    assert!(doc.passage.contains(kp_model::READ_ONLY_SKILL));
    store.approve_skill(&active[0].package.name, &owner, AT)?;
    let snapshot = store.retrieval_snapshot(&owner, AT)?;
    let doc = snapshot
        .documents()
        .iter()
        .find(|doc| doc.id == format!("fact-{skill}"))
        .ok_or(StoreError::NotFound)?;
    assert!(doc.passage.contains("\"exec_approved\":true"));
    assert!(!doc.passage.contains(kp_model::READ_ONLY_SKILL));
    Ok(())
}

#[test]
fn multi_source_clustering_merges_paraphrased_fixes_and_k_of_n_support_survives_single_withdrawal(
) -> Result<()> {
    let (dir, mut store) = library(LibraryMode::Personal)?;
    let owner = "owner".parse()?;

    let p1 = fact("premise-video", &["owner"])?;
    let p2 = fact("premise-doc", &["owner"])?;
    let p3 = fact("premise-session", &["owner"])?;
    for p in [&p1, &p2, &p3] {
        store.insert_fact(p)?;
    }

    let sources = [
        (
            "video-1",
            p1.id.clone(),
            "Pass --schema ./db/schema.prisma before running prisma migrate deploy.",
        ),
        (
            "doc-2",
            p2.id.clone(),
            "Always specify --schema ./db/schema.prisma on prisma migrate commands and check exit code 0.",
        ),
        (
            "session-3",
            p3.id.clone(),
            "Verify the target schema path exists before executing prisma migrate deploy.",
        ),
    ];

    let mut bullet_id = None;
    for (idx, (sess, pid, fix)) in sources.iter().enumerate() {
        let ep = crate::Episode {
            signal_kind: kp_model::ContrastKind::FailureToRecovery,
            session_id: (*sess).into(),
            before_trace: serde_json::json!({"content": "prisma migrate failed", "success": false}),
            after_trace: serde_json::json!({"content": fix, "success": true}),
            step_ref: format!("step:{idx}"),
            environment: serde_json::json!({"actor": "owner", "premise_ids": [pid]}),
        };
        store.record_episodes(&[ep], AT)?;
        let pending = store
            .pending_episodes(&owner)?
            .into_iter()
            .find(|p| p.episode.session_id == *sess)
            .ok_or(StoreError::NotFound)?;
        let attr = Attribution {
            skill_slug: "prisma-schema-flag-check".parse()?,
            trigger: "running prisma database migrations".into(),
            wrong_move: "Invoke prisma migrate without an explicit --schema path".into(),
            verified_fix: (*fix).into(),
            dimension: RsiDimension::Inv,
            cited_premise_ids: vec![pid.clone()],
        };
        let applied = store.apply_attribution(&pending, &owner, Some(&attr), AT)?;
        assert_eq!(applied.created, idx == 0);
        bullet_id = applied.bullet;
    }

    let bid = bullet_id.ok_or(StoreError::NotFound)?;
    assert_eq!(store.feedback_counts(&bid, &owner, AT)?.helped, 3);
    let skill_id = store
        .promote_skill(&bid, &owner, AT)?
        .ok_or(StoreError::NotFound)?;
    let skill_md = dir.path().join("skills/prisma-schema-flag-check/SKILL.md");
    assert!(skill_md.is_file());
    let rendered = std::fs::read_to_string(&skill_md)?;
    assert!(rendered.contains("Pass --schema ./db/schema.prisma"));
    assert!(rendered.contains("Always specify --schema ./db/schema.prisma"));
    assert!(rendered.contains("Verify the target schema path exists"));

    store.forget_fact_id(&p1.id, &owner, EpochMs(AT.0 + 10))?;
    assert_eq!(
        store
            .feedback_counts(&bid, &owner, EpochMs(AT.0 + 11))?
            .helped,
        2
    );
    assert_eq!(
        store.derivation_view()?.facts[&skill_id].status,
        FactStatus::Active
    );
    assert!(skill_md.is_file());

    store.forget_fact_id(&p2.id, &owner, EpochMs(AT.0 + 20))?;
    assert_eq!(
        store
            .feedback_counts(&bid, &owner, EpochMs(AT.0 + 21))?
            .helped,
        1
    );
    assert_eq!(
        store.derivation_view()?.facts[&skill_id].status,
        FactStatus::Invalidated
    );
    assert!(!skill_md.exists());
    assert_eq!(
        store.derivation_view()?.facts[&bid].status,
        FactStatus::Active
    );

    store.forget_fact_id(&p3.id, &owner, EpochMs(AT.0 + 30))?;
    assert_eq!(
        store.derivation_view()?.facts[&bid].status,
        FactStatus::Invalidated
    );
    Ok(())
}
