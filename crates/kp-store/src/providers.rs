//! GBrain-aligned multi-provider embedding recipe registry, `0600` global config loader,
//! `kp init` auto-detection resolver, and OpenAI-compatible HTTP [`ProviderEmbedder`].
//!
//! Architecture ported from Garry Tan's `GBrain` (`src/core/ai/{types.ts, recipes/*.ts, dims.ts}`, MIT).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use kp_model::{KpError, SecretRedactor};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::embedding::{
    sanitize_and_validate_embedding, DisabledModelEmbedder, Embedder, FtsOnlyEmbedder, HashEmbedder,
};
use crate::{Result, StoreError};

/// Default local ONNX model identifier selected by M2.0 Chinese synonym recall benchmark (`512d`).
pub const DEFAULT_LOCAL_EMBEDDING_MODEL: &str = "local:bge-small-zh-v1.5";
pub const DEFAULT_LOCAL_EMBEDDING_DIMS: usize = 512;

/// Pure-data embedding capability metadata on an [`EmbeddingRecipe`] (`touchpoints.embedding`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct EmbeddingTouchpoint {
    pub models: &'static [&'static str],
    pub default_model: &'static str,
    pub default_dims: usize,
    pub model_dims: &'static [(&'static str, usize)],
    pub dims_options: &'static [usize],
    pub max_input_tokens: Option<usize>,
    pub max_batch_tokens: Option<usize>,
    pub max_batch_items: Option<usize>,
    pub chars_per_token: usize,
}

impl EmbeddingTouchpoint {
    /// Looks up the default dimensionality for `model`, falling back to `default_dims` when `model`
    /// is listed in `models` or equals `default_model`.
    pub fn lookup_model_dims(&self, model: &str) -> Option<usize> {
        for &(known_model, dims) in self.model_dims {
            if known_model == model {
                return Some(dims);
            }
        }
        if !self.default_model.is_empty()
            && (model == self.default_model || self.models.contains(&model))
            && self.default_dims > 0
        {
            return Some(self.default_dims);
        }
        None
    }
}

/// Pure-data provider recipe (`GBrain` `ProviderRecipe` / `recipes/*.ts`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct EmbeddingRecipe {
    pub id: &'static str,
    pub name: &'static str,
    pub implementation: &'static str,
    pub base_url_default: Option<&'static str>,
    pub auth_env: &'static [&'static str],
    /// `false` for explicit-only local servers (`ollama`, `lmstudio`, `llama-server`) and `local`
    /// so environment variable scanning never silently connects to an unstarted local daemon.
    pub auto_detect_env: bool,
    /// `true` for remote cloud services (triggers transparency notice on `kp init`).
    pub is_cloud: bool,
    /// Whether `dimensions` may be sent in the OpenAI-compatible `/embeddings` request body when
    /// supported by the selected model (`false` by default for Volcengine Ark / Doubao).
    pub send_dimensions_param: bool,
    /// Whether arbitrary user-supplied model / endpoint IDs (e.g. Volcengine `ep-*`, Ollama tags,
    /// LM Studio / `llama-server` models) are accepted outside `touchpoint.models`.
    pub user_provided_models: bool,
    pub touchpoint: EmbeddingTouchpoint,
}

