//! High-level connector scope management and two-phase cursor-safe staged polling (`Rev 5 §0.1.3 & §0.2.2`).
//!
//! Shared by both the standalone `kp-connector` binary and optional CLI integrations while
//! maintaining zero dependency on `kp-store` or `rusqlite`.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use kp_model::{ActorId, ConnectorId, EpochMs, KpError, ModelRoutePolicy, SourceScopeId};
use serde::{Deserialize, Serialize};

use crate::feishu::{
    feishu_enable_notice, FeishuConnector, FeishuSyncConfig, FeishuTarget, LarkCliTransport,
};
use crate::github::{
    parse_owner_repo, GhCliFallbackTransport, GitHubConnector, GitHubSyncConfig,
    GitHubTransportChain, ReqwestGitHubTransport, ENV_GH_CLI_BIN, ENV_GITHUB_API_BASE,
};
use crate::sessions::{AgentSessionConnector, SessionBackfillConfig};
use crate::state::{connector_scope_state_key, ConnectorStateSnapshot, ConnectorStateStore};
use crate::subprocess::CommandOutput;
use crate::{ConnectError, Connector, ConnectorCursor, ConnectorKind, ConnectorScope, Result};

/// Staged output of [`poll_enabled_scopes_staged`].
///
/// Cursors are held in `staged_cursors` in memory and are **never** written to `state.json` until
/// downstream ingestion succeeds and [`commit_staged_cursors`] is explicitly invoked.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedConnectorSync {
    pub jsonl: String,
    pub staged_cursors: BTreeMap<String, ConnectorCursor>,
    pub scopes_polled: usize,
    pub rosters_emitted: usize,
    pub records_emitted: usize,
    pub skipped_low_signal: usize,
}

/// Parses a CLI `--model-policy` specification (`inherit`, `raw_only`, or `named_endpoint:<id>`).
pub fn parse_connector_model_policy(raw: &str) -> Result<ModelRoutePolicy> {
    let trimmed = raw.trim();
    match trimmed {
        "" | "inherit" | "default" | "host" | "host_cli" | "host-cli" => {
            Ok(ModelRoutePolicy::HostCli)
        }
        "raw_only" | "raw-only" => Ok(ModelRoutePolicy::RawOnly),
        _ => {
            let endpoint = trimmed
                .strip_prefix("named_endpoint:")
                .or_else(|| trimmed.strip_prefix("named-endpoint:"))
                .map(str::trim);
            match endpoint {
                Some(ep) if !ep.is_empty() && !ep.chars().any(char::is_control) => {
                    Ok(ModelRoutePolicy::NamedEndpoint(ep.to_owned()))
                }
                Some(_) => Err(KpError::invalid(
                    "model_policy",
                    "named_endpoint policy requires a nonempty endpoint identifier",
                )
                .into()),
                None => Err(KpError::invalid(
                    "model_policy",
                    "expected `inherit`, `raw_only`, or `named_endpoint:<id>`",
                )
                .into()),
            }
        }
    }
}

/// Parses a CLI `--identity <provider>:<external_id>=<actor_id>` confirmed identity mapping.
pub fn parse_identity_mapping(raw: &str) -> Result<((String, String), ActorId)> {
    let trimmed = raw.trim();
    let Some((lhs, rhs)) = trimmed.split_once('=') else {
        return Err(
            KpError::invalid("identity", "expected `<provider>:<external_id>=<actor_id>`").into(),
        );
    };
    let Some((provider, ext_id)) = lhs.trim().split_once(':') else {
        return Err(
            KpError::invalid("identity", "expected `<provider>:<external_id>` before `=`").into(),
        );
    };
    let clean_provider = provider.trim();
    let clean_ext = ext_id.trim();
    if clean_provider.is_empty() || clean_ext.is_empty() {
        return Err(
            KpError::invalid("identity", "provider and external_id must be nonempty").into(),
        );
    }
    let actor: ActorId = rhs.trim().parse()?;
    Ok(((clean_provider.to_owned(), clean_ext.to_owned()), actor))
}

/// Parameters for registering or updating a connector scope via [`upsert_scope_in_state`].
#[derive(Clone, Debug)]
pub struct ScopeUpsertSpec<'a> {
    pub kind: ConnectorKind,
    pub connector_id: Option<&'a str>,
    pub source_scope_id: &'a str,
    pub upstream_container_id: &'a str,
    pub label: &'a str,
    pub model_policy: Option<ModelRoutePolicy>,
    pub detected_global_model_label: &'a str,
}

