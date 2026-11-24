use std::collections::BTreeSet;

use kp_model::{
    derived_fact, ActorId, AnchorSpec, AudienceSpec, BitemporalStamp, CommunityConcept,
    ConceptKind, ConceptPolarity, ContentHash, DerivationMode, EpochMs, Fact, FactAccess, FactId,
    FactKind, FactRank, FactStatus, GovernedScope, Justification, LibraryMode, MemberRole,
    MembershipRelation, Premise, SourceScope, SourceScopeId, TrustTier, ValidityWindow,
};
use kp_store::{
    new_concept_id, new_fact_id, new_justification_id, CodeLocus, FactInput, InitOptions, Store,
};
use serde_json::json;

use super::*;
use crate::test_support::TestResult;

const NOV_19_2026_UTC: EpochMs = EpochMs(1_795_082_400_000);

fn test_store() -> TestResult<(tempfile::TempDir, Store)> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::init(
        dir.path(),
        InitOptions {
            library_id: "guard-lib".parse()?,
            name: "Guard Test".into(),
            mode: LibraryMode::Community,
            owner: "alice".parse()?,
            created_at: NOV_19_2026_UTC,
        },
    )?;
    store.add_member("bob".parse()?, MemberRole::Member, NOV_19_2026_UTC)?;
    store.add_member("carol".parse()?, MemberRole::Member, NOV_19_2026_UTC)?;
    store.add_member("dave".parse()?, MemberRole::Member, NOV_19_2026_UTC)?;
    Ok((dir, store))
}

fn make_base_fact(
    id: FactId,
    kind: FactKind,
    author: &str,
    audience: &[&str],
    subject: &str,
    object: serde_json::Value,
    quote: Option<&str>,
) -> TestResult<Fact> {
    Ok(Fact {
        id: id.clone(),
        origin_library: "guard-lib".parse()?,
        logical_key: id.as_str().parse()?,
        kind,
        subject: subject.into(),
        predicate: format!("kp.core.{kind}"),
        object,
        tags: BTreeSet::new(),
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
        source_anchor: quote.map(|q| AnchorSpec {
            verbatim_quote: Some(q.into()),
            ..AnchorSpec::default()
        }),
        unverified_gaps: Vec::new(),
        miss_count: 0,
    })
}

#[test]
fn shlex_command_segments_and_git_merge_subcommand_boundaries() -> TestResult {
    let (_dir, mut store) = test_store()?;
    let alice: ActorId = "alice".parse()?;

    let merge_rule_id = new_fact_id()?;
    let merge_rule = make_base_fact(
        merge_rule_id.clone(),
        FactKind::Constraint,
        "alice",
        &["alice", "bob"],
        "分支规范",
        json!({
            "claim_text": "维护窗口禁止执行 git merge 合入主干",
            "forbidden_commands": ["git merge"],
            "hard_guard": true
        }),
        Some("维护窗口禁止执行 git merge 合入主干"),
    )?;
    store.insert_fact(&merge_rule)?;

    let eval_bash = |cmd: &str| -> TestResult<GuardVerdict> {
        Ok(evaluate_tool_guards(
            &store,
            &GuardCheckInput {
                tool_name: "Bash".into(),
                tool_input: json!({"command": cmd}),
                cwd: None,
                viewer: alice.clone(),
                at: NOV_19_2026_UTC,
                time_zone: TimeZone::get("Asia/Shanghai")?,
            },
        )?)
    };

    // (a) `git merge-base HEAD main` & `git mergetool` MUST NOT match `git merge`!
    for safe_cmd in [
        "git merge-base HEAD main",
        "git merge-base --is-ancestor main feat",
        "git mergetool",
        "git commit -m \"mention git merge in commit message\" 2>&1",
    ] {
        let verdict = eval_bash(safe_cmd)?;
        assert!(
            !verdict.is_blocked(),
            "safe command `{safe_cmd}` falsely triggered: {verdict:?}"
        );
    }

    // (c) Unspaced `true&&git merge feat`, `echo ok;git merge feat`, and `bash -c "cd repo && git merge feat"` DO match!
    for blocked_cmd in [
        "true&&git merge feat",
        "echo ok;git merge feat",
        "bash -c \"cd repo && git merge feat\"",
        "/bin/zsh -lc 'cd repo && git -C subdir merge feat'",
        "echo 'unclosed && git merge feat",
    ] {
        let verdict = eval_bash(blocked_cmd)?;
        assert!(
            verdict.is_blocked(),
            "expected `{blocked_cmd}` to trigger guard"
        );
        assert_eq!(verdict.matched_fact_ids(), vec![merge_rule_id.clone()]);
        assert_eq!(verdict.allow_refs(), vec![merge_rule_id.to_string()]);
    }

    // (d) `echo "a; git merge x"` intentionally matches (conservative fail-safe behavior pinned by test).
    let conservative = eval_bash("echo \"a; git merge x\"")?;
    assert!(conservative.is_blocked());
    assert_eq!(conservative.matched_fact_ids(), vec![merge_rule_id]);

    Ok(())
}

