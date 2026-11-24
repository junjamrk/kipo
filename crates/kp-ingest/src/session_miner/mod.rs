//! Zero-handwritten-JSON contrastive session miner for `kp dream`.
//!
//! Scans real coding-agent transcripts across four formats:
//! 1. Antigravity `transcript.jsonl` (`USER_INPUT`, `PLANNER_RESPONSE`, `GENERIC`, and background
//!    `run_command` task completions);
//! 2. Claude Code `.jsonl` (`user` / `assistant` messages with `tool_use` and `tool_result` blocks);
//! 3. Codex `.jsonl` (`event_msg` and `response_item` `message` / `function_call` / `function_call_output`);
//! 4. In-store captured session chunks or exported Markdown dialogue logs (`[speaker:...]` / `[assistant]`
//!    or `## 用户 · ...` / `## Codex · ...`).
//!
//! Every extracted snippet and trace is scrubbed via [`SecretRedactor`] and home-directory path
//! normalization (`~`) prior to storage, anchored to L0 `raw_sources` via `verify_verbatim_quote`
//! as an `Explicit([actor])` `kp.rsi.evidence` base fact, and linked as a formal premise of the
//! recorded [`Episode`].

mod classifier;
mod parsers;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use kp_model::{
    verify_verbatim_quote, ActorId, Attribution, AudienceSpec, BitemporalStamp, ContentHash,
    ContrastKind, EpochMs, Fact, FactAccess, FactId, FactKind, FactRank, FactStatus,
    ModelRoutePolicy, PublicAudience, PublicIngestOp, PublicIngestRecord, PublicIngestSource,
    PublicSpeaker, QuoteLocationHint, RejectionEvidenceKey, RejectionRecord, RsiDimension,
    SecretRedactor, SourceId, SourceScopeId, TrustTier, ValidityWindow,
};
use kp_store::{Episode, FactInput, FtsOnlyEmbedder, Store, StoreError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{IngestError, Result};
use classifier::extract_contrast_candidates;
use parsers::{parse_session_file, parse_text_or_jsonl_str};

const MAC_HOME_ROOT: &str = concat!("/", "Users", "/");
const LINUX_HOME_ROOT: &str = concat!("/", "home", "/");
const WIN_BACKSLASH_USERS: &str = concat!(":\\", "Users", "\\");
const WIN_ESCAPED_BACKSLASH_USERS: &str = concat!(":\\\\", "Users", "\\\\");
const WIN_SLASH_USERS: &str = concat!(":/", "Users", "/");
pub(super) const MAX_EPISODES_PER_SKILL_PER_SESSION: usize = 2;

/// Summary of mining contrastive episodes and L0 verbatim evidence facts from session logs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMineReport {
    pub sessions_scanned: usize,
    pub episodes_recorded: usize,
    pub evidence_facts_created: usize,
    pub source_ids: Vec<SourceId>,
    pub evidence_fact_ids: Vec<FactId>,
}

