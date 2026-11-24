use std::collections::{BTreeMap, BTreeSet};

use kp_model::{ActorId, ContentHash, SecretRedactor};
use serde_json::Value;

use super::parsers::{
    extract_field_epoch_ms, extract_message_text, extract_sender, record_identity, unwrap_data,
};
use super::{FeishuCheckpoint, FeishuConnector, FeishuMessageState};
use crate::{
    ConnectError, ConnectorScope, ExternalIdentityProposal, FeishuTransport, IngestedSourceEvent,
    PollBatch, Result, SourceEventOp, SourceScopeRoster,
};

struct ChatSyncCtx<'a> {
    chat_id: &'a str,
    member_names: &'a BTreeMap<String, String>,
    scope: &'a ConnectorScope,
    checkpoint: &'a mut FeishuCheckpoint,
    ingested_by: &'a ActorId,
    redactor: &'a SecretRedactor,
    proposals: &'a mut BTreeMap<String, ExternalIdentityProposal>,
    batch: &'a mut PollBatch,
}

impl<T: FeishuTransport> FeishuConnector<T> {
    pub(super) async fn sync_chat(
        &self,
        chat_id: &str,
        scope: &ConnectorScope,
        checkpoint: &mut FeishuCheckpoint,
        ingested_by: &ActorId,
    ) -> Result<PollBatch> {
        let mut batch = PollBatch::default();
        let mut proposals = BTreeMap::<String, ExternalIdentityProposal>::new();
        let mut member_names = BTreeMap::<String, String>::new();
        let redactor = SecretRedactor::new().map_err(|e| ConnectError::Transport(e.to_string()))?;

        // 1. Live chat membership roster -> `source_scopes` (`SourceScopeRoster`)
        let mut member_ids = BTreeSet::new();
        let mut member_page: Option<String> = None;
        for _ in 0..self.config.max_pages_per_poll.max(1) {
            let raw = self
                .transport
                .list_chat_members(chat_id, member_page.as_deref())
                .await?;
            let container = unwrap_data(&raw);
            if let Some(items) = container
                .get("items")
                .or_else(|| container.get("members"))
                .and_then(Value::as_array)
            {
                for item in items {
                    if let Some(id) = item
                        .get("member_id")
                        .or_else(|| item.get("open_id"))
                        .or_else(|| item.get("id"))
                        .and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                    {
                        let name = item
                            .get("name")
                            .or_else(|| item.get("display_name"))
                            .and_then(Value::as_str)
                            .unwrap_or(id);
                        member_ids.insert(id.to_owned());
                        member_names.insert(id.to_owned(), name.to_owned());
                        record_identity(&mut proposals, id, name);
                    }
                }
            }
            let has_more = container
                .get("has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let next_tok = container
                .get("page_token")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            if has_more && next_tok.is_some() {
                member_page = next_tok;
            } else {
                break;
            }
        }

        batch.scope_rosters.push(SourceScopeRoster {
            scope_id: scope.source_scope_id.clone(),
            provider: "feishu".into(),
            external_container_id: chat_id.to_owned(),
            label: scope.label.clone(),
            external_member_ids: member_ids.into_iter().collect(),
            all_library_members: false,
            synced_at: self.config.now,
        });

        // 2. Resumable bounded window + recent-window message re-scan for `deleted` / `updated`
        let now_sec = self.config.now.0.div_euclid(1_000);
        let (window_start_sec, window_end_sec, mut page_token) =
            if let Some(resume_tok) = checkpoint.pending_page_token.clone() {
                (
                    checkpoint
                        .pending_window_start_sec
                        .unwrap_or_else(|| (now_sec - self.config.initial_backfill_sec).max(0)),
                    checkpoint.pending_window_end_sec.unwrap_or(now_sec),
                    Some(resume_tok),
                )
            } else if let Some(hw_ms) = checkpoint.high_water_ms {
                let hw_sec = hw_ms.div_euclid(1_000);
                let rescan_start = (hw_sec - self.config.rescan_window_sec).max(0);
                (rescan_start, now_sec, None)
            } else {
                let init_start = (now_sec - self.config.initial_backfill_sec).max(0);
                (init_start, now_sec, None)
            };

        let mut threads_needing_fetch = checkpoint.known_threads.clone();
        let mut threads_inline_complete = BTreeSet::<String>::new();

        let mut pages_fetched = 0_usize;
        let mut completed_window = true;
        while pages_fetched < self.config.max_pages_per_poll.max(1) {
            pages_fetched += 1;
            let raw = self
                .transport
                .list_chat_messages(
                    chat_id,
                    Some(window_start_sec),
                    Some(window_end_sec),
                    page_token.as_deref(),
                )
                .await?;
            let container = unwrap_data(&raw);
            // `im +chat-messages-list` returns `data.messages[]`; raw OpenAPI returns `data.items[]`.
            let msgs = container
                .get("messages")
                .or_else(|| container.get("items"))
                .and_then(Value::as_array);

            if let Some(items) = msgs {
                for msg in items {
                    {
                        let mut ctx = ChatSyncCtx {
                            chat_id,
                            member_names: &member_names,
                            scope,
                            checkpoint,
                            ingested_by,
                            redactor: &redactor,
                            proposals: &mut proposals,
                            batch: &mut batch,
                        };
                        self.process_chat_message(msg, &mut ctx);
                    }

                    if let Some(tid) = msg
                        .get("thread_id")
                        .and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                    {
                        checkpoint.known_threads.insert(tid.to_owned());
                        let has_more_inline = msg
                            .get("thread_has_more")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        if let Some(inline_replies) =
                            msg.get("thread_replies").and_then(Value::as_array)
                        {
                            let mut ctx = ChatSyncCtx {
                                chat_id,
                                member_names: &member_names,
                                scope,
                                checkpoint,
                                ingested_by,
                                redactor: &redactor,
                                proposals: &mut proposals,
                                batch: &mut batch,
                            };
                            for reply in inline_replies {
                                self.process_chat_message(reply, &mut ctx);
                            }
                            if !has_more_inline {
                                threads_inline_complete.insert(tid.to_owned());
                            } else {
                                threads_needing_fetch.insert(tid.to_owned());
                            }
                        } else {
                            threads_needing_fetch.insert(tid.to_owned());
                        }
                    }
                }
            }

            let has_more = container
                .get("has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let next_tok = container
                .get("page_token")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            if has_more && next_tok.is_some() {
                page_token = next_tok;
                if pages_fetched >= self.config.max_pages_per_poll.max(1) {
                    completed_window = false;
                    break;
                }
            } else {
                page_token = None;
                break;
            }
        }

        // 3. Expand thread replies via `GET /open-apis/im/v1/messages` (`container_id_type=thread`).
        for thread_id in threads_needing_fetch {
            if threads_inline_complete.contains(&thread_id) {
                continue;
            }
            let mut thread_page: Option<String> = None;
            for _ in 0..self.config.max_pages_per_poll.max(1) {
                let thread_raw = self
                    .transport
                    .list_thread_messages(&thread_id, thread_page.as_deref())
                    .await?;
                let thread_container = unwrap_data(&thread_raw);
                if let Some(items) = thread_container
                    .get("items")
                    .or_else(|| thread_container.get("messages"))
                    .and_then(Value::as_array)
                {
                    let mut ctx = ChatSyncCtx {
                        chat_id,
                        member_names: &member_names,
                        scope,
                        checkpoint,
                        ingested_by,
                        redactor: &redactor,
                        proposals: &mut proposals,
                        batch: &mut batch,
                    };
                    for reply in items {
                        self.process_chat_message(reply, &mut ctx);
                    }
                }
                let has_more = thread_container
                    .get("has_more")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let next_tok = thread_container
                    .get("page_token")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                if has_more && next_tok.is_some() {
                    thread_page = next_tok;
                } else {
                    break;
                }
            }
        }

        if completed_window {
            checkpoint.pending_page_token = None;
            checkpoint.pending_window_start_sec = None;
            checkpoint.pending_window_end_sec = None;
            let next_hw = checkpoint.high_water_ms.unwrap_or(0).max(self.config.now.0);
            checkpoint.high_water_ms = Some(next_hw);
        } else {
            checkpoint.pending_page_token = page_token;
            checkpoint.pending_window_start_sec = Some(window_start_sec);
            checkpoint.pending_window_end_sec = Some(window_end_sec);
        }

        batch.identity_proposals = proposals.into_values().collect();
        batch.next_cursor = checkpoint.into_cursor(self.config.now)?;
        Ok(batch)
    }

    fn process_chat_message(&self, msg: &Value, ctx: &mut ChatSyncCtx<'_>) {
        let ChatSyncCtx {
            chat_id,
            member_names,
            scope,
            checkpoint,
            ingested_by,
            redactor,
            proposals,
            batch,
        } = ctx;
        let chat_id = *chat_id;
        let member_names = *member_names;
        let scope = *scope;
        let ingested_by = *ingested_by;
        let redactor = *redactor;
        let Some(message_id) = msg
            .get("message_id")
            .or_else(|| msg.get("id"))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
        else {
            return;
        };

        let msg_type = msg
            .get("msg_type")
            .and_then(Value::as_str)
            .unwrap_or("text");
        if msg_type == "system"
            || msg.pointer("/sender/sender_type").and_then(Value::as_str) == Some("system")
        {
            return;
        }

        let (speaker_id, speaker_name) = extract_sender(msg, member_names);
        if !speaker_id.is_empty() {
            record_identity(proposals, &speaker_id, &speaker_name);
        }

        let create_ms = extract_field_epoch_ms(msg, "create_time").unwrap_or(self.config.now);
        let update_ms = extract_field_epoch_ms(msg, "update_time").unwrap_or(create_ms);
        let occurred_at = update_ms.max(create_ms);

        checkpoint.high_water_ms = Some(
            checkpoint
                .high_water_ms
                .unwrap_or(occurred_at.0)
                .max(occurred_at.0),
        );

        let thread_key = msg
            .get("thread_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                msg.get("root_id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| format!("chat:{chat_id}"));
        let reply_to = msg
            .get("parent_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|parent_id| {
                checkpoint
                    .known_messages
                    .get(parent_id)
                    .map(|s| s.active_key.clone())
                    .unwrap_or_else(|| parent_id.to_owned())
            });

        let deleted = msg.get("deleted").and_then(Value::as_bool).unwrap_or(false);
        let updated = msg.get("updated").and_then(Value::as_bool).unwrap_or(false);

        if deleted {
            if checkpoint
                .known_messages
                .get(message_id)
                .is_some_and(|prev| prev.withdrawn)
            {
                return;
            }
            let prev_state = checkpoint.known_messages.get(message_id).cloned();
            let active_key = prev_state
                .as_ref()
                .map(|s| s.active_key.clone())
                .unwrap_or_else(|| message_id.to_owned());
            let revision = prev_state.as_ref().map_or(0, |s| s.revision);
            batch.events.push(IngestedSourceEvent {
                external_key: active_key.clone(),
                uri: format!("feishu://chat/{chat_id}/{message_id}"),
                media_type: "text/plain".into(),
                speaker_external_id: speaker_id,
                speaker_display_name: speaker_name,
                ingested_by: ingested_by.clone(),
                source_scope_id: scope.source_scope_id.clone(),
                content: String::new(),
                event_op: SourceEventOp::Withdraw,
                occurred_at,
                model_policy: scope.model_policy.clone(),
                thread_key: Some(thread_key),
                reply_to,
            });
            checkpoint.known_messages.insert(
                message_id.to_owned(),
                FeishuMessageState {
                    active_key,
                    content_sha256: String::new(),
                    update_time_ms: occurred_at.0,
                    revision,
                    withdrawn: true,
                },
            );
            return;
        }

        let raw_content = extract_message_text(msg);
        let clean_content = redactor.redact(raw_content.trim());
        if clean_content.trim().is_empty() {
            if let Some(prev) = checkpoint.known_messages.get(message_id).cloned() {
                if !prev.withdrawn {
                    batch.events.push(IngestedSourceEvent {
                        external_key: prev.active_key.clone(),
                        uri: format!("feishu://chat/{chat_id}/{message_id}"),
                        media_type: "text/plain".into(),
                        speaker_external_id: speaker_id,
                        speaker_display_name: speaker_name,
                        ingested_by: ingested_by.clone(),
                        source_scope_id: scope.source_scope_id.clone(),
                        content: String::new(),
                        event_op: SourceEventOp::Withdraw,
                        occurred_at,
                        model_policy: scope.model_policy.clone(),
                        thread_key: Some(thread_key),
                        reply_to,
                    });
                    checkpoint.known_messages.insert(
                        message_id.to_owned(),
                        FeishuMessageState {
                            active_key: prev.active_key,
                            content_sha256: String::new(),
                            update_time_ms: occurred_at.0,
                            revision: prev.revision,
                            withdrawn: true,
                        },
                    );
                    return;
                }
            }
            batch.skipped_low_signal += 1;
            return;
        }

        let content_sha = ContentHash::digest(clean_content.as_bytes()).to_string();
        if let Some(prev) = checkpoint.known_messages.get(message_id).cloned() {
            if !prev.withdrawn
                && prev.content_sha256 == content_sha
                && (!updated || prev.update_time_ms >= occurred_at.0)
            {
                return;
            }
            let next_rev = prev.revision.saturating_add(1);
            let next_key = format!("{message_id}@rev{next_rev}");
            let event_op = if prev.withdrawn {
                SourceEventOp::Upsert
            } else {
                SourceEventOp::Supersede {
                    previous_external_key: prev.active_key,
                }
            };
            batch.events.push(IngestedSourceEvent {
                external_key: next_key.clone(),
                uri: format!("feishu://chat/{chat_id}/{message_id}"),
                media_type: "text/plain".into(),
                speaker_external_id: speaker_id,
                speaker_display_name: speaker_name,
                ingested_by: ingested_by.clone(),
                source_scope_id: scope.source_scope_id.clone(),
                content: clean_content,
                event_op,
                occurred_at,
                model_policy: scope.model_policy.clone(),
                thread_key: Some(thread_key),
                reply_to,
            });
            checkpoint.known_messages.insert(
                message_id.to_owned(),
                FeishuMessageState {
                    active_key: next_key,
                    content_sha256: content_sha,
                    update_time_ms: occurred_at.0,
                    revision: next_rev,
                    withdrawn: false,
                },
            );
        } else if updated {
            let next_key = format!("{message_id}@rev1");
            batch.events.push(IngestedSourceEvent {
                external_key: next_key.clone(),
                uri: format!("feishu://chat/{chat_id}/{message_id}"),
                media_type: "text/plain".into(),
                speaker_external_id: speaker_id,
                speaker_display_name: speaker_name,
                ingested_by: ingested_by.clone(),
                source_scope_id: scope.source_scope_id.clone(),
                content: clean_content,
                event_op: SourceEventOp::Supersede {
                    previous_external_key: message_id.to_owned(),
                },
                occurred_at,
                model_policy: scope.model_policy.clone(),
                thread_key: Some(thread_key),
                reply_to,
            });
            checkpoint.known_messages.insert(
                message_id.to_owned(),
                FeishuMessageState {
                    active_key: next_key,
                    content_sha256: content_sha,
                    update_time_ms: occurred_at.0,
                    revision: 1,
                    withdrawn: false,
                },
            );
        } else {
            batch.events.push(IngestedSourceEvent {
                external_key: message_id.to_owned(),
                uri: format!("feishu://chat/{chat_id}/{message_id}"),
                media_type: "text/plain".into(),
                speaker_external_id: speaker_id,
                speaker_display_name: speaker_name,
                ingested_by: ingested_by.clone(),
                source_scope_id: scope.source_scope_id.clone(),
                content: clean_content,
                event_op: SourceEventOp::Upsert,
                occurred_at,
                model_policy: scope.model_policy.clone(),
                thread_key: Some(thread_key),
                reply_to,
            });
            checkpoint.known_messages.insert(
                message_id.to_owned(),
                FeishuMessageState {
                    active_key: message_id.to_owned(),
                    content_sha256: content_sha,
                    update_time_ms: occurred_at.0,
                    revision: 0,
                    withdrawn: false,
                },
            );
        }
    }
}