#[test]
fn path_component_boundaries_match_src_lib_rs_without_matching_stdlib_rs() -> TestResult {
    let (_dir, mut store) = test_store()?;
    let alice: ActorId = "alice".parse()?;

    let lib_guard_id = new_fact_id()?;
    let lib_guard = make_base_fact(
        lib_guard_id.clone(),
        FactKind::Constraint,
        "alice",
        &["alice"],
        "入口文件保护",
        json!({
            "claim_text": "未经评审不要修改 lib.rs",
            "file_path": "lib.rs"
        }),
        Some("未经评审不要修改 lib.rs"),
    )?;
    store.insert_fact(&lib_guard)?;

    // (b) `src/stdlib.rs` MUST NOT match `lib.rs`!
    let stdlib_edit = evaluate_tool_guards(
        &store,
        &GuardCheckInput {
            tool_name: "Edit".into(),
            tool_input: json!({"file_path": "src/stdlib.rs"}),
            cwd: None,
            viewer: alice.clone(),
            at: NOV_19_2026_UTC,
            time_zone: TimeZone::get("Asia/Shanghai")?,
        },
    )?;
    assert!(!stdlib_edit.is_blocked());

    let stdlib_patch = evaluate_tool_guards(
        &store,
        &GuardCheckInput {
            tool_name: "apply_patch".into(),
            tool_input: json!({"patch": "*** Begin Patch\n*** Update File: src/stdlib.rs\n@@\n*** End Patch"}),
            cwd: None,
            viewer: alice.clone(),
            at: NOV_19_2026_UTC,
            time_zone: TimeZone::get("Asia/Shanghai")?,
        },
    )?;
    assert!(!stdlib_patch.is_blocked());

    // While `src/lib.rs` (via inner `tool_input`, outer hook payload, `MultiEdit`, and `apply_patch`) DOES match!
    for (tool_name, input_json) in [
        ("Edit", json!({"file_path": "src/lib.rs"})),
        (
            "Write",
            json!({"tool_name": "Write", "tool_input": {"path": "crates/core/src/lib.rs"}}),
        ),
        (
            "MultiEdit",
            json!({"edits": [{"file_path": "src/stdlib.rs"}, {"file_path": "src/lib.rs"}]}),
        ),
        (
            "apply_patch",
            json!({"patch": "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n"}),
        ),
    ] {
        let verdict = evaluate_tool_guards(
            &store,
            &GuardCheckInput {
                tool_name: tool_name.into(),
                tool_input: input_json,
                cwd: None,
                viewer: alice.clone(),
                at: NOV_19_2026_UTC,
                time_zone: TimeZone::get("Asia/Shanghai")?,
            },
        )?;
        assert!(
            verdict.is_blocked(),
            "expected {tool_name} on src/lib.rs to match"
        );
        assert_eq!(verdict.matched_fact_ids(), vec![lib_guard_id.clone()]);
    }

    Ok(())
}

