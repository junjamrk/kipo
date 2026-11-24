//! Hardened out-of-session host CLI runners (`claude -p --bare` & `codex exec`) with 4-layer isolation.
//!
//! Isolation patterns adapted from `claude-mem` (`main` branch Apache-2.0):
//! 1. Strip all `CLAUDE_CODE_*` and `CLAUDECODE` variables from child environment;
//! 2. Inject `KP_INTERNAL_EXTRACTION=1` so `kp` hooks immediately exit `0` without self-capture;
//! 3. Execute inside a dedicated, strictly empty temporary working directory (`cwd` jail);
//! 4. Disable all tools, settings, MCP servers, user configs, and hooks on the child CLI.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kp_model::{
    BoxFuture, EntailmentQuery, EntailmentVerdict, ExtractionBatchOutcome, ExtractionBatchRequest,
    KpError, LlmClient, ModelRoutePolicy, Result, SecretRedactor, DEFAULT_MAX_BATCH_TOKENS,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::prompts::{
    build_entailment_prompt, build_extraction_prompt, entailment_verdict_json_schema,
    extraction_batch_json_schema, is_chunk_trivial_or_ack_only, parse_entailment_verdict,
    parse_extraction_envelope, ModelExtractionEnvelope, SKIP_SUMMARY_SENTINEL,
};

/// Sentinel environment variable set on every internal extraction subprocess.
pub const INTERNAL_EXTRACTION_ENV: &str = "KP_INTERNAL_EXTRACTION";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostCliKind {
    ClaudeCode,
    Codex,
}

#[derive(Clone, Debug)]
pub struct HostCliConfig {
    pub kind: HostCliKind,
    pub program: PathBuf,
    pub model: Option<String>,
    /// Optional Codex reasoning effort (`Some("low")` by default on `HostCliKind::Codex`, passing
    /// `-c model_reasoning_effort="low"`; `None` omits the override).
    pub reasoning_effort: Option<String>,
    /// Optional explicit Codex `config.toml` path for selective `model_provider` forwarding
    /// (`None` falls back to `$CODEX_HOME/config.toml` or `$HOME/.codex/config.toml` when readable).
    pub codex_config_path: Option<PathBuf>,
    pub route_policy: ModelRoutePolicy,
    pub timeout: Duration,
    /// Token ceiling per extraction batch (`DEFAULT_MAX_BATCH_TOKENS` by default).
    pub max_batch_tokens: usize,
    /// Optional known remaining host rate-limit percentage (`0..=100`).
    pub remaining_quota_pct: Option<u8>,
}

impl HostCliConfig {
    pub fn for_kind(kind: HostCliKind) -> Self {
        match kind {
            HostCliKind::ClaudeCode => Self::claude_default(),
            HostCliKind::Codex => Self::codex_default(),
        }
    }

    pub fn claude_default() -> Self {
        Self {
            kind: HostCliKind::ClaudeCode,
            program: PathBuf::from("claude"),
            model: None,
            reasoning_effort: None,
            codex_config_path: None,
            route_policy: ModelRoutePolicy::HostCli,
            timeout: Duration::from_secs(180),
            max_batch_tokens: DEFAULT_MAX_BATCH_TOKENS,
            remaining_quota_pct: None,
        }
    }

    pub fn codex_default() -> Self {
        Self {
            kind: HostCliKind::Codex,
            program: PathBuf::from("codex"),
            model: None,
            reasoning_effort: Some("low".to_owned()),
            codex_config_path: None,
            route_policy: ModelRoutePolicy::HostCli,
            timeout: Duration::from_secs(180),
            max_batch_tokens: DEFAULT_MAX_BATCH_TOKENS,
            remaining_quota_pct: None,
        }
    }
}

/// Sanitizes parent environment variables for child CLI isolation:
/// strips `CLAUDECODE` and every `CLAUDE_CODE_*` variable, and sets `KP_INTERNAL_EXTRACTION=1`.
pub fn sanitize_child_env<I, K, V>(parent_env: I) -> BTreeMap<String, String>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut clean = BTreeMap::new();
    for (k, v) in parent_env {
        let key = k.as_ref();
        let upper = key.to_ascii_uppercase();
        if upper == "CLAUDECODE" || upper.starts_with("CLAUDE_CODE_") {
            continue;
        }
        clean.insert(key.to_owned(), v.as_ref().to_owned());
    }
    clean.insert(INTERNAL_EXTRACTION_ENV.to_owned(), "1".to_owned());
    clean
}

/// Fully specified isolated subprocess invocation passed to `SubprocessTransport`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubprocessSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    pub stdin: String,
    pub output_file: Option<PathBuf>,
    pub timeout: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubprocessRawOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub output_file_content: Option<String>,
}

/// Pluggable process execution transport enabling hermetic zero-network unit tests alongside `std::process::Command`.
pub trait SubprocessTransport: Send + Sync {
    fn execute(&self, spec: &SubprocessSpec) -> Result<SubprocessRawOutput>;
}

/// Production `std::process::Command` runner enforcing `env_clear()`, sanitized env, empty `cwd`, and timeout.
#[derive(Clone, Copy, Debug, Default)]
pub struct StdSubprocessTransport;

impl SubprocessTransport for StdSubprocessTransport {
    fn execute(&self, spec: &SubprocessSpec) -> Result<SubprocessRawOutput> {
        let mut child = Command::new(&spec.program)
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(&spec.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| {
                KpError::invalid("host_cli_spawn", "failed to spawn host CLI subprocess")
            })?;

        // Drain stdout and stderr on dedicated reader threads BEFORE writing stdin or polling `try_wait`
        // so >16KB `--json` streaming outputs never fill OS pipe buffers and deadlock the child.
        let stdout_reader = child.stdout.take().map(|mut pipe| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = std::io::Read::read_to_end(&mut pipe, &mut buf);
                buf
            })
        });
        let stderr_reader = child.stderr.take().map(|mut pipe| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = std::io::Read::read_to_end(&mut pipe, &mut buf);
                buf
            })
        });

        if let Some(mut stdin_pipe) = child.stdin.take() {
            if let Err(err) = stdin_pipe.write_all(spec.stdin.as_bytes()) {
                // If the child exited immediately (e.g. CLI flag error or fast 429), `BrokenPipe` is expected;
                // continue to collect exit status and stderr rather than masking the root error.
                if err.kind() != std::io::ErrorKind::BrokenPipe {
                    let _ = child.kill();
                    let _ = child.wait();
                    if let Some(h) = stdout_reader {
                        let _ = h.join();
                    }
                    if let Some(h) = stderr_reader {
                        let _ = h.join();
                    }
                    return Err(KpError::invalid(
                        "host_cli",
                        "failed writing prompt to stdin",
                    ));
                }
            }
        }

        let deadline = Instant::now() + spec.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        if let Some(h) = stdout_reader {
                            let _ = h.join();
                        }
                        if let Some(h) = stderr_reader {
                            let _ = h.join();
                        }
                        return Err(KpError::invalid("host_cli", "subprocess timed out"));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    if let Some(h) = stdout_reader {
                        let _ = h.join();
                    }
                    if let Some(h) = stderr_reader {
                        let _ = h.join();
                    }
                    return Err(KpError::invalid("host_cli", "failed waiting on subprocess"));
                }
            }
        };

        let stdout_bytes = match stdout_reader {
            Some(handle) => handle
                .join()
                .map_err(|_| KpError::invalid("host_cli", "stdout reader thread panicked"))?,
            None => Vec::new(),
        };
        let stderr_bytes = match stderr_reader {
            Some(handle) => handle
                .join()
                .map_err(|_| KpError::invalid("host_cli", "stderr reader thread panicked"))?,
            None => Vec::new(),
        };

        let output_file_content = match &spec.output_file {
            Some(path) if path.is_file() => std::fs::read_to_string(path).ok(),
            _ => None,
        };

        Ok(SubprocessRawOutput {
            exit_code: status.code(),
            stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
            stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
            output_file_content,
        })
    }
}

