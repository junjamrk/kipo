use std::collections::BTreeSet;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use kp_model::{
    derived_fact, justification, ActorId, AnchorSpec, AudienceSpec, BitemporalStamp,
    DerivationMode, EpochMs, Fact, FactAccess, FactId, FactKind, FactRank, FactStatus, KpError,
    LibraryMode, MemberRole, ModelRoutePolicy, PublicAudience, PublicContainerRoster,
    PublicIngestOp, PublicIngestRecord, PublicIngestSource, PublicSpeaker, ReviewKind,
    ReviewStatus, TrustTier, ValidityWindow,
};
use serde_json::json;

use crate::test_support::{library, AT};
use crate::{
    find_sqlite_table_containing_bytes, new_fact_id, ChunkInput, Embedder, RawSourceInput, Result,
    StoreError,
};

struct CountingCloudEmbedder {
    id: String,
    dims: usize,
    calls: AtomicUsize,
}

impl CountingCloudEmbedder {
    fn new(id: &str, dims: usize) -> Self {
        Self {
            id: id.to_owned(),
            dims,
            calls: AtomicUsize::new(0),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Embedder for CountingCloudEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut vec = vec![0.0_f32; self.dims];
        let hash = text.bytes().fold(0xcbf29ce484222325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
        });
        let idx_a = (hash as usize) % self.dims;
        let idx_b = ((hash >> 16) as usize) % self.dims;
        if idx_a == idx_b {
            vec[idx_a] = 1.0;
        } else {
            vec[idx_a] = 0.6;
            vec[idx_b] = 0.8;
        }
        Ok(vec)
    }

    fn model_id(&self) -> &str {
        &self.id
    }

    fn expected_dims(&self) -> Option<usize> {
        Some(self.dims)
    }

    fn allows_raw_only(&self) -> bool {
        false
    }
}

#[allow(clippy::too_many_arguments)]
fn make_base_fact(
    id: FactId,
    library_id: &str,
    kind: FactKind,
    subject: &str,
    predicate: &str,
    object: serde_json::Value,
    author: &ActorId,
    audience: AudienceSpec,
    quote: Option<&str>,
    at: EpochMs,
) -> Result<Fact> {
    Ok(Fact {
        id: id.clone(),
        origin_library: library_id.parse()?,
        logical_key: id.as_str().parse()?,
        kind,
        subject: subject.to_owned(),
        predicate: predicate.to_owned(),
        object,
        tags: BTreeSet::new(),
        author: author.clone(),
        ingested_by: Some(author.clone()),
        trust_tier: TrustTier::HumanVerbatim,
        access: FactAccess::Base {
            audience,
            stamp: BitemporalStamp {
                valid: ValidityWindow {
                    valid_from: at,
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
        source_anchor: quote.map(|q| AnchorSpec {
            verbatim_quote: Some(q.to_owned()),
            ..AnchorSpec::default()
        }),
        unverified_gaps: Vec::new(),
        miss_count: 0,
    })
}

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
                return Err(KpError::invalid("test", "unexpected bullet line format").into());
            };
            out.push(format!("{prefix}`) {new_claim}"));
            replace_next_bullet = false;
            replaced = true;
            continue;
        }
        out.push(line.to_owned());
    }
    if !replaced {
        return Err(KpError::invalid("test", "target entry marker not found in index.md").into());
    }
    let mut joined = out.join("\n");
    joined.push('\n');
    Ok(joined)
}

fn remove_entry_block(index_md: &str, fact_id: &FactId) -> Result<String> {
    let marker = format!("<!-- kp-entry: {fact_id} -->");
    let lines: Vec<&str> = index_md.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let mut removed = false;

    while i < lines.len() {
        if lines[i].trim() == marker {
            removed = true;
            i += 1;
            if i < lines.len() && lines[i].starts_with("- **[kp-ref:") {
                i += 1;
                while i < lines.len() && lines[i].starts_with("  - ") {
                    i += 1;
                }
            }
            continue;
        }
        out.push(lines[i]);
        i += 1;
    }
    if !removed {
        return Err(KpError::invalid("test", "target entry marker to delete not found").into());
    }
    let mut joined = out.join("\n");
    joined.push('\n');
    Ok(joined)
}

#[test]
fn writeback_lossless_roundtrip_edit_delete_reexport_and_idempotent_second_apply() -> Result<()> {
    let (_db_dir, mut store) = library(LibraryMode::Community)?;
    let owner: ActorId = "owner".parse()?;
    let alice: ActorId = "alice".parse()?;
    store.add_member(alice.clone(), MemberRole::Maintainer, AT)?;

    let dec_id = new_fact_id()?;
    let dec = make_base_fact(
        dec_id.clone(),
        "local",
        FactKind::Decision,
        "storage_engine",
        "kp.core.decision",
        json!({"summary": "Use PostgreSQL 15 for primary ledger"}),
        &alice,
        AudienceSpec::AllMembers,
        Some("Use PostgreSQL 15 for primary ledger"),
        AT,
    )?;
    store.insert_fact(&dec)?;

    let rule_id = new_fact_id()?;
    let rule = make_base_fact(
        rule_id.clone(),
        "local",
        FactKind::Constraint,
        "deploy_window",
        "kp.core.constraint",
        json!("Never deploy schema migrations on Friday afternoon"),
        &alice,
        AudienceSpec::AllMembers,
        Some("Never deploy schema migrations on Friday afternoon"),
        AT,
    )?;
    store.insert_fact(&rule)?;

    let obs_delete_id = new_fact_id()?;
    let obs_delete = make_base_fact(
        obs_delete_id.clone(),
        "local",
        FactKind::Observation,
        "temp_debug",
        "kp.core.note",
        json!("Temporary staging banner enabled"),
        &alice,
        AudienceSpec::Explicit(BTreeSet::from([alice.clone(), owner.clone()])),
        Some("Temporary staging banner enabled"),
        AT,
    )?;
    store.insert_fact(&obs_delete)?;

    let export_dir = tempfile::tempdir()?;
    let exp1 = store.export_markdown_directory(export_dir.path(), &alice, EpochMs(AT.0 + 10))?;
    assert_eq!(exp1.visible_facts, 3);

    // Edit `dec_id`, delete `obs_delete_id`, leave `rule_id` untouched.
    let index_path = export_dir.path().join("index.md");
    let raw_index = fs::read_to_string(&index_path)?;
    let edited_claim = "Use PostgreSQL 16 with logical replication for primary ledger";
    let step1 = replace_entry_line_claim(&raw_index, &dec_id, edited_claim)?;
    let step2 = remove_entry_block(&step1, &obs_delete_id)?;
    fs::write(&index_path, step2)?;

    let apply1 = store.apply_markdown_directory(export_dir.path(), &alice, EpochMs(AT.0 + 20))?;
    assert!(!apply1.unchanged);
    assert!(!apply1.baseline_conflict);
    assert_eq!(apply1.updated_facts.len(), 1);
    assert_eq!(apply1.updated_facts[0].old_fact_id, dec_id);
    assert_eq!(apply1.deleted_fact_ids, vec![obs_delete_id.clone()]);
    assert!(apply1.enqueued_proposals.is_empty());

    let new_dec_id = apply1.updated_facts[0].new_fact_id.clone();
    let vis_after = store.visible_facts(&alice, EpochMs(AT.0 + 30))?;
    let vis_ids: BTreeSet<FactId> = vis_after.iter().map(|f| f.id.clone()).collect();
    assert!(vis_ids.contains(&new_dec_id));
    assert!(vis_ids.contains(&rule_id));
    assert!(!vis_ids.contains(&dec_id));
    assert!(!vis_ids.contains(&obs_delete_id));

    // Re-export after apply: updated claim is present verbatim, deleted fact is absent.
    let exp2 = store.export_markdown_directory(export_dir.path(), &alice, EpochMs(AT.0 + 30))?;
    assert_eq!(exp2.visible_facts, 2);
    let reexported_index = fs::read_to_string(&index_path)?;
    assert!(reexported_index.contains(edited_claim));
    assert!(!reexported_index.contains("Use PostgreSQL 15 for primary ledger"));
    assert!(!reexported_index.contains("Temporary staging banner enabled"));

    // Second apply on the untouched re-export takes the idempotent fast path with zero DB writes.
    let seq_before_second: i64 =
        store
            .db
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM change_log", [], |r| {
                r.get(0)
            })?;
    let apply2 = store.apply_markdown_directory(export_dir.path(), &alice, EpochMs(AT.0 + 40))?;
    let seq_after_second: i64 =
        store
            .db
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM change_log", [], |r| {
                r.get(0)
            })?;
    assert!(apply2.unchanged);
    assert!(!apply2.baseline_conflict);
    assert!(apply2.updated_facts.is_empty());
    assert!(apply2.deleted_fact_ids.is_empty());
    assert!(apply2.enqueued_proposals.is_empty());
    assert_eq!(seq_before_second, seq_after_second);

    Ok(())
}

