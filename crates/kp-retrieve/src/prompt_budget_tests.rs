use kp_model::{
    ActorId, AnchorSpec, AudienceSpec, BitemporalStamp, BroaderScopeCandidate, EpochMs, Fact,
    FactAccess, FactKind, FactRank, FactStatus, GovernedScope, LibraryMode, MemberRole, ReviewItem,
    ReviewKind, ReviewStatus, SourceScope, SourceScopeId, TrustTier, ValidityWindow,
};
use kp_store::{
    new_concept_id, new_fact_id, new_review_id, ChunkInput, InitOptions, RawSourceInput, Store,
};
use proptest::prelude::*;
use proptest::test_runner::RngSeed;

use super::*;
use crate::test_support::TestResult;

// 2026-11-19T10:00:00Z
const NOV_19_2026_UTC: EpochMs = EpochMs(1_795_082_400_000);

fn community_store() -> TestResult<(tempfile::TempDir, Store)> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::init(
        dir.path(),
        InitOptions {
            library_id: "community-main".parse()?,
            name: "Community".into(),
            mode: LibraryMode::Community,
            owner: "alice".parse()?,
            created_at: NOV_19_2026_UTC,
        },
    )?;
    store.add_member("bob".parse()?, MemberRole::Member, NOV_19_2026_UTC)?;
    store.add_member("carol".parse()?, MemberRole::Member, NOV_19_2026_UTC)?;
    Ok((dir, store))
}

