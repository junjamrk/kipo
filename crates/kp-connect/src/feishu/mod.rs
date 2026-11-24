//! Read-only Feishu (`lark-cli`) connector with WeKnora (MIT) sync mechanics.

mod chat_sync;
mod doc_sync;
mod parsers;
mod transport;

pub use transport::{FeishuRateLimitPolicy, LarkCliTransport};

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use kp_model::{ActorId, BoxFuture, EpochMs};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ConnectError, Connector, ConnectorCursor, ConnectorKind, ConnectorScope, FeishuTransport,
    PollBatch, Result,
};

/// One-line transparent notice displayed when enabling a Feishu connector (`Rev 5 §0.2.4`).
pub fn feishu_enable_notice(active_model_label: &str) -> String {
    let trimmed = active_model_label.trim();
    let label = if trimmed.is_empty() {
        "自动检测"
    } else {
        trimmed
    };
    format!(
        "提示：同步内容将使用当前全局模型处理（当前：{label}），如需更改或仅存原文请见 kp config"
    )
}

/// Feishu OpenAPI frequency-limit error code (WeKnora `feishu/core/request_limits.go`).
pub const FEISHU_ERR_RATE_LIMITED: i64 = 99_991_400;

/// Default initial backfill lookback window (30 days in seconds).
pub const DEFAULT_INITIAL_BACKFILL_SEC: i64 = 30 * 86_400;

/// Default recent-window message re-scan lookback (7 days in seconds) for recall/edit detection,
/// because `lark-cli event consume` does not expose message recall/edit EventKeys.
pub const DEFAULT_RESCAN_WINDOW_SEC: i64 = 7 * 86_400;

pub(super) const ALLOWED_DRIVE_RESOURCE_TYPES: &[&str] = &[
    "doc", "docx", "sheet", "bitable", "file", "folder", "wiki", "slides",
];

const CHAT_MESSAGES_LIST_VALUE_FLAGS: &[&str] = &[
    "--as",
    "--chat-id",
    "--end",
    "--format",
    "--jq",
    "--page-size",
    "--page-token",
    "--sort",
    "--start",
    "--user-id",
];
const CHAT_MESSAGES_LIST_BOOL_FLAGS: &[&str] = &["--dry-run", "--json", "--no-reactions"];

const DOCS_FETCH_VALUE_FLAGS: &[&str] = &[
    "--api-version",
    "--as",
    "--context-after",
    "--context-before",
    "--detail",
    "--doc",
    "--doc-format",
    "--end-block-id",
    "--format",
    "--jq",
    "--keyword",
    "--limit",
    "--max-depth",
    "--offset",
    "--revision-id",
    "--scope",
    "--start-block-id",
];
const DOCS_FETCH_BOOL_FLAGS: &[&str] = &["--dry-run", "--json"];

const SERVICE_READ_VALUE_FLAGS: &[&str] = &[
    "--as",
    "--format",
    "--jq",
    "--page-delay",
    "--page-limit",
    "--params",
];
const SERVICE_READ_BOOL_FLAGS: &[&str] = &["--dry-run", "--json", "--page-all"];

const API_GET_VALUE_FLAGS: &[&str] = &[
    "--as",
    "--format",
    "--jq",
    "--page-delay",
    "--page-limit",
    "--page-size",
    "--params",
];
const API_GET_BOOL_FLAGS: &[&str] = &["--dry-run", "--json", "--page-all"];

const VC_NOTES_VALUE_FLAGS: &[&str] = &[
    "--as",
    "--calendar-event-ids",
    "--format",
    "--jq",
    "--meeting-ids",
    "--minute-tokens",
    "--output-dir",
];
const VC_NOTES_BOOL_FLAGS: &[&str] = &["--dry-run", "--json"];

/// Environment variable overriding the `lark-cli` executable path.
pub const ENV_LARK_CLI_BIN: &str = "KP_LARK_CLI_BIN";