/// Hardened out-of-session LLM runner supporting Claude Code and Codex CLIs.
pub struct HostCliRunner<T: SubprocessTransport = StdSubprocessTransport> {
    config: HostCliConfig,
    transport: Arc<T>,
    redactor: Arc<SecretRedactor>,
    env_override: Option<BTreeMap<String, String>>,
    extra_env: BTreeMap<String, String>,
}

impl HostCliRunner<StdSubprocessTransport> {
    pub fn new(config: HostCliConfig) -> Result<Self> {
        Self::with_transport(config, StdSubprocessTransport)
    }
}

impl<T: SubprocessTransport + 'static> HostCliRunner<T> {
    pub fn with_transport(config: HostCliConfig, transport: T) -> Result<Self> {
        let redactor =
            SecretRedactor::new().map_err(|_| KpError::invalid("redactor", "invalid regex"))?;
        Ok(Self {
            config,
            transport: Arc::new(transport),
            redactor: Arc::new(redactor),
            env_override: None,
            extra_env: BTreeMap::new(),
        })
    }

    /// Supplies a deterministic parent environment map (used by tests to verify `CLAUDE_CODE_*` stripping).
    pub fn with_parent_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env_override = Some(env);
        self
    }

    /// Merges explicit gateway/auth environment variables (e.g. `OPENAI_BASE_URL`,
    /// `ANTHROPIC_BASE_URL`) into the sanitized child CLI environment while preserving isolation.
    pub fn with_env_overrides(mut self, extra: BTreeMap<String, String>) -> Self {
        self.extra_env = extra;
        self
    }

    pub fn config(&self) -> &HostCliConfig {
        &self.config
    }

    pub(crate) fn build_clean_env(&self) -> BTreeMap<String, String> {
        let mut env = match &self.env_override {
            Some(map) => sanitize_child_env(map.iter()),
            None => sanitize_child_env(std::env::vars()),
        };
        for (k, v) in &self.extra_env {
            let upper = k.to_ascii_uppercase();
            if upper == "CLAUDECODE" || upper.starts_with("CLAUDE_CODE_") {
                continue;
            }
            env.insert(k.clone(), v.clone());
        }
        env.insert(INTERNAL_EXTRACTION_ENV.to_owned(), "1".to_owned());
        if let Some(parent) = self
            .config
            .program
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
        {
            if let Some(parent_str) = parent.to_str() {
                let current_path = env.get("PATH").cloned().unwrap_or_default();
                if !current_path.split(':').any(|seg| seg == parent_str) {
                    let merged = if current_path.is_empty() {
                        parent_str.to_owned()
                    } else {
                        format!("{parent_str}:{current_path}")
                    };
                    env.insert("PATH".to_owned(), merged);
                }
            }
        }
        env
    }

    fn run_cli_sync(
        config: &HostCliConfig,
        transport: &T,
        clean_env: BTreeMap<String, String>,
        schema: &Value,
        prompt: String,
    ) -> Result<SubprocessRawOutput> {
        // Dedicated empty cwd jail: zero files are ever created inside `empty_cwd`.
        let empty_cwd = tempfile::tempdir()
            .map_err(|_| KpError::invalid("host_cli", "failed creating empty temp cwd"))?;
        // Separate scratch dir holds Codex `--output-schema` and `-o` files so `empty_cwd` stays empty.
        let io_scratch = tempfile::tempdir()
            .map_err(|_| KpError::invalid("host_cli", "failed creating schema scratch dir"))?;

        let compact_schema = serde_json::to_string(schema)
            .map_err(|_| KpError::invalid("schema", "failed serializing JSON schema"))?;

        let (args, output_file) = match config.kind {
            HostCliKind::ClaudeCode => {
                let mut args = vec![
                    "-p".to_owned(),
                    "--bare".to_owned(),
                    "--tools".to_owned(),
                    String::new(),
                    "--setting-sources".to_owned(),
                    String::new(),
                    // `--json-schema` only yields the `structured_output` wrapper in JSON output mode.
                    "--output-format".to_owned(),
                    "json".to_owned(),
                    "--json-schema".to_owned(),
                    compact_schema,
                ];
                if let Some(model) = &config.model {
                    args.push("--model".to_owned());
                    args.push(model.clone());
                }
                (args, None)
            }
            HostCliKind::Codex => {
                let schema_path = io_scratch.path().join("schema.json");
                let out_path = io_scratch.path().join("output.json");
                std::fs::write(&schema_path, compact_schema.as_bytes())
                    .map_err(|_| KpError::invalid("host_cli", "failed writing schema file"))?;
                let empty_cwd_str = path_to_str(empty_cwd.path())?;
                let schema_str = path_to_str(&schema_path)?;
                let out_str = path_to_str(&out_path)?;
                let mut args = vec![
                    "exec".to_owned(),
                    "-s".to_owned(),
                    "read-only".to_owned(),
                    "--ephemeral".to_owned(),
                    "--skip-git-repo-check".to_owned(),
                    "--ignore-user-config".to_owned(),
                    "--disable".to_owned(),
                    "hooks".to_owned(),
                    "--disable".to_owned(),
                    "apps".to_owned(),
                    "--disable".to_owned(),
                    "plugins".to_owned(),
                    "--disable".to_owned(),
                    "memories".to_owned(),
                ];
                if let Some(cfg_path) = resolve_codex_config_path(config, &clean_env) {
                    if let Ok(raw_toml) = std::fs::read_to_string(cfg_path) {
                        for override_kv in extract_codex_provider_overrides(&raw_toml) {
                            args.push("-c".to_owned());
                            args.push(override_kv);
                        }
                    }
                }
                if let Some(effort) = config
                    .reasoning_effort
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    let encoded = serde_json::to_string(effort)
                        .map_err(|_| KpError::invalid("reasoning_effort", "invalid string"))?;
                    args.push("-c".to_owned());
                    args.push(format!("model_reasoning_effort={encoded}"));
                }
                if let Some(model) = &config.model {
                    args.push("--model".to_owned());
                    args.push(model.clone());
                }
                args.extend([
                    "-C".to_owned(),
                    empty_cwd_str,
                    "--output-schema".to_owned(),
                    schema_str,
                    "--json".to_owned(),
                    "-o".to_owned(),
                    out_str,
                    "-".to_owned(),
                ]);
                (args, Some(out_path))
            }
        };

        let spec = SubprocessSpec {
            program: config.program.clone(),
            args,
            env: clean_env,
            cwd: empty_cwd.path().to_path_buf(),
            stdin: prompt,
            output_file,
            timeout: config.timeout,
        };
        transport.execute(&spec)
    }
}

fn path_to_str(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| KpError::invalid("path", "non-UTF8 temporary path"))
}

fn resolve_codex_config_path(
    config: &HostCliConfig,
    clean_env: &BTreeMap<String, String>,
) -> Option<PathBuf> {
    if let Some(explicit) = &config.codex_config_path {
        return Some(explicit.clone());
    }
    if let Some(codex_home) = clean_env.get("CODEX_HOME").filter(|v| !v.trim().is_empty()) {
        return Some(PathBuf::from(codex_home).join("config.toml"));
    }
    if let Some(home) = clean_env.get("HOME").filter(|v| !v.trim().is_empty()) {
        return Some(PathBuf::from(home).join(".codex").join("config.toml"));
    }
    None
}