fn make_fact(
    id: kp_model::FactId,
    author: &str,
    audience: &[&str],
    subject: &str,
    summary: &str,
) -> TestResult<Fact> {
    Ok(Fact {
        id: id.clone(),
        origin_library: "community-main".parse()?,
        logical_key: id.as_str().parse()?,
        kind: FactKind::Constraint,
        subject: subject.into(),
        predicate: "kp.constraint.rule".into(),
        object: serde_json::json!({"summary": summary}),
        tags: Default::default(),
        author: author.parse()?,
        ingested_by: None,
        trust_tier: TrustTier::HumanVerbatim,
        access: FactAccess::Base {
            audience: AudienceSpec::Explicit(
                audience
                    .iter()
                    .map(|a| a.parse::<ActorId>())
                    .collect::<kp_model::Result<_>>()?,
            ),
            stamp: BitemporalStamp {
                valid: ValidityWindow {
                    valid_from: EpochMs(0),
                    valid_until: EpochMs::OPEN_END,
                },
                recorded_at: NOV_19_2026_UTC,
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
    })
}

#[test]
fn unrelated_queries_return_zero_injection_below_threshold() -> TestResult {
    let (_dir, mut store) = community_store()?;
    let alice: ActorId = "alice".parse()?;

    let gym_rule = make_fact(
        new_fact_id()?,
        "bob",
        &["alice", "bob"],
        "健身恢复",
        "周四膝盖酸痛恢复期，暂停负重深蹲训练",
    )?;
    store.insert_fact(&gym_rule)?;

    let unrelated = retrieve_prompt_context(
        &store,
        &alice,
        NOV_19_2026_UTC,
        "帮我重构 Cargo.toml 里的 clippy lint 配置",
        &PromptBudgetConfig::default().with_time_zone(TimeZone::get("Asia/Shanghai")?),
    )?;
    assert!(unrelated.text.is_empty());
    assert!(unrelated.items.is_empty());
    assert_eq!(unrelated.total_tokens, 0);

    let blank = retrieve_prompt_context(
        &store,
        &alice,
        NOV_19_2026_UTC,
        "   ",
        &PromptBudgetConfig::default().with_time_zone(TimeZone::get("Asia/Shanghai")?),
    )?;
    assert!(blank.text.is_empty());
    assert_eq!(blank.total_tokens, 0);
    Ok(())
}

#[test]
fn provenance_header_uses_date_speaker_source_scope_and_redacts_secrets() -> TestResult {
    let (_dir, mut store) = community_store()?;
    let alice: ActorId = "alice".parse()?;
    let scope_id: SourceScopeId = "ext_chat.dba".parse()?;
    store.upsert_source_scope(&SourceScope {
        scope_id: scope_id.clone(),
        provider: "ext_chat".into(),
        external_container_id: "oc_dba".into(),
        label: "数据库运维群".into(),
        current_audience: AudienceSpec::Explicit(
            ["alice".parse()?, "bob".parse()?].into_iter().collect(),
        ),
        synced_at: NOV_19_2026_UTC,
    })?;

    let fid = new_fact_id()?;
    let mut cutover = make_fact(
        fid.clone(),
        "bob",
        &["alice", "bob"],
        "主从切换",
        "周四凌晨两点进行数据库主从切换，窗口期内暂停离线报表任务 sk-1234567890abcdefghijklmnop",
    )?;
    // Importer (`alice`) differs from original speaker (`bob`); header MUST show `bob`.
    cutover.ingested_by = Some(alice.clone());
    cutover.source_scope_id = Some(scope_id);
    store.insert_fact(&cutover)?;

    let ctx = retrieve_prompt_context(
        &store,
        &alice,
        NOV_19_2026_UTC,
        "数据库主从切换窗口期能跑离线报表吗",
        &PromptBudgetConfig::default().with_time_zone(TimeZone::get("Asia/Shanghai")?),
    )?;
    assert_eq!(ctx.items.len(), 1);
    assert!(ctx.total_tokens <= MAX_PROMPT_CONTEXT_TOKENS);
    let expected_prefix = format!("[2026-11-19 · bob @ 数据库运维群 · kp-ref:{fid}] ");
    assert!(
        ctx.text.starts_with(&expected_prefix),
        "unexpected formatted header: {}",
        ctx.text
    );
    assert!(!ctx.text.contains("sk-1234567890"));
    assert!(ctx.text.contains("[REDACTED]"));
    Ok(())
}

#[test]
fn serendipitous_review_hint_appears_only_for_author_and_disappears_once_resolved() -> TestResult {
    let (_dir, mut store) = community_store()?;
    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;

    let club_group = new_concept_id()?;
    let fid = new_fact_id()?;
    let mut amb_fact = make_fact(
        fid.clone(),
        "bob",
        &["alice", "bob"],
        "训练禁忌",
        "膝盖酸痛时不做负重深蹲",
    )?;
    amb_fact.governed_scope = Some(GovernedScope::SelfOnly { actor: bob.clone() });
    amb_fact.broader_scope = Some(BroaderScopeCandidate {
        narrow_active: GovernedScope::SelfOnly { actor: bob.clone() },
        broader_candidate: GovernedScope::RelationGroup {
            anchor: bob.clone(),
            group_concept_id: club_group.clone(),
        },
        question: "只管你自己，还是整个训练小组？".into(),
    });
    store.insert_fact(&amb_fact)?;

    // 1) Before any review item is created, author `bob` gets the serendipitous inline hint
    //    via `broader_scope` when chatting about squat training!
    let bob_ctx = retrieve_prompt_context(
        &store,
        &bob,
        NOV_19_2026_UTC,
        "膝盖酸痛负重深蹲安排",
        &PromptBudgetConfig::default().with_time_zone(TimeZone::get("Asia/Shanghai")?),
    )?;
    assert_eq!(bob_ctx.items.len(), 1);
    assert!(bob_ctx.items[0].has_review_hint);
    assert!(
        bob_ctx
            .text
            .ends_with("（待确认：只管你自己，还是整个训练小组？ 可用 kp review 确认）"),
        "actual: {}",
        bob_ctx.text
    );

    // 2) Viewer `alice` (`viewer != fact.author`) NEVER sees Bob's review prompt!
    let alice_ctx = retrieve_prompt_context(
        &store,
        &alice,
        NOV_19_2026_UTC,
        "膝盖酸痛负重深蹲安排",
        &PromptBudgetConfig::default().with_time_zone(TimeZone::get("Asia/Shanghai")?),
    )?;
    assert_eq!(alice_ctx.items.len(), 1);
    assert!(!alice_ctx.items[0].has_review_hint);
    assert!(!alice_ctx.text.contains("待确认"));
    assert!(!alice_ctx.text.contains("kp review"));

    // 3) Cross-person collision enqueues an open `AmbiguousScope` item in `review_items`,
    //    and Bob still sees the updated question until he resolves it.
    let mut mem_fact = make_fact(
        new_fact_id()?,
        "bob",
        &["alice", "bob"],
        "alice",
        "训练小组成员",
    )?;
    mem_fact.kind = kp_model::FactKind::Membership;
    mem_fact.predicate = "kp.core.membership".into();
    mem_fact.object = serde_json::to_value(kp_model::MembershipRelation {
        anchor: bob.clone(),
        member: alice.clone(),
        group_concept_id: club_group,
    })?;
    store.insert_fact(&mem_fact)?;
    let rev = store.upsert_open_review_item(&ReviewItem {
        id: new_review_id()?,
        actor: bob.clone(),
        kind: ReviewKind::AmbiguousScope,
        target_id: fid.to_string(),
        question: "这条不做负重深蹲只管你自己，还是管整个训练小组？".into(),
        options: vec!["只管自己".into(), "训练小组".into()],
        payload: serde_json::json!({}),
        status: ReviewStatus::Open,
        created_at: NOV_19_2026_UTC,
        resolved_at: None,
    })?;
    let bob_open_ctx = retrieve_prompt_context(
        &store,
        &bob,
        NOV_19_2026_UTC,
        "膝盖酸痛负重深蹲安排",
        &PromptBudgetConfig::default().with_time_zone(TimeZone::get("Asia/Shanghai")?),
    )?;
    assert!(bob_open_ctx.text.contains(
        "（待确认：这条不做负重深蹲只管你自己，还是管整个训练小组？ 可用 kp review 确认）"
    ));

    // 4) Once Bob dismisses/resolves the review item, the inline hint stops nagging immediately!
    store.resolve_review_item(&rev.id, &bob, ReviewStatus::Dismissed, NOV_19_2026_UTC)?;
    let bob_resolved_ctx = retrieve_prompt_context(
        &store,
        &bob,
        NOV_19_2026_UTC,
        "膝盖酸痛负重深蹲安排",
        &PromptBudgetConfig::default().with_time_zone(TimeZone::get("Asia/Shanghai")?),
    )?;
    assert_eq!(bob_resolved_ctx.items.len(), 1);
    assert!(!bob_resolved_ctx.items[0].has_review_hint);
    assert!(!bob_resolved_ctx.text.contains("待确认"));
    Ok(())
}

#[test]
fn hard_budget_clamps_to_at_most_three_items_and_250_tokens_even_on_long_passages() -> TestResult {
    let (_dir, mut store) = community_store()?;
    let alice: ActorId = "alice".parse()?;

    for idx in 0..8 {
        let long_summary = format!(
            "差旅住宿报销细则第{idx}条：{}",
            "提交报销单前务必核对电子发票抬头税号与行程水单避免财务退单重审".repeat(8)
        );
        let f = make_fact(
            new_fact_id()?,
            "alice",
            &["alice"],
            &format!("差旅报销规范{idx}"),
            &long_summary,
        )?;
        store.insert_fact(&f)?;
    }

    // Even if a caller requests `max_tokens: 5000, max_items: 20`, hard caps (250 / 3) hold!
    let loose_cfg = PromptBudgetConfig {
        max_tokens: 5_000,
        max_items: 20,
        time_zone: TimeZone::get("Asia/Shanghai")?,
        ..PromptBudgetConfig::default()
    };
    let ctx = retrieve_prompt_context(
        &store,
        &alice,
        NOV_19_2026_UTC,
        "差旅住宿报销发票",
        &loose_cfg,
    )?;
    assert!(!ctx.items.is_empty());
    assert!(ctx.items.len() <= MAX_PROMPT_CONTEXT_ITEMS);
    assert!(ctx.total_tokens <= MAX_PROMPT_CONTEXT_TOKENS);
    assert_eq!(ctx.total_tokens, estimate_tokens(&ctx.text));
    for item in &ctx.items {
        assert!(item.rendered.starts_with("[2026-11-19 · alice @ "));
        assert!(item.rendered.contains(" · kp-ref:fact_"));
    }

    // Also verify raw chunk fallback provenance works cleanly.
    let raw = RawSourceInput {
        self_echo_filtered: true,
        uri: "docs/cutover-runbook.md".into(),
        media_type: "text/markdown".into(),
        content: "主库回切操作必须双人复核".as_bytes().to_vec(),
        readable_by: AudienceSpec::Explicit([alice.clone()].into_iter().collect()),
        captured_at: NOV_19_2026_UTC,
        chunks: vec![ChunkInput {
            heading: "回切制度".into(),
            body: "主库回切操作必须双人复核".into(),
            anchor: AnchorSpec::default(),
        }],
    };
    store.ingest_source(&"community-main".parse()?, &"cut-pol".parse()?, &raw)?;
    let chunk_ctx = retrieve_prompt_context(
        &store,
        &alice,
        NOV_19_2026_UTC,
        "主库回切双人复核",
        &PromptBudgetConfig::default().with_time_zone(TimeZone::get("Asia/Shanghai")?),
    )?;
    assert_eq!(chunk_ctx.items.len(), 1);
    assert!(chunk_ctx
        .text
        .starts_with("[2026-11-19 · alice @ docs/cutover-runbook.md · kp-ref:"));

    // Unrelated prompts sharing only 1-char grammatical particles (`的`, `里`, `吗`) never trigger injection.
    let particle_only = make_fact(
        new_fact_id()?,
        "alice",
        &["alice"],
        "读书会纪律",
        "周末在图书馆里的研讨室做笔记吗",
    )?;
    store.insert_fact(&particle_only)?;
    let noise_check = retrieve_prompt_context(
        &store,
        &alice,
        NOV_19_2026_UTC,
        "帮我检查 Cargo.toml 里的编译选项吗",
        &PromptBudgetConfig::default().with_time_zone(TimeZone::get("Asia/Shanghai")?),
    )?;
    assert!(
        noise_check.text.is_empty(),
        "particle-only overlap must not inject: {}",
        noise_check.text
    );
    Ok(())
}

#[test]
fn format_date_iso_is_infallible_on_open_end_and_overflow_timestamps() -> TestResult {
    let shanghai = TimeZone::get("Asia/Shanghai")?;
    // 2026-11-21T23:00:00Z == 2026-11-22T07:00:00+08:00 (must render local Shanghai date 11-22, not UTC 11-21)
    assert_eq!(
        format_date_iso_in_tz(EpochMs(1_795_302_000_000), &shanghai),
        "2026-11-22"
    );
    assert_eq!(
        format_date_iso_in_tz(EpochMs::OPEN_END, &shanghai),
        "日期未知"
    );
    assert_eq!(
        format_date_iso_in_tz(EpochMs(i64::MAX - 1), &shanghai),
        "日期未知"
    );
    assert_eq!(
        format_date_iso_in_tz(EpochMs(i64::MIN), &shanghai),
        "日期未知"
    );

    // Formatting a candidate carrying `EpochMs(i64::MAX - 1)` never errors and always carries `kp-ref:fact_`.
    let alice: ActorId = "alice".parse()?;
    let out = format_prompt_candidates(
        &alice,
        &[PromptCandidate {
            ref_id: "legacy_bare_id".into(),
            kind: Some(FactKind::Constraint),
            recorded_at: EpochMs(i64::MAX - 1),
            speaker: alice.clone(),
            fact_author: Some(alice.clone()),
            source_label: "local".into(),
            claim_text: "维护窗口复核规范".into(),
            is_untrusted: false,
            score: 0.85,
            pending_ambiguous_question: None,
            supporting_premises: Vec::new(),
        }],
        &PromptBudgetConfig::default().with_time_zone(TimeZone::get("Asia/Shanghai")?),
    )?;
    assert_eq!(
        out.text,
        "[日期未知 · alice @ local · kp-ref:fact_legacy_bare_id] 维护窗口复核规范"
    );
    Ok(())
}

#[test]
fn active_rsi_skills_and_low_signal_acks_are_folded_into_retrieve_prompt_context() -> TestResult {
    let (_dir, mut store) = community_store()?;
    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;

    let premise_fact = make_fact(
        new_fact_id()?,
        "alice",
        &["alice", "bob"],
        "接口契约前提",
        "请求体提交前需校验字段契约",
    )?;
    store.insert_fact(&premise_fact)?;

    // Insert an unapproved (quarantined) derived `skill_version` fact visible to `alice` and `bob`.
    let pkg = kp_model::SkillPackage {
        name: "verify-schema-contract".parse()?,
        description: "Verify request payload fields against the current schema before sending."
            .into(),
        version: 1,
        dimension: kp_model::RsiDimension::Dom,
        trigger: "constructing a versioned schema payload".into(),
        rules: vec!["Check each field against schema v2 before submitting.".into()],
        references: vec![premise_fact.strong_ref()],
        scripts: Vec::new(),
        exec_approved: false,
    };
    let skill_id = new_fact_id()?;
    let mut skill_fact = kp_model::derived_fact(
        &"remote-community".parse()?,
        &alice,
        FactKind::SkillVersion,
        "verify-schema-contract",
        "kp.rsi.skill_version",
        serde_json::to_value(&pkg)?,
        NOV_19_2026_UTC,
    )?;
    skill_fact.id = skill_id.clone();
    skill_fact.logical_key = skill_id.as_str().parse()?;
    store.insert_fact(&skill_fact)?;
    store.insert_justification(&kp_model::Justification {
        id: kp_store::new_justification_id()?,
        conclusion: skill_id.clone(),
        mode: kp_model::DerivationMode::RsiAttribution,
        rule_id: "R_SKILL_PROMOTE".into(),
        rule_version: "1".into(),
        premises: vec![kp_model::Premise {
            fact_id: premise_fact.id.clone(),
            strong_ref: premise_fact.strong_ref(),
        }],
        active: true,
        created_at: NOV_19_2026_UTC,
        invalidated_at: None,
        invalidation_reason: None,
    })?;

    // Low-signal acks skip retrieval cleanly.
    for ack in ["好", "好的！", "收到", "ok", "thanks~", "<skip_summary/>"] {
        let skipped = retrieve_prompt_context(
            &store,
            &bob,
            NOV_19_2026_UTC,
            ack,
            &PromptBudgetConfig::default().with_time_zone(TimeZone::get("Asia/Shanghai")?),
        )?;
        assert!(skipped.text.is_empty(), "ack `{ack}` must be skipped");
    }

    // Matching skill injects `trigger_line()`, followed immediately by `READ_ONLY_SKILL`, before `description`,
    // strictly within `<= 250` tokens and `<= 3` items.
    let ctx = retrieve_prompt_context(
        &store,
        &bob,
        NOV_19_2026_UTC,
        "constructing a versioned schema payload",
        &PromptBudgetConfig::default().with_time_zone(TimeZone::get("Asia/Shanghai")?),
    )?;
    assert_eq!(ctx.items.len(), 1);
    assert!(ctx.total_tokens <= MAX_PROMPT_CONTEXT_TOKENS);
    assert!(ctx.text.contains(&format!("kp-ref:{skill_id}")));
    let trigger_pos = ctx
        .text
        .find(&pkg.trigger_line())
        .ok_or("missing trigger_line")?;
    let notice_pos = ctx
        .text
        .find(kp_model::READ_ONLY_SKILL)
        .ok_or("missing READ_ONLY_SKILL")?;
    let desc_pos = ctx
        .text
        .find("Verify request payload")
        .ok_or("missing description")?;
    assert!(trigger_pos < notice_pos && notice_pos < desc_pos);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, rng_seed: RngSeed::Fixed(404), ..ProptestConfig::default() })]
    #[test]
    fn budget_invariants_hold_for_arbitrary_candidates(
        claims in prop::collection::vec(
            ("[a-z0-9_\u{4e00}-\u{9fa5} ]{1,300}", 0.0_f64..=1.0, prop::option::of("[\u{4e00}-\u{9fa5}a-z?？ ]{1,60}"), any::<bool>()),
            0..15
        ),
        requested_tokens in 1usize..600,
        requested_items in 1usize..10,
        min_score in 0.05_f64..0.95,
    ) {
        let viewer: ActorId = "alice".parse().map_err(|e| TestCaseError::fail(format!("{e}")))?;
        let other: ActorId = "bob".parse().map_err(|e| TestCaseError::fail(format!("{e}")))?;
        let mut candidates = Vec::with_capacity(claims.len());
        for (idx, (claim, score, question, is_author)) in claims.into_iter().enumerate() {
            let author = if is_author { viewer.clone() } else { other.clone() };
            candidates.push(PromptCandidate {
                ref_id: format!("fact_01934e3a-0000-7000-8000-{idx:012x}"),
                kind: Some(FactKind::Constraint),
                recorded_at: NOV_19_2026_UTC,
                speaker: author.clone(),
                fact_author: Some(author),
                source_label: "运维讨论群".into(),
                claim_text: claim,
                is_untrusted: false,
                score,
                pending_ambiguous_question: question,
                supporting_premises: Vec::new(),
            });
        }
        let shanghai = TimeZone::get("Asia/Shanghai")
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;
        let cfg = PromptBudgetConfig {
            max_tokens: requested_tokens,
            max_items: requested_items,
            min_score,
            time_zone: shanghai,
            candidate_limit: 24,
        };
        let out = format_prompt_candidates(&viewer, &candidates, &cfg)
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;
        prop_assert!(out.items.len() <= requested_items.min(MAX_PROMPT_CONTEXT_ITEMS));
        prop_assert!(out.total_tokens <= requested_tokens.min(MAX_PROMPT_CONTEXT_TOKENS));
        prop_assert!(out.total_tokens <= MAX_PROMPT_CONTEXT_TOKENS);
        prop_assert_eq!(out.total_tokens, estimate_tokens(&out.text));
        if candidates.iter().all(|c| c.score < min_score) {
            prop_assert!(out.text.is_empty());
            prop_assert!(out.items.is_empty());
        }
        for item in &out.items {
            prop_assert!(item.score >= min_score);
            prop_assert!(item.rendered.contains(" · kp-ref:fact_"));
            if item.rendered.contains(" @ bob ") || item.rendered.contains("· bob @") {
                prop_assert!(!item.has_review_hint);
                prop_assert!(!item.rendered.contains("待确认"));
            }
        }
    }
}