/// Resolves the installed `lark-cli` binary across `KP_LARK_CLI_BIN`, `PATH`, NVM node bins,
/// Homebrew, and `/usr/local/bin` so non-interactive daemon/hook/test processes find it reliably.
pub fn resolve_lark_cli_binary() -> String {
    if let Some(explicit) = std::env::var_os(ENV_LARK_CLI_BIN) {
        let s = explicit.to_string_lossy();
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    if let Some(path_os) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_os) {
            let candidate = dir.join("lark-cli");
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    if let Some(nvm_bin) = std::env::var_os("NVM_BIN") {
        let candidate = PathBuf::from(nvm_bin).join("lark-cli");
        if candidate.is_file() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let nvm_nodes = PathBuf::from(home).join(".nvm/versions/node");
        if let Ok(entries) = fs::read_dir(&nvm_nodes) {
            let mut candidates: Vec<PathBuf> = entries
                .filter_map(std::result::Result::ok)
                .map(|e| e.path().join("bin/lark-cli"))
                .filter(|p| p.is_file())
                .collect();
            candidates.sort();
            if let Some(best) = candidates.pop() {
                return best.to_string_lossy().into_owned();
            }
        }
    }
    for fixed in ["/opt/homebrew/bin/lark-cli", "/usr/local/bin/lark-cli"] {
        if Path::new(fixed).is_file() {
            return fixed.to_owned();
        }
    }
    "lark-cli".into()
}

/// Validates that a `lark-cli` argument vector matches BOTH the strict read-only command allowlist
/// AND the real `lark-cli` flag schema for that subcommand before spawning any process.
pub fn validate_lark_cli_readonly_args(args: &[String]) -> Result<()> {
    let mut slice = args;
    if slice.first().map(String::as_str) == Some("--profile") {
        let Some(profile_val) = slice.get(1) else {
            return Err(ConnectError::ReadOnlyViolation(
                "missing value after --profile".into(),
            ));
        };
        validate_safe_cli_value("profile", profile_val)?;
        slice = &slice[2..];
    }
    if slice.is_empty() {
        return Err(ConnectError::ReadOnlyViolation(
            "empty lark-cli command is rejected".into(),
        ));
    }

    match (
        slice.first().map(String::as_str),
        slice.get(1).map(String::as_str),
        slice.get(2).map(String::as_str),
    ) {
        (Some("im"), Some("+chat-messages-list"), _) => {
            let _ = validate_subcommand_flags(
                "im +chat-messages-list",
                &slice[2..],
                CHAT_MESSAGES_LIST_VALUE_FLAGS,
                CHAT_MESSAGES_LIST_BOOL_FLAGS,
            )?;
            Ok(())
        }
        (Some("im"), Some("chat.members"), Some("get")) => {
            let _ = validate_subcommand_flags(
                "im chat.members get",
                &slice[3..],
                SERVICE_READ_VALUE_FLAGS,
                SERVICE_READ_BOOL_FLAGS,
            )?;
            Ok(())
        }
        (Some("docs"), Some("+fetch"), _) => {
            let _ = validate_subcommand_flags(
                "docs +fetch",
                &slice[2..],
                DOCS_FETCH_VALUE_FLAGS,
                DOCS_FETCH_BOOL_FLAGS,
            )?;
            Ok(())
        }
        (Some("wiki"), Some("nodes"), Some("list")) => {
            let parsed = validate_subcommand_flags(
                "wiki nodes list",
                &slice[3..],
                SERVICE_READ_VALUE_FLAGS,
                SERVICE_READ_BOOL_FLAGS,
            )?;
            let Some(params_raw) = parsed.get("--params") else {
                return Err(ConnectError::ReadOnlyViolation(
                    "`wiki nodes list` requires `--params` JSON".into(),
                ));
            };
            let params_json: Value = serde_json::from_str(params_raw).map_err(|e| {
                ConnectError::ReadOnlyViolation(format!("invalid `wiki nodes list` --params: {e}"))
            })?;
            let Some(space_id) = params_json
                .get("space_id")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
            else {
                return Err(ConnectError::ReadOnlyViolation(
                    "`wiki nodes list` --params requires nonempty `space_id`".into(),
                ));
            };
            validate_safe_token("space_id", space_id)?;
            Ok(())
        }
        (Some("vc"), Some("+notes"), _) => {
            let parsed = validate_subcommand_flags(
                "vc +notes",
                &slice[2..],
                VC_NOTES_VALUE_FLAGS,
                VC_NOTES_BOOL_FLAGS,
            )?;
            if let Some(out_dir) = parsed.get("--output-dir") {
                validate_safe_output_dir(out_dir)?;
            }
            Ok(())
        }
        (Some("minutes"), Some("minutes"), Some("get")) => {
            let _ = validate_subcommand_flags(
                "minutes minutes get",
                &slice[3..],
                SERVICE_READ_VALUE_FLAGS,
                SERVICE_READ_BOOL_FLAGS,
            )?;
            Ok(())
        }
        (Some("calendar"), Some("events"), Some("instance_view")) => {
            let _ = validate_subcommand_flags(
                "calendar events instance_view",
                &slice[3..],
                SERVICE_READ_VALUE_FLAGS,
                SERVICE_READ_BOOL_FLAGS,
            )?;
            Ok(())
        }
        (Some("api"), Some("GET"), Some(path)) => {
            let parsed = validate_subcommand_flags(
                "api GET",
                &slice[3..],
                API_GET_VALUE_FLAGS,
                API_GET_BOOL_FLAGS,
            )?;
            validate_allowed_readonly_api_get(path, &parsed)
        }
        _ => Err(ConnectError::ReadOnlyViolation(format!(
            "lark-cli command `{}` is outside the read-only connector allowlist",
            slice.join(" ")
        ))),
    }
}

fn validate_safe_output_dir(dir: &str) -> Result<()> {
    if dir.trim().is_empty() || dir.starts_with('~') || dir.chars().any(char::is_control) {
        return Err(ConnectError::ReadOnlyViolation(format!(
            "disallowed `vc +notes --output-dir` path `{dir}`"
        )));
    }
    for comp in Path::new(dir).components() {
        if matches!(comp, Component::ParentDir) {
            return Err(ConnectError::ReadOnlyViolation(format!(
                "path traversal `..` rejected in `--output-dir` `{dir}`"
            )));
        }
    }
    Ok(())
}

fn validate_subcommand_flags<'a>(
    subcommand: &str,
    tail: &'a [String],
    value_flags: &[&str],
    bool_flags: &[&str],
) -> Result<BTreeMap<String, &'a str>> {
    let mut parsed = BTreeMap::<String, &'a str>::new();
    let mut idx = 0_usize;
    while idx < tail.len() {
        let tok = tail[idx].as_str();
        if !tok.starts_with("--") {
            return Err(ConnectError::ReadOnlyViolation(format!(
                "unexpected positional or short token `{tok}` in `lark-cli {subcommand}`"
            )));
        }
        if let Some((flag_name, inline_val)) = tok.split_once('=') {
            if value_flags.contains(&flag_name) {
                if inline_val.is_empty() || inline_val.chars().any(char::is_control) {
                    return Err(ConnectError::ReadOnlyViolation(format!(
                        "invalid inline value for `{flag_name}` in `lark-cli {subcommand}`"
                    )));
                }
                parsed.insert(flag_name.to_owned(), inline_val);
                idx += 1;
                continue;
            }
            return Err(ConnectError::ReadOnlyViolation(format!(
                "unsupported or forbidden flag `{flag_name}` for `lark-cli {subcommand}`"
            )));
        }
        if bool_flags.contains(&tok) {
            parsed.insert(tok.to_owned(), "true");
            idx += 1;
            continue;
        }
        if value_flags.contains(&tok) {
            let Some(val) = tail.get(idx + 1) else {
                return Err(ConnectError::ReadOnlyViolation(format!(
                    "missing value for `{tok}` in `lark-cli {subcommand}`"
                )));
            };
            let allow_neg_one =
                tok == "--revision-id" || tok == "--max-depth" || tok == "--end-block-id";
            if val.is_empty()
                || (val.starts_with('-') && !(allow_neg_one && val == "-1"))
                || val.chars().any(char::is_control)
            {
                return Err(ConnectError::ReadOnlyViolation(format!(
                    "disallowed value `{val}` for `{tok}` in `lark-cli {subcommand}`"
                )));
            }
            parsed.insert(tok.to_owned(), val.as_str());
            idx += 2;
            continue;
        }
        return Err(ConnectError::ReadOnlyViolation(format!(
            "unsupported or forbidden flag `{tok}` for `lark-cli {subcommand}`"
        )));
    }
    Ok(parsed)
}

