mod community;
#[cfg(feature = "connectors")]
mod connectors_cli;
mod demo_cli;
mod governance;
mod integrations;
mod mcp;
mod memory;
mod providers_cli;
mod rsi;

use std::error::Error;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use kp_model::{ActorId, EpochMs, MemberRole};
use kp_store::Store;
use serde::Serialize;

#[derive(Parser)]
#[command(
    name = "kp",
    version,
    about = "Local-first personal & team memory infrastructure for AI coding agents"
)]
struct Cli {
    /// Path to the library directory containing `kp.sqlite`.
    #[arg(long, global = true, default_value = ".")]
    path: PathBuf,
    /// Emit machine-readable JSON on stdout (and JSON errors on stderr).
    #[arg(long, global = true)]
    json: bool,
    /// Act as `<ACTOR>` (defaults to the library owner).
    #[arg(long = "as", global = true)]
    actor: Option<ActorId>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize a new personal or community library (`kp.sqlite`).
    Init {
        #[arg(long, default_value = "My Library")]
        name: String,
        #[arg(long)]
        community: bool,
        #[arg(long, default_value = "owner")]
        owner: ActorId,
        #[arg(long)]
        no_embedding: bool,
        #[arg(long)]
        embedding_model: Option<String>,
        #[arg(long)]
        embedding_dimensions: Option<usize>,
    },
    /// Show library summary, active fact/chunk counts, and embedder status.
    Status,
    #[command(flatten)]
    Integrations(integrations::IntegrationCommand),
    #[command(flatten)]
    Community(community::CommunityCommand),
    #[command(flatten)]
    Memory(memory::MemoryCommand),
    #[command(flatten)]
    Governance(governance::GovernanceCommand),
    #[command(flatten)]
    Rsi(rsi::RsiCommand),
    #[command(flatten)]
    VectorExport(providers_cli::VectorExportCommand),
    /// Manage optional external connectors (Feishu, GitHub, agent sessions) via the Public Ingest Contract.
    #[cfg(feature = "connectors")]
    Connector(connectors_cli::ConnectorCommandArgs),
    /// Run a 5-minute offline interactive walkthrough of Kipo (`--dir <path>` keeps the demo library).
    Demo(demo_cli::DemoArgs),
    /// Manage library members and authentication tokens.
    Member {
        #[command(subcommand)]
        command: MemberCommand,
    },
}

#[derive(Subcommand)]
enum MemberCommand {
    /// Add an active member to the library and print their bearer token.
    Add {
        actor: ActorId,
        #[arg(long, default_value = "member")]
        role: MemberRole,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let is_hook_invocation = raw_argv_is_hook();
    if is_hook_invocation && kp_ingest::hooks::is_internal_extraction() {
        let _ = writeln!(io::stdout().lock(), "{{}}");
        return ExitCode::SUCCESS;
    }
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            if err.use_stderr() && is_hook_invocation {
                let _ = writeln!(io::stdout().lock(), "{{}}");
                return ExitCode::SUCCESS;
            }
            let _ = err.print();
            return if err.use_stderr() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    if let Command::Integrations(integrations::IntegrationCommand::Hook { event, client }) =
        &cli.command
    {
        integrations::run_hook_infallible(&cli, *event, *client).await;
        return ExitCode::SUCCESS;
    }
    match run(&cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if cli.json {
                eprintln!("{}", serde_json::json!({"error":error.to_string()}));
            } else {
                eprintln!("kp: {error}");
            }
            ExitCode::FAILURE
        }
    }
}

fn raw_argv_is_hook() -> bool {
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        let Some(text) = arg.to_str() else {
            return false;
        };
        match text {
            "--path" | "--as" => {
                let _ = args.next();
            }
            "--json" => {}
            s if s.starts_with("--path=") || s.starts_with("--as=") => {}
            s if s.starts_with('-') => return false,
            subcommand => return subcommand == "hook",
        }
    }
    false
}

async fn run(cli: &Cli) -> Result<(), Box<dyn Error>> {
    match &cli.command {
        Command::Integrations(command) => integrations::run(cli, command).await?,
        Command::Community(command) => community::run(cli, command).await?,
        Command::Memory(command) => memory::run(cli, command).await?,
        Command::Governance(command) => governance::run(cli, command).await?,
        Command::Rsi(command) => rsi::run(cli, command).await?,
        Command::VectorExport(command) => providers_cli::run(cli, command).await?,
        #[cfg(feature = "connectors")]
        Command::Connector(args) => connectors_cli::run(cli, args).await?,
        Command::Demo(args) => demo_cli::run(cli, args).await?,
        Command::Init {
            name,
            community,
            owner,
            no_embedding,
            embedding_model,
            embedding_dimensions,
        } => {
            providers_cli::run_init(
                cli,
                name,
                *community,
                owner,
                *no_embedding,
                embedding_model.as_deref(),
                *embedding_dimensions,
            )?;
        }
        Command::Status => {
            let status = Store::open_readonly(&cli.path)?.status()?;
            let guard_note = if status.untagged_guard_candidates > 0 {
                format!(
                    "\n{} untagged prose guard candidates (tag with [forbid: ...] or run `kp dream`)",
                    status.untagged_guard_candidates
                )
            } else {
                String::new()
            };
            output(
                cli.json,
                &status,
                &format!(
                    "{} ({})\n{} members, {} sources, {} facts{}",
                    status.name,
                    status.mode,
                    status.members,
                    status.sources,
                    status.facts,
                    guard_note
                ),
            )?;
        }
        Command::Member {
            command: MemberCommand::Add { actor, role },
        } => {
            let token = Store::open(&cli.path)?.add_member(actor.clone(), *role, now()?)?;
            output(
                cli.json,
                &token,
                &format!(
                    "Added {} as {}\nBearer token (shown once): {}",
                    token.actor_id, token.role, token.token
                ),
            )?;
        }
    }
    Ok(())
}

fn output(json: bool, value: &impl Serialize, human: &str) -> Result<(), Box<dyn Error>> {
    let mut stdout = io::stdout().lock();
    if json {
        serde_json::to_writer(&mut stdout, value)?;
        writeln!(stdout)?;
    } else {
        writeln!(stdout, "{human}")?;
    }
    Ok(())
}

fn now() -> Result<EpochMs, Box<dyn Error>> {
    Ok(EpochMs(i64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?))
}
