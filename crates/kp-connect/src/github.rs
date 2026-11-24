//! Read-only GitHub connector (`ReqwestGitHubTransport` + `GhCliFallbackTransport`).
//!
//! Combines:
//! - Conditional `ETag` (`If-None-Match` -> `304 Not Modified` returning `Ok(None)`).
//! - RAGFlow (`internal/syncer/connector/github.go`, Apache-2.0) `since` window + resumable per-stream
//!   pagination checkpoint (`issues`, `issue_comments`, `pull_comments`, `releases`).
//! - Onyx (`connectors/github/rate_limit_utils.py`, MIT outside `ee/`) `X-RateLimit-Remaining` and
//!   `X-RateLimit-Reset` backoff helper.
//! - Self-written repository visibility & collaborator -> `SourceScopeRoster` mapping (public repos
//!   grant `all_library_members: true`; private/internal repos bind explicit collaborator logins).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use kp_model::{
    format_epoch_ms_rfc3339_utc, parse_timestamp_ms_in_tz, ActorId, BoxFuture, ContentHash,
    EpochMs, SecretRedactor, TimeZone,
};
use reqwest::header::{HeaderMap, ACCEPT, AUTHORIZATION, ETAG, IF_NONE_MATCH, USER_AGENT};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::subprocess::{CommandOutput, CommandRunner, Sleeper, StdCommandRunner, TokioSleeper};
use crate::{
    is_low_signal_ack, ConnectError, Connector, ConnectorCursor, ConnectorKind, ConnectorScope,
    ExternalIdentityProposal, GitHubConditionalPage, GitHubTransport, IngestedSourceEvent,
    PollBatch, Result, SourceEventOp, SourceScopeRoster,
};

/// Default initial lookback window for GitHub issues/PRs/comments/releases (30 days in ms).
pub const DEFAULT_GITHUB_INITIAL_BACKFILL_MS: i64 = 30 * 86_400 * 1_000;

/// Environment variable overriding the GitHub REST API base URL (default `https://api.github.com`),
/// e.g. for enterprise GitHub instances or local `http://127.0.0.1:<port>` read-only mock servers.
pub const ENV_GITHUB_API_BASE: &str = "KP_GITHUB_API_BASE";

/// Environment variable overriding the `gh` CLI executable path.
pub const ENV_GH_CLI_BIN: &str = "KP_GH_CLI_BIN";

/// Validates and normalizes a GitHub REST API base URL (`https://...` or `http://...` with a valid host,
/// stripping trailing `/`). Rejects non-HTTP(S) schemes, query/fragment injection, credentials, or whitespace.
pub fn validate_github_api_base_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    let after_scheme = if let Some(rest) = trimmed.strip_prefix("https://") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        rest
    } else {
        return Err(ConnectError::ReadOnlyViolation(format!(
            "GitHub API base URL `{raw}` must start with `https://` or `http://`"
        )));
    };
    if after_scheme.is_empty()
        || after_scheme.starts_with('/')
        || after_scheme.contains('@')
        || after_scheme.contains('?')
        || after_scheme.contains('#')
        || trimmed.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(ConnectError::ReadOnlyViolation(format!(
            "invalid GitHub API base URL `{raw}`"
        )));
    }
    let host_port = after_scheme.split('/').next().unwrap_or("");
    if host_port.is_empty() {
        return Err(ConnectError::ReadOnlyViolation(format!(
            "GitHub API base URL `{raw}` is missing a host"
        )));
    }
    Ok(trimmed.to_owned())
}

