use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use clap::{Args, Subcommand, ValueEnum};
use kp_derive::{
    align_concepts_and_derive, build_command_guard_backfill_prompt,
    build_extraction_batch_request_for_policy, build_named_endpoint_llm_client,
    command_guard_backfill_json_schema, detect_global_generative_model,
    parse_command_guard_backfill_reply, reconcile_contradicted_events,
    resolve_ambiguous_scope_review, resolve_named_endpoint_config, stage_extracted_atoms,
    CollisionDeriveOutcome, DetectedGenerativeModel, HostCliConfig, HostCliKind, HostCliRunner,
    HttpApiLlmClient, ModelDetectEnv, NlFunnelOptions, PolicyRoutedLlmClient, RawOnlyLlmClient,
    ScopeReviewDecision,
};
use kp_model::{
    ActorId, AnchorSpec, AudienceSpec, BitemporalStamp, CalendarTable, ContentHash, EpochMs,
    ExtractionBatchOutcome, Fact, FactAccess, FactId, FactKind, FactStatus, GovernedScope, KpError,
    LlmClient, LogicalKey, MemberRole, MembershipRelation, ModelRoutePolicy, ReviewId, ReviewKind,
    ReviewStatus, SecretRedactor, SourceOriginMeta,
};
use kp_store::{
    ChunkInput, ExtractionJobInput, RawSourceInput, ScopeReviewFactUpdates, Store, StoreError,
};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{now, output, Cli};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const NL_SCHEMA_VER: &str = "m1-nl-v1";
const ALLOW_ONCE_TTL_MS: i64 = 10 * 60 * 1_000;
const EXTRACTION_LEASE_DURATION_MS: i64 = 240_000;
const EXTRACTION_MAX_ATTEMPTS: u32 = 5;

fn is_quota_error_message(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("quota")
        || lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("429")
        || lower.contains("too many requests")
}

#[derive(Subcommand)]
pub enum GovernanceCommand {
    /// Drain queued session/note batches or extract natural-language claims via hardened host CLI (`claude`/`codex`).
    Extract(ExtractArgs),
    /// Issue single-use 10-minute owner approvals for one or more guard/constraint `<fact_id>` or `kp-ref:<fact_id>` refs.
    Allow {
        #[arg(required = true, num_args = 1..)]
        fact_ids_or_refs: Vec<String>,
        #[arg(long)]
        at: Option<i64>,
    },
    /// Mark a visible fact as wrong ("这条错了"), withdrawing it and writing a sticky dual-key rejection record.
    Reject {
        fact_id: String,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        at: Option<i64>,
    },
    /// Inspect and resolve non-intrusive governance review items (`ambiguous_scope`, `concept_alignment`, `writeback_proposal`, etc.).
    Review {
        #[command(subcommand)]
        command: ReviewCommand,
    },
    /// Bind confirmed external identities (`<provider>:<external_id>`) to library members.
    Identity {
        #[command(subcommand)]
        command: IdentityCommand,
    },
}

#[derive(Subcommand)]
pub enum IdentityCommand {
    /// Bind `<provider>:<external_id>` to an active library `<actor>` (`Owner` or `Maintainer` required).
    Bind {
        external: String,
        actor: ActorId,
        #[arg(long = "display-name")]
        display_name: Option<String>,
        #[arg(long)]
        at: Option<i64>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum RunnerKind {
    Auto,
    ClaudeCode,
    Codex,
}

#[derive(Args)]
pub struct ExtractArgs {
    #[arg(long, value_enum, default_value = "auto")]
    pub runner: RunnerKind,
    #[arg(long)]
    pub program: Option<PathBuf>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long, default_value = "auto")]
    pub policy: String,
    #[arg(long)]
    pub remaining_quota_pct: Option<u8>,
    #[arg(long, default_value_t = 10)]
    pub min_quota_pct: u8,
    #[arg(long, default_value_t = 16)]
    pub max_jobs: usize,
    #[arg(long)]
    pub text: Option<String>,
    #[arg(long)]
    pub file: Option<PathBuf>,
    #[arg(long)]
    pub audience: Option<String>,
    #[arg(long)]
    pub force: bool,
    #[arg(long)]
    pub at: Option<i64>,
}

