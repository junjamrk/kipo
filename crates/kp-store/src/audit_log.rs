//! Viewer-scoped semiring-isolated audit log (`kp log` / [`Store::visible_change_log`]).
//!
//! Enforces strict non-interference across `change_log`:
//! - Requires `viewer` to hold an active, unrevoked library membership (`PermissionDenied` otherwise);
//! - Suppresses internal plumbing operations (`alignment_state`, `session_source_span`,
//!   `fact_concept`, `concept_seed`, `public_source_meta`, `raw_only_source`, etc.);
//! - Verifies live base `audience` + live `source_scopes` containment on active, superseded, and
//!   withdrawn facts, conjunctive premise visibility on derived conclusions, live chunk/container
//!   readability on raw sources, container membership or admin role on `container_roster_sync`,
//!   and strict personal ownership on `review` events.

use std::collections::{BTreeMap, BTreeSet};

use kp_model::{
    ActorId, AudienceSpec, DerivationView, EpochMs, Fact, FactAccess, FactId, FactStatus,
    MemberRole, SourceScopeId, VisibleView,
};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::exchange::parse_publication_payload;
use crate::export::{format_kp_ref, sanitize_md_inline};
use crate::facts::read_graph;
use crate::public_ingest::require_active_member_role_tx;
use crate::snapshot::render_fact_claim_text;
use crate::writeback::is_base_fact_readable_by_actor;
use crate::{Result, Store};

/// Viewer-scoped audit event returned by [`Store::visible_change_log`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeLogEntry {
    pub seq: i64,
    pub op: String,
    pub target_id: String,
    pub actor_id: Option<ActorId>,
    pub summary: String,
    pub created_at: EpochMs,
}

struct RawChangeLogRow {
    seq: i64,
    op: String,
    target_id: Option<String>,
    origin_library_id: Option<String>,
    logical_key: Option<String>,
    actor_id: Option<String>,
    payload_json: Option<String>,
    created_at: i64,
}

fn truncate_summary(raw: &str, max_chars: usize) -> String {
    let clean = sanitize_md_inline(raw);
    let chars: Vec<char> = clean.chars().collect();
    if chars.len() > max_chars {
        format!(
            "{}…",
            chars[..max_chars.saturating_sub(1)]
                .iter()
                .collect::<String>()
                .trim_end()
        )
    } else {
        clean
    }
}

pub(crate) fn normalize_target_filter(raw: Option<&str>) -> Option<String> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() {
        return None;
    }
    let stripped = trimmed.strip_prefix("kp-ref:").unwrap_or(trimmed).trim();
    (!stripped.is_empty()).then(|| stripped.to_owned())
}

pub(crate) fn matches_target_filter(filter: &str, candidates: &[&str]) -> bool {
    let bare_filter = filter.strip_prefix("fact_").unwrap_or(filter);
    for cand in candidates {
        let c = cand.trim();
        if c.is_empty() {
            continue;
        }
        if c == filter {
            return true;
        }
        let bare_cand = c.strip_prefix("fact_").unwrap_or(c);
        if (filter.starts_with("fact_") || c.starts_with("fact_")) && bare_cand == bare_filter {
            return true;
        }
    }
    false
}

pub(crate) fn is_fact_audit_visible(
    graph: &DerivationView,
    evaluated_live: &VisibleView,
    fact: &Fact,
    viewer: &ActorId,
    visiting: &mut BTreeSet<FactId>,
) -> bool {
    if fact.status == FactStatus::UnreachableHidden {
        return false;
    }
    match &fact.access {
        FactAccess::Base { .. } => is_base_fact_readable_by_actor(graph, fact, viewer),
        FactAccess::Derived { .. } => {
            if !kp_model::derived_scope_allows(fact, viewer) {
                return false;
            }
            if evaluated_live.facts.contains(&fact.id) {
                return true;
            }
            if let Some(scope_id) = &fact.source_scope_id {
                let Some(scope_aud) = graph.source_scopes.get(scope_id) else {
                    return false;
                };
                if !scope_aud.allows(viewer, &graph.members) {
                    return false;
                }
            }
            if !visiting.insert(fact.id.clone()) {
                return false;
            }
            let mut saw_proof = false;
            let mut visible = false;
            for just in graph
                .justifications
                .values()
                .filter(|j| j.conclusion == fact.id && !j.premises.is_empty())
            {
                saw_proof = true;
                let all_premises_visible = just.premises.iter().all(|prem| {
                    graph.facts.get(&prem.fact_id).is_some_and(|pf| {
                        is_fact_audit_visible(graph, evaluated_live, pf, viewer, visiting)
                    })
                });
                if all_premises_visible {
                    visible = true;
                    break;
                }
            }
            visiting.remove(&fact.id);
            if saw_proof {
                visible
            } else {
                fact.author == *viewer
            }
        }
    }
}

