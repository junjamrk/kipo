mod support;

use kp_derive::Memory;
use kp_ingest::hooks::{run_with_path, INTERNAL_EXTRACTION_ENV};
use kp_model::{EpochMs, FactKind, ValidityWindow};
use serde_json::json;
use support::{library, options, Result};

#[tokio::test]
async fn kp_internal_extraction_env_guard_immediately_short_circuits_every_hook() -> Result {
    let (dir, mut store) = library()?;
    let actor = "alice".parse()?;
    kp_derive::remember(
        &mut store,
        Memory {
            subject: "src/pay.rs".into(),
            predicate: "kp.core.constraint".into(),
            value: json!({"hard_guard": true, "reason": "must ask"}),
            kind: FactKind::Constraint,
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

    std::env::set_var(INTERNAL_EXTRACTION_ENV, "1");
    for event in [
        "session-start",
        "user-prompt-submit",
        "pre-tool-use",
        "post-tool-use",
        "stop",
        "session-end",
    ] {
        let out = run_with_path(
            Some(dir.path()),
            &mut store,
            event,
            &json!({
                "session_id": "rec-1",
                "prompt": "src/pay.rs",
                "tool_name": "Edit",
                "tool_input": {"file_path": "src/pay.rs"},
                "messages": [{"role": "user", "content": "should never capture"}]
            }),
            &actor,
            EpochMs(100),
        )
        .await?;
        assert_eq!(out, json!({}), "expected empty JSON on {event}");
    }
    std::env::remove_var(INTERNAL_EXTRACTION_ENV);

    assert_eq!(store.status()?.sources, 0);
    Ok(())
}
