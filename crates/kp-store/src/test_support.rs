use kp_model::{
    ActorId, AnchorSpec, AudienceSpec, BitemporalStamp, DerivationMode, EpochMs, Fact, FactAccess,
    FactKind, FactRank, FactStatus, Justification, LibraryMode, Premise, TrustTier, ValidityWindow,
};
use tempfile::TempDir;

use crate::{ChunkInput, InitOptions, RawSourceInput, Result, Store};

pub(crate) const AT: EpochMs = EpochMs(1_700_000_000_000);

pub(crate) fn library(mode: LibraryMode) -> Result<(TempDir, Store)> {
    let dir = tempfile::tempdir()?;
    let store = Store::init(dir.path(), options(mode)?)?;
    Ok((dir, store))
}

pub(crate) fn options(mode: LibraryMode) -> Result<InitOptions> {
    Ok(InitOptions {
        library_id: "local".parse()?,
        name: "Test Library".into(),
        mode,
        owner: "owner".parse()?,
        created_at: AT,
    })
}

pub(crate) fn source(actors: &[&str]) -> Result<RawSourceInput> {
    Ok(RawSourceInput {
        self_echo_filtered: false,
        uri: "weather.md".into(),
        media_type: "text/markdown".into(),
        content: "# 天气\n下雨时不要开车。Use getWeather and HTTP_client.\n"
            .as_bytes()
            .to_vec(),
        readable_by: AudienceSpec::Explicit(
            actors
                .iter()
                .map(|a| a.parse::<ActorId>())
                .collect::<kp_model::Result<_>>()?,
        ),
        captured_at: AT,
        chunks: vec![ChunkInput {
            heading: "天气".into(),
            body: "下雨时不要开车。Use getWeather and HTTP_client.".into(),
            anchor: AnchorSpec::default(),
        }],
    })
}

pub(crate) fn fact(id: &str, actors: &[&str]) -> Result<Fact> {
    Ok(Fact {
        id: id.parse()?,
        origin_library: "origin".parse()?,
        logical_key: id.parse()?,
        kind: FactKind::Observation,
        subject: "weather".into(),
        predicate: "kp.core.weather".into(),
        object: serde_json::json!("rain"),
        tags: Default::default(),
        author: "owner".parse()?,
        ingested_by: None,
        trust_tier: TrustTier::HumanVerbatim,
        access: FactAccess::Base {
            audience: AudienceSpec::Explicit(
                actors
                    .iter()
                    .map(|a| a.parse())
                    .collect::<kp_model::Result<_>>()?,
            ),
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

pub(crate) fn derived(id: &str) -> Result<Fact> {
    let mut record = fact(id, &[])?;
    record.kind = FactKind::DerivedFact;
    record.access = FactAccess::Derived {
        recorded_at: AT,
        expired_at: None,
    };
    Ok(record)
}

pub(crate) fn justification(
    id: &str,
    conclusion: &Fact,
    premises: &[&Fact],
) -> Result<Justification> {
    Ok(Justification {
        id: id.parse()?,
        conclusion: conclusion.id.clone(),
        mode: DerivationMode::HardRule,
        rule_id: "rule".into(),
        rule_version: "1".into(),
        active: true,
        created_at: AT,
        invalidated_at: None,
        invalidation_reason: None,
        premises: premises
            .iter()
            .map(|fact| Premise {
                fact_id: fact.id.clone(),
                strong_ref: fact.strong_ref(),
            })
            .collect(),
    })
}