/// Parses a Codex `config.toml` via `toml_edit` and extracts **only** top-level `model_provider`
/// and its matching `[model_providers.<id>]` entries as dot-safe `-c` `key=value` overrides.
///
/// Codex's `-c <key>=<value>` flag splits `<key>` naively on `.` without honoring TOML quoted LHS
/// segments while parsing `<value>` with a full TOML parser. Nested sub-tables (such as
/// `query_params` or `[model_providers.<id>.http_headers]`) are rendered as single-line TOML inline
/// tables on the RHS (`model_providers.<id>.<field>={"k"="v", ...}`), and if `<id>` itself contains
/// `.` or non-bare characters, the entire provider table is emitted on the RHS of `model_providers=...`.
pub fn extract_codex_provider_overrides(config_toml: &str) -> Vec<String> {
    let Ok(doc) = config_toml.parse::<toml_edit::DocumentMut>() else {
        return Vec::new();
    };
    let Some(provider_id) = doc
        .get("model_provider")
        .and_then(|item| item.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Vec::new();
    };
    let Ok(encoded_provider) = serde_json::to_string(provider_id) else {
        return Vec::new();
    };

    let mut overrides = vec![format!("model_provider={encoded_provider}")];
    let Some(provider_table) = doc
        .get("model_providers")
        .and_then(|item| item.as_table_like())
        .and_then(|providers| providers.get(provider_id))
        .and_then(|entry| entry.as_table_like())
    else {
        return overrides;
    };

    let rendered_fields: Vec<(&str, String)> = provider_table
        .iter()
        .filter_map(|(field, item)| Some((field, render_toml_item_rhs(item)?)))
        .collect();
    if rendered_fields.is_empty() {
        return overrides;
    }

    if is_codex_bare_lhs_segment(provider_id)
        && rendered_fields
            .iter()
            .all(|(field, _)| is_codex_bare_lhs_segment(field))
    {
        for (field, rhs) in rendered_fields {
            overrides.push(format!("model_providers.{provider_id}.{field}={rhs}"));
        }
    } else {
        let rendered_provider = render_table_like_inline(provider_table);
        overrides.push(format!(
            "model_providers={{{encoded_provider}={rendered_provider}}}"
        ));
    }
    overrides
}

fn render_toml_item_rhs(item: &toml_edit::Item) -> Option<String> {
    if let Some(sub) = item.as_table_like() {
        return Some(render_table_like_inline(sub));
    }
    Some(render_toml_value_rhs(item.as_value()?))
}

fn render_table_like_inline(table: &dyn toml_edit::TableLike) -> String {
    let entries: Vec<String> = table
        .iter()
        .filter_map(|(k, v)| {
            let rhs = render_toml_item_rhs(v)?;
            let quoted_key = serde_json::to_string(k).ok()?;
            Some(format!("{quoted_key}={rhs}"))
        })
        .collect();
    format!("{{{}}}", entries.join(", "))
}

fn render_toml_value_rhs(val: &toml_edit::Value) -> String {
    match val {
        toml_edit::Value::String(s) => {
            serde_json::to_string(s.value()).unwrap_or_else(|_| format!("\"{}\"", s.value()))
        }
        toml_edit::Value::Integer(n) => n.value().to_string(),
        toml_edit::Value::Float(f) => {
            let s = f.value().to_string();
            if s.contains('.') || s.contains('e') || s.contains('E') {
                s
            } else {
                format!("{s}.0")
            }
        }
        toml_edit::Value::Boolean(b) => b.value().to_string(),
        toml_edit::Value::Datetime(dt) => dt.value().to_string(),
        toml_edit::Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(render_toml_value_rhs).collect();
            format!("[{}]", items.join(", "))
        }
        toml_edit::Value::InlineTable(inline) => render_table_like_inline(inline),
    }
}

fn is_codex_bare_lhs_segment(seg: &str) -> bool {
    !seg.is_empty()
        && seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn parse_live_reasoning_effort_env(clean_env: &BTreeMap<String, String>) -> Option<Option<String>> {
    clean_env
        .get("KP_LIVE_REASONING_EFFORT")
        .map(String::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|val| {
            if val.eq_ignore_ascii_case("none") || val.eq_ignore_ascii_case("default") {
                None
            } else {
                Some(val.to_owned())
            }
        })
}

/// Detects HTTP `429` or host rate-limit / quota exhaustion signals from CLI output.
pub fn detect_low_quota(
    raw: &SubprocessRawOutput,
    min_required_pct: u8,
) -> Option<(Option<u8>, String)> {
    for stream in [&raw.stderr, &raw.stdout] {
        if let Some(pct) = parse_remaining_pct_from_json(stream) {
            if pct < min_required_pct {
                return Some((
                    Some(pct),
                    format!("remaining rate limit {pct}% below watermark {min_required_pct}%"),
                ));
            }
        }
    }
    let combined = format!("{}\n{}", raw.stderr, raw.stdout);
    let lower = combined.to_ascii_lowercase();
    let quota_markers = [
        "429",
        "too many requests",
        "rate_limit",
        "rate limit",
        "quota exceeded",
        "insufficient_quota",
        "usage limit",
        "credit balance is too low",
    ];
    if raw.exit_code != Some(0) && quota_markers.iter().any(|m| lower.contains(m)) {
        return Some((
            None,
            combined
                .lines()
                .next()
                .unwrap_or("rate limited")
                .trim()
                .to_owned(),
        ));
    }
    if lower.contains("\"status\":429") || lower.contains("\"code\":429") {
        return Some((None, "HTTP 429 rate limit".to_owned()));
    }
    None
}

/// Detects CLI authentication expiry, missing login, or HTTP `401`/`403` credential errors from
/// a failed host CLI subprocess output (strictly excluding `429` / rate-limit / quota exhaustion).
pub fn detect_cli_auth_failure(raw: &SubprocessRawOutput) -> Option<String> {
    if raw.exit_code == Some(0) || detect_low_quota(raw, 1).is_some() {
        return None;
    }
    let combined = format!("{}\n{}", raw.stderr, raw.stdout);
    let lower = combined.to_ascii_lowercase();
    let auth_markers = [
        "401",
        "unauthorized",
        "unauthenticated",
        "authentication_error",
        "authentication failed",
        "auth_error",
        "invalid_api_key",
        "invalid api key",
        "incorrect api key",
        "api key not found",
        "missing api key",
        "not logged in",
        "please log in",
        "login required",
        "claude login",
        "claude auth",
        "codex login",
        "token expired",
        "expired token",
        "invalid_grant",
        "403 forbidden",
        "permission_error",
    ];
    if auth_markers.iter().any(|m| lower.contains(m)) {
        return Some("host CLI authentication failed or session expired".to_owned());
    }
    None
}

fn parse_remaining_pct_from_json(text: &str) -> Option<u8> {
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
            for key in [
                "rate_limit_remaining_pct",
                "remaining_rate_limit_percent",
                "min_rate_limit_remaining_percent",
            ] {
                if let Some(n) = val.get(key).and_then(Value::as_u64) {
                    if let Ok(pct) = u8::try_from(n) {
                        return Some(pct);
                    }
                }
            }
        }
    }
    None
}

fn select_payload_text(raw: &SubprocessRawOutput) -> &str {
    if let Some(file_out) = &raw.output_file_content {
        if !file_out.trim().is_empty() {
            return file_out.trim();
        }
    }
    raw.stdout.trim()
}

