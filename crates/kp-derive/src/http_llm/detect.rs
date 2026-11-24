use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kp_model::{KpError, LlmClient, ModelRoutePolicy, Result};
use serde::Deserialize;

use super::probes::detect_logged_in_host_cli;
use super::{
    validate_gateway_base_url, FallbackLlmClient, GenerativeApiProvider, HttpApiConfig,
    HttpApiLlmClient, RawOnlyLlmClient,
};
use crate::llm_runner::{HostCliConfig, HostCliKind, HostCliRunner};

/// Explicit named generative endpoint entry under `named_endpoints.<name>` in `0600` `~/.kp/config.json`.
#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedEndpointConfig {
    /// Provider wire/recipe family (`"openai"`, `"anthropic"`, `"dashscope"`, `"volcengine"`,
    /// `"zhipu"`, `"openrouter"`). Defaults to `"openai"` when `base_url` is set.
    #[serde(default)]
    pub provider: Option<String>,
    /// Target model identifier on the endpoint (e.g. `"qwen2.5-72b-instruct"`).
    #[serde(default)]
    pub model: Option<String>,
    /// Optional gateway base URL (validated strictly via [`validate_gateway_base_url`]).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Optional inline API key stored inside `0600` `~/.kp/config.json`.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Optional environment variable name supplying the API key for this endpoint.
    #[serde(default)]
    pub api_key_env: Option<String>,
}

impl fmt::Debug for NamedEndpointConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NamedEndpointConfig")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("api_key_env", &self.api_key_env)
            .finish()
    }
}

/// Subset of `~/.kp/config.json` (`$KP_CONFIG_PATH`) relevant to global generative model detection.
/// Read ONLY when file permissions on Unix have zero group/other access bits (`0600`).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct KpGlobalModelConfig {
    #[serde(default)]
    pub model_policy: Option<String>,
    #[serde(default)]
    pub generative_model: Option<String>,
    #[serde(default)]
    pub api_keys: BTreeMap<String, String>,
    #[serde(default)]
    pub provider_base_urls: BTreeMap<String, String>,
    #[serde(default)]
    pub named_endpoints: BTreeMap<String, NamedEndpointConfig>,
}

/// Loads `~/.kp/config.json` if it exists and satisfies strict `0600` (owner-only) permissions on Unix.
pub fn load_secure_kp_config(path: &Path) -> Option<KpGlobalModelConfig> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return None;
        }
    }
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<KpGlobalModelConfig>(&text).ok()
}

/// Hermetically injectable environment snapshot for [`detect_global_generative_model`].
#[derive(Clone, Debug, Default)]
pub struct ModelDetectEnv {
    pub env_vars: BTreeMap<String, String>,
    pub config_path: Option<PathBuf>,
    /// Optional explicit binary search directories; when `None`, uses `PATH` + standard user/system dirs.
    pub search_dirs: Option<Vec<PathBuf>>,
}

impl ModelDetectEnv {
    pub fn from_process() -> Self {
        Self {
            env_vars: std::env::vars().collect(),
            config_path: None,
            search_dirs: None,
        }
    }

    pub fn from_env_map<I, K, V>(iter: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self {
            env_vars: iter
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
            config_path: None,
            search_dirs: None,
        }
    }

    pub fn with_config_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_path = Some(path.into());
        self
    }

    pub fn with_search_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.search_dirs = Some(dirs);
        self
    }

    pub(super) fn get_env(&self, key: &str) -> Option<&str> {
        self.env_vars
            .get(key)
            .map(String::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    pub(super) fn resolved_config_path(&self) -> Option<PathBuf> {
        if let Some(explicit) = &self.config_path {
            return Some(explicit.clone());
        }
        if let Some(env_cfg) = self.get_env("KP_CONFIG_PATH") {
            return Some(PathBuf::from(env_cfg));
        }
        let home = self.get_env("HOME")?;
        Some(Path::new(home).join(".kp").join("config.json"))
    }
}

/// Detected host CLI configuration (Codex or Claude Code) with optional gateway forwarding
/// and optional backup HTTP API failover (`FallbackLlmClient`) for CLI auth/spawn errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetectedHostCli {
    pub kind: HostCliKind,
    pub program: PathBuf,
    pub model: Option<String>,
    pub codex_config_path: Option<PathBuf>,
    pub gateway_base_url: Option<String>,
    pub extra_env: BTreeMap<String, String>,
    pub fallback_http: Option<HttpApiConfig>,
}

