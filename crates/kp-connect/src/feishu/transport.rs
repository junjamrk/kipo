use std::fs;
use std::path::Path;
use std::sync::Arc;

use kp_model::{format_epoch_sec_rfc3339_utc, BoxFuture, EpochMs};
use serde_json::{json, Value};

use super::{
    resolve_lark_cli_binary, validate_lark_cli_readonly_args, validate_safe_cli_value,
    validate_safe_token, ALLOWED_DRIVE_RESOURCE_TYPES, FEISHU_ERR_RATE_LIMITED,
};
use crate::subprocess::{CommandOutput, CommandRunner, Sleeper, StdCommandRunner, TokioSleeper};
use crate::{ConnectError, FeishuTransport, Result};

/// Rate-limit cooldown configuration for Feishu `99991400` responses (ported from WeKnora MIT).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeishuRateLimitPolicy {
    pub base_cooldown_ms: u64,
    pub max_cooldown_ms: u64,
    pub max_retries: u32,
}

impl Default for FeishuRateLimitPolicy {
    fn default() -> Self {
        Self {
            base_cooldown_ms: 1_000,
            max_cooldown_ms: 30_000,
            max_retries: 3,
        }
    }
}

impl FeishuRateLimitPolicy {
    pub fn cooldown_for_attempt(&self, attempt: u32, retry_after_sec: Option<u64>) -> u64 {
        if let Some(secs) = retry_after_sec.filter(|s| *s > 0) {
            return secs.saturating_mul(1_000).min(self.max_cooldown_ms);
        }
        let factor = 1_u64.checked_shl(attempt.min(10)).unwrap_or(1024);
        self.base_cooldown_ms
            .saturating_mul(factor)
            .min(self.max_cooldown_ms)
    }
}

/// Read-only `lark-cli` subprocess transport implementing `FeishuTransport`.
pub struct LarkCliTransport {
    binary: String,
    profile: Option<String>,
    identity: String,
    rate_limit: FeishuRateLimitPolicy,
    runner: Arc<dyn CommandRunner>,
    sleeper: Arc<dyn Sleeper>,
}

impl Default for LarkCliTransport {
    fn default() -> Self {
        Self::new(resolve_lark_cli_binary())
    }
}

impl LarkCliTransport {
    pub fn new(binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
            profile: None,
            identity: "user".into(),
            rate_limit: FeishuRateLimitPolicy::default(),
            runner: Arc::new(StdCommandRunner),
            sleeper: Arc::new(TokioSleeper),
        }
    }

    pub fn with_profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = Some(profile.into());
        self
    }

    pub fn with_identity(mut self, identity: impl Into<String>) -> Self {
        self.identity = identity.into();
        self
    }

    pub fn with_rate_limit_policy(mut self, policy: FeishuRateLimitPolicy) -> Self {
        self.rate_limit = policy;
        self
    }

    pub fn with_runner_and_sleeper(
        mut self,
        runner: Arc<dyn CommandRunner>,
        sleeper: Arc<dyn Sleeper>,
    ) -> Self {
        self.runner = runner;
        self.sleeper = sleeper;
        self
    }

    pub fn build_readonly_args(&self, tail: Vec<String>) -> Result<Vec<String>> {
        let mut args = Vec::with_capacity(tail.len() + 2);
        if let Some(profile) = &self.profile {
            args.push("--profile".into());
            args.push(profile.clone());
        }
        args.extend(tail);
        validate_lark_cli_readonly_args(&args)?;
        Ok(args)
    }

    /// Executes a validated read-only `lark-cli` command with WeKnora `99991400` cooldown retries.
    pub async fn execute_readonly(&self, tail: Vec<String>) -> Result<Value> {
        self.execute_readonly_in_dir(tail, None).await
    }

    async fn execute_readonly_in_dir(
        &self,
        tail: Vec<String>,
        cwd: Option<&Path>,
    ) -> Result<Value> {
        let args = self.build_readonly_args(tail)?;
        let mut attempt = 0_u32;
        loop {
            let output = self.runner.run_in_dir(&self.binary, &args, cwd).await?;
            match parse_lark_cli_output(&output) {
                Ok(json_val) => return Ok(json_val),
                Err(LarkCallOutcome::RateLimited {
                    retry_after_sec,
                    message,
                }) => {
                    let wait_ms = self
                        .rate_limit
                        .cooldown_for_attempt(attempt, retry_after_sec);
                    if attempt >= self.rate_limit.max_retries {
                        return Err(ConnectError::RateLimited {
                            retry_after_ms: wait_ms,
                            reason: format!(
                                "Feishu error {FEISHU_ERR_RATE_LIMITED} after {} retries: {message}",
                                self.rate_limit.max_retries
                            ),
                        });
                    }
                    self.sleeper.sleep_ms(wait_ms).await;
                    attempt += 1;
                }
                Err(LarkCallOutcome::Fatal(err)) => return Err(err),
            }
        }
    }
}

