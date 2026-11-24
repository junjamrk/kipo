use std::sync::Mutex;

use kp_model::{DerivationMode, FactAccess, FactKind, Justification, Premise};
use kp_store::FactInput;
use proptest::prelude::*;

use super::*;
use crate::judge::OffJudge;
use crate::test_support::{add, fact, library, TestResult, AT};

fn options(query: &str) -> TestResult<SearchOptions> {
    Ok(SearchOptions {
        query: query.into(),
        viewer: "carol".parse()?,
        at: AT,
        limit: 100,
        deep: true,
        max_hops: 2,
    })
}

struct CaptureJudge(Mutex<Vec<String>>);
impl EvidenceJudge for CaptureJudge {
    async fn score(&self, batch: EvidenceBatch<'_>) -> Result<Option<Vec<f64>>> {
        let mut seen = self
            .0
            .lock()
            .map_err(|_| RetrieveError::Protocol("test lock poisoned"))?;
        *seen = batch
            .candidates()
            .iter()
            .map(|d| d.passage.clone())
            .collect();
        Ok(None)
    }
}

#[tokio::test]
async fn private_chunks_facts_entity_words_and_lifecycle_never_change_output() -> TestResult {
    let (dir, mut store) = library()?;
    add(
        &mut store,
        "schedule",
        "周四傍晚机房有电力检修，断电预警。Thursday outage warning.",
        "carol",
    )?;
    add(
        &mut store,
        "backup",
        "电力检修断电期间绝不跑批处理任务，改用备用集群。Outage means use standby.",
        "carol",
    )?;
    let options = options("周四还能在机房跑批处理归档吗 Thursday archive")?;
    let judge = CaptureJudge(Mutex::new(Vec::new()));
    let before = serde_json::to_vec(&search(&store, &options, &judge).await?)?;
    for i in 0..30 {
        add(
            &mut store,
            &format!("hidden-{i}"),
            "私密token 周四机房断电归档 Thursday standby",
            "bob",
        )?;
        let private = fact(
            &format!("private-{i}"),
            "privatesecret 周四机房断电归档 Thursday standby",
            "bob",
        )?;
        store.remember(&[FactInput {
            fact: private,
            entities: BTreeSet::from(["周四机房断电归档".into()]),
        }])?;
    }
    assert_eq!(
        before,
        serde_json::to_vec(&search(&store, &options, &judge).await?)?
    );
    {
        let passages = judge.0.lock().map_err(|_| "lock")?;
        assert!(passages
            .iter()
            .all(|p| !p.contains("privatesecret") && !p.contains("私密token")));
    }
    store.forget("private-0", &"bob".parse()?, AT)?;
    assert_eq!(
        before,
        serde_json::to_vec(&search(&store, &options, &judge).await?)?
    );
    store.purge(&"test".parse()?, &"hidden-0".parse()?, AT, &[])?;
    assert_eq!(
        before,
        serde_json::to_vec(&search(&store, &options, &judge).await?)?
    );
    drop(store);
    assert_eq!(
        before,
        serde_json::to_vec(&search(&Store::open(dir.path())?, &options, &judge).await?)?
    );
    Ok(())
}

struct LexicalOnly;
impl Embedder for LexicalOnly {
    fn embed(&self, _text: &str) -> kp_store::Result<Vec<f32>> {
        Ok(vec![0.0; 256])
    }
}

struct CalibratedJudge;

impl EvidenceJudge for CalibratedJudge {
    async fn score(&self, batch: EvidenceBatch<'_>) -> Result<Option<Vec<f64>>> {
        batch
            .candidates()
            .iter()
            .map(|document| match document.heading.as_str() {
                "false-positive" => Ok(0.02),
                "bridge" => Ok(0.96),
                "destination" => Ok(0.95),
                _ => Err(RetrieveError::Protocol("unexpected test candidate")),
            })
            .collect::<Result<Vec<_>>>()
            .map(Some)
    }
}