impl DetectedHostCli {
    /// Instantiates [`HostCliRunner`], automatically wrapping it in [`FallbackLlmClient`] when a
    /// backup [`HttpApiConfig`] (`fallback_http`) was detected alongside the logged-in CLI.
    pub fn build_llm_client(&self) -> Result<Arc<dyn LlmClient>> {
        self.build_llm_client_with_overrides(None, None)
    }

    /// Instantiates [`HostCliRunner`] with optional `--model` and `--remaining-quota-pct`
    /// overrides while **preserving** [`FallbackLlmClient`] failover to any detected backup API key.
    pub fn build_llm_client_with_overrides(
        &self,
        model_override: Option<&str>,
        remaining_quota_pct_override: Option<u8>,
    ) -> Result<Arc<dyn LlmClient>> {
        let mut cfg = HostCliConfig::for_kind(self.kind);
        cfg.program = self.program.clone();
        cfg.model = model_override
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| self.model.clone());
        cfg.codex_config_path = self.codex_config_path.clone();
        cfg.route_policy = ModelRoutePolicy::HostCli;
        if remaining_quota_pct_override.is_some() {
            cfg.remaining_quota_pct = remaining_quota_pct_override;
        }
        let primary: Arc<dyn LlmClient> =
            Arc::new(HostCliRunner::new(cfg)?.with_env_overrides(self.extra_env.clone()));
        if let Some(backup_cfg) = &self.fallback_http {
            let mut bcfg = backup_cfg.clone();
            if remaining_quota_pct_override.is_some() {
                bcfg.remaining_quota_pct = remaining_quota_pct_override;
            }
            let secondary: Arc<dyn LlmClient> = Arc::new(HttpApiLlmClient::new(bcfg)?);
            return Ok(Arc::new(FallbackLlmClient::new(primary, secondary)));
        }
        Ok(primary)
    }
}

/// Result of zero-configuration global generative model auto-detection (`docs/ARCHITECTURE.md`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DetectedGenerativeModel {
    HostCli(DetectedHostCli),
    HttpApi(HttpApiConfig),
    RawOnly,
}

impl DetectedGenerativeModel {
    /// Constructs a fail-closed invalid-gateway state: `route_policy()` returns
    /// [`ModelRoutePolicy::RawOnly`] so local L0 archival and FTS5 continue without disruption,
    /// while [`Self::build_llm_client`] returns a clear `base_url` validation error with zero
    /// public-cloud egress.
    pub fn invalid_gateway(provider: GenerativeApiProvider, reason: impl Into<String>) -> Self {
        let mut cfg = HttpApiConfig::for_provider(provider, String::new());
        cfg.base_url.clear();
        cfg.is_custom_gateway = true;
        cfg.route_policy = ModelRoutePolicy::RawOnly;
        cfg.invalid_state = Some((
            "base_url",
            "explicitly configured gateway base_url failed validation",
            reason.into(),
        ));
        Self::HttpApi(cfg)
    }

    fn invalid_policy(
        field: &'static str,
        static_reason: &'static str,
        detail: impl Into<String>,
    ) -> Self {
        let mut cfg = HttpApiConfig::for_provider(GenerativeApiProvider::OpenAi, String::new());
        cfg.base_url.clear();
        cfg.route_policy = ModelRoutePolicy::RawOnly;
        cfg.invalid_state = Some((field, static_reason, detail.into()));
        Self::HttpApi(cfg)
    }

    /// Returns `true` when an explicitly configured gateway base URL failed validation.
    pub fn is_invalid_gateway(&self) -> bool {
        self.invalid_gateway_reason().is_some()
    }

    /// Returns the validation failure reason when an explicit gateway base URL is malformed.
    pub fn invalid_gateway_reason(&self) -> Option<&str> {
        match self {
            Self::HttpApi(cfg) => cfg.invalid_gateway_reason(),
            Self::HostCli(_) | Self::RawOnly => None,
        }
    }

