mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use axum::{
    extract::State,
    http::header,
    routing::{get, post},
    Json, Router,
};
use kp_model::{FactId, FactKind, FactStatus};
use kp_store::Store;
use serde_json::{json, Value};

use crate::support::{run, success, Result};

#[derive(Clone, Default)]
struct MockLlmState {
    calls: Arc<AtomicUsize>,
    saw_stripped_fallback: Arc<AtomicUsize>,
    prompts: Arc<Mutex<Vec<String>>>,
}

async fn mock_chat_completions(
    State(state): State<MockLlmState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    state.calls.fetch_add(1, Ordering::SeqCst);
    let user_text = body["messages"]
        .as_array()
        .and_then(|arr| arr.last())
        .and_then(|m| m["content"].as_str())
        .unwrap_or("")
        .to_owned();
    if !user_text.contains("fallback_attribution") {
        state.saw_stripped_fallback.fetch_add(1, Ordering::SeqCst);
    }
    if let Ok(mut guard) = state.prompts.lock() {
        guard.push(user_text.clone());
    }

    let reply = if user_text.contains("prisma migrate deploy") {
        json!({
            "decision": "UPDATE",
            "attribution": {
                "skill_slug": "prisma-schema-migration-gate",
                "trigger": "running prisma database migrations across environments",
                "wrong_move": "Execute prisma migrate deploy without explicitly specifying --schema",
                "verified_fix": "Always pass --schema pointing to the verified Prisma schema before running prisma migrate deploy.",
                "dimension": "INV",
                "cited_premise_ids": []
            },
            "extra_rules": [
                "Confirm migration status exits with code 0 before traffic cutover."
            ],
            "memory_facts": [{
                "kind": "constraint",
                "subject": "prisma-migrations",
                "predicate": "kp.policy.guard",
                "summary": "Never run prisma migrate deploy without explicit --schema flag",
                "verbatim_quote": "prisma migrate deploy",
                "hard_guard": true,
                "forbidden_commands": ["prisma migrate deploy"],
                "allowed_commands": [],
                "required_flags": ["--schema"]
            }]
        })
    } else if user_text.contains("ModuleNotFoundError") {
        json!({
            "decision": "UPDATE",
            "attribution": {
                "skill_slug": "hallucinated-flag-candidate",
                "trigger": "fixing Python import errors",
                "wrong_move": "Run system pytest blindly",
                "verified_fix": "Run `pytest --invented-nonexistent-flag` to repair imports.",
                "dimension": "INV",
                "cited_premise_ids": []
            },
            "extra_rules": [],
            "memory_facts": []
        })
    } else if user_text.contains("uv pip install") {
        json!({
            "decision": "UPDATE",
            "attribution": {
                "skill_slug": "uv-pip-locked-install",
                "trigger": "installing or updating Python dependencies in this workspace",
                "wrong_move": "Invoke bare pip install without lockfile verification",
                "verified_fix": "Use uv pip install instead of bare pip install so dependency hashes stay reproducible.",
                "dimension": "INV",
                "cited_premise_ids": []
            },
            "extra_rules": [],
            "memory_facts": []
        })
    } else {
        json!({
            "decision": "KEEP_BASE",
            "rejection_class": "one_off_task_redirect",
            "extra_rules": [],
            "memory_facts": []
        })
    };

    Json(json!({
        "id": "chatcmpl-dream-test",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": reply.to_string()
            },
            "finish_reason": "stop"
        }]
    }))
}