/// Registers or updates a [`ConnectorScope`] in `state_store`, defaulting `model_policy` to
/// [`ConnectorKind::default_model_policy`] (`ModelRoutePolicy::HostCli`, inheriting the global
/// model setting) and returning the one-line [`feishu_enable_notice`] when enabling a Feishu scope
/// under global model inheritance (`Rev 5 §0.2.2`).
pub fn upsert_scope_in_state(
    state_store: &ConnectorStateStore,
    spec: &ScopeUpsertSpec<'_>,
) -> Result<(ConnectorScope, Option<String>)> {
    let ScopeUpsertSpec {
        kind,
        connector_id,
        source_scope_id,
        upstream_container_id,
        label,
        ref model_policy,
        detected_global_model_label,
    } = *spec;
    let model_policy = model_policy.clone();
    let conn_id: ConnectorId = match connector_id.map(str::trim).filter(|s| !s.is_empty()) {
        Some(explicit) => explicit.parse()?,
        None => kind.as_str().parse()?,
    };
    let scope_id: SourceScopeId = source_scope_id.trim().parse()?;
    let clean_upstream = upstream_container_id.trim();
    if clean_upstream.is_empty() || clean_upstream.chars().any(char::is_control) {
        return Err(KpError::invalid(
            "upstream_container_id",
            "upstream container identifier must be nonempty",
        )
        .into());
    }
    validate_upstream_for_kind(kind, clean_upstream)?;

    let clean_label = {
        let trimmed = label.trim();
        if trimmed.is_empty() {
            clean_upstream.to_owned()
        } else if trimmed.chars().any(char::is_control) {
            return Err(
                KpError::invalid("label", "scope label cannot contain control characters").into(),
            );
        } else {
            trimmed.to_owned()
        }
    };

    let effective_policy = model_policy.unwrap_or_else(|| kind.default_model_policy());
    let notice = if kind == ConnectorKind::Feishu && effective_policy == ModelRoutePolicy::HostCli {
        Some(feishu_enable_notice(detected_global_model_label))
    } else {
        None
    };

    let scope = ConnectorScope {
        connector_id: conn_id,
        source_scope_id: scope_id,
        upstream_container_id: clean_upstream.to_owned(),
        label: clean_label,
        enabled: true,
        model_policy: effective_policy,
    };

    let mut snapshot = state_store.load()?;
    let key = connector_scope_state_key(&scope.connector_id, &scope.source_scope_id);
    snapshot.scopes.insert(key.clone(), scope.clone());
    snapshot.scope_kinds.insert(key, kind);
    state_store.save(&snapshot)?;

    Ok((scope, notice))
}

/// Lists all configured [`ConnectorScope`]s in deterministic key order.
pub fn list_scopes_in_state(state_store: &ConnectorStateStore) -> Result<Vec<ConnectorScope>> {
    state_store.list_scopes()
}

/// Enables or disables an existing `(connector_id, source_scope_id)` in `state_store`.
pub fn set_scope_enabled_in_state(
    state_store: &ConnectorStateStore,
    connector_id: &ConnectorId,
    source_scope_id: &SourceScopeId,
    enabled: bool,
) -> Result<ConnectorScope> {
    let mut snapshot = state_store.load()?;
    let key = connector_scope_state_key(connector_id, source_scope_id);
    let Some(scope) = snapshot.scopes.get_mut(&key) else {
        return Err(KpError::invalid("scope", "configured connector scope not found").into());
    };
    scope.enabled = enabled;
    let updated = scope.clone();
    state_store.save(&snapshot)?;
    Ok(updated)
}