    /// Human-readable one-line label for connector startup notices and `kp status`.
    pub fn display_label(&self) -> String {
        match self {
            Self::RawOnly => "Raw-Only (local FTS5)".to_owned(),
            Self::HostCli(cli) => match (cli.kind, cli.gateway_base_url.is_some()) {
                (HostCliKind::Codex, false) => "Codex / OpenAI".to_owned(),
                (HostCliKind::Codex, true) => "Codex / OpenAI (Gateway)".to_owned(),
                (HostCliKind::ClaudeCode, false) => "Claude Code".to_owned(),
                (HostCliKind::ClaudeCode, true) => "Claude Code (Gateway)".to_owned(),
            },
            Self::HttpApi(cfg) => {
                if let Some((field, _, detail)) = &cfg.invalid_state {
                    return if *field == "base_url" {
                        format!("Invalid Gateway ({detail})")
                    } else {
                        format!("Invalid Model Policy ({detail})")
                    };
                }
                if cfg.route_policy == ModelRoutePolicy::RawOnly {
                    return "Raw-Only (local FTS5)".to_owned();
                }
                match (cfg.provider, cfg.is_custom_gateway) {
                    (GenerativeApiProvider::OpenAi, true) => "OpenAI Compatible Gateway".to_owned(),
                    (GenerativeApiProvider::OpenAi, false) => "OpenAI API".to_owned(),
                    (GenerativeApiProvider::Anthropic, true) => {
                        "Anthropic Compatible Gateway".to_owned()
                    }
                    (GenerativeApiProvider::Anthropic, false) => "Anthropic API".to_owned(),
                    (GenerativeApiProvider::DashScope, _) => "DashScope (Qwen)".to_owned(),
                    (GenerativeApiProvider::Volcengine, _) => "Volcengine Ark (Doubao)".to_owned(),
                    (GenerativeApiProvider::Zhipu, _) => "Zhipu AI (GLM)".to_owned(),
                    (GenerativeApiProvider::OpenRouter, _) => "OpenRouter".to_owned(),
                }
            }
        }
    }

    pub fn route_policy(&self) -> ModelRoutePolicy {
        match self {
            Self::HostCli(_) => ModelRoutePolicy::HostCli,
            Self::HttpApi(cfg) => cfg.route_policy.clone(),
            Self::RawOnly => ModelRoutePolicy::RawOnly,
        }
    }

    /// Instantiates the ready-to-run `Arc<dyn LlmClient>` for this detected model.
    pub fn build_llm_client(&self) -> Result<Arc<dyn LlmClient>> {
        self.build_llm_client_with_overrides(None, None)
    }

    /// Instantiates the `Arc<dyn LlmClient>` with optional `--model` and `--remaining-quota-pct`
    /// overrides, preserving [`FallbackLlmClient`] backup-key failover when `HostCli` is primary
    /// and still failing closed when an explicit gateway URL is malformed.
    pub fn build_llm_client_with_overrides(
        &self,
        model_override: Option<&str>,
        remaining_quota_pct_override: Option<u8>,
    ) -> Result<Arc<dyn LlmClient>> {
        match self {
            Self::RawOnly => Ok(Arc::new(RawOnlyLlmClient)),
            Self::HostCli(cli) => {
                cli.build_llm_client_with_overrides(model_override, remaining_quota_pct_override)
            }
            Self::HttpApi(cfg_in) => {
                if let Some((field, static_reason, _)) = &cfg_in.invalid_state {
                    return Err(KpError::invalid(field, static_reason));
                }
                if cfg_in.route_policy == ModelRoutePolicy::RawOnly {
                    return Ok(Arc::new(RawOnlyLlmClient));
                }
                let mut cfg = cfg_in.clone();
                if let Some(m) = model_override.map(str::trim).filter(|s| !s.is_empty()) {
                    cfg = cfg.with_model(m);
                }
                if remaining_quota_pct_override.is_some() {
                    cfg.remaining_quota_pct = remaining_quota_pct_override;
                }
                Ok(Arc::new(HttpApiLlmClient::new(cfg)?))
            }
        }
    }
}

/// Resolves an explicit `ModelRoutePolicy::NamedEndpoint(endpoint_name)` into a validated
/// [`HttpApiConfig`] from `0600` `~/.kp/config.json`'s `named_endpoints.<name>` or an explicit
/// `<provider>:<model>` / `<provider>` spec.
///
/// Fails closed immediately when `endpoint_name` is unknown, unconfigured, missing an API key,
/// or carries an invalid gateway URL — **never** falling back to global default cloud models.
pub fn resolve_named_endpoint_config(
    endpoint_name: &str,
    env: &ModelDetectEnv,
) -> Result<HttpApiConfig> {
    let file_cfg = env
        .resolved_config_path()
        .and_then(|p| load_secure_kp_config(&p))
        .unwrap_or_default();
    resolve_named_endpoint_config_with_file(endpoint_name, env, &file_cfg)
}

