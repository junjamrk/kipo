# Session Transcript Fixture Provenance

All sanitized `.jsonl` session fixtures in this directory match real local agent transcript schemas (zero private user prose):

1. `claude_code_session.jsonl`
   - Upstream format: Claude Code `2.1.x` `~/.claude/projects/<project-slug>/<session-id>.jsonl`.
   - Verified structural elements:
     - `"isMeta": true` local-command caveat record (must be skipped, never attributed as human speech).
     - Slash-command wrapper tags `<command-message>`, `<command-name>`, `<command-args>` (must be stripped).
     - Harness-injected `<system-reminder>...</system-reminder>` and `<kp-context>...</kp-context>` prepended to user prompts (stripped before attribution).
     - Assistant `content` block array mixing `thinking`, `text` (with `[kp-ref:...]` stamp & secret token), and `tool_use`, followed by user-role `tool_result` (ignored).

2. `codex_rollout.jsonl`
   - Upstream format: OpenAI Codex CLI `0.164.x` `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (`github.com/openai/codex` `codex-rs/protocol` & `codex-rs/core/src/context/`).
   - Verified structural elements:
     - `type: "session_meta"` (`payload.id`).
     - `type: "response_item"` with `payload.type == "message"` (`role: "developer"` skipped; synthetic `role: "user"` blocks containing `# AGENTS.md instructions for ...` + `<INSTRUCTIONS>...</INSTRUCTIONS>`, `<environment_context>...</environment_context>`, and `<guardian_tool_descriptions>...</guardian_tool_descriptions>` stripped completely).
     - Duplicate `event_msg` (`payload.type == "user_message"`) + `response_item` (`payload.type == "message"`, `role == "user"`) deduplication.
     - `type: "response_item"` with `payload.type == "agent_message"` (`payload.message`) assistant reply capture.

3. `antigravity_transcript.jsonl`
   - Upstream format: Google Antigravity `~/.gemini/antigravity/brain/<conversation-id>/.system_generated/logs/transcript.jsonl` (inspected by schema keys/tags only).
   - Verified structural elements:
     - `source: "SYSTEM", type: "SYSTEM_MESSAGE"` (ignored).
     - `source: "USER_EXPLICIT", type: "USER_INPUT"` with `<CONTEXT_SUMMARY>...</CONTEXT_SUMMARY>` and `<SYSTEM_MESSAGE>...</SYSTEM_MESSAGE>` stripped and `<USER_REQUEST>...</USER_REQUEST>` unwrapped.
     - `source: "MODEL", type: "PLANNER_RESPONSE"` tool-call-only step (`content` absent -> ignored) vs visible assistant prose (`content` string -> captured).
     - `source: "MODEL", type: "GENERIC"` tool execution output step (ignored).
