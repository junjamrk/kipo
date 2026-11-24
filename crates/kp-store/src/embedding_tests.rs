use proptest::prelude::*;

use super::*;

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, rng_seed: proptest::test_runner::RngSeed::Fixed(201), ..ProptestConfig::default() })]
    #[test]
    fn normalized_deterministic_unicode(text in ".{0,200}") {
        let vector = HashEmbedder.embed(&text)?;
        prop_assert_eq!(vector.len(), 256);
        prop_assert!(validate_vector(&vector).is_ok());
        prop_assert_eq!(&vector, &HashEmbedder.embed(&text)?);
    }
}

#[test]
fn invalid_vectors_fail_closed() {
    for vector in [vec![], vec![f32::NAN], vec![f32::INFINITY], vec![2.0]] {
        assert!(validate_vector(&vector).is_err());
    }
}

#[test]
fn sanitize_and_validate_embedding_screens_nan_inf_and_zero_norm_and_normalizes() -> Result<()> {
    for degenerate in [
        vec![],
        vec![0.0, 0.0, 0.0],
        vec![1e-9, -1e-9],
        vec![f32::NAN, 1.0],
        vec![f32::INFINITY, 0.0],
        vec![f32::NEG_INFINITY, 0.0],
    ] {
        assert!(sanitize_and_validate_embedding(&degenerate).is_err());
    }

    let normalized = sanitize_and_validate_embedding(&[3.0, 4.0])?;
    assert!((normalized[0] - 0.6).abs() < 1e-5);
    assert!((normalized[1] - 0.8).abs() < 1e-5);
    assert!(validate_vector(&normalized).is_ok());

    assert!(HashEmbedder.allows_raw_only());
    assert!(FtsOnlyEmbedder.allows_raw_only());
    assert!(!FtsOnlyEmbedder.is_enabled());
    assert!(DisabledModelEmbedder::new("local:bge-small-zh-v1.5", 512).allows_raw_only());
    assert!(!DisabledModelEmbedder::new("openai:text-embedding-3-small", 1536).allows_raw_only());
    Ok(())
}