#[test]
fn writeback_stale_baseline_routes_edits_deletes_and_freeform_to_proposals_then_confirms(
) -> Result<()> {
    let (_db_dir, mut store) = library(LibraryMode::Community)?;
    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;
    store.add_member(alice.clone(), MemberRole::Member, AT)?;
    store.add_member(bob.clone(), MemberRole::Member, AT)?;

    let f_edit_id = new_fact_id()?;
    let f_edit = make_base_fact(
        f_edit_id.clone(),
        "local",
        FactKind::Decision,
        "api_timeout",
        "kp.core.timeout",
        json!("Set gateway timeout to 5s"),
        &alice,
        AudienceSpec::AllMembers,
        Some("Set gateway timeout to 5s"),
        AT,
    )?;
    store.insert_fact(&f_edit)?;

    let f_del_id = new_fact_id()?;
    let f_del = make_base_fact(
        f_del_id.clone(),
        "local",
        FactKind::Observation,
        "legacy_flag",
        "kp.core.flag",
        json!("Enable legacy XML fallback"),
        &alice,
        AudienceSpec::AllMembers,
        Some("Enable legacy XML fallback"),
        AT,
    )?;
    store.insert_fact(&f_del)?;

    let f_keep_id = new_fact_id()?;
    let f_keep = make_base_fact(
        f_keep_id.clone(),
        "local",
        FactKind::Constraint,
        "tls_min",
        "kp.core.tls",
        json!("Require TLS 1.3 minimum"),
        &alice,
        AudienceSpec::AllMembers,
        Some("Require TLS 1.3 minimum"),
        AT,
    )?;
    store.insert_fact(&f_keep)?;

    let export_dir = tempfile::tempdir()?;
    let exp = store.export_markdown_directory(export_dir.path(), &alice, EpochMs(AT.0 + 10))?;

    // Concurrent DB write by `bob` AFTER `alice`'s export advances `change_log.seq > exp.base_seq`!
    // This concurrent fact MUST NOT be misclassified as deleted by `alice`.
    let f_concurrent_id = new_fact_id()?;
    let f_concurrent = make_base_fact(
        f_concurrent_id.clone(),
        "local",
        FactKind::Decision,
        "retry_budget",
        "kp.core.retry",
        json!("Cap retry attempts at 3"),
        &bob,
        AudienceSpec::AllMembers,
        Some("Cap retry attempts at 3"),
        EpochMs(AT.0 + 15),
    )?;
    store.insert_fact(&f_concurrent)?;

    // `alice` edits `f_edit`, deletes `f_del`, leaves `f_keep` untouched, and appends a freeform note.
    let index_path = export_dir.path().join("index.md");
    let raw_index = fs::read_to_string(&index_path)?;
    let s1 = replace_entry_line_claim(&raw_index, &f_edit_id, "Set gateway timeout to 12s")?;
    let mut s2 = remove_entry_block(&s1, &f_del_id)?;
    s2.push_str("\n## Follow-ups\n\nAudit connection pool saturation metrics before rollout.\n");
    fs::write(&index_path, s2)?;

    let apply_conflict =
        store.apply_markdown_directory(export_dir.path(), &alice, EpochMs(AT.0 + 25))?;
    assert!(!apply_conflict.unchanged);
    assert!(apply_conflict.baseline_conflict);
    assert_eq!(apply_conflict.base_seq, exp.base_seq);
    assert!(apply_conflict.current_seq > exp.base_seq);
    assert!(apply_conflict.updated_facts.is_empty());
    assert!(apply_conflict.deleted_fact_ids.is_empty());
    // Exactly 3 proposals: 1 edit (`f_edit`), 1 delete (`f_del`), 1 freeform (`Follow-ups`).
    // Neither untouched `f_keep` nor concurrent `f_concurrent` generated a proposal!
    assert_eq!(apply_conflict.enqueued_proposals.len(), 3);

    let open_reviews = store.list_review_items(&alice, Some(ReviewStatus::Open))?;
    assert_eq!(open_reviews.len(), 3);
    for rev in &open_reviews {
        assert_eq!(rev.kind, ReviewKind::WritebackProposal);
        assert_eq!(rev.payload.get("base_seq"), Some(&json!(exp.base_seq)));
    }

    // Confirm all 3 proposals via `apply_confirmed_writeback_proposal`.
    let mut new_edited_fact: Option<FactId> = None;
    let mut new_freeform_fact: Option<FactId> = None;
    for rev in &open_reviews {
        let op = rev
            .payload
            .get("proposal_op")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let out = store.apply_confirmed_writeback_proposal(&rev.id, &alice, EpochMs(AT.0 + 35))?;
        match op {
            "edit_fact" => new_edited_fact = out,
            "delete_fact" => assert_eq!(out, None),
            "freeform_block" => new_freeform_fact = out,
            _ => {
                return Err(KpError::invalid("test", "unexpected proposal_op").into());
            }
        }
        // Double-confirming an already resolved proposal must reject with `Conflict`.
        assert!(matches!(
            store.apply_confirmed_writeback_proposal(&rev.id, &alice, EpochMs(AT.0 + 36)),
            Err(StoreError::Conflict)
        ));
    }

    let edited_head_id =
        new_edited_fact.ok_or_else(|| KpError::invalid("test", "missing edited fact id"))?;
    let freeform_id =
        new_freeform_fact.ok_or_else(|| KpError::invalid("test", "missing freeform fact id"))?;

    let alice_vis = store.visible_facts(&alice, EpochMs(AT.0 + 40))?;
    let alice_ids: BTreeSet<FactId> = alice_vis.iter().map(|f| f.id.clone()).collect();
    assert!(alice_ids.contains(&edited_head_id));
    assert!(alice_ids.contains(&freeform_id));
    assert!(alice_ids.contains(&f_keep_id));
    assert!(alice_ids.contains(&f_concurrent_id));
    assert!(!alice_ids.contains(&f_edit_id));
    assert!(!alice_ids.contains(&f_del_id));

    // Freeform note has personal `Explicit([alice])` audience, so `bob` cannot see it!
    let bob_vis = store.visible_facts(&bob, EpochMs(AT.0 + 40))?;
    let bob_ids: BTreeSet<FactId> = bob_vis.iter().map(|f| f.id.clone()).collect();
    assert!(bob_ids.contains(&edited_head_id));
    assert!(!bob_ids.contains(&freeform_id));

    Ok(())
}

