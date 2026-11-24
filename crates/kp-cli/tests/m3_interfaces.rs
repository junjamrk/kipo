mod support;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use kp_ingest::hooks::{estimate_tokens, MAX_PROMPT_ITEMS, MAX_PROMPT_TOKENS};
use serde_json::{json, Value};
use support::{run, success, Result};

struct Mcp {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
}
impl Mcp {
    fn start(path: &Path, actor: &str) -> Result<Self> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kp"))
            .env("KP_OFFLINE", "1")
            .env("KP_MODEL_POLICY", "host")
            .arg("--path")
            .arg(path)
            .args(["mcp", "--as", actor])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let input = child.stdin.take().ok_or("stdin")?;
        let output = BufReader::new(child.stdout.take().ok_or("stdout")?);
        Ok(Self {
            child,
            input: Some(input),
            output,
        })
    }
    fn send(&mut self, request: &Value) -> Result {
        let input = self.input.as_mut().ok_or("closed stdin")?;
        writeln!(input, "{request}")?;
        input.flush()?;
        Ok(())
    }
    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.send(&json!({"jsonrpc":"2.0","id":7,"method":method,"params":params}))?;
        let mut line = String::new();
        assert!(self.output.read_line(&mut line)? > 0);
        let response: Value = serde_json::from_str(&line)?;
        assert_eq!(response["id"], 7);
        assert_eq!(response["jsonrpc"], "2.0");
        Ok(response)
    }
    fn tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        Ok(
            self.request("tools/call", json!({"name":name,"arguments":arguments}))?["result"]
                .clone(),
        )
    }
    fn finish(&mut self) -> Result {
        drop(self.input.take());
        assert!(self.child.wait()?.success());
        let mut stderr = String::new();
        self.child
            .stderr
            .take()
            .ok_or("stderr")?
            .read_to_string(&mut stderr)?;
        assert!(stderr.is_empty(), "{stderr}");
        Ok(())
    }
}
impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn hook(path: &Path, event: &str, payload: Value) -> Result<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kp"))
        .arg("--path")
        .arg(path)
        .args(["hook", event])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("stdin")?
        .write_all(serde_json::to_string(&payload)?.as_bytes())?;
    success(child.wait_with_output()?)
}

