use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use kp_model::{ActorId, EpochMs, FactId, KpError, LibraryId, LibraryMode};
#[cfg(unix)]
use kp_net::DaemonSyncClient;
use kp_net::{
    auto_publish_and_sync, load_joined_communities, remove_joined_community,
    upsert_joined_community, CommunityClient, JoinedCommunity, PublishOptions, SyncResult,
};
use kp_store::Store;
use serde_json::json;

use crate::{now, output, Cli};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Subcommand)]
pub enum CommunityManageCommand {
    /// Save a community endpoint and bearer token (`0600` `communities.json`) and pull initial peer roster.
    Join {
        url: String,
        #[arg(long)]
        token: String,
        #[cfg(unix)]
        #[arg(long, hide = true)]
        unix_socket: Option<PathBuf>,
    },
    /// List joined communities (tokens redacted).
    List,
    /// Remove a joined community from `communities.json`.
    Leave { url: String },
}

#[derive(Subcommand)]
pub enum CommunityCommand {
    /// Manage joined communities (`join`, `list`, `leave`) for automatic background sharing.
    Community {
        #[command(subcommand)]
        command: CommunityManageCommand,
    },
    /// Run deterministic Datalog derivation and JTMS maintenance rules over active facts.
    Derive,
    /// Generate a scoped synthesis derived from visible facts for a given query and audience.
    Synthesize {
        #[arg(long)]
        query: String,
        #[arg(long)]
        audience: String,
        #[arg(long)]
        at: Option<i64>,
    },
    /// Explain why a fact is remembered with its provenance semiring proof tree and L0 quotes.
    Why {
        fact_id: FactId,
        #[arg(long)]
        at: Option<i64>,
    },
    /// Serve a shared Community Library over HTTP REST, Streamable HTTP MCP, and A2A.
    Serve {
        #[arg(long)]
        community: bool,
        #[arg(long, default_value = "127.0.0.1:8080")]
        listen: SocketAddr,
    },
    /// Publish a local fact or skill to a remote Community Library via the durable outbox.
    Publish {
        fact_id_or_key: String,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        token: Option<String>,
        #[arg(long)]
        audience: Option<String>,
        #[arg(long)]
        map_predicate: Option<String>,
    },
    /// Pull viewer-isolated snapshots and flush outbox deliveries with joined communities.
    Sync {
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        token: Option<String>,
        #[arg(long)]
        verify_origin_url: Vec<String>,
    },
}