/// Resolves and constructs an `Arc<dyn LlmClient>` for `ModelRoutePolicy::NamedEndpoint(endpoint_name)`.
///
/// Immediately returns `Err` when `endpoint_name` is unconfigured, unknown, missing credentials,
/// or points to an invalid gateway URL — **never** falling back to global default cloud models.
pub fn build_named_endpoint_llm_client(
    endpoint_name: &str,
    env: &ModelDetectEnv,
) -> Result<Arc<dyn LlmClient>> {
    let cfg = resolve_named_endpoint_config(endpoint_name, env)?;
    Ok(Arc::new(HttpApiLlmClient::new(cfg)?))
}

fn resolve_named_endpoint_config_with_file(
    endpoint_name: &str,
    env: &ModelDetectEnv,
    file_cfg: &KpGlobalModelConfig,
) -> Result<HttpApiConfig> {
    let trimmed = endpoint_name.trim();
    if trimmed.is_empty() || trimmed.chars().any(char::is_control) {
        return Err(KpError::invalid(
            "model_route_policy",
            "named endpoint identifier must be nonempty",
        ));
    }

    if let Some(entry) = file_cfg.named_endpoints.get(trimmed) {
        let provider = match entry
            .provider
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(p) => GenerativeApiProvider::from_id(p).ok_or_else(|| {
                KpError::invalid(
                    "model_route_policy",
                    "unknown provider in named_endpoints configuration",
                )
            })?,
            None if entry
                .base_url
                .as_deref()
                .map(str::trim)
                .is_some_and(|u| !u.is_empty()) =>
            {
                GenerativeApiProvider::OpenAi
            }
            None => {
                return Err(KpError::invalid(
                    "model_route_policy",
                    "named_endpoints entry must specify provider or base_url",
                ));
            }
        };

        let explicit_base = entry
            .base_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let resolved_custom_base = match explicit_base {
            Some(raw_url) => {
                validate_gateway_base_url(raw_url)?;
                Some(raw_url.trim_end_matches('/').to_owned())
            }
            None => resolve_custom_base_url(env, file_cfg, provider)?,
        };

        let explicit_key_env = entry
            .api_key_env
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let resolved_key = entry
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                explicit_key_env.and_then(|env_name| {
                    env.get_env(env_name).map(str::to_owned).or_else(|| {
                        file_cfg
                            .api_keys
                            .get(env_name)
                            .map(String::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                    })
                })
            })
            .or_else(|| {
                if explicit_key_env.is_none() {
                    lookup_api_key(env, file_cfg, provider)
                } else {
                    None
                }
            })
            .or_else(|| {
                // Allow keyless loopback servers (e.g. local vLLM / Ollama) when no `api_key_env` was mandated.
                if explicit_key_env.is_none()
                    && resolved_custom_base
                        .as_deref()
                        .is_some_and(is_loopback_gateway_url)
                {
                    Some("local-loopback".to_owned())
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                KpError::invalid("api_key", "missing API key for configured named endpoint")
            })?;

        let mut cfg = HttpApiConfig::for_provider(provider, resolved_key);
        if let Some(base_url) = resolved_custom_base {
            cfg = cfg.with_base_url(base_url);
        }
        if let Some(model) = entry
            .model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            cfg = cfg.with_model(model);
        }
        cfg.route_policy = ModelRoutePolicy::NamedEndpoint(trimmed.to_owned());
        cfg.resolved_endpoint_url()?;
        return Ok(cfg);
    }

    let (prov_part, model_override) = match trimmed.split_once(':') {
        Some((p, m)) => {
            let clean_model = m.trim();
            if clean_model.is_empty() {
                return Err(KpError::invalid(
                    "model_route_policy",
                    "missing model name after colon in named endpoint",
                ));
            }
            (p, Some(clean_model))
        }
        None => (trimmed, None),
    };

    let Some(provider) = GenerativeApiProvider::from_id(prov_part) else {
        return Err(KpError::invalid(
            "model_route_policy",
            "unconfigured or unknown named endpoint",
        ));
    };

    let custom_url = resolve_custom_base_url(env, file_cfg, provider)?;
    let api_key = lookup_api_key(env, file_cfg, provider)
        .ok_or_else(|| KpError::invalid("api_key", "missing API key for named endpoint"))?;
    let mut cfg = HttpApiConfig::for_provider(provider, api_key);
    if let Some(url) = custom_url {
        cfg = cfg.with_base_url(url);
    }
    if let Some(model) = lookup_model_override(env, file_cfg, provider, model_override) {
        cfg = cfg.with_model(model);
    }
    cfg.route_policy = ModelRoutePolicy::NamedEndpoint(trimmed.to_owned());
    cfg.resolved_endpoint_url()?;
    Ok(cfg)
}