#[test]
fn collision_warning_premises_relation_group_code_anchors_and_untrusted_isolation() -> TestResult {
    let (_dir, mut store) = test_store()?;
    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;
    let carol: ActorId = "carol".parse()?;
    let dave: ActorId = "dave".parse()?;

    let dba_scope: SourceScopeId = "chat.scope.dba".parse()?;
    store.upsert_source_scope(&SourceScope {
        scope_id: dba_scope.clone(),
        provider: "chat".into(),
        external_container_id: "oc_dba".into(),
        label: "数据库值班群".into(),
        current_audience: AudienceSpec::Explicit(
            [alice.clone(), bob.clone(), dave.clone()]
                .into_iter()
                .collect(),
        ),
        synced_at: NOV_19_2026_UTC,
    })?;

    // Concept lexicon: group `cpt_dba_group` and action `cpt_db_migrate`.
    let cpt_group = CommunityConcept {
        id: new_concept_id()?,
        kind: ConceptKind::Group,
        canonical_label: "DBA值班组".into(),
        domain: "ops".into(),
        polarity: ConceptPolarity::Neutral,
        proposed_by: bob.clone(),
        created_at: NOV_19_2026_UTC,
    };
    let cpt_migrate = CommunityConcept {
        id: new_concept_id()?,
        kind: ConceptKind::Action,
        canonical_label: "执行数据库迁移".into(),
        domain: "ops".into(),
        polarity: ConceptPolarity::Negative,
        proposed_by: bob.clone(),
        created_at: NOV_19_2026_UTC,
    };
    store.upsert_community_concept(&cpt_group)?;
    store.upsert_community_concept(&cpt_migrate)?;

    // MembershipRelation: `alice` IS in `bob`'s `cpt_dba_group`; `dave` is NOT (and `carol` cannot even read the scope).
    let mem_fact = make_base_fact(
        new_fact_id()?,
        FactKind::Membership,
        "bob",
        &["alice", "bob", "dave"],
        "dba_roster",
        serde_json::to_value(MembershipRelation {
            anchor: bob.clone(),
            member: alice.clone(),
            group_concept_id: cpt_group.id.clone(),
        })?,
        None,
    )?;
    store.insert_fact(&mem_fact)?;

    // Supporting premise 1 (`Event`, visible to `{alice, bob, dave}`): cutover window.
    let evt_id = new_fact_id()?;
    let mut cutover_evt = make_base_fact(
        evt_id.clone(),
        FactKind::Event,
        "alice",
        &["alice", "bob", "dave"],
        "主从切换排期",
        json!({"claim_text": "周四凌晨两点进行账务库主从切换"}),
        Some("周四凌晨两点进行账务库主从切换"),
    )?;
    cutover_evt.source_scope_id = Some(dba_scope.clone());
    store.insert_fact(&cutover_evt)?;

    // Supporting premise 2 (`Constraint`, visible to `{alice, bob, dave}`, scoped to `RelationGroup(bob, cpt_dba_group)`
    // and carrying `action_concept_id = cpt_migrate` without inline CLI strings!).
    let rule_id = new_fact_id()?;
    let mut migrate_rule = make_base_fact(
        rule_id.clone(),
        FactKind::Constraint,
        "bob",
        &["alice", "bob", "dave"],
        "切换期约束",
        json!({
            "claim_text": "切换窗口内严禁变更表结构",
            "action_concept_id": cpt_migrate.id,
            "forbidden_commands": ["sqlx migrate"],
        }),
        Some("切换窗口内严禁变更表结构"),
    )?;
    migrate_rule.source_scope_id = Some(dba_scope);
    migrate_rule.governed_scope = Some(GovernedScope::RelationGroup {
        anchor: bob.clone(),
        group_concept_id: cpt_group.id.clone(),
    });
    store.insert_fact(&migrate_rule)?;

    // Extra private premise (`bob`-only) in a second justification for the same `CollisionWarning`:
    // `alice` MUST NEVER see `private_premise` in `formatted_reason`!
    let private_id = new_fact_id()?;
    let private_prem = make_base_fact(
        private_id.clone(),
        FactKind::Observation,
        "bob",
        &["bob"],
        "内部备注",
        json!({"claim_text": "BOB_PRIVATE_PREMISE_CANARY"}),
        Some("BOB_PRIVATE_PREMISE_CANARY"),
    )?;
    store.insert_fact(&private_prem)?;

    let mut warning = derived_fact(
        store.library_id(),
        &bob,
        FactKind::CollisionWarning,
        "alice",
        "kp.core.collision",
        json!({"claim_text": "主从切换期间存在高危冲突"}),
        NOV_19_2026_UTC,
    )?;
    warning.id = new_fact_id()?;
    warning.logical_key = warning.id.as_str().parse()?;
    store.insert_fact(&warning)?;

    // Justification 1: supported by `{cutover_evt, migrate_rule, mem_fact}` (visible to `alice` and `bob`).
    store.insert_justification(&Justification {
        id: new_justification_id()?,
        conclusion: warning.id.clone(),
        mode: DerivationMode::HardRule,
        rule_id: "R_COLLISION".into(),
        rule_version: "1".into(),
        premises: vec![
            Premise {
                fact_id: cutover_evt.id.clone(),
                strong_ref: cutover_evt.strong_ref(),
            },
            Premise {
                fact_id: migrate_rule.id.clone(),
                strong_ref: migrate_rule.strong_ref(),
            },
            Premise {
                fact_id: mem_fact.id.clone(),
                strong_ref: mem_fact.strong_ref(),
            },
        ],
        active: true,
        created_at: NOV_19_2026_UTC,
        invalidated_at: None,
        invalidation_reason: None,
    })?;

    // Justification 2: includes `private_prem` (visible ONLY to `bob`, invisible to `alice`).
    store.insert_justification(&Justification {
        id: new_justification_id()?,
        conclusion: warning.id.clone(),
        mode: DerivationMode::HardRule,
        rule_id: "R_COLLISION_PRIV".into(),
        rule_version: "1".into(),
        premises: vec![
            Premise {
                fact_id: cutover_evt.id.clone(),
                strong_ref: cutover_evt.strong_ref(),
            },
            Premise {
                fact_id: private_prem.id.clone(),
                strong_ref: private_prem.strong_ref(),
            },
        ],
        active: true,
        created_at: NOV_19_2026_UTC,
        invalidated_at: None,
        invalidation_reason: None,
    })?;

    // `TrustTier::ExternalUntrusted` rule MUST be ignored completely!
    let mut untrusted_rule = make_base_fact(
        new_fact_id()?,
        FactKind::Constraint,
        "alice",
        &["alice", "bob", "carol", "dave"],
        "src/untrusted.rs",
        json!({"hard_guard": true, "reason": "UNTRUSTED_INJECTION_CANARY"}),
        None,
    )?;
    untrusted_rule.trust_tier = TrustTier::ExternalUntrusted;
    store.insert_fact(&untrusted_rule)?;
    assert!(!evaluate_tool_guards(
        &store,
        &GuardCheckInput {
            tool_name: "Edit".into(),
            tool_input: json!({"file_path": "src/untrusted.rs"}),
            cwd: None,
            viewer: alice.clone(),
            at: NOV_19_2026_UTC,
            time_zone: TimeZone::get("Asia/Shanghai")?,
        },
    )?
    .is_blocked());

    // `code_anchors`: `scoped -> anchored` respected (`src/anchored.rs` triggers ONLY inside its repo/file).
    let repo = tempfile::tempdir()?;
    std::fs::create_dir_all(repo.path().join("src"))?;
    std::fs::write(repo.path().join("src/anchored.rs"), "pub fn settle() {}\n")?;
    let canon_repo = repo.path().canonicalize()?;
    let anchored_id = new_fact_id()?;
    let anchored_fact = make_base_fact(
        anchored_id.clone(),
        FactKind::Constraint,
        "alice",
        &["alice"],
        "settle",
        json!({"hard_guard": true, "reason": "anchored contract guard"}),
        None,
    )?;
    store.remember_anchored(
        FactInput {
            fact: anchored_fact,
            entities: BTreeSet::new(),
        },
        &CodeLocus {
            repo_root: canon_repo.to_string_lossy().into_owned(),
            file_path: "src/anchored.rs".into(),
            language: "rust".into(),
            symbol_name: "settle".into(),
            sig_hash: ContentHash::digest("sig"),
            body_hash: ContentHash::digest("body"),
            start_line: 1,
            end_line: 1,
        },
    )?;

    // Editing another file in the repo does NOT trigger the anchored guard.
    assert!(!evaluate_tool_guards(
        &store,
        &GuardCheckInput {
            tool_name: "Edit".into(),
            tool_input: json!({"file_path": "src/other.rs"}),
            cwd: Some(canon_repo.to_string_lossy().into_owned()),
            viewer: alice.clone(),
            at: NOV_19_2026_UTC,
            time_zone: TimeZone::get("Asia/Shanghai")?,
        },
    )?
    .is_blocked());
    // Editing `src/anchored.rs` inside `canon_repo` DOES trigger the anchored guard.
    let anch_verdict = evaluate_tool_guards(
        &store,
        &GuardCheckInput {
            tool_name: "Edit".into(),
            tool_input: json!({"file_path": "src/anchored.rs"}),
            cwd: Some(canon_repo.to_string_lossy().into_owned()),
            viewer: alice.clone(),
            at: NOV_19_2026_UTC,
            time_zone: TimeZone::get("Asia/Shanghai")?,
        },
    )?;
    assert_eq!(anch_verdict.matched_fact_ids(), vec![anchored_id]);

    // Now evaluate `sqlx migrate run` across `alice`, `dave`, `carol`, and an unknown outsider!
    let check_migrate = |viewer: &ActorId| -> TestResult<GuardVerdict> {
        Ok(evaluate_tool_guards(
            &store,
            &GuardCheckInput {
                tool_name: "exec_command".into(),
                tool_input: json!({"cmd": "sqlx migrate run"}),
                cwd: None,
                viewer: viewer.clone(),
                at: NOV_19_2026_UTC,
                time_zone: TimeZone::get("Asia/Shanghai")?,
            },
        )?)
    };

    // 1) `alice` (member of `cpt_dba_group`) matches via `CollisionWarning` + `action_concept_id`,
    //    citing `  ↳ [YYYY-MM-DD · speaker @ source · kp-ref:fact_...]` for visible premises ONLY,
    //    with ZERO leak of `BOB_PRIVATE_PREMISE_CANARY`!
    let alice_verdict = check_migrate(&alice)?;
    assert!(alice_verdict.is_blocked());
    let reason = alice_verdict.joined_reason();
    assert!(
        reason.contains(&format!(
            "  ↳ [2026-11-19 · alice @ 数据库值班群 · kp-ref:{evt_id}]"
        )),
        "missing event premise citation: {reason}"
    );
    assert!(
        reason.contains(&format!(
            "  ↳ [2026-11-19 · bob @ 数据库值班群 · kp-ref:{rule_id}]"
        )),
        "missing rule premise citation: {reason}"
    );
    assert!(
        reason.contains("alice 属于 bob 的「DBA值班组」") && !reason.contains("group_concept_id"),
        "expected human-readable membership premise without raw JSON: {reason}"
    );
    assert!(
        !reason.contains("BOB_PRIVATE_PREMISE_CANARY") && !reason.contains(private_id.as_str()),
        "leaked private premise to alice: {reason}"
    );

    // 2) `dave` can read the group chat, but is NOT in `MembershipRelation(bob, _, cpt_dba_group)` -> NOT blocked!
    assert!(!check_migrate(&dave)?.is_blocked());

    // 3) `carol` is outside `dba_scope` -> NOT blocked, zero leak!
    let carol_verdict = check_migrate(&carol)?;
    assert!(!carol_verdict.is_blocked());
    assert!(carol_verdict.joined_reason().is_empty());

    // 4) Non-member actor -> NOT blocked, zero leak!
    let outsider: ActorId = "outsider".parse()?;
    assert!(!check_migrate(&outsider)?.is_blocked());

    Ok(())
}

