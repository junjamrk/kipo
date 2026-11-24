use super::*;
use crate::test_support::fact;

#[test]
fn content_identity_excludes_mutable_metadata() -> Result<()> {
    let mut value = fact("f", &["A"], 0, 10)?;
    let original = value.strong_ref();
    value.status = FactStatus::Withdrawn;
    value.access = FactAccess::Base {
        audience: AudienceSpec::AllMembers,
        stamp: BitemporalStamp {
            valid: crate::ValidityWindow {
                valid_from: EpochMs(5),
                valid_until: EpochMs(7),
            },
            recorded_at: EpochMs(9),
            expired_at: Some(EpochMs(11)),
        },
    };
    assert_eq!(original, value.strong_ref());
    value.object = serde_json::json!({"condition":"sun"});
    assert_ne!(original, value.strong_ref());
    Ok(())
}

#[test]
fn audience_wire_format_is_unambiguous() -> std::result::Result<(), Box<dyn std::error::Error>> {
    assert_eq!(
        serde_json::to_string(&AudienceSpec::AllMembers)?,
        r#"["*"]"#
    );
    assert_eq!(
        serde_json::to_string(&AudienceSpec::SemiringDerived)?,
        r#""__semiring__""#
    );
    assert!(serde_json::from_str::<AudienceSpec>(r#"["*","A"]"#).is_err());
    let actors = AudienceSpec::Explicit(["B".parse()?, "A".parse()?].into());
    assert_eq!(serde_json::to_string(&actors)?, r#"["A","B"]"#);
    assert_eq!(
        serde_json::from_str::<AudienceSpec>(&serde_json::to_string(&actors)?)?,
        actors
    );
    assert!(serde_json::from_str::<TrustTier>("4").is_err());
    Ok(())
}

#[test]
fn fact_validation_rejects_ambiguous_base_access() -> Result<()> {
    let mut value = fact("f", &["A"], 3, 3)?;
    assert!(value.validate().is_err());
    value = fact("f", &["A"], 0, 10)?;
    if let FactAccess::Base { audience, .. } = &mut value.access {
        *audience = AudienceSpec::SemiringDerived;
    }
    assert!(value.validate().is_err());
    value = fact("f", &["A"], 0, 10)?;
    value.kind = FactKind::Synthesis;
    assert!(value.validate().is_err());
    Ok(())
}
