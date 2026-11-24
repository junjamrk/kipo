use std::collections::{BTreeMap, BTreeSet, VecDeque};

use kp_model::{parse_timestamp_ms_in_tz, ActorId, ContentHash, EpochMs, SecretRedactor, TimeZone};
use serde_json::Value;

use super::parsers::{
    collect_permission_members, extract_document_markdown, extract_field_epoch_ms,
    extract_minutes_text, record_identity, unwrap_data,
};
use super::{FeishuCheckpoint, FeishuConnector, FeishuMessageState, FeishuWikiNodeState};
use crate::{
    ConnectError, ConnectorScope, ExternalIdentityProposal, FeishuTransport, IngestedSourceEvent,
    PollBatch, Result, SourceEventOp, SourceScopeRoster,
};

impl<T: FeishuTransport> FeishuConnector<T> {
    pub(super) async fn sync_wiki_space(
        &self,
        space_id: &str,
        scope: &ConnectorScope,
        checkpoint: &mut FeishuCheckpoint,
        ingested_by: &ActorId,
    ) -> Result<PollBatch> {
        let mut batch = PollBatch::default();
        let mut proposals = BTreeMap::<String, ExternalIdentityProposal>::new();
        let mut roster_members = BTreeSet::<String>::new();
        let redactor = SecretRedactor::new().map_err(|e| ConnectError::Transport(e.to_string()))?;

        // Queue of `(parent_node_token, page_token)` cursor items across the wiki space tree.
        let mut queue = VecDeque::<(Option<String>, Option<String>)>::new();
        queue.push_back((None, None));
        let mut seen_nodes = BTreeSet::<String>::new();
        let mut calls = 0_usize;
        let max_calls = self.config.max_pages_per_poll.max(1);
        let mut full_traversal_complete = true;

        while !queue.is_empty() {
            if calls >= max_calls {
                full_traversal_complete = false;
                break;
            }
            let Some((parent_tok, page_tok)) = queue.pop_front() else {
                break;
            };
            calls += 1;
            let raw = self
                .transport
                .list_wiki_nodes(space_id, parent_tok.as_deref(), page_tok.as_deref())
                .await?;
            let container = unwrap_data(&raw);

            let has_more = container
                .get("has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let next_page_tok = container
                .get("page_token")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            if has_more {
                if let Some(tok) = next_page_tok {
                    queue.push_front((parent_tok.clone(), Some(tok)));
                } else {
                    full_traversal_complete = false;
                }
            }

            let Some(items) = container
                .get("items")
                .or_else(|| container.get("nodes"))
                .and_then(Value::as_array)
            else {
                continue;
            };

            for node in items {
                let Some(node_token) = node
                    .get("node_token")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                else {
                    continue;
                };
                let deleted = node
                    .get("deleted")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if deleted {
                    continue;
                }
                seen_nodes.insert(node_token.to_owned());

                if node.get("has_child").and_then(Value::as_bool) == Some(true) {
                    queue.push_back((Some(node_token.to_owned()), None));
                }

                let obj_token = node
                    .get("obj_token")
                    .and_then(Value::as_str)
                    .unwrap_or(node_token);
                let obj_type = node
                    .get("obj_type")
                    .and_then(Value::as_str)
                    .unwrap_or("docx");
                if !matches!(obj_type, "doc" | "docx" | "wiki") {
                    continue;
                }

                let edit_ms = extract_field_epoch_ms(node, "obj_edit_time")
                    .or_else(|| extract_field_epoch_ms(node, "node_create_time"))
                    .unwrap_or(self.config.now);
                checkpoint.high_water_ms =
                    Some(checkpoint.high_water_ms.unwrap_or(edit_ms.0).max(edit_ms.0));

                let owner_id = node
                    .get("owner")
                    .or_else(|| node.get("creator"))
                    .or_else(|| node.get("owner_id"))
                    .and_then(Value::as_str)
                    .unwrap_or("feishu_wiki_owner");
                let title = node
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or(obj_token);
                record_identity(&mut proposals, owner_id, owner_id);

                let perm_type = if obj_type == "doc" { "doc" } else { "docx" };
                let perm_raw = self
                    .transport
                    .list_permission_members(perm_type, obj_token)
                    .await?;
                collect_permission_members(&perm_raw, &mut roster_members, &mut proposals);

                if let Some(prev) = checkpoint.known_wiki_nodes.get(node_token).cloned() {
                    if !prev.withdrawn && edit_ms.0 <= prev.obj_edit_time_ms {
                        continue;
                    }
                    let doc_raw = self.transport.fetch_document(obj_token).await?;
                    let doc_text = redactor.redact(&extract_document_markdown(&doc_raw, title)?);
                    let next_rev = prev.revision.saturating_add(1);
                    let next_key = format!("wiki:{space_id}:{node_token}@rev{next_rev}");
                    let event_op = if prev.withdrawn {
                        SourceEventOp::Upsert
                    } else {
                        SourceEventOp::Supersede {
                            previous_external_key: prev.active_key,
                        }
                    };
                    batch.events.push(IngestedSourceEvent {
                        external_key: next_key.clone(),
                        uri: format!("feishu://wiki/{space_id}/{node_token}"),
                        media_type: "text/markdown".into(),
                        speaker_external_id: owner_id.to_owned(),
                        speaker_display_name: owner_id.to_owned(),
                        ingested_by: ingested_by.clone(),
                        source_scope_id: scope.source_scope_id.clone(),
                        content: doc_text,
                        event_op,
                        occurred_at: edit_ms,
                        model_policy: scope.model_policy.clone(),
                        thread_key: None,
                        reply_to: None,
                    });
                    checkpoint.known_wiki_nodes.insert(
                        node_token.to_owned(),
                        FeishuWikiNodeState {
                            obj_token: obj_token.to_owned(),
                            obj_type: obj_type.to_owned(),
                            obj_edit_time_ms: edit_ms.0,
                            active_key: next_key,
                            revision: next_rev,
                            withdrawn: false,
                        },
                    );
                } else {
                    let doc_raw = self.transport.fetch_document(obj_token).await?;
                    let doc_text = redactor.redact(&extract_document_markdown(&doc_raw, title)?);
                    let key = format!("wiki:{space_id}:{node_token}");
                    batch.events.push(IngestedSourceEvent {
                        external_key: key.clone(),
                        uri: format!("feishu://wiki/{space_id}/{node_token}"),
                        media_type: "text/markdown".into(),
                        speaker_external_id: owner_id.to_owned(),
                        speaker_display_name: owner_id.to_owned(),
                        ingested_by: ingested_by.clone(),
                        source_scope_id: scope.source_scope_id.clone(),
                        content: doc_text,
                        event_op: SourceEventOp::Upsert,
                        occurred_at: edit_ms,
                        model_policy: scope.model_policy.clone(),
                        thread_key: None,
                        reply_to: None,
                    });
                    checkpoint.known_wiki_nodes.insert(
                        node_token.to_owned(),
                        FeishuWikiNodeState {
                            obj_token: obj_token.to_owned(),
                            obj_type: obj_type.to_owned(),
                            obj_edit_time_ms: edit_ms.0,
                            active_key: key,
                            revision: 0,
                            withdrawn: false,
                        },
                    );
                }
            }
        }

        // WeKnora full-traversal deletion reconciliation: ONLY reconcile missing nodes when
        // every queued folder AND every `has_more` page in the space has completed!
        if full_traversal_complete && queue.is_empty() {
            for (node_token, state) in &mut checkpoint.known_wiki_nodes {
                if !state.withdrawn && !seen_nodes.contains(node_token) {
                    batch.events.push(IngestedSourceEvent {
                        external_key: state.active_key.clone(),
                        uri: format!("feishu://wiki/{space_id}/{node_token}"),
                        media_type: "text/markdown".into(),
                        speaker_external_id: "feishu_wiki".into(),
                        speaker_display_name: "Feishu Wiki".into(),
                        ingested_by: ingested_by.clone(),
                        source_scope_id: scope.source_scope_id.clone(),
                        content: String::new(),
                        event_op: SourceEventOp::Withdraw,
                        occurred_at: self.config.now,
                        model_policy: scope.model_policy.clone(),
                        thread_key: None,
                        reply_to: None,
                    });
                    state.withdrawn = true;
                }
            }
        }

        if !roster_members.is_empty() {
            batch.scope_rosters.push(SourceScopeRoster {
                scope_id: scope.source_scope_id.clone(),
                provider: "feishu".into(),
                external_container_id: format!("wiki:{space_id}"),
                label: scope.label.clone(),
                external_member_ids: roster_members.into_iter().collect(),
                all_library_members: false,
                synced_at: self.config.now,
            });
        }

        batch.identity_proposals = proposals.into_values().collect();
        batch.next_cursor = checkpoint.into_cursor(self.config.now)?;
        Ok(batch)
    }

    pub(super) async fn sync_document(
        &self,
        doc_type: &str,
        doc_token: &str,
        scope: &ConnectorScope,
        checkpoint: &mut FeishuCheckpoint,
        ingested_by: &ActorId,
    ) -> Result<PollBatch> {
        let mut batch = PollBatch::default();
        let mut proposals = BTreeMap::<String, ExternalIdentityProposal>::new();
        let mut roster_members = BTreeSet::<String>::new();
        let redactor = SecretRedactor::new().map_err(|e| ConnectError::Transport(e.to_string()))?;

        let perm_raw = self
            .transport
            .list_permission_members(doc_type, doc_token)
            .await?;
        collect_permission_members(&perm_raw, &mut roster_members, &mut proposals);
        batch.scope_rosters.push(SourceScopeRoster {
            scope_id: scope.source_scope_id.clone(),
            provider: "feishu".into(),
            external_container_id: format!("{doc_type}:{doc_token}"),
            label: scope.label.clone(),
            external_member_ids: roster_members.into_iter().collect(),
            all_library_members: false,
            synced_at: self.config.now,
        });

        let doc_raw = self.transport.fetch_document(doc_token).await?;
        let container = unwrap_data(&doc_raw);
        let title = container
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or(&scope.label);
        let owner_id = container
            .get("owner_id")
            .or_else(|| container.get("creator"))
            .and_then(Value::as_str)
            .unwrap_or("feishu_doc_owner");
        record_identity(&mut proposals, owner_id, owner_id);

        let edit_ms = extract_field_epoch_ms(container, "obj_edit_time")
            .or_else(|| {
                container
                    .pointer("/document/revision_id")
                    .and_then(Value::as_i64)
                    .map(EpochMs)
            })
            .unwrap_or(self.config.now);
        let markdown = redactor.redact(&extract_document_markdown(&doc_raw, title)?);
        let content_sha = ContentHash::digest(markdown.as_bytes()).to_string();

        if let Some(prev) = checkpoint.known_messages.get(doc_token).cloned() {
            if prev.content_sha256 != content_sha {
                let next_rev = prev.revision.saturating_add(1);
                let next_key = format!("doc:{doc_token}@rev{next_rev}");
                batch.events.push(IngestedSourceEvent {
                    external_key: next_key.clone(),
                    uri: format!("feishu://docx/{doc_token}"),
                    media_type: "text/markdown".into(),
                    speaker_external_id: owner_id.to_owned(),
                    speaker_display_name: owner_id.to_owned(),
                    ingested_by: ingested_by.clone(),
                    source_scope_id: scope.source_scope_id.clone(),
                    content: markdown,
                    event_op: SourceEventOp::Supersede {
                        previous_external_key: prev.active_key,
                    },
                    occurred_at: edit_ms,
                    model_policy: scope.model_policy.clone(),
                    thread_key: None,
                    reply_to: None,
                });
                checkpoint.known_messages.insert(
                    doc_token.to_owned(),
                    FeishuMessageState {
                        active_key: next_key,
                        content_sha256: content_sha,
                        update_time_ms: edit_ms.0,
                        revision: next_rev,
                        withdrawn: false,
                    },
                );
            }
        } else {
            let key = format!("doc:{doc_token}");
            batch.events.push(IngestedSourceEvent {
                external_key: key.clone(),
                uri: format!("feishu://docx/{doc_token}"),
                media_type: "text/markdown".into(),
                speaker_external_id: owner_id.to_owned(),
                speaker_display_name: owner_id.to_owned(),
                ingested_by: ingested_by.clone(),
                source_scope_id: scope.source_scope_id.clone(),
                content: markdown,
                event_op: SourceEventOp::Upsert,
                occurred_at: edit_ms,
                model_policy: scope.model_policy.clone(),
                thread_key: None,
                reply_to: None,
            });
            checkpoint.known_messages.insert(
                doc_token.to_owned(),
                FeishuMessageState {
                    active_key: key,
                    content_sha256: content_sha,
                    update_time_ms: edit_ms.0,
                    revision: 0,
                    withdrawn: false,
                },
            );
        }

        checkpoint.high_water_ms =
            Some(checkpoint.high_water_ms.unwrap_or(edit_ms.0).max(edit_ms.0));
        batch.identity_proposals = proposals.into_values().collect();
        batch.next_cursor = checkpoint.into_cursor(self.config.now)?;
        Ok(batch)
    }

    pub(super) async fn sync_minutes(
        &self,
        minute_token: &str,
        scope: &ConnectorScope,
        checkpoint: &mut FeishuCheckpoint,
        ingested_by: &ActorId,
    ) -> Result<PollBatch> {
        let mut batch = PollBatch::default();
        let mut proposals = BTreeMap::<String, ExternalIdentityProposal>::new();
        let mut roster_members = BTreeSet::<String>::new();
        let redactor = SecretRedactor::new().map_err(|e| ConnectError::Transport(e.to_string()))?;

        let raw = self
            .transport
            .fetch_minutes_transcript(minute_token)
            .await?;
        let container = unwrap_data(&raw);

        let first_note = container
            .get("notes")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first());

        let owner_id = first_note
            .and_then(|n| n.get("creator_id"))
            .or_else(|| container.get("owner_id"))
            .or_else(|| container.pointer("/minute/owner_id"))
            .and_then(Value::as_str)
            .unwrap_or("feishu_minutes_owner");
        record_identity(&mut proposals, owner_id, owner_id);
        if owner_id != "feishu_minutes_owner" && !owner_id.trim().is_empty() {
            roster_members.insert(owner_id.to_owned());
        }
        for arr in [
            container.get("participants").and_then(Value::as_array),
            container.get("speakers").and_then(Value::as_array),
            first_note
                .and_then(|n| n.get("participants"))
                .and_then(Value::as_array),
        ]
        .into_iter()
        .flatten()
        {
            for entry in arr {
                let ext_id = entry
                    .as_str()
                    .or_else(|| entry.get("user_id").and_then(Value::as_str))
                    .or_else(|| entry.get("open_id").and_then(Value::as_str))
                    .or_else(|| entry.get("id").and_then(Value::as_str));
                if let Some(uid) = ext_id.map(str::trim).filter(|s| !s.is_empty()) {
                    let name = entry.get("name").and_then(Value::as_str).unwrap_or(uid);
                    record_identity(&mut proposals, uid, name);
                    roster_members.insert(uid.to_owned());
                }
            }
        }
        batch.scope_rosters.push(SourceScopeRoster {
            scope_id: scope.source_scope_id.clone(),
            provider: "feishu".into(),
            external_container_id: format!("minutes:{minute_token}"),
            label: scope.label.clone(),
            external_member_ids: roster_members.into_iter().collect(),
            all_library_members: false,
            synced_at: self.config.now,
        });

        let linked_doc_token = first_note
            .and_then(|n| {
                n.get("verbatim_doc_token")
                    .or_else(|| n.get("note_doc_token"))
            })
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());

