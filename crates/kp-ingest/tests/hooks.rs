mod support;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use kp_derive::Memory;
use kp_ingest::connect::{connect, CLAUDE_MCP_ALLOWED_TOOLS, CODEX_MCP_ENABLED_TOOLS};
use kp_ingest::hooks::{estimate_tokens, run, run_with_path, MAX_PROMPT_ITEMS, MAX_PROMPT_TOKENS};
use kp_model::{
    format_opaque_id, ActorId, AnchorSpec, AudienceSpec, BitemporalStamp, BroaderScopeCandidate,
    EpochMs, Fact, FactAccess, FactId, FactKind, FactRank, FactStatus, GovernedScope, MemberRole,
    SourceScope, SourceScopeId, TrustTier, ValidityWindow,
};
use kp_store::FactInput;
use serde_json::{json, Value};
use support::{library, options, Result};

#[test]
fn both_clients_merge_hooks_idempotently_configure_async_and_auto_approve_mcp() -> Result {
    for (client, file) in [
        ("codex", ".codex/hooks.json"),
        ("claude-code", ".claude/settings.json"),
    ] {
        let (dir, _) = library()?;
        let project = dir.path().join("project ' $(touch nope)");
        let path = project.join(file);
        std::fs::create_dir_all(path.parent().ok_or("parent")?)?;
        let original = if client == "claude-code" {
            json!({
                "theme": "dark",
                "permissions": {
                    "allow": ["Bash(cargo test:*)", "mcp__kp__kp_dream"],
                    "deny": ["Bash(rm -rf:*)"]
                },
                "hooks": {
                    "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "echo custom"}]}],
                    "CustomEvent": [{"hooks": [{"type": "command", "command": "echo preserved"}]}]
                }
            })
        } else {
            std::fs::write(
                project.join(".codex/config.toml"),
                "model = \"gpt-5-codex\"\n\n[mcp_servers.other]\ncommand = \"other-mcp\"\n",
            )?;
            json!({
                "theme": "dark",
                "hooks": {
                    "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "echo custom"}]}],
                    "CustomEvent": [{"hooks": [{"type": "command", "command": "echo preserved"}]}]
                }
            })
        };
        std::fs::write(&path, serde_json::to_vec(&original)?)?;
        let actor = "alice".parse()?;
        connect(
            client,
            &project,
            dir.path(),
            &actor,
            std::path::Path::new("/path with ' quote/kp"),
        )?;
        let first = std::fs::read(&path)?;
        connect(
            client,
            &project,
            dir.path(),
            &actor,
            std::path::Path::new("/path with ' quote/kp"),
        )?;
        assert_eq!(first, std::fs::read(&path)?);

        let value: Value = serde_json::from_slice(&first)?;
        assert_eq!(value["theme"], "dark");
        assert_eq!(
            value["hooks"]["CustomEvent"],
            original["hooks"]["CustomEvent"]
        );
        assert_eq!(
            value["hooks"]["PreToolUse"][0],
            original["hooks"]["PreToolUse"][0]
        );
        assert!(value["hooks"]["SessionEnd"].is_array());
        assert!(String::from_utf8(first)?.contains("'\\\"'\\\"'"));

        if client == "claude-code" {
            for async_wire in ["PostToolUse", "PostToolUseFailure", "Stop", "SessionEnd"] {
                let entries = value["hooks"][async_wire].as_array().ok_or("async array")?;
                let last = entries.last().ok_or("last hook")?;
                assert_eq!(last["hooks"][0]["async"], json!(true), "{async_wire}");
            }
            for sync_wire in ["SessionStart", "UserPromptSubmit"] {
                let entries = value["hooks"][sync_wire].as_array().ok_or("sync array")?;
                let last = entries.last().ok_or("last hook")?;
                assert!(last["hooks"][0].get("async").is_none(), "{sync_wire}");
            }
            let allow = value["permissions"]["allow"]
                .as_array()
                .ok_or("permissions.allow")?;
            assert!(allow.contains(&json!("Bash(cargo test:*)")));
            assert!(!allow.contains(&json!("mcp__kp__kp_dream")));
            for tool in CLAUDE_MCP_ALLOWED_TOOLS {
                assert!(allow.contains(&json!(tool)), "missing {tool}");
            }
            assert_eq!(value["permissions"]["deny"], json!(["Bash(rm -rf:*)"]));
        } else {
            let codex_toml_path = project.join(".codex/config.toml");
            let first_toml = std::fs::read_to_string(&codex_toml_path)?;
            connect(
                client,
                &project,
                dir.path(),
                &actor,
                std::path::Path::new("/path with ' quote/kp"),
            )?;
            assert_eq!(first_toml, std::fs::read_to_string(&codex_toml_path)?);
            assert!(first_toml.contains("model = \"gpt-5-codex\""));
            assert!(first_toml.contains("[mcp_servers.other]"));
            assert!(first_toml.contains("[mcp_servers.kp]"));
            assert!(first_toml.contains("default_tools_approval_mode = \"approve\""));
            for tool in CODEX_MCP_ENABLED_TOOLS {
                assert!(first_toml.contains(&format!("\"{tool}\"")));
            }
            let pre_entries = value["hooks"]["PreToolUse"].as_array().ok_or("pre")?;
            let matcher = pre_entries
                .last()
                .and_then(|e| e["matcher"].as_str())
                .ok_or("matcher")?;
            assert!(matcher.contains("apply_patch") && matcher.contains("exec_command"));
        }
    }
    Ok(())
}

#[tokio::test]
async fn session_start_is_zero_injection_and_user_prompt_submit_enforces_budget_provenance_and_echo_stripping(
) -> Result {
    let (dir, mut store) = library()?;
    let actor = "alice".parse()?;
    for idx in 0..6 {
        kp_derive::remember(
            &mut store,
            Memory {
                subject: format!("payment-module-{idx}"),
                predicate: "kp.code.pitfall".into(),
                value: json!(format!(
                    "payment retry rule {idx}: always require idempotency key before settlement {}",
                    "extra context detail ".repeat(12)
                )),
                kind: FactKind::ClosedNegative,
                logical_key: None,
                tags: Default::default(),
                entities: Default::default(),
                valid: ValidityWindow {
                    valid_from: EpochMs(1_795_000_000_000),
                    valid_until: EpochMs::OPEN_END,
                },
            },
            &kp_derive::FunnelOptions {
                at: EpochMs(1_795_000_000_000),
                ..options()?
            },
        )?;
    }

    // SessionStart MUST inject 0 tokens by default (no cwd search, no system prompt bloat).
    let start_out = run_with_path(
        Some(dir.path()),
        &mut store,
        "session-start",
        &json!({"session_id": "s", "prompt": "payment", "cwd": "/repo/payment"}),
        &actor,
        EpochMs(1_795_100_000_000),
    )
    .await?;
    assert_eq!(start_out, json!({}));

    // Unrelated query or low-signal ack clears relevance threshold gate -> empty `{}` (0 tokens).
    for non_matching in ["how to sort a binary tree in haskell", "好的！", "   "] {
        let below = run_with_path(
            Some(dir.path()),
            &mut store,
            "user-prompt-submit",
            &json!({"session_id": "s", "prompt": non_matching}),
            &actor,
            EpochMs(1_795_100_000_000),
        )
        .await?;
        assert_eq!(
            below,
            json!({}),
            "expected zero injection for {non_matching}"
        );
    }

    // Matching query injects <= 3 items, <= 250 tokens, with mandatory [YYYY-MM-DD · speaker @ source · kp-ref:fact_xxx].
    let started = Instant::now();
    let injected = run_with_path(
        Some(dir.path()),
        &mut store,
        "user-prompt-submit",
        &json!({"session_id": "s", "prompt": "payment settlement idempotency"}),
        &actor,
        EpochMs(1_795_100_000_000),
    )
    .await?;
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "hot path took {:?}",
        started.elapsed()
    );
    let context = injected["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .ok_or("context")?;
    assert!(context.starts_with("<kp-context>\n") && context.ends_with("\n</kp-context>"));
    let tokens = estimate_tokens(context);
    assert!(
        tokens <= MAX_PROMPT_TOKENS,
        "exceeded 250-token budget: {tokens}"
    );
    let inner_lines: Vec<&str> = context
        .lines()
        .filter(|l| *l != "<kp-context>" && *l != "</kp-context>")
        .collect();
    assert!((1..=MAX_PROMPT_ITEMS).contains(&inner_lines.len()));
    for line in &inner_lines {
        assert!(line.starts_with("[20"), "missing ISO date header: {line}");
        assert!(
            line.contains(" · alice @ ") && line.contains(" · kp-ref:fact_"),
            "missing provenance header: {line}"
        );
    }

    // Stop strips both <kp-context> blocks and bare kp-ref lines, skips bare acks, and redacts secrets.
    let transcript = json!({"session_id": "echo-strip-session", "messages": [
        {"role": "user", "content": "Please explain payment"},
        {"role": "assistant", "content": context},
        {"role": "assistant", "content": "A genuine answer"},
        {"role": "user", "content": "好的！"},
        {"role": "user", "content": "thanks, api_key=super-secret-value"}
    ]});
    let result = run_with_path(
        Some(dir.path()),
        &mut store,
        "stop",
        &transcript,
        &actor,
        EpochMs(1_795_100_010_000),
    )
    .await?;
    assert_eq!(result["episodes"], 0);
    assert_eq!(result["self_echo_filtered"], true);
    assert_eq!(result["skipped_low_signal"], 1);
    let id = result["source"]["source_id"].as_str().ok_or("source id")?;
    let raw = store
        .raw_source_text(&id.parse()?)?
        .ok_or("missing raw_blobs source")?;
    assert!(
        !raw.contains("kp-ref:")
            && !raw.contains("<kp-context>")
            && !raw.contains("super-secret-value")
            && !raw.contains("idempotency")
            && !raw.contains("好的")
    );
    assert!(raw.contains("A genuine answer") && raw.contains("[REDACTED]"));
    assert_eq!(
        run_with_path(
            Some(dir.path()),
            &mut store,
            "stop",
            &transcript,
            &actor,
            EpochMs(1_795_100_011_000)
        )
        .await?["duplicate"],
        true
    );

    store.add_member("carol".parse()?, MemberRole::Member, EpochMs(0))?;
    assert_eq!(
        run_with_path(
            Some(dir.path()),
            &mut store,
            "user-prompt-submit",
            &json!({"prompt": "payment"}),
            &"carol".parse()?,
            EpochMs(1_795_100_020_000)
        )
        .await?,
        json!({})
    );
    Ok(())
}