#[derive(Subcommand)]
pub enum ReviewCommand {
    /// List open (or `--all`) governance review items visible to the current actor.
    List {
        #[arg(long)]
        all: bool,
    },
    /// Resolve a governance review item (`confirm`, `narrow`, or `dismiss`).
    Resolve {
        review_id: ReviewId,
        #[arg(long, value_enum, required_unless_present = "positional_choice")]
        choice: Option<ReviewChoice>,
        #[arg(value_enum, conflicts_with = "choice")]
        positional_choice: Option<ReviewChoice>,
        #[arg(long)]
        at: Option<i64>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ReviewChoice {
    Confirm,
    Narrow,
    Dismiss,
}

impl ReviewChoice {
    fn as_str(self) -> &'static str {
        match self {
            Self::Confirm => "confirm",
            Self::Narrow => "narrow",
            Self::Dismiss => "dismiss",
        }
    }
}

pub async fn run(cli: &Cli, command: &GovernanceCommand) -> Result<()> {
    if let GovernanceCommand::Review {
        command: ReviewCommand::List { all },
    } = command
    {
        let store = Store::open_readonly(&cli.path)?;
        let actor = cli
            .actor
            .clone()
            .unwrap_or_else(|| store.owner_actor().clone());
        if store.member_role(&actor)?.is_none() {
            return Err(StoreError::PermissionDenied.into());
        }
        let filter = if *all { None } else { Some(ReviewStatus::Open) };
        let items = store.list_review_items(&actor, filter)?;
        let human = if items.is_empty() {
            "No open review items".to_owned()
        } else {
            items
                .iter()
                .map(|item| {
                    format!(
                        "[{}] ({} · {}) {} -> {}",
                        item.id, item.kind, item.status, item.target_id, item.question
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        output(cli.json, &items, &human)?;
        return Ok(());
    }

    let mut store = Store::open(&cli.path)?;
    let actor = cli
        .actor
        .clone()
        .unwrap_or_else(|| store.owner_actor().clone());
    let Some(role) = store.member_role(&actor)? else {
        return Err(StoreError::PermissionDenied.into());
    };
    let clock = now()?;
    match command {
        GovernanceCommand::Extract(args) => {
            let at = args.at.map(EpochMs).unwrap_or(clock);
            let summary = run_extract(&mut store, &actor, args, at).await?;
            let human = format!(
                "Processed {} jobs ({} facts created, {} reviews queued)",
                summary["jobs_processed"].as_u64().unwrap_or(0),
                summary["created_facts"]
                    .as_array()
                    .map_or(0, std::vec::Vec::len),
                summary["emitted_reviews"]
                    .as_array()
                    .map_or(0, std::vec::Vec::len)
            );
            output(cli.json, &summary, &human)?;
        }
        GovernanceCommand::Allow {
            fact_ids_or_refs,
            at,
        } => {
            let at = at.map(EpochMs).unwrap_or(clock);
            let value = allow_guard_once(&mut store, &actor, role, fact_ids_or_refs, at)?;
            let human = if let Some(kp_ref) = value.get("kp_ref").and_then(Value::as_str) {
                format!(
                    "Allowed {kp_ref} once (valid for 10 minutes until {})",
                    value["expires_at"].as_i64().unwrap_or(0)
                )
            } else {
                let count = value["count"].as_u64().unwrap_or(0);
                format!("Allowed {count} guard(s) once (valid for 10 minutes)")
            };
            output(cli.json, &value, &human)?;
        }
        GovernanceCommand::Reject {
            fact_id,
            reason,
            at,
        } => {
            let at = at.map(EpochMs).unwrap_or(clock);
            let value = reject_fact(&mut store, &actor, role, fact_id, reason, at)?;
            crate::community::notify_or_sync_joined(&cli.path, &mut store, at).await;
            let human = format!(
                "Rejected {} ({})",
                value["fact_id"].as_str().unwrap_or(fact_id),
                value["rejection_id"].as_str().unwrap_or("")
            );
            output(cli.json, &value, &human)?;
        }
        GovernanceCommand::Review {
            command: ReviewCommand::List { .. },
        } => unreachable!("handled via read-only path above"),
        GovernanceCommand::Review {
            command:
                ReviewCommand::Resolve {
                    review_id,
                    choice,
                    positional_choice,
                    at,
                },
        } => {
            let resolved_choice = choice.or(*positional_choice).ok_or_else(|| {
                KpError::invalid(
                    "choice",
                    "review resolution choice (`confirm`, `narrow`, or `dismiss`) is required",
                )
            })?;
            let at = at.map(EpochMs).unwrap_or(clock);
            let value = resolve_review(&mut store, &actor, review_id, resolved_choice, at).await?;
            crate::community::notify_or_sync_joined(&cli.path, &mut store, at).await;
            let human = format!(
                "Resolved {} with choice '{}'",
                review_id,
                resolved_choice.as_str()
            );
            output(cli.json, &value, &human)?;
        }
        GovernanceCommand::Identity {
            command:
                IdentityCommand::Bind {
                    external,
                    actor: target_actor,
                    display_name,
                    at,
                },
        } => {
            let Some((provider, ext_id)) = external.trim().split_once(':') else {
                return Err(KpError::invalid(
                    "external",
                    "expected `<provider>:<external_id>` (e.g. `feishu:ou_xxx`)",
                )
                .into());
            };
            let clean_provider = provider.trim();
            let clean_ext = ext_id.trim();
            let at = at.map(EpochMs).unwrap_or(clock);
            store.bind_external_identity(
                &actor,
                clean_provider,
                clean_ext,
                target_actor,
                display_name.as_deref(),
                at,
            )?;
            let value = json!({
                "provider": clean_provider,
                "external_id": clean_ext,
                "confirmed_actor": target_actor,
                "display_name": display_name.as_deref().unwrap_or(clean_ext),
            });
            let human = format!("Bound {clean_provider}:{clean_ext} -> {target_actor}");
            output(cli.json, &value, &human)?;
        }
    }
    Ok(())
}

fn parse_cli_route_policy(policy: &str) -> kp_model::Result<ModelRoutePolicy> {
    let trimmed = policy.trim();
    if let Some(ep) = trimmed.strip_prefix("named_endpoint:") {
        let clean = ep.trim();
        if clean.is_empty() {
            return Err(KpError::invalid(
                "model_route_policy",
                "named endpoint identifier must be nonempty",
            ));
        }
        return Ok(ModelRoutePolicy::NamedEndpoint(clean.to_owned()));
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "" | "auto" | "inherit" | "host" | "host_cli" => Ok(ModelRoutePolicy::HostCli),
        "raw_only" | "raw-only" | "none" => Ok(ModelRoutePolicy::RawOnly),
        _ => Ok(ModelRoutePolicy::NamedEndpoint(trimmed.to_owned())),
    }
}

fn discover_cli_binary(name: &str) -> Option<PathBuf> {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let cand = dir.join(name);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let nvm_dir = Path::new(&home).join(".nvm/versions/node");
        if let Ok(entries) = std::fs::read_dir(nvm_dir) {
            let mut versions: Vec<PathBuf> =
                entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
            versions.sort();
            versions.reverse();
            for ver in versions {
                let cand = ver.join("bin").join(name);
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
        let local_bin = Path::new(&home).join(".local/bin").join(name);
        if local_bin.is_file() {
            return Some(local_bin);
        }
    }
    for prefix in ["/opt/homebrew/bin", "/usr/local/bin"] {
        let cand = Path::new(prefix).join(name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

fn resolve_host_cli_config(
    runner: RunnerKind,
    program_override: Option<&Path>,
    model: Option<String>,
    route_policy: ModelRoutePolicy,
    remaining_quota_pct: Option<u8>,
) -> HostCliConfig {
    let env_kind = std::env::var("KP_HOST_CLI_KIND").ok();
    let env_prog = std::env::var_os("KP_HOST_CLI_PROGRAM").map(PathBuf::from);
    let explicit_prog = program_override.map(Path::to_path_buf).or(env_prog);

    let kind = match runner {
        RunnerKind::ClaudeCode => HostCliKind::ClaudeCode,
        RunnerKind::Codex => HostCliKind::Codex,
        RunnerKind::Auto => match env_kind.as_deref() {
            Some("claude" | "claude-code" | "claude_code") => HostCliKind::ClaudeCode,
            Some("codex") => HostCliKind::Codex,
            _ => {
                if let Some(prog) = &explicit_prog {
                    let fname = prog
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_lowercase();
                    if fname.contains("claude") {
                        HostCliKind::ClaudeCode
                    } else {
                        HostCliKind::Codex
                    }
                } else if discover_cli_binary("codex").is_some() {
                    HostCliKind::Codex
                } else {
                    HostCliKind::ClaudeCode
                }
            }
        },
    };

    let program = explicit_prog.unwrap_or_else(|| match kind {
        HostCliKind::Codex => {
            discover_cli_binary("codex").unwrap_or_else(|| PathBuf::from("codex"))
        }
        HostCliKind::ClaudeCode => {
            discover_cli_binary("claude").unwrap_or_else(|| PathBuf::from("claude"))
        }
    });

    HostCliConfig {
        kind,
        program,
        model,
        route_policy,
        remaining_quota_pct,
        ..HostCliConfig::for_kind(kind)
    }
}

pub(crate) fn detect_global_model() -> DetectedGenerativeModel {
    if (cfg!(test) || crate::providers_cli::is_kp_offline())
        && std::env::var_os("KP_HOST_CLI_PROGRAM").is_none()
        && std::env::var_os("KP_MODEL_POLICY").is_none()
        && std::env::var_os("KP_GENERATIVE_MODEL").is_none()
    {
        return DetectedGenerativeModel::RawOnly;
    }
    detect_global_generative_model(&ModelDetectEnv::from_process())
}

pub(crate) fn default_auto_route_policy() -> ModelRoutePolicy {
    detect_global_model().route_policy()
}

/// Resolves the default `ModelRoutePolicy` for Markdown writeback (`kp apply` and
/// `kp review resolve` on `WritebackProposal`).
///
/// Unlike [`default_auto_route_policy`] (which falls back to `RawOnly` under `KP_OFFLINE=1` or
/// when no generative CLI/API key is installed so `kp ingest` skips extraction queueing), writeback
/// returns `RawOnly` **only** when global `raw_only` / `raw-only` / `none` was explicitly configured
/// or when an explicitly configured gateway/policy failed closed, preserving `HostCli` and
/// `NamedEndpoint` policies on edited facts.
pub(crate) fn explicit_writeback_default_policy() -> ModelRoutePolicy {
    let is_raw_directive = |raw: &str| {
        matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "raw_only" | "raw-only" | "none"
        )
    };
    if std::env::var("KP_MODEL_POLICY")
        .ok()
        .is_some_and(|v| is_raw_directive(&v))
        || std::env::var("KP_GENERATIVE_MODEL")
            .ok()
            .is_some_and(|v| is_raw_directive(&v))
    {
        return ModelRoutePolicy::RawOnly;
    }
    let cfg_path = std::env::var("KP_CONFIG_PATH")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(|h| Path::new(&h).join(".kp").join("config.json"))
        });
    if let Some(file_cfg) = cfg_path.and_then(|p| kp_derive::load_secure_kp_config(&p)) {
        if file_cfg
            .model_policy
            .as_deref()
            .is_some_and(is_raw_directive)
            || file_cfg
                .generative_model
                .as_deref()
                .is_some_and(is_raw_directive)
        {
            return ModelRoutePolicy::RawOnly;
        }
    }
    let detect_env = ModelDetectEnv::from_process();
    match detect_global_generative_model(&detect_env) {
        DetectedGenerativeModel::HttpApi(cfg) => cfg.route_policy,
        DetectedGenerativeModel::HostCli(_) | DetectedGenerativeModel::RawOnly => {
            ModelRoutePolicy::HostCli
        }
    }
}

fn build_global_host_llm_from_detected(
    detected: &DetectedGenerativeModel,
    args: &ExtractArgs,
) -> kp_model::Result<Arc<dyn LlmClient>> {
    match detected {
        DetectedGenerativeModel::HostCli(_) | DetectedGenerativeModel::HttpApi(_) => detected
            .build_llm_client_with_overrides(args.model.as_deref(), args.remaining_quota_pct),
        DetectedGenerativeModel::RawOnly => {
            if matches!(
                args.policy.trim().to_ascii_lowercase().as_str(),
                "host" | "host_cli"
            ) {
                let host_cfg = resolve_host_cli_config(
                    RunnerKind::Auto,
                    None,
                    args.model.clone(),
                    ModelRoutePolicy::HostCli,
                    args.remaining_quota_pct,
                );
                let host_runner = HostCliRunner::new(host_cfg)?;
                let client =
                    PolicyRoutedLlmClient::new(ModelRoutePolicy::HostCli, Arc::new(host_runner));
                return Ok(Arc::new(client));
            }
            Ok(Arc::new(RawOnlyLlmClient))
        }
    }
}

fn build_named_endpoint_client_for_args(
    endpoint_name: &str,
    env: &ModelDetectEnv,
    args: &ExtractArgs,
) -> kp_model::Result<Arc<dyn LlmClient>> {
    if args.model.is_none() && args.remaining_quota_pct.is_none() {
        return build_named_endpoint_llm_client(endpoint_name, env);
    }
    let mut cfg = resolve_named_endpoint_config(endpoint_name, env)?;
    if let Some(ref m) = args.model {
        cfg = cfg.with_model(m.clone());
    }
    cfg.remaining_quota_pct = args.remaining_quota_pct;
    Ok(Arc::new(HttpApiLlmClient::new(cfg)?))
}

/// Drains queued extraction jobs for `kp ingest --extract` using the global auto-detected model.
pub(crate) async fn drain_for_public_ingest(
    store: &mut Store,
    actor: &ActorId,
    at: EpochMs,
) -> Result<Value> {
    let args = ExtractArgs {
        runner: RunnerKind::Auto,
        program: None,
        model: None,
        policy: "auto".to_owned(),
        remaining_quota_pct: None,
        min_quota_pct: 10,
        max_jobs: 64,
        text: None,
        file: None,
        audience: None,
        force: false,
        at: None,
    };
    run_extract_inner(store, actor, &args, at, false, None).await
}

pub(crate) async fn backfill_legacy_prose_command_guards(
    store: &mut Store,
    llm_override: Option<&Arc<dyn LlmClient>>,
    at: EpochMs,
) -> Result<usize> {
    let candidates = store.list_untagged_guard_candidates()?;
    if candidates.is_empty() {
        return Ok(0);
    }
    let redactor = SecretRedactor::new()?;
    let schema = command_guard_backfill_json_schema();
    let proc_env = ModelDetectEnv::from_process();
    let mut host_client_cache: Option<Arc<dyn LlmClient>> = None;
    let mut named_cache: BTreeMap<String, Arc<dyn LlmClient>> = BTreeMap::new();
    let mut upgraded = 0usize;

    for cand in candidates {
        if cand.route_policy == ModelRoutePolicy::RawOnly {
            continue;
        }
        let client: Arc<dyn LlmClient> = if let Some(ov) = llm_override {
            if ov.is_raw_only() {
                continue;
            }
            if matches!(&cand.route_policy, ModelRoutePolicy::NamedEndpoint(_))
                && ov.route_policy() != cand.route_policy
            {
                continue;
            }
            Arc::clone(ov)
        } else {
            match &cand.route_policy {
                ModelRoutePolicy::RawOnly => continue,
                ModelRoutePolicy::HostCli => {
                    if let Some(ref c) = host_client_cache {
                        Arc::clone(c)
                    } else {
                        let detected = detect_global_model();
                        if matches!(detected, DetectedGenerativeModel::RawOnly)
                            || detected.route_policy() == ModelRoutePolicy::RawOnly
                        {
                            continue;
                        }
                        let Ok(built) = detected.build_llm_client() else {
                            continue;
                        };
                        host_client_cache = Some(Arc::clone(&built));
                        built
                    }
                }
                ModelRoutePolicy::NamedEndpoint(ep) => {
                    if let Some(c) = named_cache.get(ep) {
                        Arc::clone(c)
                    } else {
                        let Ok(built) = build_named_endpoint_llm_client(ep, &proc_env) else {
                            continue;
                        };
                        named_cache.insert(ep.clone(), Arc::clone(&built));
                        built
                    }
                }
            }
        };

        let prompt = build_command_guard_backfill_prompt(
            &cand.subject,
            &cand.claim_or_summary,
            cand.verbatim_quote.as_deref(),
            &redactor,
        );
        let Ok(Some(raw_reply)) = client
            .complete_structured_json("kp_command_guard_backfill", &schema, &prompt)
            .await
        else {
            continue;
        };
        let Ok(parsed_spec) = parse_command_guard_backfill_reply(raw_reply) else {
            continue;
        };
        if store.apply_command_guard_backfill(
            &cand.fact_id,
            &cand.content_sha256,
            parsed_spec.as_ref(),
            at,
        )? {
            upgraded += 1;
        }
    }
    Ok(upgraded)
}

pub(crate) async fn drain_for_dream(
    store: &mut Store,
    actor: &ActorId,
    at: EpochMs,
    llm_override: Option<Arc<dyn LlmClient>>,
) -> Result<Value> {
    if store.pending_extraction_job_count()? == 0 {
        let backfilled =
            backfill_legacy_prose_command_guards(store, llm_override.as_ref(), at).await?;
        let untagged = store.untagged_guard_candidate_count()?;
        return Ok(json!({
            "jobs_processed": 0,
            "jobs_paused_quota": 0,
            "created_facts": [],
            "superseded_facts": [],
            "dropped_rejected": [],
            "derived_warnings": 0,
            "superseded_contradicted_events": [],
            "emitted_reviews": [],
            "guards_backfilled": backfilled,
            "untagged_guard_candidates": untagged,
        }));
    }
    let args = ExtractArgs {
        runner: RunnerKind::Auto,
        program: None,
        model: None,
        policy: "auto".to_owned(),
        remaining_quota_pct: None,
        min_quota_pct: 10,
        max_jobs: 64,
        text: None,
        file: None,
        audience: None,
        force: false,
        at: Some(at.0),
    };
    let mut out = run_extract_inner(store, actor, &args, at, false, llm_override.clone()).await?;
    let backfilled = backfill_legacy_prose_command_guards(store, llm_override.as_ref(), at).await?;
    let untagged = store.untagged_guard_candidate_count()?;
    if let Value::Object(ref mut map) = out {
        map.insert("guards_backfilled".into(), json!(backfilled));
        map.insert("untagged_guard_candidates".into(), json!(untagged));
    }
    Ok(out)
}

/// Drains queued background extraction jobs inside `kp daemon`, routing per `job.model_policy` and
/// reusing/refreshing the daemon's resident [`crate::providers_cli::SwappableEmbedder`].
/// Returns `Ok(true)` if extraction paused due to low quota headroom so the scheduler applies cooldown.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) async fn drain_pending_extractions_for_daemon_with_embedder(
    library_dir: &Path,
    at: EpochMs,
    daemon_slot: Arc<RwLock<Option<Arc<crate::providers_cli::SwappableEmbedder>>>>,
) -> std::result::Result<bool, String> {
    let pending = Store::open_readonly(library_dir)
        .and_then(|ro| ro.daemon_queue_stats().map(|(_, p)| p))
        .map_err(|e| e.to_string())?;
    if pending == 0 {
        return Ok(false);
    }
    let mut store = Store::open(library_dir).map_err(|e| e.to_string())?;
    crate::providers_cli::attach_or_refresh_daemon_embedder(&mut store, library_dir, &daemon_slot);
    let actor = store.owner_actor().clone();
    let args = ExtractArgs {
        runner: RunnerKind::Auto,
        program: None,
        model: None,
        policy: "auto".to_owned(),
        remaining_quota_pct: None,
        min_quota_pct: 10,
        max_jobs: 16,
        text: None,
        file: None,
        audience: None,
        force: false,
        at: None,
    };
    let out = run_extract_inner(&mut store, &actor, &args, at, true, None)
        .await
        .map_err(|e| e.to_string())?;
    let paused = out
        .get("jobs_paused_quota")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        > 0;
    Ok(paused)
}

fn is_internal_alignment_or_supersede_fact(fact: &Fact) -> bool {
    matches!(
        fact.predicate.as_str(),
        "kp.concept.alignment" | "kp.event.superseded"
    )
}

fn collect_active_base_route_policies(store: &Store) -> Result<(bool, BTreeSet<String>)> {
    let graph = store.derivation_view()?;
    let mut has_host_cli = false;
    let mut named_endpoints = BTreeSet::new();
    for fact in graph.facts.values() {
        if fact.status != FactStatus::Active
            || !matches!(fact.access, FactAccess::Base { .. })
            || is_internal_alignment_or_supersede_fact(fact)
        {
            continue;
        }
        match store.fact_effective_route_policy(fact)? {
            ModelRoutePolicy::HostCli => {
                has_host_cli = true;
            }
            ModelRoutePolicy::NamedEndpoint(ep) => {
                let clean = ep.trim();
                if !clean.is_empty() {
                    named_endpoints.insert(clean.to_owned());
                }
            }
            ModelRoutePolicy::RawOnly => {}
        }
    }
    Ok((has_host_cli, named_endpoints))
}

/// Runs open-concept alignment, contradicted-event supersession, deterministic Datalog derivation,
/// and cross-person `AmbiguousScope` review emission, persisting valid `ReviewItem`s into `store`.
pub(crate) async fn run_concept_alignment_and_derive(
    store: &mut Store,
    llm_override: Option<&dyn LlmClient>,
    at: EpochMs,
) -> Result<CollisionDeriveOutcome> {
    let empty_named = BTreeMap::new();
    let (out, _) =
        run_concept_alignment_and_derive_with_named(store, llm_override, &empty_named, at).await?;
    Ok(out)
}

pub(crate) async fn run_concept_alignment_and_derive_with_named(
    store: &mut Store,
    llm_override: Option<&dyn LlmClient>,
    named_cache: &BTreeMap<String, Arc<dyn LlmClient>>,
    at: EpochMs,
) -> Result<(CollisionDeriveOutcome, Option<String>)> {
    let mut final_warnings = 0_usize;
    let mut final_justifications = 0_usize;
    let mut all_superseded = Vec::new();
    let mut persisted_reviews = Vec::new();
    let mut first_named_align_error: Option<String> = None;
    let proc_env = ModelDetectEnv::from_process();

    for pass in 0..2 {
        {
            let pre_g = store.derivation_view()?;
            let pre_tbl = kp_derive::ConceptAlignmentTable::from_graph(&pre_g);
            let _ = store.reconcile_relation_group_audiences(|l, r| pre_tbl.entails(l, r), at)?;
        }

        let started_gen = store.alignment_dirty_generation()?;
        let concepts = store.list_active_concepts()?;
        let open_reviews = store.list_all_open_review_items()?;

        let (mut outcome, alignment_evaluated, any_llm_failed) = if concepts.is_empty() {
            let out =
                align_concepts_and_derive(store, &RawOnlyLlmClient, &concepts, &open_reviews, at)
                    .await?;
            (out, true, false)
        } else {
            let (has_host_cli, named_endpoints) = collect_active_base_route_policies(store)?;
            let mut pass_superseded_ids = Vec::new();
            let mut pass_reviews = Vec::new();
            let mut last_derived = 0_usize;
            let mut last_justs = 0_usize;
            let mut all_evaluated = true;
            let mut llm_failed = false;
            let mut ran_any_pass = false;

            if let Some(llm) = llm_override {
                let out =
                    align_concepts_and_derive(store, llm, &concepts, &open_reviews, at).await?;
                ran_any_pass = true;
                last_derived = out.derived_warnings;
                last_justs = out.justifications;
                pass_superseded_ids.extend(out.superseded_contradicted_events);
                pass_reviews.extend(out.emitted_reviews);
                if llm.is_raw_only() {
                    all_evaluated = false;
                }
            } else if has_host_cli {
                let raw_proc_detected = detect_global_generative_model(&proc_env);
                let detected = detect_global_model();
                let policy = detected.route_policy();
                if raw_proc_detected.is_invalid_gateway() {
                    all_evaluated = false;
                    llm_failed = true;
                } else if policy == ModelRoutePolicy::RawOnly {
                    all_evaluated = false;
                } else {
                    match detected.build_llm_client() {
                        Ok(client) => {
                            match align_concepts_and_derive(
                                store,
                                client.as_ref(),
                                &concepts,
                                &open_reviews,
                                at,
                            )
                            .await
                            {
                                Ok(out) => {
                                    ran_any_pass = true;
                                    last_derived = out.derived_warnings;
                                    last_justs = out.justifications;
                                    pass_superseded_ids.extend(out.superseded_contradicted_events);
                                    pass_reviews.extend(out.emitted_reviews);
                                    if client.is_raw_only() {
                                        all_evaluated = false;
                                    }
                                }
                                Err(_) => {
                                    all_evaluated = false;
                                    llm_failed = true;
                                }
                            }
                        }
                        Err(_) => {
                            all_evaluated = false;
                            llm_failed = true;
                        }
                    }
                }
            }

            for ep in &named_endpoints {
                if llm_override.is_some_and(|l| {
                    l.route_policy() == ModelRoutePolicy::NamedEndpoint(ep.clone())
                }) {
                    continue;
                }
                let ep_client_res = match named_cache.get(ep) {
                    Some(cached) => Ok(Arc::clone(cached)),
                    None => build_named_endpoint_llm_client(ep, &proc_env),
                };
                match ep_client_res {
                    Ok(ep_client) => {
                        let combined_open: Vec<_> = open_reviews
                            .iter()
                            .chain(pass_reviews.iter())
                            .cloned()
                            .collect();
                        match align_concepts_and_derive(
                            store,
                            ep_client.as_ref(),
                            &concepts,
                            &combined_open,
                            at,
                        )
                        .await
                        {
                            Ok(out) => {
                                ran_any_pass = true;
                                last_derived = out.derived_warnings;
                                last_justs = out.justifications;
                                pass_superseded_ids.extend(out.superseded_contradicted_events);
                                pass_reviews.extend(out.emitted_reviews);
                            }
                            Err(err) => {
                                all_evaluated = false;
                                llm_failed = true;
                                if first_named_align_error.is_none() {
                                    first_named_align_error = Some(err.to_string());
                                }
                            }
                        }
                    }
                    Err(err) => {
                        all_evaluated = false;
                        llm_failed = true;
                        if first_named_align_error.is_none() {
                            first_named_align_error = Some(err.to_string());
                        }
                    }
                }
            }

            if !ran_any_pass || llm_failed {
                let combined_open: Vec<_> = open_reviews
                    .iter()
                    .chain(pass_reviews.iter())
                    .cloned()
                    .collect();
                let raw_out = align_concepts_and_derive(
                    store,
                    &RawOnlyLlmClient,
                    &concepts,
                    &combined_open,
                    at,
                )
                .await?;
                last_derived = raw_out.derived_warnings;
                last_justs = raw_out.justifications;
                pass_superseded_ids.extend(raw_out.superseded_contradicted_events);
                pass_reviews.extend(raw_out.emitted_reviews);
            }

            (
                CollisionDeriveOutcome {
                    derived_warnings: last_derived,
                    justifications: last_justs,
                    superseded_contradicted_events: pass_superseded_ids,
                    emitted_reviews: pass_reviews,
                },
                all_evaluated,
                llm_failed,
            )
        };

        let g = store.derivation_view()?;
        let tbl = kp_derive::ConceptAlignmentTable::from_graph(&g);
        let reconciled = store.reconcile_relation_group_audiences(|l, r| tbl.entails(l, r), at)?;
        if reconciled > 0 {
            let res = kp_derive::derive(store)?;
            outcome.derived_warnings = res.derived;
            outcome.justifications = res.justifications;
        }

        final_warnings = outcome.derived_warnings;
        final_justifications = outcome.justifications;
        let pass_superseded = !outcome.superseded_contradicted_events.is_empty();
        all_superseded.extend(outcome.superseded_contradicted_events);

        persisted_reviews.clear();
        for rev in &outcome.emitted_reviews {
            if let Some(stored) = store.upsert_open_review_item_if_valid(rev)? {
                persisted_reviews.push(stored);
            }
        }

        if any_llm_failed {
            store.set_needs_alignment(true, at)?;
            break;
        }
        if !alignment_evaluated {
            break;
        }
        if (pass_superseded || reconciled > 0) && pass == 0 {
            continue;
        }
        if !pass_superseded && reconciled == 0 {
            store.ack_alignment_generation(started_gen, at)?;
        }
        break;
    }

    Ok((
        CollisionDeriveOutcome {
            derived_warnings: final_warnings,
            justifications: final_justifications,
            superseded_contradicted_events: all_superseded,
            emitted_reviews: persisted_reviews,
        },
        first_named_align_error,
    ))
}

async fn run_extract(
    store: &mut Store,
    actor: &ActorId,
    args: &ExtractArgs,
    at: EpochMs,
) -> Result<Value> {
    run_extract_inner(store, actor, args, at, false, None).await
}

async fn run_extract_inner(
    store: &mut Store,
    actor: &ActorId,
    args: &ExtractArgs,
    at: EpochMs,
    is_daemon_background: bool,
    llm_override: Option<Arc<dyn LlmClient>>,
) -> Result<Value> {
    crate::providers_cli::attach_ready_embedder(store);

    let pol_trim = args.policy.trim();
    let pol_lower = pol_trim.to_ascii_lowercase();
    let has_explicit_cli_runner =
        args.program.is_some() || !matches!(args.runner, RunnerKind::Auto);
    let explicit_route_policy = parse_cli_route_policy(pol_trim)?;
    let explicit_host_llm: Option<Arc<dyn LlmClient>> = if let Some(injected) = llm_override {
        Some(injected)
    } else if has_explicit_cli_runner {
        let host_cfg = resolve_host_cli_config(
            args.runner,
            args.program.as_deref(),
            args.model.clone(),
            explicit_route_policy.clone(),
            args.remaining_quota_pct,
        );
        let host_runner = HostCliRunner::new(host_cfg)?;
        let client =
            PolicyRoutedLlmClient::new(explicit_route_policy.clone(), Arc::new(host_runner));
        Some(Arc::new(client))
    } else {
        None
    };

    let proc_env = ModelDetectEnv::from_process();
    let raw_proc_detected = detect_global_generative_model(&proc_env);
    let global_detected = detect_global_model();

    // Optional inline `--text` or `--file` ingestion + queue enqueue before draining.
    if args.text.is_some() || args.file.is_some() {
        let inline_route_policy = if explicit_route_policy == ModelRoutePolicy::RawOnly {
            ModelRoutePolicy::RawOnly
        } else if has_explicit_cli_runner {
            explicit_route_policy.clone()
        } else if matches!(pol_lower.as_str(), "" | "auto" | "inherit") {
            if raw_proc_detected.is_invalid_gateway() {
                let _ = raw_proc_detected.build_llm_client()?;
            }
            global_detected.route_policy()
        } else if matches!(pol_lower.as_str(), "host" | "host_cli") {
            if raw_proc_detected.is_invalid_gateway() {
                let _ = raw_proc_detected.build_llm_client()?;
            }
            ModelRoutePolicy::HostCli
        } else {
            explicit_route_policy.clone()
        };
        let raw_body = match (&args.text, &args.file) {
            (Some(t), None) => t.clone(),
            (None, Some(path)) => std::fs::read_to_string(path)?,
            (Some(_), Some(_)) => {
                return Err(
                    KpError::invalid("extract", "pass either --text or --file, not both").into(),
                )
            }
            (None, None) => String::new(),
        };
        let redactor = SecretRedactor::new()?;
        let clean = redactor.redact(raw_body.trim());
        if !clean.is_empty() {
            let chunk_body = if clean.contains("[speaker:") {
                format!("{clean}\n")
            } else {
                clean
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| format!("[speaker:{actor}] {l}\n"))
                    .collect::<String>()
            };
            let aud = crate::memory::audience(args.audience.as_deref(), actor)?;
            let tok = Uuid::now_v7().simple().to_string();
            let key: LogicalKey = format!("extract:{actor}:{tok}").parse()?;
            let uri = args
                .file
                .as_ref()
                .map_or_else(|| format!("extract:{tok}"), |p| p.display().to_string());
            let stored = store.ingest_source_with_policy(
                &store.library_id().clone(),
                &key,
                &RawSourceInput {
                    self_echo_filtered: false,
                    uri,
                    media_type: "text/markdown".into(),
                    content: chunk_body.as_bytes().to_vec(),
                    readable_by: aud,
                    captured_at: at,
                    chunks: vec![ChunkInput {
                        heading: "Extract Input".into(),
                        body: chunk_body.clone(),
                        anchor: AnchorSpec {
                            start_line: Some(1),
                            end_line: Some(
                                u32::try_from(chunk_body.lines().count().max(1)).unwrap_or(1),
                            ),
                            start_byte: Some(0),
                            end_byte: Some(u64::try_from(chunk_body.len()).unwrap_or(0)),
                            ..AnchorSpec::default()
                        },
                    }],
                },
                &inline_route_policy,
            )?;
            if inline_route_policy != ModelRoutePolicy::RawOnly {
                let queue_policy = match &inline_route_policy {
                    ModelRoutePolicy::HostCli => "host".to_owned(),
                    ModelRoutePolicy::NamedEndpoint(ep) => {
                        format!("named_endpoint_v2:{}", ep.trim())
                    }
                    ModelRoutePolicy::RawOnly => "raw_only".to_owned(),
                };
                store.enqueue_extraction_job(&ExtractionJobInput {
                    source_id: stored.source_id.clone(),
                    chunk_ids: vec![format!("{}-0", stored.source_id)],
                    chunk_batch_sha256: ContentHash::digest(&chunk_body),
                    author: actor.clone(),
                    model_policy: queue_policy,
                    priority: 10,
                    reference_time: at,
                    created_at: at,
                })?;
            }
        }
    }

    if args
        .remaining_quota_pct
        .is_none_or(|rem| rem >= args.min_quota_pct)
    {
        let _ = store.resume_paused_quota_jobs()?;
    }

    let mut jobs_processed = 0_usize;
    let mut jobs_paused_quota = 0_usize;
    let mut created_facts = Vec::new();
    let mut superseded_facts = Vec::new();
    let mut dropped_rejected = Vec::new();
    let mut cached_host_llm: Option<Arc<dyn LlmClient>> = None;
    let mut named_clients: BTreeMap<String, Arc<dyn LlmClient>> = BTreeMap::new();
    let mut first_policy_error: Option<String> = None;

    let started_inst = std::time::Instant::now();
    let fixed_at = args.at.is_some();
    let current_lease_clock = || -> EpochMs {
        let elapsed_ms = i64::try_from(started_inst.elapsed().as_millis()).unwrap_or(0);
        let fallback = EpochMs(at.0.saturating_add(elapsed_ms));
        if fixed_at {
            fallback
        } else {
            now().unwrap_or(fallback)
        }
    };

    let max_window_iters = args.max_jobs.saturating_mul(2).max(args.max_jobs);
    for _ in 0..max_window_iters {
        if jobs_processed >= args.max_jobs {
            break;
        }
        let lease_now = current_lease_clock();
        let Some(window) = store.lease_extraction_window(
            lease_now,
            EXTRACTION_LEASE_DURATION_MS,
            kp_model::DEFAULT_MAX_BATCH_TOKENS,
        )?
        else {
            break;
        };
        let Some(head_job) = window.jobs.first().cloned() else {
            break;
        };
        let job_specs: Vec<(String, u32)> = window
            .jobs
            .iter()
            .map(|j| (j.job_id.clone(), j.attempts))
            .collect();

        let decoded_policy = match kp_store::decode_queue_model_policy(&head_job.model_policy) {
            Ok(pol) => pol,
            Err(err) => {
                let err_str = err.to_string();
                let fail_now = current_lease_clock();
                let _ =
                    store.release_extraction_window_for_retry(&job_specs, &err_str, fail_now, 1);
                if first_policy_error.is_none() {
                    first_policy_error = Some(err_str);
                }
                continue;
            }
        };
        let mut any_raw_only = explicit_route_policy == ModelRoutePolicy::RawOnly;
        let mut job_policy = decoded_policy;
        for j in &window.jobs {
            let eff = store.source_effective_route_policy(&j.source_id)?;
            if eff == ModelRoutePolicy::RawOnly {
                any_raw_only = true;
            } else if eff != job_policy && store.has_public_source_meta(&j.source_id)? {
                job_policy = eff;
            }
        }

        if any_raw_only || job_policy == ModelRoutePolicy::RawOnly {
            match store.complete_extraction_window_fenced(&job_specs, &[], NL_SCHEMA_VER, at) {
                Ok(()) => {
                    jobs_processed += job_specs.len();
                }
                Err(StoreError::Conflict) => {}
                Err(err) => return Err(err.into()),
            }
            continue;
        }

        let job_llm: Arc<dyn LlmClient> = match &job_policy {
            ModelRoutePolicy::RawOnly => Arc::new(RawOnlyLlmClient),
            ModelRoutePolicy::HostCli => {
                if let Some(ref explicit) = explicit_host_llm {
                    Arc::clone(explicit)
                } else if raw_proc_detected.is_invalid_gateway() {
                    let err_str = raw_proc_detected
                        .invalid_gateway_reason()
                        .unwrap_or("invalid gateway base_url")
                        .to_owned();
                    let fail_now = current_lease_clock();
                    let _ = store
                        .release_extraction_window_for_retry(&job_specs, &err_str, fail_now, 1);
                    if first_policy_error.is_none() {
                        first_policy_error = Some(err_str);
                    }
                    continue;
                } else if matches!(global_detected, DetectedGenerativeModel::RawOnly)
                    && !matches!(pol_lower.as_str(), "host" | "host_cli")
                {
                    match store.complete_extraction_window_fenced(
                        &job_specs,
                        &[],
                        NL_SCHEMA_VER,
                        at,
                    ) {
                        Ok(()) => {
                            jobs_processed += job_specs.len();
                        }
                        Err(StoreError::Conflict) => {}
                        Err(err) => return Err(err.into()),
                    }
                    continue;
                } else if let Some(ref cached) = cached_host_llm {
                    Arc::clone(cached)
                } else {
                    match build_global_host_llm_from_detected(&global_detected, args) {
                        Ok(built) => {
                            cached_host_llm = Some(Arc::clone(&built));
                            built
                        }
                        Err(err) => {
                            let err_str = err.to_string();
                            let fail_now = current_lease_clock();
                            let _ = store.release_extraction_window_for_retry(
                                &job_specs, &err_str, fail_now, 1,
                            );
                            if first_policy_error.is_none() {
                                first_policy_error = Some(err_str);
                            }
                            continue;
                        }
                    }
                }
            }
            ModelRoutePolicy::NamedEndpoint(ep_name) => {
                if let Some(ref explicit) = explicit_host_llm {
                    if explicit_route_policy == ModelRoutePolicy::NamedEndpoint(ep_name.clone()) {
                        Arc::clone(explicit)
                    } else {
                        let err_str = format!(
                            "explicit CLI runner policy does not cover named endpoint `{ep_name}`"
                        );
                        let fail_now = current_lease_clock();
                        let _ = store
                            .release_extraction_window_for_retry(&job_specs, &err_str, fail_now, 1);
                        if first_policy_error.is_none() {
                            first_policy_error = Some(err_str);
                        }
                        continue;
                    }
                } else if let Some(cached) = named_clients.get(ep_name) {
                    Arc::clone(cached)
                } else {
                    match build_named_endpoint_client_for_args(ep_name, &proc_env, args) {
                        Ok(built) => {
                            named_clients.insert(ep_name.clone(), Arc::clone(&built));
                            built
                        }
                        Err(err) => {
                            let err_str = err.to_string();
                            let fail_now = current_lease_clock();
                            let _ = store.release_extraction_window_for_retry(
                                &job_specs, &err_str, fail_now, 1,
                            );
                            if first_policy_error.is_none() {
                                first_policy_error = Some(err_str);
                            }
                            continue;
                        }
                    }
                }
            }
        };

        let mut context_slices = Vec::new();
        let mut target_slices = Vec::new();
        let mut target_chunk_records: Vec<(String, ContentHash, ActorId)> = Vec::new();
        for slice in window.slices {
            match slice.role {
                kp_model::SliceRole::Context => {
                    context_slices.push(slice);
                }
                kp_model::SliceRole::Target => {
                    let digest = ContentHash::digest(&slice.body);
                    if !args.force
                        && store.is_chunk_extracted(&digest, &slice.author, NL_SCHEMA_VER)?
                    {
                        continue;
                    }
                    target_chunk_records.push((
                        slice.chunk_id.clone(),
                        digest,
                        slice.author.clone(),
                    ));
                    target_slices.push(slice);
                }
            }
        }

        if target_slices.is_empty() {
            match store.complete_extraction_window_fenced(&job_specs, &[], NL_SCHEMA_VER, at) {
                Ok(()) => {
                    jobs_processed += job_specs.len();
                }
                Err(StoreError::Conflict) => {
                    let retry_now = current_lease_clock();
                    let _ = store.release_extraction_window_for_retry(
                        &job_specs,
                        "window_sibling_conflict",
                        retry_now,
                        EXTRACTION_MAX_ATTEMPTS,
                    );
                }
                Err(err) => return Err(err.into()),
            }
            continue;
        }

        if context_slices.is_empty()
            && target_slices
                .iter()
                .all(|s| kp_derive::is_chunk_trivial_or_ack_only(&s.body))
        {
            let ack_hashes: Vec<(ContentHash, ActorId)> = target_chunk_records
                .iter()
                .map(|(_, d, a)| (d.clone(), a.clone()))
                .collect();
            match store.complete_extraction_window_fenced(
                &job_specs,
                &ack_hashes,
                NL_SCHEMA_VER,
                at,
            ) {
                Ok(()) => {
                    jobs_processed += job_specs.len();
                }
                Err(StoreError::Conflict) => {
                    let retry_now = current_lease_clock();
                    let _ = store.release_extraction_window_for_retry(
                        &job_specs,
                        "window_sibling_conflict",
                        retry_now,
                        EXTRACTION_MAX_ATTEMPTS,
                    );
                }
                Err(err) => return Err(err.into()),
            }
            continue;
        }

        let primary_speaker = target_slices
            .first()
            .map(|s| s.author.clone())
            .unwrap_or_else(|| head_job.author.clone());
        let primary_source_id = target_slices
            .first()
            .map(|s| s.source_id.clone())
            .unwrap_or_else(|| head_job.source_id.clone());
        let window_ref_time = window
            .jobs
            .iter()
            .map(|j| j.reference_time)
            .max()
            .unwrap_or(head_job.reference_time);

        let mut slices = context_slices;
        slices.extend(target_slices);

        let cal =
            CalendarTable::from_reference_in_tz(window_ref_time, &kp_model::system_time_zone())?;
        let active_concepts = store.list_active_concepts()?;
        let req = build_extraction_batch_request_for_policy(
            store,
            kp_derive::ExtractionBatchBuildInput {
                source_id: primary_source_id,
                speaker: primary_speaker.clone(),
                reference_calendar: cal,
                active_concepts,
                chunks: slices,
                min_rate_limit_remaining_pct: args.min_quota_pct,
            },
            &job_policy,
        )?;

        let batch_outcome = match job_llm.extract_batch(&req).await {
            Ok(out) => out,
            Err(err) => {
                let err_str = err.to_string();
                let fail_now = current_lease_clock();
                if is_quota_error_message(&err_str) {
                    if let Ok(paused) =
                        store.pause_extraction_window_quota_fenced(&job_specs, &err_str)
                    {
                        jobs_paused_quota += paused;
                    }
                    break;
                }
                let _ = store.release_extraction_window_for_retry(
                    &job_specs,
                    &err_str,
                    fail_now,
                    EXTRACTION_MAX_ATTEMPTS,
                );
                return Err(err.into());
            }
        };

        let post_llm_now = current_lease_clock();
        if !store.verify_extraction_window_leases(&job_specs, post_llm_now)? {
            let _ = store.release_extraction_window_for_retry(
                &job_specs,
                "window_sibling_conflict",
                post_llm_now,
                EXTRACTION_MAX_ATTEMPTS,
            );
            continue;
        }

        let atoms = match batch_outcome {
            ExtractionBatchOutcome::SkippedLowQuota { .. } => {
                let paused = store.pause_extraction_window_quota_fenced(
                    &job_specs,
                    "low host rate-limit headroom",
                )?;
                if paused > 0 {
                    jobs_paused_quota += paused;
                    break;
                }
                continue;
            }
            ExtractionBatchOutcome::SkippedEmpty => {
                let chunk_hashes: Vec<(ContentHash, ActorId)> = target_chunk_records
                    .iter()
                    .map(|(_, d, a)| (d.clone(), a.clone()))
                    .collect();
                match store.complete_extraction_window_fenced(
                    &job_specs,
                    &chunk_hashes,
                    NL_SCHEMA_VER,
                    at,
                ) {
                    Ok(()) => {
                        jobs_processed += job_specs.len();
                    }
                    Err(StoreError::Conflict) => {
                        let retry_now = current_lease_clock();
                        let _ = store.release_extraction_window_for_retry(
                            &job_specs,
                            "window_sibling_conflict",
                            retry_now,
                            EXTRACTION_MAX_ATTEMPTS,
                        );
                    }
                    Err(err) => return Err(err.into()),
                }
                continue;
            }
            ExtractionBatchOutcome::Completed { atoms } => atoms,
        };

        let mut unextracted_ids = BTreeSet::new();
        let mut final_chunk_hashes = Vec::with_capacity(target_chunk_records.len());
        for (cid, digest, slice_author) in target_chunk_records {
            if !args.force && store.is_chunk_extracted(&digest, &slice_author, NL_SCHEMA_VER)? {
                continue;
            }
            unextracted_ids.insert(cid);
            final_chunk_hashes.push((digest, slice_author));
        }
        if unextracted_ids.is_empty() {
            let _ = store.complete_extraction_window_fenced(&job_specs, &[], NL_SCHEMA_VER, at);
            continue;
        }
        let filtered_atoms: Vec<_> = atoms
            .into_iter()
            .filter(|a| unextracted_ids.contains(&a.quote.chunk_id))
            .collect();

        let source_trust_floor = if window.source_scope_id.is_some() {
            kp_model::TrustTier::ToolOrModelVerified
        } else if window.has_http_uri {
            kp_model::TrustTier::ExternalUntrusted
        } else {
            NlFunnelOptions::trust_floor_for_uri(&window.primary_uri)
        };
        let effective_ingester = window.ingested_by.clone().unwrap_or_else(|| actor.clone());

        let nl_opts = NlFunnelOptions {
            audience: window.readable_by,
            at: window_ref_time,
            route_policy: job_policy,
            source_trust_floor,
            source_origin: window.source_scope_id.map(|scope_id| SourceOriginMeta {
                speaker: primary_speaker.clone(),
                ingested_by: effective_ingester,
                source_scope_id: scope_id,
            }),
            rejections: store.list_rejections(None)?,
            extra_concepts: Vec::new(),
        };

        let (outcome, staged) = match stage_extracted_atoms(store, &req, &filtered_atoms, &nl_opts)
        {
            Ok(out) => out,
            Err(err) => {
                let err_str = err.to_string();
                let fail_now = current_lease_clock();
                let _ = store.release_extraction_window_for_retry(
                    &job_specs,
                    &err_str,
                    fail_now,
                    EXTRACTION_MAX_ATTEMPTS,
                );
                return Err(err.into());
            }
        };

        let commit_now = current_lease_clock();
        match store.commit_staged_extraction_job_fenced(&kp_store::FencedJobCommitSpec {
            jobs: &job_specs,
            lease_now: commit_now,
            batch: &staged,
            chunk_hashes: &final_chunk_hashes,
            schema_ver: NL_SCHEMA_VER,
            at,
        }) {
            Ok(()) => {}
            Err(StoreError::Conflict) => {
                let _ = store.release_extraction_window_for_retry(
                    &job_specs,
                    "window_sibling_conflict",
                    commit_now,
                    EXTRACTION_MAX_ATTEMPTS,
                );
                continue;
            }
            Err(err) => return Err(err.into()),
        }

        let _ = reconcile_contradicted_events(
            store,
            job_llm.as_ref(),
            &outcome.active_concepts,
            nl_opts.at,
        )
        .await?;

        created_facts.extend(outcome.created_fact_ids);
        superseded_facts.extend(outcome.superseded_old_fact_ids);
        dropped_rejected.extend(outcome.dropped_rejected);
        jobs_processed += job_specs.len();
    }

    let align_override = explicit_host_llm.as_deref();
    let (derive_outcome, named_align_err) =
        run_concept_alignment_and_derive_with_named(store, align_override, &named_clients, at)
            .await?;

    if !is_daemon_background {
        if let Some(err_msg) = first_policy_error {
            return Err(err_msg.into());
        }
        if matches!(explicit_route_policy, ModelRoutePolicy::NamedEndpoint(_)) {
            if let Some(err_msg) = named_align_err {
                return Err(err_msg.into());
            }
        }
    }

    Ok(json!({
        "jobs_processed": jobs_processed,
        "jobs_paused_quota": jobs_paused_quota,
        "created_facts": created_facts,
        "superseded_facts": superseded_facts,
        "dropped_rejected": dropped_rejected,
        "derived_warnings": derive_outcome.derived_warnings,
        "superseded_contradicted_events": derive_outcome.superseded_contradicted_events,
        "emitted_reviews": derive_outcome.emitted_reviews,
    }))
}

fn allow_guard_once(
    store: &mut Store,
    actor: &ActorId,
    role: MemberRole,
    raw_fact_ids_or_refs: &[String],
    at: EpochMs,
) -> Result<Value> {
    if kp_ingest::hooks::is_internal_extraction() {
        return Err(StoreError::PermissionDenied.into());
    }
    if !matches!(role, MemberRole::Owner | MemberRole::Maintainer) {
        return Err(StoreError::PermissionDenied.into());
    }
    if raw_fact_ids_or_refs.is_empty() {
        return Err(KpError::invalid("fact_id", "fact_id must be nonempty").into());
    }
    let mut resolved_ids = Vec::with_capacity(raw_fact_ids_or_refs.len());
    for raw_fact_id_or_ref in raw_fact_ids_or_refs {
        resolved_ids.push(store.resolve_visible_or_raw_fact_id(actor, at, raw_fact_id_or_ref)?);
    }
    let grants = store.grant_guard_allow_once_many(actor, &resolved_ids, ALLOW_ONCE_TTL_MS, at)?;
    if grants.len() == 1 {
        let mut val = serde_json::to_value(&grants[0])?;
        if let Value::Object(ref mut map) = val {
            map.insert("grants".into(), serde_json::to_value(&grants)?);
        }
        Ok(val)
    } else {
        Ok(json!({
            "single_use": true,
            "count": grants.len(),
            "grants": grants,
        }))
    }
}

fn reject_fact(
    store: &mut Store,
    actor: &ActorId,
    role: MemberRole,
    raw_fact_id: &str,
    reason: &str,
    at: EpochMs,
) -> Result<Value> {
    let trimmed_reason = reason.trim();
    if trimmed_reason.is_empty() {
        return Err(KpError::invalid("reason", "rejection reason must be nonempty").into());
    }
    let resolved_id = store.resolve_visible_or_raw_fact_id(actor, at, raw_fact_id)?;
    if !store
        .filter_visible_fact_ids(actor, at, std::slice::from_ref(&resolved_id))?
        .contains(&resolved_id)
    {
        return Err(StoreError::NotFound.into());
    }
    if !matches!(role, MemberRole::Owner | MemberRole::Maintainer) {
        let graph = store.derivation_view()?;
        let fact = graph.facts.get(&resolved_id).ok_or(StoreError::NotFound)?;
        if fact.author != *actor {
            return Err(StoreError::PermissionDenied.into());
        }
    }

    let record = store.reject_fact(&resolved_id, actor, trimmed_reason, at)?;

    Ok(json!({
        "rejection_id": record.id,
        "fact_id": resolved_id,
        "withdrawn": [resolved_id],
        "evidence_key": record.evidence_key,
        "semantic_key": record.semantic_key,
        "reason": record.reason,
    }))
}

async fn resolve_review(
    store: &mut Store,
    actor: &ActorId,
    review_id: &ReviewId,
    choice: ReviewChoice,
    at: EpochMs,
) -> Result<Value> {
    let item = store
        .get_review_item(review_id)?
        .filter(|it| &it.actor == actor)
        .ok_or(StoreError::NotFound)?;

    if item.status != ReviewStatus::Open {
        return Err(StoreError::Conflict.into());
    }

    let mut replacement_fact_id: Option<FactId> = None;
    let mut derived_count = 0_usize;
    let mut final_status = match choice {
        ReviewChoice::Confirm | ReviewChoice::Narrow => ReviewStatus::Confirmed,
        ReviewChoice::Dismiss => ReviewStatus::Dismissed,
    };

    match item.kind {
        ReviewKind::WritebackProposal => match choice {
            ReviewChoice::Confirm => {
                crate::providers_cli::attach_ready_embedder(store);
                let global_policy = explicit_writeback_default_policy();
                let new_fid = store.apply_confirmed_writeback_proposal_with_policy(
                    review_id,
                    actor,
                    &global_policy,
                    at,
                )?;
                let align_out = run_concept_alignment_and_derive(store, None, at).await?;
                return Ok(json!({
                    "review_id": item.id,
                    "choice": choice.as_str(),
                    "status": ReviewStatus::Confirmed,
                    "target_id": item.target_id,
                    "replacement_fact_id": new_fid,
                    "derived": align_out.derived_warnings,
                }));
            }
            ReviewChoice::Dismiss | ReviewChoice::Narrow => {
                final_status = ReviewStatus::Dismissed;
            }
        },
        ReviewKind::AmbiguousScope => {
            let target_fact_id: FactId = item.target_id.parse()?;
            let graph = store.derivation_view()?;
            if let Some(old_fact) = graph.facts.get(&target_fact_id).cloned() {
                if old_fact.author != *actor {
                    return Err(StoreError::PermissionDenied.into());
                }
                if old_fact.status == FactStatus::Active && old_fact.broader_scope.is_some() {
                    let old_entities = store
                        .entities()?
                        .remove(&target_fact_id)
                        .unwrap_or_default();
                    let decision = match choice {
                        ReviewChoice::Confirm => ScopeReviewDecision::ConfirmBroader,
                        ReviewChoice::Narrow => ScopeReviewDecision::KeepNarrow,
                        ReviewChoice::Dismiss => ScopeReviewDecision::Dismiss,
                    };
                    let outcome = resolve_ambiguous_scope_review(
                        store,
                        &target_fact_id,
                        actor,
                        decision,
                        Some(&item),
                        at,
                    )?;
                    derived_count = outcome.derive_derived;
                    if let Some(new_id) = outcome.replacement_fact_id {
                        let widened_audience = if choice == ReviewChoice::Confirm {
                            if let Some(broader) = old_fact
                                .broader_scope
                                .as_ref()
                                .map(|b| &b.broader_candidate)
                            {
                                match widen_access_for_scope(store, &graph, &old_fact, broader, at)?
                                {
                                    FactAccess::Base { audience, .. } => Some(audience),
                                    FactAccess::Derived { .. } => None,
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                        store.apply_scope_review_fact_updates(&ScopeReviewFactUpdates {
                            replacement_fact_id: Some(&new_id),
                            widened_audience: widened_audience.as_ref(),
                            copied_entities: (!old_entities.is_empty()).then_some(&old_entities),
                            clear_broader_scope: None,
                            change_log_entry: None,
                        })?;
                        let align_out = run_concept_alignment_and_derive(store, None, at).await?;
                        derived_count = derived_count.max(align_out.derived_warnings);
                        replacement_fact_id = Some(new_id);
                    } else if choice == ReviewChoice::Dismiss {
                        store.apply_scope_review_fact_updates(&ScopeReviewFactUpdates {
                            clear_broader_scope: Some((&target_fact_id, actor)),
                            ..ScopeReviewFactUpdates::default()
                        })?;
                    }
                }
            }
        }
        ReviewKind::IdentityMapping => {
            if let (Some(provider), Some(ext_id)) = (
                item.payload.get("provider").and_then(Value::as_str),
                item.payload.get("external_id").and_then(Value::as_str),
            ) {
                match choice {
                    ReviewChoice::Confirm => {
                        if let Some(proposed) = item
                            .payload
                            .get("proposed_actor")
                            .and_then(Value::as_str)
                            .and_then(|s| s.parse::<ActorId>().ok())
                        {
                            store.resolve_external_identity_mapping(
                                provider,
                                ext_id,
                                Some(&proposed),
                                at,
                            )?;
                        }
                    }
                    ReviewChoice::Dismiss | ReviewChoice::Narrow => {
                        store.resolve_external_identity_mapping(provider, ext_id, None, at)?;
                    }
                }
            }
        }
        ReviewKind::ConceptAlignment => {
            let verdict = match choice {
                ReviewChoice::Confirm => Some(kp_model::EntailmentVerdict::Entails),
                ReviewChoice::Narrow => Some(kp_model::EntailmentVerdict::Unrelated),
                ReviewChoice::Dismiss => None,
            };
            if let Some(verdict) = verdict {
                let left_cid = item
                    .payload
                    .get("left_concept_id")
                    .and_then(Value::as_str)
                    .map(str::parse::<kp_model::ConceptId>)
                    .transpose()?;
                let right_cid = item
                    .payload
                    .get("right_concept_id")
                    .and_then(Value::as_str)
                    .map(str::parse::<kp_model::ConceptId>)
                    .transpose()?;
                let left_fid = item
                    .payload
                    .get("event_fact_id")
                    .or_else(|| item.payload.get("left_fact_id"))
                    .and_then(Value::as_str)
                    .map(str::parse::<FactId>)
                    .transpose()?;
                let right_fid = item
                    .payload
                    .get("constraint_fact_id")
                    .or_else(|| item.payload.get("right_fact_id"))
                    .and_then(Value::as_str)
                    .map(str::parse::<FactId>)
                    .transpose()?;
                if let (Some(left), Some(right)) = (left_cid, right_cid) {
                    kp_derive::record_manual_concept_alignment(
                        store,
                        &left,
                        &right,
                        verdict,
                        left_fid.as_ref(),
                        right_fid.as_ref(),
                        at,
                    )?;
                    let g = store.derivation_view()?;
                    let tbl = kp_derive::ConceptAlignmentTable::from_graph(&g);
                    let _ =
                        store.reconcile_relation_group_audiences(|l, r| tbl.entails(l, r), at)?;
                    let res = kp_derive::derive(store)?;
                    derived_count = res.derived;
                }
            }
        }
    }

    let resolved = store.resolve_review_item(review_id, actor, final_status, at)?;
    store.apply_scope_review_fact_updates(&ScopeReviewFactUpdates {
        change_log_entry: Some((
            review_id,
            actor,
            json!({
                "choice": choice.as_str(),
                "status": resolved.status,
                "replacement_fact_id": replacement_fact_id,
            }),
            at,
        )),
        ..ScopeReviewFactUpdates::default()
    })?;

    Ok(json!({
        "review_id": resolved.id,
        "choice": choice.as_str(),
        "status": resolved.status,
        "target_id": resolved.target_id,
        "replacement_fact_id": replacement_fact_id,
        "derived": derived_count,
    }))
}

fn widen_access_for_scope(
    store: &Store,
    graph: &kp_model::DerivationView,
    old_fact: &Fact,
    broader: &GovernedScope,
    at: EpochMs,
) -> Result<FactAccess> {
    let FactAccess::Base { audience, stamp } = &old_fact.access else {
        return Ok(old_fact.access.clone());
    };
    let mut updated_audience = audience.clone();
    if let AudienceSpec::Explicit(current) = &audience {
        let mut expanded = current.clone();
        match broader {
            GovernedScope::Actors { actors } => {
                for a in actors {
                    if graph.members.contains(a) {
                        expanded.insert(a.clone());
                    }
                }
            }
            GovernedScope::RelationGroup {
                anchor,
                group_concept_id,
            } => {
                let alignment = kp_derive::ConceptAlignmentTable::from_graph(graph);
                for f in graph.facts.values() {
                    if f.status == FactStatus::Active
                        && f.kind == FactKind::Membership
                        && f.author == *anchor
                    {
                        if let Ok(rel) =
                            serde_json::from_value::<MembershipRelation>(f.object.clone())
                        {
                            if &rel.anchor == anchor
                                && alignment.entails(&rel.group_concept_id, group_concept_id)
                                && graph.members.contains(&rel.member)
                            {
                                expanded.insert(rel.member);
                            }
                        }
                    }
                }
            }
            GovernedScope::SelfOnly { .. } | GovernedScope::Resource { .. } => {}
        }
        let candidate = AudienceSpec::Explicit(expanded);
        let ceiling_ok = match &old_fact.source_scope_id {
            Some(sid) => match store.source_scope(sid)? {
                Some(scope) => scope
                    .validate_publish_audience(&candidate, &graph.members)
                    .is_ok(),
                None => false,
            },
            None => true,
        };
        if ceiling_ok {
            updated_audience = candidate;
        }
    }
    Ok(FactAccess::Base {
        audience: updated_audience,
        stamp: BitemporalStamp {
            valid: stamp.valid,
            recorded_at: at,
            expired_at: None,
        },
    })
}