#[test]
fn writeback_routes_derived_cross_author_extra_files_and_scrubs_secrets() -> Result<()> {
    let (db_dir, mut store) = library(LibraryMode::Community)?;
    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;
    store.add_member(alice.clone(), MemberRole::Member, AT)?;
    store.add_member(bob.clone(), MemberRole::Member, AT)?;

    // `alice` owns a public rule and `bob` owns a public event, producing a derived `CollisionWarning`.
    let rule_id = new_fact_id()?;
    let rule = make_base_fact(
        rule_id.clone(),
        "local",
        FactKind::Constraint,
        "prod_freeze",
        "kp.core.freeze",
        json!("Freeze production releases during maintenance window"),
        &alice,
        AudienceSpec::AllMembers,
        Some("Freeze production releases during maintenance window"),
        AT,
    )?;
    store.insert_fact(&rule)?;

    let ev_id = new_fact_id()?;
    let ev = make_base_fact(
        ev_id.clone(),
        "local",
        FactKind::Event,
        "prod_release",
        "kp.core.deploy",
        json!("Scheduled production release at 02:00"),
        &bob,
        AudienceSpec::AllMembers,
        Some("Scheduled production release at 02:00"),
        AT,
    )?;
    store.insert_fact(&ev)?;

    let warn = derived_fact(
        &"local".parse()?,
        &bob,
        FactKind::CollisionWarning,
        "prod_freeze",
        "kp.core.collision",
        json!({"summary": "Release conflicts with maintenance freeze"}),
        AT,
    )?;
    let warn_id = warn.id.clone();
    let just = justification(
        &warn,
        DerivationMode::HardRule,
        "collision_v1",
        &[&rule, &ev],
    )?;
    store.save_derivations(&[warn], &[just])?;

    let export_dir = tempfile::tempdir()?;
    store.export_markdown_directory(export_dir.path(), &bob, EpochMs(AT.0 + 10))?;

    // Cross-viewer rejection: `bob`'s exported directory CANNOT be applied as `alice`!
    assert!(matches!(
        store.apply_markdown_directory(export_dir.path(), &alice, EpochMs(AT.0 + 11)),
        Err(StoreError::PermissionDenied)
    ));

    // Construct a synthetic secret token without hardcoding real keys; verify `SecretRedactor` scrubs it.
    let fake_secret = format!("sk-proj-{}", "A9b8C7d6E5f4G3h2J1k0L9m8N7p6Q5r4S3t2");

    // As ordinary `Member` `bob`:
    // 1) Try editing `alice`'s `rule_id` (cross-author edit -> proposal)
    // 2) Try editing derived `warn_id` (derived edit -> proposal)
    // 3) Edit `sources.md` and drop an extra `scratch.md` containing `fake_secret`
    let index_path = export_dir.path().join("index.md");
    let raw_index = fs::read_to_string(&index_path)?;
    let mod1 = replace_entry_line_claim(
        &raw_index,
        &rule_id,
        &format!("Allow hotfix override with key {fake_secret}"),
    )?;
    let mod2 =
        replace_entry_line_claim(&mod1, &warn_id, "Acknowledged collision warning for hotfix")?;
    fs::write(&index_path, mod2)?;

    let sources_path = export_dir.path().join("sources.md");
    let mut raw_sources_md = fs::read_to_string(&sources_path)?;
    raw_sources_md.push_str(&format!("\nManual source annotation token {fake_secret}\n"));
    fs::write(&sources_path, raw_sources_md)?;

    fs::write(
        export_dir.path().join("scratch.md"),
        format!("Extra scratch pad note with credential {fake_secret}\n"),
    )?;

    let report = store.apply_markdown_directory(export_dir.path(), &bob, EpochMs(AT.0 + 20))?;
    assert!(!report.unchanged);
    assert!(!report.baseline_conflict);
    assert!(report.updated_facts.is_empty());
    assert!(report.deleted_fact_ids.is_empty());
    // 4 proposals: cross-author `rule_id` edit, derived `warn_id` edit, `sources.md` edit, `scratch.md`.
    assert_eq!(report.enqueued_proposals.len(), 4);

    // `alice`'s base rule remains completely unmodified!
    let live_facts = store.visible_facts(&alice, EpochMs(AT.0 + 21))?;
    assert!(live_facts.iter().any(|f| f.id == rule_id));

    // Iron Rule: `fake_secret` MUST NOT appear anywhere in ANY SQLite table!
    assert_eq!(
        find_sqlite_table_containing_bytes(db_dir.path(), fake_secret.as_bytes())?,
        None
    );

    // Non-admin `bob` cannot confirm the cross-author edit on `alice`'s `rule_id` (`PermissionDenied`),
    // while `bob` CAN confirm his own derived note and `scratch.md` freeform note!
    let bob_reviews = store.list_review_items(&bob, Some(ReviewStatus::Open))?;
    assert_eq!(bob_reviews.len(), 4);
    for rev in &bob_reviews {
        let target_fid = rev.payload.get("target_fact_id").and_then(|v| v.as_str());
        if target_fid == Some(rule_id.as_str()) {
            assert!(matches!(
                store.apply_confirmed_writeback_proposal(&rev.id, &bob, EpochMs(AT.0 + 25)),
                Err(StoreError::PermissionDenied)
            ));
        } else {
            let created =
                store.apply_confirmed_writeback_proposal(&rev.id, &bob, EpochMs(AT.0 + 25))?;
            assert!(created.is_some());
        }
    }

    assert_eq!(
        find_sqlite_table_containing_bytes(db_dir.path(), fake_secret.as_bytes())?,
        None
    );

    // Wrong `library_id` in frontmatter is rejected with `InvalidInput`.
    let tampered =
        fs::read_to_string(&index_path)?.replace("library_id: local", "library_id: other_lib");
    fs::write(&index_path, tampered)?;
    assert!(matches!(
        store.apply_markdown_directory(export_dir.path(), &bob, EpochMs(AT.0 + 30)),
        Err(StoreError::Model(KpError::InvalidInput {
            field: "library_id",
            ..
        }))
    ));

    Ok(())
}

#[test]
fn writeback_raw_only_fact_makes_zero_cloud_embedder_calls_and_preserves_raw_only() -> Result<()> {
    let db_dir = tempfile::tempdir()?;
    let mut store = crate::Store::init_with_embedding(
        db_dir.path(),
        crate::test_support::options(LibraryMode::Personal)?,
        "openai:text-embedding-3-small",
        64,
    )?;
    let cloud_emb = Arc::new(CountingCloudEmbedder::new(
        "openai:text-embedding-3-small",
        64,
    ));
    store.set_embedder(cloud_emb.clone());
    let owner: ActorId = "owner".parse()?;

    // Ingest a `RawOnly` L0 source and anchored `RawOnly` fact; cloud embedder must see 0 calls.
    let raw_src = RawSourceInput {
        self_echo_filtered: false,
        uri: "vault/notes.md".into(),
        media_type: "text/markdown".into(),
        content: b"Confidential architecture constraint: keep payroll ledger isolated.".to_vec(),
        readable_by: AudienceSpec::Explicit(BTreeSet::from([owner.clone()])),
        captured_at: AT,
        chunks: vec![ChunkInput {
            heading: "Vault".into(),
            body: "Confidential architecture constraint: keep payroll ledger isolated.".into(),
            anchor: AnchorSpec::default(),
        }],
    };
    let stored_src = store.ingest_source_with_policy(
        &"local".parse()?,
        &"vault:1".parse()?,
        &raw_src,
        &ModelRoutePolicy::RawOnly,
    )?;
    assert_eq!(cloud_emb.call_count(), 0);

    let raw_fact_id = new_fact_id()?;
    let mut raw_fact = make_base_fact(
        raw_fact_id.clone(),
        "local",
        FactKind::Constraint,
        "payroll_ledger",
        "kp.core.isolation",
        json!({
            "summary": "Confidential architecture constraint: keep payroll ledger isolated.",
            "route_policy": "raw_only"
        }),
        &owner,
        AudienceSpec::Explicit(BTreeSet::from([owner.clone()])),
        Some("Confidential architecture constraint: keep payroll ledger isolated."),
        AT,
    )?;
    raw_fact.source_anchor = Some(AnchorSpec {
        source_id: Some(stored_src.source_id.clone()),
        verbatim_quote: Some(
            "Confidential architecture constraint: keep payroll ledger isolated.".into(),
        ),
        ..AnchorSpec::default()
    });
    store.insert_fact(&raw_fact)?;
    assert_eq!(cloud_emb.call_count(), 0);
    assert_eq!(
        store.fact_effective_route_policy(&raw_fact)?,
        ModelRoutePolicy::RawOnly
    );

    // Export, edit the `RawOnly` fact in `index.md`, and `apply_markdown_directory`:
    // Counting cloud embedder MUST remain at 0 calls, and replacement MUST stay `RawOnly`!
    let export_dir = tempfile::tempdir()?;
    store.export_markdown_directory(export_dir.path(), &owner, EpochMs(AT.0 + 10))?;

    let index_path = export_dir.path().join("index.md");
    let raw_index = fs::read_to_string(&index_path)?;
    let updated_claim =
        "Confidential architecture constraint: keep payroll ledger on isolated VPC subnet.";
    fs::write(
        &index_path,
        replace_entry_line_claim(&raw_index, &raw_fact_id, updated_claim)?,
    )?;

    let apply_rep =
        store.apply_markdown_directory(export_dir.path(), &owner, EpochMs(AT.0 + 20))?;
    assert_eq!(cloud_emb.call_count(), 0);
    assert_eq!(apply_rep.updated_facts.len(), 1);

    let update = &apply_rep.updated_facts[0];
    assert_eq!(
        store.source_effective_route_policy(&update.source_id)?,
        ModelRoutePolicy::RawOnly
    );
    let vis = store.visible_facts(&owner, EpochMs(AT.0 + 25))?;
    let new_fact = vis
        .iter()
        .find(|f| f.id == update.new_fact_id)
        .ok_or_else(|| KpError::invalid("test", "missing updated raw_only fact"))?;
    assert_eq!(
        store.fact_effective_route_policy(new_fact)?,
        ModelRoutePolicy::RawOnly
    );
    let vec_rows: i64 = store.db.query_row(
        "SELECT count(*) FROM fact_vectors WHERE fact_id = ?1",
        [update.new_fact_id.as_str()],
        |r| r.get(0),
    )?;
    assert_eq!(vec_rows, 0);

    Ok(())
}

