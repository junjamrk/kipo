use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn run(path: &Path, args: &[&str]) -> Result<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_kp"))
        .env("KP_OFFLINE", "1")
        .arg("--path")
        .arg(path)
        .args(args)
        .output()?)
}

fn success(output: Output) -> Result<Value> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    Ok(serde_json::from_slice(&output.stdout)?)
}

#[test]
fn init_status_and_member_commands_emit_canonical_json() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let initialized = success(run(
        dir.path(),
        &[
            "init",
            "--name",
            "Research",
            "--owner",
            "alice",
            "--community",
            "--json",
        ],
    )?)?;
    assert_eq!(initialized["mode"], "community");
    assert_eq!(initialized["owner_actor"], "alice");
    assert_eq!(initialized["members"], 1);
    assert_eq!(
        success(run(dir.path(), &["status", "--json"])?)?,
        initialized
    );
    let member = success(run(
        dir.path(),
        &["member", "add", "bob", "--role", "maintainer", "--json"],
    )?)?;
    assert_eq!(member["actor_id"], "bob");
    assert_eq!(member["role"], "maintainer");
    let token = member["token"].as_str().ok_or("missing token")?;
    assert!(token.starts_with("kp_"));
    let status = run(dir.path(), &["status", "--json"])?;
    assert!(!String::from_utf8_lossy(&status.stdout).contains(token));
    assert_eq!(success(status)?["members"], 2);
    let duplicate = run(dir.path(), &["member", "add", "bob", "--json"])?;
    assert!(!duplicate.status.success());
    assert!(duplicate.stdout.is_empty());
    let error: Value = serde_json::from_slice(&duplicate.stderr)?;
    assert!(error["error"].is_string());
    assert!(!String::from_utf8_lossy(&duplicate.stderr).contains(token));
    Ok(())
}

#[test]
fn missing_library_errors_do_not_create_state() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("missing");
    let output = run(&path, &["status", "--json"])?;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(serde_json::from_slice::<Value>(&output.stderr)?["error"].is_string());
    assert!(!path.exists());
    Ok(())
}

#[test]
fn standalone_binary_initializes_pure_sqlite_without_git_or_tools_on_path() -> Result<()> {
    let dir = tempfile::tempdir()?;
    for args in [vec!["init", "--json"], vec!["status", "--json"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_kp"))
            .env("KP_OFFLINE", "1")
            .env("PATH", dir.path().join("missing-bin"))
            .arg("--path")
            .arg(dir.path())
            .args(args)
            .output()?;
        assert_eq!(success(output)?["mode"], "personal");
    }
    assert!(dir.path().join("kp.sqlite").is_file());
    assert!(!dir.path().join("raw").exists());
    Ok(())
}

#[test]
fn failed_initialization_cleans_new_files_and_can_be_retried() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let output = Command::new(env!("CARGO_BIN_EXE_kp"))
        .args(["init", "--name", "   ", "--json", "--path"])
        .arg(dir.path())
        .output()?;
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stderr)?;
    assert!(!error["error"].as_str().ok_or("missing error")?.is_empty());
    for name in ["kp.sqlite", "kp.sqlite-wal", "kp.sqlite-shm", "raw"] {
        assert!(
            !dir.path().join(name).exists(),
            "initialization left {name}"
        );
    }
    assert_eq!(
        success(run(dir.path(), &["init", "--json"])?)?["mode"],
        "personal"
    );
    assert!(!dir.path().join("raw").exists());
    Ok(())
}

#[test]
fn initialization_ignores_host_git_configuration_and_creates_no_raw_repo() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let host = dir.path().join("host.gitconfig");
    let contents =
        "[user]\n name = Host User\n[commit]\n gpgsign = true\n[init]\n defaultBranch = hostile\n";
    fs::write(&host, contents)?;
    let path = dir.path().join("library");
    let redirect = dir.path().join("redirected.git");
    let output = Command::new(env!("CARGO_BIN_EXE_kp"))
        .env("KP_OFFLINE", "1")
        .env("GIT_CONFIG_GLOBAL", &host)
        .env("GIT_DIR", &redirect)
        .env("GIT_WORK_TREE", dir.path())
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "init.defaultBranch")
        .env("GIT_CONFIG_VALUE_0", "wrong")
        .args(["init", "--json", "--path"])
        .arg(&path)
        .output()?;
    assert_eq!(success(output)?["mode"], "personal");
    assert_eq!(fs::read_to_string(host)?, contents);
    assert!(!redirect.exists());
    assert!(!path.join("raw").exists());
    Ok(())
}

#[test]
fn memory_funnel_search_forget_and_purge_work_end_to_end() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("library");
    success(run(&path, &["init", "--owner", "alice", "--json"])?)?;
    let input = dir.path().join("travel.md");
    fs::write(
        &input,
        "# Travel\nconstraint: 浓雾天能见度低绝不自驾出行，改乘高铁。\n",
    )?;
    let added = success(run(
        &path,
        &[
            "add",
            input.to_str().ok_or("path")?,
            "--extract",
            "--readable-by",
            "alice",
            "--json",
        ],
    )?)?;
    assert_eq!(
        added[0]["extracted"]["facts"]
            .as_array()
            .ok_or("facts")?
            .len(),
        1
    );
    let source_id = added[0]["source"]["source_id"].as_str().ok_or("source")?;
    assert!(kp_store::Store::open_readonly(&path)?
        .raw_source_bytes(&source_id.parse()?)?
        .is_some());
    assert!(!path.join("raw").exists());
    let memory = success(run(
        &path,
        &[
            "remember",
            "--subject",
            "杭州",
            "--predicate",
            "kp.core.weather",
            "--value",
            "周五傍晚杭州有大雾，浓雾预警",
            "--logical-key",
            "forecast",
            "--valid-from",
            "0",
            "--audience",
            "alice",
            "--as",
            "alice",
            "--json",
        ],
    )?)?;
    assert_eq!(memory["facts"].as_array().ok_or("facts")?.len(), 1);
    let results = success(run(
        &path,
        &[
            "search",
            "周五还能自驾去杭州露营吗",
            "--deep",
            "--limit",
            "20",
            "--as",
            "alice",
            "--json",
        ],
    )?)?;
    let text = results.to_string();
    assert!(text.contains("浓雾预警"));
    assert!(text.contains("高铁"));
    assert!(results["trace"].is_array());
    let private = success(run(
        &path,
        &["search", "杭州", "--deep", "--as", "bob", "--json"],
    )?)?;
    assert_eq!(private["results"], serde_json::json!([]));
    success(run(
        &path,
        &[
            "forget", "forecast", "--at", "100", "--as", "alice", "--json",
        ],
    )?)?;
    let after = success(run(
        &path,
        &["search", "浓雾预警", "--as", "alice", "--json"],
    )?)?;
    assert!(!after.to_string().contains("周五傍晚"));
    success(run(
        &path,
        &[
            "purge", "forecast", "--reason", "test", "--as", "alice", "--json",
        ],
    )?)?;
    success(run(
        &path,
        &["purge", source_id, "--as", "alice", "--json"],
    )?)?;
    assert!(kp_store::Store::open_readonly(&path)?
        .raw_source_bytes(&source_id.parse()?)?
        .is_none());
    assert_eq!(
        kp_store::find_sqlite_table_containing_bytes(&path, "浓雾天".as_bytes())?,
        None
    );
    assert_eq!(success(run(&path, &["status", "--json"])?)?["facts"], 0);
    assert_eq!(
        success(run(&path, &["search", "高铁", "--deep", "--json"])?)?["results"],
        serde_json::json!([])
    );
    Ok(())
}

#[test]
fn search_at_returns_old_and_new_slot_values_and_federates_libraries() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let personal = dir.path().join("personal");
    let community = dir.path().join("community");
    success(run(&personal, &["init", "--owner", "alice", "--json"])?)?;
    success(run(
        &community,
        &["init", "--owner", "alice", "--community", "--json"],
    )?)?;
    for (value, from) in [("rainy", "10"), ("sunny", "20")] {
        success(run(
            &personal,
            &[
                "remember",
                "--subject",
                "杭州",
                "--predicate",
                "kp.core.weather",
                "--value",
                value,
                "--valid-from",
                from,
                "--json",
            ],
        )?)?;
    }
    let old = success(run(&personal, &["search", "杭州", "--at", "15", "--json"])?)?;
    assert!(old.to_string().contains("rainy"));
    assert!(!old.to_string().contains("sunny"));
    let new = success(run(&personal, &["search", "杭州", "--at", "25", "--json"])?)?;
    assert!(new.to_string().contains("sunny"));
    assert!(!new.to_string().contains("rainy"));
    success(run(
        &community,
        &[
            "remember",
            "--subject",
            "杭州",
            "--predicate",
            "advice",
            "--value",
            "community train advice",
            "--valid-from",
            "0",
            "--audience",
            "*",
            "--json",
        ],
    )?)?;
    let merged = success(run(
        &personal,
        &[
            "search",
            "杭州",
            "--at",
            "25",
            "--also-library",
            community.to_str().ok_or("path")?,
            "--json",
        ],
    )?)?;
    let results = merged["results"].as_array().ok_or("results")?;
    assert_eq!(results.len(), 2);
    assert_ne!(results[0]["library_id"], results[1]["library_id"]);
    let scores = results
        .iter()
        .map(|r| r["score"].as_f64().ok_or("score"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert!(scores.iter().all(|p| (0.0..=1.0).contains(p)));
    assert!(scores.windows(2).all(|s| s[0] >= s[1]));
    let denied = run(
        &personal,
        &[
            "search",
            "杭州",
            "--judge-tier",
            "cloud-jev",
            "--judge-url",
            "http://127.0.0.1:1/decisions",
            "--json",
        ],
    )?;
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("allow-cloud-jev"));
    Ok(())
}

