use std::collections::{BTreeMap, BTreeSet};

use kp_model::{
    ActorId, AudienceSpec, ContentHash, EpochMs, Fact, FactAccess, FactId, FactKind, FactStatus,
    GovernedScope, LibraryId, LogicalKey, MembershipRelation, StrongRef,
};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};

use super::snapshot_sync::mark_alignment_dirty_tx;
use crate::facts::read_graph;
use crate::{Result, Store, StoreError};

impl Store {
    /// Recomputes `AudienceSpec::Explicit` for active locally-authored base `Constraint`s governed by
    /// `GovernedScope::RelationGroup` (`broader_scope.is_none()`), bounded by `source_scope` ceiling
    /// when `source_scope_id` is set. Returns the number of constraints whose audience changed.
    pub fn reconcile_relation_group_audiences(
        &mut self,
        entails: impl Fn(&kp_model::ConceptId, &kp_model::ConceptId) -> bool,
        at: EpochMs,
    ) -> Result<usize> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let graph = read_graph(&tx)?;
        let mut memberships = Vec::new();
        for f in graph.facts.values() {
            if f.status == FactStatus::Active
                && f.access.expired_at().is_none()
                && f.kind == FactKind::Membership
            {
                if let Ok(rel) = serde_json::from_value::<MembershipRelation>(f.object.clone()) {
                    if f.author == rel.anchor {
                        memberships.push(rel);
                    }
                }
            }
        }

        let mut updated = 0_usize;
        for fact in graph.facts.values() {
            if fact.origin_library != self.library_id
                || fact.status != FactStatus::Active
                || fact.kind != FactKind::Constraint
                || fact.broader_scope.is_some()
            {
                continue;
            }
            let FactAccess::Base {
                audience: current_audience,
                stamp,
            } = &fact.access
            else {
                continue;
            };
            if stamp.expired_at.is_some() || !matches!(current_audience, AudienceSpec::Explicit(_))
            {
                continue;
            }
            let Some(GovernedScope::RelationGroup {
                anchor,
                group_concept_id,
            }) = &fact.governed_scope
            else {
                continue;
            };

            let mut recomputed = BTreeSet::from([fact.author.clone()]);
            if graph.members.contains(anchor) {
                recomputed.insert(anchor.clone());
            }
            for rel in &memberships {
                if &rel.anchor == anchor
                    && graph.members.contains(&rel.member)
                    && (rel.group_concept_id == *group_concept_id
                        || entails(&rel.group_concept_id, group_concept_id))
                {
                    recomputed.insert(rel.member.clone());
                }
            }

            if let Some(sid) = &fact.source_scope_id {
                match graph.source_scopes.get(sid) {
                    Some(AudienceSpec::AllMembers) => {
                        recomputed.retain(|a| graph.members.contains(a) || a == &fact.author);
                    }
                    Some(AudienceSpec::Explicit(scope_actors)) => {
                        let ceiling: BTreeSet<_> =
                            scope_actors.intersection(&graph.members).cloned().collect();
                        recomputed.retain(|a| ceiling.contains(a));
                    }
                    Some(AudienceSpec::SemiringDerived) | None => {
                        if let AudienceSpec::Explicit(cur) = current_audience {
                            recomputed.retain(|a| cur.contains(a));
                        }
                    }
                }
            }

            let next_audience = AudienceSpec::Explicit(recomputed);
            if &next_audience != current_audience {
                tx.execute(
                    "UPDATE facts SET audience_json=?1 WHERE fact_id=?2",
                    params![serde_json::to_string(&next_audience)?, fact.id.as_str()],
                )?;
                cancel_unrevised_self_only_pending_publishes_tx(
                    &tx,
                    &fact.id,
                    &fact.origin_library,
                    &fact.logical_key,
                    &fact.author,
                    &next_audience,
                    &graph.members,
                )?;
                updated += 1;
            }
        }