enum LarkCallOutcome {
    RateLimited {
        retry_after_sec: Option<u64>,
        message: String,
    },
    Fatal(ConnectError),
}

fn parse_lark_cli_output(output: &CommandOutput) -> std::result::Result<Value, LarkCallOutcome> {
    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let stderr_str = String::from_utf8_lossy(&output.stderr);

    if let Ok(parsed) = serde_json::from_slice::<Value>(&output.stdout) {
        if let Some((code, msg, retry_after)) = extract_feishu_error(&parsed) {
            if code == FEISHU_ERR_RATE_LIMITED {
                return Err(LarkCallOutcome::RateLimited {
                    retry_after_sec: retry_after,
                    message: msg,
                });
            }
            return Err(LarkCallOutcome::Fatal(ConnectError::Transport(format!(
                "Feishu API error {code}: {msg}"
            ))));
        }
        if output.status_code == 0 {
            return Ok(parsed);
        }
    }

    let combined = format!("{stdout_str} {stderr_str}");
    if combined.contains(&FEISHU_ERR_RATE_LIMITED.to_string()) {
        return Err(LarkCallOutcome::RateLimited {
            retry_after_sec: None,
            message: combined.trim().to_owned(),
        });
    }

    if output.status_code != 0 {
        return Err(LarkCallOutcome::Fatal(ConnectError::Transport(format!(
            "lark-cli exited with status {}: {}",
            output.status_code,
            stderr_str.trim()
        ))));
    }

    serde_json::from_slice::<Value>(&output.stdout)
        .map_err(|err| LarkCallOutcome::Fatal(ConnectError::Json(err)))
}