fn seed_multisource_fixtures(
    root: &Path,
    lib: &Path,
    addr: std::net::SocketAddr,
) -> Result<PathBuf> {
    let video_mp4 = root.join("onboarding.mp4");
    fs::write(&video_mp4, b"MP4_HEADER_STUB")?;
    fs::write(
        root.join("onboarding.vtt"),
        "WEBVTT\n\n00:02:10.000 --> 00:02:24.000\n严禁使用 `pip install`，必须统一改用 `uv pip install` 以保证锁文件一致。\n",
    )?;
    run(
        lib,
        &["--as", "owner", "add", video_mp4.to_str().ok_or("utf8")?],
    )?;

    let audio_lrc = root.join("incident_review.lrc");
    fs::write(
        &audio_lrc,
        "[ti:Incident Postmortem]\n[01:15.50] Never run `prisma migrate deploy` without `--schema` in any shared environment.\n[01:28.00] Confirm backup snapshot before cutover.\n",
    )?;
    run(
        lib,
        &["--as", "owner", "add", audio_lrc.to_str().ok_or("utf8")?],
    )?;

    let runbook_url = format!("http://{addr}/ops-runbook.html");
    let _ = success(
        Command::new(env!("CARGO_BIN_EXE_kp"))
            .env("KP_OFFLINE", "1")
            .env("KP_ALLOW_LOCAL_URL_INGEST", "1")
            .arg("--path")
            .arg(lib)
            .args(["--as", "owner", "--json", "add", &runbook_url, "--trusted"])
            .output()?,
    )?;

    let doc_md = root.join("architecture_rules.md");
    fs::write(
        &doc_md,
        "# Database Rules\nNever run `prisma migrate deploy` without `--schema` pointing to the service schema.\n",
    )?;
    run(
        lib,
        &["--as", "owner", "add", doc_md.to_str().ok_or("utf8")?],
    )?;

    let chat_jsonl = root.join("team_chat.jsonl");
    let chat_rec = json!({
        "source": {"kind": "chat", "uri": "chat://eng_room/thread_42", "external_id": "msg-42"},
        "speaker": "owner",
        "audience": {"explicit": ["owner"]},
        "occurred_at": 1_764_000_000_000_i64,
        "content": "[speaker:owner] 大家注意，仓库里禁止使用 `pip install`，必须改用 `uv pip install`。",
        "op": "upsert"
    });
    fs::write(&chat_jsonl, format!("{chat_rec}\n"))?;
    let _ = success(
        Command::new(env!("CARGO_BIN_EXE_kp"))
            .env("KP_GENERATIVE_MODEL", "openai:gpt-4o-mini")
            .env("OPENAI_BASE_URL", format!("http://{addr}/v1"))
            .env("OPENAI_API_KEY", "sk-test-local-gateway-key")
            .env("KP_NO_AUTO_DREAM", "1")
            .arg("--path")
            .arg(lib)
            .args([
                "--as",
                "owner",
                "--json",
                "ingest",
                chat_jsonl.to_str().ok_or("utf8")?,
            ])
            .output()?,
    )?;

    let py_session = root.join("python_session.jsonl");
    let py_lines = [
        json!({
            "sessionId": "py-sess-1",
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "tu_py1", "name": "Bash", "input": {"command": "pytest tests/test_worker.py"}}
                ]
            }
        }),
        json!({
            "sessionId": "py-sess-1",
            "type": "user",
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tu_py1",
                    "is_error": true,
                    "content": "Traceback (most recent call last):\n  File \"tests/test_worker.py\", line 3, in <module>\nModuleNotFoundError: No module named 'worker_core'\nExit code: 1"
                }]
            }
        }),
        json!({
            "sessionId": "py-sess-1",
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "tu_py2", "name": "Edit", "input": {"file_path": "pyproject.toml"}}
                ]
            }
        }),
        json!({
            "sessionId": "py-sess-1",
            "type": "user",
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tu_py2",
                    "is_error": false,
                    "content": "Updated pyproject.toml"
                }]
            }
        }),
        json!({
            "sessionId": "py-sess-1",
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "tu_py3", "name": "Bash", "input": {"command": "pytest tests/"}}
                ]
            }
        }),
        json!({
            "sessionId": "py-sess-1",
            "type": "user",
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tu_py3",
                    "is_error": false,
                    "content": "4 passed in 0.21s\nExit code: 0"
                }]
            }
        }),
        json!({
            "sessionId": "py-sess-1",
            "type": "user",
            "message": {
                "role": "user",
                "content": "No problem, thanks for fixing that!"
            }
        }),
    ]
    .iter()
    .map(Value::to_string)
    .collect::<Vec<_>>()
    .join("\n");
    fs::write(&py_session, format!("{py_lines}\n"))?;
    Ok(py_session)
}

