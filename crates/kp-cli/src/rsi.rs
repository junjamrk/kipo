use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use clap::Subcommand;
use kp_derive::DetectedGenerativeModel;
use kp_ingest::dream::{dream, HttpDreamAttributor, LlmDreamAttributor, StructuredDreamAttributor};
use kp_model::{ActorId, ContentHash, ContrastKind, EpochMs, FactId, FeedbackOutcome, SkillSlug};
use kp_store::{Episode, Store};
use serde_json::{json, Value};

use crate::{now, output, Cli};

pub(crate) const DREAM_SPAWNING_LOCK_FILE: &str = "dream.spawning";
const DEFAULT_AUTO_DREAM_THRESHOLD: usize = 5;

#[derive(Subcommand)]
pub enum RsiCommand {
    /// Record contrastive RSI episodes (`human-correction`, `failure-to-recovery`, `cross-avatar-contrast`).
    Episode {
        #[command(subcommand)]
        command: EpisodeCommand,
    },
    /// Consolidate multi-source sessions, chats, documents, links, and media into governed facts and skills.
    Dream {
        #[arg(long)]
        at: Option<i64>,
        /// Mine real session transcripts (.jsonl / .md files or directories) before running attribution.
        #[arg(long = "sessions", alias = "session")]
        sessions: Vec<PathBuf>,
        #[arg(long, requires = "llm_model")]
        llm_url: Option<String>,
        #[arg(long, requires = "llm_url")]
        llm_model: Option<String>,
        #[arg(long, requires = "llm_url")]
        llm_key_env: Option<String>,
        #[arg(long, requires = "llm_url")]
        allow_cloud_llm: bool,
        #[arg(long, hide = true)]
        background: bool,
    },
    /// Record task-deduplicated `helped` or `hurt` feedback for a playbook bullet or skill.
    Feedback {
        target: String,
        #[arg(long)]
        outcome: FeedbackOutcome,
        #[arg(long)]
        task: String,
        #[arg(long)]
        model_family: Option<String>,
    },
    /// List active distilled skills or approve quarantined imported community skills.
    Skill {
        #[command(subcommand)]
        command: SkillCommand,
    },
}

#[derive(Subcommand)]
pub enum EpisodeCommand {
    /// Record a manual contrastive episode for sleep-time `kp dream` attribution.
    Record {
        #[arg(long, value_parser = ["human-correction", "failure-to-recovery", "cross-avatar-contrast"])]
        kind: String,
        #[arg(long)]
        before: String,
        #[arg(long)]
        after: String,
        #[arg(long)]
        step_ref: Option<String>,
        #[arg(long)]
        session: Option<String>,
        #[arg(long = "premise")]
        premises: Vec<FactId>,
    },
}

#[derive(Subcommand)]
pub enum SkillCommand {
    /// List active skills visible to the current actor.
    List {
        #[arg(long)]
        at: Option<i64>,
    },
    /// Mark a quarantined imported community skill as locally approved for execution.
    Approve { skill_slug: SkillSlug },
}