/// Auto-detects the single global generative model in strict §0.2.1 priority order:
/// 1. Logged-in local Host CLI (`codex` / `claude`), unless an explicit HTTP API gateway + key override is set;
/// 2. Existing API key in environment variables or `0600` `~/.kp/config.json` (`$KP_CONFIG_PATH`);
/// 3. Seamless non-blocking fallback to [`DetectedGenerativeModel::RawOnly`].
///
/// Security & compliance fail-closed guarantees:
/// - If any explicit gateway base URL (`OPENAI_BASE_URL`, `ANTHROPIC_BASE_URL`, `<PROVIDER>_BASE_URL`,
///   or `provider_base_urls`) is set to a malformed URL, returns [`DetectedGenerativeModel::invalid_gateway`]
///   (`route_policy() == RawOnly`, `build_llm_client() == Err`) and NEVER falls through to public cloud!
/// - If both a logged-in Host CLI and a backup API key are available, attaches `fallback_http` so
///   runtime CLI auth/spawn errors seamlessly fall back to [`HttpApiLlmClient`] while `429`/quota
///   exhaustion pauses without touching backup paid keys.
pub fn detect_global_generative_model(env: &ModelDetectEnv) -> DetectedGenerativeModel {
    let file_cfg = env
        .resolved_config_path()
        .and_then(|p| load_secure_kp_config(&p))
        .unwrap_or_default();

    let explicit_policy = env.get_env("KP_MODEL_POLICY").or_else(|| {
        file_cfg
            .model_policy
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    });
    let gm_hint = file_cfg
        .generative_model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let policy_directive = explicit_policy.or(gm_hint).unwrap_or("");

    if matches!(
        policy_directive.to_ascii_lowercase().as_str(),
        "raw_only" | "raw-only" | "none"
    ) {
        return DetectedGenerativeModel::RawOnly;
    }

    // Fail-closed gateway guard: any explicitly configured malformed gateway URL MUST fail closed
    // rather than silently falling through to Host CLI or public cloud API keys.
    if let Some((bad_provider, source_desc, detail)) =
        find_invalid_configured_gateway(env, &file_cfg)
    {
        return DetectedGenerativeModel::invalid_gateway(
            bad_provider,
            format!("invalid gateway URL in {source_desc}: {detail}"),
        );
    }

    // Explicit `NamedEndpoint` or `<provider>:<model>` directive:
    let has_strict_endpoint_directive = match explicit_policy {
        Some(pol) => !matches!(
            pol.to_ascii_lowercase().as_str(),
            "auto" | "inherit" | "host" | "host_cli"
        ),
        None => gm_hint.is_some_and(|gm| {
            file_cfg.named_endpoints.contains_key(gm)
                || gm.contains(':')
                || GenerativeApiProvider::from_id(gm).is_some()
        }),
    };
    if has_strict_endpoint_directive {
        return match resolve_named_endpoint_config_with_file(policy_directive, env, &file_cfg) {
            Ok(mut http_cfg) => {
                // When `generative_model` (`<provider>:<model>`) selects the global model without
                // an explicit `KP_MODEL_POLICY` / `model_policy` or `named_endpoints` isolation
                // boundary, keep `route_policy` on `HostCli` (the global default tier).
                if explicit_policy.is_none()
                    && gm_hint.is_some_and(|gm| !file_cfg.named_endpoints.contains_key(gm))
                {
                    http_cfg.route_policy = ModelRoutePolicy::HostCli;
                }
                DetectedGenerativeModel::HttpApi(http_cfg)
            }
            Err(KpError::InvalidInput { field, reason }) => {
                DetectedGenerativeModel::invalid_policy(field, reason, format!("{field}: {reason}"))
            }
            Err(other) => DetectedGenerativeModel::invalid_policy(
                "model_route_policy",
                "unconfigured or invalid named endpoint",
                other.to_string(),
            ),
        };
    }

    let forced_host_cli_only = matches!(
        policy_directive.to_ascii_lowercase().as_str(),
        "host" | "host_cli"
    );
    let forced_host_cli = forced_host_cli_only || env.get_env("KP_HOST_CLI_PROGRAM").is_some();

    // Enterprise compliance gateway override: when `OPENAI_BASE_URL` + `OPENAI_API_KEY` or
    // `ANTHROPIC_BASE_URL` + `ANTHROPIC_API_KEY` (or `provider_base_urls` in `0600` config) are
    // both configured and `host_cli` was not forced, route directly through `HttpApiLlmClient`.
    if !forced_host_cli {
        for gw_provider in [
            GenerativeApiProvider::OpenAi,
            GenerativeApiProvider::Anthropic,
        ] {
            if resolve_custom_base_url(env, &file_cfg, gw_provider)
                .ok()
                .flatten()
                .is_some()
            {
                if let Some(http_cfg) =
                    build_http_config_if_keyed(env, &file_cfg, gw_provider, None)
                {
                    if http_cfg.is_custom_gateway {
                        return DetectedGenerativeModel::HttpApi(http_cfg);
                    }
                }
            }
        }
    }

    // Tier ①: Logged-in local Host CLI (`codex` -> `claude`), chained with backup HTTP API key when present.
    if let Some(mut cli) = detect_logged_in_host_cli(env, &file_cfg) {
        if !forced_host_cli_only {
            cli.fallback_http = detect_first_keyed_http_config(env, &file_cfg);
        }
        return DetectedGenerativeModel::HostCli(cli);
    }

    // Tier ②: Environment variables or `0600` `~/.kp/config.json` API keys.
    if !forced_host_cli_only {
        if let Some(http_cfg) = detect_first_keyed_http_config(env, &file_cfg) {
            return DetectedGenerativeModel::HttpApi(http_cfg);
        }
    }

    // Tier ③: Automatic non-blocking fallback to `RawOnly` (L0 raw + local FTS5/vector).
    DetectedGenerativeModel::RawOnly
}

