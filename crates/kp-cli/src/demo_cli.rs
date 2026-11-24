use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use clap::Args;
use kp_derive::{
    align_concepts_and_derive, apply_extracted_atoms, build_extraction_batch_request, derive,
    NlFunnelOptions, RawOnlyLlmClient,
};
use kp_model::{
    ActorId, CalendarTable, ConceptKind, ConceptPolarity, DerivationMode, EpochMs,
    ExtractionChunkSlice, FactId, FactKind, FactStatus, LibraryId, LibraryMode, MemberRole,
    ModelAmbiguousScope, ModelConceptRef, ModelExtractedAtom, ModelQuoteAnchor, ModelRoutePolicy,
    ModelScopeRef, ModelWriteOp, ReviewId, ReviewKind, ReviewStatus, SourceId, SourceOriginMeta,
    SourceScopeId, ValidityWindow,
};
use kp_store::{HashEmbedder, InitOptions, Store};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{now, output, Cli};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const ALLOW_ONCE_TTL_MS: i64 = 10 * 60 * 1_000;

#[derive(Args)]
pub struct DemoArgs {
    /// Directory where the demo library, workspace configs, and Markdown projection are created.
    #[arg(long)]
    pub dir: Option<PathBuf>,

    /// Keep the generated demo directory on disk when `--dir` is not specified.
    #[arg(long)]
    pub keep: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DemoReport {
    pub root_dir: PathBuf,
    pub library_dir: PathBuf,
    pub workspace_dir: PathBuf,
    pub projection_dir: PathBuf,
    pub kept: bool,
    pub claude_settings: PathBuf,
    pub codex_hooks: PathBuf,
    pub codex_config: PathBuf,
    pub active_facts: usize,
    pub superseded_premise_id: FactId,
    pub stale_assumed_conclusion_id: FactId,
    pub active_decision_id: FactId,
    pub guard_fact_id: FactId,
    pub expired_event_id: FactId,
    pub private_memo_id: FactId,
    pub open_review_ids: Vec<ReviewId>,
    pub prompt_context_verified: bool,
    pub pre_tool_claude_ask_verified: bool,
    pub pre_tool_codex_deny_and_allow_verified: bool,
    pub markdown_roundtrip_verified: bool,
    pub carol_isolation_verified: bool,
    pub elapsed_ms: u64,
}

struct EphemeralCleanupGuard {
    path: Option<PathBuf>,
}

impl Drop for EphemeralCleanupGuard {
    fn drop(&mut self) {
        if let Some(ref p) = self.path {
            let _ = fs::remove_dir_all(p);
        }
    }
}

fn replace_entry_claim_line(index_md: &str, fact_id: &FactId, new_claim: &str) -> Result<String> {
    let marker = format!("<!-- kp-entry: {fact_id} -->");
    let mut out = Vec::new();
    let mut replace_next = false;
    let mut replaced = false;

    for line in index_md.lines() {
        if line.trim() == marker {
            replace_next = true;
            out.push(line.to_owned());
            continue;
        }
        if replace_next && line.starts_with("- **[kp-ref:") {
            let Some((prefix, _)) = line.split_once("`) ") else {
                return Err("unexpected exported bullet format in demo projection".into());
            };
            out.push(format!("{prefix}`) {new_claim}"));
            replace_next = false;
            replaced = true;
            continue;
        }
        out.push(line.to_owned());
    }
    if !replaced {
        return Err("target decision entry not found in demo index.md".into());
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
        return Err("target entry marker to remove not found in demo index.md".into());
    }
    let mut joined = out.join("\n");
    joined.push('\n');
    Ok(joined)
}

struct DemoSourceExtract<'a> {
    source_id: &'a SourceId,
    speaker: &'a ActorId,
    ingested_by: &'a ActorId,
    scope_id: Option<SourceScopeId>,
    at: EpochMs,
}