#[test]
fn cross_language_prohibitive_and_declarative_required_flag_guards_block_bad_invocations_and_allow_replacements(
) -> TestResult {
    let (_dir, mut store) = test_store()?;
    let alice: ActorId = "alice".parse()?;

    let pip_rule = make_base_fact(
        new_fact_id()?,
        FactKind::Constraint,
        "alice",
        &["alice"],
        "python-packaging",
        json!({
            "summary": "严禁使用 pip install，必须改用 uv pip install",
            "hard_guard": true,
            "forbidden_commands": ["pip install"],
            "allowed_commands": ["uv pip install"],
            "required_flags": []
        }),
        Some("严禁使用 `pip install`，必须改用 `uv pip install`"),
    )?;
    let npm_rule = make_base_fact(
        new_fact_id()?,
        FactKind::Decision,
        "alice",
        &["alice"],
        "js-packaging",
        json!({
            "summary": "Use pnpm install instead of npm install",
            "hard_guard": true,
            "forbidden_commands": ["npm install"],
            "allowed_commands": ["pnpm install"],
            "required_flags": []
        }),
        Some("Use `pnpm install` instead of `npm install`"),
    )?;
    let pytest_flag_rule = make_base_fact(
        new_fact_id()?,
        FactKind::Constraint,
        "alice",
        &["alice"],
        "pytest-live-gate",
        json!({
            "summary": "Run pytest with -m filter so live API tests stay isolated",
            "forbidden_commands": ["pytest"],
            "required_flags": ["-m"]
        }),
        Some("Never run pytest without -m marker filter"),
    )?;
    let prisma_flag_rule = make_base_fact(
        new_fact_id()?,
        FactKind::Constraint,
        "alice",
        &["alice"],
        "prisma-schema-gate",
        json!({
            "summary": "Require --schema on prisma migrate deploy",
            "command": "prisma migrate deploy",
            "required_flags": ["--schema"]
        }),
        Some("Always pass --schema to prisma migrate deploy"),
    )?;
    let prose_only_rule = make_base_fact(
        new_fact_id()?,
        FactKind::Constraint,
        "alice",
        &["alice"],
        "untagged-prose-constraint",
        json!({"summary": "严禁使用 cargo publish 发布未签名包"}),
        Some("严禁使用 `cargo publish` 发布未签名包"),
    )?;
    for f in [
        pip_rule.clone(),
        npm_rule.clone(),
        pytest_flag_rule.clone(),
        prisma_flag_rule.clone(),
        prose_only_rule,
    ] {
        store.remember(&[FactInput {
            fact: f,
            entities: BTreeSet::new(),
        }])?;
    }

    let check_cmd = |cmd: &str| -> TestResult<GuardVerdict> {
        Ok(evaluate_tool_guards(
            &store,
            &GuardCheckInput {
                tool_name: "Bash".into(),
                tool_input: json!({"command": cmd}),
                cwd: None,
                viewer: alice.clone(),
                at: NOV_19_2026_UTC,
                time_zone: TimeZone::UTC,
            },
        )?)
    };

    assert_eq!(
        check_cmd("pip install requests")?.matched_fact_ids(),
        vec![pip_rule.id.clone()]
    );
    assert_eq!(
        check_cmd("PIP_NO_CACHE_DIR=1 sh -c 'pip install requests'")?.matched_fact_ids(),
        vec![pip_rule.id]
    );
    assert!(!check_cmd("uv pip install requests")?.is_blocked());

    assert_eq!(
        check_cmd("sudo npm install lodash")?.matched_fact_ids(),
        vec![npm_rule.id]
    );
    assert!(!check_cmd("pnpm install lodash")?.is_blocked());

    assert_eq!(
        check_cmd("pytest tests/unit")?.matched_fact_ids(),
        vec![pytest_flag_rule.id]
    );
    assert!(!check_cmd("pytest -m 'not live' tests/unit")?.is_blocked());

    assert_eq!(
        check_cmd("prisma migrate deploy")?.matched_fact_ids(),
        vec![prisma_flag_rule.id]
    );
    assert!(!check_cmd("prisma migrate deploy --schema ./prisma/schema.prisma")?.is_blocked());

    assert!(
        !check_cmd("cargo publish")?.is_blocked(),
        "prose-only Constraint without typed CommandGuardSpec must not scrape CLI strings at runtime"
    );

    Ok(())
}