/// Complete registry of built-in local, domestic China, international cloud, and explicit local
/// OpenAI-compatible embedding recipes.
pub static EMBEDDING_RECIPES: &[EmbeddingRecipe] = &[
    // 1. Default in-process ONNX Runtime (`~/.kp/models/`)
    EmbeddingRecipe {
        id: "local",
        name: "Local ONNX Runtime",
        implementation: "local-onnx",
        base_url_default: None,
        auth_env: &[],
        auto_detect_env: false,
        is_cloud: false,
        send_dimensions_param: false,
        user_provided_models: false,
        touchpoint: EmbeddingTouchpoint {
            models: &["bge-small-zh-v1.5", "multilingual-e5-small"],
            default_model: "bge-small-zh-v1.5",
            default_dims: 512,
            model_dims: &[("bge-small-zh-v1.5", 512), ("multilingual-e5-small", 384)],
            dims_options: &[384, 512],
            max_input_tokens: Some(512),
            max_batch_tokens: None,
            max_batch_items: Some(64),
            chars_per_token: 2,
        },
    },
    // 2. Alibaba Cloud Model Studio / 阿里云灵积（国内直连）
    EmbeddingRecipe {
        id: "dashscope",
        name: "Alibaba Cloud DashScope (Qwen Embedding)",
        implementation: "openai-compatible",
        base_url_default: Some("https://dashscope.aliyuncs.com/compatible-mode/v1"),
        auth_env: &["DASHSCOPE_API_KEY"],
        auto_detect_env: true,
        is_cloud: true,
        send_dimensions_param: true,
        user_provided_models: false,
        touchpoint: EmbeddingTouchpoint {
            models: &[
                "text-embedding-v3",
                "text-embedding-v4",
                "text-embedding-v2",
            ],
            default_model: "text-embedding-v3",
            default_dims: 1024,
            model_dims: &[
                ("text-embedding-v3", 1024),
                ("text-embedding-v4", 1024),
                ("text-embedding-v2", 1536),
            ],
            dims_options: &[64, 128, 256, 512, 768, 1024, 1536, 2048],
            max_input_tokens: Some(8192),
            max_batch_tokens: Some(8192),
            max_batch_items: Some(10),
            chars_per_token: 2,
        },
    },
    // 3. Volcengine Ark / 火山方舟（豆包 Embedding）
    // Official Volcengine Ark OpenAI-compatible Embeddings API documentation:
    // https://www.volcengine.com/docs/82379/1521766
    // Note: Doubao text embedding models (`240715` 2560d, `240515` 2048d, `large-240915` 4096d) and
    // user inference endpoints (`ep-*`) do not uniformly support Matryoshka `dimensions` clipping,
    // so `send_dimensions_param` is `false` by default to prevent HTTP 400 errors.
    EmbeddingRecipe {
        id: "volcengine",
        name: "Volcengine Ark (Doubao Embedding)",
        implementation: "openai-compatible",
        base_url_default: Some("https://ark.cn-beijing.volces.com/api/v3"),
        auth_env: &["ARK_API_KEY"],
        auto_detect_env: true,
        is_cloud: true,
        send_dimensions_param: false,
        user_provided_models: true,
        touchpoint: EmbeddingTouchpoint {
            models: &[
                "doubao-embedding-text-240715",
                "doubao-embedding-text-240515",
                "doubao-embedding-large-text-240915",
            ],
            default_model: "doubao-embedding-text-240715",
            default_dims: 2560,
            model_dims: &[
                ("doubao-embedding-text-240715", 2560),
                ("doubao-embedding-text-240515", 2048),
                ("doubao-embedding-large-text-240915", 4096),
            ],
            dims_options: &[2048, 2560, 4096],
            max_input_tokens: Some(4096),
            max_batch_tokens: Some(16_384),
            max_batch_items: Some(32),
            chars_per_token: 2,
        },
    },
    // 4. Zhipu AI / 智谱 BigModel
    EmbeddingRecipe {
        id: "zhipu",
        name: "Zhipu AI (GLM Embedding)",
        implementation: "openai-compatible",
        base_url_default: Some("https://open.bigmodel.cn/api/paas/v4"),
        auth_env: &["ZHIPUAI_API_KEY"],
        auto_detect_env: true,
        is_cloud: true,
        send_dimensions_param: true,
        user_provided_models: false,
        touchpoint: EmbeddingTouchpoint {
            models: &["embedding-3", "embedding-2"],
            default_model: "embedding-3",
            default_dims: 1024,
            model_dims: &[("embedding-3", 1024), ("embedding-2", 1024)],
            dims_options: &[256, 512, 1024, 2048],
            max_input_tokens: Some(8192),
            max_batch_tokens: Some(32_768),
            max_batch_items: Some(64),
            chars_per_token: 2,
        },
    },
    // 5. OpenAI (supports `OPENAI_BASE_URL` enterprise gateway override)
    EmbeddingRecipe {
        id: "openai",
        name: "OpenAI",
        implementation: "openai-compatible",
        base_url_default: Some("https://api.openai.com/v1"),
        auth_env: &["OPENAI_API_KEY"],
        auto_detect_env: true,
        is_cloud: true,
        send_dimensions_param: true,
        user_provided_models: true,
        touchpoint: EmbeddingTouchpoint {
            models: &[
                "text-embedding-3-small",
                "text-embedding-3-large",
                "text-embedding-ada-002",
            ],
            default_model: "text-embedding-3-small",
            default_dims: 1536,
            model_dims: &[
                ("text-embedding-3-small", 1536),
                ("text-embedding-3-large", 3072),
                ("text-embedding-ada-002", 1536),
            ],
            dims_options: &[256, 512, 1024, 1536, 3072],
            max_input_tokens: Some(8191),
            max_batch_tokens: Some(100_000),
            max_batch_items: Some(128),
            chars_per_token: 3,
        },
    },
    // 6. Voyage AI
    EmbeddingRecipe {
        id: "voyage",
        name: "Voyage AI",
        implementation: "openai-compatible",
        base_url_default: Some("https://api.voyageai.com/v1"),
        auth_env: &["VOYAGE_API_KEY"],
        auto_detect_env: true,
        is_cloud: true,
        send_dimensions_param: true,
        user_provided_models: false,
        touchpoint: EmbeddingTouchpoint {
            models: &[
                "voyage-4",
                "voyage-3-large",
                "voyage-3",
                "voyage-3-lite",
                "voyage-code-3",
            ],
            default_model: "voyage-4",
            default_dims: 1024,
            model_dims: &[
                ("voyage-4", 1024),
                ("voyage-3-large", 1024),
                ("voyage-3", 1024),
                ("voyage-3-lite", 512),
                ("voyage-code-3", 1024),
            ],
            dims_options: &[256, 512, 1024, 2048],
            max_input_tokens: Some(32_000),
            max_batch_tokens: Some(120_000),
            max_batch_items: Some(128),
            chars_per_token: 3,
        },
    },
    // 7. Google Gemini (OpenAI-compatible endpoint)
    EmbeddingRecipe {
        id: "google",
        name: "Google Gemini",
        implementation: "openai-compatible",
        base_url_default: Some("https://generativelanguage.googleapis.com/v1beta/openai"),
        auth_env: &["GOOGLE_GENERATIVE_AI_API_KEY", "GEMINI_API_KEY"],
        auto_detect_env: true,
        is_cloud: true,
        send_dimensions_param: true,
        user_provided_models: false,
        touchpoint: EmbeddingTouchpoint {
            models: &["gemini-embedding-001", "gemini-embedding-2"],
            default_model: "gemini-embedding-001",
            default_dims: 768,
            model_dims: &[("gemini-embedding-001", 768), ("gemini-embedding-2", 768)],
            dims_options: &[256, 512, 768, 1536, 3072],
            max_input_tokens: Some(8192),
            max_batch_tokens: Some(32_768),
            max_batch_items: Some(100),
            chars_per_token: 3,
        },
    },
    // 8. OpenRouter
    EmbeddingRecipe {
        id: "openrouter",
        name: "OpenRouter",
        implementation: "openai-compatible",
        base_url_default: Some("https://openrouter.ai/api/v1"),
        auth_env: &["OPENROUTER_API_KEY"],
        auto_detect_env: true,
        is_cloud: true,
        send_dimensions_param: false,
        user_provided_models: true,
        touchpoint: EmbeddingTouchpoint {
            models: &[
                "openai/text-embedding-3-small",
                "openai/text-embedding-3-large",
                "qwen/qwen3-embedding-8b",
                "baai/bge-m3",
            ],
            default_model: "openai/text-embedding-3-small",
            default_dims: 1536,
            model_dims: &[
                ("openai/text-embedding-3-small", 1536),
                ("openai/text-embedding-3-large", 3072),
                ("qwen/qwen3-embedding-8b", 4096),
                ("baai/bge-m3", 1024),
            ],
            dims_options: &[256, 512, 1024, 1536, 3072, 4096],
            max_input_tokens: Some(8191),
            max_batch_tokens: Some(64_000),
            max_batch_items: Some(64),
            chars_per_token: 3,
        },
    },
    // 9. Ollama (explicit local service — never auto-detected from environment)
    EmbeddingRecipe {
        id: "ollama",
        name: "Ollama (Local)",
        implementation: "openai-compatible",
        base_url_default: Some("http://127.0.0.1:11434/v1"),
        auth_env: &["OLLAMA_API_KEY"],
        auto_detect_env: false,
        is_cloud: false,
        send_dimensions_param: false,
        user_provided_models: true,
        touchpoint: EmbeddingTouchpoint {
            models: &[
                "nomic-embed-text",
                "bge-m3",
                "mxbai-embed-large",
                "all-minilm",
                "snowflake-arctic-embed",
            ],
            default_model: "nomic-embed-text",
            default_dims: 768,
            model_dims: &[
                ("nomic-embed-text", 768),
                ("bge-m3", 1024),
                ("mxbai-embed-large", 1024),
                ("all-minilm", 384),
                ("snowflake-arctic-embed", 1024),
            ],
            dims_options: &[384, 768, 1024],
            max_input_tokens: Some(8192),
            max_batch_tokens: Some(32_768),
            max_batch_items: Some(64),
            chars_per_token: 3,
        },
    },
    // 10. LM Studio (explicit local service — user-provided models, requires explicit dimensions)
    EmbeddingRecipe {
        id: "lmstudio",
        name: "LM Studio (Local)",
        implementation: "openai-compatible",
        base_url_default: Some("http://127.0.0.1:1234/v1"),
        auth_env: &["LMSTUDIO_API_KEY"],
        auto_detect_env: false,
        is_cloud: false,
        send_dimensions_param: false,
        user_provided_models: true,
        touchpoint: EmbeddingTouchpoint {
            models: &[],
            default_model: "",
            default_dims: 0,
            model_dims: &[],
            dims_options: &[],
            max_input_tokens: Some(8192),
            max_batch_tokens: Some(32_768),
            max_batch_items: Some(32),
            chars_per_token: 3,
        },
    },
    // 11. llama.cpp `llama-server` (explicit local service — user-provided models, requires explicit dimensions)
    EmbeddingRecipe {
        id: "llama-server",
        name: "llama.cpp Server (Local)",
        implementation: "openai-compatible",
        base_url_default: Some("http://127.0.0.1:8080/v1"),
        auth_env: &["LLAMA_API_KEY"],
        auto_detect_env: false,
        is_cloud: false,
        send_dimensions_param: false,
        user_provided_models: true,
        touchpoint: EmbeddingTouchpoint {
            models: &[],
            default_model: "",
            default_dims: 0,
            model_dims: &[],
            dims_options: &[],
            max_input_tokens: Some(8192),
            max_batch_tokens: Some(32_768),
            max_batch_items: Some(32),
            chars_per_token: 3,
        },
    },
];