fn check_pre_tool_command(lib: &Path, cmd: &str) -> Result<Value> {
    let payload = json!({
        "tool_name": "Bash",
        "tool_input": {"command": cmd}
    });
    let out = Command::new(env!("CARGO_BIN_EXE_kp"))
        .env("KP_OFFLINE", "1")
        .env("KP_NO_AUTO_DAEMON", "1")
        .arg("--path")
        .arg(lib)
        .args(["--as", "owner", "hook", "pre-tool-use"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(payload.to_string().as_bytes())?;
            }
            child.wait_with_output()
        })?;
    assert!(out.status.success());
    Ok(serde_json::from_slice(&out.stdout)?)
}

fn assert_k_of_n_skill_withdrawal(lib: &Path) -> Result {
    let prisma_ev_ids: Vec<FactId> = {
        let ro = Store::open_readonly(lib)?;
        let graph = ro.derivation_view()?;
        let skill_fact = graph
            .facts
            .values()
            .find(|f| {
                f.kind == FactKind::SkillVersion
                    && f.subject == "prisma-schema-migration-gate"
                    && f.status == FactStatus::Active
            })
            .ok_or("active prisma skill")?;
        let proof = graph
            .justifications
            .values()
            .find(|j| j.conclusion == skill_fact.id && j.active)
            .ok_or("active prisma promotion proof")?;
        proof
            .premises
            .iter()
            .filter_map(|p| {
                let f = graph.facts.get(&p.fact_id)?;
                (f.predicate == "kp.rsi.evidence").then(|| f.id.clone())
            })
            .collect()
    };
    assert_eq!(
        prisma_ev_ids.len(),
        3,
        "expected 3 independent evidence premises"
    );

    run(lib, &["--as", "owner", "forget", prisma_ev_ids[0].as_str()])?;
    let prisma_md = lib
        .join("skills")
        .join("prisma-schema-migration-gate")
        .join("SKILL.md");
    assert!(
        prisma_md.is_file(),
        "SKILL.md must survive single source withdrawal when 2 independent sources remain"
    );

    run(lib, &["--as", "owner", "forget", prisma_ev_ids[1].as_str()])?;
    assert!(
        !prisma_md.exists(),
        "SKILL.md must be retracted once surviving independent sources drop below 2"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multisource_video_audio_link_chat_doc_and_python_session_dream_with_llm_zep_scanner_and_guards(
) -> Result {
    let dir = tempfile::tempdir()?;
    let lib = dir.path().join("m6_lib");
    run(
        &lib,
        &["init", "--owner", "owner", "--embedding-model", "hash:v1"],
    )?;

    let llm_state = MockLlmState::default();
    let app = Router::new()
        .route("/v1/chat/completions", post(mock_chat_completions))
        .route(
            "/ops-runbook.html",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    "<html><head><title>Ops Runbook</title></head><body><h2>Migration Gate</h2><p>Always pass <code>--schema</code> before running <code>prisma migrate deploy</code> in staging and production.</p></body></html>",
                )
            }),
        )
        .with_state(llm_state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let srv = tokio::spawn(async move { axum::serve(listener, app).await });

    let py_session = seed_multisource_fixtures(dir.path(), &lib, addr)?;

    let dream_res = success(
        Command::new(env!("CARGO_BIN_EXE_kp"))
            .env("KP_GENERATIVE_MODEL", "openai:gpt-4o-mini")
            .env("OPENAI_BASE_URL", format!("http://{addr}/v1"))
            .env("OPENAI_API_KEY", "sk-test-local-gateway-key")
            .env("KP_NO_AUTO_DREAM", "1")
            .arg("--path")
            .arg(&lib)
            .args([
                "--as",
                "owner",
                "--json",
                "dream",
                "--sessions",
                py_session.to_str().ok_or("utf8")?,
            ])
            .output()?,
    )?;

    assert!(
        llm_state.calls.load(Ordering::SeqCst) >= 4,
        "expected global LLM client to be invoked automatically: {dream_res}"
    );
    assert_eq!(
        llm_state.calls.load(Ordering::SeqCst),
        llm_state.saw_stripped_fallback.load(Ordering::SeqCst),
        "fallback_attribution must never be sent to generative LLM"
    );
    assert!(
        dream_res["sources_mined"].as_u64().unwrap_or(0) >= 5,
        "expected >=5 non-session sources mined (video, audio, url, doc, chat): {dream_res}"
    );
    assert!(
        dream_res["scanner_rejected"].as_u64().unwrap_or(0) >= 1,
        "Zep scanner must reject hallucinated `--invented-nonexistent-flag`: {dream_res}"
    );

    let skills = run(&lib, &["--as", "owner", "skill", "list"])?;
    let skill_names: Vec<&str> = skills
        .as_array()
        .ok_or("skills array")?
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        skill_names.contains(&"prisma-schema-migration-gate"),
        "expected promoted prisma skill in {skill_names:?}"
    );
    assert!(
        skill_names.contains(&"uv-pip-locked-install"),
        "expected promoted uv-pip skill in {skill_names:?}"
    );
    assert!(
        !skill_names.contains(&"hallucinated-flag-candidate"),
        "scanner-rejected skill must not be promoted"
    );

    {
        let ro = Store::open_readonly(&lib)?;
        let graph = ro.derivation_view()?;
        assert!(graph
            .facts
            .values()
            .any(|f| f.source_anchor.as_ref().and_then(|a| a.start_ms) == Some(130_000)));
        assert!(graph
            .facts
            .values()
            .any(|f| f.source_anchor.as_ref().and_then(|a| a.start_ms) == Some(75_500)));
    }

    let pip_bad = check_pre_tool_command(&lib, "pip install fastapi")?;
    assert_eq!(pip_bad["hookSpecificOutput"]["permissionDecision"], "ask");
    assert_eq!(
        check_pre_tool_command(&lib, "uv pip install fastapi")?,
        json!({})
    );

    let prisma_bad = check_pre_tool_command(&lib, "prisma migrate deploy")?;
    assert_eq!(
        prisma_bad["hookSpecificOutput"]["permissionDecision"],
        "ask"
    );
    assert_eq!(
        check_pre_tool_command(&lib, "prisma migrate deploy --schema ./db/schema.prisma")?,
        json!({})
    );

    assert_k_of_n_skill_withdrawal(&lib)?;
    srv.abort();
    Ok(())
}

