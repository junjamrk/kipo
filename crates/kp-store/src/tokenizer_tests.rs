use proptest::prelude::*;
use proptest::test_runner::RngSeed;

use super::*;

#[test]
fn segments_cjk_and_preserves_code_identifiers() {
    let tokenizer = Tokenizer::default();
    let tokens = tokenizer
        .tokenize("傍晚机房检修时暂停报表 getSchedule HTTP_client XMLHttpRequest snake_case");
    for expected in [
        "傍晚",
        "检修",
        "报表",
        "getschedule",
        "get",
        "schedule",
        "http_client",
        "http",
        "client",
        "xmlhttprequest",
        "xml",
        "http",
        "request",
        "snake_case",
        "snake",
        "case",
    ] {
        assert!(
            tokens.iter().any(|token| token == expected),
            "missing {expected}: {tokens:?}"
        );
    }
    assert!(tokenizer.tokenize("___ 。，！？").is_empty());
}

#[test]
fn shared_and_default_tokenizers_borrow_static_base_without_reloading_dict() {
    let _warm = Tokenizer::shared().tokenize("社区知识库读写");
    assert!(!Tokenizer::shared().has_custom_words());

    let started = std::time::Instant::now();
    for _ in 0..200 {
        let clean = Tokenizer::default();
        assert!(!clean.has_custom_words());
        let out = clean.tokenize("数据库主从切换窗口期");
        assert!(!out.is_empty());
    }
    let elapsed = started.elapsed();
    // 200 fresh `Tokenizer::default()` instances would take ~20s if reloading `Jieba::new()`.
    assert!(
        elapsed.as_millis() < 250,
        "expected static BASE_JIEBA zero-copy reuse, took {elapsed:?}"
    );
}

#[test]
fn entity_dictionary_is_instance_local() {
    let mut tokenizer = Tokenizer::default();
    tokenizer.add_entity_word("猕猴桃花粉引擎");
    assert!(tokenizer
        .tokenize("猕猴桃花粉引擎今天上线")
        .contains(&"猕猴桃花粉引擎".into()));
    assert!(!Tokenizer::default()
        .tokenize("猕猴桃花粉引擎今天上线")
        .contains(&"猕猴桃花粉引擎".into()));
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, rng_seed: RngSeed::Fixed(201), ..ProptestConfig::default() })]
    #[test]
    fn tokenization_is_deterministic_and_has_no_empty_tokens(text in ".{0,128}") {
        let tokenizer = Tokenizer::default();
        let tokens = tokenizer.tokenize(&text);
        prop_assert_eq!(&tokens, &tokenizer.tokenize(&text));
        prop_assert!(tokens.iter().all(|token| !token.is_empty() && token == &token.to_lowercase()));
    }
}