pub async fn run(cli: &Cli, command: &CommunityCommand) -> Result<()> {
    let at = now()?;
    if let CommunityCommand::Why {
        fact_id,
        at: requested,
    } = command
    {
        let store = Store::open_readonly(&cli.path)?;
        let actor = cli
            .actor
            .clone()
            .unwrap_or_else(|| store.owner_actor().clone());
        let eval_at = requested.map(EpochMs).unwrap_or(at);
        let resolved = store.resolve_visible_or_raw_fact_id(&actor, eval_at, fact_id.as_str())?;
        let proof = kp_derive::why::why(&store, &resolved, &actor, eval_at)?;
        output(cli.json, &proof, &serde_json::to_string_pretty(&proof)?)?;
        return Ok(());
    }

    if let CommunityCommand::Community {
        command: CommunityManageCommand::List,
    } = command
    {
        let _ = Store::open_readonly(&cli.path)?;
        let joined = load_joined_communities(&cli.path)?;
        let redacted: Vec<_> = joined
            .iter()
            .map(|c| {
                json!({
                    "url": c.url,
                    "library_id": c.library_id,
                    "joined_at": c.joined_at,
                })
            })
            .collect();
        let human = if redacted.is_empty() {
            "No joined communities".to_owned()
        } else {
            joined
                .iter()
                .map(|c| match &c.library_id {
                    Some(id) => format!("{} ({id})", c.url),
                    None => c.url.clone(),
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        output(cli.json, &json!({ "communities": redacted }), &human)?;
        return Ok(());
    }

    let mut store = Store::open(&cli.path)?;
    let actor = cli
        .actor
        .clone()
        .unwrap_or_else(|| store.owner_actor().clone());
    match command {
        CommunityCommand::Community { command: sub } => match sub {
            CommunityManageCommand::Join {
                url,
                token,
                #[cfg(unix)]
                unix_socket,
            } => {
                let mut entry = JoinedCommunity {
                    url: url.clone(),
                    token: token.clone(),
                    library_id: None,
                    #[cfg(unix)]
                    unix_socket: unix_socket.clone(),
                    joined_at: at,
                };
                let client = entry.to_client()?;
                let status_val = client.get(&format!("/v1/status?at={at}")).await?;
                let remote_actor = status_val
                    .get("actor")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        KpError::invalid(
                            "status",
                            "community /v1/status response missing authenticated actor",
                        )
                    })?;
                let expected_owner = store.owner_actor();
                if remote_actor != actor.as_str() || remote_actor != expected_owner.as_str() {
                    return Err(format!(
                        "invalid token: community token authenticates as '{remote_actor}', expected local actor '{expected_owner}'"
                    )
                    .into());
                }
                if let Some(id_str) = status_val.get("library_id").and_then(|v| v.as_str()) {
                    entry.library_id = id_str.parse::<LibraryId>().ok();
                }
                let canonical_url = client.url().to_owned();
                entry.url = canonical_url.clone();
                upsert_joined_community(&cli.path, entry)?;
                crate::integrations::enable_daemon_autostart(&cli.path);

                let sync_res = auto_publish_and_sync(&mut store, &client, at)
                    .await
                    .unwrap_or_default();
                let _ = crate::governance::run_concept_alignment_and_derive(&mut store, None, at)
                    .await?;
                #[cfg(unix)]
                if let Ok(dclient) = DaemonSyncClient::for_library(&cli.path) {
                    let _ = dclient.trigger_sync();
                }

                let val = json!({
                    "joined": true,
                    "url": canonical_url,
                    "mirrored": sync_res.mirrored,
                    "delivered": sync_res.delivered,
                });
                output(cli.json, &val, &format!("Joined community {canonical_url}"))?;
            }
            CommunityManageCommand::Leave { url } => {
                let removed = remove_joined_community(&cli.path, url)?;
                let val = json!({ "removed": removed, "url": url });
                output(
                    cli.json,
                    &val,
                    if removed {
                        "Left community"
                    } else {
                        "Community not found"
                    },
                )?;
            }
            CommunityManageCommand::List => unreachable!("handled via read-only path above"),
        },
        CommunityCommand::Derive => {
            let result =
                crate::governance::run_concept_alignment_and_derive(&mut store, None, at).await?;
            let payload = json!({
                "derived": result.derived_warnings,
                "justifications": result.justifications,
                "superseded_contradicted_events": result.superseded_contradicted_events,
                "emitted_reviews": result.emitted_reviews,
            });
            output(
                cli.json,
                &payload,
                &format!(
                    "Derived {} facts with {} proofs",
                    result.derived_warnings, result.justifications
                ),
            )?;
        }
        CommunityCommand::Synthesize {
            query,
            audience,
            at: requested,
        } => {
            let actors: BTreeSet<ActorId> = audience
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::parse)
                .collect::<kp_model::Result<_>>()?;
            let ids = kp_derive::synthesize_scoped(
                &mut store,
                &actor,
                &actors,
                query,
                requested.map(EpochMs).unwrap_or(at),
                &kp_derive::StructuredSynthesizer,
            )
            .await?;
            output(
                cli.json,
                &json!({"facts":ids}),
                &format!("Synthesized {} claims", ids.len()),
            )?;
        }
        CommunityCommand::Why { .. } => unreachable!("handled via read-only path above"),
        CommunityCommand::Serve { community, listen } => {
            if *community && store.status()?.mode != LibraryMode::Community {
                return Err(
                    KpError::invalid("mode", "--community requires a community library").into(),
                );
            }
            kp_derive::derive(&mut store)?;
            let listener = tokio::net::TcpListener::bind(listen).await?;
            eprintln!("kp listening on {}", listener.local_addr()?);
            axum::serve(listener, kp_net::router(store)).await?;
        }
        CommunityCommand::Publish {
            fact_id_or_key,
            to,
            token,
            audience,
            map_predicate,
        } => {
            let options = PublishOptions {
                audience: audience
                    .as_deref()
                    .map(|s| crate::memory::audience(Some(s), &actor))
                    .transpose()?,
                map_predicate: map_predicate.clone(),
            };
            match (to.as_deref(), token.as_deref()) {
                (Some(to_url), Some(tok)) => {
                    let client = CommunityClient::new(to_url, tok)?;
                    let result =
                        kp_net::publish(&mut store, fact_id_or_key, &client, &options, at).await?;
                    output(
                        cli.json,
                        &result,
                        if result.queued {
                            "Queued for delivery"
                        } else {
                            "Published"
                        },
                    )?;
                }
                (None, None) => {
                    let joined = load_joined_communities(&cli.path)?;
                    if joined.is_empty() {
                        return Err(KpError::invalid(
                            "to",
                            "pass --to and --token or join a community first with `kp community join`",
                        )
                        .into());
                    }
                    let mut last_res = None;
                    for entry in &joined {
                        let client = entry.to_client()?;
                        last_res = Some(
                            kp_net::publish(&mut store, fact_id_or_key, &client, &options, at)
                                .await?,
                        );
                    }
                    if let Some(res) = last_res {
                        output(
                            cli.json,
                            &res,
                            if res.queued {
                                "Queued for delivery"
                            } else {
                                "Published"
                            },
                        )?;
                    }
                }
                _ => {
                    return Err(KpError::invalid(
                        "token",
                        "pass both --to and --token, or omit both to use joined communities",
                    )
                    .into());
                }
            }
        }
        CommunityCommand::Sync {
            from,
            token,
            verify_origin_url,
        } => match (from.as_deref(), token.as_deref()) {
            (Some(from_url), Some(tok)) => {
                let client = CommunityClient::new(from_url, tok)?;
                let mut origins = BTreeMap::new();
                for mapping in verify_origin_url {
                    let (origin, url) = mapping.split_once('=').ok_or_else(|| {
                        KpError::invalid("verify-origin-url", "expected library_id=url")
                    })?;
                    origins.insert(origin.parse()?, CommunityClient::new(url, tok)?);
                }
                // Verification happens even if the community pull fails: hide first while offline.
                let result = kp_net::sync(&mut store, &client, at).await;
                kp_net::reconcile_remote_origins(&mut store, &origins, at).await?;
                let result = result?;
                let _ = crate::governance::run_concept_alignment_and_derive(&mut store, None, at)
                    .await?;
                output(
                    cli.json,
                    &result,
                    &format!(
                        "Mirrored {} facts; {} delivered, {} pending",
                        result.mirrored, result.delivered, result.pending
                    ),
                )?;
            }
            (None, None) => {
                let joined = load_joined_communities(&cli.path)?;
                if joined.is_empty() {
                    return Err(KpError::invalid(
                        "from",
                        "pass --from and --token or join a community first with `kp community join`",
                    )
                    .into());
                }
                let mut agg = SyncResult {
                    unchanged: true,
                    ..SyncResult::default()
                };
                for entry in &joined {
                    let client = entry.to_client()?;
                    let res = auto_publish_and_sync(&mut store, &client, at).await?;
                    agg.delivered += res.delivered;
                    agg.pending += res.pending;
                    agg.mirrored += res.mirrored;
                    agg.unchanged &= res.unchanged;
                }
                let _ = crate::governance::run_concept_alignment_and_derive(&mut store, None, at)
                    .await?;
                output(
                    cli.json,
                    &agg,
                    &format!(
                        "Mirrored {} facts; {} delivered, {} pending",
                        agg.mirrored, agg.delivered, agg.pending
                    ),
                )?;
            }
            _ => {
                return Err(KpError::invalid(
                    "token",
                    "pass both --from and --token, or omit both to sync joined communities",
                )
                .into());
            }
        },
    }
    Ok(())
}

