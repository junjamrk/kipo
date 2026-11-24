mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::sync::{Arc, Mutex};

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use kp_model::ContentHash;
use kp_store::Store;
use serde_json::{json, Value};

use crate::support::{run, success, Result};

const CLOUD_KEY_ENVS: &[&str] = &[
    "DASHSCOPE_API_KEY",
    "QWEN_API_KEY",
    "ARK_API_KEY",
    "VOLCENGINE_API_KEY",
    "ZHIPUAI_API_KEY",
    "ZHIPU_API_KEY",
    "OPENAI_API_KEY",
    "VOYAGE_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "OPENROUTER_API_KEY",
    "ANTHROPIC_API_KEY",
];

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Mcp {
    fn start(path: &Path, actor: &str) -> Result<Self> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kp"))
            .env("KP_OFFLINE", "1")
            .env("KP_MODEL_POLICY", "host")
            .arg("--path")
            .arg(path)
            .args(["--as", actor, "mcp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        Ok(Self {
            stdin: child.stdin.take().ok_or("mcp stdin")?,
            stdout: BufReader::new(child.stdout.take().ok_or("mcp stdout")?),
            child,
            next_id: 1,
        })
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        serde_json::to_writer(
            &mut self.stdin,
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )?;
        writeln!(self.stdin)?;
        self.stdin.flush()?;
        let mut line = String::new();
        self.stdout.read_line(&mut line)?;
        Ok(serde_json::from_str(&line)?)
    }

    fn tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        Ok(
            self.request("tools/call", json!({"name": name, "arguments": arguments}))?["result"]
                .clone(),
        )
    }

    fn finish(mut self) -> Result {
        drop(self.stdin);
        let status = self.child.wait()?;
        assert!(status.success());
        Ok(())
    }
}

fn run_stdin(path: &Path, args: &[&str], stdin_bytes: &[u8]) -> Result<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kp"))
        .env("KP_OFFLINE", "1")
        .arg("--path")
        .arg(path)
        .arg("--json")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("missing stdin")?
        .write_all(stdin_bytes)?;
    success(child.wait_with_output()?)
}

fn cross_proc_uds_allowed() -> bool {
    let probe_sock = Path::new("/tmp").join(format!("kp-m2-probe-{}.sock", std::process::id()));
    let _ = fs::remove_file(&probe_sock);
    match std::os::unix::net::UnixListener::bind(&probe_sock) {
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
            let _ = fs::remove_file(&probe_sock);
            ok
        }
        Err(_) => false,
    }
}

#[tokio::test]
async fn public_ingest_three_entrypoints_and_dynamic_container_membership_work_end_to_end() -> Result
{
    let dir = tempfile::tempdir()?;
    let lib = dir.path().join("lib");

    run(&lib, &["init", "--owner", "alice", "--community"])?;
    run(&lib, &["member", "add", "bob", "--role", "member"])?;
    run(&lib, &["member", "add", "carol", "--role", "member"])?;

    // Write a deterministic mock host CLI script so `kp ingest --extract` exercises global
    // generative model auto-detection (`KP_HOST_CLI_PROGRAM`) and structured fact extraction.
    let mock_claude = dir.path().join("mock-claude");
    let mock_script = r#"#!/usr/bin/env python3
import json, re, sys
prompt = sys.stdin.read()
if "<chunk id=" not in prompt:
    print(json.dumps({"structured_output": {"verdict": "unrelated", "question": None}}))
    sys.exit(0)
m = re.search(r'<chunk id="([^"]+)"', prompt)
cid = m.group(1) if m else "unknown-0"
atoms = []
if "代号朱雀" in prompt:
    atoms.append({
        "kind": "decision",
        "claim_text": "主干构建镜像启用代号朱雀方案",
        "value_json": "{}",
        "scope": None,
        "ambiguous_scope": None,
        "condition_concept": None,
        "action_concept": None,
        "group_concept": None,
        "member_mention": None,
        "valid_dates": None,
        "speaker": "alice",
        "assumed_premise_ids": [],
        "quote": {
            "chunk_id": cid,
            "verbatim_quote": "主干构建镜像启用代号朱雀方案",
            "turn_ordinal": None,
            "approx_char_offset": None
        },
        "write_op": {"op": "add", "target_fact_id": None}
    })
print(json.dumps({"structured_output": {"skip_summary": False, "atoms": atoms}}, ensure_ascii=False))
"#;
    fs::write(&mock_claude, mock_script)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&mock_claude, fs::Permissions::from_mode(0o755))?;
    }

    // 1. Entrypoint ①: CLI `kp ingest <file> --extract` + idempotent `stdin` (`-`) replay.
    let at_base = 1_763_769_600_000_i64;
    let batch_v1_lines = [
        json!({
            "container_id": "room.infra",
            "label": "Infra Room",
            "members": ["alice", "bob"],
            "all_library_members": false,
            "synced_at": at_base
        })
        .to_string(),
        json!({
            "source": {
                "kind": "chat",
                "uri": "chat://room.infra/msg-101",
                "external_id": "msg-101",
                "container_id": "room.infra"
            },
            "speaker": "alice",
            "audience": "container_members",
            "occurred_at": at_base,
            "content": "主干构建镜像启用代号朱雀方案",
            "op": "upsert"
        })
        .to_string(),
    ]
    .join("\n");
    let batch_v1_file = dir.path().join("batch_v1.jsonl");
    fs::write(&batch_v1_file, &batch_v1_lines)?;

    let ingest_file_out = success(
        Command::new(env!("CARGO_BIN_EXE_kp"))
            .env("KP_OFFLINE", "1")
            .env("KP_HOST_CLI_PROGRAM", &mock_claude)
            .env("KP_HOST_CLI_KIND", "claude")
            .arg("--path")
            .arg(&lib)
            .args(["--as", "alice", "--json", "ingest"])
            .arg(&batch_v1_file)
            .arg("--extract")
            .output()?,
    )?;
    assert_eq!(ingest_file_out["rosters_applied"], 1);
    assert_eq!(ingest_file_out["records_upserted"], 1);
    assert_eq!(ingest_file_out["jobs_enqueued"], 1);
    assert_eq!(ingest_file_out["extraction"]["jobs_processed"], 1);
    assert_eq!(
        ingest_file_out["extraction"]["created_facts"]
            .as_array()
            .map_or(0, Vec::len),
        1
    );

    // Replaying the exact same JSONL over `stdin` (`-`) is idempotent (`records_duplicate == 1`).
    let dup_out = run_stdin(
        &lib,
        &["--as", "alice", "ingest", "-"],
        batch_v1_lines.as_bytes(),
    )?;
    assert_eq!(dup_out["records_upserted"], 0);
    assert_eq!(dup_out["records_duplicate"], 1);

    // `alice` and `bob` are in `room.infra`; `carol` is NOT yet in `room.infra`.
    let bob_v1 = run(
        &lib,
        &[
            "--as",
            "bob",
            "search",
            "代号朱雀",
            "--at",
            &(at_base + 500).to_string(),
        ],
    )?;
    assert!(
        !bob_v1["results"]
            .as_array()
            .ok_or("bob_v1 array")?
            .is_empty(),
        "bob must see room.infra v1 content and extracted decision"
    );
    let carol_before_join = run(
        &lib,
        &[
            "--as",
            "carol",
            "search",
            "代号朱雀",
            "--at",
            &(at_base + 500).to_string(),
        ],
    )?;
    assert!(
        carol_before_join["results"]
            .as_array()
            .ok_or("carol_before_join array")?
            .is_empty(),
        "carol must not see room.infra before joining container"
    );

    // 2. Entrypoint ②: MCP `kp_note` (`text` AND `jsonl`) while keeping strictly 3 concise tools.
    let mut mcp = Mcp::start(&lib, "alice")?;
    assert_eq!(
        mcp.request("initialize", json!({}))?["result"]["serverInfo"]["name"],
        "kp"
    );
    let listed = mcp.request("tools/list", json!({}))?;
    let tools = listed["result"]["tools"].as_array().ok_or("tools array")?;
    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert_eq!(tool_names, ["kp_search", "kp_note", "kp_why"]);
    let tools_bytes = serde_json::to_string(tools)?.len();
    assert!(
        tools_bytes <= 700,
        "3-tool MCP schema must stay <= 700 bytes, got {tools_bytes}"
    );

    // Empty `kp_note` (missing both `text` and `jsonl`) returns `isError: true`.
    assert_eq!(mcp.tool("kp_note", json!({}))?["isError"], true);

    // `kp_note` with `text` compiles to `PublicIngestRecord(kind="mcp_note", op="upsert")` and
    // preserves 100% compatibility with explicit Observation notes + `kp_why`.
    let text_note = mcp.tool(
        "kp_note",
        json!({
            "text": "构建产物统一启用确定性可复现打包标记",
            "verbatim_quote": "确定性可复现打包标记",
            "is_correction": true
        }),
    )?;
    assert_eq!(text_note["isError"], false);
    let note_fact_id = text_note["structuredContent"]["fact_id"]
        .as_str()
        .ok_or("note_fact_id")?
        .to_owned();
    assert!(note_fact_id.starts_with("fact_"));
    assert_eq!(
        text_note["structuredContent"]["queued_for_extraction"],
        true
    );
    let mcp_why = mcp.tool("kp_why", json!({"fact_id": note_fact_id}))?;
    assert_eq!(mcp_why["isError"], false);

    // Dynamic container membership JOIN via MCP `kp_note` (`jsonl`): `carol` joins `room.infra`
    // and immediately gains visibility to both the chunk and the container-scoped extracted fact!
    let join_carol_jsonl = json!({
        "container_id": "room.infra",
        "label": "Infra Room",
        "members": ["alice", "bob", "carol"],
        "all_library_members": false,
        "synced_at": at_base + 1_000
    })
    .to_string();
    let mcp_join = mcp.tool("kp_note", json!({"jsonl": join_carol_jsonl}))?;
    assert_eq!(mcp_join["isError"], false);
    assert_eq!(mcp_join["structuredContent"]["rosters_applied"], 1);

    let carol_after_join = run(
        &lib,
        &[
            "--as",
            "carol",
            "search",
            "代号朱雀",
            "--at",
            &(at_base + 1_500).to_string(),
        ],
    )?;
    assert!(
        carol_after_join["results"]
            .as_array()
            .ok_or("carol_after_join array")?
            .len()
            >= 2,
        "carol must immediately see both chunk and extracted decision after joining room.infra"
    );

    // Dynamic container membership LEAVE (`bob` removed) + `supersede` (`msg-101` -> `msg-102`) via MCP `kp_note` (`jsonl`).
    let leave_and_supersede_jsonl = [
        json!({
            "container_id": "room.infra",
            "label": "Infra Room",
            "members": ["alice", "carol"],
            "all_library_members": false,
            "synced_at": at_base + 2_000
        })
        .to_string(),
        json!({
            "source": {
                "kind": "chat",
                "uri": "chat://room.infra/msg-102",
                "external_id": "msg-102",
                "container_id": "room.infra"
            },
            "speaker": "alice",
            "audience": "container_members",
            "occurred_at": at_base + 2_000,
            "content": "主干构建镜像改用代号玄武方案",
            "op": "supersede",
            "previous_external_id": "msg-101"
        })
        .to_string(),
    ]
    .join("\n");
    let mcp_sup = mcp.tool("kp_note", json!({"jsonl": leave_and_supersede_jsonl}))?;
    assert_eq!(mcp_sup["isError"], false);
    assert_eq!(mcp_sup["structuredContent"]["rosters_applied"], 1);
    assert_eq!(mcp_sup["structuredContent"]["records_superseded"], 1);
    mcp.finish()?;

    // `carol` sees v2 (`代号玄武`) and NO LONGER sees retired v1 (`代号朱雀`); `bob` sees NEITHER!
    let carol_v2 = run(
        &lib,
        &[
            "--as",
            "carol",
            "search",
            "代号玄武",
            "--at",
            &(at_base + 2_500).to_string(),
        ],
    )?;
    assert_eq!(carol_v2["results"].as_array().ok_or("carol_v2")?.len(), 1);
    let carol_retired_v1 = run(
        &lib,
        &[
            "--as",
            "carol",
            "search",
            "代号朱雀",
            "--at",
            &(at_base + 2_500).to_string(),
        ],
    )?;
    assert!(
        carol_retired_v1["results"]
            .as_array()
            .ok_or("carol_retired")?
            .iter()
            .all(|r| !r["passage"].as_str().unwrap_or("").contains("朱雀")),
        "retired v1 chunk and extracted fact must vanish for carol: {carol_retired_v1}"
    );
    let bob_after_leave = run(
        &lib,
        &[
            "--as",
            "bob",
            "search",
            "代号玄武",
            "--at",
            &(at_base + 2_500).to_string(),
        ],
    )?;
    assert!(bob_after_leave["results"]
        .as_array()
        .ok_or("bob_after_leave")?
        .is_empty());

    // `withdraw` (`msg-102`) over CLI `kp ingest -` (`stdin`) retires v2 completely.
    let withdraw_v2 = json!({
        "source": {
            "kind": "chat",
            "uri": "chat://room.infra/msg-102",
            "external_id": "msg-102",
            "container_id": "room.infra"
        },
        "speaker": "alice",
        "audience": "container_members",
        "occurred_at": at_base + 3_000,
        "content": "",
        "op": "withdraw"
    })
    .to_string();
    let wd_out = run_stdin(
        &lib,
        &["--as", "alice", "ingest", "-"],
        withdraw_v2.as_bytes(),
    )?;
    assert_eq!(wd_out["records_withdrawn"], 1);

    let alice_after_wd = run(
        &lib,
        &[
            "--as",
            "alice",
            "search",
            "代号玄武",
            "--at",
            &(at_base + 3_500).to_string(),
        ],
    )?;
    assert!(
        alice_after_wd["results"]
            .as_array()
            .ok_or("alice_after_wd")?
            .iter()
            .all(|r| !r["passage"].as_str().unwrap_or("").contains("玄武")
                && !r["passage"].as_str().unwrap_or("").contains("朱雀")),
        "withdrawn v2 must vanish completely: {alice_after_wd}"
    );

    // 3. Entrypoint ③: Cross-process `kp daemon` UDS `DaemonClient::ingest_jsonl` when OS UDS is permitted
    //    (in-process UDS is also verified unconditionally in `integrations::tests::daemon_uds_ingest_jsonl_*`).
    if cross_proc_uds_allowed() {
        let started = run(&lib, &["daemon", "start", "--idle-secs", "5"])?;
        assert_eq!(started["status"], "running");

        let client = kp_net::DaemonClient::for_library(&lib)?;
        let uds_batch = [
            json!({
                "container_id": "room.uds",
                "label": "UDS Channel",
                "members": ["alice", "bob"],
                "all_library_members": false,
                "synced_at": at_base + 4_000
            })
            .to_string(),
            json!({
                "source": {
                    "kind": "chat",
                    "uri": "chat://room.uds/u-1",
                    "external_id": "u-1",
                    "container_id": "room.uds"
                },
                "speaker": "alice",
                "audience": "container_members",
                "occurred_at": at_base + 4_000,
                "content": "守护进程套接字通道验证完毕",
                "op": "upsert"
            })
            .to_string(),
        ]
        .join("\n");
        let uds_res = client
            .ingest_jsonl(kp_net::IngestJsonlRequest {
                jsonl: uds_batch,
                actor: Some("alice".parse()?),
                at: Some(kp_model::EpochMs(at_base + 4_000)),
                extract_now: false,
            })
            .await?;
        assert_eq!(uds_res.rosters_applied, 1);
        assert_eq!(uds_res.records_upserted, 1);

        let bob_uds = run(
            &lib,
            &[
                "--as",
                "bob",
                "search",
                "守护进程套接字通道",
                "--at",
                &(at_base + 4_500).to_string(),
            ],
        )?;
        assert_eq!(bob_uds["results"].as_array().ok_or("bob_uds")?.len(), 1);

        let stopped = run(&lib, &["daemon", "stop"])?;
        assert_eq!(stopped["status"], "stopped");
    }

    // 4. Hook `session-end` captures human turns through `session_subbatch_to_public_ingest_record`.
    let transcript_path = dir.path().join("session_hook.jsonl");
    fs::write(
        &transcript_path,
        concat!(
            r#"{"type":"user","timestamp":"2026-11-23T09:00:00Z","message":{"role":"user","content":"会话钩子统一走公开契约校验逻辑"}}"#,
            "\n"
        ),
    )?;
    let hook_payload = json!({
        "session_id": "sess-m2-contract",
        "transcript_path": transcript_path,
    });
    let mut hook_child = Command::new(env!("CARGO_BIN_EXE_kp"))
        .env("KP_OFFLINE", "1")
        .arg("--path")
        .arg(&lib)
        .args(["--as", "alice", "hook", "session-end"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    hook_child
        .stdin
        .take()
        .ok_or("hook stdin")?
        .write_all(serde_json::to_string(&hook_payload)?.as_bytes())?;
    let _ = success(hook_child.wait_with_output()?)?;

    let hook_hits = run(&lib, &["--as", "alice", "search", "会话钩子统一走公开契约"])?;
    let hook_arr = hook_hits["results"].as_array().ok_or("hook_hits")?;
    assert!(
        hook_arr
            .first()
            .and_then(|r| r["passage"].as_str())
            .is_some_and(|p| p.contains("会话钩子统一走公开契约校验逻辑")),
        "top hit must be the captured session turn: {hook_hits}"
    );

    Ok(())
}

#[derive(Clone)]
struct MockEmbedState {
    bodies: Arc<Mutex<Vec<Value>>>,
    auths: Arc<Mutex<Vec<String>>>,
    dims: usize,
}

async fn mock_embeddings_endpoint(
    State(state): State<MockEmbedState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        if let Ok(mut guard) = state.auths.lock() {
            guard.push(auth.to_owned());
        }
    }
    if let Ok(mut guard) = state.bodies.lock() {
        guard.push(body.clone());
    }
    let count = body["input"].as_array().map_or(0, Vec::len);
    let data: Vec<Value> = (0..count)
        .map(|idx| {
            let mut vec = vec![0.0_f64; state.dims];
            vec[idx % state.dims] = 0.6;
            vec[(idx + 1) % state.dims] = 0.8;
            json!({
                "object": "embedding",
                "index": idx,
                "embedding": vec,
            })
        })
        .collect();
    (
        StatusCode::OK,
        Json(json!({
            "object": "list",
            "data": data,
        })),
    )
}

