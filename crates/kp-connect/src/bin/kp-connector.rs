//! Standalone decoupled external connector CLI (`kp-connector`) emitting standard Public Ingest
//! Contract JSONL (`Rev 5 §0.1.3 & §0.2.2`) with two-phase cursor safety.

use std::collections::BTreeMap;
use std::error::Error;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand, ValueEnum};
use kp_connect::{
    commit_staged_cursors, list_scopes_in_state, parse_connector_model_policy,
    parse_identity_mapping, pipe_jsonl_to_kp_ingest, poll_enabled_scopes_staged,
    set_scope_enabled_in_state, upsert_scope_in_state, ConnectorKind, ConnectorStateStore,
};
use kp_model::{ActorId, ConnectorId, EpochMs, KpError, SourceScopeId};
use serde_json::json;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum CliConnectorKind {
    Feishu,
    #[value(name = "github")]
    GitHub,
    #[value(name = "agent-session")]
    AgentSession,
}

impl From<CliConnectorKind> for ConnectorKind {
    fn from(value: CliConnectorKind) -> Self {
        match value {
            CliConnectorKind::Feishu => Self::Feishu,
            CliConnectorKind::GitHub => Self::GitHub,
            CliConnectorKind::AgentSession => Self::AgentSession,
        }
    }
}

#[derive(Parser)]
#[command(
    name = "kp-connector",
    version,
    about = "Decoupled external connectors emitting Public Ingest Contract JSONL for `kp ingest`"
)]
struct Cli {
    /// Path to connector-owned state JSON (defaults to `$KP_CONNECTOR_STATE_PATH` or `~/.kp/connectors/state.json`).
    #[arg(long, global = true)]
    state: Option<PathBuf>,

    /// Emit structured JSON output for administrative commands and `--pipe-kp` sync summaries.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage allowlisted upstream connector scopes in `state.json`.
    Scope {
        #[command(subcommand)]
        command: ScopeCommand,
    },
    /// Poll enabled scopes and emit Public Ingest Contract JSONL (`stdout` or `--pipe-kp`).
    Sync(SyncArgs),
}

#[derive(Subcommand)]
enum ScopeCommand {
    /// Register or update an enabled connector scope.
    Add {
        #[arg(long, value_enum)]
        kind: CliConnectorKind,
        #[arg(long = "connector-id")]
        connector_id: Option<String>,
        #[arg(long = "scope-id")]
        scope_id: String,
        #[arg(long)]
        upstream: String,
        #[arg(long, default_value = "")]
        label: String,
        #[arg(long = "model-policy")]
        model_policy: Option<String>,
        #[arg(long = "global-model-label", default_value = "")]
        global_model_label: String,
    },
    /// List all configured connector scopes.
    List,
    /// Disable a configured connector scope without deleting its cursor.
    Disable {
        connector_id: ConnectorId,
        scope_id: SourceScopeId,
    },
    /// Re-enable a previously disabled connector scope.
    Enable {
        connector_id: ConnectorId,
        scope_id: SourceScopeId,
    },
}

#[derive(clap::Args)]
struct SyncArgs {
    #[arg(long = "ingested-by")]
    ingested_by: ActorId,

    #[arg(long, value_enum)]
    kind: Option<CliConnectorKind>,

    /// Optional repeatable confirmed identity mapping (`<provider>:<external_id>=<actor_id>`).
    #[arg(long = "identity")]
    identities: Vec<String>,

    /// Optional deterministic timestamp override (epoch ms).
    #[arg(long)]
    now: Option<i64>,

    /// Persist staged cursors immediately when streaming JSONL to `stdout`.
    #[arg(long = "commit-cursors")]
    commit_cursors: bool,

    /// Pipe emitted JSONL into `<kp_bin> --path <library> --as <ingested-by> ingest -` and commit cursors only on exit 0.
    #[arg(long = "pipe-kp")]
    pipe_kp: Option<PathBuf>,

    /// Target library directory passed to `<kp_bin> --path <library>` when `--pipe-kp` is set.
    #[arg(long)]
    library: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    let state_store = match &cli.state {
        Some(path) => ConnectorStateStore::new(path),
        None => ConnectorStateStore::open_default()?,
    };