/// Returns all registered [`EmbeddingRecipe`] definitions.
pub fn embedding_recipes() -> &'static [EmbeddingRecipe] {
    EMBEDDING_RECIPES
}

/// Looks up a registered [`EmbeddingRecipe`] by provider `id` (case-insensitive).
pub fn find_embedding_recipe(provider_id: &str) -> Option<&'static EmbeddingRecipe> {
    let target = provider_id.trim();
    EMBEDDING_RECIPES
        .iter()
        .find(|r| r.id.eq_ignore_ascii_case(target))
}

/// User-level `0600` global configuration (`$KP_CONFIG_PATH` or `~/.kp/config.json`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KpGlobalConfig {
    #[serde(default)]
    pub api_keys: BTreeMap<String, String>,
    #[serde(default)]
    pub provider_base_urls: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_dimensions: Option<usize>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl KpGlobalConfig {
    /// Looks up a configured API key by `auth_env` variable name or provider `id`.
    pub fn lookup_api_key(&self, recipe: &EmbeddingRecipe) -> Option<String> {
        for &env_key in recipe.auth_env {
            if let Some(v) = self.api_keys.get(env_key).map(|s| s.trim()) {
                if !v.is_empty() {
                    return Some(v.to_owned());
                }
            }
        }
        self.api_keys
            .get(recipe.id)
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    }

    /// Looks up a configured `provider_base_urls.<id>` override.
    pub fn lookup_base_url(&self, provider_id: &str) -> Option<String> {
        self.provider_base_urls
            .get(provider_id)
            .map(|s| s.trim().trim_end_matches('/').to_owned())
            .filter(|s| !s.is_empty())
    }
}

/// Resolves the active config file path from `KP_CONFIG_PATH` or `$HOME/.kp/config.json`.
pub fn default_global_config_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("KP_CONFIG_PATH") {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    home_dir_from_env().map(|home| home.join(".kp").join("config.json"))
}

pub(crate) fn home_dir_from_env() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()))
        .map(PathBuf::from)
}

/// Loads `KpGlobalConfig` from `path` (returning `Ok(Default::default())` if the file does not exist).
/// On Unix, rejects world- or group-accessible (`mode & 0o077 != 0`) config files.
pub fn load_global_config_from_path(path: &Path) -> Result<KpGlobalConfig> {
    if !path.exists() {
        return Ok(KpGlobalConfig::default());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)?.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(StoreError::Embedding(format!(
                "global config `{}` has insecure mode {:04o}; expected 0600 (`chmod 600`)",
                path.display(),
                mode
            )));
        }
    }
    let raw = fs::read_to_string(path)?;
    if raw.trim().is_empty() {
        return Ok(KpGlobalConfig::default());
    }
    let cfg: KpGlobalConfig = serde_json::from_str(&raw)?;
    Ok(cfg)
}

/// Loads `KpGlobalConfig` from `$KP_CONFIG_PATH` or `~/.kp/config.json` when present.
pub fn load_global_config() -> Result<KpGlobalConfig> {
    match default_global_config_path() {
        Some(path) => load_global_config_from_path(&path),
        None => Ok(KpGlobalConfig::default()),
    }
}

