mod daemon_handler;
mod hook_exec;
#[cfg(all(test, unix))]
mod tests;

use std::error::Error;
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;

use clap::{Subcommand, ValueEnum};
#[cfg(unix)]
use kp_net::{DaemonClient, DaemonConfig, DaemonServer, WebInspectorConfig};
use kp_store::Store;
#[cfg(unix)]
use kp_store::StoreError;
use serde_json::json;

pub(crate) use daemon_handler::enable_daemon_autostart;
#[cfg(all(test, unix))]
use daemon_handler::maybe_autospawn_daemon_from_hook;
#[cfg(unix)]
pub use daemon_handler::CliDaemonHandler;
#[cfg(unix)]
use daemon_handler::{disable_daemon_autostart, ensure_daemon_started, SPAWNING_LOCK_FILE};
#[cfg(all(test, unix))]
use hook_exec::execute_hook_with_input;
pub use hook_exec::run_hook_infallible;

use crate::{now, output, Cli};

#[derive(Clone, Copy, ValueEnum)]
pub enum Client {
    ClaudeCode,
    Codex,
    Cursor,
    Windsurf,
}

impl Client {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
            Self::Windsurf => "windsurf",
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Event {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    Stop,
    SessionEnd,
}

impl Event {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "session-start",
            Self::UserPromptSubmit => "user-prompt-submit",
            Self::PreToolUse => "pre-tool-use",
            Self::PostToolUse => "post-tool-use",
            Self::Stop => "stop",
            Self::SessionEnd => "session-end",
        }
    }
}

#[derive(Subcommand)]
pub enum DaemonCommand {
    /// Run the local UDS daemon in the foreground (`kp daemon stop` shuts it down cleanly).
    Run {
        #[arg(long, default_value_t = 5)]
        idle_secs: u64,
        /// Enable the built-in `127.0.0.1` read-only 4-column Web Inspector.
        #[arg(long)]
        web: bool,
        /// Optional `127.0.0.1` TCP port for the read-only Web Inspector (`0` selects an ephemeral port).
        #[arg(long = "web-port")]
        web_port: Option<u16>,
    },
    /// Spawn the local UDS daemon in the background if not already running.
    Start {
        #[arg(long, default_value_t = 5)]
        idle_secs: u64,
        /// Enable (or attach to) the built-in `127.0.0.1` read-only 4-column Web Inspector.
        #[arg(long)]
        web: bool,
        /// Optional `127.0.0.1` TCP port for the read-only Web Inspector (`0` selects an ephemeral port).
        #[arg(long = "web-port")]
        web_port: Option<u16>,
    },
    /// Query status of the running local UDS daemon.
    Status,
    /// Gracefully stop the running local UDS daemon.
    Stop,
}

