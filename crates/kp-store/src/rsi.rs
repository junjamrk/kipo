use std::collections::BTreeSet;

use kp_model::{
    derived_fact, justification, ActorId, Attribution, AudienceSpec, BitemporalStamp, ContentHash,
    DerivationMode, DreamDistillEnvelope, EpochMs, Fact, FactAccess, FactId, FactKind, FactStatus,
    FeedbackCounts, FeedbackOutcome, KpError, SkillPackage, SourceScopeId, TrustTier,
    ValidityWindow,
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;

use crate::facts::{read_graph, write_fact, write_justification};
use crate::{PendingEpisode, Result, Store, StoreError};

#[derive(Default, Debug, Serialize)]
pub struct AppliedEpisode {
    pub processed: bool,
    pub created: bool,
    pub bullet: Option<FactId>,
}

#[derive(Debug, Serialize)]
pub struct FeedbackResult {
    pub fact_id: FactId,
    pub recorded: bool,
    pub counts: FeedbackCounts,
    pub rolled_back: bool,
}

impl Store {
    /// Claims the pending episode, adds provenance and independent feedback, and consumes it atomically.
    pub fn apply_attribution(
        &mut self,
        pending: &PendingEpisode,
        actor: &ActorId,
        attribution: Option<&Attribution>,
        at: EpochMs,
    ) -> Result<AppliedEpisode> {
        self.apply_attribution_with_rules(pending, actor, attribution, &[], at)
    }

    /// Like [`Self::apply_attribution`], additionally recording complementary rules synthesized
    /// during multi-source / generative distillation without mutating the base bullet's SHA-256.
    pub fn apply_attribution_with_rules(
        &mut self,
        pending: &PendingEpisode,
        actor: &ActorId,
        attribution: Option<&Attribution>,
        extra_rules: &[String],
        at: EpochMs,
    ) -> Result<AppliedEpisode> {
        if self.member_role(actor)?.is_none() {
            return Err(StoreError::PermissionDenied);
        }
        let owner = self.status()?.owner_actor;
        let sanitized_extra = DreamDistillEnvelope::sanitize_extra_rules(extra_rules)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let consumed = tx.execute("UPDATE rsi_episodes SET processed=1 WHERE episode_id=?1 AND processed=0 AND COALESCE(json_extract(environment_json,'$.actor'),?3)=?2", params![pending.id.as_str(),actor.as_str(),owner.as_str()])?;
        if consumed == 0 {
            return Ok(AppliedEpisode::default());
        }
        let Some(attribution) = attribution else {
            tx.commit()?;
            return Ok(AppliedEpisode {
                processed: true,
                ..AppliedEpisode::default()
            });
        };
        let attribution = attribution.clone().sanitize()?;
        let graph = read_graph(&tx)?;
        let visible = graph.evaluate(actor, at);
        let mut premises = Vec::new();
        for id in &attribution.cited_premise_ids {
            if !visible.facts.contains(id) {
                return Err(StoreError::NotFound);
            }
            premises.push(graph.facts.get(id).ok_or(StoreError::NotFound)?.clone());
        }
        let env_premise_ids: Vec<FactId> = pending
            .episode
            .environment
            .get("premise_ids")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        let linked_premise_ids: Vec<FactId> = {
            let mut stmt =
                tx.prepare("SELECT fact_id FROM rsi_episode_facts WHERE episode_id = ?1")?;
            let rows = stmt.query_map([pending.id.as_str()], |r| r.get::<_, String>(0))?;
            let mut ids = Vec::new();
            for row in rows {
                if let Ok(fid) = row?.parse::<FactId>() {
                    ids.push(fid);
                }
            }
            ids
        };
        if premises
            .iter()
            .any(|f| f.trust_tier == TrustTier::ExternalUntrusted)
            || env_premise_ids
                .iter()
                .chain(linked_premise_ids.iter())
                .any(|id| {
                    graph
                        .facts
                        .get(id)
                        .is_some_and(|f| f.trust_tier == TrustTier::ExternalUntrusted)
                })
        {
            tx.commit()?;
            return Ok(AppliedEpisode {
                processed: true,
                created: false,
                bullet: None,
            });
        }
        let identity =
            ContentHash::digest(serde_json::to_vec(&(&self.library_id, actor, &pending.id))?);
        let mut anchor = derived_fact(
            &self.library_id,
            actor,
            FactKind::Observation,
            "Contrastive verification",
            "kp.rsi.episode",
            serde_json::json!({"episode_digest":pending.id}),
            at,
        )?;
        anchor.id = format!("episode-{identity}").parse()?;
        anchor.logical_key = anchor.id.as_str().parse()?;
        anchor.access = FactAccess::Base {
            audience: AudienceSpec::Explicit(BTreeSet::from([actor.clone()])),
            stamp: BitemporalStamp {
                valid: ValidityWindow {
                    valid_from: at,
                    valid_until: EpochMs::OPEN_END,
                },
                recorded_at: at,
                expired_at: None,
            },
        };
        if let Some(source) = pending
            .episode
            .environment
            .get("source_id")
            .and_then(serde_json::Value::as_str)
        {
            let sid: kp_model::SourceId = source.parse()?;
            let scope_opt: Option<String> = tx
                .query_row(
                    "SELECT source_scope_id FROM raw_sources WHERE source_id = ?1",
                    [sid.as_str()],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            if let Some(scope_raw) = scope_opt.filter(|s| !s.trim().is_empty()) {
                anchor.source_scope_id = Some(scope_raw.parse::<SourceScopeId>()?);
            }
            anchor.source_anchor = Some(kp_model::AnchorSpec {
                source_id: Some(sid),
                ..Default::default()
            });
        }
        let anchor_id_str = anchor.id.as_str().to_owned();
        insert(&tx, &anchor)?;
        premises.push(anchor);

        let key = attribution.normalized_key();
        let cluster = attribution.cluster_key();
        let existing = graph.facts.values().find(|f| {
            f.kind == FactKind::PlaybookBullet
                && f.origin_library == self.library_id
                && f.author == *actor
                && visible.facts.contains(&f.id)
                && (f
                    .object
                    .get("normalized_key")
                    .and_then(serde_json::Value::as_str)
                    == Some(key.as_str())
                    || f.object
                        .get("cluster_key")
                        .and_then(serde_json::Value::as_str)
                        == Some(cluster.as_str())
                    || (f.subject == attribution.skill_slug.as_str()
                        && f.object
                            .pointer("/attribution/dimension")
                            .and_then(serde_json::Value::as_str)
                            == Some(attribution.dimension.as_str())))
        });
        let created = existing.is_none();
        let bullet = match existing {
            Some(fact) => fact.clone(),
            None => {
                let mut fact = derived_fact(
                    &self.library_id,
                    actor,
                    FactKind::PlaybookBullet,
                    attribution.skill_slug.as_str(),
                    "kp.rsi.playbook",
                    serde_json::json!({
                        "normalized_key": key,
                        "cluster_key": cluster,
                        "attribution": attribution,
                    }),
                    at,
                )?;
                // Actor and first episode partition private accumulators and preserve immutable retired versions.
                let hash = ContentHash::digest(serde_json::to_vec(&(
                    &self.library_id,
                    actor,
                    &pending.id,
                    &key,
                ))?);
                fact.id = format!("bullet-{hash}").parse()?;
                fact.logical_key = fact.id.as_str().parse()?;
                insert(&tx, &fact)?;
                fact
            }
        };
        let mut complementary_rules = Vec::new();
        let base_fix = bullet
            .object
            .pointer("/attribution/verified_fix")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if !created && !base_fix.eq_ignore_ascii_case(&attribution.verified_fix) {
            complementary_rules.push(attribution.verified_fix.clone());
        }
        for r in sanitized_extra {
            if !r.eq_ignore_ascii_case(base_fix)
                && !complementary_rules
                    .iter()
                    .any(|c: &String| c.eq_ignore_ascii_case(&r))
            {
                complementary_rules.push(r);
            }
        }
        if !complementary_rules.is_empty() {
            tx.execute(
                "INSERT INTO change_log (op, target_id, actor_id, payload_json, created_at)
                 VALUES ('rsi_bullet_rule', ?1, ?2, ?3, ?4)",
                params![
                    bullet.id.as_str(),
                    actor.as_str(),
                    serde_json::to_string(&serde_json::json!({
                        "episode_anchor_id": anchor_id_str,
                        "rules": complementary_rules,
                    }))?,
                    at.0
                ],
            )?;
        }
        let proof = justification(
            &bullet,
            DerivationMode::RsiAttribution,
            "RSI_CONTRAST",
            &premises.iter().collect::<Vec<_>>(),
        )?;
        write_justification(&tx, &proof)?;
        for fact in premises.iter().chain(std::iter::once(&bullet)) {
            tx.execute(
                "INSERT OR IGNORE INTO rsi_episode_facts VALUES (?1,?2)",
                params![pending.id.as_str(), fact.id.as_str()],
            )?;
        }
        let independence = if pending.episode.session_id.is_empty() {
            pending.id.clone()
        } else {
            ContentHash::digest(serde_json::to_vec(&(actor, &pending.episode.session_id))?)
        };
        let task = format!("episode:{independence}");
        tx.execute(
            "INSERT INTO change_log (op, target_id, actor_id, payload_json, created_at)
             VALUES ('rsi_episode_task', ?1, ?2, ?3, ?4)",
            params![
                bullet.id.as_str(),
                actor.as_str(),
                serde_json::to_string(&serde_json::json!({
                    "episode_anchor_id": anchor_id_str,
                    "task": task,
                }))?,
                at.0
            ],
        )?;
        write_feedback(
            &tx,
            &bullet.id,
            actor,
            &task,
            FeedbackOutcome::Helped,
            None,
            at,
        )?;
        tx.commit()?;
        Ok(AppliedEpisode {
            processed: true,
            created,
            bullet: Some(bullet.id),
        })
    }

    pub fn feedback_counts(
        &self,
        id: &FactId,
        actor: &ActorId,
        at: EpochMs,
    ) -> Result<FeedbackCounts> {
        let graph = self.derivation_view()?;
        if !graph.evaluate(actor, at).facts.contains(id) {
            return Err(StoreError::NotFound);
        }
        counts(&self.db, graph.facts.get(id).ok_or(StoreError::NotFound)?)
    }

    /// Promotion rechecks the threshold and proof visibility under the write lock.
    pub fn promote_skill(
        &mut self,
        bullet_id: &FactId,
        actor: &ActorId,
        at: EpochMs,
    ) -> Result<Option<FactId>> {
        let owner = self.status()?.owner_actor;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let graph = read_graph(&tx)?;
        let visible = graph.evaluate(actor, at);
        let bullet = graph
            .facts
            .get(bullet_id)
            .filter(|f| {
                f.kind == FactKind::PlaybookBullet
                    && f.origin_library == self.library_id
                    && f.author == *actor
                    && visible.facts.contains(&f.id)
            })
            .ok_or(StoreError::NotFound)?;
        if !counts(&tx, bullet)?.promotable() {
            return Ok(None);
        }
        let attribution: Attribution =
            serde_json::from_value(bullet.object["attribution"].clone())?;
        let mut version = 1_u32;
        for fact in graph.facts.values().filter(|f| {
            f.kind == FactKind::SkillVersion
                && f.origin_library == self.library_id
                && f.author == *actor
                && f.subject == attribution.skill_slug.as_str()
        }) {
            let package: SkillPackage = serde_json::from_value(fact.object.clone())?;
            if package.references.contains(&bullet.strong_ref()) {
                return Ok(None);
            }
            version = version.max(
                package
                    .version
                    .checked_add(1)
                    .ok_or_else(|| KpError::invalid("version", "exhausted"))?,
            );
        }
        let mut premise_ids = BTreeSet::from([bullet.id.clone()]);
        let mut active_episode_anchors = BTreeSet::new();
        let mut has_external_untrusted = false;
        // Promotion depends on the verification anchors as well as the reusable bullet.
        for proof in graph
            .justifications
            .values()
            .filter(|p| p.conclusion == bullet.id && p.active)
        {
            for premise in &proof.premises {
                if !visible.facts.contains(&premise.fact_id) {
                    return Err(StoreError::NotFound);
                }
                if premise.fact_id.as_str().starts_with("episode-") {
                    active_episode_anchors.insert(premise.fact_id.as_str().to_owned());
                }
                if graph
                    .facts
                    .get(&premise.fact_id)
                    .is_some_and(|f| f.trust_tier == TrustTier::ExternalUntrusted)
                {
                    has_external_untrusted = true;
                }
                premise_ids.insert(premise.fact_id.clone());
            }
        }
        let mut rules = vec![attribution.verified_fix];
        collect_active_complementary_rules(
            &tx,
            bullet.id.as_str(),
            &active_episode_anchors,
            &mut rules,
        )?;
        let scripts = Vec::new();
        let package = SkillPackage {
            name: attribution.skill_slug,
            description: format!("{} rule: {}", attribution.dimension, attribution.trigger),
            version,
            dimension: attribution.dimension,
            trigger: attribution.trigger,
            rules,
            references: vec![bullet.strong_ref()],
            exec_approved: actor == &owner && scripts.is_empty() && !has_external_untrusted,
            scripts,
        };
        package.validate_markdown(
            &package.render(package.exec_approved)?,
            package.exec_approved,
        )?;
        let mut skill = derived_fact(
            &self.library_id,
            actor,
            FactKind::SkillVersion,
            package.name.as_str(),
            "kp.rsi.skill",
            serde_json::to_value(&package)?,
            at,
        )?;
        let identity = ContentHash::digest(serde_json::to_vec(&(
            &self.library_id,
            actor,
            skill.strong_ref(),
        ))?);
        skill.id = format!("skill-{identity}").parse()?;
        skill.logical_key = skill.id.as_str().parse()?;
        insert(&tx, &skill)?;
        let premises = premise_ids
            .iter()
            .filter_map(|id| graph.facts.get(id))
            .collect::<Vec<_>>();
        write_justification(
            &tx,
            &justification(
                &skill,
                DerivationMode::RsiAttribution,
                "RSI_PROMOTION",
                &premises,
            )?,
        )?;
        tx.execute("INSERT INTO rsi_feedback SELECT feedback_id || ?1,?1,episode_or_task_id,actor_id,model_family,outcome,created_at FROM rsi_feedback WHERE skill_fact_id=?2",params![skill.id.as_str(),bullet.id.as_str()])?;
        tx.execute("INSERT INTO rsi_episode_facts SELECT episode_id,?1 FROM rsi_episode_facts WHERE fact_id=?2", params![skill.id.as_str(),bullet.id.as_str()])?;
        tx.execute(
            "INSERT INTO skill_approvals VALUES (?1,?2,?3,?4,?5)",
            params![
                skill.id.as_str(),
                skill.strong_ref().content_sha256.as_str(),
                package.exec_approved,
                actor.as_str(),
                at.0
            ],
        )?;
        tx.commit()?;
        self.sync_skills_now()?;
        Ok(Some(skill.id))
    }

    pub fn feedback(
        &mut self,
        target: &str,
        actor: &ActorId,
        task: &str,
        outcome: FeedbackOutcome,
        model_family: Option<&str>,
        at: EpochMs,
    ) -> Result<FeedbackResult> {
        if self.member_role(actor)?.is_none() {
            return Err(StoreError::PermissionDenied);
        }
        if task.trim().is_empty() || task.len() > 1024 {
            return Err(KpError::invalid("task", "requires a bounded nonempty task ID").into());
        }
        let redactor = kp_model::SecretRedactor::new()
            .map_err(|_| KpError::invalid("redactor", "invalid pattern"))?;
        let task = redactor.redact(task);
        let model_family = model_family.map(|text| redactor.redact(text));
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let graph = read_graph(&tx)?;
        let visible = graph.evaluate(actor, at);
        let exact = graph.facts.values().find(|f| {
            visible.facts.contains(&f.id)
                && matches!(f.kind, FactKind::PlaybookBullet | FactKind::SkillVersion)
                && f.id.as_str() == target
        });
        let fact = match exact {
            Some(fact) => fact,
            None => {
                let skill = crate::skills::active(&tx, actor, at)?
                    .into_iter()
                    .find(|s| s.package.name.as_str() == target)
                    .ok_or(StoreError::NotFound)?;
                graph
                    .facts
                    .get(&skill.fact_id)
                    .ok_or(StoreError::NotFound)?
            }
        };
        if !feedback_actors(&tx, fact)?.contains(actor) {
            return Err(StoreError::PermissionDenied);
        }
        let recorded = write_feedback(
            &tx,
            &fact.id,
            actor,
            &task,
            outcome,
            model_family.as_deref(),
            at,
        )?;
        let counts = counts(&tx, fact)?;
        let rolled_back = counts.requires_rollback();
        if rolled_back {
            let mut retired = BTreeSet::from([fact.id.clone()]);
            if fact.kind == FactKind::SkillVersion {
                let package: SkillPackage = serde_json::from_value(fact.object.clone())?;
                retired.extend(
                    graph
                        .facts
                        .values()
                        .filter(|bullet| {
                            bullet.kind == FactKind::PlaybookBullet
                                && bullet.origin_library == fact.origin_library
                                && bullet.author == fact.author
                                && package.references.contains(&bullet.strong_ref())
                        })
                        .map(|bullet| bullet.id.clone()),
                );
            }
            for id in retired {
                tx.execute(
                    "UPDATE facts SET status='invalidated',expired_at=?1 WHERE fact_id=?2",
                    params![at.0, id.as_str()],
                )?;
                tx.execute("UPDATE justifications SET active=0,invalidated_at=?1,invalidation_reason='rsi_hurt_threshold' WHERE conclusion_fact_id=?2",params![at.0,id.as_str()])?;
                crate::maintenance::invalidate(&tx, &id, at, "rsi_hurt_threshold")?;
                crate::exchange::cancel_publishes_and_queue_withdrawals(&tx, &id)?;
            }
        }
        let result = FeedbackResult {
            fact_id: fact.id.clone(),
            recorded,
            counts,
            rolled_back,
        };
        tx.commit()?;
        self.sync_skills_now()?;
        Ok(result)
    }
}

fn insert(db: &Connection, fact: &Fact) -> Result<()> {
    let opaque = crate::tombstone_id(
        fact.origin_library.as_str().as_bytes(),
        &fact.origin_library,
        &fact.logical_key,
    )?;
    write_fact(db, fact, &opaque)
}

fn write_feedback(
    db: &Connection,
    id: &FactId,
    actor: &ActorId,
    task: &str,
    outcome: FeedbackOutcome,
    model_family: Option<&str>,
    at: EpochMs,
) -> Result<bool> {
    let key = ContentHash::digest(serde_json::to_vec(&(id, actor, task))?);
    Ok(db.execute("INSERT INTO rsi_feedback VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(skill_fact_id,actor_id,episode_or_task_id) DO NOTHING",params![key.as_str(),id.as_str(),task,actor.as_str(),model_family,outcome.as_str(),at.0])? > 0)
}

fn feedback_actors(db: &Connection, fact: &Fact) -> Result<BTreeSet<ActorId>> {
    let mut stmt = db.prepare(
        "SELECT actor_id FROM members_tokens CROSS JOIN library_meta
         WHERE revoked_at IS NULL AND (role IN ('owner','maintainer')
             OR (library_id=?1 AND actor_id=?2))",
    )?;
    let rows = stmt.query_map(
        params![fact.origin_library.as_str(), fact.author.as_str()],
        |r| r.get::<_, String>(0),
    )?;
    rows.map(|row| Ok(row?.parse()?)).collect()
}

fn counts(db: &Connection, fact: &Fact) -> Result<FeedbackCounts> {
    let actors = serde_json::to_string(&feedback_actors(db, fact)?)?;
    let count = |outcome: &str| -> Result<usize> {
        Ok(db.query_row(
            "SELECT COUNT(DISTINCT episode_or_task_id) FROM rsi_feedback
             WHERE skill_fact_id=?1 AND outcome=?2 AND actor_id IN (SELECT value FROM json_each(?3))",
            params![fact.id.as_str(),outcome,actors], |row| row.get(0))?)
    };
    Ok(FeedbackCounts {
        helped: count("helped")?,
        hurt: count("hurt")?,
        neutral: count("neutral")?,
    })
}

const MAX_SYNTHESIZED_SKILL_RULES: usize = 5;

fn collect_active_complementary_rules(
    db: &Connection,
    bullet_id: &str,
    active_episode_anchors: &BTreeSet<String>,
    rules: &mut Vec<String>,
) -> Result<()> {
    let mut stmt = db.prepare(
        "SELECT payload_json FROM change_log
         WHERE op = 'rsi_bullet_rule' AND target_id = ?1
         ORDER BY seq ASC",
    )?;
    let rows = stmt.query_map([bullet_id], |r| r.get::<_, String>(0))?;
    for row in rows {
        if rules.len() >= MAX_SYNTHESIZED_SKILL_RULES {
            break;
        }
        let raw = row?;
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        let Some(anchor_id) = val
            .get("episode_anchor_id")
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        if !active_episode_anchors.contains(anchor_id) {
            continue;
        }
        if let Some(arr) = val.get("rules").and_then(serde_json::Value::as_array) {
            for item in arr.iter().filter_map(serde_json::Value::as_str) {
                if rules.len() >= MAX_SYNTHESIZED_SKILL_RULES {
                    break;
                }
                if !rules.iter().any(|r| r.eq_ignore_ascii_case(item)) {
                    rules.push(item.to_owned());
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn reconcile_rsi_conclusions(
    db: &Connection,
    touched_conclusions: &BTreeSet<FactId>,
    at: EpochMs,
) -> Result<()> {
    let graph = read_graph(db)?;
    let grounded = graph.grounded_support();

    let mut touched_bullets = BTreeSet::new();
    for cid in touched_conclusions {
        let Some(fact) = graph.facts.get(cid) else {
            continue;
        };
        match fact.kind {
            FactKind::PlaybookBullet => {
                touched_bullets.insert(fact.id.clone());
            }
            FactKind::SkillVersion => {
                if let Ok(pkg) = serde_json::from_value::<SkillPackage>(fact.object.clone()) {
                    for b in graph.facts.values().filter(|b| {
                        b.kind == FactKind::PlaybookBullet
                            && b.origin_library == fact.origin_library
                            && b.author == fact.author
                            && pkg.references.contains(&b.strong_ref())
                    }) {
                        touched_bullets.insert(b.id.clone());
                    }
                }
            }
            _ => {}
        }
    }

    for bullet_id in &touched_bullets {
        let active_anchors: BTreeSet<String> = graph
            .justifications
            .values()
            .filter(|j| {
                j.conclusion == *bullet_id
                    && j.active
                    && j.premises.iter().all(|p| grounded.contains(&p.fact_id))
            })
            .flat_map(|j| j.premises.iter())
            .filter(|p| p.fact_id.as_str().starts_with("episode-"))
            .map(|p| p.fact_id.as_str().to_owned())
            .collect();

        let mut stmt = db.prepare(
            "SELECT payload_json FROM change_log
             WHERE op = 'rsi_episode_task' AND target_id = ?1",
        )?;
        let rows = stmt.query_map([bullet_id.as_str()], |r| r.get::<_, String>(0))?;
        let mut active_tasks = BTreeSet::new();
        let mut all_logged_tasks = BTreeSet::new();
        for row in rows {
            let raw = row?;
            let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) else {
                continue;
            };
            let Some(anchor_id) = val
                .get("episode_anchor_id")
                .and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            let Some(task) = val.get("task").and_then(serde_json::Value::as_str) else {
                continue;
            };
            all_logged_tasks.insert(task.to_owned());
            if active_anchors.contains(anchor_id) {
                active_tasks.insert(task.to_owned());
            }
        }

        let Some(bullet) = graph.facts.get(bullet_id) else {
            continue;
        };
        let bullet_ref = bullet.strong_ref();
        let dependent_skills: Vec<FactId> = graph
            .facts
            .values()
            .filter(|s| {
                s.kind == FactKind::SkillVersion
                    && s.origin_library == bullet.origin_library
                    && s.author == bullet.author
                    && serde_json::from_value::<SkillPackage>(s.object.clone())
                        .is_ok_and(|pkg| pkg.references.contains(&bullet_ref))
            })
            .map(|s| s.id.clone())
            .collect();

        for dead_task in all_logged_tasks.difference(&active_tasks) {
            db.execute(
                "DELETE FROM rsi_feedback WHERE skill_fact_id = ?1 AND episode_or_task_id = ?2",
                params![bullet_id.as_str(), dead_task],
            )?;
            for sid in &dependent_skills {
                db.execute(
                    "DELETE FROM rsi_feedback WHERE skill_fact_id = ?1 AND episode_or_task_id = ?2",
                    params![sid.as_str(), dead_task],
                )?;
            }
        }
    }

    let graph = read_graph(db)?;
    let grounded = graph.grounded_support();
    for skill in graph.facts.values().filter(|f| {
        f.kind == FactKind::SkillVersion
            && f.status == FactStatus::Active
            && (touched_conclusions.contains(&f.id)
                || serde_json::from_value::<SkillPackage>(f.object.clone()).is_ok_and(|pkg| {
                    touched_bullets.iter().any(|bid| {
                        graph
                            .facts
                            .get(bid)
                            .is_some_and(|b| pkg.references.contains(&b.strong_ref()))
                    })
                }))
    }) {
        let Ok(pkg) = serde_json::from_value::<SkillPackage>(skill.object.clone()) else {
            continue;
        };
        let Some(bullet) = graph.facts.values().find(|b| {
            b.kind == FactKind::PlaybookBullet
                && b.origin_library == skill.origin_library
                && b.author == skill.author
                && pkg.references.contains(&b.strong_ref())
        }) else {
            continue;
        };
        let still_promotable = bullet.status == FactStatus::Active
            && grounded.contains(&bullet.id)
            && counts(db, bullet)?.promotable();
        if still_promotable {
            let mut surviving_ids = BTreeSet::from([bullet.id.clone()]);
            for j in graph.justifications.values().filter(|j| {
                j.conclusion == bullet.id
                    && j.active
                    && j.premises.iter().all(|p| grounded.contains(&p.fact_id))
            }) {
                for p in &j.premises {
                    surviving_ids.insert(p.fact_id.clone());
                }
            }
            let surviving_facts: Vec<&Fact> = surviving_ids
                .iter()
                .filter_map(|id| graph.facts.get(id))
                .collect();
            if surviving_facts.len() == surviving_ids.len() {
                let refreshed = justification(
                    skill,
                    DerivationMode::RsiAttribution,
                    "RSI_PROMOTION",
                    &surviving_facts,
                )?;
                write_justification(db, &refreshed)?;
            }
        } else {
            db.execute(
                "UPDATE justifications SET active=0, invalidated_at=?1, invalidation_reason='insufficient_surviving_rsi_support'
                 WHERE conclusion_fact_id=?2 AND active=1",
                params![at.0, skill.id.as_str()],
            )?;
        }
    }

    Ok(())
}

#[cfg(test)]
#[path = "rsi_tests.rs"]
mod tests;