#[test]
fn m2_derive_synthesize_and_why_commands_use_canonical_json() -> Result<()> {
    let dir = tempfile::tempdir()?;
    success(run(dir.path(), &["init", "--owner", "alice", "--json"])?)?;
    success(run(
        dir.path(),
        &[
            "remember",
            "--subject",
            "offsite",
            "--predicate",
            "kp.core.schedule",
            "--value",
            "rain",
            "--kind",
            "event",
            "--tags",
            "rain",
            "--valid-from",
            "0",
            "--audience",
            "alice,bob",
            "--json",
        ],
    )?)?;
    success(run(
        dir.path(),
        &[
            "remember",
            "--subject",
            "bob",
            "--predicate",
            "kp.core.constraint",
            "--value",
            "take train",
            "--kind",
            "constraint",
            "--tags",
            "rain",
            "--valid-from",
            "0",
            "--audience",
            "alice,bob",
            "--json",
        ],
    )?)?;
    assert_eq!(
        success(run(dir.path(), &["derive", "--json"])?)?["derived"],
        1
    );
    assert_eq!(
        success(run(dir.path(), &["derive", "--json"])?)?["derived"],
        0
    );
    let synthesis = success(run(
        dir.path(),
        &[
            "synthesize",
            "--query",
            "rain travel",
            "--audience",
            "alice,bob",
            "--at",
            "10",
            "--json",
        ],
    )?)?;
    let id = synthesis["facts"][0].as_str().ok_or("missing synthesis")?;
    let proof = success(run(
        dir.path(),
        &["why", id, "--as", "alice", "--at", "10", "--json"],
    )?)?;
    assert_eq!(proof["fact_id"], id);
    assert_eq!(
        success(run(
            dir.path(),
            &["why", id, "--as", "carol", "--at", "10", "--json"]
        )?)?,
        serde_json::Value::Null
    );
    Ok(())
}

#[test]
fn synthesize_as_uses_the_caller_instead_of_first_audience_member() -> Result<()> {
    let dir = tempfile::tempdir()?;
    success(run(dir.path(), &["init", "--owner", "alice", "--json"])?)?;
    success(run(
        dir.path(),
        &[
            "remember",
            "--subject",
            "weather",
            "--predicate",
            "kp.core.weather",
            "--value",
            "rain",
            "--audience",
            "alice,bob",
            "--valid-from",
            "0",
            "--json",
        ],
    )?)?;
    let result = success(run(
        dir.path(),
        &[
            "synthesize",
            "--query",
            "weather",
            "--audience",
            "alice,bob",
            "--as",
            "bob",
            "--json",
        ],
    )?)?;
    let id = result["facts"][0].as_str().ok_or("synthesis id")?.parse()?;
    let graph = kp_store::Store::open(dir.path())?.derivation_view()?;
    assert_eq!(graph.facts[&id].author.as_str(), "bob");
    Ok(())
}

#[test]
fn search_finds_a_mirror_that_returns_after_missing_snapshots() -> Result<()> {
    let origin_dir = tempfile::tempdir()?;
    let mirror_dir = tempfile::tempdir()?;
    success(run(
        origin_dir.path(),
        &["init", "--owner", "alice", "--json"],
    )?)?;
    success(run(
        mirror_dir.path(),
        &["init", "--owner", "alice", "--json"],
    )?)?;
    success(run(
        origin_dir.path(),
        &[
            "remember",
            "--subject",
            "weather",
            "--predicate",
            "kp.core.weather",
            "--value",
            "rain",
            "--audience",
            "alice",
            "--valid-from",
            "0",
            "--json",
        ],
    )?)?;
    let mut origin = kp_store::Store::open(origin_dir.path())?;
    let snapshot = origin.change_snapshot(&"alice".parse()?, kp_model::EpochMs(10))?;
    assert_eq!(snapshot.facts.len(), 1);
    let mut mirror = kp_store::Store::open(mirror_dir.path())?;
    mirror.import_snapshot("endpoint", &snapshot, kp_model::EpochMs(10))?;
    let mut empty = snapshot.clone();
    empty.facts.clear();
    mirror.import_snapshot("endpoint", &empty, kp_model::EpochMs(11))?;
    assert_eq!(
        success(run(
            mirror_dir.path(),
            &["search", "rain", "--at", "12", "--json"]
        )?)?["results"],
        serde_json::json!([])
    );
    mirror.import_snapshot("endpoint", &snapshot, kp_model::EpochMs(12))?;
    assert_eq!(
        success(run(
            mirror_dir.path(),
            &["search", "rain", "--at", "13", "--json"]
        )?)?["results"]
            .as_array()
            .ok_or("results")?
            .len(),
        1
    );
    Ok(())
}