#[tokio::test]
async fn serendipitous_inline_review_hint_only_appears_for_fact_author() -> Result {
    let (dir, mut store) = library()?;
    store.add_member("bob".parse()?, MemberRole::Member, EpochMs(0))?;
    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;
    let fid = FactId::new(format_opaque_id("fact", "fitness_ambiguous_01")?)?;
    let fact = Fact {
        id: fid.clone(),
        origin_library: store.library_id().clone(),
        logical_key: fid.as_str().parse()?,
        kind: FactKind::Constraint,
        subject: "力量训练恢复".into(),
        predicate: "kp.core.constraint".into(),
        object: json!({"text": "大重量深蹲次日不安排长距离间歇跑"}),
        tags: BTreeSet::new(),
        author: bob.clone(),
        ingested_by: None,
        trust_tier: TrustTier::HumanVerbatim,
        access: FactAccess::Base {
            audience: AudienceSpec::Explicit(BTreeSet::from([alice.clone(), bob.clone()])),
            stamp: BitemporalStamp {
                valid: ValidityWindow {
                    valid_from: EpochMs(1_795_000_000_000),
                    valid_until: EpochMs::OPEN_END,
                },
                recorded_at: EpochMs(1_795_000_000_000),
                expired_at: None,
            },
        },
        source_scope_id: None,
        governed_scope: Some(GovernedScope::SelfOnly { actor: bob.clone() }),
        broader_scope: Some(BroaderScopeCandidate {
            narrow_active: GovernedScope::SelfOnly { actor: bob.clone() },
            broader_candidate: GovernedScope::Actors {
                actors: BTreeSet::from([alice.clone(), bob.clone()]),
            },
            question: "只针对你个人课表，还是整个训练营？".into(),
        }),
        status: FactStatus::Active,
        superseded_by: None,
        rank: FactRank::Normal,
        source_anchor: None,
        unverified_gaps: Vec::new(),
        miss_count: 0,
    };
    store.remember(&[FactInput {
        fact,
        entities: BTreeSet::new(),
    }])?;

    let bob_prompt = run_with_path(
        Some(dir.path()),
        &mut store,
        "user-prompt-submit",
        &json!({"prompt": "深蹲次日间歇跑安排"}),
        &bob,
        EpochMs(1_795_000_100_000),
    )
    .await?;
    let bob_ctx = bob_prompt["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .ok_or("bob ctx")?;
    assert!(
        bob_ctx.contains("（待确认：只针对你个人课表，还是整个训练营？ 可用 kp review 确认）"),
        "unexpected bob_ctx: {bob_ctx}"
    );

    let alice_prompt = run_with_path(
        Some(dir.path()),
        &mut store,
        "user-prompt-submit",
        &json!({"prompt": "深蹲次日间歇跑安排"}),
        &alice,
        EpochMs(1_795_000_100_000),
    )
    .await?;
    let alice_ctx = alice_prompt["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .ok_or("alice ctx")?;
    assert!(!alice_ctx.contains("待确认"));
    Ok(())
}

#[tokio::test]
async fn incremental_stop_and_session_end_track_cursor_detect_rewrite_and_never_duplicate_turns(
) -> Result {
    let (dir, mut store) = library()?;
    let actor = "alice".parse()?;
    let transcript = dir.path().join("long-session.jsonl");

    // Stop #1: 2 turns
    let line1 = json!({"type":"user","message":{"role":"user","content":"First turn alphaunique"}})
        .to_string();
    let line2 = json!({"type":"assistant","message":{"role":"assistant","content":"<kp-context>\n[2026-11-21 · alice @ local · kp-ref:fact_1] injected\n</kp-context>\nReply betaunique"}}).to_string();
    let batch1 = format!("{line1}\n{line2}\n");
    std::fs::write(&transcript, &batch1)?;

    let stop1 = run_with_path(
        Some(dir.path()),
        &mut store,
        "stop",
        &json!({"session_id": "inc-1", "transcript_path": transcript}),
        &actor,
        EpochMs(100),
    )
    .await?;
    assert_eq!(stop1["captured"], true);
    assert_eq!(stop1["self_echo_filtered"], true);
    let src1 = stop1["source"]["source_id"].as_str().ok_or("src1")?;
    let raw1 = store
        .raw_source_text(&src1.parse()?)?
        .ok_or("missing raw1")?;
    assert!(raw1.contains("alphaunique") && raw1.contains("betaunique"));
    assert!(!raw1.contains("injected"));

    // Stop #2: append 1 low-signal ack + 1 new substantive turn -> ONLY delta after last_byte_offset is captured!
    let line3_ack = json!({"type":"user","message":{"role":"user","content":"收到！"}}).to_string();
    let line4_new = json!({"type":"assistant","message":{"role":"assistant","content":"Second delta gammaunique"}}).to_string();
    let batch2 = format!("{batch1}{line3_ack}\n{line4_new}\n");
    std::fs::write(&transcript, &batch2)?;

    let stop2 = run_with_path(
        Some(dir.path()),
        &mut store,
        "stop",
        &json!({"session_id": "inc-1", "transcript_path": transcript}),
        &actor,
        EpochMs(200),
    )
    .await?;
    assert_eq!(stop2["captured"], true);
    assert_eq!(stop2["skipped_low_signal"], 1);
    let src2 = stop2["source"]["source_id"].as_str().ok_or("src2")?;
    let raw2 = store
        .raw_source_text(&src2.parse()?)?
        .ok_or("missing raw2")?;
    assert!(
        !raw2.contains("alphaunique") && !raw2.contains("betaunique"),
        "earlier turns were duplicated into delta source: {raw2}"
    );
    assert!(raw2.contains("gammaunique"));

    // SessionEnd #3: append final turn -> updates last_extracted_offset and enqueues high-priority extraction job
    let line5_end =
        json!({"type":"user","message":{"role":"user","content":"Final turn deltaunique"}})
            .to_string();
    let batch3 = format!("{batch2}{line5_end}\n");
    std::fs::write(&transcript, &batch3)?;

    let end_res = run_with_path(
        Some(dir.path()),
        &mut store,
        "session-end",
        &json!({"session_id": "inc-1", "transcript_path": transcript}),
        &actor,
        EpochMs(300),
    )
    .await?;
    assert_eq!(end_res["captured"], true);
    let src3 = end_res["source"]["source_id"].as_str().ok_or("src3")?;
    let raw3 = store
        .raw_source_text(&src3.parse()?)?
        .ok_or("missing raw3")?;
    assert!(!raw3.contains("alphaunique") && !raw3.contains("gammaunique"));
    assert!(raw3.contains("deltaunique"));

    let cursor = store.get_session_cursor("inc-1")?.ok_or("cursor")?;
    assert_eq!(cursor.last_byte_offset, u64::try_from(batch3.len())?);
    assert_eq!(cursor.last_line_count, 5);
    assert_eq!(cursor.last_extracted_offset, cursor.last_byte_offset);

    let leased_high_prio = store
        .lease_next_extraction_job(EpochMs(300), 60_000)?
        .ok_or("high-priority job")?;
    assert_eq!(leased_high_prio.priority, 10);
    assert_eq!(leased_high_prio.source_id.as_str(), src3);

    // File rewrite detection via prefix_sha256: overwrite file with different content of larger or smaller length
    let rewritten = format!(
        "{}\n",
        json!({"type":"user","message":{"role":"user","content":"Rewritten session omegaunique"}})
    );
    std::fs::write(&transcript, &rewritten)?;
    let after_rewrite = run_with_path(
        Some(dir.path()),
        &mut store,
        "stop",
        &json!({"session_id": "inc-1", "transcript_path": transcript}),
        &actor,
        EpochMs(400),
    )
    .await?;
    assert_eq!(after_rewrite["captured"], true);
    let src4 = after_rewrite["source"]["source_id"]
        .as_str()
        .ok_or("src4")?;
    let raw4 = store
        .raw_source_text(&src4.parse()?)?
        .ok_or("missing raw4")?;
    assert!(raw4.contains("omegaunique"));
    Ok(())
}

#[tokio::test]
async fn tool_recovery_is_persistent_scoped_and_consumed_once() -> Result {
    let (dir, mut store) = library()?;
    let actor = "alice".parse()?;
    let failure = json!({"session_id":"session","tool_name":"Bash","tool_use_id":"step-1","tool_input":{"command":"cargo test"},"tool_response":{"exit_code":1,"stderr":"api_key=secret-value"}});
    assert_eq!(
        run(&mut store, "post-tool-use", &failure, &actor, EpochMs(20)).await?["episodes"],
        0
    );
    drop(store);
    let mut store = kp_store::Store::open(dir.path())?;
    let mut success = failure.clone();
    success["tool_response"] = json!({"exit_code":0});
    success["tool_use_id"] = json!("step-2");
    success["session_id"] = json!("other-session");
    assert_eq!(
        run(&mut store, "post-tool-use", &success, &actor, EpochMs(21)).await?["episodes"],
        0
    );
    success["session_id"] = json!("session");
    assert_eq!(
        run(&mut store, "post-tool-use", &success, &actor, EpochMs(22)).await?["episodes"],
        1
    );
    assert_eq!(
        run(&mut store, "post-tool-use", &success, &actor, EpochMs(23)).await?["episodes"],
        0
    );
    let pending = store.pending_episodes(&actor)?;
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].episode.signal_kind.as_str(),
        "failure_to_recovery"
    );
    let before = pending[0].episode.before_trace.to_string();
    assert!(!before.contains("secret-value"));
    assert!(before.contains("step-1"));
    Ok(())
}

