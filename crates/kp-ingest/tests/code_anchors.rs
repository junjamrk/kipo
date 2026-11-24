mod support;

use std::path::Path;

use kp_derive::Memory;
use kp_ingest::code_intel::{locate, resolve_path, symbols, verify};
use kp_model::{EpochMs, FactId, FactKind, FactStatus, MemberRole, ValidityWindow, VerifyStatus};
use serde_json::json;
use support::{library, options, Result};

#[test]
fn real_grammars_drive_the_complete_anchor_and_tms_lifecycle() -> Result {
    for (file, original, body_edit, signature_edit) in [
        (
            "pay.rs",
            "fn charge_card(amount: i32) -> i32 { amount + 1 }",
            "fn charge_card(amount: i32) -> i32 { amount + 2 }",
            "fn charge_card(amount: i64) -> i32 { amount + 1 }",
        ),
        (
            "pay.py",
            "def charge_card(amount):\n    return amount + 1",
            "def charge_card(amount):\n    return amount + 2",
            "def charge_card(amount, currency):\n    return amount + 1",
        ),
        (
            "pay.ts",
            "function charge_card(amount: number) { return amount + 1; }",
            "function charge_card(amount: number) { return amount + 2; }",
            "function charge_card(amount: string) { return amount + 1; }",
        ),
        (
            "pay.go",
            "package pay\nfunc charge_card(amount int) int { return amount + 1 }",
            "package pay\nfunc charge_card(amount int) int { return amount + 2 }",
            "package pay\nfunc charge_card(amount int64) int { return amount + 1 }",
        ),
        (
            "pay.js",
            "function charge_card(amount) { return amount + 1; }",
            "function charge_card(amount) { return amount + 2; }",
            "function charge_card(amount, currency) { return amount + 1; }",
        ),
    ] {
        let (dir, mut store) = library()?;
        let repo = tempfile::tempdir()?;
        let path = repo.path().join(file);
        std::fs::write(&path, original)?;
        let locus = locate(repo.path(), Path::new(file), "charge_card")?;
        for kind in [FactKind::FactSlot, FactKind::ClosedNegative] {
            let memory = Memory {
                subject: "charge_card".into(),
                predicate: if kind == FactKind::ClosedNegative {
                    "kp.code.pitfall"
                } else {
                    "kp.code.api_contract"
                }
                .into(),
                value: json!("preserve payment contract"),
                kind,
                logical_key: None,
                tags: Default::default(),
                entities: Default::default(),
                valid: ValidityWindow {
                    valid_from: EpochMs(0),
                    valid_until: EpochMs::OPEN_END,
                },
            };
            let input = kp_derive::base_fact(&store, memory, &options()?)?;
            store.remember_anchored(input, &locus)?;
        }
        drop(store);
        let mut store = kp_store::Store::open(dir.path())?;
        let actor = "alice".parse()?;
        for (source, expected, warning_count) in [
            (original.to_owned(), VerifyStatus::Verified, 0),
            (format!("\n\n{original}"), VerifyStatus::MovedUnchanged, 0),
            (format!("\n\n{body_edit}"), VerifyStatus::BodyModified, 0),
            (format!("\n\n{signature_edit}"), VerifyStatus::SigChanged, 2),
            (format!("\n\n{original}"), VerifyStatus::Verified, 0),
            (String::new(), VerifyStatus::Orphaned, 2),
        ] {
            std::fs::write(&path, source)?;
            let results = verify(&mut store, Some(repo.path()), &actor, EpochMs(20))?;
            assert_eq!(results.len(), 2);
            assert!(
                results.iter().all(|a| a.verify_status == expected),
                "{file}: {results:?}"
            );
            if expected == VerifyStatus::MovedUnchanged {
                assert_eq!(results[0].locus.start_line, locus.start_line + 2);
            }
            let warnings: Vec<_> = store
                .visible_facts(&actor, EpochMs(20))?
                .into_iter()
                .filter(|f| f.predicate == "kp.code.anchor_stale")
                .collect();
            assert_eq!(warnings.len(), warning_count, "{file}: {expected}");
            for warning in warnings {
                let proof = kp_derive::why::why(&store, &warning.id, &actor, EpochMs(20))?
                    .ok_or("missing proof")?;
                assert_eq!(proof.justifications[0].rule_id, "R4_ANCHOR_STALENESS");
            }
            kp_derive::verify_hard_rules_match_full_recompute(&store)?;
            if expected == VerifyStatus::Verified {
                assert!(!store.derivation_view()?.facts.values().any(|f| f.predicate
                    == "kp.code.anchor_stale"
                    && f.status == FactStatus::Active));
            }
        }
        store.add_member("carol".parse()?, MemberRole::Member, EpochMs(0))?;
        assert!(verify(&mut store, None, &"carol".parse()?, EpochMs(20))?.is_empty());
        std::fs::remove_file(&path)?;
        assert!(verify(&mut store, None, &actor, EpochMs(20))?
            .iter()
            .all(|a| a.verify_status == VerifyStatus::Orphaned));
    }
    Ok(())
}