#[test]
fn stdio_mcp_exposes_three_concise_tools_enforces_permissions_and_reports_token_overhead() -> Result
{
    let dir = tempfile::tempdir()?;
    run(dir.path(), &["init", "--owner", "alice"])?;
    run(dir.path(), &["member", "add", "bob"])?;
    run(dir.path(), &["member", "add", "carol"])?;
    let private = run(
        dir.path(),
        &[
            "remember",
            "--subject",
            "bob-private",
            "--predicate",
            "kp.code.pitfall",
            "--value",
            "private-only-content",
            "--as",
            "bob",
        ],
    )?;
    let shared = run(
        dir.path(),
        &[
            "remember",
            "--subject",
            "shared-by-bob",
            "--predicate",
            "kp.code.pitfall",
            "--value",
            "shared payment settlement rule",
            "--audience",
            "*",
            "--as",
            "bob",
        ],
    )?;
    let private_id = private["facts"][0].as_str().ok_or("fact")?;
    let shared_id = shared["facts"][0].as_str().ok_or("fact")?;

    let mut mcp = Mcp::start(dir.path(), "carol")?;
    assert_eq!(
        mcp.request("initialize", json!({"protocolVersion":"2025-03-26"}))?["result"]["serverInfo"]
            ["name"],
        "kp"
    );
    mcp.send(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))?;
    assert_eq!(mcp.request("ping", json!({}))?["result"], json!({}));

    // Verify 3-tool MCP contract: strictly kp_search, kp_note, kp_why.
    let catalog = mcp.request("tools/list", json!({}))?;
    let tools = catalog["result"]["tools"].as_array().ok_or("tools")?;
    assert_eq!(tools.len(), 3);
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert_eq!(names, vec!["kp_search", "kp_note", "kp_why"]);

    // Admin, dream, skill governance, and raw forget stay CLI-only.
    for cli_only in [
        "kp_dream",
        "kp_forget",
        "kp_remember",
        "kp_dead_end",
        "kp_skill_list",
        "kp_skill_approve",
        "kp_feedback",
        "kp_episode_record",
        "kp_verify",
    ] {
        assert_eq!(
            mcp.tool(cli_only, json!({}))?["isError"],
            true,
            "{cli_only} must not be exposed via MCP"
        );
    }

    // Explicit note / correction capture via kp_note allocates opaque fact_<uuidv7> ID and queues extraction.
    let noted = mcp.tool(
        "kp_note",
        json!({
            "text": "记住：payment 接口必须先校验幂等键再扣款 api_key=sk-proj-secret1234567890",
            "verbatim_quote": "payment 接口必须先校验幂等键再扣款",
            "is_correction": true
        }),
    )?;
    assert_eq!(noted["isError"], false);
    let note_id = noted["structuredContent"]["fact_id"]
        .as_str()
        .ok_or("note fact_id")?;
    assert!(
        note_id.starts_with("fact_"),
        "expected opaque fact_ prefix, got {note_id}"
    );
    assert_eq!(noted["structuredContent"]["is_correction"], true);
    assert_eq!(noted["structuredContent"]["queued_for_extraction"], true);

    let found = mcp.tool(
        "kp_search",
        json!({"query":"payment bob-private private-only-content","limit":100}),
    )?;
    assert!(!found.to_string().contains("private-only-content"));
    assert!(!found.to_string().contains("sk-proj-secret1234567890"));
    assert_eq!(found["isError"], false);
    assert!(!found["structuredContent"]["results"]
        .as_array()
        .ok_or("results")?
        .is_empty());

    assert_eq!(
        mcp.tool("kp_why", json!({"fact_id": note_id}))?["structuredContent"]["proof"]["fact_id"],
        note_id
    );
    assert_eq!(
        mcp.tool("kp_why", json!({"fact_id": shared_id}))?["structuredContent"]["proof"]["fact_id"],
        shared_id
    );
    // Zero existence oracle on hidden facts.
    assert_eq!(
        mcp.tool("kp_why", json!({"fact_id": private_id}))?,
        mcp.tool("kp_why", json!({"fact_id": "missing"}))?
    );
    assert_eq!(mcp.request("unknown", json!({}))?["error"]["code"], -32601);
    mcp.finish()?;

    // Exact Token Accounting Report: 3-tool MCP schema + SessionStart (0) + UserPromptSubmit (<= 250).
    let tools_json = serde_json::to_string(tools)?;
    let tools_list_bytes = tools_json.len();
    let tools_list_bpe_approx = tools_list_bytes.div_ceil(4);
    let tools_list_tokens = estimate_tokens(&tools_json);
    assert!(
        tools_list_bytes <= 700 && tools_list_tokens <= 360,
        "3-tool MCP tools/list exceeded concise token target: bytes={tools_list_bytes} upper_bound_tokens={tools_list_tokens}"
    );

    let session_start = hook(dir.path(), "session-start", json!({"prompt": "payment"}))?;
    assert_eq!(session_start, json!({}));
    let session_start_tokens = 0_usize;

    let prompt_submit = {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kp"))
            .arg("--path")
            .arg(dir.path())
            .args(["--as", "carol", "hook", "user-prompt-submit"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .ok_or("stdin")?
            .write_all(b"{\"prompt\":\"payment settlement\"}")?;
        success(child.wait_with_output()?)?
    };
    let injected_text = prompt_submit["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .ok_or("injected prompt context")?;
    let prompt_tokens = estimate_tokens(injected_text);
    let item_count = injected_text
        .lines()
        .filter(|l| *l != "<kp-context>" && *l != "</kp-context>")
        .count();
    assert!((1..=MAX_PROMPT_ITEMS).contains(&item_count));
    assert!(prompt_tokens <= MAX_PROMPT_TOKENS);
    eprintln!(
        "TOKEN_ACCOUNTING_REPORT: tools_list_bytes={tools_list_bytes} tools_list_bpe_approx={tools_list_bpe_approx} tools_list_upper_bound_tokens={tools_list_tokens} session_start_tokens={session_start_tokens} user_prompt_submit_tokens={prompt_tokens} total_upper_bound_tokens={}",
        tools_list_tokens + session_start_tokens + prompt_tokens
    );
    Ok(())
}

#[test]
fn all_six_hooks_always_exit_zero_on_errors_missing_library_and_internal_extraction() -> Result {
    let dir = tempfile::tempdir()?;
    let missing_lib = dir.path().join("does-not-exist");
    let valid_lib = dir.path().join("valid-lib");
    run(&valid_lib, &["init", "--owner", "alice"])?;
    run(
        &valid_lib,
        &[
            "remember",
            "--subject",
            "src/pay.rs",
            "--predicate",
            "kp.core.guard",
            "--value",
            "{\"hard_guard\":true,\"reason\":\"guard\"}",
            "--kind",
            "constraint",
        ],
    )?;

    for event in [
        "session-start",
        "user-prompt-submit",
        "pre-tool-use",
        "post-tool-use",
        "stop",
        "session-end",
    ] {
        // 1. Missing library fallback -> exit 0, empty JSON `{}`
        let out_missing = hook(&missing_lib, event, json!({"prompt": "anything"}))?;
        assert_eq!(out_missing, json!({}), "missing library failed on {event}");

        // 2. Invalid non-JSON stdin -> exit 0, empty JSON `{}`
        let mut bad_stdin = Command::new(env!("CARGO_BIN_EXE_kp"))
            .arg("--path")
            .arg(&valid_lib)
            .args(["hook", event])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        bad_stdin
            .stdin
            .take()
            .ok_or("stdin")?
            .write_all(b"not-valid-json{{{")?;
        assert_eq!(success(bad_stdin.wait_with_output()?)?, json!({}));

        // 3. Unknown / inactive actor -> exit 0, empty JSON `{}`
        let mut unknown_actor = Command::new(env!("CARGO_BIN_EXE_kp"))
            .arg("--path")
            .arg(&valid_lib)
            .args(["--as", "ghost_actor", "hook", event])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        unknown_actor
            .stdin
            .take()
            .ok_or("stdin")?
            .write_all(b"{\"prompt\":\"src/pay.rs\"}")?;
        assert_eq!(success(unknown_actor.wait_with_output()?)?, json!({}));

        // 4. Invalid CLI flag value on hook invocation -> still exit 0!
        let bad_flag = Command::new(env!("CARGO_BIN_EXE_kp"))
            .arg("--path")
            .arg(&valid_lib)
            .args(["--as", "invalid spaces", "hook", event])
            .output()?;
        assert_eq!(success(bad_flag)?, json!({}));

        // 5. KP_INTERNAL_EXTRACTION=1 anti-recursion guard -> immediate exit 0, empty JSON `{}`
        let mut internal = Command::new(env!("CARGO_BIN_EXE_kp"))
            .env("KP_INTERNAL_EXTRACTION", "1")
            .arg("--path")
            .arg(&valid_lib)
            .args(["hook", event])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        internal.stdin.take().ok_or("stdin")?.write_all(
            serde_json::to_string(&json!({
                "session_id": "s1",
                "prompt": "src/pay.rs",
                "tool_name": "Edit",
                "tool_input": {"file_path": "src/pay.rs"}
            }))?
            .as_bytes(),
        )?;
        assert_eq!(success(internal.wait_with_output()?)?, json!({}));
    }
    Ok(())
}

#[test]
fn anchored_cli_dead_end_verify_and_pre_tool_hard_guard_ask_work_together() -> Result {
    let dir = tempfile::tempdir()?;
    let library = dir.path().join("lib");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("src"))?;
    let path = repo.join("src/pay.rs");
    std::fs::write(&path, "fn charge_card(amount: i32) { }\n")?;
    run(&library, &["init", "--owner", "alice"])?;
    let repo_str = repo.to_str().ok_or("repo")?;
    run(
        &library,
        &[
            "remember",
            "--subject",
            "charge_card",
            "--predicate",
            "kp.code.api_contract",
            "--value",
            "idempotent",
            "--repo",
            repo_str,
            "--file",
            "src/pay.rs",
            "--symbol",
            "charge_card",
        ],
    )?;
    let dead = run(
        &library,
        &[
            "dead-end",
            "--subject",
            "charge_card",
            "--hypothesis",
            "blind retries",
            "--reason",
            "duplicate payments",
            "--repo",
            repo_str,
            "--file",
            "src/pay.rs",
            "--symbol",
            "charge_card",
        ],
    )?;

    // Soft dead-end pitfall stays silent on PreToolUse (surfaced in UserPromptSubmit instead).
    let soft_pre = hook(
        &library,
        "pre-tool-use",
        json!({"tool_name":"Edit","tool_input":{"file_path":path}}),
    )?;
    assert_eq!(soft_pre, json!({}));

    // Explicit anchored owner hard guard triggers permissionDecision: "ask" with quote & provenance.
    run(
        &library,
        &[
            "remember",
            "--subject",
            "charge_card",
            "--predicate",
            "kp.core.guard",
            "--value",
            "{\"hard_guard\":true,\"verbatim_quote\":\"编辑 charge_card 前必须核对幂等契约\"}",
            "--kind",
            "constraint",
            "--repo",
            repo_str,
            "--file",
            "src/pay.rs",
            "--symbol",
            "charge_card",
        ],
    )?;

    let pre = hook(
        &library,
        "pre-tool-use",
        json!({"tool_name":"Edit","tool_input":{"file_path":path}}),
    )?;
    assert_eq!(pre["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    assert_eq!(pre["hookSpecificOutput"]["permissionDecision"], "ask");
    let reason = pre["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .ok_or("reason")?;
    assert!(reason.contains("编辑 charge_card 前必须核对幂等契约"));
    assert!(reason.contains("kp-ref:fact_"));

    let unrelated = hook(
        &library,
        "pre-tool-use",
        json!({"tool_name":"Edit","tool_input":{"file_path":dir.path().join("other/src/pay.rs")}}),
    )?;
    assert_eq!(unrelated, json!({}));

    std::fs::write(&path, "fn charge_card(amount: i64) { }\n")?;
    let verified = run(&library, &["verify", "--repo", repo_str])?;
    assert!(verified
        .as_array()
        .ok_or("anchors")?
        .iter()
        .all(|a| a["verify_status"] == "sig_changed"));
    assert_eq!(
        run(&library, &["why", dead["facts"][0].as_str().ok_or("id")?])?["fact_id"],
        dead["facts"][0]
    );
    let failure = json!({"session_id":"s","tool_name":"Bash","tool_input":{"command":"cargo test"},"tool_response":{"exit_code":1}});
    hook(&library, "post-tool-use", failure)?;
    assert_eq!(
        hook(
            &library,
            "post-tool-use",
            json!({"session_id":"s","tool_name":"Bash","tool_input":{"command":"cargo test"},"tool_response":{"exit_code":0}})
        )?["episodes"],
        1
    );
    Ok(())
}

#[tokio::test]
async fn historical_skill_lists_are_read_only_for_owners_and_members() -> Result {
    use kp_model::{ContrastKind, EpochMs};
    use kp_store::{Episode, Store};

    let dir = tempfile::tempdir()?;
    run(dir.path(), &["init", "--owner", "alice"])?;
    run(dir.path(), &["member", "add", "bob"])?;
    let mut store = Store::open(dir.path())?;
    for session in ["first", "second"] {
        store.record_episodes(
            &[Episode {
                signal_kind: ContrastKind::HumanCorrection,
                session_id: session.into(),
                before_trace: json!("blindly retry"),
                after_trace: json!("No, use a bounded retry and verify the result."),
                step_ref: "tool:1".into(),
                environment: json!({"actor":"alice"}),
            }],
            EpochMs(90),
        )?;
    }
    let report = kp_ingest::dream::dream(
        &mut store,
        &"alice".parse()?,
        EpochMs(100),
        &kp_ingest::dream::StructuredDreamAttributor,
    )
    .await?;
    assert_eq!(report.promoted.len(), 1);
    let path = dir.path().join("skills/apply-explicit-correction/SKILL.md");
    let before = std::fs::read(&path)?;
    let marker = dir.path().join("skills/marker");
    std::fs::write(&marker, "read-only sentinel")?;
    for actor in ["alice", "bob"] {
        assert_eq!(
            run(dir.path(), &["--as", actor, "skill", "list", "--at", "0"])?,
            json!([])
        );
        assert_eq!(std::fs::read(&path)?, before);
        assert_eq!(std::fs::read_to_string(&marker)?, "read-only sentinel");
    }
    Ok(())
}

#[test]
fn episode_premises_use_visible_exact_ids_and_do_not_probe_hidden_facts_or_keys() -> Result {
    let dir = tempfile::tempdir()?;
    run(dir.path(), &["init", "--owner", "alice"])?;
    run(dir.path(), &["member", "add", "bob"])?;
    let hidden = run(
        dir.path(),
        &[
            "remember",
            "--subject",
            "HIDDEN_TARGET",
            "--predicate",
            "kp.code.pitfall",
            "--value",
            "private",
            "--logical-key",
            "hidden-key",
        ],
    )?;
    let visible = run(
        dir.path(),
        &[
            "remember",
            "--subject",
            "VISIBLE_TARGET",
            "--predicate",
            "kp.code.pitfall",
            "--value",
            "shared",
            "--audience",
            "*",
            "--logical-key",
            "visible-key",
        ],
    )?;
    let hidden_id = hidden["facts"][0].as_str().ok_or("hidden ID")?;
    let visible_id = visible["facts"][0].as_str().ok_or("visible ID")?;
    let mut cli_denials = Vec::new();
    for premise in [hidden_id, "hidden-key", "visible-key", "absent-id"] {
        let output = Command::new(env!("CARGO_BIN_EXE_kp"))
            .arg("--path")
            .arg(dir.path())
            .args([
                "--as",
                "bob",
                "episode",
                "record",
                "--kind",
                "human-correction",
                "--before",
                "old assumption",
                "--after",
                "No, use the verified assumption.",
                "--premise",
                premise,
            ])
            .output()?;
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("HIDDEN_TARGET"));
        cli_denials.push(output.stderr);
    }
    assert!(cli_denials.windows(2).all(|pair| pair[0] == pair[1]));
    let store = kp_store::Store::open(dir.path())?;
    assert!(store.pending_episodes(&"bob".parse()?)?.is_empty());
    let recorded = run(
        dir.path(),
        &[
            "--as",
            "bob",
            "episode",
            "record",
            "--kind",
            "human-correction",
            "--before",
            "old assumption",
            "--after",
            "No, use the verified assumption.",
            "--premise",
            visible_id,
        ],
    )?;
    assert_eq!(recorded["episodes"], 1);
    let pending = store
        .pending_episodes(&"bob".parse()?)?
        .pop()
        .ok_or("episode")?;
    assert_eq!(pending.episode.environment["target"], "VISIBLE_TARGET");
    assert!(!serde_json::to_string(&pending)?.contains("HIDDEN_TARGET"));
    Ok(())
}

#[test]
fn connect_codex_runs_self_check_prints_untrusted_folder_guidance_and_never_touches_codex_home(
) -> Result {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir()?;
    let lib = dir.path().join("lib");
    let proj = dir.path().join("proj");
    let temp_codex_home = dir.path().join("isolated_codex_home");
    std::fs::create_dir_all(&proj)?;
    std::fs::create_dir_all(&temp_codex_home)?;
    run(&lib, &["init", "--owner", "alice"])?;

    let proj_canonical = proj.canonicalize()?;

    // Hermetic mock `codex` matching real `codex 0.164` (`codex mcp get kp` checks `CODEX_HOME/config.toml` trust).
    let mock_codex = dir.path().join("mock-codex");
    std::fs::write(
        &mock_codex,
        r#"#!/usr/bin/env python3
import os, sys
assert sys.argv[1:4] == ["mcp", "get", "kp"], f"unexpected args: {sys.argv}"
chome = os.environ.get("CODEX_HOME", "")
cwd = os.path.realpath(os.getcwd())
cfg_path = os.path.join(chome, "config.toml")
trusted = False
if os.path.isfile(cfg_path):
    txt = open(cfg_path, encoding="utf-8").read()
    if cwd in txt and 'trust_level = "trusted"' in txt:
        trusted = True
proj_toml = os.path.join(cwd, ".codex", "config.toml")
if trusted and os.path.isfile(proj_toml) and "[mcp_servers.kp]" in open(proj_toml, encoding="utf-8").read():
    print("kp\n  enabled: true\n  enabled_tools: kp_search, kp_note, kp_why")
    sys.exit(0)
sys.stderr.write("Error: No MCP server named 'kp' found.\n")
sys.exit(1)
"#,
    )?;
    std::fs::set_permissions(&mock_codex, std::fs::Permissions::from_mode(0o755))?;

    let real_codex_available = Command::new("codex")
        .arg("--version")
        .env("CODEX_HOME", &temp_codex_home)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());

    let mut bins_to_verify: Vec<Option<&Path>> = vec![Some(&mock_codex)];
    if real_codex_available {
        bins_to_verify.push(None); // Also run against real `codex` on PATH with isolated `CODEX_HOME`!
    }

    for override_bin in bins_to_verify {
        let sub_home = temp_codex_home.join(if override_bin.is_some() {
            "mock_home"
        } else {
            "real_home"
        });
        std::fs::create_dir_all(&sub_home)?;

        // 1. Untrusted folder: exits 0, `codex_mcp_verified == false`, human output prints 2-step guidance,
        // and `sub_home/config.toml` is NEVER created or modified by `kp connect codex`!
        let mut human_cmd = Command::new(env!("CARGO_BIN_EXE_kp"));
        human_cmd
            .arg("--path")
            .arg(&lib)
            .args([
                "connect",
                "codex",
                "--project",
                proj_canonical.to_str().ok_or("proj")?,
            ])
            .env("CODEX_HOME", &sub_home);
        if let Some(bin) = override_bin {
            human_cmd.env("KP_CODEX_BIN", bin);
        }
        let human_out = human_cmd.output()?;
        assert!(human_out.status.success());
        let stdout_txt = String::from_utf8_lossy(&human_out.stdout);
        assert!(
            stdout_txt.contains("trust the directory")
                && stdout_txt.contains("Hooks need review")
                && stdout_txt.contains("Review hooks")
                && stdout_txt.contains("Trust all and continue"),
            "missing untrusted-folder instructions in stdout: {stdout_txt}"
        );
        assert!(
            !stdout_txt.contains("dangerously-bypass-hook-trust"),
            "must never suggest bypassing hook trust: {stdout_txt}"
        );
        assert!(
            !sub_home.join("config.toml").exists(),
            "kp connect codex must NEVER touch CODEX_HOME/config.toml!"
        );

        let mut json_cmd = Command::new(env!("CARGO_BIN_EXE_kp"));
        json_cmd
            .arg("--path")
            .arg(&lib)
            .args([
                "--json",
                "connect",
                "codex",
                "--project",
                proj_canonical.to_str().ok_or("proj")?,
            ])
            .env("CODEX_HOME", &sub_home);
        if let Some(bin) = override_bin {
            json_cmd.env("KP_CODEX_BIN", bin);
        }
        let unverified_json = success(json_cmd.output()?)?;
        assert_eq!(unverified_json["codex_mcp_verified"], false);
        assert_eq!(
            unverified_json["codex_check"]["next_steps"]
                .as_array()
                .map_or(0, Vec::len),
            2
        );

        // 2. Simulate the user trusting `proj_canonical` inside our isolated temporary `CODEX_HOME`
        // (never touching real `~/.codex`), then verify `kp connect codex` self-check turns `true`!
        std::fs::write(
            sub_home.join("config.toml"),
            format!(
                "[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
                proj_canonical.display()
            ),
        )?;
        let mut trusted_cmd = Command::new(env!("CARGO_BIN_EXE_kp"));
        trusted_cmd
            .arg("--path")
            .arg(&lib)
            .args([
                "--json",
                "connect",
                "codex",
                "--project",
                proj_canonical.to_str().ok_or("proj")?,
            ])
            .env("CODEX_HOME", &sub_home);
        if let Some(bin) = override_bin {
            trusted_cmd.env("KP_CODEX_BIN", bin);
        }
        let verified_json = success(trusted_cmd.output()?)?;
        assert_eq!(
            verified_json["codex_mcp_verified"], true,
            "expected verified=true once temporary CODEX_HOME trusts project: {verified_json}"
        );
    }
    Ok(())
}

#[test]
fn daemon_cli_start_status_hook_and_stop_work_across_processes_when_os_uds_permitted() -> Result {
    // Check whether OS sandbox allows cross-process AF_UNIX `connect()` (Seatbelt blocks cross-process connect;
    // in-process UDS is always verified by `integrations::tests::daemon_uds_fast_path_*`).
    let probe_sock = Path::new("/tmp").join(format!("kp-probe-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&probe_sock);
    let cross_proc_allowed = match std::os::unix::net::UnixListener::bind(&probe_sock) {
        Ok(_listener) => {
            let ok = Command::new("python3")
                .args([
                    "-c",
                    &format!(
                        "import socket; c=socket.socket(socket.AF_UNIX); c.connect({:?}); c.close()",
                        probe_sock.to_string_lossy()
                    ),
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            let _ = std::fs::remove_file(&probe_sock);
            ok
        }
        Err(_) => false,
    };
    if !cross_proc_allowed {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let lib = dir.path().join("lib");
    run(&lib, &["init", "--owner", "alice"])?;
    run(
        &lib,
        &[
            "remember",
            "--subject",
            "src/lib.rs",
            "--predicate",
            "kp.policy.guard",
            "--value",
            r#"{"hard_guard":true,"reason":"例行压测窗口禁止直接改动 lib.rs"}"#,
            "--kind",
            "constraint",
            "--valid-from",
            "0",
        ],
    )?;

    let started = run(&lib, &["daemon", "start", "--idle-secs", "1"])?;
    assert_eq!(started["status"], "running");

    let status = run(&lib, &["daemon", "status"])?;
    assert_eq!(status["facts"], 1);

    let pre = hook(
        &lib,
        "pre-tool-use",
        json!({"tool_name": "Edit", "tool_input": {"file_path": "src/lib.rs"}}),
    )?;
    assert_eq!(pre["hookSpecificOutput"]["permissionDecision"], "ask");

    let stopped = run(&lib, &["daemon", "stop"])?;
    assert_eq!(stopped["status"], "stopped");
    Ok(())
}
