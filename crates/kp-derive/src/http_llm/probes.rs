use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::detect::{
    is_loopback_gateway_url, lookup_api_key, resolve_custom_base_url, DetectedHostCli,
    KpGlobalModelConfig, ModelDetectEnv,
};
use super::GenerativeApiProvider;
use crate::llm_runner::HostCliKind;

pub(super) fn detect_logged_in_host_cli(
    env: &ModelDetectEnv,
    file_cfg: &KpGlobalModelConfig,
) -> Option<DetectedHostCli> {
    if let Some(prog_str) = env.get_env("KP_HOST_CLI_PROGRAM") {
        let program = PathBuf::from(prog_str);
        if program.is_file() {
            let kind = match env.get_env("KP_HOST_CLI_KIND") {
                Some("claude" | "claude-code" | "claude_code") => HostCliKind::ClaudeCode,
                Some("codex") => HostCliKind::Codex,
                _ => {
                    let fname = program
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    if fname.contains("claude") {
                        HostCliKind::ClaudeCode
                    } else {
                        HostCliKind::Codex
                    }
                }
            };
            return Some(build_detected_host_cli(kind, program, env, file_cfg));
        }
    }

    let preferred_order = match env.get_env("KP_HOST_CLI_KIND") {
        Some("claude" | "claude-code" | "claude_code") => {
            [HostCliKind::ClaudeCode, HostCliKind::Codex]
        }
        _ => [HostCliKind::Codex, HostCliKind::ClaudeCode],
    };

    for kind in preferred_order {
        let bin_name = match kind {
            HostCliKind::Codex => "codex",
            HostCliKind::ClaudeCode => "claude",
        };
        let Some(program) = find_cli_binary_in_env(bin_name, env) else {
            continue;
        };
        let logged_in = match kind {
            HostCliKind::Codex => probe_codex_logged_in(env, file_cfg),
            HostCliKind::ClaudeCode => probe_claude_logged_in(env, file_cfg),
        };
        if logged_in {
            return Some(build_detected_host_cli(kind, program, env, file_cfg));
        }
    }
    None
}

pub(super) fn build_detected_host_cli(
    kind: HostCliKind,
    program: PathBuf,
    env: &ModelDetectEnv,
    file_cfg: &KpGlobalModelConfig,
) -> DetectedHostCli {
    let mut extra_env = BTreeMap::new();
    let gateway_base_url = match kind {
        HostCliKind::Codex => {
            let gw = resolve_custom_base_url(env, file_cfg, GenerativeApiProvider::OpenAi)
                .ok()
                .flatten();
            if let Some(url) = &gw {
                extra_env.insert("OPENAI_BASE_URL".to_owned(), url.clone());
            }
            if let Some(k) = lookup_api_key(env, file_cfg, GenerativeApiProvider::OpenAi) {
                extra_env.insert("OPENAI_API_KEY".to_owned(), k);
            }
            gw
        }
        HostCliKind::ClaudeCode => {
            let gw = resolve_custom_base_url(env, file_cfg, GenerativeApiProvider::Anthropic)
                .ok()
                .flatten();
            if let Some(url) = &gw {
                extra_env.insert("ANTHROPIC_BASE_URL".to_owned(), url.clone());
            }
            if let Some(k) = lookup_api_key(env, file_cfg, GenerativeApiProvider::Anthropic) {
                extra_env.insert("ANTHROPIC_API_KEY".to_owned(), k);
            }
            gw
        }
    };
    let codex_config_path = (kind == HostCliKind::Codex)
        .then(|| {
            env.get_env("CODEX_HOME")
                .map(|p| Path::new(p).join("config.toml"))
                .or_else(|| {
                    env.get_env("HOME")
                        .map(|h| Path::new(h).join(".codex").join("config.toml"))
                })
                .filter(|p| p.is_file())
        })
        .flatten();

    DetectedHostCli {
        kind,
        program,
        model: env.get_env("KP_GENERATIVE_MODEL").map(str::to_owned),
        codex_config_path,
        gateway_base_url,
        extra_env,
        fallback_http: None,
    }
}

