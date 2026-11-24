use kp_model::{ActorId, AudienceSpec, EpochMs, LibraryMode};
use kp_store::{InitOptions, Store};

use super::*;
use crate::{add_markdown, markdown_chunks, FunnelOptions};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[test]
fn rule_and_structured_extractors_preserve_kinds_entities_and_values() -> Result<()> {
    let facts = RuleExtractor.extract("# Travel\nfact: color = \"green\"\nevent: Friday\nconstraint: Avoid [[Storm]]\ndecision: Take train\nobservation: Wind\nclosed_negative: Route closed", "default")?;
    assert_eq!(
        facts.iter().map(|f| f.kind).collect::<Vec<_>>(),
        vec![
            FactKind::FactSlot,
            FactKind::Event,
            FactKind::Constraint,
            FactKind::Decision,
            FactKind::Observation,
            FactKind::ClosedNegative
        ]
    );
    assert_eq!(facts[0].predicate, "color");
    assert_eq!(facts[0].value, serde_json::json!("green"));
    assert!(facts[2].entities.contains("storm"));
    let front = "---\nsubject: Alice\npredicate: favorite\nvalue: 42\nkind: fact_slot\nentities: [\"Alice\"]\n---\n";
    assert_eq!(RuleExtractor.extract(front, "default")?[0].value, 42);
    let json = r#"{"subject":"Alice","predicate":"x","value":true,"kind":"decision"}"#;
    assert_eq!(
        RuleExtractor.extract(&format!("```kp\n{json}\n```"), "default")?[0].kind,
        FactKind::Decision
    );
    assert_eq!(
        RuleExtractor.extract(&format!("```json\n{json}\n```"), "notes.md")?[0].kind,
        FactKind::Decision
    );
    assert_eq!(
        RuleExtractor.extract(&format!("[{json}]"), "facts.json")?[0].subject,
        "Alice"
    );
    assert!(RuleExtractor
        .extract("---\nsubject: Alice", "default")
        .is_err());
    Ok(())
}

#[test]
fn empty_markdown_headings_preserve_the_default_or_previous_subject() -> Result<()> {
    for heading in ["#", "##", "###   ", "######\t"] {
        let markdown = format!("{heading}\nobservation: first\n# Travel\n{heading}\nconstraint: check schedule\n## Plans\nevent: Friday");
        let facts = RuleExtractor.extract(&markdown, "default")?;
        assert_eq!(
            facts
                .iter()
                .map(|fact| fact.subject.as_str())
                .collect::<Vec<_>>(),
            vec!["default", "Travel", "Plans"],
            "{heading:?}"
        );
        assert_eq!(facts[1].value, serde_json::json!("check schedule"));
    }
    Ok(())
}

#[test]
fn anchored_funnel_persists_raw_vectors_entities_and_survives_reopen() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::init(
        dir.path(),
        InitOptions {
            library_id: "test".parse()?,
            name: "Test".into(),
            mode: LibraryMode::Personal,
            owner: "alice".parse()?,
            created_at: EpochMs(1700000000000),
        },
    )?;
    let actor: ActorId = "alice".parse()?;
    let options = FunnelOptions {
        actor: actor.clone(),
        audience: AudienceSpec::Explicit(BTreeSet::from([actor.clone()])),
        at: EpochMs(1700000000000),
    };
    let markdown = "# Travel\n\nconstraint: Avoid [[Bridge]]\n\n```rust\nlet sky = 1;\n```\n";
    let result = add_markdown(
        &mut store,
        "travel.md",
        markdown,
        &options,
        Some(&RuleExtractor),
    )?;
    assert_eq!(result.extracted.facts.len(), 1);
    assert_eq!(
        store.raw_source_text(&result.source.source_id)?.as_deref(),
        Some(markdown)
    );
    assert!(!dir.path().join("raw").exists());
    for chunk in markdown_chunks(markdown)? {
        let start = chunk.anchor.start_byte.ok_or("start")? as usize;
        let end = chunk.anchor.end_byte.ok_or("end")? as usize;
        assert_eq!(&markdown[start..end], chunk.body);
    }
    drop(store);
    let store = Store::open(dir.path())?;
    let snapshot = store.retrieval_snapshot(&actor, options.at)?;
    assert!(snapshot
        .documents()
        .iter()
        .any(|d| d.entities.contains("bridge")));
    assert!(snapshot.documents().iter().all(|d| d.vector.len() == 256));
    assert!(store
        .retrieval_snapshot(&"bob".parse()?, options.at)?
        .documents()
        .is_empty());
    Ok(())
}