fn unwrap_cli_json_payload(payload: &str) -> Result<Value> {
    let stripped = strip_markdown_fence(payload.trim());
    if let Ok(val) = serde_json::from_str::<Value>(stripped) {
        if let Some(structured) = val.get("structured_output") {
            return Ok(structured.clone());
        }
        if let Some(result_field) = val.get("result") {
            if let Some(s) = result_field.as_str() {
                if let Ok(inner) = serde_json::from_str::<Value>(strip_markdown_fence(s.trim())) {
                    return Ok(inner);
                }
            } else if result_field.is_object() {
                return Ok(result_field.clone());
            }
        }
        return Ok(val);
    }
    // Fallback for Codex JSONL streams when `-o` was not populated: take the last JSON object line.
    for line in stripped.lines().rev() {
        let line = line.trim();
        if line.starts_with('{') {
            if let Ok(val) = serde_json::from_str::<Value>(line) {
                if let Some(text) = val.get("text").and_then(Value::as_str) {
                    if let Ok(inner) = serde_json::from_str::<Value>(strip_markdown_fence(text)) {
                        return Ok(inner);
                    }
                }
                return Ok(val);
            }
        }
    }
    Err(KpError::invalid(
        "host_cli_json",
        "CLI output was not valid structured JSON",
    ))
}

pub(crate) fn strip_markdown_fence(text: &str) -> &str {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("```") {
        let body = match rest.find('\n') {
            Some(idx) => &rest[idx + 1..],
            None => rest,
        };
        if let Some(clean) = body.trim_end().strip_suffix("```") {
            return clean.trim();
        }
    }
    trimmed
}

impl<T: SubprocessTransport + 'static> LlmClient for HostCliRunner<T> {
    fn is_raw_only(&self) -> bool {
        self.config.route_policy == ModelRoutePolicy::RawOnly
    }

    fn route_policy(&self) -> ModelRoutePolicy {
        self.config.route_policy.clone()
    }

    fn extract_batch<'a>(
        &'a self,
        request: &'a ExtractionBatchRequest,
    ) -> BoxFuture<'a, Result<ExtractionBatchOutcome>> {
        Box::pin(async move {
            request.validate_request()?;

            match &self.config.route_policy {
                ModelRoutePolicy::RawOnly => return Ok(ExtractionBatchOutcome::SkippedEmpty),
                ModelRoutePolicy::NamedEndpoint(_) => {
                    return Err(KpError::invalid(
                        "model_route_policy",
                        "HostCliRunner cannot execute NamedEndpoint policy",
                    ));
                }
                ModelRoutePolicy::HostCli => {}
            }

            if let Some(remaining) = self.config.remaining_quota_pct {
                if remaining < request.min_rate_limit_remaining_pct {
                    return Ok(ExtractionBatchOutcome::SkippedLowQuota {
                        remaining_pct: Some(remaining),
                        reason: format!(
                            "remaining quota {remaining}% below watermark {}%",
                            request.min_rate_limit_remaining_pct
                        ),
                    });
                }
            }

            if request.chunks.is_empty()
                || request
                    .chunks
                    .iter()
                    .all(|chunk| is_chunk_trivial_or_ack_only(&chunk.body))
            {
                return Ok(ExtractionBatchOutcome::SkippedEmpty);
            }

            let prompt = build_extraction_prompt(request, &self.redactor);
            let schema = extraction_batch_json_schema();
            let mut config = self.config.clone();
            let transport = Arc::clone(&self.transport);
            let clean_env = self.build_clean_env();
            if let Some(env_effort) = parse_live_reasoning_effort_env(&clean_env) {
                config.reasoning_effort = env_effort;
            }
            let min_pct = request.min_rate_limit_remaining_pct;

            let (tx, rx) = tokio::sync::oneshot::channel();
            std::thread::spawn(move || {
                let res = Self::run_cli_sync(&config, &transport, clean_env, &schema, prompt);
                let _ = tx.send(res);
            });
            let raw = rx
                .await
                .map_err(|_| KpError::invalid("host_cli", "runner worker thread cancelled"))??;

            if let Some((remaining_pct, reason)) = detect_low_quota(&raw, min_pct) {
                return Ok(ExtractionBatchOutcome::SkippedLowQuota {
                    remaining_pct,
                    reason,
                });
            }

            if raw.exit_code.is_some_and(|code| code != 0)
                && raw
                    .output_file_content
                    .as_deref()
                    .is_none_or(|s| s.trim().is_empty())
            {
                if detect_cli_auth_failure(&raw).is_some() {
                    return Err(KpError::invalid(
                        "host_cli_auth",
                        "host CLI authentication failed or session expired",
                    ));
                }
                return Err(KpError::invalid(
                    "host_cli",
                    "host CLI exited with non-zero status",
                ));
            }

            let payload = select_payload_text(&raw);
            if payload.is_empty() || payload.contains(SKIP_SUMMARY_SENTINEL) {
                return Ok(ExtractionBatchOutcome::SkippedEmpty);
            }

            let json_val = unwrap_cli_json_payload(payload)?;
            let envelope: ModelExtractionEnvelope = parse_extraction_envelope(json_val)?;
            if envelope.skip_summary || envelope.atoms.is_empty() {
                Ok(ExtractionBatchOutcome::SkippedEmpty)
            } else {
                Ok(ExtractionBatchOutcome::Completed {
                    atoms: envelope.atoms,
                })
            }
        })
    }

    fn judge_entailment<'a>(
        &'a self,
        query: &'a EntailmentQuery,
    ) -> BoxFuture<'a, Result<EntailmentVerdict>> {
        Box::pin(async move {
            match &self.config.route_policy {
                ModelRoutePolicy::RawOnly => return Ok(EntailmentVerdict::Unrelated),
                ModelRoutePolicy::NamedEndpoint(_) => {
                    return Err(KpError::invalid(
                        "model_route_policy",
                        "HostCliRunner cannot execute NamedEndpoint policy",
                    ));
                }
                ModelRoutePolicy::HostCli => {}
            }

            let prompt = build_entailment_prompt(query, &self.redactor);
            let schema = entailment_verdict_json_schema();
            let mut config = self.config.clone();
            let transport = Arc::clone(&self.transport);
            let clean_env = self.build_clean_env();
            // Entailment runs at full default reasoning effort (`None`) rather than `"low"`
            // unless `KP_LIVE_REASONING_EFFORT` explicitly overrides it.
            config.reasoning_effort = parse_live_reasoning_effort_env(&clean_env).flatten();

            let (tx, rx) = tokio::sync::oneshot::channel();
            std::thread::spawn(move || {
                let res = Self::run_cli_sync(&config, &transport, clean_env, &schema, prompt);
                let _ = tx.send(res);
            });
            let raw = rx
                .await
                .map_err(|_| KpError::invalid("host_cli", "runner worker thread cancelled"))??;

            if detect_low_quota(&raw, 1).is_some() {
                return Err(KpError::invalid(
                    "host_cli_quota",
                    "host CLI rate limit or quota exhausted during entailment",
                ));
            }

            if raw.exit_code.is_some_and(|code| code != 0)
                && raw
                    .output_file_content
                    .as_deref()
                    .is_none_or(|s| s.trim().is_empty())
            {
                if detect_cli_auth_failure(&raw).is_some() {
                    return Err(KpError::invalid(
                        "host_cli_auth",
                        "host CLI authentication failed or session expired during entailment",
                    ));
                }
                return Err(KpError::invalid(
                    "host_cli",
                    "host CLI exited with non-zero status during entailment",
                ));
            }
            let payload = select_payload_text(&raw);
            let json_val = unwrap_cli_json_payload(payload)?;
            parse_entailment_verdict(json_val)
        })
    }

    fn complete_structured_json<'a>(
        &'a self,
        _schema_name: &'a str,
        json_schema: &'a Value,
        prompt: &'a str,
    ) -> BoxFuture<'a, Result<Option<Value>>> {
        Box::pin(async move {
            match &self.config.route_policy {
                ModelRoutePolicy::RawOnly => return Ok(None),
                ModelRoutePolicy::NamedEndpoint(_) => {
                    return Err(KpError::invalid(
                        "model_route_policy",
                        "HostCliRunner cannot execute NamedEndpoint policy",
                    ));
                }
                ModelRoutePolicy::HostCli => {}
            }
            let safe_prompt = self.redactor.redact(prompt);
            let schema = json_schema.clone();
            let mut config = self.config.clone();
            let transport = Arc::clone(&self.transport);
            let clean_env = self.build_clean_env();
            config.reasoning_effort = parse_live_reasoning_effort_env(&clean_env).flatten();

            let (tx, rx) = tokio::sync::oneshot::channel();
            std::thread::spawn(move || {
                let res = Self::run_cli_sync(&config, &transport, clean_env, &schema, safe_prompt);
                let _ = tx.send(res);
            });
            let raw = rx
                .await
                .map_err(|_| KpError::invalid("host_cli", "runner worker thread cancelled"))??;

            if detect_low_quota(&raw, 1).is_some() {
                return Err(KpError::invalid(
                    "host_cli_quota",
                    "host CLI rate limit or quota exhausted during structured completion",
                ));
            }
            if raw.exit_code.is_some_and(|code| code != 0)
                && raw
                    .output_file_content
                    .as_deref()
                    .is_none_or(|s| s.trim().is_empty())
            {
                if detect_cli_auth_failure(&raw).is_some() {
                    return Err(KpError::invalid(
                        "host_cli_auth",
                        "host CLI authentication failed or session expired",
                    ));
                }
                return Err(KpError::invalid(
                    "host_cli",
                    "host CLI exited with non-zero status",
                ));
            }
            let payload = select_payload_text(&raw);
            if payload.is_empty() || payload.contains(SKIP_SUMMARY_SENTINEL) {
                return Ok(None);
            }
            let json_val = unwrap_cli_json_payload(payload)?;
            Ok(Some(self.redactor.redact_json(&json_val)))
        })
    }
}