fn extract_feishu_error(val: &Value) -> Option<(i64, String, Option<u64>)> {
    if let Some(code) = val.get("code").and_then(Value::as_i64) {
        if code != 0 {
            let msg = val
                .get("msg")
                .or_else(|| val.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("feishu error")
                .to_owned();
            let retry_after = val
                .get("retry_after")
                .or_else(|| val.pointer("/error/retry_after"))
                .or_else(|| val.get("x_ogw_ratelimit_reset"))
                .and_then(Value::as_u64);
            return Some((code, msg, retry_after));
        }
    }
    if val.get("ok").and_then(Value::as_bool) == Some(false) {
        let code = val
            .pointer("/error/code")
            .and_then(Value::as_i64)
            .unwrap_or(-1);
        let msg = val
            .pointer("/error/message")
            .or_else(|| val.pointer("/error/msg"))
            .and_then(Value::as_str)
            .unwrap_or("lark-cli command returned ok=false")
            .to_owned();
        let retry_after = val.pointer("/error/retry_after").and_then(Value::as_u64);
        return Some((code, msg, retry_after));
    }
    None
}

impl FeishuTransport for LarkCliTransport {
    fn list_chat_messages<'a>(
        &'a self,
        chat_id: &'a str,
        start_time_sec: Option<i64>,
        end_time_sec: Option<i64>,
        page_token: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            validate_safe_token("chat_id", chat_id)?;
            validate_safe_cli_value("identity", &self.identity)?;
            let mut args = vec![
                "im".into(),
                "+chat-messages-list".into(),
                "--as".into(),
                self.identity.clone(),
                "--chat-id".into(),
                chat_id.to_owned(),
                "--sort".into(),
                "asc".into(),
                "--page-size".into(),
                "50".into(),
                "--format".into(),
                "json".into(),
            ];
            if let Some(start) = start_time_sec {
                args.push("--start".into());
                args.push(format_epoch_sec_rfc3339_utc(start));
            }
            if let Some(end) = end_time_sec {
                args.push("--end".into());
                args.push(format_epoch_sec_rfc3339_utc(end));
            }
            if let Some(token) = page_token {
                validate_safe_cli_value("page_token", token)?;
                args.push("--page-token".into());
                args.push(token.to_owned());
            }
            self.execute_readonly(args).await
        })
    }

    fn list_thread_messages<'a>(
        &'a self,
        thread_id: &'a str,
        page_token: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            validate_safe_token("thread_id", thread_id)?;
            validate_safe_cli_value("identity", &self.identity)?;
            let mut params = json!({
                "container_id_type": "thread",
                "container_id": thread_id,
                "sort_type": "ByCreateTimeAsc",
                "page_size": 50,
            });
            if let Some(token) = page_token {
                validate_safe_cli_value("page_token", token)?;
                params["page_token"] = Value::String(token.to_owned());
            }
            let args = vec![
                "api".into(),
                "GET".into(),
                "/open-apis/im/v1/messages".into(),
                "--as".into(),
                self.identity.clone(),
                "--format".into(),
                "json".into(),
                "--params".into(),
                serde_json::to_string(&params)?,
            ];
            self.execute_readonly(args).await
        })
    }

    fn list_chat_members<'a>(
        &'a self,
        chat_id: &'a str,
        page_token: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            validate_safe_token("chat_id", chat_id)?;
            validate_safe_cli_value("identity", &self.identity)?;
            let mut params = json!({
                "chat_id": chat_id,
                "member_id_type": "open_id",
                "page_size": 100,
            });
            if let Some(token) = page_token {
                validate_safe_cli_value("page_token", token)?;
                params["page_token"] = Value::String(token.to_owned());
            }
            let args = vec![
                "im".into(),
                "chat.members".into(),
                "get".into(),
                "--as".into(),
                self.identity.clone(),
                "--format".into(),
                "json".into(),
                "--params".into(),
                serde_json::to_string(&params)?,
            ];
            self.execute_readonly(args).await
        })
    }

    fn fetch_document<'a>(&'a self, doc_token: &'a str) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            validate_safe_token("doc_token", doc_token)?;
            validate_safe_cli_value("identity", &self.identity)?;
            let args = vec![
                "docs".into(),
                "+fetch".into(),
                "--as".into(),
                self.identity.clone(),
                "--doc".into(),
                doc_token.to_owned(),
                "--format".into(),
                "json".into(),
            ];
            self.execute_readonly(args).await
        })
    }

    fn list_wiki_nodes<'a>(
        &'a self,
        space_id: &'a str,
        parent_node_token: Option<&'a str>,
        page_token: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            validate_safe_token("space_id", space_id)?;
            validate_safe_cli_value("identity", &self.identity)?;
            let mut params = json!({
                "space_id": space_id,
                "page_size": 50,
            });
            if let Some(parent) = parent_node_token {
                validate_safe_token("parent_node_token", parent)?;
                params["parent_node_token"] = Value::String(parent.to_owned());
            }
            if let Some(token) = page_token {
                validate_safe_cli_value("page_token", token)?;
                params["page_token"] = Value::String(token.to_owned());
            }
            let args = vec![
                "wiki".into(),
                "nodes".into(),
                "list".into(),
                "--as".into(),
                self.identity.clone(),
                "--format".into(),
                "json".into(),
                "--params".into(),
                serde_json::to_string(&params)?,
            ];
            self.execute_readonly(args).await
        })
    }

    fn fetch_minutes_transcript<'a>(
        &'a self,
        minute_token: &'a str,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            validate_safe_token("minute_token", minute_token)?;
            // Run `vc +notes` inside an ephemeral tempdir with a relative `--output-dir` so
            // `lark-cli`'s builtin artifact export never touches the caller's working directory
            // AND satisfies `lark-cli`'s relative-path check (`lark-shared`).
            let temp_dir = tempfile::tempdir()?;
            let args = vec![
                "vc".into(),
                "+notes".into(),
                "--as".into(),
                "user".into(),
                "--minute-tokens".into(),
                minute_token.to_owned(),
                "--output-dir".into(),
                "minutes_out".into(),
                "--format".into(),
                "json".into(),
            ];
            let mut resp = self
                .execute_readonly_in_dir(args, Some(temp_dir.path()))
                .await?;
            attach_exported_minutes_transcript(&mut resp, temp_dir.path())?;
            Ok(resp)
        })
    }

    fn list_calendar_events<'a>(
        &'a self,
        calendar_id: &'a str,
        start: EpochMs,
        end: EpochMs,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            validate_safe_cli_value("calendar_id", calendar_id)?;
            validate_safe_cli_value("identity", &self.identity)?;
            let params = json!({
                "calendar_id": calendar_id,
                "start_time": (start.0.div_euclid(1_000)).to_string(),
                "end_time": (end.0.div_euclid(1_000)).to_string(),
            });
            let args = vec![
                "calendar".into(),
                "events".into(),
                "instance_view".into(),
                "--as".into(),
                self.identity.clone(),
                "--format".into(),
                "json".into(),
                "--params".into(),
                serde_json::to_string(&params)?,
            ];
            self.execute_readonly(args).await
        })
    }

    fn list_permission_members<'a>(
        &'a self,
        resource_type: &'a str,
        resource_token: &'a str,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            if !ALLOWED_DRIVE_RESOURCE_TYPES.contains(&resource_type) {
                return Err(ConnectError::ReadOnlyViolation(format!(
                    "unsupported Feishu permission resource_type `{resource_type}`"
                )));
            }
            if resource_token.is_empty()
                || !resource_token
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Err(ConnectError::ReadOnlyViolation(format!(
                    "invalid Feishu resource_token `{resource_token}`"
                )));
            }
            validate_safe_cli_value("identity", &self.identity)?;
            let params = json!({
                "type": resource_type,
                "fields": "*",
            });
            let args = vec![
                "api".into(),
                "GET".into(),
                format!("/open-apis/drive/v1/permissions/{resource_token}/members"),
                "--as".into(),
                self.identity.clone(),
                "--format".into(),
                "json".into(),
                "--params".into(),
                serde_json::to_string(&params)?,
            ];
            self.execute_readonly(args).await
        })
    }
}

