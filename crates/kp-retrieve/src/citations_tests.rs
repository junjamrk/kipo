use super::*;

#[test]
fn preserves_only_returned_ids_and_in_range_numeric_references() {
    let allowed = vec!["local:fact-1".into(), "other:fact-2".into()];
    assert_eq!(sanitize_citations("OK [kp-ref:local:fact-1] [1] [2] BAD [kp-ref:private] [0] [3] [999999999999999999999999] [text]",&allowed),
        "OK [kp-ref:local:fact-1] [1] [2] BAD     [text]");
    assert_eq!(
        sanitize_citations("plain [unfinished", &[]),
        "plain [unfinished"
    );
}

#[test]
fn nested_private_candidate_references_are_removed() {
    assert_eq!(sanitize_citations("See [[kp-ref:private]]", &[]), "See []");
    assert_eq!(
        sanitize_citations("[注释 [[kp-ref:private]]]", &[]),
        "[注释 []]"
    );
}

#[test]
fn nested_out_of_range_numeric_references_are_removed() {
    assert_eq!(
        sanitize_citations("[note [99]]", &["allowed_id".into()]),
        "[note ]"
    );
    assert_eq!(sanitize_citations("[[[0]]]", &[]), "[[]]");
}

#[test]
fn nested_allowed_candidate_and_numeric_references_are_preserved() {
    let allowed = ["allowed_id".into()];
    let text = "[[kp-ref:allowed_id]] [note [1]]";
    assert_eq!(sanitize_citations(text, &allowed), text);
}