#[derive(Subcommand)]
pub enum IntegrationCommand {
    /// Install non-intrusive Kipo hooks or MCP configuration into Claude Code, Codex, Cursor, or Windsurf.
    Connect {
        #[arg(value_enum)]
        client: Client,
        #[arg(long, default_value = ".")]
        project: PathBuf,
        /// Write user-level MCP configuration (`~/.cursor/mcp.json` or `~/.codeium/windsurf/mcp_config.json`).
        #[arg(long)]
        global: bool,
    },
    /// Handle a coding-agent lifecycle hook event (`session-start`, `user-prompt-submit`, `pre-tool-use`, etc.).
    Hook {
        #[arg(value_enum)]
        event: Event,
        #[arg(long, value_enum)]
        client: Option<Client>,
    },
    /// Run, start, inspect status, or stop the local Unix-socket `kp daemon`.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Launch or attach to the `127.0.0.1`-only read-only 4-column Web Inspector for this library.
    Inspect {
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
    /// Re-verify `tree-sitter` code symbol anchors against the current repository working tree.
    Verify {
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Serve the 3-tool read/note stdio MCP server (`kp_search`, `kp_note`, `kp_why`).
    Mcp,
}

#[cfg(unix)]
async fn run_daemon(cli: &Cli, command: &DaemonCommand) -> Result<(), Box<dyn Error>> {
    match command {
        DaemonCommand::Run {
            idle_secs,
            web,
            web_port,
        } => {
            let mut store = Store::open(&cli.path)?;
            let library_dir = cli.path.canonicalize().unwrap_or_else(|_| cli.path.clone());
            // Warm the static Jieba base dictionary once at daemon startup so CJK hooks stay <15ms.
            let _ = kp_store::Tokenizer::shared();
            let (swappable, pending_local_model) =
                crate::providers_cli::build_daemon_swappable_embedder(&store);
            if swappable.configured_model().eq_ignore_ascii_case("none") {
                store.set_embedder(Arc::new(kp_store::FtsOnlyEmbedder));
            } else {
                store.set_embedder(
                    Arc::clone(&swappable) as Arc<dyn kp_store::Embedder + Send + Sync>
                );
            }
            let mut config = DaemonConfig::for_library(&library_dir)?;
            config.idle_extract_window = Duration::from_secs(*idle_secs);
            if let Some(exit_secs) = std::env::var("KP_DAEMON_IDLE_EXIT_SECS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|&s| s > 0)
            {
                config.idle_shutdown_timeout = Some(Duration::from_secs(exit_secs));
            }
            if *web || web_port.is_some() {
                let viewer = cli
                    .actor
                    .clone()
                    .unwrap_or_else(|| store.owner_actor().clone());
                if store.member_role(&viewer)?.is_none() {
                    return Err("actor is not an active library member".into());
                }
                config.web_inspector =
                    Some(WebInspectorConfig::loopback(web_port.unwrap_or(0), viewer));
            }
            let mut handle = DaemonServer::spawn(
                store,
                config,
                Arc::new(CliDaemonHandler::with_embedder(Arc::clone(&swappable))),
            )
            .await?;
            if let Some(local_model_id) = pending_local_model {
                crate::providers_cli::spawn_background_local_onnx_warmup(
                    library_dir.clone(),
                    local_model_id,
                    swappable,
                );
            }
            let _ = std::fs::remove_file(cli.path.join(SPAWNING_LOCK_FILE));
            let _ = std::fs::remove_file(library_dir.join(SPAWNING_LOCK_FILE));
            if !cli.json {
                eprintln!("kp daemon listening on {}", handle.socket_path().display());
                if let Some(url) = handle.web_inspector_url() {
                    eprintln!("Web Inspector (read-only 127.0.0.1): {url}");
                }
            }
            handle.wait().await;
            handle.shutdown().await?;
            Ok(())
        }
        DaemonCommand::Start {
            idle_secs,
            web,
            web_port,
        } => {
            let (client, sock, already_running) = ensure_daemon_started(cli, *idle_secs).await?;
            let mut val = json!({
                "status": "running",
                "already_running": already_running,
                "socket_path": sock,
            });
            let mut human = format!("kp daemon running on {}", sock.display());
            if *web || web_port.is_some() {
                let sess = client
                    .with_timeout(Duration::from_secs(3))
                    .start_web_inspector(web_port.unwrap_or(0), cli.actor.clone())
                    .await?;
                val["web_inspector_url"] = json!(sess.url);
                val["web_inspector_bind"] = json!(sess.bind_addr);
                val["web_inspector_viewer"] = json!(sess.viewer);
                human.push_str(&format!(
                    "\nWeb Inspector (read-only 127.0.0.1, viewer `{}`): {}",
                    sess.viewer, sess.url
                ));
            }
            output(cli.json, &val, &human)?;
            Ok(())
        }
        DaemonCommand::Status => {
            if let Some(ref actor) = cli.actor {
                let ro = Store::open_readonly(&cli.path)?;
                if ro.member_role(actor)?.is_none() {
                    return Err(StoreError::PermissionDenied.into());
                }
            }
            let client = DaemonClient::for_library(&cli.path)?.with_timeout(Duration::from_secs(2));
            let mut status = client.status().await?;
            if let Some(ref actor) = cli.actor {
                if status.web_inspector_url.is_some()
                    && status.web_inspector_viewer.as_ref() != Some(actor)
                {
                    let sess = client
                        .with_timeout(Duration::from_secs(2))
                        .start_web_inspector(0, Some(actor.clone()))
                        .await?;
                    status.web_inspector_url = Some(sess.url);
                    status.web_inspector_viewer = Some(sess.viewer);
                }
            }
            let mut human = format!(
                "{} ({}, {:?})\nfacts={} sources={} tracked_sessions={} pending_extractions={} uptime_ms={} embedder={}({})",
                status.name,
                status.library_id,
                status.mode,
                status.facts,
                status.sources,
                status.tracked_sessions,
                status.pending_extractions,
                status.uptime_ms,
                status.embedder.model,
                status.embedder.state,
            );
            if let Some(ref url) = status.web_inspector_url {
                human.push_str(&format!("\nweb_inspector={url}"));
            }
            output(cli.json, &status, &human)?;
            Ok(())
        }
        DaemonCommand::Stop => {
            disable_daemon_autostart(&cli.path);
            let client = DaemonClient::for_library(&cli.path)?.with_timeout(Duration::from_secs(2));
            let sock = client.socket_path().to_path_buf();
            match client.shutdown().await {
                Ok(()) | Err(kp_net::NetError::Unavailable) => {
                    let val = json!({
                        "status": "stopped",
                        "socket_path": sock,
                    });
                    output(cli.json, &val, "kp daemon stopped")?;
                    Ok(())
                }
                Err(err) => Err(err.into()),
            }
        }
    }
}

#[cfg(not(unix))]
async fn run_daemon(_cli: &Cli, _command: &DaemonCommand) -> Result<(), Box<dyn Error>> {
    Err("local UDS daemon and web inspector are only supported on Unix platforms".into())
}

#[cfg(unix)]
async fn run_inspect(cli: &Cli, port: u16) -> Result<(), Box<dyn Error>> {
    let (client, sock, already_running) = ensure_daemon_started(cli, 5).await?;
    let sess = client
        .with_timeout(Duration::from_secs(3))
        .start_web_inspector(port, cli.actor.clone())
        .await?;
    let val = json!({
        "url": sess.url,
        "bind_addr": sess.bind_addr,
        "viewer": sess.viewer,
        "socket_path": sock,
        "daemon_already_running": already_running,
    });
    let human = format!(
        "Web Inspector ready (read-only 127.0.0.1 · viewer `{}`):\n  {}\n(Edits remain lossless Markdown round-trip: `kp export <dir>` -> edit -> `kp apply <dir>`)",
        sess.viewer, sess.url
    );
    output(cli.json, &val, &human)?;
    Ok(())
}

#[cfg(not(unix))]
async fn run_inspect(_cli: &Cli, _port: u16) -> Result<(), Box<dyn Error>> {
    Err("local UDS daemon and web inspector are only supported on Unix platforms".into())
}

pub async fn run(cli: &Cli, command: &IntegrationCommand) -> Result<(), Box<dyn Error>> {
    match command {
        IntegrationCommand::Hook { event, client } => {
            run_hook_infallible(cli, *event, *client).await;
            return Ok(());
        }
        IntegrationCommand::Daemon { command } => {
            return run_daemon(cli, command).await;
        }
        IntegrationCommand::Inspect { port } => {
            return run_inspect(cli, *port).await;
        }
        _ => {}
    }

    let mut store = Store::open(&cli.path)?;
    let actor = cli
        .actor
        .clone()
        .unwrap_or_else(|| store.owner_actor().clone());
    if store.member_role(&actor)?.is_none() {
        return Err("actor is not an active library member".into());
    }
    match command {
        IntegrationCommand::Connect {
            client,
            project,
            global,
        } => {
            let client_name = client.as_str();
            let report = kp_ingest::connect::connect_with_scope(
                client_name,
                project,
                *global,
                &cli.path,
                &actor,
                &std::env::current_exe()?,
            )?;
            enable_daemon_autostart(&cli.path);
            let mut val = json!({
                "client": report.client,
                "settings": report.settings,
            });
            let mut human = format!("Connected {client_name}: {}", report.settings.display());
            if matches!(client, Client::Windsurf) && !*global {
                human.push_str(
                    "\nTip: pass `--global` to also register `kp` in `~/.codeium/windsurf/mcp_config.json` for global Cascade sessions.",
                );
            }
            if let Some(codex_cfg) = &report.codex_config {
                val["codex_config"] = json!(codex_cfg);
            }
            if let Some(check) = &report.codex_check {
                val["codex_mcp_verified"] = json!(check.verified);
                val["codex_check"] = serde_json::to_value(check)?;
                if check.verified {
                    human.push_str(" (codex mcp get kp: verified)");
                } else {
                    human.push_str(&format!(
                        "\nNote: `codex mcp get kp` did not yet detect `kp` ({}).\nProject-scoped `.codex/config.toml` and `.codex/hooks.json` stay disabled in Codex until you trust this folder:",
                        check.detail
                    ));
                    for step in &check.next_steps {
                        human.push_str(&format!("\n  {step}"));
                    }
                }
            }
            output(cli.json, &val, &human)?;
        }
        IntegrationCommand::Hook { event, client } => {
            run_hook_infallible(cli, *event, *client).await;
        }
        IntegrationCommand::Daemon { command } => {
            run_daemon(cli, command).await?;
        }
        IntegrationCommand::Inspect { port } => {
            run_inspect(cli, *port).await?;
        }
        IntegrationCommand::Verify { repo } => {
            let results =
                kp_ingest::code_intel::verify(&mut store, repo.as_deref(), &actor, now()?)?;
            let human = results
                .iter()
                .map(|a| {
                    format!(
                        "{} {}:{} {}",
                        a.verify_status, a.locus.file_path, a.locus.start_line, a.locus.symbol_name
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            output(cli.json, &results, &human)?;
        }
        IntegrationCommand::Mcp => crate::mcp::serve(&cli.path, &mut store, &actor).await?,
    }
    Ok(())
}
