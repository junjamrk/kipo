use std::collections::BTreeSet;

use kp_model::{
    ActorId, AudienceSpec, EpochMs, FactAccess, FactId, FactKind, FactStatus, KpError, MemberRole,
    ModelRoutePolicy, ReviewId, ReviewKind, ReviewStatus,
};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};

use super::parser::{build_updated_fact_object, parse_kp_entry_marker};
use super::tx_ops::{
    follow_superseded_head, insert_standalone_writeback_fact_tx, supersede_fact_via_writeback_tx,
    StandaloneWritebackFactSpec, WritebackTxEnv,
};
use super::{default_policy_from_env, is_base_fact_readable_by_actor, merge_route_policy};
use crate::audit_log::is_fact_audit_visible;
use crate::export::sanitize_md_inline;
use crate::facts::{fact_effective_route_policy_in_tx, read_graph};
use crate::governance::read_review_row;
use crate::public_ingest::{require_active_member_role_tx, shared_redactor};
use crate::slots::withdraw_facts;
use crate::{Result, Store, StoreError};

impl Store {
    /// Applies an open [`ReviewKind::WritebackProposal`] confirmed by its owning `actor`:
    /// - `"edit_fact"`: follows any `superseded_by` chain to the current head fact, verifies base
    ///   visibility and edit authorization (`author == actor` or `Owner | Maintainer`), supersedes
    ///   active targets (or writes a new anchored fact if already retired / derived note) while
    ///   honoring `RawOnly` zero-cloud-embedder gating, marks the review `Confirmed`, and returns
    ///   `Ok(Some(new_fact_id))`;
    /// - `"delete_fact"`: verifies base visibility and authorization, withdraws the target base
    ///   fact, marks the review `Confirmed`, and returns `Ok(None)`;
    /// - `"freeform_block"`: persists a personal `Explicit([actor])` `writeback:` L0 source and an
    ///   anchored `FactKind::Observation` (`kp.core.note`) fact, marks the review `Confirmed`, and
    ///   returns `Ok(Some(new_fact_id))`.
    pub fn apply_confirmed_writeback_proposal(
        &mut self,
        review_id: &ReviewId,
        actor: &ActorId,
        at: EpochMs,
    ) -> Result<Option<FactId>> {
        let default_policy = default_policy_from_env();
        self.apply_confirmed_writeback_proposal_with_policy(review_id, actor, &default_policy, at)
    }