/// Policy-enforcing dispatcher routing `HostCli`, `NamedEndpoint`, and `RawOnly` policies cleanly.
pub struct PolicyRoutedLlmClient {
    policy: ModelRoutePolicy,
    host_cli: Arc<dyn LlmClient>,
    named_endpoints: BTreeMap<String, Arc<dyn LlmClient>>,
}

impl PolicyRoutedLlmClient {
    pub fn new(policy: ModelRoutePolicy, host_cli: Arc<dyn LlmClient>) -> Self {
        Self {
            policy,
            host_cli,
            named_endpoints: BTreeMap::new(),
        }
    }

    pub fn register_endpoint(
        mut self,
        name: impl Into<String>,
        client: Arc<dyn LlmClient>,
    ) -> Self {
        self.named_endpoints.insert(name.into(), client);
        self
    }
}

impl LlmClient for PolicyRoutedLlmClient {
    fn is_raw_only(&self) -> bool {
        matches!(self.policy, ModelRoutePolicy::RawOnly)
            || (matches!(self.policy, ModelRoutePolicy::HostCli) && self.host_cli.is_raw_only())
    }

    fn route_policy(&self) -> ModelRoutePolicy {
        if self.is_raw_only() {
            ModelRoutePolicy::RawOnly
        } else {
            self.policy.clone()
        }
    }

    fn extract_batch<'a>(
        &'a self,
        request: &'a ExtractionBatchRequest,
    ) -> BoxFuture<'a, Result<ExtractionBatchOutcome>> {
        Box::pin(async move {
            match &self.policy {
                ModelRoutePolicy::RawOnly => Ok(ExtractionBatchOutcome::SkippedEmpty),
                ModelRoutePolicy::HostCli => self.host_cli.extract_batch(request).await,
                ModelRoutePolicy::NamedEndpoint(name) => {
                    let client = self.named_endpoints.get(name).ok_or_else(|| {
                        KpError::invalid("model_route_policy", "unconfigured named endpoint")
                    })?;
                    client.extract_batch(request).await
                }
            }
        })
    }

    fn judge_entailment<'a>(
        &'a self,
        query: &'a EntailmentQuery,
    ) -> BoxFuture<'a, Result<EntailmentVerdict>> {
        Box::pin(async move {
            match &self.policy {
                ModelRoutePolicy::RawOnly => Ok(EntailmentVerdict::Unrelated),
                ModelRoutePolicy::HostCli => self.host_cli.judge_entailment(query).await,
                ModelRoutePolicy::NamedEndpoint(name) => {
                    let client = self.named_endpoints.get(name).ok_or_else(|| {
                        KpError::invalid("model_route_policy", "unconfigured named endpoint")
                    })?;
                    client.judge_entailment(query).await
                }
            }
        })
    }

    fn complete_structured_json<'a>(
        &'a self,
        schema_name: &'a str,
        json_schema: &'a Value,
        prompt: &'a str,
    ) -> BoxFuture<'a, Result<Option<Value>>> {
        Box::pin(async move {
            match &self.policy {
                ModelRoutePolicy::RawOnly => Ok(None),
                ModelRoutePolicy::HostCli => {
                    self.host_cli
                        .complete_structured_json(schema_name, json_schema, prompt)
                        .await
                }
                ModelRoutePolicy::NamedEndpoint(name) => {
                    let client = self.named_endpoints.get(name).ok_or_else(|| {
                        KpError::invalid("model_route_policy", "unconfigured named endpoint")
                    })?;
                    client
                        .complete_structured_json(schema_name, json_schema, prompt)
                        .await
                }
            }
        })
    }
}

/// Recorded subprocess call captured by `RecordingMockTransport` in hermetic tests.
#[derive(Clone, Debug)]
pub struct RecordedInvocation {
    pub spec: SubprocessSpec,
    pub cwd_was_empty: bool,
    pub schema_file_existed: bool,
}

type MockResponder = Box<dyn Fn(&SubprocessSpec) -> Result<SubprocessRawOutput> + Send + Sync>;

pub struct RecordingMockTransport {
    invocations: Mutex<Vec<RecordedInvocation>>,
    responder: MockResponder,
}

impl RecordingMockTransport {
    pub fn new(
        responder: impl Fn(&SubprocessSpec) -> Result<SubprocessRawOutput> + Send + Sync + 'static,
    ) -> Self {
        Self {
            invocations: Mutex::new(Vec::new()),
            responder: Box::new(responder),
        }
    }

    pub fn recorded(&self) -> Result<Vec<RecordedInvocation>> {
        self.invocations
            .lock()
            .map(|g| g.clone())
            .map_err(|_| KpError::invalid("mock_transport", "poisoned lock"))
    }
}

impl SubprocessTransport for RecordingMockTransport {
    fn execute(&self, spec: &SubprocessSpec) -> Result<SubprocessRawOutput> {
        let cwd_was_empty = std::fs::read_dir(&spec.cwd)
            .map(|mut d| d.next().is_none())
            .unwrap_or(false);
        let schema_file_existed = spec
            .args
            .windows(2)
            .find(|w| w[0] == "--output-schema")
            .is_some_and(|w| Path::new(&w[1]).is_file());
        let mut guard = self
            .invocations
            .lock()
            .map_err(|_| KpError::invalid("mock_transport", "poisoned lock"))?;
        guard.push(RecordedInvocation {
            spec: spec.clone(),
            cwd_was_empty,
            schema_file_existed,
        });
        drop(guard);
        (self.responder)(spec)
    }
}

