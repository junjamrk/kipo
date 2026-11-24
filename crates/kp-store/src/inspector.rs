//! Read-only semiring-isolated query primitives for the `127.0.0.1` Web Inspector (`M4`).
//!
//! Provides SQL-free viewer-scoped projections across the four Web Inspector columns:
//! 1. [`Store::inspector_visible_facts`] — active memories, rules, decisions, and warnings;
//! 2. [`Store::inspector_fact_highlights`] — Unicode-safe verbatim quote context slicing gated by
//!    live source/chunk/container readability and tombstone checks;
//! 3. [`Store::inspector_timeline`] — unified change timeline combining viewer-visible `change_log`
//!    events with time-driven expirations (`expire`), supersessions (`supersede`), and derived
//!    premise invalidations (`stale_premise`).

use std::collections::{BTreeMap, BTreeSet};

use kp_model::{
    verify_verbatim_quote, ActorId, AudienceSpec, CharSpan, DerivationView, EpochMs, Fact,
    FactAccess, FactId, FactKind, FactStatus, QuoteLocationHint, SourceId, SourceScopeId,
};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::audit_log::{is_fact_audit_visible, matches_target_filter, normalize_target_filter};
use crate::export::{format_kp_ref, sanitize_md_inline};
use crate::facts::read_graph;
use crate::public_ingest::require_active_member_role_tx;
use crate::snapshot::{
    lookup_scope_label, render_fact_claim_text, resolve_fact_source_label,
    strip_turn_tags_and_assistant_lines,
};
use crate::{Result, Store};

const HIGHLIGHT_CONTEXT_CHARS: usize = 120;

/// Column ① ("What Kipo Remembers") viewer-scoped card returned by [`Store::inspector_visible_facts`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectorFactCard {
    pub fact_id: FactId,
    pub kp_ref: String,
    pub kind: FactKind,
    pub subject: String,
    pub predicate: String,
    pub claim: String,
    pub author: ActorId,
    pub is_derived: bool,
    pub recorded_at: EpochMs,
    pub valid_from: Option<EpochMs>,
    pub valid_until: Option<EpochMs>,
    pub source_label: String,
    pub container_id: Option<SourceScopeId>,
    pub container_label: Option<String>,
    pub unverified_gaps: Vec<String>,
}

/// Column ② ("Why Kipo Remembers: Verbatim Quote Highlight") returned by [`Store::inspector_fact_highlights`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectorQuoteHighlight {
    pub fact_id: FactId,
    pub chunk_id: Option<String>,
    pub source_id: Option<SourceId>,
    pub source_uri: Option<String>,
    pub source_label: String,
    pub verbatim_quote: Option<String>,
    pub char_span: Option<CharSpan>,
    pub excerpt_prefix: String,
    pub excerpt_highlighted: String,
    pub excerpt_suffix: String,
}

/// Column ③ ("What Changed Later: Supersede, Withdraw, Expire Timeline") returned by [`Store::inspector_timeline`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectorTimelineEntry {
    pub seq: i64,
    pub category: String,
    pub op: String,
    pub target_id: String,
    pub actor_id: Option<ActorId>,
    pub summary: String,
    pub occurred_at: EpochMs,
}

struct LoadedChunkSource {
    chunk_id: String,
    source_id: SourceId,
    source_uri: String,
    body: String,
    self_echo_filtered: bool,
}

struct RawChunkQueryRow {
    chunk_id: String,
    source_id_str: String,
    source_uri: String,
    body: String,
    chunk_readable_json: String,
    source_visibility_json: String,
    source_scope_id: Option<String>,
    self_echo_filtered: bool,
    is_tombstoned: bool,
}

fn truncate_inline(raw: &str, max_chars: usize) -> String {
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

fn has_transcript_role_or_turn_tags(text: &str) -> bool {
    text.contains("[turn:")
        || text.contains("[speaker:")
        || text.contains("[assistant]")
        || text.contains("[user]")
        || text.contains("[system]")
        || text.contains("[developer]")
        || text.contains("[tool]")
}

fn find_exact_char_subslice(haystack: &[char], needle: &[char]) -> Option<(usize, usize)> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    let limit = haystack.len() - needle.len();
    for start in 0..=limit {
        if haystack[start..start + needle.len()] == *needle {
            return Some((start, start + needle.len()));
        }
    }
    None
}

fn slice_highlight_context(
    body_text: &str,
    stored_start: Option<u32>,
    stored_end: Option<u32>,
    quote_opt: Option<&str>,
    needs_transcript_strip: bool,
) -> (Option<CharSpan>, String, String, String) {
    let effective_body = if needs_transcript_strip {
        strip_turn_tags_and_assistant_lines(body_text)
    } else {
        body_text.to_owned()
    };
    let effective_quote_owned = quote_opt.map(|q| {
        if needs_transcript_strip || has_transcript_role_or_turn_tags(q) {
            strip_turn_tags_and_assistant_lines(q)
        } else {
            q.to_owned()
        }
    });
    let effective_quote = effective_quote_owned
        .as_deref()
        .filter(|q| !q.trim().is_empty());

    let body_chars: Vec<char> = effective_body.chars().collect();
    if body_chars.is_empty() {
        return (None, String::new(), String::new(), String::new());
    }

    let mut resolved_span: Option<(usize, usize)> = None;

    if !needs_transcript_strip {
        if let (Some(s_u32), Some(e_u32)) = (stored_start, stored_end) {
            let s = s_u32 as usize;
            let e = e_u32 as usize;
            if s < e && e <= body_chars.len() {
                let candidate: String = body_chars[s..e].iter().collect();
                let matches_quote = match effective_quote {
                    None => true,
                    Some(q) => {
                        candidate == q
                            || verify_verbatim_quote(&candidate, q, QuoteLocationHint::default())
                                .is_ok()
                    }
                };
                if matches_quote {
                    resolved_span = Some((s, e));
                }
            }
        }
    }

    if resolved_span.is_none() {
        if let Some(q) = effective_quote {
            let q_chars: Vec<char> = q.chars().collect();
            if let Some(exact) = find_exact_char_subslice(&body_chars, &q_chars) {
                resolved_span = Some(exact);
            } else {
                let hint = QuoteLocationHint {
                    turn_ordinal: None,
                    approx_char_offset: if needs_transcript_strip {
                        None
                    } else {
                        stored_start
                    },
                };
                if let Ok(matched) = verify_verbatim_quote(&effective_body, q, hint) {
                    let s = matched.span.char_start as usize;
                    let e = matched.span.char_end as usize;
                    if s < e && e <= body_chars.len() {
                        resolved_span = Some((s, e));
                    }
                }
            }
        }
    }

    if let Some((start, end)) = resolved_span {
        let prefix_start = start.saturating_sub(HIGHLIGHT_CONTEXT_CHARS);
        let suffix_end = (end + HIGHLIGHT_CONTEXT_CHARS).min(body_chars.len());
        let prefix: String = body_chars[prefix_start..start].iter().collect();
        let highlighted: String = body_chars[start..end].iter().collect();
        let suffix: String = body_chars[end..suffix_end].iter().collect();
        let span_opt = u32::try_from(start)
            .ok()
            .zip(u32::try_from(end).ok())
            .and_then(|(cs, ce)| CharSpan::new(cs, ce).ok());
        (span_opt, prefix, highlighted, suffix)
    } else if let Some(q) = effective_quote {
        // Quote came from the visible fact itself when no exact span matched inside `effective_body`.
        (None, String::new(), q.to_owned(), String::new())
    } else {
        let head_end = body_chars.len().min(160);
        let suffix_end = (head_end + HIGHLIGHT_CONTEXT_CHARS).min(body_chars.len());
        let highlighted: String = body_chars[..head_end].iter().collect();
        let suffix: String = body_chars[head_end..suffix_end].iter().collect();
        let span_opt = u32::try_from(head_end)
            .ok()
            .and_then(|ce| CharSpan::new(0, ce).ok());
        (span_opt, String::new(), highlighted, suffix)
    }
}