fn find_cli_binary_in_env(name: &str, env: &ModelDetectEnv) -> Option<PathBuf> {
    if let Some(dirs) = &env.search_dirs {
        for dir in dirs {
            let cand = dir.join(name);
            if cand.is_file() {
                return Some(cand);
            }
        }
        return None;
    }
    if let Some(path_var) = env.get_env("PATH") {
        for dir in std::env::split_paths(path_var) {
            let cand = dir.join(name);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    if let Some(home) = env.get_env("HOME") {
        let nvm_dir = Path::new(home).join(".nvm/versions/node");
        if let Ok(entries) = std::fs::read_dir(nvm_dir) {
            let mut versions: Vec<PathBuf> =
                entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
            versions.sort();
            versions.reverse();
            for ver in versions {
                let cand = ver.join("bin").join(name);
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
        let local_bin = Path::new(home).join(".local/bin").join(name);
        if local_bin.is_file() {
            return Some(local_bin);
        }
    }
    for prefix in ["/opt/homebrew/bin", "/usr/local/bin"] {
        let cand = Path::new(prefix).join(name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

fn has_nonempty_json_str(obj: &serde_json::Map<String, Value>, key: &str) -> bool {
    obj.get(key)
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
}

/// Structurally verifies whether local Codex CLI has valid login/credential state (`serde_json` +
/// `toml_edit::DocumentMut` — never substring matching!).
pub fn probe_codex_logged_in(env: &ModelDetectEnv, file_cfg: &KpGlobalModelConfig) -> bool {
    if lookup_api_key(env, file_cfg, GenerativeApiProvider::OpenAi).is_some() {
        return true;
    }
    let codex_dir = env
        .get_env("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| env.get_env("HOME").map(|h| Path::new(h).join(".codex")));
    let Some(dir) = codex_dir else {
        return false;
    };

    if let Ok(raw_auth) = std::fs::read_to_string(dir.join("auth.json")) {
        if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&raw_auth) {
            if has_nonempty_json_str(&obj, "OPENAI_API_KEY")
                || has_nonempty_json_str(&obj, "access_token")
                || has_nonempty_json_str(&obj, "id_token")
                || has_nonempty_json_str(&obj, "api_key")
            {
                return true;
            }
            if let Some(tokens) = obj.get("tokens").and_then(Value::as_object) {
                if has_nonempty_json_str(tokens, "access_token")
                    || has_nonempty_json_str(tokens, "id_token")
                {
                    return true;
                }
            }
        }
    }

    if let Ok(cfg_toml) = std::fs::read_to_string(dir.join("config.toml")) {
        if let Ok(doc) = cfg_toml.parse::<toml_edit::DocumentMut>() {
            if let Some(provider_id) = doc
                .get("model_provider")
                .and_then(|item| item.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if let Some(provider_table) = doc
                    .get("model_providers")
                    .and_then(|item| item.as_table_like())
                    .and_then(|providers| providers.get(provider_id))
                    .and_then(|entry| entry.as_table_like())
                {
                    let env_key_opt = provider_table
                        .get("env_key")
                        .and_then(|item| item.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty());
                    if let Some(env_key_name) = env_key_opt {
                        if env.get_env(env_key_name).is_some()
                            || file_cfg
                                .api_keys
                                .get(env_key_name)
                                .is_some_and(|v| !v.trim().is_empty())
                        {
                            return true;
                        }
                    } else if provider_table
                        .get("base_url")
                        .and_then(|item| item.as_str())
                        .is_some_and(is_loopback_gateway_url)
                    {
                        return true;
                    }
                    if let Some(headers) = provider_table
                        .get("http_headers")
                        .and_then(|item| item.as_table_like())
                    {
                        if ["Authorization", "authorization", "x-api-key", "api-key"]
                            .iter()
                            .any(|hk| {
                                headers
                                    .get(hk)
                                    .and_then(|v| v.as_str())
                                    .is_some_and(|s| !s.trim().is_empty())
                            })
                        {
                            return true;
                        }
                    }
                    if let Some(env_headers) = provider_table
                        .get("env_http_headers")
                        .and_then(|item| item.as_table_like())
                    {
                        if env_headers.iter().any(|(_, v)| {
                            v.as_str()
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .is_some_and(|env_var| env.get_env(env_var).is_some())
                        }) {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

/// Structurally verifies whether local Claude Code CLI has valid login/credential state (`serde_json`
/// parsing with strict boolean & non-empty token checks — never substring matching!).
pub fn probe_claude_logged_in(env: &ModelDetectEnv, file_cfg: &KpGlobalModelConfig) -> bool {
    if lookup_api_key(env, file_cfg, GenerativeApiProvider::Anthropic).is_some()
        || env.get_env("ANTHROPIC_AUTH_TOKEN").is_some()
    {
        return true;
    }

    let mut cred_paths = Vec::new();
    let mut claude_json_paths = Vec::new();
    let mut settings_paths = Vec::new();
    if let Some(cfg_dir) = env.get_env("CLAUDE_CONFIG_DIR") {
        let dir = Path::new(cfg_dir);
        cred_paths.push(dir.join(".credentials.json"));
        claude_json_paths.push(dir.join(".claude.json"));
        settings_paths.push(dir.join("settings.json"));
    }
    if let Some(home) = env.get_env("HOME") {
        let home_path = Path::new(home);
        cred_paths.push(home_path.join(".claude").join(".credentials.json"));
        claude_json_paths.push(home_path.join(".claude.json"));
        settings_paths.push(home_path.join(".claude").join("settings.json"));
    }

    for cred_path in cred_paths {
        if let Ok(raw) = std::fs::read_to_string(cred_path) {
            if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&raw) {
                if has_nonempty_json_str(&obj, "accessToken")
                    || has_nonempty_json_str(&obj, "access_token")
                    || has_nonempty_json_str(&obj, "apiKey")
                    || has_nonempty_json_str(&obj, "anthropicApiKey")
                {
                    return true;
                }
                if let Some(oauth) = obj.get("claudeAiOauth").and_then(Value::as_object) {
                    if has_nonempty_json_str(oauth, "accessToken")
                        || has_nonempty_json_str(oauth, "access_token")
                    {
                        return true;
                    }
                }
            }
        }
    }

    for claude_json_path in claude_json_paths {
        if let Ok(raw) = std::fs::read_to_string(claude_json_path) {
            if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&raw) {
                if has_nonempty_json_str(&obj, "primaryApiKey") {
                    return true;
                }
                if let Some(oauth) = obj.get("claudeAiOauth").and_then(Value::as_object) {
                    if has_nonempty_json_str(oauth, "accessToken")
                        || has_nonempty_json_str(oauth, "access_token")
                    {
                        return true;
                    }
                }
                // `hasCompletedOnboarding: true` alone is NEVER sufficient without a valid
                // `oauthAccount` object, and explicit `hasCompletedOnboarding: false` disqualifies it.
                let onboarding_ok = match obj.get("hasCompletedOnboarding") {
                    None => true,
                    Some(v) => v.as_bool() == Some(true),
                };
                if onboarding_ok {
                    if let Some(acct) = obj.get("oauthAccount").and_then(Value::as_object) {
                        if has_nonempty_json_str(acct, "accountUuid")
                            || has_nonempty_json_str(acct, "emailAddress")
                            || has_nonempty_json_str(acct, "accessToken")
                        {
                            return true;
                        }
                    }
                }
            }
        }
    }

    for settings_path in settings_paths {
        if let Ok(raw) = std::fs::read_to_string(settings_path) {
            if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&raw) {
                if has_nonempty_json_str(&obj, "apiKeyHelper") {
                    return true;
                }
                if let Some(env_obj) = obj.get("env").and_then(Value::as_object) {
                    if has_nonempty_json_str(env_obj, "ANTHROPIC_AUTH_TOKEN")
                        || has_nonempty_json_str(env_obj, "ANTHROPIC_API_KEY")
                    {
                        return true;
                    }
                }
            }
        }
    }
    false
}