#[test]
fn raw_session_capture_chunk_without_extracted_facts_returns_empty_prompt_context() -> TestResult {
    let (_dir, mut store) = community_store()?;
    let alice: ActorId = "alice".parse()?;

    let raw_transcript = "[speaker:alice] 周四下午断网\n[assistant] 好的，记下了。";
    store.ingest_source(
        &"community-main".parse()?,
        &"session:s1".parse()?,
        &RawSourceInput {
            self_echo_filtered: true,
            uri: "session:s1".into(),
            media_type: "text/markdown".into(),
            content: raw_transcript.as_bytes().to_vec(),
            readable_by: AudienceSpec::Explicit([alice.clone()].into_iter().collect()),
            captured_at: NOV_19_2026_UTC,
            chunks: vec![ChunkInput {
                heading: String::new(),
                body: raw_transcript.into(),
                anchor: AnchorSpec::default(),
            }],
        },
    )?;

    let ctx = retrieve_prompt_context(
        &store,
        &alice,
        NOV_19_2026_UTC,
        "周四下午断网",
        &PromptBudgetConfig::default().with_time_zone(TimeZone::get("Asia/Shanghai")?),
    )?;
    assert_eq!(ctx.text, "");
    assert!(ctx.items.is_empty());
    assert_eq!(ctx.total_tokens, 0);
    Ok(())
}