fn extract_from_public_source(
    store: &mut Store,
    spec: DemoSourceExtract<'_>,
    atoms_fn: impl FnOnce(&str) -> Vec<ModelExtractedAtom>,
) -> Result<kp_derive::NlFunnelOutcome> {
    let DemoSourceExtract {
        source_id,
        speaker,
        ingested_by,
        scope_id,
        at,
    } = spec;
    let first_cid = format!("{source_id}-0");
    let ctx = store
        .extraction_source_context(source_id, std::slice::from_ref(&first_cid))?
        .ok_or("missing ingested demo source context")?;
    let (cid, heading, body, base_anchor) = ctx
        .chunks
        .first()
        .cloned()
        .ok_or("demo source has no chunks")?;
    let cal = CalendarTable::from_reference_in_tz(at, &kp_model::system_time_zone())?;
    let active_concepts = store.list_active_concepts()?;
    let req = build_extraction_batch_request(
        store,
        source_id.clone(),
        speaker.clone(),
        cal,
        active_concepts,
        vec![ExtractionChunkSlice {
            chunk_id: cid.clone(),
            source_id: source_id.clone(),
            author: speaker.clone(),
            base_anchor,
            role: Default::default(),
            reply_to_chunk_id: None,
            heading,
            body,
        }],
        10,
    )?;
    let atoms = atoms_fn(&cid);
    let opts = NlFunnelOptions {
        audience: ctx.readable_by,
        at,
        route_policy: ModelRoutePolicy::HostCli,
        source_trust_floor: if scope_id.is_some() {
            kp_model::TrustTier::ToolOrModelVerified
        } else {
            NlFunnelOptions::trust_floor_for_uri(&ctx.uri)
        },
        source_origin: scope_id.map(|sid| SourceOriginMeta {
            speaker: speaker.clone(),
            ingested_by: ingested_by.clone(),
            source_scope_id: sid,
        }),
        rejections: Vec::new(),
        extra_concepts: Vec::new(),
    };
    Ok(apply_extracted_atoms(store, &req, &atoms, &opts)?)
}

fn drain_demo_queue_without_llm(store: &mut Store, at: EpochMs) -> Result<()> {
    while let Some(job) = store.lease_next_extraction_job(at, 60_000)? {
        store.complete_extraction_job_fenced(
            &job.job_id,
            job.attempts,
            &[],
            Some(&job.author),
            "m1-nl-v1",
            at,
        )?;
    }
    Ok(())
}