#[cfg(test)]
mod tests {
    use kp_model::{CalendarTable, EpochMs, ExtractionChunkSlice, FactKind, SourceId};
    use serde_json::json;

    use super::*;

    fn sample_request(body: &str) -> Result<ExtractionBatchRequest> {
        Ok(ExtractionBatchRequest {
            source_id: SourceId::new("src_01")?,
            speaker: "bob".parse()?,
            reference_calendar: CalendarTable::from_reference_in_tz(
                EpochMs(1_795_089_600_000),
                &kp_model::TimeZone::get("Asia/Shanghai")
                    .map_err(|_| kp_model::KpError::invalid("tz", "Asia/Shanghai"))?,
            )?,
            active_concepts: Vec::new(),
            active_decisions: Vec::new(),
            speaker_own_facts: Vec::new(),
            speaker_own_facts_by_actor: BTreeMap::new(),
            chunks: vec![ExtractionChunkSlice {
                chunk_id: "c0".into(),
                heading: "turn".into(),
                body: body.into(),
                source_id: SourceId::new("src_01")?,
                author: "bob".parse()?,
                base_anchor: Default::default(),
                role: Default::default(),
                reply_to_chunk_id: None,
            }],
            min_rate_limit_remaining_pct: 15,
        })
    }

    #[tokio::test]
    async fn claude_runner_enforces_four_layer_isolation_and_parses_atoms() -> Result<()> {
        // Payload shaped exactly by the strict `extraction_batch_json_schema()` (all keys present, nullable).
        let transport = RecordingMockTransport::new(|_spec| {
            Ok(SubprocessRawOutput {
                exit_code: Some(0),
                stdout: json!({
                    "type": "result",
                    "structured_output": {
                        "skip_summary": false,
                        "atoms": [{
                            "kind": "constraint",
                            "claim_text": "我自己出差从不坐红眼航班",
                            "value_json": "{\"rule\":\"no_red_eye\"}",
                            "scope": {"scope": "speaker_self", "mentions": null, "group_concept": null},
                            "ambiguous_scope": null,
                            "condition_concept": {
                                "ref_type": "propose_new",
                                "concept_id": null,
                                "kind": "condition",
                                "label": "出差",
                                "domain": "travel",
                                "polarity": "negative"
                            },
                            "action_concept": null,
                            "group_concept": null,
                            "member_mention": null,
                            "valid_dates": null,
                            "speaker": null,
                            "assumed_premise_ids": [],
                            "quote": {
                                "chunk_id": "c0",
                                "verbatim_quote": "我自己出差从不坐红眼航班",
                                "turn_ordinal": null,
                                "approx_char_offset": null
                            },
                            "evidence": null,
                            "write_op": {"op": "add", "target_fact_id": null}
                        }]
                    }
                })
                .to_string(),
                stderr: String::new(),
                output_file_content: None,
            })
        });

        let mut cfg = HostCliConfig::claude_default();
        cfg.model = Some("haiku".into());
        let parent_env = BTreeMap::from([
            ("CLAUDECODE".into(), "1".into()),
            ("CLAUDE_CODE_ENTRYPOINT".into(), "cli".into()),
            ("CLAUDE_CODE_SSE_PORT".into(), "9999".into()),
            ("PATH".into(), "/usr/bin:/bin".into()),
        ]);
        let runner = HostCliRunner::with_transport(cfg, transport)?.with_parent_env(parent_env);

        let req = sample_request("Bob: 我自己出差从不坐红眼航班。")?;
        let out = runner.extract_batch(&req).await?;
        let ExtractionBatchOutcome::Completed { atoms } = out else {
            return Err(KpError::invalid("test", "expected Completed outcome"));
        };
        assert_eq!(atoms.len(), 1);
        assert_eq!(atoms[0].kind, FactKind::Constraint);
        assert_eq!(atoms[0].value, json!({"rule": "no_red_eye"}));
        assert_eq!(atoms[0].scope, Some(kp_model::ModelScopeRef::SpeakerSelf));
        assert_eq!(atoms[0].write_op, kp_model::ModelWriteOp::Add);
        req.validate_model_atom(&atoms[0])?;

        let calls = runner.transport.recorded()?;
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        assert!(call.cwd_was_empty);
        assert!(!call.spec.env.contains_key("CLAUDECODE"));
        assert!(!call.spec.env.keys().any(|k| k.starts_with("CLAUDE_CODE_")));
        assert_eq!(
            call.spec
                .env
                .get(INTERNAL_EXTRACTION_ENV)
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(
            &call.spec.args[0..9],
            [
                "-p",
                "--bare",
                "--tools",
                "",
                "--setting-sources",
                "",
                "--output-format",
                "json",
                "--json-schema"
            ]
        );
        let schema_arg: Value = serde_json::from_str(&call.spec.args[9])
            .map_err(|_| KpError::invalid("test", "schema arg must be JSON"))?;
        assert_eq!(schema_arg, extraction_batch_json_schema());
        assert_eq!(&call.spec.args[10..12], ["--model", "haiku"]);
        Ok(())
    }

    #[tokio::test]
    async fn strict_entailment_payloads_round_trip_and_normalize_ambiguous_question() -> Result<()>
    {
        for (payload, expected) in [
            (
                json!({"verdict": "contradicts", "question": null}),
                EntailmentVerdict::Contradicts,
            ),
            (
                json!({"verdict": "unrelated", "question": null}),
                EntailmentVerdict::Unrelated,
            ),
            (
                json!({"verdict": "ambiguous", "question": "短途出差算不算出差？"}),
                EntailmentVerdict::Ambiguous {
                    question: "短途出差算不算出差？".into(),
                },
            ),
        ] {
            let body = json!({"type": "result", "structured_output": payload}).to_string();
            let runner = HostCliRunner::with_transport(
                HostCliConfig::claude_default(),
                RecordingMockTransport::new(move |_| {
                    Ok(SubprocessRawOutput {
                        exit_code: Some(0),
                        stdout: body.clone(),
                        stderr: String::new(),
                        output_file_content: None,
                    })
                }),
            )?;
            let verdict = runner
                .judge_entailment(&EntailmentQuery {
                    left_label: "l".into(),
                    left_domain: "d".into(),
                    right_label: "r".into(),
                    right_domain: "d".into(),
                    left_quote: None,
                    right_quote: None,
                })
                .await?;
            assert_eq!(verdict, expected);
        }
        // Ambiguous with null/blank question is normalized to a non-empty default question.
        let normalized =
            parse_entailment_verdict(json!({"verdict": "ambiguous", "question": "  "}))?;
        assert!(matches!(
            normalized,
            EntailmentVerdict::Ambiguous { ref question } if !question.trim().is_empty()
        ));
        assert!(parse_entailment_verdict(json!({"verdict": "maybe", "question": null})).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn codex_runner_uses_isolated_flags_empty_cwd_and_output_file() -> Result<()> {
        let transport = RecordingMockTransport::new(|spec| {
            if let Some(out_file) = &spec.output_file {
                let is_extraction = spec.stdin.contains("memory extraction engine");
                let body = if is_extraction {
                    json!({"skip_summary": true, "atoms": []}).to_string()
                } else {
                    json!({"verdict": "entails", "question": null}).to_string()
                };
                std::fs::write(out_file, body.as_bytes())
                    .map_err(|_| KpError::invalid("test", "write failed"))?;
                return Ok(SubprocessRawOutput {
                    exit_code: Some(0),
                    stdout: "{\"type\":\"turn.completed\"}\n".into(),
                    stderr: String::new(),
                    output_file_content: Some(body),
                });
            }
            Err(KpError::invalid("test", "missing output_file"))
        });

        let clean_test_env = BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]);
        let runner = HostCliRunner::with_transport(HostCliConfig::codex_default(), transport)?
            .with_parent_env(clean_test_env.clone());

        // 1) `judge_entailment` runs with default reasoning effort (`None`), omitting `model_reasoning_effort`.
        let verdict = runner
            .judge_entailment(&EntailmentQuery {
                left_label: "服务器宕机".into(),
                left_domain: "ops".into(),
                right_label: "服务不可用".into(),
                right_domain: "ops".into(),
                left_quote: Some("本周五服务器宕机检修".into()),
                right_quote: Some("遇到服务不可用的时候都切备用集群".into()),
            })
            .await?;
        assert_eq!(verdict, EntailmentVerdict::Entails);

        // 2) `extract_batch` on the SAME runner includes `-c model_reasoning_effort="low"`.
        let req = sample_request("周五下午三点核对读书会书单。")?;
        let batch_out = runner.extract_batch(&req).await?;
        assert_eq!(batch_out, ExtractionBatchOutcome::SkippedEmpty);

        let calls = runner.transport.recorded()?;
        assert_eq!(calls.len(), 2);

        let entail_call = &calls[0];
        assert!(entail_call.cwd_was_empty);
        assert!(entail_call.schema_file_existed);
        assert_eq!(
            &entail_call.spec.args[0..14],
            [
                "exec",
                "-s",
                "read-only",
                "--ephemeral",
                "--skip-git-repo-check",
                "--ignore-user-config",
                "--disable",
                "hooks",
                "--disable",
                "apps",
                "--disable",
                "plugins",
                "--disable",
                "memories"
            ]
        );
        assert!(
            !entail_call
                .spec
                .args
                .iter()
                .any(|a| a.starts_with("model_reasoning_effort=")),
            "judge_entailment must omit model_reasoning_effort=\"low\" by default"
        );
        assert_eq!(entail_call.spec.args[14], "-C");
        assert_eq!(
            entail_call.spec.args[15],
            path_to_str(&entail_call.spec.cwd)?
        );
        assert_eq!(entail_call.spec.args[16], "--output-schema");
        assert_eq!(entail_call.spec.args[18], "--json");
        assert_eq!(entail_call.spec.args[19], "-o");
        assert_eq!(entail_call.spec.args[21], "-");

        let extract_call = &calls[1];
        assert!(
            extract_call
                .spec
                .args
                .windows(2)
                .any(|w| w == ["-c", "model_reasoning_effort=\"low\""]),
            "extract_batch must include -c model_reasoning_effort=\"low\""
        );

        // 3) `KP_LIVE_REASONING_EFFORT` env var explicitly overrides `judge_entailment` when set.
        let env_with_override = BTreeMap::from([
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("KP_LIVE_REASONING_EFFORT".into(), "medium".into()),
        ]);
        let runner_med = HostCliRunner::with_transport(
            HostCliConfig::codex_default(),
            RecordingMockTransport::new(|spec| {
                if let Some(out_file) = &spec.output_file {
                    let body = json!({"verdict": "unrelated", "question": null}).to_string();
                    std::fs::write(out_file, body.as_bytes())
                        .map_err(|_| KpError::invalid("test", "write failed"))?;
                    return Ok(SubprocessRawOutput {
                        exit_code: Some(0),
                        stdout: String::new(),
                        stderr: String::new(),
                        output_file_content: Some(body),
                    });
                }
                Err(KpError::invalid("test", "missing output_file"))
            }),
        )?
        .with_parent_env(env_with_override);
        runner_med
            .judge_entailment(&EntailmentQuery {
                left_label: "a".into(),
                left_domain: "d".into(),
                right_label: "b".into(),
                right_domain: "d".into(),
                left_quote: None,
                right_quote: None,
            })
            .await?;
        let med_calls = runner_med.transport.recorded()?;
        assert!(med_calls[0]
            .spec
            .args
            .windows(2)
            .any(|w| w == ["-c", "model_reasoning_effort=\"medium\""]));
        Ok(())
    }

    #[tokio::test]
    async fn codex_selective_provider_forwarding_keeps_provider_and_drops_memories_notify_and_plugins(
    ) -> Result<()> {
        let dir = tempfile::tempdir().map_err(|_| KpError::invalid("tmp", "failed"))?;
        let config_path = dir.path().join("config.toml");
        let fake_config = r#"
# User global Codex settings — only model_provider + [model_providers.corp_gw] may pass through!
model = "gpt-5.1"
model_provider = "corp_gw"
notify = [
    "bash",
    "-lc",
    "osascript -e 'display notification \"done\"'"
]
developer_instructions = """
[model_providers.corp_gw]
env_key = "SHOULD_NOT_PARSE_INSIDE_TRIPLE_QUOTES"
"""

[memories]
enabled = true
storage_dir = "/tmp/codex-memories"

[plugins]
enabled = ["ext_repo", "ext_tracker", "ext_chat"]

[mcp_servers.internal_search]
command = "npx"
args = ["-y", "corp-mcp"]

[model_providers.corp_gw]
name = "Corporate Gateway"
base_url = "https://llm-gw.corp.example/v1"
env_key = "CORP_OPENAI_API_KEY"
wire_api = "responses"
request_max_retries = 4
query_params = { "api-version" = "2025-04-01" }

[model_providers.corp_gw.http_headers]
"X-Tenant" = "research"
"X.Trace" = "1"

[model_providers.other_unused]
base_url = "https://unused.example/v1"
env_key = "UNUSED_KEY"
"#;
        std::fs::write(&config_path, fake_config)
            .map_err(|_| KpError::invalid("test", "failed writing fake config.toml"))?;

        let mut cfg = HostCliConfig::codex_default();
        cfg.codex_config_path = Some(config_path);
        let runner = HostCliRunner::with_transport(
            cfg,
            RecordingMockTransport::new(|spec| {
                if let Some(out_file) = &spec.output_file {
                    let body = json!({"verdict": "unrelated", "question": null}).to_string();
                    std::fs::write(out_file, body.as_bytes())
                        .map_err(|_| KpError::invalid("test", "write failed"))?;
                    return Ok(SubprocessRawOutput {
                        exit_code: Some(0),
                        stdout: String::new(),
                        stderr: String::new(),
                        output_file_content: Some(body),
                    });
                }
                Err(KpError::invalid("test", "missing output_file"))
            }),
        )?
        .with_parent_env(BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]));