#[test]
fn visible_change_log_enforces_strict_viewer_and_departed_container_isolation() -> Result<()> {
    let (_db_dir, mut store) = library(LibraryMode::Community)?;
    let owner: ActorId = "owner".parse()?;
    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;
    let carol: ActorId = "carol".parse()?;
    let outsider: ActorId = "outsider".parse()?;
    store.add_member(alice.clone(), MemberRole::Maintainer, AT)?;
    store.add_member(bob.clone(), MemberRole::Member, AT)?;
    store.add_member(carol.clone(), MemberRole::Member, AT)?;

    // Non-member viewer is immediately rejected with `PermissionDenied`.
    assert!(matches!(
        store.visible_change_log(&outsider, AT, 50, None),
        Err(StoreError::PermissionDenied)
    ));

    // 1. `alice` syncs container `chat.grp_ops` containing `[alice, bob]` (NOT `carol`).
    store.apply_public_container_roster(
        &alice,
        &PublicContainerRoster {
            container_id: "chat.grp_ops".into(),
            label: "Ops Incident Room".into(),
            members: vec!["alice".into(), "bob".into()],
            all_library_members: false,
            synced_at: EpochMs(AT.0 + 10),
            provider: Some("chat".into()),
            upstream_container_id: Some("grp_ops".into()),
        },
    )?;

    // 2. `alice` ingests a container message into `chat.grp_ops`.
    let ing_out = store.apply_public_ingest_record(
        &PublicIngestRecord {
            source: PublicIngestSource {
                kind: "chat".into(),
                uri: "chat://grp_ops/msg_1".into(),
                external_id: "msg_1".into(),
                container_id: Some("chat.grp_ops".into()),
            },
            speaker: PublicSpeaker {
                id: "alice".into(),
                display_name: Some("Alice".into()),
            },
            audience: PublicAudience::ContainerMembers,
            occurred_at: EpochMs(AT.0 + 20),
            content: "Rotate primary database credentials tonight".into(),
            op: PublicIngestOp::Upsert,
            ingested_by: None,
            media_type: None,
            model_policy: Some(ModelRoutePolicy::RawOnly),
            thread_key: None,
            reply_to: None,
        },
        &alice,
        &ModelRoutePolicy::HostCli,
    )?;
    let grp_source_id = ing_out
        .source_id
        .ok_or_else(|| KpError::invalid("test", "expected ingested source_id"))?;

    // 3. Container-scoped fact visible to `chat.grp_ops` (`[alice, bob]`).
    let grp_fact_id = new_fact_id()?;
    let mut grp_fact = make_base_fact(
        grp_fact_id.clone(),
        "local",
        FactKind::Decision,
        "db_creds_rotation",
        "kp.core.ops",
        json!("Rotate primary database credentials tonight"),
        &alice,
        AudienceSpec::AllMembers,
        Some("Rotate primary database credentials tonight"),
        EpochMs(AT.0 + 25),
    )?;
    grp_fact.source_scope_id = Some("chat.grp_ops".parse()?);
    store.insert_fact(&grp_fact)?;

    // 4. `alice` private `SelfOnly` fact + `bob` private `SelfOnly` fact + public `carol`-visible fact.
    let alice_priv_id = new_fact_id()?;
    let alice_priv = make_base_fact(
        alice_priv_id.clone(),
        "local",
        FactKind::Observation,
        "alice_diary",
        "kp.core.private",
        json!("Private token draft for Alice only"),
        &alice,
        AudienceSpec::Explicit(BTreeSet::from([alice.clone()])),
        Some("Private token draft for Alice only"),
        EpochMs(AT.0 + 30),
    )?;
    store.insert_fact(&alice_priv)?;

    let pub_fact_id = new_fact_id()?;
    let pub_fact = make_base_fact(
        pub_fact_id.clone(),
        "local",
        FactKind::Constraint,
        "ci_checks",
        "kp.core.ci",
        json!("All pull requests must pass offline clippy"),
        &owner,
        AudienceSpec::AllMembers,
        Some("All pull requests must pass offline clippy"),
        EpochMs(AT.0 + 35),
    )?;
    store.insert_fact(&pub_fact)?;

    // 5. Derived `CollisionWarning` combining `pub_fact` + `alice_priv`: `carol` & `bob` MUST NOT see it!
    let priv_warn = derived_fact(
        &"local".parse()?,
        &alice,
        FactKind::CollisionWarning,
        "alice_diary",
        "kp.core.collision",
        json!({"summary": "Derived from Alice private premise"}),
        EpochMs(AT.0 + 36),
    )?;
    let priv_warn_id = priv_warn.id.clone();
    let priv_just = justification(
        &priv_warn,
        DerivationMode::HardRule,
        "priv_rule_v1",
        &[&pub_fact, &alice_priv],
    )?;
    store.save_derivations(&[priv_warn], &[priv_just])?;

    // Check `carol`'s audit log: sees ONLY `pub_fact_id`, ZERO private/container/internal events!
    let carol_logs = store.visible_change_log(&carol, EpochMs(AT.0 + 40), 100, None)?;
    assert_eq!(carol_logs.len(), 1);
    assert_eq!(carol_logs[0].target_id, pub_fact_id.as_str());
    assert!(carol_logs
        .iter()
        .all(|e| !e.summary.contains("Ops Incident Room")
            && !e.summary.contains("credentials")
            && !e.summary.contains("Alice")));

    // Filtering by an invisible `target_id` as `carol` returns empty `Vec`, never leaking existence.
    assert!(store
        .visible_change_log(
            &carol,
            EpochMs(AT.0 + 40),
            100,
            Some(alice_priv_id.as_str())
        )?
        .is_empty());
    // Filtering by `kp-ref:` prefix works for visible facts.
    let carol_filtered = store.visible_change_log(
        &carol,
        EpochMs(AT.0 + 40),
        100,
        Some(&format!("kp-ref:{pub_fact_id}")),
    )?;
    assert_eq!(carol_filtered.len(), 1);

    // While `bob` is in `chat.grp_ops`, `bob` sees `grp_fact_id`, `grp_source_id`, and `container_roster_sync`,
    // and still NEVER sees `alice_priv_id` or `priv_warn_id`!
    let bob_before = store.visible_change_log(&bob, EpochMs(AT.0 + 40), 100, None)?;
    let bob_targets_before: BTreeSet<&str> =
        bob_before.iter().map(|e| e.target_id.as_str()).collect();
    assert!(bob_targets_before.contains(grp_fact_id.as_str()));
    assert!(bob_targets_before.contains(grp_source_id.as_str()));
    assert!(bob_targets_before.contains("chat.grp_ops"));
    assert!(!bob_targets_before.contains(alice_priv_id.as_str()));
    assert!(!bob_targets_before.contains(priv_warn_id.as_str()));

    // Verify `seq DESC` ordering and `limit` enforcement.
    for pair in bob_before.windows(2) {
        assert!(pair[0].seq > pair[1].seq);
    }
    assert_eq!(
        store
            .visible_change_log(&bob, EpochMs(AT.0 + 40), 2, None)?
            .len(),
        2
    );

    // Now remove `bob` from `chat.grp_ops` (`members = ["alice"]`):
    // Dynamic container contraction immediately hides ALL historical `chat.grp_ops` facts, sources,
    // and `container_roster_sync` events from `bob`!
    store.apply_public_container_roster(
        &alice,
        &PublicContainerRoster {
            container_id: "chat.grp_ops".into(),
            label: "Ops Incident Room".into(),
            members: vec!["alice".into()],
            all_library_members: false,
            synced_at: EpochMs(AT.0 + 50),
            provider: Some("chat".into()),
            upstream_container_id: Some("grp_ops".into()),
        },
    )?;

    let bob_after = store.visible_change_log(&bob, EpochMs(AT.0 + 55), 100, None)?;
    let bob_targets_after: BTreeSet<&str> =
        bob_after.iter().map(|e| e.target_id.as_str()).collect();
    assert!(!bob_targets_after.contains(grp_fact_id.as_str()));
    assert!(!bob_targets_after.contains(grp_source_id.as_str()));
    assert!(!bob_targets_after.contains("chat.grp_ops"));
    assert!(bob_targets_after.contains(pub_fact_id.as_str()));

    // Meanwhile `alice` sees `alice_priv_id`, `priv_warn_id`, `grp_fact_id`, `grp_source_id`, and `chat.grp_ops`.
    let alice_logs = store.visible_change_log(&alice, EpochMs(AT.0 + 55), 100, None)?;
    let alice_targets: BTreeSet<&str> = alice_logs.iter().map(|e| e.target_id.as_str()).collect();
    assert!(alice_targets.contains(alice_priv_id.as_str()));
    assert!(alice_targets.contains(priv_warn_id.as_str()));
    assert!(alice_targets.contains(grp_fact_id.as_str()));
    assert!(alice_targets.contains(grp_source_id.as_str()));
    assert!(alice_targets.contains("chat.grp_ops"));

    Ok(())
}

