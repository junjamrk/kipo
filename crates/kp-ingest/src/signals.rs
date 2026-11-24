use kp_model::{ActorId, ContrastKind, EpochMs, FactId, FactKind, TrustTier};
use kp_store::{Episode, Store};
use serde_json::{json, Value};

use crate::Result;

/// Explicit recording is a human attestation of the selected contrast kind.
pub fn record(
    store: &mut Store,
    mut episode: Episode,
    actor: &ActorId,
    premises: &[FactId],
    at: EpochMs,
) -> Result<usize> {
    if store.member_role(actor)?.is_none() {
        return Err(kp_store::StoreError::PermissionDenied.into());
    }
    episode = episode.redacted()?;
    if !episode.has_contrast() {
        return Ok(0);
    }
    let visible = store.visible_facts(actor, at)?;
    for id in premises {
        if !visible.iter().any(|f| &f.id == id) {
            return Err(kp_store::StoreError::NotFound.into());
        }
    }
    if episode.signal_kind == ContrastKind::CrossAvatarContrast {
        let target = episode
            .after_trace
            .get("target")
            .and_then(Value::as_str)
            .or_else(|| episode.environment.get("target").and_then(Value::as_str));
        let dead_end = visible.iter().find(|f| {
            premises.contains(&f.id)
                && matches!(
                    f.kind,
                    FactKind::ClosedNegative | FactKind::CollisionWarning
                )
                && target.is_some_and(|t| {
                    t == f.subject
                        || t == f.predicate
                        || f.source_anchor.as_ref().and_then(|a| a.symbol.as_deref()) == Some(t)
                })
        });
        if dead_end.is_none()
            || episode.after_trace.get("success").and_then(Value::as_bool) != Some(true)
        {
            return Ok(0);
        }
    }
    if !episode.environment.is_object() {
        return Err(crate::IngestError::Invalid(
            "episode environment must be an object",
        ));
    }
    episode.environment["actor"] = json!(actor);
    episode.environment["premise_ids"] = json!(premises);
    Ok(store.record_episodes(&[episode], at)?)
}

/// Pairs a verified working fact with another avatar's visible dead-end on the same locus.
pub fn cross_avatar(
    store: &mut Store,
    working: &FactId,
    actor: &ActorId,
    session: &str,
    at: EpochMs,
) -> Result<usize> {
    let facts = store.visible_facts(actor, at)?;
    let fix = facts
        .iter()
        .find(|f| &f.id == working)
        .ok_or(kp_store::StoreError::NotFound)?;
    if fix.trust_tier == TrustTier::ExternalUntrusted
        || matches!(
            fix.kind,
            FactKind::ClosedNegative | FactKind::CollisionWarning
        )
        || fix.object.get("verified").and_then(Value::as_bool) != Some(true)
    {
        return Ok(0);
    }
    let mut inserted = 0;
    for dead in facts.iter().filter(|f| {
        f.author != fix.author
            && matches!(
                f.kind,
                FactKind::ClosedNegative | FactKind::CollisionWarning
            )
            && (f.subject == fix.subject
                || (f.predicate == fix.predicate
                    && f.source_anchor
                        .as_ref()
                        .and_then(|a| a.symbol.as_ref())
                        .is_some()
                    && f.source_anchor.as_ref().and_then(|a| a.symbol.as_ref())
                        == fix.source_anchor.as_ref().and_then(|a| a.symbol.as_ref())))
    }) {
        let episode = Episode {
            signal_kind: ContrastKind::CrossAvatarContrast,
            session_id: session.into(),
            before_trace: json!({"fact_id":dead.id,"content":dead.object,"target":dead.subject,"success":false}),
            after_trace: json!({"fact_id":fix.id,"content":fix.object,"target":dead.subject,"success":true,"attribution":fix.object.get("attribution")}),
            step_ref: format!("fact:{working}"),
            environment: json!({"actor":actor}),
        };
        inserted += record(
            store,
            episode,
            actor,
            &[dead.id.clone(), fix.id.clone()],
            at,
        )?;
    }
    Ok(inserted)
}

pub(crate) fn is_correction(text: &str) -> bool {
    let text = text.trim().to_lowercase();
    [
        "that's wrong",
        "that is wrong",
        "instead,",
        "不对",
        "不是",
        "应该",
        "改成",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
        || ["no,", "actually,"].iter().any(|prefix| {
            text.strip_prefix(prefix).is_some_and(|rest| {
                ["use ", "should ", "must ", "instead ", " is ", "not "]
                    .iter()
                    .any(|signal| rest.contains(signal))
            })
        })
}