/// Writes `KpGlobalConfig` atomically with `0600` file permissions on Unix.
pub fn save_global_config_0600(path: &Path, config: &KpGlobalConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let serialized = serde_json::to_vec_pretty(config)?;
    let tmp_path = path.with_extension("json.tmp");
    {
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&tmp_path)?;
            file.write_all(&serialized)?;
            file.sync_all()?;
        }
        #[cfg(not(unix))]
        {
            fs::write(&tmp_path, &serialized)?;
        }
    }
    fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Normalized `<provider>:<model>` specification and resolved vector dimensions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ResolvedEmbeddingSpec {
    /// Canonical model ID persisted in `library_meta.embedding_model` (e.g. `"local:bge-small-zh-v1.5"`).
    pub model_id: String,
    pub provider_id: String,
    pub model_name: String,
    /// Locked dimension (`0` only for `"none"` or dynamic inference endpoints like `volcengine:ep-*`
    /// that auto-lock on the first returned vector).
    pub dims: usize,
    pub is_cloud: bool,
    pub explicit_dims: bool,
}

/// Parses and validates an embedding model specification (`"none"`, `"hash:v1"`, `"local"`,
/// `"<provider>"`, or `"<provider>:<model>"`) along with optional `--embedding-dimensions`.
pub fn parse_embedding_model_spec(
    spec: &str,
    explicit_dims: Option<usize>,
) -> Result<ResolvedEmbeddingSpec> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err(KpError::invalid("embedding_model", "embedding model must be nonempty").into());
    }
    if let Some(0) = explicit_dims {
        return Err(KpError::invalid(
            "embedding_dimensions",
            "explicit embedding dimensions must be > 0",
        )
        .into());
    }

    if trimmed.eq_ignore_ascii_case("none") {
        if explicit_dims.is_some() {
            return Err(KpError::invalid(
                "embedding_dimensions",
                "cannot specify embedding dimensions when model is `none`",
            )
            .into());
        }
        return Ok(ResolvedEmbeddingSpec {
            model_id: "none".to_owned(),
            provider_id: "none".to_owned(),
            model_name: String::new(),
            dims: 0,
            is_cloud: false,
            explicit_dims: false,
        });
    }

    if trimmed.eq_ignore_ascii_case("hash:v1") || trimmed.eq_ignore_ascii_case("hash") {
        let dims = explicit_dims.unwrap_or(256);
        if dims != 256 {
            return Err(KpError::invalid(
                "embedding_dimensions",
                "hash:v1 only supports 256 dimensions",
            )
            .into());
        }
        return Ok(ResolvedEmbeddingSpec {
            model_id: "hash:v1".to_owned(),
            provider_id: "hash".to_owned(),
            model_name: "v1".to_owned(),
            dims: 256,
            is_cloud: false,
            explicit_dims: explicit_dims.is_some(),
        });
    }

    let (raw_provider, raw_model_opt) = match trimmed.split_once(':') {
        Some((p, m)) => (p.trim(), Some(m.trim())),
        None => (trimmed, None),
    };

    let Some(recipe) = find_embedding_recipe(raw_provider) else {
        return Err(StoreError::Embedding(format!(
            "unknown embedding provider `{raw_provider}`"
        )));
    };

    let model_name = match raw_model_opt {
        Some(m) if !m.is_empty() => m.to_owned(),
        Some(_) => {
            return Err(StoreError::Embedding(format!(
                "missing model name after `{}:`",
                recipe.id
            )))
        }
        None => {
            if recipe.touchpoint.default_model.is_empty() {
                return Err(StoreError::Embedding(format!(
                    "provider `{}` requires an explicit model (`{}:<model>`)",
                    recipe.id, recipe.id
                )));
            }
            recipe.touchpoint.default_model.to_owned()
        }
    };

    let is_known_model = recipe.touchpoint.models.contains(&model_name.as_str())
        || recipe
            .touchpoint
            .model_dims
            .iter()
            .any(|(m, _)| *m == model_name);
    if !is_known_model && !recipe.user_provided_models {
        return Err(StoreError::Embedding(format!(
            "unsupported model `{model_name}` for provider `{}` (supported: {})",
            recipe.id,
            recipe.touchpoint.models.join(", ")
        )));
    }

    let known_dims = recipe.touchpoint.lookup_model_dims(&model_name);
    let dims = match (explicit_dims, known_dims) {
        (Some(d), Some(kd)) if recipe.id == "local" && d != kd => {
            return Err(StoreError::Embedding(format!(
                "`local:{model_name}` has fixed {kd} dimensions (got {d})"
            )));
        }
        (Some(d), _) => d,
        (None, Some(kd)) => kd,
        (None, None) => {
            // `lmstudio` and `llama-server` carry `default_dims == 0` and require `--embedding-dimensions <N>`
            // upfront so the library metadata locks a concrete vector width; Volcengine `ep-*` user
            // endpoints allow `0` to auto-lock on the first real vector returned by Ark.
            if recipe.touchpoint.default_dims == 0 {
                return Err(StoreError::Embedding(format!(
                    "`{}:{model_name}` uses a user-provided model; pass `--embedding-dimensions <N>`",
                    recipe.id
                )));
            }
            0
        }
    };

    Ok(ResolvedEmbeddingSpec {
        model_id: format!("{}:{model_name}", recipe.id),
        provider_id: recipe.id.to_owned(),
        model_name,
        dims,
        is_cloud: recipe.is_cloud,
        explicit_dims: explicit_dims.is_some(),
    })
}

/// One cloud provider detected via environment variables or `0600` `~/.kp/config.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DetectedEmbeddingProvider {
    pub provider_id: &'static str,
    pub provider_name: &'static str,
    pub default_model_id: String,
    pub default_dims: usize,
    pub key_source: String,
}

/// Result of [`resolve_init_embedding_choice`] (`docs/ARCHITECTURE.md`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InitEmbeddingChoice {
    pub selected: ResolvedEmbeddingSpec,
    pub detected_cloud_providers: Vec<DetectedEmbeddingProvider>,
    pub cloud_privacy_notice: Option<String>,
}

