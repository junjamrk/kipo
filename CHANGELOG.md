# Changelog

All notable changes to Kipo (`kp`) are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-11-25

### Added

* **Isomorphic single-file SQLite engine (`kp-store`, `kp-model`).** Unified `kp.sqlite` WAL database storing content-addressed `raw_blobs`, `jieba-rs` CJK and English FTS5 lexical indexes (`contentless_delete=1`), local ONNX (`local:bge-small-zh-v1.5`) and multi-provider HTTP vector embeddings, bitemporal `facts`, and JTMS `justifications`.
* **Read-time provenance semiring (`kp-model`, `kp-derive`, `kp-retrieve`).** Zero-copied-ACL visibility evaluation across `(viewer, time)`, conjunctive premise proofs, disjunctive multi-proof survival, `kp why` proof trees, and viewer-isolated `kp log` audit history.
* **Non-intrusive coding agent integrations (`kp-ingest`, `kp-cli`).** `kp connect` setup for Claude Code, OpenAI Codex, Cursor, and Windsurf; background `0700` Unix Domain Socket `kp daemon`; `<= 250`-token `UserPromptSubmit` context injection with verbatim quotes; write-time typed `CommandGuardSpec` `PreToolUse` guards and single-use `kp allow` overrides.
* **Public JSONL Ingest Contract (`docs/INGEST-SCHEMA.md`).** Closed Draft 2020-12 JSON Schema supporting `PublicContainerRoster` and `PublicIngestRecord` (`upsert`, `supersede`, `withdraw`) across `kp ingest`, stdio MCP `kp_note`, and daemon UDS `ingest_jsonl`.
* **Document, code, and web ingestion (`kp add`).** Anchored ingestion for Markdown, source code (`tree-sitter` symbol signature and body hashes across Rust, Python, TypeScript, JavaScript, and Go with `kp verify` drift checks), DOCX, PPTX, XLSX, PDF (with garbled-page quality quarantine), HTML, and SSRF-guarded HTTP/HTTPS URLs.
* **Read-only loopback Web Inspector and Markdown write-back.** Capability-authenticated four-column Web Inspector (`kp inspect` on `127.0.0.1`) and optimistic-concurrency `kp export` and `kp apply` Markdown roundtrips.
* **Sleep-time consolidation (`kp dream`) and skill lifecycle.** Session and multi-source mining, contrastive attribution, safety scanning, `k-of-N` (`helped >= 2, hurt == 0`) promotion to `SKILL.md`, imported skill quarantine (`kp skill approve`), and automatic retirement on premise withdrawal or `hurt` feedback.
* **Community server and decoupled connectors (`kp-net`, `kp-connect`).** Shared Community Library server (`kp serve --community` over REST, Streamable HTTP MCP, and A2A), durable `outbox` sync (`kp publish`, `kp sync`, `kp community join`), and standalone `kp-connector` for GitHub, Feishu (`lark-cli`), and agent session backfills.
* **Cross-platform distribution and supply-chain verification.** Six-target release matrix (`x86_64` and `aarch64` across Linux `glibc >= 2.28`, macOS, and Windows MSVC), `SHA256SUMS`-verified `install.sh` and `install.ps1` installers, Homebrew formula generation, and Sigstore build provenance attestations.