/// Pushes unshared multi-actor local facts and pulls updates across all `communities.json` entries,
/// then runs concept alignment & derivation when changes arrive or local facts were delivered.
pub(crate) async fn sync_joined_communities_and_derive(
    library_dir: &Path,
    store: &mut Store,
    at: EpochMs,
) -> Result<usize> {
    let joined = load_joined_communities(library_dir)?;
    if joined.is_empty() {
        if store.needs_alignment()? {
            let _ = crate::governance::run_concept_alignment_and_derive(store, None, at).await?;
        }
        return Ok(0);
    }
    let mut total_mirrored = 0usize;
    let mut any_change = false;
    for entry in &joined {
        let Ok(client) = entry.to_client() else {
            continue;
        };
        if let Ok(res) = auto_publish_and_sync(store, &client, at).await {
            total_mirrored += res.mirrored;
            if !res.unchanged || res.mirrored > 0 || res.delivered > 0 {
                any_change = true;
            }
        }
    }
    if any_change || store.needs_alignment()? {
        let _ = crate::governance::run_concept_alignment_and_derive(store, None, at).await?;
    }
    Ok(total_mirrored)
}

/// Immediately flushes joined-community publications/withdrawals after local governance actions
/// (`kp review resolve`, `kp reject`), and wakes any running local daemon.
pub(crate) async fn notify_or_sync_joined(library_dir: &Path, store: &mut Store, at: EpochMs) {
    let _ = sync_joined_communities_and_derive(library_dir, store, at).await;
    #[cfg(unix)]
    if let Ok(dclient) = DaemonSyncClient::for_library(library_dir) {
        let _ = dclient.trigger_sync();
    }
}
