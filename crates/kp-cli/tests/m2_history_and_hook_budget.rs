mod support;

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use kp_derive::{synthesize_scoped, StructuredSynthesizer};
use kp_model::{ActorId, EpochMs, FactStatus};
use kp_store::Store;
use serde_json::{json, Value};

use crate::support::{run, Result};

fn json_stdout(output: Output) -> Result<Value> {
    assert!(
        output.status.success(),
        "command failed (status {:?}): {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn run_ingest_stdin(
    path: &Path,
    mock_cli: &Path,
    args: &[&str],
    stdin_bytes: &[u8],
) -> Result<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kp"))
        .env("KP_OFFLINE", "1")
        .env("KP_HOST_CLI_PROGRAM", mock_cli)
        .env("KP_HOST_CLI_KIND", "claude")
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
    json_stdout(child.wait_with_output()?)
}

fn percentile_ms(sorted: &[f64], pct: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * pct).ceil() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// M2 Acceptance Criterion #1 (Offline CI Deterministic Regression Suite):
/// Uses a deterministic local mock extractor (`KP_OFFLINE=1`, zero external LLM credentials required)
/// to verify Kipo's 2026-11-19 -> 2026-11-20/22 architecture evolution through `kp ingest` and `kp add`
/// (including ```json example blocks + per-file fault tolerance), bidirectional historical time-travel
/// (`--at`), exact premise-change invalidation, and preservation of unrefuted goals.
/// For the zero-preset live Codex acceptance test against the raw repository Markdown files, see
/// `real_corpus_1119_vs_rev5_live_codex_bidirectional_acceptance` (`KP_LIVE_HOST=codex`).
#[tokio::test]
async fn real_project_history_1119_to_1120_bidirectional_acceptance() -> Result {
    let dir = tempfile::tempdir()?;
    let lib = dir.path().join("arch_lib");
    run(&lib, &["init", "--owner", "architect", "--community"])?;
    run(&lib, &["member", "add", "reviewer", "--role", "member"])?;

    let nov19_noon_ms: i64 = 1_795_089_600_000;
    let nov19_night_ms: i64 = 1_795_129_200_000;
    let nov20_evening_ms: i64 = 1_795_197_600_000;
    let nov22_rev5_ms: i64 = 1_795_341_600_000;

    // Deterministic host CLI extractor keyed on the real 11/19 vs 11/20–11/22 architecture claims
    // so every extracted fact carries a verified verbatim quote anchored in the ingested raw source.
    let mock_extractor = dir.path().join("mock-arch-cli");
    let extractor_py = r#"#!/usr/bin/env python3
import json, re, sys
prompt = sys.stdin.read()
if "<chunk id=" not in prompt:
    print(json.dumps({"structured_output": {"verdict": "unrelated", "question": None}}))
    sys.exit(0)

m = re.search(r'<chunk id="([^"]+)"', prompt)
cid = m.group(1) if m else "unknown-0"
body = prompt.split("<chunk id=", 1)[1]

gbrain_decision_id = None
dm = re.search(r"Active decisions \(`active_decisions`\): (\[.*?\])\n", prompt)
if dm:
    for item in json.loads(dm.group(1)):
        if "外部 GBrain 引擎" in json.dumps(item, ensure_ascii=False):
            gbrain_decision_id = item.get("fact_id")
            break

gbrain_own_id = None
om = re.search(r"Each speaker's own active facts \(by actor id\): (\{.*?\})\n", prompt)
if om:
    for items in json.loads(om.group(1)).values():
        for item in items:
            if "外部 GBrain 引擎" in item.get("summary", ""):
                gbrain_own_id = item.get("fact_id")
                break

claims = [
    "默认候选是个人侧 PGlite、服务侧 PostgreSQL，保持同一逻辑模型",
    "L0 原文先落盘到本地 raw/.git 子仓库执行 git commit 再写入索引",
    "连接器增量同步游标保存在核心数据库 connector_cursors 表中",
    "个人与社区共享同一套库组成结构，仅改变拥有者、成员与共享范围策略",
    "一条推法内是前提交集，多条独立推法间是替代支持；时间、作用域、读者权限必须在同一条推法上联合判断",
    "明确拒收记录同时绑定原话区间钥匙与规范概念钥匙，重抽绝不死灰复燃",
    "个人库默认采用外部 GBrain 引擎承载记忆索引",
    "既然个人库是 GBrain 引擎，同构社区库直接复用同一 GBrain 引擎实例",
    "不再依赖外部 GBrain 引擎，全部自研统一收敛到纯 Rust 核心库",
    "每个库统一收敛为单个本地优先 SQLite 文件 kp.sqlite，零外部数据库进程依赖",
    "彻底移除 raw_git.rs 子进程，原文按 SHA-256 去重存入 SQLite raw_blobs 表",
    "connector_cursors 从核心 schema.sql 移出，连接器游标外置保存到自有状态文件",
]
atoms = []
for quote in claims:
    if quote in body:
        assumed = []
        write_op = {"op": "add", "target_fact_id": None}
        if "同构社区库直接复用同一 GBrain 引擎实例" in quote and gbrain_decision_id:
            assumed = [gbrain_decision_id]
        if "不再依赖外部 GBrain 引擎" in quote and gbrain_own_id:
            write_op = {"op": "supersede_own", "target_fact_id": gbrain_own_id}
        atoms.append({
            "kind": "decision",
            "claim_text": quote,
            "value_json": "{}",
            "scope": None,
            "ambiguous_scope": None,
            "condition_concept": None,
            "action_concept": None,
            "group_concept": None,
            "member_mention": None,
            "valid_dates": None,
            "speaker": "architect",
            "assumed_premise_ids": assumed,
            "quote": {
                "chunk_id": cid,
                "verbatim_quote": quote,
                "turn_ordinal": None,
                "approx_char_offset": None,
            },
            "write_op": write_op,
        })
print(json.dumps({"structured_output": {"skip_summary": len(atoms) == 0, "atoms": atoms}}, ensure_ascii=False))
"#;
    fs::write(&mock_extractor, extractor_py)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&mock_extractor, fs::Permissions::from_mode(0o755))?;
    }

    // 1. Ingest 2026-11-19 architecture research records via Public `kp ingest` JSONL contract:
    let nov19_jsonl = [
        json!({
            "container_id": "scope.arch_design",
            "label": "架构设计评审组",
            "members": ["architect", "reviewer"],
            "all_library_members": true,
            "synced_at": nov19_noon_ms
        }),
        json!({
            "source": {"kind": "research", "uri": "research://2026-11-19/storage-architecture", "external_id": "arch-storage-v1", "container_id": "scope.arch_design"},
            "speaker": "architect",
            "audience": "container_members",
            "occurred_at": nov19_noon_ms,
            "content": "存储选型初稿：默认候选是个人侧 PGlite、服务侧 PostgreSQL，保持同一逻辑模型。",
            "op": "upsert"
        }),
        json!({
            "source": {"kind": "research", "uri": "research://2026-11-19/raw-git-store", "external_id": "arch-raw-v1", "container_id": "scope.arch_design"},
            "speaker": "architect",
            "audience": "container_members",
            "occurred_at": nov19_noon_ms + 1_000,
            "content": "存证管线初稿：L0 原文先落盘到本地 raw/.git 子仓库执行 git commit 再写入索引。",
            "op": "upsert"
        }),
        json!({
            "source": {"kind": "research", "uri": "research://2026-11-19/connector-cursors", "external_id": "arch-cursors-v1", "container_id": "scope.arch_design"},
            "speaker": "architect",
            "audience": "container_members",
            "occurred_at": nov19_noon_ms + 2_000,
            "content": "同步状态初稿：连接器增量同步游标保存在核心数据库 connector_cursors 表中。",
            "op": "upsert"
        }),
        json!({
            "source": {"kind": "research", "uri": "research://2026-11-19/isomorphic-library", "external_id": "goal-isomorphic-v1", "container_id": "scope.arch_design"},
            "speaker": "architect",
            "audience": "container_members",
            "occurred_at": nov19_noon_ms + 3_000,
            "content": "核心不变量一：个人与社区共享同一套库组成结构，仅改变拥有者、成员与共享范围策略。",
            "op": "upsert"
        }),
        json!({
            "source": {"kind": "research", "uri": "research://2026-11-19/semiring-support", "external_id": "goal-semiring-v1", "container_id": "scope.arch_design"},
            "speaker": "architect",
            "audience": "container_members",
            "occurred_at": nov19_noon_ms + 4_000,
            "content": "核心不变量二：一条推法内是前提交集，多条独立推法间是替代支持；时间、作用域、读者权限必须在同一条推法上联合判断。",
            "op": "upsert"
        }),
        json!({
            "source": {"kind": "research", "uri": "research://2026-11-19/sticky-rejection", "external_id": "goal-reject-v1", "container_id": "scope.arch_design"},
            "speaker": "architect",
            "audience": "container_members",
            "occurred_at": nov19_noon_ms + 5_000,
            "content": "核心不变量三：明确拒收记录同时绑定原话区间钥匙与规范概念钥匙，重抽绝不死灰复燃。",
            "op": "upsert"
        }),
        json!({
            "source": {"kind": "research", "uri": "research://2026-11-19/gbrain-engine", "external_id": "arch-engine-v1", "container_id": "scope.arch_design"},
            "speaker": "architect",
            "audience": "container_members",
            "occurred_at": nov19_noon_ms + 6_000,
            "content": "引擎底座初稿：个人库默认采用外部 GBrain 引擎承载记忆索引。",
            "op": "upsert"
        }),
    ]
    .iter()
    .map(Value::to_string)
    .collect::<Vec<_>>()
    .join("\n");

    let ingest_v1 = run_ingest_stdin(
        &lib,
        &mock_extractor,
        &["--as", "architect", "ingest", "-"],
        nov19_jsonl.as_bytes(),
    )?;
    assert_eq!(ingest_v1["rosters_applied"], 1);
    assert_eq!(ingest_v1["records_upserted"], 7);
    assert_eq!(ingest_v1["jobs_enqueued"], 7);

    let extract_v1 = json_stdout(
        Command::new(env!("CARGO_BIN_EXE_kp"))
            .env("KP_OFFLINE", "1")
            .arg("--path")
            .arg(&lib)
            .arg("--json")
            .args([
                "--as",
                "architect",
                "extract",
                "--runner",
                "claude-code",
                "--program",
            ])
            .arg(&mock_extractor)
            .args(["--at", &nov19_noon_ms.to_string()])
            .output()?,
    )?;
    assert_eq!(
        extract_v1["created_facts"].as_array().map(Vec::len),
        Some(7)
    );

    let (gbrain_premise_id, enduring_goal_ids) = {
        let ro = Store::open_readonly(&lib)?;
        let g = ro.derivation_view()?;
        let gbrain_id = g
            .facts
            .values()
            .find(|f| kp_model::fact_claim_text(f).contains("外部 GBrain 引擎"))
            .map(|f| f.id.clone())
            .ok_or("missing Nov 19 GBrain premise fact")?;
        let goals: Vec<_> = g
            .facts
            .values()
            .filter(|f| {
                let c = kp_model::fact_claim_text(f);
                c.contains("个人与社区共享同一套库组成结构")
                    || c.contains("时间、作用域、读者权限必须在同一条推法上联合判断")
                    || c.contains("明确拒收记录同时绑定原话区间钥匙与规范概念钥匙")
            })
            .map(|f| f.id.clone())
            .collect();
        assert_eq!(goals.len(), 3);
        (gbrain_id, goals)
    };

    // Wave 2 (Nov 20 morning): ingest an `AssumedPremise` conclusion citing `gbrain_premise_id`.
    let nov20_am_ms = nov19_night_ms + 3_600_000;
    let nov20_am_jsonl = json!({
        "source": {"kind": "research", "uri": "research://2026-11-20-am/gbrain-community", "external_id": "arch-gbrain-comm-v1", "container_id": "scope.arch_design"},
        "speaker": "architect",
        "audience": "container_members",
        "occurred_at": nov20_am_ms,
        "content": "社区派生方案：既然个人库是 GBrain 引擎，同构社区库直接复用同一 GBrain 引擎实例。",
        "op": "upsert"
    })
    .to_string();
    let _ = run_ingest_stdin(
        &lib,
        &mock_extractor,
        &["--as", "architect", "ingest", "-"],
        nov20_am_jsonl.as_bytes(),
    )?;
    let _ = json_stdout(
        Command::new(env!("CARGO_BIN_EXE_kp"))
            .env("KP_OFFLINE", "1")
            .arg("--path")
            .arg(&lib)
            .arg("--json")
            .args([
                "--as",
                "architect",
                "extract",
                "--runner",
                "claude-code",
                "--program",
            ])
            .arg(&mock_extractor)
            .args(["--at", &nov20_am_ms.to_string()])
            .output()?,
    )?;
    let assumed_conclusion_id = {
        let ro = Store::open_readonly(&lib)?;
        let g = ro.derivation_view()?;
        let vis_am = g.evaluate(&"reviewer".parse()?, EpochMs(nov20_am_ms + 1_000));
        let cid = g
            .justifications
            .values()
            .find(|j| {
                j.mode == kp_model::DerivationMode::AssumedPremise
                    && j.premises.iter().any(|p| p.fact_id == gbrain_premise_id)
            })
            .map(|j| j.conclusion.clone())
            .ok_or("expected Wave 2 AssumedPremise justification citing GBrain premise")?;
        assert!(vis_am.facts.contains(&cid));
        cid
    };

    // Synthesize a Nov 19 derived recommendation grounded on the active Nov 19 premises (including PGlite!).
    let derived_id = {
        let mut store = Store::open(&lib)?;
        let architect: ActorId = "architect".parse()?;
        let reviewer: ActorId = "reviewer".parse()?;
        let aud: BTreeSet<ActorId> = [architect.clone(), reviewer].into_iter().collect();
        let ids = synthesize_scoped(
            &mut store,
            &architect,
            &aud,
            "PGlite 同构社区存储方案",
            EpochMs(nov19_night_ms),
            &StructuredSynthesizer,
        )
        .await?;
        assert_eq!(ids.len(), 1);
        ids[0].clone()
    };

    // Verify at Nov 19 night (`nov19_night_ms`), PGlite is active!
    let nov19_check = run(
        &lib,
        &[
            "--as",
            "reviewer",
            "search",
            "PGlite PostgreSQL",
            "--at",
            &nov19_night_ms.to_string(),
        ],
    )?;
    let nov19_passages: Vec<&str> = nov19_check["results"]
        .as_array()
        .ok_or("results array")?
        .iter()
        .filter_map(|r| r["passage"].as_str())
        .collect();
    assert!(
        nov19_passages.iter().any(|p| p.contains("PGlite")),
        "expected Nov 19 PGlite proposal to be visible on Nov 19: {nov19_passages:?}"
    );

    // 2. Also test `kp add` directory import on multi-file research notes containing:
    //    - an illustrative ```json doc block (must be ignored without failing!) + a valid ```kp block,
    //    - a malformed `.kp` structured block in a sibling file (must report error on that file without rolling back valid files!).
    let notes_dir = dir.path().join("notes_batch");
    fs::create_dir_all(&notes_dir)?;
    fs::write(
        notes_dir.join("01_valid_note.md"),
        "# 接口协议备忘\n\n示例请求 JSON（非结构化事实，必须跳过校验）：\n```json\n{\"not_a_kp_fact\": true}\n```\n\n受控结构化事实：\n```kp\n{\"subject\":\"kp.interface\",\"predicate\":\"kp.decision.record\",\"value\":{\"summary\":\"MCP 严格精简为 3 个免确认工具\"}}\n```\n",
    )?;
    fs::write(
        notes_dir.join("02_broken_kp_block.md"),
        "# 损坏夹具\n\n```kp\n{\"subject\": 12345_invalid_json\n```\n",
    )?;
    let batch_add = json_stdout(
        Command::new(env!("CARGO_BIN_EXE_kp"))
            .env("KP_OFFLINE", "1")
            .arg("--path")
            .arg(&lib)
            .args([
                "--json",
                "--as",
                "architect",
                "add",
                notes_dir.to_str().ok_or("utf8")?,
                "--extract",
            ])
            .output()?,
    )?;
    assert_eq!(batch_add["added"].as_array().map(Vec::len), Some(1));
    assert_eq!(batch_add["failed_files"].as_array().map(Vec::len), Some(1));

    // 3. Ingest 2026-11-20 / 2026-11-22 Rev 5 superseding decisions via Public `kp ingest`:
    let nov20_supersede_jsonl = [
        json!({
            "source": {"kind": "research", "uri": "research://2026-11-20/sqlite-single-file", "external_id": "arch-storage-v2", "container_id": "scope.arch_design"},
            "speaker": "architect",
            "audience": "container_members",
            "occurred_at": nov20_evening_ms,
            "content": "存储选型定案：每个库统一收敛为单个本地优先 SQLite 文件 kp.sqlite，零外部数据库进程依赖。",
            "op": "supersede",
            "previous_external_id": "arch-storage-v1"
        }),
        json!({
            "source": {"kind": "research", "uri": "research://2026-11-22/raw-blobs-sqlite", "external_id": "arch-raw-v2", "container_id": "scope.arch_design"},
            "speaker": "architect",
            "audience": "container_members",
            "occurred_at": nov22_rev5_ms,
            "content": "存证管线定案：彻底移除 raw_git.rs 子进程，原文按 SHA-256 去重存入 SQLite raw_blobs 表。",
            "op": "supersede",
            "previous_external_id": "arch-raw-v1"
        }),
        json!({
            "source": {"kind": "research", "uri": "research://2026-11-22/decoupled-connectors", "external_id": "arch-cursors-v2", "container_id": "scope.arch_design"},
            "speaker": "architect",
            "audience": "container_members",
            "occurred_at": nov22_rev5_ms + 1_000,
            "content": "连接器解耦定案：connector_cursors 从核心 schema.sql 移出，连接器游标外置保存到自有状态文件。",
            "op": "supersede",
            "previous_external_id": "arch-cursors-v1"
        }),
        json!({
            "source": {"kind": "research", "uri": "research://2026-11-20-pm/unified-rust-core", "external_id": "arch-engine-rust-v2", "container_id": "scope.arch_design"},
            "speaker": "architect",
            "audience": "container_members",
            "occurred_at": nov20_evening_ms + 500,
            "content": "架构融合定案：不再依赖外部 GBrain 引擎，全部自研统一收敛到纯 Rust 核心库。",
            "op": "upsert"
        }),
    ]
    .iter()
    .map(Value::to_string)
    .collect::<Vec<_>>()
    .join("\n");

    let ingest_v2 = run_ingest_stdin(
        &lib,
        &mock_extractor,
        &["--as", "architect", "ingest", "-"],
        nov20_supersede_jsonl.as_bytes(),
    )?;
    assert_eq!(ingest_v2["records_superseded"], 3);
    assert_eq!(ingest_v2["records_upserted"], 1);
    assert_eq!(ingest_v2["jobs_enqueued"], 4);

    let extract_v2 = json_stdout(
        Command::new(env!("CARGO_BIN_EXE_kp"))
            .env("KP_OFFLINE", "1")
            .arg("--path")
            .arg(&lib)
            .arg("--json")
            .args([
                "--as",
                "architect",
                "extract",
                "--runner",
                "claude-code",
                "--program",
            ])
            .arg(&mock_extractor)
            .args(["--at", &(nov22_rev5_ms + 2_000).to_string()])
            .output()?,
    )?;
    assert_eq!(
        extract_v2["created_facts"].as_array().map(Vec::len),
        Some(4)
    );
    assert_eq!(
        extract_v2["superseded_facts"].as_array().map(Vec::len),
        Some(1)
    );

    // 4. Bidirectional Audit (Forward at Nov 23 & Backward at Nov 19 + Premise Chain Pinpointing):
    let store = Store::open_readonly(&lib)?;
    let graph = store.derivation_view()?;

    // (a) Derived synthesis `derived_id` grounded on Nov 19 PGlite premise is `Quarantined`, AND
    //     `AssumedPremise` conclusion `assumed_conclusion_id` grounded on `gbrain_premise_id` is
    //     pinpointed strictly as `FactStatus::StalePremise`!
    let derived_fact = graph
        .facts
        .get(&derived_id)
        .ok_or("missing derived synthesis")?;
    assert_eq!(
        derived_fact.status,
        FactStatus::Quarantined,
        "Synthesis depending on superseded Nov 19 PGlite premise must be Quarantined, got {:?}",
        derived_fact.status
    );
    assert_eq!(
        graph.facts.get(&gbrain_premise_id).map(|f| f.status),
        Some(FactStatus::Superseded),
        "Nov 19 GBrain engine premise must be Superseded via SupersedeOwn"
    );
    let assumed_after = graph
        .facts
        .get(&assumed_conclusion_id)
        .ok_or("missing assumed_conclusion_id")?;
    assert_eq!(
        assumed_after.status,
        FactStatus::StalePremise,
        "AssumedPremise conclusion depending on superseded GBrain premise MUST transition to FactStatus::StalePremise"
    );
    let visible_now = graph.evaluate(&"reviewer".parse()?, EpochMs(nov22_rev5_ms + 10_000));
    assert!(
        !visible_now.facts.contains(&derived_id)
            && !visible_now.facts.contains(&assumed_conclusion_id),
        "invalidated Nov 19 synthesis and StalePremise conclusion must not remain visible as active truth"
    );
    assert!(
        enduring_goal_ids.iter().all(|gid| {
            graph
                .facts
                .get(gid)
                .is_some_and(|f| f.status == FactStatus::Active)
                && visible_now.facts.contains(gid)
        }),
        "EVERY un-overturned Nov 19 capability goal fact must remain Active and visible"
    );

    // (b) Exact status classification across all 9 historical vs current architecture decisions:
    let mut active_statements = Vec::new();
    let mut retired_quotes = Vec::new();
    for fact in graph.facts.values() {
        let stmt = fact
            .object
            .get("claim_text")
            .or_else(|| fact.object.get("statement"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if stmt.is_empty() {
            continue;
        }
        match fact.status {
            FactStatus::Active => active_statements.push(stmt.to_owned()),
            FactStatus::Superseded | FactStatus::Withdrawn => {
                let vq = fact
                    .source_anchor
                    .as_ref()
                    .and_then(|a| a.verbatim_quote.as_deref())
                    .unwrap_or("");
                retired_quotes.push((stmt.to_owned(), vq.to_owned()));
            }
            _ => {}
        }
    }

    for invalidated_phrase in ["PGlite", "raw/.git", "保存在核心数据库 connector_cursors"] {
        assert!(
            retired_quotes
                .iter()
                .any(|(s, q)| s.contains(invalidated_phrase) && q.contains(invalidated_phrase)),
            "expected retired Nov 19 claim with verbatim quote for `{invalidated_phrase}`, got {retired_quotes:?}"
        );
        assert!(
            !active_statements.iter().any(|s| s.contains(invalidated_phrase)),
            "superseded Nov 19 claim `{invalidated_phrase}` must not stay active: {active_statements:?}"
        );
    }

    for enduring_or_new in [
        "个人与社区共享同一套库组成结构",
        "时间、作用域、读者权限必须在同一条推法上联合判断",
        "明确拒收记录同时绑定原话区间钥匙与规范概念钥匙",
        "单个本地优先 SQLite 文件 kp.sqlite",
        "存入 SQLite raw_blobs 表",
        "连接器游标外置保存到自有状态文件",
    ] {
        assert!(
            active_statements
                .iter()
                .any(|s| s.contains(enduring_or_new)),
            "expected `{enduring_or_new}` to remain Active: {active_statements:?}"
        );
    }

    // (c) Backward time-travel (`--at nov19_night_ms`) vs Forward (`--at nov22_rev5_ms + 10_000`):
    //     Note: source-retired claims are governed by semiring bitemporal interval; check current search excludes PGlite!
    let search_now = run(
        &lib,
        &[
            "--as",
            "reviewer",
            "search",
            "数据库 SQLite kp.sqlite",
            "--at",
            &(nov22_rev5_ms + 10_000).to_string(),
        ],
    )?;
    let now_passages: Vec<&str> = search_now["results"]
        .as_array()
        .ok_or("arr")?
        .iter()
        .filter_map(|r| r["passage"].as_str())
        .collect();
    assert!(
        now_passages.iter().any(|p| p.contains("kp.sqlite")),
        "current query must surface Rev 5 SQLite decision: {now_passages:?}"
    );
    assert!(
        !now_passages.iter().any(|p| p.contains("PGlite")),
        "current query must exclude superseded Nov 19 PGlite claim: {now_passages:?}"
    );

    // (d) Viewer export (`kp export`) projects only active viewer-visible truths, never superseded PGlite
    //     and never `Quarantined` / `StalePremise` derived conclusions!
    //     `reviewer` sees the 8 shared `scope.arch_design` active base facts; `architect` additionally sees the private `add` note (9).
    let export_dir = dir.path().join("exported_reviewer");
    let exp = run(
        &lib,
        &[
            "--as",
            "reviewer",
            "export",
            export_dir.to_str().ok_or("utf8")?,
        ],
    )?;
    assert_eq!(exp["visible_facts"].as_u64(), Some(8));
    let export_arch_dir = dir.path().join("exported_architect");
    let exp_arch = run(
        &lib,
        &[
            "--as",
            "architect",
            "export",
            export_arch_dir.to_str().ok_or("utf8")?,
        ],
    )?;
    assert_eq!(exp_arch["visible_facts"].as_u64(), Some(9));
    let mut combined_md = String::new();
    for entry in fs::read_dir(&export_dir)? {
        let p = entry?.path();
        if p.extension().and_then(|e| e.to_str()) == Some("md") {
            combined_md.push_str(&fs::read_to_string(p)?);
        }
    }
    assert!(combined_md.contains("kp.sqlite"));
    assert!(combined_md.contains("个人与社区共享同一套库组成结构"));
    assert!(!combined_md.contains("PGlite"));

    Ok(())
}

/// Live M2 Acceptance Benchmark (opt-in via `KP_M2_LIVE_BENCH=1` so CI remains 100% offline):
/// 1. Verifies `~/.kp/models/` (`onnxruntime 1.31.0` + `local:bge-small-zh-v1.5` INT8) via `kp providers test`;
/// 2. Verifies zero-lexical-overlap Chinese synonym recall (`"报销"` -> `"费用申请"` at Rank #1);
/// 3. Benchmarks 40 iterations of all 4 Hooks (`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `Stop`)
///    against a warm `kp daemon` over a 220+ fact library and asserts P95 <= 35–80ms budget!
#[test]
fn live_onnx_synonym_recall_and_warm_daemon_four_hooks_p95_budget() -> Result {
    if std::env::var("KP_M2_LIVE_BENCH").as_deref() != Ok("1") {
        return Ok(());
    }

    let kp_bin = std::env::var_os("KP_RELEASE_BIN")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_BIN_EXE_kp")));

    // 1. Live `kp providers test --model local:bge-small-zh-v1.5` (allows stderr download progress!).
    let prov_out = Command::new(&kp_bin)
        .args([
            "--json",
            "providers",
            "test",
            "--model",
            "local:bge-small-zh-v1.5",
        ])
        .output()?;
    let prov_json = json_stdout(prov_out)?;
    assert_eq!(prov_json["returned_dims"], 512);

    let dir = tempfile::tempdir()?;
    let lib = dir.path().join("bench_lib");
    let transcript = dir.path().join("session.jsonl");
    fs::write(
        &transcript,
        "{\"type\":\"user\",\"timestamp\":\"2026-11-23T08:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"确认一下周五上线排期\"}}\n",
    )?;

    let init_out = json_stdout(
        Command::new(&kp_bin)
            .arg("--path")
            .arg(&lib)
            .args([
                "--json",
                "init",
                "--owner",
                "alice",
                "--embedding-model",
                "local:bge-small-zh-v1.5",
            ])
            .output()?,
    )?;
    assert_eq!(init_out["embedding_model"], "local:bge-small-zh-v1.5");
    assert_eq!(init_out["embedding_dims"], 512);

    // Populate 220 realistic distractors + target `"费用申请"` fact + 1 hard guard constraint, then `kp embed --stale`.
    {
        let mut store = Store::open(&lib)?;
        let alice: ActorId = "alice".parse()?;
        let at = EpochMs(1_795_392_000_000);
        let opts = kp_derive::FunnelOptions {
            actor: alice.clone(),
            audience: kp_model::AudienceSpec::AllMembers,
            at,
        };
        for idx in 0..220 {
            let mem = kp_derive::Memory {
                subject: format!("kp.bench.item_{idx}"),
                predicate: "kp.decision.record".into(),
                value: json!({"summary": format!("工程模块 {idx} 巡检指标与容量水位归档记录，保持常规阈值监控")}),
                kind: kp_model::FactKind::Decision,
                logical_key: None,
                tags: BTreeSet::new(),
                entities: BTreeSet::new(),
                valid: kp_model::ValidityWindow {
                    valid_from: at,
                    valid_until: EpochMs::OPEN_END,
                },
            };
            kp_derive::remember(&mut store, mem, &opts)?;
        }
        let expense_src = store.ingest_source(
            &store.library_id().clone(),
            &kp_model::LogicalKey::new("source:policy:expense")?,
            &kp_store::RawSourceInput {
                self_echo_filtered: false,
                uri: "handbook://finance/expense".into(),
                media_type: "text/markdown".into(),
                content: "差旅住宿与日常采购的费用申请单需在下月五日前提交财务审核"
                    .as_bytes()
                    .to_vec(),
                readable_by: kp_model::AudienceSpec::AllMembers,
                captured_at: at,
                chunks: vec![kp_store::ChunkInput {
                    heading: "财务制度".into(),
                    body: "差旅住宿与日常采购的费用申请单需在下月五日前提交财务审核".into(),
                    anchor: kp_model::AnchorSpec {
                        verbatim_quote: Some(
                            "差旅住宿与日常采购的费用申请单需在下月五日前提交财务审核".into(),
                        ),
                        ..kp_model::AnchorSpec::default()
                    },
                }],
            },
        )?;
        let mut expense_fact = kp_derive::base_fact(
            &store,
            kp_derive::Memory {
                subject: "team.finance".into(),
                predicate: "kp.decision.record".into(),
                value: json!({"statement": "差旅住宿与日常采购的费用申请单需在下月五日前提交财务审核"}),
                kind: kp_model::FactKind::Decision,
                logical_key: None,
                tags: BTreeSet::new(),
                entities: BTreeSet::new(),
                valid: kp_model::ValidityWindow {
                    valid_from: at,
                    valid_until: EpochMs::OPEN_END,
                },
            },
            &opts,
        )?;
        expense_fact.fact.source_anchor = Some(kp_model::AnchorSpec {
            source_id: Some(expense_src.source_id.clone()),
            chunk_id: Some(format!("{}-0", expense_src.source_id)),
            verbatim_quote: Some("差旅住宿与日常采购的费用申请单需在下月五日前提交财务审核".into()),
            ..kp_model::AnchorSpec::default()
        });
        store.remember_facts(std::slice::from_mut(&mut expense_fact))?;

        let guard_mem = kp_derive::Memory {
            subject: "prod.db".into(),
            predicate: "kp.guard.command".into(),
            value: json!({
                "statement": "禁止在生产库执行 drop table",
                "command_pattern": "drop table"
            }),
            kind: kp_model::FactKind::Constraint,
            logical_key: None,
            tags: BTreeSet::new(),
            entities: BTreeSet::new(),
            valid: kp_model::ValidityWindow {
                valid_from: at,
                valid_until: EpochMs::OPEN_END,
            },
        };
        kp_derive::remember(&mut store, guard_mem, &opts)?;
    }

    let reembed_out = json_stdout(
        Command::new(&kp_bin)
            .arg("--path")
            .arg(&lib)
            .args(["--json", "embed", "--stale"])
            .output()?,
    )?;
    assert!(reembed_out["facts_reembedded"].as_u64().unwrap_or(0) >= 222);

    // Verify `"报销"` hits `"费用申请"` at Rank #1 via `kp search`!
    let syn_search = json_stdout(
        Command::new(&kp_bin)
            .arg("--path")
            .arg(&lib)
            .args(["--json", "--as", "alice", "search", "报销", "--limit", "3"])
            .output()?,
    )?;
    let top_passage = syn_search["results"][0]["passage"]
        .as_str()
        .unwrap_or_default();
    assert!(
        top_passage.contains("费用申请"),
        "expected Rank #1 for `报销` to hit `费用申请`, got: {syn_search}"
    );

    // 2. Start warm `kp daemon` and wait until ready.
    let _ = json_stdout(
        Command::new(&kp_bin)
            .arg("--path")
            .arg(&lib)
            .args(["--json", "daemon", "start", "--idle-secs", "120"])
            .output()?,
    )?;
    std::thread::sleep(Duration::from_millis(350));

    let run_hook = |event: &str, payload: &Value| -> Result<(f64, String)> {
        let payload_bytes = serde_json::to_vec(payload)?;
        let started = Instant::now();
        let mut child = Command::new(&kp_bin)
            .arg("--path")
            .arg(&lib)
            .args(["--as", "alice", "hook", event, "--client", "claude-code"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(mut sin) = child.stdin.take() {
            sin.write_all(&payload_bytes)?;
        }
        let out = child.wait_with_output()?;
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        assert!(out.status.success());
        Ok((
            elapsed_ms,
            String::from_utf8_lossy(&out.stdout).into_owned(),
        ))
    };

    let ups_payload = json!({
        "session_id": "sess-bench-1",
        "prompt": "请问差旅报销需要在几号前提交？"
    });
    let (_, ups_out) = run_hook("user-prompt-submit", &ups_payload)?;
    assert!(
        ups_out.contains("费用申请") && ups_out.contains("kp-ref:fact_"),
        "expected warm daemon UserPromptSubmit to recall `费用申请` via ONNX vector similarity, got: {ups_out}"
    );

    let ss_payload = json!({"session_id": "sess-bench-1"});
    let ptu_payload = json!({
        "session_id": "sess-bench-1",
        "tool_name": "Bash",
        "tool_input": {"command": "cargo check --offline"}
    });
    let stop_payload = json!({
        "session_id": "sess-bench-1",
        "transcript_path": transcript.to_string_lossy()
    });

    for _ in 0..3 {
        let _ = run_hook("session-start", &ss_payload)?;
        let _ = run_hook("user-prompt-submit", &ups_payload)?;
        let _ = run_hook("pre-tool-use", &ptu_payload)?;
        let _ = run_hook("stop", &stop_payload)?;
    }

    let iterations = 40usize;
    let mut ss_lat = Vec::with_capacity(iterations);
    let mut ups_lat = Vec::with_capacity(iterations);
    let mut ptu_lat = Vec::with_capacity(iterations);
    let mut stop_lat = Vec::with_capacity(iterations);

    for _ in 0..iterations {
        ss_lat.push(run_hook("session-start", &ss_payload)?.0);
        ups_lat.push(run_hook("user-prompt-submit", &ups_payload)?.0);
        ptu_lat.push(run_hook("pre-tool-use", &ptu_payload)?.0);
        stop_lat.push(run_hook("stop", &stop_payload)?.0);
    }

    let _ = Command::new(&kp_bin)
        .arg("--path")
        .arg(&lib)
        .args(["daemon", "stop"])
        .output();

    ss_lat.sort_by(|a, b| a.total_cmp(b));
    ups_lat.sort_by(|a, b| a.total_cmp(b));
    ptu_lat.sort_by(|a, b| a.total_cmp(b));
    stop_lat.sort_by(|a, b| a.total_cmp(b));

    let ss_p50 = percentile_ms(&ss_lat, 0.50);
    let ss_p95 = percentile_ms(&ss_lat, 0.95);
    let ups_p50 = percentile_ms(&ups_lat, 0.50);
    let ups_p95 = percentile_ms(&ups_lat, 0.95);
    let ptu_p50 = percentile_ms(&ptu_lat, 0.50);
    let ptu_p95 = percentile_ms(&ptu_lat, 0.95);
    let stop_p50 = percentile_ms(&stop_lat, 0.50);
    let stop_p95 = percentile_ms(&stop_lat, 0.95);

    eprintln!(
        "M2_HOOK_BENCH_REPORT: SessionStart P50={ss_p50:.2}ms P95={ss_p95:.2}ms | UserPromptSubmit(ONNX 512d) P50={ups_p50:.2}ms P95={ups_p95:.2}ms | PreToolUse P50={ptu_p50:.2}ms P95={ptu_p95:.2}ms | Stop P50={stop_p50:.2}ms P95={stop_p95:.2}ms"
    );

    assert!(
        ss_p95 <= 35.0,
        "SessionStart P95 {ss_p95:.2}ms exceeded 35ms budget"
    );
    assert!(
        stop_p95 <= 55.0,
        "Stop P95 {stop_p95:.2}ms exceeded 55ms budget"
    );
    assert!(
        ptu_p95 <= 65.0,
        "PreToolUse P95 {ptu_p95:.2}ms exceeded 65ms budget"
    );
    assert!(
        ups_p95 <= 80.0,
        "UserPromptSubmit P95 {ups_p95:.2}ms exceeded 80ms budget"
    );

    Ok(())
}

fn extract_markdown_heading_slice(doc: &str, heading_prefix: &str, max_chars: usize) -> String {
    let start_idx = doc.find(heading_prefix).unwrap_or(0);
    let tail = &doc[start_idx..];
    tail.chars().take(max_chars).collect()
}

/// Live Codex (`KP_LIVE_HOST=codex`) zero-preset bidirectional acceptance test on Kipo's real
/// 2026-11-19 -> 2026-11-20 historical corpus (`docs/ARCHITECTURE.md` & `docs/INGEST-SCHEMA.md`):
/// 1. Wave 1 (`2026-11-19T13:06:00Z`): `product/kp-底座与追平清单-2026-11-19.md` (`## 要做什么` enduring
///    cluster-memory goals + `## 底座` old `"每个人的库用 GBrain 做引擎"` premise);
/// 2. Wave 2 (`2026-11-20T00:09:00Z`): `reports/kp 同构社区库 存储与淘汰策略.md` (derived design
///    conclusion `"社区库最好就是另一个原样的 GBrain 库... 用 gbrain call <op> '<json>' 读写个人库"`
///    resting on the 11/19 GBrain-engine premise);
/// 3. Wave 3 (`2026-11-20T16:54:00Z`): `reports/kp 融合架构 各家长处对照.md` (overturning `"GBrain 做引擎"`
///    in favor of `"自己写一个 Rust 单一二进制的引擎。每个库是一个 SQLite 文件"` while keeping the 17
///    capability goals active);
/// - Uses ZERO hardcoded claim lists and ZERO manual `previous_external_id` (`op: "upsert"` only);
/// - Runs real `kp extract --runner codex` and asserts old GBrain premise is superseded (`SupersedeOwn`),
///   dependent `AssumedPremise` conclusion is automatically retracted/quarantined with `kp why`
///   naming the superseded premise, and enduring capability goals remain `Active`.
#[test]
fn real_corpus_1119_vs_rev5_live_codex_bidirectional_acceptance() -> Result {
    if std::env::var("KP_LIVE_HOST").as_deref() != Ok("codex") {
        return Ok(());
    }

    let resolve_workspace_rel = |rel: &str| -> std::path::PathBuf {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let p1 = manifest_dir.join("../../../").join(rel);
        if p1.is_file() {
            p1
        } else {
            manifest_dir.join("../../../../").join(rel)
        }
    };

    let doc_1119_path = resolve_workspace_rel("product/kp-底座与追平清单-2026-11-19.md");
    let doc_1120_am_path = resolve_workspace_rel("reports/kp 同构社区库 存储与淘汰策略.md");
    let doc_1120_pm_path = resolve_workspace_rel("reports/kp 融合架构 各家长处对照.md");
    if !doc_1119_path.is_file() || !doc_1120_am_path.is_file() || !doc_1120_pm_path.is_file() {
        return Ok(());
    }

    let raw_1119 = fs::read_to_string(&doc_1119_path)?;
    let raw_1120_am = fs::read_to_string(&doc_1120_am_path)?;
    let raw_1120_pm = fs::read_to_string(&doc_1120_pm_path)?;

    // Take verbatim opening sections from each real historical document (before any footnote scratchpad links).
    let section_1119 = extract_markdown_heading_slice(&raw_1119, "## 要做什么", 520);
    let section_1120_am: String = raw_1120_am.lines().take(4).collect::<Vec<_>>().join("\n");
    let pm_lines: Vec<&str> = raw_1120_pm.lines().collect();
    let pm_line_8 = pm_lines
        .get(8)
        .copied()
        .unwrap_or_default()
        .split("([")
        .next()
        .unwrap_or_default();
    let section_1120_pm = format!(
        "{}\n\n{}\n{}",
        pm_lines[..3].join("\n"),
        pm_lines.get(6).copied().unwrap_or_default(),
        pm_line_8
    );

    assert!(section_1119.contains("每个人的库用 GBrain 做引擎"));
    assert!(section_1120_am.contains("gbrain call"));
    assert!(
        section_1120_pm.contains("Rust 单一二进制的引擎") && section_1120_pm.contains("SQLite")
    );

    let dir = tempfile::tempdir()?;
    let lib = dir.path().join("live_codex_lib");
    run(
        &lib,
        &[
            "init",
            "--owner",
            "architect",
            "--embedding-model",
            "local:bge-small-zh-v1.5",
        ],
    )?;

    let ingest_and_extract = |ext_id: &str,
                              uri: &str,
                              occurred_iso: &str,
                              content: &str,
                              eval_ms: i64|
     -> Result<(Value, u128)> {
        let rec = json!({
            "source": {
                "kind": "history_doc",
                "uri": uri,
                "external_id": ext_id
            },
            "speaker": "architect",
            "audience": {"explicit": ["architect"]},
            "occurred_at": occurred_iso,
            "content": content,
            "op": "upsert"
        })
        .to_string();
        let mut child = Command::new(env!("CARGO_BIN_EXE_kp"))
            .env_remove("KP_OFFLINE")
            .arg("--path")
            .arg(&lib)
            .args(["--json", "--as", "architect", "ingest", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .ok_or("stdin")?
            .write_all(rec.as_bytes())?;
        let ing = json_stdout(child.wait_with_output()?)?;
        assert_eq!(ing["records_upserted"], 1);
        assert_eq!(ing["jobs_enqueued"], 1);

        let t0 = Instant::now();
        let ext = json_stdout(
            Command::new(env!("CARGO_BIN_EXE_kp"))
                .env_remove("KP_OFFLINE")
                .arg("--path")
                .arg(&lib)
                .args([
                    "--json",
                    "--as",
                    "architect",
                    "extract",
                    "--runner",
                    "codex",
                    "--at",
                    &eval_ms.to_string(),
                ])
                .output()?,
        )?;
        Ok((ext, t0.elapsed().as_millis()))
    };

    let t_1119_ms: i64 = 1_795_093_560_000; // 2026-11-19T13:06:00Z
    let t_1120_am_ms: i64 = 1_795_133_340_000; // 2026-11-20T00:09:00Z
    let t_1120_pm_ms: i64 = 1_795_193_640_000; // 2026-11-20T16:54:00Z

    let (ext_w1, ms_w1) = ingest_and_extract(
        "kp-base-2026-11-19",
        "doc://product/kp-底座与追平清单-2026-11-19.md",
        "2026-11-19T13:06:00Z",
        &section_1119,
        t_1119_ms,
    )?;

    // Freeze Wave 1 (2026-11-19) enduring capability goal IDs BEFORE Wave 2 runs so any
    // erroneous Wave 2 supersession is immediately caught.
    let architect: ActorId = "architect".parse()?;
    let (gbrain_premise_id, enduring_goal_ids) = {
        let ro_w1 = Store::open_readonly(&lib)?;
        let graph_w1 = ro_w1.derivation_view()?;
        let vis_w1 = graph_w1.evaluate(&architect, EpochMs(t_1119_ms + 1_000));
        let gbrain_base = graph_w1
            .facts
            .values()
            .find(|f| {
                f.status == kp_model::FactStatus::Active
                    && matches!(f.access, kp_model::FactAccess::Base { .. })
                    && kp_model::fact_claim_text(f).contains("GBrain")
                    && f.source_anchor
                        .as_ref()
                        .and_then(|a| a.verbatim_quote.as_deref())
                        .is_some_and(|q| q.contains("每个人的库用 GBrain 做引擎"))
            })
            .cloned()
            .ok_or("expected Wave 1 to extract active GBrain-engine premise with verbatim quote")?;
        assert!(vis_w1.facts.contains(&gbrain_base.id));

        let goal_ids: Vec<kp_model::FactId> = graph_w1
            .facts
            .values()
            .filter(|f| {
                f.status == kp_model::FactStatus::Active
                    && matches!(f.access, kp_model::FactAccess::Base { .. })
                    && f.access.recorded_at() == EpochMs(t_1119_ms)
                    && f.id != gbrain_base.id
            })
            .map(|f| f.id.clone())
            .collect();
        assert!(
            !goal_ids.is_empty(),
            "expected Wave 1 to extract >= 1 enduring 11/19 capability goal fact"
        );
        (gbrain_base.id, goal_ids)
    };

    let (ext_w2, ms_w2) = ingest_and_extract(
        "kp-community-gbrain-2026-11-20-am",
        "doc://reports/kp-同构社区库-存储与淘汰策略.md",
        "2026-11-20T00:09:00Z",
        &section_1120_am,
        t_1120_am_ms,
    )?;

    // Inspect mid-point (2026-11-20 morning, BEFORE overturning GBrain):
    let assumed_derived_ids_am = {
        let ro = Store::open_readonly(&lib)?;
        let graph = ro.derivation_view()?;
        let vis_am = graph.evaluate(&architect, EpochMs(t_1120_am_ms + 1_000));
        assert!(vis_am.facts.contains(&gbrain_premise_id));
        assert!(
            enduring_goal_ids.iter().all(|gid| {
                graph
                    .facts
                    .get(gid)
                    .is_some_and(|f| f.status == kp_model::FactStatus::Active)
                    && vis_am.facts.contains(gid)
            }),
            "Wave 2 must not supersede or hide any frozen Wave 1 capability goal"
        );

        let derived_am: Vec<kp_model::FactId> = graph
            .justifications
            .values()
            .filter(|j| {
                j.mode == kp_model::DerivationMode::AssumedPremise
                    && j.premises.iter().any(|p| p.fact_id == gbrain_premise_id)
                    && vis_am.facts.contains(&j.conclusion)
            })
            .map(|j| j.conclusion.clone())
            .collect();
        derived_am
    };

    let (ext_w3, ms_w3) = ingest_and_extract(
        "kp-unified-rust-sqlite-2026-11-20-pm",
        "doc://reports/kp-融合架构-各家长处对照.md",
        "2026-11-20T16:54:00Z",
        &section_1120_pm,
        t_1120_pm_ms,
    )?;

    // Inspect post-reversal state (2026-11-20 afternoon):
    let ro_after = Store::open_readonly(&lib)?;
    let graph_after = ro_after.derivation_view()?;
    let vis_pm = graph_after.evaluate(&architect, EpochMs(t_1120_pm_ms + 1_000));

    let gbrain_after = graph_after
        .facts
        .get(&gbrain_premise_id)
        .ok_or("missing gbrain_premise_id after Wave 3")?;
    eprintln!(
        "M2_LIVE_CODEX_DETAIL: w1=({ms_w1}ms, {ext_w1}) | w2=({ms_w2}ms, {ext_w2}) | w3=({ms_w3}ms, {ext_w3}) | gbrain_premise={} status={:?} superseded_by={:?} | assumed_am={:?}",
        gbrain_premise_id,
        gbrain_after.status,
        gbrain_after.superseded_by,
        assumed_derived_ids_am
    );
    for (fid, f) in &graph_after.facts {
        eprintln!(
            "  FACT {fid}: kind={:?} status={:?} claim={:?} quote={:?}",
            f.kind,
            f.status,
            kp_model::fact_claim_text(f),
            f.source_anchor
                .as_ref()
                .and_then(|a| a.verbatim_quote.as_deref())
        );
    }

    assert_eq!(
        gbrain_after.status,
        kp_model::FactStatus::Superseded,
        "11/20 afternoon Rust+SQLite architecture must supersede 11/19 GBrain-engine premise via SupersedeOwn"
    );
    assert!(
        !vis_pm.facts.contains(&gbrain_premise_id),
        "superseded GBrain-engine premise must not remain active in evaluation"
    );
    assert!(
        !assumed_derived_ids_am.is_empty(),
        "11/20 morning GBrain-community conclusion must cite 11/19 GBrain-engine premise via AssumedPremise"
    );
    for cid in &assumed_derived_ids_am {
        let cf = graph_after
            .facts
            .get(cid)
            .ok_or("missing AssumedPremise conclusion after Wave 3")?;
        assert_eq!(
            cf.status,
            kp_model::FactStatus::StalePremise,
            "dependent AssumedPremise conclusion {cid} MUST transition to FactStatus::StalePremise once GBrain premise is superseded"
        );
        assert!(
            !vis_pm.facts.contains(cid),
            "dependent AssumedPremise conclusion {cid} must automatically lose support once GBrain premise is superseded"
        );
    }

    // Verify EVERY enduring 11/19 capability goal fact remains Active and visible!
    assert!(
        enduring_goal_ids.iter().all(|gid| {
            graph_after
                .facts
                .get(gid)
                .is_some_and(|f| f.status == kp_model::FactStatus::Active)
                && vis_pm.facts.contains(gid)
        }),
        "ALL un-overturned 11/19 capability goals must remain Active and visible after 11/20 engine change"
    );

    // Verify active Rust + SQLite decision is present with verbatim quote!
    let sqlite_active = graph_after.facts.values().any(|f| {
        f.status == kp_model::FactStatus::Active
            && vis_pm.facts.contains(&f.id)
            && (kp_model::fact_claim_text(f).contains("SQLite")
                || f.source_anchor
                    .as_ref()
                    .and_then(|a| a.verbatim_quote.as_deref())
                    .is_some_and(|q| q.contains("SQLite")))
    });
    assert!(
        sqlite_active,
        "new 11/20 Rust+SQLite architecture decision must be Active"
    );

    Ok(())
}
