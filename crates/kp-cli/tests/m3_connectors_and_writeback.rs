mod support;

use std::fs;

use kp_model::FactId;

use crate::support::{run, Result};

fn replace_entry_line_claim(index_md: &str, fact_id: &FactId, new_claim: &str) -> Result<String> {
    let marker = format!("<!-- kp-entry: {fact_id} -->");
    let mut out = Vec::new();
    let mut replace_next_bullet = false;
    let mut replaced = false;

    for line in index_md.lines() {
        if line.trim() == marker {
            replace_next_bullet = true;
            out.push(line.to_owned());
            continue;
        }
        if replace_next_bullet && line.starts_with("- **[kp-ref:") {
            let Some((prefix, _old_claim)) = line.split_once("`) ") else {
                return Err("unexpected exported bullet line format".into());
            };
            out.push(format!("{prefix}`) {new_claim}"));
            replace_next_bullet = false;
            replaced = true;
            continue;
        }
        out.push(line.to_owned());
    }
    if !replaced {
        return Err(format!("entry marker `{marker}` not found in index.md").into());
    }
    let mut joined = out.join("\n");
    joined.push('\n');
    Ok(joined)
}

fn remove_entry_block(index_md: &str, fact_id: &FactId) -> Result<String> {
    let marker = format!("<!-- kp-entry: {fact_id} -->");
    let lines: Vec<&str> = index_md.lines().collect();
    let mut out = Vec::new();
    let mut idx = 0;
    let mut removed = false;

    while idx < lines.len() {
        if lines[idx].trim() == marker {
            removed = true;
            idx += 1;
            if idx < lines.len() && lines[idx].starts_with("- **[kp-ref:") {
                idx += 1;
                while idx < lines.len() && lines[idx].starts_with("  - ") {
                    idx += 1;
                }
            }
            continue;
        }
        out.push(lines[idx]);
        idx += 1;
    }
    if !removed {
        return Err(format!("entry marker `{marker}` to remove not found in index.md").into());
    }
    let mut joined = out.join("\n");
    joined.push('\n');
    Ok(joined)
}

