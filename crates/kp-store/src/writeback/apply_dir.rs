use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use kp_model::{
    ActorId, ContentHash, EpochMs, FactAccess, FactId, FactStatus, KpError, MemberRole,
    ModelRoutePolicy,
};
use rusqlite::TransactionBehavior;

use super::parser::{
    build_updated_fact_object, extract_sources_md_candidates_tx, is_standard_sub_bullet,
    kp_ref_matches_fact_id, parse_frontmatter, parse_index_body, parse_standard_entry_bullet,
    scan_markdown_directory, FreeformCandidate, SourcesMdExtractCtx,
};
use super::tx_ops::{
    enqueue_writeback_proposal_tx, evaluate_baseline_view, follow_superseded_head,
    query_projection_page_tx, supersede_fact_via_writeback_tx, was_statically_permitted_for_viewer,
    WritebackProposalContext, WritebackProposalInput, WritebackTxEnv,
};
use super::{
    default_policy_from_env, is_base_fact_readable_by_actor, merge_route_policy, ApplyReport,
    WritebackProposalOp,
};
use crate::audit_log::is_fact_audit_visible;
use crate::export::{sanitize_md_inline, ExportDirLock};
use crate::facts::{fact_effective_route_policy_in_tx, read_graph};
use crate::public_ingest::{require_active_member_role_tx, shared_redactor};
use crate::slots::withdraw_facts;
use crate::snapshot::{
    render_fact_claim_text, resolve_fact_source_label, strip_turn_tags_and_assistant_lines,
};
use crate::{Result, Store, StoreError};

impl Store {
    /// Applies user edits, deletions, and freeform additions from an exported Markdown projection
    /// directory (`<dir>/index.md`, `<dir>/sources.md`, and any extra `.md` files) atomically:
    /// - Serializes directory access via `.kp-export.lock` shared with [`Store::export_markdown_directory`];
    /// - Enforces active member role and `frontmatter.viewer == *viewer` (`PermissionDenied` otherwise);
    /// - Scrubs all user-edited text via [`SecretRedactor`] prior to hashing or persistence;
    /// - Short-circuits with `unchanged = true` and zero DB writes when `page_sha256` matches and
    ///   no extra `.md` files exist;
    /// - Routes stale-baseline conflicts, derived-fact edits/deletions, cross-author non-admin
    ///   modifications, unparseable blocks, and freeform paragraphs/files into
    ///   `review_items(kind = 'writeback_proposal')` with zero silent text loss.
    pub fn apply_markdown_directory(
        &mut self,
        dir: &Path,
        viewer: &ActorId,
        at: EpochMs,
    ) -> Result<ApplyReport> {
        let default_policy = default_policy_from_env();
        self.apply_markdown_directory_with_policy(dir, viewer, &default_policy, at)
    }