#[tokio::test]
async fn deep_search_ranks_calibrated_bridge_evidence_above_hop_zero_false_positive() -> TestResult
{
    let (_dir, mut store) = library()?;
    for (id, passage) in [
        ("false-positive", "Friday camping has monsoon warnings."),
        ("bridge", "monsoon requires railway travel."),
        ("destination", "railway booking needs identification."),
    ] {
        add(&mut store, id, passage, "carol")?;
    }
    let output = search_with_embedder(
        &store,
        &options("Friday camping")?,
        &CalibratedJudge,
        &LexicalOnly,
    )
    .await?;
    let ranked: Vec<_> = output
        .results
        .iter()
        .map(|result| (result.heading.as_str(), result.score, result.hop))
        .collect();
    assert_eq!(
        ranked,
        vec![
            ("bridge", 0.96, 1),
            ("destination", 0.95, 2),
            ("false-positive", 0.02, 0)
        ]
    );
    Ok(())
}

#[test]
fn candidate_budget_includes_protected_hits_below_the_top_hundred() {
    let mut scores = vec![0.04; 110];
    scores[..5].fill(0.001);
    let protected = [0, 1, 2, 3, 4];
    assert!(rank(&scores)[..100].iter().all(|i| !protected.contains(i)));
    let pool = candidate_pool(&scores, &protected);
    assert_eq!(pool.len(), 100);
    assert_eq!(pool.iter().collect::<BTreeSet<_>>().len(), 100);
    assert!(protected.iter().all(|i| pool.contains(i)));
    assert!(pool
        .windows(2)
        .all(|pair| scores[pair[0]] >= scores[pair[1]]));
    assert_eq!(candidate_pool(&scores, &[]), rank(&scores)[..100]);
}

#[tokio::test]
async fn off_judge_normalizes_and_diversifies_deep_results() -> TestResult {
    let (_dir, mut store) = library()?;
    for (id, passage) in [
        ("a", "travel rain"),
        ("b", "travel rain"),
        ("c", "travel railway"),
    ] {
        add(&mut store, id, passage, "carol")?;
    }
    let mut options = options("rain travel")?;
    options.max_hops = 0;
    options.deep = false;
    let shallow = search_with_embedder(&store, &options, &OffJudge, &LexicalOnly).await?;
    assert_eq!(shallow.results.len(), 3);
    assert!(shallow.results[..2]
        .iter()
        .all(|r| r.passage == "travel rain"));
    options.deep = true;
    let deep = search_with_embedder(&store, &options, &OffJudge, &LexicalOnly).await?;
    assert_eq!(deep.results.len(), 3);
    assert_eq!(deep.results[0].id, shallow.results[0].id);
    assert_eq!(deep.results[1].passage, "travel railway");
    for (rank, result) in shallow.results.iter().enumerate() {
        let expected = 1.0 - (-30.0 / (61.0 + rank as f64)).exp();
        assert!((result.score - expected).abs() < 1e-12);
        assert!(deep
            .results
            .iter()
            .any(|r| r.id == result.id && r.score == result.score));
    }
    Ok(())
}

struct FixedQueryVector(Vec<f32>);