fn clean_kp_cmd(home_dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kp"));
    cmd.env_remove("KP_OFFLINE");
    cmd.env("HOME", home_dir);
    cmd.env("KP_CONFIG_PATH", home_dir.join(".kp").join("config.json"));
    cmd.env("KP_MODELS_DIR", home_dir.join(".kp").join("models"));
    for &var in CLOUD_KEY_ENVS {
        cmd.env_remove(var);
    }
    cmd.env_remove("OLLAMA_API_KEY");
    cmd.env_remove("LLAMA_API_KEY");
    cmd.env_remove("LMSTUDIO_API_KEY");
    cmd.env_remove("OPENAI_BASE_URL");
    cmd
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_providers_and_embed_stale_enforce_recipes_dim_guard_and_raw_only_privacy() -> Result {
    let dir = tempfile::tempdir()?;
    let fake_home = dir.path().join("home");
    fs::create_dir_all(&fake_home)?;

    // 1. `kp init --no-embedding`: `none` (`0d`), zero stderr.
    let lib_none = dir.path().join("lib_none");
    let out_none: Output = clean_kp_cmd(&fake_home)
        .arg("--path")
        .arg(&lib_none)
        .args(["init", "--no-embedding", "--json"])
        .output()?;
    let status_none = success(out_none)?;
    assert_eq!(status_none["embedding_model"], "none");
    assert_eq!(status_none["embedding_dims"], 0);

    // 2. `kp init --embedding-model ollama:bge-m3`: explicit local server (`1024d`), zero cloud privacy notice.
    let lib_ollama = dir.path().join("lib_ollama");
    let out_ollama = clean_kp_cmd(&fake_home)
        .arg("--path")
        .arg(&lib_ollama)
        .args(["init", "--embedding-model", "ollama:bge-m3", "--json"])
        .output()?;
    let status_ollama = success(out_ollama)?;
    assert_eq!(status_ollama["embedding_model"], "ollama:bge-m3");
    assert_eq!(status_ollama["embedding_dims"], 1024);

    // 3. `kp init` with `DASHSCOPE_API_KEY` set: auto-detects `dashscope:text-embedding-v3` (`1024d`),
    //    writes privacy notice ONLY to `stderr`, keeping `--json` `stdout` 100% clean JSON!
    let lib_cloud = dir.path().join("lib_cloud");
    let out_cloud = clean_kp_cmd(&fake_home)
        .env("DASHSCOPE_API_KEY", "sk-dash-demo")
        .arg("--path")
        .arg(&lib_cloud)
        .args(["init", "--json"])
        .output()?;
    assert!(out_cloud.status.success());
    let status_cloud: Value = serde_json::from_slice(&out_cloud.stdout)?;
    assert_eq!(
        status_cloud["embedding_model"],
        "dashscope:text-embedding-v3"
    );
    assert_eq!(status_cloud["embedding_dims"], 1024);
    let cloud_stderr = String::from_utf8_lossy(&out_cloud.stderr);
    assert!(
        cloud_stderr.contains("dashscope:text-embedding-v3"),
        "expected cloud privacy notice on stderr, got: {cloud_stderr}"
    );

    // 4. `kp init` with ONLY `OLLAMA_API_KEY` + `LLAMA_API_KEY` (no cloud keys):
    //    explicit local servers NEVER auto-trigger; defaults to `local:bge-small-zh-v1.5` (`512d`)
    //    without blocking on network downloads!
    let lib_local = dir.path().join("lib_local");
    let out_local = clean_kp_cmd(&fake_home)
        .env("OLLAMA_API_KEY", "ollama-tok")
        .env("LLAMA_API_KEY", "llama-tok")
        .arg("--path")
        .arg(&lib_local)
        .args(["init", "--json"])
        .output()?;
    assert!(out_local.status.success());
    let status_local: Value = serde_json::from_slice(&out_local.stdout)?;
    assert_eq!(status_local["embedding_model"], "local:bge-small-zh-v1.5");
    assert_eq!(status_local["embedding_dims"], 512);
    let local_stderr = String::from_utf8_lossy(&out_local.stderr);
    assert!(
        local_stderr.contains("local:bge-small-zh-v1.5"),
        "expected non-blocking local pending-download notice on stderr, got: {local_stderr}"
    );

    // 5. `kp providers list` & `kp providers env <id>` & `kp providers test --model hash:v1`.
    let plist = run(&lib_none, &["providers", "list"])?;
    let providers = plist["providers"].as_array().ok_or("providers array")?;
    let ids: Vec<&str> = providers.iter().filter_map(|p| p["id"].as_str()).collect();
    for expected in [
        "local",
        "dashscope",
        "volcengine",
        "zhipu",
        "openai",
        "voyage",
        "google",
        "openrouter",
        "ollama",
        "lmstudio",
        "llama-server",
    ] {
        assert!(ids.contains(&expected), "missing provider `{expected}`");
    }
    let dash_entry = providers
        .iter()
        .find(|p| p["id"] == "dashscope")
        .ok_or("dashscope entry")?;
    assert_eq!(dash_entry["max_batch_items"], 10);
    let volc_entry = providers
        .iter()
        .find(|p| p["id"] == "volcengine")
        .ok_or("volcengine entry")?;
    assert_eq!(volc_entry["send_dimensions_param"], false);
    for local_srv in ["ollama", "lmstudio", "llama-server"] {
        let srv = providers
            .iter()
            .find(|p| p["id"] == local_srv)
            .ok_or(local_srv)?;
        assert_eq!(srv["auto_detect_env"], false);
    }

    let env_dash = run(&lib_none, &["providers", "env", "dashscope"])?;
    assert_eq!(env_dash["auth_env"], json!(["DASHSCOPE_API_KEY"]));
    assert_eq!(
        env_dash["config_override_key"],
        "provider_base_urls.dashscope"
    );
    let env_volc = run(&lib_none, &["providers", "env", "volcengine"])?;
    assert_eq!(env_volc["auth_env"], json!(["ARK_API_KEY"]));
    let env_openai = run(&lib_none, &["providers", "env", "openai"])?;
    assert_eq!(env_openai["gateway_env"], "OPENAI_BASE_URL");

    let test_hash = run(&lib_none, &["providers", "test", "--model", "hash:v1"])?;
    assert_eq!(test_hash["returned_dims"], 256);

    // 6. Unconditional `RawOnly` zero-cloud-leakage verification (runs even without TCP loopback!):
    //    A library containing ONLY `RawOnly` records migrated via `--to dashscope:text-embedding-v3`
    //    pointed at an unreachable port (`127.0.0.1:9`) succeeds with `skipped_raw_only == 1` and
    //    ZERO network requests!
    let lib_raw_guard = dir.path().join("lib_raw_guard");
    run(&lib_raw_guard, &["init", "--owner", "alice"])?;
    let raw_only_line = json!({
        "source": {
            "kind": "vault",
            "uri": "vault://local/secret-1",
            "external_id": "secret-1"
        },
        "speaker": "alice",
        "audience": {"explicit": ["alice"]},
        "occurred_at": 1_763_769_600_000_i64,
        "content": "RAW_ONLY_ZERO_CLOUD_SENTINEL_本地机密仅限离线保存",
        "op": "upsert",
        "model_policy": {"mode": "raw_only"}
    })
    .to_string();
    run_stdin(
        &lib_raw_guard,
        &["--as", "alice", "ingest", "-"],
        raw_only_line.as_bytes(),
    )?;

    let raw_migrate_out = success(
        clean_kp_cmd(&fake_home)
            .env("DASHSCOPE_API_KEY", "sk-test-never-used")
            .env("DASHSCOPE_BASE_URL", "http://127.0.0.1:9/v1")
            .arg("--path")
            .arg(&lib_raw_guard)
            .args([
                "--json",
                "embed",
                "--stale",
                "--to",
                "dashscope:text-embedding-v3",
                "--dim",
                "64",
            ])
            .output()?,
    )?;
    assert_eq!(raw_migrate_out["chunks_reembedded"], 0);
    assert_eq!(raw_migrate_out["skipped_raw_only"], 1);

    // 7. Live `axum` OpenAI-compatible mock server testing `kp providers test` (`dashscope` +
    //    `volcengine` `ep-*` + dimension mismatch repair hint) and `kp embed --stale` batching
    //    (`max_batch_items=10` -> `10 + 2`) + `RawOnly` exclusion when TCP loopback is permitted.
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    let addr = listener.local_addr()?;
    let mock_base_url = format!("http://{addr}/v1");

    let recorded_bodies = Arc::new(Mutex::new(Vec::<Value>::new()));
    let recorded_auths = Arc::new(Mutex::new(Vec::<String>::new()));
    let state = MockEmbedState {
        bodies: recorded_bodies.clone(),
        auths: recorded_auths.clone(),
        dims: 64,
    };
    let app = Router::new()
        .route("/v1/embeddings", post(mock_embeddings_endpoint))
        .with_state(state);
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    // 7a. `kp providers test --model dashscope:text-embedding-v3 --dim 64`
    let probe_dash = success(
        clean_kp_cmd(&fake_home)
            .env("DASHSCOPE_API_KEY", "dash-live-token")
            .env("DASHSCOPE_BASE_URL", &mock_base_url)
            .arg("--path")
            .arg(&lib_none)
            .args([
                "--json",
                "providers",
                "test",
                "--model",
                "dashscope:text-embedding-v3",
                "--dim",
                "64",
            ])
            .output()?,
    )?;
    assert_eq!(probe_dash["returned_dims"], 64);

    // 7b. `kp providers test --model volcengine:ep-20261123-ark` omits `dimensions` in request body!
    let probe_volc = success(
        clean_kp_cmd(&fake_home)
            .env("ARK_API_KEY", "ark-live-token")
            .env("VOLCENGINE_BASE_URL", &mock_base_url)
            .arg("--path")
            .arg(&lib_none)
            .args([
                "--json",
                "providers",
                "test",
                "--model",
                "volcengine:ep-20261123-ark",
            ])
            .output()?,
    )?;
    assert_eq!(probe_volc["returned_dims"], 64);

    // 7c. Dimension mismatch (`--dim 128` vs server `64d`) fails and suggests `kp embed --stale --to ... --dim 64`.
    let mismatch_out = clean_kp_cmd(&fake_home)
        .env("DASHSCOPE_API_KEY", "dash-live-token")
        .env("DASHSCOPE_BASE_URL", &mock_base_url)
        .arg("--path")
        .arg(&lib_none)
        .args([
            "--json",
            "providers",
            "test",
            "--model",
            "dashscope:text-embedding-v3",
            "--dim",
            "128",
        ])
        .output()?;
    assert!(!mismatch_out.status.success());
    let mismatch_stderr = String::from_utf8_lossy(&mismatch_out.stderr);
    assert!(
        mismatch_stderr.contains("kp embed --stale --to dashscope:text-embedding-v3 --dim 64"),
        "expected actionable dimension repair hint, got: {mismatch_stderr}"
    );

    // 7d. `kp embed --stale --to dashscope:text-embedding-v3 --dim 64` on 12 normal records + 1 `RawOnly` record.
    let lib_batch = dir.path().join("lib_batch");
    run(&lib_batch, &["init", "--owner", "alice"])?;
    let mut batch_lines = Vec::new();
    for i in 0..12 {
        batch_lines.push(
            json!({
                "source": {
                    "kind": "doc",
                    "uri": format!("doc://handbook/sec-{i}"),
                    "external_id": format!("sec-{i}")
                },
                "speaker": "alice",
                "audience": {"explicit": ["alice"]},
                "occurred_at": 1_763_769_600_000_i64 + i,
                "content": format!("公开架构手册章节编号 {i}：支持流式增量向量索引"),
                "op": "upsert",
                "model_policy": {"mode": "host_cli"}
            })
            .to_string(),
        );
    }
    batch_lines.push(
        json!({
            "source": {
                "kind": "vault",
                "uri": "vault://private/raw-item",
                "external_id": "raw-item"
            },
            "speaker": "alice",
            "audience": {"explicit": ["alice"]},
            "occurred_at": 1_763_769_600_999_i64,
            "content": "NEVER_SEND_TO_HTTP_EMBEDDER_RAW_SENTINEL",
            "op": "upsert",
            "model_policy": {"mode": "raw_only"}
        })
        .to_string(),
    );
    run_stdin(
        &lib_batch,
        &["--as", "alice", "ingest", "-"],
        batch_lines.join("\n").as_bytes(),
    )?;

    let before_count = recorded_bodies.lock().map_err(|_| "mutex poisoned")?.len();
    let reembed_out = success(
        clean_kp_cmd(&fake_home)
            .env("DASHSCOPE_API_KEY", "dash-live-token")
            .env("DASHSCOPE_BASE_URL", &mock_base_url)
            .arg("--path")
            .arg(&lib_batch)
            .args([
                "--json",
                "embed",
                "--stale",
                "--to",
                "dashscope:text-embedding-v3",
                "--dim",
                "64",
            ])
            .output()?,
    )?;
    assert_eq!(reembed_out["chunks_reembedded"], 12);
    assert_eq!(reembed_out["skipped_raw_only"], 1);

    {
        let bodies = recorded_bodies.lock().map_err(|_| "mutex poisoned")?;
        // 1st call (7a DashScope test): sends `"dimensions": 64`
        assert_eq!(bodies[0]["dimensions"], json!(64));
        // 2nd call (7b Volcengine `ep-*` test): omits `"dimensions"`!
        assert!(bodies[1].get("dimensions").is_none());
        // `reembed` of 12 items under DashScope `max_batch_items = 10` splits into 2 requests (`10` + `2`)!
        let reembed_calls = &bodies[before_count..];
        assert_eq!(reembed_calls.len(), 2);
        assert_eq!(reembed_calls[0]["input"].as_array().map_or(0, Vec::len), 10);
        assert_eq!(reembed_calls[1]["input"].as_array().map_or(0, Vec::len), 2);
        for req_body in bodies.iter() {
            assert!(
                !req_body
                    .to_string()
                    .contains("NEVER_SEND_TO_HTTP_EMBEDDER_RAW_SENTINEL"),
                "RawOnly source text MUST NEVER be sent to HTTP embedding endpoint!"
            );
        }
    }
    {
        let auths = recorded_auths.lock().map_err(|_| "mutex poisoned")?;
        assert_eq!(auths[0], "Bearer dash-live-token");
        assert_eq!(auths[1], "Bearer ark-live-token");
    }

    // Subsequent `kp search` against `lib_batch` uses the ready `dashscope` embedder seamlessly!
    let search_live = success(
        clean_kp_cmd(&fake_home)
            .env("DASHSCOPE_API_KEY", "dash-live-token")
            .env("DASHSCOPE_BASE_URL", &mock_base_url)
            .arg("--path")
            .arg(&lib_batch)
            .args(["--json", "search", "流式增量向量索引"])
            .output()?,
    )?;
    assert!(!search_live["results"]
        .as_array()
        .ok_or("search_live")?
        .is_empty());

    server.abort();
    Ok(())
}

#[test]
fn export_markdown_directory_enforces_strict_viewer_isolation_and_records_projection_pages(
) -> Result {
    let dir = tempfile::tempdir()?;
    let lib = dir.path().join("lib");
    let export_at = 1_763_769_900_000_i64;

    run(&lib, &["init", "--owner", "alice", "--community"])?;
    run(&lib, &["member", "add", "bob", "--role", "member"])?;

    // 1. Alice private fact (`alice` only).
    run(
        &lib,
        &[
            "--as",
            "alice",
            "remember",
            "--subject",
            "personal.diary",
            "--predicate",
            "note",
            "--value",
            r#""PRIVATE_ALICE_ONLY_SENTINEL_771""#,
            "--audience",
            "alice",
            "--valid-from",
            "0",
        ],
    )?;

    // 2. Shared fact (`alice,bob`).
    run(
        &lib,
        &[
            "--as",
            "alice",
            "remember",
            "--subject",
            "release.window",
            "--predicate",
            "schedule",
            "--value",
            r#""SHARED_RELEASE_WINDOW_FRIDAY_20H""#,
            "--audience",
            "alice,bob",
            "--valid-from",
            "0",
        ],
    )?;

    // 3. Two containers via `kp ingest`: `room.shared` (`alice`, `bob`) and `room.restricted` (`alice` only).
    let containers_jsonl = [
        json!({
            "container_id": "room.shared",
            "label": "Shared Engineering",
            "members": ["alice", "bob"],
            "all_library_members": false,
            "synced_at": export_at - 2_000
        })
        .to_string(),
        json!({
            "source": {
                "kind": "chat",
                "uri": "chat://room.shared/s-1",
                "external_id": "s-1",
                "container_id": "room.shared"
            },
            "speaker": "alice",
            "audience": "container_members",
            "occurred_at": export_at - 2_000,
            "content": "SHARED_CONTAINER_BULLETIN_882",
            "op": "upsert"
        })
        .to_string(),
        json!({
            "container_id": "room.restricted",
            "label": "Restricted Audit",
            "members": ["alice"],
            "all_library_members": false,
            "synced_at": export_at - 1_000
        })
        .to_string(),
        json!({
            "source": {
                "kind": "chat",
                "uri": "chat://room.restricted/r-1",
                "external_id": "r-1",
                "container_id": "room.restricted"
            },
            "speaker": "alice",
            "audience": "container_members",
            "occurred_at": export_at - 1_000,
            "content": "UNAUTHORIZED_CONTAINER_SENTINEL_993",
            "op": "upsert"
        })
        .to_string(),
    ]
    .join("\n");
    run_stdin(
        &lib,
        &["--as", "alice", "ingest", "-"],
        containers_jsonl.as_bytes(),
    )?;

    // 4. Export for `bob`: sees shared fact + `room.shared`, ZERO leakage of `alice`-private or `room.restricted`!
    let bob_dir = dir.path().join("export_bob");
    let bob_report = run(
        &lib,
        &[
            "--as",
            "bob",
            "export",
            bob_dir.to_str().ok_or("bob_dir")?,
            "--at",
            &export_at.to_string(),
        ],
    )?;
    assert_eq!(bob_report["viewer"], "bob");
    assert_eq!(bob_report["visible_facts"], 1);
    assert_eq!(bob_report["visible_sources"], 1);
    assert!(!bob_report["pages"].as_array().ok_or("pages")?.is_empty());

    let mut bob_all_md = String::new();
    for entry in fs::read_dir(&bob_dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|s| s.to_str()) == Some("md") {
            bob_all_md.push_str(&fs::read_to_string(entry.path())?);
            bob_all_md.push('\n');
        }
    }
    assert!(bob_all_md.contains("viewer: bob"));
    assert!(bob_all_md.contains("SHARED_RELEASE_WINDOW_FRIDAY_20H"));
    assert!(bob_all_md.contains("SHARED_CONTAINER_BULLETIN_882"));
    assert!(bob_all_md.contains("[kp-ref:fact_"));
    assert!(
        !bob_all_md.contains("PRIVATE_ALICE_ONLY_SENTINEL_771"),
        "bob export leaked alice private fact!"
    );
    assert!(
        !bob_all_md.contains("UNAUTHORIZED_CONTAINER_SENTINEL_993"),
        "bob export leaked unauthorized container source!"
    );

    // 5. Export for `alice`: sees both private and restricted items.
    let alice_dir = dir.path().join("export_alice");
    let alice_report = run(
        &lib,
        &[
            "--as",
            "alice",
            "export",
            alice_dir.to_str().ok_or("alice_dir")?,
            "--at",
            &export_at.to_string(),
        ],
    )?;
    assert_eq!(alice_report["viewer"], "alice");
    assert_eq!(alice_report["visible_facts"], 2);
    assert_eq!(alice_report["visible_sources"], 2);

    let mut alice_all_md = String::new();
    for entry in fs::read_dir(&alice_dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|s| s.to_str()) == Some("md") {
            alice_all_md.push_str(&fs::read_to_string(entry.path())?);
            alice_all_md.push('\n');
        }
    }
    assert!(alice_all_md.contains("PRIVATE_ALICE_ONLY_SENTINEL_771"));
    assert!(alice_all_md.contains("UNAUTHORIZED_CONTAINER_SENTINEL_993"));

    // 6. Non-member (`eve`) is denied permission and writes no Markdown pages.
    let eve_dir = dir.path().join("export_eve");
    let eve_out = Command::new(env!("CARGO_BIN_EXE_kp"))
        .env("KP_OFFLINE", "1")
        .arg("--path")
        .arg(&lib)
        .args([
            "--as",
            "eve",
            "--json",
            "export",
            eve_dir.to_str().ok_or("eve_dir")?,
        ])
        .output()?;
    assert!(!eve_out.status.success());

    // 7. Verify `projection_pages` baselines persisted in SQLite match on-disk SHA-256 digests!
    let ro = Store::open_readonly(&lib)?;
    let bob_index_meta = ro
        .projection_page("index", &"bob".parse()?)?
        .ok_or("missing bob index projection_page")?;
    let bob_index_bytes = fs::read(bob_dir.join("index.md"))?;
    assert_eq!(
        bob_index_meta.page_sha256,
        ContentHash::digest(&bob_index_bytes)
    );
    assert!(bob_index_meta.base_seq > 0);

    let alice_index_meta = ro
        .projection_page("index", &"alice".parse()?)?
        .ok_or("missing alice index projection_page")?;
    let alice_index_bytes = fs::read(alice_dir.join("index.md"))?;
    assert_eq!(
        alice_index_meta.page_sha256,
        ContentHash::digest(&alice_index_bytes)
    );
    assert_ne!(bob_index_meta.page_sha256, alice_index_meta.page_sha256);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hook_session_withdraws_via_public_ingest_and_raw_only_suppresses_cloud_embedder_and_jobs(
) -> Result {
    let dir = tempfile::tempdir()?;
    let fake_home = dir.path().join("home");
    fs::create_dir_all(&fake_home)?;

    // Part A: Hook session capture shares the exact Public Ingest Contract identity so public
    // `kp ingest` with `op: "withdraw"` withdraws the Hook-captured session source AND its
    // extracted facts directly!
    let lib_contract = dir.path().join("lib_contract");
    run(&lib_contract, &["init", "--owner", "alice"])?;

    let mock_hook_cli = dir.path().join("mock-hook-claude");
    fs::write(
        &mock_hook_cli,
        r#"#!/usr/bin/env python3
import json, re, sys
prompt = sys.stdin.read()
if "<chunk id=" not in prompt:
    print(json.dumps({"structured_output": {"verdict": "unrelated", "question": None}}))
    sys.exit(0)
m = re.search(r'<chunk id="([^"]+)"', prompt)
cid = m.group(1) if m else "c-0"
atoms = [{
    "kind": "decision",
    "claim_text": "HOOK_PUBLIC_WITHDRAW_TARGET_可被公开契约撤回",
    "value_json": "{}",
    "scope": None,
    "ambiguous_scope": None,
    "condition_concept": None,
    "action_concept": None,
    "group_concept": None,
    "member_mention": None,
    "valid_dates": None,
    "speaker": "alice",
    "assumed_premise_ids": [],
    "quote": {
        "chunk_id": cid,
        "verbatim_quote": "HOOK_PUBLIC_WITHDRAW_TARGET_可被公开契约撤回",
        "turn_ordinal": None,
        "approx_char_offset": None
    },
    "write_op": {"op": "add", "target_fact_id": None}
}]
print(json.dumps({"structured_output": {"skip_summary": False, "atoms": atoms}}, ensure_ascii=False))
"#,
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&mock_hook_cli, fs::Permissions::from_mode(0o755))?;
    }

    let transcript = dir.path().join("withdraw_me.jsonl");
    let turn_line = concat!(
        r#"{"type":"user","timestamp":"2026-11-22T09:00:00Z","message":{"role":"user","content":"HOOK_PUBLIC_WITHDRAW_TARGET_可被公开契约撤回"}}"#,
        "\n"
    );
    fs::write(&transcript, turn_line)?;
    let byte_len = u64::try_from(turn_line.len())?;

    let mut hook_cmd = Command::new(env!("CARGO_BIN_EXE_kp"))
        .env("KP_OFFLINE", "1")
        .env("KP_HOST_CLI_PROGRAM", &mock_hook_cli)
        .env("KP_NO_AUTO_DAEMON", "1")
        .arg("--path")
        .arg(&lib_contract)
        .args(["--as", "alice", "hook", "session-end"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    hook_cmd.stdin.take().ok_or("stdin")?.write_all(
        serde_json::to_vec(&json!({
            "session_id": "sess-withdraw-demo",
            "transcript_path": transcript,
        }))?
        .as_slice(),
    )?;
    let hook_out = success(hook_cmd.wait_with_output()?)?;
    assert_eq!(hook_out["captured"], true);

    let ext_hook = run(
        &lib_contract,
        &[
            "--as",
            "alice",
            "extract",
            "--runner",
            "claude-code",
            "--program",
            mock_hook_cli.to_str().ok_or("mock_hook_cli")?,
        ],
    )?;
    assert_eq!(ext_hook["jobs_processed"], 1);
    assert_eq!(ext_hook["created_facts"].as_array().map_or(0, Vec::len), 1);

    let eval_at_str = "1795400000000";
    let before_wd = run(
        &lib_contract,
        &[
            "--as",
            "alice",
            "search",
            "HOOK_PUBLIC_WITHDRAW_TARGET",
            "--at",
            eval_at_str,
        ],
    )?;
    assert!(before_wd["results"]
        .as_array()
        .ok_or("results")?
        .iter()
        .any(|r| r["passage"]
            .as_str()
            .is_some_and(|p| p.contains("HOOK_PUBLIC_WITHDRAW_TARGET"))));

    let formatted_body = "[speaker:alice] HOOK_PUBLIC_WITHDRAW_TARGET_可被公开契约撤回\n";
    let body_hash = ContentHash::digest(formatted_body);
    let expected_rec = kp_ingest::hooks::session_subbatch_to_public_ingest_record(
        "sess-withdraw-demo",
        &"alice".parse()?,
        0,
        byte_len,
        formatted_body,
        &body_hash,
        kp_model::EpochMs(1_795_338_000_000),
    )?;
    let withdraw_jsonl = json!({
        "source": {
            "kind": "session",
            "uri": expected_rec.source.uri,
            "external_id": expected_rec.source.external_id,
        },
        "speaker": "alice",
        "audience": {"explicit": ["alice"]},
        "occurred_at": 1_795_390_000_000_i64,
        "content": "",
        "op": "withdraw"
    })
    .to_string();
    let wd_res = run_stdin(
        &lib_contract,
        &["--as", "alice", "ingest", "-"],
        withdraw_jsonl.as_bytes(),
    )?;
    assert_eq!(wd_res["records_withdrawn"], 1);

    let after_wd = run(
        &lib_contract,
        &[
            "--as",
            "alice",
            "search",
            "HOOK_PUBLIC_WITHDRAW_TARGET",
            "--at",
            eval_at_str,
        ],
    )?;
    assert!(
        after_wd["results"]
            .as_array()
            .ok_or("results")?
            .iter()
            .all(|r| !r["passage"]
                .as_str()
                .unwrap_or("")
                .contains("HOOK_PUBLIC_WITHDRAW_TARGET")),
        "public withdraw must retire Hook-captured session source: {after_wd}"
    );

    // Part B: When global generative model is `RawOnly` and library uses a cloud HTTP
    // `ProviderEmbedder`, BOTH Hook session capture AND MCP `kp_note` `text` make 0 cloud
    // embedding HTTP calls and enqueue 0 extraction jobs!
    let embed_bodies = Arc::new(Mutex::new(Vec::<Value>::new()));
    let (mock_embed_url, _server_guard) = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => {
            let addr = listener.local_addr()?;
            let state = MockEmbedState {
                bodies: embed_bodies.clone(),
                auths: Arc::new(Mutex::new(Vec::new())),
                dims: 64,
            };
            let app = Router::new()
                .route("/v1/embeddings", post(mock_embeddings_endpoint))
                .with_state(state);
            let srv = tokio::spawn(async move { axum::serve(listener, app).await });
            (format!("http://{addr}/v1"), Some(srv))
        }
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            // Even when loopback bind is blocked, pointing at closed port `127.0.0.1:9` proves
            // 0 outbound HTTP calls are attempted (any HTTP attempt would fail!).
            ("http://127.0.0.1:9/v1".to_owned(), None)
        }
        Err(err) => return Err(err.into()),
    };

    let lib_raw = dir.path().join("lib_raw_cloud_emb");
    let init_out = clean_kp_cmd(&fake_home)
        .env("DASHSCOPE_API_KEY", "sk-dash-raw-check")
        .env("DASHSCOPE_BASE_URL", &mock_embed_url)
        .arg("--path")
        .arg(&lib_raw)
        .args([
            "init",
            "--owner",
            "alice",
            "--embedding-model",
            "dashscope:text-embedding-v3",
            "--embedding-dimensions",
            "64",
            "--json",
        ])
        .output()?;
    assert!(init_out.status.success());

    let raw_tp = dir.path().join("raw_session.jsonl");
    fs::write(
        &raw_tp,
        concat!(
            r#"{"type":"user","timestamp":"2026-11-23T10:00:00Z","message":{"role":"user","content":"RAW_ONLY_HOOK_NO_CLOUD_EMBED"}}"#,
            "\n"
        ),
    )?;
    let mut raw_hook = clean_kp_cmd(&fake_home)
        .env("DASHSCOPE_API_KEY", "sk-dash-raw-check")
        .env("DASHSCOPE_BASE_URL", &mock_embed_url)
        .env("KP_MODEL_POLICY", "raw_only")
        .env("KP_NO_AUTO_DAEMON", "1")
        .arg("--path")
        .arg(&lib_raw)
        .args(["--as", "alice", "hook", "session-end"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    raw_hook.stdin.take().ok_or("stdin")?.write_all(
        serde_json::to_vec(&json!({
            "session_id": "sess-raw-zero-cloud",
            "transcript_path": raw_tp,
        }))?
        .as_slice(),
    )?;
    let raw_hook_res = success(raw_hook.wait_with_output()?)?;
    assert_eq!(raw_hook_res["captured"], true);

    let mut raw_mcp_child = clean_kp_cmd(&fake_home)
        .env("DASHSCOPE_API_KEY", "sk-dash-raw-check")
        .env("DASHSCOPE_BASE_URL", &mock_embed_url)
        .env("KP_MODEL_POLICY", "raw_only")
        .arg("--path")
        .arg(&lib_raw)
        .args(["--as", "alice", "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut mcp_in = raw_mcp_child.stdin.take().ok_or("mcp stdin")?;
    let mut mcp_out = BufReader::new(raw_mcp_child.stdout.take().ok_or("mcp stdout")?);
    writeln!(
        mcp_in,
        "{}",
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "kp_note",
                "arguments": {
                    "text": "RAW_ONLY_MCP_NOTE_NO_CLOUD_EMBED"
                }
            }
        })
    )?;
    mcp_in.flush()?;
    let mut resp_line = String::new();
    mcp_out.read_line(&mut resp_line)?;
    drop(mcp_in);
    let _ = raw_mcp_child.wait()?;

    let mcp_resp: Value = serde_json::from_str(&resp_line)?;
    assert_eq!(mcp_resp["result"]["isError"], false);
    assert_eq!(
        mcp_resp["result"]["structuredContent"]["queued_for_extraction"],
        false
    );

    assert_eq!(
        embed_bodies.lock().map_err(|_| "lock")?.len(),
        0,
        "global RawOnly must make 0 cloud embedding HTTP calls for Hook and MCP note!"
    );
    let ro_raw = Store::open_readonly(&lib_raw)?;
    let (_, pending_jobs) = ro_raw.daemon_queue_stats()?;
    assert_eq!(
        pending_jobs, 0,
        "global RawOnly must enqueue 0 extraction jobs for Hook and MCP note!"
    );
    assert_eq!(ro_raw.status()?.sources, 2);

    if let Some(srv) = _server_guard {
        srv.abort();
    }
    Ok(())
}