/// Resolves the `gh` CLI executable path from `KP_GH_CLI_BIN` or `"gh"`.
pub fn resolve_gh_cli_binary() -> String {
    if let Some(explicit) = std::env::var_os(ENV_GH_CLI_BIN) {
        let s = explicit.to_string_lossy();
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    "gh".into()
}

const ALLOWED_REPO_SUBPATHS: &[&str] = &[
    "collaborators",
    "issues",
    "issues/comments",
    "pulls",
    "pulls/comments",
    "releases",
    "commits",
];

const FORBIDDEN_GH_FLAGS: &[&str] = &[
    "-f",
    "--raw-field",
    "-F",
    "--field",
    "--input",
    "POST",
    "PUT",
    "PATCH",
    "DELETE",
];

/// Validates that a GitHub REST v3 endpoint is strictly a read-only `/repos/{owner}/{repo}...` path.
pub fn validate_github_readonly_endpoint(endpoint: &str) -> Result<()> {
    let trimmed = endpoint.trim();
    if !trimmed.starts_with("/repos/")
        || trimmed.contains("..")
        || trimmed.contains('\r')
        || trimmed.contains('\n')
        || trimmed.contains(' ')
    {
        return Err(ConnectError::ReadOnlyViolation(format!(
            "GitHub endpoint `{endpoint}` must be a clean `/repos/{{owner}}/{{repo}}` path"
        )));
    }
    let path_only = trimmed.split('?').next().unwrap_or(trimmed);
    let rest = path_only.strip_prefix("/repos/").unwrap_or("");
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 2 {
        return Err(ConnectError::ReadOnlyViolation(format!(
            "GitHub endpoint `{endpoint}` is missing owner or repo"
        )));
    }
    for seg in &segments[0..2] {
        if !seg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        {
            return Err(ConnectError::ReadOnlyViolation(format!(
                "invalid owner/repo segment `{seg}` in `{endpoint}`"
            )));
        }
    }
    if segments.len() > 2 {
        let subpath = segments[2..].join("/");
        if !ALLOWED_REPO_SUBPATHS.contains(&subpath.as_str()) {
            return Err(ConnectError::ReadOnlyViolation(format!(
                "GitHub subpath `{subpath}` is outside the read-only connector allowlist"
            )));
        }
    }
    Ok(())
}

const ALLOWED_GH_API_VALUE_FLAGS: &[&str] = &[
    "--method",
    "-X",
    "-H",
    "--header",
    "--hostname",
    "--cache",
    "-q",
    "--jq",
    "-p",
    "--preview",
];
const ALLOWED_GH_API_BOOL_FLAGS: &[&str] = &["--include", "-i", "--paginate", "--slurp"];

/// Validates that a `gh api` invocation is strictly read-only (`GET` only, real `gh api` flags only).
pub fn validate_gh_cli_readonly_args(args: &[String]) -> Result<()> {
    if args.len() < 5
        || args.first().map(String::as_str) != Some("api")
        || args.get(1).map(String::as_str) != Some("--method")
        || args.get(2).map(String::as_str) != Some("GET")
        || args.get(3).map(String::as_str) != Some("--include")
    {
        return Err(ConnectError::ReadOnlyViolation(format!(
            "gh invocation `{}` must start with `api --method GET --include`",
            args.join(" ")
        )));
    }
    for arg in args {
        if FORBIDDEN_GH_FLAGS.contains(&arg.as_str())
            || arg.starts_with("-f=")
            || arg.starts_with("-F=")
            || arg.starts_with("--field=")
            || arg.starts_with("--raw-field=")
            || arg.starts_with("--input=")
        {
            return Err(ConnectError::ReadOnlyViolation(format!(
                "forbidden mutating flag `{arg}` in gh invocation"
            )));
        }
    }
    let Some((endpoint, flag_slice)) = args[1..].split_last() else {
        return Err(ConnectError::ReadOnlyViolation(
            "missing GitHub endpoint".into(),
        ));
    };
    let mut idx = 0_usize;
    while idx < flag_slice.len() {
        let tok = flag_slice[idx].as_str();
        if ALLOWED_GH_API_BOOL_FLAGS.contains(&tok) {
            idx += 1;
            continue;
        }
        if ALLOWED_GH_API_VALUE_FLAGS.contains(&tok) {
            let Some(val) = flag_slice.get(idx + 1) else {
                return Err(ConnectError::ReadOnlyViolation(format!(
                    "missing value after `{tok}` in gh api invocation"
                )));
            };
            if (tok == "--method" || tok == "-X") && val != "GET" {
                return Err(ConnectError::ReadOnlyViolation(format!(
                    "non-GET method `{val}` rejected in gh api invocation"
                )));
            }
            idx += 2;
            continue;
        }
        return Err(ConnectError::ReadOnlyViolation(format!(
            "unsupported or forbidden flag `{tok}` in gh api invocation"
        )));
    }
    validate_github_readonly_endpoint(endpoint)
}

/// Rate-limit backoff helper ported from Onyx `connectors/github/rate_limit_utils.py` (MIT outside `ee/`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GitHubRateLimitPolicy {
    /// Pause proactively when `X-RateLimit-Remaining` drops to or below `low_watermark`.
    pub low_watermark: u32,
    /// Extra safety buffer added past `X-RateLimit-Reset`.
    pub reset_cushion_sec: u64,
    /// Fallback backoff when GitHub omits `X-RateLimit-Reset`.
    pub default_backoff_ms: u64,
    /// Hard cap on any single backoff wait.
    pub max_backoff_ms: u64,
    /// Maximum retries on HTTP 403/429 rate-limit responses per request.
    pub max_retries: u32,
}

impl Default for GitHubRateLimitPolicy {
    fn default() -> Self {
        Self {
            low_watermark: 25,
            reset_cushion_sec: 2,
            default_backoff_ms: 5_000,
            max_backoff_ms: 60_000,
            max_retries: 2,
        }
    }
}

impl GitHubRateLimitPolicy {
    /// Computes milliseconds to sleep when rate-limited or below `low_watermark` at `now_epoch_sec`.
    pub fn compute_wait_ms(
        &self,
        remaining: Option<u32>,
        reset_epoch_sec: Option<i64>,
        retry_after_sec: Option<u64>,
        now_epoch_sec: i64,
        force_rate_limited: bool,
    ) -> Option<u64> {
        let should_wait =
            force_rate_limited || remaining.is_some_and(|rem| rem <= self.low_watermark);
        if !should_wait {
            return None;
        }
        if let Some(ra) = retry_after_sec.filter(|s| *s > 0) {
            return Some(ra.saturating_mul(1_000).min(self.max_backoff_ms));
        }
        if let Some(reset_sec) = reset_epoch_sec {
            let delta_sec = (reset_sec - now_epoch_sec).max(0) as u64;
            let total_ms = delta_sec
                .saturating_add(self.reset_cushion_sec)
                .saturating_mul(1_000);
            return Some(total_ms.max(1_000).min(self.max_backoff_ms));
        }
        Some(self.default_backoff_ms.min(self.max_backoff_ms))
    }
}

/// Primary read-only GitHub REST transport backed by `reqwest`.
pub struct ReqwestGitHubTransport {
    client: reqwest::Client,
    api_base_url: String,
    token: Option<String>,
    rate_limit: GitHubRateLimitPolicy,
    now_epoch_sec_fn: Arc<dyn Fn() -> i64 + Send + Sync>,
    sleeper: Arc<dyn Sleeper>,
}