        let raw_transcript = if let Some(doc_tok) = linked_doc_token {
            let doc_val = self.transport.fetch_document(doc_tok).await?;
            extract_document_markdown(&doc_val, &scope.label)?
        } else {
            extract_minutes_text(container, &scope.label)
        };

        let transcript = redactor.redact(&raw_transcript);
        let content_sha = ContentHash::digest(transcript.as_bytes()).to_string();
        if checkpoint
            .known_messages
            .get(minute_token)
            .is_none_or(|prev| prev.content_sha256 != content_sha)
        {
            let key = format!("minutes:{minute_token}");
            batch.events.push(IngestedSourceEvent {
                external_key: key.clone(),
                uri: format!("feishu://minutes/{minute_token}"),
                media_type: "text/markdown".into(),
                speaker_external_id: owner_id.to_owned(),
                speaker_display_name: owner_id.to_owned(),
                ingested_by: ingested_by.clone(),
                source_scope_id: scope.source_scope_id.clone(),
                content: transcript,
                event_op: SourceEventOp::Upsert,
                occurred_at: self.config.now,
                model_policy: scope.model_policy.clone(),
                thread_key: None,
                reply_to: None,
            });
            checkpoint.known_messages.insert(
                minute_token.to_owned(),
                FeishuMessageState {
                    active_key: key,
                    content_sha256: content_sha,
                    update_time_ms: self.config.now.0,
                    revision: 0,
                    withdrawn: false,
                },
            );
        }

