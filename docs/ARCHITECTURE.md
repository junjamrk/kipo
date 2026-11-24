# Kipo (`kp`) — Master Architecture & Clean-Code Contract

Repository: <https://github.com/junjamrk/kipo> · License: [Apache-2.0](../LICENSE)

Target: Single Rust binary `kp` (package `kipo`) with 100% isomorphic Personal & Community libraries, platform-agnostic Public JSONL Ingest Contract ([`docs/INGEST-SCHEMA.md`](INGEST-SCHEMA.md)), local ONNX + multi-provider vector embeddings, dual-track derivation with provenance-semiring read-time access control & JTMS cascade invalidation, `127.0.0.1` read-only Web Inspector, decoupled external connectors (`kp-connect`), and sleep-time contrastive RSI (`kp dream`).

---

## 1. Core Architectural Principles

1. **Core Is a Platform-Agnostic Memory Database**:
   - Core crates (`kp-model`, `kp-store`, `kp-derive`, `kp-retrieve`, `kp-net`) never embed chat-platform, issue-tracker, or vendor-specific schemas. External scopes and principals map uniformly onto generic **containers** (`source_scopes`) and **external identities** (`external_identities`).
   - All external producers — Claude Code & Codex Hooks, `kp-connect` (`kp-connector`), the local Web Inspector, MCP `kp_note`, and user pipelines — write through the single **Public JSONL Ingest Contract** (`kp ingest`, MCP, and `kp daemon` UDS; see [`INGEST-SCHEMA.md`](INGEST-SCHEMA.md)).
   - `kp-connect` has **zero dependency on `kp-store` or `rusqlite`**; connectors persist their own incremental cursors in connector-owned state files and emit standard Public Ingest JSONL streams.
2. **Uniform Data Structures & Single-File SQLite Storage**:
   - Personal Library and Community Library run the **exact same single-file WAL SQLite schema (`kp.sqlite`) and Rust engine** (a Personal Library is a 1-member Community). Raw originals live inside content-addressed `raw_blobs` in SQLite (no auxiliary `.git` repository required).
   - Single-library summaries, cross-library collisions/syntheses, open-concept `Alias` alignments, and RSI `PlaybookBullet` / `SkillVersion` items are ALL represented as **`Derived` facts backed by the unified `justifications` table**.
   - Audience (`aud`) and validity intervals (`[valid_from, valid_until)`) are **never materialized onto derived conclusions**; they are evaluated at read time over the provenance semiring so container membership changes, clock expiry, and premise withdrawal take effect instantaneously with zero stale copies.
3. **Zero-Config Single Generative Model + Local-First Embeddings**:
   - Generative extraction/alignment auto-detects available host CLIs (`claude`, `codex`) or configured HTTP gateways (`KP_LLM_*`, `OPENAI_*`, `ANTHROPIC_*`), and degrades cleanly to `RawOnly` archival + lexical/vector search when no generative model is configured. Fail-closed gateway validation blocks malformed enterprise URLs from falling through to public endpoints.
   - Vector search defaults to local ONNX embeddings (`local:bge-small-zh-v1.5` via `fastembed` + `ort`, SHA-256 verified in `~/.kp/models/` with automatic BM25 FTS5 fallback when offline) alongside a pure-data multi-provider Recipe registry (`dashscope`, `volcengine`, `zhipu`, `openai`, `voyage`, `google`, `openrouter`, `ollama`) protected by library-level model & dimension guards.
4. **Clean Rust Discipline**:
   - Acyclic 8-crate graph (`rusqlite` confined strictly to `kp-store`; `kp-connect` optional and decoupled from `kp-store`):
     `kp-model` → `kp-store` → `{kp-retrieve, kp-derive}` → `kp-ingest` → `kp-net` → `kipo` (`crates/kp-cli`), plus standalone `kp-model` → `kp-connect`.
   - `unsafe_code = "forbid"`, `clippy::unwrap_used = "deny"`, `clippy::expect_used = "deny"` across the entire workspace.
   - Permissively adapted open-source designs (`Graphiti`, `Mem0`, `claude-mem`, `Onyx`, `LightRAG`, `RAGFlow`, `WeKnora`, `lark-cli`, `GBrain`, `SAG-Wiki`, `lemmalog`, `harness2`, `omnigraph-policy`) are implemented in 100% safe Rust and attributed in [`THIRD_PARTY_NOTICES.md`](../THIRD_PARTY_NOTICES.md).

---

## 2. Workspace Layout (8 Crates)