fn validate_allowed_readonly_api_get(
    path: &str,
    parsed_flags: &BTreeMap<String, &str>,
) -> Result<()> {
    if is_allowed_readonly_drive_perm_path(path) {
        return Ok(());
    }
    if path == "/open-apis/im/v1/messages" {
        let Some(params_raw) = parsed_flags.get("--params") else {
            return Err(ConnectError::ReadOnlyViolation(
                "`api GET /open-apis/im/v1/messages` requires `--params`".into(),
            ));
        };
        let params: Value = serde_json::from_str(params_raw).map_err(|e| {
            ConnectError::ReadOnlyViolation(format!("invalid im/v1/messages `--params` JSON: {e}"))
        })?;
        let container_type = params
            .get("container_id_type")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !matches!(container_type, "thread" | "chat") {
            return Err(ConnectError::ReadOnlyViolation(format!(
                "disallowed `container_id_type` `{container_type}` on im/v1/messages"
            )));
        }
        let Some(container_id) = params
            .get("container_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            return Err(ConnectError::ReadOnlyViolation(
                "missing `container_id` on im/v1/messages".into(),
            ));
        };
        validate_safe_token("container_id", container_id)?;
        return Ok(());
    }
    Err(ConnectError::ReadOnlyViolation(format!(
        "`lark-cli api GET {path}` is outside the read-only connector allowlist"
    )))
}

