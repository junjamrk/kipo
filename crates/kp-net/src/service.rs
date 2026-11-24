use std::collections::BTreeSet;

use kp_model::{
    ActorId, ContentHash, EpochMs, Fact, FactAccess, FactStatus, KpError, LibraryId, LibraryMode,
    MemberRole, SecretRedactor, StrongRef,
};
use kp_retrieve::{search, SearchOptions};
use kp_store::{InitialPublishWrite, Store};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::policy::{authorize, Action, Decision, Principal};
use crate::{NetError, Result};

#[derive(Clone, Serialize, Deserialize)]
pub struct PublishRequest {
    pub fact: Fact,
    #[serde(default)]
    pub entities: BTreeSet<String>,
    pub idempotency_key: String,
    pub origin_reference: StrongRef,
    #[serde(default)]
    pub audience_revision: u64,
    #[serde(default)]
    pub skill_audience: Option<kp_model::AudienceSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub concepts: Vec<kp_model::CommunityConcept>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_scope: Option<kp_model::SourceScope>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommunityChangeFeed {
    #[serde(flatten)]
    pub snapshot: kp_store::ChangeSnapshot,
    #[serde(default)]
    pub concepts: Vec<kp_model::CommunityConcept>,
    #[serde(default)]
    pub source_scopes: Vec<kp_model::SourceScope>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<ActorId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authenticated_actor: Option<ActorId>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct TargetRequest {
    pub fact_id: String,
    #[serde(default)]
    pub origin_library: Option<LibraryId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_key: Option<kp_model::LogicalKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_sha256: Option<ContentHash>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PurgeRequest {
    Fact(TargetRequest),
    Tombstone(kp_store::Tombstone),
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SynthesizeRequest {
    pub query: String,
    pub target_audience: BTreeSet<ActorId>,
    pub at: Option<EpochMs>,
}

#[derive(Clone, Deserialize)]
pub(crate) struct SearchRequest {
    pub q: String,
    #[serde(default)]
    pub deep: bool,
    pub at: Option<EpochMs>,
    pub limit: Option<usize>,
}

pub(crate) enum Operation {
    Status(EpochMs),
    Publish(Box<PublishRequest>),
    UpsertScope(kp_model::SourceScope),
    Withdraw(TargetRequest),
    Purge(PurgeRequest),
    Search(SearchRequest),
    Why(String, EpochMs),
    Synthesize(SynthesizeRequest),
    Changes { at: EpochMs, since: Option<String> },
    Verify(StrongRef),
}

impl Operation {
    pub(crate) fn is_readonly(&self) -> bool {
        matches!(
            self,
            Self::Status(_) | Self::Search(_) | Self::Why(_, _) | Self::Verify(_)
        )
    }
}

pub(crate) async fn dispatch_readonly(
    store: &Store,
    principal: &Principal,
    operation: Operation,
    now: EpochMs,
) -> Result<Value> {
    let mode = store.status()?.mode;
    let check = |action: Action, author: Option<&ActorId>| {
        if authorize(Some(principal), mode, action, author) == Decision::Allow {
            Ok(())
        } else {
            Err(NetError::Forbidden)
        }
    };
    match operation {
        Operation::Status(at) => {
            check(Action::Read, None)?;
            let status = store.status()?;
            let chunks = store.exchange_visible_chunk_count(&principal.actor)?;
            let facts = store.visible_facts(&principal.actor, at)?.len();
            Ok(json!({
                "library_id": status.library_id,
                "name": status.name,
                "mode": status.mode,
                "actor": principal.actor,
                "schema_ir_hash": status.schema_ir_hash,
                "facts": facts,
                "chunks": chunks
            }))
        }
        Operation::Search(request) => {
            check(Action::Read, None)?;
            let output = search(
                store,
                &SearchOptions {
                    query: request.q,
                    viewer: principal.actor.clone(),
                    at: request.at.unwrap_or(now),
                    limit: request.limit.unwrap_or(10).min(1000),
                    deep: request.deep,
                    max_hops: 2,
                },
                &kp_retrieve::judge::OffJudge,
            )
            .await?;
            Ok(serde_json::to_value(output)?)
        }
        Operation::Why(id, at) => {
            check(Action::Read, None)?;
            let id = store
                .resolve_visible_or_raw_fact_id(&principal.actor, at, &id)
                .map_err(|_| NetError::NotFound)?;
            let proof =
                kp_derive::why::why(store, &id, &principal.actor, at)?.ok_or(NetError::NotFound)?;
            Ok(serde_json::to_value(proof)?)
        }
        Operation::Verify(reference) => {
            check(Action::Read, None)?;
            let confirmed = store
                .visible_facts(&principal.actor, now)?
                .iter()
                .any(|f| f.strong_ref() == reference);
            Ok(json!({"confirmed":confirmed}))
        }
        _ => Err(NetError::Invalid("mutating operation requires write lock")),
    }
}

pub(crate) async fn dispatch(
    store: &mut Store,
    principal: &Principal,
    operation: Operation,
    now: EpochMs,
) -> Result<Value> {
    if operation.is_readonly() {
        return dispatch_readonly(store, principal, operation, now).await;
    }
    let mode = store.status()?.mode;
    let check = |action: Action, author: Option<&ActorId>| {
        if authorize(Some(principal), mode, action, author) == Decision::Allow {
            Ok(())
        } else {
            Err(NetError::Forbidden)
        }
    };
    match operation {
        Operation::UpsertScope(mut scope) => {
            check(Action::ManageMembers, None)?;
            if matches!(
                scope.current_audience,
                kp_model::AudienceSpec::SemiringDerived
            ) {
                return Err(NetError::Invalid("source scope cannot use semiring marker"));
            }
            let redactor = SecretRedactor::new().map_err(|_| NetError::Unavailable)?;
            scope.label = redactor.redact(&scope.label);
            store.upsert_source_scope(&scope)?;
            kp_derive::derive(store)?;
            Ok(json!({"scope_id":scope.scope_id,"ok":true}))
        }
        Operation::Publish(request) => {
            let publisher = request
                .fact
                .ingested_by
                .as_ref()
                .unwrap_or(&request.fact.author);
            check(Action::Publish, Some(publisher))?;
            let mut fact = request.fact;
            let skill = fact.kind == kp_model::FactKind::SkillVersion;
            if !(matches!(fact.access, FactAccess::Base { .. }) || skill)
                || fact.status != FactStatus::Active
                || fact.access.expired_at().is_some()
            {
                return Err(NetError::Invalid("publish requires an active base claim"));
            }
            if request.idempotency_key.is_empty() {
                return Err(NetError::Invalid("idempotency key required"));
            }
            let redactor = SecretRedactor::new().map_err(|_| NetError::Unavailable)?;
            redact_fact(&mut fact, &redactor)?;
            validate_publish_scope(
                store,
                &PublishScopeCheck {
                    principal,
                    mode,
                    fact: &fact,
                    skill_audience: request.skill_audience.as_ref(),
                    incoming_scope: request.source_scope.as_ref(),
                    redactor: &redactor,
                    skill,
                },
            )?;
            let requested_audience = if skill {
                request.skill_audience.clone().ok_or(NetError::Invalid(
                    "skill publication requires an explicit audience",
                ))?
            } else {
                match &fact.access {
                    FactAccess::Base { audience, .. } => audience.clone(),
                    FactAccess::Derived { .. } => {
                        return Err(NetError::Invalid("publish requires an active base claim"));
                    }
                }
            };
            if mode == LibraryMode::Community {
                let moderator = is_moderator(principal);
                let origin_owner = fact.ingested_by.as_ref().unwrap_or(&fact.author);
                if fact.origin_library == *store.library_id() {
                    if !moderator {
                        return Err(NetError::Forbidden);
                    }
                } else if !store.bind_origin_owner(&fact.origin_library, origin_owner)?
                    && !moderator
                {
                    return Err(NetError::Forbidden);
                }
            }
            fact.id = format!(
                "published-{}",
                ContentHash::digest(serde_json::to_vec(&fact.strong_ref())?)
            )
            .parse()?;
            if store.is_tombstoned(&fact.origin_library, &fact.logical_key)? {
                return Err(NetError::NotFound);
            }
            let pub_owner = fact.ingested_by.as_ref().unwrap_or(&fact.author);
            let pub_sref = fact.strong_ref();
            let redacted_origin_lkey: kp_model::LogicalKey = redactor
                .redact(request.origin_reference.logical_key.as_str())
                .parse()?;
            if store.is_version_withdrawn_for_publisher(
                &pub_sref.origin_library,
                &pub_sref.logical_key,
                &pub_sref.content_sha256,
                pub_owner,
            )? || store.is_version_withdrawn_for_publisher(
                &request.origin_reference.origin_library,
                &redacted_origin_lkey,
                &request.origin_reference.content_sha256,
                pub_owner,
            )? {
                return Err(NetError::NotFound);
            }
            let revision = request.audience_revision.max(1);
            if let Some(existing) = store.derivation_view()?.facts.get(&fact.id).cloned() {
                let existing_publisher = existing.ingested_by.as_ref().unwrap_or(&existing.author);
                check(Action::Publish, Some(existing_publisher))?;
                if existing.strong_ref() != fact.strong_ref() {
                    return Err(NetError::Invalid("immutable claim mismatch"));
                }
                if existing.status != FactStatus::Active || existing.access.expired_at().is_some() {
                    return Err(NetError::NotFound);
                }
                let revised = store.exchange_revise_published_audience(
                    &existing.id,
                    &requested_audience,
                    revision,
                    skill,
                    now,
                )?;
                sync_published_concepts(
                    store,
                    principal,
                    &existing.id,
                    &request.concepts,
                    &redactor,
                    now,
                )?;
                if revised {
                    kp_derive::derive(store)?;
                    if skill {
                        store.sync_skills_now()?;
                    }
                }
                return Ok(json!({"fact_id":existing.id,"reference":existing.strong_ref()}));
            }
            let pub_owner = fact
                .ingested_by
                .clone()
                .unwrap_or_else(|| fact.author.clone());
            let entities = request
                .entities
                .iter()
                .map(|v| redactor.redact(v))
                .collect();
            let mut origin_reference = request.origin_reference;
            origin_reference.logical_key = redactor
                .redact(origin_reference.logical_key.as_str())
                .parse()?;
            let allow_overwrite = is_moderator(principal);
            let sanitized_concepts: Vec<(kp_model::CommunityConcept, bool)> = request
                .concepts
                .iter()
                .map(|c| {
                    let mut sanitized = c.clone();
                    sanitized.canonical_label = redactor.redact(&sanitized.canonical_label);
                    sanitized.domain = redactor.redact(&sanitized.domain);
                    (sanitized, allow_overwrite)
                })
                .collect();
            let resp_id = fact.id.clone();
            let resp_ref = fact.strong_ref();
            let write = if skill {
                let package: kp_model::SkillPackage = serde_json::from_value(fact.object.clone())?;
                package.validate()?;
                if !matches!(fact.access, FactAccess::Derived { .. }) {
                    return Err(NetError::Invalid(
                        "skill publication requires derived provenance",
                    ));
                }
                let mut anchor = fact.clone();
                anchor.id = format!("support-{}", fact.id).parse()?;
                anchor.kind = kp_model::FactKind::Observation;
                anchor.subject = "Published skill attestation".into();
                anchor.predicate = "kp.rsi.publication".into();
                anchor.object = json!({"reference":origin_reference});
                anchor.access = FactAccess::Base {
                    audience: requested_audience.clone(),
                    stamp: kp_model::BitemporalStamp {
                        valid: kp_model::ValidityWindow {
                            valid_from: now,
                            valid_until: EpochMs::OPEN_END,
                        },
                        recorded_at: now,
                        expired_at: None,
                    },
                };
                let proof = kp_model::justification(
                    &fact,
                    kp_model::DerivationMode::RsiAttribution,
                    "RSI_PUBLICATION",
                    &[&anchor],
                )?;
                InitialPublishWrite::Skill {
                    anchor: Box::new(anchor),
                    skill_fact: Box::new(fact),
                    proof: Box::new(proof),
                }
            } else {
                InitialPublishWrite::Base {
                    fact: Box::new(fact),
                    entities,
                }
            };
            store.exchange_accept_initial_publish(
                write,
                &kp_store::AcceptInitialPublishSpec {
                    pub_owner: &pub_owner,
                    pub_sref: &pub_sref,
                    redacted_origin_ref: &origin_reference,
                    requested_audience: &requested_audience,
                    revision,
                    concepts: &sanitized_concepts,
                    now,
                },
            )?;
            kp_derive::derive(store)?;
            Ok(json!({"fact_id":resp_id,"reference":resp_ref}))
        }
        Operation::Withdraw(target) => mutate(store, principal, target, Action::Withdraw, now),
        Operation::Purge(PurgeRequest::Fact(target)) => {
            mutate(store, principal, target, Action::Purge, now)
        }
        Operation::Purge(PurgeRequest::Tombstone(tombstone)) => {
            // Authorize the origin before looking up the opaque key, including on retries.
            if !is_moderator(principal)
                && (tombstone.origin_library_id == *store.library_id()
                    || !store.owns_origin(&tombstone.origin_library_id, &principal.actor)?)
            {
                return Err(NetError::Forbidden);
            }
            preserve_purge_receipts(store, now)?;
            store.apply_tombstone(&tombstone)?;
            Ok(json!({"ok":true}))
        }
        Operation::Synthesize(request) => {
            check(Action::Synthesize, None)?;
            if !request.target_audience.contains(&principal.actor) {
                return Err(NetError::Forbidden);
            }
            let ids = kp_derive::synthesize_scoped(
                store,
                &principal.actor,
                &request.target_audience,
                &request.query,
                request.at.unwrap_or(now),
                &kp_derive::StructuredSynthesizer,
            )
            .await?;
            Ok(json!({"facts":ids}))
        }
        Operation::Changes { at, since } => {
            check(Action::Read, None)?;
            let members = if mode == LibraryMode::Community {
                store.derivation_view()?.members.into_iter().collect()
            } else {
                Vec::new()
            };
            let (snapshot, concepts, source_scopes) = store.change_snapshot_with_governance_since(
                &principal.actor,
                at,
                since.as_deref(),
            )?;
            Ok(serde_json::to_value(CommunityChangeFeed {
                snapshot,
                concepts,
                source_scopes,
                members,
                authenticated_actor: Some(principal.actor.clone()),
            })?)
        }
        Operation::Status(_)
        | Operation::Search(_)
        | Operation::Why(_, _)
        | Operation::Verify(_) => dispatch_readonly(store, principal, operation, now).await,
    }
}

fn mutate(
    store: &mut Store,
    principal: &Principal,
    target: TargetRequest,
    action: Action,
    now: EpochMs,
) -> Result<Value> {
    let author_scope = if is_moderator(principal) && target.origin_library.is_some() {
        None
    } else {
        Some(&principal.actor)
    };
    let Some(fact) = store.resolve_fact_target(
        &target.fact_id,
        author_scope,
        target.origin_library.as_ref(),
    )?
    else {
        if action == Action::Withdraw {
            if let (Some(origin), Some(lkey), Some(sha)) = (
                target.origin_library.as_ref(),
                target.logical_key.as_ref(),
                target.content_sha256.as_ref(),
            ) {
                let mode = store.status()?.mode;
                if authorize(
                    Some(principal),
                    mode,
                    Action::Withdraw,
                    Some(&principal.actor),
                ) != Decision::Allow
                {
                    return Err(NetError::Forbidden);
                }
                if mode == LibraryMode::Community
                    && !is_moderator(principal)
                    && (origin == store.library_id()
                        || store.origin_has_other_owner(origin, &principal.actor)?)
                {
                    return Err(NetError::Forbidden);
                }
                let redactor = SecretRedactor::new().map_err(|_| NetError::Unavailable)?;
                let clean_lkey: kp_model::LogicalKey = redactor
                    .redact(lkey.as_str())
                    .parse()
                    .unwrap_or_else(|_| lkey.clone());
                store.record_withdrawn_version(origin, &clean_lkey, sha, &principal.actor, now)?;
                if clean_lkey != *lkey {
                    store.record_withdrawn_version(origin, lkey, sha, &principal.actor, now)?;
                }
                return Ok(json!({"ok":true}));
            }
        }
        return Err(NetError::NotFound);
    };
    let mode = store.status()?.mode;
    let graph = store.derivation_view()?;
    let targets = graph.facts.values().filter(|version| {
        version.id == fact.id
            || (action == Action::Purge
                && version.origin_library == fact.origin_library
                && version.logical_key == fact.logical_key)
    });
    for version in targets {
        let author_allowed =
            authorize(Some(principal), mode, action, Some(&version.author)) == Decision::Allow;
        let ingester_allowed = version.ingested_by.as_ref().is_some_and(|ing| {
            authorize(Some(principal), mode, action, Some(ing)) == Decision::Allow
        });
        if !author_allowed && !ingester_allowed {
            return Err(
                if graph
                    .evaluate(&principal.actor, now)
                    .facts
                    .contains(&version.id)
                {
                    NetError::Forbidden
                } else {
                    NetError::NotFound
                },
            );
        }
    }
    match action {
        Action::Withdraw => {
            store.forget_fact_id(&fact.id, &principal.actor, now)?;
        }
        Action::Purge => {
            preserve_purge_receipts(store, now)?;
            store.purge(&fact.origin_library, &fact.logical_key, now, &[])?;
        }
        Action::Read
        | Action::Publish
        | Action::Synthesize
        | Action::AdoptSkill
        | Action::ManageMembers => return Err(NetError::Invalid("invalid mutation")),
    }
    Ok(json!({"ok":true}))
}

fn is_moderator(principal: &Principal) -> bool {
    matches!(principal.role, MemberRole::Owner | MemberRole::Maintainer)
}

struct PublishScopeCheck<'a> {
    principal: &'a Principal,
    mode: LibraryMode,
    fact: &'a Fact,
    skill_audience: Option<&'a kp_model::AudienceSpec>,
    incoming_scope: Option<&'a kp_model::SourceScope>,
    redactor: &'a SecretRedactor,
    skill: bool,
}

fn validate_publish_scope(store: &mut Store, check: &PublishScopeCheck<'_>) -> Result<()> {
    let PublishScopeCheck {
        principal,
        mode,
        fact,
        skill_audience,
        incoming_scope,
        redactor,
        skill,
    } = *check;
    let requested_audience = if skill {
        skill_audience.ok_or(NetError::Invalid(
            "skill publication requires an explicit audience",
        ))?
    } else {
        match &fact.access {
            FactAccess::Base { audience, .. } => audience,
            FactAccess::Derived { .. } => {
                return Err(NetError::Invalid("publish requires an active base claim"));
            }
        }
    };
    let cross_speaker = fact
        .ingested_by
        .as_ref()
        .is_some_and(|ing| ing != &fact.author);
    if cross_speaker && fact.source_scope_id.is_none() {
        return Err(NetError::Forbidden);
    }
    let Some(scope_id) = &fact.source_scope_id else {
        return Ok(());
    };
    let graph = store.derivation_view()?;
    if let Some(live_scope) = store.source_scope(scope_id)? {
        live_scope
            .validate_publish_audience(requested_audience, &graph.members)
            .map_err(|_| NetError::Forbidden)?;
        if let Some(ingester) = fact.ingested_by.as_ref().filter(|i| *i != &fact.author) {
            if !live_scope.allows(&fact.author, &graph.members)
                || !live_scope.allows(ingester, &graph.members)
            {
                return Err(NetError::Forbidden);
            }
        }
        return Ok(());
    }
    let Some(incoming) = incoming_scope.filter(|s| s.scope_id == *scope_id) else {
        return Err(NetError::Forbidden);
    };
    // On a Community library without a moderator-registered `SourceScope`, an unprivileged member
    // cannot self-register an `AllMembers` container or forge cross-speaker attribution (`author != caller`).
    if mode == LibraryMode::Community
        && !is_moderator(principal)
        && (fact.author != principal.actor
            || matches!(
                incoming.current_audience,
                kp_model::AudienceSpec::AllMembers
            ))
    {
        return Err(NetError::Forbidden);
    }
    let mut sanitized = incoming.clone();
    sanitized.label = redactor.redact(&sanitized.label);
    if !sanitized.allows(&principal.actor, &graph.members) && !is_moderator(principal) {
        return Err(NetError::Forbidden);
    }
    sanitized
        .validate_publish_audience(requested_audience, &graph.members)
        .map_err(|_| NetError::Forbidden)?;
    if let Some(ingester) = fact.ingested_by.as_ref().filter(|i| *i != &fact.author) {
        if !sanitized.allows(&fact.author, &graph.members)
            || !sanitized.allows(ingester, &graph.members)
        {
            return Err(NetError::Forbidden);
        }
    }
    store.upsert_source_scope(&sanitized)?;
    Ok(())
}

fn sync_published_concepts(
    store: &mut Store,
    principal: &Principal,
    fact_id: &kp_model::FactId,
    concepts: &[kp_model::CommunityConcept],
    redactor: &SecretRedactor,
    now: EpochMs,
) -> Result<()> {
    if concepts.is_empty() {
        return Ok(());
    }
    let allow_overwrite = is_moderator(principal);
    let mut ids = Vec::with_capacity(concepts.len());
    for concept in concepts {
        let mut sanitized = concept.clone();
        sanitized.canonical_label = redactor.redact(&sanitized.canonical_label);
        sanitized.domain = redactor.redact(&sanitized.domain);
        store.exchange_upsert_concept(&sanitized, allow_overwrite)?;
        ids.push(sanitized.id);
    }
    store.exchange_link_fact_concepts(fact_id, &ids, now)?;
    Ok(())
}

fn preserve_purge_receipts(store: &mut Store, now: EpochMs) -> Result<()> {
    let graph = store.derivation_view()?;
    let origins: BTreeSet<_> = graph
        .facts
        .values()
        .filter(|f| f.origin_library != *store.library_id())
        .map(|f| {
            (
                &f.origin_library,
                f.ingested_by.as_ref().unwrap_or(&f.author),
            )
        })
        .collect();
    for (origin, owner) in origins {
        // Adopt legacy origin ownership before deleting the last fact that established it.
        store.bind_origin_owner(origin, owner)?;
    }
    for actor in graph.members {
        store.change_snapshot(&actor, now)?;
    }
    Ok(())
}

fn redact_governed_scope(
    scope: &kp_model::GovernedScope,
    redactor: &SecretRedactor,
) -> kp_model::GovernedScope {
    match scope {
        kp_model::GovernedScope::Resource { target } => kp_model::GovernedScope::Resource {
            target: redactor.redact(target),
        },
        other => other.clone(),
    }
}

pub(crate) fn redact_fact(fact: &mut Fact, redactor: &SecretRedactor) -> Result<()> {
    fact.subject = redactor.redact(&fact.subject);
    fact.predicate = redactor.redact(&fact.predicate);
    fact.object = redactor.redact_json(&fact.object);
    fact.tags = fact.tags.iter().map(|s| redactor.redact(s)).collect();
    fact.logical_key = redactor.redact(fact.logical_key.as_str()).parse()?;
    fact.governed_scope = fact
        .governed_scope
        .as_ref()
        .map(|s| redact_governed_scope(s, redactor));
    fact.broader_scope = fact
        .broader_scope
        .as_ref()
        .map(|b| kp_model::BroaderScopeCandidate {
            narrow_active: redact_governed_scope(&b.narrow_active, redactor),
            broader_candidate: redact_governed_scope(&b.broader_candidate, redactor),
            question: redactor.redact(&b.question),
        });
    fact.source_anchor = None;
    fact.unverified_gaps = fact
        .unverified_gaps
        .iter()
        .map(|s| redactor.redact(s))
        .collect();
    fact.validate()
        .map_err(|_: KpError| NetError::Invalid("invalid published claim"))?;
    Ok(())
}
