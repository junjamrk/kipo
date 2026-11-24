use super::*;

#[test]
fn english_initials_and_chinese_sentence_context_are_preserved() -> Result<()> {
    assert_eq!(
        sentences("J. R. R. Tolkien wrote. Rain stops trips。晴天出门！"),
        vec![
            "J. R. R. Tolkien wrote.",
            " Rain stops trips。",
            "晴天出门！",
            ""
        ]
    );
    assert_eq!(
        name_phrases("J. R. R. Tolkien visits New York", &Tokenizer::default())?,
        vec!["j r r tolkien", "new york"]
    );
    Ok(())
}

#[test]
fn tokenless_terms_have_zero_document_frequency() {
    let tokenizer = Tokenizer::default();
    let vocabulary = Vocabulary {
        terms: vec![
            BTreeSet::from(["alpha".into(), "beta".into()]),
            BTreeSet::from(["alpha".into()]),
        ],
    };
    for term in ["", " \t\n", "!?。", "___"] {
        assert_eq!(vocabulary.df(term, &tokenizer), 0, "{term:?}");
    }
    assert_eq!(vocabulary.df("alpha", &tokenizer), 2);
    assert_eq!(vocabulary.df("alpha beta", &tokenizer), 1);
    assert_eq!(vocabulary.df("missing", &tokenizer), 0);
}

#[test]
fn cached_name_pattern_handles_repeated_independent_sentences() -> Result<()> {
    let tokenizer = Tokenizer::default();
    for _ in 0..3 {
        for (sentence, expected) in [
            (
                "J. R. R. Tolkien visits New York",
                vec!["j r r tolkien", "new york"],
            ),
            ("no names here", vec![]),
            ("met Ada Lovelace in London", vec!["ada lovelace"]),
        ] {
            assert_eq!(name_phrases(sentence, &tokenizer)?, expected);
        }
    }
    Ok(())
}