#[test]
fn export_edit_delete_conflict_freeform_apply_review_resolve_roundtrip_and_audit_log_isolation(
) -> Result {
    let dir = tempfile::tempdir()?;
    let lib = dir.path().join("m3_wb_lib");

    run(&lib, &["init", "--owner", "owner", "--community"])?;
    run(&lib, &["member", "add", "bob", "--role", "maintainer"])?;
    run(&lib, &["member", "add", "carol", "--role", "member"])?;

    // 1. Seed facts: two shared `owner` facts, one private `owner` fact, and one shared `bob` fact.
    let f_edit_resp = run(
        &lib,
        &[
            "--as",
            "owner",
            "remember",
            "--subject",
            "ledger_retention",
            "--predicate",
            "kp.core.decision",
            "--value",
            r#"{"summary":"账本冷备归档周期设为 90 天"}"#,
            "--kind",
            "decision",
            "--valid-from",
            "0",
            "--audience",
            "owner,bob,carol",
        ],
    )?;
    let fid_edit: FactId = f_edit_resp["facts"][0]
        .as_str()
        .ok_or("missing fid_edit")?
        .parse()?;

    let f_del_resp = run(
        &lib,
        &[
            "--as",
            "owner",
            "remember",
            "--subject",
            "legacy_batch_cron",
            "--predicate",
            "kp.core.decision",
            "--value",
            r#"{"summary":"保留旧版凌晨三点对账批处理脚本"}"#,
            "--kind",
            "decision",
            "--valid-from",
            "0",
            "--audience",
            "owner,bob,carol",
        ],
    )?;
    let fid_del: FactId = f_del_resp["facts"][0]
        .as_str()
        .ok_or("missing fid_del")?
        .parse()?;

    let _f_priv_resp = run(
        &lib,
        &[
            "--as",
            "owner",
            "remember",
            "--subject",
            "owner_private_vault",
            "--predicate",
            "kp.core.note",
            "--value",
            r#"{"summary":"OWNER_PRIVATE_NOTE_991 个人审计备忘仅本人可见"}"#,
            "--kind",
            "observation",
            "--valid-from",
            "0",
            "--audience",
            "owner",
        ],
    )?;

    let f_bob_resp = run(
        &lib,
        &[
            "--as",
            "bob",
            "remember",
            "--subject",
            "replica_pool",
            "--predicate",
            "kp.core.decision",
            "--value",
            r#"{"summary":"只读报表流量统一路由至备库连接池"}"#,
            "--kind",
            "decision",
            "--valid-from",
            "0",
            "--audience",
            "owner,bob,carol",
        ],
    )?;
    let fid_bob: FactId = f_bob_resp["facts"][0]
        .as_str()
        .ok_or("missing fid_bob")?
        .parse()?;

    // 2. `kp export <dir1>` -> edit `fid_edit` + delete `fid_del` -> `kp apply <dir1>` (clean baseline!).
    let exp_dir1 = dir.path().join("exp1");
    let exp1 = run(
        &lib,
        &["--as", "owner", "export", exp_dir1.to_str().ok_or("utf8")?],
    )?;
    assert_eq!(exp1["visible_facts"], 4);

    let index1_path = exp_dir1.join("index.md");
    let raw_index1 = fs::read_to_string(&index1_path)?;
    let edited_index1 = replace_entry_line_claim(
        &raw_index1,
        &fid_edit,
        "账本冷备归档周期延长为 180 天并启用对象锁",
    )?;
    let edited_and_deleted_index1 = remove_entry_block(&edited_index1, &fid_del)?;
    fs::write(&index1_path, edited_and_deleted_index1)?;

    let apply1 = run(
        &lib,
        &["--as", "owner", "apply", exp_dir1.to_str().ok_or("utf8")?],
    )?;
    assert_eq!(apply1["unchanged"], false);
    assert_eq!(apply1["baseline_conflict"], false);
    assert_eq!(apply1["updated_facts"].as_array().map(Vec::len), Some(1));
    assert_eq!(apply1["deleted_fact_ids"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        apply1["enqueued_proposals"].as_array().map(Vec::len),
        Some(0)
    );

    // Second `kp apply <dir1>` is a clean idempotent no-op (`unchanged: true`).
    let apply1_repeat = run(
        &lib,
        &["--as", "owner", "apply", exp_dir1.to_str().ok_or("utf8")?],
    )?;
    assert_eq!(apply1_repeat["unchanged"], true);

    // Re-export to `<dir2>` and verify updated claim is present and deleted claim is gone.
    let exp_dir2 = dir.path().join("exp2");
    let exp2 = run(
        &lib,
        &["--as", "owner", "export", exp_dir2.to_str().ok_or("utf8")?],
    )?;
    assert_eq!(exp2["visible_facts"], 3);
    let index2_path = exp_dir2.join("index.md");
    let raw_index2 = fs::read_to_string(&index2_path)?;
    assert!(raw_index2.contains("账本冷备归档周期延长为 180 天并启用对象锁"));
    assert!(!raw_index2.contains("保留旧版凌晨三点对账批处理脚本"));

    let apply2_clean = run(
        &lib,
        &["--as", "owner", "apply", exp_dir2.to_str().ok_or("utf8")?],
    )?;
    assert_eq!(apply2_clean["unchanged"], true);

    // 3. Advance `change_log` `base_seq` behind `<dir2>`'s back, while editing `fid_bob` AND appending a
    //    freeform Markdown paragraph in stale `<dir2>/index.md` -> `baseline_conflict == true` + 2 proposals!
    let _concurrent = run(
        &lib,
        &[
            "--as",
            "bob",
            "remember",
            "--subject",
            "concurrent_background_fact",
            "--predicate",
            "kp.core.decision",
            "--value",
            r#"{"summary":"后台并发新增：对账服务开启幂等流水号校验"}"#,
            "--kind",
            "decision",
            "--valid-from",
            "0",
            "--audience",
            "owner,bob,carol",
        ],
    )?;

    let mut stale_edited_index2 = replace_entry_line_claim(
        &raw_index2,
        &fid_bob,
        "只读报表流量统一路由至双活备库连接池并限流",
    )?;
    stale_edited_index2.push_str("\n补充手写架构手记：跨地域备库延迟超过两秒时自动摘除读权重。\n");
    fs::write(&index2_path, stale_edited_index2)?;

    let apply2_conflict = run(
        &lib,
        &["--as", "owner", "apply", exp_dir2.to_str().ok_or("utf8")?],
    )?;
    assert_eq!(apply2_conflict["baseline_conflict"], true);
    assert_eq!(
        apply2_conflict["enqueued_proposals"]
            .as_array()
            .map(Vec::len),
        Some(2),
        "both stale edit and freeform note must be queued losslessly: {apply2_conflict}"
    );

    // Verify `kp review list` shows both open `writeback_proposal` items, then confirm both via positional `confirm`!
    let open_reviews = run(&lib, &["--as", "owner", "review", "list"])?;
    let rev_arr = open_reviews.as_array().ok_or("review list array")?;
    let wb_rev_ids: Vec<String> = rev_arr
        .iter()
        .filter(|r| r["kind"] == "writeback_proposal" && r["status"] == "open")
        .filter_map(|r| r["id"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(wb_rev_ids.len(), 2);

    let mut confirmed_wb_fids = Vec::new();
    for rev_id in &wb_rev_ids {
        let resolved = run(
            &lib,
            &["--as", "owner", "review", "resolve", rev_id, "confirm"],
        )?;
        assert_eq!(resolved["status"], "confirmed");
        let new_fid_str = resolved["replacement_fact_id"]
            .as_str()
            .ok_or("expected replacement_fact_id string")?;
        confirmed_wb_fids.push(new_fid_str.parse::<FactId>()?);
    }

    // Re-export to `<dir3>` and verify both the confirmed conflict edit and the freeform note are visible!
    let exp_dir3 = dir.path().join("exp3");
    let _exp3 = run(
        &lib,
        &["--as", "owner", "export", exp_dir3.to_str().ok_or("utf8")?],
    )?;
    let raw_index3 = fs::read_to_string(exp_dir3.join("index.md"))?;
    assert!(raw_index3.contains("只读报表流量统一路由至双活备库连接池并限流"));
    assert!(raw_index3.contains("跨地域备库延迟超过两秒时自动摘除读权重"));

    // Verify default `KP_OFFLINE=1` writeback (`apply` + `review resolve confirm`) does NOT
    // pollute ordinary `HostCli` facts with `RawOnly`, while explicit `KP_MODEL_POLICY=raw_only`
    // stamps freeform writeback as `RawOnly`.
    let apply1_new_fid: FactId = apply1["updated_facts"][0]["new_fact_id"]
        .as_str()
        .ok_or("apply1 new_fact_id")?
        .parse()?;
    {
        let store_ro = kp_store::Store::open_readonly(&lib)?;
        let view = store_ro.derivation_view()?;
        for fid in std::iter::once(&apply1_new_fid).chain(confirmed_wb_fids.iter()) {
            let fact = view.facts.get(fid).ok_or("missing writeback fact")?;
            assert!(
                !fact.tags.contains("kp.raw_only") && !fact.tags.contains("raw_only"),
                "KP_OFFLINE=1 writeback must not stamp kp.raw_only on ordinary fact {fid}"
            );
            assert_eq!(
                store_ro.fact_effective_route_policy(fact)?,
                kp_model::ModelRoutePolicy::HostCli
            );
        }
    }

    // Explicit `KP_MODEL_POLICY=raw_only` freeform writeback -> lands with `ModelRoutePolicy::RawOnly`.
    fs::write(
        exp_dir3.join("index.md"),
        format!("{raw_index3}\n合规离线手写附录：离线密钥分片仅保存在硬件模块内。\n"),
    )?;
    let apply_raw_out = std::process::Command::new(env!("CARGO_BIN_EXE_kp"))
        .env("KP_OFFLINE", "1")
        .env("KP_MODEL_POLICY", "raw_only")
        .arg("--path")
        .arg(&lib)
        .args([
            "--as",
            "owner",
            "--json",
            "apply",
            exp_dir3.to_str().ok_or("utf8")?,
        ])
        .output()?;
    let apply_raw = crate::support::success(apply_raw_out)?;
    let raw_rev_id = apply_raw["enqueued_proposals"][0]
        .as_str()
        .ok_or("expected raw_only enqueued proposal")?;
    let resolve_raw_out = std::process::Command::new(env!("CARGO_BIN_EXE_kp"))
        .env("KP_OFFLINE", "1")
        .env("KP_MODEL_POLICY", "raw_only")
        .arg("--path")
        .arg(&lib)
        .args([
            "--as", "owner", "--json", "review", "resolve", raw_rev_id, "confirm",
        ])
        .output()?;
    let resolve_raw = crate::support::success(resolve_raw_out)?;
    let raw_fid: FactId = resolve_raw["replacement_fact_id"]
        .as_str()
        .ok_or("expected raw_only replacement_fact_id")?
        .parse()?;
    {
        let store_ro = kp_store::Store::open_readonly(&lib)?;
        let view = store_ro.derivation_view()?;
        let raw_fact = view.facts.get(&raw_fid).ok_or("missing raw_only fact")?;
        assert_eq!(
            store_ro.fact_effective_route_policy(raw_fact)?,
            kp_model::ModelRoutePolicy::RawOnly
        );
    }

    // 4. `kp log` viewer isolation: `owner` sees full history (`writeback_supersede`, `forget`,
    //    `enqueue_review`, `resolve_review`, private note), whereas `carol` NEVER sees `owner`'s
    //    private facts, private `writeback:` sources, or personal `review` events!
    let owner_log = run(&lib, &["--as", "owner", "log", "--limit", "100"])?;
    let owner_entries = owner_log.as_array().ok_or("owner log array")?;
    let owner_ops: Vec<&str> = owner_entries
        .iter()
        .filter_map(|e| e["op"].as_str())
        .collect();
    for expected_op in [
        "writeback_supersede",
        "forget",
        "enqueue_review",
        "resolve_review",
    ] {
        assert!(
            owner_ops.contains(&expected_op),
            "expected `{expected_op}` in owner audit log, got: {owner_ops:?}"
        );
    }
    let owner_log_dump = serde_json::to_string(&owner_log)?;
    assert!(owner_log_dump.contains("OWNER_PRIVATE_NOTE_991"));

    let carol_log = run(&lib, &["--as", "carol", "log", "--limit", "100"])?;
    let carol_entries = carol_log.as_array().ok_or("carol log array")?;
    let carol_ops: Vec<&str> = carol_entries
        .iter()
        .filter_map(|e| e["op"].as_str())
        .collect();
    assert!(!carol_ops.contains(&"enqueue_review"));
    assert!(!carol_ops.contains(&"resolve_review"));
    let carol_log_dump = serde_json::to_string(&carol_log)?;
    assert!(
        !carol_log_dump.contains("OWNER_PRIVATE_NOTE_991"),
        "carol log must never leak owner private note!"
    );
    assert!(
        !carol_log_dump.contains("跨地域备库延迟超过两秒时自动摘除读权重"),
        "carol log must never leak owner private freeform writeback note!"
    );
    assert!(
        carol_log_dump.contains("账本冷备归档周期延长为 180 天并启用对象锁"),
        "carol log should include shared `all` writeback update"
    );

    Ok(())
}

#[cfg(feature = "connectors")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_connectors_public_ingest_dual_pre_tool_use_and_departure_or_recall_invalidation(
) -> Result {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};

    use axum::{extract::State, routing::get, Json, Router};
    use serde_json::{json, Value};

    use crate::support::success;

    #[derive(Clone)]
    struct MockGhState {
        hits: Arc<Mutex<Vec<String>>>,
    }

    let dir = tempfile::tempdir()?;
    let lib = dir.path().join("m3_conn_lib");
    let state_path = dir.path().join("connectors_state.json");
    let sessions_dir = dir.path().join("sessions");
    fs::create_dir_all(&sessions_dir)?;

    // 1. Initialize community library (`owner`, `bob` maintainer, `carol` member outside Feishu ops group)
    //    and bind external identities `feishu:ou_owner -> owner`, `feishu:ou_bob -> bob`.
    run(&lib, &["init", "--owner", "owner", "--community"])?;
    run(&lib, &["member", "add", "bob", "--role", "maintainer"])?;
    run(&lib, &["member", "add", "carol", "--role", "member"])?;
    run(
        &lib,
        &[
            "--as",
            "owner",
            "identity",
            "bind",
            "feishu:ou_owner",
            "owner",
        ],
    )?;
    run(
        &lib,
        &["--as", "owner", "identity", "bind", "feishu:ou_bob", "bob"],
    )?;

    // 2. Write local `.jsonl` session file for `agent-session` connector backfill.
    let session_file = sessions_dir.join("ops-rollout-01.jsonl");
    fs::write(
        &session_file,
        concat!(
            r#"{"type":"event_msg","timestamp":"2026-11-22T09:00:00Z","payload":{"type":"user_message","message":"会话回填记录：清算账本对账批次统一采用幂等重放令牌。"}}"#,
            "\n"
        ),
    )?;

    // 3. Build wave-switched `mock_lark_cli.sh`:
    //    - Wave 1: group members = `{ou_owner, ou_bob}`, rule message `om_ops_2001` (`deleted: false`)
    //    - Wave 2 (Path A): `ou_bob` departs group (`{ou_owner}` only!)
    //    - Wave 3 (Path B): `om_ops_2001` recalled (`deleted: true` -> `op: "withdraw"`)
    let wave_file = dir.path().join("feishu_wave.txt");
    fs::write(&wave_file, "1")?;

    let f_mem1 = dir.path().join("mem_w1.json");
    fs::write(
        &f_mem1,
        serde_json::to_string(&json!({
            "ok": true,
            "identity": "user",
            "data": {
                "items": [
                    {"member_id_type": "open_id", "member_id": "ou_owner", "name": "Owner"},
                    {"member_id_type": "open_id", "member_id": "ou_bob", "name": "Bob"}
                ],
                "page_token": "",
                "has_more": false
            }
        }))?,
    )?;
    let f_mem2 = dir.path().join("mem_w2.json");
    fs::write(
        &f_mem2,
        serde_json::to_string(&json!({
            "ok": true,
            "identity": "user",
            "data": {
                "items": [
                    {"member_id_type": "open_id", "member_id": "ou_owner", "name": "Owner"}
                ],
                "page_token": "",
                "has_more": false
            }
        }))?,
    )?;

    let f_msg1 = dir.path().join("msg_w1.json");
    fs::write(
        &f_msg1,
        serde_json::to_string(&json!({
            "ok": true,
            "identity": "user",
            "data": {
                "messages": [{
                    "message_id": "om_ops_2001",
                    "msg_type": "text",
                    "create_time": "1795340000000",
                    "sender": {"id": "ou_owner", "name": "Owner"},
                    "content": "在生产事故窗口内严禁执行 drop table orders_ledger",
                    "deleted": false,
                    "updated": false
                }],
                "total": 1,
                "has_more": false,
                "page_token": ""
            }
        }))?,
    )?;
    let f_msg3 = dir.path().join("msg_w3_recall.json");
    fs::write(
        &f_msg3,
        serde_json::to_string(&json!({
            "ok": true,
            "identity": "user",
            "data": {
                "messages": [{
                    "message_id": "om_ops_2001",
                    "msg_type": "text",
                    "create_time": "1795340000000",
                    "sender": {"id": "ou_owner", "name": "Owner"},
                    "content": "recalled",
                    "deleted": true,
                    "updated": false
                }],
                "total": 1,
                "has_more": false,
                "page_token": ""
            }
        }))?,
    )?;

    let lark_poll_log = dir.path().join("lark_polls.log");
    let mock_lark = dir.path().join("mock-lark-cli.sh");
    fs::write(
        &mock_lark,
        format!(
            "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$*\" >> \"{}\"\nWAVE=$(cat \"{}\")\ncase \"$*\" in\n  *\"im chat.members get\"*)\n    if [ \"$WAVE\" = \"1\" ]; then cat \"{}\"; else cat \"{}\"; fi\n    ;;\n  *\"im +chat-messages-list\"*)\n    if [ \"$WAVE\" = \"3\" ]; then cat \"{}\"; else cat \"{}\"; fi\n    ;;\n  *)\n    echo '{{\"ok\":true,\"data\":{{\"items\":[],\"has_more\":false}}}}'\n    ;;\nesac\n",
            lark_poll_log.display(),
            wave_file.display(),
            f_mem1.display(),
            f_mem2.display(),
            f_msg3.display(),
            f_msg1.display(),
        ),
    )?;
    fs::set_permissions(&mock_lark, fs::Permissions::from_mode(0o755))?;

    // 4. Start local `127.0.0.1:0` `axum` GitHub mock server covering repo metadata, issues + PRs, and PR review comments
    //    (with `mock-gh` CLI fallback if TCP loopback bind is blocked by an outer sandbox).
    let gh_hits = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut gh_base_url: Option<String> = None;
    let mut gh_srv_guard = None;

    if let Ok(listener) = tokio::net::TcpListener::bind("127.0.0.1:0").await {
        let addr = listener.local_addr()?;
        gh_base_url = Some(format!("http://{addr}"));
        let state = MockGhState {
            hits: Arc::clone(&gh_hits),
        };
        let app = Router::new()
            .route(
                "/repos/org-ops/ledger-service",
                get(|State(st): State<MockGhState>| async move {
                    if let Ok(mut g) = st.hits.lock() {
                        g.push("repo".into());
                    }
                    Json(json!({
                        "private": false,
                        "visibility": "public",
                        "owner": {"login": "gh_owner"}
                    }))
                }),
            )
            .route(
                "/repos/org-ops/ledger-service/issues",
                get(|State(st): State<MockGhState>| async move {
                    if let Ok(mut g) = st.hits.lock() {
                        g.push("issues_and_prs".into());
                    }
                    Json(json!([
                        {
                            "number": 11,
                            "title": "Ledger snapshot checksum verification",
                            "body": "Ensure SHA-256 verification runs before archival.",
                            "updated_at": "2026-11-22T09:05:00Z",
                            "user": {"login": "gh_owner"}
                        },
                        {
                            "number": 12,
                            "title": "Add read-only replica circuit breaker",
                            "body": "PR introducing circuit breaker thresholds.",
                            "pull_request": {"url": "https://api.github.com/repos/org-ops/ledger-service/pulls/12"},
                            "updated_at": "2026-11-22T09:06:00Z",
                            "user": {"login": "gh_bob"}
                        }
                    ]))
                }),
            )
            .route(
                "/repos/org-ops/ledger-service/issues/comments",
                get(|| async { Json(json!([])) }),
            )
            .route(
                "/repos/org-ops/ledger-service/pulls/comments",
                get(|State(st): State<MockGhState>| async move {
                    if let Ok(mut g) = st.hits.lock() {
                        g.push("pr_reviews".into());
                    }
                    Json(json!([{
                        "id": 501,
                        "path": "src/ledger.rs",
                        "body": "PR 评审结论：账本写入事务必须保持幂等序列号校验。",
                        "updated_at": "2026-11-22T09:07:00Z",
                        "user": {"login": "gh_owner"}
                    }]))
                }),
            )
            .route(
                "/repos/org-ops/ledger-service/releases",
                get(|| async { Json(json!([])) }),
            )
            .with_state(state);
        gh_srv_guard = Some(tokio::spawn(
            async move { axum::serve(listener, app).await },
        ));
    }

    let mock_gh = dir.path().join("mock-gh.sh");
    fs::write(
        &mock_gh,
        r#"#!/bin/sh
set -eu
LAST=""
for arg in "$@"; do LAST="$arg"; done
printf "HTTP/2.0 200 OK\r\nETag: \"etag-m3\"\r\nX-RateLimit-Remaining: 4999\r\n\r\n"
case "$LAST" in
  "/repos/org-ops/ledger-service")
    echo '{"private":false,"visibility":"public","owner":{"login":"gh_owner"}}'
    ;;
  /repos/org-ops/ledger-service/issues\?*)
    echo '[{"number":11,"title":"Ledger snapshot checksum verification","body":"Ensure SHA-256 verification runs before archival.","updated_at":"2026-11-22T09:05:00Z","user":{"login":"gh_owner"}},{"number":12,"title":"Add read-only replica circuit breaker","body":"PR introducing circuit breaker thresholds.","pull_request":{"url":"https://api.github.com/repos/org-ops/ledger-service/pulls/12"},"updated_at":"2026-11-22T09:06:00Z","user":{"login":"gh_bob"}}]'
    ;;
  /repos/org-ops/ledger-service/pulls/comments*)
    echo '[{"id":501,"path":"src/ledger.rs","body":"PR 评审结论：账本写入事务必须保持幂等序列号校验。","updated_at":"2026-11-22T09:07:00Z","user":{"login":"gh_owner"}}]'
    ;;
  *)
    echo '[]'
    ;;