#[test]
fn invalid_extracted_slot_rolls_back_l0_l1_and_raw_blobs() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::init(
        dir.path(),
        InitOptions {
            library_id: "test".parse()?,
            name: "Test".into(),
            mode: LibraryMode::Personal,
            owner: "alice".parse()?,
            created_at: EpochMs(1700000000000),
        },
    )?;
    store.define_slot(&kp_model::SlotContract {
        predicate: "age".into(),
        namespace: "kp.community.test".into(),
        cardinality: kp_model::Cardinality::Single,
        mutability: kp_model::Mutability::Fixed,
        value_kind: "number".into(),
        description: "Age".into(),
        instruction: "Use a number".into(),
    })?;
    let options = FunnelOptions {
        actor: "alice".parse()?,
        audience: AudienceSpec::AllMembers,
        at: EpochMs(1700000000000),
    };
    assert!(add_markdown(
        &mut store,
        "bad.md",
        "```kp\n{\"subject\":\"Alice\",\"predicate\":\"age\",\"value\":\"invalid\",\"kind\":\"fact_slot\"}\n```",
        &options,
        Some(&RuleExtractor)
    )
    .is_err());
    assert_eq!(store.status()?.sources, 0);
    assert_eq!(store.status()?.facts, 0);
    assert!(!dir.path().join("raw").exists());
    assert_eq!(
        kp_store::find_sqlite_table_containing_bytes(dir.path(), b"\"invalid\"")?,
        None
    );
    Ok(())
}

#[test]
fn markdown_starting_with_brackets_or_braces_and_prose_emit_zero_observation_spam() -> Result<()> {
    let review_doc = concat!(
        "[Claude] Review of Unit 1 (connectors). Forward to worker after verification.\n\n",
        "All 255 tests passed in workspace check.\n\n",
        "## 1. Command flag audit\n\n",
        "- [Reference link](https://example.com/docs) with leading bracket\n",
        "- {placeholder} syntax at the start of a prose bullet\n",
        "- **event**: bold markdown label is prose, not a slot directive\n\n",
        "| Column | Notes |\n",
        "| --- | --- |\n",
        "| alpha | beta |\n\n",
        "```json\n",
        "{\"permissionDecision\": \"ask\", \"status\": \"ok\"}\n",
        "```\n",
    );

    assert!(RuleExtractor.extract(review_doc, "review.md")?.is_empty());
    assert!(RuleExtractor
        .extract_with_media_type(review_doc, "review.json", Some("text/markdown"))?
        .is_empty());
    assert!(RuleExtractor
        .extract_with_media_type(
            "{leading_brace} plain text note",
            "note.txt",
            Some("text/plain")
        )?
        .is_empty());

    let doc_with_hugo_frontmatter =
        "---\ntitle: Architecture Notes\ntags:\n  - storage\n  - sync\n---\n# Overview\nPlain paragraph.\n";
    assert!(RuleExtractor
        .extract(doc_with_hugo_frontmatter, "overview.md")?
        .is_empty());

    let dir = tempfile::tempdir()?;
    let mut store = Store::init(
        dir.path(),
        InitOptions {
            library_id: "test".parse()?,
            name: "Test".into(),
            mode: LibraryMode::Personal,
            owner: "alice".parse()?,
            created_at: EpochMs(1700000000000),
        },
    )?;
    let actor: ActorId = "alice".parse()?;
    let options = FunnelOptions {
        actor: actor.clone(),
        audience: AudienceSpec::Explicit(BTreeSet::from([actor.clone()])),
        at: EpochMs(1700000000000),
    };
    let added = add_markdown(
        &mut store,
        "kp-review-unit1.md",
        review_doc,
        &options,
        Some(&RuleExtractor),
    )?;
    assert!(added.extracted.facts.is_empty());
    assert_eq!(store.status()?.sources, 1);
    assert_eq!(store.status()?.facts, 0);
    let snapshot = store.retrieval_snapshot(&actor, options.at)?;
    assert!(!snapshot.documents().is_empty());
    assert!(snapshot.documents().iter().all(|d| d.kind == "chunk"));
    Ok(())
}

#[test]
fn malformed_json_reports_input_uri_and_never_invalid_stored_json() {
    let cases = [
        (
            RuleExtractor.extract("[Claude] not valid json", "product/broken.json"),
            "product/broken.json",
        ),
        (
            RuleExtractor.extract("{\"subject\": \"Alice\"", "bad-object.json"),
            "bad-object.json",
        ),
        (
            RuleExtractor.extract_with_media_type(
                "{not_json}",
                "mem://inline-payload",
                Some("application/json; charset=utf-8"),
            ),
            "mem://inline-payload",
        ),
        (
            RuleExtractor.extract("```kp\n{broken}\n```", "docs/spec.md"),
            "docs/spec.md",
        ),
    ];

    for (outcome, expected_uri) in cases {
        let message = match outcome {
            Ok(_) => panic!("expected invalid input JSON error for {expected_uri}"),
            Err(err) => err.to_string(),
        };
        assert!(
            message.contains(&format!("invalid input JSON in {expected_uri}:")),
            "unexpected error message for {expected_uri}: {message}"
        );
        assert!(
            !message.contains("invalid stored JSON"),
            "leaked StoreError::Json wording for {expected_uri}: {message}"
        );
    }
}

