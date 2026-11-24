# Kipo (`kp`)

**Local-first personal & team memory infrastructure for AI coding agents — backed by a single SQLite file and provenance-semiring visibility isolation.**

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

[中文教程 (TUTORIAL.md)](TUTORIAL.md) · [Architecture](docs/ARCHITECTURE.md) · [Public Ingest Contract](docs/INGEST-SCHEMA.md) · [Third-Party Notices](THIRD_PARTY_NOTICES.md)

---

## Why Kipo?

Most agent memory tools either dump flat markdown notes into every prompt or ship raw chat logs to a shared cloud index where permissions, retractions, and outdated assumptions silently bleed across teammates.

**Kipo (`kp`)** treats agent memory as database-grade local infrastructure:

1. **Single-File SQLite Engine, Isomorphic Personal & Team Libraries**: Every Personal Library and Community Library runs the exact same Rust engine and single-file WAL SQLite schema (`kp.sqlite`: content-addressed `raw_blobs`, `jieba-rs` CJK/English FTS5 `contentless_delete=1`, normalized vector embeddings, bitemporal `facts`, and JTMS `justifications`).
2. **Read-Time Provenance Semiring & Zero-Leakage Isolation**: Audience (`aud`) and validity windows (`[valid_from, valid_until)`) are **never copied onto derived conclusions**. Collision warnings, scoped syntheses, concept alignments, and learned skills evaluate visibility at read time over the provenance semiring — when a teammate leaves a group, a constraint expires, or a premise is withdrawn (`kp forget` / `kp purge`), dependent conclusions vanish instantaneously with mathematically verified non-interference.
3. **Platform-Agnostic Core & Public JSONL Ingest Contract**: Core knows only generic containers (`source_scopes`) and external identities (`external_identities`). Claude Code & Codex Hooks, external connectors (`kp-connect`), the local Web Inspector, and custom shell pipelines are all equal callers of [`kp ingest`](docs/INGEST-SCHEMA.md).

---

## Install & 5-Minute Tour

### Homebrew (macOS & Linux)

```sh
brew install junjamrk/tap/kipo
```

### One-line binary install (verifies `SHA256SUMS` before installing)

```sh
# macOS & Linux (glibc >= 2.28)
curl -fsSL https://raw.githubusercontent.com/junjamrk/kipo/main/scripts/install.sh | sh

# Windows PowerShell
irm https://raw.githubusercontent.com/junjamrk/kipo/main/scripts/install.ps1 | iex
```

### Build from source with Cargo

```sh
# Core CLI + local ONNX embeddings (`kp` binary, package `kipo`)
cargo install --git https://github.com/junjamrk/kipo kipo

# Include decoupled external connectors (`kp connector` subcommand; add `kp-connect` for standalone `kp-connector`)
cargo install --git https://github.com/junjamrk/kipo kipo --features connectors
cargo install --git https://github.com/junjamrk/kipo kp-connect
```

### Run the 5-minute offline interactive tour

No API keys or network access required — exercises host client hook setup, Public Ingest + TMS `StalePremise` invalidation + `AmbiguousScope` review, `<kp-context>` recall, dual-client `PreToolUse` command guards, lossless Markdown `export -> edit -> apply`, and `carol` zero-leakage isolation in one command:

```sh
kp demo
# Keep the generated demo library on disk to explore afterward:
kp demo --dir ./kipo-demo-lib
```

---

## Core Capabilities

### 1. Non-Intrusive Claude Code & Codex Hooks + Dual-Client `PreToolUse`

Connect a project once; Kipo preserves existing user hooks and settings:

```sh
kp init --path ~/.kp/personal --owner alice --name "Alice Personal"
kp connect claude-code --project . --path ~/.kp/personal --as alice
kp connect codex --project . --path ~/.kp/personal --as alice
kp connect cursor --project . --path ~/.kp/personal --as alice
kp connect windsurf --global --path ~/.kp/personal --as alice
```

- **Zero-blocking capture (`< 35–80ms` P95)**: First hook auto-spawns `kp daemon` in the background (`0ms` wait). Transcripts are redacted (`SecretRedactor`) before persistence and extracted out-of-band in batches.
- **Bounded `UserPromptSubmit` injection**: Injects at most **`<= 250 tokens` and `<= 3 items`**, each anchored with verbatim speaker quotes (`↳`). Irrelevant prompts inject `{}`.
- **Dual-client `PreToolUse` command guards**:
  - **Claude Code**: Read-only shell evaluation (`shlex`) returns `permissionDecision = "ask"` with the verbatim proof chain when a command collides with an active team constraint.
  - **Codex**: Returns `permissionDecision = "deny"` and prints a one-shot `kp allow <kp-ref>` command so the developer can explicitly authorize a single execution.