#[derive(Clone, Debug)]
pub(super) enum SessionEvent {
    UserTurn {
        step_label: String,
        text: String,
    },
    AssistantTurn {
        step_label: String,
        text: String,
        has_code_mutation: bool,
    },
    ToolExecution {
        step_label: String,
        command_hint: Option<String>,
        output: String,
        failed: bool,
        succeeded: bool,
        is_code_mutation: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FailureCategory {
    RustTypeAndBorrow,
    RustVisibilityAndCfg,
    ClippyStrictWarnings,
    PythonImportAndType,
    JsTsModuleAndType,
    GoCompileAndTest,
    DatabaseSchemaAndMigration,
    TestFailureTargeted,
    CliCommandRecovery,
}

impl FailureCategory {
    pub(super) fn requires_build_or_test_verification(self) -> bool {
        !matches!(self, Self::CliCommandRecovery)
    }

    pub(super) fn to_attribution(self) -> Result<Attribution> {
        let (slug, trigger, wrong_move, fix) = match self {
            Self::RustTypeAndBorrow => (
                "rust-type-and-borrow-signature-check",
                "resolving Rust type, trait bound, or borrow-checker signature mismatches during compilation",
                "Guess argument types or clone blindly without inspecting the callee signature and trait bounds",
                "Inspect the exact callee signature and trait bounds first, align call-site types or borrows minimally, and rerun cargo check or test until exit code 0.",
            ),
            Self::RustVisibilityAndCfg => (
                "rust-visibility-and-cfg-scope-check",
                "fixing unresolved imports, module visibility, or platform cfg-gated symbol errors in Rust",
                "Import platform-specific or private symbols unconditionally across targets and feature sets",
                "Align module pub(crate) visibility and matching #[cfg] or feature gates across imports and call sites, then verify both minimal and all-features builds.",
            ),
            Self::ClippyStrictWarnings => (
                "clippy-strict-warnings-gate",
                "clearing strict Rust clippy or compiler lints enforced under -D warnings",
                "Suppress lints broadly or leave dead-code and redundant-clone patterns unverified across feature flags",
                "Apply the idiomatic Rust lint fix without weakening safety invariants, then rerun clippy across --no-default-features and --all-features with -D warnings.",
            ),
            Self::PythonImportAndType => (
                "python-import-and-env-verification",
                "resolving Python import, environment, or runtime type errors",
                "Invoke system python or patch imports speculatively without checking the active project environment and module exports",
                "Verify the project virtual environment and module exports first, apply the targeted Python fix, and rerun pytest or the entrypoint until exit code 0.",
            ),
            Self::JsTsModuleAndType => (
                "ts-js-type-and-module-resolution-check",
                "fixing TypeScript or JavaScript module resolution and type-checking failures",
                "Bypass TypeScript diagnostics or guess package import paths without checking tsconfig and exported types",
                "Align import specifiers and exported TypeScript types with tsconfig, then rerun typecheck and tests until exit code 0.",
            ),
            Self::GoCompileAndTest => (
                "go-symbol-and-package-test-check",
                "fixing Go package compilation or test failures",
                "Modify exported Go signatures or interfaces without checking package callers and running go test",
                "Align Go struct, interface, and package imports across callers and rerun go test on affected packages until exit code 0.",
            ),
            Self::DatabaseSchemaAndMigration => (
                "database-schema-and-migration-check",
                "recovering from SQL, ORM schema, or database migration command errors",
                "Run database queries or migrations against mismatched schemas or missing explicit schema flags",
                "Verify the target database schema file and migration flags before rerunning the query or migration command to exit code 0.",
            ),
            Self::TestFailureTargeted => (
                "test-failure-targeted-verification",
                "diagnosing and repairing a failing automated unit or integration test",
                "Patch symptoms speculatively without tracing the failing assertion back to the underlying invariant",
                "Read the failing assertion and domain invariant, apply a minimal root-cause fix, and rerun the exact failing test target until test result is ok.",
            ),
            Self::CliCommandRecovery => (
                "cli-command-and-shell-flag-recovery",
                "recovering from a non-zero CLI command or script invocation failure",
                "Proceed past a non-zero command exit or retry identical invalid flags without adjusting the invocation",
                "Inspect stderr and supported CLI flags, correct the command arguments or target state, and rerun verification until exit code 0.",
            ),
        };
        Ok(Attribution {
            skill_slug: slug.parse()?,
            trigger: trigger.into(),
            wrong_move: wrong_move.into(),
            verified_fix: fix.into(),
            dimension: RsiDimension::Inv,
            cited_premise_ids: Vec::new(),
        })
    }
}

pub(super) fn human_correction_attribution() -> Result<Attribution> {
    Ok(Attribution {
        skill_slug: "honor-explicit-user-redirect".parse()?,
        trigger: "a user explicitly corrects or redirects the active technical approach or boundary"
            .into(),
        wrong_move:
            "Continue executing the superseded assumption or workaround after an explicit user correction"
                .into(),
        verified_fix:
            "Stop the superseded approach immediately, adopt the user-specified constraint or direction, and verify the next action satisfies it."
                .into(),
        dimension: RsiDimension::Inv,
        cited_premise_ids: Vec::new(),
    })
}

#[derive(Clone, Debug)]
pub(super) struct PendingFailure {
    pub(super) step_label: String,
    pub(super) category: FailureCategory,
    pub(super) verbatim_quote: String,
    pub(super) failure_excerpt: String,
    pub(super) mutated_after_failure: bool,
    pub(super) failed_command: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct MinedCandidate {
    pub(super) signal_kind: ContrastKind,
    pub(super) step_ref: String,
    pub(super) before_excerpt: String,
    pub(super) after_excerpt: String,
    pub(super) verbatim_quote: String,
    pub(super) snippet_text: String,
    pub(super) attribution: Attribution,
    pub(super) observed_commands: Vec<String>,
}

/// Scrubs secrets via [`SecretRedactor`] and normalizes personal macOS/Linux/Windows home directory prefixes to `~`.
pub fn scrub_session_privacy(raw: &str, redactor: &SecretRedactor) -> String {
    let redacted = redactor.redact(raw);
    scrub_personal_home_paths(&redacted)
}

pub(crate) fn scrub_personal_home_paths(input: &str) -> String {
    let mut current = replace_windows_home_prefix(input, WIN_ESCAPED_BACKSLASH_USERS);
    current = replace_windows_home_prefix(&current, WIN_BACKSLASH_USERS);
    current = replace_windows_home_prefix(&current, WIN_SLASH_USERS);
    current = replace_home_prefix(&current, MAC_HOME_ROOT);
    current = replace_home_prefix(&current, LINUX_HOME_ROOT);
    current
}

fn replace_windows_home_prefix(text: &str, marker: &str) -> String {
    if !text.contains(marker) {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    while let Some(rel) = text[cursor..].find(marker) {
        let marker_pos = cursor + rel;
        let has_drive = marker_pos >= 1 && text.as_bytes()[marker_pos - 1].is_ascii_alphabetic();
        if !has_drive {
            let step = marker_pos + marker.len();
            out.push_str(&text[cursor..step]);
            cursor = step;
            continue;
        }
        let drive_start = marker_pos - 1;
        let boundary_ok = if drive_start == 0 {
            true
        } else {
            let prefix = &text[..drive_start];
            prefix.ends_with("file:///")
                || prefix.chars().next_back().is_none_or(|ch| {
                    !ch.is_ascii_alphanumeric() && !matches!(ch, '/' | '\\' | '.' | '_' | '-')
                })
        };
        let user_start = marker_pos + marker.len();
        let after_root = &text[user_start..];
        let user_len = after_root
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
            .count();
        let valid_user = (1..=64).contains(&user_len)
            && &after_root[..user_len] != "."
            && &after_root[..user_len] != "..";
        let next_boundary_ok = after_root.len() == user_len
            || !after_root.as_bytes()[user_len].is_ascii_alphanumeric();

        if boundary_ok && valid_user && next_boundary_ok {
            out.push_str(&text[cursor..drive_start]);
            out.push('~');
            cursor = user_start + user_len;
        } else {
            let step = marker_pos + marker.len();
            out.push_str(&text[cursor..step]);
            cursor = step;
        }
    }
    out.push_str(&text[cursor..]);
    out
}

fn replace_home_prefix(text: &str, root: &str) -> String {
    if !text.contains(root) {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    while let Some(rel) = text[cursor..].find(root) {
        let match_start = cursor + rel;
        let boundary_ok = if match_start == 0 {
            true
        } else {
            let prefix = &text[..match_start];
            prefix.ends_with("file://")
                || prefix.chars().next_back().is_none_or(|ch| {
                    !ch.is_ascii_alphanumeric() && !matches!(ch, '/' | '.' | '_' | '-')
                })
        };
        let user_start = match_start + root.len();
        let after_root = &text[user_start..];
        let user_len = after_root
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
            .count();
        let valid_user = (1..=64).contains(&user_len)
            && &after_root[..user_len] != "."
            && &after_root[..user_len] != "..";
        let next_boundary_ok = after_root.len() == user_len
            || !after_root.as_bytes()[user_len].is_ascii_alphanumeric();

        if boundary_ok && valid_user && next_boundary_ok {
            out.push_str(&text[cursor..match_start]);
            out.push('~');
            cursor = user_start + user_len;
        } else {
            let step = match_start + root.len();
            out.push_str(&text[cursor..step]);
            cursor = step;
        }
    }
    out.push_str(&text[cursor..]);
    out
}

/// Mines session files or directories (`.jsonl` and `.md`, rejecting symlinks) into `store`.
pub fn mine_session_paths(
    store: &mut Store,
    paths: &[PathBuf],
    actor: &ActorId,
    route_policy: &ModelRoutePolicy,
    at: EpochMs,
) -> Result<SessionMineReport> {
    if store.member_role(actor)?.is_none() {
        return Err(kp_store::StoreError::PermissionDenied.into());
    }
    let mut files = Vec::new();
    for path in paths {
        let meta = fs::symlink_metadata(path)?;
        if meta.file_type().is_symlink() {
            return Err(IngestError::Invalid("session path must not be a symlink"));
        }
        if meta.is_file() {
            files.push(path.clone());
        } else if meta.is_dir() {
            collect_session_files_recursive(path, &mut files)?;
        }
    }
    files.sort();
    files.dedup();

    let rejections = store.list_rejections(None)?;
    let redactor = SecretRedactor::new()?;
    let mut report = SessionMineReport::default();

    for (file_idx, file_path) in files.iter().enumerate() {
        let (session_id, events) = parse_session_file(file_path, &redactor)?;
        report.sessions_scanned += 1;
        let candidates = extract_contrast_candidates(&events, &redactor)?;
        for (cand_idx, cand) in candidates.into_iter().enumerate() {
            let occurred_at =
                EpochMs(at.0.saturating_add(i64::try_from(file_idx * 100 + cand_idx).unwrap_or(0)));
            let digest = ContentHash::digest(
                format!("{session_id}\n{}\n{}", cand.step_ref, cand.verbatim_quote).as_bytes(),
            );
            let ext_id = format!("dream-{session_id}-{}", &digest.as_str()[..16]);
            let record = PublicIngestRecord {
                source: PublicIngestSource {
                    kind: "dream-evidence".into(),
                    uri: format!("dream-session:{session_id}:{}", cand.step_ref),
                    external_id: ext_id,
                    container_id: None,
                },
                speaker: PublicSpeaker::new(actor.as_str(), None),
                audience: PublicAudience::Explicit(vec![actor.clone()]),
                occurred_at,
                content: cand.snippet_text.clone(),
                op: PublicIngestOp::Upsert,
                ingested_by: None,
                media_type: Some("text/plain".into()),
                model_policy: Some(route_policy.clone()),
                thread_key: None,
                reply_to: None,
            };
            let applied = match store.apply_public_ingest_record(&record, actor, route_policy) {
                Ok(out) => out,
                Err(StoreError::Tombstoned(_)) => continue,
                Err(err) => return Err(err.into()),
            };
            if let Some(job_id) = applied.enqueued_job_id.as_deref() {
                store.complete_extraction_job(job_id)?;
            }
            let Some(source_id) = applied.source_id else {
                continue;
            };
            let ev_ctx = MinedEvidenceCtx {
                source_id: &source_id,
                session_id: &session_id,
                actor,
                route_policy,
                rejections: &rejections,
                at: occurred_at,
            };
            let ev_opt = match persist_verified_evidence_fact(store, &ev_ctx, &cand) {
                Ok(opt) => opt,
                Err(IngestError::Store(StoreError::Tombstoned(_))) => continue,
                Err(err) => return Err(err),
            };
            let Some((ev_fact_id, created_fact)) = ev_opt else {
                continue;
            };
            if !report.source_ids.contains(&source_id) {
                report.source_ids.push(source_id.clone());
            }
            if created_fact {
                report.evidence_facts_created += 1;
            }
            if !report.evidence_fact_ids.contains(&ev_fact_id) {
                report.evidence_fact_ids.push(ev_fact_id.clone());
            }
            let recorded = match record_mined_episode(
                store,
                &MinedEpisodeInput {
                    session_id: &session_id,
                    source_id: &source_id,
                    cand: &cand,
                    evidence_fact_id: &ev_fact_id,
                    actor,
                    route_policy,
                    at: occurred_at,
                },
            ) {
                Ok(n) => n,
                Err(IngestError::Store(StoreError::Tombstoned(_))) => continue,
                Err(err) => return Err(err),
            };
            report.episodes_recorded += recorded;
        }
    }

    Ok(report)
}

/// Automatically scans unmined in-store session sources (`session:*`, `agent-session:*`, `transcript:*`)
/// readable by `actor`, binds L0 verbatim evidence premises directly to each existing `source_id`,
/// and marks each scanned `source_id` via `Store::mark_session_source_dream_mined`.
pub fn mine_unprocessed_store_sessions(
    store: &mut Store,
    actor: &ActorId,
    at: EpochMs,
) -> Result<SessionMineReport> {
    if store.member_role(actor)?.is_none() {
        return Err(kp_store::StoreError::PermissionDenied.into());
    }
    let unmined = store.unmined_session_source_ids(actor)?;
    if unmined.is_empty() {
        return Ok(SessionMineReport::default());
    }

    let existing_pending_sources: BTreeSet<String> = store
        .pending_episodes(actor)?
        .into_iter()
        .filter_map(|p| {
            p.episode
                .environment
                .get("source_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect();

    let rejections = store.list_rejections(None)?;
    let redactor = SecretRedactor::new()?;
    let mut report = SessionMineReport::default();

    for source_id in unmined {
        if existing_pending_sources.contains(source_id.as_str()) {
            store.mark_session_source_dream_mined(&source_id, actor, at)?;
            continue;
        }
        let Some(raw_text) = store.raw_source_text(&source_id)? else {
            store.mark_session_source_dream_mined(&source_id, actor, at)?;
            continue;
        };
        let chunks = store.source_anchored_chunks(&source_id)?;
        if chunks.is_empty() {
            store.mark_session_source_dream_mined(&source_id, actor, at)?;
            continue;
        }

        let first_ctx = store.extraction_source_context(&source_id, &[])?;
        let (uri, source_scope_id) = match first_ctx {
            Some(ctx) => (ctx.uri, ctx.source_scope_id),
            None => (String::new(), None),
        };
        let session_id = derive_store_session_id(&uri, &source_id);
        let effective_policy = store.source_effective_route_policy(&source_id)?;

        let events = parse_text_or_jsonl_str(&raw_text, &redactor);
        let candidates = extract_contrast_candidates(&events, &redactor)?;
        report.sessions_scanned += 1;

        let ev_ctx = MinedEvidenceCtx {
            source_id: &source_id,
            session_id: &session_id,
            actor,
            route_policy: &effective_policy,
            rejections: &rejections,
            at,
        };
        for cand in candidates {
            let ev_opt = match persist_verified_evidence_fact_from_chunks(
                store,
                &ev_ctx,
                source_scope_id.clone(),
                &chunks,
                &cand,
            ) {
                Ok(opt) => opt,
                Err(IngestError::Store(StoreError::Tombstoned(_))) => continue,
                Err(_) => None,
            };
            if let Some((ev_fact_id, created_fact)) = ev_opt {
                if created_fact {
                    report.evidence_facts_created += 1;
                }
                if !report.source_ids.contains(&source_id) {
                    report.source_ids.push(source_id.clone());
                }
                if !report.evidence_fact_ids.contains(&ev_fact_id) {
                    report.evidence_fact_ids.push(ev_fact_id.clone());
                }
                if let Ok(n) = record_mined_episode(
                    store,
                    &MinedEpisodeInput {
                        session_id: &session_id,
                        source_id: &source_id,
                        cand: &cand,
                        evidence_fact_id: &ev_fact_id,
                        actor,
                        route_policy: &effective_policy,
                        at,
                    },
                ) {
                    report.episodes_recorded += n;
                }
            }
        }

        store.mark_session_source_dream_mined(&source_id, actor, at)?;
    }

    Ok(report)
}

struct MinedEpisodeInput<'a> {
    session_id: &'a str,
    source_id: &'a SourceId,
    cand: &'a MinedCandidate,
    evidence_fact_id: &'a FactId,
    actor: &'a ActorId,
    route_policy: &'a ModelRoutePolicy,
    at: EpochMs,
}

struct MinedEvidenceCtx<'a> {
    source_id: &'a SourceId,
    session_id: &'a str,
    actor: &'a ActorId,
    route_policy: &'a ModelRoutePolicy,
    rejections: &'a [RejectionRecord],
    at: EpochMs,
}

fn record_mined_episode(store: &mut Store, input: &MinedEpisodeInput<'_>) -> Result<usize> {
    let MinedEpisodeInput {
        session_id,
        source_id,
        cand,
        evidence_fact_id,
        actor,
        route_policy,
        at,
    } = *input;
    let mut env = json!({
        "actor": actor,
        "verified": true,
        "source_id": source_id,
        "modality": "agent_session",
        "target": cand.attribution.skill_slug.as_str(),
        "fallback_attribution": cand.attribution,
        "observed_commands": cand.observed_commands,
    });
    if *route_policy == ModelRoutePolicy::RawOnly {
        env["model_policy"] = json!("raw_only");
        env["raw_only"] = json!(true);
    }
    let episode = Episode {
        signal_kind: cand.signal_kind,
        session_id: session_id.to_owned(),
        before_trace: json!({
            "content": cand.before_excerpt,
            "success": false,
            "target": cand.attribution.skill_slug.as_str(),
        }),
        after_trace: json!({
            "content": cand.after_excerpt,
            "success": true,
            "target": cand.attribution.skill_slug.as_str(),
        }),
        step_ref: cand.step_ref.clone(),
        environment: env,
    };
    crate::signals::record(
        store,
        episode,
        actor,
        std::slice::from_ref(evidence_fact_id),
        at,
    )
}

fn persist_verified_evidence_fact(
    store: &mut Store,
    ctx: &MinedEvidenceCtx<'_>,
    cand: &MinedCandidate,
) -> Result<Option<(FactId, bool)>> {
    let source_scope_id = store
        .extraction_source_context(ctx.source_id, &[])?
        .and_then(|c| c.source_scope_id);
    let chunks = store.source_anchored_chunks(ctx.source_id)?;
    persist_verified_evidence_fact_from_chunks(store, ctx, source_scope_id, &chunks, cand)
}

fn persist_verified_evidence_fact_from_chunks(
    store: &mut Store,
    ctx: &MinedEvidenceCtx<'_>,
    source_scope_id: Option<SourceScopeId>,
    chunks: &[kp_store::AnchoredSourceChunk],
    cand: &MinedCandidate,
) -> Result<Option<(FactId, bool)>> {
    let MinedEvidenceCtx {
        source_id,
        session_id,
        actor,
        route_policy,
        rejections,
        at,
    } = *ctx;
    let mut matched = None;
    for chunk in chunks {
        let hint = QuoteLocationHint {
            turn_ordinal: None,
            approx_char_offset: chunk
                .body
                .find(&cand.verbatim_quote)
                .and_then(|b| u32::try_from(chunk.body[..b].chars().count()).ok()),
        };
        if let Ok(ver) = verify_verbatim_quote(&chunk.body, &cand.verbatim_quote, hint) {
            matched = Some((chunk, ver));
            break;
        }
    }
    let Some((matched_chunk, verified)) = matched else {
        return Err(IngestError::Invalid(
            "mined verbatim evidence quote not found in stored source chunks",
        ));
    };

    let ev_key = RejectionEvidenceKey {
        speaker: actor.clone(),
        chunk_sha256: ContentHash::digest(&matched_chunk.body),
        quote_span: verified.span,
    };
    if rejections
        .iter()
        .any(|rec| rec.matches_candidate(Some(&ev_key), None))
    {
        return Ok(None);
    }

    let digest = ContentHash::digest(
        serde_json::to_vec(&(
            store.library_id(),
            actor,
            source_id,
            &matched_chunk.chunk_id,
            &verified.exact_quote,
            verified.span.char_start,
        ))?
        .as_slice(),
    );
    let logical_key: kp_model::LogicalKey = format!("rsi-evidence:{digest}").parse()?;
    if let Some(existing) = store
        .visible_facts(actor, at)?
        .into_iter()
        .find(|f| f.origin_library == *store.library_id() && f.logical_key == logical_key)
    {
        return Ok(Some((existing.id, false)));
    }

    let fact_id = kp_store::new_fact_id()?;
    let mut tags = BTreeSet::new();
    if *route_policy == ModelRoutePolicy::RawOnly {
        tags.insert("kp.raw_only".to_owned());
    }
    let mut merged_anchor = matched_chunk.anchor.clone();
    merged_anchor.source_id = Some(source_id.clone());
    merged_anchor.chunk_id = Some(matched_chunk.chunk_id.clone());
    merged_anchor.verbatim_quote = Some(verified.exact_quote.clone());
    merged_anchor.char_start = Some(verified.span.char_start);
    merged_anchor.char_end = Some(verified.span.char_end);

    let fact = Fact {
        id: fact_id.clone(),
        origin_library: store.library_id().clone(),
        logical_key,
        kind: FactKind::Observation,
        subject: format!("session:{session_id}"),
        predicate: "kp.rsi.evidence".into(),
        object: json!({
            "summary": verified.exact_quote,
            "session_id": session_id,
            "step_ref": cand.step_ref,
            "signal_kind": cand.signal_kind.as_str(),
        }),
        tags,
        author: actor.clone(),
        ingested_by: Some(actor.clone()),
        trust_tier: TrustTier::HumanVerbatim,
        access: FactAccess::Base {
            audience: AudienceSpec::Explicit(BTreeSet::from([actor.clone()])),
            stamp: BitemporalStamp {
                valid: ValidityWindow {
                    valid_from: EpochMs(0),
                    valid_until: EpochMs::OPEN_END,
                },
                recorded_at: at,
                expired_at: None,
            },
        },
        status: FactStatus::Active,
        superseded_by: None,
        source_anchor: Some(merged_anchor),
        governed_scope: None,
        broader_scope: None,
        source_scope_id,
        rank: FactRank::Normal,
        miss_count: 0,
        unverified_gaps: Vec::new(),
    };
    let input = FactInput {
        fact,
        entities: BTreeSet::new(),
    };
    let remembered = if *route_policy == ModelRoutePolicy::RawOnly {
        store.remember_with_embedder(std::slice::from_ref(&input), &FtsOnlyEmbedder)?
    } else {
        store.remember(std::slice::from_ref(&input))?
    };
    let out_id = remembered.facts.into_iter().next().unwrap_or(fact_id);
    Ok(Some((out_id, true)))
}

fn collect_session_files_recursive(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        entries.push(entry.path());
    }
    entries.sort();
    for path in entries {
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            collect_session_files_recursive(&path, out)?;
        } else if meta.is_file() {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if matches!(ext.as_str(), "jsonl" | "md") {
                out.push(path);
            }
        }
    }
    Ok(())
}

pub(super) fn derive_file_session_id(
    path: &Path,
    embedded_id: Option<&str>,
    redactor: &SecretRedactor,
) -> String {
    if let Some(id) = embedded_id.map(str::trim).filter(|s| !s.is_empty()) {
        return sanitize_session_token(&scrub_session_privacy(id, redactor));
    }
    let components: Vec<String> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str().map(str::to_owned))
        .collect();
    // Detect `<conv_id>/.system_generated/logs/transcript.jsonl` Antigravity layout.
    for win in components.windows(4) {
        if win[1] == ".system_generated" && win[2] == "logs" && win[3].starts_with("transcript") {
            return format!("ag-{}", sanitize_session_token(&win[0]));
        }
    }
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session");
    let parent_name = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("dir");
    let path_hash =
        ContentHash::digest(scrub_personal_home_paths(&path.to_string_lossy()).as_bytes());
    format!(
        "sess-{}-{}-{}",
        sanitize_session_token(parent_name),
        sanitize_session_token(stem),
        &path_hash.as_str()[..8]
    )
}

fn derive_store_session_id(uri: &str, source_id: &SourceId) -> String {
    let trimmed = uri
        .strip_prefix("session:")
        .or_else(|| uri.strip_prefix("agent-session:"))
        .or_else(|| uri.strip_prefix("transcript:"))
        .unwrap_or(uri);
    let head = trimmed.split(':').next().unwrap_or("").trim();
    if head.is_empty() {
        sanitize_session_token(source_id.as_str())
    } else {
        sanitize_session_token(head)
    }
}

fn sanitize_session_token(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(48));
    for ch in raw.chars() {
        if out.len() >= 48 {
            break;
        }
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            out.push(ch.to_ascii_lowercase());
        } else if matches!(ch, '.' | '/' | ':' | ' ') && !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "session".into()
    } else {
        trimmed.to_owned()
    }
}