        runner
            .judge_entailment(&EntailmentQuery {
                left_label: "a".into(),
                left_domain: "d".into(),
                right_label: "b".into(),
                right_domain: "d".into(),
                left_quote: None,
                right_quote: None,
            })
            .await?;

        let calls = runner.transport.recorded()?;
        assert_eq!(calls.len(), 1);
        let args = &calls[0].spec.args;
        assert_eq!(
            &args[0..14],
            [
                "exec",
                "-s",
                "read-only",
                "--ephemeral",
                "--skip-git-repo-check",
                "--ignore-user-config",
                "--disable",
                "hooks",
                "--disable",
                "apps",
                "--disable",
                "plugins",
                "--disable",
                "memories"
            ]
        );

        let c_overrides: Vec<&str> = args
            .windows(2)
            .filter_map(|w| (w[0] == "-c").then_some(w[1].as_str()))
            .collect();
        assert_eq!(
            c_overrides,
            vec![
                "model_provider=\"corp_gw\"",
                "model_providers.corp_gw.name=\"Corporate Gateway\"",
                "model_providers.corp_gw.base_url=\"https://llm-gw.corp.example/v1\"",
                "model_providers.corp_gw.env_key=\"CORP_OPENAI_API_KEY\"",
                "model_providers.corp_gw.wire_api=\"responses\"",
                "model_providers.corp_gw.request_max_retries=4",
                "model_providers.corp_gw.query_params={\"api-version\"=\"2025-04-01\"}",
                "model_providers.corp_gw.http_headers={\"X-Tenant\"=\"research\", \"X.Trace\"=\"1\"}",
            ]
        );
        for forbidden in [
            "notify",
            "memories",
            "mcp_servers",
            "other_unused",
            "SHOULD_NOT_PARSE",
        ] {
            assert!(
                !c_overrides.iter().any(|ov| ov.contains(forbidden)),
                "leaked forbidden config setting in -c overrides: {forbidden}"
            );
        }