impl Embedder for FixedQueryVector {
    fn embed(&self, _text: &str) -> kp_store::Result<Vec<f32>> {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn invalid_custom_query_embeddings_return_typed_errors() -> TestResult {
    let (_dir, store) = library()?;
    let options = options("query")?;
    for vector in [
        vec![],
        vec![f32::NAN],
        vec![f32::INFINITY],
        vec![f32::NEG_INFINITY],
        vec![2.0],
        vec![0.5],
    ] {
        let result =
            search_with_embedder(&store, &options, &OffJudge, &FixedQueryVector(vector)).await;
        assert!(matches!(
            result,
            Err(RetrieveError::Store(kp_store::StoreError::Model(
                kp_model::KpError::InvalidInput {
                    field: "embedding",
                    ..
                }
            )))
        ));
    }
    Ok(())
}

#[tokio::test]
async fn two_hop_clues_bridge_english_and_chinese_without_query_words() -> TestResult {
    for (query, a, b, c) in [
        (
            "Friday camping",
            "Friday camping has monsoon warnings.",
            "monsoon requires railway travel.",
            "railway booking needs identification.",
        ),
        (
            "周四归档",
            "周四归档机房有检修，断电预警。",
            "断电检修期间切换备用集群，改走专线。",
            "专线接入需要硬件令牌。",
        ),
    ] {
        let (_dir, mut store) = library()?;
        for (id, text) in [("a", a), ("b", b), ("c", c)] {
            add(&mut store, id, text, "carol")?;
        }
        let result =
            search_with_embedder(&store, &options(query)?, &OffJudge, &LexicalOnly).await?;
        assert!(
            result.results.iter().any(|r| r.passage == b && r.hop == 1),
            "{query}: {:?}",
            result.results
        );
        assert!(
            result.results.iter().any(|r| r.passage == c && r.hop == 2),
            "{query}: {:?}",
            result.results
        );
        assert_eq!(result.trace.len(), 3);
        assert!(result
            .results
            .iter()
            .all(|r| (0.0..=1.0).contains(&r.score)));
    }
    Ok(())
}

#[tokio::test]
async fn semiring_alias_and_entity_hops_respect_joint_access() -> TestResult {
    let (_dir, mut store) = library()?;
    let mut a = fact("a", "launch checklist", "carol")?;
    a.subject = "alpha".into();
    let mut b = fact("b", "handoff guide", "carol")?;
    b.subject = "alpha".into();
    store.remember(&[
        FactInput {
            fact: a.clone(),
            entities: BTreeSet::new(),
        },
        FactInput {
            fact: b,
            entities: BTreeSet::new(),
        },
    ])?;
    let mut alias = fact("alias", "beta", "carol")?;
    alias.kind = FactKind::Alias;
    alias.subject = "alpha".into();
    alias.access = FactAccess::Derived {
        recorded_at: AT,
        expired_at: None,
    };
    store.insert_fact(&alias)?;
    store.insert_justification(&Justification {
        id: "alias-proof".parse()?,
        conclusion: alias.id.clone(),
        mode: DerivationMode::HardRule,
        rule_id: "alias".into(),
        rule_version: "1".into(),
        premises: vec![Premise {
            fact_id: a.id.clone(),
            strong_ref: a.strong_ref(),
        }],
        active: true,
        created_at: AT,
        invalidated_at: None,
        invalidation_reason: None,
    })?;
    let mut target = fact("alias-target", "transport rules", "carol")?;
    target.subject = "beta".into();
    store.remember(&[FactInput {
        fact: target,
        entities: BTreeSet::new(),
    }])?;
    let result = search_with_embedder(&store, &options("launch")?, &OffJudge, &LexicalOnly).await?;
    assert!(result.results.iter().any(|r| r.id.ends_with("fact-b")));
    assert!(result
        .results
        .iter()
        .any(|r| r.id.ends_with("fact-alias-target") && r.hop == 1));
    assert!(result.results.iter().any(|r| r.id.ends_with("fact-alias")));
    let mut outsider = options("launch")?;
    outsider.viewer = "bob".parse()?;
    assert!(search(&store, &outsider, &OffJudge)
        .await?
        .results
        .is_empty());
    store.forget("a", &"carol".parse()?, AT)?;
    assert!(!search(&store, &options("beta")?, &OffJudge)
        .await?
        .results
        .iter()
        .any(|r| r.id.ends_with("fact-alias")));
    Ok(())
}

#[tokio::test]
async fn personal_shallow_search_uses_candidate_bounded_snapshot_without_deep_expansion(
) -> TestResult {
    let (_dir, mut store) = library()?;
    let mut batch = Vec::with_capacity(1_000);
    for idx in 0..1_000 {
        let mut bg = fact(
            &format!("bg_{idx}"),
            &format!("background entry {idx}"),
            "carol",
        )?;
        bg.predicate = "kp.core.observation".into();
        batch.push(FactInput {
            fact: bg,
            entities: BTreeSet::new(),
        });
    }
    let mut hit = fact(
        "target_hit",
        "差旅住宿报销标准统一走财务系统提交电子发票",
        "carol",
    )?;
    hit.predicate = "kp.core.observation".into();
    batch.push(FactInput {
        fact: hit,
        entities: BTreeSet::from(["差旅财务系统".into()]),
    });
    store.remember(&batch)?;

    let shallow_opts = SearchOptions {
        query: "差旅住宿怎么报销".into(),
        viewer: "carol".parse()?,
        at: AT,
        limit: 5,
        deep: false,
        max_hops: 0,
    };
    let judge = CaptureJudge(Mutex::new(Vec::new()));
    let _warmup = search(&store, &shallow_opts, &judge).await?;

    let mut samples = Vec::with_capacity(3);
    let mut out = None;
    for _ in 0..3 {
        let started = std::time::Instant::now();
        out = Some(search(&store, &shallow_opts, &judge).await?);
        samples.push(started.elapsed());
    }
    samples.sort_unstable();
    let elapsed = samples[1];
    let out = out.ok_or("missing search output")?;
    assert!(!out.results.is_empty());
    assert_eq!(out.results[0].id, "test:fact-target_hit");
    assert_eq!(out.trace.len(), 1);
    // Candidate-bounded path passes at most the bounded candidate pool to the judge, not 1,001 docs,
    // and avoids `ScopedIndex::new` / `Jieba::clone()` overhead.
    let seen_len = judge.0.lock().map_err(|_| "lock")?.len();
    assert!(
        seen_len <= 64,
        "expected bounded candidate pool, got {seen_len}"
    );
    assert!(
        elapsed.as_millis() < 80,
        "expected <80ms candidate search even in debug profile, took {elapsed:?}"
    );
    Ok(())
}

#[tokio::test]
async fn search_shallow_matches_calibrated_rrf_and_preserves_viewer_noninterference() -> TestResult
{
    let (_dir, mut store) = library()?;
    add(
        &mut store,
        "ops-guide",
        "归档任务统一切换到备用存储池执行 standby pool",
        "carol",
    )?;
    let carol_fact = fact(
        "carol-rule",
        "备用存储池归档需要提前校验校验和 standby checksum",
        "carol",
    )?;
    store.remember(&[FactInput {
        fact: carol_fact,
        entities: BTreeSet::from(["备用存储池".into()]),
    }])?;

    let carol: ActorId = "carol".parse()?;
    let bob: ActorId = "bob".parse()?;
    assert!(search_shallow(&store, &carol, AT, "", 5)?
        .results
        .is_empty());
    assert!(search_shallow(&store, &carol, AT, "备用存储池", 0)?
        .results
        .is_empty());

    let query = "备用存储池 standby";
    let sync_out = search_shallow(&store, &carol, AT, query, 5)?;
    let async_out = search(
        &store,
        &SearchOptions {
            query: query.into(),
            viewer: carol.clone(),
            at: AT,
            limit: 5,
            deep: false,
            max_hops: 0,
        },
        &OffJudge,
    )
    .await?;
    assert_eq!(sync_out.results, async_out.results);
    assert_eq!(sync_out.trace.len(), 1);
    assert!(sync_out
        .results
        .iter()
        .all(|r| r.hop == 0 && (0.0..=1.0).contains(&r.score)));

    let lex_only = search_shallow_with_embedder(&store, &carol, AT, query, 5, &LexicalOnly)?;
    assert_eq!(lex_only.results.len(), 2);

    let before_bytes = serde_json::to_vec(&sync_out)?;
    for idx in 0..20 {
        add(
            &mut store,
            &format!("bob-priv-chunk-{idx}"),
            "秘密口令 备用存储池 standby checksum",
            "bob",
        )?;
        let priv_fact = fact(
            &format!("bob-priv-fact-{idx}"),
            "秘密配置 备用存储池 standby checksum",
            "bob",
        )?;
        store.remember(&[FactInput {
            fact: priv_fact,
            entities: BTreeSet::new(),
        }])?;
    }
    let after_bytes = serde_json::to_vec(&search_shallow(&store, &carol, AT, query, 5)?)?;
    assert_eq!(before_bytes, after_bytes);

    let outsider: ActorId = "dave".parse()?;
    assert!(search_shallow(&store, &outsider, AT, query, 5)?
        .results
        .is_empty());
    assert!(search_shallow(&store, &bob, AT, "秘密口令", 5)?
        .results
        .iter()
        .all(|r| r.passage.contains("秘密")));
    Ok(())
}

#[tokio::test]
async fn retrieval_paths_embed_query_exactly_once() -> TestResult {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingEmbedder {
        calls: AtomicUsize,
    }

    impl Embedder for CountingEmbedder {
        fn embed(&self, _text: &str) -> kp_store::Result<Vec<f32>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![1.0, 0.0, 0.0, 0.0])
        }
        fn model_id(&self) -> &str {
            "test:counting-v1"
        }
        fn is_enabled(&self) -> bool {
            true
        }
    }