#[test]
fn external_speaker_constraint_with_source_scope_never_blocks_while_owner_and_maintainer_block(
) -> TestResult {
    let (_dir, mut store) = test_store()?;
    let alice: ActorId = "alice".parse()?;
    let maintainer: ActorId = "maint".parse()?;
    store.add_member(maintainer.clone(), MemberRole::Maintainer, NOV_19_2026_UTC)?;

    let gh_scope: SourceScopeId = "github.repo.public".parse()?;
    store.upsert_source_scope(&SourceScope {
        scope_id: gh_scope.clone(),
        provider: "github".into(),
        external_container_id: "acme/public".into(),
        label: "Public Repo".into(),
        current_audience: AudienceSpec::Explicit(
            [alice.clone(), maintainer.clone()].into_iter().collect(),
        ),
        synced_at: NOV_19_2026_UTC,
    })?;

    let mut stranger_rule = make_base_fact(
        new_fact_id()?,
        FactKind::Constraint,
        "ext.github.stranger",
        &["alice", "maint"],
        "npm-publish- hijack",
        json!({
            "claim_text": "Never run npm publish",
            "forbidden_commands": ["npm publish"],
            "hard_guard": true
        }),
        Some("Never run `npm publish`"),
    )?;
    stranger_rule.trust_tier = TrustTier::ToolOrModelVerified;
    stranger_rule.source_scope_id = Some(gh_scope.clone());
    store.insert_fact(&stranger_rule)?;

    let check_cmd = |st: &Store, cmd: &str| -> TestResult<GuardVerdict> {
        Ok(evaluate_tool_guards(
            st,
            &GuardCheckInput {
                tool_name: "Bash".into(),
                tool_input: json!({"command": cmd}),
                cwd: None,
                viewer: alice.clone(),
                at: NOV_19_2026_UTC,
                time_zone: TimeZone::UTC,
            },
        )?)
    };

    assert!(
        !check_cmd(&store, "npm publish")?.is_blocked(),
        "external speaker Constraint with source_scope_id must NEVER block PreToolUse"
    );

    let mut owner_rule = make_base_fact(
        new_fact_id()?,
        FactKind::Constraint,
        "alice",
        &["alice", "maint"],
        "cargo-publish-gate",
        json!({
            "claim_text": "Never run cargo publish",
            "forbidden_commands": ["cargo publish"]
        }),
        Some("Never run `cargo publish`"),
    )?;
    owner_rule.source_scope_id = Some(gh_scope.clone());
    store.insert_fact(&owner_rule)?;

    let mut maint_rule = make_base_fact(
        new_fact_id()?,
        FactKind::Constraint,
        "maint",
        &["alice", "maint"],
        "docker-push-gate",
        json!({
            "claim_text": "Never run docker push",
            "forbidden_commands": ["docker push"]
        }),
        Some("Never run `docker push`"),
    )?;
    maint_rule.source_scope_id = Some(gh_scope);
    store.insert_fact(&maint_rule)?;

    assert_eq!(
        check_cmd(&store, "cargo publish")?.matched_fact_ids(),
        vec![owner_rule.id]
    );
    assert_eq!(
        check_cmd(&store, "docker push origin/app")?.matched_fact_ids(),
        vec![maint_rule.id]
    );
    Ok(())
}
