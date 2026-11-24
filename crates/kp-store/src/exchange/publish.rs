use std::collections::BTreeSet;

use kp_model::{
    ActorId, AudienceSpec, ContentHash, EpochMs, Fact, FactId, FactStatus, LibraryId, LogicalKey,
    StrongRef,
};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};

use super::publish_helpers::{
    alloc_next_publish_revision_tx, audience_has_other_viewer,
    cancel_publishes_and_queue_withdrawals, cancel_unrevised_self_only_pending_publishes_tx,
    cap_audience_by_live_scope_tx, delete_undelivered_publishes_for_identity_tx,
    enqueue_withdraw_for_outbox_sref_tx, exchange_revise_published_audience_tx,
    has_inflight_publish_for_identity_tx, has_publication_receipt_for_dest_tx,
    has_undelivered_publish_for_identity_tx, is_version_withdrawn_for_publisher_tx,
    max_known_publish_revision, payload_may_have_reached_server, read_active_members_tx,
    record_publication_with_audience_tx, ScopeCapInput,
};
use super::snapshot_sync::{link_fact_concept_tx, mark_alignment_dirty_tx, upsert_concept_tx};
use super::{write_origin_reference, InitialPublishWrite};
use crate::facts::read_graph;
use crate::{Result, Store, StoreError};

/// Target claim and audience parameters for [`Store::enqueue_publish_atomic`].
#[derive(Clone, Debug)]
pub struct PublishEnqueueSpec<'a> {
    pub destination: &'a str,
    pub origin_library: &'a LibraryId,
    pub logical_key: &'a LogicalKey,
    pub fact_id: &'a FactId,
    pub explicit_audience_override: Option<&'a AudienceSpec>,
    pub require_multi_actor_or_prior_receipt: bool,
}

/// Receipt parameters for [`Store::finalize_inflight_publish`].
#[derive(Clone, Debug)]
pub struct FinalizePublishSpec<'a> {
    pub outbox_id: i64,
    pub destination: &'a str,
    pub reference: &'a StrongRef,
    pub remote_id: &'a str,
    pub audience: &'a AudienceSpec,
    pub revision: u64,
    pub at: EpochMs,
}

/// Server-side acceptance parameters for [`Store::exchange_accept_initial_publish`].
#[derive(Clone, Debug)]
pub struct AcceptInitialPublishSpec<'a> {
    pub pub_owner: &'a ActorId,
    pub pub_sref: &'a StrongRef,
    pub redacted_origin_ref: &'a StrongRef,
    pub requested_audience: &'a AudienceSpec,
    pub revision: u64,
    pub concepts: &'a [(kp_model::CommunityConcept, bool)],
    pub now: EpochMs,
}

struct LocalPublishFactRow {
    fact_id: String,
    status: String,
    expired_at: Option<i64>,
    audience_json: String,
    author_actor: String,
    ingested_by_actor: Option<String>,
    source_scope_id: Option<String>,
}

impl LocalPublishFactRow {
    fn from_row_with_id(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            fact_id: r.get(0)?,
            status: r.get(1)?,
            expired_at: r.get(2)?,
            audience_json: r.get(3)?,
            author_actor: r.get(4)?,
            ingested_by_actor: r.get(5)?,
            source_scope_id: r.get(6)?,
        })
    }

    fn from_row_without_id(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            fact_id: String::new(),
            status: r.get(0)?,
            expired_at: r.get(1)?,
            audience_json: r.get(2)?,
            author_actor: r.get(3)?,
            ingested_by_actor: r.get(4)?,
            source_scope_id: r.get(5)?,
        })
    }
}

impl Store {
    /// Computes the next monotonic publication revision (`max_prev_revision + 1`, starting at `1`)
    /// for `(destination, origin_library, logical_key)` across durable `publish_rev_seq`, recorded
    /// publications, and pending outbox rows.
    pub fn next_publication_revision(
        &self,
        destination: &str,
        origin_library: &LibraryId,
        logical_key: &LogicalKey,
    ) -> Result<u64> {
        Ok(
            max_known_publish_revision(&self.db, destination, origin_library, logical_key)?
                .saturating_add(1),
        )
    }