struct SourceAuditRow {
    uri: String,
    visibility_scope: String,
    ingested_by_actor: Option<String>,
    source_scope_id: Option<String>,
    is_tombstoned: bool,
}

fn is_source_audit_visible(
    tx: &Connection,
    graph: &DerivationView,
    source_id: &str,
    viewer: &ActorId,
    cache: &mut BTreeMap<String, Option<String>>,
) -> Result<Option<String>> {
    if let Some(cached) = cache.get(source_id) {
        return Ok(cached.clone());
    }

    let row: Option<SourceAuditRow> = tx
        .query_row(
            "SELECT s.uri,
                    s.visibility_scope,
                    s.ingested_by_actor,
                    s.source_scope_id,
                    EXISTS(SELECT 1 FROM tombstones t WHERE t.tombstone_id = s.source_id)
             FROM raw_sources s
             WHERE s.source_id = ?1",
            [source_id],
            |r| {
                Ok(SourceAuditRow {
                    uri: r.get(0)?,
                    visibility_scope: r.get(1)?,
                    ingested_by_actor: r.get(2)?,
                    source_scope_id: r.get(3)?,
                    is_tombstoned: r.get(4)?,
                })
            },
        )
        .optional()?;

    let Some(src_row) = row else {
        cache.insert(source_id.to_owned(), None);
        return Ok(None);
    };
    if src_row.is_tombstoned {
        cache.insert(source_id.to_owned(), None);
        return Ok(None);
    }

    if let Some(scope_str) = src_row.source_scope_id {
        let Ok(scope_id) = scope_str.parse::<SourceScopeId>() else {
            cache.insert(source_id.to_owned(), None);
            return Ok(None);
        };
        let Some(scope_aud) = graph.source_scopes.get(&scope_id) else {
            cache.insert(source_id.to_owned(), None);
            return Ok(None);
        };
        if !scope_aud.allows(viewer, &graph.members) {
            cache.insert(source_id.to_owned(), None);
            return Ok(None);
        }
    }

    let mut chunk_stmt =
        tx.prepare_cached("SELECT readable_by_json FROM chunks WHERE source_id = ?1")?;
    let mut chunk_rows = chunk_stmt.query([source_id])?;
    let mut saw_chunk = false;
    let mut any_chunk_readable = false;
    while let Some(crow) = chunk_rows.next()? {
        saw_chunk = true;
        let aud_raw: String = crow.get(0)?;
        if let Ok(aud) = serde_json::from_str::<AudienceSpec>(&aud_raw) {
            if aud.allows(viewer, &graph.members) {
                any_chunk_readable = true;
                break;
            }
        }
    }

    let allowed =
        if any_chunk_readable || src_row.ingested_by_actor.as_deref() == Some(viewer.as_str()) {
            true
        } else if !saw_chunk {
            serde_json::from_str::<AudienceSpec>(&src_row.visibility_scope)
                .is_ok_and(|aud| aud.allows(viewer, &graph.members))
        } else {
            false
        };

    let res = allowed.then_some(src_row.uri);
    cache.insert(source_id.to_owned(), res.clone());
    Ok(res)
}

fn resolve_fact_by_identity<'a>(
    graph: &'a DerivationView,
    origin_lib: &str,
    logical_key: &str,
) -> Option<&'a Fact> {
    graph
        .facts
        .values()
        .find(|f| f.origin_library.as_str() == origin_lib && f.logical_key.as_str() == logical_key)
}