    /// Policy-explicit variant of [`Store::apply_markdown_directory`]: honors `default_policy`
    /// (`RawOnly` forces zero-cloud-embedder handling even on newly added freeform blocks).
    pub fn apply_markdown_directory_with_policy(
        &mut self,
        dir: &Path,
        viewer: &ActorId,
        default_policy: &ModelRoutePolicy,
        at: EpochMs,
    ) -> Result<ApplyReport> {
        let _ = require_active_member_role_tx(&self.db, viewer)?;
        let redactor = shared_redactor()?;

        // Check `dir` before acquiring `.kp-export.lock` (matching `export_markdown_directory` lock order).
        let dir_meta = fs::symlink_metadata(dir)?;
        if dir_meta.file_type().is_symlink() || !dir_meta.is_dir() {
            return Err(
                KpError::invalid("dir", "export directory must be a real directory").into(),
            );
        }
        let _dir_lock = ExportDirLock::acquire(dir)?;

        let scanned = scan_markdown_directory(dir)?;
        let index_bytes = scanned.index_bytes;
        let sources_bytes_opt = scanned.sources_bytes;
        let extra_md_files = scanned.extra_md_files;
        let index_str = String::from_utf8(index_bytes.clone())
            .map_err(|_| KpError::invalid("index.md", "`index.md` is not valid UTF-8"))?;
        let (frontmatter, index_body) = parse_frontmatter(&index_str)?;

        if frontmatter.page_slug != "index" {
            return Err(KpError::invalid(
                "page_slug",
                "`index.md` must declare `page_slug: index`",
            )
            .into());
        }
        if frontmatter.library_id != self.library_id {
            return Err(KpError::invalid(
                "library_id",
                "frontmatter `library_id` does not match target library",
            )
            .into());
        }
        if frontmatter.viewer != *viewer {
            return Err(StoreError::PermissionDenied);
        }

        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let caller_role = require_active_member_role_tx(&tx, viewer)?;
        let is_admin = matches!(caller_role, MemberRole::Owner | MemberRole::Maintainer);

        let recorded_meta =
            query_projection_page_tx(&tx, "index", viewer)?.ok_or(StoreError::Conflict)?;
        let sources_meta = query_projection_page_tx(&tx, "sources", viewer)?;
        let current_max_seq: i64 =
            tx.query_row("SELECT COALESCE(MAX(seq), 0) FROM change_log", [], |r| {
                r.get(0)
            })?;

        let index_sha = ContentHash::digest(&index_bytes);
        let sources_unchanged = match (&sources_bytes_opt, &sources_meta) {
            (Some(bytes), Some(meta)) => ContentHash::digest(bytes) == meta.page_sha256,
            (None, None) => true,
            (None, Some(_)) => true,
            (Some(_), None) => false,
        };
        if index_sha == recorded_meta.page_sha256 && sources_unchanged && extra_md_files.is_empty()
        {
            return Ok(ApplyReport {
                library_id: self.library_id.clone(),
                viewer: viewer.clone(),
                base_seq: frontmatter.base_seq,
                current_seq: current_max_seq,
                unchanged: true,
                baseline_conflict: false,
                updated_facts: Vec::new(),
                deleted_fact_ids: Vec::new(),
                enqueued_proposals: Vec::new(),
            });
        }

        let baseline_conflict = frontmatter.base_seq != recorded_meta.base_seq
            || current_max_seq > frontmatter.base_seq;

        let graph = read_graph(&tx)?;
        let baseline_visible = evaluate_baseline_view(&graph, viewer, frontmatter.exported_at);
        let live_evaluated = graph.evaluate(viewer, at);

        let parsed_index = parse_index_body(index_body);
        let mut freeform_candidates: Vec<FreeformCandidate> = parsed_index
            .freeform_blocks
            .into_iter()
            .enumerate()
            .map(|(idx, block)| FreeformCandidate {
                source_file: "index.md".to_owned(),
                ordinal: idx,
                raw_markdown: redactor.redact(&block),
                route_policy: Some(default_policy.clone()),
            })
            .collect();

        if let Some(ref s_bytes) = sources_bytes_opt {
            let sources_cands = extract_sources_md_candidates_tx(
                &tx,
                &SourcesMdExtractCtx {
                    graph: &graph,
                    sources_bytes: s_bytes,
                    sources_meta: sources_meta.as_ref(),
                    expected_library_id: &self.library_id,
                    expected_viewer: viewer,
                    default_policy,
                    redactor,
                },
            )?;
            freeform_candidates.extend(sources_cands);
        }

        for (file_idx, (rel_path, bytes)) in extra_md_files.into_iter().enumerate() {
            let text = String::from_utf8(bytes).map_err(|_| {
                KpError::invalid("markdown", "extra markdown file is not valid UTF-8")
            })?;
            let redacted = redactor.redact(text.trim());
            if !redacted.trim().is_empty() {
                freeform_candidates.push(FreeformCandidate {
                    source_file: rel_path,
                    ordinal: file_idx,
                    raw_markdown: redacted,
                    route_policy: Some(default_policy.clone()),
                });
            }
        }

        let mut updated_facts = Vec::new();
        let mut deleted_fact_ids = Vec::new();
        let mut enqueued_proposals = Vec::new();
        let mut seen_entry_ids = BTreeMap::<FactId, usize>::new();
        let proposal_ctx = WritebackProposalContext {
            viewer,
            base_seq: frontmatter.base_seq,
            current_seq: current_max_seq,
            at,
        };
        let wb_env = WritebackTxEnv {
            library_id: &self.library_id,
            actor: viewer,
            at,
            tokenizer: &self.tokenizer,
            embedder: self.embedder.as_ref(),
        };

        if baseline_conflict {
            let existed_at_base: BTreeSet<FactId> = {
                let mut stmt = tx.prepare(
                    "SELECT target_id FROM change_log
                     WHERE op = 'insert_fact' AND seq <= ?1 AND target_id IS NOT NULL",
                )?;
                let mut rows = stmt.query([frontmatter.base_seq])?;
                let mut out = BTreeSet::new();
                while let Some(row) = rows.next()? {
                    if let Ok(fid) = row.get::<_, String>(0)?.parse::<FactId>() {
                        out.insert(fid);
                    }
                }
                out
            };

            for (entry_idx, entry) in parsed_index.entries.iter().enumerate() {
                let redacted_raw = redactor.redact(&entry.raw_block);
                let Ok(fid) = entry.fact_id_raw.parse::<FactId>() else {
                    let target_key = format!(
                        "wb_freeform_{}",
                        &ContentHash::digest(
                            format!("index.md:entry:{entry_idx}:{redacted_raw}").as_bytes()
                        )
                        .as_str()[..24]
                    );
                    let rev_id = enqueue_writeback_proposal_tx(
                        &tx,
                        &proposal_ctx,
                        WritebackProposalInput {
                            op: WritebackProposalOp::FreeformBlock,
                            review_target_id: target_key,
                            target_fact_id: None,
                            redacted_proposed_claim: None,
                            redacted_raw_markdown: redacted_raw,
                            source_file: "index.md",
                            reason: "invalid_entry_id",
                            route_policy: Some(default_policy),
                        },
                    )?;
                    enqueued_proposals.push(rev_id);
                    continue;
                };

                let dup_count = seen_entry_ids
                    .entry(fid.clone())
                    .and_modify(|c| *c += 1)
                    .or_insert(1);

                let Some(orig_fact) = graph.facts.get(&fid) else {
                    let target_key = format!(
                        "wb_freeform_{}",
                        &ContentHash::digest(format!("missing:{fid}:{redacted_raw}").as_bytes())
                            .as_str()[..24]
                    );
                    let rev_id = enqueue_writeback_proposal_tx(
                        &tx,
                        &proposal_ctx,
                        WritebackProposalInput {
                            op: WritebackProposalOp::FreeformBlock,
                            review_target_id: target_key,
                            target_fact_id: None,
                            redacted_proposed_claim: None,
                            redacted_raw_markdown: redacted_raw,
                            source_file: "index.md",
                            reason: "unknown_fact_id",
                            route_policy: Some(default_policy),
                        },
                    )?;
                    enqueued_proposals.push(rev_id);
                    continue;
                };

                let orig_visible = is_base_fact_readable_by_actor(&graph, orig_fact, viewer)
                    || baseline_visible.contains(&fid)
                    || live_evaluated.facts.contains(&fid)
                    || is_fact_audit_visible(
                        &graph,
                        &live_evaluated,
                        orig_fact,
                        viewer,
                        &mut BTreeSet::new(),
                    );
                if !orig_visible {
                    // Distinguish departed-container entries (`existed_at_base` + static `audience`
                    // permits `viewer`, lost only due to `source_scopes` contraction) from unknown
                    // or never-readable entries (where reading `orig_fact` could create a side-channel).
                    if existed_at_base.contains(&fid)
                        && was_statically_permitted_for_viewer(
                            &graph,
                            orig_fact,
                            viewer,
                            &mut BTreeSet::new(),
                        )
                    {
                        if entry.content_lines.is_empty() {
                            continue;
                        }
                        let orig_claim =
                            sanitize_md_inline(&render_fact_claim_text(&tx, orig_fact)?);
                        let head_fact = follow_superseded_head(&graph, orig_fact);
                        let head_claim =
                            sanitize_md_inline(&render_fact_claim_text(&tx, head_fact)?);
                        let src_label = sanitize_md_inline(&resolve_fact_source_label(
                            &tx,
                            &self.library_id,
                            orig_fact,
                        )?);
                        let has_custom_sublines = entry
                            .content_lines
                            .iter()
                            .skip(1)
                            .any(|l| !is_standard_sub_bullet(l));
                        let parsed_bullet =
                            parse_standard_entry_bullet(&entry.content_lines[0], Some(&src_label))
                                .or_else(|| {
                                    parse_standard_entry_bullet(&entry.content_lines[0], None)
                                })
                                .filter(|b| kp_ref_matches_fact_id(&b.kp_ref, &fid));

                        if *dup_count == 1 && !has_custom_sublines {
                            if let Some(ref bullet) = parsed_bullet {
                                let redacted_claim =
                                    sanitize_md_inline(&redactor.redact(&bullet.claim));
                                let exported_quote = orig_fact
                                    .source_anchor
                                    .as_ref()
                                    .and_then(|a| a.verbatim_quote.as_deref())
                                    .map(strip_turn_tags_and_assistant_lines)
                                    .map(|q| sanitize_md_inline(&q))
                                    .filter(|q| !q.is_empty() && !orig_claim.contains(q));
                                let expected_quote_line =
                                    exported_quote.map(|q| format!("  - Quote: \"{q}\""));
                                let user_quote_line = entry
                                    .content_lines
                                    .iter()
                                    .skip(1)
                                    .find(|l| l.starts_with("  - Quote: "))
                                    .cloned();
                                let quote_unchanged = user_quote_line.is_none()
                                    || user_quote_line == expected_quote_line;
                                if quote_unchanged
                                    && (redacted_claim == orig_claim
                                        || redacted_claim == head_claim)
                                {
                                    continue;
                                }
                            }
                        }

                        let orig_pol = fact_effective_route_policy_in_tx(&tx, orig_fact)?;
                        let eff_pol = merge_route_policy(Some(&orig_pol), default_policy);
                        let proposed_claim = parsed_bullet
                            .map(|b| sanitize_md_inline(&redactor.redact(&b.claim)))
                            .filter(|s| !s.is_empty());
                        let target_key = format!(
                            "wb_freeform_{}",
                            &ContentHash::digest(
                                format!("departed:{fid}:{dup_count}:{redacted_raw}").as_bytes()
                            )
                            .as_str()[..24]
                        );
                        let rev_id = enqueue_writeback_proposal_tx(
                            &tx,
                            &proposal_ctx,
                            WritebackProposalInput {
                                op: WritebackProposalOp::FreeformBlock,
                                review_target_id: target_key,
                                target_fact_id: None,
                                redacted_proposed_claim: proposed_claim,
                                redacted_raw_markdown: redacted_raw,
                                source_file: "index.md",
                                reason: "departed_scope_edited_entry",
                                route_policy: Some(&eff_pol),
                            },
                        )?;
                        enqueued_proposals.push(rev_id);
                        continue;
                    }

                    let target_key = format!(
                        "wb_freeform_{}",
                        &ContentHash::digest(
                            format!("unreadable:{entry_idx}:{redacted_raw}").as_bytes()
                        )
                        .as_str()[..24]
                    );
                    let rev_id = enqueue_writeback_proposal_tx(
                        &tx,
                        &proposal_ctx,
                        WritebackProposalInput {
                            op: WritebackProposalOp::FreeformBlock,
                            review_target_id: target_key,
                            target_fact_id: None,
                            redacted_proposed_claim: None,
                            redacted_raw_markdown: redacted_raw,
                            source_file: "index.md",
                            reason: "unanchored_or_unreadable_entry",
                            route_policy: Some(default_policy),
                        },
                    )?;
                    enqueued_proposals.push(rev_id);
                    continue;
                }

                let orig_pol = fact_effective_route_policy_in_tx(&tx, orig_fact)?;
                let eff_orig_pol = merge_route_policy(Some(&orig_pol), default_policy);

                if entry.content_lines.is_empty() {
                    if orig_fact.status != FactStatus::Withdrawn {
                        let rev_id = enqueue_writeback_proposal_tx(
                            &tx,
                            &proposal_ctx,
                            WritebackProposalInput {
                                op: WritebackProposalOp::DeleteFact,
                                review_target_id: fid.to_string(),
                                target_fact_id: Some(&fid),
                                redacted_proposed_claim: None,
                                redacted_raw_markdown: redacted_raw,
                                source_file: "index.md",
                                reason: "baseline_conflict",
                                route_policy: Some(&eff_orig_pol),
                            },
                        )?;
                        enqueued_proposals.push(rev_id);
                    }
                    continue;
                }

                let orig_claim = sanitize_md_inline(&render_fact_claim_text(&tx, orig_fact)?);
                let head_fact = follow_superseded_head(&graph, orig_fact);
                let head_claim = sanitize_md_inline(&render_fact_claim_text(&tx, head_fact)?);
                let src_label = sanitize_md_inline(&resolve_fact_source_label(
                    &tx,
                    &self.library_id,
                    orig_fact,
                )?);
                let has_custom_sublines = entry
                    .content_lines
                    .iter()
                    .skip(1)
                    .any(|l| !is_standard_sub_bullet(l));

                let parsed_bullet =
                    parse_standard_entry_bullet(&entry.content_lines[0], Some(&src_label))
                        .or_else(|| parse_standard_entry_bullet(&entry.content_lines[0], None))
                        .filter(|b| kp_ref_matches_fact_id(&b.kp_ref, &fid));

                if *dup_count == 1 && !has_custom_sublines {
                    if let Some(ref bullet) = parsed_bullet {
                        let redacted_claim = sanitize_md_inline(&redactor.redact(&bullet.claim));
                        if redacted_claim == orig_claim || redacted_claim == head_claim {
                            continue;
                        }
                        let rev_id = enqueue_writeback_proposal_tx(
                            &tx,
                            &proposal_ctx,
                            WritebackProposalInput {
                                op: WritebackProposalOp::EditFact,
                                review_target_id: fid.to_string(),
                                target_fact_id: Some(&fid),
                                redacted_proposed_claim: Some(redacted_claim),
                                redacted_raw_markdown: redacted_raw,
                                source_file: "index.md",
                                reason: "baseline_conflict",
                                route_policy: Some(&eff_orig_pol),
                            },
                        )?;
                        enqueued_proposals.push(rev_id);
                        continue;
                    }
                }

                let target_key = if *dup_count > 1 {
                    format!("{fid}#dup{dup_count}")
                } else {
                    fid.to_string()
                };
                let fallback_claim = parsed_bullet
                    .map(|b| sanitize_md_inline(&redactor.redact(&b.claim)))
                    .filter(|s| !s.is_empty());
                let rev_id = enqueue_writeback_proposal_tx(
                    &tx,
                    &proposal_ctx,
                    WritebackProposalInput {
                        op: WritebackProposalOp::EditFact,
                        review_target_id: target_key,
                        target_fact_id: Some(&fid),
                        redacted_proposed_claim: fallback_claim,
                        redacted_raw_markdown: redacted_raw,
                        source_file: "index.md",
                        reason: "baseline_conflict",
                        route_policy: Some(&eff_orig_pol),
                    },
                )?;
                enqueued_proposals.push(rev_id);
            }

            // Detect base facts that existed at `frontmatter.base_seq`, were visible to `viewer`,
            // were NOT already superseded at `base_seq`, and were deleted from `index.md`.
            for fact in graph.facts.values() {
                if !existed_at_base.contains(&fact.id) || seen_entry_ids.contains_key(&fact.id) {
                    continue;
                }
                if fact.status == FactStatus::Withdrawn {
                    continue;
                }
                if fact
                    .superseded_by
                    .as_ref()
                    .is_some_and(|next_id| existed_at_base.contains(next_id))
                {
                    continue;
                }
                let FactAccess::Base { stamp, .. } = &fact.access else {
                    continue;
                };
                if !is_base_fact_readable_by_actor(&graph, fact, viewer)
                    || stamp.valid.valid_from > frontmatter.exported_at
                {
                    continue;
                }
                let claim_preview =
                    redactor.redact(&sanitize_md_inline(&render_fact_claim_text(&tx, fact)?));
                let fact_pol = merge_route_policy(
                    Some(&fact_effective_route_policy_in_tx(&tx, fact)?),
                    default_policy,
                );
                let rev_id = enqueue_writeback_proposal_tx(
                    &tx,
                    &proposal_ctx,
                    WritebackProposalInput {
                        op: WritebackProposalOp::DeleteFact,
                        review_target_id: fact.id.to_string(),
                        target_fact_id: Some(&fact.id),
                        redacted_proposed_claim: None,
                        redacted_raw_markdown: claim_preview,
                        source_file: "index.md",
                        reason: "baseline_conflict",
                        route_policy: Some(&fact_pol),
                    },
                )?;
                enqueued_proposals.push(rev_id);
            }
        } else {
            // Clean baseline (`frontmatter.base_seq == recorded_meta.base_seq && current_max_seq == base_seq`).
            for (entry_idx, entry) in parsed_index.entries.iter().enumerate() {
                let redacted_raw = redactor.redact(&entry.raw_block);
                let Ok(fid) = entry.fact_id_raw.parse::<FactId>() else {
                    let target_key = format!(
                        "wb_freeform_{}",
                        &ContentHash::digest(
                            format!("index.md:entry:{entry_idx}:{redacted_raw}").as_bytes()
                        )
                        .as_str()[..24]
                    );
                    let rev_id = enqueue_writeback_proposal_tx(
                        &tx,
                        &proposal_ctx,
                        WritebackProposalInput {
                            op: WritebackProposalOp::FreeformBlock,
                            review_target_id: target_key,
                            target_fact_id: None,
                            redacted_proposed_claim: None,
                            redacted_raw_markdown: redacted_raw,
                            source_file: "index.md",
                            reason: "invalid_entry_id",
                            route_policy: Some(default_policy),
                        },
                    )?;
                    enqueued_proposals.push(rev_id);
                    continue;
                };

                let dup_count = seen_entry_ids
                    .entry(fid.clone())
                    .and_modify(|c| *c += 1)
                    .or_insert(1);

                let (Some(db_fact), true) =
                    (graph.facts.get(&fid), baseline_visible.contains(&fid))
                else {
                    let target_key = format!(
                        "wb_freeform_{}",
                        &ContentHash::digest(
                            format!("index.md:unknown:{entry_idx}:{redacted_raw}").as_bytes()
                        )
                        .as_str()[..24]
                    );
                    let rev_id = enqueue_writeback_proposal_tx(
                        &tx,
                        &proposal_ctx,
                        WritebackProposalInput {
                            op: WritebackProposalOp::FreeformBlock,
                            review_target_id: target_key,
                            target_fact_id: None,
                            redacted_proposed_claim: None,
                            redacted_raw_markdown: redacted_raw,
                            source_file: "index.md",
                            reason: "unknown_or_invisible_entry",
                            route_policy: Some(default_policy),
                        },
                    )?;
                    enqueued_proposals.push(rev_id);
                    continue;
                };

                let db_fact_pol = merge_route_policy(
                    Some(&fact_effective_route_policy_in_tx(&tx, db_fact)?),
                    default_policy,
                );

                if entry.content_lines.is_empty() {
                    if *dup_count == 1 {
                        if matches!(db_fact.access, FactAccess::Base { .. })
                            && db_fact.status != FactStatus::Withdrawn
                            && (db_fact.author == *viewer || is_admin)
                        {
                            let withdrawn =
                                withdraw_facts(&tx, vec![db_fact.id.to_string()], viewer, at)?;
                            deleted_fact_ids.extend(withdrawn);
                        } else {
                            let rev_id = enqueue_writeback_proposal_tx(
                                &tx,
                                &proposal_ctx,
                                WritebackProposalInput {
                                    op: WritebackProposalOp::DeleteFact,
                                    review_target_id: fid.to_string(),
                                    target_fact_id: Some(&fid),
                                    redacted_proposed_claim: None,
                                    redacted_raw_markdown: redacted_raw,
                                    source_file: "index.md",
                                    reason: "requires_review_for_delete",
                                    route_policy: Some(&db_fact_pol),
                                },
                            )?;
                            enqueued_proposals.push(rev_id);
                        }
                    }
                    continue;
                }

                let orig_claim = sanitize_md_inline(&render_fact_claim_text(&tx, db_fact)?);
                let src_label =
                    sanitize_md_inline(&resolve_fact_source_label(&tx, &self.library_id, db_fact)?);
                let has_custom_sublines = entry
                    .content_lines
                    .iter()
                    .skip(1)
                    .any(|l| !is_standard_sub_bullet(l));
                let parsed_bullet =
                    parse_standard_entry_bullet(&entry.content_lines[0], Some(&src_label))
                        .filter(|b| kp_ref_matches_fact_id(&b.kp_ref, &fid));

                if *dup_count == 1 && !has_custom_sublines {
                    if let Some(ref bullet) = parsed_bullet {
                        let redacted_claim = sanitize_md_inline(&redactor.redact(&bullet.claim));
                        if redacted_claim == orig_claim {
                            // Check whether user modified the `  - Quote:` line despite leaving `<claim>` intact.
                            let exported_quote = db_fact
                                .source_anchor
                                .as_ref()
                                .and_then(|a| a.verbatim_quote.as_deref())
                                .map(strip_turn_tags_and_assistant_lines)
                                .map(|q| sanitize_md_inline(&q))
                                .filter(|q| !q.is_empty() && !orig_claim.contains(q));
                            let expected_quote_line =
                                exported_quote.map(|q| format!("  - Quote: \"{q}\""));
                            let user_quote_line = entry
                                .content_lines
                                .iter()
                                .skip(1)
                                .find(|l| l.starts_with("  - Quote: "))
                                .cloned();
                            if user_quote_line.is_none() || user_quote_line == expected_quote_line {
                                continue;
                            }
                        } else if !redacted_claim.is_empty()
                            && db_fact.status == FactStatus::Active
                            && matches!(db_fact.access, FactAccess::Base { .. })
                            && (db_fact.author == *viewer || is_admin)
                        {
                            if let Some(updated_obj) = build_updated_fact_object(
                                &tx,
                                db_fact,
                                &redacted_claim,
                                &db_fact_pol,
                            )? {
                                let update = supersede_fact_via_writeback_tx(
                                    &tx,
                                    &wb_env,
                                    db_fact,
                                    updated_obj,
                                    &redacted_claim,
                                    &db_fact_pol,
                                )?;
                                updated_facts.push(update);
                                continue;
                            }
                        }
                    }
                }

                let target_key = if *dup_count > 1 {
                    format!("{fid}#dup{dup_count}")
                } else {
                    fid.to_string()
                };
                let proposed_claim = parsed_bullet
                    .map(|b| sanitize_md_inline(&redactor.redact(&b.claim)))
                    .filter(|s| !s.is_empty());
                let reason = if matches!(db_fact.access, FactAccess::Derived { .. }) {
                    "derived_fact_edit"
                } else if db_fact.author != *viewer && !is_admin {
                    "cross_author_edit"
                } else {
                    "unstructured_entry_edit"
                };
                let rev_id = enqueue_writeback_proposal_tx(
                    &tx,
                    &proposal_ctx,
                    WritebackProposalInput {
                        op: WritebackProposalOp::EditFact,
                        review_target_id: target_key,
                        target_fact_id: Some(&fid),
                        redacted_proposed_claim: proposed_claim,
                        redacted_raw_markdown: redacted_raw,
                        source_file: "index.md",
                        reason,
                        route_policy: Some(&db_fact_pol),
                    },
                )?;
                enqueued_proposals.push(rev_id);
            }

            // Handle entries present in `baseline_visible` that were omitted from `index.md`.
            for fid in &baseline_visible {
                if seen_entry_ids.contains_key(fid) {
                    continue;
                }
                let Some(db_fact) = graph.facts.get(fid) else {
                    continue;
                };
                if matches!(db_fact.access, FactAccess::Base { .. })
                    && db_fact.status != FactStatus::Withdrawn
                    && (db_fact.author == *viewer || is_admin)
                {
                    let withdrawn = withdraw_facts(&tx, vec![db_fact.id.to_string()], viewer, at)?;
                    deleted_fact_ids.extend(withdrawn);
                } else if db_fact.status != FactStatus::Withdrawn {
                    let claim_preview = redactor
                        .redact(&sanitize_md_inline(&render_fact_claim_text(&tx, db_fact)?));
                    let db_fact_pol = merge_route_policy(
                        Some(&fact_effective_route_policy_in_tx(&tx, db_fact)?),
                        default_policy,
                    );
                    let reason = if matches!(db_fact.access, FactAccess::Derived { .. }) {
                        "derived_fact_delete"
                    } else {
                        "cross_author_delete"
                    };
                    let rev_id = enqueue_writeback_proposal_tx(
                        &tx,
                        &proposal_ctx,
                        WritebackProposalInput {
                            op: WritebackProposalOp::DeleteFact,
                            review_target_id: fid.to_string(),
                            target_fact_id: Some(fid),
                            redacted_proposed_claim: None,
                            redacted_raw_markdown: claim_preview,
                            source_file: "index.md",
                            reason,
                            route_policy: Some(&db_fact_pol),
                        },
                    )?;
                    enqueued_proposals.push(rev_id);
                }
            }
        }

        for candidate in freeform_candidates {
            if candidate.raw_markdown.trim().is_empty() {
                continue;
            }
            let target_key = format!(
                "wb_freeform_{}",
                &ContentHash::digest(
                    format!(
                        "{}:{}:{}",
                        candidate.source_file, candidate.ordinal, candidate.raw_markdown
                    )
                    .as_bytes(),
                )
                .as_str()[..24]
            );
            let inline_claim = sanitize_md_inline(&candidate.raw_markdown);
            let cand_pol = merge_route_policy(candidate.route_policy.as_ref(), default_policy);
            let rev_id = enqueue_writeback_proposal_tx(
                &tx,
                &proposal_ctx,
                WritebackProposalInput {
                    op: WritebackProposalOp::FreeformBlock,
                    review_target_id: target_key,
                    target_fact_id: None,
                    redacted_proposed_claim: (!inline_claim.is_empty()).then_some(inline_claim),
                    redacted_raw_markdown: candidate.raw_markdown,
                    source_file: &candidate.source_file,
                    reason: if baseline_conflict {
                        "baseline_conflict_freeform"
                    } else {
                        "freeform_markdown"
                    },
                    route_policy: Some(&cand_pol),
                },
            )?;
            enqueued_proposals.push(rev_id);
        }

        let final_seq: i64 =
            tx.query_row("SELECT COALESCE(MAX(seq), 0) FROM change_log", [], |r| {
                r.get(0)
            })?;
        let unchanged = updated_facts.is_empty()
            && deleted_fact_ids.is_empty()
            && enqueued_proposals.is_empty();
        tx.commit()?;
        if !updated_facts.is_empty() || !deleted_fact_ids.is_empty() {
            self.sync_skills_now()?;
        }

        Ok(ApplyReport {
            library_id: self.library_id.clone(),
            viewer: viewer.clone(),
            base_seq: frontmatter.base_seq,
            current_seq: final_seq,
            unchanged,
            baseline_conflict,
            updated_facts,
            deleted_fact_ids,
            enqueued_proposals,
        })
    }
}