    /// Atomically reads the live `audience` and `source_scope` of `fact_id` (or `explicit_audience_override`),
    /// optionally enforces `require_multi_actor_or_prior_receipt` (returning `Ok(None)` without queueing if an
    /// automatic publish candidate shrank to `SelfOnly` with no prior receipt), increments durable
    /// `change_log(op='publish_rev_seq')`, replaces non-inflight undelivered `'publish'` rows, and inserts
    /// the new `'publish'` row in one `TransactionBehavior::Immediate` transaction.
    pub fn enqueue_publish_atomic(
        &mut self,
        spec: &PublishEnqueueSpec<'_>,
        build_request: impl FnOnce(
            u64,
            &AudienceSpec,
            Option<&kp_model::SourceScope>,
        ) -> Result<(String, Value)>,
    ) -> Result<Option<(u64, String)>> {
        let PublishEnqueueSpec {
            destination,
            origin_library,
            logical_key,
            fact_id,
            explicit_audience_override,
            require_multi_actor_or_prior_receipt,
        } = *spec;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let opaque = crate::tombstone_id(
            origin_library.as_str().as_bytes(),
            origin_library,
            logical_key,
        )?;
        crate::tombstone::guard(&tx, &opaque)?;
        let row = tx
            .query_row(
                "SELECT status, expired_at, audience_json, author_actor, ingested_by_actor, source_scope_id
                 FROM facts WHERE fact_id=?1",
                [fact_id.as_str()],
                LocalPublishFactRow::from_row_without_id,
            )
            .optional()?;
        let Some(LocalPublishFactRow {
            status,
            expired_at,
            audience_json: aud_json,
            author_actor: author_str,
            ingested_by_actor: ingester_opt,
            source_scope_id: scope_id_opt,
            ..
        }) = row
        else {
            return Err(StoreError::NotFound);
        };
        if status != "active" || expired_at.is_some() {
            return Err(
                kp_model::KpError::invalid("publish", "publish requires an active claim").into(),
            );
        }
        let author: ActorId = author_str.parse()?;
        let ingester: Option<ActorId> = ingester_opt.as_deref().map(str::parse).transpose()?;
        let scope_id: Option<kp_model::SourceScopeId> =
            scope_id_opt.as_deref().map(str::parse).transpose()?;
        let members = read_active_members_tx(&tx)?;
        let raw_aud = match explicit_audience_override {
            Some(aud) => aud.clone(),
            None => {
                let parsed: AudienceSpec = serde_json::from_str(&aud_json)?;
                if matches!(parsed, AudienceSpec::SemiringDerived) {
                    return Err(kp_model::KpError::invalid(
                        "audience",
                        "derived skill publication requires explicit_audience_override",
                    )
                    .into());
                }
                parsed
            }
        };
        let (effective_aud, live_scope_opt) = cap_audience_by_live_scope_tx(
            &tx,
            &ScopeCapInput {
                mode: self.mode,
                raw_aud: &raw_aud,
                author: &author,
                ingester: ingester.as_ref(),
                scope_id: scope_id.as_ref(),
                members: &members,
                strict_validate: explicit_audience_override.is_some(),
            },
        )?;
        let prior_inflight = has_inflight_publish_for_identity_tx(
            &tx,
            destination,
            origin_library,
            logical_key,
            fact_id,
        )?;
        if require_multi_actor_or_prior_receipt
            && !audience_has_other_viewer(&effective_aud, &author, &members)
            && !prior_inflight
            && !has_publication_receipt_for_dest_tx(&tx, destination, origin_library, logical_key)?
        {
            cancel_unrevised_self_only_pending_publishes_tx(
                &tx,
                fact_id,
                origin_library,
                logical_key,
                &author,
                &effective_aud,
                &members,
            )?;
            tx.commit()?;
            return Ok(None);
        }

        let rev = alloc_next_publish_revision_tx(&tx, destination, origin_library, logical_key)?;
        let (idempotency_key, mut payload) =
            build_request(rev, &effective_aud, live_scope_opt.as_ref())?;
        if let Value::Object(ref mut map) = payload {
            map.insert("audience_revision".into(), json!(rev));
            map.insert("revision".into(), json!(rev));
            if explicit_audience_override.is_some() {
                map.insert("explicit_audience_override".into(), Value::Bool(true));
            }
            if prior_inflight {
                map.insert("shrunk_from_inflight".into(), Value::Bool(true));
            }
        }
        delete_undelivered_publishes_for_identity_tx(
            &tx,
            destination,
            origin_library,
            logical_key,
        )?;
        tx.execute(
            "INSERT INTO outbox (dest_community_url,event_kind,idempotency_key,payload_json)
             VALUES (?1,'publish',?2,?3)
             ON CONFLICT(idempotency_key) DO UPDATE SET
                 payload_json=excluded.payload_json,
                 delivered_at=NULL,
                 retry_count=0",
            params![
                destination,
                &idempotency_key,
                serde_json::to_string(&payload)?
            ],
        )?;
        tx.commit()?;
        Ok(Some((rev, idempotency_key)))
    }

