use proptest::prelude::*;
use proptest::test_runner::RngSeed;
use serde_json::json;

use super::*;

#[test]
fn hash_matches_sha256_vector() {
    assert_eq!(
        ContentHash::digest("abc").as_str(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn malformed_ids_and_hashes_are_rejected_at_deserialization() {
    assert!(serde_json::from_str::<LibraryId>("\"origin:ambiguous\"").is_err());
    assert!(serde_json::from_str::<ActorId>("\"*\"").is_err());
    assert!(serde_json::from_str::<ContentHash>("\"not-a-hash\"").is_err());
    assert!(LogicalKey::new(" \n").is_err());
}

#[test]
fn canonical_hash_sorts_nested_keys_and_frames_fields(
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let first: Value = serde_json::from_str(r#"{"z":[{"b":2,"a":1}],"a":true}"#)?;
    let second: Value = serde_json::from_str(r#"{"a":true,"z":[{"a":1,"b":2}]}"#)?;
    assert_eq!(
        compute_fact_content_hash("a", "bc", &first, FactKind::Observation),
        compute_fact_content_hash("a", "bc", &second, FactKind::Observation)
    );
    assert_ne!(
        compute_fact_content_hash("a", "bc", &first, FactKind::Observation),
        compute_fact_content_hash("ab", "c", &first, FactKind::Observation)
    );
    assert_ne!(
        compute_fact_content_hash("a", "bc", &json!([1, 2]), FactKind::Observation),
        compute_fact_content_hash("a", "bc", &json!([2, 1]), FactKind::Observation)
    );
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, rng_seed: RngSeed::Fixed(101), ..ProptestConfig::default() })]
    #[test]
    fn hashes_round_trip(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
        let hash = ContentHash::digest(bytes);
        prop_assert_eq!(hash.as_str().len(), 64);
        prop_assert_eq!(hash.as_str().to_uppercase().parse::<ContentHash>()?, hash.clone());
        prop_assert_eq!(serde_json::from_str::<ContentHash>(&serde_json::to_string(&hash)?)?, hash);
    }

    #[test]
    fn identifiers_round_trip(value in "[a-zA-Z0-9_.-]{1,80}") {
        let id = LibraryId::new(value)?;
        prop_assert_eq!(serde_json::from_str::<LibraryId>(&serde_json::to_string(&id)?)?, id);
    }
}