/// Polls all enabled [`ConnectorScope`]s (optionally filtered by `filter_kind`), encoding every
/// [`crate::PollBatch`] into standard Public Ingest Contract JSONL (`PublicContainerRoster` lines
/// ahead of `PublicIngestRecord` lines) and staging `next_cursor`s in memory **without** mutating
/// `state.json`.
pub async fn poll_enabled_scopes_staged(
    state_store: &ConnectorStateStore,
    filter_kind: Option<ConnectorKind>,
    ingested_by: &ActorId,
    confirmed_identities: &BTreeMap<(String, String), ActorId>,
    now: EpochMs,
) -> Result<StagedConnectorSync> {
    let snapshot = state_store.load()?;
    let mut staged = StagedConnectorSync::default();

    for (state_key, scope) in &snapshot.scopes {
        if !scope.enabled {
            continue;
        }
        let kind = resolve_scope_kind(&snapshot, state_key, scope)?;
        if let Some(expected_kind) = filter_kind {
            if kind != expected_kind {
                continue;
            }
        }

        let cursor = snapshot.cursors.get(state_key).cloned().unwrap_or_default();

        let batch = match kind {
            ConnectorKind::Feishu => {
                let connector = FeishuConnector::new(
                    LarkCliTransport::default(),
                    FeishuSyncConfig {
                        now,
                        ..FeishuSyncConfig::default()
                    },
                );
                connector
                    .poll_incremental(scope, &cursor, ingested_by)
                    .await?
            }
            ConnectorKind::GitHub => {
                let primary = ReqwestGitHubTransport::new(None)?;
                let fallback = GhCliFallbackTransport::default();
                let prefer_fallback = should_prefer_gh_cli_fallback(&primary);
                let chain = GitHubTransportChain::new(primary, fallback, prefer_fallback);
                let connector = GitHubConnector::new(
                    chain,
                    GitHubSyncConfig {
                        now,
                        ..GitHubSyncConfig::default()
                    },
                );
                connector
                    .poll_incremental(scope, &cursor, ingested_by)
                    .await?
            }
            ConnectorKind::AgentSession => {
                let connector = AgentSessionConnector::new(SessionBackfillConfig {
                    now,
                    ..SessionBackfillConfig::default()
                });
                connector
                    .poll_incremental(scope, &cursor, ingested_by)
                    .await?
            }
        };

        let scope_jsonl = batch.to_public_jsonl_string(kind, confirmed_identities)?;
        staged.jsonl.push_str(&scope_jsonl);
        staged
            .staged_cursors
            .insert(state_key.clone(), batch.next_cursor);
        staged.scopes_polled += 1;
        staged.rosters_emitted += batch.scope_rosters.len();
        staged.records_emitted += batch.events.len();
        staged.skipped_low_signal += batch.skipped_low_signal;
    }

    Ok(staged)
}

/// Atomically commits `staged_cursors` into `0600` `state.json` after downstream `kp ingest` succeeds.
pub fn commit_staged_cursors(
    state_store: &ConnectorStateStore,
    staged_cursors: &BTreeMap<String, ConnectorCursor>,
) -> Result<usize> {
    if staged_cursors.is_empty() {
        return Ok(0);
    }
    let mut snapshot = state_store.load()?;
    for (key, cursor) in staged_cursors {
        snapshot.cursors.insert(key.clone(), cursor.clone());
    }
    state_store.save(&snapshot)?;
    Ok(staged_cursors.len())
}