#[test]
fn derived_collision_warning_renders_human_premise_quotes_deduplicates_premises_and_hides_session_and_private_premises(
) -> TestResult {
    let (_dir, mut store) = community_store()?;
    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;
    let carol: ActorId = "carol".parse()?;

    // 1. Ingest raw `session:s1` staging chunk with internal speaker/assistant tags.
    let raw_transcript = "[speaker:alice] 周四下午断网\n[assistant] 好的，记下了。";
    let stored_s1 = store.ingest_source(
        &"community-main".parse()?,
        &"session:s1".parse()?,
        &RawSourceInput {
            self_echo_filtered: true,
            uri: "session:s1".into(),
            media_type: "text/markdown".into(),
            content: raw_transcript.as_bytes().to_vec(),
            readable_by: AudienceSpec::Explicit([alice.clone(), bob.clone()].into_iter().collect()),
            captured_at: NOV_19_2026_UTC,
            chunks: vec![ChunkInput {
                heading: String::new(),
                body: raw_transcript.into(),
                anchor: AnchorSpec::default(),
            }],
        },
    )?;

    let ops_scope: SourceScopeId = "chat.scope.netops".parse()?;
    store.upsert_source_scope(&SourceScope {
        scope_id: ops_scope.clone(),
        provider: "chat".into(),
        external_container_id: "oc_netops".into(),
        label: "网络值班群".into(),
        current_audience: AudienceSpec::Explicit(
            [alice.clone(), bob.clone()].into_iter().collect(),
        ),
        synced_at: NOV_19_2026_UTC,
    })?;

    let ops_group = store.ensure_community_concept(
        kp_model::ConceptKind::Group,
        "机房值班组",
        "ops",
        kp_model::ConceptPolarity::Neutral,
        &bob,
        NOV_19_2026_UTC,
    )?;

    // 2. Three visible premises (`Event` + `Constraint` + `Membership`) + 1 invisible (`bob`-only) premise.
    let ev_id = new_fact_id()?;
    let mut outage_ev = make_fact(
        ev_id.clone(),
        "alice",
        &["alice", "bob"],
        "机房网络",
        "周四下午机房断网维护",
    )?;
    outage_ev.kind = FactKind::Event;
    outage_ev.source_anchor = Some(AnchorSpec {
        source_id: Some(stored_s1.source_id),
        verbatim_quote: Some(
            "[turn:0] [speaker:alice] 周四下午断网\n[assistant] 好的，记下了。".into(),
        ),
        ..AnchorSpec::default()
    });

    let rule_id = new_fact_id()?;
    let mut sync_rule = make_fact(
        rule_id.clone(),
        "bob",
        &["alice", "bob"],
        "同步作业",
        "断网期间暂停全量同步脚本",
    )?;
    sync_rule.source_scope_id = Some(ops_scope);
    sync_rule.source_anchor = Some(AnchorSpec {
        verbatim_quote: Some("断网期间不要跑全量同步脚本".into()),
        ..AnchorSpec::default()
    });

    let mem_id = new_fact_id()?;
    let mut mem_fact = make_fact(mem_id.clone(), "bob", &["alice", "bob"], "roster", "")?;
    mem_fact.kind = FactKind::Membership;
    mem_fact.object = serde_json::to_value(kp_model::MembershipRelation {
        anchor: bob.clone(),
        member: alice.clone(),
        group_concept_id: ops_group.id,
    })?;

    let priv_id = new_fact_id()?;
    let mut priv_prem = make_fact(
        priv_id.clone(),
        "bob",
        &["bob"],
        "内部排障记录",
        "BOB_INVISIBLE_PREMISE_CANARY 周四下午断网",
    )?;
    priv_prem.kind = FactKind::Observation;
    priv_prem.source_anchor = Some(AnchorSpec {
        verbatim_quote: Some("BOB_INVISIBLE_PREMISE_CANARY".into()),
        ..AnchorSpec::default()
    });

    for f in [&outage_ev, &sync_rule, &mem_fact, &priv_prem] {
        store.insert_fact(f)?;
    }

    let warn_id = new_fact_id()?;
    let mut warning = kp_model::derived_fact(
        &"community-main".parse()?,
        &bob,
        FactKind::CollisionWarning,
        "alice",
        "kp.core.event_constraint_collision",
        serde_json::json!({
            "advice": "周四下午断网期间暂停执行全量同步脚本",
            "event": outage_ev.object,
            "constraint": sync_rule.object,
        }),
        NOV_19_2026_UTC,
    )?;
    warning.id = warn_id.clone();
    warning.logical_key = warn_id.as_str().parse()?;

    let j_vis = kp_model::Justification {
        id: kp_store::new_justification_id()?,
        conclusion: warn_id.clone(),
        mode: kp_model::DerivationMode::HardRule,
        rule_id: "R2_GOVERNED_COLLISION".into(),
        rule_version: "1".into(),
        premises: [&outage_ev, &sync_rule, &mem_fact]
            .into_iter()
            .map(|f| kp_model::Premise {
                fact_id: f.id.clone(),
                strong_ref: f.strong_ref(),
            })
            .collect(),
        active: true,
        created_at: NOV_19_2026_UTC,
        invalidated_at: None,
        invalidation_reason: None,
    };
    let j_priv = kp_model::Justification {
        id: kp_store::new_justification_id()?,
        conclusion: warn_id.clone(),
        mode: kp_model::DerivationMode::HardRule,
        rule_id: "R2_PRIV_COLLISION".into(),
        rule_version: "1".into(),
        premises: [&outage_ev, &priv_prem]
            .into_iter()
            .map(|f| kp_model::Premise {
                fact_id: f.id.clone(),
                strong_ref: f.strong_ref(),
            })
            .collect(),
        active: true,
        created_at: NOV_19_2026_UTC,
        invalidated_at: None,
        invalidation_reason: None,
    };
    store.save_derivations(std::slice::from_ref(&warning), &[j_vis, j_priv])?;

    let ctx = retrieve_prompt_context(
        &store,
        &alice,
        NOV_19_2026_UTC,
        "周四下午断网能跑全量同步脚本吗",
        &PromptBudgetConfig::default().with_time_zone(TimeZone::get("Asia/Shanghai")?),
    )?;

    // Standalone premises are deduplicated into `↳` lines under the single prioritized `CollisionWarning`!
    assert_eq!(
        ctx.items.len(),
        1,
        "expected deduplicated single CollisionWarning item, got: {}",
        ctx.text
    );
    assert_eq!(ctx.items[0].ref_id, warn_id.to_string());
    assert!(ctx.total_tokens <= MAX_PROMPT_CONTEXT_TOKENS);
    assert_eq!(ctx.total_tokens, estimate_tokens(&ctx.text));

    let lines: Vec<&str> = ctx.text.lines().collect();
    assert_eq!(
        lines.len(),
        4,
        "expected conclusion + 3 premise lines: {}",
        ctx.text
    );
    assert!(lines[0].contains(&format!("kp-ref:{warn_id}")));
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.starts_with("  ↳ [2026-11-19 · "))
            .count(),
        3
    );
    assert!(ctx
        .text
        .contains(&format!("kp-ref:{rule_id}] 断网期间不要跑全量同步脚本")));
    assert!(ctx.text.contains(&format!("kp-ref:{ev_id}] 周四下午断网")));
    assert!(ctx
        .text
        .contains(&format!("kp-ref:{mem_id}] alice 属于 bob 的「机房值班组」")));

    // Zero internal tags, zero assistant speech, zero invisible premise!
    for forbidden in [
        "[speaker:",
        "[turn:",
        "[assistant]",
        "好的，记下了",
        "BOB_INVISIBLE_PREMISE_CANARY",
        priv_id.as_str(),
    ] {
        assert!(
            !ctx.text.contains(forbidden),
            "forbidden token `{forbidden}` leaked into <kp-context>: {}",
            ctx.text
        );
    }

    let carol_ctx = retrieve_prompt_context(
        &store,
        &carol,
        NOV_19_2026_UTC,
        "周四下午断网能跑全量同步脚本吗",
        &PromptBudgetConfig::default().with_time_zone(TimeZone::get("Asia/Shanghai")?),
    )?;
    assert!(carol_ctx.text.is_empty());
    Ok(())
}