fn detect_first_keyed_http_config(
    env: &ModelDetectEnv,
    file_cfg: &KpGlobalModelConfig,
) -> Option<HttpApiConfig> {
    for provider in [
        GenerativeApiProvider::OpenAi,
        GenerativeApiProvider::Anthropic,
        GenerativeApiProvider::DashScope,
        GenerativeApiProvider::Volcengine,
        GenerativeApiProvider::Zhipu,
        GenerativeApiProvider::OpenRouter,
    ] {
        if let Some(http_cfg) = build_http_config_if_keyed(env, file_cfg, provider, None) {
            return Some(http_cfg);
        }
    }
    None
}

fn find_invalid_configured_gateway(
    env: &ModelDetectEnv,
    file_cfg: &KpGlobalModelConfig,
) -> Option<(GenerativeApiProvider, String, String)> {
    for provider in [
        GenerativeApiProvider::OpenAi,
        GenerativeApiProvider::Anthropic,
        GenerativeApiProvider::DashScope,
        GenerativeApiProvider::Volcengine,
        GenerativeApiProvider::Zhipu,
        GenerativeApiProvider::OpenRouter,
    ] {
        let env_key = provider.base_url_env();
        if let Some(raw) = env.get_env(env_key) {
            if let Err(err) = validate_gateway_base_url(raw) {
                return Some((provider, env_key.to_owned(), err.to_string()));
            }
        }
    }
    for (key, raw) in &file_cfg.provider_base_urls {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Err(err) = validate_gateway_base_url(trimmed) {
            let provider =
                GenerativeApiProvider::from_id(key).unwrap_or(GenerativeApiProvider::OpenAi);
            return Some((
                provider,
                format!("provider_base_urls.{key}"),
                err.to_string(),
            ));
        }
    }
    None
}