- **Low-noise governance**: Ambiguous scopes default to `SelfOnly` and only enter `kp review` when cross-member impact exists. `kp reject <fact_id> --reason "..."` withdraws a claim and records a dual-key sticky tombstone so re-extracting the same transcript never resurrects it.

### 2. Public JSONL Ingest Contract (`kp ingest` / MCP / UDS)

Stream container rosters and `upsert` / `supersede` / `withdraw` records from files, `stdin`, `kp mcp` (`kp_note`), or `kp daemon` UDS (`ingest_jsonl`). Full Draft 2020-12 JSON Schema lives in [`docs/INGEST-SCHEMA.md`](docs/INGEST-SCHEMA.md):

```sh
cat <<'JSONL' | kp ingest - --path ~/.kp/personal --as alice --json
{"container_id":"scope.release","label":"Release Oncall","members":["alice","bob"],"all_library_members":false,"synced_at":1795089600000}
{"source":{"kind":"chat","uri":"chat://release/msg-101","external_id":"msg-101","container_id":"scope.release"},"speaker":"bob","audience":"container_members","occurred_at":"2026-11-22T10:00:00Z","content":"Database freeze starts Friday 18:00 UTC; pause schema migrations until Monday.","op":"upsert"}
JSONL
```

### 3. Local ONNX Embeddings by Default + Multi-Provider Recipes

`kp init` defaults to local ONNX embeddings (`local:bge-small-zh-v1.5`, SHA-256 verified under `~/.kp/models/`, gracefully falling back to `jieba-rs` BM25 FTS5 when offline) and ships built-in provider recipes (`dashscope`, `volcengine`, `zhipu`, `openai`, `voyage`, `google`, `openrouter`, `ollama`) with single-dimension library locking:

```sh
kp providers list
kp providers env dashscope
kp providers test --path ~/.kp/personal --json
kp embed --stale --to local:bge-small-zh-v1.5 --path ~/.kp/personal --json
```

### 4. `127.0.0.1` Read-Only Web Inspector (`kp inspect`)

`kp daemon` serves a capability-authenticated, loopback-only (`127.0.0.1`) four-column Web Inspector scoped to the active viewer:

```sh
kp inspect --path ~/.kp/personal --as alice
# Emit the authenticated 127.0.0.1 URL as structured JSON:
kp inspect --path ~/.kp/personal --as alice --json
```

The four columns show:
1. **What Kipo Remembers** — active facts, constraints, events, warnings, and promoted skills;
2. **Why It Remembers** — highlighted verbatim L0 quotes and semiring justification chains;
3. **What Changed** — chronological timeline of `supersede`, `withdraw`, `expire`, and `stale_premise` transitions;
4. **Pending Reviews** — queued `ambiguous_scope` and identity confirmations (`kp review`).

### 5. Lossless Markdown Export & Write-Back (`kp export` / `kp apply` / `kp log`)

Inspect or edit memory in plain Markdown files with optimistic `base_seq` concurrency protection:

```sh
kp export ./memory-md --path ~/.kp/personal --as alice --json
# Edit or remove entries in ./memory-md/index.md, then apply:
kp apply ./memory-md --path ~/.kp/personal --as alice --json
kp log --limit 20 --path ~/.kp/personal --json
```

### 6. Decoupled External Connectors (`kp connector`)

Built in `kp-connect` with **zero dependency on `kp-store`** — connectors store their own cursors under `~/.kp/connectors/` and write strictly through the public `kp ingest` JSONL contract:

- **Feishu / Lark (`lark-cli`)**: Incremental chat messages, recalls/edits, member roster changes (automatic audience contraction when a member leaves), wiki & drive docs;
- **GitHub REST API**: Repository collaborators, Issues, and Pull Requests (`since` incremental windows + ETag caching + rate-limit backoff);
- **Agent Session Backfill**: Claude Code (`~/.claude/projects/`), OpenAI Codex (`~/.codex/sessions/`), and transcript JSONL directories.

```sh
kp-connector --state ~/.kp/connectors/state.json scope add \
  --kind github --scope-id repo.kipo --upstream junjamrk/kipo --label "Kipo Repo"
kp-connector --state ~/.kp/connectors/state.json sync --kind github --ingested-by alice \
  | kp ingest - --path ~/.kp/personal --as alice --json
```

### 7. Multi-source sleep-time consolidation and skill compilation