/// Resolves the embedding model and dimensions for `kp init`:
/// 1. Explicit `--no-embedding` (`none`, `0d`) or `--embedding-model <spec>` always wins.
/// 2. Otherwise scans environment variables and `0600` `config` ONLY for recipes with
///    `auto_detect_env == true` (`ollama`, `lmstudio`, `llama-server` are NEVER auto-detected).
/// 3. Falls back to `local:bge-small-zh-v1.5` (`512d`) when no cloud API key is present.
pub fn resolve_init_embedding_choice<F>(
    no_embedding: bool,
    explicit_model: Option<&str>,
    explicit_dims: Option<usize>,
    env_lookup: F,
    config: Option<&KpGlobalConfig>,
) -> Result<InitEmbeddingChoice>
where
    F: Fn(&str) -> Option<String>,
{
    if no_embedding {
        let selected = parse_embedding_model_spec("none", explicit_dims)?;
        return Ok(InitEmbeddingChoice {
            selected,
            detected_cloud_providers: Vec::new(),
            cloud_privacy_notice: None,
        });
    }

    if let Some(spec) = explicit_model.map(str::trim).filter(|s| !s.is_empty()) {
        let selected = parse_embedding_model_spec(spec, explicit_dims)?;
        let notice = selected.is_cloud.then(|| {
            format!(
                "Note: ingested text chunks and facts will be sent to `{}` (`{}`) for vector embedding.",
                selected.provider_id, selected.model_id
            )
        });
        return Ok(InitEmbeddingChoice {
            selected,
            detected_cloud_providers: Vec::new(),
            cloud_privacy_notice: notice,
        });
    }

    if let Some(cfg_model) = config
        .and_then(|c| c.embedding_model.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let dims = explicit_dims.or_else(|| config.and_then(|c| c.embedding_dimensions));
        let selected = parse_embedding_model_spec(cfg_model, dims)?;
        let notice = selected.is_cloud.then(|| {
            format!(
                "Note: ingested text chunks and facts will be sent to `{}` (`{}`) for vector embedding.",
                selected.provider_id, selected.model_id
            )
        });
        return Ok(InitEmbeddingChoice {
            selected,
            detected_cloud_providers: Vec::new(),
            cloud_privacy_notice: notice,
        });
    }

    let mut detected = Vec::new();
    for recipe in EMBEDDING_RECIPES.iter().filter(|r| r.auto_detect_env) {
        let mut found_source: Option<String> = None;
        for &env_var in recipe.auth_env {
            if env_lookup(env_var)
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
            {
                found_source = Some(format!("env:{env_var}"));
                break;
            }
        }
        if found_source.is_none() {
            if let Some(cfg) = config {
                if cfg.lookup_api_key(recipe).is_some() {
                    found_source = Some("config:api_keys".to_owned());
                }
            }
        }
        if let Some(source) = found_source {
            detected.push(DetectedEmbeddingProvider {
                provider_id: recipe.id,
                provider_name: recipe.name,
                default_model_id: format!("{}:{}", recipe.id, recipe.touchpoint.default_model),
                default_dims: recipe.touchpoint.default_dims,
                key_source: source,
            });
        }
    }

    if let Some(first) = detected.first() {
        let selected = parse_embedding_model_spec(&first.default_model_id, explicit_dims)?;
        let notice = Some(format!(
            "Note: ingested text chunks and facts will be sent to {} (`{}`) for vector embedding.",
            first.provider_name, selected.model_id
        ));
        return Ok(InitEmbeddingChoice {
            selected,
            detected_cloud_providers: detected,
            cloud_privacy_notice: notice,
        });
    }

    let selected = parse_embedding_model_spec(DEFAULT_LOCAL_EMBEDDING_MODEL, explicit_dims)?;
    Ok(InitEmbeddingChoice {
        selected,
        detected_cloud_providers: Vec::new(),
        cloud_privacy_notice: None,
    })
}

/// Configuration for constructing an OpenAI-compatible HTTP [`ProviderEmbedder`].
#[derive(Clone, Debug)]
pub struct ProviderEmbedderConfig {
    pub spec: ResolvedEmbeddingSpec,
    pub base_url_override: Option<String>,
    pub api_key_override: Option<String>,
    pub timeout: Duration,
    /// Optional override to force sending `"dimensions": N` even on providers that omit it by default.
    pub force_send_dimensions: Option<bool>,
}

impl ProviderEmbedderConfig {
    pub fn new(spec: ResolvedEmbeddingSpec) -> Self {
        Self {
            spec,
            base_url_override: None,
            api_key_override: None,
            timeout: Duration::from_secs(30),
            force_send_dimensions: None,
        }
    }
}

/// Connectivity and live dimension probe result for `kp providers test`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProviderTestReport {
    pub model_id: String,
    pub provider_id: String,
    pub endpoint_url: String,
    pub returned_dims: usize,
    pub latency_ms: u64,
}

#[cfg(test)]
type MockEmbedTransport =
    Arc<dyn Fn(&str, Option<&str>, &Value) -> Result<(u16, Vec<u8>)> + Send + Sync>;

/// Validates an explicit provider or enterprise gateway base URL (`http://` or `https://` with a
/// non-empty host and no embedded credentials, query string, or fragment).
///
/// Invalid explicit gateway URLs fail closed immediately and never fall back to public endpoints.
pub fn validate_provider_base_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(StoreError::Embedding(
            "gateway base URL must be nonempty".to_owned(),
        ));
    }
    let parsed = reqwest::Url::parse(trimmed).map_err(|e| {
        StoreError::Embedding(format!(
            "invalid gateway base URL `{trimmed}`: {e} (expected http:// or https:// URL)"
        ))
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(StoreError::Embedding(format!(
            "invalid gateway base URL `{trimmed}`: unsupported scheme `{}` (expected http or https)",
            parsed.scheme()
        )));
    }
    if !parsed.host_str().is_some_and(|h| !h.trim().is_empty()) {
        return Err(StoreError::Embedding(format!(
            "invalid gateway base URL `{trimmed}`: missing host"
        )));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(StoreError::Embedding(format!(
            "invalid gateway base URL `{trimmed}`: embedded URL credentials are not allowed"
        )));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(StoreError::Embedding(format!(
            "invalid gateway base URL `{trimmed}`: query parameters and fragments are not allowed"
        )));
    }
    Ok(trimmed.trim_end_matches('/').to_owned())
}

pub(crate) fn is_loopback_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    match parsed.host_str() {
        Some(h) if h.eq_ignore_ascii_case("localhost") => true,
        Some(h) => h
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback()),
        None => false,
    }
}