pub async fn run(cli: &Cli, command: &RsiCommand) -> Result<(), Box<dyn Error>> {
    let at = now()?;
    if let RsiCommand::Skill {
        command: SkillCommand::List { at: clock },
    } = command
    {
        let store = Store::open_readonly(&cli.path)?;
        let actor = cli
            .actor
            .clone()
            .unwrap_or_else(|| store.owner_actor().clone());
        if store.member_role(&actor)?.is_none() {
            return Err(kp_store::StoreError::PermissionDenied.into());
        }
        let eval_at = clock.map(EpochMs).unwrap_or(at);
        let value = serde_json::to_value(store.active_skills(&actor, eval_at)?)?;
        return output(cli.json, &value, &serde_json::to_string_pretty(&value)?);
    }

    let mut store = Store::open(&cli.path)?;
    let actor = cli
        .actor
        .clone()
        .unwrap_or_else(|| store.owner_actor().clone());
    if store.member_role(&actor)?.is_none() {
        return Err(kp_store::StoreError::PermissionDenied.into());
    }
    let value = match command {
        RsiCommand::Episode {
            command:
                EpisodeCommand::Record {
                    kind,
                    before,
                    after,
                    step_ref,
                    session,
                    premises,
                },
        } => record_episode(
            &mut store,
            &actor,
            at,
            &json!({"kind":kind,"before":before,"after":after,"step_ref":step_ref,"session":session,"premises":premises}),
        )?,
        RsiCommand::Dream {
            at: clock,
            sessions,
            llm_url,
            llm_model,
            llm_key_env,
            allow_cloud_llm,
            background,
        } => {
            let lock_path = cli.path.join(DREAM_SPAWNING_LOCK_FILE);
            let run_res = async {
                let at = clock.map(EpochMs).unwrap_or(at);
                crate::providers_cli::attach_ready_embedder(&mut store);
                let policy = crate::governance::explicit_writeback_default_policy();
                let mut path_sessions_mined = 0usize;
                let mut path_episodes_mined = 0usize;
                if !sessions.is_empty() {
                    let mined = kp_ingest::session_miner::mine_session_paths(
                        &mut store, sessions, &actor, &policy, at,
                    )?;
                    path_sessions_mined = mined.sessions_scanned;
                    path_episodes_mined = mined.episodes_recorded;
                }
                let api_key = llm_key_env.as_ref().map(std::env::var).transpose()?;
                let mut result = run_dream(
                    &mut store,
                    &actor,
                    at,
                    llm_url.as_deref(),
                    llm_model.as_deref(),
                    api_key,
                    *allow_cloud_llm,
                )
                .await?;
                result.sessions_mined += path_sessions_mined;
                result.episodes_mined += path_episodes_mined;
                Ok::<Value, Box<dyn Error>>(serde_json::to_value(result)?)
            }
            .await;
            if *background {
                let _ = std::fs::remove_file(&lock_path);
            }
            run_res?
        }
        RsiCommand::Feedback {
            target,
            outcome,
            task,
            model_family,
        } => serde_json::to_value(store.feedback(
            target,
            &actor,
            task,
            *outcome,
            model_family.as_deref(),
            at,
        )?)?,
        RsiCommand::Skill {
            command: SkillCommand::List { .. },
        } => unreachable!("handled via read-only path above"),
        RsiCommand::Skill {
            command: SkillCommand::Approve { skill_slug },
        } => {
            store.approve_skill(skill_slug, &actor, at)?;
            json!({"skill_slug":skill_slug,"exec_approved":true})
        }
    };
    output(cli.json, &value, &serde_json::to_string_pretty(&value)?)
}

fn record_episode(
    store: &mut Store,
    actor: &ActorId,
    at: EpochMs,
    args: &Value,
) -> Result<Value, Box<dyn Error>> {
    let kind: ContrastKind = args
        .get("kind")
        .and_then(Value::as_str)
        .ok_or("kind required")?
        .replace('-', "_")
        .parse()?;
    let trace = |key| -> Result<Value, Box<dyn Error>> {
        let value = args.get(key).ok_or("trace required")?;
        Ok(match value.as_str() {
            Some(text) => serde_json::from_str(text).unwrap_or_else(|_| json!(text)),
            None => value.clone(),
        })
    };
    let before = trace("before")?;
    let after = trace("after")?;
    let premises: Vec<FactId> = args
        .get("premises")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    let identity = ContentHash::digest(serde_json::to_vec(&(kind, &before, &after, &premises))?);
    let session = args
        .get("session")
        .and_then(Value::as_str)
        .unwrap_or(identity.as_str())
        .to_owned();
    let step = args
        .get("step_ref")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("record:{identity}"));
    let target = match premises.first() {
        Some(first) => store
            .visible_facts(actor, at)?
            .into_iter()
            .find(|fact| fact.id == *first)
            .map(|fact| fact.subject),
        None => None,
    };
    let wrap = |value: Value, success: bool| match value {
        Value::String(text) => json!({"content":text,"success":success,"target":target}),
        value => value,
    };
    if before == after
        || before.as_str().is_some_and(|s| s.trim().is_empty())
        || after.as_str().is_some_and(|s| s.trim().is_empty())
    {
        return Ok(json!({"episodes":0,"signal":"NO_ENTRY"}));
    }
    let count = kp_ingest::signals::record(
        store,
        Episode {
            signal_kind: kind,
            session_id: session,
            before_trace: wrap(before, false),
            after_trace: wrap(after, true),
            step_ref: step,
            environment: json!({"actor":actor,"verified":true,"target":target}),
        },
        actor,
        &premises,
        at,
    )?;
    Ok(json!({"episodes":count,"signal":if count == 0 { "NO_ENTRY" } else { "RECORDED" }}))
}

