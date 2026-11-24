# Public JSONL Ingest Contract (`kp ingest` / MCP / UDS)

Status: **Normative Public Wire Specification (Draft 2020-12 JSON Schema)**
Wire implementation: `crates/kp-model/src/ingest.rs` (`PublicIngestLine`, `PublicContainerRoster`, `PublicIngestRecord`)

---

## 1. Architectural Boundary & Three Peer Entrypoints

### 1.1 Core Is a Platform-Agnostic Memory Engine

Core (`kp-model`, `kp-store`, `kp-derive`, `kp-retrieve`, `kp-net`) acts strictly as a database-grade memory substrate: callers supply conversations, documents, messages, or operational events in a single public format, and Core handles **verbatim L0 archival, pre-persistence secret redaction, batched L1 extraction, semiring provenance derivation, viewer-isolated retrieval, and reversible projection**.

- **Zero platform coupling in Core**: Core knows only generic **containers** (`source_scopes`) and **external identities** (`external_identities`). It never embeds chat-app, issue-tracker, or vendor-specific schemas.
- **Equal external callers**: Claude Code / Codex Hooks, decoupled connectors (`kp-connect`), the local read-only Web Inspector, and any user-written script or pipeline all write through the exact same public contract.

### 1.2 Three Peer Write Entrypoints

| Entrypoint | Transport & Invocation | Primary Use Case |
|---|---|---|
| **CLI `kp ingest [<file>\|-]`** | Streams newline-delimited UTF-8 JSON (`JSONL`) from a file path or `stdin` (`-`) | Shell pipelines, decoupled connectors (`kp-connect`), historical backfills, and CI/cron scripts |
| **MCP `kp_note`** | JSON-RPC `2.0` `tools/call` (`name: "kp_note"`) over `kp mcp` stdio | Zero-confirmation interactive agent sessions saving explicit user notes or corrections with verbatim provenance |
| **`kp daemon` UDS `ingest_jsonl`** | Newline-delimited JSON RPC (`{"op": "ingest_jsonl", ...}`) over the user-private `0700` Unix Domain Socket (`0600` socket + bidirectional `peer_cred()` UID check) | Low-latency background hooks, long-lived connector daemons, and multi-megabyte streaming writes (`8 MiB` max frame size with automatic client-side line batching via `DaemonClient::ingest_jsonl` / `DaemonSyncClient::ingest_jsonl`) |

---

## 2. JSONL Stream Format, Strict Field Allowlists & Line Discrimination

A public ingest stream consists of UTF-8 lines separated by `\n` (or `\r\n`). Blank or whitespace-only lines are ignored. Every non-empty line MUST be a single JSON object matching `PublicIngestLine`:

1. **`PublicContainerRoster`** — recognized when the JSON object contains top-level `"container_id"` and does **not** contain `"source"`. Updates live container membership (`source_scopes`) and proposes any unresolved `namespace:id` external identities (`external_identities(status = 'proposed')`). Emit rosters **ahead of** message records so container access ceilings are current before records are evaluated.
2. **`PublicIngestRecord`** — recognized for all objects carrying `"source"`. Represents an idempotent `upsert`, an atomic edit/revision `supersede`, or a recall/deletion `withdraw`.

### 2.1 Closed Schema & Unknown-Field Rejection

To prevent compliance or routing flags from silently failing open on a typo, **every JSON object in the wire contract enforces `additionalProperties: false`**:

- Unknown or misspelled keys on `PublicContainerRoster`, `PublicIngestRecord`, `PublicIngestSource`, `PublicSpeaker` (object form), `PublicAudience` (object form), or `ModelRoutePolicy` — for example `"model_polciy"` instead of `"model_policy"` — are **rejected immediately at parse time** rather than ignored.
- `"previous_external_id"` is valid **only** when `"op": "supersede"`. Supplying `"previous_external_id"` on an `"upsert"` or `"withdraw"` record is rejected at parse time.
- `"model_policy"` may be omitted or set to `null` (`"model_policy": null`); both forms deserialize identically to `None` (inherit the library's global generative model setting).

---

## 3. JSON Schema (Draft 2020-12)

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://kipo.dev/schemas/public-ingest-line.v1.schema.json",
  "title": "PublicIngestLine",
  "description": "One newline-delimited JSON object in a Kipo (`kp`) public ingest stream.",
  "oneOf": [
    { "$ref": "#/$defs/PublicContainerRoster" },
    { "$ref": "#/$defs/PublicIngestRecord" }
  ],
  "$defs": {
    "NonEmptyCleanString": {
      "type": "string",
      "minLength": 1,
      "pattern": "^[^\\u0000-\\u001F\\u007F]*\\S[^\\u0000-\\u001F\\u007F]*$"
    },
    "ContainerId": {
      "type": "string",
      "minLength": 1,
      "description": "ASCII identifier for `source_scopes` (`A-Z`, `a-z`, `0-9`, `_`, `.`, `-`), e.g. `scope.ops_cutover`.",
      "pattern": "^[A-Za-z0-9_.-]+$"
    },
    "ActorId": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "pattern": "^[A-Za-z0-9][A-Za-z0-9_.-]*$"
    },
    "ActorOrExternalId": {
      "type": "string",
      "minLength": 1,
      "description": "Either a library `actor_id` (`eng_alice`) or an external `namespace:id` (`ext_chat:user_42`).",
      "pattern": "^([A-Za-z0-9][A-Za-z0-9_.-]*|[A-Za-z0-9_.-]+:[^\\u0000-\\u001F\\u007F\\s].*)$"
    },
    "Timestamp": {
      "description": "Unix epoch milliseconds (10-digit Unix seconds are auto-normalized to milliseconds) or an ISO-8601 / RFC 3339 timestamp string.",
      "oneOf": [
        { "type": "integer" },
        {
          "type": "string",
          "minLength": 10,
          "examples": [
            "2026-11-21T15:16:20.250+08:00",
            "2026-11-21T07:16:20Z"
          ]
        }
      ]
    },
    "PublicContainerRoster": {
      "type": "object",
      "required": [
        "container_id",
        "label",
        "members",
        "all_library_members",
        "synced_at"
      ],
      "not": {
        "required": ["source"]
      },
      "properties": {
        "container_id": { "$ref": "#/$defs/ContainerId" },
        "label": { "$ref": "#/$defs/NonEmptyCleanString" },
        "members": {
          "type": "array",
          "items": { "$ref": "#/$defs/ActorOrExternalId" }
        },
        "all_library_members": { "type": "boolean" },
        "synced_at": { "$ref": "#/$defs/Timestamp" },
        "provider": { "$ref": "#/$defs/NonEmptyCleanString" },
        "upstream_container_id": { "$ref": "#/$defs/NonEmptyCleanString" }
      },
      "additionalProperties": false
    },
    "PublicIngestSource": {
      "type": "object",
      "required": ["kind", "uri", "external_id"],
      "properties": {
        "kind": {
          "$ref": "#/$defs/NonEmptyCleanString",
          "examples": ["chat", "repo", "session", "doc", "manual"]
        },
        "uri": {
          "$ref": "#/$defs/NonEmptyCleanString",
          "examples": ["chat://room_cutover/msg_1001", "stdin://note/1"]
        },
        "external_id": { "$ref": "#/$defs/NonEmptyCleanString" },
        "container_id": { "$ref": "#/$defs/ContainerId" }
      },
      "additionalProperties": false
    },
    "PublicSpeaker": {
      "description": "Accepts either a plain identifier string or an object carrying an optional human display name.",
      "oneOf": [
        { "$ref": "#/$defs/ActorOrExternalId" },
        {
          "type": "object",
          "required": ["id"],
          "properties": {
            "id": { "$ref": "#/$defs/ActorOrExternalId" },
            "display_name": { "$ref": "#/$defs/NonEmptyCleanString" }
          },
          "additionalProperties": false
        }
      ]
    },
    "PublicAudience": {
      "description": "Follows live `source.container_id` membership (`\"container_members\"`) or lists explicit library `actor_id`s.",
      "oneOf": [
        {
          "type": "string",
          "const": "container_members"
        },
        {
          "type": "object",
          "required": ["explicit"],
          "properties": {
            "explicit": {
              "type": "array",
              "minItems": 1,
              "items": { "$ref": "#/$defs/ActorId" }
            }
          },
          "additionalProperties": false
        }
      ]
    },
    "ModelRoutePolicy": {
      "description": "Optional advanced routing override. Omit or pass `null` to inherit the library's global generative model.",
      "oneOf": [
        {
          "type": "object",
          "required": ["mode"],
          "properties": {
            "mode": { "type": "string", "const": "host_cli" }
          },
          "additionalProperties": false
        },
        {
          "type": "object",
          "required": ["mode", "endpoint"],
          "properties": {
            "mode": { "type": "string", "const": "named_endpoint" },
            "endpoint": { "$ref": "#/$defs/NonEmptyCleanString" }
          },
          "additionalProperties": false
        },
        {
          "type": "object",
          "required": ["mode"],
          "properties": {
            "mode": { "type": "string", "const": "raw_only" }
          },
          "additionalProperties": false
        }
      ]
    },
    "PublicIngestRecord": {
      "type": "object",
      "required": ["source", "speaker", "audience", "occurred_at", "op"],
      "properties": {
        "source": { "$ref": "#/$defs/PublicIngestSource" },
        "speaker": { "$ref": "#/$defs/PublicSpeaker" },
        "audience": { "$ref": "#/$defs/PublicAudience" },
        "occurred_at": { "$ref": "#/$defs/Timestamp" },
        "content": { "type": ["string", "null"], "default": "" },
        "op": {
          "type": "string",
          "enum": ["upsert", "supersede", "withdraw"]
        },
        "previous_external_id": { "$ref": "#/$defs/NonEmptyCleanString" },
        "ingested_by": { "$ref": "#/$defs/ActorId" },
        "media_type": { "$ref": "#/$defs/NonEmptyCleanString" },
        "model_policy": {
          "oneOf": [
            { "type": "null" },
            { "$ref": "#/$defs/ModelRoutePolicy" }
          ]
        }
      },
      "allOf": [
        {
          "if": {
            "properties": { "op": { "const": "upsert" } },
            "required": ["op"]
          },
          "then": {
            "required": ["content"],
            "properties": {
              "content": { "type": "string", "minLength": 1, "pattern": "\\S" }
            },
            "not": {
              "required": ["previous_external_id"]
            }
          }
        },
        {
          "if": {
            "properties": { "op": { "const": "supersede" } },
            "required": ["op"]
          },
          "then": {
            "required": ["content", "previous_external_id"],
            "properties": {
              "content": { "type": "string", "minLength": 1, "pattern": "\\S" }
            }
          }
        },
        {
          "if": {
            "properties": { "op": { "const": "withdraw" } },
            "required": ["op"]
          },
          "then": {
            "properties": {
              "content": { "type": ["string", "null"] }
            },
            "not": {
              "required": ["previous_external_id"]
            }
          }
        }
      ],
      "additionalProperties": false
    }
  }
}
```

---

## 4. Field Semantics, Security & Persistence Invariants

### 4.1 Pre-Persistence `SecretRedactor` Scrubbing

Before computing `content_sha256` or writing any data to SQLite (`raw_blobs`, `raw_sources`, `chunks`, `chunks_fts`, `source_scopes`, `external_identities`), Core scrubs all user-supplied text fields through `SecretRedactor`:

- **`PublicContainerRoster.label`**, **`PublicSpeaker.display_name`**, and **`PublicIngestRecord.content`** are redacted in memory prior to hashing, chunking, FTS5 indexing, embedding, L1 extraction, or Markdown projection (`kp export`).
- Plaintext API keys, bearer tokens, private keys, and recognized credential prefixes never touch disk or outbound model/embedding calls.

### 4.2 Caller Role Authorization & Source Ownership

Every write entrypoint resolves an authenticated `caller: ActorId` who MUST be an active library member (`Owner`, `Maintainer`, or `Member`):

- **Container roster administration (`PublicContainerRoster`)**: Only `Owner` or `Maintainer` callers may create or update `source_scopes` rosters; unprivileged `Member` callers receive `PermissionDenied`.
- **`ingested_by` delegation**: When `ingested_by` is omitted, Core binds attribution to `caller`. Setting `ingested_by != caller` is restricted to `Owner` or `Maintainer` callers (and `ingested_by` must be an active library member); unprivileged `Member` callers cannot spoof another ingester.
- **Cross-speaker attribution (`resolved_speaker != caller`)**: When an unprivileged `Member` submits a record whose `speaker` resolves to another actor or external identity, `source.container_id` MUST already exist in `source_scopes` with `caller` in its active membership, and the effective `readable_by` audience MUST include `caller` (or `*`). Unprivileged members cannot fabricate statements for others outside a shared container.
- **`supersede` & `withdraw` ownership**: Mutating or recalling an existing `raw_sources` row (`withdraw` on `(source.kind, source.external_id)` or `supersede` on `(source.kind, previous_external_id)`) requires `caller` to be `Owner` or `Maintainer`, OR for the target source to be owned by `caller` (`ingested_by_actor == caller`, or same active container with original author `== caller`). Unprivileged members cannot withdraw or overwrite another member's sources.

### 4.3 `PublicContainerRoster` & Monotonic `synced_at` Anti-Rollback

- **`container_id`**: Stable container scope identifier (`source_scope_id`, e.g. `"scope.ops_cutover"`), matching `^[A-Za-z0-9_.-]+$`.
- **`label`**: Human-readable container title (scrubbed via `SecretRedactor` before storage).
- **`members`**: List of library `actor_id`s (e.g. `"eng_alice"`) or external `namespace:id` strings (e.g. `"ext_chat:user_pm_li"`). Confirmed `external_identities` resolve to their mapped library `actor_id`; unknown external identities are recorded as `status = 'proposed'` without granting read access to unmapped outsiders.
- **`all_library_members`**: When `true`, sets the container ceiling to `AudienceSpec::AllMembers` (`*` intersected with active library members); when `false`, sets `AudienceSpec::Explicit(resolved_members)`.
- **`synced_at` strict monotonicity**: Roster snapshot timestamp (epoch milliseconds, 10-digit Unix seconds, or ISO-8601 string). When `source_scopes` already holds a row for `container_id` at `existing_synced_at`:
  - **`synced_at < existing_synced_at`**: Rejected immediately with `Conflict`. Stale or replayed older roster snapshots can never roll back a member removal or re-widen a narrowed container ceiling.
  - **`synced_at == existing_synced_at`**: Idempotent no-op **only** when the normalized resolved audience and redacted `label` match the existing row; any membership or label divergence at the same timestamp returns `Conflict`.
  - **`synced_at > existing_synced_at`**: Atomically updates `current_audience_json`, `label`, and `synced_at`, and reconciles dependent dynamic container audiences.
- **`provider` / `upstream_container_id`** *(optional)*: Opaque connector metadata recorded on `source_scopes` for diagnostics and audit.

### 4.4 `PublicIngestRecord`

- **`source: { kind, uri, external_id, container_id? }`**:
  - Core derives the deterministic logical source key `"{source.kind}:{source.external_id}"` for idempotent deduplication and targeted updates/withdrawals (shared across CLI/MCP/UDS ingest and hook session sub-batches).
  - All provided fields must be non-empty and contain no control characters, with zero unknown keys.
  - `container_id` binds the source and any extracted facts to the live `source_scopes` ceiling of that container.
- **`speaker`** *(string or object dual form)*:
  - **String form**: `"eng_alice"` (library `actor_id`) or `"ext_chat:user_pm_li"` (`namespace:id`, where `namespace` matches `[A-Za-z0-9_.-]+`).
  - **Object form**: `{"id": "ext_chat:user_pm_li", "display_name": "李四"}` (`additionalProperties: false`; whitespace around `display_name` is trimmed and redacted before storage; canonical serialization always emits the object form, omitting `display_name` when absent).
- **`audience`**:
  - `"container_members"`: Dynamically follows `container_current_members ∩ library_members` at query and semiring derivation time. **Requires** `source.container_id` to be present; if a member later leaves the container and a newer `PublicContainerRoster` narrows `members`, visibility of historical facts and derived warnings narrows immediately.
  - `{"explicit": ["eng_alice", "eng_bob"]}`: Explicit non-empty allowlist of library `actor_id`s (still bounded by the live `source.container_id` ceiling whenever `container_id` is set).
- **`occurred_at`** *(numeric or ISO-8601 dual form)*:
  - Accepts integer Unix epoch milliseconds (`1795245390000`), 10-digit Unix epoch seconds (`1795245390`, normalized to `1795245390000`), or ISO-8601 / RFC 3339 strings with timezone offset (`"2026-11-21T15:16:20.250+08:00"`, `"2026-11-21T07:16:20Z"`).
  - Anchors relative calendar expressions (such as `"next Thursday"` or `"下周四"`) during L1 extraction.
- **`op` (`upsert` | `supersede` | `withdraw`)**:
  - `"upsert"`: Idempotently stores the redacted L0 raw source and enqueues L1 extraction according to the effective model policy. Requires non-null, non-blank `content` (`minLength: 1`, `pattern: "\\S"`) and forbids `previous_external_id`. Re-submitting an `upsert` with identical `(source.kind, source.external_id)` and redacted `content_sha256` is idempotent when `model_policy` matches; narrowing an existing source to `{"mode": "raw_only"}` atomically marks it `raw_only`, cancels unexecuted extraction jobs, and clears cloud vectors, whereas conflicting non-null `model_policy` changes on the same `external_id` without `supersede` return `Conflict`.
  - `"supersede"`: Atomically replaces the prior record identified by `(source.kind, previous_external_id)`. Requires non-null, non-blank `content` (`minLength: 1`, `pattern: "\\S"`), non-empty `previous_external_id`, and `previous_external_id != source.external_id`. Marks prior extracted facts superseded and queues compensating community `withdraw` events if previously published.
  - `"withdraw"`: Recalls `(source.kind, source.external_id)` (forbids `previous_external_id`; `content` may be omitted, empty `""`, or `null`). Withdraws active facts derived from that source, cancels any unexecuted extraction job, and queues community `withdraw` events without creating a permanent purge tombstone (the claim remains re-learnable if re-stated later).
- **`ingested_by` & `media_type`** *(optional)*:
  - `ingested_by`: Library `actor_id` performing the import (defaults to `caller`; delegating `ingested_by != caller` requires `Owner` or `Maintainer`).
  - `media_type`: MIME hint such as `"text/plain"` or `"text/markdown"` (defaults to `"text/plain"`).
- **`model_policy` Inheritance Rules** *(optional / nullable)*:
  - **Omitted or `null` (default & recommended)**: Inherits the library's single global generative model setting (`HostCli` -> configured API Key / gateway `OPENAI_BASE_URL` / `ANTHROPIC_BASE_URL` -> automatic `RawOnly` local FTS5 fallback when no generative credentials exist or an explicit gateway URL is invalid).
  - **`{"mode": "raw_only"}`**: Strict local-only compliance mode. Stores redacted raw text and indexes local FTS5 / local ONNX embeddings only; **guarantees zero external generative LLM calls and zero cloud embedding API egress**.
  - **`{"mode": "host_cli"}` / `{"mode": "named_endpoint", "endpoint": "<id>"}`**: Advanced per-stream routing overrides (an unknown or unconfigured `named_endpoint` fails closed rather than falling back to a default cloud provider).

---

## 5. End-to-End Copy-Pasteable Examples

### 5.1 CLI Pipeline (`kp ingest -`): Roster Sync → Upsert → Edit Supersede → RawOnly & Withdraw

```bash
cat <<'EOF' | kp --path ./team-library ingest -
{"container_id":"scope.ops_cutover","label":"运维切换协作组","members":["eng_alice","eng_bob","ext_chat:pm_li"],"all_library_members":false,"synced_at":"2026-11-21T07:00:00Z","provider":"ext_chat","upstream_container_id":"room_cutover_01"}
{"source":{"kind":"chat","uri":"chat://room_cutover_01/msg_1001","external_id":"msg_1001","container_id":"scope.ops_cutover"},"speaker":{"id":"ext_chat:pm_li","display_name":"李四"},"audience":"container_members","occurred_at":"2026-11-21T15:05:00+08:00","content":"下周四凌晨两点进行数据库主从切换演练，窗口期暂停合并非回滚分支。","op":"upsert","ingested_by":"eng_alice","media_type":"text/plain","model_policy":null}
{"source":{"kind":"chat","uri":"chat://room_cutover_01/msg_1001_v2","external_id":"msg_1001_v2","container_id":"scope.ops_cutover"},"speaker":{"id":"ext_chat:pm_li","display_name":"李四"},"audience":"container_members","occurred_at":"2026-11-21T15:12:30+08:00","content":"更正：数据库主从切换演练推迟至下周五凌晨三点执行，期间暂停合并非回滚分支。","op":"supersede","previous_external_id":"msg_1001","ingested_by":"eng_alice"}
{"source":{"kind":"manual","uri":"stdin://scratch/temp_note_9","external_id":"temp_note_9"},"speaker":"eng_alice","audience":{"explicit":["eng_alice"]},"occurred_at":1795245600000,"content":"临时本地排查记录（仅存原文检索）","op":"upsert","model_policy":{"mode":"raw_only"}}
{"source":{"kind":"manual","uri":"stdin://scratch/temp_note_9","external_id":"temp_note_9"},"speaker":"eng_alice","audience":{"explicit":["eng_alice"]},"occurred_at":1795245660000,"op":"withdraw"}
EOF
```

### 5.2 MCP Tool Call (`kp_note` over `kp --path ./team-library mcp`)

Request frame (`stdin`):

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"kp_note","arguments":{"text":"存储结构变更必须通过迁移脚本执行，不要直接手改 schema.sql","verbatim_quote":"不要直接手改 schema.sql","is_correction":false}}}
```