#[test]
fn reject_command_withdraws_fact_and_persists_sticky_dual_key_rejection_unlike_forget() -> Result<()>
{
    use kp_model::{
        format_opaque_id, verify_verbatim_quote, AnchorSpec, AudienceSpec, BitemporalStamp,
        CharSpan, ConceptId, ContentHash, EpochMs, Fact, FactAccess, FactId, FactKind, FactRank,
        FactStatus, GovernedScope, QuoteLocationHint, RejectionEvidenceKey, RejectionSemanticKey,
        TrustTier, ValidityWindow,
    };
    use kp_store::{ChunkInput, FactInput, RawSourceInput, Store};
    use std::collections::BTreeSet;

    let dir = tempfile::tempdir()?;
    let lib = dir.path().join("lib");
    success(run(&lib, &["init", "--owner", "alice", "--json"])?)?;
    success(run(&lib, &["member", "add", "bob", "--json"])?)?;

    let raw_body = "差旅讨论：国内出差住宿每晚报销上限 600 元，超标须总监审批。\n";
    let quote = "国内出差住宿每晚报销上限 600 元";
    let verified = verify_verbatim_quote(raw_body, quote, QuoteLocationHint::default())?;

    let mut store = Store::open(&lib)?;
    let stored = store.ingest_source(
        &store.library_id().clone(),
        &"session:alice:reimb-turn-1".parse()?,
        &RawSourceInput {
            self_echo_filtered: false,
            uri: "session:reimb-turn-1".into(),
            media_type: "text/markdown".into(),
            content: raw_body.as_bytes().to_vec(),
            readable_by: AudienceSpec::Explicit(BTreeSet::from(["alice".parse()?])),
            captured_at: EpochMs(100),
            chunks: vec![ChunkInput {
                heading: "Turn 1".into(),
                body: raw_body.into(),
                anchor: AnchorSpec::default(),
            }],
        },
    )?;
    let chunk_id = format!("{}-0", stored.source_id);
    let fid = FactId::new(format_opaque_id("fact", "hotel_cap_wrong_01")?)?;
    let cond_cid: ConceptId = "cpt_domestic_trip".parse()?;
    let act_cid: ConceptId = "cpt_hotel_600".parse()?;
    let window = ValidityWindow {
        valid_from: EpochMs(100),
        valid_until: EpochMs(500),
    };
    let scope = GovernedScope::SelfOnly {
        actor: "alice".parse()?,
    };
    store.remember(&[FactInput {
        fact: Fact {
            id: fid.clone(),
            origin_library: store.library_id().clone(),
            logical_key: fid.as_str().parse()?,
            kind: FactKind::Constraint,
            subject: "差旅住宿".into(),
            predicate: "kp.core.constraint".into(),
            object: serde_json::json!({
                "text": "住宿每晚报销上限 600 元",
                "condition_concept_id": cond_cid,
                "action_concept_id": act_cid,
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
                    valid: window,
                    recorded_at: EpochMs(100),
                    expired_at: None,
                },
            },
            source_scope_id: None,
            governed_scope: Some(scope.clone()),
            broader_scope: None,
            status: FactStatus::Active,
            superseded_by: None,
            rank: FactRank::Normal,
            source_anchor: Some(AnchorSpec {
                source_id: Some(stored.source_id),
                chunk_id: Some(chunk_id),
                verbatim_quote: Some(verified.exact_quote),
                char_start: Some(verified.span.char_start),
                char_end: Some(verified.span.char_end),
                ..AnchorSpec::default()
            }),
            unverified_gaps: Vec::new(),
            miss_count: 0,
        },
        entities: BTreeSet::new(),
    }])?;
    drop(store);

    // Non-author regular member `bob` cannot reject `alice`'s fact.
    let denied = run(
        &lib,
        &[
            "--as",
            "bob",
            "reject",
            fid.as_str(),
            "--reason",
            "unauthorized",
            "--at",
            "200",
            "--json",
        ],
    )?;
    assert!(!denied.status.success());

    // `alice` marks "这条错了" via `kp reject`.
    let rejected = success(run(
        &lib,
        &[
            "--as",
            "alice",
            "reject",
            fid.as_str(),
            "--reason",
            "这条错了，一线城市上限已调至 800 元",
            "--at",
            "200",
            "--json",
        ],
    )?)?;
    let rej_id = rejected["rejection_id"].as_str().ok_or("rejection_id")?;
    assert!(rej_id.starts_with("rej_"), "expected rej_ prefix: {rej_id}");
    assert_eq!(rejected["withdrawn"], serde_json::json!([fid.as_str()]));

    // Verify L1 fact is withdrawn while L0 raw chunk remains immutable (motivating sticky rejected_claims).
    let search_after = success(run(
        &lib,
        &[
            "search",
            "住宿每晚报销上限",
            "--at",
            "250",
            "--as",
            "alice",
            "--json",
        ],
    )?)?;
    let after_hits = search_after["results"].as_array().ok_or("results")?;
    assert!(after_hits
        .iter()
        .all(|r| r["kind"] == "chunk"
            && r["id"].as_str().is_none_or(|id| !id.contains(fid.as_str()))));
    assert_eq!(
        success(run(
            &lib,
            &[
                "why",
                fid.as_str(),
                "--at",
                "250",
                "--as",
                "alice",
                "--json"
            ]
        )?)?,
        serde_json::Value::Null
    );

    let reopened = Store::open(&lib)?;
    let stored_rejs = reopened.list_rejections(None)?;
    assert_eq!(stored_rejs.len(), 1);
    let record = &stored_rejs[0];
    record.validate()?;
    assert!(record.evidence_key.is_some() && record.semantic_key.is_some());

    // Reworded LLM claim on an overlapping character span of the same chunk MUST be blocked.
    let overlapping_candidate = RejectionEvidenceKey {
        speaker: "alice".parse()?,
        chunk_sha256: ContentHash::digest(raw_body),
        quote_span: CharSpan::new(verified.span.char_start + 2, verified.span.char_end - 1)?,
    };
    assert!(record.matches_candidate(Some(&overlapping_candidate), None));

    // Re-extracted candidate on a different chunk with same canonical semantic key MUST also be blocked.
    let same_semantic = RejectionSemanticKey {
        kind: FactKind::Constraint,
        concept_ids: BTreeSet::from([cond_cid, act_cid]),
        governed_scope: Some(scope),
        valid_window: window,
    };
    assert!(record.matches_candidate(None, Some(&same_semantic)));
    drop(reopened);

    // Contrast with `kp forget`: withdraws fact without adding to `rejected_claims`, allowing future re-learning!
    let forgettable = success(run(
        &lib,
        &[
            "remember",
            "--subject",
            "temp-note",
            "--predicate",
            "kp.core.note",
            "--value",
            "temporary preference",
            "--valid-from",
            "100",
            "--as",
            "alice",
            "--json",
        ],
    )?)?;
    let forget_id = forgettable["facts"][0].as_str().ok_or("forget_id")?;
    let rej_count_before = kp_store::Store::open_readonly(&lib)?
        .list_rejections(None)?
        .len();
    success(run(
        &lib,
        &[
            "forget", forget_id, "--at", "210", "--as", "alice", "--json",
        ],
    )?)?;
    let rej_count_after = kp_store::Store::open_readonly(&lib)?
        .list_rejections(None)?
        .len();
    assert_eq!(rej_count_before, rej_count_after);
    let relearned = success(run(
        &lib,
        &[
            "remember",
            "--subject",
            "temp-note",
            "--predicate",
            "kp.core.note",
            "--value",
            "temporary preference",
            "--valid-from",
            "220",
            "--as",
            "alice",
            "--json",
        ],
    )?)?;
    assert_eq!(relearned["facts"].as_array().ok_or("relearned")?.len(), 1);
    Ok(())
}