#[test]
fn markdown_illustrative_invalid_json_fence_skips_cleanly_while_kp_fence_errors() -> Result<()> {
    let doc_a = concat!(
        "# API 返回示例\n\n",
        "```json\n",
        "{\"id\": 1, \"items\": [ ... ], // 省略}\n",
        "```\n\n",
        "Inline example: ```json {\"id\": 1, \"items\": [ ... ], // 省略} ```\n\n",
        "```json\n",
        "{\"subject\": \"Ticket #42\", \"total\": 99}\n",
        "```\n",
    );
    let doc_b = "# 普通文档\n\n这是一段普通说明文字，没有结构化事实槽位。\n";

    assert!(RuleExtractor.extract(doc_a, "a.md")?.is_empty());
    assert!(RuleExtractor.extract(doc_b, "b.md")?.is_empty());

    let dir = tempfile::tempdir()?;
    let mut store = Store::init(
        dir.path(),
        InitOptions {
            library_id: "test".parse()?,
            name: "Test".into(),
            mode: LibraryMode::Personal,
            owner: "alice".parse()?,
            created_at: EpochMs(1700000000000),
        },
    )?;
    let options = FunnelOptions {
        actor: "alice".parse()?,
        audience: AudienceSpec::AllMembers,
        at: EpochMs(1700000000000),
    };
    let added_a = add_markdown(&mut store, "a.md", doc_a, &options, Some(&RuleExtractor))?;
    let added_b = add_markdown(&mut store, "b.md", doc_b, &options, Some(&RuleExtractor))?;
    assert!(added_a.extracted.facts.is_empty());
    assert!(added_b.extracted.facts.is_empty());
    assert_eq!(store.status()?.sources, 2);
    assert_eq!(store.status()?.facts, 0);

    // Explicit ```kp fence with invalid JSON syntax MUST still fail with `invalid input JSON in a.md`.
    let broken_kp = "# Broken KP\n```kp\n{\"id\": 1, \"items\": [ ... ], // 省略}\n```\n";
    let err_msg = match RuleExtractor.extract(broken_kp, "a.md") {
        Ok(_) => return Err("expected ```kp invalid JSON to fail".into()),
        Err(err) => err.to_string(),
    };
    assert!(err_msg.contains("invalid input JSON in a.md:"));
    Ok(())
}

#[test]
fn source_code_and_non_markdown_uris_skip_directive_lookalikes() -> Result<()> {
    let rust_chunk = "pub struct Audit {\n    event: String,\n    decision: bool,\n}\n";
    assert!(RuleExtractor
        .extract(rust_chunk, "src/audit.rs")?
        .is_empty());
    assert!(RuleExtractor
        .extract("[00:01] speaker line", "meeting.vtt")?
        .is_empty());
    Ok(())
}

#[test]
fn bracket_command_guard_tags_parse_deterministically_without_polarity_guessing() -> Result<()> {
    let md = concat!(
        "# Python & Prisma Rules\n",
        "- constraint: [forbid: `pip install`, `pip3 install`] [allow: `uv pip install`] Use [[uv]] for dependency installation\n",
        "- decision: Require explicit schema path [forbid: prisma migrate deploy] [require_flag: `--schema`]\n",
        "- constraint: Always run `cargo clippy` before opening a PR\n",
    );
    let facts = RuleExtractor.extract(md, "rules.md")?;
    assert_eq!(facts.len(), 3);

    assert_eq!(facts[0].subject, "Python & Prisma Rules");
    assert_eq!(facts[0].kind, FactKind::Constraint);
    assert!(facts[0].entities.contains("uv"));
    assert_eq!(
        facts[0].value,
        serde_json::json!({
            "summary": "Use [[uv]] for dependency installation",
            "hard_guard": true,
            "forbidden_commands": ["pip install", "pip3 install"],
            "allowed_commands": ["uv pip install"],
            "required_flags": [],
        })
    );

    assert_eq!(facts[1].kind, FactKind::Decision);
    assert_eq!(
        facts[1].value,
        serde_json::json!({
            "summary": "Require explicit schema path",
            "hard_guard": true,
            "forbidden_commands": ["prisma migrate deploy"],
            "allowed_commands": [],
            "required_flags": ["--schema"],
        })
    );

    assert_eq!(
        facts[2].value,
        serde_json::json!("Always run `cargo clippy` before opening a PR")
    );
    Ok(())
}
