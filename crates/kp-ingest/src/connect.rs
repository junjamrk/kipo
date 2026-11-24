// Async hook lifecycle and child isolation patterns adapted from claude-mem main (Apache-2.0)
// and harness2/core/prompts.py (Apache-2.0); see THIRD_PARTY_NOTICES.md.
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use kp_model::ActorId;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{IngestError, Result};

/// Env override for locating the `codex` CLI binary during `kp connect codex` self-checks in tests.
pub const CODEX_BIN_ENV: &str = "KP_CODEX_BIN";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexSelfCheck {
    pub verified: bool,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_steps: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectReport {
    pub client: String,
    pub settings: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_config: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_check: Option<CodexSelfCheck>,
}

pub const EVENTS: [(&str, &str); 6] = [
    ("session-start", "SessionStart"),
    ("user-prompt-submit", "UserPromptSubmit"),
    ("pre-tool-use", "PreToolUse"),
    ("post-tool-use", "PostToolUse"),
    ("stop", "Stop"),
    ("session-end", "SessionEnd"),
];

pub const CLAUDE_MCP_ALLOWED_TOOLS: [&str; 3] =
    ["mcp__kp__kp_search", "mcp__kp__kp_note", "mcp__kp__kp_why"];

pub const CODEX_MCP_ENABLED_TOOLS: [&str; 3] = ["kp_search", "kp_note", "kp_why"];

const CLAUDE_TOOL_MATCHER: &str = "Edit|Write|MultiEdit|Bash";
const CODEX_TOOL_MATCHER: &str =
    "Edit|Write|MultiEdit|Bash|apply_patch|exec_command|shell|shell_command";

pub fn connect(
    client: &str,
    project: &Path,
    library: &Path,
    actor: &ActorId,
    executable: &Path,
) -> Result<PathBuf> {
    Ok(connect_with_report(client, project, library, actor, executable)?.settings)
}

/// Configures project hooks & MCP auto-approval and runs `codex mcp get kp` self-check for Codex.
/// Supports `claude-code`, `codex`, `cursor`, and `windsurf`.
/// Never modifies trust settings in `~/.codex/config.toml` or `CODEX_HOME`, and never passes
/// `--dangerously-bypass-hook-trust`.
pub fn connect_with_report(
    client: &str,
    project: &Path,
    library: &Path,
    actor: &ActorId,
    executable: &Path,
) -> Result<ConnectReport> {
    connect_with_scope(client, project, false, library, actor, executable)
}

/// Configures Kipo integration for `client` either inside `project` (`global == false`) or at the
/// user-level MCP configuration file (`global == true`, supported for `cursor` and `windsurf`).
pub fn connect_with_scope(
    client: &str,
    project: &Path,
    global: bool,
    library: &Path,
    actor: &ActorId,
    executable: &Path,
) -> Result<ConnectReport> {
    let library = library.canonicalize()?;
    if matches!(client, "cursor" | "windsurf") {
        let path = resolve_mcp_json_path(client, project, global)?;
        configure_mcp_json(&path, &library, actor, executable)?;
        return Ok(ConnectReport {
            client: client.to_owned(),
            settings: path,
            codex_config: None,
            codex_check: None,
        });
    }
    if global {
        return Err(IngestError::Invalid(
            "`--global` is supported for `cursor` and `windsurf`; `claude-code` and `codex` use `--project`",
        ));
    }
    let relative = match client {
        "claude-code" => ".claude/settings.json",
        "codex" => ".codex/hooks.json",
        _ => {
            return Err(IngestError::Invalid(
                "client must be claude-code, codex, cursor, or windsurf",
            ))
        }
    };
    let path = project.join(relative);
    let mut config = match std::fs::File::open(&path) {
        Ok(file) => serde_json::from_slice::<Value>(&crate::read_bounded(file, 1024 * 1024)?)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e.into()),
    };
    let object = config
        .as_object_mut()
        .ok_or(IngestError::Invalid("hook settings must be an object"))?;
    configure_hooks(object, client, &library, actor, executable)?;
    if client == "claude-code" {
        configure_claude_permissions(object)?;
    }

    write_atomic_json(&path, &config)?;

    let (codex_config, codex_check) = if client == "codex" {
        let codex_toml = project.join(".codex/config.toml");
        configure_codex_toml(&codex_toml, &library, actor, executable)?;
        let check = verify_codex_mcp(project, None, None);
        (Some(codex_toml), Some(check))
    } else {
        (None, None)
    };

    Ok(ConnectReport {
        client: client.to_owned(),
        settings: path,
        codex_config,
        codex_check,
    })
}