fn is_allowed_readonly_drive_perm_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/open-apis/drive/v1/permissions/") else {
        return false;
    };
    let Some(token) = rest.strip_suffix("/members") else {
        return false;
    };
    !token.is_empty()
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub(super) fn validate_safe_cli_value(field: &str, value: &str) -> Result<()> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.starts_with('-') || trimmed.chars().any(char::is_control) {
        return Err(ConnectError::ReadOnlyViolation(format!(
            "disallowed `{field}` value `{value}`"
        )));
    }
    Ok(())
}

pub(super) fn validate_safe_token(field: &str, token: &str) -> Result<()> {
    if token.is_empty()
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b':')
        || token.starts_with('-')
    {
        return Err(ConnectError::ReadOnlyViolation(format!(
            "invalid `{field}` token `{token}`"
        )));
    }
    Ok(())
}

/// Per-message ledger entry stored in `FeishuCheckpoint` for edit (`Supersede`) & recall (`Withdraw`) detection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeishuMessageState {
    pub active_key: String,
    pub content_sha256: String,
    pub update_time_ms: i64,
    pub revision: u32,
    pub withdrawn: bool,
}

/// Per-wiki/doc node ledger entry ported from Tencent WeKnora (`connector/feishu/wiki/connector.go`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeishuWikiNodeState {
    pub obj_token: String,
    pub obj_type: String,
    pub obj_edit_time_ms: i64,
    pub active_key: String,
    pub revision: u32,
    pub withdrawn: bool,
}

/// Resumable WeKnora-style Feishu sync checkpoint serialized inside `ConnectorCursor.cursor_token`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeishuCheckpoint {
    pub high_water_ms: Option<i64>,
    pub pending_page_token: Option<String>,
    pub pending_window_start_sec: Option<i64>,
    pub pending_window_end_sec: Option<i64>,
    #[serde(default)]
    pub known_threads: BTreeSet<String>,
    #[serde(default)]
    pub known_messages: BTreeMap<String, FeishuMessageState>,
    #[serde(default)]
    pub known_wiki_nodes: BTreeMap<String, FeishuWikiNodeState>,
}