#[tokio::test]
async fn sniffer_requires_a_grounded_contrast() -> Result {
    let (_dir, mut store) = library()?;
    let cases = [
        (
            json!([{"role":"user","content":"What is Rust?"},{"role":"assistant","content":"A language."}]),
            None,
        ),
        (
            json!([{"role":"assistant","content":"Would you like details?"},{"role":"user","content":"No, thanks."}]),
            None,
        ),
        (
            json!([{"role":"assistant","content":"Use retries."},{"role":"user","content":"No, use idempotency."}]),
            Some("human_correction"),
        ),
        (
            json!([{"role":"assistant","content":"failed","target":"task","success":false,"actor":"one"},{"role":"assistant","content":"passed","target":"task","success":true,"actor":"two"}]),
            None,
        ),
        (
            json!([{"role":"tool","content":"failed","target":"task","success":false},{"role":"tool","content":"passed","target":"task","success":true}]),
            Some("failure_to_recovery"),
        ),
    ];
    let actor: ActorId = "alice".parse()?;
    for (index, (messages, expected)) in cases.into_iter().enumerate() {
        let session = format!("case-{index}");
        let result = run(
            &mut store,
            "stop",
            &json!({"session_id":session,"messages":messages}),
            &actor,
            EpochMs(20),
        )
        .await?;
        assert_eq!(result["episodes"], usize::from(expected.is_some()));
        if let Some(expected) = expected {
            let ep = store
                .pending_episodes(&actor)?
                .into_iter()
                .find(|p| p.episode.session_id == session)
                .ok_or("episode")?;
            assert_eq!(ep.episode.signal_kind.as_str(), expected);
            assert_eq!(ep.episode.step_ref, "turn:1");
        }
    }
    Ok(())
}

#[tokio::test]
async fn claude_failure_event_and_success_event_form_a_recovery() -> Result {
    let (_dir, mut store) = library()?;
    let actor = "alice".parse()?;
    assert_eq!(run(&mut store,"post-tool-use",&json!({"session_id":"s","hook_event_name":"PostToolUseFailure","tool_name":"Write","tool_input":{"file_path":"src/pay.rs"},"error":"permission denied","tool_use_id":"failed-step"}),&actor,EpochMs(20)).await?["episodes"],0);
    assert_eq!(run(&mut store,"post-tool-use",&json!({"session_id":"s","hook_event_name":"PostToolUse","tool_name":"Write","tool_input":{"file_path":"src/pay.rs"},"tool_response":{"filePath":"src/pay.rs","type":"update"},"tool_use_id":"recovered-step"}),&actor,EpochMs(21)).await?["episodes"],1);
    Ok(())
}