    /// Policy-explicit variant of [`Store::apply_confirmed_writeback_proposal`]: honors BOTH
    /// `payload.route_policy` on the queued review item AND `default_policy` (`RawOnly` wins
    /// unconditionally to prevent cloud embedder calls).
    pub fn apply_confirmed_writeback_proposal_with_policy(
        &mut self,
        review_id: &ReviewId,
        actor: &ActorId,
        default_policy: &ModelRoutePolicy,
        at: EpochMs,
    ) -> Result<Option<FactId>> {
        let redactor = shared_redactor()?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let caller_role = require_active_member_role_tx(&tx, actor)?;
        let is_admin = matches!(caller_role, MemberRole::Owner | MemberRole::Maintainer);

        let existing = tx
            .query_row(
                "SELECT review_id, actor_id, kind, target_id, question, options_json, payload_json, status, created_at, resolved_at
                 FROM review_items
                 WHERE review_id = ?1",
                [review_id.as_str()],
                read_review_row,
            )
            .optional()?
            .transpose()?
            .ok_or(StoreError::NotFound)?;

        if existing.actor != *actor {
            return Err(StoreError::PermissionDenied);
        }
        if existing.kind != ReviewKind::WritebackProposal {
            return Err(KpError::invalid(
                "review_kind",
                "expected `writeback_proposal` review item",
            )
            .into());
        }
        if existing.status != ReviewStatus::Open {
            return Err(StoreError::Conflict);
        }

        let op_str = existing
            .payload
            .get("proposal_op")
            .and_then(Value::as_str)
            .unwrap_or("freeform_block");
        let payload_policy = existing.payload.get("route_policy").and_then(|v| {
            if v.is_null() {
                None
            } else if v.as_str() == Some("raw_only") {
                Some(ModelRoutePolicy::RawOnly)
            } else {
                serde_json::from_value::<ModelRoutePolicy>(v.clone()).ok()
            }
        });
        let graph = read_graph(&tx)?;

        let result_fact_id = match op_str {
            "edit_fact" => {
                let target_str = existing
                    .payload
                    .get("target_fact_id")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| {
                        existing
                            .target_id
                            .split_once('#')
                            .map_or(existing.target_id.as_str(), |(base, _)| base)
                    });
                let target_fid: FactId = target_str.parse()?;
                let orig_fact = graph.facts.get(&target_fid).ok_or(StoreError::NotFound)?;
                let head_fact = follow_superseded_head(&graph, orig_fact);

                let raw_claim = existing
                    .payload
                    .get("proposed_claim")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_owned)
                    .or_else(|| {
                        existing
                            .payload
                            .get("raw_markdown")
                            .and_then(Value::as_str)
                            .map(|raw| {
                                raw.lines()
                                    .filter(|l| parse_kp_entry_marker(l).is_none())
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            })
                    })
                    .unwrap_or_default();
                let redacted_claim = sanitize_md_inline(&redactor.redact(&raw_claim));
                if redacted_claim.is_empty() {
                    return Err(KpError::invalid(
                        "proposed_claim",
                        "writeback proposal claim text must be nonempty",
                    )
                    .into());
                }

                match &head_fact.access {
                    FactAccess::Base { audience, .. } => {
                        if !is_base_fact_readable_by_actor(&graph, head_fact, actor) {
                            return Err(StoreError::PermissionDenied);
                        }
                        if head_fact.author != *actor && !is_admin {
                            return Err(StoreError::PermissionDenied);
                        }
                        let head_pol = fact_effective_route_policy_in_tx(&tx, head_fact)?;
                        let orig_policy = merge_route_policy(
                            Some(&merge_route_policy(payload_policy.as_ref(), &head_pol)),
                            default_policy,
                        );
                        if head_fact.status == FactStatus::Active
                            && head_fact.access.expired_at().is_none()
                        {
                            if let Some(updated_obj) = build_updated_fact_object(
                                &tx,
                                head_fact,
                                &redacted_claim,
                                &orig_policy,
                            )? {
                                let wb_env = WritebackTxEnv {
                                    library_id: &self.library_id,
                                    actor,
                                    at,
                                    tokenizer: &self.tokenizer,
                                    embedder: self.embedder.as_ref(),
                                };
                                let update = supersede_fact_via_writeback_tx(
                                    &tx,
                                    &wb_env,
                                    head_fact,
                                    updated_obj,
                                    &redacted_claim,
                                    &orig_policy,
                                )?;
                                Some(update.new_fact_id)
                            } else {
                                let wb_env = WritebackTxEnv {
                                    library_id: &self.library_id,
                                    actor,
                                    at,
                                    tokenizer: &self.tokenizer,
                                    embedder: self.embedder.as_ref(),
                                };
                                let new_id = insert_standalone_writeback_fact_tx(
                                    &tx,
                                    &wb_env,
                                    StandaloneWritebackFactSpec {
                                        kind: FactKind::Observation,
                                        subject: head_fact.subject.clone(),
                                        predicate: "kp.core.note".to_owned(),
                                        object: Value::String(redacted_claim.clone()),
                                        audience: audience.clone(),
                                        source_scope_id: head_fact.source_scope_id.clone(),
                                        governed_scope: head_fact.governed_scope.clone(),
                                        redacted_body: &redacted_claim,
                                        policy: &orig_policy,
                                    },
                                )?;
                                Some(new_id)
                            }
                        } else {
                            let (kind, predicate, obj) = if let Some(updated_obj) =
                                build_updated_fact_object(
                                    &tx,
                                    head_fact,
                                    &redacted_claim,
                                    &orig_policy,
                                )? {
                                (head_fact.kind, head_fact.predicate.clone(), updated_obj)
                            } else {
                                (
                                    FactKind::Observation,
                                    "kp.core.note".to_owned(),
                                    Value::String(redacted_claim.clone()),
                                )
                            };
                            let wb_env = WritebackTxEnv {
                                library_id: &self.library_id,
                                actor,
                                at,
                                tokenizer: &self.tokenizer,
                                embedder: self.embedder.as_ref(),
                            };
                            let new_id = insert_standalone_writeback_fact_tx(
                                &tx,
                                &wb_env,
                                StandaloneWritebackFactSpec {
                                    kind,
                                    subject: head_fact.subject.clone(),
                                    predicate,
                                    object: obj,
                                    audience: audience.clone(),
                                    source_scope_id: head_fact.source_scope_id.clone(),
                                    governed_scope: head_fact.governed_scope.clone(),
                                    redacted_body: &redacted_claim,
                                    policy: &orig_policy,
                                },
                            )?;
                            Some(new_id)
                        }
                    }
                    FactAccess::Derived { .. } => {
                        let evaluated = graph.evaluate(actor, at);
                        if !is_fact_audit_visible(
                            &graph,
                            &evaluated,
                            orig_fact,
                            actor,
                            &mut BTreeSet::new(),
                        ) {
                            return Err(StoreError::PermissionDenied);
                        }
                        let derived_pol = fact_effective_route_policy_in_tx(&tx, orig_fact)?;
                        let effective_policy = merge_route_policy(
                            Some(&merge_route_policy(payload_policy.as_ref(), &derived_pol)),
                            default_policy,
                        );
                        let self_aud = AudienceSpec::Explicit(BTreeSet::from([actor.clone()]));
                        let wb_env = WritebackTxEnv {
                            library_id: &self.library_id,
                            actor,
                            at,
                            tokenizer: &self.tokenizer,
                            embedder: self.embedder.as_ref(),
                        };
                        let new_id = insert_standalone_writeback_fact_tx(
                            &tx,
                            &wb_env,
                            StandaloneWritebackFactSpec {
                                kind: FactKind::Observation,
                                subject: orig_fact.subject.clone(),
                                predicate: "kp.core.note".to_owned(),
                                object: Value::String(redacted_claim.clone()),
                                audience: self_aud,
                                source_scope_id: None,
                                governed_scope: None,
                                redacted_body: &redacted_claim,
                                policy: &effective_policy,
                            },
                        )?;
                        Some(new_id)
                    }
                }
            }
            "delete_fact" => {
                let target_str = existing
                    .payload
                    .get("target_fact_id")
                    .and_then(Value::as_str)
                    .unwrap_or(existing.target_id.as_str());
                let target_fid: FactId = target_str.parse()?;
                let orig_fact = graph.facts.get(&target_fid).ok_or(StoreError::NotFound)?;
                let target_fact = if orig_fact.status == FactStatus::Superseded {
                    follow_superseded_head(&graph, orig_fact)
                } else {
                    orig_fact
                };
                if matches!(target_fact.access, FactAccess::Derived { .. }) {
                    return Err(KpError::invalid(
                        "fact_access",
                        "derived facts cannot be directly withdrawn; withdraw a supporting premise",
                    )
                    .into());
                }
                if !is_base_fact_readable_by_actor(&graph, target_fact, actor) {
                    return Err(StoreError::PermissionDenied);
                }
                if target_fact.author != *actor && !is_admin {
                    return Err(StoreError::PermissionDenied);
                }
                if target_fact.status != FactStatus::Withdrawn {
                    let _ = withdraw_facts(&tx, vec![target_fact.id.to_string()], actor, at)?;
                }
                None
            }
            "freeform_block" => {
                let raw_md = existing
                    .payload
                    .get("raw_markdown")
                    .or_else(|| existing.payload.get("proposed_claim"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let redacted_body = redactor.redact(raw_md.trim());
                let proposed_inline = existing
                    .payload
                    .get("proposed_claim")
                    .and_then(Value::as_str)
                    .map(|s| sanitize_md_inline(&redactor.redact(s)))
                    .filter(|s| !s.is_empty());
                let inline_claim =
                    proposed_inline.unwrap_or_else(|| sanitize_md_inline(&redacted_body));
                if inline_claim.is_empty() {
                    return Err(KpError::invalid(
                        "raw_markdown",
                        "freeform writeback block must be nonempty",
                    )
                    .into());
                }
                let policy = merge_route_policy(payload_policy.as_ref(), default_policy);
                let self_aud = AudienceSpec::Explicit(BTreeSet::from([actor.clone()]));
                let wb_env = WritebackTxEnv {
                    library_id: &self.library_id,
                    actor,
                    at,
                    tokenizer: &self.tokenizer,
                    embedder: self.embedder.as_ref(),
                };
                let new_id = insert_standalone_writeback_fact_tx(
                    &tx,
                    &wb_env,
                    StandaloneWritebackFactSpec {
                        kind: FactKind::Observation,
                        subject: "note".to_owned(),
                        predicate: "kp.core.note".to_owned(),
                        object: Value::String(inline_claim),
                        audience: self_aud,
                        source_scope_id: None,
                        governed_scope: None,
                        redacted_body: &redacted_body,
                        policy: &policy,
                    },
                )?;
                Some(new_id)
            }
            _other => {
                return Err(
                    KpError::invalid("proposal_op", "unknown writeback proposal_op").into(),
                );
            }
        };

        tx.execute(
            "UPDATE review_items SET status = 'confirmed', resolved_at = ?1 WHERE review_id = ?2",
            params![at.0, review_id.as_str()],
        )?;
        tx.execute(
            "INSERT INTO change_log (op, target_id, actor_id, payload_json, created_at)
             VALUES ('resolve_review', ?1, ?2, ?3, ?4)",
            params![
                review_id.as_str(),
                actor.as_str(),
                serde_json::to_string(&json!({
                    "status": ReviewStatus::Confirmed,
                    "proposal_op": op_str,
                    "replacement_fact_id": result_fact_id,
                }))?,
                at.0,
            ],
        )?;
        tx.commit()?;
        self.sync_skills_now()?;
        Ok(result_fact_id)
    }
}