#[test]
fn traversal_and_external_symlinks_are_rejected_before_reading() -> Result {
    let repo = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    let external = outside.path().join("secret.rs");
    std::fs::write(&external, "fn secret() {}")?;
    assert!(resolve_path(repo.path(), Path::new("../secret.rs")).is_err());
    assert!(resolve_path(repo.path(), &external).is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path(), repo.path().join("escape"))?;
        assert!(resolve_path(repo.path(), Path::new("escape/secret.rs")).is_err());
        assert!(resolve_path(repo.path(), Path::new("escape/missing.rs")).is_err());
    }
    Ok(())
}

#[test]
fn class_trait_interface_methods_and_arrow_functions_have_real_symbols() -> Result {
    for (file, source, names) in [
        ("a.rs", "struct Card { value: i32 } trait Pay { fn charge(&self); } impl Card { fn charge(&self) {} }", vec!["Card", "Pay", "Pay::charge", "Card::charge"]),
        ("a.py", "class Card:\n    def charge(self):\n        return 1\n", vec!["Card", "Card::charge"]),
        ("a.ts", "interface Pay { charge(x: number): void; } class Card { charge(x: number) {} } const f = (x: number) => x + 1;", vec!["Pay", "Pay::charge", "Card", "Card::charge", "f"]),
        ("a.tsx", "function Card() { return <div>pay</div>; }", vec!["Card"]),
        ("a.go", "package pay\ntype Card struct { Value int }\ntype Pay interface { Charge(int) int }\nfunc (c Card) Charge(x int) int { return x }", vec!["Card", "Pay", "Pay::Charge", "Card::Charge"]),
    ] {
        let actual: Vec<_> = symbols(Path::new(file), source)?.into_iter().map(|s| s.symbol_name).collect();
        for name in names { assert!(actual.iter().any(|s| s == name), "{file}: {actual:?}, missing {name}"); }
    }
    Ok(())
}

proptest::proptest! {
    #[test]
    fn whitespace_and_comments_leave_hashes_stable(comment in "[a-zA-Z0-9 ]{0,40}", spaces in " {1,8}") {
        let baseline = symbols(Path::new("a.rs"), "fn pay(x: i32) -> i32 { x + 1 }").map_err(|e| proptest::test_runner::TestCaseError::fail(e.to_string()))?;
        let changed = symbols(Path::new("a.rs"), &format!("fn{spaces}pay(x:i32)->i32 {{ /* {comment} */ x+1 }}")).map_err(|e| proptest::test_runner::TestCaseError::fail(e.to_string()))?;
        proptest::prop_assert_eq!(&baseline[0].sig_hash, &changed[0].sig_hash);
        proptest::prop_assert_eq!(&baseline[0].body_hash, &changed[0].body_hash);
    }
}