#[test]
fn external_untrusted_facts_and_web_chunks_render_explicit_untrusted_provenance_badge() -> TestResult
{
    let (_dir, mut store) = community_store()?;
    let alice: ActorId = "alice".parse()?;
    let tz = TimeZone::get("Asia/Shanghai")?;
    let cfg = PromptBudgetConfig::default().with_time_zone(tz);

    let untrusted_fact_id = new_fact_id()?;
    let mut untrusted_fact = make_fact(
        untrusted_fact_id.clone(),
        "ext.github.stranger",
        &["alice"],
        "redis eviction policy",
        "External PR comment reports allkeys-lru eviction tuning parameters",
    )?;
    untrusted_fact.kind = FactKind::Observation;
    untrusted_fact.trust_tier = TrustTier::ExternalUntrusted;
    store.insert_fact(&untrusted_fact)?;

    let fact_ctx = retrieve_prompt_context(
        &store,
        &alice,
        NOV_19_2026_UTC,
        "allkeys-lru eviction tuning",
        &cfg,
    )?;
    assert_eq!(fact_ctx.items.len(), 1);
    assert!(
        fact_ctx.text.starts_with(&format!(
            "[2026-11-19 · untrusted · ext.github.stranger @ community-main · kp-ref:{untrusted_fact_id}] "
        )),
        "expected untrusted provenance marker on ExternalUntrusted fact, got: {}",
        fact_ctx.text
    );

    store.ingest_source(
        &"community-main".parse()?,
        &"web-doc".parse()?,
        &RawSourceInput {
            self_echo_filtered: true,
            uri: "https://docs.example.com/pgvector-hnsw".into(),
            media_type: "text/markdown".into(),
            content: "pgvector HNSW ef_search benchmark numbers"
                .as_bytes()
                .to_vec(),
            readable_by: AudienceSpec::Explicit([alice.clone()].into_iter().collect()),
            captured_at: NOV_19_2026_UTC,
            chunks: vec![ChunkInput {
                heading: "HNSW Benchmarks".into(),
                body: "pgvector HNSW ef_search benchmark numbers".into(),
                anchor: AnchorSpec::default(),
            }],
        },
    )?;

    let web_ctx = retrieve_prompt_context(
        &store,
        &alice,
        NOV_19_2026_UTC,
        "pgvector HNSW ef_search benchmark",
        &cfg,
    )?;
    assert_eq!(web_ctx.items.len(), 1);
    assert!(
        web_ctx.text.starts_with(
            "[2026-11-19 · untrusted · alice @ https://docs.example.com/pgvector-hnsw · kp-ref:"
        ),
        "expected untrusted provenance marker on external web chunk, got: {}",
        web_ctx.text
    );
    Ok(())
}
