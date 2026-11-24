use std::collections::BTreeSet;

use kp_model::{
    ActorId, AnchorSpec, AudienceSpec, BitemporalStamp, EpochMs, Fact, FactAccess, FactKind,
    FactRank, FactStatus, LibraryMode, TrustTier, ValidityWindow,
};
use kp_store::{ChunkInput, InitOptions, RawSourceInput, Store};

pub(crate) type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;
pub(crate) const AT: EpochMs = EpochMs(1700000000000);

pub(crate) fn library() -> TestResult<(tempfile::TempDir, Store)> {
    let dir = tempfile::tempdir()?;
    let store = Store::init(
        dir.path(),
        InitOptions {
            library_id: "test".parse()?,
            name: "Test".into(),
            mode: LibraryMode::Personal,
            owner: "carol".parse()?,
            created_at: AT,
        },
    )?;
    Ok((dir, store))
}

pub(crate) fn add(store: &mut Store, id: &str, body: &str, actor: &str) -> TestResult {
    store.ingest_source(
        &"test".parse()?,
        &id.parse()?,
        &RawSourceInput {
            self_echo_filtered: false,
            uri: format!("{id}.md"),
            media_type: "text/markdown".into(),
            content: body.as_bytes().to_vec(),
            readable_by: AudienceSpec::Explicit(BTreeSet::from([actor.parse()?])),
            captured_at: AT,
            chunks: vec![ChunkInput {
                heading: id.into(),
                body: body.into(),
                anchor: AnchorSpec::default(),
            }],
        },
    )?;
    Ok(())
}

pub(crate) fn fact(id: &str, body: &str, actor: &str) -> TestResult<Fact> {
    Ok(Fact {
        id: id.parse()?,
        origin_library: "test".parse()?,
        logical_key: id.parse()?,
        kind: FactKind::Observation,
        subject: id.into(),
        predicate: "observed".into(),
        object: serde_json::json!(body),
        tags: BTreeSet::new(),
        author: "carol".parse()?,
        ingested_by: None,
        trust_tier: TrustTier::HumanVerbatim,
        access: FactAccess::Base {
            audience: AudienceSpec::Explicit(BTreeSet::from([actor.parse::<ActorId>()?])),
            stamp: BitemporalStamp {
                valid: ValidityWindow {
                    valid_from: EpochMs(0),
                    valid_until: EpochMs::OPEN_END,
                },
                recorded_at: AT,
                expired_at: None,
            },
        },
        source_scope_id: None,
        governed_scope: None,
        broader_scope: None,
        status: FactStatus::Active,
        superseded_by: None,
        rank: FactRank::Normal,
        source_anchor: None,
        unverified_gaps: Vec::new(),
        miss_count: 0,
    })
}