#[test]
fn writeback_admin_cross_author_fact_slot_lock_serialization_and_review_audit_isolation(
) -> Result<()> {
    let (_db_dir, mut store) = library(LibraryMode::Community)?;
    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;
    let carol: ActorId = "carol".parse()?;
    store.add_member(alice.clone(), MemberRole::Maintainer, AT)?;
    store.add_member(bob.clone(), MemberRole::Member, AT)?;
    store.add_member(carol.clone(), MemberRole::Member, AT)?;

    // `bob` authors two base facts (`FactSlot` and `Observation`).
    let slot_id = new_fact_id()?;
    let slot_fact = make_base_fact(
        slot_id.clone(),
        "local",
        FactKind::FactSlot,
        "db_port",
        "kp.core.port",
        json!("5432"),
        &bob,
        AudienceSpec::AllMembers,
        None,
        AT,
    )?;
    store.insert_fact(&slot_fact)?;

    let bob_obs_id = new_fact_id()?;
    let bob_obs = make_base_fact(
        bob_obs_id.clone(),
        "local",
        FactKind::Observation,
        "temp_notice",
        "kp.core.note",
        json!("Outdated migration checklist"),
        &bob,
        AudienceSpec::AllMembers,
        Some("Outdated migration checklist"),
        AT,
    )?;
    store.insert_fact(&bob_obs)?;

    let export_dir = tempfile::tempdir()?;
    store.export_markdown_directory(export_dir.path(), &alice, EpochMs(AT.0 + 10))?;

    let index_path = export_dir.path().join("index.md");
    let raw_index = fs::read_to_string(&index_path)?;
    // Maintainer `alice` edits `bob`'s `FactSlot` (`db_port: 5432` -> `db_port: 6432`),
    // deletes `bob`'s `bob_obs_id`, and adds a personal freeform note.
    let mut edited = replace_entry_line_claim(&raw_index, &slot_id, "db_port: 6432")?;
    edited = remove_entry_block(&edited, &bob_obs_id)?;
    edited.push_str("\nMaintainer personal checklist reminder.\n");
    fs::write(&index_path, edited)?;

    // Simulate concurrent `.kp-export.lock` held briefly by another process/thread;
    // `apply_markdown_directory` must wait cleanly for release and succeed.
    let lock_path = export_dir.path().join(".kp-export.lock");
    fs::write(&lock_path, b"pid=999999\n")?;
    let unlock_handle = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(80));
        let _ = fs::remove_file(lock_path);
    });

    let apply_rep =
        store.apply_markdown_directory(export_dir.path(), &alice, EpochMs(AT.0 + 20))?;
    let _ = unlock_handle.join();
    assert!(!export_dir.path().join(".kp-export.lock").exists());

    // Maintainer `alice` superseded `bob`'s `FactSlot` and withdrew `bob_obs_id` directly,
    // while the freeform line became `alice`'s personal `writeback_proposal`.
    assert_eq!(apply_rep.updated_facts.len(), 1);
    assert_eq!(apply_rep.deleted_fact_ids, vec![bob_obs_id]);
    assert_eq!(apply_rep.enqueued_proposals.len(), 1);

    let rev_id = apply_rep.enqueued_proposals[0].clone();
    let _ = store.apply_confirmed_writeback_proposal(&rev_id, &alice, EpochMs(AT.0 + 25))?;

    // Re-export and verify `db_port: 6432` roundtrips losslessly and second apply is `unchanged`.
    store.export_markdown_directory(export_dir.path(), &alice, EpochMs(AT.0 + 30))?;
    let reexported = fs::read_to_string(&index_path)?;
    assert!(reexported.contains("db_port: 6432"));
    let apply_again =
        store.apply_markdown_directory(export_dir.path(), &alice, EpochMs(AT.0 + 35))?;
    assert!(apply_again.unchanged);

    // Review audit isolation: `alice` sees `enqueue_review` & `resolve_review` for `rev_id`,
    // whereas `bob` and `carol` see ZERO review events!
    let alice_logs = store.visible_change_log(&alice, EpochMs(AT.0 + 40), 100, None)?;
    assert!(alice_logs
        .iter()
        .any(|e| e.op == "enqueue_review" && e.target_id == rev_id.as_str()));
    assert!(alice_logs
        .iter()
        .any(|e| e.op == "resolve_review" && e.target_id == rev_id.as_str()));
    assert!(alice_logs.iter().any(|e| e.op == "writeback_supersede"));

    for peer in [&bob, &carol] {
        let peer_logs = store.visible_change_log(peer, EpochMs(AT.0 + 40), 100, None)?;
        assert!(peer_logs
            .iter()
            .all(|e| e.op != "enqueue_review" && e.op != "resolve_review"));
    }

    Ok(())
}