/// Runs `codex mcp get kp` inside `project` to verify whether Codex actively loads `.codex/config.toml`.
/// When `codex_home` is `Some`, overrides `CODEX_HOME` for hermetic testing without touching `~/.codex`.
pub fn verify_codex_mcp(
    project: &Path,
    codex_bin: Option<&Path>,
    codex_home: Option<&Path>,
) -> CodexSelfCheck {
    let bin = codex_bin
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os(CODEX_BIN_ENV).map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("codex"));

    let mut cmd = Command::new(&bin);
    cmd.args(["mcp", "get", "kp"])
        .current_dir(project)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(home) = codex_home {
        cmd.env("CODEX_HOME", home);
    }

    let unverified = |detail: String| {
        CodexSelfCheck {
        verified: false,
        detail,
        next_steps: vec![
            "1) Open `codex` in this directory and trust the directory (在此目录打开 `codex` 并信任该目录；未信任目录的 `.codex/config.toml` 与 `.codex/hooks.json` 默认不会加载).".to_owned(),
            "2) When Codex prompts \"Hooks need review\", choose \"Review hooks\" to confirm individually or \"Trust all and continue\" — `kp` hooks take effect once approved.".to_owned(),
        ],
    }
    };

    let Ok(mut child) = cmd.spawn() else {
        return unverified("`codex` CLI binary not found on PATH".to_owned());
    };

    let stdout_handle = child.stdout.take().map(|mut r| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = (&mut r).take(64 * 1024).read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        })
    });
    let stderr_handle = child.stderr.take().map(|mut r| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = (&mut r).take(64 * 1024).read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        })
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(15));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };

    let stdout = stdout_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let stderr = stderr_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    let Some(status) = status else {
        return unverified("`codex mcp get kp` timed out".to_owned());
    };

    let combined_lc = format!("{stdout}\n{stderr}").to_lowercase();
    let visible = status.success()
        && stdout.contains("kp")
        && !combined_lc.contains("no mcp server named 'kp'")
        && !combined_lc.contains("untrusted folders stay disabled");

    if visible {
        CodexSelfCheck {
            verified: true,
            detail: "`codex mcp get kp` confirmed `kp` MCP server is active in this directory"
                .to_owned(),
            next_steps: Vec::new(),
        }
    } else {
        let snippet = stderr
            .lines()
            .chain(stdout.lines())
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("folder is not yet trusted in Codex")
            .to_owned();
        unverified(snippet)
    }
}

fn configure_hooks(
    object: &mut serde_json::Map<String, Value>,
    client: &str,
    library: &Path,
    actor: &ActorId,
    executable: &Path,
) -> Result<()> {
    let hooks = object
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or(IngestError::Invalid("hooks must be an object"))?;
    let failure_event =
        (client == "claude-code").then_some(("post-tool-use", "PostToolUseFailure"));
    let matcher = if client == "codex" {
        CODEX_TOOL_MATCHER
    } else {
        CLAUDE_TOOL_MATCHER
    };
    for (event, wire) in EVENTS.into_iter().chain(failure_event) {
        let entries = hooks
            .entry(wire)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or(IngestError::Invalid("hook event must be an array"))?;
        for entry in entries.iter_mut() {
            if let Some(commands) = entry.get_mut("hooks").and_then(Value::as_array_mut) {
                commands.retain(|hook| {
                    !hook
                        .get("command")
                        .and_then(Value::as_str)
                        .is_some_and(|command| owned(command, event))
                });
            }
        }
        entries.retain(|entry| {
            !entry
                .get("hooks")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
        });
        let command = format!(
            "{} hook {event} --client {} --path {} --as {}",
            quote(&executable.to_string_lossy()),
            quote(client),
            quote(&library.to_string_lossy()),
            quote(actor.as_str())
        );
        let is_async_capture =
            client == "claude-code" && matches!(event, "post-tool-use" | "stop" | "session-end");
        let hook_spec = if is_async_capture {
            json!({"type":"command","command":command,"async":true})
        } else {
            json!({"type":"command","command":command})
        };
        let mut entry = json!({"hooks":[hook_spec]});
        if matches!(event, "pre-tool-use" | "post-tool-use") {
            entry["matcher"] = json!(matcher);
        }
        entries.push(entry);
    }
    Ok(())
}