fn shared_sync_http_runtime() -> Result<&'static tokio::runtime::Runtime> {
    static SYNC_HTTP_RUNTIME: OnceLock<std::result::Result<tokio::runtime::Runtime, String>> =
        OnceLock::new();
    let res = SYNC_HTTP_RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|e| format!("tokio runtime build failed: {e}"))
    });
    match res {
        Ok(rt) => Ok(rt),
        Err(msg) => Err(StoreError::Embedding(msg.clone())),
    }
}

/// OpenAI-compatible HTTP `/embeddings` client implementing [`Embedder`] (`allows_raw_only() == false`).
pub struct ProviderEmbedder {
    recipe: &'static EmbeddingRecipe,
    spec: ResolvedEmbeddingSpec,
    endpoint_url: String,
    api_key: Option<String>,
    send_dimensions: bool,
    client: reqwest::Client,
    redactor: SecretRedactor,
    #[cfg(test)]
    mock_transport: Option<MockEmbedTransport>,
}

impl std::fmt::Debug for ProviderEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderEmbedder")
            .field("model_id", &self.spec.model_id)
            .field("endpoint_url", &self.endpoint_url)
            .field("has_api_key", &self.api_key.is_some())
            .finish()
    }
}

impl ProviderEmbedder {
    /// Builds a [`ProviderEmbedder`] resolving base URL and API key across explicit overrides,
    /// environment variables (`OPENAI_BASE_URL`, `<AUTH_ENV>`), and `0600` `KpGlobalConfig`.
    pub fn from_env_and_config(
        cfg: ProviderEmbedderConfig,
        global_config: Option<&KpGlobalConfig>,
    ) -> Result<Self> {
        Self::from_lookup(cfg, |k| std::env::var(k).ok(), global_config)
    }