Response frame (`stdout`):

```json
{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{\"fact_id\":\"fact_...\",\"source_id\":\"...\"}"}],"structuredContent":{"fact_id":"fact_...","source_id":"..."},"isError":false}}
```

### 5.3 `kp daemon` Local UDS `ingest_jsonl` RPC

Socket location: resolved automatically inside the user-private `0700` runtime directory (`$TMPDIR/kp-<uid>/` on macOS, `$XDG_RUNTIME_DIR/kp/` on Linux, fallback `~/.kp/run/`). Each request/response is a single UTF-8 JSON line terminated by `\n` (up to `8 MiB` per frame; `DaemonClient::ingest_jsonl` and `DaemonSyncClient::ingest_jsonl` automatically split larger multi-line streams across frames and merge the returned `IngestJsonlResult`).

Request frame:

```json
{"op":"ingest_jsonl","jsonl":"{\"container_id\":\"scope.ops_cutover\",\"label\":\"运维切换协作组\",\"members\":[\"eng_alice\",\"eng_bob\"],\"all_library_members\":false,\"synced_at\":1795244400000}\n{\"source\":{\"kind\":\"chat\",\"uri\":\"chat://room_cutover_01/msg_2001\",\"external_id\":\"msg_2001\",\"container_id\":\"scope.ops_cutover\"},\"speaker\":\"eng_alice\",\"audience\":\"container_members\",\"occurred_at\":1795244460000,\"content\":\"主从切换演练窗口暂停执行非回滚合并\",\"op\":\"upsert\"}","actor":"eng_alice","at":1795244460000,"extract_now":false}
```

Response frame:

```json
{"status":"ingested","rosters_applied":1,"records_upserted":1,"records_superseded":0,"records_withdrawn":0,"jobs_enqueued":1,"source_ids":["a1b2c3d4..."]}
```