#[test]
fn writeback_sources_md_skips_unchanged_raw_only_and_captures_stale_entry_edits_with_zero_cloud_calls(
) -> Result<()> {
    let db_dir = tempfile::tempdir()?;
    let mut store = crate::Store::init_with_embedding(
        db_dir.path(),
        crate::test_support::options(LibraryMode::Personal)?,
        "openai:text-embedding-3-small",
        64,
    )?;
    let cloud_emb = Arc::new(CountingCloudEmbedder::new(
        "openai:text-embedding-3-small",
        64,
    ));
    store.set_embedder(cloud_emb.clone());
    let owner: ActorId = "owner".parse()?;

    // Ingest a `RawOnly` source into the library (0 cloud embedder calls).
    let raw_secret_body = "Restricted ledger architecture: HSM root ceremony stays air-gapped.";
    let raw_src = RawSourceInput {
        self_echo_filtered: false,
        uri: "vault/hsm.md".into(),
        media_type: "text/markdown".into(),
        content: raw_secret_body.as_bytes().to_vec(),
        readable_by: AudienceSpec::Explicit(BTreeSet::from([owner.clone()])),
        captured_at: AT,
        chunks: vec![ChunkInput {
            heading: "HSM".into(),
            body: raw_secret_body.into(),
            anchor: AnchorSpec::default(),
        }],
    };
    let stored_raw = store.ingest_source_with_policy(
        &"local".parse()?,
        &"vault:hsm".parse()?,
        &raw_src,
        &ModelRoutePolicy::RawOnly,
    )?;
    assert_eq!(cloud_emb.call_count(), 0);

    // Part A (Clean baseline): append a freeform note to `sources.md`.
    // The untouched `RawOnly` source summary MUST NOT be bundled into the proposal payload!
    let dir_clean = tempfile::tempdir()?;
    store.export_markdown_directory(dir_clean.path(), &owner, EpochMs(AT.0 + 10))?;

    let clean_sources_path = dir_clean.path().join("sources.md");
    let mut clean_sources_md = fs::read_to_string(&clean_sources_path)?;
    clean_sources_md.push_str("\nSchedule quarterly review of source inventory.\n");
    fs::write(&clean_sources_path, clean_sources_md)?;

    let rep_clean = store.apply_markdown_directory_with_policy(
        dir_clean.path(),
        &owner,
        &ModelRoutePolicy::RawOnly,
        EpochMs(AT.0 + 20),
    )?;
    assert_eq!(rep_clean.enqueued_proposals.len(), 1);
    let rev_clean = store
        .get_review_item(&rep_clean.enqueued_proposals[0])?
        .ok_or_else(|| KpError::invalid("test", "missing clean sources.md review"))?;
    let raw_md_payload = rev_clean
        .payload
        .get("raw_markdown")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(!raw_md_payload.contains("air-gapped"));
    assert!(raw_md_payload.contains("Schedule quarterly review"));
    let _ = store.apply_confirmed_writeback_proposal_with_policy(
        &rev_clean.id,
        &owner,
        &ModelRoutePolicy::RawOnly,
        EpochMs(AT.0 + 25),
    )?;
    assert_eq!(cloud_emb.call_count(), 0);

    // Part B (Stale baseline): export `dir_stale`, advance `change_log.seq` via another `RawOnly`
    // write, then edit the generated `- **`vault/hsm.md`** (...): <edited summary>` line in stale `sources.md`!
    let dir_stale = tempfile::tempdir()?;
    store.export_markdown_directory(dir_stale.path(), &owner, EpochMs(AT.0 + 30))?;

    let bump_fact_id = new_fact_id()?;
    let mut bump_fact = make_base_fact(
        bump_fact_id,
        "local",
        FactKind::Observation,
        "bump",
        "kp.core.note",
        json!({"summary": "Advance seq for stale baseline", "route_policy": "raw_only"}),
        &owner,
        AudienceSpec::Explicit(BTreeSet::from([owner.clone()])),
        Some("Advance seq for stale baseline"),
        EpochMs(AT.0 + 32),
    )?;
    bump_fact.tags.insert("kp.raw_only".to_owned());
    store.insert_fact(&bump_fact)?;
    assert_eq!(cloud_emb.call_count(), 0);

    let stale_sources_path = dir_stale.path().join("sources.md");
    let stale_sources_md = fs::read_to_string(&stale_sources_path)?;
    let marker = format!("<!-- kp-source: {} -->", stored_raw.source_id);
    assert!(stale_sources_md.contains(&marker));
    let edited_stale_sources = stale_sources_md.replace(
        raw_secret_body,
        "Restricted ledger architecture: HSM ceremony requires dual hardware tokens.",
    );
    assert_ne!(stale_sources_md, edited_stale_sources);
    fs::write(&stale_sources_path, edited_stale_sources)?;

    let rep_stale = store.apply_markdown_directory(dir_stale.path(), &owner, EpochMs(AT.0 + 40))?;
    assert!(rep_stale.baseline_conflict);
    // Stale edit to generated `sources.md` bullet MUST NOT be silently dropped!
    assert_eq!(rep_stale.enqueued_proposals.len(), 1);

    let rev_stale = store
        .get_review_item(&rep_stale.enqueued_proposals[0])?
        .ok_or_else(|| KpError::invalid("test", "missing stale sources.md review"))?;
    assert_eq!(
        rev_stale.payload.get("route_policy"),
        Some(&serde_json::to_value(ModelRoutePolicy::RawOnly)?)
    );

    // Confirming even under default `HostCli` policy must honor `payload.route_policy == "raw_only"`
    // and make ZERO cloud `Embedder` calls!
    let created_fid = store
        .apply_confirmed_writeback_proposal(&rev_stale.id, &owner, EpochMs(AT.0 + 45))?
        .ok_or_else(|| KpError::invalid("test", "expected confirmed note fact id"))?;
    assert_eq!(cloud_emb.call_count(), 0);

    let vis = store.visible_facts(&owner, EpochMs(AT.0 + 50))?;
    let note_fact = vis
        .iter()
        .find(|f| f.id == created_fid)
        .ok_or_else(|| KpError::invalid("test", "missing confirmed note fact"))?;
    assert_eq!(
        store.fact_effective_route_policy(note_fact)?,
        ModelRoutePolicy::RawOnly
    );

    Ok(())
}