#[test]
fn review_list_and_resolve_ambiguous_scope_confirm_narrow_and_dismiss_work_end_to_end() -> Result<()>
{
    use kp_model::{
        format_opaque_id, AudienceSpec, BitemporalStamp, BroaderScopeCandidate, ConceptId, EpochMs,
        Fact, FactAccess, FactId, FactKind, FactRank, FactStatus, GovernedScope,
        MembershipRelation, TrustTier, ValidityWindow,
    };
    use kp_store::{FactInput, Store};
    use std::collections::BTreeSet;

    let dir = tempfile::tempdir()?;
    let lib = dir.path().join("lib");
    success(run(
        &lib,
        &["init", "--owner", "alice", "--community", "--json"],
    )?)?;
    success(run(&lib, &["member", "add", "bob", "--json"])?)?;
    success(run(&lib, &["member", "add", "carol", "--json"])?)?;

    let mut store = Store::open(&lib)?;
    let club_cid: ConceptId = "cpt_reading_club".parse()?;
    let seminar_cid: ConceptId = "cpt_book_seminar".parse()?;
    let no_interview_cid: ConceptId = "cpt_no_external_interview".parse()?;

    // 1. Membership: Alice is in Bob's reading-club group.
    let mem_id = FactId::new(format_opaque_id("fact", "membership_bob_alice_club")?)?;
    store.remember(&[FactInput {
        fact: Fact {
            id: mem_id,
            origin_library: store.library_id().clone(),
            logical_key: "fact_membership_bob_alice_club".parse()?,
            kind: FactKind::Membership,
            subject: "bob".into(),
            predicate: "kp.core.membership".into(),
            object: serde_json::to_value(MembershipRelation {
                anchor: "bob".parse()?,
                member: "alice".parse()?,
                group_concept_id: club_cid.clone(),
            })?,
            tags: BTreeSet::new(),
            author: "bob".parse()?,
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
                    recorded_at: EpochMs(10),
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

    // 2. Thursday book-club seminar event shared between Alice and Bob.
    let ev_id = FactId::new(format_opaque_id("fact", "thursday_seminar_event")?)?;
    store.remember(&[FactInput {
        fact: Fact {
            id: ev_id,
            origin_library: store.library_id().clone(),
            logical_key: "fact_thursday_seminar_event".parse()?,
            kind: FactKind::Event,
            subject: "周四读书会".into(),
            predicate: "kp.core.schedule".into(),
            object: serde_json::json!({
                "claim_text": "周四晚上举办长篇共读研讨会",
                "condition_concept_id": seminar_cid,
            }),
            tags: BTreeSet::from(["seminar_trigger".into()]),
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
                        valid_until: EpochMs(1_000),
                    },
                    recorded_at: EpochMs(20),
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

    // 3. Bob's ambiguous subjectless constraint: defaults to SelfOnly(bob) with audience {bob}.
    let cs_id = FactId::new(format_opaque_id("fact", "bob_seminar_ambiguous")?)?;
    let broader = GovernedScope::RelationGroup {
        anchor: "bob".parse()?,
        group_concept_id: club_cid,
    };
    store.remember(&[FactInput {
        fact: Fact {
            id: cs_id.clone(),
            origin_library: store.library_id().clone(),
            logical_key: cs_id.as_str().parse()?,
            kind: FactKind::Constraint,
            subject: "共读日排期".into(),
            predicate: "kp.core.constraint".into(),
            object: serde_json::json!({
                "claim_text": "共读研讨会当晚不安排外部客户访谈",
                "condition_concept_id": seminar_cid,
                "action_concept_id": no_interview_cid,
            }),
            tags: BTreeSet::from(["seminar_trigger".into()]),
            author: "bob".parse()?,
            ingested_by: None,
            trust_tier: TrustTier::HumanVerbatim,
            access: FactAccess::Base {
                audience: AudienceSpec::Explicit(BTreeSet::from(["bob".parse()?])),
                stamp: BitemporalStamp {
                    valid: ValidityWindow {
                        valid_from: EpochMs(0),
                        valid_until: EpochMs::OPEN_END,
                    },
                    recorded_at: EpochMs(30),
                    expired_at: None,
                },
            },
            source_scope_id: None,
            governed_scope: Some(GovernedScope::SelfOnly {
                actor: "bob".parse()?,
            }),
            broader_scope: Some(BroaderScopeCandidate {
                narrow_active: GovernedScope::SelfOnly {
                    actor: "bob".parse()?,
                },
                broader_candidate: broader.clone(),
                question: "只针对你自己，还是整个读书小组？".into(),
            }),
            status: FactStatus::Active,
            superseded_by: None,
            rank: FactRank::Normal,
            source_anchor: None,
            unverified_gaps: Vec::new(),
            miss_count: 0,
        },
        entities: BTreeSet::new(),
    }])?;
    kp_derive::derive(&mut store)?;
    let bob_actor: kp_model::ActorId = "bob".parse()?;
    store.upsert_open_review_item(&kp_model::ReviewItem {
        id: "rev_amb_01".parse()?,
        actor: bob_actor.clone(),
        kind: kp_model::ReviewKind::AmbiguousScope,
        target_id: cs_id.to_string(),
        question: "只针对你自己，还是整个读书小组？".into(),
        options: vec!["confirm".into(), "narrow".into(), "dismiss".into()],
        payload: serde_json::json!({"broader_candidate": broader}),
        status: kp_model::ReviewStatus::Open,
        created_at: EpochMs(35),
        resolved_at: None,
    })?;
    store.upsert_open_review_item(&kp_model::ReviewItem {
        id: "rev_narrow_02".parse()?,
        actor: bob_actor.clone(),
        kind: kp_model::ReviewKind::AmbiguousScope,
        target_id: "fact_other_narrow_02".into(),
        question: "只针对个人？".into(),
        options: Vec::new(),
        payload: serde_json::json!({}),
        status: kp_model::ReviewStatus::Open,
        created_at: EpochMs(36),
        resolved_at: None,
    })?;
    store.upsert_open_review_item(&kp_model::ReviewItem {
        id: "rev_dismiss_03".parse()?,
        actor: bob_actor,
        kind: kp_model::ReviewKind::ConceptAlignment,
        target_id: "cpt_1".into(),
        question: "是否同义？".into(),
        options: Vec::new(),
        payload: serde_json::json!({}),
        status: kp_model::ReviewStatus::Open,
        created_at: EpochMs(37),
        resolved_at: None,
    })?;
    drop(store);

    // Before Bob confirms: Alice sees ZERO warning/advice from Bob's narrow SelfOnly constraint, and 0 review items!
    let alice_before = success(run(
        &lib,
        &[
            "search",
            "外部客户访谈",
            "--at",
            "50",
            "--as",
            "alice",
            "--json",
        ],
    )?)?;
    assert!(alice_before["results"]
        .as_array()
        .ok_or("results")?
        .is_empty());
    assert_eq!(
        success(run(&lib, &["review", "list", "--as", "alice", "--json"])?)?,
        serde_json::json!([])
    );

    let bob_list = success(run(&lib, &["review", "list", "--as", "bob", "--json"])?)?;
    assert_eq!(bob_list.as_array().ok_or("bob list")?.len(), 3);

    // Bob confirms broader scope (`--choice confirm`): widens scope, supersedes old narrow fact, derives Alice's warning!
    let confirmed = success(run(
        &lib,
        &[
            "review",
            "resolve",
            "rev_amb_01",
            "--choice",
            "confirm",
            "--at",
            "60",
            "--as",
            "bob",
            "--json",
        ],
    )?)?;
    assert_eq!(confirmed["status"], "confirmed");
    let new_fid = confirmed["replacement_fact_id"]
        .as_str()
        .ok_or("replacement_fact_id")?;
    assert!(new_fid.starts_with("fact_"));

    // Now Alice immediately receives the derived collision conclusion, while outsider Carol still sees ZERO leak!
    let alice_after = success(run(
        &lib,
        &[
            "search",
            "外部客户访谈",
            "--at",
            "70",
            "--as",
            "alice",
            "--json",
        ],
    )?)?;
    assert!(!alice_after["results"]
        .as_array()
        .ok_or("results")?
        .is_empty());
    let carol_after = success(run(
        &lib,
        &[
            "search",
            "外部客户访谈",
            "--at",
            "70",
            "--as",
            "carol",
            "--json",
        ],
    )?)?;
    assert!(carol_after["results"]
        .as_array()
        .ok_or("results")?
        .is_empty());

    // Resolve remaining two items with `narrow` and `dismiss`.
    let narrowed = success(run(
        &lib,
        &[
            "review",
            "resolve",
            "rev_narrow_02",
            "--choice",
            "narrow",
            "--at",
            "80",
            "--as",
            "bob",
            "--json",
        ],
    )?)?;
    assert_eq!(narrowed["status"], "confirmed");
    let dismissed = success(run(
        &lib,
        &[
            "review",
            "resolve",
            "rev_dismiss_03",
            "--choice",
            "dismiss",
            "--at",
            "90",
            "--as",
            "bob",
            "--json",
        ],
    )?)?;
    assert_eq!(dismissed["status"], "dismissed");
    assert_eq!(
        success(run(&lib, &["review", "list", "--as", "bob", "--json"])?)?,
        serde_json::json!([])
    );
    Ok(())
}

#[test]
fn m1_session_hook_extract_concept_collision_review_and_sticky_reject_journey() -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir()?;
    let lib = dir.path().join("lib");
    success(run(
        &lib,
        &["init", "--owner", "alice", "--community", "--json"],
    )?)?;
    success(run(&lib, &["member", "add", "bob", "--json"])?)?;
    success(run(&lib, &["member", "add", "carol", "--json"])?)?;

    // Direct contract guard: validate mock extraction wire shape against `kp_derive::prompts::parse_extraction_envelope`.
    let envelope_contract_sample = serde_json::json!({
        "skip_summary": false,
        "atoms": [{
            "kind": "constraint",
            "claim_text": "容灾演练窗口禁止执行 sqlx migrate",
            "value_json": "{}",
            "scope": null,
            "ambiguous_scope": {
                "broader_candidate": {
                    "scope": "relation_group",
                    "mentions": null,
                    "group_concept": {
                        "ref_type": "propose_new",
                        "concept_id": null,
                        "kind": "group",
                        "label": "架构评审组",
                        "domain": "org",
                        "polarity": "neutral"
                    }
                },
                "question": "只针对你自己，还是整个架构评审组？"
            },
            "condition_concept": {
                "ref_type": "propose_new",
                "concept_id": null,
                "kind": "condition",
                "label": "容灾演练窗口",
                "domain": "ops",
                "polarity": "positive"
            },
            "action_concept": {
                "ref_type": "propose_new",
                "concept_id": null,
                "kind": "action",
                "label": "禁止执行 sqlx migrate",
                "domain": "ops",
                "polarity": "negative"
            },
            "group_concept": null,
            "member_mention": null,
            "valid_dates": null,
            "speaker": "bob",
            "assumed_premise_ids": [],
            "quote": {
                "chunk_id": "sample-0",
                "verbatim_quote": "容灾演练窗口禁止执行 sqlx migrate",
                "turn_ordinal": null,
                "approx_char_offset": null
            },
            "write_op": {
                "op": "add",
                "target_fact_id": null
            }
        }]
    });
    let parsed_sample = kp_derive::prompts::parse_extraction_envelope(envelope_contract_sample)?;
    assert_eq!(parsed_sample.atoms.len(), 1);

    // Deterministic mock `claude` CLI binary verifying 4-layer subprocess isolation (`KP_INTERNAL_EXTRACTION=1`,
    // `CLAUDE_CODE_*` stripped, empty cwd) and returning strict-schema structured_output JSON.
    let audit_log = dir.path().join("mock_claude_audit.txt");
    let mock_claude = dir.path().join("mock-claude");
    let script = format!(
        r#"#!/usr/bin/env python3
import json, os, re, sys
prompt = sys.stdin.read()
cwd_entries = os.listdir(os.getcwd())
assert os.environ.get("KP_INTERNAL_EXTRACTION") == "1", "missing KP_INTERNAL_EXTRACTION=1"
assert not any(k == "CLAUDECODE" or k.startswith("CLAUDE_CODE_") for k in os.environ), "leaked CLAUDE_CODE env"
assert cwd_entries == [], f"expected empty cwd jail, got {{cwd_entries}}"
with open({audit_path:?}, "a", encoding="utf-8") as f:
    f.write(f"argv={{sys.argv[1:8]}}\n")

if "<chunk id=" not in prompt:
    out = {{"structured_output": {{"verdict": "entails", "question": None}}}}
    print(json.dumps(out))
    sys.exit(0)

def prop(kind, label, domain, polarity="neutral"):
    return {{"ref_type": "propose_new", "concept_id": None, "kind": kind, "label": label, "domain": domain, "polarity": polarity}}

m = re.search(r'<chunk id="([^"]+)"', prompt)
cid = m.group(1) if m else "unknown-0"
atoms = []
if "alice 是我们架构评审组的核心成员" in prompt:
    atoms.append({{
        "kind": "membership",
        "claim_text": "Alice 属于架构评审组",
        "value_json": "{{}}",
        "scope": None,
        "ambiguous_scope": None,
        "condition_concept": None,
        "action_concept": None,
        "group_concept": prop("group", "架构评审组", "org", "neutral"),
        "member_mention": "alice",
        "valid_dates": None,
        "speaker": "bob",
        "assumed_premise_ids": [],
        "quote": {{"chunk_id": cid, "verbatim_quote": "alice 是我们架构评审组的核心成员", "turn_ordinal": None, "approx_char_offset": None}},
        "write_op": {{"op": "add", "target_fact_id": None}}
    }})
if "演练窗口禁止执行 sqlx migrate" in prompt:
    atoms.append({{
        "kind": "constraint",
        "claim_text": "容灾演练窗口禁止执行 sqlx migrate",
        "value_json": "{{}}",
        "scope": None,
        "ambiguous_scope": {{
            "broader_candidate": {{
                "scope": "relation_group",
                "mentions": None,
                "group_concept": prop("group", "架构评审组", "org", "neutral")
            }},
            "question": "只针对你自己，还是整个架构评审组？"
        }},
        "condition_concept": prop("condition", "容灾演练窗口", "ops", "positive"),
        "action_concept": prop("action", "禁止执行 sqlx migrate", "ops", "negative"),
        "group_concept": None,
        "member_mention": None,
        "valid_dates": None,
        "speaker": "bob",
        "assumed_premise_ids": [],
        "quote": {{"chunk_id": cid, "verbatim_quote": "容灾演练窗口禁止执行 sqlx migrate", "turn_ordinal": None, "approx_char_offset": None}},
        "write_op": {{"op": "add", "target_fact_id": None}},
        "command_guard": {{"forbidden_commands": ["sqlx migrate"], "allowed_commands": [], "required_flags": []}}
    }})
if "下周四全天开启主库故障切换演练" in prompt:
    atoms.append({{
        "kind": "event",
        "claim_text": "下周四开启主库故障切换演练",
        "value_json": "{{}}",
        "scope": None,
        "ambiguous_scope": None,
        "condition_concept": prop("condition", "主库故障切换演练", "ops", "positive"),
        "action_concept": None,
        "group_concept": None,
        "member_mention": None,
        "valid_dates": None,
        "speaker": None,
        "assumed_premise_ids": [],
        "quote": {{"chunk_id": cid, "verbatim_quote": "下周四全天开启主库故障切换演练", "turn_ordinal": None, "approx_char_offset": None}},
        "write_op": {{"op": "add", "target_fact_id": None}},
        "command_guard": None
    }})
print(json.dumps({{"structured_output": {{"skip_summary": False, "atoms": atoms}}}}, ensure_ascii=False))
"#,
        audit_path = audit_log.display().to_string()
    );
    std::fs::write(&mock_claude, script)?;
    std::fs::set_permissions(&mock_claude, std::fs::Permissions::from_mode(0o755))?;

    // 1. SessionEnd hook captures Bob's session transcript incrementally and enqueues extraction job.
    let transcript_path = dir.path().join("bob-session.jsonl");
    let lines = [
        serde_json::json!({"type":"user","message":{"role":"user","content":"容灾演练窗口禁止执行 sqlx migrate。"}}),
        serde_json::json!({"type":"assistant","message":{"role":"assistant","content":"好的，明白。"}}),
    ];
    std::fs::write(
        &transcript_path,
        lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )?;
    let mut hook_cmd = std::process::Command::new(env!("CARGO_BIN_EXE_kp"))
        .env("KP_OFFLINE", "1")
        .env("KP_HOST_CLI_PROGRAM", &mock_claude)
        .arg("--path")
        .arg(&lib)
        .args(["--as", "bob", "hook", "session-end"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    hook_cmd.stdin.take().ok_or("stdin")?.write_all(
        serde_json::to_vec(&serde_json::json!({
            "session_id": "m1-e2e-bob",
            "transcript_path": transcript_path,
        }))?
        .as_slice(),
    )?;
    let hook_res = success(hook_cmd.wait_with_output()?)?;
    assert_eq!(hook_res["captured"], true);
    let bob_source_id: kp_model::SourceId = hook_res["source"]["source_id"]
        .as_str()
        .ok_or("bob source_id")?
        .parse()?;

    // 2. Bob shares team membership (`alice,bob` audience) and drains his queued session batch,
    //    then Alice extracts her shared maintenance event via `HostCliRunner`!
    let bob_ext = success(run(
        &lib,
        &[
            "--as",
            "bob",
            "extract",
            "--runner",
            "claude-code",
            "--program",
            mock_claude.to_str().ok_or("mock path")?,
            "--text",
            "alice 是我们架构评审组的核心成员。",
            "--audience",
            "alice,bob",
            "--json",
        ],
    )?)?;
    assert_eq!(bob_ext["jobs_processed"], 2);

    let mut ext_cmd = std::process::Command::new(env!("CARGO_BIN_EXE_kp"));
    ext_cmd
        .arg("--path")
        .arg(&lib)
        .env("CLAUDECODE", "1")
        .env("CLAUDE_CODE_ENTRYPOINT", "cli")
        .args([
            "--as",
            "alice",
            "extract",
            "--runner",
            "claude-code",
            "--program",
            mock_claude.to_str().ok_or("mock path")?,
            "--text",
            "运维公告：下周四全天开启主库故障切换演练。",
            "--audience",
            "alice,bob",
            "--json",
        ]);
    let ext_out = success(ext_cmd.output()?)?;
    assert_eq!(ext_out["jobs_processed"], 1);
    assert!(
        std::fs::read_to_string(&audit_log)?.contains("--output-format"),
        "expected --output-format json on claude runner"
    );

    // Because Alice's event ("主库故障切换演练") entails Bob's constraint ("容灾演练窗口") across members,
    // `align_concepts_and_derive` automatically emitted an `AmbiguousScope` review item for Bob (and 0 for Alice)!
    let bob_reviews = success(run(&lib, &["review", "list", "--as", "bob", "--json"])?)?;
    let bob_rev_arr = bob_reviews.as_array().ok_or("bob reviews")?;
    assert_eq!(bob_rev_arr.len(), 1);
    assert_eq!(bob_rev_arr[0]["kind"], "ambiguous_scope");
    let rev_id = bob_rev_arr[0]["id"].as_str().ok_or("rev_id")?;
    let narrow_constraint_id = bob_rev_arr[0]["target_id"]
        .as_str()
        .ok_or("target_id")?
        .to_owned();
    assert_eq!(
        success(run(&lib, &["review", "list", "--as", "alice", "--json"])?)?,
        serde_json::json!([])
    );

    // 3. Serendipitous inline hint appears in `user-prompt-submit` ONLY for Bob, never Alice.
    let run_ups = |viewer: &str| -> Result<serde_json::Value> {
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_kp"))
            .arg("--path")
            .arg(&lib)
            .args(["--as", viewer, "hook", "user-prompt-submit"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .ok_or("stdin")?
            .write_all(r#"{"prompt":"禁止执行 sqlx migrate"}"#.as_bytes())?;
        success(child.wait_with_output()?)
    };
    let bob_ups = run_ups("bob")?;
    assert!(
        bob_ups.to_string().contains("可用 kp review 确认"),
        "unexpected bob_ups: {bob_ups}"
    );
    let alice_ups = run_ups("alice")?;
    assert!(!alice_ups.to_string().contains("可用 kp review 确认"));

    // 4. Bob resolves `AmbiguousScope` with `--choice confirm` -> derives Alice's `CollisionWarning`!
    let resolved = success(run(
        &lib,
        &[
            "--as", "bob", "review", "resolve", rev_id, "--choice", "confirm", "--json",
        ],
    )?)?;
    assert_eq!(resolved["status"], "confirmed");
    assert!(resolved["derived"].as_u64().unwrap_or(0) >= 1);
    let broadened_constraint_id = resolved["replacement_fact_id"]
        .as_str()
        .ok_or("replacement_fact_id")?
        .to_owned();

    // 5. Alice runs `sqlx migrate run` -> `PreToolUse` emits `permissionDecision: "ask"` citing verbatim quotes & provenance!
    let mut pre_child = std::process::Command::new(env!("CARGO_BIN_EXE_kp"))
        .arg("--path")
        .arg(&lib)
        .args(["--as", "alice", "hook", "pre-tool-use"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    pre_child.stdin.take().ok_or("stdin")?.write_all(
        r#"{"tool_name":"Bash","tool_input":{"command":"sqlx migrate run"}}"#.as_bytes(),
    )?;
    let pre_out = success(pre_child.wait_with_output()?)?;
    assert_eq!(pre_out["hookSpecificOutput"]["permissionDecision"], "ask");
    let ask_reason = pre_out["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .ok_or("reason")?;
    assert!(
        ask_reason.contains("sqlx migrate") && ask_reason.contains("kp-ref:fact_"),
        "unexpected reason: {ask_reason}"
    );

    // Outsider Carol sees ZERO leak on both `PreToolUse` and `search`.
    let mut carol_pre = std::process::Command::new(env!("CARGO_BIN_EXE_kp"))
        .arg("--path")
        .arg(&lib)
        .args(["--as", "carol", "hook", "pre-tool-use"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    carol_pre.stdin.take().ok_or("stdin")?.write_all(
        r#"{"tool_name":"Bash","tool_input":{"command":"sqlx migrate run"}}"#.as_bytes(),
    )?;
    assert_eq!(
        success(carol_pre.wait_with_output()?)?,
        serde_json::json!({})
    );

    // 6. Bob rejects the broadened constraint (`kp reject`), cascading withdrawal to Alice's warning AND blocking re-extraction!
    assert_ne!(narrow_constraint_id, broadened_constraint_id);
    success(run(
        &lib,
        &[
            "--as",
            "bob",
            "reject",
            &broadened_constraint_id,
            "--reason",
            "演练脚本已经改成只读校验，不再限制 migrate",
            "--json",
        ],
    )?)?;
    let mut pre_after_reject = std::process::Command::new(env!("CARGO_BIN_EXE_kp"))
        .arg("--path")
        .arg(&lib)
        .args(["--as", "alice", "hook", "pre-tool-use"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    pre_after_reject.stdin.take().ok_or("stdin")?.write_all(
        r#"{"tool_name":"Bash","tool_input":{"command":"sqlx migrate run"}}"#.as_bytes(),
    )?;
    assert_eq!(
        success(pre_after_reject.wait_with_output()?)?,
        serde_json::json!({})
    );

    // Re-extracting Bob's immutable L0 session chunk (`--force`) is blocked by sticky `rejected_claims`
    // (`dropped_rejected.len() == 1`) and never resurrects the claim!
    {
        let mut s = kp_store::Store::open(&lib)?;
        s.enqueue_extraction_job(&kp_store::ExtractionJobInput {
            source_id: bob_source_id.clone(),
            chunk_ids: vec![format!("{bob_source_id}-0")],
            chunk_batch_sha256: kp_model::ContentHash::digest("reextract-bob-session"),
            author: "bob".parse()?,
            model_policy: "host".into(),
            priority: 10,
            reference_time: kp_model::EpochMs(1_763_683_200_000),
            created_at: kp_model::EpochMs(1_763_683_201_000),
        })?;
    }
    let reextract = success(run(
        &lib,
        &[
            "--as",
            "bob",
            "extract",
            "--force",
            "--runner",
            "claude-code",
            "--program",
            mock_claude.to_str().ok_or("mock path")?,
            "--json",
        ],
    )?)?;
    assert_eq!(
        reextract["dropped_rejected"]
            .as_array()
            .map_or(0, std::vec::Vec::len),
        1,
        "sticky rejection should have dropped re-extraction: {reextract}"
    );
    Ok(())
}

#[test]
fn codex_pre_tool_denies_with_kp_allow_and_single_use_ten_minute_grant_blocks_env_bypass(
) -> Result<()> {
    use std::io::Write;

    let dir = tempfile::tempdir()?;
    let lib = dir.path().join("lib");
    success(run(&lib, &["init", "--owner", "alice", "--json"])?)?;
    success(run(&lib, &["member", "add", "bob", "--json"])?)?;

    // Owner `alice` creates a hard guard on `git merge` and `src/lib.rs` (unrelated maintenance domain).
    let merge_guard = success(run(
        &lib,
        &[
            "remember",
            "--subject",
            "src/lib.rs",
            "--predicate",
            "kp.policy.guard",
            "--value",
            r#"{"hard_guard":true,"reason":"压测维护窗口禁止合并分支或直接改动 lib.rs","forbidden_commands":["git merge"]}"#,
            "--kind",
            "constraint",
            "--valid-from",
            "0",
            "--audience",
            "alice,bob",
            "--json",
        ],
    )?)?;
    let raw_fid = merge_guard["facts"][0].as_str().ok_or("fact id")?;
    let kp_ref = if raw_fid.starts_with("fact_") {
        raw_fid.to_owned()
    } else {
        format!("fact_{raw_fid}")
    };

    let run_pre = |client_flag: Option<&str>,
                   payload: &serde_json::Value,
                   extra_env: bool|
     -> Result<serde_json::Value> {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_kp"));
        cmd.arg("--path")
            .arg(&lib)
            .args(["--as", "alice", "hook", "pre-tool-use"]);
        if let Some(c) = client_flag {
            cmd.args(["--client", c]);
        }
        if extra_env {
            cmd.env("KP_ALLOW", &kp_ref)
                .env("KP_ALLOW_ALL", "1")
                .env("KP_BYPASS_GUARD", "1");
        }
        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .ok_or("stdin")?
            .write_all(serde_json::to_vec(payload)?.as_slice())?;
        success(child.wait_with_output()?)
    };

    // Precision checks: `git merge-base` NEVER matches `git merge`, and `stdlib.rs` NEVER matches `lib.rs`!
    assert_eq!(
        run_pre(
            Some("codex"),
            &serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "git merge-base HEAD main"}}),
            false
        )?,
        serde_json::json!({})
    );
    assert_eq!(
        run_pre(
            Some("codex"),
            &serde_json::json!({"tool_name": "Edit", "tool_input": {"file_path": "crates/foo/stdlib.rs"}}),
            false
        )?,
        serde_json::json!({})
    );

    // Claude Code receives `permissionDecision: "ask"`, whereas Codex receives `permissionDecision: "deny"`.
    let merge_payload =
        serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "git merge feat/test"}});
    let claude_res = run_pre(Some("claude-code"), &merge_payload, false)?;
    assert_eq!(
        claude_res["hookSpecificOutput"]["permissionDecision"],
        "ask"
    );

    // Env vars NEVER bypass `PreToolUse` on Codex!
    let codex_denied = run_pre(Some("codex"), &merge_payload, true)?;
    assert_eq!(
        codex_denied["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );
    let deny_reason = codex_denied["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .ok_or("deny reason")?;
    assert!(
        deny_reason.contains(&format!(
            "先问用户；用户同意后，请用户执行 kp allow {kp_ref}。"
        )),
        "unexpected deny_reason: {deny_reason}"
    );

    // Unprivileged member `bob` cannot issue `kp allow`.
    let bob_allow = run(&lib, &["--as", "bob", "allow", &kp_ref, "--json"])?;
    assert!(!bob_allow.status.success());

    // Expired grant (`--at 1000`, >10 minutes before `now()`) does NOT allow.
    success(run(
        &lib,
        &["--as", "alice", "allow", &kp_ref, "--at", "1000", "--json"],
    )?)?;
    assert_eq!(
        run_pre(Some("codex"), &merge_payload, false)?["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );

    // Fresh owner `kp allow kp-ref:...` allows ONCE within 10 minutes, then immediately blocks the 2nd call!
    let grant = success(run(
        &lib,
        &[
            "--as",
            "alice",
            "allow",
            &format!("kp-ref:{kp_ref}"),
            "--json",
        ],
    )?)?;
    assert_eq!(grant["single_use"], true);
    assert_eq!(
        run_pre(Some("codex"), &merge_payload, false)?,
        serde_json::json!({})
    );
    assert_eq!(
        run_pre(Some("codex"), &merge_payload, false)?["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );
    Ok(())
}

#[test]
fn pre_tool_use_blocks_derived_collision_warning_and_cites_supporting_premises_without_direct_base_guard(
) -> Result<()> {
    use std::io::Write;

    let dir = tempfile::tempdir()?;
    let lib = dir.path().join("lib");
    // `admin` owns the library; `alice`, `bob`, and `carol` are regular `Member`s.
    success(run(
        &lib,
        &["init", "--owner", "admin", "--community", "--json"],
    )?)?;
    for member in ["alice", "bob", "carol"] {
        success(run(&lib, &["member", "add", member, "--json"])?)?;
    }

    // Regular member `bob` records an unscoped `Constraint` (`governed_scope = None`, `source_scope_id = None`).
    // Because `bob` is NOT `Owner`/`Maintainer`, this base `Constraint` alone MUST NOT block `alice` directly!
    let bob_constraint = success(run(
        &lib,
        &[
            "--as",
            "bob",
            "remember",
            "--subject",
            "运维演练规范",
            "--predicate",
            "kp.core.preference",
            "--value",
            r#"{"summary":"演练窗口禁止执行 sqlx migrate","forbidden_commands":["sqlx migrate"]}"#,
            "--kind",
            "constraint",
            "--tags",
            "ops_drill_slot",
            "--valid-from",
            "0",
            "--audience",
            "alice,bob",
            "--json",
        ],
    )?)?;
    let constraint_id = bob_constraint["facts"][0].as_str().ok_or("constraint_id")?;
    let constraint_ref = if constraint_id.starts_with("fact_") {
        constraint_id.to_owned()
    } else {
        format!("fact_{constraint_id}")
    };

    let run_pre =
        |viewer: &str, client: &str| -> Result<serde_json::Value> {
            let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_kp"))
                .arg("--path")
                .arg(&lib)
                .args(["--as", viewer, "hook", "pre-tool-use", "--client", client])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()?;
            child.stdin.take().ok_or("stdin")?.write_all(
                br#"{"tool_name":"Bash","tool_input":{"command":"sqlx migrate run"}}"#,
            )?;
            success(child.wait_with_output()?)
        };

    // 1. Before any derived `CollisionWarning`, `alice` has ZERO direct base guard matching `sqlx migrate run`.
    assert_eq!(run_pre("alice", "claude-code")?, serde_json::json!({}));
    assert_eq!(run_pre("alice", "codex")?, serde_json::json!({}));

    // 2. `alice` records an active shared `Event` + `bob` also records a Bob-only private `Event`, then `kp derive` runs.
    let alice_event = success(run(
        &lib,
        &[
            "--as",
            "alice",
            "remember",
            "--subject",
            "今日运维安排",
            "--predicate",
            "kp.core.schedule",
            "--value",
            "今日开启主库故障切换演练",
            "--kind",
            "event",
            "--tags",
            "ops_drill_slot",
            "--valid-from",
            "0",
            "--audience",
            "alice,bob",
            "--json",
        ],
    )?)?;
    let event_id = alice_event["facts"][0].as_str().ok_or("event_id")?;
    let event_ref = if event_id.starts_with("fact_") {
        event_id.to_owned()
    } else {
        format!("fact_{event_id}")
    };

    success(run(
        &lib,
        &[
            "--as",
            "bob",
            "remember",
            "--subject",
            "BOB_SECRET_DRILL_CANARY",
            "--predicate",
            "kp.core.schedule",
            "--value",
            "BOB_SECRET_DRILL_CANARY_PAYLOAD",
            "--kind",
            "event",
            "--tags",
            "ops_drill_slot",
            "--valid-from",
            "0",
            "--audience",
            "bob",
            "--json",
        ],
    )?)?;

    let derived = success(run(&lib, &["--as", "alice", "derive", "--json"])?)?;
    assert!(derived["derived"].as_u64().unwrap_or(0) >= 1);

    // 3. Claude Code `PreToolUse` blocks `alice` via the derived `CollisionWarning` (`ask`) and cites BOTH visible premises (`↳`).
    let claude_out = run_pre("alice", "claude-code")?;
    assert_eq!(
        claude_out["hookSpecificOutput"]["permissionDecision"], "ask",
        "expected derived CollisionWarning ask: {claude_out}"
    );
    let claude_reason = claude_out["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .ok_or("claude_reason")?;
    assert!(
        claude_reason.contains("↳")
            && claude_reason.contains(&format!("kp-ref:{event_ref}"))
            && claude_reason.contains(&format!("kp-ref:{constraint_ref}")),
        "expected supporting premise citations in reason: {claude_reason}"
    );
    assert!(
        !claude_reason.contains("BOB_SECRET_DRILL_CANARY"),
        "invisible premise leaked to alice: {claude_reason}"
    );

    // 4. Codex `PreToolUse` blocks `alice` (`deny` + `kp allow <warning_ref>`) with identical premise provenance.
    let codex_out = run_pre("alice", "codex")?;
    assert_eq!(
        codex_out["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );
    let codex_reason = codex_out["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .ok_or("codex_reason")?;
    assert!(
        codex_reason.contains("↳")
            && codex_reason.contains(&format!("kp-ref:{event_ref}"))
            && codex_reason.contains(&format!("kp-ref:{constraint_ref}"))
            && codex_reason.contains("先问用户；用户同意后，请用户执行 kp allow fact_"),
        "unexpected codex_reason: {codex_reason}"
    );

    // 5. Outsider `carol` sees `{}` (zero leak).
    assert_eq!(run_pre("carol", "claude-code")?, serde_json::json!({}));
    assert_eq!(run_pre("carol", "codex")?, serde_json::json!({}));
    Ok(())
}

#[test]
fn extract_advances_session_cursor_only_to_completed_job_span_and_resolves_concept_alignment_review(
) -> Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir()?;
    let lib = dir.path().join("lib");
    success(run(&lib, &["init", "--owner", "alice", "--json"])?)?;
    success(run(&lib, &["member", "add", "bob", "--json"])?)?;

    let mock_claude = dir.path().join("mock-claude");
    std::fs::write(
        &mock_claude,
        r#"#!/usr/bin/env python3
import json, re, sys
prompt = sys.stdin.read()
if "<chunk id=" not in prompt:
    print(json.dumps({"structured_output": {"verdict": "ambiguous", "question": "两个窗口概念是否指向同一运维动作？"}}))
    sys.exit(0)
m = re.search(r'<chunk id="([^"]+)"', prompt)
cid = m.group(1) if m else "c-0"
atoms = []
if "周四下午灰度巡检窗口" in prompt:
    atoms.append({
        "kind": "event",
        "claim_text": "周四下午进入灰度巡检窗口",
        "value_json": "{}",
        "scope": None,
        "ambiguous_scope": None,
        "condition_concept": {"ref_type": "propose_new", "concept_id": None, "kind": "condition", "label": "灰度巡检窗口", "domain": "ops", "polarity": "neutral"},
        "action_concept": None,
        "group_concept": None,
        "member_mention": None,
        "valid_dates": None,
        "speaker": "alice",
        "assumed_premise_ids": [],
        "quote": {"chunk_id": cid, "verbatim_quote": "周四下午灰度巡检窗口开启", "turn_ordinal": None, "approx_char_offset": None},
        "write_op": {"op": "add", "target_fact_id": None}
    })
elif "核心封版期内" in prompt:
    atoms.append({
        "kind": "constraint",
        "claim_text": "核心封版期内禁止重启主网关",
        "value_json": "{\"command\":\"systemctl restart gateway\"}",
        "scope": None,
        "ambiguous_scope": None,
        "condition_concept": {"ref_type": "propose_new", "concept_id": None, "kind": "condition", "label": "核心封版期", "domain": "ops", "polarity": "neutral"},
        "action_concept": {"ref_type": "propose_new", "concept_id": None, "kind": "action", "label": "禁止重启主网关", "domain": "ops", "polarity": "negative"},
        "group_concept": None,
        "member_mention": None,
        "valid_dates": None,
        "speaker": "alice",
        "assumed_premise_ids": [],
        "quote": {"chunk_id": cid, "verbatim_quote": "核心封版期内禁止重启主网关", "turn_ordinal": None, "approx_char_offset": None},
        "write_op": {"op": "add", "target_fact_id": None}
    })
print(json.dumps({"structured_output": {"skip_summary": False, "atoms": atoms}}, ensure_ascii=False))
"#,
    )?;
    #[cfg(unix)]
    std::fs::set_permissions(&mock_claude, std::fs::Permissions::from_mode(0o755))?;

    let tp = dir.path().join("concurrent-session.jsonl");
    let line1 = "{\"type\":\"user\",\"timestamp\":\"2026-11-22T10:00:00+08:00\",\"message\":{\"role\":\"user\",\"content\":\"周四下午灰度巡检窗口开启。\"}}\n";
    std::fs::write(&tp, line1)?;
    let len1 = std::fs::metadata(&tp)?.len();

    let run_stop = |path: &Path| -> Result<Value> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kp"))
            .arg("--path")
            .arg(&lib)
            .env("KP_OFFLINE", "1")
            .env("KP_HOST_CLI_PROGRAM", &mock_claude)
            .env("KP_NO_AUTO_DAEMON", "1")
            .args(["--as", "alice", "hook", "stop"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        child.stdin.take().ok_or("stdin")?.write_all(
            serde_json::to_vec(&serde_json::json!({
                "session_id": "concurrent-s1",
                "transcript_path": path,
            }))?
            .as_slice(),
        )?;
        success(child.wait_with_output()?)
    };

    // 1. Stop #1 queues Job 1 (`0..len1`). Before `extract` runs, concurrent Stop #2 appends `len1..len2`.
    assert_eq!(run_stop(&tp)?["captured"], true);
    let line2 = "{\"type\":\"user\",\"timestamp\":\"2026-11-22T10:02:00+08:00\",\"message\":{\"role\":\"user\",\"content\":\"核心封版期内禁止重启主网关。\"}}\n";
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(&tp)?;
        f.write_all(line2.as_bytes())?;
        f.flush()?;
    }
    let len2 = std::fs::metadata(&tp)?.len();
    assert!(len2 > len1);
    assert_eq!(run_stop(&tp)?["captured"], true);

    {
        let ro = kp_store::Store::open_readonly(&lib)?;
        let cur = ro
            .get_session_cursor("concurrent-s1")?
            .ok_or("expected cursor")?;
        assert_eq!(cur.last_byte_offset, len2);
        assert_eq!(cur.last_extracted_offset, 0);
    }

    // 2. `kp extract --max-jobs 1` processes ONLY Job 1 (`0..len1`) and MUST advance `last_extracted_offset`
    //    strictly to `len1` (NEVER jumping to `last_byte_offset == len2`!).
    let ext1 = success(run(
        &lib,
        &[
            "--as",
            "alice",
            "extract",
            "--runner",
            "claude-code",
            "--program",
            mock_claude.to_str().ok_or("mock path")?,
            "--max-jobs",
            "1",
            "--json",
        ],
    )?)?;
    assert_eq!(ext1["jobs_processed"], 1);
    {
        let ro = kp_store::Store::open_readonly(&lib)?;
        let cur = ro
            .get_session_cursor("concurrent-s1")?
            .ok_or("expected cursor after job1")?;
        assert_eq!(cur.last_byte_offset, len2);
        assert_eq!(
            cur.last_extracted_offset, len1,
            "run_extract must advance only to job1 end ({len1}), never jumping to {len2}"
        );
    }

    // 3. Second `kp extract --max-jobs 1` drains Job 2 (`len1..len2`), advancing `last_extracted_offset` to `len2`,
    //    clearing `needs_alignment`, and emitting a `ConceptAlignment` review item!
    let ext2 = success(run(
        &lib,
        &[
            "--as",
            "alice",
            "extract",
            "--runner",
            "claude-code",
            "--program",
            mock_claude.to_str().ok_or("mock path")?,
            "--max-jobs",
            "1",
            "--json",
        ],
    )?)?;
    assert_eq!(ext2["jobs_processed"], 1);
    {
        let ro = kp_store::Store::open_readonly(&lib)?;
        let cur = ro
            .get_session_cursor("concurrent-s1")?
            .ok_or("expected cursor after job2")?;
        assert_eq!(cur.last_extracted_offset, len2);
        assert!(!ro.needs_alignment()?);
    }

    // 4. Confirming `ConceptAlignment` via `kp review resolve --choice confirm` persists manual alignment and derives warning!
    let reviews = success(run(&lib, &["review", "list", "--as", "alice", "--json"])?)?;
    let ca_rev = reviews
        .as_array()
        .ok_or("reviews array")?
        .iter()
        .find(|r| r["kind"] == "concept_alignment")
        .ok_or("expected concept_alignment review")?;
    let ca_id = ca_rev["id"].as_str().ok_or("ca_id")?;
    let resolved = success(
        Command::new(env!("CARGO_BIN_EXE_kp"))
            .arg("--path")
            .arg(&lib)
            .env("KP_MODEL_POLICY", "raw_only")
            .args([
                "--as", "alice", "review", "resolve", ca_id, "--choice", "confirm", "--json",
            ])
            .output()?,
    )?;
    assert_eq!(resolved["status"], "confirmed");
    assert!(
        resolved["derived"].as_u64().unwrap_or(0) >= 1,
        "expected manual concept_alignment confirm to derive warning: {resolved}"
    );
    Ok(())
}

#[test]
fn multi_guard_codex_pre_tool_use_and_multi_arg_kp_allow_are_all_or_nothing() -> Result<()> {
    use std::io::Write;

    let dir = tempfile::tempdir()?;
    let lib = dir.path().join("lib");
    success(run(&lib, &["init", "--owner", "alice", "--json"])?)?;

    let mut refs = Vec::new();
    for (key, reason) in [
        ("guard-force-1", "硬规矩一：禁止执行 git push --force"),
        (
            "guard-force-2",
            "硬规矩二：保护分支禁止强推 git push --force",
        ),
    ] {
        let out = success(run(
            &lib,
            &[
                "--as",
                "alice",
                "remember",
                "--subject",
                "git push --force",
                "--predicate",
                "kp.policy.guard",
                "--value",
                &serde_json::json!({
                    "hard_guard": true,
                    "reason": reason,
                    "forbidden_commands": ["git push --force", "git push -f"]
                })
                .to_string(),
                "--kind",
                "constraint",
                "--logical-key",
                key,
                "--valid-from",
                "0",
                "--json",
            ],
        )?)?;
        let fid = out["facts"][0].as_str().ok_or("fact_id")?;
        refs.push(if fid.starts_with("fact_") {
            fid.to_owned()
        } else {
            format!("fact_{fid}")
        });
    }
    let ref1 = &refs[0];
    let ref2 = &refs[1];

    let run_codex_pre = || -> Result<Value> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kp"))
            .arg("--path")
            .arg(&lib)
            .env("KP_NO_AUTO_DAEMON", "1")
            .args(["--as", "alice", "hook", "pre-tool-use", "--client", "codex"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        child.stdin.take().ok_or("stdin")?.write_all(
            br#"{"tool_name":"Bash","tool_input":{"command":"git push --force origin feat"}}"#,
        )?;
        success(child.wait_with_output()?)
    };

    // 1. Initial Codex PreToolUse denies and lists BOTH refs in `kp allow <ref1> <ref2>`.
    let deny0 = run_codex_pre()?;
    assert_eq!(deny0["hookSpecificOutput"]["permissionDecision"], "deny");
    let reason0 = deny0["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .ok_or("reason0")?;
    assert!(reason0.contains(ref1) && reason0.contains(ref2));

    // 2. Atomic rollback check: `kp allow <ref1> fact_missing_999` fails and issues ZERO grants!
    let bad_batch = run(
        &lib,
        &[
            "--as",
            "alice",
            "allow",
            ref1,
            "fact_missing_999999",
            "--json",
        ],
    )?;
    assert!(!bad_batch.status.success());

    // 3. Single-arg partial `kp allow <ref1>`: preserves top-level fields + `grants`, and Codex PreToolUse
    //    still denies (reporting ONLY `<ref2>`) WITHOUT consuming `<ref1>`'s grant!
    let single_grant = success(run(&lib, &["--as", "alice", "allow", ref1, "--json"])?)?;
    assert_eq!(single_grant["single_use"], true);
    assert_eq!(single_grant["kp_ref"], *ref1);
    assert_eq!(
        single_grant["grants"].as_array().map(std::vec::Vec::len),
        Some(1)
    );

    let deny_partial = run_codex_pre()?;
    assert_eq!(
        deny_partial["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );
    let partial_reason = deny_partial["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .ok_or("partial_reason")?;
    assert!(
        partial_reason.contains(ref2) && !partial_reason.contains(&format!("kp allow {ref1}")),
        "unexpected partial_reason: {partial_reason}"
    );

    // Granting `<ref2>` now allows ONCE because `<ref1>` was preserved unconsumed!
    success(run(&lib, &["--as", "alice", "allow", ref2, "--json"])?)?;
    assert_eq!(run_codex_pre()?, serde_json::json!({}));
    assert_eq!(
        run_codex_pre()?["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );

    // 4. Multi-arg `kp allow <ref1> <ref2>` issues both grants atomically in one invocation, allowing ONCE!
    let multi_grant = success(run(
        &lib,
        &[
            "--as",
            "alice",
            "allow",
            &format!("kp-ref:{ref1}"),
            ref2,
            "--json",
        ],
    )?)?;
    assert_eq!(multi_grant["single_use"], true);
    assert_eq!(multi_grant["count"], 2);
    assert_eq!(
        multi_grant["grants"].as_array().map(std::vec::Vec::len),
        Some(2)
    );
    assert_eq!(run_codex_pre()?, serde_json::json!({}));
    assert_eq!(
        run_codex_pre()?["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );
    Ok(())
}

#[test]
fn member_cannot_forget_or_purge_another_authors_shared_fact_while_author_and_owner_can(
) -> Result<()> {
    let dir = tempfile::tempdir()?;
    let lib = dir.path().join("acl_lib");
    success(run(
        &lib,
        &[
            "init",
            "--owner",
            "alice",
            "--community",
            "--no-embedding",
            "--json",
        ],
    )?)?;
    success(run(
        &lib,
        &["member", "add", "bob", "--role", "member", "--json"],
    )?)?;

    let alice_rem = success(run(
        &lib,
        &[
            "--as",
            "alice",
            "remember",
            "--subject",
            "db",
            "--predicate",
            "kp.core.note",
            "--value",
            "Alice shared note",
            "--logical-key",
            "alice-shared-key",
            "--audience",
            "alice,bob",
            "--json",
        ],
    )?)?;
    let alice_fid = alice_rem["facts"][0].as_str().ok_or("alice_fid")?;

    let bob_forget = run(&lib, &["--as", "bob", "forget", alice_fid, "--json"])?;
    assert!(!bob_forget.status.success());
    let bob_purge = run(
        &lib,
        &["--as", "bob", "purge", "alice-shared-key", "--json"],
    )?;
    assert!(!bob_purge.status.success());

    success(run(
        &lib,
        &[
            "--as",
            "bob",
            "remember",
            "--subject",
            "bob-item",
            "--predicate",
            "kp.core.note",
            "--value",
            "Bob own note",
            "--logical-key",
            "bob-own-key",
            "--audience",
            "alice,bob",
            "--json",
        ],
    )?)?;
    success(run(
        &lib,
        &["--as", "bob", "purge", "bob-own-key", "--json"],
    )?)?;
    success(run(
        &lib,
        &["--as", "alice", "forget", alice_fid, "--json"],
    )?)?;
    Ok(())
}