impl FeishuCheckpoint {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeishuTarget {
    Chat { chat_id: String },
    WikiSpace { space_id: String },
    Document { doc_type: String, doc_token: String },
    Minutes { minute_token: String },
    Calendar { calendar_id: String },
}

impl FeishuTarget {
    pub fn parse(upstream_container_id: &str) -> Result<Self> {
        let raw = upstream_container_id.trim();
        if raw.is_empty() {
            return Err(ConnectError::Transport(
                "empty Feishu upstream_container_id".into(),
            ));
        }
        if let Some(chat_id) = raw.strip_prefix("chat:") {
            return Ok(Self::Chat {
                chat_id: chat_id.to_owned(),
            });
        }
        if let Some(space_id) = raw.strip_prefix("wiki:") {
            return Ok(Self::WikiSpace {
                space_id: space_id.to_owned(),
            });
        }
        if let Some(doc_token) = raw.strip_prefix("docx:") {
            return Ok(Self::Document {
                doc_type: "docx".into(),
                doc_token: doc_token.to_owned(),
            });
        }
        if let Some(doc_token) = raw.strip_prefix("doc:") {
            return Ok(Self::Document {
                doc_type: "docx".into(),
                doc_token: doc_token.to_owned(),
            });
        }
        if let Some(minute_token) = raw.strip_prefix("minutes:") {
            return Ok(Self::Minutes {
                minute_token: minute_token.to_owned(),
            });
        }
        if let Some(calendar_id) = raw.strip_prefix("calendar:") {
            return Ok(Self::Calendar {
                calendar_id: calendar_id.to_owned(),
            });
        }
        Ok(Self::Chat {
            chat_id: raw.to_owned(),
        })
    }
}

/// Configuration for `FeishuConnector` sync windows and pagination budgets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeishuSyncConfig {
    pub now: EpochMs,
    pub initial_backfill_sec: i64,
    pub rescan_window_sec: i64,
    pub max_pages_per_poll: usize,
}

impl Default for FeishuSyncConfig {
    fn default() -> Self {
        Self {
            now: EpochMs(1_795_245_380_000),
            initial_backfill_sec: DEFAULT_INITIAL_BACKFILL_SEC,
            rescan_window_sec: DEFAULT_RESCAN_WINDOW_SEC,
            max_pages_per_poll: 20,
        }
    }
}

/// Read-only Feishu incremental connector implementing `Connector`.
pub struct FeishuConnector<T: FeishuTransport = LarkCliTransport> {
    pub(super) transport: T,
    pub(super) config: FeishuSyncConfig,
}

impl<T: FeishuTransport> FeishuConnector<T> {
    pub fn new(transport: T, config: FeishuSyncConfig) -> Self {
        Self { transport, config }
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }
}

impl<T: FeishuTransport> Connector for FeishuConnector<T> {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::Feishu
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
            let mut checkpoint = FeishuCheckpoint::from_cursor(cursor)?;
            match FeishuTarget::parse(&scope.upstream_container_id)? {
                FeishuTarget::Chat { chat_id } => {
                    self.sync_chat(&chat_id, scope, &mut checkpoint, ingested_by)
                        .await
                }
                FeishuTarget::WikiSpace { space_id } => {
                    self.sync_wiki_space(&space_id, scope, &mut checkpoint, ingested_by)
                        .await
                }
                FeishuTarget::Document {
                    doc_type,
                    doc_token,
                } => {
                    self.sync_document(&doc_type, &doc_token, scope, &mut checkpoint, ingested_by)
                        .await
                }
                FeishuTarget::Minutes { minute_token } => {
                    self.sync_minutes(&minute_token, scope, &mut checkpoint, ingested_by)
                        .await
                }
                FeishuTarget::Calendar { calendar_id } => {
                    self.sync_calendar(&calendar_id, scope, &mut checkpoint, ingested_by)
                        .await
                }
            }
        })
    }
}