fn validate_chunk_row_live_readable(
    row: RawChunkQueryRow,
    expected_source_id: Option<&SourceId>,
    fact_scope_id: Option<&SourceScopeId>,
    graph: &DerivationView,
    viewer: &ActorId,
) -> Option<LoadedChunkSource> {
    if row.is_tombstoned {
        return None;
    }
    if let Some(exp_sid) = expected_source_id {
        if row.source_id_str != exp_sid.as_str() {
            return None;
        }
    }
    let Ok(source_id) = SourceId::new(&row.source_id_str) else {
        return None;
    };

    // Enforce BOTH `raw_sources.visibility_scope` AND `chunks.readable_by_json`.
    let Ok(src_vis) = serde_json::from_str::<AudienceSpec>(&row.source_visibility_json) else {
        return None;
    };
    if !src_vis.allows(viewer, &graph.members) {
        return None;
    }
    let Ok(chunk_vis) = serde_json::from_str::<AudienceSpec>(&row.chunk_readable_json) else {
        return None;
    };
    if !chunk_vis.allows(viewer, &graph.members) {
        return None;
    }

    // Enforce live `source_scopes` containment on BOTH `raw_sources.source_scope_id` and `fact.source_scope_id`.
    if let Some(ref sid_str) = row.source_scope_id {
        let scope_id = sid_str.parse::<SourceScopeId>().ok()?;
        let scope_aud = graph.source_scopes.get(&scope_id)?;
        if !scope_aud.allows(viewer, &graph.members) {
            return None;
        }
    }
    if let Some(f_scope) = fact_scope_id {
        let scope_aud = graph.source_scopes.get(f_scope)?;
        if !scope_aud.allows(viewer, &graph.members) {
            return None;
        }
    }

    Some(LoadedChunkSource {
        chunk_id: row.chunk_id,
        source_id,
        source_uri: row.source_uri,
        body: row.body,
        self_echo_filtered: row.self_echo_filtered,
    })
}

fn load_live_readable_fact_chunk(
    tx: &Connection,
    graph: &DerivationView,
    fact: &Fact,
    viewer: &ActorId,
) -> Result<(bool, Option<LoadedChunkSource>)> {
    let Some(anchor) = &fact.source_anchor else {
        return Ok((false, None));
    };
    let has_backing_ref = anchor.chunk_id.is_some() || anchor.source_id.is_some();
    if !has_backing_ref {
        return Ok((false, None));
    }

    if let Some(ref cid) = anchor.chunk_id {
        let row_opt: Option<RawChunkQueryRow> = tx
            .query_row(
                "SELECT c.chunk_id,
                        s.source_id,
                        s.uri,
                        c.body,
                        c.readable_by_json,
                        s.visibility_scope,
                        s.source_scope_id,
                        s.self_echo_filtered,
                        EXISTS(SELECT 1 FROM tombstones t WHERE t.tombstone_id = s.source_id)
                 FROM chunks c
                 JOIN raw_sources s ON s.source_id = c.source_id
                 WHERE c.chunk_id = ?1",
                [cid.as_str()],
                |r| {
                    Ok(RawChunkQueryRow {
                        chunk_id: r.get(0)?,
                        source_id_str: r.get(1)?,
                        source_uri: r.get(2)?,
                        body: r.get(3)?,
                        chunk_readable_json: r.get(4)?,
                        source_visibility_json: r.get(5)?,
                        source_scope_id: r.get(6)?,
                        self_echo_filtered: r.get(7)?,
                        is_tombstoned: r.get(8)?,
                    })
                },
            )
            .optional()?;
        if let Some(row) = row_opt {
            return Ok((
                true,
                validate_chunk_row_live_readable(
                    row,
                    anchor.source_id.as_ref(),
                    fact.source_scope_id.as_ref(),
                    graph,
                    viewer,
                ),
            ));
        }
    }

    if let Some(ref sid) = anchor.source_id {
        let mut stmt = tx.prepare_cached(
            "SELECT c.chunk_id,
                    s.source_id,
                    s.uri,
                    c.body,
                    c.readable_by_json,
                    s.visibility_scope,
                    s.source_scope_id,
                    s.self_echo_filtered,
                    EXISTS(SELECT 1 FROM tombstones t WHERE t.tombstone_id = s.source_id)
             FROM chunks c
             JOIN raw_sources s ON s.source_id = c.source_id
             WHERE s.source_id = ?1
             ORDER BY c.ordinal ASC",
        )?;
        let rows = stmt.query_map([sid.as_str()], |r| {
            Ok(RawChunkQueryRow {
                chunk_id: r.get(0)?,
                source_id_str: r.get(1)?,
                source_uri: r.get(2)?,
                body: r.get(3)?,
                chunk_readable_json: r.get(4)?,
                source_visibility_json: r.get(5)?,
                source_scope_id: r.get(6)?,
                self_echo_filtered: r.get(7)?,
                is_tombstoned: r.get(8)?,
            })
        })?;

        let mut readable_chunks = Vec::new();
        for r in rows {
            if let Some(valid) = validate_chunk_row_live_readable(
                r?,
                Some(sid),
                fact.source_scope_id.as_ref(),
                graph,
                viewer,
            ) {
                readable_chunks.push(valid);
            }
        }
        if readable_chunks.is_empty() {
            return Ok((true, None));
        }
        if let Some(ref q) = anchor.verbatim_quote {
            if let Some(idx) = readable_chunks.iter().position(|c| {
                c.body.contains(q.as_str())
                    || verify_verbatim_quote(&c.body, q, QuoteLocationHint::default()).is_ok()
            }) {
                return Ok((true, Some(readable_chunks.swap_remove(idx))));
            }
        }
        return Ok((true, readable_chunks.into_iter().next()));
    }

    Ok((true, None))
}

fn redacted_fallback_source_label(
    tx: &Connection,
    graph: &DerivationView,
    fact: &Fact,
    viewer: &ActorId,
) -> Result<String> {
    if let Some(sid) = fact.source_scope_id.as_ref().filter(|sid| {
        graph
            .source_scopes
            .get(*sid)
            .is_some_and(|aud| aud.allows(viewer, &graph.members))
    }) {
        if let Some(label) = lookup_scope_label(tx, sid.as_str())?.filter(|l| !l.trim().is_empty())
        {
            return Ok(label);
        }
        return Ok(sid.to_string());
    }
    Ok("restricted".to_owned())
}

fn build_fact_card(
    tx: &Connection,
    graph: &DerivationView,
    library_id: &kp_model::LibraryId,
    fact: &Fact,
    viewer: &ActorId,
) -> Result<InspectorFactCard> {
    let (is_derived, recorded_at, valid_from, valid_until) = match &fact.access {
        FactAccess::Base { stamp, .. } => (
            false,
            stamp.recorded_at,
            Some(stamp.valid.valid_from),
            (stamp.valid.valid_until != EpochMs::OPEN_END).then_some(stamp.valid.valid_until),
        ),
        FactAccess::Derived { recorded_at, .. } => (true, *recorded_at, None, None),
    };
    let visible_scope_id = fact.source_scope_id.as_ref().filter(|sid| {
        graph
            .source_scopes
            .get(*sid)
            .is_some_and(|aud| aud.allows(viewer, &graph.members))
    });
    let container_label = match visible_scope_id {
        Some(sid) => lookup_scope_label(tx, sid.as_str())?,
        None => None,
    };
    let (had_backing_ref, readable_chunk_opt) =
        load_live_readable_fact_chunk(tx, graph, fact, viewer)?;
    let source_label = if had_backing_ref && readable_chunk_opt.is_none() {
        redacted_fallback_source_label(tx, graph, fact, viewer)?
    } else {
        resolve_fact_source_label(tx, library_id, fact)?
    };
    Ok(InspectorFactCard {
        fact_id: fact.id.clone(),
        kp_ref: format_kp_ref(&fact.id),
        kind: fact.kind,
        subject: fact.subject.clone(),
        predicate: fact.predicate.clone(),
        claim: render_fact_claim_text(tx, fact)?,
        author: fact.author.clone(),
        is_derived,
        recorded_at,
        valid_from,
        valid_until,
        source_label,
        container_id: visible_scope_id.cloned(),
        container_label,
        unverified_gaps: fact.unverified_gaps.clone(),
    })
}

