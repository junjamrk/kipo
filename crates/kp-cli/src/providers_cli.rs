use std::collections::BTreeMap;
use std::error::Error;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use clap::Subcommand;
use kp_model::{ActorId, EpochMs, KpError, LibraryId, LibraryMode, MemberRole, SecretRedactor};
use kp_net::DaemonEmbedderRuntimeStatus;
use kp_store::{
    build_embedder_for_spec, embedding_recipes, find_embedding_recipe, load_global_config,
    parse_embedding_model_spec, resolve_init_embedding_choice, try_build_embedder_with_reason,
    DisabledModelEmbedder, Embedder, FtsOnlyEmbedder, HashEmbedder, InitEmbeddingChoice,
    InitOptions, KpGlobalConfig, ProviderEmbedder, ProviderEmbedderConfig, ProviderTestReport,
    Store, StoreError, DEFAULT_LOCAL_EMBEDDING_MODEL,
};
use serde_json::json;
use uuid::Uuid;

use crate::{now, output, Cli};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Subcommand)]
pub enum ProvidersSubcommand {
    /// List built-in local, domestic China, international cloud, and explicit local embedding recipes.
    List,
    /// Show authentication environment variables, base URL overrides, and config examples for `<id>`.
    Env { id: String },
    /// Probe live connectivity and returned vector dimensions (`--model <provider>:<model>`).
    Test {
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        dim: Option<usize>,
    },
}

#[derive(Subcommand)]
pub enum VectorExportCommand {
    /// Inspect and test built-in local ONNX and multi-provider HTTP embedding recipes.
    Providers {
        #[command(subcommand)]
        command: ProvidersSubcommand,
    },
    /// Recompute missing or stale vectors across chunks and facts (`--to <provider>:<model> --dim <N>`).
    Embed {
        #[arg(long, required = true)]
        stale: bool,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        dim: Option<usize>,
    },
    /// Export viewer-scoped Markdown projection pages (`projection_pages` baseline tracked).
    Export {
        dir: PathBuf,
        #[arg(long)]
        at: Option<i64>,
    },
    /// Apply edits, deletions, and freeform notes from an exported Markdown directory back into the library.
    Apply {
        dir: PathBuf,
        #[arg(long)]
        at: Option<i64>,
    },
    /// Show viewer-isolated audit history (`change_log`) in reverse chronological order.
    Log {
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        at: Option<i64>,
    },
}

pub(crate) fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|v| {
            let t = v.trim();
            !t.is_empty() && !t.eq_ignore_ascii_case("0") && !t.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false)
}

/// Returns `true` when `KP_OFFLINE=1` is active (used by deterministic CI / CLI test suites).
pub(crate) fn is_kp_offline() -> bool {
    env_flag_enabled("KP_OFFLINE")
}

/// Returns `true` when background automatic ONNX downloads should stay disabled.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn is_auto_model_download_disabled() -> bool {
    cfg!(test) || is_kp_offline() || env_flag_enabled("KP_DISABLE_MODEL_DOWNLOAD")
}

/// Hot-swappable thread-safe [`Embedder`] wrapper hosted inside `kp daemon` so local ONNX models
/// are loaded at most once across the daemon's lifetime, expose live download progress/errors via
/// [`DaemonEmbedderRuntimeStatus`], and transition atomically from `DisabledModelEmbedder` (FTS5
/// fallback) to `LocalOnnxEmbedder` once background download finishes.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Clone)]
pub(crate) struct SwappableEmbedder {
    configured_model: Arc<str>,
    model_id: Arc<str>,
    dims: usize,
    inner: Arc<RwLock<Arc<dyn Embedder + Send + Sync>>>,
    runtime_status: Arc<RwLock<DaemonEmbedderRuntimeStatus>>,
}

#[cfg_attr(not(unix), allow(dead_code))]
impl SwappableEmbedder {
    pub(crate) fn new_with_status(
        configured_model: &str,
        dims: usize,
        initial: Arc<dyn Embedder + Send + Sync>,
        pending_bg_download: bool,
        initial_error: Option<String>,
    ) -> Self {
        let effective_model = if initial.model_id().is_empty() || initial.model_id() == "custom" {
            configured_model.to_owned()
        } else {
            initial.model_id().to_owned()
        };
        let (state, download_pct, last_error) = if configured_model.eq_ignore_ascii_case("none") {
            ("disabled".to_owned(), None, None)
        } else if pending_bg_download {
            ("downloading".to_owned(), Some(0), None)
        } else if initial.is_enabled() {
            ("ready".to_owned(), Some(100), None)
        } else {
            ("fts5_fallback".to_owned(), None, initial_error)
        };
        let status = DaemonEmbedderRuntimeStatus {
            model: effective_model.clone(),
            state,
            download_pct,
            last_error,
        };
        Self {
            configured_model: Arc::from(configured_model),
            model_id: Arc::from(effective_model),
            dims,
            inner: Arc::new(RwLock::new(initial)),
            runtime_status: Arc::new(RwLock::new(status)),
        }
    }

    pub(crate) fn configured_model(&self) -> &str {
        &self.configured_model
    }

    pub(crate) fn configured_dims(&self) -> usize {
        self.dims
    }

    pub(crate) fn status_snapshot(&self) -> DaemonEmbedderRuntimeStatus {
        self.runtime_status
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| DaemonEmbedderRuntimeStatus {
                model: self.model_id.to_string(),
                state: "error".to_owned(),
                download_pct: None,
                last_error: Some("daemon embedder status lock poisoned".to_owned()),
            })
    }

    #[cfg_attr(not(feature = "local-embed"), allow(dead_code))]
    pub(crate) fn update_download_progress(&self, pct: u8) {
        if let Ok(mut st) = self.runtime_status.write() {
            let clamped = pct.min(99);
            let next_pct = st.download_pct.map_or(clamped, |prev| prev.max(clamped));
            st.state = "downloading".to_owned();
            st.download_pct = Some(next_pct);
            st.last_error = None;
        }
    }

    #[cfg_attr(not(feature = "local-embed"), allow(dead_code))]
    pub(crate) fn swap(&self, next: Arc<dyn Embedder + Send + Sync>) {
        if let Ok(mut guard) = self.inner.write() {
            *guard = next;
        }
        if let Ok(mut st) = self.runtime_status.write() {
            st.state = "ready".to_owned();
            st.download_pct = Some(100);
            st.last_error = None;
        }
    }

    #[cfg_attr(not(feature = "local-embed"), allow(dead_code))]
    pub(crate) fn mark_error(&self, err: impl Into<String>) {
        if let Ok(mut st) = self.runtime_status.write() {
            st.state = "error".to_owned();
            st.last_error = Some(err.into());
        }
    }

    pub(crate) fn current_inner(&self) -> Option<Arc<dyn Embedder + Send + Sync>> {
        self.inner.read().ok().map(|g| Arc::clone(&g))
    }
}