        // Dotted `provider_id` rolls up to `model_providers={"corp.gw"={...}}` so Codex's `.` LHS
        // splitter never splits inside a quoted provider ID.
        let dotted_toml = r#"
model_provider = "corp.gw"
[model_providers."corp.gw"]
name = "Dotted Corp"
base_url = "https://dotted.corp.example/v1"
[model_providers."corp.gw".http_headers]
"X.Trace" = "1"
"#;
        assert_eq!(
            extract_codex_provider_overrides(dotted_toml),
            vec![
                "model_provider=\"corp.gw\"",
                "model_providers={\"corp.gw\"={\"name\"=\"Dotted Corp\", \"base_url\"=\"https://dotted.corp.example/v1\", \"http_headers\"={\"X.Trace\"=\"1\"}}}",
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn runner_handles_raw_only_low_quota_429_and_skip_summary() -> Result<()> {
        // 1) RawOnly policy skips without spawning any subprocess.
        let zero_transport = RecordingMockTransport::new(|_| {
            Err(KpError::invalid("test", "RawOnly must never spawn CLI"))
        });
        let mut raw_cfg = HostCliConfig::claude_default();
        raw_cfg.route_policy = ModelRoutePolicy::RawOnly;
        let raw_runner = HostCliRunner::with_transport(raw_cfg, zero_transport)?;
        assert_eq!(
            raw_runner
                .extract_batch(&sample_request("周五读书会改到线上")?)
                .await?,
            ExtractionBatchOutcome::SkippedEmpty
        );
        assert!(raw_runner.transport.recorded()?.is_empty());

        // 2) Pre-flight low quota watermark skips without spawning.
        let mut low_cfg = HostCliConfig::codex_default();
        low_cfg.remaining_quota_pct = Some(8);
        let low_runner = HostCliRunner::with_transport(
            low_cfg,
            RecordingMockTransport::new(|_| Err(KpError::invalid("test", "should skip"))),
        )?;
        let low_out = low_runner
            .extract_batch(&sample_request("周五读书会改到线上")?)
            .await?;
        assert!(matches!(
            low_out,
            ExtractionBatchOutcome::SkippedLowQuota {
                remaining_pct: Some(8),
                ..
            }
        ));
        assert!(low_runner.transport.recorded()?.is_empty());

        // 3) Runtime HTTP 429 maps to SkippedLowQuota.
        let runner_429 = HostCliRunner::with_transport(
            HostCliConfig::claude_default(),
            RecordingMockTransport::new(|_| {
                Ok(SubprocessRawOutput {
                    exit_code: Some(1),
                    stdout: String::new(),
                    stderr: "Error 429: rate_limit_error Too Many Requests".into(),
                    output_file_content: None,
                })
            }),
        )?;
        assert!(matches!(
            runner_429
                .extract_batch(&sample_request("周五读书会改到线上")?)
                .await?,
            ExtractionBatchOutcome::SkippedLowQuota { .. }
        ));

        // 4) `<skip_summary/>` sentinel maps to SkippedEmpty.
        let runner_skip = HostCliRunner::with_transport(
            HostCliConfig::claude_default(),
            RecordingMockTransport::new(|_| {
                Ok(SubprocessRawOutput {
                    exit_code: Some(0),
                    stdout: SKIP_SUMMARY_SENTINEL.into(),
                    stderr: String::new(),
                    output_file_content: None,
                })
            }),
        )?;
        assert_eq!(
            runner_skip
                .extract_batch(&sample_request("普通闲聊")?)
                .await?,
            ExtractionBatchOutcome::SkippedEmpty
        );
        Ok(())
    }

    #[tokio::test]
    async fn std_subprocess_transport_executes_real_isolated_child_process() -> Result<()> {
        // Verify real OS subprocess isolation via `/bin/sh` (local POSIX shell, zero network).
        let cfg = HostCliConfig {
            kind: HostCliKind::ClaudeCode,
            program: PathBuf::from("/bin/sh"),
            model: None,
            reasoning_effort: None,
            codex_config_path: None,
            route_policy: ModelRoutePolicy::HostCli,
            timeout: Duration::from_secs(5),
            max_batch_tokens: DEFAULT_MAX_BATCH_TOKENS,
            remaining_quota_pct: None,
        };
        let env = sanitize_child_env([
            ("CLAUDECODE", "1"),
            ("CLAUDE_CODE_SESSION", "abc"),
            ("PATH", "/usr/bin:/bin"),
        ]);
        let empty = tempfile::tempdir().map_err(|_| KpError::invalid("tmp", "failed"))?;
        let spec = SubprocessSpec {
            program: cfg.program,
            args: vec![
                "-c".into(),
                "printf '%s|%s|%s' \"$KP_INTERNAL_EXTRACTION\" \"$CLAUDECODE\" \"$(ls -A | wc -l | tr -d ' ')\""
                    .into(),
            ],
            env,
            cwd: empty.path().to_path_buf(),
            stdin: String::new(),
            output_file: None,
            timeout: cfg.timeout,
        };
        let raw = StdSubprocessTransport.execute(&spec)?;
        assert_eq!(raw.exit_code, Some(0));
        assert_eq!(raw.stdout, "1||0");
        Ok(())
    }

    #[test]
    fn std_subprocess_transport_drains_large_stdout_and_stderr_without_pipe_deadlock() -> Result<()>
    {
        // Stream ~200KB on stdout AND ~200KB on stderr while reading 64KB from stdin — well past
        // 16KB–64KB OS pipe capacities that previously deadlocked `try_wait()` before `wait_with_output()`.
        let empty = tempfile::tempdir().map_err(|_| KpError::invalid("tmp", "failed"))?;
        let env = sanitize_child_env([("PATH", "/usr/bin:/bin")]);
        let stdin_payload = "x".repeat(65_536);
        let script = concat!(
            "in_bytes=$(wc -c | tr -d ' '); ",
            "chunk='0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef'; ",
            "i=0; while [ $i -lt 3200 ]; do printf '%s' \"$chunk\"; printf '%s' \"$chunk\" >&2; i=$((i+1)); done; ",
            "printf '|in=%s' \"$in_bytes\""
        );
        let spec = SubprocessSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), script.into()],
            env,
            cwd: empty.path().to_path_buf(),
            stdin: stdin_payload,
            output_file: None,
            timeout: Duration::from_secs(5),
        };
        let raw = StdSubprocessTransport.execute(&spec)?;
        assert_eq!(raw.exit_code, Some(0));
        assert!(raw.stdout.len() > 200_000);
        assert!(raw.stdout.ends_with("|in=65536"));
        assert_eq!(raw.stderr.len(), 204_800);
        Ok(())
    }
}