        if updated > 0 {
            mark_alignment_dirty_tx(&tx, at)?;
        }
        tx.commit()?;
        Ok(updated)
    }

    /// Lists unpublished active multi-actor local base facts PLUS already-published active local base
    /// facts whose current `audience` differs from `last_published_audience` for `destination`
    /// (including when narrowed down to `Explicit({author})`). Never-published self-only facts never
    /// appear here and never leave the local machine.
    pub fn unshared_multi_actor_facts(&self, destination: &str) -> Result<Vec<FactId>> {
        let graph = self.derivation_view()?;
        let mut published_by_identity: BTreeMap<(String, String), (Option<AudienceSpec>, u64)> =
            BTreeMap::new();
        {
            let mut stmt = self.db.prepare(
                "SELECT logical_key, content_sha256, payload_json FROM change_log
                 WHERE op='publication' AND origin_library_id=?1
                   AND logical_key IS NOT NULL AND content_sha256 IS NOT NULL
                 ORDER BY seq ASC",
            )?;
            let mut rows = stmt.query([self.library_id.as_str()])?;
            while let Some(row) = rows.next()? {
                let lkey: String = row.get(0)?;
                let sha: String = row.get(1)?;
                let raw: String = row.get(2)?;
                let (dest, aud, rev) = parse_publication_payload(&raw)?;
                if dest == destination {
                    let rev_num = rev.unwrap_or(1);
                    let entry = published_by_identity
                        .entry((lkey, sha))
                        .or_insert((None, 0));
                    if rev_num >= entry.1 {
                        *entry = (aud, rev_num);
                    }
                }
            }
        }

        let mut pending_keys = BTreeSet::new();
        let mut pending_audiences: BTreeMap<(String, String), AudienceSpec> = BTreeMap::new();
        {
            let mut stmt = self.db.prepare(
                "SELECT idempotency_key, payload_json FROM outbox
                 WHERE dest_community_url=?1 AND event_kind='publish' AND delivered_at IS NULL
                 ORDER BY outbox_id ASC",
            )?;
            let mut rows = stmt.query([destination])?;
            while let Some(row) = rows.next()? {
                pending_keys.insert(row.get::<_, String>(0)?);
                let raw: String = row.get(1)?;
                if let Ok(val) = serde_json::from_str::<Value>(&raw) {
                    if let (Some(sref), Some(aud)) = (
                        val.get("origin_reference")
                            .cloned()
                            .and_then(|v| serde_json::from_value::<StrongRef>(v).ok()),
                        val.get("fact")
                            .and_then(|f| f.get("access"))
                            .and_then(|a| a.get("audience"))
                            .cloned()
                            .and_then(|v| serde_json::from_value::<AudienceSpec>(v).ok()),
                    ) {
                        if sref.origin_library == self.library_id {
                            pending_audiences.insert(
                                (
                                    sref.logical_key.to_string(),
                                    sref.content_sha256.to_string(),
                                ),
                                aud,
                            );
                        }
                    }
                }
            }
        }

        let mut candidates = Vec::new();
        for fact in graph.facts.values() {
            if fact.origin_library != self.library_id
                || fact.status != FactStatus::Active
                || fact.kind == FactKind::SkillVersion
            {
                continue;
            }
            let FactAccess::Base { audience, stamp } = &fact.access else {
                continue;
            };
            if stamp.expired_at.is_some() {
                continue;
            }
            let (effective_aud, _) = match cap_audience_by_live_scope_tx(
                &self.db,
                &ScopeCapInput {
                    mode: self.mode,
                    raw_aud: audience,
                    author: &fact.author,
                    ingester: fact.ingested_by.as_ref(),
                    scope_id: fact.source_scope_id.as_ref(),
                    members: &graph.members,
                    strict_validate: false,
                },
            ) {
                Ok(pair) => pair,
                Err(StoreError::PermissionDenied) => continue,
                Err(err) => return Err(err),
            };
            let sref = fact.strong_ref();
            let id_key = (
                sref.logical_key.to_string(),
                sref.content_sha256.to_string(),
            );

            if let Some((last_pub_aud, _)) = published_by_identity.get(&id_key) {
                let changed = match last_pub_aud {
                    Some(prev_aud) => prev_aud != &effective_aud,
                    None => false,
                };
                if !changed {
                    continue;
                }
                if pending_audiences.get(&id_key) == Some(&effective_aud) {
                    continue;
                }
                candidates.push(fact.id.clone());
                continue;
            }

            if !audience_has_other_viewer(&effective_aud, &fact.author, &graph.members) {
                continue;
            }
            if pending_audiences.get(&id_key) == Some(&effective_aud) {
                continue;
            }
            let idem =
                ContentHash::digest(serde_json::to_vec(&(destination, &fact.author, &sref))?)
                    .to_string();
            if !pending_audiences.contains_key(&id_key) && pending_keys.contains(&idem) {
                continue;
            }
            candidates.push(fact.id.clone());
        }
        Ok(candidates)
    }

    /// Records a version-specific withdrawal barrier `(origin_library, logical_key, content_sha256)`
    /// in `change_log(op='withdrawn_version')` so an out-of-order delayed `publish` of that exact
    /// withdrawn `StrongRef` is rejected without tombstoning future re-learned versions.
    pub fn record_withdrawn_version(
        &mut self,
        origin_library: &LibraryId,
        logical_key: &LogicalKey,
        content_sha256: &ContentHash,
        actor: &ActorId,
        at: EpochMs,
    ) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        record_withdrawn_version_tx(&tx, origin_library, logical_key, content_sha256, actor, at)?;
        tx.commit()?;
        Ok(())
    }

    /// Checks whether the exact `(origin_library, logical_key, content_sha256)` claim version has
    /// already been withdrawn or superseded for `publisher` (honoring `withdrawn_version` barriers
    /// recorded by `publisher` or an active community moderator).
    pub fn is_version_withdrawn_for_publisher(
        &self,
        origin_library: &LibraryId,
        logical_key: &LogicalKey,
        content_sha256: &ContentHash,
        publisher: &ActorId,
    ) -> Result<bool> {
        Ok(self.db.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM change_log c
                 WHERE c.op='withdrawn_version'
                   AND c.origin_library_id=?1
                   AND c.logical_key=?2
                   AND c.content_sha256=?3
                   AND (
                       c.actor_id IS NULL
                       OR c.actor_id=?4
                       OR EXISTS (
                           SELECT 1 FROM members_tokens m
                           WHERE m.actor_id=c.actor_id
                             AND m.role IN ('owner','maintainer')
                             AND m.revoked_at IS NULL
                       )
                   )
             ) OR EXISTS(
                 SELECT 1 FROM facts
                 WHERE origin_library_id=?1
                   AND logical_key=?2
                   AND content_sha256=?3
                   AND (status IN ('withdrawn', 'superseded') OR expired_at IS NOT NULL)
             )",
            params![
                origin_library.as_str(),
                logical_key.as_str(),
                content_sha256.as_str(),
                publisher.as_str()
            ],
            |r| r.get(0),
        )?)
    }

    /// Checks whether the exact `(origin_library, logical_key, content_sha256)` claim version has
    /// already been withdrawn or superseded.
    pub fn is_version_withdrawn(
        &self,
        origin_library: &LibraryId,
        logical_key: &LogicalKey,
        content_sha256: &ContentHash,
    ) -> Result<bool> {
        Ok(self.db.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM change_log
                 WHERE op='withdrawn_version'
                   AND origin_library_id=?1
                   AND logical_key=?2
                   AND content_sha256=?3
             ) OR EXISTS(
                 SELECT 1 FROM facts
                 WHERE origin_library_id=?1
                   AND logical_key=?2
                   AND content_sha256=?3
                   AND (status IN ('withdrawn', 'superseded') OR expired_at IS NOT NULL)
             )",
            params![
                origin_library.as_str(),
                logical_key.as_str(),
                content_sha256.as_str()
            ],
            |r| r.get(0),
        )?)
    }
}

pub(crate) fn record_withdrawn_version_tx(
    db: &rusqlite::Connection,
    origin_library: &LibraryId,
    logical_key: &LogicalKey,
    content_sha256: &ContentHash,
    actor: &ActorId,
    at: EpochMs,
) -> Result<()> {
    db.execute(
        "INSERT INTO change_log (op,origin_library_id,logical_key,content_sha256,actor_id,created_at)
         SELECT 'withdrawn_version',?1,?2,?3,?4,?5
         WHERE NOT EXISTS (
             SELECT 1 FROM change_log
             WHERE op='withdrawn_version'
               AND origin_library_id=?1
               AND logical_key=?2
               AND content_sha256=?3
               AND actor_id=?4
         )",
        params![
            origin_library.as_str(),
            logical_key.as_str(),
            content_sha256.as_str(),
            actor.as_str(),
            at.0
        ],
    )?;
    Ok(())
}

