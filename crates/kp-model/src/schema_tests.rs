use std::collections::BTreeSet;

use super::*;

#[test]
fn core_lexicon_is_unique_namespaced_and_stable(
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let slots = core_lexicon();
    assert_eq!(slots.len(), 8);
    assert_eq!(
        slots
            .iter()
            .map(|s| &s.predicate)
            .collect::<BTreeSet<_>>()
            .len(),
        slots.len()
    );
    assert!(slots
        .iter()
        .all(|s| s.predicate.starts_with(&format!("{}.", s.namespace))));
    assert!(slots
        .iter()
        .all(|s| !s.description.is_empty() && !s.instruction.is_empty()));
    assert_eq!(
        serde_json::from_str::<Vec<SlotContract>>(&serde_json::to_string(&slots)?)?,
        slots
    );
    Ok(())
}