```text
kipo/
├── Cargo.toml                  # Workspace manifest (`version = "0.1.0"`, `license = "Apache-2.0"`)
├── LICENSE                     # Apache License, Version 2.0
├── THIRD_PARTY_NOTICES.md      # Open-source attributions & license notices
├── scripts/
│   └── install.sh              # POSIX installer with target detection & SHA256SUMS verification
├── docs/
│   ├── ARCHITECTURE.md         # Master architecture & algorithmic invariants
│   └── INGEST-SCHEMA.md        # Public JSONL Ingest Contract (Draft 2020-12 JSON Schema)
└── crates/
    ├── kp-model/               # Domain newtypes, Provenance Semiring, PublicIngestLine, SecretRedactor
    ├── kp-store/               # Single-file WAL SQLite (`kp.sqlite`), `raw_blobs`, FTS5 (`jieba-rs`), ONNX/HTTP Embedders, Markdown Export/Apply
    ├── kp-derive/              # Batched L0->L1 extraction, global LLM auto-detection, Datalog meta-rules, JTMS invalidation, `why()`
    ├── kp-retrieve/            # Filter-before-rank hybrid search (BM25 + Vec + RRF + DPP), `<=250`-token prompt budget, `PreToolUse` guard, Judges
    ├── kp-ingest/              # Non-blocking Claude/Codex Hooks, `tree-sitter` code anchors, Office/PDF/HTML/Web router, real-session `kp dream` miner
    ├── kp-net/                 # Private `0700` UDS `kp daemon`, `127.0.0.1` 4-column Web Inspector, Community Axum Server (REST + MCP + A2A)
    ├── kp-connect/             # Decoupled external connectors (`kp-connector` binary: Feishu `lark-cli`, GitHub API, session backfills -> JSONL)
    └── kp-cli/                 # Package `kipo` building binary `kp` (CLI, `kp demo`, `kp inspect`, `kp mcp`, `kp daemon`, `kp serve`)
```

---

## 3. Unified Single-File SQLite Schema (`kp.sqlite`)

All timestamps are UTC milliseconds (`i64`). Open-ended `valid_until` uses `i64::MAX`.

1. **`library_meta`**: `library_id TEXT PK`, `name TEXT`, `mode TEXT ('personal'|'community')`, `owner_actor TEXT`, `schema_ir_hash TEXT`, `embedding_model TEXT`, `embedding_dims INTEGER`, `created_at INTEGER`.
2. **`members_tokens`**: `actor_id TEXT PK`, `display_name TEXT`, `role TEXT ('owner'|'maintainer'|'member')`, `token_sha256 TEXT UNIQUE`, `created_at INTEGER`, `revoked_at INTEGER`.
3. **`tombstones`** *(checked BEFORE any read, ingest, or sync import)*:
   - `tombstone_id TEXT PK` (opaque HMAC-SHA256 of `(library_id, logical_key)` so purged plaintext is unrecoverable), `origin_library_id TEXT`, `purged_at INTEGER`, `reason TEXT`.
4. **`source_scopes` & `external_identities`** *(generic container ACL ceilings & identity resolution)*:
   - `source_scopes`: `scope_id TEXT PK`, `provider TEXT`, `external_container_id TEXT`, `label TEXT`, `current_audience_json TEXT`, `synced_at INTEGER` (monotonic `synced_at` anti-rollback).
   - `external_identities`: `(provider, external_id) PK`, `display_name TEXT`, `proposed_actor_id TEXT`, `confirmed_actor_id TEXT`, `status TEXT ('proposed'|'confirmed'|'rejected')`, `updated_at INTEGER`.
5. **`raw_sources` & `raw_blobs`** *(L0 immutable originals inside SQLite)*:
   - `raw_sources`: `source_id TEXT PK`, `uri TEXT`, `media_type TEXT`, `content_sha256 TEXT NOT NULL`, `byte_len INTEGER`, `git_blob_or_commit TEXT`, `visibility_scope TEXT`, `captured_at INTEGER`, `self_echo_filtered INTEGER`, `source_scope_id TEXT`, `external_source_key TEXT`, `superseded_by_source TEXT`, `ingested_by_actor TEXT`.
   - `raw_blobs`: content-addressed UTF-8/binary storage redacted by `SecretRedactor` prior to persistence and zeroed on `purge` (`PRAGMA secure_delete = ON`).
6. **`chunks`, `chunks_fts`, `chunk_vectors` & `fact_vectors`**:
   - `chunks`: anchored segments (`page`, `start_line`, `end_line`, `start_ms`, `end_ms`, `symbol`, `sig_hash`, `body_hash`) with `readable_by_json` (dynamic container membership resolved via `raw_sources.source_scope_id`).
   - `chunks_fts`: `VIRTUAL TABLE USING fts5(title, heading, segmented_body, content='', contentless_delete=1)` tokenized via shared `OnceLock<Jieba>`.
   - `chunk_vectors` / `fact_vectors`: normalized `f32` little-endian vectors tagged with `model_id` and `dims` (skipped automatically for `RawOnly` sources when the active embedder is cloud-hosted).