#[derive(Clone)]
struct MockChatLlmState {
    requests: Arc<Mutex<Vec<Value>>>,
}

async fn mock_chat_completions_endpoint(
    State(state): State<MockChatLlmState>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if let Ok(mut guard) = state.requests.lock() {
        guard.push(body.clone());
    }
    let prompt = body["messages"]
        .as_array()
        .and_then(|arr| arr.last())
        .and_then(|m| m["content"].as_str())
        .unwrap_or("");

    let content_json = if let Some(after) = prompt.split("<chunk id=\"").nth(1) {
        let cid = after.split('"').next().unwrap_or("c-0");
        let mut atoms = Vec::new();
        for sentinel in [
            "GLOBAL_ROUTE_SENTINEL_101",
            "CORP_GW_SENTINEL_202",
            "FALLBACK_OVERRIDE_SENTINEL_808",
        ] {
            if prompt.contains(sentinel) {
                atoms.push(json!({
                    "kind": "decision",
                    "claim_text": sentinel,
                    "value_json": "{}",
                    "scope": null,
                    "ambiguous_scope": null,
                    "condition_concept": null,
                    "action_concept": null,
                    "group_concept": null,
                    "member_mention": null,
                    "valid_dates": null,
                    "speaker": null,
                    "assumed_premise_ids": [],
                    "quote": {
                        "chunk_id": cid,
                        "verbatim_quote": sentinel,
                        "turn_ordinal": null,
                        "approx_char_offset": null
                    },
                    "write_op": {"op": "add", "target_fact_id": null}
                }));
            }
        }
        if prompt.contains("CORP_ALIGN_CONSTRAINT_701") {
            atoms.push(json!({
                "kind": "constraint",
                "claim_text": "在核心账务冻结期禁止合并结算变更",
                "value_json": "{}",
                "scope": {
                    "scope": "speaker_self",
                    "mentions": null,
                    "group_concept": null
                },
                "ambiguous_scope": null,
                "condition_concept": {
                    "ref_type": "propose_new",
                    "concept_id": null,
                    "kind": "condition",
                    "label": "核心账务冻结期",
                    "domain": "finance",
                    "polarity": "neutral"
                },
                "action_concept": {
                    "ref_type": "propose_new",
                    "concept_id": null,
                    "kind": "action",
                    "label": "合并结算变更",
                    "domain": "finance",
                    "polarity": "negative"
                },
                "group_concept": null,
                "member_mention": null,
                "valid_dates": null,
                "speaker": null,
                "assumed_premise_ids": [],
                "quote": {
                    "chunk_id": cid,
                    "verbatim_quote": "CORP_ALIGN_CONSTRAINT_701",
                    "turn_ordinal": null,
                    "approx_char_offset": null
                },
                "write_op": {"op": "add", "target_fact_id": null}
            }));
        }
        if prompt.contains("CORP_ALIGN_EVENT_702") {
            atoms.push(json!({
                "kind": "event",
                "claim_text": "今晚启动月度总账封账窗口",
                "value_json": "{}",
                "scope": null,
                "ambiguous_scope": null,
                "condition_concept": {
                    "ref_type": "propose_new",
                    "concept_id": null,
                    "kind": "condition",
                    "label": "月度总账封账窗口",
                    "domain": "finance",
                    "polarity": "neutral"
                },
                "action_concept": null,
                "group_concept": null,
                "member_mention": null,
                "valid_dates": null,
                "speaker": null,
                "assumed_premise_ids": [],
                "quote": {
                    "chunk_id": cid,
                    "verbatim_quote": "CORP_ALIGN_EVENT_702",
                    "turn_ordinal": null,
                    "approx_char_offset": null
                },
                "write_op": {"op": "add", "target_fact_id": null}
            }));
        }
        if prompt.contains("ALICE_CORP_DECISION_909") {
            atoms.push(json!({
                "kind": "decision",
                "claim_text": "内网专线迁移至玄武网关集群",
                "value_json": "{}",
                "scope": null,
                "ambiguous_scope": null,
                "condition_concept": {
                    "ref_type": "propose_new",
                    "concept_id": null,
                    "kind": "condition",
                    "label": "CORP_CONCEPT_XUANWU_909",
                    "domain": "infra",
                    "polarity": "neutral"
                },
                "action_concept": null,
                "group_concept": null,
                "member_mention": null,
                "valid_dates": null,
                "speaker": null,
                "assumed_premise_ids": [],
                "quote": {
                    "chunk_id": cid,
                    "verbatim_quote": "ALICE_CORP_DECISION_909",
                    "turn_ordinal": null,
                    "approx_char_offset": null
                },
                "write_op": {"op": "add", "target_fact_id": null}
            }));
        }
        if prompt.contains("NARROWED_RAW_CONSTRAINT_707") {
            atoms.push(json!({
                "kind": "constraint",
                "claim_text": "内网冻结期禁止变更路由",
                "value_json": "{}",
                "scope": {"scope": "speaker_self", "mentions": null, "group_concept": null},
                "ambiguous_scope": null,
                "condition_concept": {
                    "ref_type": "propose_new",
                    "concept_id": null,
                    "kind": "condition",
                    "label": "RAW_NARROWED_COND_707",
                    "domain": "ops",
                    "polarity": "neutral"
                },
                "action_concept": {
                    "ref_type": "propose_new",
                    "concept_id": null,
                    "kind": "action",
                    "label": "变更路由",
                    "domain": "ops",
                    "polarity": "negative"
                },
                "group_concept": null,
                "member_mention": null,
                "valid_dates": null,
                "speaker": null,
                "assumed_premise_ids": [],
                "quote": {
                    "chunk_id": cid,
                    "verbatim_quote": "NARROWED_RAW_CONSTRAINT_707",
                    "turn_ordinal": null,
                    "approx_char_offset": null
                },
                "write_op": {"op": "add", "target_fact_id": null}
            }));
        }
        if prompt.contains("POST_NARROW_HOST_EVENT_708") {
            atoms.push(json!({
                "kind": "event",
                "claim_text": "今晚执行常规巡检",
                "value_json": "{}",
                "scope": null,
                "ambiguous_scope": null,
                "condition_concept": {
                    "ref_type": "propose_new",
                    "concept_id": null,
                    "kind": "condition",
                    "label": "POST_NARROW_COND_708",
                    "domain": "ops",
                    "polarity": "neutral"
                },
                "action_concept": null,
                "group_concept": null,
                "member_mention": null,
                "valid_dates": null,
                "speaker": null,
                "assumed_premise_ids": [],
                "quote": {
                    "chunk_id": cid,
                    "verbatim_quote": "POST_NARROW_HOST_EVENT_708",
                    "turn_ordinal": null,
                    "approx_char_offset": null
                },
                "write_op": {"op": "add", "target_fact_id": null}
            }));
        }
        json!({
            "skip_summary": atoms.is_empty(),
            "atoms": atoms
        })
        .to_string()
    } else if prompt.contains("月度总账封账窗口") && prompt.contains("核心账务冻结期")
    {
        json!({"verdict": "entails", "question": null}).to_string()
    } else {
        json!({"verdict": "unrelated", "question": null}).to_string()
    };

    (
        StatusCode::OK,
        Json(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": content_json
                }
            }]
        })),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extract_routes_per_job_named_endpoint_and_fails_closed_without_cross_leakage() -> Result {
    let dir = tempfile::tempdir()?;
    let fake_home = dir.path().join("home");
    let kp_cfg_dir = fake_home.join(".kp");
    fs::create_dir_all(&kp_cfg_dir)?;
    let empty_bin = dir.path().join("empty-bin");
    fs::create_dir_all(&empty_bin)?;

    // 1. Always-offline check: invalid global gateway URL + `"host"` job fails closed with 0 egress!
    let lib_bad_gw = dir.path().join("lib_bad_gw");
    run(&lib_bad_gw, &["init", "--owner", "alice"])?;
    let host_line = json!({
        "source": {"kind": "doc", "uri": "doc://gw/1", "external_id": "gw-1"},
        "speaker": "alice",
        "audience": {"explicit": ["alice"]},
        "occurred_at": 1_763_770_000_000_i64,
        "content": "INVALID_GATEWAY_MUST_FAIL_CLOSED",
        "op": "upsert",
        "model_policy": {"mode": "host_cli"}
    })
    .to_string();
    run_stdin(
        &lib_bad_gw,
        &["--as", "alice", "ingest", "-"],
        host_line.as_bytes(),
    )?;
    let bad_gw_out = clean_kp_cmd(&fake_home)
        .env("PATH", &empty_bin)
        .env("OPENAI_API_KEY", "sk-bad-gw")
        .env("OPENAI_BASE_URL", "ftp://invalid-gateway.example/v1")
        .arg("--path")
        .arg(&lib_bad_gw)
        .args(["--json", "extract"])
        .output()?;
    assert!(
        !bad_gw_out.status.success(),
        "invalid gateway on `host` job must fail closed!"
    );
    assert_eq!(
        Store::open_readonly(&lib_bad_gw)?.daemon_queue_stats()?.1,
        0,
        "failed-closed job must not stay pending"
    );

    // 1b. Always-offline R2 Finding 3 check: `{"mode":"named_endpoint","endpoint":"host"}` when no
    //     `"host"` named endpoint is configured MUST decode strictly as `NamedEndpoint("host")` and
    //     fail closed (`failed_permanent`, 0 pending) — NEVER misrouting to `ModelRoutePolicy::HostCli`!
    let lib_named_host_offline = dir.path().join("lib_named_host_offline");
    run(&lib_named_host_offline, &["init", "--owner", "alice"])?;
    let named_host_line = json!({
        "source": {"kind": "doc", "uri": "doc://gw/named-host", "external_id": "gw-named-host"},
        "speaker": "alice",
        "audience": {"explicit": ["alice"]},
        "occurred_at": 1_763_770_010_000_i64,
        "content": "NAMED_HOST_ENDPOINT_SENTINEL_909",
        "op": "upsert",
        "model_policy": {"mode": "named_endpoint", "endpoint": "host"}
    })
    .to_string();
    run_stdin(
        &lib_named_host_offline,
        &["--as", "alice", "ingest", "-"],
        named_host_line.as_bytes(),
    )?;
    let named_host_offline_out = clean_kp_cmd(&fake_home)
        .env("PATH", &empty_bin)
        .env("OPENAI_API_KEY", "sk-global-should-never-be-called")
        .env("OPENAI_BASE_URL", "http://127.0.0.1:9/v1")
        .arg("--path")
        .arg(&lib_named_host_offline)
        .args(["--json", "extract"])
        .output()?;
    assert!(
        !named_host_offline_out.status.success(),
        "unconfigured named_endpoint `host` must fail closed!"
    );
    let named_host_stderr = String::from_utf8_lossy(&named_host_offline_out.stderr);
    assert!(
        named_host_stderr.contains("named endpoint"),
        "expected named-endpoint fail-closed diagnostic, got: {named_host_stderr}"
    );
    assert_eq!(
        Store::open_readonly(&lib_named_host_offline)?
            .daemon_queue_stats()?
            .1,
        0,
        "failed_permanent `named_endpoint:host` job must have 0 pending"
    );

    // 2. Two live local `axum` mock servers (`global_srv` vs `corp_srv`) verifying strict per-job
    //    `job.model_policy` isolation (`host` -> global, `corp-gw` -> `corp_srv`, `missing-ep` -> fail-closed 0 egress).
    let global_listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    let corp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let global_url = format!("http://{}/v1", global_listener.local_addr()?);
    let corp_url = format!("http://{}/v1", corp_listener.local_addr()?);

    let global_reqs = Arc::new(Mutex::new(Vec::<Value>::new()));
    let corp_reqs = Arc::new(Mutex::new(Vec::<Value>::new()));

    let g_srv = tokio::spawn({
        let state = MockChatLlmState {
            requests: global_reqs.clone(),
        };
        let app = Router::new()
            .route("/v1/chat/completions", post(mock_chat_completions_endpoint))
            .with_state(state);
        async move { axum::serve(global_listener, app).await }
    });
    let c_srv = tokio::spawn({
        let state = MockChatLlmState {
            requests: corp_reqs.clone(),
        };
        let app = Router::new()
            .route("/v1/chat/completions", post(mock_chat_completions_endpoint))
            .with_state(state);
        async move { axum::serve(corp_listener, app).await }
    });

    let cfg_path = kp_cfg_dir.join("config.json");
    fs::write(
        &cfg_path,
        serde_json::to_string_pretty(&json!({
            "named_endpoints": {
                "corp-gw": {
                    "provider": "openai",
                    "model": "corp-gpt-extract",
                    "base_url": corp_url,
                    "api_key": "sk-corp-dedicated"
                }
            }
        }))?,
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&cfg_path, fs::Permissions::from_mode(0o600))?;
    }

    let lib_route = dir.path().join("lib_route");
    run(&lib_route, &["init", "--owner", "alice", "--community"])?;
    run(
        &lib_route,
        &["member", "add", "bob", "--role", "maintainer"],
    )?;
    let three_records = [
        json!({
            "source": {"kind": "doc", "uri": "doc://route/missing", "external_id": "r-missing"},
            "speaker": "alice",
            "audience": {"explicit": ["alice"]},
            "occurred_at": 1_763_770_100_000_i64,
            "content": "MISSING_EP_SENTINEL_303",
            "op": "upsert",
            "model_policy": {"mode": "named_endpoint", "endpoint": "missing-ep"}
        })
        .to_string(),
        json!({
            "source": {"kind": "doc", "uri": "doc://route/corp", "external_id": "r-corp"},
            "speaker": "bob",
            "audience": {"explicit": ["bob"]},
            "occurred_at": 1_763_770_101_000_i64,
            "content": "CORP_GW_SENTINEL_202",
            "op": "upsert",
            "model_policy": {"mode": "named_endpoint", "endpoint": "corp-gw"}
        })
        .to_string(),
        json!({
            "source": {"kind": "doc", "uri": "doc://route/host", "external_id": "r-host"},
            "speaker": "alice",
            "audience": {"explicit": ["alice"]},
            "occurred_at": 1_763_770_102_000_i64,
            "content": "GLOBAL_ROUTE_SENTINEL_101",
            "op": "upsert",
            "model_policy": {"mode": "host_cli"}
        })
        .to_string(),
    ]
    .join("\n");
    run_stdin(
        &lib_route,
        &["--as", "alice", "ingest", "-"],
        three_records.as_bytes(),
    )?;

    let ext_out = clean_kp_cmd(&fake_home)
        .env("PATH", &empty_bin)
        .env("OPENAI_API_KEY", "sk-global-default")
        .env("OPENAI_BASE_URL", &global_url)
        .arg("--path")
        .arg(&lib_route)
        .args(["--as", "alice", "--json", "extract"])
        .output()?;

    // CLI `kp extract` MUST fail closed (non-zero exit) because `missing-ep` is unconfigured!
    assert!(
        !ext_out.status.success(),
        "expected non-zero exit for unconfigured `missing-ep`, stderr={}",
        String::from_utf8_lossy(&ext_out.stderr)
    );

    {
        let g_list = global_reqs.lock().map_err(|_| "lock")?;
        let c_list = corp_reqs.lock().map_err(|_| "lock")?;
        let g_dump = serde_json::to_string(&*g_list)?;
        let c_dump = serde_json::to_string(&*c_list)?;

        assert!(
            !g_dump.contains("MISSING_EP_SENTINEL_303")
                && !c_dump.contains("MISSING_EP_SENTINEL_303"),
            "unconfigured `missing-ep` MUST NEVER leak to any external server!"
        );
        assert!(
            c_dump.contains("CORP_GW_SENTINEL_202") && !g_dump.contains("CORP_GW_SENTINEL_202"),
            "`corp-gw` record must hit ONLY `corp-gw` server and NEVER cross-leak to global server!"
        );
        assert!(
            g_dump.contains("GLOBAL_ROUTE_SENTINEL_101")
                && !c_dump.contains("GLOBAL_ROUTE_SENTINEL_101"),
            "`host` record must hit ONLY global server and NEVER cross-leak to `corp-gw`!"
        );
    }

    let ro_route = Store::open_readonly(&lib_route)?;
    assert_eq!(ro_route.daemon_queue_stats()?.1, 0);
    assert_eq!(ro_route.status()?.facts, 2);

    // 3. R2 Finding 3 live server check: `{"mode":"named_endpoint","endpoint":"host"}` makes 0 calls to `global_srv`.
    global_reqs.lock().map_err(|_| "lock")?.clear();
    corp_reqs.lock().map_err(|_| "lock")?.clear();

    let lib_named_host_live = dir.path().join("lib_named_host_live");
    run(&lib_named_host_live, &["init", "--owner", "alice"])?;
    run_stdin(
        &lib_named_host_live,
        &["--as", "alice", "ingest", "-"],
        named_host_line.as_bytes(),
    )?;
    let named_host_live_out = clean_kp_cmd(&fake_home)
        .env("PATH", &empty_bin)
        .env("OPENAI_API_KEY", "sk-global-default")
        .env("OPENAI_BASE_URL", &global_url)
        .arg("--path")
        .arg(&lib_named_host_live)
        .args(["--as", "alice", "--json", "extract"])
        .output()?;
    assert!(!named_host_live_out.status.success());
    assert_eq!(
        global_reqs.lock().map_err(|_| "lock")?.len(),
        0,
        "`named_endpoint` named `host` MUST make 0 requests to global model!"
    );
    assert_eq!(
        Store::open_readonly(&lib_named_host_live)?
            .daemon_queue_stats()?
            .1,
        0
    );

    // 4. R2 Finding 4 live server check: two `corp-gw` records (`Constraint` + semantically entailed `Event`)
    //    align concepts against `corp_srv` and derive a `CollisionWarning` with ZERO requests to `global_srv`!
    global_reqs.lock().map_err(|_| "lock")?.clear();
    corp_reqs.lock().map_err(|_| "lock")?.clear();

    let lib_corp_align = dir.path().join("lib_corp_align");
    run(&lib_corp_align, &["init", "--owner", "alice"])?;
    let corp_align_records = [
        json!({
            "source": {"kind": "doc", "uri": "doc://corp/rule", "external_id": "corp-rule-1"},
            "speaker": "alice",
            "audience": {"explicit": ["alice"]},
            "occurred_at": 1_763_770_200_000_i64,
            "content": "CORP_ALIGN_CONSTRAINT_701 在核心账务冻结期禁止合并结算变更",
            "op": "upsert",
            "model_policy": {"mode": "named_endpoint", "endpoint": "corp-gw"}
        })
        .to_string(),
        json!({
            "source": {"kind": "doc", "uri": "doc://corp/event", "external_id": "corp-event-1"},
            "speaker": "alice",
            "audience": {"explicit": ["alice"]},
            "occurred_at": 1_763_770_201_000_i64,
            "content": "CORP_ALIGN_EVENT_702 今晚启动月度总账封账窗口",
            "op": "upsert",
            "model_policy": {"mode": "named_endpoint", "endpoint": "corp-gw"}
        })
        .to_string(),
    ]
    .join("\n");
    run_stdin(
        &lib_corp_align,
        &["--as", "alice", "ingest", "-"],
        corp_align_records.as_bytes(),
    )?;

    let corp_align_out = success(
        clean_kp_cmd(&fake_home)
            .env("PATH", &empty_bin)
            .env("OPENAI_API_KEY", "sk-global-default")
            .env("OPENAI_BASE_URL", &global_url)
            .arg("--path")
            .arg(&lib_corp_align)
            .args([
                "--as",
                "alice",
                "--json",
                "extract",
                "--at",
                "1763770205000",
            ])
            .output()?,
    )?;
    assert_eq!(corp_align_out["jobs_processed"], 2);
    assert!(
        corp_align_out["derived_warnings"].as_u64().unwrap_or(0) >= 1,
        "expected per-endpoint semantic alignment to derive >= 1 CollisionWarning, got: {corp_align_out}"
    );
    assert_eq!(
        global_reqs.lock().map_err(|_| "lock")?.len(),
        0,
        "all extraction + semantic alignment for `corp-gw` facts MUST make 0 requests to `global_srv`!"
    );
    {
        let c_list = corp_reqs.lock().map_err(|_| "lock")?;
        let c_dump = serde_json::to_string(&*c_list)?;
        assert!(
            c_list.len() >= 3
                && c_dump.contains("月度总账封账窗口")
                && c_dump.contains("核心账务冻结期"),
            "expected `corp_srv` to serve both extractions and semantic entailment alignment, got {} reqs",
            c_list.len()
        );
    }

    // 5. R2 Finding 10 check: `kp extract --model custom-override-model` preserves `FallbackLlmClient`
    //    when the primary Host CLI returns `401 Unauthorized`, failing over cleanly to backup HTTP API!
    global_reqs.lock().map_err(|_| "lock")?.clear();
    let mock_401_cli = dir.path().join("mock_codex_401.sh");
    fs::write(
        &mock_401_cli,
        "#!/bin/sh\necho 'HTTP 401 Unauthorized: expired host session' >&2\nexit 1\n",
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&mock_401_cli, fs::Permissions::from_mode(0o755))?;
    }

    let lib_fallback = dir.path().join("lib_fallback");
    run(&lib_fallback, &["init", "--owner", "alice"])?;
    let fallback_line = json!({
        "source": {"kind": "doc", "uri": "doc://fb/1", "external_id": "fb-1"},
        "speaker": "alice",
        "audience": {"explicit": ["alice"]},
        "occurred_at": 1_763_770_300_000_i64,
        "content": "FALLBACK_OVERRIDE_SENTINEL_808",
        "op": "upsert",
        "model_policy": {"mode": "host_cli"}
    })
    .to_string();
    run_stdin(
        &lib_fallback,
        &["--as", "alice", "ingest", "-"],
        fallback_line.as_bytes(),
    )?;

    let fb_out = success(
        clean_kp_cmd(&fake_home)
            .env("PATH", &empty_bin)
            .env("KP_HOST_CLI_PROGRAM", &mock_401_cli)
            .env("KP_HOST_CLI_KIND", "codex")
            .env("OPENAI_API_KEY", "sk-backup-http")
            .env("OPENAI_BASE_URL", &global_url)
            .arg("--path")
            .arg(&lib_fallback)
            .args([
                "--as",
                "alice",
                "--json",
                "extract",
                "--model",
                "custom-override-model",
                "--at",
                "1763770305000",
            ])
            .output()?,
    )?;
    assert_eq!(fb_out["jobs_processed"], 1);
    assert_eq!(
        fb_out["created_facts"]
            .as_array()
            .map_or(0, std::vec::Vec::len),
        1
    );
    assert!(
        !global_reqs.lock().map_err(|_| "lock")?.is_empty(),
        "expected backup HTTP server to receive failover extraction after Host CLI 401"
    );

    // 4. R3 Findings 1 & 2: SAME-AUTHOR (`alice`) cross-route consecutive extraction (`corp-gw` -> `host_cli`)
    //    AND `HostCli` source narrowed to `RawOnly` followed by `HostCli` alignment MUST leak ZERO
    //    decisions, speaker-own facts, concepts, or narrowed quotes to `global_srv`!
    global_reqs.lock().map_err(|_| "lock")?.clear();
    corp_reqs.lock().map_err(|_| "lock")?.clear();

    let lib_same_author = dir.path().join("lib_same_author");
    run(&lib_same_author, &["init", "--owner", "alice"])?;

    let alice_corp_dec = json!({
        "source": {"kind": "doc", "uri": "doc://sa/corp", "external_id": "sa-corp-1"},
        "speaker": "alice",
        "audience": {"explicit": ["alice"]},
        "occurred_at": 1_763_770_400_000_i64,
        "content": "ALICE_CORP_DECISION_909",
        "op": "upsert",
        "model_policy": {"mode": "named_endpoint", "endpoint": "corp-gw"}
    })
    .to_string();
    run_stdin(
        &lib_same_author,
        &["--as", "alice", "ingest", "-"],
        alice_corp_dec.as_bytes(),
    )?;
    let sa_ext1 = success(
        clean_kp_cmd(&fake_home)
            .env("PATH", &empty_bin)
            .env("OPENAI_API_KEY", "sk-global-default")
            .env("OPENAI_BASE_URL", &global_url)
            .arg("--path")
            .arg(&lib_same_author)
            .args([
                "--as",
                "alice",
                "--json",
                "extract",
                "--at",
                "1763770401000",
            ])
            .output()?,
    )?;
    assert_eq!(sa_ext1["jobs_processed"], 1);
    let sa_ext1_facts = sa_ext1["created_facts"]
        .as_array()
        .ok_or("expected created_facts array on sa_ext1")?;
    assert_eq!(sa_ext1_facts.len(), 1);
    let corp_fact_id = sa_ext1_facts[0]
        .as_str()
        .ok_or("expected sa_ext1 to persist 1 corp-gw Decision fact")?
        .to_owned();
    let corp_source_id = {
        let ro_sa = Store::open_readonly(&lib_same_author)?;
        let concepts = ro_sa.list_active_concepts()?;
        assert!(
            concepts
                .iter()
                .any(|c| c.canonical_label.contains("CORP_CONCEPT_XUANWU_909")),
            "expected `CORP_CONCEPT_XUANWU_909` to be persisted in `community_concepts`: {concepts:?}"
        );
        let graph_sa = ro_sa.derivation_view()?;
        let cf = graph_sa
            .facts
            .get(&corp_fact_id.parse()?)
            .ok_or("expected corp Decision fact to be persisted in `facts`")?;
        cf.source_anchor
            .as_ref()
            .and_then(|a| a.source_id.as_ref())
            .ok_or("missing corp source_id")?
            .to_string()
    };
    {
        let c_dump_step1 = serde_json::to_string(&*corp_reqs.lock().map_err(|_| "lock")?)?;
        assert!(
            c_dump_step1.contains("ALICE_CORP_DECISION_909"),
            "expected corp_srv to receive ALICE_CORP_DECISION_909 extraction request: {c_dump_step1}"
        );
    }
    assert!(
        global_reqs.lock().map_err(|_| "lock")?.is_empty(),
        "corp-gw extraction/alignment must send 0 requests to global_srv"
    );

    // Now SAME author `alice` ingests a `HostCli` Constraint, extracts it, then narrows that source
    // to `RawOnly`, and finally ingests & extracts a `HostCli` Event (`POST_NARROW_HOST_EVENT_708`).
    let alice_host_con = json!({
        "source": {"kind": "doc", "uri": "doc://sa/narrow", "external_id": "sa-narrow-1"},
        "speaker": "alice",
        "audience": {"explicit": ["alice"]},
        "occurred_at": 1_763_770_402_000_i64,
        "content": "NARROWED_RAW_CONSTRAINT_707",
        "op": "upsert",
        "model_policy": {"mode": "host_cli"}
    })
    .to_string();
    run_stdin(
        &lib_same_author,
        &["--as", "alice", "ingest", "-"],
        alice_host_con.as_bytes(),
    )?;
    let sa_ext2 = success(
        clean_kp_cmd(&fake_home)
            .env("PATH", &empty_bin)
            .env("OPENAI_API_KEY", "sk-global-default")
            .env("OPENAI_BASE_URL", &global_url)
            .arg("--path")
            .arg(&lib_same_author)
            .args([
                "--as",
                "alice",
                "--json",
                "extract",
                "--at",
                "1763770403000",
            ])
            .output()?,
    )?;
    assert_eq!(sa_ext2["jobs_processed"], 1);
    assert_eq!(
        sa_ext2["created_facts"]
            .as_array()
            .map_or(0, std::vec::Vec::len),
        1
    );
    {
        let g_dump_step2 = serde_json::to_string(&*global_reqs.lock().map_err(|_| "lock")?)?;
        assert!(
            g_dump_step2.contains("NARROWED_RAW_CONSTRAINT_707"),
            "expected global_srv to receive NARROWED_RAW_CONSTRAINT_707 extraction request: {g_dump_step2}"
        );
        assert!(
            !g_dump_step2.contains("玄武网关集群")
                && !g_dump_step2.contains("ALICE_CORP_DECISION_909")
                && !g_dump_step2.contains("CORP_CONCEPT_XUANWU_909"),
            "same-author `corp-gw` decision/fact/concept leaked into `HostCli` prompt! dump={g_dump_step2}"
        );
    }

    // Narrow `sa-narrow-1` to `RawOnly` via duplicate `Upsert` and clear `global_reqs` before extracting `Event`!
    global_reqs.lock().map_err(|_| "lock")?.clear();
    let narrow_to_raw = json!({
        "source": {"kind": "doc", "uri": "doc://sa/narrow", "external_id": "sa-narrow-1"},
        "speaker": "alice",
        "audience": {"explicit": ["alice"]},
        "occurred_at": 1_763_770_404_000_i64,
        "content": "NARROWED_RAW_CONSTRAINT_707",
        "op": "upsert",
        "model_policy": {"mode": "raw_only"}
    })
    .to_string();
    run_stdin(
        &lib_same_author,
        &["--as", "alice", "ingest", "-"],
        narrow_to_raw.as_bytes(),
    )?;

    let post_narrow_ev = json!({
        "source": {"kind": "doc", "uri": "doc://sa/ev", "external_id": "sa-ev-1"},
        "speaker": "alice",
        "audience": {"explicit": ["alice"]},
        "occurred_at": 1_763_770_405_000_i64,
        "content": "POST_NARROW_HOST_EVENT_708",
        "op": "upsert",
        "model_policy": {"mode": "host_cli"}
    })
    .to_string();
    run_stdin(
        &lib_same_author,
        &["--as", "alice", "ingest", "-"],
        post_narrow_ev.as_bytes(),
    )?;
    let sa_ext3 = success(
        clean_kp_cmd(&fake_home)
            .env("PATH", &empty_bin)
            .env("OPENAI_API_KEY", "sk-global-default")
            .env("OPENAI_BASE_URL", &global_url)
            .arg("--path")
            .arg(&lib_same_author)
            .args([
                "--as",
                "alice",
                "--json",
                "extract",
                "--at",
                "1763770406000",
            ])
            .output()?,
    )?;
    assert_eq!(sa_ext3["jobs_processed"], 1);
    assert_eq!(
        sa_ext3["created_facts"]
            .as_array()
            .map_or(0, std::vec::Vec::len),
        1
    );
    {
        let g_dump_step3 = serde_json::to_string(&*global_reqs.lock().map_err(|_| "lock")?)?;
        assert!(
            g_dump_step3.contains("POST_NARROW_HOST_EVENT_708"),
            "expected global_srv to receive POST_NARROW_HOST_EVENT_708 extraction request: {g_dump_step3}"
        );
        assert!(
            !g_dump_step3.contains("NARROWED_RAW_CONSTRAINT_707")
                && !g_dump_step3.contains("内网冻结期禁止变更路由")
                && !g_dump_step3.contains("RAW_NARROWED_COND_707")
                && !g_dump_step3.contains("玄武网关集群")
                && !g_dump_step3.contains("CORP_CONCEPT_XUANWU_909"),
            "narrowed `RawOnly` or `corp-gw` fact/quote/concept leaked to `global_srv` during extraction or alignment! dump={g_dump_step3}"
        );
    }

    // 5. R4/R5 Finding 1: `kp purge` on the `corp-gw` fact erases `CORP_CONCEPT_XUANWU_909` from
    //    `community_concepts` (zero binary residue), repeat `kp purge` on the already-purged
    //    `corp_source_id` ALSO sweeps any pre-existing orphaned unseeded concepts, AND subsequent
    //    `HostCli` extraction processes 1 job, sends `GLOBAL_ROUTE_SENTINEL_101`, and leaks 0
    //    occurrences of `CORP_CONCEPT_XUANWU_909`!
    global_reqs.lock().map_err(|_| "lock")?.clear();
    run(&lib_same_author, &["--as", "alice", "purge", &corp_fact_id])?;
    run(
        &lib_same_author,
        &["--as", "alice", "purge", &corp_source_id],
    )?;
    {
        let ro_after_purge = Store::open_readonly(&lib_same_author)?;
        let surviving_concepts = ro_after_purge.list_active_concepts()?;
        assert!(
            !surviving_concepts
                .iter()
                .any(|c| c.canonical_label.contains("CORP_CONCEPT_XUANWU_909")),
            "purging `corp-gw` fact must erase orphaned `CORP_CONCEPT_XUANWU_909` from `community_concepts`!"
        );
        assert_eq!(
            kp_store::find_sqlite_table_containing_bytes(
                &lib_same_author,
                b"CORP_CONCEPT_XUANWU_909"
            )?,
            None,
            "SQLite must retain zero residue of `CORP_CONCEPT_XUANWU_909` after `purge`"
        );
    }
    let after_purge_host = json!({
        "source": {"kind": "doc", "uri": "doc://sa/post-purge", "external_id": "sa-post-purge"},
        "speaker": "alice",
        "audience": {"explicit": ["alice"]},
        "occurred_at": 1_763_770_407_000_i64,
        "content": "GLOBAL_ROUTE_SENTINEL_101",
        "op": "upsert",
        "model_policy": {"mode": "host_cli"}
    })
    .to_string();
    run_stdin(
        &lib_same_author,
        &["--as", "alice", "ingest", "-"],
        after_purge_host.as_bytes(),
    )?;
    let sa_ext4 = success(
        clean_kp_cmd(&fake_home)
            .env("PATH", &empty_bin)
            .env("OPENAI_API_KEY", "sk-global-default")
            .env("OPENAI_BASE_URL", &global_url)
            .arg("--path")
            .arg(&lib_same_author)
            .args([
                "--as",
                "alice",
                "--json",
                "extract",
                "--force",
                "--at",
                "1763770408000",
            ])
            .output()?,
    )?;
    assert_eq!(sa_ext4["jobs_processed"], 1);
    assert_eq!(
        sa_ext4["created_facts"]
            .as_array()
            .map_or(0, std::vec::Vec::len),
        1
    );
    {
        let g_dump_step4 = serde_json::to_string(&*global_reqs.lock().map_err(|_| "lock")?)?;
        assert!(
            g_dump_step4.contains("GLOBAL_ROUTE_SENTINEL_101"),
            "expected global_srv to receive GLOBAL_ROUTE_SENTINEL_101 extraction request: {g_dump_step4}"
        );
        assert!(
            !g_dump_step4.contains("CORP_CONCEPT_XUANWU_909"),
            "purged `corp-gw` concept leaked after purge! dump={g_dump_step4}"
        );
    }

    g_srv.abort();
    c_srv.abort();
    Ok(())
}