#[test]
fn writeback_departed_raw_only_container_skips_untouched_entries_and_shields_edited_ones(
) -> Result<()> {
    let db_dir = tempfile::tempdir()?;
    let mut store = crate::Store::init_with_embedding(
        db_dir.path(),
        crate::test_support::options(LibraryMode::Community)?,
        "openai:text-embedding-3-small",
        64,
    )?;
    let cloud_emb = Arc::new(CountingCloudEmbedder::new(
        "openai:text-embedding-3-small",
        64,
    ));
    store.set_embedder(cloud_emb.clone());

    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;
    store.add_member(alice.clone(), MemberRole::Maintainer, AT)?;
    store.add_member(bob.clone(), MemberRole::Member, AT)?;

    store.apply_public_container_roster(
        &alice,
        &PublicContainerRoster {
            container_id: "chat.oc_raw".into(),
            label: "Restricted SecOps Room".into(),
            members: vec!["alice".into(), "bob".into()],
            all_library_members: false,
            synced_at: EpochMs(AT.0 + 5),
            provider: Some("chat".into()),
            upstream_container_id: Some("oc_raw".into()),
        },
    )?;

    // `RawOnly` container fact `fact_raw` + `bob`'s personal `RawOnly` fact `fact_own`.
    let fact_raw_id = new_fact_id()?;
    let mut fact_raw = make_base_fact(
        fact_raw_id.clone(),
        "local",
        FactKind::Constraint,
        "secops_key",
        "kp.core.secops",
        json!({"summary": "Never export SecOps signing keys outside enclave", "route_policy": "raw_only"}),
        &alice,
        AudienceSpec::AllMembers,
        Some("Never export SecOps signing keys outside enclave"),
        EpochMs(AT.0 + 10),
    )?;
    fact_raw.source_scope_id = Some("chat.oc_raw".parse()?);
    fact_raw.tags.insert("kp.raw_only".to_owned());
    store.insert_fact(&fact_raw)?;

    let fact_own_id = new_fact_id()?;
    let mut fact_own = make_base_fact(
        fact_own_id.clone(),
        "local",
        FactKind::Decision,
        "bob_editor",
        "kp.core.pref",
        json!({"summary": "Prefer local terminal workflow", "route_policy": "raw_only"}),
        &bob,
        AudienceSpec::Explicit(BTreeSet::from([bob.clone()])),
        Some("Prefer local terminal workflow"),
        EpochMs(AT.0 + 12),
    )?;
    fact_own.tags.insert("kp.raw_only".to_owned());
    store.insert_fact(&fact_own)?;
    assert_eq!(cloud_emb.call_count(), 0);

    // `bob` exports two directories while still in `chat.oc_raw`.
    let dir_untouched = tempfile::tempdir()?;
    let dir_edited = tempfile::tempdir()?;
    store.export_markdown_directory(dir_untouched.path(), &bob, EpochMs(AT.0 + 15))?;
    store.export_markdown_directory(dir_edited.path(), &bob, EpochMs(AT.0 + 16))?;

    // `bob` leaves `chat.oc_raw` (`members = ["alice"]`).
    store.apply_public_container_roster(
        &alice,
        &PublicContainerRoster {
            container_id: "chat.oc_raw".into(),
            label: "Restricted SecOps Room".into(),
            members: vec!["alice".into()],
            all_library_members: false,
            synced_at: EpochMs(AT.0 + 20),
            provider: Some("chat".into()),
            upstream_container_id: Some("oc_raw".into()),
        },
    )?;

    // Case 1: `bob` edits ONLY `fact_own` in `dir_untouched` and leaves departed `fact_raw` untouched.
    // `fact_raw` MUST NOT generate any proposal and MUST NOT resurrect!
    let idx_untouched = dir_untouched.path().join("index.md");
    let raw_idx_untouched = fs::read_to_string(&idx_untouched)?;
    fs::write(
        &idx_untouched,
        replace_entry_line_claim(
            &raw_idx_untouched,
            &fact_own_id,
            "Prefer Helix terminal workflow",
        )?,
    )?;

    let rep1 = store.apply_markdown_directory(dir_untouched.path(), &bob, EpochMs(AT.0 + 25))?;
    assert!(rep1.baseline_conflict);
    assert_eq!(rep1.enqueued_proposals.len(), 1);
    let rev1 = store
        .get_review_item(&rep1.enqueued_proposals[0])?
        .ok_or_else(|| KpError::invalid("test", "missing fact_own proposal"))?;
    assert_eq!(
        rev1.payload.get("target_fact_id"),
        Some(&json!(fact_own_id.as_str()))
    );
    let _ = store.apply_confirmed_writeback_proposal(&rev1.id, &bob, EpochMs(AT.0 + 26))?;
    assert_eq!(cloud_emb.call_count(), 0);

    let bob_vis = store.visible_facts(&bob, EpochMs(AT.0 + 27))?;
    assert_eq!(bob_vis.len(), 1);
    assert!(bob_vis
        .iter()
        .all(|f| !f.object.to_string().contains("enclave")));

    // Case 2: `bob` explicitly edits the departed `fact_raw` entry in `dir_edited`.
    // Proposal inherits `route_policy = "raw_only"` so confirming makes ZERO cloud `Embedder` calls!
    let idx_edited = dir_edited.path().join("index.md");
    let raw_idx_edited = fs::read_to_string(&idx_edited)?;
    fs::write(
        &idx_edited,
        replace_entry_line_claim(
            &raw_idx_edited,
            &fact_raw_id,
            "Personal reminder: ask SecOps team before hardware token rotation",
        )?,
    )?;

    let rep2 = store.apply_markdown_directory(dir_edited.path(), &bob, EpochMs(AT.0 + 30))?;
    assert_eq!(rep2.enqueued_proposals.len(), 1);
    let rev2 = store
        .get_review_item(&rep2.enqueued_proposals[0])?
        .ok_or_else(|| KpError::invalid("test", "missing edited departed entry proposal"))?;
    assert_eq!(
        rev2.payload.get("route_policy"),
        Some(&serde_json::to_value(ModelRoutePolicy::RawOnly)?)
    );

    let note_id = store
        .apply_confirmed_writeback_proposal(&rev2.id, &bob, EpochMs(AT.0 + 35))?
        .ok_or_else(|| KpError::invalid("test", "expected confirmed personal note id"))?;
    assert_eq!(cloud_emb.call_count(), 0);

    let bob_vis_after = store.visible_facts(&bob, EpochMs(AT.0 + 36))?;
    let note_fact = bob_vis_after
        .iter()
        .find(|f| f.id == note_id)
        .ok_or_else(|| KpError::invalid("test", "missing confirmed personal note"))?;
    assert_eq!(
        store.fact_effective_route_policy(note_fact)?,
        ModelRoutePolicy::RawOnly
    );

    Ok(())
}

#[test]
fn writeback_derived_fact_with_sourceless_raw_only_tagged_premise_makes_zero_cloud_calls(
) -> Result<()> {
    let db_dir = tempfile::tempdir()?;
    let mut store = crate::Store::init_with_embedding(
        db_dir.path(),
        crate::test_support::options(LibraryMode::Personal)?,
        "openai:text-embedding-3-small",
        64,
    )?;
    let cloud_emb = Arc::new(CountingCloudEmbedder::new(
        "openai:text-embedding-3-small",
        64,
    ));
    store.set_embedder(cloud_emb.clone());
    let owner: ActorId = "owner".parse()?;

    // Premise has `source_anchor = None` and carries only `"raw_only"` in `tags`.
    let prem_id = new_fact_id()?;
    let mut prem = make_base_fact(
        prem_id.clone(),
        "local",
        FactKind::Constraint,
        "airgap_rule",
        "kp.core.airgap",
        json!("Keep enclave signing service offline"),
        &owner,
        AudienceSpec::Explicit(BTreeSet::from([owner.clone()])),
        None,
        AT,
    )?;
    prem.tags.insert("raw_only".to_owned());
    store.insert_fact(&prem)?;
    assert_eq!(cloud_emb.call_count(), 0);

    let warn = derived_fact(
        &"local".parse()?,
        &owner,
        FactKind::CollisionWarning,
        "airgap_rule",
        "kp.core.collision",
        json!({"summary": "Outbound webhook conflicts with airgap constraint"}),
        EpochMs(AT.0 + 5),
    )?;
    let warn_id = warn.id.clone();
    let just = justification(&warn, DerivationMode::HardRule, "airgap_v1", &[&prem])?;
    store.save_derivations(&[warn], &[just])?;
    assert_eq!(cloud_emb.call_count(), 0);

    let export_dir = tempfile::tempdir()?;
    store.export_markdown_directory(export_dir.path(), &owner, EpochMs(AT.0 + 10))?;

    let index_path = export_dir.path().join("index.md");
    let raw_index = fs::read_to_string(&index_path)?;
    fs::write(
        &index_path,
        replace_entry_line_claim(
            &raw_index,
            &warn_id,
            "Verified webhook stays inside loopback airgap boundary",
        )?,
    )?;

    let apply_rep =
        store.apply_markdown_directory(export_dir.path(), &owner, EpochMs(AT.0 + 20))?;
    assert_eq!(apply_rep.enqueued_proposals.len(), 1);
    let rev_id = &apply_rep.enqueued_proposals[0];
    let created_id = store
        .apply_confirmed_writeback_proposal(rev_id, &owner, EpochMs(AT.0 + 25))?
        .ok_or_else(|| KpError::invalid("test", "expected confirmed derived note id"))?;
    assert_eq!(cloud_emb.call_count(), 0);

    let vis = store.visible_facts(&owner, EpochMs(AT.0 + 30))?;
    let created = vis
        .iter()
        .find(|f| f.id == created_id)
        .ok_or_else(|| KpError::invalid("test", "missing derived note"))?;
    assert_eq!(
        store.fact_effective_route_policy(created)?,
        ModelRoutePolicy::RawOnly
    );

    Ok(())
}