pub(super) fn is_loopback_gateway_url(raw: &str) -> bool {
    let Ok(parsed) = validate_gateway_base_url(raw) else {
        return false;
    };
    let host = parsed.host_str().unwrap_or("");
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

pub(super) fn lookup_api_key(
    env: &ModelDetectEnv,
    file_cfg: &KpGlobalModelConfig,
    provider: GenerativeApiProvider,
) -> Option<String> {
    if let Some(v) = env.get_env(provider.auth_env()) {
        return Some(v.to_owned());
    }
    for key in [provider.auth_env(), provider.id()] {
        if let Some(v) = file_cfg
            .api_keys
            .get(key)
            .map(String::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(v.to_owned());
        }
    }
    if provider == GenerativeApiProvider::Volcengine {
        if let Some(v) = file_cfg
            .api_keys
            .get("ark")
            .map(String::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(v.to_owned());
        }
    }
    None
}

pub(super) fn resolve_custom_base_url(
    env: &ModelDetectEnv,
    file_cfg: &KpGlobalModelConfig,
    provider: GenerativeApiProvider,
) -> Result<Option<String>> {
    if let Some(v) = env.get_env(provider.base_url_env()) {
        validate_gateway_base_url(v)?;
        return Ok(Some(v.trim_end_matches('/').to_owned()));
    }
    for (k, raw) in &file_cfg.provider_base_urls {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if GenerativeApiProvider::from_id(k) == Some(provider) {
            validate_gateway_base_url(trimmed)?;
            return Ok(Some(trimmed.trim_end_matches('/').to_owned()));
        }
    }
    if provider != GenerativeApiProvider::Anthropic && provider != GenerativeApiProvider::OpenAi {
        if let Some(v) = env.get_env("OPENAI_BASE_URL") {
            validate_gateway_base_url(v)?;
            return Ok(Some(v.trim_end_matches('/').to_owned()));
        }
        if let Some(v) = file_cfg
            .provider_base_urls
            .get("openai")
            .map(String::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            validate_gateway_base_url(v)?;
            return Ok(Some(v.trim_end_matches('/').to_owned()));
        }
    }
    Ok(None)
}

fn lookup_model_override(
    env: &ModelDetectEnv,
    file_cfg: &KpGlobalModelConfig,
    provider: GenerativeApiProvider,
    explicit: Option<&str>,
) -> Option<String> {
    if let Some(m) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(m.to_owned());
    }
    if let Some(m) = env.get_env("KP_GENERATIVE_MODEL") {
        return Some(
            m.split_once(':')
                .map_or(m, |(_, rhs)| rhs)
                .trim()
                .to_owned(),
        );
    }
    let provider_model_env = match provider {
        GenerativeApiProvider::OpenAi => "OPENAI_MODEL",
        GenerativeApiProvider::Anthropic => "ANTHROPIC_MODEL",
        GenerativeApiProvider::DashScope => "DASHSCOPE_MODEL",
        GenerativeApiProvider::Volcengine => "ARK_MODEL",
        GenerativeApiProvider::Zhipu => "ZHIPUAI_MODEL",
        GenerativeApiProvider::OpenRouter => "OPENROUTER_MODEL",
    };
    if let Some(m) = env.get_env(provider_model_env) {
        return Some(m.to_owned());
    }
    if let Some(gm) = file_cfg
        .generative_model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if let Some((lhs, rhs)) = gm.split_once(':') {
            if GenerativeApiProvider::from_id(lhs) == Some(provider) && !rhs.trim().is_empty() {
                return Some(rhs.trim().to_owned());
            }
        } else if !file_cfg.named_endpoints.contains_key(gm)
            && GenerativeApiProvider::from_id(gm).is_none()
        {
            return Some(gm.to_owned());
        }
    }
    None
}

fn build_http_config_if_keyed(
    env: &ModelDetectEnv,
    file_cfg: &KpGlobalModelConfig,
    provider: GenerativeApiProvider,
    explicit_model: Option<&str>,
) -> Option<HttpApiConfig> {
    let custom_url = resolve_custom_base_url(env, file_cfg, provider).ok()?;
    let api_key = lookup_api_key(env, file_cfg, provider)?;
    let mut cfg = HttpApiConfig::for_provider(provider, api_key);
    if let Some(url) = custom_url {
        cfg = cfg.with_base_url(url);
    }
    if let Some(model) = lookup_model_override(env, file_cfg, provider, explicit_model) {
        cfg = cfg.with_model(model);
    }
    cfg.resolved_endpoint_url().ok()?;
    Some(cfg)
}