    /// Validates and refreshes a pending `outbox('publish')` entry right before HTTP delivery in one
    /// `TransactionBehavior::Immediate` transaction:
    /// - Missing / tombstoned / retracted / superseded / expired -> queues compensating `'withdraw'` or
    ///   `'purge'` (if needed), deletes `outbox_id`, and returns `Ok(None)`.
    /// - Live audience (capped by live `source_scopes`) shrunk to `SelfOnly` before any `publication`
    ///   receipt or prior in-flight dispatch exists -> deletes `outbox_id` and returns `Ok(None)`.
    /// - Live base audience or `source_scope` changed since queueing -> allocates next monotonic revision,
    ///   updates `outbox_id` in place, stamps `"inflight_dispatched": true`, and returns `Ok(Some(updated))`.
    pub fn prepare_outbox_publish(&mut self, outbox_id: i64) -> Result<Option<Value>> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, String, String, Option<i64>)> = tx
            .query_row(
                "SELECT dest_community_url, event_kind, payload_json, delivered_at
                 FROM outbox WHERE outbox_id=?1",
                [outbox_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((dest_url, kind, raw_payload, delivered_at)) = row else {
            tx.commit()?;
            return Ok(None);
        };
        if delivered_at.is_some() {
            tx.commit()?;
            return Ok(None);
        }
        let mut payload: Value = serde_json::from_str(&raw_payload)?;
        if kind != "publish" {
            tx.commit()?;
            return Ok(Some(payload));
        }

        let origin_ref = payload
            .get("origin_reference")
            .cloned()
            .and_then(|v| serde_json::from_value::<StrongRef>(v).ok())
            .or_else(|| {
                payload
                    .get("fact")
                    .cloned()
                    .and_then(|v| serde_json::from_value::<Fact>(v).ok())
                    .map(|f| f.strong_ref())
            });
        let Some(sref) = origin_ref else {
            tx.commit()?;
            return Ok(Some(payload));
        };

        let opaque = crate::tombstone_id(
            sref.origin_library.as_str().as_bytes(),
            &sref.origin_library,
            &sref.logical_key,
        )?;
        let tombstoned: Option<(String, i64, String)> = tx
            .query_row(
                "SELECT origin_library_id, purged_at, reason FROM tombstones WHERE tombstone_id=?1",
                [opaque.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((origin_lib, purged_at_ms, reason)) = tombstoned {
            let dedup = ContentHash::digest(serde_json::to_vec(&(&dest_url, opaque.as_str()))?);
            let purge_payload = serde_json::to_string(&json!({
                "tombstone_id": opaque,
                "origin_library_id": origin_lib,
                "purged_at": EpochMs(purged_at_ms),
                "reason": reason,
            }))?;
            tx.execute("DELETE FROM outbox WHERE outbox_id=?1", [outbox_id])?;
            tx.execute(
                "INSERT INTO outbox (dest_community_url,event_kind,idempotency_key,payload_json)
                 VALUES (?1,'purge',?2,?3)
                 ON CONFLICT(idempotency_key) DO NOTHING",
                params![dest_url, dedup.as_str(), purge_payload],
            )?;
            tx.commit()?;
            return Ok(None);
        }

        let payload_fact_id = payload
            .get("fact")
            .and_then(|f| f.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mut local_row = tx
            .query_row(
                "SELECT fact_id, status, expired_at, audience_json, author_actor, ingested_by_actor, source_scope_id
                 FROM facts
                 WHERE origin_library_id=?1 AND logical_key=?2 AND content_sha256=?3
                 ORDER BY (status='active' AND expired_at IS NULL) DESC, fact_id ASC LIMIT 1",
                params![
                    sref.origin_library.as_str(),
                    sref.logical_key.as_str(),
                    sref.content_sha256.as_str()
                ],
                LocalPublishFactRow::from_row_with_id,
            )
            .optional()?;
        if local_row.is_none() {
            if let Some(ref fid) = payload_fact_id {
                local_row = tx
                    .query_row(
                        "SELECT fact_id, status, expired_at, audience_json, author_actor, ingested_by_actor, source_scope_id
                         FROM facts
                         WHERE fact_id=?1 AND origin_library_id=?2",
                        params![fid, sref.origin_library.as_str()],
                        LocalPublishFactRow::from_row_with_id,
                    )
                    .optional()?;
            }
        }
        let Some(LocalPublishFactRow {
            fact_id: local_id_str,
            status,
            expired_at,
            audience_json: live_aud_json,
            author_actor: author_str,
            ingested_by_actor: ingester_opt,
            source_scope_id: scope_id_opt,
        }) = local_row
        else {
            tx.execute("DELETE FROM outbox WHERE outbox_id=?1", [outbox_id])?;
            tx.commit()?;
            return Ok(None);
        };
        let local_id: FactId = local_id_str.parse()?;
        if status != "active" || expired_at.is_some() {
            cancel_publishes_and_queue_withdrawals(&tx, &local_id)?;
            tx.execute("DELETE FROM outbox WHERE outbox_id=?1", [outbox_id])?;
            tx.commit()?;
            return Ok(None);
        }

        let is_skill = payload
            .get("fact")
            .and_then(|f| f.get("kind"))
            .and_then(Value::as_str)
            == Some("skill_version")
            || payload.get("skill_audience").is_some_and(|v| !v.is_null());
        let explicit_override = payload
            .get("explicit_audience_override")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let was_inflight = payload_may_have_reached_server(&payload);
        let has_receipt = has_publication_receipt_for_dest_tx(
            &tx,
            &dest_url,
            &sref.origin_library,
            &sref.logical_key,
        )?;
        let must_compensate_remote = was_inflight || has_receipt;

        let author: ActorId = author_str.parse()?;
        let ingester: Option<ActorId> = ingester_opt.as_deref().map(str::parse).transpose()?;
        let scope_id: Option<kp_model::SourceScopeId> =
            scope_id_opt.as_deref().map(str::parse).transpose()?;
        let members = read_active_members_tx(&tx)?;
        let payload_aud = if is_skill {
            payload
                .get("skill_audience")
                .cloned()
                .and_then(|v| serde_json::from_value::<AudienceSpec>(v).ok())
        } else {
            payload
                .get("fact")
                .and_then(|f| f.get("access"))
                .and_then(|a| a.get("audience"))
                .cloned()
                .and_then(|v| serde_json::from_value::<AudienceSpec>(v).ok())
        };
        let payload_scope = payload
            .get("source_scope")
            .cloned()
            .and_then(|v| serde_json::from_value::<kp_model::SourceScope>(v).ok());

        let base_aud_for_cap: AudienceSpec = if is_skill || explicit_override {
            payload_aud
                .clone()
                .unwrap_or_else(|| AudienceSpec::Explicit(BTreeSet::from([author.clone()])))
        } else {
            serde_json::from_str(&live_aud_json)?
        };

        let (effective_live_aud, live_scope_opt) = match cap_audience_by_live_scope_tx(
            &tx,
            &ScopeCapInput {
                mode: self.mode,
                raw_aud: &base_aud_for_cap,
                author: &author,
                ingester: ingester.as_ref(),
                scope_id: scope_id.as_ref(),
                members: &members,
                strict_validate: false,
            },
        ) {
            Ok(pair) => pair,
            Err(StoreError::PermissionDenied) => {
                if must_compensate_remote {
                    enqueue_withdraw_for_outbox_sref_tx(&tx, &dest_url, &sref)?;
                }
                tx.execute("DELETE FROM outbox WHERE outbox_id=?1", [outbox_id])?;
                tx.commit()?;
                return Ok(None);
            }
            Err(err) => return Err(err),
        };

        if !is_skill && !explicit_override {
            let payload_had_other_viewer = payload_aud
                .as_ref()
                .is_some_and(|pa| audience_has_other_viewer(pa, &author, &members));
            let scope_changed = payload_scope != live_scope_opt;
            if !must_compensate_remote
                && (payload_had_other_viewer
                    || payload_aud.as_ref() != Some(&effective_live_aud)
                    || scope_changed)
                && !audience_has_other_viewer(&effective_live_aud, &author, &members)
            {
                tx.execute("DELETE FROM outbox WHERE outbox_id=?1", [outbox_id])?;
                tx.commit()?;
                return Ok(None);
            }
        }

        let aud_changed = payload_aud.as_ref() != Some(&effective_live_aud);
        let scope_changed = payload_scope != live_scope_opt;
        let mut current_idem = payload
            .get("idempotency_key")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();

        if aud_changed || scope_changed {
            let next_rev = alloc_next_publish_revision_tx(
                &tx,
                &dest_url,
                &sref.origin_library,
                &sref.logical_key,
            )?;
            let pub_sref = payload
                .get("fact")
                .cloned()
                .and_then(|v| serde_json::from_value::<Fact>(v).ok())
                .map(|f| f.strong_ref())
                .unwrap_or_else(|| sref.clone());
            current_idem = ContentHash::digest(serde_json::to_vec(&(
                &dest_url,
                &author,
                &pub_sref,
                &effective_live_aud,
                &live_scope_opt,
                next_rev,
            ))?)
            .to_string();
            if !is_skill {
                if let Some(access_obj) = payload
                    .get_mut("fact")
                    .and_then(|f| f.get_mut("access"))
                    .and_then(Value::as_object_mut)
                {
                    access_obj.insert(
                        "audience".into(),
                        serde_json::to_value(&effective_live_aud)?,
                    );
                }
            }
            if let Some(root_obj) = payload.as_object_mut() {
                if is_skill {
                    root_obj.insert(
                        "skill_audience".into(),
                        serde_json::to_value(&effective_live_aud)?,
                    );
                }
                match &live_scope_opt {
                    Some(sc) => {
                        root_obj.insert("source_scope".into(), serde_json::to_value(sc)?);
                    }
                    None => {
                        root_obj.remove("source_scope");
                    }
                }
                root_obj.insert("idempotency_key".into(), json!(current_idem));
                root_obj.insert("audience_revision".into(), json!(next_rev));
                root_obj.insert("revision".into(), json!(next_rev));
            }
        }

        if let Some(root_obj) = payload.as_object_mut() {
            root_obj.insert("inflight_dispatched".into(), Value::Bool(true));
        }
        if current_idem.is_empty() {
            tx.execute(
                "UPDATE outbox SET payload_json=?1 WHERE outbox_id=?2",
                params![serde_json::to_string(&payload)?, outbox_id],
            )?;
        } else {
            tx.execute(
                "UPDATE outbox SET idempotency_key=?1, payload_json=?2 WHERE outbox_id=?3",
                params![current_idem, serde_json::to_string(&payload)?, outbox_id],
            )?;
        }

        tx.commit()?;
        Ok(Some(payload))
    }

    pub fn record_publication(
        &mut self,
        destination: &str,
        reference: &StrongRef,
        remote_id: &str,
        at: EpochMs,
    ) -> Result<()> {
        let aud_json: Option<String> = self
            .db
            .query_row(
                "SELECT audience_json FROM facts
                 WHERE origin_library_id=?1 AND logical_key=?2 AND content_sha256=?3
                 ORDER BY (status='active') DESC, fact_id ASC LIMIT 1",
                params![
                    reference.origin_library.as_str(),
                    reference.logical_key.as_str(),
                    reference.content_sha256.as_str()
                ],
                |r| r.get(0),
            )
            .optional()?;
        let base_aud = match aud_json {
            Some(raw) => match serde_json::from_str::<AudienceSpec>(&raw)? {
                AudienceSpec::SemiringDerived => AudienceSpec::AllMembers,
                other => other,
            },
            None => AudienceSpec::AllMembers,
        };
        self.record_publication_with_audience(destination, reference, remote_id, &base_aud, 1, at)
    }

    /// Records `(destination, remote_id, audience, revision)` in `change_log(op='publication')`,
    /// updating any existing row for `(remote_id, destination)` monotonically.
    pub fn record_publication_with_audience(
        &mut self,
        destination: &str,
        reference: &StrongRef,
        remote_id: &str,
        audience: &AudienceSpec,
        revision: u64,
        at: EpochMs,
    ) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        record_publication_with_audience_tx(
            &tx,
            destination,
            reference,
            remote_id,
            audience,
            revision,
            at,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Atomically removes any older undelivered `'publish'` outbox rows for `(destination, origin_library, logical_key)`
    /// and inserts the replacement `'publish'` row.
    pub fn exchange_replace_undelivered_publish(
        &mut self,
        destination: &str,
        origin_library: &LibraryId,
        logical_key: &LogicalKey,
        idempotency_key: &str,
        payload: &Value,
    ) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        delete_undelivered_publishes_for_identity_tx(
            &tx,
            destination,
            origin_library,
            logical_key,
        )?;
        tx.execute(
            "INSERT INTO outbox (dest_community_url,event_kind,idempotency_key,payload_json)
             VALUES (?1,'publish',?2,?3)
             ON CONFLICT(idempotency_key) DO UPDATE SET
                 payload_json=excluded.payload_json,
                 delivered_at=NULL,
                 retry_count=0",
            params![
                destination,
                idempotency_key,
                serde_json::to_string(payload)?
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Atomically marks `outbox_id` delivered, records publication `(audience, revision)`, and if the
    /// local claim was concurrently retracted/superseded/expired (or purged) while the publish RPC was
    /// in flight, immediately enqueues the compensating `'withdraw'` (or `'purge'` tombstone).
    pub fn finalize_inflight_publish(&mut self, spec: &FinalizePublishSpec<'_>) -> Result<()> {
        let FinalizePublishSpec {
            outbox_id,
            destination,
            reference,
            remote_id,
            audience,
            revision,
            at,
        } = *spec;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let delivered_payload: Option<Value> = tx
            .query_row(
                "SELECT payload_json FROM outbox WHERE outbox_id=?1",
                [outbox_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
        tx.execute(
            "UPDATE outbox SET delivered_at=?1 WHERE outbox_id=?2",
            params![at.0, outbox_id],
        )?;

        let opaque = crate::tombstone_id(
            reference.origin_library.as_str().as_bytes(),
            &reference.origin_library,
            &reference.logical_key,
        )?;
        let tombstoned: Option<(String, i64, String)> = tx
            .query_row(
                "SELECT origin_library_id, purged_at, reason FROM tombstones WHERE tombstone_id=?1",
                [opaque.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;

        if let Some((origin_lib, purged_at_ms, reason)) = tombstoned {
            let dedup = ContentHash::digest(serde_json::to_vec(&(destination, opaque.as_str()))?);
            let payload = serde_json::to_string(&json!({
                "tombstone_id": opaque,
                "origin_library_id": origin_lib,
                "purged_at": EpochMs(purged_at_ms),
                "reason": reason,
            }))?;
            tx.execute(
                "INSERT INTO outbox (dest_community_url,event_kind,idempotency_key,payload_json)
                 VALUES (?1,'purge',?2,?3)
                 ON CONFLICT(idempotency_key) DO UPDATE SET delivered_at=NULL, retry_count=0",
                params![destination, dedup.as_str(), payload],
            )?;
            tx.commit()?;
            return Ok(());
        }

        record_publication_with_audience_tx(
            &tx,
            destination,
            reference,
            remote_id,
            audience,
            revision,
            at,
        )?;

        let local_state = tx
            .query_row(
                "SELECT status, expired_at, audience_json, author_actor, ingested_by_actor, source_scope_id
                 FROM facts
                 WHERE origin_library_id=?1 AND logical_key=?2 AND content_sha256=?3
                 ORDER BY (status='active' AND expired_at IS NULL) DESC, fact_id ASC LIMIT 1",
                params![
                    reference.origin_library.as_str(),
                    reference.logical_key.as_str(),
                    reference.content_sha256.as_str()
                ],
                LocalPublishFactRow::from_row_without_id,
            )
            .optional()?;

        let needs_withdraw = match &local_state {
            Some(row) => row.status != "active" || row.expired_at.is_some(),
            None => false,
        };
        if needs_withdraw {
            let key =
                ContentHash::digest(serde_json::to_vec(&(destination, "withdraw", remote_id))?);
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
        } else if let (
            Some(LocalPublishFactRow {
                audience_json: live_aud_json,
                author_actor: author_str,
                ingested_by_actor: ingester_opt,
                source_scope_id: scope_id_opt,
                ..
            }),
            Some(mut template),
        ) = (local_state, delivered_payload)
        {
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
            if (!is_skill && !explicit_override) || scope_id_opt.is_some() {
                let raw_live_aud: AudienceSpec = if is_skill || explicit_override {
                    audience.clone()
                } else {
                    serde_json::from_str(&live_aud_json)?
                };
                let author: ActorId = author_str.parse()?;
                let ingester: Option<ActorId> =
                    ingester_opt.as_deref().map(str::parse).transpose()?;
                let scope_id: Option<kp_model::SourceScopeId> =
                    scope_id_opt.as_deref().map(str::parse).transpose()?;
                let members = read_active_members_tx(&tx)?;
                match cap_audience_by_live_scope_tx(
                    &tx,
                    &ScopeCapInput {
                        mode: self.mode,
                        raw_aud: &raw_live_aud,
                        author: &author,
                        ingester: ingester.as_ref(),
                        scope_id: scope_id.as_ref(),
                        members: &members,
                        strict_validate: false,
                    },
                ) {
                    Err(StoreError::PermissionDenied) => {
                        let key = ContentHash::digest(serde_json::to_vec(&(
                            destination,
                            "withdraw",
                            remote_id,
                        ))?);
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
                    }
                    Ok((effective_live_aud, live_scope_opt)) => {
                        let delivered_scope = template
                            .get("source_scope")
                            .cloned()
                            .and_then(|v| serde_json::from_value::<kp_model::SourceScope>(v).ok());
                        let has_newer_pending = has_undelivered_publish_for_identity_tx(
                            &tx,
                            destination,
                            &reference.origin_library,
                            &reference.logical_key,
                        )?;
                        if !has_newer_pending
                            && (&effective_live_aud != audience
                                || delivered_scope != live_scope_opt)
                        {
                            let next_rev = alloc_next_publish_revision_tx(
                                &tx,
                                destination,
                                &reference.origin_library,
                                &reference.logical_key,
                            )?;
                            let pub_sref = template
                                .get("fact")
                                .cloned()
                                .and_then(|v| serde_json::from_value::<Fact>(v).ok())
                                .map(|f| f.strong_ref())
                                .unwrap_or_else(|| reference.clone());
                            let new_idem = ContentHash::digest(serde_json::to_vec(&(
                                destination,
                                &author,
                                &pub_sref,
                                &effective_live_aud,
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
                                        serde_json::to_value(&effective_live_aud)?,
                                    );
                                }
                            }
                            if let Some(root_obj) = template.as_object_mut() {
                                if is_skill {
                                    root_obj.insert(
                                        "skill_audience".into(),
                                        serde_json::to_value(&effective_live_aud)?,
                                    );
                                }
                                match &live_scope_opt {
                                    Some(sc) => {
                                        root_obj.insert(
                                            "source_scope".into(),
                                            serde_json::to_value(sc)?,
                                        );
                                    }
                                    None => {
                                        root_obj.remove("source_scope");
                                    }
                                }
                                root_obj.insert("idempotency_key".into(), json!(new_idem));
                                root_obj.insert("audience_revision".into(), json!(next_rev));
                                root_obj.insert("revision".into(), json!(next_rev));
                                root_obj.insert("shrunk_from_inflight".into(), Value::Bool(true));
                                root_obj.remove("inflight_dispatched");
                            }
                            tx.execute(
                                "INSERT INTO outbox (dest_community_url,event_kind,idempotency_key,payload_json)
                                 VALUES (?1,'publish',?2,?3)
                                 ON CONFLICT(idempotency_key) DO UPDATE SET
                                     payload_json=excluded.payload_json,
                                     delivered_at=NULL,
                                     retry_count=0",
                                params![
                                    destination,
                                    new_idem,
                                    serde_json::to_string(&template)?
                                ],
                            )?;
                        }
                    }
                    Err(err) => return Err(err),
                }
            }
        }

        tx.commit()?;
        Ok(())
    }

    /// Applies an ordered, idempotent audience revision to an active published fact (or `support-{fact_id}`
    /// attestation for a `SkillVersion`) on a community server:
    /// - `revision < last_rev` -> `Ok(false)` (stale lower revision ignored).
    /// - `revision == last_rev` -> `Ok(false)` when `new_audience == &current_audience` (idempotent replay),
    ///   or `Err(StoreError::Conflict)` when `new_audience != &current_audience` (same-revision conflict).
    /// - `revision > last_rev` -> updates `audience_json` (if changed) and records `last_rev = revision`.
    pub fn exchange_revise_published_audience(
        &mut self,
        fact_id: &FactId,
        new_audience: &AudienceSpec,
        revision: u64,
        is_skill: bool,
        at: EpochMs,
    ) -> Result<bool> {
        if matches!(new_audience, AudienceSpec::SemiringDerived) {
            return Err(kp_model::KpError::invalid(
                "audience",
                "base audience required for revision",
            )
            .into());
        }
        let target_row_id = if is_skill {
            format!("support-{fact_id}")
        } else {
            fact_id.to_string()
        };
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
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
            tx.commit()?;
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
            tx.commit()?;
            return Ok(false);
        }
        if revision == last_rev {
            if new_audience == &current_audience {
                tx.commit()?;
                return Ok(false);
            }
            return Err(StoreError::Conflict);
        }

        let new_aud_json = serde_json::to_string(new_audience)?;
        let changed = new_audience != &current_audience;
        if changed {
            tx.execute(
                "UPDATE facts SET audience_json=?1 WHERE fact_id=?2",
                params![new_aud_json, target_row_id],
            )?;
            mark_alignment_dirty_tx(&tx, at)?;
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
        tx.commit()?;
        if is_skill && changed {
            self.sync_skills_now()?;
        }
        Ok(true)
    }

    /// Atomically accepts a first-time (or concurrently racing) `/v1/publish` on a remote/community
    /// server in ONE `TransactionBehavior::Immediate` transaction:
    /// - Re-checks tombstone & `withdrawn_version` barriers;
    /// - If `fact.id` already arrived concurrently, validates `strong_ref` and applies ordered
    ///   `exchange_revise_published_audience` (`409 Conflict` on same-revision audience mismatch);
    /// - Otherwise writes the base claim (or skill + attestation proof), records `publish_audience_rev`
    ///   (`revision.max(1)`), upserts concepts + `fact_concept` links, and records `origin_reference`
    ///   in the same transaction so a crash can NEVER leave a published fact with missing revision!
    pub fn exchange_accept_initial_publish(
        &mut self,
        write: InitialPublishWrite,
        spec: &AcceptInitialPublishSpec<'_>,
    ) -> Result<bool> {
        let AcceptInitialPublishSpec {
            pub_owner,
            pub_sref,
            redacted_origin_ref,
            requested_audience,
            revision,
            concepts,
            now,
        } = *spec;
        if matches!(requested_audience, AudienceSpec::SemiringDerived) {
            return Err(kp_model::KpError::invalid(
                "audience",
                "base audience required for publish",
            )
            .into());
        }
        let rev = revision.max(1);
        let (fact_id, expected_sref, is_skill) = match &write {
            InitialPublishWrite::Base { fact, .. } => (fact.id.clone(), fact.strong_ref(), false),
            InitialPublishWrite::Skill { skill_fact, .. } => {
                (skill_fact.id.clone(), skill_fact.strong_ref(), true)
            }
        };

        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let opaque = crate::tombstone_id(
            pub_sref.origin_library.as_str().as_bytes(),
            &pub_sref.origin_library,
            &pub_sref.logical_key,
        )?;
        if tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM tombstones WHERE tombstone_id=?1)",
            [opaque.as_str()],
            |r| r.get::<_, bool>(0),
        )? || is_version_withdrawn_for_publisher_tx(
            &tx,
            &pub_sref.origin_library,
            &pub_sref.logical_key,
            &pub_sref.content_sha256,
            pub_owner,
        )? || is_version_withdrawn_for_publisher_tx(
            &tx,
            &redacted_origin_ref.origin_library,
            &redacted_origin_ref.logical_key,
            &redacted_origin_ref.content_sha256,
            pub_owner,
        )? {
            return Err(StoreError::NotFound);
        }

        let existing_graph = read_graph(&tx)?;
        if let Some(existing) = existing_graph.facts.get(&fact_id) {
            let existing_publisher = existing.ingested_by.as_ref().unwrap_or(&existing.author);
            if existing_publisher != pub_owner {
                return Err(StoreError::PermissionDenied);
            }
            if existing.strong_ref() != expected_sref {
                return Err(
                    kp_model::KpError::invalid("publish", "immutable claim mismatch").into(),
                );
            }
            if existing.status != FactStatus::Active || existing.access.expired_at().is_some() {
                return Err(StoreError::NotFound);
            }
            let revised = exchange_revise_published_audience_tx(
                &tx,
                &existing.id,
                requested_audience,
                rev,
                is_skill,
                now,
            )?;
            for (c, allow_overwrite) in concepts {
                upsert_concept_tx(&tx, c, *allow_overwrite)?;
                link_fact_concept_tx(&tx, &existing.id, &c.id, now)?;
            }
            tx.commit()?;
            if is_skill && revised {
                self.sync_skills_now()?;
            }
            return Ok(revised);
        }

        let mut base_inputs_for_entities = Vec::new();
        match write {
            InitialPublishWrite::Base { fact, entities } => {
                let input = crate::FactInput {
                    fact: *fact,
                    entities,
                };
                crate::slots::remember_facts_indexed(
                    &tx,
                    std::slice::from_ref(&input),
                    &self.tokenizer,
                    self.embedder.as_ref(),
                )?;
                base_inputs_for_entities.push(input);
            }
            InitialPublishWrite::Skill {
                anchor,
                skill_fact,
                proof,
            } => {
                crate::maintenance::save_derivations_tx(
                    &tx,
                    &[*anchor, *skill_fact],
                    std::slice::from_ref(&*proof),
                )?;
            }
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
                    "revision": rev,
                    "audience": requested_audience,
                }))?,
                now.0
            ],
        )?;
        for (c, allow_overwrite) in concepts {
            upsert_concept_tx(&tx, c, *allow_overwrite)?;
            link_fact_concept_tx(&tx, &fact_id, &c.id, now)?;
        }
        write_origin_reference(&tx, &fact_id, redacted_origin_ref)?;
        mark_alignment_dirty_tx(&tx, now)?;
        tx.commit()?;

        if !base_inputs_for_entities.is_empty() {
            self.register_entities(&base_inputs_for_entities);
        }
        self.sync_skills_now()?;
        Ok(true)
    }
}
