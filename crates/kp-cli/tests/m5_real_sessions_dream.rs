mod support;

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use axum::{http::StatusCode, routing::post, Router};
use kp_model::{EpochMs, FactId, FactKind, FactStatus, EVIDENCE_BOUNDARY};
use kp_store::Store;
use serde_json::{json, Value};

use crate::support::{run, success, Result};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dream_mines_four_real_session_formats_promotes_provenanced_skills_rolls_back_on_forget_and_blocks_raw_only_http(
) -> Result {
    let dir = tempfile::tempdir()?;
    let lib = dir.path().join("m5_lib");
    let sessions_root = dir.path().join("sessions");
    fs::create_dir_all(&sessions_root)?;

    run(
        &lib,
        &[
            "init",
            "--owner",
            "owner",
            "--community",
            "--embedding-model",
            "hash:v1",
        ],
    )?;
    run(&lib, &["member", "add", "carol", "--role", "member"])?;

    let mac_home_leak = format!("/{}sers/dev_lead_priv/work/repo", 'U');
    let linux_home_leak = format!("/{}ome/dev_ops_priv/work/repo", 'h');
    let cjk_mac_leak = format!("请先阅读/{}/cjk_priv_lead/work/Cargo.toml", "Users");
    let arrow_linux_leak = format!("-->/{}/arrow_priv_ops/src/lib.rs", "home");
    let secret_leak = "sk-proj-m5dreamsecret999999999999";

    // 1. Format ①: Antigravity `transcript.jsonl` (`<conv_id>/.system_generated/logs/transcript.jsonl`)
    //    Includes `<CONTEXT_SUMMARY>` + `<SYSTEM_MESSAGE>` noise (must be ignored), a real `E0308`
    //    compile failure -> `edit_file` -> green `cargo test` recovery, and an explicit human correction.
    let ag_path = sessions_root
        .join("conv-a1111111")
        .join(".system_generated")
        .join("logs")
        .join("transcript.jsonl");
    fs::create_dir_all(ag_path.parent().ok_or("ag parent")?)?;
    let ag_lines = [
        json!({
            "step_index": 0,
            "source": "USER_EXPLICIT",
            "type": "USER_INPUT",
            "content": "<CONTEXT_SUMMARY>不对，这是旧压缩摘要噪声，绝不能当成用户当前纠正</CONTEXT_SUMMARY>\n<USER_REQUEST>\n请实现导出校验模块。\n</USER_REQUEST>"
        }),
        json!({
            "step_index": 1,
            "source": "SYSTEM",
            "type": "SYSTEM_MESSAGE",
            "content": "<SYSTEM_MESSAGE>[swarm] new board message #12 — 不对，这是系统通知噪声</SYSTEM_MESSAGE>"
        }),
        json!({
            "step_index": 2,
            "source": "MODEL",
            "type": "PLANNER_RESPONSE",
            "content": "我先在 CLI 层直接连接底层库表读取状态。",
            "tool_calls": [{
                "name": "run_command",
                "args": {"CommandLine": format!("cargo test --manifest-path {mac_home_leak}/Cargo.toml")}
            }]
        }),
        json!({
            "step_index": 3,
            "source": "MODEL",
            "type": "GENERIC",
            "content": format!(
                "Command exited with code 101.\n\nCommand output:\n{cjk_mac_leak}\nCompiling demo ({mac_home_leak}) token={secret_leak}\nerror[E0308]: mismatched types in settlement_validator\n  {arrow_linux_leak}:42:18\nerror: could not compile `demo` due to 1 previous error\n"
            )
        }),
        json!({
            "step_index": 4,
            "source": "MODEL",
            "type": "PLANNER_RESPONSE",
            "content": "核对函数签名后修正参数类型。",
            "tool_calls": [{"name": "edit_file", "args": {"Path": "src/validator.rs"}}]
        }),
        json!({
            "step_index": 5,
            "source": "MODEL",
            "type": "GENERIC",
            "content": format!("Made 1 replacement in {mac_home_leak}/src/validator.rs")
        }),
        json!({
            "step_index": 6,
            "source": "SYSTEM",
            "type": "SYSTEM_MESSAGE",
            "content": "Task id \"t-1\" finished with result:\nCommand exited with code 0.\n\nCommand output:\nFinished `test` profile [unoptimized] target(s) in 1.12s\ntest result: ok. 6 passed; 0 failed\n"
        }),
        json!({
            "step_index": 7,
            "source": "USER_EXPLICIT",
            "type": "USER_INPUT",
            "content": "<USER_REQUEST>\n不要在 CLI 层直接拼装存储查询，应该统一调用公开导出的只读方法。\n</USER_REQUEST>"
        }),
    ]
    .iter()
    .map(Value::to_string)
    .collect::<Vec<_>>()
    .join("\n");
    fs::write(&ag_path, format!("{ag_lines}\n"))?;

    // 2. Format ②: Claude Code `.jsonl` (`assistant` `tool_use` + `user` `tool_result` + human redirect)
    let claude_path = sessions_root.join("claude_session_02.jsonl");
    let claude_lines = [
        json!({
            "sessionId": "claude-s2",
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "先运行测试并默认开启远程同步。"},
                    {"type": "tool_use", "id": "toolu_01", "name": "Bash", "input": {"command": "cargo test"}}
                ]
            }
        }),
        json!({
            "sessionId": "claude-s2",
            "type": "user",
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_01",
                    "is_error": true,
                    "content": format!(
                        "Compiling ledger ({linux_home_leak})\nerror[E0277]: the trait bound `LedgerKey: FromStr` is not satisfied\nerror: could not compile `ledger` due to 1 previous error"
                    )
                }]
            }
        }),
        json!({
            "sessionId": "claude-s2",
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "toolu_02", "name": "Edit", "input": {"file_path": "src/key.rs"}}
                ]
            }
        }),
        json!({
            "sessionId": "claude-s2",
            "type": "user",
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_02",
                    "is_error": false,
                    "content": "Updated src/key.rs"
                }]
            }
        }),
        json!({
            "sessionId": "claude-s2",
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "toolu_03", "name": "Bash", "input": {"command": "cargo test"}}
                ]
            }
        }),
        json!({
            "sessionId": "claude-s2",
            "type": "user",
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_03",
                    "is_error": false,
                    "content": "Finished `test` profile [unoptimized] target(s) in 0.94s\ntest result: ok. 8 passed; 0 failed"
                }]
            }
        }),
        json!({
            "sessionId": "claude-s2",
            "type": "user",
            "message": {
                "role": "user",
                "content": "先别默认连远程网关，改成纯离线本地确定性校验。"
            }
        }),
    ]
    .iter()
    .map(Value::to_string)
    .collect::<Vec<_>>()
    .join("\n");
    fs::write(&claude_path, format!("{claude_lines}\n"))?;

    // 3. Format ③: Codex `.jsonl` (`response_item` `function_call` / `function_call_output`)
    let codex_path = sessions_root.join("rollout-codex-03.jsonl");
    let codex_lines = [
        json!({"type": "session_meta", "payload": {"id": "codex-s3"}}),
        json!({
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "call_id": "call_c1",
                "name": "exec_command",
                "arguments": "{\"cmd\":\"cargo clippy --workspace -- -D warnings\"}"
            }
        }),
        json!({
            "type": "response_item",
            "payload": {
                "type": "function_call_output",
                "call_id": "call_c1",
                "output": "Process exited with code 101\nerror: this let-binding has unit value\n  = note: `-D clippy::let-unit-value` implied by `-D warnings`\nerror: could not compile `app` due to 1 previous error"
            }
        }),
        json!({
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call",
                "call_id": "call_c2",
                "name": "apply_patch",
                "input": "*** Begin Patch\n*** Update File: src/app.rs\n*** End Patch"
            }
        }),
        json!({
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call_output",
                "call_id": "call_c2",
                "output": "Success. Updated the following files:\nM src/app.rs"
            }
        }),
        json!({
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "call_id": "call_c3",
                "name": "exec_command",
                "arguments": "{\"cmd\":\"cargo clippy --workspace -- -D warnings\"}"
            }
        }),
        json!({
            "type": "response_item",
            "payload": {
                "type": "function_call_output",
                "call_id": "call_c3",
                "output": "Finished `dev` profile [unoptimized] target(s) in 0.85s\nExit code: 0"
            }
        }),
    ]
    .iter()
    .map(Value::to_string)
    .collect::<Vec<_>>()
    .join("\n");
    fs::write(&codex_path, format!("{codex_lines}\n"))?;

    // 4. Format ④: Exported Markdown dialogue `.md` (providing a 2nd independent session for clippy lint recovery)
    let md_path = sessions_root.join("session_review_04.md");
    fs::write(
        &md_path,
        "## Codex · 2026-11-23\nerror: unnecessary use of `clone` to create a slice from a reference\n  = note: `-D clippy::cloned-ref-to-slice-refs` implied by `-D warnings`\nCommand exited with code 101\n\n## Codex · 2026-11-23\n已修改 slice 参数为 `std::slice::from_ref`。\nFinished `dev` profile [unoptimized] target(s) in 0.62s\nCommand exited with code 0\n",
    )?;

    #[cfg(unix)]
    {
        let outside = dir.path().join("outside_secret.jsonl");
        fs::write(&outside, "{}\n")?;
        std::os::unix::fs::symlink(&outside, sessions_root.join("linked_ignore.jsonl"))?;
        let symlink_direct = Command::new(env!("CARGO_BIN_EXE_kp"))
            .env("KP_OFFLINE", "1")
            .arg("--path")
            .arg(&lib)
            .arg("--json")
            .arg("dream")
            .arg("--sessions")
            .arg(sessions_root.join("linked_ignore.jsonl"))
            .output()?;
        assert!(
            !symlink_direct.status.success(),
            "explicit symlink `--sessions` target must be rejected"
        );
    }

    // ZERO `kp episode record` calls — run `kp dream --sessions <sessions_root>` directly!
    let dream_out = run(
        &lib,
        &[
            "--as",
            "owner",
            "dream",
            "--sessions",
            sessions_root.to_str().ok_or("utf8 sessions_root")?,
        ],
    )?;
    assert_eq!(dream_out["sessions_mined"], 4);
    assert!(
        dream_out["episodes_mined"].as_u64().unwrap_or(0) >= 6,
        "expected >=6 mined episodes across 4 real session formats, got: {dream_out}"
    );
    let promoted = dream_out["promoted"].as_array().ok_or("promoted array")?;
    assert!(
        promoted.len() >= 3,
        "expected >=3 promoted skills (`rust-type-and-borrow-signature-check`, `honor-explicit-user-redirect`, `clippy-strict-warnings-gate`), got: {dream_out}"
    );

    // Verify `kp skill list` & materialized `<lib>/skills/<slug>/SKILL.md` with `EVIDENCE_BOUNDARY`:
    let skills = run(&lib, &["--as", "owner", "skill", "list"])?;
    let skills_arr = skills.as_array().ok_or("skill list array")?;
    let skill_names: Vec<&str> = skills_arr
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    for expected_slug in [
        "rust-type-and-borrow-signature-check",
        "honor-explicit-user-redirect",
        "clippy-strict-warnings-gate",
    ] {
        assert!(
            skill_names.contains(&expected_slug),
            "missing promoted skill `{expected_slug}` in {skill_names:?}"
        );
        let skill_md_path = lib.join("skills").join(expected_slug).join("SKILL.md");
        assert!(
            skill_md_path.is_file(),
            "expected materialized SKILL.md at {}",
            skill_md_path.display()
        );
        let md_text = fs::read_to_string(&skill_md_path)?;
        assert!(md_text.contains(EVIDENCE_BOUNDARY));
        assert!(!md_text.contains("dev_lead_priv"));
        assert!(!md_text.contains("dev_ops_priv"));
        assert!(!md_text.contains("cjk_priv_lead"));
        assert!(!md_text.contains("arrow_priv_ops"));
        assert!(!md_text.contains(secret_leak));
    }

    // Verify SQLite tables & blobs have ZERO secret or raw personal-home-segment residue (including CJK & `-->` adjacent paths)!
    for forbidden in [
        secret_leak,
        "dev_lead_priv",
        "dev_ops_priv",
        "cjk_priv_lead",
        "arrow_priv_ops",
    ] {
        assert_eq!(
            kp_store::find_sqlite_table_containing_bytes(&lib, forbidden.as_bytes())?,
            None,
            "unexpected privacy residue `{forbidden}` inside SQLite library"
        );
    }

    // Trace full provenance chain: `SkillVersion -> PlaybookBullet -> kp.rsi.evidence -> L0 verbatim highlight`!
    let target_skill = skills_arr
        .iter()
        .find(|s| s["name"] == "rust-type-and-borrow-signature-check")
        .ok_or("target skill")?;
    let target_skill_fid: FactId = target_skill["fact_id"]
        .as_str()
        .ok_or("skill fact_id")?
        .parse()?;
    let why_skill = run(&lib, &["--as", "owner", "why", target_skill_fid.as_str()])?;
    assert_eq!(why_skill["fact_id"], target_skill_fid.as_str());
    assert_eq!(why_skill["justifications"][0]["rule_id"], "RSI_PROMOTION");

    fn collect_proof_fact_ids(node: &Value, out: &mut Vec<FactId>) -> Result<()> {
        if let Some(fid_str) = node["fact_id"].as_str() {
            let fid: FactId = fid_str.parse()?;
            if !out.contains(&fid) {
                out.push(fid);
            }
        }
        if let Some(justs) = node["justifications"].as_array() {
            for j in justs {
                if let Some(prems) = j["premises"].as_array() {
                    for p in prems {
                        collect_proof_fact_ids(p, out)?;
                    }
                }
            }
        }
        Ok(())
    }

    let mut proof_fids = Vec::new();
    collect_proof_fact_ids(&why_skill, &mut proof_fids)?;

    let mut bullet_fids = Vec::new();
    let mut evidence_fids = Vec::new();
    {
        let ro = Store::open_readonly(&lib)?;
        let graph = ro.derivation_view()?;
        for fid in &proof_fids {
            if let Some(fact) = graph.facts.get(fid) {
                if fact.kind == FactKind::PlaybookBullet {
                    bullet_fids.push(fid.clone());
                }
                if fact.predicate == "kp.rsi.evidence" {
                    evidence_fids.push(fid.clone());
                }
            }
        }
    }
    assert!(
        !bullet_fids.is_empty(),
        "expected PlaybookBullet premise on SkillVersion proof: {why_skill}"
    );
    assert!(
        evidence_fids.len() >= 2,
        "expected >=2 independent session kp.rsi.evidence premises on SkillVersion proof: {why_skill}"
    );

    {
        let ro = Store::open_readonly(&lib)?;
        let highlights =
            ro.inspector_fact_highlights(&"owner".parse()?, EpochMs(i64::MAX / 2), &evidence_fids)?;
        for efid in &evidence_fids {
            let hl = highlights
                .get(efid)
                .ok_or("missing inspector highlight for kp.rsi.evidence")?;
            assert!(
                !hl.excerpt_highlighted.is_empty(),
                "expected non-empty verbatim excerpt_highlighted for {efid}"
            );
            assert!(
                hl.excerpt_highlighted.contains("error[E0308]")
                    || hl.excerpt_highlighted.contains("error[E0277]"),
                "expected exact compiler error quote in highlight, got: {}",
                hl.excerpt_highlighted
            );
        }
    }

    // Bystander `carol` isolation: `skill list` and `log` disclose zero owner session/skill data!
    let carol_skills = run(&lib, &["--as", "carol", "skill", "list"])?;
    assert_eq!(carol_skills, json!([]));
    let carol_log = run(&lib, &["--as", "carol", "log"])?;
    let carol_log_str = serde_json::to_string(&carol_log)?;
    assert!(!carol_log_str.contains("settlement_validator"));
    assert!(!carol_log_str.contains("rust-type-and-borrow-signature-check"));

    // Premise withdrawal auto-rollback: `kp forget <one_evidence_fact_id>` immediately invalidates
    // `SkillVersion`, removes it from `kp skill list`, and deletes `<lib>/skills/<slug>/SKILL.md`!
    let withdrawn_premise = &evidence_fids[0];
    let forget_out = run(
        &lib,
        &["--as", "owner", "forget", withdrawn_premise.as_str()],
    )?;
    assert_eq!(
        forget_out["withdrawn"].as_array().map(Vec::len),
        Some(1),
        "expected 1 withdrawn evidence premise"
    );

    {
        let ro = Store::open_readonly(&lib)?;
        let graph = ro.derivation_view()?;
        let skill_after = graph
            .facts
            .get(&target_skill_fid)
            .ok_or("missing target_skill_fid in graph")?;
        assert_eq!(
            skill_after.status,
            FactStatus::Invalidated,
            "withdrawing a supporting L0 verbatim premise must invalidate SkillVersion"
        );
    }
    let skills_after = run(&lib, &["--as", "owner", "skill", "list"])?;
    let names_after: Vec<&str> = skills_after
        .as_array()
        .ok_or("skills_after array")?
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        !names_after.contains(&"rust-type-and-borrow-signature-check"),
        "invalidated skill must disappear from `kp skill list`, got: {names_after:?}"
    );
    let rolled_back_md = lib
        .join("skills")
        .join("rust-type-and-borrow-signature-check")
        .join("SKILL.md");
    assert!(
        !rolled_back_md.exists(),
        "SKILL.md on disk must be automatically removed when its premise is forgotten"
    );

    // 4b. Re-running `kp dream --sessions <sessions_root>` right after `kp forget` MUST succeed
    //     without any `facts.fact_id` PK collision, and when all premises of the skill are forgotten
    //     it cleanly re-learns & re-promotes version 2 and restores `SKILL.md` on disk!
    let dream_after_single_forget = run(
        &lib,
        &[
            "--as",
            "owner",
            "dream",
            "--sessions",
            sessions_root.to_str().ok_or("utf8 sessions_root")?,
        ],
    )?;
    assert!(
        dream_after_single_forget["episodes_mined"]
            .as_u64()
            .unwrap_or(0)
            >= 1,
        "re-running dream after single premise forget must succeed without PK conflict: {dream_after_single_forget}"
    );

    // Forget all currently active `kp.rsi.evidence` premises for `rust-type-and-borrow-signature-check`,
    // then re-run `kp dream --sessions` to verify full re-learning & `version: 2` skill restoration!
    {
        let ro = Store::open_readonly(&lib)?;
        let active_evs: Vec<FactId> = ro
            .visible_facts(&"owner".parse()?, EpochMs(i64::MAX / 2))?
            .into_iter()
            .filter(|f| {
                f.predicate == "kp.rsi.evidence"
                    && (f.object["summary"]
                        .as_str()
                        .is_some_and(|s| s.contains("E0308") || s.contains("E0277")))
            })
            .map(|f| f.id)
            .collect();
        drop(ro);
        for ev in &active_evs {
            run(&lib, &["--as", "owner", "forget", ev.as_str()])?;
        }
    }
    let dream_relearned = run(
        &lib,
        &[
            "--as",
            "owner",
            "dream",
            "--sessions",
            sessions_root.to_str().ok_or("utf8 sessions_root")?,
        ],
    )?;
    assert_eq!(
        dream_relearned["promoted"].as_array().map(Vec::len),
        Some(1),
        "expected forgotten skill to re-learn and re-promote cleanly: {dream_relearned}"
    );
    let skills_relearned = run(&lib, &["--as", "owner", "skill", "list"])?;
    let relearned_entry = skills_relearned
        .as_array()
        .ok_or("skills_relearned array")?
        .iter()
        .find(|s| s["name"] == "rust-type-and-borrow-signature-check")
        .ok_or("expected re-promoted rust-type-and-borrow-signature-check")?;
    assert_eq!(relearned_entry["version"], 2);
    assert!(
        rolled_back_md.is_file(),
        "re-learned skill must re-materialize SKILL.md on disk"
    );

    // 4c. Sticky `kp reject`: rejecting supporting evidence of `clippy-strict-warnings-gate` rolls back
    //     the skill AND prevents resurrection on subsequent `kp dream --sessions` runs!
    let clippy_skill_fid: FactId = skills_relearned
        .as_array()
        .ok_or("skills array")?
        .iter()
        .find(|s| s["name"] == "clippy-strict-warnings-gate")
        .and_then(|s| s["fact_id"].as_str())
        .ok_or("clippy skill fact_id")?
        .parse()?;
    let why_clippy = run(&lib, &["--as", "owner", "why", clippy_skill_fid.as_str()])?;
    let mut clippy_proof_fids = Vec::new();
    collect_proof_fact_ids(&why_clippy, &mut clippy_proof_fids)?;
    let clippy_ev_fids: Vec<FactId> = {
        let ro = Store::open_readonly(&lib)?;
        let graph = ro.derivation_view()?;
        clippy_proof_fids
            .into_iter()
            .filter(|fid| {
                graph
                    .facts
                    .get(fid)
                    .is_some_and(|f| f.predicate == "kp.rsi.evidence")
            })
            .collect()
    };
    assert!(!clippy_ev_fids.is_empty());
    for cef in &clippy_ev_fids {
        run(
            &lib,
            &[
                "--as",
                "owner",
                "reject",
                cef.as_str(),
                "--reason",
                "invalid assumption",
            ],
        )?;
    }
    let clippy_md = lib
        .join("skills")
        .join("clippy-strict-warnings-gate")
        .join("SKILL.md");
    assert!(!clippy_md.exists());

    let dream_after_reject = run(
        &lib,
        &[
            "--as",
            "owner",
            "dream",
            "--sessions",
            sessions_root.to_str().ok_or("utf8 sessions_root")?,
        ],
    )?;
    assert_eq!(
        dream_after_reject["episodes_mined"], 0,
        "rejected evidence spans must never be re-mined: {dream_after_reject}"
    );
    let skills_post_reject = run(&lib, &["--as", "owner", "skill", "list"])?;
    let post_reject_names: Vec<&str> = skills_post_reject
        .as_array()
        .ok_or("skills_post_reject array")?
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        !post_reject_names.contains(&"clippy-strict-warnings-gate"),
        "rejected skill must NEVER resurrect after re-running `kp dream --sessions`"
    );
    assert!(!clippy_md.exists());

    // 4d. Also verify `kp purge` (`StoreError::Tombstoned`) on a mined source does not abort `--sessions` scan!
    let any_dream_source_id: String = {
        let ro = Store::open_readonly(&lib)?;
        let graph = ro.derivation_view()?;
        graph
            .facts
            .values()
            .filter(|f| f.predicate == "kp.rsi.evidence" && f.status == FactStatus::Active)
            .find_map(|f| {
                f.source_anchor
                    .as_ref()
                    .and_then(|a| a.source_id.as_ref())
                    .map(|s| s.to_string())
            })
            .ok_or("expected active dream source_id")?
    };
    run(
        &lib,
        &[
            "--as",
            "owner",
            "purge",
            &any_dream_source_id,
            "--reason",
            "test tombstone tolerance",
        ],
    )?;
    let _dream_after_purge = run(
        &lib,
        &[
            "--as",
            "owner",
            "dream",
            "--sessions",
            sessions_root.to_str().ok_or("utf8 sessions_root")?,
        ],
    )?;

    // 5. Verify `KP_MODEL_POLICY=raw_only` makes ZERO external HTTP calls even when `--llm-url` is passed!
    let http_hits = Arc::new(AtomicUsize::new(0));
    let hits_clone = Arc::clone(&http_hits);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            hits_clone.fetch_add(1, Ordering::SeqCst);
            async { (StatusCode::INTERNAL_SERVER_ERROR, "unexpected cloud call") }
        }),
    );
    let srv = tokio::spawn(async move { axum::serve(listener, app).await });

    let raw_lib = dir.path().join("raw_only_lib");
    run(&raw_lib, &["init", "--owner", "owner", "--no-embedding"])?;
    let raw_dream = success(
        Command::new(env!("CARGO_BIN_EXE_kp"))
            .env("KP_OFFLINE", "1")
            .env("KP_MODEL_POLICY", "raw_only")
            .arg("--path")
            .arg(&raw_lib)
            .args([
                "--as",
                "owner",
                "--json",
                "dream",
                "--sessions",
                sessions_root.to_str().ok_or("utf8 sessions_root")?,
                "--llm-url",
                &format!("http://{addr}/v1/chat/completions"),
                "--llm-model",
                "mock-dream-llm",
                "--allow-cloud-llm",
            ])
            .output()?,
    )?;
    srv.abort();

    assert_eq!(
        http_hits.load(Ordering::SeqCst),
        0,
        "RawOnly session episodes must NEVER send any HTTP request to external dream LLM"
    );
    assert!(
        raw_dream["promoted"]
            .as_array()
            .is_some_and(|arr| arr.len() >= 3),
        "RawOnly fallback to local StructuredDreamAttributor must still promote skills offline"
    );

    // 6. Also verify automatic in-store unmined session discovery (`Store::unmined_session_source_ids`)!
    let store_auto_lib = dir.path().join("auto_store_lib");
    run(
        &store_auto_lib,
        &["init", "--owner", "owner", "--no-embedding"],
    )?;
    let ingest_file = dir.path().join("stored_sessions.jsonl");
    let s_jsonl = [
        json!({
            "source": {"kind": "transcript", "uri": "transcript:s-auto-1", "external_id": "auto-s1"},
            "speaker": "owner",
            "audience": {"explicit": ["owner"]},
            "occurred_at": 1_763_900_000_000_i64,
            "content": "[speaker:owner] 请更新配置结构。\n[assistant] 我先直接跳过版本字段。\n[speaker:owner] 不对，应该严格保留版本号字段并做向后兼容校验。",
            "op": "upsert"
        }),
        json!({
            "source": {"kind": "transcript", "uri": "transcript:s-auto-2", "external_id": "auto-s2"},
            "speaker": "owner",
            "audience": {"explicit": ["owner"]},
            "occurred_at": 1_763_900_010_000_i64,
            "content": "[speaker:owner] 请实现删除接口。\n[assistant] 我直接软删除不记墓碑。\n[speaker:owner] 不要只做软删除，改成写入不可逆墓碑记录。",
            "op": "upsert"
        }),
    ]
    .iter()
    .map(Value::to_string)
    .collect::<Vec<_>>()
    .join("\n");
    fs::write(&ingest_file, format!("{s_jsonl}\n"))?;
    run(
        &store_auto_lib,
        &[
            "--as",
            "owner",
            "ingest",
            ingest_file.to_str().ok_or("utf8")?,
        ],
    )?;

    let auto_dream_1 = run(&store_auto_lib, &["--as", "owner", "dream"])?;
    assert_eq!(auto_dream_1["sessions_mined"], 2);
    assert_eq!(auto_dream_1["episodes_mined"], 2);
    assert_eq!(auto_dream_1["promoted"].as_array().map(Vec::len), Some(1));

    // Second `kp dream` must see 0 unmined sessions (`dream_mined_source` idempotence).
    let auto_dream_2 = run(&store_auto_lib, &["--as", "owner", "dream"])?;
    assert_eq!(auto_dream_2["sessions_mined"], 0);
    assert_eq!(auto_dream_2["episodes_mined"], 0);

    // 7. Container-scoped sessions (`container_id`): when `owner` departs the container via
    //    `PublicContainerRoster`, the mined `kp.rsi.evidence`, promoted `SkillVersion`, AND
    //    materialized `SKILL.md` on disk are all immediately revoked/removed!
    let scoped_lib = dir.path().join("scoped_dream_lib");
    run(
        &scoped_lib,
        &["init", "--owner", "owner", "--community", "--no-embedding"],
    )?;
    run(
        &scoped_lib,
        &["member", "add", "bob", "--role", "maintainer"],
    )?;
    let scoped_ingest = dir.path().join("scoped_sessions.jsonl");
    let scoped_lines = [
        json!({
            "container_id": "scope.ops_sessions",
            "label": "运维复盘群",
            "members": ["owner", "bob"],
            "all_library_members": false,
            "synced_at": 1_763_910_000_000_i64
        }),
        json!({
            "source": {"kind": "agent-session", "uri": "agent-session:ops-s1", "external_id": "ops-s1", "container_id": "scope.ops_sessions"},
            "speaker": "owner",
            "audience": "container_members",
            "occurred_at": 1_763_910_001_000_i64,
            "content": "[speaker:owner] 请配置备份策略。\n[assistant] 默认只保留单副本。\n[speaker:owner] 不对，应该强制开启跨可用区双副本校验。",
            "op": "upsert"
        }),
        json!({
            "source": {"kind": "agent-session", "uri": "agent-session:ops-s2", "external_id": "ops-s2", "container_id": "scope.ops_sessions"},
            "speaker": "owner",
            "audience": "container_members",
            "occurred_at": 1_763_910_002_000_i64,
            "content": "[speaker:owner] 请生成演练计划。\n[assistant] 直接在主库执行压测。\n[speaker:owner] 不要在主库压测，改成在隔离影子库执行压测。",
            "op": "upsert"
        }),
    ]
    .iter()
    .map(Value::to_string)
    .collect::<Vec<_>>()
    .join("\n");
    fs::write(&scoped_ingest, format!("{scoped_lines}\n"))?;
    run(
        &scoped_lib,
        &[
            "--as",
            "owner",
            "ingest",
            scoped_ingest.to_str().ok_or("utf8")?,
        ],
    )?;

    let scoped_dream = run(&scoped_lib, &["--as", "owner", "dream"])?;
    assert_eq!(scoped_dream["promoted"].as_array().map(Vec::len), Some(1));
    let scoped_skill_md = scoped_lib
        .join("skills")
        .join("honor-explicit-user-redirect")
        .join("SKILL.md");
    assert!(scoped_skill_md.is_file());

    // Now `bob` (maintainer) updates `scope.ops_sessions` roster to remove `owner` (`members: ["bob"]`):
    let depart_file = dir.path().join("depart_roster.jsonl");
    let depart_line = json!({
        "container_id": "scope.ops_sessions",
        "label": "运维复盘群",
        "members": ["bob"],
        "all_library_members": false,
        "synced_at": 1_763_910_099_000_i64
    });
    fs::write(&depart_file, format!("{depart_line}\n"))?;
    run(
        &scoped_lib,
        &["--as", "bob", "ingest", depart_file.to_str().ok_or("utf8")?],
    )?;

    let owner_skills_after_depart = run(&scoped_lib, &["--as", "owner", "skill", "list"])?;
    assert_eq!(
        owner_skills_after_depart,
        json!([]),
        "departed container member must immediately lose visibility of container-derived skills"
    );
    let owner_search_after_depart =
        run(&scoped_lib, &["--as", "owner", "search", "跨可用区双副本"])?;
    assert!(
        owner_search_after_depart["results"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "departed container member must not read container session kp.rsi.evidence"
    );
    assert!(
        !scoped_skill_md.exists(),
        "materialized SKILL.md on disk must be immediately cleaned up when owner departs the source container"
    );

    let _ = PathBuf::new();
    Ok(())
}