impl Embedder for SwappableEmbedder {
    fn embed(&self, text: &str) -> kp_store::Result<Vec<f32>> {
        let cur = self
            .current_inner()
            .ok_or_else(|| StoreError::Embedding("daemon embedder lock poisoned".into()))?;
        cur.embed(text)
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn expected_dims(&self) -> Option<usize> {
        self.current_inner().and_then(|c| c.expected_dims())
    }

    fn is_enabled(&self) -> bool {
        self.current_inner().is_some_and(|c| c.is_enabled())
    }

    fn allows_raw_only(&self) -> bool {
        self.current_inner().is_some_and(|c| c.allows_raw_only())
    }

    fn embed_batch(&self, texts: &[String]) -> kp_store::Result<Vec<Vec<f32>>> {
        let cur = self
            .current_inner()
            .ok_or_else(|| StoreError::Embedding("daemon embedder lock poisoned".into()))?;
        cur.embed_batch(texts)
    }
}

/// Builds the daemon's resident [`SwappableEmbedder`] from `store.embedding_config()` without
/// blocking startup on network downloads. Also returns `Some(local_model_id)` when a `local:*`
/// ONNX model is not yet cached and background download is permitted.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn build_daemon_swappable_embedder(
    store: &Store,
) -> (Arc<SwappableEmbedder>, Option<String>) {
    let global_cfg = load_global_config().ok();
    build_daemon_swappable_embedder_with_config(store, global_cfg.as_ref())
}

#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn build_daemon_swappable_embedder_with_config(
    store: &Store,
    global_cfg: Option<&KpGlobalConfig>,
) -> (Arc<SwappableEmbedder>, Option<String>) {
    let (emb_model, emb_dims) = store
        .embedding_config()
        .unwrap_or_else(|_| ("hash:v1".to_owned(), 256));
    let dims_opt = (emb_dims > 0).then_some(emb_dims);

    let (initial, pending_local, initial_error) =
        match parse_embedding_model_spec(&emb_model, dims_opt) {
            Ok(spec) => {
                let (built, diag_reason) = try_build_embedder_with_reason(&spec, global_cfg);
                let needs_bg_download = spec.provider_id == "local"
                    && !built.is_enabled()
                    && !is_auto_model_download_disabled();
                let err_opt = if needs_bg_download { None } else { diag_reason };
                (built, needs_bg_download.then_some(spec.model_id), err_opt)
            }
            Err(err) => {
                let redacted = SecretRedactor::new()
                    .map_or_else(|_| err.to_string(), |r| r.redact(&err.to_string()));
                (
                    Arc::new(DisabledModelEmbedder::new(emb_model.clone(), emb_dims))
                        as Arc<dyn Embedder + Send + Sync>,
                    None,
                    Some(redacted),
                )
            }
        };

    let swappable = Arc::new(SwappableEmbedder::new_with_status(
        &emb_model,
        emb_dims,
        initial,
        pending_local.is_some(),
        initial_error,
    ));
    (swappable, pending_local)
}

#[cfg_attr(any(not(unix), not(feature = "local-embed")), allow(dead_code))]
pub(crate) fn compute_progress_pct(downloaded_bytes: u64, total_bytes: Option<u64>) -> Option<u8> {
    let total = total_bytes.filter(|&t| t > 0)?;
    let raw = downloaded_bytes.saturating_mul(100) / total;
    Some(u8::try_from(raw.min(100)).unwrap_or(100))
}

/// Spawns a non-blocking background task in `kp daemon` to download and SHA-256-verify a pending
/// `local:*` ONNX model, stream `DownloadProgress` into `swappable`, atomically swap on completion
/// (or record `state = "error"` on failure), and backfill missing vectors via `reembed_stale`.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn spawn_background_local_onnx_warmup(
    library_dir: PathBuf,
    model_id: String,
    swappable: Arc<SwappableEmbedder>,
) {
    #[cfg(feature = "local-embed")]
    {
        tokio::task::spawn_blocking(move || {
            let progress_ref = Arc::clone(&swappable);
            match kp_store::LocalOnnxEmbedder::ensure_downloaded_and_load(&model_id, |prog| {
                if let Some(pct) = compute_progress_pct(prog.downloaded_bytes, prog.total_bytes) {
                    progress_ref.update_download_progress(pct);
                }
            }) {
                Ok(loaded) => {
                    let arc_loaded: Arc<dyn Embedder + Send + Sync> = Arc::new(loaded);
                    swappable.swap(Arc::clone(&arc_loaded));
                    if let Ok(mut bg_store) = Store::open(&library_dir) {
                        bg_store.set_embedder(Arc::clone(&arc_loaded));
                        let _ = bg_store.reembed_stale(arc_loaded.as_ref(), None);
                    }
                }
                Err(err) => {
                    swappable.mark_error(err.to_string());
                }
            }
        });
    }
    #[cfg(not(feature = "local-embed"))]
    {
        let _ = (library_dir, model_id, swappable);
    }
}

/// Attaches the daemon's resident [`SwappableEmbedder`] to `store`, automatically rebuilding and
/// atomically replacing the slot in `daemon_slot` whenever `store.embedding_config()` (`library_meta`)
/// changed externally (e.g. via `kp embed --stale --to <new_model>`).
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn attach_or_refresh_daemon_embedder(
    store: &mut Store,
    library_dir: &Path,
    daemon_slot: &Arc<RwLock<Option<Arc<SwappableEmbedder>>>>,
) {
    let Ok((emb_model, emb_dims)) = store.embedding_config() else {
        attach_ready_embedder(store);
        return;
    };

    let existing_opt = daemon_slot.read().ok().and_then(|g| g.clone());
    if let Some(ref existing) = existing_opt {
        if existing.configured_model() == emb_model && existing.configured_dims() == emb_dims {
            if emb_model.eq_ignore_ascii_case("none") {
                store.set_embedder(Arc::new(FtsOnlyEmbedder));
            } else {
                store.set_embedder(Arc::clone(existing) as Arc<dyn Embedder + Send + Sync>);
            }
            return;
        }
    }

    let (rebuilt, pending_local) = build_daemon_swappable_embedder(store);
    if let Ok(mut guard) = daemon_slot.write() {
        *guard = Some(Arc::clone(&rebuilt));
    }
    if emb_model.eq_ignore_ascii_case("none") {
        store.set_embedder(Arc::new(FtsOnlyEmbedder));
    } else {
        store.set_embedder(Arc::clone(&rebuilt) as Arc<dyn Embedder + Send + Sync>);
    }
    if let Some(local_model_id) = pending_local {
        spawn_background_local_onnx_warmup(library_dir.to_path_buf(), local_model_id, rebuilt);
    }
}