fn categorize_change_log_op(op: &str) -> &'static str {
    match op {
        "writeback_supersede" | "source_superseded" => "supersede",
        "forget" | "reject" | "source_withdrawn" | "public_source_withdrawn" => "withdraw",
        "container_roster_sync" => "container",
        "enqueue_review" | "resolve_review" => "review",
        _ => "insert",
    }
}

impl Store {
    /// Returns Column ① ("What Kipo Remembers") viewer-visible active facts at `(viewer, at)`.
    ///
    /// - Rejects non-members / revoked members with [`crate::StoreError::PermissionDenied`];
    /// - Evaluates `graph.evaluate(viewer, at)` strictly;
    /// - Preserves caller relevance order when `ordered_filter_ids` is `Some(...)`, or sorts by
    ///   `(recorded_at DESC, fact_id DESC)` when `None`, truncating to `limit`.
    pub fn inspector_visible_facts(
        &self,
        viewer: &ActorId,
        at: EpochMs,
        ordered_filter_ids: Option<&[FactId]>,
        limit: usize,
    ) -> Result<Vec<InspectorFactCard>> {
        let tx = self.db.unchecked_transaction()?;
        let _ = require_active_member_role_tx(&tx, viewer)?;
        if limit == 0 {
            tx.commit()?;
            return Ok(Vec::new());
        }

        let graph = read_graph(&tx)?;
        let evaluated = graph.evaluate(viewer, at);

        let selected_facts: Vec<&Fact> = if let Some(filter_ids) = ordered_filter_ids {
            let mut seen = BTreeSet::new();
            let mut out = Vec::with_capacity(limit.min(filter_ids.len()));
            for fid in filter_ids {
                if out.len() >= limit {
                    break;
                }
                if !seen.insert(fid.clone()) || !evaluated.facts.contains(fid) {
                    continue;
                }
                if let Some(fact) = graph.facts.get(fid) {
                    out.push(fact);
                }
            }
            out
        } else {
            let mut all_vis: Vec<&Fact> = evaluated
                .facts
                .iter()
                .filter_map(|fid| graph.facts.get(fid))
                .collect();
            all_vis.sort_by(|a, b| {
                b.access
                    .recorded_at()
                    .cmp(&a.access.recorded_at())
                    .then_with(|| b.id.cmp(&a.id))
            });
            all_vis.truncate(limit);
            all_vis
        };

        let mut cards = Vec::with_capacity(selected_facts.len());
        for fact in selected_facts {
            cards.push(build_fact_card(
                &tx,
                &graph,
                &self.library_id,
                fact,
                viewer,
            )?);
        }
        tx.commit()?;
        Ok(cards)
    }

    /// Returns Column ② ("Why Kipo Remembers") Unicode-safe verbatim quote highlights for `fact_ids`.
    ///
    /// - Rejects non-members / revoked members with [`crate::StoreError::PermissionDenied`];
    /// - Skips any `fact_id` not visible to `(viewer, at)` (zero existence side-channel);
    /// - Emits raw chunk context ONLY when the backing `raw_sources` + `chunks` + `source_scopes`
    ///   are live-readable by `viewer` and non-tombstoned, slicing ~120 `chars()` before/after the
    ///   matched quote without ever panicking on multi-byte UTF-8 boundaries or leaking unreadable
    ///   source text.
    pub fn inspector_fact_highlights(
        &self,
        viewer: &ActorId,
        at: EpochMs,
        fact_ids: &[FactId],
    ) -> Result<BTreeMap<FactId, InspectorQuoteHighlight>> {
        let tx = self.db.unchecked_transaction()?;
        let _ = require_active_member_role_tx(&tx, viewer)?;
        if fact_ids.is_empty() {
            tx.commit()?;
            return Ok(BTreeMap::new());
        }

        let graph = read_graph(&tx)?;
        let evaluated = graph.evaluate(viewer, at);
        let mut results = BTreeMap::new();

        for fid in fact_ids {
            if results.contains_key(fid) {
                continue;
            }
            let Some(fact) = graph.facts.get(fid) else {
                continue;
            };
            let visible = evaluated.facts.contains(fid)
                || is_fact_audit_visible(&graph, &evaluated, fact, viewer, &mut BTreeSet::new());
            if !visible {
                continue;
            }

            let (had_backing_ref, readable_chunk_opt) =
                load_live_readable_fact_chunk(&tx, &graph, fact, viewer)?;

            if let Some(loaded) = readable_chunk_opt {
                let anchor = fact.source_anchor.as_ref();
                let raw_quote = anchor.and_then(|a| a.verbatim_quote.as_deref());
                let needs_strip =
                    loaded.self_echo_filtered || has_transcript_role_or_turn_tags(&loaded.body);
                let (char_span, excerpt_prefix, excerpt_highlighted, excerpt_suffix) =
                    slice_highlight_context(
                        &loaded.body,
                        anchor.and_then(|a| a.char_start),
                        anchor.and_then(|a| a.char_end),
                        raw_quote,
                        needs_strip,
                    );
                let clean_quote = raw_quote
                    .map(|q| {
                        if needs_strip || has_transcript_role_or_turn_tags(q) {
                            strip_turn_tags_and_assistant_lines(q)
                        } else {
                            q.to_owned()
                        }
                    })
                    .filter(|q| !q.trim().is_empty());
                let source_label = resolve_fact_source_label(&tx, &self.library_id, fact)?;
                results.insert(
                    fid.clone(),
                    InspectorQuoteHighlight {
                        fact_id: fid.clone(),
                        chunk_id: Some(loaded.chunk_id),
                        source_id: Some(loaded.source_id),
                        source_uri: Some(loaded.source_uri),
                        source_label,
                        verbatim_quote: clean_quote,
                        char_span,
                        excerpt_prefix,
                        excerpt_highlighted,
                        excerpt_suffix,
                    },
                );
            } else if had_backing_ref {
                // Backing source/chunk is unreadable, departed, or tombstoned: emit zero source text/URI.
                let fallback_label = redacted_fallback_source_label(&tx, &graph, fact, viewer)?;
                results.insert(
                    fid.clone(),
                    InspectorQuoteHighlight {
                        fact_id: fid.clone(),
                        chunk_id: None,
                        source_id: None,
                        source_uri: None,
                        source_label: fallback_label,
                        verbatim_quote: None,
                        char_span: None,
                        excerpt_prefix: String::new(),
                        excerpt_highlighted: String::new(),
                        excerpt_suffix: String::new(),
                    },
                );
            } else {
                // Unanchored / in-memory fact without `source_id` or `chunk_id`: use visible quote if any.
                let clean_quote = fact
                    .source_anchor
                    .as_ref()
                    .and_then(|a| a.verbatim_quote.as_deref())
                    .map(strip_turn_tags_and_assistant_lines)
                    .filter(|q| !q.trim().is_empty());
                let source_label = resolve_fact_source_label(&tx, &self.library_id, fact)?;
                results.insert(
                    fid.clone(),
                    InspectorQuoteHighlight {
                        fact_id: fid.clone(),
                        chunk_id: None,
                        source_id: None,
                        source_uri: None,
                        source_label,
                        verbatim_quote: clean_quote.clone(),
                        char_span: None,
                        excerpt_prefix: String::new(),
                        excerpt_highlighted: clean_quote.unwrap_or_default(),
                        excerpt_suffix: String::new(),
                    },
                );
            }
        }

        tx.commit()?;
        Ok(results)
    }