    pub fn from_lookup<F>(
        cfg: ProviderEmbedderConfig,
        env_lookup: F,
        global_config: Option<&KpGlobalConfig>,
    ) -> Result<Self>
    where
        F: Fn(&str) -> Option<String>,
    {
        let Some(recipe) = find_embedding_recipe(&cfg.spec.provider_id) else {
            return Err(StoreError::Embedding(format!(
                "unknown provider `{}`",
                cfg.spec.provider_id
            )));
        };
        if recipe.implementation != "openai-compatible" {
            return Err(StoreError::Embedding(format!(
                "provider `{}` is not an HTTP OpenAI-compatible provider",
                recipe.id
            )));
        }

        let provider_env = format!(
            "{}_BASE_URL",
            recipe.id.to_ascii_uppercase().replace('-', "_")
        );
        let explicit_base_candidate: Option<String> = cfg
            .base_url_override
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                global_config.and_then(|g| {
                    g.provider_base_urls
                        .get(recipe.id)
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty())
                })
            })
            .or_else(|| {
                env_lookup(&provider_env)
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
            })
            .or_else(|| {
                if recipe.is_cloud && recipe.implementation == "openai-compatible" {
                    env_lookup("OPENAI_BASE_URL")
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty())
                } else {
                    None
                }
            });

        let base_url = match explicit_base_candidate {
            Some(explicit_raw) => validate_provider_base_url(&explicit_raw)?,
            None => recipe
                .base_url_default
                .map(|s| s.trim_end_matches('/').to_owned())
                .ok_or_else(|| KpError::invalid("base_url", "missing provider base URL"))?,
        };

        let clean_base = base_url.trim_end_matches('/');
        let endpoint_url = if clean_base.ends_with("/embeddings") {
            clean_base.to_owned()
        } else {
            format!("{clean_base}/embeddings")
        };

        let api_key = cfg
            .api_key_override
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                recipe.auth_env.iter().find_map(|&var| {
                    env_lookup(var)
                        .map(|v| v.trim().to_owned())
                        .filter(|v| !v.is_empty())
                })
            })
            .or_else(|| global_config.and_then(|g| g.lookup_api_key(recipe)));

        if recipe.is_cloud && api_key.is_none() {
            let expected_vars = recipe.auth_env.join(" or ");
            return Err(StoreError::Embedding(format!(
                "missing API key for `{}` (set {expected_vars} or configure `~/.kp/config.json`)",
                recipe.id
            )));
        }

        let model_supports_dims = !matches!(
            cfg.spec.model_name.as_str(),
            "text-embedding-ada-002" | "text-embedding-v2" | "embedding-2"
        );
        let send_dimensions = match cfg.force_send_dimensions {
            Some(explicit) => explicit,
            None => {
                recipe.send_dimensions_param
                    && model_supports_dims
                    && cfg.spec.dims > 0
                    && (cfg.spec.explicit_dims
                        || recipe.touchpoint.dims_options.contains(&cfg.spec.dims))
            }
        };

        let mut builder = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .redirect(reqwest::redirect::Policy::none());
        if is_loopback_url(&endpoint_url) {
            builder = builder.no_proxy();
        }
        let client = builder
            .build()
            .map_err(|e| StoreError::Embedding(format!("HTTP client build failed: {e}")))?;
        let redactor = SecretRedactor::new()
            .map_err(|e| StoreError::Embedding(format!("secret redactor init failed: {e}")))?;

        Ok(Self {
            recipe,
            spec: cfg.spec,
            endpoint_url,
            api_key,
            send_dimensions,
            client,
            redactor,
            #[cfg(test)]
            mock_transport: None,
        })
    }

    #[cfg(test)]
    fn with_mock_transport(mut self, transport: MockEmbedTransport) -> Self {
        self.mock_transport = Some(transport);
        self
    }

    pub fn endpoint_url(&self) -> &str {
        &self.endpoint_url
    }

    pub fn recipe(&self) -> &'static EmbeddingRecipe {
        self.recipe
    }

    fn prepare_input_text(&self, raw: &str) -> String {
        let redacted = self.redactor.redact(raw);
        let trimmed = redacted.trim();
        let text = if trimmed.is_empty() { " " } else { trimmed };
        if let Some(max_tok) = self.recipe.touchpoint.max_input_tokens {
            let max_chars = max_tok.saturating_mul(self.recipe.touchpoint.chars_per_token.max(1));
            if text.chars().count() > max_chars && max_chars > 0 {
                return text.chars().take(max_chars).collect();
            }
        }
        text.to_owned()
    }

    fn split_batches(&self, prepared: &[String]) -> Vec<(usize, usize)> {
        let max_items = self.recipe.touchpoint.max_batch_items.unwrap_or(64).max(1);
        let max_batch_tokens = self.recipe.touchpoint.max_batch_tokens;
        let cpt = self.recipe.touchpoint.chars_per_token.max(1);

        let mut ranges = Vec::new();
        let mut start = 0usize;
        while start < prepared.len() {
            let mut end = start;
            let mut batch_tokens = 0usize;
            while end < prepared.len() && (end - start) < max_items {
                let est_tokens = prepared[end].chars().count().div_ceil(cpt).max(1);
                if let Some(limit_tok) = max_batch_tokens {
                    if end > start && batch_tokens.saturating_add(est_tokens) > limit_tok {
                        break;
                    }
                }
                batch_tokens = batch_tokens.saturating_add(est_tokens);
                end += 1;
            }
            ranges.push((start, end));
            start = end;
        }
        ranges
    }

    /// Sends multi-item batches to the OpenAI-compatible `/embeddings` endpoint asynchronously.
    pub async fn embed_batch_async(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let prepared: Vec<String> = texts.iter().map(|t| self.prepare_input_text(t)).collect();
        let ranges = self.split_batches(&prepared);
        let mut all_vectors = Vec::with_capacity(texts.len());

        for (start, end) in ranges {
            let slice = &prepared[start..end];
            let mut body = json!({
                "model": self.spec.model_name,
                "input": slice,
                "encoding_format": "float",
            });
            if self.send_dimensions && self.spec.dims > 0 {
                if let Some(obj) = body.as_object_mut() {
                    obj.insert("dimensions".to_owned(), json!(self.spec.dims));
                }
            }

            let (status_code, raw_bytes) = {
                #[cfg(test)]
                if let Some(ref transport) = self.mock_transport {
                    transport(&self.endpoint_url, self.api_key.as_deref(), &body)?
                } else {
                    self.send_http_post(&body).await?
                }
                #[cfg(not(test))]
                {
                    self.send_http_post(&body).await?
                }
            };
            if !(200..300).contains(&status_code) {
                let snippet: String = String::from_utf8_lossy(&raw_bytes)
                    .chars()
                    .take(240)
                    .collect();
                return Err(StoreError::Embedding(format!(
                    "`{}` HTTP {} from `{}`: {}",
                    self.spec.model_id,
                    status_code,
                    self.endpoint_url,
                    self.redactor.redact(&snippet)
                )));
            }

            let parsed: Value = serde_json::from_slice(&raw_bytes).map_err(|e| {
                StoreError::Embedding(format!(
                    "`{}` returned invalid JSON: {e}",
                    self.spec.model_id
                ))
            })?;
            let Some(data_arr) = parsed.get("data").and_then(Value::as_array) else {
                return Err(StoreError::Embedding(format!(
                    "`{}` response missing `data` array",
                    self.spec.model_id
                )));
            };
            if data_arr.len() != slice.len() {
                return Err(StoreError::Embedding(format!(
                    "`{}` expected {} embeddings, got {}",
                    self.spec.model_id,
                    slice.len(),
                    data_arr.len()
                )));
            }

            let mut indexed = Vec::with_capacity(data_arr.len());
            for (fallback_idx, item) in data_arr.iter().enumerate() {
                let idx = item
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|v| usize::try_from(v).ok())
                    .unwrap_or(fallback_idx);
                let Some(emb_arr) = item.get("embedding").and_then(Value::as_array) else {
                    return Err(StoreError::Embedding(format!(
                        "`{}` item {idx} missing numeric `embedding` array",
                        self.spec.model_id
                    )));
                };
                let mut raw_vec = Vec::with_capacity(emb_arr.len());
                for val in emb_arr {
                    let Some(num) = val.as_f64() else {
                        return Err(StoreError::Embedding(format!(
                            "`{}` embedding contains non-numeric element",
                            self.spec.model_id
                        )));
                    };
                    raw_vec.push(num as f32);
                }
                let validated = sanitize_and_validate_embedding(&raw_vec)?;
                if self.spec.dims > 0 && validated.len() != self.spec.dims {
                    return Err(StoreError::Embedding(format!(
                        "`{}` expected {}d vector, provider returned {}d; \
                         run `kp embed --stale --to {} --dim {}`",
                        self.spec.model_id,
                        self.spec.dims,
                        validated.len(),
                        self.spec.model_id,
                        validated.len()
                    )));
                }
                indexed.push((idx, validated));
            }
            indexed.sort_by_key(|(idx, _)| *idx);
            all_vectors.extend(indexed.into_iter().map(|(_, v)| v));
        }

        Ok(all_vectors)
    }

    async fn send_http_post(&self, body: &Value) -> Result<(u16, Vec<u8>)> {
        let mut req = self.client.post(&self.endpoint_url).json(body);
        if let Some(ref key) = self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await.map_err(|e| {
            StoreError::Embedding(format!(
                "`{}` request to `{}` failed: {e}",
                self.spec.model_id, self.endpoint_url
            ))
        })?;
        let status_code = resp.status().as_u16();
        let raw_bytes = resp.bytes().await.map_err(|e| {
            StoreError::Embedding(format!(
                "`{}` response read failed: {e}",
                self.spec.model_id
            ))
        })?;
        Ok((status_code, raw_bytes.to_vec()))
    }

    fn run_sync<T, Fut>(&self, make_fut: impl FnOnce() -> Fut + Send) -> Result<T>
    where
        T: Send,
        Fut: std::future::Future<Output = Result<T>> + Send,
    {
        let rt = shared_sync_http_runtime()?;
        if tokio::runtime::Handle::try_current().is_ok() {
            std::thread::scope(|s| {
                s.spawn(|| rt.block_on(make_fut()))
                    .join()
                    .map_err(|_| StoreError::Embedding("embedding worker thread panicked".into()))?
            })
        } else {
            rt.block_on(make_fut())
        }
    }

    /// Live connectivity & dimension self-test for `kp providers test --model <provider>:<model>`.
    pub async fn test_connectivity(&self) -> Result<ProviderTestReport> {
        let started = Instant::now();
        let probe = vec!["Kipo embedding connectivity probe".to_owned()];
        let mut out = self.embed_batch_async(&probe).await?;
        let vec = out
            .pop()
            .ok_or_else(|| StoreError::Embedding("empty probe response".into()))?;
        Ok(ProviderTestReport {
            model_id: self.spec.model_id.clone(),
            provider_id: self.spec.provider_id.clone(),
            endpoint_url: self.endpoint_url.clone(),
            returned_dims: vec.len(),
            latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        })
    }

    /// Synchronous wrapper around [`Self::test_connectivity`].
    pub fn test_connectivity_sync(&self) -> Result<ProviderTestReport> {
        self.run_sync(|| self.test_connectivity())
    }
}