7. **`facts` & `fact_entities`** *(L1 base claims, remote mirrors, derived conclusions & RSI skills)*:
   - `kind`: `'observation'|'fact_slot'|'event'|'constraint'|'decision'|'membership'|'closed_negative'|'alias'|'derived_fact'|'collision_warning'|'conflict'|'synthesis'|'playbook_bullet'|'skill_version'`.
   - `audience_json`: explicit `ActorId` set (intersected with live `source_scope_id` membership when container-scoped) for base facts; `"__semiring__"` for derived facts.
   - `status`: `'active'|'superseded'|'withdrawn'|'invalidated'|'stale_premise'|'quarantined'|'unreachable_hidden'`.
8. **`justifications` & `justification_premises`** *(Unified Truth-Maintenance & Semiring Provenance)*:
   - Tracks `hard_rule`, `scoped_synthesis`, `assumed_premise`, and `rsi_attribution` derivations with O(1) reverse premise indexes for instant cascade invalidation and `kp why` proof rendering.
9. **`extraction_queue`, `session_cursors`, `review_items` & `rejected_claims`**:
   - Lease-based background extraction queue with exponential backoff, per-session byte-offset cursors, low-noise `review_items` (`ambiguous_scope`, `concept_alignment`, `identity_mapping`, `writeback_proposal`), and dual-key sticky claim rejections (plus single-shot Codex `kp allow` grants recorded via `change_log(op='allow_guard_once')`).
10. **`projection_pages`, `change_log` & `outbox`**:
    - `projection_pages`: tracks `base_seq` and content hashes per `(viewer_actor, page_slug)` for lossless `kp export` / `kp apply` roundtrips.
    - `change_log` & `outbox`: monotonic CDC sequence log (`kp log`) and durable idempotent community sync queue (`publish`, `withdraw`, `purge`).
11. **`code_anchors`, `rsi_episodes` & `rsi_feedback`**:
    - `code_anchors`: `tree-sitter` symbol signature/body hashes for Rust, Python, JS/TS, and Go (`kp verify`).
    - `rsi_episodes` & `rsi_feedback`: redacted contrastive episodes, L0 `kp.rsi.evidence` anchors, and task-deduplicated `helped` / `hurt` counters (`kp dream`).

---

## 4. Key Algorithmic & Security Invariants

1. **Read-Time Provenance Semiring (`kp-model` & `kp-derive`)**:
   - Base fact `b` is visible & valid to `(viewer v, time t)` iff `b.status == 'active'`, `b.expired_at.is_none()`, `t >= b.valid_from && t < b.valid_until`, `v ∈ b.audience`, and (when `b.source_scope_id` is set) `v` is a current member of that `source_scope`.
   - Derived item `k` is visible & valid to `(viewer v, time t)` iff there exists at least one active justification `d` of `k` where **every** premise `p` of `d` is visible & valid to `(v, t)` (evaluated to least fixpoint over the finite distributive lattice `(2^U, ∪, ∩)`).
   - **Joint `(v, t)` check per justification** ensures `{alice,bob}@Sat` + `{alice,bob,carol}@Fri` never leaks to `carol` on Saturday.
2. **Non-Interference Property**:
   - Adding, editing, withdrawing, or expiring any fact `p` invisible to viewer `c` produces **byte-identical** search results, Web Inspector payloads, `why` explanations, and `/v1/changes` snapshots for `c`.
3. **Pre-Persistence & Pre-Egress Privacy Boundary (`SecretRedactor` + `RawOnly` Gate)**:
   - `SecretRedactor` scrubs API keys, cloud credentials, bearer tokens, and webhook secrets before SQLite persistence and before any external HTTP request (`kp-model/src/redactor.rs` explicit security-pattern exemption).
   - `ModelRoutePolicy::RawOnly` strictly forbids cloud embedding and external generative/dream LLM calls (`0` outbound HTTP requests).
   - Session mining (`kp dream --sessions`) scrubs macOS/Linux/Windows personal home-directory prefixes (`~`) and grounds every promoted rule in a verified verbatim L0 quote.
4. **Local Loopback & UDS Hardening (`kp-net`)**:
   - `kp daemon` binds its Unix Domain Socket inside a `0700` user-private runtime directory (`0600` socket) and verifies peer UID credentials (`rustix` / `peer_cred()`).
   - The read-only Web Inspector (`kp inspect`) binds strictly to `127.0.0.1`, issues per-viewer 256-bit (64-hex) CSPRNG capability tokens exchanged into `HttpOnly; SameSite=Strict` `kp_inspector_session` cookies, enforces strict `Origin` and CSP (`default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'` — zero `'unsafe-inline'`), renders DOM strictly via `createElement` + `textContent`, and exposes zero mutation endpoints.
5. **Tombstone-First Zero-Residue Purge (`kp purge`)**:
   - Writes an opaque HMAC tombstone first, deletes matching rows across `facts`, `chunks`, `chunks_fts`, `chunk_vectors`, `fact_vectors`, `justifications`, `rsi_episodes`, `raw_sources`, and `raw_blobs` under `secure_delete = ON`, rebuilds affected skill projections, and enqueues an outbox `purge` tombstone so backups or delayed sync peers can never resurrect erased data.