/// Reads `data.artifacts.transcript_file` (exported inside `temp_root`) into `data.transcript`
/// before the ephemeral `tempdir` drops.
fn attach_exported_minutes_transcript(resp: &mut Value, temp_root: &Path) -> Result<()> {
    let transcript_rel = resp
        .pointer("/data/artifacts/transcript_file")
        .or_else(|| resp.pointer("/artifacts/transcript_file"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned);

    let Some(tf) = transcript_rel else {
        return Ok(());
    };
    let candidate = Path::new(&tf);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        temp_root.join(candidate)
    };
    if !joined.is_file() {
        return Ok(());
    }
    let canon_root = temp_root.canonicalize()?;
    let canon_file = joined.canonicalize()?;
    if !canon_file.starts_with(&canon_root) {
        return Err(ConnectError::ReadOnlyViolation(format!(
            "refusing `transcript_file` outside ephemeral tempdir: `{tf}`"
        )));
    }
    let text = fs::read_to_string(&canon_file)?;
    if text.trim().is_empty() {
        return Ok(());
    }
    if let Some(data_obj) = resp.get_mut("data").and_then(Value::as_object_mut) {
        data_obj.insert("transcript".into(), Value::String(text));
    } else if let Some(root_obj) = resp.as_object_mut() {
        root_obj.insert("transcript".into(), Value::String(text));
    }
    Ok(())
}