esac
"#,
    )?;
    fs::set_permissions(&mock_gh, fs::Permissions::from_mode(0o755))?;

    // 5. Build deterministic mock host CLI extractor (`KP_HOST_CLI_PROGRAM`) for `kp connector sync --extract`.
    let mock_host_cli = dir.path().join("mock-codex-extractor.py");
    let extractor_py = r#"#!/usr/bin/env python3
import json, re, sys
prompt = sys.stdin.read()
if "<chunk id=" not in prompt:
    print(json.dumps({"verdict": "unrelated", "question": None}))
    sys.exit(0)

m = re.search(r'<chunk id="([^"]+)"', prompt)
cid = m.group(1) if m else "c-0"
body = prompt.split("<chunk id=", 1)[1]
atoms = []
rule_quote = "在生产事故窗口内严禁执行 drop table orders_ledger"
if rule_quote in body:
    atoms.append({
        "kind": "constraint",
        "claim_text": rule_quote,
        "value_json": json.dumps({"hard_guard": True, "command": "drop table orders_ledger"}),
        "scope": {
            "scope": "named_mentions",
            "mentions": ["owner", "bob"],
            "group_concept": None,
        },
        "ambiguous_scope": None,
        "condition_concept": {
            "ref_type": "propose_new",
            "concept_id": None,
            "kind": "condition",
            "label": "生产事故窗口",
            "domain": "ops",
            "polarity": "neutral",
        },
        "action_concept": {
            "ref_type": "propose_new",
            "concept_id": None,
            "kind": "action",
            "label": "drop table orders_ledger",
            "domain": "ops",
            "polarity": "negative",
        },
        "group_concept": None,
        "member_mention": None,
        "valid_dates": None,
        "speaker": "owner",
        "assumed_premise_ids": [],
        "quote": {
            "chunk_id": cid,
            "verbatim_quote": rule_quote,
            "turn_ordinal": 0,
            "approx_char_offset": None,
        },
        "write_op": {"op": "add", "target_fact_id": None},
    })