impl ReqwestGitHubTransport {
    pub fn new(token: Option<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| ConnectError::Http(e.to_string()))?;
        let api_base_url = match std::env::var(ENV_GITHUB_API_BASE) {
            Ok(raw) if !raw.trim().is_empty() => validate_github_api_base_url(&raw)?,
            _ => "https://api.github.com".into(),
        };
        let resolved_token = token.or_else(|| {
            std::env::var("GH_TOKEN")
                .ok()
                .or_else(|| std::env::var("GITHUB_TOKEN").ok())
                .filter(|s| !s.trim().is_empty())
        });
        Ok(Self {
            client,
            api_base_url,
            token: resolved_token,
            rate_limit: GitHubRateLimitPolicy::default(),
            now_epoch_sec_fn: Arc::new(current_unix_sec),
            sleeper: Arc::new(TokioSleeper),
        })
    }

    pub fn with_api_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.api_base_url = base_url.into().trim_end_matches('/').to_owned();
        self
    }

    pub fn api_base_url(&self) -> &str {
        &self.api_base_url
    }

    pub fn with_rate_limit_and_sleeper(
        mut self,
        policy: GitHubRateLimitPolicy,
        now_epoch_sec_fn: Arc<dyn Fn() -> i64 + Send + Sync>,
        sleeper: Arc<dyn Sleeper>,
    ) -> Self {
        self.rate_limit = policy;
        self.now_epoch_sec_fn = now_epoch_sec_fn;
        self.sleeper = sleeper;
        self
    }

    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }
}

impl GitHubTransport for ReqwestGitHubTransport {
    fn get_json<'a>(
        &'a self,
        endpoint: &'a str,
        etag: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Option<GitHubConditionalPage>>> {
        Box::pin(async move {
            validate_github_readonly_endpoint(endpoint)?;
            let base = validate_github_api_base_url(&self.api_base_url)?;
            let url = format!("{base}{endpoint}");
            let mut attempt = 0_u32;

            loop {
                let mut req = self
                    .client
                    .get(&url)
                    .header(ACCEPT, "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", "2022-11-28")
                    .header(USER_AGENT, "kipo-kp-connect/0.1");
                if let Some(tok) = &self.token {
                    req = req.header(AUTHORIZATION, format!("Bearer {tok}"));
                }
                if let Some(tag) = etag.filter(|s| !s.trim().is_empty()) {
                    req = req.header(IF_NONE_MATCH, tag);
                }

                let resp = req
                    .send()
                    .await
                    .map_err(|e| ConnectError::Transport(format!("GitHub HTTP GET failed: {e}")))?;
                let status = resp.status();
                let headers = resp.headers().clone();
                let (resp_etag, rem, reset_sec, retry_after_sec) =
                    extract_reqwest_headers(&headers);
                let now_sec = (self.now_epoch_sec_fn)();

                if status == StatusCode::NOT_MODIFIED {
                    return Ok(None);
                }

                if status == StatusCode::TOO_MANY_REQUESTS
                    || (status == StatusCode::FORBIDDEN
                        && (rem == Some(0) || retry_after_sec.is_some()))
                {
                    let wait_ms = self
                        .rate_limit
                        .compute_wait_ms(rem, reset_sec, retry_after_sec, now_sec, true)
                        .unwrap_or(self.rate_limit.default_backoff_ms);
                    if attempt >= self.rate_limit.max_retries {
                        return Err(ConnectError::RateLimited {
                            retry_after_ms: wait_ms,
                            reason: format!("GitHub rate limit hit on `{endpoint}` ({status})"),
                        });
                    }
                    self.sleeper.sleep_ms(wait_ms).await;
                    attempt += 1;
                    continue;
                }

                if !status.is_success() {
                    let body_text = resp.text().await.unwrap_or_default();
                    return Err(ConnectError::Transport(format!(
                        "GitHub GET `{endpoint}` failed with {status}: {}",
                        body_text.trim()
                    )));
                }

                let body: Value = resp
                    .json()
                    .await
                    .map_err(|e| ConnectError::Http(format!("invalid GitHub JSON: {e}")))?;

                if let Some(wait_ms) = self
                    .rate_limit
                    .compute_wait_ms(rem, reset_sec, None, now_sec, false)
                {
                    self.sleeper.sleep_ms(wait_ms).await;
                }

                return Ok(Some(GitHubConditionalPage {
                    body,
                    etag: resp_etag,
                    rate_limit_remaining: rem,
                    rate_limit_reset_epoch_sec: reset_sec,
                }));
            }
        })
    }
}

fn extract_reqwest_headers(
    headers: &HeaderMap,
) -> (Option<String>, Option<u32>, Option<i64>, Option<u64>) {
    let etag = headers
        .get(ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let remaining = headers
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u32>().ok());
    let reset = headers
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok());
    let retry_after = headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    (etag, remaining, reset, retry_after)
}

/// Fallback read-only GitHub transport shelling out to authenticated `gh api --method GET --include`.
pub struct GhCliFallbackTransport {
    binary: String,
    hostname: Option<String>,
    rate_limit: GitHubRateLimitPolicy,
    now_epoch_sec_fn: Arc<dyn Fn() -> i64 + Send + Sync>,
    runner: Arc<dyn CommandRunner>,
    sleeper: Arc<dyn Sleeper>,
}

impl Default for GhCliFallbackTransport {
    fn default() -> Self {
        Self::new(resolve_gh_cli_binary())
    }
}

