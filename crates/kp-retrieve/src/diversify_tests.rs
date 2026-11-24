use std::collections::BTreeSet;

use proptest::prelude::*;

use super::*;
use crate::test_support::TestResult;

fn documents() -> kp_model::Result<Vec<VisibleDocument>> {
    let library_id: kp_model::LibraryId = "test".parse()?;
    Ok((0..8)
        .map(|i| VisibleDocument {
            id: format!("doc-{i}"),
            library_id: library_id.clone(),
            kind: "chunk".into(),
            heading: String::new(),
            passage: if i == 6 {
                "railway identification booking".into()
            } else {
                "rain camping weather".into()
            },
            anchor: None,
            entities: BTreeSet::new(),
            tags: BTreeSet::new(),
            vector: Vec::new(),
        })
        .collect())
}

#[test]
fn protects_top_five_and_promotes_independent_evidence_over_duplicates() -> TestResult {
    let docs = documents()?;
    let tokenizer = Tokenizer::default();
    let vocabulary = Vocabulary::new(&docs, &tokenizer);
    let mut order: Vec<_> = (0..8).collect();
    diversify(
        &mut order,
        &[0.9, 0.85, 0.8, 0.75, 0.7, 0.65, 0.6, 0.5],
        &docs,
        &vocabulary,
        &tokenizer,
        5,
    );
    assert_eq!(&order[..5], &[0, 1, 2, 3, 4]);
    assert_eq!(order[5], 6);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 16, rng_seed: proptest::test_runner::RngSeed::Fixed(203), ..ProptestConfig::default() })]
    #[test]
    fn dpp_preserves_candidates_and_protected_prefix(scores in prop::collection::vec(0.0_f64..=1.0,8)) {
        let docs = documents()?;
        let tokenizer = Tokenizer::default();
        let vocabulary = Vocabulary::new(&docs,&tokenizer);
        let mut order: Vec<_> = (0..8).collect();
        diversify(&mut order,&scores,&docs,&vocabulary,&tokenizer,5);
        prop_assert_eq!(&order[..5],&[0,1,2,3,4]);
        order.sort();
        prop_assert_eq!(order,(0..8).collect::<Vec<_>>());
    }
}