fn configure_claude_permissions(object: &mut serde_json::Map<String, Value>) -> Result<()> {
    let permissions = object
        .entry("permissions")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or(IngestError::Invalid("permissions must be an object"))?;
    let allow = permissions
        .entry("allow")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or(IngestError::Invalid("permissions.allow must be an array"))?;
    allow.retain(|item| {
        item.as_str().is_none_or(|name| {
            !name.starts_with("mcp__kp__") || CLAUDE_MCP_ALLOWED_TOOLS.contains(&name)
        })
    });
    for tool in CLAUDE_MCP_ALLOWED_TOOLS {
        if !allow.iter().any(|item| item.as_str() == Some(tool)) {
            allow.push(Value::String(tool.to_owned()));
        }
    }
    Ok(())
}

fn resolve_mcp_json_path(client: &str, project: &Path, global: bool) -> Result<PathBuf> {
    if !global {
        return match client {
            "cursor" => Ok(project.join(".cursor/mcp.json")),
            "windsurf" => Ok(project.join(".windsurf/mcp_config.json")),
            _ => Err(IngestError::Invalid(
                "client must be claude-code, codex, cursor, or windsurf",
            )),
        };
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or(IngestError::Invalid(
            "cannot resolve HOME/USERPROFILE directory for `--global` MCP config",
        ))?;
    match client {
        "cursor" => Ok(home.join(".cursor/mcp.json")),
        "windsurf" => Ok(home.join(".codeium/windsurf/mcp_config.json")),
        _ => Err(IngestError::Invalid(
            "`--global` is supported for `cursor` and `windsurf`",
        )),
    }
}

fn configure_mcp_json(
    path: &Path,
    library: &Path,
    actor: &ActorId,
    executable: &Path,
) -> Result<()> {
    let mut config = match std::fs::File::open(path) {
        Ok(file) => serde_json::from_slice::<Value>(&crate::read_bounded(file, 1024 * 1024)?)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e.into()),
    };
    let object = config.as_object_mut().ok_or(IngestError::Invalid(
        "MCP configuration must be a JSON object",
    ))?;
    let servers = object
        .entry("mcpServers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or(IngestError::Invalid("mcpServers must be a JSON object"))?;
    servers.insert(
        "kp".to_owned(),
        json!({
            "command": executable.to_string_lossy(),
            "args": [
                "--path",
                library.to_string_lossy(),
                "mcp",
                "--as",
                actor.as_str()
            ]
        }),
    );
    write_atomic_json(path, &config)
}

fn configure_codex_toml(
    path: &Path,
    library: &Path,
    actor: &ActorId,
    executable: &Path,
) -> Result<()> {
    let existing = match std::fs::File::open(path) {
        Ok(file) => String::from_utf8(crate::read_bounded(file, 1024 * 1024)?)
            .map_err(|_| IngestError::Invalid("codex config.toml must be UTF-8"))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let tools = CODEX_MCP_ENABLED_TOOLS
        .iter()
        .map(|t| toml_basic_string(t))
        .collect::<Vec<_>>()
        .join(", ");
    let section = [
        "[mcp_servers.kp]".to_owned(),
        format!(
            "command = {}",
            toml_basic_string(&executable.to_string_lossy())
        ),
        format!(
            "args = [\"--path\", {}, \"mcp\", \"--as\", {}]",
            toml_basic_string(&library.to_string_lossy()),
            toml_basic_string(actor.as_str())
        ),
        format!("enabled_tools = [{tools}]"),
        "default_tools_approval_mode = \"approve\"".to_owned(),
    ]
    .join("\n");

    let updated = upsert_toml_section(&existing, &section);
    write_atomic_bytes(path, updated.as_bytes())
}

fn upsert_toml_section(existing: &str, section: &str) -> String {
    let lines: Vec<&str> = existing.lines().collect();
    let mut before = Vec::new();
    let mut after = Vec::new();
    let mut state = 0_u8; // 0 = before, 1 = inside [mcp_servers.kp], 2 = after
    for line in lines {
        if let Some(header) = toml_table_header(line) {
            if is_kp_mcp_table(header) {
                state = 1;
                continue;
            } else if state == 1 {
                state = 2;
            }
        }
        match state {
            0 => before.push(line),
            1 => {}
            _ => after.push(line),
        }
    }
    while before.last().is_some_and(|l| l.trim().is_empty()) {
        before.pop();
    }
    while after.first().is_some_and(|l| l.trim().is_empty()) {
        after.remove(0);
    }
    while after.last().is_some_and(|l| l.trim().is_empty()) {
        after.pop();
    }

    let mut out = String::new();
    if !before.is_empty() {
        out.push_str(&before.join("\n"));
        out.push_str("\n\n");
    }
    out.push_str(section);
    out.push('\n');
    if !after.is_empty() {
        out.push('\n');
        out.push_str(&after.join("\n"));
        out.push('\n');
    }
    out
}

fn toml_table_header(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed.starts_with('#') || !trimmed.starts_with('[') {
        return None;
    }
    let without_comment = trimmed.split('#').next().map(str::trim).unwrap_or(trimmed);
    let candidate = if let Some(inner) = without_comment
        .strip_prefix("[[")
        .and_then(|s| s.strip_suffix("]]"))
    {
        Some(inner.trim())
    } else {
        without_comment
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .map(str::trim)
    }?;
    (!candidate.is_empty() && !candidate.contains(',') && !candidate.contains('='))
        .then_some(candidate)
}

fn is_kp_mcp_table(header: &str) -> bool {
    let normalized = header.replace(['"', '\'', ' '], "");
    normalized == "mcp_servers.kp" || normalized.starts_with("mcp_servers.kp.")
}

fn toml_basic_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            c if c.is_control() => {
                let code = u32::from(c);
                out.push_str(&format!("\\u{code:04X}"));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn write_atomic_json(path: &Path, config: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(config)?;
    bytes.push(b'\n');
    write_atomic_bytes(path, &bytes)
}

fn write_atomic_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or(IngestError::Invalid("missing settings directory"))?;
    std::fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    if let Ok(metadata) = std::fs::metadata(path) {
        temp.as_file().set_permissions(metadata.permissions())?;
    }
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn owned(command: &str, event: &str) -> bool {
    let Some((program, rest)) = shell_word(command) else {
        return false;
    };
    let Some((subcommand, rest)) = shell_word(rest) else {
        return false;
    };
    Path::new(&program)
        .file_name()
        .is_some_and(|name| name == "kp")
        && subcommand == "hook"
        && shell_word(rest).is_some_and(|(name, _)| name == event)
}

// Only literal shell words establish ownership; substitutions are never evaluated.
fn shell_word(command: &str) -> Option<(String, &str)> {
    let command = command.trim_start();
    if command.is_empty() {
        return None;
    }
    let mut chars = command.char_indices();
    let mut word = String::new();
    let mut quote = None;
    while let Some((index, ch)) = chars.next() {
        match (quote, ch) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('\''), _) => word.push(ch),
            (None, '\'' | '"') => quote = Some(ch),
            (_, '\\') => {
                let (_, escaped) = chars.next()?;
                if quote == Some('"') && !matches!(escaped, '$' | '`' | '"' | '\\' | '\n') {
                    word.push('\\');
                }
                if escaped != '\n' {
                    word.push(escaped);
                }
            }
            (_, '$' | '`') => return None,
            (None, ';' | '&' | '|' | '<' | '>' | '(' | ')') => return None,
            (None, c) if c.is_whitespace() => return Some((word, &command[index..])),
            _ => word.push(ch),
        }
    }
    quote.is_none().then_some((word, ""))
}