fn remember_symbol(
    store: &mut kp_store::Store,
    repo: &Path,
    file: &str,
    name: &str,
) -> Result<FactId> {
    let locus = locate(repo, Path::new(file), name)?;
    let input = kp_derive::base_fact(
        store,
        Memory {
            subject: format!("{}:{file}:{name}", repo.display()),
            predicate: "kp.code.api_contract".into(),
            value: json!("preserve this contract"),
            kind: FactKind::FactSlot,
            logical_key: None,
            tags: Default::default(),
            entities: Default::default(),
            valid: ValidityWindow {
                valid_from: EpochMs(0),
                valid_until: EpochMs::OPEN_END,
            },
        },
        &options()?,
    )?;
    let id = input.fact.id.clone();
    store.remember_anchored(input, &locus)?;
    Ok(id)
}

#[test]
fn canonical_top_level_symbol_survives_an_added_method_with_the_same_suffix() -> Result {
    let (_dir, mut store) = library()?;
    let repo = tempfile::tempdir()?;
    let path = repo.path().join("billing.rs");
    std::fs::write(&path, "fn charge() {}")?;
    remember_symbol(&mut store, repo.path(), "billing.rs", "charge")?;
    std::fs::write(
        &path,
        "fn charge() {}\nstruct Billing; impl Billing { fn charge(&self) {} }",
    )?;
    assert_eq!(
        locate(repo.path(), Path::new("billing.rs"), "charge")?.symbol_name,
        "charge"
    );
    remember_symbol(&mut store, repo.path(), "billing.rs", "Billing::charge")?;
    let anchors = verify(&mut store, None, &"alice".parse()?, EpochMs(20))?;
    assert_eq!(anchors.len(), 2);
    assert!(anchors
        .iter()
        .all(|a| a.verify_status == VerifyStatus::Verified));
    Ok(())
}

#[test]
fn trait_impl_methods_have_distinct_canonical_names_and_verify_independently() -> Result {
    let (_dir, mut store) = library()?;
    let repo = tempfile::tempdir()?;
    let path = repo.path().join("card.rs");
    let source = "use std::fmt::{Display, Debug};\nstruct Card;\nimpl Card { fn fmt(&self) {} }\nimpl Display for Card { fn fmt(&self) {} }\nimpl Debug for Card { fn fmt(&self) {} }";
    std::fs::write(&path, source)?;
    let names = [
        "Card::fmt",
        "<Card as Display>::fmt",
        "<Card as Debug>::fmt",
    ];
    for name in names {
        assert_eq!(
            locate(repo.path(), Path::new("card.rs"), name)?.symbol_name,
            name
        );
        remember_symbol(&mut store, repo.path(), "card.rs", name)?;
    }
    assert!(locate(repo.path(), Path::new("card.rs"), "fmt").is_err());
    let anchors = verify(&mut store, None, &"alice".parse()?, EpochMs(20))?;
    assert_eq!(anchors.len(), 3);
    assert!(anchors
        .iter()
        .all(|a| a.verify_status == VerifyStatus::Verified));
    std::fs::write(
        &path,
        source.replace(
            "impl Debug for Card { fn fmt(&self) {} }",
            "impl Debug for Card { fn fmt(&self) { todo!() } }",
        ),
    )?;
    for anchor in verify(&mut store, None, &"alice".parse()?, EpochMs(30))? {
        assert_eq!(
            anchor.verify_status,
            if anchor.locus.symbol_name == "<Card as Debug>::fmt" {
                VerifyStatus::BodyModified
            } else {
                VerifyStatus::Verified
            }
        );
    }
    Ok(())
}

#[test]
fn unqualified_symbol_lookup_still_requires_a_unique_suffix_or_exact_match() -> Result {
    let repo = tempfile::tempdir()?;
    let path = repo.path().join("lookup.rs");
    std::fs::write(&path, "impl Billing { fn charge(&self) {} }")?;
    assert_eq!(
        locate(repo.path(), Path::new("lookup.rs"), "charge")?.symbol_name,
        "Billing::charge"
    );
    std::fs::write(
        &path,
        "impl Billing { fn charge(&self) {} } impl Card { fn charge(&self) {} }",
    )?;
    assert!(locate(repo.path(), Path::new("lookup.rs"), "charge").is_err());
    std::fs::write(&path, "fn charge() {} fn charge() {}")?;
    assert!(locate(repo.path(), Path::new("lookup.rs"), "charge").is_err());
    Ok(())
}

