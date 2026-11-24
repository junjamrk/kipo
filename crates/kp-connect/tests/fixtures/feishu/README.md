# Feishu Recorded Fixture Provenance

All JSON fixtures in this directory strictly follow the `larksuite/cli` `--format json` output envelope (`{"ok": true, "identity": "user", "data": ...}`) wrapping the exact shortcut or official Feishu Open Platform REST v1/v2/v4 payloads (zero personal/real user data):

1. `pm_chat_members_initial.json`, `pm_chat_members_after_departure.json`, `eng_chat_members_initial.json`
   - CLI command: `lark-cli im chat.members get --as user --format json --params '{"chat_id":"...","member_id_type":"open_id","page_size":100}'`
   - Upstream schema source: `lark-cli schema im.chat.members.get` & Feishu OpenAPI `GET /open-apis/im/v1/chats/:chat_id/members` (`https://open.feishu.cn/document/uAjLw4CM/ukTMukTMukTM/reference/im-v1/chat-members/get`), returning `data.items[].{member_id_type, member_id, name, tenant_key}`, `data.page_token`, `data.has_more`, `data.member_total`.

2. `pm_chat_messages_initial.json`, `pm_chat_messages_rescan_edit.json`, `pm_chat_messages_rescan_recall.json`, `eng_chat_messages_initial.json`
   - CLI command: `lark-cli im +chat-messages-list --as user --chat-id <oc_id> --sort asc --page-size 50 --format json`
   - Upstream schema source: `lark-cli skills read lark-im references/lark-im-chat-messages-list.md` & `lark-im-message-enrichment.md` (`github.com/larksuite/cli` `shortcuts/im/im_chat_messages_list.go`), returning `data.messages[]` with top-level `message_id`, `msg_type`, `create_time`, optional `update_time` (present when `updated == true`), `sender: {id, name}`, rendered `content`, `deleted` (bool), `updated` (bool), optional `mentions`, and optional `thread_id` (`omt_xxx`), alongside `data.total`, `data.has_more`, `data.page_token`.

3. `eng_thread_messages.json`
   - CLI command: `lark-cli api GET /open-apis/im/v1/messages --as user --format json --params '{"container_id_type":"thread","container_id":"omt_eng_cutover_thread","sort_type":"ByCreateTimeAsc","page_size":50}'`
   - Upstream schema source: Feishu OpenAPI `GET /open-apis/im/v1/messages` (`https://open.feishu.cn/document/server-docs/im-v1/message/list`), returning `data.items[]` where each message has `message_id`, `root_id`, `parent_id`, `thread_id`, `chat_id`, `msg_type`, `create_time`, `update_time`, `deleted`, `updated`, `sender: {id, id_type, sender_type, tenant_key}`, and `body: {content: "{\"text\":\"...\"}"}`.
   - Exercises both the `im +chat-messages-list` shortcut (`data.messages[]`) on root messages and the raw OpenAPI (`data.items[].body.content`) on thread reply expansion.

4. `wiki_nodes_initial.json`, `wiki_nodes_updated_and_deleted.json`
   - CLI command: `lark-cli wiki nodes list --as user --format json --params '{"space_id":"7410001","page_size":50}'`
   - Upstream schema source: `lark-cli schema wiki.nodes.list` & Feishu OpenAPI `GET /open-apis/wiki/v2/spaces/:space_id/nodes` (`https://open.feishu.cn/document/ukTMukTMukTM/uUDN04SN0QjL1QDN/wiki-v2/space-node/list`), returning `data.items[].{space_id, node_token, obj_token, obj_type, parent_node_token, node_type, origin_node_token, origin_space_id, has_child, title, obj_create_time, obj_edit_time, node_create_time, creator, owner}`.

5. `drive_perm_members.json`
   - CLI command: `lark-cli api GET /open-apis/drive/v1/permissions/<token>/members --as user --format json --params '{"type":"docx","fields":"*"}'`
   - Upstream schema source: Feishu Drive v1 Permission Members API (`https://open.feishu.cn/document/server-docs/docs/permission/permission-member/list`), returning `data.items[].{member_type, member_id, perm, perm_type, type, name}`.

6. Document & Minutes responses (used in unit/integration tests):
   - `lark-cli docs +fetch --as user --doc <token> --format json`: verified via `lark-cli skills read lark-doc references/lark-doc-fetch.md`, returning `{"ok": true, "identity": "user", "data": {"document": {"document_id": "...", "revision_id": 1, "content": "<title>...</title><p>...</p>"}}}`.
   - `lark-cli vc +notes --as user --minute-tokens <token> --output-dir minutes_out --format json`: verified via `lark-cli skills read lark-vc references/lark-vc-notes.md`, returning `data.notes[].{minute_token, note_doc_token, verbatim_doc_token, creator_id, create_time}` or `data.artifacts.{summary, todos, chapters, keywords, transcript_file}` inside an ephemeral `tempdir`.