#[test]
fn writeback_derived_proposal_allows_non_owner_after_premise_edit_and_denies_departed_owner(
) -> Result<()> {
    let (_db_dir, mut store) = library(LibraryMode::Community)?;
    let owner: ActorId = "owner".parse()?;
    let alice: ActorId = "alice".parse()?;
    store.add_member(alice.clone(), MemberRole::Maintainer, AT)?;

    // Part A: `alice` (`alice != owner`) edits her own premise `p1` AND annotates an `owner`-authored
    // derived warning `warn_a` in the same `index.md`, then confirms `warn_a`'s proposal!
    let p1_id = new_fact_id()?;
    let p1 = make_base_fact(
        p1_id.clone(),
        "local",
        FactKind::Constraint,
        "cache_ttl",
        "kp.core.ttl",
        json!("Cache TTL must not exceed 60s"),
        &alice,
        AudienceSpec::AllMembers,
        Some("Cache TTL must not exceed 60s"),
        AT,
    )?;
    store.insert_fact(&p1)?;

    let p2_id = new_fact_id()?;
    let p2 = make_base_fact(
        p2_id,
        "local",
        FactKind::Event,
        "cdn_config",
        "kp.core.cdn",
        json!("Configured CDN edge TTL to 300s"),
        &owner,
        AudienceSpec::AllMembers,
        Some("Configured CDN edge TTL to 300s"),
        AT,
    )?;
    store.insert_fact(&p2)?;

    let warn_a = derived_fact(
        &"local".parse()?,
        &owner,
        FactKind::CollisionWarning,
        "cache_ttl",
        "kp.core.collision",
        json!({"summary": "CDN TTL 300s exceeds 60s limit"}),
        EpochMs(AT.0 + 5),
    )?;
    let warn_a_id = warn_a.id.clone();
    let just_a = justification(&warn_a, DerivationMode::HardRule, "ttl_rule", &[&p1, &p2])?;
    store.save_derivations(&[warn_a], &[just_a])?;

    let dir_alice = tempfile::tempdir()?;
    store.export_markdown_directory(dir_alice.path(), &alice, EpochMs(AT.0 + 10))?;

    let idx_alice = dir_alice.path().join("index.md");
    let raw_idx_alice = fs::read_to_string(&idx_alice)?;
    let step1 = replace_entry_line_claim(&raw_idx_alice, &p1_id, "Cache TTL must not exceed 120s")?;
    let step2 = replace_entry_line_claim(
        &step1,
        &warn_a_id,
        "Raised cache TTL ceiling to 120s; CDN still needs adjustment",
    )?;
    fs::write(&idx_alice, step2)?;

    let rep_alice = store.apply_markdown_directory(dir_alice.path(), &alice, EpochMs(AT.0 + 20))?;
    // `p1` was superseded in place (invalidating `warn_a`), and `warn_a` became an open proposal.
    assert_eq!(rep_alice.updated_facts.len(), 1);
    assert_eq!(rep_alice.enqueued_proposals.len(), 1);

    // `alice` (`!= owner`) MUST succeed in confirming her annotation even though `warn_a` is now `Invalidated`!
    let confirmed_alice_note = store.apply_confirmed_writeback_proposal(
        &rep_alice.enqueued_proposals[0],
        &alice,
        EpochMs(AT.0 + 25),
    )?;
    assert!(confirmed_alice_note.is_some());

    // Part B: `owner` is inside container `chat.oc_ops`, queues a derived warning proposal, then
    // leaves `chat.oc_ops` -> confirming as `owner` MUST fail with `PermissionDenied` despite `warn.author == owner`!
    store.apply_public_container_roster(
        &alice,
        &PublicContainerRoster {
            container_id: "chat.oc_ops".into(),
            label: "Ops Container".into(),
            members: vec!["owner".into(), "alice".into()],
            all_library_members: false,
            synced_at: EpochMs(AT.0 + 30),
            provider: Some("chat".into()),
            upstream_container_id: Some("oc_ops".into()),
        },
    )?;

    let p_grp_id = new_fact_id()?;
    let mut p_grp = make_base_fact(
        p_grp_id,
        "local",
        FactKind::Constraint,
        "secret_ops_subject",
        "kp.core.ops_guard",
        json!("Require two-person approval in Ops Container"),
        &alice,
        AudienceSpec::AllMembers,
        Some("Require two-person approval in Ops Container"),
        EpochMs(AT.0 + 32),
    )?;
    p_grp.source_scope_id = Some("chat.oc_ops".parse()?);
    store.insert_fact(&p_grp)?;

    let warn_grp = derived_fact(
        &"local".parse()?,
        &owner,
        FactKind::CollisionWarning,
        "secret_ops_subject",
        "kp.core.collision",
        json!({"summary": "Ops container guard warning"}),
        EpochMs(AT.0 + 34),
    )?;
    let warn_grp_id = warn_grp.id.clone();
    let just_grp = justification(&warn_grp, DerivationMode::HardRule, "ops_v1", &[&p_grp])?;
    store.save_derivations(&[warn_grp], &[just_grp])?;

    let dir_owner = tempfile::tempdir()?;
    store.export_markdown_directory(dir_owner.path(), &owner, EpochMs(AT.0 + 35))?;

    let idx_owner = dir_owner.path().join("index.md");
    let raw_idx_owner = fs::read_to_string(&idx_owner)?;
    fs::write(
        &idx_owner,
        replace_entry_line_claim(&raw_idx_owner, &warn_grp_id, "Owner note on ops warning")?,
    )?;

    let rep_owner = store.apply_markdown_directory(dir_owner.path(), &owner, EpochMs(AT.0 + 40))?;
    assert_eq!(rep_owner.enqueued_proposals.len(), 1);
    let owner_rev_id = rep_owner.enqueued_proposals[0].clone();

    // Remove `owner` from `chat.oc_ops`!
    store.apply_public_container_roster(
        &alice,
        &PublicContainerRoster {
            container_id: "chat.oc_ops".into(),
            label: "Ops Container".into(),
            members: vec!["alice".into()],
            all_library_members: false,
            synced_at: EpochMs(AT.0 + 45),
            provider: Some("chat".into()),
            upstream_container_id: Some("oc_ops".into()),
        },
    )?;

    assert!(matches!(
        store.apply_confirmed_writeback_proposal(&owner_rev_id, &owner, EpochMs(AT.0 + 50)),
        Err(StoreError::PermissionDenied)
    ));

    Ok(())
}

#[test]
fn writeback_supersede_preserves_fact_concept_links_for_guards() -> Result<()> {
    let (_db_dir, mut store) = library(LibraryMode::Community)?;
    let owner: ActorId = "owner".parse()?;

    let cpt_cond = store.ensure_community_concept(
        kp_model::ConceptKind::Condition,
        "production database migration",
        "ops",
        kp_model::ConceptPolarity::Neutral,
        &owner,
        AT,
    )?;
    let cpt_act = store.ensure_community_concept(
        kp_model::ConceptKind::Action,
        "drop column without backup",
        "ops",
        kp_model::ConceptPolarity::Negative,
        &owner,
        AT,
    )?;

    let rule_id = new_fact_id()?;
    let rule = make_base_fact(
        rule_id.clone(),
        "local",
        FactKind::Constraint,
        "db_migration",
        "kp.core.guard",
        json!("Never drop a production column without taking a verified snapshot first"),
        &owner,
        AudienceSpec::AllMembers,
        Some("Never drop a production column without taking a verified snapshot first"),
        AT,
    )?;
    store.insert_fact(&rule)?;
    store.exchange_link_fact_concepts(&rule_id, &[cpt_cond.id.clone(), cpt_act.id.clone()], AT)?;

    let export_dir = tempfile::tempdir()?;
    store.export_markdown_directory(export_dir.path(), &owner, EpochMs(AT.0 + 10))?;

    let index_path = export_dir.path().join("index.md");
    let raw_index = fs::read_to_string(&index_path)?;
    fs::write(
        &index_path,
        replace_entry_line_claim(
            &raw_index,
            &rule_id,
            "Never drop a production column without taking and verifying a point-in-time backup",
        )?,
    )?;

    let rep = store.apply_markdown_directory(export_dir.path(), &owner, EpochMs(AT.0 + 20))?;
    assert_eq!(rep.updated_facts.len(), 1);
    let new_rule_id = rep.updated_facts[0].new_fact_id.clone();

    let linked_cids: BTreeSet<String> = {
        let mut stmt = store.db.prepare(
            "SELECT logical_key FROM change_log
             WHERE op = 'fact_concept' AND target_id = ?1 AND logical_key IS NOT NULL",
        )?;
        let rows = stmt.query_map([new_rule_id.as_str()], |r| r.get::<_, String>(0))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    assert!(linked_cids.contains(cpt_cond.id.as_str()));
    assert!(linked_cids.contains(cpt_act.id.as_str()));

    let guards = store.visible_guard_candidates(&owner, EpochMs(AT.0 + 25))?;
    let guard_hit = guards
        .iter()
        .find(|g| g.fact.id == new_rule_id)
        .ok_or_else(|| KpError::invalid("test", "missing updated guard candidate"))?;
    let guard_cids: BTreeSet<_> = guard_hit
        .linked_concepts
        .iter()
        .map(|c| c.id.clone())
        .collect();
    assert!(guard_cids.contains(&cpt_cond.id));
    assert!(guard_cids.contains(&cpt_act.id));

    Ok(())
}
