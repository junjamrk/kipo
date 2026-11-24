use std::error::Error;
use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};
use kp_connect::{
    commit_staged_cursors, list_scopes_in_state, parse_connector_model_policy,
    parse_identity_mapping, poll_enabled_scopes_staged, set_scope_enabled_in_state,
    upsert_scope_in_state, ConnectorKind, ConnectorStateStore,
};
use kp_model::{ConnectorId, EpochMs, KpError, MemberRole, SourceScopeId};
use kp_store::{PublicIngestStreamReport, Store, StoreError};
use serde_json::json;

use crate::{now, output, Cli};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum CliConnectorKind {
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

#[derive(Args)]
pub struct ConnectorCommandArgs {
    /// Optional connector state file path (defaults to `$KP_CONNECTOR_STATE_PATH` or `~/.kp/connectors/state.json`).
    #[arg(long, global = true)]
    pub state: Option<PathBuf>,

    #[command(subcommand)]
    pub command: ConnectorSubcommand,
}

#[derive(Subcommand)]
pub enum ConnectorSubcommand {
    /// Manage allowlisted upstream connector scopes stored in `0600` `state.json`.
    Scope {
        #[command(subcommand)]
        command: ConnectorScopeCommand,
    },
    /// Poll enabled connector scopes, ingest via the Public Ingest Contract, and commit cursors on success.
    Sync(ConnectorSyncArgs),
}

#[derive(Subcommand)]
pub enum ConnectorScopeCommand {
    /// Register or update an enabled connector scope.
    Add {
        #[arg(long, value_enum)]
        kind: CliConnectorKind,
        #[arg(long = "scope-id")]
        scope_id: String,
        #[arg(long)]
        upstream: String,
        #[arg(long = "connector-id")]
        connector_id: Option<String>,
        #[arg(long, default_value = "")]
        label: String,
        #[arg(long = "model-policy")]
        model_policy: Option<String>,
    },
    /// List all configured connector scopes.
    List,
    /// Re-enable a configured connector scope.
    Enable(ScopeToggleArgs),
    /// Disable a configured connector scope while preserving its cursor.
    Disable(ScopeToggleArgs),
}

#[derive(Args)]
pub struct ScopeToggleArgs {
    #[arg(long = "connector-id", required_unless_present = "pos_connector_id")]
    pub connector_id: Option<ConnectorId>,
    #[arg(long = "scope-id", required_unless_present = "pos_scope_id")]
    pub scope_id: Option<SourceScopeId>,
    #[arg(value_name = "CONNECTOR_ID", conflicts_with = "connector_id")]
    pub pos_connector_id: Option<ConnectorId>,
    #[arg(value_name = "SCOPE_ID", conflicts_with = "scope_id")]
    pub pos_scope_id: Option<SourceScopeId>,
}

impl ScopeToggleArgs {
    fn resolve(&self) -> kp_model::Result<(&ConnectorId, &SourceScopeId)> {
        let cid = self
            .connector_id
            .as_ref()
            .or(self.pos_connector_id.as_ref())
            .ok_or_else(|| KpError::invalid("connector_id", "connector_id is required"))?;
        let sid = self
            .scope_id
            .as_ref()
            .or(self.pos_scope_id.as_ref())
            .ok_or_else(|| KpError::invalid("scope_id", "scope_id is required"))?;
        Ok((cid, sid))
    }
}

#[derive(Args)]
pub struct ConnectorSyncArgs {
    #[arg(long, value_enum)]
    pub kind: Option<CliConnectorKind>,
    /// Repeatable confirmed identity mapping (`<provider>:<external_id>=<actor_id>`).
    #[arg(long = "identity")]
    pub identities: Vec<String>,
    /// Immediately drain queued extraction jobs and run derivation after ingesting.
    #[arg(long)]
    pub extract: bool,
    #[arg(long)]
    pub at: Option<i64>,
}

pub async fn run(cli: &Cli, args: &ConnectorCommandArgs) -> Result<()> {
    let state_store = match &args.state {
        Some(path) => ConnectorStateStore::new(path),
        None => ConnectorStateStore::open_default()?,
    };

    match &args.command {
        ConnectorSubcommand::Scope { command } => match command {
            ConnectorScopeCommand::Add {
                kind,
                scope_id,
                upstream,
                connector_id,
                label,
                model_policy,
            } => {
                let parsed_policy = model_policy
                    .as_deref()
                    .map(parse_connector_model_policy)
                    .transpose()?;
                let global_label = crate::governance::detect_global_model().display_label();
                let (scope, notice) = upsert_scope_in_state(
                    &state_store,
                    &kp_connect::ScopeUpsertSpec {
                        kind: ConnectorKind::from(*kind),
                        connector_id: connector_id.as_deref(),
                        source_scope_id: scope_id,
                        upstream_container_id: upstream,
                        label,
                        model_policy: parsed_policy,
                        detected_global_model_label: &global_label,
                    },
                )?;
                if !cli.json {
                    if let Some(ref line) = notice {
                        eprintln!("{line}");
                    }
                }
                let val = json!({
                    "scope": scope,
                    "notice": notice,
                });
                let human = format!(
                    "Enabled {}::{} -> {}",
                    scope.connector_id, scope.source_scope_id, scope.upstream_container_id
                );
                output(cli.json, &val, &human)?;
            }
            ConnectorScopeCommand::List => {
                let scopes = list_scopes_in_state(&state_store)?;
                let human = if scopes.is_empty() {
                    "(no connector scopes configured)".to_owned()
                } else {
                    scopes
                        .iter()
                        .map(|s| {
                            let st = if s.enabled { "enabled" } else { "disabled" };
                            format!(
                                "{}::{} [{}] upstream={} label={}",
                                s.connector_id,
                                s.source_scope_id,
                                st,
                                s.upstream_container_id,
                                s.label
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                output(cli.json, &json!({ "scopes": scopes }), &human)?;
            }
            ConnectorScopeCommand::Enable(toggle) => {
                let (cid, sid) = toggle.resolve()?;
                let updated = set_scope_enabled_in_state(&state_store, cid, sid, true)?;
                let human = format!(
                    "Enabled {}::{}",
                    updated.connector_id, updated.source_scope_id
                );
                output(cli.json, &json!({ "scope": updated }), &human)?;
            }
            ConnectorScopeCommand::Disable(toggle) => {
                let (cid, sid) = toggle.resolve()?;
                let updated = set_scope_enabled_in_state(&state_store, cid, sid, false)?;
                let human = format!(
                    "Disabled {}::{}",
                    updated.connector_id, updated.source_scope_id
                );
                output(cli.json, &json!({ "scope": updated }), &human)?;
            }
        },
        ConnectorSubcommand::Sync(sync_args) => {
            let mut store = Store::open(&cli.path)?;
            crate::providers_cli::attach_ready_embedder(&mut store);
            let actor = cli
                .actor
                .clone()
                .unwrap_or_else(|| store.owner_actor().clone());
            let Some(caller_role) = store.member_role(&actor)? else {
                return Err(StoreError::PermissionDenied.into());
            };
            let sync_at = sync_args.at.map(EpochMs).unwrap_or(now()?);

            let mut confirmed = store.confirmed_external_identities()?;
            if !sync_args.identities.is_empty()
                && !matches!(caller_role, MemberRole::Owner | MemberRole::Maintainer)
            {
                return Err(StoreError::PermissionDenied.into());
            }
            for raw_id in &sync_args.identities {
                let (key, mapped_actor) = parse_identity_mapping(raw_id)?;
                if store.member_role(&mapped_actor)?.is_none() {
                    return Err(StoreError::PermissionDenied.into());
                }
                confirmed.insert(key, mapped_actor);
            }

            // Stage 1: Poll enabled scopes in memory without advancing `state.json` cursors.
            let staged = poll_enabled_scopes_staged(
                &state_store,
                sync_args.kind.map(ConnectorKind::from),
                &actor,
                &confirmed,
                sync_at,
            )
            .await?;

            // Stage 2: Apply Public Ingest Contract JSONL.
            // Any error here returns before `commit_staged_cursors`, keeping `state.json` unadvanced.
            let ingest_report = if staged.jsonl.trim().is_empty() {
                PublicIngestStreamReport::default()
            } else {
                let global_policy = crate::governance::default_auto_route_policy();
                let rep = store.apply_public_ingest_jsonl(&staged.jsonl, &actor, &global_policy)?;
                if rep.records_superseded > 0
                    || rep.records_withdrawn > 0
                    || rep.rosters_applied > 0
                {
                    let _ = kp_derive::derive(&mut store)?;
                }
                rep
            };

            // Stage 3: Atomically commit staged cursors to `0600` `state.json` after ingest succeeded.
            let cursors_committed = commit_staged_cursors(&state_store, &staged.staged_cursors)?;

            // Stage 4: Optional immediate extraction + joined-community synchronization.
            let extraction = if sync_args.extract {
                Some(crate::governance::drain_for_public_ingest(&mut store, &actor, sync_at).await?)
            } else {
                None
            };
            if sync_args.extract
                || ingest_report.records_superseded > 0
                || ingest_report.records_withdrawn > 0
                || ingest_report.rosters_applied > 0
            {
                crate::community::notify_or_sync_joined(&cli.path, &mut store, sync_at).await;
            }

            let val = json!({
                "scopes_polled": staged.scopes_polled,
                "rosters_emitted": staged.rosters_emitted,
                "records_emitted": staged.records_emitted,
                "skipped_low_signal": staged.skipped_low_signal,
                "cursors_committed": cursors_committed,
                "ingest": ingest_report,
                "extraction": extraction,
            });
            let human = format!(
                "Synced {} scope(s): {} roster(s), {} upserted, {} superseded, {} withdrawn ({} cursor(s) committed)",
                staged.scopes_polled,
                ingest_report.rosters_applied,
                ingest_report.records_upserted,
                ingest_report.records_superseded,
                ingest_report.records_withdrawn,
                cursors_committed,
            );
            output(cli.json, &val, &human)?;
        }
    }

    Ok(())
}