async fn run_dream(
    store: &mut Store,
    actor: &ActorId,
    at: EpochMs,
    url: Option<&str>,
    model: Option<&str>,
    api_key: Option<String>,
    allow_cloud: bool,
) -> Result<kp_ingest::dream::DreamReport, Box<dyn Error>> {
    match (url, model) {
        (Some(url), Some(model)) => Ok(dream(
            store,
            actor,
            at,
            &HttpDreamAttributor::with_api_key(url, model, allow_cloud, api_key)?,
        )
        .await?),
        (None, None) if !allow_cloud => {
            let detected = crate::governance::detect_global_model();
            match detected {
                DetectedGenerativeModel::HostCli(_) | DetectedGenerativeModel::HttpApi(_) => {
                    let client = detected.build_llm_client_with_overrides(None, None)?;
                    let attributor = LlmDreamAttributor::new(client)?;
                    Ok(dream(store, actor, at, &attributor).await?)
                }
                DetectedGenerativeModel::RawOnly => {
                    Ok(dream(store, actor, at, &StructuredDreamAttributor).await?)
                }
            }
        }
        _ => Err("--llm-url and --llm-model must be supplied together".into()),
    }
}

pub(crate) fn maybe_autospawn_dream_in_background(library_dir: &Path, actor: &ActorId) {
    let explicit_enable = std::env::var_os("KP_ENABLE_AUTO_DREAM_IN_TEST").is_some()
        || std::env::var_os("KP_AUTO_DREAM").is_some();
    if kp_ingest::hooks::is_internal_extraction()
        || std::env::var_os("KP_NO_AUTO_DREAM").is_some()
        || (cfg!(test) && !explicit_enable)
        || (crate::providers_cli::is_kp_offline() && !explicit_enable)
    {
        return;
    }
    let threshold = std::env::var("KP_AUTO_DREAM_THRESHOLD")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(DEFAULT_AUTO_DREAM_THRESHOLD);

    let pending = Store::open_readonly(library_dir)
        .and_then(|ro| ro.pending_dream_work_count(actor))
        .unwrap_or(0);
    if pending < threshold {
        return;
    }

    let lock_path = library_dir.join(DREAM_SPAWNING_LOCK_FILE);
    if let Ok(meta) = std::fs::metadata(&lock_path) {
        let fresh = meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age < Duration::from_secs(120));
        if fresh {
            return;
        }
        let _ = std::fs::remove_file(&lock_path);
    }
    if std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .is_err()
    {
        return;
    }

    let Ok(exe) = std::env::current_exe() else {
        let _ = std::fs::remove_file(&lock_path);
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.env(kp_ingest::hooks::INTERNAL_EXTRACTION_ENV, "1")
        .arg("--path")
        .arg(library_dir)
        .arg("--as")
        .arg(actor.as_str())
        .args(["dream", "--background"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    if cmd.spawn().is_err() {
        let _ = std::fs::remove_file(&lock_path);
    }
}