/// Attaches non-ONNX ready embedders (HTTP `ProviderEmbedder` or `hash:v1`) on direct hook
/// fallback paths while strictly avoiding loading heavy local ONNX models inside thin hooks.
pub(crate) fn attach_non_local_embedder_for_hook(store: &mut Store) {
    let Ok((emb_model, emb_dims)) = store.embedding_config() else {
        return;
    };
    if emb_model == "none" || emb_model == "hash:v1" {
        return;
    }
    let Ok(spec) = parse_embedding_model_spec(&emb_model, (emb_dims > 0).then_some(emb_dims))
    else {
        return;
    };
    if spec.provider_id == "local" {
        return;
    }
    let global_cfg = load_global_config().ok();
    if let Ok(embedder) = build_embedder_for_spec(&spec, global_cfg.as_ref()) {
        store.set_embedder(embedder);
    }
}

/// Attaches the library's configured [`Embedder`] (`local:*` via `try_load_cached` without network,
/// or HTTP [`ProviderEmbedder`] when credentials are present), degrading seamlessly to FTS5 if
/// uncached, offline, or missing API keys.
pub(crate) fn attach_ready_embedder(store: &mut Store) {
    let Ok((emb_model, emb_dims)) = store.embedding_config() else {
        return;
    };
    if emb_model == "none" || emb_model == "hash:v1" {
        return;
    }
    if store.embedder().model_id() == emb_model && store.embedder().is_enabled() {
        return;
    }
    let Ok(spec) = parse_embedding_model_spec(&emb_model, (emb_dims > 0).then_some(emb_dims))
    else {
        return;
    };
    let global_cfg = load_global_config().ok();
    if let Ok(embedder) = build_embedder_for_spec(&spec, global_cfg.as_ref()) {
        store.set_embedder(embedder);
    }
}

fn is_local_onnx_cached(model_id: &str) -> bool {
    #[cfg(feature = "local-embed")]
    {
        let Ok(Some(models_dir)) = kp_store::resolve_models_dir().map(Some) else {
            return false;
        };
        let Ok(dylib_path) = kp_store::resolve_ort_dylib_path(&models_dir) else {
            return false;
        };
        if !dylib_path.is_file() {
            return false;
        }
        let Ok(spec) = kp_store::find_local_onnx_spec(model_id) else {
            return false;
        };
        let model_dir = models_dir.join(spec.cache_slug);
        spec.files
            .iter()
            .all(|f| model_dir.join(f.relative_path).is_file())
    }
    #[cfg(not(feature = "local-embed"))]
    {
        let _ = model_id;
        false
    }
}

#[cfg_attr(not(feature = "local-embed"), allow(dead_code))]
pub(crate) fn format_download_progress_step(
    artifact: &str,
    downloaded_bytes: u64,
    total_bytes: Option<u64>,
    finished: bool,
    last_buckets: &mut BTreeMap<String, u8>,
) -> Option<String> {
    if finished {
        last_buckets.insert(artifact.to_owned(), 100);
        let kb = downloaded_bytes / 1024;
        return Some(format!(
            "[kp model] verified `{artifact}` ({kb} KiB, SHA-256 OK)"
        ));
    }
    if downloaded_bytes == 0 && !last_buckets.contains_key(artifact) {
        last_buckets.insert(artifact.to_owned(), 0);
        return Some(match total_bytes {
            Some(total) => format!(
                "[kp model] downloading `{artifact}` ({} KiB)...",
                total / 1024
            ),
            None => format!("[kp model] downloading `{artifact}`..."),
        });
    }
    let pct = compute_progress_pct(downloaded_bytes, total_bytes)?;
    let bucket = (pct / 20) * 20;
    if (20..100).contains(&bucket) {
        let prev = last_buckets.get(artifact).copied().unwrap_or(0);
        if bucket > prev {
            last_buckets.insert(artifact.to_owned(), bucket);
            return Some(format!("[kp model] downloading `{artifact}`... {bucket}%"));
        }
    }
    None
}

#[cfg(feature = "local-embed")]
fn render_download_progress_stderr(
    progress: &kp_store::DownloadProgress,
    last_buckets: &mut BTreeMap<String, u8>,
) {
    if let Some(line) = format_download_progress_step(
        &progress.artifact,
        progress.downloaded_bytes,
        progress.total_bytes,
        progress.finished,
        last_buckets,
    ) {
        eprintln!("{line}");
    }
}