    match cli.command {
        Command::Scope { command } => match command {
            ScopeCommand::Add {
                kind,
                connector_id,
                scope_id,
                upstream,
                label,
                model_policy,
                global_model_label,
            } => {
                let parsed_policy = model_policy
                    .as_deref()
                    .map(parse_connector_model_policy)
                    .transpose()?;
                let (scope, notice) = upsert_scope_in_state(
                    &state_store,
                    &kp_connect::ScopeUpsertSpec {
                        kind: ConnectorKind::from(kind),
                        connector_id: connector_id.as_deref(),
                        source_scope_id: &scope_id,
                        upstream_container_id: &upstream,
                        label: &label,
                        model_policy: parsed_policy,
                        detected_global_model_label: &global_model_label,
                    },
                )?;
                if let Some(line) = &notice {
                    eprintln!("{line}");
                }
                if cli.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "scope": scope,
                            "notice": notice,
                        }))?
                    );
                } else {
                    println!(
                        "Enabled {}::{} -> {}",
                        scope.connector_id, scope.source_scope_id, scope.upstream_container_id
                    );
                }
            }
            ScopeCommand::List => {
                let scopes = list_scopes_in_state(&state_store)?;
                if cli.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({ "scopes": scopes }))?
                    );
                } else if scopes.is_empty() {
                    println!("No connector scopes configured.");
                } else {
                    for s in scopes {
                        let status = if s.enabled { "enabled" } else { "disabled" };
                        println!(
                            "{}::{} [{}] upstream={} label={}",
                            s.connector_id,
                            s.source_scope_id,
                            status,
                            s.upstream_container_id,
                            s.label
                        );
                    }
                }
            }
            ScopeCommand::Disable {
                connector_id,
                scope_id,
            } => {
                let updated =
                    set_scope_enabled_in_state(&state_store, &connector_id, &scope_id, false)?;
                if cli.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({ "scope": updated }))?
                    );
                } else {
                    println!(
                        "Disabled {}::{}",
                        updated.connector_id, updated.source_scope_id
                    );
                }
            }
            ScopeCommand::Enable {
                connector_id,
                scope_id,
            } => {
                let updated =
                    set_scope_enabled_in_state(&state_store, &connector_id, &scope_id, true)?;
                if cli.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({ "scope": updated }))?
                    );
                } else {
                    println!(
                        "Enabled {}::{}",
                        updated.connector_id, updated.source_scope_id
                    );
                }
            }
        },
        Command::Sync(args) => {
            if args.pipe_kp.is_some() && args.library.is_none() {
                return Err(
                    KpError::invalid("library", "`--pipe-kp` requires `--library <path>`").into(),
                );
            }
            if args.library.is_some() && args.pipe_kp.is_none() {
                return Err(KpError::invalid(
                    "pipe_kp",
                    "`--library` requires `--pipe-kp <kp_bin>`",
                )
                .into());
            }

            let mut confirmed = BTreeMap::new();
            for raw_id in &args.identities {
                let (key, actor) = parse_identity_mapping(raw_id)?;
                confirmed.insert(key, actor);
            }

            let now_ms = match args.now {
                Some(explicit) => EpochMs(explicit),
                None => current_epoch_ms()?,
            };

            let staged = poll_enabled_scopes_staged(
                &state_store,
                args.kind.map(ConnectorKind::from),
                &args.ingested_by,
                &confirmed,
                now_ms,
            )
            .await?;

            if let (Some(kp_bin), Some(lib_path)) = (&args.pipe_kp, &args.library) {
                let pipe_out =
                    pipe_jsonl_to_kp_ingest(kp_bin, lib_path, &args.ingested_by, &staged.jsonl)?;
                let committed = commit_staged_cursors(&state_store, &staged.staged_cursors)?;
                let ingest_json: Option<serde_json::Value> =
                    serde_json::from_slice(&pipe_out.stdout).ok();
                if cli.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "scopes_polled": staged.scopes_polled,
                            "rosters_emitted": staged.rosters_emitted,
                            "records_emitted": staged.records_emitted,
                            "skipped_low_signal": staged.skipped_low_signal,
                            "cursors_committed": committed,
                            "ingest": ingest_json,
                        }))?
                    );
                } else {
                    println!(
                        "Synced {} scopes ({} rosters, {} records, {} cursors committed)",
                        staged.scopes_polled,
                        staged.rosters_emitted,
                        staged.records_emitted,
                        committed
                    );
                }
            } else {
                let mut stdout = io::stdout().lock();
                stdout.write_all(staged.jsonl.as_bytes())?;
                stdout.flush()?;
                if args.commit_cursors {
                    let _ = commit_staged_cursors(&state_store, &staged.staged_cursors)?;
                }
            }
        }
    }

    Ok(())
}

fn current_epoch_ms() -> Result<EpochMs, KpError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| KpError::invalid("time", "system clock is before UNIX_EPOCH"))?
        .as_millis();
    let ms_i64 = i64::try_from(millis)
        .map_err(|_| KpError::invalid("time", "epoch millisecond overflow"))?;
    Ok(EpochMs(ms_i64))
}