pub(super) fn read_active_members_tx(db: &rusqlite::Connection) -> Result<BTreeSet<ActorId>> {
    let mut stmt = db.prepare(
        "SELECT actor_id FROM members_tokens WHERE revoked_at IS NULL ORDER BY actor_id",
    )?;
    let mut rows = stmt.query([])?;
    let mut members = BTreeSet::new();
    while let Some(row) = rows.next()? {
        members.insert(row.get::<_, String>(0)?.parse()?);
    }
    Ok(members)
}

pub(super) fn audience_has_other_viewer(
    audience: &AudienceSpec,
    author: &ActorId,
    members: &BTreeSet<ActorId>,
) -> bool {
    match audience {
        AudienceSpec::AllMembers => members.iter().any(|m| m != author),
        AudienceSpec::Explicit(actors) => actors.iter().any(|m| m != author),
        AudienceSpec::SemiringDerived => false,
    }
}

pub(super) fn has_publication_receipt_for_dest_tx(
    db: &rusqlite::Connection,
    destination: &str,
    origin_library: &LibraryId,
    logical_key: &LogicalKey,
) -> Result<bool> {
    let mut stmt = db.prepare(
        "SELECT payload_json FROM change_log
         WHERE op='publication' AND origin_library_id=?1 AND logical_key=?2",
    )?;
    let mut rows = stmt.query(params![origin_library.as_str(), logical_key.as_str()])?;
    while let Some(row) = rows.next()? {
        let raw: String = row.get(0)?;
        if parse_publication_payload(&raw)?.0 == destination {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn cancel_unrevised_self_only_pending_publishes_tx(
    tx: &rusqlite::Connection,
    fact_id: &FactId,
    origin_library: &LibraryId,
    logical_key: &LogicalKey,
    author: &ActorId,
    new_audience: &AudienceSpec,
    members: &BTreeSet<ActorId>,
) -> Result<()> {
    if audience_has_other_viewer(new_audience, author, members) {
        return Ok(());
    }
    let mut published_dests = BTreeSet::new();
    {
        let mut stmt = tx.prepare(
            "SELECT payload_json FROM change_log
             WHERE op='publication' AND origin_library_id=?1 AND logical_key=?2",
        )?;
        let mut rows = stmt.query(params![origin_library.as_str(), logical_key.as_str()])?;
        while let Some(row) = rows.next()? {
            let raw: String = row.get(0)?;
            published_dests.insert(parse_publication_payload(&raw)?.0);
        }
    }
    let stale_rows = {
        let mut stmt = tx.prepare(
            "SELECT outbox_id, dest_community_url, payload_json FROM outbox
             WHERE event_kind='publish' AND delivered_at IS NULL",
        )?;
        let mut rows = stmt.query([])?;
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let dest: String = row.get(1)?;
            if published_dests.contains(&dest) {
                continue;
            }
            let raw: String = row.get(2)?;
            if let Ok(val) = serde_json::from_str::<Value>(&raw) {
                let matches_fid = val
                    .get("fact")
                    .and_then(|f| f.get("id"))
                    .and_then(Value::as_str)
                    == Some(fact_id.as_str());
                if matches_fid || outbox_publish_matches_identity(&val, origin_library, logical_key)
                {
                    items.push((id, dest, val));
                }
            }
        }
        items
    };
    for (id, dest, mut val) in stale_rows {
        let was_inflight = payload_may_have_reached_server(&val);
        if was_inflight {
            let next_rev = alloc_next_publish_revision_tx(tx, &dest, origin_library, logical_key)?;
            let pub_sref = val
                .get("fact")
                .cloned()
                .and_then(|v| serde_json::from_value::<Fact>(v).ok())
                .map(|f| f.strong_ref());
            let new_idem = ContentHash::digest(serde_json::to_vec(&(
                &dest,
                author,
                &pub_sref,
                new_audience,
                next_rev,
            ))?)
            .to_string();
            if let Some(access_obj) = val
                .get_mut("fact")
                .and_then(|f| f.get_mut("access"))
                .and_then(Value::as_object_mut)
            {
                access_obj.insert("audience".into(), serde_json::to_value(new_audience)?);
            }
            if let Some(root_obj) = val.as_object_mut() {
                root_obj.insert("idempotency_key".into(), json!(new_idem));
                root_obj.insert("audience_revision".into(), json!(next_rev));
                root_obj.insert("revision".into(), json!(next_rev));
                root_obj.insert("shrunk_from_inflight".into(), Value::Bool(true));
            }
            tx.execute(
                "UPDATE outbox SET idempotency_key=?1, payload_json=?2 WHERE outbox_id=?3",
                params![new_idem, serde_json::to_string(&val)?, id],
            )?;
        } else {
            tx.execute("DELETE FROM outbox WHERE outbox_id=?1", [id])?;
        }
    }
    Ok(())
}

pub(super) fn max_known_publish_revision(
    db: &rusqlite::Connection,
    destination: &str,
    origin_library: &LibraryId,
    logical_key: &LogicalKey,
) -> Result<u64> {
    let mut max_prev = 0_u64;
    {
        let mut stmt = db.prepare(
            "SELECT payload_json FROM change_log
             WHERE op='publish_rev_seq' AND origin_library_id=?1 AND logical_key=?2",
        )?;
        let mut rows = stmt.query(params![origin_library.as_str(), logical_key.as_str()])?;
        while let Some(row) = rows.next()? {
            let raw: String = row.get(0)?;
            if let Ok(val) = serde_json::from_str::<Value>(&raw) {
                if val.get("destination").and_then(Value::as_str) == Some(destination) {
                    if let Some(r) = val.get("rev").and_then(Value::as_u64) {
                        max_prev = max_prev.max(r);
                    }
                }
            }
        }
    }
    {
        let mut stmt = db.prepare(
            "SELECT payload_json FROM change_log
             WHERE op='publication' AND origin_library_id=?1 AND logical_key=?2",
        )?;
        let mut rows = stmt.query(params![origin_library.as_str(), logical_key.as_str()])?;
        while let Some(row) = rows.next()? {
            let raw: String = row.get(0)?;
            let (dest, _, rev) = parse_publication_payload(&raw)?;
            if dest == destination {
                max_prev = max_prev.max(rev.unwrap_or(1));
            }
        }
    }
    {
        let mut stmt = db.prepare(
            "SELECT payload_json FROM outbox
             WHERE dest_community_url=?1 AND event_kind='publish' AND delivered_at IS NULL",
        )?;
        let mut rows = stmt.query([destination])?;
        while let Some(row) = rows.next()? {
            let raw: String = row.get(0)?;
            if let Ok(val) = serde_json::from_str::<Value>(&raw) {
                if outbox_publish_matches_identity(&val, origin_library, logical_key) {
                    let rev = val
                        .get("audience_revision")
                        .or_else(|| val.get("revision"))
                        .and_then(Value::as_u64)
                        .unwrap_or(1);
                    max_prev = max_prev.max(rev);
                }
            }
        }
    }
    Ok(max_prev)
}

pub(super) fn alloc_next_publish_revision_tx(
    tx: &rusqlite::Connection,
    destination: &str,
    origin_library: &LibraryId,
    logical_key: &LogicalKey,
) -> Result<u64> {
    let next_rev =
        max_known_publish_revision(tx, destination, origin_library, logical_key)?.saturating_add(1);
    let existing_seq: Option<i64> = {
        let mut stmt = tx.prepare(
            "SELECT seq, payload_json FROM change_log
             WHERE op='publish_rev_seq' AND origin_library_id=?1 AND logical_key=?2
             ORDER BY seq DESC",
        )?;
        let mut rows = stmt.query(params![origin_library.as_str(), logical_key.as_str()])?;
        let mut found = None;
        while let Some(row) = rows.next()? {
            let seq: i64 = row.get(0)?;
            let raw: String = row.get(1)?;
            if let Ok(val) = serde_json::from_str::<Value>(&raw) {
                if val.get("destination").and_then(Value::as_str) == Some(destination) {
                    found = Some(seq);
                    break;
                }
            }
        }
        found
    };
    let payload = serde_json::to_string(&json!({
        "destination": destination,
        "rev": next_rev,
    }))?;
    if let Some(seq) = existing_seq {
        tx.execute(
            "UPDATE change_log SET payload_json=?1 WHERE seq=?2",
            params![payload, seq],
        )?;
    } else {
        tx.execute(
            "INSERT INTO change_log (op,origin_library_id,logical_key,payload_json,created_at)
             VALUES ('publish_rev_seq',?1,?2,?3,0)",
            params![origin_library.as_str(), logical_key.as_str(), payload],
        )?;
    }
    Ok(next_rev)
}

pub(super) fn delete_undelivered_publishes_for_identity_tx(
    tx: &rusqlite::Connection,
    destination: &str,
    origin_library: &LibraryId,
    logical_key: &LogicalKey,
) -> Result<()> {
    let stale_ids = {
        let mut stmt = tx.prepare(
            "SELECT outbox_id, payload_json FROM outbox
             WHERE dest_community_url=?1 AND event_kind='publish' AND delivered_at IS NULL",
        )?;
        let mut rows = stmt.query([destination])?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let raw: String = row.get(1)?;
            if let Ok(val) = serde_json::from_str::<Value>(&raw) {
                if outbox_publish_matches_identity(&val, origin_library, logical_key) {
                    ids.push(id);
                }
            }
        }
        ids
    };
    for stale_id in stale_ids {
        tx.execute("DELETE FROM outbox WHERE outbox_id=?1", [stale_id])?;
    }
    Ok(())
}

/// Parses both legacy JSON string publication payloads (`"\"https://...\""`) and structured
/// `{"destination": "...", "audience": ..., "revision": ...}` payloads.
pub(crate) fn parse_publication_payload(
    raw: &str,
) -> Result<(String, Option<AudienceSpec>, Option<u64>)> {
    let val: Value = serde_json::from_str(raw)?;
    match val {
        Value::String(dest) => Ok((dest, None, None)),
        Value::Object(mut map) => {
            let dest = map
                .remove("destination")
                .and_then(|v| match v {
                    Value::String(s) => Some(s),
                    _ => None,
                })
                .ok_or_else(|| {
                    kp_model::KpError::invalid("publication", "missing destination string")
                })?;
            let audience = map
                .remove("audience")
                .filter(|v| !v.is_null())
                .map(serde_json::from_value)
                .transpose()?;
            let revision = map.remove("revision").and_then(|v| v.as_u64());
            Ok((dest, audience, revision))
        }
        _ => Err(kp_model::KpError::invalid(
            "publication",
            "expected destination string or publication object",
        )
        .into()),
    }
}

pub(super) fn record_publication_with_audience_tx(
    db: &rusqlite::Connection,
    destination: &str,
    reference: &StrongRef,
    remote_id: &str,
    audience: &AudienceSpec,
    revision: u64,
    at: EpochMs,
) -> Result<()> {
    let payload = serde_json::to_string(&json!({
        "destination": destination,
        "audience": audience,
        "revision": revision,
    }))?;
    let existing: Option<(i64, u64)> = {
        let mut stmt = db.prepare(
            "SELECT seq, payload_json FROM change_log
             WHERE op='publication' AND target_id=?1
             ORDER BY seq DESC",
        )?;
        let mut rows = stmt.query([remote_id])?;
        let mut matched = None;
        while let Some(row) = rows.next()? {
            let seq: i64 = row.get(0)?;
            let raw: String = row.get(1)?;
            let (dest, _, rev) = parse_publication_payload(&raw)?;
            if dest == destination {
                matched = Some((seq, rev.unwrap_or(1)));
                break;
            }
        }
        matched
    };
    if let Some((seq, prev_rev)) = existing {
        if revision >= prev_rev {
            db.execute(
                "UPDATE change_log
                 SET origin_library_id=?1, logical_key=?2, content_sha256=?3, payload_json=?4, created_at=?5
                 WHERE seq=?6",
                params![
                    reference.origin_library.as_str(),
                    reference.logical_key.as_str(),
                    reference.content_sha256.as_str(),
                    payload,
                    at.0,
                    seq
                ],
            )?;
        }
    } else {
        db.execute(
            "INSERT INTO change_log (op,target_id,origin_library_id,logical_key,content_sha256,payload_json,created_at)
             VALUES ('publication',?1,?2,?3,?4,?5,?6)",
            params![
                remote_id,
                reference.origin_library.as_str(),
                reference.logical_key.as_str(),
                reference.content_sha256.as_str(),
                payload,
                at.0
            ],
        )?;
    }
    Ok(())
}

pub(super) fn outbox_publish_matches_identity(
    payload: &Value,
    origin_library: &LibraryId,
    logical_key: &LogicalKey,
) -> bool {
    let origin = origin_library.as_str();
    let key = logical_key.as_str();
    if let Some(oref) = payload.get("origin_reference") {
        if oref.get("origin_library").and_then(Value::as_str) == Some(origin)
            && oref.get("logical_key").and_then(Value::as_str) == Some(key)
        {
            return true;
        }
    }
    if let Some(fact) = payload.get("fact") {
        if fact.get("origin_library").and_then(Value::as_str) == Some(origin)
            && fact.get("logical_key").and_then(Value::as_str) == Some(key)
        {
            return true;
        }
    }
    payload.get("origin_library_id").and_then(Value::as_str) == Some(origin)
        && payload.get("logical_key").and_then(Value::as_str) == Some(key)
}

/// Cancels any undelivered `outbox('publish')` rows for `id` AND enqueues idempotent `outbox('withdraw')`
/// events (`{"fact_id": remote_id}`, strictly `'withdraw'`, never `'purge'`) for all recorded or pending
/// destinations inside the caller's active transaction.
pub(crate) fn cancel_publishes_and_queue_withdrawals(
    tx: &rusqlite::Connection,
    id: &FactId,
) -> Result<()> {
    let fact_identity: Option<(String, String, String)> = tx
        .query_row(
            "SELECT origin_library_id, logical_key, content_sha256 FROM facts WHERE fact_id=?1",
            [id.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((origin_str, key_str, sha_str)) = fact_identity else {
        return Ok(());
    };
    let local_ref = StrongRef {
        origin_library: origin_str.parse()?,
        logical_key: key_str.parse()?,
        content_sha256: sha_str.parse()?,
    };
    let default_remote_id = format!(
        "published-{}",
        ContentHash::digest(serde_json::to_vec(&local_ref)?)
    );

    let mut targets = BTreeMap::<(String, String), StrongRef>::new();
    {
        let mut stmt = tx.prepare(
            "SELECT target_id, payload_json FROM change_log
             WHERE op='publication' AND origin_library_id=?1 AND logical_key=?2 AND content_sha256=?3",
        )?;
        let mut rows = stmt.query(params![
            local_ref.origin_library.as_str(),
            local_ref.logical_key.as_str(),
            local_ref.content_sha256.as_str(),
        ])?;
        while let Some(row) = rows.next()? {
            let remote: String = row.get(0)?;
            let raw: String = row.get(1)?;
            let (destination, _, _) = parse_publication_payload(&raw)?;
            targets.insert((destination, remote), local_ref.clone());
        }
    }

    let mut pending_publish_ids = Vec::new();
    {
        let mut stmt = tx.prepare(
            "SELECT outbox_id, dest_community_url, payload_json FROM outbox
             WHERE event_kind='publish' AND delivered_at IS NULL",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let outbox_id: i64 = row.get(0)?;
            let dest: String = row.get(1)?;
            let raw: String = row.get(2)?;
            let Ok(val) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            let matches_origin_ref = val
                .get("origin_reference")
                .cloned()
                .and_then(|v| serde_json::from_value::<StrongRef>(v).ok())
                .as_ref()
                == Some(&local_ref);
            let matches_fact_id = val
                .get("fact")
                .and_then(|f| f.get("id"))
                .and_then(Value::as_str)
                == Some(id.as_str());
            let matches_identity = matches_origin_ref
                || matches_fact_id
                || outbox_publish_matches_identity(
                    &val,
                    &local_ref.origin_library,
                    &local_ref.logical_key,
                );
            if !matches_identity {
                continue;
            }
            let (remote_id, pub_ref) = match val
                .get("fact")
                .cloned()
                .and_then(|v| serde_json::from_value::<Fact>(v).ok())
            {
                Some(pub_fact) => {
                    let sref = pub_fact.strong_ref();
                    let rid = format!(
                        "published-{}",
                        ContentHash::digest(serde_json::to_vec(&sref)?)
                    );
                    (rid, sref)
                }
                None => (default_remote_id.clone(), local_ref.clone()),
            };
            pending_publish_ids.push(outbox_id);
            targets.insert((dest, remote_id), pub_ref);
        }
    }

    for outbox_id in pending_publish_ids {
        tx.execute("DELETE FROM outbox WHERE outbox_id=?1", [outbox_id])?;
    }

    for ((destination, remote), target_ref) in targets {
        let key = ContentHash::digest(serde_json::to_vec(&(&destination, "withdraw", &remote))?);
        tx.execute(
            "INSERT INTO outbox (dest_community_url,event_kind,idempotency_key,payload_json)
             VALUES (?1,'withdraw',?2,?3)
             ON CONFLICT(idempotency_key) DO UPDATE SET delivered_at=NULL, retry_count=0",
            params![
                destination,
                key.as_str(),
                serde_json::to_string(&json!({
                    "fact_id": remote,
                    "origin_library": target_ref.origin_library,
                    "logical_key": target_ref.logical_key,
                    "content_sha256": target_ref.content_sha256,
                }))?
            ],
        )?;
    }
    Ok(())
}

pub(super) struct ScopeCapInput<'a> {
    pub mode: kp_model::LibraryMode,
    pub raw_aud: &'a AudienceSpec,
    pub author: &'a ActorId,
    pub ingester: Option<&'a ActorId>,
    pub scope_id: Option<&'a kp_model::SourceScopeId>,
    pub members: &'a BTreeSet<ActorId>,
    pub strict_validate: bool,
}

pub(super) fn cap_audience_by_live_scope_tx(
    tx: &rusqlite::Connection,
    input: &ScopeCapInput<'_>,
) -> Result<(AudienceSpec, Option<kp_model::SourceScope>)> {
    let ScopeCapInput {
        mode,
        raw_aud,
        author,
        ingester,
        scope_id: scope_id_opt,
        members,
        strict_validate,
    } = *input;
    let Some(scope_id) = scope_id_opt else {
        return Ok((raw_aud.clone(), None));
    };
    let row: Option<(String, String, String, String, i64)> = tx
        .query_row(
            "SELECT provider, external_container_id, label, current_audience_json, synced_at
             FROM source_scopes WHERE scope_id=?1",
            [scope_id.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((provider, external_container_id, label, aud_json, synced_at)) = row else {
        return Ok((raw_aud.clone(), None));
    };
    let mut live_scope = kp_model::SourceScope {
        scope_id: scope_id.clone(),
        provider,
        external_container_id,
        label,
        current_audience: serde_json::from_str(&aud_json)?,
        synced_at: EpochMs(synced_at),
    };
    let ceiling_members: BTreeSet<ActorId> = match (mode, &live_scope.current_audience) {
        (kp_model::LibraryMode::Personal, AudienceSpec::Explicit(actors)) => {
            members.union(actors).cloned().collect()
        }
        _ => members.clone(),
    };
    if !live_scope.allows(author, &ceiling_members) {
        return Err(StoreError::PermissionDenied);
    }
    if let Some(ing) = ingester.filter(|i| *i != author) {
        if !live_scope.allows(ing, &ceiling_members) {
            return Err(StoreError::PermissionDenied);
        }
    }
    let capped_aud = if strict_validate {
        live_scope
            .validate_publish_audience(raw_aud, &ceiling_members)
            .map_err(|_| StoreError::PermissionDenied)?;
        raw_aud.clone()
    } else {
        match (raw_aud, &live_scope.current_audience) {
            (_, AudienceSpec::SemiringDerived) | (AudienceSpec::SemiringDerived, _) => {
                return Err(StoreError::PermissionDenied);
            }
            (AudienceSpec::AllMembers, AudienceSpec::AllMembers) => AudienceSpec::AllMembers,
            (AudienceSpec::AllMembers, AudienceSpec::Explicit(scope_actors)) => {
                AudienceSpec::Explicit(
                    scope_actors
                        .intersection(&ceiling_members)
                        .cloned()
                        .collect(),
                )
            }
            (AudienceSpec::Explicit(fact_actors), AudienceSpec::AllMembers) => {
                AudienceSpec::Explicit(
                    fact_actors
                        .iter()
                        .filter(|a| ceiling_members.contains(a) || *a == author)
                        .cloned()
                        .collect(),
                )
            }
            (AudienceSpec::Explicit(fact_actors), AudienceSpec::Explicit(scope_actors)) => {
                let allowed: BTreeSet<ActorId> = scope_actors
                    .intersection(&ceiling_members)
                    .cloned()
                    .collect();
                AudienceSpec::Explicit(fact_actors.intersection(&allowed).cloned().collect())
            }
        }
    };
    if matches!(&capped_aud, AudienceSpec::Explicit(s) if s.is_empty()) {
        return Err(StoreError::PermissionDenied);
    }
    let redactor = kp_model::SecretRedactor::new()
        .map_err(|_| kp_model::KpError::invalid("redactor", "failed to initialize redactor"))?;
    live_scope.label = redactor.redact(&live_scope.label);
    Ok((capped_aud, Some(live_scope)))
}

pub(super) fn payload_may_have_reached_server(val: &Value) -> bool {
    val.get("inflight_dispatched")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || val
            .get("inflight_uncertain")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || val
            .get("shrunk_from_inflight")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

pub(super) fn has_inflight_publish_for_identity_tx(
    tx: &rusqlite::Connection,
    destination: &str,
    origin_library: &LibraryId,
    logical_key: &LogicalKey,
    fact_id: &FactId,
) -> Result<bool> {
    let mut stmt = tx.prepare(
        "SELECT payload_json FROM outbox
         WHERE dest_community_url=?1 AND event_kind='publish' AND delivered_at IS NULL",
    )?;
    let mut rows = stmt.query([destination])?;
    while let Some(row) = rows.next()? {
        let raw: String = row.get(0)?;
        if let Ok(val) = serde_json::from_str::<Value>(&raw) {
            let matches_fid = val
                .get("fact")
                .and_then(|f| f.get("id"))
                .and_then(Value::as_str)
                == Some(fact_id.as_str());
            if (matches_fid || outbox_publish_matches_identity(&val, origin_library, logical_key))
                && payload_may_have_reached_server(&val)
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(super) fn has_undelivered_publish_for_identity_tx(
    tx: &rusqlite::Connection,
    destination: &str,
    origin_library: &LibraryId,
    logical_key: &LogicalKey,
) -> Result<bool> {
    let mut stmt = tx.prepare(
        "SELECT payload_json FROM outbox
         WHERE dest_community_url=?1 AND event_kind='publish' AND delivered_at IS NULL",
    )?;
    let mut rows = stmt.query([destination])?;
    while let Some(row) = rows.next()? {
        let raw: String = row.get(0)?;
        if let Ok(val) = serde_json::from_str::<Value>(&raw) {
            if outbox_publish_matches_identity(&val, origin_library, logical_key) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(super) fn enqueue_withdraw_for_outbox_sref_tx(
    tx: &rusqlite::Connection,
    destination: &str,
    reference: &StrongRef,
) -> Result<()> {
    let remote_id = format!(
        "published-{}",
        ContentHash::digest(serde_json::to_vec(reference)?)
    );
    let key = ContentHash::digest(serde_json::to_vec(&(destination, "withdraw", &remote_id))?);
    let payload = serde_json::to_string(&json!({
        "fact_id": remote_id,
        "origin_library": reference.origin_library,
        "logical_key": reference.logical_key,
        "content_sha256": reference.content_sha256,
    }))?;
    tx.execute(
        "INSERT INTO outbox (dest_community_url,event_kind,idempotency_key,payload_json)
         VALUES (?1,'withdraw',?2,?3)
         ON CONFLICT(idempotency_key) DO UPDATE SET delivered_at=NULL, retry_count=0",
        params![destination, key.as_str(), payload],
    )?;
    Ok(())
}

pub(super) fn is_version_withdrawn_for_publisher_tx(
    tx: &rusqlite::Connection,
    origin_library: &LibraryId,
    logical_key: &LogicalKey,
    content_sha256: &ContentHash,
    publisher: &ActorId,
) -> Result<bool> {
    Ok(tx.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM change_log c
             WHERE c.op='withdrawn_version'
               AND c.origin_library_id=?1
               AND c.logical_key=?2
               AND c.content_sha256=?3
               AND (
                   c.actor_id IS NULL
                   OR c.actor_id=?4
                   OR EXISTS (
                       SELECT 1 FROM members_tokens m
                       WHERE m.actor_id=c.actor_id
                         AND m.role IN ('owner','maintainer')
                         AND m.revoked_at IS NULL
                   )
               )
         ) OR EXISTS(
             SELECT 1 FROM facts
             WHERE origin_library_id=?1
               AND logical_key=?2
               AND content_sha256=?3
               AND (status IN ('withdrawn', 'superseded') OR expired_at IS NOT NULL)
         )",
        params![
            origin_library.as_str(),
            logical_key.as_str(),
            content_sha256.as_str(),
            publisher.as_str()
        ],
        |r| r.get(0),
    )?)
}

pub(super) fn exchange_revise_published_audience_tx(
    tx: &rusqlite::Connection,
    fact_id: &FactId,
    new_audience: &AudienceSpec,
    revision: u64,
    is_skill: bool,
    at: EpochMs,
) -> Result<bool> {
    let target_row_id = if is_skill {
        format!("support-{fact_id}")
    } else {
        fact_id.to_string()
    };
    let current_aud_json: Option<String> = tx
        .query_row(
            "SELECT audience_json FROM facts
             WHERE fact_id=?1 AND status='active' AND expired_at IS NULL
               AND audience_json != '\"__semiring__\"'",
            [&target_row_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(current_aud_json) = current_aud_json else {
        return Ok(false);
    };
    let current_audience: AudienceSpec = serde_json::from_str(&current_aud_json)?;
    let last_rev: u64 = {
        let mut stmt = tx.prepare(
            "SELECT payload_json FROM change_log
             WHERE op='publish_audience_rev' AND target_id=?1",
        )?;
        let mut rows = stmt.query([fact_id.as_str()])?;
        let mut max_r = 1_u64;
        while let Some(row) = rows.next()? {
            let raw: String = row.get(0)?;
            if let Ok(val) = serde_json::from_str::<Value>(&raw) {
                if let Some(r) = val.get("revision").and_then(Value::as_u64) {
                    max_r = max_r.max(r);
                }
            }
        }
        max_r
    };
    if revision < last_rev {
        return Ok(false);
    }
    if revision == last_rev {
        if new_audience == &current_audience {
            return Ok(false);
        }
        return Err(StoreError::Conflict);
    }
    let changed = new_audience != &current_audience;
    if changed {
        tx.execute(
            "UPDATE facts SET audience_json=?1 WHERE fact_id=?2",
            params![serde_json::to_string(new_audience)?, target_row_id],
        )?;
        mark_alignment_dirty_tx(tx, at)?;
    }
    tx.execute(
        "DELETE FROM change_log WHERE op='publish_audience_rev' AND target_id=?1",
        [fact_id.as_str()],
    )?;
    tx.execute(
        "INSERT INTO change_log (op,target_id,payload_json,created_at)
         VALUES ('publish_audience_rev',?1,?2,?3)",
        params![
            fact_id.as_str(),
            serde_json::to_string(&json!({
                "revision": revision,
                "audience": new_audience,
            }))?,
            at.0
        ],
    )?;
    Ok(true)
}

struct ScopeAffectedFactRow {
    fact_id: String,
    origin_library_id: String,
    logical_key: String,
    content_sha256: String,
    audience_json: String,
    author_actor: String,
    ingested_by_actor: Option<String>,
}

struct ScopeRevisionTarget<'a> {
    mode: kp_model::LibraryMode,
    fact_id: &'a FactId,
    origin_library: &'a LibraryId,
    logical_key: &'a LogicalKey,
    content_sha256: &'a ContentHash,
    raw_fact_aud: &'a AudienceSpec,
    author: &'a ActorId,
    ingester: Option<&'a ActorId>,
    scope_id: &'a kp_model::SourceScopeId,
    members: &'a BTreeSet<ActorId>,
    live_scope_opt: Option<&'a kp_model::SourceScope>,
}

pub(crate) fn reconcile_pending_publishes_for_scope_tx(
    tx: &rusqlite::Connection,
    mode: kp_model::LibraryMode,
    scope_id: &kp_model::SourceScopeId,
) -> Result<()> {
    let members = read_active_members_tx(tx)?;
    let affected: Vec<ScopeAffectedFactRow> = {
        let mut stmt = tx.prepare(
            "SELECT fact_id, origin_library_id, logical_key, content_sha256, audience_json, author_actor, ingested_by_actor
             FROM facts
             WHERE source_scope_id=?1 AND status='active' AND expired_at IS NULL
               AND audience_json != '\"__semiring__\"'",
        )?;
        let mut rows = stmt.query([scope_id.as_str()])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(ScopeAffectedFactRow {
                fact_id: row.get(0)?,
                origin_library_id: row.get(1)?,
                logical_key: row.get(2)?,
                content_sha256: row.get(3)?,
                audience_json: row.get(4)?,
                author_actor: row.get(5)?,
                ingested_by_actor: row.get(6)?,
            });
        }
        out
    };
    for row in affected {
        let fid: FactId = row.fact_id.parse()?;
        let origin: LibraryId = row.origin_library_id.parse()?;
        let lkey: LogicalKey = row.logical_key.parse()?;
        let sha: ContentHash = row.content_sha256.parse()?;
        let raw_aud: AudienceSpec = serde_json::from_str(&row.audience_json)?;
        let author: ActorId = row.author_actor.parse()?;
        let ingester: Option<ActorId> = row
            .ingested_by_actor
            .as_deref()
            .map(str::parse)
            .transpose()?;
        match cap_audience_by_live_scope_tx(
            tx,
            &ScopeCapInput {
                mode,
                raw_aud: &raw_aud,
                author: &author,
                ingester: ingester.as_ref(),
                scope_id: Some(scope_id),
                members: &members,
                strict_validate: false,
            },
        ) {
            Ok((capped, live_scope_opt)) => {
                if !audience_has_other_viewer(&capped, &author, &members) {
                    cancel_unrevised_self_only_pending_publishes_tx(
                        tx, &fid, &origin, &lkey, &author, &capped, &members,
                    )?;
                }
                enqueue_scope_revisions_for_delivered_publishes_tx(
                    tx,
                    &ScopeRevisionTarget {
                        mode,
                        fact_id: &fid,
                        origin_library: &origin,
                        logical_key: &lkey,
                        content_sha256: &sha,
                        raw_fact_aud: &raw_aud,
                        author: &author,
                        ingester: ingester.as_ref(),
                        scope_id,
                        members: &members,
                        live_scope_opt: live_scope_opt.as_ref(),
                    },
                )?;
            }
            Err(StoreError::PermissionDenied) => {
                cancel_publishes_and_queue_withdrawals(tx, &fid)?;
            }
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

fn enqueue_scope_revisions_for_delivered_publishes_tx(
    tx: &rusqlite::Connection,
    target: &ScopeRevisionTarget<'_>,
) -> Result<()> {
    let ScopeRevisionTarget {
        mode,
        fact_id,
        origin_library,
        logical_key,
        content_sha256,
        raw_fact_aud,
        author,
        ingester,
        scope_id,
        members,
        live_scope_opt,
    } = *target;
    let mut receipts_by_dest: BTreeMap<String, (Option<AudienceSpec>, u64)> = BTreeMap::new();
    {
        let mut stmt = tx.prepare(
            "SELECT payload_json FROM change_log
             WHERE op='publication' AND origin_library_id=?1 AND logical_key=?2 AND content_sha256=?3
             ORDER BY seq ASC",
        )?;
        let mut rows = stmt.query(params![
            origin_library.as_str(),
            logical_key.as_str(),
            content_sha256.as_str()
        ])?;
        while let Some(row) = rows.next()? {
            let raw: String = row.get(0)?;
            let (dest, aud, rev) = parse_publication_payload(&raw)?;
            let rev_num = rev.unwrap_or(1);
            let entry = receipts_by_dest.entry(dest).or_insert((None, 0));
            if rev_num >= entry.1 {
                *entry = (aud, rev_num);
            }
        }
    }
    if receipts_by_dest.is_empty() {
        return Ok(());
    }

    for (dest, (last_pub_aud, _)) in receipts_by_dest {
        if has_undelivered_publish_for_identity_tx(tx, &dest, origin_library, logical_key)? {
            continue;
        }
        let latest_template: Option<Value> = {
            let mut stmt = tx.prepare(
                "SELECT payload_json FROM outbox
                 WHERE dest_community_url=?1 AND event_kind='publish' AND delivered_at IS NOT NULL
                 ORDER BY outbox_id DESC",
            )?;
            let mut rows = stmt.query([&dest])?;
            let mut found = None;
            while let Some(row) = rows.next()? {
                let raw: String = row.get(0)?;
                if let Ok(val) = serde_json::from_str::<Value>(&raw) {
                    let matches_fid = val
                        .get("fact")
                        .and_then(|f| f.get("id"))
                        .and_then(Value::as_str)
                        == Some(fact_id.as_str());
                    if matches_fid
                        || outbox_publish_matches_identity(&val, origin_library, logical_key)
                    {
                        found = Some(val);
                        break;
                    }
                }
            }
            found
        };
        let Some(mut template) = latest_template else {
            continue;
        };
        let is_skill = template
            .get("fact")
            .and_then(|f| f.get("kind"))
            .and_then(Value::as_str)
            == Some("skill_version")
            || template.get("skill_audience").is_some_and(|v| !v.is_null());
        let explicit_override = template
            .get("explicit_audience_override")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let base_aud = if is_skill {
            template
                .get("skill_audience")
                .cloned()
                .and_then(|v| serde_json::from_value::<AudienceSpec>(v).ok())
                .unwrap_or_else(|| raw_fact_aud.clone())
        } else if explicit_override {
            template
                .get("fact")
                .and_then(|f| f.get("access"))
                .and_then(|a| a.get("audience"))
                .cloned()
                .and_then(|v| serde_json::from_value::<AudienceSpec>(v).ok())
                .unwrap_or_else(|| raw_fact_aud.clone())
        } else {
            raw_fact_aud.clone()
        };
        let (effective_dest_aud, _) = match cap_audience_by_live_scope_tx(
            tx,
            &ScopeCapInput {
                mode,
                raw_aud: &base_aud,
                author,
                ingester,
                scope_id: Some(scope_id),
                members,
                strict_validate: false,
            },
        ) {
            Ok(pair) => pair,
            Err(StoreError::PermissionDenied) => {
                let sref = StrongRef {
                    origin_library: origin_library.clone(),
                    logical_key: logical_key.clone(),
                    content_sha256: content_sha256.clone(),
                };
                enqueue_withdraw_for_outbox_sref_tx(tx, &dest, &sref)?;
                continue;
            }
            Err(err) => return Err(err),
        };
        if last_pub_aud.as_ref() == Some(&effective_dest_aud) {
            continue;
        }
        let next_rev = alloc_next_publish_revision_tx(tx, &dest, origin_library, logical_key)?;
        let pub_sref = template
            .get("fact")
            .cloned()
            .and_then(|v| serde_json::from_value::<Fact>(v).ok())
            .map(|f| f.strong_ref())
            .unwrap_or_else(|| StrongRef {
                origin_library: origin_library.clone(),
                logical_key: logical_key.clone(),
                content_sha256: content_sha256.clone(),
            });
        let new_idem = ContentHash::digest(serde_json::to_vec(&(
            &dest,
            author,
            &pub_sref,
            &effective_dest_aud,
            &live_scope_opt,
            next_rev,
        ))?)
        .to_string();
        if !is_skill {
            if let Some(access_obj) = template
                .get_mut("fact")
                .and_then(|f| f.get_mut("access"))
                .and_then(Value::as_object_mut)
            {
                access_obj.insert(
                    "audience".into(),
                    serde_json::to_value(&effective_dest_aud)?,
                );
            }
        }
        if let Some(root_obj) = template.as_object_mut() {
            if is_skill {
                root_obj.insert(
                    "skill_audience".into(),
                    serde_json::to_value(&effective_dest_aud)?,
                );
            }
            match live_scope_opt {
                Some(sc) => {
                    root_obj.insert("source_scope".into(), serde_json::to_value(sc)?);
                }
                None => {
                    root_obj.remove("source_scope");
                }
            }
            root_obj.insert("idempotency_key".into(), json!(new_idem));
            root_obj.insert("audience_revision".into(), json!(next_rev));
            root_obj.insert("revision".into(), json!(next_rev));
            root_obj.remove("inflight_dispatched");
            root_obj.remove("inflight_uncertain");
            root_obj.remove("shrunk_from_inflight");
        }
        tx.execute(
            "INSERT INTO outbox (dest_community_url,event_kind,idempotency_key,payload_json)
             VALUES (?1,'publish',?2,?3)
             ON CONFLICT(idempotency_key) DO UPDATE SET
                 payload_json=excluded.payload_json,
                 delivered_at=NULL,
                 retry_count=0",
            params![dest, new_idem, serde_json::to_string(&template)?],
        )?;
    }
    Ok(())
}
