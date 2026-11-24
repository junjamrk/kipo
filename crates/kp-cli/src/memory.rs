use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand, ValueEnum};
use kp_derive::{FunnelOptions, Memory, RuleExtractor};
use kp_model::{
    ActorId, AudienceSpec, EpochMs, FactKind, KpError, LibraryId, LogicalKey, MemberRole,
    ModelRoutePolicy, ValidityWindow,
};
use kp_retrieve::judge::{HttpJudge, JudgeConfig, JudgeTier};
use kp_retrieve::{merge_federated_results, search, search_shallow, SearchOptions};
use kp_store::{PublicIngestStreamReport, Store, StoreError};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{now, output, Cli};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Subcommand)]
pub enum MemoryCommand {
    /// Stream newline-delimited Public Ingest Contract JSONL records (`PublicContainerRoster` & `PublicIngestRecord`) from `<file>` or `stdin` (`-`).
    Ingest {
        file: Option<PathBuf>,
        #[arg(long)]
        extract: bool,
    },
    /// Import a file, directory, or web URL (`http://` / `https://`) into L0 `raw_blobs` and search indexes.
    Add {
        file_or_dir: String,
        #[arg(long)]
        readable_by: Option<String>,
        #[arg(long)]
        extract: bool,
        #[arg(long)]
        trusted: bool,
    },
    /// Record an explicit structured fact (optionally anchored to a `tree-sitter` code symbol).
    Remember(RememberArgs),
    /// Record a verified failed approach (`ClosedNegative`) so agents avoid repeating it.
    DeadEnd(DeadEndArgs),
    /// Withdraw a fact or logical key, cascading JTMS invalidation to dependent conclusions.
    Forget {
        fact_id_or_logical_key: String,
        #[arg(long)]
        at: Option<i64>,
    },
    /// Permanently erase a logical key or source with an opaque HMAC tombstone (`secure_delete`).
    Purge {
        logical_key_or_source_id: String,
        #[arg(long)]
        origin: Option<LibraryId>,
        #[arg(long, default_value = "purged")]
        reason: String,
    },
    /// Run viewer-isolated hybrid search (BM25 FTS5 + vector RRF + DPP diversification).
    Search(SearchArgs),
}

#[derive(Args)]
pub struct RememberArgs {
    #[arg(long)]
    subject: String,
    #[arg(long)]
    predicate: String,
    #[arg(long)]
    value: String,
    #[arg(long, default_value = "fact_slot")]
    kind: FactKind,
    #[arg(long)]
    logical_key: Option<LogicalKey>,
    #[arg(long)]
    tags: Option<String>,
    #[arg(long)]
    valid_from: Option<i64>,
    #[arg(long)]
    valid_until: Option<i64>,
    #[arg(long)]
    audience: Option<String>,
    #[command(flatten)]
    anchor: AnchorArgs,
}

#[derive(Args, Default, Deserialize)]
pub(crate) struct AnchorArgs {
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long, requires = "symbol")]
    pub file: Option<PathBuf>,
    #[arg(long, requires = "file")]
    pub symbol: Option<String>,
}

#[derive(Args)]
pub struct DeadEndArgs {
    #[arg(long)]
    subject: String,
    #[arg(long)]
    hypothesis: String,
    #[arg(long)]
    reason: String,
    #[command(flatten)]
    anchor: AnchorArgs,
}

pub(crate) fn remember_memory(
    store: &mut Store,
    memory: Memory,
    options: &FunnelOptions,
    anchor: &AnchorArgs,
) -> Result<kp_store::Remembered> {
    if store.member_role(&options.actor)?.is_none() {
        return Err(KpError::invalid("actor", "not an active library member").into());
    }
    let mut result = match (&anchor.file, &anchor.symbol) {
        (Some(file), Some(symbol)) => {
            let repo = anchor.repo.clone().unwrap_or(std::env::current_dir()?);
            let locus = kp_ingest::code_intel::locate(&repo, file, symbol)?;
            let input = kp_derive::base_fact(store, memory, options)?;
            store.remember_anchored(input, &locus)?
        }
        (None, None) => kp_derive::remember(store, memory, options)?,
        _ => {
            return Err(
                KpError::invalid("anchor", "file and symbol must be supplied together").into(),
            )
        }
    };
    let visible = store
        .derivation_view()?
        .evaluate(&options.actor, options.at);
    result.conflicts.retain(|id| visible.facts.contains(id));
    Ok(result)
}