#[tokio::test]
async fn session_end_hook_spawns_detached_background_dream_and_cleans_lockfile() -> Result {
    let dir = tempfile::tempdir()?;
    let auto_lib = dir.path().join("auto_bg_lib");
    run(&auto_lib, &["init", "--owner", "owner", "--no-embedding"])?;
    let doc_a = dir.path().join("rule_a.md");
    let doc_b = dir.path().join("rule_b.md");
    fs::write(
        &doc_a,
        "# Deploy\nNever run `kubectl delete` without `--dry-run` first.\n",
    )?;
    fs::write(
        &doc_b,
        "# Deploy\nNever run `kubectl delete` without `--dry-run` check.\n",
    )?;
    run(
        &auto_lib,
        &["--as", "owner", "add", doc_a.to_str().ok_or("utf8")?],
    )?;
    run(
        &auto_lib,
        &["--as", "owner", "add", doc_b.to_str().ok_or("utf8")?],
    )?;

    let hook_sess_jsonl = dir.path().join("hook_end.jsonl");
    fs::write(
        &hook_sess_jsonl,
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"Done for today.\"}}\n",
    )?;
    let end_payload = json!({
        "session_id": "bg-end-1",
        "transcript_path": hook_sess_jsonl.to_str().ok_or("utf8")?
    });
    let hook_out = Command::new(env!("CARGO_BIN_EXE_kp"))
        .env("KP_OFFLINE", "1")
        .env("KP_MODEL_POLICY", "raw_only")
        .env("KP_NO_AUTO_DAEMON", "1")
        .env("KP_ENABLE_AUTO_DREAM_IN_TEST", "1")
        .env("KP_AUTO_DREAM_THRESHOLD", "2")
        .arg("--path")
        .arg(&auto_lib)
        .args(["--as", "owner", "hook", "session-end"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(end_payload.to_string().as_bytes())?;
            }
            child.wait_with_output()
        })?;
    assert!(hook_out.status.success());

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut promoted_bg = false;
    while Instant::now() < deadline {
        if let Ok(ro) = Store::open_readonly(&auto_lib) {
            if let Ok(active) = ro.active_skills(&"owner".parse()?, kp_model::EpochMs(i64::MAX - 1))
            {
                if !active.is_empty() && !auto_lib.join("dream.spawning").exists() {
                    promoted_bg = true;
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    assert!(
        promoted_bg,
        "detached background `kp dream --background` must promote corroborated skill and remove lockfile"
    );
    Ok(())
}