    /// Returns Column ③ ("What Changed Later") unified viewer-isolated timeline ordered by
    /// `(occurred_at DESC, seq DESC)`.
    ///
    /// Combines [`Store::visible_change_log`] with synthetic viewer-visible lifecycle transitions:
    /// - `"expire"`: non-withdrawn, non-superseded base facts whose `valid_until <= at` (`< OPEN_END`)
    ///   or `expired_at <= at`;
    /// - `"supersede"`: `FactStatus::Superseded` facts not already covered by `writeback_supersede`
    ///   or `source_superseded`;
    /// - `"stale_premise"`: `StalePremise`, `Invalidated`, or `Quarantined` facts whose conjunctive
    ///   premises are audit-visible to `viewer`.
    pub fn inspector_timeline(
        &self,
        viewer: &ActorId,
        at: EpochMs,
        limit: usize,
        target_filter: Option<&str>,
    ) -> Result<Vec<InspectorTimelineEntry>> {
        let base_logs = self.visible_change_log(viewer, at, usize::MAX, target_filter)?;
        if limit == 0 {
            return Ok(Vec::new());
        }

        let tx = self.db.unchecked_transaction()?;
        let _ = require_active_member_role_tx(&tx, viewer)?;
        let graph = read_graph(&tx)?;
        let evaluated_live = graph.evaluate(viewer, at);
        let clean_filter = normalize_target_filter(target_filter);

        // Identify old/new fact IDs already represented by explicit supersede rows in `change_log`.
        let mut covered_supersede_new = BTreeSet::<String>::new();
        let mut covered_supersede_old = BTreeSet::<String>::new();
        for entry in &base_logs {
            if entry.op == "writeback_supersede" {
                covered_supersede_new.insert(entry.target_id.clone());
            } else if entry.op == "source_superseded" {
                covered_supersede_old.insert(entry.target_id.clone());
            }
        }
        for fact in graph.facts.values() {
            if let Some(ref next_id) = fact.superseded_by {
                if covered_supersede_new.contains(next_id.as_str()) {
                    covered_supersede_old.insert(fact.id.to_string());
                }
            }
        }

        let mut timeline: Vec<InspectorTimelineEntry> = base_logs
            .into_iter()
            .map(|e| InspectorTimelineEntry {
                seq: e.seq,
                category: categorize_change_log_op(&e.op).to_owned(),
                op: e.op,
                target_id: e.target_id,
                actor_id: e.actor_id,
                summary: e.summary,
                occurred_at: e.created_at,
            })
            .collect();

        for fact in graph.facts.values() {
            if !is_fact_audit_visible(&graph, &evaluated_live, fact, viewer, &mut BTreeSet::new()) {
                continue;
            }
            let kp_ref = format_kp_ref(&fact.id);

            // 1. Time-driven expiration (`category = "expire"`).
            if let FactAccess::Base { stamp, .. } = &fact.access {
                if fact.status != FactStatus::Withdrawn && fact.status != FactStatus::Superseded {
                    let window_expired = stamp.valid.valid_until < EpochMs::OPEN_END
                        && stamp.valid.valid_until <= at;
                    let explicit_expired = stamp.expired_at.is_some_and(|exp| exp <= at);
                    if window_expired || explicit_expired {
                        let expire_at = match (window_expired, stamp.expired_at) {
                            (true, Some(exp)) if exp <= at => {
                                EpochMs(stamp.valid.valid_until.0.min(exp.0))
                            }
                            (true, _) => stamp.valid.valid_until,
                            (false, Some(exp)) => exp,
                            _ => at,
                        };
                        let filter_ok = clean_filter.as_ref().is_none_or(|flt| {
                            matches_target_filter(
                                flt,
                                &[fact.id.as_str(), kp_ref.as_str(), fact.logical_key.as_str()],
                            )
                        });
                        if filter_ok {
                            let claim = truncate_inline(&render_fact_claim_text(&tx, fact)?, 120);
                            timeline.push(InspectorTimelineEntry {
                                seq: 0,
                                category: "expire".to_owned(),
                                op: "fact_expired".to_owned(),
                                target_id: fact.id.to_string(),
                                actor_id: Some(fact.author.clone()),
                                summary: format!(
                                    "expired {} `{kp_ref}`: {claim}",
                                    fact.kind.as_str()
                                ),
                                occurred_at: expire_at,
                            });
                        }
                    }
                }
            }

            // 2. Superseded old fact not already logged via `writeback_supersede`/`source_superseded`.
            if fact.status == FactStatus::Superseded
                && !covered_supersede_old.contains(fact.id.as_str())
            {
                let next_visible = fact
                    .superseded_by
                    .as_ref()
                    .and_then(|nid| graph.facts.get(nid))
                    .filter(|nf| {
                        is_fact_audit_visible(
                            &graph,
                            &evaluated_live,
                            nf,
                            viewer,
                            &mut BTreeSet::new(),
                        )
                    });
                let occurred_at = next_visible
                    .map(|nf| nf.access.recorded_at())
                    .or_else(|| match &fact.access {
                        FactAccess::Base { stamp, .. }
                            if stamp.valid.valid_until < EpochMs::OPEN_END =>
                        {
                            Some(stamp.valid.valid_until)
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| fact.access.recorded_at());
                if occurred_at <= at {
                    let next_id_str = next_visible.map(|nf| nf.id.as_str()).unwrap_or("");
                    let filter_ok = clean_filter.as_ref().is_none_or(|flt| {
                        matches_target_filter(
                            flt,
                            &[
                                fact.id.as_str(),
                                kp_ref.as_str(),
                                fact.logical_key.as_str(),
                                next_id_str,
                            ],
                        )
                    });
                    if filter_ok {
                        let old_claim = truncate_inline(&render_fact_claim_text(&tx, fact)?, 100);
                        let (actor, summary) = if let Some(nf) = next_visible {
                            let new_ref = format_kp_ref(&nf.id);
                            let new_claim = truncate_inline(&render_fact_claim_text(&tx, nf)?, 100);
                            (
                                Some(nf.author.clone()),
                                format!(
                                    "superseded `{kp_ref}` -> `{new_ref}`: {new_claim} (was: {old_claim})"
                                ),
                            )
                        } else {
                            (
                                Some(fact.author.clone()),
                                format!(
                                    "superseded {} `{kp_ref}`: {old_claim}",
                                    fact.kind.as_str()
                                ),
                            )
                        };
                        timeline.push(InspectorTimelineEntry {
                            seq: 0,
                            category: "supersede".to_owned(),
                            op: "supersede_fact".to_owned(),
                            target_id: fact.id.to_string(),
                            actor_id: actor,
                            summary,
                            occurred_at,
                        });
                    }
                }
            }

            // 3. `StalePremise` / `Invalidated` / `Quarantined` derived or synthesis invalidations.
            if matches!(
                fact.status,
                FactStatus::StalePremise | FactStatus::Invalidated | FactStatus::Quarantined
            ) {
                let just_invalidated_at = graph
                    .justifications
                    .values()
                    .filter(|j| j.conclusion == fact.id)
                    .filter_map(|j| j.invalidated_at)
                    .filter(|t| *t <= at)
                    .max();
                let occurred_at = just_invalidated_at
                    .or_else(|| fact.access.expired_at().filter(|t| *t <= at))
                    .unwrap_or_else(|| fact.access.recorded_at());
                if occurred_at <= at {
                    let filter_ok = clean_filter.as_ref().is_none_or(|flt| {
                        matches_target_filter(
                            flt,
                            &[fact.id.as_str(), kp_ref.as_str(), fact.logical_key.as_str()],
                        )
                    });
                    if filter_ok {
                        let op = match fact.status {
                            FactStatus::StalePremise => "stale_premise",
                            FactStatus::Quarantined => "fact_quarantined",
                            _ => "derived_invalidated",
                        };
                        let claim = truncate_inline(&render_fact_claim_text(&tx, fact)?, 110);
                        timeline.push(InspectorTimelineEntry {
                            seq: 0,
                            category: "stale_premise".to_owned(),
                            op: op.to_owned(),
                            target_id: fact.id.to_string(),
                            actor_id: Some(fact.author.clone()),
                            summary: format!(
                                "{} {} `{kp_ref}`: {claim}",
                                op.replace('_', " "),
                                fact.kind.as_str()
                            ),
                            occurred_at,
                        });
                    }
                }
            }
        }

        timeline.sort_by(|a, b| {
            b.occurred_at
                .cmp(&a.occurred_at)
                .then_with(|| b.seq.cmp(&a.seq))
                .then_with(|| b.target_id.cmp(&a.target_id))
                .then_with(|| a.category.cmp(&b.category))
        });
        timeline.truncate(limit);
        tx.commit()?;
        Ok(timeline)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use kp_model::{
        derived_fact, justification, ActorId, AnchorSpec, AudienceSpec, BitemporalStamp,
        DerivationMode, EpochMs, Fact, FactAccess, FactId, FactKind, FactRank, FactStatus,
        LibraryMode, MemberRole, ModelRoutePolicy, PublicAudience, PublicContainerRoster,
        PublicIngestOp, PublicIngestRecord, PublicIngestSource, PublicSpeaker, TrustTier,
        ValidityWindow,
    };
    use serde_json::json;

    use crate::test_support::{library, AT};
    use crate::{new_fact_id, ChunkInput, RawSourceInput, Result, StoreError};

    #[allow(clippy::too_many_arguments)]
    fn make_base_fact(
        id: FactId,
        kind: FactKind,
        subject: &str,
        predicate: &str,
        object: serde_json::Value,
        author: &ActorId,
        audience: AudienceSpec,
        valid_from: EpochMs,
        valid_until: EpochMs,
        recorded_at: EpochMs,
        anchor: Option<AnchorSpec>,
    ) -> Result<Fact> {
        Ok(Fact {
            id: id.clone(),
            origin_library: "local".parse()?,
            logical_key: id.as_str().parse()?,
            kind,
            subject: subject.to_owned(),
            predicate: predicate.to_owned(),
            object,
            tags: BTreeSet::new(),
            author: author.clone(),
            ingested_by: Some(author.clone()),
            trust_tier: TrustTier::HumanVerbatim,
            access: FactAccess::Base {
                audience,
                stamp: BitemporalStamp {
                    valid: ValidityWindow {
                        valid_from,
                        valid_until,
                    },
                    recorded_at,
                    expired_at: None,
                },
            },
            source_scope_id: None,
            governed_scope: None,
            broader_scope: None,
            status: FactStatus::Active,
            superseded_by: None,
            rank: FactRank::Normal,
            source_anchor: anchor,
            unverified_gaps: Vec::new(),
            miss_count: 0,
        })
    }

    #[test]
    fn inspector_multibyte_utf8_quote_highlight_reconstructs_exact_context_and_strips_echo(
    ) -> Result<()> {
        let (_dir, mut store) = library(LibraryMode::Community)?;
        let alice: ActorId = "alice".parse()?;
        store.add_member(alice.clone(), MemberRole::Maintainer, AT)?;

        let prefix_zh = "【架构评审纪要 · 第十二周】背景说明：在高并发结算批处理链路中，为了防止跨账期死锁与主从复制延迟导致对账漂移，团队逐项核对了读写分离策略与索引约束，最终明确拍板——";
        let quote_zh = "严禁在日终清算窗口期执行在线删索引或长事务锁表操作";
        let suffix_zh = "；如遇紧急故障需先切换只读流量并经双人复核放行，随后在次日低峰窗口补齐变更记录与回滚演练报告。";
        let full_body = format!("{prefix_zh}{quote_zh}{suffix_zh}");

        let prefix_char_len = u32::try_from(prefix_zh.chars().count()).unwrap_or(0);
        let quote_char_len = u32::try_from(quote_zh.chars().count()).unwrap_or(0);

        let src_input = RawSourceInput {
            self_echo_filtered: false,
            uri: "notes/settlement_review.md".into(),
            media_type: "text/markdown".into(),
            content: full_body.as_bytes().to_vec(),
            readable_by: AudienceSpec::AllMembers,
            captured_at: AT,
            chunks: vec![ChunkInput {
                heading: "结算评审".into(),
                body: full_body.clone(),
                anchor: AnchorSpec::default(),
            }],
        };
        let stored = store.ingest_source(&"local".parse()?, &"doc:zh1".parse()?, &src_input)?;
        let chunk_id = format!("{}-0", stored.source_id);

        let rule_id = new_fact_id()?;
        let rule = make_base_fact(
            rule_id.clone(),
            FactKind::Constraint,
            "settlement_index_lock",
            "kp.core.guard",
            json!(quote_zh),
            &alice,
            AudienceSpec::AllMembers,
            AT,
            EpochMs::OPEN_END,
            AT,
            Some(AnchorSpec {
                source_id: Some(stored.source_id.clone()),
                chunk_id: Some(chunk_id.clone()),
                verbatim_quote: Some(quote_zh.to_owned()),
                char_start: Some(prefix_char_len),
                char_end: Some(prefix_char_len + quote_char_len),
                ..AnchorSpec::default()
            }),
        )?;
        store.insert_fact(&rule)?;

        // Also ingest a session staging chunk containing `[assistant]` echo + `[turn:N]` markers.
        let session_body = "[turn:0] [user] 缓存默认过期时间统一改成十五分钟\n[turn:1] [assistant] 好的，我已记录缓存十五分钟配置并准备修改代码。";
        let sess_src = store.ingest_source(
            &"local".parse()?,
            &"sess:echo1".parse()?,
            &RawSourceInput {
                self_echo_filtered: true,
                uri: "session://turn_1".into(),
                media_type: "text/plain".into(),
                content: session_body.as_bytes().to_vec(),
                readable_by: AudienceSpec::Explicit(BTreeSet::from([alice.clone()])),
                captured_at: EpochMs(AT.0 + 5),
                chunks: vec![ChunkInput {
                    heading: String::new(),
                    body: session_body.into(),
                    anchor: AnchorSpec::default(),
                }],
            },
        )?;
        let sess_fact_id = new_fact_id()?;
        let sess_fact = make_base_fact(
            sess_fact_id.clone(),
            FactKind::Decision,
            "cache_default_ttl",
            "kp.core.cache",
            json!("缓存默认过期时间统一改成十五分钟"),
            &alice,
            AudienceSpec::Explicit(BTreeSet::from([alice.clone()])),
            EpochMs(AT.0 + 5),
            EpochMs::OPEN_END,
            EpochMs(AT.0 + 5),
            Some(AnchorSpec {
                source_id: Some(sess_src.source_id.clone()),
                verbatim_quote: Some("缓存默认过期时间统一改成十五分钟".into()),
                ..AnchorSpec::default()
            }),
        )?;
        store.insert_fact(&sess_fact)?;

        let highlights = store.inspector_fact_highlights(
            &alice,
            EpochMs(AT.0 + 10),
            &[rule_id.clone(), sess_fact_id.clone()],
        )?;
        let hl = highlights
            .get(&rule_id)
            .ok_or_else(|| kp_model::KpError::invalid("test", "missing Chinese rule highlight"))?;
        assert_eq!(hl.excerpt_highlighted, quote_zh);
        assert_eq!(hl.chunk_id.as_deref(), Some(chunk_id.as_str()));
        let reconstructed = format!(
            "{}{}{}",
            hl.excerpt_prefix, hl.excerpt_highlighted, hl.excerpt_suffix
        );
        // Because prefix & suffix are both < 120 chars, concatenation equals `full_body` byte-for-byte!
        assert_eq!(reconstructed, full_body);

        let sess_hl = highlights
            .get(&sess_fact_id)
            .ok_or_else(|| kp_model::KpError::invalid("test", "missing session highlight"))?;
        assert_eq!(
            sess_hl.excerpt_highlighted,
            "缓存默认过期时间统一改成十五分钟"
        );
        assert!(!sess_hl.excerpt_prefix.contains("[assistant]"));
        assert!(!sess_hl.excerpt_suffix.contains("[assistant]"));
        assert!(!sess_hl.excerpt_suffix.contains("准备修改代码"));

        // Also test >120-char multi-byte CJK + emoji prefix/suffix clamping without byte panics.
        let long_prefix = "前序背景记录🚀：".repeat(25); // 200 Unicode chars
        let long_quote = "所有跨库同步任务必须携带幂等键并校验内容哈希";
        let long_suffix = "后续巡检核验✅；".repeat(25); // 200 Unicode chars
        let long_body = format!("{long_prefix}{long_quote}{long_suffix}");
        let long_src = store.ingest_source(
            &"local".parse()?,
            &"doc:long_zh".parse()?,
            &RawSourceInput {
                self_echo_filtered: false,
                uri: "notes/long_sync_spec.md".into(),
                media_type: "text/markdown".into(),
                content: long_body.as_bytes().to_vec(),
                readable_by: AudienceSpec::AllMembers,
                captured_at: EpochMs(AT.0 + 8),
                chunks: vec![ChunkInput {
                    heading: "同步规范".into(),
                    body: long_body.clone(),
                    anchor: AnchorSpec::default(),
                }],
            },
        )?;
        let long_fact_id = new_fact_id()?;
        let long_fact = make_base_fact(
            long_fact_id.clone(),
            FactKind::Constraint,
            "sync_idempotency",
            "kp.core.sync",
            json!(long_quote),
            &alice,
            AudienceSpec::AllMembers,
            EpochMs(AT.0 + 8),
            EpochMs::OPEN_END,
            EpochMs(AT.0 + 8),
            Some(AnchorSpec {
                source_id: Some(long_src.source_id),
                verbatim_quote: Some(long_quote.into()),
                // Intentionally pass drifted `char_start`/`char_end` to verify automatic quote fallback!
                char_start: Some(3),
                char_end: Some(10),
                ..AnchorSpec::default()
            }),
        )?;
        store.insert_fact(&long_fact)?;

        let long_hl_map = store.inspector_fact_highlights(
            &alice,
            EpochMs(AT.0 + 12),
            std::slice::from_ref(&long_fact_id),
        )?;
        let long_hl = long_hl_map
            .get(&long_fact_id)
            .ok_or_else(|| kp_model::KpError::invalid("test", "missing long Chinese highlight"))?;
        assert_eq!(long_hl.excerpt_prefix.chars().count(), 120);
        assert_eq!(long_hl.excerpt_highlighted, long_quote);
        assert_eq!(long_hl.excerpt_suffix.chars().count(), 120);
        let window_concat = format!(
            "{}{}{}",
            long_hl.excerpt_prefix, long_hl.excerpt_highlighted, long_hl.excerpt_suffix
        );
        assert!(long_body.contains(&window_concat));

        Ok(())
    }

    #[test]
    fn inspector_timeline_covers_supersede_withdraw_expire_and_stale_premise() -> Result<()> {
        let (_dir, mut store) = library(LibraryMode::Community)?;
        let owner: ActorId = "owner".parse()?;
        let alice: ActorId = "alice".parse()?;
        store.add_member(alice.clone(), MemberRole::Maintainer, AT)?;

        // 1. Old architecture premise `p_v1` supporting an `AssumedPremise` conclusion `assumed_c`,
        //    then superseded by `p_v2` -> triggers BOTH `"supersede"` AND `"stale_premise"`!
        let p_v1_id = new_fact_id()?;
        let p_v1 = make_base_fact(
            p_v1_id.clone(),
            FactKind::Decision,
            "queue_backend",
            "kp.core.arch",
            json!("Use polling cron worker for job dispatch"),
            &alice,
            AudienceSpec::AllMembers,
            AT,
            EpochMs::OPEN_END,
            AT,
            Some(AnchorSpec {
                verbatim_quote: Some("Use polling cron worker for job dispatch".into()),
                ..AnchorSpec::default()
            }),
        )?;
        store.insert_fact(&p_v1)?;

        let assumed = derived_fact(
            &"local".parse()?,
            &owner,
            FactKind::DerivedFact,
            "cron_interval",
            "kp.core.assumed",
            json!({"summary": "Keep cron poll interval at 30s"}),
            EpochMs(AT.0 + 10),
        )?;
        let assumed_id = assumed.id.clone();
        let assumed_just = justification(
            &assumed,
            DerivationMode::AssumedPremise,
            "arch_assumption_v1",
            &[&p_v1],
        )?;
        store.save_derivations(&[assumed], &[assumed_just])?;

        let p_v2_id = new_fact_id()?;
        let p_v2 = make_base_fact(
            p_v2_id.clone(),
            FactKind::Decision,
            "queue_backend",
            "kp.core.arch",
            json!("Switch to event-driven UDS wakeup queue"),
            &alice,
            AudienceSpec::AllMembers,
            EpochMs(AT.0 + 20),
            EpochMs::OPEN_END,
            EpochMs(AT.0 + 20),
            Some(AnchorSpec {
                verbatim_quote: Some("Switch to event-driven UDS wakeup queue".into()),
                ..AnchorSpec::default()
            }),
        )?;
        store.supersede(&p_v1_id, &p_v2)?;

        // 2. Withdrawn fact (`forget_fact`) -> `"withdraw"`.
        let withdraw_id = new_fact_id()?;
        let withdraw_fact = make_base_fact(
            withdraw_id.clone(),
            FactKind::Observation,
            "temp_flag",
            "kp.core.flag",
            json!("Enable verbose debug tracing"),
            &alice,
            AudienceSpec::AllMembers,
            EpochMs(AT.0 + 25),
            EpochMs::OPEN_END,
            EpochMs(AT.0 + 25),
            None,
        )?;
        store.insert_fact(&withdraw_fact)?;
        store.forget_fact_id(&withdraw_id, &alice, EpochMs(AT.0 + 30))?;

        // 3. Time-bounded maintenance window fact (`valid_until < query_at`, without calling `store.expire`)
        //    -> `"expire"` synthesized automatically!
        let exp_id = new_fact_id()?;
        let exp_fact = make_base_fact(
            exp_id.clone(),
            FactKind::Constraint,
            "maint_window",
            "kp.core.window",
            json!("Temporary read-only freeze until +40ms"),
            &alice,
            AudienceSpec::AllMembers,
            EpochMs(AT.0 + 10),
            EpochMs(AT.0 + 40),
            EpochMs(AT.0 + 10),
            None,
        )?;
        store.insert_fact(&exp_fact)?;

        let query_at = EpochMs(AT.0 + 60);
        // Active cards at `query_at`: ONLY `p_v2` remains active (`p_v1` superseded, `assumed` stale_premise,
        // `withdraw_fact` withdrawn, `exp_fact` past `valid_until`)!
        let cards = store.inspector_visible_facts(&alice, query_at, None, 50)?;
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].fact_id, p_v2_id);
        assert_eq!(cards[0].valid_until, None);

        let timeline = store.inspector_timeline(&alice, query_at, 50, None)?;
        let categories: BTreeSet<&str> = timeline.iter().map(|e| e.category.as_str()).collect();
        assert!(categories.contains("supersede"));
        assert!(categories.contains("withdraw"));
        assert!(categories.contains("expire"));
        assert!(categories.contains("stale_premise"));
        assert!(categories.contains("insert"));

        assert!(timeline
            .iter()
            .any(|e| e.category == "expire" && e.target_id == exp_id.as_str()));
        assert!(timeline
            .iter()
            .any(|e| e.category == "supersede" && e.target_id == p_v1_id.as_str()));
        assert!(timeline
            .iter()
            .any(|e| e.category == "stale_premise" && e.target_id == assumed_id.as_str()));
        assert!(timeline
            .iter()
            .any(|e| e.category == "withdraw" && e.target_id == withdraw_id.as_str()));

        for pair in timeline.windows(2) {
            assert!(
                (pair[0].occurred_at, pair[0].seq) >= (pair[1].occurred_at, pair[1].seq),
                "timeline must be ordered by (occurred_at DESC, seq DESC)"
            );
        }

        Ok(())
    }

    #[test]
    fn inspector_enforces_nonmember_denial_and_zero_leak_for_carol_departed_bob_and_tombstones(
    ) -> Result<()> {
        let (_dir, mut store) = library(LibraryMode::Community)?;
        let alice: ActorId = "alice".parse()?;
        let bob: ActorId = "bob".parse()?;
        let carol: ActorId = "carol".parse()?;
        let outsider: ActorId = "outsider".parse()?;
        store.add_member(alice.clone(), MemberRole::Maintainer, AT)?;
        store.add_member(bob.clone(), MemberRole::Member, AT)?;
        store.add_member(carol.clone(), MemberRole::Member, AT)?;

        // Non-member `outsider` gets `PermissionDenied` on all 3 endpoints!
        assert!(matches!(
            store.inspector_visible_facts(&outsider, AT, None, 20),
            Err(StoreError::PermissionDenied)
        ));
        assert!(matches!(
            store.inspector_fact_highlights(&outsider, AT, &[]),
            Err(StoreError::PermissionDenied)
        ));
        assert!(matches!(
            store.inspector_timeline(&outsider, AT, 20, None),
            Err(StoreError::PermissionDenied)
        ));

        // Container `chat.grp_sec` initially includes `[alice, bob]` (NOT `carol`).
        store.apply_public_container_roster(
            &alice,
            &PublicContainerRoster {
                container_id: "chat.grp_sec".into(),
                label: "Security Response Room".into(),
                members: vec!["alice".into(), "bob".into()],
                all_library_members: false,
                synced_at: EpochMs(AT.0 + 5),
                provider: Some("chat".into()),
                upstream_container_id: Some("grp_sec".into()),
            },
        )?;

        let ing = store.apply_public_ingest_record(
            &PublicIngestRecord {
                source: PublicIngestSource {
                    kind: "chat".into(),
                    uri: "chat://grp_sec/m1".into(),
                    external_id: "m1".into(),
                    container_id: Some("chat.grp_sec".into()),
                },
                speaker: PublicSpeaker {
                    id: "alice".into(),
                    display_name: Some("Alice".into()),
                },
                audience: PublicAudience::ContainerMembers,
                occurred_at: EpochMs(AT.0 + 10),
                content: "Rotate internal signing keys before midnight".into(),
                op: PublicIngestOp::Upsert,
                ingested_by: None,
                media_type: None,
                model_policy: Some(ModelRoutePolicy::RawOnly),
            },
            &alice,
            &ModelRoutePolicy::HostCli,
        )?;
        let grp_src_id = ing
            .source_id
            .ok_or_else(|| kp_model::KpError::invalid("test", "missing grp source_id"))?;

        let grp_fact_id = new_fact_id()?;
        let mut grp_fact = make_base_fact(
            grp_fact_id.clone(),
            FactKind::Constraint,
            "key_rotation",
            "kp.core.sec",
            json!("Rotate internal signing keys before midnight"),
            &alice,
            AudienceSpec::AllMembers,
            EpochMs(AT.0 + 10),
            EpochMs(AT.0 + 35),
            EpochMs(AT.0 + 10),
            Some(AnchorSpec {
                source_id: Some(grp_src_id.clone()),
                verbatim_quote: Some("Rotate internal signing keys before midnight".into()),
                ..AnchorSpec::default()
            }),
        )?;
        grp_fact.source_scope_id = Some("chat.grp_sec".parse()?);
        store.insert_fact(&grp_fact)?;

        // `alice` private `SelfOnly` fact + two public facts (to also verify `ordered_filter_ids`).
        let alice_priv_id = new_fact_id()?;
        let alice_priv = make_base_fact(
            alice_priv_id.clone(),
            FactKind::Observation,
            "alice_secret",
            "kp.core.priv",
            json!("Alice private incident token note"),
            &alice,
            AudienceSpec::Explicit(BTreeSet::from([alice.clone()])),
            EpochMs(AT.0 + 12),
            EpochMs(AT.0 + 32),
            EpochMs(AT.0 + 12),
            Some(AnchorSpec {
                verbatim_quote: Some("Alice private incident token note".into()),
                ..AnchorSpec::default()
            }),
        )?;
        store.insert_fact(&alice_priv)?;

        let pub1_id = new_fact_id()?;
        let pub1 = make_base_fact(
            pub1_id.clone(),
            FactKind::Decision,
            "pub_one",
            "kp.core.pub",
            json!("Public Architecture Note One"),
            &alice,
            AudienceSpec::AllMembers,
            EpochMs(AT.0 + 15),
            EpochMs::OPEN_END,
            EpochMs(AT.0 + 15),
            None,
        )?;
        store.insert_fact(&pub1)?;

        let pub2_id = new_fact_id()?;
        let pub2 = make_base_fact(
            pub2_id.clone(),
            FactKind::Decision,
            "pub_two",
            "kp.core.pub",
            json!("Public Architecture Note Two"),
            &alice,
            AudienceSpec::AllMembers,
            EpochMs(AT.0 + 18),
            EpochMs::OPEN_END,
            EpochMs(AT.0 + 18),
            None,
        )?;
        store.insert_fact(&pub2)?;

        // Check `ordered_filter_ids` preserves caller relevance order `[pub1, alice_priv, pub2]` -> `[pub1, pub2]` for `carol`!
        let carol_ordered = store.inspector_visible_facts(
            &carol,
            EpochMs(AT.0 + 20),
            Some(&[
                pub1_id.clone(),
                alice_priv_id.clone(),
                grp_fact_id.clone(),
                pub2_id.clone(),
            ]),
            10,
        )?;
        let carol_ordered_ids: Vec<FactId> = carol_ordered.into_iter().map(|c| c.fact_id).collect();
        assert_eq!(carol_ordered_ids, vec![pub1_id.clone(), pub2_id.clone()]);

        // `carol` requesting highlights for `[grp_fact_id, alice_priv_id]` gets an EMPTY map (zero leak!).
        let carol_hl = store.inspector_fact_highlights(
            &carol,
            EpochMs(AT.0 + 20),
            &[grp_fact_id.clone(), alice_priv_id.clone()],
        )?;
        assert!(carol_hl.is_empty());

        // Even after `grp_fact` and `alice_priv` expire at `AT + 40`, `carol`'s timeline shows ZERO
        // `expire` or `insert` or `container` entries from `grp_sec` or `alice_priv`!
        let carol_tl = store.inspector_timeline(&carol, EpochMs(AT.0 + 40), 100, None)?;
        assert!(carol_tl.iter().all(|e| {
            e.target_id != grp_fact_id.as_str()
                && e.target_id != alice_priv_id.as_str()
                && e.target_id != grp_src_id.as_str()
                && e.target_id != "chat.grp_sec"
                && !e.summary.contains("signing keys")
                && !e.summary.contains("Alice private")
        }));

        // While `bob` is in `chat.grp_sec` at `AT + 20`, `bob` CAN see `grp_fact_id`'s highlight.
        let bob_hl_before = store.inspector_fact_highlights(
            &bob,
            EpochMs(AT.0 + 20),
            std::slice::from_ref(&grp_fact_id),
        )?;
        assert_eq!(
            bob_hl_before
                .get(&grp_fact_id)
                .map(|h| h.excerpt_highlighted.as_str()),
            Some("Rotate internal signing keys before midnight")
        );

        // Now `bob` leaves `chat.grp_sec` (`members = ["alice"]`)!
        store.apply_public_container_roster(
            &alice,
            &PublicContainerRoster {
                container_id: "chat.grp_sec".into(),
                label: "Security Response Room".into(),
                members: vec!["alice".into()],
                all_library_members: false,
                synced_at: EpochMs(AT.0 + 45),
                provider: Some("chat".into()),
                upstream_container_id: Some("grp_sec".into()),
            },
        )?;

        // Immediately after departing, `bob` sees ZERO cards, ZERO quote highlights, and ZERO timeline
        // entries (including expired `grp_fact_id`!) from `chat.grp_sec`!
        let bob_cards_after = store.inspector_visible_facts(&bob, EpochMs(AT.0 + 25), None, 50)?;
        assert!(bob_cards_after.iter().all(|c| c.fact_id != grp_fact_id));
        let bob_hl_after = store.inspector_fact_highlights(
            &bob,
            EpochMs(AT.0 + 50),
            std::slice::from_ref(&grp_fact_id),
        )?;
        assert!(bob_hl_after.is_empty());
        let bob_tl_after = store.inspector_timeline(&bob, EpochMs(AT.0 + 50), 100, None)?;
        assert!(bob_tl_after.iter().all(|e| {
            e.target_id != grp_fact_id.as_str()
                && e.target_id != grp_src_id.as_str()
                && e.target_id != "chat.grp_sec"
                && !e.summary.contains("signing keys")
        }));

        // Also verify: if a visible fact references an unreadable private or restricted-container source,
        // BOTH `inspector_visible_facts` and `inspector_fact_highlights` redact `source_label` to `"restricted"`
        // and leak ZERO characters of the private source body/URI/file_path/container label!
        let priv_src = store.ingest_source(
            &"local".parse()?,
            &"priv:vault1".parse()?,
            &RawSourceInput {
                self_echo_filtered: false,
                uri: "vault://alice_only.md".into(),
                media_type: "text/markdown".into(),
                content: b"Top secret vault paragraph for Alice eyes only.".to_vec(),
                readable_by: AudienceSpec::Explicit(BTreeSet::from([alice.clone()])),
                captured_at: EpochMs(AT.0 + 46),
                chunks: vec![ChunkInput {
                    heading: String::new(),
                    body: "Top secret vault paragraph for Alice eyes only.".into(),
                    anchor: AnchorSpec::default(),
                }],
            },
        )?;
        let shared_summary_id = new_fact_id()?;
        let shared_summary = make_base_fact(
            shared_summary_id.clone(),
            FactKind::Decision,
            "vault_summary",
            "kp.core.summary",
            json!("High-level public decision without raw vault access"),
            &alice,
            AudienceSpec::AllMembers,
            EpochMs(AT.0 + 47),
            EpochMs::OPEN_END,
            EpochMs(AT.0 + 47),
            Some(AnchorSpec {
                source_id: Some(priv_src.source_id),
                file_path: Some("vault/alice_only.md".into()),
                verbatim_quote: Some("Top secret vault paragraph for Alice eyes only.".into()),
                ..AnchorSpec::default()
            }),
        )?;
        store.insert_fact(&shared_summary)?;

        let grp_summary_id = new_fact_id()?;
        let grp_summary = make_base_fact(
            grp_summary_id.clone(),
            FactKind::Decision,
            "room_summary",
            "kp.core.summary",
            json!("Public cross-team notice without room membership"),
            &alice,
            AudienceSpec::AllMembers,
            EpochMs(AT.0 + 48),
            EpochMs::OPEN_END,
            EpochMs(AT.0 + 48),
            Some(AnchorSpec {
                source_id: Some(grp_src_id.clone()),
                verbatim_quote: Some("Rotate internal signing keys before midnight".into()),
                ..AnchorSpec::default()
            }),
        )?;
        store.insert_fact(&grp_summary)?;

        let carol_cards = store.inspector_visible_facts(&carol, EpochMs(AT.0 + 50), None, 50)?;
        let carol_vault_card = carol_cards
            .iter()
            .find(|c| c.fact_id == shared_summary_id)
            .ok_or_else(|| {
                kp_model::KpError::invalid("test", "missing carol vault summary card")
            })?;
        assert_eq!(carol_vault_card.source_label, "restricted");
        assert_eq!(carol_vault_card.container_id, None);
        assert_eq!(carol_vault_card.container_label, None);
        let carol_room_card = carol_cards
            .iter()
            .find(|c| c.fact_id == grp_summary_id)
            .ok_or_else(|| kp_model::KpError::invalid("test", "missing carol room summary card"))?;
        assert_eq!(carol_room_card.source_label, "restricted");
        assert!(carol_cards.iter().all(|c| {
            !c.source_label.contains("alice_only")
                && !c.source_label.contains("Security Response Room")
                && !c.source_label.contains("grp_sec")
        }));

        let bob_cards_redacted =
            store.inspector_visible_facts(&bob, EpochMs(AT.0 + 50), None, 50)?;
        let bob_room_card = bob_cards_redacted
            .iter()
            .find(|c| c.fact_id == grp_summary_id)
            .ok_or_else(|| kp_model::KpError::invalid("test", "missing bob room summary card"))?;
        assert_eq!(bob_room_card.source_label, "restricted");

        let alice_cards = store.inspector_visible_facts(&alice, EpochMs(AT.0 + 50), None, 50)?;
        let alice_vault_card = alice_cards
            .iter()
            .find(|c| c.fact_id == shared_summary_id)
            .ok_or_else(|| {
                kp_model::KpError::invalid("test", "missing alice vault summary card")
            })?;
        assert_eq!(alice_vault_card.source_label, "vault://alice_only.md");
        let alice_room_card = alice_cards
            .iter()
            .find(|c| c.fact_id == grp_summary_id)
            .ok_or_else(|| kp_model::KpError::invalid("test", "missing alice room summary card"))?;
        assert_eq!(alice_room_card.source_label, "Security Response Room");

        let carol_shared_hl = store.inspector_fact_highlights(
            &carol,
            EpochMs(AT.0 + 50),
            std::slice::from_ref(&shared_summary_id),
        )?;
        let restricted_hl = carol_shared_hl.get(&shared_summary_id).ok_or_else(|| {
            kp_model::KpError::invalid("test", "expected redacted highlight card")
        })?;
        assert_eq!(restricted_hl.source_id, None);
        assert_eq!(restricted_hl.source_uri, None);
        assert_eq!(restricted_hl.source_label, "restricted");
        assert_eq!(restricted_hl.verbatim_quote, None);
        assert!(restricted_hl.excerpt_prefix.is_empty());
        assert!(restricted_hl.excerpt_highlighted.is_empty());
        assert!(restricted_hl.excerpt_suffix.is_empty());

        // Finally purge `priv:vault1` (creating a tombstone): even `alice` sees zero purged cards/highlights!
        store.purge(
            &"local".parse()?,
            &"priv:vault1".parse()?,
            EpochMs(AT.0 + 55),
            &[],
        )?;
        let alice_after_purge = store.inspector_fact_highlights(
            &alice,
            EpochMs(AT.0 + 60),
            std::slice::from_ref(&shared_summary_id),
        )?;
        assert!(alice_after_purge.is_empty());

        Ok(())
    }
}