#[tokio::test]
async fn pre_tool_is_silent_by_default_and_emits_ask_for_owner_guards_and_policy_rules_across_clients(
) -> Result {
    let (dir, mut store) = library()?;
    store.add_member("bob".parse()?, MemberRole::Member, EpochMs(0))?;
    store.add_member("pm_li".parse()?, MemberRole::Member, EpochMs(0))?;
    let mut make = |author: &str, value: Value, audience: &[&str]| -> Result {
        let options = kp_derive::FunnelOptions {
            actor: author.parse()?,
            audience: kp_model::AudienceSpec::Explicit(
                audience
                    .iter()
                    .map(|a| a.parse())
                    .collect::<kp_model::Result<_>>()?,
            ),
            at: EpochMs(1_795_000_000_000),
        };
        kp_derive::remember(
            &mut store,
            Memory {
                subject: "src/pay.rs".into(),
                predicate: "kp.core.constraint".into(),
                value,
                kind: FactKind::Constraint,
                logical_key: None,
                tags: Default::default(),
                entities: Default::default(),
                valid: ValidityWindow {
                    valid_from: EpochMs(0),
                    valid_until: EpochMs::OPEN_END,
                },
            },
            &options,
        )?;
        Ok(())
    };
    make(
        "bob",
        json!({"hard_guard":true,"reason":"member cannot block"}),
        &["alice", "bob"],
    )?;
    make(
        "alice",
        json!({"hard_guard":true,"reason":"private guard on payment module"}),
        &["alice"],
    )?;

    let claude_edit = json!({"tool_name":"Edit","tool_input":{"file_path":"src/pay.rs"}});
    let bob = run_with_path(
        Some(dir.path()),
        &mut store,
        "pre-tool-use",
        &claude_edit,
        &"bob".parse()?,
        EpochMs(1_795_000_010_000),
    )
    .await?;
    assert_eq!(bob, json!({}));
    assert!(!bob.to_string().contains("private guard"));

    // Claude Code `Edit` emits `permissionDecision: "ask"`, whereas Codex `apply_patch` emits
    // `permissionDecision: "deny"` (real Codex 0.164 rejects `"ask"`) with `kp allow` guidance!
    let codex_patch = json!({
        "tool_name": "apply_patch",
        "tool_input": {"patch": "*** Begin Patch\n*** Update File: src/pay.rs\n@@\n-old\n+new\n*** End Patch"}
    });
    for (payload, expected_decision) in [(&claude_edit, "ask"), (&codex_patch, "deny")] {
        let alice = run_with_path(
            Some(dir.path()),
            &mut store,
            "pre-tool-use",
            payload,
            &"alice".parse()?,
            EpochMs(1_795_000_010_000),
        )
        .await?;
        assert_eq!(
            alice["hookSpecificOutput"]["hookEventName"],
            json!("PreToolUse")
        );
        assert_eq!(
            alice["hookSpecificOutput"]["permissionDecision"],
            json!(expected_decision)
        );
        let reason = alice["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .ok_or("reason")?;
        assert!(reason.contains("private guard on payment module"));
        assert!(reason.contains("kp-ref:fact_"));
        if expected_decision == "deny" {
            assert!(
                reason.contains("先问用户；用户同意后，请用户执行 kp allow fact_")
                    && reason.ends_with('。'),
                "missing Codex kp allow guidance: {reason}"
            );
        }
    }

    // Unrelated file stays silent `{}`.
    let unrelated = json!({"tool_name":"Edit","tool_input":{"file_path":"src/unrelated.rs"}});
    assert_eq!(
        run_with_path(
            Some(dir.path()),
            &mut store,
            "pre-tool-use",
            &unrelated,
            &"alice".parse()?,
            EpochMs(1_795_000_010_000)
        )
        .await?,
        json!({})
    );

    // Live Feishu source_scope database-failover maintenance constraint triggers `ask` on `sqlx migrate`
    // across Claude Code & Codex without hand-stuffed `command` keys, and goes silent once withdrawn!
    let scope_id: SourceScopeId = "feishu.chat.oc_dba".parse()?;
    store.upsert_source_scope(&SourceScope {
        scope_id: scope_id.clone(),
        provider: "feishu".into(),
        external_container_id: "oc_dba".into(),
        label: "飞书DBA运维群".into(),
        current_audience: AudienceSpec::Explicit(BTreeSet::from([
            "alice".parse()?,
            "pm_li".parse()?,
        ])),
        synced_at: EpochMs(1_795_000_000_000),
    })?;
    let maint_id = FactId::new(format_opaque_id("fact", "db_failover_window_01")?)?;
    store.remember(&[FactInput {
        fact: Fact {
            id: maint_id.clone(),
            origin_library: store.library_id().clone(),
            logical_key: maint_id.as_str().parse()?,
            kind: FactKind::Constraint,
            subject: "general".into(),
            predicate: "kp.policy.constraint".into(),
            object: json!({"text": "周四凌晨主从切换维护窗口内禁止执行数据库迁移"}),
            tags: BTreeSet::new(),
            author: "pm_li".parse()?,
            ingested_by: Some("alice".parse()?),
            trust_tier: TrustTier::HumanVerbatim,
            access: FactAccess::Base {
                audience: AudienceSpec::Explicit(BTreeSet::from([
                    "alice".parse()?,
                    "pm_li".parse()?,
                ])),
                stamp: BitemporalStamp {
                    valid: ValidityWindow {
                        valid_from: EpochMs(1_795_000_000_000),
                        valid_until: EpochMs(1_795_600_000_000),
                    },
                    recorded_at: EpochMs(1_795_000_000_000),
                    expired_at: None,
                },
            },
            source_scope_id: Some(scope_id),
            governed_scope: None,
            broader_scope: None,
            status: FactStatus::Active,
            superseded_by: None,
            rank: FactRank::Normal,
            source_anchor: Some(AnchorSpec {
                verbatim_quote: Some("周四凌晨主从切换维护窗口内禁止执行数据库迁移".into()),
                ..AnchorSpec::default()
            }),
            unverified_gaps: Vec::new(),
            miss_count: 0,
        },
        entities: BTreeSet::new(),
    }])?;

    let claude_migrate = json!({"tool_name": "Bash", "tool_input": {"command": "sqlx migrate run --database-url postgres://local"}});
    // Real Codex 0.164 `PreToolUse` Bash payload (includes `turn_id` + `.codex/sessions/rollout-...`) + native Codex tools.
    let real_codex_bash = json!({
        "session_id": "0195f0c0-1111-7000-8000-000000000001",
        "turn_id": "turn_01",
        "transcript_path": "/tmp/.codex/sessions/2026/11/21/rollout-2026-11-21T16-00-00.jsonl",
        "cwd": "/repo",
        "hook_event_name": "PreToolUse",
        "model": "gpt-5-codex",
        "permission_mode": "default",
        "tool_name": "Bash",
        "tool_input": {"command": "sqlx migrate run"},
        "tool_use_id": "call_01"
    });
    let codex_exec =
        json!({"tool_name": "exec_command", "tool_input": {"cmd": "sqlx migrate run"}});
    let codex_shell =
        json!({"tool_name": "shell", "tool_input": {"command": ["sqlx", "migrate", "run"]}});

    for (migrate_payload, expected_decision) in [
        (&claude_migrate, "ask"),
        (&real_codex_bash, "deny"),
        (&codex_exec, "deny"),
        (&codex_shell, "deny"),
    ] {
        let out = run_with_path(
            Some(dir.path()),
            &mut store,
            "pre-tool-use",
            migrate_payload,
            &"alice".parse()?,
            EpochMs(1_795_100_000_000),
        )
        .await?;
        assert_eq!(
            out["hookSpecificOutput"]["permissionDecision"],
            expected_decision
        );
        let reason = out["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .ok_or("maint reason")?;
        assert!(!reason.trim().is_empty());
        assert!(reason.contains("周四凌晨主从切换维护窗口内禁止执行数据库迁移"));
        assert!(reason.contains("pm_li @ 飞书DBA运维群"));
        assert!(reason.contains(&format!("kp-ref:{maint_id}")));
        if expected_decision == "deny" {
            assert!(reason.contains(&format!(
                "先问用户；用户同意后，请用户执行 kp allow {maint_id}。"
            )));
        }
    }

    // Withdraw maintenance rule -> silent `{}`!
    store.forget_fact_id(&maint_id, &"alice".parse()?, EpochMs(1_795_200_000_000))?;
    assert_eq!(
        run_with_path(
            Some(dir.path()),
            &mut store,
            "pre-tool-use",
            &json!({"tool_name": "Bash", "tool_input": {"command": "sqlx migrate run"}}),
            &"alice".parse()?,
            EpochMs(1_795_200_001_000)
        )
        .await?,
        json!({})
    );

    // Owner `alice`'s shared-readable `GovernedScope::SelfOnly(alice)` rule with actor `subject = "bob"`
    // MUST NOT interrupt `bob` or false-positive `/workspace/bob/repo/src/unrelated.rs`!
    let self_only_id = FactId::new(format_opaque_id("fact", "alice_self_only_guard")?)?;
    store.remember(&[FactInput {
        fact: Fact {
            id: self_only_id.clone(),
            origin_library: store.library_id().clone(),
            logical_key: self_only_id.as_str().parse()?,
            kind: FactKind::Constraint,
            subject: "bob".into(),
            predicate: "kp.core.constraint".into(),
            object: json!({"text": "我自己不直接修改 Cargo.toml"}),
            tags: BTreeSet::new(),
            author: "alice".parse()?,
            ingested_by: None,
            trust_tier: TrustTier::OwnerExplicit,
            access: FactAccess::Base {
                audience: AudienceSpec::Explicit(BTreeSet::from([
                    "alice".parse()?,
                    "bob".parse()?,
                ])),
                stamp: BitemporalStamp {
                    valid: ValidityWindow {
                        valid_from: EpochMs(1_795_000_000_000),
                        valid_until: EpochMs::OPEN_END,
                    },
                    recorded_at: EpochMs(1_795_200_010_000),
                    expired_at: None,
                },
            },
            source_scope_id: None,
            governed_scope: Some(GovernedScope::SelfOnly {
                actor: "alice".parse()?,
            }),
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
    assert_eq!(
        run_with_path(
            Some(dir.path()),
            &mut store,
            "pre-tool-use",
            &json!({"tool_name": "Edit", "tool_input": {"file_path": "/workspace/bob/repo/Cargo.toml"}}),
            &"bob".parse()?,
            EpochMs(1_795_200_020_000)
        )
        .await?,
        json!({})
    );
    assert_eq!(
        run_with_path(
            Some(dir.path()),
            &mut store,
            "pre-tool-use",
            &json!({"tool_name": "Edit", "tool_input": {"file_path": "/workspace/bob/repo/src/unrelated.rs"}}),
            &"alice".parse()?,
            EpochMs(1_795_200_020_000)
        )
        .await?,
        json!({})
    );
    assert_eq!(
        run_with_path(
            Some(dir.path()),
            &mut store,
            "pre-tool-use",
            &json!({"tool_name": "Edit", "tool_input": {"file_path": "/workspace/bob/repo/Cargo.toml"}}),
            &"alice".parse()?,
            EpochMs(1_795_200_020_000)
        )
        .await?["hookSpecificOutput"]["permissionDecision"],
        json!("ask")
    );
    Ok(())
}

#[tokio::test]
async fn jsonl_transcripts_strip_echoes_inside_claude_and_codex_messages() -> Result {
    let (dir, mut store) = library()?;
    let transcript = dir.path().join("transcript.jsonl");
    let turns = [
        json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"[kp-ref:old] echoed memory\nreal response"}]}}),
        json!({"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"No, use checked arithmetic. token=private-value"}]}}),
    ];
    std::fs::write(
        &transcript,
        turns
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )?;
    let output = run(
        &mut store,
        "stop",
        &json!({"session_id":"jsonl","transcript_path":transcript}),
        &"alice".parse()?,
        EpochMs(20),
    )
    .await?;
    assert_eq!(output["self_echo_filtered"], true);
    assert_eq!(output["episodes"], 1);
    let source = output["source"]["source_id"].as_str().ok_or("source")?;
    let content = store
        .raw_source_text(&source.parse()?)?
        .ok_or("missing raw source")?;
    assert!(!content.contains("echoed memory") && !content.contains("private-value"));
    assert!(content.contains("real response"));
    Ok(())
}

/// Real-machine transcript shapes verified against:
/// 1. `openai/codex` v0.164.0 `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`:
///    - `type="session_meta"`, `type="turn_context"`, `type="response_item"` with `payload.type="reasoning"` / `payload.type="function_call"` / `payload.role="developer"` (all skipped);
///    - `type="response_item"` with `payload.type="message"`, `payload.role="user"` carrying `# AGENTS.md instructions ... <INSTRUCTIONS>...</INSTRUCTIONS>`, `<environment_context>...</environment_context>`, and `<guardian_tool_descriptions>...</guardian_tool_descriptions>` (stripped before ingestion);
///    - `type="response_item"` with `payload.type="agent_message"`, `payload.message="..."` (captured as assistant without needing `payload.role`).
/// 2. Claude Code v2.1.x `~/.claude/projects/*/*.jsonl`:
///    - `"isMeta": true` expanded skill/command injections (skipped completely);
///    - `"type": "user"` carrying `<system-reminder>...</system-reminder>`, `<command-message>...</command-message>`, `<command-name>...</command-name>`, `<local-command-caveat>...</local-command-caveat>` alongside real user speech.
#[tokio::test]
async fn real_codex_0164_and_claude_code_transcript_shapes_strip_system_blocks_skip_is_meta_and_capture_agent_message(
) -> Result {
    let (dir, mut store) = library()?;
    let actor: ActorId = "alice".parse()?;

    let codex_rollout = dir.path().join("rollout-2026-11-21T16-00-00-codex.jsonl");
    let codex_lines = [
        json!({"timestamp":"2026-11-21T08:00:00Z","type":"session_meta","payload":{"id":"sess-codex-0164","cli_version":"0.164.0"}}),
        json!({"timestamp":"2026-11-21T08:00:01Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"<permissions instructions>sandbox workspace-write</permissions instructions>"}]}}),
        json!({"timestamp":"2026-11-21T08:00:02Z","type":"response_item","payload":{"type":"message","role":"user","content":[
            {"type":"input_text","text":"# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>\nNever commit secrets_from_agents_md\n</INSTRUCTIONS>"},
            {"type":"input_text","text":"<environment_context>\n  <cwd>/repo</cwd>\n  <shell>zsh</shell>\n</environment_context>"},
            {"type":"input_text","text":"<guardian_tool_descriptions>\n  guardian_internal_marker\n</guardian_tool_descriptions>"},
            {"type":"input_text","text":"我们把重试上限改成 3 次 codex_real_user_claim"}
        ]}}),
        json!({"timestamp":"2026-11-21T08:00:03Z","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"thinking_internal_only"}]}}),
        json!({"timestamp":"2026-11-21T08:00:04Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo test\"}"}}),
        json!({"timestamp":"2026-11-21T08:00:05Z","type":"response_item","payload":{"type":"agent_message","message":"已将重试上限更新为 3 次 codex_agent_reply_captured"}}),
    ];
    std::fs::write(
        &codex_rollout,
        codex_lines
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )?;

    let codex_out = run_with_path(
        Some(dir.path()),
        &mut store,
        "stop",
        &json!({"session_id": "codex-0164-real", "transcript_path": codex_rollout}),
        &actor,
        EpochMs(1_795_200_000_000),
    )
    .await?;
    assert_eq!(codex_out["captured"], true);
    let codex_src = codex_out["source"]["source_id"]
        .as_str()
        .ok_or("codex_src")?;
    let codex_raw = store
        .raw_source_text(&codex_src.parse()?)?
        .ok_or("missing codex_raw")?;
    assert!(codex_raw.contains("codex_real_user_claim"), "{codex_raw}");
    assert!(
        codex_raw.contains("codex_agent_reply_captured"),
        "{codex_raw}"
    );
    for forbidden in [
        "secrets_from_agents_md",
        "<environment_context>",
        "guardian_internal_marker",
        "sandbox workspace-write",
        "thinking_internal_only",
        "exec_command",
    ] {
        assert!(
            !codex_raw.contains(forbidden),
            "leaked host boilerplate `{forbidden}` into raw source: {codex_raw}"
        );
    }

    let claude_jsonl = dir.path().join(".claude-session.jsonl");
    let claude_lines = [
        json!({"type":"file-history-snapshot","snapshot":{}}),
        json!({"type":"user","isMeta":true,"message":{"role":"user","content":"Expanded slash skill prompt is_meta_secret_prompt"}}),
        json!({"type":"user","message":{"role":"user","content":"<local-command-caveat>Caveat: local command output</local-command-caveat>\n<command-name>/review</command-name>\n<command-message>review</command-message>\n<system-reminder>System reminder boilerplate</system-reminder>\n<CONTEXT_SUMMARY>Compacted old context summary_must_not_leak</CONTEXT_SUMMARY>\n下周四凌晨安排账单库主从切换维护 claude_real_user_claim"}}),
        json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"internal_claude_thinking"},{"type":"text","text":"好的，已记录主从切换维护窗口 claude_assistant_reply"}]}}),
    ];
    std::fs::write(
        &claude_jsonl,
        claude_lines
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )?;

    let claude_out = run_with_path(
        Some(dir.path()),
        &mut store,
        "session-end",
        &json!({"session_id": "claude-21-real", "transcript_path": claude_jsonl}),
        &actor,
        EpochMs(1_795_200_010_000),
    )
    .await?;
    assert_eq!(claude_out["captured"], true);
    let claude_src = claude_out["source"]["source_id"]
        .as_str()
        .ok_or("claude_src")?;
    let claude_raw = store
        .raw_source_text(&claude_src.parse()?)?
        .ok_or("missing claude_raw")?;
    assert!(
        claude_raw
            .contains("[speaker:alice] 下周四凌晨安排账单库主从切换维护 claude_real_user_claim"),
        "{claude_raw}"
    );
    assert!(
        claude_raw.contains("[assistant] 好的，已记录主从切换维护窗口 claude_assistant_reply"),
        "{claude_raw}"
    );
    for forbidden in [
        "is_meta_secret_prompt",
        "System reminder boilerplate",
        "summary_must_not_leak",
        "<command-name>",
        "<command-message>",
        "Caveat: local command output",
        "internal_claude_thinking",
    ] {
        assert!(
            !claude_raw.contains(forbidden),
            "leaked Claude meta/boilerplate `{forbidden}` into raw source: {claude_raw}"
        );
    }
    Ok(())
}

#[test]
fn connect_recognizes_literal_shell_argv0_without_confusing_hook_in_paths() -> Result {
    for client in ["claude-code", "codex"] {
        let (dir, _) = library()?;
        let project = tempfile::tempdir()?;
        let actor = "alice".parse()?;
        let executable = std::path::Path::new("/tmp/my hook dir/with ' quotes/kp");
        let path = connect(client, project.path(), dir.path(), &actor, executable)?;
        let mut config: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        let commands = [
            "'/tmp/my hook dir/kp' hook stop --path old",
            "\"/tmp/my hook dir/kp\" hook stop --path old",
            "kp hook stop --path old",
            "/usr/local/bin/kp hook stop --path old",
            "/tmp/my\\ hook\\ dir/kp hook stop --path old",
            "'kp'\t'hook'\t\"stop\"",
            "echo 'kp hook stop --path keep'",
            "'/tmp/my hook dir/not-kp' hook stop --path keep",
            "kp hook stopper --path keep",
            "\"/tmp/$(echo kp)/kp\" hook stop --path keep",
            "'/tmp/unclosed/kp hook stop --path keep",
        ];
        config["hooks"]["Stop"] = json!([{"hooks":commands.iter().map(|command| json!({"type":"command","command":command})).collect::<Vec<_>>()}]);
        std::fs::write(&path, serde_json::to_vec(&config)?)?;
        connect(client, project.path(), dir.path(), &actor, executable)?;
        let first = std::fs::read(&path)?;
        connect(client, project.path(), dir.path(), &actor, executable)?;
        assert_eq!(first, std::fs::read(&path)?);
        let config: Value = serde_json::from_slice(&first)?;
        let remaining = config["hooks"]["Stop"]
            .as_array()
            .ok_or("stop hooks")?
            .iter()
            .flat_map(|entry| entry["hooks"].as_array().into_iter().flatten())
            .filter_map(|hook| hook["command"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(remaining.len(), 6);
        for command in &commands[6..] {
            assert!(
                remaining.contains(command),
                "lost unrelated hook: {command}"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn empty_and_whitespace_tags_do_not_match_unrelated_tool_targets() -> Result {
    let (_dir, mut store) = library()?;
    for tag in ["", " ", "\t"] {
        kp_derive::remember(
            &mut store,
            Memory {
                subject: "src/protected.rs".into(),
                predicate: "kp.core.constraint".into(),
                value: json!({"hard_guard":true,"reason":"protected file"}),
                kind: FactKind::Constraint,
                logical_key: None,
                tags: [tag.to_owned()].into(),
                entities: Default::default(),
                valid: ValidityWindow {
                    valid_from: EpochMs(0),
                    valid_until: EpochMs::OPEN_END,
                },
            },
            &options()?,
        )?;
    }
    let actor = "alice".parse()?;
    for input in [
        json!({"file_path":"src/unrelated.rs"}),
        json!({"command":"echo \t unrelated"}),
    ] {
        let response = run(
            &mut store,
            "pre-tool-use",
            &json!({"tool_name":"Bash","tool_input":input}),
            &actor,
            EpochMs(20),
        )
        .await?;
        assert_eq!(response, json!({}));
    }
    let related = run(
        &mut store,
        "pre-tool-use",
        &json!({"tool_name":"Edit","tool_input":{"file_path":"src/protected.rs"}}),
        &actor,
        EpochMs(20),
    )
    .await?;
    assert_eq!(related["hookSpecificOutput"]["permissionDecision"], "ask");
    Ok(())
}

#[tokio::test]
async fn post_tool_use_checks_top_level_failure_before_the_event_default() -> Result {
    let (_dir, mut store) = library()?;
    let actor = "alice".parse()?;
    for (index, failure) in [json!({"error":"permission denied"}), json!({"exit_code":1})]
        .into_iter()
        .enumerate()
    {
        let mut payload = json!({"session_id":format!("failure-{index}"),"hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{"command":"cargo test"},"tool_use_id":"failed"});
        for (key, value) in failure.as_object().ok_or("failure fields")? {
            payload[key] = value.clone();
        }
        assert_eq!(
            run(&mut store, "post-tool-use", &payload, &actor, EpochMs(20)).await?["episodes"],
            0
        );
        payload.as_object_mut().ok_or("payload")?.remove("error");
        payload
            .as_object_mut()
            .ok_or("payload")?
            .remove("exit_code");
        payload["tool_use_id"] = json!("recovered");
        assert_eq!(
            run(&mut store, "post-tool-use", &payload, &actor, EpochMs(21)).await?["episodes"],
            1
        );
    }
    Ok(())
}

#[tokio::test]
async fn duplicate_stop_responses_always_report_whether_echoes_were_filtered() -> Result {
    let (_dir, mut store) = library()?;
    let actor = "alice".parse()?;
    for (session, content, filtered) in [
        ("with-echo", "[kp-ref:old] echo\noriginal response", true),
        ("without-echo", "original response", false),
    ] {
        let payload =
            json!({"session_id":session,"messages":[{"role":"assistant","content":content}]});
        let first = run(&mut store, "stop", &payload, &actor, EpochMs(20)).await?;
        assert_eq!(first["self_echo_filtered"], filtered);
        let duplicate = run(&mut store, "stop", &payload, &actor, EpochMs(21)).await?;
        assert_eq!(duplicate["duplicate"], true);
        assert_eq!(duplicate["self_echo_filtered"], filtered);
    }
    Ok(())
}

#[test]
fn shanghai_early_morning_and_cross_midnight_session_turns_preserve_exact_local_reference_dates(
) -> Result {
    let shanghai = kp_model::TimeZone::get("Asia/Shanghai")?;
    let ts_2026_11_23_0000 =
        kp_model::parse_timestamp_ms_in_tz("2026-11-23T00:00:00+08:00", &shanghai)
            .ok_or("2026-11-23 00:00")?;
    let ts_2026_11_24_0000 =
        kp_model::parse_timestamp_ms_in_tz("2026-11-24T00:00:00+08:00", &shanghai)
            .ok_or("2026-11-24 00:00")?;
    let ts_2026_11_25_0000 =
        kp_model::parse_timestamp_ms_in_tz("2026-11-25T00:00:00+08:00", &shanghai)
            .ok_or("2026-11-25 00:00")?;

    // Case 1: Shanghai 2026-11-22 07:00 +08:00 (= 2026-11-21T23:00:00Z in UTC!).
    // "明天" MUST resolve to 2026-11-23 00:00..2026-11-24 00:00 +08:00 (not 2026-11-22 UTC!).
    let (dir1, mut store1) = library()?;
    let actor: kp_model::ActorId = "alice".parse()?;
    let tp1 = dir1.path().join("early-morning.jsonl");
    std::fs::write(
        &tp1,
        "{\"type\":\"user\",\"timestamp\":\"2026-11-22T07:00:00+08:00\",\"message\":{\"role\":\"user\",\"content\":\"明天下午三点做架构复盘\"}}\n",
    )?;
    let hook_at_0705 = kp_model::parse_timestamp_ms_in_tz("2026-11-22T07:05:00+08:00", &shanghai)
        .ok_or("07:05")?;
    let res1 = kp_ingest::hooks::capture_session_in_tz(
        &mut store1,
        &json!({"session_id": "s-morning", "transcript_path": tp1}),
        &actor,
        hook_at_0705,
        true,
        &shanghai,
    )?;
    assert_eq!(res1["captured"], true);
    let job1 = store1
        .lease_next_extraction_job(hook_at_0705, 60_000)?
        .ok_or("expected job1")?;
    let cal1 = kp_model::CalendarTable::from_reference_in_tz(job1.reference_time, &shanghai)?;
    assert_eq!(
        cal1.day_window_for_token("今天")?.valid_from,
        kp_model::parse_timestamp_ms_in_tz("2026-11-22T00:00:00+08:00", &shanghai)
            .ok_or("11-22")?
    );
    let tomorrow1 = cal1.day_window_for_token("明天")?;
    assert_eq!(tomorrow1.valid_from, ts_2026_11_23_0000);
    assert_eq!(tomorrow1.valid_until, ts_2026_11_24_0000);

    // Case 2a: Human turn at 23:50 +08:00, processed after midnight by `SessionEnd` at 00:10 +08:00.
    // `reference_time` MUST stay anchored to 23:50 (2026-11-22), so "明天" still means 2026-11-23!
    let (dir2, mut store2) = library()?;
    let tp2 = dir2.path().join("late-night-single.jsonl");
    std::fs::write(
        &tp2,
        "{\"type\":\"user\",\"timestamp\":\"2026-11-22T23:50:00+08:00\",\"message\":{\"role\":\"user\",\"content\":\"明天下午三点做架构复盘\"}}\n",
    )?;
    let hook_at_0010 = kp_model::parse_timestamp_ms_in_tz("2026-11-23T00:10:00+08:00", &shanghai)
        .ok_or("00:10")?;
    let bytes2 = std::fs::metadata(&tp2)?.len();
    assert_eq!(
        kp_ingest::hooks::ingest_daemon_session_slice(
            &mut store2,
            &kp_ingest::hooks::DaemonSessionSliceSpec {
                raw_session: "s-late",
                transcript_path: &tp2,
                start_byte_offset: 0,
                end_byte_offset: bytes2,
                actor: &actor,
                at: hook_at_0010,
                is_session_end: true,
                tz: Some(&shanghai),
                global_policy: &kp_model::ModelRoutePolicy::HostCli,
            },
        )?
        .committed_end_byte_offset,
        bytes2
    );
    let job2 = store2
        .lease_next_extraction_job(hook_at_0010, 60_000)?
        .ok_or("expected job2")?;
    let cal2 = kp_model::CalendarTable::from_reference_in_tz(job2.reference_time, &shanghai)?;
    let tomorrow2 = cal2.day_window_for_token("明天")?;
    assert_eq!(tomorrow2.valid_from, ts_2026_11_23_0000);
    assert_eq!(tomorrow2.valid_until, ts_2026_11_24_0000);

    // Case 2b: Same session crosses local 00:00 (23:50 +08:00 AND 00:05 +08:00 in one slice) ->
    // splits into 2 sub-batches so 23:50 resolves "明天" -> 2026-11-23 and 00:05 resolves "明天" -> 2026-11-24!
    let (dir3, mut store3) = library()?;
    let tp3 = dir3.path().join("cross-midnight.jsonl");
    std::fs::write(
        &tp3,
        concat!(
            "{\"type\":\"user\",\"timestamp\":\"2026-11-22T23:50:00+08:00\",\"message\":{\"role\":\"user\",\"content\":\"明天下午三点做架构复盘\"}}\n",
            "{\"type\":\"assistant\",\"timestamp\":\"2026-11-22T23:51:00+08:00\",\"message\":{\"role\":\"assistant\",\"content\":\"好的，已记录复盘安排。\"}}\n",
            "{\"type\":\"user\",\"timestamp\":\"2026-11-23T00:05:00+08:00\",\"message\":{\"role\":\"user\",\"content\":\"明天上午十点核对监控告警\"}}\n",
        ),
    )?;
    let bytes3 = std::fs::metadata(&tp3)?.len();
    assert_eq!(
        kp_ingest::hooks::ingest_daemon_session_slice(
            &mut store3,
            &kp_ingest::hooks::DaemonSessionSliceSpec {
                raw_session: "s-cross",
                transcript_path: &tp3,
                start_byte_offset: 0,
                end_byte_offset: bytes3,
                actor: &actor,
                at: hook_at_0010,
                is_session_end: true,
                tz: Some(&shanghai),
                global_policy: &kp_model::ModelRoutePolicy::HostCli,
            },
        )?
        .committed_end_byte_offset,
        bytes3
    );
    // `extraction_queue` orders same-priority jobs chronologically (`created_at ASC, reference_time ASC`)
    // so the 23:50 sub-batch leases first and the 00:05 sub-batch leases second.
    let job3_older = store3
        .lease_next_extraction_job(hook_at_0010, 60_000)?
        .ok_or("expected older 23:50 sub-batch")?;
    let job3_newer = store3
        .lease_next_extraction_job(hook_at_0010, 60_000)?
        .ok_or("expected newer 00:05 sub-batch")?;
    assert!(store3
        .lease_next_extraction_job(hook_at_0010, 60_000)?
        .is_none());
    let cal3_newer =
        kp_model::CalendarTable::from_reference_in_tz(job3_newer.reference_time, &shanghai)?;
    let cal3_older =
        kp_model::CalendarTable::from_reference_in_tz(job3_older.reference_time, &shanghai)?;
    assert_eq!(
        cal3_older.day_window_for_token("明天")?.valid_from,
        ts_2026_11_23_0000
    );
    assert_eq!(
        cal3_older.day_window_for_token("明天")?.valid_until,
        ts_2026_11_24_0000
    );
    assert_eq!(
        cal3_newer.day_window_for_token("明天")?.valid_from,
        ts_2026_11_24_0000
    );
    assert_eq!(
        cal3_newer.day_window_for_token("明天")?.valid_until,
        ts_2026_11_25_0000
    );
    Ok(())
}

#[test]
fn streaming_over_16_mib_transcript_skips_incomplete_jsonl_tail_and_tracks_monotonic_subbatch_spans(
) -> Result {
    use std::io::Write;

    let shanghai = kp_model::TimeZone::get("Asia/Shanghai")?;
    let (dir, mut store) = library()?;
    let actor: ActorId = "alice".parse()?;
    let tp = dir.path().join("huge-session.jsonl");

    // 1. Write > 17 MiB (> MAX_INPUT_BYTES = 16 MiB) of skipped `turn_context` records + 1 Day-1 turn.
    let pad_payload = "x".repeat(65_536);
    let pad_line = format!(
        "{{\"type\":\"turn_context\",\"payload\":{{\"cwd\":\"/workspace\",\"pad\":\"{pad_payload}\"}}}}\n"
    );
    let day1_line = "{\"type\":\"user\",\"timestamp\":\"2026-11-22T23:50:00+08:00\",\"message\":{\"role\":\"user\",\"content\":\"大体积增量阶段一 alpha17mib\"}}\n";
    {
        let mut f = std::io::BufWriter::new(std::fs::File::create(&tp)?);
        for _ in 0..275 {
            f.write_all(pad_line.as_bytes())?;
        }
        f.write_all(day1_line.as_bytes())?;
        f.flush()?;
    }
    let len_after_day1 = std::fs::metadata(&tp)?.len();
    assert!(
        len_after_day1 > u64::try_from(kp_ingest::MAX_INPUT_BYTES)?,
        "expected >16 MiB transcript, got {len_after_day1}"
    );

    let (inspected_off1, inspected_lines1, inspected_sha1) =
        kp_ingest::hooks::inspect_transcript_cursor_file(&tp)?;
    assert_eq!(inspected_off1, len_after_day1);
    assert_eq!(inspected_lines1, 276);
    assert_eq!(
        kp_store::sha256_file_prefix(&tp, len_after_day1)?.as_deref(),
        Some(inspected_sha1.as_str())
    );

    let stop1 = kp_ingest::hooks::capture_session_in_tz(
        &mut store,
        &json!({"session_id": "huge-inc", "transcript_path": tp}),
        &actor,
        EpochMs(1_000),
        false,
        &shanghai,
    )?;
    assert_eq!(stop1["captured"], true);
    assert_eq!(stop1["cursor_byte_offset"], len_after_day1);
    let src1 = stop1["source"]["source_id"].as_str().ok_or("src1")?;
    let raw1 = store
        .raw_source_text(&src1.parse()?)?
        .ok_or("missing raw1")?;
    assert!(raw1.contains("alpha17mib") && !raw1.contains("[unknown]"));

    // 2. Append a complete Day-2 line PLUS a trailing incomplete half-written JSONL line without `\n`.
    let day2_line = "{\"type\":\"user\",\"timestamp\":\"2026-11-23T00:05:00+08:00\",\"message\":{\"role\":\"user\",\"content\":\"跨零点阶段二 beta17mib\"}}\n";
    let half_prefix = "{\"type\":\"user\",\"timestamp\":\"2026-11-23T00:06:00+08:00\",\"message\":{\"role\":\"user\",\"content\":\"半截尚未刷盘 gamma17mib";
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(&tp)?;
        f.write_all(day2_line.as_bytes())?;
        f.write_all(half_prefix.as_bytes())?;
        f.flush()?;
    }
    let expected_complete_end = len_after_day1 + u64::try_from(day2_line.len())?;
    let total_with_half = std::fs::metadata(&tp)?.len();
    assert!(total_with_half > expected_complete_end);

    let (inspected_off2, inspected_lines2, _) =
        kp_ingest::hooks::inspect_transcript_cursor_file(&tp)?;
    assert_eq!(inspected_off2, expected_complete_end);
    assert_eq!(inspected_lines2, 277);

    let stop2 = kp_ingest::hooks::capture_session_in_tz(
        &mut store,
        &json!({"session_id": "huge-inc", "transcript_path": tp}),
        &actor,
        EpochMs(2_000),
        false,
        &shanghai,
    )?;
    assert_eq!(stop2["captured"], true);
    assert_eq!(stop2["cursor_byte_offset"], expected_complete_end);
    let src2 = stop2["source"]["source_id"].as_str().ok_or("src2")?;
    let raw2 = store
        .raw_source_text(&src2.parse()?)?
        .ok_or("missing raw2")?;
    assert!(raw2.contains("beta17mib"));
    assert!(
        !raw2.contains("gamma17mib") && !raw2.contains("[unknown]") && !raw2.contains("alpha17mib"),
        "incomplete JSONL tail must never be ingested prematurely: {raw2}"
    );

    // Repeated Stop before newline arrives sees unchanged complete prefix and still skips half-line.
    let stop2_dup = kp_ingest::hooks::capture_session_in_tz(
        &mut store,
        &json!({"session_id": "huge-inc", "transcript_path": tp}),
        &actor,
        EpochMs(2_500),
        false,
        &shanghai,
    )?;
    assert_eq!(stop2_dup["captured"], false);
    assert_eq!(stop2_dup["duplicate"], true);

    // 3. Flush the closing JSON + `\n`: now `gamma17mib` is captured cleanly as human `[speaker:alice]`.
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(&tp)?;
        f.write_all(b"\"}}\n")?;
        f.flush()?;
    }
    let full_len = std::fs::metadata(&tp)?.len();
    let stop3 = kp_ingest::hooks::capture_session_in_tz(
        &mut store,
        &json!({"session_id": "huge-inc", "transcript_path": tp}),
        &actor,
        EpochMs(3_000),
        false,
        &shanghai,
    )?;
    assert_eq!(stop3["captured"], true);
    assert_eq!(stop3["cursor_byte_offset"], full_len);
    let src3 = stop3["source"]["source_id"].as_str().ok_or("src3")?;
    let raw3 = store
        .raw_source_text(&src3.parse()?)?
        .ok_or("missing raw3")?;
    assert!(
        raw3.contains("[speaker:alice] 半截尚未刷盘 gamma17mib") && !raw3.contains("[unknown]"),
        "unexpected raw3: {raw3}"
    );

    // 4. Verify daemon slice ingestion on >17 MiB cross-midnight slice with trailing half-line clamps `committed_end`
    //    and records monotonic sub-batch spans (`0..len_after_day1`, `len_after_day1..full_len`).
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(&tp)?;
        f.write_all(
            b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"unterminated",
        )?;
        f.flush()?;
    }
    let len_with_second_half = std::fs::metadata(&tp)?.len();
    let (_dir_d, mut store_d) = library()?;
    store_d.upsert_session_cursor(&kp_store::SessionCursor {
        session_id: "daemon-huge".into(),
        client_kind: "codex".into(),
        transcript_path: Some(tp.display().to_string()),
        last_byte_offset: len_with_second_half,
        last_line_count: 278,
        prefix_sha256: String::new(),
        last_activity_at: EpochMs(4_000),
        last_extracted_offset: 0,
    })?;
    let committed = kp_ingest::hooks::ingest_daemon_session_slice(
        &mut store_d,
        &kp_ingest::hooks::DaemonSessionSliceSpec {
            raw_session: "daemon-huge",
            transcript_path: &tp,
            start_byte_offset: 0,
            end_byte_offset: len_with_second_half,
            actor: &actor,
            at: EpochMs(4_000),
            is_session_end: false,
            tz: Some(&shanghai),
            global_policy: &kp_model::ModelRoutePolicy::HostCli,
        },
    )?
    .committed_end_byte_offset;
    assert_eq!(committed, full_len);

    let j_day1 = store_d
        .lease_next_extraction_job(EpochMs(4_010), 60_000)?
        .ok_or("expected day1 job")?;
    let j_day2 = store_d
        .lease_next_extraction_job(EpochMs(4_011), 60_000)?
        .ok_or("expected day2 job")?;
    store_d.complete_extraction_job(&j_day1.job_id)?;
    assert_eq!(
        store_d.complete_session_source_span(&j_day1.source_id)?,
        Some(("daemon-huge".into(), len_after_day1))
    );
    store_d.complete_extraction_job(&j_day2.job_id)?;
    assert_eq!(
        store_d.complete_session_source_span(&j_day2.source_id)?,
        Some(("daemon-huge".into(), full_len))
    );
    Ok(())
}

#[tokio::test]
async fn codex_pre_tool_use_multi_guard_consumption_is_all_or_nothing() -> Result {
    let (_dir, mut store) = library()?;
    let actor: ActorId = "alice".parse()?;
    let at = EpochMs(50_000);

    for (id, key, reason) in [
        (
            "fact_multi_g1",
            "multi-guard-1",
            "维护窗口规矩一：禁止执行 sqlx migrate",
        ),
        (
            "fact_multi_g2",
            "multi-guard-2",
            "维护窗口规矩二：数据库变更前必须备份",
        ),
    ] {
        store.remember(&[FactInput {
            fact: Fact {
                id: id.parse()?,
                origin_library: store.library_id().clone(),
                logical_key: key.parse()?,
                kind: FactKind::Constraint,
                subject: "sqlx migrate".into(),
                predicate: "kp.policy.guard".into(),
                object: json!({ "hard_guard": true, "reason": reason }),
                tags: BTreeSet::new(),
                author: actor.clone(),
                ingested_by: None,
                trust_tier: TrustTier::OwnerExplicit,
                access: FactAccess::Base {
                    audience: AudienceSpec::Explicit(BTreeSet::from([actor.clone()])),
                    stamp: BitemporalStamp {
                        valid: ValidityWindow {
                            valid_from: EpochMs(0),
                            valid_until: EpochMs::OPEN_END,
                        },
                        recorded_at: EpochMs(1),
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
    }

    let payload = json!({
        "_kp_client": "codex",
        "tool_name": "Bash",
        "tool_input": { "command": "sqlx migrate run" }
    });

    // Partial allow (`fact_multi_g1` only): Codex PreToolUse MUST consume ZERO grants and report only `fact_multi_g2`.
    let g1: FactId = "fact_multi_g1".parse()?;
    let g2: FactId = "fact_multi_g2".parse()?;
    store.grant_guard_allow_once(&actor, &g1, 600_000, at)?;

    let partial_deny = run(&mut store, "pre-tool-use", &payload, &actor, at).await?;
    assert_eq!(
        partial_deny["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );
    let partial_reason = partial_deny["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .ok_or("partial_reason")?;
    assert!(
        partial_reason.contains("kp allow fact_multi_g2")
            && !partial_reason.contains("kp allow fact_multi_g1"),
        "expected only missing guard in reason: {partial_reason}"
    );

    // Because `fact_multi_g1` was NOT consumed, granting ONLY `fact_multi_g2` now satisfies BOTH atomically!
    store.grant_guard_allow_once(&actor, &g2, 600_000, at)?;
    let allowed = run(&mut store, "pre-tool-use", &payload, &actor, at).await?;
    assert_eq!(allowed, json!({}));

    // Next invocation blocks on BOTH guards again because both single-use grants were consumed.
    let denied_again = run(&mut store, "pre-tool-use", &payload, &actor, at).await?;
    let reason_again = denied_again["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .ok_or("reason_again")?;
    assert!(
        reason_again.contains("fact_multi_g1") && reason_again.contains("fact_multi_g2"),
        "expected both guards in second deny: {reason_again}"
    );
    Ok(())
}

#[test]
fn cross_midnight_subbatch_recovery_preserves_exact_byte_span_logical_key() -> Result {
    let shanghai = kp_model::TimeZone::get("Asia/Shanghai")?;
    let (dir, mut store) = library()?;
    let actor: ActorId = "alice".parse()?;
    let tp = dir.path().join("recovery-span-key.jsonl");
    let day1 = "{\"type\":\"user\",\"timestamp\":\"2026-11-22T23:50:00+08:00\",\"message\":{\"role\":\"user\",\"content\":\"跨夜第一段复盘安排\"}}\n";
    let day2 = "{\"type\":\"user\",\"timestamp\":\"2026-11-23T00:05:00+08:00\",\"message\":{\"role\":\"user\",\"content\":\"跨夜第二段巡检安排\"}}\n";
    std::fs::write(&tp, format!("{day1}{day2}"))?;
    let day1_end = u64::try_from(day1.len())?;
    let full_end = std::fs::metadata(&tp)?.len();

    // First run ingests `0..full_end` (splitting into `0..day1_end` and `day1_end..full_end`).
    store.upsert_session_cursor(&kp_store::SessionCursor {
        session_id: "span-stable".into(),
        client_kind: "codex".into(),
        transcript_path: Some(tp.display().to_string()),
        last_byte_offset: full_end,
        last_line_count: 2,
        prefix_sha256: String::new(),
        last_activity_at: EpochMs(1_000),
        last_extracted_offset: 0,
    })?;
    kp_ingest::hooks::ingest_daemon_session_slice(
        &mut store,
        &kp_ingest::hooks::DaemonSessionSliceSpec {
            raw_session: "span-stable",
            transcript_path: &tp,
            start_byte_offset: 0,
            end_byte_offset: full_end,
            actor: &actor,
            at: EpochMs(1_000),
            is_session_end: false,
            tz: Some(&shanghai),
            global_policy: &kp_model::ModelRoutePolicy::HostCli,
        },
    )?;
    let j1 = store
        .lease_next_extraction_job(EpochMs(1_010), 60_000)?
        .ok_or("j1")?;
    let j2 = store
        .lease_next_extraction_job(EpochMs(1_011), 60_000)?
        .ok_or("j2")?;
    // Day 1 completes (advancing `last_extracted_offset` to `day1_end`), while Day 2 (`j2`, `[day1_end, full_end)`)
    // is STILL unfinished when more Day-2 turns are appended up to `extended_end`!
    store.complete_extraction_job(&j1.job_id)?;
    let day2_extra = "{\"type\":\"user\",\"timestamp\":\"2026-11-23T00:12:00+08:00\",\"message\":{\"role\":\"user\",\"content\":\"跨夜追加第三段校准安排\"}}\n";
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&tp)?;
        f.write_all(day2_extra.as_bytes())?;
        f.flush()?;
    }
    let extended_end = std::fs::metadata(&tp)?.len();
    store.upsert_session_cursor(&kp_store::SessionCursor {
        session_id: "span-stable".into(),
        client_kind: "codex".into(),
        transcript_path: Some(tp.display().to_string()),
        last_byte_offset: extended_end,
        last_line_count: 3,
        prefix_sha256: String::new(),
        last_activity_at: EpochMs(2_000),
        last_extracted_offset: day1_end,
    })?;

    kp_ingest::hooks::ingest_daemon_session_slice(
        &mut store,
        &kp_ingest::hooks::DaemonSessionSliceSpec {
            raw_session: "span-stable",
            transcript_path: &tp,
            start_byte_offset: day1_end,
            end_byte_offset: extended_end,
            actor: &actor,
            at: EpochMs(2_000),
            is_session_end: false,
            tz: Some(&shanghai),
            global_policy: &kp_model::ModelRoutePolicy::HostCli,
        },
    )?;
    // Re-running from `day1_end..extended_end` reuses `[day1_end, full_end)` (`j2`) without overlap and
    // enqueues ONLY ONE non-overlapping job (`j3`) for `[full_end, extended_end)`!
    let j3 = store
        .lease_next_extraction_job(EpochMs(2_010), 60_000)?
        .ok_or("j3")?;
    assert_ne!(j3.source_id, j2.source_id);
    assert!(store
        .lease_next_extraction_job(EpochMs(2_011), 60_000)?
        .is_none());
    let j3_ctx = store
        .extraction_source_context(&j3.source_id, &j3.chunk_ids)?
        .ok_or("j3 ctx")?;
    assert!(
        j3_ctx.chunks[0].2.contains("跨夜追加第三段校准安排")
            && !j3_ctx.chunks[0].2.contains("跨夜第二段巡检安排"),
        "expected non-overlapping tail-only chunk, got: {}",
        j3_ctx.chunks[0].2
    );

    // Out-of-order completion: `j3` (`[full_end, extended_end)`) finishes before `j2` (`[day1_end, full_end)`) ->
    // `last_extracted_offset` stays at `day1_end` until `j2` finishes, then jumps contiguously to `extended_end`!
    store.complete_extraction_job(&j3.job_id)?;
    assert_eq!(
        store
            .get_session_cursor("span-stable")?
            .map(|c| c.last_extracted_offset),
        Some(day1_end)
    );
    store.complete_extraction_job(&j2.job_id)?;
    assert_eq!(
        store
            .get_session_cursor("span-stable")?
            .map(|c| c.last_extracted_offset),
        Some(extended_end)
    );
    Ok(())
}

#[test]
fn cursor_and_windsurf_connect_preserve_existing_mcp_servers_and_scrub_windows_home_paths() -> Result
{
    let dir = tempfile::tempdir()?;
    let lib_dir = dir.path().join("lib");
    std::fs::create_dir_all(&lib_dir)?;
    let proj_dir = dir.path().join("proj");
    std::fs::create_dir_all(proj_dir.join(".cursor"))?;
    std::fs::write(
        proj_dir.join(".cursor/mcp.json"),
        r#"{"mcpServers":{"existing":{"command":"npx","args":["other-mcp"]}},"customFlag":true}"#,
    )?;

    let alice: ActorId = "alice".parse()?;
    let fake_exe = dir.path().join("kp");
    let cursor_rep = kp_ingest::connect::connect_with_scope(
        "cursor", &proj_dir, false, &lib_dir, &alice, &fake_exe,
    )?;
    assert_eq!(cursor_rep.settings, proj_dir.join(".cursor/mcp.json"));
    let cursor_json: Value = serde_json::from_slice(&std::fs::read(&cursor_rep.settings)?)?;
    assert_eq!(cursor_json["customFlag"], true);
    assert_eq!(cursor_json["mcpServers"]["existing"]["command"], "npx");
    assert_eq!(cursor_json["mcpServers"]["kp"]["args"][2], "mcp");
    assert_eq!(cursor_json["mcpServers"]["kp"]["args"][4], "alice");

    let ws_rep = kp_ingest::connect::connect_with_scope(
        "windsurf", &proj_dir, false, &lib_dir, &alice, &fake_exe,
    )?;
    assert_eq!(ws_rep.settings, proj_dir.join(".windsurf/mcp_config.json"));
    let ws_json: Value = serde_json::from_slice(&std::fs::read(&ws_rep.settings)?)?;
    assert_eq!(ws_json["mcpServers"]["kp"]["args"][2], "mcp");

    let redactor = kp_model::SecretRedactor::new()?;
    let u_seg = format!("{}sers", 'U');
    let win_bs = format!("error in C:\\{u_seg}\\win_dev\\work\\src\\lib.rs:42");
    let win_sl = format!("opened file:///D:/{u_seg}/win_ops/app/main.ts");
    let win_esc = format!("{{\"path\":\"C:\\\\{u_seg}\\\\win_json\\\\Cargo.toml\"}}");
    assert_eq!(
        kp_ingest::session_miner::scrub_session_privacy(&win_bs, &redactor),
        "error in ~\\work\\src\\lib.rs:42"
    );
    assert_eq!(
        kp_ingest::session_miner::scrub_session_privacy(&win_sl, &redactor),
        "opened file:///~/app/main.ts"
    );
    assert_eq!(
        kp_ingest::session_miner::scrub_session_privacy(&win_esc, &redactor),
        "{\"path\":\"~\\\\Cargo.toml\"}"
    );
    Ok(())
}