pub(crate) fn dead_end_memory(
    subject: String,
    hypothesis: String,
    reason: String,
    at: EpochMs,
) -> Memory {
    Memory {
        subject,
        predicate: "kp.code.pitfall".into(),
        value: json!({"hypothesis":hypothesis,"reason":reason}),
        kind: FactKind::ClosedNegative,
        logical_key: None,
        tags: BTreeSet::new(),
        entities: BTreeSet::new(),
        valid: ValidityWindow {
            valid_from: at,
            valid_until: EpochMs::OPEN_END,
        },
    }
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Tier {
    Off,
    LocalReranker,
    LocalDecision,
    CloudJev,
}

#[derive(Args)]
pub struct SearchArgs {
    query: String,
    #[arg(long)]
    deep: bool,
    #[arg(long, value_enum, default_value = "off")]
    judge_tier: Tier,
    #[arg(long)]
    judge_url: Option<String>,
    #[arg(long)]
    judge_model: Option<String>,
    #[arg(long)]
    judge_key_env: Option<String>,
    #[arg(long)]
    allow_cloud_jev: bool,
    #[arg(long)]
    at: Option<i64>,
    #[arg(long, default_value_t = 10)]
    limit: usize,
    #[arg(long)]
    also_library: Vec<PathBuf>,
}

pub async fn run(cli: &Cli, command: &MemoryCommand) -> Result<()> {
    let at = now()?;
    if let MemoryCommand::Search(args) = command {
        let tier = match args.judge_tier {
            Tier::Off => JudgeTier::Off,
            Tier::LocalReranker => JudgeTier::LocalReranker,
            Tier::LocalDecision => JudgeTier::LocalDecisionModel,
            Tier::CloudJev => JudgeTier::CloudJev,
        };
        let config = JudgeConfig {
            tier,
            url: args.judge_url.clone(),
            model: args.judge_model.clone(),
            api_key: args.judge_key_env.as_ref().map(std::env::var).transpose()?,
            allow_cloud_jev: args.allow_cloud_jev,
            ..JudgeConfig::default()
        };
        let judge = HttpJudge::new(config)?;
        let use_full_pipeline = args.deep || !matches!(args.judge_tier, Tier::Off);
        let mut store = if use_full_pipeline {
            Store::open(&cli.path)?
        } else {
            Store::open_readonly(&cli.path)?
        };
        crate::providers_cli::attach_ready_embedder(&mut store);
        let actor = cli
            .actor
            .clone()
            .unwrap_or_else(|| store.owner_actor().clone());
        let eval_at = args.at.map(EpochMs).unwrap_or(at);
        let options = SearchOptions {
            query: args.query.clone(),
            viewer: actor.clone(),
            at: eval_at,
            limit: args.limit,
            deep: args.deep,
            max_hops: 2,
        };
        let mut result = if use_full_pipeline {
            search(&store, &options, &judge).await?
        } else {
            search_shallow(&store, &actor, eval_at, &args.query, args.limit)?
        };
        if !args.also_library.is_empty() {
            let mut slices = vec![result.results];
            for path in &args.also_library {
                let mut other = if use_full_pipeline {
                    Store::open(path)?
                } else {
                    Store::open_readonly(path)?
                };
                crate::providers_cli::attach_ready_embedder(&mut other);
                let output = if use_full_pipeline {
                    search(&other, &options, &judge).await?
                } else {
                    search_shallow(&other, &actor, eval_at, &args.query, args.limit)?
                };
                slices.push(output.results);
                result.trace.extend(output.trace);
            }
            result.results = merge_federated_results(slices);
            result.results.truncate(args.limit);
        }
        let human = result
            .results
            .iter()
            .map(|r| {
                format!(
                    "[{:.3}] {}\n{}\n[kp-ref:{}]",
                    r.score, r.heading, r.passage, r.id
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        output(cli.json, &result, &human)?;
        return Ok(());
    }

    let mut store = Store::open(&cli.path)?;
    crate::providers_cli::attach_ready_embedder(&mut store);
    let actor = cli
        .actor
        .clone()
        .unwrap_or_else(|| store.owner_actor().clone());
    match command {
        MemoryCommand::Ingest { file, extract } => {
            if store.member_role(&actor)?.is_none() {
                return Err(StoreError::PermissionDenied.into());
            }
            let global_policy = crate::governance::default_auto_route_policy();
            let report = match file.as_deref() {
                None => stream_public_jsonl_reader(
                    io::stdin().lock(),
                    &mut store,
                    &actor,
                    &global_policy,
                )?,
                Some(p) if p == Path::new("-") => stream_public_jsonl_reader(
                    io::stdin().lock(),
                    &mut store,
                    &actor,
                    &global_policy,
                )?,
                Some(path) => {
                    let reader = BufReader::new(fs::File::open(path)?);
                    stream_public_jsonl_reader(reader, &mut store, &actor, &global_policy)?
                }
            };

            let mut extraction_out: Option<Value> = None;
            if *extract {
                if global_policy != ModelRoutePolicy::RawOnly {
                    extraction_out = Some(
                        crate::governance::drain_for_public_ingest(&mut store, &actor, at).await?,
                    );
                } else {
                    let _ = kp_derive::derive(&mut store);
                }
                crate::community::notify_or_sync_joined(&cli.path, &mut store, at).await;
            } else if report.records_superseded > 0
                || report.records_withdrawn > 0
                || report.rosters_applied > 0
            {
                let _ = kp_derive::derive(&mut store);
                crate::community::notify_or_sync_joined(&cli.path, &mut store, at).await;
            }

            let mut payload = serde_json::to_value(&report)?;
            if let (Value::Object(ref mut map), Some(ext)) = (&mut payload, extraction_out) {
                map.insert("extraction".to_owned(), ext);
            }
            let human = format!(
                "Ingested {} rosters, {} upserted, {} duplicates, {} superseded, {} withdrawn ({} extraction jobs queued)",
                report.rosters_applied,
                report.records_upserted,
                report.records_duplicate,
                report.records_superseded,
                report.records_withdrawn,
                report.jobs_enqueued
            );
            output(cli.json, &payload, &human)?;
            drop(store);
            crate::rsi::maybe_autospawn_dream_in_background(&cli.path, &actor);
        }
        MemoryCommand::Add {
            file_or_dir,
            readable_by,
            extract,
            trusted,
        } => {
            let global_policy = crate::governance::default_auto_route_policy();
            let options = FunnelOptions {
                audience: audience(readable_by.as_deref(), &actor)?,
                actor: actor.clone(),
                at,
            };
            let trimmed_target = file_or_dir.trim();
            if trimmed_target.starts_with("http://") || trimmed_target.starts_with("https://") {
                let extractor: Option<&dyn kp_derive::Extractor> =
                    if *extract { Some(&RuleExtractor) } else { None };
                let res = kp_ingest::doc_router::add_url_with_policy(
                    &mut store,
                    trimmed_target,
                    &options,
                    &global_policy,
                    *trusted,
                    extractor,
                )
                .await?;
                output(cli.json, &vec![res], "Added 1 sources")?;
                drop(store);
                crate::rsi::maybe_autospawn_dream_in_background(&cli.path, &actor);
                return Ok(());
            }
            let target_path = Path::new(file_or_dir);
            let is_dir = fs::symlink_metadata(target_path)
                .map(|m| m.is_dir())
                .unwrap_or(false);
            let mut paths = Vec::new();
            collect_files(target_path, &mut paths)?;
            paths.sort();
            let mut results = Vec::new();
            let mut failed_files = Vec::new();
            for path in paths {
                let extractor: Option<&dyn kp_derive::Extractor> =
                    if *extract { Some(&RuleExtractor) } else { None };
                match kp_ingest::doc_router::add_file_with_policy(
                    &mut store,
                    &path,
                    &options,
                    &global_policy,
                    extractor,
                ) {
                    Ok(res) => results.push(res),
                    Err(err) if is_dir => {
                        failed_files.push(json!({
                            "path": path.display().to_string(),
                            "error": format!("{}: {err}", path.display()),
                        }));
                    }
                    Err(err) => return Err(err.into()),
                }
            }
            if failed_files.is_empty() {
                output(
                    cli.json,
                    &results,
                    &format!("Added {} sources", results.len()),
                )?;
            } else {
                output(
                    cli.json,
                    &json!({
                        "added": results,
                        "failed_files": failed_files,
                    }),
                    &format!(
                        "Added {} sources ({} failed)",
                        results.len(),
                        failed_files.len()
                    ),
                )?;
            }
            drop(store);
            crate::rsi::maybe_autospawn_dream_in_background(&cli.path, &actor);
        }
        MemoryCommand::Remember(args) => {
            let options = FunnelOptions {
                audience: audience(args.audience.as_deref(), &actor)?,
                actor,
                at,
            };
            let result = remember_memory(
                &mut store,
                Memory {
                    subject: args.subject.clone(),
                    predicate: args.predicate.clone(),
                    value: serde_json::from_str(&args.value)
                        .unwrap_or_else(|_| Value::String(args.value.clone())),
                    kind: args.kind,
                    logical_key: args.logical_key.clone(),
                    tags: csv(args.tags.as_deref().unwrap_or_default()),
                    entities: BTreeSet::new(),
                    valid: ValidityWindow {
                        valid_from: args.valid_from.map(EpochMs).unwrap_or(at),
                        valid_until: args.valid_until.map(EpochMs).unwrap_or(EpochMs::OPEN_END),
                    },
                },
                &options,
                &args.anchor,
            )?;
            output(
                cli.json,
                &result,
                &format!("Remembered {} facts", result.facts.len()),
            )?;
        }
        MemoryCommand::DeadEnd(args) => {
            let options = FunnelOptions {
                audience: audience(None, &actor)?,
                actor,
                at,
            };
            let result = remember_memory(
                &mut store,
                dead_end_memory(
                    args.subject.clone(),
                    args.hypothesis.clone(),
                    args.reason.clone(),
                    at,
                ),
                &options,
                &args.anchor,
            )?;
            output(
                cli.json,
                &result,
                &format!("Recorded {} dead ends", result.facts.len()),
            )?;
        }
        MemoryCommand::Forget {
            fact_id_or_logical_key,
            at: requested_at,
        } => {
            let Some(role) = store.member_role(&actor)? else {
                return Err(StoreError::PermissionDenied.into());
            };
            let forget_at = requested_at.map(EpochMs).unwrap_or(at);
            let normalized_target = store
                .resolve_visible_or_raw_fact_id(&actor, forget_at, fact_id_or_logical_key)
                .map(|id| id.to_string())
                .unwrap_or_else(|_| fact_id_or_logical_key.trim().to_owned());
            if !matches!(role, MemberRole::Owner | MemberRole::Maintainer) {
                let graph = store.derivation_view()?;
                let visible = graph.evaluate(&actor, forget_at);
                let lib_id = store.library_id();
                let targets: Vec<_> = graph
                    .facts
                    .values()
                    .filter(|f| {
                        f.id.as_str() == normalized_target
                            || (f.origin_library == *lib_id
                                && f.logical_key.as_str() == normalized_target)
                    })
                    .collect();
                if targets.is_empty() {
                    return Err(StoreError::NotFound.into());
                }
                if targets.iter().any(|f| f.author != actor) {
                    if targets.iter().any(|f| visible.facts.contains(&f.id)) {
                        return Err(StoreError::PermissionDenied.into());
                    }
                    return Err(StoreError::NotFound.into());
                }
            }
            let ids = store.forget(&normalized_target, &actor, forget_at)?;
            let _ = kp_derive::derive(&mut store);
            crate::community::notify_or_sync_joined(&cli.path, &mut store, forget_at).await;
            output(
                cli.json,
                &serde_json::json!({"withdrawn":ids}),
                &format!("Withdrew {} facts", ids.len()),
            )?;
        }
        MemoryCommand::Purge {
            logical_key_or_source_id,
            origin,
            reason,
        } => {
            let Some(role) = store.member_role(&actor)? else {
                return Err(StoreError::PermissionDenied.into());
            };
            let origin = origin.as_ref().unwrap_or(store.library_id()).clone();
            if !matches!(role, MemberRole::Owner | MemberRole::Maintainer) {
                let graph = store.derivation_view()?;
                let matching: Vec<_> = graph
                    .facts
                    .values()
                    .filter(|f| {
                        (f.origin_library == origin
                            && f.logical_key.as_str() == logical_key_or_source_id)
                            || f.source_anchor
                                .as_ref()
                                .and_then(|a| a.source_id.as_ref())
                                .is_some_and(|sid| sid.as_str() == logical_key_or_source_id)
                    })
                    .collect();
                if matching.is_empty() || matching.iter().any(|f| f.author != actor) {
                    return Err(StoreError::PermissionDenied.into());
                }
            }
            store.purge_target(&origin, logical_key_or_source_id, at, reason)?;
            let _ = kp_derive::derive(&mut store);
            crate::community::notify_or_sync_joined(&cli.path, &mut store, at).await;
            output(cli.json, &serde_json::json!({"purged":true}), "Purged")?;
        }
        MemoryCommand::Search(_) => unreachable!("handled via read-only path above"),
    }
    Ok(())
}

pub(crate) fn audience(value: Option<&str>, actor: &kp_model::ActorId) -> Result<AudienceSpec> {
    match value {
        Some("*") => Ok(AudienceSpec::AllMembers),
        Some(value) => {
            let actors = csv(value);
            if actors.is_empty() {
                return Err(KpError::invalid("audience", "requires at least one actor").into());
            }
            Ok(AudienceSpec::Explicit(
                actors
                    .into_iter()
                    .map(|a| a.parse())
                    .collect::<kp_model::Result<_>>()?,
            ))
        }
        None => Ok(AudienceSpec::Explicit(BTreeSet::from([actor.clone()]))),
    }
}

fn csv(value: &str) -> BTreeSet<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn collect_files(path: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_symlink() {
        return Ok(());
    }
    if metadata.is_file() {
        paths.push(path.to_owned());
    } else if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let path = entry.path();
            if path.is_dir() || kp_ingest::doc_router::handles(&path) {
                collect_files(&path, paths)?;
            }
        }
    }
    Ok(())
}

fn merge_ingest_report(dst: &mut PublicIngestStreamReport, batch: PublicIngestStreamReport) {
    dst.rosters_applied = dst.rosters_applied.saturating_add(batch.rosters_applied);
    dst.records_upserted = dst.records_upserted.saturating_add(batch.records_upserted);
    dst.records_duplicate = dst
        .records_duplicate
        .saturating_add(batch.records_duplicate);
    dst.records_superseded = dst
        .records_superseded
        .saturating_add(batch.records_superseded);
    dst.records_withdrawn = dst
        .records_withdrawn
        .saturating_add(batch.records_withdrawn);
    dst.jobs_enqueued = dst.jobs_enqueued.saturating_add(batch.jobs_enqueued);
    for sid in batch.source_ids {
        if !dst.source_ids.contains(&sid) {
            dst.source_ids.push(sid);
        }
    }
}

fn stream_public_jsonl_reader<R: BufRead>(
    mut reader: R,
    store: &mut Store,
    actor: &ActorId,
    global_policy: &ModelRoutePolicy,
) -> Result<PublicIngestStreamReport> {
    const MAX_BATCH_LINES: usize = 128;
    const MAX_BATCH_BYTES: usize = 512 * 1024;

    let mut total = PublicIngestStreamReport::default();
    let mut batch_buf = String::new();
    let mut batch_lines = 0usize;
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        batch_buf.push_str(&line);
        if !batch_buf.ends_with('\n') {
            batch_buf.push('\n');
        }
        batch_lines += 1;

        if batch_lines >= MAX_BATCH_LINES || batch_buf.len() >= MAX_BATCH_BYTES {
            let part = store.apply_public_ingest_jsonl(&batch_buf, actor, global_policy)?;
            merge_ingest_report(&mut total, part);
            batch_buf.clear();
            batch_lines = 0;
        }
    }

    if !batch_buf.is_empty() {
        let part = store.apply_public_ingest_jsonl(&batch_buf, actor, global_policy)?;
        merge_ingest_report(&mut total, part);
    }

    Ok(total)
}