    let (_dir, mut store) = library()?;
    let note = fact(
        "single-embed",
        "deploy checklist verify backup first",
        "carol",
    )?;
    store.remember(&[FactInput {
        fact: note,
        entities: BTreeSet::new(),
    }])?;

    let embedder = CountingEmbedder {
        calls: AtomicUsize::new(0),
    };
    let viewer: ActorId = "carol".parse()?;

    let _ = search_shallow_with_embedder(&store, &viewer, AT, "deploy checklist", 5, &embedder)?;
    assert_eq!(
        embedder.calls.swap(0, Ordering::SeqCst),
        1,
        "search_shallow_with_embedder must embed query exactly once"
    );

    let opts = SearchOptions {
        query: "deploy checklist".into(),
        viewer: viewer.clone(),
        at: AT,
        limit: 5,
        deep: false,
        max_hops: 0,
    };
    let _ = search_with_embedder(&store, &opts, &OffJudge, &embedder).await?;
    assert_eq!(
        embedder.calls.swap(0, Ordering::SeqCst),
        1,
        "search_with_embedder must embed query exactly once"
    );

    let _ = crate::retrieve_prompt_context_with_embedder(
        &store,
        &viewer,
        AT,
        "deploy checklist",
        &crate::PromptBudgetConfig::default(),
        &embedder,
    )?;
    assert_eq!(
        embedder.calls.swap(0, Ordering::SeqCst),
        1,
        "retrieve_prompt_context_with_embedder must embed query exactly once"
    );
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, rng_seed: proptest::test_runner::RngSeed::Fixed(202), ..ProptestConfig::default() })]
    #[test]
    fn federated_order_is_independent_of_slice_order(probabilities in prop::collection::vec(0.0_f64..=1.0,0..40)) {
        let library: LibraryId = "test".parse()?;
        let items: Vec<_> = probabilities.into_iter().enumerate().map(|(i,score)| SearchResult { id: format!("candidate-{i}"),library_id: library.clone(),kind: "fact".into(),heading: String::new(),passage: String::new(),anchor: None,score,hop: 0 }).collect();
        let mut reverse = items.clone(); reverse.reverse();
        let forward = merge_federated_results([items]);
        prop_assert_eq!(&forward,&merge_federated_results([reverse]));
        prop_assert!(forward.windows(2).all(|w| w[0].score >= w[1].score));
    }
}