impl Store {
    /// Returns up to `limit` viewer-visible audit entries from `change_log` ordered by `seq DESC`.
    ///
    /// - Returns `Err(StoreError::PermissionDenied)` if `viewer` is not an active library member;
    /// - Filters out internal bookkeeping events (`alignment_state`, `session_source_span`,
    ///   `fact_concept`, `concept_seed`, `public_source_meta`, `raw_only_source`, etc.);
    /// - Never leaks private facts, departed/restricted containers, private premises of derived
    ///   facts, unreadable sources, or other members' review items.
    pub fn visible_change_log(
        &self,
        viewer: &ActorId,
        at: EpochMs,
        limit: usize,
        target_filter: Option<&str>,
    ) -> Result<Vec<ChangeLogEntry>> {
        let tx = self.db.unchecked_transaction()?;
        let caller_role = require_active_member_role_tx(&tx, viewer)?;
        if limit == 0 {
            tx.commit()?;
            return Ok(Vec::new());
        }
        let is_admin = matches!(caller_role, MemberRole::Owner | MemberRole::Maintainer);
        let clean_filter = normalize_target_filter(target_filter);

        let graph = read_graph(&tx)?;
        let evaluated_live = graph.evaluate(viewer, at);
        let mut source_vis_cache = BTreeMap::<String, Option<String>>::new();

        let mut stmt = tx.prepare(
            "SELECT seq, op, target_id, origin_library_id, logical_key, actor_id, payload_json, created_at
             FROM change_log
             WHERE op NOT IN (
                 'alignment_state',
                 'session_source_span',
                 'fact_concept',
                 'concept_seed',
                 'public_source_meta',
                 'raw_only_source',
                 'embedding_migration',
                 'shared_identity',
                 'sync_snapshot',
                 'sync_cursor',
                 'origin_owner',
                 'origin_reference',
                 'publish_rev_seq',
                 'publish_audience_rev',
                 'withdrawn_version',
                 'horn_rule',
                 'hook_failure',
                 'dream_mined_source'
             )
             ORDER BY seq DESC",
        )?;

        let mut rows = stmt.query([])?;
        let mut entries = Vec::with_capacity(limit.min(64));

        while let Some(r) = rows.next()? {
            if entries.len() >= limit {
                break;
            }
            let raw = RawChangeLogRow {
                seq: r.get(0)?,
                op: r.get(1)?,
                target_id: r.get(2)?,
                origin_library_id: r.get(3)?,
                logical_key: r.get(4)?,
                actor_id: r.get(5)?,
                payload_json: r.get(6)?,
                created_at: r.get(7)?,
            };

            let parsed_actor = raw
                .actor_id
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .and_then(|s| s.parse::<ActorId>().ok());

            let maybe_entry: Option<ChangeLogEntry> = match raw.op.as_str() {
                "insert_fact"
                | "writeback_supersede"
                | "forget"
                | "reject"
                | "source_withdrawn"
                | "source_superseded"
                | "publication"
                | "allow_guard_once" => {
                    let target_fact_opt: Option<&Fact> = raw
                        .target_id
                        .as_deref()
                        .and_then(|tid| {
                            tid.parse::<FactId>()
                                .ok()
                                .and_then(|fid| graph.facts.get(&fid))
                                .or_else(|| {
                                    tid.strip_prefix("fact_")
                                        .and_then(|b| b.parse::<FactId>().ok())
                                        .and_then(|fid| graph.facts.get(&fid))
                                })
                        })
                        .or_else(|| {
                            let (orig, lkey) = (
                                raw.origin_library_id.as_deref()?,
                                raw.logical_key.as_deref()?,
                            );
                            resolve_fact_by_identity(&graph, orig, lkey)
                        });

                    let Some(fact) = target_fact_opt else {
                        continue;
                    };

                    if raw.op == "allow_guard_once"
                        && !is_admin
                        && parsed_actor.as_ref() != Some(viewer)
                    {
                        continue;
                    }

                    if !is_fact_audit_visible(
                        &graph,
                        &evaluated_live,
                        fact,
                        viewer,
                        &mut BTreeSet::new(),
                    ) {
                        continue;
                    }

                    let payload_val = raw
                        .payload_json
                        .as_deref()
                        .and_then(|p| serde_json::from_str::<Value>(p).ok());
                    let old_fid_str = payload_val
                        .as_ref()
                        .and_then(|v| v.get("old_fact_id"))
                        .and_then(Value::as_str);

                    if let Some(old_str) = old_fid_str {
                        if let Ok(old_fid) = old_str.parse::<FactId>() {
                            if let Some(old_fact) = graph.facts.get(&old_fid) {
                                if !is_fact_audit_visible(
                                    &graph,
                                    &evaluated_live,
                                    old_fact,
                                    viewer,
                                    &mut BTreeSet::new(),
                                ) {
                                    continue;
                                }
                            }
                        }
                    }

                    let kp_ref = format_kp_ref(&fact.id);
                    if let Some(ref flt) = clean_filter {
                        let cands = [
                            fact.id.as_str(),
                            kp_ref.as_str(),
                            raw.logical_key.as_deref().unwrap_or(""),
                            old_fid_str.unwrap_or(""),
                        ];
                        if !matches_target_filter(flt, &cands) {
                            continue;
                        }
                    }

                    let claim = truncate_summary(&render_fact_claim_text(&tx, fact)?, 120);
                    let summary = match raw.op.as_str() {
                        "insert_fact" => {
                            format!("inserted {} `{kp_ref}`: {claim}", fact.kind.as_str())
                        }
                        "writeback_supersede" => {
                            let prev = old_fid_str
                                .and_then(|s| s.parse::<FactId>().ok())
                                .map(|id| format_kp_ref(&id))
                                .unwrap_or_else(|| "prior".to_owned());
                            format!("writeback updated `{prev}` -> `{kp_ref}`: {claim}")
                        }
                        "forget" => {
                            format!("withdrew {} `{kp_ref}`: {claim}", fact.kind.as_str())
                        }
                        "reject" => {
                            let reason = payload_val
                                .as_ref()
                                .and_then(Value::as_str)
                                .map(|s| truncate_summary(s, 80))
                                .unwrap_or_default();
                            if reason.is_empty() {
                                format!("rejected {} `{kp_ref}`", fact.kind.as_str())
                            } else {
                                format!("rejected {} `{kp_ref}` ({reason})", fact.kind.as_str())
                            }
                        }
                        "source_withdrawn" => {
                            format!(
                                "source withdrawal retired {} `{kp_ref}`",
                                fact.kind.as_str()
                            )
                        }
                        "source_superseded" => {
                            format!("source supersede retired {} `{kp_ref}`", fact.kind.as_str())
                        }
                        "publication" => {
                            let dest = raw
                                .payload_json
                                .as_deref()
                                .and_then(|p| parse_publication_payload(p).ok())
                                .map(|t| t.0)
                                .unwrap_or_else(|| "community".to_owned());
                            format!("published `{kp_ref}` to `{}`", sanitize_md_inline(&dest))
                        }
                        "allow_guard_once" => {
                            format!("granted single-use guard allow for `{kp_ref}`")
                        }
                        _ => continue,
                    };

                    let effective_actor = parsed_actor.or_else(|| Some(fact.author.clone()));
                    Some(ChangeLogEntry {
                        seq: raw.seq,
                        op: raw.op,
                        target_id: fact.id.to_string(),
                        actor_id: effective_actor,
                        summary,
                        created_at: EpochMs(raw.created_at),
                    })
                }
                "ingest_source" => {
                    let Some(source_id) = raw.target_id.as_deref() else {
                        continue;
                    };
                    let Some(uri) = is_source_audit_visible(
                        &tx,
                        &graph,
                        source_id,
                        viewer,
                        &mut source_vis_cache,
                    )?
                    else {
                        continue;
                    };
                    if let Some(ref flt) = clean_filter {
                        let cands = [source_id, raw.logical_key.as_deref().unwrap_or("")];
                        if !matches_target_filter(flt, &cands) {
                            continue;
                        }
                    }
                    let effective_actor = match parsed_actor {
                        Some(a) => Some(a),
                        None => tx
                            .query_row(
                                "SELECT ingested_by_actor FROM raw_sources WHERE source_id = ?1",
                                [source_id],
                                |r| r.get::<_, Option<String>>(0),
                            )
                            .optional()?
                            .flatten()
                            .and_then(|s| s.parse::<ActorId>().ok()),
                    };
                    Some(ChangeLogEntry {
                        seq: raw.seq,
                        op: raw.op,
                        target_id: source_id.to_owned(),
                        actor_id: effective_actor,
                        summary: format!(
                            "ingested source `{source_id}` (`{}`)",
                            truncate_summary(&uri, 96)
                        ),
                        created_at: EpochMs(raw.created_at),
                    })
                }
                "public_source_withdrawn" => {
                    let ext_key = raw
                        .logical_key
                        .as_deref()
                        .or(raw.target_id.as_deref())
                        .unwrap_or("");
                    if ext_key.is_empty() {
                        continue;
                    }
                    let matching_sids: Vec<String> = {
                        let mut s_stmt = tx.prepare_cached(
                            "SELECT source_id FROM raw_sources
                             WHERE external_source_key = ?1
                             ORDER BY captured_at DESC, source_id ASC",
                        )?;
                        let s_rows = s_stmt.query_map([ext_key], |r| r.get::<_, String>(0))?;
                        s_rows.collect::<std::result::Result<_, _>>()?
                    };

                    let mut visible_sid: Option<String> = None;
                    if matching_sids.is_empty() {
                        if parsed_actor.as_ref() != Some(viewer) {
                            continue;
                        }
                    } else {
                        for sid in &matching_sids {
                            if is_source_audit_visible(
                                &tx,
                                &graph,
                                sid,
                                viewer,
                                &mut source_vis_cache,
                            )?
                            .is_some()
                            {
                                visible_sid = Some(sid.clone());
                                break;
                            }
                        }
                        if visible_sid.is_none() {
                            continue;
                        }
                    }

                    let primary_target = visible_sid.unwrap_or_else(|| ext_key.to_owned());
                    if let Some(ref flt) = clean_filter {
                        if !matches_target_filter(flt, &[primary_target.as_str(), ext_key]) {
                            continue;
                        }
                    }
                    Some(ChangeLogEntry {
                        seq: raw.seq,
                        op: raw.op,
                        target_id: primary_target,
                        actor_id: parsed_actor,
                        summary: format!("withdrew source `{}`", truncate_summary(ext_key, 96)),
                        created_at: EpochMs(raw.created_at),
                    })
                }
                "container_roster_sync" => {
                    let Some(scope_str) = raw.target_id.as_deref() else {
                        continue;
                    };
                    let Ok(scope_id) = scope_str.parse::<SourceScopeId>() else {
                        continue;
                    };
                    let in_container = graph
                        .source_scopes
                        .get(&scope_id)
                        .is_some_and(|aud| aud.allows(viewer, &graph.members));
                    if !is_admin && !in_container {
                        continue;
                    }
                    if let Some(ref flt) = clean_filter {
                        if !matches_target_filter(flt, &[scope_id.as_str()]) {
                            continue;
                        }
                    }
                    let label_opt: Option<String> = tx
                        .query_row(
                            "SELECT label FROM source_scopes WHERE scope_id = ?1",
                            [scope_id.as_str()],
                            |r| r.get(0),
                        )
                        .optional()?;
                    let summary = match label_opt.filter(|l| !l.trim().is_empty()) {
                        Some(lbl) => format!(
                            "synced container `{scope_id}` (`{}`)",
                            truncate_summary(&lbl, 80)
                        ),
                        None => format!("synced container `{scope_id}` roster"),
                    };
                    Some(ChangeLogEntry {
                        seq: raw.seq,
                        op: raw.op,
                        target_id: scope_id.to_string(),
                        actor_id: parsed_actor,
                        summary,
                        created_at: EpochMs(raw.created_at),
                    })
                }
                "enqueue_review" | "resolve_review" => {
                    let Some(rev_id) = raw.target_id.as_deref() else {
                        continue;
                    };
                    let owner_opt: Option<(String, String, String)> = tx
                        .query_row(
                            "SELECT actor_id, kind, target_id FROM review_items WHERE review_id = ?1",
                            [rev_id],
                            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                        )
                        .optional()?;
                    let (belongs_to_viewer, rev_kind_opt, rev_target_opt) = match owner_opt {
                        Some((owner_str, kind_str, rtarget)) => {
                            (owner_str == viewer.as_str(), Some(kind_str), Some(rtarget))
                        }
                        None => (parsed_actor.as_ref() == Some(viewer), None, None),
                    };
                    if !belongs_to_viewer {
                        continue;
                    }
                    if let Some(ref flt) = clean_filter {
                        let cands = [rev_id, rev_target_opt.as_deref().unwrap_or("")];
                        if !matches_target_filter(flt, &cands) {
                            continue;
                        }
                    }
                    let summary = if raw.op == "enqueue_review" {
                        let kind_lbl = rev_kind_opt.unwrap_or_else(|| "review".to_owned());
                        format!("enqueued `{kind_lbl}` review `{rev_id}`")
                    } else {
                        let detail = raw
                            .payload_json
                            .as_deref()
                            .and_then(|p| serde_json::from_str::<Value>(p).ok())
                            .and_then(|v| {
                                v.as_str()
                                    .map(str::to_owned)
                                    .or_else(|| {
                                        v.get("status").and_then(Value::as_str).map(str::to_owned)
                                    })
                                    .or_else(|| {
                                        v.get("choice").and_then(Value::as_str).map(str::to_owned)
                                    })
                            })
                            .unwrap_or_else(|| "resolved".to_owned());
                        format!(
                            "resolved review `{rev_id}` ({})",
                            truncate_summary(&detail, 48)
                        )
                    };
                    Some(ChangeLogEntry {
                        seq: raw.seq,
                        op: raw.op,
                        target_id: rev_id.to_owned(),
                        actor_id: Some(viewer.clone()),
                        summary,
                        created_at: EpochMs(raw.created_at),
                    })
                }
                _ => None,
            };

            if let Some(entry) = maybe_entry {
                entries.push(entry);
            }
        }

        drop(rows);
        drop(stmt);
        tx.commit()?;
        Ok(entries)
    }
}