/// Interactive / non-interactive multi-provider selector for `kp init` when multiple cloud
/// embedding API keys are present in environment variables or `0600` `~/.kp/config.json`.
pub(crate) fn select_among_multiple_detected_providers<R: BufRead, W: Write>(
    choice: &mut InitEmbeddingChoice,
    embedding_dimensions: Option<usize>,
    interactive: bool,
    reader: &mut R,
    err_writer: &mut W,
) -> Result<()> {
    if choice.detected_cloud_providers.len() <= 1 {
        return Ok(());
    }

    let cloud_count = choice.detected_cloud_providers.len();
    if !interactive {
        let first = &choice.detected_cloud_providers[0];
        choice.selected =
            parse_embedding_model_spec(&first.default_model_id, embedding_dimensions)?;
        choice.cloud_privacy_notice = Some(format!(
            "Note: ingested text chunks and facts will be sent to {} (`{}`) for vector embedding.",
            first.provider_name, choice.selected.model_id
        ));
        let detected_summary = choice
            .detected_cloud_providers
            .iter()
            .map(|p| {
                format!(
                    "{} (`{}`, {})",
                    p.provider_name, p.default_model_id, p.key_source
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(
            err_writer,
            "Note: multiple cloud embedding keys detected: {detected_summary}; defaulting to `{}` (override with `--embedding-model <provider>:<model>` or `--no-embedding`).",
            choice.selected.model_id
        )?;
        err_writer.flush()?;
        return Ok(());
    }

    writeln!(
        err_writer,
        "Multiple cloud embedding API keys detected. Choose an embedding backend:"
    )?;
    for (idx, prov) in choice.detected_cloud_providers.iter().enumerate() {
        let dims = embedding_dimensions.unwrap_or(prov.default_dims);
        writeln!(
            err_writer,
            "  [{}] {} (`{}`, {}d via {})",
            idx + 1,
            prov.provider_name,
            prov.default_model_id,
            dims,
            prov.key_source
        )?;
    }
    let local_idx = cloud_count + 1;
    let none_idx = cloud_count + 2;
    writeln!(
        err_writer,
        "  [{local_idx}] Local ONNX (`{DEFAULT_LOCAL_EMBEDDING_MODEL}`, 512d — runs locally)"
    )?;
    writeln!(
        err_writer,
        "  [{none_idx}] None (`none`, 0d — pure FTS5 keyword search)"
    )?;
    write!(err_writer, "Select [1-{none_idx}] (default 1): ")?;
    err_writer.flush()?;

    let mut line = String::new();
    let _ = reader.read_line(&mut line)?;
    let trimmed = line.trim();
    let picked = if trimmed.is_empty() {
        1usize
    } else {
        trimmed.parse::<usize>().map_err(|_| {
            KpError::invalid(
                "embedding_model",
                "invalid provider menu selection; expected option number",
            )
        })?
    };

    if picked == 0 || picked > none_idx {
        return Err(KpError::invalid(
            "embedding_model",
            "selected provider option is out of range",
        )
        .into());
    }

    if picked <= cloud_count {
        let chosen = &choice.detected_cloud_providers[picked - 1];
        choice.selected =
            parse_embedding_model_spec(&chosen.default_model_id, embedding_dimensions)?;
        choice.cloud_privacy_notice = Some(format!(
            "Note: ingested text chunks and facts will be sent to {} (`{}`) for vector embedding.",
            chosen.provider_name, choice.selected.model_id
        ));
    } else if picked == local_idx {
        choice.selected =
            parse_embedding_model_spec(DEFAULT_LOCAL_EMBEDDING_MODEL, embedding_dimensions)?;
        choice.cloud_privacy_notice = None;
    } else {
        choice.selected = parse_embedding_model_spec("none", None)?;
        choice.cloud_privacy_notice = None;
    }

    Ok(())
}

/// Executes `kp init` with M2 embedding flags and `docs/ARCHITECTURE.md` embedding auto-detection.
pub(crate) fn run_init(
    cli: &Cli,
    name: &str,
    community: bool,
    owner: &ActorId,
    no_embedding: bool,
    embedding_model: Option<&str>,
    embedding_dimensions: Option<usize>,
) -> Result<()> {
    let init_opts = InitOptions {
        library_id: LibraryId::new(Uuid::new_v4().to_string())?,
        name: name.to_owned(),
        mode: if community {
            LibraryMode::Community
        } else {
            LibraryMode::Personal
        },
        owner: owner.clone(),
        created_at: now()?,
    };

    let store = if is_kp_offline()
        && !no_embedding
        && embedding_model.is_none()
        && embedding_dimensions.is_none()
    {
        Store::init_with_embedding(&cli.path, init_opts, "hash:v1", 256)?
    } else {
        let global_cfg = load_global_config()?;
        let mut choice = resolve_init_embedding_choice(
            no_embedding,
            embedding_model,
            embedding_dimensions,
            |k| std::env::var(k).ok(),
            Some(&global_cfg),
        )?;
        if !no_embedding && embedding_model.is_none() && choice.detected_cloud_providers.len() > 1 {
            let interactive = io::stdin().is_terminal()
                && io::stderr().is_terminal()
                && !cli.json
                && !env_flag_enabled("KP_NONINTERACTIVE")
                && !is_kp_offline();
            let mut stdin_lock = io::stdin().lock();
            let mut stderr_lock = io::stderr().lock();
            select_among_multiple_detected_providers(
                &mut choice,
                embedding_dimensions,
                interactive,
                &mut stdin_lock,
                &mut stderr_lock,
            )?;
        }
        let created = Store::init_with_embedding(
            &cli.path,
            init_opts,
            &choice.selected.model_id,
            choice.selected.dims,
        )?;
        if let Some(ref notice) = choice.cloud_privacy_notice {
            eprintln!("{notice}");
        } else if choice.selected.provider_id == "local"
            && !is_local_onnx_cached(&choice.selected.model_id)
        {
            eprintln!(
                "Note: configured local embedding `{}` ({}d); model files download on first `kp daemon` / `kp providers test` / `kp embed --stale` run (using FTS5 until ready).",
                choice.selected.model_id, choice.selected.dims
            );
        }
        created
    };

    let status = store.status()?;
    output(
        cli.json,
        &status,
        &format!(
            "Initialized {:?} library {} ({}, embedding: {} {}d)",
            status.mode,
            status.library_id,
            cli.path.display(),
            status.embedding_model,
            status.embedding_dims
        ),
    )?;
    Ok(())
}

pub async fn run(cli: &Cli, command: &VectorExportCommand) -> Result<()> {
    match command {
        VectorExportCommand::Providers { command: sub } => match sub {
            ProvidersSubcommand::List => run_providers_list(cli),
            ProvidersSubcommand::Env { id } => run_providers_env(cli, id),
            ProvidersSubcommand::Test { model, dim } => {
                run_providers_test(cli, model.as_deref(), *dim).await
            }
        },
        VectorExportCommand::Embed { stale, to, dim } => {
            if !*stale {
                return Err(KpError::invalid(
                    "stale",
                    "pass `--stale` to recompute missing or stale vectors",
                )
                .into());
            }
            run_embed_stale(cli, to.as_deref(), *dim).await
        }
        VectorExportCommand::Export { dir, at } => run_export(cli, dir, *at),
        VectorExportCommand::Apply { dir, at } => run_apply(cli, dir, *at).await,
        VectorExportCommand::Log { limit, target, at } => {
            run_log(cli, *limit, target.as_deref(), *at)
        }
    }
}

fn run_providers_list(cli: &Cli) -> Result<()> {
    let global_cfg = load_global_config().ok();
    let mut entries = Vec::new();
    let mut lines = Vec::new();

    for recipe in embedding_recipes() {
        let category = if recipe.id == "local" {
            "local"
        } else if matches!(recipe.id, "dashscope" | "volcengine" | "zhipu") {
            "domestic_cloud"
        } else if recipe.is_cloud {
            "cloud"
        } else {
            "explicit_local_server"
        };

        let default_spec = if recipe.touchpoint.default_model.is_empty() {
            format!("{}:<model>", recipe.id)
        } else {
            format!("{}:{}", recipe.id, recipe.touchpoint.default_model)
        };

        let (ready, key_source) = if recipe.id == "local" {
            let cached = is_local_onnx_cached(&default_spec);
            (
                cached,
                Some(if cached {
                    "cached".to_owned()
                } else {
                    "download_on_demand".to_owned()
                }),
            )
        } else if !recipe.is_cloud {
            let env_hit = recipe.auth_env.iter().find(|&&var| {
                std::env::var(var)
                    .map(|v| !v.trim().is_empty())
                    .unwrap_or(false)
            });
            let src = env_hit
                .map(|v| format!("env:{v}"))
                .unwrap_or_else(|| "explicit_local".to_owned());
            (true, Some(src))
        } else {
            let env_hit = recipe.auth_env.iter().find(|&&var| {
                std::env::var(var)
                    .map(|v| !v.trim().is_empty())
                    .unwrap_or(false)
            });
            if let Some(&var) = env_hit {
                (true, Some(format!("env:{var}")))
            } else if global_cfg
                .as_ref()
                .and_then(|c| c.lookup_api_key(recipe))
                .is_some()
            {
                (true, Some("config:api_keys".to_owned()))
            } else {
                (false, None)
            }
        };

        lines.push(format!(
            "{:<14} {:<38} {:>5}d  [{}] {}",
            recipe.id,
            default_spec,
            recipe.touchpoint.default_dims,
            category,
            if ready {
                key_source.as_deref().unwrap_or("ready")
            } else if recipe.auth_env.is_empty() {
                "unconfigured"
            } else {
                recipe.auth_env[0]
            }
        ));

        entries.push(json!({
            "id": recipe.id,
            "name": recipe.name,
            "category": category,
            "implementation": recipe.implementation,
            "default_model": default_spec,
            "default_dims": recipe.touchpoint.default_dims,
            "models": recipe.touchpoint.models,
            "dims_options": recipe.touchpoint.dims_options,
            "max_batch_items": recipe.touchpoint.max_batch_items,
            "auth_env": recipe.auth_env,
            "base_url_default": recipe.base_url_default,
            "auto_detect_env": recipe.auto_detect_env,
            "is_cloud": recipe.is_cloud,
            "send_dimensions_param": recipe.send_dimensions_param,
            "ready": ready,
            "key_source": key_source,
        }));
    }

    output(
        cli.json,
        &json!({ "providers": entries }),
        &lines.join("\n"),
    )?;
    Ok(())
}

fn run_providers_env(cli: &Cli, id: &str) -> Result<()> {
    let Some(recipe) = find_embedding_recipe(id) else {
        return Err(StoreError::Embedding(format!("unknown embedding provider `{id}`")).into());
    };
    let base_url_env = format!(
        "{}_BASE_URL",
        recipe.id.to_ascii_uppercase().replace('-', "_")
    );
    let default_model = if recipe.touchpoint.default_model.is_empty() {
        format!("{}:<model>", recipe.id)
    } else {
        format!("{}:{}", recipe.id, recipe.touchpoint.default_model)
    };
    let gateway_env = (recipe.id == "openai").then_some("OPENAI_BASE_URL");
    let config_key = format!("provider_base_urls.{}", recipe.id);
    let base_default = recipe.base_url_default.unwrap_or("local://onnxruntime");

    let mut example_cfg = serde_json::Map::new();
    if let Some(&first_auth) = recipe.auth_env.first() {
        example_cfg.insert(
            "api_keys".to_owned(),
            json!({ first_auth: "<YOUR_API_KEY>" }),
        );
    }
    if let Some(url) = recipe.base_url_default {
        example_cfg.insert("provider_base_urls".to_owned(), json!({ recipe.id: url }));
    }

    let val = json!({
        "id": recipe.id,
        "name": recipe.name,
        "implementation": recipe.implementation,
        "default_model": default_model,
        "default_dims": recipe.touchpoint.default_dims,
        "auth_env": recipe.auth_env,
        "base_url_default": recipe.base_url_default,
        "base_url_env": base_url_env,
        "gateway_env": gateway_env,
        "config_path": "~/.kp/config.json",
        "config_override_key": config_key,
        "example_config": example_cfg,
    });

    let auth_line = if recipe.auth_env.is_empty() {
        "Auth env: (none — local ONNX cache in ~/.kp/models/)".to_owned()
    } else {
        format!("Auth env: {}", recipe.auth_env.join(" | "))
    };
    let mut human = format!(
        "{} ({})\n{}\nDefault base URL: {}\nEnv URL override: {}{}\nConfig (0600 ~/.kp/config.json): {}\nTest command: kp providers test --model {}",
        recipe.name,
        recipe.id,
        auth_line,
        base_default,
        base_url_env,
        if gateway_env.is_some() {
            " (or OPENAI_BASE_URL)"
        } else {
            ""
        },
        config_key,
        default_model
    );
    if !example_cfg.is_empty() {
        human.push_str(&format!(
            "\nExample ~/.kp/config.json:\n{}",
            serde_json::to_string_pretty(&example_cfg)?
        ));
    }

    output(cli.json, &val, &human)?;
    Ok(())
}

async fn run_providers_test(cli: &Cli, model: Option<&str>, dim: Option<usize>) -> Result<()> {
    let spec = match model.map(str::trim).filter(|s| !s.is_empty()) {
        Some(explicit) => parse_embedding_model_spec(explicit, dim)?,
        None => {
            if let Ok(ro) = Store::open_readonly(&cli.path) {
                let (m, d) = ro.embedding_config()?;
                let effective_dim = dim.or_else(|| (d > 0).then_some(d));
                parse_embedding_model_spec(&m, effective_dim)?
            } else {
                parse_embedding_model_spec(DEFAULT_LOCAL_EMBEDDING_MODEL, dim)?
            }
        }
    };

    let report = match spec.provider_id.as_str() {
        "none" => {
            return Err(
                KpError::invalid("model", "cannot test connectivity for `none` embedder").into(),
            );
        }
        "hash" => {
            let started = Instant::now();
            let vec = HashEmbedder.embed("Kipo embedding connectivity probe")?;
            ProviderTestReport {
                model_id: spec.model_id,
                provider_id: "hash".to_owned(),
                endpoint_url: "builtin://hash:v1".to_owned(),
                returned_dims: vec.len(),
                latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            }
        }
        "local" => test_local_onnx_provider(&spec.model_id, spec.dims).await?,
        _ => {
            let global_cfg = load_global_config()?;
            let embedder = ProviderEmbedder::from_env_and_config(
                ProviderEmbedderConfig::new(spec),
                Some(&global_cfg),
            )?;
            embedder.test_connectivity().await?
        }
    };

    let human = format!(
        "OK {} ({}) -> {}d in {}ms",
        report.model_id, report.endpoint_url, report.returned_dims, report.latency_ms
    );
    output(cli.json, &report, &human)?;
    Ok(())
}

async fn test_local_onnx_provider(
    model_id: &str,
    expected_dims: usize,
) -> Result<ProviderTestReport> {
    #[cfg(feature = "local-embed")]
    {
        let target_model = model_id.to_owned();
        let rep = tokio::task::spawn_blocking(
            move || -> std::result::Result<ProviderTestReport, String> {
                let mut buckets = BTreeMap::new();
                let embedder = kp_store::LocalOnnxEmbedder::ensure_downloaded_and_load(
                    &target_model,
                    |prog| render_download_progress_stderr(&prog, &mut buckets),
                )
                .map_err(|e| e.to_string())?;
                let started = Instant::now();
                let vec = embedder
                    .embed("Kipo embedding connectivity probe")
                    .map_err(|e| e.to_string())?;
                if expected_dims > 0 && vec.len() != expected_dims {
                    return Err(format!(
                        "`{target_model}` expected {expected_dims}d vector, local ONNX returned {}d; run `kp embed --stale --to {target_model} --dim {}`",
                        vec.len(),
                        vec.len()
                    ));
                }
                Ok(ProviderTestReport {
                    model_id: embedder.model_id().to_owned(),
                    provider_id: "local".to_owned(),
                    endpoint_url: "local://onnxruntime".to_owned(),
                    returned_dims: vec.len(),
                    latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                })
            },
        )
        .await
        .map_err(|e| format!("local ONNX test task failed: {e}"))??;
        Ok(rep)
    }
    #[cfg(not(feature = "local-embed"))]
    {
        let _ = (model_id, expected_dims);
        Err(
            StoreError::Embedding("`kp` was compiled without the `local-embed` feature".into())
                .into(),
        )
    }
}

async fn build_active_reembed_embedder(
    spec: &kp_store::ResolvedEmbeddingSpec,
) -> Result<Arc<dyn Embedder + Send + Sync>> {
    match spec.provider_id.as_str() {
        "none" => Ok(Arc::new(FtsOnlyEmbedder)),
        "hash" => Ok(Arc::new(HashEmbedder)),
        "local" => {
            #[cfg(feature = "local-embed")]
            {
                let model_id = spec.model_id.clone();
                let loaded = tokio::task::spawn_blocking(
                    move || -> kp_store::Result<kp_store::LocalOnnxEmbedder> {
                        let mut buckets = BTreeMap::new();
                        kp_store::LocalOnnxEmbedder::ensure_downloaded_and_load(&model_id, |prog| {
                            render_download_progress_stderr(&prog, &mut buckets)
                        })
                    },
                )
                .await
                .map_err(|e| format!("local ONNX load task failed: {e}"))??;
                Ok(Arc::new(loaded))
            }
            #[cfg(not(feature = "local-embed"))]
            {
                Err(StoreError::Embedding(
                    "`kp` was compiled without the `local-embed` feature".into(),
                )
                .into())
            }
        }
        _ => {
            let global_cfg = load_global_config()?;
            let embedder = ProviderEmbedder::from_env_and_config(
                ProviderEmbedderConfig::new(spec.clone()),
                Some(&global_cfg),
            )?;
            Ok(Arc::new(embedder))
        }
    }
}

async fn run_embed_stale(cli: &Cli, to: Option<&str>, dim: Option<usize>) -> Result<()> {
    let store_ro = Store::open_readonly(&cli.path)?;
    let actor = cli
        .actor
        .clone()
        .unwrap_or_else(|| store_ro.owner_actor().clone());
    let Some(role) = store_ro.member_role(&actor)? else {
        return Err(StoreError::PermissionDenied.into());
    };
    if !matches!(role, MemberRole::Owner | MemberRole::Maintainer) {
        return Err(StoreError::PermissionDenied.into());
    }
    let (cur_model, cur_dims) = store_ro.embedding_config()?;
    drop(store_ro);

    let (spec, migrate) = match to.map(str::trim).filter(|s| !s.is_empty()) {
        Some(target) => (parse_embedding_model_spec(target, dim)?, true),
        None => {
            if dim.is_some() {
                return Err(
                    KpError::invalid("dim", "`--dim` requires `--to <provider>:<model>`").into(),
                );
            }
            (
                parse_embedding_model_spec(&cur_model, (cur_dims > 0).then_some(cur_dims))?,
                false,
            )
        }
    };

    let embedder = build_active_reembed_embedder(&spec).await?;
    let lib_path = cli.path.clone();
    let target_model = spec.model_id.clone();
    let target_dims = spec.dims;

    let report =
        tokio::task::spawn_blocking(move || -> kp_store::Result<kp_store::ReembedReport> {
            let mut store = Store::open(&lib_path)?;
            let migrate_arg = migrate.then_some((target_model.as_str(), target_dims));
            let rep = store.reembed_stale(embedder.as_ref(), migrate_arg)?;
            store.set_embedder(embedder);
            Ok(rep)
        })
        .await
        .map_err(|e| format!("re-embed worker failed: {e}"))??;

    let human = format!(
        "Re-embedded with {} ({}d): {} chunks, {} facts ({} chunks up-to-date, {} facts up-to-date, {} raw-only skipped)",
        report.model_id,
        report.dims,
        report.chunks_reembedded,
        report.facts_reembedded,
        report.chunks_up_to_date,
        report.facts_up_to_date,
        report.skipped_raw_only
    );
    output(cli.json, &report, &human)?;
    Ok(())
}

fn run_export(cli: &Cli, dir: &Path, requested_at: Option<i64>) -> Result<()> {
    let mut store = Store::open(&cli.path)?;
    let viewer = cli
        .actor
        .clone()
        .unwrap_or_else(|| store.owner_actor().clone());
    let export_at = requested_at.map(EpochMs).unwrap_or(now()?);
    let report = store.export_markdown_directory(dir, &viewer, export_at)?;
    let human = format!(
        "Exported {} pages ({} visible facts, {} visible sources) for '{}' to {}",
        report.pages.len(),
        report.visible_facts,
        report.visible_sources,
        report.viewer,
        dir.display()
    );
    output(cli.json, &report, &human)?;
    Ok(())
}

async fn run_apply(cli: &Cli, dir: &Path, requested_at: Option<i64>) -> Result<()> {
    let mut store = Store::open(&cli.path)?;
    attach_ready_embedder(&mut store);
    let actor = cli
        .actor
        .clone()
        .unwrap_or_else(|| store.owner_actor().clone());
    let apply_at = requested_at.map(EpochMs).unwrap_or(now()?);
    let global_policy = crate::governance::explicit_writeback_default_policy();
    let report =
        store.apply_markdown_directory_with_policy(dir, &actor, &global_policy, apply_at)?;

    if !report.unchanged
        && (!report.updated_facts.is_empty() || !report.deleted_fact_ids.is_empty())
    {
        let _ = kp_derive::derive(&mut store)?;
        crate::community::notify_or_sync_joined(&cli.path, &mut store, apply_at).await;
    }

    let human = if report.unchanged {
        format!(
            "unchanged (projection at {} matches baseline seq #{})",
            dir.display(),
            report.base_seq
        )
    } else if report.baseline_conflict {
        format!(
            "Baseline conflict (exported seq #{}, current seq #{}): {} proposal(s) queued in `kp review list` (zero text loss)",
            report.base_seq,
            report.current_seq,
            report.enqueued_proposals.len()
        )
    } else if !report.enqueued_proposals.is_empty() {
        format!(
            "Applied writeback ({} updated, {} withdrawn) and queued {} proposal(s) in `kp review list` (zero text loss)",
            report.updated_facts.len(),
            report.deleted_fact_ids.len(),
            report.enqueued_proposals.len()
        )
    } else {
        format!(
            "Applied writeback ({} updated, {} withdrawn)",
            report.updated_facts.len(),
            report.deleted_fact_ids.len()
        )
    };
    output(cli.json, &report, &human)?;
    Ok(())
}

fn run_log(cli: &Cli, limit: usize, target: Option<&str>, requested_at: Option<i64>) -> Result<()> {
    let store = match Store::open_readonly(&cli.path) {
        Ok(ro) => ro,
        Err(_) => Store::open(&cli.path)?,
    };
    let actor = cli
        .actor
        .clone()
        .unwrap_or_else(|| store.owner_actor().clone());
    let eval_at = requested_at.map(EpochMs).unwrap_or(now()?);
    let entries = store.visible_change_log(&actor, eval_at, limit, target)?;
    let human = if entries.is_empty() {
        "(no visible changes)".to_owned()
    } else {
        entries
            .iter()
            .map(|e| {
                let who = e.actor_id.as_ref().map_or("-", ActorId::as_str);
                format!(
                    "#{}  {}ms  [{}]  {}  {}  — {}",
                    e.seq, e.created_at.0, e.op, who, e.target_id, e.summary
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    output(cli.json, &entries, &human)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn two_cloud_choice() -> Result<InitEmbeddingChoice> {
        let env = BTreeMap::from([
            ("DASHSCOPE_API_KEY".to_owned(), "sk-dash".to_owned()),
            ("ARK_API_KEY".to_owned(), "sk-ark".to_owned()),
        ]);
        Ok(resolve_init_embedding_choice(
            false,
            None,
            None,
            |k| env.get(k).cloned(),
            None,
        )?)
    }

    #[test]
    fn multi_cloud_selector_handles_interactive_second_provider_local_none_and_noninteractive(
    ) -> Result<()> {
        // 1. Interactive choice `2` -> picks 2nd cloud provider (`volcengine`) + updates privacy notice.
        let mut c2 = two_cloud_choice()?;
        assert_eq!(c2.detected_cloud_providers.len(), 2);
        let mut rdr2 = Cursor::new(b"2\n".to_vec());
        let mut err2 = Vec::new();
        select_among_multiple_detected_providers(&mut c2, None, true, &mut rdr2, &mut err2)?;
        assert_eq!(c2.selected.provider_id, "volcengine");
        assert!(c2
            .cloud_privacy_notice
            .as_deref()
            .is_some_and(|n| n.contains("volcengine:")));

        // 2. Interactive choice `3` (`local:bge-small-zh-v1.5`) -> clears cloud privacy notice.
        let mut clocal = two_cloud_choice()?;
        let mut rdr_local = Cursor::new(b"3\n".to_vec());
        let mut err_local = Vec::new();
        select_among_multiple_detected_providers(
            &mut clocal,
            None,
            true,
            &mut rdr_local,
            &mut err_local,
        )?;
        assert_eq!(clocal.selected.model_id, DEFAULT_LOCAL_EMBEDDING_MODEL);
        assert_eq!(clocal.selected.dims, 512);
        assert!(clocal.cloud_privacy_notice.is_none());

        // 3. Interactive choice `4` (`none`) -> pure FTS5 (`0d`).
        let mut cnone = two_cloud_choice()?;
        let mut rdr_none = Cursor::new(b"4\n".to_vec());
        let mut err_none = Vec::new();
        select_among_multiple_detected_providers(
            &mut cnone,
            None,
            true,
            &mut rdr_none,
            &mut err_none,
        )?;
        assert_eq!(cnone.selected.model_id, "none");
        assert_eq!(cnone.selected.dims, 0);
        assert!(cnone.cloud_privacy_notice.is_none());

        // 4. Non-interactive -> deterministic `detected_cloud_providers[0]` (`dashscope`) + explicit stderr notice.
        let mut cnon = two_cloud_choice()?;
        let mut rdr_empty = Cursor::new(Vec::<u8>::new());
        let mut err_non = Vec::new();
        select_among_multiple_detected_providers(
            &mut cnon,
            None,
            false,
            &mut rdr_empty,
            &mut err_non,
        )?;
        assert_eq!(cnon.selected.provider_id, "dashscope");
        let msg = String::from_utf8(err_non)?;
        assert!(msg.contains("dashscope:text-embedding-v3"));
        assert!(msg.contains("volcengine:"));
        assert!(msg.contains("--embedding-model"));
        Ok(())
    }

    #[test]
    fn swappable_embedder_tracks_download_progress_ready_error_and_step_formatting() -> Result<()> {
        let sw = SwappableEmbedder::new_with_status(
            DEFAULT_LOCAL_EMBEDDING_MODEL,
            512,
            Arc::new(DisabledModelEmbedder::new(
                DEFAULT_LOCAL_EMBEDDING_MODEL.to_owned(),
                512,
            )),
            true,
            None,
        );
        assert_eq!(sw.status_snapshot().state, "downloading");
        assert_eq!(sw.status_snapshot().download_pct, Some(0));
        assert!(sw.status_snapshot().last_error.is_none());

        sw.update_download_progress(45);
        assert_eq!(sw.status_snapshot().state, "downloading");
        assert_eq!(sw.status_snapshot().download_pct, Some(45));

        sw.mark_error("SHA-256 digest mismatch");
        assert_eq!(sw.status_snapshot().state, "error");
        assert_eq!(
            sw.status_snapshot().last_error.as_deref(),
            Some("SHA-256 digest mismatch")
        );

        sw.swap(Arc::new(HashEmbedder));
        assert_eq!(sw.status_snapshot().state, "ready");
        assert_eq!(sw.status_snapshot().download_pct, Some(100));
        assert!(sw.status_snapshot().last_error.is_none());

        let mut buckets = BTreeMap::new();
        assert!(format_download_progress_step(
            "onnx/model_quantized.onnx",
            0,
            Some(1_000),
            false,
            &mut buckets
        )
        .is_some_and(|s| s.contains("downloading")));
        assert!(format_download_progress_step(
            "onnx/model_quantized.onnx",
            420,
            Some(1_000),
            false,
            &mut buckets
        )
        .is_some_and(|s| s.contains("40%")));
        assert!(format_download_progress_step(
            "onnx/model_quantized.onnx",
            450,
            Some(1_000),
            false,
            &mut buckets
        )
        .is_none());
        assert!(format_download_progress_step(
            "onnx/model_quantized.onnx",
            1_000,
            Some(1_000),
            true,
            &mut buckets
        )
        .is_some_and(|s| s.contains("SHA-256 OK")));
        Ok(())
    }

    #[test]
    fn daemon_swappable_embedder_records_redacted_last_error_for_missing_key_and_invalid_gateway(
    ) -> Result<()> {
        let dir = tempfile::tempdir()?;

        // 1. Cloud recipe with missing API key -> `fts5_fallback` with non-empty `last_error` naming key env.
        let missing_dir = dir.path().join("missing-key");
        let missing_store = Store::init_with_embedding(
            &missing_dir,
            InitOptions {
                library_id: LibraryId::new("diag-missing")?,
                name: "Diag Missing".into(),
                mode: LibraryMode::Personal,
                owner: "alice".parse()?,
                created_at: EpochMs(0),
            },
            "voyage:voyage-3-lite",
            512,
        )?;
        let empty_cfg = KpGlobalConfig::default();
        let (sw_missing, pending_missing) =
            build_daemon_swappable_embedder_with_config(&missing_store, Some(&empty_cfg));
        assert!(pending_missing.is_none());
        if std::env::var_os("VOYAGE_API_KEY").is_none() {
            let st = sw_missing.status_snapshot();
            assert_eq!(st.state, "fts5_fallback");
            let err_msg = st
                .last_error
                .as_deref()
                .ok_or("expected last_error for missing VOYAGE_API_KEY")?;
            assert!(
                err_msg.contains("VOYAGE_API_KEY") || err_msg.contains("API key"),
                "unexpected missing-key diagnostic: {err_msg}"
            );
        }
        // Also verify lookup-isolated diagnostic deterministically regardless of ambient env.
        let voyage_spec = parse_embedding_model_spec("voyage:voyage-3-lite", Some(512))?;
        let isolated_missing = kp_store::diagnose_embedder_spec_issue_with_lookup(
            &voyage_spec,
            |_| None,
            Some(&empty_cfg),
        )
        .ok_or("expected lookup-isolated missing-key diagnostic")?;
        assert!(isolated_missing.contains("VOYAGE_API_KEY"));

        // 2. Cloud recipe with malformed explicit gateway URL -> `fts5_fallback` + accurate `last_error`,
        //    and any secret API key remains scrubbed.
        let bad_gw_dir = dir.path().join("bad-gw");
        let bad_gw_store = Store::init_with_embedding(
            &bad_gw_dir,
            InitOptions {
                library_id: LibraryId::new("diag-bad-gw")?,
                name: "Diag Bad Gateway".into(),
                mode: LibraryMode::Personal,
                owner: "alice".parse()?,
                created_at: EpochMs(0),
            },
            "dashscope:text-embedding-v3",
            1024,
        )?;
        let raw_secret = "sk-testsecret9876543210abcdef123456";
        let mut bad_cfg = KpGlobalConfig::default();
        bad_cfg
            .api_keys
            .insert("DASHSCOPE_API_KEY".to_owned(), raw_secret.to_owned());
        bad_cfg.provider_base_urls.insert(
            "dashscope".to_owned(),
            "ftp://invalid-gateway.internal/v1".to_owned(),
        );
        let (sw_bad_gw, _) =
            build_daemon_swappable_embedder_with_config(&bad_gw_store, Some(&bad_cfg));
        let bad_st = sw_bad_gw.status_snapshot();
        assert_eq!(bad_st.state, "fts5_fallback");
        let gw_err = bad_st
            .last_error
            .as_deref()
            .ok_or("expected last_error for invalid gateway URL")?;
        assert!(!gw_err.is_empty());
        assert!(!gw_err.contains(raw_secret));
        assert!(
            gw_err.contains("http") || gw_err.contains("base_url") || gw_err.contains("scheme"),
            "unexpected invalid-gateway diagnostic: {gw_err}"
        );

        // 3. Unknown provider spec persisted in `library_meta` -> `fts5_fallback` + non-empty `last_error`.
        let bad_spec_dir = dir.path().join("bad-spec");
        let bad_spec_store = Store::init_with_embedding(
            &bad_spec_dir,
            InitOptions {
                library_id: LibraryId::new("diag-bad-spec")?,
                name: "Diag Bad Spec".into(),
                mode: LibraryMode::Personal,
                owner: "alice".parse()?,
                created_at: EpochMs(0),
            },
            "unknown-provider:model-x",
            256,
        )?;
        let (sw_bad_spec, _) =
            build_daemon_swappable_embedder_with_config(&bad_spec_store, Some(&empty_cfg));
        let spec_st = sw_bad_spec.status_snapshot();
        assert_eq!(spec_st.state, "fts5_fallback");
        assert!(spec_st
            .last_error
            .as_deref()
            .is_some_and(|m| m.contains("unknown-provider")));
        Ok(())
    }
}