impl GhCliFallbackTransport {
    pub fn new(binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
            hostname: None,
            rate_limit: GitHubRateLimitPolicy::default(),
            now_epoch_sec_fn: Arc::new(current_unix_sec),
            runner: Arc::new(StdCommandRunner),
            sleeper: Arc::new(TokioSleeper),
        }
    }

    pub fn with_hostname(mut self, hostname: impl Into<String>) -> Self {
        self.hostname = Some(hostname.into());
        self
    }

    pub fn with_runner_and_sleeper(
        mut self,
        rate_limit: GitHubRateLimitPolicy,
        now_epoch_sec_fn: Arc<dyn Fn() -> i64 + Send + Sync>,
        runner: Arc<dyn CommandRunner>,
        sleeper: Arc<dyn Sleeper>,
    ) -> Self {
        self.rate_limit = rate_limit;
        self.now_epoch_sec_fn = now_epoch_sec_fn;
        self.runner = runner;
        self.sleeper = sleeper;
        self
    }

    fn build_args(&self, endpoint: &str, etag: Option<&str>) -> Result<Vec<String>> {
        validate_github_readonly_endpoint(endpoint)?;
        let mut args = vec![
            "api".into(),
            "--method".into(),
            "GET".into(),
            "--include".into(),
            "-H".into(),
            "Accept:application/vnd.github+json".into(),
            "-H".into(),
            "X-GitHub-Api-Version:2022-11-28".into(),
        ];
        if let Some(host) = &self.hostname {
            if host.trim().is_empty() || host.starts_with('-') {
                return Err(ConnectError::ReadOnlyViolation(format!(
                    "invalid gh hostname `{host}`"
                )));
            }
            args.push("--hostname".into());
            args.push(host.clone());
        }
        if let Some(tag) = etag.filter(|s| !s.trim().is_empty()) {
            if tag.contains('\r') || tag.contains('\n') {
                return Err(ConnectError::ReadOnlyViolation(
                    "invalid ETag header characters".into(),
                ));
            }
            args.push("-H".into());
            args.push(format!("If-None-Match:{tag}"));
        }
        args.push(endpoint.to_owned());
        validate_gh_cli_readonly_args(&args)?;
        Ok(args)
    }
}

impl GitHubTransport for GhCliFallbackTransport {
    fn get_json<'a>(
        &'a self,
        endpoint: &'a str,
        etag: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Option<GitHubConditionalPage>>> {
        Box::pin(async move {
            let args = self.build_args(endpoint, etag)?;
            let mut attempt = 0_u32;

            loop {
                let output = self.runner.run(&self.binary, &args).await?;
                let parsed = parse_gh_include_output(&output)?;
                let now_sec = (self.now_epoch_sec_fn)();

                if parsed.http_status == 304 {
                    return Ok(None);
                }

                let is_rate_limit = parsed.http_status == 429
                    || (parsed.http_status == 403
                        && (parsed.rate_limit_remaining == Some(0)
                            || parsed.retry_after_sec.is_some()));
                if is_rate_limit {
                    let wait_ms = self
                        .rate_limit
                        .compute_wait_ms(
                            parsed.rate_limit_remaining,
                            parsed.rate_limit_reset_epoch_sec,
                            parsed.retry_after_sec,
                            now_sec,
                            true,
                        )
                        .unwrap_or(self.rate_limit.default_backoff_ms);
                    if attempt >= self.rate_limit.max_retries {
                        return Err(ConnectError::RateLimited {
                            retry_after_ms: wait_ms,
                            reason: format!(
                                "gh api rate limit hit on `{endpoint}` (HTTP {})",
                                parsed.http_status
                            ),
                        });
                    }
                    self.sleeper.sleep_ms(wait_ms).await;
                    attempt += 1;
                    continue;
                }

                if !(200..300).contains(&parsed.http_status) || output.status_code != 0 {
                    let err_detail = String::from_utf8_lossy(&output.stderr);
                    return Err(ConnectError::Transport(format!(
                        "gh api GET `{endpoint}` failed (HTTP {}, exit {}): {}",
                        parsed.http_status,
                        output.status_code,
                        err_detail.trim()
                    )));
                }

                let body: Value = serde_json::from_str(&parsed.body_text)?;
                if let Some(wait_ms) = self.rate_limit.compute_wait_ms(
                    parsed.rate_limit_remaining,
                    parsed.rate_limit_reset_epoch_sec,
                    None,
                    now_sec,
                    false,
                ) {
                    self.sleeper.sleep_ms(wait_ms).await;
                }

                return Ok(Some(GitHubConditionalPage {
                    body,
                    etag: parsed.etag,
                    rate_limit_remaining: parsed.rate_limit_remaining,
                    rate_limit_reset_epoch_sec: parsed.rate_limit_reset_epoch_sec,
                }));
            }
        })
    }
}

struct ParsedGhHttpResponse {
    http_status: u16,
    etag: Option<String>,
    rate_limit_remaining: Option<u32>,
    rate_limit_reset_epoch_sec: Option<i64>,
    retry_after_sec: Option<u64>,
    body_text: String,
}