print(json.dumps({"skip_summary": len(atoms) == 0, "atoms": atoms}, ensure_ascii=False))
"#;
    fs::write(&mock_host_cli, extractor_py)?;
    fs::set_permissions(&mock_host_cli, fs::Permissions::from_mode(0o755))?;

    let kp_cmd = |lib_path: &Path| -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kp"));
        cmd.env("KP_OFFLINE", "1")
            .env("KP_CONNECTOR_STATE_PATH", &state_path)
            .env("KP_LARK_CLI_BIN", &mock_lark)
            .env("KP_GH_CLI_BIN", &mock_gh)
            .env("KP_HOST_CLI_PROGRAM", &mock_host_cli)
            .env("KP_HOST_CLI_KIND", "codex")
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .arg("--path")
            .arg(lib_path);
        if let Some(ref url) = gh_base_url {
            cmd.env("KP_GITHUB_API_BASE", url);
        } else {
            cmd.env_remove("KP_GITHUB_API_BASE");
        }
        cmd
    };

    // 6. Add Feishu, GitHub, and `agent-session` scopes; verify Feishu's transparent one-line notice
    //    and `0600` permissions on `KP_CONNECTOR_STATE_PATH`!
    let add_feishu = kp_cmd(&lib)
        .args([
            "connector",
            "scope",
            "add",
            "--kind",
            "feishu",
            "--scope-id",
            "feishu.chat.oc_ops",
            "--upstream",
            "chat:oc_ops",
            "--label",
            "生产数据库值班群",
        ])
        .output()?;
    assert!(
        add_feishu.status.success(),
        "feishu scope add failed: {}",
        String::from_utf8_lossy(&add_feishu.stderr)
    );
    let feishu_notice_stderr = String::from_utf8_lossy(&add_feishu.stderr);
    assert!(
        feishu_notice_stderr.contains("提示：同步内容将使用当前全局模型处理"),
        "expected transparent global-model inheritance notice on stderr, got: {feishu_notice_stderr}"
    );
    assert_eq!(
        fs::metadata(&state_path)?.permissions().mode() & 0o777,
        0o600
    );

    let add_gh = success(
        kp_cmd(&lib)
            .args([
                "--json",
                "connector",
                "scope",
                "add",
                "--kind",
                "github",
                "--scope-id",
                "github.repo.ledger_service",
                "--upstream",
                "org-ops/ledger-service",
                "--label",
                "账本服务仓库",
            ])
            .output()?,
    )?;
    assert!(add_gh["notice"].is_null());

    let _add_sess = success(
        kp_cmd(&lib)
            .args([
                "--json",
                "connector",
                "scope",
                "add",
                "--kind",
                "agent-session",
                "--scope-id",
                "session.ops_local",
                "--upstream",
                sessions_dir.to_str().ok_or("utf8")?,
                "--label",
                "本地运维会话回填",
                "--model-policy",
                "raw_only",
            ])
            .output()?,
    )?;

    // 7a. Non-member `--as outsider` is rejected before polling (`PermissionDenied`, 0 polls, 0 cursors).
    let failed_outsider = kp_cmd(&lib)
        .args([
            "--as",
            "outsider",
            "--json",
            "connector",
            "sync",
            "--at",
            "1795340050000",
        ])
        .output()?;
    assert!(!failed_outsider.status.success());
    assert!(
        !lark_poll_log.exists(),
        "non-member `outsider` must be rejected before polling upstream connectors"
    );
    let state_after_outsider: Value = serde_json::from_slice(&fs::read(&state_path)?)?;
    assert!(
        state_after_outsider["cursors"]
            .as_object()
            .is_none_or(serde_json::Map::is_empty),
        "state.json cursors MUST stay unadvanced when outsider fails: {state_after_outsider}"
    );

    // 7b. Ordinary `Member` `--as carol` passing `--identity feishu:ou_owner=carol` is rejected with
    //     `PermissionDenied` before polling, keeping `state.json` cursors empty!
    let failed_carol_identity = kp_cmd(&lib)
        .args([
            "--as",
            "carol",
            "--json",
            "connector",
            "sync",
            "--identity",
            "feishu:ou_owner=carol",
            "--at",
            "1795340060000",
        ])
        .output()?;
    assert!(
        !failed_carol_identity.status.success(),
        "non-admin `carol` must not be allowed to pass `--identity` override"
    );
    assert!(
        String::from_utf8_lossy(&failed_carol_identity.stderr)
            .to_ascii_lowercase()
            .contains("permission denied"),
        "expected PermissionDenied for non-admin `--identity`, stderr={}",
        String::from_utf8_lossy(&failed_carol_identity.stderr)
    );
    assert!(
        !lark_poll_log.exists(),
        "unauthorized `--identity` override must fail before polling upstream connectors"
    );
    let state_after_carol_id: Value = serde_json::from_slice(&fs::read(&state_path)?)?;
    assert!(
        state_after_carol_id["cursors"]
            .as_object()
            .is_none_or(serde_json::Map::is_empty),
        "state.json cursors MUST stay unadvanced on `--identity` denial: {state_after_carol_id}"
    );

    // 7c. Real two-phase post-poll rollback: ordinary `Member` `--as carol connector sync` (without `--identity`)
    //     passes initial membership check, ACTUALLY runs `poll_enabled_scopes_staged` across all 3 connectors
    //     and stages cursors in memory, then fails in `apply_public_container_roster` (`PermissionDenied`),
    //     leaving `state.json` `cursors` completely empty!
    let failed_carol_post_poll = kp_cmd(&lib)
        .args([
            "--as",
            "carol",
            "--json",
            "connector",
            "sync",
            "--at",
            "1795340070000",
        ])
        .output()?;
    assert!(
        !failed_carol_post_poll.status.success(),
        "ordinary member `carol` must fail when applying container rosters"
    );
    assert!(
        String::from_utf8_lossy(&failed_carol_post_poll.stderr)
            .to_ascii_lowercase()
            .contains("permission denied"),
        "expected PermissionDenied from apply_public_container_roster, stderr={}",
        String::from_utf8_lossy(&failed_carol_post_poll.stderr)
    );
    let polled_before_rollback = fs::read_to_string(&lark_poll_log)?;
    assert!(
        polled_before_rollback.contains("im chat.members get")
            && polled_before_rollback.contains("im +chat-messages-list"),
        "expected `poll_enabled_scopes_staged` to have polled Feishu before downstream apply failure, got: {polled_before_rollback}"
    );
    let state_after_carol_poll: Value = serde_json::from_slice(&fs::read(&state_path)?)?;
    assert!(
        state_after_carol_poll["cursors"]
            .as_object()
            .is_none_or(serde_json::Map::is_empty),
        "two-phase cursor safety: staged cursors MUST NOT be committed to state.json when apply_public_ingest_jsonl fails: {state_after_carol_poll}"
    );
    gh_hits.lock().map_err(|_| "lock")?.clear();

    // 8. Run `kp --as owner connector sync --extract`: all 3 connectors ingest via Public Ingest Contract,
    //    `state.json` commits 3 cursors (`0600`), and the Feishu ops rule is extracted!
    let sync_w1 = success(
        kp_cmd(&lib)
            .args([
                "--as",
                "owner",
                "--json",
                "connector",
                "sync",
                "--extract",
                "--at",
                "1795340100000",
            ])
            .output()?,
    )?;
    assert_eq!(sync_w1["scopes_polled"], 3);
    assert_eq!(sync_w1["cursors_committed"], 3);
    assert_eq!(sync_w1["ingest"]["rosters_applied"], 3);
    assert!(sync_w1["ingest"]["records_upserted"].as_u64().unwrap_or(0) >= 4);
    assert_eq!(
        sync_w1["extraction"]["created_facts"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    assert_eq!(
        fs::metadata(&state_path)?.permissions().mode() & 0o777,
        0o600
    );
    let state_after_ok: Value = serde_json::from_slice(&fs::read(&state_path)?)?;
    assert_eq!(
        state_after_ok["cursors"].as_object().map(|m| m.len()),
        Some(3)
    );

    if gh_base_url.is_some() {
        let seen_gh = gh_hits.lock().map_err(|_| "lock")?.clone();
        assert!(seen_gh.contains(&"issues_and_prs".to_owned()));
        assert!(seen_gh.contains(&"pr_reviews".to_owned()));
    }

    // Confirm GitHub PR review and AgentSession backfill are searchable via `kp search`.
    let gh_search = run(&lib, &["--as", "owner", "search", "幂等序列号校验"])?;
    assert!(!gh_search["results"].as_array().ok_or("arr")?.is_empty());
    let sess_search = run(&lib, &["--as", "owner", "search", "幂等重放令牌"])?;
    assert!(!sess_search["results"].as_array().ok_or("arr")?.is_empty());

    let raw_guard_fid = sync_w1["extraction"]["created_facts"][0]
        .as_str()
        .ok_or("guard fact id")?;
    let bare_fact_ref = if raw_guard_fid.starts_with("fact_") {
        raw_guard_fid.to_owned()
    } else {
        format!("fact_{raw_guard_fid}")
    };
    let kp_ref = format!("kp-ref:{bare_fact_ref}");

    let dangerous_payload = json!({
        "tool_name": "Bash",
        "tool_input": {
            "command": "psql -c \"drop table orders_ledger\""
        }
    });

    let run_pre_tool = |actor: &str, client_kind: &str| -> Result<Value> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kp"))
            .env("KP_OFFLINE", "1")
            .arg("--path")
            .arg(&lib)
            .args([
                "--as",
                actor,
                "hook",
                "pre-tool-use",
                "--client",
                client_kind,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .ok_or("stdin")?
            .write_all(serde_json::to_vec(&dangerous_payload)?.as_slice())?;
        success(child.wait_with_output()?)
    };

    // 9. Cross-group guard triggers dual-client `PreToolUse` for group member `bob`, and zero leak for `carol`!
    let bob_claude = run_pre_tool("bob", "claude-code")?;
    assert_eq!(
        bob_claude["hookSpecificOutput"]["permissionDecision"],
        "ask"
    );
    let claude_reason = bob_claude["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .ok_or("claude reason")?;
    assert!(claude_reason.contains("在生产事故窗口内严禁执行 drop table orders_ledger"));
    assert!(claude_reason.contains(&kp_ref));

    let bob_codex = run_pre_tool("bob", "codex")?;
    assert_eq!(
        bob_codex["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );
    let codex_reason = bob_codex["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .ok_or("codex reason")?;
    assert!(codex_reason.contains("在生产事故窗口内严禁执行 drop table orders_ledger"));
    assert!(codex_reason.contains(&kp_ref));
    assert!(
        codex_reason.contains(&format!("kp allow {bare_fact_ref}"))
            || codex_reason.contains(&format!("kp allow {kp_ref}")),
        "unexpected codex deny reason: {codex_reason}"
    );

    // `bob` issues `kp --as bob allow kp-ref:fact_...` -> allowed ONCE on Codex, then immediately denied again!
    let _grant = run(&lib, &["--as", "bob", "allow", &kp_ref])?;
    assert_eq!(
        run_pre_tool("bob", "codex")?,
        json!({}),
        "first Codex PreToolUse after `kp allow` must pass once"
    );
    assert_eq!(
        run_pre_tool("bob", "codex")?["hookSpecificOutput"]["permissionDecision"],
        "deny",
        "single-use `kp allow` grant must be consumed and block the second Codex call"
    );

    // Non-group member `carol` sees `{}` (strict zero leakage).
    assert_eq!(run_pre_tool("carol", "claude-code")?, json!({}));
    assert_eq!(run_pre_tool("carol", "codex")?, json!({}));

    // 10. Path A (Group Departure Shrinkage): `ou_bob` leaves `oc_ops` -> `source_scopes` shrinks dynamically!
    fs::write(&wave_file, "2")?;
    let _sync_w2 = success(
        kp_cmd(&lib)
            .args([
                "--as",
                "owner",
                "--json",
                "connector",
                "sync",
                "--kind",
                "feishu",
                "--at",
                "1795340200000",
            ])
            .output()?,
    )?;
    assert_eq!(
        run_pre_tool("bob", "claude-code")?,
        json!({}),
        "departed group member `bob` must immediately stop triggering Claude Code PreToolUse"
    );
    assert_eq!(
        run_pre_tool("bob", "codex")?,
        json!({}),
        "departed group member `bob` must immediately stop triggering Codex PreToolUse"
    );
    // Remaining member `owner` is STILL protected!
    assert_eq!(
        run_pre_tool("owner", "claude-code")?["hookSpecificOutput"]["permissionDecision"],
        "ask"
    );
    assert_eq!(
        run_pre_tool("owner", "codex")?["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );

    // 11. Path B (Original Message Recall): `om_ops_2001` marked `deleted: true` -> `op: "withdraw"`!
    fs::write(&wave_file, "3")?;
    let sync_w3 = success(
        kp_cmd(&lib)
            .args([
                "--as",
                "owner",
                "--json",
                "connector",
                "sync",
                "--kind",
                "feishu",
                "--at",
                "1795340300000",
            ])
            .output()?,
    )?;
    assert_eq!(sync_w3["ingest"]["records_withdrawn"], 1);
    assert_eq!(
        run_pre_tool("owner", "claude-code")?,
        json!({}),
        "withdrawn Feishu guard message must immediately clear Claude Code PreToolUse for `owner`"
    );
    assert_eq!(
        run_pre_tool("owner", "codex")?,
        json!({}),
        "withdrawn Feishu guard message must immediately clear Codex PreToolUse for `owner`"
    );

    if let Some(srv) = gh_srv_guard {
        srv.abort();
    }
    Ok(())
}