/// Streams `jsonl` into `<kp_bin> --path <library_path> --as <ingested_by> --json ingest -`,
/// returning `Err(ConnectError::Transport)` on any non-zero exit status so staged cursors stay unadvanced.
pub fn pipe_jsonl_to_kp_ingest(
    kp_bin: &Path,
    library_path: &Path,
    ingested_by: &ActorId,
    jsonl: &str,
) -> Result<CommandOutput> {
    let mut child = Command::new(kp_bin)
        .arg("--path")
        .arg(library_path)
        .arg("--as")
        .arg(ingested_by.as_str())
        .args(["--json", "ingest", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        if let Err(err) = stdin
            .write_all(jsonl.as_bytes())
            .and_then(|()| stdin.flush())
        {
            if err.kind() != std::io::ErrorKind::BrokenPipe {
                return Err(ConnectError::Io(err));
            }
        }
    }

    let output = child.wait_with_output()?;
    let status_code = output.status.code().unwrap_or(-1);
    if status_code != 0 {
        let stderr_msg = String::from_utf8_lossy(&output.stderr);
        return Err(ConnectError::Transport(format!(
            "`kp ingest -` exited with status {status_code}: {}",
            stderr_msg.trim()
        )));
    }

    Ok(CommandOutput {
        status_code,
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

fn validate_upstream_for_kind(kind: ConnectorKind, upstream: &str) -> Result<()> {
    match kind {
        ConnectorKind::Feishu => {
            let _ = FeishuTarget::parse(upstream)?;
            Ok(())
        }
        ConnectorKind::GitHub => {
            let _ = parse_owner_repo(upstream)?;
            Ok(())
        }
        ConnectorKind::AgentSession => {
            let (_, path) = AgentSessionConnector::parse_upstream_target(upstream)?;
            if path.as_os_str().is_empty() {
                return Err(KpError::invalid(
                    "upstream_container_id",
                    "agent-session upstream path must be nonempty",
                )
                .into());
            }
            Ok(())
        }
    }
}

fn should_prefer_gh_cli_fallback(primary: &ReqwestGitHubTransport) -> bool {
    let has_api_base_override = std::env::var_os(ENV_GITHUB_API_BASE)
        .is_some_and(|v| !v.to_string_lossy().trim().is_empty());
    if has_api_base_override || primary.has_token() {
        return false;
    }
    std::env::var_os(ENV_GH_CLI_BIN).is_some_and(|v| !v.to_string_lossy().trim().is_empty())
}

fn resolve_scope_kind(
    snapshot: &ConnectorStateSnapshot,
    state_key: &str,
    scope: &ConnectorScope,
) -> Result<ConnectorKind> {
    if let Some(&kind) = snapshot.scope_kinds.get(state_key) {
        return Ok(kind);
    }

    let conn = scope.connector_id.as_str().to_ascii_lowercase();
    let sid = scope.source_scope_id.as_str().to_ascii_lowercase();
    let upstream = scope.upstream_container_id.trim();

    if conn.starts_with("feishu")
        || conn.starts_with("lark")
        || sid.starts_with("feishu")
        || upstream.starts_with("chat:")
        || upstream.starts_with("wiki:")
        || upstream.starts_with("docx:")
        || upstream.starts_with("doc:")
        || upstream.starts_with("minutes:")
        || upstream.starts_with("calendar:")
    {
        return Ok(ConnectorKind::Feishu);
    }

    if conn.starts_with("agent-session")
        || conn.starts_with("agent_session")
        || conn.starts_with("session")
        || sid.starts_with("session")
        || sid.starts_with("agent-session")
        || upstream.starts_with("claude-code:")
        || upstream.starts_with("codex:")
        || upstream.starts_with("antigravity:")
        || upstream.ends_with(".jsonl")
    {
        return Ok(ConnectorKind::AgentSession);
    }

    if conn.starts_with("github")
        || conn.starts_with("gh")
        || sid.starts_with("github")
        || upstream.starts_with("github:")
    {
        return Ok(ConnectorKind::GitHub);
    }

    if Path::new(upstream).exists() {
        return Ok(ConnectorKind::AgentSession);
    }
    if parse_owner_repo(upstream).is_ok() {
        return Ok(ConnectorKind::GitHub);
    }

    Err(ConnectError::Transport(format!(
        "unable to infer connector kind for scope `{state_key}` (`{upstream}`)"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_management_and_policy_parsers_enforce_invariants_and_feishu_notice() -> Result<()> {
        assert_eq!(
            parse_connector_model_policy("inherit")?,
            ModelRoutePolicy::HostCli
        );
        assert_eq!(
            parse_connector_model_policy("raw_only")?,
            ModelRoutePolicy::RawOnly
        );
        assert_eq!(
            parse_connector_model_policy("named_endpoint:corp-gw")?,
            ModelRoutePolicy::NamedEndpoint("corp-gw".into())
        );
        assert!(parse_connector_model_policy("named_endpoint:").is_err());
        assert!(parse_connector_model_policy("unknown_mode").is_err());

        let ((prov, ext), actor) = parse_identity_mapping("feishu:ou_123=eng_alice")?;
        assert_eq!(prov, "feishu");
        assert_eq!(ext, "ou_123");
        assert_eq!(actor.as_str(), "eng_alice");
        assert!(parse_identity_mapping("missing_equals").is_err());

        let temp = tempfile::tempdir()?;
        let store = ConnectorStateStore::new(temp.path().join("state.json"));

        // Feishu with default (`None` -> `HostCli`) returns single-line `feishu_enable_notice`.
        let (fs_scope, fs_notice) = upsert_scope_in_state(
            &store,
            &ScopeUpsertSpec {
                kind: ConnectorKind::Feishu,
                connector_id: None,
                source_scope_id: "feishu.chat.oc_01",
                upstream_container_id: "chat:oc_01",
                label: "研发群",
                model_policy: None,
                detected_global_model_label: "Codex / OpenAI",
            },
        )?;
        assert_eq!(fs_scope.connector_id.as_str(), "feishu");
        assert_eq!(fs_scope.model_policy, ModelRoutePolicy::HostCli);
        assert_eq!(fs_notice, Some(feishu_enable_notice("Codex / OpenAI")));

        // Feishu with explicit `RawOnly` override does not print the global-model notice.
        let (_, raw_notice) = upsert_scope_in_state(
            &store,
            &ScopeUpsertSpec {
                kind: ConnectorKind::Feishu,
                connector_id: None,
                source_scope_id: "feishu.chat.oc_02",
                upstream_container_id: "chat:oc_02",
                label: "合规群",
                model_policy: Some(ModelRoutePolicy::RawOnly),
                detected_global_model_label: "Codex / OpenAI",
            },
        )?;
        assert!(raw_notice.is_none());

        // Disable & re-enable roundtrip.
        let disabled = set_scope_enabled_in_state(
            &store,
            &"feishu".parse()?,
            &"feishu.chat.oc_01".parse()?,
            false,
        )?;
        assert!(!disabled.enabled);
        assert_eq!(list_scopes_in_state(&store)?.len(), 2);

        Ok(())
    }
}