impl Embedder for ProviderEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let single = [text.to_owned()];
        let mut batch = self.embed_batch(&single)?;
        batch
            .pop()
            .ok_or_else(|| StoreError::Embedding("empty embedding batch result".into()))
    }

    fn model_id(&self) -> &str {
        &self.spec.model_id
    }

    fn expected_dims(&self) -> Option<usize> {
        (self.spec.dims > 0).then_some(self.spec.dims)
    }

    fn is_enabled(&self) -> bool {
        true
    }

    /// Cloud and external HTTP providers NEVER allow `RawOnly` content to leave the local store.
    fn allows_raw_only(&self) -> bool {
        false
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.run_sync(|| self.embed_batch_async(texts))
    }
}

/// Builds a ready-to-use `Arc<dyn Embedder + Send + Sync>` for `spec` (using cached local ONNX when
/// available without triggering unsolicited network downloads, or `ProviderEmbedder` for HTTP recipes).
pub fn build_embedder_for_spec(
    spec: &ResolvedEmbeddingSpec,
    global_config: Option<&KpGlobalConfig>,
) -> Result<Arc<dyn Embedder + Send + Sync>> {
    match spec.provider_id.as_str() {
        "none" => Ok(Arc::new(FtsOnlyEmbedder)),
        "hash" => Ok(Arc::new(HashEmbedder)),
        "local" => {
            #[cfg(feature = "local-embed")]
            {
                if let Some(loaded) =
                    crate::local_onnx::LocalOnnxEmbedder::try_load_cached(&spec.model_id)?
                {
                    return Ok(Arc::new(loaded));
                }
            }
            Ok(Arc::new(DisabledModelEmbedder::new(
                spec.model_id.clone(),
                spec.dims,
            )))
        }
        _ => {
            let embedder = ProviderEmbedder::from_env_and_config(
                ProviderEmbedderConfig::new(spec.clone()),
                global_config,
            )?;
            Ok(Arc::new(embedder))
        }
    }
}

/// Diagnoses why `spec` cannot construct an active [`Embedder`] (e.g. missing cloud API key,
/// malformed explicit gateway Base URL, or invalid `KP_ORT_DYLIB_SHA256`), returning a
/// secret-redacted human-readable reason suitable for `DaemonEmbedderRuntimeStatus.last_error`.
pub fn diagnose_embedder_spec_issue(
    spec: &ResolvedEmbeddingSpec,
    global_config: Option<&KpGlobalConfig>,
) -> Option<String> {
    diagnose_embedder_spec_issue_with_lookup(spec, |k| std::env::var(k).ok(), global_config)
}

/// Lookup-parameterized variant of [`diagnose_embedder_spec_issue`] for deterministic testing.
pub fn diagnose_embedder_spec_issue_with_lookup<F>(
    spec: &ResolvedEmbeddingSpec,
    env_lookup: F,
    global_config: Option<&KpGlobalConfig>,
) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    let redact_msg = |msg: String| match SecretRedactor::new() {
        Ok(r) => r.redact(&msg),
        Err(_) => msg,
    };
    match spec.provider_id.as_str() {
        "none" | "hash" => None,
        "local" => {
            #[cfg(feature = "local-embed")]
            {
                if let Err(err) = crate::local_onnx::find_local_onnx_spec(&spec.model_id) {
                    return Some(redact_msg(err.to_string()));
                }
                for var in ["KP_ORT_DYLIB_PATH", "ORT_DYLIB_PATH"] {
                    if let Some(raw_path) = env_lookup(var)
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty())
                    {
                        let sha = env_lookup("KP_ORT_DYLIB_SHA256")
                            .map(|s| s.trim().to_owned())
                            .filter(|s| !s.is_empty());
                        if let Err(err) = crate::local_onnx::verify_explicit_ort_dylib(
                            Path::new(&raw_path),
                            sha.as_deref(),
                        ) {
                            return Some(redact_msg(err.to_string()));
                        }
                        break;
                    }
                }
                None
            }
            #[cfg(not(feature = "local-embed"))]
            {
                Some("local ONNX embedding support is disabled (`local-embed` feature off)".into())
            }
        }
        _ => ProviderEmbedder::from_lookup(
            ProviderEmbedderConfig::new(spec.clone()),
            env_lookup,
            global_config,
        )
        .err()
        .map(|err| redact_msg(err.to_string())),
    }
}

/// Builds a ready [`Embedder`] for `spec`, or falls back to [`DisabledModelEmbedder`] alongside a
/// secret-redacted diagnostic reason (`Some(reason)`) when credentials or gateway URLs are invalid.
pub fn try_build_embedder_with_reason(
    spec: &ResolvedEmbeddingSpec,
    global_config: Option<&KpGlobalConfig>,
) -> (Arc<dyn Embedder + Send + Sync>, Option<String>) {
    match build_embedder_for_spec(spec, global_config) {
        Ok(emb) => {
            let reason = if !emb.is_enabled() && spec.provider_id != "none" {
                diagnose_embedder_spec_issue(spec, global_config)
            } else {
                None
            };
            (emb, reason)
        }
        Err(err) => {
            let scrubbed = match SecretRedactor::new() {
                Ok(r) => r.redact(&err.to_string()),
                Err(_) => err.to_string(),
            };
            (
                Arc::new(DisabledModelEmbedder::new(spec.model_id.clone(), spec.dims)),
                Some(scrubbed),
            )
        }
    }
}

#[cfg(test)]
#[path = "providers_tests.rs"]
mod tests;