`kp dream` consolidates coding sessions, team chat logs, Markdown, PDF, Office, and HTML documents, `http://` and `https://` web links, and timed video or audio transcripts in `.vtt`, `.srt`, `.lrc`, Whisper ASR `.json`, and `.mp4` or `.m4a` sidecar formats into governed declarative rules and `<library>/skills/<slug>/SKILL.md` playbooks:

```sh
kp add ./docs ./recordings/onboarding.mp4 --path ~/.kp/personal --as alice
kp add https://example.com/runbook.html --path ~/.kp/personal --as alice
kp dream --sessions ~/.codex/sessions ~/.claude/projects --path ~/.kp/personal --json
kp skill list --path ~/.kp/personal --json
```

- **Zero-config global model routing.** `kp dream` routes through logged-in host CLIs such as `claude` and `codex` or configured cloud API providers including `openai`, `anthropic`, `dashscope`, `volcengine`, `zhipu`, and `openrouter`. Under `KP_MODEL_POLICY=raw_only`, it runs deterministic local consolidation with zero network egress.
- **Non-blocking background trigger.** `session-end`, `kp add`, and `kp ingest` spawn a detached background `kp dream` subprocess once pending consolidation items reach the threshold.
- **Deterministic Zep safety scanner and modality anchors.** The scanner rejects literal personal paths, UUIDs, secrets, and hallucinated CLI flags absent from observed traces. Every distilled claim retains exact media timestamps in `start_ms` and `end_ms`, PDF page numbers, and verbatim L0 quotes.
- **`k-of-N` provenance truth maintenance.** Promotion requires at least 2 independent source or session verifications and 0 hurt outcomes. Withdrawing 1 of 3 independent sources keeps the skill active. Dropping below 2 surviving sources or recording `hurt` feedback retracts `SKILL.md` immediately. Imported community skills stay quarantined with `exec_approved: false` until the local owner runs `kp skill approve <slug>`.

### 8. Isomorphic Community Collaboration (`kp serve --community` / `kp sync`)

Run a shared Community Library with the same `kp` binary over HTTP REST, Streamable HTTP MCP (`/mcp`), and A2A (`/.well-known/agent.json`, `/a2a`):

```sh
kp init --community --owner admin --name "Eng Team" --path ./team-lib
kp member add alice --path ./team-lib --json
kp member add bob --path ./team-lib --json
kp serve --community --listen 127.0.0.1:8080 --path ./team-lib

# From Alice's machine: join once, then background daemon syncs shared claims automatically
kp community join http://127.0.0.1:8080 --token "$ALICE_TOKEN" --path ~/.kp/personal --json
```

---

## Workspace Architecture (8 Crates)

```text
kipo/
├── Cargo.toml                  # Workspace manifest (Apache-2.0, zero `unsafe`, clippy unwrap/expect denied)
├── scripts/install.sh          # POSIX release installer with SHA-256 verification
├── docs/
│   ├── ARCHITECTURE.md         # Master architecture, SQLite schema & semiring invariants
│   └── INGEST-SCHEMA.md        # Public JSONL Ingest Contract & Draft 2020-12 JSON Schema
└── crates/
    ├── kp-model/               # Domain newtypes, Provenance Semiring, PublicIngestLine, SecretRedactor
    ├── kp-store/               # Single-file WAL SQLite, FTS5 (`jieba-rs`), local ONNX + cloud Embedders, Export/Apply
    ├── kp-derive/              # Batched LLM/Host-CLI extraction, Datalog rules, JTMS cascade, `why()` proofs
    ├── kp-retrieve/            # Filter-before-rank hybrid search (BM25 + Vec + RRF + DPP), PreToolUse guard, Judges
    ├── kp-ingest/              # Claude/Codex Hooks, code symbol anchors (`tree-sitter`), Doc router, `kp dream` RSI
    ├── kp-net/                 # Private `0700` UDS `kp daemon`, `127.0.0.1` Web Inspector, Community Axum server
    ├── kp-connect/             # Decoupled external connectors (`kp-connector`: Feishu, GitHub, Sessions -> JSONL)
    └── kp-cli/                 # Package `kipo` -> single binary `kp` (CLI, daemon, inspector, MCP, community server)
```

---

## Development & Verification

All core, hook, connector, daemon, inspector, and end-to-end tests run 100% offline without external API keys:

```sh
sh -n scripts/install.sh
cargo fmt --all -- --check
cargo clippy -p kipo --all-targets --no-default-features --locked -- -D warnings
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
KP_OFFLINE=1 KP_DISABLE_MODEL_DOWNLOAD=1 cargo test --workspace --all-features --locked
```

---

## License

Licensed under the [Apache License, Version 2.0](LICENSE). Attributions for adapted open-source algorithms, prompt structures, and dependencies are documented in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