        checkpoint.high_water_ms = Some(self.config.now.0);
        batch.identity_proposals = proposals.into_values().collect();
        batch.next_cursor = checkpoint.into_cursor(self.config.now)?;
        Ok(batch)
    }

    pub(super) async fn sync_calendar(
        &self,
        calendar_id: &str,
        scope: &ConnectorScope,
        checkpoint: &mut FeishuCheckpoint,
        ingested_by: &ActorId,
    ) -> Result<PollBatch> {
        let mut batch = PollBatch::default();
        let mut proposals = BTreeMap::<String, ExternalIdentityProposal>::new();
        let mut roster_members = BTreeSet::<String>::new();
        let start = EpochMs((self.config.now.0 - self.config.initial_backfill_sec * 1_000).max(0));
        let end = EpochMs(self.config.now.0 + 30 * 86_400 * 1_000);
        let raw = self
            .transport
            .list_calendar_events(calendar_id, start, end)
            .await?;
        let container = unwrap_data(&raw);
        if let Some(items) = container.get("items").and_then(Value::as_array) {
            for item in items {
                let Some(event_id) = item.get("event_id").and_then(Value::as_str) else {
                    continue;
                };
                let status = item
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("confirmed");
                let organizer = item
                    .pointer("/organizer_calendar_id")
                    .or_else(|| item.get("creator_id"))
                    .and_then(Value::as_str)
                    .unwrap_or("feishu_calendar");
                record_identity(&mut proposals, organizer, organizer);
                if organizer != "feishu_calendar" && !organizer.trim().is_empty() {
                    roster_members.insert(organizer.to_owned());
                }
                if let Some(attendees) = item.get("attendees").and_then(Value::as_array) {
                    for att in attendees {
                        let ext_id = att
                            .as_str()
                            .or_else(|| att.get("user_id").and_then(Value::as_str))
                            .or_else(|| att.get("open_id").and_then(Value::as_str))
                            .or_else(|| att.get("id").and_then(Value::as_str));
                        if let Some(uid) = ext_id.map(str::trim).filter(|s| !s.is_empty()) {
                            let name = att
                                .get("display_name")
                                .and_then(Value::as_str)
                                .unwrap_or(uid);
                            record_identity(&mut proposals, uid, name);
                            roster_members.insert(uid.to_owned());
                        }
                    }
                }

                let summary = item
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or("(untitled event)");
                let start_ms = item
                    .pointer("/start_time/timestamp")
                    .and_then(Value::as_str)
                    .and_then(|s| parse_timestamp_ms_in_tz(s, &TimeZone::UTC))
                    .unwrap_or(self.config.now);
                if status == "cancelled" {
                    batch.events.push(IngestedSourceEvent {
                        external_key: format!("cal:{calendar_id}:{event_id}"),
                        uri: format!("feishu://calendar/{calendar_id}/{event_id}"),
                        media_type: "text/plain".into(),
                        speaker_external_id: organizer.to_owned(),
                        speaker_display_name: organizer.to_owned(),
                        ingested_by: ingested_by.clone(),
                        source_scope_id: scope.source_scope_id.clone(),
                        content: String::new(),
                        event_op: SourceEventOp::Withdraw,
                        occurred_at: start_ms,
                        model_policy: scope.model_policy.clone(),
                        thread_key: None,
                        reply_to: None,
                    });
                } else {
                    batch.events.push(IngestedSourceEvent {
                        external_key: format!("cal:{calendar_id}:{event_id}"),
                        uri: format!("feishu://calendar/{calendar_id}/{event_id}"),
                        media_type: "text/plain".into(),
                        speaker_external_id: organizer.to_owned(),
                        speaker_display_name: organizer.to_owned(),
                        ingested_by: ingested_by.clone(),
                        source_scope_id: scope.source_scope_id.clone(),
                        content: summary.to_owned(),
                        event_op: SourceEventOp::Upsert,
                        occurred_at: start_ms,
                        model_policy: scope.model_policy.clone(),
                        thread_key: None,
                        reply_to: None,
                    });
                }
            }
        }
        batch.scope_rosters.push(SourceScopeRoster {
            scope_id: scope.source_scope_id.clone(),
            provider: "feishu".into(),
            external_container_id: format!("calendar:{calendar_id}"),
            label: scope.label.clone(),
            external_member_ids: roster_members.into_iter().collect(),
            all_library_members: false,
            synced_at: self.config.now,
        });
        checkpoint.high_water_ms = Some(self.config.now.0);
        batch.identity_proposals = proposals.into_values().collect();
        batch.next_cursor = checkpoint.into_cursor(self.config.now)?;
        Ok(batch)
    }
}