fn parse_gh_include_output(output: &CommandOutput) -> Result<ParsedGhHttpResponse> {
    let raw = String::from_utf8_lossy(&output.stdout);
    let trimmed = raw.trim_start();
    if !trimmed.starts_with("HTTP/") {
        if output.status_code != 0 {
            return Err(ConnectError::Transport(format!(
                "gh api failed (exit {}): {}",
                output.status_code,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        return Ok(ParsedGhHttpResponse {
            http_status: 200,
            etag: None,
            rate_limit_remaining: None,
            rate_limit_reset_epoch_sec: None,
            retry_after_sec: None,
            body_text: trimmed.to_owned(),
        });
    }

    let (header_block, body_str) = if let Some((h, b)) = trimmed.split_once("\r\n\r\n") {
        (h, b)
    } else if let Some((h, b)) = trimmed.split_once("\n\n") {
        (h, b)
    } else {
        (trimmed, "")
    };

    let mut lines = header_block.lines();
    let status_line = lines.next().unwrap_or("");
    let http_status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(500);

    let mut etag = None;
    let mut remaining = None;
    let mut reset = None;
    let mut retry_after = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim();
            match key.as_str() {
                "etag" => etag = Some(val.to_owned()),
                "x-ratelimit-remaining" => remaining = val.parse::<u32>().ok(),
                "x-ratelimit-reset" => reset = val.parse::<i64>().ok(),
                "retry-after" => retry_after = val.parse::<u64>().ok(),
                _ => {}
            }
        }
    }

    Ok(ParsedGhHttpResponse {
        http_status,
        etag,
        rate_limit_remaining: remaining,
        rate_limit_reset_epoch_sec: reset,
        retry_after_sec: retry_after,
        body_text: body_str.trim().to_owned(),
    })
}

/// Composite read-only GitHub transport chaining primary (`ReqwestGitHubTransport`) with fallback (`GhCliFallbackTransport`).
pub struct GitHubTransportChain<P: GitHubTransport, F: GitHubTransport> {
    primary: P,
    fallback: F,
    prefer_fallback: bool,
}

impl<P: GitHubTransport, F: GitHubTransport> GitHubTransportChain<P, F> {
    pub fn new(primary: P, fallback: F, prefer_fallback: bool) -> Self {
        Self {
            primary,
            fallback,
            prefer_fallback,
        }
    }
}

impl<P: GitHubTransport, F: GitHubTransport> GitHubTransport for GitHubTransportChain<P, F> {
    fn get_json<'a>(
        &'a self,
        endpoint: &'a str,
        etag: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Option<GitHubConditionalPage>>> {
        Box::pin(async move {
            validate_github_readonly_endpoint(endpoint)?;
            if self.prefer_fallback {
                return self.fallback.get_json(endpoint, etag).await;
            }
            match self.primary.get_json(endpoint, etag).await {
                Ok(page) => Ok(page),
                Err(ConnectError::Transport(_)) | Err(ConnectError::Http(_)) => {
                    self.fallback.get_json(endpoint, etag).await
                }
                Err(other) => Err(other),
            }
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubStreamCheckpoint {
    pub since_ms: Option<i64>,
    pub etag: Option<String>,
    pub pending_page: Option<u32>,
    pub pending_since_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubItemState {
    pub active_key: String,
    pub content_sha256: String,
    pub updated_at_ms: i64,
    pub revision: u32,
}

/// Resumable RAGFlow-style GitHub checkpoint persisted inside `ConnectorCursor.cursor_token`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubCheckpoint {
    pub high_water_ms: Option<i64>,
    pub repo_etag: Option<String>,
    pub repo_is_public: Option<bool>,
    pub collab_etag: Option<String>,
    #[serde(default)]
    pub cached_collaborators: Vec<String>,
    #[serde(default)]
    pub streams: BTreeMap<String, GitHubStreamCheckpoint>,
    #[serde(default)]
    pub known_items: BTreeMap<String, GitHubItemState>,
}

impl GitHubCheckpoint {
    pub fn from_cursor(cursor: &ConnectorCursor) -> Result<Self> {
        let mut cp = match &cursor.cursor_token {
            Some(raw) if !raw.trim().is_empty() => serde_json::from_str::<Self>(raw)?,
            _ => Self::default(),
        };
        if cp.high_water_ms.is_none() {
            cp.high_water_ms = cursor.high_water_ms.map(|ms| ms.0);
        }
        Ok(cp)
    }

    pub fn into_cursor(&self, now: EpochMs) -> Result<ConnectorCursor> {
        Ok(ConnectorCursor {
            cursor_token: Some(serde_json::to_string(self)?),
            high_water_ms: self.high_water_ms.map(EpochMs),
            updated_at: now,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GitHubSyncConfig {
    pub now: EpochMs,
    pub initial_backfill_ms: i64,
    pub per_page: u32,
    pub max_pages_per_stream: u32,
}

impl Default for GitHubSyncConfig {
    fn default() -> Self {
        Self {
            now: EpochMs(1_795_245_380_000),
            initial_backfill_ms: DEFAULT_GITHUB_INITIAL_BACKFILL_MS,
            per_page: 50,
            max_pages_per_stream: 10,
        }
    }
}

/// Read-only GitHub incremental connector implementing `Connector`.
pub struct GitHubConnector<T: GitHubTransport> {
    transport: T,
    config: GitHubSyncConfig,
}

impl<T: GitHubTransport> GitHubConnector<T> {
    pub fn new(transport: T, config: GitHubSyncConfig) -> Self {
        Self { transport, config }
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    async fn sync_repo_scope(
        &self,
        owner: &str,
        repo: &str,
        scope: &ConnectorScope,
        checkpoint: &mut GitHubCheckpoint,
        proposals: &mut BTreeMap<String, ExternalIdentityProposal>,
    ) -> Result<SourceScopeRoster> {
        let repo_endpoint = format!("/repos/{owner}/{repo}");
        let is_public = if let Some(page) = self
            .transport
            .get_json(&repo_endpoint, checkpoint.repo_etag.as_deref())
            .await?
        {
            checkpoint.repo_etag = page.etag;
            if let Some(owner_login) = page
                .body
                .pointer("/owner/login")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                record_gh_identity(proposals, owner_login);
            }
            let private = page
                .body
                .get("private")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let visibility = page
                .body
                .get("visibility")
                .and_then(Value::as_str)
                .unwrap_or(if private { "private" } else { "public" });
            let pub_flag = !private && visibility == "public";
            checkpoint.repo_is_public = Some(pub_flag);
            pub_flag
        } else {
            checkpoint.repo_is_public.unwrap_or(false)
        };

        if is_public {
            checkpoint.cached_collaborators.clear();
            return Ok(SourceScopeRoster {
                scope_id: scope.source_scope_id.clone(),
                provider: "github".into(),
                external_container_id: format!("{owner}/{repo}"),
                label: scope.label.clone(),
                external_member_ids: Vec::new(),
                all_library_members: true,
                synced_at: self.config.now,
            });
        }

        // Private or internal repository: sync live collaborator logins with pull permission.
        let mut members = BTreeSet::<String>::new();
        let mut page_num = 1_u32;
        let mut used_cached = false;

        while page_num <= self.config.max_pages_per_stream.max(1) {
            let endpoint =
                format!("/repos/{owner}/{repo}/collaborators?per_page=100&page={page_num}");
            let etag = if page_num == 1 {
                checkpoint.collab_etag.as_deref()
            } else {
                None
            };
            let Some(resp) = self.transport.get_json(&endpoint, etag).await? else {
                used_cached = true;
                break;
            };
            if page_num == 1 {
                checkpoint.collab_etag = resp.etag;
            }
            let Some(items) = resp.body.as_array() else {
                break;
            };
            let count = items.len();
            for item in items {
                let can_pull = item
                    .pointer("/permissions/pull")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                if !can_pull {
                    continue;
                }
                if let Some(login) = item
                    .get("login")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    members.insert(login.to_owned());
                    record_gh_identity(proposals, login);
                }
            }
            if count < 100 {
                break;
            }
            page_num += 1;
        }

        let member_list: Vec<String> = if used_cached {
            checkpoint.cached_collaborators.clone()
        } else {
            let v: Vec<String> = members.into_iter().collect();
            checkpoint.cached_collaborators = v.clone();
            v
        };

        Ok(SourceScopeRoster {
            scope_id: scope.source_scope_id.clone(),
            provider: "github".into(),
            external_container_id: format!("{owner}/{repo}"),
            label: scope.label.clone(),
            external_member_ids: member_list,
            all_library_members: false,
            synced_at: self.config.now,
        })
    }

    async fn sync_stream(&self, stream_name: &str, ctx: &mut GitHubSyncCtx<'_>) -> Result<()> {
        let default_since_ms = (self.config.now.0 - self.config.initial_backfill_ms).max(0);
        let mut stream_cp = ctx
            .checkpoint
            .streams
            .get(stream_name)
            .cloned()
            .unwrap_or_default();

        let window_since_ms = stream_cp
            .pending_since_ms
            .or(stream_cp.since_ms)
            .unwrap_or(default_since_ms);
        let since_iso = format_epoch_ms_rfc3339_utc(EpochMs(window_since_ms));
        let mut page = stream_cp.pending_page.unwrap_or(1);
        let per_page = self.config.per_page.clamp(1, 100);
        let max_pages = self.config.max_pages_per_stream.max(1);
        let mut pages_run = 0_u32;
        let mut max_seen_ms = stream_cp.since_ms.unwrap_or(window_since_ms);
        let mut stream_complete = true;
        let owner = ctx.owner;
        let repo = ctx.repo;

        while pages_run < max_pages {
            pages_run += 1;
            let endpoint = match stream_name {
                "issues" => format!(
                    "/repos/{owner}/{repo}/issues?state=all&sort=updated&direction=asc&since={since_iso}&per_page={per_page}&page={page}"
                ),
                "issue_comments" => format!(
                    "/repos/{owner}/{repo}/issues/comments?sort=updated&direction=asc&since={since_iso}&per_page={per_page}&page={page}"
                ),
                "pull_comments" => format!(
                    "/repos/{owner}/{repo}/pulls/comments?sort=updated&direction=asc&since={since_iso}&per_page={per_page}&page={page}"
                ),
                "releases" => format!(
                    "/repos/{owner}/{repo}/releases?per_page={per_page}&page={page}"
                ),
                _ => break,
            };

            let req_etag = if page == 1 && stream_cp.pending_page.is_none() {
                stream_cp.etag.as_deref()
            } else {
                None
            };

            let page_resp = match self.transport.get_json(&endpoint, req_etag).await {
                Ok(Some(resp)) => resp,
                Ok(None) => {
                    // HTTP 304 Not Modified on conditional ETag check.
                    break;
                }
                Err(ConnectError::RateLimited {
                    retry_after_ms,
                    reason,
                }) => {
                    // Save mid-stream resumable page cursor before surfacing rate-limit error.
                    stream_cp.pending_page = Some(page);
                    stream_cp.pending_since_ms = Some(window_since_ms);
                    ctx.checkpoint
                        .streams
                        .insert(stream_name.to_owned(), stream_cp);
                    return Err(ConnectError::RateLimited {
                        retry_after_ms,
                        reason,
                    });
                }
                Err(err) => return Err(err),
            };

            if page == 1 {
                stream_cp.etag = page_resp.etag;
            }

            let Some(items) = page_resp.body.as_array() else {
                break;
            };
            let item_count = items.len();

            for item in items {
                if let Some(updated_ms) =
                    self.ingest_github_item(stream_name, window_since_ms, item, ctx)
                {
                    max_seen_ms = max_seen_ms.max(updated_ms.0);
                }
            }

            if item_count < per_page as usize {
                break;
            }
            page += 1;
            if pages_run >= max_pages {
                stream_complete = false;
                break;
            }
        }

        if stream_complete {
            stream_cp.pending_page = None;
            stream_cp.pending_since_ms = None;
            stream_cp.since_ms = Some(max_seen_ms);
            ctx.checkpoint.high_water_ms = Some(
                ctx.checkpoint
                    .high_water_ms
                    .unwrap_or(max_seen_ms)
                    .max(max_seen_ms),
            );
        } else {
            stream_cp.pending_page = Some(page);
            stream_cp.pending_since_ms = Some(window_since_ms);
        }
        ctx.checkpoint
            .streams
            .insert(stream_name.to_owned(), stream_cp);
        Ok(())
    }

    fn ingest_github_item(
        &self,
        stream_name: &str,
        window_since_ms: i64,
        item: &Value,
        ctx: &mut GitHubSyncCtx<'_>,
    ) -> Option<EpochMs> {
        let GitHubSyncCtx {
            owner,
            repo,
            scope,
            checkpoint,
            ingested_by,
            redactor,
            proposals,
            batch,
        } = ctx;
        let owner = *owner;
        let repo = *repo;
        let scope = *scope;
        let ingested_by = *ingested_by;
        let redactor = *redactor;
        let updated_ms = item
            .get("updated_at")
            .or_else(|| item.get("published_at"))
            .or_else(|| item.get("created_at"))
            .and_then(Value::as_str)
            .and_then(|s| parse_timestamp_ms_in_tz(s, &TimeZone::UTC))
            .unwrap_or(self.config.now);

        if stream_name == "releases" && updated_ms.0 < window_since_ms {
            return None;
        }

        let author = item
            .pointer("/user/login")
            .or_else(|| item.pointer("/author/login"))
            .and_then(Value::as_str)
            .unwrap_or("github-user");
        record_gh_identity(proposals, author);

        let (logical_item_key, uri, raw_text, thread_key, reply_to) = match stream_name {
            "issues" => {
                let number = item.get("number").and_then(Value::as_u64)?;
                let is_pr = item.get("pull_request").is_some();
                let kind_tag = if is_pr { "pr" } else { "issue" };
                let title = item.get("title").and_then(Value::as_str).unwrap_or("");
                let body = item.get("body").and_then(Value::as_str).unwrap_or("");
                let html_url = item
                    .get("html_url")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        format!("https://github.com/{owner}/{repo}/{kind_tag}/{number}")
                    });
                let combined = if body.trim().is_empty() {
                    format!("#{number} {title}")
                } else {
                    format!("#{number} {title}\n\n{body}")
                };
                (
                    format!("gh:{owner}/{repo}:{kind_tag}:{number}"),
                    html_url,
                    combined,
                    Some(format!("{owner}/{repo}#{number}")),
                    None,
                )
            }
            "issue_comments" | "pull_comments" => {
                let id = item.get("id").and_then(Value::as_u64)?;
                let body = item.get("body").and_then(Value::as_str).unwrap_or("");
                let path = item.get("path").and_then(Value::as_str);
                let html_url = item
                    .get("html_url")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("https://github.com/{owner}/{repo}#comment-{id}"));
                let text = if let Some(file_path) = path {
                    format!("[{file_path}] {body}")
                } else {
                    body.to_owned()
                };
                let number_opt = item
                    .get("issue_number")
                    .or_else(|| item.get("pull_request_number"))
                    .and_then(Value::as_u64)
                    .or_else(|| {
                        [
                            item.get("issue_url").and_then(Value::as_str),
                            item.get("pull_request_url").and_then(Value::as_str),
                            item.get("html_url").and_then(Value::as_str),
                        ]
                        .into_iter()
                        .flatten()
                        .find_map(extract_github_thread_number)
                    });
                let thread_key = number_opt
                    .map(|n| format!("{owner}/{repo}#{n}"))
                    .or_else(|| {
                        item.get("pull_request_review_id")
                            .and_then(Value::as_u64)
                            .map(|rid| format!("{owner}/{repo}#review-{rid}"))
                    });
                let reply_to =
                    item.get("in_reply_to_id")
                        .and_then(Value::as_u64)
                        .map(|parent_id| {
                            let base_key = format!("gh:{owner}/{repo}:{stream_name}:{parent_id}");
                            checkpoint
                                .known_items
                                .get(&base_key)
                                .map(|s| s.active_key.clone())
                                .unwrap_or(base_key)
                        });
                (
                    format!("gh:{owner}/{repo}:{stream_name}:{id}"),
                    html_url,
                    text,
                    thread_key,
                    reply_to,
                )
            }
            "releases" => {
                let id = item.get("id").and_then(Value::as_u64)?;
                let tag = item.get("tag_name").and_then(Value::as_str).unwrap_or("");
                let name = item.get("name").and_then(Value::as_str).unwrap_or(tag);
                let body = item.get("body").and_then(Value::as_str).unwrap_or("");
                let html_url = item
                    .get("html_url")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        format!("https://github.com/{owner}/{repo}/releases/tag/{tag}")
                    });
                (
                    format!("gh:{owner}/{repo}:release:{id}"),
                    html_url,
                    format!("Release {name} ({tag})\n\n{body}"),
                    None,
                    None,
                )
            }
            _ => return None,
        };

        let clean_text = redactor.redact(raw_text.trim());
        let known_prev = checkpoint.known_items.get(&logical_item_key).cloned();
        if clean_text.trim().is_empty() {
            if let Some(prev) = known_prev {
                if !prev.content_sha256.is_empty() {
                    batch.events.push(IngestedSourceEvent {
                        external_key: prev.active_key.clone(),
                        uri,
                        media_type: "text/markdown".into(),
                        speaker_external_id: author.to_owned(),
                        speaker_display_name: author.to_owned(),
                        ingested_by: ingested_by.clone(),
                        source_scope_id: scope.source_scope_id.clone(),
                        content: String::new(),
                        event_op: SourceEventOp::Withdraw,
                        occurred_at: updated_ms,
                        model_policy: scope.model_policy.clone(),
                        thread_key,
                        reply_to,
                    });
                    checkpoint.known_items.insert(
                        logical_item_key,
                        GitHubItemState {
                            active_key: prev.active_key,
                            content_sha256: String::new(),
                            updated_at_ms: updated_ms.0,
                            revision: prev.revision,
                        },
                    );
                    return Some(updated_ms);
                }
            }
            batch.skipped_low_signal += 1;
            return Some(updated_ms);
        }
        if is_low_signal_ack(&clean_text)
            && known_prev.is_none()
            && thread_key.is_none()
            && reply_to.is_none()
        {
            batch.skipped_low_signal += 1;
            return Some(updated_ms);
        }

        let content_sha = ContentHash::digest(clean_text.as_bytes()).to_string();
        if let Some(prev) = known_prev {
            if prev.content_sha256 == content_sha && prev.updated_at_ms >= updated_ms.0 {
                return Some(updated_ms);
            }
            let next_rev = prev.revision.saturating_add(1);
            let next_key = format!("{logical_item_key}@rev{next_rev}");
            let event_op = if prev.content_sha256.is_empty() {
                SourceEventOp::Upsert
            } else {
                SourceEventOp::Supersede {
                    previous_external_key: prev.active_key,
                }
            };
            batch.events.push(IngestedSourceEvent {
                external_key: next_key.clone(),
                uri,
                media_type: "text/markdown".into(),
                speaker_external_id: author.to_owned(),
                speaker_display_name: author.to_owned(),
                ingested_by: ingested_by.clone(),
                source_scope_id: scope.source_scope_id.clone(),
                content: clean_text,
                event_op,
                occurred_at: updated_ms,
                model_policy: scope.model_policy.clone(),
                thread_key,
                reply_to,
            });
            checkpoint.known_items.insert(
                logical_item_key,
                GitHubItemState {
                    active_key: next_key,
                    content_sha256: content_sha,
                    updated_at_ms: updated_ms.0,
                    revision: next_rev,
                },
            );
        } else {
            batch.events.push(IngestedSourceEvent {
                external_key: logical_item_key.clone(),
                uri,
                media_type: "text/markdown".into(),
                speaker_external_id: author.to_owned(),
                speaker_display_name: author.to_owned(),
                ingested_by: ingested_by.clone(),
                source_scope_id: scope.source_scope_id.clone(),
                content: clean_text,
                event_op: SourceEventOp::Upsert,
                occurred_at: updated_ms,
                model_policy: scope.model_policy.clone(),
                thread_key,
                reply_to,
            });
            checkpoint.known_items.insert(
                logical_item_key.clone(),
                GitHubItemState {
                    active_key: logical_item_key,
                    content_sha256: content_sha,
                    updated_at_ms: updated_ms.0,
                    revision: 0,
                },
            );
        }

        Some(updated_ms)
    }
}

fn extract_github_thread_number(url: &str) -> Option<u64> {
    let before_frag = url.split(['#', '?']).next().unwrap_or(url);
    let segments: Vec<&str> = before_frag.split('/').filter(|s| !s.is_empty()).collect();
    for pair in segments.windows(2) {
        if matches!(pair[0], "issues" | "pulls" | "pull") {
            if let Ok(num) = pair[1].parse::<u64>() {
                return Some(num);
            }
        }
    }
    None
}

struct GitHubSyncCtx<'a> {
    owner: &'a str,
    repo: &'a str,
    scope: &'a ConnectorScope,
    checkpoint: &'a mut GitHubCheckpoint,
    ingested_by: &'a ActorId,
    redactor: &'a SecretRedactor,
    proposals: &'a mut BTreeMap<String, ExternalIdentityProposal>,
    batch: &'a mut PollBatch,
}

impl<T: GitHubTransport> Connector for GitHubConnector<T> {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::GitHub
    }

    fn poll_incremental<'a>(
        &'a self,
        scope: &'a ConnectorScope,
        cursor: &'a ConnectorCursor,
        ingested_by: &'a ActorId,
    ) -> BoxFuture<'a, Result<PollBatch>> {
        Box::pin(async move {
            if !scope.enabled {
                return Err(ConnectError::ScopeDisabled(
                    scope.source_scope_id.to_string(),
                ));
            }
            let (owner, repo) = parse_owner_repo(&scope.upstream_container_id)?;
            let mut checkpoint = GitHubCheckpoint::from_cursor(cursor)?;
            let mut batch = PollBatch::default();
            let mut proposals = BTreeMap::<String, ExternalIdentityProposal>::new();
            let redactor =
                SecretRedactor::new().map_err(|e| ConnectError::Transport(e.to_string()))?;

            let roster = self
                .sync_repo_scope(&owner, &repo, scope, &mut checkpoint, &mut proposals)
                .await?;
            batch.scope_rosters.push(roster);

            let mut sync_ctx = GitHubSyncCtx {
                owner: &owner,
                repo: &repo,
                scope,
                checkpoint: &mut checkpoint,
                ingested_by,
                redactor: &redactor,
                proposals: &mut proposals,
                batch: &mut batch,
            };
            for stream_name in ["issues", "issue_comments", "pull_comments", "releases"] {
                self.sync_stream(stream_name, &mut sync_ctx).await?;
            }

            batch.identity_proposals = proposals.into_values().collect();
            batch.next_cursor = checkpoint.into_cursor(self.config.now)?;
            Ok(batch)
        })
    }
}

/// Parses `"owner/repo"` or `"github:owner/repo"` safely.
pub fn parse_owner_repo(raw: &str) -> Result<(String, String)> {
    let trimmed = raw.trim().strip_prefix("github:").unwrap_or(raw.trim());
    let Some((owner, repo)) = trimmed.split_once('/') else {
        return Err(ConnectError::Transport(format!(
            "invalid GitHub repository `{raw}`; expected `owner/repo`"
        )));
    };
    if owner.is_empty()
        || repo.is_empty()
        || repo.contains('/')
        || !owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        || !repo
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    {
        return Err(ConnectError::Transport(format!(
            "invalid GitHub repository `{raw}`"
        )));
    }
    Ok((owner.to_owned(), repo.to_owned()))
}

fn record_gh_identity(proposals: &mut BTreeMap<String, ExternalIdentityProposal>, login: &str) {
    proposals
        .entry(login.to_owned())
        .or_insert_with(|| ExternalIdentityProposal {
            provider: "github".into(),
            external_id: login.to_owned(),
            display_name: login.to_owned(),
            proposed_actor: None,
        });
}

fn current_unix_sec() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(0))
        .unwrap_or(0)
}