#[test]
fn deleted_repository_is_orphaned_without_aborting_surviving_repositories() -> Result {
    let (dir, mut store) = library()?;
    let removed = tempfile::tempdir()?;
    let surviving = tempfile::tempdir()?;
    for repo in [&removed, &surviving] {
        std::fs::write(repo.path().join("pay.rs"), "fn charge() {}")?;
        remember_symbol(&mut store, repo.path(), "pay.rs", "charge")?;
    }
    let removed_path = removed.path().canonicalize()?;
    removed.close()?;
    let anchors = verify(&mut store, None, &"alice".parse()?, EpochMs(30))?;
    assert_eq!(anchors.len(), 2);
    for anchor in &anchors {
        assert_eq!(
            anchor.verify_status,
            if Path::new(&anchor.locus.repo_root) == removed_path {
                VerifyStatus::Orphaned
            } else {
                VerifyStatus::Verified
            }
        );
        assert_eq!(anchor.verified_at, EpochMs(30));
    }
    drop(store);
    let store = kp_store::Store::open(dir.path())?;
    let persisted = store.code_anchors(&"alice".parse()?, EpochMs(30))?;
    assert_eq!(
        serde_json::to_value(persisted)?,
        serde_json::to_value(anchors)?
    );
    assert!(resolve_path(&removed_path, Path::new("../outside.rs")).is_err());
    assert!(resolve_path(&removed_path, &surviving.path().join("pay.rs")).is_err());
    Ok(())
}

#[cfg(unix)]
#[test]
fn verification_rejects_external_and_dangling_symlinks() -> Result {
    let (_dir, mut store) = library()?;
    let repo = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    let path = repo.path().join("pay.rs");
    let external = outside.path().join("pay.rs");
    std::fs::write(&path, "fn charge() {}")?;
    std::fs::write(&external, "fn charge() {}")?;
    remember_symbol(&mut store, repo.path(), "pay.rs", "charge")?;
    std::fs::remove_file(&path)?;
    std::os::unix::fs::symlink(&external, &path)?;
    assert!(verify(&mut store, None, &"alice".parse()?, EpochMs(20)).is_err());
    std::fs::remove_file(&external)?;
    assert!(verify(&mut store, None, &"alice".parse()?, EpochMs(20)).is_err());
    Ok(())
}

#[test]
fn mixed_case_source_extensions_work_for_symbols_locate_and_verify() -> Result {
    let (_dir, mut store) = library()?;
    let repo = tempfile::tempdir()?;
    for (file, source, language) in [
        ("pay.RS", "fn charge() {}", "rust"),
        ("pay.PY", "def charge():\n    return 1", "python"),
        ("pay.Js", "function charge() {}", "javascript"),
        ("pay.JSX", "function charge() {}", "javascript"),
        ("pay.TS", "function charge() {}", "typescript"),
        ("pay.TsX", "function charge() { return <div/>; }", "tsx"),
        ("pay.GO", "package pay\nfunc charge() {}", "go"),
    ] {
        std::fs::write(repo.path().join(file), source)?;
        assert_eq!(symbols(Path::new(file), source)?[0].symbol_name, "charge");
        assert_eq!(
            locate(repo.path(), Path::new(file), "charge")?.language,
            language
        );
        remember_symbol(&mut store, repo.path(), file, "charge")?;
    }
    let anchors = verify(&mut store, None, &"alice".parse()?, EpochMs(20))?;
    assert_eq!(anchors.len(), 7);
    assert!(anchors
        .iter()
        .all(|a| a.verify_status == VerifyStatus::Verified));
    Ok(())
}
