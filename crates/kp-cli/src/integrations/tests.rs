use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use kp_model::{
    AudienceSpec, BitemporalStamp, EpochMs, Fact, FactAccess, FactKind, FactRank, FactStatus,
    LibraryId, LibraryMode, TrustTier, ValidityWindow,
};
use kp_net::{DaemonClient, DaemonConfig, DaemonServer};
use kp_store::{InitOptions, Store};
use serde_json::json;

use super::{execute_hook_with_input, CliDaemonHandler, Client, Event, IntegrationCommand};
use crate::{Cli, Command};

fn test_cli(path: std::path::PathBuf, actor: &str) -> Result<Cli, Box<dyn std::error::Error>> {
    Ok(Cli {
        path,
        json: true,
        actor: Some(actor.parse()?),
        command: Command::Integrations(IntegrationCommand::Hook {
            event: Event::SessionStart,
            client: None,
        }),
    })
}

#[tokio::test]
async fn daemon_uds_fast_path_handles_prompt_context_pre_tool_allow_session_cursor_and_fallback(
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let lib_path = dir.path().join("lib");
    let at = EpochMs(1_763_683_200_000);
    let mut store = Store::init(
        &lib_path,
        InitOptions {
            library_id: LibraryId::new("daemon-cli-test")?,
            name: "Daemon CLI Test".into(),
            mode: LibraryMode::Personal,
            owner: "alice".parse()?,
            created_at: EpochMs(0),
        },
    )?;
    store.add_member("bob".parse()?, kp_model::MemberRole::Member, EpochMs(0))?;

    let guard_id: kp_model::FactId = "fact_guard_uds_merge_01".parse()?;
    store.remember(&[kp_store::FactInput {
        fact: Fact {
            id: guard_id.clone(),
            origin_library: store.library_id().clone(),
            logical_key: "guard-uds-merge".parse()?,
            kind: FactKind::Constraint,
            subject: "src/lib.rs".into(),
            predicate: "kp.policy.guard".into(),
            object: json!({
                "hard_guard": true,
                "reason": "数据库迁移核验期间禁止合并分支或直接编辑 lib.rs"
            }),
            tags: BTreeSet::new(),
            author: "alice".parse()?,
            ingested_by: None,
            trust_tier: TrustTier::HumanVerbatim,
            access: FactAccess::Base {
                audience: AudienceSpec::Explicit(BTreeSet::from([
                    "alice".parse()?,
                    "bob".parse()?,
                ])),
                stamp: BitemporalStamp {
                    valid: ValidityWindow {
                        valid_from: EpochMs(0),
                        valid_until: EpochMs::OPEN_END,
                    },
                    recorded_at: at,
                    expired_at: None,
                },
            },
            source_scope_id: None,
            governed_scope: None,
            broader_scope: None,
            status: FactStatus::Active,
            superseded_by: None,
            rank: FactRank::Normal,
            source_anchor: None,
            unverified_gaps: Vec::new(),
            miss_count: 0,
        },
        entities: BTreeSet::new(),
    }])?;
    drop(store);

    let daemon_store = Store::open(&lib_path)?;
    let canonical_lib = lib_path.canonicalize()?;
    let mut cfg = DaemonConfig::for_library(&canonical_lib)?;
    cfg.idle_extract_window = Duration::from_millis(50);
    let handle =
        DaemonServer::spawn(daemon_store, cfg, Arc::new(CliDaemonHandler::default())).await?;

    let cli = test_cli(lib_path.clone(), "alice")?;

    // 1. Daemon status works over UDS.
    let status = DaemonClient::for_library(&lib_path)?.status().await?;
    assert_eq!(status.facts, 1);

    // 2. UserPromptSubmit hits live DaemonSyncClient over UDS (`<= 250` tokens with provenance).
    let ups = execute_hook_with_input(
        &cli,
        Event::UserPromptSubmit,
        Some(Client::ClaudeCode),
        r#"{"prompt":"数据库迁移核验期间 lib.rs"}"#.as_bytes(),
        at,
    )
    .await?;
    let ctx = ups["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .ok_or("additionalContext")?;
    assert!(ctx.starts_with("<kp-context>\n"));
    assert!(ctx.contains("kp-ref:fact_guard_uds_merge_01"));

    // 3. PreToolUse precision over UDS: `git merge-base` and `stdlib.rs` stay silent (`{}`).
    let merge_base = execute_hook_with_input(
        &cli,
        Event::PreToolUse,
        Some(Client::Codex),
        br#"{"tool_name":"Bash","tool_input":{"command":"git merge-base HEAD main"}}"#,
        at,
    )
    .await?;
    assert_eq!(merge_base, json!({}));

    let stdlib_edit = execute_hook_with_input(
        &cli,
        Event::PreToolUse,
        Some(Client::Codex),
        br#"{"tool_name":"Edit","tool_input":{"file_path":"crates/foo/stdlib.rs"}}"#,
        at,
    )
    .await?;
    assert_eq!(stdlib_edit, json!({}));

    // 4. Client-split PreToolUse over UDS: Claude gets `"ask"`, Codex gets `"deny"` + `kp allow`.
    let claude_ask = execute_hook_with_input(
        &cli,
        Event::PreToolUse,
        Some(Client::ClaudeCode),
        br#"{"tool_name":"Bash","tool_input":{"command":"git merge feat/demo"}}"#,
        at,
    )
    .await?;
    assert_eq!(
        claude_ask["hookSpecificOutput"]["permissionDecision"],
        "ask"
    );

    let codex_deny = execute_hook_with_input(
        &cli,
        Event::PreToolUse,
        Some(Client::Codex),
        br#"{"tool_name":"Bash","tool_input":{"command":"git merge feat/demo"}}"#,
        at,
    )
    .await?;
    assert_eq!(
        codex_deny["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );
    assert!(codex_deny["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .ok_or("reason")?
        .contains("kp allow fact_guard_uds_merge_01"));

    // 5. Record a 1-time `allow_guard_once` grant via Store API: Claude `ask` does NOT consume it, Codex consumes it ONCE!
    {
        let mut grant_store = Store::open(&lib_path)?;
        grant_store.grant_guard_allow_once(&"alice".parse()?, &guard_id, 600_000, at)?;
    }
    let claude_still_asks = execute_hook_with_input(
        &cli,
        Event::PreToolUse,
        Some(Client::ClaudeCode),
        br#"{"tool_name":"Bash","tool_input":{"command":"git merge feat/demo"}}"#,
        at,
    )
    .await?;
    assert_eq!(
        claude_still_asks["hookSpecificOutput"]["permissionDecision"],
        "ask"
    );

    let allowed_once = execute_hook_with_input(
        &cli,
        Event::PreToolUse,
        Some(Client::Codex),
        br#"{"tool_name":"Bash","tool_input":{"command":"git merge feat/demo"}}"#,
        at,
    )
    .await?;
    assert_eq!(allowed_once, json!({}));

    let denied_again = execute_hook_with_input(
        &cli,
        Event::PreToolUse,
        Some(Client::Codex),
        br#"{"tool_name":"Bash","tool_input":{"command":"git merge feat/demo"}}"#,
        at,
    )
    .await?;
    assert_eq!(
        denied_again["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );

    // 6. SessionEnd over UDS sends `NotifyCursor` and triggers background slice ingestion.
    let tp = dir.path().join("session-uds.jsonl");
    std::fs::write(
        &tp,
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"请记下：每周三下午做备份演练。\"}}\n",
    )?;
    let stop_res = execute_hook_with_input(
        &cli,
        Event::SessionEnd,
        Some(Client::ClaudeCode),
        serde_json::to_vec(&json!({
            "session_id": "uds-s1",
            "transcript_path": tp,
        }))?
        .as_slice(),
        at,
    )
    .await?;
    assert_eq!(stop_res["queued_daemon"], true);
    let status_client = DaemonClient::for_library(&lib_path)?.with_timeout(Duration::from_secs(2));
    let mut post_status = status_client.status().await?;
    for _ in 0..60 {
        if post_status.sources >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        post_status = status_client.status().await?;
    }
    assert_eq!(post_status.sources, 1);
    assert_eq!(post_status.tracked_sessions, 1);

    // 7. Shut down daemon and verify seamless fallback to local read-only (`ClaudeCode`) and read-write (`Codex`) hooks.
    handle.shutdown().await?;
    let fallback_ask = execute_hook_with_input(
        &cli,
        Event::PreToolUse,
        Some(Client::ClaudeCode),
        br#"{"tool_name":"Bash","tool_input":{"command":"git merge feat/demo"}}"#,
        at,
    )
    .await?;
    assert_eq!(
        fallback_ask["hookSpecificOutput"]["permissionDecision"],
        "ask"
    );
    let fallback_deny = execute_hook_with_input(
        &cli,
        Event::PreToolUse,
        Some(Client::Codex),
        br#"{"tool_name":"Bash","tool_input":{"command":"git merge feat/demo"}}"#,
        at,
    )
    .await?;
    assert_eq!(
        fallback_deny["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );
    Ok(())
}

#[tokio::test]
async fn automated_two_person_community_join_keeps_personal_events_private_and_auto_syncs_collision(
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;

    use kp_derive::{
        apply_extracted_atoms, build_extraction_batch_request, evaluate_ambiguous_scope_reviews,
        ConceptAlignmentTable, NlFunnelOptions,
    };
    use kp_model::{
        CalendarTable, ConceptKind, ConceptPolarity, ExtractionChunkSlice, ModelAmbiguousScope,
        ModelConceptRef, ModelExtractedAtom, ModelQuoteAnchor, ModelRoutePolicy, ModelScopeRef,
        ModelWriteOp, TimeZone,
    };
    use kp_store::{ChunkInput, RawSourceInput};

    let dir = tempfile::tempdir()?;
    let q_path = dir.path().join("q");
    let alice_path = dir.path().join("alice");
    let bob_path = dir.path().join("bob");
    let at = EpochMs(1_795_312_800_000);
    let shanghai = TimeZone::get("Asia/Shanghai")?;

    // 1. Initialize Community + Alice & Bob personal libraries (NO manual `kp member add` in personal libs!).
    let mut q_store = Store::init(
        &q_path,
        InitOptions {
            library_id: LibraryId::new("team-hub")?,
            name: "Team Hub".into(),
            mode: LibraryMode::Community,
            owner: "admin".parse()?,
            created_at: EpochMs(0),
        },
    )?;
    let alice_tok = q_store
        .add_member("alice".parse()?, kp_model::MemberRole::Member, EpochMs(1))?
        .token;
    let bob_tok = q_store
        .add_member("bob".parse()?, kp_model::MemberRole::Member, EpochMs(1))?
        .token;

    let _ = Store::init(
        &alice_path,
        InitOptions {
            library_id: LibraryId::new("alice-personal")?,
            name: "Alice Personal".into(),
            mode: LibraryMode::Personal,
            owner: "alice".parse()?,
            created_at: EpochMs(0),
        },
    )?;
    let _ = Store::init(
        &bob_path,
        InitOptions {
            library_id: LibraryId::new("bob-personal")?,
            name: "Bob Personal".into(),
            mode: LibraryMode::Personal,
            owner: "bob".parse()?,
            created_at: EpochMs(0),
        },
    )?;

    let q_sock = kp_net::daemon_runtime_dir()?.join(format!(
        "w3q-{}-{}.sock",
        std::process::id(),
        at.0 % 100_000
    ));
    let _q_handle = kp_net::serve_unix(&q_sock, kp_net::router(q_store))?;

    // 2. Both join via `kp community join` -> `0600` `communities.json` + automatic peer roster discovery!
    let alice_cli = test_cli(alice_path.clone(), "alice")?;
    crate::community::run(
        &alice_cli,
        &crate::community::CommunityCommand::Community {
            command: crate::community::CommunityManageCommand::Join {
                url: "http://community.local".into(),
                token: alice_tok,
                unix_socket: Some(q_sock.clone()),
            },
        },
    )
    .await?;

    let bob_cli = test_cli(bob_path.clone(), "bob")?;
    crate::community::run(
        &bob_cli,
        &crate::community::CommunityCommand::Community {
            command: crate::community::CommunityManageCommand::Join {
                url: "http://community.local".into(),
                token: bob_tok,
                unix_socket: Some(q_sock.clone()),
            },
        },
    )
    .await?;

    let comm_cfg_meta = std::fs::symlink_metadata(kp_net::communities_config_path(&alice_path))?;
    assert_eq!(comm_cfg_meta.permissions().mode() & 0o777, 0o600);
    assert!(
        Store::open_readonly(&bob_path)?
            .member_role(&"alice".parse()?)?
            .is_some(),
        "Bob's personal library must auto-register Alice from community roster"
    );

    // 3. Extract Alice's personal event (`audience = {alice}`) and Bob's Membership + ambiguous Constraint (`audience = {bob}`).
    {
        let mut a_store = Store::open(&alice_path)?;
        let a_body = "[speaker:alice] 当前已进入数据库发布维护窗口，正在升级核心服务。\n";
        let a_src = a_store.ingest_source(
            &a_store.library_id().clone(),
            &"session:alice:1".parse()?,
            &RawSourceInput {
                self_echo_filtered: false,
                uri: "session:alice-01".into(),
                media_type: "text/markdown".into(),
                content: a_body.as_bytes().to_vec(),
                readable_by: AudienceSpec::Explicit(BTreeSet::from(["alice".parse()?])),
                captured_at: at,
                chunks: vec![ChunkInput {
                    heading: String::new(),
                    body: a_body.into(),
                    anchor: kp_model::AnchorSpec::default(),
                }],
            },
        )?;
        let a_req = build_extraction_batch_request(
            &a_store,
            a_src.source_id.clone(),
            "alice".parse()?,
            CalendarTable::from_reference_in_tz(at, &shanghai)?,
            Vec::new(),
            vec![ExtractionChunkSlice {
                chunk_id: format!("{}-0", a_src.source_id),
                heading: String::new(),
                body: a_body.into(),
            }],
            10,
        )?;
        apply_extracted_atoms(
            &mut a_store,
            &a_req,
            &[ModelExtractedAtom {
                speaker: None,
                kind: FactKind::Event,
                write_op: ModelWriteOp::Add,
                claim_text: "当前已进入数据库发布维护窗口".into(),
                value: json!({"summary": "数据库发布维护窗口升级核心服务"}),
                quote: ModelQuoteAnchor {
                    chunk_id: format!("{}-0", a_src.source_id),
                    verbatim_quote: "当前已进入数据库发布维护窗口，正在升级核心服务".into(),
                    turn_ordinal: Some(0),
                    approx_char_offset: None,
                },
                valid_dates: None,
                valid_window: None,
                condition_concept: Some(ModelConceptRef::ProposeNew {
                    kind: ConceptKind::Condition,
                    label: "数据库发布维护窗口".into(),
                    domain: "ops".into(),
                    polarity: ConceptPolarity::Neutral,
                }),
                action_concept: None,
                group_concept: None,
                member_mention: None,
                scope: None,
                ambiguous_scope: None,
                assumed_premise_ids: Vec::new(),
            }],
            &NlFunnelOptions {
                audience: AudienceSpec::Explicit(BTreeSet::from(["alice".parse()?])),
                at,
                route_policy: ModelRoutePolicy::RawOnly,
                source_origin: None,
                rejections: Vec::new(),
                extra_concepts: Vec::new(),
            },
        )?;
    }

    let bob_review_id = {
        let mut b_store = Store::open(&bob_path)?;
        let b_body = "[speaker:bob] Alice 是我们发布值班组的成员；遇到数据库发布维护窗口都禁止执行 sqlx migrate。\n";
        let b_src = b_store.ingest_source(
            &b_store.library_id().clone(),
            &"session:bob:1".parse()?,
            &RawSourceInput {
                self_echo_filtered: false,
                uri: "session:bob-01".into(),
                media_type: "text/markdown".into(),
                content: b_body.as_bytes().to_vec(),
                readable_by: AudienceSpec::Explicit(BTreeSet::from(["bob".parse()?])),
                captured_at: at,
                chunks: vec![ChunkInput {
                    heading: String::new(),
                    body: b_body.into(),
                    anchor: kp_model::AnchorSpec::default(),
                }],
            },
        )?;
        let b_req = build_extraction_batch_request(
            &b_store,
            b_src.source_id.clone(),
            "bob".parse()?,
            CalendarTable::from_reference_in_tz(at, &shanghai)?,
            Vec::new(),
            vec![ExtractionChunkSlice {
                chunk_id: format!("{}-0", b_src.source_id),
                heading: String::new(),
                body: b_body.into(),
            }],
            10,
        )?;
        let b_out = apply_extracted_atoms(
            &mut b_store,
            &b_req,
            &[
                ModelExtractedAtom {
                    speaker: None,
                    kind: FactKind::Membership,
                    write_op: ModelWriteOp::Add,
                    claim_text: "alice 是发布值班组成员".into(),
                    value: json!({}),
                    quote: ModelQuoteAnchor {
                        chunk_id: format!("{}-0", b_src.source_id),
                        verbatim_quote: "Alice 是我们发布值班组的成员".into(),
                        turn_ordinal: Some(0),
                        approx_char_offset: None,
                    },
                    valid_dates: None,
                    valid_window: None,
                    condition_concept: None,
                    action_concept: None,
                    group_concept: Some(ModelConceptRef::ProposeNew {
                        kind: ConceptKind::Group,
                        label: "发布值班组".into(),
                        domain: "org".into(),
                        polarity: ConceptPolarity::Neutral,
                    }),
                    member_mention: Some("Alice".into()),
                    scope: None,
                    ambiguous_scope: None,
                    assumed_premise_ids: Vec::new(),
                },
                ModelExtractedAtom {
                    speaker: None,
                    kind: FactKind::Constraint,
                    write_op: ModelWriteOp::Add,
                    claim_text: "数据库发布维护窗口期间禁止执行 sqlx migrate".into(),
                    value: json!({"command": "sqlx migrate"}),
                    quote: ModelQuoteAnchor {
                        chunk_id: format!("{}-0", b_src.source_id),
                        verbatim_quote: "遇到数据库发布维护窗口都禁止执行 sqlx migrate".into(),
                        turn_ordinal: Some(0),
                        approx_char_offset: None,
                    },
                    valid_dates: None,
                    valid_window: None,
                    condition_concept: Some(ModelConceptRef::ProposeNew {
                        kind: ConceptKind::Condition,
                        label: "数据库发布维护窗口".into(),
                        domain: "ops".into(),
                        polarity: ConceptPolarity::Neutral,
                    }),
                    action_concept: Some(ModelConceptRef::ProposeNew {
                        kind: ConceptKind::Action,
                        label: "禁止执行 sqlx migrate".into(),
                        domain: "ops".into(),
                        polarity: ConceptPolarity::Neutral,
                    }),
                    group_concept: None,
                    member_mention: None,
                    scope: None,
                    ambiguous_scope: Some(ModelAmbiguousScope {
                        broader_candidate: ModelScopeRef::RelationGroup {
                            group_concept: ModelConceptRef::ProposeNew {
                                kind: ConceptKind::Group,
                                label: "发布值班组".into(),
                                domain: "org".into(),
                                polarity: ConceptPolarity::Neutral,
                            },
                        },
                        question: "该维护窗口禁令是否对整个发布值班组生效？".into(),
                    }),
                    assumed_premise_ids: Vec::new(),
                },
            ],
            &NlFunnelOptions {
                audience: AudienceSpec::Explicit(BTreeSet::from(["bob".parse()?])),
                at,
                route_policy: ModelRoutePolicy::RawOnly,
                source_origin: None,
                rejections: Vec::new(),
                extra_concepts: Vec::new(),
            },
        )?;
        for c in &b_out.created_concepts {
            b_store.upsert_community_concept(c)?;
        }
        let graph = b_store.derivation_view()?;
        let align = ConceptAlignmentTable::from_graph(&graph);
        let reviews = evaluate_ambiguous_scope_reviews(&graph, &align, &[], at)?;
        assert_eq!(reviews.len(), 1);
        b_store.upsert_open_review_item(&reviews[0])?.id
    };

    // 4. Spawn background daemons on both Alice and Bob (`idle_extract_window = 50ms` -> auto-sync every 100ms!).
    let mut a_cfg = DaemonConfig::for_library(&alice_path.canonicalize()?)?;
    a_cfg.idle_extract_window = Duration::from_millis(50);
    let a_daemon = DaemonServer::spawn(
        Store::open(&alice_path)?,
        a_cfg,
        Arc::new(CliDaemonHandler::default()),
    )
    .await?;

    let mut b_cfg = DaemonConfig::for_library(&bob_path.canonicalize()?)?;
    b_cfg.idle_extract_window = Duration::from_millis(50);
    let b_daemon = DaemonServer::spawn(
        Store::open(&bob_path)?,
        b_cfg,
        Arc::new(CliDaemonHandler::default()),
    )
    .await?;

    // Before Bob confirms `kp review`, Bob's Membership auto-publishes, Bob's unconfirmed Constraint stays local,
    // and Alice's personal Event NEVER leaves Alice's machine!
    tokio::time::sleep(Duration::from_millis(220)).await;
    {
        let b_ro = Store::open_readonly(&bob_path)?;
        let b_vis = b_ro.visible_facts(&"bob".parse()?, EpochMs(at.0 + 500))?;
        assert!(
            b_vis.iter().all(|f| f.author.as_str() != "alice"),
            "Alice's personal event must NEVER be published to Bob!"
        );
    }

    // 5. Bob runs ONLY `kp review resolve <id> --choice confirm` -> automatically pushes widened rule to Community,
    //    and Alice's daemon automatically pulls it and derives `CollisionWarning` locally!
    crate::governance::run(
        &bob_cli,
        &crate::governance::GovernanceCommand::Review {
            command: crate::governance::ReviewCommand::Resolve {
                review_id: bob_review_id,
                choice: Some(crate::governance::ReviewChoice::Confirm),
                positional_choice: None,
                at: Some(at.0 + 1_000),
            },
        },
    )
    .await?;

    let eval_at = EpochMs(at.0 + 2_000);
    let mut alice_ups = json!({});
    for _ in 0..40 {
        alice_ups = execute_hook_with_input(
            &alice_cli,
            Event::UserPromptSubmit,
            Some(Client::ClaudeCode),
            r#"{"prompt":"数据库发布维护窗口期间准备执行 sqlx migrate"}"#.as_bytes(),
            eval_at,
        )
        .await?;
        if alice_ups["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .is_some_and(|s| s.contains("↳") && s.contains("sqlx migrate"))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    let ctx = alice_ups["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .ok_or("expected automated <kp-context> collision warning on Alice")?;
    assert!(ctx.contains("禁止执行 sqlx migrate"), "ctx={ctx}");
    assert!(
        ctx.contains("↳"),
        "expected human premise quotes in ctx={ctx}"
    );

    // Verify PreToolUse blocks `sqlx migrate run` on Alice (`ask` on Claude Code, `deny` on Codex)
    // AND Bob still has ZERO facts authored by Alice!
    let alice_pre = execute_hook_with_input(
        &alice_cli,
        Event::PreToolUse,
        Some(Client::Codex),
        br#"{"tool_name":"Bash","tool_input":{"command":"sqlx migrate run"}}"#,
        eval_at,
    )
    .await?;
    assert_eq!(
        alice_pre["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );
    assert!(
        Store::open_readonly(&bob_path)?
            .visible_facts(&"bob".parse()?, eval_at)?
            .iter()
            .all(|f| f.author.as_str() != "alice"),
        "Alice's personal event must remain strictly local after full round-trip!"
    );

    // 6. Bob forgets his widened constraint via `MemoryCommand::Forget` -> immediately flushes withdrawal
    //    to Community so Alice's `CollisionWarning` disappears automatically!
    let active_bob_constraint = Store::open_readonly(&bob_path)?
        .visible_facts(&"bob".parse()?, eval_at)?
        .into_iter()
        .find(|f| f.kind == FactKind::Constraint)
        .ok_or("expected active constraint on Bob")?
        .id;
    crate::memory::run(
        &bob_cli,
        &crate::memory::MemoryCommand::Forget {
            fact_id_or_logical_key: active_bob_constraint.to_string(),
            at: Some(eval_at.0 + 1_000),
        },
    )
    .await?;

    let post_forget_at = EpochMs(eval_at.0 + 2_000);
    let mut cleared = false;
    for _ in 0..40 {
        let check_pre = execute_hook_with_input(
            &alice_cli,
            Event::PreToolUse,
            Some(Client::Codex),
            br#"{"tool_name":"Bash","tool_input":{"command":"sqlx migrate run"}}"#,
            post_forget_at,
        )
        .await?;
        if check_pre == json!({}) {
            cleared = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    assert!(
        cleared,
        "expected MemoryCommand::Forget to propagate withdrawal and clear Alice's guard"
    );

    a_daemon.shutdown().await?;
    b_daemon.shutdown().await?;
    let _ = std::fs::remove_file(&q_sock);
    Ok(())
}

#[tokio::test]
async fn community_join_rejects_bad_or_actor_mismatched_token_without_saving_config(
) -> Result<(), Box<dyn std::error::Error>> {
    use axum::routing::get;
    use axum::{Json, Router};

    let dir = tempfile::tempdir()?;
    let q_path = dir.path().join("q");
    let alice_path = dir.path().join("alice");
    let at = EpochMs(1_795_400_000_000);

    let mut q_store = Store::init(
        &q_path,
        InitOptions {
            library_id: LibraryId::new("hub-auth-check")?,
            name: "Hub Auth Check".into(),
            mode: LibraryMode::Community,
            owner: "admin".parse()?,
            created_at: EpochMs(0),
        },
    )?;
    let alice_tok = q_store
        .add_member("alice".parse()?, kp_model::MemberRole::Member, EpochMs(1))?
        .token;

    let _ = Store::init(
        &alice_path,
        InitOptions {
            library_id: LibraryId::new("alice-auth-lib")?,
            name: "Alice Auth".into(),
            mode: LibraryMode::Personal,
            owner: "alice".parse()?,
            created_at: EpochMs(0),
        },
    )?;
    let alice_cli = test_cli(alice_path.clone(), "alice")?;

    let real_sock = kp_net::daemon_runtime_dir()?.join(format!(
        "join401-{}-{}.sock",
        std::process::id(),
        at.0 % 100_000
    ));
    let _real_handle = kp_net::serve_unix(&real_sock, kp_net::router(q_store))?;

    // 1. Invalid token (`401`) MUST return `Err` and NOT write `communities.json`!
    let bad_tok_res = crate::community::run(
        &alice_cli,
        &crate::community::CommunityCommand::Community {
            command: crate::community::CommunityManageCommand::Join {
                url: "http://community.local".into(),
                token: "kp_invalid_token_value".into(),
                unix_socket: Some(real_sock.clone()),
            },
        },
    )
    .await;
    assert!(bad_tok_res.is_err());
    assert!(!kp_net::communities_config_path(&alice_path).exists());

    // 2. `/v1/status` returning `"actor": "bob"` when local actor is `"alice"` MUST fail BEFORE saving `communities.json`!
    let mismatch_sock = kp_net::daemon_runtime_dir()?.join(format!(
        "joinact-{}-{}.sock",
        std::process::id(),
        at.0 % 100_000
    ));
    let mismatch_app = Router::new().route(
        "/v1/status",
        get(|| async {
            Json(json!({
                "library_id": "hub-auth-check",
                "name": "Hub Auth Check",
                "mode": "community",
                "actor": "bob",
                "facts": 0,
                "chunks": 0
            }))
        }),
    );
    let _mismatch_handle = kp_net::serve_unix(&mismatch_sock, mismatch_app)?;

    let mismatch_res = crate::community::run(
        &alice_cli,
        &crate::community::CommunityCommand::Community {
            command: crate::community::CommunityManageCommand::Join {
                url: "http://community.local".into(),
                token: "kp_bob_token".into(),
                unix_socket: Some(mismatch_sock.clone()),
            },
        },
    )
    .await;
    let mismatch_err = mismatch_res
        .err()
        .ok_or("expected actor mismatch join error")?
        .to_string();
    assert!(
        mismatch_err.contains("community token authenticates as 'bob'")
            && mismatch_err.contains("expected local actor 'alice'"),
        "unexpected mismatch error: {mismatch_err}"
    );
    assert!(
        !kp_net::communities_config_path(&alice_path).exists(),
        "communities.json must NOT be written when token actor mismatches local actor"
    );

    // 2b. Unreachable `/v1/status` (transport/timeout error) AND `/v1/status` missing `"actor"`
    //     BOTH fail closed without saving `communities.json`!
    let missing_sock = dir.path().join("nonexistent_status.sock");
    assert!(crate::community::run(
        &alice_cli,
        &crate::community::CommunityCommand::Community {
            command: crate::community::CommunityManageCommand::Join {
                url: "http://community.local".into(),
                token: alice_tok.clone(),
                unix_socket: Some(missing_sock),
            },
        },
    )
    .await
    .is_err());
    assert!(!kp_net::communities_config_path(&alice_path).exists());

    let no_actor_sock = kp_net::daemon_runtime_dir()?.join(format!(
        "cj_na_{}.sock",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .subsec_nanos()
    ));
    let no_actor_app = axum::Router::new().route(
        "/v1/status",
        axum::routing::get(|| async {
            axum::Json(json!({
                "library_id": "comm-join-guard",
                "mode": "community"
            }))
        }),
    );
    let _no_actor_handle = kp_net::serve_unix(&no_actor_sock, no_actor_app)?;
    assert!(crate::community::run(
        &alice_cli,
        &crate::community::CommunityCommand::Community {
            command: crate::community::CommunityManageCommand::Join {
                url: "http://community.local".into(),
                token: alice_tok.clone(),
                unix_socket: Some(no_actor_sock.clone()),
            },
        },
    )
    .await
    .is_err());
    assert!(!kp_net::communities_config_path(&alice_path).exists());
    let _ = std::fs::remove_file(&no_actor_sock);

    // 3. Valid token matching `"alice"` succeeds and writes `communities.json`.
    crate::community::run(
        &alice_cli,
        &crate::community::CommunityCommand::Community {
            command: crate::community::CommunityManageCommand::Join {
                url: "http://community.local".into(),
                token: alice_tok,
                unix_socket: Some(real_sock.clone()),
            },
        },
    )
    .await?;
    assert!(kp_net::communities_config_path(&alice_path).is_file());

    // 4. When `<socket>.lock` is held, `is_daemon_lock_held` is `true` and `maybe_autospawn_daemon_from_hook`
    //    returns immediately without creating `daemon.spawning`.
    let canon_alice = alice_path.canonicalize()?;
    let sock_path = kp_net::daemon_socket_path(&canon_alice)?;
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_path = kp_net::daemon_lock_path_for_socket(&sock_path);
    let held_lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    held_lock.try_lock()?;
    assert!(kp_net::is_daemon_lock_held(&canon_alice));
    super::maybe_autospawn_daemon_from_hook(&canon_alice);
    assert!(!canon_alice.join(super::SPAWNING_LOCK_FILE).exists());
    drop(held_lock);
    let _ = std::fs::remove_file(&lock_path);

    let _ = std::fs::remove_file(&real_sock);
    let _ = std::fs::remove_file(&mismatch_sock);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transient_extract_failure_retries_to_completion_and_alignment_generation_preserves_concurrent_dirty(
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir()?;
    let lib_path = dir.path().join("retry-lib");
    let at = EpochMs(1_763_683_200_000);
    let store = Store::init(
        &lib_path,
        InitOptions {
            library_id: LibraryId::new("retry-lib")?,
            name: "Retry Lib".into(),
            mode: LibraryMode::Personal,
            owner: "alice".parse()?,
            created_at: EpochMs(0),
        },
    )?;
    drop(store);

    // 1. Write a session transcript and capture it via `SessionEnd` fallback so a session sub-batch job is queued.
    let transcript = dir.path().join("sess_retry.jsonl");
    std::fs::write(
        &transcript,
        "{\"type\":\"user\",\"timestamp\":\"2026-11-21T10:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"以后每周四下午三点固定复盘数据库索引计划\"}}\n",
    )?;
    let cli = test_cli(lib_path.clone(), "alice")?;
    let hook_in = serde_json::to_vec(&json!({
        "session_id": "sess_retry_01",
        "transcript_path": transcript,
    }))?;
    let mut fallback_store = Store::open(&lib_path)?;
    let hook_val: serde_json::Value = serde_json::from_slice(&hook_in)?;
    kp_ingest::hooks::run(
        &mut fallback_store,
        Event::Stop.as_str(),
        &hook_val,
        &"alice".parse()?,
        at,
    )
    .await?;
    assert_eq!(
        fallback_store
            .get_session_cursor("sess_retry_01")?
            .map(|c| c.last_extracted_offset),
        Some(0)
    );
    drop(fallback_store);

    // 2. Mock host CLI runner script: fails on call #1 (simulating transient timeout), succeeds on call #2!
    let counter_path = dir.path().join("calls.txt");
    let script_path = dir.path().join("mock_claude_retry.sh");
    let script = format!(
        r#"#!/usr/bin/env python3
import json, os, sys
cf = {cf:?}
if not os.path.exists(cf):
    open(cf, "w").write("1")
    sys.stderr.write("simulated transient timeout\n")
    sys.exit(1)
prompt = sys.stdin.read()
if "<chunk id=" not in prompt:
    print(json.dumps({{"structured_output": {{"verdict": "unrelated", "question": None}}}}))
    sys.exit(0)
print(json.dumps({{"structured_output": {{"skip_summary": False, "atoms": [{{"kind": "decision", "claim_text": "每周四下午三点复盘数据库索引计划", "value_json": "{{}}", "scope": {{"scope": "speaker_self", "mentions": None, "group_concept": None}}, "ambiguous_scope": None, "condition_concept": None, "action_concept": None, "group_concept": None, "member_mention": None, "valid_dates": None, "speaker": "alice", "assumed_premise_ids": [], "quote": {{"chunk_id": "__FIRST_CHUNK__", "verbatim_quote": "每周四下午三点固定复盘数据库索引计划", "turn_ordinal": None, "approx_char_offset": None}}, "write_op": {{"op": "add", "target_fact_id": None}}}}]}}}}, ensure_ascii=False))
"#,
        cf = counter_path.display().to_string()
    );
    // Discover the exact `chunk_id` queued for `sess_retry_01` so the mock quote anchor resolves cleanly.
    let ro = Store::open_readonly(&lib_path)?;
    let first_chunk_id = {
        let mut tmp = Store::open(&lib_path)?;
        let job = tmp
            .lease_next_extraction_job(at, 1_000)?
            .ok_or("missing queued job")?;
        let cid = job.chunk_ids.first().cloned().ok_or("missing chunk_id")?;
        tmp.release_extraction_job_for_retry(&job.job_id, job.attempts, "prep", at, 5)?;
        cid
    };
    drop(ro);
    std::fs::write(
        &script_path,
        script.replace("__FIRST_CHUNK__", &first_chunk_id),
    )?;
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))?;

    let extract_cmd =
        crate::governance::GovernanceCommand::Extract(crate::governance::ExtractArgs {
            runner: crate::governance::RunnerKind::ClaudeCode,
            program: Some(script_path.clone()),
            model: None,
            policy: "host".into(),
            remaining_quota_pct: None,
            min_quota_pct: 10,
            max_jobs: 4,
            text: None,
            file: None,
            audience: None,
            force: false,
            at: None,
        });

    // Attempt #1 fails transiently (`Err`), does NOT mark `FailedPermanent`, cursor stays at 0.
    assert!(crate::governance::run(&cli, &extract_cmd).await.is_err());
    {
        let check_st = Store::open_readonly(&lib_path)?;
        assert_eq!(
            check_st
                .get_session_cursor("sess_retry_01")?
                .map(|c| c.last_extracted_offset),
            Some(0)
        );
    }

    // Verify CLI -> scheduler confirmation chain: while `[0, end)` is actively `leased` (not yet done),
    // `CliDaemonHandler::extract_session_batch` reuses the span (`subbatches_ingested == 0`) WITHOUT
    // prematurely advancing `last_extracted_offset` to `end`!
    {
        use kp_net::DaemonHandler;
        let lease_clock = crate::now()?;
        let mut active_st = Store::open(&lib_path)?;
        let cur_snap = active_st
            .get_session_cursor("sess_retry_01")?
            .ok_or("missing cursor before lease test")?;
        let leased_job = active_st
            .lease_next_extraction_job(lease_clock, 300_000)?
            .ok_or("expected leased job")?;
        drop(active_st);

        let handler = CliDaemonHandler::default();
        let res = handler
            .extract_session_batch(
                &lib_path,
                kp_net::IdleSessionBatch {
                    session_id: "sess_retry_01".into(),
                    client_kind: cur_snap.client_kind.clone(),
                    transcript_path: cur_snap.transcript_path.clone(),
                    start_byte_offset: 0,
                    end_byte_offset: cur_snap.last_byte_offset,
                    line_count: cur_snap.last_line_count,
                    prefix_sha256: cur_snap.prefix_sha256.clone(),
                    actor: "alice".parse()?,
                    at: lease_clock,
                    session_ended: false,
                },
            )
            .await;
        // Must NOT return `Ok(Some(end_byte_offset))` while `leased_job` is still incomplete!
        assert!(
            res.is_err(),
            "expected Err while span is still leased, got {res:?}"
        );
        let check_st = Store::open_readonly(&lib_path)?;
        assert_eq!(
            check_st
                .get_session_cursor("sess_retry_01")?
                .map(|c| c.last_extracted_offset),
            Some(0)
        );
        drop(check_st);

        let mut rel_st = Store::open(&lib_path)?;
        rel_st.release_extraction_job_for_retry(
            &leased_job.job_id,
            leased_job.attempts,
            "release-for-attempt2",
            lease_clock,
            5,
        )?;
    }

    // Attempt #2 recovers the job automatically, creates the decision fact, and advances `last_extracted_offset`!
    crate::governance::run(&cli, &extract_cmd).await?;
    {
        let check_st = Store::open_readonly(&lib_path)?;
        let cur = check_st
            .get_session_cursor("sess_retry_01")?
            .ok_or("missing cursor")?;
        assert!(cur.last_extracted_offset > 0);
        assert_eq!(cur.last_extracted_offset, cur.last_byte_offset);
        assert_eq!(check_st.status()?.facts, 1);
    }

    // 3. Verify `alignment_dirty_generation`: a concurrent write committed while alignment LLM runs
    //    is NOT cleared by `ack_alignment_generation`!
    struct ConcurrentDirtyLlm {
        lib_dir: std::path::PathBuf,
        triggered: std::sync::atomic::AtomicBool,
    }
    impl kp_model::LlmClient for ConcurrentDirtyLlm {
        fn extract_batch<'a>(
            &'a self,
            _req: &'a kp_model::ExtractionBatchRequest,
        ) -> kp_model::BoxFuture<'a, kp_model::Result<kp_model::ExtractionBatchOutcome>> {
            Box::pin(async { Ok(kp_model::ExtractionBatchOutcome::SkippedEmpty) })
        }
        fn judge_entailment<'a>(
            &'a self,
            _req: &'a kp_model::EntailmentQuery,
        ) -> kp_model::BoxFuture<'a, kp_model::Result<kp_model::EntailmentVerdict>> {
            Box::pin(async move {
                if !self
                    .triggered
                    .swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    let mut writer = Store::open(&self.lib_dir).map_err(|_| {
                        kp_model::KpError::invalid("store", "concurrent open failed")
                    })?;
                    writer
                        .set_needs_alignment(true, EpochMs(1_763_683_200_500))
                        .map_err(|_| {
                            kp_model::KpError::invalid(
                                "store",
                                "concurrent set_needs_alignment failed",
                            )
                        })?;
                }
                Ok(kp_model::EntailmentVerdict::Unrelated)
            })
        }
    }

    let mut align_store = Store::open(&lib_path)?;
    align_store.add_member("bob".parse()?, kp_model::MemberRole::Member, at)?;
    let cpt_a = kp_model::CommunityConcept {
        id: "cpt_gen_a".parse()?,
        canonical_label: "周四下午".into(),
        kind: kp_model::ConceptKind::Condition,
        domain: "schedule".into(),
        polarity: kp_model::ConceptPolarity::Positive,
        proposed_by: "alice".parse()?,
        created_at: at,
    };
    let cpt_b = kp_model::CommunityConcept {
        id: "cpt_gen_b".parse()?,
        canonical_label: "周五上午".into(),
        kind: kp_model::ConceptKind::Condition,
        domain: "schedule".into(),
        polarity: kp_model::ConceptPolarity::Positive,
        proposed_by: "bob".parse()?,
        created_at: at,
    };
    let cpt_act = kp_model::CommunityConcept {
        id: "cpt_gen_act".parse()?,
        canonical_label: "发布变更".into(),
        kind: kp_model::ConceptKind::Action,
        domain: "ops".into(),
        polarity: kp_model::ConceptPolarity::Negative,
        proposed_by: "alice".parse()?,
        created_at: at,
    };
    align_store.upsert_community_concept(&cpt_a)?;
    align_store.upsert_community_concept(&cpt_b)?;
    align_store.upsert_community_concept(&cpt_act)?;
    let shared_aud = AudienceSpec::Explicit(BTreeSet::from(["alice".parse()?, "bob".parse()?]));
    align_store.insert_fact(&Fact {
        id: "fact_gen_rule".parse()?,
        origin_library: align_store.library_id().clone(),
        logical_key: "gen-rule".parse()?,
        kind: FactKind::Constraint,
        subject: "ops".into(),
        predicate: "kp.constraint".into(),
        object: json!({
            "claim_text": "周四下午暂停发布变更",
            "condition_concept_id": "cpt_gen_a",
            "action_concept_id": "cpt_gen_act"
        }),
        tags: BTreeSet::new(),
        author: "alice".parse()?,
        ingested_by: None,
        trust_tier: TrustTier::HumanVerbatim,
        access: FactAccess::Base {
            audience: shared_aud.clone(),
            stamp: BitemporalStamp {
                valid: ValidityWindow {
                    valid_from: EpochMs(0),
                    valid_until: EpochMs::OPEN_END,
                },
                recorded_at: at,
                expired_at: None,
            },
        },
        source_scope_id: None,
        governed_scope: None,
        broader_scope: None,
        status: FactStatus::Active,
        superseded_by: None,
        rank: FactRank::Normal,
        source_anchor: None,
        unverified_gaps: Vec::new(),
        miss_count: 0,
    })?;
    align_store.insert_fact(&Fact {
        id: "fact_gen_evt".parse()?,
        origin_library: align_store.library_id().clone(),
        logical_key: "gen-evt".parse()?,
        kind: FactKind::Event,
        subject: "ops".into(),
        predicate: "kp.event".into(),
        object: json!({
            "claim_text": "周五上午安排常规检查",
            "condition_concept_id": "cpt_gen_b"
        }),
        tags: BTreeSet::new(),
        author: "bob".parse()?,
        ingested_by: None,
        trust_tier: TrustTier::HumanVerbatim,
        access: FactAccess::Base {
            audience: shared_aud,
            stamp: BitemporalStamp {
                valid: ValidityWindow {
                    valid_from: EpochMs(0),
                    valid_until: EpochMs::OPEN_END,
                },
                recorded_at: at,
                expired_at: None,
            },
        },
        source_scope_id: None,
        governed_scope: None,
        broader_scope: None,
        status: FactStatus::Active,
        superseded_by: None,
        rank: FactRank::Normal,
        source_anchor: None,
        unverified_gaps: Vec::new(),
        miss_count: 0,
    })?;
    assert!(align_store.needs_alignment()?);

    let race_llm = ConcurrentDirtyLlm {
        lib_dir: lib_path.clone(),
        triggered: std::sync::atomic::AtomicBool::new(false),
    };
    crate::governance::run_concept_alignment_and_derive(&mut align_store, Some(&race_llm), at)
        .await?;
    assert!(
        race_llm.triggered.load(std::sync::atomic::Ordering::SeqCst),
        "expected check_entailment to fire"
    );
    // Concurrent dirty write during LLM evaluation MUST keep `needs_alignment() == true`!
    assert!(align_store.needs_alignment()?);

    // Second pass with no concurrent writer cleanly clears `needs_alignment() == false`!
    crate::governance::run_concept_alignment_and_derive(&mut align_store, Some(&race_llm), at)
        .await?;
    assert!(!align_store.needs_alignment()?);
    Ok(())
}

#[tokio::test]
async fn daemon_uds_ingest_jsonl_applies_rosters_upsert_supersede_withdraw_and_swappable_embedder(
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let lib_path = dir.path().join("lib");
    let at = EpochMs(1_763_683_200_000);
    let mut store = Store::init_with_embedding(
        &lib_path,
        InitOptions {
            library_id: LibraryId::new("daemon-ingest-test")?,
            name: "Daemon Ingest Test".into(),
            mode: LibraryMode::Personal,
            owner: "alice".parse()?,
            created_at: EpochMs(0),
        },
        "hash:v1",
        256,
    )?;
    store.add_member("bob".parse()?, kp_model::MemberRole::Member, EpochMs(0))?;
    store.add_member("carol".parse()?, kp_model::MemberRole::Member, EpochMs(0))?;
    drop(store);

    let mut daemon_store = Store::open(&lib_path)?;
    let (swappable, pending_local) =
        crate::providers_cli::build_daemon_swappable_embedder(&daemon_store);
    assert!(pending_local.is_none());
    daemon_store.set_embedder(Arc::clone(&swappable) as Arc<dyn kp_store::Embedder + Send + Sync>);

    let canonical_lib = lib_path.canonicalize()?;
    let mut cfg = DaemonConfig::for_library(&canonical_lib)?;
    cfg.idle_extract_window = Duration::from_millis(50);
    let handle = DaemonServer::spawn(
        daemon_store,
        cfg,
        Arc::new(CliDaemonHandler::with_embedder(Arc::clone(&swappable))),
    )
    .await?;

    let client = DaemonClient::for_library(&lib_path)?;
    let initial_status = client.status().await?;
    assert_eq!(initial_status.embedder.model, "hash:v1");
    assert_eq!(initial_status.embedder.state, "ready");
    assert_eq!(initial_status.embedder.download_pct, Some(100));

    // 1. Roster (`alice`, `bob`) + Upsert v1 over Daemon UDS `IngestJsonl`.
    let batch_1 = [
        json!({
            "container_id": "room.backend",
            "label": "Backend Channel",
            "members": ["alice", "bob"],
            "all_library_members": false,
            "synced_at": at.0
        })
        .to_string(),
        json!({
            "source": {
                "kind": "chat",
                "uri": "chat://room.backend/msg-1",
                "external_id": "msg-1",
                "container_id": "room.backend"
            },
            "speaker": "alice",
            "audience": "container_members",
            "occurred_at": at.0,
            "content": "灰度窗口暂定周三晚间十点执行",
            "op": "upsert"
        })
        .to_string(),
    ]
    .join("\n");

    let res_1 = client
        .ingest_jsonl(kp_net::IngestJsonlRequest {
            jsonl: batch_1.clone(),
            actor: Some("alice".parse()?),
            at: Some(at),
            extract_now: false,
        })
        .await?;
    assert_eq!(res_1.rosters_applied, 1);
    assert_eq!(res_1.records_upserted, 1);
    assert_eq!(res_1.source_ids.len(), 1);

    // Idempotent replay of the exact same record does not create duplicate sources.
    let res_dup = client
        .ingest_jsonl(kp_net::IngestJsonlRequest {
            jsonl: batch_1,
            actor: Some("alice".parse()?),
            at: Some(at),
            extract_now: false,
        })
        .await?;
    assert_eq!(res_dup.records_upserted, 0);

    {
        let ro = Store::open_readonly(&lib_path)?;
        let bob_hits =
            kp_retrieve::search_shallow(&ro, &"bob".parse()?, at, "灰度窗口 周三晚间", 5)?;
        assert_eq!(bob_hits.results.len(), 1);
        let carol_hits =
            kp_retrieve::search_shallow(&ro, &"carol".parse()?, at, "灰度窗口 周三晚间", 5)?;
        assert!(carol_hits.results.is_empty());
    }

    // 2. Roster update (`carol` joins, `bob` leaves) + Supersede v1 -> v2.
    let batch_2 = [
        json!({
            "container_id": "room.backend",
            "label": "Backend Channel",
            "members": ["alice", "carol"],
            "all_library_members": false,
            "synced_at": at.0 + 1_000
        })
        .to_string(),
        json!({
            "source": {
                "kind": "chat",
                "uri": "chat://room.backend/msg-2",
                "external_id": "msg-2",
                "container_id": "room.backend"
            },
            "speaker": "alice",
            "audience": "container_members",
            "occurred_at": at.0 + 1_000,
            "content": "灰度窗口改期至周四凌晨两点执行",
            "op": "supersede",
            "previous_external_id": "msg-1"
        })
        .to_string(),
    ]
    .join("\n");

    let res_2 = client
        .ingest_jsonl(kp_net::IngestJsonlRequest {
            jsonl: batch_2,
            actor: Some("alice".parse()?),
            at: Some(EpochMs(at.0 + 1_000)),
            extract_now: false,
        })
        .await?;
    assert_eq!(res_2.rosters_applied, 1);
    assert_eq!(res_2.records_superseded, 1);

    {
        let ro = Store::open_readonly(&lib_path)?;
        let carol_v2 = kp_retrieve::search_shallow(
            &ro,
            &"carol".parse()?,
            EpochMs(at.0 + 2_000),
            "周四凌晨两点",
            5,
        )?;
        assert_eq!(carol_v2.results.len(), 1);
        let carol_v1 = kp_retrieve::search_shallow(
            &ro,
            &"carol".parse()?,
            EpochMs(at.0 + 2_000),
            "周三晚间十点",
            5,
        )?;
        assert!(carol_v1.results.is_empty());
        let bob_v2 = kp_retrieve::search_shallow(
            &ro,
            &"bob".parse()?,
            EpochMs(at.0 + 2_000),
            "周四凌晨两点",
            5,
        )?;
        assert!(bob_v2.results.is_empty());
    }

    // 3. Withdraw v2 over Daemon UDS.
    let withdraw_line = json!({
        "source": {
            "kind": "chat",
            "uri": "chat://room.backend/msg-2",
            "external_id": "msg-2",
            "container_id": "room.backend"
        },
        "speaker": "alice",
        "audience": "container_members",
        "occurred_at": at.0 + 3_000,
        "content": "",
        "op": "withdraw"
    })
    .to_string();

    let res_3 = client
        .ingest_jsonl(kp_net::IngestJsonlRequest {
            jsonl: withdraw_line,
            actor: Some("alice".parse()?),
            at: Some(EpochMs(at.0 + 3_000)),
            extract_now: false,
        })
        .await?;
    assert_eq!(res_3.records_withdrawn, 1);

    {
        let ro = Store::open_readonly(&lib_path)?;
        let alice_after = kp_retrieve::search_shallow(
            &ro,
            &"alice".parse()?,
            EpochMs(at.0 + 4_000),
            "周四凌晨两点",
            5,
        )?;
        assert!(alice_after.results.is_empty());
    }

    // 4. External `reembed_stale` migrating `library_meta` (`hash:v1` -> `none`) dynamically refreshes
    //    the daemon's resident `SwappableEmbedder` and observable `embedder_status` on next operation!
    {
        let mut ext_store = Store::open(&lib_path)?;
        ext_store.reembed_stale(&kp_store::FtsOnlyEmbedder, Some(("none", 0)))?;
    }
    let refresh_line = json!({
        "source": {
            "kind": "chat",
            "uri": "chat://room.backend/msg-3",
            "external_id": "msg-3",
            "container_id": "room.backend"
        },
        "speaker": "alice",
        "audience": "container_members",
        "occurred_at": at.0 + 5_000,
        "content": "向量关闭后自动回退全文检索",
        "op": "upsert"
    })
    .to_string();
    let _ = client
        .ingest_jsonl(kp_net::IngestJsonlRequest {
            jsonl: refresh_line,
            actor: Some("alice".parse()?),
            at: Some(EpochMs(at.0 + 5_000)),
            extract_now: false,
        })
        .await?;
    let refreshed_status = client.status().await?;
    assert_eq!(refreshed_status.embedder.model, "none");
    assert_eq!(refreshed_status.embedder.state, "disabled");
    assert_eq!(refreshed_status.embedder.download_pct, None);

    handle.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn daemon_query_context_hot_swaps_embedder_across_external_reembed_migrations(
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let lib_path = dir.path().join("lib");
    let at = EpochMs(1_795_405_000_000);

    // 1. Start Daemon on model A = `none` (`0d`, FTS5-only).
    let mut store = Store::init_with_embedding(
        &lib_path,
        InitOptions {
            library_id: LibraryId::new("uds-reembed-hot")?,
            name: "UDS Reembed Hot Swap".into(),
            mode: LibraryMode::Personal,
            owner: "alice".parse()?,
            created_at: EpochMs(0),
        },
        "none",
        0,
    )?;
    store.set_embedder(Arc::new(kp_store::FtsOnlyEmbedder));
    let (swappable, _) = crate::providers_cli::build_daemon_swappable_embedder(&store);
    let mut dcfg = DaemonConfig::for_library(&lib_path)?;
    dcfg.idle_extract_window = Duration::from_secs(60);
    let handle = DaemonServer::spawn(
        store,
        dcfg,
        Arc::new(CliDaemonHandler::with_embedder(swappable)),
    )
    .await?;
    let client = DaemonClient::for_library(&lib_path)?;

    // Ingest a document with an identifier that shares character 2/3-grams (`tokenx9876`) with the
    // query (`tokenx9876beta`), but has ZERO exact FTS5 lexical token overlap (`tokenx9876alpha`).
    let doc_line = json!({
        "source": {
            "kind": "doc",
            "uri": "doc://ops/tokenx",
            "external_id": "doc-tokenx-1"
        },
        "speaker": "alice",
        "audience": {"explicit": ["alice"]},
        "occurred_at": at.0,
        "content": "部署标记 tokenx9876alpha 已写入审计日志",
        "model_policy": {"mode": "raw_only"},
        "op": "upsert"
    })
    .to_string();
    let _ = client
        .ingest_jsonl(kp_net::IngestJsonlRequest {
            jsonl: doc_line,
            actor: Some("alice".parse()?),
            at: Some(at),
            extract_now: false,
        })
        .await?;

    // Under model A = `none`, `QueryContext("tokenx9876beta")` has 0 FTS5 overlap and 0 vectors.
    let ctx_a = client
        .query_context(kp_net::QueryContextRequest {
            prompt: "tokenx9876beta".into(),
            viewer: "alice".parse()?,
            at: EpochMs(at.0 + 1_000),
            max_tokens: 400,
            max_items: 5,
            time_zone: None,
        })
        .await?;
    assert!(
        !ctx_a.additional_context.contains("tokenx9876alpha"),
        "FTS5-only mode must not match distinct whole-word token before vector migration"
    );

    // 2. External migration A (`none`, 0d) -> B (`hash:v1`, 256d), simulating `kp embed --stale --to hash:v1`.
    {
        let mut ext_store = Store::open(&lib_path)?;
        ext_store.reembed_stale(&kp_store::HashEmbedder, Some(("hash:v1", 256)))?;
    }

    // Immediately query via `DaemonClient::query_context` (no intermediate write or restart!):
    // `refresh_store_embedder` replaces the old `"none"` embedder on the daemon's read-side `Store`
    // with `"hash:v1"`, hitting the semantic vector for `tokenx9876alpha`.
    let ctx_b = client
        .query_context(kp_net::QueryContextRequest {
            prompt: "tokenx9876beta".into(),
            viewer: "alice".parse()?,
            at: EpochMs(at.0 + 2_000),
            max_tokens: 400,
            max_items: 5,
            time_zone: None,
        })
        .await?;
    assert!(
        ctx_b.additional_context.contains("tokenx9876alpha"),
        "expected immediate vector hit under migrated model B (`hash:v1`), got: {:?}",
        ctx_b.additional_context
    );
    let st_b = client.status().await?;
    assert_eq!(st_b.embedder.model, "hash:v1");
    assert_eq!(st_b.embedder.state, "ready");

    // 3. Second external migration B (`hash:v1`, 256d) -> C (`none`, 0d) to verify multi-step
    //    `model_id` replacement back to FTS5-only mode.
    {
        let mut ext_store = Store::open(&lib_path)?;
        ext_store.reembed_stale(&kp_store::FtsOnlyEmbedder, Some(("none", 0)))?;
    }
    let st_c = client.status().await?;
    assert_eq!(st_c.embedder.model, "none");
    assert_eq!(st_c.embedder.state, "disabled");
    let ctx_c = client
        .query_context(kp_net::QueryContextRequest {
            prompt: "tokenx9876beta".into(),
            viewer: "alice".parse()?,
            at: EpochMs(at.0 + 3_000),
            max_tokens: 400,
            max_items: 5,
            time_zone: None,
        })
        .await?;
    assert!(!ctx_c.additional_context.contains("tokenx9876alpha"));

    // 4. Third external migration C (`none`, 0d) -> D (`hash:v1`, 256d): read-side `QueryContext`
    //    immediately hits semantic vectors again without restarting the daemon.
    {
        let mut ext_store = Store::open(&lib_path)?;
        ext_store.reembed_stale(&kp_store::HashEmbedder, Some(("hash:v1", 256)))?;
    }
    let ctx_d = client
        .query_context(kp_net::QueryContextRequest {
            prompt: "tokenx9876beta".into(),
            viewer: "alice".parse()?,
            at: EpochMs(at.0 + 4_000),
            max_tokens: 400,
            max_items: 5,
            time_zone: None,
        })
        .await?;
    assert!(ctx_d.additional_context.contains("tokenx9876alpha"));

    handle.shutdown().await?;

    // 5. Also verify over UDS `status()` that a daemon started on an unready/invalid spec
    //    exposes non-empty `embedder.last_error` alongside `fts5_fallback`.
    let diag_path = dir.path().join("lib_diag");
    let diag_store = Store::init_with_embedding(
        &diag_path,
        InitOptions {
            library_id: LibraryId::new("uds-diag-err")?,
            name: "UDS Diag Error".into(),
            mode: LibraryMode::Personal,
            owner: "alice".parse()?,
            created_at: EpochMs(0),
        },
        "unknown-provider:v2",
        512,
    )?;
    let (diag_sw, _) = crate::providers_cli::build_daemon_swappable_embedder(&diag_store);
    let diag_handle = DaemonServer::spawn(
        diag_store,
        DaemonConfig::for_library(&diag_path)?,
        Arc::new(CliDaemonHandler::with_embedder(diag_sw)),
    )
    .await?;
    let diag_status = DaemonClient::for_library(&diag_path)?.status().await?;
    assert_eq!(diag_status.embedder.state, "fts5_fallback");
    assert!(diag_status
        .embedder
        .last_error
        .as_deref()
        .is_some_and(|e| e.contains("unknown-provider")));
    diag_handle.shutdown().await?;
    Ok(())
}