pub async fn run(cli: &Cli, args: &DemoArgs) -> Result<()> {
    let started = Instant::now();
    let keep = args.dir.is_some() || args.keep;
    let raw_root = match &args.dir {
        Some(explicit) => explicit.clone(),
        None => std::env::temp_dir().join(format!("kp-demo-{}", Uuid::now_v7().simple())),
    };
    fs::create_dir_all(&raw_root)?;
    let root_dir = raw_root.canonicalize().unwrap_or(raw_root);
    let _cleanup = EphemeralCleanupGuard {
        path: (!keep).then(|| root_dir.clone()),
    };

    let library_dir = root_dir.join("demo-lib");
    let workspace_dir = root_dir.join("workspace");
    let projection_dir = root_dir.join("projection");
    fs::create_dir_all(&workspace_dir)?;

    let demo_at = now()?;
    let t_init = EpochMs(demo_at.0.saturating_sub(10_800_000));
    let t_w1 = EpochMs(demo_at.0.saturating_sub(7_200_000));
    let t_w2 = EpochMs(demo_at.0.saturating_sub(5_400_000));
    let t_w3 = EpochMs(demo_at.0.saturating_sub(3_600_000));
    let t_w4 = EpochMs(demo_at.0.saturating_sub(1_800_000));

    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;
    let carol: ActorId = "carol".parse()?;
    let ops_scope: SourceScopeId = "scope.ops_core".parse()?;

    // Step 1: Initialize `demo-lib` (`hash:v1` 256d deterministic vectors, zero network downloads)
    //         with `alice` (owner), `bob` (maintainer), and `carol` (bystander member).
    let mut store = Store::init_with_embedding(
        &library_dir,
        InitOptions {
            library_id: LibraryId::new("demo-lib")?,
            name: "Kipo Demo Library".into(),
            mode: LibraryMode::Community,
            owner: alice.clone(),
            created_at: t_init,
        },
        "hash:v1",
        256,
    )?;
    store.set_embedder(Arc::new(HashEmbedder));
    let _ = store.add_member(bob.clone(), MemberRole::Maintainer, t_init)?;
    let _ = store.add_member(carol.clone(), MemberRole::Member, t_init)?;

    // Step 2: Generate Claude Code & Codex project integration configs under `<root>/workspace`.
    let exe = std::env::current_exe()?;
    let claude_rep = kp_ingest::connect::connect_with_report(
        "claude-code",
        &workspace_dir,
        &library_dir,
        &alice,
        &exe,
    )?;
    let codex_rep = kp_ingest::connect::connect_with_report(
        "codex",
        &workspace_dir,
        &library_dir,
        &alice,
        &exe,
    )?;
    let codex_config = codex_rep
        .codex_config
        .ok_or("expected `.codex/config.toml` from codex connect")?;
    if !claude_rep.settings.is_file() || !codex_rep.settings.is_file() || !codex_config.is_file() {
        return Err("expected host integration config files on disk".into());
    }

    // Step 3: Populate 4-column rich dataset via Public Ingest Contract (`apply_public_ingest_jsonl`)
    //         + deterministic verbatim-anchored extraction & TMS derivation:
    //   ① Old premise superseded -> `AssumedPremise` conclusion transitions to `StalePremise`;
    //   ② Hard ops guard (`kubectl delete statefulset settlement-ledger`) + `alice` private memo;
    //   ③ Expired maintenance window (`valid_until < demo_at`);
    //   ④ Open `AmbiguousScope` governance review item.
    let ingest_lines = [
        json!({
            "container_id": "scope.ops_core",
            "label": "结算核心研发组",
            "members": ["alice", "bob"],
            "all_library_members": false,
            "synced_at": t_w1.0
        }),
        json!({
            "source": {"kind": "doc", "uri": "doc://arch/ledger-v1", "external_id": "demo-arch-v1", "container_id": "scope.ops_core"},
            "speaker": "alice",
            "audience": "container_members",
            "occurred_at": t_w1.0,
            "content": "存储架构初稿：结算主账本默认采用双写 Redis 流承载实时流水。",
            "op": "upsert"
        }),
        json!({
            "source": {"kind": "doc", "uri": "doc://arch/dr-plan-v1", "external_id": "demo-dr-v1", "container_id": "scope.ops_core"},
            "speaker": "bob",
            "audience": "container_members",
            "occurred_at": t_w2.0,
            "content": "容灾方案推导：既然结算主账本采用双写 Redis 流，异地容灾直接订阅 Redis AOF 增量流。",
            "op": "upsert"
        }),
        json!({
            "source": {"kind": "doc", "uri": "doc://arch/ledger-v2", "external_id": "demo-arch-v2", "container_id": "scope.ops_core"},
            "speaker": "alice",
            "audience": "container_members",
            "occurred_at": t_w3.0,
            "content": "存储架构定案：结算主账本统一收敛为单文件 SQLite WAL 引擎，淘汰双写 Redis 流；旧灰度双写比对脚本将在切换完成后下线。",
            "op": "upsert"
        }),
        json!({
            "source": {"kind": "chat", "uri": "chat://ops/rule-statefulset", "external_id": "demo-guard-1", "container_id": "scope.ops_core"},
            "speaker": "alice",
            "audience": "container_members",
            "occurred_at": t_w3.0 + 1_000,
            "content": "线上运维铁律：严禁在结算高峰期执行 kubectl delete statefulset settlement-ledger 删除核心结算副本集。",
            "op": "upsert"
        }),
        json!({
            "source": {"kind": "note", "uri": "note://alice/private-memo", "external_id": "demo-priv-1"},
            "speaker": "alice",
            "audience": {"explicit": ["alice"]},
            "occurred_at": t_w3.0 + 2_000,
            "content": "个人备忘：ALICE_PRIVATE_MEMO_774 下周三下午个人复诊请假半天，仅本人可见。",
            "op": "upsert"
        }),
        json!({
            "source": {"kind": "chat", "uri": "chat://ops/maint-window", "external_id": "demo-exp-1", "container_id": "scope.ops_core"},
            "speaker": "bob",
            "audience": "container_members",
            "occurred_at": t_w1.0 + 500,
            "content": "历史维护通知：凌晨备库已完成例行索引碎片整理维护窗口。",
            "op": "upsert"
        }),
        json!({
            "source": {"kind": "chat", "uri": "chat://ops/friday-freeze", "external_id": "demo-ambig-1", "container_id": "scope.ops_core"},
            "speaker": "alice",
            "audience": "container_members",
            "occurred_at": t_w4.0,
            "content": "Bob 属于结算核心研发组；每周五下午四点后暂停非紧急配置发布。",
            "op": "upsert"
        }),
    ]
    .iter()
    .map(Value::to_string)
    .collect::<Vec<_>>()
    .join("\n");

    let stream_rep =
        store.apply_public_ingest_jsonl(&ingest_lines, &alice, &ModelRoutePolicy::HostCli)?;
    if stream_rep.source_ids.len() != 7 {
        return Err("unexpected demo source count".into());
    }
    drain_demo_queue_without_llm(&mut store, demo_at)?;

    let sids = &stream_rep.source_ids;

    // 3a. Extract old premise (`sids[0]`), `AssumedPremise` conclusion (`sids[1]`), and `SupersedeOwn` replacement (`sids[2]`).
    let out_v1 = extract_from_public_source(
        &mut store,
        DemoSourceExtract {
            source_id: &sids[0],
            speaker: &alice,
            ingested_by: &alice,
            scope_id: Some(ops_scope.clone()),
            at: t_w1,
        },
        |cid| {
            vec![ModelExtractedAtom {
                kind: FactKind::Decision,
                claim_text: "结算主账本默认采用双写 Redis 流承载实时流水".into(),
                value: json!({"summary": "结算主账本默认采用双写 Redis 流承载实时流水"}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: None,
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: None,
                speaker: Some(alice.clone()),
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: cid.to_owned(),
                    verbatim_quote: "结算主账本默认采用双写 Redis 流承载实时流水".into(),
                    turn_ordinal: Some(0),
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
                evidence: None,
            }]
        },
    )?;
    let superseded_premise_id = out_v1
        .created_fact_ids
        .first()
        .cloned()
        .ok_or("missing demo v1 premise")?;

    let out_dr = extract_from_public_source(
        &mut store,
        DemoSourceExtract {
            source_id: &sids[1],
            speaker: &bob,
            ingested_by: &alice,
            scope_id: Some(ops_scope.clone()),
            at: t_w2,
        },
        |cid| {
            vec![ModelExtractedAtom {
                kind: FactKind::Decision,
                claim_text: "既然结算主账本采用双写 Redis 流，异地容灾直接订阅 Redis AOF 增量流"
                    .into(),
                value: json!({"summary": "异地容灾直接订阅 Redis AOF 增量流"}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: None,
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: None,
                speaker: Some(bob.clone()),
                assumed_premise_ids: vec![superseded_premise_id.clone()],
                quote: ModelQuoteAnchor {
                    chunk_id: cid.to_owned(),
                    verbatim_quote:
                        "既然结算主账本采用双写 Redis 流，异地容灾直接订阅 Redis AOF 增量流".into(),
                    turn_ordinal: Some(0),
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
                evidence: None,
            }]
        },
    )?;
    let stale_assumed_conclusion_id = out_dr
        .derived_assumed_fact_ids
        .first()
        .cloned()
        .ok_or("missing demo AssumedPremise conclusion")?;

    let out_v2 = extract_from_public_source(
        &mut store,
        DemoSourceExtract {
            source_id: &sids[2],
            speaker: &alice,
            ingested_by: &alice,
            scope_id: Some(ops_scope.clone()),
            at: t_w3,
        },
        |cid| {
            vec![
                ModelExtractedAtom {
                    kind: FactKind::Decision,
                    claim_text: "结算主账本统一收敛为单文件 SQLite WAL 引擎，淘汰双写 Redis 流"
                        .into(),
                    value: json!({"summary": "结算主账本统一收敛为单文件 SQLite WAL 引擎"}),
                    scope: None,
                    ambiguous_scope: None,
                    condition_concept: None,
                    action_concept: None,
                    group_concept: None,
                    member_mention: None,
                    valid_window: None,
                    valid_dates: None,
                    speaker: Some(alice.clone()),
                    assumed_premise_ids: Vec::new(),
                    quote: ModelQuoteAnchor {
                        chunk_id: cid.to_owned(),
                        verbatim_quote:
                            "结算主账本统一收敛为单文件 SQLite WAL 引擎，淘汰双写 Redis 流".into(),
                        turn_ordinal: Some(0),
                        approx_char_offset: None,
                    },
                    write_op: ModelWriteOp::SupersedeOwn {
                        target_fact_id: superseded_premise_id.clone(),
                    },
                    evidence: None,
                },
                ModelExtractedAtom {
                    kind: FactKind::Observation,
                    claim_text: "旧灰度双写比对脚本将在切换完成后下线".into(),
                    value: json!({"summary": "旧灰度双写比对脚本将在切换完成后下线"}),
                    scope: None,
                    ambiguous_scope: None,
                    condition_concept: None,
                    action_concept: None,
                    group_concept: None,
                    member_mention: None,
                    valid_window: None,
                    valid_dates: None,
                    speaker: Some(alice.clone()),
                    assumed_premise_ids: Vec::new(),
                    quote: ModelQuoteAnchor {
                        chunk_id: cid.to_owned(),
                        verbatim_quote: "旧灰度双写比对脚本将在切换完成后下线".into(),
                        turn_ordinal: Some(0),
                        approx_char_offset: None,
                    },
                    write_op: ModelWriteOp::Add,
                    evidence: None,
                },
            ]
        },
    )?;
    let initial_decision_id = out_v2
        .created_fact_ids
        .first()
        .cloned()
        .ok_or("missing demo v2 SQLite WAL decision")?;
    let obsolete_script_id = out_v2
        .created_fact_ids
        .get(1)
        .cloned()
        .ok_or("missing demo obsolete script observation")?;

    // 3b. Extract hard ops guard (`sids[3]`) + `alice` private memo (`sids[4]`) + expired window (`sids[5]`) + ambiguous scope (`sids[6]`).
    let out_guard = extract_from_public_source(
        &mut store,
        DemoSourceExtract {
            source_id: &sids[3],
            speaker: &alice,
            ingested_by: &alice,
            scope_id: Some(ops_scope.clone()),
            at: EpochMs(t_w3.0 + 1_000),
        },
        |cid| {
            vec![ModelExtractedAtom {
                kind: FactKind::Constraint,
                claim_text: "严禁在结算高峰期执行 kubectl delete statefulset settlement-ledger 删除核心结算副本集"
                    .into(),
                value: json!({
                    "hard_guard": true,
                    "command": "kubectl delete statefulset settlement-ledger"
                }),
                scope: Some(ModelScopeRef::NamedMentions {
                    mentions: vec!["alice".into(), "bob".into()],
                }),
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "结算高峰期".into(),
                    domain: "ops".into(),
                    polarity: ConceptPolarity::Neutral,
                }),
                action_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Action,
                    label: "kubectl delete statefulset settlement-ledger".into(),
                    domain: "ops".into(),
                    polarity: ConceptPolarity::Negative,
                }),
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: None,
                speaker: Some(alice.clone()),
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: cid.to_owned(),
                    verbatim_quote:
                        "严禁在结算高峰期执行 kubectl delete statefulset settlement-ledger 删除核心结算副本集"
                            .into(),
                    turn_ordinal: Some(0),
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
                evidence: None,
            }]
        },
    )?;
    let guard_fact_id = out_guard
        .created_fact_ids
        .first()
        .cloned()
        .ok_or("missing demo guard fact")?;

    let out_priv = extract_from_public_source(
        &mut store,
        DemoSourceExtract {
            source_id: &sids[4],
            speaker: &alice,
            ingested_by: &alice,
            scope_id: None,
            at: EpochMs(t_w3.0 + 2_000),
        },
        |cid| {
            vec![ModelExtractedAtom {
                kind: FactKind::Observation,
                claim_text: "ALICE_PRIVATE_MEMO_774 下周三下午个人复诊请假半天，仅本人可见".into(),
                value: json!({"summary": "ALICE_PRIVATE_MEMO_774 下周三下午个人复诊请假半天"}),
                scope: Some(ModelScopeRef::SpeakerSelf),
                ambiguous_scope: None,
                condition_concept: None,
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: None,
                valid_dates: None,
                speaker: Some(alice.clone()),
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: cid.to_owned(),
                    verbatim_quote: "ALICE_PRIVATE_MEMO_774 下周三下午个人复诊请假半天，仅本人可见"
                        .into(),
                    turn_ordinal: Some(0),
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
                evidence: None,
            }]
        },
    )?;
    let private_memo_id = out_priv
        .created_fact_ids
        .first()
        .cloned()
        .ok_or("missing demo private memo fact")?;

    let out_exp = extract_from_public_source(
        &mut store,
        DemoSourceExtract {
            source_id: &sids[5],
            speaker: &bob,
            ingested_by: &alice,
            scope_id: Some(ops_scope.clone()),
            at: EpochMs(t_w1.0 + 500),
        },
        |cid| {
            vec![ModelExtractedAtom {
                kind: FactKind::Event,
                claim_text: "凌晨备库已完成例行索引碎片整理维护窗口".into(),
                value: json!({"summary": "凌晨备库例行索引碎片整理维护窗口"}),
                scope: None,
                ambiguous_scope: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "索引碎片整理维护窗口".into(),
                    domain: "ops".into(),
                    polarity: ConceptPolarity::Neutral,
                }),
                action_concept: None,
                group_concept: None,
                member_mention: None,
                valid_window: Some(ValidityWindow {
                    valid_from: t_w1,
                    valid_until: t_w2,
                }),
                valid_dates: None,
                speaker: Some(bob.clone()),
                assumed_premise_ids: Vec::new(),
                quote: ModelQuoteAnchor {
                    chunk_id: cid.to_owned(),
                    verbatim_quote: "凌晨备库已完成例行索引碎片整理维护窗口".into(),
                    turn_ordinal: Some(0),
                    approx_char_offset: None,
                },
                write_op: ModelWriteOp::Add,
                evidence: None,
            }]
        },
    )?;
    let expired_event_id = out_exp
        .created_fact_ids
        .first()
        .cloned()
        .ok_or("missing demo expired event fact")?;

    let _out_ambig = extract_from_public_source(
        &mut store,
        DemoSourceExtract {
            source_id: &sids[6],
            speaker: &alice,
            ingested_by: &alice,
            scope_id: Some(ops_scope),
            at: t_w4,
        },
        |cid| {
            vec![
                ModelExtractedAtom {
                    kind: FactKind::Membership,
                    claim_text: "Bob 属于结算核心研发组".into(),
                    value: json!({}),
                    scope: None,
                    ambiguous_scope: None,
                    condition_concept: None,
                    action_concept: None,
                    group_concept: Some(ModelConceptRef::ProposeNew {
                        kind: ConceptKind::Group,
                        label: "结算核心研发组".into(),
                        domain: "team".into(),
                        polarity: ConceptPolarity::Neutral,
                    }),
                    member_mention: Some("bob".into()),
                    valid_window: None,
                    valid_dates: None,
                    speaker: Some(alice.clone()),
                    assumed_premise_ids: Vec::new(),
                    quote: ModelQuoteAnchor {
                        chunk_id: cid.to_owned(),
                        verbatim_quote: "Bob 属于结算核心研发组".into(),
                        turn_ordinal: Some(0),
                        approx_char_offset: None,
                    },
                    write_op: ModelWriteOp::Add,
                    evidence: None,
                },
                ModelExtractedAtom {
                    kind: FactKind::Constraint,
                    claim_text: "每周五下午四点后暂停非紧急配置发布".into(),
                    value: json!({"summary": "每周五下午四点后暂停非紧急配置发布"}),
                    scope: Some(ModelScopeRef::SpeakerSelf),
                    ambiguous_scope: Some(ModelAmbiguousScope {
                        broader_candidate: ModelScopeRef::RelationGroup {
                            group_concept: ModelConceptRef::ProposeNew {
                                kind: ConceptKind::Group,
                                label: "结算核心研发组".into(),
                                domain: "team".into(),
                                polarity: ConceptPolarity::Neutral,
                            },
                        },
                        question: "该周五下午暂停非紧急配置发布的规矩仅适用于你本人，还是约束整个结算核心研发组？".into(),
                    }),
                    condition_concept: Some(ModelConceptRef::ProposeNew {
                        kind: ConceptKind::Condition,
                        label: "周五下午四点后".into(),
                        domain: "ops".into(),
                        polarity: ConceptPolarity::Neutral,
                    }),
                    action_concept: Some(ModelConceptRef::ProposeNew {
                        kind: ConceptKind::Action,
                        label: "非紧急配置发布".into(),
                        domain: "ops".into(),
                        polarity: ConceptPolarity::Negative,
                    }),
                    group_concept: None,
                    member_mention: None,
                    valid_window: None,
                    valid_dates: None,
                    speaker: Some(alice.clone()),
                    assumed_premise_ids: Vec::new(),
                    quote: ModelQuoteAnchor {
                        chunk_id: cid.to_owned(),
                        verbatim_quote: "每周五下午四点后暂停非紧急配置发布".into(),
                        turn_ordinal: Some(0),
                        approx_char_offset: None,
                    },
                    write_op: ModelWriteOp::Add,
                    evidence: None,
                },
            ]
        },
    )?;

    let concepts = store.list_active_concepts()?;
    let existing_reviews = store.list_all_open_review_items()?;
    let align_out = align_concepts_and_derive(
        &mut store,
        &RawOnlyLlmClient,
        &concepts,
        &existing_reviews,
        demo_at,
    )
    .await?;
    let mut open_review_ids = Vec::new();
    for rev in &align_out.emitted_reviews {
        if let Some(stored) = store.upsert_open_review_item_if_valid(rev)? {
            open_review_ids.push(stored.id);
        }
    }
    if open_review_ids.is_empty() {
        return Err("expected open AmbiguousScope review item in demo".into());
    }

    // Step 4: Live verification of `<kp-context>`, dual-client `PreToolUse` + `allow_once`,
    //         `export -> edit -> apply -> export`, and `alice` vs `carol` non-interference!
    let ups_out = kp_ingest::hooks::run(
        &mut store,
        "user-prompt-submit",
        &json!({"prompt": "请检查结算主账本 SQLite WAL 存储引擎参数"}),
        &bob,
        demo_at,
    )
    .await?;
    let add_ctx = ups_out
        .pointer("/hookSpecificOutput/additionalContext")
        .and_then(Value::as_str)
        .unwrap_or("");
    let prompt_context_verified = add_ctx.contains("<kp-context>")
        && add_ctx.contains("SQLite WAL")
        && add_ctx.contains("kp-ref:fact_")
        && !add_ctx.contains("异地容灾直接订阅 Redis AOF");
    if !prompt_context_verified {
        return Err(format!("demo UserPromptSubmit recall check failed: {add_ctx}").into());
    }

    let claude_pre = kp_ingest::hooks::run(
        &mut store,
        "pre-tool-use",
        &json!({
            "tool_name": "Bash",
            "_kp_client": "claude-code",
            "tool_input": {"command": "kubectl delete statefulset settlement-ledger"}
        }),
        &bob,
        demo_at,
    )
    .await?;
    let pre_tool_claude_ask_verified =
        claude_pre.pointer("/hookSpecificOutput/permissionDecision") == Some(&json!("ask"));
    if !pre_tool_claude_ask_verified {
        return Err("demo Claude Code PreToolUse `ask` verification failed".into());
    }

    let codex_payload = json!({
        "tool_name": "Bash",
        "_kp_client": "codex",
        "tool_input": {"command": "kubectl delete statefulset settlement-ledger"}
    });
    let codex_deny_1 =
        kp_ingest::hooks::run(&mut store, "pre-tool-use", &codex_payload, &bob, demo_at).await?;
    let _ = store.grant_guard_allow_once_many(
        &bob,
        std::slice::from_ref(&guard_fact_id),
        ALLOW_ONCE_TTL_MS,
        demo_at,
    )?;
    let codex_allowed_once =
        kp_ingest::hooks::run(&mut store, "pre-tool-use", &codex_payload, &bob, demo_at).await?;
    let codex_deny_2 =
        kp_ingest::hooks::run(&mut store, "pre-tool-use", &codex_payload, &bob, demo_at).await?;
    let pre_tool_codex_deny_and_allow_verified = codex_deny_1
        .pointer("/hookSpecificOutput/permissionDecision")
        == Some(&json!("deny"))
        && codex_allowed_once == json!({})
        && codex_deny_2.pointer("/hookSpecificOutput/permissionDecision") == Some(&json!("deny"));
    if !pre_tool_codex_deny_and_allow_verified {
        return Err("demo Codex PreToolUse `deny` + single-use `allow` check failed".into());
    }

    // Lossless Markdown `export -> edit + delete -> apply -> export` roundtrip:
    let _ = store.export_markdown_directory(&projection_dir, &alice, demo_at)?;
    let index_path = projection_dir.join("index.md");
    let orig_md = fs::read_to_string(&index_path)?;
    let edited_md = replace_entry_claim_line(
        &orig_md,
        &initial_decision_id,
        "结算主账本统一收敛为单文件 SQLite WAL 引擎，淘汰双写 Redis 流（已开启自动检查点监控）",
    )?;
    let updated_md = remove_entry_block(&edited_md, &obsolete_script_id)?;
    fs::write(&index_path, updated_md)?;
    let wb_at = EpochMs(demo_at.0 + 1_000);
    let wb_rep = store.apply_markdown_directory_with_policy(
        &projection_dir,
        &alice,
        &ModelRoutePolicy::HostCli,
        wb_at,
    )?;
    let active_decision_id = wb_rep
        .updated_facts
        .first()
        .map(|u| u.new_fact_id.clone())
        .ok_or("expected 1 updated fact from demo Markdown writeback")?;
    if wb_rep.deleted_fact_ids.as_slice() != std::slice::from_ref(&obsolete_script_id) {
        return Err("expected 1 withdrawn fact from demo Markdown writeback".into());
    }
    let _ = derive(&mut store)?;
    let export_after_at = EpochMs(demo_at.0 + 2_000);
    let final_export = store.export_markdown_directory(&projection_dir, &alice, export_after_at)?;
    let final_md = fs::read_to_string(&index_path)?;
    let markdown_roundtrip_verified = final_md.contains("已开启自动检查点监控")
        && !final_md.contains("旧灰度双写比对脚本将在切换完成后下线")
        && !final_md.contains("结算主账本默认采用双写 Redis 流承载实时流水")
        && !final_md.contains(&stale_assumed_conclusion_id.to_string());
    if !markdown_roundtrip_verified {
        return Err("demo Markdown export/apply roundtrip verification failed".into());
    }

    // Verify TMS states + `carol` bystander non-interference:
    let graph = store.derivation_view()?;
    let prem_status = graph.facts.get(&superseded_premise_id).map(|f| f.status);
    let assumed_status = graph
        .facts
        .get(&stale_assumed_conclusion_id)
        .map(|f| f.status);
    let has_assumed_edge = graph.justifications.values().any(|j| {
        j.mode == DerivationMode::AssumedPremise && j.conclusion == stale_assumed_conclusion_id
    });
    if prem_status != Some(FactStatus::Superseded)
        || assumed_status != Some(FactStatus::StalePremise)
        || !has_assumed_edge
    {
        return Err("demo StalePremise TMS chain check failed".into());
    }
    let alice_open_reviews = store.list_review_items(&alice, Some(ReviewStatus::Open))?;
    if !alice_open_reviews
        .iter()
        .any(|r| r.kind == ReviewKind::AmbiguousScope)
    {
        return Err("demo AmbiguousScope open review missing".into());
    }

    let carol_hits = kp_retrieve::search_shallow(
        &store,
        &carol,
        export_after_at,
        "ALICE_PRIVATE_MEMO_774 SQLite",
        10,
    )?;
    let carol_log = store.visible_change_log(&carol, export_after_at, 50, None)?;
    let carol_log_json = serde_json::to_string(&carol_log)?;
    let alice_log = store.visible_change_log(&alice, export_after_at, 50, None)?;
    let carol_isolation_verified = carol_hits.results.is_empty()
        && !carol_log_json.contains("ALICE_PRIVATE_MEMO_774")
        && !carol_log_json.contains("SQLite WAL")
        && alice_log.iter().any(|e| e.op == "writeback_supersede");
    if !carol_isolation_verified {
        return Err("demo bystander `carol` isolation check failed".into());
    }

    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(0);
    let report = DemoReport {
        root_dir: root_dir.clone(),
        library_dir: library_dir.clone(),
        workspace_dir: workspace_dir.clone(),
        projection_dir: projection_dir.clone(),
        kept: keep,
        claude_settings: claude_rep.settings,
        codex_hooks: codex_rep.settings,
        codex_config,
        active_facts: final_export.visible_facts,
        superseded_premise_id,
        stale_assumed_conclusion_id,
        active_decision_id,
        guard_fact_id,
        expired_event_id,
        private_memo_id,
        open_review_ids,
        prompt_context_verified,
        pre_tool_claude_ask_verified,
        pre_tool_codex_deny_and_allow_verified,
        markdown_roundtrip_verified,
        carol_isolation_verified,
        elapsed_ms,
    };

    let next_steps = if keep {
        format!(
            "\nNext steps (explore the generated library):\n  1) kp --path {} status\n  2) kp --path {} --as alice review list\n  3) kp --path {} --as alice log\n  4) kp inspect --path {} --as alice",
            Path::new(&library_dir).display(),
            Path::new(&library_dir).display(),
            Path::new(&library_dir).display(),
            Path::new(&library_dir).display(),
        )
    } else {
        "\nTip: pass `kp demo --dir ./kipo-demo` to keep the demo library and inspect it via `kp inspect --path ./kipo-demo/demo-lib --as alice`.".to_owned()
    };

    let human = format!(
        "Kipo 5-Minute Interactive Tour completed in {}ms:\n  [1/4] Host integration configs written (.claude/settings.json, .codex/hooks.json, .codex/config.toml)\n  [2/4] Public Ingest + TMS: old Redis premise superseded -> dependent DR conclusion marked StalePremise; 1 expired window + 1 open AmbiguousScope review ready\n  [3/4] Hooks verified: <kp-context> recall OK, Claude `ask` OK, Codex `deny` + single-use `kp allow` OK\n  [4/4] Lossless Markdown `export -> edit -> apply` ({} visible facts) & `carol` zero-leak isolation OK{}",
        report.elapsed_ms, report.active_facts, next_steps
    );

    output(cli.json, &report, &human)?;
    Ok(())
}
